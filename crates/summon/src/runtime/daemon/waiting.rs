//! waiting 标记的 daemon 侧时机:人触发的轮结束时置位,owner 的明确动作清除。
//! 判据与理由见 `docs/spec/2026-10-15-menubar-waiting-design.md`;盘上存取见 `runtime::waiting_store`。
//! 判据都收在这里,调用点无条件调用 —— 判据由本文件的测试钉住,调用点由结构测试钉住。

use super::cards::Control;
use super::chat_thread::TurnResult;
use super::*;

/// 这一轮结束时 waiting 锚在哪条消息上;None = 不置位。
///
/// 人发来的那一轮(非 bot、非定时)有了终态才置位 —— 成功、空回复、失败都算,失败同样要人处理。
/// 锚点是这一轮新登记的终态回复:斜杠命令回执这类不登记终态回复的轮因此不置位,`/skill` 之类
/// 跑完整一轮的命令照常置位。失败且没有新回复时锚在触发这一轮的那条消息上,失败回执紧跟其后;
/// 失败回执本身不登记为终态回复,否则会顶掉卡片模式下过程卡的子代理尾巴。
pub(super) fn waiting_anchor(
    kind: ChatTaskKind,
    sender_is_bot: bool,
    result: TurnResult,
    registered: Option<String>,
    before_turn: Option<&str>,
    trigger: Option<&str>,
) -> Option<String> {
    if kind.is_scheduled() || sender_is_bot {
        return None;
    }
    let reply = registered.filter(|id| Some(id.as_str()) != before_turn);
    match result {
        TurnResult::Done | TurnResult::Empty => reply,
        TurnResult::Failed => reply.or_else(|| trigger.map(str::to_string)),
        TurnResult::SilentReset | TurnResult::Interrupted | TurnResult::RecoveryDeferred => None,
    }
}

/// 准入时:人发来的消息(私聊任意、群里 @ 了本 bot,斜杠命令也算)= owner 的新动作。
fn inbound_clears(sender_is_bot: bool) -> bool {
    !sender_is_bot
}

/// 开跑时:排在上一轮后面的人的消息,开跑时再清一次 —— 否则上一轮结束时置的标记会盖住它。
fn turn_start_clears(kind: ChatTaskKind, sender_is_bot: bool) -> bool {
    !kind.is_scheduled() && !sender_is_bot
}

/// 卡片按钮:退出 / 归档是 owner 处理过的表态;压缩、恢复不是,`Reset` 只来自配置变更与定时器。
/// 保活另有一道判据,见 [`keepalive_clears`]。
fn card_clears(ctrl: Control) -> bool {
    matches!(ctrl, Control::Clean | Control::Archive)
}

/// 空闲卡点击的来源与操作人，用于判定是否清除 waiting。
pub(super) struct IdleCardClick<'a> {
    pub(super) action: summon_core::feishu::CardAction,
    pub(super) from_idle_card: bool,
    pub(super) operator_open_id: &'a str,
    pub(super) is_group: bool,
    pub(super) chat_id: &'a str,
}

/// 空闲提醒卡上的「保活」也是 owner 处理过的表态:看过了,会话先留着(owner 2026-10-16 定)。
/// 保活按钮免鉴权、群里谁都能点,而清 waiting 是替 owner 表态,所以只认过得了本群 operate-gate
/// 的点击人,与同一张卡上的退出 / 归档同档。未登记的卡片动作也落到保活分派上,只有空闲卡上那个
/// 保活按钮算数。union_id 要联网解析,只在前两条成立时才取。
fn keepalive_clears(
    agent: &Agent,
    click: &IdleCardClick,
    operator_union_id: impl FnOnce() -> String,
) -> bool {
    click.action == summon_core::feishu::CardAction::KeepAlive
        && click.from_idle_card
        && agent.can_operate(
            click.operator_open_id,
            &operator_union_id(),
            click.is_group,
            click.action.operate_scope(click.chat_id),
        )
}

fn idle_close_clears(
    agent: &Agent,
    click: &IdleCardClick,
    operator_union_id: impl FnOnce() -> String,
) -> bool {
    click.action == summon_core::feishu::CardAction::SessionClose
        && click.from_idle_card
        && agent.can_operate(
            click.operator_open_id,
            &operator_union_id(),
            click.is_group,
            click.action.operate_scope(click.chat_id),
        )
}

pub(super) fn clear_on_idle_close(
    agent: &Agent,
    fc: &FeishuClient,
    chat_type: &str,
    click: IdleCardClick,
) {
    if idle_close_clears(agent, &click, || {
        fc.resolve_union_id(click.operator_open_id)
    }) {
        clear_waiting(
            agent,
            chat_type,
            click.chat_id,
            summon_i18n::text!("rd0e72fdbce"),
        );
    }
}

