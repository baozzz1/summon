//! 会话拆除与退出回执(A1 拆分):do_teardown、notify_session_exited/deliver_notify、
//! agents kill/群解散路由。

use super::*;

/// 拆除会话。`reset`=true:清本 chat 的 resume 记录(下次来消息冷启=真重置,不 resume);=false
/// 保留记录(下次 CLI 原生 resume 续接,= 续聊语义)。最后一律 kill tmux 会话释放进程。
/// 组合:Clean=(!reset,留记录续聊);「退出/新建」按钮 + 改 model/work_dir 的快重置=(reset)。
/// resume 记录已收口进 `resume_store`(键=会话名)。reset 走 `clear_chat`:按 `sess.tmux.session`
/// 清新记录,再按 `tag` 兜底清迁移期旧无头残留 `sessions/<tag>/session_id`——单一入口,不会漏清
///(arch-review C5/C10:旧实现分两套存储、只清 tmux 路,无头 agent reset 失效)。
/// 一次拆除实际改动了什么。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TeardownEffect {
    /// 拆除前还有活的 tmux 会话。
    pub(super) had_tmux: bool,
    /// reset 清掉了一份确实存在的会话记录。
    pub(super) cleared_record: bool,
}

pub(super) fn do_teardown(agent: &Agent, sess: &Session, tag: &str, reset: bool) -> TeardownEffect {
    do_teardown_inner(agent, sess, tag, reset, true)
}

pub(super) fn do_teardown_preserving_previous(
    agent: &Agent,
    sess: &Session,
    tag: &str,
) -> TeardownEffect {
    do_teardown_inner(agent, sess, tag, false, false)
}

fn do_teardown_inner(
    agent: &Agent,
    sess: &Session,
    tag: &str,
    reset: bool,
    remember: bool,
) -> TeardownEffect {
    let mut effect = TeardownEffect::default();
    if let Err(error) = if remember {
        crate::session::resume_store::remember_chat_checked(agent, &sess.tmux.session, tag)
    } else {
        Ok(())
    } {
        log(&summon_i18n::format!("r9b1a9e2154", agent.name));
    }
    if reset {
        // 真重置:清单一 resume 记录(新路径 + 迁移期旧无头残留),下次冷启不 resume。
        effect.cleared_record =
            crate::session::resume_store::clear_chat(agent, &sess.tmux.session, tag);
        log(&summon_i18n::format!(
            "r8ae05184e9",
            agent.name,
            if effect.cleared_record {
                ""
            } else {
                summon_i18n::text!("ree322f0fea")
            }
        ));
    }
    // 没有活动 tmux 会话(如 `/session` 唤出面板但 agent 从未冷启,或会话已被前一个动作拆掉)→ 无可 kill。
    if sess.tmux.has_session() {
        effect.had_tmux = true;
        if let Err(e) = sess.tmux.kill_session() {
            log(&summon_i18n::format!("r665179af1b", agent.name));
        }
    }
    // 后台任务随会话一起结束。
    crate::runtime::background_holder::BackgroundHolderStore::for_agent(&agent.state_dir())
        .clear(tag);
    // 记「路由生命周期已退休」。**无条件写,不看上面 kill 成没成功** —— 本事实的定义是「summon
    // 不再为这个 chat 维持路由」,不是「资源确认消失」。kill 失败属于另一回事,它会被判活识别成
    // `ClosedButProcessLive` 显眼地留在列表里,而不是在这里被悄悄吞掉。
    //
    // 收口在这里的理由:6 个拆除调用点(面板 Clean/Reset、群解散、bot 被移除、KillTeardown 等)
    // 无一例外都要过 `do_teardown`,挂这一处即全覆盖。这与 `exit_mark` 不同——那个只在**入队退出
    // 回执**时写,面板 PATCH 成功的场景根本不写,拿它判活会整片漏掉。
    crate::session::lifecycle::mark_closed(agent, tag);
    effect
}

/// 用户在群里显式重置(面板「退出(新建)」、`/session reset`、exact `/reset`)才算「本 chat
/// 从零开始」;改 model/工作目录顺带的快重置、定时尾轮自动重置都不算。
/// 前者要连同本 chat 未处理的恢复卡一起作废,后两者不是用户在做决定,不能替用户丢消息。
pub(super) fn reset_discards_recovery(notify_exit: bool, idle_retired: bool) -> bool {
    notify_exit && !idle_retired
}

/// 作废本 chat 名下全部未决恢复批次,返回作废的批次数。
///
/// 先把 inbox 侧 batch 写成脱敏终态(连同 replay/source pending),成功后才归档对应的
/// RecoveryCard 出站事件:inbox 侧没收口时卡必须留着,那是用户处理这批消息的唯一入口。
/// 已发出去的恢复卡不 PATCH —— 之后再点会落到「这张恢复卡已经处理完或失效」。
pub(super) fn terminalize_chat_recovery(
    inbox: &crate::runtime::inbox_wal::InboxWal,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    scope: &str,
    agent: &str,
    chat_id: &str,
) -> anyhow::Result<usize> {
    use anyhow::Context as _;
    let batches = inbox
        .terminalize_recovery_chat(scope, agent, chat_id)
        .context(summon_i18n::text!("r6a87ae895b"))?;
    outbox
        .terminalize_recovery_chat(scope, agent, chat_id)
        .context(summon_i18n::text!("rc698cc76c6"))?;
    Ok(batches.len())
}

