//! Codex App Server 的窄客户端。
//!
//! summon 通过 App Server 的 JSON-RPC 协议与 Codex CLI 通信：chat 侧捕获 turn 通知与用户输入，
//! 观测侧读取 `account/rateLimits` 快照。这里仅实现所需的方法/通知，不把完整且仍在
//! 演进的协议复制进 summon。所有未知字段都容忍，方法缺失/超时则显式返回错误。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use summon_core::codex_policy::CodexPolicy;
use summon_core::codex_quota::CodexRateLimitSnapshot;
use summon_core::codex_rpc::{parse_app_rate_limit_snapshot, CodexRpcParseError};
use summon_core::core_port::AskQuestion;
use summon_core::driver::CodexServiceTier;

use crate::runtime::executor::ChildKiller;
// 传输层(换行分帧的共享写端 / stderr 留尾 / 进程组信号)与 cursor ACP 客户端共用一份,
// 见 `runtime::rpc_stdio` 模块头:这些都是踩过的坑,抄第二份必然漂。
use crate::runtime::rpc_stdio::{
    is_server_request, signal_group, ReaderEvent, RpcStdioError, RpcSteerOutcome, RpcWriter,
    StderrTail, INTERRUPT_WRITE_WAIT, SOFT_INTERRUPT_GRACE, STDERR_EOF_GRACE,
};

/// 软打断的三种结局是**传输/进程层**的概念(codex 与 cursor 两条 RPC 路逐字同义),故住在
/// `runtime::rpc_stdio`。这里原样再导出,既有调用点路径不变。
pub use crate::runtime::rpc_stdio::RpcInterruptOutcome;
/// 协议的**纯数据面**(通知枚举 + 其解析)已下沉 `summon-core`:它零 IO,而且要被 chat 侧的
/// RPC 捕获引擎复用(core 不能反向依赖 summon)。这里原样再导出,`codex::codex_app_server::X`
/// 这条路径上的调用点一个字都不用改。
pub use summon_core::codex_rpc::{CodexAppServerNotification, CodexAppServerTurn};

/// 传输层错误 → 本客户端错误面。**取值与抽出共享传输层之前逐条相同**:调用方看到的
/// operation 串、错误分类都不变。
impl From<RpcStdioError> for CodexAppServerError {
    fn from(error: RpcStdioError) -> Self {
        match error {
            RpcStdioError::Serialize(detail) => Self::InvalidResponse {
                operation: "serialize request".into(),
                detail,
            },
            RpcStdioError::Closed => Self::Disconnected {
                operation: "write".into(),
            },
            RpcStdioError::Io(error) => Self::Io(error),
            RpcStdioError::WriteBusy => Self::Timeout {
                operation: summon_i18n::text!("r7a3fe2bd61").into(),
            },
        }
    }
}

impl From<CodexRpcParseError> for CodexAppServerError {
    fn from(error: CodexRpcParseError) -> Self {
        Self::InvalidResponse {
            operation: error.operation,
            detail: error.detail,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodexAppServerConfig {
    pub program: String,
    pub args: Vec<String>,
    pub work_dir: Option<PathBuf>,
    pub request_timeout: Duration,
    /// 临时任务的进程截止时刻，包含握手、协议读写与模型执行。
    pub process_deadline: Option<Instant>,
    pub client_name: String,
    pub client_title: String,
    pub client_version: String,
    /// 是否在 initialize 时声明 `capabilities.experimentalApi=true`。默认关：后台观测/Menu Bar
    /// 只用稳定只读方法；chat RpcPool 为 thread goal/background terminals 等当前协议方法显式开。
    pub experimental_api: bool,
    /// 子进程的**完整**环境。spawn 侧**无条件** `env_clear`,给空就是空环境,不是继承父进程。
    ///
    /// 生产的 chat 路径必须给全,理由同 `CursorAcpConfig::env`:继承 daemon 环境会把陈旧的
    /// `SUMMON_*` 身份带进子进程,导致子进程里的 CLI 按别的 agent 身份行事。
    pub env: Vec<(String, String)>,
}

impl Default for CodexAppServerConfig {
    fn default() -> Self {
        Self {
            program: summon_core::tools::resolve(summon_core::tools::Tool::Codex)
                .to_string_lossy()
                .into_owned(),
            // 后台观测 client 不向人发卡，保持 feature 关闭；只有 chat RpcPool 的显式
            // args 才启用 default_mode_request_user_input，避免后台任务突然开始等人工输入。
            args: vec!["app-server".into(), "--stdio".into()],
            work_dir: None,
            request_timeout: Duration::from_secs(10),
            process_deadline: None,
            client_name: "summon".into(),
            client_title: "Summon".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            experimental_api: false,
            // 默认给**系统基线**(不是空、也不是继承):spawn 侧现在无条件 `env_clear`,
            // 给空就等于让子进程连 PATH 都没有。基线里已排除身份三键,所以后台观测这类
            // 非 chat 路径不会莫名带上某个 agent 的身份。chat 路径另由 `RpcPool::child_env` 给全。
            env: crate::runtime::daemon::essential_system_env(),
        }
    }
}

#[derive(Debug)]
pub enum CodexAppServerError {
    Spawn(std::io::Error),
    Io(std::io::Error),
    Timeout {
        operation: String,
    },
    Disconnected {
        operation: String,
    },
    InvalidJson(String),
    InvalidResponse {
        operation: String,
        detail: String,
    },
    /// 进程已经死了。「断开」两个字什么都不解释,真正的原因(登录过期、配置坏、参数不认)
    /// 只写在 stderr,所以这里必须把尾部一起带出去。
    ProcessFailed {
        operation: String,
        detail: String,
        stderr_tail: String,
    },
    CapabilityUnavailable {
        method: String,
        detail: String,
    },
    Remote {
        method: String,
        code: Option<i64>,
        message: String,
    },
}

impl fmt::Display for CodexAppServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "{}", summon_i18n::format!("r49c00dae11")),
            Self::Io(error) => write!(f, "{}", summon_i18n::format!("r4983574f60")),
            Self::Timeout { operation } => write!(f, "{}", summon_i18n::format!("r7582967d74")),
            Self::Disconnected { operation } => {
                write!(f, "{}", summon_i18n::format!("rf6de0fc3cf"))
            }
            Self::InvalidJson(detail) => write!(f, "{}", summon_i18n::format!("rf3b0169006")),
            Self::InvalidResponse { operation, detail } => {
                write!(f, "{}", summon_i18n::format!("ra78ded9058"))
            }
            Self::ProcessFailed {
                operation,
                detail,
                stderr_tail,
            } => write!(f, "{}", summon_i18n::format!("re3638649a9")),
            Self::CapabilityUnavailable { method, detail } => {
                write!(f, "{}", summon_i18n::format!("r8dcfada022"))
            }
            Self::Remote {
                method,
                code,
                message,
            } => write!(
                f,
                "{}",
                summon_i18n::format!(
                    "rcfbcdbec1f",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                )
            ),
        }
    }
}

impl std::error::Error for CodexAppServerError {}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexAccountRateLimits {
    pub default: CodexRateLimitSnapshot,
    pub by_limit_id: BTreeMap<String, CodexRateLimitSnapshot>,
}

