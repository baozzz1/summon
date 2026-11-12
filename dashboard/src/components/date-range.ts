import { LocaleController } from '../i18n';
import { t as translate, rich } from '../i18n';
import { html, LitElement, nothing, type TemplateResult } from 'lit';
import type { UsageQuery, UsageRange } from '../api/types';

export const PRESETS: Array<{ value: UsageRange; label: string; days?: number }> = [
  { value: 'today', label: '1D', days: 1 }, { value: '7d', label: '7D', days: 7 },
  { value: '30d', label: '30D', days: 30 }, { value: '90d', label: '90D', days: 90 },
  { value: 'all', label: 'ALL' },
];

export function dateKey(date: Date): string { return date.toISOString().slice(0, 10); }
export function shiftDate(date: string, days: number): string {
  const d = new Date(`${date}T12:00:00Z`);
  d.setUTCDate(d.getUTCDate() + days);
  return dateKey(d);
}
export function validDate(date: string): boolean {
  const d = new Date(`${date}T12:00:00Z`);
  return /^\d{4}-\d{2}-\d{2}$/.test(date) && Number.isFinite(d.getTime()) && dateKey(d) === date;
}
export function validRange(from: string, to: string, today: string): boolean {
  return validDate(from) && validDate(to) && from >= '1970-01-01' && from <= to && to <= today;
}
export function monthCells(month: string): string[] {
  const first = `${month}-01`;
  const weekday = new Date(`${first}T12:00:00Z`).getUTCDay();
  return Array.from({ length: 42 }, (_, i) => shiftDate(first, i - weekday));
}