/// `agents kill` 要不要发退出回执:只有真关掉了在跑的东西、或清掉了一份会话记录才发。
/// 会话早已退出(比如用户先在空闲提醒卡上点了退出)时再 kill 一次,什么都没改变,
/// 这时发「会话已关闭」只会让人以为又被关了一次。
pub(super) fn kill_changed_anything(stopped_live: bool, effect: TeardownEffect) -> bool {
    stopped_live || effect.had_tmux || effect.cleared_record
}

pub(super) fn gone_session_names(
    agent: &Agent,
    chat_id: &str,
    primary_session: &str,
) -> Vec<String> {
    let mut sessions: Vec<String> = chat_sessions_on_disk(agent, chat_id)
        .into_iter()
        .map(|(session, _, _)| session)
        .collect();
    // `.meta` 是 best-effort，丢失/损坏不能让真实 tmux 变成不可寻址孤儿。group
    // session 的 canonical 名由 agent+chat_id 纯函数可导，Gone 规划无条件纳入。
    let canonical = agent.tmux_name_for_chat("group", chat_id, false);
    if !sessions.iter().any(|session| session == &canonical) {
        sessions.push(canonical);
    }
    if !primary_session.is_empty() && !sessions.iter().any(|session| session == primary_session) {
        sessions.push(primary_session.to_string());
    }
    sessions.sort();
    sessions.dedup();
    sessions
}

/// 群解散/bot 被移除是允许终止持久 tmux 的显式事件。历史版本可能给同一 chat 留下裸名与
/// tag 名两份会话；必须全部拆除，再删除元数据，否则另一份会变成不可寻址孤儿。
pub(super) fn teardown_gone_sessions(
    agent: &Agent,
    chat_id: &str,
    primary_session: &str,
    tag: &str,
    poll: u64,
    debounce: u64,
) -> usize {
    let sessions = gone_session_names(agent, chat_id, primary_session);
    for session in &sessions {
        let sess = Session::with_name(agent, session.clone(), poll, debounce);
        do_teardown(agent, &sess, tag, false);
    }
    sessions.len()
}

/// 退出动作收尾:把触发该动作的会话面板卡片 PATCH 成「已退出」终态(去掉按钮,避免再次点击)。
/// `card_message_id` 为 None(非卡片触发)直接跳过。best-effort,失败仅记日志、不影响已完成的拆除。
///
/// 返回**原卡是否已承载这次退出的终态**:true = 用户点的那张卡就地变成了终态、结果已经写在
/// 他眼前;false = 没有原卡可改(非卡片触发)或 PATCH 失败(卡片过期等)。调用方据此决定还发不发
/// 退出回执卡(见 [`should_notify_exit`])——两者都做就是同一件事在群里说两遍。
// 参数即收尾所需的卡片字段(fc/agent/chat/mid/models/reset)+ origin 选布局,逐项透传,合并无意义。
#[allow(clippy::too_many_arguments)]
pub(super) fn mark_session_card_closed(
    fc: &FeishuClient,
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    card_message_id: Option<&str>,
    models: &[String],
    reset: bool,
    origin: CardOrigin,
) -> bool {
    let Some(mid) = card_message_id else {
        return false;
    };
    // 空闲卡收尾:保留空闲布局(绝不能走面板渲染,否则布局漂移成 /session 的样子——本次修的 bug)。
    if let CardOrigin::Idle(state) = origin {
        let reason = if reset {
            summon_core::feishu::IdleClosedReason::Reset
        } else {
            summon_core::feishu::IdleClosedReason::Clean
        };
        // 这里**不**走 `patch_idle_closed_or_reply`:PATCH 失败有退出回执卡兜底,再补一条 markdown
        // 就成了两条。保活/compact 那两条路没有回执兜底,才需要 markdown 版本。
        return patch_idle_closed(fc, agent, chat_type, chat_id, mid, state, reason);
    }
    // 面板卡收尾:重渲染面板原貌(控件置灰)+ footer:复用面板的 cli/通道/model/effort/models。
    // 终态卡片按钮已全置灰,effort 按钮行纯展示无用,故传空 efforts 不渲染——避免把 messaging 穿进
    // teardown 路径(红线附近,保持依赖最小)。摘要区仍如实标来源:会话已退,无实时/默认桶可查,故
    // model 按本群生效值解析(本群覆盖 > 配置,rpc-aware,无 summon 默认兜底),effort/通道走纯函数。
    // A(隐私):终态卡与面板一致,**不带工作目录路径**(它同样是群可见面,路径不进群)。
    let tag = chat_tag(chat_type, chat_id);
    let effective = crate::runtime::chat_model::effective(agent, Some(&tag));
    let (agent_model, model_overridden) = if cards::uses_rpc_model(agent) {
        let target = crate::capture::cursor_config::load_target(agent, &tag).ok();
        let overridden = target
            .as_ref()
            .is_some_and(|target| target.chat.selected_model().is_some());
        let model = target
            .and_then(|target| target.model().map(str::to_string))
            .or_else(|| agent.rpc_model().map(str::to_string));
        (model, overridden)
    } else {
        (
            effective.model.clone(),
            effective.model_layer == crate::runtime::chat_model::Layer::Chat,
        )
    };
    let resolved_model = if model_overridden {
        summon_core::model_effort::ResolvedModel {
            value: agent_model,
            source: summon_core::model_effort::ModelSource::ChatOverride,
        }
    } else {
        summon_core::model_effort::resolve_model(
            None,
            agent_model.as_deref(),
            &summon_core::model_effort::DefaultModel::default(),
        )
    };
    let resolved_effort = if effective.effort_layer == crate::runtime::chat_model::Layer::Chat {
        summon_core::model_effort::ResolvedEffort {
            value: effective.effort.clone(),
            source: summon_core::model_effort::EffortSource::ChatOverride,
        }
    } else {
        summon_core::model_effort::resolve_effort(agent.cli_kind(), effective.effort())
    };
    let channel = summon_core::model_effort::channel_label(agent.cli_kind(), agent.capture_mode());
    let card = summon_core::feishu::card_session_closed(
        &chat_tag(chat_type, chat_id),
        agent.cli_kind(),
        channel,
        &resolved_model,
        &resolved_effort,
        models,
        &[],
        cards::session_panel_controls(
            agent,
            &chat_tag(chat_type, chat_id),
            &crate::runtime::daemon::chat_meta::chat_session_name(agent, chat_type, chat_id),
            &resolved_model,
        ),
        reset,
    );
    match update_card_archived(fc, &agent.state_dir(), chat_type, chat_id, mid, &card) {
        Ok(()) => true,
        Err(e) => {
            log(&summon_i18n::format!("r20f8d83039", agent.name));
            false
        }
    }
}

