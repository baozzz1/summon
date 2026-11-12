import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type { AgentDetail, ChatSummary, FieldDesc, Section } from '../api/types';
import { ConfigDraft, DocDraft, RawDraft } from '../app/editable';
import { AGENT_TABS, hrefFor, navigate, type AgentTab, type Route } from '../app/router';
import { store } from '../app/store';
import { AgentCleanup } from '../components/agent-cleanup';
import { switchCtl } from '../components/controls';
import { confirmDialog, reviewRawDialog } from '../components/dialog';
import { docEditor, issuesList, yamlEditor } from '../components/editors';
import { fieldsIn, groupSection, groupsFor, type RowEnv } from '../components/field-row';
import { icon } from '../components/icon';
import { ident } from '../components/ident';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { toast } from '../components/toast';
import { PreviewController } from '../preview/controller';
import '../preview/panel';
import { fmtRelative } from '../util/format';
import { agentLine, agentState, chatInitial, chatTitle, hasChatName } from '../util/labels';

type AgentRoute = Extract<Route, { page: 'agent' }>;

const TAB_SECTIONS: Partial<Record<AgentTab, Section>> = {
  overview: 'identity',
  runtime: 'runtime',
  permissions: 'permissions',
  messaging: 'messaging',
};

class AgentPage extends PageBase {
  private alias = '';
  private tab: AgentTab = 'overview';
  private target: string | null = null;
  private fields = new Map<string, FieldDesc>();
  private detail: AgentDetail | null = null;
  private chats: ChatSummary[] | null = null;
  private config!: ConfigDraft;
  private claude!: DocDraft;
  private soul!: DocDraft;
  private raw!: RawDraft;
  private preview!: PreviewController;
  private toggling = false;
  private cleanup: AgentCleanup | null = null;

  private setup(alias: string) {
    this.cleanup?.dispose();
    this.cleanup = new AgentCleanup(alias, this.call, () => this.requestUpdate(), async () => {
      toast(translate('d9ee71e', [alias]), 'ok');
      await store.refreshStatus(true);
      navigate(hrefFor({ page: 'agents' }));
    });
    this.alias = alias;
    const scope = { level: 'agent' as const, agent: alias };
    this.config = new ConfigDraft(scope, () => translate('db77c1d', [alias]), () => this.fields, this.call, () => {
      this.preview.schedule();
      this.requestUpdate();
    });
    this.claude = new DocDraft({ level: 'agent', agent: alias, file: 'CLAUDE.md' }, 'CLAUDE.md', this.call, () => this.requestUpdate());
    this.soul = new DocDraft({ level: 'agent', agent: alias, file: 'SOUL.md' }, 'SOUL.md', this.call, () => this.requestUpdate());
    this.raw = new RawDraft({ file: 'agent', agent: alias }, `${alias}/agent.yaml`, this.call, reviewRawDialog, () => this.requestUpdate());
    this.preview?.dispose();
    this.preview = new PreviewController(this.call, scope, () => this.config.changeList());
    this.bindPreview(this.preview);
    this.detail = null;
    this.chats = null;
    this.loading = true;
  }

  private registerEdits() {
    this.edits.replace(this.tab === 'yaml' ? [this.raw] : [this.config, this.claude, this.soul]);
  }

  override willUpdate(): void {
    const r = this.route as AgentRoute;
    if (!r) return;
    if (r.alias !== this.alias) {
      this.setup(r.alias);
      this.tab = r.tab;
      this.registerEdits();
      void this.refresh();
    } else if (r.tab !== this.tab) {
      const wasYaml = this.tab === 'yaml';
      this.tab = r.tab;
      if (wasYaml !== (r.tab === 'yaml')) {
        this.registerEdits();
        if (r.tab === 'yaml') void this.raw.load();
        else void this.config.load();
      }
      if (r.tab === 'messaging') void this.preview.request();
    }
    if (r.key && r.key !== this.target) {
      this.target = r.key;
      this.focusTarget(r.key);
    }
  }

  protected override languageChanged(): void {
    this.fields = new Map((store.state.schema?.fields ?? []).map((field) => [field.key, field]));
    if (this.preview) void this.preview.request();
    super.languageChanged();
  }

  override connectedCallback(): void {
    super.connectedCallback();
    for (const f of store.state.schema?.fields ?? []) this.fields.set(f.key, f);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.preview?.dispose();
    this.cleanup?.dispose();
  }

  protected override watchKeys(): string[] {
    return ['chat:*'];
  }

  protected override sameContext(next: Route): boolean {
    if (next.page !== 'agent' || next.alias !== this.alias) return false;
    // 表单与原文是两套草稿,来回切换时不带过去。
    return (next.tab === 'yaml') === (this.tab === 'yaml');
  }

