//! fleet 快照(纯逻辑):Menu Bar / `agents status` 看到的「谁在跑、谁空闲、daemon 起来多久」。
//!
//! 为什么放 core:快照是「盘上会话事实 + daemon 在跑的轮」的归并决策,与 tmux/socket/时钟无关,
//! 抽成纯函数才能脱离 daemon 单测。IO(会话发现、control.sock 问询)留在 summon 侧。
//!
//! 为什么不是 liveness:tmux 会话是持久层(跨 daemon 重启存活),claude 类 agent 的 `LiveTmux`
//! 几乎恒为 true —— 拿它当状态,「活跃数」会恒等于 agent 总数,零信息量。有信息量的真值只有
//! 「此刻在不在跑一轮」(busy),它只存在于 daemon 内存里。
//!
//! 「在不在列表里」是另一条事实:会话还活着才列出。claude 的会话活着 = tmux 在,盘上查得到;
//! 无头会话(codex / traex / cursor)活着 = 会话进程在,**只有 daemon 知道**(RPC 会话池在它内存里,
//! daemon 一重启进程全没了,盘上事件却还新鲜)。所以无头会话的存活也随运行时真值下发,见
//! [`RuntimeStatus::live_headless`] 与 [`is_listed`]。

use crate::observe::credential_identity::AgentCredentialIdentity;
use crate::session::session_view::{
    elapsed_secs_clamped, humanize_elapsed_secs, humanize_elapsed_secs_en, SessionView,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::SystemTime;

/// daemon 自报的运行时真值(经 control.sock `Intent::RuntimeStatus` 取)。
/// 故意最小:只含「唯有 daemon 才知道」的东西 —— 在跑的轮、进程还活着的无头会话、自己起来多久。
/// 会话发现是盘上事实,CLI 自己读,不必让 daemon 代劳(这样 daemon 不在时的降级是天然的)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub pid: u32,
    pub version: Option<String>,
    pub uptime_secs: u64,
    pub busy: Vec<BusyTurn>,
    /// 本 daemon epoch 为各 agent 冻结的飞书身份指纹(**只有摘要,没有凭据**)。
    /// `doctor` 拿它与盘上重算的指纹比对,判「daemon 用的还是不是这份凭据」。
    /// `serde(default)`:旧 daemon 不带该字段时反序列化成空 → 判定降级为「无法确认」,不假绿。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<AgentCredentialIdentity>,
    /// 会话进程此刻还活着的无头会话(RPC 会话池里进程在跑的那些)。
    ///
    /// **`None` 与 `Some(空)` 是两回事,所以是 `Option` 而不是缺省成空的 `Vec`**:
    /// `None` = 旧 daemon 没带这个字段,无头会话退回盘上判据;`Some(空)` = daemon 明确报告
    /// 一个都没有(比如刚重启),此时不在跑一轮的无头会话一律按已退出处理。缺省成空会让新 CLI
    /// 连旧 daemon 时把所有无头会话从列表里抹掉。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_headless: Option<Vec<LiveSession>>,
    /// 有活跃子 agent / workflow 的会话,不论此刻在不在跑一轮(异步派出的子 agent 在主轮收尾后
    /// 接着跑)。可见状态上它们算 Working;`busy` 仍只指在跑一轮,打断与停机提示针对的是轮。
    /// 旧 daemon 不带该字段,缺省为空,退回只按轮判。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<BusyTurn>,
}

/// 一条在跑的轮:哪个 agent 的哪个 chat。与 summon 侧 `TurnKey` 同构(它是进程内类型,不带 serde)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BusyTurn {
    pub agent: String,
    pub tag: String,
}

/// 一条会话进程还活着的无头会话:哪个 agent 的哪个 chat。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LiveSession {
    pub agent: String,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatStatus {
    /// chat-tag(g/p+hex)。
    pub tag: String,
    /// `agents open` 的目标(tmux 逻辑名);人读名字缺失时也能靠它定位。
    pub target: String,
    /// 群名/备注;无则 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// p2p / group / topic_group；旧 meta 缺失时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_type: Option<String>,
    /// 原始 chat_id(`.meta` 里那份;threads-only 孤儿无 `.meta` → None)。
    ///
    /// **为什么快照里要带它**:[`Self::tag`] 是 chat_id 的 FNV 哈希,**不可逆** —— 而管家要把
    /// 「退出这条会话」路由回目标 agent 的 dispatcher 时,拿的必须是 chat_id 本身。没有它,
    /// 一张只读快照就永远只能看不能动。缺失(None)= 这条会话不可操作,展示端据此不渲操作按钮。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub busy: bool,
    /// "transcript" | "subprocess" | "cli_session" | "rpc" —— 决定 open 是 attach 还是 tail
    /// (只有 transcript 能 attach;其余都是无头,只能 tail)。
    pub capture: String,
    /// live / recent / stale(与 `agents ls` 同一套标签)。
    pub liveness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_secs: Option<u64>,
    /// 与 `last_active_secs` 同源的人读串(`humanize_elapsed_secs` 渲染)。
    ///
    /// 为什么下发文案而不是让各端自己格式化:Menu Bar(Swift)与控制台(JS)各手抄过一份分桶,
    /// 漂了没人察觉(JS 曾多一个空格、缺 >30d 档,还就地 `.replace` 打补丁)。文案由 Rust 渲染,
    /// 消费者只负责显示,三份手抄降成一份事实源。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_text: Option<String>,
    /// 英文版「多久前」(Menu Bar 全英文;中文版留给飞书侧)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_text_en: Option<String>,
    /// 这条会话处于 waiting:agent 回完、轮到 owner 处理。来自盘上标记,见 `waiting` 模块。
    #[serde(default)]
    pub waiting: bool,
    /// 可见状态。**chat 级永远不会是 `Exited`** —— 已退出的会话在 [`build_agent`] 里就被剔除了
    /// (判据见 [`is_listed`]),能出现在这个列表里的会话按定义都还活着。保留同一个枚举是为了
    /// 三端只认一套词。
    pub state: VisibleState,
    /// 这是一条「tmux 开着但没在跑」的会话(Transcript 通道 + live)。展示端据此明示
    /// 「Tmux 空闲」而不是笼统的「空闲」—— 这条会话 attach 上去就能接着用,与无头空闲不同。
    pub idle_tmux: bool,
}

/// 一个 agent / 会话 / 群此刻的可见状态。**判据只依赖「在干活」与会话存活两条已有事实**,
/// 这里做的只是把推导收口到一处。在干活 = 在跑一轮,或主轮已收尾、子 agent 还在跑。
///
/// why 下沉到 Rust:2026-08-08 Menu Bar 升级出三态时,推导写在了 Swift 里;而 `last_active_text`
/// 早就为同一类问题留过教训 —— Swift 与 JS 各手抄过一份分桶、漂了没人察觉。飞书端是第三个消费者,
/// 再抄一份就是三份手抄,而且三个端不会有人同时看,漂了更难发现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VisibleState {
    /// 此刻正在跑一轮,或它派出的子 agent 还在跑。tmux 存活不算在干活。
    Working,
    /// 没有在跑的轮,但会话还在。
    Idle,
    /// 已无存活会话。现有契约保存不了「从未启动 / 曾启动后退出」的可靠历史,
    /// 所以这一档诚实合并两种情况,由展示端渲成「未启动/已退出」。
    Exited,
    /// **路由已退休,进程却还在** —— teardown 失败/泄漏。独立成一档,因为它在各展示端必须
    /// 长得和 `Idle` 不一样:合进 Idle 就等于把事故伪装成"空闲会话",人扫一眼永远不会去点它。
    /// owner 2026-08-13 拍板「别藏事故,要显眼」。
    Leaked,
}