/// 卡片按钮触发的退出还要不要再发一张退出回执卡。
///
/// `notify_exit` = 这次拆除是不是「用户主动退出」(配置变更快重置就不是);
/// `card_closed` = 用户点的那张卡是否已就地变成终态。两者都成立时**不发**——结果已经写在他刚点的
/// 卡片上,再来一张「♻️ 会话已重置」是同一件事说两遍。
///
/// 反过来 PATCH 没成功(卡片过期、网络失败)就必须回落到回执:回执走 outbox(先落盘再联网、
/// at-least-once、跨重启补发),是这条链路上唯一有投递保障的一环,不能让退出对用户彻底静默。
///
/// 顺带的取舍:不发回执 = 不写 `exit_mark`(标记只在入队回执时写)。这不构成回归——标记是在
/// `settle_chat_exit` 摘掉 handle **之后**才写的,本就晚于 idle-scan 那段窗口,对它没有保护作用。
pub(super) fn should_notify_exit(notify_exit: bool, card_closed: bool) -> bool {
    notify_exit && !card_closed
}

/// 空闲提醒卡被点(退出/保活/compact)后的收尾**唯一收口**:保留空闲布局、控件全置灰 + 「你已交互」
/// footer 原地 PATCH。返回是否 PATCH 成功——调用方据此决定要不要另找出路告诉用户结果。
pub(super) fn patch_idle_closed(
    fc: &FeishuClient,
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    mid: &str,
    state: IdleCardState,
    reason: summon_core::feishu::IdleClosedReason,
) -> bool {
    let tag = chat_tag(chat_type, chat_id);
    let card = summon_core::feishu::card_idle_closed_with_context(
        &tag,
        state.idle_secs,
        reason,
        state.context.as_ref(),
    );
    match update_card_archived(fc, &agent.state_dir(), chat_type, chat_id, mid, &card) {
        Ok(()) => true,
        Err(e) => {
            log(&summon_i18n::format!("r376caf403a", agent.name));
            false
        }
    }
}

/// 会话仍在的空闲卡收尾(保活 / compact):PATCH 失败时**没有**退出回执卡兜底,故退回一条普通
/// markdown(同一 footer 文案),保证用户总能看到结果。退出路径不用这个版本——它有回执兜底,
/// 再补 markdown 就是双发。
pub(super) fn patch_idle_closed_or_reply(
    fc: &FeishuClient,
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    mid: &str,
    state: IdleCardState,
    reason: summon_core::feishu::IdleClosedReason,
) {
    if patch_idle_closed(fc, agent, chat_type, chat_id, mid, state, reason) {
        return;
    }
    let _ = send_markdown_archived(
        fc,
        &agent.state_dir(),
        chat_type,
        chat_id,
        &summon_i18n::format!("r98d33a9049", reason.footer_tail()),
    );
}

/// 会话退出统一回执(两条退出路径 + 两个外部事件**唯一**调用):群仍在 → 发进原 chat;
/// 群已不在(GroupDisbanded/BotRemoved,或发原 chat 失败)→ p2p 通知群主。
/// best-effort:失败仅 log,绝不影响已完成的拆除。文案走 messaging(不 hardcode)。
/// `creator` = `.meta` 记的群主近似;`event_operator` = 事件带的操作者(仅事件路径有)。
#[allow(clippy::too_many_arguments)] // 各参数职责独立(fc/agent/messaging/chat/creator/operator/reason)
/// 内联退出回执(在线/同步路径):best-effort 包一层 [`deliver_notify`],失败只 log。
/// 行为与接缝前一致——文案空则不发、群在先发原 chat 失败回退 p2p、无目标诚实放弃。
pub(super) fn notify_session_exited(
    fc: &FeishuClient,
    agent: &Agent,
    messaging: &Messaging,
    chat_type: &str,
    chat_id: &str,
    chat_name: Option<&str>,
    creator: Option<&str>,
    event_operator: Option<&str>,
    reason: summon_core::exit::ExitReason,
    session_created_at: Option<&str>,
) {
    let text = messaging.render_session_exited_text(reason, chat_name, chat_id);
    // 内联路径不经重启,卡片就地渲染(传 None),不需要定格。
    if let Err(e) = deliver_notify(
        fc,
        agent,
        chat_type,
        chat_id,
        chat_name,
        creator,
        event_operator,
        reason,
        &text,
        None,
        session_created_at,
    ) {
        log(&summon_i18n::format!("r3a5d9fd373", agent.name));
    }
    // 谁发出回执谁写标记,让事后探测路径能识别「这次退出已经有人负责了」。
    // 投递失败也写——失败的是网络,不是「该不该发」的判定;让 idle-scan 再合成一条语义
    // 重复的回执并不能补救,只会变成双发。
    crate::session::exit_mark::mark(agent, chat_type, chat_id);
}

