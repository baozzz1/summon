import { afterEach, describe, expect, test, vi } from 'vitest';
import { render, svg } from 'lit';
import { METHOD_GUARDS } from '../api/guards';
import { createMockTransport, MockServer } from '../api/mock';
import type { InsightsFinding, InsightsSummary, UsageRange } from '../api/types';
import { hrefFor, parseRoute } from '../app/router';
import { NAV_GROUPS, PAGE_TITLES } from '../app/nav';
import { setLanguage } from '../i18n';
import { barShape, dayTick } from '../components/insights-charts';
import { bucketRows, change, comparable, dayTitle, elsewhereRows, findingText, InsightsPage, longShares, nextRows, percent, ratio, shownFindings, span } from './insights';

afterEach(async () => {
  await setLanguage('zh-CN');
  document.body.replaceChildren();
});

/** 40 条间隔:四分之一以上落在 15 分钟以上,却占了九成以上的时长。 */
function sample(): InsightsSummary {
  return {
    range: '30d',
    from: '2026-10-12',
    to: '2026-11-10',
    turns: 70,
    human_turns: 60,
    agents: 2,
    chats: 5,
    day_start_hour: 9,
    day_end_hour: 22,
    gaps: { n: 40, p50_secs: 600, p75_secs: 1800, p90_secs: 5400, total_secs: 96_000 },
    gaps_including_overnight: 47,
    queued: 3,
    buckets: [
      { key: 'lt_1m', count: 8, secs: 200 },
      { key: 'm1_5', count: 10, secs: 1800 },
      { key: 'm5_15', count: 8, secs: 4000 },
      { key: 'm15_60', count: 8, secs: 14_000 },
      { key: 'h1_4', count: 5, secs: 36_000 },
      { key: 'gte_4h', count: 1, secs: 40_000 },
    ],
    agent_turns: { n: 50, p50_secs: 120, p75_secs: 200, p90_secs: 400, total_secs: 8000 },
    elsewhere: [
      { key: 'm15_60', count: 8, secs: 14_000, active_elsewhere: 6, elsewhere_messages_p50: 2 },
      { key: 'h1_4', count: 5, secs: 36_000, active_elsewhere: 4, elsewhere_messages_p50: 4.5 },
      { key: 'gte_4h', count: 1, secs: 40_000, active_elsewhere: 0, elsewhere_messages_p50: 0 },
    ],
    long_gaps: 14,
    long_gaps_silent: 3,
    next_messages: [
      { key: 'instruction', count: 24, p50_secs: 900, p90_secs: 6000 },
      { key: 'short', count: 10, p50_secs: 300, p90_secs: 1200 },
      { key: 'approve', count: 2, p50_secs: 100, p90_secs: 150 },
      { key: 'poll', count: 4, p50_secs: 2400, p90_secs: 4000 },
    ],
    days: [
      { date: '2026-11-08', weekday: 7, messages: 3, chats: 1 },
      { date: '2026-11-09', weekday: 1, messages: 12, chats: 4 },
      { date: '2026-11-10', weekday: 2, messages: 0, chats: 0 },
    ],
    workdays: { days: 1, messages_p50: 12, messages_p90: 12, chats_p50: 4, chats_p90: 4 },
    previous: { gaps: { n: 30, p50_secs: 400, p75_secs: 1200, p90_secs: 3600, total_secs: 60_000 }, over_15m_count_share: 0.3, over_15m_secs_share: 0.8 },
    findings: [
      { key: 'tail_concentration', params: { count_share: 0.35, secs_share: 0.9375 } },
      { key: 'human_vs_agent', params: { ratio: 12 } },
      { key: 'trend', params: { direction: 'up', p50_secs: 600, previous_p50_secs: 400 } },
    ],
  };
}

