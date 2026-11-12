//! cursor ACP 客户端(`cursor-agent acp`,Agent Client Protocol v1 over stdio)。
//!
//! 与 `codex::codex_app_server` 是同一形态的两个协议适配器:同步、单 outstanding
//! request、单 reader 线程、写端共享给打断把手、进程自成组。传输层原语两边共用
//! (`runtime::rpc_stdio`),这里只写 ACP 自己的协议面。
//!
//! **与 codex app-server 的三处结构性差异**(每一处都改变了引擎的形状,不是风格差异):
//! 1. **一轮的终态不在通知流里,而是 `session/prompt` 的响应**(`{"stopReason":"end_turn"}`)。
//!    所以这里没有 `turn/started` 那种「先拿 turnId 再等终态通知」的两段式,一轮 = 发一个请求
//!    然后一边收通知一边等那条响应(见 [`CursorAcpClient::recv_turn_event`])。
//! 2. **`session/load` 会把整段历史当通知重放**(实测:user_message_chunk / tool_call /
//!    tool_call_update / agent_message_chunk,toolCallId 前缀 `replay-`)。不丢掉这批重放,
//!    懒重连后的第一轮会把整个历史当成本轮回复重发一遍 —— 见 [`CursorAcpClient::drain_replay`]。
//! 3. **PRIMER 没有原生位置**。ACP 的 `session/new` 只收 `{cwd, mcpServers}`,没有
//!    `baseInstructions` 同类字段(实测 initialize 的 agentCapabilities 里也没有相关能力位)。
//!    所以 cursor 仍按现状把 PRIMER 前置进首条用户消息(`compose_first_prompt`),不发明新形态。
//!
//! **模型与参数统一走 `session/set_config_option`**，真机测量的结论:
//! 1. spawn 时的 `cursor-agent --model <id> acp` 对 acp 子命令**无效** —— 参数被接受、进程照起,
//!    但会话的 `currentModelId` 纹丝不动。靠它换模型是「不报错、不生效」的死路。
//! 2. 握手声明 `_meta.parameterizedModelPicker=true` 后，`availableModels` / `currentModelId` 切到
//!    base id 命名空间(`gpt-5.6-sol`)；context/reasoning/fast 不再编码进 bracket id，而是动态
//!    `configOptions`，写入走 [`CursorAcpClient::set_config_option`]。
//!
//! 所以模型也是 `configId="model"` 的动态轴，必须与 context/reasoning/fast 共用同一条 setter
//! 和 exact 回执校验；保留另一套 `session/set_model` 路径只会让命名空间与成功判据再次分叉。

use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use summon_core::cursor_acp::{parse_notification, CursorAcpNotification, CursorAcpUpdate};

use crate::runtime::executor::ChildKiller;
use crate::runtime::rpc_stdio::{
    is_server_request, signal_group, ReaderEvent, RpcInterruptOutcome, RpcStdioError, RpcWriter,
    StderrTail, INTERRUPT_WRITE_WAIT, SOFT_INTERRUPT_GRACE, STDERR_EOF_GRACE,
};

/// ACP 协议版本。cursor-agent 2026.07.23 实测 initialize 回 `protocolVersion: 1`。
const ACP_PROTOCOL_VERSION: u64 = 1;

/// 握手时报给 server 的客户端名。
const CLIENT_NAME: &str = "summon";

/// stdout 是 Cursor 子进程输入给 daemon 的不可信 JSONL。`BufRead::lines` 会在看到换行前
/// 无上限扩容；server/代理一旦吐出异常大单帧，就能先于任何 catalog/card 预算把 daemon OOM。
/// 4 MiB 足以容纳正常 ACP 内容帧，同时把最坏单次分配钉死在可诊断范围内。
const MAX_JSONL_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// `configOptions` 会跨池缓存并被卡片层 clone，预算必须在 wire 入口收口，不能等渲染时再裁。
const MAX_CONFIG_AXES: usize = 64;
const MAX_CONFIG_CHOICES_PER_AXIS: usize = 128;
const MAX_CONFIG_CHOICES_TOTAL: usize = 512;
const MAX_CONFIG_STRINGS_TOTAL: usize = 2048;
const MAX_CONFIG_TOKEN_BYTES: usize = 512;
const MAX_CONFIG_TEXT_BYTES: usize = 4 * 1024;
const MAX_CONFIG_TOKEN_BYTES_TOTAL: usize = 64 * 1024;
const MAX_CONFIG_TEXT_BYTES_TOTAL: usize = 128 * 1024;

#[derive(Debug, Clone)]
pub struct CursorAcpConfig {
    pub program: String,
    pub args: Vec<String>,
    pub work_dir: Option<PathBuf>,
    /// Cursor 的可写 CLI/ACP 配置根。参数 setter 有跨 session 副作用，生产必须让每个 ACP
    /// 进程独占一份；per-agent 目标 bundle 由池层在每轮前重放，不能拿物理根当事实源。
    pub config_dir: PathBuf,
    pub request_timeout: Duration,
    /// 子进程的**完整**环境。spawn 侧**无条件** `env_clear`,所以这里给什么子进程就有什么 ——
    /// 给空就是空环境(连 PATH 都没有),不是"继承父进程"。
    ///
    /// 生产必须显式给全:ACP 子进程原先直接继承 daemon 的环境,而 daemon 自己可能带着上一个
    /// 上下文的 `SUMMON_AGENT`/`SUMMON_CHAT_ID` —— 于是子进程里跑的 CLI 执行 `summon preview`
    /// 时认领了**别的 agent 的身份**,把内容发进别的群(2026-08-14 现场:rpc-procs 记着
    /// PID 属于 agent A,`ps eww` 里却是 agent B 的 `SUMMON_AGENT`)。
    pub env: Vec<(String, String)>,
}

impl CursorAcpConfig {
    /// 拼出 `cursor-agent --yolo acp` 的完整参数。
    ///
    /// `--yolo` 与无头路的 `headless_args` 一字不差:托管会话背后没有人能答批准框,不放开就是
    /// 每个工具调用都卡在 `session/request_permission` 上(客户端侧仍有兜底应答,见
    /// [`server_request_reply`],两道闸都要有)。
    ///
    /// **刻意不带 `--model`**:真机实测它对 acp 子命令无效(参数收下、`currentModelId` 不变),
    /// 模型与参数统一走 [`CursorAcpClient::set_config_option`]。见模块头。
    pub fn acp_args() -> Vec<String> {
        // 子命令必须排在全局 flag 之后(commander.js 的解析顺序)。
        vec!["--yolo".to_string(), "acp".to_string()]
    }
}

#[derive(Debug)]
pub enum CursorAcpError {
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
    /// 进程已经死了。「断开」两个字什么都不解释,真正的原因(未登录、参数不认)只写在 stderr,
    /// 所以这里必须把尾部一起带出去。
    ProcessFailed {
        operation: String,
        detail: String,
        stderr_tail: String,
    },
    Remote {
        method: String,
        code: Option<i64>,
        message: String,
    },
}

impl std::fmt::Display for CursorAcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "{}", summon_i18n::format!("r645f4fa52d")),
            Self::Io(error) => write!(f, "{}", summon_i18n::format!("r26c1bb7468")),
            Self::Timeout { operation } => write!(f, "{}", summon_i18n::format!("rabfb5317a7")),
            Self::Disconnected { operation } => {
                write!(f, "{}", summon_i18n::format!("r1c4258e023"))
            }
            Self::InvalidJson(detail) => write!(f, "{}", summon_i18n::format!("r2cdcacc177")),
            Self::InvalidResponse { operation, detail } => {
                write!(f, "{}", summon_i18n::format!("r2eadde3d47"))
            }
            Self::ProcessFailed {
                operation,
                detail,
                stderr_tail,
            } => write!(f, "{}", summon_i18n::format!("r0a55157832")),
            Self::Remote {
                method,
                code,
                message,
            } => write!(
                f,
                "{}",
                summon_i18n::format!(
                    "r626d3bfa66",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                )
            ),
        }
    }
}

impl std::error::Error for CursorAcpError {}

impl From<RpcStdioError> for CursorAcpError {
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

/// ACP 会话模式。目录来自 server，客户端不维护静态白名单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorAcpMode {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// 一个动态配置项里的可选值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorAcpConfigValue {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
    /// 标准 ACP 允许把 options 按组返回；扁平消费时仍保留组 id / 展示名。
    pub group: Option<String>,
    pub group_name: Option<String>,
}

/// ACP 标准 `configOptions` 条目。
///
/// `id` / `category` / `option_type` 都由 server 决定。这里不枚举 fast/context 等 Cursor
/// 扩展，避免 server 新增轴时客户端静默丢能力；写入也只接受这份动态目录里的原始 id/value。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorAcpConfigOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub option_type: Option<String>,
    pub current_value: Option<String>,
    pub options: Vec<CursorAcpConfigValue>,
}

/// `session/new` / `session/load` 的产出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorAcpSession {
    pub session_id: String,
    /// parameterized handshake 下 server 报的 base model id，不含 context/reasoning/fast。
    pub current_model_id: Option<String>,
    /// 这条会话能选的 base model id 列表(**ACP parameterized 命名空间**),`/model` 的候选目录
    /// 以它为准。
    pub available_models: Vec<String>,
    /// server 报的当前会话模式。旧版 / 精简响应没有该字段时为 `None`。
    pub current_mode_id: Option<String>,
    /// server 动态提供的模式目录。坏条目逐项跳过，不拖垮整个会话。
    pub available_modes: Vec<CursorAcpMode>,
    /// server 动态提供的标准配置目录；写入走 [`CursorAcpClient::set_config_option`]。
    pub config_options: Vec<CursorAcpConfigOption>,
    /// `true` 仅表示 `session/new|load` **显式**返回了 `configOptions`，且整份目录通过严格、
    /// 有界解析。字段缺失或协议合法的 `null` 时目录为空且为 false；数组存在但坏形状/超预算
    /// 会让建会话 fail-stop，不会产出一个看似可信的半目录。
    pub config_options_trusted: bool,
}

/// 一轮消费循环里能看到的两类事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpTurnEvent {
    /// 一条内容/控制面更新。
    Update(CursorAcpUpdate),
    /// 一轮结束(`session/prompt` 的响应)。`stop_reason` 形如 `end_turn` / `cancelled`。
    Ended { stop_reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorContextUsage {
    pub used_tokens: u64,
    pub window_tokens: u64,
}

/// 「此刻在跑的一轮」的跨线程标记。`session/cancel` 的 params 只要 sessionId,但要判断
/// 「这一轮结没结束」得认那条 prompt 请求的 id。
///
/// 置位在 `prompt_start` 写出帧之后;**清位在 reader 线程**(消费者侧再兜一道,盖住
/// 「响应先到、置位后发生」的窄竞态)。清位刻意不只放在消费者那侧:那样「轮是否已终态」就取决于
/// 消费者的快慢,消费者一慢,打断就会在轮其实已经结束之后误升级成 SIGINT —— 那是把整个会话进程杀掉。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptRef {
    session_id: String,
    request_id: u64,
}

type ActivePrompt = Arc<Mutex<Option<PromptRef>>>;

fn clear_active_prompt_matching(slot: &ActivePrompt, request_id: u64) {
    let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.as_ref().is_some_and(|p| p.request_id == request_id) {
        *guard = None;
    }
}

/// reader 线程侧的终态识别:prompt 请求的**响应**(result 或 error)就是这一轮的终点。
fn note_prompt_terminal(slot: &ActivePrompt, value: &Value) {
    if value.get("result").is_none() && value.get("error").is_none() {
        return;
    }
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        clear_active_prompt_matching(slot, id);
    }
}

/// 跨线程打断把手:`Clone + Send`。
///
/// 两级:先经共享写端发 `session/cancel`(**notification**,不是 request —— 以 request 形式发
/// 返回 -32601),宽限内没进终态才 SIGINT **整个进程组**。后者会把会话进程一起带走,代价是下一条
/// 消息要重连(`session/load`,实测 ~10s),所以永远是第二选择。
///
/// §5.8 实测结论(cursor-agent 2026.07.23,见设计文档):notification 形式**生效** ——
/// 发出后 4ms 内 `session/prompt` 就返回 `{"stopReason":"cancelled"}`,之后零通知。
/// 所以 cursor 的软打断走 `session/cancel`,不降级成 SIGINT。
///
/// **只 SIGINT,不用 SIGTERM**(红线):与 codex 路同一条约定,硬终止会让下一轮 load 拿到
/// 少了一段的上下文。
#[derive(Clone)]
pub struct AcpInterrupt {
    writer: Arc<RpcWriter>,
    active_prompt: ActivePrompt,
    killer: ChildKiller,
}

