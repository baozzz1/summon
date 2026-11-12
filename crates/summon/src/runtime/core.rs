//! Core Port 实现:chat 回路路由门面 + 本机写命令 + 阻塞闸门。
//!
//! 职责分两块:
//! 1. **chat 回路委派**(行为等价):Core 持「app_id → dispatcher 发送端」路由表,
//!    `dispatch_inbound`/`dispatch_card` 把富载 `DispatchMsg` 原样投到对应 dispatcher 通道。
//! 2. **阻塞闸门 + operate-gate**:chat 轮内 agent 用 `summon ask` 提问 → `do_chat_ask` 登记闸门
//!    并投提问卡,返回 `Blocked(GateId)`;control.sock handler 在自己线程 `wait_gate` 等人作答,
//!    人点卡片走 `handle_gate_resolve`/`handle_gate_cancel` 唤醒它。
//!
//! operate-gate 在 Core(spec §2.1 #2):前端绝不自行鉴权。Feishu 卡片动作带 `is_group`/`union_id`
//! 上下文,走 `submit_feishu`;CLI 是 `Actor::Owner`,走 trait `submit`、gate 直接放行。

#[path = "core_attention.rs"]
mod attention;
use crate::runtime::daemon::DispatchMsg;
use attention::AttentionMeta;
pub(crate) use attention::{AttentionRequest, AttentionSource};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use summon_core::core_port::{
    Actor, Command, CommandOutcome, CommandSink, CoreEvent, EventBus, GateOrigin, GateTier,
    GateView, Intent, PendingGateView, QueryApi, SessionView, TeardownMode,
};
use summon_core::ids::{ChatId, GateId};
use summon_core::registry::{Agent, OperateScope, Registry};

/// chat 提问只在两个绝对时间点提醒：第 3 分钟一次、第 55 分钟一次，之后静默等待。
/// **只提醒、不放弃** —— chat 闸门无超时:超时自动放行恰恰是本特性要消灭的失败模式
/// (agent 拿到「无人作答」还是会擅自决定)。逃生阀是「跳过」按钮 / 「⏸ 打断」按钮 / daemon stop。
/// 见 `docs/spec/2026-08-04-chat-blocking-ask-design.md`。
const CHAT_ASK_REMIND_AT: [Duration; 2] =
    [Duration::from_secs(3 * 60), Duration::from_secs(55 * 60)];

fn chat_ask_reminder_due(elapsed: Duration, next_index: usize) -> Option<Duration> {
    CHAT_ASK_REMIND_AT
        .get(next_index)
        .copied()
        .filter(|at| elapsed >= *at)
}

/// RPC 原生提问等待取消信号的最大反应时间。只有带取消令牌的原生 ask 走短轮询；CLI
/// `summon ask` 在两个提醒点之间直接阻塞，不为常规路径制造无意义唤醒。
const CHAT_ASK_CANCEL_POLL: Duration = Duration::from_millis(100);
/// 回调属于不可信输入；即使飞书控件本身有限长，也要在 Core 的事实边界再兜一次，避免伪造
/// callback 把超大答案灌进 gate/协议响应/outbound 记录。
const CHAT_ASK_ANSWER_MAX_CHARS: usize = 4_000;

/// 闸门裁决结果(挂起表 channel 的载荷)。Question → 所答文本(多题时是 JSON 数组);
/// Cancel → 人主动取消 / 卡片投不出去。
#[derive(Clone, Debug)]
pub(crate) enum Resolution {
    Question(String),
    Cancel,
}

#[derive(Debug)]
enum GateAnswerError {
    Closed,
    Invalid(String),
}
impl From<String> for GateAnswerError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}
impl From<&str> for GateAnswerError {
    fn from(message: &str) -> Self {
        Self::Invalid(message.to_owned())
    }
}
impl GateAnswerError {
    fn into_outcome(self) -> CommandOutcome {
        match self {
            Self::Closed => CommandOutcome::GateClosed(summon_i18n::text!("rb90bb44aef").into()),
            Self::Invalid(message) => CommandOutcome::Denied(message),
        }
    }
}

impl Resolution {
    /// 裁决 → 回写给阻塞 CLI 的 `CommandOutcome::Value`(CLI 据此打 stdout 给 agent 读)。
    fn into_outcome(self) -> CommandOutcome {
        match self {
            Resolution::Question(ans) => {
                CommandOutcome::Value(serde_json::json!({ "answer": ans }))
            }
            Resolution::Cancel => CommandOutcome::Denied(summon_i18n::text!("r097cdf13cc").into()),
        }
    }
}

/// 接收阶段互斥，裁决早到时由暂存值替代尚未领取的通道。
enum GateReception {
    Unattached(Receiver<Resolution>),
    Attached,
    Stashed(Resolution),
}

enum GateClaim {
    Stashed(Resolution),
    Attached(Receiver<Resolution>, GateOrigin, bool),
    Missing,
}

/// 挂起闸门保留权威题面与来源，直到唯一等待方领取裁决或结束路径清理。
///
/// `submit` 返回 `Blocked` 与调用方进入 `wait_gate` 之间，人的裁决可能已经到达。
/// 此时必须保留暂存裁决，不能把条目连同尚未领取的接收端删掉，否则会静默丢失答案。
///
/// 暂存不设独立 TTL：control.sock handler / RPC worker 沿既有等待、取消和 deadline 路径
/// 收尾；已到达的裁决优先于迟到 waiter 的取消或 deadline，不能因为领取晚而丢掉人类作答。
struct GatePending {
    attention: AttentionMeta,
    reception: GateReception,
    view: GateView,
    /// ask 的权威题面。回调里 echo 的 `qs` 只供旧卡定格兼容，绝不能拿它校验答案。
    questions: Vec<summon_core::core_port::AskQuestion>,
    /// 飞书发卡成功后回填；回调必须来自这张 exact card，防复制/伪造 callback 串 gate。
    message_id: Option<String>,
    /// Claude PermissionRequest hook 登记的闸门才有:请求原文,定格/作废时据它重画卡片。
    hook: Option<summon_core::core_port::ClaudeHookRequest>,
}

impl GatePending {
    fn claim_waiter(&mut self) -> GateClaim {
        match std::mem::replace(&mut self.reception, GateReception::Attached) {
            GateReception::Unattached(rx) => GateClaim::Attached(
                rx,
                self.view.origin.clone(),
                // 授权卡不催:主 agent 两分钟就被 Claude 收掉，子 agent 的卡本身就在群里。
                self.view.kind != PERMISSION_GATE_KIND,
            ),
            GateReception::Stashed(resolution) => GateClaim::Stashed(resolution),
            GateReception::Attached => GateClaim::Missing,
        }
    }

    fn stash_before_attach(&mut self, resolution: &Resolution) -> bool {
        if matches!(self.reception, GateReception::Unattached(_)) {
            self.reception = GateReception::Stashed(resolution.clone());
            true
        } else {
            false
        }
    }
}

/// 授权卡回调前取的权威上下文:卡片定格要用请求原文重画。
#[derive(Clone)]
pub(crate) struct ClaudePermissionGateContext {
    pub(crate) origin: GateOrigin,
    pub(crate) request: summon_core::core_port::ClaudeHookRequest,
}

/// Claude 工具授权闸门的 `GateView::kind`。答案只有 `allow` / `deny` 两种。
const PERMISSION_GATE_KIND: &str = "permission";

#[derive(Clone)]
pub(crate) struct ChatAskGateContext {
    pub(crate) origin: GateOrigin,
    pub(crate) questions: Vec<summon_core::core_port::AskQuestion>,
}

/// Feishu 卡片动作的鉴权上下文(open_id 在 `Actor::Feishu` 里,这两项随卡片回调另带)。
/// CLI(`Actor::Owner`)用不到它。
pub(crate) struct FeishuGateCtx {
    pub(crate) union_id: String,
    pub(crate) is_group: bool,
    pub(crate) chat_id: String,
    pub(crate) message_id: String,
}

fn normalize_chat_ask_questions(
    questions: &[summon_core::core_port::AskQuestion],
) -> Result<Vec<summon_core::core_port::AskQuestion>, String> {
    if questions.is_empty() {
        return Err(summon_i18n::text!("r63ca0c5ad2").into());
    }
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let prompt = question.question.trim();
            if prompt.is_empty() {
                return Err(summon_i18n::format!("r5fc45d27d1", index + 1));
            }
            let mut options = Vec::with_capacity(question.options.len());
            for option in &question.options {
                let option = option.trim();
                if option.is_empty() {
                    return Err(summon_i18n::format!("r89f48f87df", index + 1));
                }
                if option.chars().count() > CHAT_ASK_ANSWER_MAX_CHARS {
                    return Err(summon_i18n::format!("ra1ea1ba146", index + 1));
                }
                if options.iter().any(|known: &String| known == option) {
                    return Err(summon_i18n::format!("r513ab55d4b", index + 1));
                }
                options.push(option.to_string());
            }
            Ok(summon_core::core_port::AskQuestion {
                question: prompt.to_string(),
                header: question
                    .header
                    .as_deref()
                    .map(str::trim)
                    .filter(|header| !header.is_empty())
                    .map(str::to_string),
                options,
                descriptions: question
                    .descriptions
                    .iter()
                    .map(|description| description.trim().to_string())
                    .collect(),
                allow_other: question.allow_other,
                multi_select: question.multi_select && !question.options.is_empty(),
            })
        })
        .collect()
}

/// 一个 agent 的 dispatcher 在三个键空间下的路由。
/// - `app_id`:飞书适配器按入站事件的 app_id 找 dispatcher(键 = 真实 app_id 或 `stub:<name>`);
/// - `alias`:scheduler 投 `ScheduledFire`、Core 后台线程按 agent 名回投 dispatcher 用;
/// - `delivery_scope`:持久卡片必须由**原 app 身份**更新——Task 改派只改执行责任,不能把 A app 的
///   message_id 交给 B app 去覆盖。
///
/// 三个键空间**同生共死**:一个 agent 上线要三张表一起加,故收进一个 `RwLock` 共一把锁 ——
/// 分三把锁会出现「加到一半被读到」的中间态(按 app_id 找得到、按 alias 找不到)。
/// 读远多于写(每条入站消息一次 lookup;只有 daemon 启动与运行期新增 agent 才写),故用 RwLock。
#[derive(Default)]
struct RouteTable {
    by_app: HashMap<String, Sender<DispatchMsg>>,
    by_alias: HashMap<String, Sender<DispatchMsg>>,
    by_delivery_scope: HashMap<String, Sender<DispatchMsg>>,
}

/// Core:Port 的具体实现持有方。chat 路由表 + 闸门挂起表 + operate-gate 用的 agent 快照。
pub struct Core {
    /// 三键路由表(见 `RouteTable`)。运行期新增 agent 时增量插入,不必重启 daemon。
    routes: RwLock<RouteTable>,
    /// 运行期接新 agent 上线的注册器(daemon 启动时回填)。用 `OnceLock` 而非构造参数:它持
    /// `Weak<Core>`,必须等 Core 进了 Arc 才建得出来 —— 这是打破「Core 持注册器、注册器要 Core」
    /// 这个循环的最小代价。CLI 单测里没有 daemon,取不到就把 `ReloadAgents` 判为 Denied。
    admitter: std::sync::OnceLock<crate::runtime::daemon::SharedAdmitter>,
    /// 本机 agent 快照(启动时载入,运行期 `admit_agent` 增量追加,`refresh_agent` 原地刷新)。
    /// 鉴权与按别名取 agent 不直接信它,先经 `current_gate_agent` 以盘上 agent.yaml 为准。
    gate_agents: Mutex<Vec<Agent>>,
    /// EventBus 订阅者集合。会话/闸门变更后广播给它们。
    subscribers: Mutex<Vec<Sender<CoreEvent>>>,
    /// 阻塞闸门挂起表:`GateId → 挂起项(含 rx)`。agent 发 `summon ask` 时登记;control.sock
    /// handler 调 `wait_gate` 取出 rx 阻塞等作答——挂起表 + channel 让闸门不阻塞 Core 其它命令。
    gates: Mutex<HashMap<GateId, GatePending>>,
    /// 闸门发送端表:`GateId → tx`。人点卡片 resolve 时取出 tx 发 Resolution 唤醒 `wait_gate`。
    /// 与 `gates` 分两表(rx 在 gates、tx 在此)是因 channel 的两端要分别给等待方/唤醒方持有。
    gate_senders: Mutex<HashMap<GateId, Sender<Resolution>>>,
    /// 文件预览服务器(daemon 启动时按配置回填)。`OnceLock`:CLI/单测里没有 daemon,
    /// 取不到 = 预览服务没开,回复里不出文件按钮。
    preview_server: std::sync::OnceLock<Arc<crate::runtime::preview_server::PreviewServer>>,
    /// 本机各 agent 的飞书身份(agent 快照 + 它自己的 FeishuClient + 投递作用域),daemon 启动尾声
    /// 回填,与 outbox drain 共用同一张表,运行期新上线的 agent 由注册器插进来。`/handoff @目标`
    /// 用它认出目标、以目标的身份校验与投递。CLI/单测里没有 daemon,取不到就是「本机没有别的 agent」。
    agent_identities: std::sync::OnceLock<crate::runtime::daemon::DrainAgents>,
}

/// 本机一个 agent 的飞书身份快照。
pub(crate) struct LocalAgentIdentity {
    pub(crate) agent: Agent,
    pub(crate) client: Arc<crate::frontend::client::FeishuClient>,
    /// 它的 dispatcher 写入站 WAL 用的投递作用域。
    pub(crate) delivery_scope: String,
}

impl Core {
    /// 用路由表 + gate agent 快照构造 Core。
    pub fn new(
        routes: HashMap<String, Sender<DispatchMsg>>,
        agent_routes: HashMap<String, Sender<DispatchMsg>>,
        agents: Vec<Agent>,
    ) -> Self {
        let by_delivery_scope = agents
            .iter()
            .filter_map(|agent| {
                agent_routes.get(&agent.name).map(|route| {
                    (
                        crate::runtime::daemon::feishu_delivery_scope(agent),
                        route.clone(),
                    )
                })
            })
            .collect();
        Self {
            routes: RwLock::new(RouteTable {
                by_app: routes,
                by_alias: agent_routes,
                by_delivery_scope,
            }),
            admitter: std::sync::OnceLock::new(),
            gate_agents: Mutex::new(agents),
            subscribers: Mutex::new(Vec::new()),
            gates: Mutex::new(HashMap::new()),
            gate_senders: Mutex::new(HashMap::new()),
            preview_server: std::sync::OnceLock::new(),
            agent_identities: std::sync::OnceLock::new(),
        }
    }

    /// 仅 chat 路由的最小构造(无 agent 快照);用于单测与无 agents 场景。
    #[cfg(test)]
    pub fn for_routes(routes: HashMap<String, Sender<DispatchMsg>>) -> Self {
        Self::new(routes, HashMap::new(), Vec::new())
    }

    /// 查某 app_id 的 dispatcher 发送端(无则该 app 未注册到任何 agent)。
    /// 锁中毒按「查不到」降级:入站消息投不出去好过 panic 掉整个 daemon(三个查询同此约定)。
    pub(crate) fn has_app_route(&self, app_id: &str) -> bool {
        self.route(app_id).is_some()
    }

    fn route(&self, app_id: &str) -> Option<Sender<DispatchMsg>> {
        self.routes.read().ok()?.by_app.get(app_id).cloned()
    }

    /// 查某 agent 别名的 dispatcher 发送端。
    fn route_by_alias(&self, alias: &str) -> Option<Sender<DispatchMsg>> {
        self.routes.read().ok()?.by_alias.get(alias).cloned()
    }

    /// 是否有任何已注册路由(锁中毒时按「没有」算,让入站被 Denied 而不是 panic)。
    fn has_routes(&self) -> bool {
        self.routes.read().is_ok_and(|r| !r.by_app.is_empty())
    }

    /// 运行期把一个新 agent 的 dispatcher 挂进三张路由表 + operate-gate 快照(onboard 新建 agent
    /// 后免重启上线)。三张表在**同一把写锁**内一起插,不留「按 app_id 找得到、按 alias 找不到」的
    /// 中间态。已存在同名 agent → 返回 false 且**不覆盖**:重复注册意味着上游算错了增量集,
    /// 覆盖会把旧 dispatcher 的发送端换掉、让它变成收不到消息的孤儿线程。
    pub fn admit_agent(
        &self,
        agent: &Agent,
        app_id: Option<&str>,
        tx: Sender<DispatchMsg>,
    ) -> bool {
        let Ok(mut routes) = self.routes.write() else {
            return false;
        };
        if routes.by_alias.contains_key(&agent.name) {
            return false;
        }
        if let Some(id) = app_id {
            routes.by_app.insert(id.to_string(), tx.clone());
        }
        routes
            .by_app
            .insert(format!("stub:{}", agent.name), tx.clone());
        routes.by_delivery_scope.insert(
            crate::runtime::daemon::feishu_delivery_scope(agent),
            tx.clone(),
        );
        routes.by_alias.insert(agent.name.clone(), tx);
        drop(routes);
        // gate_agents 是 operate-gate 与 run 解析 agent 的来源:没它,新 agent 的破坏性动作会
        // fail-closed 被拒、run 也找不到 cli/work_dir。锁中毒时前面的路由已插入 —— 这里失败只
        // 意味着鉴权侧看不到它,daemon 下次启动会一致,不回滚(回滚需要跨两把锁的原子性,不值当)。
        if let Ok(mut agents) = self.gate_agents.lock() {
            agents.retain(|a| a.name != agent.name);
            agents.push(agent.clone());
        }
        true
    }

    /// 清理屏障：dispatcher 确认空闲并退出后，三份路由一起注销。
    pub(crate) fn retire_routes(&self, agent: &Agent) -> anyhow::Result<()> {
        if let Some(tx) = self.route_by_alias(&agent.name) {
            anyhow::ensure!(
                self.current_gate_agent(&agent.name).is_some(),
                summon_i18n::format!("r48a59ec7df")
            );
            let (reply, rx) = std::sync::mpsc::sync_channel(0);
            if tx.send(DispatchMsg::Retire(reply)).is_ok() {
                rx.recv_timeout(std::time::Duration::from_secs(10))
                    .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r0102b8a7a6")))?
                    .map_err(anyhow::Error::msg)?;
            }
        }
        let mut routes = self
            .routes
            .write()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r5d249e59fb")))?;
        let app = crate::runtime::daemon::resolve_creds(agent).0;
        if let Some(app) = app {
            routes.by_app.remove(&app);
        }
        routes.by_app.remove(&format!("stub:{}", agent.name));
        routes.by_alias.remove(&agent.name);
        routes
            .by_delivery_scope
            .remove(&crate::runtime::daemon::feishu_delivery_scope(agent));
        Ok(())
    }

    pub(crate) fn retire_agent(&self, agent: &Agent) -> anyhow::Result<()> {
        let admitter = self
            .admitter
            .get()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r68b2ff7886")))?;
        admitter.retire(agent)
    }

    pub(crate) fn finish_agent_cleanup(&self, alias: &str) {
        if let Ok(mut agents) = self.gate_agents.lock() {
            agents.retain(|a| a.name != alias);
        }
        if let Some(admitter) = self.admitter.get() {
            admitter.finish_retirement(alias);
            admitter.refresh_sidecar();
        }
    }

    pub(crate) fn restore_agent(&self, alias: &str) {
        if let Some(admitter) = self.admitter.get() {
            admitter.finish_retirement(alias);
            let _ = admitter.admit_new();
        }
    }

    /// 某 agent 的 agent.yaml 重读过、飞书身份没变:本机快照与投递表里的那份 agent 原地换成新的
    /// (投递用的 client / epoch 不动)。sentinel 召唤名单、`/handoff` 目标、outbox 都读投递表,
    /// 名字、显示名、名单这类改动因此不用重启 daemon。dispatcher 重读后调用。
    pub(crate) fn refresh_agent(&self, fresh: &Agent) {
        if let Ok(mut agents) = self.gate_agents.lock() {
            if let Some(slot) = agents.iter_mut().find(|agent| agent.name == fresh.name) {
                *slot = fresh.clone();
            }
        }
        if let Some(identities) = self.agent_identities.get() {
            if let Ok(mut table) = identities.write() {
                if let Some(entry) = table.get_mut(&fresh.name) {
                    entry.agent = fresh.clone();
                }
            }
        }
    }

    /// 鉴权与按别名取 agent 都以盘上 agent.yaml 为准:名单改了(撤掉一个 operator)当场生效,不用
    /// 重启 daemon。读不出、本代上线时可路由而现在停用了,或飞书身份(名字 / 目录 / app 环境变量名
    /// / scope / `.env` 凭据)已变 → `None`,调用方一律拒绝 —— 与 dispatcher 挡入站的判据一致。
    /// 只读:快照由 dispatcher 重读 agent.yaml 后刷新,判权不写任何共享表。
    pub(crate) fn current_gate_agent(&self, alias: &str) -> Option<Agent> {
        let snapshot = self
            .gate_agents
            .lock()
            .ok()?
            .iter()
            .find(|agent| agent.name == alias)
            .cloned()?;
        let fresh = snapshot.reread()?;
        if !same_feishu_identity(&snapshot, &fresh)
            || (snapshot.is_routable() && !fresh.is_routable())
        {
            return None;
        }
        // 凭据与 dispatcher 用同一份冻结身份比对;没挂身份表(CLI、单测)时只比上面那些字段。
        if let Some(identities) = self.agent_identities.get() {
            let table = identities.read().ok()?;
            if table
                .get(alias)
                .is_some_and(|entry| !entry.still_matches(&fresh))
            {
                return None;
            }
        }
        Some(fresh)
    }

    /// daemon 启动尾声回填预览服务器。只允许一次(OnceLock),重复调用忽略后来者。
    pub fn attach_preview_server(
        &self,
        server: Arc<crate::runtime::preview_server::PreviewServer>,
    ) {
        let _ = self.preview_server.set(server);
    }

    /// 文件预览服务;没配(CLI/单测/未启用)时为 `None`,调用方拿到 `None` 即不出文件按钮。
    pub fn preview_server(&self) -> Option<Arc<crate::runtime::preview_server::PreviewServer>> {
        self.preview_server.get().cloned()
    }

    /// daemon 启动尾声回填各 agent 的飞书身份表。只允许一次(OnceLock),重复调用忽略后来者。
    pub(crate) fn attach_agent_identities(&self, identities: crate::runtime::daemon::DrainAgents) {
        let _ = self.agent_identities.set(identities);
    }

