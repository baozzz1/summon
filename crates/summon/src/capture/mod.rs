//! 捕获源(CaptureSource):per-源 把「跑一轮 + 拿到输出」抽象成一个 trait。每种源(子进程
//! stdout / 转录 tail)是一个 impl,选源决策只在 `CaptureMode` 一处发生。
//!
//! `tmux_driver` 不是 CaptureSource,而是 claude(Transcript 模式仍跑在 tmux)冷启 seed / 启动
//! 就绪 / `/compact` 等待所需的**逐 CLI 抓屏 turn-detection** 原语(无任何 marker 解析)。

use std::sync::Arc;

pub(crate) mod claude_usage_limit;
pub mod cursor_acp;
mod cursor_cli_fallback;
pub mod cursor_config;
pub(crate) mod cursor_context;
pub(crate) mod program_status;
mod program_status_pty;
pub mod rpc;
pub mod rpc_pool;
pub mod rpc_procs;
mod subprocess;
pub mod tmux_driver;
mod transcript;

pub use rpc::{RpcTurnSpec, TurnStopper};
// IO 半只在 crate 内用(chat_thread 收尾组装角标),跟着 subprocess 的可见性走。
// `Subprocess` 不再是一种「捕获模式」(2026-08-20 删),但它仍是 `summon workflow` 与
// record/replay 框架起一次性子进程的执行器 —— 那两条路与飞书轮的捕获分流无关。
pub use subprocess::Subprocess;
// codex reasoning 摘要注入值解析(chat 热路径 chat_thread.rs / run.rs 共用同一 IO 半)。
pub(crate) use subprocess::codex_reasoning_summary_arg;
pub use transcript::{SubagentTail, Transcript};
// 冷启 seed 等待复用转录 tail(`session.rs::cold_seed_wait` 调用,取代抓屏看门狗)。
pub(crate) use transcript::{
    capture_mirror_eof_fence, mirror_eof_fence_still_valid, parse_mirror_offset_record,
    resolve_seed_transcript, wait_first_turn_end, MirrorMarkerEvidence, MirrorOffsetBoundaryKind,
    MirrorOffsetRecord, MirrorTurnKind, SeedWait, TurnInjection, END_TURN_GRACE,
};
// 接管失败的三类结局:daemon 侧靠 `downcast_ref::<ReattachError>()` 分流(见 `reattach_next_step`)。
pub(crate) use transcript::ReattachError;
// 转录文件定位(mirror / subagent watcher 共用)。
pub(crate) use transcript::resolve_transcript_path;
// cmux 遥控的回传跟踪外部会话转录(`remote/relay.rs`)。
pub(crate) use transcript::tail_external_replies;
// 召回卡接管确认框列出忙碌会话还在跑的后台任务(`bridge/recall.rs`)。
pub(crate) use transcript::{pending_background_tasks, BackgroundKind, PendingBackgroundTask};
// 主 Agent 让出本轮后，后台任务的归属与存活(`runtime/background_holder.rs`)。
pub(crate) use transcript::{background_work_outstanding, HeldBackgroundTask, HeldTaskKind};

pub(crate) type CodexUserInputHandlerRef =
    Arc<dyn crate::codex::codex_app_server::CodexUserInputHandler>;
pub(crate) type CodexDynamicToolHandlerRef =
    Arc<dyn crate::codex::codex_app_server::CodexDynamicToolHandler>;
pub(crate) type CodexApprovalHandlerRef =
    Arc<dyn crate::codex::codex_app_server::CodexApprovalHandler>;

