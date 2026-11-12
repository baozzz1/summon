import { afterEach, expect, test, vi } from 'vitest';
import { store } from '../app/store';
import type { AttentionView, StatusView } from '../api/types';
import type { PageBase } from '../components/page';
import { AttentionState } from '../components/attention';
import fixture from '../api/fixtures/status.get.json';
import attentionFixture from '../api/fixtures/attention.list.json';
import './overview';

type Overview = PageBase & { attention: AttentionState };
let page: Overview | undefined;
afterEach(() => {
  page?.remove();
  page = undefined;
  vi.restoreAllMocks();
  vi.useRealTimers();
});

test('overview refreshes counts, pauses when hidden, marks failures and cleans up', async () => {
  vi.useFakeTimers();
  let visibility = 'visible';
  vi.spyOn(document, 'visibilityState', 'get').mockImplementation(() => visibility as DocumentVisibilityState);
  store.state.status = fixture.data as StatusView;
  vi.spyOn(store, 'refreshStatus').mockResolvedValue([]);
  let view = structuredClone(attentionFixture.data) as AttentionView;
  let fail = false;
  const call = vi.spyOn(store, 'call').mockImplementation(async (method) => {
    if (method === 'attention.list') {
      if (fail) throw new Error('offline');
      return structuredClone(view);
    }
    if (method === 'sessions.list') return { sessions: [], comment_tasks: [], comment_tasks_error: '' };
    if (method === 'status.issues') return [];
    throw new Error(`Unexpected method: ${method}`);
  });
  page = document.createElement('sm-page-overview') as Overview;
  document.body.append(page);
  const settle = async () => {
    await vi.advanceTimersByTimeAsync(0);
    await page!.updateComplete;
  };
  const reads = () => call.mock.calls.filter(([m]) => m === 'attention.list').length;
  await settle();
  expect(reads()).toBe(1);
  expect(page.querySelector('#attention-title')?.textContent).toContain('1');
  view = { ...view, items: [...view.items, { ...view.items[0]!, id: 'second' }] };
  await vi.advanceTimersByTimeAsync(5000);
  await settle();
  expect(page.querySelector('#attention-title')?.textContent).toContain('2');
  view = { ...view, items: [] };
  await vi.advanceTimersByTimeAsync(5000);
  await settle();
  expect(page.querySelector('#attention-title')?.textContent).toContain('0');
  visibility = 'hidden';
  document.dispatchEvent(new Event('visibilitychange'));
  const hiddenReads = reads();
  await vi.advanceTimersByTimeAsync(15000);
  expect(reads()).toBe(hiddenReads);
  visibility = 'visible';
  fail = true;
  document.dispatchEvent(new Event('visibilitychange'));
  await settle();
  expect(reads()).toBe(hiddenReads + 1);
  expect(page.textContent).toContain('过期快照');
  expect(page.attention.data?.items).toHaveLength(0);
  fail = false;
  await vi.advanceTimersByTimeAsync(5000);
  await settle();
  expect(page.textContent).not.toContain('过期快照');
  page.remove();
  const removedReads = reads();
  await vi.advanceTimersByTimeAsync(15000);
  document.dispatchEvent(new Event('visibilitychange'));
  await settle();
  expect(reads()).toBe(removedReads);
});
