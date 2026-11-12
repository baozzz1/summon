import { AttentionState, attentionPanel } from '../components/attention';
import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type { CommentTask, SessionAction, SessionRow, SessionState } from '../api/types';
import { hrefFor } from '../app/router';
import { selectCtl } from '../components/controls';
import { ident } from '../components/ident';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { forgetGoneSessions, outcomeText, runSessionAction, sessionActionButtons, sessionKey } from '../components/session-actions';
import { toast } from '../components/toast';
import { fmtElapsed, fmtRelative } from '../util/format';

/** 页面可见时每隔这么久刷新一次。 */
const POLL_MS = 5000;

export const STATE_ORDER: SessionState[] = ['leaked', 'blocked', 'waiting', 'working', 'idle'];

export const STATE_LABEL: Record<SessionState, string> = {
  get leaked() { return translate('d456e98'); },
  get blocked() { return translate('d7045c6'); },
  get waiting() { return translate('de54bd4'); },
  get working() { return translate('d1f0eb9'); },
  get idle() { return translate('ddae661'); },
};

const STATE_HINT: Record<SessionState, string> = {
  get leaked() { return translate('db3cf7a'); },
  get blocked() { return translate('d55ed8a'); },
  get waiting() { return translate('dce48ec'); },
  get working() { return translate('ddabf21'); },
  idle: '',
};

const STATE_TONE: Record<SessionState, string> = {
  leaked: 'danger',
  blocked: 'warn',
  waiting: 'accent',
  working: 'ok',
  idle: '',
};

const BLOCKED_LABEL: Record<string, string> = {
  get permission() { return translate('db5306c'); },
  get question() { return translate('d4d1895'); },
  get auth() { return translate('dd7f81b'); },
};

/** 这一行状态旁的那句话:在跑多久、等了多久、最近活跃。 */
export function sessionMeta(r: SessionRow, now = new Date()): string {
  switch (r.state) {
    case 'blocked':
      return BLOCKED_LABEL[r.blocked ?? ''] ?? translate('d7045c6');
    case 'working':
      return r.turn_started_at ? translate('defc4cf', [fmtElapsed(r.turn_started_at, now)]) : translate('d144faa');
    case 'waiting':
      return r.waiting_since ? translate('d64df6b', [fmtElapsed(r.waiting_since, now)]) : translate('de54bd4');
    case 'leaked':
      return translate('df8ceb2');
    case 'idle':
      return r.last_active ? translate('dbfa5de', [fmtRelative(r.last_active, now)]) : translate('ddae661');
  }
}

/** 按状态分组,组内保持服务端给的顺序。 */
export function groupSessions(rows: SessionRow[]): Array<{ state: SessionState; rows: SessionRow[] }> {
  return STATE_ORDER.map((state) => ({ state, rows: rows.filter((r) => r.state === state) })).filter((g) => g.rows.length);
}

export function sessionTitle(r: Pick<SessionRow, 'name' | 'chat_id' | 'tag' | 'chat_type' | 'agent'>): TemplateResult | string {
  if (r.name) return r.name;
  if (r.chat_id) return ident(r.chat_id, { agent: r.agent });
  return r.tag;
}


class SessionsPage extends PageBase {
  readonly attention = new AttentionState();
  rows: SessionRow[] = [];
  commentTasks: CommentTask[] = [];
  commentTasksError = '';
  agent = '';
  acting = new Map<string, SessionAction>();
  private timer: ReturnType<typeof setInterval> | undefined;

