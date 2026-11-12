//! 与 `dashboard/src/api/types.ts` 逐字段对应的接口结构。字段表、原文校验与预览的结构在
//! summon-core 里(`dashboard::{fields, raw_check, preview}`),这里只放接口层自己的。
//!
//! 可选字段一律 `skip_serializing_if`:前端的契约守卫对可选键只认「没有」或「类型对」,`null`
//! 会被当成类型错误。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use summon_core::dashboard::fields::{Effect, FieldOption, Layer};

// ── 公共 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChatKind {
    P2p,
    Group,
}

impl ChatKind {
    /// 盘上 `chat_type`(p2p / group / topic_group)→ 页面上的两类。认不出的按群算:群的判权更严。
    pub(crate) fn of(chat_type: &str) -> Self {
        if chat_type == "p2p" {
            Self::P2p
        } else {
            Self::Group
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub(crate) enum Scope {
    Global,
    Agent { agent: String },
    Chat { agent: String, chat_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConfigChange {
    pub(crate) key: String,
    /// `null` = 删掉本层的键,恢复继承。
    pub(crate) value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestartItem {
    pub(crate) key: String,
    pub(crate) file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeyEffect {
    pub(crate) key: String,
    pub(crate) effect: Effect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SaveResult {
    pub(crate) revision: String,
    pub(crate) effects: Vec<KeyEffect>,
    pub(crate) restart_pending: Vec<RestartItem>,
}

// ── 配置 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FieldValue {
    pub(crate) key: String,
    pub(crate) value: Value,
    pub(crate) effective: Value,
    pub(crate) source: Layer,
    pub(crate) overridden_by: Vec<String>,
    pub(crate) editable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deny_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConfigView {
    pub(crate) scope: Scope,
    pub(crate) revision: String,
    pub(crate) values: Vec<FieldValue>,
    /// 这一层生效的表情安全集(键 + 含义)。表情键只能从里面选,页面照它画表情盘;
    /// 群这一层没有表情键,不带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reactions: Option<Vec<FieldOption>>,
}

/// lark-cli 里登录的飞书用户,即 sentinel 认作 owner 的那个人。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LarkUserView {
    /// 在 lark-cli 那个应用里的 open_id
    pub(crate) open_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "file", rename_all = "snake_case")]
pub(crate) enum RawFile {
    Global,
    Agent { agent: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RawView {
    pub(crate) revision: String,
    pub(crate) text: String,
    pub(crate) path: String,
    pub(crate) editable: bool,
}

// ── 概览 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Credentials {
    Ready,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentSummary {
    pub(crate) alias: String,
    pub(crate) name: String,
    pub(crate) display: String,
    pub(crate) cli: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) routable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) load_error: Option<String>,
    pub(crate) credentials: Credentials,
    pub(crate) chats: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Autostart {
    On,
    Off,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DaemonInfo {
    pub(crate) version: String,
    pub(crate) commit: String,
    pub(crate) started_at: String,
    pub(crate) pid: u32,
    pub(crate) autostart: Autostart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StatusView {
    pub(crate) language: summon_core::i18n::Language,
    pub(crate) daemon: DaemonInfo,
    pub(crate) hostname: String,
    pub(crate) restart_pending: Vec<RestartItem>,
    pub(crate) agents: Vec<AgentSummary>,
    pub(crate) revisions: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IssuePage {
    Global,
    Agent,
    Chat,
    Schedules,
    /// 会话详情(带 agent 与 chat_id)。
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IssueLink {
    pub(crate) page: IssuePage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    Error,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Issue {
    pub(crate) severity: Severity,
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) fix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) link: Option<IssueLink>,
}

// ── Agents ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Member {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentDetail {
    pub(crate) summary: AgentSummary,
    pub(crate) app_id_env: String,
    pub(crate) scope: String,
    pub(crate) model_candidates: Vec<String>,
    pub(crate) effort_options: Vec<String>,
    pub(crate) members: Vec<Member>,
    pub(crate) can_toggle_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deny_reason: Option<String>,
}

// ── 群 ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatAgentBrief {
    pub(crate) agent: String,
    pub(crate) overrides: usize,
    pub(crate) live: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_active: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatSummary {
    pub(crate) chat_id: String,
    pub(crate) kind: ChatKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    pub(crate) agents: Vec<ChatAgentBrief>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_active: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkDirSource {
    ChatOverride,
    ChatMapping,
    AgentDefault,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MentionlessView {
    pub(crate) enabled: bool,
    pub(crate) can_disable: bool,
    pub(crate) can_enable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OperatorsView {
    pub(crate) members: Vec<Member>,
    pub(crate) revision: String,
    pub(crate) editable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkDirView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) effective: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<WorkDirSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mapping: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) r#override: Option<String>,
    pub(crate) live: bool,
    pub(crate) editable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DocExists {
    pub(crate) exists: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ChatAgentView {
    pub(crate) agent: String,
    pub(crate) config: ConfigView,
    pub(crate) mentionless: MentionlessView,
    pub(crate) operators: OperatorsView,
    pub(crate) work_dir: WorkDirView,
    pub(crate) doc: DocExists,
    pub(crate) schedules: Vec<ScheduleView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ChatDetail {
    pub(crate) chat_id: String,
    pub(crate) kind: ChatKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    pub(crate) agents: Vec<ChatAgentView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobStatus {
    Pending,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MentionlessJob {
    pub(crate) job: String,
    pub(crate) status: JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) apply_url: Option<String>,
}

// ── 定时任务 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScheduleView {
    pub(crate) id: String,
    pub(crate) agent: String,
    pub(crate) chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_name: Option<String>,
    pub(crate) kind: String,
    pub(crate) spec: String,
    pub(crate) human: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) next_fire: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_fired: Option<String>,
    pub(crate) prompt: String,
    pub(crate) output: String,
    pub(crate) context: String,
    pub(crate) enabled: bool,
    pub(crate) created_by: String,
    pub(crate) editable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WhenCheck {
    pub(crate) ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) human: Option<String>,
    pub(crate) next: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) forms: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScheduleEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<String>,
}

// ── 说明文件 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AgentDocFile {
    #[serde(rename = "CLAUDE.md")]
    Claude,
    #[serde(rename = "SOUL.md")]
    Soul,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub(crate) enum DocRef {
    Global,
    Agent { agent: String, file: AgentDocFile },
    Chat { agent: String, chat_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DocView {
    pub(crate) exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) revision: Option<String>,
    pub(crate) text: String,
    pub(crate) path: String,
    pub(crate) editable: bool,
    pub(crate) effect: Effect,
}

// ── 预览 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PreviewRequest {
    pub(crate) scope: Scope,
    #[serde(default)]
    pub(crate) draft: Vec<ConfigChange>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tagged_shapes_follow_the_contract() {
        let scope: Scope =
            serde_json::from_value(json!({"level": "chat", "agent": "a", "chat_id": "oc_x"}))
                .unwrap();
        assert_eq!(
            scope,
            Scope::Chat {
                agent: "a".into(),
                chat_id: "oc_x".into()
            }
        );
        assert_eq!(
            serde_json::to_value(Scope::Global).unwrap(),
            json!({"level": "global"})
        );
        let doc: DocRef =
            serde_json::from_value(json!({"level": "agent", "agent": "a", "file": "SOUL.md"}))
                .unwrap();
        assert_eq!(
            doc,
            DocRef::Agent {
                agent: "a".into(),
                file: AgentDocFile::Soul
            }
        );
        let work = WorkDirView {
            effective: None,
            source: Some(WorkDirSource::ChatOverride),
            mapping: None,
            r#override: Some("/tmp".into()),
            live: false,
            editable: true,
            reason: None,
        };
        assert_eq!(
            serde_json::to_value(work).unwrap(),
            json!({"source": "chat_override", "override": "/tmp", "live": false, "editable": true}),
            "可选字段不写 null"
        );
    }
}

// ── 会话 ────────────────────────────────────────────────────────────────

/// 给人看的会话状态,排序即处理的先后。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionState {
    /// 路由已退休、进程还活着。
    Leaked,
    /// claude 停在授权确认 / 提问 / 登录上。
    Blocked,
    /// 回复等人看。
    Waiting,
    /// 在跑一轮,或主轮收尾了、子 agent 还在跑。
    Working,
    Idle,
}

/// 这一行此刻能做的动作(已按看的人的权限算好)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionActions {
    pub(crate) handled: bool,
    pub(crate) interrupt: bool,
    pub(crate) exit: bool,
    pub(crate) archive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionRow {
    pub(crate) agent: String,
    pub(crate) agent_display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cli: Option<String>,
    pub(crate) tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    pub(crate) state: SessionState,
    /// 会话还在列表里;不在的只剩一条等人看的回复。
    pub(crate) listed: bool,
    pub(crate) busy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) blocked: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_since: Option<String>,
    /// 等人看的那条回复(「已处理」按它清,别处又回了一次就不清)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_reply: Option<String>,
    /// 跳回飞书的链接(等人看的那条回复)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_active: Option<String>,
    pub(crate) capture: String,
    pub(crate) actions: SessionActions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deny_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionsView {
    pub(crate) comment_tasks: Vec<summon_core::fleet::CommentTask>,
    pub(crate) comment_tasks_error: Option<String>,
    pub(crate) sessions: Vec<SessionRow>,
}

/// 运行记录里的一轮,带按价格表算出的 token 与费用(这一轮没报 token 时没有)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TurnView {
    #[serde(flatten)]
    pub(crate) record: summon_core::turn_ledger::TurnRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spend: Option<summon_core::pricing::Spend>,
}

/// 一段轮外用量(不在任何一轮里的 claude 请求),带按价格表算出的 token 与费用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OutsideView {
    pub(crate) from_ms: i64,
    pub(crate) to_ms: i64,
    pub(crate) usage: Vec<summon_core::turn_ledger::ModelUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spend: Option<summon_core::pricing::Spend>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TurnsPage {
    pub(crate) turns: Vec<TurnView>,
    pub(crate) more: bool,
    /// 只按会话查时有:这一页覆盖的时间段里这个会话的轮外用量,新的在前。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) outside: Vec<OutsideView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionDetail {
    pub(crate) agent: String,
    pub(crate) chat_id: String,
    pub(crate) tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    /// 会话不在列表里(已结束)时没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session: Option<SessionRow>,
    pub(crate) turns: Vec<TurnView>,
    pub(crate) more: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) outside: Vec<OutsideView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActOutcome {
    Done,
    /// 在跑一轮,页面没带确认:没有动手。
    Running,
    /// 已交给 daemon,还没看到它结束。
    Pending,
    /// 会话本来就不在,或没有等人看的回复。
    Absent,
    /// 页面看到的那一轮已经结束。
    Stale,
    /// 会话此刻没有在跑。
    NotRunning,
    /// 等人看的回复已换成更新的一条。
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActResult {
    pub(crate) outcome: ActOutcome,
}

// ── 用量 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum UsageRange {
    #[serde(rename = "today")]
    Today,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
    #[serde(rename = "90d")]
    Quarter,
    #[serde(rename = "all")]
    All,
    #[serde(rename = "custom")]
    Custom,
    #[serde(rename = "relative")]
    Relative,
}

/// 柱子按小时还是按天。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BucketUnit {
    Hour,
    Day,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UsageTotals {
    pub(crate) turns: u32,
    pub(crate) seconds: u64,
    pub(crate) failed: u32,
    pub(crate) interrupted: u32,
    /// 时间段里的 token 与费用:报了 token 的轮加上轮外用量;都没有就没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spend: Option<summon_core::pricing::Spend>,
    /// 其中轮外用量那一部分;没有就没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) outside: Option<summon_core::pricing::Spend>,
    /// 此刻在跑(含卡住)与等人回复的会话数;取不到会话状态时没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) working: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) waiting: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UsageBucket {
    /// 这个桶的起点(本地时区)。
    pub(crate) start: String,
    #[serde(flatten)]
    pub(crate) counts: summon_core::turn_usage::EndCounts,
}

/// 表里的一行:一个 agent、一个会话、一种 CLI 或一个模型。轮数与耗时只算轮,token 与费用含轮外用量。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct UsageRow {
    pub(crate) key: String,
    pub(crate) label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_id: Option<String>,
    /// 会话一行才有:这个会话的 chat tag。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tag: Option<String>,
    #[serde(flatten)]
    pub(crate) stats: summon_core::turn_usage::GroupStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spend: Option<summon_core::pricing::Spend>,
}

/// 按天的一行:这一天(本地日历)结束的轮与开始的轮外用量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UsageDay {
    /// 这一天的零点(本地时区)。
    pub(crate) start: String,
    pub(crate) turns: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spend: Option<summon_core::pricing::Spend>,
}

/// 计价用的价格表:来源与拉取时刻。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PriceSource {
    pub(crate) source: String,
    pub(crate) fetched_at: String,
}

/// 一个 agent 的额度:最近一次读数(`at` 是读到它的那一轮结束的时刻)与时间段里的曲线。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UsageQuota {
    pub(crate) agent: String,
    pub(crate) label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cli: Option<String>,
    pub(crate) at: String,
    #[serde(flatten)]
    pub(crate) latest: summon_core::turn_ledger::TurnQuota,
    pub(crate) points: Vec<summon_core::turn_usage::QuotaPoint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UsageScope {
    pub(crate) agent: String,
    pub(crate) agent_label: String,
    pub(crate) chat_id: String,
    pub(crate) chat_label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UsageView {
    pub(crate) range: UsageRange,
    pub(crate) scopes: Vec<UsageScope>,
    pub(crate) today: String,
    pub(crate) unit: BucketUnit,
    pub(crate) since: String,
    pub(crate) until: String,
    pub(crate) totals: UsageTotals,
    pub(crate) buckets: Vec<UsageBucket>,
    pub(crate) by_agent: Vec<UsageRow>,
    pub(crate) by_chat: Vec<UsageRow>,
    pub(crate) by_cli: Vec<UsageRow>,
    pub(crate) by_model: Vec<UsageRow>,
    /// 有轮结束或有轮外用量的那几天,新的在前。
    pub(crate) days: Vec<UsageDay>,
    pub(crate) quota: Vec<UsageQuota>,
    /// 盘上没有价格表时没有:费用一律未计价。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prices: Option<PriceSource>,
}

// ── 洞察 ────────────────────────────────────────────────────────────────

/// `insights.summary` 的返回,`summon insights --json` 输出同一个结构。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct InsightsView {
    pub(crate) range: UsageRange,
    /// 日历范围为 `YYYY-MM-DD`(含两端);相对范围为精确 RFC3339 时间戳(左闭右开)。
    pub(crate) from: String,
    pub(crate) to: String,
    #[serde(flatten)]
    pub(crate) insights: summon_core::insights::Insights,
}

/// 跑分析的主 agent。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AnalysisPrimary {
    pub(crate) agent: String,
    pub(crate) label: String,
    pub(crate) source: summon_core::insights_analysis::PrimarySource,
    pub(crate) cli: String,
    /// 没配模型(用 CLI 默认)时没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnalysisStatus {
    None,
    Running,
    Done,
    Failed,
    Unsupported,
}

/// 一次成功的分析。文本由模型生成,与规则结论分开展示。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AnalysisResult {
    pub(crate) text: String,
    /// 超过上限被截断过。
    pub(crate) truncated: bool,
    pub(crate) generated_at: String,
    pub(crate) agent: String,
    pub(crate) cli: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) elapsed_ms: u64,
    pub(crate) prompt_version: u32,
    /// CLI 输出里带了用量才有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<summon_core::insights_analysis::AnalysisUsage>,
    /// 生成它之后数据有更新,或主 agent / 模型 / 提示词版本变了。
    pub(crate) stale: bool,
    /// 这份结果之后最近一次重新生成没成功(下一次成功时消失)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retry_failure: Option<AnalysisRetryFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AnalysisRetryFailure {
    /// 按请求的语言。
    pub(crate) reason: String,
    pub(crate) at: String,
}

/// `insights.analysis.get` / `insights.analysis.start` 的返回。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AnalysisView {
    /// 没有可用的 agent 时没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) primary: Option<AnalysisPrimary>,
    pub(crate) status: AnalysisStatus,
    /// `done` 时有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<AnalysisResult>,
    /// `failed` / `unsupported` 时的原因,按请求的语言。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    /// `running` 时:开始的时刻。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) started_at: Option<String>,
    /// 当前调用者有整机设置的授权、且主 agent 支持一次性运行。
    pub(crate) can_start: bool,
}

// ── 日志 ────────────────────────────────────────────────────────────────

/// 日志里的一条(可以跨多行),已脱敏。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LogLine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) at: Option<String>,
    pub(crate) text: String,
    /// 含 error / panic / fail 或「失败」「错误」。
    pub(crate) error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LogsPage {
    pub(crate) file: summon_core::log_view::LogFile,
    pub(crate) entries: Vec<LogLine>,
    /// 更早一页的翻页位置;往前没有了就没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) before: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AttentionItem {
    pub(crate) id: String,
    pub(crate) agent: String,
    pub(crate) agent_display: String,
    pub(crate) chat_name: String,
    pub(crate) source: String,
    pub(crate) summary: String,
    pub(crate) question_count: usize,
    pub(crate) registered_at: String,
    pub(crate) waited_secs: u64,
    pub(crate) link: String,
    pub(crate) link_target: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AttentionView {
    pub(crate) items: Vec<AttentionItem>,
    pub(crate) observed_at: String,
    pub(crate) coverage: String,
    pub(crate) partial: bool,
}
