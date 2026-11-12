import { LocaleController } from '../i18n';
import { t as translate, rich } from '../i18n';
// 页面的公共部分:页头、骨架、空状态、保存条、冲突提示、预览栏布局,以及每页共用的
// 「草稿 / 保存 / 别处改过」流程。
import { html, LitElement, nothing, type TemplateResult } from 'lit';
import type { Method, Params, Returns } from '../api/types';
import { EditSet, type Editable, type SaveSummary } from '../app/editable';
import { clearLeaveGuard, setLeaveGuard, type Route } from '../app/router';
import { store } from '../app/store';
import { confirmDialog } from './dialog';
import { RERENDER_EVENT } from './events';
import { icon } from './icon';
import { toast } from './toast';

export function pageHead(
  title: string | TemplateResult,
  subtitle?: string | TemplateResult,
  actions?: TemplateResult,
  crumb?: { href: string; label: string },
): TemplateResult {
  return html`<header class="page-head">
    <div class="page-head-text">
      ${crumb ? html`<a class="crumb" href=${crumb.href}>${icon('chevronLeft', 'icon-sm')}${crumb.label}</a>` : nothing}
      <h1>${title}</h1>
      ${subtitle ? html`<p>${subtitle}</p>` : nothing}
    </div>
    ${actions ? html`<div class="page-actions">${actions}</div>` : nothing}
  </header>`;
}

export function skeletonCard(rows = 4): TemplateResult {
  return html`<div class="card" aria-busy="true" aria-label="${translate('dd04fcb')}">
    ${Array.from(
      { length: rows },
      (_, i) => html`<div class="sk-row">
        <span class="sk sk-w${i % 3}"></span>
        <span class="sk sk-short"></span>
      </div>`,
    )}
  </div>`;
}

export function emptyState(title: string, body: string | TemplateResult, action?: TemplateResult): TemplateResult {
  return html`<div class="empty">
    <h3>${title}</h3>
    <p>${body}</p>
    ${action ? html`<div class="empty-actions">${action}</div>` : nothing}
  </div>`;
}

export function errorBanner(title: string, message: string, retry?: () => void): TemplateResult {
  return html`<div class="banner banner-error" role="alert">
    ${icon('circleAlert')}
    <div class="banner-body">
      <strong>${title}</strong>
      <p>${message}</p>
      ${retry ? html`<div class="banner-actions"><button class="btn btn-sm" @click=${retry}>${rich('db8784c')}</button></div>` : nothing}
    </div>
  </div>`;
}

/** 手机宽度:预览在这以下默认收起(与 layout.css 的断点一致)。 */
const PHONE: MediaQueryList =
  typeof window !== 'undefined' && window.matchMedia
    ? window.matchMedia('(max-width: 899px)')
    : ({ matches: false, addEventListener() {}, removeEventListener() {} } as unknown as MediaQueryList);
/** 下拉超过这么多才算要展开;展开后至少这么高。 */
const OPEN_PULL_PX = 48;
const MIN_OPEN_PX = 240;

const AFTER_SAVE: Record<string, string> = {
  get immediate() { return translate('d8d544e'); },
  get next_turn() { return translate('de960e7'); },
  get new_session() { return translate('dcf9de4'); },
  get next_poll() { return translate('d36b0e7'); },
  get restart() { return translate('db4c6f0'); },
};

function effectSummary(sum: SaveSummary): string {
  const counts = new Map<string, number>();
  for (const r of sum.results) for (const e of r.effects) counts.set(e.effect, (counts.get(e.effect) ?? 0) + 1);
  return Object.keys(AFTER_SAVE)
    .filter((k) => counts.get(k))
    .map((k) => translate('d6b92fd', [counts.get(k), AFTER_SAVE[k]]))
    .join(' · ');
}

/**
 * 页面基类:光 DOM(样式全局可控)、一组可编辑块、离开前确认、轮询发现别处改动。
 * 子类实现 refresh() 与 render()。
 */