  async refresh(): Promise<void> {
    const alias = this.alias;
    try {
      const [detail, chats] = await Promise.all([this.call('agents.get', { agent: alias }), this.call('chats.list', {})]);
      if (alias !== this.alias) return;
      this.detail = detail;
      this.chats = chats.filter((c) => c.agents.some((a) => a.agent === alias));
      this.loadError = '';
      const loads: Array<Promise<void>> = [this.claude.load(), this.soul.load()];
      if (!detail.summary.load_error) loads.push(this.config.load());
      if (this.tab === 'yaml' || detail.summary.load_error) loads.push(this.raw.load());
      await Promise.all(loads);
      // 读不出来的 agent:打开原文就把出错的那一行标出来。
      if (detail.summary.load_error && this.raw.view) await this.raw.check();
      if (this.tab === 'messaging') void this.preview.request();
    } catch (err) {
      this.loadError = errorMessage(err);
    }
    this.loading = false;
    this.requestUpdate();
  }

  private env(): RowEnv {
    const d = this.detail;
    const env: RowEnv = { draft: this.config, level: 'agent', preview: this.tab === 'messaging' ? this.preview : null, target: this.highlight };
    if (d) {
      env.members = d.members;
      env.suggestions = { model: d.model_candidates };
      if (d.effort_options.length) env.options = { effort: d.effort_options.map((e) => ({ value: e, label: e })) };
    }
    return env;
  }

  private sectionGroups(section: Section): TemplateResult {
    const schema = store.state.schema!;
    if (this.detail?.summary.load_error) return this.brokenNotice();
    if (this.config.loadError) return errorBanner(translate('de5ed04'), this.config.loadError, () => this.refresh());
    if (!this.config.view) return skeletonCard(5);
    const env = this.env();
    return html`${groupsFor(schema, section).map((g) => groupSection(g, fieldsIn(schema, g.id), env))}`;
  }

  private brokenNotice(): TemplateResult {
    const d = this.detail!;
    return html`<div class="banner banner-error" role="alert">
      ${icon('circleAlert')}
      <div class="banner-body">
        <strong>${rich('dce82d1')}</strong>
        <p class="mono">${d.summary.load_error}</p>
        <p>${rich('d78ddf8')}</p>
        <div class="banner-actions">
          <a class="btn btn-sm" href=${hrefFor({ page: 'agent', alias: this.alias, tab: 'yaml' })}>${rich('d13fa63')}</a>
        </div>
      </div>
    </div>`;
  }

  private async toggleEnabled(on: boolean) {
    const d = this.detail;
    if (!d) return;
    if (!on) {
      const ok = await confirmDialog({
        title: translate('d4fd748', [d.summary.display || this.alias]),
        body: translate('d1cae81'),
        confirm: translate('d4e6fd0'),
        danger: true,
      });
      if (!ok) return;
    }
    this.toggling = true;
    this.requestUpdate();
    try {
      const summary = await this.call('agents.set_enabled', { agent: this.alias, enabled: on });
      this.detail = { ...d, summary };
      toast(on ? translate('dc17bd5', [summary.display || this.alias]) : translate('d1e8856', [summary.display || this.alias]), 'ok');
      await store.refreshStatus(true);
    } catch (err) {
      toast(translate('d860480', [errorMessage(err)]), 'error');
    }
    this.toggling = false;
    this.requestUpdate();
  }

  private tabOverview(): TemplateResult {
    const d = this.detail;
    if (!d) return skeletonCard(4);
    const s = d.summary;
    return html`${this.sectionGroups('identity')}
      <section class="group" aria-labelledby="gh-enable">
        <header class="group-head"><h2 id="gh-enable">${rich('df4f0ea')}</h2></header>
        <div class="card">
          <div class="row row--inline ${!d.can_toggle_enabled ? 'is-disabled' : ''}">
            <div class="row-grid">
              <div class="row-head">
                <div class="row-title"><span class="row-label">${rich('d95f570')}</span></div>
                <p class="row-desc">${rich('dfe3fc8')}</p>
              </div>
              <div class="row-ctl" title=${d.deny_reason ?? nothing}>
                ${switchCtl({
                  checked: s.enabled,
                  label: translate('d95f570'),
                  disabled: !d.can_toggle_enabled || this.toggling || this.cleanup?.busy,
                  onChange: (v) => this.toggleEnabled(v),
                })}
                ${d.deny_reason ? html`<span class="row-lock">${icon('lock', 'icon-sm')}<span>${d.deny_reason}</span></span>` : nothing}
              </div>
            </div>
          </div>
        </div>
      </section>
      ${!s.enabled && d.can_toggle_enabled ? this.cleanup?.render() : nothing}
      <section class="group" aria-labelledby="gh-feishu">
        <header class="group-head">
          <div>
            <h2 id="gh-feishu">${rich('da0def6')}</h2>
            <p>${rich('d30d119')} <code>summon bind ${this.alias}</code>。</p>
          </div>
        </header>
        <div class="card card-pad">
          <dl class="kv-list">
            <dt>${rich('db144ad')}</dt>
            <dd class="mono">${d.app_id_env}</dd>
            <dt>${rich('d469e5d')}</dt>
            <dd>${d.scope === 'thread' ? translate('d3b8362') : d.scope === 'chat' ? translate('dcc886f') : d.scope}</dd>
            <dt>${rich('d2dfe17')}</dt>
            <dd>
              ${s.credentials === 'ready'
                ? html`<span class="chip chip-ok">${rich('dab27f8')}</span>`
                : html`<span class="chip chip-danger">${rich('d65364e')}</span> <span class="muted">${rich('db56b0b')}</span>`}
            </dd>
          </dl>
        </div>
      </section>`;
  }

