import { t as translate, rich } from '../i18n';
// 会话的四个行内动作:已处理、打断、退出、归档。列表与详情页共用;动作走与 Menu Bar、卡片同一条受控
// 通道(服务端),这里只负责确认、调用与把结果说成人话。
import { html, nothing, type TemplateResult } from 'lit';
import type { ActOutcome, SessionAction, SessionRow } from '../api/types';
import type { Caller } from '../app/editable';
import { errorMessage } from '../api/client';
import { confirmDialog } from './dialog';
import { icon } from './icon';
import type { IconName } from './icons';
import { toast } from './toast';

const ACTIONS: Array<{ action: SessionAction; label: string; icon: IconName; danger?: boolean }> = [
  { action: 'handled', get label() { return translate('d83c5d1'); }, icon: 'checkCheck' },
  { action: 'interrupt', get label() { return translate('df9d193'); }, icon: 'pause' },
  { action: 'exit', get label() { return translate('d498e1d'); }, icon: 'logOut' },
  { action: 'archive', get label() { return translate('d5292ab'); }, icon: 'archive', danger: true },
];

/** 退出 / 归档交给 daemon 之后,在会话从列表里消失前(至多这么久)不再给动作,同一个会话不会被投第二次。 */
const HANDED_OFF_MS = 120_000;
const handedOff = new Map<string, number>();

type SessionKeyed = Pick<SessionRow, 'agent' | 'tag'>;

export function sessionKey(row: SessionKeyed): string {
  return `${row.agent}/${row.tag}`;
}

/** 退出 / 归档已经交给 daemon(做完了或还在收尾):记下这一行。 */
export function noteHandedOff(row: SessionKeyed, action: SessionAction, outcome: ActOutcome, now = Date.now()): void {
  if ((action === 'exit' || action === 'archive') && (outcome === 'done' || outcome === 'pending')) {
    handedOff.set(sessionKey(row), now + HANDED_OFF_MS);
  }
}

export function isHandedOff(row: SessionKeyed, now = Date.now()): boolean {
  const until = handedOff.get(sessionKey(row));
  if (until === undefined) return false;
  if (until > now) return true;
  handedOff.delete(sessionKey(row));
  return false;
}

/** 列表刷新之后:已经不在列表里的会话不再记着。 */
export function forgetGoneSessions(rows: SessionKeyed[]): void {
  const live = new Set(rows.map(sessionKey));
  for (const key of [...handedOff.keys()]) if (!live.has(key)) handedOff.delete(key);
}

/** 结果说成人话。 */
export function outcomeText(action: SessionAction, outcome: ActOutcome): { text: string; kind: 'ok' | 'info' | 'warn' } {
  switch (outcome) {
    case 'done':
      return {
        handled: { text: translate('dd96a41'), kind: 'ok' as const },
        interrupt: { text: translate('d1f6d57'), kind: 'ok' as const },
        exit: { text: translate('de56609'), kind: 'ok' as const },
        archive: { text: translate('d110c3e'), kind: 'ok' as const },
      }[action];
    case 'pending':
      return { text: translate('d878370'), kind: 'info' };
    case 'absent':
      return action === 'handled'
        ? { text: translate('d0d83ea'), kind: 'info' }
        : { text: translate('d494cf1'), kind: 'info' };
    case 'stale':
      return { text: translate('db7aed6'), kind: 'info' };
    case 'not_running':
      return { text: translate('daae017'), kind: 'info' };
    case 'superseded':
      return { text: translate('d0aaeeb'), kind: 'warn' };
    case 'running':
      return { text: translate('de5e510'), kind: 'warn' };
  }
}

function where(row: SessionRow): string {
  return translate('dba1bf2', [row.agent_display, row.name || row.chat_id || row.tag]);
}

/** 退出 / 归档会打断在跑的一轮:先问。 */
async function confirmInterrupting(row: SessionRow, action: 'exit' | 'archive'): Promise<boolean> {
  return confirmDialog({
    title: action === 'exit' ? translate('dd1a037') : translate('dd78d0a'),
    body:
      action === 'exit'
        ? translate('d765745', [where(row)])
        : translate('d433dab', [where(row)]),
    confirm: action === 'exit' ? translate('d1db2de') : translate('d08c5c8'),
    danger: true,
  });
}

/** 做一个动作,返回最终结果;人取消了为 `null`。 */
export async function runSessionAction(call: Caller, row: SessionRow, action: SessionAction): Promise<ActOutcome | null> {
  if (!row.chat_id) return null;
  const base = { agent: row.agent, chat_id: row.chat_id, action };
  try {
    if (action === 'handled') return (await call('sessions.act', { ...base, reply: row.waiting_reply ?? '' })).outcome;
    if (action === 'interrupt') return (await call('sessions.act', { ...base, turn_id: row.turn_id ?? '' })).outcome;
    let interrupt = false;
    if (row.busy) {
      if (!(await confirmInterrupting(row, action))) return null;
      interrupt = true;
    }
    let outcome = (await call('sessions.act', { ...base, interrupt })).outcome;
    // 页面拿到的状态晚了一拍:动手那一刻它开始跑了,再问一次。
    if (outcome === 'running') {
      if (!(await confirmInterrupting(row, action))) return null;
      outcome = (await call('sessions.act', { ...base, interrupt: true })).outcome;
    }
    noteHandedOff(row, action, outcome);
    return outcome;
  } catch (err) {
    toast(translate('d5bc4c4', [errorMessage(err)]), 'error');
    return null;
  }
}

/** 这一行能做的动作按钮。`acting` 是正在做的那个动作(转圈、其余按钮先灰掉)。 */
export function sessionActionButtons(
  row: SessionRow,
  acting: SessionAction | undefined,
  onAct: (action: SessionAction) => void,
): TemplateResult | typeof nothing {
  const shown = ACTIONS.filter((a) => row.actions[a.action]);
  if (!shown.length) return nothing;
  if (isHandedOff(row)) return html`<div class="sess-actions"><span class="chip chip-quiet">${rich('dde464b')}</span></div>`;
  return html`<div class="sess-actions" role="group" aria-label="${translate('de6f6d8')}">
    ${shown.map(
      (a) => html`<button
        type="button"
        class="btn btn-sm btn-ghost ${a.danger ? 'sess-danger' : ''}"
        title=${a.label}
        aria-label=${a.label}
        ?disabled=${acting !== undefined}
        @click=${(e: Event) => {
          e.preventDefault();
          e.stopPropagation();
          onAct(a.action);
        }}
      >
        ${acting === a.action ? icon('loaderCircle', 'icon-sm fs-spin') : icon(a.icon, 'icon-sm')}<span class="sess-action-label"
          >${a.label}</span
        >
      </button>`,
    )}
  </div>`;
}
