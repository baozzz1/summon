// 控制台前后端契约。Rust 侧 crates/summon/src/runtime/dashboard/api/ 的 serde 结构与这里一一对应;
// Rust 契约测试把每个方法的样例响应写进 dashboard/src/api/fixtures/,前端测试逐个按这里的类型校验。
// 两边任何一边改了形状而另一边没跟上,两边的测试至少挂一边。

export type Effect = 'immediate' | 'next_turn' | 'new_session' | 'next_poll' | 'restart';
export type Layer = 'builtin' | 'global' | 'agent' | 'chat';
export type ChatKind = 'p2p' | 'group';

export type Scope =
  | { level: 'global' }
  | { level: 'agent'; agent: string }
  | { level: 'chat'; agent: string; chat_id: string };

export type ErrorCode =
  | 'unauthenticated'
  | 'forbidden'
  | 'conflict'
  | 'invalid'
  | 'not_found'
  | 'busy'
  | 'unavailable'
  | 'internal';

export interface ApiError {
  code: ErrorCode;
  message: string;
  /** 出错的那一项的键路径(表单把错误挂到这一行下面) */
  key?: string;
}

export type ApiResult<T> = { ok: true; data: T } | { ok: false; error: ApiError };

// ── 会话 ────────────────────────────────────────────────────────────────

export type Identity =
  | { kind: 'owner' }
  | { kind: 'feishu'; name: string; agent: string; chat_id: string; chat_kind: ChatKind };

export interface WhoAmI {
  identity: Identity;
  /** 每个 /api 请求放进 X-Summon-Csrf 头 */
  csrf: string;
  expires_at: string;
  hostname: string;
  version: string;
  /** 能不能改整机设置;日志页也只给这些人 */
  machine: boolean;
}

// ── 字段表 ──────────────────────────────────────────────────────────────

export type FieldKind =
  | 'bool'
  | 'enum'
  | 'int'
  | 'duration_secs'
  | 'string'
  | 'path'
  | 'string_list'
  | 'string_map';

/** identity / runtime / permissions / messaging / system:决定字段出现在哪一页、哪个页签 */
export type Section = 'identity' | 'runtime' | 'permissions' | 'messaging' | 'system';

export interface FieldOption {
  value: string;
  label: string;
}

export interface FieldDesc {
  /** YAML 键路径,如 messaging.cot_visible、runtime.cli、model */
  key: string;
  label: string;
  description: string;
  group: string;
  kind: FieldKind;
  options?: FieldOption[];
  min?: number;
  max?: number;
  layers: Array<'global' | 'agent' | 'chat'>;
  effect: Effect;
  sensitive: boolean;
  /** form:通用表单;guarded:有专门校验或流程;readonly:只显示 */
  write: 'form' | 'guarded' | 'readonly';
  /** 换个取值飞书效果预览会变 */
  preview: boolean;
  /**
   * 开关显示的是 YAML 键值取反后的含义(如 messaging.hide_thinking 显示为「显示思考」)。
   * value / effective 已按页面语义给出,前端不必再取反;只在键路径旁提示。缺省 = 不取反。
   */
  inverted?: boolean;
}

export interface FieldGroup {
  id: string;
  label: string;
  description?: string;
  section: Section;
}

export interface Schema {
  groups: FieldGroup[];
  fields: FieldDesc[];
}

export interface FieldValue {
  key: string;
  /** 本层写的值;null = 本层没写(继承) */
  value: unknown | null;
  effective: unknown;
  source: Layer;
  /** 上层页面用:下层覆盖了这一项的 agent 别名或「agent · 群名」 */
  overridden_by: string[];
  editable: boolean;
  deny_reason?: string;
}

export interface ConfigView {
  scope: Scope;
  revision: string;
  values: FieldValue[];
  /** 这一层生效的表情安全集(value = 表情键,label = 含义),表情键只能从里面选;群这一层没有 */
  reactions?: FieldOption[];
}

