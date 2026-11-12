// 内存里的一份 summon:只在 `npm run dev` 与截图构建(--mode mock)里加载,生产产物里不存在。
// 它扮演 Rust 侧的角色(分层合并、校验、版本、预览渲染),让每个页面都能完整地跑、改、存,
// 也能演示冲突与失败。真实行为以 Rust 实现为准。
import { regionDiff } from '../preview/diff';
import type { Transport } from './client';
import configGetFixture from './fixtures/config.get.json';
import attentionListFixture from './fixtures/attention.list.json';
import sessionsListFixture from './fixtures/sessions.list.json';
import turnsListFixture from './fixtures/turns.list.json';
import { BUILTIN, MOCK_SCHEMA } from './mock-schema';
import type {
  AnalysisPrimary,
  AnalysisView,
  InsightsAnalysisQuery,
  InsightsDay,
  InsightsFinding,
  InsightsLongSpan,
  InsightsNextKind,
  InsightsQuantiles,
  InsightsQuery,
  InsightsSpan,
  InsightsSummary,
  LogFile,
  LogLine,
  LogsPage,
  OutsideUsage,
  Spend,
  UsageBucket,
  UsageCounts,
  UsageQuota,
  UsageQuery,
  UsageRow,
  UsageUnit,
  UsageView,
  ActOutcome,
  SessionAction,
  SessionRow,
  TurnRecord,
  AgentDetail,
  AgentSummary,
  ApiError,
  ApiResult,
  ChatAgentView,
  ChatDetail,
  ChatKind,
  ChatSummary,
  ConfigChange,
  ConfigView,
  CotItem,
  DocRef,
  DocView,
  Effect,
  FieldDesc,
  FieldValue,
  Identity,
  Issue,
  Layer,
  Member,
  MentionlessJob,
  PreviewMessage,
  PreviewView,
  RawCheck,
  RawFile,
  RawIssue,
  RestartItem,
  SaveResult,
  ScheduleView,
  Scope,
  StatusView,
  WhenCheck,
} from './types';

export const PLACEHOLDER = '«summon:redacted»';

class MockFail extends Error {
  constructor(readonly error: ApiError) {
    super(error.message);
  }
}
const fail = (code: ApiError['code'], message: string, key?: string): never => {
  throw new MockFail(key ? { code, message, key } : { code, message });
};

// ── 小工具 ──────────────────────────────────────────────────────────────

function fnv(s: string): string {
  let h1 = 0x811c9dc5;
  let h2 = 0x01000193;
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    h1 = Math.imul(h1 ^ c, 0x01000193) >>> 0;
    h2 = Math.imul(h2 ^ c, 0x5bd1e995) >>> 0;
  }
  return (h1.toString(16).padStart(8, '0') + h2.toString(16).padStart(8, '0')).slice(0, 16);
}

const clone = <T>(v: T): T => (v === undefined ? v : (JSON.parse(JSON.stringify(v)) as T));
const eq = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);
const pad = (n: number) => String(n).padStart(2, '0');

function localIso(d: Date): string {
  const off = -d.getTimezoneOffset();
  const sign = off >= 0 ? '+' : '-';
  const a = Math.abs(off);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}${sign}${pad(Math.floor(a / 60))}:${pad(a % 60)}`;
}

// ── 人物与 ID(全部虚构) ─────────────────────────────────────────────────

const SELF_UNION = 'on_00000000000000000000000000000001';
const SELF_OPEN = 'ou_00000000000000000000000000000001';
const ZHOU = 'on_00000000000000000000000000000002';
const CHEN = 'ou_00000000000000000000000000000003';
const XU = 'on_00000000000000000000000000000004';

/** 内置的表情安全集与含义,取自 Rust 生成的 config.get 样例。 */
const REACTIONS = (configGetFixture as unknown as { data: ConfigView }).data.reactions ?? [];

const NAMES: Record<string, string> = {
  [SELF_UNION]: '示例用户',
  [SELF_OPEN]: '示例用户',
  [ZHOU]: '另一位用户',
  [CHEN]: '示例同事',
  [XU]: '值班同事',
};

const CHAT_DEV = 'oc_00000000000000000000000000000001';
const CHAT_OPS = 'oc_00000000000000000000000000000002';
const CHAT_RES = 'oc_00000000000000000000000000000003';
const CHAT_P2P = 'oc_00000000000000000000000000000009';
const CHAT_UNNAMED = 'oc_00000000000000000000000000000004';

// ── 种子数据 ────────────────────────────────────────────────────────────

interface MockAgent {
  alias: string;
  enabled: boolean;
  routable: boolean;
  credentials: 'ready' | 'missing';
  load_error?: string;
  app_id_env: string;
  scope: string;
  layer: Record<string, unknown>;
  candidates: string[];
  efforts: string[];
}

interface ChatAgentState {
  overrides: Record<string, boolean>;
  live: boolean;
  last_active?: string;
  operators: string[];
  override?: string;
  mentionless: boolean;
}

interface MockChat {
  chat_id: string;
  kind: ChatKind;
  name?: string;
  agents: Record<string, ChatAgentState>;
}

interface MockSchedule extends Omit<ScheduleView, 'chat_name' | 'editable'> {
  when: string;
}

interface DocState {
  text: string;
  exists: boolean;
}

const SATURN_BROKEN_LINE = '  operators: - ou_00000000000000000000000000000005';

const SATURN_RAW = `name: saturn
display: "Saturn"
status: active
model: claude-sonnet-5

feishu:
  app_id_env: FEISHU_SATURN_APP_ID
  scope: thread
  allowed_users:
    - ${SELF_UNION}
  # 下面这一行少了换行:列表项不能直接跟在冒号后面
${SATURN_BROKEN_LINE}

runtime:
  cli: claude
  working_dir: "~/.summon/agents/saturn/workspace"
