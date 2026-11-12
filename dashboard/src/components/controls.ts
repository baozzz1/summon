import { LocaleController } from '../i18n';
import { t as translate, rich } from '../i18n';
// 设置行用的控件。都是原生元素加样式:键盘、屏幕阅读器、表单行为不用另写。
import { html, LitElement, nothing, type PropertyValues, type TemplateResult } from 'lit';
import { live } from 'lit/directives/live.js';
import type { FieldOption, Member } from '../api/types';
import { DURATION_UNITS, fmtDuration, sameValue, splitDuration } from '../util/format';
import { isLookupId } from '../app/names';
import { icon } from './icon';
import { ident } from './ident';

export function switchCtl(o: {
  checked: boolean;
  label: string;
  disabled?: boolean;
  onChange: (v: boolean) => void;
}): TemplateResult {
  return html`<button
    type="button"
    class="switch"
    role="switch"
    aria-checked=${o.checked ? 'true' : 'false'}
    aria-label=${o.label}
    ?disabled=${o.disabled}
    @click=${() => o.onChange(!o.checked)}
  >
    <span class="switch-track"></span>
  </button>`;
}

let segSeq = 0;

export function segmented(o: {
  value: string | null;
  options: FieldOption[];
  label: string;
  disabled?: boolean;
  onChange: (v: string) => void;
  size?: 'sm';
}): TemplateResult {
  const name = `seg-${++segSeq}`;
  return html`<div class="seg ${o.size === 'sm' ? 'seg-sm' : ''}" role="radiogroup" aria-label=${o.label}>
    ${o.options.map(
      (opt) => html`<label class="seg-opt">
        <input
          type="radio"
          name=${name}
          value=${opt.value}
          .checked=${live(o.value === opt.value)}
          ?disabled=${o.disabled}
          @change=${() => o.onChange(opt.value)}
        /><span>${opt.label}</span>
      </label>`,
    )}
  </div>`;
}

export function selectCtl(o: {
  id?: string;
  value: string | null;
  options: FieldOption[];
  label: string;
  disabled?: boolean;
  onChange: (v: string) => void;
}): TemplateResult {
  const known = o.options.some((x) => x.value === o.value);
  return html`<span class="select-wrap">
    <select
      class="select"
      data-control=${o.id ?? nothing}
      aria-label=${o.label}
      ?disabled=${o.disabled}
      .value=${live(o.value ?? '')}
      @change=${(e: Event) => o.onChange((e.target as HTMLSelectElement).value)}
    >
      ${!known ? html`<option value=${o.value ?? ''}>${o.value || translate('d2f5f1d')}</option>` : nothing}
      ${o.options.map((opt) => html`<option value=${opt.value} ?selected=${opt.value === o.value}>${opt.label}</option>`)}
    </select>
    ${icon('chevronDown')}
  </span>`;
}

export function textCtl(o: {
  value: string;
  label: string;
  mono?: boolean;
  wide?: boolean;
  placeholder?: string;
  disabled?: boolean;
  list?: string;
  onChange: (v: string) => void;
}): TemplateResult {
  return html`<input
    class="field-input ${o.mono ? 'mono' : ''} ${o.wide ? 'wide' : ''}"
    type="text"
    spellcheck="false"
    autocomplete="off"
    aria-label=${o.label}
    placeholder=${o.placeholder ?? ''}
    list=${o.list ?? nothing}
    ?disabled=${o.disabled}
    .value=${live(o.value)}
    @input=${(e: Event) => o.onChange((e.target as HTMLInputElement).value)}
  />`;
}

export function numberCtl(o: {
  value: number | null;
  label: string;
  min?: number;
  max?: number;
  disabled?: boolean;
  onChange: (v: number | null) => void;
}): TemplateResult {
  return html`<input
    class="field-input num"
    type="number"
    inputmode="numeric"
    aria-label=${o.label}
    min=${o.min ?? nothing}
    max=${o.max ?? nothing}
    ?disabled=${o.disabled}
    .value=${live(o.value === null ? '' : String(o.value))}
    @input=${(e: Event) => {
      const raw = (e.target as HTMLInputElement).value;
      o.onChange(raw === '' ? null : Number(raw));
    }}
  />`;
}