/// 退出回执的**投递函数体**(被内联 `notify_session_exited` 与 outbox drain 消费者共用)。
/// `text` 由调用方算好/快照(messaging 文案);空文案 → 直接 `Ok`(诚实不发,不算失败)。
///
/// 返回 `Err` **仅当真正投递失败**(原 chat 与 p2p 群主都没发出去 / 无目标可通知)——drain
/// 消费者据此决定重试 vs 死信;原 chat 发失败但 p2p 成功视为成功(回退链兜住了)。
#[allow(clippy::too_many_arguments)] // 各参数职责独立(投递目标解析所需),合并无意义
pub(super) fn deliver_notify(
    fc: &FeishuClient,
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    chat_name: Option<&str>,
    creator: Option<&str>,
    event_operator: Option<&str>,
    reason: summon_core::exit::ExitReason,
    text: &str,
    frozen_card: Option<&str>,
    session_created_at: Option<&str>,
) -> anyhow::Result<()> {
    if text.trim().is_empty() {
        return Ok(()); // 文案被配空 → 不发(诚实放弃,不算失败)。
    }
    // `frozen_card` 三态,决定发卡片还是回退文本:
    //   None        = 内联路径(面板/群解散/探测),不经重启,就地渲染;
    //   Some(非空)  = outbox 路径,用入队那刻定格的 wire(模板可能已变,不重渲染);
    //   Some(空)    = daemon 升级前入队的旧事件,回退纯文本——既不丢消息,也不发一张空白卡。
    let card = match frozen_card {
        None => Some(summon_core::feishu::card_session_exited(
            reason,
            &agent.name,
            &exit_chat_label(chat_name, chat_type, chat_id),
            &crate::runtime::exit_receipt::now_local_hm(),
            text,
            session_created_at,
        )),
        Some(frozen) if !frozen.trim().is_empty() => Some(frozen.to_string()),
        Some(_) => None,
    };
    // 群仍在(手动/面板/停机打断)先试发原 chat;群已散(事件)直接走 p2p 群主。
    if !reason.group_gone() {
        let sent = match card.as_deref() {
            Some(wire) => crate::frontend::outbound::send_frozen_card_archived(
                fc,
                &agent.state_dir(),
                chat_type,
                chat_id,
                "chat_id",
                "session_exited_card",
                wire,
            ),
            None => send_markdown_archived(fc, &agent.state_dir(), chat_type, chat_id, text),
        };
        match sent {
            Ok(()) => return Ok(()),
            Err(e) => log(&summon_i18n::format!("r8eb2051ca8", agent.name)),
        }
    }
    // 回退 / 群已散:p2p 群主。目标 = creator → 事件 operator → 配置 owner。
    let owner = agent_owner(agent);
    match summon_core::exit::resolve_notify_target(creator, event_operator, owner.as_deref()) {
        Some(summon_core::exit::NotifyTarget::OpenId(oid)) => match card.as_deref() {
            Some(wire) => crate::frontend::outbound::send_frozen_card_archived(
                fc,
                &agent.state_dir(),
                "p2p",
                &oid,
                "open_id",
                "session_exited_card",
                wire,
            ),
            None => fc.send_markdown_to_open_id(&oid, text),
        }
        .with_context(|| summon_i18n::format!("rb88c14a86d")),
        // Chat 目标在本函数里不会出现(resolve 只产 OpenId);None = 无人可通知 → 诚实放弃(不重试)。
        _ => {
            log(&summon_i18n::format!("rc5c117987c", agent.name));
            Ok(())
        }
    }
}

// 退出回执的组装/入队全在 `runtime::exit_receipt`(CLI 也要用,daemon 内部可见性关不住)。
// 此处再导出,daemon 各子模块直接构造 `ExitReceipt` 调 `exit_receipt::enqueue`,不经中转。
pub(super) use crate::runtime::exit_receipt::{self, exit_chat_label, rand_hex8, ExitReceipt};

