import { language, LocaleController, t as translate } from '../i18n';
// 洞察页的「分析」:点了才由主 agent 对当前时间段的统计做一次分析,与规则结论分开展示。
// 状态在服务端(按时间段、agent 筛选、语言存取);这里只负责取、发起与在运行中时轮询。
import { html, LitElement, nothing, type TemplateResult } from 'lit';
import { ApiCallError, errorMessage } from '../api/client';
import type { AnalysisResult, AnalysisView, InsightsAnalysisQuery, InsightsQuery, Method, Params, PrimarySource, Returns } from '../api/types';
import { hrefFor } from '../app/router';
import { store } from '../app/store';
import { fmtCount, fmtDateTime, fmtElapsed } from '../util/format';
import { renderAnalysis } from './analysis-text';
import { icon } from './icon';
import { errorBanner, skeletonCard } from './page';
import { fmtSpan } from './turn';

/** 运行中时多久查一次。 */
export const ANALYSIS_POLL_MS = 3000;

const SOURCE_LABEL: Record<PrimarySource, string> = {
  get configured() { return translate('analysis.source.configured'); },
  get usage() { return translate('analysis.source.usage'); },
  get first() { return translate('analysis.source.first'); },
};

const SETTINGS_HREF = hrefFor({ page: 'global', mode: 'form', group: 'insights', key: 'primary_agent' });

function agentDisplay(alias: string): string {
  return store.state.status?.agents.find((a) => a.alias === alias)?.display || alias;
}

function note(text: string, warn = false): TemplateResult {
  return html`<p class="row-note analysis-note ${warn ? 'is-warn' : ''}">${icon(warn ? 'triangleAlert' : 'info', 'icon-sm')}<span>${text}</span></p>`;
}

export class InsightsAnalysis extends LitElement {
  static override properties = { query: { attribute: false } };
  declare query: InsightsQuery;
  readonly locale = new LocaleController(this);
  view: AnalysisView | null = null;
  loadError = '';
  /** 发起没成功的原因(含另一项分析正在进行),就地显示,不改当前状态 */
  notice = '';
  starting = false;
  /** 查询条件(时间段、筛选、语言)每变一次加一;早发的请求晚到时按它丢弃 */
  private generation = 0;
  private key = '';
  /** 正在路上的 get 属于哪一代;同一代不重复发 */
  private pending: number | null = null;
  private timer: ReturnType<typeof setTimeout> | undefined;

  protected override createRenderRoot(): HTMLElement {
    return this;
  }

  readonly call = <M extends Method>(m: M, p: Params<M>): Promise<Returns<M>> => store.call(m, p);

  params(): InsightsAnalysisQuery {
    return { ...this.query, lang: language() };
  }

  private readonly onVisibility = () => {
    if (document.hidden) this.stopPolling();
    else void this.fetchView();
  };

  override connectedCallback(): void {
    super.connectedCallback();
    document.addEventListener('visibilitychange', this.onVisibility);
    // 离开后再回来:当作换了查询条件,重新取一次。
    this.key = '';
    this.requestUpdate();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    document.removeEventListener('visibilitychange', this.onVisibility);
    this.stopPolling();
    this.generation += 1;
  }

  protected override willUpdate(): void {
    if (!this.query) return;
    const key = JSON.stringify(this.params());
    if (key === this.key) return;
    this.key = key;
    this.generation += 1;
    this.view = null;
    this.loadError = '';
    this.notice = '';
    this.stopPolling();
    void this.fetchView();
  }

  private stopPolling(): void {
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = undefined;
  }

  /** 运行中、页面可见、还在页面上时,过一会儿再查一次;上一次查完才排下一次。 */
  private schedule(): void {
    this.stopPolling();
    if (this.view?.status !== 'running' || !this.isConnected || document.hidden) return;
    this.timer = setTimeout(() => {
      this.timer = undefined;
      void this.fetchView();
    }, ANALYSIS_POLL_MS);
  }

  async fetchView(): Promise<void> {
    const gen = this.generation;
    if (this.pending === gen || !this.query) return;
    this.pending = gen;
    this.stopPolling();
    let view: AnalysisView | null = null;
    let error = '';
    try {
      view = await this.call('insights.analysis.get', this.params());
    } catch (err) {
      error = errorMessage(err);
    }
    if (this.pending === gen) this.pending = null;
    if (gen !== this.generation || !this.isConnected) return;
    if (view) this.view = view;
    this.loadError = error;
    this.requestUpdate();
    this.schedule();
  }

