import { html, nothing } from 'lit';
import { t } from '../i18n';
import type { AttentionView } from '../api/types';
import { errorMessage } from '../api/client';
import { fmtDateTime } from '../util/format';

export class AttentionState {
  data: AttentionView | null = null;
  error = '';
  private pending: Promise<void> | null = null;
  refresh(load: () => Promise<AttentionView>): Promise<void> {
    if (this.pending) return this.pending;
    this.pending = load().then((value) => { this.data = value; this.error = ''; },
      (error) => { this.error = errorMessage(error); }).finally(() => { this.pending = null; });
    return this.pending;
  }
}

export function attentionLink(link: string): string | null {
  return /^lark:\/\/(applink\.feishu\.cn|applink\.larksuite\.com)\/client\/chat\/open\?openChatId=[A-Za-z0-9_%.-]+(?:&position=[0-9]+)?$/.test(link) ? link : null;
}

export function attentionPanel(state: AttentionState, agent = '', compact = false) {
  const data = state.data;
  const rows = (data?.items ?? []).filter((r) => !agent || r.agent === agent);
  return html`<section class="page-section" aria-labelledby="attention-title">
    <div class="group-head"><h2 id="attention-title">${t('d485504')} ${data ? rows.length : ''}</h2></div>
    <p class="muted">${t('d06ab6f')}</p>
    ${state.error ? html`<p role="alert">${t('d4e22d1')}: ${state.error}</p>` : nothing}
    ${data && state.error ? html`<p role="status">${t('d694821', [fmtDateTime(data.observed_at)])}</p>` : nothing}
    ${data?.partial ? html`<p role="status">${t('da616f2')}</p>` : nothing}
    ${!data ? html`<p>${state.error ? t('d4e22d1') : t('d2b13f0')}</p>`
      : compact ? html`<a class="link-btn" href="/sessions">${t('d04d1e1')}</a>`
      : !rows.length ? html`<p>${state.error || data.partial ? t('d4e22d1') : t('d341591')}</p>`
      : html`<div class="card">${rows.map((r) => {
          const link = !state.error && attentionLink(r.link);
          const kind = r.source === 'claude_permission' ? t('d405539') : r.source === 'codex_approval' ? t('d685a43') : t('de0ccb9');
          return html`<div class="list-row"><div class="list-main">
            <div class="list-title">${r.agent_display || r.agent} · ${r.chat_name}</div>
            <div class="list-sub">${kind}${r.summary ? html` · ${r.summary}` : nothing}</div>
            <div class="list-sub">${r.question_count ? t('d372384', [r.waited_secs, r.question_count]) : t('d66e285', [r.waited_secs])}</div>
            ${link ? html`<a class="link-btn" href=${link} rel="noopener noreferrer">${r.link_target === 'chat' ? t('d0a7d4a') : t('d4352b8')}</a>` : html`<span class="muted">${state.error ? t('d694821', [fmtDateTime(data.observed_at)]) : t('da5f172')}</span>`}
          </div></div>`;
        })}</div>`}
    </section>`;
}
