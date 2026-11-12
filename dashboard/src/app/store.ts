import { setLanguage, type Language } from '../i18n';
import { ApiCallError, createClient, errorMessage, type Client, type Transport } from '../api/client';
import type { Method, Params, Returns, Schema, StatusView, WhoAmI } from '../api/types';
import { DEFAULT_ICON } from './brand';
import { RequestScope } from './async-state';

export type AuthState = 'loading' | 'ok' | 'expired' | 'error';

export interface AppState {
  auth: AuthState;
  whoami: WhoAmI | null;
  status: StatusView | null;
  schema: Schema | null;
  /** Menu Bar 当前的 summon 图标(`menubar.icon` 的生效值),控制台的标识跟着它 */
  icon: string;
  bootError: string;
}

export const POLL_MS = 15_000;

/**
 * 全局状态只有这几样:身份、daemon 状态(含各文件版本)、字段表。页面自己的数据由页面
 * controller 管。版本变化以 `revisions` 事件广播,由各页决定自动刷新还是提示。
 */
export class Store extends EventTarget {
  state: AppState = { auth: 'loading', whoami: null, status: null, schema: null, icon: DEFAULT_ICON, bootError: '' };
  client!: Client;
  private pollTimer: ReturnType<typeof setInterval> | undefined;
  private readonly requests = new RequestScope();
  private schemaLanguage: Language | null = null;

  init(transport: Transport): void {
    this.requests.invalidate();
    this.stopPolling();
    this.client = createClient(transport, () => this.expire());
  }

  call<M extends Method>(method: M, params: Params<M>): Promise<Returns<M>> {
    return this.client.call(method, params);
  }

  private set(patch: Partial<AppState>): void {
    this.state = { ...this.state, ...patch };
    this.dispatchEvent(new Event('change'));
  }

  expire(): void {
    this.requests.invalidate();
    this.stopPolling();
    this.set({ auth: 'expired' });
  }

  async boot(): Promise<void> {
    if (this.state.auth === 'expired') return;
    const current = this.requests.begin('boot');
    try {
      const whoami = await this.client.call('session.whoami', {});
      if (!current()) return;
      this.client.setCsrf(whoami.csrf);
      const statusCurrent = this.requests.begin('status');
      const [status, schema] = await Promise.all([
        this.client.call('status.get', {}),
        this.client.call('config.schema', {}),
      ]);
      if (!current()) return;
      const selectedStatus = statusCurrent() ? status : this.state.status ?? status;
      await setLanguage(selectedStatus.language);
      if (!current()) return;
      this.schemaLanguage = status.language;
      this.set({ auth: 'ok', whoami, status: statusCurrent() ? status : this.state.status, schema });
      this.startPolling();
      void this.loadIcon();
    } catch (err) {
      if (!current()) return;
      if (err instanceof ApiCallError && err.code === 'unauthenticated') return;
      this.set({ auth: 'error', bootError: errorMessage(err) });
    }
  }

  /** 取一次 status,返回版本有变化的那些键。silent:自己刚保存过,只更新不广播。 */
  async refreshStatus(silent = false): Promise<string[]> {
    if (this.state.auth === 'expired') return [];
    const current = this.requests.begin('status');
    let status: StatusView;
    try {
      status = await this.client.call('status.get', {});
    } catch {
      return [];
    }
    if (!current()) return [];
    const translated = await setLanguage(status.language);
    if (!current()) return [];
    let schema = this.state.schema;
    let refreshedSchema = false;
    if (this.schemaLanguage !== status.language) {
      try {
        schema = await this.client.call('config.schema', {});
        if (current()) { this.schemaLanguage = status.language; refreshedSchema = true; }
      }
      catch { /* The current schema remains usable while the next poll retries. */ }
      if (!current()) return [];
    }
    const before = this.state.status?.revisions ?? {};
    const changed = changedRevisions(before, status.revisions);
    this.set({ status, schema });
    if (translated || refreshedSchema) this.dispatchEvent(new Event('language'));
    // 图标在 config.yaml 里,Menu Bar 或这里改了都会动它的版本。
    if (changed.includes('global')) void this.loadIcon();
    if (changed.length && !silent) {
      this.dispatchEvent(new CustomEvent<string[]>('revisions', { detail: changed }));
    }
    return changed;
  }

  async loadIcon(): Promise<void> {
    if (this.state.auth === 'expired') return;
    const current = this.requests.begin('icon');
    try {
      const view = await this.client.call('config.get', { scope: { level: 'global' } });
      if (!current()) return;
      const icon = view.values.find((v) => v.key === 'menubar.icon')?.effective;
      this.set({ icon: typeof icon === 'string' && icon ? icon : DEFAULT_ICON });
    } catch {
      // 取不到就保持当前的标识
    }
  }

  /** 保存成功后自己产生的版本变化不该被当成「别处改过」,先记下来。 */
  acceptRevision(key: string, revision: string): void {
    const status = this.state.status;
    if (!status) return;
    this.requests.invalidate('status');
    this.set({ status: { ...status, revisions: { ...status.revisions, [key]: revision } } });
  }

  setRestartPending(items: StatusView['restart_pending']): void {
    const status = this.state.status;
    if (!status) return;
    this.requests.invalidate('status');
    this.set({ status: { ...status, restart_pending: items } });
  }

  startPolling(): void {
    this.stopPolling();
    if (typeof document === 'undefined') return;
    this.pollTimer = setInterval(() => {
      if (document.visibilityState === 'visible') void this.refreshStatus();
    }, POLL_MS);
    document.addEventListener('visibilitychange', this.onVisible);
  }

  stopPolling(): void {
    if (this.pollTimer) clearInterval(this.pollTimer);
    this.pollTimer = undefined;
    if (typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', this.onVisible);
    }
  }

  private onVisible = (): void => {
    if (document.visibilityState === 'visible') void this.refreshStatus();
  };
}

export function changedRevisions(
  before: Record<string, string>,
  after: Record<string, string>,
): string[] {
  const keys = new Set([...Object.keys(before), ...Object.keys(after)]);
  return [...keys].filter((k) => before[k] !== after[k]).sort();
}

export const store = new Store();