`;

export interface MockOptions {
  identity?: 'feishu' | 'owner';
  now?: Date;
}

export class MockServer {
  readonly now: Date;
  private readonly t0: number;
  identity: Identity;
  global: Record<string, unknown>;
  agents: MockAgent[];
  chats: MockChat[];
  schedules: MockSchedule[];
  docs: Map<string, DocState>;
  raw = new Map<string, string>();
  bumps = new Map<string, number>();
  boot: Map<string, unknown>;
  jobs = new Map<string, { agent: string; chat_id: string; at: number; outcome: MentionlessJob }>();
  sessions?: SessionRow[];
  turns?: TurnRecord[];
  /** 洞察分析的各槽状态(时间段、筛选、语言) */
  analyses = new Map<string, MockAnalysisSlot>();
  jobSeq = 0;

  constructor(opts: MockOptions = {}) {
    this.now = opts.now ?? new Date();
    this.t0 = this.now.getTime();
    this.identity =
      opts.identity === 'owner'
        ? { kind: 'owner' }
        : { kind: 'feishu', name: '示例用户', agent: 'starlight', chat_id: CHAT_P2P, chat_kind: 'p2p' };

    this.global = {
      'messaging.idle_retire_secs': 86400,
      'messaging.progress_max_steps': 5,
      'messaging.default_model_by_cli': { codex: 'gpt-5.6-sol' },
      'sentinel.enabled': true,
      'sentinel.dry_run': true,
      'preview_server.public_base_url': 'http://10.0.0.8:7765',
      'tools.lark_cli': '/opt/homebrew/bin/lark-cli',
      'menubar.terminal': ['ghostty'],
    };

    this.agents = [
      {
        alias: 'starlight',
        enabled: true,
        routable: true,
        credentials: 'ready',
        app_id_env: 'FEISHU_STARLIGHT_APP_ID',
        scope: 'thread',
        candidates: ['claude-opus-5-5[context=1m,effort=high,fast=false]', 'claude-opus-5', 'claude-fable-5', 'claude-sonnet-5'],
        efforts: ['low', 'medium', 'high', 'xhigh', 'max'],
        layer: {
          name: 'starlight',
          display: '星光',
          status: 'active',
          'feishu.app_id_env': 'FEISHU_STARLIGHT_APP_ID',
          'feishu.app_secret_env': 'FEISHU_STARLIGHT_APP_SECRET',
          'feishu.scope': 'thread',
          aliases: { [ZHOU]: '另一位' },
          'runtime.cli': 'claude',
          model: 'claude-opus-5-5[context=1m,effort=high,fast=false]',
          effort: 'xhigh',
          'runtime.working_dir': '~/.summon/agents/starlight/workspace',
          'runtime.chat_working_dirs': { [CHAT_DEV]: '~/workspace/app', [CHAT_RES]: '~/workspace/research' },
          'runtime.env': { HTTPS_PROXY: '${CORP_PROXY}', ANTHROPIC_BASE_URL: PLACEHOLDER },
          'feishu.allowed_users': [SELF_UNION, ZHOU],
          'feishu.operators': [SELF_OPEN, SELF_UNION],
          'messaging.cot_tool_details': true,
        },
      },
      {
        alias: 'arcadia',
        enabled: true,
        routable: true,
        credentials: 'ready',
        app_id_env: 'FEISHU_ARCADIA_APP_ID',
        scope: 'chat',
        candidates: ['gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-5.6-luna', 'gpt-5.5'],
        efforts: ['low', 'medium', 'high', 'xhigh'],
        layer: {
          name: 'arcadia',
          display: '阿卡迪亚',
          status: 'active',
          'feishu.app_id_env': 'FEISHU_ARCADIA_APP_ID',
          'feishu.app_secret_env': 'FEISHU_ARCADIA_APP_SECRET',
          'feishu.scope': 'chat',
          'runtime.cli': 'codex',
          model: 'gpt-5.6-sol',
          effort: 'high',
          'runtime.working_dir': '~/workspace/app-ci',
          'runtime.codex_sandbox': 'workspace-write',
          'runtime.codex_approval_policy': 'on-request',
          'feishu.allowed_users': [SELF_UNION, XU],
          'feishu.operators': [SELF_UNION],
          'messaging.cot_visible': false,
        },
      },
      {
        alias: 'nimbus',
        enabled: true,
        routable: true,
        credentials: 'ready',
        app_id_env: 'FEISHU_NIMBUS_APP_ID_V2',
        scope: 'thread',
        candidates: ['gpt-5.6-sol-xhigh', 'claude-opus-5-thinking-xhigh', 'cursor-grok-4.5-high', 'composer-2.5'],
        efforts: [],
        layer: {
          name: 'nimbus',
          display: '云朵',
          status: 'active',
          'feishu.app_id_env': 'FEISHU_NIMBUS_APP_ID_V2',
          'feishu.app_secret_env': 'FEISHU_NIMBUS_APP_SECRET',
          'feishu.scope': 'thread',
          'runtime.cli': 'cursor',
          'runtime.capture': 'rpc',
          model: 'gpt-5.6-sol-xhigh',
          'runtime.working_dir': '/Users/example/workspace/business/2610_interpretability_research/notes/weekly-sync-and-long-running-experiments',
          'feishu.allowed_users': [],
          'feishu.operators': [SELF_UNION, CHEN],
          'messaging.cot_visible': true,
          'messaging.idle_retire_secs': 21600,
        },
      },
      {
        alias: 'saturn',
        enabled: true,
        routable: false,
        credentials: 'missing',
        load_error: 'agent.yaml 第 12 行第 14 列：mapping values are not allowed in this context',
        app_id_env: 'FEISHU_SATURN_APP_ID',
        scope: 'thread',
        candidates: [],
        efforts: [],
        layer: { name: 'saturn', display: '' },
      },
      // 再来两个,让一个群里同时挂五六个 agent(标签行要能折行)
      {
        alias: 'vega',
        enabled: true,
        routable: true,
        credentials: 'ready',
        app_id_env: 'FEISHU_VEGA_APP_ID',
        scope: 'thread',
        candidates: ['claude-sonnet-5'],
        efforts: ['low', 'medium', 'high'],
        layer: { name: 'vega', display: '织女', status: 'active', 'runtime.cli': 'claude', model: 'claude-sonnet-5', 'feishu.scope': 'thread' },
      },
      {
        alias: 'polaris',
        enabled: false,
        routable: false,
        credentials: 'ready',
        app_id_env: 'FEISHU_POLARIS_APP_ID',
        scope: 'chat',
        candidates: ['gpt-5.5'],
        efforts: ['low', 'medium', 'high'],
        layer: { name: 'polaris', display: '北极星', status: 'disabled', 'runtime.cli': 'codex', model: 'gpt-5.5', 'feishu.scope': 'chat' },
      },
    ];
    this.raw.set('agent:saturn', SATURN_RAW);

    const ago = (mins: number) => localIso(new Date(this.t0 - mins * 60_000));
    this.chats = [
      {
        chat_id: CHAT_DEV,
        kind: 'group',
        name: '示例群',
        agents: {
          starlight: {
            overrides: { 'messaging.badge_effort': false, 'messaging.timer_idle_retire_reset': true },
            live: false,
            last_active: ago(4),
            operators: [ZHOU],
            mentionless: false,
          },
          arcadia: { overrides: { 'messaging.cot_visible': true }, live: false, last_active: ago(190), operators: [], mentionless: false },
        },
      },
      {
        chat_id: CHAT_OPS,
        kind: 'group',
        name: '发布群',
        agents: {
          arcadia: { overrides: {}, live: true, last_active: ago(26), operators: [XU], mentionless: false },
          starlight: { overrides: {}, live: false, last_active: ago(90), operators: [], mentionless: false },
          vega: { overrides: { 'messaging.cot_visible': false }, live: false, last_active: ago(300), operators: [], mentionless: false },
          polaris: { overrides: {}, live: false, last_active: ago(60 * 30), operators: [], mentionless: false },
          nimbus: { overrides: { 'messaging.agent_feed': false }, live: false, last_active: ago(60 * 20), operators: [], mentionless: false },
        },
      },
      {
        chat_id: CHAT_RES,
        kind: 'group',
        name: '周会群',
        agents: {
          starlight: { overrides: {}, live: false, last_active: ago(60 * 26), operators: [], override: '~/workspace/research/notes', mentionless: false },
        },
      },
      {
        chat_id: CHAT_P2P,
        kind: 'p2p',
        name: '示例用户',
        agents: {
          starlight: { overrides: { 'messaging.hide_thinking': false }, live: true, last_active: ago(1), operators: [], mentionless: false },
        },
      },
      {
        chat_id: CHAT_UNNAMED,
        kind: 'group',
        agents: {
          nimbus: { overrides: {}, live: false, last_active: ago(60 * 24 * 6), operators: [], mentionless: false },
        },
      },
    ];

    const s = (
      id: string,
      agent: string,
      chat_id: string,
      when: string,
      prompt: string,
      extra: Partial<MockSchedule> = {},
    ): MockSchedule => {
      const parsed = parseWhen(when, this.now);
      return {
        id,
        agent,
        chat_id,
        when,
        kind: parsed.ok ? parsed.kind : 'cron',
        spec: parsed.ok ? parsed.spec : when,
        human: parsed.ok ? parsed.human : when,
        next_fire: parsed.ok ? parsed.next[0] : undefined,
        prompt,
        output: 'progress',
        context: 'resume',
        enabled: true,
        created_by: SELF_OPEN,
        ...extra,
      } as MockSchedule;
    };
    this.schedules = [
      s('SCH-1', 'starlight', CHAT_DEV, 'weekday 09:30', '汇总昨晚 CI 失败的任务,按模块列出负责人和第一条报错,没有失败就只回一句「全部通过」。', {
        last_fired: ago(60 * 5),
      }),
      s('SCH-2', 'starlight', CHAT_DEV, 'every 2h', '检查 PR 队列里超过 24 小时没人 review 的,在群里提醒作者和建议的 reviewer。', {
        output: 'result',
        last_fired: ago(48),
      }),
      s('SCH-3', 'starlight', CHAT_P2P, 'daily 22:30', '整理今天的待办:完成的打勾,没完成的挪到明天并写一句原因。', {
        context: 'fresh',
        last_fired: ago(60 * 16),
        created_by: SELF_OPEN,
      }),
      s('SCH-4', 'starlight', CHAT_RES, 'weekly mon,fri 10:00', '[周会准备 · 每周一、五] 先读 /Users/example/workflow/CLAUDE.md、/Users/example/workflow/agents/reflector-agent.md、/Users/example/workflow/memory/MEMORY.md,再汇总本周新出的可解释性论文,每篇一句话说清方法与结论,附链接。', {
        last_fired: ago(60 * 24 * 4),
      }),
      s('SCH-5', 'arcadia', CHAT_OPS, 'cron 0 9 * * 1-5', '检查昨晚灰度批次的崩溃率和核心指标,有超过阈值的列出来并给出回滚建议。', {
        output: 'result',
        last_fired: ago(60 * 5.5),
        created_by: 'ou_00000000000000000000000000000002',
      }),
      s('SCH-6', 'arcadia', CHAT_OPS, 'weekday 18:00', '汇总今天所有发布单的状态,卡住的写明卡在哪一步。', { last_fired: ago(60 * 20) }),
      s('SCH-7', 'nimbus', CHAT_OPS, 'daily 08:45', '拉取前一天的崩溃率与 ANR 报表,和上周同期对比,变化超过 10% 的单独列出。', {
        last_fired: ago(60 * 6),
        created_by: CHEN,
      }),
      s('SCH-8', 'nimbus', CHAT_UNNAMED, 'every 6h', '看一眼这个群里新反馈的问题,归类后更新到问题清单。', { last_fired: ago(60 * 3) }),
      // 已归档:一次性的触发后自动归档,还有手动停用的
      s('SCH-9', 'starlight', CHAT_P2P, 'at 2026-10-12T15:00', '提醒我把评审材料发给另一位用户。', {
        enabled: false,
        next_fire: undefined,
        last_fired: ago(60 * 24 * 8),
        created_by: SELF_OPEN,
      }),
      s('SCH-10', 'starlight', CHAT_DEV, 'at 2026-10-16T10:00', '发版前再跑一遍完整的集成测试,把结果贴到群里。', {
        enabled: false,
        next_fire: undefined,
        last_fired: ago(60 * 24 * 4),
      }),
      s('SCH-11', 'arcadia', CHAT_DEV, 'every 30m', '盯一下 main 分支的构建状态,挂了就在群里说。', {
        enabled: false,
        next_fire: undefined,
        last_fired: ago(60 * 24 * 2),
      }),
      s('SCH-12', 'nimbus', CHAT_OPS, 'at 2026-10-18T20:00', '提醒值班同学确认周末的灰度开关。', {
        enabled: false,
        next_fire: undefined,
        last_fired: ago(60 * 24 * 2),
      }),
      s('SCH-13', 'starlight', CHAT_RES, 'at 2026-10-09T09:00', '把上周讨论的三个实验方向整理成一页纸。', {
        enabled: false,
        next_fire: undefined,
        last_fired: ago(60 * 24 * 11),
      }),
      s('SCH-14', 'nimbus', CHAT_UNNAMED, 'at 2026-10-14T14:00', '体验会开始前十分钟提醒大家进会议室。', {
        enabled: false,
        next_fire: undefined,
        last_fired: ago(60 * 24 * 6),
      }),
      s('SCH-15', 'starlight', CHAT_P2P, 'at 2026-10-17T19:30', '提醒我续费域名。', {
        enabled: false,
        next_fire: undefined,
        last_fired: ago(60 * 24 * 3),
        created_by: SELF_OPEN,
      }),
      s('SCH-16', 'arcadia', CHAT_OPS, 'at 2026-10-06T09:00', '国庆后第一天,检查假期期间的告警有没有遗留。', {
        enabled: false,
        next_fire: undefined,
        last_fired: ago(60 * 24 * 14),
      }),
    ];

    this.docs = new Map([
      [
        docPath({ level: 'global' }),
        {
          exists: true,
          text: '# 全局要求\n\n- 默认用简体中文回复;对方明确要英文时用英文。\n- 先给结论,再给依据;不确定的写明哪里没验证。\n- 改动前先说明影响范围,涉及删除或发布的操作先确认。\n',
        },
      ],
      [
        docPath({ level: 'agent', agent: 'starlight', file: 'CLAUDE.md' }),
        {
          exists: true,
          text: '# 星光\n\n负责 app 仓库的日常研发协助:排查 CI、审 PR、写小工具。\n\n- 动代码前先在 worktree 里开分支。\n- 跑测试优先用受影响模块的定向测试。\n',
        },
      ],
      [
        docPath({ level: 'agent', agent: 'starlight', file: 'SOUL.md' }),
        { exists: true, text: '冷静、直接,不说客套话。遇到判断不了的事,先把选项和代价摆出来。\n' },
      ],
      [
        docPath({ level: 'chat', agent: 'starlight', chat_id: CHAT_DEV }),
        { exists: true, text: '本群是 app 仓库的研发讨论群。\n\n- 回复里提到文件时写仓库内的相对路径。\n- CI 相关的结论附上任务链接。\n' },
      ],
      [docPath({ level: 'agent', agent: 'arcadia', file: 'CLAUDE.md' }), { exists: true, text: '# 阿卡迪亚\n\n负责发布值班与 CI 巡检。\n' }],
    ]);

    // 启动时的配置快照:云朵的飞书身份在 daemon 启动后被 summon bind 换过,重启才生效。
    this.boot = new Map([['agent:nimbus:feishu.app_id_env', 'FEISHU_NIMBUS_APP_ID']]);
    for (const f of MOCK_SCHEMA.fields) {
      if (f.effect === 'restart' && f.layers.includes('global')) this.boot.set(`global:${f.key}`, this.globalEffective(f.key));
    }
  }

  // ── 分层取值 ──

  field(key: string): FieldDesc | undefined {
    return MOCK_SCHEMA.fields.find((f) => f.key === key);
  }

  agent(alias: string): MockAgent {
    return this.agents.find((a) => a.alias === alias) ?? fail('not_found', `不存在名为 ${alias} 的 agent`);
  }

  chat(chatId: string): MockChat {
    return this.chats.find((c) => c.chat_id === chatId) ?? fail('not_found', '群聊不存在');
  }

  chatLabel(c: MockChat): string {
    return c.name ?? c.chat_id;
  }

  agentDisplay(alias: string): string {
    const a = this.agents.find((x) => x.alias === alias);
    return (a?.layer.display as string) || alias;
  }

  globalEffective(key: string): unknown {
    return key in this.global ? this.global[key] : (BUILTIN[key] ?? null);
  }

  /** 按层合并:内置 → 整机 → agent → 本群;草稿只叠在本页那一层上。 */
  resolve(scope: Scope, key: string, draft?: Map<string, unknown>): { effective: unknown; source: Layer; value: unknown } {
    const f = this.field(key);
    const layers: Array<[Layer, Record<string, unknown> | undefined]> = [];
    if (f?.layers.includes('global')) layers.push(['global', this.global]);
    if (scope.level !== 'global') {
      layers.push(['agent', this.agent(scope.agent).layer]);
      if (scope.level === 'chat') layers.push(['chat', this.chat(scope.chat_id).agents[scope.agent]?.overrides]);
    }
    const own: Layer = scope.level;
    let effective: unknown = BUILTIN[key] ?? null;
    let source: Layer = 'builtin';
    let value: unknown = null;
    for (const [layer, src] of layers) {
      let has = !!src && key in src;
      let v = has ? src![key] : undefined;
      if (layer === own && draft?.has(key)) {
        const d = draft.get(key);
        has = d !== null;
        v = d;
      }
      if (layer === own) value = has ? v : null;
      if (has) {
        effective = v;
        source = layer;
      }
    }
    return { effective: clone(effective), source, value: clone(value ?? null) };
  }

  fieldsFor(scope: Scope): FieldDesc[] {
    const layer = scope.level;
    return MOCK_SCHEMA.fields.filter((f) => f.layers.includes(layer));
  }

  overriddenBy(scope: Scope, key: string): string[] {
    const out: string[] = [];
    if (scope.level === 'global') {
      for (const a of this.agents) if (!a.load_error && key in a.layer) out.push(a.alias);
    }
    if (scope.level !== 'chat') {
      for (const c of this.chats) {
        for (const [alias, st] of Object.entries(c.agents)) {
          if (scope.level === 'agent' && alias !== scope.agent) continue;
          if (key in st.overrides) out.push(`${alias} · ${this.chatLabel(c)}`);
        }
      }
    }
    return out;
  }

  canEdit(scope: Scope): { ok: true } | { ok: false; reason: string } {
    if (scope.level !== 'global' && this.agent(scope.agent).load_error) {
      return { ok: false, reason: 'agent.yaml 读取失败，请先在 YAML 页签中修复' };
    }
    return { ok: true };
  }

  revision(key: string): string {
    const bump = this.bumps.get(key) ?? 0;
    let payload: unknown;
    if (key === 'global') payload = [this.global, this.raw.get('global')];
    else if (key.startsWith('agent:')) {
      const a = this.agents.find((x) => `agent:${x.alias}` === key);
      // feishu.chat_operators 写在 agent.yaml 里,所以各群的本群操作名单算进 agent 的版本。
      const ops = this.chats.map((c) => (a ? c.agents[a.alias]?.operators : undefined));
      payload = [a?.layer, a?.enabled, this.raw.get(key), ops];
    } else if (key.startsWith('chat:')) {
      const [, alias, chatId] = key.split(':');
      const st = this.chats.find((c) => c.chat_id === chatId)?.agents[alias!];
      payload = st ? [st.overrides, st.override, st.mentionless] : null;
    } else if (key === 'schedules') payload = this.schedules;
    else if (key.startsWith('doc:')) payload = this.docs.get(key.slice(4));
    return fnv(`${key}|${bump}|${JSON.stringify(payload)}`);
  }

  bump(key: string): void {
    this.bumps.set(key, (this.bumps.get(key) ?? 0) + 1);
  }

  scopeKey(scope: Scope): string {
    return scope.level === 'global' ? 'global' : scope.level === 'agent' ? `agent:${scope.agent}` : `chat:${scope.agent}:${scope.chat_id}`;
  }

  configView(scope: Scope): ConfigView {
    const editable = this.canEdit(scope);
    const values: FieldValue[] = this.fieldsFor(scope).map((f) => {
      const r = this.resolve(scope, f.key);
      const fv: FieldValue = {
        key: f.key,
        value: r.value,
        effective: r.effective,
        source: r.source,
        overridden_by: this.overriddenBy(scope, f.key),
        editable: editable.ok && f.write !== 'readonly',
      };
      if (!editable.ok) fv.deny_reason = editable.reason;
      else if (f.write === 'readonly') fv.deny_reason = '此项在控制台中仅供查看，如需修改请在本机操作（见说明）';
      return fv;
    });
    const view: ConfigView = { scope, revision: this.revision(this.scopeKey(scope)), values };
    if (scope.level !== 'chat') view.reactions = clone(REACTIONS);
    return view;
  }

  restartPending(): RestartItem[] {
    const out: RestartItem[] = [];
    for (const [k, v] of this.boot) {
      const [layer, a, b] = k.split(':');
      if (layer === 'global') {
        if (!eq(this.globalEffective(a!), v)) out.push({ key: a!, file: 'global' });
      } else if (layer === 'agent') {
        const ag = this.agent(a!);
        const cur = b === 'feishu.app_id_env' ? ag.app_id_env : ag.layer[b!];
        if (!eq(cur, v)) out.push({ key: b!, file: `agent:${a}` });
      }
    }
    return out;
  }

  summary(a: MockAgent): AgentSummary {
    const s: AgentSummary = {
      alias: a.alias,
      name: (a.layer.name as string) || a.alias,
      display: (a.layer.display as string) ?? '',
      cli: a.load_error ? '' : String(a.layer['runtime.cli'] ?? ''),
      enabled: a.enabled,
      routable: a.routable && a.enabled && !a.load_error,
      credentials: a.credentials,
      chats: this.chats.filter((c) => a.alias in c.agents).length,
    };
    if (a.layer.model) s.model = String(a.layer.model);
    if (a.layer.effort) s.effort = String(a.layer.effort);
    if (a.load_error) s.load_error = a.load_error;
    return s;
  }

  // ── 校验 ──

  validate(f: FieldDesc, value: unknown): void {
    if (value === null) return;
    const bad = (msg: string) => fail('invalid', msg, f.key);
    switch (f.kind) {
      case 'bool':
        if (typeof value !== 'boolean') bad('应为开或关');
        break;
      case 'int':
      case 'duration_secs': {
        if (typeof value !== 'number' || !Number.isInteger(value)) bad('应为整数');
        const n = value as number;
        if (f.kind === 'duration_secs' && n < 0) bad('时长不能是负数');
        if (f.min !== undefined && n < f.min) bad(`取值范围为 ${f.min}–${f.max ?? '∞'}，当前为 ${n}`);
        if (f.max !== undefined && n > f.max) bad(`取值范围为 ${f.min ?? 0}–${f.max}，当前为 ${n}`);
        break;
      }
      case 'enum':
        if (!f.options?.some((o) => o.value === value)) bad(`只能是 ${f.options?.map((o) => o.value).join(' / ')}`);
        break;
      case 'path':
        if (typeof value !== 'string') bad('应为路径');
        if (value !== '' && !/^(\/|~\/)/.test(value as string)) bad('应为绝对路径，可使用 ~/ 开头');
        break;
      case 'string':
        if (typeof value !== 'string') bad('应为文本');
        if (f.key.endsWith('public_base_url') && value !== '' && !/^https?:\/\/[^/\s]+/.test(value as string)) {
          bad('应为以 http:// 或 https:// 开头的地址，例如 http://10.0.0.8:7767');
        }
        if (f.key === 'model' && (value as string).trim() === '') bad('模型不能为空；如不指定，请恢复继承');
        break;
      case 'string_list':
        if (!Array.isArray(value) || value.some((x) => typeof x !== 'string')) bad('应为文本列表');
        break;
      case 'string_map':
        if (!value || typeof value !== 'object' || Array.isArray(value)) bad('应为键值表');
        break;
    }
  }

  // ── 预览渲染 ──

  previewSettings(scope: Scope, draft: ConfigChange[]): (key: string) => unknown {
    const d = new Map(draft.map((c) => [c.key, c.value]));
    return (key) => this.resolve(scope, key, d).effective;
  }

  renderPreview(scope: Scope, get: (key: string) => unknown, title: string): PreviewView {
    const alias = scope.level === 'global' ? 'starlight' : scope.agent;
    const a = this.agents.find((x) => x.alias === alias);
    const display = this.agentDisplay(alias);
    const chatName = scope.level === 'chat' ? this.chatLabel(this.chat(scope.chat_id)) : '示例群';
    const cli = String(a?.layer['runtime.cli'] ?? 'claude');
    const model = String(a?.layer.model ?? 'claude-opus-5');
    const effort = cli === 'cursor' ? '' : String(a?.layer.effort ?? 'high');

    const cotVisible = get('messaging.cot_visible') === true;
    const showThinking = get('messaging.hide_thinking') === true;
    const tools = get('messaging.cot_tool_details') === true;
    const badgeEffort = get('messaging.badge_effort') === true;
    const feed = get('messaging.agent_feed') === true;
    const reactions = get('messaging.agent_reactions') === true;

    const items: CotItem[] = [];
    if (cotVisible) {
      if (showThinking) {
        items.push({
          id: 'cot_t1',
          type: 'thinking',
          text: '先看 CI 日志里第一个失败的任务。日志显示 app 的单测在 parse_config 附近失败,需要确认是代码问题还是环境问题。',
        });
      }
      if (tools) {
        items.push({ id: 'cot_tool1', type: 'tool', icon: 'bash', name: 'bash', args: 'cargo test -p app', result: 'test result: FAILED. 41 passed; 1 failed\n    parse_config_rejects_empty' });
      }
      if (showThinking) {
        items.push({
          id: 'cot_t2',
          type: 'thinking',
          text: '失败的是空字符串的边界用例:新加的分支直接返回了默认配置。改成返回 ConfigError::Empty 后再跑一遍。',
        });
      }
      if (tools) {
        items.push({ id: 'cot_tool2', type: 'tool', icon: 'read', name: 'read', args: 'src/lib.rs' });
        items.push({ id: 'cot_tool3', type: 'tool', icon: 'bash', name: 'bash', args: 'cargo test -p app', result: 'test result: ok. 42 passed; 0 failed' });
      }
    }

    const body =
      'CI 挂在 `app` 的单测 **parse_config_rejects_empty**:新加的空字符串分支没有返回错误。\n\n' +
      '已改成返回 `ConfigError::Empty`,本地 `cargo test -p app` 全部通过。排查记录在 `/tmp/report.md`。';
    const plainHead = 'CI 挂在 app 的单测 parse_config_rejects_empty:新加的空字符串分支没有返回错误。 已改成返回 ConfigError::Empty,本地 cargo test -p app 全部通过。排查记录在 /tmp/report.md。';
    const feedText = feed ? 'CI 已修复' : plainHead;
    const badge = [cli, model, badgeEffort && effort ? effort : '', '上下文 38%', '14:32'].filter(Boolean).join(' · ');
    const card = {
      schema: '2.0',
      config: { summary: { content: feedText } },
      body: {
        elements: [
          { tag: 'markdown', element_id: 'reply_body', content: body },
          { tag: 'hr', element_id: 'reply_rule' },
          { tag: 'markdown', element_id: 'badge', content: `<font color='grey'>${badge}</font>`, text_size: 'notation' },
          {
            tag: 'column_set',
            element_id: 'preview_files',
            flex_mode: 'flow',
            horizontal_spacing: '8px',
            columns: [
              {
                tag: 'column',
                width: 'auto',
                elements: [
                  {
                    tag: 'button',
                    text: { tag: 'plain_text', content: '📄 report.md' },
                    type: 'default',
                    behaviors: [{ type: 'open_url', default_url: 'http://127.0.0.1:7765/p/0000' }, { type: 'callback', value: { action: 'preview_open' } }],
                  },
                ],
              },
            ],
          },
        ],
      },
    };

    const messages: PreviewMessage[] = [
      {
        id: 'user',
        kind: 'user',
        sender: '示例用户',
        text: scope.level === 'chat' && this.chat(scope.chat_id).kind === 'p2p' ? '帮我看下 CI 为什么挂了' : `@${display} 帮我看下 CI 为什么挂了`,
        reactions: reactions ? ['OK', String(get('messaging.done_reaction') ?? 'DONE')] : [String(get('messaging.done_reaction') ?? 'DONE')],
      },
      { id: 'cot', kind: 'cot', title: cotVisible ? '帮我看下 CI 为什么挂了' : '处理请求', status: 'done', items },
      { id: 'reply', kind: 'card', card },
      { id: 'feed', kind: 'feed', chat_name: chatName, text: `${display}:${feedText}` },
    ];
    return { title, messages, regions: {} };
  }

  preview(scope: Scope, draft: ConfigChange[]): PreviewView {
    const title =
      scope.level === 'global'
        ? '按整机设置'
        : scope.level === 'agent'
          ? `按 ${this.agentDisplay(scope.agent)} 的设置`
          : `按 ${this.agentDisplay(scope.agent)} · ${this.chatLabel(this.chat(scope.chat_id))} 的设置`;
    const get = this.previewSettings(scope, draft);
    const view = this.renderPreview(scope, get, title);
    for (const f of MOCK_SCHEMA.fields) {
      if (!f.preview || !this.fieldsFor(scope).includes(f)) continue;
      const cur = get(f.key);
      const contrast = f.kind === 'bool' ? !cur : cur;
      const alt = this.renderPreview(scope, (k) => (k === f.key ? contrast : get(k)), title);
      const ids = regionDiff(view, alt);
      if (ids.length) view.regions[f.key] = ids;
    }
    return view;
  }
}

/** 核验任务号:与服务端同形的 32 位十六进制。 */
function jobId(n: number): string {
  return fnv(`job-${n}`) + fnv(`job-${n}-b`);
}

// ── 节律解析(仿 parse_when 的合法形态) ──────────────────────────────────

export const RHYTHM_FORMS =
  '合法形态:`in 30m` | `at 2026-08-22T09:00` | `every 2h` | `daily 09:00` | `weekday 09:30` | `weekly mon,fri 10:00` | `cron 0 9 * * 1-5`(时长单位 s/m/h/d;星期几认 mon..sun / 周一..周日 / 0..7)';

const DOW: Record<string, number> = { sun: 0, mon: 1, tue: 2, wed: 3, thu: 4, fri: 5, sat: 6, '周日': 0, '周一': 1, '周二': 2, '周三': 3, '周四': 4, '周五': 5, '周六': 6 };
const DOW_CN = ['日', '一', '二', '三', '四', '五', '六'];

type Parsed = { ok: true; kind: string; spec: string; human: string; next: string[] } | { ok: false; error: string };

function durSecs(s: string): number | null {
  const m = /^(\d+)([smhd])$/.exec(s);
  if (!m) return null;
  return Number(m[1]) * { s: 1, m: 60, h: 3600, d: 86400 }[m[2] as 's' | 'm' | 'h' | 'd'];
}

function cronNext(minute: number, hour: number, dows: number[] | null, now: Date, n: number): string[] {
  const out: string[] = [];
  const d = new Date(now.getFullYear(), now.getMonth(), now.getDate(), hour, minute, 0);
  for (let i = 0; out.length < n && i < 400; i++) {
    if (d.getTime() > now.getTime() && (!dows || dows.includes(d.getDay()))) out.push(localIso(d));
    d.setDate(d.getDate() + 1);
  }
  return out;
}

function parseDows(s: string): number[] | null {
  const parts = s.split(',');
  const out: number[] = [];
  for (const p of parts) {
    const range = /^(\d)-(\d)$/.exec(p);
    if (range) {
      for (let i = Number(range[1]); i <= Number(range[2]); i++) out.push(i % 7);
      continue;
    }
    const key = p.toLowerCase().slice(0, p.startsWith('周') ? 2 : 3);
    if (key in DOW) out.push(DOW[key]!);
    else if (/^[0-7]$/.test(p)) out.push(Number(p) % 7);
    else return null;
  }
  return out;
}

function parseHm(s: string | undefined): [number, number] | null {
  const m = /^(\d{1,2}):(\d{2})$/.exec(s ?? '');
  if (!m) return null;
  const h = Number(m[1]);
  const mi = Number(m[2]);
  return h < 24 && mi < 60 ? [h, mi] : null;
}

export function parseWhen(when: string, now: Date): Parsed {
  const t = when.trim().split(/\s+/);
  const head = t[0] ?? '';
  const hmText = (h: number, m: number) => `${pad(h)}:${pad(m)}`;
  if (!head) return { ok: false, error: `未填写触发规则。${RHYTHM_FORMS}` };
  if (head === 'in') {
    const secs = durSecs(t[1] ?? '');
    if (!secs) return { ok: false, error: 'in 后需要时长,例如 30m' };
    const at = new Date(now.getTime() + secs * 1000);
    return { ok: true, kind: 'once', spec: localIso(at), human: `一次 · ${pad(at.getMonth() + 1)}-${pad(at.getDate())} ${hmText(at.getHours(), at.getMinutes())}`, next: [localIso(at)] };
  }
  if (head === 'at') {
    const at = new Date(t[1] ?? '');
    if (Number.isNaN(at.getTime())) return { ok: false, error: 'at 后需要时刻,例如 2026-08-22T09:00' };
    return {
      ok: true,
      kind: 'once',
      spec: localIso(at),
      human: `一次 · ${pad(at.getMonth() + 1)}-${pad(at.getDate())} ${hmText(at.getHours(), at.getMinutes())}`,
      next: at.getTime() > now.getTime() ? [localIso(at)] : [],
    };
  }
  if (head === 'every') {
    const secs = durSecs(t[1] ?? '');
    if (!secs) return { ok: false, error: 'every 后需要时长,例如 2h' };
    const unit = secs % 86400 === 0 ? [secs / 86400, '天'] : secs % 3600 === 0 ? [secs / 3600, '小时'] : secs % 60 === 0 ? [secs / 60, '分钟'] : [secs, '秒'];
    const next = [1, 2, 3].map((i) => localIso(new Date(now.getTime() + i * secs * 1000)));
    return { ok: true, kind: 'interval', spec: String(secs), human: `每隔 ${unit[0]} ${unit[1]}`, next };
  }
  if (head === 'daily' || head === 'weekday') {
    const hm = parseHm(t[1]);
    if (!hm) return { ok: false, error: `${head} 后需要时刻,例如 09:30` };
    const dows = head === 'weekday' ? [1, 2, 3, 4, 5] : null;
    return {
      ok: true,
      kind: 'cron',
      spec: `${hm[1]} ${hm[0]} * * ${head === 'weekday' ? '1-5' : '*'}`,
      human: `${head === 'weekday' ? '每个工作日' : '每天'} ${hmText(hm[0], hm[1])}`,
      next: cronNext(hm[1], hm[0], dows, now, 3),
    };
  }
  if (head === 'weekly') {
    const dows = parseDows(t[1] ?? '');
    const hm = parseHm(t[2]);
    if (!dows || !hm) return { ok: false, error: 'weekly 后需要星期几与时刻,例如 weekly mon,fri 10:00' };
    return {
      ok: true,
      kind: 'cron',
      spec: `${hm[1]} ${hm[0]} * * ${dows.join(',')}`,
      human: `每周${dows.map((d) => DOW_CN[d]).join('、')} ${hmText(hm[0], hm[1])}`,
      next: cronNext(hm[1], hm[0], dows, now, 3),
    };
  }
  if (head === 'cron') {
    const f = t.slice(1);
    if (f.length !== 5) return { ok: false, error: `cron 需要 5 段(分 时 日 月 周),现在是 ${f.length} 段` };
    const [mi, h, dom, mon, dow] = f as [string, string, string, string, string];
    if (!/^\d+$/.test(mi) || !/^\d+$/.test(h) || dom !== '*' || mon !== '*') {
      return { ok: false, error: '示例环境只演示「固定分时 + 星期」的 cron,例如 cron 0 9 * * 1-5' };
    }
    const dows = dow === '*' ? null : parseDows(dow);
    if (dow !== '*' && !dows) return { ok: false, error: `看不懂星期段 ${dow}` };
    const hmTxt = hmText(Number(h), Number(mi));
    const human = dows ? (eq(dows, [1, 2, 3, 4, 5]) ? `每个工作日 ${hmTxt}` : `每周${dows.map((d) => DOW_CN[d]).join('、')} ${hmTxt}`) : `每天 ${hmTxt}`;
    return { ok: true, kind: 'cron', spec: f.join(' '), human: `cron ${f.join(' ')} · ${human}`, next: cronNext(Number(mi), Number(h), dows, now, 3) };
  }
  return { ok: false, error: `无法识别的触发规则 \`${head}\`。${RHYTHM_FORMS}` };
}

