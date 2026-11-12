import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { ApiCallError } from '../api/client';
import { METHOD_GUARDS } from '../api/guards';
import { createMockTransport, MOCK_ANALYSIS_MS, MockServer } from '../api/mock';
import type { AnalysisResult, AnalysisView, InsightsAnalysisQuery } from '../api/types';
import { setLanguage } from '../i18n';
import { ANALYSIS_POLL_MS, InsightsAnalysis } from './insights-analysis';

const RESULT: AnalysisResult = {
  text: '## 观察\n- 长间隔占 **74%** 的时长',
  truncated: false,
  generated_at: '2026-11-11T09:30:00+08:00',
  agent: 'sample',
  cli: 'claude',
  model: 'claude-opus-4-8',
  elapsed_ms: 41_000,
  prompt_version: 1,
  usage: { input: 4_099, output: 2_948, cache_read: 0, cache_write: 0 },
  stale: false,
};

function view(over: Partial<AnalysisView> = {}): AnalysisView {
  return {
    primary: { agent: 'sample', label: '示例', source: 'usage', cli: 'claude', model: 'claude-opus-4-8' },
    status: 'none',
    can_start: true,
    ...over,
  };
}

const done = (over: Partial<AnalysisResult> = {}) => view({ status: 'done', result: { ...RESULT, ...over } });
const running = () => view({ status: 'running', started_at: new Date(Date.now() - 12_000).toISOString() });

type Call = (m: string, p: unknown) => Promise<unknown>;
type Spy = { mock: { calls: Array<[string, unknown]> } };

async function mount(call: Call, query: InsightsAnalysisQuery | Omit<InsightsAnalysisQuery, 'lang'> = { range: '30d' }) {
  const el = document.createElement('sm-insights-analysis') as InsightsAnalysis;
  const spy = vi.spyOn(el, 'call').mockImplementation(call as never) as unknown as Spy;
  el.query = query;
  document.body.append(el);
  await settle(el);
  return { el, spy };
}

/** 等请求的 Promise 落地、元素画完。 */
async function settle(el: InsightsAnalysis) {
  for (let i = 0; i < 5; i += 1) {
    await Promise.resolve();
    await el.updateComplete;
  }
}

const text = (el: Element | null | undefined) => (el?.textContent ?? '').replace(/\s+/g, ' ').trim();
const button = (el: Element) => el.querySelector<HTMLButtonElement>('.analysis-actions button');
const gets = (spy: Spy) => spy.mock.calls.filter((c) => c[0] === 'insights.analysis.get');

function setHidden(hidden: boolean) {
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  document.dispatchEvent(new Event('visibilitychange'));
}

beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] }));

afterEach(async () => {
  document.body.replaceChildren();
  vi.useRealTimers();
  setHidden(false);
  await setLanguage('zh-CN');
});

