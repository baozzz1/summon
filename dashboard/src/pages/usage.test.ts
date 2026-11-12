import { describe, expect, test, vi } from 'vitest';
import usageFixture from '../api/fixtures/usage.summary.json';
import type { UsageBucket, UsageView } from '../api/types';
import { hrefFor, parseRoute } from '../app/router';
import { bucketLabel, bucketTitle, niceTicks, stackOf, stackTotal } from '../components/usage-chart';
import { fmtCount, fmtUsd } from '../util/format';
import { costText, partText, spendText, spendTitle, tokenTotal, unpricedModels } from '../util/spend';
import { dayLabel, pct, resetPassed, resetText, UsagePage } from './usage';

describe('用量页', () => {
  test('usage_route_round_trips', () => {
    expect(parseRoute('/usage')).toEqual({ page: 'usage' });
    expect(hrefFor({ page: 'usage' })).toBe('/usage');
  });

  test('ticks_are_clean_integers_covering_the_max', () => {
    expect(niceTicks(0)).toEqual([0, 1]);
    expect(niceTicks(1)).toEqual([0, 1]);
    expect(niceTicks(7)).toEqual([0, 2, 4, 6, 8]);
    expect(niceTicks(13)).toEqual([0, 5, 10, 15]);
    expect(niceTicks(240)).toEqual([0, 100, 200, 300]);
  });

  test('buckets_are_labelled_with_the_servers_own_calendar', () => {
    expect(bucketLabel('2026-10-21T09:00:00+08:00', 'hour')).toBe('9时');
    expect(bucketLabel('2026-10-21T00:00:00+08:00', 'day')).toBe('10/21');
    expect(bucketTitle('2026-10-21T09:00:00+08:00', 'hour')).toBe('10月21日 9:00–10:00');
    expect(bucketTitle('2026-10-21T00:00:00-07:00', 'day')).toBe('10月21日');
  });

  test('quiet_turns_fold_empty_and_silent_reset', () => {
    const b: UsageBucket = { start: '', done: 3, empty: 1, silent_reset: 2, failed: 1, interrupted: 0 };
    expect(stackOf(b)).toEqual({ done: 3, quiet: 3, interrupted: 0, failed: 1 });
    expect(stackTotal(stackOf(b))).toBe(7);
  });

  test.each([
    [0, '0'],
    [999, '999'],
    [1_000, '1K'],
    [1_049, '1K'],
    [1_050, '1.1K'],
    [1_150, '1.2K'],
    [1_250, '1.3K'],
    [9_999, '10K'],
    [12_345, '12.3K'],
    [120_000, '120K'],
    [123_456, '123.5K'],
    [999_949, '999.9K'],
    [999_999, '1000K'],
    [1_000_000, '1M'],
    [1_250_000, '1.3M'],
    [12_345_678, '12.3M'],
    [420_000_000, '420M'],
    [999_999_999, '1000M'],
    [1_000_000_000, '1B'],
    [1_250_000_000, '1.3B'],
    [4_200_000_000, '4.2B'],
  ])('formats_count_%i_as_%s', (n, expected) => {
    expect(fmtCount(n)).toBe(expected);
  });

  test('numbers_read_naturally', () => {
    expect(pct(0, 0)).toBe('—');
    expect(pct(1, 300)).toBe('<1%');
    expect(pct(3, 12)).toBe('25%');
    expect(fmtCount(1284)).toBe('1.3K');
    expect(fmtCount(129_000)).toBe('129K');
    expect(fmtCount(30_000)).toBe('30K');
    expect(fmtUsd(0)).toBe('$0');
    expect(fmtUsd(0.36000000000000004)).toBe('$0.36');
    expect(fmtUsd(0.0001234)).toBe('$0.000123');
    expect(fmtUsd(1.2216)).toBe('$1.22');
    expect(fmtUsd(1234.5)).toBe('$1,234.50');
    const now = new Date('2026-10-21T10:00:00+08:00');
    expect(resetText(undefined, now)).toBe('');
    expect(resetText(now.getTime() / 1000 - 60, now)).toBe('读数后已过重置时间');
    expect(resetPassed(now.getTime() / 1000 - 60, now)).toBe(true);
    expect(resetPassed(now.getTime() / 1000 + 60, now)).toBe(false);
    expect(resetPassed(undefined, now)).toBe(false);
  });

  test('costs_say_when_part_of_them_is_unpriced', () => {
    const tokens = { input: 100, output: 50, cache_read: 800, cache_write: 50 };
    const cost = { total: 0.5, input: 0.1, output: 0.2, cache_read: 0.1, cache_write: 0.1 };
    expect(tokenTotal(tokens)).toBe(1000);
    expect(costText(undefined)).toBe('—');
    expect(costText({ tokens, cost })).toBe('$0.5');
    expect(costText({ tokens, cost, unpriced: { tokens: 200, models: ['composer-2'] } })).toBe('$0.5 起');
    const none = { tokens, cost: { total: 0, input: 0, output: 0, cache_read: 0, cache_write: 0 }, unpriced: { tokens: 1000, models: [''] } };
    expect(costText(none)).toBe('未计价');
    expect(partText(none, 'output')).toBe('未计价');
    // 分项与合计同一口径:有未计价的部分时同样标「起」
    expect(partText({ tokens, cost }, 'output')).toBe('$0.2');
    expect(partText({ tokens, cost, unpriced: { tokens: 200, models: ['composer-2'] } }, 'output')).toBe('$0.2 起');
    expect(unpricedModels(['', 'composer-2'])).toBe('未知模型、composer-2');
    expect(spendText({ tokens, cost })).toBe('1K token · $0.5');
    expect(spendTitle({ tokens, cost, unpriced: { tokens: 200, models: ['composer-2'] } }).split('\n')).toEqual([
      '无缓存输入 $0.1 起（100 token）',
      '缓存读取 $0.1 起（800 token）',
      '缓存写入 $0.1 起（50 token）',
      '输出 $0.2 起（50 token）',
      '未计价 200 token（composer-2）',
    ]);
  });

  test('days_read_on_the_servers_calendar', () => {
    expect(dayLabel('2026-10-21T00:00:00+08:00')).toBe('10月21日 周三');
    expect(dayLabel('2026-10-18T00:00:00-07:00')).toBe('10月18日 周日');
    expect(dayLabel('bad')).toBe('bad');
  });

  test('the_contract_sample_prices_its_turn_and_names_the_unpriced_model', () => {
    const v = (usageFixture as unknown as { data: UsageView }).data;
    const s = v.totals.spend!;
    expect(s.cost.total).toBeCloseTo(s.cost.input + s.cost.output + s.cost.cache_read + s.cost.cache_write, 9);
    expect(s.unpriced?.models).toEqual(['composer-2']);
    expect(v.days).toHaveLength(1);
    expect(v.prices?.source).toContain('litellm');
  });

  test('the_contract_sample_counts_outside_usage_in_cost_but_not_in_turns', () => {
    const v = (usageFixture as unknown as { data: UsageView }).data;
    const outside = v.totals.outside!;
    expect(tokenTotal(outside.tokens)).toBeGreaterThan(0);
    expect(v.totals.spend!.tokens.output).toBeGreaterThan(outside.tokens.output);
    expect(v.totals.turns).toBe(1);
    expect(v.by_chat[0]!.tag).toBeTruthy();
  });

  test('the_contract_sample_has_a_bucket_per_day', () => {
    const v = (usageFixture as unknown as { data: UsageView }).data;
    expect(v.buckets).toHaveLength(7);
    expect(v.by_agent[0]!.label).toBe('星光');
    expect(v.totals.turns).toBe(stackTotal(stackOf(v.buckets[6]!)));
  });
});