const ALL_FINDINGS: InsightsFinding[] = [
  { key: 'sample_small', params: { n: 12 } },
  { key: 'tail_concentration', params: { count_share: 0.35, secs_share: 0.9375 } },
  { key: 'busy_elsewhere', params: { share: 0.714, silent_share: 0.214 } },
  { key: 'mostly_instructions', params: { approve_share: 0.05, approve_p50_secs: 100 } },
  { key: 'polls', params: { count: 7, p50_secs: 2400 } },
  { key: 'load', params: { messages_p50: 12.5, chats_p50: 4 } },
  { key: 'human_vs_agent', params: { ratio: 12.04 } },
  { key: 'trend', params: { direction: 'up', p50_secs: 600, previous_p50_secs: 400 } },
  { key: 'trend', params: { direction: 'down', p50_secs: 300, previous_p50_secs: 600 } },
];

async function renderPage(view: InsightsSummary | null, error = ''): Promise<InsightsPage> {
  const page = document.createElement('sm-page-insights') as InsightsPage;
  page.refresh = async () => {};
  page.view = view;
  page.loading = false;
  page.loadError = error;
  document.body.append(page);
  await page.updateComplete;
  await Promise.all([...page.querySelectorAll<HTMLElement & { updateComplete: Promise<unknown> }>('sm-bar-list, sm-day-chart')].map((e) => e.updateComplete));
  return page;
}

const text = (el: Element | null | undefined) => (el?.textContent ?? '').replace(/\s+/g, ' ').trim();
const cells = (tr: Element) => [...tr.children].map(text).join(' | ');
const section = (page: Element, id: string) => page.querySelector(`[aria-labelledby="${id}"]`)!;

