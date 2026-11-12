//! 外部 Claude 会话桥接的纯领域模型。
//!
//! 本模块只负责可持久化 schema、状态迁移和 CAS 判定；不读文件、不碰进程、不生成随机数、
//! 不发送飞书消息。IO 层必须把时间、opaque id、进程探测结果和本地群占用结果作为参数传入。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

pub const BRIDGE_SCHEMA_VERSION: u32 = 1;

/// 外部 session 的全局唯一键。SID 只在一个 Claude config root 内唯一，不能单独作键。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub config_dir: String,
    pub session_id: String,
}

impl SessionKey {
    pub fn new(config_dir: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            config_dir: config_dir.into(),
            session_id: session_id.into(),
        }
    }

    /// JSON map 使用字符串键。长度前缀避免 `a|bc` 与 `ab|c` 一类拼接歧义。
    pub fn storage_key(&self) -> String {
        format!(
            "{}:{}{}:{}",
            self.config_dir.len(),
            self.config_dir,
            self.session_id.len(),
            self.session_id
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessFingerprint {
    pub pid: u32,
    /// 平台进程启动时间的稳定文本/数值表示；与 PID 一起防 PID reuse。
    pub start_time: String,
    pub executable: String,
}

/// Claude 配置根的来源。Takeover 只支持未显式设置 `CLAUDE_CONFIG_DIR` 的标准配置；
/// `config_dir` 仍用于 Mirror 的 SessionKey 与 transcript 校验，不能据有效路径反推来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeConfigSource {
    #[default]
    Unknown,
    Default,
    Explicit,
}

/// 只记录 provider 语义，不保存 endpoint、token 或其它凭据值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeProvider {
    #[default]
    Unknown,
    Default,
    Custom,
}

impl ClaudeProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => summon_i18n::text!("r9682acd8cc"),
            Self::Default => summon_i18n::text!("r479b06ae73"),
            Self::Custom => summon_i18n::text!("r1a015e5614"),
        }
    }
}

/// Hook/目标 agent 共用的 provider 分类。这里只看非空键是否存在，返回枚举后立即丢弃值。
pub const CLAUDE_PROVIDER_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

pub fn classify_claude_provider<'a>(
    env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> ClaudeProvider {
    if env
        .into_iter()
        .any(|(key, value)| !value.is_empty() && CLAUDE_PROVIDER_ENV_KEYS.contains(&key))
    {
        ClaudeProvider::Custom
    } else {
        ClaudeProvider::Default
    }
}

impl ProcessFingerprint {
    pub fn is_complete(&self) -> bool {
        self.pid > 0 && !self.start_time.is_empty() && !self.executable.is_empty()
    }
}

/// 从源 Claude 进程提取的恢复身份 profile。
///
/// `safe_args` / `unsupported_args` 只保留脱敏后的启动形态观测，不能成为恢复命令来源。
/// Takeover 会用目标 Bot 的受控 profile 启动新进程，并以 exact ACK 复验；因此 cmux 等终端
/// wrapper 注入的 source-only 参数不影响接管资格，也不会被继承或落盘原值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResumeProfile {
    pub claude_bin: String,
    pub home: String,
    pub config_dir: String,
    pub version: String,
    #[serde(default)]
    pub safe_args: Vec<String>,
    #[serde(default)]
    pub unsupported_args: Vec<String>,
    #[serde(default)]
    pub process_verified: bool,
    #[serde(default)]
    pub environment_verified: bool,
    #[serde(default)]
    pub config_source: ClaudeConfigSource,
    #[serde(default)]
    pub provider: ClaudeProvider,
}

impl ResumeProfile {
    pub fn handoff_eligibility(&self, process: Option<&ProcessFingerprint>) -> HandoffEligibility {
        if !self.process_verified {
            return HandoffEligibility::MirrorOnly(HandoffBlock::UnverifiedProcess);
        }
        if !self.environment_verified {
            return HandoffEligibility::MirrorOnly(HandoffBlock::UnverifiedEnvironment);
        }
        match self.config_source {
            ClaudeConfigSource::Default => {}
            ClaudeConfigSource::Explicit => {
                return HandoffEligibility::MirrorOnly(HandoffBlock::CustomConfig)
            }
            ClaudeConfigSource::Unknown => {
                return HandoffEligibility::MirrorOnly(HandoffBlock::UnverifiedEnvironment)
            }
        }
        if self.provider == ClaudeProvider::Unknown {
            return HandoffEligibility::MirrorOnly(HandoffBlock::UnverifiedProvider);
        }
        if process.is_none_or(|p| !p.is_complete()) {
            return HandoffEligibility::MirrorOnly(HandoffBlock::MissingProcessFingerprint);
        }
        if self.claude_bin.is_empty() || self.home.is_empty() || self.config_dir.is_empty() {
            return HandoffEligibility::MirrorOnly(HandoffBlock::IncompleteProfile);
        }
        if process.is_some_and(|p| p.executable != self.claude_bin) {
            return HandoffEligibility::MirrorOnly(HandoffBlock::ExecutableMismatch);
        }
        HandoffEligibility::Eligible
    }
}

/// 严格 resume(handoff takeover / recall 全托管)对被恢复进程 launch 身份的期望值。
///
/// 只承载可安全落日志的路径与枚举——**token/密钥类字段永不进这里**,所以比对结果可以直接写
/// daemon.log。ACK 的反陈旧 token 校验是另一条独立检查,不经本结构。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedResumeIdentity<'a> {
    pub claude_bin: &'a str,
    pub home: &'a str,
    pub config_dir: &'a str,
    pub provider: ClaudeProvider,
}

/// 一条 launch 身份不匹配。`field` 用 `ResumeProfile` 的真实字段名,日志与测试都据它点名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeIdentityMismatch {
    pub field: &'static str,
    pub expected: String,
    pub actual: String,
}

impl std::fmt::Display for ResumeIdentityMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}",
            summon_i18n::format!("ra9b64c725f", self.field, self.expected, self.actual)
        )
    }
}

/// 单条诊断值的长度上限。这些值都是路径/枚举,正常远小于它;设上限只为防止畸形 envelope
/// 把 daemon.log 撑爆(截断只影响可读性,不影响 fail-closed 判定)。
const MAX_IDENTITY_VALUE_CHARS: usize = 200;

fn bounded_identity_value(value: &str) -> String {
    let mut out: String = value.chars().take(MAX_IDENTITY_VALUE_CHARS).collect();
    if out.chars().count() < value.chars().count() {
        out.push('…');
    }
    out
}

/// 逐项比对 ACK 报上来的 `ResumeProfile` 与期望的 launch 身份,返回**全部**不匹配项。
///
/// 语义与它取代的那个多条件 `if` 完全一致:空 = 通过,非空 = 拒(调用方必须 fail-closed)。
/// 差别只在于「为什么拒」现在点名到字段 + 期望值/实际值,不再压成一句「不一致」。
///
/// `config_source` 必须是 `Default`、两个 verified 必须为真——这三条**写死在这里**、不给调用方
/// 参数化的口子,免得将来某处顺手把 fail-closed 放松掉。
pub fn diff_resume_identity(
    profile: &ResumeProfile,
    expected: &ExpectedResumeIdentity<'_>,
) -> Vec<ResumeIdentityMismatch> {
    let mut mismatches = Vec::new();
    let mut text = |field: &'static str, expected: &str, actual: &str| {
        if expected != actual {
            mismatches.push(ResumeIdentityMismatch {
                field,
                expected: bounded_identity_value(expected),
                actual: bounded_identity_value(actual),
            });
        }
    };
    text("claude_bin", expected.claude_bin, &profile.claude_bin);
    text("home", expected.home, &profile.home);
    text("config_dir", expected.config_dir, &profile.config_dir);
    if profile.config_source != ClaudeConfigSource::Default {
        mismatches.push(ResumeIdentityMismatch {
            field: "config_source",
            expected: format!("{:?}", ClaudeConfigSource::Default),
            actual: format!("{:?}", profile.config_source),
        });
    }
    if profile.provider != expected.provider {
        mismatches.push(ResumeIdentityMismatch {
            field: "provider",
            expected: format!("{:?}", expected.provider),
            actual: format!("{:?}", profile.provider),
        });
    }
    for (field, observed) in [
        ("process_verified", profile.process_verified),
        ("environment_verified", profile.environment_verified),
    ] {
        if !observed {
            mismatches.push(ResumeIdentityMismatch {
                field,
                expected: "true".into(),
                actual: "false".into(),
            });
        }
    }
    mismatches
}