export class DateRangePicker extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = { query: { attribute: false }, today: {}, since: {} };
  query: UsageQuery = { range: '7d' };
  today = dateKey(new Date());
  since = '';
  private open = false;
  private draft: UsageRange = '7d';
  private amount = 24;
  private relativeUnit: 'hour' | 'day' = 'hour';
  private get hours(): number { return this.amount * (this.relativeUnit === 'day' ? 24 : 1); }
  private get validRelative(): boolean { return Number.isInteger(this.amount) && this.amount > 0 && this.hours <= Math.floor(Date.now() / 3_600_000); }
  private relative(amount: number, unit: 'hour' | 'day'): void {
    this.amount = amount; this.relativeUnit = unit; this.draft = 'relative'; this.from = ''; this.to = ''; this.pickingEnd = false; this.hover = ''; this.requestUpdate();
  }
  private from = '';
  private to = '';
  private month = this.today.slice(0, 7);
  private pickingEnd = false;
  private hover = '';
  protected override createRenderRoot(): HTMLElement { return this; }
  private escape = (event: KeyboardEvent) => {
    if (this.open && event.key === 'Escape') { event.stopPropagation(); this.close(); }
  };
  private outside = (event: Event) => {
    if (this.open && !event.composedPath().includes(this)) this.close(false);
  };
  override connectedCallback(): void { super.connectedCallback(); document.addEventListener('click', this.outside); this.addEventListener('keydown', this.escape); }
  override disconnectedCallback(): void { document.removeEventListener('click', this.outside); this.removeEventListener('keydown', this.escape); super.disconnectedCallback(); }
  private close(focus = true): void {
    this.open = false; this.requestUpdate();
    if (focus) this.querySelector<HTMLButtonElement>('.date-range-trigger')?.focus();
  }
  private bounds(range: UsageRange): [string, string] {
    const days = PRESETS.find((p) => p.value === range)?.days;
    if (range === 'relative') return ['', ''];
    return range === 'custom' ? [this.query.from ?? '', this.query.to ?? '']
      : [days ? shiftDate(this.today, 1 - days) : this.query.range === 'all' ? this.since.slice(0, 10) : '', this.today];
  }
  private toggle(): void {
    if (this.open) { this.close(); return; }
    this.draft = this.query.range;
    const hours = this.query.hours ?? 24;
    this.relativeUnit = hours > 24 && hours % 24 === 0 ? 'day' : 'hour';
    this.amount = this.relativeUnit === 'day' ? hours / 24 : hours;
    [this.from, this.to] = this.bounds(this.draft);
    this.month = (this.to || this.today).slice(0, 7);
    this.pickingEnd = false; this.hover = ''; this.open = true; this.requestUpdate();
  }
  private preset(range: UsageRange): void {
    this.draft = range; [this.from, this.to] = this.bounds(range);
    this.month = this.to.slice(0, 7); this.pickingEnd = false; this.hover = ''; this.requestUpdate();
  }
  private pick(date: string): void {
    this.draft = 'custom';
    if (!this.pickingEnd) { this.from = date; this.to = ''; this.pickingEnd = true; }
    else {
      [this.from, this.to] = [this.from, date].sort() as [string, string];
      this.pickingEnd = false;
    }
    this.hover = ''; this.requestUpdate();
  }
  private move(months: number): void {
    const d = new Date(`${this.month}-01T12:00:00Z`);
    d.setUTCMonth(d.getUTCMonth() + months);
    const next = dateKey(d).slice(0, 7);
    this.month = next < '1970-01' ? '1970-01' : next > this.today.slice(0, 7) ? this.today.slice(0, 7) : next;
    this.requestUpdate();
  }
  private input(which: 'from' | 'to', event: Event): void {
    this[which] = (event.target as HTMLInputElement).value;
    this.draft = 'custom'; this.pickingEnd = false; this.hover = '';
    if (validDate(this[which]) && this[which] >= '1970-01-01' && this[which] <= this.today) this.month = this[which].slice(0, 7);
    this.requestUpdate();
  }
  private apply(): void {
    if (this.draft === 'relative' ? !this.validRelative : this.draft !== 'all' && !validRange(this.from, this.to, this.today)) return;
    const query: UsageQuery = this.draft === 'relative' ? { range: 'relative', hours: this.hours } : this.draft === 'custom' ? { range: 'custom', from: this.from, to: this.to } : { range: this.draft };
    this.dispatchEvent(new CustomEvent('range-change', { detail: query, bubbles: true }));
    this.close();
  }
  protected override render(): TemplateResult {
    const [start, end] = this.bounds(this.query.range);
    const hours = this.query.hours ?? 24;
    const label = this.query.range === 'relative' ? translate(hours > 24 && hours % 24 === 0 ? 'range.pastDays' : 'range.pastHours', [hours > 24 && hours % 24 === 0 ? hours / 24 : hours]) : this.query.range === 'all' ? translate('de42579') : `${start} ～ ${end}`;
    const preview = this.pickingEnd && this.hover ? [this.from, this.hover].sort() : [this.from, this.to];
    const valid = this.draft === 'relative' ? this.validRelative : this.draft === 'all' || validRange(this.from, this.to, this.today);
    return html`<button class="btn date-range-trigger" type="button" aria-expanded=${this.open} @click=${() => this.toggle()}>${label}<span aria-hidden="true">▾</span></button>
      ${this.open ? html`<div class="date-range-panel" role="group" aria-label="${translate('d5b378e')}">
        <div class="date-relative" role="group" aria-label=${translate('range.relative')}>
          <div class="date-relative-presets">${[24, 168].map((h) => html`<button type="button" class="btn btn-quiet ${this.draft === 'relative' && this.hours === h ? 'is-selected' : ''}" aria-pressed=${this.draft === 'relative' && this.hours === h} @click=${() => this.relative(h === 24 ? 24 : 7, h === 24 ? 'hour' : 'day')}>${translate(h === 24 ? 'range.pastHours' : 'range.pastDays', [h === 24 ? 24 : 7])}</button>`)}</div>
          <label>${translate('range.past')}<input class="field-input" type="number" min="1" step="1" aria-label=${translate('range.amount')} .value=${String(this.amount)} @input=${(e: Event) => this.relative((e.target as HTMLInputElement).valueAsNumber, this.relativeUnit)}></label>
          <select class="field-input" aria-label=${translate('range.unit')} .value=${this.relativeUnit} @change=${(e: Event) => this.relative(this.amount, (e.target as HTMLSelectElement).value as 'hour' | 'day')}>
            <option value="hour">${translate('range.hours')}</option><option value="day">${translate('range.days')}</option>
          </select>
        </div>
        <div class="date-range-body">
          <div class="date-presets" role="group" aria-label="${translate('d8631c8')}">${PRESETS.map((p) => html`<button type="button" class=${this.draft === p.value ? 'is-selected' : ''} aria-pressed=${this.draft === p.value} title=${p.value === 'all' ? translate('da117e1') : translate('d570a76', [p.days])} @click=${() => this.preset(p.value)}>${p.label}</button>`)}</div>
          <div class="date-calendar">
            <div class="date-month-nav">
              <button type="button" aria-label="${translate('d43cecd')}" ?disabled=${this.month === '1970-01'} @click=${() => this.move(-12)}>«</button>
              <button type="button" aria-label="${translate('df9e727')}" ?disabled=${this.month === '1970-01'} @click=${() => this.move(-1)}>‹</button>
              <strong aria-live="polite">${rich('de3b88e', [this.month.slice(0, 4), Number(this.month.slice(5))])}</strong>
              <button type="button" aria-label="${translate('d3e0b63')}" ?disabled=${this.month >= this.today.slice(0, 7)} @click=${() => this.move(1)}>›</button>
              <button type="button" aria-label="${translate('d30fd49')}" ?disabled=${this.month >= this.today.slice(0, 7)} @click=${() => this.move(12)}>»</button>
            </div>
            <div class="date-grid">${[...translate('d006148')].map((d) => html`<span class="date-weekday">${d}</span>`)}
              ${monthCells(this.month).map((date) => {
                const edge = date === this.from || date === this.to;
                const inside = !!preview[0] && !!preview[1] && date >= preview[0] && date <= preview[1];
                return html`<button type="button" class=${`${date.slice(0, 7) !== this.month ? 'date-adjacent' : ''} ${inside ? 'date-in-range' : ''} ${edge ? 'is-selected' : ''}`}
                  aria-label=${date} aria-pressed=${edge} aria-current=${date === this.today ? 'date' : nothing} ?disabled=${date > this.today || date < '1970-01-01'}
                  @mouseenter=${() => { if (this.pickingEnd) { this.hover = date; this.requestUpdate(); } }} @click=${() => this.pick(date)}>${Number(date.slice(8))}</button>`;
              })}
            </div>
          </div>
        </div>
        <div class="date-range-footer">
          <label><span class="sr-only">${rich('d43f261')}</span><input class="field-input" type="date" ?disabled=${this.draft === 'relative'} aria-label="${translate('d43f261')}" min="1970-01-01" max=${this.today} .value=${this.from} @input=${(e: Event) => this.input('from', e)}></label>
          <span>～</span>
          <label><span class="sr-only">${rich('d318024')}</span><input class="field-input" type="date" ?disabled=${this.draft === 'relative'} aria-label="${translate('d318024')}" min="1970-01-01" max=${this.today} .value=${this.to} @input=${(e: Event) => this.input('to', e)}></label>
          <button class="btn btn-quiet" type="button" @click=${() => { this.draft = 'custom'; this.from = ''; this.to = ''; this.hover = ''; this.pickingEnd = false; this.requestUpdate(); }}>${rich('dbce237')}</button>
          <button class="btn btn-primary" type="button" ?disabled=${!valid} @click=${() => this.apply()}>${rich('dfac2a6')}</button>
        </div>
        <p class="date-range-hint" role="status">${this.draft === 'relative' ? translate(valid ? 'range.hint' : 'range.invalid') : this.draft === 'all' ? translate('d57908f') : !this.from || !this.to ? translate('d0fb866') : !valid ? translate('d2a5f0e') : translate('d749437')}</p>
      </div>` : nothing}`;
  }
}
customElements.define('sm-date-range', DateRangePicker);