impl AcpInterrupt {
    pub fn interrupt(&self) -> RpcInterruptOutcome {
        self.interrupt_within(SOFT_INTERRUPT_GRACE)
    }

    pub fn interrupt_within(&self, grace: Duration) -> RpcInterruptOutcome {
        let Some(prompt) = self.active_prompt() else {
            return RpcInterruptOutcome::Idle;
        };
        // notification:**不带 id**。带了就是 request 形式,server 回 -32601 且不打断。
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": prompt.session_id},
        });
        if let Err(error) = self.writer.send_bounded(&frame, INTERRUPT_WRITE_WAIT) {
            crate::observe::log::log(&summon_i18n::format!(
                "r8266ffb769",
                CursorAcpError::from(error)
            ));
        }
        let deadline = Instant::now() + grace;
        loop {
            match self.active_prompt() {
                Some(current) if current.request_id == prompt.request_id => {}
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

    /// 换一个硬打断句柄(同 pgid、**新的一次性闸**)。理由与 codex 侧
    /// [`crate::codex::codex_app_server::RpcInterrupt::rebind_killer`] 逐字相同:
    /// 会话进程跨轮存活,共用 spawn 那把一次性闸会让第二轮起的 SIGINT 全部变成空操作。
    pub fn rebind_killer(&self, killer: ChildKiller) -> Self {
        Self {
            writer: Arc::clone(&self.writer),
            active_prompt: Arc::clone(&self.active_prompt),
            killer,
        }
    }

    /// 此刻在跑的那一轮的 prompt 请求 id。
    ///
    /// `#[cfg(test)]`:生产侧判「打断有没有意义」看 [`Self::interrupt`] 的返回值就够了
    /// (`Idle` 就是「没有在跑的轮」),只有测试需要同步到「轮真的起来了」这个时点。
    #[cfg(test)]
    pub fn active_prompt_id(&self) -> Option<u64> {
        self.active_prompt().map(|p| p.request_id)
    }

    fn active_prompt(&self) -> Option<PromptRef> {
        self.active_prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

pub struct CursorAcpClient {
    child: Child,
    writer: Arc<RpcWriter>,
    reader: Option<thread::JoinHandle<()>>,
    stderr: Arc<StderrTail>,
    rx: Receiver<ReaderEvent>,
    /// request 等待期间到达的通知的排队区(与 codex 客户端同款,不丢事件)。
    queued_notifications: std::collections::VecDeque<Value>,
    context_usage: Option<CursorContextUsage>,
    active_prompt: ActivePrompt,
    /// 「stdout 还没 EOF」= 进程还在。reader 线程读到 EOF 即清位。
    alive: Arc<AtomicBool>,
    killer: ChildKiller,
    request_timeout: Duration,
    pgid: i32,
    terminated: bool,
}

/// 读取一条有界 JSONL 帧。返回的 bytes 不含 `\n`，CRLF 的 `\r` 也一并去掉；EOF 前最后一条
/// 无换行帧仍按 `BufRead::lines` 的既有语义接收。
fn read_bounded_jsonl_frame<R: BufRead>(
    reader: &mut R,
    frame: &mut Vec<u8>,
) -> io::Result<Option<()>> {
    frame.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(()));
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(available.len());
        let next_len = frame.len().checked_add(take).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                summon_i18n::text!("ra576a49b31"),
            )
        })?;
        if next_len > MAX_JSONL_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                summon_i18n::format!("r672634aac9", MAX_JSONL_FRAME_BYTES),
            ));
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(newline.is_some()));
        if newline.is_some() {
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(()));
        }
    }
}

fn invalid_json_detail(error: &serde_json::Error, frame_bytes: usize) -> String {
    // 帧里可能含 prompt、工具回包或凭据；诊断只保留 parser 位置和长度，绝不回显正文。
    format!("{error}(frame_bytes={frame_bytes})")
}

