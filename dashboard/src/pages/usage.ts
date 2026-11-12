import { language, t as translate, rich } from '../i18n';
// 用量:台账里跑完的轮按时间段汇总 —— 多少、多久、什么结果、多少 token 与费用,按天 / agent / 会话 / CLI / 模型
// 分开看,以及各 agent 轮末看到的额度。token 与费用含轮外用量(不在任何一轮里的请求),轮数与耗时只算轮。
// 数据都在服务端算好,这里只负责摆出来。
import { html, nothing, svg, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type { QuotaPoint, Spend, TokenCounts, UsageQuota, UsageQuery, UsageRange, UsageRow, UsageView } from '../api/types';
import { hrefFor } from '../app/router';
import { store } from '../app/store';
import { segmented, selectCtl } from '../components/controls';
import { icon } from '../components/icon';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { fmtSpan } from '../components/turn';
import '../components/usage-chart';
import '../components/date-range';
import { fmtCount, fmtDateTime, fmtDuration, fmtRelative } from '../util/format';
import { costText, partText, tokenTotal, unpricedModels } from '../util/spend';
import { costValue, nextSort, sortedRows, type SortState } from '../util/usage-sort';

type Dim = 'agent' | 'chat' | 'cli' | 'model';
type StatsSort = 'name' | 'turns' | 'avg_secs' | 'p90_secs' | 'failed' | 'cost';
type CostSort = 'name' | 'cost' | 'tokens' | keyof TokenCounts;
interface CostRow { key: string; label: string; name: TemplateResult | string; spend?: Spend }

const DIMS: Array<{ value: Dim; label: string }> = [
  { value: 'agent', get label() { return translate('dd2d279'); } },
  { value: 'chat', get label() { return translate('dc8e2a6'); } },
  { value: 'cli', get label() { return translate('d89e9c6'); } },
  { value: 'model', get label() { return translate('d19e7c0'); } },
];

const DIM_HEAD: Record<Dim, string> = { agent: 'Agent', get chat() { return translate('da63280'); }, cli: 'CLI', get model() { return translate('dc98e11'); } };

type CostDim = 'day' | Dim;

const COST_DIMS: Array<{ value: CostDim; label: string }> = [{ value: 'day', get label() { return translate('de12978'); } }, ...DIMS];

const COST_HEAD: Record<CostDim, string> = { get day() { return translate('d70d0c1'); }, agent: 'Agent', get chat() { return DIM_HEAD.chat; }, cli: 'CLI', get model() { return DIM_HEAD.model; } };

/** 费用的四项,按表头顺序。 */
const COST_PARTS: Array<{ key: keyof TokenCounts; label: string }> = [
  { key: 'input', get label() { return translate('d8dcc1b'); } },
  { key: 'cache_read', get label() { return translate('d1ded65'); } },
  { key: 'cache_write', get label() { return translate('d6078a6'); } },
  { key: 'output', get label() { return translate('dfb04ad'); } },
];

const RANGE_KEY = 'summon-dash.usage.range';

function loadRange(): UsageRange {
  try {
    const v = localStorage.getItem(RANGE_KEY);
    if (v === 'relative' && Number.isInteger(Number(localStorage.getItem(`${RANGE_KEY}.hours`))) && Number(localStorage.getItem(`${RANGE_KEY}.hours`)) > 0) return v;
    if (v === 'today' || v === '7d' || v === '30d' || v === '90d' || v === 'all') return v;
  } catch {
    // 读不到存储时用默认时间段
  }
  return '7d';
}

function saveRange(query: UsageQuery): void {
  try {
    localStorage.setItem(RANGE_KEY, query.range);
    if (query.range === 'relative') localStorage.setItem(`${RANGE_KEY}.hours`, String(query.hours));
  } catch {
    // 记不住只影响下次打开的默认时间段
  }
}

/** 占比:没有分母写「—」,不到 1% 但不为零写「<1%」。 */
export function pct(part: number, whole: number): string {
  if (!whole) return '—';
  const p = (part / whole) * 100;
  if (part > 0 && p < 1) return '<1%';
  return `${Math.round(p)}%`;
}

/** 按天那一栏的日期:「10月21日 周二」。按服务端的日历读,不换算到浏览器时区。 */
export function dayLabel(iso: string): string {
  const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(iso);
  if (!m) return iso;
  const [y, mo, d] = [Number(m[1]), Number(m[2]), Number(m[3])];
  const date = new Date(Date.UTC(y, mo - 1, d));
  const week = new Intl.DateTimeFormat(language(), { weekday: language() === 'en' ? 'short' : 'narrow', timeZone: 'UTC' }).format(date);
  return translate('d119474', [mo, d, week]);
}

/** 读数之后已经过了重置时刻:这个百分比不再是此刻的额度。 */
export function resetPassed(resetsAt: number | undefined, now = new Date()): boolean {
  return resetsAt !== undefined && resetsAt * 1000 <= now.getTime();
}

/** 额度的重置说明。 */
export function resetText(resetsAt: number | undefined, now = new Date()): string {
  if (resetsAt === undefined) return '';
  if (resetPassed(resetsAt, now)) return translate('db195eb');
  return translate('db0d55d', [fmtDateTime(new Date(resetsAt * 1000).toISOString(), now)]);
}

function meterTone(left: number): string {
  if (left < 10) return 'danger';
  if (left < 30) return 'warn';
  return 'accent';
}

function meter(label: string, left: number | undefined, resetsAt: number | undefined): TemplateResult | typeof nothing {
  if (left === undefined) return nothing;
  const reset = resetText(resetsAt);
  const tone = resetPassed(resetsAt) ? 'stale' : meterTone(left);
  return html`<div class="quota-meter">
    <div class="quota-meter-head"><span>${label}</span><strong>${rich('dbbd88e', [left])}</strong></div>
    <svg class="meter meter-${tone}" width="100%" height="6" role="img" aria-label=${translate('d15b4f8', [label, left])}>
      ${svg`<rect class="meter-track" width="100%" height="6" rx="3"></rect><rect class="meter-fill" width=${`${Math.max(0, Math.min(100, left))}%`} height="6" rx="3"></rect>`}
    </svg>
    ${reset ? html`<div class="quota-reset">${reset}</div>` : nothing}
  </div>`;
}

/** 5 小时额度在时间段里的走势。 */
function sparkline(points: QuotaPoint[], since: number, until: number): TemplateResult | typeof nothing {
  const pts = points.filter((p) => p.five_hour_left !== undefined);
  if (pts.length < 2) return nothing;
  const W = 120;
  const H = 32;
  const PAD = 5;
  const x = (t: number) => PAD + ((t - since) / Math.max(1, until - since)) * (W - 2 * PAD);
  const y = (v: number) => PAD + (1 - v / 100) * (H - 2 * PAD);
  const d = pts.map((p, i) => `${i ? 'L' : 'M'}${x(p.at_ms).toFixed(1)},${y(p.five_hour_left!).toFixed(1)}`).join('');
  const first = pts[0]!;
  const last = pts[pts.length - 1]!;
  const label = translate('d5ff945', [first.five_hour_left, last.five_hour_left]);
  return html`<svg class="spark" width=${W} height=${H} role="img" aria-label=${label}>
    ${svg`<title>${label}</title><path class="spark-line" d=${d}></path><circle class="spark-dot" cx=${x(last.at_ms)} cy=${y(last.five_hour_left!)} r="4"></circle>`}
  </svg>`;
}

function agentDisplay(alias: string): string {
  return store.state.status?.agents.find((a) => a.alias === alias)?.display || alias;
}

export class UsagePage extends PageBase {
  range: UsageRange = loadRange();
  hours: number = (() => { try { return Number(localStorage.getItem(`${RANGE_KEY}.hours`)) || 24; } catch { return 24; } })();
  from?: string;
  to?: string;
  agent = '';
  chatId = '';
  private requestId = 0;
  dim: Dim = 'agent';
  costDim: CostDim = 'day';
  statsSort: SortState<StatsSort> = { key: 'turns', direction: 'descending' };
  costSort: SortState<CostSort> | null = null;
  view: UsageView | null = null;
  refetching = false;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  async refresh(): Promise<void> {
    const requestId = ++this.requestId;
    const query: UsageQuery = { range: this.range, ...(this.range === 'relative' ? { hours: this.hours } : {}), ...(this.range === 'custom' ? { from: this.from, to: this.to } : {}), ...(this.agent ? { agent: this.agent } : {}), ...(this.chatId ? { chat_id: this.chatId } : {}) };
    this.refetching = this.view !== null;
    this.requestUpdate();
    let view: UsageView | null = null;
    let error = '';
    try {
      view = await this.call('usage.summary', query);
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
    // 日期范围会改变候选项，浏览器可能重置选中项；等 option 更新完再同步查询值。
    const agent = this.querySelector<HTMLSelectElement>('[data-control=usage-agent]');
    const chat = this.querySelector<HTMLSelectElement>('[data-control=usage-chat]');
    if (agent) agent.value = this.agent;
    if (chat) chat.value = this.chatId;
  }

  private filters(): TemplateResult {
    const scopes = this.view?.scopes ?? [];
    const agents = new Map(scopes.map((s) => [s.agent, s.agent_label]));
    const chats = new Map(scopes.filter((s) => !this.agent || s.agent === this.agent).map((s) => [s.chat_id, s.chat_label]));
    if (this.agent && !agents.has(this.agent)) agents.set(this.agent, agentDisplay(this.agent));
    if (this.chatId && !chats.has(this.chatId)) chats.set(this.chatId, this.chatId);
    return html`<div class="usage-filters">
      <label>Agent ${selectCtl({
        id: 'usage-agent', label: translate('dbefab7'), value: this.agent,
        options: [{ value: '', label: translate('d198f2f') }, ...[...agents].map(([value, label]) => ({ value, label }))],
        onChange: (value) => { this.agent = value; this.chatId = ''; void this.refresh(); },
      })}</label>
      <label>${rich('d16d657', [selectCtl({
        id: 'usage-chat', label: translate('d3342d2'), value: this.chatId,
        options: [{ value: '', label: translate('dcfc4f9') }, ...[...chats].map(([value, label]) => ({ value, label }))],
        onChange: (value) => { this.chatId = value; void this.refresh(); },
      })])}</label>
      ${this.view ? html`<span class="muted">${rich('d5db313', [this.view.since.slice(0, 10), this.view.until.slice(0, 10)])}</span>` : nothing}
    </div>`;
  }

  private stats(v: UsageView): TemplateResult {
    const t = v.totals;
    const live = t.working === undefined || t.waiting === undefined ? '—' : translate('d5fdb41', [t.working, t.waiting]);
    return html`<section class="page-section" aria-label="${translate('de009c1')}"><div class="card">
      <div class="stats">
        <div class="stat">
          <div class="stat-label">${rich('dc3e860')}</div>
          <div class="stat-value stat-figure">${fmtCount(t.turns)}</div>
        </div>
        <div class="stat">
          <div class="stat-label">${rich('d42aa8e')}</div>
          <div class="stat-value stat-figure">${t.seconds ? fmtDuration(t.seconds) : '—'}</div>
        </div>
        <div class="stat">
          <div class="stat-label">${rich('d1ea6c5')}</div>
          <div class="stat-value stat-figure">${pct(t.failed + t.interrupted, t.turns)}</div>
          <div class="stat-sub">${rich('df8a9f8', [t.failed, t.interrupted])}</div>
        </div>
        <a class="stat stat-link" href=${hrefFor({ page: 'sessions' })}>
          <div class="stat-label">${rich('d9800a4')}</div>
          <div class="stat-value stat-figure">${live}</div>
          <div class="stat-sub">${rich('d1e1715')}</div>
        </a>
      </div>
    </div></section>`;
  }

  private priceNote(v: UsageView): string {
    return v.prices
      ? translate('d65828b', [fmtDateTime(v.prices.fetched_at)])
      : translate('d0db88b');
  }

  private cost(v: UsageView): TemplateResult {
    const s = v.totals.spend;
    const part = (label: string, amount: string, tokens: number, strong = false) => html`<div class="stat">
      <div class="stat-label">${label}</div>
      <div class="stat-value ${strong ? 'stat-figure' : ''}">${amount}</div>
      <div class="stat-sub">${fmtCount(tokens)} token</div>
    </div>`;
    return html`<section class="page-section" aria-labelledby="us-cost">
      <div class="group-head">
        <div>
          <h2 id="us-cost">${rich('d7fe860')}</h2>
          <p>${rich('d179527', [this.priceNote(v)])}</p>
        </div>
      </div>
      <div class="card">
        ${s
          ? html`<div class="stats stats-cost">
                ${part(translate('de009c1'), costText(s), tokenTotal(s.tokens), true)}
                ${COST_PARTS.map((p) => part(p.label, partText(s, p.key), s.tokens[p.key]))}
              </div>
              ${v.totals.outside
                ? html`<p class="row-note cost-note">
                    ${icon('info', 'icon-sm')}<span
                      >${rich('dce0efa', [costText(v.totals.outside), fmtCount(tokenTotal(v.totals.outside.tokens))])}</span
                    >
                  </p>`
                : nothing}
              ${s.unpriced
                ? html`<p class="row-note cost-note">
                    ${icon('info', 'icon-sm')}<span
                      >${rich('d148ff7', [fmtCount(s.unpriced.tokens), v.prices ? translate('d49a215') : translate('d88f5f5'), unpricedModels(s.unpriced.models)])}</span
                    >
                  </p>`
                : nothing}`
          : emptyState(translate('d437ce7'), translate('d0713ca'))}
      </div>
    </section>`;
  }

  private groupRows(v: UsageView, dim: Dim): UsageRow[] {
    return dim === 'agent' ? v.by_agent : dim === 'chat' ? v.by_chat : dim === 'model' ? v.by_model : v.by_cli;
  }

  private sortHead<K extends string>(key: K, label: string, sort: SortState<K> | null, change: (sort: SortState<K>) => void, numeric = true, extraClass = ''): TemplateResult {
    const direction = sort?.key === key ? sort.direction : 'none';
    const next = nextSort(sort, key, numeric);
    return html`<th scope="col" class=${`${numeric ? 'num' : ''} ${extraClass}`} aria-sort=${direction}>
      <button class="table-sort" type="button" aria-label=${translate('dee6181', [label, next.direction === 'ascending' ? translate('dca715c') : translate('d4b77c3')])}
        @click=${() => { change(next); this.requestUpdate(); }}>
        ${label}<span class="table-sort-arrow" aria-hidden="true">${direction === 'ascending' ? '↑' : direction === 'descending' ? '↓' : '↕'}</span>
      </button>
    </th>`;
  }

  private costRows(v: UsageView): CostRow[] {
    const rows = this.costDim === 'day'
      ? v.days.map((d) => ({ key: d.start, label: d.start, name: dayLabel(d.start), spend: d.spend }))
      : this.groupRows(v, this.costDim).map((r) => ({ key: r.key, label: r.label, name: this.rowName(r, this.costDim as Dim), spend: r.spend }));
    return sortedRows(rows, this.costSort, (r, key) => {
      if (key === 'name') return r.label;
      if (key === 'tokens') return r.spend ? tokenTotal(r.spend.tokens) : undefined;
      return costValue(r.spend, key === 'cost' ? 'total' : key);
    });
  }

  private costTable(v: UsageView): TemplateResult {
    const rows = this.costRows(v);
    const head = (key: CostSort, label: string, numeric = true) => this.sortHead(key, label, this.costSort, (sort) => { this.costSort = sort; }, numeric);
    const cell = (s: Spend | undefined, key: keyof TokenCounts) =>
      s ? html`${partText(s, key)}<div class="cell-sub">${fmtCount(s.tokens[key])}</div>` : '—';
    return html`<section class="page-section" aria-labelledby="us-cost-table">
      <div class="group-head">
        <div>
          <h2 id="us-cost-table">${rich('dbf9e85')}</h2>
          <p>${rich('def3484')}</p>
        </div>
        ${segmented({
          label: translate('d08d199'),
          size: 'sm',
          value: this.costDim,
          options: COST_DIMS,
          onChange: (d) => {
            this.costDim = d as CostDim;
            this.requestUpdate();
          },
        })}
      </div>
      <div class="card">
        ${rows.length
          ? html`<div class="table-scroll">
              <table class="data-table">
                <thead>
                  <tr>
                    ${head('name', COST_HEAD[this.costDim], false)}
                    ${head('cost', translate('d7fe860'))}
                    ${head('tokens', 'token')}
                    ${COST_PARTS.map((p) => head(p.key, p.label))}
                  </tr>
                </thead>
                <tbody>
                  ${rows.map(
                    (r) => html`<tr>
                      <th scope="row">${r.name}</th>
                      <td class="num">${costText(r.spend)}</td>
                      <td class="num">${r.spend ? fmtCount(tokenTotal(r.spend.tokens)) : '—'}</td>
                      ${COST_PARTS.map((p) => html`<td class="num">${cell(r.spend, p.key)}</td>`)}
                    </tr>`,
                  )}
                </tbody>
              </table>
            </div>`
          : emptyState(translate('d437ce7'), '')}
      </div>
    </section>`;
  }

  private chart(v: UsageView): TemplateResult {
    return html`<section class="page-section" aria-labelledby="us-chart">
      <div class="group-head">
        <div>
          <h2 id="us-chart">${v.unit === 'hour' ? translate('d50606f') : translate('dd7c4f7')}</h2>
          <p>${rich('d057e0f')}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${v.totals.turns
          ? html`<sm-usage-chart .buckets=${v.buckets} .unit=${v.unit}></sm-usage-chart>`
          : emptyState(translate('d60f944'), translate('d66e686'))}
      </div>
    </section>`;
  }

  private rowName(r: UsageRow, dim: Dim = this.dim): TemplateResult | string {
    if (dim === 'agent' && r.agent) {
      return html`<a href=${hrefFor({ page: 'agent', alias: r.agent, tab: 'overview' })}>${r.label}</a>`;
    }
    if (dim === 'chat' && r.agent && r.chat_id) {
      return html`<a href=${hrefFor({ page: 'session', agent: r.agent, chatId: r.chat_id })}>${r.label}</a>
        <span class="muted">${agentDisplay(r.agent)}</span>`;
    }
    return r.label;
  }

  private breakdown(v: UsageView): TemplateResult {
    const rows = sortedRows(this.groupRows(v, this.dim), this.statsSort, (r, key) => {
      if (key === 'name') return r.label;
      if (key === 'cost') return costValue(r.spend);
      if (key === 'failed') return r.turns ? r.failed / r.turns : undefined;
      if (key === 'turns') return r.turns;
      return r.turns ? r[key] : undefined;
    });
    const head = (key: StatsSort, label: string, numeric = true, extraClass = '') =>
      this.sortHead(key, label, this.statsSort, (sort) => { this.statsSort = sort; }, numeric, extraClass);
    return html`<section class="page-section" aria-labelledby="us-table">
      <div class="group-head">
        <div><h2 id="us-table">${rich('db6e335')}</h2>
          ${this.dim === 'model' ? html`<p>${rich('df0a618')}</p>` : nothing}
        </div>
        ${segmented({
          label: translate('dbc8d06'),
          size: 'sm',
          value: this.dim,
          options: DIMS,
          onChange: (d) => {
            this.dim = d as Dim;
            this.requestUpdate();
          },
        })}
      </div>
      <div class="card">
        ${rows.length
          ? html`<div class="table-scroll">
              <table class="data-table">
                <thead>
                  <tr>
                    ${head('name', DIM_HEAD[this.dim], false)}
                    ${head('turns', translate('dc3e860'))}
                    ${head('avg_secs', translate('d39a4c5'))}
                    ${head('p90_secs', 'P90', true, 'col-wide')}
                    ${head('failed', translate('d999666'))}
                    ${head('cost', translate('d7fe860'))}
                  </tr>
                </thead>
                <tbody>
                  ${rows.map(
                    (r) => html`<tr>
                      <th scope="row">${this.rowName(r)}</th>
                      <td class="num">${fmtCount(r.turns)}</td>
                      <td class="num">${r.turns ? fmtSpan(r.avg_secs) : '—'}</td>
                      <td class="num col-wide">${r.turns ? fmtSpan(r.p90_secs) : '—'}</td>
                      <td class="num" title=${translate('df8a9f8', [r.failed, r.interrupted])}>${pct(r.failed, r.turns)}</td>
                      <td class="num">${costText(r.spend)}</td>
                    </tr>`,
                  )}
                </tbody>
              </table>
            </div>`
          : emptyState(translate('d60f944'), '')}
      </div>
    </section>`;
  }

  private quotaRow(q: UsageQuota, since: number, until: number): TemplateResult {
    return html`<div class="quota-row">
      <div class="quota-who">
        <strong>${q.label}</strong>
        ${q.cli ? html`<span class="chip chip-quiet">${q.cli}</span>` : nothing}
        <span class="muted">${rich('dc62592', [fmtRelative(q.at)])}</span>
      </div>
      ${meter(translate('d78d5c1'), q.five_hour_left, q.five_hour_resets_at)} ${meter(translate('d92845d'), q.long_left, q.long_resets_at)}
      ${sparkline(q.points, since, until)}
    </div>`;
  }

  private quota(v: UsageView): TemplateResult {
    const since = Date.parse(v.since);
    const until = Date.parse(v.until);
    return html`<section class="page-section" aria-labelledby="us-quota">
      <div class="group-head">
        <div>
          <h2 id="us-quota">${rich('d32ce9f')}</h2>
          <p>${rich('de3154a')}</p>
        </div>
      </div>
      <div class="card">
        ${v.quota.length
          ? v.quota.map((q) => this.quotaRow(q, since, until))
          : emptyState(translate('d8dfd0e'), translate('de969f2'))}
      </div>
    </section>`;
  }

  protected override render(): TemplateResult {
    const v = this.view;
    return html`<div class="page">
      ${pageHead(
        translate('d42723e'),
        translate('dfe1584'),
      )}
      <div class="usage-toolbar" role="region" aria-label="${translate('dc11b3d')}">
        <span class="usage-toolbar-label">${rich('dc95ff6')}</span>
        <sm-date-range .query=${{ range: this.range, from: this.from, to: this.to, hours: this.hours }} .today=${this.view?.today ?? new Date().toLocaleDateString('en-CA')} .since=${this.view?.since ?? ''}
          @range-change=${(e: CustomEvent<UsageQuery>) => this.setRange(e.detail)}></sm-date-range>
      </div>
      ${this.filters()}
      ${this.loadError ? errorBanner(translate('d4b4dff'), this.loadError, () => this.refresh()) : nothing}
      ${this.loading
        ? skeletonCard(4)
        : v
          ? html`<div class="usage ${this.refetching ? 'is-stale' : ''}">
              ${this.stats(v)} ${this.cost(v)} ${this.chart(v)} ${this.breakdown(v)} ${this.costTable(v)}
              ${this.quota(v)}
            </div>`
          : nothing}
    </div>`;
  }
}

customElements.define('sm-page-usage', UsagePage);