impl CodexAccountRateLimits {
    pub fn codex(&self) -> Option<&CodexRateLimitSnapshot> {
        self.by_limit_id.get("codex").or_else(|| {
            (self.default.limit_id == "codex" || self.default.limit_id.is_empty())
                .then_some(&self.default)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAppServerThread {
    pub id: String,
    /// App Server 的用户可见标题；不同于通常取首条 prompt 的 `preview`。
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub forked_from_id: Option<String>,
    #[serde(default)]
    pub status: Option<Value>,
    #[serde(default)]
    pub turns: Vec<CodexAppServerTurn>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAppServerThreadList {
    #[serde(default)]
    pub data: Vec<CodexAppServerThread>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// backend 自报的模型目录。只留 summon 用得到的两项:面板候选与 backend 自己的默认模型。
/// 协议里的 `description` / `supportedReasoningEfforts` / `serviceTiers` 目前没有消费方,
/// 不搬进来 —— 协议还在演进,搬多少就得跟多少。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexModelCatalog {
    /// 可见模型 id,保 backend 给的顺序(它把主推的排在前面)。
    pub models: Vec<String>,
    /// backend 标了 `isDefault` 的那个 id。
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexModelListPage {
    #[serde(default)]
    data: Vec<CodexModelEntry>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexModelEntry {
    id: String,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    is_default: bool,
}

/// 目录一页要多少条,以及最多翻几页。上限防的是目录失控把一次只读查询拖成长任务;
/// 本机 2026-09-05 实测 codex 7 条、traex 19 条,都是一页回完。
const MODEL_LIST_PAGE_LIMIT: u32 = 200;
const MODEL_LIST_MAX_PAGES: usize = 5;

/// `model/list` 的 params。单独成函数是为了让协议漂移测试**从它取键**:内联构造的字段
/// 一旦拼错 camelCase,app-server 会静默忽略然后回全量(hidden 的也在里面),没有任何报错。
fn model_list_params(cursor: Option<&str>) -> Value {
    let mut params = json!({
        "includeHidden": false,
        "limit": MODEL_LIST_PAGE_LIMIT,
    });
    if let Some(cursor) = cursor {
        params["cursor"] = Value::String(cursor.to_string());
    }
    params
}

/// Codex server→client request 的 JSON-RPC id。协议只允许 string / int64；保留原类型，
/// 回答时才能逐字节等价地关联到同一条 request。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodexServerRequestId {
    String(String),
    Integer(i64),
}

/// `item/tool/requestUserInput` 映射后的稳定输入面。Codex 专有的 question id 按题序单独
/// 保存；展示所需字段进入通用 [`AskQuestion`]，避免卡片层依赖 provider wire schema。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUserInputRequest {
    pub id: CodexServerRequestId,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub question_ids: Vec<String>,
    pub questions: Vec<AskQuestion>,
    pub is_blocking: bool,
    /// deprecated 字段仍保留供上层观测；是否限时等待由上层自己的策略决定。
    pub auto_resolution_ms: Option<u64>,
}

/// `item/tool/call` 映射后的动态工具调用。`arguments` 保留为 JSON，具体工具在受控 adapter
/// 内自行校验；传输层只负责 request 归属、单次应答与敏感信息不落日志。
#[derive(Debug, Clone, PartialEq)]
pub struct CodexDynamicToolRequest {
    pub id: CodexServerRequestId,
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub namespace: Option<String>,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexApprovalKind {
    CommandExecution,
    FileChange,
    Permissions,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexApprovalRequest {
    pub id: CodexServerRequestId,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub kind: CodexApprovalKind,
    pub reason: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub requested_permissions: Option<Value>,
    pub available_decisions: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexApprovalDecision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

/// 聊天捕获层提供的人机问答处理器。返回值与 `request.questions` 严格同序；空的内层
/// `Vec` 表示该题被跳过。`cancelled` 是本轮外部打断，`session_alive` 由 reader EOF 清零；
/// handler 必须同时观察二者，避免 app-server 已死而飞书 gate 永久等待。诊断字符串只用于
/// 结构化拒绝，不得塞入题面。
pub trait CodexUserInputHandler: Send + Sync {
    fn handle_user_input(
        &self,
        request: &CodexUserInputRequest,
        cancelled: &AtomicBool,
        session_alive: &AtomicBool,
    ) -> Result<Vec<Vec<String>>, String>;
}

/// 聊天捕获层提供的动态工具处理器。实现只暴露显式注册的窄能力，不能把任意命令或 RPC
/// 方法透传给模型。
pub trait CodexDynamicToolHandler: Send + Sync {
    fn handle_dynamic_tool(&self, request: &CodexDynamicToolRequest) -> Result<String, String>;
}

pub trait CodexApprovalHandler: Send + Sync {
    fn handle_approval(
        &self,
        request: &CodexApprovalRequest,
        cancelled: &AtomicBool,
        session_alive: &AtomicBool,
    ) -> CodexApprovalDecision;
}

impl<F> CodexApprovalHandler for F
where
    F: Fn(&CodexApprovalRequest, &AtomicBool, &AtomicBool) -> CodexApprovalDecision + Send + Sync,
{
    fn handle_approval(
        &self,
        request: &CodexApprovalRequest,
        cancelled: &AtomicBool,
        session_alive: &AtomicBool,
    ) -> CodexApprovalDecision {
        self(request, cancelled, session_alive)
    }
}

impl<F> CodexDynamicToolHandler for F
where
    F: Fn(&CodexDynamicToolRequest) -> Result<String, String> + Send + Sync,
{
    fn handle_dynamic_tool(&self, request: &CodexDynamicToolRequest) -> Result<String, String> {
        self(request)
    }
}

impl<F> CodexUserInputHandler for F
where
    F: Fn(&CodexUserInputRequest, &AtomicBool, &AtomicBool) -> Result<Vec<Vec<String>>, String>
        + Send
        + Sync,
{
    fn handle_user_input(
        &self,
        request: &CodexUserInputRequest,
        cancelled: &AtomicBool,
        session_alive: &AtomicBool,
    ) -> Result<Vec<Vec<String>>, String> {
        self(request, cancelled, session_alive)
    }
}

#[derive(Debug)]
struct ParsedCodexUserInputRequest {
    request: CodexUserInputRequest,
    contains_secret: bool,
}

#[derive(Debug, Deserialize)]
struct CodexUserInputEnvelope {
    id: CodexServerRequestId,
    method: String,
    params: CodexUserInputParams,
}

#[derive(Debug, Deserialize)]
struct CodexDynamicToolEnvelope {
    id: CodexServerRequestId,
    method: String,
    params: CodexDynamicToolParams,
}

#[derive(Debug, Deserialize)]
struct CodexApprovalEnvelope {
    id: CodexServerRequestId,
    method: String,
    params: CodexApprovalParams,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexApprovalParams {
    thread_id: String,
    turn_id: String,
    item_id: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    cwd: Option<Value>,
    #[serde(default)]
    permissions: Option<Value>,
    #[serde(default)]
    available_decisions: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexDynamicToolParams {
    thread_id: String,
    turn_id: String,
    call_id: String,
    #[serde(default)]
    namespace: Option<String>,
    tool: String,
    arguments: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexUserInputParams {
    thread_id: String,
    turn_id: String,
    item_id: String,
    questions: Vec<CodexUserInputQuestion>,
    #[serde(default = "default_true")]
    is_blocking: bool,
    #[serde(default)]
    auto_resolution_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexUserInputQuestion {
    id: String,
    #[serde(default)]
    header: Option<String>,
    question: String,
    #[serde(default)]
    is_other: bool,
    #[serde(default)]
    is_secret: bool,
    #[serde(default)]
    options: Option<Vec<CodexUserInputOption>>,
}

#[derive(Debug, Deserialize)]
struct CodexUserInputOption {
    label: String,
    #[serde(default)]
    description: String,
}

const fn default_true() -> bool {
    true
}

fn parse_codex_user_input_request(value: &Value) -> Result<ParsedCodexUserInputRequest, String> {
    let envelope: CodexUserInputEnvelope =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    if envelope.method != "item/tool/requestUserInput" {
        return Err("unexpected server request method".into());
    }
    if !(1..=3).contains(&envelope.params.questions.len()) {
        return Err("questions must contain between one and three entries".into());
    }
    let mut seen_question_ids = BTreeSet::new();
    for question in &envelope.params.questions {
        if question.id.trim().is_empty() {
            return Err("question id must not be empty".into());
        }
        if !seen_question_ids.insert(question.id.clone()) {
            return Err("question ids must be unique".into());
        }
    }

    let contains_secret = envelope
        .params
        .questions
        .iter()
        .any(|question| question.is_secret);
    let mut question_ids = Vec::with_capacity(envelope.params.questions.len());
    let mut questions = Vec::with_capacity(envelope.params.questions.len());
    for question in envelope.params.questions {
        question_ids.push(question.id);
        let options = question.options.unwrap_or_default();
        questions.push(AskQuestion {
            question: question.question,
            header: question.header,
            options: options.iter().map(|option| option.label.clone()).collect(),
            descriptions: options
                .into_iter()
                .map(|option| option.description)
                .collect(),
            allow_other: question.is_other,
            multi_select: false,
        });
    }

    Ok(ParsedCodexUserInputRequest {
        request: CodexUserInputRequest {
            id: envelope.id,
            thread_id: envelope.params.thread_id,
            turn_id: envelope.params.turn_id,
            item_id: envelope.params.item_id,
            question_ids,
            questions,
            is_blocking: envelope.params.is_blocking,
            auto_resolution_ms: envelope.params.auto_resolution_ms,
        },
        contains_secret,
    })
}

fn parse_codex_dynamic_tool_request(value: &Value) -> Result<CodexDynamicToolRequest, String> {
    let envelope: CodexDynamicToolEnvelope =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    if envelope.method != "item/tool/call" {
        return Err("unexpected server request method".into());
    }
    for (field, value) in [
        ("threadId", envelope.params.thread_id.as_str()),
        ("turnId", envelope.params.turn_id.as_str()),
        ("callId", envelope.params.call_id.as_str()),
        ("tool", envelope.params.tool.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("{field} must not be empty"));
        }
    }
    Ok(CodexDynamicToolRequest {
        id: envelope.id,
        thread_id: envelope.params.thread_id,
        turn_id: envelope.params.turn_id,
        call_id: envelope.params.call_id,
        namespace: envelope.params.namespace,
        tool: envelope.params.tool,
        arguments: envelope.params.arguments,
    })
}

fn parse_codex_approval_request(value: &Value) -> Result<CodexApprovalRequest, String> {
    let envelope: CodexApprovalEnvelope =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    let kind = match envelope.method.as_str() {
        "item/commandExecution/requestApproval" => CodexApprovalKind::CommandExecution,
        "item/fileChange/requestApproval" => CodexApprovalKind::FileChange,
        "item/permissions/requestApproval" => CodexApprovalKind::Permissions,
        _ => return Err("unexpected approval request method".into()),
    };
    for (field, value) in [
        ("threadId", envelope.params.thread_id.as_str()),
        ("turnId", envelope.params.turn_id.as_str()),
        ("itemId", envelope.params.item_id.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("{field} must not be empty"));
        }
    }
    if kind == CodexApprovalKind::Permissions && envelope.params.permissions.is_none() {
        return Err("permissions request is missing permissions".into());
    }
    let cwd = envelope.params.cwd.and_then(|value| match value {
        Value::String(value) => Some(value),
        other => other.as_object().and_then(|object| {
            object
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
    });
    Ok(CodexApprovalRequest {
        id: envelope.id,
        thread_id: envelope.params.thread_id,
        turn_id: envelope.params.turn_id,
        item_id: envelope.params.item_id,
        kind,
        reason: envelope.params.reason,
        command: envelope.params.command,
        cwd,
        requested_permissions: envelope.params.permissions,
        available_decisions: envelope.params.available_decisions.unwrap_or_default(),
    })
}

/// 「此刻在跑的一轮」的跨线程标记。`turn/interrupt` 的 params 必填 threadId+turnId,而发起打断的
/// 线程手里只有把手,所以这个二元组必须共享出去。
///
/// 置位在 `turn_start` 返回时;**清位在 reader 线程**(`recv_notification` 再兜一道,盖住
/// 「终态先到、置位后发生」的窄竞态)。清位刻意不只放在消费者那侧:那样「轮是否已终态」就取决于
/// 消费者的快慢,消费者一慢,打断就会在轮其实已经结束之后误升级成 SIGINT —— 那是把整个会话进程杀掉。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnRef {
    thread_id: String,
    turn_id: String,
}

type ActiveTurn = Arc<Mutex<Option<TurnRef>>>;

fn clear_active_turn_matching(slot: &ActiveTurn, turn_id: &str) {
    let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.as_ref().is_some_and(|turn| turn.turn_id == turn_id) {
        *guard = None;
    }
}

/// reader 线程侧的终态识别:只认 `turn/completed`(协议里一轮的唯一终态通知;`error` 通知
/// 可能带 `willRetry`,不是终态)。
fn note_turn_terminal(slot: &ActiveTurn, value: &Value) {
    if value.get("method").and_then(Value::as_str) != Some("turn/completed") {
        return;
    }
    if let Some(turn_id) = value.pointer("/params/turn/id").and_then(Value::as_str) {
        clear_active_turn_matching(slot, turn_id);
    }
}

/// 仅带外 steer 响应在 reader 线程按 id 分发；通知和主请求仍由 client 顺序消费。
type SteerReplies = Arc<Mutex<BTreeMap<u64, mpsc::Sender<Value>>>>;

fn route_steer_reply(replies: &SteerReplies, value: &Value) -> bool {
    if value.get("method").is_some() {
        return false;
    }
    let Some(id) = value.get("id").and_then(Value::as_u64) else {
        return false;
    };
    let sender = replies
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id);
    if let Some(sender) = sender {
        let _ = sender.send(value.clone());
        true
    } else {
        false
    }
}

/// 跨线程打断把手:`Clone + Send`,可以塞进注册表或交给别的线程。
///
/// 两级:先经共享写端发 `turn/interrupt`(让 codex 自己收尾,`<turn_aborted>` 留在上下文里),
/// 宽限内没进终态才 SIGINT **整个进程组** —— 后者会把会话进程一起带走,代价是下一条消息要重连
/// (`thread/resume`,实测 ~1s),所以永远是第二选择。
///
/// **只 SIGINT,不用 SIGTERM**(红线):codex 收 SIGINT 会写 `<turn_aborted>` 保留被打断的上下文;
/// SIGTERM 是硬终止,下一轮 resume 拿到空上下文 —— 不崩不报错,只是悄悄失忆。
#[derive(Clone)]
pub struct RpcInterrupt {
    writer: Arc<RpcWriter>,
    active_turn: ActiveTurn,
    steer_replies: SteerReplies,
    alive: Arc<AtomicBool>,
    killer: ChildKiller,
}

// 尚无生产调用方:按 docs/superpowers/plans/2026-08-05-codex-cursor-native-rpc.md,打断的接线点
// 在 Task 4(`capture/rpc.rs` 的一轮捕获)。把手先于调用方落地是刻意的 —— 会话池要在建进程时就
// 把它交出去,否则「进程活着但没人能打断它」这个空窗会一直存在。由 `capture::rpc_pool` 的单测覆盖。
#[allow(dead_code)]
impl RpcInterrupt {
    pub fn interrupt(&self) -> RpcInterruptOutcome {
        self.interrupt_within(SOFT_INTERRUPT_GRACE)
    }

    pub fn interrupt_within(&self, grace: Duration) -> RpcInterruptOutcome {
        let Some(turn) = self.active_turn() else {
            return RpcInterruptOutcome::Idle;
        };
        let frame = json!({
            "id": self.writer.next_id(),
            "method": "turn/interrupt",
            "params": {"threadId": turn.thread_id, "turnId": turn.turn_id},
        });
        // 响应帧归 chat 线程消费(它才是 reader 的唯一消费者),这里发完就不管:判据是「轮有没有
        // 进终态」,不是「server 回没回这条 id」。写失败(进程已死 / 写端已关 / 抢不到锁)也只是没了
        // 软打断这条路,继续走下面的宽限判定。
        if let Err(error) = self.writer.send_bounded(&frame, INTERRUPT_WRITE_WAIT) {
            crate::observe::log::log(&summon_i18n::format!(
                "r6ccd09b5cb",
                CodexAppServerError::from(error)
            ));
        }
        let deadline = Instant::now() + grace;
        loop {
            match self.active_turn() {
                Some(current) if current.turn_id == turn.turn_id => {}
                // 已清空或已换到别的轮 —— 两种都说明要打断的那一轮结束了。
                _ => return RpcInterruptOutcome::Settled,
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.killer.interrupt();
        RpcInterruptOutcome::Escalated
    }

    /// **轮内 steer**:往正在跑的那一轮追加一段用户输入,`turn/steer`。不打断、不重置本轮 ——
    /// codex 在当前 turn 里把它当作新到的用户消息接着处理。
    ///
    /// 与 [`interrupt_within`](Self::interrupt_within) 共用同一条带外通路(`send_bounded`
    /// 绕开 client 主锁),因为原因相同:chat 线程正卡在流式消费里长时间独占那把锁,等它就等不到。
    ///
    /// reader 按请求 id 回传响应；只有服务端确认同一 turn 才算接受。超时或断连不能证明未
    /// 执行，必须由调用方报告结果不明，避免重复执行用户指令。
    pub fn steer(&self, text: &str) -> RpcSteerOutcome {
        self.steer_within(text, Duration::from_secs(2))
    }

    fn steer_within(&self, text: &str, response_wait: Duration) -> RpcSteerOutcome {
        let Some(turn) = self.active_turn() else {
            return RpcSteerOutcome::Idle;
        };
        let id = self.writer.next_id();
        let (tx, rx) = mpsc::channel();
        {
            let mut replies = self
                .steer_replies
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !self.alive.load(Ordering::Acquire) {
                return RpcSteerOutcome::Failed;
            }
            replies.insert(id, tx);
        }
        let frame = json!({
            "id": id,
            "method": "turn/steer",
            "params": {
                "threadId": turn.thread_id,
                "expectedTurnId": turn.turn_id,
                "input": [{"type": "text", "text": text}],
            },
        });
        let outcome = match self.writer.send_bounded(&frame, INTERRUPT_WRITE_WAIT) {
            Ok(()) => match rx.recv_timeout(response_wait) {
                Ok(value)
                    if value.get("error").is_none()
                        && value.pointer("/result/turnId").and_then(Value::as_str)
                            == Some(&turn.turn_id) =>
                {
                    RpcSteerOutcome::Accepted
                }
                Ok(value)
                    if value.get("result").is_none()
                        && value
                            .pointer("/error/code")
                            .and_then(Value::as_i64)
                            .is_some()
                        && value
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .is_some() =>
                {
                    // 服务端错误正文可能含用户输入，日志只记 code。
                    crate::observe::log::log(&summon_i18n::format!(
                        "rf09a14d67c",
                        value["error"]["code"]
                    ));
                    RpcSteerOutcome::Rejected
                }
                response => {
                    let reason = match response {
                        Err(mpsc::RecvTimeoutError::Timeout) => summon_i18n::text!("rb883d23750"),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            summon_i18n::text!("r40c61a4197")
                        }
                        _ => summon_i18n::text!("rf290a4b583"),
                    };
                    crate::observe::log::log(&summon_i18n::format!("r63ffe4d9e6"));
                    RpcSteerOutcome::Unconfirmed
                }
            },
            Err(error) => {
                // write_all/flush 报错时可能已有字节送达，不能当作确定未执行。
                let outcome = if matches!(error, RpcStdioError::Io(_)) {
                    RpcSteerOutcome::Unconfirmed
                } else {
                    RpcSteerOutcome::Failed
                };
                crate::observe::log::log(&summon_i18n::format!(
                    "redcd3d009c",
                    CodexAppServerError::from(error)
                ));
                outcome
            }
        };
        self.steer_replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        outcome
    }

    /// 换一个硬打断句柄(指向同一个 pgid,但**自带一把新的一次性闸**)。
    ///
    /// 为什么需要:[`ChildKiller::interrupt`] 是一次性的(发过一次就永久闭闸,防对已复用的 pid
    /// 误伤)。会话进程跨轮存活,若每轮都共用 spawn 时那一把闸,任何一轮升级过 SIGINT 之后,
    /// 后续所有轮(乃至 daemon 停机)的 SIGINT 就都成了空操作,只剩宽限后的 SIGKILL ——
    /// 而 SIGKILL 恰恰是那条「codex 写不出 `<turn_aborted>`」的红线。
    pub fn rebind_killer(&self, killer: ChildKiller) -> Self {
        Self {
            writer: Arc::clone(&self.writer),
            active_turn: Arc::clone(&self.active_turn),
            steer_replies: Arc::clone(&self.steer_replies),
            alive: Arc::clone(&self.alive),
            killer,
        }
    }

    /// 此刻在跑的轮 id。调用方据此判断「现在打断有没有意义」,测试也靠它同步。
    pub fn active_turn_id(&self) -> Option<String> {
        self.active_turn().map(|turn| turn.turn_id)
    }

    fn active_turn(&self) -> Option<TurnRef> {
        self.active_turn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct ProcessDeadline {
    cancel: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ProcessDeadline {
    fn start(deadline: Instant, killer: ChildKiller) -> Self {
        let (cancel, cancelled) = mpsc::channel();
        let worker = thread::spawn(move || {
            if matches!(
                cancelled.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                killer.kill();
            }
        });
        Self {
            cancel,
            worker: Some(worker),
        }
    }
}

impl Drop for ProcessDeadline {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 同步、单 outstanding request 的 JSON-RPC 客户端。daemon 可把它放在自己的 recovery runner
/// 线程；无需 async runtime，也不会阻塞其它 summon 热路径。
pub struct CodexAppServerClient {
    child: Child,
    writer: Arc<RpcWriter>,
    reader: Option<thread::JoinHandle<()>>,
    stderr: Arc<StderrTail>,
    rx: Receiver<ReaderEvent>,
    queued_notifications: VecDeque<Value>,
    /// 此刻在跑的一轮(见 [`TurnRef`])。
    active_turn: ActiveTurn,
    steer_replies: SteerReplies,
    /// 「stdout 还没 EOF」= 进程还在。reader 线程读到 EOF 即清位。
    alive: Arc<AtomicBool>,
    killer: ChildKiller,
    request_timeout: Duration,
    deadline_guard: Option<ProcessDeadline>,
    pgid: i32,
    terminated: bool,
}

impl CodexAppServerClient {
    pub fn spawn(config: CodexAppServerConfig) -> Result<Self, CodexAppServerError> {
        let mut command = Command::new(&config.program);
        command.args(&config.args);
        if let Some(work_dir) = &config.work_dir {
            command.current_dir(work_dir);
        }
        // **无条件先清空再显式给**(fail-closed)。
        //
        // 早先写成「env 非空才清」,那是 fail-open:任何忘了给 env 的调用点都会静默退回
        // 「继承 daemon 全部环境」,而这正是要修的那条泄漏路径本身 —— 一个漏配就把防线整个绕过。
        // 现在的语义是:**这里给什么,子进程就有什么**,给空就是空环境。
        command.env_clear();
        for (key, value) in &config.env {
            command.env(key, value);
        }
        // App Server 及其工具子进程自成组，daemon 停机可一次命中，绝不遗留孤儿。
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // pipe + 并发 drain 留尾(见 `StderrTail`):null 会把启动失败的唯一线索丢掉,
            // pipe 了不读则会反压死锁 —— 两个坑只有「pipe 且有人读」能同时避开。
            .stderr(Stdio::piped());
        let mut child = crate::runtime::process::retry_text_file_busy(|| command.spawn())
            .map_err(CodexAppServerError::Spawn)?;
        let pgid = i32::try_from(child.id()).map_err(|_| CodexAppServerError::InvalidResponse {
            operation: "spawn".into(),
            detail: "child pid exceeds i32".into(),
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CodexAppServerError::InvalidResponse {
                operation: "spawn".into(),
                detail: "child stdin unavailable".into(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CodexAppServerError::InvalidResponse {
                operation: "spawn".into(),
                detail: "child stdout unavailable".into(),
            })?;
        // stderr 拿不到不该让整次 spawn 失败:少一份诊断信息,但会话本身仍能跑。
        let stderr = StderrTail::spawn(child.stderr.take());
        let (tx, rx) = mpsc::channel();
        let active_turn: ActiveTurn = Arc::new(Mutex::new(None));
        let alive = Arc::new(AtomicBool::new(true));
        let active_turn_reader = Arc::clone(&active_turn);
        let alive_reader = Arc::clone(&alive);
        let steer_replies = SteerReplies::default();
        let steer_replies_reader = Arc::clone(&steer_replies);
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        let _ = tx.send(ReaderEvent::InvalidJson(error.to_string()));
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(value) => {
                        // 终态在这里认,不等消费者:见 `TurnRef` 的注释。
                        note_turn_terminal(&active_turn_reader, &value);
                        if route_steer_reply(&steer_replies_reader, &value) {
                            continue;
                        }
                        if tx.send(ReaderEvent::Message(value)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        if tx
                            .send(ReaderEvent::InvalidJson(format!("{error}: {line}")))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            // stdout EOF / 读崩:进程(及所有仍持着这个 fd 的后代)已经走了。清掉在跑轮标记,
            // 否则打断把手会对着一个已死、甚至已被 OS 复用的 pgid 发 SIGINT。
            alive_reader.store(false, Ordering::Release);
            steer_replies_reader
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
            *active_turn_reader
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
        });
        let mut client = Self {
            child,
            writer: Arc::new(RpcWriter::new(stdin)),
            reader: Some(reader),
            stderr,
            rx,
            queued_notifications: VecDeque::new(),
            active_turn,
            steer_replies,
            alive,
            killer: ChildKiller::new(pgid),
            request_timeout: config.request_timeout,
            deadline_guard: config
                .process_deadline
                .map(|deadline| ProcessDeadline::start(deadline, ChildKiller::new(pgid))),
            pgid,
            terminated: false,
        };
        // 握手失败最常见的形态就是「进程立刻死了」(codex 未登录/参数不认),原因只在 stderr。
        if let Err(error) = client.handshake(&config) {
            return Err(client.exit_error("initialize", error));
        }
        Ok(client)
    }

    fn handshake(&mut self, config: &CodexAppServerConfig) -> Result<(), CodexAppServerError> {
        let mut params = json!({
            "clientInfo": {
                "name": config.client_name,
                "title": config.client_title,
                "version": config.client_version,
            }
        });
        if config.experimental_api {
            params["capabilities"] = json!({"experimentalApi": true});
        }
        self.request_value("initialize", params, config.request_timeout)?;
        self.send_message(&json!({"method": "initialized", "params": {}}))
    }

    /// 跨线程打断把手,见 [`RpcInterrupt`]。
    pub fn interrupt_handle(&self) -> RpcInterrupt {
        RpcInterrupt {
            writer: Arc::clone(&self.writer),
            active_turn: Arc::clone(&self.active_turn),
            steer_replies: Arc::clone(&self.steer_replies),
            alive: Arc::clone(&self.alive),
            killer: self.killer.clone(),
        }
    }

    /// 可 kill 句柄:进池的 client 要登记进 `InFlight`,daemon 停机才够得着这个进程组。
    pub fn killer(&self) -> ChildKiller {
        self.killer.clone()
    }

    /// 「进程还在不在」的跨线程存活证据(reader 线程读到 stdout EOF 即清位)。池据此判断要不要
    /// 懒重连,而**不必去抢 client 的锁** —— 一轮流式消费会长时间独占它。
    pub fn liveness(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alive)
    }

    /// 自有进程组 id(== 子进程 pid,spawn 时 `process_group(0)`)。
    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// 丢掉此刻还排在队里的通知,返回丢了几条。
    ///
    /// **一轮开始前调**:client 跨轮存活(会话池复用同一个进程),上一轮终态之后仍在飞的通知、
    /// 以及开会话请求等待响应期间排进来的东西,都会留在这个队列里。不清就会被下一轮
    /// `recv_notification` 原样弹出来当成本轮内容折进卡片。轮与轮之间没有别的读者,
    /// 所以此刻队里的每一条都必然不属于将要开始的那一轮。
    ///
    /// 只清**已排队**的那部分，并返回仍可解析的控制面通知，让调用方吸收额度等跨轮信息；
    /// 还在 reader channel 里飞的残留由消费循环的 turnId 闸挡住
    /// (见 `capture::rpc::belongs_to_turn`)—— 两道闸各管一段,谁都替不了谁。
    pub fn drain_queued_notifications(&mut self) -> (usize, Vec<CodexAppServerNotification>) {
        let dropped = self.queued_notifications.len();
        let parsed = self
            .queued_notifications
            .drain(..)
            .filter_map(|value| parse_notification(&value).ok().flatten())
            .collect();
        (dropped, parsed)
    }

    /// 当前保留的 stderr 尾部(最多 16KB,已去 ANSI)。空则 `None`。
    pub fn stderr_tail(&self) -> Option<String> {
        let text = self.stderr.snapshot();
        (!text.is_empty()).then_some(text)
    }

    pub fn account_rate_limits_read(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<CodexAccountRateLimits, CodexAppServerError> {
        let method = "account/rateLimits/read";
        let result = self.request_value(method, json!({}), self.timeout(timeout))?;
        parse_account_rate_limits(&result).map_err(|detail| CodexAppServerError::InvalidResponse {
            operation: method.into(),
            detail,
        })
    }

    /// 读 backend 自报的模型目录 —— 面板候选与 onboard 默认模型的事实源。
    ///
    /// 目录归 backend 管,不在 summon 这边留一张会过期的表:上游今天加一个模型,下一条会话
    /// 就能在面板上点到。codex 与 traex 走同一个方法(traex 是 codex fork),不按 CLI 分叉。
    ///
    /// **hidden 在客户端再滤一遍**,不只靠 `includeHidden: false`:app-server 对未知请求字段
    /// 静默忽略,老版本收下这个参数却照样回全量,那时候面板上就会出现 codex 自己都不列的
    /// 内部模型(点了起不来,且没有任何报错)。
    pub fn model_list(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<CodexModelCatalog, CodexAppServerError> {
        let method = "model/list";
        let mut catalog = CodexModelCatalog::default();
        let mut cursor: Option<String> = None;
        for _ in 0..MODEL_LIST_MAX_PAGES {
            let params = model_list_params(cursor.as_deref());
            let result = self.request_value(method, params, self.timeout(timeout))?;
            let page: CodexModelListPage = serde_json::from_value(result).map_err(|error| {
                CodexAppServerError::InvalidResponse {
                    operation: method.into(),
                    detail: error.to_string(),
                }
            })?;
            for entry in page.data {
                if entry.hidden {
                    continue;
                }
                let id = entry.id.trim();
                if id.is_empty() {
                    continue;
                }
                if entry.is_default && catalog.default_model.is_none() {
                    catalog.default_model = Some(id.to_string());
                }
                catalog.models.push(id.to_string());
            }
            match page.next_cursor {
                Some(next) if !next.trim().is_empty() => cursor = Some(next),
                _ => break,
            }
        }
        Ok(catalog)
    }

    /// app-server slash-facing 控制端口的底层请求原语。
    ///
    /// 只在 crate 内开放：方法 allowlist 与参数形状由 `capture::rpc_pool::control_existing`
    /// 冻结。若把它暴露给 daemon 任意字符串调用，等于把 app-server 的账号、文件删除、插件安装
    /// 等完整副作用面重新开放成聊天命令。
    pub(crate) fn control_request(
        &mut self,
        method: &'static str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, CodexAppServerError> {
        self.request_value(method, params, self.timeout(timeout))
    }

    pub fn thread_read(
        &mut self,
        thread_id: &str,
        include_turns: bool,
        timeout: Option<Duration>,
    ) -> Result<CodexAppServerThread, CodexAppServerError> {
        let method = "thread/read";
        let result = self.request_value(
            method,
            json!({"threadId": thread_id, "includeTurns": include_turns}),
            self.timeout(timeout),
        )?;
        parse_thread_result(method, &result)
    }

    /// 只读列出 state DB 里的线程摘要。Menu Bar 用它取真正的 `Thread.name`，不据此
    /// 判断其它 App Server 实例的 loaded/running 状态。
    pub fn thread_list(
        &mut self,
        archived: bool,
        limit: u32,
        cursor: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<CodexAppServerThreadList, CodexAppServerError> {
        let method = "thread/list";
        let mut params = json!({
            "archived": archived,
            "limit": limit,
            "useStateDbOnly": true,
            "sortKey": "updated_at",
        });
        if let Some(cursor) = cursor {
            params["cursor"] = Value::String(cursor.to_string());
        }
        let result = self.request_value(method, params, self.timeout(timeout))?;
        serde_json::from_value(result).map_err(|error| CodexAppServerError::InvalidResponse {
            operation: method.into(),
            detail: error.to_string(),
        })
    }

    /// 从 `source_thread_id` fork 出一条新 thread,并把托管会话所需的三件事一起钉上。
    ///
    /// 目标 chat 之后要长期用这条 thread,所以 cwd / PRIMER / 策略两半必须与 `thread/start` 同一
    /// 套纪律 —— 缺 cwd 会让它跑在 app-server 自己的目录里,缺 sandbox 就是「冷启好好的、
    /// 重连后写不了文件」那个老坑。
    ///
    /// PRIMER 走 `developerInstructions`(**追加**语义,见 [`thread_start_params`]),因此 fork 出的
    /// thread 与冷启的 thread 一样是 `primer_slot=developer` —— resume 时必须重发。
    pub fn thread_fork_for_chat(
        &mut self,
        source_thread_id: &str,
        cwd: Option<&str>,
        developer_instructions: Option<&str>,
        policy: &CodexPolicy,
        timeout: Option<Duration>,
    ) -> Result<CodexAppServerThread, CodexAppServerError> {
        let method = "thread/fork";
        let result = self.request_value(
            method,
            thread_fork_params(source_thread_id, cwd, developer_instructions, policy),
            self.timeout(timeout),
        )?;
        parse_thread_result(method, &result)
    }

    /// 冷启一个 thread。`cwd` 钉死 agent root(app-server 自己的 cwd 可能停在 $HOME);
    /// `developer_instructions` 是 PRIMER 的落点(**追加**语义,见 [`thread_start_params`])。
    /// 与 fork 同样显式下发 `approvalPolicy` + `sandbox`，两半都来自已解析的 summon 配置；
    /// 默认仍是 never，显式交互策略则由飞书审批处理器承接。
    /// 不带平台工具的冷启。生产路径一律走 [`Self::thread_start_with_dynamic_tools`]
    /// (它以 `None` 表达「这一轮不注册」),这条只留给协议测试。
    #[cfg(test)]
    pub fn thread_start(
        &mut self,
        cwd: Option<&str>,
        developer_instructions: Option<&str>,
        policy: &CodexPolicy,
        timeout: Option<Duration>,
    ) -> Result<CodexAppServerThread, CodexAppServerError> {
        self.thread_start_with_dynamic_tools(cwd, developer_instructions, policy, None, timeout)
    }

    /// 冷启 thread，并按 app-server 实验协议注册客户端动态工具。动态工具只允许在
    /// `thread/start` 下发；后续 `thread/resume` 从 rollout 元数据恢复，不能重传该字段。
    pub fn thread_start_with_dynamic_tools(
        &mut self,
        cwd: Option<&str>,
        developer_instructions: Option<&str>,
        policy: &CodexPolicy,
        dynamic_tools: Option<&Value>,
        timeout: Option<Duration>,
    ) -> Result<CodexAppServerThread, CodexAppServerError> {
        let method = "thread/start";
        let result = match self.request_value(
            method,
            thread_start_params(cwd, developer_instructions, policy, dynamic_tools),
            self.timeout(timeout),
        ) {
            Ok(result) => result,
            Err(CodexAppServerError::Remote {
                code: Some(-32602), ..
            }) if dynamic_tools.is_some() => {
                crate::observe::log::log(summon_i18n::text!("rb446ac721d"));
                self.request_value(
                    method,
                    thread_start_params(cwd, developer_instructions, policy, None),
                    self.timeout(timeout),
                )?
            }
            Err(error) => return Err(error),
        };
        parse_thread_result(method, &result)
    }

    /// 生成本 backend 的 MCP 启停覆盖，用于独立草稿进程。
    pub(crate) fn draft_mcp_overrides(
        &mut self,
        reader: Option<(&str, &Value)>,
    ) -> Result<Vec<String>, CodexAppServerError> {
        let response = self.request_value(
            "config/read",
            json!({"includeLayers":false}),
            self.request_timeout,
        )?;
        let params = draft_thread_params(&response, ".")?;
        let servers = params["config"]["mcp_servers"]
            .as_object()
            .expect("draft params contain server map");
        let mut entries = servers
            .keys()
            .map(|name| {
                format!(
                    "{}={{enabled=false}}",
                    serde_json::to_string(name).expect("server name serializes")
                )
            })
            .collect::<Vec<_>>();
        if let Some((name, config)) = reader {
            entries.push(format!(
                "{}={{command={},args={},enabled=true,enabled_tools=[\"read_document\"]}}",
                serde_json::to_string(name).unwrap(),
                config["command"],
                config["args"]
            ));
        }
        Ok(vec![
            "-c".into(),
            format!("mcp_servers={{{}}}", entries.join(",")),
        ])
    }

    /// 创建只读的临时起草 thread，先核验生效的 MCP 配置。
    pub(crate) fn thread_start_draft(
        &mut self,
        cwd: &str,
        reader: Option<(&str, &Value)>,
    ) -> Result<CodexAppServerThread, CodexAppServerError> {
        let config = self.request_value(
            "config/read",
            json!({"includeLayers":false}),
            self.request_timeout,
        )?;
        let mut params = draft_thread_params(&config, cwd)?;
        if config["config"]["mcp_servers"]
            .as_object()
            .is_some_and(|servers| {
                servers.iter().any(|(name, server)| {
                    if let Some((_, expected)) =
                        reader.filter(|(expected_name, _)| *expected_name == name)
                    {
                        server["enabled"] != true
                            || server["command"] != expected["command"]
                            || server["args"] != expected["args"]
                            || server["enabled_tools"] != expected["enabled_tools"]
                            || !server["url"].is_null()
                            || server["env"].as_object().is_some_and(|env| !env.is_empty())
                    } else {
                        server["enabled"] != false
                    }
                })
            })
        {
            return Err(CodexAppServerError::InvalidResponse {
                operation: "config/read".into(),
                detail: "draft MCP configuration contains an unexpected capability".into(),
            });
        }
        if let Some((name, expected)) = reader {
            if config["config"]["mcp_servers"][name]["enabled"] != true {
                return Err(CodexAppServerError::InvalidResponse {
                    operation: "config/read".into(),
                    detail: "document reader is unavailable".into(),
                });
            }
            params["config"]["mcp_servers"][name] = expected.clone();
        }
        let result = self.request_value("thread/start", params, self.request_timeout)?;
        parse_thread_result("thread/start", &result)
    }

    /// 续接一个已存在的 thread。**params 由 `thread_resume_params` 独占产出,见那里的红线注释。**
    pub fn thread_resume(
        &mut self,
        thread_id: &str,
        cwd: Option<&str>,
        developer_instructions: Option<&str>,
        policy: &CodexPolicy,
        timeout: Option<Duration>,
    ) -> Result<CodexAppServerThread, CodexAppServerError> {
        let method = "thread/resume";
        let result = self.request_value(
            method,
            thread_resume_params(thread_id, cwd, developer_instructions, policy),
            self.timeout(timeout),
        )?;
        parse_thread_result(method, &result)
    }

    pub fn turn_start(
        &mut self,
        thread_id: &str,
        prompt: &str,
        options: &TurnOptions<'_>,
        timeout: Option<Duration>,
    ) -> Result<CodexAppServerTurn, CodexAppServerError> {
        let method = "turn/start";
        let params = turn_start_params(thread_id, prompt, options);
        let result = self.request_value(method, params, self.timeout(timeout))?;
        let turn: CodexAppServerTurn =
            serde_json::from_value(result.get("turn").cloned().ok_or_else(|| {
                CodexAppServerError::InvalidResponse {
                    operation: method.into(),
                    detail: "missing result.turn".into(),
                }
            })?)
            .map_err(|error| CodexAppServerError::InvalidResponse {
                operation: method.into(),
                detail: error.to_string(),
            })?;
        // 置位「在跑的轮」,打断把手才有 threadId+turnId 可填。终态若已先一步被 reader 观察到,
        // 由 `recv_notification` 消费到那条通知时再清 —— 那条兜底就是为这个窄竞态留的。
        *self
            .active_turn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(TurnRef {
            thread_id: thread_id.to_string(),
            turn_id: turn.id.clone(),
        });
        Ok(turn)
    }

    #[cfg(test)]
    pub fn wait_for_turn_completed(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        timeout: Duration,
    ) -> Result<CodexAppServerTurn, CodexAppServerError> {
        let operation = format!("turn/completed({thread_id}/{turn_id})");
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CodexAppServerError::Timeout { operation });
            }
            let slice = remaining.min(Duration::from_millis(100));
            let notification = match self.recv_notification(Some(slice)) {
                Ok(notification) => notification,
                Err(CodexAppServerError::Timeout { .. }) => continue,
                Err(error) => return Err(error),
            };
            match notification {
                CodexAppServerNotification::TurnCompleted {
                    thread_id: got_thread,
                    turn,
                } if got_thread == thread_id && turn.id == turn_id => return Ok(turn),
                _ => {}
            }
        }
    }

    /// SIGINT 整个自有进程组，宽限期后仍存活则 SIGKILL。幂等，可由 daemon stop 与 Drop 共用。
    pub fn interrupt(&mut self, grace: Duration) -> Result<(), CodexAppServerError> {
        // 进程回收前收掉计时线程，截止时刻的信号只能命中本次 Child。
        self.deadline_guard.take();
        if self.terminated {
            return Ok(());
        }
        let mut leader_exited = match self.child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => return Err(CodexAppServerError::Io(error)),
        };
        self.terminated = true;
        self.alive.store(false, Ordering::Release);
        *self
            .active_turn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.writer.close();
        if !leader_exited {
            if let Err(error) = signal_group(self.pgid, nix::sys::signal::Signal::SIGINT) {
                // 同 uid 正常不会失败；兜底仍回收直系 child，不能因报告 signal 错误而遗留进程。
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(CodexAppServerError::Io(error));
            }
        }
        let deadline = Instant::now() + grace;
        loop {
            if !leader_exited {
                match self.child.try_wait() {
                    Ok(Some(_)) => leader_exited = true,
                    Ok(None) => {}
                    Err(error) => return Err(CodexAppServerError::Io(error)),
                }
            }
            if !crate::runtime::inflight::group_alive(self.pgid) {
                if !leader_exited {
                    self.child.wait().map_err(CodexAppServerError::Io)?;
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        signal_group(self.pgid, nix::sys::signal::Signal::SIGKILL)
            .map_err(CodexAppServerError::Io)?;
        if !leader_exited {
            self.child.wait().map_err(CodexAppServerError::Io)?;
        }
        Ok(())
    }

    /// 取下一条已分类通知。request 等待期间到达的通知也会排队，不会丢。
    pub fn recv_notification(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<CodexAppServerNotification, CodexAppServerError> {
        let operation = "notification";
        let deadline = Instant::now() + self.timeout(timeout);
        loop {
            if let Some(value) = self.queued_notifications.pop_front() {
                if let Some(notification) = parse_notification(&value)? {
                    self.note_terminal(&notification);
                    return Ok(notification);
                }
            }
            let value = self.recv_value(deadline, operation)?;
            if is_server_request(&value) {
                self.respond_to_server_request(&value)?;
                continue;
            }
            if let Some(notification) = parse_notification(&value)? {
                self.note_terminal(&notification);
                return Ok(notification);
            }
        }
    }

    /// 聊天捕获专用的通知入口。除普通通知外，它会把当前 thread+turn 的非 secret 提问交给
    /// handler；处理完一条提问并写回后立即返回本地 heartbeat，避免上层静默 watchdog 把
    /// 「人在卡片前思考」误算成 app-server 卡死。
    ///
    /// 无聊天上下文仍走 [`Self::recv_notification`]，保持逐题空答的旧行为。
    /// 只挂原生提问、不挂平台工具与审批的收帧路径。生产走
    /// [`Self::recv_notification_with_handlers`],这条只留给协议测试。
    #[cfg(test)]
    pub fn recv_notification_with_user_input(
        &mut self,
        timeout: Option<Duration>,
        expected_thread: &str,
        expected_turn: &str,
        handler: &dyn CodexUserInputHandler,
        cancelled: &AtomicBool,
        waiting_flag: &AtomicBool,
    ) -> Result<CodexAppServerNotification, CodexAppServerError> {
        self.recv_notification_with_handlers(
            timeout,
            expected_thread,
            expected_turn,
            Some(handler),
            None,
            None,
            cancelled,
            waiting_flag,
        )
    }

    /// 聊天捕获专用的客户端工具入口。每条 server request 必须得到一次响应；只有精确匹配
    /// 当前 thread+turn 的已注册 handler 才能执行动态工具，其余情况确定性拒绝。
    #[allow(clippy::too_many_arguments)]
    pub fn recv_notification_with_handlers(
        &mut self,
        timeout: Option<Duration>,
        expected_thread: &str,
        expected_turn: &str,
        user_input_handler: Option<&dyn CodexUserInputHandler>,
        dynamic_tool_handler: Option<&dyn CodexDynamicToolHandler>,
        approval_handler: Option<&dyn CodexApprovalHandler>,
        cancelled: &AtomicBool,
        waiting_flag: &AtomicBool,
    ) -> Result<CodexAppServerNotification, CodexAppServerError> {
        let operation = "notification";
        let deadline = Instant::now() + self.timeout(timeout);
        loop {
            if let Some(value) = self.queued_notifications.pop_front() {
                if let Some(notification) = parse_notification(&value)? {
                    self.note_terminal(&notification);
                    return Ok(notification);
                }
            }
            let value = self.recv_value(deadline, operation)?;
            if is_server_request(&value) {
                match value.get("method").and_then(Value::as_str) {
                    Some("item/tool/requestUserInput") => {
                        if let Some(handler) = user_input_handler {
                            return self.respond_to_user_input_request(
                                &value,
                                expected_thread,
                                expected_turn,
                                handler,
                                cancelled,
                                waiting_flag,
                            );
                        }
                    }
                    Some("item/tool/call") => {
                        if let Some(handler) = dynamic_tool_handler {
                            return self.respond_to_dynamic_tool_request(
                                &value,
                                expected_thread,
                                expected_turn,
                                handler,
                                cancelled,
                            );
                        }
                    }
                    Some(
                        "item/commandExecution/requestApproval"
                        | "item/fileChange/requestApproval"
                        | "item/permissions/requestApproval",
                    ) => {
                        if let Some(handler) = approval_handler {
                            return self.respond_to_approval_request(
                                &value,
                                expected_thread,
                                expected_turn,
                                handler,
                                cancelled,
                                waiting_flag,
                            );
                        }
                    }
                    _ => {}
                }
                self.respond_to_server_request(&value)?;
                continue;
            }
            if let Some(notification) = parse_notification(&value)? {
                self.note_terminal(&notification);
                return Ok(notification);
            }
        }
    }

    /// 消费者侧的终态兜底。reader 线程已经在原始帧上认过一遍,这里只盖住「终态先到、
    /// `turn_start` 后置位」的窄竞态 —— 那种情况下 reader 清位时标记还没写上。
    fn note_terminal(&self, notification: &CodexAppServerNotification) {
        if let CodexAppServerNotification::TurnCompleted { turn, .. } = notification {
            clear_active_turn_matching(&self.active_turn, &turn.id);
        }
    }

    fn timeout(&self, override_timeout: Option<Duration>) -> Duration {
        override_timeout.unwrap_or(self.request_timeout)
    }

    fn request_value(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CodexAppServerError> {
        let id = self.writer.next_id();
        self.send_message(&json!({"id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + timeout;
        loop {
            let value = self.recv_value(deadline, method)?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(result) = value.get("result") {
                    return Ok(result.clone());
                }
                if let Some(error) = value.get("error") {
                    return Err(remote_error(method, error));
                }
                return Err(CodexAppServerError::InvalidResponse {
                    operation: method.into(),
                    detail: "matching id has neither result nor error".into(),
                });
            }
            if is_server_request(&value) {
                self.respond_to_server_request(&value)?;
            } else if value.get("method").is_some() {
                self.queued_notifications.push_back(value);
            }
        }
    }

    fn recv_value(
        &mut self,
        deadline: Instant,
        operation: &str,
    ) -> Result<Value, CodexAppServerError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CodexAppServerError::Timeout {
                operation: operation.into(),
            });
        }
        match self.rx.recv_timeout(remaining) {
            Ok(ReaderEvent::Message(value)) => Ok(value),
            Ok(ReaderEvent::InvalidJson(detail)) => Err(CodexAppServerError::InvalidJson(detail)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(CodexAppServerError::Timeout {
                operation: operation.into(),
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let cause = CodexAppServerError::Disconnected {
                    operation: operation.into(),
                };
                Err(self.exit_error(operation, cause))
            }
        }
    }

    /// 把 stderr 尾部并进错误 —— **只在进程确实已经死了时**。活着的 app-server 的 stderr 是
    /// tracing 噪音,并进来只会淹掉真正的错因(同 `capture/subprocess.rs`「非零退出才上报」的取舍)。
    fn exit_error(&mut self, operation: &str, cause: CodexAppServerError) -> CodexAppServerError {
        if !self.wait_for_child_exit(STDERR_EOF_GRACE) {
            return cause;
        }
        self.stderr.wait_for_eof(STDERR_EOF_GRACE);
        match self.stderr_tail() {
            Some(stderr_tail) => CodexAppServerError::ProcessFailed {
                operation: operation.into(),
                detail: cause.to_string(),
                stderr_tail,
            },
            None => cause,
        }
    }

    /// 有界等子进程退出。stdout EOF 与进程变僵尸之间有极短窗口,不等会把「刚死」误判成「还活着」。
    fn wait_for_child_exit(&mut self, timeout: Duration) -> bool {
        self.deadline_guard.take();
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return false,
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn respond_to_server_request(&self, request: &Value) -> Result<(), CodexAppServerError> {
        let Some(id) = request.get("id") else {
            return Ok(());
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let reply = server_request_reply(id, method, &params);
        // 这条兼容入口没有交互处理器；逐条 log 后按 schema fail-closed 应答，不能让 server 等死。
        crate::observe::log::log(&format!(
            "codex app-server server request: method={method} id={id} → {}",
            reply.reason
        ));
        self.send_message(&reply.frame)
    }

    fn respond_to_user_input_request(
        &self,
        request: &Value,
        expected_thread: &str,
        expected_turn: &str,
        handler: &dyn CodexUserInputHandler,
        cancelled: &AtomicBool,
        waiting_flag: &AtomicBool,
    ) -> Result<CodexAppServerNotification, CodexAppServerError> {
        let parsed = match parse_codex_user_input_request(request) {
            Ok(parsed) => parsed,
            Err(_detail) => {
                // 坏 schema 也必须应答，否则 app-server 会永久等这条 id。诊断只报告类别，
                // 不把整帧或 serde error 带回去，后者可能含 secret 题面。
                let Some(id) = request.get("id") else {
                    return Err(CodexAppServerError::InvalidResponse {
                        operation: "item/tool/requestUserInput".into(),
                        detail: "server request is missing id".into(),
                    });
                };
                self.send_message(&json!({
                    "id": id,
                    "error": {
                        "code": SERVER_REQUEST_DENIED_CODE,
                        "message": "summon received an invalid user-input request schema",
                    },
                }))?;
                return Ok(user_input_resolved_notification(
                    expected_thread,
                    expected_turn,
                ));
            }
        };

        let response = if parsed.request.thread_id != expected_thread
            || parsed.request.turn_id != expected_turn
        {
            user_input_error_frame(
                &parsed.request.id,
                "summon rejected a user-input request for a different thread or turn",
            )
        } else if parsed.contains_secret {
            user_input_error_frame(
                &parsed.request.id,
                "summon refuses to relay secret user-input questions",
            )
        } else {
            let handled = {
                let _waiting = WaitingFlagGuard::new(waiting_flag);
                handler.handle_user_input(&parsed.request, cancelled, &self.alive)
            };
            match handled.and_then(|answers| user_input_result_frame(&parsed.request, answers)) {
                Ok(response) => response,
                Err(diagnostic) => {
                    let diagnostic = safe_user_input_diagnostic(&parsed.request, &diagnostic);
                    user_input_error_frame(
                        &parsed.request.id,
                        &format!("summon user-input handler failed: {diagnostic}"),
                    )
                }
            }
        };

        self.send_message(&response)?;
        // 只在回帧确实写出后记协议身份，不记 header/question/options；secret/mismatch 也不会把
        // 题面写进日志。
        crate::observe::log::log(&format!(
            "codex app-server user input resolved: id={:?} thread={} turn={}",
            parsed.request.id, parsed.request.thread_id, parsed.request.turn_id
        ));
        Ok(user_input_resolved_notification(
            &parsed.request.thread_id,
            &parsed.request.turn_id,
        ))
    }

    fn respond_to_dynamic_tool_request(
        &self,
        request: &Value,
        expected_thread: &str,
        expected_turn: &str,
        handler: &dyn CodexDynamicToolHandler,
        cancelled: &AtomicBool,
    ) -> Result<CodexAppServerNotification, CodexAppServerError> {
        let parsed = match parse_codex_dynamic_tool_request(request) {
            Ok(parsed) => parsed,
            Err(_) => {
                let Some(id) = request.get("id") else {
                    return Err(CodexAppServerError::InvalidResponse {
                        operation: "item/tool/call".into(),
                        detail: "server request is missing id".into(),
                    });
                };
                self.send_message(&dynamic_tool_result_frame(
                    id,
                    false,
                    "summon received an invalid dynamic-tool request schema",
                ))?;
                return Ok(dynamic_tool_resolved_notification(
                    expected_thread,
                    expected_turn,
                ));
            }
        };
        let response = if parsed.thread_id != expected_thread || parsed.turn_id != expected_turn {
            dynamic_tool_result_frame(
                &serde_json::to_value(&parsed.id).unwrap_or(Value::Null),
                false,
                "summon rejected a dynamic-tool request for a different thread or turn",
            )
        } else if cancelled.load(Ordering::Acquire) || !self.alive.load(Ordering::Acquire) {
            dynamic_tool_result_frame(
                &serde_json::to_value(&parsed.id).unwrap_or(Value::Null),
                false,
                "summon cancelled the dynamic-tool request because the turn is no longer active",
            )
        } else {
            match handler.handle_dynamic_tool(&parsed) {
                Ok(output) => dynamic_tool_result_frame(
                    &serde_json::to_value(&parsed.id).unwrap_or(Value::Null),
                    true,
                    &output,
                ),
                Err(_) => dynamic_tool_result_frame(
                    &serde_json::to_value(&parsed.id).unwrap_or(Value::Null),
                    false,
                    "summon dynamic-tool execution failed",
                ),
            }
        };
        self.send_message(&response)?;
        crate::observe::log::log(&format!(
            "codex app-server dynamic tool resolved: id={:?} thread={} turn={} namespace={} tool={}",
            parsed.id,
            parsed.thread_id,
            parsed.turn_id,
            parsed.namespace.as_deref().unwrap_or(""),
            parsed.tool,
        ));
        Ok(dynamic_tool_resolved_notification(
            &parsed.thread_id,
            &parsed.turn_id,
        ))
    }

    fn respond_to_approval_request(
        &self,
        request: &Value,
        expected_thread: &str,
        expected_turn: &str,
        handler: &dyn CodexApprovalHandler,
        cancelled: &AtomicBool,
        waiting_flag: &AtomicBool,
    ) -> Result<CodexAppServerNotification, CodexAppServerError> {
        let parsed = match parse_codex_approval_request(request) {
            Ok(parsed) => parsed,
            Err(_) => {
                let Some(id) = request.get("id") else {
                    return Err(CodexAppServerError::InvalidResponse {
                        operation: "approval request".into(),
                        detail: "server request is missing id".into(),
                    });
                };
                self.send_message(&json!({
                    "id": id,
                    "error": {
                        "code": SERVER_REQUEST_DENIED_CODE,
                        "message": "summon received an invalid approval request schema",
                    },
                }))?;
                return Ok(approval_resolved_notification(
                    expected_thread,
                    expected_turn,
                ));
            }
        };
        let decision = if parsed.thread_id != expected_thread || parsed.turn_id != expected_turn {
            CodexApprovalDecision::Decline
        } else {
            let _waiting = WaitingFlagGuard::new(waiting_flag);
            handler.handle_approval(&parsed, cancelled, &self.alive)
        };
        self.send_message(&approval_result_frame(&parsed, decision))?;
        crate::observe::log::log(&format!(
            "codex app-server approval resolved: id={:?} thread={} turn={} item={} decision={decision:?}",
            parsed.id, parsed.thread_id, parsed.turn_id, parsed.item_id
        ));
        Ok(approval_resolved_notification(
            &parsed.thread_id,
            &parsed.turn_id,
        ))
    }

    fn send_message(&self, value: &Value) -> Result<(), CodexAppServerError> {
        Ok(self.writer.send(value)?)
    }
}

impl Drop for CodexAppServerClient {
    fn drop(&mut self) {
        // 仅处理中断自己 spawn 的进程组，绝不碰 tmux。
        let _ = self.interrupt(Duration::from_millis(100));
        if let Some(reader) = self.reader.take() {
            crate::runtime::rpc_stdio::join_reader_bounded(reader, "codex app-server");
        }
        // stderr drain 线程不 join:进程组已被信号收掉,它会在 EOF 自行结束;逃逸的孙进程若
        // 还持着 fd,join 会把 Drop 永久卡住。
    }
}

/// 「知道这个方法,但确定性拒绝」用的自定义 code(JSON-RPC 保留区之外)。
const SERVER_REQUEST_DENIED_CODE: i64 = -32001;

/// 没见过的方法用标准「method not found」。协议在演进,新方法一定会出现;回它比回自定义码
/// 更诚实 —— 我们确实没实现。
const SERVER_REQUEST_UNKNOWN_CODE: i64 = -32601;

/// 拒绝理由会原样进模型上下文(`denied.rejection` 就是给模型看的),写人话。
const DENY_REASON: &str = "ra3dd4f1ea2";

const DYNAMIC_TOOL_UNSUPPORTED: &str = "r9daa2f12a4";

struct WaitingFlagGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> WaitingFlagGuard<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self { flag }
    }
}

impl Drop for WaitingFlagGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

fn user_input_result_frame(
    request: &CodexUserInputRequest,
    ordered_answers: Vec<Vec<String>>,
) -> Result<Value, String> {
    if ordered_answers.len() != request.question_ids.len() {
        return Err(format!(
            "returned {} answer rows for {} questions",
            ordered_answers.len(),
            request.question_ids.len()
        ));
    }
    let mut answers = serde_json::Map::new();
    for (question_id, row) in request.question_ids.iter().zip(ordered_answers) {
        answers.insert(question_id.clone(), json!({"answers": row}));
    }
    Ok(json!({
        "id": &request.id,
        "result": {"answers": answers},
    }))
}

fn user_input_error_frame(id: &CodexServerRequestId, diagnostic: &str) -> Value {
    json!({
        "id": id,
        "error": {
            "code": SERVER_REQUEST_DENIED_CODE,
            "message": diagnostic,
        },
    })
}

fn dynamic_tool_result_frame(id: &Value, success: bool, text: &str) -> Value {
    json!({
        "id": id,
        "result": {
            "success": success,
            "contentItems": [{"type": "inputText", "text": text}],
        },
    })
}

fn approval_result_frame(request: &CodexApprovalRequest, decision: CodexApprovalDecision) -> Value {
    let result = match request.kind {
        CodexApprovalKind::CommandExecution | CodexApprovalKind::FileChange => json!({
            "decision": match decision {
                    CodexApprovalDecision::Accept => "accept",
                    CodexApprovalDecision::AcceptForSession => "acceptForSession",
                    CodexApprovalDecision::Decline => "decline",
                    CodexApprovalDecision::Cancel => "cancel",
                }
        }),
        CodexApprovalKind::Permissions => {
            let accepted = matches!(
                decision,
                CodexApprovalDecision::Accept | CodexApprovalDecision::AcceptForSession
            );
            json!({
                "permissions": if accepted {
                    request.requested_permissions.clone().unwrap_or_else(|| json!({}))
                } else {
                    json!({})
                },
                "scope": if decision == CodexApprovalDecision::AcceptForSession {
                    "session"
                } else {
                    "turn"
                },
            })
        }
    };
    json!({"id": &request.id, "result": result})
}

fn safe_user_input_diagnostic(request: &CodexUserInputRequest, diagnostic: &str) -> String {
    let leaks_question_content = request.questions.iter().any(|question| {
        std::iter::once(question.question.as_str())
            .chain(question.header.as_deref())
            .chain(question.options.iter().map(String::as_str))
            .chain(question.descriptions.iter().map(String::as_str))
            .filter(|text| !text.is_empty())
            .any(|text| diagnostic.contains(text))
    });
    if leaks_question_content {
        return "diagnostic redacted because it contained question content".into();
    }

    let compact = diagnostic.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return "unspecified handler error".into();
    }
    compact.chars().take(256).collect()
}

fn user_input_resolved_notification(thread_id: &str, turn_id: &str) -> CodexAppServerNotification {
    CodexAppServerNotification::Other {
        method: "summon/userInputResolved".into(),
        params: json!({"threadId": thread_id, "turnId": turn_id}),
    }
}

fn dynamic_tool_resolved_notification(
    thread_id: &str,
    turn_id: &str,
) -> CodexAppServerNotification {
    CodexAppServerNotification::Other {
        method: "summon/dynamicToolResolved".into(),
        params: json!({"threadId": thread_id, "turnId": turn_id}),
    }
}

fn approval_resolved_notification(thread_id: &str, turn_id: &str) -> CodexAppServerNotification {
    CodexAppServerNotification::Other {
        method: "summon/approvalResolved".into(),
        params: json!({"threadId": thread_id, "turnId": turn_id}),
    }
}

struct ServerRequestReply {
    /// 完整回帧(含 id),调用方原样写回。
    frame: Value,
    /// 日志用的一句话:为什么这么答。
    reason: &'static str,
}

/// server→client request 的应答表。
///
/// **绝不静默吞 id**:app-server 发出请求后会一直等那条 id 的响应,漏一个就是整轮永久挂死
/// (设计文档 §5.2)。所以每条分支——包括没见过的方法——都产出回帧。这个函数只服务于
/// **没有对应交互处理器**的兼容/降级路径，原则是按 schema 确定性拒绝，让模型换路继续，
/// 不能静默授权，也不能把 app-server 永久挂在未应答 request id 上。
///
/// 形状取自 `codex app-server generate-json-schema` 的 `ServerRequest.json` /
/// `*Response.json`(codex-cli 0.146.0 实测生成),不是猜的。两处易错点:
/// - 新旧两套批准 API 的 decision 枚举**不同**:`item/*` 是裸字符串 `"decline"`,
///   `execCommandApproval`/`applyPatchApproval` 是 `{"denied":{"rejection":…}}` 对象。
/// - `item/permissions/requestApproval` 的响应 schema **没有拒绝变体**,只能授权;
///   空 profile 才是「一项都不给」。
fn server_request_reply(id: &Value, method: &str, params: &Value) -> ServerRequestReply {
    let current_time_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(i64::MAX);
    server_request_reply_at(id, method, params, current_time_at)
}

fn server_request_reply_at(
    id: &Value,
    method: &str,
    params: &Value,
    current_time_at: i64,
) -> ServerRequestReply {
    let result = |value: Value, reason: &'static str| ServerRequestReply {
        frame: json!({"id": id, "result": value}),
        reason,
    };
    let error = |code: i64, message: &str, reason: &'static str| ServerRequestReply {
        frame: json!({"id": id, "error": {"code": code, "message": message}}),
        reason,
    };
    match method {
        // decline = 拒绝但这一轮继续;cancel 会立刻打断整轮,不该由「没人应答」触发。
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => result(
            json!({ "decision": "decline" }),
            summon_i18n::text!("rcc15cc4f89"),
        ),
        // 旧 API 的 ReviewDecision:denied 是带 rejection 的对象,回裸字符串会被反序列化拒收。
        "execCommandApproval" | "applyPatchApproval" => result(
            json!({ "decision": { "denied": { "rejection": ::summon_core::i18n::label(DENY_REASON) } } }),
            summon_i18n::text!("rac72544f6f"),
        ),
        // scope=turn:即使将来 codex 把空 profile 记进缓存,也不外溢到整个 session。
        "item/permissions/requestApproval" => result(
            json!({ "permissions": {}, "scope": "turn" }),
            summon_i18n::text!("rc2d64da10a"),
        ),
        "item/tool/requestUserInput" => result(
            json!({ "answers": empty_user_input_answers(params) }),
            summon_i18n::text!("r38c2c94950"),
        ),
        "mcpServer/elicitation/request" => result(
            json!({ "action": "decline" }),
            summon_i18n::text!("ra4a0f14fd8"),
        ),
        // 回结构化「执行失败」而不是 JSON-RPC error:让模型看见工具失败原因并继续,
        // 而不是把整轮打成协议层错误。
        "item/tool/call" => result(
            json!({
                "success": false,
                "contentItems": [{ "type": "inputText", "text": ::summon_core::i18n::label(DYNAMIC_TOOL_UNSUPPORTED) }],
            }),
            summon_i18n::text!("r4e2cd438a3"),
        ),
        "currentTime/read" => result(
            json!({"currentTimeAt": current_time_at}),
            summon_i18n::text!("re3eb4b8280"),
        ),
        // 这两个要的是 summon 根本没有的凭据。回 error 是唯一诚实的答案 —— 编个假 token
        // 只会让 codex 拿去请求,失败得更晚更难归因。
        "account/chatgptAuthTokens/refresh" => error(
            SERVER_REQUEST_DENIED_CODE,
            summon_i18n::text!("r9aeec3f53f"),
            summon_i18n::text!("r67663269ae"),
        ),
        "attestation/generate" => error(
            SERVER_REQUEST_DENIED_CODE,
            summon_i18n::text!("ra21c29dd89"),
            summon_i18n::text!("r634bb24ac5"),
        ),
        _ => error(
            SERVER_REQUEST_UNKNOWN_CODE,
            "summon does not implement this server request",
            summon_i18n::text!("r1e39bf355b"),
        ),
    }
}

/// 逐题回空答。`answers` 是 questionId → `{answers: []}` 的表;逐题显式回空比整表为空更接近
/// 「用户跳过了」,不容易被 app-server 当成没收到而重问。
fn empty_user_input_answers(params: &Value) -> Value {
    let mut answers = serde_json::Map::new();
    if let Some(questions) = params.get("questions").and_then(Value::as_array) {
        for question in questions {
            if let Some(id) = question.get("id").and_then(Value::as_str) {
                answers.insert(id.to_string(), json!({ "answers": [] }));
            }
        }
    }
    Value::Object(answers)
}

fn remote_error(method: &str, error: &Value) -> CodexAppServerError {
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown remote error")
        .to_string();
    if code == Some(-32601) {
        CodexAppServerError::CapabilityUnavailable {
            method: method.into(),
            detail: message,
        }
    } else {
        CodexAppServerError::Remote {
            method: method.into(),
            code,
            message,
        }
    }
}

fn parse_thread_result(
    method: &str,
    result: &Value,
) -> Result<CodexAppServerThread, CodexAppServerError> {
    serde_json::from_value(result.get("thread").cloned().ok_or_else(|| {
        CodexAppServerError::InvalidResponse {
            operation: method.into(),
            detail: "missing result.thread".into(),
        }
    })?)
    .map_err(|error| CodexAppServerError::InvalidResponse {
        operation: method.into(),
        detail: error.to_string(),
    })
}

fn parse_account_rate_limits(value: &Value) -> Result<CodexAccountRateLimits, String> {
    let default = parse_app_rate_limit_snapshot(
        value
            .get("rateLimits")
            .ok_or_else(|| "missing result.rateLimits".to_string())?,
    );
    let mut by_limit_id = BTreeMap::new();
    if let Some(map) = value.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (id, snapshot) in map {
            let mut parsed = parse_app_rate_limit_snapshot(snapshot);
            if parsed.limit_id.is_empty() {
                parsed.limit_id = id.clone();
            }
            by_limit_id.insert(id.clone(), parsed);
        }
    }
    if !default.limit_id.is_empty() {
        by_limit_id
            .entry(default.limit_id.clone())
            .or_insert_with(|| default.clone());
    }
    Ok(CodexAccountRateLimits {
        default,
        by_limit_id,
    })
}

/// `thread/resume` 的 params 里**允许出现的全部键**。
///
/// 红线,不是风格问题:app-server 一旦在 resume 参数里看到任何模型/服务档位相关的覆盖,就判定
/// 「调用方在钉配置」,提前 return 出 `merge_persisted_resume_metadata`,把这个 thread 持久化的
/// `{model, model_provider, reasoning_effort}` 三元组整个丢回**当前进程的默认值**。
/// 它**不报错** —— 症状是「重启之后模型悄悄换了」,极难归因。
/// (botmux 在 codex-cli 0.145 真机验证并写死了这条;而 schema 的 `ThreadResumeParams` 恰恰接受
/// `model`/`serviceTier`/`approvalPolicy`,是最容易顺手填上的地方。)
///
/// **为什么是白名单而不是禁用键黑名单**:黑名单只挡得住今天想得到的那几个名字,schema 再加一个
/// 模型相关字段(它一直在演进)就会从缝里漏过去,而漏过去的表现恰恰是静默换模型 —— 最难归因的
/// 那一类。白名单反过来:多一个键就红,新字段必须有人显式判断该不该进。
///
/// 需要改模型/档位就走 `turn/start` 的 per-turn `model`/`effort`,那条是协议支持的就地覆盖。
/// `pub(crate)`:会话池的懒重连回归测也要拿它做断言,两处各抄一份表就等于这条红线有两个事实源。
#[allow(dead_code)] // 只被测试引用;生产路径靠 `thread_resume_params` 只写这些键来遵守
pub(crate) const RESUME_ALLOWED_KEYS: &[&str] = &[
    "threadId",
    "cwd",
    "developerInstructions",
    "sandbox",
    "approvalPolicy",
];

/// 抽成纯函数,是为了让上面那条不变量能被单测直接钉住(不需要起真 server)。
///
/// **`developerInstructions` 为什么可以进白名单**(2026-08-21,对着 codex 源码逐行核过):
/// 上面那条红线的机制是 app-server 的 `has_model_resume_override()` —— 它命中就
/// 提前 return 出 `merge_persisted_resume_metadata`,持久化的 {model, provider, effort}
/// 于是不再被恢复。而它**只**看三个键:`model` / `model_provider` / `model_reasoning_effort`。
/// `developerInstructions` / `sandbox` / `approvalPolicy` 都不在其中,走的是另一条
/// `build_thread_config_overrides` 通路,不影响模型三元组。
/// (`sandbox` 会让 `has_permission_override` 命中,但那只影响「要不要恢复持久化的
/// permission profile」—— 我们既然显式钉了 sandbox,不恢复正是想要的。)
/// 红线本身继续成立:**模型类字段一个都不许加**。
#[allow(dead_code)] // 同上:先于调用方落地,见分步计划第 1 步
fn thread_resume_params(
    thread_id: &str,
    cwd: Option<&str>,
    developer_instructions: Option<&str>,
    policy: &CodexPolicy,
) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        // **`sandbox` 必须每次 resume 都发。** codex 的 `PersistedResumeSettings` 只存
        // {approval_policy, approvals_reviewer, active_permission_profile} —— **没有 sandbox_mode**。
        // 不发的话,懒重连之后 sandbox 会悄悄退回那台机器 `~/.codex/config.toml` 的值,
        // 而它的默认是 `read-only`:于是「冷启时好好的,app-server 一死重连就写不了文件」,
        // 且没有任何报错。只在 thread/start 钉是不够的。
        "sandbox": policy.sandbox,
        // approvalPolicy 其实**会**从 rollout 恢复,这里仍显式发:让配置成为唯一事实源。
        // 否则改了 config.yaml 之后,老 thread 会一直沿用建它时那份持久化的旧策略。
        "approvalPolicy": policy.approval_policy,
    });
    if let Some(cwd) = cwd {
        params["cwd"] = Value::String(cwd.to_string());
    }
    // PRIMER **不随 rollout 持久化**(codex 的 `SessionMeta` 只存 base_instructions),
    // 所以每次 resume 都得重发,否则懒重连之后这个 thread 就没有身份了。
    // 调用方只在 `primer_slot=developer` 的 thread 上给值 —— 旧 thread 的 PRIMER 还在
    // history 的 baseInstructions 里,再发一份就是两份。
    if let Some(text) = developer_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        params["developerInstructions"] = Value::String(text.to_string());
    }
    params
}

fn draft_thread_params(response: &Value, cwd: &str) -> Result<Value, CodexAppServerError> {
    let config = response
        .get("config")
        .and_then(Value::as_object)
        .ok_or_else(|| CodexAppServerError::InvalidResponse {
            operation: "config/read".into(),
            detail: "missing config".into(),
        })?;
    let mut disabled = serde_json::Map::new();
    if let Some(servers) = config.get("mcp_servers").filter(|value| !value.is_null()) {
        let servers = servers
            .as_object()
            .ok_or_else(|| CodexAppServerError::InvalidResponse {
                operation: "config/read".into(),
                detail: "invalid mcp_servers".into(),
            })?;
        for name in servers.keys() {
            disabled.insert(name.clone(), json!({"enabled":false}));
        }
    }
    let policy = CodexPolicy {
        sandbox: "read-only".into(),
        approval_policy: "never".into(),
    };
    let mut params = thread_start_params(Some(cwd), None, &policy, None);
    params["ephemeral"] = json!(true);
    params["config"] = json!({"mcp_servers":disabled,"features":{"shell_tool":false,"hooks":false,"apps":false,"plugins":false,"remote_plugin":false,"multi_agent":false},"web_search":"disabled"});
    Ok(params)
}

/// codex 托管会话的权限策略。**sandbox 与 approvalPolicy 是同一条策略的两半**,一起下发。
///
/// 为什么不能省着不发:`SandboxMode` 在 codex 里的 `#[default]` 是 `read-only`
/// (`protocol/src/config_types.rs`)。省略 = 跟随那台机器 `~/.codex/config.toml` 的值,
/// 而一台没配过的机器就会得到「read-only + 永不询问」这个组合 —— agent 写不了任何文件,
/// 又没有审批通道可以升级,症状是工具调用一路失败且没有任何提示。
/// 只钉一半、另一半交给环境,正是仓里反复吃过亏的那个形状。
///
/// 取值分层:内置默认(danger-full-access / never,owner 2026-08-21 按本机实际状态裁定)
/// → `~/.summon/config.yaml` 的 `codex:` 段 → agent.yaml 的 `runtime.codex_*`。
/// 解析与降级规则在 `summon_core::codex_policy`,**到这里的值已经保证合法**。
fn thread_start_params(
    cwd: Option<&str>,
    developer_instructions: Option<&str>,
    policy: &CodexPolicy,
    dynamic_tools: Option<&Value>,
) -> Value {
    let mut params = json!({
        "approvalPolicy": policy.approval_policy,
        "sandbox": policy.sandbox,
        "ephemeral": false,
    });
    if let Some(cwd) = cwd {
        params["cwd"] = Value::String(cwd.to_string());
    }
    // PRIMER 走 `developerInstructions` 而**不是** `baseInstructions`。
    //
    // 两者不是同义词:`baseInstructions` 是**替换**语义 —— codex 的
    // `models-manager/src/model_info.rs` 会拿它整个顶掉 `instructions_template`,
    // 也就是 codex 自己那份 ~21KB 的系统提示词(apply_patch 用法、plan 工具、AGENTS.md
    // 作用域规约、sandbox/approval 语义全在里面)。PRIMER 是身份与飞书排版约定,
    // 不是编码 agent 的行为规约,拿它去顶掉那份提示词是纯损失。
    // `developerInstructions` 则是**追加** —— codex 把它 push 进 developer message
    // bundle,和 AGENTS.md 并列,自己的提示词原样保留。这也和 claude 那条路
    // (`--append-system-prompt-file`,同样是 append)语义对齐。
    //
    // 代价:它**不随 rollout 持久化**,每次 `thread/resume` 都要重发(见 `thread_resume_params`)。
    // 而旧 thread 的 PRIMER 已经以 baseInstructions 的形态焊进了 history、清不掉,
    // 所以新旧两种 thread 的 resume 行为相反,靠 `.session` 的 `primer_slot` 区分。
    if let Some(text) = developer_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        params["developerInstructions"] = Value::String(text.to_string());
    }
    if let Some(tools) = dynamic_tools {
        params["dynamicTools"] = tools.clone();
    }
    params
}

/// summon 给 Codex 注册的客户端能力。模型只看到参数契约，不接触轮凭证；执行时由
/// [`CodexDynamicToolHandler`] 绑定到当前 thread+turn 并再次鉴权。
pub(crate) fn summon_dynamic_tools() -> Value {
    json!([{
        "type": "namespace",
        "name": "summon",
        "description": "Interact with the current Summon chat turn.",
        "tools": [
            {
                "type": "function",
                "name": "preview",
                "description": "Check that an existing absolute local file can be previewed. Mention the path in your reply as an inline code span; summon adds an open button for it at the end of the reply. No URL is returned.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path"],
                    "properties": {
                        "path": {"type": "string"}
                    }
                }
            },
            {
                "type": "function",
                "name": "react",
                "description": "Set the semantic reaction on the Feishu message for the current turn.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["emoji"],
                    "properties": {
                        "emoji": {"type": "string"}
                    }
                }
            },
            {
                "type": "function",
                "name": "send_image",
                "description": "Send an existing absolute local image file to the current Feishu chat.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path"],
                    "properties": {
                        "path": {"type": "string"},
                        "caption": {"type": "string"}
                    }
                }
            }
        ]
    }])
}

/// 跨群 fork 的 `thread/fork` 参数。
///
/// 与 [`thread_start_params`] 逐条对齐,只多一个 `threadId`(源 thread)—— 因为 fork 出来的
/// thread 和冷启出来的一样,之后要被目标 chat 长期使用,所以策略两半、cwd、PRIMER 一个都不能省。
///
/// **`ephemeral: false` 是必须的**:fork 的目的就是让副本持久化到 rollout,否则 app-server 一死
/// 上下文就没了,而 `.session` 里那个 sid 已经落盘 —— 下一轮 resume 会失败,fork 白做。
fn thread_fork_params(
    source_thread_id: &str,
    cwd: Option<&str>,
    developer_instructions: Option<&str>,
    policy: &CodexPolicy,
) -> Value {
    let mut params = json!({
        "threadId": source_thread_id,
        "approvalPolicy": policy.approval_policy,
        "sandbox": policy.sandbox,
        "ephemeral": false,
    });
    if let Some(cwd) = cwd {
        params["cwd"] = Value::String(cwd.to_string());
    }
    if let Some(text) = developer_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        params["developerInstructions"] = Value::String(text.to_string());
    }
    params
}

/// `turn/start` 参数只由这里产出，单测直接钉住 service tier 的三态线协议。
///
/// model/effort/summary 的 `None` 一律省略，避免误覆盖线程持久化配置；service tier 则不同：
/// Standard 必须显式发 JSON null 才能清掉 sticky Fast，Fast 使用 0.147 起的 canonical id
/// `priority`。整个函数保持纯函数，协议升级时不需要起真实 app-server 才能验序列化。
fn turn_start_params(thread_id: &str, prompt: &str, options: &TurnOptions<'_>) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": [{"type": "text", "text": prompt}],
        // 与 thread 级同源:`turn/start.approvalPolicy` 覆盖本轮及后续轮,写死 never 就会
        // 把用户配的策略在第一轮悄悄盖回去。None = 不带这个键,沿用 thread 级设置。
    });
    if let Some(approval) = options.approval_policy {
        params["approvalPolicy"] = Value::String(approval.to_string());
    }
    if let Some(id) = options.client_user_message_id {
        params["clientUserMessageId"] = Value::String(id.to_string());
    }
    for (key, value) in [
        ("model", options.model),
        ("effort", options.effort),
        ("summary", options.summary),
    ] {
        if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
            params[key] = Value::String(value.to_string());
        }
    }
    match options.service_tier.as_ref() {
        None => {}
        Some(CodexServiceTier::Standard) => params["serviceTier"] = Value::Null,
        Some(CodexServiceTier::Fast) => {
            params["serviceTier"] = Value::String("priority".to_string());
        }
    }
    params
}

/// 一轮的 per-turn 覆盖项(schema `TurnStartParams` 的可选字段子集)。
///
/// 为什么必须**每轮**带而不是建 thread 时钉一次:`thread/resume` 绝不许带任何模型相关字段
/// (§5.1 红线,带了 app-server 会把持久化的 `{model, provider, effort}` 整个丢回默认值),
/// 所以懒重连后唯一还能表达「用哪个模型」的地方就是 `turn/start`。
#[derive(Debug, Clone, Default)]
pub struct TurnOptions<'a> {
    pub client_user_message_id: Option<&'a str>,
    pub model: Option<&'a str>,
    /// 逻辑 effort 档位(none/minimal/low/…)。**调用方保证已校验**:非法值由 driver 层降级丢掉,
    /// 到这里就是要么合法要么 None(同 `resolve_launch_lenient` 的 fail-closed 分层)。
    pub effort: Option<&'a str>,
    /// `model_reasoning_summary` 的 RPC 对应物(none/concise/auto/detailed)。
    pub summary: Option<&'a str>,
    /// Codex 服务档位。None=完全不接管；Standard/Fast 都要每轮显式下发以覆盖 sticky 状态。
    pub service_tier: Option<CodexServiceTier>,
    /// 本轮的 approval 策略。**必须与建 thread 时同源** —— 这个字段覆盖本轮及后续轮,
    /// 写死一个值就会把 `thread/start` 时按配置下发的策略在第一轮悄悄盖回去。
    /// None = 不带这个键,沿用 thread 级设置。
    pub approval_policy: Option<&'a str>,
}

/// 通知解析已下沉 core(纯函数、要被 chat 侧 RPC 引擎复用);这里只把错误映射回客户端错误面,
/// 让调用点与对外错误文案都保持不变。
fn parse_notification(
    value: &Value,
) -> Result<Option<CodexAppServerNotification>, CodexAppServerError> {
    summon_core::codex_rpc::parse_notification(value).map_err(CodexAppServerError::from)
}

#[cfg(test)]
mod tests {
    #[test]
    fn process_deadline_cancels_before_process_ownership_ends() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 5"])
            .process_group(0)
            .spawn()
            .unwrap();
        let killer = crate::runtime::executor::ChildKiller::new(child.id() as i32);
        let guard = super::ProcessDeadline::start(
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            killer.clone(),
        );
        drop(guard);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let still_running = child.try_wait().unwrap().is_none();
        killer.kill();
        let _ = child.wait();
        assert!(
            still_running,
            "cancelled deadline must not signal the former process owner"
        );
    }

    use super::*;

    #[test]
    #[ignore = "needs native codex and SUMMON_TEST_READER_BIN; discovers MCP tools without a model turn"]
    fn native_document_reader_mcp_discovery() {
        let root = tempfile::tempdir().unwrap();
        let server = crate::runtime::document_comment_reader::ReaderServer::start(
            root.path(),
            Box::new(|| Ok(())),
            Box::new(|| Ok("synthetic document".into())),
        )
        .unwrap();
        let mut reader = server.server_config().unwrap();
        reader["command"] =
            json!(std::env::var("SUMMON_TEST_READER_BIN").expect("SUMMON_TEST_READER_BIN"));
        let mut config = CodexAppServerConfig {
            args: vec![
                "app-server".into(),
                "--listen".into(),
                "stdio://".into(),
                "-c".into(),
                "features.hooks=false".into(),
                "-c".into(),
                "features.plugins=false".into(),
                "-c".into(),
                "features.apps=false".into(),
            ],
            experimental_api: true,
            ..Default::default()
        };
        config.env.push((
            "CODEX_HOME".into(),
            root.path().join("codex").display().to_string(),
        ));
        std::fs::create_dir_all(root.path().join("codex")).unwrap();
        let mut preflight = CodexAppServerClient::spawn(config.clone()).unwrap();
        config.args.extend(
            preflight
                .draft_mcp_overrides(Some((&server.name, &reader)))
                .unwrap(),
        );
        drop(preflight);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .thread_start_draft(
                &root.path().to_string_lossy(),
                Some((&server.name, &reader)),
            )
            .unwrap();
        let result = client
            .request_value(
                "mcpServerStatus/list",
                json!({"detail":"full"}),
                Duration::from_secs(10),
            )
            .unwrap();
        let servers = result["data"].as_array().expect("server list");
        let matching: Vec<_> = servers
            .iter()
            .filter(|item| item["name"] == server.name)
            .collect();
        assert_eq!(matching.len(), 1, "{result}");
        assert!(
            matching[0]["tools"]
                .as_object()
                .is_some_and(|tools| tools.contains_key("read_document")),
            "{result}"
        );
        assert_eq!(matching[0]["tools"].as_object().unwrap().len(), 1);
        println!("native document reader discovery passed; model turns: 0");
    }
    use crate::runtime::rpc_stdio::STDERR_TAIL_LIMIT_BYTES;
    // 只有断言里按类型名取函数指针要这个名字;生产路径上都是方法调用,故不从本模块再导出。
    use crate::test_support::{fake_rpc_server_log, process_test_guard, write_fake_app_server};
    use summon_core::codex_rpc::CodexAppServerTurnError;

    fn full_user_input_request(id: Value, thread_id: &str, turn_id: &str) -> Value {
        json!({
            "id": id,
            "method": "item/tool/requestUserInput",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": "item-7",
                "questions": [
                    {
                        "id": "q-choice",
                        "header": "范围",
                        "question": "先做哪一项？",
                        "isOther": false,
                        "isSecret": false,
                        "options": [
                            {"label": "协议", "description": "只改 wire adapter"},
                            {"label": "卡片", "description": "同时改交互层"}
                        ]
                    },
                    {
                        "id": "q-note",
                        "header": "补充",
                        "question": "还有什么约束？",
                        "isOther": true,
                        "isSecret": false,
                        "options": null
                    }
                ],
                "isBlocking": false,
                "autoResolutionMs": 30000
            }
        })
    }

    fn full_dynamic_tool_request(id: Value, thread_id: &str, turn_id: &str) -> Value {
        json!({
            "id": id,
            "method": "item/tool/call",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "callId": "call-7",
                "namespace": "summon",
                "tool": "preview",
                "arguments": {"path": "/repo/report.md"}
            }
        })
    }