/** lark-cli 里登录的飞书用户,即 sentinel 认作 owner 的那个人 */
export interface LarkUser {
  /** 在 lark-cli 那个应用里的 open_id */
  open_id: string;
  name?: string;
}

/** value 为 null = 删掉本层的键,恢复继承 */
export interface ConfigChange {
  key: string;
  value: unknown | null;
}

export interface RestartItem {
  key: string;
  file: string;
}

export interface SaveResult {
  revision: string;
  effects: Array<{ key: string; effect: Effect }>;
  restart_pending: RestartItem[];
}

// ── YAML 原文 ───────────────────────────────────────────────────────────

export type RawFile = { file: 'global' } | { file: 'agent'; agent: string };

export interface RawView {
  revision: string;
  /** 敏感值已换成占位符 */
  text: string;
  path: string;
  editable: boolean;
}

export interface RawIssue {
  line?: number;
  col?: number;
  key?: string;
  message: string;
  suggestion?: string;
}

export interface RawCheck {
  /** 统一差异格式(改动前 → 改动后,敏感值保持占位) */
  diff: string;
  errors: RawIssue[];
  warnings: RawIssue[];
}

// ── 概览 ────────────────────────────────────────────────────────────────

export interface AgentSummary {
  alias: string;
  name: string;
  display: string;
  cli: string;
  model?: string;
  effort?: string;
  enabled: boolean;
  routable: boolean;
  load_error?: string;
  credentials: 'ready' | 'missing';
  chats: number;
}

export interface StatusView {
  language: 'zh-CN' | 'en';
  daemon: {
    version: string;
    commit: string;
    started_at: string;
    pid: number;
    autostart: 'on' | 'off' | 'unknown';
  };
  hostname: string;
  restart_pending: RestartItem[];
  agents: AgentSummary[];
  /** 'global'、'agent:<alias>'、'chat:<alias>:<chat_id>'、'schedules'、'doc:<路径>' → 版本 */
  revisions: Record<string, string>;
}

export interface IssueLink {
  page: 'global' | 'agent' | 'chat' | 'schedules' | 'session';
  agent?: string;
  chat_id?: string;
  key?: string;
}

export interface Issue {
  severity: 'error' | 'warn';
  title: string;
  detail: string;
  fix: string;
  link?: IssueLink;
}

// ── Agents ──────────────────────────────────────────────────────────────

export interface Member {
  id: string;
  name?: string;
}

export interface AgentDetail {
  summary: AgentSummary;
  app_id_env: string;
  scope: string;
  model_candidates: string[];
  effort_options: string[];
  /** 名单里出现的全部 ID 及能查到的飞书名字 */
  members: Member[];
  can_toggle_enabled: boolean;
  deny_reason?: string;
}

// ── 群 ──────────────────────────────────────────────────────────────────

export interface ChatSummary {
  chat_id: string;
  kind: ChatKind;
  name?: string;
  agents: Array<{ agent: string; overrides: number; live: boolean; last_active?: string }>;
  last_active?: string;
}

export type WorkDirSource = 'chat_override' | 'chat_mapping' | 'agent_default';

export interface ChatAgentView {
  agent: string;
  /** scope = chat 的按群开关;value 为 null 表示继承 */
  config: ConfigView;
  mentionless: { enabled: boolean; can_disable: boolean; can_enable: boolean; reason?: string };
  operators: { members: Member[]; revision: string; editable: boolean; reason?: string };
  work_dir: {
    effective?: string;
    source?: WorkDirSource;
    mapping?: string;
    override?: string;
    live: boolean;
    editable: boolean;
    reason?: string;
  };
  doc: { exists: boolean };
  schedules: ScheduleView[];
}

export interface ChatDetail {
  chat_id: string;
  kind: ChatKind;
  name?: string;
  agents: ChatAgentView[];
}