// ── 说明文件路径 ────────────────────────────────────────────────────────

function docPath(doc: DocRef): string {
  switch (doc.level) {
    case 'global':
      return '~/.summon/CLAUDE.md';
    case 'agent':
      return `~/.summon/agents/${doc.agent}/${doc.file}`;
    case 'chat':
      return `~/.summon/agents/${doc.agent}/chats/${doc.chat_id}/CLAUDE.md`;
  }
}

// ── YAML 原文(示例环境由各层取值生成;原文保存后只保留原文,不回填表单) ─────

function yamlScalar(v: unknown): string {
  if (typeof v === 'string') return v === '' || /[:#{}[\],&*!|>'"%@`]|^\s|\s$|^(true|false|null|\d+)$/.test(v) ? JSON.stringify(v) : v;
  return JSON.stringify(v);
}

function toYaml(flat: Record<string, unknown>, header: string[]): string {
  const tree: Record<string, unknown> = {};
  for (const [key, v] of Object.entries(flat)) {
    if (v === null || v === undefined) continue;
    const parts = key.split('.');
    let cur = tree;
    for (const p of parts.slice(0, -1)) cur = (cur[p] ??= {}) as Record<string, unknown>;
    cur[parts[parts.length - 1]!] = v;
  }
  const lines = [...header];
  const emit = (node: Record<string, unknown>, depth: number) => {
    const ind = '  '.repeat(depth);
    for (const [k, v] of Object.entries(node)) {
      if (Array.isArray(v)) {
        if (!v.length) lines.push(`${ind}${k}: []`);
        else {
          lines.push(`${ind}${k}:`);
          for (const x of v) lines.push(`${ind}  - ${yamlScalar(x)}${NAMES[x as string] ? `   # ${NAMES[x as string]}` : ''}`);
        }
      } else if (v && typeof v === 'object') {
        if (!Object.keys(v).length) lines.push(`${ind}${k}: {}`);
        else {
          lines.push(`${ind}${k}:`);
          emit(v as Record<string, unknown>, depth + 1);
        }
      } else lines.push(`${ind}${k}: ${yamlScalar(v)}`);
    }
  };
  emit(tree, 0);
  return `${lines.join('\n')}\n`;
}

function unifiedDiff(a: string, b: string, name: string): string {
  const x = a.split('\n');
  const y = b.split('\n');
  const n = x.length;
  const m = y.length;
  const lcs: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) lcs[i]![j] = x[i] === y[j] ? lcs[i + 1]![j + 1]! + 1 : Math.max(lcs[i + 1]![j]!, lcs[i]![j + 1]!);
  const ops: Array<[' ' | '-' | '+', string]> = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (x[i] === y[j]) {
      ops.push([' ', x[i]!]);
      i++;
      j++;
    } else if (lcs[i + 1]![j]! >= lcs[i]![j + 1]!) ops.push(['-', x[i++]!]);
    else ops.push(['+', y[j++]!]);
  }
  while (i < n) ops.push(['-', x[i++]!]);
  while (j < m) ops.push(['+', y[j++]!]);
  const out = [`--- ${name}`, `+++ ${name}(修改后)`];
  const ctx = 2;
  const changed = ops.map((o) => o[0] !== ' ');
  let k = 0;
  while (k < ops.length) {
    if (!changed[k]) {
      k++;
      continue;
    }
    let start = Math.max(0, k - ctx);
    let end = k;
    while (end < ops.length && (changed[end] || changed.slice(end, end + ctx * 2 + 1).some(Boolean))) end++;
    end = Math.min(ops.length, end + ctx);
    let la = 1;
    let lb = 1;
    for (let q = 0; q < start; q++) {
      if (ops[q]![0] !== '+') la++;
      if (ops[q]![0] !== '-') lb++;
    }
    const seg = ops.slice(start, end);
    out.push(`@@ -${la},${seg.filter((o) => o[0] !== '+').length} +${lb},${seg.filter((o) => o[0] !== '-').length} @@`);
    for (const [op, line] of seg) out.push(`${op}${line}`);
    k = end;
    start = end;
  }
  return out.join('\n');
}