    /// **Codex 侧的真实 spawn 回归**,并覆盖 sibling-agent 场景。
    ///
    /// 为什么不能靠 ACP 那条:Codex 与 ACP 是**两份独立的 `spawn` 实现**,各自有一句
    /// `env_clear()`。删掉 Codex 那句,ACP 的测试一行都不会红 —— 我一度以为"两条边界代码路径
    /// 一致",是错的(2026-08-14 review 指出)。
    ///
    /// sibling 场景一并验:父进程带着**另一个 agent** 的 `SUMMON_*` 和 `FEISHU_*`,子进程里
    /// 必须只剩当前 agent 的身份 —— 飞书凭据尤其不能串,那等于拿别人的 app 身份发消息。
    #[test]
    fn a_spawned_codex_child_gets_only_this_agents_identity() {
        let _guard = crate::test_support::process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_app_server(tmp.path(), "");

        // 父进程(daemon)带着 sibling 的身份与飞书凭据。
        std::env::set_var("SUMMON_AGENT", "flare-5");
        std::env::set_var("SUMMON_CHAT_ID", "oc_sibling");
        std::env::set_var("SUMMON_CONVERSATION_TAG", "g_sibling");
        std::env::set_var("FEISHU_APP_ID", "cli_sibling_app");
        std::env::set_var("ACP_LEAK_PROBE", "leaked-from-parent");

        // 显式给出当前 agent 的身份(生产由 `RpcPool::child_env` 构造)。
        let mut env = crate::runtime::daemon::essential_system_env();
        env.push(("SUMMON_AGENT".into(), "ambrose".into()));
        env.push(("SUMMON_CHAT_ID".into(), "oc_mine".into()));
        env.push(("SUMMON_CONVERSATION_TAG".into(), "g_mine".into()));

        let client = CodexAppServerClient::spawn(CodexAppServerConfig {
            program: script.to_string_lossy().into_owned(),
            args: vec![],
            request_timeout: Duration::from_secs(15),
            env,
            ..CodexAppServerConfig::default()
        })
        .expect("假 app-server 应能起来");
        drop(client);

        for key in [
            "SUMMON_AGENT",
            "SUMMON_CHAT_ID",
            "SUMMON_CONVERSATION_TAG",
            "FEISHU_APP_ID",
            "ACP_LEAK_PROBE",
        ] {
            std::env::remove_var(key);
        }

        let dumped = std::fs::read_to_string(tmp.path().join("requests.jsonl.child-env"))
            .expect("fake app-server 应把自己的环境写出来");
        assert!(
            dumped.contains("SUMMON_AGENT=ambrose") && dumped.contains("SUMMON_CHAT_ID=oc_mine"),
            "子进程必须拿到当前 agent 的身份,实际:\n{dumped}"
        );
        assert!(
            !dumped.contains("flare-5") && !dumped.contains("oc_sibling"),
            "sibling 的身份一个字都不能漏进来,实际:\n{dumped}"
        );
        assert!(
            !dumped.contains("cli_sibling_app"),
            "sibling 的飞书凭据绝不能串 —— 那等于拿别人的 app 身份发消息,实际:\n{dumped}"
        );
        assert!(
            !dumped.contains("leaked-from-parent"),
            "未被显式注入的父进程变量必须被 env_clear 挡掉,实际:\n{dumped}"
        );
    }

