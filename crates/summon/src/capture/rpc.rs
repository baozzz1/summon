//! 原生常驻协议捕获源(codex `app-server` / cursor ACP):一轮 = 投递一个 prompt + 消费事件流到终态。
//!
//! 形态与 [`crate::capture::Subprocess`] 对齐(同一组回调、同一个 [`TurnOutcome`]),差别只有三处,
//! 每处都是「进程跨轮存活」这一个事实的推论:
//!
//! - **不 spawn**:进程从 [`RpcPool`] checkout(懒建 / 懒重连),跑完不杀 —— 下一轮直接复用。
//! - **不读 stdout 行**:输入是 typed 事件(`summon_core::codex_rpc` / `summon_core::cursor_acp`),
//!   不是 JSONL 文本。
//! - **打断分两级**:软打断(codex `turn/interrupt` / cursor `session/cancel`)只停这一轮、进程留着;
//!   硬打断 SIGINT 整组会把会话一起带走(下轮接回,~1s/~10s)。所以硬打断永远是第二选择。
//!
//! **两协议在 `AgentMessage` 之上零分叉**:各自的映射层把事件翻成 `AgentMessage`,之后一律共用
//! `subprocess::apply`(段边界 / 协议文本归并 / result 优先 / sid 与 api_error 首见即记)。两条通道对
//! 同一轮给出同一张卡,是 `runtime.capture` 逃生门能随时切的前提。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use summon_core::agent_msg::{AgentKind, AgentMessage};
use summon_core::codex_rpc::{CodexAppServerNotification, RpcTokenBreakdown};
use summon_core::cursor_acp::{acp_update_to_agent_messages, prompt_stop_to_agent_messages};

use super::cursor_acp::{AcpTurnEvent, CursorAcpError};
use super::rpc_pool::{Checkout, LockedSession, PooledClient, RpcChat, RpcPool, SoftInterrupt};
use super::{TurnCtx, TurnOutcome};
use crate::codex::codex_app_server::{CodexAppServerError, TurnOptions};
use crate::runtime::executor::ChildKiller;
use crate::runtime::rpc_stdio::RpcSteerOutcome;

/// 投递请求本身的上界。只等「server 收下了这一轮」,不等模型跑完 —— 后者由事件流的静默看门狗管
/// (见 [`TURN_SILENCE_LIMIT`])。
const TURN_START_TIMEOUT: Duration = Duration::from_secs(120);

/// 事件流的**静默**上界:连续这么久一条事件都没有才判挂死。刻意不设「一轮总时长」上界 ——
/// 长任务(跑测试、装依赖)本来就会几十分钟不吐字,按总时长砍会把正常轮误杀;而真挂死的
/// 特征恒是「彻底不出声」。
///
/// **这是引擎自己的兜底,不是生产上真正生效的那道闸**:chat 线程的无头看门狗
/// (`chat_thread::HEADLESS_STALL`,240s)对同一段静默恒先触发,并且它还能软打断、刷卡、
/// 收尾整轮。这里 600s 存在的意义只有一个 —— 引擎被单独调用(测试 / 未来别的调用点)时
/// 不会永远挂着。两个数字**刻意不对齐**:对齐了反而会变成两道同时到期的闸,谁先谁后取决于
/// 线程调度。
const TURN_SILENCE_LIMIT: Duration = Duration::from_secs(600);

/// 每次收事件的等待片。给外层留出观察静默时长的节拍,不影响吞吐(有事件就立刻返回)。
const RECV_SLICE: Duration = Duration::from_millis(200);

/// 一轮的打断把手。
///
/// 无头路只有一级(SIGINT 整组);RPC 多一级软打断,所以 chat 线程侧的 select 循环需要一个
/// 统一形态,否则「⏸ 按钮」「stall 看门狗」「steer 重跑」几处都要各自分叉。
#[derive(Clone)]
pub struct TurnStopper {
    killer: ChildKiller,
    /// None = 无头一轮(进程就是这一轮,SIGINT 即终止);Some = RPC 会话(先软打断)。
    soft: Option<SoftInterrupt>,
    /// turn 生命周期信号，既给 chat 线程的看门狗读，也给阻塞中的原生提问 waiter 读。
    waiting_for_user: Arc<AtomicBool>,
    /// 引擎侧协议活性计数:消费循环每收到一条协议通知就 +1,**包括不产生任何 TurnUpdate 的
    /// 控制面通知**(tokenUsage / rateLimits / 未映射方法)。chat 线程的静默看门狗据此把
    /// 「流活着但没有可渲染产出」与「彻底没声」区分开 —— 模型长时间生成一个大 patch 时,
    /// 渲染面可以整分钟静默,而这类通知仍在到达。计数只影响静默上界的档位
    /// (`chat_thread::headless_stall_bound`);静默计时仍只由可渲染产出喂,回收始终有界。
    liveness: Arc<AtomicU64>,
}