export interface MentionlessJob {
  job: string;
  status: 'pending' | 'done' | 'failed';
  message?: string;
  /** 缺应用权限时的申请链接 */
  apply_url?: string;
}

// ── 定时任务 ────────────────────────────────────────────────────────────

export interface ScheduleView {
  id: string;
  agent: string;
  chat_id: string;
  chat_name?: string;
  kind: string;
  spec: string;
  human: string;
  next_fire?: string;
  last_fired?: string;
  prompt: string;
  output: string;
  context: string;
  enabled: boolean;
  created_by: string;
  editable: boolean;
}

export interface WhenCheck {
  ok: boolean;
  human?: string;
  next: string[];
  error?: string;
  forms?: string;
}

export interface ScheduleEdit {
  when?: string;
  prompt?: string;
  output?: string;
  context?: string;
}

// ── 说明文件 ────────────────────────────────────────────────────────────

export type DocRef =
  | { level: 'global' }
  | { level: 'agent'; agent: string; file: 'CLAUDE.md' | 'SOUL.md' }
  | { level: 'chat'; agent: string; chat_id: string };

export interface DocView {
  exists: boolean;
  /** 文件不存在时没有版本;保存时传 null 表示新建 */
  revision?: string;
  text: string;
  path: string;
  editable: boolean;
  effect: Effect;
}

// ── 飞书效果预览 ────────────────────────────────────────────────────────

export interface PreviewRequest {
  scope: Scope;
  draft: ConfigChange[];
}

export type CotItem =
  | { id: string; type: 'thinking'; text: string }
  | { id: string; type: 'tool'; icon: string; name: string; args?: string; result?: string }
  | { id: string; type: 'text'; text: string };

export type PreviewMessage =
  | { id: string; kind: 'user'; sender: string; text: string; reactions: string[] }
  | { id: string; kind: 'cot'; title: string; status: 'done' | 'error'; items: CotItem[] }
  /** card 是飞书卡片 2.0 JSON;每个元素带 element_id,regions 引用的就是它 */
  | { id: string; kind: 'card'; card: unknown }
  | { id: string; kind: 'feed'; chat_name: string; text: string };

export interface PreviewView {
  title: string;
  messages: PreviewMessage[];
  /** 字段键路径 → 它控制的元素 id(消息 id、CoT 条目 id 或卡片 element_id) */
  regions: Record<string, string[]>;
}

// ── 会话与台账 ───────────────────────────────────────────────────────────

/** 给人看的会话状态,排序即处理的先后 */
export type SessionState = 'leaked' | 'blocked' | 'waiting' | 'working' | 'idle';
export type SessionAction = 'handled' | 'interrupt' | 'exit' | 'archive';

/** 这一行此刻能做的动作(服务端已按看的人的权限算好) */
export interface SessionActions {
  handled: boolean;
  interrupt: boolean;
  exit: boolean;
  archive: boolean;
}

export interface SessionRow {
  agent: string;
  agent_display: string;
  cli?: string;
  tag: string;
  chat_id?: string;
  chat_type?: string;
  name?: string;
  state: SessionState;
  /** 会话还在列表里;不在的只剩一条等人看的回复 */
  listed: boolean;
  busy: boolean;
  turn_started_at?: string;
  /** 在跑那一轮的凭证(打断按它围栏) */
  turn_id?: string;
  /** permission / question / auth */
  blocked?: string;
  waiting_since?: string;
  /** 等人看的那条回复(已处理按它清) */
  waiting_reply?: string;
  link?: string;
  last_active?: string;
  capture: string;
  actions: SessionActions;
  deny_reason?: string;
}

/** 评论任务只读状态，审批在飞书卡片完成。 */
export interface CommentTask {
  id: string;
  agent: string;
  title: string;
  stage: string;
  status: string;
  status_en: string;
  busy: boolean;
  received_ms: number;
  updated_ms: number;
  pid: number | null;
  error: string | null;
}

export interface SessionsView {
  comment_tasks: CommentTask[];
  comment_tasks_error: string | null;
  sessions: SessionRow[];
}