/// 当前 unix 毫秒(时钟回拨兜底 0);outbox 入队/到期判定/stream_state 时间戳用。
pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 外部飞书事件(群解散 / bot 被移除)驱动的拆除收尾。**显式事件驱动**的合法 kill 路径
///(等同用户点退出),非 idle 线程擅杀。在线 → 投 Control::Reset 让 chat 线程经 do_teardown +
/// ChatExited 收口(复用退出回执);不在线(daemon 重启后内存空)→ 从 `.meta` 重建 Session
/// 直接 do_teardown + 通知群主。群已没了 → 通知方向恒为 p2p(reason.group_gone())。
#[allow(clippy::too_many_arguments)] // 各参数职责独立,合并无意义
pub(super) fn handle_chat_gone(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    messaging: &Messaging,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    peers: &crate::runtime::peers_store::PeersStore,
    inflight: &crate::runtime::inflight::InFlight,
    poll: u64,
    debounce: u64,
    chat_id: &str,
    reason: summon_core::exit::ExitReason,
    operator_open_id: &str,
) {
    let _ = fc; // 在线分支让 chat 线程自己发回执;离线分支入队 outbox,本函数不直接联网。
    log(&summon_i18n::format!("r2861fa567b", agent.name));
    if let Some(h) = chats.get(chat_id) {
        // R2(同 handle_teardown_chat):先 dispatcher 侧立刻 SIGINT 在跑无头轮,再投 GoneTeardown——否则
        // 群解散/bot 被移除时「先打断」会被 requeue 到本轮 EOF 之后才执行,成空操作。
        interrupt_inflight_turn(inflight, &agent.name, &h.chat_type, chat_id);
        // 在线:让 chat 线程(持真 tmux session 名)自己 do_teardown + 当场发 p2p 回执,再经
        // ChatExited(notify=false)摘 handle。线程已退(send 失败)→ 落到下面盘上重建分支。
        let (ack_tx, ack_rx) = channel();
        if h.tx
            .send(ThreadMsg::GoneTeardown {
                reason,
                event_operator: operator_open_id.to_string(),
                peers: peers.clone(),
                ack: ack_tx,
            })
            .is_ok()
        {
            // 发进 channel 不等于已执行：若 Clean/Kill 先一步让线程退出，
            // Gone 会随 receiver 销毁。本地清理 ACK 是唯一提交点，断开/超时则走
            // 下方离线幂等路径。稀有外部事件最多阻塞 5s，换取不泄漏会话。
            if ack_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                chats.remove(chat_id);
                return;
            }
            log(&summon_i18n::format!("r14e83ee914", agent.name));
        }
        chats.remove(chat_id);
    }
    // 不在线:从 .meta 重建 Session 收尾(F4-b)。读不到 .meta → 仅通知群主(尽力不丢)。
    // 离线回执**入队 outbox**(不内联同步发飞书:无 chat 线程上下文、群可能已散,联网慢会卡拆除),
    // 由下次启动的 drain 线程投递。
    match chat_session_on_disk(agent, chat_id) {
        Some((session, chat_type, creator)) => {
            let chat_name = read_chat_meta(agent, &session).and_then(|meta| meta.chat_name);
            let tag = chat_tag(&chat_type, chat_id);
            // 先抓 created_at:teardown_gone_sessions 会清 .session,必须在清之前取。
            let session_created_at = crate::session::resume_store::read_record(agent, &session)
                .and_then(|r| r.created_at);
            // 「不在线」说的是**没有 chat 线程**,不等于没有常驻 RPC 会话进程:池的键是
            // (agent, tag),线程退了池里那条照样活着(且仍挂在 InFlight 上)。群都散了还留着它,
            // 就是给一个不存在的会话养孤儿进程。chat_type 到这一步才从盘上读出来,所以只能在这里打。
            interrupt_inflight_turn(inflight, &agent.name, &chat_type, chat_id);
            let torn_down = teardown_gone_sessions(agent, chat_id, &session, &tag, poll, debounce);
            exit_receipt::enqueue(
                outbox,
                messaging,
                &ExitReceipt {
                    agent,
                    delivery_scope: &feishu_storage_scope(agent).key,
                    chat_type: &chat_type,
                    chat_id,
                    chat_name: chat_name.as_deref(),
                    creator: creator.as_deref(),
                    event_operator: Some(operator_open_id),
                    reason,
                    session_created_at: session_created_at.as_deref(),
                },
            )
            .log_if_failed(&agent.name);
            let removed = purge_agent_chat_state(agent, chat_id, &session, &tag);
            if let Err(e) = peers.remove(&tag) {
                log(&summon_i18n::format!("r134473cb7e", agent.name));
            }
            log(&summon_i18n::format!("re68fd743c5", agent.name,));
        }
        None => {
            log(&summon_i18n::format!("r1abf78f5f0", agent.name));
            exit_receipt::enqueue(
                outbox,
                messaging,
                &ExitReceipt {
                    agent,
                    delivery_scope: &feishu_storage_scope(agent).key,
                    chat_type: "group",
                    chat_id,
                    chat_name: None,
                    creator: None,
                    event_operator: Some(operator_open_id),
                    reason,
                    // 无 .meta → 无 .session,created_at 未知。
                    session_created_at: None,
                },
            )
            .log_if_failed(&agent.name);
            // meta 已缺时仍按稳定 tag 回收 threads/sessions/inbound/peers 残留 + 常驻 RPC 会话。
            // 群解散事件只可能来自群,故 chat_type 恒 "group"(与下面的 tag 同一个假设)。
            interrupt_inflight_turn(inflight, &agent.name, "group", chat_id);
            let tag = chat_tag("group", chat_id);
            let torn_down = teardown_gone_sessions(agent, chat_id, "", &tag, poll, debounce);
            let removed = purge_agent_chat_state(agent, chat_id, "", &tag);
            if let Err(e) = peers.remove(&tag) {
                log(&summon_i18n::format!("r134473cb7e", agent.name));
            }
            log(&summon_i18n::format!("r28c8bdcc16", agent.name));
        }
    }
}

