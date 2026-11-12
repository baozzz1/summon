import { t as translate } from '../i18n';
// 运行时类型守卫:逐字段对照 types.ts。对象一律「多一个键也不行」,这样 Rust 侧把字段名
// 拼错、改名或多吐一个字段时,前端的契约测试当场挂,而不是到页面上才显示成空白。
import type { Method } from './types';

export type Guard = (v: unknown, path: string) => string | null;

const describe = (v: unknown) => (v === null ? 'null' : Array.isArray(v) ? 'array' : typeof v);

export const str: Guard = (v, p) => (typeof v === 'string' ? null : translate('d4f9e04', [p, describe(v)]));
export const num: Guard = (v, p) =>
  typeof v === 'number' && Number.isFinite(v) ? null : translate('dbf9c39', [p, describe(v)]);
export const bool: Guard = (v, p) => (typeof v === 'boolean' ? null : translate('d2754ef', [p, describe(v)]));
export const any: Guard = () => null;

export const lit =
  (...values: Array<string | number | boolean>): Guard =>
  (v, p) =>
    values.includes(v as string) ? null : translate('dbc426c', [p, values.map((x) => JSON.stringify(x)).join(' | '), JSON.stringify(v)]);

export const arr =
  (item: Guard): Guard =>
  (v, p) => {
    if (!Array.isArray(v)) return translate('d6bbc58', [p, describe(v)]);
    for (let i = 0; i < v.length; i++) {
      const e = item(v[i], `${p}[${i}]`);
      if (e) return e;
    }
    return null;
  };

export const record =
  (value: Guard): Guard =>
  (v, p) => {
    if (!v || typeof v !== 'object' || Array.isArray(v)) return translate('dd0178d', [p, describe(v)]);
    for (const [k, val] of Object.entries(v)) {
      const e = value(val, `${p}.${k}`);
      if (e) return e;
    }
    return null;
  };

export const nullable =
  (g: Guard): Guard =>
  (v, p) =>
    v === null ? null : g(v, p);

/** 精确对象:必填键都在、可选键类型对、没有任何别的键。 */
export const obj =
  (required: Record<string, Guard>, optional: Record<string, Guard> = {}): Guard =>
  (v, p) => {
    if (!v || typeof v !== 'object' || Array.isArray(v)) return translate('dd0178d', [p, describe(v)]);
    const o = v as Record<string, unknown>;
    for (const [k, g] of Object.entries(required)) {
      if (!(k in o)) return translate('dc16b6a', [p, k]);
      const e = g(o[k], `${p}.${k}`);
      if (e) return e;
    }
    for (const [k, g] of Object.entries(optional)) {
      if (k in o && o[k] !== undefined) {
        const e = g(o[k], `${p}.${k}`);
        if (e) return e;
      }
    }
    for (const k of Object.keys(o)) {
      if (!(k in required) && !(k in optional)) return translate('d85bccb', [p, k]);
    }
    return null;
  };

/** 按判别字段分派的联合类型;每个分支同样是精确对象(判别字段本身算必填)。 */
export const tagged =
  (tag: string, variants: Record<string, [Record<string, Guard>, Record<string, Guard>?]>): Guard =>
  (v, p) => {
    if (!v || typeof v !== 'object' || Array.isArray(v)) return translate('dd0178d', [p, describe(v)]);
    const t = (v as Record<string, unknown>)[tag];
    const variant = typeof t === 'string' ? variants[t] : undefined;
    if (!variant) return translate('d3cee3a', [p, tag, JSON.stringify(t), Object.keys(variants).join(' | ')]);
    const [req, opt] = variant;
    return obj({ ...req, [tag]: lit(t as string) }, opt ?? {})(v, p);
  };

// ── 基础类型 ────────────────────────────────────────────────────────────

const effect = lit('immediate', 'next_turn', 'new_session', 'next_poll', 'restart');
const layer = lit('builtin', 'global', 'agent', 'chat');
const chatKind = lit('p2p', 'group');
const empty = obj({});

export const G_SCOPE = tagged('level', {
  global: [{}],
  agent: [{ agent: str }],
  chat: [{ agent: str, chat_id: str }],
});

const identity = tagged('kind', {
  owner: [{}],
  feishu: [{ name: str, agent: str, chat_id: str, chat_kind: chatKind }],
});

export const G_WHOAMI = obj({ identity, csrf: str, expires_at: str, hostname: str, version: str, machine: bool });

const fieldOption = obj({ value: str, label: str });