async function tablePage() {
  const page = document.createElement('sm-page-usage') as import('./usage').UsagePage;
  page.view = structuredClone((usageFixture as unknown as { data: UsageView }).data);
  page.loading = false;
  page.refresh = async () => {};
  document.body.append(page);
  await page.updateComplete;
  return page;
}

test('all_columns_sort_both_ways_with_independent_table_state', async () => {
  const page = await tablePage();
  const base = page.view!.by_agent[0]!;
  page.view!.by_agent = [
    { ...base, key: 'z', label: 'Zulu', agent: undefined, turns: 9, avg_secs: 100, p90_secs: 200, failed: 1 },
    { ...base, key: 'a', label: 'Alpha', agent: undefined, turns: 20, avg_secs: 20, p90_secs: 50, failed: 10 },
  ];
  page.requestUpdate();
  await page.updateComplete;
  try {
    for (const id of ['us-table', 'us-cost-table']) {
      const section = page.querySelector(`[aria-labelledby="${id}"]`)!;
      const headers = [...section.querySelectorAll('thead th')];
      for (let i = 0; i < headers.length; i++) {
        const header = () => section.querySelectorAll('thead th')[i]!;
        expect(header().querySelector('button'), `${id} column ${i}`).not.toBeNull();
        header().querySelector<HTMLButtonElement>('button')!.click();
        await page.updateComplete;
        const first = header().getAttribute('aria-sort');
        expect(['ascending', 'descending']).toContain(first);
        header().querySelector<HTMLButtonElement>('button')!.click();
        await page.updateComplete;
        expect(header().getAttribute('aria-sort')).toBe(first === 'ascending' ? 'descending' : 'ascending');
      }
    }
    const stat = page.querySelector('[aria-labelledby="us-table"]')!;
    const cost = page.querySelector('[aria-labelledby="us-cost-table"]')!;
    const active = () => stat.querySelector('[aria-sort="ascending"], [aria-sort="descending"]')!.textContent;
    const statSort = active();
    cost.querySelector<HTMLButtonElement>('thead button')!.click();
    await page.updateComplete;
    expect(active()).toBe(statSort);
    stat.querySelector<HTMLButtonElement>('thead th:nth-child(2) button')!.click();
    await page.updateComplete;
    const names = () => [...stat.querySelectorAll('tbody th')].map((e) => e.textContent?.trim());
    expect(names()).toEqual(['Alpha', 'Zulu']);
    stat.querySelector<HTMLButtonElement>('thead th:nth-child(2) button')!.click();
    await page.updateComplete;
    expect(names()).toEqual(['Zulu', 'Alpha']);
  } finally { page.remove(); }
});