export type TurnOrigin = 'human' | 'bot' | 'schedule' | 'sentinel' | 'slash' | 'internal' | 'recovery';
export type TurnEnd = 'done' | 'empty' | 'silent_reset' | 'failed' | 'interrupted';

/** 一轮里同一模型、同一输入档位、同一服务档的请求合在一起的 token;input 是未命中缓存的输入 */
export interface ModelUsage {
  /** CLI 没报模型时为空 */
  model: string;
  /** 单次请求的输入总量超过的分档门槛(千 token):100 / 200 / 272;没超过时没有 */
  band?: number;
  /** 非标准的服务档,如 priority */
  variant?: string;
  requests: number;
  input: number;
  output: number;
  cache_read: number;
  cache_write_5m: number;
  cache_write_1h: number;
}

/** 四项 token:input 是未命中缓存的输入,cache_write 是两档缓存写入之和 */
export interface TokenCounts {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
}

/** 四项费用(美元),只含计了价的部分 */
export interface CostBreakdown {
  total: number;
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
}

/** token 与按公开价格表算的费用;unpriced 是价格表里找不到模型或单价的部分(models 里空串是没报模型) */
export interface Spend {
  tokens: TokenCounts;
  cost: CostBreakdown;
  unpriced?: { tokens: number; models: string[] };
}

/** 台账里的一轮;spend 是这一轮报了 token 时按价格表算的 */
export interface TurnRecord {
  agent: string;
  chat_id: string;
  tag: string;
  turn?: string;
  origin: TurnOrigin;
  schedule?: string;
  requester?: string;
  message_id: string;
  prompt_head?: string;
  started_at_ms: number;
  ended_at_ms: number;
  end: TurnEnd;
  error?: string;
  reattached?: boolean;
  tool_steps: number;
  cli?: string;
  model?: string;
  effort?: string;
  ctx?: { used?: number; window?: number; pct?: number };
  usage?: ModelUsage[];
  spend?: Spend;
  quota?: { five_hour_left?: number; five_hour_resets_at?: number; long_left?: number; long_resets_at?: number };
  feed?: string;
}

/** 一段轮外用量:不在任何一轮里的 claude 请求(如两轮之间还在跑的后台子 agent) */
export interface OutsideUsage {
  from_ms: number;
  to_ms: number;
  usage: ModelUsage[];
  spend?: Spend;
}

export interface TurnsPage {
  turns: TurnRecord[];
  more: boolean;
  /** 只按会话查时有:这一页覆盖的时间段里这个会话的轮外用量,新的在前 */
  outside?: OutsideUsage[];
}

export interface SessionDetail {
  agent: string;
  chat_id: string;
  tag: string;
  name?: string;
  /** 会话已结束时没有 */
  session?: SessionRow;
  turns: TurnRecord[];
  more: boolean;
  outside?: OutsideUsage[];
}

export type ActOutcome = 'done' | 'running' | 'pending' | 'absent' | 'stale' | 'not_running' | 'superseded';

// ── 日志 ────────────────────────────────────────────────────────────────

export type LogFile = 'daemon' | 'sidecar' | 'audit';

/** 日志里的一条(可以跨多行),已脱敏 */
export interface LogLine {
  at?: string;
  text: string;
  /** 含 error / panic / fail 或「失败」「错误」 */
  error: boolean;
}

export interface LogsPage {
  file: LogFile;
  entries: LogLine[];
  /** 更早一页的翻页位置;往前没有了就没有 */
  before?: string;
}

// ── 用量 ────────────────────────────────────────────────────────────────

export type UsageRange = 'today' | '7d' | '30d' | '90d' | 'all' | 'custom' | 'relative';
export interface UsageQuery { range: UsageRange; from?: string; to?: string; hours?: number; agent?: string; chat_id?: string }
export type UsageUnit = 'hour' | 'day';