// ── 方法实现 ────────────────────────────────────────────────────────────

type Handler = (srv: MockServer, p: never) => unknown;

const H: Record<string, Handler> = {
  'session.whoami': (srv) => ({
    identity: srv.identity,
    csrf: 'mock-csrf-0f3a9c',
    expires_at: localIso(new Date(srv.now.getTime() + 11.5 * 3600_000)),
    hostname: 'example-host',
    version: '0.6.18',
    machine: srv.canEdit({ level: 'global' }).ok,
  }),

  'status.get': (srv): StatusView => {
    const revisions: Record<string, string> = { global: srv.revision('global'), schedules: srv.revision('schedules') };
    for (const a of srv.agents) revisions[`agent:${a.alias}`] = srv.revision(`agent:${a.alias}`);
    for (const c of srv.chats) for (const alias of Object.keys(c.agents)) revisions[`chat:${alias}:${c.chat_id}`] = srv.revision(`chat:${alias}:${c.chat_id}`);
    for (const [path, d] of srv.docs) if (d.exists) revisions[`doc:${path}`] = srv.revision(`doc:${path}`);
    return {
      daemon: {
        version: '0.6.18',
        commit: '20886b2',
        started_at: localIso(new Date(srv.now.getTime() - (3 * 86400 + 4 * 3600 + 12 * 60) * 1000)),
        pid: 48213,
        autostart: 'on',
      },
      hostname: 'example-host',
      language: srv.global.language === 'en' ? 'en' : 'zh-CN',
      restart_pending: srv.restartPending(),
      agents: srv.agents.map((a) => srv.summary(a)),
      revisions,
    };
  },

  'status.issues': (srv): Issue[] => {
    const out: Issue[] = [];
    const failed = mockTurns(srv).filter((t) => t.end === 'failed');
    const latest = failed[0];
    if (latest) {
      const s = mockSessions(srv).find((r) => r.agent === latest.agent && r.chat_id === latest.chat_id);
      const at = new Date(latest.ended_at_ms);
      const hm = `${String(at.getHours()).padStart(2, '0')}:${String(at.getMinutes()).padStart(2, '0')}`;
      out.push({
        severity: 'warn',
        title: `最近 24 小时有 ${failed.length} 个轮次失败`,
        detail: `涉及 1 个会话；最近一次为 ${s?.agent_display ?? latest.agent} 在「${s?.name ?? latest.chat_id}」中，${at.getMonth() + 1}-${at.getDate()} ${hm}，失败类别 \`${latest.error ?? 'capture'}\``,
        fix: '请打开会话详情查看该轮次的结果与失败类别；同一会话反复失败时，请查看 daemon 日志中对应时段的记录',
        link: { page: 'session', agent: latest.agent, chat_id: latest.chat_id },
      });
    }
    const broken = srv.agents.find((a) => a.alias === 'saturn');
    if (broken?.load_error) {
      out.push({
        severity: 'error',
        title: 'saturn 的 agent.yaml 读取失败',
        detail: `${broken.load_error}。该 agent 当前无法上线，其他 agent 不受影响。`,
        fix: '请在 YAML 模式中将第 12 行的列表项移到下一行，保存后将自动上线。',
        link: { page: 'agent', agent: 'saturn' },
      });
    }
    out.push({
      severity: 'warn',
      title: 'config.yaml 存在需要注意的写法',
      detail: 'config.yaml 第 72 行 `quota_recovery`：`quota_recovery` 已废弃：CLI 额度自动恢复已下线，该配置段不再生效，可以删除',
      fix: '请在 YAML 模式中删除 `quota_recovery` 配置段；并对照 `runtime.codex_approval_policy` 等合法键名检查拼写。',
      link: { page: 'global' },
    });
    if (!srv.bumps.has('fixed-typo')) {
      out.push({
        severity: 'warn',
        title: '未知键 messaging.cot_visable',
        detail: '~/.summon/config.yaml 第 5 行。该键不会被读取，不会产生任何效果。',
        fix: '请在 YAML 模式中改为最接近的合法键 messaging.cot_visible，或删除此行。',
        link: { page: 'global', key: 'messaging.cot_visible' },
      });
    }
    const bind = srv.globalEffective('preview_server.bind');
    const pub = srv.globalEffective('preview_server.public_base_url');
    if (bind === '127.0.0.1' && typeof pub === 'string' && pub && !pub.includes('127.0.0.1')) {
      out.push({
        severity: 'warn',
        title: '文件预览链接无法打开',
        detail: `监听地址为 127.0.0.1，对外地址却为 ${pub}，链接指向的地址未在监听`,
        fix: '将监听地址（preview_server.bind）改为 0.0.0.0，并重启 daemon',
        link: { page: 'global', key: 'preview_server.bind' },
      });
    }
    return out;
  },

  'config.schema': () => clone(MOCK_SCHEMA),

  'config.get': (srv, p: { scope: Scope }) => {
    const a = p.scope.level !== 'global' ? srv.agent(p.scope.agent) : null;
    if (a?.load_error) fail('unavailable', `agent.yaml 读取失败：${a.load_error}`);
    if (p.scope.level === 'chat') srv.chat(p.scope.chat_id);
    return srv.configView(p.scope);
  },

  'config.patch': (srv, p: { scope: Scope; revision: string; changes: ConfigChange[] }): SaveResult => {
    const key = srv.scopeKey(p.scope);
    const editable = srv.canEdit(p.scope);
    if (!editable.ok) fail('forbidden', editable.reason);
    if (p.revision !== srv.revision(key)) fail('conflict', '此文件已在其他位置被修改，请查看差异后重新应用');
    const allowed = srv.fieldsFor(p.scope);
    for (const c of p.changes) {
      const f = allowed.find((x) => x.key === c.key) ?? fail('invalid', `此层级不能设置 ${c.key}`, c.key);
      if (f.write === 'readonly') fail('forbidden', `${f.label} 只能在本机修改`, c.key);
      srv.validate(f, c.value);
    }
    const target =
      p.scope.level === 'global'
        ? srv.global
        : p.scope.level === 'agent'
          ? srv.agent(p.scope.agent).layer
          : (srv.chat(p.scope.chat_id).agents[p.scope.agent]!.overrides as Record<string, unknown>);
    for (const c of p.changes) {
      if (c.value === null) delete target[c.key];
      else {
        let v = c.value;
        // 敏感映射里原样带回的占位符还原成原值
        if (c.key === 'runtime.env' && v && typeof v === 'object') {
          const old = (target[c.key] ?? {}) as Record<string, string>;
          v = Object.fromEntries(Object.entries(v as Record<string, string>).map(([k, x]) => [k, x === PLACEHOLDER ? (old[k] ?? x) : x]));
        }
        target[c.key] = clone(v);
      }
    }
    srv.raw.delete(key === 'global' ? 'global' : key);
    return {
      revision: srv.revision(key),
      effects: p.changes.map((c) => ({ key: c.key, effect: (srv.field(c.key)?.effect ?? 'next_turn') as Effect })),
      restart_pending: srv.restartPending(),
    };
  },

  'config.raw.get': (srv, p: RawFile) => {
    const key = p.file === 'global' ? 'global' : `agent:${p.agent}`;
    const text = srv.raw.get(key) ?? (p.file === 'global' ? globalYaml(srv) : agentYaml(srv, p.agent));
    return {
      revision: srv.revision(key),
      text,
      path: p.file === 'global' ? '~/.summon/config.yaml' : `~/.summon/agents/${p.agent}/agent.yaml`,
      editable: true,
    };
  },

  'config.raw.check': (srv, p: RawFile & { text: string }): RawCheck => rawCheck(srv, p),

  'config.raw.put': (srv, p: RawFile & { revision: string; text: string }): SaveResult => {
    const key = p.file === 'global' ? 'global' : `agent:${p.agent}`;
    if (p.revision !== srv.revision(key)) fail('conflict', '此文件已在其他位置被修改，请查看差异后重新应用');
    const check = rawCheck(srv, p);
    if (check.errors.length) fail('invalid', check.errors[0]!.message);
    srv.raw.set(key, p.text);
    if (p.file === 'agent' && p.agent === 'saturn') {
      const a = srv.agent('saturn');
      delete a.load_error;
      a.routable = true;
      a.credentials = 'ready';
      a.layer = { name: 'saturn', display: 'Saturn', 'runtime.cli': 'claude', model: 'claude-sonnet-5', 'feishu.allowed_users': [SELF_UNION], 'feishu.operators': [] };
    }
    if (p.file === 'global' && !p.text.includes('cot_visable')) srv.bumps.set('fixed-typo', 1);
    return { revision: srv.revision(key), effects: [], restart_pending: srv.restartPending() };
  },

  'agents.list': (srv) => srv.agents.map((a) => srv.summary(a)),

  'agents.get': (srv, p: { agent: string }): AgentDetail => {
    const a = srv.agent(p.agent);
    const ids = new Set<string>([...((a.layer['feishu.allowed_users'] as string[]) ?? []), ...((a.layer['feishu.operators'] as string[]) ?? [])]);
    for (const c of srv.chats) for (const id of c.agents[a.alias]?.operators ?? []) ids.add(id);
    const members: Member[] = [...ids].map((id) => (NAMES[id] ? { id, name: NAMES[id] } : { id }));
    return {
      summary: srv.summary(a),
      app_id_env: a.app_id_env,
      scope: a.scope,
      model_candidates: a.candidates,
      effort_options: a.efforts,
      members,
      can_toggle_enabled: !a.load_error,
      ...(a.load_error ? { deny_reason: '该 agent 的配置读取失败，请先修复 agent.yaml 再启用或停用' } : {}),
    };
  },

  'agents.cleanup.prepare': () => fail('unavailable', '示例环境不生成含凭据的备份，请在真实控制台使用'),
  'agents.cleanup.status': () => fail('not_found', '示例环境没有备份任务'),
  'agents.cleanup.download': () => fail('not_found', '示例环境没有备份任务'),
  'agents.cleanup.confirm': () => fail('unavailable', '示例环境不执行 agent 清理'),
  'agents.set_enabled': (srv, p: { agent: string; enabled: boolean }) => {
    const a = srv.agent(p.agent);
    if (a.load_error) fail('forbidden', '该 agent 的配置读取失败，请先修复 agent.yaml 再启用或停用');
    a.enabled = p.enabled;
    srv.bump(`agent:${a.alias}`);
    return srv.summary(a);
  },

  'chats.list': (srv): ChatSummary[] =>
    srv.chats.map((c) => {
      const agents = Object.entries(c.agents).map(([agent, st]) => {
        const o: ChatSummary['agents'][number] = { agent, overrides: Object.keys(st.overrides).length + st.operators.length + (st.override ? 1 : 0), live: st.live };
        if (st.last_active) o.last_active = st.last_active;
        return o;
      });
      const last = agents.map((a) => a.last_active).filter(Boolean).sort().pop();
      const s: ChatSummary = { chat_id: c.chat_id, kind: c.kind, agents };
      if (c.name) s.name = c.name;
      if (last) s.last_active = last;
      return s;
    }),

  'chats.get': (srv, p: { chat_id: string }): ChatDetail => {
    const c = srv.chat(p.chat_id);
    const d: ChatDetail = { chat_id: c.chat_id, kind: c.kind, agents: Object.keys(c.agents).map((alias) => chatAgentView(srv, c, alias)) };
    if (c.name) d.name = c.name;
    return d;
  },

  'chats.set_mentionless': (srv, p: { agent: string; chat_id: string; enabled: boolean }): MentionlessJob => {
    const c = srv.chat(p.chat_id);
    const st = c.agents[p.agent] ?? fail('not_found', '该 agent 不在此群聊中');
    const view = chatAgentView(srv, c, p.agent);
    if (!p.enabled) {
      st.mentionless = false;
      srv.bump(`chat:${p.agent}:${p.chat_id}`);
      return { job: jobId(++srv.jobSeq), status: 'done', message: '已关闭本群免 @ 回复，群聊中需 @ 后才会回应' };
    }
    if (!view.mentionless.can_enable) fail('forbidden', view.mentionless.reason ?? '当前无法开启');
    const job = jobId(++srv.jobSeq);
    const bots = Object.keys(c.agents);
    const outcome: MentionlessJob =
      bots.length > 1
        ? {
            job,
            status: 'failed',
            message: `未开启：需要群内恰好只有你和当前 Bot（群内有 ${bots.length} 个 Bot：${bots.map((b) => srv.agentDisplay(b)).join('、')}）。配置保持不变；这一条件无法通过申请权限满足。`,
          }
        : p.chat_id === CHAT_RES
        ? {
            job,
            status: 'failed',
            message: '当前应用缺少权限：im:chat.members:read。配置保持不变。',
            apply_url: 'https://open.feishu.cn/app/cli_0000000000000001/auth?q=im:chat.members:read',
          }
        : { job, status: 'done', message: '已开启本群免 @ 对话：普通消息、附件和斜杠命令都可以不 @ 当前 Bot。只对当前 agent 和本群生效；每条消息仍会检查群里是否只有你和当前 Bot，不满足时请显式 @。' };
    srv.jobs.set(job, { agent: p.agent, chat_id: p.chat_id, at: Date.now(), outcome });
    return { job, status: 'pending', message: '核验中：正在检查应用权限、群成员与你的操作权限' };
  },

  'chats.mentionless_job': (srv, p: { job: string }): MentionlessJob => {
    const j = srv.jobs.get(p.job) ?? fail('not_found', '核验任务已过期');
    if (Date.now() - j.at < 1600) return { job: p.job, status: 'pending', message: '核验中：正在检查应用权限、群成员与你的操作权限' };
    if (j.outcome.status === 'done') {
      const st = srv.chat(j.chat_id).agents[j.agent];
      if (st && !st.mentionless) {
        st.mentionless = true;
        srv.bump(`chat:${j.agent}:${j.chat_id}`);
      }
    }
    return j.outcome;
  },

  'chats.set_operators': (srv, p: { agent: string; chat_id: string; revision: string; members: string[] }): SaveResult => {
    const c = srv.chat(p.chat_id);
    const st = c.agents[p.agent] ?? fail('not_found', '该 agent 不在此群聊中');
    const key = `agent:${p.agent}`;
    if (p.revision !== srv.revision(key)) fail('conflict', '此文件已在其他位置被修改，请查看差异后重新应用');
    const bad = p.members.find((m) => !/^(ou|on)_[0-9a-z]{6,}$/.test(m));
    if (bad) fail('invalid', `${bad} 不是飞书用户 ID：请填写 union_id（on_ 开头）或 open_id（ou_ 开头）`, 'feishu.chat_operators');
    st.operators = [...new Set(p.members)];
    return { revision: srv.revision(key), effects: [{ key: 'feishu.chat_operators', effect: 'immediate' }], restart_pending: srv.restartPending() };
  },

  'chats.set_work_dir': (
    srv,
    p: { agent: string; chat_id: string; revision: string; mapping: string | null; clear_override: boolean },
  ): ChatAgentView => {
    const c = srv.chat(p.chat_id);
    const st = c.agents[p.agent] ?? fail('not_found', '该 agent 不在此群聊中');
    // 按群映射写在 agent.yaml 里,版本校验跟本群操作名单是同一份
    if (p.revision !== srv.revision(`agent:${p.agent}`)) fail('conflict', '此文件已在其他位置被修改，请查看差异后重新应用');
    if (st.live) fail('busy', '该群聊的会话正在运行。工作目录仅在下次启动会话时生效，请先在群聊中退出会话再修改。');
    if (p.mapping !== null && p.mapping !== '' && !/^(\/|~\/)/.test(p.mapping)) fail('invalid', '应为绝对路径，可使用 ~/ 开头', 'runtime.chat_working_dirs');
    const a = srv.agent(p.agent);
    const map = { ...((a.layer['runtime.chat_working_dirs'] as Record<string, string>) ?? {}) };
    if (p.mapping === null || p.mapping === '') delete map[p.chat_id];
    else map[p.chat_id] = p.mapping;
    a.layer['runtime.chat_working_dirs'] = map;
    if (p.clear_override) delete st.override;
    srv.bump(`chat:${p.agent}:${p.chat_id}`);
    return chatAgentView(srv, c, p.agent);
  },

  'schedules.list': (srv) => srv.schedules.map((s) => scheduleView(srv, s)),

  'schedules.check_when': (_srv, p: { when: string }): WhenCheck => {
    const r = parseWhen(p.when, new Date());
    return r.ok ? { ok: true, human: r.human, next: r.next } : { ok: false, next: [], error: r.error, forms: RHYTHM_FORMS };
  },

  'schedules.edit': (srv, p: { id: string; edit: { when?: string; prompt?: string; output?: string; context?: string } }) => {
    const s = srv.schedules.find((x) => x.id === p.id) ?? fail('not_found', '定时任务不存在');
    if (p.edit.when !== undefined) {
      const r = parseWhen(p.edit.when, new Date());
      if (!r.ok) fail('invalid', r.error, 'when');
      else Object.assign(s, { when: p.edit.when, kind: r.kind, spec: r.spec, human: r.human, next_fire: s.enabled ? r.next[0] : undefined });
    }
    if (p.edit.prompt !== undefined) {
      if (!p.edit.prompt.trim()) fail('invalid', '提示词不能为空', 'prompt');
      s.prompt = p.edit.prompt;
    }
    if (p.edit.output !== undefined) s.output = p.edit.output;
    if (p.edit.context !== undefined) s.context = p.edit.context;
    srv.bump('schedules');
    return scheduleView(srv, s);
  },

  'schedules.set_enabled': (srv, p: { id: string; enabled: boolean }) => {
    const s = srv.schedules.find((x) => x.id === p.id) ?? fail('not_found', '定时任务不存在');
    s.enabled = p.enabled;
    if (p.enabled) {
      const r = parseWhen(s.when, new Date());
      s.next_fire = r.ok ? r.next[0] : undefined;
      if (s.kind === 'once' && !s.next_fire) fail('invalid', '该单次任务的触发时间已过，请先修改触发规则再重新启用', 'when');
    } else s.next_fire = undefined;
    srv.bump('schedules');
    return scheduleView(srv, s);
  },

  'docs.get': (srv, p: { doc: DocRef }) => docView(srv, p.doc),

  'docs.put': (srv, p: { doc: DocRef; revision: string | null; text: string }) => {
    const path = docPath(p.doc);
    const cur = srv.docs.get(path);
    const rev = cur?.exists ? srv.revision(`doc:${path}`) : null;
    if (rev !== p.revision) fail('conflict', '此文件已在其他位置被修改，请查看差异后重新应用');
    srv.docs.set(path, { exists: true, text: p.text });
    return docView(srv, p.doc);
  },

  'preview.render': (srv, p: { scope: Scope; draft: ConfigChange[] }) => srv.preview(p.scope, p.draft),
  'comments.stop': () => ({ stopped: true }),
  'attention.list': () => clone(attentionListFixture.data),
  'sessions.list': (srv) => ({ ...clone(sessionsListFixture.data), sessions: clone(mockSessions(srv)) }),
  'sessions.get': (srv, p: { agent: string; chat_id: string }) => {
    const s = mockSessions(srv).find((r) => r.agent === p.agent && r.chat_id === p.chat_id);
    const turns = mockTurns(srv).filter((t) => t.agent === p.agent && t.chat_id === p.chat_id);
    return {
      agent: p.agent,
      chat_id: p.chat_id,
      tag: s?.tag ?? turns[0]?.tag ?? '',
      ...(s?.name ? { name: s.name } : {}),
      ...(s ? { session: clone(s) } : {}),
      turns: clone(turns.slice(0, 30)),
      more: turns.length > 30,
      outside: mockOutside(turns),
    };
  },
  'usage.summary': (srv, p: UsageQuery) => mockUsage(srv, p),
  'insights.summary': (srv, p: InsightsQuery) => mockInsights(srv, p),
  'insights.analysis.get': (srv, p: InsightsAnalysisQuery) => mockAnalysis(srv, p),
  'insights.analysis.start': (srv, p: InsightsAnalysisQuery & { force?: boolean }) => mockAnalysisStart(srv, p),
  'logs.tail': (srv, p: MockLogParams) => {
    if (!srv.canEdit({ level: 'global' }).ok) fail('forbidden', '日志仅对本机用户与有整机设置权限的用户开放');
    return mockLogs(srv, p);
  },
  'turns.list': (srv, p: { agent: string; chat_id?: string; schedule?: string; before?: number; limit?: number }) => {
    const limit = p.limit ?? 30;
    const all = mockTurns(srv).filter(
      (t) =>
        t.agent === p.agent &&
        (!p.chat_id || t.chat_id === p.chat_id) &&
        (!p.schedule || t.schedule === p.schedule) &&
        (p.before === undefined || t.ended_at_ms < p.before),
    );
    return {
      turns: clone(all.slice(0, limit)),
      more: all.length > limit,
      ...(p.chat_id && !p.schedule && p.before !== undefined ? { outside: mockOutside(all) } : {}),
    };
  },
  'sessions.act': (
    srv,
    p: { agent: string; chat_id: string; action: SessionAction; interrupt?: boolean; turn_id?: string; reply?: string },
  ) => {
    const rows = mockSessions(srv);
    const row = rows.find((r) => r.agent === p.agent && r.chat_id === p.chat_id);
    if (!row) return { outcome: 'absent' as ActOutcome };
    if (!row.actions[p.action]) fail('forbidden', row.deny_reason ?? '无此群聊的操作权限');
    const drop = () => rows.splice(rows.indexOf(row), 1);
    switch (p.action) {
      case 'handled':
        if (row.waiting_reply !== p.reply) return { outcome: 'superseded' as ActOutcome };
        if (!row.listed) drop();
        else {
          delete row.waiting_since;
          delete row.waiting_reply;
          row.actions.handled = false;
          if (row.state === 'waiting') row.state = 'idle';
        }
        return { outcome: 'done' as ActOutcome };
      case 'interrupt':
        if (!row.busy) return { outcome: 'not_running' as ActOutcome };
        if (row.turn_id !== p.turn_id) return { outcome: 'stale' as ActOutcome };
        row.busy = false;
        row.state = 'idle';
        delete row.turn_started_at;
        delete row.turn_id;
        row.actions.interrupt = false;
        return { outcome: 'done' as ActOutcome };
      case 'exit':
      case 'archive':
        if (row.busy && !p.interrupt) return { outcome: 'running' as ActOutcome };
        drop();
        return { outcome: 'done' as ActOutcome };
    }
  },
  'sentinel.lark_user': (srv) => {
    const can = srv.canEdit({ level: 'global' });
    if (!can.ok) fail('forbidden', can.reason);
    return { open_id: SELF_OPEN, name: NAMES[SELF_OPEN] };
  },
  'names.resolve': (srv, p: { ids: string[] }) => {
    const chats = new Map(srv.chats.filter((c) => c.name).map((c) => [c.chat_id, c.name!]));
    const out: Record<string, string> = {};
    for (const id of p.ids) {
      const name = NAMES[id] ?? chats.get(id);
      if (name) out[id] = name;
    }
    return out;
  },
};