    fn fake_server(script_body: &str) -> (tempfile::TempDir, CodexAppServerConfig, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_app_server(tmp.path(), script_body);
        let config = CodexAppServerConfig {
            program: script.to_string_lossy().into_owned(),
            args: vec![],
            // workspace 全量并发测试下进程调度可能明显慢于单测；具体 timeout 行为由每个
            // request 的 override 单独验证，这里只给 fake server 初始化留足余量。
            request_timeout: Duration::from_secs(15),
            ..CodexAppServerConfig::default()
        };
        let log = fake_rpc_server_log(tmp.path());
        (tmp, config, log)
    }

    fn wait_for_logged_response(log: &std::path::Path, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(response) = std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|value| value.get("id") == Some(&json!(id)))
            {
                return response;
            }
            assert!(
                Instant::now() < deadline,
                "等待 fake app-server 记录 response(id={id}) 超时"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn typed_user_input_preserves_wire_ids_and_maps_complete_questions() {
        let frame = full_user_input_request(json!(i64::MAX), "thread-9", "turn-3");
        let parsed = parse_codex_user_input_request(&frame).unwrap();

        assert_eq!(parsed.request.id, CodexServerRequestId::Integer(i64::MAX));
        assert_eq!(parsed.request.thread_id, "thread-9");
        assert_eq!(parsed.request.turn_id, "turn-3");
        assert_eq!(parsed.request.item_id, "item-7");
        assert_eq!(parsed.request.question_ids, ["q-choice", "q-note"]);
        assert!(!parsed.request.is_blocking);
        assert_eq!(parsed.request.auto_resolution_ms, Some(30_000));
        assert!(!parsed.contains_secret);

        let choice = &parsed.request.questions[0];
        assert_eq!(choice.header.as_deref(), Some("范围"));
        assert_eq!(choice.question, "先做哪一项？");
        assert_eq!(choice.options, ["协议", "卡片"]);
        assert_eq!(choice.descriptions, ["只改 wire adapter", "同时改交互层"]);
        assert!(!choice.allow_other);

        let note = &parsed.request.questions[1];
        assert_eq!(note.header.as_deref(), Some("补充"));
        assert!(note.options.is_empty());
        assert!(note.descriptions.is_empty());
        assert!(note.allow_other);
    }

    #[test]
    fn dynamic_tool_contract_registers_the_three_strict_summon_tools() {
        let tools = summon_dynamic_tools();
        assert_eq!(tools[0]["type"], "namespace");
        assert_eq!(tools[0]["name"], "summon");
        let registered = tools[0]["tools"].as_array().unwrap();
        assert_eq!(registered.len(), 3);
        assert_eq!(registered[0]["name"], "preview");
        assert_eq!(registered[0]["inputSchema"]["required"], json!(["path"]));
        assert_eq!(registered[1]["name"], "react");
        assert_eq!(registered[1]["inputSchema"]["required"], json!(["emoji"]));
        assert_eq!(registered[2]["name"], "send_image");
        assert_eq!(registered[2]["inputSchema"]["required"], json!(["path"]));
        for tool in registered {
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
    }

    #[test]
    fn typed_dynamic_tool_preserves_identity_and_arguments() {
        let request = parse_codex_dynamic_tool_request(&full_dynamic_tool_request(
            json!(i64::MAX),
            "thread-9",
            "turn-3",
        ))
        .unwrap();

        assert_eq!(request.id, CodexServerRequestId::Integer(i64::MAX));
        assert_eq!(request.thread_id, "thread-9");
        assert_eq!(request.turn_id, "turn-3");
        assert_eq!(request.call_id, "call-7");
        assert_eq!(request.namespace.as_deref(), Some("summon"));
        assert_eq!(request.tool, "preview");
        assert_eq!(request.arguments, json!({"path": "/repo/report.md"}));
    }

    #[test]
    fn typed_dynamic_tool_rejects_missing_or_empty_identity_fields() {
        let base = full_dynamic_tool_request(json!(1), "thread-9", "turn-3");
        for field in ["threadId", "turnId", "callId", "tool"] {
            let mut request = base.clone();
            request["params"][field] = json!("  ");
            assert!(
                parse_codex_dynamic_tool_request(&request).is_err(),
                "空的 {field} 必须被拒绝"
            );
        }
    }

    #[test]
    fn typed_user_input_accepts_string_id_and_legacy_missing_is_blocking() {
        let mut frame = full_user_input_request(json!("request-string"), "thread-9", "turn-3");
        frame["params"]
            .as_object_mut()
            .unwrap()
            .remove("isBlocking");

        let parsed = parse_codex_user_input_request(&frame).unwrap();
        assert_eq!(
            parsed.request.id,
            CodexServerRequestId::String("request-string".into())
        );
        assert!(parsed.request.is_blocking);
    }

    #[test]
    fn typed_user_input_rejects_invalid_question_count_and_ids() {
        let base = full_user_input_request(json!("request-string"), "thread-9", "turn-3");

        let mut no_questions = base.clone();
        no_questions["params"]["questions"] = json!([]);
        assert!(parse_codex_user_input_request(&no_questions).is_err());

        let mut four_questions = base.clone();
        let mut third = four_questions["params"]["questions"][0].clone();
        third["id"] = json!("q3");
        let mut fourth = third.clone();
        fourth["id"] = json!("q4");
        let questions = four_questions["params"]["questions"]
            .as_array_mut()
            .unwrap();
        questions.push(third);
        questions.push(fourth);
        assert!(parse_codex_user_input_request(&four_questions).is_err());

        let mut duplicate_id = base.clone();
        duplicate_id["params"]["questions"][1]["id"] = json!("q-choice");
        assert!(parse_codex_user_input_request(&duplicate_id).is_err());

        let mut empty_id = base;
        empty_id["params"]["questions"][0]["id"] = json!("   ");
        assert!(parse_codex_user_input_request(&empty_id).is_err());
    }

    #[test]
    fn ordered_answers_map_partial_and_skipped_rows_to_original_question_ids() {
        let frame = full_user_input_request(json!(i64::MAX), "thread-9", "turn-3");
        let parsed = parse_codex_user_input_request(&frame).unwrap();
        let response = user_input_result_frame(
            &parsed.request,
            vec![vec!["协议".into(), "自定义补充".into()], vec![]],
        )
        .unwrap();

        assert_eq!(response["id"], json!(i64::MAX));
        assert_eq!(
            response["result"]["answers"]["q-choice"]["answers"],
            json!(["协议", "自定义补充"])
        );
        assert_eq!(
            response["result"]["answers"]["q-note"]["answers"],
            json!([])
        );
    }

    struct StaticUserInputHandler<'a> {
        waiting: &'a AtomicBool,
        calls: &'a std::sync::atomic::AtomicUsize,
        answer: Result<Vec<Vec<String>>, String>,
    }

    impl CodexUserInputHandler for StaticUserInputHandler<'_> {
        fn handle_user_input(
            &self,
            _request: &CodexUserInputRequest,
            _cancelled: &AtomicBool,
            session_alive: &AtomicBool,
        ) -> Result<Vec<Vec<String>>, String> {
            assert!(
                self.waiting.load(Ordering::Acquire),
                "handler 运行期间 waiting 必须已经置位"
            );
            assert!(session_alive.load(Ordering::Acquire));
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.answer.clone()
        }
    }

