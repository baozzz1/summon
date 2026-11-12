import { afterEach, expect, test, vi } from 'vitest';
import { render } from 'lit';
import { AgentCleanup } from './agent-cleanup';
import type { CleanupJob } from '../api/types';
import type { Caller } from '../app/editable';

const ready = (): CleanupJob => ({ job: 'job', agent: 'sample', status: 'ready', message: null, bytes: 3, sha256: null, filename: 'sample-backup.tar.gz', paths: ['agents/sample'], downloaded: false });
afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });

function fixture(failDownload = false) {
  let job = ready();
  const calls: string[] = [];
  const call: Caller = (async (method: string) => {
    calls.push(method);
    if (method === 'agents.cleanup.prepare') return job;
    if (method === 'agents.cleanup.status') return job;
    if (method === 'agents.cleanup.download') {
      if (failDownload) throw new Error('download failed');
      job = { ...job, downloaded: true };
      return { offset: 0, next: 3, done: true, base64: 'YWJj' };
    }
    if (method === 'agents.cleanup.confirm') return { ...job, status: 'done' };
    throw new Error(method);
  }) as Caller;
  const host = document.createElement('div');
  document.body.append(host);
  const finished = vi.fn(async () => {});
  const cleanup = new AgentCleanup('sample', call, () => render(cleanup.render(), host), finished);
  return { cleanup, host, calls, finished };
}

test('cleanup_requires_saved_download_name_and_final_confirmation', async () => {
  const { cleanup, host, calls, finished } = fixture();
  await cleanup.prepare();
  await cleanup.confirm();
  expect(calls).not.toContain('agents.cleanup.confirm');
  expect(host.querySelector('[aria-label="确认清理的 agent 名称"]')).toBeNull();
  vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {});
  await cleanup.download();
  expect(cleanup.error).toBe('');
  expect(cleanup.saved).toBe(false);
  cleanup.saved = true;
  cleanup.name = 'different';
  await cleanup.confirm();
  expect(calls).not.toContain('agents.cleanup.confirm');
  cleanup.name = 'sample';
  const confirmation = cleanup.confirm();
  const dialog = document.querySelector('dialog')!;
  expect(dialog.textContent).toContain('清理 sample');
  (dialog.querySelector('.btn-danger') as HTMLButtonElement).click();
  await confirmation;
  expect(calls.filter((m) => m === 'agents.cleanup.confirm')).toHaveLength(1);
  expect(finished).toHaveBeenCalledOnce();
});

test('download_failure_keeps_confirmation_unavailable_and_does_not_delete', async () => {
  const { cleanup, host, calls } = fixture(true);
  await cleanup.prepare();
  await cleanup.download();
  expect(cleanup.error).toContain('download failed');
  expect(cleanup.saved).toBe(false);
  expect(host.querySelector('[aria-label="确认清理的 agent 名称"]')).toBeNull();
  await cleanup.confirm();
  expect(calls).not.toContain('agents.cleanup.confirm');
});
