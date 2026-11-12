import { t as translate } from '../i18n';
// History API 路由表,写法同 OpenClaw 经典版 navigation.ts:路径 ↔ 路由一一对应,页面模块按需加载。
import type { LogFile } from '../api/types';

export const AGENT_TABS = [
  { id: 'overview', get label() { return translate('daaaf44'); } },
  { id: 'runtime', get label() { return translate('dc3bdfb'); } },
  { id: 'permissions', get label() { return translate('d978cbc'); } },
  { id: 'messaging', get label() { return translate('d4da199'); } },
  { id: 'chats', get label() { return translate('d35b49e'); } },
  { id: 'docs', get label() { return translate('d4262c4'); } },
  { id: 'yaml', label: 'YAML' },
] as const;

export type AgentTab = (typeof AGENT_TABS)[number]['id'];

export type Route =
  | { page: 'overview' }
  /** group:全局设置的哪一页(字段表的分组 id,或 doc = 全局说明);没写时由 key 或第一组定 */
  | { page: 'global'; mode: 'form' | 'yaml'; group?: string; key?: string }
  | { page: 'agents' }
  | { page: 'agent'; alias: string; tab: AgentTab; key?: string }
  | { page: 'chats' }
  | { page: 'chat'; chatId: string; agent?: string }
  | { page: 'schedules' }
  | { page: 'comments'; agent?: string }
  | { page: 'sessions' }
  | { page: 'session'; agent: string; chatId: string }
  | { page: 'usage' }
  | { page: 'insights' }
  /** 日志页的条件放在地址里:会话详情里「看这段时间的日志」直接链过来 */
  | { page: 'logs'; file?: LogFile; agent?: string; since?: string; until?: string }
  | { page: 'not_found'; path: string };

export type PageId = Route['page'];

/** 导航条目高亮用:详情页归到它的列表条目下。 */
export function navIdFor(route: Route): string {
  switch (route.page) {
    case 'agent':
      return 'agents';
    case 'chat':
      return 'chats';
    case 'session':
      return 'sessions';
    default:
      return route.page;
  }
}

function isAgentTab(v: string): v is AgentTab {
  return AGENT_TABS.some((t) => t.id === v);
}

export function parseRoute(pathname: string, search = ''): Route {
  const q = new URLSearchParams(search);
  const parts = pathname.replace(/\/+$/, '').split('/').filter(Boolean).map(decodeURIComponent);
  const key = q.get('key') ?? undefined;
  const withKey = <T extends object>(r: T): T => (key ? { ...r, key } : r);
  if (parts.length === 0) return { page: 'overview' };
  const [head, a, b] = parts;
  if (head === 'global' && parts.length <= 2) {
    const mode = q.get('mode') === 'yaml' ? 'yaml' : 'form';
    return withKey(a ? { page: 'global', mode, group: a } : { page: 'global', mode });
  }
  if (head === 'agents') {
    if (parts.length === 1) return { page: 'agents' };
    if (a && parts.length <= 3) {
      const tab = b ?? 'overview';
      if (isAgentTab(tab)) return withKey({ page: 'agent', alias: a, tab });
    }
  }
  if (head === 'chats') {
    if (parts.length === 1) return { page: 'chats' };
    if (a && parts.length === 2) {
      const agent = q.get('agent');
      return agent ? { page: 'chat', chatId: a, agent } : { page: 'chat', chatId: a };
    }
  }
  if (head === 'schedules' && parts.length === 1) return { page: 'schedules' };
  if (head === 'usage' && parts.length === 1) return { page: 'usage' };
  if (head === 'insights' && parts.length === 1) return { page: 'insights' };
  if (head === 'logs' && parts.length === 1) {
    const r: Extract<Route, { page: 'logs' }> = { page: 'logs' };
    const file = q.get('file');
    if (file === 'daemon' || file === 'sidecar' || file === 'audit') r.file = file;
    for (const k of ['agent', 'since', 'until'] as const) {
      const v = q.get(k);
      if (v) r[k] = v;
    }
    return r;
  }
  if (head === 'comments' && parts.length === 1) {
    const agent = q.get('agent');
    return agent ? { page: 'comments', agent } : { page: 'comments' };
  }
  if (head === 'sessions') {
    if (parts.length === 1) return { page: 'sessions' };
    if (a && b && parts.length === 3) return { page: 'session', agent: a, chatId: b };
  }
  return { page: 'not_found', path: pathname };
}

