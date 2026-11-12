import { t as translate } from '../i18n';
// 列表与页头里共用的几句人话。只看服务端给的字段,不推断配置。
import type { AgentSummary, ChatSummary, Identity } from '../api/types';

/** 群头像上的一个字:有名字取首字,私聊与取不到名字的群用 #。 */
export function chatInitial(c: Pick<ChatSummary, 'kind' | 'name'>): string {
  if (c.kind === 'p2p') return [...(c.name ?? '#')][0] ?? '#';
  return c.name ? ([...c.name][0] ?? '#') : '#';
}

/** 群名取不到时标题就是 chat id,副标题里不另写一遍。 */
export function hasChatName(c: Pick<ChatSummary, 'kind' | 'name'>): boolean {
  return !!c.name;
}

export function chatTitle(c: Pick<ChatSummary, 'kind' | 'name' | 'chat_id'>): string {
  if (c.kind === 'p2p') return translate('d1e1d07', [c.name ?? translate('dddfdac')]);
  return c.name ?? c.chat_id;
}

/** RestartItem.file 与 status.revisions 的键同形:global / agent:<alias>。 */
export function fileLabel(file: string): string {
  if (file === 'global') return 'config.yaml';
  if (file.startsWith('agent:')) return translate('db77c1d', [file.slice(6)]);
  return file;
}

/** agent 的一句状态:读取失败 / 停用 / 需重启 / 凭据缺失 / 未上线 / 在线 */
export function agentState(a: AgentSummary, restartFiles: string[]): { text: string; tone: 'ok' | 'warn' | 'danger' | '' } {
  if (a.load_error) return { text: translate('de5ed04'), tone: 'danger' };
  if (!a.enabled) return { text: translate('da8c369'), tone: '' };
  if (restartFiles.includes(`agent:${a.alias}`)) return { text: translate('d7c4436'), tone: 'warn' };
  if (a.credentials === 'missing') return { text: translate('d17289b'), tone: 'danger' };
  if (!a.routable) return { text: translate('d2ca248'), tone: 'warn' };
  return { text: translate('db90866'), tone: 'ok' };
}

export function agentLine(a: AgentSummary): string {
  if (a.load_error) return translate('dce82d1');
  return [a.cli, a.model, a.effort].filter(Boolean).join(' · ');
}

/**
 * 整机层预览里机器人叫什么:签发链接的 agent 启用且在线就用它,否则第一个启用且在线的;
 * 都没有时写「agent」。停用或读不出的 agent 不拿来当示例。
 */
export function previewBotName(agents: AgentSummary[], identity: Identity | undefined): string {
  const live = agents.filter((a) => a.enabled && a.routable && !a.load_error);
  const pick = (identity?.kind === 'feishu' && live.find((a) => a.alias === identity.agent)) || live[0];
  return pick ? pick.display || pick.alias : 'agent';
}
