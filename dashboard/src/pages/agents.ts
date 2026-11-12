import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type { AgentDetail, AgentSummary } from '../api/types';
import { hrefFor } from '../app/router';
import { store } from '../app/store';
import { switchCtl } from '../components/controls';
import { confirmDialog } from '../components/dialog';
import { toast } from '../components/toast';
import { icon } from '../components/icon';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { agentLine, agentState } from '../util/labels';

export class AgentsPage extends PageBase {
  agents: AgentSummary[] = [];
  private permissions = new Map<string, Pick<AgentDetail, 'can_toggle_enabled' | 'deny_reason'>>();
  private toggling = new Set<string>();
  private requestId = 0;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  protected override watchKeys(): string[] {
    return ['agent:*'];
  }

  async refresh(): Promise<void> {
    const requestId = ++this.requestId;
    try {
      const agents = await this.call('agents.list', {});
      const permissions = await Promise.allSettled(agents.map((a) => this.call('agents.get', { agent: a.alias })));
      if (requestId !== this.requestId) return;
      this.agents = agents;
      this.permissions = new Map(permissions.map((p, i) => [agents[i]!.alias, p.status === 'fulfilled'
        ? p.value : { can_toggle_enabled: false, deny_reason: translate('de27e94') }]));
      this.loadError = '';
    } catch (err) {
      if (requestId !== this.requestId) return;
      this.loadError = errorMessage(err);
    }
    this.loading = false;
    this.requestUpdate();
  }

  private async toggleEnabled(a: AgentSummary, enabled: boolean): Promise<void> {
    if (!this.permissions.get(a.alias)?.can_toggle_enabled || a.load_error || this.toggling.has(a.alias)) return;
    this.toggling.add(a.alias);
    this.requestUpdate();
    try {
      if (!enabled && !await confirmDialog({
        title: translate('d4fd748', [a.display || a.alias]),
        body: translate('d1cae81'),
        confirm: translate('d4e6fd0'), danger: true,
      })) return;
      const summary = await this.call('agents.set_enabled', { agent: a.alias, enabled });
      ++this.requestId;
      this.agents = this.agents.map((agent) => agent.alias === a.alias ? summary : agent);
      toast(enabled ? translate('dc17bd5', [summary.display || a.alias]) : translate('d1e8856', [summary.display || a.alias]), 'ok');
      await store.refreshStatus(true);
    } catch (err) {
      toast(translate('d860480', [errorMessage(err)]), 'error');
    } finally {
      this.toggling.delete(a.alias);
      this.requestUpdate();
    }
  }

  private card(a: AgentSummary, restartFiles: string[]): TemplateResult {
    const s = agentState(a, restartFiles);
    const href = hrefFor({ page: 'agent', alias: a.alias, tab: a.load_error ? 'yaml' : 'overview' });
    return html`<article class="card agent-card">
      <a class="agent-card-link" href=${href} aria-label=${translate('dac11c0', [a.display || a.alias])}>
      <div class="agent-card-top">
        <span class="avatar avatar-lg" aria-hidden="true">${[...(a.display || a.alias)][0] ?? '?'}</span>
        <div class="list-main">
          <div class="list-title"><span>${a.display || a.alias}</span></div>
          <div class="list-sub mono">${a.alias}</div>
        </div>
        <span class="chip ${s.tone ? `chip-${s.tone}` : 'chip-quiet'}">${s.text}</span>
      </div>
      ${a.load_error
        ? html`<div class="agent-card-error">
            <p class="mono">${a.load_error}</p>
            <span class="link-btn">${rich('da5753c', [icon('chevronRight', 'icon-sm')])}</span>
          </div>`
        : html`<dl class="agent-card-facts">
            <div><dt>${rich('d8f6cbd')}</dt><dd>${agentLine(a) || '—'}</dd></div>
            <div><dt>${rich('dd334c4')}</dt><dd>${a.enabled ? translate('ddfb802') : translate('da8c369')}</dd></div>
            <div><dt>${rich('d2dfe17')}</dt><dd>${a.credentials === 'ready' ? translate('dab27f8') : html`<span class="danger-text">${rich('d65364e')}</span>`}</dd></div>
            <div><dt>${rich('d35b49e')}</dt><dd class="tnum">${rich('d93c883', [a.chats])}</dd></div>
          </dl>`}
      </a>
      <div class="agent-card-toggle" title=${a.load_error ? translate('dee5fa6') : this.permissions.get(a.alias)?.deny_reason ?? ''}>
        <span>${this.toggling.has(a.alias) ? translate('dd3d211') : a.enabled ? translate('ddfb802') : translate('da8c369')}</span>
        ${switchCtl({ checked: a.enabled, label: translate('d2687cb', [a.display || a.alias]),
          disabled: !!a.load_error || !this.permissions.get(a.alias)?.can_toggle_enabled || this.toggling.has(a.alias),
          onChange: (on) => void this.toggleEnabled(a, on) })}
      </div>
    </article>`;
  }

  protected override render(): TemplateResult {
    const files = store.state.status?.restart_pending.map((r) => r.file) ?? [];
    return html`<div class="page">
      ${pageHead('Agents', translate('d421d7e'))}
      ${this.loadError ? errorBanner(translate('de1816e'), this.loadError, () => this.refresh()) : nothing}
      ${this.loading
        ? skeletonCard(3)
        : this.agents.length
          ? html`<div class="agent-grid">${this.agents.map((a) => this.card(a, files))}</div>`
          : html`<div class="card">${emptyState(translate('d7acc5d'), translate('dfdcc82'))}</div>`}
    </div>`;
  }
}

customElements.define('sm-page-agents', AgentsPage);