impl CursorAcpClient {
    pub fn spawn(config: CursorAcpConfig) -> Result<Self, CursorAcpError> {
        if config.config_dir.as_os_str().is_empty() || !config.config_dir.is_absolute() {
            return Err(CursorAcpError::InvalidResponse {
                operation: "spawn".into(),
                detail: "config_dir must be a non-empty absolute per-process path".into(),
            });
        }
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
        // Cursor 的 `session/set_config_option` 会写 CLI 配置并影响后续 session；显式钉在该
        // ACP 进程独占的根，既不跨 chat 串档，也绝不修改 owner 的 ~/.cursor。
        command.env("CURSOR_CONFIG_DIR", &config.config_dir);
        // 自成进程组:daemon 停机可一次命中,绝不遗留孤儿。**只 killpg 自己 spawn 的组,不碰 tmux。**
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // pipe + 并发 drain 留尾:null 会把启动失败的唯一线索(未登录/参数不认)丢掉,
            // pipe 了不读则会反压死锁 —— 只有「pipe 且有人读」能同时避开两个坑。
            .stderr(Stdio::piped())
            .spawn()
            .map_err(CursorAcpError::Spawn)?;
        let pgid = i32::try_from(child.id()).map_err(|_| CursorAcpError::InvalidResponse {
            operation: "spawn".into(),
            detail: "child pid exceeds i32".into(),
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CursorAcpError::InvalidResponse {
                operation: "spawn".into(),
                detail: "child stdin unavailable".into(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CursorAcpError::InvalidResponse {
                operation: "spawn".into(),
                detail: "child stdout unavailable".into(),
            })?;
        let stderr = StderrTail::spawn(child.stderr.take());
        let (tx, rx) = mpsc::channel();
        let active_prompt: ActivePrompt = Arc::new(Mutex::new(None));
        let alive = Arc::new(AtomicBool::new(true));
        let active_reader = Arc::clone(&active_prompt);
        let alive_reader = Arc::clone(&alive);
        let reader = thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            let mut frame = Vec::with_capacity(8 * 1024);
            loop {
                match read_bounded_jsonl_frame(&mut stdout, &mut frame) {
                    Ok(Some(())) => {}
                    Ok(None) => break,
                    Err(error) => {
                        let _ = tx.send(ReaderEvent::InvalidJson(error.to_string()));
                        break;
                    }
                }
                if frame.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice::<Value>(&frame) {
                    Ok(value) => {
                        // 终态在这里认,不等消费者:见 [`PromptRef`]。
                        note_prompt_terminal(&active_reader, &value);
                        if tx.send(ReaderEvent::Message(value)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        if tx
                            // 不把原始帧拼进错误：它可能含 prompt/凭据，也可能接近 4 MiB。
                            .send(ReaderEvent::InvalidJson(invalid_json_detail(
                                &error,
                                frame.len(),
                            )))
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
            *active_reader.lock().unwrap_or_else(PoisonError::into_inner) = None;
        });
        let mut client = Self {
            child,
            writer: Arc::new(RpcWriter::new(stdin)),
            reader: Some(reader),
            stderr,
            rx,
            queued_notifications: std::collections::VecDeque::new(),
            context_usage: None,
            active_prompt,
            alive,
            killer: ChildKiller::new(pgid),
            request_timeout: config.request_timeout,
            pgid,
            terminated: false,
        };
        // 握手失败最常见的形态就是「进程立刻死了」(cursor 未登录/参数不认),原因只在 stderr。
        if let Err(error) = client.handshake() {
            return Err(client.exit_error("initialize", error));
        }
        Ok(client)
    }

    /// ACP 握手。summon 不做 fs/terminal 代理，二者仍明确为 false；Cursor 私有 `_meta`
    /// 只启用参数化模型目录，Fast/context/reasoning 才能作为动态 configOptions 出现。
    fn handshake(&mut self) -> Result<(), CursorAcpError> {
        self.request_value(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                    "_meta": {"parameterizedModelPicker": true},
                },
                "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
            }),
            self.request_timeout,
        )?;
        Ok(())
    }

    pub fn interrupt_handle(&self) -> AcpInterrupt {
        AcpInterrupt {
            writer: Arc::clone(&self.writer),
            active_prompt: Arc::clone(&self.active_prompt),
            killer: self.killer.clone(),
        }
    }

    pub fn killer(&self) -> ChildKiller {
        self.killer.clone()
    }

    pub fn liveness(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alive)
    }

    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// 当前保留的 stderr 尾部(最多 16KB,已去 ANSI)。空则 `None`。
    pub fn stderr_tail(&self) -> Option<String> {
        let text = self.stderr.snapshot();
        (!text.is_empty()).then_some(text)
    }

    /// 冷启一个会话。`cwd` 是 ACP 的必填项,钉死 agent 的工作目录
    /// (server 自己的 cwd 可能停在 `$HOME`)。
    pub fn session_new(
        &mut self,
        cwd: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<CursorAcpSession, CursorAcpError> {
        let method = "session/new";
        let result = self.request_value(
            method,
            json!({"cwd": resolve_cwd(cwd), "mcpServers": []}),
            self.timeout(timeout),
        )?;
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| CursorAcpError::InvalidResponse {
                operation: method.into(),
                detail: "missing result.sessionId".into(),
            })?
            .to_string();
        parse_session(&session_id, &result, method)
    }

    /// 接回一个已存在的会话(懒重连)。
    ///
    /// **必须丢掉历史重放**:实测 `session/load` 在响应之前会把整段历史当 `session/update`
    /// 通知发一遍。不丢的话,懒重连后第一轮会把历史当成本轮回复整个重发。
    pub fn session_load(
        &mut self,
        session_id: &str,
        cwd: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<CursorAcpSession, CursorAcpError> {
        let method = "session/load";
        let result = self.request_value(
            method,
            json!({"sessionId": session_id, "cwd": resolve_cwd(cwd), "mcpServers": []}),
            self.timeout(timeout),
        )?;
        let dropped = self.drain_queued_notifications();
        if dropped > 0 {
            crate::observe::log::log(&summon_i18n::format!("rf4482679d9"));
        }
        // Cursor 2026.07/08 的 load 响应不回 sessionId；请求值就是 authoritative id。兼容
        // 仍回该字段的版本，但只要出现就必须是字符串且 exact match，防止接错上下文。
        match result.get("sessionId") {
            None => {}
            Some(Value::String(returned_session_id)) if returned_session_id == session_id => {}
            Some(Value::String(returned_session_id)) => {
                return Err(CursorAcpError::InvalidResponse {
                    operation: method.into(),
                    detail: format!(
                        "sessionId mismatch(expected={session_id}, actual={returned_session_id})"
                    ),
                });
            }
            Some(_) => {
                return Err(CursorAcpError::InvalidResponse {
                    operation: method.into(),
                    detail: "result.sessionId must be a string when present".into(),
                });
            }
        }
        parse_session(session_id, &result, method)
    }

    /// 修改 Cursor 动态模型参数，并返回 server 回执里的完整新目录。
    ///
    /// 这不是 session-local setter：Cursor 会把值写入 `CURSOR_CONFIG_DIR` 并让后续 session
    /// 继承。因此隔离边界在 [`CursorAcpConfig::config_dir`]，`session_id` 只是协议要求的作用对象。
    /// server 还会对不兼容组合静默回退，所以 JSON-RPC success 不足以证明成功；必须在回执目录
    /// 中找到 requested configId，且 `currentValue` 与请求值 exact match。
    pub fn set_config_option(
        &mut self,
        session_id: &str,
        config_id: &str,
        value: &str,
        timeout: Option<Duration>,
    ) -> Result<Vec<CursorAcpConfigOption>, CursorAcpError> {
        let method = "session/set_config_option";
        let result = self.request_value(
            method,
            json!({"sessionId": session_id, "configId": config_id, "value": value}),
            self.timeout(timeout),
        )?;
        let config_options = parse_config_options_strict(&result, method)?;
        let returned = config_options
            .iter()
            .find(|option| option.id == config_id)
            .ok_or_else(|| CursorAcpError::InvalidResponse {
                operation: method.into(),
                detail: format!("returned configOptions missing requested configId `{config_id}`"),
            })?;
        if returned.current_value.as_deref() != Some(value) {
            return Err(CursorAcpError::InvalidResponse {
                operation: method.into(),
                detail: format!(
                    "requested `{config_id}` currentValue mismatch(expected={value}, actual={})",
                    returned.current_value.as_deref().unwrap_or("<missing>")
                ),
            });
        }
        Ok(config_options)
    }

    /// 切换 ACP 会话模式。模式目录来自 `session/*` 响应，合法性仍由 server 判定。
    ///
    /// 只走标准 `session/set_mode`；刻意不复用 `session/set_config_option`，后者会写 ACP 进程
    /// 的物理配置根并影响后续 session，语义不是当前会话的 mode 切换。
    pub fn set_mode(
        &mut self,
        session_id: &str,
        mode_id: &str,
        timeout: Option<Duration>,
    ) -> Result<(), CursorAcpError> {
        self.request_value(
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": mode_id}),
            self.timeout(timeout),
        )?;
        Ok(())
    }

    /// 丢掉此刻还排在队里的通知,返回丢了几条。两处调用点,同一条理由「队里的东西一定不属于
    /// 接下来那一轮」:
    ///
    /// - `session/load` 之后:重放全部发生在 load 响应之前(server 单线程按序写),响应一到
    ///   就说明重放已经收完。**load 失败时同样要清** —— 失败退冷启不会让已经排队的重放消失。
    /// - 每一轮 `session/prompt` 之前:client 跨轮存活,上一轮的尾巴留在队里就会被当成本轮内容。
    ///
    /// 只清**已经排到队里**的;之后到达的通知属于新的一轮,绝不能一起清掉。
    pub fn drain_queued_notifications(&mut self) -> usize {
        let queued = std::mem::take(&mut self.queued_notifications);
        let dropped = queued.len();
        for value in queued {
            if let Some(CursorAcpNotification::SessionUpdate {
                update:
                    CursorAcpUpdate::Usage {
                        used_tokens,
                        window_tokens,
                    },
                ..
            }) = parse_notification(&value)
            {
                self.context_usage = Some(CursorContextUsage {
                    used_tokens,
                    window_tokens,
                });
            }
        }
        dropped
    }

    pub fn context_usage(&self) -> Option<CursorContextUsage> {
        self.context_usage
    }

    pub fn clear_context_usage(&mut self) {
        self.context_usage = None;
    }

    /// 投递一轮。**不等终态**:响应就是终态,由 [`Self::recv_turn_event`] 在流式消费里等到。
    pub fn prompt_start(&mut self, session_id: &str, prompt: &str) -> Result<(), CursorAcpError> {
        let id = self.writer.next_id();
        self.send_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": prompt}],
            },
        }))?;
        // 置位「在跑的轮」,打断把手才有 sessionId 可填、才知道该不该发 cancel。
        *self
            .active_prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(PromptRef {
            session_id: session_id.to_string(),
            request_id: id,
        });
        Ok(())
    }

    /// 取下一条轮内事件:一条更新,或这一轮的终态。
    ///
    /// server→client 的**请求**在这里就地应答(绝不吞 id,吞了就是整轮永久挂死),不上抛。
    pub fn recv_turn_event(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<AcpTurnEvent, CursorAcpError> {
        let operation = "session/update";
        let deadline = Instant::now() + self.timeout(timeout);
        loop {
            if let Some(value) = self.queued_notifications.pop_front() {
                if let Some(event) = self.classify(&value)? {
                    return Ok(event);
                }
                continue;
            }
            let value = self.recv_value(deadline, operation)?;
            if is_server_request(&value) {
                self.respond_to_server_request(&value)?;
                continue;
            }
            if let Some(event) = self.classify(&value)? {
                return Ok(event);
            }
        }
    }

    /// 一帧 → 轮内事件。`None` = 这一帧与本轮无关(别的响应 / 非 session/update 通知)。
    fn classify(&mut self, value: &Value) -> Result<Option<AcpTurnEvent>, CursorAcpError> {
        // 响应帧:只有本轮那条 prompt 请求的响应才是终态。
        if value.get("result").is_some() || value.get("error").is_some() {
            let id = value.get("id").and_then(Value::as_u64);
            let is_ours = self
                .active_prompt
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(|p| Some(p.request_id) == id)
                // 已被 reader 清位(终态先到),仍按「这就是终态」处理 —— 这条兜底与 codex 侧
                // `note_terminal` 同一个窄竞态。
                .unwrap_or(true);
            if !is_ours {
                return Ok(None);
            }
            if let Some(id) = id {
                clear_active_prompt_matching(&self.active_prompt, id);
            }
            if let Some(error) = value.get("error") {
                return Err(remote_error("session/prompt", error));
            }
            let stop_reason = value
                .pointer("/result/stopReason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return Ok(Some(AcpTurnEvent::Ended { stop_reason }));
        }
        match parse_notification(value) {
            Some(CursorAcpNotification::SessionUpdate { update, .. }) => {
                if let CursorAcpUpdate::Usage {
                    used_tokens,
                    window_tokens,
                } = &update
                {
                    self.context_usage = Some(CursorContextUsage {
                        used_tokens: *used_tokens,
                        window_tokens: *window_tokens,
                    });
                }
                Ok(Some(AcpTurnEvent::Update(update)))
            }
            // 非 session/update 的通知:协议演进的容忍位,忽略而不是拆流。
            Some(CursorAcpNotification::Other { .. }) | None => Ok(None),
        }
    }

    /// SIGINT 整个自有进程组,宽限期后仍存活则 SIGKILL。幂等,可由 daemon stop 与 Drop 共用。
    pub fn interrupt(&mut self, grace: Duration) -> Result<(), CursorAcpError> {
        if self.terminated {
            return Ok(());
        }
        let mut leader_exited = match self.child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => return Err(CursorAcpError::Io(error)),
        };
        self.terminated = true;
        self.alive.store(false, Ordering::Release);
        *self
            .active_prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.writer.close();
        if !leader_exited {
            if let Err(error) = signal_group(self.pgid, nix::sys::signal::Signal::SIGINT) {
                // 同 uid 正常不会失败;兜底仍回收直系 child,不能因报告 signal 错误而遗留进程。
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(CursorAcpError::Io(error));
            }
        }
        let deadline = Instant::now() + grace;
        loop {
            if !leader_exited {
                match self.child.try_wait() {
                    Ok(Some(_)) => leader_exited = true,
                    Ok(None) => {}
                    Err(error) => return Err(CursorAcpError::Io(error)),
                }
            }
            if !crate::runtime::inflight::group_alive(self.pgid) {
                if !leader_exited {
                    self.child.wait().map_err(CursorAcpError::Io)?;
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        signal_group(self.pgid, nix::sys::signal::Signal::SIGKILL).map_err(CursorAcpError::Io)?;
        if !leader_exited {
            self.child.wait().map_err(CursorAcpError::Io)?;
        }
        Ok(())
    }

    fn timeout(&self, override_timeout: Option<Duration>) -> Duration {
        override_timeout.unwrap_or(self.request_timeout)
    }

    fn request_value(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CursorAcpError> {
        let id = self.writer.next_id();
        self.send_message(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
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
                return Err(CursorAcpError::InvalidResponse {
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

    fn recv_value(&mut self, deadline: Instant, operation: &str) -> Result<Value, CursorAcpError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CursorAcpError::Timeout {
                operation: operation.into(),
            });
        }
        match self.rx.recv_timeout(remaining) {
            Ok(ReaderEvent::Message(value)) => Ok(value),
            Ok(ReaderEvent::InvalidJson(detail)) => Err(CursorAcpError::InvalidJson(detail)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(CursorAcpError::Timeout {
                operation: operation.into(),
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let cause = CursorAcpError::Disconnected {
                    operation: operation.into(),
                };
                Err(self.exit_error(operation, cause))
            }
        }
    }

    /// 把 stderr 尾部并进错误 —— **只在进程确实已经死了时**。活着的 server 的 stderr 是噪音,
    /// 并进来只会淹掉真正的错因(同 `capture/subprocess.rs`「非零退出才上报」的取舍)。
    fn exit_error(&mut self, operation: &str, cause: CursorAcpError) -> CursorAcpError {
        if !self.wait_for_child_exit(STDERR_EOF_GRACE) {
            return cause;
        }
        self.stderr.wait_for_eof(STDERR_EOF_GRACE);
        match self.stderr_tail() {
            Some(stderr_tail) => CursorAcpError::ProcessFailed {
                operation: operation.into(),
                detail: cause.to_string(),
                stderr_tail,
            },
            None => cause,
        }
    }

    fn wait_for_child_exit(&mut self, timeout: Duration) -> bool {
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

    fn respond_to_server_request(&self, request: &Value) -> Result<(), CursorAcpError> {
        let Some(id) = request.get("id") else {
            return Ok(());
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let reply = server_request_reply(id, method, &params);
        crate::observe::log::log(&format!(
            "cursor ACP server request: method={method} id={id} → {}",
            reply.reason
        ));
        self.send_message(&reply.frame)
    }

    fn send_message(&self, value: &Value) -> Result<(), CursorAcpError> {
        Ok(self.writer.send(value)?)
    }
}

impl Drop for CursorAcpClient {
    fn drop(&mut self) {
        // 仅中断自己 spawn 的进程组,绝不碰 tmux。
        let _ = self.interrupt(Duration::from_millis(100));
        if let Some(reader) = self.reader.take() {
            crate::runtime::rpc_stdio::join_reader_bounded(reader, "cursor ACP");
        }
        // stderr drain 线程不 join:进程组已被信号收掉,它会在 EOF 自行结束;逃逸的孙进程若
        // 还持着 fd,join 会把 Drop 永久卡住。
    }
}

/// ACP 的 `cwd` 是必填且必须是绝对路径。调用方没给就退到进程 cwd —— 这只发生在测试/异常路径,
/// 生产的 chat 路永远经 `start_work_dir` 校验过再传进来。
fn resolve_cwd(cwd: Option<&str>) -> String {
    match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        Some(cwd) => cwd.to_string(),
        None => std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_string()),
    }
}

/// 把一次 `session/new|load` 响应收敛成稳定的客户端视图。
///
/// `modes` 仍是非安全关键的展示目录，坏条目可逐项跳过；`configOptions` 则会驱动模型、上下文和
/// 计费配置，不能 fail-soft。非 null 数组一旦出现，整份目录必须严格且有界，否则 session 建立
/// 直接失败；协议用缺失/null 表达“不提供目录”时则保持 untrusted。
fn parse_session(
    session_id: &str,
    result: &Value,
    operation: &str,
) -> Result<CursorAcpSession, CursorAcpError> {
    let (config_options, config_options_trusted) = match result.get("configOptions") {
        None | Some(Value::Null) => (Vec::new(), false),
        Some(_) => (parse_config_options_strict(result, operation)?, true),
    };
    let mode_option = config_options.iter().find(|option| option.id == "mode");
    let current_mode_id = nonempty_string(result.pointer("/modes/currentModeId")).or_else(|| {
        mode_option
            .and_then(|option| option.current_value.as_deref())
            .filter(|mode| !mode.is_empty())
            .map(str::to_string)
    });
    let mut available_modes = parse_available_modes(result);
    // 新版 ACP 以 configOptions 作为统一控制面。若 legacy `modes` 块缺失，仍可从 mode 选项
    // 恢复同一份目录；只有 `modes` 真为空时才回退，避免合并两份目录制造重复项。
    if available_modes.is_empty() {
        if let Some(mode) = mode_option {
            available_modes = mode
                .options
                .iter()
                .map(|choice| CursorAcpMode {
                    id: choice.value.clone(),
                    name: choice.name.clone(),
                    description: choice.description.clone(),
                })
                .collect();
        }
    }
    Ok(CursorAcpSession {
        session_id: session_id.to_string(),
        current_model_id: current_model_id(result),
        available_models: available_model_ids(result),
        current_mode_id,
        available_modes,
        config_options,
        config_options_trusted,
    })
}

fn parse_available_modes(result: &Value) -> Vec<CursorAcpMode> {
    result
        .pointer("/modes/availableModes")
        .and_then(Value::as_array)
        .map(|modes| modes.iter().filter_map(parse_mode).collect())
        .unwrap_or_default()
}

fn parse_mode(value: &Value) -> Option<CursorAcpMode> {
    let id = nonempty_string(value.get("id"))?;
    Some(CursorAcpMode {
        name: nonempty_string(value.get("name")).unwrap_or_else(|| id.clone()),
        description: nonempty_string(value.get("description")),
        id,
    })
}

/// 写操作的回执不能沿用 session 目录的 fail-soft parser：setter 的成功证明要求完整目录无损，
/// 否则 trim、坏 sibling、重复 id 或超大目录都可能把 server 的非 exact 回执伪装成成功，或在
/// 卡片预算介入前耗尽 daemon 内存。session/new|load 只要显式携带非 null 数组，也复用同一解析器。
/// Cursor 当前内嵌 ACP schema 的 select axis 必须带 id/name/type/currentValue/options，options
/// 又必须是同构的 flat[] 或 group[]；这里按协议形状验全，不为缺字段发明本地默认值。
fn parse_config_options_strict(
    result: &Value,
    operation: &str,
) -> Result<Vec<CursorAcpConfigOption>, CursorAcpError> {
    let options = result
        .get("configOptions")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_config_receipt(operation, "missing array result.configOptions"))?;
    if options.len() > MAX_CONFIG_AXES {
        return Err(invalid_config_receipt(
            operation,
            format!(
                "result.configOptions axis count {} exceeds limit {MAX_CONFIG_AXES}",
                options.len()
            ),
        ));
    }
    let mut budget = ConfigCatalogBudget::default();
    let mut ids = std::collections::HashSet::with_capacity(options.len());
    let mut parsed = Vec::with_capacity(options.len());
    for (index, value) in options.iter().enumerate() {
        let path = format!("configOptions[{index}]");
        let object = value.as_object().ok_or_else(|| {
            invalid_config_receipt(operation, format!("{path} must be an object"))
        })?;
        let id = strict_catalog_string(
            object.get("id"),
            operation,
            &format!("{path}.id"),
            CatalogStringKind::Token,
            false,
            &mut budget,
        )?
        .to_string();
        if !ids.insert(id.clone()) {
            return Err(invalid_config_receipt(
                operation,
                format!("duplicate configOptions id at {path}.id"),
            ));
        }
        let name = strict_catalog_string(
            object.get("name"),
            operation,
            &format!("{path}.name"),
            CatalogStringKind::Text,
            true,
            &mut budget,
        )?
        .to_string();
        let description = optional_nullable_catalog_string(
            object.get("description"),
            operation,
            &format!("{path}.description"),
            CatalogStringKind::Text,
            true,
            &mut budget,
        )?;
        let category = optional_nullable_catalog_string(
            object.get("category"),
            operation,
            &format!("{path}.category"),
            CatalogStringKind::Token,
            true,
            &mut budget,
        )?;
        let option_type = strict_catalog_string(
            object.get("type"),
            operation,
            &format!("{path}.type"),
            CatalogStringKind::Token,
            false,
            &mut budget,
        )?;
        if option_type != "select" {
            return Err(invalid_config_receipt(
                operation,
                format!("{path}.type must be `select`"),
            ));
        }
        let current_value = strict_catalog_string(
            object.get("currentValue"),
            operation,
            &format!("{path}.currentValue"),
            CatalogStringKind::Token,
            true,
            &mut budget,
        )?
        .to_string();
        let values =
            parse_config_values_strict(object.get("options"), operation, index, &mut budget)?;
        parsed.push(CursorAcpConfigOption {
            id,
            name,
            description,
            category,
            option_type: Some(option_type.to_string()),
            current_value: Some(current_value),
            options: values,
        });
    }
    Ok(parsed)
}

#[derive(Debug, Clone, Copy)]
enum CatalogStringKind {
    Token,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigChoiceShape {
    Flat,
    Grouped,
}

#[derive(Debug, Default)]
struct ConfigCatalogBudget {
    entries: usize,
    strings: usize,
    token_bytes: usize,
    text_bytes: usize,
}

impl ConfigCatalogBudget {
    fn charge_entry(&mut self, operation: &str, path: &str) -> Result<(), CursorAcpError> {
        self.entries = self.entries.checked_add(1).ok_or_else(|| {
            invalid_config_receipt(operation, "configOptions entry count overflow")
        })?;
        if self.entries > MAX_CONFIG_CHOICES_TOTAL {
            return Err(invalid_config_receipt(
                operation,
                format!("{path} exceeds total option entry limit {MAX_CONFIG_CHOICES_TOTAL}"),
            ));
        }
        Ok(())
    }

    fn charge_string(
        &mut self,
        operation: &str,
        path: &str,
        kind: CatalogStringKind,
        bytes: usize,
    ) -> Result<(), CursorAcpError> {
        self.strings = self.strings.checked_add(1).ok_or_else(|| {
            invalid_config_receipt(operation, "configOptions string count overflow")
        })?;
        if self.strings > MAX_CONFIG_STRINGS_TOTAL {
            return Err(invalid_config_receipt(
                operation,
                format!("{path} exceeds total string count limit {MAX_CONFIG_STRINGS_TOTAL}"),
            ));
        }
        let (used, limit) = match kind {
            CatalogStringKind::Token => (&mut self.token_bytes, MAX_CONFIG_TOKEN_BYTES_TOTAL),
            CatalogStringKind::Text => (&mut self.text_bytes, MAX_CONFIG_TEXT_BYTES_TOTAL),
        };
        *used = (*used).checked_add(bytes).ok_or_else(|| {
            invalid_config_receipt(operation, "configOptions string byte count overflow")
        })?;
        if *used > limit {
            return Err(invalid_config_receipt(
                operation,
                format!("{path} exceeds total {:?} byte limit {limit}", kind),
            ));
        }
        Ok(())
    }
}

fn parse_config_values_strict(
    value: Option<&Value>,
    operation: &str,
    config_index: usize,
    budget: &mut ConfigCatalogBudget,
) -> Result<Vec<CursorAcpConfigValue>, CursorAcpError> {
    let value = value.ok_or_else(|| {
        invalid_config_receipt(
            operation,
            format!("configOptions[{config_index}].options is required"),
        )
    })?;
    let choices = value.as_array().ok_or_else(|| {
        invalid_config_receipt(
            operation,
            format!("configOptions[{config_index}].options must be an array"),
        )
    })?;
    if choices.len() > MAX_CONFIG_CHOICES_PER_AXIS {
        return Err(invalid_config_receipt(
            operation,
            format!(
                "configOptions[{config_index}].options count {} exceeds per-axis limit {MAX_CONFIG_CHOICES_PER_AXIS}",
                choices.len()
            ),
        ));
    }
    let mut parsed = Vec::new();
    let mut values = std::collections::HashSet::new();
    let mut groups = std::collections::HashSet::new();
    let mut shape = None;
    for (choice_index, choice) in choices.iter().enumerate() {
        let path = format!("configOptions[{config_index}].options[{choice_index}]");
        budget.charge_entry(operation, &path)?;
        let object = choice.as_object().ok_or_else(|| {
            invalid_config_receipt(operation, format!("{path} must be an object"))
        })?;
        let current_shape = match (object.contains_key("value"), object.contains_key("group")) {
            (true, false) => ConfigChoiceShape::Flat,
            (false, true) => ConfigChoiceShape::Grouped,
            _ => {
                return Err(invalid_config_receipt(
                    operation,
                    format!("{path} must contain exactly one of value or group"),
                ));
            }
        };
        if shape.is_some_and(|shape| shape != current_shape) {
            return Err(invalid_config_receipt(
                operation,
                format!(
                    "configOptions[{config_index}].options must not mix flat and grouped choices"
                ),
            ));
        }
        shape = Some(current_shape);

        match current_shape {
            ConfigChoiceShape::Flat => {
                if object.contains_key("options") {
                    return Err(invalid_config_receipt(
                        operation,
                        format!("{path} mixes a value with nested options"),
                    ));
                }
                let value = strict_catalog_string(
                    object.get("value"),
                    operation,
                    &format!("{path}.value"),
                    CatalogStringKind::Token,
                    false,
                    budget,
                )?
                .to_string();
                if !values.insert(value.clone()) {
                    return Err(invalid_config_receipt(
                        operation,
                        format!("duplicate option value at {path}.value"),
                    ));
                }
                let name = strict_catalog_string(
                    object.get("name"),
                    operation,
                    &format!("{path}.name"),
                    CatalogStringKind::Text,
                    true,
                    budget,
                )?
                .to_string();
                let description = optional_nullable_catalog_string(
                    object.get("description"),
                    operation,
                    &format!("{path}.description"),
                    CatalogStringKind::Text,
                    true,
                    budget,
                )?;
                parsed.push(CursorAcpConfigValue {
                    value,
                    name,
                    description,
                    group: None,
                    group_name: None,
                });
            }
            ConfigChoiceShape::Grouped => {
                let group = strict_catalog_string(
                    object.get("group"),
                    operation,
                    &format!("{path}.group"),
                    CatalogStringKind::Token,
                    false,
                    budget,
                )?
                .to_string();
                if !groups.insert(group.clone()) {
                    return Err(invalid_config_receipt(
                        operation,
                        format!("duplicate option group at {path}.group"),
                    ));
                }
                let group_name = strict_catalog_string(
                    object.get("name"),
                    operation,
                    &format!("{path}.name"),
                    CatalogStringKind::Text,
                    true,
                    budget,
                )?
                .to_string();
                let nested = object
                    .get("options")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        invalid_config_receipt(
                            operation,
                            format!("{path}.options must be an array for a group"),
                        )
                    })?;
                if nested.len() > MAX_CONFIG_CHOICES_PER_AXIS {
                    return Err(invalid_config_receipt(
                        operation,
                        format!(
                            "{path}.options count {} exceeds per-axis limit {MAX_CONFIG_CHOICES_PER_AXIS}",
                            nested.len()
                        ),
                    ));
                }
                for (nested_index, nested_choice) in nested.iter().enumerate() {
                    let nested_path = format!("{path}.options[{nested_index}]");
                    budget.charge_entry(operation, &nested_path)?;
                    if parsed.len() >= MAX_CONFIG_CHOICES_PER_AXIS {
                        return Err(invalid_config_receipt(
                            operation,
                            format!(
                                "configOptions[{config_index}] flattened option count exceeds per-axis limit {MAX_CONFIG_CHOICES_PER_AXIS}"
                            ),
                        ));
                    }
                    let nested_object = nested_choice.as_object().ok_or_else(|| {
                        invalid_config_receipt(
                            operation,
                            format!("{nested_path} must be a flat value object"),
                        )
                    })?;
                    if nested_object.get("group").is_some()
                        || nested_object.get("options").is_some()
                    {
                        return Err(invalid_config_receipt(
                            operation,
                            format!("{nested_path} must be a flat value object"),
                        ));
                    }
                    let value = strict_catalog_string(
                        nested_object.get("value"),
                        operation,
                        &format!("{nested_path}.value"),
                        CatalogStringKind::Token,
                        false,
                        budget,
                    )?
                    .to_string();
                    if !values.insert(value.clone()) {
                        return Err(invalid_config_receipt(
                            operation,
                            format!("duplicate option value at {nested_path}.value"),
                        ));
                    }
                    let name = strict_catalog_string(
                        nested_object.get("name"),
                        operation,
                        &format!("{nested_path}.name"),
                        CatalogStringKind::Text,
                        true,
                        budget,
                    )?
                    .to_string();
                    let description = optional_nullable_catalog_string(
                        nested_object.get("description"),
                        operation,
                        &format!("{nested_path}.description"),
                        CatalogStringKind::Text,
                        true,
                        budget,
                    )?;
                    // group/group_name 会进入每个扁平 value。按实际 retained multiplicity 计费，
                    // 否则一个 4 KiB group label 可在合法 entry 数内放大成 MiB 级稳定目录。
                    budget.charge_string(
                        operation,
                        &format!("{nested_path}.retained-group"),
                        CatalogStringKind::Token,
                        group.len(),
                    )?;
                    budget.charge_string(
                        operation,
                        &format!("{nested_path}.retained-group-name"),
                        CatalogStringKind::Text,
                        group_name.len(),
                    )?;
                    parsed.push(CursorAcpConfigValue {
                        value,
                        name,
                        description,
                        group: Some(group.clone()),
                        group_name: Some(group_name.clone()),
                    });
                }
            }
        }
    }
    Ok(parsed)
}

fn optional_nullable_catalog_string(
    value: Option<&Value>,
    operation: &str,
    path: &str,
    kind: CatalogStringKind,
    allow_empty: bool,
    budget: &mut ConfigCatalogBudget,
) -> Result<Option<String>, CursorAcpError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            strict_catalog_string(Some(value), operation, path, kind, allow_empty, budget)
                .map(|value| Some(value.to_string()))
        }
    }
}

fn strict_catalog_string<'a>(
    value: Option<&'a Value>,
    operation: &str,
    path: &str,
    kind: CatalogStringKind,
    allow_empty: bool,
    budget: &mut ConfigCatalogBudget,
) -> Result<&'a str, CursorAcpError> {
    let raw = value
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_config_receipt(operation, format!("{path} must be a string")))?;
    if (!allow_empty && raw.is_empty())
        || (matches!(kind, CatalogStringKind::Token) && !raw.is_empty() && raw.trim() != raw)
    {
        return Err(invalid_config_receipt(
            operation,
            format!("{path} has an invalid empty or whitespace-ambiguous token"),
        ));
    }
    let per_string_limit = match kind {
        CatalogStringKind::Token => MAX_CONFIG_TOKEN_BYTES,
        CatalogStringKind::Text => MAX_CONFIG_TEXT_BYTES,
    };
    if raw.len() > per_string_limit {
        return Err(invalid_config_receipt(
            operation,
            format!("{path} exceeds {:?} byte limit {per_string_limit}", kind),
        ));
    }
    let invalid_control = match kind {
        CatalogStringKind::Token => raw.chars().any(char::is_control),
        CatalogStringKind::Text => raw
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t')),
    };
    if invalid_control {
        return Err(invalid_config_receipt(
            operation,
            format!("{path} contains unsupported control characters"),
        ));
    }
    budget.charge_string(operation, path, kind, raw.len())?;
    Ok(raw)
}