impl TurnStopper {
    /// 子进程停止器。CliSession / 无头一次性子进程走这条:进程就是这一轮,SIGINT 即终止。
    pub fn child(killer: ChildKiller) -> Self {
        Self {
            killer,
            soft: None,
            waiting_for_user: Arc::new(AtomicBool::new(false)),
            liveness: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn rpc(killer: ChildKiller, soft: SoftInterrupt) -> Self {
        // RPC 的 ChildKiller 属于跨轮会话进程；上一轮软打断留下的逻辑取消位必须在发布本轮
        // stopper 前清零，否则这个新 turn 的首张提问卡会被当成立即取消。
        killer.reset_waiter_cancellation();
        Self {
            killer,
            soft: Some(soft),
            waiting_for_user: Arc::new(AtomicBool::new(false)),
            liveness: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 优雅打断。RPC 先发软打断帧(codex 自己收尾、写 `<turn_aborted>` 保住上下文;cursor 回
    /// `stopReason: cancelled` —— 两边都是进程留着给下一轮),宽限内不收敛才由把手内部升级
    /// SIGINT 整组;无头路直接 SIGINT。
    ///
    /// **绝不 SIGTERM**(红线):硬终止会让下一轮接回时拿到悄悄少了一段的上下文。
    ///
    /// `Idle`(此刻没有在跑的轮)**回退到 SIGINT**,不是什么都不做。把手是在 `checkout` 之后、
    /// `turn/start` 之前就发布出去的(必须如此:进程已经在了,这中间的停机得够得着它),
    /// 于是存在一个真实窗口 —— 轮还没投出去,软打断没有作用对象。这时若一枪不发,上层
    /// (`await_interrupted_headless`)宽限一到就直接 SIGKILL:SIGINT 这一级被整个跳过,
    /// codex 写不出 `<turn_aborted>`。回退的代价只是会话被 SIGINT 收掉、下条消息懒重连
    /// (~1s / ~10s),上下文在盘上,不丢。
    pub fn interrupt(&self) {
        // 必须先发布，再进入 soft.interrupt 的最长 1s 等待：worker 可能正阻塞在飞书问题卡，
        // 唯有这枚令牌能让它撤 gate、回协议帧并使 turn/interrupt 收敛。
        self.killer.cancel_waiters();
        match &self.soft {
            Some(soft) => match soft.interrupt() {
                crate::runtime::rpc_stdio::RpcInterruptOutcome::Idle => {
                    crate::observe::log::log(summon_i18n::text!("radcf09a59b"));
                    self.killer.interrupt();
                }
                outcome => crate::observe::log::log(&summon_i18n::format!("r58a6367cb4")),
            },
            None => self.killer.interrupt(),
        }
    }

    /// 轮内 steer:把一段用户输入追加进**正在跑的这一轮**,不打断、不重跑。
    ///
    /// 无头一次性子进程那条路(`soft == None`)恒 `Unsupported` —— 它的 stdin 早已关掉,
    /// 一轮就是一个进程,没有任何「往里追加」的通路。这不是暂缺实现,是形态决定的。
    ///
    /// **返回值必须被检查**：`Accepted` 才算已交付；确定未接受可补跑，`Unconfirmed` 须报告
    /// 结果不明并停止自动重发（见 [`RpcSteerOutcome`]）。
    #[must_use]
    pub fn steer(&self, text: &str) -> RpcSteerOutcome {
        match &self.soft {
            Some(soft) => soft.steer(text),
            None => RpcSteerOutcome::Unsupported,
        }
    }

    /// 此刻有没有在跑的 RPC 轮(轮未起时打断走 Idle 回退 SIGINT,用例要先等轮起来)。
    ///
    /// `#[cfg(test)]`:同 [`SoftInterrupt::has_active_turn`] —— 生产判定看 `interrupt()` 的结果。
    #[cfg(test)]
    pub fn has_active_turn(&self) -> bool {
        self.soft
            .as_ref()
            .is_some_and(SoftInterrupt::has_active_turn)
    }

    /// 这一轮的把手**有没有** steer 能力。见 [`SoftInterrupt::can_steer`] —— 决策侧必须在
    /// durable claim 之前问这个,否则不支持的路会把消息 claim 走再发现发不出去。
    pub fn can_steer(&self) -> bool {
        self.soft.as_ref().is_some_and(SoftInterrupt::can_steer)
    }

    /// 原生 ask 正在等人。该状态只暂停“静默=挂死”的看门狗，不改变 busy/turn 生命周期。
    pub fn is_waiting_for_user(&self) -> bool {
        self.waiting_for_user.load(Ordering::Acquire)
    }

    /// 引擎侧协议活性计数的当前值(语义见 `liveness` 字段)。读侧只比较「变没变」,
    /// 绝对值没有含义。
    pub fn liveness_beats(&self) -> u64 {
        self.liveness.load(Ordering::Acquire)
    }

    /// 消费循环收到一条本轮消费的协议通知。轮归属由轮前 drain 与 codex 侧的隔离闸
    /// 尽力保证 —— 不带轮标识的控制面通知天然无从校验。[`SilenceWatchdog::beat`] 喂
    /// 引擎自己的兜底,这个喂 chat 线程的生产闸(`chat_thread::HEADLESS_STALL`)。
    fn beat_liveness(&self) {
        self.liveness.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn cancellation_flag(&self) -> &AtomicBool {
        self.killer.cancellation_flag()
    }

    pub(crate) fn waiting_for_user_flag(&self) -> &AtomicBool {
        &self.waiting_for_user
    }

    /// 本轮进程归属,交给 `InFlight` 的拒绝路径判断能不能就地收掉这个进程组。
    /// RPC 轮借用的是跨轮存活的会话进程,无头轮独占自己那一个。
    pub fn shape(&self) -> crate::runtime::inflight::TurnShape {
        match self.soft {
            Some(_) => crate::runtime::inflight::TurnShape::SharedSession,
            None => crate::runtime::inflight::TurnShape::OwnedProcess,
        }
    }

    /// 最后一级:SIGKILL 整组。RPC 走到这里等于把会话进程也杀了,下条消息懒重连接回上下文。
    pub fn kill(&self) {
        self.killer.kill();
    }

    /// 硬 killer 本体:`InFlight` 登记要的就是它(那张表的语义是「可 kill 的子进程组」,
    /// 停机只认信号,没有软打断的位置)。
    pub fn killer(&self) -> &ChildKiller {
        &self.killer
    }
}

/// 一轮的身份 + 池句柄。字段与 [`RpcChat`] 对齐,但都是 owned —— 这一轮要在 worker 线程里跑。
pub struct RpcTurnSpec {
    pub agent: summon_core::registry::Agent,
    pub session: String,
    pub tag: String,
    /// 本 chat 的飞书 chat_id。子进程身份三件套之一,必须由**调用方**给准 —— 从盘上反查是
    /// best-effort,失败会让子进程拿到半份身份(见 `RpcPool::child_env`)。
    pub chat_id: String,
}

/// 跑一轮。回调语义与 `Subprocess::run_turn_with_hooks` 逐条相同:
/// `on_spawn` 在拿到会话(可打断)那一刻发布把手。Codex 的 `on_session_id` 在
/// checkout 后立即回传；Cursor 必须等首条 `session/prompt` 帧成功写出才回传——否则
/// prompt 前失败会把一个从未收到 PRIMER 的空 SID 落盘，新 daemon load 它后将永久漏注身份。
#[allow(clippy::too_many_arguments)] // 各回调职责独立(消息/工具/流式/全量事件 + 把手/sid),同 drive 的取舍
pub fn run_turn(
    pool: &RpcPool,
    spec: &RpcTurnSpec,
    prompt: &str,
    primer: Option<&str>,
    ctx: &TurnCtx,
    on_message: &mut dyn FnMut(&str),
    on_stream: &mut dyn FnMut(&str),
    on_tool: &mut dyn FnMut(&str),
    on_event: &mut dyn FnMut(&AgentMessage),
    on_spawn: &mut dyn FnMut(&TurnStopper),
    on_session_id: &mut dyn FnMut(&str),
) -> anyhow::Result<TurnOutcome> {
    // 权限策略在**这里**解析(而不是池里):池不该读全局配置 —— 它拿不到 `Config`,
    // 而且从池里读会让池的单测跟着开发机上的 `~/.summon/config.yaml` 走,失去自洽。
    // 非法值已在 `codex_policy::resolve` 里降级,这里只负责把被拒的值写进日志:
    // 静默降级会让「我明明配了」查不下去。
    let (codex_policy, fallbacks) = spec
        .agent
        .codex_policy(Some(&summon_core::config::Config::load().codex));
    for fallback in &fallbacks {
        crate::observe::log::log(&summon_i18n::format!(
            "r7c26dff308",
            spec.agent.name,
            fallback.field,
            fallback.rejected
        ));
    }
    if let Some(warning) = codex_policy.approval_warning() {
        crate::observe::log::log(&format!("[{}] {warning}", spec.agent.name));
    }
    if let Some(warning) = codex_policy.write_blocked_warning() {
        crate::observe::log::log(&format!("[{}] {warning}", spec.agent.name));
    }
    let client = pool.checkout(&RpcChat {
        agent: &spec.agent,
        session: &spec.session,
        tag: &spec.tag,
        chat_id: &spec.chat_id,
        cwd: ctx.work_dir,
        primer,
        model: rpc_model_for(&spec.agent, ctx.model),
        service_tier: ctx.codex_service_tier,
        cursor_mode: ctx.cursor_mode.as_ref(),
        codex_policy,
        register_platform_tools: ctx.codex_dynamic_tool_handler.is_some(),
    })?;
    // 把手要**紧挨着 checkout** 发布:进程已经在了,这中间任何一刻的停机/打断都得够得着它。
    let stopper = TurnStopper::rpc(client.killer(), client.interrupt_handle());
    on_spawn(&stopper);
    let cursor = summon_core::driver::CliDriver::from_kind(spec.agent.cli_kind())
        == summon_core::driver::CliDriver::Cursor;
    if !cursor {
        on_session_id(client.session_id());
    }
    // 「懒重连真的走了 resume」在生产里也要可证:热路径(Reused)不刷日志,建/接会话才记一条。
    if client.checkout_mode() != Checkout::Reused {
        crate::observe::log::log(&summon_i18n::format!(
            "re6f5b2f905",
            spec.agent.name,
            spec.agent.cli_kind(),
            client.checkout_mode(),
            client.session_id(),
            client.pgid()
        ));
    }
    let prompt = compose_prompt(&spec.agent, client.cursor_primer_pending(), primer, prompt);
    consume_turn(
        pool,
        &client,
        &spec.agent.name,
        &spec.tag,
        &prompt,
        ctx,
        &stopper,
        on_message,
        on_stream,
        on_tool,
        on_event,
        on_session_id,
    )
}

/// 本轮要用的模型 —— **按通道取,不混写**。
///
/// codex 用 `ctx.model`(agent.yaml 顶层 `model`,每轮经 `turn/start` 下发,与 exec 路同一套值)。
/// Cursor 的长期事实源是 per-agent 参数 bundle；这里的 `runtime.rpc_model` 只作为 bundle
/// 尚不存在时的一次性 legacy seed。池在 prompt 前会把 seed/bundle 映射到 parameterized
/// model + configOptions，绝不再把 bracket id 直接交给 server。
fn rpc_model_for<'a>(
    agent: &'a summon_core::registry::Agent,
    ctx_model: Option<&'a str>,
) -> Option<&'a str> {
    match summon_core::driver::CliDriver::from_kind(agent.cli_kind()) {
        summon_core::driver::CliDriver::Cursor => agent.rpc_model(),
        _ => ctx_model,
    }
}

/// 本轮真正喂给 server 的首段文本 —— 也就是 **cursor 的 PRIMER 落点**。
///
/// 两条协议的 PRIMER 位置本就不同(codex 走 `developerInstructions`,由池在 `thread/start`
/// 与每次 `thread/resume` 下发;ACP 的 `session/new` 只收 `{cwd, mcpServers}`,没有同类
/// 字段,只能前置进首条用户消息)。**关键是判据**:注不注必须看这条 session 的首个 prompt
/// 是否已经成功写出，不能只看本次 checkout 的 ColdStart/Reused 标签或盘上有没有 sid。
///
/// 为什么:`session/load` 会失败(聊天记录被删 / id 不存在),池此时退回 `session/new` 开一个
/// **全新**会话 —— 而盘上明明还有 sid。另一个窗口是 ColdStart 已建好、参数收敛却在 prompt
/// 前失败；下一次标签是 Reused，但首条消息仍没发生。两种都由池内 pending 位覆盖，直到
/// `prompt_start` 真正成功才消费。
fn compose_prompt(
    agent: &summon_core::registry::Agent,
    cursor_primer_pending: bool,
    primer: Option<&str>,
    prompt: &str,
) -> String {
    let cursor = summon_core::driver::CliDriver::from_kind(agent.cli_kind())
        == summon_core::driver::CliDriver::Cursor;
    let primer = primer.map(str::trim).filter(|p| !p.is_empty());
    match (cursor, cursor_primer_pending, primer) {
        // 与 `CliDriver::compose_first_prompt` 逐字同一种拼法(两个换行),不另起格式。
        (true, true, Some(primer)) => format!("{primer}\n\n{prompt}"),
        _ => prompt.to_string(),
    }
}

/// 一轮的累积态。**两条协议共用同一份**:段边界 / 去重 / result 优先的规则只有一处实现,
/// 抄第二份必漂(那正是 `codex_rpc` / `cursor_acp` 的一致性断言在防的事,只不过那边钉的是
/// 「事件序列」,这边钉的是「事件怎么折成卡」)。
struct TurnAccum {
    outcome: TurnOutcome,
    accumulated_assistant: Vec<String>,
    stream_buf: String,
    result_text: Option<String>,
}

impl TurnAccum {
    fn new(session_id: &str) -> Self {
        Self {
            // 会话 id 就是本轮的 resume id(sid)。无头路要等 CLI 自己吐一行才知道,
            // 这里开会话时就有了。
            outcome: TurnOutcome {
                session_id: Some(session_id.to_string()),
                ..Default::default()
            },
            accumulated_assistant: Vec::new(),
            stream_buf: String::new(),
            result_text: None,
        }
    }

    fn observe_main_notification(&mut self, notification: &CodexAppServerNotification) {
        if let Some(text) = summon_core::codex_rpc::final_answer_text(notification) {
            self.result_text = Some(text.to_string());
        }
        if let CodexAppServerNotification::TurnCompleted { turn, .. } = notification {
            match turn.status.as_str() {
                "completed" => {}
                "interrupted" | "aborted" => self.outcome.interrupted = true,
                status => {
                    // 重置会清除上下文，只有 provider 明确成功的终态才授予该权限。
                    self.outcome
                        .api_error
                        .get_or_insert_with(|| format!("Codex turn ended with status: {status}"));
                }
            }
        }
    }

    fn apply(
        &mut self,
        msg: &AgentMessage,
        on_tool: &mut dyn FnMut(&str),
        on_stream: &mut dyn FnMut(&str),
        on_event: &mut dyn FnMut(&AgentMessage),
    ) {
        super::subprocess::apply(
            msg,
            super::subprocess::AssistantTextMode::Delta,
            &mut self.outcome,
            &mut self.accumulated_assistant,
            &mut self.stream_buf,
            &mut self.result_text,
            on_tool,
            on_stream,
            on_event,
        );
        if self
            .outcome
            .api_error
            .as_ref()
            .is_some_and(|error| error.trim().is_empty())
        {
            self.outcome.api_error = None;
        }
    }

    /// 与 `Subprocess::drive` 逐条同规则的收尾:未收尾的 delta 先提交(不丢字),
    /// 终态回复优先 result、退化为累积正文。
    fn finish(mut self, on_message: &mut dyn FnMut(&str)) -> TurnOutcome {
        if !self.stream_buf.is_empty() {
            self.accumulated_assistant
                .push(std::mem::take(&mut self.stream_buf));
        }
        let last_assistant = if self.accumulated_assistant.is_empty() {
            None
        } else {
            Some(self.accumulated_assistant.join("\n\n"))
        };
        self.outcome.reply = self.result_text.or(last_assistant);
        if let Some(reply) = self.outcome.reply.as_deref() {
            if !reply.trim().is_empty() {
                on_message(reply);
            }
        }
        self.outcome
    }
}

/// 一轮的静默看门狗。两条消费循环共用:超上界即判挂死(见 [`TURN_SILENCE_LIMIT`])。
struct SilenceWatchdog(Instant);

impl SilenceWatchdog {
    fn new() -> Self {
        Self(Instant::now())
    }

    fn beat(&mut self) {
        self.0 = Instant::now();
    }

    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.0.elapsed() < TURN_SILENCE_LIMIT,
            summon_i18n::format!("r49dd2d334d", TURN_SILENCE_LIMIT.as_secs())
        );
        Ok(())
    }
}

/// 独占取用 client 跑完一轮。**消费权单线程独占**(见 `rpc_pool` 模块头):投递与整段流式消费
/// 都在这一把锁里,打断走另一条通路。协议分派只在这一处发生。
#[allow(clippy::too_many_arguments)]
fn consume_turn(
    pool: &RpcPool,
    client: &PooledClient,
    agent: &str,
    tag: &str,
    prompt: &str,
    ctx: &TurnCtx,
    stopper: &TurnStopper,
    on_message: &mut dyn FnMut(&str),
    on_stream: &mut dyn FnMut(&str),
    on_tool: &mut dyn FnMut(&str),
    on_event: &mut dyn FnMut(&AgentMessage),
    on_session_id: &mut dyn FnMut(&str),
) -> anyhow::Result<TurnOutcome> {
    let session_id = client.session_id().to_string();
    let mut accum = TurnAccum::new(&session_id);
    match client.lock() {
        LockedSession::Codex(mut guard) => consume_codex_turn(
            pool,
            &mut guard,
            client,
            agent,
            tag,
            &session_id,
            prompt,
            ctx,
            stopper,
            &mut accum,
            on_stream,
            on_tool,
            on_event,
        )?,
        LockedSession::Cursor(mut guard) => consume_cursor_turn(
            pool,
            &mut guard,
            client,
            agent,
            tag,
            &session_id,
            prompt,
            stopper,
            &mut accum,
            on_stream,
            on_tool,
            on_event,
            on_session_id,
        )?,
    }
    Ok(accum.finish(on_message))
}

#[allow(clippy::too_many_arguments)]
fn consume_codex_turn(
    pool: &RpcPool,
    guard: &mut crate::codex::codex_app_server::CodexAppServerClient,
    client: &PooledClient,
    agent: &str,
    tag: &str,
    thread_id: &str,
    prompt: &str,
    ctx: &TurnCtx,
    stopper: &TurnStopper,
    accum: &mut TurnAccum,
    on_stream: &mut dyn FnMut(&str),
    on_tool: &mut dyn FnMut(&str),
    on_event: &mut dyn FnMut(&AgentMessage),
) -> anyhow::Result<()> {
    // per-turn 覆盖:model/effort/summary 每轮下发。**不能在建 thread 时钉一次** —— `thread/resume`
    // 绝不许带模型字段(§5.1),懒重连之后 `turn/start` 是唯一还能表达「用哪个模型」的地方。
    // effort 先过 driver 的合法集:非法值丢掉降级(不 panic、不阻断本轮),同 launch 路的分层。
    // 这里硬编码 Codex 是对的:traex 走同一份 codex app-server 协议,且 EFFORTS 集在 driver.rs
    // 里就是 traex 复用 codex::legal_efforts(fork 同构),两 CLI 的合法 effort 逐字节相同。
    let effort = ctx.effort.filter(|e| {
        summon_core::driver::CliDriver::Codex
            .resolve_launch(None, Some(e))
            .is_ok()
    });
    let options = TurnOptions {
        model: ctx.model,
        effort,
        summary: ctx.codex_reasoning_summary,
        service_tier: ctx.codex_service_tier,
        ..Default::default()
    };
    // 轮开始前先把队里的残留清掉:client 跨轮存活,上一轮的尾巴(终态之后仍在飞的 delta、
    // 开会话请求期间排进来的东西)会被下一轮原样弹出来当成自己的内容。见
    // `CodexAppServerClient::drain_queued_notifications`。**必须在 `turn/start` 之前** ——
    // 那之后排进队里的就是本轮自己的事件了。
    let (stale, stale_notifications) = guard.drain_queued_notifications();
    for notification in stale_notifications {
        record_codex_quota_notification(agent, tag, &notification);
    }
    if stale > 0 {
        crate::observe::log::log(&summon_i18n::format!("r9e143c2ac3"));
    }

    let turn = guard
        .turn_start(thread_id, prompt, &options, Some(TURN_START_TIMEOUT))
        .context(summon_i18n::text!("reb0a7d659a"))?;

    // usage 的跨通知 join:`turn/completed` 不带 usage,它由 `thread/tokenUsage/updated` 单独送达
    // (纯映射函数不跨条持状态,所以这个状态只能在引擎里)。记最后一份 `last`(这一轮的口径),
    // 终态时挂到 Result 事件上,与 exec 的 `turn.completed.usage` 对齐。
    let mut last_usage: Option<RpcTokenBreakdown> = None;
    let mut silence = SilenceWatchdog::new();
    // 未映射通知的日志去重键(方法名)。**必须去重**:`configWarning` 这类一轮里会重复来,
    // 而 `contextCompaction` 之外还有一批高频的已被 `notable_notice` 挡在外面。
    // 一轮一份:跨轮重新提示是对的(那是新一轮的新情况),轮内刷屏不是。
    let mut noticed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut subagents = SubagentThreads::default();
    let mut requests = RequestTally::new(&turn.id);
    // 记进运行记录:主线程与本轮派出的子线程的每次模型请求。子线程用的模型报不上来,按本轮的记。
    // codex 的 `fast` 档在 OpenAI 那边就是 priority 处理。
    let variant = matches!(
        ctx.codex_service_tier,
        Some(summon_core::driver::CodexServiceTier::Fast)
    )
    .then_some("priority");
    let mut note_request = |notification: &CodexAppServerNotification| {
        let model = pool
            .current_model(agent, tag)
            .or_else(|| ctx.model.map(str::to_string));
        if let Some(usage) = requests.observe(notification, model, variant) {
            crate::runtime::turn_ledger::note(agent, tag, |facts| facts.absorb_request(&usage));
        }
    };

    loop {
        let next = match (
            ctx.codex_user_input_handler.as_deref(),
            ctx.codex_dynamic_tool_handler.as_deref(),
            ctx.codex_approval_handler.as_deref(),
        ) {
            (None, None, None) => guard.recv_notification(Some(RECV_SLICE)),
            (user_input_handler, dynamic_tool_handler, approval_handler) => guard
                .recv_notification_with_handlers(
                    Some(RECV_SLICE),
                    thread_id,
                    &turn.id,
                    user_input_handler,
                    dynamic_tool_handler,
                    approval_handler,
                    stopper.cancellation_flag(),
                    stopper.waiting_for_user_flag(),
                ),
        };
        let notification = match next {
            Ok(notification) => notification,
            Err(CodexAppServerError::Timeout { .. }) => {
                silence.check()?;
                continue;
            }
            // 进程没了(EOF / 被 SIGINT 收掉 / 崩)或协议对不上。**已出的内容照常收尾**
            //(不丢字,下一轮 checkout 会 `thread/resume` 接回),但这一轮必须记成失败:
            // 见 [`fatal_stream_error`]。
            Err(error) => {
                log_stream_break(client, &format!("codex app-server: {error}"));
                fatal_stream_error(
                    accum,
                    &summon_i18n::format!("rc53af87a19"),
                    dead_session_stderr(client, || guard.stderr_tail()),
                    on_tool,
                    on_stream,
                    on_event,
                );
                break;
            }
        };
        silence.beat();
        // 子 agent 线程的通知走自己的路:不进本轮的隔离闸、活性计数、usage、回复累积,只盖上
        // 子 agent 标识交给 `on_event`,由过程展示放进它自己的一段。
        match subagents.route(&notification, thread_id) {
            SubagentRoute::Main => {}
            SubagentRoute::Child(child) => {
                note_request(&notification);
                forward_subagent_notification(&notification, &child, on_event);
                // 子线程也能派子 agent:孙线程同样登记,作为独立的一段。
                if let Some(grandchild) = spawned_subagent_thread(&notification) {
                    subagents.register_and_forward(grandchild, &mut note_request, on_event);
                }
                continue;
            }
            SubagentRoute::Held => continue,
        }
        // **跨轮隔离闸**:池复用同一个进程,上一轮的尾随事件仍可能在路上。带轮标识且不是本轮的
        // 一律丢 —— 混进来的表现是「这一轮的卡上多出上一轮的半句话 / usage 是上一轮的口径」,
        // 不报错、不丢回复,只是内容归错了轮。不带轮标识的控制面通知(rateLimits 等)本就无从
        // 归属,照旧放行。
        if !belongs_to_turn(&notification, &turn.id) {
            crate::observe::log::log(&summon_i18n::format!("r7296267e65", turn.id));
            continue;
        }
        // 活性计数在隔离闸之后敲。闸挡的是带 turnId 且不属本轮的通知 —— 生产上主要是
        // 并发 sub-agent 流量(单轮可达数千条),别的 turn 的心跳证明不了本轮的流活着;
        // 不带轮标识的控制面通知无从归属、照旧放行,跨轮污染的防线是轮前
        // `drain_queued_notifications`。已知覆盖边界:静默窗口内若只有外来 turnId 的
        // 通知到达,本轮计数不前进,240s 档照旧生效 —— 全 sub-agent 流量的长静默不在
        // 抬档覆盖内。
        stopper.beat_liveness();
        note_request(&notification);
        if let CodexAppServerNotification::TokenUsage {
            last,
            model_context_window,
            ..
        } = &notification
        {
            match (last, model_context_window.filter(|window| *window > 0)) {
                (Some(breakdown), Some(window)) => {
                    last_usage = Some(*breakdown);
                    // 同步记到跨轮存活的会话上,供收尾角标读取。**取 `last.input_tokens`**:
                    // 那一轮送进模型的输入量就是此刻的上下文长度;`total_*` 是整个 thread 的
                    // 累计,拿它会越报越离谱。
                    pool.record_ctx_budget(agent, tag, breakdown.input_tokens, window);
                }
                (Some(breakdown), None) => {
                    last_usage = Some(*breakdown);
                    pool.clear_ctx_budget(agent, tag);
                }
                (None, _) => {
                    pool.clear_ctx_budget(agent, tag);
                }
            }
        }
        // 额度推送不带轮标识、也不保证每轮都来(额度没变就不推),所以**不挂在这一轮上**,
        // 而是记到跨轮存活的会话里 —— 否则角标会在「这轮推了/没推」之间闪烁。
        if let Some(left) = record_codex_quota_notification(agent, tag, &notification) {
            // 只在值变了才记:这条通知可能每轮都来,而额度是慢变量。**这条日志同时是
            // 「推送到底来没来」的唯一证据** —— 角标上不显示时,要能分清是没推还是没接上。
            crate::observe::log::log(&summon_i18n::format!("r4522486ca7"));
        }
        // 未映射但值得知道的通知:日志便于当场排障；纯映射层同时把同一内容变成 system 事件，
        // 供 events.jsonl / replay 审计。system 不进入助手正文与卡片段。
        let mut duplicate_notice = false;
        if let CodexAppServerNotification::Other { method, .. } = &notification {
            if let Some(notice) = summon_core::codex_rpc::notable_notice(&notification) {
                // 去重键带上正文:`warning` 的 method 恒为 "warning" 而每条内容不同,只按 method 去重
                // 会把第二条起的告警整个吞掉 —— 而那正是 codex 判定「该让人看到」的那类。
                if noticed.insert(format!("{method}\u{1f}{notice}")) {
                    crate::observe::log::log(&format!("[{agent}] {notice}(thread={thread_id})"));
                } else {
                    duplicate_notice = true;
                }
            }
        }
        let terminal = matches!(
            &notification,
            CodexAppServerNotification::TurnCompleted { turn: t, .. } if t.id == turn.id
        );
        let messages = if duplicate_notice {
            Vec::new()
        } else {
            summon_core::codex_rpc::rpc_notification_to_agent_messages(&notification)
        };
        for mut msg in messages {
            // 终态事件带上 join 出来的 usage + sid,形态与 `parse_codex_line` 的 `turn.completed` 一致。
            if msg.kind == AgentKind::Result {
                msg.usage = last_usage.map(RpcTokenBreakdown::to_usage);
                msg.session_id = Some(thread_id.to_string());
            }
            accum.apply(&msg, on_tool, on_stream, on_event);
        }
        accum.observe_main_notification(&notification);
        if let Some(child) = spawned_subagent_thread(&notification) {
            subagents.register_and_forward(child, &mut note_request, on_event);
        }
        if terminal {
            break;
        }
    }
    Ok(())
}

/// 本轮派出的子 agent 线程(含子线程再派出的孙线程)。登记来自 `subAgentActivity(kind=started)`;
/// 子线程的通知可能先于登记到达(真机:子线程的 `thread/status/changed` 就比它早),没登记的线程
/// 先按线程攒着,登记后补投。攒的量有条数与字节两道上限:永远等不到登记的线程(不是本轮派的)
/// 不能把内存撑大,而一次命令输出就能有几 MB。
#[derive(Default)]
struct SubagentThreads {
    children: std::collections::HashSet<String>,
    /// 按线程攒着的通知与它的估算字节数。
    held: std::collections::HashMap<String, Vec<(CodexAppServerNotification, usize)>>,
    held_count: usize,
    held_bytes: usize,
    /// 攒超额的线程:之后的通知一律丢,不在攒下的序列里留空洞。
    overflowed: std::collections::HashSet<String>,
}

enum SubagentRoute {
    Main,
    Child(String),
    /// 攒着等登记,或超额丢弃。
    Held,
}

impl SubagentThreads {
    const MAX_HELD: usize = 512;
    const MAX_HELD_BYTES: usize = 1 << 20;

    fn route(
        &mut self,
        notification: &CodexAppServerNotification,
        main_thread: &str,
    ) -> SubagentRoute {
        let Some(thread) = notification_thread_id(notification).filter(|t| *t != main_thread)
        else {
            return SubagentRoute::Main;
        };
        if self.children.contains(thread) {
            return SubagentRoute::Child(thread.to_string());
        }
        if self.overflowed.contains(thread) {
            return SubagentRoute::Held;
        }
        let bytes = approx_notification_bytes(notification);
        if self.held_count >= Self::MAX_HELD || self.held_bytes + bytes > Self::MAX_HELD_BYTES {
            self.overflowed.insert(thread.to_string());
            crate::observe::log::log(&summon_i18n::format!("rf9f284466d"));
            return SubagentRoute::Held;
        }
        self.held_count += 1;
        self.held_bytes += bytes;
        self.held
            .entry(thread.to_string())
            .or_default()
            .push((notification.clone(), bytes));
        SubagentRoute::Held
    }

    fn register(&mut self, thread: &str) -> Vec<CodexAppServerNotification> {
        if !self.children.insert(thread.to_string()) {
            return Vec::new();
        }
        let held = self.held.remove(thread).unwrap_or_default();
        self.held_count -= held.len();
        self.held_bytes -= held.iter().map(|(_, bytes)| bytes).sum::<usize>();
        held.into_iter()
            .map(|(notification, _)| notification)
            .collect()
    }

    /// 登记一个子 agent 线程并补投它攒着的通知。补投的通知里若有它再派出的线程,一并登记。
    /// 攒着的用量推送同样交给 `on_usage` 记账。
    fn register_and_forward(
        &mut self,
        thread: &str,
        on_usage: &mut dyn FnMut(&CodexAppServerNotification),
        on_event: &mut dyn FnMut(&AgentMessage),
    ) {
        let mut pending = vec![thread.to_string()];
        while let Some(thread) = pending.pop() {
            for held in self.register(&thread) {
                on_usage(&held);
                forward_subagent_notification(&held, &thread, on_event);
                if let Some(grandchild) = spawned_subagent_thread(&held) {
                    pending.push(grandchild.to_string());
                }
            }
        }
    }
}

/// 本轮各线程的模型请求。`thread/tokenUsage/updated` 每次请求后推一次:`last` 是这次请求的用量,
/// `total` 是线程累计。同一份用量可能重复推送:累计与该线程上一条相同的不计。累计也可能回落
/// (压缩上下文之后),所以只比上一条、不比历史最大值;没带累计的旧版本每条都计。
pub(crate) struct RequestTally {
    /// 这一次 `turn/start` 的 turn id。同一行运行记录可能跨好几次 `turn/start`(轮内追加消息降级
    /// 重跑),各次的计数都从 1 起,请求 id 靠它区分。
    turn_id: String,
    last_total: std::collections::HashMap<String, u64>,
    /// 计过的请求数:去重已在这里做完,请求 id 只需在这一次 `turn/start` 里唯一。
    counted: u32,
}

impl RequestTally {
    pub(crate) fn new(turn_id: &str) -> Self {
        Self {
            turn_id: turn_id.to_string(),
            last_total: std::collections::HashMap::new(),
            counted: 0,
        }
    }

    pub(crate) fn observe(
        &mut self,
        notification: &CodexAppServerNotification,
        model: Option<String>,
        variant: Option<&str>,
    ) -> Option<summon_core::agent_msg::RequestUsage> {
        let CodexAppServerNotification::TokenUsage {
            thread_id,
            total_tokens,
            last: Some(last),
            ..
        } = notification
        else {
            return None;
        };
        if last.total_tokens == 0 {
            return None;
        }
        if let Some(total) = total_tokens {
            if self.last_total.insert(thread_id.clone(), *total) == Some(*total) {
                return None;
            }
        }
        self.counted += 1;
        let id = format!("{thread_id}:{}#{}", self.turn_id, self.counted);
        Some(last.to_request_usage(id, model, variant.map(str::to_string)))
    }
}

/// 攒着的通知大约占多少字节。只求量级:大头是正文增量与 item 里的命令输出。
fn approx_notification_bytes(notification: &CodexAppServerNotification) -> usize {
    const OVERHEAD: usize = 64;
    OVERHEAD
        + match notification {
            CodexAppServerNotification::AgentMessageDelta { delta, .. }
            | CodexAppServerNotification::ReasoningDelta { delta, .. } => delta.len(),
            CodexAppServerNotification::ItemStarted { item, .. }
            | CodexAppServerNotification::ItemCompleted { item, .. } => item.to_string().len(),
            CodexAppServerNotification::Other { params, .. } => params.to_string().len(),
            _ => 0,
        }
}

/// `subAgentActivity(kind=started)` 派出的线程 id。
fn spawned_subagent_thread(notification: &CodexAppServerNotification) -> Option<&str> {
    let (CodexAppServerNotification::ItemStarted { item, .. }
    | CodexAppServerNotification::ItemCompleted { item, .. }) = notification
    else {
        return None;
    };
    if item.get("type").and_then(serde_json::Value::as_str) != Some("subAgentActivity")
        || item.get("kind").and_then(serde_json::Value::as_str) != Some("started")
    {
        return None;
    }
    item.get("agentThreadId")
        .and_then(serde_json::Value::as_str)
        .filter(|thread| !thread.is_empty())
}

pub(crate) fn forward_subagent_notification(
    notification: &CodexAppServerNotification,
    child: &str,
    on_event: &mut dyn FnMut(&AgentMessage),
) {
    for mut msg in summon_core::codex_rpc::rpc_notification_to_agent_messages(notification) {
        msg.parent_run_id = Some(summon_core::agent_msg::RunId(child.to_string()));
        if let CodexAppServerNotification::TurnCompleted { turn, .. } = notification {
            use summon_core::agent_msg::TurnStatus;
            msg.status = match turn.status.as_str() {
                "completed" => Some(TurnStatus::Completed),
                "failed" => Some(TurnStatus::Failed),
                "interrupted" => Some(TurnStatus::Aborted),
                _ => None,
            };
        }
        // 子线程的用量与会话 id 属于它自己,不能被当成这个会话的。
        msg.usage = None;
        msg.session_id = None;
        on_event(&msg);
    }
}

fn notification_thread_id(notification: &CodexAppServerNotification) -> Option<&str> {
    match notification {
        CodexAppServerNotification::TurnCompleted { thread_id, .. }
        | CodexAppServerNotification::TurnStarted { thread_id, .. }
        | CodexAppServerNotification::Error { thread_id, .. }
        | CodexAppServerNotification::AgentMessageDelta { thread_id, .. }
        | CodexAppServerNotification::ReasoningDelta { thread_id, .. }
        | CodexAppServerNotification::PlanUpdated { thread_id, .. }
        | CodexAppServerNotification::ItemStarted { thread_id, .. }
        | CodexAppServerNotification::ItemCompleted { thread_id, .. }
        | CodexAppServerNotification::TokenUsage { thread_id, .. } => Some(thread_id),
        CodexAppServerNotification::RateLimitsUpdated(_) => None,
        CodexAppServerNotification::Other { params, .. } => {
            params.get("threadId").and_then(serde_json::Value::as_str)
        }
    }
}

fn record_codex_quota_notification(
    agent: &str,
    tag: &str,
    notification: &CodexAppServerNotification,
) -> Option<u8> {
    let CodexAppServerNotification::RateLimitsUpdated(snapshot) = notification else {
        return None;
    };
    let (left, _) = snapshot.weekly_left_pct_and_reset_at()?;
    crate::capture::rpc_pool::record_codex_quota_snapshot_shared(Some((agent, tag)), snapshot)
        .then_some(left)
}

/// 这条通知属不属于 `turn_id` 这一轮。
///
/// `None` 那一档(通知本身不带轮标识)一律**放行**:`account/rateLimits/updated`、未知方法这类
/// 控制面通知本就不属于任何一轮,按「不是本轮」丢掉会把协议演进容忍位一起丢了。
/// `TokenUsage.turnId` 是 optional 的,给了就校验、没给就只能认 —— 那是唯一的 usage 来源。
fn belongs_to_turn(notification: &CodexAppServerNotification, turn_id: &str) -> bool {
    notification_turn_id(notification).is_none_or(|id| id == turn_id)
}

fn notification_turn_id(notification: &CodexAppServerNotification) -> Option<&str> {
    match notification {
        CodexAppServerNotification::TurnCompleted { turn, .. }
        | CodexAppServerNotification::TurnStarted { turn, .. } => Some(&turn.id),
        CodexAppServerNotification::AgentMessageDelta { turn_id, .. }
        | CodexAppServerNotification::ReasoningDelta { turn_id, .. }
        | CodexAppServerNotification::PlanUpdated { turn_id, .. }
        | CodexAppServerNotification::ItemStarted { turn_id, .. }
        | CodexAppServerNotification::ItemCompleted { turn_id, .. }
        | CodexAppServerNotification::Error { turn_id, .. } => Some(turn_id),
        CodexAppServerNotification::TokenUsage { turn_id, .. } => turn_id.as_deref(),
        CodexAppServerNotification::RateLimitsUpdated(_) => None,
        CodexAppServerNotification::Other { params, .. } => {
            params.get("turnId").and_then(serde_json::Value::as_str)
        }
    }
}

/// 流被非超时错误打断:把原因记成本轮的 `api_error`,而不是当正常完成。
///
/// 为什么不能「已出的字交出去就算了」:进程死 / 协议错和「模型说完了」在卡上长得**一模一样**
/// (都是 DONE 表情 + 一段正文),用户看不出这一轮其实断了 —— 那正是「编译通过、语义静默归错边」
/// 的失败。exec 路对同一类事实是 `bail!` + stderr 尾部,两条通道必须给同一张卡。
///
/// 走 `AgentKind::Error` 这条**既有**通路(`subprocess::apply` 的「api_error 首见即记」),
/// 与 codex `error{willRetry:false}` 通知落在同一处,不另开一条平行的错误面。
fn fatal_stream_error(
    accum: &mut TurnAccum,
    detail: &str,
    stderr_tail: Option<String>,
    on_tool: &mut dyn FnMut(&str),
    on_stream: &mut dyn FnMut(&str),
    on_event: &mut dyn FnMut(&AgentMessage),
) {
    let mut msg = AgentMessage::of(AgentKind::Error);
    msg.error = Some(match stderr_tail {
        // 客户端的 `ProcessFailed` 自己就带尾部,别再贴第二遍。
        Some(tail) if !detail.contains(tail.as_str()) => {
            summon_i18n::format!("rad7b587d11")
        }
        _ => detail.to_string(),
    });
    accum.apply(&msg, on_tool, on_stream, on_event);
}

/// 只在会话进程**确实已经死了**时才取 stderr 尾部。活着的 server 的 stderr 是 tracing 噪音,
/// 并进来只会淹掉真正的错因(同两个客户端 `exit_error` 的取舍)。
fn dead_session_stderr(
    client: &PooledClient,
    tail: impl FnOnce() -> Option<String>,
) -> Option<String> {
    (!client.is_running())
        .then(tail)
        .flatten()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

#[allow(clippy::too_many_arguments)]
fn consume_cursor_turn(
    pool: &RpcPool,
    guard: &mut super::cursor_acp::CursorAcpClient,
    client: &PooledClient,
    agent: &str,
    tag: &str,
    session_id: &str,
    prompt: &str,
    stopper: &TurnStopper,
    accum: &mut TurnAccum,
    on_stream: &mut dyn FnMut(&str),
    on_tool: &mut dyn FnMut(&str),
    on_event: &mut dyn FnMut(&AgentMessage),
    on_session_id: &mut dyn FnMut(&str),
) -> anyhow::Result<()> {
    // 轮开始前清残留:ACP 的历史重放(`session/load`)、上一轮的尾巴都排在同一个队里。
    // codex 侧同理,理由见那边的注释。**必须在 `session/prompt` 之前**。
    let stale = guard.drain_queued_notifications();
    if stale > 0 {
        crate::observe::log::log(&summon_i18n::format!("ra8d941b6eb"));
    }
    if let Some(usage) = guard.context_usage() {
        pool.record_ctx_budget(agent, tag, usage.used_tokens, usage.window_tokens);
    }

    // **模型不在 prompt 里传**：池在 checkout 阶段已用 parameterized config setter 把
    // model/context/thinking/reasoning/fast 整向量 exact 收敛，prompt 只承载用户文本。
    guard
        .prompt_start(session_id, prompt)
        .context(summon_i18n::text!("rf68d7a43ac"))?;
    // 只有帧真正写出才消费冷启身份注入。参数/mode 在 checkout 里失败，或 prompt_start 自己
    // 失败，都必须让下一次 checkout（即便显示 Reused）仍携带 PRIMER，也不能向
    // 上层发布 SID。一旦写出，即便后续响应/流失败也必须已落 resume id。
    client.mark_cursor_primer_delivered();
    if let Some(config_dir) = client.cursor_config_dir() {
        crate::capture::cursor_context::register_acp_session(agent, tag, config_dir, session_id);
    }
    on_session_id(session_id);

    let mut silence = SilenceWatchdog::new();
    loop {
        let event = match guard.recv_turn_event(Some(RECV_SLICE)) {
            Ok(event) => event,
            Err(CursorAcpError::Timeout { .. }) => {
                silence.check()?;
                continue;
            }
            // 与 codex 侧逐条同规则:进程死 / prompt 被 server 拒(-32602 之类)/ 协议对不上
            // 都要记成本轮失败,而不是「空轮 DONE」。见 [`fatal_stream_error`]。
            Err(error) => {
                log_stream_break(client, &format!("cursor ACP: {error}"));
                fatal_stream_error(
                    accum,
                    &summon_i18n::format!("r5d11faf774"),
                    dead_session_stderr(client, || guard.stderr_tail()),
                    on_tool,
                    on_stream,
                    on_event,
                );
                break;
            }
        };
        silence.beat();
        stopper.beat_liveness();
        match event {
            AcpTurnEvent::Update(update) => {
                if let summon_core::cursor_acp::CursorAcpUpdate::Usage {
                    used_tokens,
                    window_tokens,
                } = &update
                {
                    pool.record_ctx_budget(agent, tag, *used_tokens, *window_tokens);
                }
                // **传输层错误要留痕。** cursor-agent 偶发把自己的错误串当 `agent_message_chunk`
                // 吐出来(`Error: RetriableError: WritableIterable is closed` 这类),而
                // `stopReason` 仍是干净的 `end_turn`。渲染侧把它滤掉是对的(那不是给用户的正文),
                // 但滤成 `vec![]` 之后**任何地方都不留痕**:events.jsonl 没有、日志没有。
                //
                // 后果(owner 2026-08-15 报「cursor 回答着就断了」):用户看到一段半截的回答 +
                // 一个「✅ 已完成」,而 summon 这边查不出任何异常 —— 轮是 end_turn 正常收的、
                // 没有中止、没有看门狗。**「答完了」和「传输断了」在所有可观测面上完全同形。**
                //
                // 这里只记一行日志,**不改判定**:错误名就叫 Retriable,cursor 很可能自己重试
                // 成功、这一轮照样是好的。把它升级成「本轮失败」需要真机证据支撑,不是现在能定的。
                if let summon_core::cursor_acp::CursorAcpUpdate::AgentMessageChunk(text) = &update {
                    if summon_core::cursor_acp::is_cursor_transport_error(text) {
                        crate::observe::log::log(&summon_i18n::format!("reeeee0ddfe", text.trim()));
                    }
                }
                for msg in acp_update_to_agent_messages(&update) {
                    accum.apply(&msg, on_tool, on_stream, on_event);
                }
            }
            // 终态来自 prompt 的**响应**(不是通知),这是与 codex 最大的结构差异。
            AcpTurnEvent::Ended { stop_reason } => {
                if let Some((used, window)) =
                    crate::capture::cursor_context::registered_budget(agent, tag, session_id)
                        .and_then(|budget| Some((budget.used_tokens?, budget.window_tokens?)))
                {
                    pool.record_ctx_budget(agent, tag, used, window);
                }
                for mut msg in prompt_stop_to_agent_messages(&stop_reason) {
                    msg.session_id = Some(session_id.to_string());
                    accum.apply(&msg, on_tool, on_stream, on_event);
                }
                break;
            }
        }
    }
    Ok(())
}

/// 流断了的日志。区分「进程真的走了」和「只是这一帧读坏了」:前者多见于软打断升级 / 停机 SIGINT,
/// 后者说明协议对不上,值得单独看一眼。两种都会让本轮记 `api_error`(见 [`fatal_stream_error`]),
/// 会话本身仍由下一轮 checkout 懒重连接回。
fn log_stream_break(client: &PooledClient, detail: &str) {
    let dead = if client.is_running() {
        summon_i18n::text!("re47de3c5e6")
    } else {
        summon_i18n::text!("re51e1cc44f")
    };
    crate::observe::log::log(&summon_i18n::format!("rccfafd502f"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_reset_requires_a_successful_codex_terminal_status() {
        let keyword = summon_core::messaging::result_control::SILENT_RESET_KEYWORD;
        for status in [
            "completed",
            "interrupted",
            "aborted",
            "failed",
            "inProgress",
            "unknown",
        ] {
            let mut accum = TurnAccum::new("thread-test");
            let final_answer = CodexAppServerNotification::ItemCompleted {
                thread_id: "thread-test".into(),
                turn_id: "turn-test".into(),
                item: serde_json::json!({"type":"agentMessage", "id":"final", "phase":"final_answer", "text":keyword}),
            };
            let terminal = CodexAppServerNotification::TurnCompleted {
                thread_id: "thread-test".into(),
                turn: serde_json::from_value(
                    serde_json::json!({"id":"turn-test", "status":status}),
                )
                .unwrap(),
            };
            for notification in [final_answer, terminal] {
                accum.observe_main_notification(&notification);
                for event in
                    summon_core::codex_rpc::rpc_notification_to_agent_messages(&notification)
                {
                    accum.apply(&event, &mut |_| {}, &mut |_| {}, &mut |_| {});
                }
            }
            let outcome = accum.finish(&mut |_| {});
            assert_eq!(outcome.reply.as_deref(), Some(keyword));
            assert_eq!(
                outcome.interrupted,
                matches!(status, "interrupted" | "aborted"),
                "{status}"
            );
            assert_eq!(
                outcome.api_error.is_some(),
                !matches!(status, "completed" | "interrupted" | "aborted"),
                "{status}"
            );
        }
    }

    #[test]
    fn silent_reset_uses_the_codex_final_answer_independently_of_commentary() {
        let keyword = summon_core::messaging::result_control::SILENT_RESET_KEYWORD;
        for (progress, final_text) in [
            ("正在检查", keyword.to_string()),
            (keyword, "正常结果".to_string()),
            ("正在检查", format!("{keyword}\n")),
        ] {
            let mut accum = TurnAccum::new("thread-test");
            for (phase, text) in [
                ("commentary", progress),
                ("final_answer", final_text.as_str()),
            ] {
                let notification = CodexAppServerNotification::ItemCompleted {
                    thread_id: "thread-test".into(),
                    turn_id: "turn-test".into(),
                    item: serde_json::json!({"type":"agentMessage", "id":phase, "phase":phase, "text":text}),
                };
                accum.observe_main_notification(&notification);
                for event in
                    summon_core::codex_rpc::rpc_notification_to_agent_messages(&notification)
                {
                    accum.apply(&event, &mut |_| {}, &mut |_| {}, &mut |_| {});
                }
            }
            let mut delivered = None;
            let outcome = accum.finish(&mut |text| delivered = Some(text.to_string()));
            assert_eq!(outcome.reply.as_deref(), Some(final_text.as_str()));
            assert_eq!(delivered.as_deref(), Some(final_text.as_str()));
        }
    }
    use crate::capture::rpc_pool::RpcPool;
    use crate::runtime::inflight::InFlight;
    use crate::test_support::{
        fake_rpc_server_log, process_test_guard, write_fake_acp_server, write_fake_app_server,
    };
    use serde_json::Value;
    use summon_core::registry::Agent;

    const TAG: &str = "gfeedfacefeedface";
    const SESSION: &str = "soul-test-gfeedfacefeedface";

    fn agent_with_fake_server(cases: &str) -> (Agent, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_app_server(tmp.path(), cases);
        let yaml = format!(
            "name: test\nruntime:\n  cli: codex\n  cli_cmd: {}\n",
            script.to_string_lossy()
        );
        let mut agent: Agent = serde_yaml::from_str(&yaml).unwrap();
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串 resume 记录。
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        (agent, tmp)
    }

    /// 轮询到条件成立或超时。两条 RPC 测里的「进程真的没了」都不是同步事件(要等 reader
    /// 线程看到 stdout EOF),固定 sleep 只会把断言变成掷骰子。
    fn wait_until(deadline: Duration, mut ready: impl FnMut() -> bool) -> bool {
        let stop = Instant::now() + deadline;
        while Instant::now() < stop {
            if ready() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ready()
    }

    fn spec(agent: &Agent) -> RpcTurnSpec {
        RpcTurnSpec {
            agent: agent.clone(),
            session: SESSION.to_string(),
            tag: TAG.to_string(),
            chat_id: "oc_test".into(),
        }
    }

    #[test]
    fn cursor_primer_is_driven_by_delivery_state_not_checkout_label() {
        let cursor: Agent = serde_yaml::from_str("name: test\nruntime:\n  cli: cursor\n").unwrap();
        assert_eq!(
            compose_prompt(&cursor, true, Some("IDENTITY"), "hello"),
            "IDENTITY\n\nhello"
        );
        assert_eq!(
            compose_prompt(&cursor, false, Some("IDENTITY"), "hello"),
            "hello"
        );

        let codex: Agent = serde_yaml::from_str("name: test\nruntime:\n  cli: codex\n").unwrap();
        assert_eq!(
            compose_prompt(&codex, true, Some("IDENTITY"), "hello"),
            "hello",
            "Codex 身份走 developerInstructions，不得重复前置到用户 prompt"
        );
    }

    #[test]
    fn queued_rate_limit_notification_is_absorbed_before_the_next_turn() {
        use summon_core::codex_quota::{CodexRateLimitSnapshot, CodexRateLimitWindow};

        let agent = "queued-quota-observation";
        let notification = CodexAppServerNotification::RateLimitsUpdated(CodexRateLimitSnapshot {
            limit_id: "codex".into(),
            limit_name: None,
            primary: Some(CodexRateLimitWindow {
                used_percent: 37.0,
                window_minutes: Some(10_080),
                resets_at: Some(900),
            }),
            secondary: None,
            rate_limit_reached_type: None,
        });

        assert_eq!(
            record_codex_quota_notification(agent, "g-stale-queue", &notification),
            Some(63)
        );
        assert_eq!(
            crate::capture::rpc_pool::current_quota_shared(agent, "g-next-turn"),
            Some(63)
        );
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

    /// 一轮的完整通知序列(按设计文档 §2 的真机序列):正文 delta ×2 + 完成快照 + usage + 终态。
    const PONG_TURN: &str = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-1","turnId":"turn-1","item":{"id":"m","type":"agentMessage"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"po"}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"ng"}}';
        printf '%s\n' '{"method":"item/completed","params":{"threadId":"th-1","turnId":"turn-1","item":{"id":"m","type":"agentMessage","text":"pong"}}}';
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"th-1","turnId":"turn-1","tokenUsage":{"total":{"totalTokens":99},"last":{"inputTokens":11,"cachedInputTokens":2,"outputTokens":3,"totalTokens":14},"modelContextWindow":258400}}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;

    const INCOMPLETE_CONTEXT_TURNS: &str = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) turn_no=$((${turn_no:-0} + 1));
        printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-'"$turn_no"'","status":"inProgress"}}}';
        printf '%s\n' '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"turn-'"$turn_no"'","status":"inProgress"}}}';
        if [ "$turn_no" -eq 1 ] || [ "$turn_no" -eq 3 ]; then
            printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"th-1","turnId":"turn-'"$turn_no"'","tokenUsage":{"last":{"inputTokens":11,"cachedInputTokens":2,"outputTokens":3,"totalTokens":14},"modelContextWindow":258400}}}';
        elif [ "$turn_no" -eq 2 ]; then
            printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"th-1","turnId":"turn-2","tokenUsage":{"last":{"inputTokens":22,"cachedInputTokens":2,"outputTokens":3,"totalTokens":25}}}}';
        else
            printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"th-1","turnId":"turn-4","tokenUsage":{"total":{"totalTokens":44},"modelContextWindow":258400}}}';
        fi;
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-'"$turn_no"'","status":"completed"}}}' ;;
"#;

    struct Sink {
        message: Option<String>,
        streams: Vec<String>,
        events: Vec<AgentMessage>,
        sid: Option<String>,
        stopper_pgid: Option<i32>,
    }

    fn run(
        pool: &RpcPool,
        agent: &Agent,
        prompt: &str,
        primer: Option<&str>,
    ) -> (TurnOutcome, Sink) {
        run_with_service_tier(pool, agent, prompt, primer, None)
    }

    fn run_with_service_tier(
        pool: &RpcPool,
        agent: &Agent,
        prompt: &str,
        primer: Option<&str>,
        service_tier: Option<summon_core::driver::CodexServiceTier>,
    ) -> (TurnOutcome, Sink) {
        let mut sink = Sink {
            message: None,
            streams: Vec::new(),
            events: Vec::new(),
            sid: None,
            stopper_pgid: None,
        };
        let ctx = TurnCtx {
            model: Some("gpt-5.5"),
            effort: Some("high"),
            codex_reasoning_summary: Some("detailed"),
            codex_service_tier: service_tier,
            ..Default::default()
        };
        let out = run_turn(
            pool,
            &spec(agent),
            prompt,
            primer,
            &ctx,
            &mut |body| sink.message = Some(body.to_string()),
            &mut |acc| sink.streams.push(acc.to_string()),
            &mut |_tool| {},
            &mut |ev| sink.events.push(ev.clone()),
            &mut |stopper| sink.stopper_pgid = Some(stopper.killer().pgid()),
            &mut |sid| sink.sid = Some(sid.to_string()),
        )
        .unwrap();
        (out, sink)
    }

    /// 从 chat 级 TurnCtx 一路钉到真实发出的 JSON：Fast 用 priority，关闭时必须显式 null。
    /// 第二轮沿用同一 thread，证明 Standard 不是靠重建会话碰巧清掉 sticky Fast。
    #[test]
    fn service_tier_flows_through_the_rpc_engine_and_standard_clears_fast() {
        use summon_core::driver::CodexServiceTier;

        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(PONG_TURN);
        let pool = RpcPool::new(InFlight::new());

        run_with_service_tier(&pool, &agent, "fast", None, Some(CodexServiceTier::Fast));
        run_with_service_tier(
            &pool,
            &agent,
            "standard",
            None,
            Some(CodexServiceTier::Standard),
        );

        let starts = sent(tmp.path(), "turn/start");
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["params"]["serviceTier"], "priority");
        assert_eq!(
            starts[1]["params"].get("serviceTier"),
            Some(&Value::Null),
            "关闭 Fast 必须显式清掉 thread 的 sticky override"
        );
        assert_eq!(sent(tmp.path(), "thread/start").len(), 1);
        assert!(sent(tmp.path(), "thread/resume").is_empty());
    }

    /// 一轮端到端:流式增量刷卡、终态回复、sid、usage join、per-turn model/effort/summary 全在帧里。
    #[test]
    fn one_turn_streams_and_reports_the_same_shape_as_the_subprocess_path() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(PONG_TURN);
        let pool = RpcPool::new(InFlight::new());

        let (out, sink) = run(&pool, &agent, "ping", Some("你是测试体"));

        assert_eq!(out.reply.as_deref(), Some("pong"), "终态回复来自累积正文");
        assert_eq!(sink.message.as_deref(), Some("pong"));
        assert_eq!(out.session_id.as_deref(), Some("th-1"));
        assert_eq!(sink.sid.as_deref(), Some("th-1"), "sid 应在轮开始时就回传");
        assert!(
            sink.stopper_pgid.unwrap_or(0) > 0,
            "把手应紧随 checkout 发布"
        );
        // 打字机:delta 逐字增长;完成快照再刷一帧同文(它是**段终止符**)而不叠加成 "pongpong"
        // 完成快照是当前段的权威全文，内容与已经累计的 delta 相同时保持正文不变。
        assert_eq!(
            sink.streams,
            vec!["po".to_string(), "pong".to_string(), "pong".to_string()]
        );

        // usage join:终态事件带的是「这一轮」的 last 口径,不是 thread 累计。
        let result = sink
            .events
            .iter()
            .find(|e| e.kind == AgentKind::Result)
            .expect("应有终态事件");
        let usage = result.usage.as_ref().expect("终态应带 usage");
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.cache_read, 2);
        assert_eq!(
            pool.current_ctx_budget(&agent.name, TAG),
            Some((11, 258400)),
            "Codex app-server 的窗口与本轮输入量应组成可计算百分比的上下文快照"
        );

        // per-turn 覆盖真的进了 turn/start 的 params。
        let starts = sent(tmp.path(), "turn/start");
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0]["params"]["model"], "gpt-5.5");
        assert_eq!(starts[0]["params"]["effort"], "high");
        assert_eq!(starts[0]["params"]["summary"], "detailed");
        assert_eq!(starts[0]["params"]["input"][0]["text"], "ping");

        // PRIMER 走 `developerInstructions`(追加语义),**不拼进用户消息**。
        let cold = sent(tmp.path(), "thread/start");
        assert_eq!(cold[0]["params"]["developerInstructions"], "你是测试体");
        assert!(
            !starts[0]["params"]["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains("你是测试体"),
            "PRIMER 绝不能混进用户首条消息"
        );
    }

    #[test]
    fn codex_incomplete_realtime_budget_clears_the_previous_context_snapshot() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(INCOMPLETE_CONTEXT_TURNS);
        let pool = RpcPool::new(InFlight::new());

        run(&pool, &agent, "first", None);
        assert_eq!(
            pool.current_ctx_budget(&agent.name, TAG),
            Some((11, 258400))
        );

        run(&pool, &agent, "second", None);
        assert_eq!(
            pool.current_ctx_budget(&agent.name, TAG),
            None,
            "实时通知缺少窗口时必须清掉旧快照，不能把上一轮百分比带进本轮页脚"
        );

        run(&pool, &agent, "third", None);
        assert_eq!(
            pool.current_ctx_budget(&agent.name, TAG),
            Some((11, 258400))
        );

        run(&pool, &agent, "fourth", None);
        assert_eq!(
            pool.current_ctx_budget(&agent.name, TAG),
            None,
            "实时通知缺少本轮用量时必须清掉旧快照，不能把上一轮百分比带进本轮页脚"
        );
    }

    #[test]
    fn codex_realtime_context_snapshot_reaches_the_completion_card_footer() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(PONG_TURN);
        let pool = crate::capture::rpc_pool::shared_for_tests();
        pool.evict(&agent.name, TAG);

        run(&pool, &agent, "footer", None);
        let seed = summon_core::feishu::TurnBadge {
            cli: "codex".into(),
            model: Some("gpt-5.6-sol".into()),
            ..Default::default()
        };
        let badge = crate::runtime::daemon::completion_badge_for_test(
            &seed,
            &agent.name,
            &agent.state_dir(),
            TAG,
        );

        assert_eq!(
            badge.render().as_deref(),
            Some("codex · gpt-5.6-sol · 11/258k ░░░░░░░░░░ 0%")
        );
        pool.evict(&agent.name, TAG);
    }

    #[test]
    fn native_user_input_round_trips_through_the_rpc_engine_before_turn_completion() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"id":"ask-1","method":"item/tool/requestUserInput","params":{"threadId":"th-1","turnId":"turn-1","itemId":"item-1","questions":[{"id":"q-plan","header":"方案","question":"选哪个？","isOther":false,"isSecret":false,"options":[{"label":"A","description":"第一项"},{"label":"B","description":"第二项"}]},{"id":"q-note","header":"补充","question":"需要补充吗？","isOther":true,"isSecret":false,"options":null}],"isBlocking":true,"autoResolutionMs":null}}' ;;
    *'"id":"ask-1","result"'*) printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"收到回答"}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;
        let (agent, tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let stopper_slot = Arc::new(std::sync::Mutex::new(None::<TurnStopper>));
        let stopper_for_handler = Arc::clone(&stopper_slot);
        let seen = Arc::new(std::sync::Mutex::new(None));
        let seen_w = Arc::clone(&seen);
        let handler: crate::capture::CodexUserInputHandlerRef = Arc::new(
            move |request: &crate::codex::codex_app_server::CodexUserInputRequest,
                  cancelled: &AtomicBool,
                  session_alive: &AtomicBool| {
                assert!(!cancelled.load(Ordering::Acquire));
                assert!(session_alive.load(Ordering::Acquire));
                assert!(
                    stopper_for_handler
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(TurnStopper::is_waiting_for_user),
                    "进入 handler 前必须发布 waiting 状态给外层 watchdog"
                );
                *seen_w.lock().unwrap() = Some(request.clone());
                Ok(vec![vec!["B".into()], Vec::new()])
            },
        );
        let ctx = TurnCtx {
            codex_user_input_handler: Some(handler),
            ..Default::default()
        };
        let mut reply = None;
        let stopper_for_spawn = Arc::clone(&stopper_slot);
        let out = run_turn(
            &pool,
            &spec(&agent),
            "先问再答",
            None,
            &ctx,
            &mut |body| reply = Some(body.to_string()),
            &mut |_stream| {},
            &mut |_tool| {},
            &mut |_event| {},
            &mut |stopper| *stopper_for_spawn.lock().unwrap() = Some(stopper.clone()),
            &mut |_sid| {},
        )
        .unwrap();

        assert_eq!(out.reply.as_deref(), Some("收到回答"));
        assert_eq!(reply.as_deref(), Some("收到回答"));
        let request = seen.lock().unwrap().clone().expect("handler 必须收到提问");
        assert_eq!(request.question_ids, ["q-plan", "q-note"]);
        assert_eq!(request.questions[0].options, ["A", "B"]);
        assert!(request.questions[1].allow_other);
        let response = requests(tmp.path())
            .into_iter()
            .find(|frame| {
                frame.get("id").and_then(Value::as_str) == Some("ask-1")
                    && frame.get("result").is_some()
            })
            .expect("客户端必须写回同一个 string request id");
        assert_eq!(response["result"]["answers"]["q-plan"]["answers"][0], "B");
        assert_eq!(
            response["result"]["answers"]["q-note"]["answers"],
            serde_json::json!([])
        );
        assert!(
            !stopper_slot
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_waiting_for_user(),
            "回帧后 waiting 必须由 RAII 清零"
        );
        pool.evict(&agent.name, TAG);
    }

    /// resume 轮不重注 PRIMER:第二轮复用同一个进程,压根不再发 thread/start;
    /// 而懒重连那次走的是 `thread/resume`(params 里没有 baseInstructions 可带)。
    #[test]
    fn primer_is_injected_once_at_cold_start_not_every_turn() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(PONG_TURN);
        let pool = RpcPool::new(InFlight::new());

        run(&pool, &agent, "ping", Some("你是测试体"));
        run(&pool, &agent, "再来", Some("你是测试体"));

        assert_eq!(
            sent(tmp.path(), "thread/start").len(),
            1,
            "第二轮复用会话,不该再建 thread"
        );
        assert_eq!(sent(tmp.path(), "turn/start").len(), 2);
    }

    #[test]
    fn cold_codex_session_handles_first_message_with_protocol_in_the_same_turn() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(PONG_TURN);
        let pool = RpcPool::new(InFlight::new());
        let primer = summon_core::config::builtin_messaging().system_prompt_filled(
            "/tmp/emoji.md",
            true,
            summon_core::config::PrimerSections::ALL,
        );
        let (outcome, _) = run(&pool, &agent, "第一条业务消息", Some(&primer));
        assert_eq!(outcome.reply.as_deref(), Some("pong"));
        let starts = sent(tmp.path(), "thread/start");
        assert_eq!(starts.len(), 1);
        assert!(starts[0]["params"]["developerInstructions"]
            .as_str()
            .unwrap()
            .contains("【Summon 消息协议 v1】"));
        let turns = sent(tmp.path(), "turn/start");
        assert_eq!(turns.len(), 1, "首条消息只运行一个模型轮");
        assert_eq!(turns[0]["params"]["input"][0]["text"], "第一条业务消息");
        pool.evict(&agent.name, TAG);
    }

    /// 非法 effort:丢掉 effort 降级跑完本轮,**不 panic、不阻断**(同 launch 路的 fail-closed 分层)。
    #[test]
    fn illegal_effort_is_dropped_instead_of_failing_the_turn() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(PONG_TURN);
        let pool = RpcPool::new(InFlight::new());
        let ctx = TurnCtx {
            effort: Some("telepathy"),
            ..Default::default()
        };
        let out = run_turn(
            &pool,
            &spec(&agent),
            "ping",
            None,
            &ctx,
            &mut |_b| {},
            &mut |_s| {},
            &mut |_t| {},
            &mut |_e| {},
            &mut |_k| {},
            &mut |_s| {},
        )
        .unwrap();
        assert_eq!(out.reply.as_deref(), Some("pong"));
        let starts = sent(tmp.path(), "turn/start");
        assert!(
            starts[0]["params"].get("effort").is_none(),
            "非法档位不该被原样发出去"
        );
    }

    #[test]
    fn failed_codex_terminal_keeps_capacity_reason_in_the_notice_card() {
        let _guard = process_test_guard();
        for (error, reason) in [
            (
                serde_json::json!({"message":"Selected model is at capacity. Please try a different model.","codexErrorInfo":"serverOverloaded"}),
                "Selected model is at capacity. Please try a different model.",
            ),
            (
                serde_json::json!({"message":""}),
                "Codex turn ended with status: failed",
            ),
            (
                serde_json::json!({"message":"   "}),
                "Codex turn ended with status: failed",
            ),
        ] {
            let terminal = serde_json::json!({"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"failed","error":error}}});
            let cases = format!(
                r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{{"id":'"$id"',"result":{{"thread":{{"id":"th-1","status":{{"type":"idle"}},"turns":[]}}}}}}' ;;
    *'turn/start'*) printf '%s\n' '{{"id":'"$id"',"result":{{"turn":{{"id":"turn-1","status":"inProgress"}}}}}}';
        printf '%s\n' '{{"method":"error","params":{{"threadId":"th-1","turnId":"turn-1","willRetry":false,"error":{{"message":"   "}}}}}}';
        printf '%s\n' '{}' ;;
"#,
                terminal
            );
            let (agent, tmp) = agent_with_fake_server(&cases);
            let pool = RpcPool::new(InFlight::new());
            let (outcome, _) = run(&pool, &agent, "ping", None);
            assert_eq!(outcome.api_error.as_deref(), Some(reason));
            let frontend = FailureCardFrontend(std::cell::RefCell::new(Vec::new()));
            let chat = crate::frontend::ChatRef {
                agent_dir: tmp.path(),
                chat_id: "oc_capacity",
                chat_type: "group",
                agent_name: "test",
                delivery_scope: "scope-test",
                thread_anchor: None,
            };
            let outbox = crate::runtime::outbox_store::OutboxStore::new(tmp.path().join("outbox"));
            assert!(crate::runtime::daemon::failure_notice_for_test(
                &frontend, &chat, &outbox, &outcome
            ));
            let cards = frontend.0.borrow();
            assert_eq!(cards.len(), 1);
            assert!(cards[0].contains(reason));
            if !reason.starts_with("Codex turn ended") {
                assert!(!cards[0].contains("Codex turn ended with status"));
            }
            assert!(outbox.pending_events().is_empty());
            pool.evict(&agent.name, TAG);
        }
    }

    struct FailureCardFrontend(std::cell::RefCell<Vec<String>>);
    impl crate::frontend::Frontend for FailureCardFrontend {
        fn send_message(&self, _: &crate::frontend::ChatRef, _: &str) -> anyhow::Result<()> {
            panic!("失败必须通过卡片投递")
        }
        fn stream_begin(
            &self,
            _: &crate::frontend::ChatRef,
            _: &str,
            _: bool,
        ) -> anyhow::Result<crate::frontend::StreamId> {
            Ok(crate::frontend::StreamId(1))
        }
        fn stream_patch(&self, _: crate::frontend::StreamId, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn react(&self, _: &crate::frontend::MsgRef, _: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn unreact(&self, _: &crate::frontend::MsgRef, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn send_card(&self, _: &crate::frontend::ChatRef, card: &str) -> anyhow::Result<()> {
            self.0.borrow_mut().push(card.to_string());
            Ok(())
        }
    }

    #[test]
    fn failed_codex_status_keeps_an_earlier_error_and_falls_back_when_absent() {
        for reason in [
            None,
            Some("Selected model is at capacity. Please try a different model."),
        ] {
            let mut accum = TurnAccum::new("thread-test");
            if let Some(reason) = reason {
                let error = CodexAppServerNotification::Error {
                    thread_id: "thread-test".into(),
                    turn_id: "turn-test".into(),
                    will_retry: false,
                    error: serde_json::from_value(serde_json::json!({"message":reason})).unwrap(),
                };
                for event in summon_core::codex_rpc::rpc_notification_to_agent_messages(&error) {
                    accum.apply(&event, &mut |_| {}, &mut |_| {}, &mut |_| {});
                }
            }
            let terminal = CodexAppServerNotification::TurnCompleted {
                thread_id: "thread-test".into(),
                turn: serde_json::from_value(
                    serde_json::json!({"id":"turn-test","status":"failed"}),
                )
                .unwrap(),
            };
            accum.observe_main_notification(&terminal);
            for event in summon_core::codex_rpc::rpc_notification_to_agent_messages(&terminal) {
                accum.apply(&event, &mut |_| {}, &mut |_| {}, &mut |_| {});
            }
            let outcome = accum.finish(&mut |_| {});
            assert_eq!(
                outcome.api_error.as_deref(),
                Some(reason.unwrap_or("Codex turn ended with status: failed"))
            );
        }
    }

    /// 会话在轮内死掉:已出的正文照常收尾(不丢字),**但这一轮必须记成失败**。
    ///
    /// 两半都要:只留前半(旧行为)的话,进程崩和「模型说完了」在卡上长得一模一样 —— 同样的
    /// DONE 表情、同样一段正文,用户看不出这一轮其实断了。诊断只在 stderr 里,所以尾部必须
    /// 一起带进 `api_error`(exec 路的 `bail!` 就是这么做的)。
    #[test]
    fn session_death_mid_turn_keeps_partial_text_and_reports_an_api_error() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"半句"}}';
        printf '%s\n' 'summon-test-stderr: app-server 崩了' >&2;
        exit 0 ;;
"#;
        let (agent, _tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let (out, sink) = run(&pool, &agent, "ping", None);
        assert_eq!(
            out.reply.as_deref(),
            Some("半句"),
            "断连也要把已出的字交出去"
        );
        assert_eq!(sink.message.as_deref(), Some("半句"));
        let error = out
            .api_error
            .expect("进程死在轮内必须记成本轮失败,不能当正常完成");
        assert!(
            error.contains("summon-test-stderr: app-server 崩了"),
            "错误里必须带 stderr 尾部(真因只写在那儿),实得: {error}"
        );
    }

    /// **跨轮隔离**:池复用同一个进程,上一轮的尾随事件仍可能在路上。带别的 turnId 的正文、
    /// usage、甚至 `turn/completed` 全都不许进本轮 —— 混进来不报错、不丢回复,只是内容归错了轮
    /// (卡上多出上一轮的半句话 / usage 是上一轮的口径),是最难被发现的那种错。
    #[test]
    fn notifications_from_another_turn_never_reach_this_one() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-2","status":"inProgress"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"上一轮的尾巴"}}';
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"th-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":999,"outputTokens":999,"totalTokens":999}}}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-2","itemId":"m","delta":"本轮"}}';
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"th-1","turnId":"turn-2","tokenUsage":{"last":{"inputTokens":11,"cachedInputTokens":2,"outputTokens":3,"totalTokens":14}}}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-2","status":"completed"}}}' ;;
"#;
        let (agent, _tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let (out, sink) = run(&pool, &agent, "ping", None);

        assert_eq!(
            out.reply.as_deref(),
            Some("本轮"),
            "上一轮的正文绝不能拼进本轮回复"
        );
        // 上一轮的 `turn/completed` 也不许把本轮提前收掉(收掉了本轮正文就整段没了)。
        let results: Vec<_> = sink
            .events
            .iter()
            .filter(|e| e.kind == AgentKind::Result)
            .collect();
        assert_eq!(results.len(), 1, "只该有本轮那一个终态事件");
        let usage = results[0].usage.as_ref().expect("终态应带 usage");
        assert_eq!(usage.input_tokens, 11, "usage 必须是本轮口径,不是上一轮的");
        assert_eq!(usage.output_tokens, 3);
    }

    /// 真机形状(codex-cli 0.153.4):子线程的通知可以先于主线程的 `subAgentActivity(started)`
    /// 到达。子线程的事件带着子 agent 标识交给事件回调,不进本轮回复、不带用量,子线程的
    /// `turn/completed` 不收本轮;没登记过的线程一直不投。
    #[test]
    fn subagent_thread_notifications_reach_the_event_sink_tagged_and_isolated() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-child","turnId":"tc-1","item":{"id":"c0","type":"commandExecution","command":"/bin/zsh -lc '"'"'echo early'"'"'"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-other","turnId":"to-1","item":{"id":"o0","type":"commandExecution","command":"ls"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-1","turnId":"turn-1","item":{"id":"a1","type":"subAgentActivity","kind":"started","agentThreadId":"th-child","agentPath":"/root/child_probe"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-child","turnId":"tc-1","itemId":"cm","delta":"子 agent 的话"}}';
        printf '%s\n' '{"method":"item/completed","params":{"threadId":"th-child","turnId":"tc-1","item":{"id":"cm","type":"agentMessage","text":"子 agent 的话"}}}';
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"th-child","turnId":"tc-1","tokenUsage":{"last":{"inputTokens":777,"outputTokens":7,"totalTokens":784},"modelContextWindow":1000}}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-child","turn":{"id":"tc-1","status":"completed"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"主回复"}}';
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"th-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":11,"outputTokens":3,"totalTokens":14}}}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;
        let (agent, _tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let (out, sink) = run(&pool, &agent, "ping", None);

        assert_eq!(
            out.reply.as_deref(),
            Some("主回复"),
            "子 agent 的话不进本轮回复"
        );
        let child: Vec<_> = sink
            .events
            .iter()
            .filter(|e| e.parent_run_id.is_some())
            .collect();
        assert!(child
            .iter()
            .all(|e| e.parent_run_id.as_ref().map(|id| id.0.as_str()) == Some("th-child")));
        assert!(child
            .iter()
            .all(|e| e.usage.is_none() && e.session_id.is_none()));
        assert_eq!(
            child[0].tool.as_deref(),
            Some("command_execution"),
            "登记前到达的子线程事件在登记后补投,且排在最前"
        );
        assert!(child.iter().any(|e| e.kind == AgentKind::Result));
        assert!(
            !sink
                .events
                .iter()
                .any(|e| e.tool_input == Some(serde_json::json!("ls"))),
            "没登记过的线程不投"
        );
        let results: Vec<_> = sink
            .events
            .iter()
            .filter(|e| e.kind == AgentKind::Result && e.parent_run_id.is_none())
            .collect();
        assert_eq!(results.len(), 1, "子线程的 turn/completed 不收本轮");
        assert_eq!(results[0].usage.as_ref().map(|u| u.input_tokens), Some(11));
    }

    /// 子线程也能派子 agent:子线程上的 `subAgentActivity(started)` 登记孙线程,孙线程的通知
    /// 以它自己的线程 id 为子 agent 标识交给事件回调。派出记录可能在子线程登记后直接到达,也可能
    /// 被攒着(子线程还没登记)、补投时才看到,两条路都登记,孙线程早到的通知跟着补投。
    #[test]
    fn grandchild_thread_notifications_are_registered_from_the_child_thread() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-grand","turnId":"tg-1","item":{"id":"g0","type":"commandExecution","command":"echo g0"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-child","turnId":"tc-1","item":{"id":"a2","type":"subAgentActivity","kind":"started","agentThreadId":"th-grand","agentPath":"/root/child/grand"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-1","turnId":"turn-1","item":{"id":"a1","type":"subAgentActivity","kind":"started","agentThreadId":"th-child","agentPath":"/root/child"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-grand","turnId":"tg-1","item":{"id":"g1","type":"commandExecution","command":"echo g1"}}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-grand","turn":{"id":"tg-1","status":"completed"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-grand2","turnId":"tg-2","item":{"id":"h0","type":"commandExecution","command":"echo h0"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-child","turnId":"tc-1","item":{"id":"a3","type":"subAgentActivity","kind":"started","agentThreadId":"th-grand2","agentPath":"/root/child/grand2"}}}';
        printf '%s\n' '{"method":"item/started","params":{"threadId":"th-grand2","turnId":"tg-2","item":{"id":"h1","type":"commandExecution","command":"echo h1"}}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-child","turn":{"id":"tc-1","status":"completed"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"主回复"}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;
        let (agent, _tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let (out, sink) = run(&pool, &agent, "ping", None);

        assert_eq!(out.reply.as_deref(), Some("主回复"));
        let tagged: Vec<(&str, AgentKind, Option<&str>)> = sink
            .events
            .iter()
            .filter_map(|e| {
                let parent = e.parent_run_id.as_ref()?.0.as_str();
                Some((parent, e.kind.clone(), e.tool_id.as_deref()))
            })
            .collect();
        assert_eq!(
            tagged,
            [
                ("th-child", AgentKind::ToolUse, Some("a2")),
                ("th-grand", AgentKind::ToolUse, Some("g0")),
                ("th-grand", AgentKind::ToolUse, Some("g1")),
                ("th-grand", AgentKind::Result, None),
                ("th-child", AgentKind::ToolUse, Some("a3")),
                ("th-grand2", AgentKind::ToolUse, Some("h0")),
                ("th-grand2", AgentKind::ToolUse, Some("h1")),
                ("th-child", AgentKind::Result, None),
            ]
        );
        let results = sink
            .events
            .iter()
            .filter(|e| e.kind == AgentKind::Result && e.parent_run_id.is_none())
            .count();
        assert_eq!(results, 1, "孙线程的 turn/completed 同样不收本轮");
    }

    fn child_notification(thread: &str, value: serde_json::Value) -> CodexAppServerNotification {
        let mut value = value;
        value["params"]["threadId"] = serde_json::json!(thread);
        summon_core::codex_rpc::parse_notification(&value)
            .unwrap()
            .unwrap()
    }

    /// 没登记的线程先攒着,但总量既有条数上限、也有字节上限:一次命令输出就能有几 MB。超额的丢掉,
    /// 该线程之后也不再攒(不在攒下的序列里留空洞);登记后照常按子线程走。
    #[test]
    fn unregistered_threads_are_held_within_count_and_byte_budgets() {
        let big = |id: usize| {
            child_notification(
                "th-big",
                serde_json::json!({"method": "item/completed", "params": {"turnId": "t", "item": {
                    "id": format!("c{id}"), "type": "commandExecution",
                    "aggregatedOutput": "x".repeat(100_000)}}}),
            )
        };
        let mut threads = SubagentThreads::default();
        for id in 0..40 {
            assert!(matches!(
                threads.route(&big(id), "th-1"),
                SubagentRoute::Held
            ));
        }
        assert!(threads.held_bytes <= SubagentThreads::MAX_HELD_BYTES);
        threads.route(&child_notification("th-big", tiny_delta()), "th-1");
        let held = threads.register("th-big");
        assert!(
            (5..40).contains(&held.len()),
            "只攒得下预算内的:{}",
            held.len()
        );
        assert!(
            held.iter()
                .all(|n| matches!(n, CodexAppServerNotification::ItemCompleted { .. })),
            "超额之后到的小通知也不攒"
        );
        assert_eq!((threads.held_count, threads.held_bytes), (0, 0));
        assert!(matches!(
            threads.route(&big(99), "th-1"),
            SubagentRoute::Child(thread) if thread == "th-big"
        ));

        let mut threads = SubagentThreads::default();
        for n in 0..SubagentThreads::MAX_HELD + 20 {
            let thread = format!("th-sub{}", n % 7);
            threads.route(&child_notification(&thread, tiny_delta()), "th-1");
        }
        assert_eq!(threads.held_count, SubagentThreads::MAX_HELD);
    }

    fn tiny_delta() -> serde_json::Value {
        serde_json::json!({"method": "item/agentMessage/delta", "params": {
            "turnId": "t", "itemId": "m", "delta": "x"}})
    }

    fn token_usage(
        thread: &str,
        total: Option<u64>,
        input: u64,
        cached: u64,
    ) -> CodexAppServerNotification {
        let mut usage = serde_json::json!({"last": {
            "inputTokens": input, "cachedInputTokens": cached, "outputTokens": 30,
            "reasoningOutputTokens": 12, "totalTokens": input + 30}});
        if let Some(total) = total {
            usage["total"] = serde_json::json!({"totalTokens": total});
        }
        child_notification(
            thread,
            serde_json::json!({"method": "thread/tokenUsage/updated", "params": {
                "turnId": "t", "tokenUsage": usage}}),
        )
    }

    /// 每次请求一条:同一份用量重复推送不重复计;累计回落之后照常计;各线程的累计各算各的;输入换成
    /// 不含缓存的口径。
    #[test]
    fn request_tally_counts_each_request_once_per_thread() {
        let mut tally = RequestTally::new("turn-1");
        let model = || Some("gpt-x".to_string());
        let mut observe = |thread: &str, total: Option<u64>, input: u64, cached: u64| {
            tally.observe(&token_usage(thread, total, input, cached), model(), None)
        };
        let first = observe("th-1", Some(1_030), 1_000, 800).unwrap();
        assert_eq!(first.id, "th-1:turn-1#1");
        assert_eq!(
            (first.input, first.cache_read, first.output),
            (200, 800, 30)
        );
        assert_eq!(first.model.as_deref(), Some("gpt-x"));
        // 同一份用量重复推送
        assert!(observe("th-1", Some(1_030), 1_000, 800).is_none());
        // 子线程的累计各算各的
        assert!(observe("th-c", Some(1_030), 1_000, 0).is_some());
        assert!(observe("th-1", Some(2_100), 1_040, 1_000).is_some());
        // 压缩上下文之后累计回落:之后的请求照常计,回到旧值也不与之前的请求混成一条
        let after = observe("th-1", Some(900), 870, 0).unwrap();
        let back = observe("th-1", Some(2_100), 1_200, 900).unwrap();
        assert_ne!(after.id, back.id);
        assert!(observe("th-1", Some(2_100), 1_200, 900).is_none());
        // 没带累计的每条都计,id 各不相同
        let a = observe("th-1", None, 10, 0).unwrap();
        let b = observe("th-1", None, 10, 0).unwrap();
        assert_ne!(a.id, b.id);
        let child = tally
            .observe(
                &token_usage("th-c", Some(1_600), 500, 0),
                model(),
                Some("priority"),
            )
            .unwrap();
        assert_eq!(child.variant.as_deref(), Some("priority"));
        assert!(tally
            .observe(&child_notification("th-1", tiny_delta()), None, None)
            .is_none());
    }

    /// 轮内追加消息降级重跑:同一行运行记录里第二次 `turn/start` 的请求不能与第一次的同 id,
    /// 否则按 id 合并时只剩较大的那份。
    #[test]
    fn requests_of_a_rerun_in_the_same_record_keep_distinct_ids() {
        let usage = token_usage("th-1", Some(1_030), 1_000, 800);
        let first = RequestTally::new("turn-1")
            .observe(&usage, None, None)
            .unwrap();
        let rerun = RequestTally::new("turn-2")
            .observe(&usage, None, None)
            .unwrap();
        assert_ne!(first.id, rerun.id);
        let mut facts = crate::runtime::turn_ledger::TurnFacts::default();
        facts.absorb_request(&first);
        facts.absorb_request(&rerun);
        assert_eq!(facts.requests.len(), 2);
    }

    #[test]
    fn plan_updates_from_another_turn_are_filtered_before_the_event_sink() {
        let foreign = summon_core::codex_rpc::parse_notification(&serde_json::json!({
            "method": "turn/plan/updated",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-old",
                "plan": [{"step": "旧计划", "status": "inProgress"}]
            }
        }))
        .unwrap()
        .unwrap();
        assert!(!belongs_to_turn(&foreign, "turn-current"));
        assert!(belongs_to_turn(&foreign, "turn-old"));

        let warning = summon_core::codex_rpc::parse_notification(&serde_json::json!({
            "method": "warning",
            "params": {"threadId": "thread-1", "turnId": "turn-old", "message": "旧告警"}
        }))
        .unwrap()
        .unwrap();
        assert!(!belongs_to_turn(&warning, "turn-current"));
        assert!(belongs_to_turn(&warning, "turn-old"));
    }

    #[test]
    fn repeated_notices_leave_one_audit_event_per_method_per_turn() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"warning","params":{"threadId":"th-1","turnId":"turn-1","message":"同一告警"}}';
        printf '%s\n' '{"method":"warning","params":{"threadId":"th-1","turnId":"turn-1","message":"同一告警"}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;
        let (agent, _tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let (_out, sink) = run(&pool, &agent, "ping", None);
        let notices: Vec<_> = sink
            .events
            .iter()
            .filter(|event| {
                event.kind == AgentKind::System
                    && event
                        .text
                        .as_deref()
                        .is_some_and(|text| text.contains("同一告警"))
            })
            .collect();
        assert_eq!(notices.len(), 1);
    }

    /// 轮开始**之前**就排在队里的通知一律丢:client 跨轮存活,开会话请求等待响应期间排进来的
    /// 东西属于上一段生命,不属于将要开始的这一轮。
    ///
    /// 夹具刻意让残留带上**本轮将要拿到的那个 turnId**(`turn-1`)—— 这样 turnId 闸挡不住它,
    /// 唯一能挡的就是轮前 drain。两道闸各管一段,这条钉的是后一道。
    #[test]
    fn notifications_queued_before_the_turn_started_are_dropped() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"开会话期间的残留"}}';
        printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"本轮"}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;
        let (agent, _tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let (out, _) = run(&pool, &agent, "ping", None);
        assert_eq!(
            out.reply.as_deref(),
            Some("本轮"),
            "轮前残留必须在投递之前就被丢掉"
        );
    }

    /// **B2**:轮还没投出去时打断,必须至少发出 SIGINT,不能一枪不发。
    ///
    /// 把手是在 `checkout` 之后、`turn/start` 之前发布的(必须如此:进程已经在了,这中间的
    /// 停机/打断都得够得着它),于是「软打断此刻没有作用对象」是一个真实存在的窗口。老实现
    /// 在这里只记一行 `Idle` 日志、什么都不发 —— 上层 `await_interrupted_headless` 宽限一到
    /// 就直接 SIGKILL:**SIGINT 这一级被整个跳过**,codex 写不出 `<turn_aborted>`,
    /// 下一轮 resume 拿到的是悄悄少了一段的上下文。
    ///
    /// 断言看的是 `was_killed()`(那把一次性闸只有真发过信号才置位)+ 进程真的收敛了。
    #[test]
    fn interrupting_before_the_turn_starts_still_sends_sigint() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(
            r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
"#,
        );
        let pool = RpcPool::new(InFlight::new());
        let client = pool
            .checkout(&crate::capture::rpc_pool::RpcChat {
                agent: &agent,
                session: SESSION,
                tag: TAG,
                chat_id: "oc_test",
                cwd: None,
                primer: None,
                model: None,
                service_tier: None,
                cursor_mode: None,
                codex_policy: Default::default(),
                register_platform_tools: false,
            })
            .unwrap();
        let pgid = client.pgid();
        let stopper = TurnStopper::rpc(client.killer(), client.interrupt_handle());

        // 此刻没有任何在跑的轮 —— 正是 `on_spawn` 发布把手到 `turn/start` 之间那个窗口。
        stopper.interrupt();

        assert!(
            stopper.killer().was_killed(),
            "轮未在跑时打断必须回退到 SIGINT,而不是一枪不发让上层直接 SIGKILL"
        );
        assert!(
            sent(tmp.path(), "turn/interrupt").is_empty(),
            "没有在跑的轮就不该发软打断帧(server 侧必然 expectedTurnId 失配)"
        );
        assert!(
            wait_until(Duration::from_secs(3), || !client.is_running()),
            "SIGINT 之后会话进程应收敛(下条消息经 thread/resume 懒重连接回上下文)"
        );
        let _ = pgid;
        pool.evict(&agent.name, TAG);
    }

    /// 控制卡「⏸ 打断」在 Rpc 模式下的接线证明:chat 线程 select 循环拿到的把手就是
    /// `on_spawn` 发布的这一个,调它 → 发 `turn/interrupt`(**软**打断)→ 本轮收敛成 aborted,
    /// 而**会话进程仍活着**,下一轮直接复用。
    ///
    /// 这条如果错成硬打断,`!` 每按一次就杀掉一个会话进程 —— 不报错、不丢回复,只是每轮都要重连,
    /// 正是那种「编译通过、语义静默归错边」的失败。
    #[test]
    fn bang_interrupt_goes_soft_and_keeps_the_session_alive_for_the_next_turn() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"x"}}';
        (i=1; while [ $i -lt 8 ]; do sleep 0.05; printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"x"}}'; i=$((i+1)); done) & ;;
    *'turn/interrupt'*) printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"aborted"}}}' ;;
