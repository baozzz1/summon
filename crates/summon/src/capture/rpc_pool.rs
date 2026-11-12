//! per-chat 的原生协议会话池:懒建、懒重连、跨线程打断把手、`InFlight` 登记。
//!
//! 两种协议共池:codex `app-server`(`thread/start` / `thread/resume`)与 cursor ACP
//! (`session/new` / `session/load`)。差异只有「怎么起进程、怎么开会话、怎么软打断」三件事,
//! 全部收在 [`SessionProto`] 一个枚举上集中分派;生命周期(懒建/懒重连/InFlight/回收)与键空间
//! 对两边同构。SID 事实源仍唯一，但 Cursor cold start 必须等首条 prompt 写出才提交，不能提前落空会话。
//!
//! 形态由设计文档 §7 拍板:**不常驻、懒重连**。进程随 daemon 生死,上下文留在磁盘(codex rollout /
//! `~/.cursor/chats`),daemon 软重启后**不主动重连** —— 等某个 chat 的下一条消息来了,`checkout`
//! 才 spawn 新进程并接回去(实测 codex `thread/resume` ~1s、cursor `session/load` ~10s,而每轮冷启
//! 分别是 ~11s / ~31s,所以懒重连在任何情况下都不劣于现状)。这样 fleet 观测面不多出第三类常驻进程,
//! 孤儿回收、进程数上界、停机收敛全部为零成本。
//!
//! 三条不变量,改之前先读:
//! - **消费权单线程独占**。reader / 收事件只有 chat 线程碰([`PooledClient::lock`] 把这点变成类型上
//!   的事实);打断走另一条通路([`SoftInterrupt`],共享写端)。刻意不做全双工多路复用 ——
//!   每 chat 一个进程、一次一轮,复用换不来任何东西。
//! - **进程必须在 `InFlight` 里挂着**,否则 daemon 停机够不着它,直接变孤儿。键与「这一轮」的键
//!   **分开**(见 [`RPC_KEY_PREFIX`])。
//! - **sid 只有一个事实源**:`session::resume_store`(键=会话名,与 `.meta` 同键),与无头 / tmux 两路
//!   共用同一份记录,不另起存储 —— 这也是 `runtime.capture` 逃生门能随时切的前提。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use serde_json::{json, Value};
use summon_core::codex_quota::CodexRateLimitSnapshot;
use summon_core::driver::slash_pool::{RpcGoalCommand, RpcSlashCommand};
use summon_core::driver::{CliDriver, CodexServiceTier, CursorSessionMode};
use summon_core::pending::{PendingTurn, PendingTurnKind};
use summon_core::registry::{Agent, CaptureMode};

use crate::capture::cursor_acp::{
    AcpInterrupt, CursorAcpClient, CursorAcpConfig, CursorAcpConfigOption, CursorAcpError,
    CursorAcpMode,
};
use crate::codex::codex_app_server::{
    CodexAccountRateLimits, CodexAppServerClient, CodexAppServerConfig, RpcInterrupt,
};
use crate::observe::log::log;
use crate::runtime::executor::ChildKiller;
use crate::runtime::inflight::{group_alive, InFlight, InFlightTurn, TurnKey};
use crate::runtime::rpc_stdio::{RpcInterruptOutcome, RpcSteerOutcome};
use crate::session::resume_store;

/// 开会话(冷启 / 接回)的上界。比默认 request timeout 宽:接回要重读历史,冷启要跟服务端
/// 完整握手一轮 —— cursor `session/new` 实测 23-49s,且这一段**全程静默**,没有任何通知能
/// 用来判活,只能干等那一个应答。所以预算按最坏负载留:多 agent 并发把机器压到 load average
/// 30+ 时再翻一倍仍在界内,卡在这里重试一次代价更大。
const SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(240);
/// mode 是每轮 prompt 前的轻量本地 ACP 设置，不能沿用 `SESSION_SETUP_TIMEOUT` 那份开/接回
/// 会话的预算；超时后 fail-closed 并保留进程，避免一条配置请求长期占住 chat。
const CURSOR_MODE_SYNC_TIMEOUT: Duration = Duration::from_secs(10);
/// 后写的轴可能让 Cursor 静默回退先前轴。最多再重放一轮寻找 fixed point；两轮仍不 exact
/// 就视为不兼容组合并 fail-stop，不能无限占住 agent 事务锁。
const CURSOR_CONFIG_SYNC_MAX_PASSES: usize = 2;

/// 开会话时顺带问一次额度的上界。**刻意比 `SESSION_SETUP_TIMEOUT` 短一个量级**:这一项只是
/// 角标上的装饰,不值得让一条能用的会话为它多等 —— 超时即降级(不显示这项),不影响本轮。
const QUOTA_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 开会话时顺带问一次模型目录的超时。与额度同档:两者都是「问不到就降级」的观察项,
/// 不该让一条能用的会话为它们多等。
const MODEL_LIST_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 飞书控制指令是用户主动等待的轻量调用；比额度装饰宽，但不能沿用开会话的 `SESSION_SETUP_TIMEOUT`。
const RPC_CONTROL_TIMEOUT: Duration = Duration::from_secs(15);

/// 等一次 `/compact` 真的压缩完的硬顶。压缩是模型调用,不是普通控制请求 —— 真机
/// (claude 侧同类操作,76k 上下文)约 80s,给足余量。等不到就如实回执「已提交、未确认完成」。
const RPC_COMPACT_TIMEOUT: Duration = Duration::from_secs(300);

/// 收到 `thread/compacted` 之后再等用量推送的短窗。真机 claude 侧 statusLine 滞后 ~1.2s,
/// app-server 的 `tokenUsage/updated` 同为紧随其后的一条,不必为它挂满整个硬顶。
const RPC_COMPACT_USAGE_GRACE: Duration = Duration::from_secs(5);

/// `evict` 收敛一条会话进程的宽限:SIGINT 之后等这么久,仍在就 SIGKILL。
/// 与 `daemon::HEADLESS_INTERRUPT_GRACE` 同为 2s —— 都是「给自有进程组一次体面退出的机会」。
/// 池按自己的 `close_grace` 字段取用;生产只经 [`RpcPool::new`] 构造,恒为这个值。
const SESSION_CLOSE_GRACE: Duration = Duration::from_secs(2);

/// `InFlight` 里给 RPC **常驻进程**用的键前缀。
///
/// **为什么必须与轮键分开**:`InFlight` 键是 `(agent, tag)` 且同键至多一条,而 RPC 进程**跨轮存活**。
/// 若与「这一轮」共用一个键,轮结束时的 `finish` 会把进程的注册一并摘掉,daemon stop 就再也够不着
/// 它 —— 进程当场变孤儿。前缀把两种生命周期在键空间上隔开。
/// (chat tag 形如 `g<16hex>`/`p<16hex>`、worker tag 形如 `worker-…`,都撞不上 `rpc:`。)
const RPC_KEY_PREFIX: &str = "rpc:";

/// 正在收敛的会话用的键前缀,**每次 evict 一个新号**。
///
/// 为什么不复用 `rpc:<tag>`:evict 要把注销推迟到进程确认退出之后(否则中间有一段谁都够不着的
/// 窗口),可这条 chat 的下一条会话立刻就会来占 `rpc:<tag>` —— 同键至多一条,它会被拒。
/// 换一个唯一键让多条收敛身份都能留在表上；同一逻辑 chat 另有 `closing` fence，旧组证明 Gone
/// 之前 checkout 不会启动新 writer。
const RPC_CLOSING_KEY_PREFIX: &str = "rpc-closing:";

pub fn inflight_key(agent: &str, tag: &str) -> TurnKey {
    TurnKey::new(agent, format!("{RPC_KEY_PREFIX}{tag}"))
}

fn closing_key(agent: &str, tag: &str) -> TurnKey {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    TurnKey::new(agent, format!("{RPC_CLOSING_KEY_PREFIX}{tag}#{seq}"))
}

/// 一个 chat 在池里的身份 + 冷启上下文。
///
/// 各项都少不了:sid 记录按**会话名**键(与 `.meta` 同键)、迁移期旧无头存储按 **tag** 键、
/// `InFlight` 按 **(agent, tag)** 键、`cwd` 是开会话必须钉死的东西(server 自己的 cwd 可能停在 `$HOME`)。
pub struct RpcChat<'a> {
    pub agent: &'a Agent,
    pub session: &'a str,
    pub tag: &'a str,
    /// 本 chat 的飞书 chat_id。**由调用方直接传**,不从盘上 `.meta` 反查——反查是 best-effort,
    /// 读不到就静默缺失,子进程拿到半份身份反而更难查。
    pub chat_id: &'a str,
    pub cwd: Option<&'a str>,
    /// PRIMER。**池只管 codex 这一半**:冷启(`thread/start`)与 resume 都下发到
    /// `developerInstructions`(**追加**语义,不顶掉 codex 自己的系统提示词)。
    ///
    /// 为什么 resume 也要发:那个字段**不随 rollout 持久化**,不重发这个 thread 就没有身份。
    /// 这与旧形态(`baseInstructions`,替换语义 + 持久化,故只冷启发一次)恰好相反,
    /// 两者靠 `.session` 的 `primer_slot` 区分 —— 判据在 [`Self::start_codex`]。
    ///
    /// cursor 的 ACP **没有任何对应位置**(`session/new` 只收 `{cwd, mcpServers}`),
    /// 不发明协议里没有的形态 —— 它的 PRIMER 由引擎按本次 checkout 的结果前置进首条用户消息,
    /// 见 `capture::rpc::compose_prompt`。
    pub primer: Option<&'a str>,
    /// 本轮要用的模型。**只有把模型钉在进程上的 CLI(cursor)用得到**,见 [`spawn_pinned_model`]。
    pub model: Option<&'a str>,
    /// Codex per-chat 服务档位。Fast 需要 app-server 进程启用 `fast_mode`；Standard/None 不需要。
    pub service_tier: Option<CodexServiceTier>,
    /// codex 托管会话的权限策略(sandbox + approval)。由调用方按
    /// 「内置默认 → `~/.summon/config.yaml` 的 `codex:` → agent.yaml 的 `runtime.codex_*`」
    /// 解析好再传进来 —— 池不读配置(它拿不到全局 `Config`),也不在这里做降级。
    pub codex_policy: summon_core::codex_policy::CodexPolicy,
    /// Cursor ACP per-chat 会话模式。只走标准 `session/set_mode`，不触碰全局模型参数。
    pub cursor_mode: Option<&'a CursorSessionMode>,
    /// 这条会话冷启时要不要注册 summon 的平台工具。
    ///
    /// 判据是**调用方这一轮装没装配处理器**,不是「cli 是不是 codex」——注册了却没人处理,
    /// 模型的工具列表里就多出几个调了必失败的条目。灰度开关与通道判据都收在装配那一处。
    pub register_platform_tools: bool,
}

/// 一次 `checkout` 实际做了什么。观测 + 测试断言用 —— 「懒重连真的走了 resume」必须可被证明,
/// 而 cursor 的 PRIMER 该不该注也**只认这个值**(见 `capture::rpc::compose_prompt`),
/// 所以它必须如实反映真实发生的事,尤其是降级:接回失败退冷启时报 `ColdStart` 而不是 `Resumed`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkout {
    /// 进程还活着、cwd 没换且满足本轮 Fast feature 要求，直接复用(热路径)。
    /// 见 [`Session::usable_for`]。
    Reused,
    /// 开出来的是一个**全新会话**:盘上没有可用 sid,或有 sid 但接回失败(rollout / 聊天记录
    /// 被删)退了冷启。两种情形对上层是同一件事 —— 上下文是空的。
    ColdStart,
    /// 盘上有 sid、进程没了(或模型/cwd 换了)→ 新进程接回上下文。
    Resumed,
}

/// 池里一条会话背后的协议。**新增一种 RPC CLI 就在这里加一个变体**,别在调用点散 `match cli`。
enum SessionProto {
    Codex {
        client: Arc<Mutex<CodexAppServerClient>>,
        interrupt: RpcInterrupt,
    },
    Cursor {
        client: Arc<Mutex<CursorAcpClient>>,
        interrupt: AcpInterrupt,
    },
}

impl SessionProto {
    fn soft_interrupt(&self) -> SoftInterrupt {
        match self {
            Self::Codex { interrupt, .. } => SoftInterrupt::Codex(interrupt.clone()),
            Self::Cursor { interrupt, .. } => SoftInterrupt::Cursor(interrupt.clone()),
        }
    }

    /// 只有池自己还持有底层 client，且消费锁可立即取得，才算可迁移的 idle。
    ///
    /// `try_lock` 一项挡「正在协议调用/消费流」；`strong_count == 1` 额外挡住已经 checkout、
    /// 但尚未开始 `turn/start` 的把手。后者此刻锁是空的，单看 try_lock 会把仍在别人手上的会话
    /// 错判为空闲，rekey 后那把旧 handle 仍能向同一进程发下一轮。
    fn exclusively_idle(&self) -> bool {
        fn client_is_exclusively_idle<T>(client: &Arc<Mutex<T>>) -> bool {
            if Arc::strong_count(client) != 1 {
                return false;
            }
            client.try_lock().is_ok()
        }

        match self {
            Self::Codex { client, .. } => client_is_exclusively_idle(client),
            Self::Cursor { client, .. } => client_is_exclusively_idle(client),
        }
    }

    /// 交出一份**绑到本轮硬打断句柄**的句柄。软打断内部的升级(宽限不收敛 → SIGINT 整组)
    /// 因此走的也是本轮那把新闸,而不是 spawn 时那把跨轮共用的一次性闸。
    fn clone_handle(&self, killer: &ChildKiller) -> Self {
        match self {
            Self::Codex { client, interrupt } => Self::Codex {
                client: Arc::clone(client),
                interrupt: interrupt.rebind_killer(killer.clone()),
            },
            Self::Cursor { client, interrupt } => Self::Cursor {
                client: Arc::clone(client),
                interrupt: interrupt.rebind_killer(killer.clone()),
            },
        }
    }
}

/// 独占取用底层 client 的守卫。**消费权单线程独占**是刻意的(见模块头):一轮的投递 + 整段流式
/// 消费都在这把锁里跑完,打断绝不走这条路。
pub enum LockedSession<'a> {
    Codex(MutexGuard<'a, CodexAppServerClient>),
    Cursor(MutexGuard<'a, CursorAcpClient>),
}

/// 跨线程软打断把手(`Clone + Send`):控制卡「⏸ 打断」按钮从别的线程调它。
///
/// 两条协议各有自己的软打断帧(codex `turn/interrupt` request、cursor `session/cancel`
/// notification),但对调用方是同一件事,所以在这里收口成一个枚举 —— chat 线程的 select 循环
/// 不必知道协议。
#[derive(Clone)]
pub enum SoftInterrupt {
    Codex(RpcInterrupt),
    Cursor(AcpInterrupt),
}

impl SoftInterrupt {
    /// 此刻有没有在跑的一轮。两协议的「轮标识」形状不同(codex 是 turnId 串、cursor 是 prompt
    /// 请求 id),对调用方有意义的只是「有没有」,故只暴露这个最小公分母。
    ///
    /// `#[cfg(test)]`:同 [`AcpInterrupt::active_prompt_id`] —— 生产判定看 `interrupt()` 的返回值。
    #[cfg(test)]
    pub fn has_active_turn(&self) -> bool {
        match self {
            Self::Codex(handle) => handle.active_turn_id().is_some(),
            Self::Cursor(handle) => handle.active_prompt_id().is_some(),
        }
    }

    pub fn interrupt(&self) -> RpcInterruptOutcome {
        match self {
            Self::Codex(handle) => handle.interrupt(),
            Self::Cursor(handle) => handle.interrupt(),
        }
    }

    /// 轮内 steer(往正在跑的那一轮追加输入,不打断)。**这里不是最小公分母**:codex 的
    /// app-server 有 `turn/steer`,cursor 的 ACP 没有任何对应方法 —— 它那条路上「轮内新消息」
    /// 只能排队等下一轮。所以 cursor 分支恒 `Unsupported`,由调用方把消息放回队列。
    ///
    /// 刻意**不**在这里造一个 cursor 的假实现(比如取消再重发):那会把「一条消息一轮」的
    /// 语义悄悄改成「丢掉部分回答再重跑」,和 steer 是两回事。
    pub fn steer(&self, text: &str) -> RpcSteerOutcome {
        match self {
            Self::Codex(handle) => handle.steer(text),
            Self::Cursor(_) => RpcSteerOutcome::Unsupported,
        }
    }

    /// 这条路**有没有** steer 能力(与「此刻能不能 steer」无关 —— 那要看有没有在跑的轮)。
    ///
    /// 存在的理由是**不吞消息**:决策侧一旦把一条消息判成 SteerTask,就会 `mark_started` 把它
    /// 从队列里取走(durable,退不回去)。所以「这条路根本不支持」必须在 claim **之前**问清楚,
    /// 不能等发帧失败再补救。
    pub fn can_steer(&self) -> bool {
        matches!(self, Self::Codex(_))
    }
}

/// 交给 chat 线程的会话句柄。
pub struct PooledClient {
    proto: SessionProto,
    alive: Arc<AtomicBool>,
    killer: ChildKiller,
    session_id: String,
    pgid: i32,
    mode: Checkout,
    /// 这条会话此刻的模型 id(ACP 命名空间)。只有 cursor 非 None。
    current_model: Option<String>,
    /// 这条 Cursor 会话此刻的 mode id；Codex 恒 None。
    current_mode: Option<String>,
    /// Cursor 参数化目录的最新完整快照；Codex 恒空。
    config_options: Vec<CursorAcpConfigOption>,
    /// 这份 Cursor 目录是否来自 strict parser / setter exact 回执。fail-soft 或缺失目录绝不能
    /// 作为「值已经生效」的证据，否则 trim/坏 sibling 会让 early-return 静默放行。
    config_catalog_trusted: bool,
    /// 最近一次把**整份** bundle 向量最终 exact 验收通过的 generation。只验过旧代时，即使缓存
    /// currentValue 恰好等于新目标，也必须先刷新 live 目录，防 ABA 把旧快照当现状。
    config_applied_generation: Option<crate::capture::cursor_config::CursorTargetGeneration>,
    /// Cursor 没有原生 PRIMER 位；冷启后的第一条 prompt 真正写出前，这个位必须跨 checkout
    /// 保留。配置/mode 在 prompt 前失败不等于已经消费了身份注入。
    cursor_primer_pending: Arc<AtomicBool>,
    /// 池里这条跨轮进程的恢复快照。Cursor cold start 的 SID 与 PRIMER 同一提交点：
    /// 只有 prompt 帧写出后才能放进 pending，否则 daemon stop 会把空 SID 带到新一代。
    session_pending: Arc<Mutex<PendingTurn>>,
    /// Cursor configProvider 是进程级共享状态；每个 ACP 进程必须持有自己的临时根 guard，
    /// 不能只按 chat 命名（Bridge rekey 后进程环境不会跟着 tag 改）。Codex 恒 None。
    cursor_config_dir: Option<Arc<crate::capture::cursor_config::CursorProcessConfigDir>>,
}

impl PooledClient {
    /// 这一轮要投递到的原生会话 id(codex thread id / cursor sessionId)。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn checkout_mode(&self) -> Checkout {
        self.mode
    }

    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// 会话进程还在不在。判据同池内部([`Session::is_running`]):**不看 `killpg(0)` 一家之言** ——
    /// 已死未 reap 的僵尸进程照样「存在」,只有 stdout EOF 才是进程真的走了的证据。
    pub fn is_running(&self) -> bool {
        self.alive.load(Ordering::Acquire) && group_alive(self.pgid)
    }

    /// 跨线程软打断把手。
    pub fn interrupt_handle(&self) -> SoftInterrupt {
        self.proto.soft_interrupt()
    }

    /// **这一轮**的硬打断句柄:同一个会话 pgid,但每轮一把新的一次性闸(见 [`Session::handle`])。
    /// 软打断走 `interrupt_handle`,这是它之后的最后一级。
    ///
    /// 它同时就是把这一轮登记进 `InFlight` 时交出去的身份 —— 与池那条 `rpc:` 登记的身份
    /// **刻意不同**:`InFlight::finish`/`remove_if` 认的是 `Arc` 身份,共用一把的话,
    /// 被 detach 的上一轮迟到的 `finish` 会把下一轮的登记误摘掉。
    pub fn killer(&self) -> ChildKiller {
        self.killer.clone()
    }

    /// 独占取用底层 client。
    pub fn lock(&self) -> LockedSession<'_> {
        match &self.proto {
            SessionProto::Codex { client, .. } => {
                LockedSession::Codex(client.lock().unwrap_or_else(PoisonError::into_inner))
            }
            SessionProto::Cursor { client, .. } => {
                LockedSession::Cursor(client.lock().unwrap_or_else(PoisonError::into_inner))
            }
        }
    }

    /// Cursor 首条 prompt 是否仍需前置 PRIMER。Codex 的 developerInstructions 不走这条位。
    pub(crate) fn cursor_primer_pending(&self) -> bool {
        self.cursor_primer_pending.load(Ordering::Acquire)
    }

    pub(crate) fn cursor_config_dir(&self) -> Option<&std::path::Path> {
        self.cursor_config_dir.as_deref().map(|dir| dir.path())
    }

    /// 只在 `session/prompt` 帧已成功写出后清位；checkout / 参数收敛成功都不能提前消费。
    pub(crate) fn mark_cursor_primer_delivered(&self) {
        if let Ok(mut pending) = self.session_pending.lock() {
            pending.session_id = Some(self.session_id.clone());
        }
        self.cursor_primer_pending.store(false, Ordering::Release);
    }
}

/// 池里的一条会话。
struct Session {
    proto: SessionProto,
    /// 进程存活证据(reader 线程读到 stdout EOF 即清位)。判活不抢 client 的锁 —— 那把锁在一轮
    /// 流式消费期间会被 chat 线程长时间持有。
    alive: Arc<AtomicBool>,
    killer: ChildKiller,
    session_id: String,
    pgid: i32,
    /// 这条**会话**此刻的模型 id(ACP 命名空间;codex 恒 None —— 它每轮经 `turn/start` 下发)。
    /// 换模型走统一的 parameterized `session/set_config_option`,**不重开进程**。
    current_model: Option<String>,
    /// 这条会话可选的模型 id:有 `configOptions` 的 `model` 轴时取它(与每轮收敛同一份目录),
    /// 否则取 `availableModels`。
    available_models: Vec<String>,
    /// Cursor 当前会话模式与 server 动态目录；Codex 恒为空。
    current_mode: Option<String>,
    available_modes: Vec<CursorAcpMode>,
    /// Cursor `configOptions` 最新完整目录；Codex 恒空。
    config_options: Vec<CursorAcpConfigOption>,
    config_catalog_trusted: bool,
    config_applied_generation: Option<crate::capture::cursor_config::CursorTargetGeneration>,
    /// 这条刚开/接回的会话(目录是新取的)仍不含目标模型时记下的目标代际。复用时同一代际再
    /// 命中就直接报错，不再重启进程刷新目录。
    model_catalog_miss_generation: Option<crate::capture::cursor_config::CursorTargetGeneration>,
    /// 与每轮 `PooledClient` 共享，直到第一条 Cursor prompt 真正写出才清位。
    cursor_primer_pending: Arc<AtomicBool>,
    /// 与底层 Cursor client/所有 checkout handle 同寿命的进程独占配置根；Codex 恒 None。
    cursor_config_dir: Option<Arc<crate::capture::cursor_config::CursorProcessConfigDir>>,
    /// 周额度**剩余**百分比(codex `account/rateLimits/updated` 推的长周期窗;cursor 无 → 恒 None)。
    ///
    /// **存在会话上而不是随一轮走**:这条通知不带 turn 标识、也不保证每轮都推(额度没变就不推)。
    /// 跟着一轮走的话,角标会在"这轮推了/没推"之间闪烁;挂在跨轮存活的会话上,最后一次已知值
    /// 一直有效,直到下一次推送覆盖它。
    quota_left_pct: Option<u8>,
    /// 最后一次已知的**上下文占用**。
    ///
    /// **必须取 `last` 不能取 `total_*`**:后者是整个 thread 的累计,会随会话越滚越大,拿它当
    /// 上下文长度会越报越离谱(`parse::codex_rpc` 里有同样的警告)。那一轮送进模型的输入量,
    /// 就是此刻的上下文长度。
    ///
    /// 与 `quota_left_pct` 同理挂在跨轮存活的会话上:某轮没推 usage 时角标沿用上一次已知值,
    /// 不闪烁。Codex 写 `last.inputTokens`；Cursor 写 ACP `usage_update.used`。
    ctx_input_tokens: Option<u64>,
    /// 上下文窗口：Cursor 取 ACP `usage_update.size`，Codex 取
    /// `thread/tokenUsage/updated.tokenUsage.modelContextWindow`。
    ctx_window_tokens: Option<u64>,
    /// 这个**进程**是用哪个 cwd 起的。两种协议都把 cwd 钉在进程/会话上(codex `thread/start.cwd`、
    /// cursor `session/new.cwd` 且是必填),开完就再也改不了 —— 所以面板改工作目录之后若照常复用,
    /// agent 会一直在旧目录里干活:不报错、不丢回复,只是改了个寂寞。
    spawn_cwd: Option<String>,
    /// 这条 Codex app-server 是否由 summon 显式启用了 fast_mode。Cursor 恒 false。
    fast_mode_enabled: bool,
    /// 这条会话在 `InFlight` 里的登记原样留一份。`evict` 要把它**换个键**重新挂上去
    /// (见 [`RPC_CLOSING_KEY_PREFIX`]),照抄比就地拼一份假的诚实。
    registration: InFlightTurn,
}

impl Session {
    /// 进程还在不在。两个证据都要:`alive` 覆盖「进程死了但 pgid 已被 OS 复用」,
    /// `group_alive` 覆盖「reader 线程还没来得及看到 EOF」。
    fn is_running(&self) -> bool {
        self.alive.load(Ordering::Acquire) && group_alive(self.pgid)
    }

    /// 这条会话还能不能给本轮用：进程活着、**cwd 没换**，且显式 Fast 所需的 feature 已启用。
    ///
    /// cwd 是「钉在进程/会话上、开完改不了」的东西(codex `thread/start.cwd`、cursor
    /// `session/new.cwd` 必填),不比对就是让面板改工作目录静默失效。
    ///
    /// **模型刻意不在这里比**:两条协议都不必为换模型重开进程 —— codex 每轮经 `turn/start`
    /// 下发,cursor 走 `session/set_config_option`(见 [`RpcPool::sync_cursor_configuration`])。此前 cursor 靠
    /// 「模型不符就作废重开」是建立在 spawn `--model` 有效这个**已被真机证伪**的前提上的,
    /// 留着只会让每次 `/model` 白白重连一次会话,而模型其实压根没换。
    fn usable_for(&self, want_cwd: Option<&str>, require_fast_mode: bool) -> bool {
        self.is_running()
            && self.spawn_cwd.as_deref() == want_cwd
            && (!require_fast_mode || self.fast_mode_enabled)
    }

    /// 交出这一轮的句柄。**硬打断句柄每轮新建一把**(同 pgid、新的一次性闸),两条理由:
    ///
    /// - `ChildKiller::interrupt` 是一次性闸。会话进程跨轮存活,共用 spawn 那一把的话,
    ///   任何一轮升级过 SIGINT 之后,后续每一轮乃至 daemon 停机的 SIGINT 全变空操作 ——
    ///   只剩 SIGKILL 兜底,而那正是「不许 SIGKILL 掉带上下文的会话」那条红线。
    /// - `InFlight` 的 `finish`/`remove_if` 按 `Arc` 身份核对。轮 N 与轮 N+1 若共用同一把,
    ///   身份就恒相等:被 detach 的轮 N 迟到的 `finish` 会把轮 N+1 的登记摘掉。
    fn handle(&self, mode: Checkout) -> PooledClient {
        let turn_killer = ChildKiller::new(self.pgid);
        PooledClient {
            proto: self.proto.clone_handle(&turn_killer),
            alive: Arc::clone(&self.alive),
            killer: turn_killer,
            session_id: self.session_id.clone(),
            pgid: self.pgid,
            mode,
            current_model: self.current_model.clone(),
            current_mode: self.current_mode.clone(),
            config_options: self.config_options.clone(),
            config_catalog_trusted: self.config_catalog_trusted,
            config_applied_generation: self.config_applied_generation,
            cursor_primer_pending: Arc::clone(&self.cursor_primer_pending),
            session_pending: Arc::clone(&self.registration.pending),
            cursor_config_dir: self.cursor_config_dir.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PoolKey {
    agent: String,
    tag: String,
}

/// Bridge 列举 summon-owned RPC 会话用的只读快照。
///
/// 这只是候选视图，不是 reservation：真正提交仍由 `rekey_idle_exact` 在池锁 + InFlight 锁内
/// 重新验证同一组事实。`idle=false` 是保守结果，可能表示正在跑协议调用，也可能只是仍有一份
/// checkout handle 尚未释放；两种都不允许迁移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcHandoffSnapshot {
    pub session_id: String,
    pub spawn_cwd: Option<String>,
    pub pgid: i32,
    pub running: bool,
    pub idle: bool,
}

/// 一次成功 exact rekey 的回执。上层把 `.session` 当持久化提交点；提交失败时把本回执原样交给
/// `rollback_rekey`，它会按同一会话的 killer identity + sid/cwd/pgid 反向精确迁移。
pub struct RpcRekeyReceipt {
    pub agent: String,
    pub source_tag: String,
    pub target_tag: String,
    pub session_id: String,
    pub spawn_cwd: Option<String>,
    pub pgid: i32,
    identity: ChildKiller,
}

/// 一个 key 的 owner 与阶段放在同一条目中，不能同时既 Ready 又 Closing。
/// Opening 期间保留已有会话，供配置同步和观测使用；只有 reservation 可以摘换 owner。
enum PoolEntry {
    Opening(Option<Box<Session>>),
    Ready(Box<Session>),
    Closing,
}

impl PoolEntry {
    fn session(&self) -> Option<&Session> {
        match self {
            Self::Opening(Some(session)) | Self::Ready(session) => Some(session),
            Self::Opening(None) | Self::Closing => None,
        }
    }

    fn session_mut(&mut self) -> Option<&mut Session> {
        match self {
            Self::Opening(Some(session)) | Self::Ready(session) => Some(session),
            Self::Opening(None) | Self::Closing => None,
        }
    }

    fn transitioning(&self) -> bool {
        !matches!(self, Self::Ready(_))
    }
}

#[derive(Default)]
struct PoolState {
    entries: Mutex<HashMap<PoolKey, PoolEntry>>,
    changed: Condvar,
}

struct CheckoutReservation {
    state: Arc<PoolState>,
    key: PoolKey,
}

impl CheckoutReservation {
    fn take_session(&self) -> anyhow::Result<Box<Session>> {
        let mut entries = self
            .state
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match entries.get_mut(&self.key) {
            Some(PoolEntry::Opening(session)) => {
                session.take().context(summon_i18n::text!("r363ac47292"))
            }
            _ => anyhow::bail!(summon_i18n::format!("r363ac47292")),
        }
    }

    fn install(&self, session: Session) -> Result<(), Box<Session>> {
        let mut entries = self
            .state
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match entries.get_mut(&self.key) {
            Some(PoolEntry::Opening(owner @ None)) => {
                *owner = Some(Box::new(session));
                Ok(())
            }
            _ => Err(Box::new(session)),
        }
    }
}

impl Drop for CheckoutReservation {
    fn drop(&mut self) {
        let mut entries = self
            .state
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(PoolEntry::Opening(owner)) = entries.get_mut(&self.key) {
            match owner.take() {
                Some(session) => {
                    entries.insert(self.key.clone(), PoolEntry::Ready(session));
                }
                None => {
                    entries.remove(&self.key);
                }
            }
            self.state.changed.notify_all();
        }
    }
}

/// 会话池本体。clone 廉价(`Arc`),daemon 与各 chat 线程共享同一份。
#[derive(Clone)]
pub struct RpcPool {
    inflight: InFlight,
    state: Arc<PoolState>,
    /// SIGINT 之后、补 SIGKILL 之前的宽限。只管「等体面退出」这一段;SIGKILL 之后证明 Gone 的
    /// 上界与 checkout 等旧 writer 的上界仍按 [`SESSION_CLOSE_GRACE`],它们收敛即返回。
    close_grace: Duration,
}

/// 进程内唯一的会话池。
///
/// 为什么用全局而不是把池顺着 `daemon → dispatcher → chat_thread` 一路传下去:池的键是
/// `(agent, tag)`,本来就是进程级的;而 daemon 里 `InFlight` 只有一份,顺着传只会给十几个
/// 函数签名各加一个参数,换不到任何隔离性。
///
/// 首次调用用传入的 `InFlight` 绑定,之后恒返回同一个池(**认第一份**)。这正是「必须挂在
/// daemon 那份注册表上」这条不变量在全局形态下的表达 —— 第二份注册表若能改绑,进程就会挂到一个
/// 没人停机的表上变孤儿。
static SHARED: std::sync::OnceLock<RpcPool> = std::sync::OnceLock::new();

/// 账号级额度是慢变量，不属于某一条 RPC session。按 agent 保留最后一次观测，使其它 chat、
/// 尚未建 session 的面板和刚被摘除 session 的收尾都能读取同一份最新值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuotaOrigin {
    Rollout,
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LatestQuota {
    origin: QuotaOrigin,
    left_pct: u8,
    resets_at: Option<i64>,
    /// 这份观测产生的 Unix 秒:盘上快照带落盘时刻,实时读到的记当下;缺失视为最旧。
    observed_at: Option<i64>,
}

static LATEST_QUOTA: std::sync::OnceLock<Mutex<HashMap<String, LatestQuota>>> =
    std::sync::OnceLock::new();

const DEFAULT_CODEX_ACCOUNT: &str = "\0codex-default-account";

fn latest_quota_cache() -> &'static Mutex<HashMap<String, LatestQuota>> {
    LATEST_QUOTA.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 5 小时窗口的账号级剩余额度,与周额度分表存、同一套新旧规则。两个窗口各自重置,放进同一条
/// 记录的话,任一窗口的新观测都会被当成另一窗口的新旧依据。
static LATEST_FIVE_HOUR_QUOTA: std::sync::OnceLock<Mutex<HashMap<String, LatestQuota>>> =
    std::sync::OnceLock::new();

fn latest_five_hour_quota_cache() -> &'static Mutex<HashMap<String, LatestQuota>> {
    LATEST_FIVE_HOUR_QUOTA.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 读或写默认 Codex 账号额度的测试的串行闸,进入与释放各清一次那条缓存。
///
/// 其它测试靠各取唯一 agent 名互不干扰,这一条不行:`DEFAULT_CODEX_ACCOUNT` 是所有「用默认
/// Codex 账号」的 agent 共用的同一个键,同进程并发跑就会互相读到对方写的观测。锁只需进程内:
/// `LATEST_QUOTA` 是进程 static,`cargo nextest` 每条测试各起一个进程本就隔离,真正会串的
/// 是 `cargo test` 的同进程多线程。
///
/// 与 [`crate::test_support::process_test_guard`] 是两把独立的锁;两把都要的测试按
/// 「先进程闸、后额度闸」的顺序取。
#[cfg(test)]
pub(crate) struct DefaultCodexQuotaTestGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
pub(crate) fn default_codex_quota_test_guard() -> DefaultCodexQuotaTestGuard {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    // 单条测试失败不该把同组其余测试全变成 PoisonError,保留最初的真实失败证据。
    let lock = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    forget_default_codex_quota();
    DefaultCodexQuotaTestGuard { _lock: lock }
}

#[cfg(test)]
impl Drop for DefaultCodexQuotaTestGuard {
    fn drop(&mut self) {
        forget_default_codex_quota();
    }
}

#[cfg(test)]
fn forget_default_codex_quota() {
    latest_quota_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(DEFAULT_CODEX_ACCOUNT);
    latest_five_hour_quota_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(DEFAULT_CODEX_ACCOUNT);
    five_hour_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(DEFAULT_CODEX_ACCOUNT);
}

/// 两个 reset 时刻相差在此之内算同一窗口:Codex 同一周窗口的 resets_at 在相邻观测间有秒级抖动。
const SAME_WINDOW_TOLERANCE_SECS: u64 = 60;

/// 不同来源按原始观测时刻排序，同秒时实时账号观测优先。
/// rollout 回放按原始观测时刻排序，兼容账号与计划变更。
/// 实时来源内新窗口压过旧窗口，同一窗口内按观测时刻取后者。供应商可以在不动 reset 时刻的前提下把
/// 额度整个刷新(2026-09-02 Claude 周额度在同一 resets_at 下从剩 31% 回到剩 99%),同窗口内的
/// 剩余量因此并不单调,只有观测时刻能定新旧。
fn quota_observation_is_newer(current: LatestQuota, incoming: LatestQuota) -> bool {
    if current.origin != incoming.origin {
        if incoming.observed_at != current.observed_at {
            return incoming.observed_at > current.observed_at;
        }
        return incoming.origin == QuotaOrigin::Live;
    }
    if incoming.origin == QuotaOrigin::Rollout && incoming.observed_at != current.observed_at {
        return incoming.observed_at > current.observed_at;
    }
    match (current.resets_at, incoming.resets_at) {
        (Some(current_reset), Some(incoming_reset))
            if incoming_reset.abs_diff(current_reset) > SAME_WINDOW_TOLERANCE_SECS =>
        {
            incoming_reset > current_reset
        }
        (None, Some(_)) => true,
        (Some(_), None) => false,
        _ => incoming.observed_at >= current.observed_at,
    }
}

fn record_latest_quota(
    agent: &str,
    left_pct: u8,
    resets_at: Option<i64>,
    observed_at: Option<i64>,
    origin: QuotaOrigin,
) -> (bool, bool) {
    record_latest_quota_in(
        latest_quota_cache(),
        agent,
        left_pct,
        resets_at,
        observed_at,
        origin,
    )
}

fn record_latest_quota_in(
    cache: &Mutex<HashMap<String, LatestQuota>>,
    agent: &str,
    left_pct: u8,
    resets_at: Option<i64>,
    observed_at: Option<i64>,
    origin: QuotaOrigin,
) -> (bool, bool) {
    let Ok(mut quotas) = cache.lock() else {
        return (false, false);
    };
    let incoming = LatestQuota {
        origin,
        left_pct: left_pct.min(100),
        resets_at,
        observed_at,
    };
    let Some(current) = quotas.get(agent).copied() else {
        quotas.insert(agent.to_string(), incoming);
        return (true, true);
    };
    if !quota_observation_is_newer(current, incoming) {
        return (false, false);
    }
    let changed = current.left_pct != incoming.left_pct || current.resets_at != incoming.resets_at;
    quotas.insert(agent.to_string(), incoming);
    (true, changed)
}

fn latest_quota(agent: &str) -> Option<LatestQuota> {
    latest_quota_cache()
        .lock()
        .ok()
        .and_then(|quotas| quotas.get(agent).copied())
}

/// 5 小时窗口一天重置好几次,重置时刻已过的观测描述的是一个已经不存在的窗口,不拿来显示。
fn latest_five_hour_quota(agent: &str, now: i64) -> Option<LatestQuota> {
    latest_five_hour_quota_cache()
        .lock()
        .ok()
        .and_then(|quotas| quotas.get(agent).copied())
        .filter(|quota| quota.resets_at.is_none_or(|resets_at| resets_at > now))
}

fn newer_quota(left: Option<LatestQuota>, right: Option<LatestQuota>) -> Option<LatestQuota> {
    match (left, right) {
        (Some(left), Some(right)) if quota_observation_is_newer(left, right) => Some(right),
        (Some(left), Some(_)) => Some(left),
        (Some(left), None) => Some(left),
        (None, right) => right,
    }
}

/// 5 小时窗口的重置时刻与限额类型，供额度耗尽时发预填定时卡。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodexFiveHourReset {
    pub resets_at: i64,
    pub is_temporal: bool,
    pub observed_at: i64,
}

/// 5h 窗合法 reset 距当前 ≤ 6h，与 Claude 的 `MAX_RESET_AHEAD` 对齐；
/// 异常大的 resets_at 不发定时卡，回退通知卡。
const CODEX_MAX_RESET_AHEAD_SECS: i64 = 6 * 60 * 60;

impl CodexFiveHourReset {
    pub fn is_actionable_at(&self, now: i64) -> bool {
        self.is_temporal
            && self.resets_at > now
            && self.resets_at - now <= CODEX_MAX_RESET_AHEAD_SECS
    }
}

static LATEST_CODEX_FIVE_HOUR: std::sync::OnceLock<Mutex<HashMap<String, CodexFiveHourReset>>> =
    std::sync::OnceLock::new();

fn five_hour_cache() -> &'static Mutex<HashMap<String, CodexFiveHourReset>> {
    LATEST_CODEX_FIVE_HOUR.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记录 5 小时窗口的重置时刻。只在快照确实带短窗（< 一天）时写入；
/// 同键已有更新观测时不覆盖。
fn record_codex_five_hour_reset(
    agent_chat: Option<(&str, &str)>,
    snapshot: &CodexRateLimitSnapshot,
    observed_at: Option<i64>,
) {
    let Some(resets_at) = snapshot.five_hour_resets_at() else {
        return;
    };
    let key = agent_chat.map(|(a, _)| a).unwrap_or(DEFAULT_CODEX_ACCOUNT);
    let observed_at = observed_at.unwrap_or_else(unix_now_secs);
    let entry = CodexFiveHourReset {
        resets_at,
        is_temporal: snapshot.is_temporal_limit(),
        observed_at,
    };
    if let Ok(mut cache) = five_hour_cache().lock() {
        let newer = cache
            .get(key)
            .map(|existing| observed_at >= existing.observed_at)
            .unwrap_or(true);
        if newer {
            cache.insert(key.to_string(), entry);
        }
    }
}

/// 读 agent 对应的最新 5 小时重置时刻。agent 用默认 Codex 账号时默认键与 agent 键都查，
/// 取观测时刻更新的那个。
pub fn latest_codex_five_hour_reset(agent: &Agent) -> Option<CodexFiveHourReset> {
    let cache = five_hour_cache().lock().ok()?;
    if agent_uses_default_codex_account(agent) {
        match (
            cache.get(DEFAULT_CODEX_ACCOUNT).copied(),
            cache.get(agent.name.as_str()).copied(),
        ) {
            (Some(default), Some(agent_entry)) => {
                if agent_entry.observed_at > default.observed_at {
                    Some(agent_entry)
                } else {
                    Some(default)
                }
            }
            (Some(default), None) => Some(default),
            (None, agent_entry) => agent_entry,
        }
    } else {
        cache.get(agent.name.as_str()).copied()
    }
}

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 取共享池。**前提会被当场校验,而不是靠调用方自觉**:传进来的注册表若不是绑定的那一份
/// (典型是测试自建了一份 `InFlight`),返回一个照传入表直建的**独立池** + 一条 warn,
/// 而不是把这些会话默默挂到另一张表上。
///
/// 为什么是 fail-safe 而非 fail-silent:静默复用全局池会让「进程挂在哪张表上」与调用方以为的
/// 不一致 —— 生产里这意味着停机够不着,测试里意味着一条用例能永久污染同进程内其余用例。
/// 返回值改成 owned(`RpcPool` 是两个 `Arc`,clone 廉价)正是为了让这条降级表达得出来。
pub fn shared(inflight: &InFlight) -> RpcPool {
    let pool = SHARED.get_or_init(|| RpcPool::new(inflight.clone()));
    if pool.inflight.instance_id() == inflight.instance_id() {
        return pool.clone();
    }
    log(&summon_i18n::format!(
        "r024bb7e493",
        pool.inflight.instance_id(),
        inflight.instance_id()
    ));
    RpcPool::new(inflight.clone())
}

#[cfg(test)]
pub(crate) fn shared_for_tests() -> RpcPool {
    SHARED.get_or_init(|| RpcPool::new(InFlight::new())).clone()
}

#[cfg(test)]
pub(crate) fn shared_inflight_for_tests() -> InFlight {
    shared_for_tests().inflight.clone()
}

/// 对 setter 返回的**最后一份完整目录**做整向量验收。逐次 setter 只证明本次轴，后写轴可能
/// 同时把先前 context/model 静默回退；不在循环末尾再看一遍就会拿半收敛配置发 prompt。
fn cursor_config_vector_mismatches(
    client: &PooledClient,
    desired_model: &str,
    desired_options: &[(String, String)],
) -> Vec<String> {
    if !client.config_catalog_trusted {
        return vec!["catalog-untrusted".to_string()];
    }
    let mut desired = Vec::with_capacity(desired_options.len() + 1);
    desired.push(("model", desired_model));
    desired.extend(
        desired_options
            .iter()
            .map(|(id, value)| (id.as_str(), value.as_str())),
    );
    desired
        .into_iter()
        .filter_map(|(id, value)| {
            let exact = client.config_options.iter().any(|axis| {
                axis.id == id
                    && axis.current_value.as_deref() == Some(value)
                    && axis.options.iter().any(|option| option.value == value)
            });
            (!exact).then(|| id.to_string())
        })
        .collect()
}

/// `agents tail` 里那行候选模型:两份目录都列全,`/session` 与开跑前检查认的是 `model` 参数
/// 那份,`availableModels` 是旧式目录;缺哪份就明说缺,排查时才分得清是没下发还是下发了空的。
fn cursor_catalog_note(opened: &crate::capture::cursor_acp::CursorAcpSession) -> String {
    let none = summon_i18n::text!("r7b3e91c0f4");
    let list = |models: &[String]| {
        if models.is_empty() {
            none.to_string()
        } else {
            models.join(", ")
        }
    };
    let axis = cursor_model_axis_values(&opened.config_options).unwrap_or_default();
    let current = opened.current_model_id.as_deref().unwrap_or(none);
    summon_i18n::format!(
        "r9d4f2a6b18",
        axis.len(),
        list(&axis),
        opened.available_models.len(),
        list(&opened.available_models),
        current
    )
}

/// Cursor 动态目录里 `model` 轴的全部可选值；没有这条轴或它为空时为 None。
///
/// 每轮收敛按这份目录校验模型，`/session` 的候选与保存校验也必须读同一份，否则面板能存下
/// 收敛时不认的值，每一轮都卡在 prompt 前。
fn cursor_model_axis_values(config_options: &[CursorAcpConfigOption]) -> Option<Vec<String>> {
    config_options
        .iter()
        .find(|option| option.id == "model")
        .map(|option| {
            option
                .options
                .iter()
                .map(|value| value.value.clone())
                .collect::<Vec<_>>()
        })
        .filter(|models| !models.is_empty())
}

/// 收敛目标模型不在本进程持有的 Cursor `model` 目录里。
///
/// 目录在 ACP 进程启动/接回时下发，之后只随 setter 回执更新；常驻进程的目录可能落后于账号的
/// 实际目录（cursor-agent 自更新、模型上新）。复用会话命中它时换进程重取一次目录再判，
/// `generation` 用来避免同一目标在已刷新过的会话上反复重启进程。
#[derive(Debug)]
struct CursorModelCatalogMiss {
    model: String,
    generation: crate::capture::cursor_config::CursorTargetGeneration,
}

impl std::fmt::Display for CursorModelCatalogMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let model = &self.model;
        f.write_str(&summon_i18n::format!("r4c1b7e93a0"))
    }
}

