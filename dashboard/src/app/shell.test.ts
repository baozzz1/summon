import { describe, expect, test, vi } from 'vitest';
import { App } from './shell';
import { createMockTransport, MockServer } from '../api/mock';
import type { PageBase } from '../components/page';
import { setLanguage } from '../i18n';
import '../pages/global';
import { store } from './store';

describe('外壳', () => {
  test('unauthenticated_shows_reopen_screen', async () => {
    store.init(async () => ({ ok: false, error: { code: 'unauthenticated', message: '登录已失效' } }));
    const el = document.createElement('sm-app') as HTMLElement & { updateComplete: Promise<boolean> };
    document.body.appendChild(el);
    await store.boot();
    await el.updateComplete;
    expect(store.state.auth).toBe('expired');
    const notice = el.querySelector('[data-testid="reopen"]');
    expect(notice).toBeTruthy();
    expect(notice!.textContent).toContain('请从飞书重新打开控制台');
    el.remove();
  });
});


test('language_menu_in_the_topbar_keeps_other_global_drafts_and_replaces_the_web_field', async () => {
  const server = new MockServer({ identity: 'owner' });
  store.init(createMockTransport(server));
  store.state = { ...store.state, auth: 'loading' };
  await store.boot();
  const app = new App();
  app.route = { page: 'global', mode: 'form', group: 'web' };
  document.body.append(app);
  try {
    await vi.waitFor(() => expect(app.querySelector('[data-key="dashboard.port"] input')).toBeTruthy());
    const page = app.querySelector('sm-page-global') as PageBase;
    const input = page.querySelector<HTMLInputElement>('[data-key="dashboard.port"] input')!;
    const originalPort = server.global['dashboard.port'];
    input.value = '8001';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    await page.updateComplete;
    expect(page.edits.dirtyCount()).toBe(1);
    expect(page.querySelector('[data-key="language"]')).toBeNull();
    const language = app.querySelector<HTMLInputElement>('.topbar sm-language-menu input[value="en"]')!;
    await vi.waitFor(() => expect(language.disabled).toBe(false));
    language.click();
    await vi.waitFor(() => expect(document.documentElement.lang).toBe('en'));
    await vi.waitFor(() => expect(page.stale).toContain('global'));
    expect(app.querySelector('sm-page-global')).toBe(page);
    expect(page.edits.dirtyCount()).toBe(1);
    expect(input.value).toBe('8001');
    expect(server.global['dashboard.port']).toBe(originalPort);
    expect(server.global.language).toBe('en');
  } finally {
    app.remove(); store.stopPolling(); await setLanguage('zh-CN');
  }
});