  private readonly onVisibility = () => {
    if (document.visibilityState === 'visible') void this.refresh();
  };

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
    this.timer = setInterval(() => {
      if (document.visibilityState === 'visible') void this.refresh();
    }, POLL_MS);
    document.addEventListener('visibilitychange', this.onVisibility);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    clearInterval(this.timer);
    document.removeEventListener('visibilitychange', this.onVisibility);
  }

  async refresh(): Promise<void> {
    const attention = this.attention.refresh(() => this.call('attention.list', {}));
    try {
      const view = await this.call('sessions.list', {});
      this.rows = view.sessions;
      this.commentTasks = view.comment_tasks ?? [];
      this.commentTasksError = view.comment_tasks_error ?? '';
      forgetGoneSessions(this.rows);
      this.loadError = '';
    } catch (err) {
      this.loadError = errorMessage(err);
    }
    await attention;
    this.loading = false;
    this.requestUpdate();
  }

  private async act(r: SessionRow, action: SessionAction) {
    const key = sessionKey(r);
    this.acting.set(key, action);
    this.requestUpdate();
    const outcome = await runSessionAction(this.call, r, action);
    if (outcome) {
      const { text, kind } = outcomeText(action, outcome);
      toast(text, kind);
    }
    await this.refresh();
    this.acting.delete(key);
    this.requestUpdate();
  }

  private row(r: SessionRow): TemplateResult {
    const title = sessionTitle(r);
    const main = html`<span class="dot ${STATE_TONE[r.state]}" aria-hidden="true"></span>
      <span class="sess-name">
        <strong>${title}</strong>
        <span class="agent-tag">${r.agent_display}</span>
        ${r.chat_type === 'p2p' ? html`<span class="chip chip-quiet">${rich('d6add27')}</span>` : nothing}
      </span>
      <span class="sess-meta">${sessionMeta(r)}</span>`;
    return html`<div class="sess-row" data-state=${r.state}>
      ${r.chat_id
        ? html`<a class="sess-main" href=${hrefFor({ page: 'session', agent: r.agent, chatId: r.chat_id })}>${main}</a>`
        : html`<div class="sess-main">${main}</div>`}
      ${sessionActionButtons(r, this.acting.get(sessionKey(r)), (a) => void this.act(r, a))}
    </div>`;
  }

  private filters(): TemplateResult {
    const agents = [...new Map([...this.rows.map((r) => [r.agent, r.agent_display] as const), ...this.commentTasks.map((r) => [r.agent, r.agent] as const)]).entries()];
    return html`<div class="toolbar">
      ${selectCtl({
        label: translate('dbbe7b2'),
        value: this.agent,
        options: [{ value: '', label: translate('d198f2f') }, ...agents.map(([value, label]) => ({ value, label }))],
        onChange: (v) => {
          this.agent = v ?? '';
          this.requestUpdate();
        },
      })}
      <span class="sess-counts">
        ${groupSessions(this.rows).map(
          (g) => html`<span class="sess-count"><span class="dot ${STATE_TONE[g.state]}"></span>${STATE_LABEL[g.state]} ${g.rows.length}</span>`,
        )}
      </span>
    </div>`;
  }

  protected override render(): TemplateResult {
    const rows = this.agent ? this.rows.filter((r) => r.agent === this.agent) : this.rows;
    return html`<div class="page">
      ${pageHead(translate('da63280'), translate('d4d760f'))}
      ${attentionPanel(this.attention, this.agent)}
      ${this.loadError ? errorBanner(translate('d064365'), this.loadError, () => this.refresh()) : nothing}
      ${this.commentTasksError ? errorBanner(translate('d3a68cc'), this.commentTasksError, () => this.refresh()) : nothing}
      ${this.commentTasks.length ? html`<p><a href=${hrefFor({ page: 'comments' })}>${rich('d55b263', [this.commentTasks.length])}</a></p>` : nothing}
      ${this.loading
        ? skeletonCard(5)
        : !this.rows.length
          ? html`<div class="card">${emptyState(translate('d69e71f'), translate('d882e8c'))}</div>`
          : html`${this.filters()}
              ${groupSessions(rows).map(
                (g) => html`<section class="group" aria-labelledby=${`sg-${g.state}`}>
                  <header class="group-head">
                    <div>
                      <h2 id=${`sg-${g.state}`}>${STATE_LABEL[g.state]} <span class="muted">${g.rows.length}</span></h2>
                      ${STATE_HINT[g.state] ? html`<p>${STATE_HINT[g.state]}</p>` : nothing}
                    </div>
                  </header>
                  <div class="card">${g.rows.map((r) => this.row(r))}</div>
                </section>`,
              )}`}
    </div>`;
  }
}

customElements.define('sm-page-sessions', SessionsPage);