impl std::error::Error for CursorModelCatalogMiss {}

fn cursor_config_apply_rank(id: &str) -> u8 {
    match id {
        "context" => 10,
        "thinking" => 20,
        "reasoning" | "effort" => 30,
        "fast" => 50,
        _ => 40,
    }
}

pub fn rpc_control_help() -> String {
    let blacklist = summon_core::driver::slash_pool::SlashBlacklist::default();
    let toggles = summon_core::driver::slash_pool::SlashToggles::default();
    let mut help = String::from(summon_i18n::text!("rfc5d7f62f3"));
    help.push_str(&summon_core::driver::slash_pool::backend_help_markdown(
        CliDriver::Codex,
        CaptureMode::Rpc,
        &blacklist,
        &toggles,
    ));
    // 这条回执走单元素 markdown 通道,渲不出灰字;注脚原样追加,信息不丢。
    if let Some(note) = summon_core::driver::slash_pool::backend_help_note(
        CliDriver::Codex,
        CaptureMode::Rpc,
        &blacklist,
        &toggles,
    ) {
        help.push_str(&format!("\n\n{note}"));
    }
    help
}

#[derive(Debug, thiserror::Error)]
enum RpcControlFailure {
    #[error("{}", summon_i18n::text!("r2abdd8f3f0"))]
    NotLoaded,
    #[error("{}", summon_i18n::text!("r80c76ffce1"))]
    Exited,
    #[error("{}", summon_i18n::text!("r0fee9a94c8"))]
    Busy,
    #[error("{}", summon_i18n::text!("r3aa6ad7ffd"))]
    Stopping,
    #[error("{}", summon_i18n::text!("r36dc4d3f45"))]
    TooMany,
    #[error("{}", summon_i18n::text!("r22daf2815c"))]
    ListTimeout,
    #[error("{}", summon_i18n::text!("r4cf0e808af"))]
    PaginationLoop,
    #[error("{}", summon_i18n::format!("r4e28ec6b0c", stopped = .stopped, total = .total))]
    StopTimeout { stopped: usize, total: usize },
    #[error("{}", summon_i18n::format!("rc3169e1561", stopped = .stopped, total = .total))]
    PartialStop { stopped: usize, total: usize },
}

pub fn rpc_control_public_error(error: &anyhow::Error) -> String {
    if let Some(reason) = error.downcast_ref::<RpcControlFailure>() {
        return safe_rpc_label(&reason.to_string(), 300);
    }
    if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<crate::codex::codex_app_server::CodexAppServerError>(),
            Some(
                crate::codex::codex_app_server::CodexAppServerError::Disconnected { .. }
                    | crate::codex::codex_app_server::CodexAppServerError::ProcessFailed { .. }
            )
        )
    }) {
        return summon_i18n::text!("re88d4c30da").into();
    }
    summon_i18n::text!("rc82e1cd638").into()
}

fn grouped_u64(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn safe_rpc_label(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut out = chars
        .by_ref()
        .take(max_chars)
        .map(|character| {
            if character.is_control() {
                ' '
            } else if character == '`' {
                '｀'
            } else {
                character
            }
        })
        .collect::<String>();
    if chars.next().is_some() {
        out.push('…');
    }
    summon_core::messaging::feishu::escape_lark_md(&out)
}

fn clip_rpc_markdown(value: &str, max_chars: usize) -> String {
    let total = value.chars().count();
    if total <= max_chars {
        return value.to_string();
    }
    let head = value.chars().take(max_chars).collect::<String>();
    summon_i18n::format!("r405e7f4d64")
}

fn rpc_names<'a>(value: &'a Value, path: &str, key: &str) -> Vec<&'a str> {
    value
        .pointer(path)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get(key).and_then(Value::as_str))
        .collect()
}

fn nested_array_count(value: &Value, outer: &str, inner: &str) -> usize {
    value
        .get(outer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|entry| {
            entry
                .get(inner)
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        })
        .sum()
}

fn render_simple_names(label: &str, names: Vec<&str>) -> String {
    if names.is_empty() {
        return summon_i18n::format!("r8d8af40e38");
    }
    let shown = names
        .iter()
        .take(50)
        .map(|name| format!("- `{}`", safe_rpc_label(name, 120)))
        .collect::<Vec<_>>()
        .join("\n");
    if names.len() > 50 {
        summon_i18n::format!("r3d2757bc47", names.len())
    } else {
        summon_i18n::format!("r9b552e7ae4", names.len())
    }
}

fn background_process_ids(
    client: &mut CodexAppServerClient,
    driver: CliDriver,
    thread_id: &str,
    deadline: Instant,
) -> anyhow::Result<Vec<String>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(!remaining.is_zero(), RpcControlFailure::ListTimeout);
    if driver == CliDriver::Traex {
        let result = client.control_request(
            "thread/backgroundTerminals/list",
            json!({"threadId": thread_id}),
            Some(remaining.min(RPC_CONTROL_TIMEOUT)),
        )?;
        let ids = result
            .get("processIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        anyhow::ensure!(ids.len() <= 200, RpcControlFailure::TooMany);
        return Ok(ids);
    }

    let mut ids = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), RpcControlFailure::ListTimeout);
        let result = client.control_request(
            "thread/backgroundTerminals/list",
            json!({"threadId": thread_id, "cursor": cursor, "limit": 100}),
            Some(remaining.min(RPC_CONTROL_TIMEOUT)),
        )?;
        ids.extend(
            result
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.get("processId").and_then(Value::as_str))
                .map(str::to_string),
        );
        anyhow::ensure!(ids.len() <= 200, RpcControlFailure::TooMany);
        let next = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(next) = next else {
            break;
        };
        anyhow::ensure!(seen.insert(next.clone()), RpcControlFailure::PaginationLoop);
        cursor = Some(next);
    }
    Ok(ids)
}

fn paged_control_data(
    client: &mut CodexAppServerClient,
    method: &'static str,
    mut base_params: Value,
) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut data = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        base_params["cursor"] = cursor.clone().map_or(Value::Null, Value::String);
        base_params["limit"] = json!(100);
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), summon_i18n::format!("r95870b2292"));
        let result = client.control_request(
            method,
            base_params.clone(),
            Some(remaining.min(RPC_CONTROL_TIMEOUT)),
        )?;
        data.extend(
            result
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        anyhow::ensure!(data.len() <= 500, summon_i18n::format!("r053c28ae09"));
        let next = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(next) = next else {
            break;
        };
        anyhow::ensure!(
            seen.insert(next.clone()),
            summon_i18n::format!("r5c2724a65c")
        );
        cursor = Some(next);
    }
    Ok(data)
}

/// 等 `/compact` 真的压缩完,并把压缩后的上下文占用带回来。
///
/// `thread/compact/start` 只是**提交**:压缩在 app-server 侧跑,期间不开轮,完成通知
/// `thread/compacted` 与随后的 `thread/tokenUsage/updated` 因此都落在**轮外**。轮外通知要等
/// 下一轮开始才被 drain,而那次 drain 只提额度、把用量丢掉 —— 于是「已压缩」的回执和卡上的
/// 占用都还停在压缩前的数字(owner 2026-09-09 报告)。这里把那段等待收进控制指令自己。
///
/// 返回 `(是否确认压缩完成, 压缩后的 (占用, 窗口))`。两者独立:等到完成但没等到用量推送是
/// 常态,回执分开说,不拿旧数字冒充新的。
///
/// **代价如实记下**:等待期间收到的其它控制面通知(额度推送等)会被消费掉,不再进下一轮的
/// drain。额度每轮都会重推,少一次只影响角标何时刷新;而压缩期间本就不该有别的轮在跑。
fn wait_for_compaction(client: &mut CodexAppServerClient) -> (bool, Option<(u64, u64)>) {
    let mut deadline = Instant::now() + RPC_COMPACT_TIMEOUT;
    let mut compacted = false;
    let mut ctx = None;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match client.recv_notification(Some(remaining.min(RPC_CONTROL_TIMEOUT))) {
            Ok(summon_core::parse::codex_rpc::CodexAppServerNotification::Other {
                method, ..
            }) if method == "thread/compacted" => {
                compacted = true;
                // 用量推送紧跟完成而来,但不保证来 —— 只为它多等一个短窗,不挂满硬顶。
                deadline = Instant::now() + RPC_COMPACT_USAGE_GRACE;
            }
            Ok(summon_core::parse::codex_rpc::CodexAppServerNotification::TokenUsage {
                last,
                model_context_window,
                ..
            }) => {
                if let (Some(breakdown), Some(window)) =
                    (last, model_context_window.filter(|window| *window > 0))
                {
                    // 取 `last.input_tokens`:那一次送进模型的输入量就是此刻的上下文长度。
                    // `total_*` 是整个 thread 的累计,拿它会越报越离谱(同 `record_ctx_budget`)。
                    ctx = Some((breakdown.input_tokens, window));
                }
                if compacted {
                    break;
                }
            }
            // 压缩期间的其它通知:不属于任何一轮,丢弃。
            Ok(_) => {}
            // 超时片 / 传输错都退出:再等下去也分不出「压缩慢」与「连接坏了」。
            Err(_) => break,
        }
    }
    (compacted, ctx)
}