fn invalid_config_receipt(operation: &str, detail: impl Into<String>) -> CursorAcpError {
    CursorAcpError::InvalidResponse {
        operation: operation.into(),
        detail: detail.into(),
    }
}

fn nonempty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn current_model_id(result: &Value) -> Option<String> {
    result
        .pointer("/models/currentModelId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 从 `session/*` 响应里抠出可选模型 id 列表。
///
/// 键名容忍两种写法(`modelId` 与 `id`):ACP 的 `ModelInfo` 用前者,但协议在演进 —— 认死一个
/// 的代价是某次升级后候选静默变空(面板没按钮,而且没有任何报错),不值得。
fn available_model_ids(result: &Value) -> Vec<String> {
    result
        .pointer("/models/availableModels")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    model
                        .get("modelId")
                        .or_else(|| model.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

struct ServerRequestReply {
    frame: Value,
    reason: &'static str,
}

/// server→client request 的应答表。
///
/// **绝不静默吞 id**:server 发出请求后会一直等那条 id 的响应,漏一个就是整轮永久挂死。
/// 所以每条分支——包括没见过的方法——都产出回帧。
///
/// 与 codex 侧「一律确定性拒绝」不同的一处:`session/request_permission` 这里**批准**。
/// 理由是无头路今天就带 `--yolo`(见 `driver/cursor.rs::headless_args`),两条通道必须同权限
/// 语义 —— 同一个 agent 的能力不该随通道漂移。spawn 参数里已经有 `--yolo`,
/// 这条是第二道闸(万一某个版本仍然问)。找不到 allow 选项时回 `cancelled`,fail-closed。
fn server_request_reply(id: &Value, method: &str, params: &Value) -> ServerRequestReply {
    let result = |value: Value, reason: &'static str| ServerRequestReply {
        frame: json!({"jsonrpc": "2.0", "id": id, "result": value}),
        reason,
    };
    let error = |code: i64, message: &str, reason: &'static str| ServerRequestReply {
        frame: json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        reason,
    };
    match method {
        "session/request_permission" => match pick_allow_option(params) {
            Some(option_id) => result(
                json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
                summon_i18n::text!("r24c6e3ef78"),
            ),
            // 没有任何 allow 选项 → 说明这不是我们理解的那种批准框。取消这一次请求(仍是合法
            // outcome,不吞 id、不挂死),让模型自己换条路走。
            None => result(
                json!({"outcome": {"outcome": "cancelled"}}),
                summon_i18n::text!("r8fa4b1f499"),
            ),
        },
        // fs/terminal 能力我们在握手时明确声明为 false,收到就是协议不一致。回标准
        // method not found 比编个假结果诚实。
        _ => error(
            -32601,
            "summon does not implement this client method",
            summon_i18n::text!("r3c65b18509"),
        ),
    }
}

/// 从 `options` 里挑一个「允许」项。优先 `allow_always`(少来回,与 `--yolo` 最接近),
/// 其次任何 kind 含 `allow` 的。
fn pick_allow_option(params: &Value) -> Option<String> {
    let options = params.get("options")?.as_array()?;
    let by_kind = |wanted: &str| {
        options
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(wanted))
            .and_then(|o| o.get("optionId").and_then(Value::as_str))
            .map(str::to_string)
    };
    by_kind("allow_always")
        .or_else(|| by_kind("allow_once"))
        .or_else(|| {
            options
                .iter()
                .find(|o| {
                    o.get("kind")
                        .and_then(Value::as_str)
                        .is_some_and(|k| k.contains("allow"))
                })
                .and_then(|o| o.get("optionId").and_then(Value::as_str))
                .map(str::to_string)
        })
}

fn remote_error(method: &str, error: &Value) -> CursorAcpError {
    CursorAcpError::Remote {
        method: method.into(),
        code: error.get("code").and_then(Value::as_i64),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown remote error")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fake_rpc_server_log, process_test_guard, write_fake_acp_server};
    use serde_json::json;

    const NEW_CASE: &str = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1","models":{"currentModelId":"gpt-5.6-sol","availableModels":[{"modelId":"gpt-5.6-sol","name":"GPT-5.6 SOL"}]},"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"gpt-5.6-sol","options":[{"value":"gpt-5.6-sol","name":"GPT-5.6 SOL"}]}]}}' ;;
