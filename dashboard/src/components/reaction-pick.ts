import { t as translate, rich } from '../i18n';
// 四个表情键合成一块:左边每个键一个格子,右边是这一层可选的表情。把表情拖进格子,或者先点
// 格子、再点表情;手机上没有拖放,点选照样能用。候选只来自这一层生效的表情安全集。
import { html, nothing, type TemplateResult } from 'lit';
import { live } from 'lit/directives/live.js';
import type { FieldDesc, FieldOption } from '../api/types';
import { emojiFace, hasEmojiFace } from '../preview/emoji-face';
import { LAYER_LABEL } from '../util/format';
import { RERENDER_EVENT } from './events';
import type { RowEnv } from './field-row';
import { icon } from './icon';

export const REACTION_KEYS = [
  'messaging.working_reaction',
  'messaging.done_reaction',
  'messaging.interrupted_reaction',
  'messaging.default_reaction',
];

const DRAG_TYPE = 'text/plain';

/** 正在填的格子与搜索词。行的模板由页面重画,这两样记在模板看得见的地方(同 field-row 的展开态)。 */
let active = REACTION_KEYS[0]!;
let query = '';

function rerender(target: EventTarget | null) {
  target?.dispatchEvent(new CustomEvent(RERENDER_EVENT, { bubbles: true }));
}

function tip(key: string, meaning: string): string {
  return meaning ? `${key} · ${meaning}` : key;
}

function shortLabel(f: FieldDesc): string {
  return f.label.replace(/表情$/, '') || f.label;
}

function slot(f: FieldDesc, env: RowEnv, meanings: Map<string, string>, disabled: boolean): TemplateResult {
  const { draft } = env;
  const fv = draft.fieldValue(f.key)!;
  const shown = draft.shown(f.key).value;
  const key = typeof shown === 'string' ? shown : '';
  const meaning = meanings.get(key) ?? '';
  const changed = draft.isChanged(f.key);
  const pendingInherit = changed && draft.changes.get(f.key) === null;
  const setHere = fv.value !== null && fv.value !== undefined;
  const err = draft.errors.get(f.key);
  const inheritWord = env.level === 'global' ? translate('dba2e93') : translate('d5bd845');
  const isActive = active === f.key && !disabled;
  const pv = env.preview;
  const put = (k: string) => {
    active = f.key;
    draft.setShown(f.key, k);
  };
  const leave = (e: DragEvent) => {
    const el = e.currentTarget as HTMLElement;
    if (!el.contains(e.relatedTarget as Node | null)) el.classList.remove('is-over');
  };
  const before = setHere ? String(fv.value) : translate('d6492ec', [String(fv.effective ?? '')]);
  return html`<div
    class="rx-slot ${isActive ? 'is-active' : ''} ${changed ? 'is-changed' : ''} ${env.target === f.key ? 'is-target' : ''}"
    id=${`f-${f.key}`}
    data-key=${f.key}
    @mouseenter=${() => pv?.setFocus(f.key)}
    @mouseleave=${() => pv?.setFocus(null)}
    @focusin=${() => pv?.setFocus(f.key)}
    @dragover=${(e: DragEvent) => {
      if (disabled) return;
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
      (e.currentTarget as HTMLElement).classList.add('is-over');
    }}
    @dragleave=${leave}
    @drop=${(e: DragEvent) => {
      e.preventDefault();
      (e.currentTarget as HTMLElement).classList.remove('is-over');
      const k = e.dataTransfer?.getData(DRAG_TYPE) ?? '';
      if (!disabled && meanings.has(k)) put(k);
    }}
  >
    <button
      type="button"
      class="rx-pick"
      aria-pressed=${isActive ? 'true' : 'false'}
      aria-label=${`${f.label}：${key ? tip(key, meaning) : translate('d2f5f1d')}${disabled ? '' : translate('d7ecaf4')}`}
      title=${f.description}
      ?disabled=${disabled}
      @click=${(e: Event) => {
        active = f.key;
        rerender(e.currentTarget);
      }}
    >
      <span class="rx-face">${key ? emojiFace(key, meaning) : html`<span class="emo-word">${rich('d2f5f1d')}</span>`}</span>
      <span class="rx-slot-label">${shortLabel(f)}</span>
      <span class="rx-sub"
        ><span class="rx-key">${key || '—'}</span>${key
          ? html`<span class="rx-meaning">${meanings.has(key) ? meaning : translate('d4b3305')}</span>`
          : nothing}</span
      >
    </button>
    <div class="rx-meta">
      ${pendingInherit
        ? html`<span>${rich('d90aa39', [inheritWord])}</span><button class="link-btn" @click=${() => draft.revert(f.key)}>${rich('d926a50')}</button>`
        : changed
          ? html`<span class="chip">${rich('d9fc6ec', [LAYER_LABEL[env.level]])}</span><span class="row-orig">${rich('dc2daf0')} <s>${before}</s></span>`
          : setHere
            ? html`<span class="chip">${LAYER_LABEL[env.level]}</span>${disabled
                  ? nothing
                  : html`<button class="link-btn quiet" @click=${() => draft.inherit(f.key)}>${icon('rotateCcw', 'icon-sm')}${inheritWord}</button>`}`
            : html`<span class="chip chip-quiet">${rich('d352629', [LAYER_LABEL[fv.source]])}</span>`}
    </div>
    ${err ? html`<p class="row-error" role="alert">${icon('circleAlert', 'icon-sm')}<span>${err}</span></p>` : nothing}
  </div>`;
}