/// 一轮的输入上下文:resume id / 工作目录 / claude 系统提示文件 / run 环境注入 / model+effort。
/// 最小够用,后续按需长字段。model/effort 经 driver `resolve_launch` 落成各 CLI 的 launch 片段。
#[derive(Default)]
pub struct TurnCtx<'a> {
    /// resume:续接已有原生会话(无则冷启)。
    pub session_id: Option<&'a str>,
    /// 工作目录(子进程 `current_dir`)。
    pub work_dir: Option<&'a str>,
    /// claude `--append-system-prompt-file`:每轮注入系统提示的文件路径(仅 claude 用)。
    pub sys_prompt_file: Option<&'a str>,
    /// 叠加到子进程的环境变量(chat 路径注入 `SUMMON_AGENT`/`SUMMON_CHAT_ID` 等)。
    pub run_env: Vec<(String, String)>,
    /// agent.yaml 顶层 `model`(per-CLI 私有 token,空=CLI 默认)。
    pub model: Option<&'a str>,
    /// agent.yaml 顶层 `effort`(规范逻辑档位,空=用 CLI 默认)。
    pub effort: Option<&'a str>,
    /// codex `model_reasoning_summary`:调用方已按优先级解析好的注入值(None=不注入;仅 codex 用)。
    pub codex_reasoning_summary: Option<&'a str>,
    /// Codex Standard/Fast 的 per-chat 显式选择；None=保持该原生会话既有档位。
    pub codex_service_tier: Option<summon_core::driver::CodexServiceTier>,
    /// Cursor ACP 的 per-chat 会话模式；None=沿用 server 当前模式。
    pub cursor_mode: Option<summon_core::driver::CursorSessionMode>,
    /// codex 专用:显式允许非 Git 工作目录，映射为 `--skip-git-repo-check`。
    pub codex_skip_git_repo_check: bool,
    /// Codex app-server 原生 requestUserInput 的本轮桥。只有“由真人消息触发且 current-turn
    /// lease 成功发布”的 chat turn 才有；恢复轮/定时轮/无上下文调用保持 None 并 fail-closed。
    pub codex_user_input_handler: Option<CodexUserInputHandlerRef>,
    /// Codex app-server 动态工具的本轮桥。只在 current-turn lease 已发布时存在；模型不持有
    /// turn_id，handler 使用进程内身份执行并由 Core 再验一次活跃轮。
    pub codex_dynamic_tool_handler: Option<CodexDynamicToolHandlerRef>,
    /// Codex app-server 审批请求的本轮桥。只有 current-turn lease 存活时可用；默认
    /// `approvalPolicy=never` 时不会触发，开启交互策略后由飞书 operate-gate 裁决。
    pub codex_approval_handler: Option<CodexApprovalHandlerRef>,
}

/// 一轮的产出:本轮原生 session_id(供下轮 resume 落盘)+ 终态回复文本。
#[derive(Debug, Default)]
pub struct TurnOutcome {
    pub session_id: Option<String>,
    pub reply: Option<String>,
    /// 本轮已成功注入或并入重跑的 followup message_id。调用层只在捕获正常返回后
    /// 销 WAL；若进程在重跑中崩溃，这些 id 仍保持 `started=true`，由恢复卡接管。
    pub followup_message_ids: Vec<String>,
    /// 本轮是否被「⏸ 打断」按钮中止(任务4)。true → 调用方把过程卡定格为「已暂停」、不发完成总结。
    pub interrupted: bool,
    /// 用户以 `/new` 结束旧会话。
    pub session_ended: bool,
    /// 捕获层的静默看门狗是否按超时收口；与用户主动打断分开。
    pub timed_out: bool,
    /// Claude API 错误(model_not_found 等):捕获层从 JSONL 的 `message.error` 字段检测到,
    /// 上层据此把 TurnResult 标为 Failed(贴 ERROR 表情),而不是等 240s stall 超时。
    pub api_error: Option<String>,
    /// Claude 五小时会话额度的结构化重置时刻。与错误文案分离，避免从展示文本猜日期。
    pub claude_session_limit: Option<summon_core::claude_session_limit::ClaudeSessionLimit>,
    /// Provider 的结构化失败类别。与 `api_error` 文案分离，避免靠本地化字符串决定产品行为。
    pub api_error_kind: Option<summon_core::agent_msg::AgentErrorKind>,
    /// claude/Transcript:本轮子 agent 转录的旁路 tail(读到哪、还有谁没对上)。子 agent 比
    /// 主轮活得久时,CoT 跟随线程从这里接着读。其余捕获路恒 `None`。
    pub subagent_tail: Option<Box<SubagentTail>>,
    /// claude/Transcript:跨后台任务续跑时当场发出的段终点回复条数。其余捕获路恒 0。
    pub interim_replies: usize,
    /// 主 Agent 已结束输出，将后台等待期间到达的任务交给独立轮次。
    pub yielded_to_queue: bool,
}