describe('洞察页', () => {
  test('insights_route_and_nav_entry_sit_next_to_usage', () => {
    expect(parseRoute('/insights')).toEqual({ page: 'insights' });
    expect(hrefFor({ page: 'insights' })).toBe('/insights');
    const ids = NAV_GROUPS.flatMap((g) => g.items.map((i) => i.id));
    expect(ids.indexOf('insights')).toBe(ids.indexOf('usage') + 1);
    // 可见范围与用量页相同:不是只给整机设置的人
    expect(NAV_GROUPS.flatMap((g) => g.items).find((i) => i.id === 'insights')!.machine).toBeUndefined();
    expect(PAGE_TITLES.insights).toBe('洞察');
  });

  test('numbers_read_naturally', () => {
    expect(percent(0)).toBe('0%');
    expect(percent(0.004)).toBe('<1%');
    expect(percent(0.9375)).toBe('94%');
    expect(ratio(12)).toBe('12');
    expect(ratio(6.49)).toBe('6.5');
    expect(change(600, 400)).toBe('+50%');
    expect(change(300, 400)).toBe('−25%');
    expect(change(400.1, 400)).toBe('持平');
    expect(change(5, 0)).toBe('');
    expect(span(0, 0)).toBe('—');
    expect(span(600)).toBe('10 分钟');
    expect(span(469)).toBe('7 分钟 49 秒');
    expect(span(2088)).toBe('35 分钟');
    expect(span(0.4)).toBe('少于 1 秒');
    expect(dayTick('2026-11-09')).toBe('11/9');
    expect(dayTitle({ date: '2026-11-09', weekday: 1, messages: 0, chats: 0 })).toBe('11月9日 周一');
    expect(dayTitle({ date: '2026-11-08', weekday: 7, messages: 0, chats: 0 })).toBe('11月8日 周日');
  });

  test('every_finding_reads_as_a_sentence_in_both_languages', async () => {
    const zh = ALL_FINDINGS.map(findingText);
    expect(zh).toEqual([
      '当日口径的间隔只有 12 条，不足 30 条，样本过少，暂不给出结论。可选择更长的时间段。',
      '15 分钟以上的间隔占条数的 35%，却占间隔总时长的 94%：时长主要集中在少数长间隔上。',
      '15 分钟以上的长间隔中，71% 期间其它会话有真人消息；期间所有会话基本无消息的只占 21%。',
      '下一条消息以新指令和短指令为主，放行类只占 5%（间隔中位数 1 分钟 40 秒）。',
      '下一条消息中有 7 条是询问进展，此前的间隔中位数为 40 分钟。',
      '有消息的工作日，每天消息数的中位数为 12.5 条，涉及会话数的中位数为 4 个。',
      '间隔合计是 agent 单轮耗时合计的 12 倍。',
      '间隔中位数较上一时段延长：6 分钟 40 秒 → 10 分钟。',
      '间隔中位数较上一时段缩短：10 分钟 → 5 分钟。',
    ]);
    await setLanguage('en');
    const en = ALL_FINDINGS.map(findingText);
    expect(en[0]).toBe('Only 12 same-day gaps (fewer than 30); the sample is too small for findings. Choose a longer period.');
    expect(en[1]).toBe('Gaps of 15 min or more are 35% of all gaps but 94% of total gap time: most of the time sits in a few long gaps.');
    expect(en[2]).toContain('71%');
    expect(en[3]).toContain('approvals are only 5%');
    expect(en[4]).toBe('7 of the next messages asked about progress; the gap before them has a median of 40 min.');
    expect(en[5]).toBe('On weekdays with messages, the median is 12.5 messages per day and 4 chats per day.');
    expect(en[6]).toBe('Total gap time is 12 times total agent turn time.');
    expect(en[7]).toContain('grew');
    expect(en[8]).toContain('shrank');
    for (const s of en) expect(s).not.toMatch(/[㐀-鿿]/u);
  });

  test('a_small_sample_shows_only_that_finding', async () => {
    expect(shownFindings(ALL_FINDINGS)).toEqual([ALL_FINDINGS[0]]);
    const v = sample();
    v.findings = [{ key: 'human_vs_agent', params: { ratio: 3 } }, { key: 'sample_small', params: { n: 12 } }];
    const page = await renderPage(v);
    const items = [...section(page, 'in-findings').querySelectorAll('li')];
    expect(items.map((li) => li.dataset.key)).toEqual(['sample_small']);
    expect(text(items[0])).toContain('样本过少');
  });

  test('findings_render_in_server_order_and_say_when_no_rule_fired', async () => {
    let page = await renderPage(sample());
    expect([...section(page, 'in-findings').querySelectorAll('li')].map((li) => li.dataset.key)).toEqual(['tail_concentration', 'human_vs_agent', 'trend']);
    page.remove();
    page = await renderPage({ ...sample(), findings: [] });
    expect(text(section(page, 'in-findings'))).toContain('所选时间段没有触发任何规则');
  });

  test('overview_shows_quantiles_totals_and_the_previous_period', async () => {
    const page = await renderPage(sample());
    const stats = [...section(page, 'in-overview').querySelectorAll('.stat')].map((s) => text(s));
    expect(stats).toEqual([
      '间隔条数 40 上一时段 30 条间隔（+33%）',
      '间隔中位数 10 分钟 上一时段 6 分钟 40 秒（+50%）',
      'P75 30 分钟 上一时段 20 分钟（+50%）',
      'P90 1 小时 30 分钟 上一时段 1 小时（+50%）',
      '间隔合计 1 天 2 小时 上一时段 16 小时 40 分钟（+60%）',
      'agent 单轮耗时中位数 2 分钟 P90 6 分钟 40 秒 · 共 50 轮',
      'agent 单轮耗时合计 2 小时 13 分钟',
      '间隔合计 / 耗时合计 12 倍',
    ]);
    expect(text(section(page, 'in-overview').querySelector('.group-head p'))).toBe('时间段内共 60 条真人消息，涉及 2 个 agent、5 个会话');
  });

  test('no_comparison_when_the_previous_period_has_no_gaps_or_is_absent', async () => {
    const v = sample();
    v.previous = { gaps: { n: 0, p50_secs: 0, p75_secs: 0, p90_secs: 0, total_secs: 0 }, over_15m_count_share: 0, over_15m_secs_share: 0 };
    expect(comparable(v)).toBe(false);
    let page = await renderPage(v);
    for (const id of ['in-overview', 'in-dist']) {
      expect(text(section(page, id))).not.toContain('上一时段');
      expect(text(section(page, id))).not.toContain('100%');
    }
    page.remove();
    const all = sample();
    delete all.previous;
    all.range = 'all' as UsageRange;
    page = await renderPage(all);
    expect(text(section(page, 'in-overview'))).not.toContain('上一时段');
  });

  test('distribution_shows_count_share_and_time_share_on_one_scale', async () => {
    const v = sample();
    const rows = bucketRows(v);
    expect(rows.map((r) => r.bars.map((b) => b.text))).toEqual([
      ['20%', '<1%'],
      ['25%', '2%'],
      ['20%', '4%'],
      ['20%', '15%'],
      ['13%', '38%'],
      ['3%', '42%'],
    ]);
    // 最长的那根(4 小时以上占时长 42%)占满一行,其余按同一把尺子
    expect(rows[5]!.bars[1]!.fraction).toBe(1);
    expect(rows[1]!.bars[0]!.fraction).toBeCloseTo(0.25 / (40_000 / 96_000), 6);
    expect(longShares(v)).toEqual({ count: 0.35, secs: 0.9375 });
    const page = await renderPage(v);
    const dist = section(page, 'in-dist');
    expect(text(dist.querySelector('.group-head p'))).toBe('15 分钟以上的间隔占条数 35%、占时长 94%；上一时段分别为 30%、80%');
    expect([...dist.querySelectorAll('.uc-legend-item')].map(text)).toEqual(['占条数', '占时长']);
    const h14 = dist.querySelector('.ib-row[data-key="h1_4"]')!;
    expect(text(h14)).toBe('1–4 小时 13% 38% 5 条 · 合计 10 小时');
    expect(h14.querySelectorAll('.ib-fill')).toHaveLength(2);
    expect(dist.querySelectorAll('tbody tr')).toHaveLength(6);
    expect(cells(dist.querySelector('tbody tr:last-child')!)).toBe('4 小时以上 | 1 | 3% | 11 小时 7 分钟 | 42%');
  });

  test('bars_hover_with_a_tooltip_and_have_readable_labels', async () => {
    const page = await renderPage(sample());
    const row = section(page, 'in-dist').querySelector<HTMLElement>('.ib-row[data-key="gte_4h"]')!;
    expect(row.getAttribute('aria-label')).toBe('4 小时以上，1 条，占条数 3%，合计 11 小时 7 分钟，占时长 42%');
    row.dispatchEvent(new Event('pointerenter'));
    const list = row.closest('sm-bar-list') as HTMLElement & { updateComplete: Promise<unknown> };
    await list.updateComplete;
    expect([...list.querySelector('.uc-tip')!.children].map(text)).toEqual(['4 小时以上', '1 条，占条数 3%', '合计 11 小时 7 分钟，占时长 42%']);
    expect(list.querySelectorAll('.ib-row.is-dim')).toHaveLength(5);
  });

  test('bar_ends_are_rounded_and_zero_draws_nothing', () => {
    const host = document.createElement('div');
    render(svg`<svg>${barShape('time', 0.5)}${barShape('count', 0)}</svg>`, host);
    const inner = host.querySelectorAll('svg svg');
    expect(inner).toHaveLength(1);
    expect(inner[0]!.getAttribute('width')).toBe('50%');
    expect(inner[0]!.querySelector('rect')!.getAttribute('rx')).toBe('4');
  });

  test('elsewhere_shows_share_median_and_quiet_long_gaps', async () => {
    const v = sample();
    expect(elsewhereRows(v).map((r) => [r.bars[0]!.text, r.note])).toEqual([
      ['75%', '8 条 · 期间别处消息数中位数 2 条'],
      ['80%', '5 条 · 期间别处消息数中位数 4.5 条'],
      ['0%', '1 条 · 期间别处消息数中位数 0 条'],
    ]);
    const page = await renderPage(v);
    expect(text(section(page, 'in-elsewhere').querySelector('.row-note'))).toBe('14 条长间隔中有 3 条（21%）期间所有会话基本无消息：无消息的最长一段不短于该间隔的 80%。');
  });

  test('next_messages_show_counts_and_gap_quantiles', async () => {
    const rows = nextRows(sample());
    expect(rows.map((r) => [r.label, r.bars[0]!.text, r.note])).toEqual([
      ['新指令', '24 条（60%）', '间隔中位数 15 分钟 · P90 1 小时 40 分钟'],
      ['短指令', '10 条（25%）', '间隔中位数 5 分钟 · P90 20 分钟'],
      ['放行', '2 条（5%）', '间隔中位数 1 分钟 40 秒 · P90 2 分钟 30 秒'],
      ['询问进展', '4 条（10%）', '间隔中位数 40 分钟 · P90 1 小时 7 分钟'],
    ]);
    expect(rows[0]!.bars[0]!.fraction).toBe(1);
    const page = await renderPage(sample());
    // 单一系列不画图例
    expect(section(page, 'in-next').querySelector('.uc-legend')).toBeNull();
  });

  test('daily_input_draws_messages_and_lists_chats_in_the_table', async () => {
    const page = await renderPage(sample());
    const daily = section(page, 'in-daily');
    expect(text(daily.querySelector('.group-head p'))).toBe('有消息的工作日 1 天：每天消息数中位数 12、P90 12；会话数中位数 4、P90 4');
    // 零消息的那天不画柱子,周末用浅一档
    expect(daily.querySelectorAll('path.ib-day')).toHaveLength(2);
    expect(daily.querySelectorAll('path.ib-weekend')).toHaveLength(1);
    expect([...daily.querySelectorAll('.uc-legend-item')].map(text)).toEqual(['工作日', '周末']);
    expect([...daily.querySelectorAll('tbody tr')].map(cells)).toEqual(['11月8日 周日 | 3 | 1', '11月9日 周一 | 12 | 4', '11月10日 周二 | 0 | 0']);
    expect(daily.querySelector('.uc-hit')!.getAttribute('aria-label')).toBe('11月8日 周日：3 条消息，涉及 1 个会话');
    // 不做双 y 轴:只有一组刻度
    const ticks = [...daily.querySelectorAll('text.uc-tick[text-anchor="end"]')].map(text);
    expect(ticks).toEqual(['0', '5', '10', '15']);
  });

  test('caliber_states_the_same_day_rule_overnight_and_queued_counts_and_the_upper_bound', async () => {
    const page = await renderPage(sample());
    const items = [...section(page, 'in-caliber').querySelectorAll('li')].map(text);
    expect(items[1]).toBe('当日口径：上一轮在 09:00–22:00 之间结束，且下一条消息在同一自然日发出（服务端本机时间）。各项间隔统计均按此口径；不限此口径的间隔共 47 条（含过夜）。');
    expect(items[2]).toContain('本时间段共 3 条');
    expect(items[3]).toMatch(/^间隔是上界/);
    expect(items.some((i) => i.includes('仅供参考'))).toBe(false);
    page.remove();
    // 「今天」的本期只到此刻,上一时段是完整的昨天:说明两者不等长
    const today = await renderPage({ ...sample(), range: 'today' });
    expect(text(section(today, 'in-caliber'))).toContain('上一时段是完整的昨天');
  });

  test('empty_and_failed_states', async () => {
    const empty = { ...sample(), human_turns: 0, gaps: { n: 0, p50_secs: 0, p75_secs: 0, p90_secs: 0, total_secs: 0 }, findings: [{ key: 'sample_small' as const, params: { n: 0 } }] };
    let page = await renderPage(empty);
    expect(text(page.querySelector('.empty'))).toContain('该时间段内没有真人发起的轮次');
    expect(page.querySelector('sm-bar-list')).toBeNull();
    expect(section(page, 'in-caliber')).not.toBeNull();
    page.remove();
    page = await renderPage({ ...sample(), gaps: { n: 0, p50_secs: 0, p75_secs: 0, p90_secs: 0, total_secs: 0 }, long_gaps: 0 });
    expect(text(section(page, 'in-dist'))).toContain('该时间段内没有可统计的间隔');
    expect(section(page, 'in-daily').querySelector('sm-day-chart')).not.toBeNull();
    page.remove();
    page = await renderPage(null, '无法连接控制台服务');
    expect(text(page.querySelector('.banner-error'))).toContain('洞察数据加载失败');
    expect(text(page.querySelector('.banner-error'))).toContain('无法连接控制台服务');
  });

  test('loading_shows_a_skeleton', async () => {
    const page = document.createElement('sm-page-insights') as InsightsPage;
    page.refresh = async () => {};
    document.body.append(page);
    await page.updateComplete;
    expect(page.querySelector('[aria-busy="true"]')).not.toBeNull();
  });
});