export function hrefFor(route: Route): string {
  const enc = encodeURIComponent;
  switch (route.page) {
    case 'overview':
      return '/';
    case 'global': {
      const q = new URLSearchParams();
      if (route.mode === 'yaml') q.set('mode', 'yaml');
      if (route.key) q.set('key', route.key);
      const s = q.toString();
      const base = route.group ? `/global/${enc(route.group)}` : '/global';
      return s ? `${base}?${s}` : base;
    }
    case 'agents':
      return '/agents';
    case 'agent': {
      const base = `/agents/${enc(route.alias)}/${route.tab}`;
      return route.key ? `${base}?key=${enc(route.key)}` : base;
    }
    case 'chats':
      return '/chats';
    case 'chat':
      return route.agent ? `/chats/${enc(route.chatId)}?agent=${enc(route.agent)}` : `/chats/${enc(route.chatId)}`;
    case 'schedules':
      return '/schedules';
    case 'comments':
      return route.agent ? `/comments?agent=${enc(route.agent)}` : '/comments';
    case 'sessions':
      return '/sessions';
    case 'session':
      return `/sessions/${enc(route.agent)}/${enc(route.chatId)}`;
    case 'usage':
      return '/usage';
    case 'insights':
      return '/insights';
    case 'logs': {
      const q = new URLSearchParams();
      for (const k of ['file', 'agent', 'since', 'until'] as const) {
        const v = route[k];
        if (v) q.set(k, v);
      }
      const s = q.toString();
      return s ? `/logs?${s}` : '/logs';
    }
    case 'not_found':
      return route.path;
  }
}

type LeaveGuard = (next: Route) => Promise<boolean>;
let leaveGuard: LeaveGuard | null = null;

/** 页面有草稿时登记一个离开前的确认;返回 false 就留在原地。 */
export function setLeaveGuard(guard: LeaveGuard | null): void {
  leaveGuard = guard;
}

export function clearLeaveGuard(guard: LeaveGuard): void {
  if (leaveGuard === guard) leaveGuard = null;
}

export const ROUTE_EVENT = 'sm-route';

export async function navigate(href: string, opts: { replace?: boolean } = {}): Promise<void> {
  const target = new URL(href, location.href);
  if (leaveGuard && !(await leaveGuard(parseRoute(target.pathname, target.search)))) return;
  if (opts.replace) history.replaceState(null, '', target.pathname + target.search);
  else history.pushState(null, '', target.pathname + target.search);
  window.dispatchEvent(new Event(ROUTE_EVENT));
}

export function currentRoute(): Route {
  return parseRoute(location.pathname, location.search);
}

/** 站内链接走 History API;带修饰键、新窗口打开、外链与下载照常交给浏览器。 */
export function interceptLinks(root: HTMLElement): void {
  root.addEventListener('click', (ev) => {
    if (ev.defaultPrevented || ev.button !== 0 || ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey) return;
    const a = (ev.target as Element | null)?.closest?.('a[href]');
    if (!(a instanceof HTMLAnchorElement)) return;
    if (a.target || a.hasAttribute('download') || a.origin !== location.origin) return;
    if (!a.pathname.startsWith('/') || a.pathname.startsWith('/api/')) return;
    ev.preventDefault();
    void navigate(a.pathname + a.search);
  });
  window.addEventListener('popstate', () => window.dispatchEvent(new Event(ROUTE_EVENT)));
}