test('both_tables_offer_models_and_explain_multi_model_turns', async () => {
  const page = await tablePage();
  try {
    for (const id of ['us-table', 'us-cost-table']) {
      const section = page.querySelector(`[aria-labelledby="${id}"]`)!;
      const button = section.querySelector<HTMLInputElement>('input[value="model"]');
      expect(button).toBeDefined();
      button!.click();
      await page.updateComplete;
      expect(section.querySelector('thead th')!.textContent).toContain('模型');
      expect(section.querySelector('tbody')!.textContent).toContain('composer-2');
    }
    expect(page.querySelector('[aria-labelledby="us-table"]')!.textContent).toContain('一轮涉及多个模型');
  } finally { page.remove(); }
});

test('statistics_sort_raw_durations_failure_ratios_and_known_costs', async () => {
  const page = await tablePage();
  const base = page.view!.by_agent[0]!;
  const spend = (value: number) => ({ tokens: { input: 1, output: 0, cache_read: 0, cache_write: 0 }, cost: { total: value, input: value, output: 0, cache_read: 0, cache_write: 0 } });
  page.view!.by_agent = [
    { ...base, key: 'a', label: 'Alpha', agent: undefined, turns: 100, avg_secs: 100, p90_secs: 90, failed: 2, spend: spend(0.1) },
    { ...base, key: 'b', label: 'Beta', agent: undefined, turns: 2, avg_secs: 9, p90_secs: 200, failed: 1, spend: spend(0.01) },
    { ...base, key: 'c', label: 'Missing', agent: undefined, turns: 0, avg_secs: 0, p90_secs: 0, failed: 0, spend: undefined },
  ];
  page.requestUpdate();
  await page.updateComplete;
  try {
    const section = page.querySelector('[aria-labelledby="us-table"]')!;
    for (const [column, names] of [[3, ['Alpha', 'Beta', 'Missing']], [4, ['Beta', 'Alpha', 'Missing']], [5, ['Beta', 'Alpha', 'Missing']], [6, ['Alpha', 'Beta', 'Missing']]] as const) {
      section.querySelector<HTMLButtonElement>(`thead th:nth-child(${column}) button`)!.click();
      await page.updateComplete;
      const actual = () => [...section.querySelectorAll('tbody th')].map((e) => e.textContent?.trim());
      expect(actual()).toEqual(names);
      section.querySelector<HTMLButtonElement>(`thead th:nth-child(${column}) button`)!.click();
      await page.updateComplete;
      expect(actual()).toEqual([names[1], names[0], names[2]]);
    }
  } finally { page.remove(); }
});