/// 跑一条 RPC 控制指令。`observed_ctx` 是**出参**:只有 `/compact` 会写它 —— 压缩之后的
/// 上下文占用只能在这条路上观测到(压缩不开轮,用量推送落在轮外),调用方据它更新会话。
fn run_rpc_control(
    client: &mut CodexAppServerClient,
    driver: CliDriver,
    thread_id: &str,
    cwd: Option<&str>,
    cached_ctx_tokens: Option<u64>,
    command: &RpcSlashCommand,
    observed_ctx: &mut Option<(u64, u64)>,
) -> anyhow::Result<String> {
    let request = |client: &mut CodexAppServerClient, method, params| {
        client
            .control_request(method, params, Some(RPC_CONTROL_TIMEOUT))
            .map_err(anyhow::Error::from)
    };
    match command {
        RpcSlashCommand::Help => Ok(rpc_control_help()),
        RpcSlashCommand::Compact => {
            request(
                client,
                "thread/compact/start",
                json!({"threadId": thread_id}),
            )?;
            let (compacted, ctx) = wait_for_compaction(client);
            *observed_ctx = ctx;
            Ok(match (compacted, ctx) {
                (true, Some((used, window))) => {
                    summon_i18n::format!("r36673ec9a3", grouped_u64(used), grouped_u64(window))
                }
                (true, None) => summon_i18n::text!("r7c9a81a63c").into(),
                (false, _) => summon_i18n::text!("r6e4deccb65").into(),
            })
        }
        RpcSlashCommand::Status => {
            let thread = client
                .thread_read(thread_id, false, Some(RPC_CONTROL_TIMEOUT))
                .context(summon_i18n::text!("rea24b8cbdf"))?;
            let name = thread
                .name
                .as_deref()
                .unwrap_or(summon_i18n::text!("r67bee6acfc"));
            let status = thread
                .status
                .as_ref()
                .and_then(|value| value.get("type").and_then(Value::as_str))
                .unwrap_or("unknown");
            // 回执可能落在群里；thread id 与本机绝对 cwd 不属于群消息，绝不能照 RPC 原样回显。
            Ok(summon_i18n::format!(
                "rc4d60117ab",
                safe_rpc_label(name, 120),
                safe_rpc_label(status, 32)
            ))
        }
        RpcSlashCommand::Context if driver == CliDriver::Traex => {
            let result = request(
                client,
                "thread/contextUsage",
                json!({"threadId": thread_id}),
            )?;
            let categories = result
                .pointer("/contextWindowUsage/categories")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if categories.is_empty() {
                return Ok(summon_i18n::text!("r4a905e3c8e").into());
            }
            let total = categories
                .iter()
                .filter_map(|entry| entry.get("tokens").and_then(Value::as_u64))
                .sum::<u64>();
            let lines = categories
                .iter()
                .map(|entry| {
                    let kind = entry
                        .get("kind")
                        .or_else(|| entry.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let tokens = entry.get("tokens").and_then(Value::as_u64).unwrap_or(0);
                    format!(
                        "- `{}`：{} tokens",
                        safe_rpc_label(kind, 80),
                        grouped_u64(tokens)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(summon_i18n::format!("r176fd7bcc9", grouped_u64(total)))
        }
        RpcSlashCommand::Context => Ok(cached_ctx_tokens
            .map(|tokens| summon_i18n::format!("rfbee24946e", grouped_u64(tokens)))
            .unwrap_or_else(|| summon_i18n::text!("r2fa77ec4e6").into())),
        RpcSlashCommand::Rename(name) => {
            request(
                client,
                "thread/name/set",
                json!({"threadId": thread_id, "name": name}),
            )?;
            Ok(summon_i18n::format!(
                "rd3b8c846d4",
                safe_rpc_label(name, 100)
            ))
        }
        RpcSlashCommand::Goal(RpcGoalCommand::Get) => {
            let result = request(client, "thread/goal/get", json!({"threadId": thread_id}))?;
            let Some(goal) = result.get("goal").filter(|goal| !goal.is_null()) else {
                return Ok(summon_i18n::text!("r2330537ff5").into());
            };
            let objective = goal
                .get("objective")
                .and_then(Value::as_str)
                .unwrap_or(summon_i18n::text!("r90c5a502e1"));
            let status = goal
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let tokens = goal.get("tokensUsed").and_then(Value::as_u64).unwrap_or(0);
            Ok(summon_i18n::format!(
                "r27dde06838",
                safe_rpc_label(objective, 500),
                safe_rpc_label(status, 32),
                grouped_u64(tokens)
            ))
        }
        RpcSlashCommand::Goal(RpcGoalCommand::Set(objective)) => {
            request(
                client,
                "thread/goal/set",
                json!({"threadId": thread_id, "objective": objective, "status": "active"}),
            )?;
            Ok(summon_i18n::format!(
                "r104b838676",
                safe_rpc_label(objective, 500)
            ))
        }
        RpcSlashCommand::Goal(RpcGoalCommand::Clear) => {
            request(client, "thread/goal/clear", json!({"threadId": thread_id}))?;
            Ok(summon_i18n::text!("rb94bf22412").into())
        }
        RpcSlashCommand::Mcp { verbose } => {
            let detail = if *verbose { "full" } else { "toolsAndAuthOnly" };
            let servers =
                paged_control_data(client, "mcpServerStatus/list", json!({"detail": detail}))?;
            if servers.is_empty() {
                return Ok(summon_i18n::text!("r305955fcb2").into());
            }
            let lines = servers
                .iter()
                .take(50)
                .map(|server| {
                    let name = server
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let tools = server
                        .get("tools")
                        .and_then(Value::as_object)
                        .map(|tools| tools.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default();
                    if *verbose && !tools.is_empty() {
                        let shown = tools
                            .iter()
                            .take(20)
                            .map(|tool| safe_rpc_label(tool, 100))
                            .collect::<Vec<_>>()
                            .join("、");
                        let suffix = if tools.len() > 20 {
                            summon_i18n::format!("rac1d50e040", tools.len() - 20)
                        } else {
                            String::new()
                        };
                        format!("- `{}`：{shown}{suffix}", safe_rpc_label(name, 120))
                    } else {
                        format!("- `{}`：{} tools", safe_rpc_label(name, 120), tools.len())
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let suffix = if servers.len() > 50 {
                summon_i18n::text!("rf8894f2cb8")
            } else {
                ""
            };
            Ok(summon_i18n::format!("r147fba7f15", servers.len()))
        }
        RpcSlashCommand::Skills => {
            let result = request(
                client,
                "skills/list",
                json!({"cwds": cwd.into_iter().collect::<Vec<_>>(), "forceReload": false}),
            )?;
            let names = result
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|entry| rpc_names(entry, "/skills", "name"))
                .collect();
            let errors = nested_array_count(&result, "data", "errors");
            Ok(summon_i18n::format!(
                "r1305c4db59",
                render_simple_names("Skills", names)
            ))
        }
        RpcSlashCommand::Hooks => {
            let result = request(
                client,
                "hooks/list",
                json!({"cwds": cwd.into_iter().collect::<Vec<_>>()}),
            )?;
            let names = result
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|entry| {
                    let mut names = rpc_names(entry, "/hooks", "key");
                    if names.is_empty() {
                        names = rpc_names(entry, "/hooks", "name");
                    }
                    names
                })
                .collect();
            let errors = nested_array_count(&result, "data", "errors");
            let warnings = nested_array_count(&result, "data", "warnings");
            Ok(summon_i18n::format!(
                "r043b3994ce",
                render_simple_names("Hooks", names)
            ))
        }
        RpcSlashCommand::Plugins => {
            let result = request(
                client,
                "plugin/list",
                json!({"cwds": cwd.into_iter().collect::<Vec<_>>()}),
            )?;
            let marketplaces = result
                .get("marketplaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if marketplaces.is_empty() {
                let errors = result
                    .get("marketplaceLoadErrors")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                return Ok(summon_i18n::format!("ra4a2b1864c"));
            }
            let lines = marketplaces
                .iter()
                .take(50)
                .map(|marketplace| {
                    let name = marketplace
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let plugins = marketplace
                        .get("plugins")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    format!("- `{}`：{plugins} plugins", safe_rpc_label(name, 120))
                })
                .collect::<Vec<_>>()
                .join("\n");
            let suffix = if marketplaces.len() > 50 {
                summon_i18n::text!("rf8894f2cb8")
            } else {
                ""
            };
            let errors = result
                .get("marketplaceLoadErrors")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Ok(summon_i18n::format!("r705c223c5b"))
        }
        RpcSlashCommand::Experimental => {
            let params = if driver == CliDriver::Codex {
                json!({"threadId": thread_id})
            } else {
                json!({})
            };
            let features = paged_control_data(client, "experimentalFeature/list", params)?;
            if features.is_empty() {
                return Ok(summon_i18n::text!("r82a8f4da0b").into());
            }
            let lines = features
                .iter()
                .map(|feature| {
                    let name = feature
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let enabled = feature
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    format!(
                        "- `{}`：{}",
                        safe_rpc_label(name, 120),
                        if enabled { "on" } else { "off" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(summon_i18n::format!("r45da73b250", features.len()))
        }
        RpcSlashCommand::Processes => {
            let count = background_process_ids(
                client,
                driver,
                thread_id,
                Instant::now() + Duration::from_secs(30),
            )?
            .len();
            // process id 是本机运行时标识；查看命令只回数量，`/stop` 由受控端口内部消费 exact id。
            Ok(summon_i18n::format!("re7f2a3e540"))
        }
        RpcSlashCommand::StopBackground => {
            let deadline = Instant::now() + Duration::from_secs(30);
            let ids = background_process_ids(client, driver, thread_id, deadline)?;
            let method = if driver == CliDriver::Traex {
                "thread/backgroundTerminals/stop"
            } else {
                "thread/backgroundTerminals/terminate"
            };
            for (index, process_id) in ids.iter().enumerate() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                anyhow::ensure!(
                    !remaining.is_zero(),
                    RpcControlFailure::StopTimeout {
                        stopped: index,
                        total: ids.len()
                    }
                );
                if let Err(error) = client.control_request(
                    method,
                    json!({"threadId": thread_id, "processId": process_id}),
                    Some(remaining.min(Duration::from_secs(5))),
                ) {
                    return Err(anyhow::Error::new(error).context(
                        RpcControlFailure::PartialStop {
                            stopped: index,
                            total: ids.len(),
                        },
                    ));
                }
            }
            Ok(if ids.is_empty() {
                summon_i18n::text!("rbe5eff2ef2").into()
            } else {
                summon_i18n::format!("rc49226d2a0", ids.len())
            })
        }
    }
}

impl RpcPool {
    /// 必须传 daemon 那份 `InFlight`。**没有 `Default`** 是刻意的:默认构造出一份空注册表,进程就
    /// 挂在一个没人会去停机的表上,停机时无声无息变孤儿 —— 这种错误不该有默认值可犯。
    pub fn new(inflight: InFlight) -> Self {
        Self {
            inflight,
            state: Arc::new(PoolState::default()),
            close_grace: SESSION_CLOSE_GRACE,
        }
    }

    /// 换一个更短的 SIGINT 宽限。只给夹具屏蔽 SIGINT、宽限必然走满的用例:它们验的是
    /// 「到点补第二枪」与「宽限内不提前开枪」,与宽限的绝对长短无关。
    #[cfg(test)]
    fn with_close_grace(inflight: InFlight, close_grace: Duration) -> Self {
        Self {
            close_grace,
            ..Self::new(inflight)
        }
    }

    /// 对一条已经加载、且此刻空闲的 Codex 家族会话执行原生 app-server 控制方法。
    ///
    /// **只认 live session** 是安全边界，不是功能偷懒：控制命令不应为了“查状态/压缩”冷启一个
    /// 空 thread，更不能在 resume 失败时静默换成新会话。普通消息仍负责 checkout/resume；控制面
    /// 只作用于它已经建立的唯一 owner。`Help` 是纯本地清单，例外地不要求 live session。
    pub fn control_existing(
        &self,
        agent: &Agent,
        tag: &str,
        command: &RpcSlashCommand,
    ) -> anyhow::Result<String> {
        if command == &RpcSlashCommand::Help {
            return Ok(rpc_control_help());
        }
        let driver = CliDriver::from_kind(agent.cli_kind());
        anyhow::ensure!(
            matches!(driver, CliDriver::Codex | CliDriver::Traex),
            summon_i18n::format!("r9bd8e721aa")
        );
        anyhow::ensure!(self.inflight.accepting(), RpcControlFailure::Stopping);
        let key = PoolKey {
            agent: agent.name.clone(),
            tag: tag.to_string(),
        };
        // 与 checkout/evict 共用 per-key transition fence。只做 strong_count + try_lock 后立刻
        // 松 sessions 锁会留下 TOCTOU：另一线程可在控制调用拿 client 锁前 checkout 同一会话、
        // 让控制方法与 turn/start 交错。整段请求持 reservation，另一条 writer 无法进入。
        let _reservation = self.reserve_checkout(&key)?;
        let (client, thread_id, cwd, cached_ctx_tokens) = {
            let sessions = self
                .state
                .entries
                .lock()
                .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r2f8c309a65")))?;
            let session = sessions
                .get(&key)
                .and_then(PoolEntry::session)
                .with_context(|| RpcControlFailure::NotLoaded)?;
            anyhow::ensure!(session.is_running(), RpcControlFailure::Exited);
            anyhow::ensure!(session.proto.exclusively_idle(), RpcControlFailure::Busy);
            let SessionProto::Codex { client, .. } = &session.proto else {
                anyhow::bail!(summon_i18n::format!("r271ba9beb5"))
            };
            (
                Arc::clone(client),
                session.session_id.clone(),
                session.spawn_cwd.clone(),
                session.ctx_input_tokens,
            )
        };
        let mut client = client.lock().unwrap_or_else(PoisonError::into_inner);
        let mut observed_ctx = None;
        let markdown = run_rpc_control(
            &mut client,
            driver,
            &thread_id,
            cwd.as_deref(),
            cached_ctx_tokens,
            command,
            &mut observed_ctx,
        )?;
        drop(client);
        // 压缩后的占用只在上面那段等待里观测得到(压缩不开轮),记进跨轮存活的会话,
        // 空闲卡与角标下次现采就是新数字。
        if let Some((used, window)) = observed_ctx {
            self.record_ctx_budget(&agent.name, tag, used, window);
        }
        Ok(clip_rpc_markdown(&markdown, 8_000))
    }

    /// checkout 从查池到把新 Session 放回池全程持一个 per-key Opening reservation。`evict`
    /// 必须先拿同一状态机的 Closing，故不可能再出现“checkout 过了 fence、evict 摘旧、checkout
    /// 才 spawn”的 TOCTOU。
    fn reserve_checkout(&self, key: &PoolKey) -> anyhow::Result<CheckoutReservation> {
        let mut entries = self
            .state
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r2f8c309a65")))?;
        let deadline = Instant::now() + SESSION_CLOSE_GRACE * 2 + Duration::from_secs(1);
        loop {
            match entries.get_mut(key) {
                None => {
                    entries.insert(key.clone(), PoolEntry::Opening(None));
                    return Ok(CheckoutReservation {
                        state: Arc::clone(&self.state),
                        key: key.clone(),
                    });
                }
                Some(entry @ PoolEntry::Ready(_)) => {
                    let PoolEntry::Ready(session) =
                        std::mem::replace(entry, PoolEntry::Opening(None))
                    else {
                        unreachable!("matched Ready entry")
                    };
                    *entry = PoolEntry::Opening(Some(session));
                    return Ok(CheckoutReservation {
                        state: Arc::clone(&self.state),
                        key: key.clone(),
                    });
                }
                Some(PoolEntry::Opening(_)) => {
                    anyhow::bail!(summon_i18n::format!("rc6aa5bffe6", key.agent, key.tag))
                }
                Some(PoolEntry::Closing) => {
                    let now = Instant::now();
                    anyhow::ensure!(
                        now < deadline,
                        summon_i18n::format!("r015be125d8", key.agent, key.tag)
                    );
                    let (next, _) = self
                        .state
                        .changed
                        .wait_timeout(entries, deadline.saturating_duration_since(now))
                        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r35e5915aa8")))?;
                    entries = next;
                }
            }
        }
    }

    /// Ready owner 与 Closing fence 在同一把锁内交接。空池没有需要收敛的 owner，保持空池。
    fn reserve_closing(&self, key: &PoolKey) -> Option<Box<Session>> {
        let mut entries = self
            .state
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let deadline = Instant::now() + SESSION_SETUP_TIMEOUT + Duration::from_secs(1);
        loop {
            match entries.get_mut(key) {
                None | Some(PoolEntry::Closing) => return None,
                Some(entry @ PoolEntry::Ready(_)) => {
                    let PoolEntry::Ready(session) = std::mem::replace(entry, PoolEntry::Closing)
                    else {
                        unreachable!("matched Ready entry")
                    };
                    return Some(session);
                }
                Some(PoolEntry::Opening(_)) => {
                    let now = Instant::now();
                    if now >= deadline {
                        log(&summon_i18n::format!("r1073d92b71", key.agent, key.tag));
                        return None;
                    }
                    let (next, _) = self
                        .state
                        .changed
                        .wait_timeout(entries, deadline.saturating_duration_since(now))
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    entries = next;
                }
            }
        }
    }

    /// 会话进程此刻还活着的 (agent, tag),按字典序。fleet 观测面用:无头会话还在不在,只有池知道。
    ///
    /// 池里有条目不等于活着:进程崩了或被外杀之后,条目要等这个 chat 下一次 checkout 才摘,所以
    /// 逐条按 [`Session::is_running`] 判。evict 之后还在收敛的进程已经不在池里,不算活着。
    /// 纯读:不摘条目、不发信号、不碰 `InFlight`。锁毒化时照样读出里面的数据(读到的仍是真值)。
    pub fn live_sessions(&self) -> Vec<(String, String)> {
        let sessions = self
            .state
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut live: Vec<(String, String)> = sessions
            .iter()
            .filter(|(_, entry)| entry.session().is_some_and(Session::is_running))
            .map(|(key, _)| (key.agent.clone(), key.tag.clone()))
            .collect();
        live.sort();
        live
    }

    /// 读取一条池会话供 Bridge 展示。锁毒化显式报错，不能把「读不了」折成「不存在」。
    pub fn handoff_snapshot(
        &self,
        agent: &str,
        tag: &str,
    ) -> anyhow::Result<Option<RpcHandoffSnapshot>> {
        let sessions = self
            .state
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r718742d7dd")))?;
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        let Some(entry) = sessions.get(&key) else {
            return Ok(None);
        };
        let Some(session) = entry.session() else {
            return Ok(None);
        };
        let running = session.is_running();
        let idle = !entry.transitioning()
            && running
            && session.proto.exclusively_idle()
            && !self
                .inflight
                .contains(&TurnKey::new(agent.to_string(), tag.to_string()));
        Ok(Some(RpcHandoffSnapshot {
            session_id: session.session_id.clone(),
            spawn_cwd: session.spawn_cwd.clone(),
            pgid: session.pgid,
            running,
            idle,
        }))
    }

    /// Opening、Ready、Closing 都占住 target，旧 writer 证明 Gone 前不能迁入。
    pub fn handoff_target_free(&self, agent: &str, tag: &str) -> anyhow::Result<bool> {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        let sessions = self
            .state
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("ra456beef3e")))?;
        Ok(!sessions.contains_key(&key))
    }

    /// 把 same-agent 的一条 live idle 会话从 source tag exact 迁到 target tag。
    ///
    /// 这是**内存所有权迁移**，不碰 `.session`：上层随后以 resume_store 的 atomic transfer 作为
    /// 持久化提交点；失败则调 `rollback_rekey`。本函数同时搬三份运行时身份：PoolKey、
    /// `InFlight rpc:` key、`rpc_procs` tag。任一校验/登记簿 IO 失败时前两者零变化，且不发信号。
    pub fn rekey_idle_exact(
        &self,
        agent: &str,
        source_tag: &str,
        target_tag: &str,
        expected: &RpcHandoffSnapshot,
    ) -> anyhow::Result<RpcRekeyReceipt> {
        self.rekey_idle_exact_inner(agent, source_tag, target_tag, expected, None)
    }

    /// 按成功回执反向 exact rekey。target 已被 checkout、进程已死/替换、SID/cwd/identity 变化时
    /// 一律拒绝，不会为了“回滚成功”覆盖后来者。
    pub fn rollback_rekey(&self, receipt: &RpcRekeyReceipt) -> anyhow::Result<()> {
        let expected = RpcHandoffSnapshot {
            session_id: receipt.session_id.clone(),
            spawn_cwd: receipt.spawn_cwd.clone(),
            pgid: receipt.pgid,
            running: true,
            idle: true,
        };
        let reversed = self.rekey_idle_exact_inner(
            &receipt.agent,
            &receipt.target_tag,
            &receipt.source_tag,
            &expected,
            Some(&receipt.identity),
        )?;
        anyhow::ensure!(
            reversed.pgid == receipt.pgid,
            summon_i18n::format!("rcffcce9d39", receipt.pgid, reversed.pgid)
        );
        Ok(())
    }

    fn rekey_idle_exact_inner(
        &self,
        agent: &str,
        source_tag: &str,
        target_tag: &str,
        expected: &RpcHandoffSnapshot,
        expected_identity: Option<&ChildKiller>,
    ) -> anyhow::Result<RpcRekeyReceipt> {
        anyhow::ensure!(!agent.is_empty(), summon_i18n::format!("r7290ebf598"));
        anyhow::ensure!(!source_tag.is_empty(), summon_i18n::format!("rbdc53fa160"));
        anyhow::ensure!(!target_tag.is_empty(), summon_i18n::format!("r1d59971df5"));
        anyhow::ensure!(
            source_tag != target_tag,
            summon_i18n::format!("r92da5a853f")
        );
        anyhow::ensure!(
            !expected.session_id.is_empty(),
            summon_i18n::format!("r83e95dfc85")
        );
        anyhow::ensure!(
            expected.running && expected.idle,
            summon_i18n::format!("r39c9afd548")
        );

        let source = PoolKey {
            agent: agent.to_string(),
            tag: source_tag.to_string(),
        };
        let target = PoolKey {
            agent: agent.to_string(),
            tag: target_tag.to_string(),
        };
        // 阶段与 owner 同锁校验，Closing target 也属于已占用，不能被迁入覆盖。
        let mut sessions = self
            .state
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r7ae35ba14a")))?;
        anyhow::ensure!(
            !sessions.get(&source).is_some_and(PoolEntry::transitioning)
                && !sessions.get(&target).is_some_and(PoolEntry::transitioning),
            summon_i18n::format!("r4c2f464a75")
        );
        anyhow::ensure!(
            !sessions.contains_key(&target),
            summon_i18n::format!("r52eb239b75")
        );
        let session = sessions
            .get(&source)
            .and_then(PoolEntry::session)
            .with_context(|| summon_i18n::format!("r9fb75926dc"))?;
        anyhow::ensure!(
            session.session_id == expected.session_id,
            summon_i18n::format!("reeebdc3930", expected.session_id, session.session_id)
        );
        anyhow::ensure!(
            session.spawn_cwd == expected.spawn_cwd,
            summon_i18n::format!("r2e31b60b0c", expected.spawn_cwd, session.spawn_cwd)
        );
        anyhow::ensure!(
            session.pgid == expected.pgid,
            summon_i18n::format!("r29c837a41c", expected.pgid, session.pgid)
        );
        anyhow::ensure!(session.is_running(), summon_i18n::format!("r2ce0fd76db"));
        anyhow::ensure!(
            session.proto.exclusively_idle(),
            summon_i18n::format!("r231fe4ab43")
        );
        if let Some(expected) = expected_identity {
            anyhow::ensure!(
                session.killer.same_handle(expected),
                summon_i18n::format!("re04de150a4")
            );
        }
        let identity = session.killer.clone();
        let pgid = session.pgid;
        let session_id = session.session_id.clone();
        let spawn_cwd = session.spawn_cwd.clone();

        let source_process_key = inflight_key(agent, source_tag);
        let target_process_key = inflight_key(agent, target_tag);
        let blockers = [
            TurnKey::new(agent.to_string(), source_tag.to_string()),
            TurnKey::new(agent.to_string(), target_tag.to_string()),
        ];
        // guard 持住 InFlight 锁直至 rpc_procs 原子 retag 完成；retag 失败时 Drop 自动恢复 source key。
        let rekey = self.inflight.begin_rekey_exact(
            &source_process_key,
            &target_process_key,
            &identity,
            &blockers,
        )?;
        crate::capture::rpc_procs::retag_exact(agent, source_tag, target_tag, pgid)
            .context(summon_i18n::text!("r325e53c7d4"))?;

        // 同一把 sessions 锁内刚验证过，remove/insert 不再有并发失败点。pending 是常驻进程的
        // Worker 调试快照，不参与恢复；仍同步改 tag，避免停机日志继续把它标成 source。
        let Some(entry) = sessions.remove(&source) else {
            // 理论上不可达；仍把已提交的 rpc_procs tag exact 搬回去，InFlight guard 随返回回滚。
            crate::capture::rpc_procs::retag_exact(agent, target_tag, source_tag, pgid)
                .context(summon_i18n::text!("r790d417625"))?;
            anyhow::bail!(summon_i18n::format!("rb2860d304d"));
        };
        let session = entry
            .session()
            .expect("source was Ready under the same lock");
        if let Ok(mut pending) = session.registration.pending.lock() {
            pending.chat_id = target_tag.to_string();
        }
        if let Some(config_dir) = session.cursor_config_dir.as_deref() {
            crate::capture::cursor_context::register_acp_session(
                agent,
                target_tag,
                config_dir.path(),
                &session.session_id,
            );
        }
        sessions.insert(target, entry);
        rekey.commit();

        Ok(RpcRekeyReceipt {
            agent: agent.to_string(),
            source_tag: source_tag.to_string(),
            target_tag: target_tag.to_string(),
            session_id,
            spawn_cwd,
            pgid,
            identity,
        })
    }

    /// 取一个可用会话:进程活着且 cwd 没换就复用,否则 spawn 新进程并按盘上 sid 决定接回还是冷启。
    /// 拿到会话之后再按 per-agent bundle 收敛 Cursor 模型与参数，不重开进程。
    ///
    /// **停机屏障在最前面**:热路径(`Reused`)不经 `admit`,老实现于是让 freeze 之后的轮照常
    /// 拿到会话、再在轮级 `InFlight::insert` 上被拒 —— 那条拒绝路径当时会零宽限 SIGKILL 掉
    /// 这条跨轮会话。前置成「本轮干净失败」既不碰会话进程,也把失败点挪到了它该在的地方。
    pub fn checkout(&self, chat: &RpcChat<'_>) -> anyhow::Result<PooledClient> {
        if !self.inflight.accepting() {
            bail!(summon_i18n::format!("r4a08a4984e"));
        }
        let key = PoolKey {
            agent: chat.agent.name.clone(),
            tag: chat.tag.to_string(),
        };
        // `evict` 先摘池、异步给旧进程 SIGINT 宽限。没有这个逻辑 fence，Clean 后紧跟的消息
        // 会在旧 writer 尚活的 2 秒内 exact resume 同一 SID，直接形成双 writer。
        let reservation = self.reserve_checkout(&key)?;
        let mut strict = self.strict_resume_record(chat)?;
        let driver = CliDriver::from_kind(chat.agent.cli_kind());
        let require_fast_mode =
            driver == CliDriver::Codex && chat.service_tier == Some(CodexServiceTier::Fast);
        // 先只在锁里做判定与摘除,**绝不持锁 spawn**:握手 + 开会话可能要十几秒,持锁会把
        // 其它 chat 的 checkout 一起堵住(每 chat 一条线程,本就该互不相干)。
        let stale = {
            let sessions = self
                .state
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match sessions.get(&key).and_then(PoolEntry::session) {
                Some(session) if session.usable_for(chat.cwd, require_fast_mode) => {
                    if let Some(expected) = &strict {
                        anyhow::ensure!(
                            session.session_id == expected.id,
                            summon_i18n::format!("rb318f4e177", expected.id, session.session_id)
                        );
                    }
                    let mut client = session.handle(Checkout::Reused);
                    let refreshed_miss = session.model_catalog_miss_generation;
                    drop(sessions);
                    if let Some(expected) = &strict {
                        if let Err(error) = resume_store::clear_strict_checked(
                            chat.agent,
                            chat.session,
                            &expected.cli,
                            &expected.id,
                        ) {
                            // exact 活池已经证明是同一会话；清位失败只让下一代继续 strict resume，
                            // 不需要打断本轮，也绝不能覆盖记录。
                            log(&summon_i18n::format!("rb9cabbb787", chat.agent.name));
                        }
                    }
                    let error = match self.sync_cursor_configuration(
                        &key,
                        &mut client,
                        driver,
                        chat.agent,
                        chat.model,
                    ) {
                        Ok(()) => {
                            self.sync_cursor_mode(&key, &mut client, driver, chat.cursor_mode)?;
                            return Ok(client);
                        }
                        Err(error) => error,
                    };
                    let Some(miss) = error.downcast_ref::<CursorModelCatalogMiss>() else {
                        return Err(error);
                    };
                    if refreshed_miss == Some(miss.generation) {
                        return Err(error);
                    }
                    // 目录可能是这个常驻进程启动时的旧快照：收掉进程，按盘上 SID 接回同一会话，
                    // 用新进程下发的目录再收敛一次。上下文随 `session/load` 保留。
                    log(&summon_i18n::format!("r8e2f05d7c6", key.agent, key.tag));
                    drop(client);
                    {
                        let sessions = self
                            .state
                            .entries
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let idle = sessions
                            .get(&key)
                            .and_then(PoolEntry::session)
                            .is_some_and(|session| session.proto.exclusively_idle());
                        if !idle {
                            return Err(error);
                        }
                    }
                    // 上面可能已清掉 strict 位；按盘上现状重判，接回仍只认同一条记录。
                    strict = self.strict_resume_record(chat)?;
                    Some(reservation.take_session()?)
                }
                Some(session) if !session.proto.exclusively_idle() => {
                    anyhow::bail!(summon_i18n::format!("rd899b6cf86", key.agent, key.tag))
                }
                Some(_) => {
                    drop(sessions);
                    Some(reservation.take_session()?)
                }
                None => None,
            }
        };
        if let Some(stale) = stale {
            // cwd 变化或进程失活都不能直接摘登记后 spawn：旧 client 可能仍被上一轮持有，
            // Drop 也只保证 leader 回收，不自动证明整组 Gone。Opening reservation 仍在手里，
            // 同步收敛并证明 Gone 后才释放旧身份、允许下面创建 replacement。
            self.converge_stale_session(&key, stale)?;
        }

        let (session, mode) = self.start_session(chat, driver, strict.as_ref())?;
        let mut client = session.handle(mode);
        if let Err(session) = reservation.install(session) {
            // Opening reservation 期间 rekey/checkout/evict 都不能写同 key；若未来新增路径绕过
            // transition fence，宁可同步收掉刚开的 owner 并报不变量错误，也不能覆盖池中旧进程或
            // 只摘 InFlight 后把新进程泄成孤儿。
            drop(client);
            let identity = session.killer.clone();
            let pgid = session.pgid;
            let alive = Arc::clone(&session.alive);
            self.settle_failed_spawn(chat, &identity, pgid, alive, session);
            anyhow::bail!(summon_i18n::format!("r3f0e79b4da", key.agent, key.tag));
        }
        // 新建/接回的会话也要对一次模型:`session/load` 接回的是**上次那个**模型,
        // 而配置可能在会话不在的这段时间里改过。
        if let Err(error) =
            self.sync_cursor_configuration(&key, &mut client, driver, chat.agent, chat.model)
        {
            if let Some(miss) = error.downcast_ref::<CursorModelCatalogMiss>() {
                self.note_model_catalog_miss(&key, miss.generation);
            }
            return Err(error);
        }
        self.sync_cursor_mode(&key, &mut client, driver, chat.cursor_mode)?;
        Ok(client)
    }

    fn strict_resume_record(
        &self,
        chat: &RpcChat<'_>,
    ) -> anyhow::Result<Option<summon_core::session_id::SessionRecord>> {
        let Some(record) = resume_store::read_record_checked(chat.agent, chat.session)? else {
            return Ok(None);
        };
        if !record.strict_resume {
            return Ok(None);
        }
        let channel = chat.agent.resume_cli_kind();
        anyhow::ensure!(
            record.cli == channel,
            summon_i18n::format!("ree74d5e7aa", record.cli)
        );
        anyhow::ensure!(
            record.cwd.as_deref() == chat.cwd,
            summon_i18n::format!("r8c467ce6da", record.cwd, chat.cwd)
        );
        Ok(Some(record))
    }

    /// Cursor parameterized model + options 的统一收敛点。目标是本群合成目标(默认 bundle
    /// 叠本群稀疏覆盖)；每个 ACP 进程使用独占物理配置根，别的群改档写不进本进程。
    /// 任一值未 exact 生效就 fail-stop，绝不能用旧模型/旧计费档继续跑。
    fn sync_cursor_configuration(
        &self,
        key: &PoolKey,
        client: &mut PooledClient,
        driver: CliDriver,
        agent: &Agent,
        _explicit: Option<&str>,
    ) -> anyhow::Result<()> {
        if driver != CliDriver::Cursor {
            return Ok(());
        }
        match crate::capture::cursor_config::with_agent_lock(agent, || {
            self.sync_cursor_configuration_locked(key, client, agent)
        }) {
            Ok(result) => result,
            Err(error) => {
                log(&summon_i18n::format!(
                    "rcd99591770",
                    key.agent,
                    error.diagnostic()
                ));
                Err(anyhow::anyhow!(error.to_string()))
            }
        }
    }

    fn sync_cursor_configuration_locked(
        &self,
        key: &PoolKey,
        client: &mut PooledClient,
        agent: &Agent,
    ) -> anyhow::Result<()> {
        let fresh = agent.reread().unwrap_or_else(|| agent.clone());
        let target =
            crate::capture::cursor_config::load_target(&fresh, &key.tag).map_err(|error| {
                log(&summon_i18n::format!(
                    "rb3c3fa9d06",
                    key.agent,
                    key.tag,
                    error.diagnostic()
                ));
                anyhow::anyhow!(error.to_string())
            })?;
        let available = cursor_model_axis_values(&client.config_options).unwrap_or_else(|| {
            self.state
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(key)
                .and_then(PoolEntry::session)
                .map(|session| session.available_models.clone())
                .unwrap_or_default()
        });

        // Legacy `runtime.rpc_model` 已由 `cursor_config::load` 一次性解析进 view。若 view 仍没有
        // selected_model，顶层 `model` 只有与 dynamic base id **原样 exact** 时才可接管；dash
        // suffix 的“猜测映射”会丢掉 thinking/reasoning/context，desired_options 又为空，等于静默降档。
        // `Agent::model()` 会 trim，这里故意读 raw field：opaque model id 不允许先规范化
        // 再冒充 dynamic catalog 里的另一个 token。
        let top_level_model = fresh.model.as_deref();
        let desired_model = match target.model() {
            Some(model) => Some(model.to_string()),
            None => match top_level_model {
                None => None,
                Some(model) => available
                    .iter()
                    .find(|candidate| candidate.as_str() == model)
                    .cloned()
                    .or_else(|| {
                        let lossy = summon_core::cursor_acp::match_cursor_model(model, &available);
                        if lossy.is_some() {
                            log(&summon_i18n::format!("rf02e76f761", key.agent, lossy));
                        }
                        None
                    }),
            },
        };
        let Some(desired_model) = desired_model else {
            if top_level_model.is_some() {
                log(&summon_i18n::format!(
                    "r9225a5fb95",
                    key.agent,
                    top_level_model,
                    available.len()
                ));
                bail!(summon_i18n::format!("r69c7ade77e"));
            }
            return Ok(());
        };

        let mut desired_options = if target.model() == Some(desired_model.as_str()) {
            target.options().into_iter().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        desired_options.sort_by_key(|(id, _)| cursor_config_apply_rank(id));

        for (config_id, _) in &desired_options {
            anyhow::ensure!(
                config_id != "model" && config_id != "mode",
                summon_i18n::format!("rf148537726")
            );
        }

        // 只有 strict 目录且**这一代整向量**已经验收过，缓存相等才有资格短路。新 session、
        // 旧 generation 或曾失败的半收敛态都强制先写一次 model，拿 setter 的 full catalog
        // exact 回执刷新事实；这同时堵住 false→true→false 的 ABA。
        let force_catalog_refresh = !client.config_catalog_trusted
            || client.config_applied_generation != Some(target.generation());
        let mut last_mismatches = Vec::new();
        for pass in 0..CURSOR_CONFIG_SYNC_MAX_PASSES {
            self.set_cursor_config_value(
                key,
                client,
                "model",
                &desired_model,
                pass == 0 && force_catalog_refresh,
                target.generation(),
            )?;
            for (config_id, value) in &desired_options {
                self.set_cursor_config_value(
                    key,
                    client,
                    config_id,
                    value,
                    false,
                    target.generation(),
                )?;
            }
            last_mismatches =
                cursor_config_vector_mismatches(client, &desired_model, &desired_options);
            if last_mismatches.is_empty() {
                break;
            }
            if pass + 1 < CURSOR_CONFIG_SYNC_MAX_PASSES {
                log(&summon_i18n::format!(
                    "re4616f6815",
                    key.agent,
                    pass + 1,
                    last_mismatches.len(),
                    last_mismatches.join(",")
                ));
            }
        }
        anyhow::ensure!(
            last_mismatches.is_empty(),
            summon_i18n::format!("r5f116cab1a", last_mismatches.join(","))
        );
        let observed =
            crate::capture::cursor_config::load_target(&fresh, &key.tag).map_err(|error| {
                log(&summon_i18n::format!(
                    "re25447b10d",
                    key.agent,
                    error.diagnostic()
                ));
                anyhow::anyhow!(error.to_string())
            })?;
        anyhow::ensure!(
            observed.generation() == target.generation(),
            summon_i18n::format!("rfb6568267b")
        );
        // 只有 full-vector exact + 两层 generation 复核都通过，才允许下一轮同代走缓存短路。
        client.config_applied_generation = Some(target.generation());
        self.record_cursor_catalog(key, client);
        Ok(())
    }

    fn set_cursor_config_value(
        &self,
        key: &PoolKey,
        client: &mut PooledClient,
        config_id: &str,
        value: &str,
        force: bool,
        generation: crate::capture::cursor_config::CursorTargetGeneration,
    ) -> anyhow::Result<()> {
        let axis = client
            .config_options
            .iter()
            .find(|option| option.id == config_id)
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rf36e5ab800")))?;
        if !axis.options.iter().any(|option| option.value == value) {
            if config_id == "model" {
                return Err(CursorModelCatalogMiss {
                    model: value.to_string(),
                    generation,
                }
                .into());
            }
            bail!(summon_i18n::format!("r6a943bf505"));
        }
        if !force && client.config_catalog_trusted && axis.current_value.as_deref() == Some(value) {
            return Ok(());
        }
        let result = match client.lock() {
            LockedSession::Cursor(mut guard) => guard.set_config_option(
                &client.session_id,
                config_id,
                value,
                Some(CURSOR_MODE_SYNC_TIMEOUT),
            ),
            LockedSession::Codex(_) => return Ok(()),
        };
        let updated = match result {
            Ok(updated) => updated,
            Err(error) => {
                if matches!(&error, CursorAcpError::Timeout { .. }) {
                    // 超时是 outcome unknown：server 可能稍后才写配置。立即收掉该进程，下一轮
                    // 重新握手并按 bundle 重放，不能在同一连接上拿旧快照继续。
                    log(&summon_i18n::format!("raf40985fe3", key.agent));
                    client.killer.interrupt();
                    let deadline = std::time::Instant::now() + self.close_grace;
                    while client.is_running() && std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    if client.is_running() {
                        client.killer.kill();
                    }
                } else {
                    log(&summon_i18n::format!("r9343abab15", key.agent));
                }
                bail!(summon_i18n::format!("rdadb55e857"));
            }
        };
        client.config_options = updated;
        client.config_catalog_trusted = true;
        self.record_cursor_catalog(key, client);
        log(&summon_i18n::format!("r39eb1c7398", key.agent));
        Ok(())
    }

    fn record_cursor_catalog(&self, key: &PoolKey, client: &mut PooledClient) {
        let current_model = client
            .config_options
            .iter()
            .find(|option| option.id == "model")
            .and_then(|option| option.current_value.clone());
        let available_models = cursor_model_axis_values(&client.config_options);
        if current_model.is_some() {
            client.current_model = current_model.clone();
        }
        if let Ok(mut sessions) = self.state.entries.lock() {
            if let Some(session) = sessions.get_mut(key).and_then(PoolEntry::session_mut) {
                session.config_options = client.config_options.clone();
                session.config_catalog_trusted = client.config_catalog_trusted;
                session.config_applied_generation = client.config_applied_generation;
                if current_model.is_some() {
                    session.current_model = current_model;
                }
                if let Some(available_models) = available_models {
                    session.available_models = available_models;
                }
            }
        }
    }

    fn note_model_catalog_miss(
        &self,
        key: &PoolKey,
        generation: crate::capture::cursor_config::CursorTargetGeneration,
    ) {
        if let Ok(mut sessions) = self.state.entries.lock() {
            if let Some(session) = sessions.get_mut(key).and_then(PoolEntry::session_mut) {
                session.model_catalog_miss_generation = Some(generation);
            }
        }
    }

    /// Cursor 的会话模式只属于当前 ACP session：每次 checkout 按 per-chat 偏好补齐一次，
    /// `session/new` / `session/load` / daemon 懒重连都不会把其它 chat 的选择带进来。
    ///
    /// 有动态目录时先精确复核 id；目录缺失（旧 server）则把合法 token 交给 server 裁决。
    /// 切换失败必须阻止本轮 prompt：尤其从 agent 切到 plan/ask 时，沿用旧模式继续会让界面
    /// 宣称的权限边界与真实执行不一致。会话进程仍留在池里，用户修正配置后下一轮可直接重试。
    fn sync_cursor_mode(
        &self,
        key: &PoolKey,
        client: &mut PooledClient,
        driver: CliDriver,
        desired: Option<&CursorSessionMode>,
    ) -> anyhow::Result<()> {
        if driver != CliDriver::Cursor {
            return Ok(());
        }
        let Some(desired) = desired else {
            return Ok(());
        };
        let want = desired.as_str();
        let available = {
            let sessions = self
                .state
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            sessions
                .get(key)
                .and_then(PoolEntry::session)
                .map(|session| session.available_modes.clone())
                .unwrap_or_default()
        };
        if !available.is_empty() && !available.iter().any(|mode| mode.id == want) {
            log(&summon_i18n::format!(
                "r0ddd1b892c",
                key.agent,
                available.len(),
                client.current_mode
            ));
            bail!(summon_i18n::format!("r5478160dc0"));
        }
        if client.current_mode.as_deref() == Some(want) {
            return Ok(());
        }
        let switched = match client.lock() {
            LockedSession::Cursor(mut guard) => {
                guard.set_mode(&client.session_id, want, Some(CURSOR_MODE_SYNC_TIMEOUT))
            }
            LockedSession::Codex(_) => return Ok(()),
        };
        match switched {
            Ok(()) => {
                log(&summon_i18n::format!("rad51ec9366", key.agent));
                client.current_mode = Some(want.to_string());
                if let Ok(mut sessions) = self.state.entries.lock() {
                    if let Some(session) = sessions.get_mut(key).and_then(PoolEntry::session_mut) {
                        session.current_mode = Some(want.to_string());
                    }
                }
                Ok(())
            }
            Err(error) => {
                log(&summon_i18n::format!("r62f557e139", key.agent));
                bail!(summon_i18n::format!("rc837ff2fae"))
            }
        }
    }

    /// 某 chat 当前会话报出来的可选模型(ACP `availableModels`)。
    ///
    /// `/model` 面板的候选来自 parameterized model 目录。池里没有这条会话就返回空,候选退回
    /// 配置桶(见 `cards::model_candidates`)。
    pub fn available_models(&self, agent: &str, tag: &str) -> Vec<String> {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        self.state
            .entries
            .lock()
            .map(|sessions| {
                sessions
                    .get(&key)
                    .and_then(PoolEntry::session)
                    .map(|session| session.available_models.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// 该 chat 会话此刻生效的模型(`current_model`)。锁只做快照。
    pub fn current_model(&self, agent: &str, tag: &str) -> Option<String> {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        self.state.entries.lock().ok().and_then(|sessions| {
            sessions
                .get(&key)
                .and_then(PoolEntry::session)
                .and_then(|s| s.current_model.clone())
        })
    }

    /// 某 chat 当前 Cursor ACP 会话动态公布的模式目录。
    pub fn available_modes(&self, agent: &str, tag: &str) -> Vec<CursorAcpMode> {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        self.state
            .entries
            .lock()
            .map(|sessions| {
                sessions
                    .get(&key)
                    .and_then(PoolEntry::session)
                    .map(|session| session.available_modes.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// 某 chat 当前 Cursor ACP session 的生效模式。
    pub fn current_mode(&self, agent: &str, tag: &str) -> Option<String> {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        self.state.entries.lock().ok().and_then(|sessions| {
            sessions
                .get(&key)
                .and_then(PoolEntry::session)
                .and_then(|s| s.current_mode.clone())
        })
    }

    /// 某 chat 当前 Cursor ACP 会话公布的参数化配置目录。只返回 exact chat：同一 agent 的
    /// 其它会话可能尚未收敛到最新 model，借它的目录渲染会把旧模型选项伪装成当前可用。
    pub fn cursor_config_options(&self, agent: &str, tag: &str) -> Vec<CursorAcpConfigOption> {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        self.state
            .entries
            .lock()
            .map(|sessions| {
                sessions
                    .get(&key)
                    .and_then(PoolEntry::session)
                    .map(|session| session.config_options.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// 记下这条会话最新的周额度剩余百分比。池里没有这条会话就丢弃(会话已被摘 → 这份观测
    /// 已经没有归属),**绝不为它新建会话**。锁中毒时静默返回:额度是角标上的装饰,
    /// 不该因为它让一轮出错。
    /// 返回**值是否变了**(首次记入也算变),给调用方决定要不要记一条日志 —— 额度推送可能每轮
    /// 都来,只有变化才值得进日志。
    pub fn record_quota(&self, agent: &str, tag: &str, left_pct: u8) -> bool {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        let Ok(mut sessions) = self.state.entries.lock() else {
            return false;
        };
        let Some(session) = sessions.get_mut(&key).and_then(PoolEntry::session_mut) else {
            return false;
        };
        let changed = session.quota_left_pct != Some(left_pct);
        session.quota_left_pct = Some(left_pct);
        changed
    }

    pub fn record_ctx_budget(&self, agent: &str, tag: &str, used: u64, window: u64) -> bool {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        let Ok(mut sessions) = self.state.entries.lock() else {
            return false;
        };
        let Some(session) = sessions.get_mut(&key).and_then(PoolEntry::session_mut) else {
            return false;
        };
        let changed =
            session.ctx_input_tokens != Some(used) || session.ctx_window_tokens != Some(window);
        session.ctx_input_tokens = Some(used);
        session.ctx_window_tokens = Some(window);
        changed
    }

    pub fn clear_ctx_budget(&self, agent: &str, tag: &str) -> bool {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        let Ok(mut sessions) = self.state.entries.lock() else {
            return false;
        };
        let Some(session) = sessions.get_mut(&key).and_then(PoolEntry::session_mut) else {
            return false;
        };
        let changed = session.ctx_input_tokens.is_some() || session.ctx_window_tokens.is_some();
        session.ctx_input_tokens = None;
        session.ctx_window_tokens = None;
        changed
    }

    pub fn current_ctx_budget(&self, agent: &str, tag: &str) -> Option<(u64, u64)> {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        self.state.entries.lock().ok().and_then(|sessions| {
            let session = sessions.get(&key).and_then(PoolEntry::session)?;
            Some((session.ctx_input_tokens?, session.ctx_window_tokens?))
        })
    }

    /// 这条会话最后一次已知的周额度剩余百分比。锁只做快照。
    pub fn current_quota(&self, agent: &str, tag: &str) -> Option<u8> {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        self.state.entries.lock().ok().and_then(|sessions| {
            sessions
                .get(&key)
                .and_then(PoolEntry::session)
                .and_then(|s| s.quota_left_pct)
        })
    }

    /// 摘掉一个 chat 的会话:从池里移除 + **自带有界收敛**(SIGINT 整组 → 宽限 → SIGKILL),
    /// 收敛确认之后才注销 `InFlight`。返回是否真有会话被摘。
    ///
    /// 两条都是刻意的,各自堵一个窗口:
    ///
    /// - **不依赖 `InFlight::interrupt_one_bounded` 的延迟 SIGKILL**。那条路的第二枪要靠
    ///   sleep 后复查注册表还在不在;同步 evict 把注册摘了,它就永远不开枪 —— 一个屏蔽 SIGINT
    ///   的会话进程于是活到天荒地老。
    /// - **不依赖 client 的 `Drop`**。`Drop` 只在最后一个持有者释放时发生,而正在跑一轮的
    ///   chat 线程手上还攥着一份 `PooledClient`:从 evict 到它松手这段时间里,进程既不在池里、
    ///   也不在注册表里 —— 任何 kill 路径都够不着。推迟注销就把这个窗口关掉了。
    pub fn evict(&self, agent: &str, tag: &str) -> bool {
        let key = PoolKey {
            agent: agent.to_string(),
            tag: tag.to_string(),
        };
        let Some(session) = self.reserve_closing(&key) else {
            return false;
        };
        // Closing reservation 在摘池前已经与 checkout 的 Opening reservation 原子互斥。再挂上
        // 「收敛中」登记、摘掉「在用」登记，任何时刻旧组都既可被停机够到，又封住同 tag 新建。
        let closing = closing_key(agent, tag);
        let mut closing_registration = session.registration.clone();
        // 这条进程早已由 pool + rpc_procs 持有，不是“刚 spawn、登记失败就会变孤儿”的新进程。
        // daemon freeze/锁毒化时 closing insert 被拒也只能保留旧 key，绝不能走 OwnedProcess 的
        // 零宽限 SIGKILL 拒绝路径。
        closing_registration.shape = crate::runtime::inflight::TurnShape::SharedSession;
        let registered_key = if self.inflight.insert(closing.clone(), closing_registration) {
            self.inflight
                .remove_if(&inflight_key(agent, tag), &session.killer);
            closing
        } else {
            inflight_key(agent, tag)
        };

        let inflight = self.inflight.clone();
        // 注销身份用会话自己那把闸(`remove_if` 认 Arc 身份);发信号另起一把新闸 ——
        // 会话闸可能早被某一轮的升级用掉了(一次性),拿它发 SIGINT 会是空操作。
        let identity = session.killer.clone();
        let signal = ChildKiller::new(session.pgid);
        let (pgid, alive) = (session.pgid, Arc::clone(&session.alive));
        // 收敛线程只拥有摘下的 session，不延长池里其它 Ready owner 的 RAII 寿命。
        let state = Arc::downgrade(&self.state);
        let close_grace = self.close_grace;
        std::thread::spawn(move || {
            // 判活两个证据都要,同 [`Session::is_running`]:这里没人 `wait` 过这个 child,
            // 已死未 reap 的僵尸对 `killpg(0)` 照样返回真,只有 stdout EOF 才是进程真走了的证据。
            let running = || alive.load(Ordering::Acquire) && group_alive(pgid);
            signal.interrupt();
            let deadline = std::time::Instant::now() + close_grace;
            while running() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if group_alive(pgid) {
                log(&summon_i18n::format!("r3bf033c2b2"));
                signal.kill();
            }
            // 先 drop client 让 Child::drop/wait 有机会回收 leader；否则一个已经退出的僵尸仍会
            // 让 killpg(0) 报“组存在”，我们永远证明不了 Gone。
            drop(session);
            let gone_deadline = std::time::Instant::now() + SESSION_CLOSE_GRACE;
            while group_alive(pgid) && std::time::Instant::now() < gone_deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if group_alive(pgid) {
                // 不能证明 Gone 就保留 closing InFlight + 跨代登记。下一代 boot 会继续 exact
                // 收敛；现在注销会让一个幸存 writer 永久失联。
                log(&summon_i18n::format!("r1e2baeeaab"));
                return;
            }
            inflight.remove_if(&registered_key, &identity);
            crate::capture::rpc_procs::forget(pgid);
            if let Some(state) = state.upgrade() {
                let mut entries = state.entries.lock().unwrap_or_else(PoisonError::into_inner);
                if matches!(entries.get(&key), Some(PoolEntry::Closing)) {
                    entries.remove(&key);
                }
                state.changed.notify_all();
            }
        });
        true
    }

    /// 起进程 + 登记 + 开会话。Codex 和 Cursor resume 可立即落 sid；Cursor cold start
    /// 要延迟到首条 prompt 帧写出，与 PRIMER 消费使用同一提交点。
    fn start_session(
        &self,
        chat: &RpcChat<'_>,
        driver: CliDriver,
        strict: Option<&summon_core::session_id::SessionRecord>,
    ) -> anyhow::Result<(Session, Checkout)> {
        crate::capture::rpc_procs::preflight_recording()
            .context(summon_i18n::text!("r69dad43395"))?;
        // 盘上记录按 **agent 自己的 cli** 取:换了 cli 的旧 id 作废(防把 codex 的 thread id
        // 喂给 cursor)。与无头 / tmux 两路共用同一份记录，但按 transport 隔离命名空间。
        // 记录按 **通道** 取(`resume_cli_kind`,不是裸 cli 名):cursor 的 ACP sessionId 与
        // `-p --resume` 认的 chats id 是两个不互通的命名空间,串了不是丢上下文而是接错会话。
        let channel = chat.agent.resume_cli_kind();
        let recorded = match strict {
            Some(record) => {
                let current = resume_store::read_record(chat.agent, chat.session)
                    .context(summon_i18n::text!("rb7b9c41414"))?;
                anyhow::ensure!(current == *record, summon_i18n::format!("r9caaf5c2d9"));
                Some(record.id.clone())
            }
            None => resume_store::read_id_migrating(chat.agent, chat.session, chat.tag, &channel),
        };
        // **无条件读整条记录。** 不挂在 `recorded.is_some()` 上 —— `recorded` 经过 kind guard
        // (`read_id_migrating` 只在 `record.cli == channel` 时给值),而待 fork 的记录里那个
        // `cli` 是**发起 fork 时**那条源记录的通道名。agent 在 fork 落盘之后、第一轮之前换过 cli,
        // guard 就会把 `recorded` 滤成 None,于是 fork 记录明明完好却读不出来 → 静默冷启,
        // `fork_pending` 与血缘双双被抹掉,而目标群刚收到「已接上上下文副本」的回执。
        // fork 是显式命令,它的执行不该被 cli 变更悄悄取消(与 `plan_start` 那条路同一条纪律)。
        let on_disk = resume_store::read_record(chat.agent, chat.session);
        // `recorded_primer_slot` 仍然只在 `recorded` 有值时才有意义:槽位描述的是**要接回的那条
        // thread**,而 `recorded` 是 None 时本轮不接回,槽位无从谈起。
        let recorded_primer_slot = recorded
            .is_some()
            .then(|| {
                on_disk
                    .as_ref()
                    .and_then(|record| record.primer_slot.clone())
            })
            .flatten();
        // 待 fork 的源 sid。strict 与 fork 互斥:strict 意味着这条记录是 Bridge handoff 的
        // exact owner,那条路要求「只能恢复本记录的 id」,与 fork「从别处取历史」正相反。
        // 判据在 `fork::evaluate_fork` 就拒了 strict 源,这里是纵深防御。
        let fork_from = on_disk
            .as_ref()
            .filter(|record| record.fork_pending && !record.strict_resume)
            .and_then(|record| record.forked_from.as_ref().map(|origin| origin.sid.clone()));
        if recorded.is_none() && on_disk.is_some() && fork_from.is_none() {
            // 盘上有记录但通道对不上(典型:刚从 `capture: cli` 切到 ACP)。冷启是对的,
            // 但得说一声 —— 否则「上下文怎么没了」只能靠猜。
            log(&summon_i18n::format!("r4808415860", chat.agent.name));
        }
        let (session, mode) = match driver {
            CliDriver::Cursor => self.start_cursor(chat, recorded.as_deref(), strict.is_some())?,
            // codex 是另一条 RPC 协议;别的 CLI 到不了这里(`capture_mode` 已分流),真到了也按
            // codex 走 —— 同 `TurnEngine::build` 的兜底取舍。
            _ => self.start_codex(
                chat,
                recorded.as_deref(),
                recorded_primer_slot.as_deref(),
                strict.is_some(),
                fork_from.as_deref(),
            )?,
        };
        let cursor_cold_prompt_pending = driver == CliDriver::Cursor && mode == Checkout::ColdStart;
        if !cursor_cold_prompt_pending {
            if let Ok(mut pending) = session.registration.pending.lock() {
                pending.session_id = Some(session.session_id.clone());
            }
            // sid 落盘:下次 daemon 起来后就是靠它决定接回还是冷启。Cursor cold
            // 的空 SID 不能进这里；prompt_start 成功后 `on_session_id` 才会落盘。
            // created_at:resume 沿用原记录的首次冷启时间;冷启写当前时间。
            let created_at = if recorded.is_some() {
                resume_store::read_record(chat.agent, chat.session).and_then(|r| r.created_at)
            } else {
                Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string())
            };
            // 槽位判据按 **mode** 而不是 `recorded.is_some()`:resume 失败会退回冷启
            // (`start_codex` 里那条兜底),那时是一条**全新** thread,PRIMER 已经走了
            // developerInstructions,槽位就必须跟着翻新 —— 拿 `recorded` 判会把新 thread
            // 标成旧的,下次 resume 就不发 PRIMER 了。
            // cursor 不用这个位(ACP 没有对应字段),恒 None。
            let primer_slot = match (driver, mode) {
                (CliDriver::Cursor, _) => None,
                (_, Checkout::ColdStart) => Some(summon_core::session_id::PRIMER_SLOT_DEVELOPER),
                // Resumed 保留原槽位。Reused(池内命中)本来到不了 `start_session`,
                // 真到了也按「不改判据」处理 —— 槽位只应在**建线程**那一刻被决定。
                (_, Checkout::Resumed | Checkout::Reused) => recorded_primer_slot.as_deref(),
            };
            resume_store::write_with_primer_slot(
                chat.agent,
                chat.session,
                &channel,
                &session.session_id,
                chat.cwd,
                created_at.as_deref(),
                primer_slot,
                // 这一轮是不是 fork 兑现:只有走了 `thread/fork` 的那次
                // 才让血缘跨 sid 替换存活(冷启顶掉待 fork 记录时血缘必须丢,否则它在说谎)。
                fork_from.is_some(),
            );
        }
        Ok((session, mode))
    }

    /// spawn 之后**紧接着**登记进 `InFlight`:这中间的窗口里 daemon 若停机,这个进程组就没人认领。
    /// 返回登记用的伴随快照(session id 此刻还不知道,拿到后由调用方回填)。
    fn admit(&self, chat: &RpcChat<'_>, killer: &ChildKiller) -> anyhow::Result<InFlightTurn> {
        let registration = InFlightTurn {
            killer: killer.clone(),
            base: chat.agent.state_dir().clone(),
            pending: Arc::new(Mutex::new(idle_pending(chat))),
            // 此刻这个进程组**刚 spawn、还没有第二个主人**:登记被拒就必须当场收掉它,
            // 否则它脱离 daemon 变孤儿。轮级那条登记恰恰相反(`SharedSession`)——
            // 那时会话已经归池管了。
            shape: crate::runtime::inflight::TurnShape::OwnedProcess,
        };
        let admitted = self.inflight.insert(
            inflight_key(&chat.agent.name, chat.tag),
            registration.clone(),
        );
        if !admitted {
            // 停机屏障已立(或同键仍被占):`insert` 已经当场把这个新生进程组收掉了,别再把一个
            // 正在死的 client 交出去。
            bail!(summon_i18n::format!("rcb464c39bc"));
        }
        Ok(registration)
    }

    fn record_and_admit_spawn(
        &self,
        chat: &RpcChat<'_>,
        killer: &ChildKiller,
        pgid: i32,
    ) -> anyhow::Result<InFlightTurn> {
        if let Err(error) =
            crate::capture::rpc_procs::record(&chat.agent.name, chat.tag, chat.session, pgid, pgid)
        {
            // 调用方仍持有 client，只有它能在发信号后 wait leader。这里先尽量挂到本代
            // InFlight，返回后由调用方把 owner 交给 `settle_failed_spawn` 同步证明整组 Gone。
            let _ = self.admit(chat, killer);
            return Err(error).context(summon_i18n::text!("rfaf7015eb9"));
        }
        self.admit(chat, killer)
    }

    fn bind_spawned_session(&self, chat: &RpcChat<'_>, pgid: i32, sid: &str) -> anyhow::Result<()> {
        crate::capture::rpc_procs::bind_session_exact(
            &chat.agent.name,
            chat.tag,
            chat.session,
            pgid,
            sid,
        )
        .context(summon_i18n::text!("r83e8f061f3"))
    }

    fn converge_owned_process<T>(&self, pgid: i32, alive: Arc<AtomicBool>, owner: T) -> bool {
        let signal = ChildKiller::new(pgid);
        signal.interrupt();
        let graceful_deadline = std::time::Instant::now() + self.close_grace;
        while alive.load(Ordering::Acquire)
            && group_alive(pgid)
            && std::time::Instant::now() < graceful_deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        if group_alive(pgid) {
            signal.kill();
        }
        // client Drop 负责 wait 直系 child；它不一定是进程组最后一个成员，随后仍要独立证明 Gone。
        drop(owner);
        let gone_deadline = std::time::Instant::now() + SESSION_CLOSE_GRACE;
        while group_alive(pgid) && std::time::Instant::now() < gone_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        !group_alive(pgid)
    }

    fn settle_failed_spawn<T>(
        &self,
        chat: &RpcChat<'_>,
        killer: &ChildKiller,
        pgid: i32,
        alive: Arc<AtomicBool>,
        owner: T,
    ) {
        if self.converge_owned_process(pgid, alive, owner) {
            self.inflight
                .remove_if(&inflight_key(&chat.agent.name, chat.tag), killer);
            crate::capture::rpc_procs::forget(pgid);
        } else {
            // 进程登记和 InFlight 身份都保留；冻结本代，下一代 boot 继续 exact 收敛。
            self.inflight.freeze();
        }
    }

    fn converge_stale_session(&self, key: &PoolKey, stale: Box<Session>) -> anyhow::Result<()> {
        let pgid = stale.pgid;
        let identity = stale.killer.clone();
        let alive = Arc::clone(&stale.alive);
        if !self.converge_owned_process(pgid, alive, stale) {
            self.inflight.freeze();
            anyhow::bail!(summon_i18n::format!("r6844d3ad3d", key.agent, key.tag));
        }
        self.inflight
            .remove_if(&inflight_key(&key.agent, &key.tag), &identity);
        crate::capture::rpc_procs::forget(pgid);
        Ok(())
    }

    /// RPC/ACP 子进程的**完整**环境。
    ///
    /// 这两条 spawn 边界原先直接继承 daemon 的环境。daemon 自己可能带着上一个上下文的
    /// `SUMMON_*`(现场实测:rpc-procs 记着 PID 属于 agent A,`ps eww` 里却是
    /// agent B 的 `SUMMON_AGENT`),于是子进程里的 CLI 执行 `summon preview` / `chats` 等命令时
    /// 按错误身份认领,把内容发进别的群、读到别的 agent 的会话。
    ///
    /// 这里给全:系统基线 + `resolve_extra_env`(已挡掉 runtime-owned 键)+ 飞书隔离 +
    /// **本 chat 的身份三件套**。三件套全部来自入参 `RpcChat`,不做任何盘上反查 —— 反查是
    /// best-effort,失败会静默缺一件,子进程拿到半份身份反而更难查。
    fn child_env(chat: &RpcChat<'_>) -> Vec<(String, String)> {
        let mut env = crate::runtime::daemon::essential_system_env();
        // **走 `resolve_extra_env` 而不是裸 `agent.extra_env()`**:前者做占位符展开,而这里再把
        // runtime-owned 的键挡掉。裸取等于给 agent 配置留了个后门 —— 在 `runtime.env` 里写一行
        // `SUMMON_AGENT: 别人` 或 `FEISHU_APP_ID: …`,就能把刚清干净的身份重新注回去。
        for (key, value) in crate::runtime::daemon::resolve_extra_env(chat.agent) {
            if summon_core::agent_identity::is_runtime_owned_env_key(&key) {
                log(&summon_i18n::format!("r95e70f570e", chat.agent.name));
                continue;
            }
            env.push((key, value));
        }
        let (_, feishu) = crate::runtime::daemon::agent_feishu_isolation_tmux(chat.agent);
        env.extend(feishu);
        // 身份三件套最后写(覆盖上面任何来源),且**都从入参取** —— 不再 best-effort 反查 `.meta`:
        // 反查失败会静默缺一件,子进程于是拿到半份身份,比明确失败更难查。
        env.push(("SUMMON_AGENT".to_string(), chat.agent.name.clone()));
        env.push(("SUMMON_CONVERSATION_TAG".to_string(), chat.tag.to_string()));
        env.push(("SUMMON_CHAT_ID".to_string(), chat.chat_id.to_string()));
        env
    }

    /// `recorded_primer_slot` = 盘上那条 `.session` 记录的 `primer_slot`。
    ///
    /// 它决定 resume 要不要重发 PRIMER,**两个方向都会静默出错**:
    /// - 旧 thread(`None`)误发 → PRIMER 两份(history 里那份 baseInstructions 清不掉);
    /// - 新 thread(`developer`)漏发 → 这个 thread 从此没有身份。
    ///
    /// 两种都不报错,所以判据必须显式传进来,不能在里面猜。
    fn start_codex(
        &self,
        chat: &RpcChat<'_>,
        recorded: Option<&str>,
        recorded_primer_slot: Option<&str>,
        strict_resume: bool,
        fork_from: Option<&str>,
    ) -> anyhow::Result<(Session, Checkout)> {
        let fast_mode_enabled = chat.service_tier == Some(CodexServiceTier::Fast);
        let args = codex_app_server_args(
            chat.service_tier,
            chat.agent
                .runtime
                .as_ref()
                .and_then(|r| r.model_context_window),
        );
        let config = CodexAppServerConfig {
            // 走 agent 自己的 cli_cmd 覆盖(与无头路同一处解析),不硬编码 "codex"。
            program: chat.agent.cli_cmd(),
            args,
            work_dir: chat.cwd.map(PathBuf::from),
            env: Self::child_env(chat),
            // thread goal / background terminals 在当前 schema 里属于 experimental API；只给
            // chat 控制面 opt in，额度恢复/Menu Bar 等只读旁路保持默认关。
            experimental_api: true,
            ..CodexAppServerConfig::default()
        };
        let mut client = CodexAppServerClient::spawn(config)
            .with_context(|| summon_i18n::format!("r0c8182514a", chat.agent.name))?;
        let killer = client.killer();
        // 登记进跨代登记簿:daemon 若被 SIGKILL,`InFlight` 连同所有句柄一起消失,
        // 只有盘上这条记录能让下一代认出这个进程组是自己的(见 `rpc_procs`)。
        let registration = match self.record_and_admit_spawn(chat, &killer, client.pgid()) {
            Ok(registration) => registration,
            Err(error) => {
                let pgid = client.pgid();
                let alive = client.liveness();
                self.settle_failed_spawn(chat, &killer, pgid, alive, client);
                return Err(error);
            }
        };

        // resume 只对「建线程时就把 PRIMER 放进 developerInstructions」的 thread 重发。
        // 旧 thread 的 PRIMER 焊在 rollout 的 baseInstructions 里,codex 会自己读回来。
        let resume_primer = (recorded_primer_slot
            == Some(summon_core::session_id::PRIMER_SLOT_DEVELOPER))
        .then_some(chat.primer)
        .flatten();
        let opened = (|| {
            // 跨群 fork 先判:记录里那个 sid 在 codex 侧还不存在(它是 fork 的**目标**,由
            // `thread/fork` 的返回值回填),拿它去 resume 必然失败并退冷启 —— 目标群就拿到一个
            // 空会话 —— 而目标群的人刚发过那句 `/fork` 接受它。
            if let Some(source) = fork_from {
                let thread = client
                    .thread_fork_for_chat(
                        source,
                        chat.cwd,
                        chat.primer,
                        &chat.codex_policy,
                        Some(SESSION_SETUP_TIMEOUT),
                    )
                    .with_context(|| summon_i18n::format!("r193b03ec79"))?;
                // fork **不允许**退冷启:那会把「带着源群上下文」变成「空会话」,而两者对用户
                // 来说长得一样。宁可这一轮报错、记录留着下条消息重试。
                return Ok((thread.id, Checkout::ColdStart));
            }
            if let Some(sid) = recorded {
                match client.thread_resume(
                    sid,
                    chat.cwd,
                    resume_primer,
                    &chat.codex_policy,
                    Some(SESSION_SETUP_TIMEOUT),
                ) {
                    Ok(thread) if strict_resume && thread.id != sid => {
                        anyhow::bail!(summon_i18n::format!("r658d36d0f7", thread.id))
                    }
                    Ok(thread) => return Ok((thread.id, Checkout::Resumed)),
                    Err(error) if strict_resume => {
                        return Err(error).context(summon_i18n::format!("re9981f7ed5"))
                    }
                    // rollout 被删 / thread 不存在:重连失败不该让这个 chat 从此哑掉。丢上下文比丢
                    // 服务轻,退回冷启并留日志。
                    Err(error) => log(&summon_i18n::format!("rc78e5a2125")),
                }
            }
            let dynamic_tools = chat
                .register_platform_tools
                .then(crate::codex::codex_app_server::summon_dynamic_tools);
            let thread = client
                .thread_start_with_dynamic_tools(
                    chat.cwd,
                    chat.primer,
                    &chat.codex_policy,
                    dynamic_tools.as_ref(),
                    Some(SESSION_SETUP_TIMEOUT),
                )
                .context(summon_i18n::text!("rba577f8291"))?;
            Ok((thread.id, Checkout::ColdStart))
        })();
        let (session_id, mode) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let pgid = client.pgid();
                let alive = client.liveness();
                self.settle_failed_spawn(chat, &killer, pgid, alive, client);
                return Err(error);
            }
        };
        if let Err(error) = self.bind_spawned_session(chat, client.pgid(), &session_id) {
            let pgid = client.pgid();
            let alive = client.liveness();
            self.settle_failed_spawn(chat, &killer, pgid, alive, client);
            return Err(error);
        }
        // 开会话时主动问一次额度,**不等推送**:`account/rateLimits/updated` 只在额度变化时来,
        // 光靠它的话一条新会话的头几轮角标上没有这一项。查失败一律忽略并降级(额度是角标上的
        // 装饰,不该让一条能用的会话开不起来);推送照常继续增量更新这个值。
        let quota_observation = match client.account_rate_limits_read(Some(QUOTA_READ_TIMEOUT)) {
            Ok(limits) => {
                record_codex_account_rate_limits_shared(
                    Some((&chat.agent.name, chat.tag)),
                    &limits,
                );
                limits
                    .codex()
                    .and_then(|snapshot| snapshot.weekly_left_pct_and_reset_at())
            }
            Err(error) => {
                log(&summon_i18n::format!("r65cc152ab4"));
                None
            }
        };
        let quota_left_pct = quota_observation.map(|(left_pct, _)| left_pct);
        // 顺带把 backend 的模型目录抓进进程内缓存:面板候选与 onboard 默认值都以它为准,配置桶
        // 只剩冷启兜底。每次开会话都问一遍(开会话本身是低频事件),这样上游今天加的模型,
        // 下一条新会话就能在面板上点到。查失败一律忽略并降级,理由同上面的额度。
        match client.model_list(Some(MODEL_LIST_READ_TIMEOUT)) {
            Ok(catalog) => {
                crate::codex::model_catalog::record_for_agent(chat.agent, catalog);
            }
            Err(error) => log(&summon_i18n::format!("rc174cfac0d")),
        }

        let session = Session {
            alive: client.liveness(),
            pgid: client.pgid(),
            proto: SessionProto::Codex {
                interrupt: client.interrupt_handle(),
                client: Arc::new(Mutex::new(client)),
            },
            killer,
            session_id,
            // codex 的模型每轮经 `turn/start` 下发,与进程/会话无关。
            current_model: None,
            available_models: Vec::new(),
            current_mode: None,
            available_modes: Vec::new(),
            config_options: Vec::new(),
            config_catalog_trusted: false,
            config_applied_generation: None,
            model_catalog_miss_generation: None,
            cursor_primer_pending: Arc::new(AtomicBool::new(false)),
            cursor_config_dir: None,
            quota_left_pct,
            ctx_input_tokens: None,
            ctx_window_tokens: None,
            spawn_cwd: chat.cwd.map(str::to_string),
            fast_mode_enabled,
            registration,
        };
        Ok((session, mode))
    }

    fn start_cursor(
        &self,
        chat: &RpcChat<'_>,
        recorded: Option<&str>,
        strict_resume: bool,
    ) -> anyhow::Result<(Session, Checkout)> {
        let config_dir = Arc::new(
            crate::capture::cursor_config::process_config_dir(chat.agent).map_err(|error| {
                log(&summon_i18n::format!(
                    "rbafc368498",
                    chat.agent.name,
                    error.diagnostic()
                ));
                anyhow::anyhow!(error.to_string())
            })?,
        );
        let config = CursorAcpConfig {
            // 走 agent 自己的 cli_cmd 覆盖(与无头路同一处解析),不硬编码 "cursor-agent"。
            program: chat.agent.cli_cmd(),
            // **不带 `--model`**:真机实测它对 acp 子命令无效,模型与参数统一走动态 setter。
            args: CursorAcpConfig::acp_args(),
            work_dir: chat.cwd.map(PathBuf::from),
            config_dir: config_dir.path().to_path_buf(),
            request_timeout: Duration::from_secs(30),
            env: Self::child_env(chat),
        };
        let mut client = CursorAcpClient::spawn(config)
            .with_context(|| summon_i18n::format!("r110d844034", chat.agent.name))?;
        let killer = client.killer();
        // 同 codex 那条:紧挨着 spawn 落跨代登记,见 `rpc_procs`。
        let registration = match self.record_and_admit_spawn(chat, &killer, client.pgid()) {
            Ok(registration) => registration,
            Err(error) => {
                let pgid = client.pgid();
                let alive = client.liveness();
                self.settle_failed_spawn(chat, &killer, pgid, alive, client);
                return Err(error);
            }
        };

        let opened = (|| {
            if let Some(sid) = recorded {
                match client.session_load(sid, chat.cwd, Some(SESSION_SETUP_TIMEOUT)) {
                    Ok(session) => return Ok((session, Checkout::Resumed)),
                    // 聊天记录被删 / id 不存在:同 codex,丢上下文比丢服务轻。
                    Err(error) => {
                        log(&summon_i18n::format!("r8305e7f1c6"));
                        // **失败路径也必须清重放**:`session/load` 是先重放整段历史、再回响应,
                        // 失败(超时 / -32602 / 记录已删)时那批重放已经排进队里了,而成功路径的
                        // drain 压根没跑到。不清就会被下一轮当成本轮内容整段重发 —— 正是
                        // `session_load_drops_the_replayed_history_before_the_next_turn` 防的那件事,
                        // 只不过发生在它没覆盖的那一半路径上。
                        let dropped = client.drain_queued_notifications();
                        if dropped > 0 {
                            log(&summon_i18n::format!("rba7397e3f0"));
                        }
                        client.clear_context_usage();
                        if strict_resume {
                            return Err(error).context(summon_i18n::format!("rde8cafc903"));
                        }
                    }
                }
            }
            let session = client
                .session_new(chat.cwd, Some(SESSION_SETUP_TIMEOUT))
                .context(summon_i18n::text!("r82dd99bb39"))?;
            Ok((session, Checkout::ColdStart))
        })();
        let (opened, mode) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let pgid = client.pgid();
                let alive = client.liveness();
                self.settle_failed_spawn(chat, &killer, pgid, alive, client);
                return Err(error);
            }
        };
        if let Err(error) = self.bind_spawned_session(chat, client.pgid(), &opened.session_id) {
            let pgid = client.pgid();
            let alive = client.liveness();
            self.settle_failed_spawn(chat, &killer, pgid, alive, client);
            return Err(error);
        }

        crate::capture::cursor_context::register_acp_session(
            &chat.agent.name,
            chat.tag,
            config_dir.path(),
            &opened.session_id,
        );
        let store_budget = crate::capture::cursor_context::registered_budget(
            &chat.agent.name,
            chat.tag,
            &opened.session_id,
        )
        .unwrap_or_default();
        crate::observe::event_log::append_model_catalog(
            &chat.agent.state_dir(),
            chat.tag,
            &cursor_catalog_note(&opened),
            summon_core::event_log::EventStamp {
                agent: Some(chat.agent.name.clone()),
                model: opened.current_model_id.clone(),
                turn: None,
            },
        );
        let context_usage = client.context_usage();
        let session = Session {
            alive: client.liveness(),
            pgid: client.pgid(),
            proto: SessionProto::Cursor {
                interrupt: client.interrupt_handle(),
                client: Arc::new(Mutex::new(client)),
            },
            killer,
            session_id: opened.session_id,
            current_model: opened.current_model_id,
            available_models: cursor_model_axis_values(&opened.config_options)
                .unwrap_or(opened.available_models),
            current_mode: opened.current_mode_id,
            available_modes: opened.available_modes,
            config_options: opened.config_options,
            config_catalog_trusted: opened.config_options_trusted,
            config_applied_generation: None,
            model_catalog_miss_generation: None,
            cursor_primer_pending: Arc::new(AtomicBool::new(mode == Checkout::ColdStart)),
            cursor_config_dir: Some(config_dir),
            // cursor 的 ACP 面不下发额度,这条恒 None。
            quota_left_pct: None,
            ctx_input_tokens: context_usage
                .map(|usage| usage.used_tokens)
                .or(store_budget.used_tokens),
            ctx_window_tokens: context_usage
                .map(|usage| usage.window_tokens)
                .or(store_budget.window_tokens),
            spawn_cwd: chat.cwd.map(str::to_string),
            fast_mode_enabled: false,
            registration,
        };
        Ok((session, mode))
    }
}

fn codex_app_server_args(
    service_tier: Option<CodexServiceTier>,
    model_context_window: Option<u64>,
) -> Vec<String> {
    // 原生 requestUserInput 是 app-server feature gate；生产会话始终启用，真正是否转交人类
    // 由 adapter 的精确 thread+turn/secret 闸决定。
    let mut args = vec![
        "app-server".into(),
        "--enable".into(),
        "default_mode_request_user_input".into(),
    ];
    if service_tier == Some(CodexServiceTier::Fast) {
        // feature 与收费档位正交：只有用户显式选 Fast 才覆盖全局 disable；Standard/legacy
        // 继续尊重用户自己的 Codex 配置。若热会话此前未启用，checkout 会只重建 app-server、
        // 随后 resume 同一 thread，不碰 tmux、不丢上下文。
        args.extend(["--enable".into(), "fast_mode".into()]);
    }
    // per-agent 的上下文窗口覆盖:不写死在全局 config.toml / traecli.toml(bot 之间隔离),
    // 通过 -c 传给 app-server。没配 → 不传,尊重 CLI 自己的全局配置。
    if let Some(cw) = model_context_window {
        args.extend(["-c".into(), format!("model_context_window={cw}")]);
    }
    // 用 --listen stdio:// 而非 --stdio:codex 两者都支持,但 traex(codex fork)只认 --listen,
    // 传 --stdio 会报 "unexpected argument" 导致 app-server 起不来。
    args.extend(["--listen".into(), "stdio://".into()]);
    args
}

/// 进程内会话池里进程还活着的会话。池还没建(这一代 daemon 没开过任何 RPC 会话)= 一个都没有。
pub fn live_sessions_shared() -> Vec<(String, String)> {
    SHARED.get().map(RpcPool::live_sessions).unwrap_or_default()
}

/// 摘掉共享池里某 chat 的会话 —— 池**还没被初始化**就什么都不做(返回 false)。
///
/// 给够不着 daemon 那份 `InFlight` 句柄的调用点用(卡片路的配置变更快重置)。不给它一个
/// `RpcPool::new` 的口子是刻意的:新造一份注册表就等于把进程挂到没人停机的表上(见
/// [`RpcPool::new`] 的注释),而「池还没建 = 没有任何 RPC 会话」本来就是要的语义。
pub fn evict_shared(agent: &str, tag: &str) -> bool {
    SHARED.get().is_some_and(|pool| pool.evict(agent, tag))
}

/// 在生产共享池上执行一条 slash-facing RPC 控制指令。池尚未初始化 = 不存在 live session，
/// 明确失败而不是为了控制命令新造一份池/进程。
pub fn control_existing_shared(
    agent: &Agent,
    tag: &str,
    command: &RpcSlashCommand,
) -> anyhow::Result<String> {
    if command == &RpcSlashCommand::Help {
        return Ok(rpc_control_help());
    }
    SHARED
        .get()
        .with_context(|| RpcControlFailure::NotLoaded)?
        .control_existing(agent, tag, command)
}

/// 共享池里某 chat 的会话报出来的可选模型。池没建 / 没这条会话 → 空(见 [`RpcPool::available_models`])。
pub fn available_models_shared(agent: &str, tag: &str) -> Vec<String> {
    SHARED
        .get()
        .map(|pool| pool.available_models(agent, tag))
        .unwrap_or_default()
}

/// 共享池里某 chat 会话**此刻生效**的模型(ACP `currentModelId`,冷启即从 `session/new` 填、
/// `set_model` 成功后更新)。面板「当前模型」显示它,而不是只读 agent 配置 —— cursor 常没配
/// `rpc_model`,配置侧是 None,但会话真有一个在跑的模型。池没建 / 没这条会话 → None。
pub fn current_model_shared(agent: &str, tag: &str) -> Option<String> {
    SHARED.get().and_then(|pool| pool.current_model(agent, tag))
}

/// 共享池里某 Cursor chat 的 live mode catalog；没有会话时为空。
pub fn available_modes_shared(agent: &str, tag: &str) -> Vec<CursorAcpMode> {
    SHARED
        .get()
        .map(|pool| pool.available_modes(agent, tag))
        .unwrap_or_default()
}

/// 共享池里某 Cursor chat 当前生效的会话模式；没有会话时为 None。
pub fn current_mode_shared(agent: &str, tag: &str) -> Option<String> {
    SHARED.get().and_then(|pool| pool.current_mode(agent, tag))
}

/// 共享池里 exact Cursor chat 的参数化配置目录；没有 live 会话时为空。
pub fn cursor_config_options_shared(agent: &str, tag: &str) -> Vec<CursorAcpConfigOption> {
    SHARED
        .get()
        .map(|pool| pool.cursor_config_options(agent, tag))
        .unwrap_or_default()
}

/// 当前进程最后一次观测到的**额度剩余**百分比。账号额度按 agent 共享；exact chat 的
/// RPC session 值只作为尚未进入账号缓存时的兼容回退。
pub fn current_quota_shared(agent: &str, tag: &str) -> Option<u8> {
    current_quota_with_reset_shared(agent, tag).map(|(left_pct, _)| left_pct)
}

/// 同 [`current_quota_shared`],另带这份观测的重置时刻(Unix 秒)。百分比与重置时刻出自同一条
/// 观测;会话级回退值没有重置时刻。
pub fn current_quota_with_reset_shared(agent: &str, tag: &str) -> Option<(u8, Option<i64>)> {
    quota_with_upcoming_reset(latest_quota(agent), unix_now_secs())
        .or_else(|| current_session_quota_without_reset(agent, tag))
}

/// 默认 Codex 后台账号观测没有 chat 身份；只有调用方已证明 agent 使用同一默认账号时才能
/// 调用此入口。Trae、自定义 home/env/包装器必须只读 [`current_quota_shared`]。
pub fn current_default_codex_quota_shared(agent: &str, tag: &str) -> Option<u8> {
    current_default_codex_quota_with_reset_shared(agent, tag).map(|(left_pct, _)| left_pct)
}

/// [`current_default_codex_quota_shared`] 带重置时刻的版本,调用前提相同。
pub fn current_default_codex_quota_with_reset_shared(
    agent: &str,
    tag: &str,
) -> Option<(u8, Option<i64>)> {
    quota_with_upcoming_reset(
        newer_quota(latest_quota(DEFAULT_CODEX_ACCOUNT), latest_quota(agent)),
        unix_now_secs(),
    )
    .or_else(|| current_session_quota_without_reset(agent, tag))
}

/// 已经过去的重置时刻描述的是上一个窗口,不随百分比一起交出去。
fn quota_with_upcoming_reset(quota: Option<LatestQuota>, now: i64) -> Option<(u8, Option<i64>)> {
    quota.map(|quota| {
        (
            quota.left_pct,
            quota.resets_at.filter(|resets_at| *resets_at > now),
        )
    })
}

fn current_session_quota_without_reset(agent: &str, tag: &str) -> Option<(u8, Option<i64>)> {
    SHARED
        .get()
        .and_then(|pool| pool.current_quota(agent, tag))
        .map(|left_pct| (left_pct, None))
}

/// 账号最后一次观测到的 **5 小时窗口**:(剩余百分比, 重置时刻 Unix 秒)。只有 claude 与 codex 系
/// 会写入。
pub fn current_five_hour_quota_shared(agent: &str) -> Option<(u8, Option<i64>)> {
    latest_five_hour_quota(agent, unix_now_secs()).map(|quota| (quota.left_pct, quota.resets_at))
}

/// 5 小时窗口版的 [`current_default_codex_quota_shared`],调用前提相同。
pub fn current_default_codex_five_hour_quota_shared(agent: &str) -> Option<(u8, Option<i64>)> {
    let now = unix_now_secs();
    newer_quota(
        latest_five_hour_quota(DEFAULT_CODEX_ACCOUNT, now),
        latest_five_hour_quota(agent, now),
    )
    .map(|quota| (quota.left_pct, quota.resets_at))
}

/// 记下 agent 账号最新的 5 小时窗口剩余百分比(claude statusLine 的实时观测)。
pub fn record_five_hour_quota_observation_shared(
    agent: &str,
    left_pct: u8,
    resets_at: Option<i64>,
    observed_at: Option<i64>,
) {
    record_latest_quota_in(
        latest_five_hour_quota_cache(),
        agent,
        left_pct,
        resets_at,
        observed_at,
        QuotaOrigin::Live,
    );
}

pub fn agent_uses_default_codex_account(agent: &Agent) -> bool {
    if summon_core::driver::CliDriver::from_kind(agent.cli_kind())
        != summon_core::driver::CliDriver::Codex
        || std::env::var_os("CODEX_HOME").is_some()
    {
        return false;
    }
    agent.runtime.as_ref().is_none_or(|runtime| {
        runtime.cli_cmd.is_none()
            && runtime
                .env
                .as_ref()
                .is_none_or(std::collections::HashMap::is_empty)
    })
}

pub fn current_ctx_budget_shared(agent: &str, tag: &str) -> Option<(u64, u64)> {
    SHARED
        .get()
        .and_then(|pool| pool.current_ctx_budget(agent, tag))
}

/// 记下 agent 账号最新的额度剩余百分比，并同步更新仍存活的 exact chat RPC session。
/// 即使共享池或 session 尚未建立，账号快照也会保留。返回账号值是否变化。
/// `observed_at` 是这份观测产生的 Unix 秒:从盘上快照读来的传快照落盘时刻,实时读到的传当下。
pub fn record_quota_observation_shared(
    agent: &str,
    tag: &str,
    left_pct: u8,
    resets_at: Option<i64>,
    observed_at: Option<i64>,
) -> bool {
    record_quota_observation_with_origin(
        agent,
        tag,
        left_pct,
        resets_at,
        observed_at,
        QuotaOrigin::Live,
    )
}

fn record_quota_observation_with_origin(
    agent: &str,
    tag: &str,
    left_pct: u8,
    resets_at: Option<i64>,
    observed_at: Option<i64>,
    origin: QuotaOrigin,
) -> bool {
    let (accepted, changed) = record_latest_quota(agent, left_pct, resets_at, observed_at, origin);
    if accepted {
        if let Some(pool) = SHARED.get() {
            pool.record_quota(agent, tag, left_pct);
        }
    }
    changed
}

/// 主动拉取的额度:整值替换(`Some`)或清掉(`None`)该 agent 的账号额度。
///
/// 不走 [`quota_observation_is_newer`] 的新旧排序 —— 那套排序防的是被动推送的乱序,而主动拉取
/// 拿的是当前凭据此刻的答案,本身就是最新的。换了账号时新账号的周期可能反而更早结束,按窗口
/// 排序会把它当成迟到的旧观测丢掉,角标就一直挂着旧账号的额度。只写账号缓存、不写会话池:
/// 清掉时不能留一份会话级的旧值兜底。
pub fn replace_account_quota_shared(
    agent: &str,
    quota: Option<(u8, Option<i64>)>,
    observed_at: i64,
) {
    let Ok(mut quotas) = latest_quota_cache().lock() else {
        return;
    };
    match quota {
        Some((left_pct, resets_at)) => {
            quotas.insert(
                agent.to_string(),
                LatestQuota {
                    origin: QuotaOrigin::Live,
                    left_pct: left_pct.min(100),
                    resets_at,
                    observed_at: Some(observed_at),
                },
            );
        }
        None => {
            quotas.remove(agent);
        }
    }
}

/// 吸收 Codex `account/rateLimits/read` 与额度推送这类**刚发生**的观测,时刻记当下。
/// `agent_chat=None` 用于没有 chat 身份的默认 Codex 后台账号观测；带身份时只更新该
/// agent，避免 Trae 等兼容 app-server 的独立账号污染默认 Codex 账号。
pub fn record_codex_quota_snapshot_shared(
    agent_chat: Option<(&str, &str)>,
    snapshot: &CodexRateLimitSnapshot,
) -> bool {
    record_codex_quota_snapshot_with_origin(
        agent_chat,
        snapshot,
        Some(unix_now_secs()),
        QuotaOrigin::Live,
    )
}

/// 带明确观测时刻的入口,给 rollout 回放用:daemon 启动会从头重放 8 天内的 rollout,每一行
/// 都有自己的 `timestamp`,按它排先后才不会让旧文件末尾那行压过真正最新的观测。
pub fn record_codex_quota_snapshot_at(
    agent_chat: Option<(&str, &str)>,
    snapshot: &CodexRateLimitSnapshot,
    observed_at: Option<i64>,
) -> bool {
    record_codex_quota_snapshot_with_origin(agent_chat, snapshot, observed_at, QuotaOrigin::Rollout)
}

fn record_codex_quota_snapshot_with_origin(
    agent_chat: Option<(&str, &str)>,
    snapshot: &CodexRateLimitSnapshot,
    observed_at: Option<i64>,
    origin: QuotaOrigin,
) -> bool {
    record_codex_five_hour_reset(agent_chat, snapshot, observed_at);
    if let Some((left_pct, resets_at)) = snapshot.five_hour_left_pct_and_reset_at() {
        record_latest_quota_in(
            latest_five_hour_quota_cache(),
            agent_chat
                .map(|(agent, _)| agent)
                .unwrap_or(DEFAULT_CODEX_ACCOUNT),
            left_pct,
            resets_at,
            observed_at,
            origin,
        );
    }
    let Some((left_pct, resets_at)) = snapshot.weekly_left_pct_and_reset_at() else {
        return false;
    };
    match agent_chat {
        Some((agent, tag)) => record_quota_observation_with_origin(
            agent,
            tag,
            left_pct,
            resets_at,
            observed_at,
            origin,
        ),
        None => {
            record_latest_quota(
                DEFAULT_CODEX_ACCOUNT,
                left_pct,
                resets_at,
                observed_at,
                origin,
            )
            .1
        }
    }
}

pub fn record_codex_account_rate_limits_shared(
    agent_chat: Option<(&str, &str)>,
    limits: &CodexAccountRateLimits,
) -> bool {
    limits
        .codex()
        .is_some_and(|snapshot| record_codex_quota_snapshot_shared(agent_chat, snapshot))
}

/// 登记进 `InFlight` 的伴随快照。
///
/// `Worker` 种类是关键:常驻进程**不是一轮**,停机时既不该落 `pending.json`(重跑什么?),也不该给
/// owner 发「会话被打断」通知 —— 那两件事都由真正的那一轮自己的注册条目负责。这里只借 `InFlight`
/// 的「可 kill 子进程注册表」那一半语义。
fn idle_pending(chat: &RpcChat<'_>) -> PendingTurn {
    PendingTurn {
        kind: PendingTurnKind::Worker,
        delivery_scope: String::new(),
        agent_name: chat.agent.name.clone(),
        inbox_message_ids: Vec::new(),
        resume_authorized: false,
        // Worker 种类下这个字段只供调试(见 `PendingTurn.chat_id` 注释),存 tag。
        chat_id: chat.tag.to_string(),
        chat_type: String::new(),
        cli: chat.agent.cli_kind().to_string(),
        prompt: String::new(),
        session_id: None,
        partial: String::new(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pool_lifecycle_unwind_releases_an_empty_opening() {
        let pool = RpcPool::new(InFlight::new());
        let key = PoolKey {
            agent: "probe".into(),
            tag: "opening".into(),
        };
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _reservation = pool.reserve_checkout(&key).unwrap();
            assert!(!pool.handoff_target_free(&key.agent, &key.tag).unwrap());
            assert!(pool.reserve_checkout(&key).is_err());
            panic!("opening aborted");
        }));
        assert!(unwind.is_err());
        assert!(pool.handoff_target_free(&key.agent, &key.tag).unwrap());
        assert!(pool.reserve_checkout(&key).is_ok());
    }

    #[test]
    fn pool_lifecycle_unwind_restores_the_same_ready_owner() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());
        let client = pool.checkout(&chat(&agent)).unwrap();
        let pgid = client.pgid();
        drop(client);
        let key = PoolKey {
            agent: agent.name.clone(),
            tag: TAG.into(),
        };
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _reservation = pool.reserve_checkout(&key).unwrap();
            assert_eq!(pool.live_sessions(), [(agent.name.clone(), TAG.into())]);
            assert!(pool.record_ctx_budget(&agent.name, TAG, 17, 100));
            panic!("configuration aborted");
        }));
        assert!(unwind.is_err());
        let reused = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(reused.checkout_mode(), Checkout::Reused);
        assert_eq!(reused.pgid(), pgid);
        assert_eq!(pool.current_ctx_budget(&agent.name, TAG), Some((17, 100)));
        assert_eq!(sent(tmp.path(), "thread/start").len(), 1);
        drop(reused);
        assert!(pool.evict(&agent.name, TAG));
    }

    #[test]
    fn pool_lifecycle_opening_owner_is_visible_but_not_a_handoff_candidate() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());
        drop(pool.checkout(&chat(&agent)).unwrap());
        let ready = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        assert!(ready.running && ready.idle);
        let key = PoolKey {
            agent: agent.name.clone(),
            tag: TAG.into(),
        };
        let reservation = pool.reserve_checkout(&key).unwrap();
        let opening = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        assert!(opening.running);
        assert!(!opening.idle);
        assert_eq!(opening.pgid, ready.pgid);
        assert!(pool
            .rekey_idle_exact(&agent.name, TAG, "target", &ready)
            .is_err());
        drop(reservation);
        assert_eq!(
            pool.handoff_snapshot(&agent.name, TAG).unwrap(),
            Some(ready)
        );
        assert!(pool.evict(&agent.name, TAG));
    }

    #[test]
    fn pool_lifecycle_evict_waits_for_opening_and_wakes_checkout_after_gone() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::with_close_grace(InFlight::new(), Duration::from_millis(40));
        let client = pool.checkout(&chat(&agent)).unwrap();
        let pgid = client.pgid();
        drop(client);
        let key = PoolKey {
            agent: agent.name.clone(),
            tag: TAG.into(),
        };
        let reservation = pool.reserve_checkout(&key).unwrap();
        let closing_pool = pool.clone();
        let name = agent.name.clone();
        let (started, entered) = std::sync::mpsc::channel();
        let (done, evicted) = std::sync::mpsc::channel();
        let closer = std::thread::spawn(move || {
            started.send(()).unwrap();
            done.send(closing_pool.evict(&name, TAG)).unwrap();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            evicted.recv_timeout(Duration::from_millis(40)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(group_alive(pgid));
        drop(reservation);
        assert!(evicted.recv_timeout(Duration::from_secs(2)).unwrap());
        closer.join().unwrap();
        let replacement = pool.checkout(&chat(&agent)).unwrap();
        assert!(
            !group_alive(pgid),
            "replacement must wait for the old writer to be Gone"
        );
        assert_ne!(replacement.pgid(), pgid);
        assert_eq!(replacement.checkout_mode(), Checkout::Resumed);
        assert_eq!(sent(tmp.path(), "thread/start").len(), 1);
        assert_eq!(sent(tmp.path(), "thread/resume").len(), 1);
        drop(replacement);
        assert!(pool.evict(&agent.name, TAG));
    }

    #[test]
    fn pool_lifecycle_closer_does_not_retain_other_ready_sessions() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let script = write_sigint_immune_app_server(tmp.path(), &codex_open_cases());
        let (agent, _tmp) = agent_pointing_at("codex", &script, tmp);
        let (other, _other_tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::with_close_grace(InFlight::new(), Duration::from_secs(1));
        let closing = pool.checkout(&chat(&agent)).unwrap();
        let closing_pgid = closing.pgid();
        drop(closing);
        let ready = pool
            .checkout(&RpcChat {
                session: "soul-test-other",
                tag: "other",
                ..chat(&other)
            })
            .unwrap();
        let ready_pgid = ready.pgid();
        drop(ready);
        assert!(pool.evict(&agent.name, TAG));
        drop(pool);
        assert!(
            !group_alive(ready_pgid),
            "dropping the pool must release unrelated Ready owners immediately"
        );
        assert!(wait_until(Duration::from_secs(3), || !group_alive(
            closing_pgid
        )));
    }

    /// **污染父环境后,`child_env` 给出的必须是本 chat 自己的身份,一个字都不能继承。**
    ///
    /// 现场(2026-08-14):`rpc-procs.json` 记着 PID 属于 agent A,`ps eww` 里却是
    /// agent B 的 `SUMMON_AGENT` —— 全新 spawn、没经过 takeover,污染来自 daemon 自身的环境,
    /// 而 ACP/app-server 两条 spawn 边界都没 `env_clear`。子进程里的 CLI 据此认领错误 agent:
    /// 发错群、读到别的 agent 的会话。
    ///
    /// 这条测的是**环境构造**这一半(spawn 时 `env_clear` + 只注入它,由 `spawn` 保证)。
    #[test]
    fn child_env_never_inherits_a_polluted_identity() {
        // **必须持锁**:本测试改的是**进程全局**环境。不持锁并发跑时,它会把 `SUMMON_*` 泼给
        // 同进程里其它正在跑的测试 —— 实测让 `another_thread_can_interrupt_…` 间歇性红。
        let _guard = crate::test_support::process_test_guard();
        let (agent, _tmp) = crate::test_support::agent_dir("cursor", "");
        // 模拟被污染的父进程环境。
        std::env::set_var("SUMMON_AGENT", "flare-5");
        std::env::set_var("SUMMON_CHAT_ID", "oc_someone_else");
        std::env::set_var("SUMMON_CONVERSATION_TAG", "gffffffffffffffff");

        let chat = RpcChat {
            agent: &agent,
            session: "soul-probe-gaaaaaaaaaaaaaaaa",
            tag: "gaaaaaaaaaaaaaaaa",
            chat_id: "oc_mine",
            cwd: None,
            primer: None,
            model: None,
            service_tier: None,
            cursor_mode: None,
            codex_policy: Default::default(),
            register_platform_tools: false,
        };
        let env = RpcPool::child_env(&chat);
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());

        assert_eq!(
            get("SUMMON_AGENT").as_deref(),
            Some(agent.name.as_str()),
            "身份必须来自本 chat 的 agent,不能是父环境里的 flare-5"
        );
        assert_eq!(
            get("SUMMON_CONVERSATION_TAG").as_deref(),
            Some("gaaaaaaaaaaaaaaaa"),
            "tag 必须来自本 chat"
        );
        assert_ne!(
            get("SUMMON_CHAT_ID").as_deref(),
            Some("oc_someone_else"),
            "绝不能把父环境里别人的 chat_id 带下去"
        );

        std::env::remove_var("SUMMON_AGENT");
        std::env::remove_var("SUMMON_CHAT_ID");
        std::env::remove_var("SUMMON_CONVERSATION_TAG");
    }

    use super::*;
    use crate::codex::codex_app_server::{
        CodexAppServerError, CodexAppServerNotification, RESUME_ALLOWED_KEYS,
    };
    use crate::test_support::{
        fake_rpc_server_log, process_test_guard, write_fake_acp_server, write_fake_app_server,
        write_sigint_immune_app_server,
    };
    use serde_json::Value;
    use std::time::Instant;

    const TAG: &str = "gdeadbeefdeadbeef";
    const SESSION: &str = "soul-test-gdeadbeefdeadbeef";

    #[test]
    fn codex_app_server_enables_user_input_and_only_adds_fast_for_explicit_fast() {
        // 用 --listen stdio:// 而非 --stdio:codex 两者都支持,但 traex(codex fork)
        // 只认 --listen,传 --stdio 会报 "unexpected argument" 导致 app-server 起不来。
        assert_eq!(
            codex_app_server_args(None, None),
            [
                "app-server",
                "--enable",
                "default_mode_request_user_input",
                "--listen",
                "stdio://"
            ]
        );
        assert_eq!(
            codex_app_server_args(Some(CodexServiceTier::Standard), None),
            [
                "app-server",
                "--enable",
                "default_mode_request_user_input",
                "--listen",
                "stdio://"
            ]
        );
        assert_eq!(
            codex_app_server_args(Some(CodexServiceTier::Fast), None),
            [
                "app-server",
                "--enable",
                "default_mode_request_user_input",
                "--enable",
                "fast_mode",
                "--listen",
                "stdio://"
            ]
        );
    }

    #[test]
    fn codex_app_server_args_passes_model_context_window_via_config_override() {
        // per-agent 配了 model_context_window → 通过 -c 传给 app-server,
        // 不写死在全局 traecli.toml / config.toml 里(bot 之间隔离)。
        let args = codex_app_server_args(None, Some(1_000_000));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1] == "model_context_window=1000000"),
            "expected -c model_context_window=1000000 in {args:?}"
        );
        // 没配 → 不传 -c,尊重 CLI 自己的全局配置。
        let args = codex_app_server_args(None, None);
        assert!(!args.contains(&"-c".to_string()));
    }

    /// 起一个假 app-server + 指向它的 codex agent。返回 `(agent, tempdir)`,tempdir 同时是假 server
    /// 的请求日志目录 —— 断言「发出去的帧」全看那份 log。
    fn agent_with_fake_server(cases: &str) -> (Agent, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_app_server(tmp.path(), cases);
        agent_pointing_at("codex", &script, tmp)
    }

    fn traex_agent_with_fake_server(cases: &str) -> (Agent, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_app_server(tmp.path(), cases);
        agent_pointing_at("traex", &script, tmp)
    }

    /// 同上,但起假 ACP server + cursor agent。
    fn cursor_agent_with_fake_server(cases: &str) -> (Agent, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_acp_server(tmp.path(), cases);
        agent_pointing_at("cursor", &script, tmp)
    }

    fn cursor_agent_with_parameter_seed(
        cases: &str,
        rpc_model: &str,
    ) -> (Agent, tempfile::TempDir) {
        let (mut agent, tmp) = cursor_agent_with_fake_server(cases);
        agent.runtime.as_mut().unwrap().rpc_model = Some(rpc_model.to_string());
        (agent, tmp)
    }

    fn agent_pointing_at(
        cli: &str,
        script: &std::path::Path,
        tmp: tempfile::TempDir,
    ) -> (Agent, tempfile::TempDir) {
        // 池只被 Rpc 引擎使用,池测试的 agent 一律显式 `capture: rpc` 与生产对齐——
        // cursor 默认通道已是 CliSession,不写的话 resume 通道键会落到 CLI 侧,接回全变冷启。
        let yaml = format!(
            "name: test\nruntime:\n  cli: {cli}\n  capture: rpc\n  cli_cmd: {}\n",
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

    fn chat<'a>(agent: &'a Agent) -> RpcChat<'a> {
        RpcChat {
            agent,
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
        }
    }

    /// 端到端:配置里的策略要真的落到 `thread/start` / `thread/resume` 的线上。
    ///
    /// 中间隔着 `Agent::codex_policy`(分层解析)→ `RpcChat` → params 三层,任何一层
    /// 漏传都不会报错,只会静默用回默认值 —— 那恰恰是这次要修的那类问题。
    #[test]
    fn a_configured_policy_reaches_the_wire_on_both_cold_start_and_reconnect() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());
        let policy = summon_core::codex_policy::CodexPolicy {
            sandbox: "workspace-write".into(),
            approval_policy: "untrusted".into(),
        };
        fn chat_of<'a>(
            agent: &'a Agent,
            policy: &summon_core::codex_policy::CodexPolicy,
        ) -> RpcChat<'a> {
            RpcChat {
                codex_policy: policy.clone(),
                ..chat(agent)
            }
        }

        let first = pool.checkout(&chat_of(&agent, &policy)).unwrap();
        kill_and_await_death(&first);
        drop(first);
        drop(pool.checkout(&chat_of(&agent, &policy)).unwrap());

        let starts = sent(tmp.path(), "thread/start");
        assert_eq!(starts[0]["params"]["sandbox"], "workspace-write");
        assert_eq!(starts[0]["params"]["approvalPolicy"], "untrusted");

        let resumes = sent(tmp.path(), "thread/resume");
        assert_eq!(resumes.len(), 1);
        assert_eq!(
            resumes[0]["params"]["sandbox"], "workspace-write",
            "codex 不持久化 sandbox,重连不带就退回机器默认"
        );
        assert_eq!(resumes[0]["params"]["approvalPolicy"], "untrusted");
    }

    /// 注册平台工具的判据是**调用方这一轮装没装配处理器**,不是 cli 种类。
    ///
    /// 反过来(注册了没人处理)的形态是:模型的工具列表里多出几个调了必失败的条目 ——
    /// 它不报错,只是每次都白费一次工具调用。所以两个方向都要钉。
    #[test]
    fn platform_tools_are_registered_only_when_the_caller_brought_a_handler() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        drop(
            pool.checkout(&RpcChat {
                register_platform_tools: false,
                ..chat(&agent)
            })
            .unwrap(),
        );
        let starts = sent(tmp.path(), "thread/start");
        assert!(
            starts[0]["params"].get("dynamicTools").is_none(),
            "没有处理器就不该注册工具"
        );

        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());
        drop(
            pool.checkout(&RpcChat {
                register_platform_tools: true,
                ..chat(&agent)
            })
            .unwrap(),
        );
        let starts = sent(tmp.path(), "thread/start");
        assert_eq!(starts[0]["params"]["dynamicTools"][0]["name"], "summon");
        assert_eq!(
            starts[0]["params"]["dynamicTools"][0]["tools"][0]["name"],
            "preview"
        );
    }

    /// `thread/resume` 绝不重传 `dynamicTools`:app-server 靠 rollout 恢复工具定义,
    /// 而 resume 带实验字段会把持久化的那份顶掉。
    #[test]
    fn resume_never_replays_the_dynamic_tool_registration() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());
        let first = pool
            .checkout(&RpcChat {
                register_platform_tools: true,
                ..chat(&agent)
            })
            .unwrap();
        kill_and_await_death(&first);
        drop(first);
        drop(
            pool.checkout(&RpcChat {
                register_platform_tools: true,
                ..chat(&agent)
            })
            .unwrap(),
        );

        let resumes = sent(tmp.path(), "thread/resume");
        assert_eq!(resumes.len(), 1);
        assert!(
            resumes[0]["params"].get("dynamicTools").is_none(),
            "thread/resume 依赖 rollout 恢复动态工具，不能重传实验字段"
        );
    }

    fn chat_with_primer<'a>(agent: &'a Agent, primer: &'a str) -> RpcChat<'a> {
        RpcChat {
            primer: Some(primer),
            ..chat(agent)
        }
    }

    /// 冷启把 PRIMER 放进 `developerInstructions`(追加语义),**绝不**放 `baseInstructions`
    /// (替换语义,会顶掉 codex 自己那份 ~21KB 系统提示词);同时把槽位落进 `.session`,
    /// 否则下次 resume 无从判断该不该重发。
    #[test]
    fn a_cold_codex_thread_puts_the_primer_in_developer_instructions_and_records_the_slot() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        drop(
            pool.checkout(&chat_with_primer(&agent, "我是阿卡迪亚"))
                .unwrap(),
        );

        let starts = sent(tmp.path(), "thread/start");
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0]["params"]["developerInstructions"], "我是阿卡迪亚");
        assert!(
            !starts[0]["params"]
                .as_object()
                .unwrap()
                .contains_key("baseInstructions"),
            "baseInstructions 会替换掉 codex 的系统提示词,PRIMER 不该走那里"
        );
        assert_eq!(starts[0]["params"]["sandbox"], "danger-full-access");

        let record = resume_store::read_record(&agent, SESSION).expect("冷启必须落记录");
        assert_eq!(
            record.primer_slot.as_deref(),
            Some(summon_core::session_id::PRIMER_SLOT_DEVELOPER),
            "槽位没落盘的话,下次 resume 会把这条新 thread 当成旧 thread、不再重发 PRIMER"
        );
    }

    /// `developerInstructions` **不随 rollout 持久化**,所以懒重连必须重发 —— 漏发的症状是
    /// 「重连之后这个 agent 不认识自己了」,而且不报错。
    #[test]
    fn a_lazy_reconnect_resends_the_primer_for_a_developer_slot_thread() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let first = pool
            .checkout(&chat_with_primer(&agent, "我是阿卡迪亚"))
            .unwrap();
        kill_and_await_death(&first);
        drop(first);

        let revived = pool
            .checkout(&chat_with_primer(&agent, "我是阿卡迪亚"))
            .unwrap();
        assert_eq!(revived.checkout_mode(), Checkout::Resumed);

        let resumes = sent(tmp.path(), "thread/resume");
        assert_eq!(resumes.len(), 1);
        assert_eq!(
            resumes[0]["params"]["developerInstructions"], "我是阿卡迪亚",
            "developerInstructions 不持久化,resume 不重发这个 thread 就没有身份了"
        );
        // 槽位要一路保留下来:resume 之后再落盘一次,不能把它抹成 None。
        let record = resume_store::read_record(&agent, SESSION).expect("resume 后仍应有记录");
        assert_eq!(
            record.primer_slot.as_deref(),
            Some(summon_core::session_id::PRIMER_SLOT_DEVELOPER)
        );
    }

    /// 存量迁移的另一半:2026-08-21 之前建的 thread(记录里没有 `primer_slot`)的 PRIMER
    /// 已经以 `baseInstructions` 的形态焊进 rollout,codex 会自己从 history 读回来。
    /// 这时再发一份 `developerInstructions` 就是**两份 PRIMER**,纯浪费上下文。
    #[test]
    fn a_legacy_thread_without_a_slot_never_gets_the_primer_resent() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        // 先冷启拿到一条真记录,再把它改写成「旧形态」(无 primer_slot),模拟升级前留下的存量。
        let first = pool
            .checkout(&chat_with_primer(&agent, "我是阿卡迪亚"))
            .unwrap();
        kill_and_await_death(&first);
        drop(first);
        resume_store::write(&agent, SESSION, "codex", "th-1", None, None);
        assert_eq!(
            resume_store::read_record(&agent, SESSION).and_then(|r| r.primer_slot),
            None,
            "前置条件:这条记录现在是旧形态"
        );

        let (agent2, tmp2) = agent_with_fake_server(&codex_open_cases());
        // 把旧形态记录搬到新 agent 的盘上,确保 resume 路径读到的是「无槽位」。
        resume_store::write(&agent2, SESSION, "codex", "th-1", None, None);
        let revived = pool
            .checkout(&chat_with_primer(&agent2, "我是阿卡迪亚"))
            .unwrap();
        assert_eq!(revived.checkout_mode(), Checkout::Resumed);

        let resumes = sent(tmp2.path(), "thread/resume");
        assert_eq!(resumes.len(), 1);
        assert!(
            !resumes[0]["params"]
                .as_object()
                .unwrap()
                .contains_key("developerInstructions"),
            "旧 thread 的 PRIMER 还在 history 里,重发会变成两份"
        );
    }

    /// 模拟 `rpc::consume_cursor_turn` 在 prompt 帧成功写出后的两步提交：池内
    /// PRIMER/pending 清位 + 上层 `on_session_id` 按 ACP 通道落盘。
    fn commit_cursor_prompt(agent: &Agent, client: &PooledClient) {
        client.mark_cursor_primer_delivered();
        resume_store::write(
            agent,
            SESSION,
            &agent.resume_cli_kind(),
            client.session_id(),
            None,
            None,
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

    fn acp_config_dirs(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut path = fake_rpc_server_log(dir).into_os_string();
        path.push(".config-dirs");
        std::fs::read_to_string(PathBuf::from(path))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect()
    }

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

    /// 取 codex 侧的独占守卫。两协议共池之后 `lock()` 返回枚举,codex 用例先解出自己那支。
    fn codex_guard(client: &PooledClient) -> MutexGuard<'_, CodexAppServerClient> {
        match client.lock() {
            LockedSession::Codex(guard) => guard,
            LockedSession::Cursor(_) => panic!("codex agent 不该开出 cursor 会话"),
        }
    }

    // 冷启与接回**刻意拆成两个 case 分支**:并成一条 `*'thread/start'*|*'thread/resume'*` 的话,
    // 「resume 失败退冷启」这半条路在夹具层面根本表达不出来,降级分支于是一条测都没有。
    const CODEX_START_CASE: &str = r#"
    *'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
"#;
    const CODEX_RESUME_CASE: &str = r#"
    *'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
"#;
    /// 接回失败(rollout 被删 / thread 不存在):app-server 回 JSON-RPC error,池应退回冷启。
    const CODEX_RESUME_FAILS_CASE: &str = r#"
    *'thread/resume'*) printf '%s\n' '{"id":'"$id"',"error":{"code":-32602,"message":"thread not found"}}' ;;