"#;
        let (agent, tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let (tx, rx) = std::sync::mpsc::channel::<TurnStopper>();

        // 线程 B:拿到把手就打断(模拟 chat 线程的 `poll_force_interrupt`)。
        let interrupter = std::thread::spawn(move || {
            let stopper = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("应发布把手");
            // 等这一轮真的起来(轮未起时打断是另一条分支,由 rpc_pool 的 Idle 用例覆盖)。
            assert!(
                wait_until(Duration::from_secs(5), || stopper.has_active_turn()),
                "turn/start 之后把手应能看到在跑的轮"
            );
            stopper.interrupt();
            stopper.killer().pgid()
        });

        let ctx = TurnCtx::default();
        let started = Instant::now();
        let out = run_turn(
            &pool,
            &spec(&agent),
            "数到 100",
            None,
            &ctx,
            &mut |_b| {},
            &mut |_s| {},
            &mut |_t| {},
            &mut |_e| {},
            &mut |stopper| {
                let _ = tx.send(stopper.clone());
            },
            &mut |_s| {},
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "软打断后本轮必须收敛(这条 fixture 只有收到 turn/interrupt 才发终态),实测 {:?}",
            started.elapsed()
        );
        assert!(out.session_id.is_some());

        let pgid = interrupter.join().unwrap();
        assert_eq!(
            sent(tmp.path(), "turn/interrupt").len(),
            1,
            "必须真的发出 turn/interrupt(软打断),而不是直接 SIGINT"
        );
        assert!(
            crate::runtime::inflight::group_alive(pgid),
            "软打断绝不能把会话进程一起带走 —— 带走了下一轮就得重连"
        );

        // 「下一轮直接复用同一进程」由 `primer_is_injected_once_at_cold_start_not_every_turn`
        // 单独钉住(那条不涉及打断,时序干净)。这里只证「软打断没把进程带走」。
        pool.evict(&agent.name, TAG);
    }

    /// **能力闸**:`can_steer` 决定决策侧敢不敢把一条消息 durable claim 走。
    /// 无头一次性子进程(`TurnStopper::child`)那条路恒 false —— 一轮一个进程、stdin 早关,
    /// 没有任何往里追加的通路。判错的后果不是"steer 失败"这么轻:决策侧会先 `mark_started`
    /// 把消息从队列取走,发现发不出去时已经退不回去了。
    #[test]
    fn a_headless_child_handle_never_claims_steer_capability() {
        let stopper = TurnStopper::child(crate::runtime::executor::ChildKiller::new(990_301));
        assert!(!stopper.can_steer(), "无头一次性子进程没有 steer 通路");
        assert_eq!(
            stopper.steer("追加一句"),
            RpcSteerOutcome::Unsupported,
            "没有能力就必须明说 Unsupported,绝不能返回 Sent 让调用方误销消息"
        );
    }

    /// 原生提问等待是合法的长静默，不应被 240s stall 当成挂死；同一把 turn handle 也必须在
    /// 打断发生前先发布取消信号，让阻塞在 Core gate 的 worker 有机会自行收尾。
    #[test]
    fn stopper_exposes_waiting_state_and_interrupt_publishes_cancellation() {
        use std::sync::atomic::Ordering;

        let stopper = TurnStopper::child(crate::runtime::executor::ChildKiller::new(990_302));
        assert!(!stopper.is_waiting_for_user());
        stopper
            .waiting_for_user_flag()
            .store(true, Ordering::Release);
        assert!(stopper.is_waiting_for_user());
        assert!(!stopper.cancellation_flag().load(Ordering::Acquire));

        stopper.interrupt();
        assert!(
            stopper.cancellation_flag().load(Ordering::Acquire),
            "打断必须先唤醒 native ask waiter"
        );

        let externally_cancelled =
            TurnStopper::child(crate::runtime::executor::ChildKiller::new(990_303));
        externally_cancelled.killer().interrupt();
        assert!(
            externally_cancelled
                .cancellation_flag()
                .load(Ordering::Acquire),
            "daemon stop 只持 ChildKiller，也必须唤醒同一 turn 的 native ask waiter"
        );
    }

    #[test]
    fn a_reused_rpc_process_does_not_inherit_the_previous_turns_cancellation() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(PONG_TURN);
        let pool = RpcPool::new(InFlight::new());
        let client = pool
            .checkout(&crate::capture::rpc_pool::RpcChat {
                agent: &agent,
                session: SESSION,
                tag: TAG,
                chat_id: "oc_test",
                cwd: None,
                primer: None,
                model: None,
                service_tier: None,
                cursor_mode: None,
                codex_policy: Default::default(),
                register_platform_tools: false,
            })
            .unwrap();
        let killer = client.killer();
        killer.cancel_waiters();
        assert!(killer.cancellation_flag().load(Ordering::Acquire));

        let next_turn = TurnStopper::rpc(killer, client.interrupt_handle());
        assert!(
            !next_turn.cancellation_flag().load(Ordering::Acquire),
            "会话进程可复用，但上一轮的逻辑取消位不能跨 turn 泄漏"
        );
        pool.evict(&agent.name, TAG);
    }

    /// 一轮只有控制面通知,渲染面零产出:started + rateLimits + 未映射方法 + 终态,
    /// 外加一条**非本轮**的尾随 delta(turn-999,必须被跨轮隔离闸丢掉、不计活性)。
    const CONTROL_PLANE_TURN: &str = r#"
    *'thread/start'*|*'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"account/rateLimits/updated","params":{"rateLimits":{"secondary":{"usedPercent":17,"windowMinutes":10080}}}}';
        printf '%s\n' '{"method":"experimental/heartbeat","params":{}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-999","itemId":"x","delta":"stale"}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;

    /// 控制面通知(rateLimits / 未映射方法)不产生任何 TurnUpdate,但每条都要敲一次活性计数;
    /// 上一轮的尾随事件被隔离闸丢掉,**不许**给本轮抬档。chat 线程的静默看门狗靠这份计数
    /// 区分「流活着但没有可渲染产出」与「彻底没声」
    /// (裁决在 `chat_thread::EngineLiveness` 与 `headless_stall_bound`;误杀现场:2026-08-31)。
    #[test]
    fn control_plane_notifications_beat_the_liveness_counter() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(CONTROL_PLANE_TURN);
        let pool = RpcPool::new(InFlight::new());
        let mut stopper: Option<TurnStopper> = None;
        run_turn(
            &pool,
            &spec(&agent),
            "ping",
            None,
            &TurnCtx::default(),
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut |s| stopper = Some(s.clone()),
            &mut |_| {},
        )
        .unwrap();
        let stopper = stopper.expect("on_spawn 必须在 checkout 后立即发布把手");
        assert_eq!(
            stopper.liveness_beats(),
            4,
            "turn/started + rateLimits + 未映射方法 + turn/completed 各敲一次;turn-999 的尾随 delta 不计"
        );
    }

    // ---- cursor ACP 分支 ----

    #[test]
    fn rpc_completed_snapshot_replaces_the_current_delta_segment() {
        let mut accum = TurnAccum::new("snapshot-test");
        for (text, partial) in [("abc", true), ("ab", true), ("corrected", false)] {
            let mut message = AgentMessage::of(AgentKind::Assistant);
            message.text = Some(text.to_string());
            message.partial = partial;
            accum.apply(&message, &mut |_| {}, &mut |_| {}, &mut |_| {});
        }
        assert_eq!(
            accum.finish(&mut |_| {}).reply.as_deref(),
            Some("corrected")
        );
    }

    #[test]
    fn cursor_preserves_repeated_delta_bytes_in_final_result() {
        let _guard = process_test_guard();
        for chunks in [
            vec!["__SUMMON_S", "ILENT_EXIT", "__"],
            vec!["哈", "哈", "哈哈"],
            vec!["__SUMMON_SILENT_EXIT__", "\n"],
            vec!["__SUMMON_SILENT_EXIT__", "__SUMMON_SILENT_EXIT__"],
        ] {
            let updates = chunks
                .iter()
                .map(|text| {
                    let update = serde_json::json!({
                        "jsonrpc": "2.0", "method": "session/update",
                        "params": {"sessionId": "sess-1", "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": text}
                        }}
                    });
                    format!("printf '%s\\n' '{update}';")
                })
                .collect::<String>();
            let cases = format!(
                r#"
    *'session/new'*|*'session/load'*) printf '%s\n' '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"sessionId":"sess-1"}}}}' ;;
    *'session/prompt'*) {updates}
        printf '%s\n' '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"stopReason":"end_turn"}}}}' ;;
