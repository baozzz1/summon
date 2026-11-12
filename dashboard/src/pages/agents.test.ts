import { afterEach, describe, expect, test, vi } from 'vitest';
import type { AgentDetail, AgentSummary } from '../api/types';
import { store } from '../app/store';
import { confirmDialog } from '../components/dialog';
import { AgentsPage } from './agents';

vi.mock('../components/dialog', () => ({ confirmDialog: vi.fn() }));
const base: AgentSummary = { alias: 'example', name: 'example', display: 'Example', cli: 'codex', enabled: false, routable: true, credentials: 'ready', chats: 1 };
const pages: AgentsPage[] = [];
afterEach(() => { pages.splice(0).forEach((p) => p.remove()); vi.restoreAllMocks(); });
async function pageFor(agents: AgentSummary[], allowed = true) {
  const call = vi.spyOn(store, 'call').mockImplementation(async (method, params) => {
    if (method === 'agents.list') return agents;
    if (method === 'agents.get') return { can_toggle_enabled: allowed, deny_reason: allowed ? undefined : '无权限' } as AgentDetail;
    if (method === 'agents.set_enabled') return { ...base, enabled: (params as { enabled: boolean }).enabled };
    throw new Error(method);
  });
  vi.spyOn(store, 'refreshStatus').mockResolvedValue([]);
  const page = new AgentsPage(); pages.push(page);
  document.body.append(page);
  await vi.waitFor(() => expect(page.loading).toBe(false));
  await page.updateComplete;
  return { page, call, toggle: () => page.querySelector<HTMLButtonElement>('[role="switch"]')! };
}

describe('agent 卡片开关', () => {
  test('uses_a_separate_button_and_updates_only_after_server_success', async () => {
    const { page, call, toggle } = await pageFor([base]);
    expect(toggle().closest('a')).toBeNull();
    expect(page.querySelector('a')?.getAttribute('href')).toContain('/agents/example');
    toggle().click(); toggle().click();
    await vi.waitFor(() => expect(toggle().getAttribute('aria-checked')).toBe('true'));
    expect(call.mock.calls.filter(([m]) => m === 'agents.set_enabled')).toEqual([['agents.set_enabled', { agent: 'example', enabled: true }]]);
  });
  test('asks_before_disabling_and_preserves_state_on_cancel', async () => {
    vi.mocked(confirmDialog).mockResolvedValue(false);
    const { call, toggle } = await pageFor([{ ...base, enabled: true }]);
    toggle().click();
    await vi.waitFor(() => expect(confirmDialog).toHaveBeenCalled());
    expect(call.mock.calls.some(([m]) => m === 'agents.set_enabled')).toBe(false);
    expect(toggle().getAttribute('aria-checked')).toBe('true');
  });
  test('disables_unavailable_and_unauthorized_actions', async () => {
    const denied = await pageFor([base], false);
    expect(denied.toggle().disabled).toBe(true);
    const broken = await pageFor([{ ...base, load_error: 'invalid YAML' }]);
    expect(broken.toggle().disabled).toBe(true);
  });
  test('preserves_state_and_releases_the_button_after_failure', async () => {
    const { call, toggle } = await pageFor([base]);
    call.mockImplementation(async (method) => { throw new Error(method); });
    toggle().click();
    await vi.waitFor(() => expect(toggle().disabled).toBe(false));
    expect(toggle().getAttribute('aria-checked')).toBe('false');
    expect(document.body.textContent).toContain('修改失败');
  });
});