"#;

    /// **真实 spawn 回归**:污染父环境后起一个 fake ACP server,读它**进程里实际拿到**的环境。
    ///
    /// 这条补的是纯逻辑测试够不着的那一格 —— `child_env` 返回的 vector 对不对是一回事,
    /// 那份 vector 有没有真的落到子进程里是另一回事。上一版就在这类"两侧对接"上栽过:
    /// CLI 与 Core 各自的测试全绿,拼起来却是拒绝所有命令。
    ///
    /// 无需真 provider:fake server 启动时把三件套写进 `.child-env`,读文件断言即可。
    #[test]
    fn a_spawned_child_gets_this_chat_identity_not_the_polluted_parent_env() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_acp_server(tmp.path(), NEW_CASE);

        // 父进程(模拟 daemon)带着别人的身份。
        std::env::set_var("SUMMON_AGENT", "flare-5");
        std::env::set_var("SUMMON_CHAT_ID", "oc_someone_else");
        std::env::set_var("SUMMON_CONVERSATION_TAG", "gffffffffffffffff");
        // **探针**:一个父进程有、而 `config.env` 里没有的变量。三件套会被显式 push 覆盖,
        // 所以它们验不出 `env_clear` —— 只有"没人覆盖它"的变量能证明环境确实被清空过。
        // (这条是变异验证逼出来的:去掉 `env_clear` 后原来的断言照样绿。)
        //
        // 名字**不带 `SUMMON_` 前缀**:`essential_system_env` 会**有意**把所有 `SUMMON_*` 运维键
        // (SUMMON_HOME/SUMMON_LOG 等)复制给子进程,只排除身份三键 —— 用 SUMMON_ 开头的探针
        // 测到的是那条设计,不是 env_clear。
        std::env::set_var("ACP_LEAK_PROBE", "leaked-from-parent");

        // 显式给出本 chat 的身份(生产由 `RpcPool::child_env` 构造)。
        let mut env = crate::runtime::daemon::essential_system_env();
        env.push(("SUMMON_AGENT".into(), "ambrose".into()));
        env.push(("SUMMON_CHAT_ID".into(), "oc_mine".into()));
        env.push(("SUMMON_CONVERSATION_TAG".into(), "gaaaaaaaaaaaaaaaa".into()));

        let client = CursorAcpClient::spawn(CursorAcpConfig {
            program: script.to_string_lossy().into_owned(),
            args: vec!["acp".into()],
            work_dir: None,
            config_dir: tmp.path().join("cursor-config"),
            request_timeout: Duration::from_secs(30),
            env,
        })
        .expect("假 ACP server 应能握手");
        drop(client);

        std::env::remove_var("SUMMON_AGENT");
        std::env::remove_var("SUMMON_CHAT_ID");
        std::env::remove_var("SUMMON_CONVERSATION_TAG");
        std::env::remove_var("ACP_LEAK_PROBE");

        let dumped = std::fs::read_to_string(
            fake_rpc_server_log(tmp.path()).with_extension("jsonl.child-env"),
        )
        .or_else(|_| std::fs::read_to_string(tmp.path().join("requests.jsonl.child-env")))
        .expect("fake server 应把自己的环境写出来");
        assert!(
            dumped.contains("SUMMON_AGENT=ambrose"),
            "子进程必须拿到本 chat 的 agent,实际:\n{dumped}"
        );
        assert!(
            dumped.contains("SUMMON_CHAT_ID=oc_mine"),
            "子进程必须拿到本 chat 的 chat_id,实际:\n{dumped}"
        );
        assert!(
            !dumped.contains("flare-5") && !dumped.contains("oc_someone_else"),
            "父进程的陈旧身份一个字都不能漏进子进程,实际:\n{dumped}"
        );
        assert!(
            !dumped.contains("leaked-from-parent"),
            "未被显式注入的父进程变量必须被 env_clear 挡掉,实际:\n{dumped}"
        );
    }

    fn client_with(cases: &str) -> (CursorAcpClient, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_acp_server(tmp.path(), cases);
        let client = CursorAcpClient::spawn(CursorAcpConfig {
            program: script.to_string_lossy().into_owned(),
            args: vec!["acp".into()],
            work_dir: None,
            config_dir: tmp.path().join("cursor-config"),
            request_timeout: Duration::from_secs(30),
            env: crate::runtime::daemon::essential_system_env(),
        })
        .expect("假 ACP server 应能握手");
        (client, tmp)
    }

    fn requests(dir: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(fake_rpc_server_log(dir))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn sent(dir: &std::path::Path, method: &str) -> Vec<Value> {
        requests(dir)
            .into_iter()
            .filter(|value| value.get("method").and_then(Value::as_str) == Some(method))
            .collect()
    }

    #[test]
    fn jsonl_reader_accepts_the_exact_limit_and_fails_before_growing_past_it() {
        let mut exact = vec![b'x'; MAX_JSONL_FRAME_BYTES];
        exact.push(b'\n');
        let mut reader = BufReader::with_capacity(8 * 1024, std::io::Cursor::new(exact));
        let mut frame = Vec::new();
        assert_eq!(
            read_bounded_jsonl_frame(&mut reader, &mut frame).unwrap(),
            Some(())
        );
        assert_eq!(frame.len(), MAX_JSONL_FRAME_BYTES);
        assert_eq!(
            read_bounded_jsonl_frame(&mut reader, &mut frame).unwrap(),
            None
        );

        let over_limit = vec![b'x'; MAX_JSONL_FRAME_BYTES + 1];
        let mut reader = BufReader::with_capacity(8 * 1024, std::io::Cursor::new(over_limit));
        let error = read_bounded_jsonl_frame(&mut reader, &mut frame)
            .expect_err("超限帧必须在继续扩容前 fail-stop");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(frame.len() <= MAX_JSONL_FRAME_BYTES);
        assert_eq!(
            error.to_string(),
            format!(
                "cursor ACP JSONL 单帧超过 {} bytes 安全上限",
                MAX_JSONL_FRAME_BYTES
            ),
            "边界错误不得拼入不可信帧正文"
        );
    }

    #[test]
    fn jsonl_reader_preserves_crlf_and_unterminated_frame_semantics() {
        let mut reader = std::io::Cursor::new(b"{}\r\n[]".as_slice());
        let mut frame = Vec::new();
        assert_eq!(
            read_bounded_jsonl_frame(&mut reader, &mut frame).unwrap(),
            Some(())
        );
        assert_eq!(frame, b"{}");
        assert_eq!(
            read_bounded_jsonl_frame(&mut reader, &mut frame).unwrap(),
            Some(())
        );
        assert_eq!(frame, b"[]");
        assert_eq!(
            read_bounded_jsonl_frame(&mut reader, &mut frame).unwrap(),
            None
        );
    }

    #[test]
    fn invalid_json_diagnostic_never_echoes_the_untrusted_frame() {
        let frame = br#"{"secret":"do-not-echo""#;
        let error = serde_json::from_slice::<Value>(frame).unwrap_err();
        let detail = invalid_json_detail(&error, frame.len());
        assert!(detail.contains(&format!("frame_bytes={}", frame.len())));
        assert!(!detail.contains("do-not-echo"));
    }

    #[test]
    fn config_catalog_rejects_every_structural_and_wire_budget_overrun() {
        let reject = |options: Vec<Value>, expected: &str| {
            let error = parse_session("sess", &json!({"configOptions": options}), "session/new")
                .expect_err("超预算目录必须 fail-stop");
            let rendered = error.to_string();
            assert!(
                rendered.contains(expected),
                "错误应指出 {expected}，实得: {rendered}"
            );
        };

        reject(
            (0..=MAX_CONFIG_AXES)
                .map(|index| json!({"id": format!("axis-{index}")}))
                .collect(),
            "axis count",
        );
        reject(
            vec![json!({
                "id": "too-many-per-axis",
                "name": "Too many per axis",
                "type": "select",
                "currentValue": "",
                "options": (0..=MAX_CONFIG_CHOICES_PER_AXIS)
                    .map(|index| json!({
                        "value": format!("value-{index}"),
                        "name": format!("Value {index}")
                    }))
                    .collect::<Vec<_>>()
            })],
            "per-axis limit",
        );

        let mut remaining = MAX_CONFIG_CHOICES_TOTAL + 1;
        let mut too_many_total = Vec::new();
        for axis_index in 0..MAX_CONFIG_AXES {
            if remaining == 0 {
                break;
            }
            let count = remaining.min(MAX_CONFIG_CHOICES_PER_AXIS);
            too_many_total.push(json!({
                "id": format!("axis-{axis_index}"),
                "name": format!("Axis {axis_index}"),
                "type": "select",
                "currentValue": "",
                "options": (0..count)
                    .map(|choice_index| json!({
                        "value": format!("value-{choice_index}"),
                        "name": format!("Value {choice_index}")
                    }))
                    .collect::<Vec<_>>()
            }));
            remaining -= count;
        }
        assert_eq!(remaining, 0, "测试常量本身必须能覆盖 total option 上限");
        reject(too_many_total, "total option entry limit");

        reject(
            vec![json!({"id": "x".repeat(MAX_CONFIG_TOKEN_BYTES + 1)})],
            "Token byte limit",
        );

        let too_many_strings = [128usize, 128, 128, 22]
            .into_iter()
            .enumerate()
            .map(|(axis_index, count)| {
                json!({
                    "id": format!("string-axis-{axis_index}"),
                    "name": format!("String axis {axis_index}"),
                    "type": "select",
                    "currentValue": "",
                    "options": [{
                        "group": format!("group-{axis_index}"),
                        "name": format!("Group {axis_index}"),
                        "options": (0..count)
                            .map(|choice_index| json!({
                                "value": format!("value-{choice_index}"),
                                "name": "name",
                                "description": "description"
                            }))
                            .collect::<Vec<_>>()
                    }]
                })
            })
            .collect();
        reject(too_many_strings, "total string count limit");

        let too_many_token_bytes = (0..2)
            .map(|axis_index| {
                json!({
                    "id": format!("token-axis-{axis_index}"),
                    "name": format!("Token axis {axis_index}"),
                    "type": "select",
                    "currentValue": "",
                    "options": (0..MAX_CONFIG_CHOICES_PER_AXIS)
                        .map(|choice_index| {
                            let prefix = format!("{axis_index}-{choice_index}-");
                            let value = format!(
                                "{prefix}{}",
                                "x".repeat(300usize.saturating_sub(prefix.len()))
                            );
                            json!({"value": value, "name": "Value"})
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        reject(too_many_token_bytes, "total Token byte limit");

        reject(
            vec![json!({
                "id": "text-axis",
                "name": "Text axis",
                "type": "select",
                "currentValue": "",
                "options": (0..33)
                    .map(|index| json!({
                        "value": format!("value-{index}"),
                        "name": format!("Value {index}"),
                        "description": "x".repeat(MAX_CONFIG_TEXT_BYTES)
                    }))
                    .collect::<Vec<_>>()
            })],
            "total Text byte limit",
        );
    }

    #[test]
    fn trusted_catalog_requires_the_complete_acp_select_schema() {
        let malformed = [
            // 旧 fail-soft parser 会补 name=id，或把缺失 options 当空目录后标 trusted。
            json!({
                "id": "fast",
                "type": "select",
                "currentValue": "false",
                "options": []
            }),
            json!({
                "id": "fast",
                "name": "Fast",
                "currentValue": "false",
                "options": []
            }),
            json!({
                "id": "fast",
                "name": "Fast",
                "type": "select",
                "options": []
            }),
            json!({
                "id": "fast",
                "name": "Fast",
                "type": "select",
                "currentValue": "false"
            }),
            json!({
                "id": "fast",
                "name": "Fast",
                "type": "toggle",
                "currentValue": "false",
                "options": []
            }),
            json!({
                "id": "fast",
                "name": "Fast",
                "type": "select",
                "currentValue": "false",
                "options": [{"value": "false"}]
            }),
            json!({
                "id": "grouped",
                "name": "Grouped",
                "type": "select",
                "currentValue": "nested",
                "options": [{
                    "group": "group",
                    "options": [{"value": "nested", "name": "Nested"}]
                }]
            }),
            json!({
                "id": "grouped",
                "name": "Grouped",
                "type": "select",
                "currentValue": "nested",
                "options": [{
                    "group": "group",
                    "name": "Group",
                    "options": [{"value": "nested"}]
                }]
            }),
            json!({
                "id": "mixed",
                "name": "Mixed",
                "type": "select",
                "currentValue": "flat",
                "options": [
                    {"value": "flat", "name": "Flat"},
                    {
                        "group": "grouped",
                        "name": "Grouped",
                        "options": [{"value": "nested", "name": "Nested"}]
                    }
                ]
            }),
        ];
        for option in malformed {
            parse_session("sess", &json!({"configOptions": [option]}), "session/new")
                .expect_err("缺必填字段、非 select 或 flat/group 混排都不能标 trusted");
        }
    }

    #[test]
    fn nullable_metadata_and_empty_current_value_follow_the_acp_schema() {
        let session = parse_session(
            "sess",
            &json!({
                "configOptions": [{
                    "id": "fast",
                    "name": "  Fast display\n",
                    "description": null,
                    "category": null,
                    "type": "select",
                    "currentValue": "",
                    "options": [{
                        "value": "false",
                        "name": "",
                        "description": null
                    }]
                }]
            }),
            "session/new",
        )
        .unwrap();
        assert!(session.config_options_trusted);
        let option = &session.config_options[0];
        assert_eq!(option.name, "  Fast display\n", "展示文本必须原样保留");
        assert_eq!(option.description, None);
        assert_eq!(option.category, None);
        assert_eq!(option.current_value.as_deref(), Some(""));
        assert_eq!(option.options[0].name, "");
        assert_eq!(option.options[0].description, None);
    }

    #[test]
    fn grouped_label_clones_are_charged_by_retained_multiplicity() {
        let error = parse_session(
            "sess",
            &json!({
                "configOptions": [{
                    "id": "model",
                    "name": "Model",
                    "type": "select",
                    "currentValue": "",
                    "options": [{
                        "group": "group",
                        "name": "x".repeat(MAX_CONFIG_TEXT_BYTES),
                        "options": (0..40)
                            .map(|index| json!({
                                "value": format!("value-{index}"),
                                "name": format!("Value {index}")
                            }))
                            .collect::<Vec<_>>()
                    }]
                }]
            }),
            "session/new",
        )
        .expect_err("group label 的每份稳定 String clone 都必须进入总预算");
        assert!(
            error.to_string().contains("total Text byte limit"),
            "实得: {error}"
        );
    }

    /// 握手 + 冷启:fs/terminal 保持关闭，同时必须用 Cursor 的精确 `_meta` 扩展进入
    /// parameterized namespace；否则 Fast/context/reasoning 根本不会成为可写 configOptions。
    #[test]
    fn handshake_declares_parameterized_model_picker_without_fs_or_terminal() {
        let _guard = process_test_guard();
        let (mut client, tmp) = client_with(NEW_CASE);
        let session = client.session_new(Some("/tmp"), None).unwrap();
        assert_eq!(session.session_id, "sess-1");
        assert_eq!(session.current_model_id.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(session.available_models, vec!["gpt-5.6-sol"]);
        assert!(
            session.config_options_trusted,
            "显式返回且完整通过严格解析的初始目录才可标可信"
        );

        let init = &sent(tmp.path(), "initialize")[0];
        assert_eq!(init["params"]["protocolVersion"], 1);
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["readTextFile"],
            false
        );
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["writeTextFile"],
            false
        );
        assert_eq!(init["params"]["clientCapabilities"]["terminal"], false);
        assert_eq!(
            init["params"]["clientCapabilities"]["_meta"]["parameterizedModelPicker"], true,
            "扩展名、层级和值都属于 Cursor wire contract，任一变化都会退回 bracket namespace"
        );
        // cwd 必须钉死传进去(server 自己的 cwd 可能停在 $HOME)。
        assert_eq!(sent(tmp.path(), "session/new")[0]["params"]["cwd"], "/tmp");
    }

    /// `CURSOR_CONFIG_DIR` 是参数写能力的隔离边界；不显式注入就会修改 owner 的 `~/.cursor`。
    #[test]
    fn spawn_injects_the_isolated_cursor_config_dir() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("agent-cursor-config");
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1","configOptions":[{"id":"observed-config-dir","name":"Observed config dir","type":"select","currentValue":"'"$CURSOR_CONFIG_DIR"'","options":[{"value":"'"$CURSOR_CONFIG_DIR"'","name":"Current"}]}]}}' ;;
"#;
        let script = write_fake_acp_server(tmp.path(), cases);
        let mut client = CursorAcpClient::spawn(CursorAcpConfig {
            program: script.to_string_lossy().into_owned(),
            args: vec!["acp".into()],
            work_dir: None,
            config_dir: config_dir.clone(),
            request_timeout: Duration::from_secs(30),
            env: crate::runtime::daemon::essential_system_env(),
        })
        .expect("假 ACP server 应能握手");

        let session = client.session_new(Some("/tmp"), None).unwrap();
        assert_eq!(
            session.config_options[0].current_value.as_deref(),
            Some(config_dir.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn spawn_rejects_a_non_absolute_config_root_before_starting_cursor() {
        let Err(error) = CursorAcpClient::spawn(CursorAcpConfig {
            program: "/definitely/not/cursor-agent".into(),
            args: vec!["acp".into()],
            work_dir: None,
            config_dir: "relative/cursor-config".into(),
            request_timeout: Duration::from_secs(1),
            env: crate::runtime::daemon::essential_system_env(),
        }) else {
            panic!("相对配置根必须在 spawn 前 fail-closed");
        };
        assert!(
            matches!(
                error,
                CursorAcpError::InvalidResponse { ref operation, .. } if operation == "spawn"
            ),
            "实得: {error}"
        );
    }

    /// modes 是非安全关键展示目录，仍可逐项跳过坏条目；显式返回的 configOptions 则必须整份
    /// 严格成功，才能把目录 provenance 标为可信。
    #[test]
    fn session_new_parses_modes_and_marks_a_strict_catalog_trusted() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1","modes":{"currentModeId":"agent","availableModes":[{"id":"agent","name":"Agent","description":"Full access"},{"id":"plan"},{"name":"missing-id"},"future-shape"]},"configOptions":[{"id":"mode","name":"Mode","description":"Execution mode","category":"mode","type":"select","currentValue":"agent","options":[{"value":"agent","name":"Agent","description":"Full access"},{"value":"plan","name":"plan"}]},{"id":"fast","name":"Fast","category":"model_config","type":"select","currentValue":"false","options":[{"value":"false","name":"Off"},{"value":"true","name":"Fast"}]}]}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        let session = client.session_new(Some("/tmp"), None).unwrap();

        assert_eq!(session.current_mode_id.as_deref(), Some("agent"));
        assert_eq!(session.available_modes.len(), 2);
        assert_eq!(session.available_modes[0].id, "agent");
        assert_eq!(session.available_modes[0].name, "Agent");
        assert_eq!(
            session.available_modes[0].description.as_deref(),
            Some("Full access")
        );
        assert_eq!(session.available_modes[1].id, "plan");
        assert_eq!(
            session.available_modes[1].name, "plan",
            "缺展示名时应回退稳定 id，而非丢掉整个合法模式"
        );

        assert_eq!(session.config_options.len(), 2);
        let mode = &session.config_options[0];
        assert_eq!(mode.id, "mode");
        assert_eq!(mode.name, "Mode");
        assert_eq!(mode.description.as_deref(), Some("Execution mode"));
        assert_eq!(mode.category.as_deref(), Some("mode"));
        assert_eq!(mode.option_type.as_deref(), Some("select"));
        assert_eq!(mode.current_value.as_deref(), Some("agent"));
        assert_eq!(mode.options.len(), 2);
        assert_eq!(mode.options[1].value, "plan");
        assert_eq!(mode.options[1].name, "plan");

        let fast = &session.config_options[1];
        assert_eq!(fast.id, "fast", "未知 config id 也必须动态保留");
        assert_eq!(fast.name, "Fast");
        assert_eq!(fast.options.len(), 2);
        assert!(session.config_options_trusted);
    }

    #[test]
    fn absent_catalog_is_untrusted_but_an_explicit_malformed_catalog_fails_closed() {
        let empty = parse_session("sess", &json!({}), "session/new").unwrap();
        assert_eq!(empty.current_mode_id, None);
        assert!(empty.available_modes.is_empty());
        assert!(empty.config_options.is_empty());
        assert!(!empty.config_options_trusted);

        let explicit_null =
            parse_session("sess", &json!({"configOptions": null}), "session/load").unwrap();
        assert!(explicit_null.config_options.is_empty());
        assert!(
            !explicit_null.config_options_trusted,
            "ACP 允许 new/load 用 null 表示没公布目录，不能据此产生可信证据"
        );

        parse_session(
            "sess",
            &json!({"modes": [], "configOptions": {"id": "not-an-array"}}),
            "session/new",
        )
        .expect_err("显式目录不是数组时不能降级成空目录并冒充可信");
    }

    #[test]
    fn config_catalog_can_supply_modes_and_preserves_groups_losslessly() {
        let config_only = parse_session(
            "sess",
            &json!({
                "configOptions": [{
                    "id": "mode",
                    "name": "Mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [{"value": "ask", "name": "Ask"}]
                }]
            }),
            "session/new",
        )
        .unwrap();
        assert_eq!(config_only.current_mode_id.as_deref(), Some("ask"));
        assert_eq!(config_only.available_modes.len(), 1);
        assert_eq!(config_only.available_modes[0].id, "ask");
        assert!(config_only.config_options_trusted);

        let grouped = parse_session(
            "sess",
            &json!({
                "configOptions": [{
                    "id": "model",
                    "name": "Model",
                    "type": "select",
                    "currentValue": "gpt-1",
                    "options": [
                        {
                            "group": "gpt",
                            "name": "GPT",
                            "options": [{"value": "gpt-1", "name": "GPT 1"}]
                        },
                        {
                            "group": "claude",
                            "name": "claude",
                            "options": [{"value": "claude-1", "name": "claude-1"}]
                        }
                    ]
                }]
            }),
            "session/load",
        )
        .unwrap();
        let values = &grouped.config_options[0].options;
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].value, "gpt-1");
        assert_eq!(values[0].group.as_deref(), Some("gpt"));
        assert_eq!(values[0].group_name.as_deref(), Some("GPT"));
        assert_eq!(values[1].name, "claude-1");
        assert_eq!(values[1].group.as_deref(), Some("claude"));
        assert_eq!(values[1].group_name.as_deref(), Some("claude"));
        assert!(grouped.config_options_trusted);
    }

    /// mode 是标准 ACP 会话轴，切换只发 `session/set_mode`，不碰 Cursor 的全局模型参数 setter。
    #[test]
    fn set_mode_sends_the_standard_session_request() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/set_mode'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{}}' ;;
"#;
        let (mut client, tmp) = client_with(cases);
        client.session_new(Some("/tmp"), None).unwrap();
        client.set_mode("sess-1", "plan", None).unwrap();

        let frames = sent(tmp.path(), "session/set_mode");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["params"]["sessionId"], "sess-1");
        assert_eq!(frames[0]["params"]["modeId"], "plan");
        assert!(
            sent(tmp.path(), "session/set_config_option").is_empty(),
            "mode 是会话轴，不应偷偷改 ACP 进程配置根里的模型参数"
        );
    }

    /// 参数 setter 的成功回执不是空对象，而是**完整的新目录**；调用方随后要靠它刷新动态 id/value。
    #[test]
    fn set_config_option_sends_exact_values_and_returns_the_full_catalog() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/set_config_option'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"gpt-5.6-sol","options":[{"value":"gpt-5.6-sol","name":"GPT-5.6 SOL"}]},{"id":"context","name":"Context","type":"select","currentValue":"1m","options":[{"value":"272k","name":"272K"},{"value":"1m","name":"1M"}]},{"id":"fast","name":"Fast","type":"select","currentValue":"false","options":[{"value":"false","name":"Off"},{"value":"true","name":"Fast"}]}]}}' ;;
"#;
        let (mut client, tmp) = client_with(cases);
        client.session_new(Some("/tmp"), None).unwrap();

        let catalog = client
            .set_config_option("sess-1", "context", "1m", None)
            .unwrap();
        assert_eq!(
            catalog
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec!["model", "context", "fast"],
            "必须把完整返回目录交给上层，不能只拼一个本地 option"
        );
        assert_eq!(catalog[1].current_value.as_deref(), Some("1m"));

        let frames = sent(tmp.path(), "session/set_config_option");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["params"]["sessionId"], "sess-1");
        assert_eq!(frames[0]["params"]["configId"], "context");
        assert_eq!(frames[0]["params"]["value"], "1m");
    }

    /// Cursor 会对不兼容组合静默归一化并返回成功；只有回执里的 requested currentValue exact
    /// 才能证明写入生效，不能把 JSON-RPC success 当成功。
    #[test]
    fn set_config_option_rejects_a_silent_server_fallback() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/set_config_option'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":"context","name":"Context","type":"select","currentValue":"272k","options":[{"value":"272k","name":"272K"},{"value":"1m","name":"1M"}]}]}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        client.session_new(Some("/tmp"), None).unwrap();

        let error = client
            .set_config_option("sess-1", "context", "1m", None)
            .expect_err("server 静默退回 272k 时必须 fail-closed");
        assert!(
            matches!(
                error,
                CursorAcpError::InvalidResponse { ref operation, .. }
                    if operation == "session/set_config_option"
            ),
            "实得: {error}"
        );
    }

    #[test]
    fn set_config_option_rejects_trimmed_identifiers_or_values() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/set_config_option'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":" context ","name":"Context","type":"select","currentValue":" 1m ","options":[{"value":"1m","name":"1M"}]}]}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        client.session_new(Some("/tmp"), None).unwrap();

        client
            .set_config_option("sess-1", "context", "1m", None)
            .expect_err("trim 后相等不属于 exact wire 成功");
    }

    #[test]
    fn set_config_option_rejects_a_malformed_sibling_in_the_full_catalog() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/set_config_option'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":"context","name":"Context","type":"select","currentValue":"1m","options":[{"value":"272k","name":"272K"},{"value":"1m","name":"1M"}]},{"name":"missing-id","type":"select","currentValue":"","options":[]}]}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        client.session_new(Some("/tmp"), None).unwrap();

        client
            .set_config_option("sess-1", "context", "1m", None)
            .expect_err("完整目录里任何坏 sibling 都不能被静默丢弃");
    }

    #[test]
    fn set_config_option_rejects_duplicate_config_ids() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/set_config_option'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":"context","name":"Context","type":"select","currentValue":"1m","options":[{"value":"1m","name":"1M"}]},{"id":"context","name":"Context","type":"select","currentValue":"1m","options":[{"value":"1m","name":"1M"}]}]}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        client.session_new(Some("/tmp"), None).unwrap();

        client
            .set_config_option("sess-1", "context", "1m", None)
            .expect_err("重复 requested id 不能由 find(first) 冒充唯一 exact 回执");
    }

    /// 一轮:更新流逐条到手,终态来自 prompt 的**响应**而不是通知。
    #[test]
    fn a_turn_streams_updates_and_ends_on_the_prompt_response() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"po"}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ng"}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}' ;;
