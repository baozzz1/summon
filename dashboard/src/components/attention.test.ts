import { expect, test } from 'vitest';
import { render } from 'lit';
import { AttentionState, attentionLink, attentionPanel } from './attention';
import type { AttentionView } from '../api/types';
const view: AttentionView = { items: [{id:'example',agent:'starlight',agent_display:'Star',chat_name:'Room',source:'agent_ask',summary:'<script>secret</script>',question_count:1,registered_at:'2026-11-09T00:00:00Z',waited_secs:10,link:'lark://applink.feishu.cn/client/chat/open?openChatId=oc_example&position=7',link_target:'message'}],observed_at:'2026-11-09T00:00:10Z',coverage:'structured_requests',partial:false };
test('attention links accept only generated AppLinks', () => {
  expect(attentionLink(view.items[0]!.link)).toBeTruthy();
  for (const url of ['javascript:alert(1)', 'lark://evil.test/client/chat/open?openChatId=oc_x', view.items[0]!.link+'&extra=x']) expect(attentionLink(url)).toBeNull();
});
test('failed refresh keeps a labeled snapshot without active request links', async () => {
  const state = new AttentionState(); await state.refresh(async () => structuredClone(view));
  const root = document.createElement('div'); render(attentionPanel(state),root);
  expect(root.querySelectorAll('a')).toHaveLength(1); expect(root.querySelector('script')).toBeNull();
  await state.refresh(async () => { throw new Error('offline'); }); render(attentionPanel(state),root);
  expect(root.textContent).toContain('过期快照'); expect(root.querySelectorAll('a')).toHaveLength(0);
  await state.refresh(async () => ({...view,items:[]}));render(attentionPanel(state),root);
  expect(root.textContent).toContain('当前没有待处理请求');
});