const isoAgo = (now: Date, secs: number) => new Date(now.getTime() - secs * 1000).toISOString();

/** 会话一栏:Rust 生成的样例,时间换成相对此刻;群里那条在跑的给一个本轮凭证,能演示打断。 */
function mockSessions(srv: MockServer): SessionRow[] {
  if (!srv.sessions) {
    const rows = clone((sessionsListFixture as unknown as { data: { sessions: SessionRow[] } }).data.sessions);
    for (const r of rows) {
      if (r.waiting_since) r.waiting_since = isoAgo(srv.now, 600);
      if (r.last_active) r.last_active = isoAgo(srv.now, 30);
      if (r.busy) {
        r.turn_started_at = isoAgo(srv.now, 192);
        r.turn_id = 'mock-turn';
        r.actions.interrupt = r.actions.exit;
      }
    }
    srv.sessions = rows;
  }
  return srv.sessions;
}

/** 台账:以 Rust 样例那一轮为模子,铺开几轮不同来源与结果的。 */
function mockTurns(srv: MockServer): TurnRecord[] {
  if (!srv.turns) {
    const base = (turnsListFixture as unknown as { data: { turns: TurnRecord[] } }).data.turns[0]!;
    const specs: Array<[TurnRecord['origin'], TurnRecord['end'], string, string | undefined, number, number, string?]> = [
      ['human', 'done', '帮我看下 CI 为什么挂了', '修好单测断言', 95, 6],
      ['schedule', 'done', '看一眼昨晚的 CI,挂了的标出负责人', '昨晚全绿', 42, 3],
      ['human', 'interrupted', '把这个 PR 拆成两个', undefined, 21, 2],
      ['human', 'failed', '再跑一遍集成测试', undefined, 240, 9, 'timed_out'],
      ['bot', 'empty', '同步一下进度', undefined, 8, 0],
      ['human', 'done', '这个报错是什么意思', '解释了报错原因', 33, 1],
    ];
    const now = srv.now.getTime();
    srv.turns = specs.map(([origin, end, prompt, feed, secs, tools, error], i) => {
      const ended = now - 5_000 - i * 3_600_000;
      const t: TurnRecord = {
        ...clone(base),
        origin,
        end,
        prompt_head: prompt,
        started_at_ms: ended - secs * 1000,
        ended_at_ms: ended,
        tool_steps: tools,
        message_id: `om_mock_${i}`,
      };
      if (feed) t.feed = feed;
      else delete t.feed;
      if (error) t.error = error;
      if (origin === 'schedule') {
        t.schedule = 'SCH-1';
        delete t.requester;
      }
      return t;
    });
  }
  return srv.turns;
}

