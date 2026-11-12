type DisplayLabel = string | (() => string);
import { ApiCallError, errorMessage } from '../api/client';
import type {
  ApiError,
  ConfigChange,
  ConfigView,
  DocRef,
  DocView,
  FieldDesc,
  FieldValue,
  Method,
  Params,
  RawCheck,
  RawFile,
  RawView,
  Returns,
  SaveResult,
  Scope,
} from '../api/types';
import { sameValue } from '../util/format';
import { RequestScope, Submission } from './async-state';

export type Caller = <M extends Method>(method: M, params: Params<M>) => Promise<Returns<M>>;

export type SaveOutcome =
  | { ok: true; result?: SaveResult; label: string }
  | { ok: false; error: ApiError; label: string }
  | { ok: false; cancelled: true; label: string };

/**
 * 页面里一块可编辑的东西(一份配置层、一个说明文件、原文、名单…)。保存条把同页所有块的
 * 改动数加起来,一次保存逐块提交;每块各自带版本,冲突也各自报。
 */
export interface Editable {
  readonly id: string;
  readonly label: string;
  dirtyCount(): number;
  restartCount(): number;
  hasConflict(): boolean;
  save(): Promise<SaveOutcome>;
  reset(): void;
  /** 冲突后重新读取:换上新的原值,草稿保留 */
  reload(): Promise<void>;
  /** 这块对应 status.revisions 里的哪个键(用于轮询发现别处改动) */
  revisionKey(): string | null;
}

function asApiError(err: unknown): ApiError {
  if (err instanceof ApiCallError) {
    return err.key ? { code: err.code, message: err.message, key: err.key } : { code: err.code, message: err.message };
  }
  return { code: 'internal', message: errorMessage(err) };
}

export function scopeRevisionKey(scope: Scope): string {
  switch (scope.level) {
    case 'global':
      return 'global';
    case 'agent':
      return `agent:${scope.agent}`;
    case 'chat':
      return `chat:${scope.agent}:${scope.chat_id}`;
  }
}

// ── 一份配置层的草稿 ────────────────────────────────────────────────────

interface ConfigSubmission {
  view: ConfigView;
  changes: ConfigChange[];
  versions: Map<string, number>;
}

interface TextSubmission<V> {
  view: V;
  text: string;
  version: number;
}

export class ConfigDraft implements Editable {
  get label(): string { return typeof this.labelSource === 'function' ? this.labelSource() : this.labelSource; }
  readonly id: string;
  private readonly labelSource: DisplayLabel;
  view: ConfigView | null = null;
  loadError = '';
  /** key → 本层的新值;null = 删掉本层的键(恢复继承) */
  readonly changes = new Map<string, unknown>();
  readonly errors = new Map<string, string>();
  conflict = false;
  private readonly requests = new RequestScope();
  private readonly submission = new Submission<ConfigSubmission, SaveOutcome>();
  private readonly versions = new Map<string, number>();

  constructor(
    readonly scope: Scope,
    label: DisplayLabel,
    private readonly fields: () => Map<string, FieldDesc>,
    private readonly call: Caller,
    private readonly onChange: () => void,
  ) {
    this.id = `config:${scopeRevisionKey(scope)}`;
    this.labelSource = label;
  }

  revisionKey(): string {
    return scopeRevisionKey(this.scope);
  }

  async load(): Promise<void> {
    const current = this.requests.begin('view');
    const versions = new Map(this.versions);
    try {
      const view = await this.call('config.get', { scope: this.scope });
      if (!current()) return;
      this.keepNewEdits(versions);
      this.setView(view);
      this.loadError = '';
    } catch (err) {
      if (!current()) return;
      this.loadError = errorMessage(err);
      this.onChange();
    }
  }

  setView(view: ConfigView): void {
    const pending = this.submission.snapshot;
    if (pending) this.keepNewEdits(pending.versions);
    this.view = view;
    // 新的原值与草稿相同的项不算改动(例如别处恰好写了同一个值)。
    for (const [key, v] of [...this.changes]) {
      if (sameValue(this.base(key), v)) this.changes.delete(key);
    }
    this.onChange();
  }

  fieldValue(key: string): FieldValue | undefined {
    return this.view?.values.find((v) => v.key === key);
  }

  /** 本层原来写的值(null = 没写) */
  base(key: string): unknown {
    return this.fieldValue(key)?.value ?? null;
  }