"#;
    /// 假 app-server 的 `thread/fork` 成功分支:返回一条新 thread(child id 与源不同,
    /// 与真 codex 一致 —— 新 thread id 由服务端生成)。
    const CODEX_FORK_OK_CASE: &str = r#"
    *'thread/fork'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-forked","status":{"type":"idle"},"turns":[]}}}' ;;
"#;
    /// 假 app-server 的 `thread/fork` 失败分支。fork 的源 thread 被删 / rollout 不在了。
    const CODEX_FORK_FAILS_CASE: &str = r#"
    *'thread/fork'*) printf '%s\n' '{"id":'"$id"',"error":{"code":-32602,"message":"thread not found"}}' ;;
"#;
    /// 假 ACP server 的建会话分支。同样拆开:冷启与接回各占一条。
    ///
    /// 响应同时带 parameterized base-model 目录与完整 configOptions。普通 load 刻意不回
    /// sessionId：Cursor 2026.07/08 的合法真机形状就是如此，请求里的 id 才是 authoritative。
    const ACP_NEW_CASE: &str = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-1","models":{"currentModelId":"gpt-5.6-sol","availableModels":[{"modelId":"gpt-5.6-sol","name":"GPT-5.6 SOL"},{"modelId":"composer-2.5","name":"Composer"}]},"modes":{"currentModeId":"agent","availableModes":[{"id":"agent","name":"Agent"},{"id":"plan","name":"Plan"},{"id":"ask","name":"Ask"}]},"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"gpt-5.6-sol","options":[{"value":"gpt-5.6-sol","name":"GPT-5.6 SOL"},{"value":"composer-2.5","name":"Composer"}]},{"id":"context","name":"Context","type":"select","currentValue":"272k","options":[{"value":"272k","name":"272K"},{"value":"1m","name":"1M"}]},{"id":"reasoning","name":"Reasoning","type":"select","currentValue":"medium","options":[{"value":"medium","name":"Medium"},{"value":"xhigh","name":"XHigh"}]},{"id":"fast","name":"Fast","description":"2x more expensive","type":"select","currentValue":"false","options":[{"value":"false","name":"Off"},{"value":"true","name":"Fast"}]}]}}' ;;