    #[test]
    fn typed_user_input_round_trip_returns_local_heartbeat_and_restores_waiting() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"ask-1","method":"item/tool/requestUserInput","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-1","questions":[{"id":"q1","header":"选择","question":"选哪个？","isOther":false,"isSecret":false,"options":[{"label":"A","description":"第一项"}]},{"id":"q2","header":"补充","question":"要补充吗？","isOther":true,"isSecret":false,"options":null}],"isBlocking":true,"autoResolutionMs":null}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let waiting = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let handler = StaticUserInputHandler {
            waiting: &waiting,
            calls: &calls,
            answer: Ok(vec![vec!["A".into()], vec![]]),
        };
        let notification = client
            .recv_notification_with_user_input(
                Some(Duration::from_secs(5)),
                "thread-1",
                "turn-1",
                &handler,
                &cancelled,
                &waiting,
            )
            .unwrap();

        assert_eq!(
            notification,
            CodexAppServerNotification::Other {
                method: "summon/userInputResolved".into(),
                params: json!({"threadId": "thread-1", "turnId": "turn-1"}),
            }
        );
        assert!(!waiting.load(Ordering::Acquire));
        assert_eq!(calls.load(Ordering::Acquire), 1);

        let response = wait_for_logged_response(&log, "ask-1");
        assert_eq!(response["result"]["answers"]["q1"]["answers"], json!(["A"]));
        assert_eq!(response["result"]["answers"]["q2"]["answers"], json!([]));
    }

    #[test]
    fn dynamic_tool_round_trip_returns_text_content_and_local_heartbeat() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"tool-1","method":"item/tool/call","params":{"threadId":"thread-1","turnId":"turn-1","callId":"call-1","namespace":"summon","tool":"preview","arguments":{"path":"/repo/report.md"}}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let cancelled = AtomicBool::new(false);
        let waiting = AtomicBool::new(false);
        let handler = |request: &CodexDynamicToolRequest| {
            assert_eq!(request.arguments["path"], "/repo/report.md");
            Ok("https://preview.example/report".to_string())
        };
        let notification = client
            .recv_notification_with_handlers(
                Some(Duration::from_secs(5)),
                "thread-1",
                "turn-1",
                None,
                Some(&handler),
                None,
                &cancelled,
                &waiting,
            )
            .unwrap();

        assert_eq!(
            notification,
            CodexAppServerNotification::Other {
                method: "summon/dynamicToolResolved".into(),
                params: json!({"threadId": "thread-1", "turnId": "turn-1"}),
            }
        );
        let response = wait_for_logged_response(&log, "tool-1");
        assert_eq!(response["result"]["success"], true);
        assert_eq!(
            response["result"]["contentItems"],
            json!([{"type": "inputText", "text": "https://preview.example/report"}])
        );
    }

    /// 开关关掉(或 traex 这类没装配处理器的轮)时,工具本来就不会注册;但 rollout 里
    /// 已经带着定义的老 thread 仍会发 `item/tool/call`。那时必须回一帧「执行失败」——
    /// **不回帧就是整轮永久挂死**,回 JSON-RPC error 又会把它升级成协议层故障。
    #[test]
    fn a_tool_call_without_a_handler_gets_a_failed_result_instead_of_silence() {
        let _guard = process_test_guard();
        // 假 server 收到工具调用的回帧才推下一条增量:收帧拿到的第一条通知只能是它,
        // 工具调用本身不该冒成通知,没回帧则一路等到超时。
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"tool-orphan","method":"item/tool/call","params":{"threadId":"thread-1","turnId":"turn-1","callId":"call-1","namespace":"summon","tool":"preview","arguments":{"path":"/repo/report.md"}}}' ;;
    *'"id":"tool-orphan"'*) printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"m","delta":"after-tool"}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let cancelled = AtomicBool::new(false);
        let waiting = AtomicBool::new(false);
        let outcome = client.recv_notification_with_handlers(
            Some(Duration::from_secs(5)),
            "thread-1",
            "turn-1",
            None,
            None,
            None,
            &cancelled,
            &waiting,
        );
        assert!(
            matches!(
                &outcome,
                Ok(CodexAppServerNotification::AgentMessageDelta { delta, .. }) if delta == "after-tool"
            ),
            "工具调用由通用兜底就地回掉、不冒成通知,之后才轮到回帧换来的那条增量:{outcome:?}"
        );

        let response = wait_for_logged_response(&log, "tool-orphan");
        assert_eq!(response["result"]["success"], false);
        assert_eq!(
            response["result"]["contentItems"][0]["type"], "inputText",
            "失败也要按 schema 的形状回,否则 codex 解不出来"
        );
    }

    #[test]
    fn dynamic_tool_rejects_cross_turn_requests_without_calling_handler() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"tool-cross-turn","method":"item/tool/call","params":{"threadId":"thread-1","turnId":"turn-other","callId":"call-1","namespace":"summon","tool":"preview","arguments":{"path":"/repo/report.md"}}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let cancelled = AtomicBool::new(false);
        let waiting = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let handler = |_request: &CodexDynamicToolRequest| {
            calls.fetch_add(1, Ordering::AcqRel);
            Ok("should not run".to_string())
        };
        client
            .recv_notification_with_handlers(
                Some(Duration::from_secs(5)),
                "thread-1",
                "turn-1",
                None,
                Some(&handler),
                None,
                &cancelled,
                &waiting,
            )
            .unwrap();

        assert_eq!(calls.load(Ordering::Acquire), 0);
        let response = wait_for_logged_response(&log, "tool-cross-turn");
        assert_eq!(response["result"]["success"], false);
    }

    #[test]
    fn dynamic_tool_maps_handler_failure_and_skips_work_after_turn_cancellation() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"tool-error","method":"item/tool/call","params":{"threadId":"thread-1","turnId":"turn-1","callId":"call-1","namespace":"summon","tool":"preview","arguments":{"path":"/repo/report.md"}}}' '{"id":"tool-cancelled","method":"item/tool/call","params":{"threadId":"thread-1","turnId":"turn-1","callId":"call-2","namespace":"summon","tool":"preview","arguments":{"path":"/repo/report.md"}}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let cancelled = AtomicBool::new(false);
        let waiting = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let handler = |_request: &CodexDynamicToolRequest| {
            calls.fetch_add(1, Ordering::AcqRel);
            Err("sensitive internal failure".to_string())
        };
        client
            .recv_notification_with_handlers(
                Some(Duration::from_secs(5)),
                "thread-1",
                "turn-1",
                None,
                Some(&handler),
                None,
                &cancelled,
                &waiting,
            )
            .unwrap();
        cancelled.store(true, Ordering::Release);
        client
            .recv_notification_with_handlers(
                Some(Duration::from_secs(5)),
                "thread-1",
                "turn-1",
                None,
                Some(&handler),
                None,
                &cancelled,
                &waiting,
            )
            .unwrap();

        assert_eq!(calls.load(Ordering::Acquire), 1);
        for id in ["tool-error", "tool-cancelled"] {
            let response = wait_for_logged_response(&log, id);
            assert_eq!(response["result"]["success"], false);
            assert!(!response.to_string().contains("sensitive internal failure"));
        }
    }

    #[test]
    fn approval_round_trip_uses_the_current_thread_turn_and_typed_decision() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"approval-1","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-1","startedAtMs":1,"command":"git push","cwd":"/repo","reason":"publish"}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let cancelled = AtomicBool::new(false);
        let waiting = AtomicBool::new(false);
        // 判决必须非 Accept:与写死批准同值就分不出「透传了」和「根本没问」。计数同理。
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let handler = |request: &CodexApprovalRequest, _: &AtomicBool, _: &AtomicBool| {
            calls.fetch_add(1, Ordering::AcqRel);
            assert_eq!(request.item_id, "item-1");
            assert_eq!(request.command.as_deref(), Some("git push"));
            CodexApprovalDecision::Decline
        };
        let notification = client
            .recv_notification_with_handlers(
                Some(Duration::from_secs(5)),
                "thread-1",
                "turn-1",
                None,
                None,
                Some(&handler),
                &cancelled,
                &waiting,
            )
            .unwrap();
        assert_eq!(
            notification,
            CodexAppServerNotification::Other {
                method: "summon/approvalResolved".into(),
                params: json!({"threadId": "thread-1", "turnId": "turn-1"}),
            }
        );
        assert!(!waiting.load(Ordering::Acquire));
        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "审批必须恰好问一次 handler —— 零次说明分派点被摘掉了"
        );
        let response = wait_for_logged_response(&log, "approval-1");
        assert_eq!(
            response["result"]["decision"], "decline",
            "线上帧必须原样透传 handler 的判决;这里若是 accept,说明有人在分派点写死了批准"
        );
    }

    #[test]
    fn permissions_approval_round_trip_carries_the_handler_scope_over_the_wire() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"approval-perm","method":"item/permissions/requestApproval","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-1","startedAtMs":1,"cwd":"/repo","permissions":{"network":{"enabled":true}}}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let cancelled = AtomicBool::new(false);
        let waiting = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let handler = |_: &CodexApprovalRequest, _: &AtomicBool, _: &AtomicBool| {
            calls.fetch_add(1, Ordering::AcqRel);
            CodexApprovalDecision::AcceptForSession
        };
        client
            .recv_notification_with_handlers(
                Some(Duration::from_secs(5)),
                "thread-1",
                "turn-1",
                None,
                None,
                Some(&handler),
                &cancelled,
                &waiting,
            )
            .unwrap();

        assert_eq!(calls.load(Ordering::Acquire), 1);
        let response = wait_for_logged_response(&log, "approval-perm");
        // `scope` 只有 handler 答 AcceptForSession 才是 session。分派点若写死任何单一判决,
        // 这里读到的就是 turn(Accept/Decline/Cancel 全部映射成 turn)。
        assert_eq!(response["result"]["scope"], "session");
        assert_eq!(
            response["result"]["permissions"],
            json!({"network": {"enabled": true}})
        );
    }

    #[test]
    fn approval_response_never_grants_permissions_on_decline_or_cancel() {
        let request = CodexApprovalRequest {
            id: CodexServerRequestId::String("approval-permissions".into()),
            thread_id: "thread-1".into(),
            turn_id: "turn-1".into(),
            item_id: "item-1".into(),
            kind: CodexApprovalKind::Permissions,
            reason: None,
            command: None,
            cwd: Some("/repo".into()),
            requested_permissions: Some(json!({"network": {"enabled": true}})),
            available_decisions: Vec::new(),
        };
        for denied in [
            CodexApprovalDecision::Decline,
            CodexApprovalDecision::Cancel,
        ] {
            let frame = approval_result_frame(&request, denied);
            assert_eq!(frame["result"]["permissions"], json!({}));
            assert_eq!(frame["result"]["scope"], "turn");
        }
        let accepted = approval_result_frame(&request, CodexApprovalDecision::AcceptForSession);
        assert_eq!(
            accepted["result"]["permissions"],
            json!({"network": {"enabled": true}})
        );
        assert_eq!(accepted["result"]["scope"], "session");
    }

    #[test]
    fn approval_rejects_cross_turn_without_calling_the_handler() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"approval-cross-turn","method":"item/permissions/requestApproval","params":{"threadId":"thread-1","turnId":"turn-other","itemId":"item-1","startedAtMs":1,"cwd":"/repo","permissions":{"network":{"enabled":true}}}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let cancelled = AtomicBool::new(false);
        let waiting = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let handler = |_: &CodexApprovalRequest, _: &AtomicBool, _: &AtomicBool| {
            calls.fetch_add(1, Ordering::AcqRel);
            CodexApprovalDecision::Accept
        };
        client
            .recv_notification_with_handlers(
                Some(Duration::from_secs(5)),
                "thread-1",
                "turn-1",
                None,
                None,
                Some(&handler),
                &cancelled,
                &waiting,
            )
            .unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 0);
        let response = wait_for_logged_response(&log, "approval-cross-turn");
        assert_eq!(response["result"]["permissions"], json!({}));
        assert_eq!(response["result"]["scope"], "turn");
    }

    #[test]
    fn thread_start_retries_without_dynamic_tools_when_experimental_params_are_rejected() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*)
        case "$line" in
          *'dynamicTools'*) printf '%s\n' '{"id":2,"error":{"code":-32602,"message":"Invalid params"}}' ;;
          *) printf '%s\n' '{"id":3,"result":{"thread":{"id":"thread-fallback","turns":[]}}}' ;;
        esac ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let tools = summon_dynamic_tools();
        let thread = client
            .thread_start_with_dynamic_tools(
                Some("/repo"),
                None,
                &CodexPolicy::default(),
                Some(&tools),
                None,
            )
            .unwrap();

        assert_eq!(thread.id, "thread-fallback");
        let starts: Vec<Value> = std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|frame: &Value| frame["method"] == "thread/start")
            .collect();
        assert_eq!(starts.len(), 2);
        assert!(starts[0]["params"].get("dynamicTools").is_some());
        assert!(starts[1]["params"].get("dynamicTools").is_none());
    }

    #[test]
    fn typed_user_input_handler_observes_app_server_eof_while_waiting() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"ask-dead","method":"item/tool/requestUserInput","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-1","questions":[{"id":"q1","header":"确认","question":"还在吗？","isOther":false,"isSecret":false,"options":null}],"isBlocking":true,"autoResolutionMs":null}}'; exit 0 ;;