    /// 本机全部 agent 的飞书身份快照。锁中毒按「没有」算:认不出目标的交接会被拒,不 panic。
    pub(crate) fn local_agent_identities(&self) -> Vec<LocalAgentIdentity> {
        self.agent_identities
            .get()
            .and_then(|identities| {
                identities.read().ok().map(|identities| {
                    identities
                        .values()
                        .map(|drain| LocalAgentIdentity {
                            agent: drain.agent.clone(),
                            client: Arc::clone(&drain.client),
                            delivery_scope: drain.delivery_scope().to_string(),
                        })
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    /// 已注册的 agent 别名集合(算「哪些是新增」用)。
    pub fn known_agent_aliases(&self) -> Vec<String> {
        self.routes
            .read()
            .map(|r| r.by_alias.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// daemon 启动尾声回填注册器。只允许一次(OnceLock),重复调用忽略后来者。
    pub fn attach_admitter(&self, admitter: crate::runtime::daemon::SharedAdmitter) {
        let _ = self.admitter.set(admitter);
    }

    /// 扫盘接新 agent 上线(`Intent::ReloadAgents`)。返回人读回执:上线了谁 / 是否已是最新。
    fn reload_agents(&self) -> CommandOutcome {
        let Some(admitter) = self.admitter.get() else {
            return CommandOutcome::Denied(summon_i18n::text!("rf1318f7b38").into());
        };
        match admitter.admit_new() {
            Ok(report) if report.admitted.is_empty() => CommandOutcome::Value(
                serde_json::json!({"admitted": [], "already_known": report.already_known}),
            ),
            Ok(report) => CommandOutcome::Value(serde_json::json!({
                "admitted": report.admitted,
                "already_known": report.already_known,
                "sidecar_restarted": report.sidecar_restarted,
            })),
            Err(e) => CommandOutcome::Denied(format!("{e:#}")),
        }
    }

    /// 所有已注册 dispatcher 的发送端(停机时统一投 `Shutdown`)。取自 `by_alias` —— 它是
    /// agent → dispatcher 的权威表,运行期新上线的 agent 自动在内;停机若依赖启动时拍下的
    /// 那份快照,后来上线的 dispatcher 就会漏收停机信号。
    pub fn all_dispatcher_senders(&self) -> Vec<Sender<DispatchMsg>> {
        self.routes
            .read()
            .map(|r| r.by_alias.values().cloned().collect())
            .unwrap_or_default()
    }

    /// 把一条富载入站消息委派给对应 dispatcher(承载 mentions/resources 等全部元数据)。
    /// 等价于接缝前 `handle_conn` 里的 `tx.send(DispatchMsg::Inbound(..))`。返回是否成功投递。
    pub fn dispatch_inbound(&self, app_id: &str, cmd: &Command, msg: DispatchMsg) -> bool {
        if matches!(self.submit_ref(cmd), CommandOutcome::Denied(_)) {
            return false;
        }
        match self.route(app_id) {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        }
    }

    /// 把一条卡片回调委派给对应 dispatcher(会话面板 / idle 卡片等,本步不经 Intent)。
    pub fn dispatch_card(&self, app_id: &str, msg: DispatchMsg) -> bool {
        match self.route(app_id) {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        }
    }

    /// 把 `agents kill` 的 TeardownChat 路由到该 alias 的 dispatcher。`agent_routes` 按 agent.name
    /// 键控(与 feishu 的 app_id 键 `routes` 同源、不同键)。alias 未注册 → Denied 让 CLI 据实回执。
    fn route_teardown_chat(
        &self,
        alias: &str,
        chat_id: &ChatId,
        mode: TeardownMode,
    ) -> CommandOutcome {
        let Some(tx) = self.route_by_alias(alias) else {
            return CommandOutcome::Denied(summon_i18n::format!("r2e3cab7184"));
        };
        let msg = DispatchMsg::TeardownChat {
            chat_id: chat_id.to_string(),
            reset: matches!(mode, TeardownMode::Reset),
        };
        if tx.send(msg).is_ok() {
            CommandOutcome::Done
        } else {
            CommandOutcome::Denied(summon_i18n::format!("r51c4814243"))
        }
    }

    /// 管家(steward)在**别的 agent** 的会话上按下退出:过**目标 agent 自己的 operate 闸**,
    /// 再落到与 `summon agents kill` 同一条 [`DispatchMsg::TeardownChat`] 通道。
    ///
    /// **这是第二道闸,不是唯一一道。** 第一道(管家自己的 operate 名单)在 dispatcher 的集中
    /// 鉴权里已经过了。之所以还要这一道:这条路径能跨身份停会话,只认管家一张名单就等于把所有
    /// agent 的操作权合并到管家身上 —— 每个 agent 自己的闸不该因为「从管家页面点的」而被绕过。
    ///
    /// **`is_group` 硬编码为 `true`**(最严档):`can_operate` 在名单为空时按 `!is_group` 放行,
    /// 而这里的操作对象是别的 agent 的会话,「名单没配」必须读成拒绝而不是默认放行;名单皆空时
    /// 应用所有者的放行只限本群范围的动作,同样够不到这里。
    /// 与 sentinel 建群确认卡对目标群的处置同一纪律。
    ///
    /// **只认目标 agent 的 agent 级名单**:按钮在管家的卡上、不在被停的那个群里,本群 operator
    /// 的授权范围是「在那个群里」,够不到这里。
    pub(crate) fn route_fleet_session_control(
        &self,
        operator_open_id: &str,
        operator_union_id: &str,
        target_alias: &str,
        chat_id: &str,
        reset: bool,
    ) -> CommandOutcome {
        let Some(target) = self.current_gate_agent(target_alias) else {
            return CommandOutcome::Denied(summon_i18n::format!("r4e24179262"));
        };
        let allowed = target.can_operate(
            operator_open_id,
            operator_union_id,
            true,
            OperateScope::Agent,
        );
        if !allowed {
            return CommandOutcome::Denied(summon_i18n::format!("rf07ab1e1bc"));
        }
        self.send_fleet_session_control(target_alias, chat_id, reset)
    }

    /// 把「退出 / 归档这条会话」投给目标 agent 的 dispatcher。管家卡与 Menu Bar 共用,鉴权在调用方。
    fn send_fleet_session_control(
        &self,
        target_alias: &str,
        chat_id: &str,
        archive: bool,
    ) -> CommandOutcome {
        match self.route_by_alias(target_alias) {
            Some(tx) => match tx.send(DispatchMsg::FleetSessionControl {
                chat_id: chat_id.to_string(),
                archive,
            }) {
                Ok(()) => CommandOutcome::Done,
                Err(_) => CommandOutcome::Denied(summon_i18n::text!("r2afe9a9c1d").into()),
            },
            None => CommandOutcome::Denied(summon_i18n::text!("r2afe9a9c1d").into()),
        }
    }

    /// 本机 CLI 改某 agent 的 model:fail-closed 校验(新 model + 现 effort 在该 cli 合法)后
    /// 写 agent.yaml。与飞书卡片路径(`handle_card_set_model`)同一校验/写入;写完唤醒 dispatcher,
    /// 在线 chat 线程两轮之间跟上新值(claude 会话注入 `/model` 就地切)。
    /// alias 未注册 / 校验失败 / 写盘失败 → Denied。
    fn set_agent_model(&self, alias: &str, model: &str) -> CommandOutcome {
        let Some(mut agent) = self.current_gate_agent(alias) else {
            return CommandOutcome::Denied(summon_i18n::format!("r2a70421b78"));
        };
        if let Err(e) = summon_core::model_effort::check_model_id(model) {
            return CommandOutcome::Denied(e.to_string());
        }
        // 本机命令改的是 bot 默认,按默认层校验。
        let default = crate::runtime::chat_model::effective(&agent, None);
        if let Err(e) =
            summon_core::model_effort::validate(agent.cli_kind(), Some(model), default.effort())
        {
            return CommandOutcome::Denied(summon_i18n::format!("r37a9fdb90f"));
        }
        match crate::setup::bind::set_model(&agent.manifest_path(), model) {
            // 同步快照(否则 list_sessions 读旧值、5s 轮询把乐观 UI 刷回);dispatcher 看到
            // agent.yaml 变了,通知在线 chat 线程在两轮之间重读。
            Ok(()) => {
                agent.model = Some(model.to_string());
                self.refresh_agent(&agent);
                // 唤醒失败只是晚一点跟上:dispatcher 下一个事件或空闲扫描时照样看到 mtime 变了。
                if let Some(tx) = self.route_by_alias(alias) {
                    let _ = tx.send(DispatchMsg::AgentYamlWritten);
                }
                CommandOutcome::Done
            }
            Err(e) => CommandOutcome::Denied(summon_i18n::format!("r3cc2beca3d")),
        }
    }

    /// 本机 CLI 改某 agent 的 effort:同 `set_agent_model`,校验「现 model + 新 effort」合法后写盘。
    fn set_agent_effort(&self, alias: &str, effort: &str) -> CommandOutcome {
        let Some(mut agent) = self.current_gate_agent(alias) else {
            return CommandOutcome::Denied(summon_i18n::format!("r2a70421b78"));
        };
        let default = crate::runtime::chat_model::effective(&agent, None);
        if let Err(e) =
            summon_core::model_effort::validate(agent.cli_kind(), default.model(), Some(effort))
        {
            return CommandOutcome::Denied(summon_i18n::format!("r25044a2462"));
        }
        match crate::setup::bind::set_effort(&agent.manifest_path(), effort) {
            Ok(()) => {
                agent.effort = Some(effort.to_string());
                self.refresh_agent(&agent);
                CommandOutcome::Done
            }
            Err(e) => CommandOutcome::Denied(summon_i18n::format!("r3cc2beca3d")),
        }
    }

    /// daemon 自报运行时快照:在跑的轮(`observe::runtime_status` 的活动注册表)+ 自身 pid/版本/uptime。
    /// 纯读,无副作用。序列化失败不可能发生(结构全是 owned 基本类型),兜底也不 panic。
    fn runtime_status(&self) -> CommandOutcome {
        let snapshot = crate::observe::runtime_status::snapshot(
            std::process::id(),
            Some(crate::version::VERSION.to_string()),
            std::time::SystemTime::now(),
        );
        match serde_json::to_value(&snapshot) {
            Ok(v) => CommandOutcome::Value(v),
            Err(e) => CommandOutcome::Denied(summon_i18n::format!("r44578c379a")),
        }
    }

    /// 控制台改完某 agent 的 agent.yaml:与本机 CLI 改 model 同一套收尾 —— 刷新快照、唤醒
    /// dispatcher,在线 chat 线程两轮之间跟上新值。唤醒失败只是晚一点跟上(下一个事件或空闲
    /// 扫描时 dispatcher 照样看到 mtime 变了)。
    pub(crate) fn dashboard_agent_written(&self, alias: &str) {
        if let Some(fresh) = self.current_gate_agent(alias) {
            self.refresh_agent(&fresh);
        }
        if let Some(tx) = self.route_by_alias(alias) {
            let _ = tx.send(DispatchMsg::AgentYamlWritten);
        }
    }

    /// 按 agent 别名把一条消息交给它的 dispatcher。没有路由或 dispatcher 已退出 → false。
    pub(crate) fn dispatch_to(&self, alias: &str, msg: DispatchMsg) -> bool {
        self.route_by_alias(alias)
            .is_some_and(|tx| tx.send(msg).is_ok())
    }

    /// 把定时器触发按 agent 别名路由到该 dispatcher(同 agent_routes 键,供 scheduler 线程用)。
    /// 无此 agent 路由(未注册 / dispatcher 未起)→ false,调用方 log 跳过。
    pub fn dispatch_scheduled(&self, alias: &str, msg: DispatchMsg) -> bool {
        match self.route_by_alias(alias) {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        }
    }

    /// `submit` 的借用版(避免为走 Port 判定而 clone 整个 Command)。chat 入站只判可路由;
    /// 闸门 resolve/取消走 operate-gate;其余是 Owner-only 的本机命令。
    fn submit_ref(&self, cmd: &Command) -> CommandOutcome {
        match &cmd.intent {
            Intent::ChatMessage { .. } => {
                if !self.has_routes() {
                    CommandOutcome::Denied(summon_i18n::text!("r52983631e7").into())
                } else {
                    CommandOutcome::Done
                }
            }
            // 闸门 resolve(人点卡片):过 operate-gate,从挂起表唤醒阻塞的 control.sock handler。
            Intent::QuestionResolve(..) => self.handle_gate_resolve(&cmd.actor, &cmd.intent, None),
            // 闸门取消(人点卡片):过 operate-gate,发 Cancel 唤醒阻塞的 handler。
            Intent::CancelGate(..) => self.handle_gate_cancel(&cmd.actor, &cmd.intent, None),
            // `agents kill` 经 control.sock 来的无头会话拆除:Actor::Owner(本机可信,不过 operate-gate)。
            // 按 alias 找该 agent 的 dispatcher,投 TeardownChat 让它打断在跑轮 + 拆 handle + 归档。
            Intent::TeardownChat {
                alias,
                chat_id,
                mode,
            } => match &cmd.actor {
                Actor::Owner => self.route_teardown_chat(alias, chat_id, *mode),
                _ => CommandOutcome::Denied(summon_i18n::text!("r98cd2c1075").into()),
            },
            // Menu Bar 的 Exit / Archive:本机 Owner,落到管家卡同一条 `FleetSessionControl` 通道。
            Intent::EndChatSession {
                alias,
                chat_id,
                archive,
            } => match &cmd.actor {
                Actor::Owner => {
                    self.send_fleet_session_control(alias, &chat_id.to_string(), *archive)
                }
                _ => CommandOutcome::Denied(summon_i18n::text!("re62c3b4ad0").into()),
            },
            // 开机自启开/关:Owner-only(改本机 launchd LaunchAgent,不过 operate-gate)。GUI 三端经此
            // 统一路由;Core 在 daemon 内执行本机 IO(写 plist + launchctl)。
            Intent::SetAutostart {
                enabled,
                keep_alive,
            } => match &cmd.actor {
                Actor::Owner => {
                    let r = if *enabled {
                        crate::setup::autostart::enable(*keep_alive)
                    } else {
                        crate::setup::autostart::disable()
                    };
                    match r {
                        Ok(()) => CommandOutcome::Done,
                        Err(e) => CommandOutcome::Denied(summon_i18n::format!("rad482b9ea6")),
                    }
                }
                _ => CommandOutcome::Denied(summon_i18n::text!("refdcc74230").into()),
            },
            // 本机 owner 改 model/effort:Owner-only(本机改 agent.yaml,不过 operate-gate)。
            // 写前 fail-closed 校验(与卡片路径同一 `model_effort::validate`)。
            Intent::SetModel { alias, model } => match &cmd.actor {
                Actor::Owner => self.set_agent_model(alias, model),
                _ => CommandOutcome::Denied(summon_i18n::text!("rcf8359ecf3").into()),
            },
            Intent::SetEffort { alias, effort } => match &cmd.actor {
                Actor::Owner => self.set_agent_effort(alias, effort),
                _ => CommandOutcome::Denied(summon_i18n::text!("r138839d7da").into()),
            },
            // chat 轮内 agent 提问:只认 ChatAgent(它自己那一轮),鉴权在 do_chat_ask 里靠
            // turn_id 复验本代活跃表 —— Owner/Feishu 都不该走这条(他们不是"某一轮内的 agent")。
            Intent::ChatAsk {
                turn_id,
                alias,
                conversation_tag,
                questions,
            } => match &cmd.actor {
                Actor::ChatAgent(who)
                    if who.turn_id == *turn_id
                        && who.alias == *alias
                        && who.conversation_tag == *conversation_tag =>
                {
                    self.do_chat_ask(turn_id, alias, conversation_tag, questions)
                }
                Actor::ChatAgent(_) => {
                    CommandOutcome::Denied(summon_i18n::text!("r92cf21959d").into())
                }
                _ => CommandOutcome::Denied(summon_i18n::text!("rd363497e63").into()),
            },
            // Claude 的 PermissionRequest hook:本机进程,路由只认 settings 里写死的 agent 与会话名。
            Intent::ClaudeHookAsk {
                alias,
                session,
                request,
            } => match &cmd.actor {
                Actor::Owner => self.do_claude_hook_ask(alias, session, request),
                _ => CommandOutcome::Denied(summon_i18n::text!("rd8fa934e6e").into()),
            },
            // Claude 的 PostCompact hook:同上,本机进程,路由只认 settings 里写死的 agent 与会话名。
            Intent::ClaudeCompacted {
                alias,
                session,
                trigger,
            } => match &cmd.actor {
                Actor::Owner => self.do_claude_compacted(alias, session, trigger.as_deref()),
                _ => CommandOutcome::Denied(summon_i18n::text!("rb0cf4bf3a4").into()),
            },
            // 扫盘接新建的 agent 上线:Owner-only(本机 onboard 建完调它,替代 daemon restart)。
            Intent::ReloadAgents => match &cmd.actor {
                Actor::Owner => self.reload_agents(),
                _ => CommandOutcome::Denied(summon_i18n::text!("ra6c0cf25c5").into()),
            },
            // `summon dashboard open`:本机码只发给本机 owner;飞书身份只能走 `/dash` 的点击认证。
            Intent::DashboardOpen => match &cmd.actor {
                Actor::Owner => match crate::runtime::dashboard::service() {
                    Ok(dash) => match dash.issue_local() {
                        Ok(url) => CommandOutcome::Value(serde_json::Value::String(url)),
                        Err(error) => CommandOutcome::Denied(format!("{error:#}")),
                    },
                    Err(reason) => CommandOutcome::Denied(summon_i18n::format!("r7b2779bd79")),
                },
                _ => CommandOutcome::Denied(summon_i18n::text!("rdc8535f9ce").into()),
            },
            // 只读运行时快照(`agents status` / Menu Bar):Owner-only,无副作用故不过 operate-gate。
            // 飞书侧永不发这条(它只走 submit_feishu 的鉴权路径),故非 Owner 一律拒。
            Intent::RuntimeStatus => match &cmd.actor {
                Actor::Owner => self.runtime_status(),
                _ => CommandOutcome::Denied(summon_i18n::text!("r83535d2bf6").into()),
            },
            // 复验一轮是否还活在**本代内存**里。主动 CLI 拿它当发送前的最后一道闸:盘上 lease
            // 清理失败时旧凭证仍在,只有这张内存表能说明"那一轮早结束了"。纯查询、无副作用。
            Intent::VerifyActiveTurn {
                alias,
                conversation_tag,
                turn_id,
            } => {
                // 发起者只能是**轮内 agent 自己**(`summon preview` / `react` / `chats` 等)。
                // 它出示的 `ChatAgentIdentity` 里的 alias 必须与 intent 里的一致 —— 否则就是
                // 拿 A 的凭证去问 B 的轮,直接拒。
                let Actor::ChatAgent(identity) = &cmd.actor else {
                    return CommandOutcome::Denied(summon_i18n::text!("rcfc61ad312").into());
                };
                if identity.alias != *alias || identity.turn_id != *turn_id {
                    return CommandOutcome::Denied(summon_i18n::text!("r8d80daa199").into());
                }
                match self.lookup_active_turn(alias, conversation_tag, turn_id) {
                    Some(context) => match serde_json::to_value(&context) {
                        Ok(value) => CommandOutcome::Value(value),
                        Err(error) => CommandOutcome::Denied(summon_i18n::format!("rb4f60701b1")),
                    },
                    // 活跃表缺记录无法区分轮已结束和接管未完成，不能替调用方猜成永久失效。
                    None => CommandOutcome::Denied(
                        ::summon_core::i18n::label(
                            crate::runtime::current_turn_store::INACTIVE_TURN_MESSAGE,
                        )
                        .into(),
                    ),
                }
            }
            Intent::DelegateInstructions {
                target,
                summary,
                content,
                dry_run,
            } => {
                let Actor::ChatAgent(identity) = &cmd.actor else {
                    return CommandOutcome::Denied(summon_i18n::text!("rbcf81ab13c").into());
                };
                let perform = || -> anyhow::Result<serde_json::Value> {
                    let agent = self
                        .agent_by_alias(&identity.alias)
                        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("re6dfe35de7")))?;
                    let context = self
                        .lookup_active_turn(
                            &identity.alias,
                            &identity.conversation_tag,
                            &identity.turn_id,
                        )
                        .ok_or_else(|| {
                            anyhow::anyhow!(::summon_core::i18n::label(
                                crate::runtime::current_turn_store::INACTIVE_TURN_MESSAGE
                            ))
                        })?;
                    crate::runtime::daemon::delegate::run(
                        self,
                        &agent,
                        &context,
                        crate::runtime::daemon::delegate::Request {
                            target,
                            content,
                            summary: summary.as_deref(),
                            dry_run: *dry_run,
                        },
                        || {
                            let current_agent =
                                self.agent_by_alias(&identity.alias).ok_or_else(|| {
                                    anyhow::anyhow!(summon_i18n::format!("re6dfe35de7"))
                                })?;
                            let current = self
                                .lookup_active_turn(
                                    &identity.alias,
                                    &identity.conversation_tag,
                                    &identity.turn_id,
                                )
                                .ok_or_else(|| {
                                    anyhow::anyhow!(::summon_core::i18n::label(
                                        crate::runtime::current_turn_store::INACTIVE_TURN_MESSAGE
                                    ))
                                })?;
                            anyhow::ensure!(
                                current == context,
                                summon_i18n::format!("rbc9dacdeb6")
                            );
                            crate::runtime::daemon::delegate::authorize(&current_agent, &current)
                        },
                    )
                };
                match perform() {
                    Ok(value) => CommandOutcome::Value(value),
                    Err(error) => CommandOutcome::Denied(format!("{error:#}")),
                }
            }
            Intent::CreateFilePreview {
                alias,
                conversation_tag,
                turn_id,
                path,
            } => {
                let Actor::ChatAgent(identity) = &cmd.actor else {
                    return CommandOutcome::Denied(summon_i18n::text!("r5111b3e51f").into());
                };
                if identity.alias != *alias
                    || identity.turn_id != *turn_id
                    || identity.conversation_tag != *conversation_tag
                {
                    return CommandOutcome::Denied(summon_i18n::text!("ra52a2bd6f0").into());
                }
                if self
                    .lookup_active_turn(alias, conversation_tag, turn_id)
                    .is_none()
                {
                    return CommandOutcome::Denied(
                        ::summon_core::i18n::label(
                            crate::runtime::current_turn_store::INACTIVE_TURN_MESSAGE,
                        )
                        .into(),
                    );
                }
                let Some(server) = self.preview_server.get() else {
                    return CommandOutcome::Denied(summon_i18n::text!("r957294060c").into());
                };
                if !std::path::Path::new(path).is_absolute()
                    || !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
                {
                    return CommandOutcome::Denied(summon_i18n::text!("raf1a0b3fe0").into());
                }
                // 预览页头部显示项目相对路径:按发起 agent 的 working_dir 缩。
                let working_dir = self
                    .current_gate_agent(alias)
                    .and_then(|agent| agent.working_dir());
                // 签一次定位 token 只为确认它能预览;打开入口是出站在回复末尾生成的按钮,这里交回路径。
                match server.locator_token(path, working_dir.as_deref()) {
                    Some(_) => CommandOutcome::Value(serde_json::Value::String(path.clone())),
                    None => CommandOutcome::Denied(summon_i18n::text!("r998f9f44f8").into()),
                }
            }
        }
    }

    /// Feishu 卡片动作专用提交:带 `union_id`/`is_group` 让 operate-gate 用 `can_operate` 正确判定。
    pub(crate) fn submit_feishu(&self, cmd: Command, ctx: FeishuGateCtx) -> CommandOutcome {
        match &cmd.intent {
            // 闸门 resolve(人点卡片)带 feishu 鉴权上下文走 operate-gate。
            Intent::QuestionResolve(..) => {
                self.handle_gate_resolve(&cmd.actor, &cmd.intent, Some(ctx))
            }
            // 闸门取消(人点卡片)带 feishu 鉴权上下文走 operate-gate。
            Intent::CancelGate(..) => self.handle_gate_cancel(&cmd.actor, &cmd.intent, Some(ctx)),
            _ => self.submit_ref(&cmd),
        }
    }

    /// operate-gate:`Actor::Owner` 一律放行(localhost/CLI 可信);`Actor::Feishu` 按 tier 查名单。
    /// `Actor::ChatAgent` 一律挡 —— agent 不能自答自己发起的闸门,那样闸门就白设了。
    /// gate 权威 agent = 发起该闸门的 agent,缺则首个 gate agent;无任何 agent → 仅 Owner 可过。
    fn gate_allows(
        &self,
        actor: &Actor,
        tier: GateTier,
        origin: Option<&GateOrigin>,
        ctx: Option<&FeishuGateCtx>,
    ) -> bool {
        let open_id = match actor {
            Actor::Owner => return true,
            Actor::Feishu(id) => id.as_str(),
            Actor::ChatAgent(_) => return false,
        };
        let authority = match self.gate_agents.lock() {
            Ok(agents) => origin
                .and_then(|o| agents.iter().find(|a| a.name == o.alias))
                .or_else(|| agents.first())
                .map(|agent| agent.name.clone()),
            Err(_) => return false, // 锁中毒:fail-closed
        };
        // 名单以盘上为准(撤掉的 operator 当场失效);读不出或身份变了同样挡。
        let Some(agent) = authority.and_then(|alias| self.current_gate_agent(&alias)) else {
            return false; // 无 gate agent:非 Owner 一律挡
        };
        let (union_id, is_group) = ctx
            .map(|c| (c.union_id.as_str(), c.is_group))
            .unwrap_or(("", false));
        // 答一个闸门只影响它所属 chat 上的那一轮,所以本群名单算数 —— 但只在卡就是在那个 chat 里
        // 被点的时候。卡被转发到别的群再点,点击处的本群名单管不到发起闸门的那个群。
        let scope = match (ctx, origin) {
            (Some(c), Some(o)) if c.chat_id == o.chat_id => OperateScope::Chat(&o.chat_id),
            _ => OperateScope::Agent,
        };
        match tier {
            GateTier::Operate => agent.can_operate(open_id, union_id, is_group, scope),
        }
    }

    /// 登记一个闸门:生成 GateId、建 channel、rx 存挂起表(`wait_gate` 等)、tx 存 sender 表
    ///(resolve 唤醒)。返回 gate_id。
    #[cfg(test)]
    fn register_gate(&self, view: GateView) -> GateId {
        let gate_id = GateId::new(format!("gate-{}", uuid::Uuid::new_v4()));
        let (tx, rx) = channel::<Resolution>();
        if let Ok(mut gates) = self.gates.lock() {
            gates.insert(
                gate_id.clone(),
                GatePending {
                    attention: AttentionMeta::default(),
                    reception: GateReception::Unattached(rx),
                    view,
                    questions: Vec::new(),
                    message_id: None,
                    hook: None,
                },
            );
        }
        if let Ok(mut senders) = self.gate_senders.lock() {
            senders.insert(gate_id.clone(), tx);
        }
        gate_id
    }

    /// 同一 chat 的「检查没有 pending ask + 插入新 gate」必须在一把 `gates` 锁里完成；分成
    /// `chat_has_pending_ask`/`register_gate` 两步会让并发子任务各自都看到空，最终发出两张卡。
    fn register_chat_ask_gate(
        &self,
        view: GateView,
        questions: Vec<summon_core::core_port::AskQuestion>,
        hook: Option<summon_core::core_port::ClaudeHookRequest>,
    ) -> Option<GateId> {
        let gate_id = GateId::new(format!("gate-{}", uuid::Uuid::new_v4()));
        let (tx, rx) = channel::<Resolution>();
        {
            let mut gates = self.gates.lock().ok()?;
            if gates.values().any(|pending| {
                pending.view.kind == "ask" && pending.view.origin.chat_id == view.origin.chat_id
            }) {
                return None;
            }
            self.insert_gate(&mut gates, &gate_id, rx, view, questions, hook);
        }
        self.insert_sender(gate_id, tx)
    }

    /// Claude 工具授权闸门。不设「同一会话一张」的约束:并行的子 agent 各自请求,各是一张卡。
    fn register_permission_gate(
        &self,
        view: GateView,
        request: summon_core::core_port::ClaudeHookRequest,
    ) -> Option<GateId> {
        let gate_id = GateId::new(format!("gate-{}", uuid::Uuid::new_v4()));
        let (tx, rx) = channel::<Resolution>();
        {
            let mut gates = self.gates.lock().ok()?;
            self.insert_gate(&mut gates, &gate_id, rx, view, Vec::new(), Some(request));
        }
        self.insert_sender(gate_id, tx)
    }

    /// hook 登记的闸门同时占住抓屏中介(见 `claude_hook::claim_screen`),落幕时在 `drop_gate` /
    /// `deliver_resolution` 释放。
    fn insert_gate(
        &self,
        gates: &mut HashMap<GateId, GatePending>,
        gate_id: &GateId,
        rx: Receiver<Resolution>,
        view: GateView,
        questions: Vec<summon_core::core_port::AskQuestion>,
        hook: Option<summon_core::core_port::ClaudeHookRequest>,
    ) {
        if let Some(request) = &hook {
            crate::runtime::claude_hook::claim_screen(
                gate_id.as_str(),
                &view.origin.chat_id,
                request.agent_id.is_none(),
            );
        }
        gates.insert(
            gate_id.clone(),
            GatePending {
                attention: AttentionMeta::for_hook(hook.as_ref()),
                reception: GateReception::Unattached(rx),
                view,
                questions,
                message_id: None,
                hook,
            },
        );
    }

    fn insert_sender(&self, gate_id: GateId, tx: Sender<Resolution>) -> Option<GateId> {
        match self.gate_senders.lock() {
            Ok(mut senders) => {
                senders.insert(gate_id.clone(), tx);
                Some(gate_id)
            }
            Err(_) => {
                self.drop_gate(&gate_id);
                None
            }
        }
    }

    /// 发卡成功后把 exact message_id 绑定到 gate。绑定失败意味着 gate 已结束；调用方无需恢复。
    pub(crate) fn bind_gate_message(&self, gate_id: &GateId, message_id: &str) -> bool {
        let Ok(mut gates) = self.gates.lock() else {
            return false;
        };
        let Some(pending) = gates
            .get_mut(gate_id)
            .filter(|p| p.view.kind == "ask" || p.view.kind == PERMISSION_GATE_KIND)
        else {
            return false;
        };
        pending.message_id = Some(message_id.to_string());
        true
    }

    fn is_permission_gate(&self, gate_id: &GateId) -> bool {
        self.gates.lock().ok().is_some_and(|gates| {
            gates
                .get(gate_id)
                .is_some_and(|p| p.view.kind == PERMISSION_GATE_KIND)
        })
    }

    /// 授权卡回调在 resolve 前取请求原文与来源;闸门一旦结束就返回 None。
    pub(crate) fn claude_permission_gate_context(
        &self,
        gate_id: &GateId,
    ) -> Option<ClaudePermissionGateContext> {
        let gates = self.gates.lock().ok()?;
        let pending = gates
            .get(gate_id)
            .filter(|p| p.view.kind == PERMISSION_GATE_KIND)?;
        Some(ClaudePermissionGateContext {
            origin: pending.view.origin.clone(),
            request: pending.hook.clone()?,
        })
    }

    /// 卡片回调在 resolve 前取权威题面/来源；闸门一旦结束就返回 None，旧卡自然失效。
    pub(crate) fn chat_ask_gate_context(&self, gate_id: &GateId) -> Option<ChatAskGateContext> {
        let gates = self.gates.lock().ok()?;
        let pending = gates.get(gate_id).filter(|p| p.view.kind == "ask")?;
        Some(ChatAskGateContext {
            origin: pending.view.origin.clone(),
            questions: pending.questions.clone(),
        })
    }

    /// 往提问所在会话补一张提醒卡片(@ 提问者)。best-effort:投不出去只 log,绝不影响还在等的那轮。
    fn send_chat_ask_reminder(&self, origin: &GateOrigin, gate_id: &GateId, waited: Duration) {
        let GateOrigin {
            alias,
            chat_id,
            chat_type,
            requester_open_id,
        } = origin;
        let Some(route) = self.route_by_alias(alias) else {
            crate::observe::log::log(&summon_i18n::format!("r171af18ef7"));
            return;
        };
        let _ = route.send(DispatchMsg::ChatAskRemind {
            gate_id: gate_id.as_str().to_string(),
            chat_id: chat_id.clone(),
            chat_type: chat_type.clone(),
            requester_open_id: requester_open_id.clone(),
            waited_secs: waited.as_secs(),
        });
    }

    /// 测试同步点:这个闸门的 rx 是否**已被 [`Self::wait_gate`] 取走**(= 等待方真的在等了)。
    ///
    /// `#[cfg(test)]`:生产没人需要问这个问题。它是给「要断言 **已 attach** 那条分支」的测试用的
    /// 确定性同步点 —— `list_gates()` 只证明闸门**已登记**,拿它当「waiter 在等了」的判据是在赌
    /// 调度,会让测试悄悄测到暂存分支去。裁决抢在 attach 之前到达本身已经不再丢答案
    /// (`deliver_resolution` 会暂存,见 `GatePending`),但「谁在测哪条分支」仍要说得清。
    #[cfg(test)]
    pub(crate) fn gate_waiter_attached(&self, gate_id: &GateId) -> bool {
        self.gates
            .lock()
            .ok()
            .and_then(|gates| {
                gates
                    .get(gate_id)
                    .map(|pending| matches!(pending.reception, GateReception::Attached))
            })
            .unwrap_or(false)
    }

    /// 阻塞等某闸门 resolve(control.sock handler 在自己线程调用)。拿到作答回 `Value`。
    /// 不阻塞 Core 其它命令:仅本调用方线程阻塞;等待期间 entry 仍在表(resolve 能找权威 agent)。
    ///
    /// **不超时**:对话是交互场景,人迟早会回;超时自动放行恰恰是本特性要消灭的失败模式
    /// (agent 拿到「无人作答」还是会擅自决定)。改为第 3/55 分钟各发一张提醒卡片后
    /// 继续等。逃生阀不是超时,而是「跳过」按钮 / 「⏸ 打断」按钮 / daemon stop ——
    /// 三者都能把 agent 从阻塞里放出来。
    pub(crate) fn wait_gate(&self, gate_id: &GateId) -> CommandOutcome {
        self.wait_gate_inner(gate_id, None, None, None)
            .expect(summon_i18n::text!("r1ece754fab"))
    }

    /// RPC 原生提问的等待形态。turn 被软打断/daemon 停机，或 app-server 自行退出时，worker
    /// 线程本身仍活在 daemon 进程里，不能像 `summon ask` 子进程那样靠 SIGINT 进程组自然退出；
    /// 取消位与可选 session liveness 因此同时负责唤醒等待并清掉闸门两张内存表。
    pub(crate) fn wait_gate_cancellable(
        &self,
        gate_id: &GateId,
        cancelled: &AtomicBool,
        session_alive: Option<&AtomicBool>,
    ) -> CommandOutcome {
        self.wait_gate_inner(gate_id, Some(cancelled), session_alive, None)
            .expect(summon_i18n::text!("r1ece754fab"))
    }

    /// non-blocking 原生提问的客户端等待策略。`None` 只表示 deadline 到期；用户取消/turn
    /// 打断仍是 `Some(Denied)`，调用方据此区分“自动回空继续”与“这一轮已经结束”。
    pub(crate) fn wait_gate_cancellable_for(
        &self,
        gate_id: &GateId,
        cancelled: &AtomicBool,
        session_alive: Option<&AtomicBool>,
        timeout: Duration,
    ) -> Option<CommandOutcome> {
        self.wait_gate_inner(gate_id, Some(cancelled), session_alive, Some(timeout))
    }

    fn wait_gate_inner(
        &self,
        gate_id: &GateId,
        cancelled: Option<&AtomicBool>,
        session_alive: Option<&AtomicBool>,
        timeout: Option<Duration>,
    ) -> Option<CommandOutcome> {
        // 三选一,全收在同一把 `gates` 锁里(与 `deliver_resolution` 严格互斥,不留中间态):
        // ① 条目里已有暂存裁决(resolve/cancel 抢在本次 attach 之前到达)→ 就地领走,不用等;
        // ② 否则取走 rx(只此一个等待者)挂上去等,entry 留在表里(origin 信息 resolve 时还要用);
        // ③ 条目不在 / rx 已被别人取走 → 诚实拒绝。
        let claim = match self.gates.lock() {
            Ok(mut gates) => gates
                .get_mut(gate_id)
                .map(GatePending::claim_waiter)
                .unwrap_or(GateClaim::Missing),
            Err(_) => GateClaim::Missing,
        };
        let (rx, origin, remind) = match claim {
            GateClaim::Stashed(res) => {
                // 领到暂存 = 闸门就此落幕,双表清干净(tx 早在 deliver 时就被摘走了)。
                self.drop_gate(gate_id);
                return Some(res.into_outcome());
            }
            GateClaim::Missing => {
                return Some(CommandOutcome::Denied(
                    summon_i18n::text!("r25ea6c4b6b").into(),
                ))
            }
            GateClaim::Attached(rx, origin, remind) => (rx, origin, remind),
        };
        let mut next_reminder = 0usize;
        let started = Instant::now();
        loop {
            // 每次决策前先交付已经到达的人类裁决：既避免「裁决与打断同到」时丢答案，也避免
            // recv_timeout 恰在提醒点返回后、答案随即到达所造成的一张过时 @ 提醒。
            match rx.try_recv() {
                Ok(res) => {
                    self.drop_gate(gate_id);
                    return Some(res.into_outcome());
                }
                Err(TryRecvError::Disconnected) => {
                    self.invalidate_chat_ask_card(
                        gate_id,
                        crate::runtime::daemon::ChatAskEndReason::TurnEnded,
                    );
                    self.drop_gate(gate_id);
                    return Some(CommandOutcome::Denied(
                        summon_i18n::text!("r9a24a35833").into(),
                    ));
                }
                Err(TryRecvError::Empty) => {}
            }
            let externally_cancelled = cancelled.is_some_and(|flag| flag.load(Ordering::Acquire));
            let session_ended = session_alive.is_some_and(|flag| !flag.load(Ordering::Acquire));
            if externally_cancelled || session_ended {
                if let Some(resolution) = self.resolution_won_gate_end_race(gate_id, &rx) {
                    self.drop_gate(gate_id);
                    return Some(resolution.into_outcome());
                }
                self.invalidate_chat_ask_card(
                    gate_id,
                    crate::runtime::daemon::ChatAskEndReason::TurnEnded,
                );
                self.drop_gate(gate_id);
                let why = if session_ended && !externally_cancelled {
                    summon_i18n::text!("r9699b415d6")
                } else {
                    summon_i18n::text!("rad87b751f3")
                };
                return Some(CommandOutcome::Denied(why.into()));
            }
            let elapsed = started.elapsed();
            if let Some(timeout) = timeout {
                let remaining = timeout.saturating_sub(elapsed);
                if remaining.is_zero() {
                    if let Some(resolution) = self.resolution_won_gate_end_race(gate_id, &rx) {
                        self.drop_gate(gate_id);
                        return Some(resolution.into_outcome());
                    }
                    self.invalidate_chat_ask_card(
                        gate_id,
                        crate::runtime::daemon::ChatAskEndReason::TimedOut,
                    );
                    self.drop_gate(gate_id);
                    return None;
                }
            }
            if let Some(waited) = chat_ask_reminder_due(elapsed, next_reminder) {
                next_reminder += 1;
                if remind {
                    self.send_chat_ask_reminder(&origin, gate_id, waited);
                }
                continue;
            }

            // 无取消、无 deadline，且两个提醒点都已过：直接永久阻塞等答案，不做小时级空醒。
            // 这正是 owner 要“只在 3/55 分钟提醒”的 CPU 语义，不把“停止提醒”实现成轮询。
            let until_reminder = CHAT_ASK_REMIND_AT
                .get(next_reminder)
                .map(|at| at.saturating_sub(elapsed));
            let timeout_remaining = timeout.map(|limit| limit.saturating_sub(elapsed));
            let wait = match (cancelled.is_some(), until_reminder, timeout_remaining) {
                (true, reminder, deadline) => Some(
                    reminder
                        .into_iter()
                        .chain(deadline)
                        .fold(CHAT_ASK_CANCEL_POLL, Duration::min),
                ),
                (false, reminder, deadline) => reminder.into_iter().chain(deadline).min(),
            };
            let received = match wait {
                Some(wait) => rx.recv_timeout(wait),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(res) => {
                    self.drop_gate(gate_id);
                    return Some(res.into_outcome());
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    // 唯一发送端没了 = 闸门被清(daemon 停机路径)。不再等,让命令返回。
                    self.invalidate_chat_ask_card(
                        gate_id,
                        crate::runtime::daemon::ChatAskEndReason::TurnEnded,
                    );
                    self.drop_gate(gate_id);
                    return Some(CommandOutcome::Denied(
                        summon_i18n::text!("r9a24a35833").into(),
                    ));
                }
            }
        }
    }

    /// 回答与取消/超时的原子完成权：两边都先从 `gate_senders` remove 同一个 sender。
    /// - 本函数 remove 成功 → 结束方赢，之后任何卡片回调都只能得到“已结束”；
    /// - remove 失败 → resolve 方已先取走 sender，等它把结果送进 rx，不能再把人的回答丢掉。
    fn resolution_won_gate_end_race(
        &self,
        gate_id: &GateId,
        rx: &Receiver<Resolution>,
    ) -> Option<Resolution> {
        let end_side_claimed = match self.gate_senders.lock() {
            Ok(mut senders) => senders.remove(gate_id).is_some(),
            // 锁毒化也要把唯一 sender 摘掉；否则 map 仍持有发送端，下面等断连会永久挂住。
            Err(poisoned) => poisoned.into_inner().remove(gate_id).is_some(),
        };
        if end_side_claimed {
            return None;
        }
        // sender map 里没有且结束方没有摘到，只可能是 resolve/cancel 已取走唯一 sender，或其它
        // 结束路径已把它 drop。前者必然 send/drop，后者会断开 channel；因此这里等到“答案或断连”
        // 是确定性完成协议，不依赖调度恰好落在某个毫秒窗口内。
        rx.recv().ok()
    }

    fn invalidate_chat_ask_card(
        &self,
        gate_id: &GateId,
        reason: crate::runtime::daemon::ChatAskEndReason,
    ) {
        let snapshot = self.gates.lock().ok().and_then(|gates| {
            let pending = gates.get(gate_id)?;
            Some((
                pending.view.kind.clone(),
                pending.view.origin.clone(),
                pending.message_id.clone()?,
                pending.questions.clone(),
                pending.hook.clone(),
            ))
        });
        let Some((kind, origin, message_id, questions, hook)) = snapshot else {
            return;
        };
        let Some(route) = self.route_by_alias(&origin.alias) else {
            return;
        };
        if kind == PERMISSION_GATE_KIND {
            let Some(request) = hook else {
                return;
            };
            let _ = route.send(DispatchMsg::ClaudePermissionVoid {
                chat_id: origin.chat_id,
                chat_type: origin.chat_type,
                message_id,
                requester_open_id: origin.requester_open_id,
                request,
            });
            return;
        }
        if questions.is_empty() {
            return;
        }
        // hook 来的提问只会因为对端断开而结束:终端里答过、Claude 收回提问或 claude 退出。
        let reason = if hook.is_some() {
            crate::runtime::daemon::ChatAskEndReason::ResolvedElsewhere
        } else {
            reason
        };
        let _ = route.send(DispatchMsg::ChatAskInvalidated {
            chat_id: origin.chat_id,
            chat_type: origin.chat_type,
            message_id,
            requester_open_id: origin.requester_open_id,
            questions,
            reason,
        });
    }

    /// 把一条裁决送达闸门(resolve / cancel / 卡片投不出去自撤,三条路共用)。
    ///
    /// 先从 `gate_senders` 摘 tx:摘到 = 本次是这个闸门的**首次**裁决;摘不到 = 闸门不存在或
    /// 已被裁决过 → 诚实拒绝(这同时是防二次 resolve 的那把闸)。摘到之后按「waiter 是否已 attach」
    /// 二选一,**判定与写入在同一把 `gates` 锁内完成**,中间不给 `wait_gate` 插进来的机会:
    /// - 已 attach(rx 已被取走):照旧 `tx.send` 唤醒阻塞的 handler —— 既有行为逐字不变;
    /// - 未 attach(rx 还躺在条目里):把裁决暂存进条目、**条目不删**,由随后的 `wait_gate` 领走。
    ///   删条目正是本次要修的 bug:裁决会连同没人接的 rx 一起消失(见 `GatePending` 注释)。
    fn deliver_resolution(&self, gate_id: &GateId, resolution: Resolution) -> CommandOutcome {
        let tx = match self.gate_senders.lock() {
            Ok(mut s) => s.remove(gate_id),
            Err(_) => None,
        };
        let Some(tx) = tx else {
            return CommandOutcome::GateClosed(summon_i18n::text!("rb90bb44aef").into());
        };
        let mut stashed = false;
        if let Ok(mut gates) = self.gates.lock() {
            if let Some(pending) = gates.get_mut(gate_id) {
                stashed = pending.stash_before_attach(&resolution);
            }
        }
        if stashed {
            return CommandOutcome::Done;
        }
        // waiter 已 attach(或条目已被清):走 channel。发不出去 = 等待方已退出(rx 已死)。
        let sent = tx.send(resolution).is_ok();
        // 清 gates:成功时 `wait_gate` 收到后也会再清一次(幂等),失败时条目已无人可唤醒。
        if let Ok(mut g) = self.gates.lock() {
            g.remove(gate_id);
        }
        crate::runtime::claude_hook::release_screen(gate_id.as_str());
        if sent {
            CommandOutcome::Done
        } else {
            CommandOutcome::Denied(summon_i18n::text!("reb74a93468").into())
        }
    }

    /// 闸门落幕:从挂起表 + sender 表清掉(幂等;锁中毒无害)。
    fn drop_gate(&self, gate_id: &GateId) {
        if let Ok(mut g) = self.gates.lock() {
            g.remove(gate_id);
        }
        if let Ok(mut s) = self.gate_senders.lock() {
            s.remove(gate_id);
        }
        crate::runtime::claude_hook::release_screen(gate_id.as_str());
    }

    /// 人点闸门卡片(提交答案 / 全部跳过):过 operate-gate,从挂起表取 sender 发 Resolution
    /// 唤醒阻塞的 control.sock handler。gate 已不在表(重复点)→ 诚实回执。
    fn handle_gate_resolve(
        &self,
        actor: &Actor,
        intent: &Intent,
        ctx: Option<FeishuGateCtx>,
    ) -> CommandOutcome {
        let (gate_id, choice) = match intent {
            Intent::QuestionResolve(g, choice) => (g, choice),
            _ => return CommandOutcome::Denied(summon_i18n::text!("r29121c68e9").into()),
        };
        // 鉴权档位统一由 `Intent::gate_tier` 给(resolve = operate 档,spec §8),不在此另立判据。
        // 权威 agent = 发起该闸门的那个;Owner 放行;Feishu 查 operate 名单;agent 不能自答。
        let Some(tier) = intent.gate_tier() else {
            return CommandOutcome::Denied(summon_i18n::text!("r29121c68e9").into());
        };
        let origin = self.gate_origin(gate_id);
        if !self.gate_allows(actor, tier, origin.as_ref(), ctx.as_ref()) {
            return CommandOutcome::Denied(summon_i18n::text!("r663cc56d21").into());
        }
        if let Some(ctx) = ctx.as_ref() {
            if let Err(why) = self.validate_chat_ask_card_fence(gate_id, ctx) {
                return why.into_outcome();
            }
        }
        let choice = match self.canonicalize_chat_ask_answers(gate_id, choice) {
            Ok(choice) => choice,
            Err(why) => return why.into_outcome(),
        };
        // 送达统一走 `deliver_resolution`:waiter 已 attach 就发 channel,没 attach 就暂存等它领。
        self.deliver_resolution(gate_id, Resolution::Question(choice))
    }

    /// 只对新式 chat ask 启用 exact-card fence；旧测试/旧闸门没有权威 questions，维持兼容。
    /// 鉴权已经在调用方先完成，因此这里返回“闸门是否存在”不会向未授权操作者泄漏状态。
    fn validate_chat_ask_card_fence(
        &self,
        gate_id: &GateId,
        ctx: &FeishuGateCtx,
    ) -> Result<(), GateAnswerError> {
        let gates = self
            .gates
            .lock()
            .map_err(|_| summon_i18n::text!("r958fe86e66").to_string())?;
        let pending = gates.get(gate_id).ok_or(GateAnswerError::Closed)?;
        if pending.questions.is_empty() && pending.view.kind != PERMISSION_GATE_KIND {
            return Ok(());
        }
        if pending.view.origin.chat_id != ctx.chat_id {
            return Err(summon_i18n::text!("r573b77c36f").into());
        }
        match pending.message_id.as_deref() {
            Some(expected) if expected == ctx.message_id => Ok(()),
            Some(_) => Err(summon_i18n::text!("r008c85d98f").into()),
            None => Err(summon_i18n::text!("r863d68d16c").into()),
        }
    }

    /// 用 gate 内的权威题面校验并规范化答案。按钮 value 里的 `qs` 与下拉 value 都能被客户端
    /// 伪造；选项题（未开放“其他”）只能回原候选，自由题/其他答案仍有统一长度上界。
    fn canonicalize_chat_ask_answers(
        &self,
        gate_id: &GateId,
        raw: &str,
    ) -> Result<String, GateAnswerError> {
        let gates = self
            .gates
            .lock()
            .map_err(|_| summon_i18n::text!("r958fe86e66").to_string())?;
        let Some(pending) = gates.get(gate_id) else {
            return Err(GateAnswerError::Closed);
        };
        if pending.view.kind == PERMISSION_GATE_KIND {
            // 卡上看不全的请求不给「允许」按钮;回调能伪造,这里再守一次。
            let shown = pending
                .hook
                .as_ref()
                .is_some_and(summon_core::feishu::claude_permission_fully_shown);
            return match raw {
                "allow" if shown => Ok(raw.to_string()),
                "allow" => Err(summon_i18n::text!("r2c307de7e2").into()),
                "deny" => Ok(raw.to_string()),
                _ => Err(summon_i18n::text!("r4551c2573c").into()),
            };
        }
        if pending.questions.is_empty() {
            return Ok(raw.to_string());
        }
        let mut answers: Vec<Option<String>> =
            serde_json::from_str(raw).map_err(|_| summon_i18n::text!("r5966c6e360").to_string())?;
        if answers.len() > pending.questions.len() {
            return Err(summon_i18n::text!("r6abfa64866").into());
        }
        for (index, answer) in answers.iter_mut().enumerate() {
            let Some(value) = answer.take() else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            if value.chars().count() > CHAT_ASK_ANSWER_MAX_CHARS {
                return Err(summon_i18n::format!("r1c17a9a970", index + 1).into());
            }
            let question = &pending.questions[index];
            if !question.options.is_empty()
                && !question.allow_other
                && !question.options.iter().any(|option| option.trim() == value)
            {
                return Err(summon_i18n::format!("rf311bf2a08", index + 1).into());
            }
            *answer = Some(value.to_string());
        }
        serde_json::to_string(&answers)
            .map_err(|_| GateAnswerError::Invalid(summon_i18n::text!("rf526408145").to_string()))
    }

    /// 人取消闸门(plan/question 都可取消):过 operate-gate,从挂起表取 sender 发 Cancel
    /// 唤醒阻塞的 control.sock handler。agent 收到 Cancel 后应停止当前动作、回到待命。
    fn handle_gate_cancel(
        &self,
        actor: &Actor,
        intent: &Intent,
        ctx: Option<FeishuGateCtx>,
    ) -> CommandOutcome {
        let Intent::CancelGate(gate_id) = intent else {
            return CommandOutcome::Denied(summon_i18n::text!("r46d68cfae4").into());
        };
        // 鉴权档位同 resolve(operate 档,spec §8);权威 agent = 发起该闸门的那个。
        let Some(tier) = intent.gate_tier() else {
            return CommandOutcome::Denied(summon_i18n::text!("r46d68cfae4").into());
        };
        let origin = self.gate_origin(gate_id);
        if !self.gate_allows(actor, tier, origin.as_ref(), ctx.as_ref()) {
            return CommandOutcome::Denied(summon_i18n::text!("r336d031a83").into());
        }
        // 授权卡的「到终端处理」也是一次裁决:同样只认发出去的那一张卡。
        if let Some(ctx) = ctx.as_ref().filter(|_| self.is_permission_gate(gate_id)) {
            if let Err(why) = self.validate_chat_ask_card_fence(gate_id, ctx) {
                return why.into_outcome();
            }
        }
        // 与 resolve 对称:waiter 已 attach 就发 Cancel 唤醒,没 attach 就暂存等它领 ——
        // 否则「取消」也会在同一个窗口里丢掉,agent 拿到的是「闸门不存在」而非明确的「已取消」。
        self.deliver_resolution(gate_id, Resolution::Cancel)
    }

    /// chat 轮内 agent 提问(`summon ask`)。建闸门 + 发提问卡片,返回 `Blocked(GateId)`,
    /// 由 CLI 侧 `wait_gate` 阻塞到人作答/跳过。
    ///
    /// 鉴权靠 `turn_id`:它是 daemon 每轮随动态 prompt 下发的一次性代际凭证,CLI 从
    /// `CurrentTurnStore` 读出并原样带回。这里只认「本代 daemon 当前仍活跃的那一条」——
    /// 换代/过期一律 fail-closed,防旧会话拿陈旧身份往当前 chat 投卡片。
    ///
    /// **一轮一问**(spec 不变量 5):同一会话已有未决闸门就直接拒。否则 agent 一次抛三个问题,
    /// 用户收到三张卡、而它只在等第一张 —— 后两张答了也没人收。
    fn do_chat_ask(
        &self,
        turn_id: &str,
        alias: &str,
        conversation_tag: &str,
        questions: &[summon_core::core_port::AskQuestion],
    ) -> CommandOutcome {
        self.do_chat_ask_with_source(
            turn_id,
            alias,
            conversation_tag,
            questions,
            AttentionSource::AgentAsk,
        )
    }

    fn do_chat_ask_with_source(
        &self,
        turn_id: &str,
        alias: &str,
        conversation_tag: &str,
        questions: &[summon_core::core_port::AskQuestion],
        source: AttentionSource,
    ) -> CommandOutcome {
        let questions = match normalize_chat_ask_questions(questions) {
            Ok(questions) => questions,
            Err(why) => return CommandOutcome::Denied(why),
        };
        let Some(ctx) = self.lookup_active_turn(alias, conversation_tag, turn_id) else {
            return CommandOutcome::Denied(summon_i18n::text!("r9d34a7e9d1").into());
        };
        let origin = GateOrigin {
            alias: alias.to_string(),
            chat_id: ctx.chat_id.clone(),
            chat_type: ctx.chat_type.clone(),
            requester_open_id: ctx.requester_open_id.clone(),
        };
        let Some(route) = self.route_by_alias(alias) else {
            return CommandOutcome::Denied(summon_i18n::format!("r1c98b8680f"));
        };
        // 先建闸门再发卡片:反过来的话用户可能在闸门就位前就点了按钮,resolve 找不到 sender。
        // 闸门同时保存权威题面；按钮 value 只承载答案，不能反过来成为校验事实源。
        let view = GateView {
            origin,
            kind: "ask".into(),
            text: questions[0].question.trim().to_string(),
        };
        let Some(gate_id) = self.register_chat_ask_gate(view.clone(), questions.clone(), None)
        else {
            // 措辞必须可操作:干巴巴一句「拒绝」会让 agent 以为提问这条路不通,退回去自己
            // 拍板 —— 那正是本特性要消灭的失败模式,不能由本特性自己制造出来。
            return CommandOutcome::Denied(summon_i18n::text!("ra8d6ac4db0").into());
        };
        self.set_attention_source(&gate_id, source);
        self.publish(CoreEvent::Gate(gate_id.clone(), view));
        if route
            .send(DispatchMsg::ChatAskCard {
                gate_id: gate_id.as_str().to_string(),
                chat_id: ctx.chat_id.clone(),
                chat_type: ctx.chat_type.clone(),
                requester_open_id: ctx.requester_open_id.clone(),
                questions,
            })
            .is_err()
        {
            self.drop_gate(&gate_id);
            return CommandOutcome::Denied(summon_i18n::text!("r657143338e").into());
        }
        CommandOutcome::Blocked(gate_id)
    }

    /// Claude 的 PostCompact hook 报告某个会话压缩完了:校验同授权 hook,通过后交给该 agent 的
    /// dispatcher 去推长度卡,立即返回。证不了就 `Denied`,这张卡不推。
    fn do_claude_compacted(
        &self,
        alias: &str,
        session: &str,
        trigger: Option<&str>,
    ) -> CommandOutcome {
        let Some(agent) = self.agent_by_alias(alias) else {
            return CommandOutcome::Denied(summon_i18n::format!("rf791ffee5a"));
        };
        if agent.cli_kind() != "claude" {
            return CommandOutcome::Denied(summon_i18n::text!("rb05b901b06").into());
        }
        let (chat_id, chat_type) = match claude_session_chat(&agent, session) {
            Ok(chat) => chat,
            Err(why) => return CommandOutcome::Denied(why),
        };
        let Some(route) = self.route_by_alias(alias) else {
            return CommandOutcome::Denied(summon_i18n::format!("r1c98b8680f"));
        };
        // 收到上报的时刻是判边界新鲜的基准;读不出时钟就不推,不拿 0 当基准(那会让任何旧边界都算新)。
        let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
            return CommandOutcome::Denied(summon_i18n::text!("r39b085453d").into());
        };
        if route
            .send(DispatchMsg::ClaudeCompacted {
                chat_id: chat_id.clone(),
                chat_type,
                session: session.to_string(),
                reported_at_ms: now.as_millis() as i64,
            })
            .is_err()
        {
            return CommandOutcome::Denied(summon_i18n::text!("re03bf4e536").into());
        }
        crate::observe::log::log(&summon_i18n::format!(
            "rc1a3d4a860",
            trigger.unwrap_or(summon_i18n::text!("re41134b99e"))
        ));
        CommandOutcome::Done
    }

    /// Claude 的 PermissionRequest hook 交来一次授权请求 / AskUserQuestion:建闸门、往该会话所属的
    /// chat 发卡,返回 `Blocked`,control.sock handler 等人裁决(对端断开即取消)。
    ///
    /// 任何一条证不了就 `Denied`,hook 拿到后不作答,Claude 走自己的终端对话框 —— 这里拒绝的
    /// 代价只是退回旧路径,所以宁拒勿错。见 `docs/spec/2026-10-16-claude-permission-hook-design.md`。
    fn do_claude_hook_ask(
        &self,
        alias: &str,
        session: &str,
        request: &summon_core::core_port::ClaudeHookRequest,
    ) -> CommandOutcome {
        use summon_core::claude_hook::{classify, HookKind};
        let Some(agent) = self.agent_by_alias(alias) else {
            return CommandOutcome::Denied(summon_i18n::format!("rf791ffee5a"));
        };
        if agent.cli_kind() != "claude" {
            return CommandOutcome::Denied(summon_i18n::text!("rbf179e3972").into());
        }
        if !crate::runtime::daemon::resolve_messaging(&agent).claude_permission_hook {
            return CommandOutcome::Denied(summon_i18n::text!("rce66bf76e6").into());
        }
        let (chat_id, chat_type) = match claude_session_chat(&agent, session) {
            Ok(chat) => chat,
            Err(why) => return CommandOutcome::Denied(why),
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default();
        // 只 @ 真人:本轮若是别的 bot 派的活,@ 它会把卡片当成一条新消息投给它。
        let requester_open_id =
            crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
                .peek_turn_actor_by_chat(&chat_id, now_ms)
                .filter(|actor| {
                    !actor.is_peer
                        && actor.origin
                            == crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu
                })
                .map(|actor| actor.open_id)
                .unwrap_or_default();
        let origin = GateOrigin {
            alias: alias.to_string(),
            chat_id: chat_id.clone(),
            chat_type: chat_type.clone(),
            requester_open_id: requester_open_id.clone(),
        };
        let Some(route) = self.route_by_alias(alias) else {
            return CommandOutcome::Denied(summon_i18n::format!("r1c98b8680f"));
        };
        let who = match &request.agent_type {
            _ if request.agent_id.is_none() => summon_i18n::text!("r584edc12fd").to_string(),
            Some(kind) => summon_i18n::format!("r4969f92ed4"),
            None => summon_i18n::text!("r58138d0b95").to_string(),
        };
        match classify(&request.tool_name) {
            HookKind::Unsupported => {
                CommandOutcome::Denied(summon_i18n::format!("r6e59293d41", request.tool_name))
            }
            HookKind::Ask => {
                let questions = match summon_core::claude_hook::ask_questions(&request.tool_input)
                    .ok_or_else(|| summon_i18n::text!("r0f00ac7955").to_string())
                    .and_then(|questions| normalize_chat_ask_questions(&questions))
                {
                    Ok(questions) => questions,
                    Err(why) => return CommandOutcome::Denied(why),
                };
                let view = GateView {
                    origin,
                    kind: "ask".into(),
                    text: questions[0].question.clone(),
                };
                let Some(gate_id) = self.register_chat_ask_gate(
                    view.clone(),
                    questions.clone(),
                    Some(request.clone()),
                ) else {
                    return CommandOutcome::Denied(summon_i18n::text!("re149001e26").into());
                };
                self.publish(CoreEvent::Gate(gate_id.clone(), view));
                if route
                    .send(DispatchMsg::ChatAskCard {
                        gate_id: gate_id.as_str().to_string(),
                        chat_id: chat_id.clone(),
                        chat_type,
                        requester_open_id,
                        questions,
                    })
                    .is_err()
                {
                    self.drop_gate(&gate_id);
                    return CommandOutcome::Denied(summon_i18n::text!("rc04599fdd4").into());
                }
                crate::observe::log::log(&summon_i18n::format!("r3179f32bb7"));
                CommandOutcome::Blocked(gate_id)
            }
            HookKind::Permission => {
                let view = GateView {
                    origin,
                    kind: PERMISSION_GATE_KIND.into(),
                    text: request.tool_name.clone(),
                };
                let Some(gate_id) = self.register_permission_gate(view.clone(), request.clone())
                else {
                    return CommandOutcome::Denied(summon_i18n::text!("r153d0cca6d").into());
                };
                self.publish(CoreEvent::Gate(gate_id.clone(), view));
                if route
                    .send(DispatchMsg::ClaudePermissionCard {
                        gate_id: gate_id.as_str().to_string(),
                        chat_id: chat_id.clone(),
                        chat_type,
                        requester_open_id,
                        request: request.clone(),
                    })
                    .is_err()
                {
                    self.drop_gate(&gate_id);
                    return CommandOutcome::Denied(summon_i18n::text!("r1aa59eff25").into());
                }
                crate::observe::log::log(&summon_i18n::format!("rfa2bab3c99", request.tool_name));
                CommandOutcome::Blocked(gate_id)
            }
        }
    }

    /// 提问卡 / 授权卡发不出去时就地取消闸门:没人能回答的闸门留着只会让等待方白等。
    /// 与人主动取消同一条路(发 `Resolution::Cancel`):`summon ask` 拿到「提问被取消」,
    /// Claude 的 hook 拿到被拒、不作答,落回终端对话框。
    ///
    /// 走与人主动取消同一条 `deliver_resolution`(含暂存分支)。这条路上「裁决早于 waiter attach」
    /// 尤其常见:卡片是在 `Blocked` 返回**之前**投给 dispatcher 的,dispatcher
    /// 完全可能在 handler 进 `wait_gate` 之前就发现卡片发不出去。裁决送不出去(闸门已结束)只是
    /// 回执被丢,不影响调用方,故忽略返回值。
    pub(crate) fn cancel_undelivered_gate(&self, gate_id: &GateId) {
        let _ = self.deliver_resolution(gate_id, Resolution::Cancel);
    }

    /// 查「这一轮还活着且 turn_id 对得上」,拿回本轮上下文(chat_id/chat_type/提问者)。
    /// 权威是**本代 daemon 内存**里的活跃表，不是盘上文件；未获可信接管权或轮已结束均取不到，
    /// 防旧会话拿陈旧凭证往当前 chat 投卡片。
    fn lookup_active_turn(
        &self,
        alias: &str,
        conversation_tag: &str,
        turn_id: &str,
    ) -> Option<crate::runtime::current_turn_store::CurrentTurnContext> {
        let agent = self.agent_by_alias(alias)?;
        crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
            .active_context(alias, conversation_tag, turn_id)
    }

    /// 某闸门的来源:鉴权权威 agent(= 发起提问的那个)与它所属的 chat。闸门已不在表 → None,
    /// `gate_allows` 回落首个 gate agent、只认 agent 级名单。
    fn gate_origin(&self, gate_id: &GateId) -> Option<GateOrigin> {
        self.gates
            .lock()
            .ok()
            .and_then(|g| g.get(gate_id).map(|p| p.view.origin.clone()))
    }

    /// 按别名取 agent 快照(`summon ask` 要它的 `dir` 去读本轮活跃凭证)。未注册 → None。
    fn agent_by_alias(&self, alias: &str) -> Option<Agent> {
        self.current_gate_agent(alias)
    }

    /// 向所有订阅者广播一条 CoreEvent;已死订阅端就地剔除,广播失败绝不中断主链路。
    pub fn publish(&self, ev: CoreEvent) {
        let Ok(mut subs) = self.subscribers.lock() else {
            return;
        };
        subs.retain(|tx| tx.send(ev.clone()).is_ok());
    }
}

impl CommandSink for Core {
    fn submit(&self, cmd: Command) -> CommandOutcome {
        self.submit_ref(&cmd)
    }
}

impl QueryApi for Core {
    /// 会话快照与 CLI 共用 `session::discovery`:tmux + `.meta` + `threads/` 三源合并。
    /// 因此 cursor/codex 无头会话也可见，不再让其它入口退回 tmux-only 的第二套真相。
    fn list_sessions(&self) -> Vec<SessionView> {
        let agents = match self.gate_agents.lock() {
            Ok(a) => a,
            Err(_) => return Vec::new(),
        };
        let mut rows = Vec::new();
        let now = std::time::SystemTime::now();
        for agent in agents.iter() {
            // 候选 model/effort 与飞书面板同源:effort 走 options_for(cli legal 交集),
            // model 再经 model_candidates 换成 backend 自报的目录(配置桶只剩冷启兜底)。
            let msg = crate::runtime::daemon::resolve_messaging(agent);
            let opts = summon_core::model_effort::options_for(&msg, agent.cli_kind());
            let tmux_sessions = crate::session::tmux::list_sessions_with_prefix(&agent.tmux_name());
            for session in crate::session::discovery::discover_sessions(
                agent,
                &tmux_sessions,
                now,
                msg.idle_ttl(),
            ) {
                // 每行显示这个会话所在群的生效值(本群覆盖 > bot 默认)。
                let effective = crate::runtime::chat_model::effective(agent, Some(&session.tag));
                rows.push(SessionView {
                    key: session.session,
                    alias: agent.name.clone(),
                    live: session.liveness.is_active(),
                    cli: agent.cli_kind().to_string(),
                    model: effective.model,
                    effort: effective.effort,
                    models: crate::runtime::daemon::model_candidates(
                        agent,
                        &session.tag,
                        opts.models.clone(),
                    ),
                    efforts: opts.efforts.clone(),
                });
            }
        }
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        rows
    }

    fn list_gates(&self) -> Vec<PendingGateView> {
        let Ok(gates) = self.gates.lock() else {
            return Vec::new();
        };
        let mut rows: Vec<_> = gates
            .iter()
            .map(|(gate_id, pending)| PendingGateView {
                gate_id: gate_id.clone(),
                gate: pending.view.clone(),
            })
            .collect();
        rows.sort_by(|a, b| a.gate_id.as_str().cmp(b.gate_id.as_str()));
        rows
    }
}

impl EventBus for Core {
    fn subscribe(&self) -> Receiver<CoreEvent> {
        let (tx, rx) = channel();
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(tx);
        }
        rx
    }
}

/// claude hook 报上来的会话归哪个 chat:会话名必须是这个 agent 的(settings 是 summon 按 agent
/// 写的,对不上就不是它起的会话),chat 只认该会话 `.meta` 里记的。返回 (chat_id, chat_type)。
fn claude_session_chat(agent: &Agent, session: &str) -> Result<(String, String), String> {
    let base = agent.tmux_name();
    if session != base && !session.starts_with(&format!("{base}-")) {
        return Err(summon_i18n::format!("rcbc6872df2", agent.name));
    }
    crate::runtime::daemon::read_chat_meta(agent, session)
        .and_then(|meta| {
            Some((
                meta.chat_id?,
                meta.chat_type.unwrap_or_else(|| "group".into()),
            ))
        })
        .ok_or_else(|| summon_i18n::format!("r7e82768430"))
}

/// 两份 agent 是不是同一个飞书身份(只比 manifest 层;`.env` 凭据漂移由 dispatcher 的身份闸管)。
fn same_feishu_identity(a: &Agent, b: &Agent) -> bool {
    let feishu = |agent: &Agent| {
        agent.feishu.as_ref().map(|config| {
            (
                config.app_id_env.clone(),
                config.app_secret_env.clone(),
                config.scope.clone(),
            )
        })
    };
    a.name == b.name && a.dir == b.dir && feishu(a) == feishu(b)
}

/// 仅 chat 路由的轻量构造与 gate agents 透传:把启动期建好的 Registry 转成 gate 快照。
pub fn gate_agents_from_registry(reg: &Registry) -> Vec<Agent> {
    reg.agents.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::ids::ChatId;

    fn sample_inbound(chat: &str, text: &str) -> DispatchMsg {
        DispatchMsg::inbound_for_test(chat, text)
    }

    fn chat_cmd(chat: &str, text: &str) -> Command {
        Command {
            actor: Actor::Feishu("ou_sender".into()),
            intent: Intent::ChatMessage {
                chat: ChatId::new(chat),
                text: text.into(),
            },
        }
    }

    /// `VerifyActiveTurn` 的 **actor 接缝**:CLI 发的是 `ChatAgent`,Core 必须认。
    ///
    /// 这条测试是补一次真事故:上一版 CLI 改发 `ChatAgent`,Core 的 match 却仍只接 `Owner`,
    /// 于是 preview / send-image / react / chats / chat-id **五条命令全部被 daemon 拒绝**。
    /// 两侧各自的纯逻辑测试都是绿的 —— 漏的正是"两侧对接"这一格。
    #[test]
    fn verify_active_turn_accepts_the_chat_agent_actor_and_rejects_others() {
        use summon_core::core_port::ChatAgentIdentity;
        let (core, _dir) = core_with(vec![agent_yaml("name: arcadia\n")]);
        let identity = |alias: &str, turn: &str| {
            Actor::ChatAgent(Box::new(ChatAgentIdentity {
                turn_id: turn.into(),
                alias: alias.into(),
                requester_open_id: "ou_requester".into(),
                source_ref: summon_core::MsgRef::default(),
                chat_type: "group".into(),
                conversation_tag: "g123".into(),
            }))
        };
        let intent = || Intent::VerifyActiveTurn {
            alias: "arcadia".into(),
            conversation_tag: "g123".into(),
            turn_id: "turn-1".into(),
        };

        // Owner 不再被接受:这条命令的语义就是"轮内 agent 自证",本机 owner 没有轮。
        let owner = core.submit(Command {
            actor: Actor::Owner,
            intent: intent(),
        });
        assert!(
            matches!(owner, CommandOutcome::Denied(ref m) if m.contains("ChatAgent")),
            "Owner 应被拒并说明只接受 ChatAgent,实际:{owner:?}"
        );

        // 自证 alias 与待验轮次不符 = 拿 A 的凭证问 B 的轮,拒。
        let mismatched = core.submit(Command {
            actor: identity("flare-5", "turn-1"),
            intent: intent(),
        });
        assert!(
            matches!(mismatched, CommandOutcome::Denied(ref m) if m.contains("不一致")),
            "alias 错配应被拒,实际:{mismatched:?}"
        );

        // 自证一致但本代没有活跃记录：仍拒绝，并解释接管期间的重试条件。
        let no_active = core.submit(Command {
            actor: identity("arcadia", "turn-1"),
            intent: intent(),
        });
        assert!(
            matches!(no_active, CommandOutcome::Denied(ref m)
                if m.contains("未激活或已撤销") && m.contains("接管期间") && m.contains("已结束")),
            "无活跃轮应被拒,实际:{no_active:?}"
        );
    }

    #[test]
    fn delegate_core_uses_live_requester_and_refuses_owner_actor() {
        use summon_core::core_port::ChatAgentIdentity;
        let (agent, _tmp) = crate::test_support::agent_dir(
            "codex",
            "feishu:\n  allowed_users: [ou_owner, ou_other]\n",
        );
        let (core, _dir) = core_with(vec![agent.clone()]);
        let ctx = crate::runtime::current_turn_store::CurrentTurnContext {
            turn_id: uuid::Uuid::new_v4().to_string(),
            agent_alias: agent.name.clone(),
            delivery_scope: "default".into(),
            chat_id: "oc_source".into(),
            chat_type: "group".into(),
            conversation_tag: "g-source".into(),
            message_id: "om_source".into(),
            origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
            schedule_output: summon_core::ScheduleOutput::Progress,
            requester_open_id: "ou_other".into(),
            requester_union_id: String::new(),
            requester_is_peer: false,
            requester_is_bot: false,
            thread_id: None,
            root_id: None,
            started_at_ms: 1,
            reattached: false,
        };
        let store =
            crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir());
        let lease = store.activate(&ctx).unwrap();
        let intent = || Intent::DelegateInstructions {
            target: "oc_target".into(),
            summary: None,
            content: "任务".into(),
            dry_run: true,
        };
        assert!(matches!(
            core.submit(Command {
                actor: Actor::Owner,
                intent: intent()
            }),
            CommandOutcome::Denied(_)
        ));
        let actor = || {
            Actor::ChatAgent(Box::new(ChatAgentIdentity {
                turn_id: ctx.turn_id.clone(),
                alias: agent.name.clone(),
                requester_open_id: "ou_owner".into(),
                source_ref: summon_core::MsgRef::default(),
                chat_type: "group".into(),
                conversation_tag: ctx.conversation_tag.clone(),
            }))
        };
        let outcome = core.submit(Command {
            actor: actor(),
            intent: intent(),
        });
        assert!(
            matches!(outcome, CommandOutcome::Denied(ref reason) if reason.contains("只有 owner")),
            "{outcome:?}"
        );
        drop(lease);
        assert!(
            matches!(core.submit(Command { actor: actor(), intent: intent() }), CommandOutcome::Denied(ref reason) if reason.contains("未激活或已撤销"))
        );
    }

    /// **成功路径**:轮真的活着时,必须返回权威 context。
    ///
    /// 单独成一条是因为上一版只测了三个拒绝分支 —— 把 `lookup_active_turn` 改成永远返回
    /// `None`,那三条照样全绿。只验"该拒的拒了"证明不了"该过的能过",而后者才是这条命令
    /// 存在的意义:五条主动 CLI 全靠它放行。
    #[test]
    fn verify_active_turn_returns_the_authoritative_context_for_a_live_turn() {
        use summon_core::core_port::ChatAgentIdentity;
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let alias = agent.name.clone();
        let (core, _dir) = core_with(vec![agent.clone()]);

        // 造一条**真活着**的轮:activate 拿到 lease,持到断言之后。
        let ctx = crate::runtime::current_turn_store::CurrentTurnContext {
            turn_id: "turn-live".into(),
            agent_alias: alias.clone(),
            delivery_scope: "default".into(),
            chat_id: "oc_live".into(),
            chat_type: "group".into(),
            conversation_tag: "g_live".into(),
            message_id: "om_live".into(),
            origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
            schedule_output: summon_core::ScheduleOutput::Progress,
            requester_open_id: "ou_requester".into(),
            requester_union_id: String::new(),
            requester_is_bot: false,
            requester_is_peer: false,
            thread_id: None,
            root_id: None,
            started_at_ms: 1,
            reattached: false,
        };
        let _lease =
            crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
                .activate(&ctx)
                .expect("激活活跃轮");

        let outcome = core.submit(Command {
            actor: Actor::ChatAgent(Box::new(ChatAgentIdentity {
                turn_id: ctx.turn_id.clone(),
                alias: alias.clone(),
                requester_open_id: ctx.requester_open_id.clone(),
                source_ref: summon_core::MsgRef::default(),
                chat_type: ctx.chat_type.clone(),
                conversation_tag: ctx.conversation_tag.clone(),
            })),
            intent: Intent::VerifyActiveTurn {
                alias,
                conversation_tag: ctx.conversation_tag.clone(),
                turn_id: ctx.turn_id.clone(),
            },
        });

        let CommandOutcome::Value(value) = outcome else {
            panic!("活跃轮必须返回权威 context,实际:{outcome:?}");
        };
        let returned: crate::runtime::current_turn_store::CurrentTurnContext =
            serde_json::from_value(value).expect("返回值必须是 CurrentTurnContext");
        // 逐字段等于权威值 —— CLI 后续所有位置都用它,错一个字段就发错地方。
        assert_eq!(returned.turn_id, ctx.turn_id);
        assert_eq!(returned.chat_id, ctx.chat_id);
        assert_eq!(returned.conversation_tag, ctx.conversation_tag);
        assert_eq!(returned.message_id, ctx.message_id);
    }

    /// 陈旧凭证的 `ChatAsk` 必须在**占闸门名额之前**被拒。
    ///
    /// 「一轮一问」是每 chat 一张卡:名额被占住,同会话真正在跑的那一轮就问不出来了。
    /// 凭证复验若挪到 `register_chat_ask_gate` 之后,一个早已结束的轮就能反复占名额 ——
    /// 拿到的仍是 Denied,但闸门已经被它握住。所以判据不是「返回 Denied」,而是
    /// 「拒完之后名额还在」。
    #[test]
    fn a_stale_chat_ask_is_refused_before_it_can_take_the_gate_slot() {
        use summon_core::core_port::{AskQuestion, ChatAgentIdentity};
        let (agent, _agent_tmp) = crate::test_support::agent_dir("claude", "");
        let alias = agent.name.clone();
        // 路由要挂在**这个** agent 上:`lookup_active_turn` 按 alias 找 agent、再按它的
        // state_dir 找活跃轮表,夹具另造一个同名 agent 会指向别的目录。
        let (tx, rx) = channel();
        let core = Core::new(
            HashMap::new(),
            HashMap::from([(alias.clone(), tx)]),
            vec![agent.clone()],
        );
        let ctx = crate::runtime::current_turn_store::CurrentTurnContext {
            turn_id: "turn-ask".into(),
            agent_alias: alias.clone(),
            delivery_scope: "default".into(),
            chat_id: "oc_ask".into(),
            chat_type: "group".into(),
            conversation_tag: "g_ask".into(),
            message_id: "om_ask".into(),
            origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
            schedule_output: summon_core::ScheduleOutput::Progress,
            requester_open_id: "ou_requester".into(),
            requester_union_id: String::new(),
            requester_is_bot: false,
            requester_is_peer: false,
            thread_id: None,
            root_id: None,
            started_at_ms: 1,
            reattached: false,
        };
        let ask = |turn_id: &str| Command {
            actor: Actor::ChatAgent(Box::new(ChatAgentIdentity {
                turn_id: turn_id.to_string(),
                alias: alias.clone(),
                requester_open_id: ctx.requester_open_id.clone(),
                source_ref: summon_core::MsgRef::default(),
                chat_type: ctx.chat_type.clone(),
                conversation_tag: ctx.conversation_tag.clone(),
            })),
            intent: Intent::ChatAsk {
                turn_id: turn_id.to_string(),
                alias: alias.clone(),
                conversation_tag: ctx.conversation_tag.clone(),
                questions: vec![AskQuestion {
                    header: None,
                    question: "继续吗?".into(),
                    options: vec!["是".into(), "否".into()],
                    descriptions: Vec::new(),
                    allow_other: false,
                    multi_select: false,
                }],
            },
        };
        let _lease =
            crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
                .activate(&ctx)
                .expect("激活活跃轮");

        // 陈旧 turn_id:拒,且不得留下痕迹。
        assert!(matches!(
            core.submit(ask("turn-stale")),
            CommandOutcome::Denied(_)
        ));
        assert!(rx.try_recv().is_err(), "陈旧凭证不得发出提问卡");

        // External wire fields cannot select an internal provider identity.
        fn inject_source(value: &mut serde_json::Value, source: &str) {
            match value {
                serde_json::Value::Object(fields) => {
                    if fields.contains_key("questions") {
                        fields.insert("source".into(), source.into());
                    }
                    for value in fields.values_mut() {
                        inject_source(value, source);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        inject_source(value, source);
                    }
                }
                _ => {}
            }
        }
        for forged in ["codex_approval", "codex_question", "claude_question"] {
            let mut wire = serde_json::to_value(ask(&ctx.turn_id)).unwrap();
            inject_source(&mut wire, forged);
            if let Ok(command) = serde_json::from_value::<Command>(wire) {
                let CommandOutcome::Blocked(id) = core.submit(command) else {
                    panic!("valid ordinary question refused");
                };
                assert!(core.bind_gate_message(&id, "om_forged"));
                assert_eq!(
                    core.attention_requests().unwrap()[0].source,
                    AttentionSource::AgentAsk
                );
                core.drop_gate(&id);
                let _ = rx.try_recv();
            }
        }
        let Intent::ChatAsk { questions, .. } = ask(&ctx.turn_id).intent else {
            unreachable!()
        };
        assert!(matches!(
            core.ask_codex_approval("turn-stale", &alias, &ctx.conversation_tag, &questions),
            CommandOutcome::Denied(_)
        ));
        let CommandOutcome::Blocked(id) =
            core.ask_codex_question(&ctx.turn_id, &alias, &ctx.conversation_tag, &questions)
        else {
            panic!("native question refused")
        };
        assert!(core.bind_gate_message(&id, "om_native_q"));
        assert_eq!(
            core.attention_requests().unwrap()[0].source,
            AttentionSource::CodexQuestion
        );
        core.drop_gate(&id);
        let _ = rx.try_recv();
        let CommandOutcome::Blocked(id) =
            core.ask_codex_approval(&ctx.turn_id, &alias, &ctx.conversation_tag, &questions)
        else {
            panic!("native approval refused")
        };
        assert!(core.bind_gate_message(&id, "om_native_a"));
        assert_eq!(
            core.attention_requests().unwrap()[0].source,
            AttentionSource::CodexApproval
        );
        assert!(core.attention_requests().unwrap()[0].text.is_empty());
        core.drop_gate(&id);
        let _ = rx.try_recv();

        // 名额没被占走 —— 本轮自己仍问得出来。这一条才是上面那次拒绝的真实判据。
        assert!(matches!(
            core.submit(ask(&ctx.turn_id)),
            CommandOutcome::Blocked(_)
        ));
        assert!(matches!(rx.try_recv(), Ok(DispatchMsg::ChatAskCard { .. })));
    }

    #[test]
    fn create_file_preview_mints_url_only_for_the_live_turn() {
        use summon_core::core_port::ChatAgentIdentity;
        let (agent, _agent_tmp) = crate::test_support::agent_dir("claude", "");
        let alias = agent.name.clone();
        let (core, _dir) = core_with(vec![agent.clone()]);
        let preview_tmp = tempfile::tempdir().unwrap();
        let file = preview_tmp.path().join("report.md");
        std::fs::write(&file, "# report").unwrap();
        core.attach_preview_server(crate::runtime::preview_server::PreviewServer::for_test(
            preview_tmp.path().join("tokens.json"),
            "http://localhost:7765",
            1024,
        ));
        let ctx = crate::runtime::current_turn_store::CurrentTurnContext {
            turn_id: "turn-preview".into(),
            agent_alias: alias.clone(),
            delivery_scope: "default".into(),
            chat_id: "oc_preview".into(),
            chat_type: "group".into(),
            conversation_tag: "g_preview".into(),
            message_id: "om_preview".into(),
            origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
            schedule_output: summon_core::ScheduleOutput::Progress,
            requester_open_id: "ou_requester".into(),
            requester_union_id: String::new(),
            requester_is_bot: false,
            requester_is_peer: false,
            thread_id: None,
            root_id: None,
            started_at_ms: 1,
            reattached: false,
        };
        let _lease =
            crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
                .activate(&ctx)
                .unwrap();
        let actor = || {
            Actor::ChatAgent(Box::new(ChatAgentIdentity {
                turn_id: ctx.turn_id.clone(),
                alias: alias.clone(),
                requester_open_id: ctx.requester_open_id.clone(),
                source_ref: summon_core::MsgRef::default(),
                chat_type: ctx.chat_type.clone(),
                conversation_tag: ctx.conversation_tag.clone(),
            }))
        };
        let intent = |turn_id: &str| Intent::CreateFilePreview {
            alias: alias.clone(),
            conversation_tag: ctx.conversation_tag.clone(),
            turn_id: turn_id.to_string(),
            path: file.to_string_lossy().into_owned(),
        };

        let outcome = core.submit(Command {
            actor: actor(),
            intent: intent(&ctx.turn_id),
        });
        assert!(
            matches!(outcome, CommandOutcome::Value(serde_json::Value::String(ref path)) if *path == file.to_string_lossy()),
            "活跃轮确认文件可预览、交回路径:{outcome:?}"
        );
        let stale = core.submit(Command {
            actor: actor(),
            intent: intent("stale-turn"),
        });
        assert!(matches!(stale, CommandOutcome::Denied(_)));
    }

    #[test]
    fn runtime_status_is_owner_only_and_reports_busy_turns() {
        let (core, _dir) = core_with(vec![]);
        crate::observe::runtime_status::mark_daemon_started();
        let _busy = crate::observe::runtime_status::enter_turn("core-test-agent", "g1a2b3c4");

        let owner = Command {
            actor: Actor::Owner,
            intent: Intent::RuntimeStatus,
        };
        let CommandOutcome::Value(v) = core.submit(owner) else {
            panic!("Owner 应拿到运行时快照");
        };
        let snap: summon_core::fleet::RuntimeStatus = serde_json::from_value(v).unwrap();
        assert_eq!(snap.pid, std::process::id());
        assert!(snap
            .busy
            .iter()
            .any(|b| b.agent == "core-test-agent" && b.tag == "g1a2b3c4"));

        // 飞书侧永不该拿到运行时内幕(它连 pid 都不该看见)。
        let feishu = Command {
            actor: Actor::Feishu("ou_x".into()),
            intent: Intent::RuntimeStatus,
        };
        assert!(matches!(core.submit(feishu), CommandOutcome::Denied(_)));
    }

    /// 控制台本机码只发给 control.sock 上的本机 owner:飞书身份与轮内 agent 一律拒,拿不到 URL。
    /// 测试进程不装控制台服务,Owner 拿到的是「没起来」的原因而不是一个链接。
    #[test]
    fn intent_dashboard_open_owner_only() {
        use summon_core::core_port::ChatAgentIdentity;
        let (core, _dir) = core_with(vec![]);
        let identity = ChatAgentIdentity {
            turn_id: "t".into(),
            alias: "a".into(),
            requester_open_id: "ou_x".into(),
            source_ref: summon_core::MsgRef::default(),
            chat_type: "group".into(),
            conversation_tag: "g1".into(),
        };
        for actor in [
            Actor::Feishu("ou_x".into()),
            Actor::ChatAgent(Box::new(identity)),
        ] {
            let outcome = core.submit(Command {
                actor: actor.clone(),
                intent: Intent::DashboardOpen,
            });
            let CommandOutcome::Denied(reason) = outcome else {
                panic!("{actor:?} 不该拿到本机码:{outcome:?}");
            };
            assert!(reason.contains("本机 Owner"), "{reason}");
        }
        let outcome = core.submit(Command {
            actor: Actor::Owner,
            intent: Intent::DashboardOpen,
        });
        let CommandOutcome::Denied(reason) = outcome else {
            panic!("测试进程没有控制台服务:{outcome:?}");
        };
        assert!(reason.contains("控制台服务没有启动"), "{reason}");
    }

    /// 指定 gate agents 的 Core(tempdir 只是给依赖它的用例一个可写根)。
    fn core_with(agents: Vec<Agent>) -> (Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Core::new(HashMap::new(), HashMap::new(), agents), dir)
    }

    /// 带一条 agent dispatcher 路由的 Core:闸门提醒卡要往它投。
    fn core_with_gate_route(alias: &str) -> (Core, tempfile::TempDir, Receiver<DispatchMsg>) {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = channel();
        let core = Core::new(
            HashMap::new(),
            HashMap::from([(alias.to_string(), tx)]),
            vec![agent_on_disk(dir.path(), &format!("name: {alias}\n"))],
        );
        (core, dir, rx)
    }

    /// 带 dispatcher 路由 + operate 名单的 Core:闸门 resolve 的鉴权走 `operators`。
    fn core_with_gate_route_operated(
        alias: &str,
        operator: &str,
    ) -> (Core, tempfile::TempDir, Receiver<DispatchMsg>) {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = channel();
        let agent = agent_on_disk(
            dir.path(),
            &format!("name: {alias}\nfeishu:\n  operators: ['{operator}']\n"),
        );
        let core = Core::new(
            HashMap::new(),
            HashMap::from([(alias.to_string(), tx)]),
            vec![agent],
        );
        (core, dir, rx)
    }

    fn agent_yaml(y: &str) -> Agent {
        serde_yaml::from_str(y).unwrap()
    }

    /// 落盘的 agent:判权以盘上 agent.yaml 为准(见 `current_gate_agent`)。
    fn agent_on_disk(root: &std::path::Path, yaml: &str) -> Agent {
        let mut agent = agent_yaml(yaml);
        let dir = root.join(&agent.name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.yaml"), yaml).unwrap();
        agent.dir = dir;
        agent
    }

    // ── Slice 3 行为等价回归(沿用)──────────────────────────────────────────────

    #[test]
    fn chat_message_delegates_to_dispatcher_unchanged() {
        let (tx, rx) = channel::<DispatchMsg>();
        let mut routes = HashMap::new();
        routes.insert("app-1".to_string(), tx);
        let core = Core::for_routes(routes);

        let cmd = chat_cmd("oc_room", "你好");
        assert_eq!(core.submit(cmd.clone()), CommandOutcome::Done);
        assert!(core.dispatch_inbound("app-1", &cmd, sample_inbound("oc_room", "你好")));
        let got = rx.recv().unwrap();
        assert!(
            matches!(&got, DispatchMsg::Inbound(i) if i.chat_id == "oc_room" && i.text == "你好"),
            "dispatcher 应原样收到等价 Inbound"
        );
    }

    #[test]
    fn chat_ask_question_normalization_rejects_ambiguous_specs() {
        use summon_core::core_port::AskQuestion;

        assert!(normalize_chat_ask_questions(&[]).is_err());
        assert!(normalize_chat_ask_questions(&[AskQuestion {
            question: "   ".into(),
            ..Default::default()
        }])
        .is_err());
        assert!(normalize_chat_ask_questions(&[AskQuestion {
            question: "选择".into(),
            options: vec![" A ".into(), "A".into()],
            ..Default::default()
        }])
        .is_err());

        let normalized = normalize_chat_ask_questions(&[AskQuestion {
            question: "  选择方案  ".into(),
            header: Some("  决策  ".into()),
            options: vec![" A ".into(), " B ".into()],
            descriptions: vec!["  快  ".into()],
            allow_other: true,
            multi_select: false,
        }])
        .unwrap();
        assert_eq!(normalized[0].question, "选择方案");
        assert_eq!(normalized[0].header.as_deref(), Some("决策"));
        assert_eq!(normalized[0].options, ["A", "B"]);
        assert_eq!(normalized[0].descriptions, ["快"]);
    }

    #[test]
    fn teardown_chat_routes_to_agent_dispatcher() {
        // agent_routes 按 alias 键控:TeardownChat{Reset} 应原样投到该 dispatcher 通道。
        let (tx, rx) = channel::<DispatchMsg>();
        let mut agent_routes = HashMap::new();
        agent_routes.insert("starlight".to_string(), tx);
        let core = Core::new(HashMap::new(), agent_routes, Vec::new());

        let cmd = Command {
            actor: Actor::Owner,
            intent: Intent::TeardownChat {
                alias: "starlight".into(),
                chat_id: ChatId::new("oc_room"),
                mode: TeardownMode::Reset,
            },
        };
        assert_eq!(core.submit(cmd), CommandOutcome::Done);
        let got = rx.recv().unwrap();
        assert!(
            matches!(&got, DispatchMsg::TeardownChat { chat_id, reset } if chat_id == "oc_room" && *reset),
            "dispatcher 应收到 TeardownChat{{chat_id=oc_room, reset=true}}"
        );
    }

    /// Menu Bar 的 Exit / Archive 落在管家卡同一条通道上,`archive` 原样带过去。
    #[test]
    fn end_chat_session_routes_to_the_fleet_session_channel() {
        for archive in [false, true] {
            let (tx, rx) = channel::<DispatchMsg>();
            let mut agent_routes = HashMap::new();
            agent_routes.insert("starlight".to_string(), tx);
            let core = Core::new(HashMap::new(), agent_routes, Vec::new());
            let cmd = Command {
                actor: Actor::Owner,
                intent: Intent::EndChatSession {
                    alias: "starlight".into(),
                    chat_id: ChatId::new("oc_room"),
                    archive,
                },
            };
            assert_eq!(core.submit(cmd), CommandOutcome::Done);
            let got = rx.recv().unwrap();
            assert!(
                matches!(&got, DispatchMsg::FleetSessionControl { chat_id, archive: a } if chat_id == "oc_room" && *a == archive),
                "archive={archive} 时 dispatcher 应收到同值的 FleetSessionControl"
            );
        }
    }

    /// 飞书来的人不能借这条 Owner 通道结束会话,也不许投出任何消息。
    #[test]
    fn end_chat_session_from_feishu_is_denied_without_dispatching() {
        let (tx, rx) = channel::<DispatchMsg>();
        let mut agent_routes = HashMap::new();
        agent_routes.insert("starlight".to_string(), tx);
        let core = Core::new(HashMap::new(), agent_routes, Vec::new());
        let cmd = Command {
            actor: Actor::Feishu("ou_x".into()),
            intent: Intent::EndChatSession {
                alias: "starlight".into(),
                chat_id: ChatId::new("oc_room"),
                archive: true,
            },
        };
        assert!(matches!(core.submit(cmd), CommandOutcome::Denied(_)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn end_chat_session_unknown_alias_is_denied() {
        let core = Core::for_routes(HashMap::new());
        let cmd = Command {
            actor: Actor::Owner,
            intent: Intent::EndChatSession {
                alias: "nope".into(),
                chat_id: ChatId::new("oc"),
                archive: false,
            },
        };
        assert!(matches!(core.submit(cmd), CommandOutcome::Denied(_)));
    }

    #[test]
    fn teardown_chat_unknown_alias_is_denied() {
        let core = Core::for_routes(HashMap::new());
        let cmd = Command {
            actor: Actor::Owner,
            intent: Intent::TeardownChat {
                alias: "nope".into(),
                chat_id: ChatId::new("oc"),
                mode: TeardownMode::Clean,
            },
        };
        assert!(matches!(core.submit(cmd), CommandOutcome::Denied(_)));
    }

    /// **管家跨 agent 停会话的第二道闸**:目标 agent 自己的 operate 名单说了算。
    /// 名单里没有这个人 → 拒绝,而且**一条消息都不许投出去** —— 投出去就已经停了。
    #[test]
    fn fleet_session_control_checks_the_target_agents_own_roster() {
        let (tx, rx) = channel::<DispatchMsg>();
        let mut agent_routes = HashMap::new();
        agent_routes.insert("starlight".to_string(), tx);
        let root = tempfile::tempdir().unwrap();
        let target = agent_on_disk(
            root.path(),
            r#"
name: starlight
feishu:
  operators: ["ou_boss"]
"#,
        );
        let core = Core::new(HashMap::new(), agent_routes, vec![target]);

        // 名单里的人:放行,并落到既有的 TeardownChat 通道。
        assert_eq!(
            core.route_fleet_session_control("ou_boss", "on_boss", "starlight", "oc_room", true),
            CommandOutcome::Done
        );
        let got = rx.recv().unwrap();
        assert!(
            matches!(&got, DispatchMsg::FleetSessionControl { chat_id, archive } if chat_id == "oc_room" && *archive)
        );

        // 名单外的人:即使他能用管家(第一道闸已过),也停不了这个 agent 的会话。
        assert!(matches!(
            core.route_fleet_session_control("ou_stranger", "on_x", "starlight", "oc_room", false),
            CommandOutcome::Denied(_)
        ));
        assert!(rx.try_recv().is_err(), "被拒时绝不能已经把拆除指令投出去");
    }

    /// 目标 agent 的本群名单不算数:按钮在管家的卡上,不在被停的那个群里。
    #[test]
    fn fleet_session_control_ignores_the_target_chats_own_operators() {
        let (tx, rx) = channel::<DispatchMsg>();
        let mut agent_routes = HashMap::new();
        agent_routes.insert("starlight".to_string(), tx);
        let root = tempfile::tempdir().unwrap();
        let target = agent_on_disk(
            root.path(),
            r#"
name: starlight
feishu:
  operators: ["ou_boss"]
  chat_operators:
    oc_room: ["ou_colleague"]
"#,
        );
        let core = Core::new(HashMap::new(), agent_routes, vec![target]);
        assert!(matches!(
            core.route_fleet_session_control("ou_colleague", "", "starlight", "oc_room", false),
            CommandOutcome::Denied(_)
        ));
        assert!(rx.try_recv().is_err(), "被拒时绝不能已经把拆除指令投出去");
    }

    /// **目标 agent 名单为空时必须拒绝**,不能沿用 `can_operate` 私聊档的默认放行:
    /// 这条路径操作的是别人的会话,「没配名单」只能读成「不许」。
    #[test]
    fn fleet_session_control_fails_closed_on_empty_roster() {
        let (tx, rx) = channel::<DispatchMsg>();
        let mut agent_routes = HashMap::new();
        agent_routes.insert("starlight".to_string(), tx);
        let root = tempfile::tempdir().unwrap();
        let target = agent_on_disk(root.path(), "name: starlight\n");
        let core = Core::new(HashMap::new(), agent_routes, vec![target]);
        assert!(matches!(
            core.route_fleet_session_control("ou_boss", "on_boss", "starlight", "oc_room", false),
            CommandOutcome::Denied(_)
        ));
        assert!(rx.try_recv().is_err(), "fail-closed:不许投出去");
    }

    /// 名单以盘上 agent.yaml 为准:从名单里撤掉的人当场停不了会话,也答不了闸门,不用重启
    /// daemon。快照(`list_sessions` 等读它)同时刷新。
    #[test]
    fn a_revoked_operator_loses_control_without_a_daemon_restart() {
        let (tx, rx) = channel::<DispatchMsg>();
        let root = tempfile::tempdir().unwrap();
        let target = agent_on_disk(
            root.path(),
            "name: starlight\nfeishu:\n  operators: ['ou_boss', 'ou_former']\n",
        );
        let manifest = target.manifest_path();
        let core = Core::new(
            HashMap::new(),
            HashMap::from([("starlight".to_string(), tx)]),
            vec![target],
        );
        assert_eq!(
            core.route_fleet_session_control("ou_former", "", "starlight", "oc_room", false),
            CommandOutcome::Done
        );
        let _ = rx.recv().unwrap();
        let gate_id = register_chat_gate(&core, "starlight", "oc_room", "选 A 还是 B?");

        std::fs::write(
            &manifest,
            "name: starlight\nfeishu:\n  operators: ['ou_boss']\n",
        )
        .unwrap();
        assert!(matches!(
            core.route_fleet_session_control("ou_former", "", "starlight", "oc_room", false),
            CommandOutcome::Denied(_)
        ));
        assert!(rx.try_recv().is_err(), "被撤掉的人不能再把拆除指令投出去");
        let answer = core.submit_feishu(
            Command {
                actor: Actor::Feishu("ou_former".into()),
                intent: Intent::QuestionResolve(gate_id, "B".into()),
            },
            FeishuGateCtx {
                union_id: String::new(),
                is_group: true,
                chat_id: "oc_room".into(),
                message_id: "om_card".into(),
            },
        );
        assert!(matches!(answer, CommandOutcome::Denied(_)), "{answer:?}");
        let snapshot = core.gate_agents.lock().unwrap()[0].clone();
        assert_eq!(
            snapshot.feishu.unwrap().operators.unwrap(),
            vec!["ou_boss".to_string(), "ou_former".to_string()],
            "判权只读盘上那份,不回写共享快照(快照由 dispatcher 重读后刷新)"
        );
    }

    /// 本代上线时可路由的 agent 在盘上停用了:与 dispatcher 挡入站同一判据,判权一律拒。
    #[test]
    fn a_deactivated_agent_fails_closed() {
        let (tx, rx) = channel::<DispatchMsg>();
        let root = tempfile::tempdir().unwrap();
        let yaml = "name: starlight\nfeishu:\n  app_id_env: APP_A\n  operators: ['ou_boss']\n";
        let target = agent_on_disk(root.path(), yaml);
        let manifest = target.manifest_path();
        let core = Core::new(
            HashMap::new(),
            HashMap::from([("starlight".to_string(), tx)]),
            vec![target],
        );
        assert_eq!(
            core.route_fleet_session_control("ou_boss", "", "starlight", "oc_room", false),
            CommandOutcome::Done
        );
        let _ = rx.recv().unwrap();
        std::fs::write(&manifest, format!("{yaml}status: disabled\n")).unwrap();
        assert!(matches!(
            core.route_fleet_session_control("ou_boss", "", "starlight", "oc_room", false),
            CommandOutcome::Denied(_)
        ));
        assert!(rx.try_recv().is_err());
    }

    /// `.env` 里的凭据换了(manifest 字段没变):与 dispatcher 用同一份冻结身份比对,判权一律拒。
    #[test]
    fn changed_credentials_fail_closed_until_restart() {
        let (tx, rx) = channel::<DispatchMsg>();
        let root = tempfile::tempdir().unwrap();
        let target = agent_on_disk(
            root.path(),
            "name: starlight\nfeishu:\n  app_id_env: T_APP_ID\n  app_secret_env: T_APP_SECRET\n  operators: ['ou_boss']\n",
        );
        std::fs::write(
            target.dir.join(".env"),
            "T_APP_ID=app-a\nT_APP_SECRET=secret-a\n",
        )
        .unwrap();
        let core = Core::new(
            HashMap::new(),
            HashMap::from([("starlight".to_string(), tx)]),
            vec![target.clone()],
        );
        core.attach_agent_identities(std::sync::Arc::new(std::sync::RwLock::new(HashMap::from(
            [(
                "starlight".to_string(),
                crate::runtime::daemon::drain_agent_for_test(&target, "app-a", "secret-a"),
            )],
        ))));
        assert_eq!(
            core.route_fleet_session_control("ou_boss", "", "starlight", "oc_room", false),
            CommandOutcome::Done
        );
        let _ = rx.recv().unwrap();
        std::fs::write(
            target.dir.join(".env"),
            "T_APP_ID=app-b\nT_APP_SECRET=secret-b\n",
        )
        .unwrap();
        assert!(matches!(
            core.route_fleet_session_control("ou_boss", "", "starlight", "oc_room", false),
            CommandOutcome::Denied(_)
        ));
        assert!(rx.try_recv().is_err());
    }

    /// 飞书身份(app 环境变量名等)在盘上变了:身份要等重启 daemon 才能换,这期间判权一律拒。
    #[test]
    fn a_changed_feishu_identity_fails_closed_until_restart() {
        let (tx, rx) = channel::<DispatchMsg>();
        let root = tempfile::tempdir().unwrap();
        let target = agent_on_disk(
            root.path(),
            "name: starlight\nfeishu:\n  app_id_env: APP_A\n  operators: ['ou_boss']\n",
        );
        let manifest = target.manifest_path();
        let core = Core::new(
            HashMap::new(),
            HashMap::from([("starlight".to_string(), tx)]),
            vec![target],
        );
        std::fs::write(
            &manifest,
            "name: starlight\nfeishu:\n  app_id_env: APP_B\n  operators: ['ou_boss']\n",
        )
        .unwrap();
        assert!(matches!(
            core.route_fleet_session_control("ou_boss", "", "starlight", "oc_room", false),
            CommandOutcome::Denied(_)
        ));
        assert!(rx.try_recv().is_err());
    }

    /// 未注册的 alias 直接拒,不去猜「是不是想说另一个」。
    #[test]
    fn fleet_session_control_unknown_agent_is_denied() {
        let core = Core::new(HashMap::new(), HashMap::new(), Vec::new());
        assert!(matches!(
            core.route_fleet_session_control("ou_boss", "on_boss", "nope", "oc", false),
            CommandOutcome::Denied(_)
        ));
    }

    #[test]
    fn set_autostart_non_owner_is_denied() {
        // 非 Owner(飞书用户)不许改本机开机自启;在任何 IO 前就 Denied(故不触 launchctl)。
        let core = Core::for_routes(HashMap::new());
        let cmd = Command {
            actor: Actor::Feishu("ou_x".into()),
            intent: Intent::SetAutostart {
                enabled: true,
                keep_alive: false,
            },
        };
        assert!(matches!(core.submit(cmd), CommandOutcome::Denied(_)));
    }

    #[test]
    fn teardown_chat_non_owner_is_denied() {
        // 非 Owner(飞书用户 / agent)不许走 CLI 拆除通道。
        let (tx, _rx) = channel::<DispatchMsg>();
        let mut agent_routes = HashMap::new();
        agent_routes.insert("starlight".to_string(), tx);
        let core = Core::new(HashMap::new(), agent_routes, Vec::new());
        let cmd = Command {
            actor: Actor::Feishu("ou_x".into()),
            intent: Intent::TeardownChat {
                alias: "starlight".into(),
                chat_id: ChatId::new("oc"),
                mode: TeardownMode::Clean,
            },
        };
        assert!(matches!(core.submit(cmd), CommandOutcome::Denied(_)));
    }

    #[test]
    fn set_model_effort_owner_gated_and_agent_checked() {
        // 本机 owner 改 model/effort 是 Owner-only(本机改 agent.yaml);两道闸都在写盘前拦下:
        // ① 非 Owner(飞书用户)→ Denied;② Owner 但 alias 未注册 → Denied(无此 agent),均不触 IO。
        let core = Core::for_routes(HashMap::new());
        let non_owner = Command {
            actor: Actor::Feishu("ou_x".into()),
            intent: Intent::SetModel {
                alias: "ambrose".into(),
                model: "claude-opus-4-8".into(),
            },
        };
        assert!(matches!(core.submit(non_owner), CommandOutcome::Denied(_)));
        let owner_unknown = Command {
            actor: Actor::Owner,
            intent: Intent::SetEffort {
                alias: "no-such-agent".into(),
                effort: "high".into(),
            },
        };
        assert!(matches!(
            core.submit(owner_unknown),
            CommandOutcome::Denied(_)
        ));
    }

    /// 刷新线程只握着启动时那份 agent 的 clone:它填上的应用所有者,经 Core 现读盘的判权副本、
    /// 以及 dispatcher 重读后换进来的副本都要看得到。
    #[test]
    fn app_owner_filled_by_refresher_reaches_gate_agents() {
        use summon_core::config::registry::{AppOwner, AppOwnerState};
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = channel();
        let agent = agent_on_disk(dir.path(), "name: aodi\nfeishu:\n  app_id_env: A\n");
        let refresher_copy = agent.clone();
        let core = Core::new(
            HashMap::new(),
            HashMap::from([("aodi".to_string(), tx)]),
            vec![agent.clone()],
        );
        let can = |core: &Core| {
            core.current_gate_agent("aodi").unwrap().can_operate(
                "ou_owner",
                "",
                true,
                summon_core::config::registry::OperateScope::Chat("oc_x"),
            )
        };
        assert!(!can(&core));
        refresher_copy
            .app_owner
            .set(AppOwnerState::Known(Some(AppOwner {
                open_id: "ou_owner".into(),
                union_id: "on_owner".into(),
            })));
        assert!(can(&core));
        core.refresh_agent(&agent.reread().unwrap());
        assert!(can(&core));
        refresher_copy.app_owner.set(AppOwnerState::Known(None));
        assert!(!can(&core));
    }

    /// CLI 改完 model 当场唤醒 dispatcher:它循环顶部的 mtime 检查把新值推给在线 chat 线程,
    /// claude 会话据此注入 `/model`。不唤醒就要等下一个事件或空闲扫描节拍。
    #[test]
    fn setting_a_model_writes_agent_yaml_and_wakes_the_dispatcher() {
        let (core, _dir, rx) = core_with_gate_route("ambrose");
        let set = Command {
            actor: Actor::Owner,
            intent: Intent::SetModel {
                alias: "ambrose".into(),
                model: "claude-sonnet-5-5".into(),
            },
        };
        assert_eq!(core.submit(set), CommandOutcome::Done);
        assert_eq!(
            core.current_gate_agent("ambrose")
                .and_then(|agent| agent.model().map(str::to_string))
                .as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert!(matches!(rx.try_recv(), Ok(DispatchMsg::AgentYamlWritten)));
    }

    /// 不像模型 id 的输入不写 agent.yaml、不唤醒 dispatcher。
    #[test]
    fn a_sentence_is_not_written_as_a_model() {
        let (core, _dir, rx) = core_with_gate_route("ambrose");
        let before = core
            .current_gate_agent("ambrose")
            .and_then(|agent| agent.model().map(str::to_string));
        let set = Command {
            actor: Actor::Owner,
            intent: Intent::SetModel {
                alias: "ambrose".into(),
                model: "切到 sonnet 吧".into(),
            },
        };
        assert!(matches!(core.submit(set), CommandOutcome::Denied(_)));
        assert_eq!(
            core.current_gate_agent("ambrose")
                .and_then(|agent| agent.model().map(str::to_string)),
            before
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn reload_agents_is_owner_only_and_needs_admitter() {
        // 飞书侧永不该能触发「接新 agent 上线」——它会起进程、重启 sidecar。
        let core = Core::for_routes(HashMap::new());
        let non_owner = Command {
            actor: Actor::Feishu("ou_x".into()),
            intent: Intent::ReloadAgents,
        };
        assert!(matches!(core.submit(non_owner), CommandOutcome::Denied(_)));
        // Owner 但没装配注册器(非 daemon 进程内的 Core)→ Denied,而不是 panic 或静默成功。
        let owner = Command {
            actor: Actor::Owner,
            intent: Intent::ReloadAgents,
        };
        assert!(matches!(core.submit(owner), CommandOutcome::Denied(_)));
    }

    #[test]
    fn admit_agent_registers_all_three_key_spaces_and_refuses_duplicates() {
        let core = Core::for_routes(HashMap::new());
        let agent: Agent = serde_yaml::from_str(
            "name: newbie\nfeishu:\n  app_id_env: A\nruntime:\n  cli: codex\n",
        )
        .unwrap();
        let (tx, rx) = channel::<DispatchMsg>();
        assert!(core.admit_agent(&agent, Some("app-new"), tx));
        // 三个键空间必须同时可达:任一缺失都会变成「消息能进来但投不出去」这类难查的半死状态。
        assert!(core.route("app-new").is_some());
        assert!(core.route("stub:newbie").is_some()); // stub 键给本机注入用
        assert!(core.route_by_alias("newbie").is_some());
        assert!(core
            .routes
            .read()
            .unwrap()
            .by_delivery_scope
            .contains_key(&crate::runtime::daemon::feishu_delivery_scope(&agent)));
        // operate-gate 快照也要认识它,否则它的破坏性动作会被 fail-closed 拒掉。
        assert!(core
            .gate_agents
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.name == "newbie"));
        // 停机时必须能收到 Shutdown —— 取自路由表而非启动快照。
        assert_eq!(core.all_dispatcher_senders().len(), 1);
        // 重复注册不覆盖:覆盖会把旧 dispatcher 换成收不到消息的孤儿线程。
        let (tx2, _rx2) = channel::<DispatchMsg>();
        assert!(!core.admit_agent(&agent, Some("app-new"), tx2));
        // 原发送端仍然是活的那一个(rx 还连着它)。
        core.route_by_alias("newbie")
            .unwrap()
            .send(DispatchMsg::Shutdown)
            .unwrap();
        assert!(matches!(rx.recv().unwrap(), DispatchMsg::Shutdown));
    }

    #[test]
    fn retire_routes_waits_for_idle_ack_and_removes_all_keys() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_on_disk(dir.path(), "name: retired\n");
        let core = Core::for_routes(HashMap::new());
        let (tx, rx) = channel();
        assert!(core.admit_agent(&agent, None, tx));
        let worker = std::thread::spawn(move || {
            let DispatchMsg::Retire(reply) = rx.recv().unwrap() else {
                panic!("expected retire");
            };
            reply.send(Ok(())).unwrap();
        });
        core.retire_routes(&agent).unwrap();
        worker.join().unwrap();
        assert!(!core.has_app_route("stub:retired"));
        assert!(core.route_by_alias("retired").is_none());
        assert!(core.routes.read().unwrap().by_delivery_scope.is_empty());
        core.finish_agent_cleanup("retired");
        assert!(core.gate_agents.lock().unwrap().is_empty());
        let (tx, _rx) = channel();
        assert!(core.admit_agent(&agent, None, tx));
        assert!(core.has_app_route("stub:retired"));
    }

    #[test]
    fn retire_routes_preserves_registration_on_busy_reply() {
        let (core, _dir, rx) = core_with_gate_route("busy");
        let agent = core.current_gate_agent("busy").unwrap();
        let worker = std::thread::spawn(move || {
            let DispatchMsg::Retire(reply) = rx.recv().unwrap() else {
                panic!("expected retire");
            };
            reply.send(Err("busy".into())).unwrap();
        });
        assert!(core.retire_routes(&agent).is_err());
        worker.join().unwrap();
        assert!(core.route_by_alias("busy").is_some());
    }

    #[test]
    fn unknown_app_is_not_routed() {
        let (tx, _rx) = channel::<DispatchMsg>();
        let mut routes = HashMap::new();
        routes.insert("app-1".to_string(), tx);
        let core = Core::for_routes(routes);
        let cmd = chat_cmd("oc_room", "hi");
        assert!(!core.dispatch_inbound("app-missing", &cmd, sample_inbound("oc_room", "hi")));
    }

    #[test]
    fn empty_routes_deny_chat_message() {
        let core = Core::for_routes(HashMap::new());
        assert!(matches!(
            core.submit(chat_cmd("oc_x", "x")),
            CommandOutcome::Denied(_)
        ));
    }

    // ── Slice 4 Task 写命令 + operate-gate ──────────────────────────────────────

    #[test]
    fn subscribe_then_publish_delivers() {
        let core = Core::for_routes(HashMap::new());
        let rx = core.subscribe();
        core.publish(CoreEvent::SessionChanged(SessionView {
            key: "soul-x".into(),
            alias: "x".into(),
            live: true,
            cli: "claude".into(),
            model: None,
            effort: None,
            models: Vec::new(),
            efforts: Vec::new(),
        }));
        assert!(matches!(rx.recv(), Ok(CoreEvent::SessionChanged(_))));
    }

    // ── Slice 5 动作通道 + 阻塞闸门 ─────────────────────────────────────────────

    /// 闸门夹具:登记一个 chat 闸门(来源 = 该 alias 的 `summon ask`)。
    ///
    /// 直接驱动 Core 的闸门面而不经 `do_chat_ask`:后者要一条**本代 daemon 内存里活着**的 turn
    /// 租约,那是 `summon ask` 端到端的事;这里要钉的是「登记 / 等待 / 唤醒 / 清表」这套机制本身。
    fn register_chat_gate(core: &Core, alias: &str, chat_id: &str, text: &str) -> GateId {
        core.register_gate(GateView {
            origin: GateOrigin {
                alias: alias.to_string(),
                chat_id: chat_id.to_string(),
                chat_type: "group".into(),
                requester_open_id: "ou_asker".into(),
            },
            kind: "ask".into(),
            text: text.to_string(),
        })
    }

    fn register_bound_chat_ask_gate(
        core: &Core,
        alias: &str,
        chat_id: &str,
        message_id: &str,
        questions: Vec<summon_core::core_port::AskQuestion>,
    ) -> GateId {
        let view = GateView {
            origin: GateOrigin {
                alias: alias.to_string(),
                chat_id: chat_id.to_string(),
                chat_type: "group".into(),
                requester_open_id: "ou_asker".into(),
            },
            kind: "ask".into(),
            text: questions[0].question.clone(),
        };
        let gate_id = core
            .register_chat_ask_gate(view, questions, None)
            .expect("首张提问卡应登记成功");
        assert!(core.bind_gate_message(&gate_id, message_id));
        gate_id
    }

    /// 登记即两表就位,且 `list_gates` 能把来源/正文原样报出来 —— resolve 要靠 origin 找权威 agent,
    /// 少一表就等于闸门要么唤不醒(缺 sender)要么无人可查(缺 gates)。
    #[test]
    fn register_gate_puts_gate_in_both_tables_and_snapshot() {
        let (core, _d, _rx) = core_with_gate_route("coder");
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "用哪个方案?");

        assert!(core.gates.lock().unwrap().contains_key(&gate_id));
        assert!(core.gate_senders.lock().unwrap().contains_key(&gate_id));
        let snapshot = core.list_gates();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].gate_id, gate_id);
        assert_eq!(snapshot[0].gate.text, "用哪个方案?");
        assert_eq!(snapshot[0].gate.origin.alias, "coder");
        assert_eq!(snapshot[0].gate.origin.chat_id, "oc_room");
    }

    /// 并发子任务可能几乎同时发起 native ask；“同 chat 一张卡”必须由 Core 原子保证，不能
    /// 依赖 Codex 当前通常单线程这个偶然事实。
    #[test]
    fn concurrent_chat_ask_registration_admits_exactly_one_gate() {
        use std::sync::{Arc, Barrier};

        let (core, _d, _rx) = core_with_gate_route("coder");
        let core = Arc::new(core);
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();
        for i in 0..8 {
            let core = Arc::clone(&core);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                core.register_chat_ask_gate(
                    GateView {
                        origin: GateOrigin {
                            alias: "coder".into(),
                            chat_id: "oc_same".into(),
                            chat_type: "group".into(),
                            requester_open_id: "ou_asker".into(),
                        },
                        kind: "ask".into(),
                        text: format!("q{i}"),
                    },
                    Vec::new(),
                    None,
                )
            }));
        }
        let admitted: Vec<_> = workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(admitted.len(), 1, "同 chat 并发只许一个 gate 胜出");
        core.cancel_undelivered_gate(&admitted[0]);
    }

    /// 一个闸门只允许一个等待者:第二次 `wait_gate` 必须诚实拒绝而不是也挂上去 ——
    /// 两个等待者里注定有一个永远等不到唤醒(rx 只有一个)。
    #[test]
    fn invisible_gate_is_denied_without_leaking_waiters() {
        let (core, _d, _rx) = core_with_gate_route("coder");
        // 从没登记过的闸门:等它 = 立刻拒,不留悬挂等待。
        let out = core.wait_gate(&GateId::new("gate-never-registered"));
        assert!(
            matches!(out, CommandOutcome::Denied(ref m) if m.contains("闸门不存在或已被等待")),
            "不存在的闸门必须立即失败:{out:?}"
        );
        assert!(core.gates.lock().unwrap().is_empty());
        assert!(core.gate_senders.lock().unwrap().is_empty());

        // 已被等待的闸门:rx 已被取走,第二个等待者同样拒。
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "q");
        let core = std::sync::Arc::new(core);
        let core_w = std::sync::Arc::clone(&core);
        let g = gate_id.clone();
        let waiter = std::thread::spawn(move || core_w.wait_gate(&g));
        // 等第一个等待者真的取走 rx,再验第二个被拒(不靠固定 sleep 猜调度)。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !core.gate_waiter_attached(&gate_id) {
            assert!(
                std::time::Instant::now() < deadline,
                "等待者应在有界时间内取走 rx"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let second = core.wait_gate(&gate_id);
        assert!(
            matches!(second, CommandOutcome::Denied(_)),
            "同一闸门的第二个等待者必须被拒:{second:?}"
        );
        core.cancel_undelivered_gate(&gate_id);
        assert!(matches!(waiter.join().unwrap(), CommandOutcome::Denied(_)));
    }

    /// owner 只要两次催答：第 3 分钟提醒一次、第 55 分钟再提醒一次，之后永久静默等待。
    #[test]
    fn chat_ask_reminders_fire_only_at_three_and_fifty_five_minutes() {
        assert_eq!(chat_ask_reminder_due(Duration::from_secs(179), 0), None);
        assert_eq!(
            chat_ask_reminder_due(Duration::from_secs(180), 0),
            Some(Duration::from_secs(180))
        );
        assert_eq!(chat_ask_reminder_due(Duration::from_secs(3_299), 1), None);
        assert_eq!(
            chat_ask_reminder_due(Duration::from_secs(3_300), 1),
            Some(Duration::from_secs(3_300))
        );
        assert_eq!(
            chat_ask_reminder_due(Duration::from_secs(24 * 60 * 60), 2),
            None,
            "55 分钟后不得继续周期性 @ 用户"
        );
    }

    /// RPC 原生提问不像 `summon ask` 子进程那样会随进程组一起被 SIGINT 收掉；打断 turn 时必须
    /// 主动撤掉内存闸门并唤醒 worker，否则 chat 线程已经进入下一轮，旧问题仍会永久占着
    /// 「同一会话只挂一张卡」的名额。
    #[test]
    fn cancellable_gate_wait_drops_both_tables_and_returns_bounded() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let (core, _d, _rx) = core_with_gate_route("coder");
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "继续吗?");
        let core = Arc::new(core);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let core_w = Arc::clone(&core);
        let cancelled_w = Arc::clone(&cancelled);
        let g = gate_id.clone();
        std::thread::spawn(move || {
            let _ = done_tx.send(core_w.wait_gate_cancellable(&g, &cancelled_w, None));
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !core.gate_waiter_attached(&gate_id) {
            assert!(
                std::time::Instant::now() < deadline,
                "等待者应在有界时间内 attach"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        cancelled.store(true, Ordering::Release);
        let outcome = done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("取消后必须有界返回");
        assert!(
            matches!(outcome, CommandOutcome::Denied(ref why) if why.contains("本轮已被打断")),
            "取消应给出可诊断的拒绝:{outcome:?}"
        );
        assert!(!core.gates.lock().unwrap().contains_key(&gate_id));
        assert!(!core.gate_senders.lock().unwrap().contains_key(&gate_id));
    }

    #[test]
    fn dead_rpc_session_cancels_native_gate_without_external_interrupt() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let (core, _d, _rx) = core_with_gate_route("coder");
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "还活着吗?");
        let core = Arc::new(core);
        let cancelled = Arc::new(AtomicBool::new(false));
        let session_alive = Arc::new(AtomicBool::new(true));
        let core_w = Arc::clone(&core);
        let cancelled_w = Arc::clone(&cancelled);
        let session_alive_w = Arc::clone(&session_alive);
        let gate_w = gate_id.clone();
        let waiter = std::thread::spawn(move || {
            core_w.wait_gate_cancellable(&gate_w, &cancelled_w, Some(&session_alive_w))
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while !core.gate_waiter_attached(&gate_id) {
            assert!(Instant::now() < deadline, "等待者应在有界时间内 attach");
            std::thread::sleep(Duration::from_millis(5));
        }
        session_alive.store(false, Ordering::Release);
        let outcome = waiter.join().unwrap();
        assert!(
            matches!(outcome, CommandOutcome::Denied(ref why) if why.contains("app-server 已退出")),
            "session EOF 必须让 native gate 结束:{outcome:?}"
        );
        assert!(core.list_gates().is_empty());
        assert!(!cancelled.load(Ordering::Acquire));
    }

    /// Codex default mode 的 requestUserInput 是 non-blocking；客户端策略到期后应回空答案继续，
    /// 但首先必须精确撤掉旧 gate，避免迟到点击串进下一轮。
    #[test]
    fn cancellable_gate_deadline_expires_without_leaking() {
        use std::sync::atomic::AtomicBool;

        let (core, _d, _rx) = core_with_gate_route("coder");
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "现在回答吗?");
        let outcome = core.wait_gate_cancellable_for(
            &gate_id,
            &AtomicBool::new(false),
            None,
            std::time::Duration::from_millis(20),
        );
        assert!(outcome.is_none(), "deadline 到期应用 None 与取消区分");
        assert!(!core.gates.lock().unwrap().contains_key(&gate_id));
        assert!(!core.gate_senders.lock().unwrap().contains_key(&gate_id));
    }

    #[test]
    fn ending_native_ask_emits_exact_card_invalidation() {
        use std::sync::atomic::AtomicBool;

        let (core, _d, rx) = core_with_gate_route("coder");
        let gate_id = register_bound_chat_ask_gate(
            &core,
            "coder",
            "oc_room",
            "om_question",
            vec![summon_core::core_port::AskQuestion {
                question: "还要继续吗?".into(),
                ..Default::default()
            }],
        );
        assert!(core
            .wait_gate_cancellable_for(
                &gate_id,
                &AtomicBool::new(false),
                None,
                Duration::from_millis(10),
            )
            .is_none());
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            DispatchMsg::ChatAskInvalidated {
                chat_id,
                message_id,
                reason,
                ..
            } => {
                assert_eq!(chat_id, "oc_room");
                assert_eq!(message_id, "om_question");
                assert_eq!(reason, crate::runtime::daemon::ChatAskEndReason::TimedOut);
            }
            _ => panic!("应投 exact-card 失效消息"),
        }
    }

    #[test]
    fn gate_end_and_human_resolution_share_one_atomic_completion_right() {
        let (core, _d, _rx) = core_with_gate_route("coder");
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "现在回答吗?");
        let GateClaim::Attached(rx, ..) = core
            .gates
            .lock()
            .unwrap()
            .get_mut(&gate_id)
            .unwrap()
            .claim_waiter()
        else {
            panic!("新闸门应允许等待方领取通道");
        };

        // resolve 先 remove sender 的真实中间态：结束方必须等到那枚已取走的 sender 完成 send。
        let sender = core.gate_senders.lock().unwrap().remove(&gate_id).unwrap();
        let answer = std::thread::spawn(move || {
            // 故意超过取消轮询周期，证明完成权不依赖“resolver 恰好在 100ms 内被调度”。
            std::thread::sleep(Duration::from_millis(150));
            sender
                .send(Resolution::Question(r#"["人类答案"]"#.into()))
                .unwrap();
        });
        let resolution = core
            .resolution_won_gate_end_race(&gate_id, &rx)
            .expect("resolve 已先取得 sender，结束方必须交付它的答案");
        assert!(matches!(resolution, Resolution::Question(ref raw) if raw == r#"["人类答案"]"#));
        answer.join().unwrap();
        core.drop_gate(&gate_id);

        // 反向：结束方先 remove sender 后，迟到回调不能再消费 gate。
        let second = register_chat_gate(&core, "coder", "oc_room", "还回答吗?");
        let GateClaim::Attached(second_rx, ..) = core
            .gates
            .lock()
            .unwrap()
            .get_mut(&second)
            .unwrap()
            .claim_waiter()
        else {
            panic!("新闸门应允许等待方领取通道");
        };
        assert!(core
            .resolution_won_gate_end_race(&second, &second_rx)
            .is_none());
        assert!(matches!(
            core.deliver_resolution(&second, Resolution::Question("late".into())),
            CommandOutcome::GateClosed(_)
        ));
        core.drop_gate(&second);
    }

    /// resolve 唤醒阻塞的 `wait_gate`,答案原样回灌,两表清干净。
    /// actor 用 operate 名单里的飞书用户(不是 Owner):这条路的鉴权就是靠 operate 名单。
    #[test]
    fn plan_resolve_wakes_blocked_waiter_with_verdict() {
        use std::sync::Arc;
        let (core, _d, _rx) = core_with_gate_route_operated("coder", "ou_op");
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "要继续吗?");
        let core = Arc::new(core);
        // 另一线程模拟「control.sock handler 阻塞等 resolve」。
        let core_w = Arc::clone(&core);
        let g = gate_id.clone();
        let waiter = std::thread::spawn(move || core_w.wait_gate(&g));
        std::thread::sleep(std::time::Duration::from_millis(50));
        let r = core.submit(Command {
            actor: Actor::Feishu("ou_op".into()),
            intent: Intent::QuestionResolve(gate_id.clone(), r#"["是"]"#.into()),
        });
        assert_eq!(r, CommandOutcome::Done);
        match waiter.join().unwrap() {
            CommandOutcome::Value(v) => assert_eq!(v["answer"], r#"["是"]"#),
            other => panic!("等待方应拿到作答,得到 {other:?}"),
        }
        // resolve 后两表都清干净。
        assert!(!core.gates.lock().unwrap().contains_key(&gate_id));
        assert!(!core.gate_senders.lock().unwrap().contains_key(&gate_id));
    }

    #[test]
    fn chat_ask_callback_is_fenced_to_exact_chat_and_message() {
        let (core, _d, _rx) = core_with_gate_route_operated("coder", "ou_op");
        let gate_id = register_bound_chat_ask_gate(
            &core,
            "coder",
            "oc_room",
            "om_current",
            vec![summon_core::core_port::AskQuestion {
                question: "选哪个?".into(),
                options: vec!["A".into(), "B".into()],
                ..Default::default()
            }],
        );
        let context = core.chat_ask_gate_context(&gate_id).unwrap();
        assert_eq!(context.origin.requester_open_id, "ou_asker");

        for (chat_id, message_id) in [
            ("oc_other", "om_current"),
            ("oc_room", "om_copied_or_stale"),
        ] {
            let denied = core.submit_feishu(
                Command {
                    actor: Actor::Feishu("ou_op".into()),
                    intent: Intent::QuestionResolve(gate_id.clone(), r#"["A"]"#.into()),
                },
                FeishuGateCtx {
                    union_id: "on_op".into(),
                    is_group: true,
                    chat_id: chat_id.into(),
                    message_id: message_id.into(),
                },
            );
            assert!(matches!(denied, CommandOutcome::Denied(_)), "{denied:?}");
            assert!(
                core.gate_senders.lock().unwrap().contains_key(&gate_id),
                "错 card 的回调不能消耗 gate"
            );
        }
    }

    #[test]
    fn chat_ask_answers_are_validated_against_authoritative_questions() {
        let (core, _d, _rx) = core_with_gate_route_operated("coder", "ou_op");
        let gate_id = register_bound_chat_ask_gate(
            &core,
            "coder",
            "oc_room",
            "om_current",
            vec![
                summon_core::core_port::AskQuestion {
                    question: "选哪个?".into(),
                    options: vec!["A".into(), "B".into()],
                    ..Default::default()
                },
                summon_core::core_port::AskQuestion {
                    question: "补充说明".into(),
                    ..Default::default()
                },
            ],
        );
        let submit = |raw: &str| {
            core.submit_feishu(
                Command {
                    actor: Actor::Feishu("ou_op".into()),
                    intent: Intent::QuestionResolve(gate_id.clone(), raw.into()),
                },
                FeishuGateCtx {
                    union_id: "on_op".into(),
                    is_group: true,
                    chat_id: "oc_room".into(),
                    message_id: "om_current".into(),
                },
            )
        };

        for invalid in [
            r#"["伪造候选"]"#.to_string(),
            r#"["A",null,"多出来的一题"]"#.to_string(),
            serde_json::json!(["A", "x".repeat(CHAT_ASK_ANSWER_MAX_CHARS + 1)]).to_string(),
        ] {
            let denied = submit(&invalid);
            assert!(matches!(denied, CommandOutcome::Denied(_)), "{denied:?}");
            assert!(core.gate_senders.lock().unwrap().contains_key(&gate_id));
        }

        assert_eq!(submit(r#"["  A  ","  真实补充  "]"#), CommandOutcome::Done);
        match core.wait_gate(&gate_id) {
            CommandOutcome::Value(value) => {
                assert_eq!(value["answer"], r#"["A","真实补充"]"#)
            }
            other => panic!("应拿到规范化答案，得到 {other:?}"),
        }
    }

    /// 本群 operator 能答自己群里的闸门;闸门卡被转发进别的群,那个群的本群 operator 点了不算数。
    #[test]
    fn chat_operator_answers_gates_only_in_the_gates_own_chat() {
        let (tx, _rx) = channel();
        let root = tempfile::tempdir().unwrap();
        let core = Core::new(
            HashMap::new(),
            HashMap::from([("coder".to_string(), tx)]),
            vec![agent_on_disk(
                root.path(),
                "name: coder\nfeishu:\n  operators: ['ou_op']\n  chat_operators:\n    oc_room: ['ou_colleague']\n    oc_other: ['ou_other_colleague']\n",
            )],
        );
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "选 A 还是 B?");
        let click = |who: &str, chat_id: &str| {
            core.submit_feishu(
                Command {
                    actor: Actor::Feishu(who.into()),
                    intent: Intent::QuestionResolve(gate_id.clone(), "B".into()),
                },
                FeishuGateCtx {
                    union_id: String::new(),
                    is_group: true,
                    chat_id: chat_id.into(),
                    message_id: "om_legacy".into(),
                },
            )
        };
        // 两种错法各有一条拦:按点击处判会放过前者,不看点击处会放过后者。
        for (who, chat_id) in [
            ("ou_other_colleague", "oc_other"),
            ("ou_colleague", "oc_other"),
        ] {
            let forwarded = click(who, chat_id);
            assert!(
                matches!(forwarded, CommandOutcome::Denied(_)),
                "{who}@{chat_id}: {forwarded:?}"
            );
            assert!(core.gate_senders.lock().unwrap().contains_key(&gate_id));
        }
        assert_eq!(click("ou_colleague", "oc_room"), CommandOutcome::Done);
    }

    /// 取消也必须唤醒等待方 —— 它是「不超时」设计的逃生阀:agent 拿到明确的「已取消」,
    /// 而不是无声挂死。且非授权用户不得 resolve/取消别人的闸门。
    #[test]
    fn question_resolve_returns_chosen_answer() {
        use std::sync::Arc;
        let (core, _d, _rx) = core_with_gate_route_operated("coder", "ou_op");
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "选 A 还是 B?");
        let core = Arc::new(core);
        // 名单外的人点按钮:一律挡,闸门原样挂着。
        let denied = core.submit_feishu(
            Command {
                actor: Actor::Feishu("ou_stranger".into()),
                intent: Intent::QuestionResolve(gate_id.clone(), "B".into()),
            },
            FeishuGateCtx {
                union_id: "on_stranger".into(),
                is_group: true,
                chat_id: "oc_room".into(),
                message_id: "om_legacy".into(),
            },
        );
        assert!(matches!(denied, CommandOutcome::Denied(_)), "{denied:?}");
        assert!(core.gate_senders.lock().unwrap().contains_key(&gate_id));

        let core_w = Arc::clone(&core);
        let g = gate_id.clone();
        let waiter = std::thread::spawn(move || core_w.wait_gate(&g));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            core.submit(Command {
                actor: Actor::Feishu("ou_op".into()),
                intent: Intent::CancelGate(gate_id.clone()),
            }),
            CommandOutcome::Done
        );
        match waiter.join().unwrap() {
            CommandOutcome::Denied(m) => assert!(m.contains("闸门已取消"), "{m}"),
            other => panic!("取消应把等待方明确放出来,得到 {other:?}"),
        }
        assert!(core.list_gates().is_empty());
    }

    #[test]
    fn unknown_gate_resolve_is_denied() {
        let (core, _d) = core_with(vec![]);
        // resolve 一个不存在的闸门 → 诚实拒绝(不 panic、不挂死)。
        let out = core.submit(Command {
            actor: Actor::Owner,
            intent: Intent::QuestionResolve(GateId::new("gate-nope"), "A".into()),
        });
        assert!(matches!(out, CommandOutcome::GateClosed(_)));
        let cancel = core.submit(Command {
            actor: Actor::Owner,
            intent: Intent::CancelGate(GateId::new("gate-nope")),
        });
        assert!(matches!(cancel, CommandOutcome::GateClosed(_)));
    }

    /// 裁决抢在 waiter attach 之前到达:条目**带暂存活下来**,随后迟到的 `wait_gate` 领走答案、
    /// 清干净双表。
    ///
    /// 这条取代了旧的 `gate_resolve_without_waiter_cleans_gates_hashmap` —— 它断言的
    /// 「无 waiter resolve → 两表立刻全清」编码的正是本次要修的 bug:裁决连同还没被取走的 rx
    /// 一起消失,迟到的 waiter 只看到「闸门不存在」,用户答案被静默丢弃。防泄漏语义没有删除,
    /// 而是迁移到「领取时清」:清表的责任从 resolve 移到 `wait_gate`。
    #[test]
    fn early_resolve_is_stashed_and_claimed_by_the_late_waiter() {
        let (core, _d, _rx) = core_with_gate_route("coder");
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "计划 A");
        // waiter 还没来就 resolve(= handler 还没从 submit 一路返回进 wait_gate)。
        let r = core.submit(Command {
            actor: Actor::Owner,
            intent: Intent::QuestionResolve(gate_id.clone(), "A".into()),
        });
        assert_eq!(r, CommandOutcome::Done);
        assert!(
            core.gates.lock().unwrap().contains_key(&gate_id),
            "裁决早到时条目必须留活,否则答案连同没人接的 rx 一起丢"
        );
        // sender 已摘掉:它同时是「本闸门已被裁决过」的凭证,二次 resolve 必须被挡。
        assert!(!core.gate_senders.lock().unwrap().contains_key(&gate_id));
        let second = core.submit(Command {
            actor: Actor::Owner,
            intent: Intent::QuestionResolve(gate_id.clone(), "B".into()),
        });
        assert!(
            matches!(second, CommandOutcome::GateClosed(_)),
            "暂存中的闸门不许被二次裁决覆盖:{second:?}"
        );
        // 迟到的 waiter 领走暂存,拿到的是**第一次**那个答案。
        match core.wait_gate(&gate_id) {
            CommandOutcome::Value(v) => assert_eq!(v["answer"], "A"),
            other => panic!("迟到的等待方应领到暂存裁决,得到 {other:?}"),
        }
        // 领取即落幕:防泄漏语义在这里兑现(两表清空 + 快照见不到)。
        assert!(!core.gates.lock().unwrap().contains_key(&gate_id));
        assert!(!core.gate_senders.lock().unwrap().contains_key(&gate_id));
        assert!(core.list_gates().is_empty());
    }

    /// 取消也对称暂存:早到的 `CancelGate` 同样等 waiter 来领,agent 拿到的是明确的「已取消」,
    /// 而不是「闸门不存在」那种看不出发生了什么的回执。
    #[test]
    fn early_cancel_is_stashed_and_claimed_by_the_late_waiter() {
        let (core, _d, _rx) = core_with_gate_route_operated("coder", "ou_op");
        let gate_id = register_chat_gate(&core, "coder", "oc_room", "要继续吗?");
        assert_eq!(
            core.submit(Command {
                actor: Actor::Feishu("ou_op".into()),
                intent: Intent::CancelGate(gate_id.clone()),
            }),
            CommandOutcome::Done
        );
        assert!(core.gates.lock().unwrap().contains_key(&gate_id));
        match core.wait_gate(&gate_id) {
            CommandOutcome::Denied(m) => assert!(m.contains("闸门已取消"), "{m}"),
            other => panic!("迟到的等待方应领到「已取消」,得到 {other:?}"),
        }
        assert!(core.list_gates().is_empty());
    }

    /// 真并发形态:**完全不等 attach**,resolve 与 `wait_gate` 直接抢,交错交给调度器决定。
    /// 修复前这就是掷硬币(实测 40 次挂 11 次:resolve 抢先则裁决连同 rx 被 remove,
    /// waiter 只剩 `Denied("闸门不存在或已被等待")`)。修复后两种交错都必须把答案原样交到
    /// 等待方手里 —— 用户点了「是」,agent 就不能收到「闸门不存在」。
    #[test]
    fn resolve_racing_a_late_waiter_never_loses_the_answer() {
        use std::sync::Arc;
        let (core, _d, _rx) = core_with_gate_route("coder");
        let core = Arc::new(core);
        for round in 0..40 {
            let gate_id = register_chat_gate(&core, "coder", "oc_room", "要继续吗?");
            let core_w = Arc::clone(&core);
            let g = gate_id.clone();
            let waiter = std::thread::spawn(move || core_w.wait_gate(&g));
            let r = core.submit(Command {
                actor: Actor::Owner,
                intent: Intent::QuestionResolve(gate_id.clone(), r#"["是"]"#.into()),
            });
            assert_eq!(r, CommandOutcome::Done, "第 {round} 轮 resolve 应成功");
            match waiter.join().unwrap() {
                CommandOutcome::Value(v) => {
                    assert_eq!(v["answer"], r#"["是"]"#, "第 {round} 轮答案被改写")
                }
                other => panic!("第 {round} 轮:用户答案不该丢,得到 {other:?}"),
            }
            assert!(core.list_gates().is_empty(), "第 {round} 轮闸门应清干净");
        }
    }

    #[test]
    fn stashed_answer_is_claimed_once_by_competing_cancelled_late_waiters() {
        use std::sync::{Arc, Barrier};

        let (core, _d, _rx) = core_with_gate_route("coder");
        let core = Arc::new(core);
        for _ in 0..20 {
            let gate_id = register_chat_gate(&core, "coder", "oc_room", "A or B?");
            assert_eq!(
                core.submit(Command {
                    actor: Actor::Owner,
                    intent: Intent::QuestionResolve(gate_id.clone(), "A".into()),
                }),
                CommandOutcome::Done
            );
            let start = Arc::new(Barrier::new(3));
            let mut waiters = Vec::new();
            for _ in 0..2 {
                let core = Arc::clone(&core);
                let gate_id = gate_id.clone();
                let start = Arc::clone(&start);
                waiters.push(std::thread::spawn(move || {
                    start.wait();
                    core.wait_gate_cancellable_for(
                        &gate_id,
                        &AtomicBool::new(true),
                        None,
                        Duration::ZERO,
                    )
                    .unwrap()
                }));
            }
            start.wait();
            let outcomes: Vec<_> = waiters
                .into_iter()
                .map(|waiter| waiter.join().unwrap())
                .collect();
            assert_eq!(
                outcomes
                    .iter()
                    .filter(
                        |out| matches!(out, CommandOutcome::Value(value) if value["answer"] == "A")
                    )
                    .count(),
                1
            );
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|out| matches!(out, CommandOutcome::Denied(_)))
                    .count(),
                1
            );
            assert!(core.list_gates().is_empty());
            assert!(!core.gate_senders.lock().unwrap().contains_key(&gate_id));
        }
    }

    #[test]
    fn competing_resolve_and_cancel_commands_deliver_only_the_first_verdict() {
        use std::sync::{Arc, Barrier};

        let (core, _d, _rx) = core_with_gate_route("coder");
        let core = Arc::new(core);
        for cancel in [false, true] {
            for _ in 0..20 {
                let gate_id = register_chat_gate(&core, "coder", "oc_room", "A or B?");
                let start = Arc::new(Barrier::new(3));
                let mut resolvers = Vec::new();
                for second in [false, true] {
                    let core = Arc::clone(&core);
                    let gate_id = gate_id.clone();
                    let start = Arc::clone(&start);
                    resolvers.push(std::thread::spawn(move || {
                        start.wait();
                        let intent = if second && cancel {
                            Intent::CancelGate(gate_id)
                        } else {
                            Intent::QuestionResolve(gate_id, if second { "B" } else { "A" }.into())
                        };
                        core.submit(Command {
                            actor: Actor::Owner,
                            intent,
                        })
                    }));
                }
                start.wait();
                let results: Vec<_> = resolvers
                    .into_iter()
                    .map(|resolver| resolver.join().unwrap())
                    .collect();
                assert_eq!(
                    results
                        .iter()
                        .filter(|out| **out == CommandOutcome::Done)
                        .count(),
                    1
                );
                assert_eq!(
                    results
                        .iter()
                        .filter(|out| matches!(out, CommandOutcome::GateClosed(_)))
                        .count(),
                    1
                );
                let winner = results
                    .iter()
                    .position(|out| *out == CommandOutcome::Done)
                    .unwrap();
                let answer = core.wait_gate(&gate_id);
                if winner == 1 && cancel {
                    assert!(
                        matches!(answer, CommandOutcome::Denied(reason) if reason.contains("闸门已取消"))
                    );
                } else {
                    assert_eq!(
                        answer,
                        CommandOutcome::Value(
                            serde_json::json!({"answer": if winner == 0 { "A" } else { "B" }})
                        )
                    );
                }
                assert!(core.list_gates().is_empty());
                assert!(!core.gate_senders.lock().unwrap().contains_key(&gate_id));
            }
        }
    }

    // ── Claude PermissionRequest hook ────────────────────────────────────────────

    fn hook_agent(extra_yaml: &str) -> (Agent, tempfile::TempDir) {
        crate::test_support::agent_dir(
            "claude",
            &format!("feishu:\n  operators: ['ou_op']\n{extra_yaml}"),
        )
    }

    fn write_meta(agent: &Agent, session: &str, chat_id: &str) {
        let path = agent
            .state_dir()
            .join(summon_core::chat_meta::meta_rel_path(session));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("chat_id={chat_id}\nchat_type=group\n")).unwrap();
    }

    fn hook_core(agent: &Agent) -> (std::sync::Arc<Core>, Receiver<DispatchMsg>) {
        let (tx, rx) = channel();
        let core = Core::new(
            HashMap::new(),
            HashMap::from([(agent.name.clone(), tx)]),
            vec![agent.clone()],
        );
        (std::sync::Arc::new(core), rx)
    }

    fn hook_request(
        tool: &str,
        input: serde_json::Value,
        agent_id: Option<&str>,
    ) -> summon_core::core_port::ClaudeHookRequest {
        summon_core::core_port::ClaudeHookRequest {
            tool_name: tool.into(),
            tool_input: input,
            agent_id: agent_id.map(str::to_string),
            agent_type: agent_id.map(|_| "general-purpose".to_string()),
        }
    }

    fn hook_ask(
        alias: &str,
        session: &str,
        request: summon_core::core_port::ClaudeHookRequest,
    ) -> Command {
        Command {
            actor: Actor::Owner,
            intent: Intent::ClaudeHookAsk {
                alias: alias.into(),
                session: session.into(),
                request,
            },
        }
    }

    fn click(
        core: &Core,
        gate_id: &GateId,
        chat_id: &str,
        message_id: &str,
        answer: &str,
    ) -> CommandOutcome {
        core.submit_feishu(
            Command {
                actor: Actor::Feishu("ou_op".into()),
                intent: Intent::QuestionResolve(gate_id.clone(), answer.into()),
            },
            FeishuGateCtx {
                union_id: String::new(),
                is_group: true,
                chat_id: chat_id.into(),
                message_id: message_id.into(),
            },
        )
    }

    #[test]
    fn claude_permission_request_posts_card_and_hands_back_the_click() {
        let (agent, _dir) = hook_agent("");
        let session = format!("{}-g1", agent.tmux_name());
        let chat = "oc_hook_allow";
        write_meta(&agent, &session, chat);
        let (core, rx) = hook_core(&agent);
        let request = hook_request(
            "Bash",
            serde_json::json!({"command": "bash -c 'rm $1'"}),
            None,
        );
        let CommandOutcome::Blocked(gate_id) =
            core.submit(hook_ask(&agent.name, &session, request.clone()))
        else {
            panic!("授权请求应阻塞等人裁决");
        };
        match rx.try_recv() {
            Ok(DispatchMsg::ClaudePermissionCard {
                chat_id,
                request: sent,
                requester_open_id,
                ..
            }) => {
                assert_eq!(chat_id, chat, "chat 取自会话的 .meta");
                assert_eq!(sent, request);
                assert_eq!(requester_open_id, "", "没有活跃轮就不 @ 人");
            }
            Ok(other) => panic!("应发授权卡,得到 {}", other.kind_label()),
            Err(e) => panic!("应发授权卡: {e}"),
        }
        assert!(crate::runtime::claude_hook::screen_claim(chat).main);
        assert!(
            matches!(
                click(&core, &gate_id, chat, "om_card", "allow"),
                CommandOutcome::Denied(_)
            ),
            "卡还没绑定 message_id 时回调必须被围栏挡住"
        );
        assert!(core.bind_gate_message(&gate_id, "om_card"));
        for (chat_id, message_id, answer) in [
            ("oc_elsewhere", "om_card", "allow"),
            (chat, "om_copied", "allow"),
            (chat, "om_card", "maybe"),
        ] {
            assert!(
                matches!(
                    click(&core, &gate_id, chat_id, message_id, answer),
                    CommandOutcome::Denied(_)
                ),
                "{chat_id}/{message_id}/{answer} 应被拒"
            );
        }
        let waiter = {
            let core = std::sync::Arc::clone(&core);
            let gate_id = gate_id.clone();
            std::thread::spawn(move || core.wait_gate(&gate_id))
        };
        assert_eq!(
            click(&core, &gate_id, chat, "om_card", "allow"),
            CommandOutcome::Done
        );
        assert_eq!(
            waiter.join().unwrap(),
            CommandOutcome::Value(serde_json::json!({"answer": "allow"}))
        );
        assert!(core.list_gates().is_empty());
        assert_eq!(
            crate::runtime::claude_hook::screen_claim(chat),
            crate::runtime::claude_hook::ScreenClaim::default(),
            "闸门落幕即让出抓屏"
        );
    }

    #[test]
    fn requests_not_fully_shown_can_only_be_denied_or_sent_back_to_the_terminal() {
        let (agent, _dir) = hook_agent("");
        let session = format!("{}-g8", agent.tmux_name());
        let chat = "oc_hook_long";
        write_meta(&agent, &session, chat);
        let (core, _rx) = hook_core(&agent);
        let long = format!("bash -c '{} ; rm -rf $T'", "true ".repeat(3000));
        let submit = || {
            let CommandOutcome::Blocked(gate_id) = core.submit(hook_ask(
                &agent.name,
                &session,
                hook_request("Bash", serde_json::json!({"command": long}), Some("a1")),
            )) else {
                panic!("应阻塞");
            };
            assert!(core.bind_gate_message(&gate_id, "om_long"));
            gate_id
        };
        let gate_id = submit();
        assert!(
            matches!(
                click(&core, &gate_id, chat, "om_long", "allow"),
                CommandOutcome::Denied(_)
            ),
            "卡上看不全的请求不能在飞书上批准"
        );
        assert_eq!(
            click(&core, &gate_id, chat, "om_long", "deny"),
            CommandOutcome::Done
        );

        let gate_id = submit();
        let terminal = |message_id: &str| {
            core.submit_feishu(
                Command {
                    actor: Actor::Feishu("ou_op".into()),
                    intent: Intent::CancelGate(gate_id.clone()),
                },
                FeishuGateCtx {
                    union_id: String::new(),
                    is_group: true,
                    chat_id: chat.into(),
                    message_id: message_id.into(),
                },
            )
        };
        assert!(
            matches!(terminal("om_other"), CommandOutcome::Denied(_)),
            "「到终端处理」同样只认发出去的那张卡"
        );
        assert_eq!(terminal("om_long"), CommandOutcome::Done);
        assert!(
            matches!(core.wait_gate(&gate_id), CommandOutcome::Denied(_)),
            "hook 拿到取消,不作答,落回终端对话框"
        );
    }

    #[test]
    fn claude_hook_requests_that_cannot_be_routed_are_refused_without_a_card() {
        let (agent, _dir) = hook_agent("");
        let session = format!("{}-g2", agent.tmux_name());
        write_meta(&agent, &session, "oc_hook_refuse");
        let (core, rx) = hook_core(&agent);
        let bash = || hook_request("Bash", serde_json::json!({"command": "rm x"}), None);
        let refused = [
            hook_ask(&agent.name, "soul-someone-else-g2", bash()),
            hook_ask(
                &agent.name,
                &format!("{}-g-no-meta", agent.tmux_name()),
                bash(),
            ),
            hook_ask("nobody", &session, bash()),
            hook_ask(
                &agent.name,
                &session,
                hook_request("ExitPlanMode", serde_json::json!({}), None),
            ),
            Command {
                actor: Actor::Feishu("ou_op".into()),
                intent: hook_ask(&agent.name, &session, bash()).intent,
            },
        ];
        for command in refused {
            assert!(
                matches!(core.submit(command.clone()), CommandOutcome::Denied(_)),
                "{command:?}"
            );
        }
        assert!(rx.try_recv().is_err(), "被拒的请求不能发卡");
        assert!(core.list_gates().is_empty());

        let (off, _off_dir) = hook_agent("messaging:\n  claude_permission_hook: false\n");
        let off_session = format!("{}-g3", off.tmux_name());
        write_meta(&off, &off_session, "oc_hook_off");
        let (off_core, off_rx) = hook_core(&off);
        assert!(matches!(
            off_core.submit(hook_ask(&off.name, &off_session, bash())),
            CommandOutcome::Denied(_)
        ));
        assert!(off_rx.try_recv().is_err(), "开关关掉后不发卡");

        let (cursor, _cursor_dir) = crate::test_support::agent_dir("cursor", "");
        let cursor_session = format!("{}-g4", cursor.tmux_name());
        write_meta(&cursor, &cursor_session, "oc_hook_cursor");
        let (cursor_core, _cursor_rx) = hook_core(&cursor);
        assert!(matches!(
            cursor_core.submit(hook_ask(&cursor.name, &cursor_session, bash())),
            CommandOutcome::Denied(_)
        ));
    }

    fn compacted(alias: &str, session: &str) -> Command {
        Command {
            actor: Actor::Owner,
            intent: Intent::ClaudeCompacted {
                alias: alias.into(),
                session: session.into(),
                trigger: Some("auto".into()),
            },
        }
    }

    /// PostCompact hook 报来的压缩:路由校验同授权 hook,通过才把会话交给 dispatcher 推长度卡。
    #[test]
    fn claude_compaction_reports_reach_the_dispatcher_only_for_the_agents_own_session() {
        let (agent, _dir) = hook_agent("");
        let session = format!("{}-g6", agent.tmux_name());
        write_meta(&agent, &session, "oc_hook_compact");
        // 盘上有 .meta 也不行:会话名不归这个 agent,就不是它起的会话。
        write_meta(&agent, "soul-someone-else-g6", "oc_hook_compact");
        let (core, rx) = hook_core(&agent);
        let refused = [
            compacted(&agent.name, "soul-someone-else-g6"),
            compacted(&agent.name, &format!("{}-g-no-meta", agent.tmux_name())),
            compacted("nobody", &session),
            Command {
                actor: Actor::Feishu("ou_op".into()),
                intent: compacted(&agent.name, &session).intent,
            },
        ];
        for command in refused {
            assert!(
                matches!(core.submit(command.clone()), CommandOutcome::Denied(_)),
                "{command:?}"
            );
        }
        assert!(rx.try_recv().is_err(), "被拒的上报不能推卡");

        assert!(matches!(
            core.submit(compacted(&agent.name, &session)),
            CommandOutcome::Done
        ));
        match rx.try_recv() {
            Ok(DispatchMsg::ClaudeCompacted {
                chat_id,
                chat_type,
                session: reported,
                reported_at_ms,
            }) => {
                assert!(reported_at_ms > 0, "收到上报的时刻由 Core 记下");
                assert_eq!(chat_id, "oc_hook_compact", "chat 取自会话的 .meta");
                assert_eq!(chat_type, "group");
                assert_eq!(reported, session);
            }
            Ok(other) => panic!("应交给 dispatcher,得到 {}", other.kind_label()),
            Err(e) => panic!("应交给 dispatcher: {e}"),
        }

        let (cursor, _cursor_dir) = crate::test_support::agent_dir("cursor", "");
        let cursor_session = format!("{}-g7", cursor.tmux_name());
        write_meta(&cursor, &cursor_session, "oc_hook_cursor_compact");
        let (cursor_core, cursor_rx) = hook_core(&cursor);
        assert!(matches!(
            cursor_core.submit(compacted(&cursor.name, &cursor_session)),
            CommandOutcome::Denied(_)
        ));
        assert!(cursor_rx.try_recv().is_err());
    }

    #[test]
    fn claude_ask_user_question_uses_the_ask_card_and_keeps_multi_select() {
        let (agent, _dir) = hook_agent("");
        let session = format!("{}-g5", agent.tmux_name());
        let chat = "oc_hook_ask";
        write_meta(&agent, &session, chat);
        let (core, rx) = hook_core(&agent);
        let request = hook_request(
            "AskUserQuestion",
            serde_json::json!({"questions": [
                {"question": "Which color?", "options": [{"label": "Red"}, {"label": "Blue"}]},
                {"question": "Toppings?", "multiSelect": true,
                 "options": [{"label": "A"}, {"label": "B"}]}
            ]}),
            Some("a1"),
        );
        let CommandOutcome::Blocked(gate_id) =
            core.submit(hook_ask(&agent.name, &session, request.clone()))
        else {
            panic!("提问应阻塞等人作答");
        };
        match rx.try_recv() {
            Ok(DispatchMsg::ChatAskCard { questions, .. }) => {
                assert_eq!(questions.len(), 2);
                assert!(questions[0].allow_other && !questions[0].multi_select);
                assert!(questions[1].multi_select);
            }
            Ok(other) => panic!("应发提问卡,得到 {}", other.kind_label()),
            Err(e) => panic!("应发提问卡: {e}"),
        }
        assert_eq!(
            crate::runtime::claude_hook::screen_claim(chat),
            crate::runtime::claude_hook::ScreenClaim {
                main: false,
                any: true
            },
            "子 agent 的请求不占屏上的对话框"
        );
        assert!(
            matches!(
                core.submit(hook_ask(&agent.name, &session, request)),
                CommandOutcome::Denied(_)
            ),
            "同一会话同时只挂一张提问卡"
        );
        assert!(core.bind_gate_message(&gate_id, "om_ask"));
        assert_eq!(
            click(&core, &gate_id, chat, "om_ask", r#"["Blue","A, B"]"#),
            CommandOutcome::Done
        );
        assert_eq!(
            core.wait_gate(&gate_id),
            CommandOutcome::Value(serde_json::json!({"answer": r#"["Blue","A, B"]"#}))
        );
    }

    #[test]
    fn hook_disconnect_voids_the_card_it_posted() {
        let (agent, _dir) = hook_agent("");
        let session = format!("{}-g6", agent.tmux_name());
        let chat = "oc_hook_void";
        write_meta(&agent, &session, chat);
        let (core, rx) = hook_core(&agent);
        let CommandOutcome::Blocked(gate_id) = core.submit(hook_ask(
            &agent.name,
            &session,
            hook_request("Bash", serde_json::json!({"command": "rm x"}), None),
        )) else {
            panic!("应阻塞");
        };
        assert!(matches!(
            rx.try_recv(),
            Ok(DispatchMsg::ClaudePermissionCard { .. })
        ));
        assert!(core.bind_gate_message(&gate_id, "om_void"));
        let gone = std::sync::atomic::AtomicBool::new(true);
        assert!(matches!(
            core.wait_gate_cancellable(&gate_id, &gone, None),
            CommandOutcome::Denied(_)
        ));
        match rx.try_recv() {
            Ok(DispatchMsg::ClaudePermissionVoid { message_id, .. }) => {
                assert_eq!(message_id, "om_void")
            }
            Ok(other) => panic!("应作废原卡,得到 {}", other.kind_label()),
            Err(e) => panic!("应作废原卡: {e}"),
        }
        assert!(core.list_gates().is_empty());
        assert_eq!(
            crate::runtime::claude_hook::screen_claim(chat),
            crate::runtime::claude_hook::ScreenClaim::default()
        );
        assert!(
            matches!(
                click(&core, &gate_id, chat, "om_void", "allow"),
                CommandOutcome::GateClosed(_)
            ),
            "作废后的卡点了也不生效"
        );
    }

    #[test]
    fn hook_ask_disconnect_says_it_ended_elsewhere() {
        let (agent, _dir) = hook_agent("");
        let session = format!("{}-g7", agent.tmux_name());
        write_meta(&agent, &session, "oc_hook_ask_void");
        let (core, rx) = hook_core(&agent);
        let CommandOutcome::Blocked(gate_id) = core.submit(hook_ask(
            &agent.name,
            &session,
            hook_request(
                "AskUserQuestion",
                serde_json::json!({"questions": [{"question": "Q?", "options": [{"label": "A"}, {"label": "B"}]}]}),
                None,
            ),
        )) else {
            panic!("应阻塞");
        };
        assert!(matches!(rx.try_recv(), Ok(DispatchMsg::ChatAskCard { .. })));
        assert!(core.bind_gate_message(&gate_id, "om_q"));
        let gone = std::sync::atomic::AtomicBool::new(true);
        let _ = core.wait_gate_cancellable(&gate_id, &gone, None);
        match rx.try_recv() {
            Ok(DispatchMsg::ChatAskInvalidated { reason, .. }) => assert_eq!(
                reason,
                crate::runtime::daemon::ChatAskEndReason::ResolvedElsewhere
            ),
            Ok(other) => panic!("应作废提问卡,得到 {}", other.kind_label()),
            Err(e) => panic!("应作废提问卡: {e}"),
        }
    }
}