/** 示例费用:k 份按 Opus 级单价折算的 token;`unpriced` 时另有一份价格表没收录的模型。 */
function mockSpend(k: number, unpriced = false): Spend {
  const tokens = {
    input: Math.round(k * 4.2e4),
    output: Math.round(k * 2.6e5),
    cache_read: Math.round(k * 1.9e7),
    cache_write: Math.round(k * 9.1e5),
  };
  const cost = {
    total: 0,
    input: tokens.input * 4e-6,
    output: tokens.output * 2e-5,
    cache_read: tokens.cache_read * 2e-7,
    cache_write: tokens.cache_write * 8e-6,
  };
  cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
  if (!unpriced) return { tokens, cost };
  const extra = Math.round(k * 1.2e5);
  tokens.input += extra;
  return { tokens, cost, unpriced: { tokens: extra, models: ['composer-2'] } };
}

/** 会话的轮外用量:在相邻两轮之间放一段后台子 agent 的请求。 */
function mockOutside(turns: TurnRecord[]): OutsideUsage[] {
  return turns
    .slice(1, 4)
    .filter((_, i) => i !== 1)
    .map((t, i) => {
      const from = t.ended_at_ms + 90_000;
      const spend = mockSpend(0.4 + i * 0.3);
      return {
        from_ms: from,
        to_ms: from + 6 * 60_000,
        usage: [
          {
            model: 'claude-opus-5-5',
            requests: 6 + i * 4,
            input: spend.tokens.input,
            output: spend.tokens.output,
            cache_read: spend.tokens.cache_read,
            cache_write_5m: 0,
            cache_write_1h: spend.tokens.cache_write,
          },
        ],
        spend,
      };
    });
}

/** 线性插值的分位数,与服务端 `pct` 同一算法;空列表为 0。 */
function quantile(sorted: number[], p: number): number {
  if (!sorted.length) return 0;
  const k = (sorted.length - 1) * p;
  const f = Math.floor(k);
  const c = Math.ceil(k);
  return sorted[f]! + (sorted[c]! - sorted[f]!) * (k - f);
}

function quantiles(values: number[]): InsightsQuantiles {
  const v = [...values].sort((x, y) => x - y);
  return { n: v.length, p50_secs: quantile(v, 0.5), p75_secs: quantile(v, 0.75), p90_secs: quantile(v, 0.9), total_secs: v.reduce((s, x) => s + x, 0) };
}

const INSIGHT_SPANS: Array<{ key: InsightsSpan; lo: number; hi: number }> = [
  { key: 'lt_1m', lo: 2, hi: 60 },
  { key: 'm1_5', lo: 60, hi: 300 },
  { key: 'm5_15', lo: 300, hi: 900 },
  { key: 'm15_60', lo: 900, hi: 3600 },
  { key: 'h1_4', lo: 3600, hi: 14400 },
  { key: 'gte_4h', lo: 14400, hi: 30000 },
];
/** 30 天里各档的间隔条数:多数间隔很短,少数长间隔占去大部分时长。 */
const INSIGHT_BASE = [22, 38, 31, 33, 19, 5];

function spanOf(secs: number): InsightsSpan {
  return [...INSIGHT_SPANS].reverse().find((s) => secs >= s.lo)!.key;
}

/** 按档铺出各条间隔的时长:档内偏向下沿。 */
function mockGaps(scale: number, stretch: number): number[] {
  return INSIGHT_SPANS.flatMap((s, i) => {
    const c = Math.round(INSIGHT_BASE[i]! * scale);
    return Array.from({ length: c }, (_, j) => Math.round(Math.min(s.hi - 1, (s.lo + (s.hi - s.lo) * ((j + 0.5) / c) ** 1.6) * stretch)));
  });
}

/** 与服务端同一套阈值得出结论,示例数据换了时间段也自洽。 */
function mockFindings(v: Omit<InsightsSummary, 'findings'>): InsightsFinding[] {
  const n = v.gaps.n;
  if (n < 30) return [{ key: 'sample_small', params: { n } }];
  const out: InsightsFinding[] = [];
  const long = v.buckets.filter((b) => b.key === 'm15_60' || b.key === 'h1_4' || b.key === 'gte_4h');
  const longCount = long.reduce((x, b) => x + b.count, 0);
  const secsShare = long.reduce((x, b) => x + b.secs, 0) / v.gaps.total_secs;
  if (secsShare >= 0.7) out.push({ key: 'tail_concentration', params: { count_share: longCount / n, secs_share: secsShare } });
  const active = v.elsewhere.reduce((x, e) => x + e.active_elsewhere, 0);
  if (v.long_gaps >= 20 && active / v.long_gaps >= 0.6) {
    out.push({ key: 'busy_elsewhere', params: { share: active / v.long_gaps, silent_share: v.long_gaps_silent / v.long_gaps } });
  }
  const next = (k: InsightsNextKind) => v.next_messages.find((r) => r.key === k)!;
  const approve = next('approve');
  if (approve.count / n < 0.1 && (next('instruction').count + next('short').count) / n >= 0.8) {
    out.push({ key: 'mostly_instructions', params: { approve_share: approve.count / n, approve_p50_secs: approve.p50_secs } });
  }
  const poll = next('poll');
  if (poll.count >= 5) out.push({ key: 'polls', params: { count: poll.count, p50_secs: poll.p50_secs } });
  if (v.workdays.days >= 3) out.push({ key: 'load', params: { messages_p50: v.workdays.messages_p50, chats_p50: v.workdays.chats_p50 } });
  if (v.agent_turns.total_secs > 0 && v.gaps.total_secs / v.agent_turns.total_secs >= 1.5) {
    out.push({ key: 'human_vs_agent', params: { ratio: v.gaps.total_secs / v.agent_turns.total_secs } });
  }
  const prev = v.previous?.gaps;
  if (prev && prev.n >= 30 && prev.p50_secs > 0) {
    const delta = (v.gaps.p50_secs - prev.p50_secs) / prev.p50_secs;
    if (Math.abs(delta) >= 0.25) out.push({ key: 'trend', params: { direction: delta > 0 ? 'up' : 'down', p50_secs: v.gaps.p50_secs, previous_p50_secs: prev.p50_secs } });
  }
  return out;
}

/** 示例环境里一次分析要多久(毫秒)。 */
export const MOCK_ANALYSIS_MS = 6000;

type MockAnalysisSlot =
  | { status: 'running'; startedAt: number }
  | { status: 'done'; at: number; stale: boolean; retry?: string }
  | { status: 'failed'; reason: string };

function analysisSlot(p: InsightsAnalysisQuery): string {
  return JSON.stringify([p.range, p.from ?? '', p.to ?? '', p.agent ?? '', p.lang]);
}

/** 主 agent:配置指定的(示例环境不判可用性)或用量最多的 starlight。 */
function mockPrimary(srv: MockServer): AnalysisPrimary {
  const configured = srv.global.primary_agent;
  const alias = typeof configured === 'string' && configured ? configured : 'starlight';
  const a = srv.agents.find((x) => x.alias === alias) ?? srv.agent('starlight');
  const model = a.layer.model;
  return {
    agent: a.alias,
    label: String(a.layer.display || a.alias),
    source: alias === configured ? 'configured' : 'usage',
    cli: String(a.layer['runtime.cli'] ?? 'claude'),
    ...(typeof model === 'string' && model ? { model } : { model: 'claude-opus-4-8' }),
  };
}

/** 各时间段的初始状态,用来演示:30 天已完成;7 天已过期且上次重新生成没成功;90 天失败;其余没有结果。 */
function seededSlot(srv: MockServer, p: InsightsAnalysisQuery): MockAnalysisSlot | undefined {
  if (p.agent) return undefined;
  const zh = p.lang === 'zh-CN';
  const at = srv.now.getTime() - 3 * 3600_000;
  if (p.range === '30d') return { status: 'done', at, stale: false };
  if (p.range === '7d') return { status: 'done', at: at - 86_400_000, stale: true, retry: zh ? 'claude 异常退出' : 'claude exited abnormally' };
  if (p.range === '90d') return { status: 'failed', reason: zh ? '分析超过 300 秒未完成，已停止' : 'The analysis did not finish within 300 seconds and was stopped' };
  return undefined;
}