/// 打断本 chat 在跑的无头一轮(`agents kill` 在拆除前调):按 `(agent, tag)` 查 InFlight 注册表,
/// 查到则整组 SIGINT(子进程随 stdout EOF 退,turn 收尾自行 remove)。无在跑轮则无害(直接 teardown)。
///
/// **RPC 会话另有一条登记**(键 `rpc:<tag>`,见 `capture::rpc_pool`):它跨轮存活,轮键那条摘掉之后
/// 进程还在。chat 都要拆了,留着它就是给一个不存在的会话养一个孤儿进程 —— 一并收掉。
/// 两个键分开是刻意的(生命周期不同),这里也就必须显式各打一次,不能指望其中一条兼管另一条。
///
/// 返回有没有真打到活的东西(在跑的轮或常驻会话进程)。
pub(super) fn interrupt_inflight_turn(
    inflight: &crate::runtime::inflight::InFlight,
    agent_name: &str,
    chat_type: &str,
    chat_id: &str,
) -> bool {
    let tag = chat_tag(chat_type, chat_id);
    let key = crate::runtime::inflight::TurnKey::new(agent_name, tag.clone());
    let turn = inflight.interrupt_one_bounded(&key, HEADLESS_INTERRUPT_GRACE);
    if turn {
        log(&summon_i18n::format!("rbe1151f1ed"));
    }
    // 常驻会话交给池的 `evict`:它自带有界收敛(SIGINT → 宽限 → SIGKILL),且**收敛确认之后
    // 才注销 `InFlight`**。刻意不再用 `interrupt_one_bounded` 打它 —— 那条路的第二枪要靠 sleep
    // 后复查注册表还在不在,而 evict 会把注册摘掉,于是延迟 SIGKILL 永远不开枪(屏蔽 SIGINT 的
    // 会话进程就此长生),中间还有一段「池里没有、表里也没有」的够不着窗口。
    if crate::capture::rpc_pool::shared(inflight).evict(agent_name, &tag) {
        log(&summon_i18n::format!("r93c4235762"));
        return true;
    }
    // 池里没有这条会话(只可能落在「已 spawn 登记、尚未入池」的窄窗口):退回按注册表打断。
    // 此时没有 evict 的同步摘除来抢跑,延迟 SIGKILL 能正常兑现。
    let session_key = crate::capture::rpc_pool::inflight_key(agent_name, &tag);
    let unpooled = inflight.interrupt_one_bounded(&session_key, HEADLESS_INTERRUPT_GRACE);
    if unpooled {
        log(&summon_i18n::format!("r54f954627c"));
    }
    turn || unpooled
}

/// 管家卡与 Menu Bar 的退出 / 归档:沿用归档/退出的保存检查与回执。离线会话从持久化身份执行。
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_fleet_chat_control(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    messaging: &Messaging,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    inflight: &crate::runtime::inflight::InFlight,
    poll: u64,
    debounce: u64,
    chat_id: &str,
    archive: bool,
) {
    super::waiting::clear_waiting(
        agent,
        &super::cards::persisted_chat_type(chats, agent, chat_id),
        chat_id,
        summon_i18n::text!("r41b2a7b97f"),
    );
    if let Some(handle) = chats.get(chat_id) {
        if handle
            .tx
            .send(ThreadMsg::Control {
                ctrl: if archive {
                    Control::Archive
                } else {
                    Control::Clean
                },
                card_message_id: None,
                models: Vec::new(),
                origin: CardOrigin::Panel,
                notify_exit: true,
                idle_retired: false,
                source_task: None,
                source_started: false,
            })
            .is_ok()
        {
            return;
        }
        chats.remove(chat_id);
    }
    let Some((session, chat_type, creator)) = chat_session_on_disk(agent, chat_id) else {
        log(&summon_i18n::format!("r40a53a854a", agent.name));
        return;
    };
    let tag = chat_tag(&chat_type, chat_id);
    if let Err(error) = crate::session::resume_store::remember_chat_checked(agent, &session, &tag) {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            &summon_i18n::format!("re194d8f9b6"),
        );
        return;
    }
    let session_created_at = crate::session::resume_store::read_record(agent, &session)
        .and_then(|record| record.created_at);
    let sess = Session::with_name(agent, session.clone(), poll, debounce);
    interrupt_inflight_turn(inflight, &agent.name, &chat_type, chat_id);
    do_teardown(agent, &sess, &tag, archive);
    exit_receipt::enqueue(
        outbox,
        messaging,
        &ExitReceipt {
            delivery_scope: &feishu_storage_scope(agent).key,
            agent,
            chat_type: &chat_type,
            chat_id,
            chat_name: read_chat_meta(agent, &session)
                .and_then(|meta| meta.chat_name)
                .as_deref(),
            creator: creator.as_deref(),
            event_operator: None,
            reason: if archive {
                summon_core::exit::ExitReason::PanelReset
            } else {
                summon_core::exit::ExitReason::PanelClean
            },
            session_created_at: session_created_at.as_deref(),
        },
    );
}

