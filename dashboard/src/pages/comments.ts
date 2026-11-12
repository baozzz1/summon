import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type { AgentSummary, CommentTask, FieldDesc } from '../api/types';
import { ConfigDraft } from '../app/editable';
import { hrefFor, navigate } from '../app/router';
import { store } from '../app/store';
import { commentTaskRow } from '../components/comment-task';
import { selectCtl } from '../components/controls';
import { fieldsIn, groupSection } from '../components/field-row';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { toast } from '../components/toast';

export const COMMENT_GROUP = 'document_comments';

export class CommentsPage extends PageBase {
  private agent: string | null = null;
  private agents: AgentSummary[] = [];
  private fields = new Map<string, FieldDesc>();
  private config!: ConfigDraft;
  private tasks: CommentTask[] = [];
  private taskError = '';
  private scopeLoad = 0;
  private stopping = new Set<string>();
  private timer: ReturnType<typeof setInterval> | undefined;

  protected override languageChanged(): void {
    this.fields = new Map((store.state.schema?.fields ?? []).map((field) => [field.key, field]));
    super.languageChanged();
  }

  override connectedCallback(): void {
    super.connectedCallback();
    for (const field of store.state.schema?.fields ?? []) this.fields.set(field.key, field);
    this.timer = setInterval(() => {
      if (document.visibilityState === 'visible') void this.refreshTasks();
    }, 5000);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    clearInterval(this.timer);
  }

  override willUpdate(): void {
    if (this.route?.page !== 'comments') return;
    const agent = this.route.agent ?? '';
    if (agent === this.agent) return;
    this.agent = agent;
    const scope = agent ? { level: 'agent' as const, agent } : { level: 'global' as const };
    this.config = new ConfigDraft(scope, () => agent ? translate('d12e37e', [agent]) : translate('de40081'), () => this.fields, this.call, () => this.requestUpdate());
    this.edits.replace([this.config]);
    void this.refresh();
  }

  protected override watchKeys(): string[] { return this.agent ? ['global'] : []; }

  async refresh(): Promise<void> {
    const config = this.config;
    if (!config) return;
    const sequence = ++this.scopeLoad;
    try {
      const agents = await this.call('agents.list', {});
      if (sequence !== this.scopeLoad) return;
      this.agents = agents;
      if (!store.state.whoami?.machine && !this.agent && this.agents[0]) {
        await navigate(hrefFor({ page: 'comments', agent: this.agents[0].alias }), { replace: true });
        return;
      }
      await config.load();
      this.loadError = '';
    } catch (err) { this.loadError = errorMessage(err); }
    await this.refreshTasks();
    this.loading = false;
    this.requestUpdate();
  }

  private async refreshTasks(): Promise<void> {
    try {
      const view = await this.call('sessions.list', {});
      this.tasks = view.comment_tasks;
      this.taskError = view.comment_tasks_error ?? '';
    } catch (err) { this.taskError = errorMessage(err); }
    this.requestUpdate();
  }

  private async stopTask(task: CommentTask): Promise<void> {
    if (this.stopping.has(task.id)) return;
    this.stopping.add(task.id);
    this.requestUpdate();
    try {
      await this.call('comments.stop', { agent: task.agent, id: task.id });
      toast(translate('d97193c'), 'ok');
    } catch (err) { toast(errorMessage(err), 'error'); }
    await this.refreshTasks();
    this.stopping.delete(task.id);
    this.requestUpdate();
  }

  protected override render(): TemplateResult {
    const schema = store.state.schema;
    const group = schema?.groups.find((g) => g.id === COMMENT_GROUP);
    const config = this.config;
    const tasks = this.tasks.filter((t) => !this.agent || t.agent === this.agent);
    const picker = selectCtl({ label: translate('dd77674'), value: this.agent ?? '',
      options: [...(store.state.whoami?.machine ? [{ value: '', label: translate('d419bf8') }] : []), ...this.agents.map((a) => ({ value: a.alias, label: a.display || a.alias }))],
      onChange: async (agent) => {
        await navigate(hrefFor(agent ? { page: 'comments', agent } : { page: 'comments' }));
        this.requestUpdate();
      } });
    return html`<div class="page">
      ${pageHead(translate('d2549c4'), translate('d3ec43c'), picker)}
      ${this.loadError ? errorBanner(translate('db91496'), this.loadError, () => this.refresh()) : nothing}
      ${this.conflictBanner()}
      ${config?.loadError ? errorBanner(translate('d2251b2'), config.loadError, () => this.refresh()) :
        config?.view && group && schema ? groupSection(group, fieldsIn(schema, COMMENT_GROUP), { draft: config, level: this.agent ? 'agent' : 'global', call: this.call }) : skeletonCard(2)}
      <p class="muted">${rich('d4eec61')}</p>
      ${this.saveBar()}
      <section class="group" aria-label="${translate('dcc5c3f')}"><header class="group-head"><h2>${rich('d793cce')}</h2><p>${rich('df54a06')}</p></header>
        ${this.taskError ? errorBanner(translate('dd00d24'), this.taskError, () => this.refreshTasks()) : nothing}
        <div class="card">${tasks.length ? tasks.map((t) => commentTaskRow(t, () => void this.stopTask(t), this.stopping.has(t.id))) : emptyState(translate('df08367'), translate('d5de1a3'))}</div>
      </section>
    </div>`;
  }
}
customElements.define('sm-page-comments', CommentsPage);