export interface UsageCounts {
  done: number;
  empty: number;
  silent_reset: number;
  failed: number;
  interrupted: number;
}

export interface UsageBucket extends UsageCounts {
  /** 时段起点(服务端时区) */
  start: string;
}

/** 表里的一行:一个 agent、一个会话、一种 CLI 或一个模型;轮数与耗时只算轮,token 与费用含轮外用量 */
export interface UsageRow {
  key: string;
  label: string;
  agent?: string;
  chat_id?: string;
  /** 会话一行才有 */
  tag?: string;
  turns: number;
  failed: number;
  interrupted: number;
  seconds: number;
  avg_secs: number;
  p90_secs: number;
  /** 一轮都没报 token 时没有 */
  spend?: Spend;
}

/** 按天的一行:这一天(服务端日历)结束的轮与开始的轮外用量 */
export interface UsageDay {
  /** 这一天的零点 */
  start: string;
  turns: number;
  spend?: Spend;
}

export interface QuotaPoint {
  at_ms: number;
  five_hour_left?: number;
  long_left?: number;
}

/** 一个 agent 最近一次的额度读数(at = 读到它的那一轮结束的时刻)与时间段里的曲线;重置时刻是 Unix 秒 */
export interface UsageQuota {
  agent: string;
  label: string;
  cli?: string;
  at: string;
  five_hour_left?: number;
  five_hour_resets_at?: number;
  long_left?: number;
  long_resets_at?: number;
  points: QuotaPoint[];
}

export interface UsageView {
  range: UsageRange;
  today: string;
  scopes: Array<{ agent: string; agent_label: string; chat_id: string; chat_label: string }>;
  unit: UsageUnit;
  since: string;
  until: string;
  /** working / waiting:此刻在跑(含卡住)与等人回复的会话数,取不到会话状态时没有 */
  /** spend 含轮外用量,outside 是其中轮外用量的部分 */
  totals: { turns: number; seconds: number; failed: number; interrupted: number; spend?: Spend; outside?: Spend; working?: number; waiting?: number };
  buckets: UsageBucket[];
  by_agent: UsageRow[];
  by_chat: UsageRow[];
  by_cli: UsageRow[];
  /** 模型轮数每轮每模型计一次,费用只计该模型请求 */
  by_model: UsageRow[];
  /** 有轮结束或有轮外用量的那几天,新的在前 */
  days: UsageDay[];
  quota: UsageQuota[];
  /** 计价用的价格表;盘上还没有时没有,费用一律未计价 */
  prices?: { source: string; fetched_at: string };
}

// ── 洞察 ────────────────────────────────────────────────────────────────
//
// 间隔 = 同一会话里同一个人:上一轮(真人发起、正常完成)结束 → 下一条消息开始。不到 2 秒的是轮内
// 已发出的排队消息,不算间隔。除注明外,分布一律按当日口径:上一轮结束在服务端本地
// [day_start_hour, day_end_hour) 点之间,且下一条消息在同一个自然日。时长单位为秒,比例为 0–1。

export interface InsightsQuery { range: UsageRange; from?: string; to?: string; hours?: number; agent?: string }

/** 一组时长的分位数(线性插值);n = 0 时全为 0 */
export interface InsightsQuantiles { n: number; p50_secs: number; p75_secs: number; p90_secs: number; total_secs: number }

/** 时长分档:<1 分钟、1–5 分钟、5–15 分钟、15–60 分钟、1–4 小时、4 小时以上(左闭右开) */
export type InsightsSpan = 'lt_1m' | 'm1_5' | 'm5_15' | 'm15_60' | 'h1_4' | 'gte_4h';
export type InsightsLongSpan = 'm15_60' | 'h1_4' | 'gte_4h';

export interface InsightsBucket { key: InsightsSpan; count: number; secs: number }