/// claude/Transcript:主 Agent 已结束输出、只剩后台任务在跑的那一刻。
pub struct BackgroundIdle<'a> {
    pub session_id: &'a str,
    pub transcript: &'a std::path::Path,
    /// 本轮起的、还没收到完成通知的后台任务，按启动顺序。
    pub tasks: Vec<HeldBackgroundTask>,
}

/// 为什么重跑。两者都靠 SIGINT + 新进程实现，但 UI 必须分开：`CursorCliSteer` 是同一条
/// transport 内的续跑，应续用同一张过程卡；`NativeSteerFallback` 是退化路径，应定格旧卡另起。
/// 实现手段相同，不能因此泄漏成同一种 UI。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerunCause {
    CursorCliSteer,
    NativeSteerFallback,
}

/// 打断已完成 durable `started` 标记后，交给捕获层的重跑请求。prompt、原因与 WAL ids
/// 必须一起流转，避免正文已注入 CLI、销账身份却丢在易失局部变量里。
pub struct RerunRequest {
    pub prompt: String,
    pub message_ids: Vec<String>,
    pub cause: RerunCause,
}

/// 轮内 steer:把消息交给 claude 自己的队列,**不打断、不重置本轮**。与 `Rerun` 共用
/// 「先 durable started 再流转正文」的约束,区别只在于本轮继续跑、由 claude 在下一个工具边界
/// 一并消费。
pub struct SteerRequest {
    pub prompt: String,
    pub message_ids: Vec<String>,
}

/// turn 进行中 poll 出的打断意图(任务4)。`Stop`=控制卡「⏸ 打断」按钮纯停(不重跑);
/// `Rerun`=Cursor CLI Session 的 steer 重跑(中断当前 `-p` + `--resume` 续跑);
/// `Steer`=轮内注入,**不是打断**——本轮继续,消息挂进 claude 队列(默认关,同上)。
pub enum TurnInterrupt {
    Stop,
    SessionEnded,
    Rerun(RerunRequest),
    Steer(SteerRequest),
}

/// `on_interactive` 回报的交互中介卡投递结果。捕获层据它决定要不要在下一个探测节拍重发同一张卡:
/// 卡片是用户看见这张菜单的**唯一**途径,一次网络失败就放弃等于把整轮挂死在没人看得见的菜单上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediationCard {
    /// 卡已经在用户手里了。不做交互中介的调用方(worker / 蒸馏)也回它 —— 没有卡,也就没有可重发的东西。
    Settled,
    /// 发送失败。菜单仍在时由后续探测节拍重发。
    Undelivered,
    /// 这一拍不发:屏上的对话框已经由别的通道发了卡(Claude 授权 hook 的闸门挂着)。不记签名、
    /// 不扣重发预算、不进等待窗 —— 那条通道没答成就结束时,同一张菜单下一拍要能照常认定并发卡。
    Yielded,
}

// per-源 捕获原语:Subprocess(无头子进程 stdout)与 Transcript(claude tmux + tail jsonl)各把
// 「注入 prompt + 捕获本轮输出 + 驱动回调」实现为**inherent** 方法(`Subprocess::run_turn` /
// `Subprocess::run_turn_with_hooks` / `Transcript::run_turn`),签名按各自形态裁剪(无头要 driver+ctx,
// Transcript 只要回调)。曾有一个统一 `CaptureSource` trait,但两源签名发散、调用方又各自直调具体
// 类型(daemon 无头走 run_turn_with_hooks、run/distill 走 drive/Transcript),trait 无实际多态调用点
// → 删之,避免「为抽象而抽象」的死 trait。选源决策仍只在 `CaptureMode` 一处(daemon::stream_turn)。