"#;
        let (mut client, tmp) = client_with(cases);
        client.session_new(None, None).unwrap();
        client.prompt_start("sess-1", "ping").unwrap();
        assert!(
            client.interrupt_handle().active_prompt_id().is_some(),
            "投递之后把手就该看得见在跑的轮"
        );

        let mut texts = Vec::new();
        let stop = loop {
            match client
                .recv_turn_event(Some(Duration::from_secs(5)))
                .unwrap()
            {
                AcpTurnEvent::Update(CursorAcpUpdate::AgentMessageChunk(t)) => texts.push(t),
                AcpTurnEvent::Update(_) => {}
                AcpTurnEvent::Ended { stop_reason } => break stop_reason,
            }
        };
        assert_eq!(texts, vec!["po".to_string(), "ng".to_string()]);
        assert_eq!(stop, "end_turn");
        assert_eq!(
            client.interrupt_handle().active_prompt_id(),
            None,
            "终态之后不该再有在跑的轮"
        );
        assert_eq!(
            sent(tmp.path(), "session/prompt")[0]["params"]["prompt"][0]["text"],
            "ping"
        );
    }

    /// **§5.8 的实现落点**:软打断发的是 `session/cancel` **notification**(不带 id)。
    /// 带了 id 就是 request 形式,实测 server 回 -32601 且**不打断** —— 这条断言就是那个坑的钉子。
    ///
    /// 假 server 先同步推一条增量再转后台慢发,主循环照常读 stdin:cancel 一到就收掉后台、回终态,
    /// 与真 server 一样边推流边收打断。增量在终态之前已经写出,所以这一轮确实是在推流途中被打断的。
    #[test]
    fn soft_interrupt_sends_cancel_as_a_notification_without_an_id() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) prompt_id=$id;
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x"}}}}';
        (i=1; while [ $i -lt 10 ]; do sleep 0.05; printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x"}}}}'; i=$((i+1)); done) & streamer=$! ;;
    *'session/cancel'*) kill "$streamer" 2>/dev/null; printf '%s\n' '{"jsonrpc":"2.0","id":'"$prompt_id"',"result":{"stopReason":"cancelled"}}' ;;