/** 一档长间隔期间的别处活动;「别处」= 其它 agent 或其它会话,不区分发消息的人 */
export interface InsightsElsewhere {
  key: InsightsLongSpan;
  count: number;
  secs: number;
  /** 期间其它会话里有真人发起的轮的间隔条数 */
  active_elsewhere: number;
  /** 期间其它会话里真人发起的轮数的中位数 */
  elsewhere_messages_p50: number;
}

/** 下一条消息的类型:较长的新指令、短指令、放行(继续 / 可以 / 选项)、询问进展 */
export type InsightsNextKind = 'instruction' | 'short' | 'approve' | 'poll';
export interface InsightsNext { key: InsightsNextKind; count: number; p50_secs: number; p90_secs: number }

/** 时间段里的一个自然日(服务端日历):真人发起的轮数与涉及的会话数;weekday 为 ISO 星期,1 = 周一 … 7 = 周日 */
export interface InsightsDay { date: string; weekday: number; messages: number; chats: number }

/** 有消息的工作日(周一至周五)的每日输入量 */
export interface InsightsWorkdays { days: number; messages_p50: number; messages_p90: number; chats_p50: number; chats_p90: number }

/** 紧邻的、天数相同的上一时段 */
export interface InsightsPrevious { gaps: InsightsQuantiles; over_15m_count_share: number; over_15m_secs_share: number }

/**
 * 规则生成的结论,句子由界面按语言渲染。参数(比例 0–1,时长为秒):
 * - sample_small { n }:当日口径间隔不足 30 条,此时只有这一条
 * - tail_concentration { count_share, secs_share }:15 分钟以上的间隔占总时长 ≥ 70%
 * - busy_elsewhere { share, silent_share }:长间隔 ≥ 20 条且期间别处有消息的占 ≥ 60%
 * - mostly_instructions { approve_share, approve_p50_secs }:放行 < 10% 且新指令与短指令 ≥ 80%
 * - polls { count, p50_secs }:询问进展 ≥ 5 条
 * - load { messages_p50, chats_p50 }:有消息的工作日 ≥ 3 天
 * - human_vs_agent { ratio }:间隔合计 / agent 单轮耗时合计 ≥ 1.5
 * - trend { direction: 'up' | 'down', p50_secs, previous_p50_secs }:上一时段 ≥ 30 条且中位数变化 ≥ 25%
 */
export type InsightsFindingKey =
  | 'sample_small'
  | 'tail_concentration'
  | 'busy_elsewhere'
  | 'mostly_instructions'
  | 'polls'
  | 'load'
  | 'human_vs_agent'
  | 'trend';
export interface InsightsFinding { key: InsightsFindingKey; params: Record<string, number | string> }

export interface InsightsSummary {
  range: UsageRange;
  /** 日历范围为 YYYY-MM-DD(含两端);relative 为精确 RFC3339 时间戳(左闭右开) */
  from: string;
  to: string;
  /** 时间段里开始的轮;human_turns 是其中真人发起的,agents / chats 是这些轮涉及的 agent 数与会话数 */
  turns: number;
  human_turns: number;
  agents: number;
  chats: number;
  day_start_hour: number;
  day_end_hour: number;
  gaps: InsightsQuantiles;
  /** 不限当日口径的间隔条数(含过夜) */
  gaps_including_overnight: number;
  /** 排队消息条数 */
  queued: number;
  /** 恒为 6 项,顺序同 InsightsSpan */
  buckets: InsightsBucket[];
  /** agent 单轮耗时:真人发起、正常完成的轮 */
  agent_turns: InsightsQuantiles;
  /** 恒为 3 项,顺序同 InsightsLongSpan */
  elsewhere: InsightsElsewhere[];
  /** 15 分钟以上的间隔条数;long_gaps_silent 是其中所有会话都无人发消息的最长一段 ≥ 间隔 80% 的条数 */
  long_gaps: number;
  long_gaps_silent: number;
  /** 恒为 4 项,顺序同 InsightsNextKind */
  next_messages: InsightsNext[];
  /** 时间段里的每一天,早的在前,没有消息的日子也在 */
  days: InsightsDay[];
  workdays: InsightsWorkdays;
  /** range = all 时没有 */
  previous?: InsightsPrevious;
  findings: InsightsFinding[];
}