describe('洞察页的分析区', () => {
  test('none_offers_to_generate_and_shows_the_main_agent', async () => {
    const { el } = await mount(async () => view());
    expect(text(el.querySelector('h2'))).toBe('分析');
    expect(text(el.querySelector('.group-head p'))).toContain('内容由模型生成，可能有误');
    expect(text(el.querySelector('.analysis-primary'))).toBe('主 agent 示例 近 30 天用量最多 · claude · claude-opus-4-8 在全局设置中修改');
    expect(el.querySelector('.analysis-primary a')!.getAttribute('href')).toBe('/global/insights?key=primary_agent');
    expect(text(el.querySelector('.analysis-empty'))).toContain('暂无分析结果');
    expect(text(button(el))).toBe('生成分析');
    expect(button(el)!.disabled).toBe(false);
  });

  test('done_renders_the_text_and_its_meta', async () => {
    const { el } = await mount(async () => done());
    const body = el.querySelector('.analysis-text')!;
    expect(body.querySelector('h3')!.textContent).toBe('观察');
    expect(body.querySelector('li strong')!.textContent).toBe('74%');
    expect(text(el.querySelector('.analysis-meta'))).toMatch(/^生成于 .+ · sample · claude-opus-4-8 · 耗时 41 秒 · 输入 4\.1K · 输出 2\.9K token$/);
    expect(text(button(el))).toBe('重新生成');
    expect(el.querySelectorAll('.analysis-note')).toHaveLength(0);
  });

  test('meta_leaves_out_model_and_tokens_when_absent', async () => {
    const result = { ...RESULT };
    delete result.model;
    delete result.usage;
    const { el } = await mount(async () => view({ status: 'done', result }));
    expect(text(el.querySelector('.analysis-meta'))).toMatch(/^生成于 .+ · sample · 耗时 41 秒$/);
  });

  test('stale_retry_failure_and_truncated_are_flagged_around_the_old_result', async () => {
    const { el } = await mount(async () => done({ stale: true, truncated: true, retry_failure: { reason: 'claude 异常退出', at: '2026-11-11T10:00:00+08:00' } }));
    const card = el.querySelector('.analysis')!;
    const order = [...card.children].map((c) => (c.classList.contains('analysis-note') ? text(c) : c.className));
    expect(order).toEqual([
      'analysis-primary',
      '重新生成未成功：claude 异常退出。以下为之前的结果。',
      '统计数据或设置在生成之后有变化，可重新生成。',
      'analysis-text',
      '结果超出长度上限，已截断。',
      'analysis-meta',
      'analysis-actions',
    ]);
    expect(card.querySelector('.analysis-note.is-warn')).not.toBeNull();
  });

  test('running_shows_progress_and_elapsed_time_without_a_button', async () => {
    const { el } = await mount(async () => running());
    expect(text(el.querySelector('.analysis-running'))).toBe('正在分析，已用时 12 秒');
    expect(button(el)).toBeNull();
  });

  test('failed_shows_the_reason_and_can_start_again', async () => {
    const { el, spy } = await mount(async (m) => (m === 'insights.analysis.get' ? view({ status: 'failed', reason: '分析超过 300 秒未完成，已停止' }) : running()));
    expect(text(el.querySelector('.analysis-note'))).toBe('分析失败：分析超过 300 秒未完成，已停止');
    expect(text(button(el))).toBe('生成分析');
    button(el)!.click();
    await settle(el);
    // 失败不挡下一次发起;没有成功结果时不带 force
    expect(spy.mock.calls.find((c) => c[0] === 'insights.analysis.start')![1]).toEqual({ range: '30d', lang: 'zh-CN' });
  });

  test('unsupported_shows_the_server_reason_and_no_button', async () => {
    const { el } = await mount(async () => view({ status: 'unsupported', reason: '主 agent 示例 使用 cursor，不支持分析', can_start: false }));
    expect(text(el.querySelector('.analysis-note.is-warn'))).toBe('主 agent 示例 使用 cursor，不支持分析');
    expect(button(el)).toBeNull();
    // 没有可用 agent 时没有主 agent 那一行
    const { el: none } = await mount(async () => ({ status: 'unsupported', reason: '没有可用的 agent', can_start: false }));
    expect(none.querySelector('.analysis-primary')).toBeNull();
    expect(text(none.querySelector('.analysis-note'))).toBe('没有可用的 agent');
  });

  test('without_permission_the_button_is_disabled_and_says_why', async () => {
    const { el } = await mount(async () => ({ ...done(), can_start: false }));
    expect(button(el)!.disabled).toBe(true);
    expect(text(el.querySelector('.analysis-actions'))).toContain('发起分析需要整机设置权限。');
  });

  test('starting_enters_running_polls_every_3s_and_stops_when_done', async () => {
    const answers: AnalysisView[] = [view(), running(), running(), done()];
    const { el, spy } = await mount(async (m) => (m === 'insights.analysis.start' ? running() : answers.shift()!));
    button(el)!.click();
    await settle(el);
    expect(el.querySelector('.analysis')!.getAttribute('data-status')).toBe('running');
    expect(gets(spy)).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(ANALYSIS_POLL_MS - 1);
    expect(gets(spy)).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(gets(spy)).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(ANALYSIS_POLL_MS * 2);
    await settle(el);
    expect(gets(spy)).toHaveLength(4);
    expect(el.querySelector('.analysis-text')).not.toBeNull();
    await vi.advanceTimersByTimeAsync(ANALYSIS_POLL_MS * 5);
    expect(gets(spy)).toHaveLength(4);
  });

  test('regenerating_a_result_forces_a_new_run', async () => {
    const { el, spy } = await mount(async (m) => (m === 'insights.analysis.start' ? running() : done()));
    button(el)!.click();
    await settle(el);
    expect(spy.mock.calls.find((c) => c[0] === 'insights.analysis.start')![1]).toEqual({ range: '30d', lang: 'zh-CN', force: true });
  });

  test('a_hidden_page_does_not_poll_and_queries_once_when_visible_again', async () => {
    const { el, spy } = await mount(async () => running());
    setHidden(true);
    await vi.advanceTimersByTimeAsync(ANALYSIS_POLL_MS * 5);
    expect(gets(spy)).toHaveLength(1);
    setHidden(false);
    await settle(el);
    expect(gets(spy)).toHaveLength(2);
    // 回来之后照常轮询
    await vi.advanceTimersByTimeAsync(ANALYSIS_POLL_MS);
    expect(gets(spy)).toHaveLength(3);
  });

  test('leaving_the_page_stops_polling', async () => {
    const { el, spy } = await mount(async () => running());
    el.remove();
    await vi.advanceTimersByTimeAsync(ANALYSIS_POLL_MS * 5);
    expect(gets(spy)).toHaveLength(1);
  });

  test('no_duplicate_request_while_one_is_in_flight', async () => {
    let release!: (v: AnalysisView) => void;
    const { el, spy } = await mount(() => new Promise<AnalysisView>((r) => { release = r; }));
    setHidden(true);
    setHidden(false);
    expect(gets(spy)).toHaveLength(1);
    release(running());
    await settle(el);
    expect(el.querySelector('.analysis-running')).not.toBeNull();
  });

  test('busy_start_is_reported_in_place_without_changing_the_result', async () => {
    const { el } = await mount(async (m) => {
      if (m === 'insights.analysis.start') throw new ApiCallError({ code: 'busy', message: '另一项分析正在进行，请稍后再试' });
      return done();
    });
    button(el)!.click();
    await settle(el);
    expect(text(el.querySelector('.analysis-notice'))).toBe('另一项分析正在进行，请稍后再试');
    expect(el.querySelector('.analysis')!.getAttribute('data-status')).toBe('done');
    expect(el.querySelector('.analysis-text')).not.toBeNull();
    expect(button(el)!.disabled).toBe(false);
  });

  test('changing_the_range_refetches_and_drops_the_late_answer', async () => {
    const pending: Array<(v: AnalysisView) => void> = [];
    const { el, spy } = await mount(() => new Promise<AnalysisView>((r) => pending.push(r)));
    el.query = { range: '7d', agent: 'sample' };
    await settle(el);
    expect(gets(spy).map((c) => c[1])).toEqual([
      { range: '30d', lang: 'zh-CN' },
      { range: '7d', agent: 'sample', lang: 'zh-CN' },
    ]);
    pending[1]!(view({ status: 'failed', reason: '新时间段' }));
    await settle(el);
    pending[0]!(done());
    await settle(el);
    expect(el.querySelector('.analysis')!.getAttribute('data-status')).toBe('failed');
    // 同一组条件重画不重新请求
    el.query = { range: '7d', agent: 'sample' };
    await settle(el);
    expect(gets(spy)).toHaveLength(2);
  });

  test('requests_follow_the_interface_language', async () => {
    const { el, spy } = await mount(async () => view());
    await setLanguage('en');
    await settle(el);
    expect(gets(spy).map((c) => (c[1] as InsightsAnalysisQuery).lang)).toEqual(['zh-CN', 'en']);
    expect(text(button(el))).toBe('Generate analysis');
    expect(text(el.querySelector('.analysis-primary'))).toContain('Main agent');
  });

  test('a_failed_load_offers_a_retry', async () => {
    const { el, spy } = await mount(async () => { throw new Error('无法连接控制台服务'); });
    expect(text(el.querySelector('.banner-error'))).toContain('分析结果加载失败');
    el.querySelector<HTMLButtonElement>('.banner-error button')!.click();
    await settle(el);
    expect(gets(spy)).toHaveLength(2);
  });
});