"#;
    const ACP_LOAD_CASE: &str = r#"
    *'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"models":{"currentModelId":"gpt-5.6-sol","availableModels":[{"modelId":"gpt-5.6-sol","name":"GPT-5.6 SOL"},{"modelId":"composer-2.5","name":"Composer"}]},"modes":{"currentModeId":"agent","availableModes":[{"id":"agent","name":"Agent"},{"id":"plan","name":"Plan"},{"id":"ask","name":"Ask"}]},"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"gpt-5.6-sol","options":[{"value":"gpt-5.6-sol","name":"GPT-5.6 SOL"},{"value":"composer-2.5","name":"Composer"}]},{"id":"context","name":"Context","type":"select","currentValue":"272k","options":[{"value":"272k","name":"272K"},{"value":"1m","name":"1M"}]},{"id":"reasoning","name":"Reasoning","type":"select","currentValue":"medium","options":[{"value":"medium","name":"Medium"},{"value":"xhigh","name":"XHigh"}]},{"id":"fast","name":"Fast","description":"2x more expensive","type":"select","currentValue":"false","options":[{"value":"false","name":"Off"},{"value":"true","name":"Fast"}]}]}}' ;;
"#;
    /// 参数写入成功：维护一个进程内目录快照，并在每次回执里返回**完整** configOptions。
    const ACP_SET_CONFIG_CASE: &str = r#"
    *'session/set_config_option'*) c=$(printf '%s' "$line" | sed -n 's/.*"configId":"\([^"]*\)".*/\1/p'); v=$(printf '%s' "$line" | sed -n 's/.*"value":"\([^"]*\)".*/\1/p');
        model=${model:-gpt-5.6-sol}; context=${context:-272k}; reasoning=${reasoning:-medium}; fast=${fast:-false};
        case "$c" in model) model=$v ;; context) context=$v ;; reasoning) reasoning=$v ;; fast) fast=$v ;; esac;
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"'"$model"'","options":[{"value":"gpt-5.6-sol","name":"GPT-5.6 SOL"},{"value":"composer-2.5","name":"Composer"}]},{"id":"context","name":"Context","type":"select","currentValue":"'"$context"'","options":[{"value":"272k","name":"272K"},{"value":"1m","name":"1M"}]},{"id":"reasoning","name":"Reasoning","type":"select","currentValue":"'"$reasoning"'","options":[{"value":"medium","name":"Medium"},{"value":"xhigh","name":"XHigh"}]},{"id":"fast","name":"Fast","description":"2x more expensive","type":"select","currentValue":"'"$fast"'","options":[{"value":"false","name":"Off"},{"value":"true","name":"Fast"}]}]}}' ;;
"#;
    const ACP_SET_CONFIG_FAILS_CASE: &str = r#"
    *'session/set_config_option'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32602,"message":"Invalid config value"}}' ;;
"#;
    /// reasoning=xhigh 会静默把先前 context=1m 回退成 272k。每一次 setter 自己都 exact，只有
    /// 最终 full-vector 复核才能看出组合永远到不了 fixed point。
    const ACP_SET_CONFIG_RESETS_CONTEXT_CASE: &str = r#"
    *'session/set_config_option'*) c=$(printf '%s' "$line" | sed -n 's/.*"configId":"\([^"]*\)".*/\1/p'); v=$(printf '%s' "$line" | sed -n 's/.*"value":"\([^"]*\)".*/\1/p');
        model=${model:-gpt-5.6-sol}; context=${context:-272k}; reasoning=${reasoning:-medium}; fast=${fast:-false};
        case "$c" in
          model) model=$v ;;
          context) context=$v; if [ "$v" = 1m ]; then reasoning=medium; fi ;;
          reasoning) reasoning=$v; if [ "$v" = xhigh ]; then context=272k; fi ;;
          fast) fast=$v ;;
        esac;
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"'"$model"'","options":[{"value":"gpt-5.6-sol","name":"GPT-5.6 SOL"},{"value":"composer-2.5","name":"Composer"}]},{"id":"context","name":"Context","type":"select","currentValue":"'"$context"'","options":[{"value":"272k","name":"272K"},{"value":"1m","name":"1M"}]},{"id":"reasoning","name":"Reasoning","type":"select","currentValue":"'"$reasoning"'","options":[{"value":"medium","name":"Medium"},{"value":"xhigh","name":"XHigh"}]},{"id":"fast","name":"Fast","type":"select","currentValue":"'"$fast"'","options":[{"value":"false","name":"Off"},{"value":"true","name":"Fast"}]}]}}' ;;
"#;
    /// 第一次参数写被拒，后续恢复。用于证明失败发生在 prompt 前时，冷启 PRIMER 事实仍留在池中。
    const ACP_SET_CONFIG_REJECTS_ONCE_CASE: &str = r#"
    *'session/set_config_option'*)
        if [ "${rejected_once:-false}" = false ]; then
          rejected_once=true;
          printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32602,"message":"temporary rejection"}}';
        else
          c=$(printf '%s' "$line" | sed -n 's/.*"configId":"\([^"]*\)".*/\1/p'); v=$(printf '%s' "$line" | sed -n 's/.*"value":"\([^"]*\)".*/\1/p');
          model=${model:-gpt-5.6-sol}; context=${context:-272k}; reasoning=${reasoning:-medium}; fast=${fast:-false};
          case "$c" in model) model=$v ;; context) context=$v ;; reasoning) reasoning=$v ;; fast) fast=$v ;; esac;
          printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"'"$model"'","options":[{"value":"gpt-5.6-sol","name":"GPT-5.6 SOL"},{"value":"composer-2.5","name":"Composer"}]},{"id":"context","name":"Context","type":"select","currentValue":"'"$context"'","options":[{"value":"272k","name":"272K"},{"value":"1m","name":"1M"}]},{"id":"reasoning","name":"Reasoning","type":"select","currentValue":"'"$reasoning"'","options":[{"value":"medium","name":"Medium"},{"value":"xhigh","name":"XHigh"}]},{"id":"fast","name":"Fast","type":"select","currentValue":"'"$fast"'","options":[{"value":"false","name":"Off"},{"value":"true","name":"Fast"}]}]}}';
        fi ;;
"#;
    const ACP_SET_MODE_CASE: &str = r#"
    *'session/set_mode'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{}}' ;;
"#;
    const ACP_SET_MODE_FAILS_CASE: &str = r#"
    *'session/set_mode'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32602,"message":"Invalid mode value"}}' ;;
"#;
    /// 接回失败(聊天记录被删 / id 不存在):先按真机行为重放一段历史,再回 JSON-RPC error。
    /// 重放那一行是关键 —— 它就是「失败路径漏 drain」那条缺陷的载体。
    const ACP_LOAD_FAILS_CASE: &str = r#"
    *'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-gone","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"上一轮的回复"}}}}';
        printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32602,"message":"session not found"}}' ;;
"#;
    const ACP_LOAD_WRONG_SESSION_CASE: &str = r#"
    *'session/load'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-other"}}' ;;
"#;

    /// 冷启 + 接回都成功的常规夹具(绝大多数用例要的)。
    fn codex_open_cases() -> String {
        format!("{CODEX_START_CASE}{CODEX_RESUME_CASE}")
    }

    fn acp_open_cases() -> String {
        format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}")
    }

    /// SIGKILL 掉假 server 的进程组,并等到「进程真的没了」这件事被观察到。
    ///
    /// 判据用 `is_running()` 而不是 `group_alive()`:此刻 client 还被池持有着,没有人 `wait` 过这个
    /// child,它会以**僵尸**形态留在进程表里 —— `killpg(pgid, 0)` 对僵尸照样返回成功。真正的证据是
    /// stdout EOF。这正是 [`Session::is_running`] 要两个证据都看的原因。
    fn kill_and_await_death(client: &PooledClient) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(client.pgid()),
            nix::sys::signal::Signal::SIGKILL,
        );
        assert!(
            wait_until(Duration::from_secs(3), || !client.is_running()),
            "假 server 被 SIGKILL 后应观察到进程已死"
        );
    }

    /// 冷启:无 sid → `thread/start`,并把 thread id 落进 resume 记录(下次 daemon 起来才有得 resume)。
    #[test]
    fn cold_checkout_starts_a_thread_and_records_the_sid() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let client = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(client.checkout_mode(), Checkout::ColdStart);
        assert_eq!(client.session_id(), "th-1");
        assert_eq!(
            resume_store::read_id(&agent, SESSION, "codex").as_deref(),
            Some("th-1"),
            "sid 必须落盘,否则重启后只能冷启、上下文全丢"
        );
        assert_eq!(sent(tmp.path(), "thread/start").len(), 1);
        assert!(sent(tmp.path(), "thread/resume").is_empty());
    }

    /// RPC slash 必须调用 app-server 控制方法并直接产出回执，不能偷偷开 `turn/start` 让模型
    /// “回答命令”。同一份 live session 是唯一目标；没有 live owner 时另测 fail-closed。
    #[test]
    fn rpc_controls_use_native_methods_without_starting_a_model_turn() {
        use summon_core::driver::slash_pool::{RpcGoalCommand, RpcSlashCommand};

        let _guard = process_test_guard();
        let cases = format!(
            r#"{CODEX_START_CASE}
    *'thread/read'*) printf '%s\n' '{{"id":'"$id"',"result":{{"thread":{{"id":"th-1","name":"控制面测试","cwd":"/repo","status":{{"type":"idle"}},"turns":[]}}}}}}' ;;
    *'thread/compact/start'*) printf '%s\n' '{{"id":'"$id"',"result":{{}}}}';
        printf '%s\n' '{{"method":"thread/compacted","params":{{"threadId":"th-1"}}}}';
        printf '%s\n' '{{"method":"thread/tokenUsage/updated","params":{{"threadId":"th-1","tokenUsage":{{"last":{{"inputTokens":1200,"cachedInputTokens":0,"outputTokens":0,"totalTokens":1200}},"modelContextWindow":272000}}}}}}' ;;
    *'thread/name/set'*) printf '%s\n' '{{"id":'"$id"',"result":{{}}}}' ;;
    *'thread/goal/get'*) printf '%s\n' '{{"id":'"$id"',"result":{{"goal":{{"threadId":"th-1","objective":"补齐 RPC","status":"active","createdAt":1,"updatedAt":2,"timeUsedSeconds":3,"tokensUsed":4}}}}}}' ;;
    *'thread/goal/set'*|*'thread/goal/clear'*) printf '%s\n' '{{"id":'"$id"',"result":{{}}}}' ;;
    *'thread/contextUsage'*) printf '%s\n' '{{"id":'"$id"',"result":{{"contextWindowUsage":{{"calibrated":true,"source":"tokenUsage","categories":[{{"name":"conversation","tokens":1200}}]}}}}}}' ;;
    *'mcpServerStatus/list'*) printf '%s\n' '{{"id":'"$id"',"result":{{"data":[{{"name":"docs","authStatus":"notRequired","tools":{{"search":{{}}}},"resources":[],"resourceTemplates":[]}}]}}}}' ;;
    *'skills/list'*) printf '%s\n' '{{"id":'"$id"',"result":{{"data":[{{"cwd":"/repo","skills":[{{"name":"diagnose"}}],"errors":[{{"message":"hidden"}}]}}]}}}}' ;;
    *'hooks/list'*) printf '%s\n' '{{"id":'"$id"',"result":{{"data":[{{"cwd":"/repo","hooks":[{{"name":"guard"}}],"errors":[{{"message":"hidden"}}],"warnings":["hidden"]}}]}}}}' ;;
    *'plugin/list'*) printf '%s\n' '{{"id":'"$id"',"result":{{"marketplaces":[{{"name":"local","plugins":[]}}],"marketplaceLoadErrors":[{{"message":"hidden"}}]}}}}' ;;
    *'experimentalFeature/list'*) printf '%s\n' '{{"id":'"$id"',"result":{{"data":[{{"name":"fast_mode","enabled":true,"defaultEnabled":false,"stage":"beta"}}]}}}}' ;;
    *'thread/backgroundTerminals/list'*) printf '%s\n' '{{"id":'"$id"',"result":{{"processIds":["p1","p2"]}}}}' ;;
    *'thread/backgroundTerminals/stop'*) printf '%s\n' '{{"id":'"$id"',"result":{{}}}}' ;;
"#
        );
        let (agent, tmp) = traex_agent_with_fake_server(&cases);
        let pool = RpcPool::new(InFlight::new());
        drop(pool.checkout(&chat(&agent)).unwrap());

        let status = pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Status)
            .unwrap();
        assert!(status.contains("控制面测试"));
        assert!(
            !status.contains("th-1") && !status.contains("/repo"),
            "群回执不得泄露 provider thread id 或本机绝对路径：{status}"
        );
        assert!(pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Context)
            .unwrap()
            .contains("1,200"));
        assert!(pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Rename("新名称".into()))
            .unwrap()
            .contains("新名称"));
        assert!(pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Goal(RpcGoalCommand::Get))
            .unwrap()
            .contains("补齐 RPC"));
        assert!(pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Mcp { verbose: false })
            .unwrap()
            .contains("docs"));
        let skills = pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Skills)
            .unwrap();
        assert!(skills.contains("diagnose") && skills.contains("加载错误：1"));
        assert!(!skills.contains("hidden"), "加载错误只报数量，不回显详情");
        let hooks = pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Hooks)
            .unwrap();
        assert!(hooks.contains("加载错误：1") && hooks.contains("警告：1"));
        let plugins = pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Plugins)
            .unwrap();
        assert!(plugins.contains("加载错误：1"));
        let processes = pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Processes)
            .unwrap();
        assert!(processes.contains('2'));
        assert!(!processes.contains("p1"), "后台 process id 不应进入群回执");
        assert!(pool
            .control_existing(&agent, TAG, &RpcSlashCommand::StopBackground)
            .unwrap()
            .contains("2"));
        // `/compact` 等到 `thread/compacted` 才回执,并把随之而来的用量记进会话 ——
        // 不等的话回执与卡上的 ctx 都还是压缩前的数字(owner 2026-09-09 报告)。
        let compacted = pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Compact)
            .unwrap();
        assert!(compacted.contains("已压缩当前会话"), "{compacted}");
        assert!(
            compacted.contains("1,200"),
            "回执要带压缩后的占用: {compacted}"
        );
        assert_eq!(
            pool.current_ctx_budget(&agent.name, TAG),
            Some((1_200, 272_000)),
            "压缩后的占用必须记进跨轮存活的会话,否则空闲卡现采到的还是旧值"
        );

        assert!(sent(tmp.path(), "turn/start").is_empty());
        let initialize = sent(tmp.path(), "initialize");
        assert_eq!(
            initialize[0]["params"]["capabilities"]["experimentalApi"], true,
            "chat pool 必须显式协商 experimental API，不能只靠 schema 里看得到方法"
        );
        assert_eq!(sent(tmp.path(), "thread/backgroundTerminals/stop").len(), 2);
        let rename = sent(tmp.path(), "thread/name/set");
        assert_eq!(rename[0]["params"]["threadId"], "th-1");
        assert_eq!(rename[0]["params"]["name"], "新名称");
    }

    #[test]
    fn codex_background_controls_follow_the_paginated_data_schema() {
        use summon_core::driver::slash_pool::RpcSlashCommand;

        let _guard = process_test_guard();
        let cases = format!(
            r#"{CODEX_START_CASE}
    *'thread/backgroundTerminals/list'*)
        case "$line" in
          *'"cursor":"next-1"'*) printf '%s\n' '{{"id":'"$id"',"result":{{"data":[{{"processId":"p2","command":"b","cwd":"/repo","itemId":"i2"}}],"nextCursor":null}}}}' ;;
          *) printf '%s\n' '{{"id":'"$id"',"result":{{"data":[{{"processId":"p1","command":"a","cwd":"/repo","itemId":"i1"}}],"nextCursor":"next-1"}}}}' ;;
        esac ;;
    *'thread/backgroundTerminals/terminate'*) printf '%s\n' '{{"id":'"$id"',"result":{{}}}}' ;;
    *'experimentalFeature/list'*)
        case "$line" in
          *'"cursor":"feature-next"'*) printf '%s\n' '{{"id":'"$id"',"result":{{"data":[],"nextCursor":null}}}}' ;;
          *) printf '%s\n' '{{"id":'"$id"',"result":{{"data":[{{"name":"f1","enabled":true}}],"nextCursor":"feature-next"}}}}' ;;
        esac ;;
