import { afterEach, describe, expect, test, vi } from 'vitest';
import type { WhoAmI } from '../api/types';
import { createMockTransport, MockServer } from '../api/mock';
import { MOCK_SCHEMA } from '../api/mock-schema';
import { NAV_GROUPS, PAGES } from '../app/nav';
import { hrefFor, parseRoute } from '../app/router';
import { store } from '../app/store';
import { CommentsPage } from './comments';

const TIMEOUT = 'messaging.document_comment_timeout_secs';
const OWNER_MENTIONS = 'messaging.document_comments_owner_mentions_enabled';
const NON_OWNER = 'messaging.document_comments_non_owner_enabled';
const ENABLED = 'messaging.document_comments_enabled';

async function mount(agent?: string, denied = false) {
  const srv = new MockServer();
  const transport = createMockTransport(srv);
  store.init(async (method, body, headers) => {
    if (denied && method === 'config.get') return { ok: false, error: { code: 'forbidden', message: '没有此配置范围的修改权限' } };
    return transport(method, body, headers);
  });
  store.state = { ...store.state, auth: 'ok', schema: MOCK_SCHEMA,
    whoami: { identity: { kind: 'owner' }, csrf: 'csrf', expires_at: '', hostname: 'test', version: 'test', machine: true } as WhoAmI };
  const page = document.createElement('sm-page-comments') as CommentsPage;
  page.route = agent ? { page: 'comments', agent } : { page: 'comments' };
  document.body.append(page);
  await vi.waitFor(() => expect(page.querySelector(denied ? '.banner' : `[data-key="${TIMEOUT}"]`)).not.toBeNull());
  return { srv, page };
}

afterEach(() => { document.body.replaceChildren(); });