"#
            );
            let (agent, _tmp) = cursor_agent_with_fake_server(&cases);
            let pool = RpcPool::new(InFlight::new());
            let (out, sink) = run(&pool, &agent, "probe", None);
            let expected = chunks.concat();
            assert_eq!(out.reply.as_deref(), Some(expected.as_str()));
            assert_eq!(sink.message.as_deref(), Some(expected.as_str()));
            assert_eq!(sink.streams.last(), Some(&expected));
            assert!(!out.interrupted);
            assert!(out.api_error.is_none());
            assert!(pool.evict(&agent.name, TAG));
        }
    }

    fn cursor_agent_with_fake_server(cases: &str) -> (Agent, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_acp_server(tmp.path(), cases);
        // cursor 默认通道已是 CliSession:走 ACP 路的测试 agent 必须显式 `capture: rpc`,
        // 否则 resume 通道键(resume_cli_kind)落在 CLI 侧,接回路径全部退化成冷启。
        let yaml = format!(
            "name: test\nruntime:\n  cli: cursor\n  capture: rpc\n  cli_cmd: {}\n",
            script.to_string_lossy()
        );
        let mut agent: Agent = serde_yaml::from_str(&yaml).unwrap();
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串 resume 记录。
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        (agent, tmp)
    }

    /// 一轮 ACP:纯 delta 流 + 工具段边界 + 终态来自 prompt 的**响应**。
    ///
    /// 断言的是「与 exec 路同一张卡」:打字机逐字增长、工具把前后文本切成两段、终态回复是
    /// 累积正文(ACP 没有 `result` 文本字段)。段规则共用 `subprocess::apply`,所以这条同时
    /// 证明了两条通道在 `AgentMessage` 之上零分叉。
    const ACP_TURN: &str = r#"
    *'session/new'*|*'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"session_info_update","title":"T"}}}';
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Step one done."}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"`echo hi`","kind":"execute","status":"pending","rawInput":{"command":"echo hi"}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed"}}}';
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Step two done."}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"usage_update","used":53000,"size":200000}}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}' ;;
"#;

    /// 一轮只有控制面更新,渲染面零产出:session_info + usage + 终态响应。
    const ACP_CONTROL_PLANE_TURN: &str = r#"
    *'session/new'*|*'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"session_info_update","title":"T"}}}';
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"usage_update","used":53000,"size":200000}}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}' ;;
"#;

    /// cursor ACP 侧同一约定:session_info / usage 这类不产生 TurnUpdate 的控制面更新
    /// 也要逐条敲活性计数(codex 侧的对应测试见
    /// `control_plane_notifications_beat_the_liveness_counter`)。
    #[test]
    fn cursor_control_plane_updates_beat_the_liveness_counter() {
        let _guard = process_test_guard();
        let (agent, _tmp) = cursor_agent_with_fake_server(ACP_CONTROL_PLANE_TURN);
        let pool = RpcPool::new(InFlight::new());
        let mut stopper: Option<TurnStopper> = None;
        run_turn(
            &pool,
            &spec(&agent),
            "ping",
            None,
            &TurnCtx::default(),
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut |s| stopper = Some(s.clone()),
            &mut |_| {},
        )
        .unwrap();
        let stopper = stopper.expect("on_spawn 必须在 checkout 后立即发布把手");
        assert_eq!(
            stopper.liveness_beats(),
            3,
            "session_info + usage + 终态响应,各敲一次"
        );
    }

    /// `session/new` 回帧后关掉 stdin 但短暂保持进程/stdout，把 `session/prompt` 的
    /// BrokenPipe 窗口做成确定性。这是“SID 已知，prompt 帧尚未写出”的精确边界。
    const ACP_CLOSES_STDIN_BEFORE_PROMPT: &str = r#"
    *'session/new'*) exec 0<&-; printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}'; sleep 5; exit 0 ;;