"#
        );
        let (agent, tmp) = agent_with_fake_server(&cases);
        let pool = RpcPool::new(InFlight::new());
        drop(pool.checkout(&chat(&agent)).unwrap());

        let processes = pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Processes)
            .unwrap();
        assert!(processes.contains('2'));
        assert!(pool
            .control_existing(&agent, TAG, &RpcSlashCommand::StopBackground)
            .unwrap()
            .contains('2'));
        pool.control_existing(&agent, TAG, &RpcSlashCommand::Experimental)
            .unwrap();

        assert_eq!(sent(tmp.path(), "thread/backgroundTerminals/list").len(), 4);
        assert_eq!(
            sent(tmp.path(), "thread/backgroundTerminals/terminate").len(),
            2
        );
        let experimental = sent(tmp.path(), "experimentalFeature/list");
        assert_eq!(
            experimental.len(),
            2,
            "必须读完 nextCursor，不能把首页伪装成全量"
        );
        assert!(
            experimental
                .iter()
                .all(|frame| frame["params"]["threadId"] == "th-1"),
            "Codex 每一页都要带当前 threadId 解析 project-local config"
        );
        assert_eq!(
            sent(tmp.path(), "initialize")[0]["params"]["capabilities"]["experimentalApi"],
            true,
            "Codex chat 也必须显式协商 experimental API"
        );
        assert!(sent(tmp.path(), "turn/start").is_empty());
    }

    #[test]
    fn rpc_control_without_a_live_session_never_cold_starts_one() {
        use summon_core::driver::slash_pool::RpcSlashCommand;

        let _guard = process_test_guard();
        let (agent, tmp) = traex_agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let error = pool
            .control_existing(&agent, TAG, &RpcSlashCommand::Compact)
            .unwrap_err()
            .to_string();
        assert!(error.contains("没有已加载的 RPC 会话"), "{error}");
        assert!(sent(tmp.path(), "thread/start").is_empty());
        assert!(sent(tmp.path(), "turn/start").is_empty());
    }

    #[test]
    fn rpc_control_labels_strip_all_control_characters_and_bound_length() {
        let rendered = safe_rpc_label("a\tb\n`c`\u{0000}<tag>", 100);
        assert!(!rendered.chars().any(char::is_control));
        assert!(!rendered.contains('`'));
        assert!(
            rendered.contains("<\u{200b}tag>"),
            "Lark 标签形态必须被拆散"
        );
        assert!(safe_rpc_label(&"x".repeat(101), 100).ends_with('…'));
        let error = anyhow::Error::new(
            crate::codex::codex_app_server::CodexAppServerError::ProcessFailed {
                operation: "thread/read".into(),
                detail: "failed".into(),
                stderr_tail: "secret=/tmp/private-token".into(),
            },
        );
        let public = rpc_control_public_error(&error);
        assert!(!public.contains("secret") && !public.contains("/tmp"));
    }

    #[test]
    fn rpc_control_reservation_prevents_a_concurrent_turn_checkout() {
        use summon_core::driver::slash_pool::RpcSlashCommand;

        let _guard = process_test_guard();
        // `thread/read` 的应答由测试放行:控制请求在 checkout 被拒之前一直挂着,不靠 server 端
        // 睡一段固定时长去赌两边的先后。
        let cases = format!(
            r#"{CODEX_START_CASE}
    *'thread/read'*) while [ ! -e "$DIR/release-thread-read" ]; do sleep 0.01; done; printf '%s\n' '{{"id":'"$id"',"result":{{"thread":{{"id":"th-1","status":{{"type":"idle"}},"turns":[]}}}}}}' ;;
"#
        );
        let (agent, tmp) = traex_agent_with_fake_server(&cases);
        let pool = RpcPool::new(InFlight::new());
        drop(pool.checkout(&chat(&agent)).unwrap());

        let control_pool = pool.clone();
        let control_agent = agent.clone();
        let control = std::thread::spawn(move || {
            control_pool.control_existing(&control_agent, TAG, &RpcSlashCommand::Status)
        });
        assert!(wait_until(Duration::from_secs(2), || {
            !sent(tmp.path(), "thread/read").is_empty()
        }));
        let refused = pool.checkout(&chat(&agent)).is_err();
        std::fs::write(tmp.path().join("release-thread-read"), b"").unwrap();
        assert!(
            refused,
            "控制请求持有 per-key reservation 时，另一轮 checkout 必须在发 turn 前失败"
        );
        control.join().unwrap().unwrap();
        assert!(sent(tmp.path(), "turn/start").is_empty());
    }

    #[test]
    fn corrupt_resume_record_never_gets_overwritten_by_a_cold_start() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());
        let path = resume_store::record_path(&agent, SESSION);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "cli=codex\nstrict_resume=true\n").unwrap();

        assert!(pool.checkout(&chat(&agent)).is_err());

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "cli=codex\nstrict_resume=true\n"
        );
        assert!(sent(tmp.path(), "thread/start").is_empty());
        assert!(sent(tmp.path(), "thread/resume").is_empty());
    }

    /// 开会话时主动问一次额度并记进池 —— **不等 `account/rateLimits/updated` 推送**:
    /// 那条通知只在额度变化时来,光靠它的话新会话头几轮角标上没有额度项。
    ///
    /// 夹具用 2026-08-08 真机探针的形状:周窗在 **primary**(`windowDurationMins: 10080`)、
    /// `secondary` 是 null。口径也在这里钉死:上游给「已用 95%」,池里存的是「还剩 5%」。
    #[test]
    fn 开会话时读到的周额度进池() {
        let _guard = process_test_guard();
        let _quota_guard = default_codex_quota_test_guard();
        let quota_case = r#"
    *'account/rateLimits/read'*) printf '%s\n' '{"id":'"$id"',"result":{"rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":95,"windowDurationMins":10080,"resetsAt":200},"secondary":null,"rateLimitReachedType":null},"rateLimitsByLimitId":{"codex":{"limitId":"codex","limitName":null,"primary":{"usedPercent":95,"windowDurationMins":10080,"resetsAt":200},"secondary":null,"rateLimitReachedType":null}}}}' ;;
"#;
        let (agent, _tmp) = agent_with_fake_server(&format!("{}{quota_case}", codex_open_cases()));
        let pool = RpcPool::new(InFlight::new());

        let _client = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(
            pool.current_quota(&agent.name, TAG),
            Some(5),
            "开会话时就该拿到周额度剩余(95% 已用 → 5% 剩余)"
        );
        assert_eq!(
            current_default_codex_quota_shared(&agent.name, "g-other"),
            Some(5),
            "账号读取结果应供同 agent 其它尚未建 RPC session 的 Codex 卡片使用"
        );
    }

    fn two_window_snapshot(
        five_hour_used: f64,
        five_hour_reset: i64,
        weekly_used: f64,
    ) -> CodexRateLimitSnapshot {
        CodexRateLimitSnapshot {
            limit_id: "codex".into(),
            limit_name: None,
            primary: Some(summon_core::codex_quota::CodexRateLimitWindow {
                used_percent: five_hour_used,
                window_minutes: Some(300),
                resets_at: Some(five_hour_reset),
            }),
            secondary: Some(summon_core::codex_quota::CodexRateLimitWindow {
                used_percent: weekly_used,
                window_minutes: Some(10_080),
                resets_at: Some(five_hour_reset + 7 * 86_400),
            }),
            rate_limit_reached_type: None,
        }
    }

    #[test]
    fn codex_snapshot_keeps_the_five_hour_window_beside_the_weekly_one() {
        let agent = "quota-five-hour-beside-weekly";
        let now = unix_now_secs();
        record_codex_quota_snapshot_shared(
            Some((agent, "g-five")),
            &two_window_snapshot(35.0, now + 3_600, 12.0),
        );
        assert_eq!(
            current_five_hour_quota_shared(agent),
            Some((65, Some(now + 3_600))),
            "5 小时窗口连同重置时刻一起记下"
        );
        assert_eq!(current_quota_shared(agent, "g-five"), Some(88));

        // 5 小时窗口已经重置过的观测描述的是一个不存在的窗口,不显示;周额度不受影响。
        let stale = "quota-five-hour-stale";
        record_codex_quota_snapshot_shared(
            Some((stale, "g-five")),
            &two_window_snapshot(90.0, now - 60, 12.0),
        );
        assert_eq!(current_five_hour_quota_shared(stale), None);
        assert_eq!(current_quota_shared(stale, "g-five"), Some(88));
    }

    #[test]
    fn default_codex_account_five_hour_quota_merges_with_the_agent_key() {
        let _guard = default_codex_quota_test_guard();
        let agent = "quota-five-hour-default";
        let now = unix_now_secs();
        record_codex_quota_snapshot_at(
            None,
            &two_window_snapshot(20.0, now + 3_600, 10.0),
            Some(now - 10),
        );
        assert_eq!(
            current_default_codex_five_hour_quota_shared(agent).map(|(left_pct, _)| left_pct),
            Some(80)
        );
        assert_eq!(current_five_hour_quota_shared(agent), None);

        record_codex_quota_snapshot_shared(
            Some((agent, "g-default")),
            &two_window_snapshot(45.0, now + 3_600, 10.0),
        );
        assert_eq!(
            current_default_codex_five_hour_quota_shared(agent).map(|(left_pct, _)| left_pct),
            Some(55)
        );
    }

    #[test]
    fn latest_quota_is_kept_without_a_session_and_shared_across_chats() {
        let agent = "quota-memory-across-chats";
        assert!(record_quota_observation_shared(
            agent, "g-source", 61, None, None
        ));
        assert_eq!(current_quota_shared(agent, "g-source"), Some(61));
        assert_eq!(
            current_quota_shared(agent, "p2p-other"),
            Some(61),
            "账号额度不应绑死在产生观测的 chat session 上"
        );
        assert!(!record_quota_observation_shared(
            agent, "g-source", 61, None, None
        ));
        assert!(record_quota_observation_shared(
            agent,
            "p2p-other",
            59,
            None,
            None
        ));
        assert_eq!(current_quota_shared(agent, "g-third"), Some(59));
    }

    #[test]
    fn latest_quota_rejects_late_observations_from_older_windows() {
        let agent = "quota-memory-version-order";
        assert!(record_quota_observation_shared(
            agent,
            "g-new",
            50,
            Some(200),
            Some(1_000)
        ));
        // 旧窗口:观测时刻再新也压不过新窗口。
        assert!(!record_quota_observation_shared(
            agent,
            "g-old-window",
            20,
            Some(100),
            Some(9_000)
        ));
        // 同窗口、更早的观测:另一条 chat 迟到的旧快照,不能把账号值倒回去。
        assert!(!record_quota_observation_shared(
            agent,
            "g-old-snapshot",
            60,
            Some(200),
            Some(900)
        ));
        assert!(!record_quota_observation_shared(
            agent,
            "g-unversioned",
            10,
            None,
            Some(9_000)
        ));
        assert_eq!(current_quota_shared(agent, "g-read"), Some(50));

        assert!(record_quota_observation_shared(
            agent,
            "g-fresh",
            45,
            Some(200),
            Some(1_100)
        ));
        assert!(record_quota_observation_shared(
            agent,
            "g-reset",
            99,
            Some(300),
            Some(1_200)
        ));
        assert_eq!(current_quota_shared(agent, "g-read"), Some(99));
    }

    /// 同窗口内以观测时刻定新旧:额度被供应商刷新、剩余量回升的新快照必须赢。
    #[test]
    fn same_window_later_observation_wins_even_when_quota_was_refilled() {
        let agent = "quota-memory-refill";
        assert!(record_quota_observation_shared(
            agent,
            "g-night",
            31,
            Some(200),
            Some(1_000)
        ));
        assert!(record_quota_observation_shared(
            agent,
            "g-morning",
            99,
            Some(200),
            Some(2_000)
        ));
        assert_eq!(current_quota_shared(agent, "g-read"), Some(99));
        // 刷新之后另一条 chat 读到的仍是自己那份凌晨快照:时刻更早,照旧拒绝。
        assert!(!record_quota_observation_shared(
            agent,
            "g-stale-chat",
            31,
            Some(200),
            Some(1_000)
        ));
        assert_eq!(current_quota_shared(agent, "g-read"), Some(99));
    }

    /// 没带观测时刻的快照视为最旧:能填空账号,压不过任何带时刻的观测。
    #[test]
    fn undated_observation_only_fills_an_empty_account() {
        let agent = "quota-memory-undated";
        assert!(record_quota_observation_shared(
            agent,
            "g-legacy",
            40,
            Some(200),
            None
        ));
        assert!(record_quota_observation_shared(
            agent,
            "g-dated",
            70,
            Some(200),
            Some(1_000)
        ));
        assert!(!record_quota_observation_shared(
            agent,
            "g-legacy-again",
            40,
            Some(200),
            None
        ));
        assert_eq!(current_quota_shared(agent, "g-read"), Some(70));
    }

    /// Codex 同一周窗口的 resets_at 在相邻观测间有秒级抖动,差几秒的两个值不能当成两个窗口。
    #[test]
    fn reset_jitter_within_tolerance_is_the_same_window() {
        let agent = "quota-memory-reset-jitter";
        assert!(record_quota_observation_shared(
            agent,
            "g-a",
            60,
            Some(1_000_002),
            Some(1_000)
        ));
        // 同窗口(差 2 秒)、更早的观测:拒。
        assert!(!record_quota_observation_shared(
            agent,
            "g-b",
            80,
            Some(1_000_000),
            Some(900)
        ));
        // 同窗口(差 2 秒)、更晚的观测:收,哪怕 resets_at 比缓存里的小。
        assert!(record_quota_observation_shared(
            agent,
            "g-c",
            55,
            Some(1_000_000),
            Some(1_100)
        ));
        assert_eq!(current_quota_shared(agent, "g-read"), Some(55));
        // 真正的下一个窗口:无条件压过。
        assert!(record_quota_observation_shared(
            agent,
            "g-d",
            99,
            Some(1_604_800),
            Some(1_050)
        ));
        assert_eq!(current_quota_shared(agent, "g-read"), Some(99));
    }

    #[test]
    fn reliable_reset_watermark_survives_unversioned_and_older_observations() {
        let agent = "quota-memory-reset-watermark";
        assert!(record_quota_observation_shared(
            agent,
            "g-current",
            99,
            Some(300),
            Some(1_000)
        ));
        assert!(!record_quota_observation_shared(
            agent,
            "g-unversioned",
            10,
            None,
            Some(2_000)
        ));
        assert!(!record_quota_observation_shared(
            agent,
            "g-older-cycle",
            20,
            Some(200),
            Some(2_000)
        ));
        assert_eq!(current_quota_shared(agent, "g-read"), Some(99));
    }

    #[test]
    fn codex_account_merge_keeps_the_newest_reliable_reset_watermark() {
        let _quota_guard = default_codex_quota_test_guard();
        let agent = "quota-memory-codex-account-merge";
        record_latest_quota(
            DEFAULT_CODEX_ACCOUNT,
            40,
            Some(1_000_000),
            Some(1_000),
            QuotaOrigin::Live,
        );
        record_quota_observation_shared(agent, "g-agent", 90, Some(1_604_800), Some(1_000));
        assert_eq!(
            current_default_codex_quota_shared(agent, "g-read"),
            Some(90),
            "agent 的更新周期应胜过默认 Codex 后台账号的旧周期"
        );

        assert!(!record_quota_observation_shared(
            agent,
            "g-unversioned",
            5,
            None,
            Some(2_000)
        ));
        assert_eq!(
            current_default_codex_quota_shared(agent, "g-read"),
            Some(90)
        );
    }

    #[test]
    fn live_codex_quota_wins_over_a_later_historical_reset() {
        let _guard = default_codex_quota_test_guard();
        let agent = "quota-live-vs-history";
        assert!(
            record_latest_quota(
                DEFAULT_CODEX_ACCOUNT,
                100,
                Some(1_794_272_826),
                Some(1_791_679_628),
                QuotaOrigin::Rollout
            )
            .1
        );
        assert!(record_quota_observation_shared(
            agent,
            "g-live",
            83,
            Some(1_792_472_315),
            Some(1_791_878_000)
        ));
        assert_eq!(
            current_default_codex_quota_shared(agent, "g-read"),
            Some(83)
        );
        assert!(
            !record_latest_quota(
                agent,
                100,
                Some(1_794_272_826),
                Some(1_791_679_628),
                QuotaOrigin::Rollout
            )
            .0
        );
        assert_eq!(current_quota_shared(agent, "g-read"), Some(83));
    }

    #[test]
    fn newer_default_rollout_updates_an_earlier_live_quota() {
        let _guard = default_codex_quota_test_guard();
        let agent = "quota-new-rollout-after-live";
        record_quota_observation_shared(agent, "g-live", 83, Some(2_000_000), Some(1_000));
        let snapshot = CodexRateLimitSnapshot {
            limit_id: "codex".into(),
            limit_name: None,
            primary: Some(summon_core::codex_quota::CodexRateLimitWindow {
                used_percent: 20.0,
                window_minutes: Some(10080),
                resets_at: Some(2_000_000),
            }),
            secondary: None,
            rate_limit_reached_type: None,
        };
        record_codex_quota_snapshot_at(None, &snapshot, Some(2_000));
        assert_eq!(
            current_default_codex_quota_shared(agent, "g-read"),
            Some(80)
        );
        record_quota_observation_shared(agent, "g-live", 79, Some(2_000_000), Some(2_000));
        assert_eq!(
            current_default_codex_quota_shared(agent, "g-read"),
            Some(79)
        );
    }

    #[test]
    fn rollout_replay_uses_time_across_plan_reset_changes() {
        let _guard = default_codex_quota_test_guard();
        assert!(
            record_latest_quota(
                DEFAULT_CODEX_ACCOUNT,
                100,
                Some(1_794_272_826),
                Some(1_791_679_628),
                QuotaOrigin::Rollout
            )
            .1
        );
        assert!(
            record_latest_quota(
                DEFAULT_CODEX_ACCOUNT,
                83,
                Some(1_792_472_315),
                Some(1_791_878_000),
                QuotaOrigin::Rollout
            )
            .1
        );
        assert!(
            !record_latest_quota(
                DEFAULT_CODEX_ACCOUNT,
                100,
                Some(1_794_272_826),
                Some(1_791_679_628),
                QuotaOrigin::Rollout
            )
            .0
        );
        assert_eq!(
            current_default_codex_quota_shared("quota-replay-reader", "g-read"),
            Some(83)
        );
    }

    /// 换了账号的新周期可能比旧账号的更早结束;主动拉取的结果照样要压过去。
    #[test]
    fn pulled_quota_replaces_a_later_cycle_and_can_be_cleared() {
        let agent = "pulled-quota-account-switch";
        replace_account_quota_shared(agent, Some((20, Some(1_790_812_800))), 1_000);
        replace_account_quota_shared(agent, Some((80, Some(1_790_294_400))), 2_000);
        assert_eq!(current_quota_shared(agent, "g-any"), Some(80));

        replace_account_quota_shared(agent, Some((70, None)), 3_000);
        assert_eq!(
            current_quota_shared(agent, "g-any"),
            Some(70),
            "缺周期的结果同样生效"
        );

        replace_account_quota_shared(agent, None, 4_000);
        assert_eq!(
            current_quota_shared(agent, "g-any"),
            None,
            "失效后不再显示旧值"
        );
    }

    #[test]
    fn custom_codex_runtime_is_not_treated_as_the_default_account() {
        let with_env: Agent = serde_yaml::from_str(
            "name: isolated-env\nruntime:\n  cli: codex\n  env:\n    CODEX_HOME: /tmp/other\n",
        )
        .unwrap();
        let with_wrapper: Agent = serde_yaml::from_str(
            "name: isolated-wrapper\nruntime:\n  cli: codex\n  cli_cmd: /opt/custom-codex\n",
        )
        .unwrap();

        assert!(!agent_uses_default_codex_account(&with_env));
        assert!(!agent_uses_default_codex_account(&with_wrapper));
    }

    /// codex 不支持 `account/rateLimits/read`(老版本 → `-32601`)时,会话照常建起来,
    /// 只是角标上没有额度这一项。**额度是装饰,绝不能让它挡住一条能用的会话**。
    #[test]
    fn 额度读取失败不挡会话建立() {
        let _guard = process_test_guard();
        // 夹具默认就答 -32601(见 write_app_server_with_trap 的兜底 case)。
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let client = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(client.session_id(), "th-1", "会话必须照常建起来");
        assert_eq!(
            pool.current_quota(&agent.name, TAG),
            None,
            "读不到就整项省略,不显示假值"
        );
    }

    /// 热路径:进程活着就复用同一个 client,不再握手、不再 start。
    #[test]
    fn second_checkout_reuses_the_live_process() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let first = pool.checkout(&chat(&agent)).unwrap();
        let second = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(second.checkout_mode(), Checkout::Reused);
        assert_eq!(first.pgid(), second.pgid(), "复用应是同一个进程");
        assert_eq!(
            sent(tmp.path(), "thread/start").len(),
            1,
            "复用不该再建一个 thread"
        );
    }

    /// 懒重连(§7 的核心不变量):记 sid → 杀进程 → 下一条消息来时 `thread/resume` 接回去。
    /// 同时钉死 §5.1 红线:resume 的 params **键集合必须 ⊆ `RESUME_ALLOWED_KEYS`**。
    #[test]
    fn lazy_reconnect_resumes_the_recorded_thread_with_only_allowed_keys() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let first = pool.checkout(&chat(&agent)).unwrap();
        let dead_pgid = first.pgid();
        // 模拟 app-server 自己死掉(不是 daemon 停机):进程组整组收掉,池里那条随之失效。
        kill_and_await_death(&first);
        drop(first);

        let revived = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(revived.checkout_mode(), Checkout::Resumed);
        assert_eq!(revived.session_id(), "th-1");
        assert_ne!(revived.pgid(), dead_pgid, "应是一个全新的进程");

        let resumes = sent(tmp.path(), "thread/resume");
        assert_eq!(resumes.len(), 1, "懒重连必须真的发出 thread/resume");
        assert_eq!(resumes[0]["params"]["threadId"], "th-1");
        // 白名单而非黑名单:黑名单只挡得住今天想得到的键名,schema 新增一个模型相关字段就会
        // 从缝里漏过去,而漏过去的症状恰恰是「静默换模型」。见 `RESUME_ALLOWED_KEYS` 注释。
        let params = resumes[0]["params"].as_object().unwrap();
        for key in params.keys() {
            assert!(
                RESUME_ALLOWED_KEYS.contains(&key.as_str()),
                "thread/resume 带了白名单外的键 `{key}` —— app-server 可能静默丢掉持久化的 model/effort"
            );
        }
        assert_eq!(
            sent(tmp.path(), "thread/start").len(),
            1,
            "有 sid 就不该再冷启一次"
        );
    }

    #[test]
    fn opening_a_codex_session_caches_the_backend_model_catalog() {
        let _guard = process_test_guard();
        let catalog_case = r#"
    *'model/list'*) printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"probe-model-a","hidden":false,"isDefault":true},{"id":"probe-model-hidden","hidden":true,"isDefault":false}],"nextCursor":null}}' ;;
