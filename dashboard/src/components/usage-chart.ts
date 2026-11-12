import { LocaleController } from '../i18n';
import { t as translate, rich } from '../i18n';
// 用量页的柱状图:每个时间桶一根柱子,按结果分段堆叠。自己画 SVG,不引图表库;几何全走 SVG 属性,
// 浮层位置经 CSSOM 设(CSP 不放行内联样式)。颜色是 tokens.css 里的 --chart-*(按结果的状态色)。
import { html, LitElement, nothing, svg, type TemplateResult } from 'lit';
import type { UsageBucket, UsageUnit } from '../api/types';
import { niceTicks, roundTop } from '../util/chart';

export { niceTicks };

export const SERIES = [
  { key: 'done', get label() { return translate('dc0b3fb'); } },
  { key: 'quiet', get label() { return translate('df6a763'); } },
  { key: 'interrupted', get label() { return translate('df9d193'); } },
  { key: 'failed', get label() { return translate('d28384d'); } },
] as const;

export type SeriesKey = (typeof SERIES)[number]['key'];
export type Stack = Record<SeriesKey, number>;

/** 一个桶自下而上的各段:静默重置与空回复合成「没有回复」。 */
export function stackOf(b: UsageBucket): Stack {
  return { done: b.done, quiet: b.empty + b.silent_reset, interrupted: b.interrupted, failed: b.failed };
}

export function stackTotal(s: Stack): number {
  return s.done + s.quiet + s.interrupted + s.failed;
}

/** 桶在横轴上的短标签。直接取服务端那个时区的字面值,与分桶的日历对得上。 */
export function bucketLabel(start: string, unit: UsageUnit): string {
  const month = Number(start.slice(5, 7));
  const day = Number(start.slice(8, 10));
  const hour = Number(start.slice(11, 13));
  return unit === 'hour' ? translate('d42fbea', [hour]) : `${month}/${day}`;
}

/** 浮层与读屏用的完整说法。 */
export function bucketTitle(start: string, unit: UsageUnit): string {
  const month = Number(start.slice(5, 7));
  const day = Number(start.slice(8, 10));
  const hour = Number(start.slice(11, 13));
  return unit === 'hour' ? translate('d616570', [month, day, hour, hour + 1]) : translate('dd330a4', [month, day]);
}

const PLOT_H = 180;
const TOP = 8;
const AXIS_H = 24;
const LEFT = 32;
const GAP = 2;