function mockAnalysisText(zh: boolean): string {
  return zh
    ? '## 观察\n\n1. 15 分钟以上的间隔占条数的 **18%**，却占间隔总时长的 **74%**：时长集中在少数长间隔上。\n2. 长间隔中 68% 期间别处有消息，多半是在其它会话里处理别的事。\n3. 下一条消息以新指令为主，询问进展有 9 条，此前间隔中位数为 42 分钟。\n\n## 建议\n\n- 回复末尾集中列出需要确认的事项，减少来回。\n- 长任务在回复里写明预计完成时间，减少询问进展。\n- 用 `summon remind` 为耗时任务设置完成提醒。\n\n## 局限\n\n间隔是上界，包含回复送达到被读到的延迟；别处的消息可能来自其他人。'
    : '## Observations\n\n1. Gaps of 15 minutes or more are **18%** of gaps but **74%** of total gap time.\n2. During 68% of long gaps there were messages elsewhere.\n3. Most next messages are new instructions; 9 asked about progress after a median gap of 42 minutes.\n\n## Recommendations\n\n- List the items that need confirmation at the end of each reply.\n- State the expected finish time for long tasks.\n- Use `summon remind` to get notified when a long task finishes.\n\n## Limitations\n\nA gap is an upper bound that includes the delay before a reply is read; messages elsewhere may come from other people.';
}

/** 洞察分析:按槽(时间段、筛选、语言)记状态;发起后经过 MOCK_ANALYSIS_MS 变为已完成。 */
function mockAnalysis(srv: MockServer, p: InsightsAnalysisQuery): AnalysisView {
  const primary = mockPrimary(srv);
  const zh = p.lang === 'zh-CN';
  if (primary.cli === 'cursor') {
    const reason = zh
      ? `主 agent ${primary.label} 使用 cursor，其一次性运行无法可靠关闭 hooks，不支持分析`
      : `The main agent ${primary.label} uses cursor, whose one-shot runs cannot reliably disable hooks, so analysis is not supported`;
    return { primary, status: 'unsupported', reason, can_start: false };
  }
  const key = analysisSlot(p);
  let slot = srv.analyses.get(key) ?? seededSlot(srv, p);
  if (slot?.status === 'running' && Date.now() - slot.startedAt >= MOCK_ANALYSIS_MS) {
    slot = { status: 'done', at: Date.now(), stale: false };
    srv.analyses.set(key, slot);
  }
  const view: AnalysisView = { primary, status: slot?.status ?? 'none', can_start: srv.canEdit({ level: 'global' }).ok };
  if (slot?.status === 'running') view.started_at = new Date(slot.startedAt).toISOString();
  if (slot?.status === 'failed') view.reason = slot.reason;
  if (slot?.status === 'done') {
    view.result = {
      text: mockAnalysisText(zh),
      truncated: false,
      generated_at: new Date(slot.at).toISOString(),
      agent: primary.agent,
      cli: primary.cli,
      ...(primary.model ? { model: primary.model } : {}),
      elapsed_ms: 41_000,
      prompt_version: 1,
      usage: { input: 4_099, output: 2_948, cache_read: 0, cache_write: 0 },
      stale: slot.stale,
      ...(slot.retry ? { retry_failure: { reason: slot.retry, at: new Date(slot.at + 3600_000).toISOString() } } : {}),
    };
  }
  return view;
}

/** 发起:同槽在跑返回进行中;别的槽在跑回 busy;没过期的结果不带 force 不重跑。 */
function mockAnalysisStart(srv: MockServer, p: InsightsAnalysisQuery & { force?: boolean }): AnalysisView {
  if (!srv.canEdit({ level: 'global' }).ok) fail('forbidden', p.lang === 'zh-CN' ? '发起分析需要整机设置的权限' : 'Starting an analysis requires machine settings permission');
  const now = mockAnalysis(srv, p);
  if (now.status === 'unsupported' || now.status === 'running') return now;
  if (now.status === 'done' && !now.result?.stale && !p.force) return now;
  const key = analysisSlot(p);
  for (const [other, slot] of srv.analyses) {
    if (other !== key && slot.status === 'running' && Date.now() - slot.startedAt < MOCK_ANALYSIS_MS) {
      fail('busy', p.lang === 'zh-CN' ? '另一项分析正在进行，请稍后再试' : 'Another analysis is in progress; try again later');
    }
  }
  srv.analyses.set(key, { status: 'running', startedAt: Date.now() });
  return mockAnalysis(srv, p);
}

/** 洞察:合成一份能看出各图形态的示例 —— 30 天约 150 条间隔、触发多条结论、带上一时段;
 * 「今天」只有几条,落到样本不足。数按时间段定种子,同一个时间段每次一样。 */