describe('独立文档评论页面', () => {
  test('routes_navigation_and_module_are_registered', () => {
    expect(parseRoute('/comments')).toEqual({ page: 'comments' });
    expect(parseRoute('/comments', '?agent=writer')).toEqual({ page: 'comments', agent: 'writer' });
    expect(hrefFor({ page: 'comments', agent: 'writer' })).toBe('/comments?agent=writer');
    expect(NAV_GROUPS.flatMap((g) => g.items).find((i) => i.id === 'comments')?.label).toBe('文档评论');
    expect(PAGES.comments.tag).toBe('sm-page-comments');
  });

  test('global_defaults_and_saved_agent_override_keep_other_fields_unchanged', async () => {
    const { page, srv } = await mount();
    const input = page.querySelector<HTMLInputElement>(`[data-key="${TIMEOUT}"] input`)!;
    expect(input.value).toBe('5');
    expect(input.min).toBe('1');
    expect(page.querySelector(`[data-key="${ENABLED}"] [role="switch"]`)!.getAttribute('aria-checked')).toBe('false');
    input.value = '2'; input.dispatchEvent(new Event('input', { bubbles: true }));
    await page.save();
    expect(srv.global[TIMEOUT]).toBe(120);
    expect(srv.global[ENABLED]).toBeUndefined();
    page.route = { page: 'comments', agent: 'starlight' };
    await page.updateComplete;
    await vi.waitFor(() => expect(page.textContent).toContain('继承'));
    await page.refresh(); await page.updateComplete;
    const scoped = page.querySelector<HTMLInputElement>(`[data-key="${TIMEOUT}"] input`)!;
    expect(scoped.value).toBe('2');
    scoped.value = '10'; scoped.dispatchEvent(new Event('input', { bubbles: true }));
    await page.save();
    const view = await store.call('config.get', { scope: { level: 'agent', agent: 'starlight' } });
    expect(view.values.find((v) => v.key === TIMEOUT)?.value).toBe(600);
    expect(srv.global[TIMEOUT]).toBe(120);
    expect(page.textContent).toContain('评论任务');
    const reset = [...page.querySelectorAll<HTMLButtonElement>(`[data-key="${TIMEOUT}"] button`)].find((b) => b.textContent?.includes('继承'))!;
    reset.click(); await page.save();
    const inherited = await store.call('config.get', { scope: { level: 'agent', agent: 'starlight' } });
    expect(inherited.values.find((v) => v.key === TIMEOUT)?.source).toBe('global');
    expect(inherited.values.find((v) => v.key === TIMEOUT)?.effective).toBe(120);
  });

  test('non_owner_comments_can_be_enabled_globally_and_overridden_per_agent', async () => {
    const { page, srv } = await mount();
    const toggle = () => page.querySelector<HTMLButtonElement>(`[data-key="${NON_OWNER}"] [role="switch"]`)!;
    expect(toggle().getAttribute('aria-checked')).toBe('false');
    toggle().click(); await page.save();
    expect(srv.global[NON_OWNER]).toBe(true);
    expect(srv.global[ENABLED]).toBeUndefined();
    page.route = { page: 'comments', agent: 'starlight' };
    await page.updateComplete; await page.refresh(); await page.updateComplete;
    expect(toggle().getAttribute('aria-checked')).toBe('true');
    toggle().click(); await page.save();
    const view = await store.call('config.get', { scope: { level: 'agent', agent: 'starlight' } });
    expect(view.values.find((v) => v.key === NON_OWNER)?.value).toBe(false);
    expect(srv.global[NON_OWNER]).toBe(true);
    const other = await store.call('config.get', { scope: { level: 'agent', agent: 'arcadia' } });
    expect(other.values.find((v) => v.key === NON_OWNER)?.effective).toBe(true);
    const reset = [...page.querySelectorAll<HTMLButtonElement>(`[data-key="${NON_OWNER}"] button`)].find((b) => b.textContent?.includes('继承'))!;
    reset.click(); await page.save();
    const inherited = await store.call('config.get', { scope: { level: 'agent', agent: 'starlight' } });
    expect(inherited.values.find((v) => v.key === NON_OWNER)?.source).toBe('global');
    expect(inherited.values.find((v) => v.key === NON_OWNER)?.effective).toBe(true);
  });

  test('owner_mentions_can_be_enabled_globally_and_overridden_per_agent', async () => {
    const { page, srv } = await mount();
    const toggle = () => page.querySelector<HTMLButtonElement>(`[data-key="${OWNER_MENTIONS}"] [role="switch"]`)!;
    expect(toggle().getAttribute('aria-checked')).toBe('false');
    toggle().click(); await page.save();
    expect(srv.global[OWNER_MENTIONS]).toBe(true);
    expect(srv.global[ENABLED]).toBeUndefined();
    page.route = { page: 'comments', agent: 'starlight' };
    await page.updateComplete; await page.refresh(); await page.updateComplete;
    expect(toggle().getAttribute('aria-checked')).toBe('true');
    toggle().click(); await page.save();
    const view = await store.call('config.get', { scope: { level: 'agent', agent: 'starlight' } });
    expect(view.values.find((v) => v.key === OWNER_MENTIONS)?.value).toBe(false);
    expect(srv.global[OWNER_MENTIONS]).toBe(true);
  });

  test('inherited_global_changes_refresh_without_overwriting_unsaved_edits', async () => {
    const { page, srv } = await mount('starlight');
    srv.global[TIMEOUT] = 120;
    store.dispatchEvent(new CustomEvent('revisions', { detail: ['global'] }));
    await vi.waitFor(() => expect(page.querySelector<HTMLInputElement>(`[data-key="${TIMEOUT}"] input`)!.value).toBe('2'));
    const input = page.querySelector<HTMLInputElement>(`[data-key="${TIMEOUT}"] input`)!;
    input.value = '9'; input.dispatchEvent(new Event('input', { bubbles: true }));
    srv.global[TIMEOUT] = 180;
    store.dispatchEvent(new CustomEvent('revisions', { detail: ['global'] }));
    await page.updateComplete;
    expect(input.value).toBe('9');
    expect(page.textContent).toContain('其他位置被修改');
  });

  test('cancelled_scope_change_restores_picker_and_preserves_draft', async () => {
    const { page } = await mount();
    const input = page.querySelector<HTMLInputElement>(`[data-key="${TIMEOUT}"] input`)!;
    input.value = '9'; input.dispatchEvent(new Event('input', { bubbles: true }));
    const picker = page.querySelector<HTMLSelectElement>('select[aria-label="配置范围"]')!;
    picker.value = 'starlight'; picker.dispatchEvent(new Event('change', { bubbles: true }));
    await vi.waitFor(() => expect(document.querySelector('dialog')).not.toBeNull());
    const keep = [...document.querySelectorAll<HTMLButtonElement>('dialog button')].find((b) => b.textContent === '继续编辑')!;
    keep.click();
    await vi.waitFor(() => expect(picker.value).toBe(''));
    expect(input.value).toBe('9');
    expect(page.edits.dirtyCount()).toBe(1);
  });

  test('denied_config_does_not_offer_editable_fields', async () => {
    const { page } = await mount(undefined, true);
    expect(page.textContent).toContain('没有此配置范围的修改权限');
    expect(page.querySelector(`[data-key="${TIMEOUT}"] input`)).toBeNull();
  });
});