export const G_FIELD_DESC = obj(
  {
    key: str,
    label: str,
    description: str,
    group: str,
    kind: lit('bool', 'enum', 'int', 'duration_secs', 'string', 'path', 'string_list', 'string_map'),
    layers: arr(lit('global', 'agent', 'chat')),
    effect,
    sensitive: bool,
    write: lit('form', 'guarded', 'readonly'),
    preview: bool,
  },
  { options: arr(fieldOption), min: num, max: num, inverted: bool },
);

export const G_SCHEMA = obj({
  groups: arr(
    obj(
      { id: str, label: str, section: lit('identity', 'runtime', 'permissions', 'messaging', 'system') },
      { description: str },
    ),
  ),
  fields: arr(G_FIELD_DESC),
});

const fieldValue = obj(
  { key: str, value: any, effective: any, source: layer, overridden_by: arr(str), editable: bool },
  { deny_reason: str },
);

export const G_CONFIG_VIEW = obj({ scope: G_SCOPE, revision: str, values: arr(fieldValue) }, { reactions: arr(fieldOption) });

const configChange = obj({ key: str, value: any });
const restartItem = obj({ key: str, file: str });

export const G_SAVE_RESULT = obj({
  revision: str,
  effects: arr(obj({ key: str, effect })),
  restart_pending: arr(restartItem),
});

const rawFile = (extra: Record<string, Guard>) =>
  tagged('file', { global: [{ ...extra }], agent: [{ agent: str, ...extra }] });

const rawView = obj({ revision: str, text: str, path: str, editable: bool });
const rawIssue = obj({ message: str }, { line: num, col: num, key: str, suggestion: str });
const rawCheck = obj({ diff: str, errors: arr(rawIssue), warnings: arr(rawIssue) });

const agentSummary = obj(
  {
    alias: str,
    name: str,
    display: str,
    cli: str,
    enabled: bool,
    routable: bool,
    credentials: lit('ready', 'missing'),
    chats: num,
  },
  { model: str, effort: str, load_error: str },
);

const statusView = obj({
  language: lit('zh-CN', 'en'),
  daemon: obj({
    version: str,
    commit: str,
    started_at: str,
    pid: num,
    autostart: lit('on', 'off', 'unknown'),
  }),
  hostname: str,
  restart_pending: arr(restartItem),
  agents: arr(agentSummary),
  revisions: record(str),
});

const issue = obj(
  { severity: lit('error', 'warn'), title: str, detail: str, fix: str },
  {
    link: obj(
      { page: lit('global', 'agent', 'chat', 'schedules', 'session') },
      { agent: str, chat_id: str, key: str },
    ),
  },
);

const member = obj({ id: str }, { name: str });

const agentDetail = obj(
  {
    summary: agentSummary,
    app_id_env: str,
    scope: str,
    model_candidates: arr(str),
    effort_options: arr(str),
    members: arr(member),
    can_toggle_enabled: bool,
  },
  { deny_reason: str },
);

const chatSummary = obj(
  {
    chat_id: str,
    kind: chatKind,
    agents: arr(obj({ agent: str, overrides: num, live: bool }, { last_active: str })),
  },
  { name: str, last_active: str },
);

const scheduleView = obj(
  {
    id: str,
    agent: str,
    chat_id: str,
    kind: str,
    spec: str,
    human: str,
    prompt: str,
    output: str,
    context: str,
    enabled: bool,
    created_by: str,
    editable: bool,
  },
  { chat_name: str, next_fire: str, last_fired: str },
);

const chatAgentView = obj({
  agent: str,
  config: G_CONFIG_VIEW,
  mentionless: obj({ enabled: bool, can_disable: bool, can_enable: bool }, { reason: str }),
  operators: obj({ members: arr(member), revision: str, editable: bool }, { reason: str }),
  work_dir: obj(
    { live: bool, editable: bool },
    {
      effective: str,
      source: lit('chat_override', 'chat_mapping', 'agent_default'),
      mapping: str,
      override: str,
      reason: str,
    },
  ),
  doc: obj({ exists: bool }),
  schedules: arr(scheduleView),
});

const chatDetail = obj({ chat_id: str, kind: chatKind, agents: arr(chatAgentView) }, { name: str });

const mentionlessJob = obj(
  { job: str, status: lit('pending', 'done', 'failed') },
  { message: str, apply_url: str },
);

const whenCheck = obj({ ok: bool, next: arr(str) }, { human: str, error: str, forms: str });
const scheduleEdit = obj({}, { when: str, prompt: str, output: str, context: str });

