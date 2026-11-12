import { t as translate } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import type { CommentTask } from '../api/types';

export function commentTaskRow(t: CommentTask, stop?: () => void, stopping = false): TemplateResult {
  return html`<div class="sess-row" data-comment-stage=${t.stage}><div class="sess-main">
    <span class="sess-name"><strong>${t.title}</strong><span class="agent-tag">${t.agent}</span></span>
    <span class="sess-meta" title=${t.error ?? ''}>${t.status}${t.pid ? ` · PID ${t.pid}` : ''}${t.error ? ` · ${t.error}` : ''}</span>
  </div>${stop && ['received', 'queued', 'drafting', 'paused'].includes(t.stage)
    ? html`<button class="btn" ?disabled=${stopping} @click=${stop}>${stopping ? translate('d8a99be') : translate('d0d2425')}</button>` : nothing}</div>`;
}