// ── 洞察分析 ────────────────────────────────────────────────────────────
//
// 由主 agent 对 insights.summary 的结果做一次分析:只把计数与时长交给模型,文本由模型生成,与规则
// 结论分开展示。结果按「时间段 + agent 筛选 + 语言」存取;数据有更新或主 agent / 模型 / 提示词版本
// 变了,上一份照样返回、标为 stale。同一份数据与主 agent / 模型只跑一次。

export type UiLanguage = 'zh-CN' | 'en';
export interface InsightsAnalysisQuery extends InsightsQuery { lang: UiLanguage }

/** 主 agent 怎么定的:配置显式指定 / 近 30 天 token 用量最多 / 还没有用量时最早配置的 */
export type PrimarySource = 'configured' | 'usage' | 'first';
export interface AnalysisPrimary {
  agent: string;
  label: string;
  source: PrimarySource;
  cli: string;
  /** 没配模型(用 CLI 默认)时没有 */
  model?: string;
}

export type AnalysisStatus = 'none' | 'running' | 'done' | 'failed' | 'unsupported';

export interface AnalysisUsage { input: number; output: number; cache_read: number; cache_write: number }

export interface AnalysisResult {
  /** Markdown;超过上限时截断,truncated = true */
  text: string;
  truncated: boolean;
  generated_at: string;
  agent: string;
  cli: string;
  model?: string;
  elapsed_ms: number;
  prompt_version: number;
  /** CLI 输出里带了用量才有 */
  usage?: AnalysisUsage;
  /** 生成之后数据有更新,或主 agent / 模型 / 提示词版本变了 */
  stale: boolean;
  /** 这份结果之后最近一次重新生成没成功(原因按请求的语言);下一次成功时消失 */
  retry_failure?: { reason: string; at: string };
}

export interface AnalysisView {
  /** 没有可用的 agent 时没有 */
  primary?: AnalysisPrimary;
  status: AnalysisStatus;
  /** status = done 时有 */
  result?: AnalysisResult;
  /** failed / unsupported 的原因,按请求的语言 */
  reason?: string;
  /** status = running 时:开始的时刻 */
  started_at?: string;
  /** 有整机设置的授权且主 agent 支持;发起时另一项分析在跑会回 busy */
  can_start: boolean;
}

// ── 方法表:方法名 → [参数, 返回] ─────────────────────────────────────────

/** 停用 agent 的备份与清理任务，只向创建者返回。 */
export interface CleanupJob {
  job: string;
  agent: string;
  status: 'preparing' | 'ready' | 'cleaning' | 'done' | 'failed';
  message: string | null;
  bytes: number | null;
  sha256: string | null;
  filename: string;
  paths: string[] | null;
  downloaded: boolean;
}

export interface AttentionItem {
  id: string;
  agent: string;
  agent_display: string;
  chat_name: string;
  source: 'agent_ask' | 'claude_question' | 'claude_permission' | 'codex_question' | 'codex_approval';
  summary: string;
  question_count: number;
  registered_at: string;
  waited_secs: number;
  link: string;
  link_target: 'message' | 'chat';
}
export interface AttentionView {
  items: AttentionItem[];
  observed_at: string;
  coverage: 'structured_requests';
  partial: boolean;
}

