import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { live } from 'lit/directives/live.js';
import { errorMessage } from '../api/client';
import type { ChatSummary } from '../api/types';
import { hrefFor } from '../app/router';
import { store } from '../app/store';
import { icon } from '../components/icon';
import { ident } from '../components/ident';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { fmtRelative } from '../util/format';
import { chatInitial, chatTitle, hasChatName } from '../util/labels';

export function overrideCount(c: ChatSummary): number {
  return c.agents.reduce((n, a) => n + a.overrides, 0);
}

class ChatsPage extends PageBase {
  chats: ChatSummary[] = [];
  query = '';
  onlyOverrides = false;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  protected override watchKeys(): string[] {
    return ['chat:*'];
  }

  async refresh(): Promise<void> {
    try {
      const list = await this.call('chats.list', {});
      this.chats = list.sort((a, b) => (b.last_active ?? '').localeCompare(a.last_active ?? ''));
      this.loadError = '';
    } catch (err) {
      this.loadError = errorMessage(err);
    }
    this.loading = false;
    this.requestUpdate();
  }

  private display(alias: string): string {
    return store.state.status?.agents.find((a) => a.alias === alias)?.display || alias;
  }

  private filtered(): ChatSummary[] {
    const q = this.query.trim().toLowerCase();
    return this.chats.filter((c) => {
      if (this.onlyOverrides && !overrideCount(c)) return false;
      if (!q) return true;
      const hay = [chatTitle(c), c.chat_id, ...c.agents.flatMap((a) => [a.agent, this.display(a.agent)])].join(' ').toLowerCase();
      return hay.includes(q);
    });
  }

  private row(c: ChatSummary): TemplateResult {
    const n = overrideCount(c);
    const title = chatTitle(c);
    return html`<a class="chat-row" href=${hrefFor({ page: 'chat', chatId: c.chat_id })}>
      <div class="chat-name">
        <span class="avatar" aria-hidden="true">${chatInitial(c)}</span>
        <div class="chat-name-text">
          <strong>${hasChatName(c) || c.kind === 'p2p' ? title : ident(c.chat_id, { agent: c.agents[0]?.agent ?? '' })}</strong>
          ${hasChatName(c) ? html`<span class="mono">${c.chat_id}</span>` : nothing}
        </div>
      </div>
      <div class="agent-tags">
        ${c.agents.map(
          (a) => html`<span class="agent-tag" title=${a.live ? translate('d7d817d') : translate('dc1f158')}
            ><span class="dot ${a.live ? 'ok' : ''}"></span>${this.display(a.agent)}</span
          >`,
        )}
      </div>
      <span class="cell-muted cell-overrides count">${n ? translate('d8af892', [n]) : '—'}</span>
      <span class="cell-muted cell-active">${fmtRelative(c.last_active)}</span>
      <span class="chat-meta-narrow">${n ? translate('ded467e', [n]) : translate('d27e364')} · ${fmtRelative(c.last_active)}</span>
      ${icon('chevronRight', 'icon-sm')}
    </a>`;
  }

  protected override render(): TemplateResult {
    const list = this.filtered();
    return html`<div class="page">
      ${pageHead(translate('d35b49e'), translate('d1088a7'))}
      ${this.loadError ? errorBanner(translate('d2ac9a4'), this.loadError, () => this.refresh()) : nothing}
      <div class="toolbar">
        <label class="search">
          ${icon('search')}
          <input
            class="field-input"
            type="search"
            placeholder="${translate('dc3f81b')}"
            aria-label="${translate('d890042')}"
            .value=${live(this.query)}
            @input=${(e: Event) => {
              this.query = (e.target as HTMLInputElement).value;
              this.requestUpdate();
            }}
          />
        </label>
        <label class="check">
          <input
            type="checkbox"
            .checked=${this.onlyOverrides}
            @change=${(e: Event) => {
              this.onlyOverrides = (e.target as HTMLInputElement).checked;
              this.requestUpdate();
            }}
          />${rich('d5fb2f7')}
        </label>
      </div>
      ${this.loading
        ? skeletonCard(5)
        : !this.chats.length
          ? html`<div class="card">${emptyState(translate('d437c79'), translate('d9f6e64'))}</div>`
          : !list.length
            ? html`<div class="card">
                ${emptyState(
                  translate('de2c9f9'),
                  this.onlyOverrides ? translate('d7aeac8') : translate('d5a7c1c'),
                )}
              </div>`
            : html`<div class="card">
                <div class="chat-list-head" aria-hidden="true">
                  <span>${rich('d35b49e')}</span><span>${rich('dc5bccb')}</span><span>${rich('df2330c')}</span><span>${rich('dc1d6dc')}</span><span></span>
                </div>
                ${list.map((c) => this.row(c))}
              </div>`}
    </div>`;
  }
}

customElements.define('sm-page-chats', ChatsPage);