describe('洞察页的请求', () => {
  test('defaults_to_30_days_and_refetches_with_the_chosen_range_and_agent', async () => {
    localStorage.removeItem('summon-dash.insights.range');
    const page = document.createElement('sm-page-insights') as InsightsPage;
    const call = vi.spyOn(page, 'call').mockResolvedValue(sample());
    document.body.append(page);
    await vi.waitFor(() => expect(page.view).not.toBeNull());
    expect(call.mock.calls[0]).toEqual(['insights.summary', { range: '30d' }]);
    await page.updateComplete;
    const picker = page.querySelector<import('../components/date-range').DateRangePicker>('sm-date-range')!;
    picker.querySelector<HTMLButtonElement>('.date-range-trigger')!.click();
    await picker.updateComplete;
    [...picker.querySelectorAll<HTMLButtonElement>('.date-presets button')].find((b) => b.textContent === '7D')!.click();
    await picker.updateComplete;
    picker.querySelector<HTMLButtonElement>('.btn-primary')!.click();
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(2));
    expect(call.mock.calls[1]).toEqual(['insights.summary', { range: '7d' }]);
    expect(localStorage.getItem('summon-dash.insights.range')).toBe('7d');
    picker.dispatchEvent(new CustomEvent('range-change', { detail: { range: 'custom', from: '2026-11-01', to: '2026-11-05' }, bubbles: true }));
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(3));
    expect(call.mock.calls[2]).toEqual(['insights.summary', { range: 'custom', from: '2026-11-01', to: '2026-11-05' }]);
    page.agent = 'alpha';
    page.requestUpdate();
    await page.updateComplete;
    const select = page.querySelector<HTMLSelectElement>('[aria-label="筛选 agent"]')!;
    select.value = '';
    select.dispatchEvent(new Event('change'));
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(4));
    expect(call.mock.calls[3]).toEqual(['insights.summary', { range: 'custom', from: '2026-11-01', to: '2026-11-05' }]);
    await page.refresh();
    expect(call).toHaveBeenCalledTimes(5);
    page.agent = 'alpha';
    await page.refresh();
    expect(call.mock.calls[5]).toEqual(['insights.summary', { range: 'custom', from: '2026-11-01', to: '2026-11-05', agent: 'alpha' }]);
  });

  test('the_latest_request_wins_and_a_failure_clears_the_view', async () => {
    const page = document.createElement('sm-page-insights') as InsightsPage;
    let resolveOld!: (v: InsightsSummary) => void;
    const call = vi.spyOn(page, 'call')
      .mockReturnValueOnce(new Promise<InsightsSummary>((r) => { resolveOld = r; }))
      .mockResolvedValueOnce({ ...sample(), from: '2026-11-01' });
    const old = page.refresh();
    page.range = '7d';
    await page.refresh();
    resolveOld(sample());
    await old;
    expect(page.view!.from).toBe('2026-11-01');
    call.mockRejectedValueOnce(new Error('加载失败'));
    await page.refresh();
    expect(page.view).toBeNull();
    expect(page.loadError).toContain('加载失败');
  });
});

