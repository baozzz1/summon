import { describe, expect, test, vi } from 'vitest';
import { ApiCallError, createClient, CSRF_HEADER, type Transport } from './client';

describe('client', () => {
  test('csrf_header_is_sent', async () => {
    const seen: Array<Record<string, string>> = [];
    const t: Transport = async (_m, _b, headers) => {
      seen.push(headers);
      return { ok: true, data: {} };
    };
    const c = createClient(t, () => {});
    await c.call('session.whoami', {});
    c.setCsrf('tok-123');
    await c.call('status.get', {});
    expect(seen[0]).toEqual({});
    expect(seen[1]).toEqual({ [CSRF_HEADER]: 'tok-123' });
  });

  test('unauthenticated_calls_the_shell_hook_and_throws', async () => {
    const hook = vi.fn();
    const c = createClient(async () => ({ ok: false, error: { code: 'unauthenticated', message: '会话已失效' } }), hook);
    await expect(c.call('status.get', {})).rejects.toBeInstanceOf(ApiCallError);
    expect(hook).toHaveBeenCalledTimes(1);
  });

  test('error_key_is_kept_for_the_row', async () => {
    const c = createClient(async () => ({ ok: false, error: { code: 'invalid', message: '取值范围 1–20', key: 'messaging.progress_max_steps' } }), () => {});
    const err = await c.call('config.patch', { scope: { level: 'global' }, revision: 'r', changes: [] }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiCallError);
    expect((err as ApiCallError).key).toBe('messaging.progress_max_steps');
  });
});
