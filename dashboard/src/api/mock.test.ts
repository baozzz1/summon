import { describe, expect, test } from 'vitest';
import { MockServer, parseWhen } from './mock';
import type { CotItem, PreviewView } from './types';

const DEV = 'oc_00000000000000000000000000000001';
const srv = () => new MockServer({ now: new Date('2026-10-20T14:32:00+08:00') });
const cot = (v: PreviewView) => v.messages.find((m) => m.kind === 'cot') as { title: string; items: CotItem[] };
const card = (v: PreviewView) => JSON.stringify((v.messages.find((m) => m.kind === 'card') as { card: unknown }).card);

describe('示例环境的预览随草稿变化', () => {
  const g = { level: 'global' } as const;

  test('cot_and_thinking_and_tools', () => {
    const s = srv();
    expect(cot(s.preview(g, [])).items.map((i) => i.type)).toEqual(['thinking', 'thinking']);
    expect(cot(s.preview(g, [{ key: 'messaging.cot_tool_details', value: true }])).items.some((i) => i.type === 'tool')).toBe(true);
    expect(cot(s.preview(g, [{ key: 'messaging.hide_thinking', value: false }])).items).toEqual([]);
    const hidden = cot(s.preview(g, [{ key: 'messaging.cot_visible', value: false }]));
    expect(hidden.items).toEqual([]);
    expect(hidden.title).toBe('处理请求');
  });

  test('badge_feed_reactions', () => {
    const s = srv();
    expect(card(s.preview(g, []))).toContain('xhigh');
    expect(card(s.preview(g, [{ key: 'messaging.badge_effort', value: false }]))).not.toContain('xhigh');
    const feed = (v: PreviewView) => (v.messages.find((m) => m.kind === 'feed') as { text: string }).text;
    expect(feed(s.preview(g, []))).toContain('CI 已修复');
    expect(feed(s.preview(g, [{ key: 'messaging.agent_feed', value: false }]))).toContain('parse_config_rejects_empty');
    const reacts = (v: PreviewView) => (v.messages.find((m) => m.kind === 'user') as { reactions: string[] }).reactions;
    expect(reacts(s.preview(g, []))).toEqual(['OK', 'DONE']);
    expect(reacts(s.preview(g, [{ key: 'messaging.agent_reactions', value: false }]))).toEqual(['DONE']);
  });

  test('regions_point_at_the_blocks_each_switch_controls', () => {
    const v = srv().preview(g, []);
    expect(v.regions['messaging.cot_visible']).toEqual(['cot']);
    expect(v.regions['messaging.hide_thinking']).toEqual(['cot_t1', 'cot_t2']);
    expect(v.regions['messaging.cot_tool_details']).toEqual(['cot']);
    expect(v.regions['messaging.badge_effort']).toEqual(['badge']);
    expect(v.regions['messaging.agent_feed']).toEqual(['feed']);
    expect(v.regions['messaging.agent_reactions']).toEqual(['user']);
    expect(v.regions['messaging.mask_email']).toBeUndefined();
  });

  test('chat_scope_uses_chat_overrides', () => {
    const v = srv().preview({ level: 'chat', agent: 'starlight', chat_id: DEV }, []);
    expect(v.title).toBe('按 星光 · 示例群 的设置');
    expect(card(v)).not.toContain('xhigh');
  });
});

describe('节律', () => {
  const now = new Date('2026-10-20T14:32:00+08:00');
  test('forms_parse_with_next_fires', () => {
    const r = parseWhen('weekday 09:30', now);
    expect(r.ok && r.human).toBe('每个工作日 09:30');
    expect(r.ok && r.next).toHaveLength(3);
    expect(parseWhen('every 2h', now).ok).toBe(true);
    expect(parseWhen('weekly mon,fri 10:00', now).ok && (parseWhen('weekly mon,fri 10:00', now) as { human: string }).human).toBe('每周一、五 10:00');
    const bad = parseWhen('sometimes', now);
    expect(bad.ok).toBe(false);
  });
});

describe('冲突', () => {
  test('stale_revision_is_rejected_and_nothing_changes', async () => {
    const s = srv();
    const rev = s.revision('global');
    s.global['messaging.progress_max_steps'] = 9;
    s.bump('global');
    const before = JSON.stringify(s.global);
    const { createMockTransport } = await import('./mock');
    const r = await createMockTransport(s)('config.patch', { scope: { level: 'global' }, revision: rev, changes: [{ key: 'messaging.mask_email', value: false }] }, {});
    expect(r.ok).toBe(false);
    expect(!r.ok && r.error.code).toBe('conflict');
    expect(JSON.stringify(s.global)).toBe(before);
  });
});