describe('示例环境的洞察数据', () => {
  const srv = () => new MockServer({ now: new Date('2026-11-11T14:32:00+08:00') });
  const fetch = async (params: Record<string, unknown>) => {
    const r = await createMockTransport(srv())('insights.summary', params, {});
    if (!r.ok) throw new Error(r.error.message);
    return r.data as InsightsSummary;
  };

  test('the_30_day_sample_is_rich_enough_to_show_every_chart', async () => {
    const v = await fetch({ range: '30d' });
    expect(METHOD_GUARDS['insights.summary'].data(v, 'x')).toBeNull();
    expect(v.gaps.n).toBeGreaterThanOrEqual(30);
    expect(v.days).toHaveLength(30);
    expect(v.to).toBe('2026-11-11');
    expect(v.previous!.gaps.n).toBeGreaterThan(0);
    expect(v.findings.map((f) => f.key)).toEqual(expect.arrayContaining(['tail_concentration', 'busy_elsewhere', 'polls', 'load', 'human_vs_agent', 'trend']));
    expect(v.buckets.reduce((s, b) => s + b.count, 0)).toBe(v.gaps.n);
    expect(v.next_messages.reduce((s, r) => s + r.count, 0)).toBe(v.gaps.n);
    expect(v.elsewhere.reduce((s, e) => s + e.count, 0)).toBe(v.long_gaps);
  });

  test('today_falls_to_a_small_sample_and_all_has_no_previous_period', async () => {
    const today = await fetch({ range: 'today' });
    expect(today.findings.map((f) => f.key)).toEqual(['sample_small']);
    const all = await fetch({ range: 'all' });
    expect(all.previous).toBeUndefined();
    expect(METHOD_GUARDS['insights.summary'].data(all, 'x')).toBeNull();
    const custom = await fetch({ range: 'custom', from: '2026-11-01', to: '2026-11-05' });
    expect([custom.from, custom.to, custom.days.length]).toEqual(['2026-11-01', '2026-11-05', 5]);
  });
});


