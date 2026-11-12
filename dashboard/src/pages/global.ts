import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import type { FieldDesc } from '../api/types';
import { ConfigDraft, DocDraft, RawDraft } from '../app/editable';
import { GLOBAL_DIRTY_EVENT, GLOBAL_DOC, globalGroupOf, globalSections } from '../app/nav';
import { hrefFor, navigate, type Route } from '../app/router';
import { store } from '../app/store';
import { confirmDialog, reviewRawDialog } from '../components/dialog';
import { docEditor, issuesList, yamlEditor } from '../components/editors';
import { fieldsIn, groupSection } from '../components/field-row';
import { icon } from '../components/icon';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { PreviewController } from '../preview/controller';
import '../preview/panel';
import { previewBotName } from '../util/labels';

type GlobalRoute = Extract<Route, { page: 'global' }>;

const DASHBOARD_KEYS = ['dashboard.enabled', 'dashboard.port', 'dashboard.public_base_url', 'preview_server.bind'];

export function botNameForPreview(): string {
  return previewBotName(store.state.status?.agents ?? [], store.state.whoami?.identity);
}

/** 主 agent 的候选:「自动」加上此刻可用的 agent(服务端写入时还会再判一次)。 */
export function primaryAgentOptions(): Array<{ value: string; label: string }> {
  const agents = (store.state.status?.agents ?? []).filter((a) => a.enabled && a.routable && !a.load_error);
  return [
    { value: '', label: translate('analysis.auto') },
    ...agents.map((a) => ({ value: a.alias, label: a.display && a.display !== a.alias ? `${a.display} · ${a.alias}` : a.alias })),
  ];
}

export function modeSwitch(mode: 'form' | 'yaml', onPick: (m: 'form' | 'yaml') => void): TemplateResult {
  return html`<div class="seg" role="radiogroup" aria-label="${translate('d400d0e')}">
    ${(['form', 'yaml'] as const).map(
      (m) => html`<label class="seg-opt">
        <input type="radio" name="edit-mode" .checked=${mode === m} @change=${() => onPick(m)} /><span>${m === 'form' ? translate('d62abf7') : 'YAML'}</span>
      </label>`,
    )}
  </div>`;
}

class GlobalPage extends PageBase {
  private fields = new Map<string, FieldDesc>();
  private config = new ConfigDraft({ level: 'global' }, 'config.yaml', () => this.fields, this.call, () => this.changed());
  private doc = new DocDraft({ level: 'global' }, () => translate('ddd96b8'), this.call, () => this.requestUpdate());
  private raw = new RawDraft({ file: 'global' }, 'config.yaml', this.call, reviewRawDialog, () => this.requestUpdate());
  private preview = new PreviewController(this.call, { level: 'global' }, () => this.config.changeList());
  private mode: 'form' | 'yaml' = 'form';
  private target: string | null = null;
  private dirtySent = '';

  private changed() {
    this.preview.schedule();
    this.requestUpdate();
  }