"#;
        let (_tmp, config, _log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let waiting = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let saw_eof = AtomicBool::new(false);
        let handler = |_request: &CodexUserInputRequest,
                       _cancelled: &AtomicBool,
                       session_alive: &AtomicBool| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while session_alive.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if !session_alive.load(Ordering::Acquire) {
                saw_eof.store(true, Ordering::Release);
            }
            Err("app-server exited while waiting".to_string())
        };
        let _ = client.recv_notification_with_user_input(
            Some(Duration::from_secs(5)),
            "thread-1",
            "turn-1",
            &handler,
            &cancelled,
            &waiting,
        );

        assert!(saw_eof.load(Ordering::Acquire));
        assert!(!waiting.load(Ordering::Acquire));
    }

    #[test]
    fn typed_user_input_rejects_mismatch_secret_and_handler_error_without_prompt_leak() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"id":"ask-mismatch","method":"item/tool/requestUserInput","params":{"threadId":"other-thread","turnId":"turn-1","itemId":"item-1","questions":[{"id":"q1","header":"不要泄漏的标题","question":"不要泄漏的错轮题面","isOther":false,"isSecret":false,"options":null}],"isBlocking":true,"autoResolutionMs":null}}' '{"id":"ask-secret","method":"item/tool/requestUserInput","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-2","questions":[{"id":"q2","header":"密码","question":"不要泄漏的秘密题面","isOther":false,"isSecret":true,"options":null}],"isBlocking":true,"autoResolutionMs":null}}' '{"id":"ask-error","method":"item/tool/requestUserInput","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-3","questions":[{"id":"q3","header":"普通题","question":"不要泄漏的 handler 题面","isOther":false,"isSecret":false,"options":null}],"isBlocking":true,"autoResolutionMs":null}}' '{"id":"ask-schema","method":"item/tool/requestUserInput","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-4","questions":[{"id":"duplicate","header":"格式题一","question":"不要泄漏的 schema 题面一","isOther":false,"isSecret":false,"options":null},{"id":"duplicate","header":"格式题二","question":"不要泄漏的 schema 题面二","isOther":false,"isSecret":false,"options":null}],"isBlocking":true,"autoResolutionMs":null}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-1", "继续", &TurnOptions::default(), None)
            .unwrap();

        let waiting = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let handler = StaticUserInputHandler {
            waiting: &waiting,
            calls: &calls,
            answer: Err("gate was cancelled; 不要泄漏的 handler 题面".into()),
        };
        for expected_request_thread in ["other-thread", "thread-1", "thread-1", "thread-1"] {
            let notification = client
                .recv_notification_with_user_input(
                    Some(Duration::from_secs(5)),
                    "thread-1",
                    "turn-1",
                    &handler,
                    &cancelled,
                    &waiting,
                )
                .unwrap();
            assert_eq!(
                notification,
                CodexAppServerNotification::Other {
                    method: "summon/userInputResolved".into(),
                    params: json!({
                        "threadId": expected_request_thread,
                        "turnId": "turn-1",
                    }),
                }
            );
            assert!(!waiting.load(Ordering::Acquire));
        }
        assert_eq!(calls.load(Ordering::Acquire), 1);

        for id in ["ask-mismatch", "ask-secret", "ask-error", "ask-schema"] {
            let response = wait_for_logged_response(&log, id);
            assert!(
                response.get("error").is_some(),
                "{id} 必须收到 JSON-RPC error"
            );
            let error = response["error"].to_string();
            assert!(error.contains("summon"), "错误必须有可诊断来源:{error}");
            for prompt in [
                "不要泄漏的标题",
                "不要泄漏的错轮题面",
                "不要泄漏的秘密题面",
                "不要泄漏的 handler 题面",
                "不要泄漏的 schema 题面一",
                "不要泄漏的 schema 题面二",
            ] {
                assert!(!error.contains(prompt), "错误帧泄漏了题面:{error}");
            }
        }
    }

    /// 问**跑起来的那个 app-server**要全部合法方法名。
    ///
    /// 手法:发一个不存在的方法,serde 的报错会把 `expected one of \`a\`, \`b\`, …` 全列出来。
    /// 这比 `generate-json-schema` 权威 —— dump 会把 experimental 方法 gate 掉,
    /// 而它们在运行时是能调的(首跑就被这个咬过一次)。
    fn live_method_names() -> std::collections::BTreeSet<String> {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};

        let mut child = Command::new("codex")
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("起 codex app-server");
        {
            let stdin = child.stdin.as_mut().expect("stdin");
            // 不握手也能触发方法名反序列化报错;只要那份清单,不建任何会话、不消耗 token。
            writeln!(
                stdin,
                r#"{{"id":1,"method":"summon/definitely-not-a-real-method","params":{{}}}}"#
            )
            .expect("写探针");
        }
        let stdout = child.stdout.take().expect("stdout");
        let mut names = std::collections::BTreeSet::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(message) = value["error"]["message"].as_str() else {
                continue;
            };
            let Some(list) = message.split("expected one of ").nth(1) else {
                continue;
            };
            for token in list.split(',') {
                let name = token.trim().trim_matches('`').trim();
                if !name.is_empty() {
                    names.insert(name.to_string());
                }
            }
            break;
        }
        let _ = child.kill();
        let _ = child.wait();
        names
    }

    /// **协议漂移检测**(真机,`cargo test -p summon -- --ignored codex_app_server_schema`)。
    ///
    /// 存在的理由:app-server 对**未知请求字段静默忽略** —— 实测给 `turn/start` 塞一个
    /// `zzzTotallyFakeField` 照样走到「thread not found」,一声不吭。也就是说 summon 拼错一个
    /// camelCase 键、或某个字段在上游被删掉,行为直接降级而**没有任何报错**。这类问题在真实
    /// 会话里的表现是「模型好像没按配置走」,几乎无法归因。
    ///
    /// 好在 codex 自己给了判据源:`codex app-server generate-json-schema --experimental` 能从
    /// **这台机器上装的那个二进制**导出完整 JSON Schema。于是把「summon 实际发出去的方法与
    /// 字段」拿去比对它,就是一道真正守得住的闸。
    ///
    /// 现成的实例:`thread/contextUsage` 在 codex 0.147 里根本不存在(实测报 `unknown variant`),
    /// 它只在 traex 那个 fork 上有 —— 所以它被显式排除,见下面的 `TRAEX_ONLY_METHODS`。
    ///
    /// **这道闸守得住什么、守不住什么**(别高估它):
    /// - 守得住:上游删方法 / 改方法名 / 删字段 / 改字段名 —— 也就是已经真实发生过的那一类。
    /// - 守不住:summon 新增了方法却忘了加进 `CONTROL_METHODS`。三个热路径的 params
    ///   (start / resume / turn)是**从产参函数本身取键**的,新增字段自动被覆盖;
    ///   控制面方法名仍是手写清单。
    #[test]
    #[ignore = "需要本机装了 codex;只导出 schema,不起会话、不消耗模型 token"]
    fn codex_app_server_schema_still_has_every_method_and_field_summon_sends() {
        use std::collections::BTreeSet;

        /// 只在 traex(codex 的 fork)上存在的方法。codex 本体没有,比对时必须排除,
        /// 否则这道闸会因为「codex 缺一个它本来就没有的方法」长期红着,然后被人忽略。
        const TRAEX_ONLY_METHODS: &[&str] = &["thread/contextUsage"];

        /// summon 调用的控制面方法。**新增方法请往这里加一行** —— 漏加不会让测试变红,
        /// 只会让这道闸少守一条,所以它是清单而不是断言。
        const CONTROL_METHODS: &[&str] = &[
            "initialize",
            "thread/start",
            "thread/resume",
            "thread/fork",
            "thread/read",
            "thread/list",
            "thread/name/set",
            "thread/goal/get",
            "thread/goal/set",
            "thread/goal/clear",
            "thread/compact/start",
            "thread/backgroundTerminals/list",
            "thread/backgroundTerminals/terminate",
            "turn/start",
            "turn/steer",
            "turn/interrupt",
            "account/rateLimits/read",
            "model/list",
            "mcpServerStatus/list",
            "skills/list",
            "hooks/list",
            "plugin/list",
            "experimentalFeature/list",
        ];

        /// summon 的 `parse_notification` 认得的通知方法。少一条不报错,只会静默丢事件 ——
        /// 正文流少一段、usage 挂不上,都不会有任何提示,所以逐条钉住。
        const PARSED_NOTIFICATIONS: &[&str] = &[
            "turn/started",
            "turn/completed",
            "item/started",
            "item/completed",
            "item/agentMessage/delta",
            "item/reasoning/textDelta",
            "item/reasoning/summaryTextDelta",
            "turn/plan/updated",
            "thread/tokenUsage/updated",
            "account/rateLimits/updated",
            "error",
        ];

        let tmp = tempfile::tempdir().expect("建临时目录");
        let out = tmp.path().join("schema");
        let status = std::process::Command::new("codex")
            .args([
                "app-server",
                "generate-json-schema",
                "--experimental",
                "--out",
            ])
            .arg(&out)
            .status();
        let status = match status {
            Ok(status) => status,
            Err(error) => {
                panic!("本机没装 codex 或起不来({error});这条测试是 ignored,按需在装了 codex 的机器上跑")
            }
        };
        assert!(status.success(), "generate-json-schema 失败");

        let read = |name: &str| -> serde_json::Value {
            let path = out.join(name);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("读不到 {}: {e}", path.display()));
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} 不是合法 JSON: {e}", name))
        };

        let server_requests = serde_json::to_string(&read("ServerRequest.json")).unwrap();
        assert!(
            server_requests.contains("item/tool/call"),
            "本机 app-server 的实验协议缺 item/tool/call"
        );
        let dynamic_response = read("DynamicToolCallResponse.json");
        let required = dynamic_response["required"]
            .as_array()
            .expect("DynamicToolCallResponse.required");
        assert!(required.contains(&json!("success")));
        assert!(required.contains(&json!("contentItems")));
        let protocol =
            serde_json::to_string(&read("codex_app_server_protocol.schemas.json")).unwrap();
        assert!(
            protocol.contains("dynamicToolCall"),
            "本机 app-server 的 ThreadItem 缺 dynamicToolCall"
        );
        for (file, required_fields) in [
            (
                "CommandExecutionRequestApprovalParams.json",
                &["threadId", "turnId", "itemId"][..],
            ),
            (
                "FileChangeRequestApprovalParams.json",
                &["threadId", "turnId", "itemId"][..],
            ),
            (
                "PermissionsRequestApprovalParams.json",
                &["threadId", "turnId", "itemId", "permissions"][..],
            ),
        ] {
            let schema = read(file);
            let required = schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{file}.required"));
            for field in required_fields {
                assert!(
                    required.contains(&json!(field)),
                    "本机 app-server 的 {file} 缺必填字段 {field}"
                );
            }
        }
        for file in [
            "CommandExecutionRequestApprovalResponse.json",
            "FileChangeRequestApprovalResponse.json",
        ] {
            let schema = serde_json::to_string(&read(file)).unwrap();
            for decision in ["accept", "acceptForSession", "decline", "cancel"] {
                assert!(schema.contains(decision), "{file} 缺 decision={decision}");
            }
        }
        let permission_response = read("PermissionsRequestApprovalResponse.json");
        let permission_required = permission_response["required"]
            .as_array()
            .expect("PermissionsRequestApprovalResponse.required");
        assert!(permission_required.contains(&json!("permissions")));

        let plan_update = read("v2/TurnPlanUpdatedNotification.json");
        let plan_required = plan_update["required"]
            .as_array()
            .expect("TurnPlanUpdatedNotification.required");
        for field in ["threadId", "turnId", "plan"] {
            assert!(
                plan_required.contains(&json!(field)),
                "本机 app-server 的计划更新通知缺必填字段 {field}"
            );
        }

        // --- 请求侧:方法名 + params 字段 ---
        let client_request = read("ClientRequest.json");
        let definitions = client_request["definitions"]
            .as_object()
            .expect("ClientRequest.definitions");
        let mut methods: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
        for variant in client_request["oneOf"].as_array().expect("oneOf") {
            let Some(method) = variant["properties"]["method"]["enum"][0].as_str() else {
                continue;
            };
            // params 的字段集从 `$ref` 指向的定义里取;取不到就记成空集(该方法只校验存在性)。
            let keys = variant["properties"]["params"]["$ref"]
                .as_str()
                .and_then(|r| r.rsplit('/').next())
                .and_then(|name| definitions.get(name))
                .and_then(|def| def["properties"].as_object())
                .map(|props| props.keys().cloned().collect())
                .unwrap_or_default();
            methods.insert(method.to_string(), keys);
        }
        assert!(
            methods.len() > 50,
            "只解析出 {} 个方法,schema 形态可能变了 —— 先修这条测试再谈漂移",
            methods.len()
        );

        // 方法存在性再向**跑起来的那个二进制**复核:发一个不存在的方法,它的报错会把全部合法
        // 方法名列出来。schema 负责字段，运行时清单负责发现 feature gate 与实际路由不一致。
        let live_methods = live_method_names();
        assert!(
            live_methods.len() > 100,
            "只从 app-server 问出 {} 个方法,报错格式可能变了 —— 先修这条测试再谈漂移",
            live_methods.len()
        );
        let mut missing = Vec::new();
        for method in CONTROL_METHODS {
            if TRAEX_ONLY_METHODS.contains(method) {
                continue;
            }
            if !live_methods.contains(*method) {
                missing.push((*method).to_string());
            }
        }
        assert!(
            missing.is_empty(),
            "这些方法本机 codex 已经不认了(summon 仍在调用,而 app-server 会拒绝):{missing:?}"
        );
        // 反向:被排除的 traex 专属方法,如果哪天 codex 本体也有了,这条排除就该撤掉。
        for method in TRAEX_ONLY_METHODS {
            assert!(
                !live_methods.contains(*method),
                "`{method}` 现在 codex 本体也支持了,请把它从 TRAEX_ONLY_METHODS 里移出去"
            );
        }

        // 五个热路径的 params:**从产参函数本身取键**,新增字段自动纳入比对。
        let hot: [(&str, serde_json::Value); 5] = [
            (
                "thread/start",
                thread_start_params(
                    Some("/repo"),
                    Some("primer"),
                    &CodexPolicy::default(),
                    Some(&summon_dynamic_tools()),
                ),
            ),
            (
                "thread/resume",
                thread_resume_params(
                    "th-1",
                    Some("/repo"),
                    Some("primer"),
                    &CodexPolicy::default(),
                ),
            ),
            (
                "thread/fork",
                thread_fork_params(
                    "th-1",
                    Some("/repo"),
                    Some("primer"),
                    &CodexPolicy::default(),
                ),
            ),
            (
                "turn/start",
                turn_start_params(
                    "th-1",
                    "hello",
                    &TurnOptions {
                        client_user_message_id: Some("cid"),
                        model: Some("gpt-5.2-codex"),
                        effort: Some("high"),
                        summary: Some("detailed"),
                        service_tier: Some(CodexServiceTier::Fast),
                        approval_policy: Some("never"),
                    },
                ),
            ),
            ("model/list", model_list_params(Some("page-2"))),
        ];
        let mut dropped = Vec::new();
        for (method, params) in hot {
            // 只有 stable 方法才在 dump 里有字段表。三条热路径目前都是 stable;
            // 哪天不是了,这里跳过而不是误报 —— 少守一条好过长期红着被忽略。
            let Some(known) = methods.get(method) else {
                continue;
            };
            for key in params.as_object().expect("params 是对象").keys() {
                if !known.contains(key) {
                    dropped.push(format!("{method}.{key}"));
                }
            }
        }
        assert!(
            !methods.is_empty() && methods.contains_key("thread/start"),
            "热路径方法从 schema 里消失了,字段检查已经形同虚设"
        );
        assert!(
        dropped.is_empty(),
        "这些字段本机 codex 已经不认了。**它不会报错,只会静默丢掉**,所以症状是「配置好像没生效」:{dropped:?}"
    );

        // --- `dynamicTools` 与工具结果的载荷形状 ---
        // 热路径那段只比对 params 的**顶层键**,所以 `dynamicTools` 里面拼错什么都看不出来 ——
        // 而它是这份协议面里唯一的 experimental 字段,最容易漂。app-server 对未知字段静默忽略,
        // 漂了的症状是「工具没注册成功,模型的工具列表里空空如也」,不报错。
        let start_params = read("v2/ThreadStartParams.json");
        let pick = |defs: &Value, title: &str| -> Value {
            defs["oneOf"]
                .as_array()
                .expect("oneOf")
                .iter()
                .find(|variant| variant["title"] == title)
                .unwrap_or_else(|| panic!("schema 里找不到 `{title}` 这个形态"))
                .clone()
        };
        let namespace_spec = pick(
            &start_params["definitions"]["DynamicToolSpec"],
            "NamespaceDynamicToolSpec",
        );
        let function_tool = pick(
            &start_params["definitions"]["DynamicToolNamespaceTool"],
            "FunctionDynamicToolNamespaceTool",
        );

        // 三件事一起比:发出去的键 schema 认不认(不认 = 静默忽略)、schema 要求的键发没发
        // (没发 = 整个请求被拒)、枚举字段的值对不对(值错了同样静默,而且键名比对看不出来)。
        let compare = |what: &str, sent: &Value, schema: &Value, drift: &mut Vec<String>| {
            let properties = schema["properties"].as_object().expect("properties");
            let sent = sent.as_object().expect("发出去的必须是对象");
            for key in sent.keys() {
                if !properties.contains_key(key) {
                    drift.push(format!("{what} 多发了 schema 不认的键 `{key}`"));
                }
            }
            for required in schema["required"].as_array().expect("required") {
                let required = required.as_str().unwrap_or_default();
                if !sent.contains_key(required) {
                    drift.push(format!("{what} 少了 schema 要求的键 `{required}`"));
                }
            }
            for (key, value) in sent {
                let Some(allowed) = properties.get(key).and_then(|p| p["enum"].as_array()) else {
                    continue;
                };
                if !allowed.contains(value) {
                    drift.push(format!("{what}.{key} 的值 {value} 不在 schema 的取值集里"));
                }
            }
        };

        let tools = summon_dynamic_tools();
        let namespace = &tools.as_array().expect("dynamicTools 是数组")[0];
        let mut drift = Vec::new();
        compare("dynamicTools[0]", namespace, &namespace_spec, &mut drift);
        for tool in namespace["tools"].as_array().expect("namespace.tools") {
            let name = tool["name"].as_str().unwrap_or("?");
            compare(&format!("tool `{name}`"), tool, &function_tool, &mut drift);
        }

        // 回给 server 的那一帧同理:`contentItems[].type` 写错不报错,只让工具结果整个丢掉。
        let response = read("DynamicToolCallResponse.json");
        let text_item = pick(
            &response["definitions"]["DynamicToolCallOutputContentItem"],
            "InputTextDynamicToolCallOutputContentItem",
        );
        let frame = dynamic_tool_result_frame(&json!(1), true, "ok");
        compare(
            "DynamicToolCallResponse",
            &frame["result"],
            &response,
            &mut drift,
        );
        compare(
            "contentItems[0]",
            &frame["result"]["contentItems"][0],
            &text_item,
            &mut drift,
        );
        assert!(
            drift.is_empty(),
            "`dynamicTools` / 工具结果的载荷与本机 codex 对不上(症状:工具静默不注册,或结果丢失):{drift:?}"
        );

        // --- 响应侧:`model/list` 的 Model 字段 ---
        // 请求字段拼错会被静默忽略,响应字段改名同样静默 —— `isDefault` 换个名字,summon 只会
        // 悄悄退回配置桶的默认模型,面板上看不出任何异常。所以这几个键单独钉一遍。
        let model_list_response = read("v2/ModelListResponse.json");
        let model_props = model_list_response["definitions"]["Model"]["properties"]
            .as_object()
            .expect("ModelListResponse.definitions.Model.properties");
        let mut renamed = Vec::new();
        for key in ["id", "hidden", "isDefault"] {
            if !model_props.contains_key(key) {
                renamed.push(key.to_string());
            }
        }
        let page_props = model_list_response["properties"]
            .as_object()
            .expect("ModelListResponse.properties");
        for key in ["data", "nextCursor"] {
            if !page_props.contains_key(key) {
                renamed.push(key.to_string());
            }
        }
        assert!(
            renamed.is_empty(),
            "`model/list` 响应少了这些字段(症状:面板候选/默认模型悄悄退回配置桶,无报错):{renamed:?}"
        );

        // --- 通知侧 ---
        let notifications = read("ServerNotification.json");
        let text = serde_json::to_string(&notifications).expect("序列化");
        let mut gone = Vec::new();
        for method in PARSED_NOTIFICATIONS {
            if !text.contains(&format!("\"{method}\"")) {
                gone.push((*method).to_string());
            }
        }
        assert!(
        gone.is_empty(),
        "summon 仍在解析这些通知,但本机 codex 已经不发了(症状:正文少一段 / usage 挂不上,且无任何报错):{gone:?}"
    );
    }

    /// 红线回归:`thread/resume` 的 params 键集合必须 ⊆ `RESUME_ALLOWED_KEYS`。
    /// 多出任何一个键(尤其模型/档位覆盖类)都会让 app-server 静默丢掉该 thread 持久化的
    /// {model, model_provider, reasoning_effort},表现为「重启后模型悄悄换了」且不报错。
    /// 白名单而非黑名单的理由见 `RESUME_ALLOWED_KEYS` 的注释。
    #[test]
    fn thread_resume_params_carry_nothing_beyond_the_allowed_keys() {
        for cwd in [None, Some("/repo")] {
            let params =
                thread_resume_params("th-1", cwd, Some("我是阿卡迪亚"), &CodexPolicy::default());
            let obj = params.as_object().expect("params 应是对象");
            for key in obj.keys() {
                assert!(
                    RESUME_ALLOWED_KEYS.contains(&key.as_str()),
                    "thread/resume 带了白名单外的键 `{key}`(可能让 app-server 丢掉持久化的 model/effort)"
                );
            }
            assert_eq!(obj["threadId"], "th-1");
        }
        assert_eq!(
            thread_resume_params("th-1", Some("/repo"), None, &CodexPolicy::default())["cwd"],
            "/repo"
        );
        // 不给 cwd 就不写这个键 —— 让 codex 自己按 rollout 里记录的 cwd 恢复。
        assert!(
            !thread_resume_params("th-1", None, None, &CodexPolicy::default())
                .as_object()
                .unwrap()
                .contains_key("cwd")
        );
    }

    #[test]
    fn thread_start_registers_dynamic_tools_without_changing_resume_params() {
        let tools = summon_dynamic_tools();
        let start = thread_start_params(
            Some("/repo"),
            Some("primer"),
            &CodexPolicy::default(),
            Some(&tools),
        );
        assert_eq!(start["dynamicTools"], tools);

        let resume = thread_resume_params(
            "th-1",
            Some("/repo"),
            Some("primer"),
            &CodexPolicy::default(),
        );
        assert!(
            resume.get("dynamicTools").is_none(),
            "dynamicTools 由 rollout 恢复，thread/resume 不接受该字段"
        );
    }

    /// PRIMER 不随 rollout 持久化,所以 `primer_slot=developer` 的 thread 每次 resume 都要重发;
    /// 旧 thread(slot=None)则一个字都不能带 —— 它的 PRIMER 还焊在 history 的 baseInstructions
    /// 里,再发一份就是两份。这条判据分错任何一边都不报错,只会静默劣化,故单测直接钉住。
    #[test]
    fn thread_resume_sends_the_primer_only_when_the_thread_was_created_with_the_developer_slot() {
        let with_primer =
            thread_resume_params("th-1", None, Some("我是阿卡迪亚"), &CodexPolicy::default());
        assert_eq!(with_primer["developerInstructions"], "我是阿卡迪亚");

        for legacy in [None, Some(""), Some("   ")] {
            assert!(
                !thread_resume_params("th-1", None, legacy, &CodexPolicy::default())
                    .as_object()
                    .unwrap()
                    .contains_key("developerInstructions"),
                "旧 thread / 空 PRIMER 不得带 developerInstructions(输入={legacy:?})"
            );
        }
    }

    /// **sandbox 必须每次 resume 都发。**
    ///
    /// codex 的 `PersistedResumeSettings` 只存 {approval_policy, approvals_reviewer,
    /// active_permission_profile} —— 唯独**没有 sandbox_mode**。所以只在 thread/start 钉是
    /// 不够的:app-server 一死、懒重连接回来,sandbox 就退回那台机器 `~/.codex/config.toml`
    /// 的值(默认 `read-only`),表现为「冷启时好好的,重连后突然写不了文件」且零报错。
    #[test]
    fn resume_always_carries_the_sandbox_because_codex_does_not_persist_it() {
        let policy = CodexPolicy {
            sandbox: "workspace-write".into(),
            approval_policy: "never".into(),
        };
        let params = thread_resume_params("th-1", None, None, &policy);
        assert_eq!(
            params["sandbox"], "workspace-write",
            "resume 不带 sandbox 的话,重连后会退回机器默认的 read-only"
        );
        assert_eq!(params["approvalPolicy"], "never");
    }

    /// 策略是配置说了算,不是「建这条 thread 那天说了算」。approvalPolicy 虽然会从 rollout
    /// 恢复,但那样改了 config.yaml 之后老 thread 会一直沿用旧值。
    #[test]
    fn resume_and_start_send_the_same_policy_so_config_stays_the_single_source_of_truth() {
        let policy = CodexPolicy {
            sandbox: "read-only".into(),
            approval_policy: "untrusted".into(),
        };
        let start = thread_start_params(None, None, &policy, None);
        let resume = thread_resume_params("th-1", None, None, &policy);
        for key in ["sandbox", "approvalPolicy"] {
            assert_eq!(
                start[key], resume[key],
                "`{key}` 在冷启与重连之间不一致 —— 会话行为会随「这次是接回还是新建」而变"
            );
        }
    }

    /// `turn/start.approvalPolicy` 覆盖本轮**及后续轮**。在这里填一个固定值,
    /// 会让 `thread/start` 按配置下发的策略在第一轮被悄悄盖掉。
    #[test]
    fn turn_start_never_hardcodes_an_approval_policy_over_the_thread_level_one() {
        let inherited = turn_start_params("th-1", "hi", &TurnOptions::default());
        assert!(
            !inherited
                .as_object()
                .unwrap()
                .contains_key("approvalPolicy"),
            "不带这个键才表示「沿用 thread 级」;写死一个值会覆盖用户配的策略"
        );

        let explicit = turn_start_params(
            "th-1",
            "hi",
            &TurnOptions {
                approval_policy: Some("untrusted"),
                ..Default::default()
            },
        );
        assert_eq!(explicit["approvalPolicy"], "untrusted");
    }

    /// 红线的另一半:resume **仍然**不许带任何模型三元组字段。加 developerInstructions 时
    /// 顺手把模型字段一起放进去是最容易犯的错,这条把它钉死。
    #[test]
    fn thread_resume_still_refuses_every_model_shaped_key() {
        let params = thread_resume_params(
            "th-1",
            Some("/repo"),
            Some("primer"),
            &CodexPolicy::default(),
        );
        let obj = params.as_object().unwrap();
        for forbidden in [
            "model",
            "modelProvider",
            "effort",
            "serviceTier",
            "personality",
            "baseInstructions",
        ] {
            assert!(
                !obj.contains_key(forbidden),
                "thread/resume 绝不能带 `{forbidden}`"
            );
        }
    }

    /// 冷启相反:必须显式钉 cwd(app-server 自己的 cwd 可能停在 $HOME)及配置中的批准策略。
    #[test]
    fn thread_start_params_pin_cwd_and_default_approval_policy() {
        let params = thread_start_params(Some("/repo"), None, &CodexPolicy::default(), None);
        assert_eq!(params["cwd"], "/repo");
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["ephemeral"], false);
    }

    /// `approvalPolicy: never` 与 `sandbox` 是同一条策略的两半。只钉前者、后者跟随机器配置,
    /// 在一台没配过 `sandbox_mode` 的机器上就是「read-only + 永不询问」——写操作全失败且无处升级。
    /// codex 的 `SandboxMode` 默认值恰恰就是 read-only,所以省略不是中立选择。
    #[test]
    fn thread_start_pins_the_sandbox_together_with_the_approval_policy() {
        let params = thread_start_params(None, None, &CodexPolicy::default(), None);
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(
            params["sandbox"],
            summon_core::codex_policy::DEFAULT_SANDBOX
        );
        assert_ne!(
            params["sandbox"], "read-only",
            "read-only 配 never 批准 = agent 写不了任何东西又无法升级"
        );
    }

    /// PRIMER 必须落在 `developerInstructions`(追加)而**不是** `baseInstructions`(替换)。
    /// 走错槽位不会报错,只会把 codex 自己那份 ~21KB 系统提示词整个顶掉,症状是工具使用变差
    /// —— 没有任何信号能提示你,所以只能靠这条测试守。
    #[test]
    fn the_primer_goes_to_developer_instructions_never_to_base_instructions() {
        let params = thread_start_params(
            Some("/repo"),
            Some("我是阿卡迪亚"),
            &CodexPolicy::default(),
            None,
        );
        assert_eq!(params["developerInstructions"], "我是阿卡迪亚");
        assert!(
            !params.as_object().unwrap().contains_key("baseInstructions"),
            "baseInstructions 是替换语义,会顶掉 codex 的系统提示词"
        );
    }

    #[test]
    fn an_empty_primer_writes_no_instruction_key_at_all() {
        for empty in [None, Some(""), Some("  \n ")] {
            let params = thread_start_params(None, empty, &CodexPolicy::default(), None);
            let obj = params.as_object().unwrap();
            assert!(!obj.contains_key("developerInstructions"), "输入={empty:?}");
            assert!(!obj.contains_key("baseInstructions"), "输入={empty:?}");
        }
    }

    /// service tier 是三态而非 bool：省略表示不接管线程当前配置，Standard 必须显式发 null
    /// 才能关闭 sticky Fast，Fast 则使用 app-server 0.147 的 canonical id `priority`。
    #[test]
    fn turn_start_params_encode_service_tier_three_states() {
        let inherited = turn_start_params("th-1", "继续", &TurnOptions::default());
        assert!(!inherited.as_object().unwrap().contains_key("serviceTier"));

        let standard = turn_start_params(
            "th-1",
            "继续",
            &TurnOptions {
                service_tier: Some(CodexServiceTier::Standard),
                ..Default::default()
            },
        );
        assert_eq!(standard.get("serviceTier"), Some(&Value::Null));

        let fast = turn_start_params(
            "th-1",
            "继续",
            &TurnOptions {
                service_tier: Some(CodexServiceTier::Fast),
                ..Default::default()
            },
        );
        assert_eq!(fast["serviceTier"], "priority");
    }

    /// 流式通知全谱解析。任何一条错位,打字机/思考卡/工具卡就会静默少一段。
    #[test]
    fn streaming_notifications_parse_into_typed_variants() {
        let parse = |raw: &str| {
            parse_notification(&serde_json::from_str::<Value>(raw).unwrap())
                .unwrap()
                .unwrap()
        };

        assert_eq!(
            parse(
                r#"{"method":"item/agentMessage/delta","params":{"threadId":"t","turnId":"u","itemId":"m","delta":"pong"}}"#
            ),
            CodexAppServerNotification::AgentMessageDelta {
                thread_id: "t".into(),
                turn_id: "u".into(),
                item_id: "m".into(),
                delta: "pong".into(),
            }
        );

        // 两种 reasoning 增量归一到同一个 variant，但保留 summary/detail 语义；itemId/index
        // 缺省不报错，兼容旧 app-server。
        for (method, summary) in [
            ("item/reasoning/textDelta", false),
            ("item/reasoning/summaryTextDelta", true),
        ] {
            assert_eq!(
                parse(&format!(
                    r#"{{"method":"{method}","params":{{"threadId":"t","turnId":"u","delta":"想"}}}}"#
                )),
                CodexAppServerNotification::ReasoningDelta {
                    thread_id: "t".into(),
                    turn_id: "u".into(),
                    item_id: String::new(),
                    summary,
                    part_index: None,
                    delta: "想".into(),
                }
            );
        }

        // 真机形态(codex 0.146.0):summaryTextDelta 自带 itemId + summaryIndex,段边界就在这里。
        assert_eq!(
            parse(
                r#"{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"t","turnId":"u","itemId":"rs_1","delta":"想","summaryIndex":2}}"#
            ),
            CodexAppServerNotification::ReasoningDelta {
                thread_id: "t".into(),
                turn_id: "u".into(),
                item_id: "rs_1".into(),
                summary: true,
                part_index: Some(2),
                delta: "想".into(),
            }
        );

        let started = parse(
            r#"{"method":"item/started","params":{"threadId":"t","turnId":"u","item":{"type":"commandExecution","command":"ls"}}}"#,
        );
        let CodexAppServerNotification::ItemStarted { item, .. } = &started else {
            panic!("应解析成 ItemStarted:{started:?}");
        };
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["command"], "ls", "item 原样透传,不在这层裁字段");

        let completed = parse(
            r#"{"method":"item/completed","params":{"threadId":"t","turnId":"u","item":{"type":"agentMessage","text":"pong"}}}"#,
        );
        let CodexAppServerNotification::ItemCompleted { item, .. } = &completed else {
            panic!("应解析成 ItemCompleted:{completed:?}");
        };
        assert_eq!(item["text"], "pong");

        // Task 4 起多解一份 `last`(这一轮的用量拆分),引擎据它 join 出终态 `Result.usage`。
        // 只有 `total` 的旧形状仍要解得出来 —— 字段随版本增删,缺了不许拆流。
        assert_eq!(
            parse(
                r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"t","turnId":"u","tokenUsage":{"total":{"totalTokens":20762,"inputTokens":20757,"cachedInputTokens":11008}}}}"#
            ),
            CodexAppServerNotification::TokenUsage {
                thread_id: "t".into(),
                turn_id: Some("u".into()),
                total_tokens: Some(20762),
                cached_input_tokens: Some(11008),
                model_context_window: None,
                last: None,
            }
        );
        assert_eq!(
            parse(
                r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"t","turnId":"u","tokenUsage":{"total":{"totalTokens":20762},"last":{"inputTokens":11,"cachedInputTokens":2,"outputTokens":3,"totalTokens":14}}}}"#
            ),
            CodexAppServerNotification::TokenUsage {
                thread_id: "t".into(),
                turn_id: Some("u".into()),
                total_tokens: Some(20762),
                cached_input_tokens: None,
                model_context_window: None,
                last: Some(summon_core::codex_rpc::RpcTokenBreakdown {
                    input_tokens: 11,
                    cached_input_tokens: 2,
                    cache_write_input_tokens: 0,
                    output_tokens: 3,
                    total_tokens: 14,
                }),
            }
        );

        let turn_started = parse(
            r#"{"method":"turn/started","params":{"threadId":"t","turn":{"id":"u","status":"inProgress"}}}"#,
        );
        let CodexAppServerNotification::TurnStarted { turn, .. } = &turn_started else {
            panic!("应解析成 TurnStarted:{turn_started:?}");
        };
        assert_eq!(turn.id, "u", "turnId 是 steer/interrupt 的必填参数,不能丢");
    }

    /// 协议仍在演进:没见过的通知必须落 `Other` 而不是报错拆掉整条流。
    #[test]
    fn unknown_notification_degrades_to_other_instead_of_erroring() {
        let raw = r#"{"method":"thread/realtime/sdp","params":{"threadId":"t"}}"#;
        let parsed = parse_notification(&serde_json::from_str::<Value>(raw).unwrap())
            .expect("未知通知不该是错误")
            .expect("未知通知仍应产出一个事件");
        assert!(matches!(
            parsed,
            CodexAppServerNotification::Other { ref method, .. } if method == "thread/realtime/sdp"
        ));
    }

    /// 缺必填字段要显式报错,不许静默当成空串往下走。
    #[test]
    fn malformed_delta_notification_is_reported_not_silently_empty() {
        let raw = r#"{"method":"item/agentMessage/delta","params":{"threadId":"t"}}"#;
        let err = parse_notification(&serde_json::from_str::<Value>(raw).unwrap())
            .expect_err("缺 turnId/delta 应报错");
        assert!(
            format!("{err:?}").contains("turnId"),
            "错误应点名缺失字段:{err:?}"
        );
    }

    #[test]
    fn thread_read_parses_turn_error() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/read'*) printf '%s\n' '{"id":2,"result":{"thread":{"id":"t1","cwd":"/repo","status":{"type":"idle"},"turns":[{"id":"turn-1","status":"failed","items":[{"type":"userMessage","id":"item-1","clientId":"attempt-1","content":[]}],"error":{"message":"limited","codexErrorInfo":"usageLimitExceeded"}}]}}}' ;;