"#;

    /// prompt 帧已写出，但 server 随即用错误终止轮次。SID 必须在读这份响应前就发布。
    const ACP_PROMPT_RESPONSE_FAILS: &str = r#"
    *'session/new'*|*'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32603,"message":"prompt failed"}}' ;;
"#;

    #[test]
    fn cursor_prompt_write_failure_does_not_publish_an_empty_sid_across_pool_restart() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(ACP_CLOSES_STDIN_BEFORE_PROMPT);
        let pool = RpcPool::new(InFlight::new());
        let mut published = Vec::new();
        let mut failed_pgid = None;
        let error = run_turn(
            &pool,
            &spec(&agent),
            "hello",
            Some("IDENTITY"),
            &TurnCtx::default(),
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut |stopper| failed_pgid = Some(stopper.killer().pgid()),
            &mut |sid| {
                published.push(sid.to_string());
                crate::session::resume_store::write(
                    &agent,
                    SESSION,
                    &agent.resume_cli_kind(),
                    sid,
                    None,
                    None,
                );
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("session/prompt"));
        assert!(published.is_empty());
        assert!(crate::session::resume_store::read_record(&agent, SESSION).is_none());
        assert!(pool.evict(&agent.name, TAG));
        let failed_pgid = failed_pgid.unwrap();
        assert!(wait_until(Duration::from_secs(3), || {
            !crate::runtime::inflight::group_alive(failed_pgid)
        }));
        drop(pool);

        // 模拟 daemon 重启后的新进程：同一 agent/session 仍无可 load 的空 SID，
        // 所以必须再走 session/new，并把 PRIMER 放进真正的首条 prompt。
        write_fake_acp_server(tmp.path(), ACP_TURN);
        let restarted = RpcPool::new(InFlight::new());
        let (outcome, sink) = run(&restarted, &agent, "hello", Some("IDENTITY"));
        assert_eq!(outcome.session_id.as_deref(), Some("sess-1"));
        assert_eq!(sink.sid.as_deref(), Some("sess-1"));
        assert_eq!(sent(tmp.path(), "session/new").len(), 2);
        assert!(sent(tmp.path(), "session/load").is_empty());
        let prompts = sent(tmp.path(), "session/prompt");
        assert_eq!(prompts.len(), 1);
        assert_eq!(
            prompts[0]["params"]["prompt"][0]["text"].as_str(),
            Some("IDENTITY\n\nhello")
        );
        assert!(restarted.evict(&agent.name, TAG));
    }

    #[test]
    fn cursor_publishes_sid_after_prompt_write_even_when_the_response_fails() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(ACP_PROMPT_RESPONSE_FAILS);
        let pool = RpcPool::new(InFlight::new());
        let mut published = Vec::new();
        let outcome = run_turn(
            &pool,
            &spec(&agent),
            "hello",
            Some("IDENTITY"),
            &TurnCtx::default(),
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut |sid| {
                published.push(sid.to_string());
                crate::session::resume_store::write(
                    &agent,
                    SESSION,
                    &agent.resume_cli_kind(),
                    sid,
                    None,
                    None,
                );
            },
        )
        .unwrap();

        assert!(outcome.api_error.is_some());
        assert_eq!(published, ["sess-1"]);
        assert_eq!(sent(tmp.path(), "session/prompt").len(), 1);
        assert_eq!(
            crate::session::resume_store::read_record(&agent, SESSION).map(|record| record.id),
            Some("sess-1".to_string())
        );
        assert!(pool.evict(&agent.name, TAG));
    }

    #[test]
    fn cursor_turn_streams_and_reports_the_same_shape_as_the_subprocess_path() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(ACP_TURN);
        let pool = RpcPool::new(InFlight::new());
        assert_eq!(
            crate::runtime::daemon::cursor_context_budget_from_pool(&pool, &agent.name, TAG),
            Default::default(),
            "ACP 未报告 usage 时不得猜上下文"
        );

        let mut sink = Sink {
            message: None,
            streams: Vec::new(),
            events: Vec::new(),
            sid: None,
            stopper_pgid: None,
        };
        let ctx = TurnCtx {
            model: Some("composer-2.5"),
            ..Default::default()
        };
        let out = run_turn(
            &pool,
            &spec(&agent),
            "两步走",
            Some("你是测试体"),
            &ctx,
            &mut |body| sink.message = Some(body.to_string()),
            &mut |acc| sink.streams.push(acc.to_string()),
            &mut |_tool| {},
            &mut |ev| sink.events.push(ev.clone()),
            &mut |stopper| sink.stopper_pgid = Some(stopper.killer().pgid()),
            &mut |sid| sink.sid = Some(sid.to_string()),
        )
        .unwrap();

        assert_eq!(out.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            sink.sid.as_deref(),
            Some("sess-1"),
            "sid 应在轮开始时就回传"
        );
        assert!(
            sink.stopper_pgid.unwrap_or(0) > 0,
            "把手应紧随 checkout 发布"
        );
        // 工具是段边界:前后两段用 "\n\n" 分隔(与 exec 路的 cursor 流同规则)。
        assert_eq!(
            out.reply.as_deref(),
            Some("Step one done.\n\nStep two done."),
            "ACP 没有 result 文本字段,终态回复来自累积正文"
        );
        assert_eq!(
            sink.message.as_deref(),
            Some("Step one done.\n\nStep two done.")
        );
        assert!(
            sink.events
                .iter()
                .any(|e| e.kind == AgentKind::ToolUse && e.tool.as_deref() == Some("shell")),
            "ACP 的 kind=execute 必须归一成 exec 路的 shell(否则卡片标签会漂)"
        );
        // 打字机:delta 逐字增长,工具后另起一段。
        assert_eq!(
            sink.streams,
            vec![
                "Step one done.".to_string(),
                "Step one done.\n\nStep two done.".to_string()
            ]
        );
        assert_eq!(
            pool.current_ctx_budget(&agent.name, TAG),
            Some((53_000, 200_000)),
            "Cursor usage_update 必须进入跨轮会话快照"
        );
        let context =
            crate::runtime::daemon::cursor_context_budget_from_pool(&pool, &agent.name, TAG);
        let badge = summon_core::feishu::TurnBadge {
            cli: "cursor".into(),
            model: Some("composer-2.5".into()),
            ctx: context,
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(badge.contains("53k/200k"));
        assert!(badge.contains("26%"));
        let card = summon_core::feishu::card_session_panel(
            TAG,
            "cursor",
            "RPC · ACP 常驻",
            &summon_core::model_effort::resolve_model(
                None,
                Some("composer-2.5"),
                &summon_core::model_effort::DefaultModel::default(),
            ),
            &summon_core::model_effort::resolve_effort("cursor", None),
            &[],
            &[],
            &summon_core::subagent::SubagentDisplay::default(),
            summon_core::feishu::SessionPanelControls {
                context,
                ..Default::default()
            },
        );
        assert!(card.contains("53k / 200k"));
        assert!(card.contains("26%"));

        // PRIMER **不**走原生位置:ACP 的 `session/new` 只收 `{cwd, mcpServers}`,没有
        // baseInstructions 同类字段,不该发明一个。
        let opens = sent(tmp.path(), "session/new");
        assert_eq!(opens.len(), 1);
        assert!(
            !opens[0].to_string().contains("你是测试体"),
            "ACP 没有 PRIMER 的原生位置,不该发明一个"
        );
        // 冷启轮由**引擎**把 PRIMER 前置进首条真正投递的 prompt(见 `compose_prompt`)；
        // 模型已在 checkout 阶段经 config setter 收敛，不进 prompt 帧。
        let prompts = sent(tmp.path(), "session/prompt");
        assert_eq!(
            prompts[0]["params"]["prompt"][0]["text"],
            "你是测试体\n\n两步走"
        );
        assert!(prompts[0]["params"].get("model").is_none());
    }

    /// cursor 的打断:把手发的是 `session/cancel`(**软**打断),本轮收敛成 cancelled,
    /// 而**会话进程仍活着**。与 codex 那条同构 —— 错成硬打断就是每按一次 `!` 杀一个会话进程。
    #[test]
    fn cursor_acp_turn_reads_native_session_store_when_usage_update_is_absent() {
        let _guard = process_test_guard();
        let cases: String = ACP_TURN
            .lines()
            .filter(|line| !line.contains("usage_update"))
            .collect::<Vec<_>>()
            .join("\n");
        let (agent, tmp) = cursor_agent_with_fake_server(&cases);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        crate::capture::cursor_context::write_test_acp_snapshot(
            &agent
                .state_dir()
                .join(crate::capture::cursor_config::CURSOR_ACP_SESSIONS_DIR_NAME),
            "sess-1",
            52_000,
            1_000_000,
        );
        let pool = RpcPool::new(InFlight::new());
        let (out, _) = run(&pool, &agent, "ping", None);
        assert_eq!(out.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            pool.current_ctx_budget(&agent.name, TAG),
            Some((52_000, 1_000_000)),
            "ACP 未推 usage_update 时必须从本地会话库读取上下文占用与窗口"
        );
        crate::capture::cursor_context::write_test_acp_snapshot(
            &agent
                .state_dir()
                .join(crate::capture::cursor_config::CURSOR_ACP_SESSIONS_DIR_NAME),
            "sess-1",
            88_000,
            1_000_000,
        );
        let (second, _) = run(&pool, &agent, "pong", None);
        assert_eq!(second.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            pool.current_ctx_budget(&agent.name, TAG),
            Some((88_000, 1_000_000)),
            "复用同一常驻 ACP 进程的后续轮必须在轮末刷新池内上下文预算"
        );
        let badge = crate::runtime::daemon::completion_badge_with_session_for_test(
            &summon_core::feishu::TurnBadge {
                cli: "cursor".into(),
                ..Default::default()
            },
            &agent.name,
            &agent.state_dir(),
            TAG,
            "sess-1",
        );
        assert_eq!(badge.ctx.used_tokens, Some(88_000));
        assert_eq!(badge.ctx.window_tokens, Some(1_000_000));
        assert_eq!(badge.ctx.used_pct(), Some(8));
        let _ = tmp;
        assert!(pool.evict(&agent.name, TAG));
    }

    #[test]
    fn cursor_bang_interrupt_goes_soft_and_keeps_the_session_alive() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*|*'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) prompt_id=$id;
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x"}}}}';
        (i=1; while [ $i -lt 8 ]; do sleep 0.05; printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x"}}}}'; i=$((i+1)); done) & ;;
    *'session/cancel'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$prompt_id"',"result":{"stopReason":"cancelled"}}' ;;
"#;
        let (agent, tmp) = cursor_agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let (tx, rx) = std::sync::mpsc::channel::<TurnStopper>();

        let interrupter = std::thread::spawn(move || {
            let stopper = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("应发布把手");
            // 等这一轮真的起来(轮未起时打断是另一条分支,由 rpc_pool 的 Idle 用例覆盖)。
            assert!(
                wait_until(Duration::from_secs(5), || stopper.has_active_turn()),
                "session/prompt 之后把手应能看到在跑的轮"
            );
            stopper.interrupt();
            stopper.killer().pgid()
        });

        let ctx = TurnCtx::default();
        let started = Instant::now();
        let out = run_turn(
            &pool,
            &spec(&agent),
            "数到 100",
            None,
            &ctx,
            &mut |_b| {},
            &mut |_s| {},
            &mut |_t| {},
            &mut |_e| {},
            &mut |stopper| {
                let _ = tx.send(stopper.clone());
            },
            &mut |_s| {},
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "软打断后本轮必须收敛(这条 fixture 只有收到 cancel 才发终态),实测 {:?}",
            started.elapsed()
        );
        assert!(out.session_id.is_some());

        let pgid = interrupter.join().unwrap();
        let cancels = sent(tmp.path(), "session/cancel");
        assert_eq!(
            cancels.len(),
            1,
            "必须真的发出 session/cancel,而不是直接 SIGINT"
        );
        assert!(
            cancels[0].get("id").is_none(),
            "必须是 notification —— request 形式实测返回 -32601 且不打断"
        );
        assert!(
            crate::runtime::inflight::group_alive(pgid),
            "软打断绝不能把会话进程一起带走"
        );
        pool.evict(&agent.name, TAG);
    }

    /// server 拒了 `session/prompt`(-32602 之类):必须**上报错误**,而不是「空轮 DONE」。
    ///
    /// 客户端层早就把它当错误(`a_failing_prompt_request_surfaces_as_an_error`),但引擎层此前
    /// 把非超时错误一律 log+break 成 `Ok(partial)` —— 于是一轮什么都没跑成,卡上却是正常收尾。
    /// 这条钉的就是那一层。
    #[test]
    fn a_rejected_cursor_prompt_is_reported_as_an_error_not_an_empty_done() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/new'*|*'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32602,"message":"Invalid model value"}}' ;;