export abstract class PageBase extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = { route: { attribute: false } };
  declare route: Route;

  readonly edits = new EditSet(() => this.onEditsChanged());
  /** 轮询发现别处改过、而本页有草稿的那些版本键 */
  stale: string[] = [];
  /** 从别处链过来要高亮的那一行(几秒后自动淡出) */
  highlight: string | null = null;
  loading = true;
  loadError = '';

  protected override createRenderRoot(): HTMLElement {
    return this;
  }

  readonly call = <M extends Method>(m: M, p: Params<M>): Promise<Returns<M>> => store.call(m, p);

  protected onEditsChanged(): void {
    this.requestUpdate();
  }

  abstract refresh(): Promise<void>;

  /** 本页还关心哪些版本键(列表类数据,不在可编辑块里) */
  protected watchKeys(): string[] {
    return [];
  }

  /** 跳到 next 还算不算同一处编辑(同一页面实例,草稿留着)。 */
  protected sameContext(next: Route): boolean {
    const r = this.route;
    if (!r || next.page !== r.page) return false;
    if (next.page === 'comments' && r.page === 'comments') return next.agent === r.agent;
    if (next.page === 'agent' && r.page === 'agent') return next.alias === r.alias;
    if (next.page === 'chat' && r.page === 'chat') return next.chatId === r.chatId;
    // 表单与原文是两份草稿,换方式等于离开;表单里换一组还是同一份草稿。
    if (next.page === 'global' && r.page === 'global') return next.mode === r.mode;
    return true;
  }

  private readonly guard = async (next: Route): Promise<boolean> => {
    const n = this.edits.dirtyCount();
    if (!n || this.sameContext(next)) return true;
    return confirmDialog({
      title: translate('d535083'),
      body: translate('d1bc96d', [n]),
      confirm: translate('d61145f'),
      cancel: translate('dfd4b9e'),
      danger: true,
    });
  };

  private readonly onRerender = () => this.requestUpdate();

  private readonly onUnload = (e: BeforeUnloadEvent) => {
    if (this.edits.dirtyCount()) {
      e.preventDefault();
      e.returnValue = '';
    }
  };

  private readonly onRevisions = (e: Event) => {
    const keys = (e as CustomEvent<string[]>).detail;
    const mine = new Set([
      ...this.edits.all().map((x) => x.revisionKey()).filter((k): k is string => !!k),
      ...this.watchKeys(),
    ]);
    const hits = keys.filter((k) => mine.has(k) || [...mine].some((m) => m.endsWith('*') && k.startsWith(m.slice(0, -1))));
    if (!hits.length) return;
    if (this.edits.dirtyCount() === 0) {
      void this.refresh().then(() => toast(translate('df7ad96'), 'info'));
    } else {
      this.stale = [...new Set([...this.stale, ...hits])];
      this.requestUpdate();
    }
  };

  protected languageChanged(): void {
    this.requestUpdate();
    if (this.edits.dirtyCount() === 0) void this.refresh();
  }

  private readonly onLanguage = () => this.languageChanged();

  override connectedCallback(): void {
    super.connectedCallback();
    store.addEventListener('revisions', this.onRevisions);
    store.addEventListener('language', this.onLanguage);
    window.addEventListener('beforeunload', this.onUnload);
    this.addEventListener(RERENDER_EVENT, this.onRerender);
    setLeaveGuard(this.guard);
    PHONE.addEventListener('change', this.onPhoneChange);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    store.removeEventListener('revisions', this.onRevisions);
    store.removeEventListener('language', this.onLanguage);
    window.removeEventListener('beforeunload', this.onUnload);
    clearLeaveGuard(this.guard);
    PHONE.removeEventListener('change', this.onPhoneChange);
  }

  /** 保存前的额外确认(如改控制台自己的端口);返回 false 取消。 */
  protected async beforeSave(): Promise<boolean> {
    return true;
  }

  async save(): Promise<void> {
    if (!(await this.beforeSave())) return;
    const sum = await this.edits.saveAll();
    const last = sum.results[sum.results.length - 1];
    if (last) store.setRestartPending(last.restart_pending);
    // 自己保存产生的版本变化先记下,免得下一次轮询把它当成别处的改动。
    await store.refreshStatus(true);
    if (sum.saved) void store.loadIcon();
    const conflicts = sum.failed.filter((f) => 'error' in f && f.error.code === 'conflict');
    const others = sum.failed.filter((f) => 'error' in f && f.error.code !== 'conflict');
    if (sum.saved) {
      const eff = effectSummary(sum);
      toast(eff ? translate('d7be7d6', [eff]) : translate('d1bd91a'), 'ok', 4200);
    }
    if (others.length) {
      const f = others[0]!;
      toast(translate('d33565a', [f.label, 'error' in f ? f.error.message : '']), 'error');
    }
    if (conflicts.length) toast(translate('de034cd'), 'warn');
    this.stale = [];
    this.requestUpdate();
  }

  discard(): void {
    this.edits.reset();
    this.stale = [];
  }

  async reloadKeepingDraft(): Promise<void> {
    try {
      await this.edits.reloadConflicts();
      for (const e of this.edits.all()) {
        const k = e.revisionKey();
        if (k && this.stale.includes(k) && !e.hasConflict()) await e.reload();
      }
      if (this.stale.some((k) => this.watchKeys().some((w) => (w.endsWith('*') ? k.startsWith(w.slice(0, -1)) : w === k)))) {
        await this.refresh();
      }
      this.stale = [];
      toast(translate('d425e0b'), 'info');
    } catch (err) {
      toast(translate('d38c673', [String(err)]), 'error');
    }
    this.requestUpdate();
  }

  protected conflictBanner(): TemplateResult | typeof nothing {
    const conflicts: Editable[] = this.edits.conflicts();
    if (!conflicts.length && !this.stale.length) return nothing;
    const what = conflicts.length ? conflicts.map((c) => c.label).join('、') : translate('dab00ae');
    return html`<div class="banner banner-warn" role="alert">
      ${icon('triangleAlert')}
      <div class="banner-body">
        <strong>${rich('df1832b', [what])}</strong>
        <p>${rich('d4e9421')}</p>
        <div class="banner-actions">
          <button class="btn btn-sm" @click=${() => this.reloadKeepingDraft()}>${rich('d7ef6d0', [icon('refreshCw', 'icon-sm')])}</button>
          <button
            class="btn btn-sm btn-ghost"
            @click=${() => {
              this.discard();
              void this.refresh();
            }}
          >
            ${rich('d704037')}
          </button>
        </div>
      </div>
    </div>`;
  }

  protected saveBar(): TemplateResult | typeof nothing {
    const n = this.edits.dirtyCount();
    if (!n) return nothing;
    const r = this.edits.restartCount();
    return html`<div class="savebar" role="region" aria-label="${translate('d444065')}">
      <p class="savebar-text" aria-live="polite"><strong>${rich('dc3bc19', [n])}</strong>${r ? html` ${rich('da59965', [r])}` : nothing}</p>
      <div class="savebar-actions">
        <button class="btn btn-ghost" ?disabled=${this.edits.saving} @click=${() => this.discard()}>${rich('de7f51b')}</button>
        <button class="btn btn-primary" ?disabled=${this.edits.saving} @click=${() => this.save()}>
          ${this.edits.saving ? translate('dff509c') : translate('da3030b')}
        </button>
      </div>
    </div>`;
  }

  // ── 预览栏 ──

  /** 有预览的页面:区块表到了就刷新一次,设置行上的预览标记才对得上。 */
  protected bindPreview(ctl: { addEventListener(t: string, f: () => void): void }): void {
    ctl.addEventListener('view', () => this.requestUpdate());
  }

  /** 手机上预览展开着没有(默认收起;宽一些的屏幕上预览总是展开的,不看它) */
  previewOpen = false;

  private readonly togglePreview = (open = !this.previewOpen) => {
    this.previewOpen = open;
    this.requestUpdate();
  };

  /** 点预览标题行开合(标题行里的按钮除外)。 */
  private readonly onPreviewClick = (e: Event) => {
    if (!PHONE.matches) return;
    const t = e.target as HTMLElement;
    if (t.closest('.pv-head') && !t.closest('button')) this.togglePreview();
  };

  /** 拖底部把手:跟着手指改高度,松手时往下拉够了就展开(停在拉到的高度),否则收起。 */
  private readonly startGrip = (e: PointerEvent) => {
    const grip = e.currentTarget as HTMLElement;
    const col = grip.closest<HTMLElement>('.preview-col');
    if (!col) return;
    grip.setPointerCapture(e.pointerId);
    const startY = e.clientY;
    const startH = col.getBoundingClientRect().height;
    const closedH = startH - (col.querySelector('.pv-scroll')?.getBoundingClientRect().height ?? 0);
    const maxH = window.innerHeight * 0.8;
    let moved = false;
    const move = (ev: PointerEvent) => {
      const dy = ev.clientY - startY;
      if (Math.abs(dy) > 4) moved = true;
      if (!moved) return;
      col.setAttribute('data-dragging', '');
      col.style.height = `${Math.round(Math.min(maxH, Math.max(closedH, startH + dy)))}px`;
    };
    const up = (ev: PointerEvent) => {
      grip.removeEventListener('pointermove', move);
      grip.removeEventListener('pointerup', up);
      grip.removeEventListener('pointercancel', up);
      col.removeAttribute('data-dragging');
      const h = Math.min(maxH, Math.max(closedH, startH + ev.clientY - startY));
      col.style.removeProperty('height');
      if (!moved) {
        this.togglePreview();
        return;
      }
      const open = h - closedH > OPEN_PULL_PX;
      if (open) col.style.setProperty('--pv-open-h', `${Math.round(Math.max(MIN_OPEN_PX, h))}px`);
      else col.style.removeProperty('--pv-open-h');
      this.togglePreview(open);
    };
    grip.addEventListener('pointermove', move);
    grip.addEventListener('pointerup', up);
    grip.addEventListener('pointercancel', up);
  };

  private readonly onPhoneChange = () => this.requestUpdate();

  /**
   * 有预览的页面分页头、预览、表单三块:宽屏预览在右侧吸顶;窄了排在页头下面、吸在顶栏下方。
   * 手机上它默认收成一条标题,下拉展开、上拉收起,不占着看设置的地方。
   */
  protected withPreview(head: TemplateResult, body: TemplateResult, panel: TemplateResult): TemplateResult {
    const folded = !this.previewOpen;
    return html`<div class="with-preview">
      <div class="wp-head">${head}</div>
      <aside class="preview-col" aria-label="${translate('d23d081')}" ?data-folded=${folded} @click=${this.onPreviewClick}>
        ${panel}
        <div
          class="pv-grip"
          role="button"
          tabindex="0"
          aria-expanded=${folded ? 'false' : 'true'}
          aria-label=${folded ? translate('d1ba5f5') : translate('d72f937')}
          @pointerdown=${this.startGrip}
          @keydown=${(e: KeyboardEvent) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              this.togglePreview();
            }
          }}
        >
          <span class="pv-grip-bar" aria-hidden="true"></span>
          <span class="pv-grip-hint">${folded ? translate('d2b8ef2') : translate('d43fefd')}</span>
        </div>
      </aside>
      <div class="wp-body">${body}</div>
    </div>`;
  }

  /** 从概览「需要处理」跳来时,滚到那一行并短暂高亮。 */
  protected focusTarget(key: string | undefined): void {
    if (!key) return;
    this.highlight = key;
    void this.updateComplete.then(() => {
      document.getElementById(`f-${key}`)?.scrollIntoView({ block: 'center', behavior: 'smooth' });
      setTimeout(() => {
        if (this.highlight === key) this.highlight = null;
        this.requestUpdate();
      }, 2400);
    });
  }
}