  isChanged(key: string): boolean {
    return this.changes.has(key);
  }

  /** 控件当前要显示的值:有草稿用草稿;本层写了用本层;否则显示继承来的生效值。 */
  shown(key: string): { value: unknown; inherited: boolean } {
    if (this.changes.has(key)) {
      const v = this.changes.get(key);
      if (v === null) return { value: this.fieldValue(key)?.effective ?? null, inherited: true };
      return { value: v, inherited: false };
    }
    const fv = this.fieldValue(key);
    if (!fv) return { value: null, inherited: true };
    if (fv.value !== null && fv.value !== undefined) return { value: fv.value, inherited: false };
    return { value: fv.effective, inherited: true };
  }

  set(key: string, value: unknown): void {
    this.touch(key);
    this.errors.delete(key);
    if (sameValue(this.base(key), value)) this.changes.delete(key);
    else this.changes.set(key, value);
    this.onChange();
  }

  /**
   * 开关、输入框这类控件显示的是生效值:拨回没有草稿时显示的那个值就等于没改,哪怕原来是继承来的。
   * 「继承 / 开 / 关」三态由人明确选层,走 set。
   */
  setShown(key: string, value: unknown): void {
    const fv = this.fieldValue(key);
    const before = fv && fv.value !== null && fv.value !== undefined ? fv.value : (fv?.effective ?? null);
    if (sameValue(before, value)) this.revert(key);
    else this.set(key, value);
  }

  inherit(key: string): void {
    this.set(key, null);
  }

  revert(key: string): void {
    this.touch(key);
    this.changes.delete(key);
    this.errors.delete(key);
    this.onChange();
  }

  changeList(): ConfigChange[] {
    return [...this.changes].map(([key, value]) => ({ key, value }));
  }

  dirtyCount(): number {
    return this.changes.size;
  }

  restartCount(): number {
    const fields = this.fields();
    return [...this.changes.keys()].filter((k) => fields.get(k)?.effect === 'restart').length;
  }

  hasConflict(): boolean {
    return this.conflict;
  }

  reset(): void {
    for (const key of this.versions.keys()) this.touch(key);
    this.changes.clear();
    this.errors.clear();
    this.conflict = false;
    this.onChange();
  }

  async reload(): Promise<void> {
    const current = this.requests.begin('view');
    const versions = new Map(this.versions);
    const view = await this.call('config.get', { scope: this.scope });
    if (!current()) return;
    this.keepNewEdits(versions);
    this.conflict = false;
    this.setView(view);
  }

  private touch(key: string): void {
    this.versions.set(key, (this.versions.get(key) ?? 0) + 1);
  }

  /** 改回旧原值也算新意图，不能因为 changes 暂时为空就被旧 ACK 吞掉。 */
  private keepNewEdits(versions: Map<string, number>): void {
    for (const [key, version] of this.versions) {
      if (version !== versions.get(key)) {
        const desired = this.changes.has(key) ? this.changes.get(key) : this.base(key);
        this.changes.set(key, desired);
      }
    }
  }

  async save(): Promise<SaveOutcome> {
    if (this.submission.pending) return this.submission.pending;
    if (!this.view || this.changes.size === 0) return { ok: true, label: this.label };
    const snapshot: ConfigSubmission = { view: this.view, changes: this.changeList(), versions: new Map(this.versions) };
    return this.submission.run(snapshot, 'submitting', async (transition) => {
      try {
        const result = await this.call('config.patch', {
          scope: this.scope,
          revision: snapshot.view.revision,
          changes: snapshot.changes,
        });
        this.requests.invalidate();
        this.keepNewEdits(snapshot.versions);
        for (const { key } of snapshot.changes) {
          if (this.versions.get(key) === snapshot.versions.get(key)) {
            this.changes.delete(key);
            this.errors.delete(key);
          }
        }
        const submitted = new Map(snapshot.changes.map(({ key, value }) => [key, value]));
        this.conflict = false;
        this.setView({
          ...snapshot.view,
          revision: result.revision,
          values: snapshot.view.values.map((field) => submitted.has(field.key) ? { ...field, value: submitted.get(field.key) } : field),
        });
        transition('refreshing');
        try {
          await this.reload();
        } catch {
          // 写入已确认，读取失败仍以提交快照作为新原值。
        }
        return { ok: true, result, label: this.label };
      } catch (err) {
        const error = asApiError(err);
        if (error.code === 'conflict') this.conflict = true;
        if (error.key && this.versions.get(error.key) === snapshot.versions.get(error.key)) this.errors.set(error.key, error.message);
        this.onChange();
        return { ok: false, error, label: this.label };
      }
    });
  }
}