/** 时长:数字与单位；可按字段声明设置最小秒数。 */
export function durationCtl(o: {
  value: number | null;
  label: string;
  disabled?: boolean;
  minSecs?: number;
  onChange: (v: number) => void;
}): TemplateResult {
  const secs = typeof o.value === 'number' ? o.value : 0;
  const { amount, unit } = splitDuration(secs);
  return html`<span class="dur">
    <input
      class="field-input num"
      type="number"
      inputmode="numeric"
      min=${Math.ceil((o.minSecs ?? 0) / unit)}
      aria-label=${translate('d4e2271', [o.label])}
      placeholder=${o.minSecs ? nothing : translate('d5d0ae6')}
      ?disabled=${o.disabled}
      .value=${live(secs > 0 ? String(amount) : '')}
      @input=${(e: Event) => {
        const raw = (e.target as HTMLInputElement).value;
        const n = raw === '' ? 0 : Math.max(0, Math.round(Number(raw)));
        o.onChange(n * unit);
      }}
    />
    <span class="select-wrap">
      <select
        class="select"
        aria-label=${translate('db12052', [o.label])}
        ?disabled=${o.disabled}
        .value=${live(String(unit))}
        @change=${(e: Event) => o.onChange((secs > 0 ? amount : 1) * Number((e.target as HTMLSelectElement).value))}
      >
        ${DURATION_UNITS.map((u) => html`<option value=${String(u.secs)} ?selected=${u.secs === unit}>${u.label}</option>`)}
      </select>
      ${icon('chevronDown')}
    </span>
  </span>`;
}

export function durationHuman(v: unknown): TemplateResult {
  return html`<span class="dur-human">${fmtDuration(v)}</span>`;
}

// ── 标签输入:用户 ID、群 ID 这类字符串列表 ─────────────────────────────

export class TagInput extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = {
    values: { attribute: false },
    members: { attribute: false },
    disabled: { type: Boolean },
    label: { type: String },
    placeholder: { type: String },
  };

  declare values: string[];
  declare members: Member[];
  declare disabled: boolean;
  declare label: string;
  declare placeholder: string;

  constructor() {
    super();
    this.values = [];
    this.members = [];
    this.disabled = false;
    this.label = '';
    this.placeholder = translate('d39d59e');
  }

  protected override createRenderRoot(): HTMLElement {
    return this;
  }

  private emit(values: string[]) {
    this.dispatchEvent(new CustomEvent<string[]>('change', { detail: values, bubbles: true }));
  }

  private add(raw: string): boolean {
    const items = raw
      .split(/[\s,，、]+/)
      .map((s) => s.trim())
      .filter(Boolean)
      .filter((s) => !this.values.includes(s));
    if (!items.length) return false;
    this.emit([...this.values, ...items]);
    return true;
  }

  private onKey(e: KeyboardEvent) {
    const input = e.target as HTMLInputElement;
    if ((e.key === 'Enter' || e.key === ',') && input.value.trim()) {
      e.preventDefault();
      if (this.add(input.value)) input.value = '';
    } else if (e.key === 'Backspace' && !input.value && this.values.length) {
      this.emit(this.values.slice(0, -1));
    }
  }

  private onBlur(e: FocusEvent) {
    const input = e.target as HTMLInputElement;
    if (input.value.trim() && this.add(input.value)) input.value = '';
  }

  protected override render(): TemplateResult {
    const names = new Map(this.members.map((m) => [m.id, m.name]));
    return html`<div
      class="tags"
      aria-disabled=${this.disabled ? 'true' : 'false'}
      @click=${(e: Event) => {
        if (e.target === e.currentTarget) (this.querySelector('.tags-input') as HTMLInputElement | null)?.focus();
      }}
    >
      ${this.values.map((v) => {
        const name = names.get(v);
        return html`<span class="tag" title=${v}>
          ${isLookupId(v) ? ident(v, { name: name ?? '' }) : html`<span class="tag-id">${v}</span>`}
          ${this.disabled
            ? nothing
            : html`<button
                type="button"
                class="tag-x"
                aria-label=${translate('da32111', [name ?? v])}
                @click=${() => this.emit(this.values.filter((x) => x !== v))}
              >
                ${icon('x', 'icon-sm')}
              </button>`}
        </span>`;
      })}
      ${this.disabled
        ? this.values.length
          ? nothing
          : html`<span class="tag-id">${rich('d2f8267')}</span>`
        : html`<input
            class="tags-input"
            type="text"
            spellcheck="false"
            autocomplete="off"
            aria-label=${this.label}
            placeholder=${this.values.length ? '' : this.placeholder}
            @keydown=${this.onKey}
            @blur=${this.onBlur}
          />`}
    </div>`;
  }
}
customElements.define('sm-tags', TagInput);