/// `summon agents kill` 经 control.sock→Core 路由来的无头会话拆除。`reset` 决定 Clean/Reset。
/// 在线:投 `ThreadMsg::KillTeardown`(线程先打断在跑子进程、再 do_teardown、再发回执);
/// 不在线:从盘上 `.meta` 重建 Session 直接归档 + 发退出回执。reason=ManualKill(等同 `agents down`)。
/// 两条路都只在 [`kill_changed_anything`] 时发回执。
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_teardown_chat(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    messaging: &Messaging,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    inflight: &crate::runtime::inflight::InFlight,
    poll: u64,
    debounce: u64,
    chat_id: &str,
    reset: bool,
) {
    let _ = fc; // 在线分支由 chat 线程发回执;离线分支入队 outbox,本函数不直接联网。
    log(&summon_i18n::format!("r0e57d7276b", agent.name));
    if let Some(h) = chats.get(chat_id) {
        // R2:**先在 dispatcher 侧立刻 SIGINT 在跑无头轮**,再投 KillTeardown。否则 KillTeardown 会被
        // chat 线程的 poll_force_interrupt 当控制消息 requeue 到本轮自然 EOF(可达数分钟)之后才处理,
        // 届时 interrupt_one 已查无在跑轮、「先打断」成空操作。dispatcher 持 inflight 句柄,直接整组打断
        // 让子进程 EOF、线程尽快轮转到 KillTeardown 臂收尾(外部 SIGINT 不触发 `!` 重跑,turn 直接结束)。
        interrupt_inflight_turn(inflight, &agent.name, &h.chat_type, chat_id);
        // chat 线程持真 tmux/会话名,自己 do_teardown,再经 ChatExited 摘 handle。
        // 线程已退(send 失败)→ 落到下面盘上重建分支。
        if h.tx.send(ThreadMsg::KillTeardown { reset }).is_ok() {
            return;
        }
        chats.remove(chat_id);
    }
    // 不在线:从 .meta 重建 Session 收尾(无活跃 chat 线程,无在跑子进程可打断)。离线回执**入队
    // outbox**(替代内联同步发飞书,不阻塞拆除),由下次启动 drain 线程投递。
    match chat_session_on_disk(agent, chat_id) {
        Some((session, chat_type, creator)) => {
            // 「无活跃 chat 线程」不蕴含「无常驻 RPC 会话」:池的键是 (agent, tag),线程退了
            // 池里那条照样活着。kill 却留着它 = 下条消息把「已被 kill 的会话」原样接回去。
            let stopped_live = interrupt_inflight_turn(inflight, &agent.name, &chat_type, chat_id);
            let sess = Session::with_name(agent, session, poll, debounce);
            // 先抓 created_at:do_teardown(reset=true) 会清 .session,必须在清之前取。
            let session_created_at =
                crate::session::resume_store::read_record(agent, &sess.tmux.session)
                    .and_then(|r| r.created_at);
            let effect = do_teardown(agent, &sess, &chat_tag(&chat_type, chat_id), reset);
            if !kill_changed_anything(stopped_live, effect) {
                log(&summon_i18n::format!("r643a32b1b4", agent.name));
                return;
            }
            exit_receipt::enqueue(
                outbox,
                messaging,
                &ExitReceipt {
                    agent,
                    delivery_scope: &feishu_storage_scope(agent).key,
                    chat_type: &chat_type,
                    chat_id,
                    chat_name: None,
                    creator: creator.as_deref(),
                    event_operator: None,
                    reason: summon_core::exit::manual_kill_reason(reset),
                    session_created_at: session_created_at.as_deref(),
                },
            )
            .log_if_failed(&agent.name);
        }
        None => log(&summon_i18n::format!("r3b18a06aff", agent.name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::resume_store;

    /// 测试用的唯一 chat_id:tag 由 chat_id 哈希而来,唯一 chat_id ⇒ 唯一 tmux 名,
    /// `has_session()` 必然为 false ⇒ 测试永远不会碰到任何真实 tmux 会话(红线)。
    fn unique_chat_id() -> String {
        format!("oc_teardown_test_{}", uuid::Uuid::new_v4().simple())
    }

    /// 只有用户显式重置才替他作废恢复卡;改配置的快重置(不报退出)与定时尾轮自动重置都不算。
    #[test]
    fn only_user_explicit_reset_discards_recovery() {
        assert!(
            reset_discards_recovery(true, false),
            "面板/`/reset`/`/session reset`"
        );
        assert!(
            !reset_discards_recovery(false, false),
            "改 model/工作目录的快重置"
        );
        assert!(!reset_discards_recovery(true, true), "定时尾轮自动重置");
        assert!(!reset_discards_recovery(false, true));
    }

    #[test]
    fn kill_receipt_only_when_something_actually_changed() {
        let none = TeardownEffect::default();
        assert!(!kill_changed_anything(false, none));
        assert!(
            kill_changed_anything(true, none),
            "打断了在跑的轮或常驻会话"
        );
        assert!(kill_changed_anything(
            false,
            TeardownEffect {
                had_tmux: true,
                ..none
            }
        ));
        assert!(kill_changed_anything(
            false,
            TeardownEffect {
                cleared_record: true,
                ..none
            }
        ));
    }

    #[test]
    fn teardown_reports_whether_a_record_was_cleared() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chat_id = unique_chat_id();
        let tag = chat_tag("group", &chat_id);
        let session = agent.tmux_name_for_chat("group", &chat_id, false);
        resume_store::write(&agent, &session, "claude", "sid-1", None, None);
        let sess = Session::with_name(&agent, session, 50, 50);

        assert_eq!(
            do_teardown(&agent, &sess, &tag, true),
            TeardownEffect {
                had_tmux: false,
                cleared_record: true
            }
        );
        assert_eq!(
            do_teardown(&agent, &sess, &tag, true),
            TeardownEffect::default(),
            "第二次已无记录可清"
        );
    }

    /// 会话拆掉后台任务随之结束,让轮留下的持有者一并清掉:否则 owner 手工 attach 时会被
    /// 已经不在跑的后台任务的发起人限权。
    #[test]
    fn teardown_clears_background_holders_of_the_chat() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chat_id = unique_chat_id();
        let tag = chat_tag("group", &chat_id);
        let session = agent.tmux_name_for_chat("group", &chat_id, false);
        let sess_name = session.clone();
        let holders =
            crate::runtime::background_holder::BackgroundHolderStore::for_agent(&agent.state_dir());
        holders
            .add(
                &tag,
                crate::runtime::background_holder::BackgroundHolder {
                    turn_id: "turn-held".into(),
                    message_id: "om-held".into(),
                    chat_id: chat_id.clone(),
                    chat_type: "group".into(),
                    origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
                    requester_open_id: "ou_requester".into(),
                    requester_union_id: String::new(),
                    requester_is_peer: false,
                    tmux_session: sess_name.clone(),
                    session_id: "sid-1".into(),
                    transcript: std::path::PathBuf::from("/nonexistent"),
                    tasks: Vec::new(),
                    created_at_ms: 0,
                    expires_at_ms: i64::MAX,
                },
            )
            .unwrap();
        let sess = Session::with_name(&agent, session, 50, 50);
        do_teardown(&agent, &sess, &tag, false);
        assert!(!holders.has_any(&tag));
    }

    /// reset=true 的语义是「真重置」:清掉 resume 记录,下次冷启不 resume。
    /// 这条与下一条互为反向,把 `if reset` 的方向钉死——此前 548 行 0 测试,
    /// 把条件反转过来全量门禁照样绿。
    #[test]
    fn teardown_with_reset_clears_resume_record() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chat_id = unique_chat_id();
        let tag = chat_tag("group", &chat_id);
        let session = agent.tmux_name_for_chat("group", &chat_id, false);
        resume_store::write(&agent, &session, "claude", "sid-1", None, None);
        assert!(resume_store::read_id(&agent, &session, "claude").is_some());

        let sess = Session::with_name(&agent, session.clone(), 50, 50);
        do_teardown(&agent, &sess, &tag, true);

        assert!(
            resume_store::read_id(&agent, &session, "claude").is_none(),
            "reset 必须清 resume 记录,否则下次冷启还会续上旧上下文"
        );
    }

    /// reset=false = Clean(留记录续聊):记录必须原封不动,否则用户点「清理」会丢上下文。
    #[test]
    fn teardown_without_reset_keeps_resume_record() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chat_id = unique_chat_id();
        let tag = chat_tag("group", &chat_id);
        let session = agent.tmux_name_for_chat("group", &chat_id, false);
        resume_store::write(&agent, &session, "claude", "sid-1", None, None);

        let sess = Session::with_name(&agent, session.clone(), 50, 50);
        do_teardown(&agent, &sess, &tag, false);

        assert_eq!(
            resume_store::read_id(&agent, &session, "claude").as_deref(),
            Some("sid-1"),
            "Clean 语义是留记录续聊,不该动 resume 记录"
        );
    }

    /// 历史版本会给同一 chat 留下裸名与 tag 名两份会话;群解散必须两份都拆,
    /// 否则另一份变成不可寻址孤儿。`teardown_gone_sessions` 返回值 = 它实际遍历
    /// 并逐个 `do_teardown` 的会话数(reset=false 在盘上无可观测副作用,故断言规划集合 + 计数)。
    #[test]
    fn teardown_gone_sessions_visits_bare_and_tag_named_sessions() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chat_id = unique_chat_id();
        let tag = chat_tag("group", &chat_id);
        let bare = agent.tmux_name();
        let tagged = agent.tmux_name_for_chat("group", &chat_id, false);
        for session in [&bare, &tagged] {
            write_chat_meta_checked(
                &agent,
                session,
                &chat_id,
                "group",
                Some("测试群"),
                "ou_sender",
                Some("ou_creator"),
            )
            .expect("测试 .meta 应可写");
        }

        let planned = gone_session_names(&agent, &chat_id, &bare);
        assert!(planned.contains(&bare), "裸名会话必须纳入拆除计划");
        assert!(planned.contains(&tagged), "tag 名会话必须纳入拆除计划");

        let torn = teardown_gone_sessions(&agent, &chat_id, &bare, &tag, 50, 50);
        assert_eq!(torn, 2, "两份会话都该被 do_teardown 走一遍");
    }

    /// 群解散/bot 被移除但盘上已无 `.meta` 的分支(此前 0 测试)。此时没有任何会话事实可读,
    /// 回执只能按「群」假定发:chat_type 恒 "group"、无 chat_name/creator,通知目标全靠事件
    /// operator。钉死这四项——它们各自是「怎么把消息送到人手里」的唯一线索。
    #[test]
    fn handle_chat_gone_without_meta_still_enqueues_group_notify() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chat_id = unique_chat_id();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true); // dry-run
        let messaging = summon_core::config::builtin_messaging();
        let outbox = crate::runtime::outbox_store::OutboxStore::new(agent.dir.join("outbox"));
        let peers = crate::runtime::peers_store::PeersStore::new(agent.dir.join("peers"));
        let inflight = crate::runtime::inflight::InFlight::new();
        let mut chats: HashMap<String, ChatHandle> = HashMap::new();

        handle_chat_gone(
            &mut chats,
            &agent,
            &fc,
            &messaging,
            &outbox,
            &peers,
            &inflight,
            50,
            50,
            &chat_id,
            summon_core::exit::ExitReason::GroupDisbanded,
            "ou_op",
        );

        let due = outbox.list_due(now_unix_ms() + 1);
        assert_eq!(due.len(), 1, "无 meta 也必须尽力入队一条回执");
        assert_eq!(due[0].delivery_scope, feishu_storage_scope(&agent).key);
        match &due[0].kind {
            summon_core::outbox::OutboxKind::NotifyChat {
                chat_type,
                creator,
                event_operator,
                reason,
                rendered_card,
                ..
            } => {
                assert_eq!(chat_type, "group", "无 meta 时按群假定");
                assert!(creator.is_none(), "无 meta 就没有群主近似,不许凭空造");
                assert_eq!(event_operator.as_deref(), Some("ou_op"));
                assert_eq!(*reason, summon_core::exit::ExitReason::GroupDisbanded);
                assert!(
                    rendered_card.contains(&chat_tag("group", &chat_id)),
                    "无群名时卡片「会话」列回退 chat tag"
                );
            }
            other => panic!("应入队 NotifyChat,得到 {other:?}"),
        }
    }
}
