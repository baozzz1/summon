import { t as translate, rich } from '../i18n';
// 日志:daemon / sidecar 日志与控制台审计,最新的在上,往前翻页;按关键字、agent、时间范围与「只看出错」
// 筛。只给能改整机设置的人(导航里看不到这一项的人,接口也会拒绝)。
import { html, nothing, type TemplateResult } from 'lit';
import { live } from 'lit/directives/live.js';
import { errorMessage } from '../api/client';
import type { LogFile, LogLine } from '../api/types';
import { hrefFor, navigate, type Route } from '../app/router';
import { store } from '../app/store';
import { segmented, selectCtl, switchCtl } from '../components/controls';
import { icon } from '../components/icon';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { fmtDateTime } from '../util/format';

type LogsRoute = Extract<Route, { page: 'logs' }>;

const FILES: Array<{ value: LogFile; label: string }> = [
  { value: 'daemon', label: 'daemon' },
  { value: 'sidecar', label: 'sidecar' },
  { value: 'audit', get label() { return translate('d490f4d'); } },
];

/** 时间范围说成人话(来自会话详情里「看这段时间的日志」)。 */
export function rangeText(since?: string, until?: string): string {
  if (since && until) return translate('dd884d6', [fmtDateTime(since), fmtDateTime(until)]);
  if (since) return translate('d6e1414', [fmtDateTime(since)]);
  if (until) return translate('df56398', [fmtDateTime(until)]);
  return '';
}

/** 条目的时刻:今天的只写时分秒,其余带上月日。 */
export function logTime(iso: string, now = new Date()): string {
  const d = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, '0');
  const hms = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  return d.toDateString() === now.toDateString() ? hms : `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${hms}`;
}

/** 时刻已经单列:daemon 与 sidecar 正文开头的那个时间戳不再重复(sidecar 的级别前缀留着)。 */
export function bodyText(text: string): string {
  return text.replace(/^(?:\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\] |(\[[A-Za-z]+\] )?\d{4}\/\d{2}\/\d{2} \d{2}:\d{2}:\d{2}(?:\.\d+)? )/, '$1');
}

class LogsView extends PageBase {
  entries: LogLine[] = [];
  /** 更早一页的翻页位置 */
  cursor: string | undefined;
  query = '';
  errors = false;
  loadingMore = false;
  private seq = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;

  private get target(): LogsRoute {
    return this.route as LogsRoute;
  }

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    clearTimeout(this.timer);
  }

  override willUpdate(changed: Map<PropertyKey, unknown>): void {
    super.willUpdate(changed);
    const prev = changed.get('route') as LogsRoute | undefined;
    if (prev && hrefFor(prev) !== hrefFor(this.target)) void this.refresh();
  }

  private params(before?: string) {
    const r = this.target;
    return {
      file: r.file ?? 'daemon',
      ...(before ? { before } : {}),
      ...(this.query.trim() ? { query: this.query.trim() } : {}),
      ...(r.agent ? { agent: r.agent } : {}),
      ...(r.since ? { since: r.since } : {}),
      ...(r.until ? { until: r.until } : {}),
      ...(this.errors ? { errors: true } : {}),
    };
  }

  async refresh(): Promise<void> {
    const mine = ++this.seq;
    try {
      const page = await this.call('logs.tail', this.params());
      if (mine !== this.seq) return;
      this.entries = page.entries;
      this.cursor = page.before;
      this.loadError = '';
    } catch (err) {
      if (mine !== this.seq) return;
      this.loadError = errorMessage(err);
    }
    this.loading = false;
    this.requestUpdate();
  }

  private async more(): Promise<void> {
    if (!this.cursor) return;
    const mine = this.seq;
    this.loadingMore = true;
    this.requestUpdate();
    try {
      const page = await this.call('logs.tail', this.params(this.cursor));
      if (mine === this.seq) {
        this.entries = [...this.entries, ...page.entries];
        this.cursor = page.before;
      }
    } catch (err) {
      this.loadError = errorMessage(err);
    }
    this.loadingMore = false;
    this.requestUpdate();
  }

  /** 改了地址里的条件(文件、agent、时间范围):换地址,由路由带着重新取。 */
  private go(patch: Partial<Record<'file' | 'agent' | 'since' | 'until', string | undefined>>): void {
    const r = this.target;
    const merged = { file: r.file, agent: r.agent, since: r.since, until: r.until, ...patch };
    const next: LogsRoute = { page: 'logs' };
    if (merged.file === 'daemon' || merged.file === 'sidecar' || merged.file === 'audit') next.file = merged.file;
    if (merged.agent) next.agent = merged.agent;
    if (merged.since) next.since = merged.since;
    if (merged.until) next.until = merged.until;
    void navigate(hrefFor(next), { replace: true });
  }

  private toolbar(): TemplateResult {
    const r = this.target;
    const agents = store.state.status?.agents ?? [];
    const range = rangeText(r.since, r.until);
    return html`<div class="toolbar log-tools">
      ${segmented({ label: translate('df88d39'), size: 'sm', value: r.file ?? 'daemon', options: FILES, onChange: (v) => this.go({ file: v }) })}
      <label class="search">
        ${icon('search')}
        <input
          class="field-input"
          type="search"
          placeholder="${translate('d884e46')}"
          aria-label="${translate('d0474b5')}"
          .value=${live(this.query)}
          @input=${(e: Event) => {
            this.query = (e.target as HTMLInputElement).value;
            clearTimeout(this.timer);
            this.timer = setTimeout(() => void this.refresh(), 300);
          }}
        />
      </label>
      ${selectCtl({
        label: translate('dbbe7b2'),
        value: r.agent ?? '',
        options: [{ value: '', label: translate('d198f2f') }, ...agents.map((a) => ({ value: a.alias, label: a.display || a.alias }))],
        onChange: (v) => this.go({ agent: v || undefined }),
      })}
      <span class="log-errors">
        ${rich('d1d4d86', [switchCtl({
          checked: this.errors,
          label: translate('dd40b5b'),
          onChange: (v) => {
            this.errors = v;
            void this.refresh();
          },
        })])}
      </span>
      ${range
        ? html`<span class="chip">${range}</span>
            <button class="btn btn-sm btn-ghost" @click=${() => this.go({ since: undefined, until: undefined })}>${rich('de98833')}</button>`
        : nothing}
    </div>`;
  }

  private row(e: LogLine): TemplateResult {
    return html`<div class="log-row ${e.error ? 'is-error' : ''}">
      <span class="log-at">${e.at ? logTime(e.at) : ''}</span>
      <pre class="log-text">${e.at ? bodyText(e.text) : e.text}</pre>
    </div>`;
  }

  protected override render(): TemplateResult {
    return html`<div class="page">
      ${pageHead(translate('d7dbac1'), translate('d3e0845'))} ${this.toolbar()}
      ${this.loadError ? errorBanner(translate('d6b62b0'), this.loadError, () => this.refresh()) : nothing}
      ${this.loading
        ? skeletonCard(6)
        : this.entries.length
          ? html`<div class="card log-list">${this.entries.map((e) => this.row(e))}</div>`
          : html`<div class="card">${emptyState(translate('dda3a85'), this.cursor ? translate('da05c61') : translate('d9a1ff7'))}</div>`}
      ${this.cursor
        ? html`<div class="more-row">
            <button class="btn btn-sm" ?disabled=${this.loadingMore} @click=${() => this.more()}>
              ${this.loadingMore ? translate('d4927a5') : translate('dfab190')}
            </button>
          </div>`
        : nothing}
    </div>`;
  }
}

customElements.define('sm-page-logs', LogsView);
