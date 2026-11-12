import { t as translate } from '../i18n';
import type { Schema } from '../api/types';
import type { IconName } from '../components/icons';
import { EFFECT_LABEL } from '../util/format';
import type { PageId, Route } from './router';

export interface NavItem {
  id: string;
  label: string;
  href: string;
  icon: IconName;
  /** 只给能改整机设置的人 */
  machine?: boolean;
}

export interface NavGroup {
  label: string;
  items: NavItem[];
}

// 新页面组(以后的监控、统计)在这里加一组,再在 PAGES 里登记模块;界面上不放任何未实现的入口。
export const NAV_GROUPS: NavGroup[] = [
  { get label() { return translate('da33db5'); }, items: [{ id: 'overview', get label() { return translate('dfea405'); }, href: '/', icon: 'layoutDashboard' }] },
  {
    get label() { return translate('d75b269'); },
    items: [
      { id: 'sessions', get label() { return translate('da63280'); }, href: '/sessions', icon: 'activity' },
      { id: 'usage', get label() { return translate('d42723e'); }, href: '/usage', icon: 'chartColumn' },
      { id: 'insights', get label() { return translate('d9d7407'); }, href: '/insights', icon: 'lightbulb' },
      { id: 'logs', get label() { return translate('d7dbac1'); }, href: '/logs', icon: 'scrollText', machine: true },
    ],
  },
  {
    get label() { return translate('d148d19'); },
    items: [
      { id: 'global', get label() { return translate('d3f754f'); }, href: '/global', icon: 'slidersHorizontal' },
      { id: 'agents', label: 'Agents', href: '/agents', icon: 'bot' },
      { id: 'chats', get label() { return translate('d35b49e'); }, href: '/chats', icon: 'messagesSquare' },
    ],
  },
  { get label() { return translate('d829a35'); }, items: [{ id: 'comments', get label() { return translate('d2549c4'); }, href: '/comments', icon: 'messagesSquare' }, { id: 'schedules', get label() { return translate('d3c39e9'); }, href: '/schedules', icon: 'alarmClock' }] },
];

/** 全局说明在二级菜单里的 id(其余各页是字段表的分组 id)。 */
export const GLOBAL_DOC = 'doc';

export interface GlobalSection {
  id: string;
  label: string;
  description: string;
}

/** 全局设置列出含全局字段的分组，最后是全局说明。 */
export function globalSections(schema: Schema | null): GlobalSection[] {
  const groups = (schema?.groups ?? []).filter((g) => schema?.fields.some((f) => f.group === g.id && f.layers.includes('global')));
  return [
    ...groups.map((g) => ({ id: g.id, label: g.label, description: g.description ?? '' })),
    { id: GLOBAL_DOC, label: translate('ddd96b8'), description: translate('d73e010', [EFFECT_LABEL.new_session]) },
  ];
}

/** 这一页是全局设置的哪一组:地址里写了就用它;只带了 key 就用那一项所在的组;都没有用第一组。 */
export function globalGroupOf(route: Extract<Route, { page: 'global' }>, schema: Schema | null): string {
  const ids = new Set(globalSections(schema).map((s) => s.id));
  if (route.group && ids.has(route.group)) return route.group;
  const byKey = route.key ? schema?.fields.find((f) => f.key === route.key)?.group : undefined;
  if (byKey && ids.has(byKey)) return byKey;
  return globalSections(schema)[0]!.id;
}

/** 全局设置页告诉导航哪几组有未保存的改动(detail:组 id 列表),二级菜单上标一个点。 */
export const GLOBAL_DIRTY_EVENT = 'sm-global-dirty';

export interface PageModule {
  tag: string;
  load: () => Promise<unknown>;
}

export const PAGES: Record<Exclude<PageId, 'not_found'>, PageModule> = {
  overview: { tag: 'sm-page-overview', load: () => import('../pages/overview') },
  global: { tag: 'sm-page-global', load: () => import('../pages/global') },
  agents: { tag: 'sm-page-agents', load: () => import('../pages/agents') },
  agent: { tag: 'sm-page-agent', load: () => import('../pages/agent-detail') },
  chats: { tag: 'sm-page-chats', load: () => import('../pages/chats') },
  chat: { tag: 'sm-page-chat', load: () => import('../pages/chat-detail') },
  schedules: { tag: 'sm-page-schedules', load: () => import('../pages/schedules') },
  comments: { tag: 'sm-page-comments', load: () => import('../pages/comments') },
  sessions: { tag: 'sm-page-sessions', load: () => import('../pages/sessions') },
  session: { tag: 'sm-page-session', load: () => import('../pages/session-detail') },
  usage: { tag: 'sm-page-usage', load: () => import('../pages/usage') },
  insights: { tag: 'sm-page-insights', load: () => import('../pages/insights') },
  logs: { tag: 'sm-page-logs', load: () => import('../pages/logs') },
};

export const PAGE_TITLES: Record<PageId, string> = {
  get overview() { return translate('dfea405'); },
  get global() { return translate('d3f754f'); },
  agents: 'Agents',
  agent: 'Agent',
  get chats() { return translate('d35b49e'); },
  get chat() { return translate('d9fca8b'); },
  get schedules() { return translate('d3c39e9'); },
  get comments() { return translate('d2549c4'); },
  get sessions() { return translate('da63280'); },
  get session() { return translate('da63280'); },
  get usage() { return translate('d42723e'); },
  get insights() { return translate('d9d7407'); },
  get logs() { return translate('d7dbac1'); },
  get not_found() { return translate('de75737'); },
};
