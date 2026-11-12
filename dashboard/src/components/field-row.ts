import { t as translate, rich } from '../i18n';
// 一项设置一行:名称 + 键路径、说明、来源、生效时机、控件、改动态、错误、权限。
import { html, nothing, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type { FieldDesc, FieldGroup, FieldOption, FieldValue, Member, Schema, Section } from '../api/types';
import { brandIcon, DEFAULT_ICON } from '../app/brand';
import type { Caller, ConfigDraft } from '../app/editable';
import type { PreviewController } from '../preview/controller';
import { EFFECT_LABEL, fmtValue, LAYER_LABEL } from '../util/format';
import { durationCtl, durationHuman, numberCtl, segmented, selectCtl, switchCtl, textCtl } from './controls';
import { RERENDER_EVENT } from './events';
import { icon } from './icon';
import { inlineCode } from './inline';
import { REACTION_KEYS, reactionBlock } from './reaction-pick';
import { toast } from './toast';

export interface RowEnv {
  draft: ConfigDraft;
  level: 'global' | 'agent' | 'chat';
  preview?: PreviewController | null;
  members?: Member[];
  /** 按键覆盖候选(如 agent 的模型目录、档位) */
  options?: Record<string, FieldOption[]>;
  suggestions?: Record<string, string[]>;
  target?: string | null;
  /** 整块都不可改、原因已在块上写过一次时,每行只留一个锁标记 */
  quietLock?: boolean;
  /** 行内要问服务端的控件用(如从 lark-cli 读 open_id) */
  call?: Caller;
}

const FROM_LABEL: Record<string, string> = { get builtin() { return translate('d03c3f6'); }, get global() { return translate('d63d6b4'); }, agent: ' agent', get chat() { return translate('d0d5f45'); } };

const BLOCK_KINDS = new Set(['string_list', 'string_map']);
// 宽屏说明收成两行(约 100 字),更长的才给「展开」;窄屏说明全文显示(没有悬停可看)。
const LONG_DESC = 100;
/** 展开过说明的行;行的 class 由模板管,展开态也得记在模板看得见的地方。 */
const expanded = new Set<string>();

function isMono(f: FieldDesc): boolean {
  return f.kind === 'path' || /url|bind|command|model|_env|token/.test(f.key);
}

function overrideSummary(names: string[]): string {
  const chats = names.filter((n) => n.includes(' · ')).length;
  const agents = names.length - chats;
  const parts = [agents ? translate('ddf1c5d', [agents]) : '', chats ? translate('d2ef6e4', [chats]) : ''].filter(Boolean);
  return translate('dafd932', [parts.join('、')]);
}

/** 取反项:YAML 里存的是与开关相反的含义(如 hide_thinking 存「隐藏」,开关是「显示思考」)。 */
function invertedTip(f: FieldDesc): string {
  const leaf = f.key.split('.').pop() ?? f.key;
  return translate('da46b6d', [leaf, leaf, leaf]);
}

function invertedTag(f: FieldDesc): TemplateResult {
  const id = `inv:${f.key}`;
  return html`<button
    type="button"
    class="inv-tag"
    title=${invertedTip(f)}
    aria-expanded=${expanded.has(id) ? 'true' : 'false'}
    @click=${(e: Event) => {
      if (expanded.has(id)) expanded.delete(id);
      else expanded.add(id);
      e.currentTarget?.dispatchEvent(new CustomEvent(RERENDER_EVENT, { bubbles: true }));
    }}
  >
    ${rich('dd5061c')}
  </button>`;
}

const LARK_OWNER_KEY = 'sentinel.owner_open_id';
/** 正在问 lark-cli(按钮转圈用) */
let readingLarkUser = false;

function rerender(target: EventTarget | null) {
  target?.dispatchEvent(new CustomEvent(RERENDER_EVENT, { bubbles: true }));
}

/** 「你的 open_id」:问 sentinel 会用的那份 lark-cli 配置里登录的是谁,填进草稿。 */
async function fillLarkUser(env: RowEnv, call: Caller, target: EventTarget | null) {
  const profile = env.draft.shown('sentinel.lark_profile').value;
  readingLarkUser = true;
  env.draft.errors.delete(LARK_OWNER_KEY);
  rerender(target);
  try {
    const user = await call('sentinel.lark_user', typeof profile === 'string' && profile.trim() ? { profile } : {});
    env.draft.setShown(LARK_OWNER_KEY, user.open_id);
    toast(user.name ? translate('defc174', [user.name]) : translate('dd6b0dc'), 'ok');
  } catch (err) {
    env.draft.errors.set(LARK_OWNER_KEY, errorMessage(err));
  } finally {
    readingLarkUser = false;
    rerender(target);
  }
}

async function copyKey(key: string) {
  try {
    await navigator.clipboard.writeText(key);
    toast(translate('dac4717', [key]), 'ok', 1800);
  } catch {
    toast(translate('d5fe28d'), 'warn');
  }
}

function control(f: FieldDesc, fv: FieldValue, env: RowEnv, disabled: boolean): TemplateResult {
  const { draft } = env;
  // 只读项直接写出值,不画一个点不动的控件。
  if (f.write === 'readonly') {
    return html`<span class="ro-value ${isMono(f) || f.kind === 'enum' ? 'mono' : ''}">${fmtValue(fv.effective, f.kind, f.options)}</span>`;
  }
  const shown = draft.shown(f.key);
  const set = (v: unknown) => draft.setShown(f.key, v);
  const label = f.label;

  if (env.level === 'chat' && f.kind === 'bool') {
    const cur = draft.isChanged(f.key) ? (draft.changes.get(f.key) as boolean | null) : (fv.value as boolean | null);
    const v = cur === null || cur === undefined ? 'inherit' : cur ? 'on' : 'off';
    return segmented({
      label,
      value: v,
      disabled,
      options: [
        { value: 'inherit', label: translate('d49f1e2') },
        { value: 'on', label: translate('d39eae6') },
        { value: 'off', label: translate('d5d0ae6') },
      ],
      onChange: (x) => draft.set(f.key, x === 'inherit' ? null : x === 'on'),
    });
  }

  // Menu Bar 图标:直接画出图标来选,选中的也就是控制台左上角与标签页上的那个。
  if (f.key === 'menubar.icon') {
    const value = typeof shown.value === 'string' && shown.value ? shown.value : DEFAULT_ICON;
    return html`<div class="icon-pick" role="radiogroup" aria-label=${label}>
      ${(f.options ?? []).map(
        (o) => html`<label class="icon-pick-opt" title=${o.label}>
          <input type="radio" name="menubar-icon" .checked=${value === o.value} ?disabled=${disabled} @change=${() => set(o.value)} />
          ${brandIcon(o.value, 'icon-pick-glyph')}<span>${o.label}</span>
        </label>`,
      )}
    </div>`;
  }

  if (f.key === LARK_OWNER_KEY && env.call) {
    const call = env.call;
    return html`<div class="ctl-with-action">
      ${textCtl({
        label,
        value: shown.value === null || shown.value === undefined ? '' : String(shown.value),
        mono: true,
        wide: true,
        disabled,
        placeholder: translate('da73dfc'),
        onChange: set,
      })}
      <button
        type="button"
        class="btn btn-sm"
        ?disabled=${disabled || readingLarkUser}
        @click=${(e: Event) => void fillLarkUser(env, call, e.currentTarget)}
      >
        ${rich('dd0778f', [readingLarkUser ? icon('loaderCircle', 'icon-sm fs-spin') : icon('terminal', 'icon-sm')])}
      </button>
    </div>`;
  }

  switch (f.kind) {
    case 'bool':
      return switchCtl({ checked: shown.value === true, label, disabled, onChange: set });
    case 'enum': {
      const options = env.options?.[f.key] ?? f.options ?? [];
      const value = shown.value === null || shown.value === undefined ? null : String(shown.value);
      return options.length <= 4
        ? segmented({ label, value, options, disabled, onChange: set })
        : selectCtl({ label, value, options, disabled, onChange: set });
    }
    case 'int':
      return numberCtl({
        label,
        value: typeof shown.value === 'number' ? shown.value : null,
        disabled,
        onChange: (n) => set(n),
        ...(f.min !== undefined ? { min: f.min } : {}),
        ...(f.max !== undefined ? { max: f.max } : {}),
      });
    case 'duration_secs':
      return html`${durationCtl({
        label,
        ...(f.min !== undefined ? { minSecs: f.min } : {}),
        value: typeof shown.value === 'number' ? shown.value : null,
        disabled,
        onChange: set,
      })}${durationHuman(shown.value)}`;
    case 'string':
    case 'path': {
      const opts = env.options?.[f.key];
      if (opts?.length) {
        // 候选里的空值表示不设置:选它等于恢复默认,不写一个空字符串。
        const value = shown.value === null || shown.value === undefined ? '' : String(shown.value);
        return selectCtl({ label, value, options: opts, disabled, onChange: (v) => set(v === '' ? null : v) });
      }
      const sugg = env.suggestions?.[f.key];
      const listId = sugg ? `dl-${f.key.replace(/\W/g, '-')}` : undefined;
      return html`${textCtl({
        label,
        value: shown.value === null || shown.value === undefined ? '' : String(shown.value),
        mono: isMono(f),
        wide: true,
        disabled,
        placeholder: f.kind === 'path' ? translate('d13924b') : '',
        onChange: set,
        ...(listId ? { list: listId } : {}),
      })}${sugg ? html`<datalist id=${listId}>${sugg.map((s) => html`<option value=${s}></option>`)}</datalist>` : nothing}`;
    }
    case 'string_list':
      return html`<sm-tags
        .values=${Array.isArray(shown.value) ? (shown.value as string[]) : []}
        .members=${env.members ?? []}
        .label=${label}
        ?disabled=${disabled}
        placeholder=${f.key.startsWith('feishu.') ? translate('d083393') : translate('d39d59e')}
        @change=${(e: CustomEvent<string[]>) => set(e.detail)}
      ></sm-tags>`;
    case 'string_map':
      return html`<sm-kv
        .value=${(shown.value && typeof shown.value === 'object' ? shown.value : {}) as Record<string, string>}
        .label=${label}
        ?sensitive=${f.sensitive}
        ?disabled=${disabled}
        keyPlaceholder=${f.key.endsWith('chat_working_dirs') ? translate('d97a07b') : f.key.endsWith('by_cli') ? 'CLI' : f.key === 'aliases' ? translate('d3526f4') : translate('d721b00')}
        valuePlaceholder=${f.key.endsWith('chat_working_dirs') ? translate('d52daa7') : f.key === 'aliases' ? translate('d27521b') : translate('dcda1d5')}
        @change=${(e: CustomEvent<Record<string, string>>) => set(e.detail)}
      ></sm-kv>`;
  }
}

export function fieldRow(f: FieldDesc, env: RowEnv): TemplateResult | typeof nothing {
  const fv = env.draft.fieldValue(f.key);
  if (!fv) return nothing;
  const { draft } = env;
  const changed = draft.isChanged(f.key);
  const pendingInherit = changed && draft.changes.get(f.key) === null;
  const disabled = !fv.editable || f.write === 'readonly';
  const setHere = fv.value !== null && fv.value !== undefined;
  const block = BLOCK_KINDS.has(f.kind);
  // 开关很小,窄屏里也留在右边(同手机系统设置的写法),不单独占一行。
  const inline = (f.kind === 'bool' && env.level !== 'chat') || f.write === 'readonly';
  const err = draft.errors.get(f.key);
  const pv = env.preview;
  const hasRegion = !!pv?.hasRegion(f.key);
  const ownLabel = LAYER_LABEL[env.level] ?? env.level;
  const inheritWord = env.level === 'global' ? translate('dba2e93') : translate('d5bd845');

  const meta: Array<TemplateResult> = [];
  if (env.level === 'chat' && f.kind === 'bool' && !setHere && !changed) {
    meta.push(html`<span class="chip chip-quiet">${rich('d56e14e', [fmtValue(fv.effective, f.kind), FROM_LABEL[fv.source] ?? fv.source])}</span>`);
  } else if (!pendingInherit) {
    meta.push(
      setHere && !changed
        ? html`<span class="chip">${ownLabel}</span>`
        : !setHere && !changed
          ? html`<span class="chip chip-quiet">${rich('d352629', [LAYER_LABEL[fv.source]])}</span>`
          : html`<span class="chip">${rich('d9fc6ec', [ownLabel])}</span>`,
    );
  }
  if (fv.overridden_by.length) {
    meta.push(html`<details class="ovr" title=${fv.overridden_by.join('\n')}>
      <summary>${overrideSummary(fv.overridden_by)}</summary>
      <div class="ovr-list">${fv.overridden_by.map((n) => html`<span class="chip chip-quiet">${n}</span>`)}</div>
    </details>`);
  }
  if (f.effect !== 'next_turn') {
    meta.push(html`<span class="chip chip-quiet">${EFFECT_LABEL[f.effect] ?? f.effect}</span>`);
  }
  if (pendingInherit) {
    meta.push(html`<span>${rich('d90aa39', [inheritWord])}</span>
      <button class="link-btn" @click=${() => draft.revert(f.key)}>${rich('d926a50')}</button>`);
  } else if (setHere && !disabled && !(env.level === 'chat' && f.kind === 'bool')) {
    meta.push(html`<button class="link-btn quiet" @click=${() => draft.inherit(f.key)}>
      ${icon('rotateCcw', 'icon-sm')}${inheritWord}
    </button>`);
  }

  const orig = changed
    ? html`<span class="row-orig">${rich('dc2daf0')} <s>${setHere ? fmtValue(fv.value, f.kind, f.options) : translate('d1e58d8', [fmtValue(fv.effective, f.kind, f.options)])}</s></span>`
    : nothing;

  const lockText = f.write === 'readonly' ? translate('d3b5ec3') : !fv.editable ? (fv.deny_reason ?? translate('d5f24a8')) : '';

  return html`<div
    class="row ${changed ? 'is-changed' : ''} ${expanded.has(f.key) ? 'is-expanded' : ''} ${disabled ? 'is-disabled' : ''} ${block ? 'row--block' : ''} ${inline ? 'row--inline' : ''} ${env.target ===
    f.key
      ? 'is-target'
      : ''}"
    id=${`f-${f.key}`}
    data-key=${f.key}
    @mouseenter=${() => pv?.setFocus(f.key)}
    @mouseleave=${() => pv?.setFocus(null)}
    @focusin=${() => pv?.setFocus(f.key)}
  >
    <div class="row-grid">
      <div class="row-head">
        <div class="row-title">
          <span class="row-label" id=${`l-${f.key}`}>${f.label}</span>
          ${hasRegion
            ? html`<span class="preview-mark" title="${translate('d624706')}">${icon('eye', 'icon-sm')}</span>`
            : nothing}
        </div>
        <div class="row-keyline">
          <button class="row-key" title="${translate('dbbdf4f')}" @click=${() => copyKey(f.key)}>${f.key}${icon('copy')}</button>
          ${f.inverted ? invertedTag(f) : nothing}
        </div>
        ${f.inverted && expanded.has(`inv:${f.key}`) ? html`<p class="inv-note">${invertedTip(f)}</p>` : nothing}
        ${f.description
          ? html`<p class="row-desc" title=${f.description}>${inlineCode(f.description)}</p>
              ${f.description.length > LONG_DESC
                ? html`<button
                    class="link-btn quiet row-more"
                    aria-expanded=${expanded.has(f.key) ? 'true' : 'false'}
                    @click=${(e: Event) => {
                      if (expanded.has(f.key)) expanded.delete(f.key);
                      else expanded.add(f.key);
                      // 请所在页面重画一次(展开态记在模块里,不直接改 DOM,免得和 Lit 抢节点)
                      e.currentTarget?.dispatchEvent(new CustomEvent(RERENDER_EVENT, { bubbles: true }));
                    }}
                  >
                    ${expanded.has(f.key) ? translate('dafd4b7') : translate('d00bd39')}
                  </button>`
                : nothing}`
          : nothing}
        <div class="row-meta">${meta}</div>
      </div>
      <div class="row-ctl" title=${lockText || nothing}>
        ${control(f, fv, env, disabled)} ${orig}
        ${lockText && env.quietLock
          ? html`<span class="row-lock" title=${lockText}>${icon('lock', 'icon-sm')}</span>`
          : lockText && lockText !== translate('d3b5ec3')
          ? html`<span class="row-lock">${icon('lock', 'icon-sm')}<span>${inlineCode(lockText)}</span></span>`
          : lockText
            ? html`<span class="row-lock">${icon('lock', 'icon-sm')}<span>${rich('d3b5ec3')}</span></span>`
            : nothing}
      </div>
    </div>
    ${err ? html`<p class="row-error" role="alert">${icon('circleAlert', 'icon-sm')}<span>${err}</span></p>` : nothing}
  </div>`;
}

/**
 * 几个键在页面上合成一个控件。目前只有 Sentinel:字段表里是 sentinel.enabled(开启前服务端
 * 核验 lark-cli 身份)与 sentinel.dry_run 两个布尔,页面上是「关 / 演练 / 开」一组分段,
 * 选哪一档就把对应的键写进同一份草稿,随同一次 config.patch 提交。
 */
interface Composite {
  keys: string[];
  label: string;
  options: Array<{ value: string; label: string; set: Record<string, boolean> }>;
  read: (get: (key: string) => unknown) => string;
}

export const COMPOSITES: Composite[] = [
  {
    keys: ['sentinel.enabled', 'sentinel.dry_run'],
    get label() { return translate('d3e5c3b'); },
    options: [
      { value: 'off', get label() { return translate('d5d0ae6'); }, set: { 'sentinel.enabled': false } },
      { value: 'dry_run', get label() { return translate('d9fcfba'); }, set: { 'sentinel.enabled': true, 'sentinel.dry_run': true } },
      { value: 'on', get label() { return translate('d39eae6'); }, set: { 'sentinel.enabled': true, 'sentinel.dry_run': false } },
    ],
    read: (get) => (get('sentinel.enabled') !== true ? 'off' : get('sentinel.dry_run') === true ? 'dry_run' : 'on'),
  },
];

function compositeRow(c: Composite, fields: FieldDesc[], env: RowEnv): TemplateResult | typeof nothing {
  const { draft } = env;
  const fvs = c.keys.map((k) => draft.fieldValue(k));
  if (fvs.some((v) => !v)) return nothing;
  const primary = fields.find((f) => f.key === c.keys[0])!;
  const shown = (k: string) => draft.shown(k).value;
  const base = (k: string) => {
    const fv = draft.fieldValue(k)!;
    return fv.value ?? fv.effective;
  };
  const value = c.read(shown);
  const before = c.read(base);
  const changed = c.keys.some((k) => draft.isChanged(k));
  const editable = fvs.every((v) => v!.editable);
  const deny = fvs.find((v) => !v!.editable)?.deny_reason;
  const errs = c.keys.map((k) => draft.errors.get(k)).filter(Boolean);
  const setHere = fvs.some((v) => v!.value !== null && v!.value !== undefined);
  const labelOf = (v: string) => c.options.find((o) => o.value === v)?.label ?? v;
  const pick = (v: string) => {
    const opt = c.options.find((o) => o.value === v);
    if (!opt) return;
    for (const k of c.keys) {
      if (k in opt.set) draft.set(k, opt.set[k]);
      else draft.revert(k);
    }
  };
  return html`<div class="row row--composite ${changed ? 'is-changed' : ''} ${!editable ? 'is-disabled' : ''}" id=${`f-${c.keys[0]}`}>
    <div class="row-grid">
      <div class="row-head">
        <div class="row-title"><span class="row-label">${c.label}</span></div>
        <span class="row-key">${c.keys.join(' · ')}</span>
        ${primary.description ? html`<p class="row-desc" title=${primary.description}>${inlineCode(primary.description)}</p>` : nothing}
        <div class="row-meta">
          <span class="chip ${setHere ? '' : 'chip-quiet'}">${setHere ? LAYER_LABEL[env.level] : translate('d352629', [LAYER_LABEL[fvs[0]!.source]])}</span>
          ${primary.effect !== 'next_turn' ? html`<span class="chip chip-quiet">${EFFECT_LABEL[primary.effect]}</span>` : nothing}
          ${primary.write === 'guarded' ? html`<span class="chip chip-quiet">${rich('d5f8725')}</span>` : nothing}
        </div>
      </div>
      <div class="row-ctl" title=${deny ?? nothing}>
        ${segmented({ label: c.label, value, options: c.options, disabled: !editable, onChange: pick })}
        ${changed ? html`<span class="row-orig">${rich('dc2daf0')} <s>${labelOf(before)}</s></span>` : nothing}
        ${deny ? html`<span class="row-lock">${icon('lock', 'icon-sm')}<span>${deny}</span></span>` : nothing}
      </div>
    </div>
    ${errs.map((e) => html`<p class="row-error" role="alert">${icon('circleAlert', 'icon-sm')}<span>${e}</span></p>`)}
  </div>`;
}

export function groupsFor(schema: Schema, section: Section | Section[]): FieldGroup[] {
  const set = Array.isArray(section) ? section : [section];
  return schema.groups.filter((g) => set.includes(g.section));
}

export function fieldsIn(schema: Schema, group: string): FieldDesc[] {
  return schema.fields.filter((f) => f.group === group);
}

/**
 * 一个分组:标题 + 一张卡,只列出这一层视图里有值的项。bare:标题已在页头上(全局设置一组一页),
 * 这里只画卡。
 */
export function groupSection(
  g: FieldGroup,
  fields: FieldDesc[],
  env: RowEnv,
  opts: { bare?: boolean } = {},
): TemplateResult | typeof nothing {
  const present = fields.filter((f) => env.draft.fieldValue(f.key));
  if (!present.length) return nothing;
  const composites = COMPOSITES.filter((c) => c.keys.every((k) => present.some((f) => f.key === k)));
  const merged = new Set(composites.flatMap((c) => c.keys));
  // 表情几个键合成一块,放在第一个表情键原来的位置。
  const rxFields = present.filter((f) => REACTION_KEYS.includes(f.key));
  const rx = rxFields.length ? reactionBlock(rxFields, env) : null;
  const rows = present.flatMap((f) => {
    if (merged.has(f.key)) return [];
    if (rx && REACTION_KEYS.includes(f.key)) return f === rxFields[0] ? [rx] : [];
    return [fieldRow(f, env)];
  });
  const card = html`<div class="card">${composites.map((c) => compositeRow(c, present, env))} ${rows}</div>`;
  if (opts.bare) {
    return html`<section class="group" id=${`g-${g.id}`} aria-label=${g.label}>${card}</section>`;
  }
  return html`<section class="group" id=${`g-${g.id}`} aria-labelledby=${`gh-${g.id}`}>
    <header class="group-head">
      <div>
        <h2 id=${`gh-${g.id}`}>${g.label}</h2>
        ${g.description ? html`<p>${inlineCode(g.description)}</p>` : nothing}
      </div>
    </header>
    ${card}
  </section>`;
}