"#;
        let (agent, _tmp) = cursor_agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let (out, sink) = run(&pool, &agent, "ping", None);

        let error = out
            .api_error
            .expect("被拒的 prompt 不能当成正常完成 —— 那一轮压根没跑");
        assert!(
            error.contains("Invalid model value"),
            "错误里要带 server 给的原因,实得: {error}"
        );
        assert_eq!(out.reply, None, "什么都没生成就不该有回复");
        assert_eq!(sink.message, None);
        pool.evict(&agent.name, TAG);
    }

    /// **cursor 的 PRIMER 落点**:`session/load` 失败退冷启时,新会话必须拿到 PRIMER。
    ///
    /// 判据是**这次 checkout 的实际结果**而不是盘上有没有 sid —— 这条测里盘上明明有 sid
    /// (`sess-gone`),会话却是全新的。按 sid 判的旧写法在这里会静默漏注,症状是「某次重连之后
    /// agent 忽然不认得自己是谁」:不报错、不丢回复,只是人格没了。
    ///
    /// 顺带钉住失败路径的重放清理:load 前重放的那段历史不许进本轮回复。
    #[test]
    fn cursor_cold_start_after_a_failed_load_still_gets_the_primer() {
        let _guard = process_test_guard();
        let cases = r#"
    *'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-gone","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"上一轮的回复"}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32602,"message":"session not found"}}' ;;
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1"}}' ;;
    *'session/prompt'*) printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"本轮"}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}' ;;
