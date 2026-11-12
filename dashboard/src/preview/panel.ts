import { LocaleController } from '../i18n';
import { t as translate, rich } from '../i18n';
import { html, LitElement, nothing, type PropertyValues, type TemplateResult } from 'lit';
import type { PreviewMessage } from '../api/types';
import { icon } from '../components/icon';
import { renderCot, type CotLive } from './cot';
import type { PreviewController } from './controller';
import { emojiFace } from './emoji-face';
import { renderCard, type Mark } from './feishu-card';
import { renderInline } from './markdown';

const REPLAY_STEP_MS = 650;

function initial(name: string): string {
  return [...name.trim()][0] ?? '?';
}

function inlineNode(text: string): DocumentFragment {
  const frag = document.createDocumentFragment();
  renderInline(text, frag);
  return frag;
}

/** 用户消息里的 @名字 画成飞书的蓝色提及。 */
function userText(text: string): Array<string | TemplateResult> {
  const out: Array<string | TemplateResult> = [];
  const re = /@[^\s@]+/g;
  let last = 0;
  for (const m of text.matchAll(re)) {
    if (m.index > last) out.push(text.slice(last, m.index));
    out.push(html`<span class="fs-at">${m[0]}</span>`);
    last = m.index + m[0].length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

export class PreviewPanel extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = {
    ctl: { attribute: false },
    botName: { attribute: false },
  };

  declare ctl: PreviewController | null;
  declare botName: string;

  constructor() {
    super();
    this.ctl = null;
    this.botName = 'agent';
  }

  protected override createRenderRoot(): HTMLElement {
    return this;
  }

  private readonly onCtl = () => this.requestUpdate();
  private bound: PreviewController | null = null;
  private revealed: Set<string> | null = null;
  private readonly cotFold = {
    collapsed: false,
    toggle: () => {
      this.cotFold.collapsed = !this.cotFold.collapsed;
      this.requestUpdate();
    },
  };
  /** 回放这一轮:过程逐条出现、标题扫光,跑完才出回复卡与会话列表那一行。 */
  private live: CotLive | null = null;
  private liveTimer: ReturnType<typeof setTimeout> | undefined;
  private played = false;
  /** 预览此刻露没露出来:手机上默认收起,回放等到展开那一刻才跑。 */
  private shown = typeof IntersectionObserver === 'undefined';
  private observer: IntersectionObserver | null = null;

  private replay(): void {
    clearTimeout(this.liveTimer);
    const cot = this.ctl?.view?.messages.find((m) => m.kind === 'cot');
    if (!cot || cot.kind !== 'cot') return;
    const total = cot.items.length;
    this.live = { shown: 0 };
    const step = () => {
      if (!this.live) return;
      if (this.live.shown < total) {
        this.live = { shown: this.live.shown + 1 };
        this.liveTimer = setTimeout(step, REPLAY_STEP_MS);
      } else {
        this.live = null;
      }
      this.requestUpdate();
    };
    this.liveTimer = setTimeout(step, REPLAY_STEP_MS);
    this.requestUpdate();
  }

  protected override willUpdate(changed: PropertyValues<this>): void {
    if (changed.has('ctl') && this.bound !== this.ctl) {
      this.bound?.removeEventListener('change', this.onCtl);
      this.ctl?.addEventListener('change', this.onCtl);
      this.bound = this.ctl;
    }
    this.autoplay();
  }

  /** 预览第一次露出来、画面也到了时回放一遍;之后拨开关只原地换成新的终态。 */
  private autoplay(): void {
    if (this.played || !this.shown || !this.ctl?.view) return;
    this.played = true;
    if (!window.matchMedia('(prefers-reduced-motion: reduce)').matches) this.replay();
  }

  protected override firstUpdated(): void {
    if (typeof IntersectionObserver === 'undefined') return;
    this.observer = new IntersectionObserver((entries) => {
      if (!entries.some((e) => e.isIntersecting)) return;
      this.shown = true;
      this.observer?.disconnect();
      this.observer = null;
      this.autoplay();
    });
    const box = this.querySelector('.pv-scroll');
    if (box) this.observer.observe(box);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.bound?.removeEventListener('change', this.onCtl);
    this.bound = null;
    clearTimeout(this.liveTimer);
    this.live = null;
    this.observer?.disconnect();
    this.observer = null;
  }

  /**
   * 拨了开关、有区块变了:把第一块滚进预览的可见范围。窄屏上预览只占小半屏,不滚的话变化常常
   * 发生在看不见的地方。只滚预览自己的滚动区,不动页面。
   */
  protected override updated(): void {
    const changed = this.ctl?.changed;
    if (!changed?.size || changed === this.revealed) return;
    this.revealed = changed;
    const box = this.querySelector<HTMLElement>('.pv-scroll');
    const el = box?.querySelector<HTMLElement>('.is-changed');
    if (!box || !el) return;
    const b = box.getBoundingClientRect();
    const e = el.getBoundingClientRect();
    if (e.top >= b.top && e.bottom <= b.bottom) return;
    const smooth = !window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    box.scrollBy({ top: e.top - b.top - 12, behavior: smooth ? 'smooth' : 'auto' });
  }

  private renderMessages(messages: PreviewMessage[], mark: Mark): TemplateResult[] {
    const out: TemplateResult[] = [];
    let bot: TemplateResult[] = [];
    const flushBot = () => {
      if (!bot.length) return;
      out.push(html`<div class="fs-msg">
        <span class="fs-avatar bot" aria-hidden="true">${initial(this.botName)}</span>
        <div class="fs-msg-main">
          <div class="fs-name">${this.botName}<span class="fs-bot-tag">${rich('da4d144')}</span><time>14:32</time></div>
          ${bot}
        </div>
      </div>`);
      bot = [];
    };
    for (const m of messages) {
      switch (m.kind) {
        case 'user':
          flushBot();
          out.push(html`<div class="fs-msg">
            <span class="fs-avatar" aria-hidden="true">${initial(m.sender)}</span>
            <div class="fs-msg-main ${mark(m.id)}" data-eid=${m.id}>
              <div class="fs-name">${m.sender}<time>14:31</time></div>
              <div class="fs-bubble">${userText(m.text)}</div>
              ${m.reactions.length
                ? html`<div class="fs-reactions">
                    ${m.reactions.map(
                      (r) => html`<span class="fs-reaction" title=${r}>${emojiFace(r, r)}${this.botName}</span>`,
                    )}
                  </div>`
                : nothing}
            </div>
          </div>`);
          break;
        case 'cot':
          bot.push(renderCot(m, mark, this.cotFold, this.live));
          break;
        case 'card':
          if (this.live) break;
          bot.push(html`<div class="fs-card-wrap ${mark(m.id)}" data-eid=${m.id}>${renderCard(m.card, mark)}</div>`);
          break;
        case 'feed':
          break;
      }
    }
    flushBot();
    return out;
  }

  private renderFeed(messages: PreviewMessage[], mark: Mark): TemplateResult | typeof nothing {
    const feed = messages.find((m): m is Extract<PreviewMessage, { kind: 'feed' }> => m.kind === 'feed');
    if (!feed || this.live) return nothing;
    return html`<div class="fs-feed-label">${rich('dc980de')}</div>
      <div class="fs-feed ${mark(feed.id)}" data-eid=${feed.id}>
        <span class="fs-avatar group" aria-hidden="true">${initial(feed.chat_name)}</span>
        <div>
          <div class="fs-feed-top"><strong>${feed.chat_name}</strong><time>14:32</time></div>
          <div class="fs-feed-text">${inlineNode(feed.text)}</div>
        </div>
      </div>`;
  }

  protected override render(): TemplateResult {
    const ctl = this.ctl;
    const view = ctl?.view;
    const mark = ctl?.mark ?? (() => '');
    return html`<section class="pv" aria-label="${translate('d23d081')}">
      <header class="pv-head">
        <div class="pv-head-text">
          <h2>${rich('dae9f6a')}</h2>
          <p>${view?.title ?? translate('ded25f6')}<span class="pv-note"> ${rich('decf66c')}</span></p>
        </div>
        <button class="icon-btn" title="${translate('ddc4db9')}" aria-label="${translate('ddc4db9')}" ?disabled=${!view} @click=${() => this.replay()}>
          ${icon('rotateCcw', 'icon-sm')}
        </button>
      </header>
      <div class="pv-scroll">
        ${view
          ? html`<div class="fs">
              <div class="fs-chat">${this.renderMessages(view.messages, mark)}</div>
              ${this.renderFeed(view.messages, mark)}
            </div>`
          : ctl?.error
            ? html`<p class="pv-status">${rich('de042e1', [ctl.error])}</p>`
            : html`<div class="sk-row"><span class="sk"></span><span class="sk"></span></div>`}
      </div>
      <p class="pv-foot">${rich('d857433')}</p>
    </section>`;
  }
}

customElements.define('sm-preview', PreviewPanel);