"#;
        let (mut client, tmp) = client_with(cases);
        client.session_new(None, None).unwrap();
        let handle = client.interrupt_handle();
        let pgid = client.pgid();
        client.prompt_start("sess-1", "数到 100").unwrap();
        assert!(
            handle.active_prompt_id().is_some(),
            "prompt 帧写出后把手应能看到在跑的轮"
        );

        let interrupter =
            std::thread::spawn(move || handle.interrupt_within(Duration::from_secs(3)));

        let mut updates = 0_usize;
        let stop = loop {
            match client
                .recv_turn_event(Some(Duration::from_secs(6)))
                .unwrap()
            {
                AcpTurnEvent::Ended { stop_reason } => break stop_reason,
                AcpTurnEvent::Update(_) => updates += 1,
            }
        };
        assert_eq!(stop, "cancelled");
        assert!(updates > 0, "打断应发生在推流途中");
        assert_eq!(interrupter.join().unwrap(), RpcInterruptOutcome::Settled);

        let cancels = sent(tmp.path(), "session/cancel");
        assert_eq!(cancels.len(), 1, "必须真的发出 session/cancel");
        assert!(
            cancels[0].get("id").is_none(),
            "session/cancel 必须是 notification;带 id 会被 server 当 request 回 -32601 且不打断"
        );
        assert_eq!(cancels[0]["params"]["sessionId"], "sess-1");
        assert!(
            crate::runtime::inflight::group_alive(pgid),
            "软打断成功就不该把会话进程一起杀了"
        );
    }

    /// 轮已结束之后才到的打断:不发帧、**尤其不许 SIGINT**(会把会话连同下一轮一起杀掉)。
    #[test]
    fn interrupt_after_the_turn_ended_is_a_no_op() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}' ;;