"#;
        let (agent, tmp) = cursor_agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        // 盘上留一个已经失效的 sid:`session/load` 会失败,池退回 `session/new`。
        crate::session::resume_store::write(&agent, SESSION, "cursor-acp", "sess-gone", None, None);

        let (out, _) = run(&pool, &agent, "接着说", Some("你是测试体"));

        let prompts = sent(tmp.path(), "session/prompt");
        let text = prompts[0]["params"]["prompt"][0]["text"].as_str().unwrap();
        assert_eq!(
            text, "你是测试体\n\n接着说",
            "退冷启的新会话必须补上 PRIMER(ACP 没有 baseInstructions,只能前置)"
        );
        assert_eq!(
            out.reply.as_deref(),
            Some("本轮"),
            "load 失败前重放的历史不许进本轮回复"
        );
        pool.evict(&agent.name, TAG);
    }

    /// 反向钉:`session/load` **成功**(接回上下文)那一轮绝不重注 PRIMER —— 上下文里已经有了,
    /// 重注既污染对话又伤 provider 缓存。与上一条一起把「按实际 checkout 结果分流」两边都盖住。
    #[test]
    fn cursor_resumed_turn_does_not_reinject_the_primer() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(ACP_TURN);
        let pool = RpcPool::new(InFlight::new());
        crate::session::resume_store::write(&agent, SESSION, "cursor-acp", "sess-1", None, None);

        run(&pool, &agent, "接着说", Some("你是测试体"));

        assert_eq!(sent(tmp.path(), "session/load").len(), 1, "该走接回");
        let prompts = sent(tmp.path(), "session/prompt");
        assert_eq!(
            prompts[0]["params"]["prompt"][0]["text"], "接着说",
            "接回轮的首条 prompt 必须一字不多"
        );
    }

    /// **真机验收 ①(消耗额度)**:codex chat 一轮经 RPC 引擎端到端 —— 走的是生产入口
    /// `capture::rpc::run_turn`(池 + PRIMER + per-turn model/effort),不是裸客户端。
    #[test]
    #[ignore = "需要本机已登录 codex;跑法 cargo test -p summon -- --ignored real_codex_rpc_engine_replies"]
    fn real_codex_rpc_engine_replies() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: rpc-e2e\nruntime:\n  cli: codex\n").unwrap();
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串 resume 记录。
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        assert_eq!(
            agent.capture_mode(),
            summon_core::registry::CaptureMode::Rpc,
            "codex 默认就该走原生协议"
        );
        let pool = RpcPool::new(InFlight::new());
        let started = Instant::now();
        let (out, sink) = run_e2e(&pool, &agent, "reply with exactly the single word: pong");
        eprintln!("[real] RPC 一轮耗时 {:?}", started.elapsed());
        assert!(out.session_id.is_some(), "应捕获 thread id");
        assert!(
            out.reply
                .unwrap_or_default()
                .to_lowercase()
                .contains("pong"),
            "回复应含 pong"
        );
        assert!(!sink.streams.is_empty(), "应有流式增量(否则打字机是死的)");
        pool.evict(&agent.name, TAG);
    }

    /// **真机验收 ②(消耗额度)**:杀掉会话进程后,下一轮 checkout 走 `thread/resume` 把上下文接回来
    /// —— 暗号只存在于已死那个进程跑过的轮里,答得出就证明懒重连没丢上下文。
    #[test]
    #[ignore = "需要本机已登录 codex;跑法 cargo test -p summon -- --ignored real_codex_rpc_resume_after_kill"]
    fn real_codex_rpc_resume_after_kill() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: rpc-resume\nruntime:\n  cli: codex\n").unwrap();
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串 resume 记录。
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let pool = RpcPool::new(InFlight::new());

        let first = Instant::now();
        let (out, _) = run_e2e(
            &pool,
            &agent,
            "Remember the passphrase `orchid-42`. Reply with exactly: ok",
        );
        eprintln!("[real] 冷启轮 {:?} → {:?}", first.elapsed(), out.reply);

        // 拆掉会话进程(模拟 daemon 软重启 / app-server 崩)。
        let client = pool
            .checkout(&RpcChat {
                agent: &agent,
                session: SESSION,
                tag: TAG,
                chat_id: "oc_test",
                cwd: None,
                primer: None,
                model: None,
                service_tier: None,
                cursor_mode: None,
                codex_policy: Default::default(),
                register_platform_tools: false,
            })
            .unwrap();
        let pgid = client.pgid();
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!client.is_running(), "会话进程应已死");
        drop(client);

        let second = Instant::now();
        let (revived, _) = run_e2e(
            &pool,
            &agent,
            "What was the passphrase? Reply with it only.",
        );
        let elapsed = second.elapsed();
        let reply = revived.reply.unwrap_or_default();
        eprintln!("[real] 懒重连轮 {elapsed:?} → {reply:?}");
        assert!(
            reply.to_lowercase().contains("orchid-42"),
            "resume 应接回上下文,实得: {reply}"
        );
        pool.evict(&agent.name, TAG);
    }

    /// **真机验收 ①(消耗 cursor 额度)**:cursor chat 一轮经 ACP 引擎端到端 —— 走的是生产入口
    /// `capture::rpc::run_turn`(池 + `session/new`),不是裸客户端。
    #[test]
    #[ignore = "需要本机已登录 cursor-agent;跑法 cargo test -p summon -- --ignored real_cursor_acp_engine_replies"]
    fn real_cursor_acp_engine_replies() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: acp-e2e\nruntime:\n  cli: cursor\n  capture: rpc\n")
                .unwrap();
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串 resume 记录。
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        assert_eq!(
            agent.capture_mode(),
            summon_core::registry::CaptureMode::Rpc,
            "显式 capture: rpc 才走 ACP 降级通道(cursor 默认已是 CliSession)"
        );
        let pool = RpcPool::new(InFlight::new());
        let started = Instant::now();
        let (out, sink) = run_e2e(&pool, &agent, "reply with exactly the single word: pong");
        eprintln!("[real] cursor ACP 一轮耗时 {:?}", started.elapsed());
        assert!(out.session_id.is_some(), "应捕获 sessionId");
        assert!(
            out.reply
                .unwrap_or_default()
                .to_lowercase()
                .contains("pong"),
            "回复应含 pong"
        );
        assert!(!sink.streams.is_empty(), "应有流式增量(否则打字机是死的)");
        let sid = out.session_id.as_deref().unwrap();
        let badge = crate::runtime::daemon::completion_badge_with_session_for_test(
            &summon_core::feishu::TurnBadge {
                cli: "cursor".into(),
                ..Default::default()
            },
            &agent.name,
            &agent.state_dir(),
            TAG,
            sid,
        );
        assert!(badge.ctx.used_tokens.is_some_and(|used| used > 0));
        assert!(badge.ctx.window_tokens.is_some_and(|window| window > 0));
        eprintln!(
            "[real] cursor ACP ctx={:?} badge={:?}",
            badge.ctx,
            badge.render()
        );
        pool.evict(&agent.name, TAG);
    }

    #[test]
    #[ignore = "需要本机已登录 cursor-agent，验证 ACP 静默结果原文"]
    fn real_cursor_acp_preserves_silent_result() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        // 固定模型使传输测试的正文只来自指定 provider，避免 Auto 路由提示参与结果。
        let mut agent: Agent = serde_yaml::from_str(
            "name: acp-silent-e2e\nruntime:\n  cli: cursor\n  capture: rpc\n  rpc_model: claude-opus-5-5\n",
        )
        .unwrap();
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        let pool = RpcPool::new(InFlight::new());
        let (out, sink) = run_e2e(
            &pool,
            &agent,
            "This is a transport test. Reply with exactly __SUMMON_SILENT_EXIT__ and nothing else. Do not use tools or add whitespace, a newline, or formatting.",
        );
        assert!(pool.evict(&agent.name, TAG));
        let keyword = summon_core::messaging::result_control::SILENT_RESET_KEYWORD;
        assert_eq!(out.reply.as_deref(), Some(keyword));
        assert_eq!(sink.message.as_deref(), Some(keyword));
        assert!(!out.interrupted);
        assert!(out.api_error.is_none());
    }

    /// **真机验收 ②(消耗 cursor 额度)**:杀掉会话进程后,下一轮 checkout 走 `session/load` 把
    /// 上下文接回来 —— 暗号只存在于已死那个进程跑过的轮里,答得出就证明懒重连没丢上下文。
    /// 同时验掉「历史重放不能被当成本轮回复」那条(答案里不该整段复读上一轮)。
    #[test]
    #[ignore = "需要本机已登录 cursor-agent;跑法 cargo test -p summon -- --ignored real_cursor_acp_load_after_kill"]
    fn real_cursor_acp_load_after_kill() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: acp-resume\nruntime:\n  cli: cursor\n  capture: rpc\n")
                .unwrap();
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串 resume 记录。
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let pool = RpcPool::new(InFlight::new());

        let first = Instant::now();
        let (out, _) = run_e2e(
            &pool,
            &agent,
            "Remember the passphrase `orchid-42`. Reply with exactly: ok",
        );
        eprintln!("[real] 冷启轮 {:?} → {:?}", first.elapsed(), out.reply);

        // 拆掉会话进程(模拟 daemon 软重启 / server 崩)。
        let client = pool
            .checkout(&RpcChat {
                agent: &agent,
                session: SESSION,
                tag: TAG,
                chat_id: "oc_test",
                cwd: None,
                primer: None,
                model: None,
                service_tier: None,
                cursor_mode: None,
                codex_policy: Default::default(),
                register_platform_tools: false,
            })
            .unwrap();
        let pgid = client.pgid();
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!client.is_running(), "会话进程应已死");
        drop(client);

        let second = Instant::now();
        let (revived, _) = run_e2e(
            &pool,
            &agent,
            "What was the passphrase? Reply with it only.",
        );
        let elapsed = second.elapsed();
        let reply = revived.reply.unwrap_or_default();
        eprintln!("[real] 懒重连轮 {elapsed:?} → {reply:?}");
        assert!(
            reply.to_lowercase().contains("orchid-42"),
            "session/load 应接回上下文,实得: {reply}"
        );
        assert!(
            !reply.contains("Remember the passphrase"),
            "历史重放必须在 load 时丢掉,不能当成本轮回复,实得: {reply}"
        );
        pool.evict(&agent.name, TAG);
    }

    #[cfg(test)]
    fn run_e2e(pool: &RpcPool, agent: &Agent, prompt: &str) -> (TurnOutcome, Sink) {
        let mut sink = Sink {
            message: None,
            streams: Vec::new(),
            events: Vec::new(),
            sid: None,
            stopper_pgid: None,
        };
        let ctx = TurnCtx::default();
        let out = run_turn(
            pool,
            &spec(agent),
            prompt,
            None,
            &ctx,
            &mut |body| sink.message = Some(body.to_string()),
            &mut |acc| sink.streams.push(acc.to_string()),
            &mut |_tool| {},
            &mut |ev| sink.events.push(ev.clone()),
            &mut |_k| {},
            &mut |sid| {
                sink.sid = Some(sid.to_string());
                crate::session::resume_store::write(
                    agent,
                    SESSION,
                    &agent.resume_cli_kind(),
                    sid,
                    None,
                    None,
                );
            },
        )
        .unwrap();
        (out, sink)
    }
}
