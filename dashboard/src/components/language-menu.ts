import { html, LitElement, nothing } from 'lit';
import { live } from 'lit/directives/live.js';
import { errorMessage } from '../api/client';
import type { ConfigView } from '../api/types';
import { store } from '../app/store';
import { LocaleController, t } from '../i18n';

const OPTIONS = ['auto', 'zh-CN', 'en'] as const;
type Preference = typeof OPTIONS[number];

export class LanguageMenu extends LitElement {
  readonly locale = new LocaleController(this);
  private view: ConfigView | null = null;
  private loading = false;
  private loaded = false;
  private saving = false;
  private error = '';
  private read = 0;

  protected override createRenderRoot(): HTMLElement { return this; }

  override connectedCallback(): void {
    super.connectedCallback();
    document.addEventListener('pointerdown', this.onOutside);
    document.addEventListener('keydown', this.onKey);
    store.addEventListener('change', this.onStore);
    void this.load();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    ++this.read;
    document.removeEventListener('pointerdown', this.onOutside);
    document.removeEventListener('keydown', this.onKey);
    store.removeEventListener('change', this.onStore);
  }

  private readonly onStore = () => this.requestUpdate();
  private readonly onOutside = (event: Event) => {
    if (!event.composedPath().includes(this)) this.close();
  };
  private readonly onKey = (event: KeyboardEvent) => {
    const details = this.querySelector('details');
    if (event.key === 'Escape' && details?.open) {
      this.close();
      this.querySelector('summary')?.focus();
    }
  };
  private close(): void {
    const details = this.querySelector('details');
    if (details) details.open = false;
  }

  private preference(): Preference {
    const field = this.view?.values.find((f) => f.key === 'language');
    const value = field?.value ?? field?.effective;
    return OPTIONS.includes(value as Preference) ? value as Preference : 'auto';
  }

  private canEdit(): boolean {
    return this.loaded && !!store.state.whoami?.machine && !!this.view?.values.find((f) => f.key === 'language')?.editable;
  }

  private async load(): Promise<void> {
    if (this.saving || !store.state.whoami?.machine) return;
    const read = ++this.read;
    this.loading = true;
    this.requestUpdate();
    try {
      const view = await store.call('config.get', { scope: { level: 'global' } });
      if (read !== this.read) return;
      this.view = view;
      this.loaded = true;
      this.error = '';
    } catch (err) {
      if (read !== this.read) return;
      this.loaded = false;
      this.error = errorMessage(err);
    } finally {
      if (read === this.read) {
        this.loading = false;
        this.requestUpdate();
      }
    }
  }

  private async choose(value: Preference): Promise<void> {
    if (this.loading || this.saving || !this.view || !this.canEdit() || value === this.preference()) {
      this.requestUpdate();
      return;
    }
    ++this.read;
    this.saving = true;
    this.error = '';
    this.requestUpdate();
    try {
      const result = await store.call('config.patch', {
        scope: { level: 'global' }, revision: this.view.revision, changes: [{ key: 'language', value }],
      });
      this.view = { ...this.view, revision: result.revision,
        values: this.view.values.map((field) => field.key === 'language' ? { ...field, value } : field) };
      store.setRestartPending(result.restart_pending);
      // Other open editors receive the revision change and retain their own drafts.
      await store.refreshStatus();
    } catch (err) {
      this.error = errorMessage(err);
    } finally {
      this.saving = false;
      this.requestUpdate();
    }
  }

  protected override render() {
    const denied = !store.state.whoami?.machine;
    const disabled = this.loading || this.saving || !this.canEdit();
    const reason = this.view?.values.find((f) => f.key === 'language')?.deny_reason;
    return html`<details class="language-menu" @toggle=${(e: Event) => {
      if ((e.target as HTMLDetailsElement).open) void this.load();
    }}>
      <summary class="btn btn-sm" aria-label=${t('language.title')}>${t('language.title')}</summary>
      <div class="language-panel">
        <p class="muted">${t('language.scope')}</p>
        <div class="language-options" role="radiogroup" aria-label=${t('language.title')} aria-busy=${this.saving || this.loading ? 'true' : 'false'}>
          ${OPTIONS.map((value) => html`<label>
            <input type="radio" name="display-language" value=${value} .checked=${live(this.view !== null && this.preference() === value)} ?disabled=${disabled}
              @change=${() => this.choose(value)} />
            <span>${value === 'auto' ? t('language.auto') : value === 'zh-CN' ? t('language.zh-CN') : t('language.en')}</span>
          </label>`)}
        </div>
        ${denied || reason ? html`<p class="muted">${reason || t('language.denied')}</p>` : nothing}
        ${this.loading ? html`<p role="status" class="muted">${t('language.loading')}</p>` : nothing}
        ${this.error ? html`<p role="alert" class="row-error">${this.error}</p>
          <button class="btn btn-sm" ?disabled=${this.loading || this.saving} @click=${() => this.load()}>${t('language.retry')}</button>` : nothing}
      </div>
    </details>`;
  }
}
customElements.define('sm-language-menu', LanguageMenu);
