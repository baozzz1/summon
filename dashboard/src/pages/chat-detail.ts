import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { ApiCallError, errorMessage } from '../api/client';
import type { ChatAgentView, ChatDetail, FieldDesc, MentionlessJob, Scope } from '../api/types';
import { ConfigDraft, DocDraft, ValueDraft } from '../app/editable';
import type { Route } from '../app/router';
import { store } from '../app/store';
import { switchCtl, textCtl } from '../components/controls';
import { confirmDialog } from '../components/dialog';
import { docEditor } from '../components/editors';
import { fieldRow, type RowEnv } from '../components/field-row';
import { icon } from '../components/icon';
import { ident } from '../components/ident';
import { inlineCode } from '../components/inline';
import { errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { editScheduleDialog, scheduleRow, toggleSchedule } from '../components/schedule';
import { toast } from '../components/toast';
import { PreviewController } from '../preview/controller';
import '../preview/panel';
import { chatTitle } from '../util/labels';

type ChatRoute = Extract<Route, { page: 'chat' }>;

const WD_SOURCE: Record<string, string> = {
  get chat_override() { return translate('d16973c'); },
  get chat_mapping() { return translate('dacbb8c'); },
  get agent_default() { return translate('d968ebe'); },
};

interface Section {
  view: ChatAgentView;
  config: ConfigDraft;
  operators: ValueDraft<string[]>;
  opRevision: string;
  mapping: ValueDraft<string>;
  doc: DocDraft;
  docOpen: boolean;
  job: MentionlessJob | null;
  busy: boolean;
}

class ChatPage extends PageBase {
  private chatId = '';
  private detail: ChatDetail | null = null;
  private sections = new Map<string, Section>();
  private fields = new Map<string, FieldDesc>();
  private active = '';
  private preview = new PreviewController(this.call, null, () => this.sections.get(this.active)?.config.changeList() ?? []);
  private scrolled = false;

  protected override languageChanged(): void {
    this.fields = new Map((store.state.schema?.fields ?? []).map((field) => [field.key, field]));
    if (this.preview) void this.preview.request();
    super.languageChanged();
  }

  override connectedCallback(): void {
    super.connectedCallback();
    for (const f of store.state.schema?.fields ?? []) this.fields.set(f.key, f);
    this.bindPreview(this.preview);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.preview.dispose();
  }

  override willUpdate(): void {
    const r = this.route as ChatRoute;
    if (r && r.chatId !== this.chatId) {
      this.chatId = r.chatId;
      this.detail = null;
      this.sections.clear();
      this.loading = true;
      this.scrolled = false;
      void this.refresh();
    }
  }

  protected override watchKeys(): string[] {
    return ['schedules'];
  }

  private agentName(alias: string): string {
    return store.state.status?.agents.find((a) => a.alias === alias)?.display || alias;
  }

  private build(v: ChatAgentView): Section {
    const chatId = this.chatId;
    const scope: Scope = { level: 'chat', agent: v.agent, chat_id: chatId };
    const name = this.agentName(v.agent);
    const config = new ConfigDraft(scope, () => translate('dbc3dde', [name]), () => this.fields, this.call, () => {
      if (this.active === v.agent) this.preview.schedule();
      this.requestUpdate();
    });
    config.setView(v.config);
    const sec: Section = {
      view: v,
      config,
      opRevision: v.operators.revision,
      operators: new ValueDraft<string[]>(
        `ops:${v.agent}`,
        () => translate('d2a59d4', [name]),
        v.operators.members.map((m) => m.id),
        async (members) => {
          const r = await this.call('chats.set_operators', { agent: v.agent, chat_id: chatId, revision: sec.opRevision, members });
          sec.opRevision = r.revision;
          return members;
        },
        async () => (await this.refetch(sec))?.operators.members.map((m) => m.id) ?? [],
        () => this.requestUpdate(),
        `agent:${v.agent}`,
      ),
      mapping: new ValueDraft<string>(
        `wd:${v.agent}`,
        () => translate('dc994af', [name]),
        v.work_dir.mapping ?? '',
        async (mapping) => {
          const nv = await this.call('chats.set_work_dir', {
            agent: v.agent,
            chat_id: chatId,
            revision: sec.opRevision,
            mapping: mapping.trim() || null,
            clear_override: false,
          });
          sec.view = nv;
          sec.opRevision = nv.operators.revision;
          return nv.work_dir.mapping ?? '';
        },
        async () => (await this.refetch(sec))?.work_dir.mapping ?? '',
        () => this.requestUpdate(),
        `agent:${v.agent}`,
      ),
      doc: new DocDraft({ level: 'chat', agent: v.agent, chat_id: chatId }, () => translate('dc27f3c', [name]), this.call, () => this.requestUpdate()),
      docOpen: v.doc.exists,
      job: null,
      busy: false,
    };
    return sec;
  }

  /** 冲突后重读这一节:名单与按群映射都在 agent.yaml 里,共用一个版本。 */
  private async refetch(sec: Section): Promise<ChatAgentView | undefined> {
    const d = await this.call('chats.get', { chat_id: this.chatId });
    const fresh = d.agents.find((a) => a.agent === sec.view.agent);
    if (fresh) {
      sec.view = fresh;
      sec.opRevision = fresh.operators.revision;
      sec.config.setView(fresh.config);
    }
    return fresh;
  }

  async refresh(): Promise<void> {
    const chatId = this.chatId;
    try {
      const d = await this.call('chats.get', { chat_id: chatId });
      if (chatId !== this.chatId) return;
      this.detail = d;
      for (const v of d.agents) {
        const old = this.sections.get(v.agent);
        if (old) {
          old.view = v;
          old.config.setView(v.config);
          old.opRevision = v.operators.revision;
          old.operators.rebase(v.operators.members.map((m) => m.id));
          old.mapping.rebase(v.work_dir.mapping ?? '');
        } else this.sections.set(v.agent, this.build(v));
      }
      const all = [...this.sections.values()];
      this.edits.replace(all.flatMap((s) => [s.config, s.operators, s.mapping, s.doc]));
      await Promise.all(all.map((s) => (s.view.doc.exists && !s.doc.view ? s.doc.load() : Promise.resolve())));
      const want = (this.route as ChatRoute).agent;
      const first = want && this.sections.has(want) ? want : d.agents[0]?.agent;
      if (first && !this.active) this.activate(first);
      else void this.preview.request();
      this.loadError = '';
    } catch (err) {
      this.loadError = errorMessage(err);
    }
    this.loading = false;
    this.requestUpdate();
    const want = (this.route as ChatRoute).agent;
    if (want && !this.scrolled) {
      this.scrolled = true;
      void this.updateComplete.then(() => document.getElementById(`a-${want}`)?.scrollIntoView({ block: 'start' }));
    }
  }

  /** 预览跟着正在改的那一节走。 */
  private activate(alias: string) {
    if (this.active === alias) return;
    this.active = alias;
    this.preview.setScope({ level: 'chat', agent: alias, chat_id: this.chatId });
    this.requestUpdate();
  }

  private async setMentionless(sec: Section, on: boolean) {
    const alias = sec.view.agent;
    sec.busy = true;
    sec.job = on ? { job: '', status: 'pending', message: translate('d482bd2') } : null;
    this.requestUpdate();
    try {
      let job = await this.call('chats.set_mentionless', { agent: alias, chat_id: this.chatId, enabled: on });
      sec.job = job;
      this.requestUpdate();
      const deadline = Date.now() + 30_000;
      while (job.status === 'pending' && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 800));
        job = await this.call('chats.mentionless_job', { job: job.job });
        sec.job = job;
        this.requestUpdate();
      }
      if (job.status === 'pending') sec.job = { ...job, status: 'failed', message: translate('d659fca') };
      if (job.status === 'done') toast(on ? translate('d33abb4') : translate('d802b86'), 'ok');
      await this.refresh();
      if (!on) sec.job = null;
    } catch (err) {
      sec.job = { job: '', status: 'failed', message: errorMessage(err) };
    }
    sec.busy = false;
    this.requestUpdate();
  }

  private async clearOverride(sec: Section) {
    const ok = await confirmDialog({
      title: translate('d3688a1'),
      body: translate('db3a665', [sec.view.work_dir.mapping ? translate('d881f54') : translate('d778e74')]),
      confirm: translate('dbce237'),
    });
    if (!ok) return;
    try {
      sec.view = await this.call('chats.set_work_dir', {
        agent: sec.view.agent,
        chat_id: this.chatId,
        revision: sec.opRevision,
        mapping: sec.mapping.base.trim() || null,
        clear_override: true,
      });
      sec.opRevision = sec.view.operators.revision;
      toast(translate('d1a6f57'), 'ok');
    } catch (err) {
      if (err instanceof ApiCallError && err.code === 'conflict') {
        // 别处刚改过 agent.yaml:重读这一节再让人确认,不在旧版本上清
        await this.refetch(sec);
        toast(translate('d302143'), 'warn');
      } else toast(translate('d055d4f', [errorMessage(err)]), 'error');
    }
    this.requestUpdate();
  }

  private block(title: string, body: TemplateResult, hint?: string | TemplateResult): TemplateResult {
    return html`<section class="group">
      <header class="group-head">
        <div>
          <h3>${title}</h3>
          ${hint ? html`<p>${hint}</p>` : nothing}
        </div>
      </header>
      ${body}
    </section>`;
  }

  /** 这个 agent 在本群的开关全都改不了时,原因(通常是不在它的操作名单里) */
  private readOnlyReason(sec: Section): string | null {
    const vals = sec.view.config.values;
    if (!vals.length || vals.some((v) => v.editable)) return null;
    return vals.find((v) => v.deny_reason)?.deny_reason ?? translate('d5f24a8');
  }

  private switches(sec: Section): TemplateResult {
    const env: RowEnv = {
      draft: sec.config,
      level: 'chat',
      preview: this.active === sec.view.agent ? this.preview : null,
      quietLock: !!this.readOnlyReason(sec),
    };
    const keys = sec.view.config.values.map((v) => v.key);
    const fields = keys.map((k) => this.fields.get(k)).filter((f): f is FieldDesc => !!f);
    if (!fields.length) return html`<div class="card"><div class="empty"><p>${rich('d32cdc4')}</p></div></div>`;
    return html`<div class="card">${fields.map((f) => fieldRow(f, env))}</div>`;
  }

  private mentionless(sec: Section): TemplateResult {
    const m = sec.view.mentionless;
    const job = sec.job;
    const canToggle = m.enabled ? m.can_disable : m.can_enable;
    return html`<div class="row row--inline ${!canToggle && !sec.busy ? 'is-disabled' : ''}">
        <div class="row-grid">
          <div class="row-head">
            <div class="row-title"><span class="row-label">${rich('d40a899')}</span></div>
            <span class="row-key">feishu.chat_reply_without_mention</span>
            <p class="row-desc">${rich('d47c4ed')}</p>
            <div class="row-meta"><span class="chip chip-quiet">${rich('d0d8877')}</span></div>
          </div>
          <div class="row-ctl">
            ${switchCtl({
              checked: m.enabled,
              label: translate('d40a899'),
              disabled: !canToggle || sec.busy,
              onChange: (v) => this.setMentionless(sec, v),
            })}
          </div>
        </div>
        ${m.reason && !m.enabled && !job ? html`<p class="row-note">${icon('info', 'icon-sm')}<span>${inlineCode(m.reason)}</span></p>` : nothing}
        ${job
          ? html`<div class="job ${job.status}" role="status">
              ${job.status === 'pending'
                ? icon('loaderCircle', 'icon-sm spin')
                : job.status === 'done'
                  ? icon('circleCheck', 'icon-sm')
                  : icon('circleAlert', 'icon-sm')}
              <span>
                ${job.status === 'pending' ? translate('d672fe0') : ''}${job.message ?? ''}
                ${job.apply_url
                  ? html`<a class="link-btn" href=${job.apply_url} target="_blank" rel="noopener noreferrer">${rich('da342d5', [icon('externalLink', 'icon-sm')])}</a>`
                  : nothing}
              </span>
            </div>`
          : nothing}
      </div>`;
  }

  private operators(sec: Section): TemplateResult {
    const o = sec.view.operators;
    const changed = sec.operators.dirtyCount() > 0;
    return html`<div class="row row--block ${changed ? 'is-changed' : ''} ${!o.editable ? 'is-disabled' : ''}">
        <div class="row-grid">
          <div class="row-head">
            <div class="row-title"><span class="row-label">${rich('de3137d')}</span></div>
            <span class="row-key">feishu.chat_operators.${this.chatId}</span>
            <p class="row-desc">${rich('dc507a8', [this.agentName(sec.view.agent)])}</p>
          </div>
          <div class="row-ctl">
            <sm-tags
              .values=${sec.operators.value}
              .members=${o.members}
              .label=${translate('de3137d')}
              ?disabled=${!o.editable}
              placeholder="${translate('d083393')}"
              @change=${(e: CustomEvent<string[]>) => sec.operators.set(e.detail)}
            ></sm-tags>
            ${!o.editable && o.reason ? html`<span class="row-lock">${icon('lock', 'icon-sm')}<span>${o.reason}</span></span>` : nothing}
          </div>
        </div>
        ${sec.operators.error ? html`<p class="row-error">${icon('circleAlert', 'icon-sm')}<span>${sec.operators.error}</span></p>` : nothing}
      </div>`;
  }

  private workDir(sec: Section): TemplateResult {
    const w = sec.view.work_dir;
    const changed = sec.mapping.dirtyCount() > 0;
    return html`<div class="card">
      <div class="row">
        <dl class="kv-list">
          <dt>${rich('d7ed094')}</dt>
          <dd class="wd-path">${w.effective ?? translate('d2f5f1d')}</dd>
          <dt>${rich('da488e9')}</dt>
          <dd>${w.source ? WD_SOURCE[w.source] : '—'}</dd>
        </dl>
        ${w.live || w.reason
          ? html`<p class="row-note">${icon('info', 'icon-sm')}<span>${w.reason ?? translate('df6754e')}</span></p>`
          : nothing}
      </div>
      <div class="row ${changed ? 'is-changed' : ''} ${!w.editable ? 'is-disabled' : ''}">
        <div class="row-grid">
          <div class="row-head">
            <div class="row-title"><span class="row-label">${rich('d83d89d')}</span></div>
            <span class="row-key">runtime.chat_working_dirs.${this.chatId}</span>
            <p class="row-desc">${rich('d2869c8')}</p>
          </div>
          <div class="row-ctl">
            ${textCtl({
              value: sec.mapping.value,
              label: translate('db27aea'),
              mono: true,
              wide: true,
              placeholder: '~/workspace/…',
              disabled: !w.editable,
              onChange: (v) => sec.mapping.set(v),
            })}
            ${changed ? html`<span class="row-orig">${rich('dc2daf0')} <s>${sec.mapping.base || translate('de026c6')}</s></span>` : nothing}
          </div>
        </div>
        ${sec.mapping.error ? html`<p class="row-error">${icon('circleAlert', 'icon-sm')}<span>${sec.mapping.error}</span></p>` : nothing}
      </div>
      ${w.override
        ? html`<div class="row">
            <div class="row-grid">
              <div class="row-head">
                <div class="row-title"><span class="row-label">${rich('d5a5872')}</span></div>
                <p class="row-desc wd-path">${w.override}</p>
              </div>
              <div class="row-ctl">
                <button class="btn btn-sm" ?disabled=${!w.editable} @click=${() => this.clearOverride(sec)}>${rich('d14266a')}</button>
              </div>
            </div>
          </div>`
        : nothing}
    </div>`;
  }

  private doc(sec: Section): TemplateResult {
    const d = sec.doc;
    if (!sec.docOpen) {
      return html`<div class="card">
        <div class="empty">
          <h3>${rich('d1a9b25')}</h3>
          <p>${rich('d642c47', [this.agentName(sec.view.agent)])}</p>
          <div class="empty-actions">
            <button
              class="btn"
              @click=${async () => {
                sec.docOpen = true;
                if (!d.view) await d.load();
                this.requestUpdate();
              }}
            >
              ${rich('d1ab0e4', [icon('plus', 'icon-sm')])}
            </button>
          </div>
        </div>
      </div>`;
    }
    return html`<div class="card card-pad">
      ${d.view
        ? html`<div class="editor-head"><span class="editor-path">${d.view.path}</span>${d.view.exists ? nothing : html`<span class="chip">${rich('dd9abaa')}</span>`}</div>
            ${docEditor({ text: d.text, label: translate('d08d3e1'), readonly: !d.view.editable, placeholder: translate('d40e9ed'), onInput: (v) => d.setText(v) })}
            ${d.error ? html`<p class="row-error">${icon('circleAlert', 'icon-sm')}${d.error}</p>` : nothing}`
        : d.loadError
          ? html`<p class="row-error">${d.loadError}</p>`
          : html`<span class="sk sk-w1"></span>`}
    </div>`;
  }

  private schedules(sec: Section): TemplateResult {
    const list = sec.view.schedules;
    if (!list.length) {
      return html`<div class="card"><div class="empty"><p>${rich('d09011e', [this.agentName(sec.view.agent)])}</p></div></div>`;
    }
    const replace = (v: ScheduleViewLike) => {
      sec.view = { ...sec.view, schedules: sec.view.schedules.map((x) => (x.id === v.id ? v : x)) };
      this.requestUpdate();
    };
    return html`<div class="card">
      ${[...list]
        .sort((a, b) => Number(b.enabled) - Number(a.enabled))
        .map((s) =>
          scheduleRow(s, {
            onToggle: async (x, on) => {
              const v = await toggleSchedule(this.call, x, on);
              if (v) replace(v);
            },
            onEdit: async (x) => {
              const v = await editScheduleDialog(this.call, x);
              if (v) replace(v);
            },
          }),
        )}
    </div>`;
  }

  private section(sec: Section): TemplateResult {
    const v = sec.view;
    const name = this.agentName(v.agent);
    const n = v.config.values.filter((x) => x.value !== null).length + v.operators.members.length + (v.work_dir.override ? 1 : 0);
    const summary = [v.work_dir.live ? translate('d7d817d') : translate('dc1f158'), n ? translate('ded467e', [n]) : translate('d23590d'), v.mentionless.enabled ? translate('d3015b6') : ''].filter(Boolean).join(' · ');
    return html`<section
      class="agent-section"
      id=${`a-${v.agent}`}
      aria-labelledby=${`ah-${v.agent}`}
      @mouseenter=${() => this.activate(v.agent)}
      @focusin=${() => this.activate(v.agent)}
    >
      <header class="agent-section-head">
        <span class="avatar avatar-lg" aria-hidden="true">${[...name][0]}</span>
        <div>
          <h2 id=${`ah-${v.agent}`}>${name} <span class="mono muted">${v.agent}</span></h2>
          <p>${summary}</p>
        </div>
      </header>
      ${this.readOnlyReason(sec)
        ? html`<p class="row-note section-note">${icon('lock', 'icon-sm')}<span>${rich('dce6171', [inlineCode(this.readOnlyReason(sec) ?? '')])}</span></p>`
        : nothing}
      ${this.block(translate('d1a37d0'), this.switches(sec), translate('dd38b2e'))}
      ${this.detail?.kind === 'group'
        ? this.block(translate('debcff4'), html`<div class="card">${this.mentionless(sec)}${this.operators(sec)}</div>`)
        : nothing}
      ${this.block(translate('d3db7b0'), this.workDir(sec))} ${this.block(translate('d032d5c'), this.doc(sec), translate('de3a102'))}
      ${this.block(translate('dac7cc1'), this.schedules(sec))}
    </section>`;
  }

  protected override render(): TemplateResult {
    const d = this.detail;
    const named = !d || !!d.name;
    const title = d ? (named ? chatTitle(d) : ident(d.chat_id, { agent: this.active })) : translate('d9fca8b');
    const head = pageHead(
      title,
      html`${named ? nothing : html`<span>${rich('d6fd1f1')} </span>`}<button
        class="copy-id"
        title="${translate('da74d5a')}"
        @click=${async () => {
          try {
            await navigator.clipboard.writeText(this.chatId);
            toast(translate('d024ad8'), 'ok', 1600);
          } catch {
            toast(translate('d5fe28d'), 'warn');
          }
        }}
      >
        ${named ? this.chatId : translate('da74d5a')}${icon('copy', 'icon-sm')}
      </button>`,
      undefined,
      { href: '/chats', label: translate('d35b49e') },
    );
    const body = this.loadError && !d
      ? errorBanner(translate('d6657ea'), this.loadError, () => this.refresh())
      : !d
        ? skeletonCard(6)
        : html`${[...this.sections.values()].map((s) => this.section(s))}`;
    return this.withPreview(head, html`${this.conflictBanner()}${body}${this.saveBar()}`, html`<sm-preview .ctl=${this.preview} .botName=${this.agentName(this.active)}></sm-preview>`);
  }
}

type ScheduleViewLike = ChatAgentView['schedules'][number];

customElements.define('sm-page-chat', ChatPage);