  override willUpdate(): void {
    const r = this.route as GlobalRoute;
    if (r && r.mode !== this.mode) {
      this.mode = r.mode;
      this.edits.replace(this.mode === 'form' ? [this.config, this.doc] : [this.raw]);
      if (this.mode === 'yaml' && !this.raw.view) void this.raw.load();
    }
    if (r?.key && r.key !== this.target) {
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
    this.bindPreview(this.preview);
    this.edits.replace([this.config, this.doc]);
    void this.refresh();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.preview.dispose();
    this.dirtySent = '';
    window.dispatchEvent(new CustomEvent(GLOBAL_DIRTY_EVENT, { detail: [] }));
  }

  /** 这一页显示的是哪一组 */
  private group(): string {
    return globalGroupOf(this.route as GlobalRoute, store.state.schema);
  }

  /** 草稿留在别的组里时,导航上那一组标个点(保存条的计数是整份草稿的)。 */
  protected override updated(): void {
    const groups = new Set<string>();
    for (const c of this.config.changeList()) {
      const g = this.fields.get(c.key)?.group;
      if (g) groups.add(g);
    }
    if (this.doc.dirtyCount()) groups.add(GLOBAL_DOC);
    const list = [...groups].sort();
    const sig = list.join(',');
    if (sig === this.dirtySent) return;
    this.dirtySent = sig;
    window.dispatchEvent(new CustomEvent(GLOBAL_DIRTY_EVENT, { detail: list }));
  }

  async refresh(): Promise<void> {
    await Promise.all([this.config.load(), this.doc.load(), this.mode === 'yaml' ? this.raw.load() : Promise.resolve()]);
    this.loading = false;
    void this.preview.request();
    this.requestUpdate();
    if (this.target) this.focusTarget(this.target);
  }

  protected override async beforeSave(): Promise<boolean> {
    const keys = this.config.changeList().map((c) => c.key);
    if (!keys.some((k) => DASHBOARD_KEYS.includes(k))) return true;
    return confirmDialog({
      title: translate('da80d66'),
      body: translate('db5b304'),
      confirm: translate('d35ffbf'),
    });
  }

  private async pickMode(m: 'form' | 'yaml') {
    if (m === this.mode) return;
    if (this.edits.dirtyCount()) {
      const ok = await confirmDialog({
        title: translate('d102832'),
        body: translate('d4c5e93', [m === 'yaml' ? translate('d62abf7') : 'YAML', this.edits.dirtyCount()]),
        confirm: translate('daa65ca'),
        danger: true,
      });
      if (!ok) {
        this.requestUpdate();
        return;
      }
      this.discard();
    }
    const group = (this.route as GlobalRoute).group;
    await navigate(hrefFor(group ? { page: 'global', mode: m, group } : { page: 'global', mode: m }), { replace: true });
  }

  private form(group: string): TemplateResult {
    if (group === GLOBAL_DOC) return this.docCard();
    const schema = store.state.schema!;
    const env = { draft: this.config, level: 'global' as const, preview: this.preview, target: this.highlight, call: this.call, options: { primary_agent: primaryAgentOptions() } };
    if (this.config.loadError) return errorBanner(translate('dedc371'), this.config.loadError, () => this.refresh());
    if (!this.config.view) return skeletonCard(6);
    const g = schema.groups.find((x) => x.id === group);
    const section = g ? groupSection(g, fieldsIn(schema, g.id).filter((f) => f.key !== 'language'), env, { bare: true }) : nothing;
    return section === nothing ? emptyState(translate('dfe58f8'), translate('d71e0cf')) : section;
  }

  private docCard(): TemplateResult {
    return html`<section class="group" id="g-doc" aria-label="${translate('ddd96b8')}">
        <div class="card card-pad">
          ${this.doc.loadError
            ? html`<p class="row-error">${this.doc.loadError}</p>`
            : this.doc.view
              ? html`<div class="editor-head">
                    <span class="editor-path">${this.doc.view.path}</span>
                    ${this.doc.dirtyCount() ? html`<span class="chip">${rich('d6a1be0')}</span>` : nothing}
                  </div>
                  ${docEditor({
                    text: this.doc.text,
                    label: translate('dd138e5'),
                    readonly: !this.doc.view.editable,
                    placeholder: translate('d81782c'),
                    onInput: (v) => this.doc.setText(v),
                  })}
                  ${this.doc.error ? html`<p class="row-error">${icon('circleAlert', 'icon-sm')}${this.doc.error}</p>` : nothing}`
              : html`<span class="sk sk-w1"></span>`}
        </div>
      </section>`;
  }

  private yaml(): TemplateResult {
    const r = this.raw;
    if (r.loadError) return errorBanner(translate('def3200'), r.loadError, () => r.load());
    if (!r.view) return skeletonCard(8);
    const check = r.lastCheck;
    return html`<div class="editor-head">
        <div>
          <span class="editor-path">${r.view.path}</span>
          <p class="muted">${rich('de1d8cd')}</p>
        </div>
        <button class="btn btn-sm" @click=${() => r.check()}>${rich('de12bee', [icon('circleCheck', 'icon-sm')])}</button>
      </div>
      ${check && !check.errors.length && !check.warnings.length ? html`<p class="row-note">${icon('check', 'icon-sm')}<span>${rich('d6260cd')}</span></p>` : nothing}
      ${check ? issuesList(check.errors, check.warnings) : nothing}
      ${r.error ? html`<p class="row-error">${icon('circleAlert', 'icon-sm')}${r.error}</p>` : nothing}
      ${yamlEditor({ text: r.text, label: translate('df93771'), errors: check?.errors ?? [], readonly: !r.view.editable, onInput: (v) => r.setText(v) })}`;
  }

  protected override render(): TemplateResult {
    const modes = modeSwitch(this.mode, (m) => this.pickMode(m));
    if (this.mode === 'yaml') {
      const head = pageHead(translate('d3f754f'), translate('da88453'), modes);
      return html`<div class="page">${head}${this.conflictBanner()}${this.yaml()}${this.saveBar()}</div>`;
    }
    const schema = store.state.schema;
    const group = this.group();
    const sec = globalSections(schema).find((s) => s.id === group);
    const head = pageHead(sec?.label ?? translate('d3f754f'), sec?.description || translate('db33ada'), modes);
    const body = html`${this.conflictBanner()}${this.form(group)}${this.saveBar()}`;
    // 这一组没有会改变飞书效果的项,就不占一栏放预览。
    if (!fieldsIn(schema!, group).some((f) => f.preview)) return html`<div class="page">${head}${body}</div>`;
    return this.withPreview(head, body, html`<sm-preview .ctl=${this.preview} .botName=${botNameForPreview()}></sm-preview>`);
  }
}

customElements.define('sm-page-global', GlobalPage);