  private tabPermissions(): TemplateResult {
    const chats = this.chats ?? [];
    return html`${this.sectionGroups('permissions')}
      <section class="group" aria-labelledby="gh-chatops">
        <header class="group-head">
          <div>
            <h2 id="gh-chatops">${rich('de3137d')}</h2>
            <p>${rich('d49cff2', [this.detail?.summary.display || this.alias])}</p>
          </div>
        </header>
        ${chats.length
          ? html`<div class="card">${chats.filter((c) => c.kind === 'group').map((c) => this.chatLink(c, 'permissions'))}</div>`
          : html`<div class="card">${emptyState(translate('d437c79'), translate('d9b02e4'))}</div>`}
      </section>`;
  }

  private chatLink(c: ChatSummary, why: 'permissions' | 'chats'): TemplateResult {
    const mine = c.agents.find((a) => a.agent === this.alias);
    return html`<a class="list-row" href=${hrefFor({ page: 'chat', chatId: c.chat_id, agent: this.alias })}>
      <span class="avatar" aria-hidden="true">${chatInitial(c)}</span>
      <div class="list-main">
        <div class="list-title">
          <span>${hasChatName(c) || c.kind === 'p2p' ? chatTitle(c) : ident(c.chat_id, { agent: this.alias })}</span>${mine?.live ? html`<span class="chip chip-ok">${rich('d7d817d')}</span>` : nothing}
        </div>
        ${hasChatName(c) ? html`<div class="list-sub mono">${c.chat_id}</div>` : nothing}
      </div>
      <div class="list-aside">
        ${why === 'chats'
          ? html`<span class="count">${mine?.overrides ? translate('ded467e', [mine.overrides]) : translate('d27e364')}</span>
              <span class="hide-narrow">${fmtRelative(mine?.last_active)}</span>`
          : html`<span>${rich('d051836')}</span>`}
        ${icon('chevronRight', 'icon-sm')}
      </div>
    </a>`;
  }

  private tabChats(): TemplateResult {
    if (!this.chats) return skeletonCard(3);
    return html`<div class="group-head">
        <div>
          <h2>${rich('d295aa9')}</h2>
          <p>${rich('dcf895d', [this.detail?.summary.display || this.alias])}</p>
        </div>
      </div>
      ${this.chats.length
        ? html`<div class="card">${this.chats.map((c) => this.chatLink(c, 'chats'))}</div>`
        : html`<div class="card">${emptyState(translate('d437c79'), translate('d797cab'))}</div>`}`;
  }

  private docCard(d: DocDraft, title: string, hint: string): TemplateResult {
    return html`<section class="card card-pad doc-card" aria-label=${title}>
      <div class="editor-head">
        <div>
          <h2>${title}</h2>
          <p class="muted">${hint}</p>
        </div>
        ${d.view ? html`<span class="editor-path">${d.view.path}</span>` : nothing}
      </div>
      ${d.loadError
        ? html`<p class="row-error">${d.loadError}</p>`
        : d.view
          ? html`${docEditor({
              text: d.text,
              label: title,
              readonly: !d.view.editable,
              placeholder: d.view.exists ? '' : translate('d9b7cea'),
              onInput: (v) => d.setText(v),
            })}${d.error ? html`<p class="row-error">${icon('circleAlert', 'icon-sm')}${d.error}</p>` : nothing}`
          : html`<span class="sk sk-w1"></span>`}
    </section>`;
  }

  private tabDocs(): TemplateResult {
    return html`${this.docCard(this.claude, 'CLAUDE.md', translate('d04ed47'))}
    ${this.docCard(this.soul, 'SOUL.md', translate('daf99f4'))}`;
  }

