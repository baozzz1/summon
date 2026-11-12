import { AttentionState, attentionPanel } from '../components/attention';
import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type { Issue, IssueLink, SessionRow, SessionState } from '../api/types';
import { hrefFor } from '../app/router';
import { store } from '../app/store';
import { icon } from '../components/icon';
import { inlineCode } from '../components/inline';
import { errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { fmtDateTime, fmtUptime } from '../util/format';
import { agentLine, agentState, fileLabel } from '../util/labels';

export function issueHref(link: IssueLink): string {
  switch (link.page) {
    case 'global':
      return hrefFor(link.key ? { page: 'global', mode: 'form', key: link.key } : { page: 'global', mode: 'form' });
    case 'agent':
      return link.agent ? hrefFor({ page: 'agent', alias: link.agent, tab: 'yaml' }) : '/agents';
    case 'chat':
      return link.chat_id ? hrefFor(link.agent ? { page: 'chat', chatId: link.chat_id, agent: link.agent } : { page: 'chat', chatId: link.chat_id }) : '/chats';
    case 'schedules':
      return '/schedules';
    case 'session':
      return link.agent && link.chat_id ? hrefFor({ page: 'session', agent: link.agent, chatId: link.chat_id }) : '/sessions';
  }
}

const running = (): Array<[SessionState, string]> => [
  ['working', translate('d1f0eb9')],
  ['waiting', translate('de54bd4')],
  ['blocked', translate('d7045c6')],
  ['leaked', translate('d456e98')],
];

class OverviewPage extends PageBase {
  readonly attention = new AttentionState();
  private timer: ReturnType<typeof setInterval> | undefined;
  private readonly onVisibility = () => {
    if (document.visibilityState === 'visible') void this.refresh();
  };
  issues: Issue[] = [];
  /** 取不到会话状态(daemon 端口不在)时不显示「运行」这一栏 */
  sessions: SessionRow[] | null = null;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
    this.timer = setInterval(() => {
      if (document.visibilityState === 'visible') void this.refresh();
    }, 5000);
    document.addEventListener('visibilitychange', this.onVisibility);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    clearInterval(this.timer);
    document.removeEventListener('visibilitychange', this.onVisibility);
  }

  protected override watchKeys(): string[] {
    return ['global', 'agent:*'];
  }

  async refresh(): Promise<void> {
    const attention = this.attention.refresh(() => this.call('attention.list', {}));
    const sessions = this.call('sessions.list', {}).then(
      (v) => v.sessions,
      () => null,
    );
    try {
      const [issues] = await Promise.all([this.call('status.issues', {}), store.refreshStatus(true)]);
      this.issues = issues;
      this.loadError = '';
    } catch (err) {
      this.loadError = errorMessage(err);
    }
    this.sessions = await sessions;
    await attention;
    this.loading = false;
    this.requestUpdate();
  }

  private daemonCard(): TemplateResult {
    const st = store.state.status!;
    const d = st.daemon;
    const auto = d.autostart === 'on' ? translate('d8a4ef3') : d.autostart === 'off' ? translate('d3cffa9') : translate('d4d8c1c');
    return html`<section class="page-section" aria-labelledby="ov-daemon">
      <div class="group-head"><h2 id="ov-daemon">daemon</h2></div>
      <div class="card">
        <div class="stats">
          <div class="stat">
            <div class="stat-label">${rich('d5f76b2')}</div>
            <div class="stat-value">v${d.version} <span class="mono">${d.commit}</span></div>
          </div>
          <div class="stat">
            <div class="stat-label">${rich('db1e3a9')}</div>
            <div class="stat-value" title=${translate('d7f8c46', [fmtDateTime(d.started_at)])}>${fmtUptime(d.started_at)}</div>
          </div>
          <div class="stat">
            <div class="stat-label">${rich('dde38b9')}</div>
            <div class="stat-value">${auto}</div>
          </div>
          <div class="stat">
            <div class="stat-label">${rich('d172099')}</div>
            <div class="stat-value">${st.hostname} <span class="mono">pid ${d.pid}</span></div>
          </div>
        </div>
        ${st.restart_pending.length
          ? html`<div class="issue issue-warn">
              ${icon('refreshCw')}
              <div class="issue-body">
                <div class="issue-title">${rich('d9b0d94', [st.restart_pending.length])}</div>
                <div class="issue-detail">
                  ${st.restart_pending.map((r, i) => html`${i ? '、' : ''}<code>${r.key}</code>（${fileLabel(r.file)}）`)}
                </div>
                <div class="issue-fix"><span>${rich('dfb1d16')} <code>summon daemon restart</code>${rich('d00c0e0')}</span></div>
              </div>
            </div>`
          : nothing}
      </div>
    </section>`;
  }

  private runningCard(): TemplateResult | typeof nothing {
    const rows = this.sessions;
    if (!rows) return nothing;
    return html`<section class="page-section" aria-labelledby="ov-running">
      <div class="group-head">
        <h2 id="ov-running">${rich('de46b02')}</h2>
        <a class="link-btn quiet" href=${hrefFor({ page: 'usage' })}>${rich('d4cd877', [icon('chevronRight', 'icon-sm')])}</a>
      </div>
      <div class="card">
        <div class="stats">
          ${running().map(
            ([state, label]) => html`<a class="stat stat-link" href=${hrefFor({ page: 'sessions' })}>
              <div class="stat-label">${label}</div>
              <div class="stat-value">${rows.filter((r) => r.state === state).length}</div>
            </a>`,
          )}
        </div>
      </div>
    </section>`;
  }

  private issuesCard(): TemplateResult {
    const list = this.issues;
    return html`<section class="page-section" aria-labelledby="ov-issues">
      <div class="group-head">
        <div>
          <h2 id="ov-issues">${rich('db7e3e7')}</h2>
          <p>${rich('dab1152')}</p>
        </div>
      </div>
      ${this.loading
        ? skeletonCard(2)
        : list.length
          ? html`<div class="card">
              ${list.map(
                (it) => html`<div class="issue issue-${it.severity}">
                  ${icon(it.severity === 'error' ? 'circleAlert' : 'triangleAlert')}
                  <div class="issue-body">
                    <div class="issue-title">${inlineCode(it.title)}</div>
                    <div class="issue-detail">${inlineCode(it.detail)}</div>
                    <div class="issue-fix">
                      <span>${inlineCode(it.fix)}</span>
                      ${it.link
                        ? html`<a class="link-btn" href=${issueHref(it.link)}>${it.link.page === 'session' ? translate('da748cc') : translate('d90e01a')}${icon('chevronRight', 'icon-sm')}</a>`
                        : nothing}
                    </div>
                  </div>
                </div>`,
              )}
            </div>`
          : html`<div class="card">
              <div class="empty"><h3>${rich('d4f5245')}</h3><p>${rich('d1d32ac')}</p></div>
            </div>`}
    </section>`;
  }

  private agentsCard(): TemplateResult {
    const st = store.state.status!;
    const files = st.restart_pending.map((r) => r.file);
    return html`<section class="page-section" aria-labelledby="ov-agents">
      <div class="group-head">
        <h2 id="ov-agents">Agents</h2>
        <a class="link-btn quiet" href="/agents">${rich('df293a2', [icon('chevronRight', 'icon-sm')])}</a>
      </div>
      <div class="card">
        ${st.agents.map((a) => {
          const s = agentState(a, files);
          return html`<a class="list-row" href=${hrefFor({ page: 'agent', alias: a.alias, tab: a.load_error ? 'yaml' : 'overview' })}>
            <span class="avatar" aria-hidden="true">${[...(a.display || a.alias)][0] ?? '?'}</span>
            <div class="list-main">
              <div class="list-title"><span>${a.display || a.alias}</span><span class="mono muted">${a.alias}</span></div>
              <div class="list-sub">${agentLine(a)}</div>
            </div>
            <div class="list-aside">
              <span class="chip ${s.tone ? `chip-${s.tone}` : 'chip-quiet'}">${s.text}</span>
              ${icon('chevronRight', 'icon-sm')}
            </div>
          </a>`;
        })}
      </div>
    </section>`;
  }

  protected override render(): TemplateResult {
    return html`<div class="page">
      ${pageHead(translate('dfea405'), translate('d86c2ba'))}
      ${this.loadError ? errorBanner(translate('dd7a880'), this.loadError, () => this.refresh()) : nothing}
      ${this.daemonCard()} ${attentionPanel(this.attention, '', true)} ${this.runningCard()} ${this.issuesCard()} ${this.agentsCard()}
    </div>`;
  }
}

customElements.define('sm-page-overview', OverviewPage);