// ── 说明文件 ────────────────────────────────────────────────────────────

export class DocDraft implements Editable {
  get label(): string { return typeof this.labelSource === 'function' ? this.labelSource() : this.labelSource; }
  readonly id: string;
  view: DocView | null = null;
  text = '';
  conflict = false;
  error = '';
  loadError = '';
  private version = 0;
  private readonly requests = new RequestScope();
  private readonly submission = new Submission<TextSubmission<DocView>, SaveOutcome>();

  constructor(
    readonly doc: DocRef,
    private readonly labelSource: DisplayLabel,
    private readonly call: Caller,
    private readonly onChange: () => void,
  ) {
    this.id = `doc:${JSON.stringify(doc)}`;
  }

  revisionKey(): string | null {
    return this.view ? `doc:${this.view.path}` : null;
  }

  async load(): Promise<void> {
    const current = this.requests.begin('view');
    const version = this.version;
    const wasClean = !this.dirtyCount();
    try {
      const view = await this.call('docs.get', { doc: this.doc });
      if (!current()) return;
      const pending = this.submission.snapshot;
      this.setView(view, wasClean && version === this.version && (!pending || pending.version === this.version));
      this.loadError = '';
    } catch (err) {
      if (!current()) return;
      this.loadError = errorMessage(err);
      this.onChange();
    }
  }

  private setView(view: DocView, replaceText: boolean): void {
    this.view = view;
    if (replaceText) this.text = view.text;
    this.onChange();
  }

  setText(text: string): void {
    this.version += 1;
    this.text = text;
    this.error = '';
    this.onChange();
  }

  dirtyCount(): number {
    return this.view && this.text !== this.view.text ? 1 : 0;
  }

  restartCount(): number {
    return this.dirtyCount() && this.view?.effect === 'restart' ? 1 : 0;
  }

  hasConflict(): boolean {
    return this.conflict;
  }

  reset(): void {
    this.version += 1;
    if (this.view) this.text = this.view.text;
    this.conflict = false;
    this.error = '';
    this.onChange();
  }

  async reload(): Promise<void> {
    const current = this.requests.begin('view');
    const view = await this.call('docs.get', { doc: this.doc });
    if (!current()) return;
    this.conflict = false;
    this.setView(view, false);
  }

  async save(): Promise<SaveOutcome> {
    if (this.submission.pending) return this.submission.pending;
    if (!this.view || !this.dirtyCount()) return { ok: true, label: this.label };
    const snapshot = { view: this.view, text: this.text, version: this.version };
    return this.submission.run(snapshot, 'submitting', async () => {
      try {
        const view = await this.call('docs.put', {
          doc: this.doc,
          revision: snapshot.view.exists ? (snapshot.view.revision ?? null) : null,
          text: snapshot.text,
        });
        this.requests.invalidate();
        this.conflict = false;
        this.setView(view, snapshot.version === this.version);
        return { ok: true, label: this.label };
      } catch (err) {
        const error = asApiError(err);
        if (error.code === 'conflict') this.conflict = true;
        else if (snapshot.version === this.version) this.error = error.message;
        this.onChange();
        return { ok: false, error, label: this.label };
      }
    });
  }
}

// ── YAML 原文 ───────────────────────────────────────────────────────────

/** 保存前给人看差异与校验结果;返回 false 表示取消。 */
export type ReviewRaw = (check: RawCheck, label: string) => Promise<boolean>;

export class RawDraft implements Editable {
  get label(): string { return typeof this.labelSource === 'function' ? this.labelSource() : this.labelSource; }
  readonly id: string;
  view: RawView | null = null;
  text = '';
  conflict = false;
  lastCheck: RawCheck | null = null;
  error = '';
  loadError = '';
  private version = 0;
  private readonly requests = new RequestScope();
  private readonly submission = new Submission<TextSubmission<RawView>, SaveOutcome>();