const docRef = tagged('level', {
  global: [{}],
  agent: [{ agent: str, file: lit('CLAUDE.md', 'SOUL.md') }],
  chat: [{ agent: str, chat_id: str }],
});

const docView = obj({ exists: bool, text: str, path: str, editable: bool, effect }, { revision: str });

const cotItem = tagged('type', {
  thinking: [{ id: str, text: str }],
  tool: [{ id: str, icon: str, name: str }, { args: str, result: str }],
  text: [{ id: str, text: str }],
});

const previewMessage = tagged('kind', {
  user: [{ id: str, sender: str, text: str, reactions: arr(str) }],
  cot: [{ id: str, title: str, status: lit('done', 'error'), items: arr(cotItem) }],
  card: [{ id: str, card: any }],
  feed: [{ id: str, chat_name: str, text: str }],
});

export const G_PREVIEW_VIEW = obj({ title: str, messages: arr(previewMessage), regions: record(arr(str)) });

const previewRequest = obj({ scope: G_SCOPE, draft: arr(configChange) });

// ── 方法表:每个方法的参数与返回各一个守卫。TS 的映射类型保证不漏方法。 ────

const commentTask = obj({
  id: str,
  agent: str,
  title: str,
  stage: str,
  status: str,
  status_en: str,
  busy: bool,
  received_ms: num,
  updated_ms: num,
  pid: nullable(num),
  error: nullable(str),
});

const sessionRow = obj(
  {
    agent: str,
    agent_display: str,
    tag: str,
    state: lit('leaked', 'blocked', 'waiting', 'working', 'idle'),
    listed: bool,
    busy: bool,
    capture: str,
    actions: obj({ handled: bool, interrupt: bool, exit: bool, archive: bool }),
  },
  {
    cli: str,
    chat_id: str,
    chat_type: str,
    name: str,
    turn_started_at: str,
    turn_id: str,
    blocked: str,
    waiting_since: str,
    waiting_reply: str,
    link: str,
    last_active: str,
    deny_reason: str,
  },
);

const tokenCounts = obj({ input: num, output: num, cache_read: num, cache_write: num });

const modelUsage = obj(
  { model: str, requests: num, input: num, output: num, cache_read: num, cache_write_5m: num, cache_write_1h: num },
  { band: num, variant: str },
);

const spend = obj(
  {
    tokens: tokenCounts,
    cost: obj({ total: num, input: num, output: num, cache_read: num, cache_write: num }),
  },
  { unpriced: obj({ tokens: num, models: arr(str) }) },
);

const turnRecord = obj(
  {
    agent: str,
    chat_id: str,
    tag: str,
    origin: lit('human', 'bot', 'schedule', 'sentinel', 'slash', 'internal', 'recovery'),
    message_id: str,
    started_at_ms: num,
    ended_at_ms: num,
    end: lit('done', 'empty', 'silent_reset', 'failed', 'interrupted'),
    tool_steps: num,
  },
  {
    turn: str,
    schedule: str,
    requester: str,
    prompt_head: str,
    error: str,
    reattached: bool,
    cli: str,
    model: str,
    effort: str,
    ctx: obj({}, { used: num, window: num, pct: num }),
    usage: arr(modelUsage),
    spend,
    quota: obj({}, { five_hour_left: num, five_hour_resets_at: num, long_left: num, long_resets_at: num }),
    feed: str,
  },
);

const outsideUsage = obj({ from_ms: num, to_ms: num, usage: arr(modelUsage) }, { spend });

const turnsPage = obj({ turns: arr(turnRecord), more: bool }, { outside: arr(outsideUsage) });

const usageRange = lit('today', '7d', '30d', '90d', 'all', 'custom');

const logFile = lit('daemon', 'sidecar', 'audit');

const usageRow = obj(
  { key: str, label: str, turns: num, failed: num, interrupted: num, seconds: num, avg_secs: num, p90_secs: num },
  { agent: str, chat_id: str, tag: str, spend },
);

const usageView = obj({
  range: usageRange,
  today: str,
  scopes: arr(obj({ agent: str, agent_label: str, chat_id: str, chat_label: str })),
  unit: lit('hour', 'day'),
  since: str,
  until: str,
  totals: obj({ turns: num, seconds: num, failed: num, interrupted: num }, { spend, outside: spend, working: num, waiting: num }),
  buckets: arr(obj({ start: str, done: num, empty: num, silent_reset: num, failed: num, interrupted: num })),
  by_agent: arr(usageRow),
  by_chat: arr(usageRow),
  by_cli: arr(usageRow),
  by_model: arr(usageRow),
  days: arr(obj({ start: str, turns: num }, { spend })),
  quota: arr(
    obj(
      { agent: str, label: str, at: str, points: arr(obj({ at_ms: num }, { five_hour_left: num, long_left: num })) },
      { cli: str, five_hour_left: num, five_hour_resets_at: num, long_left: num, long_resets_at: num },
    ),
  ),
}, { prices: obj({ source: str, fetched_at: str }) });

