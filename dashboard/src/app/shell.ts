import '../components/language-menu';
import { LocaleController } from '../i18n';
import { t as translate, rich } from '../i18n';
import { html, LitElement, nothing, type TemplateResult } from 'lit';
import { icon } from '../components/icon';
import { applyFavicon, brandIcon } from './brand';
import { fmtDateTime, fmtUptime } from '../util/format';
import { fileLabel } from '../util/labels';
import { GLOBAL_DIRTY_EVENT, globalGroupOf, globalSections, NAV_GROUPS, PAGE_TITLES, PAGES } from './nav';
import { currentRoute, hrefFor, interceptLinks, navIdFor, ROUTE_EVENT, type Route } from './router';
import { store } from './store';
import { applyTheme, loadTheme, saveTheme, type ThemePref } from './theme';

const THEMES: Array<{ value: ThemePref; label: string; icon: 'monitor' | 'sun' | 'moon' }> = [
  { value: 'system', get label() { return translate('d217cfe'); }, icon: 'monitor' },
  { value: 'light', get label() { return translate('daa0819'); }, icon: 'sun' },
  { value: 'dark', get label() { return translate('da6b75d'); }, icon: 'moon' },
];

export class App extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = {
    route: { state: true },
    drawer: { state: true },
    theme: { state: true },
  };

  declare route: Route;
  declare drawer: boolean;
  declare theme: ThemePref;
  private pageEl: HTMLElement | null = null;
  /** 全局设置里有未保存改动的那几组 */
  private globalDirty = new Set<string>();
  private pageKey = '';
  private loadSeq = 0;

  constructor() {
    super();
    this.route = currentRoute();
    this.drawer = false;
    this.theme = loadTheme();
    applyTheme(this.theme);
  }

  protected override createRenderRoot(): HTMLElement {
    return this;
  }

  private readonly onStore = () => this.requestUpdate();
  private readonly onRoute = () => {
    this.route = currentRoute();
    this.drawer = false;
    window.scrollTo({ top: 0 });
  };
  private readonly onGlobalDirty = (e: Event) => {
    this.globalDirty = new Set((e as CustomEvent<string[]>).detail);
    this.requestUpdate();
  };
  private readonly onKey = (e: KeyboardEvent) => {
    if (e.key === 'Escape' && this.drawer) this.closeDrawer();
  };

  /** 抽屉开合时把焦点带进去、再还给菜单按钮,键盘用户不至于停在被遮住的页面上。 */
  private openDrawer() {
    this.drawer = true;
    void this.updateComplete.then(() => (this.querySelector<HTMLElement>('.drawer .nav-item[aria-current="page"]') ?? this.querySelector<HTMLElement>('.drawer .nav-item'))?.focus());
  }

  private closeDrawer() {
    if (!this.drawer) return;
    this.drawer = false;
    void this.updateComplete.then(() => this.querySelector<HTMLElement>('.nav-open')?.focus());
  }

  override connectedCallback(): void {
    super.connectedCallback();
    store.addEventListener('change', this.onStore);
    window.addEventListener(ROUTE_EVENT, this.onRoute);
    window.addEventListener('keydown', this.onKey);
    window.addEventListener(GLOBAL_DIRTY_EVENT, this.onGlobalDirty);
    interceptLinks(this);
  }

  protected override updated(): void {
    document.title = `summon · ${PAGE_TITLES[this.route.page]}`;
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    store.removeEventListener('change', this.onStore);
    window.removeEventListener(ROUTE_EVENT, this.onRoute);
    window.removeEventListener('keydown', this.onKey);
    window.removeEventListener(GLOBAL_DIRTY_EVENT, this.onGlobalDirty);
  }

  private setTheme(t: ThemePref) {
    this.theme = t;
    saveTheme(t);
  }

  /** 页面模块按路由懒加载;同一页面只换 route 属性,不重建元素(保住草稿)。 */
  private page(): TemplateResult | HTMLElement {
    const r = this.route;
    if (r.page === 'not_found') {
      return html`<div class="page">
        <div class="empty">
          <h3>${rich('de75737')}</h3>
          <p>${rich('d37aac5')} <code>${r.path}</code> ${rich('d9e695a')}</p>
          <div class="empty-actions"><a class="btn" href="/">${rich('d21d1bb')}</a></div>
        </div>
      </div>`;
    }
    const mod = PAGES[r.page];
    const key =
      r.page === 'agent'
        ? `agent:${r.alias}`
        : r.page === 'chat'
          ? `chat:${r.chatId}`
          : r.page === 'session'
            ? `session:${r.agent}:${r.chatId}`
            : r.page;
    if (this.pageKey !== key || !this.pageEl) {
      this.pageKey = key;
      const seq = ++this.loadSeq;
      if (customElements.get(mod.tag)) {
        this.pageEl = document.createElement(mod.tag);
      } else {
        this.pageEl = null;
        void mod.load().then(() => {
          if (seq === this.loadSeq) {
            this.pageEl = document.createElement(mod.tag);
            (this.pageEl as unknown as { route: Route }).route = this.route;
            this.requestUpdate();
          }
        });
      }
    }
    if (!this.pageEl) return html`<div class="page" aria-busy="true"></div>`;
    (this.pageEl as unknown as { route: Route }).route = r;
    return this.pageEl;
  }

  private nav(inDrawer: boolean): TemplateResult {
    const active = navIdFor(this.route);
    return html`<nav class=${inDrawer ? 'nav-in-drawer' : 'nav'} aria-label="${translate('d9bae68')}">
      ${NAV_GROUPS.map(
        (g) => html`<div class="nav-group">
          <div class="nav-group-label">${g.label}</div>
          ${g.items
            .filter((it) => !it.machine || store.state.whoami?.machine)
            .map((it) =>
            it.id === 'global' && active === 'global'
              ? html`<a class="nav-item" href=${it.href} aria-current="true">${icon(it.icon)}<span>${it.label}</span></a>
                  ${this.globalSubnav()}`
              : html`<a class="nav-item" href=${it.href} aria-current=${it.id === active ? 'page' : nothing}>
                  ${icon(it.icon)}<span>${it.label}</span>
                </a>`,
          )}
        </div>`,
      )}
    </nav>`;
  }

  /** 全局设置的二级菜单,只在全局设置页展开;原文模式下不点亮哪一组。 */
  private globalSubnav(): TemplateResult {
    const r = this.route;
    const schema = store.state.schema;
    const current = r.page === 'global' && r.mode === 'form' ? globalGroupOf(r, schema) : null;
    return html`<div class="nav-sub" role="group" aria-label="${translate('da5c037')}">
      ${globalSections(schema).map(
        (s) => html`<a
          class="nav-subitem"
          href=${hrefFor({ page: 'global', mode: 'form', group: s.id })}
          aria-current=${s.id === current ? 'page' : nothing}
        >
          <span>${s.label}</span>${this.globalDirty.has(s.id)
            ? html`<span class="nav-dirty" title="${translate('d535083')}"><span class="sr-only">${rich('d535083')}</span></span>`
            : nothing}
        </a>`,
      )}
    </div>`;
  }

  private identity(): TemplateResult {
    const id = store.state.whoami?.identity;
    if (!id) return html``;
    const exp = store.state.whoami?.expires_at;
    const until = exp ? translate('ddbbe72', [fmtDateTime(exp)]) : '';
    if (id.kind === 'owner') return html`<span class="identity" title=${translate('dd3e20e', [until])}>${rich('db862f2', [icon('monitor', 'icon-sm')])}</span>`;
    const agent = store.state.status?.agents.find((a) => a.alias === id.agent);
    return html`<span class="identity" title=${translate('d3b66a3', [agent?.display || id.agent, id.chat_kind === 'p2p' ? translate('d6add27') : translate('d35b49e'), until])}
      >${id.name}<span class="identity-via">${rich('dd746d7', [agent?.display || id.agent])}</span></span
    >`;
  }

  private themeSwitch(): TemplateResult {
    return html`<div class="seg seg-sm seg-icon" role="radiogroup" aria-label="${translate('d788db1')}">
      ${THEMES.map(
        (t) => html`<label class="seg-opt" title=${t.label}>
          <input type="radio" name="theme" .checked=${this.theme === t.value} aria-label=${t.label} @change=${() => this.setTheme(t.value)} />
          <span>${icon(t.icon, 'icon-sm')}</span>
        </label>`,
      )}
    </div>`;
  }

  private statusPill(): TemplateResult {
    const st = store.state.status;
    if (!st) return html``;
    const restart = st.restart_pending.length;
    return html`${restart
        ? html`<a class="pill pill-warn" href="/" title=${st.restart_pending.map((r) => `${r.key} · ${fileLabel(r.file)}`).join('\n')}>
            ${rich('daca962', [icon('refreshCw', 'icon-sm'), restart])}
          </a>`
        : nothing}
      <span class="pill" title=${translate('d9a030b', [st.daemon.pid, st.daemon.commit])}>
        <span class="dot ok"></span>${rich('d1f0eb9')}<span class="mono">v${st.daemon.version}</span><span class="muted">${fmtUptime(st.daemon.started_at)}</span>
      </span>`;
  }

  private renderNotice(): TemplateResult {
    const { auth, bootError } = store.state;
    if (auth === 'loading') {
      return html`<div class="centered" aria-busy="true"><div class="notice">${brandIcon(store.state.icon)}<p class="muted">${rich('dfcfa09')}</p></div></div>`;
    }
    if (auth === 'error') {
      return html`<div class="centered">
        <div class="notice">
          ${brandIcon(store.state.icon)}
          <h1>${rich('d5de14d')}</h1>
          <p>${bootError}</p>
          <p class="hint"><button class="btn" @click=${() => store.boot()}>${rich('db8784c')}</button></p>
        </div>
      </div>`;
    }
    return html`<div class="centered" data-testid="reopen">
      <div class="notice">
        ${brandIcon(store.state.icon)}
        <h1>${rich('d075903')}</h1>
        <p>${rich('d4d86ad')}</p>
        <p class="hint">${rich('de76c9f')} <code>/dash</code>${rich('de6c7ff')} <code>summon dashboard open</code>。</p>
      </div>
    </div>`;
  }

  protected override render(): TemplateResult {
    const { auth, whoami, status } = store.state;
    applyFavicon(store.state.icon);
    if (auth !== 'ok') return this.renderNotice();
    const r = this.route;
    const title =
      r.page === 'agent' ? (status?.agents.find((a) => a.alias === r.alias)?.display || r.alias) : PAGE_TITLES[r.page];
    const host = status?.hostname ?? whoami?.hostname ?? '';
    return html`<div class="shell" ?data-drawer=${this.drawer}>
      <header class="topbar">
        <button class="icon-btn only-narrow nav-open" aria-label="${translate('d38e4f2')}" aria-expanded=${this.drawer ? 'true' : 'false'} @click=${() => this.openDrawer()}>
          ${icon('menu', 'icon-lg')}
        </button>
        <a class="brand" href="/" aria-label="${translate('dd50bb0')}">
          ${brandIcon(store.state.icon)}<span class="brand-name">summon</span><span class="brand-sub">${rich('dd0d07d')}</span>${host
            ? html`<span class="brand-host">· ${host}</span>`
            : nothing}
        </a>
        <div class="topbar-title">${title}</div>
        <div class="topbar-right">
          ${this.statusPill()}
          <span class="topbar-sep" aria-hidden="true"></span>
          ${this.identity()} ${this.themeSwitch()}
          <sm-language-menu></sm-language-menu>
          <span class="status-dot-only" title=${status?.restart_pending.length ? translate('ddb8dcb') : translate('d12dd37')}>
            <span class="dot ${status?.restart_pending.length ? 'warn' : 'ok'}"></span>
          </span>
        </div>
      </header>
      ${this.nav(false)}
      <main class="content" id="main">${this.page()}</main>
      <div class="drawer-scrim" @click=${() => this.closeDrawer()}></div>
      <aside class="drawer" aria-label="${translate('de72622')}" ?inert=${!this.drawer}>
        <div class="drawer-head">
          <span class="brand">${brandIcon(store.state.icon)}<span class="brand-name">summon</span><span class="brand-sub">${rich('dd0d07d')}</span></span>
          <button class="icon-btn" aria-label="${translate('dbaf9f5')}" @click=${() => this.closeDrawer()}>${icon('x', 'icon-lg')}</button>
        </div>
        ${this.nav(true)}
        <div class="drawer-foot">
          ${status
            ? html`<div class="identity"><span class="dot ok"></span>${rich('d122bf1', [status.daemon.version, host])}</div>`
            : nothing}
          ${status?.restart_pending.length
            ? html`<div class="identity"><span class="dot warn"></span>${rich('d33209d', [status.restart_pending.length])}</div>`
            : nothing}
          <div>${this.identity()}</div>
          <div class="drawer-row">${this.themeSwitch()}</div>
        </div>
      </aside>
    </div>`;
  }
}

customElements.define('sm-app', App);