  constructor(
    readonly file: RawFile,
    private readonly labelSource: DisplayLabel,
    private readonly call: Caller,
    private readonly review: ReviewRaw,
    private readonly onChange: () => void,
  ) {
    this.id = `raw:${JSON.stringify(file)}`;
  }

  revisionKey(): string {
    return this.file.file === 'global' ? 'global' : `agent:${this.file.agent}`;
  }

  async load(): Promise<void> {
    const current = this.requests.begin('view');
    const version = this.version;
    const wasClean = !this.dirtyCount();
    try {
      const view = await this.call('config.raw.get', this.file);
      if (!current()) return;
      this.requests.invalidate('check');
      this.lastCheck = null;
      this.view = view;
      const pending = this.submission.snapshot;
      if (wasClean && version === this.version && (!pending || pending.version === this.version)) this.text = view.text;
      this.loadError = '';
    } catch (err) {
      if (!current()) return;
      this.loadError = errorMessage(err);
    }
    this.onChange();
  }

  setText(text: string): void {
    this.version += 1;
    this.text = text;
    this.error = '';
    this.lastCheck = null;
    this.onChange();
  }

  /** 只校验不保存:解析错误带行列号,未知键给出最接近的合法键。 */
  async check(): Promise<void> {
    const current = this.requests.begin('check');
    const version = this.version;
    try {
      const check = await this.call('config.raw.check', { ...this.file, text: this.text });
      if (!current() || version !== this.version) return;
      this.lastCheck = check;
      this.error = '';
    } catch (err) {
      if (!current() || version !== this.version) return;
      this.error = errorMessage(err);
    }
    this.onChange();
  }

  dirtyCount(): number {
    return this.view && this.text !== this.view.text ? 1 : 0;
  }

  restartCount(): number {
    return 0;
  }

  hasConflict(): boolean {
    return this.conflict;
  }

  reset(): void {
    this.version += 1;
    if (this.view) this.text = this.view.text;
    this.conflict = false;
    this.lastCheck = null;
    this.error = '';
    this.onChange();
  }

  async reload(): Promise<void> {
    const current = this.requests.begin('view');
    const view = await this.call('config.raw.get', this.file);
    if (!current()) return;
    this.requests.invalidate('check');
    this.lastCheck = null;
    this.view = view;
    this.conflict = false;
    this.onChange();
  }

  async save(): Promise<SaveOutcome> {
    if (this.submission.pending) return this.submission.pending;
    if (!this.view || !this.dirtyCount()) return { ok: true, label: this.label };
    const snapshot = { view: this.view, text: this.text, version: this.version };
    const checkCurrent = this.requests.begin('check');
    return this.submission.run(snapshot, 'reviewing', async (transition) => {
      try {
        const check = await this.call('config.raw.check', { ...this.file, text: snapshot.text });
        if (checkCurrent() && snapshot.version === this.version) this.lastCheck = check;
        this.onChange();
        if (check.errors.length) {
          return { ok: false, error: { code: 'invalid', message: check.errors[0]!.message }, label: this.label };
        }
        if (!(await this.review(check, this.label))) return { ok: false, cancelled: true, label: this.label };
        transition('submitting');
        const result = await this.call('config.raw.put', { ...this.file, revision: snapshot.view.revision, text: snapshot.text });
        this.requests.invalidate();
        this.view = { ...snapshot.view, revision: result.revision, text: snapshot.text };
        if (snapshot.version === this.version) this.text = snapshot.text;
        this.lastCheck = null;
        this.conflict = false;
        this.onChange();
        transition('refreshing');
        const current = this.requests.begin('view');
        const version = this.version;
        const replaceText = snapshot.version === this.version;
        try {
          const view = await this.call('config.raw.get', this.file);
          if (current()) {
            this.view = view;
            if (replaceText && version === this.version) this.text = view.text;
            this.onChange();
          }
        } catch {
          // 写入已确认，刷新失败不将它改报成未保存。
        }
        return { ok: true, result, label: this.label };
      } catch (err) {
        const error = asApiError(err);
        if (error.code === 'conflict') this.conflict = true;
        else if (snapshot.version === this.version) this.error = error.message;
        this.onChange();
        return { ok: false, error, label: this.label };
      }
    });
  }
}

// ── 通用:一个值 + 一次提交(名单、按群工作目录) ─────────────────────────

