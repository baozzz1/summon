import { afterEach, expect, test, vi } from 'vitest';
import type { ConfigView, SaveResult, WhoAmI } from '../api/types';
import whoami from '../api/fixtures/session.whoami.json';
import { ApiCallError } from '../api/client';
import { store } from '../app/store';
import { setLanguage } from '../i18n';
import { LanguageMenu } from './language-menu';

const view = (value = 'auto', revision = 'r1', editable = true): ConfigView => ({
  scope: { level: 'global' }, revision,
  values: [{ key: 'language', value, effective: value, source: 'global', overridden_by: [], editable }],
});
const saved: SaveResult = { revision: 'r2', effects: [{ key: 'language', effect: 'immediate' }], restart_pending: [] };
const menus: LanguageMenu[] = [];
afterEach(async () => { menus.splice(0).forEach((m) => m.remove()); vi.restoreAllMocks(); store.stopPolling(); await setLanguage('zh-CN'); });
async function mount(machine = true) {
  store.state = { ...store.state, whoami: { ...whoami.data, machine } as WhoAmI };
  const menu = new LanguageMenu(); menus.push(menu); document.body.append(menu);
  await vi.waitFor(() => expect(menu.querySelectorAll('input')).toHaveLength(3));
  await menu.updateComplete;
  return menu;
}
const radio = (menu: LanguageMenu, value: string) => menu.querySelector<HTMLInputElement>(`input[value="${value}"]`)!;

test('saves_only_machine_language_with_revision_and_updates_after_confirmation', async () => {
  let complete!: (value: SaveResult) => void;
  const patch = new Promise<SaveResult>((resolve) => { complete = resolve; });
  const call = vi.spyOn(store, 'call').mockImplementation(async (method) => method === 'config.get' ? view() : patch);
  const refresh = vi.spyOn(store, 'refreshStatus').mockImplementation(async () => { await setLanguage('en'); return ['global']; });
  const menu = await mount();
  await vi.waitFor(() => expect(radio(menu, 'en').disabled).toBe(false));
  radio(menu, 'en').click(); radio(menu, 'zh-CN').click();
  await menu.updateComplete;
  expect(radio(menu, 'auto').checked).toBe(true);
  expect(radio(menu, 'en').disabled).toBe(true);
  expect(call.mock.calls.filter(([m]) => m === 'config.patch')).toEqual([
    ['config.patch', { scope: { level: 'global' }, revision: 'r1', changes: [{ key: 'language', value: 'en' }] }],
  ]);
  complete(saved);
  await vi.waitFor(() => expect(menu.querySelector('summary')?.textContent).toBe('Language'));
  expect(radio(menu, 'en').checked).toBe(true);
  expect(refresh.mock.calls).toEqual([[]]);
  expect(menu.textContent).toContain('CLI');
  expect(menu.textContent).toContain('Menu Bar');
});

test('failed_save_preserves_selection_and_reload_uses_the_new_revision', async () => {
  const call = vi.spyOn(store, 'call').mockImplementation(async (method) => {
    if (method === 'config.get') return view();
    throw new ApiCallError({ code: 'conflict', message: 'revision changed' });
  });
  const refresh = vi.spyOn(store, 'refreshStatus').mockResolvedValue([]);
  const menu = await mount();
  await vi.waitFor(() => expect(radio(menu, 'en').disabled).toBe(false));
  radio(menu, 'en').click();
  await vi.waitFor(() => expect(menu.querySelector('[role="alert"]')?.textContent).toContain('revision changed'));
  expect(radio(menu, 'auto').checked).toBe(true);
  expect(refresh).not.toHaveBeenCalled();
  call.mockImplementation(async (method) => method === 'config.get' ? view('zh-CN', 'new-revision') : saved);
  menu.querySelector<HTMLButtonElement>('button')!.click();
  await vi.waitFor(() => expect(radio(menu, 'zh-CN').checked).toBe(true));
  radio(menu, 'en').click();
  await vi.waitFor(() => expect(call.mock.calls.filter(([m]) => m === 'config.patch')).toHaveLength(2));
  expect(call.mock.calls.at(-1)?.[1]).toMatchObject({ revision: 'new-revision' });
});

test('read_failure_and_both_permission_gates_disable_writes', async () => {
  const call = vi.spyOn(store, 'call').mockRejectedValue(new Error('unavailable'));
  const failed = await mount();
  await vi.waitFor(() => expect(failed.querySelector('[role="alert"]')).toBeTruthy());
  expect([...failed.querySelectorAll('input')].every((i) => i.disabled)).toBe(true);
  call.mockClear();
  const denied = await mount(false);
  expect([...denied.querySelectorAll('input')].every((i) => i.disabled)).toBe(true);
  expect(call).not.toHaveBeenCalled();
  call.mockResolvedValue(view('en', 'r1', false));
  const fieldDenied = await mount();
  await vi.waitFor(() => expect(radio(fieldDenied, 'en').checked).toBe(true));
  radio(fieldDenied, 'zh-CN').click();
  expect(call.mock.calls.some(([method]) => method === 'config.patch')).toBe(false);
});

test('opening_reloads_preference_and_escape_restores_focus', async () => {
  const call = vi.spyOn(store, 'call').mockResolvedValue(view());
  const menu = await mount();
  await vi.waitFor(() => expect(radio(menu, 'en').disabled).toBe(false));
  call.mockResolvedValue(view('en', 'r2'));
  const details = menu.querySelector('details')!;
  details.open = true; details.dispatchEvent(new Event('toggle'));
  await vi.waitFor(() => expect(radio(menu, 'en').checked).toBe(true));
  call.mockRejectedValue(new Error('unavailable'));
  details.open = false; details.open = true; details.dispatchEvent(new Event('toggle'));
  await vi.waitFor(() => expect(menu.querySelector('[role="alert"]')).toBeTruthy());
  expect(radio(menu, 'en').checked).toBe(true);
  expect([...menu.querySelectorAll('input')].every((i) => i.disabled)).toBe(true);
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
  expect(details.open).toBe(false);
  expect(document.activeElement).toBe(menu.querySelector('summary'));
});
