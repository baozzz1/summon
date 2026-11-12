import { t as translate, rich } from '../i18n';
// 飞书卡片 2.0 的前端画法。supported-tags.json 由 Rust 侧测试列举 summon 实际会发出的元素,
// 这里必须全部画得出(契约测试钉着);另外多认 img、collapsible_panel 两种常见元素。认不出的
// 元素原地画占位,不静默丢掉。按钮与链接只是样子,不可点、不可聚焦。
import { html, nothing, type TemplateResult } from 'lit';
import { icon } from '../components/icon';
import { renderMarkdown } from './markdown';
import SUPPORTED from './supported-tags.json';

export const SUPPORTED_TAGS: readonly string[] = SUPPORTED;

type El = Record<string, unknown>;

/** 给带 element_id 的块追加描边 / 高亮的类名。 */
export type Mark = (id: string) => string;

const noMark: Mark = () => '';

function textOf(v: unknown): string {
  if (typeof v === 'string') return v;
  if (v && typeof v === 'object' && typeof (v as El).content === 'string') return (v as El).content as string;
  return '';
}

function idAttrs(el: El, mark: Mark): { eid: string | typeof nothing; cls: string } {
  const id = typeof el.element_id === 'string' ? el.element_id : '';
  return { eid: id || nothing, cls: id ? mark(id) : '' };
}

type Renderer = (el: El, mark: Mark) => TemplateResult;

// CSP 不允许内联 style,间距按档位映射成类名。
function gapClass(v: unknown): string {
  const named: Record<string, number> = { small: 4, medium: 8, large: 12, extra_large: 16 };
  const px = typeof v === 'string' ? (named[v] ?? Number.parseInt(v, 10)) : 8;
  const steps = [0, 4, 8, 12, 16];
  const n = Number.isFinite(px) ? steps.reduce((a, b) => (Math.abs(b - px) < Math.abs(a - px) ? b : a)) : 8;
  return `gap-${n}`;
}

const children = (list: unknown, mark: Mark) =>
  Array.isArray(list) ? (list as El[]).map((c) => renderElement(c, mark)) : nothing;

const RENDERERS: Record<string, Renderer> = {
  markdown: (el, mark) => {
    const { eid, cls } = idAttrs(el, mark);
    const size = el.text_size === 'notation' ? 'notation' : '';
    const align = el.text_align === 'center' ? 'align-center' : el.text_align === 'right' ? 'align-right' : '';
    return html`<div class="fsc-md ${size} ${align} ${cls}" data-eid=${eid}>${renderMarkdown(textOf(el.content))}</div>`;
  },
  hr: (el, mark) => {
    const { eid, cls } = idAttrs(el, mark);
    return html`<div class="fsc-hr ${cls}" data-eid=${eid} role="separator"></div>`;
  },
  img: (el, mark) => {
    const { eid, cls } = idAttrs(el, mark);
    const alt = textOf(el.alt) || translate('dd24c10');
    // 预览不联网:img_key 取不到真图,画一块占位并写出替代文字。
    return html`<div class="fsc-img ${cls}" data-eid=${eid}>${icon('fileText', 'icon-sm')}<span>${alt}</span></div>`;
  },
  column_set: (el, mark) => {
    const { eid, cls } = idAttrs(el, mark);
    const mode = typeof el.flex_mode === 'string' ? el.flex_mode : 'none';
    return html`<div class="fsc-cols ${mode} ${gapClass(el.horizontal_spacing)} ${cls}" data-eid=${eid}>${children(el.columns, mark)}</div>`;
  },
  column: (el, mark) => {
    const { eid, cls } = idAttrs(el, mark);
    const width = el.width === 'weighted' ? 'weighted' : 'auto';
    const va = el.vertical_align === 'center' ? 'v-center' : el.vertical_align === 'bottom' ? 'v-bottom' : '';
    return html`<div class="fsc-col ${width} ${va} ${cls}" data-eid=${eid}>${children(el.elements, mark)}</div>`;
  },
  button: (el, mark) => {
    const { eid, cls } = idAttrs(el, mark);
    const type = typeof el.type === 'string' ? el.type : 'default';
    return html`<span class="fsc-btn ${type} ${el.disabled ? 'disabled' : ''} ${cls}" data-eid=${eid} aria-hidden="true"
      >${textOf(el.text)}</span
    >`;
  },
  collapsible_panel: (el, mark) => {
    const { eid, cls } = idAttrs(el, mark);
    const header = (el.header ?? {}) as El;
    const title = textOf(header.title);
    return html`<div class="fsc-panel ${el.expanded ? 'expanded' : ''} ${cls}" data-eid=${eid}>
      <div class="fsc-panel-head">${icon('chevronRight', 'icon-sm')}<span>${renderMarkdown(title)}</span></div>
      <div class="fsc-panel-body">${children(el.elements, mark)}</div>
    </div>`;
  },
};

export function renderElement(el: El, mark: Mark = noMark): TemplateResult {
  const tag = typeof el?.tag === 'string' ? el.tag : '';
  const r = Object.hasOwn(RENDERERS, tag) ? RENDERERS[tag] : undefined;
  if (!r) {
    const { eid, cls } = idAttrs(el ?? {}, mark);
    return html`<div class="fsc-unknown ${cls}" data-eid=${eid}>${rich('d04038f')} <code>&lt;${tag || '?'}&gt;</code></div>`;
  }
  return r(el, mark);
}

export function renderCard(card: unknown, mark: Mark = noMark): TemplateResult {
  const c = (card ?? {}) as { body?: { elements?: unknown }; elements?: unknown };
  const elements = c.body?.elements ?? c.elements;
  return html`<div class="fsc">${children(elements, mark)}</div>`;
}

export function hasRenderer(tag: string): boolean {
  return tag in RENDERERS;
}