export class ValueDraft<T> implements Editable {
  get label(): string { return typeof this.labelSource === 'function' ? this.labelSource() : this.labelSource; }
  base: T;
  value: T;
  conflict = false;
  error = '';
  private version = 0;
  private readonly requests = new RequestScope();
  private readonly submission = new Submission<{ value: T; version: number }, SaveOutcome>();

  constructor(
    readonly id: string,
    private readonly labelSource: DisplayLabel,
    initial: T,
    private readonly submit: (value: T) => Promise<T>,
    private readonly fetchBase: (() => Promise<T>) | null,
    private readonly onChange: () => void,
    private readonly revKey: string | null = null,
  ) {
    this.base = initial;
    this.value = initial;
  }

  revisionKey(): string | null {
    return this.revKey;
  }

  set(value: T): void {
    this.version += 1;
    this.value = value;
    this.error = '';
    this.onChange();
  }

  rebase(base: T): void {
    this.requests.invalidate();
    const pending = this.submission.snapshot;
    const wasClean = sameValue(this.value, this.base) && (!pending || pending.version === this.version);
    this.base = base;
    if (wasClean) this.value = base;
    this.onChange();
  }

  dirtyCount(): number {
    return sameValue(this.value, this.base) ? 0 : 1;
  }

  restartCount(): number {
    return 0;
  }

  hasConflict(): boolean {
    return this.conflict;
  }

  reset(): void {
    this.version += 1;
    this.value = this.base;
    this.conflict = false;
    this.error = '';
    this.onChange();
  }

  async reload(): Promise<void> {
    if (!this.fetchBase) return;
    const current = this.requests.begin('view');
    const base = await this.fetchBase();
    if (!current()) return;
    this.base = base;
    this.conflict = false;
    this.onChange();
  }

  async save(): Promise<SaveOutcome> {
    if (this.submission.pending) return this.submission.pending;
    if (!this.dirtyCount()) return { ok: true, label: this.label };
    const snapshot = { value: this.value, version: this.version };
    return this.submission.run(snapshot, 'submitting', async () => {
      try {
        const saved = await this.submit(snapshot.value);
        this.requests.invalidate();
        this.base = saved;
        if (snapshot.version === this.version) this.value = saved;
        this.conflict = false;
        this.onChange();
        return { ok: true, label: this.label };
      } catch (err) {
        const error = asApiError(err);
        if (error.code === 'conflict') this.conflict = true;
        else if (snapshot.version === this.version) this.error = error.message;
        this.onChange();
        return { ok: false, error, label: this.label };
      }
    });
  }
}

// ── 一页里的全部可编辑块 ────────────────────────────────────────────────

export interface SaveSummary {
  saved: number;
  failed: SaveOutcome[];
  results: SaveResult[];
}

export class EditSet {
  private items: Editable[] = [];
  private readonly submission = new Submission<readonly Editable[], SaveSummary>();

  get saving(): boolean {
    return this.submission.pending !== null;
  }

  constructor(private readonly onChange: () => void) {}

  register(...e: Editable[]): void {
    for (const item of e) {
      if (!this.items.includes(item)) this.items.push(item);
    }
  }

  replace(e: Editable[]): void {
    this.items = [...e];
    this.onChange();
  }

  all(): readonly Editable[] {
    return this.items;
  }

  dirtyCount(): number {
    return this.items.reduce((n, e) => n + e.dirtyCount(), 0);
  }

  restartCount(): number {
    return this.items.reduce((n, e) => n + e.restartCount(), 0);
  }

  conflicts(): Editable[] {
    return this.items.filter((e) => e.hasConflict());
  }

  reset(): void {
    for (const e of this.items) e.reset();
  }

  async saveAll(): Promise<SaveSummary> {
    if (this.submission.pending) return this.submission.pending;
    const items = [...this.items];
    const pending = this.submission.run(items, 'submitting', async () => {
      const summary: SaveSummary = { saved: 0, failed: [], results: [] };
      for (const e of items) {
        if (!e.dirtyCount()) continue;
        const out = await e.save();
        if (out.ok) {
          summary.saved += 1;
          if (out.result) summary.results.push(out.result);
        } else summary.failed.push(out);
      }
      return summary;
    });
    this.onChange();
    try {
      return await pending;
    } finally {
      this.onChange();
    }
  }

  async reloadConflicts(): Promise<void> {
    for (const e of this.conflicts()) await e.reload();
  }
}