  async start(): Promise<void> {
    if (this.starting || !this.view) return;
    const gen = this.generation;
    // 已有结果时是「重新生成」:没过期的结果不带 force 不会重跑。
    const force = this.view.status === 'done';
    this.starting = true;
    this.notice = '';
    this.requestUpdate();
    try {
      const view = await this.call('insights.analysis.start', { ...this.params(), ...(force ? { force: true } : {}) });
      if (gen !== this.generation) return;
      this.view = view;
      this.loadError = '';
    } catch (err) {
      if (gen !== this.generation) return;
      this.notice = errorMessage(err);
      // 判权变了(如名单被改):重新取一次,按钮状态跟着服务端走。
      if (err instanceof ApiCallError && err.code === 'forbidden') void this.fetchView();
    } finally {
      this.starting = false;
      this.requestUpdate();
    }
    this.schedule();
  }

  private primaryLine(v: AnalysisView): TemplateResult | typeof nothing {
    const p = v.primary;
    if (!p) return nothing;
    const detail = [SOURCE_LABEL[p.source], p.cli, p.model].filter(Boolean).join(' · ');
    return html`<div class="analysis-primary">
      <span class="analysis-primary-text">
        <span class="muted">${translate('analysis.primary')}</span>
        <strong>${p.label}</strong>
        <span class="muted">${detail}</span>
      </span>
      <a class="link-btn" href=${SETTINGS_HREF}>${translate('analysis.change')}${icon('chevronRight', 'icon-sm')}</a>
    </div>`;
  }

  private meta(r: AnalysisResult): TemplateResult {
    const items = [
      translate('analysis.generatedAt', [fmtDateTime(r.generated_at)]),
      agentDisplay(r.agent),
      r.model,
      translate('analysis.elapsed', [fmtSpan(Math.round(r.elapsed_ms / 1000))]),
      r.usage ? translate('analysis.tokens', [fmtCount(r.usage.input), fmtCount(r.usage.output)]) : undefined,
    ].filter((x): x is string => !!x);
    return html`<p class="analysis-meta">${items.join(' · ')}</p>`;
  }

  private state(v: AnalysisView): TemplateResult | typeof nothing {
    switch (v.status) {
      case 'none':
        return html`<p class="analysis-empty">${translate('analysis.none')}</p>`;
      case 'running':
        return html`<p class="analysis-running" role="status">
          ${icon('loaderCircle', 'icon-sm fs-spin')}<span>${translate('analysis.running', [v.started_at ? fmtElapsed(v.started_at) : '—'])}</span>
        </p>`;
      case 'failed':
        return note(translate('analysis.failed', [v.reason ?? '']), true);
      case 'unsupported':
        return note(v.reason ?? '', true);
      case 'done': {
        const r = v.result;
        if (!r) return nothing;
        return html`${r.retry_failure ? note(translate('analysis.retryFailed', [r.retry_failure.reason]), true) : nothing}
          ${r.stale ? note(translate('analysis.stale')) : nothing}
          ${renderAnalysis(r.text)}
          ${r.truncated ? note(translate('analysis.truncated')) : nothing}
          ${this.meta(r)}`;
      }
    }
  }

  private actions(v: AnalysisView): TemplateResult | typeof nothing {
    if (v.status === 'running' || v.status === 'unsupported') return nothing;
    const label = v.status === 'done' ? translate('analysis.regenerate') : translate('analysis.generate');
    return html`<div class="analysis-actions">
      <button class="btn ${v.status === 'done' ? '' : 'btn-primary'}" ?disabled=${!v.can_start || this.starting} @click=${() => this.start()}>
        ${this.starting ? icon('loaderCircle', 'icon-sm fs-spin') : icon('refreshCw', 'icon-sm')}${label}
      </button>
      ${!v.can_start ? html`<span class="muted">${translate('analysis.denied')}</span>` : nothing}
      ${this.notice ? html`<span class="analysis-notice" role="alert">${icon('info', 'icon-sm')}${this.notice}</span>` : nothing}
    </div>`;
  }

  protected override render(): TemplateResult {
    const v = this.view;
    // 外层的 page-section 是元素自己(页面上与相邻各节同级排版)。
    return html`<div class="group-head">
        <div>
          <h2 id="in-analysis">${translate('analysis.title')}</h2>
          <p>${translate('analysis.subtitle')}</p>
        </div>
      </div>
      ${v
        ? html`<div class="card card-pad analysis" data-status=${v.status}>
            ${this.primaryLine(v)} ${this.state(v)}
            ${this.loadError ? note(translate('analysis.refreshFailed', [this.loadError]), true) : nothing}
            ${this.actions(v)}
          </div>`
        : this.loadError
          ? errorBanner(translate('analysis.loadFailed'), this.loadError, () => void this.fetchView())
          : skeletonCard(2)}`;
  }
}

customElements.define('sm-insights-analysis', InsightsAnalysis);
