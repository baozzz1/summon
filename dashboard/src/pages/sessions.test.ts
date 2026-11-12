import { commentTaskRow } from '../components/comment-task';
import { render } from 'lit';
import { describe, expect, test } from 'vitest';
import listFixture from '../api/fixtures/sessions.list.json';
import type { SessionRow } from '../api/types';
import { hrefFor, navIdFor, parseRoute } from '../app/router';
import { forgetGoneSessions, isHandedOff, noteHandedOff, outcomeText } from '../components/session-actions';
import { fmtElapsed } from '../util/format';
import detailFixture from '../api/fixtures/sessions.get.json';
import type { OutsideUsage, SessionDetail } from '../api/types';
import { mergeOutside, timeline } from './session-detail';
import { groupSessions, sessionMeta } from './sessions';

const rows = (listFixture as unknown as { data: { sessions: SessionRow[] } }).data.sessions;

describe('会话页', () => {
  test('session_routes_round_trip', () => {
    expect(parseRoute('/sessions')).toEqual({ page: 'sessions' });
    const r = parseRoute('/sessions/starlight/oc_1');
    expect(r).toEqual({ page: 'session', agent: 'starlight', chatId: 'oc_1' });
    expect(hrefFor({ page: 'session', agent: 'starlight', chatId: 'oc_1' })).toBe('/sessions/starlight/oc_1');
    expect(navIdFor(r)).toBe('sessions');
    expect(parseRoute('/sessions/starlight')).toEqual({ page: 'not_found', path: '/sessions/starlight' });
  });

  test('groups_follow_the_handling_order', () => {
    const shuffled = [...rows].reverse();
    expect(groupSessions(shuffled).map((g) => g.state)).toEqual(['waiting', 'working', 'idle']);
  });

  test('meta_says_how_long_in_plain_words', () => {
    const now = new Date('2026-10-21T10:00:00+08:00');
    const at = (secs: number) => new Date(now.getTime() - secs * 1000).toISOString();
    const base = rows[1]!;
    expect(sessionMeta({ ...base, state: 'working', turn_started_at: at(192) }, now)).toBe('已运行 3 分 12 秒');
    expect(sessionMeta({ ...base, state: 'working', turn_started_at: undefined }, now)).toBe('子 agent 运行中');
    expect(sessionMeta({ ...base, state: 'waiting', waiting_since: at(3_900) }, now)).toBe('已等待 1 小时 5 分');
    expect(sessionMeta({ ...base, state: 'blocked', blocked: 'permission' }, now)).toBe('等待授权确认');
    expect(fmtElapsed(at(42), now)).toBe('42 秒');
    expect(fmtElapsed(at(2 * 86_400 + 3 * 3600), now)).toBe('2 天 3 小时');
  });

  test('outcomes_read_as_plain_sentences', () => {
    expect(outcomeText('exit', 'done').text).toBe('已退出会话，下一条消息将恢复上下文');
    expect(outcomeText('handled', 'absent').text).toBe('该回复已处理');
    expect(outcomeText('archive', 'absent').text).toBe('会话已结束');
    expect(outcomeText('handled', 'superseded').kind).toBe('warn');
  });
});

describe('交给 daemon 之后的会话', () => {
  test('exit_and_archive_lock_the_row_until_it_leaves_or_two_minutes_pass', () => {
    const row = { agent: 'starlight', tag: 'g-handed' };
    noteHandedOff(row, 'exit', 'pending', 1_000);
    expect(isHandedOff(row, 61_000)).toBe(true);
    expect(isHandedOff(row, 121_001)).toBe(false);

    noteHandedOff(row, 'archive', 'done', 0);
    forgetGoneSessions([{ agent: 'starlight', tag: 'g-other' }]);
    expect(isHandedOff(row, 1)).toBe(false);

    noteHandedOff(row, 'interrupt', 'done', 0);
    noteHandedOff(row, 'exit', 'running', 0);
    noteHandedOff(row, 'archive', 'absent', 0);
    expect(isHandedOff(row, 1)).toBe(false);
  });

  test('outside_usage_sits_between_turns_by_time_and_merges_once', () => {
    const d = (detailFixture as unknown as { data: SessionDetail }).data;
    const outside = d.outside!;
    expect(outside).toHaveLength(1);
    const entries = timeline(d.turns, outside);
    expect(entries.map((e) => ('turn' in e ? 'turn' : 'outside'))).toEqual(['outside', 'turn']);
    const later: OutsideUsage = { ...outside[0]!, from_ms: outside[0]!.to_ms + 1, to_ms: outside[0]!.to_ms + 2 };
    expect(mergeOutside([later, ...outside], outside).map((o) => o.to_ms)).toEqual([later.to_ms, outside[0]!.to_ms]);
  });
});

test('评论任务展示进程和失败原因且不提供会话动作', () => {
  const host = document.createElement('div');
  render(commentTaskRow({ id: 'task', agent: 'writer', title: '<script>probe</script>',
    stage: 'failed', status: '处理失败', status_en: 'Failed', busy: false,
    pid: 4242, received_ms: 100, updated_ms: 200, error: '创建者无法验证' }), host);
  expect(host.textContent).toContain('PID 4242');
  expect(host.textContent).toContain('创建者无法验证');
  expect(host.textContent).toContain('<script>probe</script>');
  expect(host.querySelector('script')).toBeNull();
  expect(host.querySelector('button')).toBeNull();
  expect(host.querySelector('[data-comment-stage="failed"]')).not.toBeNull();
});

test('评论任务只在起草阶段提供停止入口', () => {
  const host = document.createElement('div');
  const task = { id: 'task', agent: 'writer', title: 'Document', stage: 'drafting', status: '正在起草', status_en: 'Drafting', busy: true, pid: 4242, received_ms: 1, updated_ms: 2, error: null };
  let calls = 0;
  render(commentTaskRow(task, () => calls++), host);
  host.querySelector('button')!.click();
  expect(calls).toBe(1);
  render(commentTaskRow(task, () => calls++, true), host);
  expect(host.querySelector('button')!.disabled).toBe(true);
  for (const stage of ['prepared', 'draft_ready', 'sending', 'closed', 'stopping']) {
    render(commentTaskRow({ ...task, stage }, () => calls++), host);
    expect(host.querySelector('button')).toBeNull();
  }
});
