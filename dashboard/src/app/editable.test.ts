import { describe, expect, test } from 'vitest';
import { ApiCallError } from '../api/client';
import type { ConfigView, DocView, FieldDesc, RawCheck, RawView } from '../api/types';
import { ConfigDraft, DocDraft, EditSet, RawDraft, ValueDraft, type Caller } from './editable';

const field = (key: string, effect: FieldDesc['effect'] = 'next_turn'): FieldDesc => ({
  key,
  label: key,
  description: '',
  group: 'g',
  kind: 'bool',
  layers: ['global'],
  effect,
  sensitive: false,
  write: 'form',
  preview: false,
});

const FIELDS = new Map([
  ['a', field('a')],
  ['b', field('b', 'restart')],
  ['c', field('c', 'restart')],
]);

function view(rev: string, a: boolean | null = null, b: boolean | null = null): ConfigView {
  return {
    scope: { level: 'global' },
    revision: rev,
    values: [
      { key: 'a', value: a, effective: a ?? true, source: a === null ? 'builtin' : 'global', overridden_by: [], editable: true },
      { key: 'b', value: b, effective: b ?? false, source: b === null ? 'builtin' : 'global', overridden_by: [], editable: true },
      { key: 'c', value: null, effective: 8, source: 'builtin', overridden_by: [], editable: true },
    ],
  };
}