function palette(options: FieldOption[], env: RowEnv, target: FieldDesc | undefined): TemplateResult {
  const q = query.trim();
  const lower = q.toLowerCase();
  const list = q ? options.filter((o) => o.value.toLowerCase().includes(lower) || o.label.includes(q)) : options;
  const current = target ? env.draft.shown(target.key).value : null;
  return html`<div class="rx-palette">
    <div class="rx-palette-head">
      <span class="rx-search">
        ${icon('search', 'icon-sm')}
        <input
          class="field-input"
          type="search"
          aria-label="${translate('d6904a2')}"
          placeholder="${translate('ddb71a4')}"
          .value=${live(query)}
          @input=${(e: Event) => {
            query = (e.target as HTMLInputElement).value;
            rerender(e.currentTarget);
          }}
        />
      </span>
      ${target ? html`<span class="rx-hint">${rich('d6e68d6', [shortLabel(target)])}</span>` : nothing}
    </div>
    <div class="rx-grid" role="group" aria-label="${translate('ddba2f8')}">
      ${list.map(
        (o) => html`<button
          type="button"
          class="rx-tile ${hasEmojiFace(o.value) ? '' : 'rx-tile--word'} ${o.value === current ? 'is-current' : ''}"
          title=${tip(o.value, o.label)}
          aria-label=${tip(o.value, o.label)}
          aria-pressed=${o.value === current ? 'true' : 'false'}
          draggable=${target ? 'true' : 'false'}
          ?disabled=${!target}
          @dragstart=${(e: DragEvent) => {
            e.dataTransfer?.setData(DRAG_TYPE, o.value);
            if (e.dataTransfer) e.dataTransfer.effectAllowed = 'copy';
          }}
          @click=${() => {
            if (target) env.draft.setShown(target.key, o.value);
          }}
        >
          ${emojiFace(o.value, o.label)}
        </button>`,
      )}
      ${list.length ? nothing : html`<p class="rx-empty">${rich('dc22885', [q])}</p>`}
    </div>
  </div>`;
}

/** 表情这一块;这一层没有候选时返回 null,由调用方照普通行显示。 */
export function reactionBlock(fields: FieldDesc[], env: RowEnv): TemplateResult | null {
  const options = env.draft.view?.reactions;
  if (!options?.length || !fields.length) return null;
  const meanings = new Map(options.map((o): [string, string] => [o.value, o.label]));
  const fv = env.draft.fieldValue(fields[0]!.key);
  const disabled = !fields.some((f) => env.draft.fieldValue(f.key)?.editable && f.write !== 'readonly');
  if (!fields.some((f) => f.key === active)) active = fields[0]!.key;
  const target = disabled ? undefined : fields.find((f) => f.key === active);
  const changed = fields.some((f) => env.draft.isChanged(f.key));
  const lock = disabled ? (fv?.deny_reason ?? translate('d5f24a8')) : '';
  return html`<div class="row row--block row--reactions ${changed ? 'is-changed' : ''} ${disabled ? 'is-disabled' : ''}">
    <div class="row-head">
      <div class="row-title"><span class="row-label">${rich('dc762ed')}</span></div>
      <p class="row-desc">${rich('d03d069')}</p>
      ${lock ? html`<span class="row-lock">${icon('lock', 'icon-sm')}<span>${lock}</span></span>` : nothing}
    </div>
    <div class="rx">
      <div class="rx-slots">${fields.map((f) => slot(f, env, meanings, disabled))}</div>
      ${palette(options, env, target)}
    </div>
  </div>`;
}