function mockInsights(srv: MockServer, p: InsightsQuery): InsightsSummary {
  const ymd = (d: Date) => `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, '0')}-${String(d.getUTCDate()).padStart(2, '0')}`;
  const now = srv.now;
  const today = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}-${String(now.getDate()).padStart(2, '0')}`;
  const shift = (date: string, n: number) => {
    const d = new Date(`${date}T12:00:00Z`);
    d.setUTCDate(d.getUTCDate() + n);
    return ymd(d);
  };
  const custom = p.range === 'custom' && p.from && p.to;
  const to = custom ? p.to! : today;
  const span = custom ? Math.round((Date.parse(p.to!) - Date.parse(p.from!)) / 86400000) + 1 : { today: 1, '7d': 7, '30d': 30, '90d': 90, all: 60, custom: 30, relative: Math.max(1, Math.ceil((p.hours ?? 24) / 24)) }[p.range];
  const from = shift(to, 1 - span);
  let seed = span * 7919 + (p.agent ? 31 : 17);
  const rnd = () => {
    seed = (seed * 48271) % 2147483647;
    return seed / 2147483647;
  };
  const scale = (span / 30) * (p.agent ? 0.6 : 1);
  const gaps = mockGaps(scale, 1);
  const days: InsightsDay[] = Array.from({ length: span }, (_, i) => {
    const date = shift(from, i);
    const weekday = ((new Date(`${date}T12:00:00Z`).getUTCDay() + 6) % 7) + 1;
    const messages = Math.round((weekday <= 5 ? 6 + rnd() * 12 : rnd() * 4) * (p.agent ? 0.6 : 1));
    return { date, weekday, messages, chats: messages ? Math.min(messages, 1 + Math.floor(rnd() * 6)) : 0 };
  });
  const work = days.filter((d) => d.weekday <= 5 && d.messages > 0);
  const sorted = (xs: number[]) => [...xs].sort((x, y) => x - y);
  const human = days.reduce((x, d) => x + d.messages, 0);
  const kinds = gaps.map((g): InsightsNextKind => {
    const r = rnd();
    if (g < 900) return r < 0.12 ? 'approve' : r < 0.45 ? 'short' : 'instruction';
    return r < 0.2 ? 'poll' : r < 0.45 ? 'short' : 'instruction';
  });
  const next = (['instruction', 'short', 'approve', 'poll'] as const).map((key) => {
    const q = quantiles(gaps.filter((_, i) => kinds[i] === key));
    return { key, count: q.n, p50_secs: q.p50_secs, p90_secs: q.p90_secs };
  });
  const elsewhereShare: Record<InsightsLongSpan, [number, number]> = { m15_60: [0.72, 3], h1_4: [0.84, 7], gte_4h: [0.8, 12] };
  const elsewhere = (['m15_60', 'h1_4', 'gte_4h'] as const).map((key) => {
    const own = gaps.filter((g) => spanOf(g) === key);
    const [share, median] = elsewhereShare[key];
    return { key, count: own.length, secs: own.reduce((x, g) => x + g, 0), active_elsewhere: Math.round(own.length * share), elsewhere_messages_p50: own.length ? median : 0 };
  });
  const longGaps = elsewhere.reduce((x, e) => x + e.count, 0);
  const agentN = Math.round(human * 0.85);
  const previousGaps = mockGaps(scale * 0.82, 0.72);
  const prevLong = previousGaps.filter((g) => g >= 900);
  const base = {
    range: p.range,
    from: p.range === 'relative' ? localIso(new Date(now.getTime() - (p.hours ?? 24) * 3_600_000)) : from,
    to: p.range === 'relative' ? localIso(now) : to,
    turns: human + Math.round(span * 1.2),
    human_turns: human,
    agents: p.agent ? 1 : 3,
    chats: p.agent ? 4 : 9,
    day_start_hour: 9,
    day_end_hour: 22,
    gaps: quantiles(gaps),
    gaps_including_overnight: gaps.length + Math.round(gaps.length * 0.15),
    queued: Math.round(gaps.length * 0.06),
    buckets: INSIGHT_SPANS.map((s) => {
      const own = gaps.filter((g) => spanOf(g) === s.key);
      return { key: s.key, count: own.length, secs: own.reduce((x, g) => x + g, 0) };
    }),
    agent_turns: quantiles(Array.from({ length: agentN }, (_, j) => Math.round(25 + 900 * ((j + 0.5) / agentN) ** 2.4))),
    elsewhere,
    long_gaps: longGaps,
    long_gaps_silent: Math.round(longGaps * 0.11),
    next_messages: next,
    days,
    workdays: {
      days: work.length,
      messages_p50: quantile(sorted(work.map((d) => d.messages)), 0.5),
      messages_p90: quantile(sorted(work.map((d) => d.messages)), 0.9),
      chats_p50: quantile(sorted(work.map((d) => d.chats)), 0.5),
      chats_p90: quantile(sorted(work.map((d) => d.chats)), 0.9),
    },
    ...(p.range === 'all'
      ? {}
      : {
          previous: {
            gaps: quantiles(previousGaps),
            over_15m_count_share: previousGaps.length ? prevLong.length / previousGaps.length : 0,
            over_15m_secs_share: previousGaps.length ? prevLong.reduce((x, g) => x + g, 0) / previousGaps.reduce((x, g) => x + g, 0) : 0,
          },
        }),
  };
  return { ...base, findings: mockFindings(base) };
}

/** 用量:按时间段铺一份看起来像样的数。伪随机按时间段定种子,同一个时间段每次一样。 */
function mockUsage(srv: MockServer, p: UsageQuery): UsageView {
  const range = p.range;
  const now = srv.now;
  const live = mockSessions(srv);
  const allAgents = srv.agents.filter((a) => !a.load_error).slice(0, 3);
  const scopes = srv.chats.flatMap((chat) => allAgents.filter((a) => a.alias in chat.agents).map((a) => ({
    agent: a.alias, agent_label: srv.summary(a).display || a.alias, chat_id: chat.chat_id, chat_label: chat.name || chat.chat_id,
  })));
  const selected = scopes.filter((s) => (!p.agent || s.agent === p.agent) && (!p.chat_id || s.chat_id === p.chat_id));
  const share = selected.length / Math.max(1, scopes.length);
  const unit: UsageUnit = range === 'today' || range === 'relative' && (p.hours ?? 24) <= 48 ? 'hour' : 'day';
  const end = range === 'custom' && p.to ? new Date(`${p.to}T23:59:59`) : now;
  const n = range === 'relative' ? Math.ceil((p.hours ?? 24) / (unit === 'hour' ? 1 : 24)) : range === 'custom' && p.from && p.to ? Math.round((Date.parse(p.to) - Date.parse(p.from)) / 86400000) + 1 : range === 'today' ? now.getHours() + 1 : range === '7d' ? 7 : range === '30d' ? 30 : 90;
  let seed = n * 7919 + 17;
  const rnd = () => {
    seed = (seed * 48271) % 2147483647;
    return seed / 2147483647;
  };
  const starts = Array.from({ length: n }, (_, i) => {
    if (range === 'relative') return new Date(end.getTime() - (p.hours ?? 24) * 3_600_000 + i * (unit === 'hour' ? 3_600_000 : 86_400_000));
    const d = new Date(end);
    d.setMinutes(0, 0, 0);
    if (unit === 'hour') d.setHours(i);
    else {
      d.setHours(0);
      d.setDate(d.getDate() - (n - 1 - i));
    }
    return d;
  });
  const buckets: UsageBucket[] = starts.map((d) => {
    const busy = unit === 'hour' ? (d.getHours() >= 9 && d.getHours() <= 22 ? 1 : 0.1) : d.getDay() % 6 === 0 ? 0.35 : 1;
    const scale = (unit === 'hour' ? 7 : 40) * busy;
    return {
      start: localIso(d),
      done: Math.round((rnd() * scale + scale / 3) * share),
      empty: Math.round(rnd() * scale * 0.08 * share),
      silent_reset: rnd() < 0.08 * share ? 1 : 0,
      interrupted: Math.round(rnd() * scale * 0.06 * share),
      failed: rnd() < 0.3 * busy * share ? 1 + Math.round(rnd()) : 0,
    };
  });
  const sum = (k: keyof UsageCounts) => buckets.reduce((acc, b) => acc + b[k], 0);
  const turns = sum('done') + sum('empty') + sum('silent_reset') + sum('failed') + sum('interrupted');
  const failed = sum('failed');
  const interrupted = sum('interrupted');
  const avg = 74;
  const row = (key: string, label: string, share: number, extra: Partial<UsageRow> = {}): UsageRow => {
    const t = Math.round(turns * share);
    return {
      key,
      label,
      turns: t,
      failed: Math.round(failed * share),
      interrupted: Math.round(interrupted * share),
      seconds: t * avg,
      avg_secs: Math.round(avg * (0.6 + share)),
      p90_secs: Math.round(avg * (2.2 + share)),
      spend: mockSpend(t / 10),
      ...extra,
    };
  };
  const agents = allAgents.filter((a) => selected.some((s) => s.agent === a.alias));
  const by_agent = agents.map((a) => row(a.alias, srv.summary(a).display || a.alias,
    selected.filter((s) => s.agent === a.alias).length / selected.length, { agent: a.alias }));
  const by_chat = selected.map((s) => row(`${s.agent}/${s.chat_id}`, s.chat_label, 1 / selected.length, { agent: s.agent, chat_id: s.chat_id }));
  const by_cli = [
    row('claude', 'claude', 0.6),
    row('codex', 'codex', 0.3),
    row('cursor', 'cursor', 0.1, { spend: mockSpend(turns / 100, true) }),
  ];
  const dayRows = (unit === 'hour' ? [{ start: localIso(new Date(new Date(now).setHours(0, 0, 0, 0))), b: buckets }] : buckets.map((b) => ({ start: b.start, b: [b] })))
    .map(({ start, b }) => {
      const t = b.reduce((acc, x) => acc + x.done + x.empty + x.silent_reset + x.failed + x.interrupted, 0);
      return { start, turns: t, spend: mockSpend(t / 10) };
    })
    .filter((d) => d.turns > 0)
    .reverse();
  const t0 = starts[0]!.getTime();
  const span = Math.max(1, end.getTime() - t0);
  const nowSecs = Math.round(now.getTime() / 1000);
  const quota: UsageQuota[] = agents.slice(0, 2).map((a, i) => {
    const points = Array.from({ length: 24 }, (_, j) => ({
      at_ms: Math.round(t0 + (span * (j + 1)) / 24),
      five_hour_left: Math.max(4, 100 - ((j * (9 + i * 7)) % 96)),
      long_left: Math.max(10, Math.round(88 - i * 30 - j * 0.9)),
    }));
    const last = points[points.length - 1]!;
    return {
      agent: a.alias,
      label: srv.summary(a).display || a.alias,
      cli: i ? 'codex' : 'claude',
      at: localIso(new Date(last.at_ms)),
      five_hour_left: last.five_hour_left,
      five_hour_resets_at: nowSecs + 3 * 3600,
      long_left: last.long_left,
      long_resets_at: nowSecs + 4 * 86400,
      points,
    };
  });
  return {
    range,
    scopes,
    today: localIso(now).slice(0, 10),
    unit,
    since: localIso(starts[0]!),
    until: localIso(end),
    totals: {
      turns,
      seconds: turns * avg,
      failed,
      interrupted,
      spend: mockSpend(turns / 10, true),
      outside: mockSpend(turns / 45),
      working: p.agent || p.chat_id ? undefined : live.filter((r) => r.state === 'working' || r.state === 'blocked').length,
      waiting: p.agent || p.chat_id ? undefined : live.filter((r) => r.state === 'waiting').length,
    },
    buckets,
    by_agent,
    by_chat,
    by_cli,
    by_model: [row('claude-test-1', 'claude-test-1', 0.6), row('gpt-test-1', 'gpt-test-1', 0.3), row('composer-2', 'composer-2', 0.1, { spend: mockSpend(turns / 100, true) })],
    days: dayRows,
    quota,
    prices: {
      source: 'https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json',
      fetched_at: localIso(new Date(now.getTime() - 5 * 3600_000)),
    },
  };
}

type MockLogParams = { file: LogFile; before?: string; limit?: number; query?: string; agent?: string; since?: string; until?: string; errors?: boolean };

const MOCK_DAEMON_LINES = [
  '[summon] [starlight] 收到消息 chat=oc_00000000000000000000000000000001',
  '[summon] [starlight] 本轮完成:4 步工具,用时 38 秒',
  '[summon] [arcadia] 会话镜像已启用 chat=oc_00000000000000000000000000000002',
  '[summon] [nimbus] 投递失败(将重试):timeout after 30s',
  '[summon] [starlight] 跑了一条命令\n  git status --short\n  M dashboard/src/pages/usage.ts',
  '[summon] sidecar上线 protocol=2',
  '[summon] [arcadia] 额度读数:5 小时剩 62%',
];

/** 日志:从「此刻」往前每 37 秒一条,按条件筛、按序号翻页。 */
function mockLogs(srv: MockServer, p: MockLogParams): LogsPage {
  const now = srv.now.getTime();
  const count = p.file === 'daemon' ? 420 : p.file === 'sidecar' ? 60 : 6;
  const pad = (n: number) => String(n).padStart(2, '0');
  const all: LogLine[] = Array.from({ length: count }, (_, i) => {
    const at = new Date(now - 4_000 - i * 37_000);
    const date = `${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())}`;
    const time = `${pad(at.getHours())}:${pad(at.getMinutes())}:${pad(at.getSeconds())}`;
    const text =
      p.file === 'audit'
        ? JSON.stringify({ ts: localIso(at), identity: { kind: 'owner' }, file: 'global', keys: ['menubar.icon'], result: 'ok' })
        : p.file === 'sidecar'
          ? `${date.replace(/-/g, '/')} ${time} [Info] [event-dispatch is ready]`
          : `[${date} ${time}] ${MOCK_DAEMON_LINES[i % MOCK_DAEMON_LINES.length]}`;
    return { at: localIso(at), text, error: /error|panic|fail|失败|错误/i.test(text) };
  });
  const since = p.since ? Date.parse(p.since) : -Infinity;
  const until = p.until ? Date.parse(p.until) : Infinity;
  const q = p.query?.trim().toLowerCase();
  const kept = all.filter((e) => {
    const t = Date.parse(e.at!);
    return (
      t >= since &&
      t <= until &&
      (!p.errors || e.error) &&
      (!q || e.text.toLowerCase().includes(q)) &&
      (!p.agent || e.text.includes(`[${p.agent}]`) || e.text.includes(`"${p.agent}"`))
    );
  });
  const start = p.before ? Number(p.before) : 0;
  const limit = p.limit ?? 200;
  const end = start + limit;
  return { file: p.file, entries: kept.slice(start, end), ...(end < kept.length ? { before: String(end) } : {}) };
}

function chatAgentView(srv: MockServer, c: MockChat, alias: string): ChatAgentView {
  const st = c.agents[alias]!;
  const a = srv.agent(alias);
  const config = a.load_error
    ? { scope: { level: 'chat' as const, agent: alias, chat_id: c.chat_id }, revision: srv.revision(`chat:${alias}:${c.chat_id}`), values: [] }
    : srv.configView({ level: 'chat', agent: alias, chat_id: c.chat_id });
  let reason: string | undefined;
  let canEnable = true;
  if (c.kind === 'p2p') {
    canEnable = false;
    reason = '私聊无需 @。';
  } else if (srv.identity.kind === 'owner') {
    canEnable = false;
    reason = '从本机打开的控制台没有飞书身份，无法证明「群聊中唯一的真人是你」，此处只能关闭。如需开启，请从飞书打开控制台，或在群聊中 @ Bot 发送 /mentionless on。';
  }
  const mapping = ((a.layer['runtime.chat_working_dirs'] as Record<string, string>) ?? {})[c.chat_id];
  const work: ChatAgentView['work_dir'] = { live: st.live, editable: !st.live };
  const agentDir = String(a.layer['runtime.working_dir'] ?? '');
  if (st.override) Object.assign(work, { effective: st.override, source: 'chat_override', override: st.override });
  else if (mapping) Object.assign(work, { effective: mapping, source: 'chat_mapping' });
  else if (agentDir) Object.assign(work, { effective: agentDir, source: 'agent_default' });
  if (mapping) work.mapping = mapping;
  if (st.live) work.reason = '该群聊的会话正在运行。工作目录仅在下次启动会话时生效，请先在群聊中退出会话再修改。';
  const ops: ChatAgentView['operators'] = {
    members: st.operators.map((id) => (NAMES[id] ? { id, name: NAMES[id] } : { id })),
    revision: srv.revision(`agent:${alias}`),
    editable: !a.load_error,
  };
  if (a.load_error) ops.reason = 'agent.yaml 读取失败';
  return {
    agent: alias,
    config,
    mentionless: { enabled: st.mentionless, can_disable: st.mentionless, can_enable: canEnable && !st.mentionless, ...(reason ? { reason } : {}) },
    operators: ops,
    work_dir: work,
    doc: { exists: !!srv.docs.get(docPath({ level: 'chat', agent: alias, chat_id: c.chat_id }))?.exists },
    schedules: srv.schedules.filter((s) => s.agent === alias && s.chat_id === c.chat_id).map((s) => scheduleView(srv, s)),
  };
}

function scheduleView(srv: MockServer, s: MockSchedule): ScheduleView {
  const c = srv.chats.find((x) => x.chat_id === s.chat_id);
  const { when: _when, ...rest } = s;
  void _when;
  const v: ScheduleView = { ...clone(rest), editable: true };
  if (c) v.chat_name = c.kind === 'p2p' ? `私聊 · ${c.name ?? ''}` : (c.name ?? c.chat_id);
  if (v.next_fire === undefined) delete v.next_fire;
  if (v.last_fired === undefined) delete v.last_fired;
  return v;
}

function docView(srv: MockServer, doc: DocRef): DocView {
  const path = docPath(doc);
  const d = srv.docs.get(path);
  const v: DocView = { exists: !!d?.exists, text: d?.text ?? '', path, editable: true, effect: 'new_session' };
  if (d?.exists) v.revision = srv.revision(`doc:${path}`);
  return v;
}

function globalYaml(srv: MockServer): string {
  return toYaml(srv.global, [
    '# summon 整机配置。没写的键用内置默认;agent.yaml 与按群设置可以再覆盖。',
    '# 改完 daemon 会热重载;启动期参数(端口、监听、工具路径)要重启 daemon。',
    '',
  ]).replace('messaging:\n', srv.bumps.has('fixed-typo') ? 'messaging:\n' : 'messaging:\n  cot_visable: true\n');
}

function agentYaml(srv: MockServer, alias: string): string {
  const a = srv.agent(alias);
  const flat = { ...a.layer, status: a.enabled ? 'active' : 'disabled', 'feishu.app_id_env': a.app_id_env, 'feishu.scope': a.scope };
  const order = ['name', 'display', 'status', 'model', 'effort'];
  const sorted: Record<string, unknown> = {};
  for (const k of order) if (k in flat) sorted[k] = flat[k as keyof typeof flat];
  for (const [k, v] of Object.entries(flat)) if (!(k in sorted)) sorted[k] = v;
  return `${toYaml(sorted, [`# ${(a.layer.display as string) || alias} 的自我认知与运行时配置`, ''])}\ncognition:\n  history_lookback_days: 2\n  writeback: propose\n`;
}

function rawCheck(srv: MockServer, p: RawFile & { text: string }): RawCheck {
  const key = p.file === 'global' ? 'global' : `agent:${p.agent}`;
  const before = srv.raw.get(key) ?? (p.file === 'global' ? globalYaml(srv) : agentYaml(srv, p.agent));
  const errors: RawIssue[] = [];
  const warnings: RawIssue[] = [];
  p.text.split('\n').forEach((line, i) => {
    const tab = line.search(/^\s*\t/);
    if (tab >= 0) errors.push({ line: i + 1, col: line.indexOf('\t') + 1, message: `第 ${i + 1} 行使用了 Tab 缩进；YAML 仅支持空格缩进。` });
    if (line === SATURN_BROKEN_LINE) {
      errors.push({ line: i + 1, col: 14, message: `第 ${i + 1} 行第 14 列：列表项不能与冒号位于同一行，请将「- …」移到下一行并缩进两格。` });
    }
    const typo = /^\s*cot_visable:/.exec(line);
    if (typo) warnings.push({ line: i + 1, col: 3, key: 'messaging.cot_visable', message: '未知键 `messaging.cot_visable`，daemon 会忽略它', suggestion: 'messaging.cot_visible' });
    if (/^tmux_bin:/.test(line)) warnings.push({ line: i + 1, key: 'tmux_bin', message: '已废弃：请改用 tools.tmux' });
    if (/^\s+bogus:/.test(line) && /execution/.test(p.text)) errors.push({ line: i + 1, key: 'runtime.execution.bogus', message: 'runtime.execution 不接受未知键 bogus，整个文件将无法读取。' });
  });
  const name = p.file === 'global' ? 'config.yaml' : `${p.agent}/agent.yaml`;
  return { diff: unifiedDiff(before, p.text, name), errors, warnings };
}

// ── 对外:Transport 与示例环境的控制面 ──────────────────────────────────

export function createMockTransport(srv: MockServer, latencyMs = 0): Transport {
  return async (method, body) => {
    if (latencyMs) await new Promise((r) => setTimeout(r, latencyMs + Math.random() * latencyMs));
    const h = H[method];
    if (!h) return { ok: false, error: { code: 'not_found', message: `示例环境没有实现 ${method}` } } as ApiResult<unknown>;
    try {
      return { ok: true, data: clone(h(srv, clone(body) as never)) };
    } catch (err) {
      if (err instanceof MockFail) return { ok: false, error: err.error };
      return { ok: false, error: { code: 'internal', message: String(err) } };
    }
  };
}

export const MOCK_METHODS = Object.keys(H);

/** 模拟「别处改了文件」:手改、Menu Bar、/set 或另一个页面。 */
export function externalEdit(srv: MockServer, target: 'global' | 'starlight' | 'schedules'): string {
  if (target === 'global') {
    srv.global['messaging.progress_max_steps'] = ((srv.global['messaging.progress_max_steps'] as number) ?? 3) + 1;
    srv.bump('global');
    return '已模拟:别处把 config.yaml 的 messaging.progress_max_steps 改了';
  }
  if (target === 'starlight') {
    srv.agent('starlight').layer['messaging.idle_ttl_secs'] = 43200;
    srv.bump('agent:starlight');
    return '已模拟:别处改了 starlight 的 agent.yaml';
  }
  const s = srv.schedules.find((x) => x.id === 'SCH-2');
  if (s) s.prompt = `${s.prompt}(别处刚改过)`;
  srv.bump('schedules');
  return '已模拟:agent 自己经 summon remind 改了一条定时任务';
}
