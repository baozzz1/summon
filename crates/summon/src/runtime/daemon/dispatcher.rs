//! per-agent dispatcher(A1 拆分):按 chat_id 分线程路由、空闲扫描、会话镜像。

use super::*;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PwdReplyTarget {
    Denied,
    CurrentChat,
    OperatorDm,
}

pub(super) fn pwd_reply_target(is_group: bool, can_operate: bool) -> PwdReplyTarget {
    if !can_operate {
        PwdReplyTarget::Denied
    } else if is_group {
        PwdReplyTarget::OperatorDm
    } else {
        PwdReplyTarget::CurrentChat
    }
}

/// 工作目录报告只投给选定的接收者；私聊失败时原会话给出不含路径的错误提示。
pub(super) fn deliver_pwd_report(
    fc: &FeishuClient,
    agent: &Agent,
    msg: &Inbound,
    chat_type: &str,
    target: PwdReplyTarget,
    report: &str,
) {
    match target {
        PwdReplyTarget::CurrentChat => {
            let _ = send_markdown_archived_anchored(
                fc,
                &agent.state_dir(),
                chat_type,
                &msg.chat_id,
                command_anchor(msg),
                report,
            );
        }
        PwdReplyTarget::OperatorDm => {
            if let Err(error) = send_markdown_to_open_id_archived(
                fc,
                &agent.state_dir(),
                &msg.sender,
                "pwd",
                report,
            ) {
                log(&summon_i18n::format!(
                    "r096932c349",
                    agent.name,
                    msg.chat_id,
                    msg.sender,
                ));
                let _ = send_markdown_archived_anchored(
                    fc,
                    &agent.state_dir(),
                    chat_type,
                    &msg.chat_id,
                    command_anchor(msg),
                    summon_i18n::text!("r737993733d"),
                );
            }
        }
        PwdReplyTarget::Denied => {}
    }
}

/// `/dash` 之后的话只说给发起人:群里发的私聊他,群里不留任何东西;私聊发不出去才回到原会话。
pub(super) fn deliver_dash_notice(
    fc: &FeishuClient,
    agent: &Agent,
    msg: &Inbound,
    chat_type: &str,
    is_group: bool,
    notice: super::dashboard_entry::DashNotice,
) {
    use super::dashboard_entry::DashNotice;
    let in_chat = |text: &str| {
        let _ = send_markdown_archived_anchored(
            fc,
            &agent.state_dir(),
            chat_type,
            &msg.chat_id,
            command_anchor(msg),
            text,
        );
    };
    match notice {
        DashNotice::Delivered => {}
        DashNotice::ToSender(text) if is_group => {
            if let Err(error) = send_markdown_to_open_id_archived(
                fc,
                &agent.state_dir(),
                &msg.sender,
                "dashboard_notice",
                &text,
            ) {
                log(&summon_i18n::format!(
                    "ra0c2754b85",
                    agent.name,
                    msg.chat_id,
                    msg.sender,
                ));
                in_chat(&text);
            }
        }
        DashNotice::ToSender(text) | DashNotice::InChat(text) => in_chat(&text),
    }
}

fn ack_card_callback(
    card: &CardActionMsg,
    card_actions: &crate::runtime::card_action_wal::CardActionWal,
) {
    let Some(id) = card.wal_id.as_deref() else {
        return;
    };
    if let Err(error) = card_actions.ack(id) {
        // 处理已结束但 ACK 删除失败时保留 WAL；冷启重放比静默丢 callback 更安全。
        log(&summon_i18n::format!("r9be449c842"));
    }
}

pub(super) fn finish_durable_card_callback(
    card: &CardActionMsg,
    card_actions: &crate::runtime::card_action_wal::CardActionWal,
    disposition: CallbackDisposition,
) {
    match disposition {
        CallbackDisposition::Terminal => ack_card_callback(card, card_actions),
        CallbackDisposition::Retry => log(&summon_i18n::format!(
            "re69dd1e1c4",
            card.wal_id.as_deref().unwrap_or("<none>")
        )),
    }
}

/// 定时触发的预告文案:告诉在场的人「接下来这轮是定时任务,是哪一条」。
///
/// **正文要截断**。定时任务的 prompt 可以是整段工作流(几百上千字),原样贴进群里就是刷屏,
/// 而这张卡只需要让人认出是哪条任务——完整正文马上会体现在 agent 自己的回复里。
/// 按**字符**而不是字节截断:中文 prompt 按字节切会切碎多字节序列。
pub(super) fn scheduled_fire_notice(prompt: &str) -> String {
    const MAX_CHARS: usize = 200;
    let body = prompt.trim();
    if body.is_empty() {
        return summon_i18n::text!("rec99628983").to_string();
    }
    let total = body.chars().count();
    if total <= MAX_CHARS {
        return summon_i18n::format!("r4dceea5c9f");
    }
    let head: String = body.chars().take(MAX_CHARS).collect();
    summon_i18n::format!("ree52bba0c6")
}

/// 是否该对某 chat 发空闲提示:超过 ttl 未活动 + 尚未提示过 + **此刻没有活跃 subagent/workflow**
/// + **此刻没有活跃 turn**。now<last 视作未到点。
///
/// 三种捕获模式一视同仁。卡问的是「这条会话怎么处理」,而每种模式都有东西可处理:Transcript
/// 与 Rpc 挂着常驻进程(tmux / app-server、ACP 会话),CliSession 轮外无进程但上下文一直在长。
/// 四个动作里只有 compact 分模式 —— 无头 CLI 由 `CompactSpec::Unsupported` 如实回执自管压缩,
/// 不谎报已压缩。
///
/// `turn_in_flight`:主轮正在跑时这条会话**在工作**,空闲提醒卡只是噪音(owner 2026-08-17 报告:
/// agent 连续输出 55 分钟,空闲提醒仍发出)。与 `should_retire_idle_session` 的 `!turn_in_flight`
/// 同源同语义。`last_activity` 只跟入站活动走,不跟 agent 输出走,所以长跑轮的入站空闲会到点;
/// 必须在这里挡。
///
/// `subagent_active`:主轮结束不等于会话空着 —— 后台 Agent/Workflow 还在跑时这条会话
/// **在工作**,空闲提醒卡只是噪音(owner 2026-08-14 报告)。它是暂态:subagent 结束后
/// 空闲期从其最后活动起算(见 `effective_idle_anchor`),到点照常提醒。观测不可得时调用方
/// 传 false(回退现行为)。
pub(super) fn should_prompt_idle(
    last_activity: std::time::SystemTime,
    now: std::time::SystemTime,
    ttl: Duration,
    idle_prompted: bool,
    turn_in_flight: bool,
    subagent_active: bool,
) -> bool {
    !idle_prompted
        && !turn_in_flight
        && !subagent_active
        && now
            .duration_since(last_activity)
            .map(|d| d > ttl)
            .unwrap_or(false)
}

/// 这条空闲会话该不该自动退出(归档续聊)。
///
/// **这是全仓唯一由扫描线程发起的会话终止**,与 CLAUDE.md 第一红线「回收/空闲扫描从不擅杀」
/// 是 owner 2026-08-12 显式授权的例外。红线仍然管着**怎么杀**:本函数只产出意图,真正的拆除由
/// chat 线程走既有的 `Control::Clean` 路径执行(`teardown.rs` 是白名单内的合法销毁点),
/// dispatcher 自己一行 tmux 销毁代码都不写。
///
/// 判据全部为真才返回 true,**任一条证不了就跳过**——宁可多留一个会话到下轮扫描,不可拆错一个:
/// - `retire_after`:这台机器显式开了自动退出(默认 None = 关,不写配置的机器行为不变);
/// - `session_present`:会话确实还在——不在就没什么可拆,发退出回执只会误导。Transcript 由
///   tmux 探测证明;无头路没有外部持久层,会话的存在就是 chat handle 的存在,调用方传 true;
/// - `resume_enabled` + `has_stored_id`:**「归档续聊」的续得回来是前提**。这两条任缺一条,
///   拆掉就等于把上下文扔了,那是重置不是退出,不在授权范围内;
/// - `!turn_in_flight`:此刻没有活跃 turn 记录。`Control::Clean` 会被 mid-turn 偷看抢占执行,
///   所以「chat 线程是串行的」保护不了正在跑的那一轮,必须在这里挡住;
/// - `!subagent_active`:此刻没有活跃(时效窗内的)后台 subagent/workflow。主轮结束后它们
///   还在跑时,拆会话会连 CLI 进程一起收掉,**在途工作全部丢失**(owner 2026-08-14 报告)。
///   与 turn 同为暂态:结束后空闲期从其最后活动起算,到点照常回收。观测不可得(无观测句柄 /
///   watcher 死了)时调用方传 false 回退现行为 —— 判活自带 2h 时效轴,冻死的 watcher 最多把
///   回收压后一个窗,不会让会话永不回收;
/// - `idle_for >= retire_after`:真的空闲够久。`now < last_activity`(时钟回拨)按未到点处理。
///
/// 判据里没有捕获模式:拆除动作对三种模式都成立且是同一条 —— `Control::Clean` 那条臂先
/// `interrupt_inflight_turn` 收掉常驻 RPC 会话进程并摘池,再走 `do_teardown`(tmux 不在就跳过
/// kill、只记生命周期)。CliSession 轮外没有进程可收,那条臂对它就退化成「退线程、留记录」。
///
/// 「不在停机窗口内」这条不用参数表达:停机是 `DispatchMsg::Shutdown`,收到即 break 主循环,
/// 之后不会再有 idle 扫描 —— 由调用时机保证,加个参数反而假装它是可配的。
#[allow(clippy::too_many_arguments)] // 判据即参数,合并任何两条都会让「哪条没过」不可单测
pub(super) fn should_retire_idle_session(
    last_activity: std::time::SystemTime,
    now: std::time::SystemTime,
    retire_after: Option<Duration>,
    session_present: bool,
    resume_enabled: bool,
    has_stored_id: bool,
    turn_in_flight: bool,
    subagent_active: bool,
) -> bool {
    let Some(retire_after) = retire_after else {
        return false;
    };
    session_present
        && resume_enabled
        && has_stored_id
        && !turn_in_flight
        && !subagent_active
        && now
            .duration_since(last_activity)
            .map(|d| d >= retire_after)
            .unwrap_or(false)
}

/// 这条 chat 此刻有一轮在跑，或主 Agent 让出本轮后起的后台任务还在跑。
pub(super) fn chat_has_running_work(agent: &Agent, tag: &str) -> bool {
    crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
        .has_turn_record(tag)
        || crate::runtime::background_holder::BackgroundHolderStore::for_agent(&agent.state_dir())
            .has_any(tag)
}

/// 空闲时钟的有效起点:入站活动与 subagent 最后活动取**最大**。
///
/// 主轮结束后后台 Agent/Workflow 还在跑时,会话的「最后活跃」应跟着它们走 —— subagent
/// 结束后的空闲期从其最后活动起算,而不是主轮/入站起算。历史 subagent(远早于入站)的旧
/// 时刻被 max 自然淹没,不会把时钟拨回过去;观测不可得(None)= 时钟不动,现行为。
pub(super) fn effective_idle_anchor(
    inbound_last_activity: std::time::SystemTime,
    subagent_last_activity: Option<chrono::DateTime<chrono::Utc>>,
) -> std::time::SystemTime {
    match subagent_last_activity {
        Some(at) => inbound_last_activity.max(std::time::SystemTime::from(at)),
        None => inbound_last_activity,
    }
}

/// 一条会话该用哪个空闲退出窗口。
///
/// 尾轮是定时任务且 `timer_idle_retire_secs` 开着 → 用 timer 窗口(owner 2026-08-13 需求:
/// 定时触发后没人跟进的会话,55min 静默走);其余(人类尾轮 / timer 机制配 0 关闭)→ 沿用
/// 普通 `idle_retire_secs`,行为与本机制引入前一致。窗口只是**换阈值**,判据与执行通路
/// (`should_retire_idle_session` → `Control::Clean` → teardown)一条不减。
pub(super) fn idle_retire_window(
    last_inbound_scheduled: bool,
    timer_retire_after: Option<Duration>,
    retire_after: Option<Duration>,
) -> Option<Duration> {
    if last_inbound_scheduled && timer_retire_after.is_some() {
        timer_retire_after
    } else {
        retire_after
    }
}

/// 定时尾轮的静默回收是否「托管」了这条会话的空闲提醒(true = 到点不发提醒卡)。
///
/// 需求核心是「定时任务会话安静地走」:到点直接归档续聊,不出卡。但压卡的前提是回收
/// **结构上可达** —— timer 机制开着 + 尾轮确实是定时器合成轮(按投递来源记,不看文本)+
/// 既有 retire 的全部**结构**门槛可证:会话确实在、resume 开着、盘上有 session id。
/// 时间未到、turn 在跑是暂态,不影响「终将回收」,不参与本判断;结构门槛任一
/// 证不了 → false,回退旧行为(到点照发提醒卡),绝不让会话进入「既无卡又不回收」的悬空态。
pub(super) fn timer_quiet_owns_idle(
    last_inbound_scheduled: bool,
    timer_retire_after: Option<Duration>,
    session_present: bool,
    resume_enabled: bool,
    has_stored_id: bool,
) -> bool {
    last_inbound_scheduled
        && timer_retire_after.is_some()
        && session_present
        && resume_enabled
        && has_stored_id
}

/// 下一次 `recv_timeout` 该等多久:**距下次扫描的剩余时间**,不是一整个周期。
///
/// why:扫描的触发条件只看 `last_scan.elapsed() >= idle_scan`(见循环尾),但此前唤醒时长固定传
/// `idle_scan` —— 每收到一条消息(哪怕是「未 @ 本 bot」这种当场丢弃的群消息)就把下次醒来推后一整个
/// 周期,于是实际扫描间隔在 [idle_scan, 2×idle_scan) 抖动。R8 修掉的是「扫描挂在 Timeout 分支上被
/// 消息流饿死」,唤醒时机这半边当时没跟上。
///
/// 实测 2026-08-05(arcadia):12:35 扫过一轮,12:36:55 来了条无关群消息,唤醒被推到 12:41:55 ——
/// 间隔 415s 而非 300s,tmux 被外带销毁(用户在会话里敲 `/quit`)的退出回执跟着迟到几分钟。
///
/// 剩余为 0(已到点)返回 `ZERO`:`recv_timeout(ZERO)` 立刻 Timeout,循环尾当轮就扫并重置
/// `last_scan`,不会忙等。
///
/// **这条「不会忙等」依赖一个契约:循环尾一定跑得到。** 只要有 `continue` 绕过循环尾,
/// `last_scan` 就永不重置、超时恒为 0,dispatcher 线程当场 100% 空转 —— 2026-08-10 实测
/// (owner 报 `summond` 占 70% CPU)正是 steward 的消息白名单把 `Err(Timeout)` 也 `continue`
/// 掉了。**往这个循环里加 `continue` 之前,先确认它不会跳过循环尾的扫描与 `last_scan` 重置。**
pub(super) fn next_recv_timeout(idle_scan: Duration, since_last_scan: Duration) -> Duration {
    idle_scan.saturating_sub(since_last_scan)
}

/// 空闲卡上那行 ctx:**发卡这一刻现采**,现采取不到才回落轮末落的 `status_snapshot`。
///
/// 轮末快照看不见轮外发生的上下文变化 —— `/compact` 正是这样一件事:压缩完了,快照还停在
/// 上一轮末的数字,卡上于是报一个已经不成立的占用(owner 2026-09-09 报告)。现采的来源本身
/// 就是实时的:claude 读 statusLine 快照(claude 每渲染一次就写),codex/cursor 读会话池里
/// 最后一次已知的占用。
///
/// 回落这一步不能省:codex/cursor 的实时值只活在会话池的**内存**里,daemon 换代之后是空的,
/// 而轮末快照跨代际存活。都取不到就返回空预算,卡上不渲染 ctx 行。
fn latest_idle_context(
    agent: &Agent,
    tag: &str,
    session: &str,
) -> summon_core::context_budget::ContextBudget {
    if super::cards::is_cursor_cli_session(agent) {
        // CLI 库可直接现采；缺失时也不借用旧的推算快照。
        return super::cards::current_session_context_budget(agent, tag, session);
    }
    let live = super::cards::current_session_context_budget(agent, tag, session);
    if live.used_tokens.is_some() || live.reported_pct.is_some() {
        return live;
    }
    let Some(session_id) =
        crate::session::resume_store::read_id(agent, session, &agent.resume_cli_kind())
    else {
        return live;
    };
    match crate::runtime::status_snapshot::read_context(&agent.state_dir(), tag, &session_id) {
        Ok(context) => context
            .filter(|budget| {
                CliDriver::from_kind(agent.cli_kind()) != CliDriver::Cursor
                    || budget.window_tokens.is_some()
            })
            .unwrap_or(live),
        Err(error) => {
            log(&summon_i18n::format!("r23b4c0c7c8", agent.name));
            live
        }
    }
}

/// 空闲扫描一遍(R8:与消息到达解耦,dispatcher 每 idle_scan 调一次,不再只在 recv_timeout 的 Timeout
/// 分支跑)。三件事:① F3-b 探测 `agents kill`/`down`(独立 CLI 直接 kill tmux、对飞书静默)——只对
/// Transcript(有真 tmux)且「曾观测到 live、本轮又消失」的会话合成 ManualKill 退出回执,**只观测、绝不
/// 杀**(合规 CLAUDE.md:idle 线程从不擅杀);② 到点(idle_ttl)的 Transcript 会话发空闲提醒卡片；
/// ③ 定时尾轮的 RPC 会话在显式打开 reset 时投既有 `Control::Reset` 意图。
///
/// `_migrated` 是纯类型证明,不携带数据:它只能由 `ensure_idle_prompt_marker_migration`
/// 成功构造 ——「空闲卡账本迁移未完成不进扫描」由签名钉死,不靠 boot 里的调用顺序默契
/// (迁移失败的启动拿不到证明,本函数在那个进程里根本不可调用)。
pub(super) fn run_idle_scan(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    effective: &Messaging,
    self_tx: &Sender<DispatchMsg>,
    fc: &FeishuClient,
    _migrated: &IdleMarkersMigrated,
) {
    let now = SystemTime::now();
    // subagent 活跃探针用同一个「现在」(chrono 轴):两个时钟源在同一轮扫描里混用会让
    // 「刚好在窗沿」的判定飘忽。
    let now_utc = chrono::DateTime::<chrono::Utc>::from(now);
    let ttl = effective.idle_ttl();
    let retire_after = effective.idle_retire();
    let timer_retire_after = effective.timer_idle_retire();
    // 只用于 tmux 活性探测与外带 kill 合成回执:无头路没有 tmux,也没有「被别人杀掉」这回事。
    let transcript = matches!(agent.capture_mode(), CaptureMode::Transcript);
    // 自动退出的 per-agent 前提,循环外算一次。短路在前:两种自动退出(普通/定时尾轮)都没开
    // 的机器不为它读配置文件(idle scan 每 idle_scan_secs 跑一遍)。resume 关着 = 拆掉就接
    // 不回来,那是重置不是「归档续聊」,不在授权范围内。
    let resume_enabled = (retire_after.is_some() || timer_retire_after.is_some())
        && agent.resume_enabled(summon_core::config::Config::load().resume_sessions);
    let turns = crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir());
    let holders =
        crate::runtime::background_holder::BackgroundHolderStore::for_agent(&agent.state_dir());
    // (chat_id, 生效窗口秒数, 是否定时尾轮, 定时归档是否重置) —— 窗口随会话变(定时尾轮走 timer
    // 窗口),日志要报真值;重置位决定定时尾轮到点走 Reset(清 resume)还是 Clean(归档续聊)。
    let mut retiring: Vec<(String, u64, bool, bool)> = Vec::new();
    // 静默回收已「托管」空闲处理的 chat:到点不再发提醒卡(需求核心:定时任务会话安静地走)。
    let mut timer_quiet: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut killed_externally: Vec<(String, String, Option<String>, Option<String>)> = Vec::new();
    for (chat_id, h) in chats.iter_mut() {
        // 主 Agent 让出本轮后，后台任务还在跑的会话同样算在跑(拆掉它就是杀掉后台任务)。
        // 先按会话现状清掉已经不在跑的，会话不在了的一并清掉。
        if transcript {
            if let Err(error) = crate::runtime::background_holder::prune_for_session(
                agent,
                &chat_tag(&h.chat_type, chat_id),
                &h.tmux_name,
            ) {
                log(&summon_i18n::format!("rdbd5765762", agent.name));
            }
        }
        // Transcript 的会话活性由 tmux 表达(还要据此探测外带 kill);无头路没有外部持久层,
        // 会话的存在就是 chat handle 的存在。
        let present = if transcript {
            crate::session::tmux::Tmux::new(h.tmux_name.clone()).has_session()
        } else {
            true
        };
        if !present {
            if h.tmux_seen_live {
                // 曾活后消失 = 被外带 kill。① 投 SilentExit 让残留 chat 线程退出(PF1:它持自己 sender
                // 的 clone,rx 永不 Disconnected,不投则线程 + mirror watcher 永久泄漏)。② 记下,循环后
                // 统一发 ChatExited 合成回执(避免边遍历边改 chats)。
                // **SilentExit 无条件投**:让路只是「不再合成回执」,不是「什么都不做」——
                // 否则线程与 watcher 照样泄漏。
                let _ = h.tx.send(ThreadMsg::SilentExit);
                // 已有标记 = CLI kill 已入队回执并负责投递 → 本探测让路,不再合成,否则同一次
                // 退出发两条。无标记才合成:那是「非 summon CLI 销毁的 tmux」(如用户在终端手动
                // 拆掉会话),它仍需要这条兜底。
                // (措辞刻意避开 tmux 销毁 API 的字面量:本文件受红线守卫文本扫描,dispatcher
                //  永远不该进那张白名单——idle 线程从不擅杀。)
                if crate::session::exit_mark::exists(agent, &h.chat_type, chat_id) {
                    crate::session::exit_mark::clear(agent, &h.chat_type, chat_id);
                    continue;
                }
                // summon 自己拆的会话(定时轮 `context: fresh`、不发回执的重置)在销毁之前已由
                // `do_teardown` 写下 lifecycle Closed:那不是外带 kill。退出路径自己发 ChatExited
                // 摘 handle,这里再合成一条会摘掉紧接着重建的新 worker、还往群里发一条假回执。
                // 新 worker admission 写回 Open,之后的真外杀照旧能探测到。
                if crate::session::lifecycle::read(agent, &chat_tag(&h.chat_type, chat_id))
                    .is_closed()
                {
                    continue;
                }
                // 外带 kill 没走 do_teardown,.session 还在,可读 created_at。
                let session_created_at =
                    crate::session::resume_store::read_record(agent, &h.tmux_name)
                        .and_then(|r| r.created_at);
                killed_externally.push((
                    chat_id.clone(),
                    h.chat_type.clone(),
                    read_chat_meta_creator(agent, &h.tmux_name),
                    session_created_at,
                ));
            }
            continue;
        }
        if transcript {
            h.tmux_seen_live = true;
        }
        // 空闲太久 → 产出「退出(归档续聊)」意图。**只产出意图**:真正的拆除由 chat 线程
        // 走既有 Control::Clean 路径执行(teardown.rs 是白名单内的合法销毁点),本文件
        // 一行 tmux 销毁代码都不写 —— 红线管的是「谁动手」,授权改的是「要不要动手」。
        // 定时尾轮(timer_idle_retire_secs)只换空闲窗口,判据与通路与普通 retire 完全同一套。
        let tag = chat_tag(&h.chat_type, chat_id);
        if crate::session::lifecycle::read(agent, &tag).is_closed() {
            continue;
        }
        // turn 刚结束(在跑 → 不在跑)且不是定时尾轮:把空闲时钟拨到 turn 结束时刻。
        // 锚点原本只跟入站消息,长 turn(>ttl)一结束,提醒/回收会因入站锚点已过点而立刻
        // 补发(owner 2026-08-19 实测:12:58 回复、13:03 即提醒 55min)。定时尾轮保持原行为
        // (从 timer fire 起算静默回收),不在此重置。
        let currently_active = turns.has_turn_record(&tag) || holders.has_any(&tag);
        if h.turn_was_active && !currently_active && !h.last_inbound_scheduled {
            h.note_activity(agent, chat_id, None);
        }
        h.turn_was_active = currently_active;
        let has_stored_id =
            crate::session::resume_store::read_id(agent, &h.tmux_name, &agent.resume_cli_kind())
                .is_some();
        let window = idle_retire_window(h.last_inbound_scheduled, timer_retire_after, retire_after);
        // subagent 观测:活跃 → 本轮不回收(暂态,同 turn 在跑);最近活动 → 推进
        // 空闲时钟(subagent 结束后的空闲期从它的最后活动起算)。观测不可得(None,
        // 含 watcher 死了/锁争用)→ 按「无 subagent」回退现行为。
        let probe = h.subagent_probe(now_utc);
        if should_retire_idle_session(
            effective_idle_anchor(h.last_activity, probe.and_then(|p| p.last_activity)),
            now,
            window,
            present,
            resume_enabled,
            has_stored_id,
            currently_active,
            probe.map(|p| p.active).unwrap_or(false),
        ) {
            // 定时尾轮才查重置开关:人类尾轮永远走 Clean(归档续聊)。per-chat 覆盖
            // 优先于 agent 维度配置;只在真要回收时读一次盘(非热路径)。
            let timer_reset = h.last_inbound_scheduled
                && resolve_settings(agent, &h.tmux_name, effective).timer_idle_retire_reset();
            retiring.push((
                chat_id.clone(),
                window.map(|d| d.as_secs()).unwrap_or(0),
                h.last_inbound_scheduled,
                timer_reset,
            ));
        }
        // 结构门槛全部可证才压提醒卡;任一证不了(如 resume 记录缺失)→ 不进集合,
        // 到点照发提醒卡(回退旧行为),不留「既无卡又不回收」的悬空态。
        if timer_quiet_owns_idle(
            h.last_inbound_scheduled,
            timer_retire_after,
            present,
            resume_enabled,
            has_stored_id,
        ) {
            timer_quiet.insert(chat_id.clone());
        }
    }
    // 自动退出:只投意图,拆除由 chat 线程执行(见 `should_retire_idle_session` 的说明)。
    // 不在上面的遍历里投是为了不边遍历边改 chats;handle 的摘除照常由 ChatExited 收口。
    dispatch_retire_intents(chats, agent, retiring, &mut timer_quiet);
    for (chat_id, chat_type, creator, session_created_at) in killed_externally {
        log(&summon_i18n::format!("ra5098c7a95", agent.name));
        let _ = self_tx.send(DispatchMsg::ChatExited {
            chat_id,
            chat_type,
            reason: summon_core::exit::ExitReason::ManualKill,
            creator,
            notify: true,
            session_created_at,
        });
    }
    for (chat_id, h) in chats.iter_mut() {
        // 定时尾轮且静默回收结构可达 → 不发提醒卡(需求核心)。注意不置 idle_prompted:
        // 这条 chat 的空闲处理已由回收托管,而托管一旦失效(如 resume 记录消失)下轮扫描
        // 会自然回到这里补发提醒卡。
        if timer_quiet.contains(chat_id) {
            continue;
        }
        // 与回收判定同源的 subagent 观测:活跃 → 不发提醒卡(会话在工作,卡只是噪音);
        // 最近活动 → 推进空闲时钟。观测不可得(None)→ 按「无 subagent」回退。
        let probe = h.subagent_probe(now_utc);
        let idle_anchor =
            effective_idle_anchor(h.last_activity, probe.and_then(|p| p.last_activity));
        let tag = chat_tag(&h.chat_type, chat_id);
        if crate::session::lifecycle::read(agent, &tag).is_closed() {
            continue;
        }
        if should_prompt_idle(
            idle_anchor,
            now,
            ttl,
            h.idle_prompted,
            // 与回收判定同源:主轮在跑就不发提醒卡(owner 2026-08-17:连续输出 55min 仍发提醒)。
            turns.has_turn_record(&tag) || holders.has_any(&tag),
            probe.map(|p| p.active).unwrap_or(false),
        ) {
            // 空闲提醒只问当前会话如何处理,不复用带模型/工作目录的 session 面板。
            let context = latest_idle_context(agent, &tag, &h.tmux_name);
            let card = summon_core::feishu::card_idle_prompt_with_context(
                &tag,
                ttl.as_secs(),
                Some(&context),
            );
            // 账本先行(终审阻塞,2026-08-13):先把「本段空闲期已尝试发卡」写成 durable
            // marker,写成功才允许联网发送。写失败 → 不发、也不置 idle_prompted,下轮扫描
            // 重试(磁盘瞬时写满恢复后照常发)—— fail-closed 的方向是漏、不是重。
            // period_start 记生效的空闲期起点(诊断字段,判定只看 attempted_at)。
            if let Err(e) = write_idle_prompt_marker(agent, &h.chat_type, chat_id, idle_anchor, now)
            {
                log(&summon_i18n::format!("r9d7266c2de", agent.name));
                continue;
            }
            // marker 已落盘 = 本段空闲期的发送机会已用掉。之后发送层报什么都不改账本:
            // 「成功」可能没归档、「失败/超时」可能实际已送达 —— 事后报告证明不了任何事,
            // 这正是归档推导被终审否掉的原因。send 失败不重试(at-most-once,漏不重发)。
            h.idle_prompted = true;
            match send_card_archived(
                fc,
                &agent.state_dir(),
                &h.chat_type,
                chat_id,
                None,
                "idle_prompt_card",
                &card,
            ) {
                Ok(()) => {
                    log(&summon_i18n::format!("r89ce9dfd2a", agent.name));
                }
                Err(e) => log(&summon_i18n::format!("r5476596522", agent.name)),
            }
        }
    }
}

/// 把空闲扫描判定出的退出意图逐条投给 chat 线程,并维护 `timer_quiet`(静默回收托管集合)。
///
/// **Clean 投递失败必须撤销该 chat 本轮的 quiet 托管**(终审阻塞②):receiver 已断
/// (chat 线程已退、ChatExited 尚未收口)时回收意图没有执行者,若仍压提醒卡,会话就落进
/// 「既不回收也不发卡」的悬空态,违反「证不了就回退发卡」的声明。撤销后由同一轮扫描后段的
/// 提醒卡循环按原 idle_ttl 兜底 —— 方向是「多打扰一次」,不是「拆错一个」。
/// (本函数仍只投意图、零 tmux 副作用,红线不变。)
pub(super) fn dispatch_retire_intents(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    retiring: Vec<(String, u64, bool, bool)>,
    timer_quiet: &mut std::collections::HashSet<String>,
) {
    for (chat_id, window_secs, timer_tail, timer_reset) in retiring {
        let Some(h) = chats.get(&chat_id) else {
            // handle 已不在(并发收口):意图无处投,托管一并撤销(不在集合里则为 no-op)。
            timer_quiet.remove(&chat_id);
            continue;
        };
        // 定时尾轮 + 开了重置 → 走 Reset(清 resume 记录,下次触发全新 session);其余走
        // Clean(归档续聊)。人类尾轮 timer_reset 位为 false,永远续聊。
        let reset = timer_tail && timer_reset;
        log(&summon_i18n::format!(
            "r55f94b5d48",
            agent.name,
            if timer_tail {
                summon_i18n::text!("r924e344d52")
            } else {
                summon_i18n::text!("ra63280253f")
            },
            if reset {
                summon_i18n::text!("r65437fa17e")
            } else {
                summon_i18n::text!("r4ac05daf77")
            },
        ));
        let sent = h.tx.send(ThreadMsg::Control {
            ctrl: if reset {
                Control::Reset
            } else {
                Control::Clean
            },
            // 没有可 PATCH 的卡片(不是用户点出来的),回执走 ChatExited。
            card_message_id: None,
            models: Vec::new(),
            origin: CardOrigin::Panel,
            // **定时尾轮静默走,连退出回执也不发**(owner 2026-08-16)。
            //
            // 「定时任务会话安静地走」这条需求原本只落了一半:`timer_quiet_owns_idle` 压掉了
            // 到点的**空闲提醒卡**,但 55 分钟真回收时那张「📦 会话已归档」照发不误 —— 对一条
            // 没人跟进的定时会话来说,那正是它唯一还会在群里冒的泡,也就是需求要消掉的那个。
            //
            // 只压回执,不动拆除:`do_teardown` 里的 `lifecycle::mark_closed` 是**无条件**写的,
            // MenuBar / 判活一个字节都不受影响;resume 记录照留,下条消息接回上下文。
            //
            // 也**不写** `exit_mark`:那个标记的消费方只有 bridge(「该群已收到终态,拒绝
            // /bridge 过去」)。这里本就没发终态给用户,写了等于平白挡住一条合法通路。
            notify_exit: !timer_tail,
            idle_retired: true,
            source_task: None,
            source_started: false,
        });
        if sent.is_err() {
            timer_quiet.remove(&chat_id);
            log(&summon_i18n::format!("r1f2a6c573c", agent.name));
        }
    }
}

/// 孤儿重投的活动收口(终审阻塞①孤儿半边 + 阻塞③):统一走 `ChatHandle::note_activity`,
/// 时钟推进与空闲卡去重清除(内存位 + durable marker)对新建/已有 handle 一视同仁 ——
/// 重投的孤儿任务是真实活动,不清去重位就会把 spawn 从账本恢复出的 `idle_prompted=true`
/// 一路带到 timer 静默托管撤销之后,挡住兜底提醒卡(「既不回收也不发卡」悬空态)。
/// 来源代际只在本次重投**新建**的 handle 上登记队尾任务 id(pending,等 `ScheduledAdmitted`
/// 命中才置定时尾轮);命中已有 handle 时来源不动 —— 已有来源写于旧 worker 死后,
/// 必然比死前入队的孤儿新。
pub(super) fn note_orphan_requeue_activity(
    handle: &mut ChatHandle,
    agent: &Agent,
    chat_id: &str,
    rebuilt: bool,
    tasks: &[ChatTask],
) {
    let source = if rebuilt {
        tasks.last().map(|t| t.message_id.as_str())
    } else {
        None
    };
    handle.note_activity(agent, chat_id, source);
}

/// 非空任务批次按原序交接；新建 worker 由调用方提供当前路由与运行配置。
pub(super) fn redeliver_orphaned_tasks(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    chat_id: &str,
    tasks: Vec<ChatTask>,
    spawn: impl FnOnce() -> Option<ChatHandle>,
) -> Option<usize> {
    if tasks.is_empty() {
        return Some(0);
    }
    let (handle, rebuilt) = match chats.entry(chat_id.to_string()) {
        std::collections::hash_map::Entry::Occupied(entry) => (entry.into_mut(), false),
        std::collections::hash_map::Entry::Vacant(entry) => (entry.insert(spawn()?), true),
    };
    note_orphan_requeue_activity(handle, agent, chat_id, rebuilt, &tasks);
    let mut delivered = 0;
    for task in tasks {
        let mid = task.message_id.clone();
        match handle.tx.send(ThreadMsg::Task(Box::new(task))) {
            Ok(()) => delivered += 1,
            Err(_) => log(&summon_i18n::format!("r04c558a018", agent.name,)),
        }
    }
    Some(delivered)
}

/// R3 启动扫描:枚举本 agent `threads/*/pending.json`,返回同一 daemon 飞书 epoch 下待续跑的
/// `(chat_id, chat_type)`。身份不匹配/旧 schema 在这里就删除，确保不会仅因旧 app checkpoint
/// 拉起 chat worker，随后触发表情恢复等飞书网络副作用；chat 线程入口仍做第二次同样的硬校验。
/// 读不出/损坏的跳过（`read_pending` 会当场删除损坏文件，降级为不恢复）。
/// 这次点击带的 fencing token 是否就是本 chat**此刻在跑**的那一轮。
///
/// 校验权威是 `CurrentTurnStore::load_by_tag` —— 它同时比对 turn id 与租约新鲜度。三种情况一律判否
/// (fail-closed):payload 没带 token(改动之前发出的老卡)、当前没有活跃 turn(daemon 重启后 store
/// 已被 `clear_on_daemon_boot` 清空)、token 指向已结束的旧轮。
/// boot 反熵:把上个 daemon 留下的孤儿 live 卡定格成「观察中断」态(去掉打断按钮)。
///
/// **这是收敛,不是正确性机制。** 正确性由两条独立保证:① 打断按钮带 turn fencing token,陈旧卡片
/// 点了也会被拒(`interrupt_token_is_current`);② 卡上带「更新于」时刻,读的人不依赖任何推送就能
/// 看出它陈了。本函数只是让卡**看起来**也对。所以它全程 best-effort:PATCH 失败(消息被删、权限
/// 变化、飞书抖动)只 log,绝不阻塞启动、也不影响任何判断。
///
/// 正常收尾的轮不会留 `stream_state.json`(`clear_stream_state` 已清),所以扫到的恰好就是孤儿。
/// 重建孤儿卡正文时从 events.jsonl 尾部回读的行数上限。
///
/// 一轮的事件量级是几十到几百行(含工具入参/结果);2000 行足够覆盖最后一轮,又不会把
/// 一个 50MB 的文件整份读进内存 —— 这是启动路径,不该有内存尖峰。
const ORPHAN_REBUILD_MAX_LINES: usize = 2000;

fn finalize_orphan_stream_cards(agent: &Agent, fc: &FeishuClient) {
    // 接管轮认领的那条 CoT 跳过不收:它是那一轮唯一的过程账本,提前收口 = 用户看着它
    // 停在停机那一刻,而轮其实还在跑。keep 集来自启动那次全量接管扫描(见
    // [`plan_reattach_with`]),与「镜像给主路径让路」查同一份结果,因而看同一份 tmux/WAL 快照。
    crate::frontend::finalize_orphan_cot_messages(fc, &agent.state_dir(), &|tag, cot_id| {
        cot_claimed_by_reattach(&agent.name, tag, cot_id)
    });
    let Ok(entries) = std::fs::read_dir(agent.state_dir().join("threads")) else {
        return;
    };
    let at = chrono::Local::now().format("%m-%d %H:%M").to_string();
    for e in entries.flatten() {
        let Some(tag) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(state) = crate::runtime::inflight::read_stream_state(&agent.state_dir(), &tag)
        else {
            continue;
        };
        // 没带按钮的卡不会误伤,也不冒充「进行中」,不必动它。
        if !state.with_interrupt {
            crate::runtime::inflight::clear_stream_state(&agent.state_dir(), &tag);
            continue;
        }
        // **从 events.jsonl 重建那一轮已生成的正文**,连同中断说明一起渲成终态卡。
        //
        // 上一版试过"读回原卡再追加",但**飞书不提供读回卡片原文的能力**(`im/v1/messages/:id`
        // 对 interactive 只返回「请升级客户端」这样的降级占位),那条路从未生效过,每次都静默
        // 退回整卡覆盖 —— 正文被两行说明盖掉,正是 owner 报的现象。
        //
        // 改用盘上事实:events.jsonl 本来就在全量 tee,按 `user_input` 切出最后一轮即可,
        // 零新增写入、不给流式热路径加任何 IO。重建失败(文件缺失/全是工具事件)→ 空 segments
        // → 退化成纯说明卡,与改动前等价。
        let segments = summon_core::event_log::rebuild_last_turn_segments(
            &crate::observe::event_log::read_tail_records(
                &agent.state_dir(),
                &tag,
                ORPHAN_REBUILD_MAX_LINES,
            ),
        );
        let card = summon_core::feishu::card_observation_interrupted_with_body(&segments, &at);
        match fc.update_card(&state.message_id, &card) {
            Ok(()) => log(&summon_i18n::format!("rc82ba3b814", agent.name)),
            Err(error) => log(&summon_i18n::format!("r00cfe62253", agent.name)),
        }
        // 无论 PATCH 成没成都清 checkpoint:它只用于「续传同一张卡」,而本轮已经没有续传对象了。
        crate::runtime::inflight::clear_stream_state(&agent.state_dir(), &tag);
    }
}

/// 控制台会话页的「打断」:与控制卡「⏸ 打断」同一道围栏(本轮凭证要对得上,见
/// [`interrupt_token_is_current`]),过了才投 `ThreadMsg::Interrupt` 给 chat 线程。
fn dashboard_interrupt(
    agent: &Agent,
    chats: &std::collections::HashMap<String, ChatHandle>,
    chat_id: &str,
    turn_id: &str,
) -> super::DashInterrupt {
    let chat_type = card_chat_type(chats, chat_id);
    let tag = chat_tag(&chat_type, chat_id);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let current = !turn_id.is_empty()
        && crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
            .load_by_tag(&tag, turn_id, now_ms)
            .is_ok();
    if !current {
        return super::DashInterrupt::Stale;
    }
    match chats.get(chat_id) {
        Some(h) if h.tx.send(ThreadMsg::Interrupt).is_ok() => super::DashInterrupt::Sent,
        _ => super::DashInterrupt::NotRunning,
    }
}

fn interrupt_token_is_current(
    agent: &Agent,
    chats: &std::collections::HashMap<String, ChatHandle>,
    card: &CardActionMsg,
) -> bool {
    let Some(token) = card.value.get("turn_id").and_then(|v| v.as_str()) else {
        return false;
    };
    if token.is_empty() {
        return false;
    }
    let chat_type = card_chat_type(chats, &card.chat_id);
    let tag = chat_tag(&chat_type, &card.chat_id);
    let now_ms = chrono::Utc::now().timestamp_millis();
    crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
        .load_by_tag(&tag, token, now_ms)
        .is_ok()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MirrorChatDiskEntry {
    session: String,
    tag: String,
    chat_id: String,
    chat_type: String,
}

fn mirror_chat_entries(agent: &Agent) -> Vec<MirrorChatDiskEntry> {
    let Ok(entries) = std::fs::read_dir(agent.state_dir().join("runtime").join("chats")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(session) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".meta"))
        else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let meta = summon_core::chat_meta::ChatMeta::parse(&content);
        let (Some(chat_id), Some(tag)) = (meta.chat_id, meta.tag) else {
            continue;
        };
        let chat_type = meta.chat_type.unwrap_or_else(|| "group".to_string());
        if chat_tag(&chat_type, &chat_id) != tag {
            continue;
        }
        out.push(MirrorChatDiskEntry {
            session: session.to_string(),
            tag,
            chat_id,
            chat_type,
        });
    }
    out.sort_by(|left, right| left.session.cmp(&right.session));
    out
}

fn mirror_transcript_path(
    agent: &Agent,
    entry: &MirrorChatDiskEntry,
    record: &crate::capture::MirrorOffsetRecord,
) -> Option<PathBuf> {
    crate::capture::resolve_transcript_path(
        agent,
        &entry.session,
        Some(&entry.chat_id),
        None,
        record.sid(),
    )
    .ok()
}

/// 优雅停机的 byte fence:给每个 live chat 记下**停机那一刻的 transcript EOF**。
///
/// **为什么不再要求 offset 已追平 EOF**(2026-08-13 改)。旧判据是「只签已追平的」,理由是
/// 「否则 boot 无法区分停机前旧欠账与真正停机期输出」——那个理由成立,但解法选错了:
/// 「停机时正好有一轮在跑」恰恰保证追不平,于是**那一轮必然签不到 fence、必然补不回来**。
/// agent 自己跑 `deploy.sh` 时 100% 命中这个场景,自己的汇报每次都丢(owner 2026-08-13 实测)。
///
/// 区分旧欠账与停机期输出不需要「追平」这个前提,只需要把分界线本身记下来:`stop_eof` 右侧的字节
/// 一定落盘于 daemon 死后。见 [`MirrorStopFence::stop_eof`]。
pub(super) fn capture_mirror_stop_fences(
    registry: &Registry,
    frozen_epochs: &HashMap<String, String>,
) -> Vec<MirrorStopFence> {
    // 停机关键路径只执行一次 `tmux list-sessions`。逐 chat `has-session` 会把 O(chat) 个子进程
    // 串在 clean marker 之前；列表读取失败按空集合 fail-closed，不给任何 chat 签 fence。
    let live_sessions = crate::session::tmux::list_all_sessions()
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    capture_mirror_stop_fences_with(
        registry,
        |agent| frozen_epochs.get(&agent.name).cloned(),
        |session| live_sessions.contains(session),
        mirror_transcript_path,
        crate::capture::capture_mirror_eof_fence,
        unix_now_secs,
    )
}

/// 现在的 unix 秒。测试注入自己的时钟,生产用系统钟 —— summon-core 不碰时钟的约定只约束 core,
/// 这里已经是 IO 层。
pub(super) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn capture_mirror_stop_fences_with<F, L, R, C, N>(
    registry: &Registry,
    mut frozen_epoch: F,
    mut is_live: L,
    mut resolve_transcript: R,
    mut capture_eof: C,
    mut now_secs: N,
) -> Vec<MirrorStopFence>
where
    F: FnMut(&Agent) -> Option<String>,
    L: FnMut(&str) -> bool,
    R: FnMut(&Agent, &MirrorChatDiskEntry, &crate::capture::MirrorOffsetRecord) -> Option<PathBuf>,
    C: FnMut(&std::path::Path) -> anyhow::Result<(u64, String)>,
    N: FnMut() -> u64,
{
    let mut fences = Vec::new();
    for agent in &registry.agents {
        if agent.capture_mode() != CaptureMode::Transcript {
            continue;
        }
        let Some(feishu_epoch_fingerprint) = frozen_epoch(agent) else {
            continue;
        };
        for entry in mirror_chat_entries(agent) {
            if !is_live(&entry.session) {
                continue;
            }
            let offset_path = agent
                .state_dir()
                .join("threads")
                .join(&entry.tag)
                .join("mirror_offset");
            let Ok(raw) = std::fs::read_to_string(offset_path) else {
                continue;
            };
            let Some(record) = crate::capture::parse_mirror_offset_record(&raw) else {
                continue;
            };
            if record.epoch_fingerprint() != feishu_epoch_fingerprint {
                continue;
            }
            if record.boundary_kind() != crate::capture::MirrorOffsetBoundaryKind::TerminalBoundary
            {
                continue;
            }
            let Some(transcript_path) = resolve_transcript(agent, &entry, &record) else {
                continue;
            };
            // 停机那一刻的分界线。取不到(文件消失/权限)就不签这个 chat —— 主动出站宁可不补。
            let Ok((stop_eof, stop_eof_checkpoint)) = capture_eof(&transcript_path) else {
                continue;
            };
            fences.push(MirrorStopFence {
                agent: agent.name.clone(),
                feishu_epoch_fingerprint: feishu_epoch_fingerprint.clone(),
                session: entry.session,
                tag: entry.tag,
                chat_id: entry.chat_id,
                chat_type: entry.chat_type,
                // 比较 exact 原文而非重新编码；任何停机快照后的 offset 推进都会让 boot 拒绝。
                offset_record: raw,
                stop_eof,
                stop_eof_checkpoint,
                stopped_at_secs: now_secs(),
            });
        }
    }
    fences.sort_by(|left, right| {
        (&left.agent, &left.session, &left.tag).cmp(&(&right.agent, &right.session, &right.tag))
    });
    fences
}

/// 本代启动的镜像补发计划:`(agent, tag) → catchup_floor`。
///
/// 每条 fence 都要过完这一串闸才算数,任一条证不了就跳过(主动出站宁可漏补,不能猜):
/// ① 上一代 fence 的 offset 原文与本代盘上 **exact 相同**(停机后没人动过 offset)
/// ② epoch 指纹一致(不把 A 代内容发进 B 代群)、boundary 是 TerminalBoundary
/// ③ 停机那一刻记下的 `stop_eof` 分界线在本代盘上仍能 exact 复验(文件没被 truncate/换代)
/// ④ **新鲜度**:停机距今不超过 [`MIRROR_CATCHUP_MAX_STALENESS_SECS`]
/// ⑤ tmux 会话仍存活(在 `mirror_behind_chats_on_disk_with` 里复核)
/// ⑥ 这个 chat **不会被主路径接管**:接管轮自己就持有停机窗口那段字节的投递权,镜像再补一遍
///    就是同一轮内容发两遍(接管计划见 [`plan_reattach_with`],判据是同一份
///    [`reattach_anchor_on_disk`])
///
/// **不再要求「fence 之后确有字节」**:那条恰恰把最该补的场景挡在门外 —— 停机时正在跑的那一轮,
/// 停机快照那一刻还没落盘任何字节,等它跑完时早已没人监听(见 [`capture_mirror_stop_fences`])。
pub(super) fn plan_mirror_catchup(
    registry: &Registry,
    clean_record: &CleanShutdownRecordV2,
    frozen_epochs: &HashMap<String, String>,
    frozen_scopes: &HashMap<String, String>,
) -> std::collections::HashMap<(String, String), u64> {
    let live_sessions = crate::session::tmux::list_all_sessions()
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let inbox_wal = crate::runtime::inbox_wal::InboxWal::new(run_dir().join("inbox"));
    let now_secs = unix_now_secs();
    // 接管计划先算,两个消费者(镜像让路、CoT 收口跳过)都只查这一份。同一次判定 = 同一份
    // tmux / WAL 快照,分两次算就可能出现「镜像让了路、CoT 却被收口」的半拉子状态。
    //
    // scope 取**冻结快照**里的那一份,不现读 `.env`:两次读取之间 A→B 会让接管判据落在另一个
    // 身份上,而这里恰恰是「这一轮归谁」的裁决点。取不到 scope 即证不了身份 → 不判接管,
    // 镜像按既有闸自行决定。
    // 停机时刻取 daemon 级那一个(与 `boot_recover_inbox` 同源),不是某条 fence 的。
    let reattaching = plan_reattach_with(
        registry,
        clean_record.stopped_at_secs,
        now_secs,
        |agent| frozen_scopes.get(&agent.name).cloned(),
        |session| live_sessions.contains(session),
        &inbox_wal,
    );
    record_reattaching_cots(
        reattaching
            .iter()
            .filter_map(|(key, cot)| cot.clone().map(|cot_id| (key.clone(), cot_id)))
            .collect(),
    );
    plan_mirror_catchup_with(
        registry,
        clean_record,
        |agent| frozen_epochs.get(&agent.name).cloned(),
        |session| live_sessions.contains(session),
        mirror_transcript_path,
        crate::capture::mirror_eof_fence_still_valid,
        |agent, tag| reattaching.contains_key(&(agent.name.clone(), tag.to_string())),
        now_secs,
    )
}

/// 本代获批接管的 chat:`(agent, tag) → 它认领的那条 CoT`(停机前还没开出 CoT 时为 `None`)。
///
/// **全量扫盘,不搭 mirror fence 的便车。** 签 fence 要求盘上已有一枚 TerminalBoundary 的
/// `mirror_offset`,而首轮就被重启的会话一条 fence 都没有 —— 它恰恰是最该接管的那种。只在
/// fence 循环里顺手判接管,这类 chat 就永远进不了 keep 集:CoT 被启动收口关掉,接管轮只能
/// 退回新载体。
///
/// 判据是同一份 [`reattach_anchor_on_disk_with`],与 `boot_recover_inbox` 逐字节相同;停机时刻
/// 也同源(都取 clean marker 上那个 daemon 级的)。**但 tmux 存活与 now 是两次独立采样** ——
/// 本次扫描一份、`boot_recover_inbox` 另一份。分叉只有一个方向:会话在两次采样之间消失,
/// 本函数判接管而 `boot_recover_inbox` 判不接管,于是 keep 集多留一条本代不收的 CoT ——
/// 它由下一次启动的收口关掉。偏保守,不会把该收的 CoT 提前封口,也不会让镜像与主路径双发
/// (镜像让路的判据同样出自这一份结果)。
fn plan_reattach_with(
    registry: &Registry,
    stopped_at_secs: u64,
    now_secs: u64,
    mut scope_of: impl FnMut(&Agent) -> Option<String>,
    mut is_live: impl FnMut(&str) -> bool,
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
) -> std::collections::HashMap<(String, String), Option<String>> {
    let mut plan = std::collections::HashMap::new();
    for agent in &registry.agents {
        // 接管只有 Transcript 形态;无头路的在跑轮有自己的 pending.json 续跑语义。
        if agent.capture_mode() != CaptureMode::Transcript {
            continue;
        }
        let Some(scope) = scope_of(agent) else {
            continue;
        };
        // 同一个 tag 可以有多份 `.meta`(历史命名残留),锚点判据自己会逐条问 sid,这里只需要
        // 去重后的 tag 集合。
        let mut tags = mirror_chat_entries(agent)
            .into_iter()
            .map(|entry| entry.tag)
            .collect::<Vec<_>>();
        tags.sort();
        tags.dedup();
        for tag in tags {
            let Some(anchor) = reattach_anchor_on_disk_with(
                agent,
                &scope,
                inbox_wal,
                &tag,
                stopped_at_secs,
                now_secs,
                &mut is_live,
            ) else {
                continue;
            };
            plan.insert((agent.name.clone(), tag), anchor.cot.map(|cot| cot.cot_id));
        }
    }
    plan
}

/// 本代获批接管的 chat 认领的那条 CoT:`(agent, tag) → cot_id`。取自 [`plan_reattach_with`]
/// 那次全量扫描,所以有没有 mirror fence 不影响它 —— 首轮就被重启的会话同样在里面。
///
/// 与 [`APPROVED_MIRROR_CATCHUP`] 同一形态:启动规划严格早于任何 dispatcher,所以 dispatcher
/// 线程读到的一定是规划完的结果。未设置(硬崩、没有 clean 记录)= 空集 —— 一条都不接管,
/// 启动收口全收。
static REATTACHING_COTS: std::sync::OnceLock<std::collections::HashMap<(String, String), String>> =
    std::sync::OnceLock::new();

fn record_reattaching_cots(cots: std::collections::HashMap<(String, String), String>) {
    let _ = REATTACHING_COTS.set(cots);
}

pub(super) fn cot_claimed_by_reattach(agent: &str, tag: &str, cot_id: &str) -> bool {
    REATTACHING_COTS
        .get()
        .and_then(|cots| cots.get(&(agent.to_string(), tag.to_string())))
        .is_some_and(|claimed| claimed == cot_id)
}

#[allow(clippy::too_many_arguments)]
fn plan_mirror_catchup_with<F, L, R, V, A>(
    registry: &Registry,
    clean_record: &CleanShutdownRecordV2,
    mut frozen_epoch: F,
    mut is_live: L,
    mut resolve_transcript: R,
    mut eof_fence_valid: V,
    mut is_reattaching: A,
    now_secs: u64,
) -> std::collections::HashMap<(String, String), u64>
where
    F: FnMut(&Agent) -> Option<String>,
    L: FnMut(&str) -> bool,
    R: FnMut(&Agent, &MirrorChatDiskEntry, &crate::capture::MirrorOffsetRecord) -> Option<PathBuf>,
    V: FnMut(&std::path::Path, u64, &str) -> anyhow::Result<bool>,
    A: FnMut(&Agent, &str) -> bool,
{
    let mut approved = std::collections::HashMap::new();
    if clean_record.version != 2 {
        return approved;
    }
    for fence in &clean_record.mirror_fences {
        let Some(agent) = registry.get(&fence.agent) else {
            continue;
        };
        if agent.capture_mode() != CaptureMode::Transcript {
            continue;
        }
        if frozen_epoch(agent).as_deref() != Some(fence.feishu_epoch_fingerprint.as_str()) {
            continue;
        }
        // 主路径要接管这一轮:停机窗口那段字节归它,镜像让路。
        if is_reattaching(agent, &fence.tag) {
            continue;
        }
        let Some(entry) = mirror_chat_entries(agent).into_iter().find(|entry| {
            entry.session == fence.session
                && entry.tag == fence.tag
                && entry.chat_id == fence.chat_id
                && entry.chat_type == fence.chat_type
        }) else {
            continue;
        };
        if !is_live(&entry.session) {
            continue;
        }
        let offset_path = agent
            .state_dir()
            .join("threads")
            .join(&entry.tag)
            .join("mirror_offset");
        let Ok(raw) = std::fs::read_to_string(offset_path) else {
            continue;
        };
        if raw != fence.offset_record {
            continue;
        }
        let Some(record) = crate::capture::parse_mirror_offset_record(&raw) else {
            continue;
        };
        if record.epoch_fingerprint() != fence.feishu_epoch_fingerprint {
            continue;
        }
        if record.boundary_kind() != crate::capture::MirrorOffsetBoundaryKind::TerminalBoundary {
            continue;
        }
        let Some(transcript_path) = resolve_transcript(agent, &entry, &record) else {
            continue;
        };
        // ③ 分界线本身必须能验真:文件被 truncate / 换了一代,`stop_eof` 右侧就不再是「停机后
        // 新落盘的字节」,补发会把别的 generation 的内容发进群。
        let Ok(true) =
            eof_fence_valid(&transcript_path, fence.stop_eof, &fence.stop_eof_checkpoint)
        else {
            continue;
        };
        // ④ 新鲜度。旧格式记录 `stopped_at_secs=0` → 与 now 的差恒 > 上限 → 自动落在不补那边。
        // 时钟回拨(now < stopped_at)按「不新鲜」处理:证不了就不补。
        let Some(elapsed) = now_secs.checked_sub(fence.stopped_at_secs) else {
            continue;
        };
        if elapsed > MIRROR_CATCHUP_MAX_STALENESS_SECS {
            continue;
        }
        approved.insert((agent.name.clone(), entry.tag), fence.stop_eof);
    }
    approved
}

/// 启动扫描:哪些 chat 的**会话镜像**有经 stop fence 批准的历史 backlog，需要提前拉起并收敛。
///
/// **为什么仍要预热。** 会话镜像懒挂在 chat 线程上；若不拉起，重启后的终端 attach 输出要等下一条
/// 群入站才开始监听。这里不把历史 backlog 当 delivery ledger：旧 main 是否已经发送不可证明，故历史
/// terminal 恒 `HistoricalObserved`、零网络；预热只收敛 durable offset 并提前挂好未来 watcher。
///
/// 只对 Transcript 捕获(claude)有意义 —— 别的 driver 没有会话镜像。boot 已用上一代 stop byte
/// fence 生成一次性批准集；这里仍复核 live tmux + v3 epoch/boundary 指纹，防规划后盘上事实
/// 漂移。未获批准、旧格式、读错、指纹错一律跳过(主动出站宁可漏补，不能猜历史)。
///
/// **不再要求 fence 之后确有字节**(2026-08-13):停机那一刻 backlog 为空恰恰是「轮还在跑」的
/// 正常态,据此放弃监听,那一轮跑完就再没人接。批准与否交给 `plan_mirror_catchup` 的那串闸。
pub(super) fn mirror_behind_chats_on_disk(agent: &Agent) -> Vec<(String, String)> {
    mirror_behind_chats_on_disk_with(agent, |tag| mirror_catchup_is_approved(&agent.name, tag))
}

fn mirror_behind_chats_on_disk_with(
    agent: &Agent,
    mut is_approved: impl FnMut(&str) -> bool,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if agent.capture_mode() != CaptureMode::Transcript {
        return out;
    }
    for entry in mirror_chat_entries(agent) {
        if !is_approved(&entry.tag) {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(&entry.tag)
                .join("mirror_offset"),
        ) else {
            continue;
        };
        // boot 是主动群出站：只认带 epoch + sid + exact boundary 指纹的 v3；旧格式无法证明
        // 属于当前 transcript generation，宁可等用户下次说话也不能拉起后灌历史。
        let Some(offset_record) = crate::capture::parse_mirror_offset_record(&raw) else {
            continue;
        };
        if offset_record.boundary_kind()
            != crate::capture::MirrorOffsetBoundaryKind::TerminalBoundary
        {
            continue;
        }
        if mirror_transcript_path(agent, &entry, &offset_record).is_none() {
            continue;
        }
        // tmux 会话必须还活着。死会话不可能在已冻结的停机窗口里产出新 turn,拉起它只会把陈年
        // backlog 灌进群 —— 2026-08-07 首版漏了这条,把 3 天前已死会话的 14 条 turn 补发进了
        // 「Demo支持XYZ的MVP功能」群。
        if !crate::session::tmux::Tmux::new(entry.session).has_session() {
            continue;
        }
        out.push((entry.chat_id, entry.chat_type));
    }
    out
}

/// 这个 chat 盘上有没有一枚**可接管**的锚点:上一代注入的那一轮还在 tmux 里跑,新一代可以
/// 零注入接住它。判据 —— capture 形态、锚点存在、身份、新鲜度、tmux 存活、盘上原生 sid、
/// Inbox 来源 —— 任一条证不了就返回 `None`,那时按今天的分支走(恢复卡)。
///
/// 判松的代价不是「少接一轮」而是**双执行**:身份认错就把上一代的输出贴进另一条上下文,
/// sid 认错等于往一条已经换过代的会话里接输出,来源对不上则说明这一轮的归属证不全。
///
/// 启动路径上有两个调用方(`boot_recover_inbox` 决定接管、`plan_mirror_catchup` 决定让路),
/// 两者的答案必须逐字节相同,所以判据只有这一份:分家复制会从第二天起漂移成「主路径接管了、
/// 镜像也在补发」的双发。
pub(super) fn reattach_anchor_on_disk(
    agent: &Agent,
    delivery_scope: &str,
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
    tag: &str,
    stopped_at_secs: u64,
    now_secs: u64,
) -> Option<summon_core::TurnAnchor> {
    reattach_anchor_on_disk_with(
        agent,
        delivery_scope,
        inbox_wal,
        tag,
        stopped_at_secs,
        now_secs,
        |session| crate::session::tmux::Tmux::new(session.to_string()).has_session(),
    )
}

#[allow(clippy::too_many_arguments)] // 七条判据的输入各自独立,收 struct 只是换个地方列
fn reattach_anchor_on_disk_with(
    agent: &Agent,
    delivery_scope: &str,
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
    tag: &str,
    stopped_at_secs: u64,
    now_secs: u64,
    mut is_live: impl FnMut(&str) -> bool,
) -> Option<summon_core::TurnAnchor> {
    // 接管只有 Transcript 形态:无头路的在跑轮有自己的 pending.json 续跑语义,两套混用就是双跑。
    if agent.capture_mode() != CaptureMode::Transcript {
        return None;
    }
    let anchor = crate::runtime::inflight::read_turn_anchor(&agent.state_dir(), tag)?;
    // 身份:scope/agent 与本代逐字节相同,且锚点确实归这个 thread 目录 —— `tag` 是独立于锚点
    // 内容的那一半证据,少了它「锚点自称是谁」就成了自证。
    if !anchor.reattachable_in_context(
        delivery_scope,
        &agent.name,
        &anchor.chat_id,
        &anchor.chat_type,
    ) || chat_tag(&anchor.chat_type, &anchor.chat_id) != tag
    {
        return None;
    }
    // 新鲜度:停机三十分钟以上,tmux 里跑着的早已不是停机那一刻那一轮。
    // 拿不到可信停机时刻(`stopped_at_secs == 0`)时 `fresh_at` 自动判过期。
    if !anchor.fresh_at(stopped_at_secs, now_secs, MIRROR_CATCHUP_MAX_STALENESS_SECS) {
        return None;
    }
    // 会话名只认盘上 `.meta`(与镜像 fence 同一份事实源),不按规则 derive:derive 出来的名字
    // 在 owner 私聊等历史命名下会指向另一条会话。
    //
    // **同一个 chat 可以有多份 `.meta`**(裸名 `soul-<alias>` 与带 tag 的那份并存,是 owner 私聊
    // 换过命名的历史残留)。取第一条就等于赌排序,赌错了 sid 对不上、静默不接管。所以逐条问:
    // 谁的盘上 sid 与锚点同一条、且 tmux 还活着,谁才是当轮那条会话。
    let candidates = mirror_chat_entries(agent)
        .into_iter()
        .filter(|entry| {
            entry.tag == tag
                && entry.chat_id == anchor.chat_id
                && entry.chat_type == anchor.chat_type
        })
        .collect::<Vec<_>>();
    if !candidates.iter().any(|entry| {
        anchor.session_matches(
            crate::session::resume_store::read_id(agent, &entry.session, agent.cli_kind())
                .as_deref(),
        ) && is_live(&entry.session)
    }) {
        log(&summon_i18n::format!(
            "r28c69d0b86",
            agent.name,
            candidates.len()
        ));
        return None;
    }
    // 来源一致:锚点认领的 id 集合与 WAL 里本 chat 的 started 全集必须精确相等。多一条说明有
    // 记录接管轮不会替它收尾,少一条说明还有别的轮混在里面。WAL 不可读一律当证不了。
    match inbox_wal.resume_sources_match(
        &agent.name,
        delivery_scope,
        &anchor.chat_id,
        &anchor.chat_type,
        &anchor.inbox_message_ids,
    ) {
        Ok(true) => Some(anchor),
        Ok(false) => None,
        Err(error) => {
            log(&summon_i18n::format!("red6cd02a3e", agent.name));
            None
        }
    }
}

pub(super) fn pending_chats_on_disk(
    agent: &Agent,
    delivery_scope: &str,
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(agent.state_dir().join("threads")) else {
        return out;
    };
    for e in entries.flatten() {
        let Some(tag) = e.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        let Some(p) = crate::runtime::inflight::read_pending(&agent.state_dir(), &tag) else {
            continue;
        };
        let checkpoint_matches =
            p.resumable_in_context(delivery_scope, &agent.name, &p.chat_id, &p.chat_type)
                && chat_tag(&p.chat_type, &p.chat_id) == tag
                && p.cli == agent.cli_kind();
        let sources_match = if checkpoint_matches {
            inbox_wal.resume_sources_match(
                &agent.name,
                delivery_scope,
                &p.chat_id,
                &p.chat_type,
                &p.inbox_message_ids,
            )
        } else {
            Ok(false)
        };
        match sources_match {
            Ok(true) => out.push((p.chat_id, p.chat_type)),
            Ok(false) => {
                log(&summon_i18n::format!("r5440403a08", agent.name));
                crate::runtime::inflight::clear_pending(&agent.state_dir(), &tag);
                crate::runtime::inflight::clear_stream_state(&agent.state_dir(), &tag);
            }
            Err(error) => log(&summon_i18n::format!("r53123242c2", agent.name)),
        }
    }
    out
}

/// 从 `/introduce` / `/unintroduce` 的 @ 列表里取出要(解除)认识的成员:排除发命令的人和本 bot
/// 自己;身份取飞书 @ 事件给的 `mentioned_type`。
fn peers_from_mentions(
    mentions: &[summon_core::ipc::Mention],
    sender: &str,
    self_open_id: &str,
) -> Vec<summon_core::peers::Peer> {
    mentions
        .iter()
        .filter(|m| !m.open_id.is_empty() && m.open_id != sender && m.open_id != self_open_id)
        .map(|m| summon_core::peers::Peer {
            open_id: m.open_id.clone(),
            name: m.name.clone(),
            kind: summon_core::peers::PeerKind::from_mentioned_type(&m.kind),
        })
        .collect()
}

/// 这条消息要不要计入防环预算:发信人是 peer,且不能确认是真人。peer 也可能是被 `/introduce`
/// 的同事,他们连发几条是正常对话,不该把全群的 peer 消息一起熔断。发送者类型缺失也计入 ——
/// 这道闸防的是 bot 互相触发,拿不准时宁可按 bot 算。
fn counts_toward_peer_budget(sender_is_peer: bool, sender_type: &str) -> bool {
    sender_is_peer
        && summon_core::peers::PeerKind::from_sender_type(sender_type)
            != summon_core::peers::PeerKind::Human
}

/// peer 发来消息时,按飞书给的发送者类型补记 / 更正它在本群 peers 里的身份。只在有变化时落盘;
/// 失败只记日志 —— 身份只影响 PRIMER 的措辞,不该拦下这条消息。
fn record_peer_kind(
    peers_store: &crate::runtime::peers_store::PeersStore,
    agent: &Agent,
    msg: &Inbound,
) {
    let tag = chat_tag(msg.chat_type.as_deref().unwrap_or("p2p"), &msg.chat_id);
    let observed = summon_core::peers::PeerKind::from_sender_type(&msg.sender_type);
    match peers_store.record_peer_kind(&tag, &msg.sender, observed) {
        Ok(true) => log(&summon_i18n::format!(
            "r907f2cdc49",
            agent.name,
            msg.chat_id,
            msg.sender
        )),
        Ok(false) => {}
        Err(e) => log(&summon_i18n::format!(
            "rbbcd45e12b",
            agent.name,
            msg.chat_id
        )),
    }
}

/// 用盘上记录的身份补全这次 @ 列表里没给身份的成员(同 open_id),顺序不变。
fn with_recorded_kinds(
    peers: Vec<summon_core::peers::Peer>,
    recorded: &[summon_core::peers::Peer],
) -> Vec<summon_core::peers::Peer> {
    peers
        .into_iter()
        .map(|mut peer| {
            if peer.kind.is_unknown() {
                if let Some(known) = recorded.iter().find(|r| r.open_id == peer.open_id) {
                    peer.kind = known.kind;
                }
            }
            peer
        })
        .collect()
}

fn sender_is_known_peer(
    peers_store: &crate::runtime::peers_store::PeersStore,
    chat_type: &str,
    chat_id: &str,
    sender_open_id: &str,
) -> bool {
    let tag = chat_tag(chat_type, chat_id);
    peers_store
        .load(&tag)
        .is_some_and(|pf| pf.peers.iter().any(|peer| peer.open_id == sender_open_id))
}

/// Talk gate：显式 allowed_users 命中，或 sender 是当前 chat 已 introduce 的 peer。
/// peers 绝不参与 operate gate；破坏性动作仍由各分支的 `Agent::can_operate` 单独判定。
fn sender_can_talk(
    agent: &Agent,
    sender_is_peer: bool,
    sender_open_id: &str,
    sender_union_id: &str,
) -> bool {
    agent.allows(sender_open_id, sender_union_id) || sender_is_peer
}

/// operate-gate 取 operator 的 union_id：**优先用入站事件自带的**,事件没带才回退联系人 API。
/// 顺序反了(先打 HTTP)会让只按 union_id 配 operators 的授权用户在 API 抖动/权限缺失时被误拒 ——
/// 同一个人在 `/bridge` 通过、在 `/cd` 被拒。事件里的 union_id 与 open_id 同源可信,不比 HTTP 弱。
/// 只可能让「本该通过的通过」:两级都拿不到就交回 `can_operate` 的既有 fail-closed 规则。
pub(super) fn operator_union_id(event_union_id: &str, resolve: impl FnOnce() -> String) -> String {
    if event_union_id.is_empty() {
        resolve()
    } else {
        event_union_id.to_string()
    }
}

/// rejected slash 不能把 chat 永久标成「本地已占用」。只有 operate 已放行、capture 确有
/// 交互通道、且这条命令**真会碰到本地会话**(池内透传 / 未知命令的通用注入,或 exact
/// `/quit` `/exit` 的受控拆除)才 claim。
///
/// 判定与 chat 线程共用 `slash_pool::plan_slash` 这一个纯函数 —— 两边各写一套判据,迟早会出现
/// 「dispatcher 认为要 claim、worker 认为该拒」这类错位。所以黑名单也必须传**同一份**已
/// resolve 的 messaging:两边取到不同的黑名单,就等于两边跑了两套准入。`/clear` 只发卡,不 claim。
fn slash_claims_local_session(
    text: &str,
    can_operate: bool,
    capture: CaptureMode,
    cli: CliDriver,
    blacklist: &slash_pool::SlashBlacklist,
) -> bool {
    can_operate
        && slash_pool::plan_slash(
            cli,
            capture,
            text,
            blacklist,
            &slash_pool::SlashToggles::default(),
        )
        .touches_local_session()
}

/// 过 operate-gate 的文字命令。与 `CardAction::operate_scope` 同一纪律:每条命令显式声明效果
/// 落在哪,穷举、无通配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperateCommand {
    Quota,
    SessionTeardown,
    Term,
    Dash,
    Export,
    Model,
    ModelDefault,
    Effort,
    EffortDefault,
    Fast,
    Mode,
    Pwd,
    Cd,
    Remind,
    Introduce,
    Unintroduce,
    ProviderSlash,
}

impl OperateCommand {
    fn scope(self, chat_id: &str) -> OperateScope<'_> {
        match self {
            // 改 bot 默认(agent.yaml / cursor 的 per-agent bundle):所有没有本群覆盖的群一起变。
            Self::ModelDefault | Self::EffortDefault => OperateScope::Agent,
            // 本群模型 / effort / Fast(Codex 是本会话服务档位,Cursor 写本群参数覆盖)。
            Self::Model
            | Self::Effort
            | Self::Fast
            | Self::Quota
            | Self::SessionTeardown
            | Self::Term
            // 与 `/term` 同一口径:按这个 chat 的语义判,本群名单算数。进了控制台之后每一处改动
            // 另按它落在哪一层判(见控制台判权)。
            | Self::Dash
            // 导出的是这个 chat 的材料,按这个 chat 的语义判。
            | Self::Export
            | Self::Mode
            | Self::Pwd
            | Self::Cd
            | Self::Remind
            | Self::Introduce
            | Self::Unintroduce
            | Self::ProviderSlash => OperateScope::Chat(chat_id),
        }
    }
}

/// operate 档卡片动作里不走集中闸、由专用 handler 自己判权限的那几个。它们的卡在私聊里,
/// 集中闸按卡所在 chat 判会错用私聊语义;handler 按目标 chat 的群语义重验。
fn checks_operate_in_handler(action: summon_core::feishu::CardAction) -> bool {
    use summon_core::feishu::CardAction;
    matches!(
        action,
        CardAction::BridgeFamily
            | CardAction::Fork
            | CardAction::RecallResume
            | CardAction::RecallRpcHandoff
            | CardAction::RecallPage
            | CardAction::RecallCmuxRemote
            | CardAction::DismissRecovery
            | CardAction::TerminalRegenerate
            | CardAction::TerminalAuth
            | CardAction::SetWorkdirTo
    )
}

/// 回复卡末尾文件按钮的点击回调:点击人过得了判权就私聊他一条新签的打开链接,过不了就私聊他
/// 没有权限。链接与回执都只进私聊,群里不留任何东西。
fn handle_card_preview_open(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    core: &crate::runtime::core::Core,
    peers_store: &crate::runtime::peers_store::PeersStore,
    card: &CardActionMsg,
) {
    let operator = &card.operator_open_id;
    let token = card
        .value
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let allowed = preview_click_allowed(chats, agent, fc, peers_store, card);
    let sent = if !allowed {
        send_markdown_to_open_id_archived(
            fc,
            &agent.state_dir(),
            operator,
            "preview_notice",
            ::summon_core::i18n::label(PREVIEW_DENIED),
        )
    } else {
        match core
            .preview_server()
            .and_then(|previews| previews.open_link(token))
        {
            Some(link) => crate::frontend::outbound::send_card_to_open_id_redacted_archived_id(
                fc,
                &agent.state_dir(),
                operator,
                "preview_link",
                &summon_core::feishu::card_preview_link(
                    &link.label,
                    link.is_dir,
                    &link.url,
                    link.ttl_secs,
                ),
            )
            .map(drop),
            None => send_markdown_to_open_id_archived(
                fc,
                &agent.state_dir(),
                operator,
                "preview_notice",
                ::summon_core::i18n::label(PREVIEW_UNAVAILABLE),
            ),
        }
    };
    log(&summon_i18n::format!(
        "r64405dcfc8",
        agent.name,
        card.chat_id,
        match &sent {
            Ok(()) => summon_i18n::text!("r23bda9495c").to_string(),
            Err(error) => summon_i18n::format!("r2a8b92eccd"),
        }
    ));
}

const PREVIEW_DENIED: &str = "r871112acca";
const PREVIEW_UNAVAILABLE: &str = "r6536f49f40";

/// 文件按钮的判权,只有这一处:点击人过得了这个 chat 的 talk gate(与入站对话同一道闸:talk 名单,
/// 或本群 `/introduce` 认识的成员),或过得了本群范围的 operate-gate。能对话的人本就能让 agent
/// 把文件内容读给他。
fn preview_click_allowed(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    peers_store: &crate::runtime::peers_store::PeersStore,
    card: &CardActionMsg,
) -> bool {
    let operator = &card.operator_open_id;
    let union = fc.resolve_union_id(operator);
    let is_group = cards::card_chat_is_group(chats, agent, &card.chat_id);
    // peers 按 chat_tag 存,tag 只分群 / 非群。卡片回调不带 chat 类型,只能按内存 / 盘上记录推断,
    // 判不出时与 operate 那半一样按群查:查错 tag 只会查不到,不会放行。
    let chat_type = if is_group { "group" } else { "p2p" };
    let is_peer = sender_is_known_peer(peers_store, chat_type, &card.chat_id, operator);
    sender_can_talk(agent, is_peer, operator, &union)
        || agent.can_operate(
            operator,
            &union,
            is_group,
            OperateScope::Chat(&card.chat_id),
        )
}

fn provider_slash_is_addressed(
    is_group: bool,
    bot_open_id: Option<&str>,
    text: &str,
    mentions: &[summon_core::ipc::Mention],
) -> bool {
    !is_group
        || bot_open_id.is_some_and(|me| summon_core::ipc::is_leading_mention(text, mentions, me))
}

/// 群里一条命令是不是说给本 bot 的:开头 @ 本 bot,或本条消息经一人一 Bot 验证免 @ 放行 ——
/// 群里只有发信人和本 bot,命令不可能是说给别的 bot 的。操作权限仍由各命令自己的 operate-gate 判。
fn command_is_addressed(
    is_group: bool,
    bot_open_id: Option<&str>,
    text: &str,
    mentions: &[summon_core::ipc::Mention],
    mentionless_admitted: bool,
) -> bool {
    mentionless_admitted || provider_slash_is_addressed(is_group, bot_open_id, text, mentions)
}

/// 任务正文、是否按原始斜杠语法处理、是否在对本 bot 说话。斜杠命令取 `slash_addressed`
/// (见 [`command_is_addressed`]);普通对话认开头 @ 本 bot 或一人一 Bot 验证放行的免 @ 消息 ——
/// 走到这里的免 @ 消息只可能是说给本 bot 的,按「@你」交给 agent。
fn task_text_and_addressing(
    slash: Option<String>,
    conversation: &str,
    slash_addressed: bool,
    leading_self_mention: bool,
    mentionless_admitted: bool,
) -> (String, bool, bool) {
    match slash {
        Some(command) => (command, true, slash_addressed),
        None => (
            conversation.to_string(),
            false,
            leading_self_mention || mentionless_admitted,
        ),
    }
}

/// 一条消息作为普通对话交给 agent 时的正文:去掉开头那个 @本 bot,其余 @ 渲染成 `@名字`。
/// 返回值第二项 = 开头确实 @ 了本 bot。
fn conversation_text(
    text: &str,
    mentions: &[summon_core::ipc::Mention],
    bot_open_id: Option<&str>,
) -> (String, bool) {
    if mentions.is_empty() {
        return (text.to_string(), false);
    }
    let (visible, was_stripped) = match bot_open_id {
        Some(me) => summon_core::ipc::strip_leading_mention_by_open_id(text, mentions, me),
        None => (text.to_string(), false),
    };
    (
        summon_core::ipc::render_mentions(&visible, mentions),
        was_stripped,
    )
}

/// 获准的 `/new <内容>` 不转发给 agent:改写成以 `<内容>` 起头、要求新上下文的普通对话,chat 线程
/// 按 `/reset` 同一段代码拆旧会话后冷启它。`<内容>` 取自 `conversation`(同一条消息按普通对话
/// 取的正文),与把这句话直接发给 agent 时看到的一样,不经 slash 命令那套归一化。未获准的照旧作为
/// slash 交 worker 拒绝。
fn rewrite_new_session(
    text: String,
    kind: ChatTaskKind,
    slash_admitted: bool,
    conversation: &str,
) -> (String, ChatTaskKind, bool) {
    if !(kind.is_slash() && slash_admitted) || slash_pool::new_session_text(&text).is_none() {
        return (text, kind, false);
    }
    match slash_pool::new_session_text(conversation) {
        Some(rest) => (rest.to_string(), ChatTaskKind::Conversation, true),
        None => (text, kind, false),
    }
}

/// 获准的 `/handoff [@目标] [备注]` 不转发给 agent:改写成「写交接」的普通对话,带上解析出的目标与
/// 备注;chat 线程起跑前校验目标,轮末按回复里的交接标签交接。目标要从原始消息里认(命令名后面那个
/// @ 的 open_id 与 bot 身份),所以解析取 `raw` 与 `mentions`,不取已抹掉 @ 的 slash 文本。未获准的
/// 照旧作为 slash 交 worker 拒绝。不带目标时按 [`summon_core::handoff::agent_compacts_in_place`]
/// 告诉 agent 写完后是原地压缩还是开新会话,与 chat 线程轮末收尾同一判据。
fn rewrite_handoff(
    text: String,
    kind: ChatTaskKind,
    slash_admitted: bool,
    raw: &str,
    mentions: &[summon_core::ipc::Mention],
    bot_open_id: Option<&str>,
    agent: &Agent,
) -> (
    String,
    ChatTaskKind,
    Option<summon_core::handoff::HandoffCommand>,
) {
    let head = text.split_whitespace().next().unwrap_or_default();
    if !(kind.is_slash() && slash_admitted) || head != summon_core::handoff::HANDOFF_COMMAND {
        return (text, kind, None);
    }
    match summon_core::handoff::handoff_command(raw, mentions, bot_open_id) {
        Some(command) => {
            use summon_core::handoff::HandoffAfter;
            let after = match command.target.as_ref() {
                Some(target) => HandoffAfter::Peer(target.name.as_str()),
                None if summon_core::handoff::agent_compacts_in_place(agent) => {
                    HandoffAfter::CompactInPlace
                }
                None => HandoffAfter::NewSession,
            };
            (
                summon_core::handoff::instruction(after, &command.note),
                ChatTaskKind::Conversation,
                Some(command),
            )
        }
        None => (text, kind, None),
    }
}

/// `/queue <内容>` 不转发给 agent:改写成以 `<内容>` 起头的普通对话,只是标上排队、不参与轮内
/// steer。它只是换一种方式发一条消息,不碰会话,所以与普通对话同一道准入,不走 slash 的
/// operate-gate。`<内容>` 取自 `conversation`,理由同 [`rewrite_new_session`]。
fn rewrite_queued(
    text: String,
    kind: ChatTaskKind,
    raw_provider_syntax: bool,
    conversation: &str,
) -> (String, ChatTaskKind, bool) {
    if !raw_provider_syntax || slash_pool::queued_text(&text).is_none() {
        return (text, kind, false);
    }
    match slash_pool::queued_text(conversation) {
        Some(rest) => (rest.to_string(), ChatTaskKind::Conversation, true),
        None => (text, kind, false),
    }
}

fn chat_task_kind(
    raw_provider_syntax: bool,
    driver: CliDriver,
    capture: CaptureMode,
    text: &str,
    blacklist: &slash_pool::SlashBlacklist,
) -> ChatTaskKind {
    let head = text.split_whitespace().next().unwrap_or_default();
    match (raw_provider_syntax, driver, capture) {
        // 已知原生命令始终进受控规划器：RPC 映射原生方法，Subprocess/Transcript 至少保证
        // `/quit` 生命周期仍生效、其余明确拒绝。动态 `/skill-name` 保留 `$skill-name` provider
        // prompt 语义，避免补控制面顺手切断 skills/plugins。
        (true, CliDriver::Codex | CliDriver::Traex, _)
            if slash_pool::is_codex_family_tui_command(text) || blacklist.blocks(driver, head) =>
        {
            ChatTaskKind::SlashCommand
        }
        (true, CliDriver::Codex | CliDriver::Traex, _) => ChatTaskKind::ProviderPrompt,
        (true, _, _) => ChatTaskKind::SlashCommand,
        (false, _, _) => ChatTaskKind::Conversation,
    }
}

/// `/help` 卡片的两段:命令清单正文 + 末尾那句注脚原文(注脚由卡片层渲成灰字元素)。
fn feishu_chat_help(agent: &Agent) -> (String, Option<&'static str>) {
    let driver = CliDriver::from_kind(agent.cli_kind());
    let messaging = resolve_messaging(agent);
    let controls = slash_pool::summon_control_help_entries(driver, &messaging.slash_blacklist);
    let channel = summon_core::model_effort::channel_label(agent.cli_kind(), agent.capture_mode());
    let mut help = summon_i18n::format!(
        "r0f68d60ec3",
        summon_core::ipc::chat_help_text(driver),
        slash_pool::render_control_help_items(&controls),
        agent.cli_kind(),
        channel,
    );
    let toggles = slash_pool::SlashToggles::default();
    help.push_str(&slash_pool::backend_help_markdown(
        driver,
        agent.capture_mode(),
        &messaging.slash_blacklist,
        &toggles,
    ));
    let note = slash_pool::backend_help_note(
        driver,
        agent.capture_mode(),
        &messaging.slash_blacklist,
        &toggles,
    );
    (help, note)
}

fn retain_local_session_claim(previous: bool, current_task_claims: bool) -> bool {
    previous || current_task_claims
}

fn task_claims_local_session(
    kind: ChatTaskKind,
    is_group: bool,
    addressed: bool,
    slash_admitted: bool,
) -> bool {
    let silently_ignored_provider_syntax =
        kind.uses_provider_slash_syntax() && is_group && !addressed;
    !silently_ignored_provider_syntax && (!kind.is_slash() || slash_admitted)
}

/// 拿不到发信人真名时的**稳定兜底串**。dispatcher 首次遇到某发信人(无 alias、缓存未命中)时落它,
/// chat 线程也用它来判定「这个 who 是不是待补拉的占位」(相等即占位,反之是 alias/缓存真名,不动)。
/// 所以它是两处共用的**单一事实源**——分别拼一遍就会两边对不上,chat 线程要么误覆盖真名、要么永不补拉。
///
/// - **人侧 `用户(open_id)`**:带 open_id 让 agent 能区分不同发信人、不把两个陌生人糊成同一个「用户」;
/// - **bot 侧裸 open_id**:多 bot 同群时名字解析要 `im:chat.members:read`(按 app 申请),没申请就
///   人人都叫「bot」分不清;open_id 入站自带、零权限、对同一接收方稳定,足以区分。
pub(super) fn fallback_display_name(open_id: &str, is_bot: bool) -> String {
    if is_bot {
        open_id.to_string()
    } else {
        summon_i18n::format!("r1ef4ba8f88")
    }
}

/// 一条入站消息该往身份缓存里记哪些 `{open_id, name}`。
///
/// **发信人必须在内。** 原来只记 `mentions`(被 @ 到的人),于是出现一个恰好相反的结果:
/// owner 是那个一直在 @ 别人、自己几乎不被 @ 的人,他的身份从来没进过表,
/// `render_outbound_mentions` 查不到 → agent 写的 `@张三` 原样发出 → 飞书按纯文本渲染,
/// **agent 永远 @ 不到正在跟它说话的那个人**(owner 2026-08-15 报障)。
///
/// 记的是 `sender_display` —— agent 在 prompt 里看到的正是这个名字,它要 @ 谁就照着打。
/// 与兜底占位(`用户(ou_...)` / 裸 open_id)相等时**不记**:那不是名字,记进去只会在表里
/// 留一条永远匹配不上的垃圾,还会挡住之后真名解析出来的那一条(缓存按 name 去重)。
pub(super) fn identities_to_cache(
    sender_open_id: &str,
    sender_display: &str,
    sender_fallback: &str,
    mentioned: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(mentioned.len() + 1);
    if sender_display != sender_fallback {
        out.push((sender_open_id.to_string(), sender_display.to_string()));
    }
    out.extend(mentioned.iter().cloned());
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InboundFrontAdmission {
    Admit,
    DenyTalk,
    NeedBotIdentity,
}

/// 所有命令解析之前的统一前置闸。调用方只有拿到 Admit，或在 NeedBotIdentity 后继续通过群 @ 闸，
/// 才能进入命令和普通 chat 分支；权限不再依赖巨型 match 中的物理行序。
fn inbound_front_admission(
    agent: &Agent,
    sender_is_peer: bool,
    sender_open_id: &str,
    sender_union_id: &str,
    is_group: bool,
) -> InboundFrontAdmission {
    if !sender_can_talk(agent, sender_is_peer, sender_open_id, sender_union_id) {
        return InboundFrontAdmission::DenyTalk;
    }
    if is_group {
        InboundFrontAdmission::NeedBotIdentity
    } else {
        InboundFrontAdmission::Admit
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupMentionAdmission {
    Admit,
    MissingSelfMention,
    BotIdentityUnavailable,
}

/// 群消息必须能确定本 bot 身份且确实 @ 自己；身份 API 失败时 fail-closed。p2p 不受此闸影响。
fn group_mention_admission(
    is_group: bool,
    bot_open_id: Option<&str>,
    mentions: &[summon_core::ipc::Mention],
) -> GroupMentionAdmission {
    if !is_group {
        return GroupMentionAdmission::Admit;
    }
    let Some(bot_open_id) = bot_open_id else {
        return GroupMentionAdmission::BotIdentityUnavailable;
    };
    if summon_core::ipc::mentions_contain(mentions, bot_open_id) {
        GroupMentionAdmission::Admit
    } else {
        GroupMentionAdmission::MissingSelfMention
    }
}

/// 陈旧提示的 per-chat 冷却:一次补投风暴只出一句,不刷屏。
const STALE_NOTICE_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// 同一个 chat 在连击窗口内连着判这么多条陈旧、中间没有一条放行,就只留日志不再出声。零星的
/// 延迟补投是一条两条;连成串说明闸本身在批量误杀(最典型的是本机时钟跑快,每条消息的 age 都被
/// 高估),这时出声只会让用户重发,而重发的每一条都会被同样丢掉。
const STALE_NOTICE_MAX_CONSECUTIVE: u32 = 2;
/// 连击窗口:距上一次判陈旧超过它,就当作新的零星事件重新计数。批量误杀是连着发生的,相隔
/// 几天的两次偶发延迟不该互相拖累。
const STALE_NOTICE_STREAK_WINDOW: Duration = Duration::from_secs(60 * 60);

/// 一个 chat 的陈旧提示记账。
#[derive(Default, Clone, Copy)]
struct StaleNoticeChat {
    last_notice: Option<Instant>,
    last_stale: Option<Instant>,
    /// 连击窗口内、自上一条放行以来连着判陈旧的条数。
    consecutive_stale: u32,
}

/// 做提示准入所需的全部事实。全是值,判定因此可以离开 dispatcher 单测。
struct StaleNoticeFacts {
    /// 管家只回卡片、不进会话层,这句 markdown 不该由它发。
    is_steward: bool,
    /// peer(另一个 bot)的消息被丢不出声:防环预算不该被一条提示绕过。
    sender_is_peer: bool,
    /// talk 名单:不在名单里的人,这条消息就算按时到了我也不会理他。
    can_talk: bool,
    /// bridge 是 operate 控制面,在主路径上排在 talk 名单之前、不受它约束,所以单独算一条准入。
    is_bridge_command: bool,
    /// cli 没有捕获通道时,主路径对任何消息都不作答。
    auto_captures: bool,
    mention: GroupMentionAdmission,
    create_ms: i64,
    now_ms: i64,
}

/// 从入站消息取出准入要用的事实。`bot_open_id` 要打 HTTP,只在群消息上调用。
fn stale_notice_facts(
    agent: &Agent,
    msg: &Inbound,
    peers: &crate::runtime::peers_store::PeersStore,
    bot_open_id: impl FnOnce() -> Option<String>,
    create_ms: i64,
    now_ms: i64,
) -> StaleNoticeFacts {
    let chat_type = msg.chat_type.as_deref().unwrap_or("p2p");
    let is_group = summon_core::authz::is_group_chat(chat_type);
    let sender_is_peer = sender_is_known_peer(peers, chat_type, &msg.chat_id, &msg.sender);
    let bot_open_id = if is_group { bot_open_id() } else { None };
    StaleNoticeFacts {
        is_steward: agent.is_steward(),
        sender_is_peer,
        can_talk: sender_can_talk(agent, sender_is_peer, &msg.sender, &msg.sender_union_id),
        is_bridge_command: summon_core::ipc::bridge_command(
            &msg.text,
            &msg.mentions,
            is_group,
            false,
        )
        .is_some(),
        auto_captures: CliDriver::from_kind(agent.cli_kind()).auto_captures(),
        mention: group_mention_admission(is_group, bot_open_id.as_deref(), &msg.mentions),
        create_ms,
        now_ms,
    }
}

/// 这条被丢的消息按时到了的话,我会不会理它。只有会理的才谈得上给提示、才计入连击。
///
/// 覆盖的是主路径上按发信人与消息本身决定「理不理」的闸;依赖运行时状态的那几道(bridge slot
/// 读不出来之类)不在这里,它们的失败本身就会在日志里留痕。
fn stale_notice_admitted(facts: &StaleNoticeFacts) -> bool {
    !facts.is_steward
        && !facts.sender_is_peer
        && facts.auto_captures
        && matches!(facts.mention, GroupMentionAdmission::Admit)
        && (facts.can_talk || facts.is_bridge_command)
        // 闸只对 create_time 已知的消息判陈旧;未知说明判据本身出了问题,算不出过期多久。
        && facts.create_ms > 0
}

/// 一条已准入的陈旧消息这次出不出声、说什么。`chat` 是记上这一条之后的账。
fn stale_notice_text(
    facts: &StaleNoticeFacts,
    chat: StaleNoticeChat,
    now: Instant,
) -> Option<String> {
    if chat.consecutive_stale > STALE_NOTICE_MAX_CONSECUTIVE {
        return None;
    }
    if chat
        .last_notice
        .is_some_and(|prev| now.duration_since(prev) < STALE_NOTICE_COOLDOWN)
    {
        return None;
    }
    // 过期是按本机时钟判的,文案就只说到这一层。分钟数只在超过时效门槛时报:没超过却被丢,是水位
    // 在本机时钟落后时命中,那时算出来的差值偏小。
    let lag_ms = facts.now_ms - facts.create_ms;
    Some(if lag_ms > summon_core::inbound::DEFAULT_MAX_AGE_MS {
        summon_i18n::format!("r89680e111b", lag_ms / 60_000)
    } else {
        summon_i18n::text!("r84c0349e1e").to_string()
    })
}

/// 陈旧提示的 per-agent 记账。
#[derive(Default)]
struct StaleNoticeLedger {
    chats: HashMap<String, StaleNoticeChat>,
}

impl StaleNoticeLedger {
    /// 入站闸的每个裁决都经这里记账,返回 `Some` 就是该在原消息下回的那句话。放行与丢弃的记账
    /// 收在一处、判定与写冷却在同一步,调用方只管发送。`facts` 只在判陈旧时才取(要打 HTTP)。
    fn after_gate(
        &mut self,
        admission: summon_core::inbound::Admission,
        chat_id: &str,
        facts: impl FnOnce() -> StaleNoticeFacts,
        now: Instant,
    ) -> Option<String> {
        use summon_core::inbound::Admission;
        match admission {
            Admission::Admit => {
                if let Some(chat) = self.chats.get_mut(chat_id) {
                    chat.consecutive_stale = 0;
                }
                None
            }
            Admission::DropDuplicate => None,
            Admission::DropStale => {
                // 本来就不会理的消息不计连击:多 bot 群里别的 bot 的陈旧消息不该把真人那条的提示挤掉。
                let facts = facts();
                if !stale_notice_admitted(&facts) {
                    return None;
                }
                let chat = self.chats.entry(chat_id.to_string()).or_default();
                if chat
                    .last_stale
                    .is_some_and(|prev| now.duration_since(prev) > STALE_NOTICE_STREAK_WINDOW)
                {
                    chat.consecutive_stale = 0;
                }
                chat.last_stale = Some(now);
                chat.consecutive_stale = chat.consecutive_stale.saturating_add(1);
                let notice = stale_notice_text(&facts, *chat, now)?;
                chat.last_notice = Some(now);
                Some(notice)
            }
        }
    }
}

const PEER_TRIGGER_WINDOW: Duration = Duration::from_secs(60);
const PEER_TRIGGER_MAX_PER_WINDOW: usize = 3;
const PEER_TRIGGER_COOLDOWN: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerTriggerAdmission {
    Admit,
    /// 本条触发熔断，调用方应给人类发一次无 mention 提示。
    Tripped,
    /// 已在冷却期，静默丢弃即可，避免每条循环消息都刷提示。
    CoolingDown,
}

#[derive(Default)]
struct PeerTriggerState {
    recent: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

/// per-agent、per-chat 的 peer-origin 硬预算。正常 bot 协作可连续三次；第四次在一分钟内到达即
/// 熔断五分钟。被丢的一环不会产生回复，机械上切断 ping-pong；真人消息完全不走本预算(判据见
/// [`counts_toward_peer_budget`])。
#[derive(Default)]
struct PeerTriggerLimiter {
    chats: HashMap<String, PeerTriggerState>,
}

impl PeerTriggerLimiter {
    fn admit(&mut self, chat_id: &str, now: Instant) -> PeerTriggerAdmission {
        let state = self.chats.entry(chat_id.to_string()).or_default();
        if state.blocked_until.is_some_and(|until| now < until) {
            return PeerTriggerAdmission::CoolingDown;
        }
        state.blocked_until = None;
        while state
            .recent
            .front()
            .is_some_and(|seen| now.saturating_duration_since(*seen) >= PEER_TRIGGER_WINDOW)
        {
            state.recent.pop_front();
        }
        if state.recent.len() >= PEER_TRIGGER_MAX_PER_WINDOW {
            state.recent.clear();
            state.blocked_until = Some(now + PEER_TRIGGER_COOLDOWN);
            return PeerTriggerAdmission::Tripped;
        }
        state.recent.push_back(now);
        PeerTriggerAdmission::Admit
    }

    fn remove(&mut self, chat_id: &str) {
        self.chats.remove(chat_id);
    }
}

/// 每个 agent 一个 dispatcher 线程:按 chat_id 多路复用到各自的 chat 线程(真并行);
/// 用 recv_timeout 兼做空闲扫描(到点发提示卡片,从不擅自 kill)。
#[allow(clippy::too_many_arguments)] // 各参数职责独立(agent/poll/debounce/fc/rx/self_tx/core),无合并意义
pub(super) fn agent_dispatcher(
    agent: Agent,
    poll: u64,
    debounce: u64,
    fc: Arc<FeishuClient>,
    epoch_identity: FeishuEpochIdentity,
    rx: Receiver<DispatchMsg>,
    self_tx: Sender<DispatchMsg>,
    core: Arc<crate::runtime::core::Core>,
    inflight: crate::runtime::inflight::InFlight,
) {
    let epoch = crate::runtime::agent_retirement::epoch(&agent.dir);
    let retired = epoch.read().unwrap_or_else(|e| e.into_inner());
    if *retired {
        return;
    }
    let _catalog_refresher = crate::codex::model_catalog::start_refreshing(&agent);
    agent_dispatcher_with_stores(
        agent,
        poll,
        debounce,
        fc,
        epoch_identity,
        rx,
        self_tx,
        core,
        inflight,
        DispatcherStores::runtime_default(),
    )
}

pub(super) struct DispatcherStores {
    pub(super) bridge: crate::bridge::store::BridgeStore,
    pub(super) outbox: crate::runtime::outbox_store::OutboxStore,
    pub(super) inbox: crate::runtime::inbox_wal::InboxWal,
    pub(super) card_actions: crate::runtime::card_action_wal::CardActionWal,
    /// `None` 使用生产 run_dir；测试传 tempdir，连 peers/identity 的只读路径也完全隔离。
    pub(super) support_root: Option<std::path::PathBuf>,
}

impl DispatcherStores {
    fn runtime_default() -> Self {
        Self {
            bridge: crate::bridge::store::BridgeStore::runtime_default(),
            outbox: crate::runtime::outbox_store::OutboxStore::at_run_dir(),
            inbox: crate::runtime::inbox_wal::InboxWal::new(run_dir().join("inbox")),
            card_actions: crate::runtime::card_action_wal::CardActionWal::at_run_dir(),
            support_root: None,
        }
    }
}

fn dispatcher_peers_store(
    storage_scope: &FeishuStorageScope,
    support_root: Option<&std::path::Path>,
) -> crate::runtime::peers_store::PeersStore {
    match support_root {
        Some(root) => crate::runtime::peers_store::PeersStore::new(
            root.join("peers").join(&storage_scope.key),
        ),
        None => crate::runtime::peers_store::PeersStore::at_run_dir(
            &storage_scope.key,
            storage_scope.allow_legacy_import,
        ),
    }
}

fn dispatcher_identity_store(
    storage_scope: &FeishuStorageScope,
    support_root: Option<&std::path::Path>,
) -> crate::runtime::identity_cache::IdentityCacheStore {
    match support_root {
        Some(root) => crate::runtime::identity_cache::IdentityCacheStore::new(
            root.join("identity-cache")
                .join(format!("{}.json", storage_scope.key)),
        ),
        None => crate::runtime::identity_cache::IdentityCacheStore::at_run_dir(
            &storage_scope.key,
            storage_scope.allow_legacy_import,
        ),
    }
}

fn boot_chat_allowed_by_bridge(
    store: &crate::bridge::store::BridgeStore,
    storage_scope: &str,
    agent: &Agent,
    chat_id: &str,
    chat_type: &str,
    purpose: &str,
) -> bool {
    if !matches!(chat_type, "group" | "topic_group") {
        return true;
    }
    if crate::remote::inbound::remote_bound(agent, chat_id) {
        log(&summon_i18n::format!("r360cf00142", agent.name));
        return false;
    }
    let target = summon_core::bridge::ScopedChatKey::new(storage_scope, &agent.name, chat_id);
    match bridge_slot_message(store, &target) {
        Ok(None) => true,
        Ok(Some(_)) => {
            log(&summon_i18n::format!("r2871c0daca", agent.name));
            false
        }
        Err(error) => {
            log(&summon_i18n::format!("r9a61327f63", agent.name));
            false
        }
    }
}

const REMOTE_UNBOUND: &str = "r63e1edad44";
const REMOTE_UNBIND_FAILED: &str = "r360c06e027";

/// 已遥控群的一条入站,按 `plan_inbound` 的裁决落地;回执都锚在这条群消息上。只动 summon 自己的
/// 记录与线程,不碰 cmux 里的进程。
fn handle_remote_inbound(
    agent: &Agent,
    fc: &Arc<FeishuClient>,
    store: &crate::remote::store::RemoteStore,
    msg: &Inbound,
    binding: &summon_core::remote::RemoteBinding,
    plan: crate::remote::inbound::InboundPlan,
) {
    use crate::remote::inbound::{InboundPlan, StartRefused};
    use crate::remote::runtime;
    let chat_id = msg.chat_id.as_str();
    let ack = |ack: CommandAck| {
        if let Err(error) = send_command_ack(
            fc,
            agent,
            msg.chat_type.as_deref().unwrap_or("group"),
            chat_id,
            command_anchor(msg),
            &ack,
        ) {
            log(&summon_i18n::format!(
                "r9546636a1a",
                agent.name,
                msg.message_id
            ));
        }
    };
    let reply = |text: &str| ack(CommandAck::Note(text.to_string()));
    let finish_ended = |notice: &str| {
        reply(notice);
        if let Err(error) = store.remove(chat_id) {
            log(&summon_i18n::format!("rf7c9d2b9ff", agent.name));
        }
    };
    match plan {
        InboundPlan::NotBound => {
            log(&summon_i18n::format!(
                "rd790e2542a",
                agent.name,
                msg.message_id
            ));
        }
        InboundPlan::EndedNotice(notice) => {
            log(&summon_i18n::format!(
                "r08a265db5e",
                agent.name,
                binding.session_id
            ));
            finish_ended(&notice);
        }
        InboundPlan::Unbind => {
            // 先删记录再叫停:叫停之后,下一条群消息不能再按这份记录把 worker 拉起来。删没删掉
            // 都叫停 —— 叫停只停 summon 自己的线程,记录还在时下一条消息会按需重新拉起。
            let removed = store.remove(chat_id);
            runtime::stop(&agent.name, chat_id);
            match removed {
                Ok(()) => {
                    log(&summon_i18n::format!(
                        "r25ef5e025d",
                        agent.name,
                        binding.session_id,
                        msg.sender
                    ));
                    ack(CommandAck::Done(
                        ::summon_core::i18n::label(REMOTE_UNBOUND).to_string(),
                    ));
                }
                Err(error) => {
                    log(&summon_i18n::format!("ra4d9165d1c", agent.name));
                    reply(::summon_core::i18n::label(REMOTE_UNBIND_FAILED));
                }
            }
        }
        InboundPlan::Submit(lines) => {
            let job = || runtime::RemoteJob {
                message_id: msg.message_id.clone(),
                anchor: msg.anchor_message_id.clone(),
                lines: lines.clone(),
                resources: msg.resources.clone(),
            };
            if runtime::submit(&agent.name, chat_id, job()) {
                return;
            }
            // 未结束的绑定却没有在跑的 worker(启动复验读不出而没起,或 worker 已退出):按需拉起。
            // 拉起前先复验身份,通过后回传从此刻的转录末尾读起,没经过复验的那段不补发。
            match runtime::start_on_demand(agent, Arc::clone(fc), binding.clone()) {
                runtime::OnDemandStart::Started => {
                    if !runtime::submit(&agent.name, chat_id, job()) {
                        reply(::summon_core::i18n::label(
                            crate::remote::inbound::NOT_READY,
                        ));
                    }
                    return;
                }
                runtime::OnDemandStart::Ended(reason) => {
                    finish_ended(&runtime::ended_notice(reason));
                    return;
                }
                runtime::OnDemandStart::Unverifiable => {
                    reply(::summon_core::i18n::label(
                        crate::remote::inbound::NOT_READY,
                    ));
                    return;
                }
                runtime::OnDemandStart::NotStarted => {}
            }
            let reloaded = store.load(chat_id);
            if let Err(error) = &reloaded {
                log(&summon_i18n::format!("r919846a668", agent.name));
            }
            match crate::remote::inbound::plan_start_refused(&reloaded) {
                StartRefused::EndedNotice(notice) => finish_ended(&notice),
                StartRefused::Reply(text) => reply(text),
            }
        }
        InboundPlan::Reply(text) => reply(&text),
    }
}

/// 喂给入站闸的 create_time。闸把 `0` 定义为「未知 → 只去重、不判时效/水位」,所以这里做的是
/// **免检映射**,而不是改写消息:`msg.create_time_ms` 全程保真(2026-08-05 排障要它)。
///
/// 两类免检:
/// - `replayed`:优雅重启重注入 / 恢复卡重放,本就由 daemon 显式发起。
/// - `reattach`:换代后接管上一代那一轮。它复用原始 message_id 与 create_time_ms(锚点要拿
///   它们与 WAL 对账),而「消息发出 → 跑了很久 → 换代 → 启动扫描」轻易就超过 max_age ——
///   不免检的话整条接管路径会被 DropStale 静默吞掉,现场只留一行「丢弃陈旧入站」。
pub(super) fn gate_create_ms(msg: &Inbound) -> i64 {
    if msg.replayed || msg.reattach {
        0
    } else {
        msg.create_time_ms
    }
}

/// agent.yaml 改了、飞书身份没变:本机快照与投递表里那份 agent 原地换新(sentinel 召唤名单、
/// `/handoff` 目标、outbox 都读投递表),在线 chat 线程在两轮之间就地重读 —— model、目录、名单
/// 这类改动不用重启 daemon。
fn agent_yaml_changed(
    core: &crate::runtime::core::Core,
    agent: &Agent,
    chats: &HashMap<String, ChatHandle>,
) {
    core.refresh_agent(agent);
    for (chat_id, handle) in chats {
        if handle.tx.send(ThreadMsg::AgentReload).is_err() {
            log(&summon_i18n::format!("r7745ccfb3d", agent.name));
        }
    }
}

/// 生产 wrapper 固定使用 runtime stores；集成测试注入同一临时根，确保真正穿过 dispatcher
/// 分支时也不会读取或污染用户的 `~/.summon`。
#[allow(clippy::too_many_arguments)]
pub(super) fn agent_dispatcher_with_stores(
    mut agent: Agent,
    poll: u64,
    debounce: u64,
    fc: Arc<FeishuClient>,
    epoch_identity: FeishuEpochIdentity,
    rx: Receiver<DispatchMsg>,
    self_tx: Sender<DispatchMsg>,
    core: Arc<crate::runtime::core::Core>,
    inflight: crate::runtime::inflight::InFlight,
    stores: DispatcherStores,
) {
    let DispatcherStores {
        bridge: bridge_store,
        outbox,
        inbox: inbox_wal,
        card_actions,
        support_root,
    } = stores;
    let daemon_identity = agent.clone();
    let mentionless_queue = super::mentionless::Queue::new(fc.clone(), self_tx.clone());
    let mut mentionless_setup_queue =
        super::mentionless_setup::Queue::new(fc.clone(), self_tx.clone());
    // 观测面登记:掐入站的就是这个 epoch,`summon doctor` 经 control.sock 取它与盘上比对。
    // 此前「凭据已漂移 → 外部入站 fail-closed」只有 daemon.log 一行,而用户不翻日志:
    // agent 聋了,doctor 却报全绿。只上报指纹摘要,凭据本身永不出 daemon。
    crate::observe::runtime_status::register_credential_identity(
        &daemon_identity.name,
        epoch_identity.fingerprint(),
    );
    let mut route_identity_blocked = false;
    let mut resolved_identity_blocked = false;
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    // 入站去重+时效闸(per-agent、单线程持有无需锁):挡飞书 at-least-once 在网络抖动重连后补投的
    // 重复/陈旧事件(如很久前发、抖动后才到的「继续」)。check 判定、record 在确定要处理后记账。
    let mut inbound_gate = summon_core::inbound::InboundGate::default();
    // 已 introduce peer 的消息获得 talk 能力，但必须有机械防环预算；真人消息不计入。
    let mut peer_trigger_limiter = PeerTriggerLimiter::default();
    // 本代已经被点过的上下文提醒卡(见 `cards::handle_context_warn_card`)。
    let mut context_warn_claimed = std::collections::HashSet::new();
    // 被闸丢掉的入站消息给用户留话的记账(冷却、连击)。
    let mut stale_notices = StaleNoticeLedger::default();
    // R5:重启后预热去重集——把本 agent 近期已消费的 message_id(入站 WAL consumed/)灌进 seen。daemon
    // 重启正是飞书重投未 ACK 事件的高发时刻,内存 seen 清零会让重投消息被二次执行(重复回复/表情)。
    {
        let ids =
            inbox_wal.recent_consumed_ids(&agent.name, summon_core::inbound::DEFAULT_SEEN_CAPACITY);
        if !ids.is_empty() {
            log(&summon_i18n::format!("rca4710a19c", agent.name, ids.len()));
            inbound_gate.preheat_seen(ids);
        }
    }
    // 昵称热重载:dispatcher 解析 who 后只把 who 传给 chat 线程(线程不碰 aliases/contact)。
    let mut aliases = agent.aliases.clone();
    let mut manifest_mtime = agent.manifest_mtime();
    // 消息层配置(PRIMER/表情/模板/idle)= 内置默认 + 全局 config.yaml + agent.yaml 覆盖,mtime 热重载。
    let cfg_path = summon_core::config::config_path();
    let mut cfg_mtime = file_mtime(&cfg_path);
    let mut effective_language = summon_core::i18n::language();
    let mut effective =
        summon_core::i18n::with_language(effective_language, || resolve_messaging(&agent));
    // 裸名 soul-<alias> 归 owner 私聊(向后兼容、复用现存会话)。owner = allowed_users 首位;
    // 若 allowlist 为空(allow-all,如 starlight),无显式 owner —— 由**首个** p2p chat 认领裸名
    // (owner 通常是重启后第一个发消息的人),其余 p2p 与所有群一律带后缀。否则现存会话会被孤立。
    let mut owner = agent_owner(&agent);
    let mut primary_p2p_chat: Option<String> = None;
    // open_id 是 per-app 身份：peers/identity cache 必须与当前 agent+app 同 scope。旧全局数据只在
    // 单飞书 agent 时可无歧义导入，多 agent 升级时保留旧文件但不读取，等待重新 /introduce/入站重建。
    let storage_scope = epoch_identity.storage_scope().clone();
    let peers_store = dispatcher_peers_store(&storage_scope, support_root.as_deref());
    let identity_cache = dispatcher_identity_store(&storage_scope, support_root.as_deref());
    if peers_store.has_ambiguous_legacy_data() || identity_cache.has_ambiguous_legacy_data() {
        log(&summon_i18n::format!("r91c62805af", agent.name));
    }
    // 冷启预热:把盘上已知的 open_id→name 灌进 FeishuClient,让第一条入站消息就有真名可用
    // (取名已无网络路径,不预热就得等群成员列表异步补齐,首条必是占位)。
    // best-effort——缓存损坏/缺失只是回到「等异步补齐」,不该阻断 dispatcher 起飞。
    fc.prime_names(
        identity_cache
            .load()
            .entries
            .into_iter()
            .map(|(open_id, entry)| (open_id, entry.name)),
    );
    // 离线拆除/群散出站与入站 WAL 均来自同一 dispatcher store bundle；生产绑定 runtime root，
    // 测试绑定 tempdir，避免跨测试或读取用户状态。
    // R8:空闲扫描节拍——与消息到达解耦(见循环尾)。初值为 now,首个 idle_scan 周期后才首扫。
    let mut last_scan = std::time::Instant::now();
    // chat worker 的 receiver 已断时，重建前会把当前已入队事件扫一遍，提取旧 worker 的
    // ChatExited 作为退出屏障；其余事件原序放回这里，下一轮优先处理，不能因修复一次 send
    // failure 而吞掉或重排其他 chat 的工作。
    let mut deferred_dispatches = VecDeque::new();

    // 升级迁移(终审阻塞,2026-08-13),**必须在下面两个 boot 恢复扫描之前**:恢复出的
    // handle 在 spawn 时从 durable marker 账本读 idle_prompted,老版本发过卡的会话若此刻
    // 还没补上 marker,就会被恢复成「未提醒」,首轮空闲扫描一次性给所有已到点会话再发一张
    // (owner 报告事故的完整复演)。顺序由 tests.rs 源码守卫钉死;「迁移未完成不进扫描」
    // 则由类型钉死 —— run_idle_scan 以 IdleMarkersMigrated 为必填参数,失败分支拿不到它。
    // 部分失败:本进程整个生命周期不进空闲扫描(不发卡、也不做外杀探测/回收,fail-closed
    // 到底 —— 账本都写不动的盘,不该做任何主动出站),下次启动从缺的地方续跑。
    let idle_scan_proof = match ensure_idle_prompt_marker_migration(&agent) {
        Ok(proof) => Some(proof),
        Err(e) => {
            log(&summon_i18n::format!("rac609c43cd", agent.name));
            None
        }
    };

    // R3:重启续跑不再惰性——启动即扫 pending.json(优雅停机被打断的无头轮),主动拉起命中 chat
    // 的线程;chat 线程入口的 resume_pending_turn 据此自动续跑(顺带恢复孤儿 working 表情)。
    // 此前唯一 spawn 点是该 chat 再来新 Inbound,而等答案的用户恰恰不会再发消息。崩溃路径不受影响:
    // boot_recover_inbox 在 was_crash 时已清 pending.json(恢复卡确认重跑,不自动),扫不到。
    for (chat_id, chat_type) in pending_chats_on_disk(&agent, &storage_scope.key, &inbox_wal) {
        if chats.contains_key(&chat_id) {
            continue;
        }
        if !boot_chat_allowed_by_bridge(
            &bridge_store,
            &storage_scope.key,
            &agent,
            &chat_id,
            &chat_type,
            summon_i18n::text!("rce34f3c779"),
        ) {
            continue;
        }
        // 会话名优先用盘上 .meta 记录的(owner 裸名等历史命名),缺失再按规则重derive。
        let tmux_name = chat_session_on_disk(&agent, &chat_id)
            .map(|(session, _, _)| session)
            .unwrap_or_else(|| agent.tmux_name_for_chat(&chat_type, &chat_id, false));
        log(&summon_i18n::format!("r538e825211", agent.name));
        let mut handle = spawn_chat_thread(
            agent.clone(),
            storage_scope.key.clone(),
            tmux_name,
            poll,
            debounce,
            fc.clone(),
            epoch_identity.clone(),
            chat_id.clone(),
            chat_type,
            self_tx.clone(),
            core.clone(),
            inflight.clone(),
        );
        handle.local_claimed = true;
        chats.insert(chat_id, handle);
    }

    // claude/Transcript 的 mirror 只随 chat worker 存活。这里用 clean-stop byte fence 提前拉起，
    // 让历史 backlog 以 observation-only 收敛 offset、未来 live attach 立即有人监听。stop fence 不是
    // delivery ledger，绝不把历史 Summon turn 主动补发。反熵前先定格孤儿过程卡。
    finalize_orphan_stream_cards(&agent, &fc);
    for (chat_id, chat_type) in mirror_behind_chats_on_disk(&agent) {
        if chats.contains_key(&chat_id) {
            continue;
        }
        if !boot_chat_allowed_by_bridge(
            &bridge_store,
            &storage_scope.key,
            &agent,
            &chat_id,
            &chat_type,
            summon_i18n::text!("re90a04cdcf"),
        ) {
            continue;
        }
        let tmux_name = chat_session_on_disk(&agent, &chat_id)
            .map(|(session, _, _)| session)
            .unwrap_or_else(|| agent.tmux_name_for_chat(&chat_type, &chat_id, false));
        log(&summon_i18n::format!("r234fddb9d0", agent.name));
        let mut handle = spawn_chat_thread(
            agent.clone(),
            storage_scope.key.clone(),
            tmux_name,
            poll,
            debounce,
            fc.clone(),
            epoch_identity.clone(),
            chat_id.clone(),
            chat_type,
            self_tx.clone(),
            core.clone(),
            inflight.clone(),
        );
        handle.local_claimed = true;
        chats.insert(chat_id, handle);
    }

    loop {
        let dispatch = match deferred_dispatches.pop_front() {
            Some(message) => Ok(message),
            // 等「距下次扫描还剩多久」而非一整个周期——否则每条消息都把唤醒推后一拍,
            // 扫描间隔抖到 2×idle_scan(见 next_recv_timeout 的 why)。
            None => rx.recv_timeout(next_recv_timeout(
                effective.idle_scan(),
                last_scan.elapsed(),
            )),
        };
        // 热重载对所有事件生效:卡片按钮回调也会用最新 operators/allowed_users 判权。
        let cur_manifest = agent.manifest_mtime();
        let cur_cfg = file_mtime(&cfg_path);
        if cur_manifest != manifest_mtime || cur_cfg != cfg_mtime {
            // operators 等策略可热重载；name/app/scope 与 sidecar/client/socket 生命周期绑定，
            // 不能在 daemon epoch 中切换。损坏、inactive 或 identity 漂移都阻断外部入站。
            match agent.reread() {
                Some(fresh) if fresh.is_routable() && epoch_identity.matches_current(&fresh) => {
                    agent = fresh;
                    route_identity_blocked = false;
                    if cur_manifest != manifest_mtime {
                        agent_yaml_changed(&core, &agent, &chats);
                    }
                }
                Some(_) => {
                    route_identity_blocked = true;
                    log(&summon_i18n::format!("r3bb095e289", daemon_identity.name));
                }
                None => {
                    route_identity_blocked = true;
                    log(&summon_i18n::format!("r4f1be96234", daemon_identity.name));
                }
            }
            aliases = agent.aliases.clone();
            effective = resolve_messaging(&agent);
            owner = agent_owner(&agent);
            if cur_cfg != cfg_mtime {
                // 外部工具路径(`tools.*`)是进程级快照,spawn 时只读它;config.yaml 改了就重装。
                let fresh_config = summon_core::config::Config::load();
                summon_core::tools::configure(&fresh_config);
                crate::runtime::localization::reload(&fresh_config);
            }
            manifest_mtime = cur_manifest;
            cfg_mtime = cur_cfg;
            log(&summon_i18n::format!("r6017ee6475", agent.name));
        }
        let language = summon_core::i18n::language();
        if language != effective_language {
            effective = summon_core::i18n::with_language(language, || resolve_messaging(&agent));
            effective_language = language;
        }
        let resolved_identity_matches = epoch_identity.matches_current(&agent);
        if !resolved_identity_matches && !resolved_identity_blocked {
            log(&summon_i18n::format!("r0aa0450c60", daemon_identity.name));
        }
        resolved_identity_blocked = !resolved_identity_matches;
        let dispatch_identity_blocked = route_identity_blocked || resolved_identity_blocked;
        let (dispatch, mentionless_verdict) = match dispatch {
            Ok(DispatchMsg::MentionlessChecked(checked)) => {
                let (msg, verdict) = checked.into_verdict(&agent, manifest_mtime);
                (Ok(DispatchMsg::Inbound(msg)), Some(verdict))
            }
            other => (other, None),
        };
        let mentionless_admitted = matches!(mentionless_verdict, Some(Ok(())));
        // **管家只认三类消息**:入站(自己回卡片)、卡片回调(自己重渲)、停机。其余全部丢弃 ——
        // 那些分支(sentinel 召唤、定时器 fire、孤儿任务回收…)通往会话层,而管家一步都不该
        // 走进去。写成**白名单**而不是逐个拦:新增一种消息时默认被挡在外面,而不是默认放行。
        //
        // **只对真收到的消息判定(`dispatch.is_ok()`)。** `recv_timeout` 超时返回的是
        // `Err(Timeout)`,它同样不匹配那三个 `Ok(...)` 模式 —— 少了这个前置条件,每次空闲超时
        // 都会被当成「非观测类消息」`continue` 掉,于是跳过循环尾的 idle scan 与 `last_scan`
        // 重置;`last_scan` 永不更新,`next_recv_timeout` 就恒返回 0,`recv_timeout(0)` 立刻
        // 返回、下一轮又是 0 —— 一个 100% 空转的忙循环。
        //
        // 2026-08-10 实测:owner 报 `summond` 占 70% CPU,正是 commander(steward)这一个
        // dispatcher 线程在这里空转。超时不是消息,不该走消息白名单。
        if agent.is_steward()
            && dispatch.is_ok()
            && !matches!(
                dispatch,
                Ok(DispatchMsg::Inbound(_)) | Ok(DispatchMsg::Card(_)) | Ok(DispatchMsg::Shutdown)
            )
        {
            if let Ok(m) = &dispatch {
                log(&summon_i18n::format!(
                    "r742a063454",
                    agent.name,
                    m.kind_label()
                ));
            }
            continue;
        }
        match dispatch {
            Ok(DispatchMsg::MentionlessSetupChecked(checked)) => {
                let msg = checked.message().clone();
                let from_dashboard = checked.asked_by_dashboard();
                mentionless_setup_queue.complete(&checked);
                let reply = checked.finish(&agent, |current| {
                    !route_identity_blocked && epoch_identity.matches_current(current)
                });
                let _ = self_tx.send(DispatchMsg::AgentYamlWritten);
                // 控制台发起的核验:结果已交回页面,控制台不往群里发消息。
                if from_dashboard {
                    continue;
                }
                let _ = send_command_ack(
                    &fc,
                    &agent,
                    msg.chat_type.as_deref().unwrap_or("group"),
                    &msg.chat_id,
                    command_anchor(&msg),
                    &reply,
                );
                continue;
            }
            Ok(DispatchMsg::DashboardMentionless(request)) => {
                mentionless_setup_queue.submit_from_dashboard(
                    &agent,
                    request,
                    manifest_mtime,
                    dispatch_identity_blocked,
                );
                continue;
            }
            Ok(DispatchMsg::MentionlessChecked(_)) => {
                unreachable!("{}", summon_i18n::format!("ra485064d93"))
            }
            Ok(DispatchMsg::Retire(reply)) => {
                if !chats.is_empty() || inflight.has_agent(&agent.name) {
                    let _ = reply.send(Err(summon_i18n::text!("r3acf57f929").into()));
                    continue;
                }
                if reply.send(Ok(())).is_ok() {
                    break;
                }
            }
            Ok(DispatchMsg::Shutdown) => {
                log(&summon_i18n::format!("r928fae447c", agent.name));
                break;
            }
            Ok(DispatchMsg::Inbound(msg)) if dispatch_identity_blocked => {
                inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                let _ = send_markdown_archived(
                    &fc,
                    &daemon_identity.dir,
                    msg.chat_type.as_deref().unwrap_or("p2p"),
                    &msg.chat_id,
                    summon_i18n::text!("rb945b3d4b1"),
                );
                continue;
            }
            Ok(DispatchMsg::Card(card)) if dispatch_identity_blocked => {
                let _ = send_markdown_archived(
                    &fc,
                    &daemon_identity.dir,
                    "p2p",
                    &card.chat_id,
                    summon_i18n::text!("rb945b3d4b1"),
                );
                if card.wal_id.is_some() {
                    // 当前 daemon epoch 无法证明 callback 所属 app/scope；durable action 留待
                    // 身份恢复后的冷启重放，不能把“未处理”误记成终态。
                    log(&summon_i18n::format!(
                        "ra8cb4b3336",
                        agent.name,
                        card.wal_id.as_deref().unwrap_or("<none>")
                    ));
                }
                continue;
            }
            Ok(DispatchMsg::Inbound(msg)) => {
                match inbox_wal.is_recovery_original(&agent.name, &msg.message_id) {
                    Ok(true) => {
                        log(&summon_i18n::format!(
                            "r7a5ff6f26b",
                            agent.name,
                            msg.message_id,
                            msg.chat_id
                        ));
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        log(&summon_i18n::format!("r03931000ed", agent.name));
                        continue;
                    }
                }
                // 入站去重+时效闸(纯判定,不改状态):挡飞书抖动重连后补投的重复/陈旧事件。命中即丢、
                // 不投 chat 线程。记账(record)推迟到通过「要不要应答」门控之后,避免被丢消息污染水位。
                let now_ms = now_unix_ms() as i64;
                let gate_create_ms = gate_create_ms(&msg);
                let admission =
                    inbound_gate.check(&msg.message_id, &msg.chat_id, gate_create_ms, now_ms);
                let stale_notice = stale_notices.after_gate(
                    admission,
                    &msg.chat_id,
                    || {
                        stale_notice_facts(
                            &agent,
                            &msg,
                            &peers_store,
                            || fc.bot_open_id().ok(),
                            gate_create_ms,
                            now_ms,
                        )
                    },
                    Instant::now(),
                );
                match admission {
                    summon_core::inbound::Admission::DropDuplicate => {
                        log(&summon_i18n::format!(
                            "r9b9d71d4ff",
                            agent.name,
                            msg.message_id,
                            msg.chat_id
                        ));
                        continue;
                    }
                    summon_core::inbound::Admission::DropStale => {
                        log(&summon_i18n::format!(
                            "rb039d3a0a5",
                            agent.name,
                            msg.message_id,
                            msg.chat_id,
                            msg.create_time_ms
                        ));
                        if let Some(text) = stale_notice {
                            let chat_type = msg.chat_type.as_deref().unwrap_or("p2p");
                            if let Err(error) = send_markdown_archived_replying(
                                &fc,
                                &agent.state_dir(),
                                chat_type,
                                &msg.chat_id,
                                stale_notice_reply_target(&msg),
                                &text,
                            ) {
                                // 提示发不出去不该牵连别的消息,日志留证即可。
                                log(&summon_i18n::format!(
                                    "rdf28701855",
                                    agent.name,
                                    msg.chat_id
                                ));
                            }
                        }
                        continue;
                    }
                    summon_core::inbound::Admission::Admit => {}
                }
                // `/introduce` 认识的成员只在**同 chat**获得 talk 能力，不能借另一群的关系旁路；
                // 所有 operate 分支仍独立调用 can_operate，不读取 peers，保持 fail-closed。
                let sender_is_peer = sender_is_known_peer(
                    &peers_store,
                    msg.chat_type.as_deref().unwrap_or("p2p"),
                    &msg.chat_id,
                    &msg.sender,
                );
                // 在 @ 门控之前补记:同事在群里说话多半不 @ bot,等到 @ 才记,身份会一直悬着。
                if sender_is_peer {
                    record_peer_kind(&peers_store, &agent, &msg);
                }
                // 自动应答门控只挡**未支持的 CLI**(Other)。claude/cursor/codex 都有捕获路径
                // (Transcript / Subprocess),codex 经 Subprocess 自动捕获——旧 `captures()` 门控
                // 对 codex 返 false 把它误挡在外,导致 codex agent 静默不答。
                // 显式 @ 与一人一 Bot 的按群免 @ 共用后续准入；身份不可用时静默拒收。
                let is_group_msg = matches!(
                    msg.chat_type.as_deref().unwrap_or("p2p"),
                    "group" | "topic_group"
                );
                let (bot_open_id, bot_identity_error) = if is_group_msg {
                    match fc.bot_open_id() {
                        Ok(me) => (Some(me), None),
                        Err(e) => (None, Some(e)),
                    }
                } else {
                    (None, None)
                };
                if is_group_msg
                    && mentionless_verdict.is_none()
                    && super::mentionless::candidate(&agent, &msg)
                {
                    let chat_id = msg.chat_id.clone();
                    if !mentionless_queue.submit(&agent, msg, manifest_mtime) {
                        log(&summon_i18n::format!("rcff350cfb0", agent.name));
                    }
                    continue;
                }
                let mention_admission =
                    group_mention_admission(is_group_msg, bot_open_id.as_deref(), &msg.mentions);
                let mention_admission = if mention_admission
                    == GroupMentionAdmission::MissingSelfMention
                    && mentionless_admitted
                {
                    GroupMentionAdmission::Admit
                } else {
                    mention_admission
                };
                match mention_admission {
                    GroupMentionAdmission::Admit => {}
                    GroupMentionAdmission::MissingSelfMention => {
                        match &mentionless_verdict {
                            Some(Err(rejection)) => log(&summon_i18n::format!(
                                "rdde971aa70",
                                agent.name,
                                msg.chat_id,
                                msg.message_id,
                                msg.sender
                            )),
                            _ => log(&summon_i18n::format!(
                                "rf6efeac2f4",
                                agent.name,
                                msg.chat_id,
                                msg.sender
                            )),
                        }
                        continue;
                    }
                    GroupMentionAdmission::BotIdentityUnavailable => {
                        let mentionless_note = match &mentionless_verdict {
                            Some(Ok(())) => summon_i18n::text!("rb4d065ebe2").to_string(),
                            Some(Err(rejection)) => summon_i18n::format!("rb84258e719"),
                            None => String::new(),
                        };
                        log(&summon_i18n::format!(
                            "r50748d5b3f",
                            agent.name,
                            msg.chat_id,
                            msg.sender,
                            bot_identity_error
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_else(|| summon_i18n::text!("ref3e902bed").to_string())
                        ));
                        continue;
                    }
                }
                if !msg.reattach {
                    let may_reply = disabled_notice_allowed(&agent, sender_is_peer, &msg);
                    let admitted = admit_agent_inbound(
                        &inbox_wal,
                        &agent.name,
                        &storage_scope.key,
                        &msg,
                        may_reply,
                    );
                    match admitted {
                        Ok(true) => {}
                        Ok(false) => {
                            inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                            continue;
                        }
                        Err(error) => {
                            log(&summon_i18n::format!("r5fa7d20edd", agent.name));
                            continue;
                        }
                    }
                }
                if agent.is_steward() {
                    let chat_type = msg.chat_type.clone().unwrap_or_else(|| "p2p".to_string());
                    steward::handle_inbound(
                        &agent,
                        &fc,
                        &chat_type,
                        &msg.chat_id,
                        &msg.text,
                        &msg.mentions,
                        &msg.sender,
                        &msg.sender_union_id,
                    );
                    continue;
                }
                // Bridge 是 operate 控制面，不受一般 talk allowlist 约束：operators 与
                // allowed_users 可以解耦。群里说给本 bot(@ 本 bot 或一人一 Bot 免 @ 放行)已在
                // 上面 fail-closed 验证；私聊短码再从 ticket 恢复目标群并强制 is_group=true
                // operate。命中后绝不进入 ChatHandle/CLI。
                if let Some(command) = summon_core::ipc::bridge_command(
                    &msg.text,
                    &msg.mentions,
                    is_group_msg,
                    mentionless_admitted,
                ) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    handle_bridge_command_with_store(
                        command,
                        &agent,
                        &fc,
                        &storage_scope,
                        &msg,
                        &chats,
                        &inbox_wal,
                        &inflight,
                        &bridge_store,
                        &outbox,
                    );
                    continue;
                }
                match inbound_front_admission(
                    &agent,
                    sender_is_peer,
                    &msg.sender,
                    &msg.sender_union_id,
                    is_group_msg,
                ) {
                    InboundFrontAdmission::Admit | InboundFrontAdmission::NeedBotIdentity => {}
                    InboundFrontAdmission::DenyTalk => {
                        log(&summon_i18n::format!("rdbe15adc43", agent.name, msg.sender));
                        continue;
                    }
                }
                if let Some(command) = super::mentionless_setup::command(&msg) {
                    use super::mentionless_setup::{self, Command};
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    // 只有群里真人发的命令值得判权，私聊和 bot 不为它去查 union_id。
                    let msg = if is_group_msg && msg.sender_type == "user" {
                        mentionless_setup::with_operator_union_id(&msg, || {
                            fc.resolve_union_id(&msg.sender)
                        })
                    } else {
                        msg.clone()
                    };
                    let reply = if !is_group_msg {
                        Some(CommandAck::Note(
                            summon_i18n::text!("r8e6ef381a7").to_string(),
                        ))
                    } else if let Some(reason) = mentionless_setup::denial(&agent, &msg) {
                        Some(CommandAck::Note(reason))
                    } else if !command_is_addressed(
                        true,
                        bot_open_id.as_deref(),
                        &msg.text,
                        &msg.mentions,
                        mentionless_admitted,
                    ) {
                        Some(CommandAck::Note(
                            summon_i18n::text!("r0139ee0f43").to_string(),
                        ))
                    } else {
                        match command {
                            Command::Help => Some(CommandAck::Note(
                                ::summon_core::i18n::label(mentionless_setup::HELP).into(),
                            )),
                            Command::Off => {
                                mentionless_setup_queue.cancel(&msg.chat_id);
                                Some(
                                    match mentionless_setup::turn_off_checked(
                                        &agent,
                                        &msg,
                                        manifest_mtime,
                                        |current| epoch_identity.matches_current(current),
                                    ) {
                                        Ok(()) => {
                                            let _ = self_tx.send(DispatchMsg::AgentYamlWritten);
                                            CommandAck::Done(
                                                summon_i18n::text!("rf35e9ef145").into(),
                                            )
                                        }
                                        Err(error) => {
                                            CommandAck::Note(summon_i18n::format!("r3f56faca6e"))
                                        }
                                    },
                                )
                            }
                            Command::On | Command::Permissions => mentionless_setup_queue
                                .submit(&agent, msg.clone(), command, manifest_mtime)
                                .err()
                                .map(|error| CommandAck::Note(error.to_string())),
                        }
                    };
                    if let Some(reply) = reply {
                        let _ = send_command_ack(
                            &fc,
                            &agent,
                            msg.chat_type.as_deref().unwrap_or("group"),
                            &msg.chat_id,
                            command_anchor(&msg),
                            &reply,
                        );
                    }
                    continue;
                }
                if counts_toward_peer_budget(sender_is_peer, &msg.sender_type) {
                    match peer_trigger_limiter.admit(&msg.chat_id, Instant::now()) {
                        PeerTriggerAdmission::Admit => {}
                        PeerTriggerAdmission::Tripped => {
                            log(&summon_i18n::format!(
                                "r40a3207010",
                                agent.name,
                                msg.chat_id,
                                msg.sender,
                                PEER_TRIGGER_COOLDOWN.as_secs()
                            ));
                            let chat_type = msg.chat_type.as_deref().unwrap_or("p2p");
                            let _ = send_markdown_archived(
                                &fc,
                                &agent.state_dir(),
                                chat_type,
                                &msg.chat_id,
                                summon_i18n::text!("r5ab06ff86c"),
                            );
                            continue;
                        }
                        PeerTriggerAdmission::CoolingDown => {
                            log(&summon_i18n::format!(
                                "ra53aed516c",
                                agent.name,
                                msg.chat_id,
                                msg.sender
                            ));
                            continue;
                        }
                    }
                }
                // Bridge slot 是群本地入口的预占 CAS。必须在 ChatHandle/tmux/session/WAL 之前拦截；
                // state 损坏同样 fail-closed，不能把“读不到绑定”解释为 Unbound 后 cold-start。
                if is_group_msg {
                    let target = summon_core::bridge::ScopedChatKey::new(
                        &storage_scope.key,
                        &agent.name,
                        &msg.chat_id,
                    );
                    match bridge_slot_message(&bridge_store, &target) {
                        Ok(Some(message)) => {
                            inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                            let _ = send_markdown_archived(
                                &fc,
                                &agent.state_dir(),
                                msg.chat_type.as_deref().unwrap_or("group"),
                                &msg.chat_id,
                                message,
                            );
                            continue;
                        }
                        Err(error) => {
                            inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                            log(&summon_i18n::format!("r0ac843b41f", agent.name));
                            continue;
                        }
                        Ok(None) => {}
                    }
                }
                // 遥控中的群:消息交给遥控 worker,不建 ChatHandle、不起 tmux、不进本地会话。记录
                // 读不出同样拦下:当成没绑定会让这条落进本地会话。
                if is_group_msg {
                    let remote_store = crate::remote::store::RemoteStore::for_agent(&agent);
                    let binding = match remote_store.load(&msg.chat_id) {
                        Ok(binding) => binding,
                        Err(error) => {
                            inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                            log(&summon_i18n::format!(
                                "recde806ec9",
                                agent.name,
                                msg.chat_id,
                                msg.message_id
                            ));
                            let _ = send_markdown_archived_anchored(
                                &fc,
                                &agent.state_dir(),
                                msg.chat_type.as_deref().unwrap_or("group"),
                                &msg.chat_id,
                                command_anchor(&msg),
                                summon_i18n::text!("r2f3a376700"),
                            );
                            continue;
                        }
                    };
                    if let Some(binding) = binding {
                        inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                        let message = summon_core::remote::remote_message(
                            &msg.text,
                            &msg.mentions,
                            bot_open_id.as_deref(),
                        );
                        // 只有解除遥控要鉴权;别的消息不为它去查 union_id。
                        let can_operate =
                            matches!(message, Ok(summon_core::remote::RemoteMessage::Reset))
                                && agent.can_operate(
                                    &msg.sender,
                                    &operator_union_id(&msg.sender_union_id, || {
                                        fc.resolve_union_id(&msg.sender)
                                    }),
                                    true,
                                    OperateCommand::SessionTeardown.scope(&msg.chat_id),
                                );
                        let plan = crate::remote::inbound::plan_inbound(
                            Some(&binding),
                            message,
                            can_operate,
                            !msg.resources.is_empty(),
                        );
                        handle_remote_inbound(&agent, &fc, &remote_store, &msg, &binding, plan);
                        continue;
                    }
                }
                if !CliDriver::from_kind(agent.cli_kind()).auto_captures() {
                    log(&summon_i18n::format!(
                        "ra6830d4820",
                        agent.name,
                        agent.cli_kind(),
                        agent.name
                    ));
                    continue;
                }
                // 发信人是不是另一个 bot。多 bot 同群互 @ 时,agent 判断「要不要应答」依赖这个
                // 信号;缺了它 bot 和真人在 prompt 里长得一模一样,防回音无从下手。
                // 判据必须同时认 `bot` 与 `app`:飞书**实测下发的是 `"bot"`**(2026-08-04 真样本
                // om_x100b000000000000000000000000003),而开放平台文档写的是 `"app"`。只认文档那个
                // 值,真机上这个标志恒为 false —— 单测用 `"app"` 构造照样全绿,一路骗到线上。
                let sender_is_bot = matches!(msg.sender_type.as_str(), "app" | "bot");
                let who = match aliases.get(&msg.sender) {
                    Some(nick) => nick.to_string(),
                    None => match fc.resolve_name_cached(&msg.sender) {
                        Some(name) => name, // 缓存命中:直接用,不阻塞 dispatcher
                        // 首次出现:**不在 dispatcher 上同步/异步拉名**。同步拉会阻塞整条路由线程
                        // (拖慢所有 chat 的派发);异步占位又只落个笼统「用户」,agent 分不清人。
                        // 改为只落**稳定兜底串**(见 [`fallback_display_name`],人侧 `用户(open_id)`、
                        // bot 侧 open_id),真名由 chat 线程**首轮同步补拉**(带短超时,只阻塞本 chat
                        // 自己那一轮 ≤3s,不卡路由;拉到即用真名、否则维持占位)。与 union_id 解析
                        // 放在 chat 线程同理:别让一次 HTTP 阻塞同 agent 其他 chat 的扇出。
                        None => fallback_display_name(&msg.sender, sender_is_bot),
                    },
                };
                let chat_type = msg.chat_type.clone().unwrap_or_else(|| "p2p".to_string());
                let is_group = matches!(chat_type.as_str(), "group" | "topic_group");
                let is_owner_p2p = !is_group
                    && match &owner {
                        // 有显式 owner:稳定按 sender 判定(与谁先发消息无关)。owner 可能是
                        // open_id 或 union_id,两种 id 任一命中即算(与 allows 双判一致)。
                        Some(o) => owner_id_matches(o, &msg.sender, &msg.sender_union_id),
                        // 无显式 owner:首个 p2p chat 认领裸名,之后认准该 chat_id
                        None => match &primary_p2p_chat {
                            Some(c) => c == &msg.chat_id,
                            None => {
                                primary_p2p_chat = Some(msg.chat_id.clone());
                                true
                            }
                        },
                    };
                let tmux_name = agent.tmux_name_for_chat(&chat_type, &msg.chat_id, is_owner_p2p);
                let chat_id = msg.chat_id.clone();
                // 走到这里的群消息已经过了 @ 门控与对话白名单:人发来的就是 owner 对这个会话的新动作。
                super::waiting::clear_on_inbound(&agent, &chat_type, &chat_id, sender_is_bot);

                let handle = chats.entry(chat_id.clone()).or_insert_with(|| {
                    log(&summon_i18n::format!("r1ee7df782c", agent.name));
                    spawn_chat_thread(
                        agent.clone(),
                        storage_scope.key.clone(),
                        tmux_name.clone(),
                        poll,
                        debounce,
                        fc.clone(),
                        epoch_identity.clone(),
                        chat_id.clone(),
                        chat_type.clone(),
                        self_tx.clone(),
                        core.clone(),
                        inflight.clone(),
                    )
                });
                // 真人/bot 入站 → 会话回到普通空闲规则(提醒卡 + idle_retire):时钟、空闲卡
                // 去重(含 durable marker)、来源代际经统一收口一起推进 —— 在途的定时
                // admission 一并作废(见 ChatHandle::note_activity,终审阻塞③)。
                handle.note_activity(&agent, &chat_id, Some(&msg.message_id));
                // `/help` 是 summon 本地只读目录，不注入 provider，也不要求原生 session 已启动。
                // 内容由公开命令目录生成，再按当前 CLI/通道追加可用的原生命令，避免静态文档漂移。
                if summon_core::ipc::help_command(&msg.text, &msg.mentions) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let (help, note) = feishu_chat_help(&agent);
                    let card =
                        summon_core::feishu::card_markdown_with_note(&help, note.unwrap_or(""));
                    if let Err(error) = send_card_archived(
                        &fc,
                        &agent.state_dir(),
                        &chat_type,
                        &chat_id,
                        command_anchor(&msg),
                        "help_card",
                        &card,
                    ) {
                        log(&summon_i18n::format!("re5338c04d1", agent.name));
                    }
                    continue;
                }
                // `/session`(群里要先 @bot)= 主动唤出会话控制面板,不注入给 agent。必须在
                // get-or-spawn 之后:这样 `chats` 里已登记本 chat 的 handle(带正确 chat_type),
                // 卡片按钮回调能按 chat_id 路由到本线程、operate-gate 也能正确判定群/私聊。
                // 线程已 spawn 但 tmux 不会冷启(只在收到 Task 时 ensure_started),代价很低。
                // `/quota` 是 `/session quota` 的短别名(owner 2026-08-19),归一化成同一个
                // SessionCommand::Quota,下面的 operate 闸与回执逻辑两种形式共用。
                let quota_alias = summon_core::ipc::quota_command(&msg.text, &msg.mentions)
                    .map(summon_core::ipc::SessionCommand::Quota);
                if let Some(sub) =
                    summon_core::ipc::session_command(&msg.text, &msg.mentions).or(quota_alias)
                {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    // `/session clean|reset` = 面板上那两个退出按钮的文字入口。破坏性,过 operate-gate
                    // (与按钮同一把闸);`/session` 唤面板本身不破坏,不需要。
                    // `/quota [on|off]`(等价 `/session quota`)= 面板「额度显示」按钮的文字入口。
                    // 与按钮同一把 operate 闸(改持久状态 + 敏感面展示);无卡片可刷,回执文字确认。
                    if let summon_core::ipc::SessionCommand::Quota(explicit) = sub {
                        let op_union = operator_union_id(&msg.sender_union_id, || {
                            fc.resolve_union_id(&msg.sender)
                        });
                        if !agent.can_operate(
                            &msg.sender,
                            &op_union,
                            is_group,
                            OperateCommand::Quota.scope(&chat_id),
                        ) {
                            log(&summon_i18n::format!("rde22667af1", agent.name, msg.sender));
                            let _ = send_markdown_archived_anchored(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                command_anchor(&msg),
                                summon_i18n::text!("rc61c8f2ba1"),
                            );
                            continue;
                        }
                        let tag = chat_tag(&chat_type, &chat_id);
                        let on =
                            explicit.unwrap_or_else(|| !cards::quota_badge_enabled(&agent, &tag));
                        let written = cards::set_quota_badge(&agent, &tag, on);
                        if written.is_ok() {
                            log(&format!(
                                "[{}] /quota → {} chat={chat_id}",
                                agent.name,
                                if on {
                                    summon_i18n::text!("r39eae64cfc")
                                } else {
                                    summon_i18n::text!("r5d0ae622f6")
                                }
                            ));
                        }
                        let _ = send_command_ack(
                            &fc,
                            &agent,
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            &quota_ack(explicit.is_some(), on, written),
                        );
                        continue;
                    }
                    let teardown = match sub {
                        summon_core::ipc::SessionCommand::Clean => Some(Control::Clean),
                        summon_core::ipc::SessionCommand::Reset => Some(Control::Archive),
                        summon_core::ipc::SessionCommand::Panel
                        | summon_core::ipc::SessionCommand::Quota(_) => None,
                    };
                    if let Some(ctrl) = teardown {
                        let op_union = operator_union_id(&msg.sender_union_id, || {
                            fc.resolve_union_id(&msg.sender)
                        });
                        if !agent.can_operate(
                            &msg.sender,
                            &op_union,
                            is_group,
                            OperateCommand::SessionTeardown.scope(&chat_id),
                        ) {
                            log(&summon_i18n::format!("r4bded86aa4", agent.name, msg.sender));
                            let _ = send_markdown_archived_anchored(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                command_anchor(&msg),
                                summon_i18n::text!("r3b2fa12203"),
                            );
                            continue;
                        }
                        if let Some(handle) = chats.get(&chat_id) {
                            let _ = handle.tx.send(ThreadMsg::Control {
                                ctrl,
                                // `/session clean|reset` 文字指令,用户主动。
                                idle_retired: false,
                                // 文字指令没有卡片可置灰;回执由 notify_exit 显式驱动。
                                card_message_id: None,
                                models: Vec::new(),
                                origin: CardOrigin::Panel,
                                notify_exit: true,
                                source_task: None,
                                source_started: false,
                            });
                        }
                        log(&format!(
                            "[{}] /session {} chat={chat_id}",
                            agent.name,
                            if matches!(ctrl, Control::Reset | Control::Archive) {
                                "reset"
                            } else {
                                "clean"
                            }
                        ));
                        continue;
                    }
                    let tag = chat_tag(&chat_type, &chat_id);
                    // 复用点:卡片渲染 model/effort 选项走 core 的统一接缝
                    // `model_effort::options_for(&effective, cli)`:per-cli 模型桶 + 该 cli 的独立
                    // effort 候选(cursor/Other 的 efforts 为空 → 卡片不渲染 effort 行)。
                    let mut opts =
                        summon_core::model_effort::options_for(&effective, agent.cli_kind());
                    // cursor 走 ACP 时候选来自会话自己报的 `availableModels`(见 `cards::model_candidates`)。
                    opts.models = cards::model_candidates(&agent, &tag, opts.models);
                    // 摘要三行(通道/模型+来源/effort+来源)。A(隐私):面板**不再展示工作目录路径**
                    // (路径不进群),故摘要原料里也不带 work_dir;查看/切换一律经「私信候选目录」私信选择器。
                    let inputs = cards::panel_summary_inputs(&agent, &tag, &effective);
                    let card = summon_core::feishu::card_session_panel(
                        &tag,
                        agent.cli_kind(),
                        inputs.channel,
                        &inputs.model,
                        &inputs.effort,
                        &opts.models,
                        &opts.efforts,
                        // 初始卡不带 subagent:dispatcher 摸不到 chat 线程的共享状态。chat 线程
                        // 收到 RenderSubagentPanelSnapshot 后补渲当时快照,并把这张卡登记为
                        // 「最新卡」交给刷新 tick 接管(owner 2026-08-14)。
                        &summon_core::subagent::SubagentDisplay::default(),
                        cards::session_panel_controls(&agent, &tag, &tmux_name, &inputs.model),
                    );
                    match deliver_session_panel(&fc, &agent, &msg, &chat_type, &card) {
                        Ok(msg_id) => {
                            // 让 chat 线程把当时的 subagent 快照补渲到这张卡上(仅此一次)。
                            if let Some(handle) = chats.get(&chat_id) {
                                let _ = handle.tx.send(ThreadMsg::RenderSubagentPanelSnapshot {
                                    card_id: msg_id,
                                });
                            }
                            log(&summon_i18n::format!("r5f77ad7495", agent.name));
                        }
                        Err(e) => log(&summon_i18n::format!("rb0fac37c51", agent.name)),
                    }
                    continue;
                }
                if let Some(argument) = super::fork_entry::command_argument(
                    &agent,
                    &msg.text,
                    &msg.mentions,
                    &effective.slash_blacklist,
                )
                .filter(|_| {
                    command_is_addressed(
                        is_group,
                        bot_open_id.as_deref(),
                        &msg.text,
                        &msg.mentions,
                        mentionless_admitted,
                    )
                }) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    let result = if sender_is_bot {
                        Err(anyhow::anyhow!(summon_i18n::format!("r2b467ef6ec")))
                    } else {
                        super::fork_entry::start(
                            &super::fork_entry::Environment {
                                agent: &agent,
                                fc: &fc,
                                chats: &chats,
                                scope: &storage_scope.key,
                            },
                            super::fork_entry::Source {
                                chat_id: &chat_id,
                                chat_type: &chat_type,
                                message_id: &msg.message_id,
                                requester: &msg.sender,
                                union_id: &union,
                            },
                            &argument,
                        )
                    };
                    if let Err(error) = result {
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            &summon_i18n::format!(
                                "r05c640407a",
                                summon_core::feishu::escape_lark_md(&error.to_string())
                            ),
                        );
                    }
                    continue;
                }
                // `/dash` = 私聊触发者一张控制台卡。就地处理的理由同下面的 `/term`:不排在卡住的那
                // 一轮后面,operate 权限按此刻的名单判。同形变体与带参形态落进 worker 由 `plan_slash`
                // 拒绝;群里没有说给本 bot 的同样交给 worker 静默忽略。
                if super::dashboard_entry::is_dashboard_command(
                    &agent,
                    &msg.text,
                    &msg.mentions,
                    &effective.slash_blacklist,
                ) && command_is_addressed(
                    is_group,
                    bot_open_id.as_deref(),
                    &msg.text,
                    &msg.mentions,
                    mentionless_admitted,
                ) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    let notice = if agent.can_operate(
                        &msg.sender,
                        &op_union,
                        is_group,
                        OperateCommand::Dash.scope(&chat_id),
                    ) {
                        let name = fc.resolve_name_cached(&msg.sender).unwrap_or_default();
                        super::dashboard_entry::send_dashboard_card(
                            &fc,
                            &super::dashboard_entry::DashRequest {
                                agent: &agent,
                                chat_id: &chat_id,
                                is_group,
                                sender_open_id: &msg.sender,
                                sender_union_id: &op_union,
                                sender_name: &name,
                            },
                        )
                    } else {
                        log(&summon_i18n::format!("r0de51d067a", agent.name, msg.sender));
                        super::dashboard_entry::DashNotice::ToSender(
                            ::summon_core::i18n::label(super::dashboard_entry::DASH_DENIED)
                                .to_string(),
                        )
                    };
                    deliver_dash_notice(&fc, &agent, &msg, &chat_type, is_group, notice);
                    continue;
                }
                // `/summon-export` = 导出本会话的排查材料,只私聊触发者。就地处理的理由同 `/term`:会话卡在
                // 一轮里的时候正是要导出的时候。同形变体与别的参数落进 worker 由 `plan_slash` 拒绝。
                if let Some(mode) = super::export_entry::export_mode(
                    &agent,
                    &msg.text,
                    &msg.mentions,
                    &effective.slash_blacklist,
                )
                .filter(|_| {
                    command_is_addressed(
                        is_group,
                        bot_open_id.as_deref(),
                        &msg.text,
                        &msg.mentions,
                        mentionless_admitted,
                    )
                }) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    if agent.can_operate(
                        &msg.sender,
                        &op_union,
                        is_group,
                        OperateCommand::Export.scope(&chat_id),
                    ) {
                        super::export_entry::spawn(
                            fc.clone(),
                            core.preview_server(),
                            super::export_entry::ExportJob {
                                agent: agent.clone(),
                                chat_id: chat_id.to_string(),
                                chat_type: chat_type.to_string(),
                                sender_open_id: msg.sender.clone(),
                                anchor: command_anchor(&msg).map(str::to_string),
                                mode,
                            },
                        );
                    } else {
                        log(&summon_i18n::format!("r5ae208e116", agent.name, msg.sender));
                        deliver_dash_notice(
                            &fc,
                            &agent,
                            &msg,
                            &chat_type,
                            is_group,
                            super::dashboard_entry::DashNotice::ToSender(
                                ::summon_core::i18n::label(super::export_entry::EXPORT_DENIED)
                                    .to_string(),
                            ),
                        );
                    }
                    continue;
                }
                // `/term` = 私聊触发者一张终端卡。在 dispatcher 就地处理,不进 worker 队列:会话卡在
                // 一轮里的时候正是要开终端的时候,排队就得等那一轮结束;operate 权限也按此刻(热加载
                // 之后)的名单判。同形变体与带参形态照旧落进 worker,由 `plan_slash` 拒绝。群里没有
                // 说给本 bot 的(见 `command_is_addressed`)不是给我们的,同样交给 worker 静默忽略。
                if super::terminal::is_terminal_command(
                    &agent,
                    &msg.text,
                    &msg.mentions,
                    &effective.slash_blacklist,
                ) && command_is_addressed(
                    is_group,
                    bot_open_id.as_deref(),
                    &msg.text,
                    &msg.mentions,
                    mentionless_admitted,
                ) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    let reply = if agent.can_operate(
                        &msg.sender,
                        &op_union,
                        is_group,
                        OperateCommand::Term.scope(&chat_id),
                    ) {
                        let session = cards::resolve_chat_session(&chats, &agent, &chat_id)
                            .unwrap_or_else(|| tmux_name.clone());
                        let req = super::terminal::IssueRequest {
                            agent: &agent,
                            chat_id: &chat_id,
                            chat_type: &chat_type,
                            tmux_session: Some(&session),
                            recipient_open_id: &msg.sender,
                        };
                        super::terminal::terminal_command_reply(
                            super::terminal::send_terminal_card(&fc, &req),
                        )
                    } else {
                        log(&summon_i18n::format!("rb415ad69c8", agent.name, msg.sender));
                        Some(::summon_core::i18n::label(super::terminal::TERM_DENIED).to_string())
                    };
                    if let Some(text) = reply {
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            &text,
                        );
                    }
                    continue;
                }
                // `/model` 是 summon 的持久模型配置，不透传给 CLI。否则尚未启动会话时它会被当作普通
                // prompt,既不改变下一轮的 --model,也可能直接报错。改本群按本群范围判权,
                // `default` 改 bot 默认按 agent 范围判权。
                if let Some(argument) = summon_core::ipc::model_command(&msg.text, &msg.mentions) {
                    use summon_core::model_effort::LayeredCommand;
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let command = summon_core::model_effort::layered_command(&argument);
                    let default_layer = matches!(
                        command,
                        LayeredCommand::Default(_) | LayeredCommand::DefaultWithoutValue
                    );
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    let operate = if default_layer {
                        OperateCommand::ModelDefault
                    } else {
                        OperateCommand::Model
                    };
                    if !agent.can_operate(&msg.sender, &op_union, is_group, operate.scope(&chat_id))
                    {
                        log(&summon_i18n::format!("r188a05160a", agent.name, msg.sender));
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            if default_layer {
                                summon_i18n::text!("re424dc9837")
                            } else {
                                summon_i18n::text!("r18f1ca938d")
                            },
                        );
                        continue;
                    }
                    let tag = chat_tag(&chat_type, &chat_id);
                    let cli_driver_is_cursor =
                        CliDriver::from_kind(agent.cli_kind()) == CliDriver::Cursor;
                    // 人很难手打对模型名：只认候选目录里的 id(claude 另认它内置的别名与当前配置的
                    // 模型),对不上就不改、回候选按钮。
                    let candidates = cards::model_candidates(
                        &agent,
                        &tag,
                        summon_core::model_effort::options_for(
                            &resolve_messaging(&agent),
                            agent.cli_kind(),
                        )
                        .models,
                    );
                    // cursor 目录上百项,按钮放不下:无参 `/model` / `/model default` 发完整目录的
                    // 下拉选择卡,分别写本群与 bot 默认(判权已按各自范围做过)。
                    if cli_driver_is_cursor
                        && matches!(
                            command,
                            LayeredCommand::Picker | LayeredCommand::DefaultWithoutValue
                        )
                    {
                        cards::reply_cursor_model_picker(
                            &agent,
                            &fc,
                            &chat_id,
                            &chat_type,
                            &candidates,
                            command_anchor(&msg),
                            default_layer,
                        );
                        continue;
                    }
                    let (input, target) = match command {
                        LayeredCommand::Picker => {
                            cards::reply_model_usage(
                                &agent,
                                &fc,
                                &chat_id,
                                &chat_type,
                                &candidates,
                                command_anchor(&msg),
                            );
                            continue;
                        }
                        LayeredCommand::DefaultWithoutValue => {
                            let _ = send_markdown_archived_anchored(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                command_anchor(&msg),
                                summon_i18n::text!("r037ca1fd66"),
                            );
                            continue;
                        }
                        LayeredCommand::Inherit => ("", cards::LayerTarget::ChatInherit),
                        LayeredCommand::Chat(input) => (input, cards::LayerTarget::Chat),
                        LayeredCommand::Default(input) => (input, cards::LayerTarget::Default),
                    };
                    if target == cards::LayerTarget::ChatInherit {
                        cards::set_model_for_chat(
                            &mut chats,
                            &agent,
                            &fc,
                            &chat_id,
                            &chat_type,
                            "",
                            command_anchor(&msg),
                            target,
                            true,
                        );
                        continue;
                    }
                    // 候选之外再认默认模型与本群当前的模型:不在候选里时换走之后还换得回来。
                    let mut known = candidates.clone();
                    known.extend(crate::runtime::chat_model::chat_model(&agent, &tag));
                    let catalog = summon_core::model_effort::text_command_catalog(
                        agent.cli_kind(),
                        &known,
                        crate::runtime::chat_model::effective(&agent, None).model(),
                    );
                    match summon_core::model_effort::model_text_command(input, &catalog) {
                        summon_core::model_effort::ModelTextCommand::Switch(id) => {
                            cards::set_model_for_chat(
                                &mut chats,
                                &agent,
                                &fc,
                                &chat_id,
                                &chat_type,
                                &id,
                                command_anchor(&msg),
                                target,
                                id.eq_ignore_ascii_case(input),
                            );
                        }
                        decision => {
                            log(&summon_i18n::format!("rb2b7d5f109", agent.name));
                            cards::reply_model_not_switched(
                                &agent,
                                &fc,
                                &chat_id,
                                &chat_type,
                                input,
                                &decision,
                                command_anchor(&msg),
                                default_layer,
                            );
                        }
                    }
                    continue;
                }
                // `/effort`:与 `/model` 同构。本群档位落进 summon 的本群配置,透传给 CLI 只会改它
                // 自己的会话(claude 还会顺带改写用户级默认)。
                if let Some(argument) = summon_core::ipc::effort_command(&msg.text, &msg.mentions) {
                    use summon_core::model_effort::LayeredCommand;
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let command = summon_core::model_effort::layered_command(&argument);
                    let default_layer = matches!(
                        command,
                        LayeredCommand::Default(_) | LayeredCommand::DefaultWithoutValue
                    );
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    let operate = if default_layer {
                        OperateCommand::EffortDefault
                    } else {
                        OperateCommand::Effort
                    };
                    let reply = |text: &str| {
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            text,
                        );
                    };
                    if !agent.can_operate(&msg.sender, &op_union, is_group, operate.scope(&chat_id))
                    {
                        log(&summon_i18n::format!("rea0478367f", agent.name, msg.sender));
                        reply(if default_layer {
                            summon_i18n::text!("r61aedfad9a")
                        } else {
                            summon_i18n::text!("r8e3f361f3e")
                        });
                        continue;
                    }
                    let efforts = summon_core::model_effort::options_for(
                        &resolve_messaging(&agent),
                        agent.cli_kind(),
                    )
                    .efforts;
                    if efforts.is_empty() {
                        reply(if cards::uses_rpc_model(&agent) {
                            summon_i18n::text!("rcdfaa1db11")
                        } else {
                            summon_i18n::text!("r7c95396730")
                        });
                        continue;
                    }
                    let levels = efforts.join(" / ");
                    let (input, target) = match command {
                        LayeredCommand::Picker => {
                            reply(&summon_i18n::format!("r6c9c956a88"));
                            continue;
                        }
                        LayeredCommand::DefaultWithoutValue => {
                            reply(&summon_i18n::format!("ra086b776c9"));
                            continue;
                        }
                        LayeredCommand::Inherit => ("", cards::LayerTarget::ChatInherit),
                        LayeredCommand::Chat(input) => (input, cards::LayerTarget::Chat),
                        LayeredCommand::Default(input) => (input, cards::LayerTarget::Default),
                    };
                    let effort = if target == cards::LayerTarget::ChatInherit {
                        Some(String::new())
                    } else {
                        summon_core::model_effort::effort_text_command(input, &efforts)
                    };
                    match effort {
                        Some(effort) => {
                            cards::set_effort_for_chat(
                                &mut chats,
                                &agent,
                                &fc,
                                &chat_id,
                                &chat_type,
                                &effort,
                                command_anchor(&msg),
                                target,
                                effort.eq_ignore_ascii_case(input),
                            );
                        }
                        None => reply(&summon_i18n::format!(
                            "ra05ed92bf1",
                            summon_core::feishu::escape_lark_md(input)
                        )),
                    }
                    continue;
                }
                // `/fast on|off`：Codex 是 per-chat service tier；Cursor-RPC 写本群参数覆盖。两者都
                // 涉及额外额度/成本，统一走 operate-gate，且只保存下一轮意图。
                // Claude Code 自带同名命令，解析器对它返回 None，落到下面的 slash 透传。
                if let Some(command) = summon_core::ipc::fast_command(
                    &msg.text,
                    &msg.mentions,
                    CliDriver::from_kind(agent.cli_kind()),
                ) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    let scope = OperateCommand::Fast.scope(&chat_id);
                    if !agent.can_operate(&msg.sender, &op_union, is_group, scope) {
                        log(&summon_i18n::format!("r5ae428272f", agent.name, msg.sender));
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            summon_i18n::text!("re298be8433"),
                        );
                        continue;
                    }
                    let on = match command {
                        summon_core::ipc::FastCommand::On => true,
                        summon_core::ipc::FastCommand::Off => false,
                        summon_core::ipc::FastCommand::Invalid => {
                            let _ = send_markdown_archived_anchored(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                command_anchor(&msg),
                                summon_i18n::text!("rb88afc6bd1"),
                            );
                            continue;
                        }
                    };
                    let tag = chat_tag(&chat_type, &chat_id);
                    let reply = match summon_core::driver::CliDriver::from_kind(agent.cli_kind()) {
                        summon_core::driver::CliDriver::Cursor => {
                            match cards::apply_cursor_config_option(
                                &agent,
                                &tag,
                                "fast",
                                Some(if on { "true" } else { "false" }),
                            ) {
                                Ok(()) => {
                                    log(&summon_i18n::format!(
                                        "rfdc67a3281",
                                        agent.name,
                                        if on { "true" } else { "false" }
                                    ));
                                    fast_ack(
                                        on,
                                        summon_i18n::text!("rfc5c921135"),
                                        summon_i18n::text!("rbddbc4075f"),
                                    )
                                }
                                Err(error) => {
                                    log(&summon_i18n::format!("rade20d87fc", agent.name));
                                    CommandAck::Note(error.to_string())
                                }
                            }
                        }
                        summon_core::driver::CliDriver::Codex => {
                            let tier = if on {
                                summon_core::driver::CodexServiceTier::Fast
                            } else {
                                summon_core::driver::CodexServiceTier::Standard
                            };
                            match cards::apply_codex_service_tier(&agent, &tag, tier) {
                                Ok(_) => {
                                    log(&format!(
                                        "[{}] /fast → {} chat={chat_id}",
                                        agent.name,
                                        tier.as_persisted_str()
                                    ));
                                    fast_ack(
                                        on,
                                        summon_i18n::text!("r182894dfa1"),
                                        summon_i18n::text!("rc5bc6986a5"),
                                    )
                                }
                                Err(e) => {
                                    log(&summon_i18n::format!("r448ba30cf6", agent.name));
                                    CommandAck::Note(e.to_string())
                                }
                            }
                        }
                        _ => CommandAck::Note(summon_i18n::text!("r62376ef77c").to_string()),
                    };
                    let _ = send_command_ack(
                        &fc,
                        &agent,
                        &chat_type,
                        &chat_id,
                        command_anchor(&msg),
                        &reply,
                    );
                    continue;
                }
                // `/mode <id>`：Cursor ACP 的 per-chat 会话模式。与卡片按钮共用动态目录校验和
                // 原子持久化；只走标准 session/set_mode，不接会改 owner 全局模型配置的参数 setter。
                if let Some(command) = summon_core::ipc::mode_command(&msg.text, &msg.mentions) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    if !agent.can_operate(
                        &msg.sender,
                        &op_union,
                        is_group,
                        OperateCommand::Mode.scope(&chat_id),
                    ) {
                        log(&summon_i18n::format!("ra1281f40ad", agent.name, msg.sender));
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            summon_i18n::text!("r8c40eb28ec"),
                        );
                        continue;
                    }
                    let requested = match command {
                        summon_core::ipc::ModeCommand::Set(mode) => mode,
                        summon_core::ipc::ModeCommand::Invalid => {
                            let _ = send_markdown_archived_anchored(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                command_anchor(&msg),
                                summon_i18n::text!("rf02ed38def"),
                            );
                            continue;
                        }
                    };
                    let tag = chat_tag(&chat_type, &chat_id);
                    let reply = match cards::apply_cursor_session_mode(&agent, &tag, &requested) {
                        Ok(()) => {
                            log(&format!(
                                "[{}] /mode → {requested} chat={chat_id}",
                                agent.name
                            ));
                            CommandAck::Done(summon_i18n::format!("ree4b3a27cd"))
                        }
                        Err(error) => {
                            log(&summon_i18n::format!("r0a8275f9c1", agent.name));
                            CommandAck::Note(error.to_string())
                        }
                    };
                    let _ = send_command_ack(
                        &fc,
                        &agent,
                        &chat_type,
                        &chat_id,
                        command_anchor(&msg),
                        &reply,
                    );
                    continue;
                }
                // `/pwd`:只读对账配置目录与运行目录，不喂 agent，也不启动 provider session。
                if summon_core::ipc::pwd_command(&msg.text, &msg.mentions) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    let target = pwd_reply_target(
                        is_group,
                        agent.can_operate(
                            &msg.sender,
                            &op_union,
                            is_group,
                            OperateCommand::Pwd.scope(&chat_id),
                        ),
                    );
                    if target == PwdReplyTarget::Denied {
                        log(&summon_i18n::format!("r7ba6b4af54", agent.name, msg.sender));
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            summon_i18n::text!("r1405cf4cfc"),
                        );
                        continue;
                    }
                    let display_plan = work_dir_plan(&agent, &tmux_name, Some(&chat_id), None)
                        .map_paths(summon_core::registry::collapse_tilde);
                    let runtime =
                        if agent.capture_mode() == summon_core::registry::CaptureMode::Transcript {
                            let tmux = crate::session::tmux::Tmux::new(tmux_name.clone());
                            if tmux.has_session() {
                                summon_core::work_dir::RuntimeWorkDir::Running(
                                    tmux.pane_current_path()
                                        .ok()
                                        .map(|path| summon_core::registry::collapse_tilde(&path)),
                                )
                            } else {
                                summon_core::work_dir::RuntimeWorkDir::Stopped
                            }
                        } else {
                            let tag = chat_tag(&chat_type, &chat_id);
                            match crate::capture::rpc_pool::shared(&inflight)
                                .handoff_snapshot(&agent.name, &tag)
                            {
                                Ok(Some(snapshot)) if snapshot.running => {
                                    summon_core::work_dir::RuntimeWorkDir::Running(
                                        snapshot.spawn_cwd.as_deref().map(|path| {
                                            summon_core::registry::collapse_tilde(path)
                                        }),
                                    )
                                }
                                Ok(_) => summon_core::work_dir::RuntimeWorkDir::Stopped,
                                Err(error) => {
                                    log(&summon_i18n::format!("r73f0ad3e03", agent.name));
                                    summon_core::work_dir::RuntimeWorkDir::Unknown
                                }
                            }
                        };
                    let fallback = std::env::current_dir()
                        .ok()
                        .map(|path| summon_core::registry::collapse_tilde(&path.to_string_lossy()))
                        .unwrap_or_else(|| ".".to_string());
                    let report =
                        summon_core::work_dir::pwd_report(&display_plan, runtime, &fallback);
                    deliver_pwd_report(&fc, &agent, &msg, &chat_type, target, &report);
                    continue;
                }
                // `/cd <路径>`:设置**本会话**工作目录,不喂 agent。必须在斜杠透传之前判,否则
                // `/cd /x` 的首 token `cd` 会被当命令样式原样转发给 CLI。Q1=C:私聊+群都要 operate 权限。
                if let Some(arg) = summon_core::ipc::cd_command(&msg.text, &msg.mentions) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    if !agent.can_operate(
                        &msg.sender,
                        &op_union,
                        is_group,
                        OperateCommand::Cd.scope(&chat_id),
                    ) {
                        log(&summon_i18n::format!("r234df85411", agent.name, msg.sender));
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            summon_i18n::text!("r76b0dd4a3e"),
                        );
                        continue;
                    }
                    if arg.is_empty() {
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            summon_i18n::text!("r8d2e102e01"),
                        );
                        continue;
                    }
                    // 校验/写盘 apply_chat_workdir(A5:与面板 set_workdir 同一实现);成功返回展示路径。
                    if let Some(shown) =
                        apply_chat_workdir(&agent, &fc, &chat_type, &chat_id, &tmux_name, &arg)
                    {
                        // 注入 /cd 到运行中的 claude tmux 会话(Claude 原生支持),实现热切换不杀进程。
                        // 注入要等当前轮结束才被 claude 消费(同 model/effort 切换行为),回执说清时机。
                        let expanded = summon_core::registry::expand_tilde(&arg);
                        let slash = format!("/cd {expanded}");
                        log(&summon_i18n::format!("rfd30e23a36", agent.name));
                        if let Err(e) = handle.tx.send(ThreadMsg::SwitchApply {
                            slash: Some(slash),
                            receipt: CommandAck::Done(summon_i18n::format!("r23430a3702")),
                            anchor: command_anchor(&msg).map(str::to_string),
                            confirm_model: None,
                        }) {
                            log(&summon_i18n::format!("reb4c12bdce", agent.name));
                        }
                    }
                    continue;
                }
                // `/schedules`:列出定时器(只读,talk 即可,不喂 agent)。每条启用项带「取消」按钮,
                // 点了经 operate-gate → 停用 → 原地刷新本卡(见 Card 分支 schedule_cancel)。
                if summon_core::ipc::is_schedule_command(&msg.text, &msg.mentions) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let all = match crate::schedule::schedule_store::ScheduleStore::at_summon_home()
                        .load()
                    {
                        Ok(all) => all,
                        Err(e) => {
                            let _ = send_markdown_archived_anchored(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                command_anchor(&msg),
                                &summon_i18n::format!("r706a2650ed"),
                            );
                            continue;
                        }
                    };
                    // schedules.yaml 是全局单文件,列表也必须按 (agent, chat) 归属过滤:
                    // 否则本会话能看到别的 agent/会话的定时器 prompt 正文,且初始列表与
                    // 取消后的刷新列表(cards.rs:199 已过滤)条目数对不上。
                    let visible =
                        summon_core::authz::schedules_owned_by(&all, &agent.name, &chat_id);
                    // 面板上的「定时归档」开关反映本 chat 当前生效值(per-chat 覆盖优先)。
                    let timer_reset = resolve_settings(
                        &agent,
                        &sidecar_session_key(
                            chats.get(&chat_id).map(|h| h.tmux_name.as_str()),
                            &agent,
                            &chat_type,
                            &chat_id,
                        ),
                        &effective,
                    )
                    .timer_idle_retire_reset();
                    let card = summon_core::feishu::card_schedules_panel(&visible, timer_reset);
                    let _ = send_card_archived(
                        &fc,
                        &agent.state_dir(),
                        &chat_type,
                        &chat_id,
                        command_anchor(&msg),
                        "schedules_panel",
                        &card,
                    );
                    continue;
                }
                // 裸 `/remind`(不带参数)→ 起点卡,点选着建。只读渲染,talk 即可;真正的写在
                // 卡片提交那步过 operate-gate。带参数的老语法继续走下面的 parse_remind。
                if summon_core::ipc::bare_remind_command(&msg.text, &msg.mentions) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let card = summon_core::feishu::card_schedule_start("", None);
                    let _ = send_card_archived(
                        &fc,
                        &agent.state_dir(),
                        &chat_type,
                        &chat_id,
                        command_anchor(&msg),
                        "schedule_start",
                        &card,
                    );
                    continue;
                }
                // `/remind <when…> <prompt>`:老文本语法(operate 档,改本机持久状态)→ operate-gate
                // → 建 schedule → 回确认。
                match summon_core::ipc::parse_remind(&msg.text, &msg.mentions, chrono::Local::now())
                {
                    Ok(Some(r)) => {
                        inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                        let op_union = operator_union_id(&msg.sender_union_id, || {
                            fc.resolve_union_id(&msg.sender)
                        });
                        if !agent.can_operate(
                            &msg.sender,
                            &op_union,
                            is_group,
                            OperateCommand::Remind.scope(&chat_id),
                        ) {
                            log(&summon_i18n::format!("r74bf636dfa", agent.name, msg.sender));
                            let _ = send_markdown_archived_anchored(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                command_anchor(&msg),
                                summon_i18n::text!("ra43962187e"),
                            );
                            continue;
                        }
                        let st = crate::schedule::schedule_store::ScheduleStore::at_summon_home();
                        match st.next_id() {
                            Ok(id) => {
                                let s = summon_core::Schedule {
                                    id,
                                    kind: r.kind,
                                    spec: r.spec,
                                    agent: agent.name.clone(),
                                    chat_id: chat_id.clone(),
                                    chat_type: chat_type.clone(),
                                    prompt: r.prompt,
                                    created_by: msg.sender.clone(),
                                    created_at: chrono::Local::now()
                                        .format("%Y-%m-%d %H:%M:%S")
                                        .to_string(),
                                    output: summon_core::ScheduleOutput::Progress,
                                    context: summon_core::ScheduleContext::Resume,
                                    enabled: true,
                                    last_fired: None,
                                    revision: 0,
                                };
                                let reply = match st.add(&s) {
                                    Ok(()) => summon_core::feishu::schedule_confirm_markdown(&s),
                                    Err(e) => summon_i18n::format!("r3535b88fe0"),
                                };
                                let _ = send_markdown_archived_anchored(
                                    &fc,
                                    &agent.state_dir(),
                                    &chat_type,
                                    &chat_id,
                                    command_anchor(&msg),
                                    &reply,
                                );
                            }
                            Err(e) => {
                                let _ = send_markdown_archived_anchored(
                                    &fc,
                                    &agent.state_dir(),
                                    &chat_type,
                                    &chat_id,
                                    command_anchor(&msg),
                                    &summon_i18n::format!("rcea3d84fc7"),
                                );
                            }
                        }
                        continue;
                    }
                    Err(e) => {
                        inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                        // /remind 语法错(明确是 /remind 开头但解析失败)→ 友好回执,不喂 agent。
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            &format!("⏰ {e}"),
                        );
                        continue;
                    }
                    Ok(None) => {} // 不是 /remind,继续常规处理
                }
                // `/introduce @A @B [分工说明]`:让本群的 bot 与同事互相认识,把被 @ 的成员连同身份
                // 存为 peers、热更新 tmux 会话、回执确认。operate 档,不喂 agent。命令词后面的说明文字
                // (「A 开发、B 审核」)一并存进 peers 文件并注入双方——那是用户发这条命令的主要意图。
                if let Some(note) = summon_core::ipc::introduce_command(&msg.text, &msg.mentions) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    if !agent.can_operate(
                        &msg.sender,
                        &op_union,
                        is_group,
                        OperateCommand::Introduce.scope(&chat_id),
                    ) {
                        log(&summon_i18n::format!("r392382f227", agent.name, msg.sender));
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            summon_i18n::text!("r2d967cbff4"),
                        );
                        continue;
                    }
                    let extracted = peers_from_mentions(
                        &msg.mentions,
                        &msg.sender,
                        bot_open_id.as_deref().unwrap_or(""),
                    );
                    if extracted.is_empty() {
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            summon_i18n::text!("r72b5cd6083"),
                        );
                        continue;
                    }
                    // 存 peers(per-chat),连同这次交代的分工说明
                    let tag = chat_tag(&chat_type, &chat_id);
                    // 回执与热更新按合并后的名单说身份:这次 @ 事件没给身份时,盘上已确认的那个才是事实。
                    let stored =
                        peers_store.add_peers(&tag, extracted.clone(), Some(note.as_str()));
                    let (extracted, stored) = match stored {
                        Ok(pf) => {
                            log(&summon_i18n::format!(
                                "rfae089a549",
                                agent.name,
                                extracted.len(),
                                pf.peers.len()
                            ));
                            (with_recorded_kinds(extracted, &pf.peers), Ok(()))
                        }
                        Err(e) => {
                            log(&summon_i18n::format!("rad820ce26f", agent.name));
                            (extracted, Err(e))
                        }
                    };
                    // 热更新只排入 chat 线程；由它在两轮之间用 send_line_for_driver 串行注入。
                    // 无头/尚无 handle 的会话跳过，下次冷启从 peers 文件进 PRIMER。
                    if matches!(agent.capture_mode(), CaptureMode::Transcript) {
                        if let Some(handle) = chats.get(&chat_id) {
                            let notice = summon_core::peers::compose_peers_hot_update(
                                &extracted,
                                Some(note.as_str()),
                            );
                            if handle.tx.send(ThreadMsg::PeersNotice { notice }).is_err() {
                                log(&summon_i18n::format!("r534af52d59", agent.name));
                            }
                        }
                    }
                    // 确认类回执绝不 @ peer：否则回执本身就会触发另一 bot，成为 ping-pong 起点。
                    // 显示名仍走普通外部 markdown 转义，不能把缓存 name 当受信标签。
                    let reply = peers_ack(
                        summon_i18n::format!(
                            "rcf6cbac24f",
                            summon_core::peers::describe_by_kind(&extracted)
                        ),
                        stored,
                    );
                    if let Err(e) = send_command_ack(
                        &fc,
                        &agent,
                        &chat_type,
                        &chat_id,
                        command_anchor(&msg),
                        &reply,
                    ) {
                        log(&summon_i18n::format!("r37fd912045", agent.name));
                    }
                    continue;
                }
                // `/unintroduce @A @B` / `/unintroduce --all`:解除本群的相互认识(bot 或同事)。
                // 与 `/introduce` 严格对称 —— 群消息投给**每个被 @ 的 bot**,各自从自己的 peers
                // 里移除「被 @ 的其余人」。A 移除 B/C、B 移除 A/C、C 移除 A/B,**两两解除是这个
                // 机制的自然结果**,不需要谁做全局编排。没被 @ 的旧相识原样保留。
                if let Some(scope) = summon_core::ipc::unintroduce_command(&msg.text, &msg.mentions)
                {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    let op_union = operator_union_id(&msg.sender_union_id, || {
                        fc.resolve_union_id(&msg.sender)
                    });
                    // 鉴权门槛与 `/introduce` 相同(operate 档)。解除是破坏性动作,门槛不该比建立低。
                    if !agent.can_operate(
                        &msg.sender,
                        &op_union,
                        is_group,
                        OperateCommand::Unintroduce.scope(&chat_id),
                    ) {
                        log(&summon_i18n::format!("r9cf0296956", agent.name, msg.sender));
                        let _ = send_markdown_archived_anchored(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            command_anchor(&msg),
                            summon_i18n::text!("rfd9c60b529"),
                        );
                        continue;
                    }
                    let tag = chat_tag(&chat_type, &chat_id);
                    let (notice, reply) = match scope {
                        summon_core::ipc::UnintroduceScope::All => {
                            let removed = peers_store.remove(&tag);
                            match &removed {
                                Ok(true) => log(&summon_i18n::format!("rb0cc6d7d05", agent.name)),
                                Ok(false) => log(&summon_i18n::format!("r0461408a71", agent.name)),
                                Err(e) => log(&summon_i18n::format!("rd388497d44", agent.name)),
                            }
                            (
                                summon_core::peers::compose_peers_cleared_notice(),
                                peers_ack(
                                    summon_i18n::text!("r026be81e76").to_string(),
                                    removed.map(|_| ()),
                                ),
                            )
                        }
                        summon_core::ipc::UnintroduceScope::Mentioned => {
                            // 与 `/introduce` 用同一套提取:排除发命令的人与本 bot 自己。
                            // 于是 `/unintroduce @A` 只 @ 了一个 bot 时 extracted 为空 ——
                            // 那种写法本就无意义(只有 A 收得到,而它要移除的正是自己),给用法提示。
                            let targets = peers_from_mentions(
                                &msg.mentions,
                                &msg.sender,
                                bot_open_id.as_deref().unwrap_or(""),
                            );
                            if targets.is_empty() {
                                let _ = send_markdown_archived_anchored(
                                    &fc,
                                    &agent.state_dir(),
                                    &chat_type,
                                    &chat_id,
                                    command_anchor(&msg),
                                    summon_i18n::text!("r8f7ee1f738"),
                                );
                                continue;
                            }
                            let ids: Vec<String> =
                                targets.iter().map(|p| p.open_id.clone()).collect();
                            let targets = match peers_store.load(&tag) {
                                Some(pf) => with_recorded_kinds(targets, &pf.peers),
                                None => targets,
                            };
                            let removed = peers_store.remove_peers(&tag, &ids);
                            match &removed {
                                Ok((removed, left)) => {
                                    log(&summon_i18n::format!("ra3fcfec0a6", agent.name))
                                }
                                Err(e) => log(&summon_i18n::format!("rfa9fe11371", agent.name)),
                            }
                            (
                                summon_core::peers::compose_peers_removal_notice(&targets),
                                peers_ack(
                                    summon_i18n::format!(
                                        "r0fbcade90b",
                                        summon_core::peers::describe_by_kind(&targets)
                                    ),
                                    removed.map(|_| ()),
                                ),
                            )
                        }
                    };
                    // 热更新:peers 只在冷启进 PRIMER,跑着的会话上下文里还留着「你认识 X」。
                    // 不注入的话 agent 会继续按旧认知去 @ 一个已经不认识它的 bot —— 消息送达却被
                    // 对方 talk 闸拒,日志只留一行「拒绝非白名单」,两边都看不出发生了什么。
                    if matches!(agent.capture_mode(), CaptureMode::Transcript) && !notice.is_empty()
                    {
                        if let Some(handle) = chats.get(&chat_id) {
                            if handle.tx.send(ThreadMsg::PeersNotice { notice }).is_err() {
                                log(&summon_i18n::format!("r434e5feafe", agent.name));
                            }
                        }
                    }
                    // 回执**绝不 @ 被解除的 bot**:与 `/introduce` 同一条理由 —— 带 @ 会触发对方,
                    // 而它此刻已经不认识我们了,那条消息只会撞在它的 talk 闸上。
                    if let Err(e) = send_command_ack(
                        &fc,
                        &agent,
                        &chat_type,
                        &chat_id,
                        command_anchor(&msg),
                        &reply,
                    ) {
                        log(&summon_i18n::format!("r11c247be72", agent.name));
                    }
                    continue;
                }
                // `/summon <agent>` 是 Claude 侧的一个 skill,不是飞书侧用户命令(summon 2026-08-08
                // 起冷启也不再注入它,见 `driver::default_seed`)。用户误发会被 `slash_command_text`
                // 透传给 CLI,CLI 把它当 skill 执行 → 不输出 end_turn → `run_turn` 傻等 STALL_WINDOW
                // (240s)超时,期间新消息全部排队不处理。这里拦截并给友好提示,绝不透传。
                if summon_core::ipc::is_summon_command(&msg.text, &msg.mentions) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    log(&summon_i18n::format!("r6410aa7191", agent.name, msg.sender));
                    let _ = send_markdown_archived_anchored(
                        &fc,
                        &agent.state_dir(),
                        &chat_type,
                        &chat_id,
                        command_anchor(&msg),
                        summon_i18n::text!("r2a36a43c6f"),
                    );
                    continue;
                }
                // 兜底:首 token 是 summon 自有命令,却没被上面任何专门分支消费 —— 说明命令形态
                // 没被它自己的解析器认出(参数写错等)。这种既不能透传 CLI(当 skill 执行会报错、
                // 不输出 end_turn,`run_turn` 傻等满 stall,期间该 chat 的后续消息全部排队),
                // 也不该静默当普通消息喂给 agent(bot 会对着 `/session 看看` 用自然语言瞎回)。
                // 回一句用法提示,让用户知道命令没被识别。
                if let Some(cmd) = summon_core::ipc::summon_owned_command(
                    &msg.text,
                    &msg.mentions,
                    CliDriver::from_kind(agent.cli_kind()),
                ) {
                    inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
                    log(&summon_i18n::format!("rbdc6f2bed4", agent.name, msg.sender));
                    let _ = send_markdown_archived_anchored(
                        &fc,
                        &agent.state_dir(),
                        &chat_type,
                        &chat_id,
                        command_anchor(&msg),
                        &summon_i18n::format!("r9a321dfe11"),
                    );
                    continue;
                }
                // 入站 WAL:到这里归 summon 的命令类(/session /model /fast /cd /schedules /remind
                // /introduce /summon 已各自 continue;Claude 上的 /fast 归 provider,不在此列)都已分流,
                // 剩下的都会进入 chat worker(普通消息或 provider slash 候选；后者可能被安全策略拒绝)。
                // 在进入易失通道前把整条 Inbound
                // 落盘。append 失败时不记 seen、不向 chat worker 交付；网络重投或用户重发仍可重试。
                if let Err(error) =
                    after_durable_inbox(&inbox_wal, &agent.name, &storage_scope.key, &msg, || {
                        inbound_gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms)
                    })
                {
                    log(&summon_i18n::format!(
                        "rfe5cca987a",
                        agent.name,
                        msg.message_id
                    ));
                    if let Err(send_error) = send_markdown_archived(
                        &fc,
                        &agent.state_dir(),
                        msg.chat_type.as_deref().unwrap_or("p2p"),
                        &msg.chat_id,
                        summon_i18n::text!("rd9d5bba014"),
                    ) {
                        log(&summon_i18n::format!("r3c4b57b557", agent.name));
                    }
                    continue;
                }
                // 斜杠分类只保留 provider 命令的原始语法；真正执行策略在 chat worker 收口为
                // operate-gate + Transcript + driver 声明的 exact `/compact`。`/clear`、未知命令、
                // 无权限或 headless/RPC 都会诚实拒绝，不得因此占用本地会话。
                // 否则走常规:
                //   ① 位置感知剥离:只有开头的 @本bot 才剥掉(视为「对本 bot 说话」),剥完正文的首
                //     字符就是用户真正写的第一个字;非开头的 @本bot 保留为信息(agent 据此判断
                //     意图——可能只是被 CC,不需要响应)。
                //   ② 把其余 @ 占位渲染成 @名字(不带 open_id;bot 回 @ 由出站层查 peers 解析)。
                let slash_addressed = command_is_addressed(
                    is_group,
                    bot_open_id.as_deref(),
                    &msg.text,
                    &msg.mentions,
                    mentionless_admitted,
                );
                let (conversation, leading_self_mention) =
                    conversation_text(&msg.text, &msg.mentions, bot_open_id.as_deref());
                let (text, raw, addressed) = task_text_and_addressing(
                    summon_core::ipc::slash_command_text(
                        &msg.text,
                        &msg.mentions,
                        CliDriver::from_kind(agent.cli_kind()),
                    ),
                    &conversation,
                    slash_addressed,
                    leading_self_mention,
                    mentionless_admitted,
                );
                let kind = chat_task_kind(
                    raw,
                    CliDriver::from_kind(agent.cli_kind()),
                    agent.capture_mode(),
                    &text,
                    &effective.slash_blacklist,
                );
                let (text, kind, queued) = rewrite_queued(text, kind, raw, &conversation);
                let slash_admitted = if kind.is_slash() && addressed {
                    // dispatcher 扇出线程不做联系人 HTTP。事件自带 union_id/open_id 足够时冻结
                    // fresh 权限事实；缺 union_id 且名单只配 union_id 时保守拒绝，由用户重试，不能
                    // 为一个 slash 把该 agent 的所有 chat 路由阻塞到联系人 API 超时。
                    slash_claims_local_session(
                        &text,
                        agent.can_operate(
                            &msg.sender,
                            &msg.sender_union_id,
                            is_group,
                            OperateCommand::ProviderSlash.scope(&msg.chat_id),
                        ),
                        agent.capture_mode(),
                        CliDriver::from_kind(agent.cli_kind()),
                        &effective.slash_blacklist,
                    )
                } else {
                    false
                };
                let (text, kind, fresh_requested) =
                    rewrite_new_session(text, kind, slash_admitted, &conversation);
                let (text, kind, handoff) = rewrite_handoff(
                    text,
                    kind,
                    slash_admitted,
                    &msg.text,
                    &msg.mentions,
                    bot_open_id.as_deref(),
                    &agent,
                );
                let claims_local_session =
                    task_claims_local_session(kind, is_group, addressed, slash_admitted);
                // 普通对话会启动/续用本地会话；受控 compact 在排队到真正执行之间也必须先占位，
                // 防止 Bridge 于空窗内接管。同一 handle 既已 claim 时，后续 rejected slash 不回退。
                handle.local_claimed =
                    retain_local_session_claim(handle.local_claimed, claims_local_session);
                // 身份缓存:把这条消息里所有 mention 的 {open_id, name} 记下来,
                // 供出站 @mention 在 peers store 找不到时回退查找。best-effort,失败忽略。
                // **发信人也要进表。** 原来只记 `msg.mentions`(这条消息里被 @ 到的人),
                // 于是出现一个恰好相反的结果:owner 是那个一直在 @ 别人、自己几乎不被 @ 的人,
                // 他的 {open_id, name} 从来没进过缓存,`render_outbound_mentions` 查不到 →
                // agent 写的 `@张三` 原样发出 → 飞书按纯文本渲染,**永远 @ 不到跟它说话的人**
                // (owner 2026-08-15 报障)。
                //
                // 记的是 `who` —— agent 在 prompt 里看到的**正是这个名字**,它要 @ 谁就照着打。
                // 兜底占位(`用户(ou_...)` / 裸 open_id)不记:那不是名字,记进去只会让缓存里
                // 多一条永远匹配不上的垃圾,还会挡住之后真名解析出来的那一条。
                let identities = identities_to_cache(
                    &msg.sender,
                    &who,
                    &fallback_display_name(&msg.sender, sender_is_bot),
                    &msg.mentions
                        .iter()
                        .map(|m| (m.open_id.clone(), m.name.clone()))
                        .collect::<Vec<_>>(),
                );
                if !identities.is_empty() {
                    // 同一批 mention 立刻进内存名字表:本条消息里 @ 到的人,下一条自己发言时就有真名
                    // (这是 `/introduce` 之后对方第一条消息就能被认出来的原因)。
                    fc.prime_names(identities.iter().cloned());
                    if let Err(e) =
                        identity_cache.record_identities(&identities, now_unix_ms() as i64)
                    {
                        log(&summon_i18n::format!("ra61317464a", agent.name));
                    }
                }
                // 多 bot 共 @ 提示:收集本条消息里 @ 到的其他 bot 名字(不含自己),
                // 供 handle_task 在 prompt 末尾追加一条「同时 @ 了谁」的提示,
                // 帮 agent 判断自己是不是主要收件人。best-effort,只看名字不看 open_id。
                let co_mentioned: Vec<String> = msg
                    .mentions
                    .iter()
                    .filter(|m| match bot_open_id.as_deref() {
                        Some(me) => m.open_id != me,
                        None => true,
                    })
                    .map(|m| m.name.clone())
                    .filter(|n| !n.is_empty())
                    .collect();
                let task = ChatTask {
                    // 普通入站消息不是定时任务。
                    schedule_notice: None,
                    fresh_context: fresh_requested,
                    fresh_requested,
                    queued,
                    // 写交接那一轮的正文是 summon 构造的指令,不进卡片底注。
                    hide_context_hint: handoff.is_some(),
                    handoff,
                    sender_is_bot,
                    text,
                    message_id: msg.message_id,
                    anchor_message_id: msg.anchor_message_id,
                    who,
                    who_id: msg.sender.clone(),
                    // 事件自带的 union_id 与 open_id 同源可信;为空时由 chat 线程按需回退联系人 API
                    // (放在那边是为了不让 HTTP 阻塞 dispatcher 的扇出路径,同群名解析的理由)。
                    who_union_id: msg.sender_union_id.clone(),
                    sender_is_peer,
                    is_group,
                    kind,
                    slash_admitted,
                    addressed,
                    parent_id: msg.parent_id,
                    thread_id: msg.thread_id,
                    root_id: msg.root_id,
                    resources: msg.resources,
                    co_mentioned,
                    reattach: msg.reattach,
                    messaging: effective.clone(),
                };
                let mid = task.message_id.clone();
                let cumulative_local_claimed = handle.local_claimed;
                let current_tx = handle.tx.clone();
                let delivery =
                    deliver_task_with_one_replacement(&current_tx, Box::new(task), || {
                        // SendError 证明旧 receiver 已销毁。若旧 worker 在退出前发过 ChatExited，
                        // 该事件在 receiver drop 前已完成 enqueue；先扫出并终态化它，才能保证
                        // 它不会在 replacement 插入后又按 chat_id 把新 handle 摘掉。
                        chats.remove(&chat_id);
                        for exited in
                            drain_disconnected_chat_exits(&rx, &mut deferred_dispatches, &chat_id)
                        {
                            settle_chat_exit(
                                &mut chats,
                                &mut peer_trigger_limiter,
                                &peers_store,
                                &outbox,
                                &effective,
                                &agent,
                                &storage_scope,
                                exited,
                            );
                        }

                        // Bridge 可由本机 socket 与 dispatcher 并发推进；replacement cold-start
                        // 前必须再验一次 slot，不能复用本轮较早时刻的 admission 结论。
                        if is_group {
                            let target = summon_core::bridge::ScopedChatKey::new(
                                &storage_scope.key,
                                &agent.name,
                                &chat_id,
                            );
                            match bridge_slot_message(&bridge_store, &target) {
                                Ok(None) => {}
                                Ok(Some(_)) => {
                                    log(&summon_i18n::format!("r97c0e50786", agent.name));
                                    return None;
                                }
                                Err(error) => {
                                    log(&summon_i18n::format!("r1a927dfc3f", agent.name));
                                    return None;
                                }
                            }
                            if crate::remote::inbound::remote_bound(&agent, &chat_id) {
                                log(&summon_i18n::format!("r1ce96695ec", agent.name));
                                return None;
                            }
                        }

                        log(&summon_i18n::format!("rb620296d40", agent.name));
                        let mut replacement = spawn_chat_thread(
                            agent.clone(),
                            storage_scope.key.clone(),
                            tmux_name.clone(),
                            poll,
                            debounce,
                            fc.clone(),
                            epoch_identity.clone(),
                            chat_id.clone(),
                            chat_type.clone(),
                            self_tx.clone(),
                            core.clone(),
                            inflight.clone(),
                        );
                        replacement.local_claimed = cumulative_local_claimed;
                        Some(replacement)
                    });
                match delivery {
                    TaskDelivery::Current => {}
                    TaskDelivery::Replacement(mut replacement) => {
                        // replacement 接住的是**这条**入站任务:活动收口(时钟/空闲卡去重含
                        // durable marker/来源代际)必须补在新 handle 上 —— 旧 handle 上那次
                        // 已随摘除作废,而 spawn 从账本恢复出的 idle_prompted 可能是 true、
                        // 来源代际是空串(终审阻塞③:漏这半边,会话收到真人任务后仍顶着
                        // 「已提醒」,下段空闲期既不发卡;admission 围栏也失去参照)。
                        replacement.note_activity(&agent, &chat_id, Some(&mid));
                        chats.insert(chat_id.clone(), replacement);
                        log(&summon_i18n::format!("r49448261c8", agent.name));
                    }
                    TaskDelivery::RetainedPending => {
                        // 两次 receiver 都不可用或 Bridge gate 拒绝重建：WAL 仍是唯一真相。
                        // 绝不能 mark_consumed；Recovery replay 也必须保持 Claimed，待冷启修复。
                        log(&summon_i18n::format!("r6c75613119", agent.name));
                    }
                }
            }
            Ok(DispatchMsg::Card(card)) => {
                use summon_core::feishu::CardAction;
                // 会话控制面板 / idle 卡片:value.action ∈ keepalive/compact/clean/reset/model/set_workdir…
                let action = card
                    .value
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // C12:先把 action 归一成 CardAction 枚举,鉴权(gate)与下面的分派 match 都对同一
                // 枚举**穷举、无 catch-all**——新增一种卡片动作时编译器同时逼出「鉴权档位」和「分派
                // 臂」,漏一处不编译;未登记串归 Unknown → gate fail-closed(operate)、分派走无害兜底
                // (KeepAlive)。这才是「从结构上焊死」的真身(旧实现靠 `_ => None` 通配,会静默
                // fail-open)。gate_* 归 Family,在 Core 层按 Intent::gate_tier 鉴权。
                let card_action = CardAction::from_action(&action);
                // Bridge callback 必须先按 ticket 精确校验 requester/scope/card，再使用 ticket 中
                // 由可信入站事件保存的 union_id 做 operate-gate；在这里提前查联系人 API 会让
                // union-only operator 在 API 短暂失败时被误拒。专用 handler 仍强制群语义。
                if card_action.gate() == summon_core::feishu::CardGate::Operate
                    && !checks_operate_in_handler(card_action)
                {
                    let op_union = fc.resolve_union_id(&card.operator_open_id);
                    let is_group = card_chat_is_group(&chats, &agent, &card.chat_id);
                    if !agent.can_operate(
                        &card.operator_open_id,
                        &op_union,
                        is_group,
                        card_action.operate_scope(&card.chat_id),
                    ) {
                        log(&summon_i18n::format!(
                            "r5c94b62b9b",
                            agent.name,
                            card.chat_id,
                            card.operator_open_id
                        ));
                        let chat_type = card_chat_type(&chats, &card.chat_id);
                        let _ = send_markdown_archived(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &card.chat_id,
                            summon_i18n::text!("r2e1b5d0084"),
                        );
                        ack_card_callback(&card, &card_actions);
                        continue;
                    }
                }
                let recovery_ack = match card_action {
                    CardAction::Recover => Some(handle_card_recover(
                        &chats,
                        &inbox_wal,
                        &self_tx,
                        &agent,
                        &fc,
                        &storage_scope,
                        &bridge_store,
                        &card,
                    )),
                    CardAction::DismissRecovery => Some(handle_card_dismiss_recovery(
                        &chats,
                        &inbox_wal,
                        &agent,
                        &fc,
                        &storage_scope,
                        &bridge_store,
                        &card,
                    )),
                    _ => None,
                };
                if let Some(ack) = recovery_ack {
                    if ack {
                        ack_card_callback(&card, &card_actions);
                    } else {
                        log(&summon_i18n::format!(
                            "rfe8bba2e3e",
                            agent.name,
                            card.event_id
                        ));
                    }
                    continue;
                }
                match card_action {
                    CardAction::Fork => {
                        let result = super::fork_entry::callback(
                            &super::fork_entry::Environment {
                                agent: &agent,
                                fc: &fc,
                                chats: &chats,
                                scope: &storage_scope.key,
                            },
                            &card,
                        );
                        if let Err(error) = result {
                            let kind = card_chat_type(&chats, &card.chat_id);
                            let _ = send_markdown_archived(
                                &fc,
                                &agent.state_dir(),
                                &kind,
                                &card.chat_id,
                                &summon_i18n::format!(
                                    "r05c640407a",
                                    summon_core::feishu::escape_lark_md(&error.to_string())
                                ),
                            );
                        }
                    }
                    CardAction::BridgeFamily => {
                        let disposition = handle_bridge_card_action_with_stores(
                            &agent,
                            &fc,
                            &storage_scope,
                            &chats,
                            &inbox_wal,
                            &inflight,
                            &card,
                            &bridge_store,
                            &outbox,
                        );
                        finish_durable_card_callback(&card, &card_actions, disposition);
                        continue;
                    }
                    // sentinel 三方建群确认卡:operate 档已在上面集中鉴权;handler 内再按盘上
                    // proposal 存根做 fencing 复验(Pending 状态 + 卡片 message_id + owner 本人 +
                    // 30min TTL),幂等、fail-closed。非 durable:点击丢失 = 卡还在,owner 再点。
                    CardAction::SentinelFamily => {
                        sentinel::actions::handle_card(&agent, &fc, &inbox_wal, &self_tx, &card);
                    }
                    CardAction::DocumentComment => {
                        if let Err(error) = document_comments::handle_card(
                            &agent,
                            &fc,
                            &storage_scope.key,
                            &epoch_identity,
                            &card,
                        ) {
                            log(&summon_i18n::format!("rffcba46895"));
                        }
                        finish_durable_card_callback(
                            &card,
                            &card_actions,
                            CallbackDisposition::Terminal,
                        );
                        continue;
                    }
                    // 托管 RPC 迁移有 durable callback：进程崩在 durable owner move 前后都由 WAL
                    // 重放，source revision + target 占用闸让重放幂等 fail-closed。handler 返回即
                    // terminal；进程中途崩溃则走不到 ACK，下一代继续。
                    CardAction::RecallRpcHandoff => {
                        handle_bridge_recall_card_action(
                            &agent,
                            &fc,
                            &storage_scope,
                            &mut chats,
                            &bridge_store,
                            &card,
                            LocalChatStores::new(&inbox_wal, &inflight, &outbox),
                        );
                        finish_durable_card_callback(
                            &card,
                            &card_actions,
                            CallbackDisposition::Terminal,
                        );
                        continue;
                    }
                    // 外部召回、翻页与遥控沿用无状态 callback；RPC handoff 在上面的 durable 专用臂。
                    CardAction::RecallResume
                    | CardAction::RecallPage
                    | CardAction::RecallCmuxRemote => {
                        handle_bridge_recall_card_action(
                            &agent,
                            &fc,
                            &storage_scope,
                            &mut chats,
                            &bridge_store,
                            &card,
                            LocalChatStores::new(&inbox_wal, &inflight, &outbox),
                        );
                    }
                    // 闸门卡片动作:chat 提问卡的提交/跳过/取消 → 经 Core(operate-gate)resolve 闸门。
                    CardAction::GateFamily => {
                        handle_gate_card_action(&core, &agent, &fc, &chats, &card, &action)
                    }
                    // 交互菜单中介(Q1):用户在飞书点选项/填文本/取消 → 注入对应按键到该 chat 的
                    // tmux pane(claude 的菜单在那等输入)。operate 档,已在上面集中鉴权。
                    // retry_now 同属按键注入(「继续」+ Esc),只是触发点在 live 卡而不是菜单卡。
                    CardAction::TuiChoice
                    | CardAction::TuiText
                    | CardAction::TuiEsc
                    | CardAction::RetryNow => {
                        handle_tui_inject(&chats, &agent, &fc, &card, &action)
                    }
                    // 配置变更类:写 agent.yaml 热切换/快重置,operate 档已在上面集中鉴权。
                    CardAction::Model => {
                        handle_card_set_model(&mut chats, &agent, &fc, &card, false)
                    }
                    CardAction::ModelDefault => {
                        handle_card_set_model(&mut chats, &agent, &fc, &card, true)
                    }
                    CardAction::Effort => {
                        handle_card_set_effort(&mut chats, &agent, &fc, &card, false)
                    }
                    CardAction::EffortDefault => {
                        handle_card_set_effort(&mut chats, &agent, &fc, &card, true)
                    }
                    CardAction::CursorConfigDefault => {
                        cards::handle_card_cursor_config_default(&mut chats, &agent, &fc, &card)
                    }
                    CardAction::CodexServiceTier => {
                        cards::handle_card_codex_service_tier(&mut chats, &agent, &fc, &card)
                    }
                    CardAction::CursorMode => {
                        cards::handle_card_cursor_mode(&mut chats, &agent, &fc, &card)
                    }
                    CardAction::CursorConfig => {
                        cards::handle_card_cursor_config(&mut chats, &agent, &fc, &card)
                    }
                    CardAction::QuotaBadge => {
                        cards::handle_card_quota_badge(&mut chats, &agent, &fc, &card)
                    }
                    action @ (CardAction::BadgeCli
                    | CardAction::BadgeModel
                    | CardAction::BadgeEffort
                    | CardAction::BadgeCtx) => {
                        // 四个变体都有对应开关,None 走不到;真走到了也只是不处理这次点击。
                        if let Some(item) = action.badge_setting() {
                            cards::handle_card_badge_item(&mut chats, &agent, &fc, &card, item);
                        }
                    }
                    CardAction::SessionCot => {
                        cards::handle_card_session_cot(&mut chats, &agent, &fc, &card)
                    }
                    CardAction::SessionRefresh => {
                        cards::handle_card_session_refresh(&mut chats, &agent, &fc, &card)
                    }
                    CardAction::SessionClose => {
                        cards::handle_card_session_close(&chats, &agent, &fc, &card)
                    }
                    CardAction::SetWorkdir => {
                        handle_card_set_workdir(&mut chats, &agent, &fc, &card)
                    }
                    // 群面板「私信候选目录」→ 主动私聊操作者选择器卡(路径不进群)。operate 档已集中鉴权。
                    CardAction::PickWorkdir => handle_card_pick_workdir(&chats, &agent, &fc, &card),
                    // 私信候选按钮 → 写目标 chat 的 .cwd。集中闸豁免,handler 按目标 chat 判,是唯一一道 operate 闸。
                    CardAction::SetWorkdirTo => {
                        handle_card_set_workdir_to(&chats, &agent, &fc, &card)
                    }
                    // 面板 / 回执 / 降级菜单卡上的「>_ 打开终端」:operate 档已集中鉴权(按卡所在 chat)。
                    CardAction::TerminalOpen => {
                        super::terminal::handle_card_terminal_open(&chats, &agent, &fc, &card)
                    }
                    // 私聊终端卡上的「🔄 换一组新链接」:集中闸已豁免,handler 内按目标 chat 重验。
                    CardAction::TerminalRegenerate => {
                        super::terminal::handle_card_terminal_regenerate(&chats, &agent, &fc, &card)
                    }
                    CardAction::TerminalAuth => {
                        super::terminal::handle_card_terminal_auth(&chats, &agent, &fc, &card)
                    }
                    CardAction::DelegateDetails => {
                        super::delegate::handle_details(&agent, &fc, &card);
                    }
                    CardAction::PreviewOpen => {
                        handle_card_preview_open(&chats, &agent, &fc, &core, &peers_store, &card)
                    }
                    // 崩溃恢复卡片:恢复=重注入(免鉴权:带原 sender 再过白名单,不绕门控);
                    // 全部忽略=销毁待办,operate 档已在上面集中鉴权(T4)。
                    CardAction::Recover | CardAction::DismissRecovery => {
                        unreachable!("{}", summon_i18n::format!("r72792489b1"))
                    }
                    // 定时器取消:停用该 id → 原地刷新本卡。operate 档已在上面集中鉴权。
                    CardAction::ScheduleCancel => {
                        handle_schedule_cancel(&agent, &fc, &chats, &card)
                    }
                    // schedules 面板「定时归档」开关:翻转本 chat 的 per-chat 重置覆盖并原地刷新。
                    // operate 档已在上面集中鉴权。
                    CardAction::ScheduleTimerResetToggle => {
                        handle_schedule_timer_reset_toggle(&agent, &fc, &chats, &card)
                    }
                    // 定时器建 / 改的卡片流。前三个纯渲染(不碰盘,或只读本会话名下的条目),
                    // 后两个落盘、operate 档已在上面集中鉴权;归属校验一律在各 handler 内再过一次。
                    CardAction::ScheduleNew => handle_schedule_new(&agent, &fc, &chats, &card),
                    CardAction::SchedulePick => handle_schedule_pick(&agent, &fc, &chats, &card),
                    CardAction::ScheduleEdit => handle_schedule_edit(&agent, &fc, &chats, &card),
                    CardAction::ScheduleCreate => {
                        handle_schedule_create(&agent, &fc, &chats, &card)
                    }
                    CardAction::ScheduleSave => handle_schedule_save(&agent, &fc, &chats, &card),
                    // 控制卡「⏸ 打断」→ 投 ThreadMsg::Interrupt 给该 chat 线程(turn 进行中即纯停)。
                    // operate 档已在上面集中鉴权。chat 不在线(本轮已结束)→ 回执,不报错。
                    // **fencing 校验先于路由**。卡片是托管在飞书、summon 无法保证能刷新的副本;
                    // 权限只能在动作点校验,不能靠「按钮还在屏幕上」。此前只按 chat_id 路由,于是
                    // daemon 重启后残留的旧卡(没人去定格它)点一下就能打断一个**毫不相干的新轮**。
                    //
                    // 令牌粒度是 **turn 而非 daemon epoch**:同一 daemon 内上一轮结束、新一轮开始时
                    // epoch 相同,只有 turn id 能 fence 住轮次边界。`load_by_tag` 同时比对 token 与
                    // 新鲜度,缺 token 的老卡自然落到拒绝分支(fail-closed)。
                    CardAction::Interrupt if !interrupt_token_is_current(&agent, &chats, &card) => {
                        let chat_type = card_chat_type(&chats, &card.chat_id);
                        let _ = send_markdown_archived(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &card.chat_id,
                            summon_i18n::text!("rbff2f9ae04"),
                        );
                        log(&summon_i18n::format!(
                            "raf5c4c9184",
                            agent.name,
                            card.chat_id
                        ));
                    }
                    CardAction::Interrupt => match chats.get(&card.chat_id) {
                        Some(h) if h.tx.send(ThreadMsg::Interrupt).is_ok() => {
                            log(&summon_i18n::format!(
                                "rdfd63fe894",
                                agent.name,
                                card.chat_id
                            ));
                        }
                        _ => {
                            let chat_type = card_chat_type(&chats, &card.chat_id);
                            let _ = send_markdown_archived(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &card.chat_id,
                                summon_i18n::text!("r90da89c8f2"),
                            );
                        }
                    },
                    // 管家卡的刷新 / 下钻 / 返回总览:原地重渲同一张只读卡。
                    CardAction::FleetRefresh => {
                        // value 带 `agent` = 总览页上某个 agent 的下钻按钮(免打字);没带就是
                        // 刷新 / 返回总览。两者共用 `fleet_refresh`,都是只读重渲。
                        let target = card
                            .value
                            .get("agent")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|a| !a.is_empty());
                        // 远端目标另带 `remote`(机器名):两台机器可以有同名 agent,
                        // 只凭 alias 会下钻到另一台去。
                        let target_remote = card
                            .value
                            .get("remote")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|r| !r.is_empty());
                        steward::handle_card(&agent, &fc, &card.message_id, target, target_remote);
                    }
                    // 翻到某 agent 的会话操作页 / 在该页刷新:同样只是重渲一张只读卡。
                    CardAction::FleetSessions => {
                        match card.value.get("agent").and_then(|v| v.as_str()) {
                            Some(target) => steward::handle_sessions_card(
                                &agent,
                                &fc,
                                &card.message_id,
                                target,
                                None,
                            ),
                            // value 没带目标 = 卡片渲错了,退回总览而不是渲一页空白。
                            None => steward::handle_card(&agent, &fc, &card.message_id, None, None),
                        }
                    }
                    // 管家卡待处理区的「标记已处理」:operate 闸已在上面集中过完,steward 只删那一条
                    // 观测标记再原地重渲,不碰会话。
                    CardAction::FleetWaitingHandled => {
                        steward::handle_waiting_handled(&agent, &fc, &card.message_id, &card.value);
                    }
                    // 管家在**别的 agent** 的会话上按下退出。管家自己那道 operate 闸已在上面集中
                    // 鉴权过;这里把动作交给 Core,由它过**目标 agent** 那道闸再落到既有的
                    // `FleetSessionControl` 通道，目标端按归档或退出执行。
                    //
                    // **steward.rs 不参与执行**:那边只渲卡。会话层的事留在这条既有路径上,
                    // 管家「永不进会话层」的红线不动。
                    CardAction::FleetSessionControl => {
                        handle_fleet_session_control(&agent, &fc, &core, &card);
                    }
                    CardAction::ArchiveEnded => {
                        handle_archive_ended(&chats, &agent, &fc, &inflight, &card);
                    }
                    // 上下文提醒卡:Handoff / Ignore 的 operate 档已在上面集中鉴权,Compact 与面板
                    // compact 同档免鉴权。
                    CardAction::ContextCompact
                    | CardAction::ContextHandoff
                    | CardAction::ContextIgnore => {
                        if let Some(ctx_action) = cards::ContextWarnAction::of(card_action) {
                            cards::handle_context_warn_card(
                                &chats,
                                &agent,
                                &fc,
                                &card,
                                ctx_action,
                                &effective,
                                cards::ContextWarnClick {
                                    inbox: &inbox_wal,
                                    bridge: &bridge_store,
                                    delivery_scope: &storage_scope.key,
                                    self_tx: &self_tx,
                                    claimed: &mut context_warn_claimed,
                                },
                            );
                        }
                    }
                    // 生命周期类 + 未登记兜底:keepalive/compact 免鉴权;clean/reset operate 档已在
                    // 上面集中鉴权;Unknown 经 control_for_action → KeepAlive(无害保活,不误清理/重置)。
                    CardAction::Clean
                    | CardAction::Reset
                    | CardAction::Compact
                    | CardAction::KeepAlive
                    | CardAction::Unknown => route_lifecycle(
                        &mut chats,
                        &agent,
                        &fc,
                        &card,
                        control_for_action(&action),
                        &effective,
                        CardOrigin::from_value(&card.value),
                    ),
                }
                ack_card_callback(&card, &card_actions);
            }
            Ok(DispatchMsg::ChatExited {
                chat_id,
                chat_type,
                reason,
                creator,
                notify,
                session_created_at,
            }) => settle_chat_exit(
                &mut chats,
                &mut peer_trigger_limiter,
                &peers_store,
                &outbox,
                &effective,
                &agent,
                &storage_scope,
                ChatExitEvent {
                    chat_id,
                    chat_type,
                    reason,
                    creator,
                    notify,
                    session_created_at,
                },
            ),
            Ok(DispatchMsg::OrphanedTasks {
                chat_id,
                chat_type,
                tmux_name,
                tasks,
            }) => {
                // worker 先发 ChatExited 再发本消息(同 sender FIFO),此刻旧 handle 已被
                // settle_chat_exit 摘除 → get-or-spawn 新 worker 重投。Task 已过完整 admission
                // (WAL pending、去重闸已 seen),不重跑入站闸——重跑必被 DropDuplicate 吞掉。
                // send 失败不销账:WAL 仍是唯一真相,冷启兜底(与 RetainedPending 同语义)。
                let count = tasks.len();
                let delivered =
                    redeliver_orphaned_tasks(&mut chats, &agent, &chat_id, tasks, || {
                        if matches!(chat_type.as_str(), "group" | "topic_group")
                            && crate::remote::inbound::remote_bound(&agent, &chat_id)
                        {
                            log(&summon_i18n::format!("rf5555962a7", agent.name,));
                            return None;
                        }
                        log(&summon_i18n::format!("ree0a51c905", agent.name,));
                        Some(spawn_chat_thread(
                            agent.clone(),
                            storage_scope.key.clone(),
                            tmux_name.clone(),
                            poll,
                            debounce,
                            fc.clone(),
                            epoch_identity.clone(),
                            chat_id.clone(),
                            chat_type.clone(),
                            self_tx.clone(),
                            core.clone(),
                            inflight.clone(),
                        ))
                    });
                if let Some(redelivered) = delivered {
                    log(&summon_i18n::format!("rfd086700e4", agent.name,));
                }
            }
            Ok(DispatchMsg::ChatGone {
                chat_id,
                reason,
                operator_open_id,
            }) => {
                // 生命周期终态先于 Bridge state / 通用 teardown 分支落盘；BindingPaused 也会绕过
                // handle_chat_gone，不能让它绕过召回卡 target fence。exit mark 挡当前终态，独立
                // revision 让群重建后的旧私聊卡仍然失效。
                crate::session::exit_mark::mark(&agent, "group", &chat_id);
                if let Err(error) = invalidate_recall_target_lifecycle(&agent, &chat_id) {
                    inflight.freeze();
                    log(&summon_i18n::format!("re8da6cdf7b", agent.name));
                }
                match super::teardown::terminalize_chat_recovery(
                    &inbox_wal,
                    &outbox,
                    &storage_scope.key,
                    &agent.name,
                    &chat_id,
                ) {
                    Ok(0) => {}
                    Ok(n) => log(&summon_i18n::format!("r5a8dee9e2f", agent.name)),
                    Err(error) => log(&summon_i18n::format!("rc4aae246eb", agent.name)),
                }
                match handle_bridge_chat_gone_with_store(
                    &storage_scope,
                    &agent,
                    &chat_id,
                    reason,
                    &bridge_store,
                ) {
                    Ok(summon_core::bridge::BridgeChatGoneAction::None)
                    | Ok(summon_core::bridge::BridgeChatGoneAction::PendingCancelled { .. })
                    | Ok(summon_core::bridge::BridgeChatGoneAction::MirrorStopped(_)) => {
                        handle_chat_gone(
                            &mut chats,
                            &agent,
                            &fc,
                            &effective,
                            &outbox,
                            &peers_store,
                            &inflight,
                            poll,
                            debounce,
                            &chat_id,
                            reason,
                            &operator_open_id,
                        )
                    }
                    Ok(summon_core::bridge::BridgeChatGoneAction::BindingPaused(binding)) => {
                        log(&summon_i18n::format!("r57a51914c0", agent.name, binding.id));
                    }
                    Err(error) => log(&summon_i18n::format!("r519fae08bd", agent.name)),
                }
            }
            Ok(DispatchMsg::FleetSessionControl { chat_id, archive }) => {
                handle_fleet_chat_control(
                    &mut chats, &agent, &fc, &effective, &outbox, &inflight, poll, debounce,
                    &chat_id, archive,
                );
            }
            Ok(DispatchMsg::DashboardInterrupt {
                chat_id,
                turn_id,
                reply,
            }) => {
                // 控制台等不及已经放弃时发送失败,可忽略。
                let _ = reply.send(dashboard_interrupt(&agent, &chats, &chat_id, &turn_id));
            }
            Ok(DispatchMsg::TeardownChat { chat_id, reset }) => {
                handle_teardown_chat(
                    &mut chats, &agent, &fc, &effective, &outbox, &inflight, poll, debounce,
                    &chat_id, reset,
                );
            }
            // chat 轮内 agent 提问:发一张 form 卡(每题一行 + 提交/全部跳过)。
            // 卡片动作回来后 resolve 闸门,唤醒阻塞的 `summon ask`。
            Ok(DispatchMsg::ChatAskCard {
                gate_id,
                chat_id,
                chat_type,
                requester_open_id,
                questions,
            }) => {
                let card = summon_core::feishu::card_chat_ask(&summon_core::feishu::ChatAskCard {
                    gate_id: &gate_id,
                    requester_open_id: &requester_open_id,
                    questions: &questions,
                });
                let gate = summon_core::ids::GateId::new(gate_id);
                match send_card_archived_id(
                    &fc,
                    &agent.state_dir(),
                    &chat_type,
                    &chat_id,
                    None,
                    "chat_ask",
                    &card,
                ) {
                    Ok(message_id) => {
                        // exact card 绑定是 callback 围栏的一部分。若 gate 已在发卡窗口被取消，
                        // 这张卡会自然变成过期卡；绝不能重新建 gate 接住它。
                        if !core.bind_gate_message(&gate, &message_id) {
                            // turn 在发卡网络往返期间已结束：不能留下“看似能答、实则无 waiter”的卡。
                            let expired = summon_core::feishu::card_chat_ask_invalidated(
                                &summon_core::feishu::ChatAskCard {
                                    gate_id: "",
                                    requester_open_id: &requester_open_id,
                                    questions: &questions,
                                },
                                summon_core::feishu::AskCardEnd::TurnEnded,
                            );
                            let _ = update_card_archived(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                &message_id,
                                &expired,
                            );
                        }
                        core.cache_attention_position(&fc, &gate, &message_id);
                    }
                    Err(e) => {
                        // 卡片发不出去 = 没人能回答,阻塞的 agent 只会白等到被打断。
                        // 立刻取消闸门,让它拿到「问不出去」而不是无声挂死。
                        log(&summon_i18n::format!("r9e08106e42", agent.name));
                        core.cancel_undelivered_gate(&gate);
                    }
                }
            }
            // 等太久的提醒:**不是超时**,闸门仍在等。
            Ok(DispatchMsg::ChatAskRemind {
                gate_id,
                chat_id,
                chat_type,
                requester_open_id,
                waited_secs,
            }) => {
                let card = summon_core::feishu::card_chat_ask_remind(
                    &gate_id,
                    &requester_open_id,
                    waited_secs / 60,
                );
                if let Err(e) = send_card_archived(
                    &fc,
                    &agent.state_dir(),
                    &chat_type,
                    &chat_id,
                    None,
                    "chat_ask",
                    &card,
                ) {
                    log(&summon_i18n::format!("r380d9fc381", agent.name));
                }
            }
            Ok(DispatchMsg::ChatAskInvalidated {
                chat_id,
                chat_type,
                message_id,
                requester_open_id,
                questions,
                reason,
            }) => {
                let card = summon_core::feishu::card_chat_ask_invalidated(
                    &summon_core::feishu::ChatAskCard {
                        gate_id: "",
                        requester_open_id: &requester_open_id,
                        questions: &questions,
                    },
                    match reason {
                        super::ChatAskEndReason::TimedOut => {
                            summon_core::feishu::AskCardEnd::TimedOut
                        }
                        super::ChatAskEndReason::TurnEnded => {
                            summon_core::feishu::AskCardEnd::TurnEnded
                        }
                        super::ChatAskEndReason::ResolvedElsewhere => {
                            summon_core::feishu::AskCardEnd::Elsewhere
                        }
                    },
                );
                if let Err(error) = update_card_archived(
                    &fc,
                    &agent.state_dir(),
                    &chat_type,
                    &chat_id,
                    &message_id,
                    &card,
                ) {
                    log(&summon_i18n::format!("reb25cec55b", agent.name));
                }
            }
            Ok(DispatchMsg::ClaudePermissionCard {
                gate_id,
                chat_id,
                chat_type,
                requester_open_id,
                request,
            }) => {
                let spec = summon_core::feishu::ClaudePermissionCard {
                    gate_id: &gate_id,
                    requester_open_id: &requester_open_id,
                    request: &request,
                };
                let card = summon_core::feishu::card_claude_permission(&spec);
                let gate = summon_core::ids::GateId::new(gate_id.clone());
                match send_card_archived_id(
                    &fc,
                    &agent.state_dir(),
                    &chat_type,
                    &chat_id,
                    None,
                    "claude_permission",
                    &card,
                ) {
                    Ok(message_id) => {
                        // 发卡的网络往返里闸门可能已经结束(终端里答掉了):别留一张看似能点的卡。
                        if !core.bind_gate_message(&gate, &message_id) {
                            let void = summon_core::feishu::card_claude_permission_settled(
                                &spec,
                                summon_core::feishu::ClaudePermissionEnd::Elsewhere,
                            );
                            let _ = update_card_archived(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                &message_id,
                                &void,
                            );
                        }
                        core.cache_attention_position(&fc, &gate, &message_id);
                    }
                    Err(e) => {
                        // 卡发不出去 = 没人能点:取消闸门,hook 不作答,Claude 回到终端对话框。
                        log(&summon_i18n::format!("r5a2a58df6e", agent.name));
                        core.cancel_undelivered_gate(&gate);
                    }
                }
            }
            Ok(DispatchMsg::ClaudePermissionVoid {
                chat_id,
                chat_type,
                message_id,
                requester_open_id,
                request,
            }) => {
                let card = summon_core::feishu::card_claude_permission_settled(
                    &summon_core::feishu::ClaudePermissionCard {
                        gate_id: "",
                        requester_open_id: &requester_open_id,
                        request: &request,
                    },
                    summon_core::feishu::ClaudePermissionEnd::Elsewhere,
                );
                if let Err(error) = update_card_archived(
                    &fc,
                    &agent.state_dir(),
                    &chat_type,
                    &chat_id,
                    &message_id,
                    &card,
                ) {
                    log(&summon_i18n::format!("r52cc52fb05", agent.name));
                }
            }
            // 找边界要等它落盘、读转录,不能堵住 dispatcher。
            Ok(DispatchMsg::ClaudeCompacted {
                chat_id,
                chat_type,
                session,
                reported_at_ms,
            }) => {
                let (reporter, card_fc) = (agent.clone(), fc.clone());
                let epoch = crate::runtime::agent_retirement::epoch(&agent.dir);
                if let Err(error) = thread::Builder::new()
                    .name("summon-compact-card".into())
                    .spawn(move || {
                        let retired = epoch.read().unwrap_or_else(|e| e.into_inner());
                        if *retired {
                            return;
                        }
                        super::chat_thread::report_hook_compaction(
                            &reporter,
                            &card_fc,
                            &session,
                            &chat_type,
                            &chat_id,
                            reported_at_ms,
                        )
                    })
                {
                    log(&summon_i18n::format!("rf5266df74a", agent.name));
                }
            }
            Ok(DispatchMsg::ScheduledFire {
                chat_id,
                chat_type,
                prompt,
                fire_id,
                output,
                context,
            }) => {
                match inbox_wal.reject_disabled(&agent.name, &storage_scope.key, &fire_id) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => {
                        log(&summon_i18n::format!("r690012254b", agent.name));
                        continue;
                    }
                }
                let is_group = matches!(chat_type.as_str(), "group" | "topic_group");
                // 到点时这条 chat 正有一轮或让轮后的后台任务在跑 → 该轮沿用现有上下文。判据取
                // current-turn 与后台持有者记录;轮内到达再被回填的情形由 chat 线程在回填点兜底。
                let context = {
                    let effective = schedule_context_at_fire(
                        context,
                        chat_has_running_work(&agent, &chat_tag(&chat_type, &chat_id)),
                    );
                    if effective != context {
                        log(&summon_i18n::format!("r0fe7366fc6", agent.name));
                    }
                    effective
                };
                if is_group {
                    let target = summon_core::bridge::ScopedChatKey::new(
                        &storage_scope.key,
                        &agent.name,
                        &chat_id,
                    );
                    match bridge_slot_message(&bridge_store, &target) {
                        Ok(None) => {}
                        Ok(Some(_)) => {
                            inbox_wal.mark_consumed(&agent.name, &fire_id);
                            let _ = send_markdown_archived(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                summon_i18n::text!("r61ed647e50"),
                            );
                            continue;
                        }
                        Err(error) => {
                            inbox_wal.mark_consumed(&agent.name, &fire_id);
                            log(&summon_i18n::format!("r53718abd5f", agent.name));
                            continue;
                        }
                    }
                    if crate::remote::inbound::remote_bound(&agent, &chat_id) {
                        inbox_wal.mark_consumed(&agent.name, &fire_id);
                        log(&summon_i18n::format!("r512a6133cf", agent.name));
                        continue;
                    }
                }
                // 会话名:优先盘上 .meta 记录的(owner 裸名等历史命名),缺失再按规则 derive
                // (无 sender → 非 owner)。同 R3 启动续跑路径。
                let tmux_name = chat_session_name(&agent, &chat_type, &chat_id);
                let handle = chats.entry(chat_id.clone()).or_insert_with(|| {
                    log(&summon_i18n::format!("r94979d38eb", agent.name));
                    spawn_chat_thread(
                        agent.clone(),
                        storage_scope.key.clone(),
                        tmux_name.clone(),
                        poll,
                        debounce,
                        fc.clone(),
                        epoch_identity.clone(),
                        chat_id.clone(),
                        chat_type.clone(),
                        self_tx.clone(),
                        core.clone(),
                        inflight.clone(),
                    )
                });
                // 定时器合成轮:活动收口统一走 note_activity(时钟/空闲卡去重含 durable
                // marker/来源代际)。来源只登记 fire_id,**不置定时尾轮** —— 投递 ≠ 执行,
                // WAL 可能判 AlreadyStarted 直接跳过(迟到的重复 fire / 启动恢复重注入),在投递点
                // 置 true 会把真人尾轮改判回定时尾轮、55min 后误回收真人会话(终审阻塞①)。
                // 定时尾轮由 worker 真正取得 durable start 后回报的 ScheduledAdmitted 确认,
                // 且确认时 fire_id 仍须是最新入站(见 confirm_scheduled_admission)。
                // 空闲计时起点沿用 last_activity(= fire 到达);retire 判据另有「无活跃 turn」
                // 门槛,故长定时轮跑完前不会被拆,跑完后从这里起算的钟只会比「轮结束」早,
                // 方向与 idle-clock-from-disk 的「下界」语义一致。
                handle.note_activity(&agent, &chat_id, Some(&fire_id));
                handle.local_claimed = true;
                // 预告卡**不在这里发**:此刻只知道「收到了 fire」,不知道这一轮会不会真的执行
                // (WAL 可能判 AlreadyStarted 直接跳过)。文案随 task 交给 chat 线程,由它在拿到
                // durable start 之后再发 —— 见 `ChatTask::schedule_notice`。
                let task = scheduled_chat_task(
                    fire_id.clone(),
                    &prompt,
                    is_group,
                    effective.clone(),
                    output,
                    context,
                    resolve_settings(&agent, &handle.tmux_name, &effective).agent_reactions(),
                );
                log(&summon_i18n::format!("r9ae8d8c755", agent.name));
                if handle.tx.send(ThreadMsg::Task(Box::new(task))).is_err() {
                    chats.remove(&chat_id); // 线程已退,下条重起
                }
            }
            Ok(DispatchMsg::ScheduledAdmitted { chat_id, fire_id }) => {
                // 定时尾轮判定的唯一置 true 点:worker 已对该 fire 取得 durable start(Marked)。
                // 代际围栏:fire_id 必须仍是该 chat 的最新入站 —— admission 在途期间真人插话/
                // 新 fire 都会推进 last_inbound_id,迟到的确认自动作废,保持普通空闲规则。
                // handle 已被摘除(ChatExited 先到)则直接丢弃:重建的 handle 从 false 起步,方向安全。
                if let Some(handle) = chats.get_mut(&chat_id) {
                    if handle.confirm_scheduled_admission(&fire_id) {
                        log(&summon_i18n::format!("r306bcf36a6", agent.name));
                    } else {
                        log(&summon_i18n::format!("r20867aa181", agent.name));
                    }
                }
            }
            Ok(DispatchMsg::SentinelFire {
                chat_id,
                chat_type,
                prompt,
                fire_id,
                who,
                who_id,
                who_union_id,
                handoff_note,
            }) => {
                match inbox_wal.reject_disabled(&agent.name, &storage_scope.key, &fire_id) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => {
                        log(&summon_i18n::format!("rbf9dc1aeb6", agent.name));
                        continue;
                    }
                }
                if super::delegate::validate_receiver(
                    &agent,
                    &storage_scope.key,
                    &fire_id,
                    &chat_id,
                    &chat_type,
                    &who_id,
                    &who_union_id,
                )
                .is_err()
                {
                    log(&summon_i18n::format!("r0c863d3071", agent.name));
                    continue;
                }
                let is_group = matches!(chat_type.as_str(), "group" | "topic_group");
                if is_group {
                    let target = summon_core::bridge::ScopedChatKey::new(
                        &storage_scope.key,
                        &agent.name,
                        &chat_id,
                    );
                    match bridge_slot_message(&bridge_store, &target) {
                        Ok(None) => {}
                        Ok(Some(_)) => {
                            inbox_wal.mark_consumed(&agent.name, &fire_id);
                            let _ = send_markdown_archived(
                                &fc,
                                &agent.state_dir(),
                                &chat_type,
                                &chat_id,
                                summon_i18n::text!("r92417a7281"),
                            );
                            continue;
                        }
                        Err(error) => {
                            inbox_wal.mark_consumed(&agent.name, &fire_id);
                            log(&summon_i18n::format!("ra4bb07f2f1", agent.name));
                            continue;
                        }
                    }
                    if crate::remote::inbound::remote_bound(&agent, &chat_id) {
                        inbox_wal.mark_consumed(&agent.name, &fire_id);
                        log(&summon_i18n::format!("r50ad2c764f", agent.name));
                        continue;
                    }
                }
                // 会话名推导同 ScheduledFire:优先盘上 .meta,缺失按规则 derive(无 sender → 非 owner)。
                let tmux_name = chat_session_on_disk(&agent, &chat_id)
                    .map(|(session, _, _)| session)
                    .unwrap_or_else(|| agent.tmux_name_for_chat(&chat_type, &chat_id, false));
                let handle = chats.entry(chat_id.clone()).or_insert_with(|| {
                    log(&summon_i18n::format!("rba7f2449c7", agent.name));
                    spawn_chat_thread(
                        agent.clone(),
                        storage_scope.key.clone(),
                        tmux_name.clone(),
                        poll,
                        debounce,
                        fc.clone(),
                        epoch_identity.clone(),
                        chat_id.clone(),
                        chat_type.clone(),
                        self_tx.clone(),
                        core.clone(),
                        inflight.clone(),
                    )
                });
                // sentinel 是 owner 亲手召唤,不是定时任务 → 普通空闲规则;活动收口统一走
                // note_activity(时钟/空闲卡去重含 durable marker/来源代际),在途的定时
                // admission 一并作废。
                handle.note_activity(&agent, &chat_id, Some(&fire_id));
                handle.local_claimed = true;
                let task = sentinel_chat_task(
                    fire_id.clone(),
                    &prompt,
                    handoff_note,
                    is_group,
                    who,
                    who_id,
                    who_union_id,
                    effective.clone(),
                    resolve_settings(&agent, &handle.tmux_name, &effective).agent_reactions(),
                );
                log(&summon_i18n::format!("rc962ca9404", agent.name));
                if handle.tx.send(ThreadMsg::Task(Box::new(task))).is_err() {
                    chats.remove(&chat_id); // 线程已退,下条重起
                }
            }
            Ok(DispatchMsg::RecallMessage { message_id }) => {
                // 用户撤回消息:写 WAL tombstone,chat 线程消费 Task 前会检查、已撤回则跳过。
                // 仅影响排队中消息,不打断正在跑的轮。
                inbox_wal.mark_recalled(&agent.name, &message_id);
                log(&summon_i18n::format!("rfdf8fde09b", agent.name));
            }
            // 改动已由循环顶部的 mtime 检查推给在线 chat 线程。
            Ok(DispatchMsg::AgentYamlWritten) => {}
            // R8:空闲扫描(外带 kill 探测 + idle 卡片)已挪到循环尾、与消息到达解耦(见 run_idle_scan
            // 的 last_scan 节拍)。Timeout 分支仅用于「队列空时按 idle_scan 周期醒一下」,不再承载扫描——
            // 否则消息流稳定时 Timeout 永不触发,扫描被饿死。
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // R8:每轮(无论 Ok/Timeout)都查「距上次扫描 >= idle_scan 则跑一遍」,把扫描与「队列恰好安静」
        // 解耦。扫描触发只看 last_scan,不再被持续的消息流重置而饿死(外带 kill 回执/空闲卡片不再
        // 依赖队列安静)。唤醒时长同样按 last_scan 算(见 next_recv_timeout),两边一致:只要消息间
        // 存在间隙,扫描间隔的上界就是 idle_scan。已知缺口:match 各分支的 continue 会跳过本检查,
        // 队列持续非空且每条都命中 continue 的无间隙流量仍可推迟扫描——但每轮唤醒窗口都按真实
        // elapsed 收窄,稀疏消息不受影响(缺口为 R8 基线既有,非 next_recv_timeout 引入)。
        if last_scan.elapsed() >= effective.idle_scan() {
            // 迁移未完成(启动时已告警)→ 没有证明,扫描不可调用;last_scan 照常重置 ——
            // 它同时是 next_recv_timeout 的节拍器,不重置就是 recv_timeout(0) 忙循环
            // (2026-08-10 的 70% CPU 教训)。
            if let Some(proof) = &idle_scan_proof {
                run_idle_scan(&mut chats, &agent, &effective, &self_tx, &fc, proof);
            }
            last_scan = std::time::Instant::now();
        }
    }
}

/// 管家会话操作页上的「退出」:把动作交给 Core(过**目标 agent** 那道 operate 闸)后原地重渲本页。
///
/// **管家自己那道闸已经在集中鉴权里过了**(`FleetSessionControl` 是 operate 档),这里不重复;
/// 第二道在 `Core::route_fleet_session_control` 里,理由见那里。
///
/// 参数缺失一律当渲染出错处理:不猜 agent、不猜 chat —— 猜错就是停掉别人的会话。
fn handle_fleet_session_control(
    agent: &Agent,
    fc: &FeishuClient,
    core: &std::sync::Arc<crate::runtime::core::Core>,
    card: &CardActionMsg,
) {
    let action = card
        .value
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let reset = action == "fleet_session_reset";
    let target = card.value.get("agent").and_then(|v| v.as_str());
    let chat_id = card.value.get("chat_id").and_then(|v| v.as_str());
    let (Some(target), Some(chat_id)) = (target, chat_id) else {
        log(&summon_i18n::format!("r125b9c9e74", agent.name, card.value));
        return;
    };
    let op_union = fc.resolve_union_id(&card.operator_open_id);
    let outcome =
        core.route_fleet_session_control(&card.operator_open_id, &op_union, target, chat_id, reset);
    let verb = if reset {
        summon_i18n::text!("rdc2e173490")
    } else {
        summon_i18n::text!("r498e1d59b4")
    };
    let notice = match &outcome {
        summon_core::core_port::CommandOutcome::Done => {
            log(&summon_i18n::format!(
                "rafe51655ef",
                agent.name,
                card.operator_open_id
            ));
            // 拆除是异步的(投给目标 agent 的 dispatcher 再到 chat 线程),此刻列表多半还没变 ——
            // 所以说「已下发」而不是「已完成」,并提示刷新看结果。谎报完成会让人以为没生效而重复点。
            summon_i18n::format!("r20bdff741e")
        }
        summon_core::core_port::CommandOutcome::Denied(reason) => {
            log(&summon_i18n::format!(
                "r56f1c178b8",
                agent.name,
                card.operator_open_id
            ));
            format!("🔒 {reason}")
        }
        other => {
            log(&summon_i18n::format!("r19eb21884f", agent.name));
            summon_i18n::format!("rb368c80e85")
        }
    };
    steward::handle_sessions_card(agent, fc, &card.message_id, target, Some(&notice));
}

/// 会话镜像 slice 2(detached watcher,`messaging.session_mirror` **默认开**,仅 claude/Transcript):
/// 持续 tail 转录,把**任何** assistant turn 推回飞书 —— 含用户 attach 终端直敲触发的(飞书没注入也转发)。
/// normal turn 的双发防护按 typed marker + exact delivery tracker 仲裁；mirror_watermark 只表达
/// cold seed 的连续前缀，绝不能跨 prompt gate 产生的洞。
/// owned `agent`(clone)→ 无 lifetime 羁绊,可 detached 跑。等会话冷启出 native sid(转录就位)再 tail。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MirrorTurnToken(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MirrorFailureClaim {
    /// mirror 已确认送达，或已发起但网络结果不可证明。两者都禁止用另一
    /// 发送路径追加失败卡；安全优先，宁可暂时漏答也不双发。
    SuppressFailureNotice,
    /// 当前 turn 没有 mirror 发送尝试，失败路径负责给用户发可见回执。
    SendFailureNotice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MainDeliveryClaim {
    /// 主路径取得本轮输出所有权；mirror 对同 marker 的晚到 terminal 必须让路。
    DeliverOnMainPath,
    /// mirror 已确认送达或投递结果不可证明；主路径只收尾，不得用不同幂等键再发。
    SuppressMainDelivery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MirrorDeliveryPhase {
    /// 当前 prompt 注入尚未把 exact transport marker 登记回来。
    AwaitingMarker,
    /// exact transport marker 已登记；只有携带同 marker 的 terminal 才能绑定当前 turn。
    Eligible,
    /// 飞书发送已取得所有权但网络结果未返回；失败路径必须等结果，不能抢跑。
    Sending,
    /// 飞书发送成功，是唯一能抑制当前 turn 失败回执的状态。
    Delivered,
    /// 发送已产生网络副作用，但 Err/panic 使本地无法证明飞书是否已收到。主路径/失败卡必须
    /// suppress；没有 durable attempt ledger 前，后续只做保守本地收敛，不能换路径猜测重发。
    DeliveryUncertain,
    /// 主 Transcript gate 已认出 exact marker 并取得输出所有权；mirror 必须让路。
    MainClaimed,
    /// 失败路径先取得所有权；晚到的同轮镜像不得再发送答案，避免答案与失败卡双发。
    FailureClaimed,
}

#[derive(Debug)]
struct ActiveMirrorTurn {
    token: MirrorTurnToken,
    marker: Option<String>,
    /// marker 绑定时 watcher 已冻结起点。true 时 marker 不可能落在 watcher 起点之前，因此随后
    /// evidence 为空的 terminal 明确属于前序 attach；false 覆盖 cold Bypassed marker straddle。
    marker_bound_after_watcher_ready: bool,
    delivery_marker: Option<String>,
    delivery_offset: Option<u64>,
    /// 主 gate 认出 exact marker 消费记录的字节边界。未带 marker 的 terminal 只有
    /// 严格在它之后才可能属于本轮；更早 terminal 是前序 attach。
    consumed_boundary: Option<u64>,
    /// 本 active 期间是否已观察到本轮 terminal（exact 或因无 marker 而 fail-closed 归入本轮）。
    /// finish 只在从未观察到 terminal 时留下 markerless tombstone，避免误伤下一条 attach turn。
    terminal_observed: bool,
    /// mirror 观察到的 exact terminal boundary；Peer/Reply 同属一个 terminal，先处理 Peer 后靠它
    /// 保证 Reply 不会把同一 cold-straddle 终态重新当 attach 发送。
    terminal_observed_offset: Option<u64>,
    /// 本轮内 steer 注入的 transport marker(旁路名单,有界)。**不走 bind_transport_marker**:
    /// 那条路会清 consumed_boundary/delivery/terminal 边界(为 `!` rerun 的新顺序区间设计),
    /// steer 不是新区间,清掉会抹掉当前轮的顺序证明。名单干三件事:terminal 证据里出现某
    /// steer marker 时当场认领(N:1 并轮,mirror 照旧让路);孤儿 terminal 在轮内到达且三条
    /// 前提全证成时就地放行(主路径已 claim、RAII 尚未 finish 的窗口,见 begin_delivery);
    /// 本轮收尾仍无人认领时,由 finish_turn 记孤儿放行凭证(steer 在 end_turn 后才被消费、
    /// 自成一轮的场景)。
    steer_markers: Vec<String>,
    /// turn 类 slash(零 marker 注入)的**显式 markerless fence**:注入前冻结的转录字节 EOF。
    /// `Some(fence)` 声明本轮是主路径自己消费的 markerless 轮 —— 严格在 fence 右侧的
    /// markerless terminal 归主路径(mirror 让路),fence 左侧(≤)的仍是前序 attach 轮、按旧
    /// 语义放行。归属由这条**显式声明**裁决,不靠「当前没有 active tracker」碰巧放行;
    /// `!` rerun 升级成 marker 轮时(bind_transport_marker)fence 随即清空,exact 判据接管。
    markerless_fence: Option<u64>,
    /// 主路径已读到并下过结论的最后一个 terminal 的边界,与 mirror 的 terminal_offset 是同一套
    /// 字节偏移(行尾 `\n` 之后)。续跑区间右端未定时,mirror 据它判断某一段主路径处理完没有。
    main_read_through: Option<u64>,
    phase: MirrorDeliveryPhase,
}

impl ActiveMirrorTurn {
    /// 从本轮 steer 名单里认领已出现在**消费证据**里的 marker,返回认领到的那些。
    /// 「该 marker 从未出现在任何已消费 terminal 的 evidence 里」是孤儿放行的前提之二,
    /// 认领(移除)就是这条前提的实现:被认领过的 marker 不会再进孤儿凭证。
    fn claim_steer_markers(&mut self, consumed: &[String]) -> Vec<String> {
        let mut claimed = Vec::new();
        self.steer_markers.retain(|marker| {
            let hit = consumed.iter().any(|candidate| candidate == marker);
            if hit {
                claimed.push(marker.clone());
            }
            !hit
        });
        claimed
    }

    /// 孤儿 steer 放行的公共前提(轮内就地放行与 finish_turn 留凭证共用同一判据,不许分叉):
    /// 主路径已对用户负责(MainClaimed/Delivered)且 mirror 亲眼记下了本轮 terminal boundary
    /// (有 boundary 才能对孤儿 terminal 判序)。返回该 boundary;任一条证不了返回 `None`
    /// (fail-closed,宁漏发)。FailureClaimed/Uncertain 都证不了 —— 答案与失败卡不能双发。
    fn orphan_release_boundary(&self) -> Option<u64> {
        if !matches!(
            self.phase,
            MirrorDeliveryPhase::MainClaimed | MirrorDeliveryPhase::Delivered
        ) || !self.terminal_observed
        {
            return None;
        }
        self.terminal_observed_offset
    }
}

/// 孤儿 steer 轮的一次性放行凭证(修 3)。两个前提在 finish_turn 当场证过:主路径 terminal
/// 已被消费(后面带该 marker 的 terminal 必是新轮),且该 steer marker 从未出现在任何已消费
/// terminal 的证据里(没有任何路径投递过它的答案)。
#[derive(Clone, Debug, Eq, PartialEq)]
struct OrphanSteerMarker {
    marker: String,
    /// 主路径 terminal 的 boundary(mirror 亲眼观察到的)。孤儿轮 terminal 必须严格在它右侧,
    /// 否则序证明失败,凭证作废(fail-closed,宁漏发)。
    after_offset: u64,
}

/// 续跑区间 (after, through]:`after` = active turn 的 consumed_boundary(或 markerless_fence);
/// `through` = 主路径收尾时读到的最后一个 terminal 边界,收尾前为 None。右端钉住之后,区间外的
/// terminal 主路径没读到、也不会投递(比如续跑结束后终端里直接敲出的轮),按原有规矩归 mirror
/// 裁决。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ContinuationClaim {
    after: u64,
    through: Option<u64>,
}

/// 续跑区间里 mirror 先于主路径读到某一段时,等主路径对它下结论的上限。主路径每 300ms 读一次
/// 转录,只在同步发段终点回复时会慢几秒;等满仍没结论就让路(宁漏勿重)。
const CONTINUATION_VERDICT_WAIT: Duration = Duration::from_secs(30);

/// mirror 对续跑区间里一个 terminal 的判断。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContinuationVerdict {
    /// 不在任何续跑区间里,按原有规矩裁决。
    Outside,
    /// 主路径已读到并对这一段下了结论(投递与否见 `settle_main_segment`)。
    SettledByMain,
    /// 主路径迟迟没读到也没收尾,判不出。
    Undecided,
}

/// 镜像让路(Suppressed)的原因。**只做观测,不参与任何裁决**:此前 begin_delivery 的 7 条
/// Suppressed 出口在日志里全被硬编码成「已由失败回执取得发送所有权」,而实际样本绝大多数是
/// MainClaimed —— 误标让 2026-08-13 的排查整整偏航一轮。拆开后日志照实打印,判决逻辑零改动。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MirrorSuppressedBy {
    /// 主 Transcript gate 已认出 exact marker 并抢到本轮输出权(常态:gate 早于 terminal)。
    MainClaimed,
    /// 失败回执先取得所有权,晚到的同轮答案让路。
    FailureClaimed,
    /// marker 已有裁决记录(resolved 队列),迟到/重复 terminal 让路。
    Resolved,
    /// markerless tombstone 兜底压掉(parser schema 漂移时的一次性 fail-closed)。
    Tombstone,
    /// 同一枚 terminal boundary 已被观察过(Peer/Reply 复用同一裁决,或同 marker 已投递)。
    DuplicateTerminal,
    /// 无 marker 且无法证明归属,fail-closed 记到当前轮观察。
    MarkerlessFailClosed,
    /// 镜像自己对同 marker 的发送尚在进行(Sending),结果未返回。
    InFlight,
    /// terminal 只带当前活跃轮的 steer marker(不带主 marker),且轮内就地放行的三条前提
    /// (主路径已 MainClaimed/Delivered、mirror 记下主 terminal boundary、孤儿严格在其右侧)
    /// 有证不了的 → fail-closed 让路;认领记录同时保证 finish_turn 不再为它开孤儿口(宁漏发)。
    SteerClaimedByActiveTurn,
    /// turn 类 slash 的 markerless 主路径轮:terminal 严格落在显式 fence 右侧,归主路径的
    /// 同构消费路投递(流式卡 + 收尾),mirror 让路。
    MarkerlessMainTurn,
    /// 跨后台任务续跑区间里的 terminal(见 `continuation_claims`):主路径读到并处理了这一段
    /// (发段终点回复或终态),或主路径迟迟没有结论,mirror 让路。
    ContinuationClaim,
}

/// observation-only(零网络)的原因。同款教训:三种「Observed」在日志里长得一模一样,
/// 分不出是历史观察、歧义基线还是本代不认识的 marker。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MirrorObservedBy {
    /// 带本代 tracker 不认识的 consumed marker(lazy replay / 历史 / 上一代 bind)。
    HistoricalUnknown,
    /// 主路径已消费的本轮 terminal,mirror 只收敛凭证。
    ReplyObserved,
    /// Ready 前冻结的历史 backlog,与当前 tracker 代际无可证明关联。
    HistoricalObserved,
    /// marker 候选溢出 / 未知 shape 歧义,fail-closed 只推进 offset。
    AmbiguousObserved,
    /// 通知卡属于主路径或历史 Summon turn。
    NoticeObservation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MarkerlessTombstone {
    token: MirrorTurnToken,
    /// Some = marker 轮留下的(可被 exact Enqueued 证据排除);None = markerless fence 轮
    /// 留下的(没有 marker 可比对,只按 after_offset 判序)。
    marker: Option<String>,
    /// Some = exact marker 已被主 gate 消费(或 fence 轮的注入基线)；只压严格更后的 terminal。
    /// None 表示 partial-send/注入失败窗口，仍 fail-closed，但 exact Enqueued 可证明当前
    /// terminal 更早。
    after_offset: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkerlessTerminalDisposition {
    Untracked,
    /// 携带首次冻结时的让路原因,Peer/Reply 复用同一裁决时日志才能照实打印。
    Suppressed(MirrorSuppressedBy),
    SuppressedByTombstone(MirrorTurnToken),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum MirrorWatcherState {
    /// 当前 chat 没启用 mirror，主路径无需等待。
    #[default]
    Disabled,
    /// watcher 已启动，但尚未取得 native sid 对应转录的起始 EOF。
    Starting,
    /// transcript 尚未创建/仍 0B，先放开主路径打破“首 prompt 才产首行”的循环依赖；watcher
    /// 自身仍在 stop-aware 重试，取得稳定非空起点后会转为 Ready。
    Bypassed,
    /// 起始 offset 已冻结；从此注入的 marker 不会落在 watcher 起点之前。
    Ready,
    /// watcher 在 ready 前/后退出。主路径继续独立工作，不再把 mirror 当兜底。
    Failed,
}

/// 会话镜像与 Transcript 主路径之间的单 chat 送达仲裁器。
///
/// 它不再用「某段时间里全局计数变大」猜本轮是否已回答，而是把凭证绑定到一个 opaque turn token，
/// 并且只有携带当前 prompt exact transport marker 的 terminal turn 成功发送后，才有资格完成本
/// token。终端 attach 产生的无 marker turn、别的 marker，以及发送失败都不会污染本轮凭证。
#[derive(Debug, Default)]
pub(super) struct MirrorDeliveryTracker {
    next_token: std::sync::atomic::AtomicU64,
    active: std::sync::Mutex<Option<ActiveMirrorTurn>>,
    /// 已裁决 marker 的有界 tombstone：失败卡先赢时拦截迟到答案；答案先赢时拦截重复 terminal。
    resolved_markers: std::sync::Mutex<VecDeque<(String, Option<u64>)>>,
    /// 主/失败路径收尾后仍可能迟到一个无法提取 marker 的 terminal。每枚 tombstone 只压一次；
    /// 这是 parser schema 漂移时的 fail-closed 兜底，不把 attach 永久关闭。
    markerless_tombstones: std::sync::Mutex<VecDeque<MarkerlessTombstone>>,
    /// Peer/Reply/ReplyObserved 是同一 terminal 的多个回调。Peer 首次冻结 markerless 归属，
    /// 同 boundary 的后续块必须复用，禁止网络期间新 active/tombstone 导致“半轮”。
    markerless_dispositions: std::sync::Mutex<VecDeque<(u64, MarkerlessTerminalDisposition)>>,
    /// 孤儿 steer 轮的一次性放行凭证(有界,见 `OrphanSteerMarker`)。锁序恒为 active → 本表,
    /// 与 active → resolved 同型,不与任何反向持锁路径共存。
    orphan_steer_markers: std::sync::Mutex<VecDeque<OrphanSteerMarker>>,
    /// 跨后台任务续跑区间(见 `ContinuationClaim`),按偏移递增排列,只有最后一个可能右端未定。
    /// 主路径在 end_turn 时仍有后台任务没通知 → 续跑,区间内的 terminal 由主路径读到并
    /// 下结论,mirror 不抢。**挂 tracker 不挂 active turn**:mirror 的轮询在转录静默时退避到 5s,
    /// 而续跑正是长时间静默,它常在主路径发完终态、收完尾,甚至下一轮已经开始之后才读到续跑的
    /// 最后一段。右端钉住的区间一直留到 mirror 读过它的右端;右端没钉上的随轮收尾 / 换轮清掉。
    /// 偏移跨轮可比:一个 tracker 只对应一份转录,换会话(重置 / 新会话)会退出 chat 线程,
    /// 转录被截断或替换时 mirror fail-closed 退出。
    continuation_claims: std::sync::Mutex<VecDeque<ContinuationClaim>>,
    /// 测试用:替换 `CONTINUATION_VERDICT_WAIT`。
    continuation_verdict_wait: Option<Duration>,
    changed: std::sync::Condvar,
    watcher: std::sync::Mutex<MirrorWatcherState>,
    watcher_changed: std::sync::Condvar,
}

impl MirrorDeliveryTracker {
    fn lock_watcher(&self) -> std::sync::MutexGuard<'_, MirrorWatcherState> {
        self.watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// spawn 前先声明 watcher 正在启动。首个 Transcript turn 会在 `ensure_started` 之后等待
    /// `mark_watcher_ready`，堵住 cold start 默认 500ms 轮询造成的 EOF 竞态。
    pub(super) fn expect_watcher(&self) {
        *self.lock_watcher() = MirrorWatcherState::Starting;
        self.watcher_changed.notify_all();
    }

    /// `mirror_tail` 已冻结 native sid + start offset 后调用；这才是允许首个 prompt 注入的 fence。
    pub(super) fn mark_watcher_ready(&self) {
        *self.lock_watcher() = MirrorWatcherState::Ready;
        self.watcher_changed.notify_all();
    }

    pub(super) fn mark_watcher_bypassed(&self) {
        let mut state = self.lock_watcher();
        if *state == MirrorWatcherState::Starting {
            *state = MirrorWatcherState::Bypassed;
            self.watcher_changed.notify_all();
        }
    }

    /// watcher 此刻能投递之后的输出:起始 offset 已冻结,且还没退出。
    pub(super) fn watcher_delivers(&self) -> bool {
        *self.lock_watcher() == MirrorWatcherState::Ready
    }

    /// watcher 退出时撤销 ready。失败后主路径仍可工作，只是不再依赖 mirror 证明送达。
    pub(super) fn mark_watcher_failed(&self) {
        *self.lock_watcher() = MirrorWatcherState::Failed;
        self.watcher_changed.notify_all();
    }

    /// 等 watcher 取得 EOF fence。Disabled/Failed 都不会阻塞；Starting 超时则本轮在注入前失败，
    /// 绝不能一边让迟到 watcher 从 marker 后的 EOF 起步，一边继续主路径制造重复答案。
    pub(super) fn wait_watcher_ready(&self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.lock_watcher();
        loop {
            match *state {
                MirrorWatcherState::Disabled
                | MirrorWatcherState::Bypassed
                | MirrorWatcherState::Ready
                | MirrorWatcherState::Failed => return Ok(()),
                MirrorWatcherState::Starting => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return Err(anyhow::anyhow!(summon_i18n::format!("rea3a3cd54b")));
                    }
                    let (next, wait) = self
                        .watcher_changed
                        .wait_timeout(state, deadline.saturating_duration_since(now))
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state = next;
                    if wait.timed_out() && *state == MirrorWatcherState::Starting {
                        return Err(anyhow::anyhow!(summon_i18n::format!("rea3a3cd54b")));
                    }
                }
            }
        }
    }

    fn lock_active(&self) -> std::sync::MutexGuard<'_, Option<ActiveMirrorTurn>> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_resolved(&self) -> std::sync::MutexGuard<'_, VecDeque<(String, Option<u64>)>> {
        self.resolved_markers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remember_markerless_tombstone(&self, current: &ActiveMirrorTurn) {
        const MAX_MARKERLESS_TOMBSTONES: usize = 128;
        // marker 在任何 send 副作用前登记;markerless fence 同理在注入前绑定。两者都没有,说明
        // run_turn 在定位 sid/path 等前置阶段就失败了，CLI 根本没收到 prompt；此时留 tombstone
        // 只会误伤下一条合法 attach。fence 轮的 after_offset 取注入基线:注入确认后主路径已
        // claim,基线右侧迟到的 terminal 正是要压的那一枚。
        if current.marker.is_none() && current.markerless_fence.is_none() {
            return;
        }
        let after_offset = current.consumed_boundary.or(current.markerless_fence);
        let mut tombstones = self
            .markerless_tombstones
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = tombstones
            .iter_mut()
            .find(|tombstone| tombstone.token == current.token)
        {
            existing.marker.clone_from(&current.marker);
            existing.after_offset = after_offset;
            return;
        }
        tombstones.push_back(MarkerlessTombstone {
            token: current.token,
            marker: current.marker.clone(),
            after_offset,
        });
        while tombstones.len() > MAX_MARKERLESS_TOMBSTONES {
            tombstones.pop_front();
        }
    }

    fn matching_markerless_tombstone(
        &self,
        evidence: &crate::capture::MirrorMarkerEvidence,
        terminal_offset: u64,
    ) -> Option<MirrorTurnToken> {
        let tombstones = self
            .markerless_tombstones
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tombstones
            .iter()
            .find(|tombstone| {
                // fence 轮的 tombstone 无 marker 可比对(marker=None),只按 after_offset 判序。
                let explicitly_before_prompt = tombstone.marker.as_ref().is_some_and(|marker| {
                    evidence
                        .enqueued()
                        .iter()
                        .any(|candidate| candidate == marker)
                });
                !explicitly_before_prompt
                    && tombstone
                        .after_offset
                        .is_none_or(|after| terminal_offset > after)
            })
            .map(|tombstone| tombstone.token)
    }

    fn consume_markerless_tombstone(&self, token: MirrorTurnToken) -> bool {
        let mut tombstones = self
            .markerless_tombstones
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = tombstones
            .iter()
            .position(|tombstone| tombstone.token == token)
        {
            tombstones.remove(index);
            true
        } else {
            false
        }
    }

    fn retire_markerless_tombstones_at_exact_terminal(
        &self,
        marker: Option<&str>,
        terminal_offset: u64,
        active_token: Option<MirrorTurnToken>,
    ) {
        self.markerless_tombstones
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|tombstone| {
                let same_marker = marker
                    .zip(tombstone.marker.as_deref())
                    .is_some_and(|(exact, saved)| exact == saved);
                let proven_older = tombstone
                    .after_offset
                    .is_some_and(|after| after < terminal_offset);
                // 只有 mirror 按 transcript 顺序亲眼走到更晚 active token 的 exact terminal，
                // 才能退役没有消费边界的旧 tombstone；main 观察不能提供这条顺序证明。
                let older_token = active_token.is_some_and(|token| tombstone.token.0 < token.0);
                !same_marker && !proven_older && !older_token
            });
    }

    fn cached_markerless_disposition(
        &self,
        terminal_offset: u64,
    ) -> Option<MarkerlessTerminalDisposition> {
        self.markerless_dispositions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(saved_offset, _)| *saved_offset == terminal_offset)
            .map(|(_, disposition)| *disposition)
    }

    fn remember_markerless_disposition(
        &self,
        terminal_offset: u64,
        disposition: MarkerlessTerminalDisposition,
    ) {
        const MAX_MARKERLESS_DISPOSITIONS: usize = 128;
        let mut dispositions = self
            .markerless_dispositions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, saved)) = dispositions
            .iter_mut()
            .find(|(saved_offset, _)| *saved_offset == terminal_offset)
        {
            *saved = disposition;
            return;
        }
        dispositions.push_back((terminal_offset, disposition));
        while dispositions.len() > MAX_MARKERLESS_DISPOSITIONS {
            dispositions.pop_front();
        }
    }

    fn take_markerless_disposition(
        &self,
        terminal_offset: u64,
    ) -> Option<MarkerlessTerminalDisposition> {
        let mut dispositions = self
            .markerless_dispositions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = dispositions
            .iter()
            .position(|(saved_offset, _)| *saved_offset == terminal_offset)?;
        dispositions
            .remove(index)
            .map(|(_, disposition)| disposition)
    }

    fn remember_resolved(&self, marker: &str, terminal_offset: Option<u64>) {
        const MAX_RESOLVED_MARKERS: usize = 128;

        let mut resolved = self.lock_resolved();
        if let Some(existing) = resolved.iter_mut().find(|(saved, _)| saved == marker) {
            if terminal_offset.is_some() {
                existing.1 = terminal_offset;
            }
            return;
        }
        resolved.push_back((marker.to_string(), terminal_offset));
        while resolved.len() > MAX_RESOLVED_MARKERS {
            resolved.pop_front();
        }
    }

    fn lock_orphan_steer(&self) -> std::sync::MutexGuard<'_, VecDeque<OrphanSteerMarker>> {
        self.orphan_steer_markers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 轮内 steer 注入成功后登记 steer marker(修 3,transcript 的 Steer 分支调用)。
    /// 见 `ActiveMirrorTurn::steer_markers` 的注释:不是 bind,不动任何既有顺序边界。
    pub(super) fn note_steer_marker(&self, token: MirrorTurnToken, marker: impl Into<String>) {
        // steer 名单上限:一轮里塞进两位数 steer 已属异常,超界的宁可退化成 observation-only
        // (漏发方向),也不让名单无界增长。
        const MAX_STEER_MARKERS: usize = 8;
        let mut active = self.lock_active();
        let Some(current) = active.as_mut().filter(|current| current.token == token) else {
            return;
        };
        let marker = marker.into();
        if current.marker.as_deref() == Some(marker.as_str())
            || current.steer_markers.contains(&marker)
        {
            return;
        }
        if current.steer_markers.len() >= MAX_STEER_MARKERS {
            log(&summon_i18n::format!("rd836a49837"));
            return;
        }
        current.steer_markers.push(marker);
    }

    /// finish_turn/begin_turn 收尾时,把本轮**无人认领**的 steer marker 落成孤儿放行凭证。
    /// 报告的两前提之上再收一格:必须有 mirror 亲眼观察到的主 terminal boundary。若只有
    /// main parser 的观察(无 offset),mirror 侧可能还没消化合并 terminal —— 那枚 terminal
    /// 稍后带着 steer marker 到达时无从判序,留凭证就是双发口子;证不了 → 不留(漏发方向)。
    fn remember_orphan_steer_markers(&self, current: &ActiveMirrorTurn) {
        const MAX_ORPHAN_STEER_MARKERS: usize = 32;
        if current.steer_markers.is_empty() {
            return;
        }
        // 前提(与轮内就地放行同一判据,见 orphan_release_boundary):主路径已对用户负责且
        // mirror 亲眼记下本轮 terminal boundary —— 之后带 steer marker 的 terminal 必是新轮。
        let Some(after_offset) = current.orphan_release_boundary() else {
            return;
        };
        let mut orphans = self.lock_orphan_steer();
        for marker in &current.steer_markers {
            if orphans.iter().any(|orphan| orphan.marker == *marker) {
                continue;
            }
            orphans.push_back(OrphanSteerMarker {
                marker: marker.clone(),
                after_offset,
            });
        }
        while orphans.len() > MAX_ORPHAN_STEER_MARKERS {
            orphans.pop_front();
        }
    }

    /// 消费一枚孤儿放行凭证:consumed 证据命中即**取出销账**(先销后发,send 失败宁漏不重试),
    /// 且只有 terminal 严格晚于主 terminal boundary 才真正放行;序证明失败凭证同样作废。
    fn take_orphan_steer_marker(
        &self,
        consumed: &[String],
        terminal_offset: u64,
    ) -> Option<String> {
        let mut orphans = self.lock_orphan_steer();
        let index = orphans
            .iter()
            .position(|orphan| consumed.iter().any(|candidate| candidate == &orphan.marker))?;
        let orphan = orphans.remove(index)?;
        (terminal_offset > orphan.after_offset).then_some(orphan.marker)
    }

    /// observation-only signal 已证明该 terminal 无需镜像发送；这里只收敛 tracker 凭证，绝不取得
    /// 发送 permit。
    /// exact active 优先，active 已清理则用 resolved marker 消化 finish 留下的一次性兜底 tombstone。
    fn observe_consumed_terminal(
        &self,
        evidence: &crate::capture::MirrorMarkerEvidence,
        terminal_offset: u64,
    ) {
        let consumed = evidence.consumed();
        let mut active = self.lock_active();
        if let Some(current) = active.as_mut() {
            // 观察路径同样要认领 steer marker(N:1 并轮的 terminal 可能以 ReplyObserved 形态
            // 到达):没有这步,finish_turn 会把已被消费的 steer 误记成孤儿凭证。
            let _ = current.claim_steer_markers(consumed);
            let exact_marker = current.marker.as_ref().filter(|expected| {
                consumed
                    .iter()
                    .any(|candidate| candidate.as_str() == expected.as_str())
            });
            if let Some(marker) = exact_marker.cloned() {
                current.terminal_observed = true;
                current.terminal_observed_offset = Some(terminal_offset);
                current.delivery_offset = Some(terminal_offset);
                self.remember_resolved(&marker, Some(terminal_offset));
                self.retire_markerless_tombstones_at_exact_terminal(
                    Some(&marker),
                    terminal_offset,
                    Some(current.token),
                );
                self.changed.notify_all();
                return;
            }
        }

        let resolved_marker = {
            let mut resolved = self.lock_resolved();
            let found = consumed.iter().find(|candidate| {
                resolved
                    .iter()
                    .any(|(saved, _)| saved == candidate.as_str())
            });
            if let Some(marker) = found {
                if let Some((_, saved_offset)) = resolved
                    .iter_mut()
                    .find(|(saved, _)| saved == marker.as_str())
                {
                    *saved_offset = Some(terminal_offset);
                }
                true
            } else {
                false
            }
        };
        if resolved_marker || !consumed.is_empty() {
            // exact terminal 只能退役 consumed boundary 严格早于它的 tombstone，以及
            // 同 marker 本身。main parser 或一枚更早 terminal 不能证明 mirror 已跨过未来 B。
            let marker = consumed.first().map(String::as_str);
            self.retire_markerless_tombstones_at_exact_terminal(marker, terminal_offset, None);
        } else {
            if let Some(disposition) = self.take_markerless_disposition(terminal_offset) {
                if let MarkerlessTerminalDisposition::SuppressedByTombstone(token) = disposition {
                    let _ = self.consume_markerless_tombstone(token);
                }
                // Peer 已为这个 terminal 冻结归属；ReplyObserved 只做同一裁决的收尾，
                // 不得因中途 active 变化而污染后来 turn。
                self.changed.notify_all();
                return;
            }
            let belongs_to_pending_prompt = active.as_ref().is_some_and(|current| {
                matches!(current.phase, MirrorDeliveryPhase::AwaitingMarker)
                    || current.marker.as_ref().is_some_and(|marker| {
                        evidence
                            .enqueued()
                            .iter()
                            .any(|candidate| candidate == marker)
                    })
            });
            if let Some(token) = self.matching_markerless_tombstone(evidence, terminal_offset) {
                let _ = self.consume_markerless_tombstone(token);
            } else if belongs_to_pending_prompt {
                // seed cutoff/空正文观察到的是 pending prompt 之前的 terminal，不得把它记到 active。
            } else if let Some(current) = active.as_mut() {
                current.terminal_observed = true;
                current.terminal_observed_offset = Some(terminal_offset);
            }
        }
        self.changed.notify_all();
    }

    pub(super) fn mark_main_terminal_observed(&self, token: MirrorTurnToken) {
        let mut active = self.lock_active();
        if let Some(current) = active.as_mut().filter(|current| current.token == token) {
            current.terminal_observed = true;
            self.changed.notify_all();
        }
    }

    /// 主路径对读到的一段(一个 terminal)下了结论。记下读到的边界;`delivered` = 这一段的回复由
    /// 主路径投递(段终点回复已发出,或它就是终态),此时认领它消费到的本轮 steer marker,mirror
    /// 见到同一 marker 按 Resolved 让路、收尾时也不为它留孤儿凭证。主路径没投递的 steer 段
    /// (仅结果模式;接管轮里接管时已经落盘的重放段终点)marker 留在名单里,仍由 mirror 按孤儿补发。
    pub(super) fn settle_main_segment(
        &self,
        token: MirrorTurnToken,
        boundary: u64,
        consumed: &[String],
        delivered: bool,
    ) {
        let mut active = self.lock_active();
        if let Some(current) = active.as_mut().filter(|current| current.token == token) {
            current.main_read_through = Some(
                current
                    .main_read_through
                    .map_or(boundary, |seen| seen.max(boundary)),
            );
            if delivered {
                for marker in current.claim_steer_markers(consumed) {
                    self.remember_resolved(&marker, Some(boundary));
                }
            }
            self.changed.notify_all();
        }
    }

    /// run_turn 返回(正常收尾、打断、出错都算):主路径不会再读这一轮的转录。续跑区间的右端钉在
    /// 它下过结论的最后一个 terminal 上。
    pub(super) fn close_main_tail(&self, token: MirrorTurnToken) {
        let active = self.lock_active();
        let Some(current) = active.as_ref().filter(|current| current.token == token) else {
            return;
        };
        if let (Some(claim), Some(through)) = (
            self.lock_continuation_claims()
                .back_mut()
                .filter(|claim| claim.through.is_none()),
            current.main_read_through,
        ) {
            claim.through = Some(through.max(claim.after));
        }
        self.changed.notify_all();
    }

    /// 主路径决定跨后台任务续跑时调用一次:以 active turn 的 consumed_boundary(或 markerless
    /// fence)为左端开一个续跑区间,右端由 `close_main_tail` 钉住(见 `continuation_claims`)。
    pub(super) fn mark_continuation(&self, token: MirrorTurnToken) {
        let active = self.lock_active();
        let Some(current) = active.as_ref().filter(|current| current.token == token) else {
            // token 不匹配 = active turn 已收尾或被换代。claim 不落上,但必须留痕——
            // 静默落空会让续跑 terminal 走 Untracked 双发,且查不出原因。
            log(&summon_i18n::format!("r91e515a965"));
            return;
        };
        let after = current.consumed_boundary.or(current.markerless_fence);
        drop(active);
        // 两者皆 None = 无法证明区间左端。fail-closed 以 0 为左端压住整段(宁漏勿重);右端照常
        // 由 close_main_tail 钉住。正常路径走不到这:marker 轮有 consumed_boundary,fence 轮有 fence。
        if after.is_none() {
            log(summon_i18n::text!("r98ce0373bb"));
        }
        const MAX_CONTINUATION_CLAIMS: usize = 8;
        let mut claims = self.lock_continuation_claims();
        claims.retain(|claim| claim.through.is_some());
        claims.push_back(ContinuationClaim {
            after: after.unwrap_or(0),
            through: None,
        });
        while claims.len() > MAX_CONTINUATION_CLAIMS {
            claims.pop_front();
        }
        drop(claims);
        self.changed.notify_all();
    }

    fn lock_continuation_claims(&self) -> std::sync::MutexGuard<'_, VecDeque<ContinuationClaim>> {
        self.continuation_claims
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 测试用:把续跑区间里等主路径结论的上限换成 `wait`。
    #[cfg(test)]
    fn with_continuation_verdict_wait(wait: Duration) -> Self {
        Self {
            continuation_verdict_wait: Some(wait),
            ..Self::default()
        }
    }

    /// 续跑区间只裁决两类 terminal:没消费任何 marker 的(task-notification 唤醒的续跑段),和只消费
    /// 了本轮 steer marker 的(续跑等待期间追加的消息)。带主 marker 的走 exact 裁决,带别的 marker
    /// 的走历史 / 孤儿裁决。
    fn continuation_candidate(active: Option<&ActiveMirrorTurn>, consumed: &[String]) -> bool {
        consumed.is_empty()
            || active.is_some_and(|current| {
                consumed
                    .iter()
                    .all(|marker| current.steer_markers.contains(marker))
            })
    }

    /// mirror 对续跑区间里一个 terminal 的判断。落在右端已钉的区间 (after, through] 里:主路径
    /// 读过它、下过结论。落在右端未定的区间里:主路径已对它下过结论就同上;还没有就等主路径读到
    /// 或收尾再判 —— 它可能是续跑中的一段(主路径会处理),也可能落在主路径的终态之后(只有
    /// mirror 会投递),抢跑哪边都错。mirror 按偏移递增读,判过的 terminal 已越过的区间就此销账。
    fn await_continuation_owner<'a>(
        &'a self,
        mut active: std::sync::MutexGuard<'a, Option<ActiveMirrorTurn>>,
        terminal_offset: u64,
    ) -> (
        std::sync::MutexGuard<'a, Option<ActiveMirrorTurn>>,
        ContinuationVerdict,
    ) {
        let wait = self
            .continuation_verdict_wait
            .unwrap_or(CONTINUATION_VERDICT_WAIT);
        let deadline = std::time::Instant::now() + wait;
        loop {
            let claim = {
                let mut claims = self.lock_continuation_claims();
                claims.retain(|claim| {
                    claim
                        .through
                        .is_none_or(|through| through >= terminal_offset)
                });
                claims
                    .iter()
                    .copied()
                    .find(|claim| terminal_offset > claim.after)
            };
            let Some(claim) = claim else {
                return (active, ContinuationVerdict::Outside);
            };
            if let Some(through) = claim.through {
                let verdict = if terminal_offset <= through {
                    ContinuationVerdict::SettledByMain
                } else {
                    ContinuationVerdict::Outside
                };
                return (active, verdict);
            }
            // 右端未定的区间只会属于当前 active turn:收尾与换轮都会清掉它。
            let Some(current) = active.as_ref() else {
                return (active, ContinuationVerdict::Undecided);
            };
            if current
                .main_read_through
                .is_some_and(|seen| terminal_offset <= seen)
            {
                return (active, ContinuationVerdict::SettledByMain);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                log(&summon_i18n::format!("r7ff292816d", wait.as_secs()));
                return (active, ContinuationVerdict::Undecided);
            }
            active = self
                .changed
                .wait_timeout(active, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// 主 gate 每次（含 `!` rerun）认出 exact marker 时更新顺序边界。它与输出权裁决分开：
    /// rerun 已由主路径持有输出权，但迟到 markerless terminal 仍必须按最新 marker/boundary 判断。
    pub(super) fn mark_marker_consumed(&self, token: MirrorTurnToken, boundary: u64) {
        let mut active = self.lock_active();
        if let Some(current) = active.as_mut().filter(|current| current.token == token) {
            current.consumed_boundary = Some(boundary);
            self.changed.notify_all();
        }
    }

    /// 建立当前 Transcript turn 的 fencing token。调用方必须在 prompt 注入前调用。
    pub(super) fn begin_turn(&self) -> MirrorTurnToken {
        use std::sync::atomic::Ordering::Relaxed;

        let token = MirrorTurnToken(self.next_token.fetch_add(1, Relaxed).wrapping_add(1));
        let mut active = self.lock_active();
        if let Some(previous) = active.as_ref() {
            if matches!(
                previous.phase,
                MirrorDeliveryPhase::Delivered
                    | MirrorDeliveryPhase::DeliveryUncertain
                    | MirrorDeliveryPhase::MainClaimed
                    | MirrorDeliveryPhase::FailureClaimed
            ) {
                if let Some(marker) = previous.marker.as_deref() {
                    self.remember_resolved(marker, previous.delivery_offset);
                }
                if matches!(
                    previous.phase,
                    MirrorDeliveryPhase::MainClaimed | MirrorDeliveryPhase::FailureClaimed
                ) && !previous.terminal_observed
                {
                    self.remember_markerless_tombstone(previous);
                }
                // 上一轮无人认领的 steer marker:与 finish_turn 同一套前提落孤儿放行凭证
                // (RAII 正常路径走 finish_turn,这里兜 begin 先于迟到 finish 的换轮竞态)。
                self.remember_orphan_steer_markers(previous);
            }
        }
        *active = Some(ActiveMirrorTurn {
            token,
            marker: None,
            marker_bound_after_watcher_ready: false,
            delivery_marker: None,
            delivery_offset: None,
            consumed_boundary: None,
            terminal_observed: false,
            terminal_observed_offset: None,
            steer_markers: Vec::new(),
            markerless_fence: None,
            main_read_through: None,
            phase: MirrorDeliveryPhase::AwaitingMarker,
        });
        // 新轮开始:上一轮右端没钉上的续跑区间无从判断在哪结束,清掉;右端已钉的留着,mirror
        // 可能还没读到区间里的最后一段。
        self.lock_continuation_claims()
            .retain(|claim| claim.through.is_some());
        self.changed.notify_all();
        token
    }

    /// turn 类 slash(零 marker 注入)的显式归属栅栏。**必须在注入产生任何 send 副作用之前**
    /// 调用(与 marker 的 bind-before-send 同一条不变量):fence = 注入前冻结的转录 EOF,
    /// 严格在它右侧的 markerless terminal 归主路径,左侧的仍是前序 attach 轮。注入随后失败
    /// (闸拒/Stuck)也无害:未 claim 的 fence 轮 finish 时不留 tombstone,mirror 语义回到旧样。
    pub(super) fn bind_markerless_fence(&self, token: MirrorTurnToken, fence: u64) -> bool {
        let mut active = self.lock_active();
        let Some(current) = active.as_mut().filter(|current| current.token == token) else {
            return false;
        };
        current.markerless_fence = Some(fence);
        self.changed.notify_all();
        true
    }

    /// prompt 注入成功后立刻登记 `inject_with_marker` 返回的 exact marker。mirror 自己从 consumed
    /// user/queued_command 到 terminal 建立关联；主 gate 即使漏认同一条记录，也不影响这条独立凭证。
    pub(super) fn bind_transport_marker(
        &self,
        token: MirrorTurnToken,
        marker: impl Into<String>,
    ) -> bool {
        let watcher_ready = *self.lock_watcher() == MirrorWatcherState::Ready;
        let mut active = self.lock_active();
        let Some(current) = active.as_mut().filter(|current| current.token == token) else {
            return false;
        };
        let marker = marker.into();
        if current.marker.as_deref() != Some(marker.as_str()) {
            // `!` rerun 复用同一 tracker token，但它是新的 prompt 顺序区间。旧 marker 的
            // consumed/terminal/delivery 边界都不能泄漏到新轮；新边界会在 Echoed 后另行登记。
            // markerless fence 同理:fence 轮被 `!` rerun 升级成 marker 轮后,归属改由 exact
            // marker 判据接管,旧 fence 留着会把 rerun 前的迟到 terminal 错归给主路径。
            current.consumed_boundary = None;
            current.delivery_marker = None;
            current.delivery_offset = None;
            current.terminal_observed = false;
            current.terminal_observed_offset = None;
            current.markerless_fence = None;
        }
        current.marker = Some(marker.clone());
        current.marker_bound_after_watcher_ready = watcher_ready;
        match current.phase {
            MirrorDeliveryPhase::AwaitingMarker => {
                current.phase = MirrorDeliveryPhase::Eligible;
                true
            }
            MirrorDeliveryPhase::Eligible => true,
            // mirror 极快时可能先取得另一个 marker 的候选 permit；登记本轮 marker 仍然成功，
            // 候选是否能成为本轮凭证留给 claim/begin_delivery 的 exact 比较裁决。
            MirrorDeliveryPhase::Sending
            | MirrorDeliveryPhase::Delivered
            | MirrorDeliveryPhase::DeliveryUncertain => true,
            MirrorDeliveryPhase::MainClaimed => {
                self.remember_resolved(&marker, None);
                true
            }
            MirrorDeliveryPhase::FailureClaimed => {
                self.remember_resolved(&marker, None);
                false
            }
        }
    }

    /// `downtime_catchup`:这条 terminal 收尾于上一代 daemon 死后(终点在 stop fence 的
    /// `stop_eof` 右侧)。它只放宽下面**一条**判据 —— 见 `HistoricalUnknown` 那段。
    fn begin_delivery(
        &self,
        evidence: &crate::capture::MirrorMarkerEvidence,
        terminal_offset: u64,
        downtime_catchup: bool,
    ) -> MirrorDeliveryStart {
        // active→resolved 是 tracker 的统一锁序。必须先拿 active fence 再读 tombstone：若先读
        // resolved=false、随后主路径 claim+finish(A)，迟到 mirror 会拿旧判断落成 Untracked 并
        // 重发 A。持有 active 到裁决结束，使「写 tombstone+清 active」与本次读取线性化。
        let mut active = self.lock_active();
        let consumed = evidence.consumed();
        // 续跑区间里的 terminal 先等主路径的结论,必须先于 steer 认领与孤儿放行裁决。没消费
        // marker 的段归主路径(它发段终点回复或终态),mirror 让路。只消费了本轮 steer marker
        // 的段:主路径投递了的已认领(下面按 Resolved 让路),没投递的 marker 还在名单里,照常走
        // 孤儿放行。判不出时一律让路。
        if Self::continuation_candidate(active.as_ref(), consumed) {
            let (guard, verdict) = self.await_continuation_owner(active, terminal_offset);
            active = guard;
            match verdict {
                ContinuationVerdict::Undecided => {
                    return MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim);
                }
                ContinuationVerdict::SettledByMain if consumed.is_empty() => {
                    return MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim);
                }
                ContinuationVerdict::SettledByMain | ContinuationVerdict::Outside => {}
            }
        }
        let has_resolved_candidate = {
            let resolved = self.lock_resolved();
            consumed
                .iter()
                .any(|marker| resolved.iter().any(|(saved, _)| saved == marker))
        };
        let exact_candidate = active.as_ref().and_then(|current| {
            current.marker.as_deref().and_then(|expected| {
                consumed
                    .iter()
                    .find(|candidate| candidate.as_str() == expected)
                    .cloned()
            })
        });
        // 修 3:凡出现在消费证据里的本轮 steer marker,当场认领。认领是孤儿放行前提之二
        // (「从未出现在任何已消费 terminal 的 evidence 里」)的实现:被认领过的不再进孤儿凭证。
        // N:1 并轮 terminal 同时带主/steer 消费记录时,exact_candidate 仍按主 marker 走原有
        // phase 裁决(MainClaimed → 让路),行为不变。
        let steer_claimed = active
            .as_mut()
            .map(|current| current.claim_steer_markers(consumed))
            .unwrap_or_default();
        if exact_candidate.is_none() && has_resolved_candidate {
            let marker = consumed.first().map(String::as_str);
            self.retire_markerless_tombstones_at_exact_terminal(marker, terminal_offset, None);
            return MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Resolved);
        }
        if exact_candidate.is_none() && !steer_claimed.is_empty() {
            // terminal 只带活跃轮的 steer marker(不带主 marker):steer 自成了一轮。真实竞态:
            // 主路径发卡尚未返回、RAII guard 尚未 finish_turn,而 claude 已把孤儿轮写进转录 ——
            // 此时等 finish 不是保守,是永久漏发(认领已把 marker 移出名单,finish 不会再留
            // 凭证)。孤儿放行的三条前提若**当场全部证成**,就与 finish 后走凭证放行完全同构:
            // 1) 主路径已对用户负责(MainClaimed/Delivered);2) mirror 亲眼记下主 terminal
            // boundary(两条合并在 orphan_release_boundary);3) 这枚 terminal 严格在其右侧。
            // 先记 resolved 再放行(先销后发):send 失败宁漏,同证据 replay 落
            // Suppressed(Resolved),at-most-once 不破。
            if let Some(after_offset) = active
                .as_ref()
                .and_then(|current| current.orphan_release_boundary())
                .filter(|after| terminal_offset > *after)
            {
                for marker in &steer_claimed {
                    self.remember_resolved(marker, Some(terminal_offset));
                }
                self.retire_markerless_tombstones_at_exact_terminal(
                    steer_claimed.first().map(String::as_str),
                    terminal_offset,
                    None,
                );
                log(&summon_i18n::format!("r1ff1293f64"));
                return MirrorDeliveryStart::Untracked;
            }
            // 任一前提证不了 → fail-closed 让路,且**不**给 steer marker 发送 permit:
            // claim_main_delivery/claim_failure 的 exact 比较只认主 marker,steer permit 会被
            // 那两处看成「别的 marker」,主路径再发一次就是双发。宁可漏掉这声答案;认领记录
            // 同时保证 finish_turn 不再为它开孤儿口。
            let marker = consumed.first().map(String::as_str);
            self.retire_markerless_tombstones_at_exact_terminal(marker, terminal_offset, None);
            return MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::SteerClaimedByActiveTurn);
        }
        if exact_candidate.is_none() && !consumed.is_empty() {
            // 修 3:孤儿 steer 轮放行。两个前提在 finish_turn 当场证过(主路径 terminal 已消费
            // 且所有权已定;该 steer marker 从未出现在任何已消费 terminal 的证据里),这枚
            // terminal 是唯一没有任何进程会投递的答案 → 恰好发一次。凭证取出即销账、随手记
            // resolved(先销后发):send 失败宁漏,同证据 replay 落 Suppressed(Resolved),
            // at-most-once 不破。
            if let Some(steer_marker) = self.take_orphan_steer_marker(consumed, terminal_offset) {
                self.remember_resolved(&steer_marker, Some(terminal_offset));
                self.retire_markerless_tombstones_at_exact_terminal(
                    Some(steer_marker.as_str()),
                    terminal_offset,
                    None,
                );
                log(&summon_i18n::format!("r128801298f"));
                return MirrorDeliveryStart::Untracked;
            }
            // 本代 prompt 在任何 send 副作用前就同步 bind；因此 active 不认识的 Summon consumed
            // marker 只能来自旧 offset/lazy replay，绝不能按 Untracked 主动重发。
            //
            // **停机窗口是第三种来源**(owner 2026-08-13 实测):marker 由**上一代** daemon bind,
            // 那个进程已经死了、不可能投递;本代从没 bind 过它,也不会投递。于是「没有任何进程
            // 会发这一轮」——补发不重复。这个前提由 `stop_eof` 证明(右侧字节全部落盘于 daemon
            // 死后),不是猜的;调用链上任何一环证不了,`downtime_catchup` 就是 false,行为不变。
            let marker = consumed.first().map(String::as_str);
            self.retire_markerless_tombstones_at_exact_terminal(marker, terminal_offset, None);
            // 这一行是给下次排查用的。2026-08-13 定位这个 bug 花了整整一轮,就因为三种
            // 「Observed」在日志里长得一模一样,分不出是历史观察、歧义基线还是这一条。
            log(&summon_i18n::format!("r9b78643e8c"));
            if downtime_catchup {
                return MirrorDeliveryStart::Untracked;
            }
            return MirrorDeliveryStart::HistoricalUnknown;
        }
        if exact_candidate.is_none() {
            if let Some(disposition) = self.take_markerless_disposition(terminal_offset) {
                return match disposition {
                    MarkerlessTerminalDisposition::Untracked => MirrorDeliveryStart::Untracked,
                    MarkerlessTerminalDisposition::Suppressed(reason) => {
                        MirrorDeliveryStart::Suppressed(reason)
                    }
                    MarkerlessTerminalDisposition::SuppressedByTombstone(token) => {
                        let _ = self.consume_markerless_tombstone(token);
                        MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Tombstone)
                    }
                };
            }
            if let Some(token) = self.matching_markerless_tombstone(evidence, terminal_offset) {
                let _ = self.consume_markerless_tombstone(token);
                return MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Tombstone);
            }
        }
        let Some(current) = active.as_mut() else {
            return MirrorDeliveryStart::Untracked;
        };
        let Some(associated_marker) = exact_candidate.as_deref() else {
            // turn 类 slash 的显式 markerless fence:归属由**偏移比较**裁决,不走下面按 phase
            // 猜的启发式。fence 左侧(≤)的字节在注入前就已落盘 —— 前序 attach 轮,照旧转发;
            // 严格右侧的属于主路径自己消费的这条 markerless 轮,mirror 让路。
            if let Some(fence) = current.markerless_fence {
                if terminal_offset <= fence {
                    return MirrorDeliveryStart::Untracked;
                }
                if current.terminal_observed_offset == Some(terminal_offset) {
                    return MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::DuplicateTerminal);
                }
                current.terminal_observed = true;
                current.terminal_observed_offset = Some(terminal_offset);
                return MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::MarkerlessMainTurn);
            }
            let current_was_only_enqueued = current.marker.as_ref().is_some_and(|marker| {
                evidence
                    .enqueued()
                    .iter()
                    .any(|candidate| candidate == marker)
            });
            // Awaiting/Eligible 都证明当前 prompt 尚未在这枚 terminal 前被消费；exact enqueue 更强，
            // 即便主线程已抢先 MainClaimed，也能证明该 terminal 是前序 attach A。
            if current_was_only_enqueued
                || current.phase == MirrorDeliveryPhase::AwaitingMarker
                || (current.phase == MirrorDeliveryPhase::Eligible
                    && current.marker_bound_after_watcher_ready)
            {
                return MirrorDeliveryStart::Untracked;
            }
            if current.terminal_observed_offset == Some(terminal_offset) {
                return MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::DuplicateTerminal);
            }
            if current.terminal_observed {
                return MirrorDeliveryStart::Untracked;
            }
            current.terminal_observed = true;
            current.terminal_observed_offset = Some(terminal_offset);
            return MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::MarkerlessFailClosed);
        };
        current.terminal_observed = true;
        current.terminal_observed_offset = Some(terminal_offset);
        self.retire_markerless_tombstones_at_exact_terminal(
            Some(associated_marker),
            terminal_offset,
            Some(current.token),
        );
        match current.phase {
            MirrorDeliveryPhase::AwaitingMarker => {
                // bind-before-send 下不可达；若未来调用序改变，仍必须 fail-closed，不能把 backlog
                // marker 猜成本轮。
                MirrorDeliveryStart::HistoricalUnknown
            }
            MirrorDeliveryPhase::Eligible => {
                current.delivery_marker = Some(associated_marker.to_string());
                current.delivery_offset = Some(terminal_offset);
                current.phase = MirrorDeliveryPhase::Sending;
                MirrorDeliveryStart::Tracked(MirrorDeliveryPermit {
                    token: current.token,
                    marker: associated_marker.to_string(),
                    terminal_offset,
                })
            }
            // 先前成功/不确定的候选属于别的 marker，不妨碍当前 exact terminal 重新竞争。
            MirrorDeliveryPhase::Delivered | MirrorDeliveryPhase::DeliveryUncertain
                if current.delivery_marker.as_deref() != Some(associated_marker) =>
            {
                current.delivery_marker = Some(associated_marker.to_string());
                current.delivery_offset = Some(terminal_offset);
                current.phase = MirrorDeliveryPhase::Sending;
                MirrorDeliveryStart::Tracked(MirrorDeliveryPermit {
                    token: current.token,
                    marker: associated_marker.to_string(),
                    terminal_offset,
                })
            }
            MirrorDeliveryPhase::Sending => {
                MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::InFlight)
            }
            // 走到这里的 Delivered/DeliveryUncertain 必然是同 marker(不同 marker 已在上面重新
            // 竞争):同一轮答案的重复 terminal。
            MirrorDeliveryPhase::Delivered | MirrorDeliveryPhase::DeliveryUncertain => {
                MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::DuplicateTerminal)
            }
            MirrorDeliveryPhase::MainClaimed => {
                MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::MainClaimed)
            }
            MirrorDeliveryPhase::FailureClaimed => {
                MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::FailureClaimed)
            }
        }
    }

    fn notice_is_observation_only(
        &self,
        evidence: &crate::capture::MirrorMarkerEvidence,
        terminal_offset: u64,
    ) -> bool {
        let consumed = evidence.consumed();
        let active = self.lock_active();
        if !consumed.is_empty() {
            // Peer 只要携带 Summon consumed marker 就属于主路径或无法证明未送达的历史轮。
            // stop fence 不是 delivery 账本，跨重启 fresh marker 绝不能因 backlog 资格而放行。
            return true;
        }
        // 续跑区间内的通知卡归主路径的通知卡(同 begin_delivery 的区间裁决),mirror 不转发。
        // 区间在 disposition 缓存之前查:它落于续跑决定那一刻,早于任何续跑 terminal,不存在
        // 「先冻了 Untracked 再落区间」的竞态。
        let (mut active, verdict) = self.await_continuation_owner(active, terminal_offset);
        if verdict != ContinuationVerdict::Outside {
            return true;
        }

        if let Some(disposition) = self.cached_markerless_disposition(terminal_offset) {
            return disposition != MarkerlessTerminalDisposition::Untracked;
        }

        let disposition = if let Some(token) =
            self.matching_markerless_tombstone(evidence, terminal_offset)
        {
            MarkerlessTerminalDisposition::SuppressedByTombstone(token)
        } else if let Some(current) = active.as_mut() {
            // fence 轮的通知卡与 terminal 同一条规矩:偏移裁决,右侧归主路径(它的流式卡
            // 已经在播这段内容),左侧照旧转发。
            if let Some(fence) = current.markerless_fence {
                if terminal_offset <= fence {
                    MarkerlessTerminalDisposition::Untracked
                } else if current.terminal_observed_offset == Some(terminal_offset) {
                    MarkerlessTerminalDisposition::Suppressed(MirrorSuppressedBy::DuplicateTerminal)
                } else {
                    current.terminal_observed = true;
                    current.terminal_observed_offset = Some(terminal_offset);
                    MarkerlessTerminalDisposition::Suppressed(
                        MirrorSuppressedBy::MarkerlessMainTurn,
                    )
                }
            } else if current.terminal_observed_offset == Some(terminal_offset) {
                MarkerlessTerminalDisposition::Suppressed(MirrorSuppressedBy::DuplicateTerminal)
            } else {
                let current_was_only_enqueued = current.marker.as_ref().is_some_and(|marker| {
                    evidence
                        .enqueued()
                        .iter()
                        .any(|candidate| candidate == marker)
                });
                if current_was_only_enqueued
                    || current.phase == MirrorDeliveryPhase::AwaitingMarker
                    || (current.phase == MirrorDeliveryPhase::Eligible
                        && current.marker_bound_after_watcher_ready)
                    || current.terminal_observed
                {
                    MarkerlessTerminalDisposition::Untracked
                } else {
                    current.terminal_observed = true;
                    current.terminal_observed_offset = Some(terminal_offset);
                    MarkerlessTerminalDisposition::Suppressed(
                        MirrorSuppressedBy::MarkerlessFailClosed,
                    )
                }
            }
        } else {
            MarkerlessTerminalDisposition::Untracked
        };
        self.remember_markerless_disposition(terminal_offset, disposition);
        disposition != MarkerlessTerminalDisposition::Untracked
    }

    fn finish_delivery(&self, permit: MirrorDeliveryPermit, sent: bool) {
        let mut active = self.lock_active();
        let mut resolved = None;
        if let Some(current) = active.as_mut().filter(|current| {
            current.token == permit.token
                && current.delivery_marker.as_deref() == Some(permit.marker.as_str())
                && current.delivery_offset == Some(permit.terminal_offset)
        }) {
            if current.phase == MirrorDeliveryPhase::Sending {
                current.phase = if sent {
                    MirrorDeliveryPhase::Delivered
                } else {
                    MirrorDeliveryPhase::DeliveryUncertain
                };
                if sent {
                    resolved = Some((permit.marker.clone(), permit.terminal_offset));
                }
            }
        }
        if let Some((marker, terminal_offset)) = resolved {
            self.remember_resolved(&marker, Some(terminal_offset));
        }
        self.changed.notify_all();
    }

    /// 原 turn 失败时原子裁决由谁对用户负责。若镜像发送正在进行，等其有界 HTTP 调用返回后再判，
    /// 从而覆盖「主路径先检查/镜像后成功」与「镜像先发/主路径后检查」两种竞争顺序。
    pub(super) fn claim_failure(&self, token: MirrorTurnToken) -> MirrorFailureClaim {
        let mut active = self.lock_active();
        loop {
            let Some(current) = active.as_mut().filter(|current| current.token == token) else {
                return MirrorFailureClaim::SendFailureNotice;
            };
            let delivery_is_exact = current.marker.is_some()
                && current.marker.as_deref() == current.delivery_marker.as_deref();
            match current.phase {
                MirrorDeliveryPhase::Delivered | MirrorDeliveryPhase::DeliveryUncertain
                    if delivery_is_exact =>
                {
                    return MirrorFailureClaim::SuppressFailureNotice;
                }
                MirrorDeliveryPhase::Sending if delivery_is_exact => {
                    active = self
                        .changed
                        .wait(active)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                MirrorDeliveryPhase::AwaitingMarker
                | MirrorDeliveryPhase::Eligible
                | MirrorDeliveryPhase::Sending
                | MirrorDeliveryPhase::Delivered
                | MirrorDeliveryPhase::DeliveryUncertain
                | MirrorDeliveryPhase::MainClaimed
                | MirrorDeliveryPhase::FailureClaimed => {
                    current.phase = MirrorDeliveryPhase::FailureClaimed;
                    if let Some(marker) = current.marker.clone() {
                        self.remember_resolved(&marker, None);
                    }
                    return MirrorFailureClaim::SendFailureNotice;
                }
            }
        }
    }

    /// 主 gate 认出 exact marker 后抢本轮答案投递权。通常它早于 terminal，直接让 mirror 让路；
    /// 若 deadline rescue 时 mirror 已确认发送或结果不可证明，则抑制主路径，避免答案双发。
    pub(super) fn claim_main_delivery(&self, token: MirrorTurnToken) -> MainDeliveryClaim {
        let mut active = self.lock_active();
        loop {
            let Some(current) = active.as_mut().filter(|current| current.token == token) else {
                return MainDeliveryClaim::DeliverOnMainPath;
            };
            let delivery_is_exact = current.marker.is_some()
                && current.marker.as_deref() == current.delivery_marker.as_deref();
            match current.phase {
                MirrorDeliveryPhase::Delivered | MirrorDeliveryPhase::DeliveryUncertain
                    if delivery_is_exact =>
                {
                    return MainDeliveryClaim::SuppressMainDelivery;
                }
                MirrorDeliveryPhase::Sending if delivery_is_exact => {
                    active = self
                        .changed
                        .wait(active)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                MirrorDeliveryPhase::AwaitingMarker
                | MirrorDeliveryPhase::Eligible
                | MirrorDeliveryPhase::Sending
                | MirrorDeliveryPhase::Delivered
                | MirrorDeliveryPhase::DeliveryUncertain
                | MirrorDeliveryPhase::MainClaimed
                | MirrorDeliveryPhase::FailureClaimed => {
                    current.phase = MirrorDeliveryPhase::MainClaimed;
                    if let Some(marker) = current.marker.clone() {
                        self.remember_resolved(&marker, None);
                    }
                    return MainDeliveryClaim::DeliverOnMainPath;
                }
            }
        }
    }

    /// turn 所有终态统一清理。token fence 防旧 turn 的迟到清理抹掉下一轮状态。
    pub(super) fn finish_turn(&self, token: MirrorTurnToken) {
        let mut active = self.lock_active();
        if active
            .as_ref()
            .is_some_and(|current| current.token == token)
        {
            if let Some(current) = active.as_ref() {
                if matches!(
                    current.phase,
                    MirrorDeliveryPhase::Delivered
                        | MirrorDeliveryPhase::DeliveryUncertain
                        | MirrorDeliveryPhase::MainClaimed
                        | MirrorDeliveryPhase::FailureClaimed
                ) {
                    if let Some(marker) = current.marker.as_deref() {
                        self.remember_resolved(marker, current.delivery_offset);
                    }
                    // marker 有无的甄别在 remember_markerless_tombstone 内部:零 marker 的
                    // fence 轮同样要压迟到 terminal,两者都没有才是「CLI 根本没收到」不留。
                    if matches!(
                        current.phase,
                        MirrorDeliveryPhase::MainClaimed | MirrorDeliveryPhase::FailureClaimed
                    ) && !current.terminal_observed
                    {
                        self.remember_markerless_tombstone(current);
                    }
                }
                // 修 3:本轮收尾时仍无人认领的 steer marker → 孤儿放行凭证(前提校验在
                // `remember_orphan_steer_markers` 内,任一条证不了就不留,fail-closed)。
                self.remember_orphan_steer_markers(current);
            }
            *active = None;
            // 右端钉住的续跑区间留着:mirror 晚于收尾才读到的续跑段仍归主路径,区间之外的(续跑
            // 结束后终端里直接敲出的轮)不受影响。右端没钉上的(主路径没走到 close_main_tail)
            // 无从判断区间在哪结束,随轮收尾清掉 —— 留着会把之后几小时里终端直敲的轮都压掉。
            self.lock_continuation_claims()
                .retain(|claim| claim.through.is_some());
            self.changed.notify_all();
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MirrorDeliveryPermit {
    token: MirrorTurnToken,
    marker: String,
    terminal_offset: u64,
}

/// 网络发送跨 panic/Err 都必须把 `Sending` 收敛并唤醒 claim waiter。否则一次适配器 panic 会把
/// active 永久卡在 Condvar 等待，整个 chat 线程再也无法发失败回执。
struct MirrorDeliveryPermitGuard<'a> {
    tracker: &'a MirrorDeliveryTracker,
    permit: Option<MirrorDeliveryPermit>,
}

impl MirrorDeliveryPermitGuard<'_> {
    fn finish_sent(mut self) {
        if let Some(permit) = self.permit.take() {
            self.tracker.finish_delivery(permit, true);
        }
    }
}

impl Drop for MirrorDeliveryPermitGuard<'_> {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            self.tracker.finish_delivery(permit, false);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MirrorDeliveryStart {
    Tracked(MirrorDeliveryPermit),
    Untracked,
    Suppressed(MirrorSuppressedBy),
    /// 有 Summon consumed marker，但本代 tracker 无 active/resolved 凭证。它只能是 lazy replay
    /// 或崩溃窗口里的历史轮；主动出站优先不重复，结构上零网络。
    HistoricalUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MirrorSendOutcome {
    SentTracked,
    SentUntracked,
    Suppressed(MirrorSuppressedBy),
    Observed(MirrorObservedBy),
}

/// 发送和记账必须是同一个临界协议：先取得当前 turn permit，再执行 I/O，成功后才提交凭证。
/// 返回 Err 时已经把 tracked permit 记成 DeliveryUncertain，调用方只负责记录网络错误。
fn deliver_mirror_turn_once(
    tracker: &MirrorDeliveryTracker,
    evidence: &crate::capture::MirrorMarkerEvidence,
    terminal_offset: u64,
    downtime_catchup: bool,
    send: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<MirrorSendOutcome> {
    match tracker.begin_delivery(evidence, terminal_offset, downtime_catchup) {
        MirrorDeliveryStart::Suppressed(reason) => Ok(MirrorSendOutcome::Suppressed(reason)),
        MirrorDeliveryStart::HistoricalUnknown => Ok(MirrorSendOutcome::Observed(
            MirrorObservedBy::HistoricalUnknown,
        )),
        MirrorDeliveryStart::Untracked => {
            send()?;
            Ok(MirrorSendOutcome::SentUntracked)
        }
        MirrorDeliveryStart::Tracked(permit) => {
            let guard = MirrorDeliveryPermitGuard {
                tracker,
                permit: Some(permit),
            };
            send()?;
            guard.finish_sent();
            Ok(MirrorSendOutcome::SentTracked)
        }
    }
}

fn handle_mirror_reply_once(
    tracker: &MirrorDeliveryTracker,
    kind: crate::capture::MirrorTurnKind,
    evidence: &crate::capture::MirrorMarkerEvidence,
    terminal_offset: u64,
    send: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<MirrorSendOutcome> {
    match kind {
        // Ready 前冻结的历史 backlog / 歧义证据只推进 durable offset；不能消费/污染 on_ready 后
        // 才建立的 active/tombstone，更不能触发网络。它与当前 tracker 代际没有可证明的关联。
        crate::capture::MirrorTurnKind::HistoricalObserved => Ok(MirrorSendOutcome::Observed(
            MirrorObservedBy::HistoricalObserved,
        )),
        crate::capture::MirrorTurnKind::AmbiguousObserved => Ok(MirrorSendOutcome::Observed(
            MirrorObservedBy::AmbiguousObserved,
        )),
        crate::capture::MirrorTurnKind::ReplyObserved => {
            tracker.observe_consumed_terminal(evidence, terminal_offset);
            Ok(MirrorSendOutcome::Observed(MirrorObservedBy::ReplyObserved))
        }
        crate::capture::MirrorTurnKind::Reply => {
            deliver_mirror_turn_once(tracker, evidence, terminal_offset, false, send)
        }
        // 停机窗口补发:与 Reply 同一条投递路径,只在 tracker 里多放行一条判据。
        crate::capture::MirrorTurnKind::CatchupReply => {
            deliver_mirror_turn_once(tracker, evidence, terminal_offset, true, send)
        }
        crate::capture::MirrorTurnKind::Notice => {
            anyhow::bail!(summon_i18n::format!("r46af1b5916"))
        }
    }
}

fn handle_mirror_reply_with_preflight(
    tracker: &MirrorDeliveryTracker,
    kind: crate::capture::MirrorTurnKind,
    evidence: &crate::capture::MirrorMarkerEvidence,
    terminal_offset: u64,
    preflight: impl FnOnce() -> anyhow::Result<()>,
    send: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<MirrorSendOutcome> {
    if matches!(
        kind,
        crate::capture::MirrorTurnKind::Reply | crate::capture::MirrorTurnKind::CatchupReply
    ) {
        // 零网络身份校验必须在 delivery permit 之前。失败表示“确定未尝试发送”，主路径仍可
        // 正常回复；若放进 permit 后才 Err，会被误判 DeliveryUncertain 并把两条路径都压掉。
        preflight()?;
    }
    handle_mirror_reply_once(tracker, kind, evidence, terminal_offset, send)
}

/// 通知卡与 reply 共用 terminal marker 证据：主路径/历史 Summon turn 的通知卡只观察不联网
/// （那一轮由主路径自己实时刷卡）；markerless 或仅带下一 prompt enqueue 的 attach 轮才允许发送。
fn handle_mirror_notice_once(
    tracker: &MirrorDeliveryTracker,
    evidence: &crate::capture::MirrorMarkerEvidence,
    terminal_offset: u64,
    send: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<MirrorSendOutcome> {
    if !tracker.notice_is_observation_only(evidence, terminal_offset) {
        send()?;
        Ok(MirrorSendOutcome::SentUntracked)
    } else {
        if !evidence.consumed().is_empty() {
            tracker.observe_consumed_terminal(evidence, terminal_offset);
        }
        Ok(MirrorSendOutcome::Observed(
            MirrorObservedBy::NoticeObservation,
        ))
    }
}

fn handle_mirror_notice_with_preflight(
    tracker: &MirrorDeliveryTracker,
    evidence: &crate::capture::MirrorMarkerEvidence,
    terminal_offset: u64,
    preflight: impl FnOnce() -> anyhow::Result<()>,
    send: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<MirrorSendOutcome> {
    if tracker.notice_is_observation_only(evidence, terminal_offset) {
        return handle_mirror_notice_once(tracker, evidence, terminal_offset, send);
    }
    preflight()?;
    handle_mirror_notice_once(tracker, evidence, terminal_offset, send)
}

/// run_mirror 无论正常退出、Err 还是 send closure panic，都必须撤销 Ready；不 catch panic，
/// 只用 Drop 收敛 tracker 状态，让后续主路径不再信任已经死掉的 watcher。
struct MirrorWatcherExitGuard<'a> {
    tracker: &'a MirrorDeliveryTracker,
}

impl Drop for MirrorWatcherExitGuard<'_> {
    fn drop(&mut self) {
        self.tracker.mark_watcher_failed();
    }
}

/// 一个 terminal 内逐块递增的幂等序号。
///
/// **按 kind 各自计数**:序号进 uuid 的摘要,共用一个计数器的话,一块的身份就取决于同一轮里
/// **别的**块出现了几次 —— 那一轮多一条通知,回复的 uuid 就跟着变,而 uuid 存在的意义正是
/// 「同一块跨重启重解析算出同一个键」。
#[derive(Default)]
struct MirrorMessageUuidCursor {
    boundary: Option<u64>,
    boundary_checkpoint: Vec<u8>,
    next_ordinal: std::collections::HashMap<String, u32>,
}

impl MirrorMessageUuidCursor {
    fn next(
        &mut self,
        epoch_fingerprint: &str,
        chat_id: &str,
        sid: &str,
        terminal_boundary: u64,
        boundary_checkpoint: &[u8],
        kind: &str,
    ) -> String {
        if self.boundary != Some(terminal_boundary)
            || self.boundary_checkpoint != boundary_checkpoint
        {
            self.boundary = Some(terminal_boundary);
            self.boundary_checkpoint = boundary_checkpoint.to_vec();
            self.next_ordinal.clear();
        }
        let slot = self.next_ordinal.entry(kind.to_string()).or_default();
        let ordinal = *slot;
        *slot = slot.saturating_add(1);
        mirror_message_uuid(
            epoch_fingerprint,
            chat_id,
            sid,
            terminal_boundary,
            boundary_checkpoint,
            kind,
            ordinal,
        )
    }
}

/// 镜像发一块正文;被飞书内容审核拦下时改发一句说明顶替这一块(见 [`send_or_explain_rejection`])。
#[allow(clippy::too_many_arguments)] // 镜像块的身份、正文与文件按钮各自独立
fn send_mirror_block(
    fc: &FeishuClient,
    agent: &str,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    rendered: &summon_core::feishu::RenderedLarkMd,
    buttons: &[summon_core::feishu::PreviewButton],
    uuid: &str,
) -> anyhow::Result<()> {
    send_or_explain_rejection(
        agent,
        || {
            if buttons.is_empty() {
                return send_rendered_markdown_idempotent_archived(
                    fc, agent_dir, chat_type, chat_id, rendered, uuid,
                );
            }
            let card = summon_core::feishu::card_markdown_v2_rendered(rendered);
            let card =
                summon_core::feishu::card_with_preview_buttons(&card, buttons).unwrap_or(card);
            crate::frontend::outbound::send_card_idempotent_archived(
                fc, agent_dir, chat_type, chat_id, rendered, &card, uuid,
            )
        },
        |notice| {
            send_rendered_markdown_idempotent_archived(
                fc,
                agent_dir,
                chat_type,
                chat_id,
                notice,
                &rejected_notice_uuid(uuid),
            )
        },
    )
}

/// 飞书内容审核拦下的一块,同一正文重发结果都一样。改发一句说明顶替它:说明发出即视为这一块
/// 已投递,offset 照常推进;说明也发不出去才按发送失败处理。
fn send_or_explain_rejection(
    agent: &str,
    send: impl FnOnce() -> anyhow::Result<()>,
    explain: impl FnOnce(&summon_core::feishu::RenderedLarkMd) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Err(error) = send() else {
        return Ok(());
    };
    let Some(rejected) = error.downcast_ref::<crate::frontend::client::ContentRejected>() else {
        return Err(error);
    };
    log(&summon_i18n::format!("r0f0183f26c"));
    let notice = summon_core::feishu::render_external_lark_md(
        &summon_core::feishu::content_rejected_notice(&rejected.reason),
        &[],
        &std::collections::HashMap::new(),
    );
    explain(&notice).map_err(|explain_error| error.context(explain_error))
}

/// 顶替说明的幂等键:由原块的键派生,同一段跨重启重解析仍是同一个键;与原块不同,不会被飞书
/// 当成那次被拒的请求。
fn rejected_notice_uuid(uuid: &str) -> String {
    format!("{uuid}-r")
}

fn mirror_message_uuid(
    epoch_fingerprint: &str,
    chat_id: &str,
    sid: &str,
    terminal_boundary: u64,
    boundary_checkpoint: &[u8],
    kind: &str,
    ordinal: u32,
) -> String {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut feed = |label: &[u8], value: &[u8]| {
        digest.update(label);
        digest.update(&(value.len() as u64).to_le_bytes());
        digest.update(value);
    };
    feed(b"epoch", epoch_fingerprint.as_bytes());
    feed(b"chat", chat_id.as_bytes());
    feed(b"sid", sid.as_bytes());
    feed(b"boundary", &terminal_boundary.to_le_bytes());
    feed(b"checkpoint", boundary_checkpoint);
    feed(b"kind", kind.as_bytes());
    feed(b"ordinal", &ordinal.to_le_bytes());
    let hash = digest.finish();
    let mut encoded = String::with_capacity(32);
    for byte in &hash.as_ref()[..16] {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    format!("summon-mirror-{encoded}")
}

fn ensure_mirror_epoch_current(
    agent: &Agent,
    frozen_epoch: &FeishuEpochIdentity,
) -> anyhow::Result<()> {
    let fresh = agent
        .reread()
        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r49f7ca6266")))?;
    anyhow::ensure!(
        fresh.is_routable() && frozen_epoch.matches_current(&fresh),
        summon_i18n::format!("r05fbf62aee")
    );
    Ok(())
}

/// 会话镜像的出站 @mention 渲染。**与 `FeishuFrontend::merged_peers` 同源**:per-chat peers
/// 优先(`/introduce` 正式认识的),身份缓存作回退(见过但没 introduce 的)。
///
/// **为什么非有不可**(2026-08-12 owner 实测的静默断链):镜像路径此前把裸文本直接交给
/// `card_markdown`,`@炼金师` 于是以纯文字发出。飞书据此认定这条消息没 @ 任何人,**被 @ 的 bot
/// 的 app 根本收不到消息事件** —— 不是被哪道闸拒了,是压根没投递,日志里连一行拒绝都不会有。
/// 同群对照坐实:炼金师→Arcadia 走卡片路径(`render_mentions_in_card` 渲染过)通,
/// Arcadia→炼金师 走镜像路径没渲染,不通;两边 peers 都是好的,`/introduce` 也都做过。
///
/// 这跟同仓那个「新路径抄了老路径的位置、没抄它的必要处理」是同一类错误。所以这里刻意与卡片
/// 路径共用同一组数据来源,而不是另起一套。
fn render_mirror_outbound(
    agent: &Agent,
    tag: &str,
    preview: Option<&crate::runtime::preview_server::PreviewServer>,
    base: Option<&str>,
    text: &str,
) -> (
    summon_core::feishu::RenderedLarkMd,
    Vec<summon_core::feishu::PreviewButton>,
) {
    let scope = super::agent_env::feishu_storage_scope(agent);
    let mut peers =
        crate::runtime::peers_store::PeersStore::at_run_dir(&scope.key, scope.allow_legacy_import)
            .load(tag)
            .map(|pf| pf.peers)
            .unwrap_or_default();
    let existing: std::collections::HashSet<String> =
        peers.iter().map(|p| p.name.clone()).collect();
    let cache = crate::runtime::identity_cache::IdentityCacheStore::at_run_dir(
        &scope.key,
        scope.allow_legacy_import,
    )
    .load();
    for (open_id, entry) in &cache.entries {
        if !existing.contains(&entry.name) {
            peers.push(summon_core::peers::Peer {
                open_id: open_id.clone(),
                name: entry.name.clone(),
                ..Default::default()
            });
        }
    }
    let alias_map = super::agent_env::build_alias_name_map();
    // 与 `FeishuFrontend` 同序:**先**找本地路径做成文件按钮,**再**转义 + 渲 mention。
    // 反过来的话看到的是已插零宽空格的文本,认不出路径。镜像与 turn 出站是两条独立的
    // 渲染路径,同一份约定得在两边各写一次 —— 少写一边的后果是这条路上的文件没有打开入口。
    let (text, buttons) = match preview {
        Some(previews) => {
            let (text, buttons) = previews.link_buttons(text, base);
            (std::borrow::Cow::Owned(text), buttons)
        }
        None => (std::borrow::Cow::Borrowed(text), Vec::new()),
    };
    (
        summon_core::feishu::render_external_lark_md(&text, &peers, &alias_map),
        buttons,
    )
}

/// summon 自己处理的指令(`/help`、`/session`、`/model`、拒绝与用法提示……)回执的 thread 锚点:
/// 群里一律挂进**那条指令消息**的 thread —— 指令还在群主干时这会新开一条 thread,正是要的效果
/// (owner 2026-09-02:指令回执进 thread,群主干只留对话)。私聊没有 thread,返回 None 照常发。
/// 只确认照做了的回执不开 thread,改在这条消息上贴完成表情(见 [`CommandAck`])。
///
/// 判据与 chat 线程侧的 `command_anchor_for` **必须一致**:两处一旦漂移,同一条指令的回执会
/// 一半在 thread、一半在群里。普通轮的回复另有一套(`thread_anchor_for`):那边不许为一句回答
/// 切出话题串。
///
/// 为什么锚的是 message_id 而不是 thread_id:飞书是「回复某条消息」把新消息带进那条消息所在
/// 的 thread,没有「直接投递到某 thread」的接口。
pub(super) fn command_anchor(msg: &Inbound) -> Option<&str> {
    if !summon_core::authz::is_group_chat(msg.chat_type.as_deref().unwrap_or("p2p")) {
        return None;
    }
    feishu_reply_id(msg)
}

fn deliver_session_panel(
    fc: &FeishuClient,
    agent: &Agent,
    msg: &Inbound,
    chat_type: &str,
    card: &str,
) -> anyhow::Result<String> {
    send_card_archived_id(
        fc,
        &agent.state_dir(),
        chat_type,
        &msg.chat_id,
        command_anchor(msg),
        "session_panel",
        card,
    )
    .inspect_err(|_| {
        let notice = summon_i18n::text!("r4673bc6148");
        if let Err(error) = send_markdown_archived_anchored(
            fc,
            &agent.state_dir(),
            chat_type,
            &msg.chat_id,
            command_anchor(msg),
            notice,
        ) {
            log(&summon_i18n::format!("rfce6a02b2c", agent.name));
        }
    })
}

/// `/quota` 的回执。显式 `on` / `off` 照做了就是纯确认;裸 `/quota` 是翻转,回执里的开关状态就是
/// 结果,照常发文字。
fn quota_ack(explicit: bool, on: bool, written: std::io::Result<()>) -> CommandAck {
    match written {
        Ok(()) => {
            let text = if on {
                summon_i18n::text!("rcc42beb17f")
            } else {
                summon_i18n::text!("reb6210e817")
            };
            if explicit {
                CommandAck::Done(text.to_string())
            } else {
                CommandAck::Note(text.to_string())
            }
        }
        Err(e) => CommandAck::Note(summon_i18n::format!("r8c765b78ac")),
    }
}

/// `/fast` 照做了的回执。开启会多耗额度,这句提示要让人看到;关掉是纯确认。
fn fast_ack(on: bool, on_text: &str, off_text: &str) -> CommandAck {
    if on {
        CommandAck::Note(on_text.to_string())
    } else {
        CommandAck::Done(off_text.to_string())
    }
}

/// `/introduce` / `/unintroduce` 的回执。认识关系落盘了才算照做:peers 文件既进冷启 PRIMER,
/// 也是对方 bot 消息的放行名单,没存下来就得让人知道。
fn peers_ack(done: String, stored: anyhow::Result<()>) -> CommandAck {
    match stored {
        Ok(()) => CommandAck::Done(done),
        Err(e) => CommandAck::Note(summon_i18n::format!("rc1654584fa")),
    }
}

/// 回复一条入站消息时该引用哪个飞书 id。与 chat 线程的 `feishu_anchor` 同一条规矩:恢复重放的
/// `message_id` 是本地幂等键,回复只能落在原消息上。
fn feishu_reply_id(msg: &Inbound) -> Option<&str> {
    msg.anchor_message_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .or_else(|| Some(msg.message_id.as_str()).filter(|id| !id.is_empty()))
}

/// 陈旧提示挂在被丢的那条消息上。群里照 summon 回执的惯例进它的 thread;私聊没有 thread,
/// 用不开 thread 的引用回复指明是哪条 —— 被丢的消息至少晚到一个时效门槛,中间往往已有别的来回。
fn stale_notice_reply_target(msg: &Inbound) -> Option<(&str, bool)> {
    let is_group = summon_core::authz::is_group_chat(msg.chat_type.as_deref().unwrap_or("p2p"));
    feishu_reply_id(msg).map(|id| (id, is_group))
}

/// observation-only 日志的受众标记:consumed marker 非空 ⟹ 入站消息触发的 live turn(用户在
/// 飞书等回复);空 ⟹ 无消费证据(markerless 终端 attach 为主——first-start 从 EOF 起读漏看
/// user 行、uncertain-only 是例外,归属看 reason 字段)。排查「用户没收到回复」时按 live turn 优先。
fn observation_audience(evidence: &crate::capture::MirrorMarkerEvidence) -> &'static str {
    if evidence.consumed().is_empty() {
        "markerless"
    } else {
        "live turn"
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_mirror(
    agent: Agent,
    tmux_name: String,
    poll: u64,
    debounce: u64,
    fc: Arc<FeishuClient>,
    frozen_epoch: FeishuEpochIdentity,
    chat_id: String,
    chat_type: String,
    // 只承载 cold seed 的连续前缀 cutoff；normal turn 归属由 typed marker evidence 仲裁。
    mirror_watermark: Arc<std::sync::atomic::AtomicU64>,
    // 与 chat 线程共享 exact turn 送达仲裁；只认 marker 消费后的成功发送，不认时间窗计数。
    mirror_delivery: Arc<MirrorDeliveryTracker>,
    // 回复里本地文件的按钮由预览服务签发。turn 出站经 `FeishuFrontend` 拿到它，镜像这条路得自己带。
    preview: Option<Arc<crate::runtime::preview_server::PreviewServer>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    // 停机窗口补发下界:只有本代 boot 用上一代 stop fence 批准过的 chat、且是 chat 线程本代第一次
    // 起来时起的 watcher 才有值(见 `plan_mirror_catchup` 的闸与 `take_mirror_catchup_floor`)。
    catchup_floor: Option<u64>,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let _watcher_exit = MirrorWatcherExitGuard {
        tracker: &mirror_delivery,
    };
    let sess = Session::with_name(&agent, tmux_name, poll, debounce)
        .with_mirror_watermark(mirror_watermark)
        .with_chat_id(&chat_id);
    // PF4:sid 未就位的等待也退避。3s 内拿不到 SID 就先把主路径放行为 Bypassed，但 watcher
    // 继续低频等待，SID 后到仍可建立未来轮次的 fence；镜像启动不能永久卡死该 chat。
    let mut wait = std::time::Duration::from_millis(500);
    let sid_fence_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut sid_fence_bypassed = false;
    while !stop.load(Relaxed) {
        if sess.native_session_id().is_some() {
            break;
        }
        if !sid_fence_bypassed && std::time::Instant::now() >= sid_fence_deadline {
            mirror_delivery.mark_watcher_bypassed();
            sid_fence_bypassed = true;
            log(&summon_i18n::format!("r25c45f6413", agent.name));
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(std::time::Duration::from_secs(5));
    }
    if stop.load(Relaxed) {
        return;
    }
    let Some(mirror_sid) = sess.native_session_id() else {
        return;
    };
    let epoch_fingerprint = frozen_epoch.fingerprint();
    let mut uuid_cursor = MirrorMessageUuidCursor::default();
    let mut on_turn = |kind: crate::capture::MirrorTurnKind,
                       evidence: &crate::capture::MirrorMarkerEvidence,
                       terminal_offset: u64,
                       boundary_checkpoint: &[u8],
                       text: &str| {
        if kind == crate::capture::MirrorTurnKind::Notice {
            return match handle_mirror_notice_with_preflight(
                &mirror_delivery,
                evidence,
                terminal_offset,
                || ensure_mirror_epoch_current(&agent, &frozen_epoch),
                || {
                    let uuid = uuid_cursor.next(
                        &epoch_fingerprint,
                        &chat_id,
                        &mirror_sid,
                        terminal_offset,
                        boundary_checkpoint,
                        // 幂等 uuid 的命名空间。它和序号共同决定这一块的键,两者都是线上事实:
                        // 换掉任何一个,重启后重解析这一段就会按新键当成没发过、再发一条。
                        "notice",
                    );
                    // 与 reply 块同一条规矩:`@名字` 要渲染成真实 `<at>`,否则镜像回群的 @
                    // 谁也触发不了。正文里的标签形态已在渲染这一块时转义过,不会被当成真标签。
                    let (rendered, buttons) = render_mirror_outbound(
                        &agent,
                        &chat_tag(&chat_type, &chat_id),
                        preview.as_deref(),
                        agent.working_dir().as_deref(),
                        text,
                    );
                    send_mirror_block(
                        &fc,
                        &agent.name,
                        &agent.state_dir(),
                        &chat_type,
                        &chat_id,
                        &rendered,
                        &buttons,
                        &uuid,
                    )
                },
            ) {
                Ok(MirrorSendOutcome::SentUntracked) => {
                    log(&summon_i18n::format!("r80ba0b824a", agent.name));
                    Ok(())
                }
                Ok(MirrorSendOutcome::Observed(reason)) => {
                    log(&summon_i18n::format!("r3d19c42e82", agent.name));
                    Ok(())
                }
                Ok(MirrorSendOutcome::Suppressed(reason)) => {
                    log(&summon_i18n::format!("ra177fdd330", agent.name));
                    Ok(())
                }
                Ok(MirrorSendOutcome::SentTracked) => {
                    anyhow::bail!(summon_i18n::format!("r5befb55275"))
                }
                Err(error) => {
                    log(&summon_i18n::format!("rc83c814cf6", agent.name));
                    Err(error)
                }
            };
        }
        match handle_mirror_reply_with_preflight(
            &mirror_delivery,
            kind,
            evidence,
            terminal_offset,
            || ensure_mirror_epoch_current(&agent, &frozen_epoch),
            || {
                let uuid = uuid_cursor.next(
                    &epoch_fingerprint,
                    &chat_id,
                    &mirror_sid,
                    terminal_offset,
                    boundary_checkpoint,
                    "reply",
                );
                // 必须走**已渲染**版:裸文本会让 `@某bot` 以纯文字发出,飞书据此认为没 @ 任何人,
                // 被 @ 的 bot 收不到消息事件(见 `render_mirror_outbound`)。
                let (rendered, buttons) = render_mirror_outbound(
                    &agent,
                    &chat_tag(&chat_type, &chat_id),
                    preview.as_deref(),
                    agent.working_dir().as_deref(),
                    text,
                );
                send_mirror_block(
                    &fc,
                    &agent.name,
                    &agent.state_dir(),
                    &chat_type,
                    &chat_id,
                    &rendered,
                    &buttons,
                    &uuid,
                )
            },
        ) {
            Ok(MirrorSendOutcome::SentTracked) => {
                log(&summon_i18n::format!("r22f22ad625", agent.name));
                Ok(())
            }
            Ok(MirrorSendOutcome::SentUntracked) => {
                log(&summon_i18n::format!("rabe1c0a676", agent.name));
                Ok(())
            }
            // 原因必须照实打印:这行日志曾硬编码「已由失败回执取得发送所有权」,而 7 条
            // Suppressed 路径里绝大多数样本其实是 MainClaimed,误标让排查整整偏航一轮(2026-08-13)。
            Ok(MirrorSendOutcome::Suppressed(reason)) => {
                log(&summon_i18n::format!("r669e439b69", agent.name));
                Ok(())
            }
            Ok(MirrorSendOutcome::Observed(reason)) => {
                let audience = observation_audience(evidence);
                log(&summon_i18n::format!("rb2d2ab057d", agent.name));
                Ok(())
            }
            Err(error) => {
                log(&summon_i18n::format!("rcd32d9adea", agent.name));
                Err(error)
            }
        }
    };
    let mut on_ready = || mirror_delivery.mark_watcher_ready();
    let mut on_start_bypassed = || mirror_delivery.mark_watcher_bypassed();
    // 没有补发下界 → 历史一律只观察。
    if let Some(floor) = catchup_floor {
        log(&summon_i18n::format!("rfc3996cd4a", agent.name));
    }
    let result = crate::capture::Transcript::new(&sess, crate::runtime::executor::LocalExecutor)
        .mirror_tail(
            &epoch_fingerprint,
            &stop,
            &mut on_turn,
            &mut on_ready,
            &mut on_start_bypassed,
            // 持久化消费 offset 到 threads/<tag>/mirror_offset,daemon 重启后从上次位置续读
            // (补回停机期间 tmux 里完成的 turn)。
            Some(
                &agent
                    .state_dir()
                    .join("threads")
                    .join(chat_tag(&chat_type, &chat_id))
                    .join("mirror_offset"),
            ),
            // 停机窗口补发下界:只有本代 boot 用上一代 stop fence 批准过的 chat 才有值。
            // 未获批 → None → 历史一律只观察,与本次改动前逐字节一致。
            catchup_floor,
        );
    if let Err(e) = result {
        log(&summon_i18n::format!("rb4bcc9ef28", agent.name));
    }
}

/// 启动恢复(slice 3):把上次崩溃/重启时本 chat 留下的**孤儿 working 表情**收尾——撤 OnIt + 贴 DONE +
/// 清队列,避免 OnIt 永远卡在那条飞书消息上。chat 线程起跑时调(有 fc/agent/chat 定位)。best-effort。
///
/// 盘上锚点认领的那几条 id **不是孤儿**:它们的轮此刻还在 tmux 里跑,接管任务经任务通道晚于
/// 本扫描到达。把它们一起收了,用户看到的是「👀 刚变成 ✅,然后回答才慢慢出来」——轮还没跑完
/// 就先谎报了完成。跳过它们,轮末的撤/贴由那一轮自己按既有记录收口。
pub(super) fn recover_orphan_reactions(
    agent: &Agent,
    fc: &FeishuClient,
    chat_id: &str,
    chat_type: &str,
) {
    let store = crate::runtime::reaction_store::ReactionStore::for_agent(&agent.state_dir());
    let done = resolve_messaging(agent).done_reaction;
    let running = crate::runtime::inflight::read_turn_anchor(
        &agent.state_dir(),
        &chat_tag(chat_type, chat_id),
    )
    .map(|anchor| anchor.inbox_message_ids)
    .unwrap_or_default();
    for r in store
        .list()
        .into_iter()
        .filter(|r| r.chat_id == chat_id && !running.contains(&r.message_id))
    {
        if !r.onit_reaction_id.is_empty() {
            let _ = fc.unreact(r.anchor(), &r.onit_reaction_id);
        }
        if let Err(e) = fc.react(r.anchor(), &done) {
            log(&summon_i18n::format!(
                "rb55a2c1950",
                agent.name,
                r.message_id
            ));
        }
        store.remove(&r.message_id);
        log(&summon_i18n::format!(
            "r2dd8550086",
            agent.name,
            r.message_id
        ));
    }
}

/// chat worker 退出通知的 owned 形态。send failure 重建路径会先从 dispatcher channel 中
/// 提取旧 worker 已经提交的通知，作为「旧代已收口」屏障；正常接收路径复用同一个收口函数。
struct ChatExitEvent {
    chat_id: String,
    chat_type: String,
    reason: summon_core::exit::ExitReason,
    creator: Option<String>,
    notify: bool,
    session_created_at: Option<String>,
}

/// 一条 Task 的交付结果。`RetainedPending` 明确表示两次 send 都未把消息交给 receiver（或重建
/// 被占用 gate 拒绝）；调用方只能保留 WAL，绝不能把它终态化。
enum TaskDelivery {
    Current,
    Replacement(ChatHandle),
    RetainedPending,
}

fn disabled_notice_allowed(agent: &Agent, sender_is_peer: bool, inbound: &Inbound) -> bool {
    if agent.is_steward() {
        agent
            .feishu
            .as_ref()
            .is_some_and(|feishu| !feishu.allowed_users.is_empty())
            && agent.allows(&inbound.sender, &inbound.sender_union_id)
    } else {
        sender_can_talk(
            agent,
            sender_is_peer,
            &inbound.sender,
            &inbound.sender_union_id,
        )
    }
}

fn admit_agent_inbound(
    wal: &crate::runtime::inbox_wal::InboxWal,
    agent: &str,
    delivery_scope: &str,
    inbound: &Inbound,
    may_reply: bool,
) -> anyhow::Result<bool> {
    if wal.agent_enabled(agent)? {
        return Ok(true);
    }
    if !may_reply {
        return Ok(false);
    }
    wal.append(agent, delivery_scope, inbound)?;
    Ok(!wal.reject_disabled(agent, delivery_scope, &inbound.message_id)?)
}

/// 这条入站要不要先落 WAL。
///
/// 接管入站(见 [`Inbound::reattach`])的那条记录**已经在盘上**,而且 `started=true` 正是接管
/// 的立身之本 —— 上一代注入过它。再 append 一次是整条重写,两处都写坏:
/// ① `reattach` 被写进记录,接管失败退恢复卡后用户点「恢复」投出的仍是接管形态,合成 replay_id
///    查锚点恒判假,于是又一张恢复卡 —— 死循环;
/// ② `started` 被重置回 false,这一轮没跑完时下次冷启把它当未开跑重注入,同一句话跑两遍。
fn inbound_needs_wal_append(inbound: &Inbound) -> bool {
    !inbound.reattach
}

/// 把所有“WAL 成功后才允许发生”的易失副作用放在同一个结构边界后。append 失败时闭包绝不
/// 运行；dispatcher 因而既不记 seen，也不向 chat worker 投递，飞书重投/用户重发仍可重试。
///
/// 判据收在这里而不是调用点:一条 WAL 记录终态唯一,而「这条要不要落盘」是这个边界的语义
/// 本身,散出去就会有第二个调用点漏判。
fn after_durable_inbox<T>(
    wal: &crate::runtime::inbox_wal::InboxWal,
    agent: &str,
    delivery_scope: &str,
    inbound: &Inbound,
    on_durable: impl FnOnce() -> T,
) -> anyhow::Result<T> {
    if inbound_needs_wal_append(inbound) {
        wal.append(agent, delivery_scope, inbound)?;
    }
    Ok(on_durable())
}

/// 把 `SendError` 中原样退回的 Task 投给至多一个 replacement。重建本身由闭包负责，便于调用方
/// 在 spawn 前完成旧代退出屏障与 Bridge 二次 gate；本函数不接触 WAL，从结构上避免「没交付却
/// mark_consumed」。
fn deliver_task_with_one_replacement(
    current: &Sender<ThreadMsg>,
    task: Box<ChatTask>,
    replacement: impl FnOnce() -> Option<ChatHandle>,
) -> TaskDelivery {
    let task = match current.send(ThreadMsg::Task(task)) {
        Ok(()) => return TaskDelivery::Current,
        Err(std::sync::mpsc::SendError(ThreadMsg::Task(task))) => task,
        Err(_) => unreachable!("{}", summon_i18n::format!("r32320e8223")),
    };
    let Some(replacement) = replacement() else {
        return TaskDelivery::RetainedPending;
    };
    match replacement.tx.send(ThreadMsg::Task(task)) {
        Ok(()) => TaskDelivery::Replacement(replacement),
        Err(_) => TaskDelivery::RetainedPending,
    }
}

/// `current.send(Task)` 已返回 SendError 时，旧 worker 的 receiver 确定已经 drop。旧 worker 若
/// 发过 ChatExited，则 send(ChatExited) happens-before receiver drop，因而事件此刻已经在本 `rx`
/// 中；把它提取并收口后再插 replacement，旧事件就不可能误删新 handle。其他消息保持 FIFO 放入
/// deferred，下一轮优先处理。
fn drain_disconnected_chat_exits(
    rx: &Receiver<DispatchMsg>,
    deferred: &mut VecDeque<DispatchMsg>,
    chat_id: &str,
) -> Vec<ChatExitEvent> {
    let mut exited = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(DispatchMsg::ChatExited {
                chat_id: exited_chat,
                chat_type,
                reason,
                creator,
                notify,
                session_created_at,
            }) if exited_chat == chat_id => exited.push(ChatExitEvent {
                chat_id: exited_chat,
                chat_type,
                reason,
                creator,
                notify,
                session_created_at,
            }),
            Ok(other) => deferred.push_back(other),
            Err(std::sync::mpsc::TryRecvError::Empty)
            | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }
    exited
}

#[allow(clippy::too_many_arguments)]
fn settle_chat_exit(
    chats: &mut HashMap<String, ChatHandle>,
    peer_trigger_limiter: &mut PeerTriggerLimiter,
    peers_store: &crate::runtime::peers_store::PeersStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    effective: &Messaging,
    agent: &Agent,
    storage_scope: &FeishuStorageScope,
    exited: ChatExitEvent,
) {
    let ChatExitEvent {
        chat_id,
        chat_type,
        reason,
        creator,
        notify,
        session_created_at,
    } = exited;
    chats.remove(&chat_id);
    peer_trigger_limiter.remove(&chat_id);
    if reason.group_gone() {
        let tag = chat_tag(&chat_type, &chat_id);
        match peers_store.remove(&tag) {
            Ok(true) => log(&summon_i18n::format!("r84bce2bfb3", agent.name)),
            Ok(false) => {}
            Err(e) => log(&summon_i18n::format!("r134473cb7e", agent.name)),
        }
    }
    log(&summon_i18n::format!("r4920f2e294", agent.name));
    if notify {
        // R4:不在 dispatcher 线程同步发飞书；网络黑洞不能饿死本 agent 的其他 chat。
        exit_receipt::enqueue(
            outbox,
            effective,
            &ExitReceipt {
                agent,
                delivery_scope: &storage_scope.key,
                chat_type: &chat_type,
                chat_id: &chat_id,
                chat_name: None,
                creator: creator.as_deref(),
                event_operator: None,
                reason,
                session_created_at: session_created_at.as_deref(),
            },
        )
        .log_if_failed(&agent.name);
    }
}

/// chat 线程退出时(任何 return 路径)停掉并 join 会话镜像 watcher,绝不留孤儿线程。
pub(super) struct MirrorGuard {
    pub(super) stop: Arc<std::sync::atomic::AtomicBool>,
    pub(super) handle: Option<thread::JoinHandle<()>>,
}
impl Drop for MirrorGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 一个 chat 的菜单账本:dispatcher 句柄一份、chat 线程一份,指向同一个 `Arc`。
pub(super) fn chat_menu_ledgers() -> (
    Arc<summon_core::interactive::MenuLedger>,
    Arc<summon_core::interactive::MenuLedger>,
) {
    let ledger = Arc::<summon_core::interactive::MenuLedger>::default();
    (ledger.clone(), ledger)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_chat_thread(
    agent: Agent,
    delivery_scope: String,
    tmux_name: String,
    poll: u64,
    debounce: u64,
    fc: Arc<FeishuClient>,
    frozen_epoch: FeishuEpochIdentity,
    chat_id: String,
    chat_type: String,
    self_tx: Sender<DispatchMsg>,
    core: Arc<crate::runtime::core::Core>,
    inflight: crate::runtime::inflight::InFlight,
) -> ChatHandle {
    let (tx, rx) = channel::<ThreadMsg>();
    // 计时器起点从盘上恢复(见 read_chat_last_activity):用 now() 会让每次 daemon 重启把所有
    // chat 的空闲计时一起清零 —— 既造成集体同相推卡,又让空闲会话永远续命。
    let last_activity = read_chat_last_activity(&agent, &tmux_name).unwrap_or_else(SystemTime::now);
    // 「本段空闲期已发过提醒卡」这个事实同样要跨重启成立,账本是 durable marker(先写后发,
    // 见 read_idle_prompt_marker_since):只恢复空闲时钟、把 idle_prompted 归零的话,重启后
    // 第一轮扫描就会给所有已到点的会话再发一张卡(owner 2026-08-13 两次部署实测)。
    // marker 缺失 = 本机制确实没尝试过 → false 照常发;marker 在但读不清 → fail-closed 不发。
    // 「缺失即没试过」对老版本(无 marker 机制)发过卡的存量会话本不成立 —— 升级迁移
    // (ensure_idle_prompt_marker_migration,boot 恢复扫描之前跑)已为它们的当段空闲期补写
    // 「已提醒」marker,把这条不变量接到升级前的历史上;不为此保留归档回看(best-effort
    // 归档当不了账本,轮转 .1 还要多一层语义)。
    // 入站路径在 spawn 之后照旧走 note_activity 重置,新一段空闲期不受这里影响。
    let idle_prompted = read_idle_prompt_marker_since(&agent, &chat_type, &chat_id, last_activity);
    let nm = tmux_name.clone();
    let ct = chat_type.clone();
    // 给 chat 线程一个自身 channel 的发送端 clone:mid-turn pause+append 偷看 rx 时,把误取的
    // 普通/控制消息 requeue 回队尾(不丢、维持排队语义)。
    let self_chat_tx = tx.clone();
    // subagent 观测状态在 spawn 侧建(而不是 chat_thread_run 内):dispatcher 的空闲判定
    // 要经 handle 读同一份。线程内 Reload 复用同一个 Arc(先清零再重放),句柄跨 Reload 不失效。
    let subagent_state: SharedSubagentState = Arc::new(std::sync::Mutex::new(
        summon_core::subagent::SubagentState::new(),
    ));
    let thread_subagent_state = subagent_state.clone();
    // 菜单账本同理在 spawn 侧建:卡片上的单选由 dispatcher 注入,答过的菜单要经它告诉 chat 线程的守望。
    let (menu_ledger, thread_menu_ledger) = chat_menu_ledgers();
    let watch_tag = chat_tag(&chat_type, &chat_id);
    let epoch = crate::runtime::agent_retirement::epoch(&agent.dir);
    thread::spawn(move || {
        let retired = epoch.read().unwrap_or_else(|e| e.into_inner());
        if *retired {
            return;
        }
        // fleet 观测面:主轮收尾后子 agent 还在跑的会话也算在干活。随线程退出注销。
        let _subagents = crate::observe::runtime_status::watch_subagents(
            &agent.name,
            &watch_tag,
            &thread_subagent_state,
        );
        chat_thread(
            agent,
            delivery_scope,
            nm,
            poll,
            debounce,
            fc,
            frozen_epoch,
            rx,
            self_chat_tx,
            chat_id,
            ct,
            self_tx,
            core,
            inflight,
            thread_subagent_state,
            thread_menu_ledger,
        )
    });
    ChatHandle {
        tx,
        last_activity,
        idle_prompted,
        chat_type,
        tmux_name,
        tmux_seen_live: false,
        local_claimed: false,
        // 来源标记不落盘,handle 重建(含 daemon 重启)一律从「非定时尾轮」起步:
        // 证不了就当普通会话,回退提醒卡行为,绝不凭猜测静默拆会话。
        // 代际同理从空串起步:重启前发出的 admission 已随旧 dispatcher 消亡,不存在错配。
        last_inbound_scheduled: false,
        last_inbound_id: String::new(),
        turn_was_active: false,
        subagent_state: Some(subagent_state),
        menu_ledger: Some(menu_ledger),
    }
}

#[cfg(test)]
mod scheduled_fire_notice_tests {
    use super::*;

    /// 卡片的目的是「认出是哪条定时任务」,不是复述整段 prompt——短正文原样带上。
    #[test]
    fn 定时预告带上短正文() {
        let notice = scheduled_fire_notice("每日梳理 workflow inbox");
        assert!(notice.contains("定时任务触发"), "{notice}");
        assert!(notice.contains("每日梳理 workflow inbox"), "{notice}");
        assert!(!notice.contains("已截断"), "{notice}");
    }

    /// 定时任务的 prompt 可以是整段工作流,原样贴进群里就是刷屏。
    /// 按**字符**截断:中文按字节切会切碎多字节序列(这条用纯中文正文钉死这一点)。
    #[test]
    fn 定时预告截断长正文且报总字数() {
        let body = "梳".repeat(500);
        let notice = scheduled_fire_notice(&body);
        assert!(notice.contains("正文共 500 字"), "{notice}");
        assert!(notice.contains("已截断"), "{notice}");
        assert!(
            notice.chars().count() < 260,
            "截断后仍过长:{} 字",
            notice.chars().count()
        );
    }

    /// 空 prompt 不该发出一张只有标题的空卡——说清「这条任务没附正文」。
    #[test]
    fn 定时预告处理空正文() {
        for empty in ["", "   ", "\n\t"] {
            let notice = scheduled_fire_notice(empty);
            assert!(notice.contains("未附正文"), "input={empty:?} -> {notice}");
        }
    }
}

#[cfg(test)]
mod observation_audience_tests {
    use super::*;

    #[test]
    fn consumed_marker_marks_live_turn() {
        let evidence = crate::capture::MirrorMarkerEvidence::from_parts(vec!["m1"], vec!["m1"]);
        assert_eq!(observation_audience(&evidence), "live turn");
    }

    #[test]
    fn empty_evidence_marks_markerless() {
        let evidence = crate::capture::MirrorMarkerEvidence::default();
        assert_eq!(observation_audience(&evidence), "markerless");
    }

    /// 文件按钮的点击按本 chat 的 talk gate(talk 名单、本群 `/introduce` 认识的成员)或本群范围的
    /// operate-gate 判:过了私聊一条新签的打开链接(归档只记已脱敏,链接不落盘),都过不了私聊没有
    /// 权限、不签链接;群里什么都不发。
    #[test]
    fn a_preview_button_click_sends_an_open_link_privately_to_talkers_and_operators() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("report.md");
        std::fs::write(&file, "# 报告\n").unwrap();
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let agent_with = |yaml: &str| {
            let mut agent: Agent = serde_yaml::from_str(yaml).unwrap();
            agent.dir = tmp.path().join("agents").join(&agent.name);
            std::fs::create_dir_all(&agent.dir).unwrap();
            agent
        };
        let listed = agent_with(
            "name: listed\nfeishu:\n  allowed_users: [ou_talker]\n  operators: [ou_operator]\n  \
             chat_operators:\n    oc_group: [ou_chat_op]\n",
        );
        let open = agent_with("name: open\nfeishu: {}\n");
        let peers = crate::runtime::peers_store::PeersStore::new(tmp.path().join("peers"));
        let introduce = |chat_type: &str, chat_id: &str, open_id: &str| {
            peers
                .add_peers(
                    &chat_tag(chat_type, chat_id),
                    vec![summon_core::peers::Peer {
                        open_id: open_id.into(),
                        name: "同事".into(),
                        kind: summon_core::peers::PeerKind::Human,
                    }],
                    Some(""),
                )
                .unwrap();
        };
        introduce("group", "oc_group", "ou_peer");
        introduce("group", "oc_other", "ou_elsewhere");
        let core =
            crate::runtime::core::Core::new(HashMap::new(), HashMap::new(), vec![listed.clone()]);
        core.attach_preview_server(crate::runtime::preview_server::PreviewServer::for_test(
            tmp.path().join("tokens.json"),
            "http://10.0.0.9:7765",
            1024 * 1024,
        ));
        let previews = core.preview_server().unwrap();
        let token = previews
            .locator_token(file.to_str().unwrap(), None)
            .unwrap();
        let click = |operator: &str| CardActionMsg {
            chat_id: "oc_group".into(),
            message_id: "om_reply".into(),
            operator_open_id: operator.into(),
            event_id: "ev_1".into(),
            value: serde_json::json!({"action": "preview_open", "token": token}),
            wal_id: None,
            wal_replayed: false,
        };
        let archived = |agent: &Agent, chat_type: &str, chat_id: &str| {
            std::fs::read_to_string(
                agent
                    .state_dir()
                    .join("threads")
                    .join(chat_tag(chat_type, chat_id))
                    .join("outbound.ndjson"),
            )
            .unwrap_or_default()
        };
        let chats = HashMap::new();

        // talk 名单里的人、本群认识的成员、只在 operate 名单或本群操作名单里的人,以及名单皆空时
        // 群里的任何人(人人可对话)。
        let admitted = [
            (&listed, "ou_talker"),
            (&listed, "ou_peer"),
            (&listed, "ou_operator"),
            (&listed, "ou_chat_op"),
            (&open, "ou_stranger"),
        ];
        for (agent, operator) in admitted {
            handle_card_preview_open(&chats, agent, &fc, &core, &peers, &click(operator));
            let sent = archived(agent, "p2p", operator);
            assert!(
                sent.contains("preview_link") && sent.contains("content_redacted"),
                "{operator}: {sent}"
            );
            assert!(!sent.contains("10.0.0.9"), "打开链接不落归档:{sent}");
        }
        assert_eq!(previews.open_token_count(), admitted.len());

        // 陌生人,以及只在别的群被认识的成员。
        for operator in ["ou_stranger", "ou_elsewhere"] {
            handle_card_preview_open(&chats, &listed, &fc, &core, &peers, &click(operator));
            let notice = archived(&listed, "p2p", operator);
            assert!(
                notice.contains(::summon_core::i18n::label(PREVIEW_DENIED)),
                "{operator}: {notice}"
            );
            assert!(!notice.contains("preview_link"), "{operator}: {notice}");
        }
        assert_eq!(
            previews.open_token_count(),
            admitted.len(),
            "判不过的点击不签链接"
        );
        for agent in [&listed, &open] {
            assert!(archived(agent, "group", "oc_group").is_empty(), "群里不发");
        }

        // 盘上记着是私聊的 chat:peers 按私聊的 tag 查,同一 chat_id 记在群 tag 下的不算。
        std::fs::create_dir_all(listed.state_dir()).unwrap();
        crate::runtime::daemon::chat_meta::write_chat_meta(
            &listed,
            "soul-listed-dm",
            "oc_dm",
            "p2p",
            None,
            "ou_talker",
            None,
        );
        introduce("p2p", "oc_dm", "ou_dm_peer");
        introduce("group", "oc_dm", "ou_group_tag");
        let dm_click = |operator: &str| CardActionMsg {
            chat_id: "oc_dm".into(),
            ..click(operator)
        };
        handle_card_preview_open(&chats, &listed, &fc, &core, &peers, &dm_click("ou_dm_peer"));
        assert!(archived(&listed, "p2p", "ou_dm_peer").contains("preview_link"));
        handle_card_preview_open(
            &chats,
            &listed,
            &fc,
            &core,
            &peers,
            &dm_click("ou_group_tag"),
        );
        assert!(archived(&listed, "p2p", "ou_group_tag")
            .contains(::summon_core::i18n::label(PREVIEW_DENIED)));

        let mut expired = click("ou_operator");
        expired.value = serde_json::json!({"action": "preview_open", "token": "0".repeat(32)});
        handle_card_preview_open(&chats, &listed, &fc, &core, &peers, &expired);
        assert!(archived(&listed, "p2p", "ou_operator")
            .contains(::summon_core::i18n::label(PREVIEW_UNAVAILABLE)));
    }

    /// 镜像补发是与 turn 出站**完全独立**的一条渲染路径,它自己不接预览服务的话,停机窗口
    /// 补发的那一轮回复没有文件按钮。2026-08-30 owner 在飞书上撞见:部署重启 daemon 冲掉了
    /// turn 记录,回复经镜像补发,路径打不开——那张卡连角标都没有,因为它根本不是常规回复。
    /// 同一份「先认路径再转义」的约定得在两条路上各写一次。
    #[test]
    fn mirror_outbound_turns_local_paths_into_buttons_like_the_turn_path_does() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: mirror-agent\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("mirror-agent");
        std::fs::create_dir_all(&agent.dir).unwrap();
        let file = tmp.path().join("report.md");
        std::fs::write(&file, "# 报告\n").unwrap();
        let preview = crate::runtime::preview_server::PreviewServer::for_test(
            tmp.path().join("tokens.json"),
            "http://10.0.0.9:7765",
            1024 * 1024,
        );
        let text = format!("报告在 `{}` 里", file.to_str().unwrap());

        let (rendered, buttons) =
            render_mirror_outbound(&agent, "gtest", Some(preview.as_ref()), None, &text);
        assert_eq!(buttons.len(), 1, "镜像出站必须给本地文件生成打开按钮");
        let link = preview.open_link(&buttons[0].token).unwrap();
        assert!(link.url.starts_with("http://10.0.0.9:7765/p/"));
        assert_eq!(buttons[0].label, "report.md");
        assert!(
            rendered.as_str().contains(file.to_str().unwrap()),
            "正文里的路径保持原样:{}",
            rendered.as_str()
        );

        // 没挂预览服务时路径原样保留、没有按钮 —— 预览是可选层,缺它不该改动别的东西。
        let (plain, none) = render_mirror_outbound(&agent, "gtest", None, None, &text);
        assert!(none.is_empty());
        assert!(
            plain.as_str().contains(file.to_str().unwrap()),
            "{}",
            plain.as_str()
        );
        assert!(
            !plain.as_str().contains("http://10.0.0.9"),
            "{}",
            plain.as_str()
        );
    }

    #[test]
    fn help_uses_one_title_and_separates_summon_from_backend_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: help-agent\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("help-agent");
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::write(
            agent.dir.join("agent.yaml"),
            "name: help-agent\nruntime:\n  cli: claude\n",
        )
        .unwrap();
        let (help, note) = feishu_chat_help(&agent);
        // 注脚必须留在正文之外:它要渲成灰字,而灰字模板 `<font>` 拼进正文就会被出站转义成明文。
        assert!(!help.contains("<font"), "help 正文不得夹带模板标签: {help}");
        assert_eq!(
            note,
            Some("其它 `/…` 命令也会经过权限与黑名单检查；动态命令以当前 CLI 的实时能力为准。")
        );
        let card = summon_core::feishu::card_markdown_with_note(&help, note.unwrap_or(""));
        assert!(
            card.contains("<font color='grey'>其它"),
            "注脚必须以完整 <font> 模板进卡片,而不是被消毒成明文: {card}"
        );
        assert!(help.starts_with("**可用命令**"));
        assert_eq!(help.matches("**可用命令**").count(), 1);
        assert!(!help.contains("飞书聊天指令"));
        assert!(!help.contains("当前会话命令"));
        let summon = help.find("**Summon 管理**").unwrap();
        let backend = help.find("**当前后端：claude · tmux 交互**").unwrap();
        assert!(summon < help.find("`/cot on|off`").unwrap());
        assert!(help.find("`/cot on|off`").unwrap() < backend);
        assert!(backend < help.find("`/status [参数]`").unwrap());
        let exit_row = help.lines().find(|row| row.contains("`/quit`")).unwrap();
        assert!(exit_row.contains("`/exit`"));
        let archive_row = help.lines().find(|row| row.contains("`/reset`")).unwrap();
        assert!(archive_row.contains("`/archive`") && archive_row.contains("`/arc`"));
        for alias in ["/quit", "/exit", "/reset", "/archive", "/arc"] {
            assert_eq!(help.matches(&format!("`{alias}`")).count(), 1);
        }
        for blocked in [
            &["/quit", "/reset"][..],
            &["/quit", "/exit", "/reset", "/archive", "/arc"][..],
        ] {
            let manifest = serde_yaml::from_str::<serde_yaml::Value>(&format!(
                "name: help-agent\nruntime:\n  cli: claude\nmessaging:\n  slash_blacklist:\n    default: {}\n",
                serde_json::to_string(blocked).unwrap()
            )).unwrap();
            std::fs::write(
                agent.manifest_path(),
                serde_yaml::to_string(&manifest).unwrap(),
            )
            .unwrap();
            let (filtered, _) = feishu_chat_help(&agent);
            for alias in ["/quit", "/exit", "/reset", "/archive", "/arc"] {
                assert_eq!(
                    filtered.contains(&format!("`{alias}`")),
                    !blocked.contains(&alias)
                );
            }
            assert!(filtered.contains("`/resume`"));
            if blocked.len() == 2 {
                assert!(filtered.contains("- `/archive` · `/arc`："));
                assert!(filtered.contains("- `/exit`："));
            }
        }
    }
}

#[cfg(test)]
mod peer_talk_gate_tests {
    use super::*;
    use summon_core::peers::{Peer, PeerKind};

    /// 被 introduce 的同事连发几条是正常对话,不能把全群 peer 消息一起熔断;
    /// bot 与身份拿不准的都计入。
    #[test]
    fn only_non_human_peer_messages_count_toward_the_loop_budget() {
        assert!(counts_toward_peer_budget(true, "app"));
        assert!(!counts_toward_peer_budget(true, "user"));
        assert!(counts_toward_peer_budget(true, ""), "拿不准按 bot 算");
        assert!(
            !counts_toward_peer_budget(false, "app"),
            "非 peer 不走这道闸"
        );
    }

    /// 这次 @ 事件没给身份时,回执与热更新用盘上已确认的那个。
    #[test]
    fn recorded_kinds_fill_only_the_unknown_ones() {
        let peer = |open_id: &str, kind| Peer {
            open_id: open_id.into(),
            name: open_id.into(),
            kind,
        };
        let filled = with_recorded_kinds(
            vec![
                peer("ou_a", PeerKind::Unknown),
                peer("ou_b", PeerKind::Bot),
                peer("ou_c", PeerKind::Unknown),
            ],
            &[peer("ou_a", PeerKind::Human), peer("ou_b", PeerKind::Human)],
        );
        let kinds: Vec<_> = filled.iter().map(|p| p.kind).collect();
        assert_eq!(kinds, [PeerKind::Human, PeerKind::Bot, PeerKind::Unknown]);
    }

    #[test]
    fn introduce_takes_each_members_kind_from_the_mention() {
        let mention = |open_id: &str, name: &str, kind: &str| summon_core::ipc::Mention {
            open_id: open_id.into(),
            name: name.into(),
            kind: kind.into(),
            ..Default::default()
        };
        let peers = peers_from_mentions(
            &[
                mention("ou_self", "我", "bot"),
                mention("ou_sender", "发令人", "user"),
                mention("ou_bot", "星光", "bot"),
                mention("ou_h", "张三", "user"),
                mention("ou_x", "未给身份", ""),
            ],
            "ou_sender",
            "ou_self",
        );
        let kinds: Vec<_> = peers.iter().map(|p| (p.name.as_str(), p.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("星光", PeerKind::Bot),
                ("张三", PeerKind::Human),
                ("未给身份", PeerKind::Unknown)
            ]
        );
    }

    fn persist_bridge_callback(
        wal: &crate::runtime::card_action_wal::CardActionWal,
        event_id: &str,
    ) -> CardActionMsg {
        let card = CardActionMsg {
            chat_id: "oc_private".into(),
            message_id: "om_control".into(),
            operator_open_id: "ou_owner".into(),
            event_id: event_id.into(),
            value: serde_json::json!({
                "action": "bridge_stop",
                "ticket": "ticket",
                "choice": "choice",
            }),
            wal_id: None,
            wal_replayed: false,
        };
        match wal.persist("app-test", &card).unwrap() {
            crate::runtime::card_action_wal::PersistOutcome::Pending { record, .. } => record.card,
            crate::runtime::card_action_wal::PersistOutcome::Consumed => {
                panic!("新 callback 不应预先 consumed")
            }
        }
    }

    /// P1-D:兜底占位串。人侧带 open_id(区分不同人),bot 侧退到裸 open_id。这个公式是
    /// dispatcher 与 chat 线程共用的单一事实源,写死一次防两边对不上。
    #[test]
    /// **跟你说话的人,必须能被你 @ 回去。**
    ///
    /// 真机(2026-08-15):agent 在别的群写 `@张三` 一直发成纯文本、@ 不到人。查下来
    /// 身份缓存里躺着李四、王五、炼金师、星光……**唯独没有 owner** —— 因为缓存只记
    /// 「这条消息里被 @ 到的人」,而 owner 恰恰是那个一直在 @ 别人、自己几乎不被 @ 的人。
    fn the_sender_is_cached_so_the_agent_can_mention_them_back() {
        let got = identities_to_cache("ou_owner", "张三", "用户(ou_owner)", &[]);
        assert_eq!(
            got,
            vec![("ou_owner".to_string(), "张三".to_string())],
            "一条没有任何 @ 的普通消息,也必须把发信人记进表"
        );

        // 发信人 + 被 @ 到的人,两边都要在。
        let got = identities_to_cache(
            "ou_owner",
            "张三",
            "用户(ou_owner)",
            &[("ou_bot".into(), "炼金师".into())],
        );
        assert_eq!(
            got,
            vec![
                ("ou_owner".to_string(), "张三".to_string()),
                ("ou_bot".to_string(), "炼金师".to_string()),
            ]
        );
    }

    /// 反向那半条:**名字还没解析出来时不能污染缓存**。兜底占位不是名字,记进去会在表里留一条
    /// 永远匹配不上的垃圾,还会挡住之后真名解析出来的那一条(缓存按 name 去重)。
    #[test]
    fn a_placeholder_display_name_is_never_cached() {
        let fb = fallback_display_name("ou_x", false);
        assert_eq!(
            identities_to_cache("ou_x", &fb, &fb, &[]),
            Vec::<(String, String)>::new(),
            "兜底占位不得进表"
        );
        // bot 侧的兜底是裸 open_id,同样不进。
        let fb_bot = fallback_display_name("ou_b", true);
        assert!(identities_to_cache("ou_b", &fb_bot, &fb_bot, &[]).is_empty());
        // 但被 @ 到的人照旧要记 —— 发信人没名字不该连累别人。
        assert_eq!(
            identities_to_cache("ou_x", &fb, &fb, &[("ou_y".into(), "李四".into())]),
            vec![("ou_y".to_string(), "李四".to_string())]
        );
    }

    #[test]
    fn fallback_display_name_human_carries_open_id_bot_uses_open_id() {
        assert_eq!(fallback_display_name("ou_abc", false), "用户(ou_abc)");
        assert_eq!(fallback_display_name("ou_bot", true), "ou_bot");
    }

    #[test]
    fn durable_callback_ack_only_happens_for_terminal_disposition() {
        let temp = tempfile::tempdir().unwrap();
        let wal = crate::runtime::card_action_wal::CardActionWal::new(temp.path().join("card-wal"));
        let card = persist_bridge_callback(&wal, "event-disposition");

        finish_durable_card_callback(&card, &wal, CallbackDisposition::Retry);
        let retry_health = wal.inspect_health().unwrap();
        assert_eq!(retry_health.pending, 1);
        assert_eq!(retry_health.consumed, 0);

        finish_durable_card_callback(&card, &wal, CallbackDisposition::Terminal);
        let terminal_health = wal.inspect_health().unwrap();
        assert_eq!(terminal_health.pending, 0);
        assert_eq!(terminal_health.consumed, 1);
    }

    fn test_chat_task(message_id: &str) -> Box<ChatTask> {
        Box::new(ChatTask {
            anchor_message_id: None,
            schedule_notice: None,
            fresh_context: false,
            fresh_requested: false,
            queued: false,
            handoff: None,
            sender_is_bot: false,
            text: "请继续".into(),
            message_id: message_id.into(),
            who: "用户".into(),
            who_id: "ou_owner".into(),
            who_union_id: String::new(),
            sender_is_peer: false,
            is_group: false,
            kind: ChatTaskKind::Conversation,
            slash_admitted: false,
            addressed: true,
            parent_id: None,
            thread_id: None,
            root_id: None,
            resources: Vec::new(),
            co_mentioned: Vec::new(),
            hide_context_hint: false,
            reattach: false,
            messaging: summon_core::config::builtin_messaging(),
        })
    }

    fn test_handle(tx: Sender<ThreadMsg>) -> ChatHandle {
        ChatHandle {
            tx,
            last_activity: SystemTime::now(),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: "soul-test".into(),
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        }
    }

    /// 接管入站经 dispatcher 时**一个字节都不许再落 WAL**:盘上那条记录已经在了,`started=true`
    /// 正是接管的立身之本。再 append 一次会把它整条重写 —— 记录带上接管形态(退恢复卡后用户
    /// 点「恢复」投出的还是接管形态,锚点恒对不上,又一张卡)、`started` 归零(这一轮没跑完时
    /// 下次冷启当未开跑重注入,同一句话跑两遍)。
    #[test]
    fn a_reattach_inbound_never_rewrites_the_record_it_came_from() {
        let temp = tempfile::tempdir().unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let mut inbound = Inbound {
            anchor_message_id: None,
            chat_id: "oc_1".into(),
            chat_type: Some("p2p".into()),
            text: "上一代注入过的那条".into(),
            sender: "ou_1".into(),
            sender_union_id: String::new(),
            sender_type: String::new(),
            message_id: "om_reattach".into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms: 1,
            replayed: false,
            reattach: false,
        };
        // 上一代:普通入站落盘 + 开跑。
        wal.append("agent-a", "scope-a", &inbound).unwrap();
        wal.mark_started("agent-a", "scope-a", "om_reattach")
            .unwrap();

        // 本代:同一条消息以接管形态回到 dispatcher。
        inbound.reattach = true;
        let calls = std::cell::Cell::new(0usize);
        after_durable_inbox(&wal, "agent-a", "scope-a", &inbound, || {
            calls.set(calls.get() + 1)
        })
        .unwrap();

        assert_eq!(calls.get(), 1, "易失副作用照做:去重记账 + 投递 chat 线程");
        let pending = wal.scan_pending();
        assert_eq!(pending.len(), 1, "不得多出一条 pending 记录");
        assert!(pending[0].started, "接管要的正是这条已 started 的记录");
        assert!(
            !pending[0].inbound.reattach,
            "盘上记录不得被接管形态重写,否则恢复卡投出的重放会无限自繁殖"
        );
    }

    #[test]
    fn append_failure_never_enters_volatile_dispatch_then_repair_runs_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("inbox");
        std::fs::create_dir_all(&root).unwrap();
        // pending 本应为目录；用同名文件制造稳定、无需权限假设的 create/read 失败。
        std::fs::write(root.join("pending"), b"blocked").unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(root.clone());
        let inbound = Inbound {
            anchor_message_id: None,
            chat_id: "oc_1".into(),
            chat_type: Some("p2p".into()),
            text: "只执行一次".into(),
            sender: "ou_1".into(),
            sender_union_id: String::new(),
            sender_type: String::new(),
            message_id: "om_write_ahead".into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms: 1,
            replayed: false,
            reattach: false,
        };
        let calls = std::cell::Cell::new(0usize);
        let failed = after_durable_inbox(&wal, "agent-a", "scope-a", &inbound, || {
            calls.set(calls.get() + 1)
        });
        assert!(failed.is_err());
        assert_eq!(
            calls.get(),
            0,
            "append 未 durable 时不得进入 chat/handle_task"
        );

        std::fs::remove_file(root.join("pending")).unwrap();
        after_durable_inbox(&wal, "agent-a", "scope-a", &inbound, || {
            calls.set(calls.get() + 1)
        })
        .unwrap();
        assert_eq!(calls.get(), 1, "存储修复后的同一轮只执行一次");
        assert_eq!(wal.scan_pending().len(), 1);
    }

    #[test]
    fn disconnected_worker_retries_exact_task_once_on_replacement() {
        let (current_tx, current_rx) = channel();
        drop(current_rx);
        let (replacement_tx, replacement_rx) = channel();

        let delivery = deliver_task_with_one_replacement(
            &current_tx,
            test_chat_task("om_retry_exact"),
            || Some(test_handle(replacement_tx)),
        );
        let TaskDelivery::Replacement(_handle) = delivery else {
            panic!("旧 receiver 断开时应把原任务交给 replacement");
        };
        match replacement_rx.recv().unwrap() {
            ThreadMsg::Task(task) => assert_eq!(task.message_id, "om_retry_exact"),
            _ => panic!("replacement 只能收到原 Task"),
        }
    }

    #[test]
    fn failed_recovery_replay_delivery_stays_claimed_and_pending() {
        use crate::runtime::inbox_wal::{InboxWal, RecoveryClaimOutcome, RecoveryItemState};

        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        let original = Inbound {
            anchor_message_id: None,
            chat_id: "oc_recovery".into(),
            chat_type: Some("p2p".into()),
            text: "只执行一次".into(),
            sender: "ou_owner".into(),
            sender_union_id: String::new(),
            sender_type: String::new(),
            message_id: "om_original".into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms: 1,
            replayed: false,
            reattach: false,
        };
        wal.append("agent-a", "scope-a", &original).unwrap();
        let source = wal
            .scan_pending()
            .into_iter()
            .find(|record| record.inbound.message_id == original.message_id)
            .unwrap();
        let batch = wal
            .create_recovery_batch(
                "scope-a",
                "agent-a",
                "oc_recovery",
                "p2p",
                None,
                vec![source],
                1,
            )
            .unwrap();
        wal.finalize_recovery_sources(&batch).unwrap();
        wal.record_recovery_card_delivery(&batch.id, &batch.outbox_id, "om_card")
            .unwrap();
        let replay = match wal
            .claim_recovery(
                &batch.id,
                "scope-a",
                "agent-a",
                "oc_recovery",
                "om_card",
                "om_original",
            )
            .unwrap()
        {
            RecoveryClaimOutcome::Claimed(record) => record,
            _ => panic!("首次 recovery claim 应生成 replay"),
        };

        let (current_tx, current_rx) = channel();
        drop(current_rx);
        let (replacement_tx, replacement_rx) = channel();
        drop(replacement_rx);
        let delivery = deliver_task_with_one_replacement(
            &current_tx,
            test_chat_task(&replay.inbound.message_id),
            || Some(test_handle(replacement_tx)),
        );
        assert!(matches!(delivery, TaskDelivery::RetainedPending));

        let active = wal.load_recovery_batch(&batch.id).unwrap().unwrap();
        assert!(matches!(
            active.items[0].state,
            RecoveryItemState::Claimed { .. }
        ));
        let pending = wal
            .scan_pending()
            .into_iter()
            .find(|record| record.inbound.message_id == replay.inbound.message_id)
            .expect("未交给任何 worker 的 replay 必须继续留在 pending");
        assert_eq!(
            pending.recovery_batch_id.as_deref(),
            Some(batch.id.as_str())
        );
        assert!(
            wal.read_consumed("agent-a", &replay.inbound.message_id)
                .unwrap()
                .is_none(),
            "send failure 绝不能把 replay 迁到 consumed/Completed"
        );
    }

    #[test]
    fn disconnected_exit_barrier_extracts_only_target_and_preserves_other_fifo() {
        let (tx, rx) = channel();
        tx.send(DispatchMsg::ScheduledFire {
            chat_id: "oc_other".into(),
            chat_type: "p2p".into(),
            prompt: "later".into(),
            fire_id: "sched-later".into(),
            output: summon_core::ScheduleOutput::Progress,
            context: summon_core::ScheduleContext::Resume,
        })
        .unwrap();
        tx.send(DispatchMsg::ChatExited {
            chat_id: "oc_dead".into(),
            chat_type: "p2p".into(),
            reason: summon_core::exit::ExitReason::PanelClean,
            creator: None,
            notify: false,
            session_created_at: None,
        })
        .unwrap();
        tx.send(DispatchMsg::ChatExited {
            chat_id: "oc_other".into(),
            chat_type: "p2p".into(),
            reason: summon_core::exit::ExitReason::PanelReset,
            creator: None,
            notify: false,
            session_created_at: None,
        })
        .unwrap();
        tx.send(DispatchMsg::Shutdown).unwrap();

        let mut deferred = VecDeque::new();
        let exits = drain_disconnected_chat_exits(&rx, &mut deferred, "oc_dead");
        assert_eq!(exits.len(), 1);
        assert_eq!(exits[0].chat_id, "oc_dead");
        assert!(matches!(
            deferred.pop_front(),
            Some(DispatchMsg::ScheduledFire { fire_id, .. }) if fire_id == "sched-later"
        ));
        assert!(matches!(
            deferred.pop_front(),
            Some(DispatchMsg::ChatExited { chat_id, .. }) if chat_id == "oc_other"
        ));
        assert!(matches!(deferred.pop_front(), Some(DispatchMsg::Shutdown)));
        assert!(deferred.is_empty());
    }

    #[test]
    fn introduced_peer_can_talk_only_in_same_chat_and_cannot_operate() {
        let agent: Agent = serde_yaml::from_str(
            "name: bot\nfeishu:\n  allowed_users: [ou_owner]\n  operators: [ou_owner]\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runtime::peers_store::PeersStore::new(dir.path().join("peers"));
        let introduced_chat = "oc_introduced";
        store
            .add_peers(
                &chat_tag("group", introduced_chat),
                vec![Peer {
                    open_id: "ou_peer_bot".into(),
                    name: "peer".into(),
                    ..Default::default()
                }],
                Some(""),
            )
            .unwrap();

        let introduced_peer = sender_is_known_peer(&store, "group", introduced_chat, "ou_peer_bot");
        let other_chat_peer = sender_is_known_peer(&store, "group", "oc_other", "ou_peer_bot");
        assert!(sender_can_talk(&agent, introduced_peer, "ou_peer_bot", ""));
        assert!(!sender_can_talk(&agent, other_chat_peer, "ou_peer_bot", ""));
        assert!(!sender_is_known_peer(
            &store,
            "group",
            "oc_other",
            "ou_peer_bot"
        ));
        assert!(
            !agent.can_operate("ou_peer_bot", "", true, OperateScope::Agent),
            "peers 只能旁路 talk，不能提升 operate 权限"
        );
        // 上一条说的是**卡片破坏性动作**(清理/重置)的 operate-gate —— 那条边界不变。
        // 与之正交:PreToolUse 执行闸把 peer 视为「内部」(见
        // `summon_core::authz::requester_is_external`)。理由是威胁模型不同:operate-gate 防的是
        // 「谁能拆掉别人的会话」,执行闸防的是「没被授权的人借 agent 之手在 owner 机器上写文件跑命令」;
        // peer 经 operate-gate 保护的 `/introduce` 显式认下(bot 或同事),认下即授权,不在后者的威胁面里。
        assert!(!summon_core::authz::requester_is_external(
            summon_core::authz::TurnRequester::new("ou_peer_bot", "", true),
            &agent,
            true,
            "oc_other",
        ));
    }

    #[test]
    fn text_command_scope_table_pins_what_stays_inside_one_chat() {
        // 改这张表 = 改本群名单的权力边界。
        for command in [OperateCommand::ModelDefault, OperateCommand::EffortDefault] {
            assert_eq!(
                command.scope("oc_1"),
                OperateScope::Agent,
                "{command:?}:改 bot 默认,所有没有本群覆盖的群一起变"
            );
        }
        for command in [
            OperateCommand::Model,
            OperateCommand::Effort,
            OperateCommand::Fast,
            OperateCommand::Quota,
            OperateCommand::SessionTeardown,
            OperateCommand::Term,
            OperateCommand::Dash,
            OperateCommand::Export,
            OperateCommand::Mode,
            OperateCommand::Pwd,
            OperateCommand::Cd,
            OperateCommand::Remind,
            OperateCommand::Introduce,
            OperateCommand::Unintroduce,
            OperateCommand::ProviderSlash,
        ] {
            assert_eq!(
                command.scope("oc_1"),
                OperateScope::Chat("oc_1"),
                "{command:?}"
            );
        }
    }

    /// 只在本群 `chat_operators` 里的人:改本群放行,改默认(`/model default`、`/effort default`、
    /// 卡片上的设为默认)一律拒;agent 级 operator 两者都放行。文字命令按 `default` 选判权档位的
    /// 那段结构由源码守卫钉住(文字命令的判权跑在 dispatcher 大循环里,单测起不来)。
    #[test]
    fn chat_operators_may_change_this_chat_but_not_the_default() {
        use summon_core::feishu::CardAction;
        let agent: Agent = serde_yaml::from_str(
            "name: examplebot\nfeishu:\n  app_id_env: A\n  operators: [ou_lead]\n  chat_operators: {oc_x: [ou_mate]}\nruntime:\n  cli: claude\n",
        )
        .unwrap();
        let can = |who: &str, scope| agent.can_operate(who, "", true, scope);
        for scope in [
            OperateCommand::Model.scope("oc_x"),
            OperateCommand::Effort.scope("oc_x"),
            CardAction::Model.operate_scope("oc_x"),
            CardAction::Effort.operate_scope("oc_x"),
            CardAction::CursorConfig.operate_scope("oc_x"),
        ] {
            assert!(can("ou_mate", scope) && can("ou_lead", scope), "{scope:?}");
        }
        for scope in [
            OperateCommand::ModelDefault.scope("oc_x"),
            OperateCommand::EffortDefault.scope("oc_x"),
            CardAction::ModelDefault.operate_scope("oc_x"),
            CardAction::EffortDefault.operate_scope("oc_x"),
            CardAction::CursorConfigDefault.operate_scope("oc_x"),
        ] {
            assert!(!can("ou_mate", scope), "{scope:?}");
            assert!(can("ou_lead", scope), "{scope:?}");
        }

        let source = include_str!("dispatcher.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let squash = |text: &str| text.split_whitespace().collect::<String>();
        for (entry, default_command, chat_command) in [
            (
                "summon_core::ipc::model_command(",
                "OperateCommand::ModelDefault",
                "OperateCommand::Model",
            ),
            (
                "summon_core::ipc::effort_command(",
                "OperateCommand::EffortDefault",
                "OperateCommand::Effort",
            ),
        ] {
            let start = production.find(entry).expect(entry);
            let branch = squash(&production[start..start + 4000]);
            assert!(
                branch.contains(&squash(
                    "let default_layer = matches!( command, LayeredCommand::Default(_) | LayeredCommand::DefaultWithoutValue );"
                )),
                "{entry}"
            );
            assert!(
                branch.contains(&format!(
                    "letoperate=ifdefault_layer{{{default_command}}}else{{{chat_command}}};"
                )),
                "{entry}:default 走 agent 档"
            );
            assert!(branch.contains("operate.scope(&chat_id)"), "{entry}");
        }
    }

    /// 只有会真正碰到本地会话的 slash 才 claim。被拒的命令若也 claim,这条 chat 会被永久标成
    /// 「本地已占用」,Bridge 再也接管不了 —— 那是拿一条拒绝换掉一整条通道。
    #[test]
    fn only_authorized_pool_commands_and_quit_claim_the_local_session() {
        // 黑名单取**出厂**那份(真相源 messaging.default.yaml),与 worker 侧同源。
        let factory = summon_core::config::builtin_messaging().slash_blacklist;
        let claim = |text: &str, can_operate: bool, capture: CaptureMode| {
            slash_claims_local_session(text, can_operate, capture, CliDriver::Claude, &factory)
        };
        // 池内透传 + 一步生效的 /quit、/exit(owner 2026-08-14 扩入的别名)会碰本地会话;
        // 透传类的带参形态(owner 2026-08-14 授权)与无参同判;**未知命令**准入反转后走通用
        // 执行器,同样会真注入 —— dispatcher 不 claim 而 worker 注入,Bridge 就能在空窗内接管。
        for text in [
            "/compact",
            "/status",
            "/cost",
            "/context",
            "/context all",
            "/status foo",
            "/quit",
            "/exit",
            "/some-unknown-command",
        ] {
            assert!(claim(text, true, CaptureMode::Transcript), "{text}");
        }
        // 没权限 / 没映射 / 只发卡 / 带参 —— 一律不 claim。
        assert!(
            !claim("/compact", false, CaptureMode::Transcript),
            "无 operate 权限"
        );
        assert!(!claim("/compact", true, CaptureMode::Rpc), "无交互通道");
        assert!(
            claim("/quit", true, CaptureMode::Rpc),
            "生命周期拆除不依赖 TUI，RPC 下也必须 claim 后走 Control::Clean"
        );
        assert!(
            !claim("/clear", true, CaptureMode::Transcript),
            "/clear 只发确认卡"
        );
        assert!(
            claim("/compact --extra", true, CaptureMode::Transcript),
            "owner 2026-08-20 授权 /compact 带参:参数即压缩指令,整行透传"
        );
        assert!(!claim("/login", true, CaptureMode::Transcript), "黑名单");
        assert!(
            !claim("/QUIT", true, CaptureMode::Transcript),
            "生命周期命令的同形变体绝不许被通用执行器裸注入"
        );
        assert!(
            !slash_claims_local_session(
                "/compact",
                true,
                CaptureMode::Transcript,
                CliDriver::Codex,
                &factory
            ),
            "codex 没有 tmux 命令通道"
        );
        // 反转后尤其要钉住这一格:codex 不能因为「默认放行」凭空得到一条裸注入通道。
        assert!(
            !slash_claims_local_session(
                "/some-unknown-command",
                true,
                CaptureMode::Transcript,
                CliDriver::Codex,
                &factory
            ),
            "codex 连未知命令的通用路都不该有"
        );
        for cli in [CliDriver::Codex, CliDriver::Traex] {
            assert!(
                slash_claims_local_session("/compact", true, CaptureMode::Rpc, cli, &factory),
                "Codex 家族 RPC 的原生控制方法必须 claim 本地会话"
            );
            assert!(
                !slash_claims_local_session(
                    "/not-a-native-control",
                    true,
                    CaptureMode::Rpc,
                    cli,
                    &factory
                ),
                "没有 RPC 映射的命令不能 claim"
            );
            assert!(
                slash_claims_local_session("/quit", true, CaptureMode::Rpc, cli, &factory),
                "exact /quit 的受控生命周期不依赖 app-server/TUI 通道"
            );
        }
    }

    /// 获准的 `/new <内容>` 改写成以内容起头、要求新上下文的普通对话;未获准或别的命令原样不动。
    #[test]
    fn admitted_new_session_becomes_a_fresh_conversation() {
        let rewrite =
            |text: &str, kind, admitted| rewrite_new_session(text.into(), kind, admitted, text);
        assert_eq!(
            rewrite("/new 帮我看下这个实验", ChatTaskKind::SlashCommand, true),
            ("帮我看下这个实验".into(), ChatTaskKind::Conversation, true)
        );
        assert_eq!(
            rewrite("/new 帮我看下这个实验", ChatTaskKind::SlashCommand, false),
            (
                "/new 帮我看下这个实验".into(),
                ChatTaskKind::SlashCommand,
                false
            ),
            "未获准交 worker 拒绝"
        );
        assert_eq!(
            rewrite("/new", ChatTaskKind::SlashCommand, true),
            ("/new".into(), ChatTaskKind::SlashCommand, false),
            "exact 无参 /new 走 /reset 那条受控路径"
        );
        assert_eq!(
            rewrite("/new 内容", ChatTaskKind::Conversation, true),
            ("/new 内容".into(), ChatTaskKind::Conversation, false),
            "不是 slash 的消息不改写"
        );
    }

    /// 三种 backend 各走一遍 dispatcher 对 `/new <内容>` 的规划:识别成受控 slash、获准占用本地
    /// 会话、改写成要求新上下文的普通对话。
    #[test]
    fn new_session_is_planned_the_same_on_every_backend() {
        let blacklist = summon_core::config::builtin_messaging().slash_blacklist;
        for cli in ["claude", "codex", "cursor"] {
            let (agent, _tmp) = crate::test_support::agent_dir(cli, "");
            let driver = CliDriver::from_kind(agent.cli_kind());
            let capture = agent.capture_mode();
            let text = "/new 帮我看下这个实验";
            let kind = chat_task_kind(true, driver, capture, text, &blacklist);
            assert!(kind.is_slash(), "{cli}: /new 必须进受控规划");
            assert!(
                slash_claims_local_session(text, true, capture, driver, &blacklist),
                "{cli}: 获准的 /new 要占用本地会话"
            );
            assert!(
                !slash_claims_local_session(text, false, capture, driver, &blacklist),
                "{cli}: 未获准不占用"
            );
            assert_eq!(
                rewrite_new_session(text.into(), kind, true, text),
                ("帮我看下这个实验".into(), ChatTaskKind::Conversation, true),
                "{cli}"
            );
        }
    }

    /// 获准的 `/handoff` 改写成写交接的指令并带上目标与备注;未获准或不是这条命令的原样放过。
    #[test]
    fn an_admitted_handoff_is_rewritten_into_the_handoff_instruction() {
        let claude = crate::test_support::agent_with("claude", "");
        let mentions = vec![
            summon_core::ipc::Mention {
                open_id: "ou_me".into(),
                key: "@_user_1".into(),
                name: "Arcadia".into(),
                kind: "bot".into(),
            },
            summon_core::ipc::Mention {
                open_id: "ou_peer".into(),
                key: "@_user_2".into(),
                name: "星光".into(),
                kind: "bot".into(),
            },
        ];
        let raw = "@_user_1 /handoff @_user_2 把 MR 合了";
        let slash = summon_core::ipc::slash_command_text(raw, &mentions, CliDriver::Claude)
            .expect("开头 @ 了本 bot 的 slash");
        let (text, kind, handoff) = rewrite_handoff(
            slash.clone(),
            ChatTaskKind::SlashCommand,
            true,
            raw,
            &mentions,
            Some("ou_me"),
            &claude,
        );
        assert_eq!(kind, ChatTaskKind::Conversation);
        assert_eq!(
            text,
            summon_core::handoff::instruction(
                summon_core::handoff::HandoffAfter::Peer("星光"),
                "把 MR 合了"
            )
        );
        let handoff = handoff.expect("带上命令");
        assert_eq!(
            handoff.target.map(|target| target.open_id).as_deref(),
            Some("ou_peer")
        );
        assert_eq!(handoff.note, "把 MR 合了");

        assert_eq!(
            rewrite_handoff(
                slash.clone(),
                ChatTaskKind::SlashCommand,
                false,
                raw,
                &mentions,
                Some("ou_me"),
                &claude,
            ),
            (slash, ChatTaskKind::SlashCommand, None),
            "未获准的留给 worker 拒绝"
        );
        assert_eq!(
            rewrite_handoff(
                "/compact".into(),
                ChatTaskKind::SlashCommand,
                true,
                "@_user_1 /compact",
                &mentions,
                Some("ou_me"),
                &claude,
            ),
            ("/compact".into(), ChatTaskKind::SlashCommand, None)
        );
    }

    /// 不带目标时,指令按会话的压缩通道写:有交互式压缩命令的原地压缩,其余开新会话。
    #[test]
    fn an_untargeted_handoff_compacts_in_place_only_where_it_can() {
        let mentions = vec![summon_core::ipc::Mention {
            open_id: "ou_me".into(),
            key: "@_user_1".into(),
            name: "Arcadia".into(),
            kind: "bot".into(),
        }];
        let raw = "@_user_1 /handoff 先修登录";
        let slash = summon_core::ipc::slash_command_text(raw, &mentions, CliDriver::Claude)
            .expect("开头 @ 了本 bot 的 slash");
        for (cli, after) in [
            ("claude", summon_core::handoff::HandoffAfter::CompactInPlace),
            ("codex", summon_core::handoff::HandoffAfter::NewSession),
        ] {
            let agent = crate::test_support::agent_with(cli, "");
            let (text, kind, handoff) = rewrite_handoff(
                slash.clone(),
                ChatTaskKind::SlashCommand,
                true,
                raw,
                &mentions,
                Some("ou_me"),
                &agent,
            );
            assert_eq!(kind, ChatTaskKind::Conversation);
            assert_eq!(text, summon_core::handoff::instruction(after, "先修登录"));
            assert!(handoff.is_some_and(|handoff| handoff.target.is_none()));
        }
    }

    /// 被 @ 成目标的那个 bot 也会收到这条 `/handoff`:它不是开头那个 @,所以在它那边是一条未定向
    /// 的群 slash,不占本地会话、由 worker 静默忽略 —— 交接由发起方的 summon 投过来,不靠这条消息。
    #[test]
    fn the_target_bot_leaves_the_raw_handoff_message_alone() {
        let mentions = vec![
            summon_core::ipc::Mention {
                open_id: "ou_source".into(),
                key: "@_user_1".into(),
                name: "Arcadia".into(),
                kind: "bot".into(),
            },
            summon_core::ipc::Mention {
                open_id: "ou_me".into(),
                key: "@_user_2".into(),
                name: "星光".into(),
                kind: "bot".into(),
            },
        ];
        let raw = "@_user_1 /handoff @_user_2 把 MR 合了";
        let factory = summon_core::config::builtin_messaging().slash_blacklist;
        for cli in [CliDriver::Claude, CliDriver::Codex, CliDriver::Cursor] {
            let slash = summon_core::ipc::slash_command_text(raw, &mentions, cli)
                .expect("对目标 bot 也是一条 slash");
            let kind = chat_task_kind(true, cli, CaptureMode::Rpc, &slash, &factory);
            assert!(kind.uses_provider_slash_syntax(), "{cli:?}");
            let addressed = provider_slash_is_addressed(true, Some("ou_me"), raw, &mentions);
            assert!(!addressed);
            assert!(!task_claims_local_session(kind, true, addressed, false));
            assert_eq!(
                rewrite_handoff(
                    slash.clone(),
                    kind,
                    false,
                    raw,
                    &mentions,
                    Some("ou_me"),
                    &crate::test_support::agent_with("claude", ""),
                )
                .2,
                None
            );
        }
    }

    /// `/queue <内容>` 改写成普通对话并标上排队;不是这个形态的原样放过,无参的留给规划器回用法。
    #[test]
    fn queue_is_rewritten_into_a_queued_conversation() {
        let mentions = vec![summon_core::ipc::Mention {
            open_id: "ou_me".into(),
            key: "@_user_1".into(),
            name: "Summon".into(),
            ..Default::default()
        }];
        let group = "@_user_1 /queue 这轮跑完再帮我看日志";
        let slash = summon_core::ipc::slash_command_text(group, &mentions, CliDriver::Claude)
            .expect("开头 @ 了本 bot 的 slash");
        assert_eq!(
            rewrite_queued(
                slash,
                ChatTaskKind::SlashCommand,
                true,
                &conversation_text(group, &mentions, Some("ou_me")).0,
            ),
            (
                "这轮跑完再帮我看日志".into(),
                ChatTaskKind::Conversation,
                true
            )
        );
        assert_eq!(
            rewrite_queued("/queue".into(), ChatTaskKind::SlashCommand, true, "/queue"),
            ("/queue".into(), ChatTaskKind::SlashCommand, false),
            "无参留给规划器"
        );
        assert_eq!(
            rewrite_queued(
                "说说 /queue 是什么".into(),
                ChatTaskKind::Conversation,
                false,
                "说说 /queue 是什么"
            ),
            (
                "说说 /queue 是什么".into(),
                ChatTaskKind::Conversation,
                false
            ),
            "不是命令形态的普通对话不动"
        );
        // 改写不了的形态在 codex 家族上也得是 slash 任务:落成 `$skill` 提示词就会被 steer 进当前轮。
        let factory = summon_core::config::builtin_messaging().slash_blacklist;
        for cli in [CliDriver::Codex, CliDriver::Traex] {
            for form in ["/queue", "/queue!", "/QUEUE 内容"] {
                assert_eq!(
                    chat_task_kind(true, cli, CaptureMode::Rpc, form, &factory),
                    ChatTaskKind::SlashCommand,
                    "{cli:?} {form}"
                );
            }
        }
    }

    /// `/new` 的内容与把这句话直接发给 agent 时一样:其余 @ 渲染成 `@名字`,私聊里的换行原样保留,
    /// 不被 slash 命令那套归一化抹掉。
    #[test]
    fn new_session_content_keeps_mentions_and_line_breaks() {
        let mentions = vec![
            summon_core::ipc::Mention {
                open_id: "ou_me".into(),
                key: "@_user_1".into(),
                name: "Summon".into(),
                ..Default::default()
            },
            summon_core::ipc::Mention {
                open_id: "ou_zhang".into(),
                key: "@_user_2".into(),
                name: "张三".into(),
                ..Default::default()
            },
        ];
        let group = "@_user_1 /new 请 @_user_2 看下这个";
        let slash = summon_core::ipc::slash_command_text(group, &mentions, CliDriver::Claude)
            .expect("开头 @ 了本 bot 的 slash");
        let (text, kind, fresh) = rewrite_new_session(
            slash,
            ChatTaskKind::SlashCommand,
            true,
            &conversation_text(group, &mentions, Some("ou_me")).0,
        );
        assert_eq!(text, "请 @张三 看下这个");
        assert_eq!(kind, ChatTaskKind::Conversation);
        assert!(fresh);

        let p2p = "/new 看下这段日志\nline 1\nline 2";
        let (text, _, fresh) = rewrite_new_session(
            p2p.into(),
            ChatTaskKind::SlashCommand,
            true,
            &conversation_text(p2p, &[], None).0,
        );
        assert_eq!(text, "看下这段日志\nline 1\nline 2");
        assert!(fresh);
    }

    #[test]
    fn mentionless_admitted_conversation_is_addressed_to_this_bot() {
        assert_eq!(
            task_text_and_addressing(None, "帮我看下", false, false, true),
            ("帮我看下".to_string(), false, true)
        );
        assert_eq!(
            task_text_and_addressing(None, "帮我看下", false, false, false),
            ("帮我看下".to_string(), false, false)
        );
        assert_eq!(
            task_text_and_addressing(None, "帮我看下", false, true, false),
            ("帮我看下".to_string(), false, true)
        );
        // 斜杠命令是否说给本 bot 已由 command_is_addressed 并入免 @ 放行,这里原样采用。
        assert_eq!(
            task_text_and_addressing(Some("/compact".into()), "/compact", true, false, true),
            ("/compact".to_string(), true, true)
        );
        assert_eq!(
            task_text_and_addressing(Some("/compact".into()), "/compact", false, false, false),
            ("/compact".to_string(), true, false)
        );
    }

    #[test]
    fn group_commands_are_addressed_by_a_leading_mention_or_a_mentionless_proof() {
        let mentions = vec![summon_core::ipc::Mention {
            open_id: "ou_me".into(),
            key: "@_user_1".into(),
            ..Default::default()
        }];
        assert!(command_is_addressed(
            true,
            Some("ou_me"),
            "/reset",
            &[],
            true
        ));
        assert!(command_is_addressed(
            true,
            Some("ou_me"),
            "@_user_1 /reset",
            &mentions,
            false
        ));
        assert!(!command_is_addressed(
            true,
            Some("ou_me"),
            "/reset",
            &[],
            false
        ));
        assert!(!command_is_addressed(true, None, "/reset", &[], false));
        assert!(command_is_addressed(false, None, "/reset", &[], false));
    }

    #[test]
    fn group_provider_slash_requires_a_leading_self_mention() {
        let factory = summon_core::config::builtin_messaging().slash_blacklist;
        let mentions = vec![summon_core::ipc::Mention {
            open_id: "ou_me".into(),
            key: "@_user_1".into(),
            name: "Summon".into(),
            ..Default::default()
        }];
        assert!(provider_slash_is_addressed(
            true,
            Some("ou_me"),
            "@_user_1 /compact",
            &mentions
        ));
        assert!(!provider_slash_is_addressed(
            true,
            Some("ou_me"),
            "/compact @_user_1",
            &mentions
        ));
        assert!(!provider_slash_is_addressed(true, None, "/compact", &[]));
        assert!(provider_slash_is_addressed(false, None, "/compact", &[]));
        assert_eq!(
            chat_task_kind(
                true,
                CliDriver::Codex,
                CaptureMode::Rpc,
                "/my-skill",
                &factory,
            ),
            ChatTaskKind::ProviderPrompt,
            "Codex /skill 是普通 provider prompt，不得误入受控 tmux slash 通道"
        );
        assert_eq!(
            chat_task_kind(true, CliDriver::Traex, CaptureMode::Rpc, "/quit", &factory,),
            ChatTaskKind::SlashCommand,
            "Traex 的已知 TUI 命令必须进 RPC 控制规划器"
        );
        assert_eq!(
            chat_task_kind(
                true,
                CliDriver::Traex,
                CaptureMode::Rpc,
                "/review",
                &factory,
            ),
            ChatTaskKind::SlashCommand,
            "尚未完整接入的原生控制命令也必须明确拒绝，不能退成模型 prompt"
        );
        assert_eq!(
            chat_task_kind(true, CliDriver::Codex, CaptureMode::Rpc, "/quit", &factory,),
            ChatTaskKind::SlashCommand,
            "生命周期命令不因 capture 降级成模型 prompt"
        );
        for lookalike in ["/QUIT", "/quit!", "/Exit."] {
            assert_eq!(
                chat_task_kind(
                    true,
                    CliDriver::Traex,
                    CaptureMode::Rpc,
                    lookalike,
                    &factory,
                ),
                ChatTaskKind::SlashCommand,
                "{lookalike} 必须进生命周期同形拒绝，不能发给模型"
            );
        }
        assert_eq!(
            chat_task_kind(
                true,
                CliDriver::Claude,
                CaptureMode::Transcript,
                "/compact",
                &factory,
            ),
            ChatTaskKind::SlashCommand
        );
        let custom = slash_pool::SlashBlacklist {
            codex: vec!["/dangerous-skill".into()],
            ..Default::default()
        };
        assert_eq!(
            chat_task_kind(
                true,
                CliDriver::Traex,
                CaptureMode::Rpc,
                "/dangerous-skill",
                &custom,
            ),
            ChatTaskKind::SlashCommand,
            "用户自定义 blacklist 必须先进入 planner，不能按动态 skill 旁路"
        );
        assert!(
            !task_claims_local_session(ChatTaskKind::ProviderPrompt, true, false, false),
            "未定向 Codex /skill 会被 worker 静默忽略，不能先污染 Bridge local claim"
        );
        assert!(task_claims_local_session(
            ChatTaskKind::ProviderPrompt,
            true,
            true,
            false
        ));
        assert!(
            !task_claims_local_session(ChatTaskKind::SlashCommand, true, false, true),
            "即使误带 admitted，未定向群 slash 也不得 claim"
        );
        assert!(
            retain_local_session_claim(true, false),
            "worker 重建时，被拒 slash 不能抹掉旧 handle 已持有的本地会话 claim"
        );
    }

    #[test]
    fn operate_gate_prefers_event_union_id_and_matches_bridge_verdict() {
        // operators 只写 union_id：只有真拿到 union_id 才可能放行,正好暴露「丢掉事件自带
        // union_id、只信 HTTP」的误拒。名单皆空的 agent 用来守 fail-closed 方向没被放松。
        let union_only: Agent = serde_yaml::from_str(
            "name: bot\nfeishu:\n  allowed_users: [ou_talk]\n  operators: [on_owner]\n",
        )
        .unwrap();
        let no_list: Agent = serde_yaml::from_str("name: bot\nfeishu: {}\n").unwrap();

        // (用例名, agent, sender_open_id, 事件带的 union_id, HTTP 能解析出的 union_id, is_group, 期望放行)
        let cases: Vec<(&str, &Agent, &str, &str, &str, bool, bool)> = vec![
            (
                "事件带 union + HTTP 失败 + 群 + 授权",
                &union_only,
                "ou_owner",
                "on_owner",
                "",
                true,
                true,
            ),
            (
                "事件带 union + HTTP 失败 + 私聊 + 授权",
                &union_only,
                "ou_owner",
                "on_owner",
                "",
                false,
                true,
            ),
            (
                "事件不带 union + HTTP 成功 + 群 + 授权(回退生效)",
                &union_only,
                "ou_owner",
                "",
                "on_owner",
                true,
                true,
            ),
            (
                "事件不带 union + HTTP 失败 + 群 + 授权(仍 fail-closed)",
                &union_only,
                "ou_owner",
                "",
                "",
                true,
                false,
            ),
            (
                "事件不带 union + HTTP 失败 + 私聊 + 授权(名单非空,私聊也拒)",
                &union_only,
                "ou_owner",
                "",
                "",
                false,
                false,
            ),
            (
                "事件带非授权 union + HTTP 返回授权 union + 群(信事件,不被 HTTP 提权)",
                &union_only,
                "ou_intruder",
                "on_intruder",
                "on_owner",
                true,
                false,
            ),
            (
                "未授权 + 事件带 union + 私聊",
                &union_only,
                "ou_intruder",
                "on_intruder",
                "",
                false,
                false,
            ),
            (
                "名单皆空 + 群 → fail-closed",
                &no_list,
                "ou_anyone",
                "on_anyone",
                "on_anyone",
                true,
                false,
            ),
            (
                "名单皆空 + 私聊 → 放行(裸会话即 owner)",
                &no_list,
                "ou_anyone",
                "",
                "",
                false,
                true,
            ),
        ];

        for (name, agent, open_id, event_union, http_union, is_group, expect_allow) in cases {
            let http_calls = std::cell::Cell::new(0usize);
            let op_union = operator_union_id(event_union, || {
                http_calls.set(http_calls.get() + 1);
                http_union.to_string()
            });
            let verdict = agent.can_operate(open_id, &op_union, is_group, OperateScope::Agent);
            assert_eq!(verdict, expect_allow, "{name}");

            // 事件自带 union 时不该再打飞书 HTTP(热路径上的多余网络调用,也是误拒的源头)。
            let expected_calls = usize::from(event_union.is_empty());
            assert_eq!(http_calls.get(), expected_calls, "{name}: HTTP 调用次数");

            // 与 `/bridge` 路径(daemon/bridge.rs 里同款「事件优先、空了回退」)判定必须一致。
            let bridge_union = if event_union.is_empty() {
                http_union.to_string()
            } else {
                event_union.to_string()
            };
            assert_eq!(op_union, bridge_union, "{name}: 与 /bridge 取 union 一致");
            assert_eq!(
                verdict,
                agent.can_operate(open_id, &bridge_union, is_group, OperateScope::Agent),
                "{name}: 与 /bridge 鉴权结论一致"
            );
        }
    }

    #[test]
    fn group_identity_failure_and_missing_self_mention_fail_closed() {
        let mentions = vec![summon_core::ipc::Mention {
            open_id: "ou_this_bot".into(),
            key: "@_user_1".into(),
            name: "bot".into(),
            ..Default::default()
        }];
        assert_eq!(
            group_mention_admission(true, None, &mentions),
            GroupMentionAdmission::BotIdentityUnavailable
        );
        assert_eq!(
            group_mention_admission(true, Some("ou_other_bot"), &mentions),
            GroupMentionAdmission::MissingSelfMention
        );
        assert_eq!(
            group_mention_admission(true, Some("ou_this_bot"), &mentions),
            GroupMentionAdmission::Admit
        );
        assert_eq!(
            group_mention_admission(false, None, &[]),
            GroupMentionAdmission::Admit,
            "p2p 不依赖 bot_open_id 网络查询"
        );
    }

    #[test]
    fn inbound_front_gate_denies_untrusted_senders_before_all_command_paths() {
        let agent: Agent = serde_yaml::from_str(
            "name: bot\nfeishu:\n  allowed_users: [ou_owner]\nruntime:\n  cli: cursor\n",
        )
        .unwrap();
        assert_eq!(
            inbound_front_admission(&agent, false, "ou_intruder", "", false),
            InboundFrontAdmission::DenyTalk
        );
        assert_eq!(
            inbound_front_admission(&agent, false, "ou_owner", "", false),
            InboundFrontAdmission::Admit
        );
        assert_eq!(
            inbound_front_admission(&agent, false, "ou_owner", "", true),
            InboundFrontAdmission::NeedBotIdentity
        );

        let unsupported: Agent = serde_yaml::from_str(
            "name: bot\nfeishu:\n  allowed_users: [ou_owner]\nruntime:\n  cli: unknown\n",
        )
        .unwrap();
        assert_eq!(
            inbound_front_admission(&unsupported, false, "ou_owner", "", false),
            InboundFrontAdmission::Admit,
            "CLI capability 在 bridge slot 拦截之后单独检查"
        );
    }

    #[test]
    fn peer_trigger_budget_trips_then_cools_down_per_chat() {
        let mut limiter = PeerTriggerLimiter::default();
        let start = Instant::now();
        for offset in 0..PEER_TRIGGER_MAX_PER_WINDOW {
            assert_eq!(
                limiter.admit("oc_loop", start + Duration::from_secs(offset as u64)),
                PeerTriggerAdmission::Admit
            );
        }
        let tripped_at = start + Duration::from_secs(10);
        assert_eq!(
            limiter.admit("oc_loop", tripped_at),
            PeerTriggerAdmission::Tripped
        );
        assert_eq!(
            limiter.admit("oc_loop", tripped_at + Duration::from_secs(1)),
            PeerTriggerAdmission::CoolingDown
        );
        assert_eq!(
            limiter.admit("oc_other", tripped_at + Duration::from_secs(1)),
            PeerTriggerAdmission::Admit,
            "不同 chat 预算必须隔离"
        );
        assert_eq!(
            limiter.admit("oc_loop", tripped_at + PEER_TRIGGER_COOLDOWN),
            PeerTriggerAdmission::Admit,
            "冷却结束后 bot-to-bot 协作恢复"
        );
    }
}

#[cfg(test)]
mod chat_exit_settle_tests {
    use super::*;

    fn scope() -> FeishuStorageScope {
        FeishuStorageScope {
            key: "starlight-0123456789abcdef".into(),
            allow_legacy_import: false,
        }
    }

    fn settle(
        outbox: &crate::runtime::outbox_store::OutboxStore,
        agent: &Agent,
        exited: ChatExitEvent,
    ) {
        settle_chat_exit(
            &mut HashMap::new(),
            &mut PeerTriggerLimiter::default(),
            &crate::runtime::peers_store::PeersStore::new(agent.dir.join("peers")),
            outbox,
            &summon_core::config::builtin_messaging(),
            agent,
            &scope(),
            exited,
        );
    }

    fn offset_record(epoch_fingerprint: &str, sid: &str, bytes: &[u8]) -> String {
        let checkpoint = &bytes[bytes.len().saturating_sub(1024)..];
        let encoded: String = checkpoint
            .iter()
            .flat_map(|byte| format!("{byte:02x}").chars().collect::<Vec<_>>())
            .collect();
        format!(
            "v3 {epoch_fingerprint} {sid} {} {encoded} terminal",
            bytes.len()
        )
    }

    fn mirror_fixture() -> (Agent, tempfile::TempDir, Registry, String, String, PathBuf) {
        let (agent, tmp) = crate::test_support::agent_dir("claude", "feishu:\n  scope: scope-a\n");
        let chat_id = "oc_lag";
        let tag = chat_tag("group", chat_id);
        let session = format!("soul-{}-{tag}", agent.name);
        let chats_dir = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats_dir).unwrap();
        std::fs::write(
            chats_dir.join(format!("{session}.meta")),
            format!("chat_id={chat_id}\nchat_type=group\ntag={tag}\n"),
        )
        .unwrap();
        std::fs::create_dir_all(agent.state_dir().join("threads").join(&tag)).unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        let registry = Registry {
            agents: vec![agent.clone()],
        };
        (agent, tmp, registry, session, tag, transcript)
    }

    /// stop fence 是主动群出站的 lower bound：停机前必须 exact 追平 EOF；boot 时同一 v3 原文、
    /// 同一 live chat 与同代 checkpoint 仍成立，并且 EOF 真正增长，才批准一次补发。
    #[test]
    fn mirror_catchup_requires_exact_stop_fence_and_boot_growth() {
        let (agent, _tmp, registry, session, tag, transcript) = mirror_fixture();
        let prefix = b"already delivered\n";
        std::fs::write(&transcript, prefix).unwrap();
        let epoch = FeishuEpochIdentity::capture(&agent).fingerprint();
        let raw = offset_record(&epoch, "sid-stop", prefix);
        let offset_path = agent
            .state_dir()
            .join("threads")
            .join(&tag)
            .join("mirror_offset");
        std::fs::write(
            &offset_path,
            offset_record("different-epoch", "sid-stop", prefix),
        )
        .unwrap();
        assert!(
            capture_mirror_stop_fences_with(
                &registry,
                |_| Some(epoch.clone()),
                |_| true,
                |_, _, _| Some(transcript.clone()),
                crate::capture::capture_mirror_eof_fence,
                || 1_000,
            )
            .is_empty(),
            "A 代 mirror offset 不能被 B 代 daemon 重新签成停机 fence"
        );
        std::fs::write(&offset_path, &raw).unwrap();

        let fences = capture_mirror_stop_fences_with(
            &registry,
            |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
            |candidate| candidate == session,
            |_, _, _| Some(transcript.clone()),
            crate::capture::capture_mirror_eof_fence,
            || 1_000,
        );
        assert_eq!(fences.len(), 1, "live chat 都要签 fence 记下停机 EOF");
        let clean = CleanShutdownRecordV2 {
            version: 2,
            mirror_fences: fences,
            stopped_at_secs: 1_700_000_000,
        };

        // 停机期还没新字节 → 照样批准。旧判据在这里 return empty,于是「停机时正在跑的那一轮」
        // 跑完后没有任何 watcher 在听 —— 那正是要修的洞。批准只是挂上 watcher;发不发由
        // `catchup_floor` 在轮收尾时按终点再判一次。
        assert!(
            plan_mirror_catchup_with(
                &registry,
                &clean,
                |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
                |_| true,
                |_, _, _| Some(transcript.clone()),
                crate::capture::mirror_eof_fence_still_valid,
                |_, _| false,
                1_000,
            )
            .contains_key(&(agent.name.clone(), tag.clone())),
            "停机期暂无新字节也要挂上 watcher,否则那一轮跑完就没人接了"
        );

        std::fs::write(
            &transcript,
            [prefix.as_slice(), b"downtime reply\n"].concat(),
        )
        .unwrap();
        assert!(plan_mirror_catchup_with(
            &registry,
            &clean,
            |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
            |_| true,
            |_, _, _| Some(transcript.clone()),
            crate::capture::mirror_eof_fence_still_valid,
            |_, _| false,
            1_000,
        )
        .contains_key(&(agent.name.clone(), tag)));

        let mut drifted_agent = agent.clone();
        drifted_agent.feishu.as_mut().unwrap().scope = "scope-b".to_string();
        let drifted_registry = Registry {
            agents: vec![drifted_agent],
        };
        assert!(
            plan_mirror_catchup_with(
                &drifted_registry,
                &clean,
                |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
                |_| true,
                |_, _, _| Some(transcript.clone()),
                crate::capture::mirror_eof_fence_still_valid,
                |_, _| false,
                1_000,
            )
            .is_empty(),
            "停机后飞书 app/env/secret/scope 身份漂移时不能向新 epoch 主动补旧内容"
        );

        assert!(
            plan_mirror_catchup_with(
                &registry,
                &clean,
                |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
                |_| false,
                |_, _, _| Some(transcript.clone()),
                crate::capture::mirror_eof_fence_still_valid,
                |_, _| false,
                1_000,
            )
            .is_empty(),
            "tmux 已死时不得主动群出站"
        );

        std::fs::write(&offset_path, format!("{raw}\n")).unwrap();
        assert!(
            plan_mirror_catchup_with(
                &registry,
                &clean,
                |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
                |_| true,
                |_, _, _| Some(transcript.clone()),
                crate::capture::mirror_eof_fence_still_valid,
                |_, _| false,
                1_000,
            )
            .is_empty(),
            "stop 后 offset 原文有任何漂移都必须拒绝"
        );
    }

    /// 停机时 offset < EOF(有旧欠账,或更常见的:**正有一轮在跑**)照样签 fence —— 这正是
    /// 2026-08-13 修的那个洞。分界线记在 `stop_eof` 上,旧欠账靠它被挡在补发之外,不需要
    /// 「签之前必须先追平」这个前提(那前提会让「停机时在跑的轮」永远补不回来)。
    #[test]
    fn mirror_stop_fence_signs_even_with_preexisting_backlog() {
        let (agent, _tmp, registry, _session, tag, transcript) = mirror_fixture();
        let delivered = b"delivered\n";
        let epoch = FeishuEpochIdentity::capture(&agent).fingerprint();
        std::fs::write(
            &transcript,
            [delivered.as_slice(), b"old backlog\n"].concat(),
        )
        .unwrap();
        std::fs::write(
            agent
                .state_dir()
                .join("threads")
                .join(tag)
                .join("mirror_offset"),
            offset_record(&epoch, "sid-stop", delivered),
        )
        .unwrap();
        assert!(
            !capture_mirror_stop_fences_with(
                &registry,
                |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
                |_| true,
                |_, _, _| Some(transcript.clone()),
                crate::capture::capture_mirror_eof_fence,
                || 1_000,
            )
            .is_empty(),
            "offset<EOF 也要签:分界线是 stop_eof,不是 offset"
        );
    }

    /// **这条钉的是 2026-08-13 那个真实故障**:agent 自己跑 `deploy.sh`,停机那一刻它这一轮还在跑,
    /// 于是 `offset < EOF` → 旧判据不签 fence → 新 daemon 补不了 → 那一轮的汇报永久丢失。
    ///
    /// 新语义:照签,并记下停机那一刻的 EOF 当分界线。停机后才收尾的轮(终点 > stop_eof)可以补,
    /// 停机前就躺在那儿的旧欠账(终点 ≤ stop_eof)仍然不补 —— 旧 main 发没发不可证明。
    #[test]
    fn mirror_catchup_covers_the_turn_that_was_running_at_shutdown() {
        let (agent, _tmp, registry, session, tag, transcript) = mirror_fixture();
        let delivered = b"delivered turn\n";
        // 停机那一刻:offset 停在上一轮终点,而文件里已经堆了本轮跑到一半的内容。
        let mid_turn = [delivered.as_slice(), b"half of the running turn\n"].concat();
        std::fs::write(&transcript, &mid_turn).unwrap();
        let epoch = FeishuEpochIdentity::capture(&agent).fingerprint();
        std::fs::write(
            agent
                .state_dir()
                .join("threads")
                .join(&tag)
                .join("mirror_offset"),
            offset_record(&epoch, "sid-stop", delivered),
        )
        .unwrap();

        let fences = capture_mirror_stop_fences_with(
            &registry,
            |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
            |candidate| candidate == session,
            |_, _, _| Some(transcript.clone()),
            crate::capture::capture_mirror_eof_fence,
            || 1_000,
        );
        assert_eq!(
            fences.len(),
            1,
            "轮跑到一半也必须签 fence,否则这轮永远补不回来"
        );
        assert_eq!(
            fences[0].stop_eof,
            mid_turn.len() as u64,
            "分界线是停机那一刻的 EOF,不是 offset"
        );
        let clean = CleanShutdownRecordV2 {
            version: 2,
            mirror_fences: fences,
            stopped_at_secs: 1_700_000_000,
        };

        // 停机后那一轮跑完,终态落盘。
        std::fs::write(
            &transcript,
            [mid_turn.as_slice(), b"the reply nobody delivered\n"].concat(),
        )
        .unwrap();
        let approved = plan_mirror_catchup_with(
            &registry,
            &clean,
            |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
            |_| true,
            |_, _, _| Some(transcript.clone()),
            crate::capture::mirror_eof_fence_still_valid,
            |_, _| false,
            1_000 + 30,
        );
        assert_eq!(
            approved.get(&(agent.name.clone(), tag.clone())).copied(),
            Some(mid_turn.len() as u64),
            "该批准补发,且 floor 就是停机 EOF"
        );

        // 新鲜度闸:同一份盘上事实,停机过去太久就不补(daemon 停了三天再起 ≠ 补发窗口)。
        let stale = plan_mirror_catchup_with(
            &registry,
            &clean,
            |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
            |_| true,
            |_, _, _| Some(transcript.clone()),
            crate::capture::mirror_eof_fence_still_valid,
            |_, _| false,
            1_000 + MIRROR_CATCHUP_MAX_STALENESS_SECS + 1,
        );
        assert!(stale.is_empty(), "停机太久不补,防往群里倒历史");

        // 时钟回拨也当「证不了」处理。
        let skewed = plan_mirror_catchup_with(
            &registry,
            &clean,
            |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
            |_| true,
            |_, _, _| Some(transcript.clone()),
            crate::capture::mirror_eof_fence_still_valid,
            |_, _| false,
            999,
        );
        assert!(skewed.is_empty(), "now < stopped_at 证不了新鲜,不补");

        // 分界线验真:文件被换代/truncate 后,stop_eof 右侧不再是同一份内容 → 不补。
        std::fs::write(&transcript, b"a different generation entirely\n").unwrap();
        let replaced = plan_mirror_catchup_with(
            &registry,
            &clean,
            |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
            |_| true,
            |_, _, _| Some(transcript.clone()),
            crate::capture::mirror_eof_fence_still_valid,
            |_, _| false,
            1_000 + 30,
        );
        assert!(replaced.is_empty(), "EOF 分界线复验不过就不补");
    }

    /// 旧格式记录(没有 stop_eof/stopped_at)必须自动落在「不补」那边 —— 跨版本升级时,
    /// 上一代 daemon 写的 fence 反序列化后这两个字段是 0,不能被当成「停机于 1970 年」放行。
    #[test]
    fn legacy_fence_without_eof_marker_never_authorizes_catchup() {
        let (agent, _tmp, registry, _session, tag, transcript) = mirror_fixture();
        let delivered = b"delivered\n";
        std::fs::write(&transcript, [delivered.as_slice(), b"more\n"].concat()).unwrap();
        let epoch = FeishuEpochIdentity::capture(&agent).fingerprint();
        let raw = offset_record(&epoch, "sid-stop", delivered);
        std::fs::write(
            agent
                .state_dir()
                .join("threads")
                .join(&tag)
                .join("mirror_offset"),
            &raw,
        )
        .unwrap();
        // 手工构造一份「旧 daemon 写的」fence:serde default 会把三个新字段填 0/空串。
        let legacy: MirrorStopFence = serde_json::from_str(&format!(
            r#"{{"agent":"{}","feishu_epoch_fingerprint":"{epoch}","session":"soul-x",
                 "tag":"{tag}","chat_id":"oc_lag","chat_type":"group","offset_record":{}}}"#,
            agent.name,
            serde_json::to_string(&raw).unwrap()
        ))
        .unwrap();
        assert_eq!(legacy.stop_eof, 0);
        assert_eq!(legacy.stopped_at_secs, 0);
        let clean = CleanShutdownRecordV2 {
            version: 2,
            mirror_fences: vec![legacy],
            stopped_at_secs: 1_700_000_000,
        };
        assert!(
            plan_mirror_catchup_with(
                &registry,
                &clean,
                |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
                |_| true,
                |_, _, _| Some(transcript.clone()),
                crate::capture::mirror_eof_fence_still_valid,
                |_, _| false,
                1_700_000_000,
            )
            .is_empty(),
            "旧格式 fence 无分界线可验,必须 fail-closed"
        );
    }

    #[test]
    fn mirror_scan_still_revalidates_approved_disk_facts() {
        let (agent, _tmp, _registry, session, tag, _transcript) = mirror_fixture();
        assert!(
            mirror_behind_chats_on_disk_with(&agent, |_| true).is_empty(),
            "获批 key 缺 mirror_offset 时仍须 fail-closed"
        );
        std::fs::write(
            agent
                .state_dir()
                .join("threads")
                .join(tag)
                .join("mirror_offset"),
            "garbage",
        )
        .unwrap();
        assert!(
            mirror_behind_chats_on_disk_with(&agent, |_| true).is_empty(),
            "坏 offset 不能仅凭批准 key 拉起"
        );

        std::fs::write(
            agent
                .state_dir()
                .join("runtime/chats")
                .join(format!("{session}.meta")),
            "chat_id=oc_other\nchat_type=group\ntag=not-derived-from-chat\n",
        )
        .unwrap();
        assert!(
            mirror_chat_entries(&agent).is_empty(),
            "meta 的 chat_id/chat_type/tag 不自洽时不能决定主动出站目标"
        );

        let (codex, _tmp2) = crate::test_support::agent_dir("codex", "");
        assert!(
            mirror_behind_chats_on_disk_with(&codex, |_| true).is_empty(),
            "只有 Transcript 有会话镜像"
        );
    }

    /// 主路径要接管的 chat,镜像补发必须让路:两条路盯着同一段停机窗口的字节,都放行就是
    /// 同一轮内容发两遍。判据两处共用 `reattach_anchor_on_disk`,这里把真判据接进来验一遍。
    #[test]
    fn mirror_catchup_steps_aside_for_a_chat_the_main_path_will_reattach() {
        let (agent, tmp, registry, session, tag, transcript) = mirror_fixture();
        let prefix = b"already delivered\n";
        std::fs::write(&transcript, prefix).unwrap();
        let epoch = FeishuEpochIdentity::capture(&agent).fingerprint();
        std::fs::write(
            agent
                .state_dir()
                .join("threads")
                .join(&tag)
                .join("mirror_offset"),
            offset_record(&epoch, "sid-run", prefix),
        )
        .unwrap();
        let (now, stopped) = (1_700_000_000u64, 1_700_000_000u64 - 60);
        let clean = CleanShutdownRecordV2 {
            version: 2,
            mirror_fences: capture_mirror_stop_fences_with(
                &registry,
                |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
                |candidate| candidate == session,
                |_, _, _| Some(transcript.clone()),
                crate::capture::capture_mirror_eof_fence,
                || stopped,
            ),
            stopped_at_secs: 1_700_000_000,
        };
        let key = (agent.name.clone(), tag.clone());

        assert!(
            plan_mirror_catchup_with(
                &registry,
                &clean,
                |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
                |_| true,
                |_, _, _| Some(transcript.clone()),
                crate::capture::mirror_eof_fence_still_valid,
                |_, _| false,
                now,
            )
            .contains_key(&key),
            "没有接管时这条 fence 本该获批,否则下面的排除断言什么都证明不了"
        );

        write_native_sid(&agent, &session, "sid-run");
        crate::runtime::inflight::write_turn_anchor(
            &agent.state_dir(),
            &tag,
            &anchor_for("oc_lag", "group"),
        )
        .unwrap();
        let wal = started_wal(tmp.path(), &agent, "oc_lag", "group");
        assert!(
            plan_mirror_catchup_with(
                &registry,
                &clean,
                |candidate| Some(FeishuEpochIdentity::capture(candidate).fingerprint()),
                |_| true,
                |_, _, _| Some(transcript.clone()),
                crate::capture::mirror_eof_fence_still_valid,
                |candidate, fence_tag| reattach_anchor_on_disk_with(
                    candidate,
                    "scope-a",
                    &wal,
                    fence_tag,
                    stopped,
                    now,
                    |_| true
                )
                .is_some(),
                now,
            )
            .is_empty(),
            "主路径持有投递权时镜像不得再补发同一段字节"
        );
    }

    /// 盘上锚点认领的那几条消息,轮此刻还在 tmux 里跑 —— 接管任务经任务通道晚于本扫描到达。
    /// 一起收了就是「👀 先变成 ✅,回答再慢慢出来」;更糟的是记录一删,接管失败那条路
    /// (`retract_onit` 按 `find_by_message` 撤)就再也撤不掉上一代贴的那个 👀。
    #[test]
    fn orphan_reaction_scan_leaves_the_messages_a_running_turn_still_claims() {
        let (agent, _tmp, _session, _tag, _wal) = reattach_fixture();
        let store = crate::runtime::reaction_store::ReactionStore::for_agent(&agent.state_dir());
        for message_id in ["om_1", "om_orphan"] {
            store.enqueue(&crate::runtime::reaction_store::ReactionRecord {
                message_id: message_id.into(),
                anchor_message_id: None,
                delivered_at_ms: 1,
                onit_reaction_id: format!("rid-{message_id}"),
                chat_id: "oc_run".into(),
                chat_type: "group".into(),
                agent_reaction_id: None,
            });
        }
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);

        recover_orphan_reactions(&agent, &fc, "oc_run", "group");

        assert!(
            store.find_by_message("om_1").is_some(),
            "锚点认领的消息不是孤儿:它的轮还在跑"
        );
        assert!(
            store.find_by_message("om_orphan").is_none(),
            "锚点之外的 working 记录仍要收尾,否则 👀 永远卡着"
        );
    }

    /// 接管判据的盘上夹具:一个 claude agent + 一条 live chat(`.meta`)+ 盘上原生 sid
    /// (`.session`)+ 一枚锚点 + WAL 里一条已 started 的记录。
    #[allow(clippy::type_complexity)]
    fn reattach_fixture() -> (
        Agent,
        tempfile::TempDir,
        String,
        String,
        crate::runtime::inbox_wal::InboxWal,
    ) {
        let (agent, tmp) = crate::test_support::agent_dir("claude", "feishu:\n  scope: scope-a\n");
        let (chat_id, chat_type) = ("oc_run", "group");
        let tag = chat_tag(chat_type, chat_id);
        let session = format!("soul-{}-{tag}", agent.name);
        let chats_dir = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats_dir).unwrap();
        std::fs::write(
            chats_dir.join(format!("{session}.meta")),
            format!("chat_id={chat_id}\nchat_type={chat_type}\ntag={tag}\n"),
        )
        .unwrap();
        write_native_sid(&agent, &session, "sid-run");
        crate::runtime::inflight::write_turn_anchor(
            &agent.state_dir(),
            &tag,
            &anchor_for(chat_id, chat_type),
        )
        .unwrap();
        let wal = started_wal(tmp.path(), &agent, chat_id, chat_type);
        (agent, tmp, session, tag, wal)
    }

    #[test]
    fn cursor_idle_context_uses_only_supported_snapshots() {
        use summon_core::context_budget::ContextBudget;
        for (capture, window, expected) in [
            ("cli", None, None),
            ("cli", Some(2_000_000), None),
            ("rpc", None, None),
            ("rpc", Some(2_000_000), Some(1_900_000)),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut agent: Agent = serde_yaml::from_str(&format!(
                "name: cursor-idle-context-test\nruntime:\n  cli: cursor\n  capture: {capture}\n"
            ))
            .unwrap();
            agent.dir = tmp.path().join("agents").join(&agent.name);
            let session = "soul-cursor-idle-context-test";
            let tag = "g-test";
            crate::session::resume_store::write(
                &agent,
                session,
                &agent.resume_cli_kind(),
                "sid-context",
                None,
                None,
            );
            assert_eq!(
                crate::session::resume_store::read_id(&agent, session, &agent.resume_cli_kind())
                    .as_deref(),
                Some("sid-context")
            );
            crate::runtime::status_snapshot::record_context(
                &agent.state_dir(),
                tag,
                "sid-context",
                ContextBudget::from_tokens(1_900_000, window),
            )
            .unwrap();
            let context = latest_idle_context(&agent, tag, session);
            assert_eq!(
                context.used_tokens, expected,
                "capture={capture}, window={window:?}"
            );
            if capture == "cli" {
                assert_eq!(
                    super::super::cards::current_session_context_budget(&agent, tag, session),
                    ContextBudget::default()
                );
            }
        }
    }

    #[test]
    fn cursor_idle_context_reads_native_cli_store_after_daemon_restart() {
        use summon_core::context_budget::ContextBudget;
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("cursor");
        let mut agent: Agent = serde_yaml::from_str(&format!(
            "name: cursor-idle-native-test\nruntime:\n  cli: cursor\n  capture: cli\n  env:\n    CURSOR_CONFIG_DIR: '{}'\n",
            config.display()
        ))
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        let session = "soul-cursor-idle-native-test";
        let tag = "g-native";
        crate::session::resume_store::write(
            &agent,
            session,
            &agent.resume_cli_kind(),
            "sid-native",
            None,
            None,
        );
        crate::runtime::status_snapshot::record_context(
            &agent.state_dir(),
            tag,
            "sid-native",
            ContextBudget::from_tokens(850_000, Some(1_000_000)),
        )
        .unwrap();
        let db = crate::capture::cursor_context::write_test_snapshot(
            &config.join("chats"),
            "sid-native",
            24_854,
            1_000_000,
        );
        assert_eq!(
            latest_idle_context(&agent, tag, session),
            ContextBudget::from_tokens(24_854, Some(1_000_000))
        );
        std::fs::remove_file(db).unwrap();
        assert_eq!(
            latest_idle_context(&agent, tag, session),
            ContextBudget::default()
        );
    }

    fn write_native_sid(agent: &Agent, session: &str, sid: &str) {
        std::fs::write(
            agent
                .state_dir()
                .join("runtime")
                .join("chats")
                .join(format!("{session}.session")),
            summon_core::session_id::format_record("claude", sid, None, None),
        )
        .unwrap();
    }

    fn anchor_for(chat_id: &str, chat_type: &str) -> summon_core::TurnAnchor {
        summon_core::TurnAnchor {
            agent_name: "test".into(),
            delivery_scope: "scope-a".into(),
            chat_id: chat_id.into(),
            chat_type: chat_type.into(),
            inbox_message_ids: vec!["om_1".into()],
            native_session_id: "sid-run".into(),
            marker: "mk-1".into(),
            prompt_head: "上一代那条 prompt 的开头".into(),
            inject_offset: 100,
            consumed_boundary: None,
            injected_at_ms: 1_000,
            cot: None,
            credential: None,
        }
    }

    fn started_wal(
        root: &std::path::Path,
        agent: &Agent,
        chat_id: &str,
        chat_type: &str,
    ) -> crate::runtime::inbox_wal::InboxWal {
        let wal = crate::runtime::inbox_wal::InboxWal::new(root.join("inbox"));
        let inbound = Inbound {
            anchor_message_id: None,
            chat_id: chat_id.into(),
            chat_type: Some(chat_type.into()),
            text: "上一代已经注入过的那条".into(),
            sender: "ou_1".into(),
            sender_union_id: String::new(),
            sender_type: "user".into(),
            message_id: "om_1".into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms: 1,
            replayed: false,
            reattach: false,
        };
        wal.append(&agent.name, "scope-a", &inbound).unwrap();
        wal.mark_started(&agent.name, "scope-a", "om_1").unwrap();
        wal
    }

    /// 接管的五条前置(身份 / 来源 / sid / tmux 存活 / 新鲜度)全部证得才返回锚点。
    /// 判松的代价不是「少接一轮」而是双执行,所以每一条都单独钉一遍。
    #[test]
    fn reattach_anchor_on_disk_needs_every_proof() {
        let (agent, _tmp, session, tag, wal) = reattach_fixture();
        let now = 1_700_000_000;
        let stopped = now - 60;
        assert_eq!(
            reattach_anchor_on_disk_with(&agent, "scope-a", &wal, &tag, stopped, now, |s| s
                == session),
            Some(anchor_for("oc_run", "group")),
            "五条全部证得"
        );

        assert!(
            reattach_anchor_on_disk_with(&agent, "scope-b", &wal, &tag, stopped, now, |_| true)
                .is_none(),
            "飞书身份换了就不是同一条投递上下文"
        );
        assert!(
            reattach_anchor_on_disk_with(&agent, "scope-a", &wal, &tag, stopped, now, |_| false)
                .is_none(),
            "tmux 会话已不在:那一轮没人在跑,接管无从谈起"
        );
        assert!(
            reattach_anchor_on_disk_with(
                &agent,
                "scope-a",
                &wal,
                &tag,
                stopped - MIRROR_CATCHUP_MAX_STALENESS_SECS,
                now,
                |_| true
            )
            .is_none(),
            "停机太久,tmux 里跑的早已不是停机那一刻那一轮"
        );

        write_native_sid(&agent, &session, "sid-other");
        assert!(
            reattach_anchor_on_disk_with(&agent, "scope-a", &wal, &tag, stopped, now, |_| true)
                .is_none(),
            "盘上 sid 变了说明这条 tmux 已换过代,接管进去等于往另一条上下文里贴输出"
        );
    }

    /// CoT keep 集不许挂在 mirror fence 上。签 fence 要求盘上已有一枚 TerminalBoundary 的
    /// `mirror_offset`,首轮就被重启的会话一条都没有 —— 而它恰恰是最该接管的那种。挂上去的
    /// 后果是启动收口把它的 CoT 关掉,接管轮只能退回新载体。
    #[test]
    fn reattach_plan_covers_a_chat_that_never_committed_a_mirror_offset() {
        let (agent, _tmp, session, tag, wal) = reattach_fixture();
        // 这个 chat 从没提交过 mirror_offset,所以上一代一条 fence 都签不出来。
        assert!(!agent
            .state_dir()
            .join("threads")
            .join(&tag)
            .join("mirror_offset")
            .exists());
        let mut anchor = anchor_for("oc_run", "group");
        anchor.cot = Some(summon_core::TurnAnchorCot {
            cot_id: "cot_running".into(),
            message_id: "om_cot".into(),
        });
        crate::runtime::inflight::write_turn_anchor(&agent.state_dir(), &tag, &anchor).unwrap();
        let registry = Registry {
            agents: vec![agent.clone()],
        };
        let now = 1_700_000_000;

        let plan = plan_reattach_with(
            &registry,
            now - 60,
            now,
            |_| Some("scope-a".to_string()),
            |s| s == session,
            &wal,
        );

        assert_eq!(
            plan.get(&(agent.name.clone(), tag.clone())),
            Some(&Some("cot_running".to_string())),
            "无 fence 但锚点全部证得:既要让路,也要保住那条 CoT"
        );

        // 判据本身不放水:tmux 不在就不接管,与 fence 有无无关。
        assert!(plan_reattach_with(
            &registry,
            now - 60,
            now,
            |_| Some("scope-a".to_string()),
            |_| false,
            &wal,
        )
        .is_empty());
    }

    /// 子 agent 观测的登记要绑在 chat 线程上:guard 必须在 spawn 出的闭包里、`chat_thread(` 之前
    /// 绑到具名变量,活过整个线程。写成 `let _ =` 会当场注销,子 agent 在跑的会话再也不会报成
    /// Working,而登记表自己的单测照样全绿。
    #[test]
    fn chat_thread_keeps_its_subagent_watch_for_its_whole_life() {
        let src = include_str!("dispatcher.rs");
        let production = src
            .split("\n#[cfg(test)]\nmod ")
            .next()
            .expect("dispatcher 生产源");
        let start = production
            .find("pub(super) fn spawn_chat_thread(")
            .expect("spawn_chat_thread");
        let body = &production[start..];
        let spawn = body.find("thread::spawn(move || {").expect("线程闭包");
        let watch = body
            .find("let _subagents = crate::observe::runtime_status::watch_subagents(")
            .expect("子 agent 观测登记");
        let run = body
            .find("        chat_thread(\n")
            .expect("chat_thread 调用");
        assert!(
            spawn < watch && watch < run,
            "登记要在线程闭包里、跑 chat_thread 之前"
        );
        assert!(body[watch..run].contains("&thread_subagent_state"));
    }

    /// keep 集的**定义域**只能是全量扫描那一份。把 `record_reattaching_cots` 挪回 fence 循环
    /// 里(或在那里再补一次),没有 mirror fence 的 chat 就又进不了 keep 集 —— 而这条线只有
    /// 「首轮就被重启」那一次走得到,漏了不会有任何别的测试变红,只在真机上表现为「接管成功
    /// 但另开了一条 CoT」。
    #[test]
    fn the_cot_keep_set_is_recorded_from_the_full_scan_not_the_fence_loop() {
        let src = include_str!("dispatcher.rs");
        let production = src.split("#[cfg(test)]").next().expect("dispatcher 生产源");
        let slice = |from: &str, to: &str| {
            let start = production.find(from).unwrap_or_else(|| panic!("{from}"));
            let end = production[start..]
                .find(to)
                .unwrap_or_else(|| panic!("{to}"));
            &production[start..start + end]
        };

        let planner = slice(
            "pub(super) fn plan_mirror_catchup(",
            "fn plan_reattach_with(",
        );
        assert!(
            planner.contains("let reattaching = plan_reattach_with(")
                && planner.contains("record_reattaching_cots("),
            "keep 集只能写自那次全量扫描的结果"
        );

        let fence_loop = slice("fn plan_mirror_catchup_with<", "\n/// 启动扫描:");
        assert!(
            !fence_loop.contains("record_reattaching_cots("),
            "写入点不得落进 fence 循环:没签出 fence 的 chat 会因此进不了 keep 集"
        );
        assert!(
            !fence_loop.contains("reattach_anchor_on_disk"),
            "fence 循环只查全量扫描算好的结果,不许自己再判一次接管"
        );
    }

    /// 免 @ 验证结论要交到每一处「群内命令是否说给本 bot」的判断:漏一处,那个入口在免 @ 群里
    /// 就静默无反应。这几处多在 chat 线程里才见效果,单测碰不到接线,所以钉住源码。
    #[test]
    fn dispatcher_hands_the_mentionless_proof_to_every_addressed_command() {
        let src = include_str!("dispatcher.rs");
        let production = src
            .split("\n#[cfg(test)]\nmod ")
            .next()
            .expect("dispatcher 生产源");
        let calls: Vec<&str> = production
            .match_indices("command_is_addressed(")
            .map(|(at, _)| &production[at..at + production[at..].find(';').unwrap_or(0)])
            .filter(|call| !call.starts_with("command_is_addressed(\n    is_group: bool"))
            .collect();
        assert_eq!(
            calls.len(),
            6,
            "/delegate、/mentionless、/dash、/summon-export、/term 与 provider 斜杠命令"
        );
        for call in calls {
            let args: String = call
                .lines()
                .take_while(|line| !line.trim_start().starts_with(')'))
                .collect();
            assert!(args.contains("mentionless_admitted"), "{call}");
        }
        assert_eq!(
            production.matches("provider_slash_is_addressed(").count(),
            2,
            "除定义与 command_is_addressed 内部外,不得绕开免 @ 结论直接判开头 @"
        );
        let bridge = production
            .find("if let Some(command) = summon_core::ipc::bridge_command(")
            .expect("入站的 /bridge 分支");
        let bridge_end = production[bridge..].find(") {").expect("/bridge 调用收口");
        assert!(production[bridge..bridge + bridge_end].contains("mentionless_admitted,"));
        let task = production
            .find("let (text, raw, addressed) = task_text_and_addressing(")
            .expect("任务正文与 addressed 计算点");
        let task_end = production[task..].find(");").expect("调用收口");
        let task_call = &production[task..task + task_end];
        assert!(
            task_call.contains("slash_addressed,") && task_call.contains("mentionless_admitted,")
        );
        let slash = production
            .find("let slash_addressed = command_is_addressed(")
            .expect("provider 斜杠命令的定向判断");
        assert!(slash < task);
    }

    /// `reattach` 必须原样透传进 ChatTask。丢了它,接管入站会被当成普通轮重新注入 —— 同一句话
    /// 跑两遍;而这条线只有 daemon 换代那一次走得到,漏了不会有任何别的测试变红。
    #[test]
    fn dispatcher_hands_the_reattach_flag_to_the_chat_task() {
        let src = include_str!("dispatcher.rs");
        let production = src.split("#[cfg(test)]").next().expect("dispatcher 生产源");
        let start = production
            .find("let task = ChatTask {")
            .expect("普通入站的 ChatTask 组装点");
        let end = production[start..].find("};").expect("ChatTask 字面量收口");
        assert!(
            production[start..start + end].contains("reattach: msg.reattach,"),
            "普通入站组装 ChatTask 时必须原样带上 reattach"
        );
    }

    /// `/handoff` 的改写要在算出 slash 准入之后(只有获准的才改写),解析出的命令原样带进
    /// ChatTask,且这一轮的指令不进卡片底注。
    #[test]
    fn dispatcher_hands_the_admitted_handoff_to_the_chat_task() {
        let src = include_str!("dispatcher.rs");
        let production = src.split("#[cfg(test)]").next().expect("dispatcher 生产源");
        let admission = production
            .find("let slash_admitted = if kind.is_slash() && addressed {")
            .expect("slash 准入计算点");
        let rewrite = production
            .find("let (text, kind, handoff) = rewrite_handoff(")
            .expect("普通入站必须改写 /handoff");
        assert!(admission < rewrite, "改写要在算出 slash 准入之后");
        let call = &production[rewrite..rewrite + 300];
        assert!(call.contains("slash_admitted,") && call.contains("&msg.text,"));
        let start = production
            .find("let task = ChatTask {")
            .expect("普通入站的 ChatTask 组装点");
        let end = production[start..].find("};").expect("ChatTask 字面量收口");
        let literal = &production[start..start + end];
        assert!(literal.contains("hide_context_hint: handoff.is_some(),"));
        assert!(literal.contains("handoff,"));
        let between = &production[rewrite + 1..start];
        assert!(
            !between.contains("let handoff") && !between.contains("handoff ="),
            "改写结果到 ChatTask 之间不得改动交接命令"
        );
    }

    /// `/queue` 的改写结果要真的进 ChatTask:改写发生在算 slash 准入之前(改写后它是普通对话,
    /// 不走 operate-gate),排队标记原样带进任务。只测 `rewrite_queued` 本身的话,入站这一段
    /// 漏调或漏传都不会有测试变红。
    #[test]
    fn dispatcher_hands_the_queued_flag_to_the_chat_task() {
        let src = include_str!("dispatcher.rs");
        let production = src.split("#[cfg(test)]").next().expect("dispatcher 生产源");
        let rewrite = production
            .find("let (text, kind, queued) = rewrite_queued(text, kind, raw, &conversation);")
            .expect("普通入站必须改写 /queue");
        let admission = production
            .find("let slash_admitted = if kind.is_slash() && addressed {")
            .expect("slash 准入计算点");
        assert!(rewrite < admission, "改写要在算 slash 准入之前");
        let start = production
            .find("let task = ChatTask {")
            .expect("普通入站的 ChatTask 组装点");
        let end = production[start..].find("};").expect("ChatTask 字面量收口");
        assert!(production[start..start + end].contains("queued,"));
        let between = &production[rewrite..start];
        assert!(
            !between.contains("let queued") && !between.contains("queued ="),
            "改写结果到 ChatTask 之间不得改动排队标记"
        );
    }

    /// 新鲜度只看 daemon 级停机时刻,与这个 chat 有没有 mirror fence 无关 —— 首轮就被重启的
    /// 会话一条 fence 都签不出来,那恰恰是最该接管的一类。
    #[test]
    fn reattach_needs_no_mirror_fence_only_a_fresh_daemon_stop() {
        let (agent, _tmp, session, tag, wal) = reattach_fixture();
        let now = 1_700_000_000;
        let fenceless = super::super::CleanShutdownRecordV2 {
            version: 2,
            mirror_fences: Vec::new(),
            stopped_at_secs: now - 60,
        };
        assert!(
            reattach_anchor_on_disk_with(
                &agent,
                "scope-a",
                &wal,
                &tag,
                fenceless.stopped_at_secs,
                now,
                |s| s == session
            )
            .is_some(),
            "没有 mirror fence 不该妨碍接管"
        );
        let legacy = super::super::CleanShutdownRecordV2 {
            version: 2,
            mirror_fences: Vec::new(),
            stopped_at_secs: 0,
        };
        assert!(
            reattach_anchor_on_disk_with(
                &agent,
                "scope-a",
                &wal,
                &tag,
                legacy.stopped_at_secs,
                now,
                |_| true
            )
            .is_none(),
            "拿不到停机时刻就证不了停了多久,不接管"
        );
    }

    /// 同一个 chat 可以有多份 `.meta`(裸名 + 带 tag 的历史残留)。判据是「谁的 sid 与锚点相符
    /// 且还活着」,不是「谁排在前面」—— 赌排序会让当轮那条会话被排在后面时静默不接管。
    #[test]
    fn reattach_picks_the_session_whose_sid_matches_not_the_first_meta() {
        let (agent, _tmp, session, tag, wal) = reattach_fixture();
        let chats_dir = agent.state_dir().join("runtime").join("chats");
        // 裸名排在带 tag 那条前面(mirror_chat_entries 按会话名排序),且 sid 是另一条会话的。
        let stale = format!("soul-{}", agent.name);
        assert!(stale < session, "夹具前提:裸名必须排在前面");
        std::fs::write(
            chats_dir.join(format!("{stale}.meta")),
            format!("chat_id=oc_run\nchat_type=group\ntag={tag}\n"),
        )
        .unwrap();
        write_native_sid(&agent, &stale, "sid-of-another-session");

        let now = 1_700_000_000;
        assert_eq!(
            reattach_anchor_on_disk_with(&agent, "scope-a", &wal, &tag, now - 60, now, |s| s
                == session),
            Some(anchor_for("oc_run", "group")),
            "sid 相符且存活的那条才是当轮会话"
        );
        assert!(
            reattach_anchor_on_disk_with(&agent, "scope-a", &wal, &tag, now - 60, now, |s| s
                == stale)
            .is_none(),
            "sid 相符的那条不在跑时,不能拿另一条顶包"
        );
    }

    /// 锚点认领的 id 集合必须与 WAL 里本 chat 的 started 全集**精确相等**:多一条说明有记录
    /// 没跟上(接管轮不会替它收尾),少一条说明那一轮的归属证不全。
    #[test]
    fn reattach_anchor_on_disk_rejects_inbox_source_drift() {
        let (agent, _tmp, _session, tag, wal) = reattach_fixture();
        let mut drifted = anchor_for("oc_run", "group");
        drifted.inbox_message_ids.push("om_2".into());
        crate::runtime::inflight::write_turn_anchor(&agent.state_dir(), &tag, &drifted).unwrap();
        assert!(reattach_anchor_on_disk_with(
            &agent,
            "scope-a",
            &wal,
            &tag,
            1_700_000_000 - 60,
            1_700_000_000,
            |_| true
        )
        .is_none());
    }

    /// 无头路有自己的 pending.json 续跑语义,锚点对它没有意义;没有锚点的 chat 同样不接管。
    #[test]
    fn reattach_anchor_on_disk_is_closed_for_headless_and_missing_anchor() {
        let (agent, _tmp, _session, tag, wal) = reattach_fixture();
        let (codex, _tmp2) = crate::test_support::agent_dir("codex", "");
        assert!(reattach_anchor_on_disk_with(
            &codex,
            "scope-a",
            &wal,
            &tag,
            1_700_000_000 - 60,
            1_700_000_000,
            |_| true
        )
        .is_none());

        crate::runtime::inflight::clear_turn_anchor(&agent.state_dir(), &tag);
        assert!(reattach_anchor_on_disk_with(
            &agent,
            "scope-a",
            &wal,
            &tag,
            1_700_000_000 - 60,
            1_700_000_000,
            |_| true
        )
        .is_none());
    }

    #[test]
    fn boot_mirror_catchup_respects_bridge_slot_and_fails_closed_on_store_error() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        assert!(boot_chat_allowed_by_bridge(
            &store,
            "scope",
            &agent,
            "oc_group",
            "group",
            "启动镜像预热"
        ));

        let target = summon_core::bridge::ScopedChatKey::new("scope", &agent.name, "oc_group");
        store
            .with_state(|state| {
                state.slots.insert(
                    target.storage_key(),
                    summon_core::bridge::ChatSlot::Pending {
                        ticket_id: "ticket".into(),
                        revision: 1,
                        expires_at_ms: u64::MAX,
                    },
                );
                Ok(())
            })
            .unwrap();
        assert!(
            !boot_chat_allowed_by_bridge(
                &store,
                "scope",
                &agent,
                "oc_group",
                "group",
                "启动镜像预热"
            ),
            "Bridge 占用群不能拉起本地 mirror watcher"
        );

        let corrupt_root = tempfile::tempdir().unwrap();
        let corrupt = crate::bridge::store::BridgeStore::new(corrupt_root.path().join("bridge"));
        std::fs::create_dir_all(corrupt.state_path().parent().unwrap()).unwrap();
        std::fs::write(corrupt.state_path(), "{").unwrap();
        assert!(
            !boot_chat_allowed_by_bridge(
                &corrupt,
                "scope",
                &agent,
                "oc_group",
                "group",
                "启动镜像预热"
            ),
            "Bridge state 不可读时主动群出站必须 fail-closed"
        );
    }

    #[test]
    fn boot_does_not_start_local_sessions_for_remote_bound_groups() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        crate::remote::store::RemoteStore::for_agent(&agent)
            .save(&summon_core::remote::RemoteBinding {
                schema_version: summon_core::remote::REMOTE_BINDING_SCHEMA_VERSION,
                agent: agent.name.clone(),
                scope: "scope".into(),
                chat_id: "oc_group".into(),
                chat_type: "group".into(),
                session_id: "sid-1".into(),
                session_name: "n".into(),
                surface_id: "S".into(),
                workspace_id: "W".into(),
                pid: 42,
                pid_start_seconds: 1000,
                transcript_path: "/t.jsonl".into(),
                actor_open_id: "ou".into(),
                created_at_ms: 1,
                ended: None,
            })
            .unwrap();
        assert!(
            !boot_chat_allowed_by_bridge(&store, "scope", &agent, "oc_group", "group", "启动续跑"),
            "遥控中的群不能被启动扫描拉起本地会话"
        );
        assert!(
            boot_chat_allowed_by_bridge(&store, "scope", &agent, "oc_other", "group", "启动续跑"),
            "别的群照常"
        );
    }

    /// chat worker 退出收口:`notify=true` 必须入队一条回执,且 delivery_scope 取**调用方传入的**
    /// storage scope(不是现算)——凭据换绑后旧事件绝不能被发到新 app。
    #[test]
    fn settle_with_notify_enqueues_receipt_under_given_scope() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let outbox = crate::runtime::outbox_store::OutboxStore::new(agent.dir.join("outbox"));
        settle(
            &outbox,
            &agent,
            ChatExitEvent {
                chat_id: "oc_settle".into(),
                chat_type: "group".into(),
                reason: summon_core::exit::ExitReason::PanelReset,
                creator: Some("ou_boss".into()),
                notify: true,
                session_created_at: None,
            },
        );

        let due = outbox.list_due(now_unix_ms() + 1);
        assert_eq!(due.len(), 1, "notify=true 必须入队回执");
        assert_eq!(due[0].delivery_scope, scope().key);
        assert_eq!(due[0].agent, agent.name);
        match &due[0].kind {
            summon_core::outbox::OutboxKind::NotifyChat {
                chat_id,
                chat_type,
                creator,
                event_operator,
                reason,
                ..
            } => {
                assert_eq!(chat_id, "oc_settle");
                assert_eq!(chat_type, "group");
                assert_eq!(creator.as_deref(), Some("ou_boss"));
                assert!(
                    event_operator.is_none(),
                    "dispatcher 收口路径没有飞书事件 operator"
                );
                assert_eq!(*reason, summon_core::exit::ExitReason::PanelReset);
            }
            other => panic!("应入队 NotifyChat,得到 {other:?}"),
        }
    }

    /// `notify=false` = 回执已由别人负责(chat 线程当场发过 / Gone 路径已收口)。
    /// 这里再入一条就是同一次退出发两遍。
    #[test]
    fn settle_without_notify_enqueues_nothing() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let outbox = crate::runtime::outbox_store::OutboxStore::new(agent.dir.join("outbox"));
        settle(
            &outbox,
            &agent,
            ChatExitEvent {
                chat_id: "oc_quiet".into(),
                chat_type: "p2p".into(),
                reason: summon_core::exit::ExitReason::ManualKill,
                creator: None,
                notify: false,
                session_created_at: None,
            },
        );
        assert!(
            outbox.list_due(now_unix_ms() + 1).is_empty(),
            "notify=false 不许再补一条,否则同一次退出发两遍"
        );
    }
}

#[cfg(test)]
mod idle_scan_cadence_tests {
    use super::*;

    /// 核心回归:上次扫描后已过 115s,只该再等 185s。传满 300s 正是把扫描推到 415s 的原因
    /// (2026-08-05 实测:12:36:55 一条未 @ 本 bot 的群消息 → 唤醒被推到 12:41:55)。
    #[test]
    fn wait_shrinks_to_time_left_until_next_scan() {
        assert_eq!(
            next_recv_timeout(Duration::from_secs(300), Duration::from_secs(115)),
            Duration::from_secs(185)
        );
    }

    #[test]
    fn waits_full_period_right_after_a_scan() {
        assert_eq!(
            next_recv_timeout(Duration::from_secs(300), Duration::ZERO),
            Duration::from_secs(300)
        );
    }

    /// 已到点/已超时:立刻返回,让循环尾当轮就扫,而不是再睡一个周期。
    /// (`recv_timeout(ZERO)` 立刻 Timeout,扫完 `last_scan` 重置,故不会忙等。)
    #[test]
    fn returns_zero_when_scan_already_due() {
        for elapsed in [300u64, 415, 600] {
            assert_eq!(
                next_recv_timeout(Duration::from_secs(300), Duration::from_secs(elapsed)),
                Duration::ZERO,
                "elapsed={elapsed}s 已到点,不该再等"
            );
        }
    }

    /// 扫描间隔的上界就是 idle_scan:since ≤ period(消息处理没拖过一个周期)时被消息唤醒,
    /// 「已过 + 还要等」都不超过一个周期。这条是这次修复的不变量,旧实现(恒等一个周期)在
    /// since>0 时必然违反。since > period 时上界天然不成立——等待已收敛到 ZERO,真实间隔就是
    /// since 本身(超出部分全是处理耗时),那半边界由 returns_zero_when_scan_already_due 钉住。
    #[test]
    fn elapsed_plus_wait_never_exceeds_one_period() {
        let period = Duration::from_secs(300);
        for since in [0u64, 1, 115, 299, 300] {
            let since = Duration::from_secs(since);
            assert!(
                since + next_recv_timeout(period, since) <= period,
                "since={since:?} 时扫描间隔超过了一个周期"
            );
        }
    }
}

#[cfg(test)]
mod mirror_delivery_tests {
    use super::*;

    fn candidates(markers: &[&str]) -> crate::capture::MirrorMarkerEvidence {
        crate::capture::MirrorMarkerEvidence::from_parts(
            std::iter::empty::<&str>(),
            markers.iter().copied(),
        )
    }

    fn no_marker_evidence() -> crate::capture::MirrorMarkerEvidence {
        crate::capture::MirrorMarkerEvidence::default()
    }

    #[test]
    fn only_a_ready_watcher_counts_as_delivering() {
        let tracker = MirrorDeliveryTracker::default();
        assert!(!tracker.watcher_delivers(), "未启用");
        tracker.expect_watcher();
        assert!(!tracker.watcher_delivers(), "启动中");
        tracker.mark_watcher_bypassed();
        assert!(!tracker.watcher_delivers(), "被绕过");
        tracker.mark_watcher_ready();
        assert!(tracker.watcher_delivers());
        tracker.mark_watcher_failed();
        assert!(!tracker.watcher_delivers(), "已退出");
    }

    /// turn 类 slash 的显式 markerless fence(终审阻塞 1):归属由**偏移比较**裁决 ——
    /// fence 左侧(≤,注入前已落盘)的 markerless terminal 是前序 attach 轮,照旧转发;
    /// 严格右侧的归主路径的同构消费路,mirror 让路。**不靠「没有 active tracker」碰巧放行**:
    /// 把 begin_delivery 里的 fence 分支删掉,这条会红(右侧 terminal 被当 attach 轮转发,
    /// 与主路径的流式卡双发)。
    #[test]
    fn a_markerless_fence_routes_terminals_by_offset_not_by_luck() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_markerless_fence(token, 100));
        // 注入确认后主路径认领(run_turn PreInjected 臂开头调 on_main_claim)。
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        // fence 左侧:注入前已在盘上的前序 attach 轮,mirror 照旧转发。
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 100, false),
            MirrorDeliveryStart::Untracked,
            "≤ fence 的 terminal 是前序 attach 的,不得被本轮抢走"
        );
        // 严格右侧:主路径自己消费的轮,mirror 让路。
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::MarkerlessMainTurn),
        );
        // 同一枚 terminal 复用同一裁决(Peer/Reply 竞态)。
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::DuplicateTerminal),
        );
        tracker.finish_turn(token);
        // terminal 已被 mirror 亲眼观察 → finish 不留 tombstone,下一条 attach 轮不被误伤。
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 200, false),
            MirrorDeliveryStart::Untracked,
        );
    }

    /// 右端没钉上的续跑区间(主路径没走到 close_main_tail)无从判断在哪结束,随轮收尾清掉:
    /// 留着会把之后几小时里终端直敲的轮都压掉。
    #[test]
    fn a_continuation_without_a_closed_end_clears_when_turn_finishes() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_markerless_fence(token, 100));
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 120, &[], true);
        // 主路径 end_turn 决定续跑:落区间(左端 = fence 100,consumed_boundary 缺省)。
        tracker.mark_continuation(token);
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
        );
        tracker.finish_turn(token);
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 250, false),
            MirrorDeliveryStart::Untracked,
            "右端没钉上的区间必须随 finish_turn 清除,否则终端直敲轮被误压"
        );
    }

    /// mirror 的轮询在转录静默时退避到 5s,续跑正是长时间静默:续跑的最后一段常在主路径发完
    /// 终态、收完尾之后才被 mirror 读到。右端钉住的区间收尾后仍在,这一段照样归主路径;区间
    /// 之外终端里直接敲出的轮照常转发。
    #[test]
    fn a_continuation_segment_read_after_the_main_path_finished_is_not_sent_again() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.mark_marker_consumed(token, 100);
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        tracker.mark_continuation(token);
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 300, &[], true);
        tracker.close_main_tail(token);
        tracker.finish_turn(token);
        assert!(tracker.notice_is_observation_only(&no_marker_evidence(), 300));
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 300, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
            "主路径已发过终态的续跑段,mirror 晚读到也不得再发"
        );
        assert!(!tracker.notice_is_observation_only(&no_marker_evidence(), 400));
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 400, false),
            MirrorDeliveryStart::Untracked,
            "区间右端之后的 terminal 主路径没读到,照常转发"
        );
    }

    /// 续跑轮收尾后紧接着开始下一轮(排队的消息、到点的定时器)时,mirror 可能还没读到续跑的
    /// 最后一段。右端钉住的区间不随换轮清掉,那一段仍归主路径;mirror 读过区间右端之后,区间销账。
    #[test]
    fn a_closed_continuation_survives_the_next_turn() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.mark_marker_consumed(token, 100);
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        tracker.mark_continuation(token);
        tracker.settle_main_segment(token, 300, &[], true);
        tracker.close_main_tail(token);
        tracker.finish_turn(token);
        let next = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(next, "marker-next"));
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 300, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
            "下一轮开始后才读到的续跑最后一段仍归主路径"
        );
        assert_ne!(
            tracker.begin_delivery(&no_marker_evidence(), 350, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
            "区间右端之后的 terminal 不归续跑区间,按下一轮的规矩裁决"
        );
        assert!(
            tracker.lock_continuation_claims().is_empty(),
            "mirror 读过右端之后,区间销账"
        );
        tracker.finish_turn(next);
    }

    /// 主路径读到了插话那一段却没有投递(仅结果模式;接管轮里接管时已经落盘的重放段终点):marker 不认领,
    /// mirror 等到主路径的结论后照常按孤儿补发一次。
    #[test]
    fn a_steer_segment_the_main_path_did_not_deliver_goes_to_the_mirror() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.mark_marker_consumed(token, 100);
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-main"]), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::MainClaimed),
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        tracker.mark_continuation(token);
        tracker.note_steer_marker(token, "marker-steer");
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 300, &["marker-steer".to_string()], false);
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-steer"]), 300, false),
            MirrorDeliveryStart::Untracked,
        );
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-steer"]), 300, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Resolved),
            "只补发一次"
        );
    }

    /// 主路径迟迟不对续跑里的一段下结论(卡在同步发卡上):等满上限让路,不抢发 —— markerless
    /// 段与插话段都一样。插话段的 marker 不认领,主路径之后投递时照常认领。
    #[test]
    fn an_undecided_continuation_segment_yields_after_the_wait() {
        let tracker =
            MirrorDeliveryTracker::with_continuation_verdict_wait(Duration::from_millis(50));
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.mark_marker_consumed(token, 100);
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-main"]), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::MainClaimed),
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        tracker.mark_continuation(token);
        tracker.note_steer_marker(token, "marker-steer");
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 200, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
        );
        assert!(tracker.notice_is_observation_only(&no_marker_evidence(), 200));
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-steer"]), 300, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
        );
        tracker.settle_main_segment(token, 300, &["marker-steer".to_string()], true);
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-steer"]), 300, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Resolved),
        );
    }

    /// 续跑等待期间追加的消息自成一段,恰好又是这一轮的最后一段:主路径读到它、发终态并认领
    /// 它的 steer marker。mirror 在收尾前后读到同一段都按已裁决让路,收尾也不再为它留孤儿凭证。
    #[test]
    fn a_steer_answered_as_the_last_continuation_segment_is_sent_once() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.mark_marker_consumed(token, 100);
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-main"]), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::MainClaimed),
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        tracker.mark_continuation(token);
        tracker.note_steer_marker(token, "marker-steer");
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 300, &["marker-steer".to_string()], true);
        tracker.close_main_tail(token);
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-steer"]), 300, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Resolved),
        );
        tracker.finish_turn(token);
        let mut sent = false;
        assert!(matches!(
            deliver_mirror_turn_once(&tracker, &candidates(&["marker-steer"]), 300, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(_)
        ));
        assert!(!sent, "主路径已投递的 steer 段,收尾后也不得被孤儿凭证放行");
    }

    /// mirror 先于主路径读到续跑里的一段(主路径正忙着同步发上一段的回复):它要等主路径读到
    /// 再判。主路径读到就归主路径。
    #[test]
    fn the_mirror_waits_for_the_main_path_to_read_a_continuation_segment() {
        let tracker = std::sync::Arc::new(MirrorDeliveryTracker::default());
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.mark_marker_consumed(token, 100);
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        tracker.mark_continuation(token);
        tracker.note_steer_marker(token, "marker-steer");
        let mirror = {
            let tracker = tracker.clone();
            std::thread::spawn(move || {
                tracker.begin_delivery(&candidates(&["marker-steer"]), 300, false)
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !mirror.is_finished(),
            "主路径还没读到这一段,mirror 不得抢先裁决"
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 300, &["marker-steer".to_string()], true);
        assert_eq!(
            mirror.join().unwrap(),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Resolved),
        );
    }

    /// 主路径发完终态之后 claude 才消费的追加消息:主路径不会读到它。mirror 等到主路径收尾、
    /// 区间右端钉住,判出它在区间之外,按孤儿放行恰好发一次。
    #[test]
    fn a_steer_consumed_after_the_main_final_segment_goes_to_the_mirror() {
        let tracker = std::sync::Arc::new(MirrorDeliveryTracker::default());
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.mark_marker_consumed(token, 100);
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-main"]), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::MainClaimed),
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        tracker.mark_continuation(token);
        tracker.note_steer_marker(token, "marker-steer");
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 300, &[], true);
        let mirror = {
            let tracker = tracker.clone();
            std::thread::spawn(move || {
                tracker.begin_delivery(&candidates(&["marker-steer"]), 400, false)
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        assert!(!mirror.is_finished(), "主路径还在读,mirror 不得抢先裁决");
        tracker.close_main_tail(token);
        assert_eq!(mirror.join().unwrap(), MirrorDeliveryStart::Untracked);
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-steer"]), 400, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Resolved),
        );
    }

    /// 旧 markerless 逻辑只压**一枚** terminal(terminal_observed 置位后,第 2+ 枚落 Untracked →
    /// 双发)。续跑会产生多枚 markerless terminal(每次 task-notification 唤醒一枚),claim 不碰
    /// terminal_observed,每枚都压。
    #[test]
    fn continuation_claim_suppresses_every_markerless_terminal_not_just_the_first() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_markerless_fence(token, 100));
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 120, &[], true);
        tracker.mark_continuation(token);
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 170, &[], true);
        for offset in [150, 160, 170] {
            assert_eq!(
                tracker.begin_delivery(&no_marker_evidence(), offset, false),
                MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
                "续跑区间内 offset={offset} 的 terminal 必须被压住(旧逻辑第 2 枚起就双发)"
            );
        }
    }

    /// begin_turn 的 claim 清是防御性兜底:正常路径 finish_turn 已清;但「begin 先于迟到
    /// finish」的换轮竞态下,上一轮没跑 finish_turn,claim 还挂着——begin_turn 必须清掉,
    /// 否则新轮的 markerless attach 被旧 claim 误压。
    #[test]
    fn begin_turn_clears_continuation_claim_when_previous_turn_did_not_finish() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_markerless_fence(token, 100));
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 120, &[], true);
        tracker.mark_continuation(token);
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
        );
        // 换轮竞态:不调 finish_turn,直接 begin_turn(模拟上一轮迟到 finish 未跑)。
        let token2 = tracker.begin_turn();
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 250, false),
            MirrorDeliveryStart::Untracked,
            "旧 claim 必须被 begin_turn 清掉,否则新轮的 markerless attach 被误压"
        );
        tracker.finish_turn(token2);
    }

    /// 通知卡/Reply 路与 begin_delivery 同一条 claim 判据:续跑区间内的通知卡归主路径流式卡,
    /// mirror 不转发(observation-only)。
    #[test]
    fn peer_observation_only_inside_continuation_claim() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_markerless_fence(token, 100));
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 120, &[], true);
        tracker.mark_continuation(token);
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 150, &[], true);
        assert!(
            tracker.notice_is_observation_only(&no_marker_evidence(), 150),
            "claim 区间内的通知卡必须 observation-only(归主路径流式卡)"
        );
        assert!(
            !tracker.notice_is_observation_only(&no_marker_evidence(), 100),
            "claim 左侧的通知卡不在续跑区间,照旧转发"
        );
    }

    /// marker 轮的 claim 取 consumed_boundary(exact marker 消费边界),不是 fence。
    #[test]
    fn continuation_claim_uses_consumed_boundary_when_present() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.mark_marker_consumed(token, 200);
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 220, &[], true);
        // 区间左端 = consumed_boundary = 200:压它右侧;左侧不压(落 MarkerlessFailClosed,
        // 那是 active 轮自己的 fail-closed,与区间无关)。
        tracker.mark_continuation(token);
        tracker.mark_main_terminal_observed(token);
        tracker.settle_main_segment(token, 250, &[], true);
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 250, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
            "consumed_boundary 右侧的续跑 terminal 必须被 claim 压住"
        );
        assert_ne!(
            tracker.begin_delivery(&no_marker_evidence(), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::ContinuationClaim),
            "consumed_boundary 左侧不在续跑区间,claim 不压"
        );
        tracker.close_main_tail(token);
        tracker.finish_turn(token);
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 260, false),
            MirrorDeliveryStart::Untracked,
            "区间右端 250 之后的 terminal 不再被压"
        );
    }

    /// fence 轮已 claim 但 mirror 未及观察 terminal 就收尾(主路径 stall 收尾早于 mirror tail):
    /// 留**一枚** fence tombstone 压迟到的那枚 terminal —— 只压 fence 右侧、只压一次,再往后的
    /// markerless attach 轮不受影响。未 claim(注入被闸拒/Stuck)则什么都不留:tombstone 误留
    /// 会吞掉下一条合法 attach 轮的答案。
    #[test]
    fn a_claimed_fence_turn_leaves_a_one_shot_tombstone_and_an_unclaimed_one_leaves_nothing() {
        // claimed:留一枚,只压 fence 右侧、只压一次。
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_markerless_fence(token, 100));
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.finish_turn(token);
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 90, false),
            MirrorDeliveryStart::Untracked,
            "fence 左侧的迟到 terminal 不在 tombstone 压制范围内"
        );
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 150, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Tombstone),
        );
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 160, false),
            MirrorDeliveryStart::Untracked,
            "tombstone 一次性,不得永久压 attach"
        );

        // unclaimed:注入没走到 claim(闸拒/Stuck),不留任何 tombstone。
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_markerless_fence(token, 100));
        tracker.finish_turn(token);
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 150, false),
            MirrorDeliveryStart::Untracked,
            "未 claim 的 fence 轮不得留 tombstone"
        );
    }

    /// `!` rerun 把 fence 轮升级成 marker 轮:bind_transport_marker 必须清掉 fence,归属改由
    /// exact marker 判据接管 —— 否则 rerun 前的迟到 markerless terminal 会被旧 fence 错归。
    #[test]
    fn rebinding_a_transport_marker_clears_the_markerless_fence() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_markerless_fence(token, 100));
        assert!(tracker.bind_transport_marker(token, "marker-rerun"));
        // fence 已清:左侧偏移不再有「照旧转发」的特权,落回 marker 轮的既有 markerless
        // 启发式(watcher 未 ready → fail-closed 归当前轮观察)。
        assert_eq!(
            tracker.begin_delivery(&no_marker_evidence(), 50, false),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::MarkerlessFailClosed),
        );
    }

    /// **这条钉的是 2026-08-13 那个漏网的第二层**:fence 修好之后 watcher 确实被拉起了、下界也
    /// 正确,但那条 terminal 仍然一个字节没发 —— tracker 看到「本代 active 不认识的 consumed
    /// marker」就判 `HistoricalUnknown` 零网络。
    ///
    /// 那条判据的注释写着这种 marker「只能来自旧 offset/lazy replay」,漏了第三种来源:**上一代
    /// daemon bind 的**。上一代已经死了不可能投递,本代从没接手过也不会投递 —— 没有任何进程会发
    /// 这一轮,补发不重复。前提由 stop fence 的 `stop_eof` 证明,证不了就退回原判据。
    #[test]
    fn downtime_catchup_delivers_a_marker_bound_by_the_previous_generation() {
        let tracker = MirrorDeliveryTracker::default();
        // 本代 tracker 干净:从没 bind 过这个 marker(它是上一代注入那条消息带的)。
        let evidence = candidates(&["marker-from-dead-daemon"]);

        // 常规路径:不能主动重发 —— 这是既有不变量,一个字节都不能松。
        let mut sent = false;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &evidence, 4096, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "本代不认识的 consumed marker,常规路径必须零网络"
        );
        assert!(!sent, "常规路径不得联网");

        // 停机窗口补发:同一份证据、同一个 tracker,只因为终点落在 stop_eof 右侧 → 该发。
        let mut catchup_sent = false;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &evidence, 4096, true, || {
                catchup_sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "停机窗口那一轮没有任何进程会投递,必须补发"
        );
        assert!(catchup_sent, "补发路径必须真的联网发出去");
    }

    /// 后台通知回复不带任何 summon marker;重启后本代没有在跑的轮时,补发轮照发。
    #[test]
    fn markerless_catchup_reply_without_an_active_turn_is_sent() {
        let tracker = MirrorDeliveryTracker::default();
        let mut sent = 0;
        assert_eq!(
            handle_mirror_reply_once(
                &tracker,
                crate::capture::MirrorTurnKind::CatchupReply,
                &crate::capture::MirrorMarkerEvidence::default(),
                4096,
                || {
                    sent += 1;
                    Ok(())
                },
            )
            .unwrap(),
            MirrorSendOutcome::SentUntracked
        );
        assert_eq!(sent, 1);
    }

    /// catchup 只放宽**那一条**判据。本代正在跑的轮(active 认得 marker)仍走 tracked permit,
    /// 已被 tombstone/resolved 记过的仍 Suppressed —— 否则「补发」会变成对活跃轮的重复投递。
    #[test]
    fn downtime_catchup_does_not_loosen_the_other_gates() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        // active 认得 → 仍是 Tracked(不因 catchup 变成裸发)。
        match tracker.begin_delivery(&candidates(&["marker-current"]), 200, true) {
            MirrorDeliveryStart::Tracked(permit) => tracker.finish_delivery(permit, true),
            other => panic!("本代活跃轮必须仍走 tracked permit,实际 {other:?}"),
        }
        tracker.finish_turn(turn);
        // 已 resolved 的 marker:catchup 也不能让它重发。
        assert_eq!(
            tracker.begin_delivery(&candidates(&["marker-current"]), 300, true),
            MirrorDeliveryStart::Suppressed(MirrorSuppressedBy::Resolved),
            "已投递过的轮,补发路径同样必须抑制"
        );
    }

    /// 验收不变量 1(修 3):steer 并进同一轮(N:1)时,terminal 仍归主路径,mirror 零发送;
    /// 且被合并 terminal 认领过的 steer marker 此后**永远**不会再触发孤儿放行。
    #[test]
    fn steer_merged_into_the_same_turn_keeps_terminal_on_main_path_with_zero_mirror_send() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.note_steer_marker(token, "marker-steer");
        // 常态:主 gate 先于 terminal 认出 exact marker,抢到输出权。
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );

        // N:1 合并 terminal:同一枚 terminal 同时携带主/steer 两条消费记录。
        let merged = candidates(&["marker-main", "marker-steer"]);
        let mut sent = false;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &merged, 2048, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MainClaimed),
            "steer 并轮时凭证仍按主 marker 走 phase 裁决,mirror 让路"
        );
        assert!(!sent, "N:1 并轮的 terminal 归主路径,mirror 零发送");
        tracker.finish_turn(token);

        // steer marker 已被合并 terminal 认领:迟到 replay 不得再借孤儿放行发出去。
        let replay = candidates(&["marker-steer"]);
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &replay, 4096, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "被认领过的 steer marker 没有孤儿凭证,replay 只观察"
        );
        assert!(!sent, "全程零网络");
    }

    /// 验收不变量 2(修 3 + 钉死修 2 回归):steer 自成一轮且主路径已收尾时,mirror 恰好发一次;
    /// `downtime_catchup=false`(即 catchup_floor=None / 配额耗尽)时同样成立 —— 孤儿轮的答案
    /// 不再依赖停机补发路径的侥幸放行。
    #[test]
    fn orphan_steer_turn_after_main_finish_sends_exactly_once_without_catchup() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.note_steer_marker(token, "marker-steer");
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        // mirror 亲眼看到主 terminal(exact 主 marker)→ 让路,同时冻结主 terminal boundary。
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &candidates(&["marker-main"]), 1000, false, || {
                panic!("主路径已取得所有权,mirror 不得发送")
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MainClaimed)
        );
        tracker.finish_turn(token);

        // 孤儿轮:steer 消息在 end_turn 后才被消费,自成一轮,terminal 只带 steer marker
        // (复现见 transcript::steer_consumed_after_end_turn_forms_an_orphan_turn_with_only_the_steer_marker)。
        let orphan = candidates(&["marker-steer"]);
        let mut sends = 0usize;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &orphan, 2000, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "两前提都成立(主 terminal 已消费 + steer marker 无人认领)→ 放行发送"
        );
        // 同证据 replay:凭证已销账并入 resolved,恰好一次,不双发。
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &orphan, 2000, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Resolved),
            "孤儿凭证一次性:replay 必须被 resolved 拦住"
        );
        assert_eq!(sends, 1, "at-most-once:恰好发一次");
    }

    /// 终审点名的竞态(修 3 轮内放行):主路径发卡尚未返回、RAII guard 尚未 drop
    /// (finish_turn 未执行),mirror 已读到紧随其后的孤儿 steer terminal。三条前提此刻已
    /// 全部证成(主 exact terminal 已被 mirror 记下 boundary、phase 已 MainClaimed、孤儿
    /// terminal 严格在其右侧)—— 等 finish 就是永久漏发,必须就地放行,且恰好一次。
    #[test]
    fn orphan_steer_terminal_during_claimed_turn_sends_exactly_once_before_finish() {
        let tracker = MirrorDeliveryTracker::default();
        let token = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(token, "marker-main"));
        tracker.note_steer_marker(token, "marker-steer");
        assert_eq!(
            tracker.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        // mirror 亲眼看到主 exact terminal → 让路,同时冻结主 terminal boundary(前提 1+2)。
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &candidates(&["marker-main"]), 1000, false, || {
                panic!("主路径已取得所有权,mirror 不得发送")
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MainClaimed)
        );

        // 主路径尚未 finish_turn:孤儿 terminal 已到,严格晚于主 terminal(前提 3)→ 就地放行。
        let orphan = candidates(&["marker-steer"]);
        let mut sends = 0usize;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &orphan, 2000, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "三前提全证成时,in-flight 等待只会永久漏发 → 就地放行"
        );
        // 同证据 replay(仍在 finish 前):先销后发已记 resolved,恰好一次。
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &orphan, 2000, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Resolved),
            "轮内放行同样一次性:replay 必须被 resolved 拦住"
        );
        tracker.finish_turn(token);
        // finish 之后也没有第二条路:marker 已被认领,不存在孤儿凭证,replay 仍被拦。
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &orphan, 2000, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Resolved),
            "finish 后无孤儿凭证,resolved 继续拦住 replay"
        );
        assert_eq!(sends, 1, "at-most-once:恰好发一次");
    }

    /// 修 3 的 fail-closed 面:孤儿放行的前提任一条证不了,都必须退回零网络观察,宁漏发。
    #[test]
    fn orphan_steer_release_fails_closed_when_any_premise_is_unproven() {
        // 前提 1 破:本轮以失败回执收尾(FailureClaimed)→ 不留孤儿凭证。
        let failed = MirrorDeliveryTracker::default();
        let token = failed.begin_turn();
        assert!(failed.bind_transport_marker(token, "marker-main"));
        failed.note_steer_marker(token, "marker-steer");
        assert_eq!(
            failed.claim_failure(token),
            MirrorFailureClaim::SendFailureNotice
        );
        assert_eq!(
            deliver_mirror_turn_once(&failed, &candidates(&["marker-main"]), 1000, false, || {
                panic!("失败回执已取得所有权")
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::FailureClaimed)
        );
        failed.finish_turn(token);
        let mut sent = false;
        assert_eq!(
            deliver_mirror_turn_once(&failed, &candidates(&["marker-steer"]), 2000, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "失败轮不开孤儿口:答案与失败卡不能双发"
        );
        assert!(!sent);

        // 前提破:mirror 从未亲眼见过主 terminal(只有 main parser 观察,无 boundary)→ 无从判序,
        // 不留凭证。
        let unseen = MirrorDeliveryTracker::default();
        let token = unseen.begin_turn();
        assert!(unseen.bind_transport_marker(token, "marker-main"));
        unseen.note_steer_marker(token, "marker-steer");
        assert_eq!(
            unseen.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        unseen.mark_main_terminal_observed(token);
        unseen.finish_turn(token);
        assert_eq!(
            deliver_mirror_turn_once(&unseen, &candidates(&["marker-steer"]), 2000, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "mirror 没有主 terminal boundary 时证不了顺序,fail-closed 只观察"
        );
        assert!(!sent);

        // 序证明破:孤儿 terminal 不晚于主 terminal boundary → 凭证作废,且不给第二次机会。
        let ordered = MirrorDeliveryTracker::default();
        let token = ordered.begin_turn();
        assert!(ordered.bind_transport_marker(token, "marker-main"));
        ordered.note_steer_marker(token, "marker-steer");
        assert_eq!(
            ordered.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            deliver_mirror_turn_once(&ordered, &candidates(&["marker-main"]), 1000, false, || {
                panic!("主路径已取得所有权")
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MainClaimed)
        );
        ordered.finish_turn(token);
        assert_eq!(
            deliver_mirror_turn_once(
                &ordered,
                &candidates(&["marker-steer"]),
                1000,
                false,
                || {
                    sent = true;
                    Ok(())
                }
            )
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "offset 不大于主 terminal 的 steer terminal 证不了是新轮"
        );
        assert_eq!(
            deliver_mirror_turn_once(
                &ordered,
                &candidates(&["marker-steer"]),
                3000,
                false,
                || {
                    sent = true;
                    Ok(())
                }
            )
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "序证明失败的凭证一并销账,之后也不再放行(宁漏发)"
        );
        assert!(!sent);

        // 轮内序证明破:主 terminal 已被 mirror 记下 boundary、phase 已 MainClaimed,但孤儿
        // terminal 不在其严格右侧 → 就地放行的前提 3 证不了,仍认领 + 让路;finish 后也不再
        // 有孤儿口(宁漏发)。
        let inflight_ordered = MirrorDeliveryTracker::default();
        let token = inflight_ordered.begin_turn();
        assert!(inflight_ordered.bind_transport_marker(token, "marker-main"));
        inflight_ordered.note_steer_marker(token, "marker-steer");
        assert_eq!(
            inflight_ordered.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            deliver_mirror_turn_once(
                &inflight_ordered,
                &candidates(&["marker-main"]),
                1000,
                false,
                || panic!("主路径已取得所有权")
            )
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MainClaimed)
        );
        assert_eq!(
            deliver_mirror_turn_once(
                &inflight_ordered,
                &candidates(&["marker-steer"]),
                1000,
                false,
                || {
                    sent = true;
                    Ok(())
                }
            )
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::SteerClaimedByActiveTurn),
            "offset 不大于主 terminal 的轮内 steer terminal 序证不成,让路"
        );
        inflight_ordered.finish_turn(token);
        assert_eq!(
            deliver_mirror_turn_once(
                &inflight_ordered,
                &candidates(&["marker-steer"]),
                3000,
                false,
                || {
                    sent = true;
                    Ok(())
                }
            )
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "轮内被认领过的 marker 在 finish 后没有孤儿凭证,replay 只观察"
        );
        assert!(!sent);

        // 轮未收尾:steer 自成 terminal 但 active 还在 → fail-closed 让路,并就地认领,
        // finish 后也不再有孤儿口。
        let live = MirrorDeliveryTracker::default();
        let token = live.begin_turn();
        assert!(live.bind_transport_marker(token, "marker-main"));
        live.note_steer_marker(token, "marker-steer");
        assert_eq!(
            live.claim_main_delivery(token),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            deliver_mirror_turn_once(&live, &candidates(&["marker-steer"]), 500, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::SteerClaimedByActiveTurn),
            "轮内出现的 steer terminal 前提证不了,让路"
        );
        live.finish_turn(token);
        assert_eq!(
            deliver_mirror_turn_once(&live, &candidates(&["marker-steer"]), 600, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "已在轮内被认领的 steer marker 不再有孤儿凭证"
        );
        assert!(!sent, "四种前提破缺全程零网络");
    }

    #[test]
    fn mirror_message_uuid_is_stable_bounded_and_terminal_specific() {
        let checkpoint = b"generation-a";
        let base = mirror_message_uuid("epoch", "oc_chat", "sid", 42, checkpoint, "reply", 0);
        assert_eq!(
            base,
            mirror_message_uuid("epoch", "oc_chat", "sid", 42, checkpoint, "reply", 0)
        );
        for changed in [
            mirror_message_uuid("other-epoch", "oc_chat", "sid", 42, checkpoint, "reply", 0),
            mirror_message_uuid("epoch", "other-chat", "sid", 42, checkpoint, "reply", 0),
            mirror_message_uuid("epoch", "oc_chat", "other-sid", 42, checkpoint, "reply", 0),
            mirror_message_uuid("epoch", "oc_chat", "sid", 43, checkpoint, "reply", 0),
            mirror_message_uuid("epoch", "oc_chat", "sid", 42, b"generation-b", "reply", 0),
            mirror_message_uuid("epoch", "oc_chat", "sid", 42, checkpoint, "peer", 0),
            mirror_message_uuid("epoch", "oc_chat", "sid", 42, checkpoint, "reply", 1),
        ] {
            assert_ne!(base, changed);
        }
        assert!(base.len() <= 50);
        assert!(base
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));

        let mut first = MirrorMessageUuidCursor::default();
        let mut replay = MirrorMessageUuidCursor::default();
        for (kind, expected_ordinal) in [("notice", 0), ("notice", 1), ("reply", 0)] {
            let first_value = first.next("epoch", "oc_chat", "sid", 42, checkpoint, kind);
            let replay_value = replay.next("epoch", "oc_chat", "sid", 42, checkpoint, kind);
            assert_eq!(
                first_value,
                mirror_message_uuid(
                    "epoch",
                    "oc_chat",
                    "sid",
                    42,
                    checkpoint,
                    kind,
                    expected_ordinal,
                )
            );
            assert_eq!(replay_value, first_value);
        }

        // 一块的键只由它自己决定:同一轮里前面多发了几块别的 kind,回复的 uuid 一个字节都不该变。
        // 共用一个计数器就做不到 —— 那一轮多一条通知,重解析时回复就按新键重发一遍。
        let mut lean = MirrorMessageUuidCursor::default();
        let lean_reply = lean.next("epoch", "oc_chat", "sid", 42, checkpoint, "reply");
        let mut busy = MirrorMessageUuidCursor::default();
        for _ in 0..5 {
            busy.next("epoch", "oc_chat", "sid", 42, checkpoint, "notice");
        }
        assert_eq!(
            busy.next("epoch", "oc_chat", "sid", 42, checkpoint, "reply"),
            lean_reply
        );
    }

    #[test]
    fn first_turn_waits_until_mirror_has_frozen_its_start_offset() {
        let tracker = Arc::new(MirrorDeliveryTracker::default());
        tracker.expect_watcher();
        let waiting = Arc::clone(&tracker);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(waiting.wait_watcher_ready(Duration::from_secs(1)))
                .unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "Starting 不能提前放行首个 prompt"
        );
        tracker.mark_watcher_ready();
        rx.recv_timeout(Duration::from_secs(1))
            .unwrap()
            .expect("ready 后应放行");
        worker.join().unwrap();
    }

    #[test]
    fn cold_empty_bypass_releases_main_path_while_watcher_can_later_become_ready() {
        let tracker = MirrorDeliveryTracker::default();
        tracker.expect_watcher();
        tracker.mark_watcher_bypassed();
        tracker
            .wait_watcher_ready(Duration::from_millis(10))
            .expect("0B bypass 后主路径必须立即放行");
        assert_eq!(*tracker.lock_watcher(), MirrorWatcherState::Bypassed);

        tracker.mark_watcher_ready();
        assert_eq!(*tracker.lock_watcher(), MirrorWatcherState::Ready);
    }

    #[test]
    fn watcher_timeout_can_be_downgraded_to_bypass_without_losing_the_prompt() {
        let tracker = MirrorDeliveryTracker::default();
        tracker.expect_watcher();
        assert!(
            tracker
                .wait_watcher_ready(Duration::from_millis(1))
                .is_err(),
            "Starting 的确应先给调用方一个有界超时"
        );
        tracker.mark_watcher_bypassed();
        tracker
            .wait_watcher_ready(Duration::from_millis(1))
            .expect("调用方降级后必须继续原 prompt，不能把 WAL 销账为 Failed");
    }

    #[test]
    fn zero_network_preflight_failure_leaves_main_delivery_eligible() {
        let tracker = MirrorDeliveryTracker::default();
        tracker.mark_watcher_ready();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        let mut network_called = false;
        let error = handle_mirror_reply_with_preflight(
            &tracker,
            crate::capture::MirrorTurnKind::Reply,
            &candidates(&["marker-current"]),
            100,
            || anyhow::bail!("epoch drift before network"),
            || {
                network_called = true;
                Ok(())
            },
        )
        .expect_err("preflight 应拒绝镜像");
        assert!(error.to_string().contains("epoch drift"));
        assert!(!network_called, "身份复验失败不得触发 HTTP");
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::DeliverOnMainPath,
            "零网络失败不能伪装成 DeliveryUncertain 后压掉主回复"
        );
    }

    #[test]
    fn watcher_exit_guard_revokes_ready_even_when_delivery_panics() {
        let tracker = MirrorDeliveryTracker::default();
        tracker.expect_watcher();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = MirrorWatcherExitGuard { tracker: &tracker };
            tracker.mark_watcher_ready();
            panic!("模拟 Peer/Untracked send panic");
        }));
        assert!(panic.is_err());
        assert_eq!(*tracker.lock_watcher(), MirrorWatcherState::Failed);
        tracker
            .wait_watcher_ready(Duration::from_millis(10))
            .expect("panic 退出后主路径不得继续等待已死 watcher");
    }

    #[test]
    fn failed_watcher_releases_main_path_without_claiming_delivery() {
        let tracker = MirrorDeliveryTracker::default();
        tracker.expect_watcher();
        tracker.mark_watcher_failed();
        tracker
            .wait_watcher_ready(Duration::from_millis(10))
            .expect("watcher 已退出时主路径独立继续");
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );
    }

    #[test]
    fn uncertain_send_suppresses_a_second_delivery_path() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));

        let outcome = deliver_mirror_turn_once(
            &tracker,
            &candidates(&["marker-current"]),
            101,
            false,
            || anyhow::bail!("飞书不可达"),
        );

        assert!(outcome.is_err());
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SuppressFailureNotice,
            "HTTP Err 不能证明服务端未收；失败卡走另一发送路径会制造双发"
        );
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::SuppressMainDelivery,
            "网络结果不确定时主答案也必须抑制"
        );
    }

    #[test]
    fn panicking_send_releases_the_inflight_delivery_waiter() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = deliver_mirror_turn_once(
                &tracker,
                &candidates(&["marker-current"]),
                101,
                false,
                || -> anyhow::Result<()> { panic!("模拟发送适配器 panic") },
            );
        }));
        assert!(panic.is_err());
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SuppressFailureNotice,
            "RAII 必须把 Sending 收敛成不确定态，不能让 Condvar 永久等待或重复发送"
        );
    }

    #[test]
    fn cold_cut_markerless_terminal_is_suppressed_while_a_prompt_is_active() {
        let tracker = MirrorDeliveryTracker::default();
        tracker.expect_watcher();
        tracker.mark_watcher_bypassed();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        tracker.mark_watcher_ready();
        let mut sent = false;

        let outcome = deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 102, false, || {
            sent = true;
            Ok(())
        });

        assert!(!sent, "无法证明与 active prompt 无关时必须 fail-closed");
        assert_eq!(
            outcome.unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MarkerlessFailClosed)
        );
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice,
            "markerless terminal 不能冒充本轮答案"
        );
    }

    #[test]
    fn markerless_terminal_is_sent_only_without_an_active_or_resolved_fence() {
        let tracker = MirrorDeliveryTracker::default();
        let mut sent = 0usize;
        let outcome = deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 102, false, || {
            sent += 1;
            Ok(())
        });
        assert_eq!(outcome.unwrap(), MirrorSendOutcome::SentUntracked);
        assert_eq!(sent, 1, "真正的终端 attach turn 仍应转发");

        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(turn);

        let outcome = deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 103, false, || {
            sent += 1;
            Ok(())
        });
        assert_eq!(
            outcome.unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Tombstone)
        );
        assert_eq!(sent, 1, "guard drop 后首个 ambiguous terminal 仍须拦住");

        let outcome = deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 104, false, || {
            sent += 1;
            Ok(())
        });
        assert_eq!(outcome.unwrap(), MirrorSendOutcome::SentUntracked);
        assert_eq!(sent, 2, "一次性 tombstone 不能永久关闭终端 attach");
    }

    #[test]
    fn observed_exact_terminal_does_not_leave_a_markerless_tombstone() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            deliver_mirror_turn_once(
                &tracker,
                &candidates(&["marker-current"]),
                105,
                false,
                || { panic!("主路径已取得所有权，exact terminal 不得再发送") }
            )
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MainClaimed)
        );
        tracker.finish_turn(turn);

        let mut sent = false;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 106, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked
        );
        assert!(
            sent,
            "exact terminal 已观察后，下一条 markerless attach 不应被误伤"
        );
    }

    #[test]
    fn reply_observed_after_main_finish_is_zero_network_and_releases_the_next_attach() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.finish_turn(turn);

        let mut network_sends = 0usize;
        assert_eq!(
            handle_mirror_reply_once(
                &tracker,
                crate::capture::MirrorTurnKind::ReplyObserved,
                &candidates(&["marker-current"]),
                120,
                || {
                    network_sends += 1;
                    Ok(())
                },
            )
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::ReplyObserved)
        );
        assert_eq!(network_sends, 0, "ReplyObserved 结构上绝不能触发网络发送");

        assert_eq!(
            handle_mirror_reply_once(
                &tracker,
                crate::capture::MirrorTurnKind::Reply,
                &no_marker_evidence(),
                121,
                || {
                    network_sends += 1;
                    Ok(())
                },
            )
            .unwrap(),
            MirrorSendOutcome::SentUntracked
        );
        assert_eq!(
            network_sends, 1,
            "exact observed 已消化 tombstone，attach 必须送达"
        );
    }

    #[test]
    fn bypassed_main_success_uses_parser_terminal_proof_but_failure_keeps_one_tombstone() {
        let success = MirrorDeliveryTracker::default();
        success.expect_watcher();
        success.mark_watcher_bypassed();
        let turn = success.begin_turn();
        assert!(success.bind_transport_marker(turn, "marker-success"));
        assert_eq!(
            success.claim_main_delivery(turn),
            MainDeliveryClaim::DeliverOnMainPath
        );
        success.mark_main_terminal_observed(turn);
        success.finish_turn(turn);
        assert_eq!(
            deliver_mirror_turn_once(&success, &no_marker_evidence(), 122, false, || Ok(()))
                .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "主 parser 已见 terminal 时，bypass 首轮不得留下 tombstone 误伤 attach"
        );

        let failed = MirrorDeliveryTracker::default();
        failed.expect_watcher();
        failed.mark_watcher_bypassed();
        let turn = failed.begin_turn();
        assert!(failed.bind_transport_marker(turn, "marker-failed"));
        assert_eq!(
            failed.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );
        failed.finish_turn(turn);
        assert_eq!(
            deliver_mirror_turn_once(&failed, &no_marker_evidence(), 123, false, || Ok(()))
                .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Tombstone),
            "主 parser 未见 terminal 的失败轮仍需留一次 tombstone 压迟到答案"
        );
        assert_eq!(
            deliver_mirror_turn_once(&failed, &no_marker_evidence(), 124, false, || Ok(()))
                .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "失败兜底只压一次，不能永久关闭 attach"
        );
    }

    #[test]
    fn cold_cut_markerless_peer_and_reply_are_both_zero_network() {
        let tracker = MirrorDeliveryTracker::default();
        tracker.expect_watcher();
        tracker.mark_watcher_bypassed();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        tracker.mark_watcher_ready();

        let mut network_sends = 0usize;
        assert_eq!(
            handle_mirror_notice_once(&tracker, &no_marker_evidence(), 1240, || {
                network_sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::NoticeObservation)
        );
        assert_eq!(
            handle_mirror_reply_once(
                &tracker,
                crate::capture::MirrorTurnKind::Reply,
                &no_marker_evidence(),
                1240,
                || {
                    network_sends += 1;
                    Ok(())
                },
            )
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MarkerlessFailClosed)
        );
        assert_eq!(
            network_sends, 0,
            "marker 可能落在 watcher cold cut 之前时，Peer/Reply 都必须零网络"
        );
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::DeliverOnMainPath,
            "cold fence 只抑制不确定 mirror，主路径仍必须负责本轮答案"
        );
    }

    #[test]
    fn ready_watcher_sends_prior_attach_peer_and_reply_before_enqueued_prompt() {
        let tracker = MirrorDeliveryTracker::default();
        tracker.expect_watcher();
        tracker.mark_watcher_ready();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-b"));
        let evidence = crate::capture::MirrorMarkerEvidence::from_parts(
            ["marker-b"],
            std::iter::empty::<&str>(),
        );

        let mut network_sends = 0usize;
        assert_eq!(
            handle_mirror_notice_once(&tracker, &evidence, 1250, || {
                network_sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked
        );
        assert_eq!(
            handle_mirror_reply_once(
                &tracker,
                crate::capture::MirrorTurnKind::Reply,
                &evidence,
                1250,
                || {
                    network_sends += 1;
                    Ok(())
                },
            )
            .unwrap(),
            MirrorSendOutcome::SentUntracked
        );
        assert_eq!(
            network_sends, 2,
            "前序 attach A 的 peer/reply 都不能被 B 吞掉"
        );
        assert!(
            !tracker
                .lock_active()
                .as_ref()
                .expect("B 仍 active")
                .terminal_observed,
            "A terminal 不能污染尚未 consumed 的 B"
        );
    }

    #[test]
    fn old_failure_tombstone_precedes_later_active_markerless_heuristics() {
        for bind_later_prompt in [false, true] {
            let tracker = MirrorDeliveryTracker::default();
            tracker.expect_watcher();
            tracker.mark_watcher_ready();
            let old = tracker.begin_turn();
            assert!(tracker.bind_transport_marker(old, "marker-a"));
            assert_eq!(
                tracker.claim_failure(old),
                MirrorFailureClaim::SendFailureNotice
            );
            tracker.finish_turn(old);

            let current = tracker.begin_turn();
            if bind_later_prompt {
                assert!(tracker.bind_transport_marker(current, "marker-b"));
            }
            let mut network_sends = 0usize;
            assert_eq!(
                handle_mirror_notice_once(&tracker, &no_marker_evidence(), 1255, || {
                    network_sends += 1;
                    Ok(())
                })
                .unwrap(),
                MirrorSendOutcome::Observed(MirrorObservedBy::NoticeObservation)
            );
            assert_eq!(
                handle_mirror_reply_once(
                    &tracker,
                    crate::capture::MirrorTurnKind::Reply,
                    &no_marker_evidence(),
                    1255,
                    || {
                        network_sends += 1;
                        Ok(())
                    },
                )
                .unwrap(),
                MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Tombstone)
            );
            assert_eq!(
                network_sends, 0,
                "A 已发失败回执后，迟到 Peer/Reply 不能因 B active 再联网"
            );
            let active = tracker.lock_active();
            let active = active.as_ref().expect("B 仍 active");
            assert_eq!(active.token, current);
            assert!(
                !active.terminal_observed,
                "A 的迟到 terminal 不能污染 B 的 terminal 状态"
            );
        }
    }

    #[test]
    fn markerless_observed_terminal_consumes_old_tombstone_even_with_a_new_active_turn() {
        let tracker = MirrorDeliveryTracker::default();
        let old = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(old, "marker-old"));
        assert_eq!(
            tracker.claim_failure(old),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(old);

        let current = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(current, "marker-current"));
        tracker.observe_consumed_terminal(&no_marker_evidence(), 125);
        tracker.finish_turn(current);

        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 126, false, || Ok(()))
                .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "迟到 markerless observed 必须消耗旧 tombstone，不能留给后续 attach"
        );
    }

    #[test]
    fn later_exact_terminal_clears_all_older_markerless_tombstones() {
        let tracker = MirrorDeliveryTracker::default();
        for marker in ["marker-old-a", "marker-old-b"] {
            let turn = tracker.begin_turn();
            assert!(tracker.bind_transport_marker(turn, marker));
            assert_eq!(
                tracker.claim_failure(turn),
                MirrorFailureClaim::SendFailureNotice
            );
            tracker.finish_turn(turn);
        }

        let current = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(current, "marker-current"));
        tracker.observe_consumed_terminal(&candidates(&["marker-current"]), 127);
        tracker.finish_turn(current);

        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 128, false, || Ok(()))
                .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "transcript 单调，later exact B 已证明所有旧 markerless tombstone 过期"
        );
    }

    #[test]
    fn failure_waits_for_inflight_delivery_and_suppresses_only_after_success() {
        let tracker = Arc::new(MirrorDeliveryTracker::default());
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        let permit = match tracker.begin_delivery(&candidates(&["marker-current"]), 103, false) {
            MirrorDeliveryStart::Tracked(permit) => permit,
            other => panic!("terminal 携带 exact marker，应拿到 tracked permit，实际 {other:?}"),
        };

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let claimant = Arc::clone(&tracker);
        let join = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(claimant.claim_failure(turn)).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "镜像发送尚未完成时，失败路径不能抢跑发卡"
        );

        tracker.finish_delivery(permit, true);
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            MirrorFailureClaim::SuppressFailureNotice
        );
        join.join().unwrap();
    }

    #[test]
    fn failure_claim_wins_race_and_blocks_late_answer_delivery() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );

        let mut send_called = false;
        let outcome = deliver_mirror_turn_once(
            &tracker,
            &candidates(&["marker-current"]),
            104,
            false,
            || {
                send_called = true;
                Ok(())
            },
        );
        assert_eq!(
            outcome.unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::FailureClaimed)
        );
        assert!(!send_called, "失败回执已取得所有权后，镜像不能再发同轮答案");
    }

    #[test]
    fn resolved_marker_stays_suppressed_after_turn_cleanup() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(turn);

        let mut send_called = false;
        let outcome = deliver_mirror_turn_once(
            &tracker,
            &candidates(&["marker-current"]),
            107,
            false,
            || {
                send_called = true;
                Ok(())
            },
        );
        assert_eq!(
            outcome.unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Resolved)
        );
        assert!(
            !send_called,
            "turn 清理后、下一轮尚未开始时，迟到答案必须被 tombstone 拦住"
        );

        let next_turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(next_turn, "marker-next"));
        let outcome = deliver_mirror_turn_once(
            &tracker,
            &candidates(&["marker-current"]),
            108,
            false,
            || {
                send_called = true;
                Ok(())
            },
        );
        assert_eq!(
            outcome.unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Resolved)
        );
        assert!(
            !send_called,
            "下一轮已开始后，迟到的旧 marker 答案仍必须被 tombstone 拦住"
        );
    }

    #[test]
    fn another_marker_cannot_complete_the_current_turn() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));

        assert_eq!(
            deliver_mirror_turn_once(&tracker, &candidates(&["marker-other"]), 105, false, || Ok(
                ()
            ))
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "active 不认识的 consumed marker 是历史轮，只观察不联网"
        );
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );
    }

    #[test]
    fn active_exact_marker_wins_over_a_stale_candidate_from_an_unfinished_turn() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));

        let outcome = deliver_mirror_turn_once(
            &tracker,
            &candidates(&["marker-stale", "marker-current"]),
            111,
            false,
            || Ok(()),
        )
        .unwrap();

        assert_eq!(outcome, MirrorSendOutcome::SentTracked);
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::SuppressMainDelivery,
            "stale 首候选不能把当前 exact terminal 降成 Untracked 后双发"
        );
    }

    #[test]
    fn any_resolved_candidate_suppresses_a_late_terminal_when_active_marker_is_different() {
        let tracker = MirrorDeliveryTracker::default();
        let old = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(old, "marker-resolved"));
        assert_eq!(
            tracker.claim_failure(old),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(old);

        let current = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(current, "marker-current"));
        let mut sent = false;
        let outcome = deliver_mirror_turn_once(
            &tracker,
            &candidates(&["marker-resolved", "marker-steer"]),
            112,
            false,
            || {
                sent = true;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            outcome,
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Resolved)
        );
        assert!(
            !sent,
            "resolved 候选的迟到 terminal 绝不能降成 Untracked 发送"
        );
    }

    #[test]
    fn awaiting_marker_never_adopts_a_historical_consumed_candidate() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert_eq!(
            deliver_mirror_turn_once(
                &tracker,
                &candidates(&["marker-current"]),
                106,
                false,
                || Ok(())
            )
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown)
        );
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice,
            "bind-before-send 不变量下，登记前看到的 candidate 绝不能冒充本轮送达"
        );
    }

    #[test]
    fn earlier_candidate_does_not_block_exact_marker_registered_afterward() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert_eq!(
            deliver_mirror_turn_once(
                &tracker,
                &candidates(&["marker-earlier"]),
                107,
                false,
                || Ok(())
            )
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown)
        );

        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            deliver_mirror_turn_once(
                &tracker,
                &candidates(&["marker-current"]),
                108,
                false,
                || Ok(())
            )
            .unwrap(),
            MirrorSendOutcome::SentTracked
        );
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SuppressFailureNotice
        );
    }

    #[test]
    fn mirror_answer_wins_before_rescued_main_path_without_double_send() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            deliver_mirror_turn_once(
                &tracker,
                &candidates(&["marker-current"]),
                109,
                false,
                || Ok(())
            )
            .unwrap(),
            MirrorSendOutcome::SentTracked
        );
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::SuppressMainDelivery,
            "deadline rescue 认出 marker 时，mirror 已发成功就不能让主路径再发一遍"
        );
    }

    #[test]
    fn main_path_wins_before_terminal_and_suppresses_late_mirror() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-current"));
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::DeliverOnMainPath
        );

        let mut send_called = false;
        assert_eq!(
            deliver_mirror_turn_once(
                &tracker,
                &candidates(&["marker-current"]),
                110,
                false,
                || {
                    send_called = true;
                    Ok(())
                }
            )
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MainClaimed)
        );
        assert!(!send_called, "主路径取得所有权后，迟到 mirror 必须让路");
    }

    #[test]
    fn next_prompt_enqueue_does_not_swallow_the_previous_attach_terminal() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-b"));
        let evidence = crate::capture::MirrorMarkerEvidence::from_parts(
            ["marker-b"],
            std::iter::empty::<&str>(),
        );
        let mut sends = 0usize;

        assert_eq!(
            deliver_mirror_turn_once(&tracker, &evidence, 130, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "B 只 enqueue 未 consumed 时，markerless terminal 明确属于前序 attach A"
        );
        assert_eq!(sends, 1);
        assert!(
            !tracker
                .lock_active()
                .as_ref()
                .expect("B 仍 active")
                .terminal_observed,
            "A terminal 不能误记到 B"
        );

        let consumed = crate::capture::MirrorMarkerEvidence::from_parts(
            std::iter::empty::<&str>(),
            ["marker-b"],
        );
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &consumed, 131, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::MainClaimed)
        );
        assert_eq!(sends, 1, "B 仍只由已取得所有权的主路径发送");
    }

    #[test]
    fn terminal_before_marker_injection_is_an_untracked_attach() {
        let tracker = MirrorDeliveryTracker::default();
        let _turn = tracker.begin_turn();
        let mut sent = false;

        assert_eq!(
            deliver_mirror_turn_once(
                &tracker,
                &crate::capture::MirrorMarkerEvidence::default(),
                132,
                false,
                || {
                    sent = true;
                    Ok(())
                },
            )
            .unwrap(),
            MirrorSendOutcome::SentUntracked
        );
        assert!(sent);
        assert!(
            !tracker
                .lock_active()
                .as_ref()
                .expect("新 prompt 仍 active")
                .terminal_observed
        );
    }

    #[test]
    fn failure_before_marker_generation_leaves_no_markerless_tombstone() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(turn);

        let mut sent = false;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 140, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "sid/path 定位阶段失败时尚无 CLI 副作用，不能压住下一条 attach"
        );
        assert!(sent);
    }

    #[test]
    fn markerless_tombstone_only_suppresses_terminals_after_its_consumed_boundary() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-b"));
        tracker.mark_marker_consumed(turn, 200);
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::DeliverOnMainPath
        );
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(turn);

        let mut sends = 0usize;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 150, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "A terminal 早于 B consumed boundary，绝不能消费 B 的 tombstone"
        );
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 201, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Tombstone),
            "B 之后的首个 markerless terminal 才与 tombstone 匹配"
        );
        assert_eq!(sends, 1);
    }

    #[test]
    fn rerun_replaces_the_marker_consumption_boundary_for_tombstones() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-first"));
        tracker.mark_marker_consumed(turn, 100);
        assert_eq!(
            tracker.claim_main_delivery(turn),
            MainDeliveryClaim::DeliverOnMainPath
        );

        assert!(tracker.bind_transport_marker(turn, "marker-rerun"));
        tracker.mark_marker_consumed(turn, 300);
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(turn);

        let mut sends = 0usize;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 250, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "rerun 前后的 attach 不能被首轮旧 boundary 或新轮 tombstone 吞掉"
        );
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 301, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Tombstone),
            "rerun 消费边界之后的迟到 markerless terminal 才应命中 tombstone"
        );
        assert_eq!(sends, 1);
    }

    #[test]
    fn enqueued_evidence_keeps_an_earlier_attach_from_consuming_an_unbounded_tombstone() {
        let tracker = MirrorDeliveryTracker::default();
        let turn = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(turn, "marker-b"));
        assert_eq!(
            tracker.claim_failure(turn),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(turn);
        let enqueued = crate::capture::MirrorMarkerEvidence::from_parts(
            ["marker-b"],
            std::iter::empty::<&str>(),
        );

        let mut sends = 0usize;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &enqueued, 300, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "B 只 enqueue 时，当前 terminal 是前序 attach A"
        );
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 301, false, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Tombstone),
            "A 不得消费的 unbounded tombstone 应继续压住 B 迟到终态"
        );
        assert_eq!(sends, 1);
    }

    #[test]
    fn peer_freezes_markerless_disposition_for_the_reply_at_the_same_boundary() {
        let tracker = MirrorDeliveryTracker::default();
        let mut sends = 0usize;
        assert_eq!(
            handle_mirror_notice_once(&tracker, &no_marker_evidence(), 400, || {
                sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::SentUntracked
        );

        let later = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(later, "marker-later"));
        assert_eq!(
            tracker.claim_failure(later),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(later);

        assert_eq!(
            handle_mirror_reply_once(
                &tracker,
                crate::capture::MirrorTurnKind::Reply,
                &no_marker_evidence(),
                400,
                || {
                    sends += 1;
                    Ok(())
                },
            )
            .unwrap(),
            MirrorSendOutcome::SentUntracked,
            "Peer 与 Reply 属同一 terminal，中途新建的 tombstone 不得导致半轮"
        );
        assert_eq!(sends, 2);
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 401, false, || Ok(()))
                .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Tombstone),
            "后来轮的 tombstone 必须保留给后来 terminal"
        );
    }

    #[test]
    fn main_terminal_observation_does_not_retire_an_older_unseen_tombstone() {
        let tracker = MirrorDeliveryTracker::default();
        let old = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(old, "marker-a"));
        assert_eq!(
            tracker.claim_failure(old),
            MirrorFailureClaim::SendFailureNotice
        );
        tracker.finish_turn(old);

        let current = tracker.begin_turn();
        assert!(tracker.bind_transport_marker(current, "marker-b"));
        tracker.mark_marker_consumed(current, 500);
        assert_eq!(
            tracker.claim_main_delivery(current),
            MainDeliveryClaim::DeliverOnMainPath
        );
        tracker.mark_main_terminal_observed(current);
        tracker.finish_turn(current);

        let mut sent = false;
        assert_eq!(
            deliver_mirror_turn_once(&tracker, &no_marker_evidence(), 450, false, || {
                sent = true;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Suppressed(MirrorSuppressedBy::Tombstone),
            "main 先观察 B 不证明 mirror 已跨过 A，A 失败回执的 tombstone 必须保留"
        );
        assert!(!sent);
    }

    #[test]
    fn fresh_tracker_never_replays_a_historical_summon_marker() {
        let tracker = MirrorDeliveryTracker::default();
        let historical = crate::capture::MirrorMarkerEvidence::from_parts(
            std::iter::empty::<&str>(),
            ["summon-transport-turn:123e4567-e89b-12d3-a456-426614174000"],
        );
        let mut network_sends = 0usize;

        assert_eq!(
            deliver_mirror_turn_once(&tracker, &historical, 133, false, || {
                network_sends += 1;
                Ok(())
            })
            .unwrap(),
            MirrorSendOutcome::Observed(MirrorObservedBy::HistoricalUnknown),
            "main 已发但 mirror offset 未提交时，fresh tracker lazy replay 必须 fail-closed"
        );
        assert_eq!(network_sends, 0);
    }
}

#[cfg(test)]
mod inbound_gate_mapping_tests {
    use super::*;

    #[test]
    fn disabled_inbound_produces_receipt_without_dispatching() {
        let root = tempfile::tempdir().unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(root.path().join("inbox"));
        let inbound = old_message(false);
        assert!(admit_agent_inbound(&wal, "agent-a", "scope-a", &inbound, true).unwrap());
        assert!(wal.scan_pending().is_empty());
        crate::runtime::agent_availability::AgentAvailability::lock(
            &root.path().join("inbox"),
            "agent-a",
        )
        .unwrap()
        .set_enabled(false)
        .unwrap();
        assert!(!admit_agent_inbound(&wal, "agent-a", "scope-a", &inbound, false).unwrap());
        assert!(!root.path().join("outbox").exists());
        assert!(!admit_agent_inbound(&wal, "agent-a", "scope-a", &inbound, true).unwrap());
        assert!(
            wal.read_consumed("agent-a", &inbound.message_id)
                .unwrap()
                .unwrap()
                .rejected_disabled
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("outbox/pending"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn disabled_steward_notice_keeps_explicit_roster_requirement() {
        let mut agent: Agent =
            serde_yaml::from_str("name: steward\nrole: steward\nfeishu:\n  allowed_users: []\n")
                .unwrap();
        let inbound = old_message(false);
        assert!(!disabled_notice_allowed(&agent, true, &inbound));
        agent.feishu.as_mut().unwrap().allowed_users = vec![inbound.sender.clone()];
        assert!(disabled_notice_allowed(&agent, false, &inbound));
        agent.feishu.as_mut().unwrap().allowed_users = vec!["another-user".into()];
        assert!(!disabled_notice_allowed(&agent, true, &inbound));
    }

    fn old_message(reattach: bool) -> Inbound {
        Inbound {
            anchor_message_id: None,
            chat_id: "oc_1".into(),
            chat_type: Some("p2p".into()),
            text: "上一代已经注入过的那条".into(),
            sender: "ou_1".into(),
            sender_union_id: String::new(),
            sender_type: "user".into(),
            message_id: if reattach { "om_reattach" } else { "om_plain" }.into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            // 一小时前发出:远超 DEFAULT_MAX_AGE_MS(10 分钟),普通入站到这儿必判陈旧。
            create_time_ms: 1_000_000_000_000,
            replayed: false,
            reattach,
        }
    }

    /// 接管入站必须复用原始 message_id 与 create_time_ms,而那一轮很可能已经跑了很久 ——
    /// 时效闸不放行的话,整条接管路径会被静默吞掉,只留一行「丢弃陈旧入站」。
    #[test]
    fn reattach_inbound_survives_the_staleness_gate_that_would_drop_a_plain_one() {
        let now_ms = 1_000_000_000_000 + 60 * 60 * 1000;
        let gate = summon_core::inbound::InboundGate::default();
        let plain = old_message(false);
        assert_eq!(
            gate.check(
                &plain.message_id,
                &plain.chat_id,
                gate_create_ms(&plain),
                now_ms
            ),
            summon_core::inbound::Admission::DropStale,
            "夹具本身必须真的会被时效闸丢,否则下面那条断言什么都没证明"
        );
        let reattached = old_message(true);
        assert_eq!(
            gate.check(
                &reattached.message_id,
                &reattached.chat_id,
                gate_create_ms(&reattached),
                now_ms
            ),
            summon_core::inbound::Admission::Admit
        );
    }

    /// 免检的只是时效/水位,去重照旧 —— 同一条接管消息投两次仍然只跑一次。
    #[test]
    fn reattach_inbound_is_still_deduplicated_by_message_id() {
        let now_ms = 1_000_000_000_000 + 60 * 60 * 1000;
        let mut gate = summon_core::inbound::InboundGate::default();
        let msg = old_message(true);
        assert_eq!(
            gate.check(&msg.message_id, &msg.chat_id, gate_create_ms(&msg), now_ms),
            summon_core::inbound::Admission::Admit
        );
        gate.record(&msg.message_id, &msg.chat_id, msg.create_time_ms);
        assert_eq!(
            gate.check(&msg.message_id, &msg.chat_id, gate_create_ms(&msg), now_ms),
            summon_core::inbound::Admission::DropDuplicate
        );
    }

    use summon_core::inbound::Admission;

    fn admitted_facts() -> StaleNoticeFacts {
        StaleNoticeFacts {
            is_steward: false,
            sender_is_peer: false,
            can_talk: true,
            is_bridge_command: false,
            auto_captures: true,
            mention: GroupMentionAdmission::Admit,
            create_ms: 1_000_000,
            now_ms: 1_000_000 + 11 * 60_000,
        }
    }

    fn owner_agent(extra: &str) -> Agent {
        serde_yaml::from_str(&format!(
            "name: bot\nfeishu:\n  allowed_users: [ou_owner]\n{extra}"
        ))
        .unwrap()
    }

    fn late_inbound(chat_type: &str, sender: &str, text: &str) -> Inbound {
        let mentions = if chat_type == "p2p" {
            Vec::new()
        } else {
            vec![summon_core::ipc::Mention {
                key: "@_user_1".into(),
                open_id: "ou_bot".into(),
                name: "bot".into(),
                ..Default::default()
            }]
        };
        Inbound {
            chat_id: "oc_late".into(),
            chat_type: Some(chat_type.into()),
            sender: sender.into(),
            text: text.into(),
            mentions,
            ..old_message(false)
        }
    }

    fn peers_with(
        chat_id: &str,
        open_id: &str,
    ) -> (tempfile::TempDir, crate::runtime::peers_store::PeersStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::runtime::peers_store::PeersStore::new(dir.path().join("peers"));
        store
            .add_peers(
                &chat_tag("group", chat_id),
                vec![summon_core::peers::Peer {
                    open_id: open_id.into(),
                    name: "peer".into(),
                    ..Default::default()
                }],
                Some(""),
            )
            .unwrap();
        (dir, store)
    }

    /// 提示是主动往外发东西:这条消息按时到了也不会被理的,晚到被丢也不出声。
    #[test]
    fn stale_notice_asks_every_gate_the_main_path_asks() {
        assert!(stale_notice_admitted(&admitted_facts()));

        type Tweak = fn(&mut StaleNoticeFacts);
        let silenced: [(&str, Tweak); 7] = [
            ("群里没 @ 本 bot", |f| {
                f.mention = GroupMentionAdmission::MissingSelfMention
            }),
            ("取不到 bot 身份", |f| {
                f.mention = GroupMentionAdmission::BotIdentityUnavailable
            }),
            ("发信人不在 talk 名单", |f| f.can_talk = false),
            ("发信人是 peer bot", |f| f.sender_is_peer = true),
            ("管家身份", |f| f.is_steward = true),
            ("cli 没有捕获通道", |f| f.auto_captures = false),
            ("create_time 未知", |f| f.create_ms = 0),
        ];
        for (why, tweak) in silenced {
            let mut facts = admitted_facts();
            tweak(&mut facts);
            assert!(!stale_notice_admitted(&facts), "{why} 时不该出声");
        }

        let mut bridge = admitted_facts();
        bridge.can_talk = false;
        bridge.is_bridge_command = true;
        assert!(
            stale_notice_admitted(&bridge),
            "bridge 不受 talk 名单约束,operator 的 /bridge 晚到被丢也要告诉他"
        );
    }

    #[test]
    fn stale_notice_reports_minutes_only_past_the_age_threshold() {
        let chat = StaleNoticeChat {
            consecutive_stale: 1,
            ..StaleNoticeChat::default()
        };
        let now = Instant::now();
        assert_eq!(
            stale_notice_text(&admitted_facts(), chat, now).as_deref(),
            Some(
                "⏳ 按我这边的时钟,这条消息是 11 分钟前发的,已判为过期,这次投递我没有执行。还需要的话请重发一次。"
            )
        );
        // 没超过时效门槛却被丢 = 水位在本机时钟落后时命中,差值偏小,不报数。
        let mut watermark_hit = admitted_facts();
        watermark_hit.now_ms = watermark_hit.create_ms + 60_000;
        assert_eq!(
            stale_notice_text(&watermark_hit, chat, now).as_deref(),
            Some("⏳ 这条消息已判为过期,这次投递我没有执行。还需要的话请重发一次。")
        );
    }

    /// 群里照回执惯例进原消息的 thread;私聊用不开 thread 的引用回复指明是哪条。
    #[test]
    fn stale_notice_replies_to_the_dropped_message() {
        let group = late_inbound("group", "ou_owner", "@_user_1 帮我看看");
        assert_eq!(
            stale_notice_reply_target(&group),
            Some((group.message_id.as_str(), true))
        );
        let p2p = late_inbound("p2p", "ou_owner", "帮我看看");
        assert_eq!(
            stale_notice_reply_target(&p2p),
            Some((p2p.message_id.as_str(), false))
        );
        let replayed = Inbound {
            anchor_message_id: Some("om_real".into()),
            ..late_inbound("p2p", "ou_owner", "帮我看看")
        };
        assert_eq!(
            stale_notice_reply_target(&replayed),
            Some(("om_real", false)),
            "重放的 message_id 是本地幂等键,回复只能落在原消息上"
        );
    }

    /// 事实全部从消息本身和本 agent 的配置算出来,与主路径同源。
    #[test]
    fn stale_notice_facts_come_from_the_message_and_the_agent() {
        let (_dir, peers) = peers_with("oc_late", "ou_peer_bot");
        let agent = owner_agent("");
        let bot = || Some("ou_bot".to_string());

        let stranger = late_inbound("group", "ou_stranger", "@_user_1 帮我看看");
        let facts = stale_notice_facts(&agent, &stranger, &peers, bot, 1, 2);
        assert!(!facts.can_talk && !facts.sender_is_peer && !facts.is_steward);
        assert!(facts.auto_captures);
        assert_eq!(facts.mention, GroupMentionAdmission::Admit);
        assert!(!facts.is_bridge_command);

        let other_bot = || Some("ou_other_bot".to_string());
        let facts = stale_notice_facts(&agent, &stranger, &peers, other_bot, 1, 2);
        assert_eq!(facts.mention, GroupMentionAdmission::MissingSelfMention);

        let peer = late_inbound("group", "ou_peer_bot", "@_user_1 帮我看看");
        let facts = stale_notice_facts(&agent, &peer, &peers, bot, 1, 2);
        assert!(facts.sender_is_peer, "peer 要从 peers 表里真的查出来");

        let bridge = late_inbound("group", "ou_stranger", "@_user_1 /bridge");
        let facts = stale_notice_facts(&agent, &bridge, &peers, bot, 1, 2);
        assert!(facts.is_bridge_command && !facts.can_talk);

        let owner = late_inbound("group", "ou_owner", "@_user_1 帮我看看");
        let steward = owner_agent("role: steward\n");
        assert!(stale_notice_facts(&steward, &owner, &peers, bot, 1, 2).is_steward);
        let no_capture = owner_agent("runtime:\n  cli: something-else\n");
        assert!(!stale_notice_facts(&no_capture, &owner, &peers, bot, 1, 2).auto_captures);

        // 私聊不打 HTTP 取 bot 身份,并且照样过 talk 名单。
        let p2p = late_inbound("p2p", "ou_stranger", "帮我看看");
        let facts =
            stale_notice_facts(&agent, &p2p, &peers, || panic!("私聊不该取 bot 身份"), 1, 2);
        assert_eq!(facts.mention, GroupMentionAdmission::Admit);
        assert!(!facts.can_talk);
    }

    /// 放行与重复都不出声;放行清零连击;事实只在判陈旧时才取。
    #[test]
    fn stale_notice_ledger_books_every_gate_verdict() {
        let mut ledger = StaleNoticeLedger::default();
        let now = Instant::now();
        let never = || -> StaleNoticeFacts { panic!("不是陈旧就不该取事实") };
        assert_eq!(
            ledger.after_gate(Admission::Admit, "oc_1", never, now),
            None
        );
        assert_eq!(
            ledger.after_gate(Admission::DropDuplicate, "oc_1", never, now),
            None
        );
        assert!(ledger
            .after_gate(Admission::DropStale, "oc_1", admitted_facts, now)
            .is_some());

        // 连击到上限后闭嘴;一条放行把它清零,冷却过后又能出声。
        let mut t = now;
        for _ in 0..STALE_NOTICE_MAX_CONSECUTIVE {
            t += STALE_NOTICE_COOLDOWN + Duration::from_secs(1);
            ledger.after_gate(Admission::DropStale, "oc_1", admitted_facts, t);
        }
        assert_eq!(
            ledger.after_gate(Admission::DropStale, "oc_1", admitted_facts, t),
            None
        );
        ledger.after_gate(Admission::Admit, "oc_1", never, t);
        t += STALE_NOTICE_COOLDOWN + Duration::from_secs(1);
        assert!(ledger
            .after_gate(Admission::DropStale, "oc_1", admitted_facts, t)
            .is_some());
    }

    /// 本来就不会理的陈旧消息不计连击:多 bot 群里两条 bot 消息先被丢,真人那条照样出声。
    #[test]
    fn stale_notice_streak_counts_only_messages_we_would_answer() {
        let mut ledger = StaleNoticeLedger::default();
        let now = Instant::now();
        let from_peer = || StaleNoticeFacts {
            sender_is_peer: true,
            ..admitted_facts()
        };
        let not_for_me = || StaleNoticeFacts {
            mention: GroupMentionAdmission::MissingSelfMention,
            ..admitted_facts()
        };
        for facts in [from_peer, not_for_me, from_peer, not_for_me] {
            assert_eq!(
                ledger.after_gate(Admission::DropStale, "oc_1", facts, now),
                None
            );
        }
        assert!(ledger
            .after_gate(Admission::DropStale, "oc_1", admitted_facts, now)
            .is_some());
    }

    /// 一次补投风暴只出一句,冷却按 chat 分桶。
    #[test]
    fn stale_notice_cools_down_per_chat() {
        fn stale(ledger: &mut StaleNoticeLedger, chat: &str, at: Instant) -> Option<String> {
            ledger.after_gate(Admission::DropStale, chat, admitted_facts, at)
        }
        let mut ledger = StaleNoticeLedger::default();
        let first = Instant::now();
        assert!(stale(&mut ledger, "oc_1", first).is_some());
        // 中间有正常消息放行,排除连击闸的干扰,单看冷却。
        ledger.after_gate(Admission::Admit, "oc_1", admitted_facts, first);
        assert_eq!(
            stale(&mut ledger, "oc_1", first + Duration::from_secs(1)),
            None,
            "冷却内第二条要压住"
        );
        assert!(stale(&mut ledger, "oc_2", first).is_some());
        ledger.after_gate(Admission::Admit, "oc_1", admitted_facts, first);
        let later = first + STALE_NOTICE_COOLDOWN + Duration::from_secs(1);
        assert!(stale(&mut ledger, "oc_1", later).is_some());
    }

    /// 连击窗口内一直没有放行 = 闸在批量误杀,只留日志;隔了很久的下一次陈旧是新的零星事件。
    #[test]
    fn stale_notice_goes_quiet_while_the_gate_drops_everything() {
        let mut ledger = StaleNoticeLedger::default();
        let mut now = Instant::now();
        let mut spoken = 0;
        for _ in 0..10 {
            if ledger
                .after_gate(Admission::DropStale, "oc_1", admitted_facts, now)
                .is_some()
            {
                spoken += 1;
            }
            now += STALE_NOTICE_COOLDOWN + Duration::from_secs(1);
        }
        assert_eq!(spoken, STALE_NOTICE_MAX_CONSECUTIVE);

        now += STALE_NOTICE_STREAK_WINDOW + Duration::from_secs(1);
        assert!(
            ledger
                .after_gate(Admission::DropStale, "oc_1", admitted_facts, now)
                .is_some(),
            "相隔超过连击窗口的偶发延迟不该被上一串拖累"
        );
    }
}

#[cfg(test)]
mod command_anchor_tests {
    use super::*;

    fn command(chat_type: &str) -> Inbound {
        Inbound {
            anchor_message_id: None,
            chat_id: "oc_1".into(),
            chat_type: Some(chat_type.into()),
            text: "/help".into(),
            sender: "ou_1".into(),
            sender_union_id: String::new(),
            sender_type: String::new(),
            message_id: "om_cmd".into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms: 1,
            replayed: false,
            reattach: false,
        }
    }

    /// 群主干上的指令也锚到它自己(回执因此进 thread);私聊没有 thread,照常发;
    /// 恢复重放的 message_id 是本地幂等键,锚点必须是真实的那条。
    #[test]
    fn session_panel_delivery_reports_failure_in_the_command_thread() {
        for fail in [false, true] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let fc = FeishuClient::with_test_base(format!("http://{}", server.server_addr()));
            let remote = std::thread::spawn(move || {
                let mut sends = Vec::new();
                while sends.len() < if fail { 2 } else { 1 } {
                    let mut request = server
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .unwrap()
                        .unwrap();
                    if request.url().contains("auth/v3/tenant_access_token") {
                        request
                            .respond(tiny_http::Response::from_string(
                                r#"{"tenant_access_token":"token","expire":7200}"#,
                            ))
                            .unwrap();
                        continue;
                    }
                    assert!(request.url().ends_with("/messages/om_panel_command/reply"));
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(body["reply_in_thread"], true);
                    sends.push(body);
                    let response = if fail && sends.len() == 1 {
                        tiny_http::Response::from_string(
                            r#"{"code":230099,"msg":"private diagnostic"}"#,
                        )
                        .with_status_code(400)
                    } else {
                        tiny_http::Response::from_string(
                            r#"{"code":0,"data":{"message_id":"om_panel_result"}}"#,
                        )
                    };
                    request.respond(response).unwrap();
                }
                assert!(server
                    .recv_timeout(std::time::Duration::from_millis(100))
                    .unwrap()
                    .is_none());
                sends
            });
            let dir = tempfile::tempdir().unwrap();
            let mut agent: Agent = serde_yaml::from_str("name: bot\n").unwrap();
            agent.dir = dir.path().join("agents/bot");
            std::fs::create_dir_all(&agent.dir).unwrap();
            let mut msg = command("group");
            msg.message_id = "om_panel_command".into();
            let card = summon_core::feishu::card_markdown_v2("Session 控制面板");
            let result = deliver_session_panel(&fc, &agent, &msg, "group", &card);
            let sends = remote.join().unwrap();
            assert_eq!(result.is_err(), fail);
            assert!(sends[0]["content"]
                .as_str()
                .unwrap()
                .contains("Session 控制面板"));
            if fail {
                let notice = sends[1]["content"].as_str().unwrap();
                assert!(notice.contains("会话控制面板暂时发送失败"));
                assert!(!notice.contains("private diagnostic"));
            }
            let archive = std::fs::read_to_string(
                agent
                    .state_dir()
                    .join("threads")
                    .join(chat_tag("group", &msg.chat_id))
                    .join("outbound.ndjson"),
            )
            .unwrap();
            assert_eq!(archive.lines().count(), 1);
            assert_eq!(archive.contains("markdown_card"), fail);
            assert!(!archive.contains("private diagnostic"));
        }
    }

    #[test]
    fn command_anchor_targets_the_command_message_only_in_groups() {
        let mut msg = command("group");
        assert_eq!(command_anchor(&msg), Some("om_cmd"));
        msg.anchor_message_id = Some("om_real".into());
        assert_eq!(command_anchor(&msg), Some("om_real"));
        assert_eq!(command_anchor(&command("topic_group")), Some("om_cmd"));
        assert_eq!(command_anchor(&command("p2p")), None);
        let mut blank = command("group");
        blank.message_id.clear();
        assert_eq!(command_anchor(&blank), None, "空 id 不能拿去 reply");
    }

    /// 显式开关是纯确认;裸 `/quota` 的回执要说翻到了哪边;写盘失败如实说。
    #[test]
    fn only_an_explicit_quota_switch_is_a_pure_confirmation() {
        assert!(matches!(quota_ack(true, true, Ok(())), CommandAck::Done(_)));
        assert!(matches!(
            quota_ack(true, false, Ok(())),
            CommandAck::Done(_)
        ));
        let toggled = quota_ack(false, false, Ok(()));
        assert!(matches!(toggled, CommandAck::Note(_)), "{toggled:?}");
        assert!(toggled.text().contains("关"), "{toggled:?}");
        let failed = quota_ack(true, true, Err(std::io::Error::other("磁盘满")));
        assert!(matches!(failed, CommandAck::Note(_)), "{failed:?}");
        assert!(failed.text().contains("磁盘满"), "{failed:?}");
    }

    /// 开 Fast 的回执带着多耗额度的提示,照常发;关掉是纯确认。
    #[test]
    fn turning_fast_on_keeps_its_cost_notice() {
        assert_eq!(fast_ack(true, "开", "关"), CommandAck::Note("开".into()));
        assert_eq!(fast_ack(false, "开", "关"), CommandAck::Done("关".into()));
    }

    /// 认识关系存下来才贴完成表情;没存下来要说出来,不能报成功。
    #[test]
    fn peers_ack_reports_a_failed_write_as_a_note() {
        assert_eq!(
            peers_ack("✅ 已在本群认识 bot:A".into(), Ok(())),
            CommandAck::Done("✅ 已在本群认识 bot:A".into())
        );
        let failed = peers_ack("✅ 已在本群认识 bot:A".into(), Err(anyhow::anyhow!("只读")));
        assert!(matches!(failed, CommandAck::Note(_)), "{failed:?}");
        assert!(failed.text().contains("只读"), "{failed:?}");
        assert!(!failed.text().contains("✅"), "{failed:?}");
    }
}

#[cfg(test)]
mod terminal_command_tests {
    use super::*;

    /// 「换一组新链接」的卡在私聊里,集中闸按私聊语义判会判错,交 handler 按目标 chat 重验;
    /// 「打开终端」的卡就在目标 chat 里,集中闸的判法正确,不能被豁免。
    #[test]
    fn only_terminal_regeneration_skips_the_central_card_gate() {
        use summon_core::feishu::CardAction;
        assert!(checks_operate_in_handler(CardAction::TerminalRegenerate));
        assert!(
            checks_operate_in_handler(CardAction::SetWorkdirTo),
            "改目录候选卡在私聊里,集中闸只知道私聊,得由 handler 按目标会话判"
        );
        assert!(!checks_operate_in_handler(CardAction::TerminalOpen));
    }

    /// `/summon-export` 与 `/dash` 同一位置就地处理:先于入站 WAL 与 worker 投递,过 operate-gate,
    /// 导出交给独立线程,无权限的回执只经 `deliver_dash_notice` 说给发起人。
    #[test]
    fn export_is_handled_before_the_worker_queue() {
        let source = include_str!("dispatcher.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("dispatcher production source");
        let start = production
            .find("super::export_entry::export_mode(")
            .expect("/summon-export 分派入口");
        let end = start
            + production[start..]
                .find("super::terminal::is_terminal_command(")
                .expect("/summon-export 分支收口在 /term 之前");
        let branch = &production[start..end];
        assert!(branch.contains("command_is_addressed("), "{branch}");
        assert!(branch.contains("can_operate("), "{branch}");
        assert!(branch.contains("OperateCommand::Export.scope("), "{branch}");
        assert!(branch.contains("super::export_entry::spawn("), "{branch}");
        assert!(
            branch.contains("deliver_dash_notice(") && !branch.contains("send_markdown"),
            "回执只经 deliver_dash_notice 说给发起人,不直接回到群里: {branch}"
        );
        assert!(branch.contains("continue;"));
        assert!(!branch.contains("ThreadMsg::Task"));
        let wal = production
            .find("after_durable_inbox(&inbox_wal")
            .expect("入站 WAL");
        assert!(
            start < wal,
            "/summon-export 必须先于入站 WAL 与 worker 投递"
        );
    }

    /// `/dash` 与 `/term` 同一位置就地处理:先于入站 WAL 与 worker 投递,过 operate-gate,签发
    /// 走 `send_dashboard_card`。
    #[test]
    fn dash_is_handled_before_the_worker_queue() {
        let source = include_str!("dispatcher.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("dispatcher production source");
        let start = production
            .find("super::dashboard_entry::is_dashboard_command(")
            .expect("/dash 分派入口");
        let end = start
            + production[start..]
                .find("super::export_entry::export_mode(")
                .expect("/dash 分支收口在 /summon-export 之前");
        let branch = &production[start..end];
        assert!(branch.contains("can_operate("), "{branch}");
        assert!(branch.contains("OperateCommand::Dash.scope("), "{branch}");
        assert!(branch.contains("super::dashboard_entry::send_dashboard_card("));
        assert!(
            branch.contains("deliver_dash_notice(") && !branch.contains("send_markdown"),
            "回执只经 deliver_dash_notice 说给发起人,不直接回到群里: {branch}"
        );
        assert!(branch.contains("continue;"));
        assert!(!branch.contains("ThreadMsg::Task"));
        let wal = production
            .find("after_durable_inbox(&inbox_wal")
            .expect("入站 WAL");
        assert!(start < wal, "/dash 必须先于入站 WAL 与 worker 投递");
    }

    /// 召回卡的「遥控」在私聊卡上,与「接管」一样交 handler 按目标群重验。
    #[test]
    fn recall_cmux_remote_skips_the_central_card_gate() {
        use summon_core::feishu::CardAction;
        assert!(checks_operate_in_handler(CardAction::RecallCmuxRemote));
    }

    /// `/term` 在 dispatcher 就地处理:先于入站 WAL 与 worker 投递,过 operate-gate,签发走共用的
    /// `send_terminal_card`。进了 worker 队列就得排在正卡着的那一轮后面。
    #[test]
    fn term_is_handled_before_the_worker_queue() {
        let source = include_str!("dispatcher.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("dispatcher production source");
        let start = production
            .find("super::terminal::is_terminal_command(")
            .expect("/term 分派入口");
        let end = start
            + production[start..]
                .find("summon_core::ipc::model_command(")
                .expect("/term 分支收口");
        let branch = &production[start..end];
        assert!(branch.contains("can_operate("), "{branch}");
        assert!(branch.contains("super::terminal::send_terminal_card("));
        assert!(!branch.contains("ThreadMsg::Task"));
        let wal = production
            .find("after_durable_inbox(&inbox_wal")
            .expect("入站 WAL");
        assert!(start < wal, "/term 必须先于入站 WAL 与 worker 投递");
    }
}

#[cfg(test)]
mod mirror_rejection_tests {
    use super::*;

    /// 审核拦下的一段改发说明、算作发过(watcher 继续、offset 推进);别的失败与说明也发不出去
    /// 时按发送失败处理。
    #[test]
    fn audit_rejected_mirror_block_is_replaced_by_a_notice() {
        let rejected = || {
            Err(anyhow::Error::new(
                crate::frontend::client::ContentRejected::for_test(
                    "contain sensitive data: EMAIL_ADDRESS",
                ),
            ))
        };
        let mut notices = Vec::new();
        send_or_explain_rejection("tester", rejected, |notice| {
            notices.push(notice.as_str().to_string());
            Ok(())
        })
        .expect("说明发出去了,这一段就算发过");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("飞书内容审核拦下"));
        assert!(notices[0].contains("EMAIL_ADDRESS"));

        let network = send_or_explain_rejection(
            "tester",
            || Err(anyhow::anyhow!("send_card 失败 502 Bad Gateway")),
            |_| panic!("网络失败不改发说明"),
        );
        assert!(network.is_err());

        let explain_failed =
            send_or_explain_rejection("tester", rejected, |_| Err(anyhow::anyhow!("说明也被拒")));
        assert!(explain_failed.is_err(), "说明也发不出去 = 发送失败");
        assert_eq!(
            rejected_notice_uuid("summon-mirror-ab"),
            "summon-mirror-ab-r"
        );
    }
    /// 镜像实际走的那一条:原 uuid 被 230028 拒 → 以 `{uuid}-r` 发说明 → 这一块算发过。
    #[test]
    fn mirror_block_sends_the_notice_under_a_derived_key_after_rejection() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            let mut sends = Vec::new();
            while sends.len() < 2 {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .expect("原文被拒后应改发说明");
                if request.url().contains("auth/v3/tenant_access_token") {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"code":0,"tenant_access_token":"t","expire":7200}"#,
                        ))
                        .unwrap();
                    continue;
                }
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let response = if sends.is_empty() {
                    tiny_http::Response::from_string(
                        r#"{"code":230028,"msg":"The messages do NOT pass the audit, ext=contain sensitive data: EMAIL_ADDRESS"}"#,
                    )
                    .with_status_code(400)
                } else {
                    tiny_http::Response::from_string(
                        r#"{"code":0,"data":{"message_id":"om_notice"}}"#,
                    )
                };
                sends.push(body);
                request.respond(response).unwrap();
            }
            sends
        });

        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base(base);
        let rendered = summon_core::feishu::render_external_lark_md(
            "登录用 someone@example.com",
            &[],
            &std::collections::HashMap::new(),
        );
        let result = send_mirror_block(
            &fc,
            "tester",
            dir.path(),
            "group",
            "oc_mirror",
            &rendered,
            &[],
            "summon-mirror-k",
        );
        let sends = server_thread.join().unwrap();

        result.expect("说明发出去了,这一块就算发过");
        assert!(
            sends[0].contains(r#""uuid":"summon-mirror-k""#),
            "{}",
            sends[0]
        );
        assert!(
            sends[1].contains(r#""uuid":"summon-mirror-k-r""#),
            "{}",
            sends[1]
        );
        assert!(sends[1].contains("飞书内容审核拦下"), "{}", sends[1]);
    }
}

#[cfg(test)]
mod agent_yaml_reload_tests {
    use super::*;

    fn handle(tx: Sender<ThreadMsg>) -> ChatHandle {
        ChatHandle {
            tx,
            last_activity: SystemTime::now(),
            idle_prompted: false,
            chat_type: "group".into(),
            tmux_name: "soul-test".into(),
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        }
    }

    /// agent.yaml 改了:每个在线 chat 线程都收到就地重读通知(两轮之间重读,队列不丢)。
    #[test]
    fn a_changed_agent_yaml_reloads_every_online_chat() {
        let agent: Agent = serde_yaml::from_str("name: coder\n").unwrap();
        let core = crate::runtime::core::Core::new(HashMap::new(), HashMap::new(), vec![]);
        let (tx_a, rx_a) = std::sync::mpsc::channel();
        let (tx_b, rx_b) = std::sync::mpsc::channel();
        let chats = HashMap::from([
            ("oc_a".to_string(), handle(tx_a)),
            ("oc_b".to_string(), handle(tx_b)),
        ]);
        agent_yaml_changed(&core, &agent, &chats);
        assert!(matches!(rx_a.try_recv(), Ok(ThreadMsg::AgentReload)));
        assert!(matches!(rx_b.try_recv(), Ok(ThreadMsg::AgentReload)));
    }

    /// agent.yaml 改了:sentinel 召唤名单、`/handoff` 目标、outbox 读的投递表跟着换成新的那份。
    #[test]
    fn a_changed_agent_yaml_refreshes_the_delivery_table() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("coder");
        std::fs::create_dir_all(&dir).unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: coder\ndisplay: 旧名字\nfeishu:\n  app_id_env: T_APP_ID\n")
                .unwrap();
        agent.dir = dir;
        let core =
            crate::runtime::core::Core::new(HashMap::new(), HashMap::new(), vec![agent.clone()]);
        core.attach_agent_identities(Arc::new(std::sync::RwLock::new(HashMap::from([(
            "coder".to_string(),
            super::drain_agent_for_test(&agent, "app-a", "secret-a"),
        )]))));
        let mut fresh = agent.clone();
        fresh.display = "新名字".into();
        agent_yaml_changed(&core, &fresh, &HashMap::new());
        let identities = core.local_agent_identities();
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].agent.display, "新名字");
    }

    /// 停机补发批准每个 chat 每代只领得到一次:重建的 watcher 不再按补发放行,本代主路径投递过
    /// 的轮只观察不重发。没获批的一直是 `None`。
    #[test]
    fn the_downtime_catchup_floor_is_handed_out_once_per_chat() {
        let consumed = std::sync::Mutex::new(std::collections::BTreeSet::new());
        assert_eq!(
            super::take_catchup_floor(Some(42), &consumed, "coder", "gabc"),
            Some(42)
        );
        assert_eq!(
            super::take_catchup_floor(Some(42), &consumed, "coder", "gabc"),
            None,
            "重建的 watcher 不再领到补发下界"
        );
        assert_eq!(
            super::take_catchup_floor(Some(7), &consumed, "coder", "gdef"),
            Some(7),
            "别的 chat 各领各的"
        );
        assert_eq!(
            super::take_catchup_floor(None, &consumed, "coder", "gxyz"),
            None
        );
    }

    /// 接线:重读成功(身份没变)且 agent.yaml 变了才刷新快照、通知 chat 线程;config.yaml 变了
    /// 重装外部工具路径快照。真实时序要真 daemon,只能静态钉住。
    #[test]
    fn the_dispatcher_reloads_agent_and_tool_snapshots_on_change() {
        let source = include_str!("dispatcher.rs");
        let production = source
            .split("\n#[cfg(test)]\nmod ")
            .next()
            .unwrap_or_default();
        let squashed: String = production
            .lines()
            .map(|line| line.split("//").next().unwrap_or_default())
            .collect::<String>()
            .split_whitespace()
            .collect();
        assert!(
            squashed.contains(
                "Some(fresh)iffresh.is_routable()&&epoch_identity.matches_current(&fresh)=>{agent=fresh;route_identity_blocked=false;ifcur_manifest!=manifest_mtime{agent_yaml_changed(&core,&agent,&chats);}}"
            ),
            "身份没变的重读成功后要刷新快照并通知在线 chat 线程"
        );
        assert!(
            squashed.contains(
                "ifcur_cfg!=cfg_mtime{letfresh_config=summon_core::config::Config::load();summon_core::tools::configure(&fresh_config);crate::runtime::localization::reload(&fresh_config);}"
            ),
            "config.yaml 变了要重装外部工具路径快照"
        );
        let chat_thread: String = include_str!("chat_thread.rs").split_whitespace().collect();
        let claim = chat_thread
            .find("letmutcatchup_floor=super::take_mirror_catchup_floor(&agent.name,")
            .expect("chat 线程起来时领取补发下界");
        let entry = chat_thread[claim..]
            .find("loop{matchchat_thread_run(")
            .expect("领取在进入循环之前");
        assert!(
            chat_thread[claim + entry..].contains("catchup_floor.take(),"),
            "补发下界只交给第一次进入"
        );
        assert!(
            !squashed.contains("take_mirror_catchup_floor(&agent.name,&chat_tag("),
            "watcher 不能自己按批准表领取"
        );
    }
}

#[cfg(test)]
mod dashboard_interrupt_tests {
    use super::super::DashInterrupt;
    use super::*;
    use crate::runtime::current_turn_store::{
        CurrentTurnContext, CurrentTurnLease, CurrentTurnOrigin, CurrentTurnStore,
    };

    fn handle(tx: Sender<ThreadMsg>) -> ChatHandle {
        ChatHandle {
            tx,
            last_activity: SystemTime::now(),
            idle_prompted: false,
            chat_type: "group".into(),
            tmux_name: "soul-test".into(),
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        }
    }

    fn agent_in(root: &std::path::Path) -> Agent {
        let mut agent: Agent = serde_yaml::from_str("name: coder\n").unwrap();
        agent.dir = root.join("agents").join("coder");
        agent
    }

    /// 在这个会话上登记一轮在跑的 turn:打断围栏比对的就是它的凭证。
    fn running(agent: &Agent, chat_type: &str, chat_id: &str, turn_id: &str) -> CurrentTurnLease {
        let context = CurrentTurnContext {
            turn_id: turn_id.into(),
            agent_alias: "coder".into(),
            delivery_scope: "test-scope".into(),
            chat_id: chat_id.into(),
            chat_type: chat_type.into(),
            conversation_tag: chat_tag(chat_type, chat_id),
            message_id: "om_running".into(),
            origin: CurrentTurnOrigin::Feishu,
            schedule_output: summon_core::ScheduleOutput::Progress,
            requester_open_id: "ou_asker".into(),
            requester_union_id: String::new(),
            requester_is_bot: false,
            requester_is_peer: false,
            thread_id: None,
            root_id: None,
            started_at_ms: chrono::Utc::now().timestamp_millis() - 1_000,
            reattached: false,
        };
        CurrentTurnStore::for_agent(&agent.state_dir())
            .activate(&context)
            .unwrap()
    }

    /// 页面带来的凭证正是在跑的这一轮:打断交给 chat 线程。
    #[test]
    fn dashboard_interrupt_with_the_current_turn_id_reaches_the_chat_thread() {
        let root = tempfile::tempdir().unwrap();
        let agent = agent_in(root.path());
        let _lease = running(&agent, "group", "oc_run", "turn-now");
        let (tx, rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([("oc_run".to_string(), handle(tx))]);
        assert_eq!(
            dashboard_interrupt(&agent, &chats, "oc_run", "turn-now"),
            DashInterrupt::Sent
        );
        assert!(matches!(rx.try_recv(), Ok(ThreadMsg::Interrupt)));
    }

    /// 页面看到的那一轮已经结束、或没带凭证:不动手,chat 线程什么都收不到 —— 不打断之后开跑的新一轮。
    #[test]
    fn dashboard_interrupt_with_a_stale_or_missing_turn_id_sends_nothing() {
        let root = tempfile::tempdir().unwrap();
        let agent = agent_in(root.path());
        let _lease = running(&agent, "group", "oc_run", "turn-new");
        let (tx, rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([("oc_run".to_string(), handle(tx))]);
        assert_eq!(
            dashboard_interrupt(&agent, &chats, "oc_run", "turn-old"),
            DashInterrupt::Stale
        );
        assert_eq!(
            dashboard_interrupt(&agent, &chats, "oc_run", ""),
            DashInterrupt::Stale
        );
        assert!(rx.try_recv().is_err());
    }

    /// 凭证对得上、但这个会话此刻没有能收消息的线程:报没在跑。
    #[test]
    fn dashboard_interrupt_without_a_chat_thread_reports_not_running() {
        let root = tempfile::tempdir().unwrap();
        let agent = agent_in(root.path());
        // 不在线的 chat 按私聊推 tag(见 `card_chat_type`)。
        let _p2p = running(&agent, "p2p", "oc_gone", "turn-p2p");
        assert_eq!(
            dashboard_interrupt(&agent, &HashMap::new(), "oc_gone", "turn-p2p"),
            DashInterrupt::NotRunning
        );
        let _group = running(&agent, "group", "oc_closed", "turn-group");
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let chats = HashMap::from([("oc_closed".to_string(), handle(tx))]);
        assert_eq!(
            dashboard_interrupt(&agent, &chats, "oc_closed", "turn-group"),
            DashInterrupt::NotRunning
        );
    }
}
