import { language, t as translate, rich } from '../i18n';
// 洞察:从运行记录统计 agent 回复后到同一会话里同一个人发出下一条消息的间隔,以及长间隔期间的别处
// 活动、下一条消息的类型、每日输入量,再把服务端按规则得出的结论(key + 参数)渲染成句子。
// 数都在服务端算好,这里只负责摆出来;可见范围与用量页相同。
import { html, nothing, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type {
  InsightsDay,
  InsightsFinding,
  InsightsLongSpan,
  InsightsNextKind,
  InsightsQuantiles,
  InsightsQuery,
  InsightsSpan,
  InsightsSummary,
  UsageQuery,
  UsageRange,
} from '../api/types';
import { store } from '../app/store';
import { selectCtl } from '../components/controls';
import { icon } from '../components/icon';
import type { BarRow, BarSeries } from '../components/insights-charts';
import '../components/insights-charts';
import '../components/insights-analysis';
import '../components/date-range';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { fmtSpan } from '../components/turn';
import { fmtCount } from '../util/format';

const RANGE_KEY = 'summon-dash.insights.range';
const DEFAULT_RANGE: UsageRange = '30d';

function loadRange(): UsageRange {
  try {
    const v = localStorage.getItem(RANGE_KEY);
    if (v === 'relative' && Number.isInteger(Number(localStorage.getItem(`${RANGE_KEY}.hours`))) && Number(localStorage.getItem(`${RANGE_KEY}.hours`)) > 0) return v;
    if (v === 'today' || v === '7d' || v === '30d' || v === '90d' || v === 'all') return v;
  } catch {
    // 读不到存储时用默认时间段
  }
  return DEFAULT_RANGE;
}

function saveRange(query: UsageQuery): void {
  try {
    localStorage.setItem(RANGE_KEY, query.range);
    if (query.range === 'relative') localStorage.setItem(`${RANGE_KEY}.hours`, String(query.hours));
  } catch {
    // 记不住只影响下次打开的默认时间段
  }
}

/** 时长:没有样本写「—」;10 分钟以上取整到分钟,秒在这个量级上只是噪音。 */
export function span(secs: number, n = 1): string {
  if (!(n > 0 && secs > 0)) return '—';
  // 不到 1 秒时 fmtSpan(0) 写「少于 1 秒」;直接给小数会被四舍五入成空串
  return fmtSpan(secs < 1 ? 0 : secs >= 600 ? Math.round(secs / 60) * 60 : secs);
}

/** 0–1 的比例写成百分比:不到 1% 但不为零写「<1%」。 */
export function percent(share: number): string {
  if (!(share > 0)) return '0%';
  if (share < 0.01) return '<1%';
  return `${Math.round(share * 100)}%`;
}

/** 两数之比:一位小数,整数不带 .0。 */
export function ratio(v: number): string {
  return (Math.round(v * 10) / 10).toFixed(1).replace(/\.0$/, '');
}

/** 与上一时段相比的变化:「+22%」「−8%」;变化不到 1% 写「持平」;上一时段为 0 时不比。 */
export function change(current: number, previous: number): string {
  if (!(previous > 0)) return '';
  const delta = (current - previous) / previous;
  if (Math.abs(delta) < 0.005) return translate('da9105f');
  return `${delta > 0 ? '+' : '−'}${Math.round(Math.abs(delta) * 100)}%`;
}

/** 有可比的上一时段:范围不是「全部」,且上一时段至少有一条间隔。 */
export function comparable(v: InsightsSummary): boolean {
  return (v.previous?.gaps.n ?? 0) > 0;
}

export const SPAN_LABEL: Record<InsightsSpan, string> = {
  get lt_1m() { return translate('d3dd879'); },
  get m1_5() { return translate('dfae951'); },
  get m5_15() { return translate('d88d8ed'); },
  get m15_60() { return translate('dd8c68a'); },
  get h1_4() { return translate('d2453b8'); },
  get gte_4h() { return translate('dd5a7de'); },
};

const LONG_SPANS: InsightsLongSpan[] = ['m15_60', 'h1_4', 'gte_4h'];

export const NEXT_LABEL: Record<InsightsNextKind, string> = {
  get instruction() { return translate('db2a33c'); },
  get short() { return translate('db0ec1f'); },
  get approve() { return translate('d561817'); },
  get poll() { return translate('dfe2371'); },
};

const NEXT_HINT: Record<InsightsNextKind, string> = {
  get instruction() { return translate('dc5a06f'); },
  get short() { return translate('d6f453b'); },
  get approve() { return translate('d147221'); },
  get poll() { return translate('d5837ec'); },
};

const num = (f: InsightsFinding, k: string) => Number(f.params[k] ?? 0);

/** 一条结论的句子。参数缺失按 0 处理,不因服务端多给或少给一个参数而整页出错。 */
export function findingText(f: InsightsFinding): string {
  switch (f.key) {
    case 'sample_small':
      return translate('dc30a96', [num(f, 'n')]);
    case 'tail_concentration':
      return translate('d698dbb', [percent(num(f, 'count_share')), percent(num(f, 'secs_share'))]);
    case 'busy_elsewhere':
      return translate('d0db839', [percent(num(f, 'share')), percent(num(f, 'silent_share'))]);
    case 'mostly_instructions':
      return translate('d191e9b', [percent(num(f, 'approve_share')), span(num(f, 'approve_p50_secs'))]);
    case 'polls':
      return translate('d6bc98a', [num(f, 'count'), span(num(f, 'p50_secs'))]);
    case 'load':
      return translate('da711a8', [ratio(num(f, 'messages_p50')), ratio(num(f, 'chats_p50'))]);
    case 'human_vs_agent':
      return translate('d57d7bd', [ratio(num(f, 'ratio'))]);
    case 'trend':
      return f.params.direction === 'down'
        ? translate('d35aef4', [span(num(f, 'previous_p50_secs')), span(num(f, 'p50_secs'))])
        : translate('db75e9f', [span(num(f, 'previous_p50_secs')), span(num(f, 'p50_secs'))]);
  }
}

/** 页面上展示的结论:样本不足时只有那一条(服务端已保证,这里再收一次口)。 */
export function shownFindings(findings: InsightsFinding[]): InsightsFinding[] {
  const small = findings.find((f) => f.key === 'sample_small');
  return small ? [small] : findings;
}

/** 15 分钟以上的间隔占条数、占时长。 */
export function longShares(v: InsightsSummary): { count: number; secs: number } {
  const long = v.buckets.filter((b) => (LONG_SPANS as string[]).includes(b.key));
  const count = long.reduce((n, b) => n + b.count, 0);
  const secs = long.reduce((n, b) => n + b.secs, 0);
  return { count: v.gaps.n ? count / v.gaps.n : 0, secs: v.gaps.total_secs ? secs / v.gaps.total_secs : 0 };
}

/** 间隔分布的各行:占条数与占时长画在同一把尺子上,最长的那根占满。 */
export function bucketRows(v: InsightsSummary): BarRow[] {
  const n = v.gaps.n;
  const total = v.gaps.total_secs;
  const shares = v.buckets.map((b) => ({ b, count: n ? b.count / n : 0, secs: total ? b.secs / total : 0 }));
  const scale = Math.max(0, ...shares.flatMap((s) => [s.count, s.secs])) || 1;
  return shares.map(({ b, count, secs }) => ({
    key: b.key,
    label: SPAN_LABEL[b.key],
    note: translate('d233f28', [fmtCount(b.count), span(b.secs, b.count)]),
    bars: [
      { series: 'count', fraction: count / scale, text: percent(count) },
      { series: 'time', fraction: secs / scale, text: percent(secs) },
    ],
    tip: [
      SPAN_LABEL[b.key],
      translate('d4c3652', [fmtCount(b.count), percent(count)]),
      translate('d678404', [span(b.secs, b.count), percent(secs)]),
    ],
  }));
}

/** 长间隔期间的别处活动:每档里期间其它会话有消息的占比。 */
export function elsewhereRows(v: InsightsSummary): BarRow[] {
  return v.elsewhere.map((e) => {
    const share = e.count ? e.active_elsewhere / e.count : 0;
    const median = translate('ddf0ee6', [ratio(e.elsewhere_messages_p50)]);
    return {
      key: e.key,
      label: SPAN_LABEL[e.key],
      note: translate('d7b2430', [fmtCount(e.count), median]),
      bars: [{ series: 'count', fraction: share, text: e.count ? percent(share) : '—' }],
      tip: [
        SPAN_LABEL[e.key],
        translate('df7ecdf', [fmtCount(e.count), span(e.secs, e.count)]),
        translate('d0e7fb1', [fmtCount(e.active_elsewhere), e.count ? percent(share) : '—']),
        median,
      ],
    };
  });
}

/** 下一条消息的类型:条数画条,间隔中位数与 P90 写在下面。 */
export function nextRows(v: InsightsSummary): BarRow[] {
  const max = Math.max(0, ...v.next_messages.map((r) => r.count)) || 1;
  const n = v.next_messages.reduce((s, r) => s + r.count, 0);
  return v.next_messages.map((r) => {
    const quantiles = translate('d45de09', [span(r.p50_secs, r.count), span(r.p90_secs, r.count)]);
    return {
      key: r.key,
      label: NEXT_LABEL[r.key],
      note: quantiles,
      bars: [{ series: 'count', fraction: r.count / max, text: translate('dcba97a', [fmtCount(r.count), n ? percent(r.count / n) : '—']) }],
      tip: [`${NEXT_LABEL[r.key]} · ${NEXT_HINT[r.key]}`, translate('d70e7f1', [fmtCount(r.count), n ? percent(r.count / n) : '—']), quantiles],
    };
  });
}

/** 日期的完整说法:「11月3日 周一」。按服务端日历,weekday 1 = 周一。 */
export function dayTitle(d: InsightsDay): string {
  const date = new Date(Date.UTC(2024, 0, d.weekday)); // 2024-01-01 是周一
  const week = new Intl.DateTimeFormat(language(), { weekday: language() === 'en' ? 'short' : 'narrow', timeZone: 'UTC' }).format(date);
  return translate('d119474', [Number(d.date.slice(5, 7)), Number(d.date.slice(8, 10)), week]);
}

function agentDisplay(alias: string): string {
  return store.state.status?.agents.find((a) => a.alias === alias)?.display || alias;
}

const COUNT_SERIES: BarSeries[] = [{ key: 'count', get label() { return translate('da4dcf6'); } }];
const SHARE_SERIES: BarSeries[] = [
  { key: 'count', get label() { return translate('da4dcf6'); } },
  { key: 'time', get label() { return translate('d90e693'); } },
];

export class InsightsPage extends PageBase {
  range: UsageRange = loadRange();
  hours: number = (() => { try { return Number(localStorage.getItem(`${RANGE_KEY}.hours`)) || 24; } catch { return 24; } })();
  from?: string;
  to?: string;
  agent = '';
  private requestId = 0;
  view: InsightsSummary | null = null;
  refetching = false;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  query(): InsightsQuery {
    return { range: this.range, ...(this.range === 'relative' ? { hours: this.hours } : {}), ...(this.range === 'custom' ? { from: this.from, to: this.to } : {}), ...(this.agent ? { agent: this.agent } : {}) };
  }

  async refresh(): Promise<void> {
    const requestId = ++this.requestId;
    this.refetching = this.view !== null;
    this.requestUpdate();
    let view: InsightsSummary | null = null;
    let error = '';
    try {
      view = await this.call('insights.summary', this.query());
    } catch (err) {
      error = errorMessage(err);
    }
    // 连着换了几个时间段:只认最后选的那个,早发的请求晚到就丢掉。
    if (requestId !== this.requestId) return;
    this.view = view;
    this.loadError = error;
    this.loading = false;
    this.refetching = false;
    this.requestUpdate();
  }

  private setRange(query: UsageQuery): void {
    this.range = query.range;
    this.from = query.from;
    this.to = query.to;
    this.hours = query.hours ?? 24;
    if (query.range !== 'custom') saveRange(query);
    void this.refresh();
  }

  protected override updated(): void {
    // 候选项变了浏览器可能重置选中项;等 option 更新完再同步查询值。
    const agent = this.querySelector<HTMLSelectElement>('[data-control=insights-agent]');
    if (agent) agent.value = this.agent;
  }

  private filters(): TemplateResult {
    const agents = new Map((store.state.status?.agents ?? []).map((a) => [a.alias, a.display || a.alias]));
    if (this.agent && !agents.has(this.agent)) agents.set(this.agent, agentDisplay(this.agent));
    const v = this.view;
    return html`<div class="usage-filters">
      <label>Agent ${selectCtl({
        id: 'insights-agent', label: translate('dbefab7'), value: this.agent,
        options: [{ value: '', label: translate('d198f2f') }, ...[...agents].map(([value, label]) => ({ value, label }))],
        onChange: (value) => { this.agent = value; void this.refresh(); },
      })}</label>
      ${v ? html`<span class="muted">${v.range === 'relative' ? rich('range.executedTimes', [v.from.replace('T', ' '), v.to.replace('T', ' ')]) : rich('d5db313', [v.from, v.to])}</span>` : nothing}
    </div>`;
  }

  private findings(v: InsightsSummary): TemplateResult {
    const items = shownFindings(v.findings);
    return html`<section class="page-section" aria-labelledby="in-findings">
      <div class="group-head">
        <div>
          <h2 id="in-findings">${rich('d14163a')}</h2>
          <p>${rich('d63f5fb')}</p>
        </div>
      </div>
      <div class="card">
        ${items.length
          ? html`<ul class="finding-list">
              ${items.map((f) => html`<li class="finding ${f.key === 'sample_small' ? 'is-notice' : ''}" data-key=${f.key}>${icon(f.key === 'sample_small' ? 'info' : 'lightbulb', 'icon-sm')}<span>${findingText(f)}</span></li>`)}
            </ul>`
          : emptyState(translate('d243cb8'), translate('d9da773'))}
      </div>
    </section>`;
  }

  private stat(label: string, value: string, sub?: string): TemplateResult {
    return html`<div class="stat">
      <div class="stat-label">${label}</div>
      <div class="stat-value stat-figure">${value}</div>
      ${sub ? html`<div class="stat-sub">${sub}</div>` : nothing}
    </div>`;
  }

  /** 与上一时段的对比:没有可比的上一时段时不写。 */
  private versus(current: number, previous: number | undefined, text: string): string | undefined {
    if (previous === undefined) return undefined;
    const delta = change(current, previous);
    return delta ? translate('d284b22', [text, delta]) : translate('dc3d5ef', [text]);
  }

  private overview(v: InsightsSummary): TemplateResult {
    const g = v.gaps;
    const p: InsightsQuantiles | undefined = comparable(v) ? v.previous!.gaps : undefined;
    const a = v.agent_turns;
    const human = a.total_secs > 0 && g.n > 0 ? translate('db50a49', [ratio(g.total_secs / a.total_secs)]) : '—';
    const q = (key: 'p50_secs' | 'p75_secs' | 'p90_secs' | 'total_secs') => this.versus(g[key], p?.[key], span(p?.[key] ?? 0, p?.n));
    return html`<section class="page-section" aria-labelledby="in-overview">
      <div class="group-head">
        <div>
          <h2 id="in-overview">${rich('dfea405')}</h2>
          <p>${rich('d5ce21a', [fmtCount(v.human_turns), v.agents, v.chats])}</p>
        </div>
      </div>
      <div class="card">
        <div class="stats stats-insights">
          ${this.stat(translate('dbd9f05'), fmtCount(g.n), this.versus(g.n, p?.n, translate('d5cc275', [fmtCount(p?.n ?? 0)])))}
          ${this.stat(translate('d5034a4'), span(g.p50_secs, g.n), q('p50_secs'))}
          ${this.stat('P75', span(g.p75_secs, g.n), q('p75_secs'))}
          ${this.stat('P90', span(g.p90_secs, g.n), q('p90_secs'))}
          ${this.stat(translate('d793403'), span(g.total_secs, g.n), q('total_secs'))}
          ${this.stat(translate('d29ae0a'), span(a.p50_secs, a.n), translate('d3dca46', [span(a.p90_secs, a.n), fmtCount(a.n)]))}
          ${this.stat(translate('d37c1fb'), span(a.total_secs, a.n))}
          ${this.stat(translate('d520715'), human)}
        </div>
      </div>
    </section>`;
  }

  private distribution(v: InsightsSummary): TemplateResult {
    const now = longShares(v);
    const prev = comparable(v) ? v.previous : undefined;
    return html`<section class="page-section" aria-labelledby="in-dist">
      <div class="group-head">
        <div>
          <h2 id="in-dist">${rich('d89faa5')}</h2>
          <p>${v.gaps.n ? rich('d8f092c', [percent(now.count), percent(now.secs)]) : nothing}${v.gaps.n && prev ? rich('d26612d', [percent(prev.over_15m_count_share), percent(prev.over_15m_secs_share)]) : nothing}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${v.gaps.n
          ? html`<sm-bar-list .series=${SHARE_SERIES} .rows=${bucketRows(v)} label=${translate('ddad469')}
              .table=${{
                head: [translate('d4f5df7'), translate('de18aa1'), translate('da4dcf6'), translate('d4fb147'), translate('d90e693')],
                rows: v.buckets.map((b) => [SPAN_LABEL[b.key], fmtCount(b.count), percent(v.gaps.n ? b.count / v.gaps.n : 0), span(b.secs, b.count), percent(v.gaps.total_secs ? b.secs / v.gaps.total_secs : 0)]),
              }}></sm-bar-list>`
          : this.noGaps()}
      </div>
    </section>`;
  }

  private noGaps(): TemplateResult {
    return emptyState(translate('d51af1a'), translate('d72a198'));
  }

  private elsewhere(v: InsightsSummary): TemplateResult {
    return html`<section class="page-section" aria-labelledby="in-elsewhere">
      <div class="group-head">
        <div>
          <h2 id="in-elsewhere">${rich('dd7bcab')}</h2>
          <p>${rich('df832ac')}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${v.long_gaps
          ? html`<sm-bar-list .series=${COUNT_SERIES} .rows=${elsewhereRows(v)} label=${translate('d4759f6')}
                .table=${{
                  head: [translate('d4f5df7'), translate('de18aa1'), translate('dac5c50'), translate('d11147c')],
                  rows: v.elsewhere.map((e) => [SPAN_LABEL[e.key], fmtCount(e.count), translate('d73d555', [fmtCount(e.active_elsewhere), e.count ? percent(e.active_elsewhere / e.count) : '—']), ratio(e.elsewhere_messages_p50)]),
                }}></sm-bar-list>
              <p class="row-note">${icon('info', 'icon-sm')}<span>${rich('d895924', [fmtCount(v.long_gaps), fmtCount(v.long_gaps_silent), percent(v.long_gaps_silent / v.long_gaps)])}</span></p>`
          : v.gaps.n
            ? emptyState(translate('d25c89e'), '')
            : this.noGaps()}
      </div>
    </section>`;
  }

  private nextMessages(v: InsightsSummary): TemplateResult {
    return html`<section class="page-section" aria-labelledby="in-next">
      <div class="group-head">
        <div>
          <h2 id="in-next">${rich('d9e879c')}</h2>
          <p>${rich('d1f8684')}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${v.gaps.n
          ? html`<sm-bar-list wide .series=${COUNT_SERIES} .rows=${nextRows(v)} label=${translate('d1e488f')}
              .table=${{
                head: [translate('dba4001'), translate('de18aa1'), translate('d5034a4'), 'P90'],
                rows: v.next_messages.map((r) => [NEXT_LABEL[r.key], fmtCount(r.count), span(r.p50_secs, r.count), span(r.p90_secs, r.count)]),
              }}></sm-bar-list>`
          : this.noGaps()}
      </div>
    </section>`;
  }

  private daily(v: InsightsSummary): TemplateResult {
    const w = v.workdays;
    return html`<section class="page-section" aria-labelledby="in-daily">
      <div class="group-head">
        <div>
          <h2 id="in-daily">${rich('dfbac42')}</h2>
          <p>${w.days
            ? rich('d8dbc74', [w.days, ratio(w.messages_p50), ratio(w.messages_p90), ratio(w.chats_p50), ratio(w.chats_p90)])
            : rich('d2bdb32')}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${v.days.some((d) => d.messages > 0)
          ? html`<sm-day-chart .days=${v.days} .dayTitle=${dayTitle}></sm-day-chart>`
          : emptyState(translate('d2f780d'), '')}
      </div>
    </section>`;
  }

  private caliber(v: InsightsSummary): TemplateResult {
    const hours = [String(v.day_start_hour).padStart(2, '0'), String(v.day_end_hour).padStart(2, '0')];
    return html`<section class="page-section" aria-labelledby="in-caliber">
      <div class="group-head">
        <div><h2 id="in-caliber">${rich('d380869')}</h2></div>
      </div>
      <div class="card card-pad">
        <ul class="caliber-list">
          <li>${rich('d1f2cb0')}</li>
          <li>${rich('da42c2a', [hours[0], hours[1], fmtCount(v.gaps_including_overnight)])}</li>
          <li>${rich('da2a731', [fmtCount(v.queued)])}</li>
          <li>${rich('d0fa3f6')}</li>
          <li>${rich('de22c64')}</li>
          <li>${rich('de853fe')}</li>
          ${v.range === 'today' && v.previous ? html`<li>${rich('df07340')}</li>` : nothing}
        </ul>
      </div>
    </section>`;
  }

  private body(v: InsightsSummary): TemplateResult {
    if (!v.human_turns) {
      return html`<section class="page-section"><div class="card">
          ${emptyState(translate('d2f780d'), translate('d66e686'))}
        </div></section>
        ${this.caliber(v)}`;
    }
    // 分析是模型写的文本,放在规则结论之后、单独成节,不与结论混在一起。
    return html`${this.findings(v)} <sm-insights-analysis class="page-section" role="region" aria-labelledby="in-analysis" .query=${this.query()}></sm-insights-analysis> ${this.overview(v)} ${this.distribution(v)} ${this.elsewhere(v)} ${this.nextMessages(v)} ${this.daily(v)}
      ${this.caliber(v)}`;
  }

  protected override render(): TemplateResult {
    const v = this.view;
    const today = v && this.range !== 'custom' ? v.to.slice(0, 10) : new Date().toLocaleDateString('en-CA');
    return html`<div class="page">
      ${pageHead(translate('d9d7407'), translate('d6f6c60'))}
      <div class="usage-toolbar" role="region" aria-label="${translate('d45c828')}">
        <span class="usage-toolbar-label">${rich('dc95ff6')}</span>
        <sm-date-range .query=${{ range: this.range, from: this.from, to: this.to, hours: this.hours }} .today=${today} .since=${v?.from ?? ''}
          @range-change=${(e: CustomEvent<UsageQuery>) => this.setRange(e.detail)}></sm-date-range>
      </div>
      ${this.filters()}
      ${this.loadError ? errorBanner(translate('d9e90fa'), this.loadError, () => this.refresh()) : nothing}
      ${this.loading
        ? skeletonCard(4)
        : v
          ? html`<div class="usage ${this.refetching ? 'is-stale' : ''}">${this.body(v)}</div>`
          : nothing}
    </div>`;
  }
}

customElements.define('sm-page-insights', InsightsPage);