// ── 键值编辑器:映射类设置 ──────────────────────────────────────────────

export const REDACTED = '«summon:redacted»';

interface KvRow {
  k: string;
  v: string;
}

export class KvEditor extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = {
    value: { attribute: false },
    disabled: { type: Boolean },
    sensitive: { type: Boolean },
    label: { type: String },
    keyPlaceholder: { type: String },
    valuePlaceholder: { type: String },
  };

  declare value: Record<string, string>;
  declare disabled: boolean;
  declare sensitive: boolean;
  declare label: string;
  declare keyPlaceholder: string;
  declare valuePlaceholder: string;
  private rows: KvRow[] = [];
  private lastEmitted: Record<string, string> | null = null;

  constructor() {
    super();
    this.value = {};
    this.disabled = false;
    this.sensitive = false;
    this.label = '';
    this.keyPlaceholder = translate('d721b00');
    this.valuePlaceholder = translate('dcda1d5');
  }

  protected override createRenderRoot(): HTMLElement {
    return this;
  }

  protected override willUpdate(changed: PropertyValues<this>): void {
    // 外部换了值(恢复、放弃、重新载入)才重建行;自己刚发出去的值不重建,免得打字时丢焦点。
    if (changed.has('value') && !sameValue(this.value, this.lastEmitted)) {
      this.rows = Object.entries(this.value ?? {}).map(([k, v]) => ({ k, v: String(v) }));
    }
  }

  private emit() {
    const out: Record<string, string> = {};
    for (const r of this.rows) if (r.k.trim()) out[r.k.trim()] = r.v;
    this.lastEmitted = out;
    this.dispatchEvent(new CustomEvent<Record<string, string>>('change', { detail: out, bubbles: true }));
  }

  private edit(i: number, patch: Partial<KvRow>) {
    this.rows = this.rows.map((r, j) => (j === i ? { ...r, ...patch } : r));
    this.emit();
    this.requestUpdate();
  }

  protected override render(): TemplateResult {
    return html`<div class="kv" role="group" aria-label=${this.label}>
      ${this.rows.map((r, i) => {
        const hidden = this.sensitive && r.v === REDACTED;
        return html`<div class="kv-row">
          <div class="kv-key">
            <input
              class="field-input mono"
              type="text"
              spellcheck="false"
              autocomplete="off"
              aria-label=${translate('d5d2bce', [i + 1])}
              placeholder=${this.keyPlaceholder}
              ?disabled=${this.disabled}
              .value=${live(r.k)}
              @input=${(e: Event) => this.edit(i, { k: (e.target as HTMLInputElement).value })}
            />
            ${isLookupId(r.k.trim()) ? html`<span class="kv-ident">${ident(r.k.trim())}</span>` : nothing}
          </div>
          <input
            class="field-input mono kv-val"
            type="text"
            spellcheck="false"
            autocomplete="off"
            aria-label=${translate('d5cb36c', [i + 1])}
            placeholder=${hidden ? translate('da8c392') : this.valuePlaceholder}
            ?disabled=${this.disabled}
            .value=${live(hidden ? '' : r.v)}
            @input=${(e: Event) => {
              const v = (e.target as HTMLInputElement).value;
              // 清空等于没动:占位符原样带回,服务端还原原值。
              this.edit(i, { v: this.sensitive && v === '' && this.value[r.k] === REDACTED ? REDACTED : v });
            }}
          />
          ${this.disabled
            ? nothing
            : html`<button
                type="button"
                class="icon-btn"
                aria-label=${translate('d94c3a4', [r.k || translate('d5fb0f5', [i + 1])])}
                @click=${() => {
                  this.rows = this.rows.filter((_, j) => j !== i);
                  this.emit();
                  this.requestUpdate();
                }}
              >
                ${icon('x')}
              </button>`}
        </div>`;
      })}
      ${this.disabled
        ? this.rows.length
          ? nothing
          : html`<span class="muted">${rich('d2f8267')}</span>`
        : html`<button
            type="button"
            class="btn btn-sm btn-ghost kv-add"
            @click=${() => {
              this.rows = [...this.rows, { k: '', v: '' }];
              this.requestUpdate();
              void this.updateComplete.then(() => {
                const inputs = this.querySelectorAll<HTMLInputElement>('.kv-row input');
                inputs[inputs.length - 2]?.focus();
              });
            }}
          >
            ${rich('d4b67ac', [icon('plus', 'icon-sm')])}
          </button>`}
    </div>`;
  }
}
customElements.define('sm-kv', KvEditor);
