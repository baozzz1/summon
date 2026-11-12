import { describe, expect, test } from 'vitest';
import type { AgentSummary } from '../api/types';
import { previewBotName } from './labels';

const agent = (alias: string, over: Partial<AgentSummary> = {}): AgentSummary => ({
  alias,
  name: alias,
  display: '',
  cli: 'claude',
  enabled: true,
  routable: true,
  credentials: 'ready',
  chats: 0,
  ...over,
});

describe('整机层预览的机器人名', () => {
  test('preview_bot_skips_disabled_and_offline_agents', () => {
    const list = [agent('a', { enabled: false, display: '停用的' }), agent('b', { routable: false }), agent('c', { display: '星光' })];
    expect(previewBotName(list, { kind: 'owner' })).toBe('星光');
    expect(previewBotName([agent('d')], undefined)).toBe('d');
    expect(previewBotName([agent('a', { enabled: false })], { kind: 'owner' })).toBe('agent');
  });

  test('preview_bot_prefers_the_issuing_agent_when_live', () => {
    const list = [agent('c', { display: '星光' }), agent('e', { display: '阿卡迪亚' })];
    expect(previewBotName(list, { kind: 'feishu', name: 'u', agent: 'e', chat_id: 'x', chat_kind: 'p2p' })).toBe('阿卡迪亚');
  });
});