"#;
        let (agent, tmp) = agent_with_fake_server(&format!("{catalog_case}{CODEX_START_CASE}"));
        let pool = RpcPool::new(InFlight::new());
        pool.checkout(&chat(&agent)).unwrap();

        assert_eq!(
            sent(tmp.path(), "model/list").len(),
            1,
            "开会话要顺带问一次目录,否则面板永远只有配置桶那几个"
        );
        let cached = crate::codex::model_catalog::cached_for_agent(&agent)
            .expect("目录抓到了就该进缓存,面板读的是这一份");
        assert!(
            cached.models.contains(&"probe-model-a".to_string()),
            "候选要来自 backend 自报的目录"
        );
        assert!(
            !cached.models.contains(&"probe-model-hidden".to_string()),
            "hidden 的模型 codex 自己都不列,不能出现在面板上"
        );
        assert_eq!(cached.default_model.as_deref(), Some("probe-model-a"));
    }

    #[test]
    fn a_codex_session_still_opens_when_the_model_catalog_is_unavailable() {
        let _guard = process_test_guard();
        // 夹具兜底把 model/list 答成 -32601,正好是「codex 版本不支持这个方法」的真实形态。
        let (agent, _tmp) = agent_with_fake_server(CODEX_START_CASE);
        let pool = RpcPool::new(InFlight::new());
        let client = pool
            .checkout(&chat(&agent))
            .expect("目录问不到只该降级,不该让会话开不起来");
        assert_eq!(client.session_id(), "th-1");
    }

    /// **降级分支(此前 0 测试)**:盘上有 sid、但 `thread/resume` 失败(rollout 被删 / thread 不存在)
    /// → 必须退回冷启,并且 ① 模式如实报 `ColdStart`;② PRIMER 走 `thread/start` 的原生位置
    /// **无条件下发**(新会话什么都不知道,不补注就是一个没有人格的 agent);③ 记录被新 sid 覆盖。
    ///
    /// 此前冷启/接回并成同一条夹具分支,这条路在测试里根本走不到。
    #[test]
    fn codex_resume_failure_falls_back_to_a_cold_start_that_still_carries_the_primer() {
        let _guard = process_test_guard();
        let (agent, tmp) =
            agent_with_fake_server(&format!("{CODEX_START_CASE}{CODEX_RESUME_FAILS_CASE}"));
        let pool = RpcPool::new(InFlight::new());
        // 盘上留一个已经失效的 sid:这正是「rollout 被删」在 summon 侧的形态。
        resume_store::write(&agent, SESSION, "codex", "th-gone", None, None);

        let client = pool
            .checkout(&RpcChat {
                primer: Some("你是测试体"),
                ..chat(&agent)
            })
            .unwrap();

        assert_eq!(
            client.checkout_mode(),
            Checkout::ColdStart,
            "resume 失败必须如实报冷启,报成 Resumed 会让上层以为上下文还在"
        );
        assert_eq!(client.session_id(), "th-1");
        assert_eq!(
            sent(tmp.path(), "thread/resume").len(),
            1,
            "该试一次 resume"
        );
        let starts = sent(tmp.path(), "thread/start");
        assert_eq!(starts.len(), 1, "失败后必须真的退回冷启");
        assert_eq!(
            starts[0]["params"]["developerInstructions"], "你是测试体",
            "退冷启的新 thread 拿不到 PRIMER 就是个没有人格的 agent"
        );
        assert_eq!(
            resume_store::read_id(&agent, SESSION, "codex").as_deref(),
            Some("th-1"),
            "失效的旧 sid 必须被新会话覆盖,否则每一轮都重试一次注定失败的 resume"
        );
        // 退回冷启 = 一条**全新** thread,PRIMER 走的是 developerInstructions,槽位必须跟着翻新。
        // 这里若沿用旧记录的空槽位,下次 resume 就不重发 PRIMER,这条新 thread 从此失忆。
        assert_eq!(
            resume_store::read_record(&agent, SESSION).and_then(|r| r.primer_slot),
            Some(summon_core::session_id::PRIMER_SLOT_DEVELOPER.to_string()),
            "resume 失败退冷启后,槽位要按新 thread 记"
        );
    }

    #[test]
    fn strict_codex_resume_failure_never_cold_starts_or_overwrites_the_sid() {
        let _guard = process_test_guard();
        let (agent, tmp) =
            agent_with_fake_server(&format!("{CODEX_START_CASE}{CODEX_RESUME_FAILS_CASE}"));
        let pool = RpcPool::new(InFlight::new());
        resume_store::write(&agent, SESSION, "codex", "th-gone", None, None);
        resume_store::mark_strict_checked(&agent, SESSION, "codex", "th-gone").unwrap();

        assert!(pool.checkout(&chat(&agent)).is_err());
        assert_eq!(sent(tmp.path(), "thread/resume").len(), 1);
        assert!(
            sent(tmp.path(), "thread/start").is_empty(),
            "strict handoff 记录绝不能以 cold start 冒充恢复成功"
        );
        let record = resume_store::read_record(&agent, SESSION).unwrap();
        assert_eq!(record.id, "th-gone");
        assert!(record.strict_resume, "失败后必须保留 strict intent 供重试");
    }

    /// **fork 失败绝不退冷启。**
    ///
    /// 普通 resume 失败退冷启是对的:丢上下文比丢服务轻。但这条推理对 fork **不成立** ——
    /// fork 的全部意义就是「带着源群的上下文」,退成空会话之后,用户看到的和成功长得一模一样
    /// (都是一个能正常对话的会话),他会对着一段其实不存在的上下文继续聊。
    ///
    /// 代码里那句「fork 不允许退冷启」的注释是正确的意图,但意图不是门禁:把它改成 fallback
    /// 到 `thread_start` 不会让任何行为测试变红,所以钉在这里。
    #[test]
    fn a_failed_codex_fork_never_falls_back_to_a_cold_start() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&format!(
            "{CODEX_START_CASE}{CODEX_RESUME_CASE}{CODEX_FORK_FAILS_CASE}"
        ));
        let pool = RpcPool::new(InFlight::new());
        // 一条待 fork 的记录:sid 是占位值(codex 的真 thread id 由 `thread/fork` 返回)。
        let origin =
            summon_core::session_id::ForkOrigin::new("gsrc", "th-source", "2026-09-03 11:00:00")
                .unwrap();
        resume_store::write_fork_pending_checked(
            &agent,
            SESSION,
            "codex",
            "th-placeholder",
            None,
            None,
            &origin,
        )
        .unwrap();

        assert!(
            pool.checkout(&chat(&agent)).is_err(),
            "fork 失败必须让这一轮失败,不能悄悄给出一个空会话"
        );
        assert_eq!(sent(tmp.path(), "thread/fork").len(), 1, "该试过 fork");
        assert!(
            sent(tmp.path(), "thread/start").is_empty(),
            "fork 失败绝不能以冷启冒充成功 —— 空会话与带着上下文的会话用户分不出来"
        );
        assert!(
            sent(tmp.path(), "thread/resume").is_empty(),
            "待 fork 的记录不该去 resume 那个还不存在的 sid"
        );
        let record = resume_store::read_record(&agent, SESSION).unwrap();
        assert!(record.fork_pending, "判据必须留着,下一条消息重试");
        assert_eq!(record.forked_from, Some(origin), "血缘不该因失败丢掉");
    }

    /// **换过 cli 之后,待 fork 的记录照样要 fork。**
    ///
    /// fork 记录里的 `cli` 是发起 fork 那一刻源记录的通道名。agent 在落盘之后、第一轮之前换过
    /// cli 时,`read_id_migrating` 的 kind guard 会把 `recorded` 滤成 None —— 如果 `fork_from`
    /// 挂在 `recorded` 上,fork 就读不出来了:静默冷启、`fork_pending` 与血缘一起被抹掉,
    /// 而目标群刚收到「已接上「甲群」的上下文副本」。空会话和带着上下文的会话,用户分不出来。
    #[test]
    fn a_pending_fork_survives_a_cli_change_instead_of_cold_starting() {
        let _guard = process_test_guard();
        let (agent, tmp) =
            agent_with_fake_server(&format!("{}{CODEX_FORK_OK_CASE}", codex_open_cases()));
        let pool = RpcPool::new(InFlight::new());
        let origin =
            summon_core::session_id::ForkOrigin::new("gsrc", "th-source", "2026-09-03 11:00:00")
                .unwrap();
        // 记录的通道是 claude(fork 发起时该 agent 跑 claude),而现在这个 agent 跑 codex。
        resume_store::write_fork_pending_checked(
            &agent,
            SESSION,
            "claude",
            "placeholder",
            None,
            None,
            &origin,
        )
        .unwrap();

        let checked_out = pool.checkout(&chat(&agent));

        assert!(
            sent(tmp.path(), "thread/fork").len() == 1,
            "必须仍然走 fork —— kind guard 不该把一次已授权的 fork 悄悄取消"
        );
        assert!(
            sent(tmp.path(), "thread/start").is_empty(),
            "绝不能冷启:那会让目标群拿到空会话,而回执已经说接上了上下文"
        );
        drop(checked_out);
    }

    #[test]
    fn strict_codex_resume_success_clears_intent_without_cold_start() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());
        resume_store::write(&agent, SESSION, "codex", "th-1", None, None);
        resume_store::mark_strict_checked(&agent, SESSION, "codex", "th-1").unwrap();

        let resumed = pool.checkout(&chat(&agent)).unwrap();

        assert_eq!(resumed.checkout_mode(), Checkout::Resumed);
        assert_eq!(sent(tmp.path(), "thread/resume").len(), 1);
        assert!(sent(tmp.path(), "thread/start").is_empty());
        let record = resume_store::read_record(&agent, SESSION).unwrap();
        assert_eq!(record.id, "th-1");
        assert!(!record.strict_resume, "exact resume 成功后应恢复常规策略");
    }

    #[test]
    fn strict_codex_resume_rejects_a_different_returned_sid() {
        let _guard = process_test_guard();
        let mismatch = r#"
    *'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-other","status":{"type":"idle"},"turns":[]}}}' ;;
"#;
        let (agent, tmp) = agent_with_fake_server(&format!("{CODEX_START_CASE}{mismatch}"));
        let pool = RpcPool::new(InFlight::new());
        resume_store::write(&agent, SESSION, "codex", "th-1", None, None);
        resume_store::mark_strict_checked(&agent, SESSION, "codex", "th-1").unwrap();

        assert!(pool.checkout(&chat(&agent)).is_err());

        assert_eq!(sent(tmp.path(), "thread/resume").len(), 1);
        assert!(sent(tmp.path(), "thread/start").is_empty());
        let record = resume_store::read_record(&agent, SESSION).unwrap();
        assert_eq!(record.id, "th-1");
        assert!(record.strict_resume);
    }

    #[test]
    fn exact_live_pool_reuse_clears_strict_intent_without_respawn() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());
        let first = pool.checkout(&chat(&agent)).unwrap();
        let pgid = first.pgid();
        drop(first);
        resume_store::mark_strict_checked(&agent, SESSION, "codex", "th-1").unwrap();

        let reused = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(reused.checkout_mode(), Checkout::Reused);
        assert_eq!(reused.pgid(), pgid);
        assert_eq!(sent(tmp.path(), "thread/start").len(), 1);
        assert!(
            !resume_store::read_record(&agent, SESSION)
                .unwrap()
                .strict_resume
        );
    }

    /// 进程 spawn 即登记 `InFlight`(daemon 停机才够得着),且**不占用轮键**;evict 时注销。
    #[test]
    fn checkout_registers_under_a_key_that_never_collides_with_the_turn_key() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let inflight = InFlight::new();
        let pool = RpcPool::new(inflight.clone());

        let client = pool.checkout(&chat(&agent)).unwrap();
        assert!(
            inflight.contains(&inflight_key("test", TAG)),
            "常驻进程必须挂在 InFlight 上,否则 daemon stop 收不掉它"
        );
        assert!(
            !inflight.contains(&TurnKey::new("test", TAG)),
            "绝不能占用「这一轮」的键:轮结束时的 finish 会把进程的注册一起摘掉"
        );
        assert_eq!(inflight.len(), 1);

        assert!(pool.evict("test", TAG));
        assert!(
            !inflight.contains(&inflight_key("test", TAG)),
            "evict 应当场腾出 `rpc:` 键(会话改挂 `rpc-closing:` 键收敛,见 evict)"
        );
        assert!(!pool.evict("test", TAG), "幂等:再 evict 一次返回 false");
        drop(client);
    }

    /// fleet 只把进程还活着的无头会话列成空闲。进程被外杀后条目还留在池里(下一次 checkout
    /// 才摘),这时必须已经不算活着 —— 否则 daemon 在跑而 RPC 进程崩了的会话会一直显示空闲。
    #[test]
    fn live_sessions_lists_only_sessions_whose_process_is_running() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());
        assert!(pool.live_sessions().is_empty());

        let client = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(
            pool.live_sessions(),
            vec![(agent.name.clone(), TAG.to_string())]
        );
        kill_and_await_death(&client);
        drop(client);
        assert!(
            pool.handoff_snapshot(&agent.name, TAG).unwrap().is_some(),
            "前提:死进程的条目此刻还在池里"
        );
        assert!(pool.live_sessions().is_empty(), "进程死了就不算活着");
    }

    /// daemon 回给 CLI 的运行时快照里,存活名单取的就是这个共享池:池里进程活着的会话在名单里,
    /// 进程被杀后就不在。测的是 `runtime_status::snapshot` 的生产接线,放在这里是为了用本模块的
    /// 假 server 夹具。
    #[test]
    fn runtime_snapshot_reports_live_sessions_of_the_shared_pool() {
        let _guard = process_test_guard();
        let (mut agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        // 共享池跨用例存活,用独有的 agent 名隔开。
        agent.name = "runtime-snapshot-live-headless".into();
        let listed = |agent: &str| {
            crate::observe::runtime_status::snapshot(1, None, std::time::SystemTime::now())
                .live_headless
                .expect("daemon 恒报存活名单")
                .iter()
                .any(|s| s.agent == agent && s.tag == TAG)
        };
        assert!(!listed(&agent.name));

        let pool = shared_for_tests();
        let client = pool.checkout(&chat(&agent)).unwrap();
        assert!(listed(&agent.name), "池里进程活着的会话要在名单里");
        kill_and_await_death(&client);
        drop(client);
        assert!(!listed(&agent.name), "进程死了就不在名单里");
        // 摘掉共享池里这条死条目,别留给同进程的其它用例。
        let _ = pool.evict(&agent.name, TAG);
    }

    #[test]
    fn live_idle_session_rekeys_exactly_and_can_be_rolled_back() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let inflight = InFlight::new();
        let pool = RpcPool::new(inflight.clone());
        let target_tag = "gfeedfacefeedface";
        let target_session = "soul-test-gfeedfacefeedface";

        let source = pool.checkout(&chat(&agent)).unwrap();
        let pgid = source.pgid();
        let held = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        assert!(held.running);
        assert!(!held.idle, "尚有 PooledClient 在外时不能声称独占空闲");
        drop(source);

        let idle = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        assert_eq!(idle.session_id, "th-1");
        assert_eq!(idle.spawn_cwd, None);
        assert_eq!(idle.pgid, pgid);
        assert!(idle.running);
        assert!(idle.idle);

        let receipt = pool
            .rekey_idle_exact(&agent.name, TAG, target_tag, &idle)
            .expect("live + idle + exact sid/cwd 应迁移成功");
        assert!(pool.handoff_snapshot(&agent.name, TAG).unwrap().is_none());
        assert!(!inflight.contains(&inflight_key(&agent.name, TAG)));
        assert!(inflight.contains(&inflight_key(&agent.name, target_tag)));

        let target = pool
            .checkout(&RpcChat {
                session: target_session,
                tag: target_tag,
                ..chat(&agent)
            })
            .unwrap();
        assert_eq!(target.checkout_mode(), Checkout::Reused);
        assert_eq!(target.pgid(), pgid, "target 必须复用同一个 app-server 进程");
        drop(target);

        pool.rollback_rekey(&receipt)
            .expect("上层持久化提交失败时必须能 exact rollback");
        assert!(pool
            .handoff_snapshot(&agent.name, target_tag)
            .unwrap()
            .is_none());
        let restored = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        assert_eq!(restored.session_id, "th-1");
        assert_eq!(restored.pgid, pgid);
        assert!(restored.running && restored.idle);
        assert!(inflight.contains(&inflight_key(&agent.name, TAG)));
        assert!(!inflight.contains(&inflight_key(&agent.name, target_tag)));

        assert!(pool.evict(&agent.name, TAG));
    }

    #[test]
    fn cursor_idle_session_rekeys_without_loading_or_spawning_again() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let inflight = InFlight::new();
        let pool = RpcPool::new(inflight.clone());
        let target_tag = "gfeedfacefeedface";
        let target_session = "soul-test-gfeedfacefeedface";

        let source = pool.checkout(&chat(&agent)).unwrap();
        let pgid = source.pgid();
        drop(source);
        let idle = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        let receipt = pool
            .rekey_idle_exact(&agent.name, TAG, target_tag, &idle)
            .expect("Cursor ACP live idle 会话也应走同一条 exact rekey");

        let target = pool
            .checkout(&RpcChat {
                session: target_session,
                tag: target_tag,
                ..chat(&agent)
            })
            .unwrap();
        assert_eq!(target.checkout_mode(), Checkout::Reused);
        assert_eq!(target.pgid(), pgid);
        assert_eq!(sent(tmp.path(), "session/new").len(), 1);
        assert!(
            sent(tmp.path(), "session/load").is_empty(),
            "活池 handoff 不该为了换 chat 再 load 一次"
        );
        drop(target);

        pool.rollback_rekey(&receipt).unwrap();
        assert!(inflight.contains(&inflight_key(&agent.name, TAG)));
        assert!(!inflight.contains(&inflight_key(&agent.name, target_tag)));
        assert!(pool.evict(&agent.name, TAG));
    }

    #[test]
    fn cursor_process_config_dirs_are_unique_across_two_tags_of_the_same_agent() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let pool = RpcPool::new(InFlight::new());
        let other_tag = "gfeedfacefeedface";

        let first = pool.checkout(&chat(&agent)).unwrap();
        let second = pool
            .checkout(&RpcChat {
                session: "soul-test-gfeedfacefeedface",
                tag: other_tag,
                ..chat(&agent)
            })
            .unwrap();
        let dirs = acp_config_dirs(tmp.path());

        assert_eq!(dirs.len(), 2);
        assert_ne!(dirs[0], dirs[1]);
        assert!(dirs.iter().all(|dir| dir.is_absolute() && dir.is_dir()));
        let expected_root = std::fs::canonicalize(agent.state_dir().join("cursor-config")).unwrap();
        assert!(dirs.iter().all(|dir| dir.starts_with(&expected_root)));

        drop((first, second));
        assert!(pool.evict(&agent.name, TAG));
        assert!(pool.evict(&agent.name, other_tag));
        assert!(
            wait_until(Duration::from_secs(6), || dirs
                .iter()
                .all(|dir| !dir.exists())),
            "两条会话收敛后 TempDir guard 必须同步释放物理根"
        );
    }

    #[test]
    fn cursor_process_config_dir_follows_rekey_and_source_respawn_gets_a_new_root() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let pool = RpcPool::new(InFlight::new());
        let target_tag = "gfeedfacefeedface";

        let source = pool.checkout(&chat(&agent)).unwrap();
        let original_pgid = source.pgid();
        drop(source);
        let original_dir = acp_config_dirs(tmp.path()).pop().unwrap();
        let idle = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        pool.rekey_idle_exact(&agent.name, TAG, target_tag, &idle)
            .unwrap();

        let target = pool
            .checkout(&RpcChat {
                session: "soul-test-gfeedfacefeedface",
                tag: target_tag,
                ..chat(&agent)
            })
            .unwrap();
        assert_eq!(target.pgid(), original_pgid);
        let recorded_dirs = acp_config_dirs(tmp.path());
        assert_eq!(
            recorded_dirs.as_slice(),
            std::slice::from_ref(&original_dir)
        );
        assert!(original_dir.is_dir(), "rekey 不得提前释放原进程的 guard");

        let replacement = pool.checkout(&chat(&agent)).unwrap();
        let dirs = acp_config_dirs(tmp.path());
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0], original_dir);
        assert_ne!(dirs[0], dirs[1]);
        assert_ne!(replacement.pgid(), original_pgid);
        assert!(dirs.iter().all(|dir| dir.is_dir()));

        drop((target, replacement));
        assert!(pool.evict(&agent.name, target_tag));
        assert!(pool.evict(&agent.name, TAG));
    }

    #[test]
    fn rekey_validation_failures_leave_the_live_source_untouched() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let source_cwd = tmp.path().join("source");
        std::fs::create_dir_all(&source_cwd).unwrap();
        let source_cwd = source_cwd.to_string_lossy().into_owned();
        let inflight = InFlight::new();
        let pool = RpcPool::new(inflight.clone());
        let target_tag = "gfeedfacefeedface";

        let held = pool
            .checkout(&RpcChat {
                cwd: Some(&source_cwd),
                ..chat(&agent)
            })
            .unwrap();
        let pgid = held.pgid();
        let mut held_snapshot = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        assert!(!held_snapshot.idle);
        // 即使调用方错误地把旧候选标成 idle，提交点仍必须按当前 Arc/锁事实复验。
        held_snapshot.idle = true;
        assert!(
            pool.rekey_idle_exact(&agent.name, TAG, target_tag, &held_snapshot)
                .is_err(),
            "仍有 checkout handle 时不是 idle"
        );
        assert_eq!(
            pool.handoff_snapshot(&agent.name, TAG)
                .unwrap()
                .unwrap()
                .pgid,
            pgid
        );
        assert!(!inflight.contains(&inflight_key(&agent.name, target_tag)));
        drop(held);

        let exact = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        let mut wrong_sid = exact.clone();
        wrong_sid.session_id = "th-wrong".to_string();
        let mut wrong_cwd = exact.clone();
        wrong_cwd.spawn_cwd = None;
        let mut wrong_pgid = exact.clone();
        wrong_pgid.pgid += 1;
        for stale in [&wrong_sid, &wrong_cwd, &wrong_pgid] {
            assert!(pool
                .rekey_idle_exact(&agent.name, TAG, target_tag, stale)
                .is_err());
            let source = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
            assert_eq!(source.session_id, "th-1");
            assert_eq!(source.spawn_cwd.as_deref(), Some(source_cwd.as_str()));
            assert_eq!(source.pgid, pgid);
            assert!(source.running && source.idle);
            assert!(!inflight.contains(&inflight_key(&agent.name, target_tag)));
        }

        let target_session = "soul-test-gfeedfacefeedface";
        let target = pool
            .checkout(&RpcChat {
                session: target_session,
                tag: target_tag,
                cwd: Some(&source_cwd),
                ..chat(&agent)
            })
            .unwrap();
        let target_pgid = target.pgid();
        drop(target);
        assert!(
            pool.rekey_idle_exact(&agent.name, TAG, target_tag, &exact)
                .is_err(),
            "target PoolKey 已存在时不得覆盖"
        );
        assert_eq!(
            pool.handoff_snapshot(&agent.name, TAG)
                .unwrap()
                .unwrap()
                .pgid,
            pgid
        );
        assert_eq!(
            pool.handoff_snapshot(&agent.name, target_tag)
                .unwrap()
                .unwrap()
                .pgid,
            target_pgid
        );

        assert!(pool.evict(&agent.name, TAG));
        assert!(pool.evict(&agent.name, target_tag));
    }

    #[test]
    fn dead_session_cannot_be_rekeyed() {
        let _guard = process_test_guard();
        let (agent, _tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let inflight = InFlight::new();
        let pool = RpcPool::new(inflight.clone());
        let target_tag = "gfeedfacefeedface";

        let client = pool.checkout(&chat(&agent)).unwrap();
        let pgid = client.pgid();
        kill_and_await_death(&client);
        drop(client);
        let mut dead = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        assert!(!dead.running && !dead.idle);
        // 同 idle 用例：伪造/过期的候选布尔值不能绕过提交点的真实判活。
        dead.running = true;
        dead.idle = true;
        assert!(
            pool.rekey_idle_exact(&agent.name, TAG, target_tag, &dead)
                .is_err(),
            "进程已死时不能只搬 key 冒充成功 handoff"
        );
        let source = pool.handoff_snapshot(&agent.name, TAG).unwrap().unwrap();
        assert_eq!(source.pgid, pgid);
        assert!(!source.running);
        assert!(inflight.contains(&inflight_key(&agent.name, TAG)));
        assert!(!inflight.contains(&inflight_key(&agent.name, target_tag)));

        assert!(pool.evict(&agent.name, TAG));
    }

    /// 进程死了再 checkout:旧注册先摘、新进程再登记,表里始终只有一条(旧条目不摘的话
    /// `InFlight::insert` 会当场把新进程杀掉)。
    #[test]
    fn respawn_swaps_the_inflight_registration_instead_of_stacking_it() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let inflight = InFlight::new();
        let pool = RpcPool::new(inflight.clone());

        let first = pool.checkout(&chat(&agent)).unwrap();
        kill_and_await_death(&first);
        drop(first);

        let revived = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(inflight.len(), 1, "同一个 chat 在表里只该有一条");
        assert!(inflight.contains(&inflight_key("test", TAG)));
        assert!(group_alive(revived.pgid()), "新进程应真的活着");
    }

    /// 停机屏障已立时不许再建会话:`InFlight::insert` 已把新生进程组收掉,`checkout` 必须失败,
    /// 而不是交出一个正在死的 client。
    #[test]
    fn checkout_fails_closed_after_the_shutdown_barrier() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let inflight = InFlight::new();
        inflight.freeze();
        let pool = RpcPool::new(inflight);

        assert!(
            pool.checkout(&chat(&agent)).is_err(),
            "停机后不该再交出新会话"
        );
    }

    /// **计划冻结验收**:线程 A 正在消费慢发的流式增量,线程 B 经把手打断 —— A 必须看到终态。
    /// 假 server 收到 `turn/interrupt` 才回 `turn/completed{status:aborted}`,所以这条同时证明打断帧
    /// 真的从另一个线程写进了同一根 stdin。
    ///
    /// 总 deadline 给 15s 而非 2s:并行跑全量(3000+ 测试)时,假 server 进程启动与 shell 调度
    /// 可能被延迟数秒,2s 预算会 flake。打断延迟(interrupter 的 elapsed)仍守 2s —— 那才是
    /// 真正的性能断言。
    ///
    /// 后台慢发增量的子 shell 在收到 `turn/interrupt` 时被夹具自己收掉:非交互 sh 的后台任务
    /// 忽略 SIGINT,不收的话它会一直持着 stdout 发完全部增量,收尾那段「进程组收敛」就得陪它等完。
    /// 真 server 被打断后也不再推增量。
    #[test]
    fn another_thread_can_interrupt_a_streaming_turn() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"x"}}';
        (i=1; while [ $i -lt 8 ]; do sleep 0.05; printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"x"}}'; i=$((i+1)); done) & streamer=$! ;;
    *'turn/interrupt'*) kill "$streamer" 2>/dev/null; printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"aborted"}}}' ;;
"#;
        let (agent, tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let client = pool.checkout(&chat(&agent)).unwrap();
        let handle = client.interrupt_handle();
        let pgid = client.pgid();

        // 线程 B:等这一轮真的起来再打断(轮还没起就打断是另一条分支,由 idle 那条测覆盖)。
        let interrupter = std::thread::spawn(move || {
            assert!(
                wait_until(Duration::from_secs(5), || handle.has_active_turn()),
                "turn/start 之后把手应能看到在跑的轮"
            );
            let started = Instant::now();
            (handle.interrupt(), started.elapsed())
        });

        // 线程 A:独占消费,直到看见终态。
        let mut guard = codex_guard(&client);
        let turn = guard
            .turn_start("th-1", "数到 100", &Default::default(), None)
            .unwrap();
        assert_eq!(turn.id, "turn-1");
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut deltas = 0_usize;
        let status = loop {
            assert!(Instant::now() < deadline, "15s 内必须收敛到终态");
            match guard.recv_notification(Some(Duration::from_millis(200))) {
                Ok(CodexAppServerNotification::TurnCompleted { turn, .. }) => break turn.status,
                Ok(CodexAppServerNotification::AgentMessageDelta { .. }) => deltas += 1,
                Ok(_) => {}
                // 超时**只是这一片没等到事件**,不是错误 —— 生产消费循环对它也只是 continue。
                // 并行跑测试时假 server 被调度晚一拍就会命中 200ms 的空片,当成 panic 会 flake
                // (实测复现)。真正的上界是外面那个 2s deadline。
                Err(CodexAppServerError::Timeout { .. }) => continue,
                Err(error) => panic!("流式消费不该出错:{error}"),
            }
        };
        assert_eq!(status, "aborted");
        assert!(deltas > 0, "应该真的在消费流式增量,否则这条测没验到并发");
        drop(guard);

        let (outcome, elapsed) = interrupter.join().unwrap();
        assert_eq!(
            outcome,
            RpcInterruptOutcome::Settled,
            "server 已按 turn/interrupt 收敛,不该升级到 SIGINT"
        );
        assert!(elapsed < Duration::from_secs(2), "打断耗时 {elapsed:?}");
        assert_eq!(sent(tmp.path(), "turn/interrupt").len(), 1);
        assert!(group_alive(pgid), "软打断成功就不该把会话进程一起杀了");

        // 进程组收敛:池摘掉最后一个持有者后,Drop 负责 SIGINT 整组回收,不留孤儿。
        pool.evict("test", TAG);
        drop(client);
        assert!(
            wait_until(Duration::from_secs(2), || !group_alive(pgid)),
            "会话释放后进程组必须收敛"
        );
    }

    /// server 压根不理 `turn/interrupt` 时的升级路径:宽限到点 SIGINT 整组,进程组随即收敛,
    /// 消费线程也不会永远挂着。
    #[test]
    fn interrupt_escalates_to_sigint_when_the_server_ignores_it() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}'; i=0; while [ $i -lt 200 ]; do printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"turn-1","itemId":"m","delta":"x"}}'; sleep 0.05; i=$((i+1)); done ;;
"#;
        let (agent, _tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let client = pool.checkout(&chat(&agent)).unwrap();
        let handle = client.interrupt_handle();
        let pgid = client.pgid();
        let (started, start_confirmed) = std::sync::mpsc::channel();

        let interrupter = std::thread::spawn(move || {
            start_confirmed
                .recv_timeout(Duration::from_secs(5))
                .expect("server 应先确认 turn/start");
            assert!(wait_until(Duration::from_secs(5), || handle.has_active_turn()));
            // server 一直在前台循环里发增量、根本不读 stdin,打断帧永远等不到回应:升级是必然的,
            // 宽限多长都只是白等,取短值。
            match &handle {
                SoftInterrupt::Codex(codex) => codex.interrupt_within(Duration::from_millis(50)),
                SoftInterrupt::Cursor(_) => panic!("codex agent 不该开出 cursor 把手"),
            }
        });

        let mut guard = codex_guard(&client);
        guard
            .turn_start("th-1", "数到 100", &Default::default(), None)
            .unwrap();
        // 本地 active 位先于请求应答设置，收到应答后才进入正在运行的轮的升级场景。
        started.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let ended = loop {
            assert!(Instant::now() < deadline, "SIGINT 之后消费侧必须尽快返回");
            match guard.recv_notification(Some(Duration::from_millis(200))) {
                // 进程被 SIGINT 收掉 → stdout EOF → 消费侧拿到断开类错误,这就是终态。
                Err(CodexAppServerError::Timeout { .. }) => continue,
                Err(_) => break true,
                Ok(_) => {}
            }
        };
        assert!(ended);
        drop(guard);

        assert_eq!(interrupter.join().unwrap(), RpcInterruptOutcome::Escalated);
        assert!(
            wait_until(Duration::from_secs(2), || !group_alive(pgid)),
            "升级之后进程组必须收敛"
        );
    }

    /// 竞态容忍(设计文档 §5.5):轮已经结束之后才到的打断 —— 不 panic、不发帧、**尤其不许 SIGINT**,
    /// 否则会把会话进程连同下一轮一起杀掉。
    #[test]
    fn interrupt_after_the_turn_ended_is_a_no_op_and_never_kills_the_next_turn() {
        let _guard = process_test_guard();
        let cases = r#"
    *'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}'; printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}' ;;
