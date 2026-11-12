import { language, setLanguage } from '../i18n';
import { afterEach, describe, expect, test } from 'vitest';
import type { ApiResult, StatusView } from '../api/types';
import statusFixture from '../api/fixtures/status.get.json';
import whoamiFixture from '../api/fixtures/session.whoami.json';
import schemaFixture from '../api/fixtures/config.schema.json';
import configFixture from '../api/fixtures/config.get.json';
import { Store } from './store';

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((yes) => { resolve = yes; });
  return { promise, resolve };
}

const status = (revision: string): StatusView => ({ ...statusFixture.data as StatusView, revisions: { global: revision } });
const ok = (data: unknown): ApiResult<unknown> => ({ ok: true, data });
const stores: Store[] = [];
function newStore() {
  const store = new Store();
  stores.push(store);
  store.state = { ...store.state, auth: 'ok', status: status('r0') };
  return store;
}
afterEach(async () => { stores.splice(0).forEach((store) => store.stopPolling()); await setLanguage('zh-CN'); });

describe('状态请求代际', () => {
  test('older_status_response_never_overwrites_the_latest_response_or_broadcasts', async () => {
    const old = deferred<ApiResult<unknown>>();
    const recent = deferred<ApiResult<unknown>>();
    const store = newStore();
    let requests = 0;
    store.init(async (method) => method === 'status.get' ? (requests++ === 0 ? old.promise : recent.promise) : ok(configFixture.data));
    const broadcasts: string[][] = [];
    store.addEventListener('revisions', (e) => broadcasts.push((e as CustomEvent<string[]>).detail));
    const first = store.refreshStatus();
    const second = store.refreshStatus();
    recent.resolve(ok(status('r2')));
    await second;
    old.resolve(ok(status('r1')));
    expect(await first).toEqual([]);
    expect(store.state.status?.revisions).toEqual({ global: 'r2' });
    expect(broadcasts).toEqual([['global']]);
  });

  test('local_revision_ack_invalidates_pre_save_status_requests', async () => {
    const read = deferred<ApiResult<unknown>>();
    const store = newStore();
    store.init(async () => read.promise);
    const pending = store.refreshStatus();
    store.acceptRevision('global', 'r2');
    read.resolve(ok(status('r1')));
    expect(await pending).toEqual([]);
    expect(store.state.status?.revisions.global).toBe('r2');
  });

  test('expire_discards_inflight_status_and_icon_responses', async () => {
    const statusRead = deferred<ApiResult<unknown>>();
    const iconRead = deferred<ApiResult<unknown>>();
    const store = newStore();
    store.init(async (method) => method === 'status.get' ? statusRead.promise : iconRead.promise);
    const before = store.state;
    const pendingStatus = store.refreshStatus();
    const pendingIcon = store.loadIcon();
    store.expire();
    statusRead.resolve(ok(status('r2')));
    iconRead.resolve(ok({ ...configFixture.data, values: [{ key: 'menubar.icon', effective: 'new-icon' }] }));
    await Promise.all([pendingStatus, pendingIcon]);
    expect(store.state.auth).toBe('expired');
    expect(store.state.status).toBe(before.status);
    expect(store.state.icon).toBe(before.icon);
  });

  test('expire_during_boot_cannot_restart_polling_or_authenticate', async () => {
    const read = deferred<ApiResult<unknown>>();
    const started = deferred<void>();
    const store = newStore();
    store.init(async (method) => {
      if (method === 'session.whoami') return ok(whoamiFixture.data);
      if (method === 'config.schema') return ok(schemaFixture.data);
      started.resolve();
      return read.promise;
    });
    const boot = store.boot();
    await started.promise;
    store.expire();
    read.resolve(ok(status('r1')));
    await boot;
    expect(store.state.auth).toBe('expired');
    expect(store.state.whoami).toBeNull();
  });

  test('older_icon_response_cannot_replace_the_new_icon', async () => {
    const old = deferred<ApiResult<unknown>>();
    const recent = deferred<ApiResult<unknown>>();
    const store = newStore();
    let requests = 0;
    store.init(async () => requests++ === 0 ? old.promise : recent.promise);
    const first = store.loadIcon();
    const second = store.loadIcon();
    recent.resolve(ok({ values: [{ key: 'menubar.icon', effective: 'new-icon' }] }));
    await second;
    old.resolve(ok({ values: [{ key: 'menubar.icon', effective: 'old-icon' }] }));
    await first;
    expect(store.state.icon).toBe('new-icon');
  });
});


test('language_switch_refreshes_the_schema_without_a_revision_change_and_retries_failed_reads', async () => {
  const store = newStore();
  const original = schemaFixture.data;
  store.state.schema = original as typeof store.state.schema;
  let reads = 0;
  store.init(async (method) => {
    if (method === 'status.get') return ok({ ...status('r0'), language: 'en' });
    if (method === 'config.schema') {
      if (++reads === 1) return { ok: false, error: { code: 'unavailable', message: 'temporarily unavailable' } };
      return ok({ ...original, groups: original.groups.map((group) => ({ ...group, label: 'translated' })) });
    }
    return ok(configFixture.data);
  });
  await store.refreshStatus(true);
  expect(language()).toBe('en');
  expect(store.state.schema).toBe(original);
  expect(store.state.status?.revisions.global).toBe('r0');
  await store.refreshStatus(true);
  expect(reads).toBe(2);
  expect(store.state.schema?.groups[0]?.label).toBe('translated');
});