/// 置位:取锚点消息的会话内序号、拼跳转链接、落盘。取不到序号只记日志,链接退回只打开会话。
pub(super) fn mark_waiting(
    agent: &Agent,
    fc: &FeishuClient,
    chat_type: &str,
    chat_id: &str,
    anchor_message_id: &str,
) {
    let position = match fc.fetch_message_position(anchor_message_id) {
        Ok(position) => position,
        Err(error) => {
            log(&summon_i18n::format!("r8b8bd6bcf3", agent.name));
            None
        }
    };
    let mark = summon_core::waiting::WaitingMark {
        chat_id: chat_id.to_string(),
        reply_message_id: anchor_message_id.to_string(),
        link: summon_core::waiting::feishu_chat_link(
            fc.applink_host(),
            chat_id,
            position.as_deref(),
        ),
        since_unix: SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    if let Err(error) =
        crate::runtime::waiting_store::set(&agent.state_dir(), &chat_tag(chat_type, chat_id), &mark)
    {
        log(&summon_i18n::format!("rfd1df50c8f", agent.name));
    }
}

pub(super) fn clear_on_inbound(agent: &Agent, chat_type: &str, chat_id: &str, sender_is_bot: bool) {
    if inbound_clears(sender_is_bot) {
        clear_waiting(agent, chat_type, chat_id, summon_i18n::text!("rf6b2476c5a"));
    }
}

pub(super) fn clear_on_turn_start(
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    kind: ChatTaskKind,
    sender_is_bot: bool,
) {
    if turn_start_clears(kind, sender_is_bot) {
        clear_waiting(agent, chat_type, chat_id, summon_i18n::text!("rc62088ac5b"));
    }
}

pub(super) fn clear_on_card(agent: &Agent, chat_type: &str, chat_id: &str, ctrl: Control) {
    if card_clears(ctrl) {
        clear_waiting(agent, chat_type, chat_id, summon_i18n::text!("rf1cf9e3aa0"));
    }
}

pub(super) fn clear_on_keepalive(
    agent: &Agent,
    fc: &FeishuClient,
    chat_type: &str,
    click: IdleCardClick,
) {
    if keepalive_clears(agent, &click, || {
        fc.resolve_union_id(click.operator_open_id)
    }) {
        clear_waiting(
            agent,
            chat_type,
            click.chat_id,
            summon_i18n::text!("rcccce989a4"),
        );
    }
}

/// 清除:owner 的明确动作。`why` 只进日志。
pub(super) fn clear_waiting(agent: &Agent, chat_type: &str, chat_id: &str, why: &str) {
    match crate::runtime::waiting_store::clear(&agent.state_dir(), &chat_tag(chat_type, chat_id)) {
        Ok(true) => log(&summon_i18n::format!("rc0a834f9ed", agent.name)),
        Ok(false) => {}
        Err(error) => log(&summon_i18n::format!("r98835d94e2", agent.name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_is_the_new_reply_of_a_finished_human_turn() {
        use ChatTaskKind::*;
        use TurnResult::*;
        let reply = || Some("om_reply".to_string());
        for kind in [Conversation, ProviderPrompt, SlashCommand] {
            for result in [Done, Empty, Failed] {
                assert_eq!(
                    waiting_anchor(
                        kind,
                        false,
                        result,
                        reply(),
                        Some("om_before"),
                        Some("om_ask")
                    ),
                    Some("om_reply".to_string()),
                    "{kind:?} {result:?}"
                );
            }
        }
        for result in [Interrupted, SilentReset, RecoveryDeferred] {
            assert_eq!(
                waiting_anchor(Conversation, false, result, reply(), None, Some("om_ask")),
                None,
                "{result:?}"
            );
        }
        assert_eq!(
            waiting_anchor(Conversation, true, Done, reply(), None, Some("om_ask")),
            None,
            "bot 发起的是 agent 间协作"
        );
        for kind in [ScheduledPrompt, ScheduledResult] {
            assert_eq!(waiting_anchor(kind, false, Done, reply(), None, None), None);
        }
    }

    #[test]
    fn only_a_reply_registered_in_this_turn_counts() {
        use ChatTaskKind::Conversation;
        use TurnResult::Done;
        let same = waiting_anchor(
            Conversation,
            false,
            Done,
            Some("om_1".into()),
            Some("om_1"),
            Some("om_ask"),
        );
        assert_eq!(same, None, "槽里还是上一轮那张");
        assert_eq!(
            waiting_anchor(
                Conversation,
                false,
                Done,
                None,
                Some("om_1"),
                Some("om_ask")
            ),
            None,
            "斜杠命令回执这类没登记终态回复的轮不置位"
        );
    }

    #[test]
    fn a_failed_turn_without_a_reply_anchors_at_its_trigger() {
        use ChatTaskKind::Conversation;
        use TurnResult::Failed;
        assert_eq!(
            waiting_anchor(Conversation, false, Failed, None, None, Some("om_ask")),
            Some("om_ask".to_string()),
            "失败回执紧跟在触发消息后面"
        );
        assert_eq!(
            waiting_anchor(Conversation, false, Failed, None, None, None),
            None,
            "没有可锚的飞书消息就不置位"
        );
    }

    #[test]
    fn only_people_and_owner_buttons_clear() {
        use ChatTaskKind::*;
        assert!(inbound_clears(false));
        assert!(!inbound_clears(true), "bot 的消息不是 owner 的动作");
        assert!(turn_start_clears(Conversation, false));
        assert!(turn_start_clears(SlashCommand, false));
        assert!(!turn_start_clears(ScheduledPrompt, false));
        assert!(!turn_start_clears(ScheduledResult, false));
        assert!(!turn_start_clears(Conversation, true));
        assert!(card_clears(Control::Clean));
        assert!(card_clears(Control::Archive));
        for ctrl in [Control::Compact, Control::Resume, Control::Reset] {
            assert!(!card_clears(ctrl), "{ctrl:?}");
        }
    }

    #[test]
    fn keepalive_clears_only_when_an_operator_clicks_it_on_the_idle_card() {
        use summon_core::feishu::CardAction;
        let agent: Agent = serde_yaml::from_str(
            "name: a\nfeishu:\n  operators: [ou_owner]\n  chat_operators:\n    oc_room: [ou_colleague]\n",
        )
        .unwrap();
        let click = |action, from_idle_card, operator_open_id| IdleCardClick {
            action,
            from_idle_card,
            operator_open_id,
            is_group: true,
            chat_id: "oc_room",
        };
        let clears = |click: IdleCardClick| keepalive_clears(&agent, &click, String::new);
        assert!(clears(click(CardAction::KeepAlive, true, "ou_owner")));
        assert!(
            clears(click(CardAction::KeepAlive, true, "ou_colleague")),
            "本群 operator 与退出 / 归档同档"
        );
        assert!(
            !clears(click(CardAction::KeepAlive, true, "ou_guest")),
            "群里过不了 operate-gate 的人"
        );
        assert!(
            !clears(click(CardAction::Unknown, true, "ou_owner")),
            "落到保活分派的未登记动作"
        );
        assert!(
            !clears(click(CardAction::KeepAlive, false, "ou_owner")),
            "不是空闲卡上的点击"
        );
        let other_room = IdleCardClick {
            chat_id: "oc_elsewhere",
            ..click(CardAction::KeepAlive, true, "ou_colleague")
        };
        assert!(!clears(other_room), "本群名单只在自己那个群算数");
    }

    #[test]
    fn idle_close_requires_an_operator_in_the_same_chat() {
        use summon_core::feishu::CardAction;
        let agent: Agent = serde_yaml::from_str(
            "name: a\nfeishu:\n  operators: [ou_owner]\n  chat_operators:\n    oc_room: [ou_colleague]\n",
        ).unwrap();
        for (who, chat, idle, action, expected) in [
            ("ou_owner", "oc_room", true, CardAction::SessionClose, true),
            (
                "ou_colleague",
                "oc_room",
                true,
                CardAction::SessionClose,
                true,
            ),
            ("ou_guest", "oc_room", true, CardAction::SessionClose, false),
            (
                "ou_colleague",
                "oc_elsewhere",
                true,
                CardAction::SessionClose,
                false,
            ),
            (
                "ou_owner",
                "oc_room",
                false,
                CardAction::SessionClose,
                false,
            ),
            ("ou_owner", "oc_room", true, CardAction::Unknown, false),
        ] {
            let click = IdleCardClick {
                action,
                from_idle_card: idle,
                operator_open_id: who,
                is_group: true,
                chat_id: chat,
            };
            assert_eq!(idle_close_clears(&agent, &click, String::new), expected);
        }
        let empty: Agent = serde_yaml::from_str("name: a\n").unwrap();
        let click = IdleCardClick {
            action: CardAction::SessionClose,
            from_idle_card: true,
            operator_open_id: "ou_guest",
            is_group: true,
            chat_id: "oc_room",
        };
        assert!(!idle_close_clears(&empty, &click, String::new));
    }

    fn production(src: &'static str) -> &'static str {
        src.split_once("\n#[cfg(test)]")
            .map_or(src, |(code, _)| code)
    }

    /// 调用点必须是无条件的一条语句:判据收在被调函数里,由上面的判据测试钉住。
    fn assert_unconditional_call(src: &str, call: &str, file: &str) {
        let lines: Vec<&str> = src.lines().collect();
        let at = lines
            .iter()
            .position(|line| line.trim_start().starts_with(call))
            .unwrap_or_else(|| panic!("{file} 里找不到调用 `{call}`"));
        let previous = lines[..at]
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| !line.is_empty() && !line.starts_with("//"))
            .unwrap_or_default();
        assert!(
            !previous.starts_with("if ") && !previous.starts_with("} else"),
            "{file} 里 `{call}` 被包进了条件:{previous}"
        );
    }

    #[test]
    fn every_clear_point_is_wired() {
        let dispatcher = production(include_str!("dispatcher.rs"));
        assert_unconditional_call(
            dispatcher,
            "super::waiting::clear_on_inbound(",
            "dispatcher.rs",
        );
        let admission = &dispatcher[dispatcher.find("let sender_is_bot =").expect("准入")..];
        let admission = &admission[..admission
            .find("chats.entry(chat_id.clone())")
            .expect("建会话")];
        assert!(
            admission.contains("super::waiting::clear_on_inbound("),
            "人的消息准入时要清"
        );

        let chat_thread = production(include_str!("chat_thread.rs"));
        // 拼接写法:全仓扫描「一轮入口只在 chat_thread.rs 调用」的守卫会把字面量当成调用点。
        let start = &chat_thread[chat_thread
            .find(concat!("match run_after_durable_start", "("))
            .expect("durable start")..];
        let start = &start[..start.find(concat!("handle_task", "(")).expect("起跑")];
        assert!(
            start.contains("super::waiting::clear_on_turn_start("),
            "排队的人的消息开跑时要再清一次"
        );
        assert_unconditional_call(
            chat_thread,
            "super::waiting::clear_on_turn_start(",
            "chat_thread.rs",
        );

        let cards = production(include_str!("cards.rs"));
        assert_unconditional_call(cards, "super::waiting::clear_on_card(", "cards.rs");
        assert_unconditional_call(cards, "super::waiting::clear_on_keepalive(", "cards.rs");
        let keepalive = &cards[cards.find("Control::KeepAlive => {").expect("保活臂")..];
        let keepalive = &keepalive[..keepalive.find("Control::Compact =>").expect("保活臂结束")];
        for fact in [
            "super::waiting::clear_on_keepalive(",
            "CardAction::from_action(",
            "matches!(origin, CardOrigin::Idle(_))",
            "operator_open_id: &card.operator_open_id",
            "is_group: card_chat_is_group(chats, agent, chat_id)",
            "chat_id: &card.chat_id",
        ] {
            assert!(
                keepalive.contains(fact),
                "保活臂要把真实点击交给判据:{fact}"
            );
        }
        let waiting = production(include_str!("waiting.rs"));
        let keepalive_entry = &waiting[waiting
            .find("pub(super) fn clear_on_keepalive(")
            .expect("保活清除入口")..];
        assert!(
            keepalive_entry.contains("fc.resolve_union_id(click.operator_open_id)"),
            "union_id 按点击人解析"
        );
        let ended = &cards[cards
            .find("fn handle_archive_ended(")
            .expect("结束说明归档")..];
        let ended = &ended[..ended.find("let outcome =").expect("归档主体")];
        assert!(ended.contains("super::waiting::clear_waiting("));

        let teardown = production(include_str!("teardown.rs"));
        let fleet = &teardown[teardown
            .find("fn handle_fleet_chat_control(")
            .expect("管家卡")..];
        let fleet = &fleet[..fleet.find("if let Some(handle)").expect("在线分支")];
        assert!(fleet.contains("super::waiting::clear_waiting("));
    }

    #[test]
    fn the_turn_end_sets_waiting_before_the_agent_can_archive() {
        let chat_thread = production(include_str!("chat_thread.rs"));
        let consumed = &chat_thread[chat_thread
            .find("MarkConsumedOutcome::AlreadyConsumed) =>")
            .expect("销账成功臂")..];
        let anchor = consumed
            .find("super::waiting::waiting_anchor(")
            .expect("轮末要算 waiting 锚点");
        let archive = consumed
            .find("archive_on_agent_request(")
            .expect("agent 自己归档");
        assert!(anchor < archive, "置位必须在 agent 自己归档之前");
        let wiring = &consumed[anchor..archive];
        assert!(
            wiring.contains("super::waiting::mark_waiting("),
            "算出锚点之后要真的置位"
        );
        assert!(
            wiring.contains("task.sender_is_bot,"),
            "判据要拿到这一轮真实的发起方"
        );
    }
}