"#;
        let (agent, tmp) = agent_with_fake_server(cases);
        let pool = RpcPool::new(InFlight::new());
        let client = pool.checkout(&chat(&agent)).unwrap();
        let handle = client.interrupt_handle();
        let pgid = client.pgid();

        {
            let mut guard = codex_guard(&client);
            let turn = guard
                .turn_start("th-1", "ping", &Default::default(), None)
                .unwrap();
            let done = guard
                .wait_for_turn_completed("th-1", &turn.id, Duration::from_secs(3))
                .unwrap();
            assert_eq!(done.status, "completed");
        }

        assert!(!handle.has_active_turn(), "终态后不该再有在跑的轮");
        assert_eq!(handle.interrupt(), RpcInterruptOutcome::Idle);
        assert!(
            sent(tmp.path(), "turn/interrupt").is_empty(),
            "没有在跑的轮就不该发打断帧(server 侧必然 expectedTurnId 失配)"
        );
        assert!(group_alive(pgid), "迟到的打断绝不许杀掉会话进程");

        // 「不误杀新 turn」的正面证据:同一个进程还能照常再跑一轮。
        let mut guard = codex_guard(&client);
        let next = guard
            .turn_start("th-1", "再来", &Default::default(), None)
            .unwrap();
        assert_eq!(next.id, "turn-1");
    }

    // ---- cursor ACP 分支 ----

    /// cursor 冷启后先不落空 SID；prompt 提交后才落在 ACP 自己的通道键上。
    ///
    /// 通道键(`cursor-acp`)而不是裸 `cursor`:ACP 的 sessionId 与 `cursor -p --resume` 认的
    /// `~/.cursor/chats` id 是两个不互通的命名空间(真机已证),共用一个键就会让逃生门切回 exec
    /// 之后拿着 acp-uuid 去 `--resume`。同理它更不能被 codex 认领。
    #[test]
    fn cursor_cold_checkout_defers_the_sid_until_prompt_commit() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let client = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(client.checkout_mode(), Checkout::ColdStart);
        assert_eq!(client.session_id(), "sess-1");
        assert!(resume_store::read_record(&agent, SESSION).is_none());
        commit_cursor_prompt(&agent, &client);
        assert_eq!(
            resume_store::read_id(&agent, SESSION, "cursor-acp").as_deref(),
            Some("sess-1")
        );
        assert!(
            resume_store::read_id(&agent, SESSION, "cursor").is_none(),
            "ACP sessionId 绝不能被 `-p --resume` 那条通道读到 —— 它是另一个命名空间"
        );
        assert!(
            resume_store::read_id(&agent, SESSION, "codex").is_none(),
            "cursor 的会话 id 绝不能被 codex 认领"
        );
        assert_eq!(sent(tmp.path(), "session/new").len(), 1);
        assert!(sent(tmp.path(), "session/load").is_empty());
        // 池对两种协议同构:cursor 也必须挂在 InFlight 的 `rpc:` 键上。
        assert!(client.is_running());
    }

    /// cursor 懒重连:进程没了 → 下一条消息来时 `session/load` 接回上下文(不是重新 new 一个)。
    #[test]
    fn cursor_lazy_reconnect_loads_the_recorded_session() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let first = pool.checkout(&chat(&agent)).unwrap();
        commit_cursor_prompt(&agent, &first);
        kill_and_await_death(&first);
        drop(first);

        let revived = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(revived.checkout_mode(), Checkout::Resumed);
        assert_eq!(revived.session_id(), "sess-1");
        let loads = sent(tmp.path(), "session/load");
        assert_eq!(loads.len(), 1, "懒重连必须真的发出 session/load");
        assert_eq!(loads[0]["params"]["sessionId"], "sess-1");
        assert_eq!(
            sent(tmp.path(), "session/new").len(),
            1,
            "有 sid 就不该再冷启一次"
        );
    }

    /// **降级分支(此前 0 测试)**:盘上有 sid、但 `session/load` 失败 → 退回 `session/new`,
    /// 模式如实报 `ColdStart`。旧 sid 保留到 prompt 提交，之后才被新 sid 覆盖。
    ///
    /// 这条与 codex 那条同构,但 cursor 多一件事:load 失败**之前**已经重放了一段历史通知,
    /// 那批残留必须在失败路径上也被丢掉(成功路径的 drain 压根没跑到)。残留有没有漏进下一轮
    /// 由 `capture::rpc` 侧的引擎测断言;这里只钉降级本身发生了。
    #[test]
    fn cursor_load_failure_keeps_the_stale_sid_until_the_cold_prompt_commits() {
        let _guard = process_test_guard();
        let (agent, tmp) =
            cursor_agent_with_fake_server(&format!("{ACP_NEW_CASE}{ACP_LOAD_FAILS_CASE}"));
        let pool = RpcPool::new(InFlight::new());
        resume_store::write(&agent, SESSION, "cursor-acp", "sess-gone", None, None);

        let client = pool.checkout(&chat(&agent)).unwrap();

        assert_eq!(
            client.checkout_mode(),
            Checkout::ColdStart,
            "load 失败必须如实报冷启"
        );
        assert_eq!(client.session_id(), "sess-1");
        assert_eq!(sent(tmp.path(), "session/load").len(), 1, "该试一次 load");
        assert_eq!(
            sent(tmp.path(), "session/new").len(),
            1,
            "失败后必须真的退回冷启"
        );
        assert_eq!(
            resume_store::read_id(&agent, SESSION, "cursor-acp").as_deref(),
            Some("sess-gone"),
            "prompt 前失败时保留 stale sid，新 daemon 会再 load 失败并正确冷启"
        );
        commit_cursor_prompt(&agent, &client);
        assert_eq!(
            resume_store::read_id(&agent, SESSION, "cursor-acp").as_deref(),
            Some("sess-1"),
            "prompt 已写出后，失效的旧 sid 必须被新会话覆盖"
        );
    }

    #[test]
    fn strict_cursor_load_failure_never_creates_or_overwrites_a_session() {
        let _guard = process_test_guard();
        let (agent, tmp) =
            cursor_agent_with_fake_server(&format!("{ACP_NEW_CASE}{ACP_LOAD_FAILS_CASE}"));
        let pool = RpcPool::new(InFlight::new());
        resume_store::write(&agent, SESSION, "cursor-acp", "sess-gone", None, None);
        resume_store::mark_strict_checked(&agent, SESSION, "cursor-acp", "sess-gone").unwrap();

        assert!(pool.checkout(&chat(&agent)).is_err());
        assert_eq!(sent(tmp.path(), "session/load").len(), 1);
        assert!(
            sent(tmp.path(), "session/new").is_empty(),
            "strict ACP handoff 记录绝不能退 session/new"
        );
        let record = resume_store::read_record(&agent, SESSION).unwrap();
        assert_eq!(record.id, "sess-gone");
        assert!(record.strict_resume);
    }

    #[test]
    fn strict_cursor_load_success_clears_intent_without_new_session() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let pool = RpcPool::new(InFlight::new());
        resume_store::write(&agent, SESSION, "cursor-acp", "sess-1", None, None);
        resume_store::mark_strict_checked(&agent, SESSION, "cursor-acp", "sess-1").unwrap();

        let resumed = pool.checkout(&chat(&agent)).unwrap();

        assert_eq!(resumed.checkout_mode(), Checkout::Resumed);
        assert_eq!(sent(tmp.path(), "session/load").len(), 1);
        assert!(sent(tmp.path(), "session/new").is_empty());
        let record = resume_store::read_record(&agent, SESSION).unwrap();
        assert_eq!(record.id, "sess-1");
        assert!(!record.strict_resume, "exact load 成功后应恢复常规策略");
    }

    #[test]
    fn strict_cursor_load_rejects_a_different_returned_sid_without_new_session() {
        let _guard = process_test_guard();
        let (agent, tmp) =
            cursor_agent_with_fake_server(&format!("{ACP_NEW_CASE}{ACP_LOAD_WRONG_SESSION_CASE}"));
        let pool = RpcPool::new(InFlight::new());
        resume_store::write(&agent, SESSION, "cursor-acp", "sess-1", None, None);
        resume_store::mark_strict_checked(&agent, SESSION, "cursor-acp", "sess-1").unwrap();

        assert!(pool.checkout(&chat(&agent)).is_err());
        assert_eq!(sent(tmp.path(), "session/load").len(), 1);
        assert!(
            sent(tmp.path(), "session/new").is_empty(),
            "返回 SID 漂移时 strict ACP owner 不得退 session/new"
        );
        let record = resume_store::read_record(&agent, SESSION).unwrap();
        assert_eq!(record.id, "sess-1");
        assert!(record.strict_resume, "SID 漂移后必须保留 strict intent");
    }

    /// `evict` 之后再 checkout **绝不能复用**:面板「退出/重置」、`agents kill`、群解散都靠这条
    /// 把旧会话真的换掉。错成 Reused 就是「重置了个寂寞」—— 不报错、不丢回复,只是重置没发生。
    #[test]
    fn checkout_after_evict_is_never_reused() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let first = pool.checkout(&chat(&agent)).unwrap();
        let first_pgid = first.pgid();
        assert!(pool.evict(&agent.name, TAG));
        drop(first);

        let next = pool.checkout(&chat(&agent)).unwrap();
        assert_ne!(
            next.checkout_mode(),
            Checkout::Reused,
            "evict 之后的 checkout 必须重开进程,而不是从池里捞回同一条"
        );
        assert_ne!(next.pgid(), first_pgid, "应是一个全新的进程");
    }

    /// 改工作目录:cwd 钉在进程/会话上(codex `thread/start.cwd`、cursor `session/new.cwd` 必填),
    /// 开完改不了。所以 cwd 一变就必须**换进程**并 resume 接回,与 `/model` 那条同构。
    ///
    /// 这条如果错成「照常复用」,面板改工作目录就永远静默不生效:agent 一直在旧目录里干活。
    #[test]
    fn cwd_change_respawns_the_session_instead_of_silently_reusing_it() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let old_cwd = tmp.path().join("old");
        let new_cwd = tmp.path().join("new");
        std::fs::create_dir_all(&old_cwd).unwrap();
        std::fs::create_dir_all(&new_cwd).unwrap();
        let (old_cwd, new_cwd) = (old_cwd.to_string_lossy(), new_cwd.to_string_lossy());
        let pool = RpcPool::new(InFlight::new());

        let first = pool
            .checkout(&RpcChat {
                cwd: Some(&old_cwd),
                ..chat(&agent)
            })
            .unwrap();
        let first_pgid = first.pgid();

        // 同一个 cwd:热路径复用。
        let same = pool
            .checkout(&RpcChat {
                cwd: Some(&old_cwd),
                ..chat(&agent)
            })
            .unwrap();
        assert_eq!(same.checkout_mode(), Checkout::Reused);
        assert_eq!(same.pgid(), first_pgid);
        drop(same);

        assert!(
            pool.checkout(&RpcChat {
                cwd: Some(&new_cwd),
                ..chat(&agent)
            })
            .is_err(),
            "上一轮 handle 未释放前不得杀旧 writer 或并发创建 replacement"
        );
        assert!(first.is_running());
        drop(first);

        let switched = pool
            .checkout(&RpcChat {
                cwd: Some(&new_cwd),
                ..chat(&agent)
            })
            .unwrap();
        assert_eq!(
            switched.checkout_mode(),
            Checkout::Resumed,
            "换 cwd 要换进程,但上下文靠 thread/resume 接回,不该丢"
        );
        assert_ne!(switched.pgid(), first_pgid, "换 cwd 必须是新进程");
        let resumes = sent(tmp.path(), "thread/resume");
        assert_eq!(resumes.len(), 1);
        assert_eq!(
            resumes[0]["params"]["cwd"], *new_cwd,
            "接回时必须钉**新** cwd"
        );
    }

    /// Fast feature 属于 app-server **进程**能力：从 Standard 切 Fast 时，旧进程若没显式启用就
    /// 必须重建并 resume 同一 thread；Fast 切回 Standard 只需下一轮发 serviceTier=null，进程可复用。
    #[test]
    fn enabling_fast_respawns_app_server_once_and_preserves_the_thread() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let first = pool
            .checkout(&RpcChat {
                service_tier: Some(CodexServiceTier::Standard),
                ..chat(&agent)
            })
            .unwrap();
        let first_pgid = first.pgid();
        assert_eq!(first.checkout_mode(), Checkout::ColdStart);
        drop(first);

        let fast = pool
            .checkout(&RpcChat {
                service_tier: Some(CodexServiceTier::Fast),
                ..chat(&agent)
            })
            .unwrap();
        assert_eq!(fast.checkout_mode(), Checkout::Resumed);
        assert_ne!(
            fast.pgid(),
            first_pgid,
            "启用 feature 必须换 app-server 进程"
        );
        let fast_pgid = fast.pgid();
        drop(fast);
        assert_eq!(
            sent(tmp.path(), "thread/resume").len(),
            1,
            "进程重建后必须接回原 thread，不能丢上下文"
        );

        let standard = pool
            .checkout(&RpcChat {
                service_tier: Some(CodexServiceTier::Standard),
                ..chat(&agent)
            })
            .unwrap();
        assert_eq!(standard.checkout_mode(), Checkout::Reused);
        assert_eq!(standard.pgid(), fast_pgid, "关闭 Fast 不需要再重启进程");
    }

    /// parameterized bundle 必须在 prompt 前把 context/reasoning/Fast 全部收敛；同值不重发，
    /// 后续 `/model` 也走同一个 setter、同一个 SID/进程，不再混用 legacy set_model namespace。
    #[test]
    fn cursor_parameter_bundle_converges_all_axes_on_the_same_live_session() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
        let (agent, tmp) = cursor_agent_with_parameter_seed(
            &cases,
            "gpt-5.6-sol[context=1m,reasoning=xhigh,fast=true]",
        );
        let pool = RpcPool::new(InFlight::new());

        let first = pool.checkout(&chat(&agent)).unwrap();
        let first_pgid = first.pgid();
        assert_eq!(first.checkout_mode(), Checkout::ColdStart);
        let frames = sent(tmp.path(), "session/set_config_option");
        assert_eq!(
            frames
                .iter()
                .map(|frame| (
                    frame["params"]["configId"].as_str().unwrap(),
                    frame["params"]["value"].as_str().unwrap()
                ))
                .collect::<Vec<_>>(),
            [
                ("model", "gpt-5.6-sol"),
                ("context", "1m"),
                ("reasoning", "xhigh"),
                ("fast", "true")
            ],
            "新 session 尚未验过 bundle generation，必须先用 model setter 刷新 strict 目录，再按依赖顺序整组收敛"
        );
        drop(first);

        let same = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(same.checkout_mode(), Checkout::Reused);
        assert_eq!(same.pgid(), first_pgid);
        assert_eq!(
            sent(tmp.path(), "session/set_config_option").len(),
            4,
            "全轴 exact 后，同一 bundle 不得重复写"
        );
        drop(same);

        crate::capture::cursor_config::set_selected_model(&agent, 0, "composer-2.5").unwrap();
        let switched = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(
            switched.checkout_mode(),
            Checkout::Reused,
            "模型切换不应重连或丢上下文"
        );
        assert_eq!(switched.pgid(), first_pgid, "换模型必须是同一个进程");
        let frames = sent(tmp.path(), "session/set_config_option");
        assert_eq!(frames.len(), 5, "换模型必须走统一 parameterized setter");
        assert_eq!(frames[4]["params"]["configId"], "model");
        assert_eq!(frames[4]["params"]["value"], "composer-2.5");
        assert_eq!(frames[4]["params"]["sessionId"], "sess-1");
        assert_eq!(
            pool.current_model("test", TAG).as_deref(),
            Some("composer-2.5")
        );
        assert_eq!(
            sent(tmp.path(), "session/new").len(),
            1,
            "全程只开过一次会话"
        );
        assert!(
            sent(tmp.path(), "session/load").is_empty(),
            "没有任何重连发生"
        );
        drop(switched);
    }

    /// 本群覆盖进收敛目标:本群换了模型,下一轮在原会话里切过去;清掉覆盖,下一轮切回默认。
    #[test]
    fn cursor_converges_to_the_chats_own_overlay_and_back_to_the_default() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
        let (agent, tmp) = cursor_agent_with_parameter_seed(&cases, "gpt-5.6-sol");
        let pool = RpcPool::new(InFlight::new());
        let first = pool.checkout(&chat(&agent)).unwrap();
        let pgid = first.pgid();
        drop(first);
        let model_frames = || {
            sent(tmp.path(), "session/set_config_option")
                .into_iter()
                .filter(|frame| frame["params"]["configId"] == "model")
                .map(|frame| frame["params"]["value"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(model_frames(), ["gpt-5.6-sol"]);

        let overlay = crate::capture::cursor_config::set_chat_selected_model(
            &agent,
            TAG,
            0,
            Some("composer-2.5"),
        )
        .unwrap();
        let own = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(own.checkout_mode(), Checkout::Reused);
        assert_eq!(own.pgid(), pgid);
        assert_eq!(model_frames(), ["gpt-5.6-sol", "composer-2.5"]);
        drop(own);

        crate::capture::cursor_config::set_chat_selected_model(
            &agent,
            TAG,
            overlay.bundle.generation,
            None,
        )
        .unwrap();
        let back = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(back.pgid(), pgid);
        assert_eq!(
            model_frames(),
            ["gpt-5.6-sol", "composer-2.5", "gpt-5.6-sol"]
        );
        drop(back);
        pool.evict(&agent.name, TAG);
    }

    fn model_and_option_frames(dir: &std::path::Path) -> Vec<(String, String)> {
        sent(dir, "session/set_config_option")
            .into_iter()
            .map(|frame| {
                (
                    frame["params"]["configId"].as_str().unwrap().to_string(),
                    frame["params"]["value"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    const OTHER_TAG: &str = "gfeedfacefeedface";

    fn other_chat(agent: &Agent) -> RpcChat<'_> {
        RpcChat {
            session: "soul-test-gfeedfacefeedface",
            tag: OTHER_TAG,
            ..chat(agent)
        }
    }

    /// 两个群各有自己的覆盖(一个换模型、一个只改 reasoning):各自进程收敛到各自目标,同代再取
    /// 不重复写。
    #[test]
    fn two_chats_converge_to_their_own_cursor_targets_side_by_side() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
        let (agent, tmp) = cursor_agent_with_parameter_seed(&cases, "gpt-5.6-sol");
        crate::capture::cursor_config::set_chat_selected_model(
            &agent,
            TAG,
            0,
            Some("composer-2.5"),
        )
        .unwrap();
        crate::capture::cursor_config::set_chat_model_option(
            &agent,
            OTHER_TAG,
            0,
            "gpt-5.6-sol",
            "reasoning",
            Some("xhigh"),
        )
        .unwrap();
        let pool = RpcPool::new(InFlight::new());

        drop(pool.checkout(&chat(&agent)).unwrap());
        let after_a = model_and_option_frames(tmp.path());
        assert_eq!(after_a, [("model".to_string(), "composer-2.5".to_string())]);
        drop(pool.checkout(&other_chat(&agent)).unwrap());
        let after_b = model_and_option_frames(tmp.path());
        assert_eq!(
            &after_b[after_a.len()..],
            [
                ("model".to_string(), "gpt-5.6-sol".to_string()),
                ("reasoning".to_string(), "xhigh".to_string()),
            ]
        );
        drop(pool.checkout(&chat(&agent)).unwrap());
        drop(pool.checkout(&other_chat(&agent)).unwrap());
        assert_eq!(
            model_and_option_frames(tmp.path()).len(),
            after_b.len(),
            "同代不重写"
        );
        pool.evict(&agent.name, TAG);
        pool.evict(&agent.name, OTHER_TAG);
    }

    /// 只钉了模型的群照样跟随默认的 reasoning 改动;本群层代际变了,即使目标值没变也先强制刷新
    /// 目录(与默认层代际同一条规矩)。
    #[test]
    fn a_chat_pinning_only_the_model_follows_default_option_changes() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
        let (agent, tmp) = cursor_agent_with_parameter_seed(&cases, "gpt-5.6-sol");
        let pinned = crate::capture::cursor_config::set_chat_selected_model(
            &agent,
            TAG,
            0,
            Some("gpt-5.6-sol"),
        )
        .unwrap();
        let pool = RpcPool::new(InFlight::new());
        drop(pool.checkout(&chat(&agent)).unwrap());
        let before = model_and_option_frames(tmp.path()).len();

        crate::capture::cursor_config::set_model_option_for_model(
            &agent,
            0,
            "gpt-5.6-sol",
            "reasoning",
            "xhigh",
        )
        .unwrap();
        drop(pool.checkout(&chat(&agent)).unwrap());
        let frames = model_and_option_frames(tmp.path());
        assert!(
            frames[before..].contains(&("reasoning".to_string(), "xhigh".to_string())),
            "{frames:?}"
        );

        // 本群层再写一次同样的模型:目标值不变,代际变了,先用 model setter 强制刷新目录。
        let settled = frames.len();
        crate::capture::cursor_config::set_chat_selected_model(
            &agent,
            TAG,
            pinned.bundle.generation,
            Some("gpt-5.6-sol"),
        )
        .unwrap();
        drop(pool.checkout(&chat(&agent)).unwrap());
        assert_eq!(
            model_and_option_frames(tmp.path())[settled..].first(),
            Some(&("model".to_string(), "gpt-5.6-sol".to_string())),
            "本群层代际变化必须触发一次强制刷新"
        );
        pool.evict(&agent.name, TAG);
    }

    /// 本群覆盖读不懂:checkout 不放行 prompt。
    #[test]
    fn an_unreadable_chat_overlay_blocks_the_turn() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
        let (agent, _tmp) = cursor_agent_with_parameter_seed(&cases, "gpt-5.6-sol");
        let overlay = crate::runtime::chat_model::chat_dir(&agent, TAG)
            .unwrap()
            .join(crate::capture::cursor_config::CURSOR_CONFIG_BUNDLE_FILE);
        std::fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        std::fs::write(&overlay, b"{not json").unwrap();
        let pool = RpcPool::new(InFlight::new());
        assert!(pool.checkout(&chat(&agent)).is_err());
        pool.evict(&agent.name, TAG);
    }

    /// 应用期间本群层被改(这里由假 server 在写 reasoning 时改写本群覆盖):应用后复核两层代际,
    /// 本群层一变就不放行这一轮。
    #[test]
    fn a_chat_overlay_changed_during_apply_fails_the_post_apply_check() {
        let _guard = process_test_guard();
        let base = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
        let (agent, tmp) = cursor_agent_with_parameter_seed(&base, "gpt-5.6-sol");
        crate::capture::cursor_config::set_chat_model_option(
            &agent,
            TAG,
            0,
            "gpt-5.6-sol",
            "reasoning",
            Some("xhigh"),
        )
        .unwrap();
        let overlay = crate::runtime::chat_model::chat_dir(&agent, TAG)
            .unwrap()
            .join(crate::capture::cursor_config::CURSOR_CONFIG_BUNDLE_FILE);
        let rewrite = format!(
            r#"
    *'"configId":"reasoning"'*) printf '%s' '{{"version":1,"generation":7,"selected_model":null,"profiles":{{"gpt-5.6-sol":{{"options":{{"reasoning":"xhigh"}}}}}}}}' > '{}';
        reasoning=xhigh; printf '%s
' '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"configOptions":[{{"id":"model","name":"Model","type":"select","currentValue":"gpt-5.6-sol","options":[{{"value":"gpt-5.6-sol","name":"GPT-5.6 SOL"}},{{"value":"composer-2.5","name":"Composer"}}]}},{{"id":"context","name":"Context","type":"select","currentValue":"272k","options":[{{"value":"272k","name":"272K"}},{{"value":"1m","name":"1M"}}]}},{{"id":"reasoning","name":"Reasoning","type":"select","currentValue":"xhigh","options":[{{"value":"medium","name":"Medium"}},{{"value":"xhigh","name":"XHigh"}}]}},{{"id":"fast","name":"Fast","type":"select","currentValue":"false","options":[{{"value":"false","name":"Off"}},{{"value":"true","name":"Fast"}}]}}]}}}}' ;;
"#,
            overlay.display()
        );
        write_fake_acp_server(
            tmp.path(),
            &format!("{rewrite}{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}"),
        );
        let pool = RpcPool::new(InFlight::new());
        let error = pool
            .checkout(&chat(&agent))
            .err()
            .expect("应用期间被改必须拒绝");
        assert!(error.to_string().contains("应用期间被外部更新"), "{error}");
        pool.evict(&agent.name, TAG);
    }

    #[test]
    fn cursor_completion_badge_tracks_model_switch_on_the_same_session() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
        let (mut agent, tmp) = cursor_agent_with_parameter_seed(&cases, "gpt-5.6-sol");
        agent.name = "cursor-model-badge-switch".into();
        let pool = shared_for_tests();
        let first = pool.checkout(&chat(&agent)).unwrap();
        let pgid = first.pgid();
        drop(first);
        let seed = summon_core::feishu::TurnBadge {
            cli: "cursor".into(),
            model: pool.current_model(&agent.name, TAG),
            ..Default::default()
        };
        assert_eq!(seed.model.as_deref(), Some("gpt-5.6-sol"));
        crate::capture::cursor_config::set_selected_model(&agent, 0, "composer-2.5").unwrap();
        let switched = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(switched.checkout_mode(), Checkout::Reused);
        assert_eq!(switched.pgid(), pgid);
        assert_eq!(
            pool.current_model(&agent.name, TAG).as_deref(),
            Some("composer-2.5")
        );
        let completed = crate::runtime::daemon::completion_badge_for_test(
            &seed,
            &agent.name,
            &agent.state_dir(),
            TAG,
        );
        assert_eq!(completed.model.as_deref(), Some("composer-2.5"));
        let rendered = completed.render().unwrap();
        assert!(rendered.contains("composer-2.5"));
        assert!(!rendered.contains("gpt-5.6-sol"));
        let other_chat = crate::runtime::daemon::completion_badge_for_test(
            &seed,
            &agent.name,
            &agent.state_dir(),
            "other-chat",
        );
        assert_eq!(other_chat.model, seed.model);
        let codex_seed = summon_core::feishu::TurnBadge {
            cli: "codex".into(),
            ..seed.clone()
        };
        let codex = crate::runtime::daemon::completion_badge_for_test(
            &codex_seed,
            &agent.name,
            &agent.state_dir(),
            TAG,
        );
        assert_eq!(codex.model, seed.model);
        assert_eq!(sent(tmp.path(), "session/new").len(), 1);
        assert!(sent(tmp.path(), "session/load").is_empty());
        drop(switched);
        pool.evict(&agent.name, TAG);
    }

    #[test]
    fn cursor_parameter_apply_order_keeps_thinking_before_reasoning_and_effort() {
        let mut ids = vec![
            "fast",
            "effort",
            "other",
            "reasoning",
            "thinking",
            "context",
        ];
        ids.sort_by_key(|id| cursor_config_apply_rank(id));
        assert_eq!(
            ids,
            [
                "context",
                "thinking",
                "effort",
                "reasoning",
                "other",
                "fast"
            ]
        );
    }

    #[test]
    fn cursor_top_level_model_requires_an_exact_dynamic_base_without_a_parameter_bundle() {
        let _guard = process_test_guard();
        for model in ["gpt-5.6-sol-xhigh", " gpt-5.6-sol "] {
            let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
            let (mut agent, tmp) = cursor_agent_with_fake_server(&cases);
            agent.model = Some(model.to_string());
            let pool = RpcPool::new(InFlight::new());

            let error = match pool.checkout(&chat(&agent)) {
                Ok(_) => panic!("顶层模型 `{model}` 不得 trim/截后缀后冒充 dynamic base id"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("不能无损映射"));
            assert!(sent(tmp.path(), "session/set_config_option").is_empty());
            assert!(sent(tmp.path(), "session/prompt").is_empty());
        }
    }

    /// generation 发生 false→true→false 的 ABA 时，旧 session 的缓存表面上又等于目标；仍必须
    /// 强制刷新一次 strict live catalog，不能把「旧快照相等」当成「现在已经生效」。
    #[test]
    fn cursor_old_generation_cannot_early_return_after_an_aba() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
        let (agent, tmp) = cursor_agent_with_parameter_seed(&cases, "gpt-5.6-sol[fast=false]");
        let pool = RpcPool::new(InFlight::new());

        let first = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(sent(tmp.path(), "session/set_config_option").len(), 1);
        drop(first);

        let on = crate::capture::cursor_config::set_model_option_for_model(
            &agent,
            0,
            "gpt-5.6-sol",
            "fast",
            "true",
        )
        .unwrap();
        crate::capture::cursor_config::set_model_option_for_model(
            &agent,
            on.bundle.generation,
            "gpt-5.6-sol",
            "fast",
            "false",
        )
        .unwrap();

        let after_aba = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(after_aba.checkout_mode(), Checkout::Reused);
        let frames = sent(tmp.path(), "session/set_config_option");
        assert_eq!(frames.len(), 2, "旧 generation 必须强制拿一次新 exact 回执");
        assert_eq!(frames[1]["params"]["configId"], "model");
        assert_eq!(frames[1]["params"]["value"], "gpt-5.6-sol");
    }

    /// 每个 setter 自己 exact 仍不够：后写 reasoning 可以把先写 context 静默回退。两轮有界重放
    /// 仍到不了 fixed point 时必须 prompt 前失败，绝不能拿 272k 冒充已启用 1m。
    #[test]
    fn cursor_final_vector_rejects_a_later_setter_resetting_an_earlier_axis() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_RESETS_CONTEXT_CASE}");
        let (agent, tmp) =
            cursor_agent_with_parameter_seed(&cases, "gpt-5.6-sol[context=1m,reasoning=xhigh]");
        let pool = RpcPool::new(InFlight::new());

        let error = match pool.checkout(&chat(&agent)) {
            Ok(_) => panic!("整向量不 exact 时不得返回可 prompt 的 client"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("未能稳定收敛"));
        assert!(sent(tmp.path(), "session/prompt").is_empty());
        let axes = sent(tmp.path(), "session/set_config_option")
            .into_iter()
            .map(|frame| frame["params"]["configId"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            axes,
            ["model", "context", "reasoning", "context", "reasoning"],
            "最多两轮；第二轮仍振荡就止损"
        );
    }

    /// 冷启 session 已创建、但参数在 prompt 前失败时，下一次 checkout 虽然是 Reused，仍是这条
    /// 原生会话的第一条 prompt，必须继续携带 PRIMER。
    #[test]
    fn cursor_pre_prompt_config_failure_does_not_consume_the_primer() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_REJECTS_ONCE_CASE}");
        let (agent, _tmp) = cursor_agent_with_parameter_seed(&cases, "gpt-5.6-sol[fast=false]");
        let pool = RpcPool::new(InFlight::new());

        assert!(pool.checkout(&chat(&agent)).is_err());
        let recovered = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(recovered.checkout_mode(), Checkout::Reused);
        assert!(
            recovered.cursor_primer_pending(),
            "checkout 失败只保存 live session，不能消费尚未发出的身份注入"
        );
        recovered.mark_cursor_primer_delivered();
        drop(recovered);

        let later = pool.checkout(&chat(&agent)).unwrap();
        assert!(!later.cursor_primer_pending());
    }

    const AXIS_TAIL: &str = r#"{"value":"composer-2.5","name":"Composer"}]"#;
    const AXIS_TAIL_WITH_NEW_MODEL: &str = r#"{"value":"composer-2.5","name":"Composer"},{"value":"gemini-4-carbon","name":"Gemini 4 Carbon"}]"#;

    /// 新进程下发的目录比常驻进程持有的多一个模型(cursor-agent 自更新、模型上新)。
    fn cursor_cases_with_newer_catalog_after_reload() -> String {
        format!(
            "{ACP_NEW_CASE}{}{}",
            ACP_LOAD_CASE.replace(AXIS_TAIL, AXIS_TAIL_WITH_NEW_MODEL),
            ACP_SET_CONFIG_CASE.replace(AXIS_TAIL, AXIS_TAIL_WITH_NEW_MODEL),
        )
    }

    /// 复用会话的目录不含目标模型:不能每轮卡在 prompt 前,而是收掉常驻进程、按同一 SID
    /// `session/load` 接回(保留上下文),用新目录收敛到目标模型。
    #[test]
    fn cursor_reused_session_reloads_once_when_the_target_model_is_missing_from_its_catalog() {
        let _guard = process_test_guard();
        let (agent, tmp) =
            cursor_agent_with_fake_server(&cursor_cases_with_newer_catalog_after_reload());
        let pool = RpcPool::new(InFlight::new());
        let first = pool.checkout(&chat(&agent)).unwrap();
        let first_pgid = first.pgid();
        commit_cursor_prompt(&agent, &first);
        drop(first);

        crate::capture::cursor_config::set_selected_model(&agent, 0, "gemini-4-carbon").unwrap();
        let refreshed = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(refreshed.checkout_mode(), Checkout::Resumed);
        assert_ne!(refreshed.pgid(), first_pgid, "目录要靠新进程重新下发");
        assert_eq!(
            refreshed.session_id(),
            "sess-1",
            "必须接回同一会话,不能冷启丢上下文"
        );
        assert_eq!(sent(tmp.path(), "session/new").len(), 1);
        let loads = sent(tmp.path(), "session/load");
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0]["params"]["sessionId"], "sess-1");
        let model_frames = sent(tmp.path(), "session/set_config_option")
            .into_iter()
            .filter(|frame| frame["params"]["configId"] == "model")
            .map(|frame| frame["params"]["value"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(model_frames, ["gemini-4-carbon"]);
        assert_eq!(
            pool.current_model("test", TAG).as_deref(),
            Some("gemini-4-carbon")
        );
        drop(refreshed);
        pool.evict(&agent.name, TAG);
    }

    /// 接回后的新目录仍不含目标:报出可操作的错误;同一目标代际下一轮不再重启进程。
    #[test]
    fn cursor_target_missing_after_reload_fails_without_restarting_every_turn() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_CASE}");
        let (agent, tmp) = cursor_agent_with_fake_server(&cases);
        let pool = RpcPool::new(InFlight::new());
        let first = pool.checkout(&chat(&agent)).unwrap();
        commit_cursor_prompt(&agent, &first);
        drop(first);

        let missing =
            crate::capture::cursor_config::set_selected_model(&agent, 0, "gemini-4-carbon")
                .unwrap();
        let error = pool.checkout(&chat(&agent)).err().unwrap();
        assert!(error.downcast_ref::<CursorModelCatalogMiss>().is_some());
        assert!(error.to_string().contains("gemini-4-carbon"), "{error}");
        assert_eq!(sent(tmp.path(), "session/load").len(), 1);

        let again = pool.checkout(&chat(&agent)).err().unwrap();
        assert!(again.downcast_ref::<CursorModelCatalogMiss>().is_some());
        assert_eq!(
            sent(tmp.path(), "session/load").len(),
            1,
            "同一目标已在新目录上核对过,不能每轮重启进程"
        );

        crate::capture::cursor_config::set_selected_model(
            &agent,
            missing.bundle.generation,
            "composer-2.5",
        )
        .unwrap();
        let fixed = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(fixed.checkout_mode(), Checkout::Reused);
        assert_eq!(
            pool.current_model("test", TAG).as_deref(),
            Some("composer-2.5")
        );
        drop(fixed);
        pool.evict(&agent.name, TAG);
    }

    /// 会话建好但还没提交过 prompt(盘上无 SID):刷新目录只能冷启新会话,身份注入仍待发送。
    #[test]
    fn cursor_catalog_refresh_without_a_recorded_sid_cold_starts_and_keeps_the_primer_pending() {
        let _guard = process_test_guard();
        // 第一个进程拿旧目录,之后的进程拿含新模型的目录。
        let fresh_new = ACP_NEW_CASE
            .trim()
            .trim_start_matches("*'session/new'*)")
            .replace(AXIS_TAIL, AXIS_TAIL_WITH_NEW_MODEL);
        let stale_new = ACP_NEW_CASE
            .trim()
            .trim_start_matches("*'session/new'*)")
            .to_string();
        let new_case = format!(
            "\n    *'session/new'*) if [ -e \"$DIR/opened-once\" ]; then {} else : > \"$DIR/opened-once\"; {} fi ;;\n",
            fresh_new.trim().trim_end_matches(";;").trim_end().to_string() + ";",
            stale_new.trim().trim_end_matches(";;").trim_end().to_string() + ";",
        );
        let cases = format!(
            "{new_case}{ACP_LOAD_CASE}{}",
            ACP_SET_CONFIG_CASE.replace(AXIS_TAIL, AXIS_TAIL_WITH_NEW_MODEL),
        );
        let (agent, tmp) = cursor_agent_with_fake_server(&cases);
        let pool = RpcPool::new(InFlight::new());
        let first = pool.checkout(&chat(&agent)).unwrap();
        let first_pgid = first.pgid();
        drop(first);

        crate::capture::cursor_config::set_selected_model(&agent, 0, "gemini-4-carbon").unwrap();
        let refreshed = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(refreshed.checkout_mode(), Checkout::ColdStart);
        assert_ne!(refreshed.pgid(), first_pgid);
        assert!(sent(tmp.path(), "session/load").is_empty());
        assert_eq!(sent(tmp.path(), "session/new").len(), 2);
        assert!(
            refreshed.cursor_primer_pending(),
            "新会话的身份注入还没发出"
        );
        assert_eq!(
            pool.current_model("test", TAG).as_deref(),
            Some("gemini-4-carbon")
        );
        drop(refreshed);
        pool.evict(&agent.name, TAG);
    }

    /// `model` 轴存在但为空时不算目录,候选退回 legacy `availableModels`。
    #[test]
    fn cursor_available_models_fall_back_to_legacy_models_when_the_model_axis_is_empty() {
        let _guard = process_test_guard();
        let cases = ACP_NEW_CASE.replace(
            r#""options":[{"value":"gpt-5.6-sol","name":"GPT-5.6 SOL"},{"value":"composer-2.5","name":"Composer"}]"#,
            r#""options":[]"#,
        );
        assert_ne!(cases, ACP_NEW_CASE);
        let (agent, _tmp) = cursor_agent_with_fake_server(&cases);
        let pool = RpcPool::new(InFlight::new());
        let client = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(
            pool.available_models("test", TAG),
            vec!["gpt-5.6-sol".to_string(), "composer-2.5".to_string()]
        );
        drop(client);
        pool.evict(&agent.name, TAG);
    }

    /// `/session` 的候选与保存校验读 `available_models`;它必须与每轮收敛校验的 `model`
    /// 轴同源,legacy `availableModels` 多出来的值不能进候选。
    #[test]
    fn cursor_available_models_follow_the_model_axis_over_legacy_models() {
        let _guard = process_test_guard();
        let legacy_tail = r#"{"modelId":"composer-2.5","name":"Composer"}]"#;
        let cases = ACP_NEW_CASE.replace(
            legacy_tail,
            r#"{"modelId":"composer-2.5","name":"Composer"},{"modelId":"gemini-4-carbon","name":"Gemini 4 Carbon"}]"#,
        );
        let (agent, _tmp) = cursor_agent_with_fake_server(&cases);
        let pool = RpcPool::new(InFlight::new());
        let client = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(
            pool.available_models("test", TAG),
            vec!["gpt-5.6-sol".to_string(), "composer-2.5".to_string()]
        );
        drop(client);
        pool.evict(&agent.name, TAG);
    }

    /// Cursor mode 是 per-session 标准 ACP 状态：同一进程/同一 session 上切换；相同目标不重发；
    /// live catalog 已知时拒绝目录外 id，且不得悄悄用旧模式继续本轮。
    #[test]
    fn cursor_mode_change_uses_live_catalog_on_the_same_session() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_MODE_CASE}");
        let (agent, tmp) = cursor_agent_with_fake_server(&cases);
        let pool = RpcPool::new(InFlight::new());
        let plan: CursorSessionMode = "plan".parse().unwrap();

        let first = pool
            .checkout(&RpcChat {
                cursor_mode: Some(&plan),
                codex_policy: Default::default(),
                register_platform_tools: false,
                ..chat(&agent)
            })
            .unwrap();
        let pgid = first.pgid();
        assert_eq!(first.checkout_mode(), Checkout::ColdStart);
        let frames = sent(tmp.path(), "session/set_mode");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["params"]["sessionId"], "sess-1");
        assert_eq!(frames[0]["params"]["modeId"], "plan");
        assert_eq!(pool.current_mode("test", TAG).as_deref(), Some("plan"));
        assert_eq!(
            pool.available_modes("test", TAG)
                .into_iter()
                .map(|mode| mode.id)
                .collect::<Vec<_>>(),
            ["agent", "plan", "ask"]
        );

        let same = pool
            .checkout(&RpcChat {
                cursor_mode: Some(&plan),
                codex_policy: Default::default(),
                register_platform_tools: false,
                ..chat(&agent)
            })
            .unwrap();
        assert_eq!(same.checkout_mode(), Checkout::Reused);
        assert_eq!(same.pgid(), pgid);
        assert_eq!(sent(tmp.path(), "session/set_mode").len(), 1);

        let unavailable: CursorSessionMode = "review-v2".parse().unwrap();
        let error = match pool.checkout(&RpcChat {
            cursor_mode: Some(&unavailable),
            codex_policy: Default::default(),
            register_platform_tools: false,
            ..chat(&agent)
        }) {
            Ok(_) => panic!("目录外 mode 必须阻止本轮，不能沿用权限更宽的旧模式"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("本轮未启动"));
        assert_eq!(sent(tmp.path(), "session/set_mode").len(), 1);
        assert_eq!(pool.current_mode("test", TAG).as_deref(), Some("plan"));
    }

    /// server 拒绝 mode 时，偏好仍留给后续重试，但这轮绝不能以旧 mode 发 prompt。
    #[test]
    fn cursor_mode_rejection_fails_checkout_before_prompt() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_MODE_FAILS_CASE}");
        let (agent, tmp) = cursor_agent_with_fake_server(&cases);
        let pool = RpcPool::new(InFlight::new());
        let ask: CursorSessionMode = "ask".parse().unwrap();

        let error = match pool.checkout(&RpcChat {
            cursor_mode: Some(&ask),
            codex_policy: Default::default(),
            register_platform_tools: false,
            ..chat(&agent)
        }) {
            Ok(_) => panic!("set_mode 失败必须让 checkout 失败"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("本轮未启动"));
        assert_eq!(sent(tmp.path(), "session/set_mode").len(), 1);
        assert!(sent(tmp.path(), "session/prompt").is_empty());
        assert_eq!(pool.current_mode("test", TAG).as_deref(), Some("agent"));
    }

    /// 计费/上下文参数未 exact 生效时必须 prompt 前 fail-stop，绝不能沿用旧 Fast/model 跑。
    #[test]
    fn rejected_cursor_parameter_fails_checkout_before_prompt() {
        let _guard = process_test_guard();
        let cases = format!("{ACP_NEW_CASE}{ACP_LOAD_CASE}{ACP_SET_CONFIG_FAILS_CASE}");
        let (agent, tmp) = cursor_agent_with_parameter_seed(&cases, "composer-2.5[fast=true]");
        let pool = RpcPool::new(InFlight::new());

        let error = match pool.checkout(&chat(&agent)) {
            Ok(_) => panic!("参数未确认生效时不得返回可 prompt 的 client"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("本轮未启动"));
        assert_eq!(sent(tmp.path(), "session/set_config_option").len(), 1);
        assert!(sent(tmp.path(), "session/prompt").is_empty());
        assert_eq!(
            pool.current_model("test", TAG).as_deref(),
            Some("gpt-5.6-sol")
        );
    }

    /// `/model` 面板的候选来自会话报的 `availableModels`(ACP 命名空间)。池里没这条会话时给空,
    /// 候选退回配置桶。
    #[test]
    fn available_models_come_from_the_live_session_and_default_to_empty() {
        let _guard = process_test_guard();
        let (agent, _tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let pool = RpcPool::new(InFlight::new());
        assert!(
            pool.available_models("test", TAG).is_empty(),
            "还没有会话就没有候选"
        );

        let client = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(
            pool.available_models("test", TAG),
            vec!["gpt-5.6-sol".to_string(), "composer-2.5".to_string()]
        );
        drop(client);
    }

    /// codex 反过来:模型每轮经 `turn/start` 下发,换模型**不该**动进程(动了就是白白重连),
    /// 也一帧 Cursor `session/set_config_option` 都不该发。
    #[test]
    fn codex_model_change_keeps_the_same_process() {
        let _guard = process_test_guard();
        let (agent, tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let first = pool
            .checkout(&RpcChat {
                model: Some("gpt-5.6-sol"),
                ..chat(&agent)
            })
            .unwrap();
        let switched = pool
            .checkout(&RpcChat {
                model: Some("gpt-5.5"),
                ..chat(&agent)
            })
            .unwrap();
        assert_eq!(switched.checkout_mode(), Checkout::Reused);
        assert_eq!(switched.pgid(), first.pgid());
        assert!(sent(tmp.path(), "session/set_config_option").is_empty());
    }

    // ---- D2:cursor 的两条 resume 通道互不串门 ----

    /// 方向一:**exec 那路留下的 chats id,ACP 绝不能拿去 `session/load`**。
    /// 串了不是丢上下文,是拿一个 ACP 根本不认的 id 去接会话(-32602,或更糟:接到别的东西)。
    #[test]
    fn acp_never_loads_a_session_id_left_by_the_exec_channel() {
        let _guard = process_test_guard();
        let (agent, tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let pool = RpcPool::new(InFlight::new());
        // `cursor -p` 那条通道的记录(键=裸 cli 名)。
        resume_store::write(
            &agent,
            SESSION,
            "cursor",
            "chats-uuid-from-exec",
            None,
            None,
        );

        let client = pool.checkout(&chat(&agent)).unwrap();

        assert_eq!(
            client.checkout_mode(),
            Checkout::ColdStart,
            "读不到本通道的记录 = 干净冷启,不能拿 exec 的 id 去 load"
        );
        assert!(
            sent(tmp.path(), "session/load").is_empty(),
            "一条 session/load 都不该发 —— 那个 id 不属于这个命名空间"
        );
        assert_eq!(sent(tmp.path(), "session/new").len(), 1);
        assert_eq!(
            resume_store::read_id(&agent, SESSION, "cursor").as_deref(),
            Some("chats-uuid-from-exec"),
            "prompt 提交前不得用空 ACP 会话覆盖 exec 记录"
        );
        commit_cursor_prompt(&agent, &client);
        assert_eq!(
            resume_store::read_id(&agent, SESSION, "cursor-acp").as_deref(),
            Some("sess-1"),
            "prompt 已提交的新会话按本通道落记录"
        );
    }

    /// 方向二:**ACP 落的 sessionId,exec 那路绝不能读到**(读到就会拿它去 `-p --resume`)。
    #[test]
    fn the_exec_channel_never_reads_an_acp_session_id() {
        let _guard = process_test_guard();
        let (agent, _tmp) = cursor_agent_with_fake_server(&acp_open_cases());
        let pool = RpcPool::new(InFlight::new());
        let client = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(client.session_id(), "sess-1");
        commit_cursor_prompt(&agent, &client);

        assert!(
            resume_store::read_id(&agent, SESSION, "cursor").is_none(),
            "exec 通道读到 ACP 的 sessionId 就会拿它去 --resume"
        );
        assert_eq!(
            resume_store::read_id(&agent, SESSION, "cursor-acp").as_deref(),
            Some("sess-1")
        );
    }

    // ---- 信号阶梯:每轮一把新闸 / 拒绝路径不误伤 / evict 自带收敛 ----

    /// **B1**:一次性闸不许跨轮。`ChildKiller::interrupt` 发过一次就永久闭闸(防对已复用的 pid
    /// 误伤),而会话进程跨轮存活 —— 共用 spawn 那一把的话,任何一轮升级过 SIGINT 之后,
    /// 后续每一轮的 SIGINT(乃至 daemon 停机那一发)全都成了空操作,只剩 SIGKILL 兜底。
    ///
    /// 断言用 `was_killed()`:它读的正是那把一次性闸,发得出去才会置位。
    #[test]
    fn each_turn_gets_a_fresh_kill_latch_so_sigint_still_fires_next_turn() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let pool = RpcPool::new(InFlight::new());

        let first = pool.checkout(&chat(&agent)).unwrap();
        let second = pool.checkout(&chat(&agent)).unwrap();
        assert_eq!(second.checkout_mode(), Checkout::Reused, "同一条会话");

        let (first_killer, second_killer) = (first.killer(), second.killer());
        assert_eq!(first_killer.pgid(), second_killer.pgid(), "同一个进程组");
        assert!(
            !first_killer.same_handle(&second_killer),
            "轮键登记按 Arc 身份核对:两轮共用一把身份恒相等,\
             被 detach 的上一轮迟到的 finish 就会误摘下一轮的登记"
        );

        // 轮 1 升级过 SIGINT(闸已置位)。
        first_killer.interrupt();
        assert!(first_killer.was_killed());
        // 轮 2 的打断仍必须发得出去 —— 老实现里这里恒 false。
        assert!(!second_killer.was_killed(), "轮 2 的闸不该被轮 1 用掉");
        second_killer.interrupt();
        assert!(
            second_killer.was_killed(),
            "升级过一次 SIGINT 之后,下一轮的打断仍必须能发出 SIGINT"
        );
    }

    /// **B3**:停机屏障立起之后,`checkout` 必须**在碰会话之前**干净失败。
    ///
    /// 老实现只在 `admit` 里挡(那是 spawn 新进程才走的路),热路径 `Reused` 照常把会话交出去,
    /// 于是失败点被推迟到轮级 `InFlight::insert` —— 那条拒绝路径当时会零宽限 SIGKILL 掉这条
    /// 跨轮会话(连 `<turn_aborted>` 都写不出)。
    #[test]
    fn checkout_after_freeze_fails_without_touching_the_live_session() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let inflight = InFlight::new();
        let pool = RpcPool::new(inflight.clone());

        let live = pool.checkout(&chat(&agent)).unwrap();
        let pgid = live.pgid();
        inflight.freeze();

        assert!(
            pool.checkout(&chat(&agent)).is_err(),
            "停机屏障后不该再交出会话"
        );
        assert!(
            group_alive(pgid),
            "拒绝取用绝不能顺手把这条还带着上下文的会话进程杀了"
        );
        drop(live);
    }

    /// **B4**:`evict` 必须**自带收敛**,并且注销 `InFlight` 发生在进程确认退出之后。
    ///
    /// 老实现:`interrupt_one_bounded` 先 SIGINT + 起一条 sleep 后复查注册表的线程,紧接着
    /// `evict` 同步把注册摘掉 → 那条线程醒来查无此条,第二枪永不开;而 evict 到最后一个
    /// `PooledClient` 松手之前,进程既不在池里也不在注册表里,任何 kill 路径都够不着。
    ///
    /// 夹具屏蔽 SIGINT,只有第二枪收得掉它 —— 这样「有没有补第二枪」才验得出来。
    #[test]
    fn evict_converges_a_sigint_immune_session_and_deregisters_only_after() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let script = write_sigint_immune_app_server(tmp.path(), &codex_open_cases());
        let (agent, _tmp) = agent_pointing_at("codex", &script, tmp);
        let inflight = InFlight::new();
        let pool = RpcPool::with_close_grace(inflight.clone(), SIGINT_IMMUNE_CLOSE_GRACE);

        // 故意留着一份 client(模拟 chat 线程正在跑一轮):进程的回收绝不能依赖它松手。
        let held = pool.checkout(&chat(&agent)).unwrap();
        assert!(inflight.contains(&inflight_key("test", TAG)));

        assert!(pool.evict("test", TAG));
        let old_pgid = held.pgid();
        assert!(
            !pool.handoff_target_free("test", TAG).unwrap(),
            "Session 已摘池也不能把仍在宽限内的 closing writer 报成 handoff target 空闲"
        );
        // 判据用 `is_running()`(alive + group_alive)而不是裸 `group_alive`:此刻还没人 `wait`
        // 过这个 child,它会以**僵尸**形态留在进程表里,`killpg(pgid, 0)` 对僵尸照样成功。
        assert!(
            wait_until(Duration::from_secs(6), || !held.is_running()),
            "屏蔽 SIGINT 的会话进程必须在宽限后被 SIGKILL 收敛(老实现的第二枪永不开)"
        );
        // 进程已经退出，但 held 仍持有 Child，leader 尚未 wait、进程组仍可能表现为 zombie。
        // closing fence 在这期间不能放新 writer；松手完成 wait 后 checkout 才能继续。
        drop(held);
        let replacement = pool
            .checkout(&chat(&agent))
            .expect("checkout 应等待旧进程组 Gone 后再建 replacement");
        assert_ne!(replacement.pgid(), old_pgid);
        assert!(
            inflight.contains(&inflight_key("test", TAG)),
            "旧 closing 销账后 replacement 应成为唯一常驻登记"
        );
        drop(replacement);
        assert!(pool.evict("test", TAG));
    }

    /// 屏蔽 SIGINT 的夹具上宽限必然走满,所以这两条用例给池换一个短宽限。它仍要明显长于
    /// 「SIGKILL 之后观察到进程已死」的延迟:`evict_after_freeze_*` 靠「死得不早于宽限」
    /// 区分有没有零宽限 SIGKILL。
    const SIGINT_IMMUNE_CLOSE_GRACE: Duration = Duration::from_millis(200);

    #[test]
    fn evict_after_freeze_still_gives_a_cross_turn_session_sigint_grace() {
        let _guard = process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let script = write_sigint_immune_app_server(tmp.path(), &codex_open_cases());
        let (agent, _tmp) = agent_pointing_at("codex", &script, tmp);
        let inflight = InFlight::new();
        let pool = RpcPool::with_close_grace(inflight.clone(), SIGINT_IMMUNE_CLOSE_GRACE);
        let held = pool.checkout(&chat(&agent)).unwrap();
        inflight.freeze();

        let evicted_at = Instant::now();
        assert!(pool.evict("test", TAG));
        assert!(
            wait_until(Duration::from_secs(6), || !held.is_running()),
            "SIGINT 宽限结束后仍应由 evict 的有界收敛回收"
        );
        // 观察到死亡的时刻只会晚于真正死亡,所以正确实现下这条恒成立;零宽限 SIGKILL 则在
        // evict 返回前后几毫秒就让进程死掉。
        let died_after = evicted_at.elapsed();
        assert!(
            died_after >= SIGINT_IMMUNE_CLOSE_GRACE,
            "closing key 在 freeze 后登记失败也不能触发 OwnedProcess 的零宽限 SIGKILL,\
             实测 {died_after:?} 就没了"
        );
        drop(held);
    }

    /// **C2**:`shared` 的前提要当场校验,不能靠调用方自觉。传一份不是绑定那张的注册表时,
    /// 返回按传入表直建的独立池(fail-safe),而不是把会话默默挂到另一张表上。
    ///
    /// 生产里这条错误的症状是「停机够不着 → 孤儿」;测试里是「一条用例永久污染同进程内其余用例」。
    #[test]
    fn shared_pool_never_binds_sessions_to_a_foreign_registry() {
        let _guard = process_test_guard();
        let (agent, _tmp) = agent_with_fake_server(&codex_open_cases());
        let mine = InFlight::new();

        let pool = shared(&mine);
        let client = pool.checkout(&chat(&agent)).unwrap();
        assert!(
            mine.contains(&inflight_key("test", TAG)),
            "会话必须挂在**我传进去的**那张表上"
        );
        pool.evict("test", TAG);
        drop(client);
    }
}
