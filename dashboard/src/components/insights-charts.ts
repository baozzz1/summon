import { LocaleController } from '../i18n';
import { t as translate, rich } from '../i18n';
// 洞察页的两种图:横向条形列表(分档、类型这类有序或少量的类别)与每日柱状图。自己画 SVG,
// 几何全走 SVG 属性,浮层位置经 CSSOM 设(CSP 不放行内联样式)。颜色按度量取 tokens.css 里的
// --chart-count(条数)与 --chart-time(时长),同一度量在各张图里同色;文字一律用正文 / 次要文字色。
import { html, LitElement, nothing, svg, type TemplateResult } from 'lit';
import type { InsightsDay } from '../api/types';
import { niceTicks, roundTop } from '../util/chart';

export type BarSeriesKey = 'count' | 'time';

export interface BarSeries {
  key: BarSeriesKey;
  label: string;
}

export interface BarDatum {
  series: BarSeriesKey;
  /** 条长,占满整行为 1 */
  fraction: number;
  /** 条尾的读数 */
  text: string;
}

export interface BarRow {
  key: string;
  label: string;
  /** 条下面的补充读数 */
  note?: string;
  bars: BarDatum[];
  /** 悬停提示里的各行,第一行是标题;读屏也读它 */
  tip: string[];
}

export interface TableView {
  head: string[];
  rows: string[][];
}

const BAR_H = 8;

/** 一根横条:左端方角贴着基线,右端(数据端)4px 圆角。内层 svg 按比例给宽,超出部分被它裁掉。 */
export function barShape(series: BarSeriesKey, fraction: number): TemplateResult | typeof nothing {
  if (!(fraction > 0)) return nothing;
  const width = `${Math.min(100, Math.max(0.5, fraction * 100))}%`;
  return svg`<svg class="ib-fill ib-${series}" width=${width} height=${BAR_H}>
    <rect width="100%" height=${BAR_H} rx="4"></rect><rect width="4" height=${BAR_H}></rect>
  </svg>`;
}

function tableView(label: string, table: TableView): TemplateResult {
  return html`<details class="uc-table">
    <summary>${rich('d9cee74')}</summary>
    <div class="table-scroll">
      <table class="data-table" aria-label=${label}>
        <thead>
          <tr>${table.head.map((h, i) => html`<th scope="col" class=${i ? 'num' : ''}>${h}</th>`)}</tr>
        </thead>
        <tbody>
          ${table.rows.map(
            (r) => html`<tr>${r.map((c, i) => (i ? html`<td class="num">${c}</td>` : html`<th scope="row">${c}</th>`))}</tr>`,
          )}
        </tbody>
      </table>
    </div>
  </details>`;
}

function legend(items: Array<{ key: string; label: string }>): TemplateResult {
  return html`<div class="uc-legend">
    ${items.map((d) => html`<span class="uc-legend-item"><span class="uc-swatch ib-key-${d.key}" aria-hidden="true"></span>${d.label}</span>`)}
  </div>`;
}

/** 横向条形列表:每个类别一行,一行里每个系列一根条,条尾写读数。 */
class BarList extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = { series: { attribute: false }, rows: { attribute: false }, table: { attribute: false }, label: {}, wide: { type: Boolean } };
  declare series: BarSeries[];
  declare rows: BarRow[];
  declare table: TableView;
  declare label: string;
  /** 条尾读数较长(带条数与占比)时加宽读数一栏 */
  declare wide: boolean;
  private hover: number | null = null;

  protected override createRenderRoot(): HTMLElement {
    return this;
  }

  private setHover(i: number | null): void {
    if (this.hover === i) return;
    this.hover = i;
    this.requestUpdate();
  }

  protected override updated(): void {
    const tip = this.querySelector<HTMLElement>('.uc-tip');
    const row = this.hover === null ? null : this.querySelectorAll<HTMLElement>('.ib-row')[this.hover];
    if (!tip || !row) return;
    tip.style.top = `${row.offsetTop + row.offsetHeight + 4}px`;
  }

  protected override render(): TemplateResult {
    const series = this.series ?? [];
    const rows = this.rows ?? [];
    const tip = this.hover === null ? undefined : rows[this.hover];
    return html`<div class="ib ${this.wide ? 'ib-wide' : ''}">
      ${series.length > 1 ? legend(series) : nothing}
      <div class="ib-rows" role="list" aria-label=${this.label} @pointerleave=${() => this.setHover(null)}>
        ${rows.map(
          (r, i) => html`<div class="ib-row ${this.hover !== null && this.hover !== i ? 'is-dim' : ''}" role="listitem" tabindex="0"
            aria-label=${r.tip.join('，')} data-key=${r.key}
            @pointerenter=${() => this.setHover(i)} @focus=${() => this.setHover(i)} @blur=${() => this.setHover(null)}>
            <span class="ib-label">${r.label}</span>
            <span class="ib-bars">
              ${r.bars.map(
                (b) => html`<span class="ib-bar"><svg class="ib-svg" width="100%" height=${BAR_H} aria-hidden="true">${barShape(b.series, b.fraction)}</svg><span class="ib-value">${b.text}</span></span>`,
              )}
              ${r.note ? html`<span class="ib-note">${r.note}</span>` : nothing}
            </span>
          </div>`,
        )}
        ${tip
          ? html`<div class="uc-tip ib-tip" role="status">
              <div class="uc-tip-head">${tip.tip[0]}</div>
              ${tip.tip.slice(1).map((line) => html`<div class="ib-tip-line">${line}</div>`)}
            </div>`
          : nothing}
      </div>
      ${this.table ? tableView(this.label, this.table) : nothing}
    </div>`;
  }
}

