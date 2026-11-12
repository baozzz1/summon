import { t as translate } from '../i18n';
// 台账里一轮的通用说法:来源、结果、耗时,以及定时任务执行记录里的那种一行摘要。
import { html, nothing, type TemplateResult } from 'lit';
import type { TurnRecord } from '../api/types';
import { hrefFor } from '../app/router';
import { fmtDateTime, fmtDuration } from '../util/format';

export const ORIGIN_LABEL: Record<TurnRecord['origin'], string> = {
  get human() { return translate('da7dfea'); },
  get bot() { return translate('d573f52'); },
  get schedule() { return translate('d3c39e9'); },
  sentinel: 'sentinel',
  get slash() { return translate('d6679f6'); },
  get internal() { return translate('d6e71c7'); },
  get recovery() { return translate('d342a95'); },
};

export const END_LABEL: Record<TurnRecord['end'], { text: string; tone: string }> = {
  done: { get text() { return translate('dc0b3fb'); }, tone: 'ok' },
  empty: { get text() { return translate('df6a763'); }, tone: '' },
  silent_reset: { get text() { return translate('d89fa3a'); }, tone: '' },
  failed: { get text() { return translate('d28384d'); }, tone: 'danger' },
  interrupted: { get text() { return translate('d88cf2d'); }, tone: 'warn' },
};

/** 一轮用了多久(秒)。 */
export function turnSeconds(t: TurnRecord): number {
  return Math.max(0, Math.round((t.ended_at_ms - t.started_at_ms) / 1000));
}

/** 时长说成人话。0 秒单独写:fmtDuration 是给开关类时长用的,会把 0 写成「关」。 */
export function fmtSpan(secs: number): string {
  return secs > 0 ? fmtDuration(secs) : translate('d33a1e1');
}

export function endChip(t: TurnRecord): TemplateResult {
  const end = END_LABEL[t.end];
  return html`<span class="chip ${end.tone ? `chip-${end.tone}` : 'chip-quiet'}">${end.text}${t.error ? ` · ${t.error}` : ''}</span>`;
}

/** 一行摘要:什么时候、什么结果、多久、feed;时间链到那个会话的详情。 */
export function turnBrief(t: TurnRecord): TemplateResult {
  return html`<div class="run-row">
    <a class="tnum" href=${hrefFor({ page: 'session', agent: t.agent, chatId: t.chat_id })}>${fmtDateTime(new Date(t.ended_at_ms).toISOString())}</a>
    ${endChip(t)}
    <span class="muted tnum">${fmtSpan(turnSeconds(t))}</span>
    ${t.feed ? html`<span class="run-feed">${t.feed}</span>` : nothing}
  </div>`;
}