"#;
        let (mut client, tmp) = client_with(cases);
        client.session_new(None, None).unwrap();
        client.prompt_start("sess-1", "ping").unwrap();
        let handle = client.interrupt_handle();
        let pgid = client.pgid();
        assert!(matches!(
            client
                .recv_turn_event(Some(Duration::from_secs(5)))
                .unwrap(),
            AcpTurnEvent::Ended { .. }
        ));

        assert_eq!(handle.interrupt(), RpcInterruptOutcome::Idle);
        assert!(sent(tmp.path(), "session/cancel").is_empty());
        assert!(
            crate::runtime::inflight::group_alive(pgid),
            "迟到的打断绝不许杀掉会话进程"
        );
    }

    /// **懒重连的坑**:`session/load` 会把整段历史当通知重放。不丢掉的话,下一轮的第一批事件
    /// 就是历史 —— 卡片会把上一轮的回复当本轮重发一遍。
    #[test]
    fn session_load_drops_the_replayed_history_before_the_next_turn() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"上一轮我说的"}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"上一轮的回复"}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"usage_update","used":42000,"size":200000}}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"models":{"currentModelId":"m"}}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"本轮"}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        let session = client.session_load("sess-1", Some("/tmp"), None).unwrap();
        assert_eq!(session.session_id, "sess-1");
        assert_eq!(
            client.context_usage(),
            Some(CursorContextUsage {
                used_tokens: 42_000,
                window_tokens: 200_000,
            }),
            "load 历史正文应丢弃，但 session 级 usage 必须保留"
        );

        client.prompt_start("sess-1", "接着说").unwrap();
        let mut texts = Vec::new();
        loop {
            match client
                .recv_turn_event(Some(Duration::from_secs(5)))
                .unwrap()
            {
                AcpTurnEvent::Update(CursorAcpUpdate::AgentMessageChunk(t)) => texts.push(t),
                AcpTurnEvent::Update(_) => {}
                AcpTurnEvent::Ended { .. } => break,
            }
        }
        assert_eq!(
            texts,
            vec!["本轮".to_string()],
            "历史重放必须在 load 时丢掉,否则会被当成本轮回复重发"
        );
    }

    /// 新版 Cursor 的 `session/load` 响应不带 sessionId；请求 id 是 authoritative。旧版若仍回
    /// sessionId，则必须 exact match，防止接错上下文。
    #[test]
    fn session_load_rejects_a_mismatched_optional_session_id() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"other-session"}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        let error = client
            .session_load("expected-session", Some("/tmp"), None)
            .expect_err("响应若带 sessionId 就必须和请求 exact match");
        assert!(
            matches!(
                error,
                CursorAcpError::InvalidResponse { ref operation, .. }
                    if operation == "session/load"
            ),
            "实得: {error}"
        );
        assert!(error.to_string().contains("sessionId mismatch"));
    }

    #[test]
    fn session_load_rejects_a_non_string_optional_session_id() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":7}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        let error = client
            .session_load("expected-session", Some("/tmp"), None)
            .expect_err("存在的 sessionId 不是字符串时不能按缺失兼容");
        assert!(
            error.to_string().contains("must be a string"),
            "实得: {error}"
        );
    }

    /// server→client 请求必须**全部应答**:漏一个 id,server 就一直等,整轮永久挂死。
    /// 批准框按 `--yolo` 语义批准,其余方法保底回 method-not-found。
    #[test]
    fn every_server_request_is_answered_and_the_turn_still_completes() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"sess-1","options":[{"optionId":"no","kind":"reject_once"},{"optionId":"yes","kind":"allow_always"}]}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":9002,"method":"fs/read_text_file","params":{"path":"/etc/passwd"}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}' ;;
"#;
        let (mut client, tmp) = client_with(cases);
        client.session_new(None, None).unwrap();
        client.prompt_start("sess-1", "跑个命令").unwrap();
        assert!(matches!(
            client
                .recv_turn_event(Some(Duration::from_secs(5)))
                .unwrap(),
            AcpTurnEvent::Ended { .. }
        ));

        // 假 server 把收到的每一行都记进 log —— 包括我们回给它的响应帧。**要等**:轮的终态
        // (prompt 响应)是 server 一口气打完的,它此刻可能还没把我们的回帧从 stdin 读走。
        let deadline = Instant::now() + Duration::from_secs(3);
        let replies = loop {
            let replies: Vec<Value> = requests(tmp.path())
                .into_iter()
                .filter(|v| v.get("method").is_none())
                .collect();
            if replies.len() >= 2 || Instant::now() >= deadline {
                break replies;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let permission = replies
            .iter()
            .find(|v| v["id"] == 9001)
            .expect("批准请求必须被应答");
        assert_eq!(permission["result"]["outcome"]["outcome"], "selected");
        assert_eq!(
            permission["result"]["outcome"]["optionId"], "yes",
            "应选允许项(与无头路 --yolo 同权限语义)"
        );
        let fs_read = replies
            .iter()
            .find(|v| v["id"] == 9002)
            .expect("未实现的方法也必须应答,绝不吞 id");
        assert_eq!(fs_read["error"]["code"], -32601);
    }

    /// prompt 请求本身失败(JSON-RPC error)→ 明确报错,不假装是正常终态。
    #[test]
    fn a_failing_prompt_request_surfaces_as_an_error() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32603,"message":"boom"}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        client.session_new(None, None).unwrap();
        client.prompt_start("sess-1", "ping").unwrap();
        let error = client
            .recv_turn_event(Some(Duration::from_secs(5)))
            .expect_err("失败的 prompt 不该被当成正常终态");
        assert!(error.to_string().contains("boom"), "实得: {error}");
    }

    /// spawn 参数:`--yolo` 与无头路一致、子命令排最后,**且不带 `--model`**。
    ///
    /// `--model` 对 acp 子命令无效(两次独立真机测量:参数收下、`currentModelId` 不变)。
    /// 留着它只会让人以为模型是 spawn 时定的 —— 那正是老实现「换模型重开进程」的错误前提。
    #[test]
    fn spawn_args_mirror_the_headless_flags_without_a_dead_model_flag() {
        assert_eq!(CursorAcpConfig::acp_args(), vec!["--yolo", "acp"]);
        assert!(
            !CursorAcpConfig::acp_args().iter().any(|a| a == "--model"),
            "spawn --model 对 acp 无效，模型必须走 parameterized config setter"
        );
    }

    /// server 的结构化拒绝必须原样上抛；不能与「回执成功但 currentValue 回退」混为一谈。
    #[test]
    fn a_rejected_set_config_option_is_reported_as_an_error() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/set_config_option'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32602,"message":"Invalid fast value"}}' ;;
"#;
        let (mut client, _tmp) = client_with(cases);
        client.session_new(None, None).unwrap();
        let error = client
            .set_config_option("sess-1", "fast", "turbo", None)
            .expect_err("-32602 必须传给调用方");
        assert!(
            error.to_string().contains("Invalid fast value"),
            "实得: {error}"
        );
    }

    /// 没有 `availableModels` 的响应(旧版 / 精简响应):候选为空,不报错、不 panic;`/model` 的
    /// 候选退回配置桶。
    #[test]
    fn a_response_without_available_models_yields_an_empty_bucket() {
        assert!(available_model_ids(&json!({})).is_empty());
        assert!(available_model_ids(&json!({"models": {}})).is_empty());
        // `id` 写法也认:协议在演进,认死一个键的代价是候选某次升级后静默变空。
        assert_eq!(
            available_model_ids(
                &json!({"models":{"availableModels":[{"id":"m1"},{"modelId":"m2"}]}})
            ),
            vec!["m1".to_string(), "m2".to_string()]
        );
    }

    #[test]
    fn permission_reply_falls_back_to_cancelled_when_no_allow_option_exists() {
        let reply = server_request_reply(
            &json!(7),
            "session/request_permission",
            &json!({"options": [{"optionId":"no","kind":"reject_once"}]}),
        );
        assert_eq!(reply.frame["result"]["outcome"]["outcome"], "cancelled");
        // 无论如何都得有一帧回去:吞 id = 整轮永久挂死。
        assert_eq!(reply.frame["id"], 7);
    }

    /// 真 Cursor 的零 token 协议探针：只握手并新建空会话，绝不发 prompt / setter。
    ///
    /// 它验证动态参数目录与配置根隔离，不验证任何模型调用。owner 配置只做元数据快照，测试
    /// 本身不会读取其中的敏感正文；cwd 也用空临时目录，避免给真实仓库触发索引。
    #[test]
    #[ignore = "需要本机已登录 cursor-agent；只做 initialize + session/new，不消耗模型 token"]
    fn real_cursor_exposes_parameterized_options_without_touching_owner_config() {
        fn metadata_snapshot(path: &std::path::Path) -> Option<(u64, std::time::SystemTime)> {
            match std::fs::metadata(path) {
                Ok(metadata) => Some((
                    metadata.len(),
                    metadata.modified().expect("应能读取 Cursor 配置元数据"),
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("读取 {} 元数据失败: {error}", path.display()),
            }
        }

        let _guard = process_test_guard();
        let owner_cursor_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("真机测试需要 HOME")
            .join(".cursor");
        let owner_paths = [
            owner_cursor_dir.clone(),
            owner_cursor_dir.join("cli-config.json"),
            owner_cursor_dir.join("acp-config.json"),
        ];
        let owner_before = owner_paths
            .iter()
            .map(|path| metadata_snapshot(path))
            .collect::<Vec<_>>();

        let isolated_config = tempfile::tempdir().unwrap();
        // 动态参数目录随当前模型变化；显式在临时配置根选择一款同时有 context/reasoning/fast
        // 的模型，使探针不依赖 owner 当前选择。只写测试目录，不调用 ACP setter。
        let parameters = json!([
            {"id": "context", "value": "272k"},
            {"id": "reasoning", "value": "medium"},
            {"id": "fast", "value": "false"}
        ]);
        std::fs::write(
            isolated_config.path().join("cli-config.json"),
            serde_json::to_vec_pretty(&json!({
                "hasChangedDefaultModel": true,
                "selectedModel": {
                    "modelId": "gpt-5.6-sol",
                    "parameters": parameters.clone(),
                },
                "modelParameters": {"gpt-5.6-sol": parameters},
            }))
            .unwrap(),
        )
        .unwrap();
        let isolated_workspace = tempfile::tempdir().unwrap();
        let mut client = CursorAcpClient::spawn(CursorAcpConfig {
            program: summon_core::tools::resolve(summon_core::tools::Tool::CursorAgent)
                .to_string_lossy()
                .into_owned(),
            args: CursorAcpConfig::acp_args(),
            work_dir: None,
            config_dir: isolated_config.path().to_path_buf(),
            request_timeout: Duration::from_secs(120),
            env: crate::runtime::daemon::essential_system_env(),
        })
        .expect("本机 cursor-agent 应能完成 initialize");
        let session = client
            .session_new(
                isolated_workspace.path().to_str(),
                Some(Duration::from_secs(120)),
            )
            .expect("session/new 不应触发模型请求");
        assert_eq!(
            session.current_model_id.as_deref(),
            Some("gpt-5.6-sol"),
            "session/new 必须读到临时 CURSOR_CONFIG_DIR 的选择"
        );

        let ids = session
            .config_options
            .iter()
            .map(|option| option.id.to_ascii_lowercase())
            .collect::<Vec<_>>();
        for required in ["model", "context", "fast"] {
            assert!(
                ids.iter().any(|id| id == required),
                "缺动态参数 {required}: {ids:?}"
            );
        }
        assert!(
            ids.iter()
                .any(|id| matches!(id.as_str(), "reasoning" | "effort" | "thinking")),
            "reasoning / effort / thinking 至少应出现一项: {ids:?}"
        );

        drop(client);
        let owner_after = owner_paths
            .iter()
            .map(|path| metadata_snapshot(path))
            .collect::<Vec<_>>();
        assert_eq!(
            owner_after, owner_before,
            "initialize + session/new 不得修改 owner ~/.cursor 的目录或配置文件元数据"
        );
    }

    /// 启动失败(未登录 / 参数不认)的诊断必须带上 stderr 尾部,否则用户只看到「断开」。
    #[test]
    fn spawn_failure_reports_the_stderr_tail() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("broken.sh");
        crate::test_support::write_test_executable(
            &script,
            "#!/bin/sh\necho 'not logged in: run cursor-agent login' >&2\nexit 1\n",
        );
        // 不用 `expect_err`:`CursorAcpClient` 没有 `Debug`(里面挂着 Child / 线程句柄),
        // 给它加一个纯为测试服务的 derive 不值当。
        let Err(error) = CursorAcpClient::spawn(CursorAcpConfig {
            program: script.to_string_lossy().into_owned(),
            args: vec!["acp".into()],
            work_dir: None,
            config_dir: tmp.path().join("cursor-config"),
            request_timeout: Duration::from_secs(3),
            env: crate::runtime::daemon::essential_system_env(),
        }) else {
            panic!("进程立刻退出应报错");
        };
        assert!(
            error.to_string().contains("not logged in"),
            "错误里必须带上 stderr 尾部,实得: {error}"
        );
    }
}