test('relative_range_refresh_keeps_duration_and_passes_filters_to_the_api', async () => {
  const page = document.createElement('sm-page-insights') as import('./insights').InsightsPage;
  const call = vi.spyOn(page, 'call').mockResolvedValue(sample());
  document.body.append(page);
  await vi.waitFor(() => expect(page.view).not.toBeNull());
  await page.updateComplete;
  const picker = page.querySelector<import('../components/date-range').DateRangePicker>('sm-date-range')!;
  picker.dispatchEvent(new CustomEvent('range-change', { detail: { range: 'relative', hours: 5 }, bubbles: true }));
  await vi.waitFor(() => expect(page.range).toBe('relative'));
  page.agent = 'alpha';
  await page.refresh();
  expect(call.mock.calls.at(-1)).toEqual(['insights.summary', { range: 'relative', hours: 5, agent: 'alpha' }]);
  await page.refresh();
  expect(call.mock.calls.at(-1)).toEqual(['insights.summary', { range: 'relative', hours: 5, agent: 'alpha' }]);
  const restored = document.createElement('sm-page-insights') as import('./insights').InsightsPage;
  expect(restored.range).toBe('relative'); expect(restored.hours).toBe(5);
  page.remove();
  localStorage.removeItem('summon-dash.insights.range');
  localStorage.removeItem('summon-dash.insights.range.hours');
});