customElements.define('sm-bar-list', BarList);

/** 横轴上的日期:「11/3」。按服务端日历的字面值,不换算时区。 */
export function dayTick(date: string): string {
  return `${Number(date.slice(5, 7))}/${Number(date.slice(8, 10))}`;
}

export function isWeekend(d: InsightsDay): boolean {
  return d.weekday >= 6;
}

const PLOT_H = 160;
const TOP = 8;
const AXIS_H = 24;
const LEFT = 32;

/** 每日输入量:每天一根柱子,高度是真人发起的消息数;周末的柱子用浅一档的同色,图例说明。 */
class DayChart extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = { days: { attribute: false }, dayTitle: { attribute: false } };
  declare days: InsightsDay[];
  /** 日期的完整说法(带星期),页面给 */
  declare dayTitle: (d: InsightsDay) => string;
  private width = 640;
  private hover: number | null = null;
  private observer: ResizeObserver | undefined;

  protected override createRenderRoot(): HTMLElement {
    return this;
  }

  override connectedCallback(): void {
    super.connectedCallback();
    this.observer = new ResizeObserver((entries) => {
      const w = Math.round(entries[0]?.contentRect.width ?? 0);
      if (w > 0 && w !== this.width) {
        this.width = w;
        this.requestUpdate();
      }
    });
    this.observer.observe(this);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.observer?.disconnect();
  }

  private setHover(i: number | null): void {
    if (this.hover === i) return;
    this.hover = i;
    this.requestUpdate();
  }

  protected override updated(): void {
    const tip = this.querySelector<HTMLElement>('.uc-tip');
    if (!tip || this.hover === null) return;
    const slot = (this.width - LEFT) / Math.max(1, this.days.length);
    const cx = LEFT + slot * this.hover + slot / 2;
    tip.style.left = `${Math.min(Math.max(cx - tip.offsetWidth / 2, 0), this.width - tip.offsetWidth)}px`;
  }

  private label(d: InsightsDay): string {
    return translate('d0f96ef', [this.dayTitle(d), d.messages, d.chats]);
  }

  protected override render(): TemplateResult {
    const days = this.days ?? [];
    const plotW = Math.max(120, this.width - LEFT);
    const ticks = niceTicks(Math.max(0, ...days.map((d) => d.messages)));
    const top = ticks[ticks.length - 1]!;
    const y = (v: number) => TOP + PLOT_H - (v / top) * PLOT_H;
    const slot = plotW / Math.max(1, days.length);
    const w = Math.max(2, Math.min(20, slot - 2));
    const every = Math.max(1, Math.ceil(48 / slot));
    const hovered = this.hover === null ? undefined : days[this.hover];
    return html`<div class="uc">
      ${legend([
        { key: 'count', label: translate('d07161a') },
        { key: 'weekend', label: translate('d709b90') },
      ])}
      <div class="uc-plot" @pointerleave=${() => this.setHover(null)}>
        <svg class="uc-svg" width=${this.width} height=${TOP + PLOT_H + AXIS_H} role="group" aria-label=${translate('db9893c')}>
          ${ticks.map(
            (t) => svg`<line class="uc-grid" x1=${LEFT} x2=${LEFT + plotW} y1=${y(t)} y2=${y(t)}></line>
              <text class="uc-tick" x=${LEFT - 6} y=${y(t)} dy="0.32em" text-anchor="end">${t}</text>`,
          )}
          ${days.map((d, i) => {
            if (!d.messages) return nothing;
            const x = LEFT + slot * i + (slot - w) / 2;
            const dim = this.hover !== null && this.hover !== i;
            return svg`<path class="uc-col ib-day ${isWeekend(d) ? 'ib-weekend' : 'ib-count'} ${dim ? 'is-dim' : ''}" d=${roundTop(x, y(d.messages), w, Math.max(1, y(0) - y(d.messages)))}></path>`;
          })}
          ${days.map((d, i) =>
            (days.length - 1 - i) % every === 0
              ? svg`<text class="uc-tick" x=${LEFT + slot * i + slot / 2} y=${TOP + PLOT_H + 16} text-anchor="middle">${dayTick(d.date)}</text>`
              : nothing,
          )}
          ${days.map(
            (d, i) => svg`<rect class="uc-hit" x=${LEFT + slot * i} y=${TOP} width=${slot} height=${PLOT_H} tabindex="0" role="img" aria-label=${this.label(d)}
              @pointerenter=${() => this.setHover(i)} @pointermove=${() => this.setHover(i)}
              @focus=${() => this.setHover(i)} @blur=${() => this.setHover(null)}></rect>`,
          )}
        </svg>
        ${hovered
          ? html`<div class="uc-tip" role="status">
              <div class="uc-tip-head">${this.dayTitle(hovered)}</div>
              <div class="ib-tip-line">${rich('db514d4', [hovered.messages])}</div>
              <div class="ib-tip-line">${rich('d39d036', [hovered.chats])}</div>
            </div>`
          : nothing}
      </div>
      ${tableView(translate('db9893c'), {
        head: [translate('d70d0c1'), translate('d0d5e2b'), translate('d578c55')],
        rows: days.map((d) => [this.dayTitle(d), String(d.messages), String(d.chats)]),
      })}
    </div>`;
  }
}

customElements.define('sm-day-chart', DayChart);