const quantiles = obj({ n: num, p50_secs: num, p75_secs: num, p90_secs: num, total_secs: num });
const numOrStr: Guard = (v, p) => (typeof v === 'string' ? null : num(v, p));

const insightsSummary = obj(
  {
    range: usageRange,
    from: str,
    to: str,
    turns: num,
    human_turns: num,
    agents: num,
    chats: num,
    day_start_hour: num,
    day_end_hour: num,
    gaps: quantiles,
    gaps_including_overnight: num,
    queued: num,
    buckets: arr(obj({ key: lit('lt_1m', 'm1_5', 'm5_15', 'm15_60', 'h1_4', 'gte_4h'), count: num, secs: num })),
    agent_turns: quantiles,
    elsewhere: arr(
      obj({ key: lit('m15_60', 'h1_4', 'gte_4h'), count: num, secs: num, active_elsewhere: num, elsewhere_messages_p50: num }),
    ),
    long_gaps: num,
    long_gaps_silent: num,
    next_messages: arr(obj({ key: lit('instruction', 'short', 'approve', 'poll'), count: num, p50_secs: num, p90_secs: num })),
    days: arr(obj({ date: str, weekday: num, messages: num, chats: num })),
    workdays: obj({ days: num, messages_p50: num, messages_p90: num, chats_p50: num, chats_p90: num }),
    findings: arr(
      obj({
        key: lit('sample_small', 'tail_concentration', 'busy_elsewhere', 'mostly_instructions', 'polls', 'load', 'human_vs_agent', 'trend'),
        params: record(numOrStr),
      }),
    ),
  },
  { previous: obj({ gaps: quantiles, over_15m_count_share: num, over_15m_secs_share: num }) },
);

const analysisView = obj(
  { status: lit('none', 'running', 'done', 'failed', 'unsupported'), can_start: bool },
  {
    primary: obj({ agent: str, label: str, source: lit('configured', 'usage', 'first'), cli: str }, { model: str }),
    result: obj(
      { text: str, truncated: bool, generated_at: str, agent: str, cli: str, elapsed_ms: num, prompt_version: num, stale: bool },
      {
        model: str,
        usage: obj({ input: num, output: num, cache_read: num, cache_write: num }),
        retry_failure: obj({ reason: str, at: str }),
      },
    ),
    reason: str,
    started_at: str,
  },
);
const analysisQuery = (extra: Record<string, Guard> = {}) =>
  obj({ range: usageRange, lang: lit('zh-CN', 'en') }, { from: str, to: str, agent: str, ...extra });

const cleanupJob = obj({ job: str, agent: str, status: lit('preparing', 'ready', 'cleaning', 'done', 'failed'),
  message: nullable(str), bytes: nullable(num), sha256: nullable(str), filename: str, paths: nullable(arr(str)), downloaded: bool });