impl VisibleState {
    /// 泄漏优先于一切:它是事故,盖过 working/idle 的正常语义。其次在干活,最后「还有会话吗」。
    fn of(working: bool, has_live_session: bool) -> Self {
        Self::of_with_leak(working, has_live_session, false)
    }

    /// 折叠视图的聚合:`Leaked > Working > Idle > Exited`。
    ///
    /// **泄漏必须压过一切**——agent/group 行是折叠态,人可能根本不展开看下面的 chat。若一个 agent
    /// 名下有一条泄漏会话、其余空闲,聚合成 `Idle` 就等于把事故折叠没了,和当初把它并进 `Gone`
    /// 是同一个错误换了个地方犯。
    pub(crate) fn merge(states: impl Iterator<Item = VisibleState>) -> Option<Self> {
        states.fold(None, |acc, s| match (acc, s) {
            (Some(Self::Leaked), _) | (_, Self::Leaked) => Some(Self::Leaked),
            (Some(Self::Working), _) | (_, Self::Working) => Some(Self::Working),
            (Some(Self::Idle), _) | (_, Self::Idle) => Some(Self::Idle),
            (Some(a), _) => Some(a),
            (None, s) => Some(s),
        })
    }

    /// `leaked` = 该会话是 `ClosedButProcessLive`。单独开一个入口而不是塞进 `of`,是为了让所有
    /// 既有调用点保持逐字等价,只有真正拿到泄漏事实的地方才走新分支。
    fn of_with_leak(working: bool, has_live_session: bool, leaked: bool) -> Self {
        if leaked {
            Self::Leaked
        } else if working {
            Self::Working
        } else if has_live_session {
            Self::Idle
        } else {
            Self::Exited
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMemberStatus {
    pub alias: String,
    pub target: String,
    pub busy: bool,
    pub capture: String,
    pub liveness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_text: Option<String>,
    /// 同 [`ChatStatus::last_active_text_en`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_text_en: Option<String>,
    /// 同 [`ChatStatus::waiting`]。
    #[serde(default)]
    pub waiting: bool,
    /// 同 [`ChatStatus::state`](成员就是某个 agent 在这个群里的那条会话)。
    pub state: VisibleState,
    /// 同 [`ChatStatus::idle_tmux`]。
    pub idle_tmux: bool,
}

/// 同一个 chat 在不同 agent 下的会话聚合。身份键只能是 tag，name 只用于显示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupStatus {
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_type: Option<String>,
    pub busy: bool,
    pub agents: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_text_en: Option<String>,
    /// 原始 chat_id(取自成员会话的 `.meta`,同一个群的成员都相同);拼群链接(`link`)用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// 打开这个会话的飞书桌面版链接(不定位到消息)。由 CLI 按飞书域名填。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// 群级状态:任一成员在干活 → Working;没有成员 → Exited;否则 Idle。
    pub state: VisibleState,
    /// 群里有没有「tmux 开着但空闲」的成员。
    pub idle_tmux: bool,
    pub members: Vec<GroupMemberStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub alias: String,
    /// 该 agent 任一 chat 在跑 → busy。
    pub busy: bool,
    /// 展示中的会话数(不含 Gone 残骸)。
    pub sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_text_en: Option<String>,
    /// agent 级状态:任一会话在干活 → Working;一条存活会话都没有 → Exited;否则 Idle。
    pub state: VisibleState,
    /// 该 agent 有没有「tmux 开着但空闲」的会话。
    pub idle_tmux: bool,
    pub chats: Vec<ChatStatus>,
}

/// 文档评论任务的只读投影，不包含评论正文或回复草稿。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentTask {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub stage: String,
    pub status: String,
    pub status_en: String,
    pub busy: bool,
    pub received_ms: i64,
    pub updated_ms: i64,
    pub pid: Option<i32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSnapshot {
    #[serde(default)]
    pub comment_tasks: Vec<CommentTask>,
    #[serde(default)]
    pub comment_tasks_error: Option<String>,
    pub daemon: DaemonStatus,
    pub agents: Vec<AgentStatus>,
    pub groups: Vec<GroupStatus>,
    /// owner sentinel 三态徽标:"off" / "dry-run" / "on"。CLI 侧填值(读 config),
    /// Menu Bar / Electron 只显示;旧消费端按缺省兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentinel: Option<String>,
    /// waiting 的 (agent, 会话),等得最久的在前;会话已退出的也在列。
    #[serde(default)]
    pub waiting: Vec<crate::observe::waiting::WaitingStatus>,
    /// Menu Bar 状态栏用哪个 summon 图标(`menubar.icon`,CLI 填值;与 `sentinel` 同一做法)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menubar_icon: Option<String>,
    /// Menu Bar 状态栏要不要显示计数(`menubar.show_counts`,CLI 填值;与 `menubar_icon` 同一做法)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menubar_show_counts: Option<bool>,
}

impl FleetSnapshot {
    /// 在跑的 agent 数(`summon agents status` 的人读抬头)。
    pub fn busy_agents(&self) -> usize {
        self.agents.iter().filter(|a| a.busy).count()
    }

    /// working:此刻在干活的 (agent, 会话) 数 —— 在跑一轮,或主轮已收尾、子 agent 还在跑。
    /// 按会话数:一个 agent 同时在三个群干活就是 3。
    pub fn working_sessions(&self) -> usize {
        self.agents
            .iter()
            .map(|a| {
                a.chats
                    .iter()
                    .filter(|c| c.state == VisibleState::Working)
                    .count()
            })
            .sum()
    }
}

/// 盘上会话事实 + daemon 运行时真值 → fleet 快照。
///
/// - `sessions`:每个 agent(registry 全集,含无会话的)的会话列表。已退出的会话会被剔除
///   (判据见 [`is_listed`])—— 它们是死历史,展示只会混淆(与 `agents ls` 同一取舍)。
/// - `runtime`:`None` = daemon 不在(降级:busy 全 false、无 uptime、无头会话按盘上事件判),
///   快照依然可用。
pub fn build_fleet(
    sessions: &[(String, Vec<SessionView>)],
    runtime: Option<&RuntimeStatus>,
    now: SystemTime,
) -> FleetSnapshot {
    let daemon = match runtime {
        Some(rt) => DaemonStatus {
            running: true,
            pid: Some(rt.pid),
            version: rt.version.clone(),
            uptime_secs: Some(rt.uptime_secs),
        },
        None => DaemonStatus {
            running: false,
            pid: None,
            version: None,
            uptime_secs: None,
        },
    };
    let mut agents: Vec<AgentStatus> = sessions
        .iter()
        .map(|(alias, views)| build_agent(alias, views, runtime, now))
        .collect();
    // busy 排前(眼睛先看到在干活的),其次最近活跃,最后按名字稳定排序。
    agents.sort_by(|a, b| {
        b.busy
            .cmp(&a.busy)
            .then(sort_key(a.last_active_secs).cmp(&sort_key(b.last_active_secs)))
            .then(a.alias.cmp(&b.alias))
    });

    let groups = build_groups(&agents);

    FleetSnapshot {
        comment_tasks: Vec::new(),

        comment_tasks_error: None,
        sentinel: None,
        daemon,
        agents,
        groups,
        waiting: Vec::new(),
        menubar_icon: None,
        menubar_show_counts: None,
    }
}

/// 这条会话进不进 fleet 列表 —— 也就是「它还活着吗」。fleet 快照与 Menu Bar 结束会话后的
/// 「会话消失了没有」都按这一处判,两边看到的是同一个列表。
///
/// - 先过 `Liveness::should_display(true)`:Gone 不列,泄漏恒列。
/// - claude(Transcript)到此为止:tmux 在就是活着,发现层已经查过。
/// - 无头(Rpc / CliSession)还要 daemon 作证:此刻在跑一轮,或会话进程还活着。证不了就按已退出
///   处理,与 claude 的 tmux 没了同一个结果。盘上事件新鲜不算 —— daemon 一重启 RPC 会话进程就全没了,
///   事件文件却还是刚写的。cli-session(cursor `-p`)每轮一个进程、轮间没有常驻进程,所以它只在
///   跑一轮时列出:轮间的它与「退出、保留续聊」是同一个状态,下一条消息照样 `--resume` 接回。
/// - daemon 不在(`runtime = None`)或旧 daemon 没带 [`RuntimeStatus::live_headless`]:拿不到
///   进程事实,无头会话退回盘上判据(`recent` / `stale` 照列),降级只往「多列」那边倒。
pub fn is_listed(alias: &str, view: &SessionView, runtime: Option<&RuntimeStatus>) -> bool {
    use crate::config::registry::CaptureMode;
    if !view.liveness.should_display(true) {
        return false;
    }
    match view.capture {
        CaptureMode::Transcript => true,
        CaptureMode::Rpc | CaptureMode::CliSession => {
            let Some(rt) = runtime else {
                return true;
            };
            let Some(live) = rt.live_headless.as_deref() else {
                return true;
            };
            is_busy(rt, alias, &view.tag)
                || live.iter().any(|s| s.agent == alias && s.tag == view.tag)
        }
    }
}

fn is_busy(runtime: &RuntimeStatus, alias: &str, tag: &str) -> bool {
    runtime
        .busy
        .iter()
        .any(|b| b.agent == alias && b.tag == tag)
}

fn has_active_subagents(runtime: &RuntimeStatus, alias: &str, tag: &str) -> bool {
    runtime
        .subagents
        .iter()
        .any(|b| b.agent == alias && b.tag == tag)
}

fn build_agent(
    alias: &str,
    views: &[SessionView],
    runtime: Option<&RuntimeStatus>,
    now: SystemTime,
) -> AgentStatus {
    let mut chats: Vec<ChatStatus> = views
        .iter()
        .filter(|v| is_listed(alias, v, runtime))
        .map(|v| {
            let last_active_secs = v.last_active.map(|t| elapsed_secs_clamped(t, now));
            let busy = runtime.is_some_and(|rt| is_busy(rt, alias, &v.tag));
            let working = busy || runtime.is_some_and(|rt| has_active_subagents(rt, alias, &v.tag));
            let capture = capture_label(v.capture).to_string();
            let liveness = v.liveness.label().to_string();
            // 能走到这里的会话都还活着(上面 filter 过),所以 has_live_session 恒 true。
            // 泄漏态必须走独立档:合进 Idle 就是把 teardown 事故渲成"空闲会话"。
            let leaked = v.liveness == crate::session_view::Liveness::ClosedButProcessLive;
            let state = VisibleState::of_with_leak(working, true, leaked);
            ChatStatus {
                tag: v.tag.clone(),
                target: v.session.clone(),
                name: v.chat_name.clone(),
                chat_type: v.chat_type.clone(),
                chat_id: v.chat_id.clone(),
                idle_tmux: is_idle_tmux(state, &capture, &liveness),
                state,
                busy,
                capture,
                liveness,
                last_active_text: last_active_secs.map(humanize_elapsed_secs),
                last_active_text_en: last_active_secs.map(humanize_elapsed_secs_en),
                waiting: false,
                last_active_secs,
            }
        })
        .collect();
    chats.sort_by(|a, b| {
        b.busy
            .cmp(&a.busy)
            .then(sort_key(a.last_active_secs).cmp(&sort_key(b.last_active_secs)))
            .then(a.tag.cmp(&b.tag))
    });

    let last_active_secs = chats.iter().filter_map(|c| c.last_active_secs).min();
    let busy_any = chats.iter().any(|c| c.busy);
    let working_any = chats.iter().any(|c| c.state == VisibleState::Working);
    AgentStatus {
        alias: alias.to_string(),
        busy: busy_any,
        sessions: chats.len(),
        last_active_secs,
        last_active_text: last_active_secs.map(humanize_elapsed_secs),
        last_active_text_en: last_active_secs.map(humanize_elapsed_secs_en),
        // 一条存活会话都没有 = Exited(界面渲「未启动/已退出」);现有契约区分不了这两者。
        // 但**泄漏要向上传播**:折叠行若把它聚合成 Idle,事故就在折叠态里消失了。
        state: VisibleState::merge(chats.iter().map(|c| c.state))
            .filter(|s| *s == VisibleState::Leaked)
            .unwrap_or_else(|| VisibleState::of(working_any, !chats.is_empty())),
        idle_tmux: chats.iter().any(|c| c.idle_tmux),
        chats,
    }
}

#[derive(Default)]
struct GroupAccumulator {
    name: Option<String>,
    name_age: Option<u64>,
    chat_type: Option<String>,
    chat_id: Option<String>,
    members: Vec<GroupMemberStatus>,
}

fn build_groups(agents: &[AgentStatus]) -> Vec<GroupStatus> {
    let mut by_tag: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    for agent in agents {
        for chat in &agent.chats {
            let group = by_tag.entry(chat.tag.clone()).or_default();
            if group.chat_type.is_none() {
                group.chat_type.clone_from(&chat.chat_type);
            }
            if group.chat_id.is_none() {
                group.chat_id.clone_from(&chat.chat_id);
            }
            if let Some(name) = chat
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
            {
                let should_replace = group.name.is_none()
                    || sort_key(chat.last_active_secs) < sort_key(group.name_age);
                if should_replace {
                    group.name = Some(name.to_string());
                    group.name_age = chat.last_active_secs;
                }
            }
            group.members.push(GroupMemberStatus {
                alias: agent.alias.clone(),
                target: chat.target.clone(),
                busy: chat.busy,
                capture: chat.capture.clone(),
                liveness: chat.liveness.clone(),
                last_active_secs: chat.last_active_secs,
                last_active_text: chat.last_active_text.clone(),
                last_active_text_en: chat.last_active_text_en.clone(),
                waiting: chat.waiting,
                // 成员就是那条 chat,状态照抄 —— 不重算,免得两处判据漂开。
                state: chat.state,
                idle_tmux: chat.idle_tmux,
            });
        }
    }

    let mut groups: Vec<GroupStatus> = by_tag
        .into_iter()
        .map(|(tag, mut group)| {
            group.members.sort_by(|a, b| {
                b.busy
                    .cmp(&a.busy)
                    .then(sort_key(a.last_active_secs).cmp(&sort_key(b.last_active_secs)))
                    .then(a.alias.cmp(&b.alias))
            });
            let last_active_secs = group
                .members
                .iter()
                .filter_map(|m| m.last_active_secs)
                .min();
            let busy_any = group.members.iter().any(|m| m.busy);
            let working_any = group
                .members
                .iter()
                .any(|m| m.state == VisibleState::Working);
            GroupStatus {
                tag,
                name: group.name,
                chat_type: group.chat_type,
                busy: busy_any,
                agents: group.members.len(),
                last_active_secs,
                last_active_text: last_active_secs.map(humanize_elapsed_secs),
                last_active_text_en: last_active_secs.map(humanize_elapsed_secs_en),
                chat_id: group.chat_id,
                link: None,
                state: VisibleState::merge(group.members.iter().map(|m| m.state))
                    .filter(|s| *s == VisibleState::Leaked)
                    .unwrap_or_else(|| VisibleState::of(working_any, !group.members.is_empty())),
                idle_tmux: group.members.iter().any(|m| m.idle_tmux),
                members: group.members,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.busy
            .cmp(&a.busy)
            .then(sort_key(a.last_active_secs).cmp(&sort_key(b.last_active_secs)))
            .then(a.tag.cmp(&b.tag))
    });
    groups
}

/// 「tmux 开着但没在跑」——Transcript 通道 + live 的空闲会话。
///
/// 与无头空闲的差别不是程度而是可操作性:这条 attach 上去就能接着用,而无头空闲什么也 attach 不了。
/// 界面把它单独标出来,是因为它是**用户下一步真能做点什么**的那一档。
fn is_idle_tmux(state: VisibleState, capture: &str, liveness: &str) -> bool {
    matches!(state, VisibleState::Idle) && capture == "transcript" && liveness == "live"
}

fn capture_label(capture: crate::config::registry::CaptureMode) -> &'static str {
    match capture {
        crate::config::registry::CaptureMode::Transcript => "transcript",
        crate::config::registry::CaptureMode::CliSession => "cli-session",
        crate::config::registry::CaptureMode::Rpc => "rpc",
    }
}

/// 「从没活跃过」排在所有活跃过的之后(而不是当成 0 秒前排最前)。
fn sort_key(last_active_secs: Option<u64>) -> u64 {
    last_active_secs.unwrap_or(u64::MAX)
}

/// daemon uptime 的人读串:`3天2小时` / `5小时12分` / `42分钟` / `30秒`。
/// 只取两级(天时 / 时分 / 分 / 秒)——菜单一行放得下,再细也没人读。
pub fn format_uptime(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s >= DAY => {
            let (d, h) = (s / DAY, (s % DAY) / HOUR);
            if h == 0 {
                summon_i18n::format!("rddf93a56c6")
            } else {
                summon_i18n::format!("re5663795c4")
            }
        }
        s if s >= HOUR => {
            let (h, m) = (s / HOUR, (s % HOUR) / MIN);
            if m == 0 {
                summon_i18n::format!("r182536a767")
            } else {
                summon_i18n::format!("ra54a15bf0b")
            }
        }
        s if s >= MIN => summon_i18n::format!("re33716e9e8", s / MIN),
        s => summon_i18n::format!("r9cd5816d64"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::registry::CaptureMode;
    use crate::session_view::Liveness;
    use std::time::Duration;

    fn view(tag: &str, capture: CaptureMode, liveness: Liveness, age: Option<u64>) -> SessionView {
        SessionView {
            agent: "starlight".into(),
            tag: tag.into(),
            session: format!("soul-starlight-{tag}"),
            chat_id: Some(format!("oc_{tag}")),
            chat_type: Some("group".into()),
            chat_name: Some("研发群".into()),
            capture,
            liveness,
            last_active: age.map(|s| base_now() - Duration::from_secs(s)),
        }
    }

    /// ④ 契约:泄漏必须**从 chat 传播到 agent 行**。
    ///
    /// agent/group 是折叠态,人可能根本不展开看下面的 chat。一个 agent 名下有一条泄漏会话、
    /// 其余空闲,若聚合成 Idle,事故就在折叠层消失了 —— 和当初把它并进 `Gone` 是同一个错误
    /// 换了个地方犯。所以聚合优先级是 `Leaked > Working > Idle > Exited`。
    #[test]
    fn a_leaked_chat_makes_the_whole_agent_row_show_the_leak() {
        let views = [
            view("g1", CaptureMode::Transcript, Liveness::LiveTmux, Some(5)),
            view(
                "g2",
                CaptureMode::Transcript,
                Liveness::ClosedButProcessLive,
                Some(9),
            ),
        ];
        let agent = build_agent("starlight", &views, None, base_now());
        assert_eq!(
            agent.state,
            VisibleState::Leaked,
            "一条泄漏就该把 agent 行标成泄漏,不能被其余正常会话稀释掉"
        );
        // 且 chat 层自己也要标对(折叠展开后看到的是这一层)。
        let leaked: Vec<_> = agent
            .chats
            .iter()
            .filter(|c| c.state == VisibleState::Leaked)
            .collect();
        assert_eq!(leaked.len(), 1, "只有那条泄漏会话该标 Leaked");
        assert_eq!(leaked[0].tag, "g2");
    }

    /// 没有泄漏时聚合行为**逐字不变**:busy 优先,否则有会话即 Idle。防止上面那条传播规则
    /// 顺手改坏了既有语义。
    #[test]
    fn without_a_leak_agent_aggregation_is_unchanged() {
        let views = [
            view("g1", CaptureMode::Transcript, Liveness::LiveTmux, Some(5)),
            view("g2", CaptureMode::Rpc, Liveness::IdleHeadless, Some(9)),
        ];
        let agent = build_agent("starlight", &views, None, base_now());
        assert_eq!(agent.state, VisibleState::Idle);
    }

    /// 聚合优先级的真值表(纯函数,不依赖构造 SessionView)。
    #[test]
    fn merge_ranks_leaked_above_everything_else() {
        use VisibleState::*;
        assert_eq!(
            VisibleState::merge([Idle, Leaked, Working].into_iter()),
            Some(Leaked)
        );
        assert_eq!(
            VisibleState::merge([Exited, Working, Idle].into_iter()),
            Some(Working)
        );
        assert_eq!(VisibleState::merge([Exited, Idle].into_iter()), Some(Idle));
        assert_eq!(VisibleState::merge([Exited].into_iter()), Some(Exited));
        assert_eq!(VisibleState::merge([].into_iter()), None);
    }

    fn base_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    /// 新 daemon 的运行时真值:在跑的轮 + 一条无头会话进程都没有。
    fn runtime(busy: &[(&str, &str)]) -> RuntimeStatus {
        runtime_with_live(busy, &[])
    }

    /// 同 [`runtime`],另报这些无头会话的进程还活着。
    fn runtime_with_live(busy: &[(&str, &str)], live: &[(&str, &str)]) -> RuntimeStatus {
        RuntimeStatus {
            pid: 4242,
            version: Some("0.1.0".into()),
            uptime_secs: 269_400,
            busy: busy
                .iter()
                .map(|(a, t)| BusyTurn {
                    agent: (*a).into(),
                    tag: (*t).into(),
                })
                .collect(),
            credentials: Vec::new(),
            live_headless: Some(
                live.iter()
                    .map(|(a, t)| LiveSession {
                        agent: (*a).into(),
                        tag: (*t).into(),
                    })
                    .collect(),
            ),
            subagents: Vec::new(),
        }
    }

    /// 旧 daemon:不带 `live_headless` 字段。
    fn legacy_runtime(busy: &[(&str, &str)]) -> RuntimeStatus {
        RuntimeStatus {
            live_headless: None,
            ..runtime(busy)
        }
    }

    #[test]
    fn runtime_status_without_credentials_field_still_deserializes() {
        // 版本偏斜:新 CLI 连旧 daemon。缺字段必须降级成空清单(→ 判定「无法确认」),
        // 而不是解析失败让 `agents status` 整体降级成「daemon 未运行」。
        let legacy = r#"{"pid":1,"version":null,"uptime_secs":0,"busy":[]}"#;
        let parsed: RuntimeStatus = serde_json::from_str(legacy).unwrap();
        assert!(parsed.credentials.is_empty());
    }

    /// 主轮已收尾、异步派出的子 agent 还在跑:会话、agent、群都算 Working,`busy` 仍是 false
    /// —— 打断与停机提示只针对在跑的轮。
    #[test]
    fn a_chat_whose_subagents_are_still_running_is_working_but_not_busy() {
        let sessions = vec![(
            "starlight".to_string(),
            vec![
                view("g1", CaptureMode::Transcript, Liveness::LiveTmux, Some(5)),
                view("g2", CaptureMode::Transcript, Liveness::LiveTmux, Some(9)),
            ],
        )];
        let rt = RuntimeStatus {
            subagents: vec![BusyTurn {
                agent: "starlight".into(),
                tag: "g2".into(),
            }],
            ..runtime(&[])
        };
        let snap = build_fleet(&sessions, Some(&rt), base_now());
        let agent = &snap.agents[0];
        let chat = |tag: &str| agent.chats.iter().find(|c| c.tag == tag).unwrap();
        assert_eq!(chat("g2").state, VisibleState::Working);
        assert!(!chat("g2").busy);
        assert_eq!(chat("g1").state, VisibleState::Idle);
        assert_eq!(agent.state, VisibleState::Working);
        assert!(!agent.busy);
        assert_eq!(
            snap.working_sessions(),
            1,
            "管家卡抬头的「N 个会话工作中」也算上它"
        );
        let group = snap.groups.iter().find(|g| g.tag == "g2").unwrap();
        assert_eq!(group.state, VisibleState::Working);
        assert_eq!(group.members[0].state, VisibleState::Working);
        assert!(!group.busy);
    }

    #[test]
    fn subagents_of_another_agent_in_the_same_chat_do_not_leak() {
        let sessions = vec![(
            "starlight".to_string(),
            vec![view(
                "g1",
                CaptureMode::Transcript,
                Liveness::LiveTmux,
                Some(5),
            )],
        )];
        let rt = RuntimeStatus {
            subagents: vec![BusyTurn {
                agent: "saturn".into(),
                tag: "g1".into(),
            }],
            ..runtime(&[])
        };
        let snap = build_fleet(&sessions, Some(&rt), base_now());
        assert_eq!(snap.agents[0].chats[0].state, VisibleState::Idle);
    }

    #[test]
    fn runtime_status_without_subagents_field_falls_back_to_turns_only() {
        let legacy = r#"{"pid":1,"version":null,"uptime_secs":0,"busy":[]}"#;
        let parsed: RuntimeStatus = serde_json::from_str(legacy).unwrap();
        assert!(parsed.subagents.is_empty());
        let json = serde_json::to_string(&runtime(&[])).unwrap();
        assert!(
            !json.contains("subagents"),
            "空清单不上线,旧 CLI 读到的形态不变"
        );
    }

    #[test]
    fn chat_busy_lifts_agent_to_busy() {
        let sessions = vec![(
            "starlight".to_string(),
            vec![
                view(
                    "g1a2b3c4",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(5),
                ),
                view(
                    "p9f8e7d6",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(900),
                ),
            ],
        )];
        let snap = build_fleet(
            &sessions,
            Some(&runtime(&[("starlight", "g1a2b3c4")])),
            base_now(),
        );

        let agent = &snap.agents[0];
        assert!(agent.busy, "任一 chat 在跑 → agent busy");
        assert_eq!(agent.sessions, 2);
        assert_eq!(snap.busy_agents(), 1);
        // busy 的 chat 排在前
        assert_eq!(agent.chats[0].tag, "g1a2b3c4");
        assert!(agent.chats[0].busy);
        assert!(!agent.chats[1].busy);
    }

    #[test]
    fn busy_turn_of_other_agent_does_not_leak() {
        let sessions = vec![(
            "starlight".to_string(),
            vec![view(
                "g1a2b3c4",
                CaptureMode::Transcript,
                Liveness::LiveTmux,
                Some(5),
            )],
        )];
        // 同 tag 但属于别的 agent —— 不得把 starlight 标成 busy。
        let snap = build_fleet(
            &sessions,
            Some(&runtime(&[("nimbus", "g1a2b3c4")])),
            base_now(),
        );
        assert!(!snap.agents[0].busy);
        assert_eq!(snap.busy_agents(), 0);
    }

    #[test]
    fn same_chat_tag_groups_agents_together() {
        let sessions = vec![
            (
                "starlight".to_string(),
                vec![view(
                    "g1a2b3c4",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(5),
                )],
            ),
            (
                "saturn".to_string(),
                vec![view(
                    "g1a2b3c4",
                    CaptureMode::Rpc,
                    Liveness::RecentHeadless,
                    Some(60),
                )],
            ),
        ];
        let snap = build_fleet(
            &sessions,
            Some(&runtime_with_live(
                &[("starlight", "g1a2b3c4")],
                &[("saturn", "g1a2b3c4")],
            )),
            base_now(),
        );

        assert_eq!(snap.groups.len(), 1);
        let group = &snap.groups[0];
        assert_eq!(group.tag, "g1a2b3c4");
        assert_eq!(group.name.as_deref(), Some("研发群"));
        assert_eq!(group.agents, 2);
        assert!(group.busy);
        assert_eq!(group.last_active_secs, Some(5));
        assert_eq!(group.members[0].alias, "starlight", "busy member 排前");
        assert_eq!(group.members[1].alias, "saturn");
    }

    #[test]
    fn duplicate_group_names_do_not_merge_distinct_tags() {
        let sessions = vec![(
            "starlight".to_string(),
            vec![
                view(
                    "g1111111",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(5),
                ),
                view(
                    "g2222222",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(10),
                ),
            ],
        )];
        let snap = build_fleet(&sessions, Some(&runtime(&[])), base_now());

        assert_eq!(snap.groups.len(), 2);
        assert_eq!(snap.groups[0].name.as_deref(), Some("研发群"));
        assert_eq!(snap.groups[1].name.as_deref(), Some("研发群"));
        assert_ne!(snap.groups[0].tag, snap.groups[1].tag);
    }

    #[test]
    fn live_tmux_alone_is_not_busy() {
        // 前提纠正的回归:tmux 恒 live,但没有在跑的轮 → 必须是 idle。
        let sessions = vec![(
            "starlight".to_string(),
            vec![view(
                "g1a2b3c4",
                CaptureMode::Transcript,
                Liveness::LiveTmux,
                Some(3600),
            )],
        )];
        let snap = build_fleet(&sessions, Some(&runtime(&[])), base_now());
        assert!(!snap.agents[0].busy);
    }

    #[test]
    fn no_daemon_degrades_to_disk_facts() {
        let sessions = vec![(
            "starlight".to_string(),
            vec![view(
                "g1a2b3c4",
                CaptureMode::Transcript,
                Liveness::LiveTmux,
                Some(30),
            )],
        )];
        let snap = build_fleet(&sessions, None, base_now());

        assert!(!snap.daemon.running);
        assert_eq!(snap.daemon.uptime_secs, None);
        assert_eq!(snap.daemon.pid, None);
        // 会话事实仍在(盘上直读),只是 busy 无从得知 → 一律 false。
        assert_eq!(snap.agents[0].sessions, 1);
        assert!(!snap.agents[0].busy);
        assert_eq!(snap.agents[0].last_active_secs, Some(30));
    }

    #[test]
    fn gone_sessions_are_not_shown() {
        let sessions = vec![(
            "starlight".to_string(),
            vec![
                view(
                    "g1a2b3c4",
                    CaptureMode::Transcript,
                    Liveness::Gone,
                    Some(10),
                ),
                view(
                    "p9f8e7d6",
                    CaptureMode::Rpc,
                    Liveness::IdleHeadless,
                    Some(60),
                ),
            ],
        )];
        let snap = build_fleet(&sessions, None, base_now());
        assert_eq!(snap.agents[0].sessions, 1);
        assert_eq!(snap.agents[0].chats[0].tag, "p9f8e7d6");
        assert_eq!(snap.agents[0].chats[0].liveness, "stale");
        assert_eq!(snap.agents[0].chats[0].capture, "rpc");
    }

    #[test]
    fn agent_without_sessions_still_listed() {
        // registry 里注册但从没起过的 agent:Menu Bar 的分母要含它(2/4 的 4)。
        let sessions = vec![("sakura".to_string(), vec![])];
        let snap = build_fleet(&sessions, None, base_now());
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.agents[0].sessions, 0);
        assert!(!snap.agents[0].busy);
        assert_eq!(snap.agents[0].last_active_secs, None);
        assert!(snap.groups.is_empty(), "无会话 agent 不伪造成群组");
    }

    /// working 按会话数,不按 agent 去重:同一个 agent 在两个群各跑一轮就是 2。
    #[test]
    fn working_sessions_counts_each_busy_chat() {
        let sessions = vec![
            (
                "starlight".to_string(),
                vec![
                    view(
                        "g1a2b3c4",
                        CaptureMode::Transcript,
                        Liveness::LiveTmux,
                        Some(5),
                    ),
                    view(
                        "p9f8e7d6",
                        CaptureMode::Transcript,
                        Liveness::LiveTmux,
                        Some(9),
                    ),
                    view(
                        "g5555555",
                        CaptureMode::Transcript,
                        Liveness::LiveTmux,
                        Some(900),
                    ),
                ],
            ),
            (
                "nimbus".to_string(),
                vec![view(
                    "g1111111",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(5),
                )],
            ),
        ];
        let snap = build_fleet(
            &sessions,
            Some(&runtime(&[
                ("starlight", "g1a2b3c4"),
                ("starlight", "p9f8e7d6"),
                ("nimbus", "g1111111"),
            ])),
            base_now(),
        );
        assert_eq!(snap.busy_agents(), 2);
        assert_eq!(snap.working_sessions(), 3);
    }

    #[test]
    fn busy_agents_sort_first_then_recent() {
        let sessions = vec![
            (
                "sakura".to_string(),
                vec![view(
                    "g0000000",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(10),
                )],
            ),
            ("saturn".to_string(), vec![]),
            (
                "nimbus".to_string(),
                vec![view(
                    "g1111111",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(999),
                )],
            ),
        ];
        let snap = build_fleet(
            &sessions,
            Some(&runtime(&[("nimbus", "g1111111")])),
            base_now(),
        );
        let order: Vec<&str> = snap.agents.iter().map(|a| a.alias.as_str()).collect();
        // nimbus busy 排最前;sakura 更近活跃排 saturn(从没活跃)之前。
        assert_eq!(order, vec!["nimbus", "sakura", "saturn"]);
    }

    #[test]
    fn future_last_active_clamps_to_zero_not_none() {
        // 时钟回拨:last_active 在 now 之后。`None` 在本契约里的语义是「从没活跃过」
        // (排序垫底 + 各端显示「无活动记录」),拿它表达回拨等于把「刚动过」谎报成「从没动过」。
        // 统一按 `elapsed_secs_clamped` 钳到 0 =「刚刚」,与 `humanize_since`、外部会话快照同口径。
        let mut v = view(
            "g1a2b3c4",
            CaptureMode::Transcript,
            Liveness::LiveTmux,
            None,
        );
        v.last_active = Some(base_now() + Duration::from_secs(60));
        let snap = build_fleet(&[("starlight".to_string(), vec![v])], None, base_now());
        assert_eq!(snap.agents[0].chats[0].last_active_secs, Some(0));
        assert_eq!(
            snap.agents[0].chats[0].last_active_text.as_deref(),
            Some("刚刚")
        );
        assert_eq!(snap.agents[0].last_active_secs, Some(0));
    }

    #[test]
    fn last_active_text_is_rendered_by_the_single_source_of_truth() {
        // 各端不再自己格式化:secs 在,text 必在,且必须等于 `humanize_elapsed_secs` 的结果。
        let sessions = vec![(
            "starlight".to_string(),
            vec![
                view(
                    "g1a2b3c4",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(45),
                ),
                view("p9f8e7d6", CaptureMode::Rpc, Liveness::RecentHeadless, None),
            ],
        )];
        let snap = build_fleet(&sessions, None, base_now());
        let agent = &snap.agents[0];
        assert_eq!(agent.last_active_text.as_deref(), Some("45s前"));
        assert_eq!(agent.chats[0].last_active_text.as_deref(), Some("45s前"));
        // 从没活跃过 → secs / text 一起缺席(不许一个在一个不在)。
        assert_eq!(agent.chats[1].last_active_secs, None);
        assert_eq!(agent.chats[1].last_active_text, None);
        assert_eq!(snap.groups[0].last_active_text.as_deref(), Some("45s前"));
        assert_eq!(
            snap.groups[0].members[0].last_active_text.as_deref(),
            Some("45s前")
        );
    }

    #[test]
    fn uptime_reads_like_a_human_wrote_it() {
        assert_eq!(format_uptime(30), "30秒");
        assert_eq!(format_uptime(90), "1分钟");
        assert_eq!(format_uptime(3600), "1小时");
        assert_eq!(format_uptime(3600 + 12 * 60), "1小时12分");
        assert_eq!(format_uptime(3 * 86400 + 2 * 3600), "3天2小时");
        assert_eq!(format_uptime(2 * 86400), "2天");
    }

    #[test]
    fn runtime_status_json_round_trip() {
        // control.sock 上的契约:daemon 写、CLI 读,字段名必须稳。
        let rt = runtime(&[("starlight", "g1a2b3c4")]);
        let json = serde_json::to_string(&rt).unwrap();
        assert!(json.contains(r#""uptime_secs":269400"#));
        assert!(json.contains(r#""agent":"starlight""#));
        assert_eq!(serde_json::from_str::<RuntimeStatus>(&json).unwrap(), rt);
    }

    /// 「一个都没有」必须照样上线:省掉空名单,CLI 就会把它读成「旧 daemon 没带」,
    /// 刚重启、一条无头会话进程都没有的 daemon 反而让它们全按盘上判据显示成空闲。
    #[test]
    fn empty_live_headless_list_survives_the_wire() {
        let rt = runtime(&[]);
        let json = serde_json::to_string(&rt).unwrap();
        assert!(json.contains(r#""live_headless":[]"#), "{json}");
        let parsed: RuntimeStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.live_headless, Some(Vec::new()));
    }

    /// 版本偏斜:新 CLI 连旧 daemon。缺字段是「不知道」(`None`),不是「一个都没有」。
    #[test]
    fn runtime_status_without_live_headless_field_means_unknown() {
        let legacy = r#"{"pid":1,"version":null,"uptime_secs":0,"busy":[]}"#;
        let parsed: RuntimeStatus = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.live_headless, None);
    }

    fn agent_named<'a>(snap: &'a FleetSnapshot, alias: &str) -> &'a AgentStatus {
        snap.agents
            .iter()
            .find(|a| a.alias == alias)
            .unwrap_or_else(|| panic!("快照里应有 {alias}"))
    }

    /// owner 2026-10-16 定:会话进程不在的无头会话按已退出处理。daemon 重启后 RPC 会话进程全没了,
    /// 盘上事件却还新鲜(recent)或只是旧了(stale);这些会话与 claude 的 tmux 没了同一个结果:
    /// 不进列表,agent 一条会话都不剩就是 Exited。
    #[test]
    fn headless_sessions_without_a_live_process_are_not_listed() {
        let sessions = vec![(
            "black".to_string(),
            vec![
                view(
                    "g1111111",
                    CaptureMode::Rpc,
                    Liveness::RecentHeadless,
                    Some(5),
                ),
                view(
                    "p2222222",
                    CaptureMode::Rpc,
                    Liveness::IdleHeadless,
                    Some(9_000),
                ),
                view(
                    "p3333333",
                    CaptureMode::CliSession,
                    Liveness::IdleHeadless,
                    Some(9_000),
                ),
            ],
        )];
        let snap = build_fleet(&sessions, Some(&runtime(&[])), base_now());
        let agent = agent_named(&snap, "black");
        assert!(agent.chats.is_empty(), "{:?}", agent.chats);
        assert_eq!(agent.sessions, 0);
        assert_eq!(agent.state, VisibleState::Exited);
        assert!(snap.groups.is_empty(), "已退出的会话不进群组");
    }

    /// 会话进程还活着、没在跑:空闲。名单按 (agent, tag) 对,别的 agent 同一个群的进程不算。
    #[test]
    fn headless_session_with_a_live_process_is_idle() {
        let sessions = vec![(
            "black".to_string(),
            vec![
                view(
                    "g1111111",
                    CaptureMode::Rpc,
                    Liveness::IdleHeadless,
                    Some(9_000),
                ),
                view(
                    "g2222222",
                    CaptureMode::Rpc,
                    Liveness::RecentHeadless,
                    Some(5),
                ),
            ],
        )];
        let snap = build_fleet(
            &sessions,
            Some(&runtime_with_live(
                &[],
                &[("black", "g1111111"), ("ambrose", "g2222222")],
            )),
            base_now(),
        );
        let agent = agent_named(&snap, "black");
        let tags: Vec<&str> = agent.chats.iter().map(|c| c.tag.as_str()).collect();
        assert_eq!(tags, ["g1111111"], "只有进程活着的那条");
        let chat = &agent.chats[0];
        assert_eq!(chat.state, VisibleState::Idle);
        assert!(!chat.idle_tmux, "无头空闲不是 tmux 空闲");
        assert_eq!(chat.liveness, "stale", "liveness 仍是盘上活动的标签");
        assert_eq!(agent.state, VisibleState::Idle);
    }

    /// 在跑一轮的无头会话总在列表里,不管池里有没有它的进程:cli-session 每轮一个进程、
    /// 不进 RPC 会话池,轮间没有常驻进程。所以它只在跑一轮时出现,轮间按已退出处理。
    #[test]
    fn cli_session_is_listed_only_while_running_a_turn() {
        let sessions = vec![(
            "ambrose".to_string(),
            vec![view(
                "g1111111",
                CaptureMode::CliSession,
                Liveness::RecentHeadless,
                Some(5),
            )],
        )];
        let running = build_fleet(
            &sessions,
            Some(&runtime(&[("ambrose", "g1111111")])),
            base_now(),
        );
        let agent = agent_named(&running, "ambrose");
        assert_eq!(agent.sessions, 1);
        assert_eq!(agent.chats[0].state, VisibleState::Working);

        let between = build_fleet(&sessions, Some(&runtime(&[])), base_now());
        let agent = agent_named(&between, "ambrose");
        assert_eq!(agent.sessions, 0);
        assert_eq!(agent.state, VisibleState::Exited);

        // 别的 agent 在同一个群里跑,不算这条会话在跑。
        let other = build_fleet(
            &sessions,
            Some(&runtime(&[("black", "g1111111")])),
            base_now(),
        );
        assert_eq!(agent_named(&other, "ambrose").sessions, 0);
    }

    /// 旧 daemon 不报进程事实:无头会话按盘上事件列出(recent / stale 照列)。
    #[test]
    fn legacy_daemon_keeps_headless_sessions_on_disk_facts() {
        let sessions = vec![(
            "black".to_string(),
            vec![
                view(
                    "g1111111",
                    CaptureMode::Rpc,
                    Liveness::RecentHeadless,
                    Some(5),
                ),
                view(
                    "p2222222",
                    CaptureMode::CliSession,
                    Liveness::IdleHeadless,
                    Some(9_000),
                ),
            ],
        )];
        let snap = build_fleet(&sessions, Some(&legacy_runtime(&[])), base_now());
        let agent = agent_named(&snap, "black");
        assert_eq!(agent.sessions, 2);
        assert!(agent.chats.iter().all(|c| c.state == VisibleState::Idle));
        assert!(snap.daemon.running, "旧 daemon 也是在跑的 daemon");
    }

    /// daemon 不在:同样退回盘上判据。
    #[test]
    fn no_daemon_keeps_headless_sessions_on_disk_facts() {
        let sessions = vec![(
            "black".to_string(),
            vec![view(
                "g1111111",
                CaptureMode::Rpc,
                Liveness::RecentHeadless,
                Some(5),
            )],
        )];
        let snap = build_fleet(&sessions, None, base_now());
        assert_eq!(agent_named(&snap, "black").sessions, 1);
    }

    /// 进程名单只管无头会话:claude 的判据仍是 tmux 在不在,名单里有没有它都不影响。
    /// Gone 的会话也不会因为名单里有它而复活 —— 名单只收窄,不放宽。
    #[test]
    fn live_headless_list_only_narrows_headless_sessions() {
        let sessions = vec![(
            "starlight".to_string(),
            vec![
                view(
                    "g1111111",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    Some(5),
                ),
                view("g2222222", CaptureMode::Transcript, Liveness::Gone, Some(5)),
                view("g3333333", CaptureMode::Rpc, Liveness::Gone, Some(5)),
            ],
        )];
        let snap = build_fleet(
            &sessions,
            Some(&runtime_with_live(
                &[],
                &[("starlight", "g2222222"), ("starlight", "g3333333")],
            )),
            base_now(),
        );
        let tags: Vec<&str> = agent_named(&snap, "starlight")
            .chats
            .iter()
            .map(|c| c.tag.as_str())
            .collect();
        assert_eq!(tags, ["g1111111"]);
    }

    /// 跨语言 golden 的输入。覆盖面是刻意挑的:busy/空闲、群/私聊、四种 capture、
    /// live/recent/stale、群名有无、相对时间各档(刚刚 / s / h / d / >30d)、无会话 agent。
    /// 字段少一个都会让 Swift/JS 的「未知键静默忽略」把改名降级成空白而不是崩溃。
    fn golden_sessions() -> Vec<(String, Vec<SessionView>)> {
        fn v(
            agent: &str,
            tag: &str,
            name: Option<&str>,
            chat_type: &str,
            capture: CaptureMode,
            liveness: Liveness,
            age: u64,
        ) -> SessionView {
            SessionView {
                agent: agent.into(),
                tag: tag.into(),
                session: format!("soul-{agent}-{tag}"),
                chat_id: Some(format!("oc_{tag}")),
                chat_type: Some(chat_type.into()),
                chat_name: name.map(str::to_string),
                capture,
                liveness,
                last_active: Some(base_now() - Duration::from_secs(age)),
            }
        }
        vec![
            (
                "starlight".to_string(),
                vec![
                    v(
                        "starlight",
                        "g1a2b3c4",
                        Some("研发群"),
                        "group",
                        CaptureMode::Transcript,
                        Liveness::LiveTmux,
                        3,
                    ),
                    v(
                        "starlight",
                        "p9f8e7d6",
                        None,
                        "p2p",
                        CaptureMode::Rpc,
                        Liveness::RecentHeadless,
                        3 * 3600 + 120,
                    ),
                ],
            ),
            (
                "saturn".to_string(),
                vec![
                    v(
                        "saturn",
                        "g1a2b3c4",
                        Some("研发群"),
                        "group",
                        CaptureMode::Rpc,
                        Liveness::RecentHeadless,
                        45,
                    ),
                    v(
                        "saturn",
                        "g7777777",
                        Some("运维群"),
                        "group",
                        CaptureMode::Transcript,
                        Liveness::LiveTmux,
                        2 * 86400,
                    ),
                    v(
                        "saturn",
                        "p2222222",
                        None,
                        "p2p",
                        CaptureMode::Rpc,
                        Liveness::IdleHeadless,
                        40 * 86400,
                    ),
                    // codex 原生常驻协议:`capture` 的第三个取值必须真的出现在 golden 里,
                    // 否则 Swift/JS 那边「未知取值静默降级成空白」会一路绿着上线。
                    v(
                        "saturn",
                        "p3333333",
                        None,
                        "p2p",
                        CaptureMode::Rpc,
                        Liveness::RecentHeadless,
                        75,
                    ),
                    // 盘上事件新鲜、会话进程却已经没了(daemon 重启过):不在 [`golden_runtime`]
                    // 的存活名单里,快照里不该有它。golden JSON 里找不到 p4444444 就是这条判据的证据。
                    v(
                        "saturn",
                        "p4444444",
                        None,
                        "p2p",
                        CaptureMode::Rpc,
                        Liveness::RecentHeadless,
                        30,
                    ),
                ],
            ),
            (
                "arcadia".to_string(),
                vec![v(
                    "arcadia",
                    "gdeadbee1",
                    Some("sample开发群"),
                    "group",
                    CaptureMode::Transcript,
                    Liveness::LiveTmux,
                    600,
                )],
            ),
            ("sakura".to_string(), vec![]),
        ]
    }

    /// golden 的运行时真值:starlight 在研发群跑一轮;除 p4444444 外的无头会话进程都还活着。
    fn golden_runtime() -> RuntimeStatus {
        runtime_with_live(
            &[("starlight", "g1a2b3c4")],
            &[
                ("starlight", "p9f8e7d6"),
                ("saturn", "g1a2b3c4"),
                ("saturn", "p2222222"),
                ("saturn", "p3333333"),
            ],
        )
    }

    /// 三态推导的四种形态一次钉死。**这是三端唯一的判据源** —— Swift/JS/飞书都只读下发值,
    /// 所以这一处错了三个界面一起错,而且不会有人同时看到三个界面去发现不一致。
    #[test]
    fn 三态推导覆盖工作空闲tmux空闲与退出() {
        let snap = build_fleet(&golden_sessions(), Some(&golden_runtime()), base_now());
        let agent = |alias: &str| {
            snap.agents
                .iter()
                .find(|a| a.alias == alias)
                .unwrap_or_else(|| panic!("golden 里应有 {alias}"))
        };

        // ① Working:有一条会话在跑。
        let starlight = agent("starlight");
        assert_eq!(starlight.state, VisibleState::Working);
        let running = starlight
            .chats
            .iter()
            .find(|c| c.tag == "g1a2b3c4")
            .expect("在跑的那条");
        assert_eq!(running.state, VisibleState::Working);
        // **在跑的 transcript 会话不算 tmux 空闲** —— idle_tmux 说的是"空着可以接手",
        // 正在跑的会话接手只会打断它。
        assert!(!running.idle_tmux, "busy 的会话不该被标成 tmux 空闲");
        assert!(!starlight.idle_tmux);

        // ② Idle + ③ Tmux 空闲:没在跑,但 transcript 通道且 live → 可以 attach 接着用。
        let arcadia = agent("arcadia");
        assert_eq!(arcadia.state, VisibleState::Idle);
        assert!(
            arcadia.idle_tmux,
            "transcript+live 的空闲会话应标 tmux 空闲"
        );
        let only = &arcadia.chats[0];
        assert_eq!(only.state, VisibleState::Idle);
        assert!(only.idle_tmux);

        // 无头空闲不是 tmux 空闲:它什么也 attach 不了,这个区分正是这一位存在的理由。
        let saturn = agent("saturn");
        let headless_idle = saturn
            .chats
            .iter()
            .find(|c| c.capture == "rpc")
            .expect("rpc 会话");
        assert_eq!(headless_idle.state, VisibleState::Idle);
        assert!(!headless_idle.idle_tmux);

        // ④ Exited:一条存活会话都没有。
        let sakura = agent("sakura");
        assert_eq!(sakura.state, VisibleState::Exited);
        assert!(!sakura.idle_tmux);
        assert!(sakura.chats.is_empty());

        // 群级与成员级同源:成员照抄 chat,群按成员聚合,不各算一套。
        let busy_group = snap
            .groups
            .iter()
            .find(|g| g.tag == "g1a2b3c4")
            .expect("有在跑成员的群");
        assert_eq!(busy_group.state, VisibleState::Working);
        let tmux_group = snap
            .groups
            .iter()
            .find(|g| g.idle_tmux)
            .expect("应有含 tmux 空闲成员的群");
        assert_eq!(tmux_group.state, VisibleState::Idle);
        assert!(tmux_group.members.iter().any(|m| m.idle_tmux));
    }

    /// daemon 不在时不许把「不知道在不在跑」说成 Working。降级只能往保守的方向倒。
    #[test]
    fn daemon不在时没有任何工作中状态() {
        let snap = build_fleet(&golden_sessions(), None, base_now());
        assert!(
            snap.agents.iter().all(|a| a.state != VisibleState::Working),
            "daemon 不在 = 拿不到 busy 事实,不能声称有人在工作"
        );
        assert!(snap.groups.iter().all(|g| g.state != VisibleState::Working));
    }

    /// Rust ↔ Swift ↔ JS 的**共享** golden:三边解同一个文件,谁改字段名谁先红。
    ///
    /// 为什么必须是共享文件而不是各写各的内联 JSON:Swift 的 `Codable`、JS 的取值对未知 key
    /// 都是静默忽略 —— `last_active_secs` 一改名,Menu Bar 全变空白,而三边测试仍然全绿。
    #[test]
    fn fleet_snapshot_matches_cross_language_golden() {
        let mut snap = build_fleet(&golden_sessions(), Some(&golden_runtime()), base_now());
        // sentinel 徽标由 CLI 层填值(build_fleet 不知晓 config);golden 覆盖"有值"形态,
        // 缺席形态由 Swift 的 decodeIfPresent / JS 的可选取值天然兼容。
        snap.sentinel = Some("dry-run".to_string());
        snap.menubar_icon = Some("xyz".to_string());
        snap.comment_tasks.push(CommentTask {
            id: "comment-task".into(),
            agent: "starlight".into(),
            title: "Document review".into(),
            stage: "draft_ready".into(),
            status: "待 owner 批准".into(),
            status_en: "Awaiting approval".into(),
            busy: false,
            received_ms: 1_800_000_000_000,
            updated_ms: 1_800_000_001_000,
            pid: None,
            error: None,
        });
        crate::observe::waiting::attach_waiting(
            &mut snap,
            &golden_sessions(),
            &[(
                "saturn".to_string(),
                "g1a2b3c4".to_string(),
                crate::observe::waiting::WaitingMark {
                    chat_id: "oc_golden".to_string(),
                    reply_message_id: "om_golden_reply".to_string(),
                    link: "lark://applink.feishu.cn/client/chat/open?openChatId=oc_golden&position=1485"
                        .to_string(),
                    since_unix: 1_800_000_000 - 720,
                },
            )],
            1_800_000_000,
        );
        crate::observe::waiting::attach_group_links(&mut snap, "applink.feishu.cn");
        let actual = serde_json::to_string_pretty(&snap).expect("fleet 快照必须可序列化");
        // 逃生舱:`SUMMON_REGEN_GOLDEN=1 cargo test -p summon-core fleet_snapshot_matches` 重生成
        // fixture。手抄 80 行 JSON 必然抄漏,而漏掉的字段在 Swift/JS 那边是静默降级成空白。
        // 默认不写盘(env 不设即纯断言),重生成后仍必须人工 review diff 并同步两个消费者。
        if std::env::var("SUMMON_REGEN_GOLDEN").is_ok() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/fixtures/fleet-status.golden.json"
                ),
                format!("{}\n", actual),
            )
            .unwrap();
        }
        // include_str! 而不是运行时读盘:summon-core 是纯逻辑 crate,且改 fixture 会触发重编译。
        let golden = include_str!("../../../../tests/fixtures/fleet-status.golden.json");
        assert_eq!(
            actual.trim_end(),
            golden.trim_end(),
            "fleet JSON 契约变了。Menu Bar(Swift)与控制台(JS)都按这份 fixture 解码 —— \
             改 schema 必须同步更新 tests/fixtures/fleet-status.golden.json 和两个消费者。"
        );
    }

    #[test]
    fn fleet_snapshot_json_omits_absent_daemon_fields() {
        // Menu Bar 的 Swift 侧按此形态解码:daemon 不在时 pid/version/uptime_secs 整键缺席。
        let snap = build_fleet(&[("sakura".to_string(), vec![])], None, base_now());
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains(r#""running":false"#));
        assert!(!json.contains("uptime_secs"));
        assert!(!json.contains("\"pid\""));
    }
}
