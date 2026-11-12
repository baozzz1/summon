import { afterEach, expect, test } from 'vitest';
import { DateRangePicker, monthCells, shiftDate, validRange } from './date-range';

async function picker() {
  const p = new DateRangePicker();
  p.today = '2026-11-04'; p.query = { range: '7d' };
  document.body.append(p); await p.updateComplete;
  p.querySelector<HTMLButtonElement>('.date-range-trigger')!.click(); await p.updateComplete;
  return p;
}
function button(p: Element, label: string) {
  return [...p.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.getAttribute('aria-label') === label || b.textContent?.trim() === label)!;
}
afterEach(() => document.body.replaceChildren());

test('calendar_handles_leap_days_and_year_boundaries', () => {
  expect(shiftDate('2026-01-01', -1)).toBe('2025-12-31');
  expect(monthCells('2024-02')).toContain('2024-02-29');
  expect(validRange('2026-02-30', '2026-03-01', '2026-11-04')).toBe(false);
  expect(validRange('2026-11-04', '2026-11-04', '2026-11-04')).toBe(true);
  expect(validRange('2026-11-05', '2026-11-04', '2026-11-04')).toBe(false);
  expect(validRange('2026-11-04', '2026-11-05', '2026-11-04')).toBe(false);
});

test('presets_wait_for_confirmation_and_clear_disables_apply', async () => {
  const p = await picker(); const changes: unknown[] = [];
  p.addEventListener('range-change', (e) => changes.push((e as CustomEvent).detail));
  button(p, '90D').click(); await p.updateComplete;
  expect(p.querySelector<HTMLInputElement>('[aria-label="起始日期"]')!.value).toBe('2026-08-07');
  expect(changes).toEqual([]);
  button(p, '确定').click(); await p.updateComplete;
  expect(changes).toEqual([{ range: '90d' }]);
  p.querySelector<HTMLButtonElement>('.date-range-trigger')!.click(); await p.updateComplete;
  button(p, '清除').click(); await p.updateComplete;
  expect(button(p, '确定').disabled).toBe(true);
  button(p, 'ALL').click(); await p.updateComplete;
  button(p, '确定').click(); await p.updateComplete;
  expect(changes.at(-1)).toEqual({ range: 'all' });
});

test('selects_cross_month_range_in_either_direction_and_escape_discards_draft', async () => {
  const p = await picker(); const changes: unknown[] = [];
  p.addEventListener('range-change', (e) => changes.push((e as CustomEvent).detail));
  button(p, '2026-11-04').click(); await p.updateComplete;
  button(p, '上个月').click(); await p.updateComplete;
  button(p, '2026-10-29').click(); await p.updateComplete;
  button(p, '确定').click(); await p.updateComplete;
  expect(changes).toEqual([{ range: 'custom', from: '2026-10-29', to: '2026-11-04' }]);
  p.querySelector<HTMLButtonElement>('.date-range-trigger')!.click(); await p.updateComplete;
  button(p, '上一年').click(); await p.updateComplete;
  expect(p.querySelector('.date-month-nav strong')!.textContent).toBe('2025年11月');
  button(p, '下一年').click(); await p.updateComplete;
  button(p, '30D').click(); await p.updateComplete;
  p.querySelector('.date-range-panel')!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); await p.updateComplete;
  expect(p.querySelector('.date-range-panel')).toBeNull();
  expect(changes).toHaveLength(1);
});

test('manual_dates_validate_and_future_days_cannot_be_picked', async () => {
  const p = await picker();
  expect(button(p, '2026-11-05').disabled).toBe(true);
  const from = p.querySelector<HTMLInputElement>('[aria-label="起始日期"]')!;
  from.value = '2026-11-05'; from.dispatchEvent(new Event('input')); await p.updateComplete;
  expect(button(p, '确定').disabled).toBe(true);
  from.value = '2026-10-01'; from.dispatchEvent(new Event('input')); await p.updateComplete;
  expect(button(p, '确定').disabled).toBe(false);
});


test('escape_on_the_trigger_closes_the_panel_without_applying', async () => {
  const p = await picker();
  p.querySelector('.date-range-trigger')!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
  await p.updateComplete;
  expect(p.querySelector('.date-range-panel')).toBeNull();
});


test('relative_presets_and_custom_durations_emit_hours_only_after_apply', async () => {
  const p = await picker(); const changes: unknown[] = [];
  p.addEventListener('range-change', (e) => changes.push((e as CustomEvent).detail));
  button(p, '过去 24 小时').click(); await p.updateComplete;
  expect(changes).toEqual([]);
  expect(p.querySelector<HTMLInputElement>('input[type="date"]')!.disabled).toBe(true);
  button(p, '确定').click(); await p.updateComplete;
  expect(changes).toEqual([{ range: 'relative', hours: 24 }]);
  p.query = { range: 'relative', hours: 24 };
  await p.updateComplete;
  expect(p.querySelector('.date-range-trigger')!.textContent).toContain('过去 24 小时');
  button(p, '过去 24 小时▾').click(); await p.updateComplete;
  button(p, '过去 7 天').click(); await p.updateComplete;
  const amount = p.querySelector<HTMLInputElement>('input[type="number"]')!;
  const unit = p.querySelector<HTMLSelectElement>('select')!;
  expect(amount.value).toBe('7'); expect(unit.value).toBe('day');
  amount.value = '3'; amount.dispatchEvent(new Event('input')); await p.updateComplete;
  button(p, '确定').click(); await p.updateComplete;
  expect(changes.at(-1)).toEqual({ range: 'relative', hours: 72 });
});

test('relative_duration_validation_and_escape_preserve_the_applied_selection', async () => {
  const p = await picker(); const changes: unknown[] = [];
  p.addEventListener('range-change', (e) => changes.push((e as CustomEvent).detail));
  const input = p.querySelector<HTMLInputElement>('input[type="number"]')!;
  for (const value of ['', '0', '-1', '1.5', '4294967295']) {
    input.value = value; input.dispatchEvent(new Event('input')); await p.updateComplete;
    expect(button(p, '确定').disabled, value).toBe(true);
  }
  input.value = '5'; input.dispatchEvent(new Event('input')); await p.updateComplete;
  expect(button(p, '确定').disabled).toBe(false);
  p.querySelector('.date-range-panel')!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); await p.updateComplete;
  expect(changes).toEqual([]);
  p.querySelector<HTMLButtonElement>('.date-range-trigger')!.click(); await p.updateComplete;
  expect(p.querySelector('.date-presets .is-selected')!.textContent).toBe('7D');
});