export const METHOD_GUARDS: { [M in Method]: { params: Guard; data: Guard } } = {
  'session.whoami': { params: empty, data: G_WHOAMI },
  'status.get': { params: empty, data: statusView },
  'status.issues': { params: empty, data: arr(issue) },
  'config.schema': { params: empty, data: G_SCHEMA },
  'config.get': { params: obj({ scope: G_SCOPE }), data: G_CONFIG_VIEW },
  'config.patch': {
    params: obj({ scope: G_SCOPE, revision: str, changes: arr(configChange) }),
    data: G_SAVE_RESULT,
  },
  'config.raw.get': { params: rawFile({}), data: rawView },
  'config.raw.check': { params: rawFile({ text: str }), data: rawCheck },
  'config.raw.put': { params: rawFile({ revision: str, text: str }), data: G_SAVE_RESULT },
  'agents.list': { params: empty, data: arr(agentSummary) },
  'agents.get': { params: obj({ agent: str }), data: agentDetail },
  'agents.cleanup.prepare': { params: obj({ agent: str }), data: cleanupJob },
  'agents.cleanup.status': { params: obj({ job: str }), data: cleanupJob },
  'agents.cleanup.download': { params: obj({ job: str, offset: num }), data: obj({ offset: num, next: num, done: bool, base64: str }) },
  'agents.cleanup.confirm': { params: obj({ job: str, agent: str, saved: bool }), data: cleanupJob },
  'agents.set_enabled': { params: obj({ agent: str, enabled: bool }), data: agentSummary },
  'chats.list': { params: empty, data: arr(chatSummary) },
  'chats.get': { params: obj({ chat_id: str }), data: chatDetail },
  'chats.set_mentionless': {
    params: obj({ agent: str, chat_id: str, enabled: bool }),
    data: mentionlessJob,
  },
  'chats.mentionless_job': { params: obj({ job: str }), data: mentionlessJob },
  'chats.set_operators': {
    params: obj({ agent: str, chat_id: str, revision: str, members: arr(str) }),
    data: G_SAVE_RESULT,
  },
  'chats.set_work_dir': {
    params: obj({ agent: str, chat_id: str, revision: str, mapping: nullable(str), clear_override: bool }),
    data: chatAgentView,
  },
  'schedules.list': { params: empty, data: arr(scheduleView) },
  'schedules.check_when': { params: obj({ when: str }), data: whenCheck },
  'schedules.edit': { params: obj({ id: str, edit: scheduleEdit }), data: scheduleView },
  'schedules.set_enabled': { params: obj({ id: str, enabled: bool }), data: scheduleView },
  'docs.get': { params: obj({ doc: docRef }), data: docView },
  'docs.put': { params: obj({ doc: docRef, revision: nullable(str), text: str }), data: docView },
  'preview.render': { params: previewRequest, data: G_PREVIEW_VIEW },
  'names.resolve': { params: obj({ ids: arr(str) }, { agent: str }), data: record(str) },
  'sentinel.lark_user': { params: obj({}, { profile: str }), data: obj({ open_id: str }, { name: str }) },
  'comments.stop': { params: obj({ agent: str, id: str }), data: obj({ stopped: bool }) },
  'attention.list': { params: empty, data: obj({ items: arr(obj({ id: str, agent: str, agent_display: str, chat_name: str, source: lit('agent_ask', 'claude_question', 'claude_permission', 'codex_question', 'codex_approval'), summary: str, question_count: num, registered_at: str, waited_secs: num, link: str, link_target: lit('message', 'chat') })), observed_at: str, coverage: lit('structured_requests'), partial: bool }) },
  'sessions.list': { params: empty, data: obj({ sessions: arr(sessionRow), comment_tasks: arr(commentTask), comment_tasks_error: nullable(str) }) },
  'sessions.get': {
    params: obj({ agent: str, chat_id: str }),
    data: obj(
      { agent: str, chat_id: str, tag: str, turns: arr(turnRecord), more: bool },
      { name: str, session: sessionRow, outside: arr(outsideUsage) },
    ),
  },
  'sessions.act': {
    params: obj({ agent: str, chat_id: str, action: lit('handled', 'interrupt', 'exit', 'archive') }, { interrupt: bool, turn_id: str, reply: str }),
    data: obj({ outcome: lit('done', 'running', 'pending', 'absent', 'stale', 'not_running', 'superseded') }),
  },
  'turns.list': { params: obj({ agent: str }, { chat_id: str, schedule: str, before: num, limit: num }), data: turnsPage },
  'usage.summary': { params: obj({ range: usageRange }, { from: str, to: str, agent: str, chat_id: str }), data: usageView },
  'insights.summary': { params: obj({ range: usageRange }, { from: str, to: str, agent: str }), data: insightsSummary },
  'insights.analysis.get': { params: analysisQuery(), data: analysisView },
  'insights.analysis.start': { params: analysisQuery({ force: bool }), data: analysisView },
  'logs.tail': {
    params: obj({ file: logFile }, { before: str, limit: num, query: str, agent: str, since: str, until: str, errors: bool }),
    data: obj({ file: logFile, entries: arr(obj({ text: str, error: bool }, { at: str })) }, { before: str }),
  },
};

export const METHODS = Object.keys(METHOD_GUARDS) as Method[];

const apiError = obj(
  { code: lit('unauthenticated', 'forbidden', 'conflict', 'invalid', 'not_found', 'busy', 'unavailable', 'internal'), message: str },
  { key: str },
);

/** 校验一个完整响应体 ApiResult<T>。 */
export function checkResult(method: Method, body: unknown): string | null {
  if (!body || typeof body !== 'object' || Array.isArray(body)) return translate('dc36536');
  if ((body as { ok?: unknown }).ok === true) {
    return obj({ ok: lit(true), data: METHOD_GUARDS[method].data })(body, 'result');
  }
  return obj({ ok: lit(false), error: apiError })(body, 'result');
}