function draftWith(call: Caller) {
  let changes = 0;
  const d = new ConfigDraft({ level: 'global' }, 'config.yaml', () => FIELDS, call, () => changes++);
  return { d, changes: () => changes };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

const saved = { revision: 'r2', effects: [], restart_pending: [] };
const docView = (text: string, revision = 'r1'): DocView => ({ exists: true, revision, text, path: '/example/notes.md', editable: true, effect: 'next_turn' });
const rawView = (text: string, revision = 'r1'): RawView => ({ revision, text, path: '/example/config.yaml', editable: true });
const rawCheck: RawCheck = { diff: 'reviewed diff', errors: [], warnings: [] };

describe('在途草稿', () => {
  test.each(['same-key', 'new-key', 'restore-original'] as const)('config_keeps_edits_after_submission_%s', async (edit) => {
    const patch = deferred<typeof saved>();
    const call = (async (method: string) => method === 'config.patch' ? patch.promise : view('r2', false)) as Caller;
    const { d } = draftWith(call);
    d.setView(view('r1', true));
    d.set('a', false);
    const pending = d.save();
    if (edit === 'same-key') d.set('a', null);
    if (edit === 'new-key') d.set('b', true);
    if (edit === 'restore-original') d.set('a', true);
    patch.resolve(saved);
    expect((await pending).ok).toBe(true);
    expect(d.changeList()).toEqual(edit === 'new-key' ? [{ key: 'b', value: true }] : [{ key: 'a', value: edit === 'same-key' ? null : true }]);
  });

  test('config_keeps_edits_during_post_save_reload_and_updates_base_when_reload_fails', async () => {
    const refresh = deferred<ConfigView>();
    const refreshing = deferred<void>();
    const call = (async (method: string) => {
      if (method === 'config.patch') return saved;
      refreshing.resolve();
      return refresh.promise;
    }) as Caller;
    const { d } = draftWith(call);
    d.setView(view('r1', true));
    d.set('a', false);
    const pending = d.save();
    await refreshing.promise;
    d.set('a', true);
    d.set('b', true);
    refresh.reject(new Error('offline'));
    expect((await pending).ok).toBe(true);
    expect(d.base('a')).toBe(false);
    expect(d.view?.revision).toBe('r2');
    expect(d.changeList()).toEqual([{ key: 'a', value: true }, { key: 'b', value: true }]);
  });

  test('config_failure_keeps_latest_edits_and_does_not_attach_old_validation_to_new_value', async () => {
    const patch = deferred<typeof saved>();
    const { d } = draftWith((async () => patch.promise) as Caller);
    d.setView(view('r1'));
    d.set('c', 99);
    const pending = d.save();
    d.set('c', 7);
    patch.reject(new ApiCallError({ code: 'invalid', message: 'invalid submitted value', key: 'c' }));
    expect((await pending).ok).toBe(false);
    expect(d.changeList()).toEqual([{ key: 'c', value: 7 }]);
    expect(d.errors.has('c')).toBe(false);
  });

  test('config_coalesces_saves_and_reconciles_only_the_submitted_snapshot', async () => {
    const patch = deferred<typeof saved>();
    const calls: string[] = [];
    const { d } = draftWith((async (method: string) => {
      calls.push(method);
      return method === 'config.patch' ? patch.promise : view('r2', false);
    }) as Caller);
    d.setView(view('r1', true));
    d.set('a', false);
    const first = d.save();
    d.set('b', true);
    const second = d.save();
    patch.resolve(saved);
    await Promise.all([first, second]);
    expect(calls.filter((m) => m === 'config.patch')).toHaveLength(1);
    expect(d.changeList()).toEqual([{ key: 'b', value: true }]);
  });

  test('config_keeps_reset_intent_when_reload_starts_after_reset_during_save', async () => {
    const patch = deferred<typeof saved>();
    const { d } = draftWith((async (method: string) => method === 'config.patch' ? patch.promise : view('r2', false)) as Caller);
    d.setView(view('r1', true));
    d.set('a', false);
    const pending = d.save();
    d.reset();
    await d.reload();
    expect(d.shown('a').value).toBe(true);
    patch.resolve(saved);
    await pending;
    expect(d.changeList()).toEqual([{ key: 'a', value: true }]);
  });

  test('config_retains_new_edits_when_post_save_reload_succeeds', async () => {
    const refresh = deferred<ConfigView>();
    const refreshing = deferred<void>();
    const { d } = draftWith((async (method: string) => {
      if (method === 'config.patch') return saved;
      refreshing.resolve();
      return refresh.promise;
    }) as Caller);
    d.setView(view('r1', true));
    d.set('a', false);
    const pending = d.save();
    await refreshing.promise;
    d.set('a', true);
    d.set('b', true);
    refresh.resolve(view('r2', false));
    await pending;
    expect(d.changeList()).toEqual([{ key: 'a', value: true }, { key: 'b', value: true }]);
  });

  test('doc_save_preserves_new_text_and_ignores_a_read_from_before_the_commit', async () => {
    const put = deferred<DocView>();
    const oldRead = deferred<DocView>();
    let gets = 0;
    const d = new DocDraft({ level: 'global' }, 'notes', (async (method: string) => {
      if (method === 'docs.put') return put.promise;
      return gets++ === 0 ? docView('base') : oldRead.promise;
    }) as Caller, () => {});
    await d.load();
    const reading = d.reload();
    d.setText('submitted');
    const pending = d.save();
    d.setText('latest');
    put.resolve(docView('submitted', 'r2'));
    await pending;
    oldRead.resolve(docView('base'));
    await reading;
    expect(d.view?.revision).toBe('r2');
    expect(d.text).toBe('latest');
    expect(d.dirtyCount()).toBe(1);
  });

  test('doc_load_preserves_edits_made_while_reading', async () => {
    const read = deferred<DocView>();
    const d = new DocDraft({ level: 'global' }, 'notes', (async () => read.promise) as Caller, () => {});
    const pending = d.load();
    d.setText('typed while loading');
    read.resolve(docView('base'));
    await pending;
    expect(d.text).toBe('typed while loading');
  });

  test('doc_preserves_reset_during_save_even_when_a_new_load_arrives', async () => {
    const put = deferred<DocView>();
    let gets = 0;
    const d = new DocDraft({ level: 'global' }, 'notes', (async (method: string) => {
      if (method === 'docs.put') return put.promise;
      return gets++ === 0 ? docView('base') : docView('submitted', 'r2');
    }) as Caller, () => {});
    await d.load();
    d.setText('submitted');
    const pending = d.save();
    d.reset();
    await d.load();
    put.resolve(docView('submitted', 'r2'));
    await pending;
    expect(d.text).toBe('base');
    expect(d.dirtyCount()).toBe(1);
  });

  test('raw_review_and_write_use_one_snapshot_while_new_edits_remain_unsaved', async () => {
    const approved = deferred<boolean>();
    const reviewing = deferred<void>();
    const writes: unknown[] = [];
    let gets = 0;
    const d = new RawDraft({ file: 'global' }, 'yaml', (async (method: string, params: unknown) => {
      if (method === 'config.raw.check') return rawCheck;
      if (method === 'config.raw.put') { writes.push(params); return saved; }
      return gets++ === 0 ? rawView('base') : rawView('submitted', 'r2');
    }) as Caller, async () => { reviewing.resolve(); return approved.promise; }, () => {});
    await d.load();
    d.setText('submitted');
    const pending = d.save();
    await reviewing.promise;
    d.setText('latest');
    const again = d.save();
    approved.resolve(true);
    await Promise.all([pending, again]);
    expect(writes).toEqual([{ file: 'global', revision: 'r1', text: 'submitted' }]);
    expect(d.text).toBe('latest');
    expect(d.view?.text).toBe('submitted');
    expect(d.dirtyCount()).toBe(1);
  });

  test('raw_cancel_preserves_new_edits_and_never_writes', async () => {
    const approval = deferred<boolean>();
    const reviewing = deferred<void>();
    const calls: string[] = [];
    const d = new RawDraft({ file: 'global' }, 'yaml', (async (method: string) => {
      calls.push(method);
      return method === 'config.raw.check' ? rawCheck : rawView('base');
    }) as Caller, async () => { reviewing.resolve(); return approval.promise; }, () => {});
    await d.load();
    d.setText('submitted');
    const pending = d.save();
    await reviewing.promise;
    d.setText('latest');
    approval.resolve(false);
    expect(await pending).toMatchObject({ ok: false, cancelled: true });
    expect(d.text).toBe('latest');
    expect(calls).not.toContain('config.raw.put');
  });

  test('raw_does_not_show_validation_for_a_previous_text', async () => {
    const check = deferred<RawCheck>();
    const d = new RawDraft({ file: 'global' }, 'yaml', (async () => check.promise) as Caller, async () => true, () => {});
    d.setText('submitted');
    const pending = d.check();
    d.setText('latest');
    check.resolve(rawCheck);
    await pending;
    expect(d.lastCheck).toBeNull();
  });

  test.each(['doc', 'raw'] as const)('%s_failure_preserves_latest_text', async (kind) => {
    const write = deferred<never>();
    const call = (async (method: string) => {
      if (method.endsWith('.put')) return write.promise;
      if (method.endsWith('.check')) return rawCheck;
      return kind === 'doc' ? docView('base') : rawView('base');
    }) as Caller;
    const d = kind === 'doc' ? new DocDraft({ level: 'global' }, kind, call, () => {}) : new RawDraft({ file: 'global' }, kind, call, async () => true, () => {});
    await d.load();
    d.setText('submitted');
    const pending = d.save();
    d.setText('latest');
    write.reject(new ApiCallError({ code: 'conflict', message: 'changed elsewhere' }));
    expect((await pending).ok).toBe(false);
    expect(d.text).toBe('latest');
    expect(d.hasConflict()).toBe(true);
  });

  test('raw_ack_survives_read_failure_and_preserves_edits_during_refresh', async () => {
    const refresh = deferred<RawView>();
    const refreshing = deferred<void>();
    let gets = 0;
    const d = new RawDraft({ file: 'global' }, 'yaml', (async (method: string) => {
      if (method.endsWith('.check')) return rawCheck;
      if (method.endsWith('.put')) return saved;
      if (gets++ === 0) return rawView('base');
      refreshing.resolve();
      return refresh.promise;
    }) as Caller, async () => true, () => {});
    await d.load();
    d.setText('submitted');
    const pending = d.save();
    await refreshing.promise;
    d.setText('latest');
    refresh.reject(new Error('offline'));
    expect((await pending).ok).toBe(true);
    expect(d.view?.revision).toBe('r2');
    expect(d.view?.text).toBe('submitted');
    expect(d.text).toBe('latest');
  });

  test('raw_latest_check_owns_validation_even_when_an_older_check_finishes_last', async () => {
    const first = deferred<RawCheck>();
    const second = deferred<RawCheck>();
    let checks = 0;
    const d = new RawDraft({ file: 'global' }, 'yaml', (async () => checks++ === 0 ? first.promise : second.promise) as Caller, async () => true, () => {});
    d.setText('same text');
    const old = d.check();
    const recent = d.check();
    const latest = { ...rawCheck, diff: 'latest diff' };
    second.resolve(latest);
    await recent;
    first.resolve(rawCheck);
    await old;
    expect(d.lastCheck).toBe(latest);
  });

  test('raw_save_review_does_not_replace_a_newer_explicit_validation', async () => {
    const oldCheck = deferred<RawCheck>();
    let checks = 0;
    const latest = { ...rawCheck, diff: 'latest diff' };
    const d = new RawDraft({ file: 'global' }, 'yaml', (async (method: string) => {
      if (method.endsWith('.get')) return rawView('base');
      return checks++ === 0 ? oldCheck.promise : latest;
    }) as Caller, async () => false, () => {});
    await d.load();
    d.setText('submitted');
    const saving = d.save();
    await d.check();
    oldCheck.resolve(rawCheck);
    await saving;
    expect(d.lastCheck).toBe(latest);
  });

  test('value_save_preserves_new_edits_and_coalesces_submission', async () => {
    const write = deferred<string>();
    const writes: string[] = [];
    const d = new ValueDraft('workdir', 'work directory', 'base', async (value) => { writes.push(value); return write.promise; }, null, () => {});
    d.set('submitted');
    const first = d.save();
    d.set('base');
    d.rebase('submitted');
    const second = d.save();
    write.resolve('submitted');
    await Promise.all([first, second]);
    expect(writes).toEqual(['submitted']);
    expect(d.base).toBe('submitted');
    expect(d.value).toBe('base');
    expect(d.dirtyCount()).toBe(1);
  });

  test('save_all_coalesces_and_keeps_saving_until_its_transaction_finishes', async () => {
    const write = deferred<string>();
    const d = new ValueDraft('workdir', 'work directory', 'base', async () => write.promise, null, () => {});
    const set = new EditSet(() => {});
    set.register(d);
    d.set('submitted');
    const first = set.saveAll();
    const second = set.saveAll();
    expect(set.saving).toBe(true);
    write.resolve('submitted');
    const [a, b] = await Promise.all([first, second]);
    expect(a).toBe(b);
    expect(a.saved).toBe(1);
    expect(set.saving).toBe(false);
  });
});

describe('草稿', () => {
  test('flipping_a_switch_back_leaves_nothing_unsaved', () => {
    const { d } = draftWith(async () => {
      throw new Error('不该联网');
    });
    d.setView(view('r1', null, true));
    // a 继承来的生效值是开:关掉再打开,结果没变
    d.setShown('a', false);
    expect(d.dirtyCount()).toBe(1);
    d.setShown('a', true);
    expect(d.dirtyCount()).toBe(0);
    // b 本层写了开:同样拨回来不算改动
    d.setShown('b', false);
    d.setShown('b', true);
    expect(d.dirtyCount()).toBe(0);
  });

  test('draft_tracks_changes_and_reset', () => {
    const { d } = draftWith(async () => {
      throw new Error('不该联网');
    });
    d.setView(view('r1', null, true));
    expect(d.dirtyCount()).toBe(0);
    d.set('a', false);
    expect(d.isChanged('a')).toBe(true);
    expect(d.shown('a')).toEqual({ value: false, inherited: false });
    // 改回原值不算改动
    d.set('a', null);
    expect(d.isChanged('a')).toBe(false);
    d.set('a', false);
    d.inherit('b');
    expect(d.changeList()).toEqual([
      { key: 'a', value: false },
      { key: 'b', value: null },
    ]);
    // 恢复继承的草稿:控件显示继承来的生效值
    expect(d.shown('b').inherited).toBe(true);
    d.reset();
    expect(d.dirtyCount()).toBe(0);
    expect(d.shown('b')).toEqual({ value: true, inherited: false });
  });

  test('save_bar_counts_restart_items', () => {
    const { d } = draftWith(async () => ({}) as never);
    d.setView(view('r1'));
    const set = new EditSet(() => {});
    set.register(d);
    d.set('a', false);
    d.set('b', true);
    d.set('c', 9);
    expect(set.dirtyCount()).toBe(3);
    expect(set.restartCount()).toBe(2);
    d.revert('c');
    expect(set.restartCount()).toBe(1);
  });

  test('conflict_keeps_draft_and_offers_reload', async () => {
    let server = view('r2', true, null);
    const call = (async (method: string, params: { revision?: string }) => {
      if (method === 'config.patch') {
        if (params.revision !== server.revision) throw new ApiCallError({ code: 'conflict', message: '这个文件刚被别处改过' });
        return { revision: 'r3', effects: [], restart_pending: [] };
      }
      if (method === 'config.get') return server;
      throw new Error(method);
    }) as unknown as Caller;
    const { d } = draftWith(call);
    d.setView(view('r1'));
    d.set('b', true);
    const set = new EditSet(() => {});
    set.register(d);
    const sum = await set.saveAll();
    expect(sum.saved).toBe(0);
    expect(sum.failed).toHaveLength(1);
    expect(d.hasConflict()).toBe(true);
    expect(set.conflicts()).toEqual([d]);
    // 草稿原样保留
    expect(d.changeList()).toEqual([{ key: 'b', value: true }]);
    // 重新载入:换上别处改过的新原值,草稿还在,冲突解除
    await set.reloadConflicts();
    expect(d.hasConflict()).toBe(false);
    expect(d.view?.revision).toBe('r2');
    expect(d.fieldValue('a')?.value).toBe(true);
    expect(d.changeList()).toEqual([{ key: 'b', value: true }]);
    // 再保存就带上新版本
    server = { ...server, revision: 'r2' };
    const again = await set.saveAll();
    expect(again.saved).toBe(1);
    expect(d.dirtyCount()).toBe(0);
  });

  test('invalid_error_lands_on_its_row', async () => {
    const call = (async () => {
      throw new ApiCallError({ code: 'invalid', message: '取值范围 1–20', key: 'c' });
    }) as unknown as Caller;
    const { d } = draftWith(call);
    d.setView(view('r1'));
    d.set('c', 99);
    const out = await d.save();
    expect(out.ok).toBe(false);
    expect(d.errors.get('c')).toBe('取值范围 1–20');
    expect(d.hasConflict()).toBe(false);
  });
});