test('cost_columns_sort_amounts_and_keep_sort_when_changing_dimension_or_range', async () => {
  const page = await tablePage();
  const spend = (n: number) => ({ tokens: { input: n, output: n, cache_read: n, cache_write: n }, cost: { total: n, input: n, output: n, cache_read: n, cache_write: n } });
  page.view!.days = [
    { start: '2026-12-31T00:00:00+08:00', turns: 1, spend: spend(9) },
    { start: '2027-01-01T00:00:00+08:00', turns: 1, spend: spend(100) },
    { start: '2027-01-02T00:00:00+08:00', turns: 1, spend: { ...spend(0), unpriced: { tokens: 0, models: ['unknown'] } } },
  ];
  page.requestUpdate();
  await page.updateComplete;
  try {
    const section = page.querySelector('[aria-labelledby="us-cost-table"]')!;
    for (let col = 2; col <= 7; col++) {
      section.querySelector<HTMLButtonElement>(`thead th:nth-child(${col}) button`)!.click();
      await page.updateComplete;
      expect(section.querySelector('tbody th')!.textContent).toContain('1月1日');
      section.querySelector<HTMLButtonElement>(`thead th:nth-child(${col}) button`)!.click();
      await page.updateComplete;
      expect(section.querySelector('tbody th')!.textContent).toContain(col === 3 ? '1月2日' : '12月31日');
    }
    const state = { ...page.costSort! };
    section.querySelector<HTMLInputElement>('input[value="model"]')!.click();
    await page.updateComplete;
    const picker = page.querySelector<import('../components/date-range').DateRangePicker>('sm-date-range')!;
    picker.querySelector<HTMLButtonElement>('.date-range-trigger')!.click();
    await picker.updateComplete;
    [...picker.querySelectorAll<HTMLButtonElement>('.date-presets button')].find((b) => b.textContent === '30D')!.click();
    await picker.updateComplete;
    picker.querySelector<HTMLButtonElement>('.btn-primary')!.click();
    await page.updateComplete;
    expect(page.costSort).toEqual(state);
    expect(page.costDim).toBe('model');
    expect(page.range).toBe('30d');
  } finally { page.remove(); }
});


test('latest_filter_request_wins_even_for_two_custom_ranges', async () => {
  const page = document.createElement('sm-page-usage') as import('./usage').UsagePage;
  const fixture = structuredClone((usageFixture as unknown as { data: UsageView }).data);
  let resolveOld!: (v: UsageView) => void;
  let resolveNew!: (v: UsageView) => void;
  const call = vi.spyOn(page, 'call')
    .mockReturnValueOnce(new Promise<UsageView>((r) => { resolveOld = r; }))
    .mockReturnValueOnce(new Promise<UsageView>((r) => { resolveNew = r; }));
  page.range = 'custom'; page.from = '2026-10-01'; page.to = '2026-10-02';
  const old = page.refresh();
  page.from = '2026-10-03'; page.to = '2026-10-04'; page.agent = 'starlight'; page.chatId = 'oc_example';
  const latest = page.refresh();
  resolveNew({ ...fixture, since: '2026-10-03' }); await latest;
  resolveOld(fixture); await old;
  expect(page.view!.since).toBe('2026-10-03');
  expect(call.mock.calls[1]).toEqual(['usage.summary', { range: 'custom', from: '2026-10-03', to: '2026-10-04', agent: 'starlight', chat_id: 'oc_example' }]);
  call.mockRejectedValueOnce(new Error('加载失败'));
  await page.refresh();
  expect(page.view).toBeNull();
  expect(page.loadError).toContain('加载失败');
});