  private tabYaml(): TemplateResult {
    const r = this.raw;
    const broken = this.detail?.summary.load_error;
    if (r.loadError) return errorBanner(translate('dd523c9'), r.loadError, () => r.load());
    if (!r.view) return skeletonCard(8);
    const check = r.lastCheck;
    return html`${broken
        ? html`<div class="banner banner-error" role="alert">
            ${icon('circleAlert')}
            <div class="banner-body">
              <strong>${rich('dc58a0c')}</strong>
              <p class="mono">${broken}</p>
              <p>${rich('df21f72')}</p>
            </div>
          </div>`
        : nothing}
      <div class="editor-head">
        <div>
          <span class="editor-path">${r.view.path}</span>
          <p class="muted">${rich('db0d77c')}<code>cognition:</code> ${rich('d1b9a28')}</p>
        </div>
        <button class="btn btn-sm" @click=${() => r.check()}>${rich('de12bee', [icon('circleCheck', 'icon-sm')])}</button>
      </div>
      ${check && !check.errors.length && !check.warnings.length ? html`<p class="row-note">${icon('check', 'icon-sm')}<span>${rich('d6260cd')}</span></p>` : nothing}
      ${check ? issuesList(check.errors, check.warnings) : nothing}
      ${r.error ? html`<p class="row-error">${icon('circleAlert', 'icon-sm')}${r.error}</p>` : nothing}
      ${yamlEditor({ text: r.text, label: translate('dc856e6'), errors: check?.errors ?? [], readonly: !r.view.editable, onInput: (v) => r.setText(v) })}`;
  }

  private header(): TemplateResult {
    const agents = store.state.status?.agents ?? [];
    const d = this.detail;
    const summary = d?.summary ?? agents.find((a) => a.alias === this.alias);
    const files = store.state.status?.restart_pending.map((r) => r.file) ?? [];
    const state = summary ? agentState(summary, files) : null;
    const pick = html`<span class="select-wrap agent-pick">
      <select
        class="select"
        aria-label="${translate('d15ca36')}"
        .value=${this.alias}
        @change=${(e: Event) => {
          const v = (e.target as HTMLSelectElement).value;
          void navigate(hrefFor({ page: 'agent', alias: v, tab: this.tab })).then(() => {
            (e.target as HTMLSelectElement).value = this.alias;
          });
        }}
      >
        ${agents.map((a) => html`<option value=${a.alias} ?selected=${a.alias === this.alias}>${a.display || a.alias} · ${a.alias}</option>`)}
      </select>
      ${icon('chevronDown')}
    </span>`;
    return pageHead(
      html`<span class="title-with-pick">${summary?.display || this.alias}${state
          ? html`<span class="chip ${state.tone ? `chip-${state.tone}` : 'chip-quiet'}">${state.text}</span>`
          : nothing}</span>`,
      html`<span class="mono">${this.alias}</span>${summary && !summary.load_error ? html` · ${agentLine(summary)}` : nothing}`,
      pick,
      { href: '/agents', label: 'Agents' },
    );
  }

  private tabs(): TemplateResult {
    return html`<nav class="tabs" aria-label="${translate('d720ab3')}">
      ${AGENT_TABS.map(
        (t) => html`<a class="tab" href=${hrefFor({ page: 'agent', alias: this.alias, tab: t.id })} aria-current=${t.id === this.tab ? 'page' : nothing}
          >${t.label}</a
        >`,
      )}
    </nav>`;
  }

  private body(): TemplateResult {
    if (this.loadError && !this.detail) return errorBanner(translate('d11f19a'), this.loadError, () => this.refresh());
    switch (this.tab) {
      case 'overview':
        return this.tabOverview();
      case 'runtime':
      case 'messaging':
        return this.sectionGroups(TAB_SECTIONS[this.tab]!);
      case 'permissions':
        return this.tabPermissions();
      case 'chats':
        return this.tabChats();
      case 'docs':
        return this.tabDocs();
      case 'yaml':
        return this.tabYaml();
    }
  }

  protected override render(): TemplateResult {
    const head = html`${this.header()}${this.tabs()}`;
    const body = html`${this.conflictBanner()}${this.body()}${this.saveBar()}`;
    if (this.tab === 'messaging' && !this.detail?.summary.load_error) {
      return this.withPreview(head, body, html`<sm-preview .ctl=${this.preview} .botName=${this.detail?.summary.display || this.alias}></sm-preview>`);
    }
    return html`<div class="page">${head}${body}</div>`;
  }
}

customElements.define('sm-page-agent', AgentPage);