/// 把不匹配项渲染成单行日志文案(逗号分隔)。空列表返回空串。
pub fn describe_resume_identity_mismatches(mismatches: &[ResumeIdentityMismatch]) -> String {
    mismatches
        .iter()
        .map(ResumeIdentityMismatch::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffEligibility {
    Eligible,
    MirrorOnly(HandoffBlock),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffBlock {
    UnverifiedProcess,
    MissingProcessFingerprint,
    UnverifiedEnvironment,
    UnverifiedProvider,
    CustomConfig,
    IncompleteProfile,
    ExecutableMismatch,
    SessionBusy,
    SessionEnded,
    SessionConflict,
    LocatorInvalid,
    TargetIncompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalLocator {
    pub key: SessionKey,
    /// Hook 直接报告的权威路径，不能从 cwd 反推。
    pub transcript_path: String,
    /// 最近一次真实 SessionStart 的 cwd，用于 `claude --resume SID` 查找。
    pub resume_lookup_cwd: String,
    /// CwdChanged 后的当前目录，只用于当前定位/展示。
    pub current_cwd: String,
    pub profile: ResumeProfile,
    pub revision: u64,
}

impl ExternalLocator {
    pub fn paths_are_absolute(&self) -> bool {
        [
            self.transcript_path.as_str(),
            self.resume_lookup_cwd.as_str(),
            self.current_cwd.as_str(),
            self.profile.claude_bin.as_str(),
            self.profile.home.as_str(),
            self.profile.config_dir.as_str(),
        ]
        .iter()
        .all(|p| p.starts_with('/'))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalLifecycle {
    Unverified,
    ActiveIdle,
    ActiveBusy,
    Ended,
    Conflict,
}

impl ExternalLifecycle {
    /// 卡片上给人看的标签。**别用 `{:?}`** —— 那会把 `ActiveIdle` 这种 Rust 枚举名原样甩到
    /// 飞书卡片上,读的人得先懂 Rust 才知道自己的会话是什么状态。
    pub fn label(self) -> &'static str {
        match self {
            ExternalLifecycle::Unverified => summon_i18n::text!("r555ee27e0a"),
            ExternalLifecycle::ActiveIdle => summon_i18n::text!("rdae661d17c"),
            ExternalLifecycle::ActiveBusy => summon_i18n::text!("rb03ebaa09c"),
            ExternalLifecycle::Ended => summon_i18n::text!("rd79b1d0e5c"),
            ExternalLifecycle::Conflict => summon_i18n::text!("r45fb956103"),
        }
    }
}

/// 「最近活动」的人读串。卡片此前直接打 `last_seen_ms`,于是飞书上出现一串
/// `1784044645534` —— 那是 unix 毫秒,没人读得出它是几分钟前。
///
/// 时钟回拨(last_seen 在未来)按「刚刚」处理,不谎报成一个巨大的负数时长。
pub fn humanize_last_seen(last_seen_ms: u64, now_ms: u64) -> String {
    let elapsed_secs = now_ms.saturating_sub(last_seen_ms) / 1_000;
    crate::session::session_view::humanize_elapsed_secs(elapsed_secs)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingExternalPrompt {
    pub event_id: String,
    /// 防止 SID 复用后的新 generation 被旧 binding 的清理误删。
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub prompt_id: Option<String>,
    pub text: String,
    pub observed_at_ms: u64,
    #[serde(default)]
    pub content_permit: Option<ContentPermit>,
}

/// Bridge state 只暂存最终卡片真正可能展示的正文量。预算按 JSON string 编码后的字节计算，
/// 因而换行、控制字符等转义膨胀也受控；上下文胶囊仍独立从 transcript 有界读取。
pub const EXTERNAL_PROMPT_STATE_JSON_BUDGET: usize = 6_500;
pub const EXTERNAL_ASSISTANT_STATE_JSON_BUDGET: usize = 15_500;
const EXTERNAL_BODY_TRUNCATED_NOTICE: &str = "rc8c6a8cc07";

pub fn clip_external_body_for_state(text: &str, json_budget: usize) -> String {
    let encoded_len = |candidate: &str| {
        serde_json::to_string(candidate).map_or(usize::MAX, |encoded| encoded.len())
    };
    if encoded_len(text) <= json_budget {
        return text.to_string();
    }

    let boundaries = text
        .char_indices()
        .map(|(index, character)| index + character.len_utf8())
        .collect::<Vec<_>>();
    let mut low = 0usize;
    let mut high = boundaries.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let candidate = format!(
            "{}{}",
            &text[..boundaries[middle]],
            ::summon_core::i18n::label(EXTERNAL_BODY_TRUNCATED_NOTICE)
        );
        if encoded_len(&candidate) <= json_budget {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    let end = low
        .checked_sub(1)
        .and_then(|index| boundaries.get(index).copied())
        .unwrap_or(0);
    let clipped = format!(
        "{}{}",
        &text[..end],
        ::summon_core::i18n::label(EXTERNAL_BODY_TRUNCATED_NOTICE)
    );
    if encoded_len(&clipped) <= json_budget {
        clipped
    } else {
        String::new()
    }
}

/// Hook 采样正文时取得的绑定代际证明。正文归约前必须再与当前 slot 精确比对；单纯的
/// `session_accepts_content=true` 无法阻止旧 binding 的 spool 在解绑/重绑后串到新群。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentPermit {
    pub binding_id: String,
    pub binding_revision: u64,
    /// SessionKey 可跨 `claude --resume` 进程续用；permit 仍必须绑定签发时的进程 epoch。
    /// 旧 state 缺字段按 0 解析并 fail-closed，不能自动提升到当前 generation。
    #[serde(default)]
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalVisibleRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalVisibleMessage {
    pub role: ExternalVisibleRole,
    pub text: String,
}

/// Stop/StopFailure 已完成、等待 T5 写入网络 outbox 的本地 durable turn。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMirrorTurn {
    pub id: String,
    pub session: SessionKey,
    pub generation: u64,
    pub binding_id: String,
    pub binding_revision: u64,
    pub target: ScopedChatKey,
    #[serde(default = "default_group_chat_type")]
    pub target_chat_type: String,
    pub observed_at_ms: u64,
    /// Hook spool 的全局单调序列；同一 binding 的 outbox 必须按此顺序投递。
    #[serde(default)]
    pub sequence: u64,
    /// 恒为 `[user, assistant]`；不用两个可选字段，避免下游换序或只投一半。
    pub messages: Vec<ExternalVisibleMessage>,
}

fn default_group_chat_type() -> String {
    "group".to_string()
}

/// 群审计消息已经被飞书确认接收的持久化证明。
///
/// receipt 只保存路由/代际元数据，不保存任何外部会话正文。Mirror 的最终 CAS 必须精确匹配
/// 本证明，不能仅凭 `Activating` slot 直接进入 `MirrorBound`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationAuditReceipt {
    pub id: String,
    pub outbox_id: String,
    pub ticket_id: String,
    pub binding_id: String,
    pub binding_revision: u64,
    pub target: ScopedChatKey,
    pub session: SessionKey,
    pub generation: u64,
    pub sequence: u64,
    pub delivered_at_ms: u64,
}

/// pending turn 已经原子导出到 durable outbox 的轻量证明。
///
/// 正文只存在 outbox；state 在写入本 receipt 的同一次事务中删除 pending body。网络消费者必须
/// 先精确验证 receipt，防止“outbox 已写、state 尚未提交”的 crash window 提前泄露正文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorExportReceipt {
    pub id: String,
    pub outbox_id: String,
    pub turn_id: String,
    pub binding_id: String,
    pub binding_revision: u64,
    pub target: ScopedChatKey,
    pub session: SessionKey,
    pub generation: u64,
    pub sequence: u64,
    #[serde(default)]
    pub delivered_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorExportOutcome {
    Exported,
    AlreadyExported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextExportReceipt {
    pub id: String,
    pub outbox_id: String,
    pub ticket_id: String,
    pub choice_id: String,
    pub requester_open_id: String,
    pub binding_id: String,
    pub binding_revision: u64,
    pub target: ScopedChatKey,
    #[serde(default = "default_group_chat_type")]
    pub target_chat_type: String,
    pub session: SessionKey,
    pub generation: u64,
    pub sequence: u64,
    #[serde(default)]
    pub delivered_at_ms: Option<u64>,
}

/// Hook IO schema 到纯领域归约器之间的无副作用观察值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalHookObservation {
    pub event_id: String,
    /// BridgeStore 在单一 flock 下分配的全局单调序列；同毫秒事件仍有确定顺序。
    pub sequence: u64,
    pub observed_at_ms: u64,
    pub session: SessionKey,
    pub transcript_path: String,
    pub cwd: String,
    pub process: Option<ProcessFingerprint>,
    pub resume_profile: ResumeProfile,
    pub content_permit: Option<ContentPermit>,
    pub managed_takeover: bool,
    pub event: ExternalHookEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalHookEvent {
    SessionStart {
        source: String,
    },
    CwdChanged {
        old_cwd: Option<String>,
        new_cwd: String,
    },
    UserPromptSubmit {
        prompt_id: Option<String>,
        prompt: Option<String>,
    },
    Stop {
        prompt_id: Option<String>,
        last_assistant_message: Option<String>,
        /// 另一个 Stop hook 已让 Claude 继续；若首个候选回答已投递，本次作为同 prompt 的更新。
        stop_hook_active: bool,
        /// Claude 仍在等待 background task / session cron 唤醒，本次不是可导出的终态。
        has_pending_wakeups: bool,
    },
    StopFailure {
        prompt_id: Option<String>,
        error: Option<String>,
        error_details: Option<String>,
        last_assistant_message: Option<String>,
    },
    SessionEnd {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalHookApply {
    Applied,
    Duplicate,
    Stale,
    Conflict,
}

const INTERRUPTED_TERMINAL_MESSAGE: &str = "r880ed4137f";
const SESSION_ENDED_TERMINAL_MESSAGE: &str = "rf700d02222";

#[derive(Debug, Clone, Copy)]
struct StopSemantics {
    hook_active: bool,
    pending_wakeups: bool,
    retain_for_continuation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSessionRecord {
    pub key: SessionKey,
    pub generation: u64,
    pub locator: ExternalLocator,
    pub process: Option<ProcessFingerprint>,
    pub lifecycle: ExternalLifecycle,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    #[serde(default)]
    pub last_applied_sequence: u64,
    #[serde(default)]
    pub pending_prompt: Option<PendingExternalPrompt>,
}

impl ExternalSessionRecord {
    pub fn handoff_eligibility(&self) -> HandoffEligibility {
        match self.lifecycle {
            ExternalLifecycle::ActiveIdle
            | ExternalLifecycle::ActiveBusy
            | ExternalLifecycle::Ended => {}
            ExternalLifecycle::Conflict => {
                return HandoffEligibility::MirrorOnly(HandoffBlock::SessionConflict)
            }
            ExternalLifecycle::Unverified => {
                return HandoffEligibility::MirrorOnly(HandoffBlock::UnverifiedProcess)
            }
        }
        if !self.locator.paths_are_absolute()
            || self.locator.key != self.key
            || self.locator.profile.config_dir != self.key.config_dir
        {
            return HandoffEligibility::MirrorOnly(HandoffBlock::LocatorInvalid);
        }
        self.locator
            .profile
            .handoff_eligibility(self.process.as_ref())
    }

    pub fn safe_candidate(&self, choice_id: String) -> SafeBridgeCandidate {
        let basename = self
            .locator
            .current_cwd
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .map(sanitize_project_name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| summon_i18n::text!("re89982ec33").to_string());
        SafeBridgeCandidate {
            choice_id,
            project_name: basename,
            lifecycle: self.lifecycle,
            handoff: self.handoff_eligibility(),
            source_provider: self.locator.profile.provider,
            last_seen_ms: self.last_seen_ms,
        }
    }
}

fn sanitize_project_name(raw: &str) -> String {
    const MAX_CHARS: usize = 80;
    const MAX_BYTES: usize = 240;
    let mut output = String::new();
    let mut previous_space = false;
    for character in raw.chars() {
        let deceptive = character.is_control()
            || matches!(
                character,
                '\u{200B}'..='\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{FEFF}'
                    | '`'
                    | '*'
                    | '_'
                    | '~'
                    | '#'
                    | '['
                    | ']'
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '!'
                    | '|'
                    | '\\'
            );
        if deceptive {
            continue;
        }
        let character = if character.is_whitespace() {
            if previous_space {
                continue;
            }
            previous_space = true;
            ' '
        } else {
            previous_space = false;
            character
        };
        if output.chars().count() >= MAX_CHARS
            || output.len().saturating_add(character.len_utf8()) > MAX_BYTES
        {
            break;
        }
        output.push(character);
    }
    output.trim().to_string()
}

fn normalize_short_code(raw: &str) -> Result<String, BridgeError> {
    let code = raw.trim().to_ascii_uppercase();
    if code.is_empty()
        || code.len() > 128
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(BridgeError::InvalidInput);
    }
    Ok(code)
}

/// 可直接进入私聊卡片的候选 DTO；刻意没有 SID、config/cwd/transcript/executable。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeBridgeCandidate {
    pub choice_id: String,
    pub project_name: String,
    pub lifecycle: ExternalLifecycle,
    pub handoff: HandoffEligibility,
    pub source_provider: ClaudeProvider,
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeMode {
    Mirror,
    Handoff,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeChoice {
    pub id: String,
    /// 服务端私有；卡片 renderer 只能消费 [`SafeBridgeCandidate`]。
    pub session: SessionKey,
    pub registry_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ScopedChatKey {
    pub scope: String,
    pub agent: String,
    pub chat_id: String,
}

impl ScopedChatKey {
    pub fn new(
        scope: impl Into<String>,
        agent: impl Into<String>,
        chat_id: impl Into<String>,
    ) -> Self {
        Self {
            scope: scope.into(),
            agent: agent.into(),
            chat_id: chat_id.into(),
        }
    }

    pub fn storage_key(&self) -> String {
        format!(
            "{}:{}{}:{}{}:{}",
            self.scope.len(),
            self.scope,
            self.agent.len(),
            self.agent,
            self.chat_id.len(),
            self.chat_id
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TicketStatus {
    Pending,
    Activating {
        binding_id: String,
        choice_id: String,
        mode: BridgeMode,
    },
    Committed {
        binding_id: String,
        choice_id: String,
        mode: BridgeMode,
    },
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeTicket {
    pub id: String,
    pub short_code: Option<String>,
    pub target: ScopedChatKey,
    pub target_chat_type: String,
    #[serde(default)]
    pub target_provider: ClaudeProvider,
    pub requester_open_id: String,
    pub requester_union_id: String,
    pub control_card_message_id: Option<String>,
    pub private_chat_id: Option<String>,
    pub slot_revision: u64,
    pub choices: Vec<BridgeChoice>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub status: TicketStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatBinding {
    pub id: String,
    pub target: ScopedChatKey,
    pub target_chat_type: String,
    pub session: SessionKey,
    pub mode: BridgeMode,
    pub actor_open_id: String,
    pub created_at_ms: u64,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ChatSlot {
    Pending {
        ticket_id: String,
        revision: u64,
        expires_at_ms: u64,
    },
    Activating(ChatBinding),
    MirrorBound(ChatBinding),
    HandoffPending(ChatBinding),
    ExternalOwned(ChatBinding),
    /// 群已解散或 bot 被移除。保留 binding + lease，等后续 coordinator 只清理由 summon
    /// 创建的 staging/owned 资源；绝不能因群事件误杀用户原始 tmux，也不能提前让第二群抢 SID。
    Closing {
        binding: ChatBinding,
        reason: BridgeCloseReason,
    },
}

impl ChatSlot {
    pub fn binding(&self) -> Option<&ChatBinding> {
        match self {
            Self::Pending { .. } => None,
            Self::Activating(b)
            | Self::MirrorBound(b)
            | Self::HandoffPending(b)
            | Self::ExternalOwned(b) => Some(b),
            Self::Closing { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeCloseReason {
    BotRemoved,
    ChatDisbanded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeChatGoneAction {
    None,
    PendingCancelled {
        ticket_id: String,
    },
    MirrorStopped(ChatBinding),
    /// Handoff/遗留 ExternalOwned 只暂停并保留 lease，实际资源清理由 coordinator 显式确认。
    BindingPaused(ChatBinding),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLease {
    pub binding_id: String,
    pub target: ScopedChatKey,
}

/// Takeover 成功后的轻量所有权哨兵。运行本身已经回归普通 chat/tmux；本记录只保留 SID 与
/// resumed process，用于发现同 SID 被其它终端再次 resume 的双 writer 冲突。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedTakeover {
    pub binding_id: String,
    pub binding_revision: u64,
    pub target: ScopedChatKey,
    /// Takeover rename 后的 canonical tmux 名。它是轻量哨兵释放时唯一允许核对的本地所有权键；
    /// 不能在清理阶段按当前配置重新猜名字，否则配置漂移会误判其它会话。
    #[serde(default)]
    pub target_session: String,
    pub session: SessionKey,
    pub generation: u64,
    pub process: ProcessFingerprint,
    pub session_start_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeState {
    pub schema_version: u32,
    pub revision: u64,
    #[serde(default)]
    pub sessions: BTreeMap<String, ExternalSessionRecord>,
    #[serde(default)]
    pub tickets: BTreeMap<String, BridgeTicket>,
    /// 大小写无关的一次性短码 → ticket id。短码只做检索，授权仍绑定 requester/scope/operate。
    #[serde(default)]
    pub short_code_index: BTreeMap<String, String>,
    #[serde(default)]
    pub slots: BTreeMap<String, ChatSlot>,
    #[serde(default)]
    pub session_leases: BTreeMap<String, SessionLease>,
    #[serde(default)]
    pub managed_takeovers: BTreeMap<String, ManagedTakeover>,
    /// state commit 成功、event archive 尚未完成时的 crash window 去重证据。
    #[serde(default)]
    pub processed_events: BTreeMap<String, u64>,
    #[serde(default)]
    pub pending_mirror_turns: BTreeMap<String, PendingMirrorTurn>,
    /// 最近一次 Stop 已投递的 bounded prompt，仅用于 `stop_hook_active=true` 的后续修正；
    /// 新 prompt、解绑、generation rollover 或 SessionEnd 都会清除。
    #[serde(default)]
    pub continuation_prompts: BTreeMap<String, PendingExternalPrompt>,
    /// binding id → 群审计投递证明；不含会话正文。
    #[serde(default)]
    pub activation_audit_receipts: BTreeMap<String, ActivationAuditReceipt>,
    /// receipt id → durable outbox 导出证明；不含会话正文。
    #[serde(default)]
    pub mirror_export_receipts: BTreeMap<String, MirrorExportReceipt>,
    #[serde(default)]
    pub context_export_receipts: BTreeMap<String, ContextExportReceipt>,
    /// 每个 binding 的网络投递单调序列游标。audit 必为首项；turn/context 共用同一游标，
    /// planner 才能让 context 卡永不越过更早 turn 的 backoff。
    #[serde(default)]
    pub delivery_sequence_cursors: BTreeMap<String, u64>,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            schema_version: BRIDGE_SCHEMA_VERSION,
            revision: 0,
            sessions: BTreeMap::new(),
            tickets: BTreeMap::new(),
            short_code_index: BTreeMap::new(),
            slots: BTreeMap::new(),
            session_leases: BTreeMap::new(),
            managed_takeovers: BTreeMap::new(),
            processed_events: BTreeMap::new(),
            pending_mirror_turns: BTreeMap::new(),
            continuation_prompts: BTreeMap::new(),
            activation_audit_receipts: BTreeMap::new(),
            mirror_export_receipts: BTreeMap::new(),
            context_export_receipts: BTreeMap::new(),
            delivery_sequence_cursors: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketRequest {
    pub ticket_id: String,
    pub short_code: Option<String>,
    pub target: ScopedChatKey,
    pub target_chat_type: String,
    pub target_provider: ClaudeProvider,
    pub requester_open_id: String,
    pub requester_union_id: String,
    pub choices: Vec<BridgeChoice>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginChoiceOutcome {
    Started(ChatBinding),
    AlreadyStarted(ChatBinding),
}

/// IO 层在同一次卡片操作中完成 source registry + exact process 复验后提交的快照。
/// 专用 Handoff API 仍会逐字段与当前 durable record 对账，不能用它绕过 revision/generation CAS。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffBeginProof {
    pub session: SessionKey,
    pub generation: u64,
    pub registry_revision: u64,
    pub process: ProcessFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleApply {
    Applied,
    StaleGeneration,
    StaleProcess,
    Missing,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BridgeError {
    #[error("{}", summon_i18n::text!("rb55b5c1090"))]
    ChatOccupied,
    #[error("{}", summon_i18n::text!("r14f6ebcbc4"))]
    TicketExists,
    #[error("{}", summon_i18n::text!("rb59e2659bc"))]
    TicketMissing,
    #[error("{}", summon_i18n::text!("rd48a0480ee"))]
    TicketExpired,
    #[error("{}", summon_i18n::text!("r174a014a15"))]
    TicketFinal,
    #[error("{}", summon_i18n::text!("r97ccff0514"))]
    ShortCodeInvalid,
    #[error("{}", summon_i18n::text!("rc2b95c9cc4"))]
    WrongActor,
    #[error("{}", summon_i18n::text!("r15227646e5"))]
    WrongCard,
    #[error("{}", summon_i18n::text!("r3efb089fe9"))]
    ChoiceMissing,
    #[error("{}", summon_i18n::text!("r0272ea6a69"))]
    StaleSession,
    #[error("{}", summon_i18n::text!("r05beb403c5"))]
    StaleSlot,
    #[error("{}", summon_i18n::text!("rd124218421"))]
    SessionLeased,
    #[error("{}", summon_i18n::text!("re1ef4aa332"))]
    SessionUnavailable,
    #[error("{}", summon_i18n::text!("r121ac4adbe"))]
    HandoffIneligible,
    #[error("{}", summon_i18n::text!("r0f0947855f"))]
    HandoffCapabilityRequired,
    #[error("{}", summon_i18n::text!("re6b2f08f91"))]
    HandoffProofRequired,
    #[error("{}", summon_i18n::text!("rc3e8df8376"))]
    ActivationAuditRequired,
    #[error("{}", summon_i18n::text!("r83bcc6df2a"))]
    ActivationAuditMismatch,
    #[error("{}", summon_i18n::text!("rc40403145e"))]
    MirrorTurnMissing,
    #[error("{}", summon_i18n::text!("r572ddbdc68"))]
    MirrorExportMismatch,
    #[error("{}", summon_i18n::text!("r67b097c8d4"))]
    PendingMirrorTurns,
    #[error("{}", summon_i18n::text!("reef7fd099b"))]
    ContextExportMismatch,
    #[error("{}", summon_i18n::text!("ra191f1e0b0"))]
    BindingMismatch,
    #[error("{}", summon_i18n::text!("re5bb90a8c6"))]
    InvalidInput,
    #[error("{}", summon_i18n::text!("rf13a7c8721"))]
    InvalidObservation,
}

impl BridgeState {
    pub fn session(&self, key: &SessionKey) -> Option<&ExternalSessionRecord> {
        self.sessions.get(&key.storage_key())
    }

    pub fn session_is_referenced(&self, key: &SessionKey) -> bool {
        let storage_key = key.storage_key();
        self.session_leases.contains_key(&storage_key)
            || self
                .managed_takeovers
                .values()
                .any(|takeover| takeover.session == *key)
            || self
                .tickets
                .values()
                .filter(|ticket| match &ticket.status {
                    TicketStatus::Pending | TicketStatus::Activating { .. } => true,
                    TicketStatus::Committed { binding_id, .. } => self
                        .slot(&ticket.target)
                        .and_then(ChatSlot::binding)
                        .is_some_and(|binding| binding.id == *binding_id),
                    TicketStatus::Cancelled | TicketStatus::Expired => false,
                })
                .any(|ticket| ticket.choices.iter().any(|choice| choice.session == *key))
            || self
                .slots
                .values()
                .filter_map(ChatSlot::binding)
                .any(|binding| binding.session == *key)
            || self
                .pending_mirror_turns
                .values()
                .any(|turn| turn.session == *key)
            || self
                .activation_audit_receipts
                .values()
                .any(|receipt| receipt.session == *key)
            || self
                .mirror_export_receipts
                .values()
                .any(|receipt| receipt.session == *key)
            || self
                .context_export_receipts
                .values()
                .any(|receipt| receipt.session == *key)
    }

    pub fn remove_unreferenced_session(
        &mut self,
        key: &SessionKey,
        generation: u64,
    ) -> Result<bool, BridgeError> {
        match self.session(key) {
            Some(record) if record.generation == generation => {}
            Some(_) => return Ok(false),
            None => return Ok(false),
        }
        if self.session_is_referenced(key) {
            return Err(BridgeError::SessionLeased);
        }
        self.sessions.remove(&key.storage_key());
        self.continuation_prompts.remove(&key.storage_key());
        self.prune_terminal_tickets();
        self.bump();
        Ok(true)
    }

    pub fn upsert_session(&mut self, record: ExternalSessionRecord) {
        self.sessions.insert(record.key.storage_key(), record);
        self.bump();
    }

    /// 把一个不可变 Hook 观察值归约到 durable registry。
    ///
    /// event id 在 state commit 内登记，所以「state 已写、event 还没搬 archive」的崩溃窗口重放
    /// 不会重复产生 mirror turn。正文同时绑定 permit 与当前 slot 的 SID/id/revision/终态。
    pub fn apply_external_hook(
        &mut self,
        observation: &ExternalHookObservation,
    ) -> Result<ExternalHookApply, BridgeError> {
        self.validate_observation(observation)?;
        if self.processed_events.contains_key(&observation.event_id) {
            return Ok(ExternalHookApply::Duplicate);
        }

        let next_revision = self.revision.saturating_add(1);
        if self
            .session(&observation.session)
            .is_some_and(|record| observation.sequence <= record.last_applied_sequence)
        {
            self.remember_processed(observation);
            self.revision = next_revision;
            return Ok(ExternalHookApply::Stale);
        }
        let outcome = match &observation.event {
            ExternalHookEvent::SessionStart { .. } => {
                self.apply_session_start(observation, next_revision)
            }
            ExternalHookEvent::CwdChanged { new_cwd, .. } => {
                self.apply_session_update(observation, next_revision, |record| {
                    record.locator.current_cwd = new_cwd.clone();
                })
            }
            ExternalHookEvent::UserPromptSubmit { prompt_id, prompt } => {
                self.apply_user_prompt(observation, prompt_id, prompt, next_revision)
            }
            ExternalHookEvent::Stop {
                prompt_id,
                last_assistant_message,
                stop_hook_active,
                has_pending_wakeups,
            } => self.apply_stop(
                observation,
                prompt_id,
                last_assistant_message.as_deref(),
                StopSemantics {
                    hook_active: *stop_hook_active,
                    pending_wakeups: *has_pending_wakeups,
                    retain_for_continuation: true,
                },
                next_revision,
            ),
            ExternalHookEvent::StopFailure { prompt_id, .. } => self.apply_stop(
                observation,
                prompt_id,
                None,
                StopSemantics {
                    hook_active: false,
                    pending_wakeups: false,
                    retain_for_continuation: false,
                },
                next_revision,
            ),
            ExternalHookEvent::SessionEnd { .. } => {
                self.apply_session_end_observation(observation, next_revision)
            }
        };

        if observation.managed_takeover && outcome == ExternalHookApply::Applied {
            let resumed = self.session(&observation.session).and_then(|record| {
                record
                    .process
                    .clone()
                    .map(|process| (record.generation, process))
            });
            if let Some((generation, process)) = resumed {
                if let Some(takeover) = self
                    .managed_takeovers
                    .values_mut()
                    .find(|takeover| takeover.session == observation.session)
                {
                    takeover.generation = generation;
                    takeover.process = process;
                    takeover.session_start_event_id = observation.event_id.clone();
                }
            }
        }

        self.remember_processed(observation);
        self.revision = next_revision;
        Ok(outcome)
    }

    fn validate_observation(
        &self,
        observation: &ExternalHookObservation,
    ) -> Result<(), BridgeError> {
        let valid_id = !observation.event_id.is_empty()
            && observation.event_id.len() <= 256
            && observation
                .event_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        let locator = ExternalLocator {
            key: observation.session.clone(),
            transcript_path: observation.transcript_path.clone(),
            resume_lookup_cwd: observation.cwd.clone(),
            current_cwd: observation.cwd.clone(),
            profile: observation.resume_profile.clone(),
            revision: self.revision,
        };
        let valid_permit = observation.content_permit.as_ref().is_none_or(|permit| {
            !permit.binding_id.is_empty() && permit.binding_revision > 0 && permit.generation > 0
        });
        if !valid_id
            || observation.sequence == 0
            || observation.session.session_id.is_empty()
            || !locator.paths_are_absolute()
            || locator.profile.config_dir != observation.session.config_dir
            || !valid_permit
        {
            return Err(BridgeError::InvalidObservation);
        }
        if let ExternalHookEvent::CwdChanged { new_cwd, .. } = &observation.event {
            if !new_cwd.starts_with('/') {
                return Err(BridgeError::InvalidObservation);
            }
        }
        if observation.managed_takeover
            && !matches!(
                &observation.event,
                ExternalHookEvent::SessionStart { source } if source == "resume"
            )
        {
            return Err(BridgeError::InvalidObservation);
        }
        Ok(())
    }

    fn apply_session_start(
        &mut self,
        observation: &ExternalHookObservation,
        next_revision: u64,
    ) -> ExternalHookApply {
        let storage_key = observation.session.storage_key();
        let Some(existing) = self.sessions.get(&storage_key) else {
            self.sessions.insert(
                storage_key,
                Self::record_from_start(observation, 1, next_revision),
            );
            return ExternalHookApply::Applied;
        };

        // 已明确结束的生命周期才允许同 SID 开新 generation；活动进程身份不同但无法证明旧
        // 进程已死时保持原 locator，标 Conflict，绝不“最后写入者获胜”。
        let same_verified_process = match (&existing.process, &observation.process) {
            (Some(current), Some(observed)) => current == observed,
            (None, _) | (_, None) => false,
        };
        if existing.lifecycle != ExternalLifecycle::Ended && !same_verified_process {
            let record = self
                .sessions
                .get_mut(&storage_key)
                .expect(summon_i18n::text!("r016449f28f"));
            record.lifecycle = ExternalLifecycle::Conflict;
            record.last_seen_ms = record.last_seen_ms.max(observation.observed_at_ms);
            record.last_applied_sequence = observation.sequence;
            record.pending_prompt = None;
            self.continuation_prompts.remove(&storage_key);
            return ExternalHookApply::Conflict;
        }

        if existing.lifecycle == ExternalLifecycle::Ended {
            let generation = existing.generation.saturating_add(1);
            let verified_resume = mirror_origin_verified(observation)
                && matches!(
                    &observation.event,
                    ExternalHookEvent::SessionStart { source, .. } if source == "resume"
                );
            self.prepare_generation_rollover(
                &observation.session,
                existing.generation,
                verified_resume,
            );
            self.sessions.insert(
                storage_key,
                Self::record_from_start(observation, generation, next_revision),
            );
            return ExternalHookApply::Applied;
        }

        let record = self
            .sessions
            .get_mut(&storage_key)
            .expect(summon_i18n::text!("r016449f28f"));
        if observation.observed_at_ms < record.first_seen_ms {
            return ExternalHookApply::Stale;
        }
        Self::refresh_locator(record, observation, next_revision);
        record.locator.resume_lookup_cwd = observation.cwd.clone();
        if record.lifecycle != ExternalLifecycle::Conflict {
            record.lifecycle = if mirror_origin_verified(observation) {
                ExternalLifecycle::ActiveIdle
            } else {
                ExternalLifecycle::Unverified
            };
        }
        record.last_seen_ms = record.last_seen_ms.max(observation.observed_at_ms);
        record.pending_prompt = None;
        self.continuation_prompts.remove(&storage_key);
        ExternalHookApply::Applied
    }

    fn record_from_start(
        observation: &ExternalHookObservation,
        generation: u64,
        revision: u64,
    ) -> ExternalSessionRecord {
        ExternalSessionRecord {
            key: observation.session.clone(),
            generation,
            locator: ExternalLocator {
                key: observation.session.clone(),
                transcript_path: observation.transcript_path.clone(),
                resume_lookup_cwd: observation.cwd.clone(),
                current_cwd: observation.cwd.clone(),
                // eligibility 完全沿用 probe 证据；SessionStart 本身不把 strict handoff 提升为
                // verified/eligible。
                profile: observation.resume_profile.clone(),
                revision,
            },
            process: observation.process.clone(),
            lifecycle: if mirror_origin_verified(observation) {
                ExternalLifecycle::ActiveIdle
            } else {
                ExternalLifecycle::Unverified
            },
            first_seen_ms: observation.observed_at_ms,
            last_seen_ms: observation.observed_at_ms,
            last_applied_sequence: observation.sequence,
            pending_prompt: None,
        }
    }

    fn apply_session_update(
        &mut self,
        observation: &ExternalHookObservation,
        next_revision: u64,
        update: impl FnOnce(&mut ExternalSessionRecord),
    ) -> ExternalHookApply {
        let Some(record) = self.sessions.get_mut(&observation.session.storage_key()) else {
            return ExternalHookApply::Stale;
        };
        if observation.observed_at_ms < record.first_seen_ms {
            return ExternalHookApply::Stale;
        }
        if record.process != observation.process {
            record.lifecycle = ExternalLifecycle::Conflict;
            record.last_seen_ms = record.last_seen_ms.max(observation.observed_at_ms);
            record.last_applied_sequence = observation.sequence;
            return ExternalHookApply::Conflict;
        }
        if record.lifecycle == ExternalLifecycle::Ended {
            return ExternalHookApply::Stale;
        }
        Self::refresh_locator(record, observation, next_revision);
        update(record);
        record.last_seen_ms = record.last_seen_ms.max(observation.observed_at_ms);
        ExternalHookApply::Applied
    }

    fn apply_user_prompt(
        &mut self,
        observation: &ExternalHookObservation,
        prompt_id: &Option<String>,
        prompt: &Option<String>,
        next_revision: u64,
    ) -> ExternalHookApply {
        let permit_is_current = self.current_binding_for_permit(observation).is_some();
        self.continuation_prompts
            .remove(&observation.session.storage_key());
        let mut replaced = None;
        let outcome = self.apply_session_update(observation, next_revision, |record| {
            if matches!(
                record.lifecycle,
                ExternalLifecycle::ActiveIdle | ExternalLifecycle::ActiveBusy
            ) {
                record.lifecycle = ExternalLifecycle::ActiveBusy;
            }
            replaced = record.pending_prompt.take();
            record.pending_prompt = match (
                prompt.as_deref().filter(|text| !text.is_empty()),
                observation
                    .content_permit
                    .clone()
                    .filter(|_| permit_is_current),
            ) {
                (Some(text), Some(content_permit)) => Some(PendingExternalPrompt {
                    event_id: observation.event_id.clone(),
                    generation: record.generation,
                    prompt_id: prompt_id.clone(),
                    text: text.to_string(),
                    observed_at_ms: observation.observed_at_ms,
                    content_permit: Some(content_permit),
                }),
                _ => None,
            };
        });
        if outcome == ExternalHookApply::Applied {
            self.finalize_pending_prompt(
                observation,
                replaced,
                ::summon_core::i18n::label(INTERRUPTED_TERMINAL_MESSAGE),
            );
        }
        outcome
    }

    fn apply_stop(
        &mut self,
        observation: &ExternalHookObservation,
        prompt_id: &Option<String>,
        assistant: Option<&str>,
        semantics: StopSemantics,
        next_revision: u64,
    ) -> ExternalHookApply {
        let storage_key = observation.session.storage_key();
        let Some(record) = self.sessions.get_mut(&storage_key) else {
            return ExternalHookApply::Stale;
        };
        if observation.observed_at_ms < record.first_seen_ms {
            return ExternalHookApply::Stale;
        }
        if record.process != observation.process {
            record.lifecycle = ExternalLifecycle::Conflict;
            record.last_seen_ms = record.last_seen_ms.max(observation.observed_at_ms);
            record.last_applied_sequence = observation.sequence;
            return ExternalHookApply::Conflict;
        }
        if record.lifecycle == ExternalLifecycle::Ended {
            return ExternalHookApply::Stale;
        }
        Self::refresh_locator(record, observation, next_revision);
        record.last_seen_ms = record.last_seen_ms.max(observation.observed_at_ms);
        if semantics.pending_wakeups {
            if matches!(
                record.lifecycle,
                ExternalLifecycle::ActiveIdle | ExternalLifecycle::ActiveBusy
            ) {
                record.lifecycle = ExternalLifecycle::ActiveBusy;
            }
            return ExternalHookApply::Applied;
        }
        let pending_matches = record.pending_prompt.as_ref().is_some_and(|prompt| {
            prompt.content_permit.as_ref() == observation.content_permit.as_ref()
                && prompt_ids_match(prompt.prompt_id.as_deref(), prompt_id.as_deref())
        });
        let pending = if pending_matches {
            if matches!(
                record.lifecycle,
                ExternalLifecycle::ActiveIdle | ExternalLifecycle::ActiveBusy
            ) {
                record.lifecycle = ExternalLifecycle::ActiveIdle;
            }
            record.pending_prompt.take()
        } else {
            // 迟到 Stop/StopFailure 不能吞掉已经开始的下一轮；没有正文 pending 时仍可更新
            // lifecycle，保持 discovery 的忙闲观测准确。
            if record.pending_prompt.is_none()
                && matches!(
                    record.lifecycle,
                    ExternalLifecycle::ActiveIdle | ExternalLifecycle::ActiveBusy
                )
            {
                record.lifecycle = ExternalLifecycle::ActiveIdle;
            }
            None
        };
        let continuation = if pending.is_none() && semantics.hook_active {
            self.continuation_prompts
                .get(&storage_key)
                .filter(|prompt| {
                    prompt.content_permit.as_ref() == observation.content_permit.as_ref()
                        && prompt_ids_match(prompt.prompt_id.as_deref(), prompt_id.as_deref())
                })
                .cloned()
        } else {
            None
        };
        let terminal_prompt = pending.or(continuation);
        if let Some(prompt) = terminal_prompt {
            let final_message = assistant
                .filter(|text| !text.is_empty())
                .unwrap_or(::summon_core::i18n::label(INTERRUPTED_TERMINAL_MESSAGE));
            if semantics.retain_for_continuation && assistant.is_some_and(|text| !text.is_empty()) {
                self.continuation_prompts
                    .insert(storage_key, prompt.clone());
            } else {
                self.continuation_prompts.remove(&storage_key);
            }
            self.finalize_pending_prompt(observation, Some(prompt), final_message);
        }
        ExternalHookApply::Applied
    }

    fn apply_session_end_observation(
        &mut self,
        observation: &ExternalHookObservation,
        next_revision: u64,
    ) -> ExternalHookApply {
        let Some(record) = self.sessions.get_mut(&observation.session.storage_key()) else {
            return ExternalHookApply::Stale;
        };
        if observation.observed_at_ms < record.first_seen_ms
            || record.process != observation.process
            || observation
                .process
                .as_ref()
                .is_none_or(|process| !process.is_complete())
        {
            return ExternalHookApply::Stale;
        }
        let should_end = record.lifecycle != ExternalLifecycle::Conflict;
        // SessionEnd 往往发生在进程环境已经不可读之后，probe 只能给出降级 profile。
        // 结束事件只确认当前 cwd/时序，不能覆盖本 generation 已验证的 resume 身份与路径；
        // 否则远程接管刚杀源就会把自己后续恢复所需的 capability 证据抹掉。
        record.locator.current_cwd = observation.cwd.clone();
        record.locator.revision = next_revision;
        record.last_applied_sequence = observation.sequence;
        record.last_seen_ms = record.last_seen_ms.max(observation.observed_at_ms);
        let pending = record.pending_prompt.take();
        self.continuation_prompts
            .remove(&observation.session.storage_key());
        // SessionEnd 的安全终态仍属于刚结束的 verified generation。必须在 lifecycle 切到
        // Ended 前按 permit/fingerprint 完成归约；一般 Prompt/Stop 仍只接受 Active lifecycle。
        self.finalize_pending_prompt(
            observation,
            pending,
            ::summon_core::i18n::label(SESSION_ENDED_TERMINAL_MESSAGE),
        );
        if should_end {
            self.sessions
                .get_mut(&observation.session.storage_key())
                .expect(summon_i18n::text!("r1405d55c9d"))
                .lifecycle = ExternalLifecycle::Ended;
        }
        ExternalHookApply::Applied
    }

    /// 同一 SessionKey 的 verified `--resume` 可延续 Mirror，但进程 epoch 之间不能遗留正文或
    /// 激活围栏。未验证的新 epoch 直接释放旧 binding；Activating 无论新 epoch 是否 verified 都
    /// 取消，要求用户基于新进程重新选择。
    fn prepare_generation_rollover(
        &mut self,
        session: &SessionKey,
        old_generation: u64,
        verified_resume: bool,
    ) {
        self.continuation_prompts.remove(&session.storage_key());
        let bindings = self
            .slots
            .iter()
            .filter_map(|(slot_key, slot)| {
                let binding = slot.binding()?;
                (binding.session == *session).then(|| {
                    (
                        slot_key.clone(),
                        binding.clone(),
                        matches!(slot, ChatSlot::MirrorBound(_) | ChatSlot::HandoffPending(_))
                            && verified_resume,
                    )
                })
            })
            .collect::<Vec<_>>();
        for (slot_key, binding, preserve_binding) in bindings {
            // verified resume 是 HandoffPending 正在等待的正向事件；reservation 必须保留到
            // coordinator 用 token/process/tmux exact ACK 提交或显式失败。提前释放会把合法
            // staging 误判成“binding 消失”并终止。
            if preserve_binding {
                continue;
            }
            self.slots.remove(&slot_key);
            if self
                .session_leases
                .get(&session.storage_key())
                .is_some_and(|lease| {
                    lease.binding_id == binding.id && lease.target == binding.target
                })
            {
                self.session_leases.remove(&session.storage_key());
            }
            self.purge_binding_receipts_and_turns(&binding);
            self.remove_tickets_for_binding(&binding);
        }
        if !verified_resume {
            self.pending_mirror_turns
                .retain(|_, turn| turn.session != *session || turn.generation != old_generation);
            self.mirror_export_receipts.retain(|_, receipt| {
                receipt.session != *session || receipt.generation != old_generation
            });
            self.context_export_receipts.retain(|_, receipt| {
                receipt.session != *session || receipt.generation != old_generation
            });
        }
    }

    fn refresh_locator(
        record: &mut ExternalSessionRecord,
        observation: &ExternalHookObservation,
        revision: u64,
    ) {
        record.locator.transcript_path = observation.transcript_path.clone();
        record.locator.current_cwd = observation.cwd.clone();
        record.locator.profile = observation.resume_profile.clone();
        record.locator.revision = revision;
        record.last_applied_sequence = observation.sequence;
    }

    fn remember_processed(&mut self, observation: &ExternalHookObservation) {
        const MAX_PROCESSED_EVENTS: usize = 8_192;
        self.processed_events
            .insert(observation.event_id.clone(), observation.sequence);
        while self.processed_events.len() > MAX_PROCESSED_EVENTS {
            let oldest = self
                .processed_events
                .iter()
                .min_by_key(|(event_id, sequence)| (**sequence, *event_id))
                .map(|(event_id, _)| event_id.clone());
            if let Some(oldest) = oldest {
                self.processed_events.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn current_binding_for_permit(
        &self,
        observation: &ExternalHookObservation,
    ) -> Option<&ChatBinding> {
        self.current_binding_for_content(&observation.session, observation.content_permit.as_ref())
    }

    fn current_binding_for_content(
        &self,
        session: &SessionKey,
        permit: Option<&ContentPermit>,
    ) -> Option<&ChatBinding> {
        let permit = permit?;
        let record = self.session(session)?;
        if permit.generation != record.generation
            || !matches!(
                record.lifecycle,
                ExternalLifecycle::ActiveIdle | ExternalLifecycle::ActiveBusy
            )
            || !record.locator.profile.process_verified
            || record.process.as_ref().is_none_or(|process| {
                !process.is_complete() || process.executable != record.locator.profile.claude_bin
            })
        {
            return None;
        }
        self.slots.values().find_map(|slot| match slot {
            ChatSlot::MirrorBound(binding) | ChatSlot::ExternalOwned(binding)
                if binding.session == *session
                    && binding.id == permit.binding_id
                    && binding.revision == permit.binding_revision =>
            {
                Some(binding)
            }
            ChatSlot::Pending { .. }
            | ChatSlot::Activating(_)
            | ChatSlot::HandoffPending(_)
            | ChatSlot::MirrorBound(_)
            | ChatSlot::ExternalOwned(_)
            | ChatSlot::Closing { .. } => None,
        })
    }

    fn finalize_pending_prompt(
        &mut self,
        observation: &ExternalHookObservation,
        pending: Option<PendingExternalPrompt>,
        assistant: &str,
    ) {
        let Some(prompt) = pending else {
            return;
        };
        let Some(binding) = self
            .current_binding_for_content(&observation.session, prompt.content_permit.as_ref())
            .cloned()
        else {
            return;
        };
        let Some(record) = self.session(&observation.session) else {
            return;
        };
        if prompt.generation != 0 && prompt.generation != record.generation {
            return;
        }
        self.pending_mirror_turns.insert(
            observation.event_id.clone(),
            PendingMirrorTurn {
                id: observation.event_id.clone(),
                session: observation.session.clone(),
                generation: record.generation,
                binding_id: binding.id.clone(),
                binding_revision: binding.revision,
                target: binding.target.clone(),
                target_chat_type: binding.target_chat_type.clone(),
                observed_at_ms: observation.observed_at_ms,
                sequence: observation.sequence,
                messages: vec![
                    ExternalVisibleMessage {
                        role: ExternalVisibleRole::User,
                        text: prompt.text,
                    },
                    ExternalVisibleMessage {
                        role: ExternalVisibleRole::Assistant,
                        text: assistant.to_string(),
                    },
                ],
            },
        );
    }

    pub fn slot(&self, key: &ScopedChatKey) -> Option<&ChatSlot> {
        self.slots.get(&key.storage_key())
    }

    pub fn reserve_ticket(&mut self, request: TicketRequest) -> Result<(), BridgeError> {
        if request.ticket_id.is_empty()
            || request.requester_open_id.is_empty()
            || request.choices.is_empty()
            || request.expires_at_ms <= request.issued_at_ms
        {
            return Err(BridgeError::InvalidInput);
        }
        if self.tickets.contains_key(&request.ticket_id) {
            return Err(BridgeError::TicketExists);
        }
        let normalized_short_code = request
            .short_code
            .as_deref()
            .map(normalize_short_code)
            .transpose()?;
        if normalized_short_code
            .as_ref()
            .is_some_and(|code| self.short_code_index.contains_key(code))
        {
            return Err(BridgeError::TicketExists);
        }
        self.prune_terminal_tickets();
        let slot_key = request.target.storage_key();
        if self.slots.contains_key(&slot_key) {
            return Err(BridgeError::ChatOccupied);
        }
        let slot_revision = self.revision.saturating_add(1);
        self.slots.insert(
            slot_key,
            ChatSlot::Pending {
                ticket_id: request.ticket_id.clone(),
                revision: slot_revision,
                expires_at_ms: request.expires_at_ms,
            },
        );
        let ticket_id = request.ticket_id.clone();
        self.tickets.insert(
            ticket_id.clone(),
            BridgeTicket {
                id: request.ticket_id,
                short_code: normalized_short_code.clone(),
                target: request.target,
                target_chat_type: request.target_chat_type,
                target_provider: request.target_provider,
                requester_open_id: request.requester_open_id,
                requester_union_id: request.requester_union_id,
                control_card_message_id: None,
                private_chat_id: None,
                slot_revision,
                choices: request.choices,
                issued_at_ms: request.issued_at_ms,
                expires_at_ms: request.expires_at_ms,
                status: TicketStatus::Pending,
            },
        );
        if let Some(code) = normalized_short_code {
            self.short_code_index.insert(code, ticket_id);
        }
        self.bump();
        Ok(())
    }

    fn prune_terminal_tickets(&mut self) {
        const MAX_TERMINAL_TICKETS: usize = 512;
        let mut removable = self
            .tickets
            .values()
            .filter(|ticket| match &ticket.status {
                TicketStatus::Cancelled | TicketStatus::Expired => true,
                TicketStatus::Committed {
                    binding_id, mode, ..
                } => self
                    .slot(&ticket.target)
                    .and_then(ChatSlot::binding)
                    .is_none_or(|binding| binding.id != *binding_id || binding.mode != *mode),
                TicketStatus::Pending | TicketStatus::Activating { .. } => false,
            })
            .map(|ticket| (ticket.issued_at_ms, ticket.id.clone()))
            .collect::<Vec<_>>();
        if removable.len() <= MAX_TERMINAL_TICKETS {
            return;
        }
        let remove_count = removable.len() - MAX_TERMINAL_TICKETS;
        removable.sort();
        for (_, id) in removable.into_iter().take(remove_count) {
            if let Some(ticket) = self.tickets.remove(&id) {
                if let Some(code) = ticket.short_code {
                    self.short_code_index.remove(&code);
                }
            }
        }
    }

    /// 短码只负责恢复服务端 ticket；调用方仍必须按 ticket 目标群执行 operate-gate。
    /// 所有失败合并成同一错误，避免通过私聊枚举 requester/scope/ticket 状态。
    pub fn redeem_short_code(
        &self,
        code: &str,
        scope: &str,
        requester_open_id: &str,
        now_ms: u64,
    ) -> Result<BridgeTicket, BridgeError> {
        let code = normalize_short_code(code).map_err(|_| BridgeError::ShortCodeInvalid)?;
        let ticket_id = self
            .short_code_index
            .get(&code)
            .ok_or(BridgeError::ShortCodeInvalid)?;
        let ticket = self
            .tickets
            .get(ticket_id)
            .ok_or(BridgeError::ShortCodeInvalid)?;
        if ticket.short_code.as_deref() != Some(code.as_str())
            || ticket.target.scope != scope
            || ticket.requester_open_id != requester_open_id
            || !matches!(ticket.status, TicketStatus::Pending)
            || ticket.expires_at_ms < now_ms
        {
            return Err(BridgeError::ShortCodeInvalid);
        }
        match self.slot(&ticket.target) {
            Some(ChatSlot::Pending {
                ticket_id: current,
                revision,
                expires_at_ms,
            }) if current == ticket_id
                && *revision == ticket.slot_revision
                && *expires_at_ms == ticket.expires_at_ms =>
            {
                Ok(ticket.clone())
            }
            _ => Err(BridgeError::ShortCodeInvalid),
        }
    }

    pub fn attach_control_card(
        &mut self,
        ticket_id: &str,
        requester_open_id: &str,
        private_chat_id: Option<&str>,
        message_id: &str,
    ) -> Result<(), BridgeError> {
        let ticket = self
            .tickets
            .get_mut(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        if ticket.requester_open_id != requester_open_id {
            return Err(BridgeError::WrongActor);
        }
        if private_chat_id.is_some_and(str::is_empty) || message_id.is_empty() {
            return Err(BridgeError::InvalidInput);
        }
        match (
            ticket.private_chat_id.as_deref(),
            ticket.control_card_message_id.as_deref(),
        ) {
            (current_chat, Some(current_message)) if current_message == message_id => {
                match (current_chat, private_chat_id) {
                    (Some(current), Some(incoming)) if current != incoming => {
                        return Err(BridgeError::BindingMismatch)
                    }
                    (None, Some(incoming)) => {
                        ticket.private_chat_id = Some(incoming.to_string());
                        self.revision = self.revision.saturating_add(1);
                    }
                    (Some(_), None) | (Some(_), Some(_)) | (None, None) => {}
                }
                return Ok(());
            }
            (_, Some(_)) | (Some(_), None) => return Err(BridgeError::BindingMismatch),
            (None, None) => {}
        }
        if !matches!(ticket.status, TicketStatus::Pending) {
            return Err(BridgeError::TicketFinal);
        }
        let short_code = ticket.short_code.take();
        ticket.private_chat_id = private_chat_id.map(str::to_string);
        ticket.control_card_message_id = Some(message_id.to_string());
        if let Some(code) = short_code {
            self.short_code_index.remove(&code);
        }
        self.bump();
        Ok(())
    }

    /// MirrorBound 控制卡过期无法 PATCH 时，用新发卡的 message id 做 exact replacement。
    pub fn replace_bound_control_card(
        &mut self,
        ticket_id: &str,
        binding_id: &str,
        requester_open_id: &str,
        private_chat_id: &str,
        old_message_id: &str,
        new_message_id: &str,
    ) -> Result<(), BridgeError> {
        if private_chat_id.is_empty() || old_message_id.is_empty() || new_message_id.is_empty() {
            return Err(BridgeError::InvalidInput);
        }
        let ticket = self
            .tickets
            .get(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        if ticket.requester_open_id != requester_open_id
            || ticket.private_chat_id.as_deref() != Some(private_chat_id)
            || ticket.control_card_message_id.as_deref() != Some(old_message_id)
            || !matches!(
                &ticket.status,
                TicketStatus::Committed {
                    binding_id: current,
                    mode: BridgeMode::Mirror,
                    ..
                } if current == binding_id
            )
        {
            return Err(BridgeError::BindingMismatch);
        }
        match self.slot(&ticket.target) {
            Some(ChatSlot::MirrorBound(binding)) if binding.id == binding_id => {}
            _ => return Err(BridgeError::BindingMismatch),
        }
        self.tickets
            .get_mut(ticket_id)
            .ok_or(BridgeError::TicketMissing)?
            .control_card_message_id = Some(new_message_id.to_string());
        self.bump();
        Ok(())
    }

    /// 当前 operate-gate 用户在私聊显式 `/bridge` 时接管并重发 Mirror 控制卡。授权判断由
    /// adapter 在进入本方法前完成；领域层只允许对 exact committed Mirror 原子替换操作者与卡。
    #[allow(clippy::too_many_arguments)]
    pub fn transfer_bound_control_card(
        &mut self,
        ticket_id: &str,
        binding_id: &str,
        new_operator_open_id: &str,
        new_operator_union_id: &str,
        private_chat_id: &str,
        new_message_id: &str,
    ) -> Result<(), BridgeError> {
        if new_operator_open_id.is_empty()
            || private_chat_id.is_empty()
            || new_message_id.is_empty()
        {
            return Err(BridgeError::InvalidInput);
        }
        let target = {
            let ticket = self
                .tickets
                .get(ticket_id)
                .ok_or(BridgeError::TicketMissing)?;
            if !matches!(
                &ticket.status,
                TicketStatus::Committed {
                    binding_id: current,
                    mode: BridgeMode::Mirror,
                    ..
                } if current == binding_id
            ) {
                return Err(BridgeError::BindingMismatch);
            }
            ticket.target.clone()
        };
        match self.slot(&target) {
            Some(ChatSlot::MirrorBound(binding)) if binding.id == binding_id => {}
            _ => return Err(BridgeError::BindingMismatch),
        }
        let ticket = self
            .tickets
            .get_mut(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        ticket.requester_open_id = new_operator_open_id.to_string();
        ticket.requester_union_id = new_operator_union_id.to_string();
        ticket.private_chat_id = Some(private_chat_id.to_string());
        ticket.control_card_message_id = Some(new_message_id.to_string());
        let Some(ChatSlot::MirrorBound(binding)) = self.slots.get_mut(&target.storage_key()) else {
            return Err(BridgeError::BindingMismatch);
        };
        binding.actor_open_id = new_operator_open_id.to_string();
        self.bump();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_choice(
        &mut self,
        ticket_id: &str,
        choice_id: &str,
        actor_open_id: &str,
        card_message_id: &str,
        mode: BridgeMode,
        binding_id: &str,
        now_ms: u64,
    ) -> Result<BeginChoiceOutcome, BridgeError> {
        self.begin_choice_inner(
            ticket_id,
            choice_id,
            actor_open_id,
            card_message_id,
            mode,
            binding_id,
            now_ms,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_handoff(
        &mut self,
        ticket_id: &str,
        choice_id: &str,
        actor_open_id: &str,
        card_message_id: &str,
        binding_id: &str,
        now_ms: u64,
        proof: &HandoffBeginProof,
    ) -> Result<BeginChoiceOutcome, BridgeError> {
        self.begin_choice_inner(
            ticket_id,
            choice_id,
            actor_open_id,
            card_message_id,
            BridgeMode::Handoff,
            binding_id,
            now_ms,
            Some(proof),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_choice_inner(
        &mut self,
        ticket_id: &str,
        choice_id: &str,
        actor_open_id: &str,
        card_message_id: &str,
        mode: BridgeMode,
        binding_id: &str,
        now_ms: u64,
        handoff_proof: Option<&HandoffBeginProof>,
    ) -> Result<BeginChoiceOutcome, BridgeError> {
        let ticket = self
            .tickets
            .get(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        if ticket.requester_open_id != actor_open_id {
            return Err(BridgeError::WrongActor);
        }
        if ticket.control_card_message_id.as_deref() != Some(card_message_id) {
            return Err(BridgeError::WrongCard);
        }
        match &ticket.status {
            TicketStatus::Activating {
                binding_id: old_binding,
                choice_id: old_choice,
                mode: old_mode,
            }
            | TicketStatus::Committed {
                binding_id: old_binding,
                choice_id: old_choice,
                mode: old_mode,
            } if old_binding == binding_id && old_choice == choice_id && *old_mode == mode => {
                let expected_session = ticket
                    .choices
                    .iter()
                    .find(|choice| choice.id == *old_choice)
                    .map(|choice| &choice.session)
                    .ok_or(BridgeError::ChoiceMissing)?;
                let slot = self
                    .slot(&ticket.target)
                    .and_then(ChatSlot::binding)
                    .filter(|binding| {
                        binding.id == *old_binding
                            && binding.mode == *old_mode
                            && binding.session == *expected_session
                    })
                    .cloned()
                    .ok_or(BridgeError::BindingMismatch)?;
                return Ok(BeginChoiceOutcome::AlreadyStarted(slot));
            }
            TicketStatus::Pending => {}
            _ => return Err(BridgeError::TicketFinal),
        }
        if ticket.expires_at_ms < now_ms {
            return Err(BridgeError::TicketExpired);
        }
        // 通用卡片路径永远无权进入 HandoffPending；只有上面的专用 API 能携带 source
        // registry + exact process 快照进入本分支。
        if mode == BridgeMode::Handoff && handoff_proof.is_none() {
            return Err(BridgeError::HandoffCapabilityRequired);
        }

        let choice = ticket
            .choices
            .iter()
            .find(|c| c.id == choice_id)
            .cloned()
            .ok_or(BridgeError::ChoiceMissing)?;
        let target = ticket.target.clone();
        let target_chat_type = ticket.target_chat_type.clone();
        let slot_revision = ticket.slot_revision;
        let actor_open_id = ticket.requester_open_id.clone();
        let record = self
            .session(&choice.session)
            .ok_or(BridgeError::SessionUnavailable)?;
        if record.locator.revision != choice.registry_revision {
            return Err(BridgeError::StaleSession);
        }
        match (mode, record.lifecycle) {
            (BridgeMode::Mirror, ExternalLifecycle::ActiveIdle)
            | (
                BridgeMode::Handoff,
                ExternalLifecycle::ActiveIdle
                | ExternalLifecycle::ActiveBusy
                | ExternalLifecycle::Ended,
            ) => {}
            _ => return Err(BridgeError::SessionUnavailable),
        }
        if mode == BridgeMode::Handoff
            && record.handoff_eligibility() != HandoffEligibility::Eligible
        {
            return Err(BridgeError::HandoffIneligible);
        }
        if mode == BridgeMode::Handoff
            && handoff_proof.is_none_or(|proof| {
                proof.session != record.key
                    || proof.generation != record.generation
                    || proof.registry_revision != record.locator.revision
                    || Some(&proof.process) != record.process.as_ref()
            })
        {
            return Err(BridgeError::StaleSession);
        }
        let slot_key = target.storage_key();
        match self.slots.get(&slot_key) {
            Some(ChatSlot::Pending {
                ticket_id: current_ticket,
                revision,
                ..
            }) if current_ticket == ticket_id && *revision == slot_revision => {}
            _ => return Err(BridgeError::StaleSlot),
        }
        let session_key = choice.session.storage_key();
        if self.session_leases.contains_key(&session_key) {
            return Err(BridgeError::SessionLeased);
        }

        let binding = ChatBinding {
            id: binding_id.to_string(),
            target: target.clone(),
            target_chat_type,
            session: choice.session,
            mode,
            actor_open_id,
            created_at_ms: now_ms,
            revision: self.revision.saturating_add(1),
        };
        self.session_leases.insert(
            session_key,
            SessionLease {
                binding_id: binding.id.clone(),
                target: target.clone(),
            },
        );
        let slot = match mode {
            BridgeMode::Mirror => ChatSlot::Activating(binding.clone()),
            BridgeMode::Handoff => ChatSlot::HandoffPending(binding.clone()),
        };
        self.slots.insert(slot_key, slot);
        let ticket = self
            .tickets
            .get_mut(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        ticket.status = TicketStatus::Activating {
            binding_id: binding.id.clone(),
            choice_id: choice_id.to_string(),
            mode,
        };
        self.bump();
        Ok(BeginChoiceOutcome::Started(binding))
    }

    pub fn allocate_activation_delivery_sequence(
        &mut self,
        ticket_id: &str,
        binding_id: &str,
    ) -> Result<u64, BridgeError> {
        let ticket = self
            .tickets
            .get(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        let binding = match (&ticket.status, self.slot(&ticket.target)) {
            (
                TicketStatus::Activating {
                    binding_id: current,
                    mode: BridgeMode::Mirror,
                    ..
                },
                Some(ChatSlot::Activating(binding)),
            ) if current == binding_id && binding.id == binding_id => binding.clone(),
            _ => return Err(BridgeError::BindingMismatch),
        };
        self.require_exact_lease(&binding)?;
        match self
            .delivery_sequence_cursors
            .get(binding_id)
            .copied()
            .unwrap_or(0)
        {
            0 => {
                self.delivery_sequence_cursors
                    .insert(binding_id.to_string(), 1);
                self.bump();
            }
            1 if !self.activation_audit_receipts.contains_key(binding_id) => {}
            _ => return Err(BridgeError::ActivationAuditMismatch),
        }
        Ok(1)
    }

    pub fn allocate_mirror_turn_delivery_sequence(
        &mut self,
        turn_id: &str,
    ) -> Result<u64, BridgeError> {
        let turn = self
            .pending_mirror_turns
            .get(turn_id)
            .ok_or(BridgeError::MirrorTurnMissing)?
            .clone();
        let binding = match self.slot(&turn.target) {
            Some(ChatSlot::MirrorBound(binding))
                if binding.id == turn.binding_id
                    && binding.revision == turn.binding_revision
                    && binding.session == turn.session =>
            {
                binding.clone()
            }
            _ => return Err(BridgeError::BindingMismatch),
        };
        self.require_exact_lease(&binding)?;
        self.next_delivery_sequence(&binding.id)
    }

    pub fn allocate_context_delivery_sequence(
        &mut self,
        binding_id: &str,
        binding_revision: u64,
    ) -> Result<u64, BridgeError> {
        let binding = self
            .slots
            .values()
            .find_map(|slot| match slot {
                ChatSlot::MirrorBound(binding)
                    if binding.id == binding_id && binding.revision == binding_revision =>
                {
                    Some(binding.clone())
                }
                _ => None,
            })
            .ok_or(BridgeError::BindingMismatch)?;
        if self.pending_mirror_turns.values().any(|turn| {
            turn.binding_id == binding.id
                && turn.binding_revision == binding.revision
                && turn.target == binding.target
        }) {
            return Err(BridgeError::PendingMirrorTurns);
        }
        self.require_exact_lease(&binding)?;
        self.next_delivery_sequence(&binding.id)
    }

    fn next_delivery_sequence(&mut self, binding_id: &str) -> Result<u64, BridgeError> {
        let current = self
            .delivery_sequence_cursors
            .get(binding_id)
            .copied()
            .ok_or(BridgeError::ActivationAuditRequired)?;
        let next = current.checked_add(1).ok_or(BridgeError::InvalidInput)?;
        self.delivery_sequence_cursors
            .insert(binding_id.to_string(), next);
        self.bump();
        Ok(next)
    }

    /// 记录群审计投递成功。调用方必须先得到飞书成功响应，再在独立 state commit 中调用；这样
    /// daemon 崩溃后可以用同一官方 uuid 重试，而正文始终要等 receipt durable 后才有资格导出。
    pub fn record_activation_audit(
        &mut self,
        receipt: ActivationAuditReceipt,
    ) -> Result<(), BridgeError> {
        if receipt.id.is_empty()
            || receipt.outbox_id.is_empty()
            || receipt.ticket_id.is_empty()
            || receipt.binding_id.is_empty()
            || receipt.generation == 0
            || receipt.sequence != 1
        {
            return Err(BridgeError::InvalidInput);
        }
        if let Some(existing) = self.activation_audit_receipts.get(&receipt.binding_id) {
            return if existing == &receipt {
                Ok(())
            } else {
                Err(BridgeError::ActivationAuditMismatch)
            };
        }
        let ticket = self
            .tickets
            .get(&receipt.ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        match &ticket.status {
            TicketStatus::Activating {
                binding_id, mode, ..
            } if binding_id == &receipt.binding_id && *mode == BridgeMode::Mirror => {}
            _ => return Err(BridgeError::BindingMismatch),
        }
        if self.delivery_sequence_cursors.get(&receipt.binding_id) != Some(&receipt.sequence) {
            return Err(BridgeError::ActivationAuditMismatch);
        }
        if ticket.target != receipt.target {
            return Err(BridgeError::BindingMismatch);
        }
        let binding = match self.slot(&receipt.target) {
            Some(ChatSlot::Activating(binding))
                if binding.id == receipt.binding_id
                    && binding.revision == receipt.binding_revision
                    && binding.session == receipt.session
                    && binding.mode == BridgeMode::Mirror =>
            {
                binding.clone()
            }
            _ => return Err(BridgeError::BindingMismatch),
        };
        let record = self
            .session(&binding.session)
            .ok_or(BridgeError::SessionUnavailable)?;
        if record.generation != receipt.generation
            || record.lifecycle != ExternalLifecycle::ActiveIdle
        {
            return Err(BridgeError::SessionUnavailable);
        }
        self.require_exact_lease(&binding)?;
        self.activation_audit_receipts
            .insert(receipt.binding_id.clone(), receipt);
        self.bump();
        Ok(())
    }

    /// 专用于 Mirror 的最终 CAS。receipt id 是显式 proof capability，避免调用者只凭 ticket/
    /// binding 猜中当前状态就完成激活。
    pub fn complete_mirror_after_audit(
        &mut self,
        ticket_id: &str,
        binding_id: &str,
        receipt_id: &str,
    ) -> Result<ChatBinding, BridgeError> {
        match self.activation_audit_receipts.get(binding_id) {
            Some(receipt)
                if receipt.id == receipt_id
                    && receipt.ticket_id == ticket_id
                    && receipt.binding_id == binding_id => {}
            Some(_) => return Err(BridgeError::ActivationAuditMismatch),
            None => return Err(BridgeError::ActivationAuditRequired),
        }
        self.complete_binding(ticket_id, binding_id, BridgeMode::Mirror)
    }

    pub fn complete_binding(
        &mut self,
        ticket_id: &str,
        binding_id: &str,
        mode: BridgeMode,
    ) -> Result<ChatBinding, BridgeError> {
        if mode == BridgeMode::Handoff {
            return Err(BridgeError::HandoffProofRequired);
        }
        let ticket = self
            .tickets
            .get(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        let choice_id = match &ticket.status {
            TicketStatus::Activating {
                binding_id: current,
                choice_id,
                mode: current_mode,
            } if current == binding_id && *current_mode == mode => choice_id.clone(),
            TicketStatus::Committed {
                binding_id: current,
                choice_id,
                mode: current_mode,
            } if current == binding_id && *current_mode == mode => {
                let expected_session = ticket
                    .choices
                    .iter()
                    .find(|choice| choice.id == *choice_id)
                    .map(|choice| &choice.session)
                    .ok_or(BridgeError::ChoiceMissing)?;
                let binding = self
                    .slot(&ticket.target)
                    .and_then(ChatSlot::binding)
                    .filter(|binding| {
                        binding.id == binding_id
                            && binding.mode == mode
                            && binding.session == *expected_session
                    })
                    .cloned()
                    .ok_or(BridgeError::BindingMismatch)?;
                let audit = self
                    .activation_audit_receipts
                    .get(binding_id)
                    .ok_or(BridgeError::ActivationAuditRequired)?;
                if audit.ticket_id != ticket_id
                    || audit.binding_id != binding.id
                    || audit.binding_revision != binding.revision
                    || audit.target != binding.target
                    || audit.session != binding.session
                {
                    return Err(BridgeError::ActivationAuditMismatch);
                }
                self.require_exact_lease(&binding)?;
                return Ok(binding);
            }
            _ => return Err(BridgeError::BindingMismatch),
        };
        let target = ticket.target.clone();
        let slot_key = target.storage_key();
        let binding = self
            .slots
            .get(&slot_key)
            .and_then(ChatSlot::binding)
            .filter(|b| b.id == binding_id && b.mode == mode)
            .cloned()
            .ok_or(BridgeError::BindingMismatch)?;
        let audit = self
            .activation_audit_receipts
            .get(binding_id)
            .ok_or(BridgeError::ActivationAuditRequired)?;
        if audit.ticket_id != ticket_id
            || audit.binding_id != binding.id
            || audit.binding_revision != binding.revision
            || audit.target != binding.target
            || audit.session != binding.session
        {
            return Err(BridgeError::ActivationAuditMismatch);
        }
        let choice = ticket
            .choices
            .iter()
            .find(|choice| choice.id == choice_id)
            .ok_or(BridgeError::ChoiceMissing)?;
        if binding.session != choice.session {
            return Err(BridgeError::BindingMismatch);
        }
        let record = self
            .session(&choice.session)
            .ok_or(BridgeError::SessionUnavailable)?;
        if record.locator.revision != choice.registry_revision {
            return Err(BridgeError::StaleSession);
        }
        if record.lifecycle != ExternalLifecycle::ActiveIdle {
            return Err(BridgeError::SessionUnavailable);
        }
        if record.generation != audit.generation {
            return Err(BridgeError::ActivationAuditMismatch);
        }
        self.require_exact_lease(&binding)?;
        self.slots.insert(
            slot_key,
            match mode {
                BridgeMode::Mirror => ChatSlot::MirrorBound(binding.clone()),
                BridgeMode::Handoff => ChatSlot::ExternalOwned(binding.clone()),
            },
        );
        self.tickets
            .get_mut(ticket_id)
            .ok_or(BridgeError::TicketMissing)?
            .status = TicketStatus::Committed {
            binding_id: binding.id.clone(),
            choice_id,
            mode,
        };
        self.bump();
        Ok(binding)
    }

    pub fn complete_handoff_takeover(
        &mut self,
        ticket_id: &str,
        binding_id: &str,
        target_session: &str,
        resumed_generation: u64,
        resumed_process: &ProcessFingerprint,
        session_start_event_id: &str,
    ) -> Result<ManagedTakeover, BridgeError> {
        if resumed_generation == 0
            || !resumed_process.is_complete()
            || target_session.is_empty()
            || session_start_event_id.is_empty()
        {
            return Err(BridgeError::InvalidInput);
        }
        let ticket = self
            .tickets
            .get(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        let choice_id = match &ticket.status {
            TicketStatus::Activating {
                binding_id: current,
                choice_id,
                mode: BridgeMode::Handoff,
            } if current == binding_id => choice_id.clone(),
            TicketStatus::Committed {
                binding_id: current,
                mode: BridgeMode::Handoff,
                ..
            } if current == binding_id => {
                return self
                    .managed_takeovers
                    .get(&ticket.target.storage_key())
                    .filter(|receipt| {
                        receipt.binding_id == binding_id
                            && receipt.target_session == target_session
                            && receipt.generation == resumed_generation
                            && receipt.process == *resumed_process
                            && receipt.session_start_event_id == session_start_event_id
                    })
                    .cloned()
                    .ok_or(BridgeError::BindingMismatch)
            }
            _ => return Err(BridgeError::BindingMismatch),
        };
        let target = ticket.target.clone();
        let binding = match self.slot(&target) {
            Some(ChatSlot::HandoffPending(binding))
                if binding.id == binding_id && binding.mode == BridgeMode::Handoff =>
            {
                binding.clone()
            }
            _ => return Err(BridgeError::BindingMismatch),
        };
        self.require_exact_lease(&binding)?;
        let choice = ticket
            .choices
            .iter()
            .find(|choice| choice.id == choice_id)
            .ok_or(BridgeError::ChoiceMissing)?;
        if choice.session != binding.session {
            return Err(BridgeError::BindingMismatch);
        }
        let record = self
            .session(&binding.session)
            .ok_or(BridgeError::SessionUnavailable)?;
        if record.generation != resumed_generation
            || record.process.as_ref() != Some(resumed_process)
            || record.lifecycle != ExternalLifecycle::ActiveIdle
            || !self.processed_events.contains_key(session_start_event_id)
        {
            return Err(BridgeError::HandoffProofRequired);
        }
        let receipt = ManagedTakeover {
            binding_id: binding.id.clone(),
            binding_revision: binding.revision,
            target: target.clone(),
            target_session: target_session.to_string(),
            session: binding.session.clone(),
            generation: resumed_generation,
            process: resumed_process.clone(),
            session_start_event_id: session_start_event_id.to_string(),
        };
        self.slots.remove(&target.storage_key());
        self.session_leases.remove(&binding.session.storage_key());
        self.managed_takeovers
            .insert(target.storage_key(), receipt.clone());
        self.tickets
            .get_mut(ticket_id)
            .ok_or(BridgeError::TicketMissing)?
            .status = TicketStatus::Committed {
            binding_id: binding.id,
            choice_id,
            mode: BridgeMode::Handoff,
        };
        self.bump();
        Ok(receipt)
    }

    /// IO coordinator 已证明 canonical tmux 与 takeover marker 都不存在后，按完整 receipt
    /// 快照释放冲突哨兵。字段不一致时 fail-closed，避免旧清理任务删除新一代 Takeover。
    pub fn release_managed_takeover(
        &mut self,
        expected: &ManagedTakeover,
    ) -> Result<bool, BridgeError> {
        let key = expected.target.storage_key();
        let Some(current) = self.managed_takeovers.get(&key) else {
            return Ok(false);
        };
        if current != expected {
            return Err(BridgeError::BindingMismatch);
        }
        self.managed_takeovers.remove(&key);
        self.bump();
        Ok(true)
    }

    pub fn takeover_conflict(&self, target: &ScopedChatKey) -> bool {
        self.managed_takeovers
            .get(&target.storage_key())
            .and_then(|takeover| self.session(&takeover.session))
            .is_some_and(|record| record.lifecycle == ExternalLifecycle::Conflict)
    }

    /// 把 pending body 原子换成轻量 export receipt。IO 层必须先 `enqueue_once` 同一
    /// `outbox_id`；若在两步间崩溃，消费者因为 receipt 尚不存在只能 retry，不能发送正文。
    pub fn export_pending_mirror_turn(
        &mut self,
        receipt: MirrorExportReceipt,
    ) -> Result<MirrorExportOutcome, BridgeError> {
        if receipt.id.is_empty()
            || receipt.outbox_id.is_empty()
            || receipt.turn_id.is_empty()
            || receipt.binding_id.is_empty()
            || receipt.generation == 0
            || receipt.sequence == 0
        {
            return Err(BridgeError::InvalidInput);
        }
        if let Some(existing) = self.mirror_export_receipts.get(&receipt.id) {
            return if existing == &receipt {
                Ok(MirrorExportOutcome::AlreadyExported)
            } else {
                Err(BridgeError::MirrorExportMismatch)
            };
        }
        if self.mirror_export_receipts.values().any(|existing| {
            existing.turn_id == receipt.turn_id || existing.outbox_id == receipt.outbox_id
        }) {
            return Err(BridgeError::MirrorExportMismatch);
        }
        let turn = self
            .pending_mirror_turns
            .get(&receipt.turn_id)
            .ok_or(BridgeError::MirrorTurnMissing)?;
        if turn.id != receipt.turn_id
            || turn.binding_id != receipt.binding_id
            || turn.binding_revision != receipt.binding_revision
            || turn.target != receipt.target
            || turn.session != receipt.session
            || turn.generation != receipt.generation
        {
            return Err(BridgeError::MirrorExportMismatch);
        }
        let binding = match self.slot(&receipt.target) {
            Some(ChatSlot::MirrorBound(binding))
                if binding.id == receipt.binding_id
                    && binding.revision == receipt.binding_revision
                    && binding.session == receipt.session
                    && binding.mode == BridgeMode::Mirror =>
            {
                binding.clone()
            }
            _ => return Err(BridgeError::BindingMismatch),
        };
        let record = self
            .session(&receipt.session)
            .ok_or(BridgeError::SessionUnavailable)?;
        if !record_accepts_authorized_generation(record, receipt.generation) {
            return Err(BridgeError::MirrorExportMismatch);
        }
        self.require_exact_lease(&binding)?;
        if self
            .delivery_sequence_cursors
            .get(&receipt.binding_id)
            .is_none_or(|cursor| *cursor < receipt.sequence)
        {
            return Err(BridgeError::MirrorExportMismatch);
        }
        self.pending_mirror_turns.remove(&receipt.turn_id);
        self.mirror_export_receipts
            .insert(receipt.id.clone(), receipt);
        self.bump();
        Ok(MirrorExportOutcome::Exported)
    }

    /// 网络成功后的精确收尾。缺失表示上次已完成，按幂等成功处理；存在但 outbox 不匹配则
    /// fail-closed，绝不能误删另一轮的 proof。
    pub fn mark_mirror_export_delivered(
        &mut self,
        receipt_id: &str,
        outbox_id: &str,
        delivered_at_ms: u64,
    ) -> Result<(), BridgeError> {
        let receipt = self
            .mirror_export_receipts
            .get_mut(receipt_id)
            .ok_or(BridgeError::MirrorTurnMissing)?;
        if receipt.outbox_id != outbox_id {
            return Err(BridgeError::MirrorExportMismatch);
        }
        if receipt.delivered_at_ms.is_none() {
            receipt.delivered_at_ms = Some(delivered_at_ms);
            self.bump();
        }
        Ok(())
    }

    pub fn finish_mirror_export(
        &mut self,
        receipt_id: &str,
        outbox_id: &str,
    ) -> Result<bool, BridgeError> {
        match self.mirror_export_receipts.get(receipt_id) {
            Some(receipt) if receipt.outbox_id == outbox_id => {}
            Some(_) => return Err(BridgeError::MirrorExportMismatch),
            None => return Ok(false),
        }
        self.mirror_export_receipts.remove(receipt_id);
        self.bump();
        Ok(true)
    }

    pub fn discard_pending_mirror_turn(
        &mut self,
        turn_id: &str,
        binding_id: &str,
        binding_revision: u64,
    ) -> Result<bool, BridgeError> {
        match self.pending_mirror_turns.get(turn_id) {
            Some(turn)
                if turn.binding_id == binding_id && turn.binding_revision == binding_revision => {}
            Some(_) => return Err(BridgeError::MirrorExportMismatch),
            None => return Ok(false),
        }
        self.pending_mirror_turns.remove(turn_id);
        self.bump();
        Ok(true)
    }

    pub fn record_context_export(
        &mut self,
        receipt: ContextExportReceipt,
    ) -> Result<(), BridgeError> {
        if receipt.id.is_empty()
            || receipt.outbox_id.is_empty()
            || receipt.ticket_id.is_empty()
            || receipt.choice_id.is_empty()
            || receipt.requester_open_id.is_empty()
            || receipt.sequence == 0
        {
            return Err(BridgeError::InvalidInput);
        }
        if let Some(existing) = self.context_export_receipts.get(&receipt.id) {
            return if existing == &receipt {
                Ok(())
            } else {
                Err(BridgeError::ContextExportMismatch)
            };
        }
        self.validate_context_export(&receipt)?;
        if self
            .delivery_sequence_cursors
            .get(&receipt.binding_id)
            .is_none_or(|cursor| *cursor < receipt.sequence)
        {
            return Err(BridgeError::ContextExportMismatch);
        }
        self.context_export_receipts
            .insert(receipt.id.clone(), receipt);
        self.bump();
        Ok(())
    }

    /// 恢复“outbox 已 durable、state cursor/receipt 尚未提交”的唯一合法窗口。pending outbox
    /// event 是前一事务的 durable 证据；只有 cursor 恰好落后一位且该 sequence 未被任何 receipt
    /// 占用时，才允许在同一次 state commit 中推进 cursor 并登记 receipt。
    pub fn recover_context_export(
        &mut self,
        receipt: ContextExportReceipt,
    ) -> Result<bool, BridgeError> {
        if let Some(existing) = self.context_export_receipts.get(&receipt.id) {
            return if existing == &receipt {
                Ok(false)
            } else {
                Err(BridgeError::ContextExportMismatch)
            };
        }
        self.validate_context_export(&receipt)?;
        let cursor = self
            .delivery_sequence_cursors
            .get(&receipt.binding_id)
            .copied()
            .ok_or(BridgeError::ContextExportMismatch)?;
        if cursor.checked_add(1) != Some(receipt.sequence)
            || self.delivery_sequence_is_used(&receipt.binding_id, receipt.sequence)
        {
            return Err(BridgeError::ContextExportMismatch);
        }
        self.delivery_sequence_cursors
            .insert(receipt.binding_id.clone(), receipt.sequence);
        self.context_export_receipts
            .insert(receipt.id.clone(), receipt);
        self.bump();
        Ok(true)
    }

    fn validate_context_export(&self, receipt: &ContextExportReceipt) -> Result<(), BridgeError> {
        if receipt.id.is_empty()
            || receipt.outbox_id.is_empty()
            || receipt.ticket_id.is_empty()
            || receipt.choice_id.is_empty()
            || receipt.requester_open_id.is_empty()
            || receipt.sequence == 0
        {
            return Err(BridgeError::InvalidInput);
        }
        let ticket = self
            .tickets
            .get(&receipt.ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        if ticket.target != receipt.target
            || ticket.requester_open_id != receipt.requester_open_id
            || !ticket
                .choices
                .iter()
                .any(|choice| choice.id == receipt.choice_id && choice.session == receipt.session)
            || !matches!(
                &ticket.status,
                TicketStatus::Committed {
                    binding_id,
                    choice_id,
                    mode: BridgeMode::Mirror,
                } if binding_id == &receipt.binding_id && choice_id == &receipt.choice_id
            )
        {
            return Err(BridgeError::ContextExportMismatch);
        }
        let binding = match self.slot(&receipt.target) {
            Some(ChatSlot::MirrorBound(binding))
                if binding.id == receipt.binding_id
                    && binding.revision == receipt.binding_revision
                    && binding.session == receipt.session =>
            {
                binding.clone()
            }
            _ => return Err(BridgeError::BindingMismatch),
        };
        if binding.target_chat_type != receipt.target_chat_type {
            return Err(BridgeError::ContextExportMismatch);
        }
        let record = self
            .session(&receipt.session)
            .ok_or(BridgeError::SessionUnavailable)?;
        if !record_accepts_authorized_generation(record, receipt.generation) {
            return Err(BridgeError::ContextExportMismatch);
        }
        self.require_exact_lease(&binding)?;
        Ok(())
    }

    fn delivery_sequence_is_used(&self, binding_id: &str, sequence: u64) -> bool {
        self.activation_audit_receipts
            .values()
            .any(|receipt| receipt.binding_id == binding_id && receipt.sequence == sequence)
            || self
                .mirror_export_receipts
                .values()
                .any(|receipt| receipt.binding_id == binding_id && receipt.sequence == sequence)
            || self
                .context_export_receipts
                .values()
                .any(|receipt| receipt.binding_id == binding_id && receipt.sequence == sequence)
    }

    pub fn finish_context_export(
        &mut self,
        receipt_id: &str,
        outbox_id: &str,
    ) -> Result<bool, BridgeError> {
        match self.context_export_receipts.get(receipt_id) {
            Some(receipt) if receipt.outbox_id == outbox_id => {}
            Some(_) => return Err(BridgeError::ContextExportMismatch),
            None => return Ok(false),
        }
        self.context_export_receipts.remove(receipt_id);
        self.bump();
        Ok(true)
    }

    pub fn mark_context_export_delivered(
        &mut self,
        receipt_id: &str,
        outbox_id: &str,
        delivered_at_ms: u64,
    ) -> Result<(), BridgeError> {
        let receipt = self
            .context_export_receipts
            .get_mut(receipt_id)
            .ok_or(BridgeError::ContextExportMismatch)?;
        if receipt.outbox_id != outbox_id {
            return Err(BridgeError::ContextExportMismatch);
        }
        if receipt.delivered_at_ms.is_none() {
            receipt.delivered_at_ms = Some(delivered_at_ms);
            self.bump();
        }
        Ok(())
    }

    fn require_exact_lease(&self, binding: &ChatBinding) -> Result<(), BridgeError> {
        match self.session_leases.get(&binding.session.storage_key()) {
            Some(lease) if lease.binding_id == binding.id && lease.target == binding.target => {
                Ok(())
            }
            _ => Err(BridgeError::BindingMismatch),
        }
    }

    /// 取消仍在 Pending/Activating 的 ticket；终态绑定必须走显式 stop/unbind。
    pub fn cancel_ticket(&mut self, ticket_id: &str) -> Result<(), BridgeError> {
        let ticket = self
            .tickets
            .get(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        let target = ticket.target.clone();
        let target_key = target.storage_key();
        let binding_to_remove = match &ticket.status {
            TicketStatus::Pending => match self.slots.get(&target_key) {
                Some(ChatSlot::Pending {
                    ticket_id: current,
                    revision,
                    ..
                }) if current == ticket_id && *revision == ticket.slot_revision => None,
                _ => return Err(BridgeError::StaleSlot),
            },
            TicketStatus::Activating {
                binding_id, mode, ..
            } => match self.slots.get(&target_key).and_then(ChatSlot::binding) {
                Some(binding) if binding.id == *binding_id && binding.mode == *mode => {
                    Some(binding.clone())
                }
                _ => return Err(BridgeError::StaleSlot),
            },
            TicketStatus::Committed { .. } | TicketStatus::Cancelled | TicketStatus::Expired => {
                return Err(BridgeError::TicketFinal)
            }
        };
        if let Some(binding) = binding_to_remove {
            match self.session_leases.get(&binding.session.storage_key()) {
                Some(lease) if lease.binding_id == binding.id && lease.target == binding.target => {
                    self.session_leases.remove(&binding.session.storage_key());
                }
                Some(_) => return Err(BridgeError::BindingMismatch),
                None => {}
            }
            self.purge_binding_receipts_and_turns(&binding);
        }
        self.slots.remove(&target_key);
        let ticket = self
            .tickets
            .get_mut(ticket_id)
            .ok_or(BridgeError::TicketMissing)?;
        if let Some(code) = ticket.short_code.take() {
            self.short_code_index.remove(&code);
        }
        ticket.status = TicketStatus::Cancelled;
        self.bump();
        Ok(())
    }

    pub fn expire_pending(&mut self, now_ms: u64) -> Vec<String> {
        let ids: Vec<String> = self
            .tickets
            .values()
            .filter(|t| matches!(t.status, TicketStatus::Pending) && t.expires_at_ms < now_ms)
            .map(|t| t.id.clone())
            .collect();
        for id in &ids {
            if let Some(ticket) = self.tickets.get(id) {
                self.slots.remove(&ticket.target.storage_key());
            }
            if let Some(ticket) = self.tickets.get_mut(id) {
                if let Some(code) = ticket.short_code.take() {
                    self.short_code_index.remove(&code);
                }
                ticket.status = TicketStatus::Expired;
            }
        }
        if !ids.is_empty() {
            self.bump();
        }
        ids
    }

    /// 群生命周期事件只改变 bridge 领域所有权；本函数没有进程/tmux IO。
    /// Mirror 可直接释放；handoff/owned 必须进入 Closing 并保留 lease，等待后续 coordinator ACK。
    pub fn chat_gone(
        &mut self,
        target: &ScopedChatKey,
        reason: BridgeCloseReason,
    ) -> Result<BridgeChatGoneAction, BridgeError> {
        let slot_key = target.storage_key();
        let Some(slot) = self.slots.get(&slot_key).cloned() else {
            return Ok(BridgeChatGoneAction::None);
        };
        match slot {
            ChatSlot::Pending {
                ticket_id,
                revision,
                ..
            } => {
                let ticket = self
                    .tickets
                    .get(&ticket_id)
                    .ok_or(BridgeError::TicketMissing)?;
                if ticket.target != *target
                    || ticket.slot_revision != revision
                    || !matches!(ticket.status, TicketStatus::Pending)
                {
                    return Err(BridgeError::StaleSlot);
                }
                self.slots.remove(&slot_key);
                let ticket = self
                    .tickets
                    .get_mut(&ticket_id)
                    .ok_or(BridgeError::TicketMissing)?;
                if let Some(code) = ticket.short_code.take() {
                    self.short_code_index.remove(&code);
                }
                ticket.status = TicketStatus::Cancelled;
                self.bump();
                Ok(BridgeChatGoneAction::PendingCancelled { ticket_id })
            }
            ChatSlot::Activating(binding) | ChatSlot::MirrorBound(binding)
                if binding.mode == BridgeMode::Mirror =>
            {
                self.require_exact_lease(&binding)?;
                self.slots.remove(&slot_key);
                self.session_leases.remove(&binding.session.storage_key());
                self.purge_binding_receipts_and_turns(&binding);
                self.cancel_ticket_for_binding(&binding);
                self.bump();
                Ok(BridgeChatGoneAction::MirrorStopped(binding))
            }
            ChatSlot::HandoffPending(binding) | ChatSlot::ExternalOwned(binding)
                if binding.mode == BridgeMode::Handoff =>
            {
                self.require_exact_lease(&binding)?;
                self.slots.insert(
                    slot_key,
                    ChatSlot::Closing {
                        binding: binding.clone(),
                        reason,
                    },
                );
                self.bump();
                Ok(BridgeChatGoneAction::BindingPaused(binding))
            }
            // 重复群生命周期事件仍必须显式告诉 dispatcher“strict binding 已暂停”；若返回
            // None，调用方会误走通用 teardown，可能按 canonical 名误杀仍需 coordinator 的 tmux。
            ChatSlot::Closing { binding, .. } => {
                Ok(BridgeChatGoneAction::BindingPaused(binding.clone()))
            }
            ChatSlot::Activating(_)
            | ChatSlot::MirrorBound(_)
            | ChatSlot::HandoffPending(_)
            | ChatSlot::ExternalOwned(_) => Err(BridgeError::BindingMismatch),
        }
    }

    /// coordinator 已完成 exact staging 清理（或证明尚未启动）后的 Closing 提交点。
    /// 群生命周期事件本身只把 slot 置 Closing；没有这次 ACK 不得释放 lease。
    pub fn complete_closing(
        &mut self,
        target: &ScopedChatKey,
        binding_id: &str,
    ) -> Result<ChatBinding, BridgeError> {
        let slot_key = target.storage_key();
        let binding = match self.slots.get(&slot_key) {
            Some(ChatSlot::Closing { binding, .. })
                if binding.id == binding_id && binding.target == *target =>
            {
                binding.clone()
            }
            _ => return Err(BridgeError::BindingMismatch),
        };
        self.require_exact_lease(&binding)?;
        self.slots.remove(&slot_key);
        self.session_leases.remove(&binding.session.storage_key());
        self.purge_binding_receipts_and_turns(&binding);
        self.cancel_ticket_for_binding(&binding);
        self.bump();
        Ok(binding)
    }

    fn cancel_ticket_for_binding(&mut self, binding: &ChatBinding) {
        if let Some(ticket) = self.tickets.values_mut().find(|ticket| {
            ticket.target == binding.target
                && matches!(
                    &ticket.status,
                    TicketStatus::Activating { binding_id, .. }
                        | TicketStatus::Committed { binding_id, .. }
                        if binding_id == &binding.id
                )
        }) {
            if let Some(code) = ticket.short_code.take() {
                self.short_code_index.remove(&code);
            }
            ticket.status = TicketStatus::Cancelled;
        }
    }

    fn purge_binding_receipts_and_turns(&mut self, binding: &ChatBinding) {
        self.delivery_sequence_cursors.remove(&binding.id);
        self.activation_audit_receipts.remove(&binding.id);
        self.pending_mirror_turns.retain(|_, turn| {
            turn.binding_id != binding.id
                || turn.binding_revision != binding.revision
                || turn.target != binding.target
        });
        self.mirror_export_receipts.retain(|_, receipt| {
            receipt.binding_id != binding.id
                || receipt.binding_revision != binding.revision
                || receipt.target != binding.target
        });
        self.context_export_receipts.retain(|_, receipt| {
            receipt.binding_id != binding.id
                || receipt.binding_revision != binding.revision
                || receipt.target != binding.target
        });
        if let Some(record) = self.sessions.get_mut(&binding.session.storage_key()) {
            let current_generation = record.generation;
            let should_clear = record.pending_prompt.as_ref().is_some_and(|prompt| {
                prompt.content_permit.as_ref().is_some_and(|permit| {
                    permit.binding_id == binding.id && permit.binding_revision == binding.revision
                }) && (prompt.generation == 0 || prompt.generation == current_generation)
            });
            if should_clear {
                record.pending_prompt = None;
            }
        }
        let should_clear_continuation = self
            .continuation_prompts
            .get(&binding.session.storage_key())
            .is_some_and(|prompt| {
                prompt.content_permit.as_ref().is_some_and(|permit| {
                    permit.binding_id == binding.id && permit.binding_revision == binding.revision
                })
            });
        if should_clear_continuation {
            self.continuation_prompts
                .remove(&binding.session.storage_key());
        }
    }

    fn remove_tickets_for_binding(&mut self, binding: &ChatBinding) {
        let ids = self
            .tickets
            .iter()
            .filter(|(_, ticket)| {
                ticket.target == binding.target
                    && matches!(
                        &ticket.status,
                        TicketStatus::Activating { binding_id, .. }
                            | TicketStatus::Committed { binding_id, .. }
                            if binding_id == &binding.id
                    )
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(ticket) = self.tickets.remove(&id) {
                if let Some(code) = ticket.short_code {
                    self.short_code_index.remove(&code);
                }
            }
        }
    }

    pub fn stop_mirror(
        &mut self,
        target: &ScopedChatKey,
        expected_binding_id: &str,
        expected_binding_revision: u64,
    ) -> Result<ChatBinding, BridgeError> {
        let slot_key = target.storage_key();
        let binding = match self.slots.get(&slot_key) {
            Some(ChatSlot::MirrorBound(binding))
                if binding.id == expected_binding_id
                    && binding.revision == expected_binding_revision =>
            {
                binding.clone()
            }
            _ => return Err(BridgeError::BindingMismatch),
        };
        match self.session_leases.get(&binding.session.storage_key()) {
            Some(lease) if lease.binding_id == binding.id && lease.target == binding.target => {}
            _ => return Err(BridgeError::BindingMismatch),
        }
        self.slots.remove(&slot_key);
        self.session_leases.remove(&binding.session.storage_key());
        self.purge_binding_receipts_and_turns(&binding);
        self.remove_tickets_for_binding(&binding);
        self.bump();
        Ok(binding)
    }

    /// 只接受同一 generation + 同一 process 的结束事件；迟到事件不改状态。
    pub fn end_session(
        &mut self,
        key: &SessionKey,
        generation: u64,
        process: Option<&ProcessFingerprint>,
        now_ms: u64,
    ) -> LifecycleApply {
        let Some(record) = self.sessions.get_mut(&key.storage_key()) else {
            return LifecycleApply::Missing;
        };
        if record.generation != generation {
            return LifecycleApply::StaleGeneration;
        }
        if process.is_none_or(|process| !process.is_complete())
            || record.process.as_ref() != process
            || record
                .process
                .as_ref()
                .is_none_or(|process| !process.is_complete())
        {
            return LifecycleApply::StaleProcess;
        }
        record.lifecycle = ExternalLifecycle::Ended;
        record.last_seen_ms = now_ms;
        record.pending_prompt = None;
        self.continuation_prompts.remove(&key.storage_key());
        self.bump();
        LifecycleApply::Applied
    }

    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

fn prompt_ids_match(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left == right)
        || matches!((left, right), (None, None))
}

fn mirror_origin_verified(observation: &ExternalHookObservation) -> bool {
    observation.resume_profile.process_verified
        && observation.process.as_ref().is_some_and(|process| {
            process.is_complete() && process.executable == observation.resume_profile.claude_bin
        })
}

fn record_accepts_authorized_generation(record: &ExternalSessionRecord, generation: u64) -> bool {
    // Pending turn/context receipt 形成时已经过 active process + permit + exact binding 校验。
    // 进程随后 Ended 不应吞掉已授权内容；只禁止“未来 generation”伪造回旧 binding。
    generation > 0 && generation <= record.generation
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_BIN: &str = "/opt/bot/.local/share/claude/versions/2.1.220";
    const EXPECTED_HOME: &str = "/opt/bot";
    const EXPECTED_CONFIG: &str = "/opt/bot/.claude";

    fn expected_identity() -> ExpectedResumeIdentity<'static> {
        ExpectedResumeIdentity {
            claude_bin: EXPECTED_BIN,
            home: EXPECTED_HOME,
            config_dir: EXPECTED_CONFIG,
            provider: ClaudeProvider::Default,
        }
    }

    fn matching_profile() -> ResumeProfile {
        ResumeProfile {
            claude_bin: EXPECTED_BIN.into(),
            home: EXPECTED_HOME.into(),
            config_dir: EXPECTED_CONFIG.into(),
            process_verified: true,
            environment_verified: true,
            config_source: ClaudeConfigSource::Default,
            provider: ClaudeProvider::Default,
            ..ResumeProfile::default()
        }
    }

    #[test]
    fn identical_resume_identity_has_no_mismatch() {
        assert!(diff_resume_identity(&matching_profile(), &expected_identity()).is_empty());
        assert_eq!(describe_resume_identity_mismatches(&[]), "");
    }

    /// (期望点名的字段, 把该字段改坏, 改坏后的实际值)。
    type BrokenIdentityCase = (&'static str, fn(&mut ResumeProfile), &'static str);

    /// 逐字段各改坏一次:必须**只**点名那一个字段,且文案里带得出期望值/实际值。
    #[test]
    fn each_broken_field_is_named_alone_with_expected_and_actual() {
        let cases: [BrokenIdentityCase; 7] = [
            (
                "claude_bin",
                |profile| profile.claude_bin = "/private/var/folders/T/shim/claude".into(),
                "/private/var/folders/T/shim/claude",
            ),
            (
                "home",
                |profile| profile.home = "/other/home".into(),
                "/other/home",
            ),
            (
                "config_dir",
                |profile| profile.config_dir = "/other/.claude".into(),
                "/other/.claude",
            ),
            (
                "config_source",
                |profile| profile.config_source = ClaudeConfigSource::Explicit,
                "Explicit",
            ),
            (
                "provider",
                |profile| profile.provider = ClaudeProvider::Custom,
                "Custom",
            ),
            (
                "process_verified",
                |profile| profile.process_verified = false,
                "false",
            ),
            (
                "environment_verified",
                |profile| profile.environment_verified = false,
                "false",
            ),
        ];
        for (field, break_it, actual) in cases {
            let mut profile = matching_profile();
            break_it(&mut profile);
            let mismatches = diff_resume_identity(&profile, &expected_identity());
            assert_eq!(
                mismatches.iter().map(|m| m.field).collect::<Vec<_>>(),
                vec![field],
                "只应点名 {field}"
            );
            assert_eq!(mismatches[0].actual, actual);
            let text = describe_resume_identity_mismatches(&mismatches);
            assert!(text.contains(field), "文案应点名字段:{text}");
            assert!(text.contains(actual), "文案应给出实际值:{text}");
            assert!(!mismatches[0].expected.is_empty(), "文案应给出期望值");
        }
    }

    #[test]
    fn all_broken_fields_are_reported_together() {
        let profile = ResumeProfile {
            provider: ClaudeProvider::Custom,
            ..ResumeProfile::default()
        };
        let mismatches = diff_resume_identity(&profile, &expected_identity());
        assert_eq!(
            mismatches.iter().map(|m| m.field).collect::<Vec<_>>(),
            vec![
                "claude_bin",
                "home",
                "config_dir",
                "config_source",
                "provider",
                "process_verified",
                "environment_verified",
            ]
        );
    }

    /// 诊断只吐路径/枚举/布尔:哪怕上游把 token 形态的串塞进别的字段,也不会被这里打出来。
    #[test]
    fn mismatch_text_only_carries_the_compared_identity_fields() {
        let token = "a".repeat(64);
        let mut profile = matching_profile();
        profile.claude_bin = "/other/claude".into();
        profile.version = token.clone();
        profile.safe_args = vec!["--model".into(), token.clone()];
        profile.unsupported_args = vec![token.clone()];
        let text = describe_resume_identity_mismatches(&diff_resume_identity(
            &profile,
            &expected_identity(),
        ));
        assert!(
            !text.contains(&token),
            "诊断文案不得带出 token 形态字符串:{text}"
        );
        assert!(text.contains("claude_bin"));
    }

    #[test]
    fn oversized_identity_value_is_truncated_for_logs() {
        let mut profile = matching_profile();
        profile.home = "x".repeat(MAX_IDENTITY_VALUE_CHARS + 50);
        let mismatches = diff_resume_identity(&profile, &expected_identity());
        assert_eq!(mismatches.len(), 1);
        assert_eq!(
            mismatches[0].actual.chars().count(),
            MAX_IDENTITY_VALUE_CHARS + 1,
            "截断后应只多一个省略号"
        );
        assert!(mismatches[0].actual.ends_with('…'));
    }

    fn process(pid: u32, started: &str) -> ProcessFingerprint {
        ProcessFingerprint {
            pid,
            start_time: started.into(),
            executable: "/opt/claude".into(),
        }
    }

    fn session(sid: &str, revision: u64, lifecycle: ExternalLifecycle) -> ExternalSessionRecord {
        let key = SessionKey::new("/tmp/config", sid);
        ExternalSessionRecord {
            key: key.clone(),
            generation: 1,
            locator: ExternalLocator {
                key,
                transcript_path: format!("/tmp/config/projects/demo/{sid}.jsonl"),
                resume_lookup_cwd: "/tmp/demo".into(),
                current_cwd: "/tmp/demo/src".into(),
                profile: ResumeProfile {
                    claude_bin: "/opt/claude".into(),
                    home: "/tmp/home".into(),
                    config_dir: "/tmp/config".into(),
                    version: "2.1.207".into(),
                    safe_args: vec!["--model".into(), "sonnet".into()],
                    unsupported_args: vec![],
                    process_verified: true,
                    environment_verified: true,
                    config_source: ClaudeConfigSource::Default,
                    provider: ClaudeProvider::Default,
                },
                revision,
            },
            process: Some(process(10, "start-a")),
            lifecycle,
            first_seen_ms: 1,
            last_seen_ms: 2,
            last_applied_sequence: 1,
            pending_prompt: None,
        }
    }

    fn target(chat: &str) -> ScopedChatKey {
        ScopedChatKey::new("app-a", "bot", chat)
    }

    fn ticket_request(id: &str, chat: &str, session: &ExternalSessionRecord) -> TicketRequest {
        TicketRequest {
            ticket_id: id.into(),
            short_code: Some(format!("CODE-{id}")),
            target: target(chat),
            target_chat_type: "group".into(),
            target_provider: ClaudeProvider::Default,
            requester_open_id: "ou_owner".into(),
            requester_union_id: "on_owner".into(),
            choices: vec![BridgeChoice {
                id: format!("choice-{id}"),
                session: session.key.clone(),
                registry_revision: session.locator.revision,
            }],
            issued_at_ms: 10,
            expires_at_ms: 100,
        }
    }

    fn prepare_ticket(state: &mut BridgeState, id: &str, chat: &str, rec: &ExternalSessionRecord) {
        state.reserve_ticket(ticket_request(id, chat, rec)).unwrap();
        state
            .attach_control_card(id, "ou_owner", Some("oc_private"), &format!("om-{id}"))
            .unwrap();
    }

    #[test]
    fn takeover_requires_observed_default_config_but_not_source_version() {
        let mut record = session("sid-profile", 7, ExternalLifecycle::ActiveIdle);
        record.locator.profile.version.clear();
        assert_eq!(record.handoff_eligibility(), HandoffEligibility::Eligible);

        record.locator.profile.config_source = ClaudeConfigSource::Explicit;
        assert_eq!(
            record.handoff_eligibility(),
            HandoffEligibility::MirrorOnly(HandoffBlock::CustomConfig)
        );

        record.locator.profile.config_source = ClaudeConfigSource::Default;
        record.locator.profile.provider = ClaudeProvider::Unknown;
        assert_eq!(
            record.handoff_eligibility(),
            HandoffEligibility::MirrorOnly(HandoffBlock::UnverifiedProvider)
        );
    }

    #[test]
    fn provider_classification_never_needs_secret_values() {
        assert_eq!(
            classify_claude_provider([("PATH", "/usr/bin"), ("ANTHROPIC_BASE_URL", "")]),
            ClaudeProvider::Default
        );
        assert_eq!(
            classify_claude_provider([
                ("ANTHROPIC_BASE_URL", "https://proxy.example"),
                ("ANTHROPIC_AUTH_TOKEN", "secret"),
            ]),
            ClaudeProvider::Custom
        );
    }

    fn record_audit(state: &mut BridgeState, ticket_id: &str, binding_id: &str) -> String {
        let ticket = state.tickets[ticket_id].clone();
        let binding = state
            .slot(&ticket.target)
            .and_then(ChatSlot::binding)
            .unwrap()
            .clone();
        let receipt_id = format!("audit-receipt-{binding_id}");
        let sequence = state
            .allocate_activation_delivery_sequence(ticket_id, binding_id)
            .unwrap();
        state
            .record_activation_audit(ActivationAuditReceipt {
                id: receipt_id.clone(),
                outbox_id: format!("audit-outbox-{binding_id}"),
                ticket_id: ticket_id.into(),
                binding_id: binding_id.into(),
                binding_revision: binding.revision,
                target: binding.target.clone(),
                session: binding.session.clone(),
                generation: state.session(&binding.session).unwrap().generation,
                sequence,
                delivered_at_ms: 21,
            })
            .unwrap();
        receipt_id
    }

    #[test]
    fn ticket_reservation_is_single_per_scoped_chat() {
        let mut state = BridgeState::default();
        let rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        state
            .reserve_ticket(ticket_request("t1", "oc_group", &rec))
            .unwrap();
        assert_eq!(
            state.reserve_ticket(ticket_request("t2", "oc_group", &rec)),
            Err(BridgeError::ChatOccupied)
        );
    }

    #[test]
    fn session_gc_requires_matching_generation_and_no_durable_references() {
        fn assert_protected(mut state: BridgeState, key: &SessionKey) {
            assert!(state.session_is_referenced(key));
            let generation = state.session(key).unwrap().generation;
            assert_eq!(
                state.remove_unreferenced_session(key, generation),
                Err(BridgeError::SessionLeased)
            );
            assert!(state.session(key).is_some());
        }

        let rec = session("sid-gc", 7, ExternalLifecycle::Ended);

        let mut ticket_state = BridgeState::default();
        ticket_state.upsert_session(rec.clone());
        ticket_state
            .reserve_ticket(ticket_request("ticket-gc", "oc_gc", &rec))
            .unwrap();
        assert_protected(ticket_state, &rec.key);

        let mut turn_state = BridgeState::default();
        turn_state.upsert_session(rec.clone());
        turn_state.pending_mirror_turns.insert(
            "turn-gc".into(),
            PendingMirrorTurn {
                id: "turn-gc".into(),
                session: rec.key.clone(),
                generation: rec.generation,
                binding_id: "binding-gc".into(),
                binding_revision: 1,
                target: target("oc_gc"),
                target_chat_type: "group".into(),
                observed_at_ms: 20,
                sequence: 1,
                messages: vec![],
            },
        );
        assert_protected(turn_state, &rec.key);

        let mut audit_state = BridgeState::default();
        audit_state.upsert_session(rec.clone());
        audit_state.activation_audit_receipts.insert(
            "binding-gc".into(),
            ActivationAuditReceipt {
                id: "audit-gc".into(),
                outbox_id: "outbox-audit-gc".into(),
                ticket_id: "ticket-gc".into(),
                binding_id: "binding-gc".into(),
                binding_revision: 1,
                target: target("oc_gc"),
                session: rec.key.clone(),
                generation: rec.generation,
                sequence: 1,
                delivered_at_ms: 20,
            },
        );
        assert_protected(audit_state, &rec.key);

        let mut mirror_state = BridgeState::default();
        mirror_state.upsert_session(rec.clone());
        mirror_state.mirror_export_receipts.insert(
            "mirror-gc".into(),
            MirrorExportReceipt {
                id: "mirror-gc".into(),
                outbox_id: "outbox-mirror-gc".into(),
                turn_id: "turn-gc".into(),
                binding_id: "binding-gc".into(),
                binding_revision: 1,
                target: target("oc_gc"),
                session: rec.key.clone(),
                generation: rec.generation,
                sequence: 2,
                delivered_at_ms: None,
            },
        );
        assert_protected(mirror_state, &rec.key);

        let mut context_state = BridgeState::default();
        context_state.upsert_session(rec.clone());
        context_state.context_export_receipts.insert(
            "context-gc".into(),
            ContextExportReceipt {
                id: "context-gc".into(),
                outbox_id: "outbox-context-gc".into(),
                ticket_id: "ticket-gc".into(),
                choice_id: "choice-gc".into(),
                requester_open_id: "ou_owner".into(),
                binding_id: "binding-gc".into(),
                binding_revision: 1,
                target: target("oc_gc"),
                target_chat_type: "group".into(),
                session: rec.key.clone(),
                generation: rec.generation,
                sequence: 3,
                delivered_at_ms: None,
            },
        );
        assert_protected(context_state, &rec.key);

        let mut unreferenced = BridgeState::default();
        let mut newer = rec.clone();
        newer.generation = 2;
        unreferenced.upsert_session(newer.clone());
        assert_eq!(
            unreferenced.remove_unreferenced_session(&newer.key, 1),
            Ok(false)
        );
        assert!(unreferenced.session(&newer.key).is_some());
        assert_eq!(
            unreferenced.remove_unreferenced_session(&newer.key, newer.generation),
            Ok(true)
        );
        assert!(unreferenced.session(&newer.key).is_none());
    }

    #[test]
    fn session_lease_allows_exactly_one_group() {
        let mut state = BridgeState::default();
        let rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        prepare_ticket(&mut state, "t1", "oc_a", &rec);
        prepare_ticket(&mut state, "t2", "oc_b", &rec);
        state
            .begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                20,
            )
            .unwrap();
        assert_eq!(
            state.begin_choice(
                "t2",
                "choice-t2",
                "ou_owner",
                "om-t2",
                BridgeMode::Mirror,
                "bind-b",
                20,
            ),
            Err(BridgeError::SessionLeased)
        );
    }

    #[test]
    fn stale_revision_and_replayed_ticket_are_rejected() {
        let mut state = BridgeState::default();
        let rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        prepare_ticket(&mut state, "t1", "oc_a", &rec);
        let mut changed = rec.clone();
        changed.locator.revision = 8;
        state.upsert_session(changed);
        assert_eq!(
            state.begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                20,
            ),
            Err(BridgeError::StaleSession)
        );

        let mut state = BridgeState::default();
        state.upsert_session(rec.clone());
        prepare_ticket(&mut state, "t1", "oc_a", &rec);
        let first = state
            .begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                20,
            )
            .unwrap();
        assert!(matches!(first, BeginChoiceOutcome::Started(_)));
        let replay = state
            .begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                101,
            )
            .unwrap();
        assert!(matches!(replay, BeginChoiceOutcome::AlreadyStarted(_)));
        assert_eq!(
            state.begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Handoff,
                "bind-other",
                21,
            ),
            Err(BridgeError::TicketFinal)
        );
    }

    #[test]
    fn old_generation_end_cannot_stop_new_process() {
        let mut state = BridgeState::default();
        let mut rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        rec.generation = 2;
        rec.process = Some(process(11, "new-start"));
        state.upsert_session(rec.clone());
        assert_eq!(
            state.end_session(&rec.key, 1, Some(&process(10, "old-start")), 9),
            LifecycleApply::StaleGeneration
        );
        assert_eq!(
            state.session(&rec.key).unwrap().lifecycle,
            ExternalLifecycle::ActiveIdle
        );
        assert_eq!(
            state.end_session(&rec.key, 2, Some(&process(11, "wrong-start")), 9),
            LifecycleApply::StaleProcess
        );
        assert_eq!(
            state.end_session(&rec.key, 2, Some(&process(11, "new-start")), 9),
            LifecycleApply::Applied
        );
        assert_eq!(
            state.session(&rec.key).unwrap().lifecycle,
            ExternalLifecycle::Ended
        );
    }

    #[test]
    fn source_only_launch_arguments_do_not_block_takeover() {
        let mut source_rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        source_rec.locator.profile.unsupported_args =
            vec!["<unsupported-flag>".into(), "<positional>".into()];
        assert_eq!(
            source_rec.handoff_eligibility(),
            HandoffEligibility::Eligible,
            "源 argv 不会被新 owner 重放，不能越过已验证的 session/process/transcript 事实"
        );
        source_rec.lifecycle = ExternalLifecycle::Conflict;
        assert_eq!(
            source_rec.handoff_eligibility(),
            HandoffEligibility::MirrorOnly(HandoffBlock::SessionConflict)
        );
    }

    #[test]
    fn active_busy_session_can_reserve_remote_handoff() {
        let record = session("sid-busy", 7, ExternalLifecycle::ActiveBusy);
        assert_eq!(record.handoff_eligibility(), HandoffEligibility::Eligible);

        let mut state = BridgeState::default();
        state.upsert_session(record.clone());
        prepare_ticket(&mut state, "th-busy", "oc-busy", &record);
        let proof = HandoffBeginProof {
            session: record.key.clone(),
            generation: record.generation,
            registry_revision: record.locator.revision,
            process: record.process.clone().unwrap(),
        };
        assert!(matches!(
            state
                .begin_handoff(
                    "th-busy",
                    "choice-th-busy",
                    "ou_owner",
                    "om-th-busy",
                    "bind-busy",
                    20,
                    &proof,
                )
                .unwrap(),
            BeginChoiceOutcome::Started(_)
        ));
        assert!(matches!(
            state.slot(&target("oc-busy")),
            Some(ChatSlot::HandoffPending(_))
        ));
    }

    #[test]
    fn safe_candidate_and_serde_do_not_expose_private_locator() {
        let rec = session("secret-sid", 7, ExternalLifecycle::ActiveIdle);
        let candidate = rec.safe_candidate("opaque-choice".into());
        let json = serde_json::to_string(&candidate).unwrap();
        assert!(json.contains("opaque-choice"));
        assert!(json.contains("src"));
        for secret in ["secret-sid", "/tmp/config", "jsonl", "/opt/claude"] {
            assert!(!json.contains(secret), "candidate 泄露了 {secret}");
        }
        let state_json = serde_json::to_string(&BridgeState::default()).unwrap();
        let back: BridgeState = serde_json::from_str(&state_json).unwrap();
        assert_eq!(back, BridgeState::default());
    }

    #[test]
    fn candidate_project_name_strips_markdown_bidi_and_bounds_output() {
        let mut rec = session("sid", 7, ExternalLifecycle::ActiveIdle);
        rec.locator.current_cwd = format!("/tmp/\u{202e}[点我](https://evil)#`{}", "x".repeat(200));
        let name = rec.safe_candidate("choice".into()).project_name;
        assert!(name.chars().count() <= 80);
        assert!(name.len() <= 240);
        for deceptive in ['\u{202e}', '[', ']', '(', ')', '#', '`'] {
            assert!(!name.contains(deceptive));
        }
    }

    #[test]
    fn control_card_attachment_is_first_write_wins() {
        let mut state = BridgeState::default();
        let rec = session("sid", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        state
            .reserve_ticket(ticket_request("t1", "oc", &rec))
            .unwrap();
        state
            .attach_control_card("t1", "ou_owner", Some("private-a"), "message-a")
            .unwrap();
        state
            .attach_control_card("t1", "ou_owner", Some("private-a"), "message-a")
            .unwrap();
        assert_eq!(
            state.attach_control_card("t1", "ou_owner", Some("private-b"), "message-b",),
            Err(BridgeError::BindingMismatch)
        );
    }

    #[test]
    fn proactive_card_first_callback_claims_private_chat_once() {
        let mut state = BridgeState::default();
        let rec = session("sid", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        state
            .reserve_ticket(ticket_request("t1", "oc", &rec))
            .unwrap();
        state
            .attach_control_card("t1", "ou_owner", None, "message-a")
            .unwrap();
        state
            .attach_control_card("t1", "ou_owner", Some("private-a"), "message-a")
            .unwrap();
        assert_eq!(
            state.tickets["t1"].private_chat_id.as_deref(),
            Some("private-a")
        );
        assert_eq!(
            state.attach_control_card("t1", "ou_owner", Some("private-b"), "message-a"),
            Err(BridgeError::BindingMismatch)
        );
    }

    #[test]
    fn short_code_is_scoped_to_requester_and_consumed_by_card_attachment() {
        let mut state = BridgeState::default();
        let rec = session("sid", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        state
            .reserve_ticket(ticket_request("t1", "oc", &rec))
            .unwrap();

        let ticket = state
            .redeem_short_code("code-t1", "app-a", "ou_owner", 20)
            .unwrap();
        assert_eq!(ticket.id, "t1");
        assert_eq!(
            state.redeem_short_code("CODE-t1", "app-b", "ou_owner", 20),
            Err(BridgeError::ShortCodeInvalid)
        );
        assert_eq!(
            state.redeem_short_code("CODE-t1", "app-a", "ou_other", 20),
            Err(BridgeError::ShortCodeInvalid)
        );

        state
            .attach_control_card("t1", "ou_owner", None, "message-a")
            .unwrap();
        assert_eq!(
            state.redeem_short_code("CODE-t1", "app-a", "ou_owner", 20),
            Err(BridgeError::ShortCodeInvalid)
        );
        assert!(!state.short_code_index.contains_key("CODE-T1"));
    }

    #[test]
    fn short_code_expiry_and_cancellation_release_index_and_slot() {
        let mut state = BridgeState::default();
        let rec = session("sid", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        state
            .reserve_ticket(ticket_request("t1", "oc-a", &rec))
            .unwrap();
        assert_eq!(
            state.redeem_short_code("CODE-t1", "app-a", "ou_owner", 101),
            Err(BridgeError::ShortCodeInvalid)
        );
        assert_eq!(state.expire_pending(101), vec!["t1".to_string()]);
        assert!(!state.short_code_index.contains_key("CODE-T1"));
        assert!(state.slot(&target("oc-a")).is_none());

        state
            .reserve_ticket(ticket_request("t2", "oc-b", &rec))
            .unwrap();
        state.cancel_ticket("t2").unwrap();
        assert!(!state.short_code_index.contains_key("CODE-T2"));
        assert!(state.slot(&target("oc-b")).is_none());
    }

    #[test]
    fn chat_gone_cancels_pending_and_is_idempotent() {
        let mut state = BridgeState::default();
        let rec = session("sid", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        state
            .reserve_ticket(ticket_request("t1", "oc", &rec))
            .unwrap();

        assert_eq!(
            state
                .chat_gone(&target("oc"), BridgeCloseReason::BotRemoved)
                .unwrap(),
            BridgeChatGoneAction::PendingCancelled {
                ticket_id: "t1".into()
            }
        );
        assert!(state.slot(&target("oc")).is_none());
        assert!(!state.short_code_index.contains_key("CODE-T1"));
        assert_eq!(
            state
                .chat_gone(&target("oc"), BridgeCloseReason::BotRemoved)
                .unwrap(),
            BridgeChatGoneAction::None
        );
    }

    #[test]
    fn chat_gone_stops_mirror_but_pauses_handoff_without_releasing_lease() {
        let mut mirror = BridgeState::default();
        let rec = session("sid-m", 7, ExternalLifecycle::ActiveIdle);
        mirror.upsert_session(rec.clone());
        prepare_ticket(&mut mirror, "tm", "oc-m", &rec);
        let binding = match mirror
            .begin_choice(
                "tm",
                "choice-tm",
                "ou_owner",
                "om-tm",
                BridgeMode::Mirror,
                "bind-m",
                20,
            )
            .unwrap()
        {
            BeginChoiceOutcome::Started(binding) => binding,
            BeginChoiceOutcome::AlreadyStarted(_) => unreachable!(),
        };
        record_audit(&mut mirror, "tm", "bind-m");
        mirror
            .complete_binding("tm", "bind-m", BridgeMode::Mirror)
            .unwrap();
        assert_eq!(
            mirror
                .chat_gone(&target("oc-m"), BridgeCloseReason::ChatDisbanded)
                .unwrap(),
            BridgeChatGoneAction::MirrorStopped(binding.clone())
        );
        assert!(mirror.slot(&target("oc-m")).is_none());
        assert!(!mirror
            .session_leases
            .contains_key(&binding.session.storage_key()));

        let mut handoff = BridgeState::default();
        let ended = session("sid-h", 7, ExternalLifecycle::Ended);
        handoff.upsert_session(ended.clone());
        prepare_ticket(&mut handoff, "th", "oc-h", &ended);
        assert_eq!(
            handoff.begin_choice(
                "th",
                "choice-th",
                "ou_owner",
                "om-th",
                BridgeMode::Handoff,
                "bind-h",
                20,
            ),
            Err(BridgeError::HandoffCapabilityRequired)
        );
        // ChatGone 仍需兼容未来 T6 或重启时已经持久化的 HandoffPending；直接构造该状态，
        // 证明群事件只暂停且保留 lease，不为 T4 打开通用 handoff 入口。
        let handoff_binding = ChatBinding {
            id: "bind-h".into(),
            target: target("oc-h"),
            target_chat_type: "group".into(),
            session: ended.key.clone(),
            mode: BridgeMode::Handoff,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 20,
            revision: 21,
        };
        handoff.slots.insert(
            handoff_binding.target.storage_key(),
            ChatSlot::HandoffPending(handoff_binding.clone()),
        );
        handoff.session_leases.insert(
            handoff_binding.session.storage_key(),
            SessionLease {
                binding_id: handoff_binding.id.clone(),
                target: handoff_binding.target.clone(),
            },
        );
        handoff.tickets.get_mut("th").unwrap().status = TicketStatus::Activating {
            binding_id: handoff_binding.id.clone(),
            choice_id: "choice-th".into(),
            mode: BridgeMode::Handoff,
        };
        assert_eq!(
            handoff
                .chat_gone(&target("oc-h"), BridgeCloseReason::BotRemoved)
                .unwrap(),
            BridgeChatGoneAction::BindingPaused(handoff_binding.clone())
        );
        assert!(matches!(
            handoff.slot(&target("oc-h")),
            Some(ChatSlot::Closing { binding, .. }) if binding == &handoff_binding
        ));
        assert!(handoff
            .session_leases
            .contains_key(&handoff_binding.session.storage_key()));
        assert_eq!(
            handoff
                .chat_gone(&target("oc-h"), BridgeCloseReason::BotRemoved)
                .unwrap(),
            BridgeChatGoneAction::BindingPaused(handoff_binding.clone())
        );
        assert_eq!(
            handoff.complete_closing(&target("oc-h"), "bind-h").unwrap(),
            handoff_binding
        );
        assert!(handoff.slot(&target("oc-h")).is_none());
        assert!(!handoff
            .session_leases
            .contains_key(&ended.key.storage_key()));
        assert!(matches!(
            handoff.tickets.get("th").map(|ticket| &ticket.status),
            Some(TicketStatus::Cancelled)
        ));
    }

    #[test]
    fn handoff_cannot_complete_without_typed_ack_proof() {
        let mut state = BridgeState::default();
        assert_eq!(
            state.complete_binding("missing", "binding", BridgeMode::Handoff),
            Err(BridgeError::HandoffProofRequired)
        );
    }

    #[test]
    fn dedicated_handoff_begin_requires_exact_source_snapshot() {
        let mut state = BridgeState::default();
        let record = session("sid-handoff", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(record.clone());
        prepare_ticket(&mut state, "th", "oc-handoff", &record);
        let process = record.process.clone().unwrap();
        let proof = HandoffBeginProof {
            session: record.key.clone(),
            generation: record.generation,
            registry_revision: record.locator.revision,
            process,
        };
        let binding = state
            .begin_handoff("th", "choice-th", "ou_owner", "om-th", "bind-h", 20, &proof)
            .unwrap();
        assert!(matches!(binding, BeginChoiceOutcome::Started(_)));
        assert!(matches!(
            state.slot(&target("oc-handoff")),
            Some(ChatSlot::HandoffPending(_))
        ));

        let mut stale = proof;
        stale.generation += 1;
        let mut other = BridgeState::default();
        other.upsert_session(record.clone());
        prepare_ticket(&mut other, "th2", "oc-other", &record);
        assert_eq!(
            other.begin_handoff(
                "th2",
                "choice-th2",
                "ou_owner",
                "om-th2",
                "bind-h2",
                20,
                &stale,
            ),
            Err(BridgeError::StaleSession)
        );
    }

    #[test]
    fn completed_takeover_releases_slot_but_retains_conflict_sentinel() {
        let mut state = BridgeState::default();
        let record = session("sid-takeover", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(record.clone());
        prepare_ticket(&mut state, "tt", "oc-takeover", &record);
        let proof = HandoffBeginProof {
            session: record.key.clone(),
            generation: record.generation,
            registry_revision: record.locator.revision,
            process: record.process.clone().unwrap(),
        };
        let binding = match state
            .begin_handoff(
                "tt",
                "choice-tt",
                "ou_owner",
                "om-tt",
                "bind-takeover",
                20,
                &proof,
            )
            .unwrap()
        {
            BeginChoiceOutcome::Started(binding) => binding,
            BeginChoiceOutcome::AlreadyStarted(_) => unreachable!(),
        };
        let resumed = ProcessFingerprint {
            pid: 99,
            start_time: "resumed".into(),
            executable: "/opt/claude".into(),
        };
        let current = state.sessions.get_mut(&record.key.storage_key()).unwrap();
        current.generation = 8;
        current.process = Some(resumed.clone());
        current.lifecycle = ExternalLifecycle::ActiveIdle;
        state.processed_events.insert("resume-event".into(), 33);

        let receipt = state
            .complete_handoff_takeover(
                "tt",
                &binding.id,
                "soul-bot-g123",
                8,
                &resumed,
                "resume-event",
            )
            .unwrap();
        assert_eq!(receipt.target, binding.target);
        assert!(state.slot(&binding.target).is_none());
        assert!(!state
            .session_leases
            .contains_key(&binding.session.storage_key()));
        assert!(!state.takeover_conflict(&binding.target));

        state
            .sessions
            .get_mut(&binding.session.storage_key())
            .unwrap()
            .lifecycle = ExternalLifecycle::Conflict;
        assert!(state.takeover_conflict(&binding.target));
    }

    #[test]
    fn managed_resume_updates_takeover_sentinel_and_exact_release_is_idempotent() {
        let mut state = BridgeState::default();
        let mut record = session("external-sid", 7, ExternalLifecycle::ActiveIdle);
        record.generation = 4;
        state.upsert_session(record.clone());
        prepare_ticket(&mut state, "tm", "oc-managed", &record);
        let proof = HandoffBeginProof {
            session: record.key.clone(),
            generation: record.generation,
            registry_revision: record.locator.revision,
            process: record.process.clone().unwrap(),
        };
        let binding = match state
            .begin_handoff(
                "tm",
                "choice-tm",
                "ou_owner",
                "om-tm",
                "bind-managed",
                20,
                &proof,
            )
            .unwrap()
        {
            BeginChoiceOutcome::Started(binding) => binding,
            BeginChoiceOutcome::AlreadyStarted(_) => unreachable!(),
        };
        let first_resumed = process(90, "takeover-start");
        let current = state.sessions.get_mut(&record.key.storage_key()).unwrap();
        current.generation = 5;
        current.process = Some(first_resumed.clone());
        current.lifecycle = ExternalLifecycle::ActiveIdle;
        state
            .processed_events
            .insert("takeover-start-event".into(), 33);
        let receipt = state
            .complete_handoff_takeover(
                "tm",
                &binding.id,
                "soul-bot-g456",
                5,
                &first_resumed,
                "takeover-start-event",
            )
            .unwrap();

        let ended = state.sessions.get_mut(&record.key.storage_key()).unwrap();
        ended.lifecycle = ExternalLifecycle::Ended;
        ended.process = None;
        let mut managed_resume = observation(
            "managed-resume-event",
            100,
            Some(process(91, "managed-resume-start")),
            ExternalHookEvent::SessionStart {
                source: "resume".into(),
            },
        );
        managed_resume.managed_takeover = true;
        assert_eq!(
            state.apply_external_hook(&managed_resume).unwrap(),
            ExternalHookApply::Applied
        );
        let updated = state
            .managed_takeovers
            .get(&binding.target.storage_key())
            .unwrap()
            .clone();
        assert_eq!(updated.generation, 6);
        assert_eq!(updated.process.pid, 91);
        assert_eq!(updated.session_start_event_id, "managed-resume-event");

        assert_eq!(
            state.release_managed_takeover(&receipt),
            Err(BridgeError::BindingMismatch),
            "旧 generation receipt 不得释放新哨兵"
        );
        assert_eq!(state.release_managed_takeover(&updated), Ok(true));
        assert_eq!(state.release_managed_takeover(&updated), Ok(false));
    }

    fn observation(
        event_id: &str,
        observed_at_ms: u64,
        process: Option<ProcessFingerprint>,
        event: ExternalHookEvent,
    ) -> ExternalHookObservation {
        ExternalHookObservation {
            event_id: event_id.into(),
            sequence: observed_at_ms,
            observed_at_ms,
            session: SessionKey::new("/tmp/config", "external-sid"),
            transcript_path: "/tmp/config/projects/demo/external-sid.jsonl".into(),
            cwd: "/tmp/demo".into(),
            process,
            resume_profile: ResumeProfile {
                claude_bin: "/opt/claude".into(),
                home: "/tmp/home".into(),
                config_dir: "/tmp/config".into(),
                version: "2.1.207".into(),
                safe_args: vec![],
                unsupported_args: vec![],
                process_verified: true,
                environment_verified: true,
                config_source: ClaudeConfigSource::Default,
                provider: ClaudeProvider::Default,
            },
            content_permit: None,
            managed_takeover: false,
            event,
        }
    }

    fn bind_mirror(state: &mut BridgeState, key: &SessionKey) -> ContentPermit {
        let binding = ChatBinding {
            id: "binding-a".into(),
            target: target("oc_mirror"),
            target_chat_type: "group".into(),
            session: key.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 11,
            revision: 1,
        };
        state.slots.insert(
            binding.target.storage_key(),
            ChatSlot::MirrorBound(binding.clone()),
        );
        ContentPermit {
            binding_id: binding.id,
            binding_revision: binding.revision,
            generation: state.session(key).map_or(0, |record| record.generation),
        }
    }

    #[test]
    fn hook_reducer_is_idempotent_and_emits_user_then_assistant() {
        let mut state = BridgeState::default();
        let source = process(42, "source-start");
        let start = observation(
            "start-1",
            10,
            Some(source.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        assert_eq!(
            state.apply_external_hook(&start).unwrap(),
            ExternalHookApply::Applied
        );
        let permit = bind_mirror(&mut state, &start.session);

        let mut prompt = observation(
            "prompt-1",
            20,
            Some(source.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("prompt-id".into()),
                prompt: Some("用户问题".into()),
            },
        );
        prompt.content_permit = Some(permit.clone());
        state.apply_external_hook(&prompt).unwrap();
        let mut stop = observation(
            "stop-1",
            30,
            Some(source),
            ExternalHookEvent::Stop {
                prompt_id: Some("prompt-id".into()),
                last_assistant_message: Some("最终回答".into()),
                stop_hook_active: false,
                has_pending_wakeups: false,
            },
        );
        stop.content_permit = Some(permit);
        state.apply_external_hook(&stop).unwrap();
        let turn = state.pending_mirror_turns.get("stop-1").unwrap();
        assert_eq!(
            turn.messages,
            vec![
                ExternalVisibleMessage {
                    role: ExternalVisibleRole::User,
                    text: "用户问题".into(),
                },
                ExternalVisibleMessage {
                    role: ExternalVisibleRole::Assistant,
                    text: "最终回答".into(),
                },
            ]
        );
        let revision = state.revision;
        assert_eq!(
            state.apply_external_hook(&stop).unwrap(),
            ExternalHookApply::Duplicate
        );
        assert_eq!(state.revision, revision);
        assert_eq!(state.pending_mirror_turns.len(), 1);
    }

    #[test]
    fn stop_waits_for_background_wakeup_and_mirrors_stop_hook_continuation_update() {
        let mut state = BridgeState::default();
        let source = process(42, "source-start");
        let start = observation(
            "start",
            10,
            Some(source.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let permit = bind_mirror(&mut state, &start.session);
        let mut prompt = observation(
            "prompt",
            20,
            Some(source.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: None,
                prompt: Some("等待后台任务的问题".into()),
            },
        );
        prompt.content_permit = Some(permit.clone());
        state.apply_external_hook(&prompt).unwrap();

        let mut paused = observation(
            "stop-paused",
            30,
            Some(source.clone()),
            ExternalHookEvent::Stop {
                prompt_id: None,
                last_assistant_message: Some("后台任务尚未结束".into()),
                stop_hook_active: false,
                has_pending_wakeups: true,
            },
        );
        paused.content_permit = Some(permit.clone());
        state.apply_external_hook(&paused).unwrap();
        assert!(state.pending_mirror_turns.is_empty());
        assert!(state
            .session(&start.session)
            .unwrap()
            .pending_prompt
            .is_some());
        assert_eq!(
            state.session(&start.session).unwrap().lifecycle,
            ExternalLifecycle::ActiveBusy
        );

        let mut completed = observation(
            "stop-completed",
            40,
            Some(source.clone()),
            ExternalHookEvent::Stop {
                prompt_id: None,
                last_assistant_message: Some("后台任务后的最终回答".into()),
                stop_hook_active: false,
                has_pending_wakeups: false,
            },
        );
        completed.content_permit = Some(permit.clone());
        state.apply_external_hook(&completed).unwrap();
        assert_eq!(
            state.pending_mirror_turns["stop-completed"].messages[1].text,
            "后台任务后的最终回答"
        );

        let mut continued = observation(
            "stop-hook-update",
            50,
            Some(source.clone()),
            ExternalHookEvent::Stop {
                prompt_id: None,
                last_assistant_message: Some("另一个 Stop hook 继续后的更新回答".into()),
                stop_hook_active: true,
                has_pending_wakeups: false,
            },
        );
        continued.content_permit = Some(permit.clone());
        state.apply_external_hook(&continued).unwrap();
        let update = &state.pending_mirror_turns["stop-hook-update"];
        assert_eq!(update.messages[0].text, "等待后台任务的问题");
        assert_eq!(update.messages[1].text, "另一个 Stop hook 继续后的更新回答");

        let mut next = observation(
            "next-prompt",
            60,
            Some(source),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: None,
                prompt: Some("下一轮".into()),
            },
        );
        next.content_permit = Some(permit);
        state.apply_external_hook(&next).unwrap();
        assert!(!state
            .continuation_prompts
            .contains_key(&start.session.storage_key()));
    }

    #[test]
    fn external_body_state_clipping_is_utf8_safe_and_json_bounded() {
        let body = "\0<&汉\n".repeat(20_000);
        for budget in [
            EXTERNAL_PROMPT_STATE_JSON_BUDGET,
            EXTERNAL_ASSISTANT_STATE_JSON_BUDGET,
        ] {
            let clipped = clip_external_body_for_state(&body, budget);
            assert!(serde_json::to_string(&clipped).unwrap().len() <= budget);
            assert!(clipped.ends_with(::summon_core::i18n::label(EXTERNAL_BODY_TRUNCATED_NOTICE)));
            assert!(clipped.is_char_boundary(clipped.len()));
        }
    }

    #[test]
    fn interrupted_and_failed_turns_emit_safe_terminal_instead_of_error_details() {
        let mut state = BridgeState::default();
        let source = process(42, "source-start");
        let start = observation(
            "start",
            10,
            Some(source.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let permit = bind_mirror(&mut state, &start.session);

        let mut first = observation(
            "prompt-a",
            20,
            Some(source.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("a".into()),
                prompt: Some("第一个问题".into()),
            },
        );
        first.content_permit = Some(permit.clone());
        state.apply_external_hook(&first).unwrap();
        let mut second = observation(
            "prompt-b",
            30,
            Some(source.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("b".into()),
                prompt: Some("第二个问题".into()),
            },
        );
        second.content_permit = Some(permit.clone());
        state.apply_external_hook(&second).unwrap();
        let interrupted = state.pending_mirror_turns.get("prompt-b").unwrap();
        assert_eq!(interrupted.messages[0].text, "第一个问题");
        assert_eq!(
            interrupted.messages[1].text,
            ::summon_core::i18n::label(INTERRUPTED_TERMINAL_MESSAGE)
        );

        let mut failed = observation(
            "failed-b",
            40,
            Some(source),
            ExternalHookEvent::StopFailure {
                prompt_id: Some("b".into()),
                error: Some("VERY_SECRET_ERROR".into()),
                error_details: Some("/private/path VERY_SECRET_STACK".into()),
                last_assistant_message: Some("VERY_SECRET_PARTIAL_NOT_FINAL".into()),
            },
        );
        failed.content_permit = Some(permit);
        state.apply_external_hook(&failed).unwrap();
        let terminal = state.pending_mirror_turns.get("failed-b").unwrap();
        assert_eq!(terminal.messages[0].text, "第二个问题");
        assert_eq!(
            terminal.messages[1].text,
            ::summon_core::i18n::label(INTERRUPTED_TERMINAL_MESSAGE)
        );
        let serialized = serde_json::to_string(terminal).unwrap();
        assert!(!serialized.contains("VERY_SECRET"));
        assert!(!serialized.contains("/private/path"));
    }

    #[test]
    fn session_end_finalizes_pending_prompt_and_stop_purges_exact_pending_body() {
        let mut state = BridgeState::default();
        let source = process(42, "source-start");
        let start = observation(
            "start",
            10,
            Some(source.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let permit = bind_mirror(&mut state, &start.session);
        let binding = state
            .slot(&target("oc_mirror"))
            .and_then(ChatSlot::binding)
            .unwrap()
            .clone();
        state.session_leases.insert(
            start.session.storage_key(),
            SessionLease {
                binding_id: binding.id.clone(),
                target: binding.target.clone(),
            },
        );
        let mut prompt = observation(
            "prompt",
            20,
            Some(source.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("p".into()),
                prompt: Some("结束前的问题".into()),
            },
        );
        prompt.content_permit = Some(permit.clone());
        state.apply_external_hook(&prompt).unwrap();
        assert!(state
            .session(&start.session)
            .unwrap()
            .pending_prompt
            .is_some());

        state
            .stop_mirror(&binding.target, &binding.id, binding.revision)
            .unwrap();
        assert!(state
            .session(&start.session)
            .unwrap()
            .pending_prompt
            .is_none());

        // 重建同一 mirror 验证 SessionEnd 的显式终态。
        let permit = bind_mirror(&mut state, &start.session);
        let mut prompt = observation(
            "prompt-2",
            30,
            Some(source.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("p2".into()),
                prompt: Some("会话结束问题".into()),
            },
        );
        prompt.content_permit = Some(permit);
        state.apply_external_hook(&prompt).unwrap();
        let end = observation(
            "session-end",
            40,
            Some(source),
            ExternalHookEvent::SessionEnd {
                reason: Some("exit".into()),
            },
        );
        state.apply_external_hook(&end).unwrap();
        let terminal = state.pending_mirror_turns.get("session-end").unwrap();
        assert_eq!(terminal.messages[0].text, "会话结束问题");
        assert_eq!(
            terminal.messages[1].text,
            ::summon_core::i18n::label(SESSION_ENDED_TERMINAL_MESSAGE)
        );
        let binding = state
            .slot(&target("oc_mirror"))
            .and_then(ChatSlot::binding)
            .unwrap()
            .clone();
        state.session_leases.insert(
            binding.session.storage_key(),
            SessionLease {
                binding_id: binding.id.clone(),
                target: binding.target.clone(),
            },
        );
        state
            .delivery_sequence_cursors
            .insert(binding.id.clone(), 1);
        let sequence = state
            .allocate_mirror_turn_delivery_sequence("session-end")
            .unwrap();
        assert_eq!(
            state
                .export_pending_mirror_turn(MirrorExportReceipt {
                    id: "ended-receipt".into(),
                    outbox_id: "ended-outbox".into(),
                    turn_id: "session-end".into(),
                    binding_id: binding.id.clone(),
                    binding_revision: binding.revision,
                    target: binding.target.clone(),
                    session: binding.session.clone(),
                    generation: 1,
                    sequence,
                    delivered_at_ms: None,
                })
                .unwrap(),
            MirrorExportOutcome::Exported,
            "SessionEnd 后已形成的授权终态仍必须可导出"
        );
    }

    #[test]
    fn verified_resume_preserves_mirror_and_backlog_but_rotates_content_permit() {
        let mut state = BridgeState::default();
        let old_process = process(42, "old-start");
        let start = observation(
            "start-old",
            10,
            Some(old_process.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let old_permit = bind_mirror(&mut state, &start.session);
        let mut prompt = observation(
            "prompt-old",
            20,
            Some(old_process.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("old".into()),
                prompt: Some("旧 epoch 已授权问题".into()),
            },
        );
        prompt.content_permit = Some(old_permit.clone());
        state.apply_external_hook(&prompt).unwrap();
        state
            .apply_external_hook(&observation(
                "end-old",
                30,
                Some(old_process),
                ExternalHookEvent::SessionEnd {
                    reason: Some("exit".into()),
                },
            ))
            .unwrap();
        assert!(state.pending_mirror_turns.contains_key("end-old"));

        let new_process = process(43, "new-start");
        state
            .apply_external_hook(&observation(
                "start-resume",
                40,
                Some(new_process.clone()),
                ExternalHookEvent::SessionStart {
                    source: "resume".into(),
                },
            ))
            .unwrap();
        assert_eq!(state.session(&start.session).unwrap().generation, 2);
        assert!(matches!(
            state.slot(&target("oc_mirror")),
            Some(ChatSlot::MirrorBound(_))
        ));
        assert!(
            state.pending_mirror_turns.contains_key("end-old"),
            "verified resume 不得吞掉离线 backlog"
        );

        let mut stale_prompt = observation(
            "stale-permit",
            50,
            Some(new_process.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("stale".into()),
                prompt: Some("不得采样".into()),
            },
        );
        stale_prompt.content_permit = Some(old_permit.clone());
        state.apply_external_hook(&stale_prompt).unwrap();
        assert!(state
            .session(&start.session)
            .unwrap()
            .pending_prompt
            .is_none());

        let current_permit = ContentPermit {
            generation: 2,
            ..old_permit
        };
        let mut current_prompt = observation(
            "current-prompt",
            60,
            Some(new_process.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("current".into()),
                prompt: Some("新 epoch 问题".into()),
            },
        );
        current_prompt.content_permit = Some(current_permit.clone());
        state.apply_external_hook(&current_prompt).unwrap();
        let mut stop = observation(
            "current-stop",
            70,
            Some(new_process),
            ExternalHookEvent::Stop {
                prompt_id: Some("current".into()),
                last_assistant_message: Some("新 epoch 回答".into()),
                stop_hook_active: false,
                has_pending_wakeups: false,
            },
        );
        stop.content_permit = Some(current_permit);
        state.apply_external_hook(&stop).unwrap();
        assert_eq!(state.pending_mirror_turns["current-stop"].generation, 2);
    }

    #[test]
    fn verified_resume_preserves_handoff_reservation_until_exact_commit() {
        let mut state = BridgeState::default();
        let old_process = process(42, "old-start");
        let start = observation(
            "start-old",
            10,
            Some(old_process.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let record = state.session(&start.session).unwrap().clone();
        prepare_ticket(&mut state, "t-handoff", "oc_handoff", &record);
        let proof = HandoffBeginProof {
            session: record.key.clone(),
            generation: record.generation,
            registry_revision: record.locator.revision,
            process: old_process.clone(),
        };
        let binding = match state
            .begin_handoff(
                "t-handoff",
                "choice-t-handoff",
                "ou_owner",
                "om-t-handoff",
                "binding-handoff",
                20,
                &proof,
            )
            .unwrap()
        {
            BeginChoiceOutcome::Started(binding) => binding,
            BeginChoiceOutcome::AlreadyStarted(_) => panic!("first handoff cannot be idempotent"),
        };
        state
            .apply_external_hook(&observation(
                "end-old",
                30,
                Some(old_process),
                ExternalHookEvent::SessionEnd { reason: None },
            ))
            .unwrap();
        let resumed = process(43, "new-start");
        state
            .apply_external_hook(&observation(
                "start-resume",
                40,
                Some(resumed.clone()),
                ExternalHookEvent::SessionStart {
                    source: "resume".into(),
                },
            ))
            .unwrap();

        assert_eq!(state.session(&start.session).unwrap().generation, 2);
        assert!(matches!(
            state.slot(&binding.target),
            Some(ChatSlot::HandoffPending(current)) if current == &binding
        ));
        assert!(state
            .session_leases
            .get(&start.session.storage_key())
            .is_some_and(|lease| lease.binding_id == binding.id && lease.target == binding.target));
        assert!(matches!(
            state.tickets["t-handoff"].status,
            TicketStatus::Activating {
                mode: BridgeMode::Handoff,
                ..
            }
        ));

        let receipt = state
            .complete_handoff_takeover(
                "t-handoff",
                &binding.id,
                "soul-bot-gverified-resume",
                2,
                &resumed,
                "start-resume",
            )
            .unwrap();
        assert_eq!(receipt.session, start.session);
        assert_eq!(receipt.generation, 2);
        assert!(state.slot(&binding.target).is_none());
        assert!(!state
            .session_leases
            .contains_key(&start.session.storage_key()));
        assert_eq!(
            state.managed_takeovers.get(&binding.target.storage_key()),
            Some(&receipt)
        );
    }

    #[test]
    fn non_resume_or_unverified_rollover_releases_old_mirror() {
        for (source, verified_process) in [("startup", true), ("resume", false)] {
            let mut state = BridgeState::default();
            let old_process = process(42, "old-start");
            let start = observation(
                "start-old",
                10,
                Some(old_process.clone()),
                ExternalHookEvent::SessionStart {
                    source: "startup".into(),
                },
            );
            state.apply_external_hook(&start).unwrap();
            bind_mirror(&mut state, &start.session);
            state
                .apply_external_hook(&observation(
                    "end-old",
                    20,
                    Some(old_process),
                    ExternalHookEvent::SessionEnd { reason: None },
                ))
                .unwrap();
            let process = verified_process.then(|| process(43, "new-start"));
            state
                .apply_external_hook(&observation(
                    "start-new",
                    30,
                    process,
                    ExternalHookEvent::SessionStart {
                        source: source.into(),
                    },
                ))
                .unwrap();
            assert!(state.slot(&target("oc_mirror")).is_none());
        }
    }

    #[test]
    fn generation_rollover_always_cancels_incomplete_activation() {
        let mut state = BridgeState::default();
        let old_process = process(42, "old-start");
        let start = observation(
            "start-old",
            10,
            Some(old_process.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let record = state.session(&start.session).unwrap().clone();
        prepare_ticket(&mut state, "t-roll", "oc_roll", &record);
        state
            .begin_choice(
                "t-roll",
                "choice-t-roll",
                "ou_owner",
                "om-t-roll",
                BridgeMode::Mirror,
                "binding-roll",
                20,
            )
            .unwrap();
        state
            .apply_external_hook(&observation(
                "end-old",
                30,
                Some(old_process),
                ExternalHookEvent::SessionEnd { reason: None },
            ))
            .unwrap();
        state
            .apply_external_hook(&observation(
                "start-resume",
                40,
                Some(process(43, "new-start")),
                ExternalHookEvent::SessionStart {
                    source: "resume".into(),
                },
            ))
            .unwrap();
        assert!(state.slot(&target("oc_roll")).is_none());
        assert!(!state.tickets.contains_key("t-roll"));
        assert!(!state
            .session_leases
            .contains_key(&start.session.storage_key()));
    }

    #[test]
    fn late_stop_does_not_consume_newer_pending_prompt() {
        let mut state = BridgeState::default();
        let source = process(42, "source-start");
        let start = observation(
            "start",
            10,
            Some(source.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let permit = bind_mirror(&mut state, &start.session);
        let mut prompt = observation(
            "prompt-new",
            20,
            Some(source.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("new".into()),
                prompt: Some("新问题".into()),
            },
        );
        prompt.content_permit = Some(permit.clone());
        state.apply_external_hook(&prompt).unwrap();
        let mut late = observation(
            "stop-old",
            30,
            Some(source),
            ExternalHookEvent::Stop {
                prompt_id: Some("old".into()),
                last_assistant_message: Some("旧回答".into()),
                stop_hook_active: false,
                has_pending_wakeups: false,
            },
        );
        late.content_permit = Some(permit);
        state.apply_external_hook(&late).unwrap();
        let record = state.session(&start.session).unwrap();
        assert_eq!(record.lifecycle, ExternalLifecycle::ActiveBusy);
        assert_eq!(
            record.pending_prompt.as_ref().unwrap().prompt_id.as_deref(),
            Some("new")
        );
        assert!(!state.pending_mirror_turns.contains_key("stop-old"));
    }

    #[test]
    fn old_process_session_end_cannot_end_new_generation() {
        let mut state = BridgeState::default();
        let old = process(42, "old-start");
        let new = process(43, "new-start");
        let start_old = observation(
            "start-old",
            10,
            Some(old.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start_old).unwrap();
        let end_old = observation(
            "end-old",
            20,
            Some(old.clone()),
            ExternalHookEvent::SessionEnd {
                reason: Some("prompt_input_exit".into()),
            },
        );
        state.apply_external_hook(&end_old).unwrap();
        let start_new = observation(
            "start-new",
            30,
            Some(new),
            ExternalHookEvent::SessionStart {
                source: "resume".into(),
            },
        );
        state.apply_external_hook(&start_new).unwrap();
        let late_old_end = observation(
            "late-end-old",
            21,
            Some(old),
            ExternalHookEvent::SessionEnd { reason: None },
        );
        assert_eq!(
            state.apply_external_hook(&late_old_end).unwrap(),
            ExternalHookApply::Stale
        );
        let record = state.session(&start_new.session).unwrap();
        assert_eq!(record.generation, 2);
        assert_eq!(record.lifecycle, ExternalLifecycle::ActiveIdle);
    }

    #[test]
    fn competing_live_process_marks_session_conflict() {
        let mut state = BridgeState::default();
        let first = observation(
            "start-a",
            10,
            Some(process(42, "start-a")),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&first).unwrap();
        let competing = observation(
            "start-b",
            11,
            Some(process(43, "start-b")),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        assert_eq!(
            state.apply_external_hook(&competing).unwrap(),
            ExternalHookApply::Conflict
        );
        assert_eq!(
            state.session(&first.session).unwrap().lifecycle,
            ExternalLifecycle::Conflict
        );
    }

    #[test]
    fn unverified_none_process_is_not_bindable_and_second_writer_conflicts() {
        let mut state = BridgeState::default();
        let first = observation(
            "start-unverified",
            10,
            None,
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&first).unwrap();
        assert_eq!(
            state.session(&first.session).unwrap().lifecycle,
            ExternalLifecycle::Unverified
        );
        let second = observation(
            "start-unknown-writer",
            11,
            None,
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        assert_eq!(
            state.apply_external_hook(&second).unwrap(),
            ExternalHookApply::Conflict
        );
    }

    #[test]
    fn lower_sequence_event_cannot_roll_back_lifecycle_or_locator() {
        let mut state = BridgeState::default();
        let source = process(42, "start");
        let start = observation(
            "start",
            10,
            Some(source.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let prompt = observation(
            "prompt-newer",
            30,
            Some(source.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: None,
                prompt: None,
            },
        );
        state.apply_external_hook(&prompt).unwrap();
        let mut stale_cwd = observation(
            "cwd-older",
            40,
            Some(source),
            ExternalHookEvent::CwdChanged {
                old_cwd: Some("/tmp/demo".into()),
                new_cwd: "/tmp/rolled-back".into(),
            },
        );
        stale_cwd.sequence = 20;
        assert_eq!(
            state.apply_external_hook(&stale_cwd).unwrap(),
            ExternalHookApply::Stale
        );
        let record = state.session(&start.session).unwrap();
        assert_eq!(record.lifecycle, ExternalLifecycle::ActiveBusy);
        assert_eq!(record.locator.current_cwd, "/tmp/demo");
    }

    #[test]
    fn processed_event_index_is_bounded() {
        let mut state = BridgeState::default();
        for sequence in 1..=8_300 {
            let event = observation(
                &format!("event-{sequence}"),
                sequence,
                Some(process(42, "start")),
                ExternalHookEvent::CwdChanged {
                    old_cwd: None,
                    new_cwd: "/tmp/demo".into(),
                },
            );
            state.apply_external_hook(&event).unwrap();
        }
        assert_eq!(state.processed_events.len(), 8_192);
        assert!(!state.processed_events.contains_key("event-1"));
        assert!(state.processed_events.contains_key("event-8300"));
    }

    #[test]
    fn stale_content_permit_cannot_deliver_into_rebound_chat() {
        let mut state = BridgeState::default();
        let source = process(42, "source-start");
        let start = observation(
            "start",
            10,
            Some(source.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let old_permit = bind_mirror(&mut state, &start.session);
        let mut prompt = observation(
            "prompt",
            20,
            Some(source.clone()),
            ExternalHookEvent::UserPromptSubmit {
                prompt_id: Some("p1".into()),
                prompt: Some("旧绑定正文".into()),
            },
        );
        prompt.content_permit = Some(old_permit.clone());
        state.apply_external_hook(&prompt).unwrap();

        let new_binding = ChatBinding {
            id: "binding-new".into(),
            target: target("oc_mirror"),
            target_chat_type: "group".into(),
            session: start.session.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 21,
            revision: 2,
        };
        state.slots.insert(
            new_binding.target.storage_key(),
            ChatSlot::MirrorBound(new_binding),
        );
        let mut stop = observation(
            "stop",
            30,
            Some(source),
            ExternalHookEvent::Stop {
                prompt_id: Some("p1".into()),
                last_assistant_message: Some("旧绑定回答".into()),
                stop_hook_active: false,
                has_pending_wakeups: false,
            },
        );
        stop.content_permit = Some(old_permit);
        state.apply_external_hook(&stop).unwrap();
        assert!(state.pending_mirror_turns.is_empty());
    }

    #[test]
    fn committed_ticket_replays_cannot_mutate_or_return_replacement_binding() {
        let mut state = BridgeState::default();
        let rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        prepare_ticket(&mut state, "t1", "oc_a", &rec);
        let first = match state
            .begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                20,
            )
            .unwrap()
        {
            BeginChoiceOutcome::Started(binding) => binding,
            BeginChoiceOutcome::AlreadyStarted(_) => unreachable!(),
        };
        record_audit(&mut state, "t1", "bind-a");
        state
            .complete_binding("t1", "bind-a", BridgeMode::Mirror)
            .unwrap();
        state
            .stop_mirror(&first.target, &first.id, first.revision)
            .unwrap();

        prepare_ticket(&mut state, "t2", "oc_a", &rec);
        let second = match state
            .begin_choice(
                "t2",
                "choice-t2",
                "ou_owner",
                "om-t2",
                BridgeMode::Mirror,
                "bind-b",
                30,
            )
            .unwrap()
        {
            BeginChoiceOutcome::Started(binding) => binding,
            BeginChoiceOutcome::AlreadyStarted(_) => unreachable!(),
        };
        record_audit(&mut state, "t2", "bind-b");
        state
            .complete_binding("t2", "bind-b", BridgeMode::Mirror)
            .unwrap();

        assert_eq!(state.cancel_ticket("t1"), Err(BridgeError::TicketMissing));
        assert_eq!(
            state.begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                31,
            ),
            Err(BridgeError::TicketMissing)
        );
        assert_eq!(
            state.complete_binding("t1", "bind-a", BridgeMode::Mirror),
            Err(BridgeError::TicketMissing)
        );
        assert_eq!(
            state.stop_mirror(&second.target, &first.id, first.revision),
            Err(BridgeError::BindingMismatch)
        );
        assert_eq!(
            state.slot(&second.target).and_then(ChatSlot::binding),
            Some(&second)
        );
    }

    #[test]
    fn authorized_control_recovery_atomically_transfers_operator_and_card() {
        let mut state = BridgeState::default();
        let rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        prepare_ticket(&mut state, "t1", "oc_a", &rec);
        state
            .begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                20,
            )
            .unwrap();
        let audit = record_audit(&mut state, "t1", "bind-a");
        state
            .complete_mirror_after_audit("t1", "bind-a", &audit)
            .unwrap();

        state
            .transfer_bound_control_card(
                "t1",
                "bind-a",
                "ou_new_operator",
                "on_new_operator",
                "oc_new_private",
                "om_new_card",
            )
            .unwrap();
        let ticket = &state.tickets["t1"];
        assert_eq!(ticket.requester_open_id, "ou_new_operator");
        assert_eq!(ticket.requester_union_id, "on_new_operator");
        assert_eq!(ticket.private_chat_id.as_deref(), Some("oc_new_private"));
        assert_eq!(
            ticket.control_card_message_id.as_deref(),
            Some("om_new_card")
        );
        let binding = state
            .slot(&target("oc_a"))
            .and_then(ChatSlot::binding)
            .unwrap();
        assert_eq!(binding.actor_open_id, "ou_new_operator");
        assert_eq!(
            state.transfer_bound_control_card(
                "t1",
                "wrong-binding",
                "ou_attacker",
                "",
                "oc_attacker",
                "om_attacker",
            ),
            Err(BridgeError::BindingMismatch)
        );
        assert_eq!(state.tickets["t1"].requester_open_id, "ou_new_operator");
    }

    #[test]
    fn mirror_complete_rechecks_session_revision_and_lease() {
        let mut state = BridgeState::default();
        let rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        prepare_ticket(&mut state, "t1", "oc_a", &rec);
        state
            .begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                20,
            )
            .unwrap();
        record_audit(&mut state, "t1", "bind-a");
        let mut changed = rec;
        changed.locator.revision = 8;
        state.upsert_session(changed);
        assert_eq!(
            state.complete_binding("t1", "bind-a", BridgeMode::Mirror),
            Err(BridgeError::StaleSession)
        );
    }

    #[test]
    fn mirror_cannot_complete_before_activation_audit_receipt() {
        let mut state = BridgeState::default();
        let rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        prepare_ticket(&mut state, "t1", "oc_a", &rec);
        state
            .begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                20,
            )
            .unwrap();

        assert_eq!(
            state.complete_binding("t1", "bind-a", BridgeMode::Mirror),
            Err(BridgeError::ActivationAuditRequired)
        );
        assert!(matches!(
            state.slot(&target("oc_a")),
            Some(ChatSlot::Activating(_))
        ));
    }

    #[test]
    fn pending_turn_export_is_atomic_idempotent_and_stop_purges_all_artifacts() {
        let mut state = BridgeState::default();
        let rec = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        state.upsert_session(rec.clone());
        prepare_ticket(&mut state, "t1", "oc_a", &rec);
        let binding = match state
            .begin_choice(
                "t1",
                "choice-t1",
                "ou_owner",
                "om-t1",
                BridgeMode::Mirror,
                "bind-a",
                20,
            )
            .unwrap()
        {
            BeginChoiceOutcome::Started(binding) => binding,
            BeginChoiceOutcome::AlreadyStarted(_) => unreachable!(),
        };
        let audit_id = record_audit(&mut state, "t1", "bind-a");
        state
            .complete_mirror_after_audit("t1", "bind-a", &audit_id)
            .unwrap();
        state.pending_mirror_turns.insert(
            "turn-1".into(),
            PendingMirrorTurn {
                id: "turn-1".into(),
                session: binding.session.clone(),
                generation: 1,
                binding_id: binding.id.clone(),
                binding_revision: binding.revision,
                target: binding.target.clone(),
                target_chat_type: binding.target_chat_type.clone(),
                observed_at_ms: 30,
                sequence: 30,
                messages: vec![
                    ExternalVisibleMessage {
                        role: ExternalVisibleRole::User,
                        text: "secret-user".into(),
                    },
                    ExternalVisibleMessage {
                        role: ExternalVisibleRole::Assistant,
                        text: "secret-assistant".into(),
                    },
                ],
            },
        );
        assert_eq!(
            state.allocate_context_delivery_sequence(&binding.id, binding.revision),
            Err(BridgeError::PendingMirrorTurns),
            "context 不得越过尚未导出的旧 turn"
        );
        let delivery_sequence = state
            .allocate_mirror_turn_delivery_sequence("turn-1")
            .unwrap();
        let receipt = MirrorExportReceipt {
            id: "receipt-1".into(),
            outbox_id: "outbox-1".into(),
            turn_id: "turn-1".into(),
            binding_id: binding.id.clone(),
            binding_revision: binding.revision,
            target: binding.target.clone(),
            session: binding.session.clone(),
            generation: 1,
            sequence: delivery_sequence,
            delivered_at_ms: None,
        };

        assert_eq!(
            state.export_pending_mirror_turn(receipt.clone()).unwrap(),
            MirrorExportOutcome::Exported
        );
        assert!(!state.pending_mirror_turns.contains_key("turn-1"));
        assert_eq!(
            state.export_pending_mirror_turn(receipt.clone()).unwrap(),
            MirrorExportOutcome::AlreadyExported
        );
        let context_sequence = state
            .allocate_context_delivery_sequence(&binding.id, binding.revision)
            .unwrap();
        assert_eq!(
            context_sequence, 3,
            "即使旧 turn 在 outbox backoff，context 也必须排在它之后"
        );
        state
            .record_context_export(ContextExportReceipt {
                id: "context-receipt".into(),
                outbox_id: "context-outbox".into(),
                ticket_id: "t1".into(),
                choice_id: "choice-t1".into(),
                requester_open_id: "ou_owner".into(),
                binding_id: binding.id.clone(),
                binding_revision: binding.revision,
                target: binding.target.clone(),
                target_chat_type: binding.target_chat_type.clone(),
                session: binding.session.clone(),
                generation: 1,
                sequence: context_sequence,
                delivered_at_ms: None,
            })
            .unwrap();

        state
            .stop_mirror(&binding.target, &binding.id, binding.revision)
            .unwrap();
        assert!(!state.tickets.contains_key("t1"));
        assert!(state.pending_mirror_turns.is_empty());
        assert!(state.mirror_export_receipts.is_empty());
        assert!(state.context_export_receipts.is_empty());
        assert!(state.activation_audit_receipts.is_empty());
        assert!(!state
            .session_leases
            .contains_key(&binding.session.storage_key()));
    }

    #[test]
    fn unverified_session_end_never_ends_an_active_generation() {
        let mut state = BridgeState::default();
        let mut record = session("sid-a", 7, ExternalLifecycle::ActiveIdle);
        record.generation = 2;
        record.process = None;
        state.upsert_session(record.clone());
        assert_eq!(
            state.end_session(&record.key, 2, None, 20),
            LifecycleApply::StaleProcess
        );
        assert_eq!(
            state.session(&record.key).unwrap().lifecycle,
            ExternalLifecycle::ActiveIdle
        );
    }

    #[test]
    fn session_end_preserves_verified_resume_identity() {
        let mut state = BridgeState::default();
        let source = process(42, "source-start");
        let start = observation(
            "start-preserve-profile",
            10,
            Some(source.clone()),
            ExternalHookEvent::SessionStart {
                source: "startup".into(),
            },
        );
        state.apply_external_hook(&start).unwrap();
        let expected_locator = state.session(&start.session).unwrap().locator.clone();

        let mut end = observation(
            "end-preserve-profile",
            20,
            Some(source),
            ExternalHookEvent::SessionEnd { reason: None },
        );
        end.resume_profile.claude_bin = "/tmp/unverified-wrapper".into();
        end.resume_profile.version.clear();
        end.resume_profile.process_verified = false;
        end.resume_profile.environment_verified = false;
        state.apply_external_hook(&end).unwrap();

        let ended = state.session(&start.session).unwrap();
        assert_eq!(ended.lifecycle, ExternalLifecycle::Ended);
        assert_eq!(
            ended.locator.transcript_path,
            expected_locator.transcript_path
        );
        assert_eq!(
            ended.locator.resume_lookup_cwd,
            expected_locator.resume_lookup_cwd
        );
        assert_eq!(ended.locator.profile, expected_locator.profile);
        assert_eq!(ended.locator.current_cwd, end.cwd);
        assert_eq!(ended.last_applied_sequence, end.sequence);
    }

    #[test]
    fn storage_keys_are_unambiguous() {
        assert_ne!(
            SessionKey::new("a", "bc").storage_key(),
            SessionKey::new("ab", "c").storage_key()
        );
        assert_ne!(
            ScopedChatKey::new("a", "bc", "d").storage_key(),
            ScopedChatKey::new("ab", "c", "d").storage_key()
        );
    }
}
