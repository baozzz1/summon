import { t as translate } from '../i18n';
// 原生 CoT 消息的画法:一个可折叠的过程块,条目是思考段与工具步骤。和飞书里一样点标题行折叠 / 展开;
// 回放这一轮时标题扫光、图标转圈,条目逐条出现,对应飞书里这一轮还在跑的样子。
import { html, type TemplateResult } from 'lit';
import { icon } from '../components/icon';
import type { IconName } from '../components/icons';
import type { CotItem, PreviewMessage } from '../api/types';
import type { Mark } from './feishu-card';

type CotMessage = Extract<PreviewMessage, { kind: 'cot' }>;

const TOOL_ICONS: Record<string, IconName> = {
  bash: 'terminal',
  read: 'fileText',
  write: 'pencil',
  search: 'search',
  doc: 'fileText',
  task: 'circleCheck',
  calendar: 'clock',
  meeting: 'clock',
};

function item(it: CotItem, mark: Mark): TemplateResult {
  switch (it.type) {
    case 'thinking':
      return html`<div class="fs-cot-item thinking ${mark(it.id)}" data-eid=${it.id}>${it.text}</div>`;
    case 'text':
      return html`<div class="fs-cot-item ${mark(it.id)}" data-eid=${it.id}>${it.text}</div>`;
    case 'tool':
      return html`<div class="fs-cot-item ${mark(it.id)}" data-eid=${it.id}>
        <div class="fs-tool">
          ${icon(TOOL_ICONS[it.icon] ?? 'terminal')}<span class="fs-tool-name">${it.name}</span>${it.args
            ? html`<span class="fs-tool-args">${it.args}</span>`
            : ''}
        </div>
        ${it.result ? html`<pre class="fs-tool-result">${it.result}</pre>` : ''}
      </div>`;
  }
}

/** 过程块的折叠状态由预览面板持有,重渲染(拨开关)时保持。 */
export interface CotFold {
  collapsed: boolean;
  toggle: () => void;
}

/** 回放中的过程:已经出现的条目数。 */
export interface CotLive {
  shown: number;
}

export function renderCot(m: CotMessage, mark: Mark, fold: CotFold, live: CotLive | null = null): TemplateResult {
  const items = live ? m.items.slice(0, live.shown) : m.items;
  const steps = items.length;
  const status = live ? translate('ddc9591') : m.status === 'error' ? translate('d01ad2b') : translate('df28461');
  const open = steps > 0 && !fold.collapsed;
  const state = live ? 'running' : m.status;
  const glyph = live
    ? icon('loaderCircle', 'icon-sm fs-spin')
    : icon(m.status === 'error' ? 'circleX' : 'circleCheck', 'icon-sm');
  return html`<div class="fs-cot ${state} ${mark(m.id)}" data-eid=${m.id} ?data-open=${open}>
    <button type="button" class="fs-cot-head" aria-expanded=${open ? 'true' : 'false'} ?disabled=${!steps} @click=${fold.toggle}>
      ${glyph}
      <span class="fs-cot-title ${live ? 'fs-shimmer' : ''}">${m.title}</span>
      <span class="fs-cot-steps">${steps ? translate('d310f38', [status, steps]) : status}</span>
      ${steps ? icon('chevronDown', 'icon-sm fs-cot-chevron') : ''}
    </button>
    ${open ? html`<div class="fs-cot-body">${items.map((it) => item(it, mark))}</div>` : ''}
  </div>`;
}