"#;
        let (_tmp, config, _log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let thread = client.thread_read("t1", true, None).unwrap();
        assert!(thread.turns[0]
            .error
            .as_ref()
            .is_some_and(CodexAppServerTurnError::is_usage_limit_exceeded));
        assert!(thread.turns[0].has_client_user_message_id("attempt-1"));
        assert!(!thread.turns[0].has_client_user_message_id("other"));
    }

    #[test]
    fn thread_list_reads_user_facing_name_from_state_db() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/list'*) printf '%s\n' '{"id":2,"result":{"data":[{"id":"t1","name":"真正标题","preview":"第一条 prompt","cwd":"/repo","status":{"type":"notLoaded"},"turns":[]}],"nextCursor":"page-2"}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let page = client
            .thread_list(false, 200, None, Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(page.data[0].name.as_deref(), Some("真正标题"));
        assert_eq!(page.data[0].preview.as_deref(), Some("第一条 prompt"));
        assert_eq!(page.next_cursor.as_deref(), Some("page-2"));

        let requests = std::fs::read_to_string(log).unwrap();
        let request = requests
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value.get("method").and_then(Value::as_str) == Some("thread/list"))
            .unwrap();
        assert_eq!(request["params"]["archived"], false);
        assert_eq!(request["params"]["useStateDbOnly"], true);
        assert_eq!(request["params"]["sortKey"], "updated_at");
    }

    #[test]
    fn model_list_keeps_visible_models_in_catalog_order_and_reports_upstream_default() {
        let _guard = process_test_guard();
        let cases = r#"
    *'model/list'*) printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"gpt-6-astra","displayName":"GPT-6-Astra","hidden":false,"isDefault":true},{"id":"codex-auto-review","displayName":"Auto Review","hidden":true,"isDefault":false},{"id":"gpt-5.5","displayName":"GPT-5.5","hidden":false,"isDefault":false}],"nextCursor":null}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let catalog = client.model_list(Some(Duration::from_secs(2))).unwrap();

        // hidden 的不进候选:它在 codex 自己的 picker 里也不列,列出来只会被点了起不来。
        assert_eq!(catalog.models, ["gpt-6-astra", "gpt-5.5"]);
        assert_eq!(catalog.default_model.as_deref(), Some("gpt-6-astra"));

        let requests = std::fs::read_to_string(log).unwrap();
        let request = requests
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value.get("method").and_then(Value::as_str) == Some("model/list"))
            .unwrap();
        assert_eq!(request["params"]["includeHidden"], false);
    }

    #[test]
    fn model_list_follows_the_cursor_until_the_catalog_is_exhausted() {
        let _guard = process_test_guard();
        // 第一个匹配的 case 生效,所以带 cursor 的分支必须排在前面。
        let cases = r#"
    *'"cursor":"page-2"'*) printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"gpt-5.5","hidden":false,"isDefault":false}],"nextCursor":null}}' ;;
    *'model/list'*) printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"gpt-6-astra","hidden":false,"isDefault":true}],"nextCursor":"page-2"}}' ;;
"#;
        let (_tmp, config, _log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let catalog = client.model_list(Some(Duration::from_secs(2))).unwrap();

        assert_eq!(catalog.models, ["gpt-6-astra", "gpt-5.5"]);
        assert_eq!(catalog.default_model.as_deref(), Some("gpt-6-astra"));
    }

    #[test]
    fn model_list_tolerates_an_empty_catalog() {
        let _guard = process_test_guard();
        let cases = r#"
    *'model/list'*) printf '%s\n' '{"id":'"$id"',"result":{"data":[],"nextCursor":null}}' ;;
"#;
        let (_tmp, config, _log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let catalog = client.model_list(Some(Duration::from_secs(2))).unwrap();
        assert!(catalog.models.is_empty());
        assert_eq!(catalog.default_model, None);
    }

    #[test]
    fn single_bucket_without_limit_id_is_the_codex_default() {
        let parsed = parse_account_rate_limits(&json!({
            "rateLimits": {
                "primary": {"usedPercent": 100.0, "resetsAt": 200},
                "secondary": null,
                "rateLimitReachedType": "rate_limit_reached"
            },
            "rateLimitsByLimitId": null
        }))
        .unwrap();
        assert_eq!(
            parsed
                .codex()
                .and_then(|snapshot| snapshot.primary.as_ref())
                .and_then(|window| window.resets_at),
            Some(200)
        );
    }

    #[test]
    fn missing_method_is_explicit_capability_error() {
        let _guard = process_test_guard();
        let cases = r#"
    *'account/rateLimits/read'*) printf '%s\n' '{"id":2,"error":{"code":-32601,"message":"method not found"}}' ;;