describe('示例环境的分析', () => {
  const now = new Date('2026-11-11T14:32:00+08:00');
  async function ask(srv: MockServer, method: 'insights.analysis.get' | 'insights.analysis.start', params: Record<string, unknown>) {
    const r = await createMockTransport(srv)(method, { lang: 'zh-CN', ...params }, {});
    if (r.ok) expect(METHOD_GUARDS[method].data(r.data, 'x')).toBeNull();
    return r;
  }
  const data = (r: Awaited<ReturnType<typeof ask>>) => (r.ok ? (r.data as AnalysisView) : null);

  test('ranges_demonstrate_each_state', async () => {
    const srv = new MockServer({ now, identity: 'owner' });
    expect(data(await ask(srv, 'insights.analysis.get', { range: '30d' }))!.result!.text).toContain('## 观察');
    const week = data(await ask(srv, 'insights.analysis.get', { range: '7d' }))!;
    expect(week.result!.stale).toBe(true);
    expect(week.result!.retry_failure).toBeDefined();
    expect(data(await ask(srv, 'insights.analysis.get', { range: '90d' }))!.status).toBe('failed');
    expect(data(await ask(srv, 'insights.analysis.get', { range: 'today' }))!.status).toBe('none');
    expect(data(await ask(srv, 'insights.analysis.get', { range: '30d', agent: 'starlight' }))!.status).toBe('none');
    srv.global.primary_agent = 'nimbus';
    const cursor = data(await ask(srv, 'insights.analysis.get', { range: '30d' }))!;
    expect(cursor.status).toBe('unsupported');
    expect(cursor.primary!.source).toBe('configured');
  });

  test('starting_runs_then_finishes_and_another_slot_is_busy_meanwhile', async () => {
    vi.setSystemTime(now);
    const srv = new MockServer({ now, identity: 'owner' });
    expect(data(await ask(srv, 'insights.analysis.start', { range: 'today' }))!.status).toBe('running');
    const busy = await ask(srv, 'insights.analysis.start', { range: '7d' });
    expect(busy.ok ? null : busy.error.code).toBe('busy');
    // 没过期的结果不带 force 不重跑
    expect(data(await ask(srv, 'insights.analysis.start', { range: '30d' }))!.status).toBe('done');
    vi.setSystemTime(now.getTime() + MOCK_ANALYSIS_MS);
    const finished = data(await ask(srv, 'insights.analysis.get', { range: 'today' }))!;
    expect(finished.status).toBe('done');
    expect(finished.result!.stale).toBe(false);
  });

  test('a_feishu_viewer_without_machine_rights_cannot_start', async () => {
    const srv = new MockServer({ now });
    expect(data(await ask(srv, 'insights.analysis.get', { range: '30d' }))!.can_start).toBe(srv.canEdit({ level: 'global' }).ok);
  });
});
