import { describe, expect, test } from 'vitest';
import { hrefFor, parseRoute } from '../app/router';
import { NAV_GROUPS } from '../app/nav';
import { bodyText, logTime, rangeText } from './logs';

describe('日志页', () => {
  test('log_routes_carry_their_filters', () => {
    expect(parseRoute('/logs')).toEqual({ page: 'logs' });
    const r = parseRoute('/logs', '?file=sidecar&agent=starlight&since=2026-10-21T01%3A00%3A00Z&until=2026-10-21T02%3A00%3A00Z');
    expect(r).toEqual({ page: 'logs', file: 'sidecar', agent: 'starlight', since: '2026-10-21T01:00:00Z', until: '2026-10-21T02:00:00Z' });
    expect(parseRoute(new URL(hrefFor(r), 'http://x').pathname, new URL(hrefFor(r), 'http://x').search)).toEqual(r);
    expect(parseRoute('/logs', '?file=passwd')).toEqual({ page: 'logs' });
  });

  test('logs_entry_is_only_for_people_who_can_change_machine_settings', () => {
    const logs = NAV_GROUPS.flatMap((g) => g.items).find((it) => it.id === 'logs');
    expect(logs?.machine).toBe(true);
  });

  test('time_range_reads_naturally', () => {
    expect(rangeText()).toBe('');
    expect(rangeText('2026-10-21T01:00:00Z')).toMatch(/之后$/);
    expect(rangeText(undefined, '2026-10-21T01:00:00Z')).toMatch(/之前$/);
    expect(rangeText('2026-10-21T01:00:00Z', '2026-10-21T02:00:00Z')).toContain(' 至 ');
  });

  test('entries_show_their_time_once', () => {
    const now = new Date(2026, 9, 21, 18, 0, 0);
    expect(logTime(new Date(2026, 9, 21, 17, 8, 31).toISOString(), now)).toBe('17:08:31');
    expect(logTime(new Date(2026, 9, 20, 9, 5, 7).toISOString(), now)).toBe('10-20 09:05:07');
    expect(bodyText('[2026-10-21 17:08:31] [summon] 收到消息\n  第二行')).toBe('[summon] 收到消息\n  第二行');
    expect(bodyText('2026/10/21 16:55:16 [Info] ready')).toBe('[Info] ready');
    expect(bodyText('[Error] 2026/07/31 05:35:25.229368 handle message failed')).toBe('[Error] handle message failed');
    expect(bodyText('{"ts":"2026-10-21T12:59:47+08:00"}')).toBe('{"ts":"2026-10-21T12:59:47+08:00"}');
  });
});