"#;
        let (_tmp, config, _log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        assert!(matches!(
            client.account_rate_limits_read(None),
            Err(CodexAppServerError::CapabilityUnavailable { method, .. })
                if method == "account/rateLimits/read"
        ));
    }

    #[test]
    fn request_timeout_is_bounded() {
        let _guard = process_test_guard();
        // server 收到请求但**故意不回**（`:` 是 shell 的 no-op）；客户端必须按 deadline 返回。
        //
        // 这条 case 必须显式写出来:夹具对 `account/rateLimits/read` 有一条兜底应答
        // （见 `write_app_server_with_trap`——开会话会顺带问一次额度，不答就人人白等超时），
        // 传空 cases 的话这里会被那条兜底答掉，超时机制就没被测到了。
        let (_tmp, config, _log) = fake_server("*'account/rateLimits/read'*) : ;;\n");
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let started = Instant::now();
        assert!(matches!(
            client.account_rate_limits_read(Some(Duration::from_millis(30))),
            Err(CodexAppServerError::Timeout { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// 轮内 steer 的**帧形状**:`turn/steer` 三个必填参数一个都不能少或错位。
    /// `expectedTurnId` 尤其关键 —— 它是 server 侧「这段输入是不是发给当前这一轮」的唯一判据,
    /// 服务端据此拒绝发往过期轮的输入。
    #[test]
    fn steer_frames_carry_thread_and_expected_turn_id() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' ;;
    *'turn/steer'*) printf '%s\n' '{"id":'"$id"',"result":{"turnId":"turn-1"}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("thread-9", "原始问题", &TurnOptions::default(), None)
            .unwrap();
        let handle = client.interrupt_handle();
        assert_eq!(handle.steer("补充一句"), RpcSteerOutcome::Accepted);
        // 帧是从**另一条通路**(send_bounded)写出去的,不经 request_value,故只能从 server 端日志核对。
        let deadline = Instant::now() + Duration::from_secs(5);
        let steer = loop {
            let found = std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|v| v.get("method").and_then(Value::as_str) == Some("turn/steer"));
            if let Some(steer) = found {
                break steer;
            }
            assert!(Instant::now() < deadline, "应发出 turn/steer 帧");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(steer["params"]["threadId"], "thread-9");
        assert_eq!(steer["params"]["expectedTurnId"], "turn-1");
        assert_eq!(steer["params"]["input"][0]["type"], "text");
        assert_eq!(steer["params"]["input"][0]["text"], "补充一句");
    }

    #[test]
    fn steer_server_rejection_is_not_delivery() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' ;;
    *'turn/steer'*) printf '%s\n' '{"id":'"$id"',"error":{"code":-32600,"message":"turn mismatch"}}' ;;
"#;
        let (_tmp, config, _log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("t1", "question", &TurnOptions::default(), None)
            .unwrap();
        assert_eq!(
            client.interrupt_handle().steer("followup"),
            RpcSteerOutcome::Rejected
        );
    }

    #[test]
    fn steer_uncertain_responses_are_not_retryable_and_release_waiters() {
        let _guard = process_test_guard();
        for response in [
            ":",
            "exit 0",
            r#"printf '%s\n' '{"id":'"$id"',"result":{"turnId":"another-turn"}}'"#,
            r#"printf '%s\n' '{"id":'"$id"',"result":{}}'"#,
            r#"printf '%s\n' '{"id":'"$id"',"result":{"turnId":"turn-1"},"error":{"code":-1,"message":"ambiguous"}}'"#,
        ] {
            let cases = format!(
                r#"
    *'turn/start'*) printf '%s\n' '{{"id":'"$id"',"result":{{"turn":{{"id":"turn-1","status":"inProgress"}}}}}}' ;;
    *'turn/steer'*) {response} ;;
"#
            );
            let (_tmp, config, _) = fake_server(&cases);
            let mut client = CodexAppServerClient::spawn(config).unwrap();
            client
                .turn_start("t1", "question", &TurnOptions::default(), None)
                .unwrap();
            let handle = client.interrupt_handle();
            let started = Instant::now();
            assert_eq!(
                handle.steer_within("followup", Duration::from_millis(100)),
                RpcSteerOutcome::Unconfirmed,
                "{response}"
            );
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(handle.steer_replies.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn steer_reader_routes_only_matching_responses_and_preserves_notifications() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' ;;
    *'turn/steer'*) printf '%s\n' '{"id":'"$id"',"method":"thread/unknownRequest","params":{}}';
        printf '%s\n' '{"id":99999,"result":{"turnId":"wrong"}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"turn-1","status":"completed"}}}';
        printf '%s\n' '{"id":'"$id"',"result":{"turnId":"turn-1"}}' ;;
"#;
        let (_tmp, config, _) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("t1", "question", &TurnOptions::default(), None)
            .unwrap();
        let handle = client.interrupt_handle();
        // 主 client 在整个确认期间都没有消费 reader 队列。
        assert_eq!(handle.steer("followup"), RpcSteerOutcome::Accepted);
        assert!(client
            .wait_for_turn_completed("t1", "turn-1", Duration::from_secs(2))
            .is_ok());
        assert!(handle.steer_replies.lock().unwrap().is_empty());
    }

    #[test]
    fn steer_late_reply_cannot_confirm_the_next_request() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' ;;
    *'turn/steer'*) if [ -z "$old_id" ]; then old_id=$id; else
        printf '%s\n' '{"id":'"$old_id"',"result":{"turnId":"turn-1"}}'; fi ;;
"#;
        let (_tmp, config, _) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("t1", "question", &TurnOptions::default(), None)
            .unwrap();
        let handle = client.interrupt_handle();
        for _ in 0..2 {
            assert_eq!(
                handle.steer_within("followup", Duration::from_millis(100)),
                RpcSteerOutcome::Unconfirmed
            );
            assert!(handle.steer_replies.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn steer_responses_can_arrive_out_of_order() {
        let replies = SteerReplies::default();
        let (tx1, rx1) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        replies.lock().unwrap().extend([(7, tx1), (8, tx2)]);
        assert!(!route_steer_reply(
            &replies,
            &json!({"id":7,"method":"server/request"})
        ));
        assert!(!route_steer_reply(&replies, &json!({"id":9,"result":{}})));
        assert!(route_steer_reply(
            &replies,
            &json!({"id":8,"result":{"turnId":"b"}})
        ));
        assert!(route_steer_reply(
            &replies,
            &json!({"id":7,"result":{"turnId":"a"}})
        ));
        assert_eq!(rx1.recv().unwrap()["result"]["turnId"], "a");
        assert_eq!(rx2.recv().unwrap()["result"]["turnId"], "b");
        assert!(replies.lock().unwrap().is_empty());
    }

    #[test]
    #[ignore = "需要本机已登录 codex，会真实执行一轮"]
    fn real_codex_app_server_confirms_steer() {
        let dir = tempfile::tempdir().unwrap();
        let config = CodexAppServerConfig {
            args: vec!["app-server".into(), "--listen".into(), "stdio://".into()],
            env: std::env::vars().collect(),
            ..Default::default()
        };
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let thread = client
            .thread_start(
                Some(dir.path().to_str().unwrap()),
                None,
                &CodexPolicy::default(),
                None,
            )
            .unwrap();
        let turn = client
            .turn_start(
                &thread.id,
                "Explain how a binary search works. Do not use tools.",
                &TurnOptions::default(),
                None,
            )
            .unwrap();
        assert_eq!(
            client
                .interrupt_handle()
                .steer("Include the exact word orchid in your answer."),
            RpcSteerOutcome::Accepted
        );
        client
            .wait_for_turn_completed(&thread.id, &turn.id, Duration::from_secs(120))
            .unwrap();
    }

    /// 没有在跑的轮时**一个字节都不许发**,并且必须回 `Idle` 而不是 `Accepted`。
    ///
    /// 这条是「不吞消息」的第一道闸:调用方拿 `Accepted` 当"已交付"来销 WAL,这里若乐观地发一帧
    /// 就返回 `Accepted`,那条用户消息就会被销账 —— 而根本没有任何一轮收到过它。
    #[test]
    fn steer_without_an_active_turn_sends_nothing_and_reports_idle() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/read'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"t1","status":{"type":"idle"},"turns":[]}}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let handle = client.interrupt_handle();
        assert_eq!(handle.steer("没人在跑"), RpcSteerOutcome::Idle);
        // 写端只有一根、server 按序落日志:随后这条请求得到应答时,之前写出的帧必已在日志里。
        client.thread_read("t1", false, None).unwrap();
        let requests = std::fs::read_to_string(log).unwrap_or_default();
        assert!(
            !requests.contains("turn/steer"),
            "无活跃轮时不许发 steer 帧:{requests}"
        );
    }

    /// 轮进终态后 steer 必须转 `Idle`。竞态是常态(消息刚好在收尾那一刻到),
    /// 判据只能是 `active_turn` 而不是「进程还活着」。
    #[test]
    fn steer_after_the_turn_completed_reports_idle() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}'; printf '%s\n' '{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;
        let (_tmp, config, _log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client
            .turn_start("t1", "问题", &TurnOptions::default(), None)
            .unwrap();
        let handle = client.interrupt_handle();
        client
            .wait_for_turn_completed("t1", "turn-1", Duration::from_secs(5))
            .unwrap();
        assert_eq!(handle.steer("晚了一步"), RpcSteerOutcome::Idle);
    }

    #[test]
    fn unknown_server_request_is_denied_fail_closed() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/read'*) printf '%s\n' '{"id":"unknown-1","method":"thread/somethingBrandNew","params":{}}'; printf '%s\n' '{"id":2,"result":{"thread":{"id":"t1","status":{"type":"idle"},"turns":[]}}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        client.thread_read("t1", false, None).unwrap();
        // request_value 在返回 id=2 前已经处理并拒绝服务端 request;回帧要等 server 读走才落日志。
        let response = wait_for_logged_response(&log, "unknown-1");
        assert!(response.get("error").is_some(), "{response}");
    }

    /// 应答表:ServerRequest.json(codex-cli 0.146.0)里的 10 个 stable 方法 + experimental
    /// `currentTime/read` + 未知方法,一条都不许没有
    /// 回帧。形状以同批生成的 `*Response.json` 为准 —— 形状错了 app-server 反序列化失败,
    /// 效果和不答一样(整轮挂死)。
    #[test]
    fn every_server_request_method_gets_a_schema_shaped_reply() {
        let id = json!("req-1");
        let reply = |method: &str, params: Value| server_request_reply(&id, method, &params).frame;

        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
        ] {
            let frame = reply(method, json!({}));
            assert_eq!(frame["id"], id, "{method} 必须回同一个 id");
            assert_eq!(frame["result"]["decision"], "decline", "{method}");
        }

        for method in ["execCommandApproval", "applyPatchApproval"] {
            let frame = reply(method, json!({}));
            assert_eq!(frame["id"], id, "{method} 必须回同一个 id");
            // 旧 API 的 denied 是对象且 rejection 必填,裸字符串 "denied" 会被拒收。
            assert!(
                frame["result"]["decision"]["denied"]["rejection"].is_string(),
                "{method} 应答形状不合 ReviewDecision:{frame}"
            );
        }

        let permissions = reply("item/permissions/requestApproval", json!({}));
        assert!(
            permissions["result"]["permissions"]
                .as_object()
                .is_some_and(serde_json::Map::is_empty),
            "schema 无拒绝变体,空 profile 才是「一项都不给」:{permissions}"
        );
        assert_eq!(permissions["result"]["scope"], "turn");

        let user_input = reply(
            "item/tool/requestUserInput",
            full_user_input_request(json!("ignored"), "t1", "turn-1")["params"].clone(),
        );
        assert_eq!(
            user_input["result"]["answers"]["q-choice"]["answers"],
            json!([])
        );
        assert_eq!(
            user_input["result"]["answers"]["q-note"]["answers"],
            json!([])
        );

        assert_eq!(
            reply("mcpServer/elicitation/request", json!({}))["result"]["action"],
            "decline"
        );

        let tool_call = reply("item/tool/call", json!({}));
        assert_eq!(tool_call["result"]["success"], false);
        assert_eq!(
            tool_call["result"]["contentItems"][0]["type"], "inputText",
            "结构化失败而不是协议错误,模型才能看见原因继续:{tool_call}"
        );

        let current_time =
            server_request_reply_at(&id, "currentTime/read", &json!({"threadId": "t1"}), 42).frame;
        assert_eq!(current_time["result"]["currentTimeAt"], 42);

        // 没有凭据可给的两个:回 JSON-RPC error,绝不编假 token。
        for method in ["account/chatgptAuthTokens/refresh", "attestation/generate"] {
            let frame = reply(method, json!({}));
            assert_eq!(frame["id"], id, "{method} 必须回同一个 id");
            assert_eq!(
                frame["error"]["code"], SERVER_REQUEST_DENIED_CODE,
                "{method} 应回结构化拒绝"
            );
        }

        // 协议在演进:没见过的方法也必须应答,否则 app-server 永远等那条 id。
        let unknown = reply("thread/somethingBrandNew", json!({}));
        assert_eq!(unknown["id"], id);
        assert_eq!(unknown["error"]["code"], SERVER_REQUEST_UNKNOWN_CODE);
    }

    /// 全谱不挂死:假 server 在一轮里把 11 个 ServerRequest 全发一遍,客户端必须逐条回帧,
    /// 且这一轮仍然走到 turn/completed。漏答任何一条,真 app-server 都会永久等下去。
    #[test]
    fn all_server_requests_are_answered_and_the_turn_still_completes() {
        let _guard = process_test_guard();
        let cases = r#"
    *'turn/start'*) printf '%s\n' '{"id":"sr-1","method":"item/commandExecution/requestApproval","params":{}}' '{"id":"sr-2","method":"item/fileChange/requestApproval","params":{}}' '{"id":"sr-3","method":"item/tool/requestUserInput","params":{"threadId":"t1","turnId":"turn-1","itemId":"item-1","questions":[{"id":"q1","header":"h","question":"?","isOther":false,"isSecret":false,"options":[{"label":"A","description":"option A"}]}],"isBlocking":true,"autoResolutionMs":null}}' '{"id":"sr-4","method":"mcpServer/elicitation/request","params":{}}' '{"id":"sr-5","method":"item/permissions/requestApproval","params":{}}' '{"id":"sr-6","method":"item/tool/call","params":{"callId":"c1","tool":"x"}}' '{"id":"sr-7","method":"account/chatgptAuthTokens/refresh","params":{}}' '{"id":"sr-8","method":"attestation/generate","params":{}}' '{"id":"sr-9","method":"applyPatchApproval","params":{}}' '{"id":"sr-10","method":"execCommandApproval","params":{}}' '{"id":"sr-11","method":"currentTime/read","params":{"threadId":"t1"}}' '{"id":2,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}' '{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;
        let (_tmp, config, log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let turn = client
            .turn_start("t1", "继续", &TurnOptions::default(), None)
            .unwrap();
        let completed = client
            .wait_for_turn_completed("t1", &turn.id, Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            completed.status, "completed",
            "应答完 10 条后这一轮仍要收敛"
        );

        // 回帧写进假 server 的 stdin,由它落到同一份 log;等它读完再断言。
        let answered = |lines: &[Value], n: usize| {
            let id = Value::String(format!("sr-{n}"));
            lines.iter().any(|value| {
                value.get("id") == Some(&id)
                    && (value.get("result").is_some() || value.get("error").is_some())
            })
        };
        let read_log = || -> Vec<Value> {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut lines = read_log();
        while !(1..=11).all(|n| answered(&lines, n)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
            lines = read_log();
        }
        for n in 1..=11 {
            assert!(
                answered(&lines, n),
                "sr-{n} 没有回帧 —— 真 app-server 会永久挂在这条 id 上"
            );
        }
    }

    /// stderr 不能丢:进程异常退出时错误里必须带**最后**写入的几行(不是最早的),且尾部有界。
    /// 189f052 刚在 Subprocess 路补过这个洞,新路径不许重新丢掉。
    #[test]
    fn abnormal_exit_reports_bounded_stderr_tail() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/read'*) printf 'discard-me-' >&2; yes x | head -c 20000 >&2; printf 'tail-marker\n' >&2; exit 4 ;;
"#;
        let (_tmp, config, _log) = fake_server(cases);
        let mut client = CodexAppServerClient::spawn(config).unwrap();
        let error = client
            .thread_read("t1", false, None)
            .expect_err("server 异常退出应报错");
        let message = error.to_string();
        assert!(
            message.contains("tail-marker"),
            "必须保留 stderr 末尾,实得:{message}"
        );
        assert!(
            !message.contains("discard-me"),
            "超出 16KB 上限的开头必须被丢掉,实得开头:{}",
            &message[..message.len().min(200)]
        );
        assert!(
            message.len() <= STDERR_TAIL_LIMIT_BYTES + 512,
            "尾部必须有界,实得 {} 字节",
            message.len()
        );
    }

    /// 真机端到端:冷启 thread → 跑一轮 → 收流式增量 → 断掉 client → **新进程 resume 同一个
    /// thread** → 问上一轮埋的暗号。答得出来才证明「daemon 重启不丢上下文」这条前提成立。
    /// 会消耗 codex 额度,故 ignore;发布前批量验收时跑。
    #[test]
    #[ignore = "需要本机已登录的 codex，会真跑两轮消耗额度"]
    fn real_codex_app_server_streams_a_turn_and_resumes_context() {
        let work_dir = tempfile::tempdir().unwrap();
        let cwd = work_dir.path().to_string_lossy().to_string();
        let long = Some(Duration::from_secs(120));

        let thread_id = {
            let mut client = CodexAppServerClient::spawn(CodexAppServerConfig::default()).unwrap();
            let thread = client
                .thread_start(Some(&cwd), None, &CodexPolicy::default(), long)
                .unwrap();
            let turn = client
                .turn_start(
                    &thread.id,
                    "记住暗号 zanzibar。只回复这个词本身,不要用工具。",
                    &TurnOptions::default(),
                    long,
                )
                .unwrap();

            let mut deltas = String::new();
            let mut saw_turn_started = false;
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                assert!(Instant::now() < deadline, "等 turn/completed 超时");
                match client.recv_notification(Some(Duration::from_secs(5))) {
                    Ok(CodexAppServerNotification::TurnStarted { .. }) => saw_turn_started = true,
                    Ok(CodexAppServerNotification::AgentMessageDelta { delta, .. }) => {
                        deltas.push_str(&delta)
                    }
                    Ok(CodexAppServerNotification::TurnCompleted { turn: done, .. })
                        if done.id == turn.id =>
                    {
                        break
                    }
                    Ok(_) => {}
                    Err(CodexAppServerError::Timeout { .. }) => {}
                    Err(e) => panic!("通知流出错:{e:?}"),
                }
            }
            assert!(saw_turn_started, "应收到 turn/started");
            assert!(
                deltas.to_lowercase().contains("zanzibar"),
                "流式增量应拼出回复正文,实得:{deltas:?}"
            );
            thread.id
        }; // client 在此 Drop —— 进程组被 SIGINT 收掉,模拟 daemon 重启

        let mut fresh = CodexAppServerClient::spawn(CodexAppServerConfig::default()).unwrap();
        fresh
            .thread_resume(&thread_id, Some(&cwd), None, &CodexPolicy::default(), long)
            .unwrap();
        let turn = fresh
            .turn_start(
                &thread_id,
                "我刚才让你记的暗号是什么?只回那个词,不要用工具。",
                &TurnOptions::default(),
                long,
            )
            .unwrap();
        let mut answer = String::new();
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            assert!(
                Instant::now() < deadline,
                "等 resume 后的 turn/completed 超时"
            );
            match fresh.recv_notification(Some(Duration::from_secs(5))) {
                Ok(CodexAppServerNotification::AgentMessageDelta { delta, .. }) => {
                    answer.push_str(&delta)
                }
                Ok(CodexAppServerNotification::TurnCompleted { turn: done, .. })
                    if done.id == turn.id =>
                {
                    break
                }
                Ok(_) => {}
                Err(CodexAppServerError::Timeout { .. }) => {}
                Err(e) => panic!("resume 后通知流出错:{e:?}"),
            }
        }
        assert!(
            answer.to_lowercase().contains("zanzibar"),
            "新进程 resume 后应答得出上一进程埋的暗号,实得:{answer:?}"
        );
    }

    #[test]
    #[ignore = "需要本机已登录的 codex，仅读 account/rateLimits/read"]
    fn real_codex_app_server_reads_rate_limits() {
        let mut client = CodexAppServerClient::spawn(CodexAppServerConfig::default()).unwrap();
        let limits = client
            .account_rate_limits_read(Some(Duration::from_secs(15)))
            .unwrap();
        assert!(limits.codex().is_some());
    }

    /// 目录的形状只有真机能证:夹具里的 hidden/isDefault 是测试自己造的,证明不了 backend 真这么回。
    #[test]
    #[ignore = "需要本机已登录的 codex，仅读 model/list，不起会话、不消耗 token"]
    fn real_codex_app_server_reports_a_usable_model_catalog() {
        let mut client = CodexAppServerClient::spawn(CodexAppServerConfig::default()).unwrap();
        let catalog = client.model_list(Some(Duration::from_secs(15))).unwrap();
        assert!(
            !catalog.models.is_empty(),
            "目录为空的话面板会一路退回配置桶,等于这条链没接上"
        );
        let default_model = catalog
            .default_model
            .expect("backend 必须标出一个 isDefault,否则 onboard 的默认值拿不到上游那一层");
        assert!(
            catalog.models.contains(&default_model),
            "标成默认的模型必须也在候选里,否则面板上点不到它"
        );
    }

    #[test]
    #[ignore = "需要本机已登录的 traex；创建并删除一个空 thread，验证原生控制方法与 experimental capability"]
    fn real_traex_app_server_accepts_the_slash_facing_control_methods() {
        let cwd = std::env::current_dir().unwrap();
        let mut client = CodexAppServerClient::spawn(CodexAppServerConfig {
            program: summon_core::tools::resolve(summon_core::tools::Tool::Traex)
                .to_string_lossy()
                .into_owned(),
            args: vec!["app-server".into(), "--listen".into(), "stdio://".into()],
            work_dir: Some(cwd.clone()),
            env: crate::runtime::daemon::essential_system_env(),
            experimental_api: true,
            request_timeout: Duration::from_secs(30),
            ..CodexAppServerConfig::default()
        })
        .unwrap();
        let thread = client
            .thread_start(
                cwd.to_str(),
                None,
                &CodexPolicy::default(),
                Some(Duration::from_secs(30)),
            )
            .unwrap();
        let thread_id = thread.id.clone();
        let verified = (|| -> Result<(), CodexAppServerError> {
            client.thread_read(&thread_id, false, None)?;
            client.control_request(
                "thread/name/set",
                json!({"threadId": thread_id, "name": "summon-rpc-control-probe"}),
                None,
            )?;
            client.control_request(
                "thread/goal/set",
                json!({"threadId": thread_id, "objective": "probe", "status": "active"}),
                None,
            )?;
            let goal =
                client.control_request("thread/goal/get", json!({"threadId": thread_id}), None)?;
            assert_eq!(goal["goal"]["objective"], "probe");
            client.control_request("thread/contextUsage", json!({"threadId": thread_id}), None)?;
            client.control_request(
                "mcpServerStatus/list",
                json!({"detail": "toolsAndAuthOnly", "limit": 1}),
                None,
            )?;
            client.control_request(
                "thread/backgroundTerminals/list",
                json!({"threadId": thread_id}),
                None,
            )?;
            client.control_request("thread/compact/start", json!({"threadId": thread_id}), None)?;
            Ok(())
        })();
        // 只删除本测试刚刚创建并拿到 exact id 的空 thread；即使中间方法失败也尽力清理。
        let cleanup = client.control_request(
            "thread/delete",
            json!({"threadId": thread_id}),
            Some(Duration::from_secs(30)),
        );
        verified.unwrap();
        cleanup.unwrap();
    }

    #[test]
    #[ignore = "需要本机已登录的 codex；创建并删除一个空 thread，验证 experimental 控制方法"]
    fn real_codex_app_server_accepts_experimental_control_methods() {
        let cwd = std::env::current_dir().unwrap();
        let mut client = CodexAppServerClient::spawn(CodexAppServerConfig {
            program: summon_core::tools::resolve(summon_core::tools::Tool::Codex)
                .to_string_lossy()
                .into_owned(),
            args: vec!["app-server".into(), "--listen".into(), "stdio://".into()],
            work_dir: Some(cwd.clone()),
            env: crate::runtime::daemon::essential_system_env(),
            experimental_api: true,
            request_timeout: Duration::from_secs(30),
            ..CodexAppServerConfig::default()
        })
        .unwrap();
        let thread = client
            .thread_start(
                cwd.to_str(),
                None,
                &CodexPolicy::default(),
                Some(Duration::from_secs(30)),
            )
            .unwrap();
        let thread_id = thread.id.clone();
        let verified = (|| -> Result<(), CodexAppServerError> {
            let terminals = client.control_request(
                "thread/backgroundTerminals/list",
                json!({"threadId": thread_id, "limit": 10}),
                None,
            )?;
            assert!(terminals["data"].is_array());
            let features = client.control_request(
                "experimentalFeature/list",
                json!({"threadId": thread_id, "limit": 10}),
                None,
            )?;
            assert!(features["data"].is_array());
            Ok(())
        })();
        let cleanup = client.control_request(
            "thread/delete",
            json!({"threadId": thread_id}),
            Some(Duration::from_secs(30)),
        );
        verified.unwrap();
        cleanup.unwrap();
    }
}