export interface Methods {
  'session.whoami': [Record<string, never>, WhoAmI];
  'status.get': [Record<string, never>, StatusView];
  'status.issues': [Record<string, never>, Issue[]];
  'config.schema': [Record<string, never>, Schema];
  'config.get': [{ scope: Scope }, ConfigView];
  'config.patch': [{ scope: Scope; revision: string; changes: ConfigChange[] }, SaveResult];
  'config.raw.get': [RawFile, RawView];
  'config.raw.check': [RawFile & { text: string }, RawCheck];
  'config.raw.put': [RawFile & { revision: string; text: string }, SaveResult];
  'agents.list': [Record<string, never>, AgentSummary[]];
  'agents.get': [{ agent: string }, AgentDetail];
  'agents.cleanup.prepare': [{ agent: string }, CleanupJob];
  'agents.cleanup.status': [{ job: string }, CleanupJob];
  'agents.cleanup.download': [{ job: string; offset: number }, { offset: number; next: number; done: boolean; base64: string }];
  'agents.cleanup.confirm': [{ job: string; saved: boolean; agent: string }, CleanupJob];
  'agents.set_enabled': [{ agent: string; enabled: boolean }, AgentSummary];
  'chats.list': [Record<string, never>, ChatSummary[]];
  'chats.get': [{ chat_id: string }, ChatDetail];
  'chats.set_mentionless': [{ agent: string; chat_id: string; enabled: boolean }, MentionlessJob];
  'chats.mentionless_job': [{ job: string }, MentionlessJob];
  'chats.set_operators': [
    { agent: string; chat_id: string; revision: string; members: string[] },
    SaveResult,
  ];
  'chats.set_work_dir': [
    {
      agent: string;
      chat_id: string;
      /** agent.yaml 的版本(同 ChatAgentView.operators.revision),按群映射存在那里 */
      revision: string;
      mapping: string | null;
      clear_override: boolean;
    },
    ChatAgentView,
  ];
  'schedules.list': [Record<string, never>, ScheduleView[]];
  'schedules.check_when': [{ when: string }, WhenCheck];
  'schedules.edit': [{ id: string; edit: ScheduleEdit }, ScheduleView];
  'schedules.set_enabled': [{ id: string; enabled: boolean }, ScheduleView];
  'docs.get': [{ doc: DocRef }, DocView];
  'docs.put': [{ doc: DocRef; revision: string | null; text: string }, DocView];
  'preview.render': [PreviewRequest, PreviewView];
  /** 群 ID → 群名,open_id / union_id → 人名;查不到的不在结果里 */
  'names.resolve': [{ agent?: string; ids: string[] }, Record<string, string>];
  /** profile 是页面上此刻填的 lark-cli 配置名(可能没保存);不带 = 默认配置 */
  'sentinel.lark_user': [{ profile?: string }, LarkUser];
  'comments.stop': [{ agent: string; id: string }, { stopped: boolean }];
  'attention.list': [Record<string, never>, AttentionView];
  'sessions.list': [Record<string, never>, SessionsView];
  'sessions.get': [{ agent: string; chat_id: string }, SessionDetail];
  /** 退出 / 归档时 interrupt = 在跑的那一轮打断也行(人确认过) */
  'sessions.act': [
    { agent: string; chat_id: string; action: SessionAction; interrupt?: boolean; turn_id?: string; reply?: string },
    { outcome: ActOutcome },
  ];
  /** before:上一页最后一轮的结束时刻(毫秒) */
  'turns.list': [{ agent: string; chat_id?: string; schedule?: string; before?: number; limit?: number }, TurnsPage];
  'usage.summary': [UsageQuery, UsageView];
  'insights.summary': [InsightsQuery, InsightsSummary];
  'insights.analysis.get': [InsightsAnalysisQuery, AnalysisView];
  /** 发起要有整机设置的授权;同一份数据已有结果不重跑(过期了直接重跑),force = true 时重新生成 */
  'insights.analysis.start': [InsightsAnalysisQuery & { force?: boolean }, AnalysisView];
  'logs.tail': [
    { file: LogFile; before?: string; limit?: number; query?: string; agent?: string; since?: string; until?: string; errors?: boolean },
    LogsPage,
  ];
}

export type Method = keyof Methods;
export type Params<M extends Method> = Methods[M][0];
export type Returns<M extends Method> = Methods[M][1];