class UsageChart extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = { buckets: { attribute: false }, unit: { attribute: false } };
  declare buckets: UsageBucket[];
  declare unit: UsageUnit;
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
    const slot = (this.width - LEFT) / Math.max(1, this.buckets.length);
    const cx = LEFT + slot * this.hover + slot / 2;
    const left = Math.min(Math.max(cx - tip.offsetWidth / 2, 0), this.width - tip.offsetWidth);
    tip.style.left = `${left}px`;
  }

  private column(b: UsageBucket, i: number, slot: number, y: (v: number) => number): TemplateResult {
    const s = stackOf(b);
    const w = Math.max(3, Math.min(24, slot * 0.62));
    const x = LEFT + slot * i + (slot - w) / 2;
    const shown = SERIES.filter((d) => s[d.key] > 0);
    let base = 0;
    const parts = shown.map((d, k) => {
      const top = y(base + s[d.key]);
      const bottom = y(base) - (base > 0 ? GAP : 0);
      base += s[d.key];
      const h = Math.max(1, bottom - top);
      return k === shown.length - 1
        ? svg`<path class="uc-seg uc-${d.key}" d=${roundTop(x, top, w, h)}></path>`
        : svg`<rect class="uc-seg uc-${d.key}" x=${x} y=${top} width=${w} height=${h}></rect>`;
    });
    const dim = this.hover !== null && this.hover !== i;
    return svg`<g class="uc-col ${dim ? 'is-dim' : ''}">${parts}</g>`;
  }

  private tooltip(): TemplateResult | typeof nothing {
    if (this.hover === null) return nothing;
    const b = this.buckets[this.hover];
    if (!b) return nothing;
    const s = stackOf(b);
    return html`<div class="uc-tip" role="status">
      <div class="uc-tip-head">${rich('d147384', [bucketTitle(b.start, this.unit), stackTotal(s)])}</div>
      ${SERIES.map(
        (d) => html`<div class="uc-tip-row"><span class="uc-key uc-key-${d.key}" aria-hidden="true"></span><strong>${s[d.key]}</strong><span>${d.label}</span></div>`,
      )}
    </div>`;
  }

  private table(): TemplateResult {
    return html`<details class="uc-table">
      <summary>${rich('d9cee74')}</summary>
      <div class="table-scroll">
        <table class="data-table">
          <thead>
            <tr>
              <th scope="col">${this.unit === 'hour' ? translate('dac3ff4') : translate('d70d0c1')}</th>
              ${SERIES.map((d) => html`<th scope="col" class="num">${d.label}</th>`)}
              <th scope="col" class="num">${rich('de009c1')}</th>
            </tr>
          </thead>
          <tbody>
            ${this.buckets.map((b) => {
              const s = stackOf(b);
              return html`<tr>
                <th scope="row">${bucketTitle(b.start, this.unit)}</th>
                ${SERIES.map((d) => html`<td class="num">${s[d.key]}</td>`)}
                <td class="num">${stackTotal(s)}</td>
              </tr>`;
            })}
          </tbody>
        </table>
      </div>
    </details>`;
  }

  protected override render(): TemplateResult {
    const buckets = this.buckets ?? [];
    const plotW = Math.max(120, this.width - LEFT);
    const ticks = niceTicks(Math.max(0, ...buckets.map((b) => stackTotal(stackOf(b)))));
    const top = ticks[ticks.length - 1]!;
    const y = (v: number) => TOP + PLOT_H - (v / top) * PLOT_H;
    const slot = plotW / Math.max(1, buckets.length);
    // 横轴标签至少隔 44px,从最近的一根往回数,最近的那一根总有标签。
    const every = Math.max(1, Math.ceil(44 / slot));
    const height = TOP + PLOT_H + AXIS_H;
    return html`<div class="uc">
      <div class="uc-legend">
        ${SERIES.map((d) => html`<span class="uc-legend-item"><span class="uc-swatch uc-key-${d.key}" aria-hidden="true"></span>${d.label}</span>`)}
      </div>
      <div class="uc-plot" @pointerleave=${() => this.setHover(null)}>
        <svg class="uc-svg" width=${this.width} height=${height} role="group" aria-label="${translate('d3774a0')}">
          ${ticks.map(
            (t) => svg`<line class="uc-grid" x1=${LEFT} x2=${LEFT + plotW} y1=${y(t)} y2=${y(t)}></line>
              <text class="uc-tick" x=${LEFT - 6} y=${y(t)} dy="0.32em" text-anchor="end">${t}</text>`,
          )}
          ${buckets.map((b, i) => this.column(b, i, slot, y))}
          ${buckets.map((b, i) =>
            (buckets.length - 1 - i) % every === 0
              ? svg`<text class="uc-tick" x=${LEFT + slot * i + slot / 2} y=${TOP + PLOT_H + 16} text-anchor="middle">${bucketLabel(b.start, this.unit)}</text>`
              : nothing,
          )}
          ${buckets.map((b, i) => {
            const s = stackOf(b);
            const label = translate('d0aa198', [bucketTitle(b.start, this.unit), stackTotal(s), SERIES.map((d) => `${d.label} ${s[d.key]}`).join('，')]);
            return svg`<rect class="uc-hit" x=${LEFT + slot * i} y=${TOP} width=${slot} height=${PLOT_H} tabindex="0" role="img" aria-label=${label}
              @pointerenter=${() => this.setHover(i)} @pointermove=${() => this.setHover(i)}
              @focus=${() => this.setHover(i)} @blur=${() => this.setHover(null)}></rect>`;
          })}
        </svg>
        ${this.tooltip()}
      </div>
      ${this.table()}
    </div>`;
  }
}

customElements.define('sm-usage-chart', UsageChart);