test('agent_selection_limits_chat_choices_and_resets_the_previous_chat', async () => {
  const page = await tablePage();
  page.view!.scopes = [
    { agent: 'alpha', agent_label: 'Alpha', chat_id: 'first', chat_label: 'First' },
    { agent: 'beta', agent_label: 'Beta', chat_id: 'second', chat_label: 'Second' },
  ];
  page.chatId = 'first'; page.requestUpdate(); await page.updateComplete;
  const agents = page.querySelector<HTMLSelectElement>('[aria-label="筛选 agent"]')!;
  agents.value = 'beta'; agents.dispatchEvent(new Event('change'));
  page.requestUpdate(); await page.updateComplete;
  expect(page.agent).toBe('beta'); expect(page.chatId).toBe('');
  expect([...page.querySelector<HTMLSelectElement>('[aria-label="筛选群或会话"]')!.options].map((o) => o.value)).toEqual(['', 'second']);
  page.remove();
});


test('filter_selection_stays_visible_when_date_changes_shrink_or_reorder_scopes', async () => {
  const page = await tablePage();
  const alpha = { agent: 'alpha', agent_label: 'Alpha', chat_id: 'first', chat_label: 'First' };
  const beta = { agent: 'beta', agent_label: 'Beta', chat_id: 'second', chat_label: 'Second' };
  const gamma = { agent: 'beta', agent_label: 'Beta', chat_id: 'third', chat_label: 'Third' };
  const agents = () => page.querySelector<HTMLSelectElement>('[aria-label="筛选 agent"]')!;
  const chats = () => page.querySelector<HTMLSelectElement>('[aria-label="筛选群或会话"]')!;
  try {
    page.view!.scopes = [alpha, beta, gamma];
    page.requestUpdate(); await page.updateComplete;
    agents().value = 'beta'; agents().dispatchEvent(new Event('change'));
    page.requestUpdate(); await page.updateComplete;
    chats().value = 'third'; chats().dispatchEvent(new Event('change'));
    page.requestUpdate(); await page.updateComplete;
    for (const scopes of [[gamma], [], [alpha, gamma, beta], [alpha, beta, gamma]]) {
      page.view!.scopes = scopes;
      page.requestUpdate(); await page.updateComplete;
      expect(agents().value).toBe(page.agent);
      expect(agents().value).toBe('beta');
      expect(chats().value).toBe(page.chatId);
      expect(chats().value).toBe('third');
    }
    const call = vi.spyOn(page, 'call').mockResolvedValue(page.view!);
    await UsagePage.prototype.refresh.call(page);
    expect(call.mock.calls.at(-1)![1]).toMatchObject({ agent: 'beta', chat_id: 'third' });
  } finally { page.remove(); }
});


test('relative_range_refresh_keeps_duration_and_passes_filters_to_the_api', async () => {
  const page = document.createElement('sm-page-usage') as import('./usage').UsagePage;
  const call = vi.spyOn(page, 'call').mockResolvedValue(structuredClone((usageFixture as unknown as { data: UsageView }).data));
  document.body.append(page);
  await vi.waitFor(() => expect(page.view).not.toBeNull());
  await page.updateComplete;
  const picker = page.querySelector<import('../components/date-range').DateRangePicker>('sm-date-range')!;
  picker.dispatchEvent(new CustomEvent('range-change', { detail: { range: 'relative', hours: 5 }, bubbles: true }));
  await vi.waitFor(() => expect(page.range).toBe('relative'));
  page.agent = 'alpha';
  await page.refresh();
  expect(call.mock.calls.at(-1)).toEqual(['usage.summary', { range: 'relative', hours: 5, agent: 'alpha' }]);
  await page.refresh();
  expect(call.mock.calls.at(-1)).toEqual(['usage.summary', { range: 'relative', hours: 5, agent: 'alpha' }]);
  const restored = document.createElement('sm-page-usage') as import('./usage').UsagePage;
  expect(restored.range).toBe('relative'); expect(restored.hours).toBe(5);
  page.remove();
  localStorage.removeItem('summon-dash.usage.range');
  localStorage.removeItem('summon-dash.usage.range.hours');
});
