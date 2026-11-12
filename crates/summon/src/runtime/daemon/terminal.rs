//! 终端卡的签发。`/term`、面板与回执上的「>_ 打开终端」、私聊卡上的「🔄 换一组新链接」共用这里。
//! 卡只私聊发给触发的人,链接绝不进群(owner 2026-09-22)。

use std::collections::HashMap;

use summon_core::driver::slash_pool::{self, SlashBlacklist, SlashPlan, SlashToggles};
use summon_core::driver::CliDriver;
use summon_core::registry::{chat_tag, Agent, CaptureMode, OperateScope};
use summon_core::web_terminal::grants::GrantTarget;

use super::cards::{card_chat_is_group, resolve_chat_session, resolve_chat_type};
use super::{CardActionMsg, ChatHandle};
use crate::frontend::client::FeishuClient;
use crate::frontend::outbound::{
    send_card_to_open_id_redacted_archived_id, send_markdown_archived,
    send_markdown_to_open_id_archived, update_card_unlinked_archived,
};
use crate::observe::log::log;
use crate::session::tmux::Tmux;

pub(super) struct IssueRequest<'a> {
    pub agent: &'a Agent,
    pub chat_id: &'a str,
    pub chat_type: &'a str,
    /// 这个 chat 的 tmux 会话名;查不到时为 None。
    pub tmux_session: Option<&'a str>,
    pub recipient_open_id: &'a str,
}

pub(super) enum IssueOutcome {
    Sent,
    Refused(String),
}

/// 按钮无权限时的回复,与集中闸同一句。
const OPERATE_DENIED: &str = "r2e1b5d0084";
/// 文本 `/term` 无权限时的回复,与 `/session clean|reset` 同一句式。
pub(super) const TERM_DENIED: &str = "r2a0c42695d";

fn is_group(chat_type: &str) -> bool {
    matches!(chat_type, "group" | "topic_group")
}

pub(super) fn headless_refusal(agent: &str, tag: &str) -> String {
    summon_i18n::format!("r70b8cc508a")
}

fn service_absent_refusal(reason: &str) -> String {
    summon_i18n::format!("ra215534768")
}

/// exact 无参 `/term`。判定与 chat 线程共用 `plan_slash`:同形变体与带参形态照旧由规划器拒绝。
pub(super) fn is_terminal_command(
    agent: &Agent,
    text: &str,
    mentions: &[summon_core::ipc::Mention],
    blacklist: &SlashBlacklist,
) -> bool {
    let cli = CliDriver::from_kind(agent.cli_kind());
    summon_core::ipc::slash_command_text(text, mentions, cli).is_some_and(|command| {
        slash_pool::plan_slash(
            cli,
            agent.capture_mode(),
            &command,
            blacklist,
            &SlashToggles::default(),
        ) == SlashPlan::Terminal
    })
}

/// 文本 `/term` 的私聊卡就是成功回复，拒绝时在原会话说明原因。
pub(super) fn terminal_command_reply(outcome: IssueOutcome) -> Option<String> {
    match outcome {
        IssueOutcome::Sent => None,
        IssueOutcome::Refused(reason) => Some(reason),
    }
}

/// 渲染好的终端卡,外加作废上一组所需要的凭据(目标 + 这一组的代次)。假实现不接终端服务,
/// 没有可作废的东西,`links` 为 None。
struct Issued {
    card: String,
    links: Option<(GrantTarget, u64)>,
}

/// 终端卡 handler 对外的动作:终端服务(出链接、认卡)与飞书上的卡片收发。生产走
/// `LiveTerminalCards`;测试换成记账的假实现,核对签没签、哪张卡被原地更新、哪张是新发的。
trait TerminalCards {
    /// 出一组链接并渲染成卡;前提不满足时给出人能照着做的原因。**上一组这时还有效**。
    fn render(&self, req: &IssueRequest) -> Result<Issued, String>;
    /// 卡确实送到了:作废上一组(没用过的码与已经打开的页面)。送不到就不作废——否则用户的页面
    /// 被关掉,而新链接一条都没到手,回执只会说卡片没发出去。
    fn commit(&self, issued: &Issued);
    /// 这张卡是不是 summon 私聊发出去的终端卡。
    fn is_ours(&self, message_id: &str) -> bool;
    /// 原地更新私聊里的那张卡。
    fn update(&self, dm_chat_id: &str, message_id: &str, card: &str) -> anyhow::Result<()>;
    /// 私聊发一张新卡并记下它。
    fn send(&self, recipient_open_id: &str, card: &str) -> anyhow::Result<()>;
}

struct LiveTerminalCards<'a> {
    fc: &'a FeishuClient,
    agent: &'a Agent,
}

impl TerminalCards for LiveTerminalCards<'_> {
    fn render(&self, req: &IssueRequest) -> Result<Issued, String> {
        terminal_card_for(self.fc, req)
    }

    fn commit(&self, issued: &Issued) {
        let Some((target, generation)) = &issued.links else {
            return;
        };
        if let Ok(service) = crate::runtime::web_terminal::service() {
            service.revoke_superseded(target, *generation);
        }
    }

    fn is_ours(&self, message_id: &str) -> bool {
        crate::runtime::web_terminal::is_terminal_card(message_id)
    }

    fn update(&self, dm_chat_id: &str, message_id: &str, card: &str) -> anyhow::Result<()> {
        update_card_unlinked_archived(
            self.fc,
            &self.agent.state_dir(),
            "p2p",
            dm_chat_id,
            "terminal_card_update",
            message_id,
            card,
        )
    }

    fn send(&self, recipient_open_id: &str, card: &str) -> anyhow::Result<()> {
        let message_id = send_card_to_open_id_redacted_archived_id(
            self.fc,
            &self.agent.state_dir(),
            recipient_open_id,
            "terminal_card",
            card,
        )?;
        crate::runtime::web_terminal::remember_card(&message_id);
        Ok(())
    }
}

/// 出一组链接并渲染成卡。任一前提不满足就给出人能照着做的原因。先看服务在不在:服务没起时
/// 「先把会话拉起来」是一句白费功夫的指引。
fn terminal_card_for(fc: &FeishuClient, req: &IssueRequest) -> Result<Issued, String> {
    if req.agent.capture_mode() != CaptureMode::Transcript {
        let tag = chat_tag(req.chat_type, req.chat_id);
        return Err(headless_refusal(&req.agent.name, &tag));
    }
    let service = crate::runtime::web_terminal::service()
        .map_err(|reason| service_absent_refusal(&reason))?;
    let Some(session) = req
        .tmux_session
        .filter(|s| Tmux::new(s.to_string()).has_session())
    else {
        return Err(summon_i18n::text!("rd0ad9b1afd").to_string());
    };
    let chat_label = if is_group(req.chat_type) {
        fc.resolve_source_group(req.chat_id)
            .unwrap_or_else(|| summon_i18n::text!("r35b49ee58a").to_string())
    } else {
        summon_i18n::text!("r6add2714c4").to_string()
    };
    let target = GrantTarget {
        agent: req.agent.name.clone(),
        chat_id: req.chat_id.to_string(),
        chat_label: chat_label.clone(),
        tmux_session: session.to_string(),
        recipient_open_id: req.recipient_open_id.to_string(),
    };
    let links = service
        .issue(target.clone())
        .map_err(|refusal| refusal.reason().to_string())?;
    log(&summon_i18n::format!(
        "r984d4f8d72",
        req.agent.name,
        req.chat_id,
        req.recipient_open_id
    ));
    Ok(Issued {
        card: summon_core::feishu::card_terminal(&summon_core::feishu::TerminalCard {
            agent: &req.agent.name,
            chat_label: &chat_label,
            read_only_url: &links.read_only,
            operate_url: &links.operate,
            target_chat_id: req.chat_id,
            target_chat_type: req.chat_type,
            recipient_open_id: req.recipient_open_id,
        }),
        links: Some((target, links.generation)),
    })
}

/// 私聊一张终端卡。私聊发不出去时不退回群里发链接。
pub(super) fn send_terminal_card(fc: &FeishuClient, req: &IssueRequest) -> IssueOutcome {
    send_terminal_card_with(
        &LiveTerminalCards {
            fc,
            agent: req.agent,
        },
        req,
    )
}

fn send_terminal_card_with(cards: &impl TerminalCards, req: &IssueRequest) -> IssueOutcome {
    let issued = match cards.render(req) {
        Ok(issued) => issued,
        Err(reason) => return IssueOutcome::Refused(reason),
    };
    match cards.send(req.recipient_open_id, &issued.card) {
        Ok(()) => {
            cards.commit(&issued);
            IssueOutcome::Sent
        }
        Err(e) => {
            log(&summon_i18n::format!("r5412c06d23", req.agent.name));
            IssueOutcome::Refused(summon_i18n::text!("re19b6306b7").to_string())
        }
    }
}

/// 「>_ 打开终端」:卡在哪个 chat 被点,就给那个 chat 的会话出卡。集中闸已按该 chat 判过权限,
/// 这里按同一语义再判一次,不单靠集中闸的豁免名单。成功时不在群里说话:sidecar 的即时 toast
/// 就是反馈,卡已经私聊过去了。
pub(super) fn handle_card_terminal_open(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    open_terminal_with(chats, agent, fc, card, &LiveTerminalCards { fc, agent });
}

fn open_terminal_with(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
    cards: &impl TerminalCards,
) {
    let chat_id = &card.chat_id;
    let chat_type = resolve_chat_type(chats, agent, chat_id);
    let reply = |text: &str| {
        let _ = send_markdown_archived(fc, &agent.state_dir(), &chat_type, chat_id, text);
    };
    let op_union = fc.resolve_union_id(&card.operator_open_id);
    if !agent.can_operate(
        &card.operator_open_id,
        &op_union,
        card_chat_is_group(chats, agent, chat_id),
        OperateScope::Chat(chat_id),
    ) {
        return reply(::summon_core::i18n::label(OPERATE_DENIED));
    }
    let session = resolve_chat_session(chats, agent, chat_id);
    let req = IssueRequest {
        agent,
        chat_id,
        chat_type: &chat_type,
        tmux_session: session.as_deref(),
        recipient_open_id: &card.operator_open_id,
    };
    if let IssueOutcome::Refused(reason) = send_terminal_card_with(cards, &req) {
        reply(&reason);
    }
}

/// 换新链接的绑定校验:同一个 agent、点的人就是原收件人、目标 chat 完整。
pub(super) fn regen_binding(
    value: &serde_json::Value,
    agent_name: &str,
    operator: &str,
) -> Option<(String, String)> {
    let field = |k: &str| value.get(k).and_then(serde_json::Value::as_str);
    if field("bound_agent")? != agent_name || field("bound_requester")? != operator {
        return None;
    }
    let chat_id = field("target_chat_id").filter(|s| !s.is_empty())?;
    let chat_type = field("target_chat_type")?;
    matches!(chat_type, "group" | "topic_group" | "p2p")
        .then(|| (chat_id.to_string(), chat_type.to_string()))
}

/// 新一组链接送到哪里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CardDelivery {
    /// 点的就是 summon 私聊发出去的那张卡:原地更新。
    UpdateInPlace,
    /// 点的卡 summon 不认得(转发出去的副本、daemon 重启前发的):私聊发一张新卡,别的卡一律不碰。
    SendNew,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RegenPlan {
    Refuse(&'static str),
    Issue {
        chat_id: String,
        chat_type: String,
        delivery: CardDelivery,
    },
}

/// 「🔄 换一组新链接」的判定。绑定不对就拒;目标 chat 的群语义取按钮值与本地记录里更严的那个
/// (任一说是群就按群判),再按它过 operate-gate;最后看点的是不是我们发出去的那张卡。
pub(super) fn plan_regenerate(
    value: &serde_json::Value,
    agent_name: &str,
    operator: &str,
    target_is_group_on_record: impl FnOnce(&str) -> bool,
    can_operate: impl FnOnce(&str, bool) -> bool,
    our_card: bool,
) -> RegenPlan {
    let Some((chat_id, chat_type)) = regen_binding(value, agent_name, operator) else {
        return RegenPlan::Refuse(summon_i18n::text!("r8bf9e058bc"));
    };
    let group = is_group(&chat_type) || target_is_group_on_record(&chat_id);
    if !can_operate(&chat_id, group) {
        return RegenPlan::Refuse(summon_i18n::text!("r61a273731a"));
    }
    RegenPlan::Issue {
        chat_id,
        chat_type,
        delivery: if our_card {
            CardDelivery::UpdateInPlace
        } else {
            CardDelivery::SendNew
        },
    }
}

/// 「🔄 换一组新链接」:卡在私聊里,dispatcher 不走集中闸;判定见 `plan_regenerate`。这里的回复
/// 一律私聊给点按钮的人,不发往卡所在的 chat——那张卡可能已被转发进群。
pub(super) fn handle_card_terminal_regenerate(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    regenerate_with(chats, agent, fc, card, &LiveTerminalCards { fc, agent });
}

fn regenerate_with(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
    cards: &impl TerminalCards,
) {
    let operator = &card.operator_open_id;
    let reply = |text: &str| {
        let _ = send_markdown_to_open_id_archived(
            fc,
            &agent.state_dir(),
            operator,
            "terminal_notice",
            text,
        );
    };
    let our_card = cards.is_ours(&card.message_id);
    let plan = plan_regenerate(
        &card.value,
        &agent.name,
        operator,
        |chat_id| card_chat_is_group(chats, agent, chat_id),
        |chat_id, group| {
            let union = fc.resolve_union_id(operator);
            agent.can_operate(operator, &union, group, OperateScope::Chat(chat_id))
        },
        our_card,
    );
    let (chat_id, chat_type, delivery) = match plan {
        RegenPlan::Refuse(text) => return reply(text),
        RegenPlan::Issue {
            chat_id,
            chat_type,
            delivery,
        } => (chat_id, chat_type, delivery),
    };
    let session = resolve_chat_session(chats, agent, &chat_id);
    let req = IssueRequest {
        agent,
        chat_id: &chat_id,
        chat_type: &chat_type,
        tmux_session: session.as_deref(),
        recipient_open_id: operator,
    };
    let issued = match cards.render(&req) {
        Ok(issued) => issued,
        Err(reason) => return reply(&reason),
    };
    if delivery == CardDelivery::UpdateInPlace {
        match cards.update(&card.chat_id, &card.message_id, &issued.card) {
            Ok(()) => return cards.commit(&issued),
            Err(e) => log(&summon_i18n::format!("r5546434ed1", agent.name)),
        }
    }
    match cards.send(operator, &issued.card) {
        Ok(()) => cards.commit(&issued),
        Err(e) => {
            log(&summon_i18n::format!("r5412c06d23", agent.name));
            reply(summon_i18n::text!("rdd19b6edc3"));
        }
    }
}

/// 终端卡上「只读查看」「可操作」的点击回调:点击人是这张卡的收件人、且对目标 chat 仍有操作
/// 权限,就激活这条链接(落地页 1 分钟内可进入);否则把它的激活作废,并私聊告诉点的人。卡在私聊里,
/// dispatcher 不走集中闸,判权同 `plan_regenerate`。
pub(super) fn handle_card_terminal_auth(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let operator = &card.operator_open_id;
    let code = card
        .value
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let allowed = terminal_auth_allowed(
        &card.value,
        &agent.name,
        operator,
        |chat_id| card_chat_is_group(chats, agent, chat_id),
        |chat_id, group| {
            let union = fc.resolve_union_id(operator);
            agent.can_operate(operator, &union, group, OperateScope::Chat(chat_id))
        },
    );
    let activated = crate::runtime::web_terminal::service()
        .is_ok_and(|service| service.activate(code, operator, allowed));
    log(&summon_i18n::format!("r8a2aeccc69", agent.name));
    if !allowed {
        let _ = send_markdown_to_open_id_archived(
            fc,
            &agent.state_dir(),
            operator,
            "terminal_notice",
            summon_i18n::text!("re2f65603fb"),
        );
    }
}

/// 终端链接点击的判权:卡片绑定的 agent 与收件人对得上,且收件人对目标 chat 仍过 operate-gate。
pub(super) fn terminal_auth_allowed(
    value: &serde_json::Value,
    agent_name: &str,
    operator: &str,
    target_is_group_on_record: impl FnOnce(&str) -> bool,
    can_operate: impl FnOnce(&str, bool) -> bool,
) -> bool {
    regen_binding(value, agent_name, operator).is_some_and(|(chat_id, chat_type)| {
        let group = is_group(&chat_type) || target_is_group_on_record(&chat_id);
        can_operate(&chat_id, group)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value() -> serde_json::Value {
        serde_json::json!({
            "action": "term_regen",
            "bound_agent": "arcadia",
            "bound_requester": "ou_a",
            "target_chat_id": "oc_1",
            "target_chat_type": "group",
        })
    }

    #[test]
    fn regenerating_requires_the_same_agent_and_the_original_recipient() {
        assert_eq!(
            regen_binding(&value(), "arcadia", "ou_a"),
            Some(("oc_1".to_string(), "group".to_string()))
        );
        assert_eq!(
            regen_binding(&value(), "starlight", "ou_a"),
            None,
            "别的 agent 的卡"
        );
        assert_eq!(
            regen_binding(&value(), "arcadia", "ou_b"),
            None,
            "转发出去的卡"
        );
    }

    #[test]
    fn regenerating_rejects_a_missing_or_unknown_target() {
        let mut v = value();
        v["target_chat_type"] = "channel".into();
        assert_eq!(regen_binding(&v, "arcadia", "ou_a"), None);
        let mut v = value();
        v.as_object_mut().unwrap().remove("target_chat_id");
        assert_eq!(regen_binding(&v, "arcadia", "ou_a"), None);
    }

    fn plan(value: &serde_json::Value, operator_ok: bool, our_card: bool) -> RegenPlan {
        plan_regenerate(
            value,
            "arcadia",
            "ou_a",
            |_| false,
            |_, _| operator_ok,
            our_card,
        )
    }

    #[test]
    fn only_the_card_we_sent_is_updated_in_place() {
        let issue = |delivery| RegenPlan::Issue {
            chat_id: "oc_1".into(),
            chat_type: "group".into(),
            delivery,
        };
        assert_eq!(
            plan(&value(), true, true),
            issue(CardDelivery::UpdateInPlace)
        );
        assert_eq!(
            plan(&value(), true, false),
            issue(CardDelivery::SendNew),
            "认不得的卡(转发出去的副本)不碰,另发新卡"
        );
    }

    #[test]
    fn a_non_operator_gets_no_new_links() {
        assert!(matches!(
            plan(&value(), false, true),
            RegenPlan::Refuse(text) if text.contains("🔒")
        ));
        assert!(matches!(
            plan_regenerate(&value(), "arcadia", "ou_b", |_| false, |_, _| true, true),
            RegenPlan::Refuse(_)
        ));
    }

    #[test]
    fn the_target_counts_as_a_group_if_either_the_button_or_the_record_says_so() {
        let mut p2p = value();
        p2p["target_chat_type"] = "p2p".into();
        let seen = std::cell::Cell::new(None);
        let _ = plan_regenerate(
            &p2p,
            "arcadia",
            "ou_a",
            |_| true,
            |_, group| {
                seen.set(Some(group));
                true
            },
            true,
        );
        assert_eq!(seen.get(), Some(true), "本地记录说是群,按群判");
        let _ = plan_regenerate(
            &p2p,
            "arcadia",
            "ou_a",
            |_| false,
            |_, group| {
                seen.set(Some(group));
                true
            },
            true,
        );
        assert_eq!(seen.get(), Some(false));
    }

    #[test]
    fn regenerating_checks_operate_against_the_target_chat() {
        let seen = std::cell::RefCell::new(String::new());
        let _ = plan_regenerate(
            &value(),
            "arcadia",
            "ou_a",
            |_| false,
            |chat_id, _| {
                *seen.borrow_mut() = chat_id.to_string();
                true
            },
            true,
        );
        assert_eq!(*seen.borrow(), "oc_1", "卡在私聊里,权限按目标会话判");
    }

    fn agent_in(tmp: &tempfile::TempDir) -> Agent {
        let mut agent: Agent =
            serde_yaml::from_str("name: claude-agent\nfeishu:\n  operators:\n    - ou_owner\n")
                .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        agent
    }

    fn click(chat_id: &str, operator: &str, value: serde_json::Value) -> CardActionMsg {
        click_on("om_card", chat_id, operator, value)
    }

    fn click_on(
        message_id: &str,
        chat_id: &str,
        operator: &str,
        value: serde_json::Value,
    ) -> CardActionMsg {
        CardActionMsg {
            chat_id: chat_id.to_string(),
            message_id: message_id.to_string(),
            operator_open_id: operator.to_string(),
            event_id: "ev_1".to_string(),
            value,
            wal_id: None,
            wal_replayed: false,
        }
    }

    fn archived(agent: &Agent, chat_type: &str, chat_id: &str) -> String {
        std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(chat_tag(chat_type, chat_id))
                .join("outbound.ndjson"),
        )
        .unwrap_or_default()
    }

    /// 「>_ 打开终端」自己判权限,不单靠集中闸的豁免名单:无权限的人只拿到 🔒。
    #[test]
    fn opening_a_terminal_checks_operate_permission_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(&tmp);
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let chats = HashMap::new();
        handle_card_terminal_open(
            &chats,
            &agent,
            &fc,
            &click(
                "oc_group",
                "ou_guest",
                serde_json::json!({"action": "term_open"}),
            ),
        );
        let reply = archived(&agent, "p2p", "oc_group");
        assert!(
            reply.contains(::summon_core::i18n::label(OPERATE_DENIED)),
            "{reply}"
        );
        assert!(
            !reply.contains("终端服务没有启动"),
            "没权限就不往下走:{reply}"
        );
    }

    /// 终端链接按钮的点击只认收件人本人,且他对目标会话仍过 operate-gate。
    #[test]
    fn a_terminal_link_click_counts_only_for_the_recipient_who_can_still_operate() {
        let mut auth = value();
        auth["action"] = "term_auth".into();
        let allowed = |agent: &str, operator: &str, operate_ok: bool| {
            terminal_auth_allowed(&auth, agent, operator, |_| false, |_, _| operate_ok)
        };
        assert!(allowed("arcadia", "ou_a", true));
        assert!(!allowed("arcadia", "ou_b", true), "转发出去的卡,别人点不算");
        assert!(!allowed("arcadia", "ou_a", false), "收件人已没有操作权限");
        assert!(!allowed("starlight", "ou_a", true), "别的 agent 的卡");
    }

    /// 别人点了终端链接按钮:私聊告诉他这张卡不属于他。
    #[test]
    fn a_refused_terminal_link_click_is_told_privately() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(&tmp);
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let mut v = value();
        v["action"] = "term_auth".into();
        v["bound_agent"] = "claude-agent".into();
        v["bound_requester"] = "ou_owner".into();
        v["code"] = "c".repeat(32).into();

        handle_card_terminal_auth(&HashMap::new(), &agent, &fc, &click("oc_dm", "ou_guest", v));
        assert!(archived(&agent, "p2p", "ou_guest").contains("🔒"));
    }

    /// 记账的终端卡:签发总是成功,记下签了几次、原地更新了哪张卡、给谁新发了卡、作废了几次
    /// 上一组。`send_fails` 模拟私聊发不出去。
    #[derive(Default)]
    struct FakeCards {
        ours: Vec<String>,
        update_fails: bool,
        send_fails: bool,
        rendered: std::cell::Cell<usize>,
        committed: std::cell::Cell<usize>,
        updated: std::cell::RefCell<Vec<String>>,
        sent_to: std::cell::RefCell<Vec<String>>,
    }

    impl TerminalCards for FakeCards {
        fn render(&self, _req: &IssueRequest) -> Result<Issued, String> {
            self.rendered.set(self.rendered.get() + 1);
            Ok(Issued {
                card: r#"{"card":"terminal"}"#.to_string(),
                links: None,
            })
        }
        fn commit(&self, _issued: &Issued) {
            self.committed.set(self.committed.get() + 1);
        }
        fn is_ours(&self, message_id: &str) -> bool {
            self.ours.iter().any(|id| id == message_id)
        }
        fn update(&self, _dm_chat_id: &str, message_id: &str, _card: &str) -> anyhow::Result<()> {
            if self.update_fails {
                anyhow::bail!("卡片已过期");
            }
            self.updated.borrow_mut().push(message_id.to_string());
            Ok(())
        }
        fn send(&self, recipient_open_id: &str, _card: &str) -> anyhow::Result<()> {
            if self.send_fails {
                anyhow::bail!("不在这个 bot 的可用范围内");
            }
            self.sent_to
                .borrow_mut()
                .push(recipient_open_id.to_string());
            Ok(())
        }
    }

    fn regen_click(message_id: &str) -> CardActionMsg {
        let mut v = value();
        v["bound_agent"] = "claude-agent".into();
        v["bound_requester"] = "ou_owner".into();
        click_on(message_id, "oc_dm", "ou_owner", v)
    }

    /// 认不得的卡(转发出去的副本、daemon 重启前发的)一律不碰:私聊给点的人发一张新卡。
    #[test]
    fn regenerating_on_an_unknown_card_sends_a_fresh_dm_card_and_updates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(&tmp);
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let cards = FakeCards {
            ours: vec!["om_ours".into()],
            ..FakeCards::default()
        };
        regenerate_with(
            &HashMap::new(),
            &agent,
            &fc,
            &regen_click("om_forwarded_copy"),
            &cards,
        );
        assert!(cards.updated.borrow().is_empty(), "认不得的卡不原地更新");
        assert_eq!(*cards.sent_to.borrow(), ["ou_owner"]);
    }

    #[test]
    fn regenerating_on_our_own_card_updates_it_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(&tmp);
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let cards = FakeCards {
            ours: vec!["om_ours".into()],
            ..FakeCards::default()
        };
        regenerate_with(
            &HashMap::new(),
            &agent,
            &fc,
            &regen_click("om_ours"),
            &cards,
        );
        assert_eq!(*cards.updated.borrow(), ["om_ours"]);
        assert!(cards.sent_to.borrow().is_empty(), "原地更新成功就不另发");

        let failing = FakeCards {
            ours: vec!["om_ours".into()],
            update_fails: true,
            ..FakeCards::default()
        };
        regenerate_with(
            &HashMap::new(),
            &agent,
            &fc,
            &regen_click("om_ours"),
            &failing,
        );
        assert_eq!(
            *failing.sent_to.borrow(),
            ["ou_owner"],
            "更新失败就私聊补发一张"
        );
    }

    /// 上一组链接是「卡确实送到了」才作废,不是签发那一刻就作废。私聊发不出去时用户手里已经开着
    /// 的页面还得能用——否则他既丢了旧链接,又没拿到新的,回执只说卡片没发出去。
    #[test]
    fn the_previous_links_are_revoked_only_after_the_card_is_delivered() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(&tmp);
        let req = IssueRequest {
            agent: &agent,
            chat_id: "oc_1",
            chat_type: "p2p",
            tmux_session: Some("soul-x"),
            recipient_open_id: "ou_owner",
        };

        let sent = FakeCards::default();
        assert!(matches!(
            send_terminal_card_with(&sent, &req),
            IssueOutcome::Sent
        ));
        assert_eq!(sent.committed.get(), 1, "送到了才作废上一组");

        let failed = FakeCards {
            send_fails: true,
            ..FakeCards::default()
        };
        assert!(matches!(
            send_terminal_card_with(&failed, &req),
            IssueOutcome::Refused(_)
        ));
        assert_eq!(failed.rendered.get(), 1, "签发了新的一组");
        assert_eq!(failed.committed.get(), 0, "卡没送到就不作废上一组");
    }

    /// 换一组新链接:原地更新成功、或更新失败后补发成功,都算送到;两条路都送不到就不作废。
    #[test]
    fn regenerating_revokes_the_previous_links_only_on_a_delivered_card() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(&tmp);
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let ours = || vec!["om_ours".to_string()];

        let updated = FakeCards {
            ours: ours(),
            ..FakeCards::default()
        };
        regenerate_with(
            &HashMap::new(),
            &agent,
            &fc,
            &regen_click("om_ours"),
            &updated,
        );
        assert_eq!(updated.committed.get(), 1, "原地更新成功即送到");

        let resent = FakeCards {
            ours: ours(),
            update_fails: true,
            ..FakeCards::default()
        };
        regenerate_with(
            &HashMap::new(),
            &agent,
            &fc,
            &regen_click("om_ours"),
            &resent,
        );
        assert_eq!(resent.committed.get(), 1, "补发成功也算送到");

        let lost = FakeCards {
            ours: ours(),
            update_fails: true,
            send_fails: true,
            ..FakeCards::default()
        };
        regenerate_with(&HashMap::new(), &agent, &fc, &regen_click("om_ours"), &lost);
        assert_eq!(lost.committed.get(), 0, "两条路都没送到就不作废上一组");
        assert!(
            archived(&agent, "p2p", "ou_owner").contains("原来那组链接仍然有效"),
            "回执要说清旧链接还能用:{}",
            archived(&agent, "p2p", "ou_owner")
        );
    }

    /// 生产的卡片收发:发出去的卡记进内存(换新链接据此认卡),归档里既没有卡面也没有 message_id。
    #[test]
    fn live_terminal_cards_remember_what_they_send_but_archive_no_message_id() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(&tmp);
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let live = LiveTerminalCards {
            fc: &fc,
            agent: &agent,
        };
        let card = r#"{"url":"http://10.0.0.5:7766/t/dddddddddddddddddddddddddddddddd"}"#;
        live.send("ou_owner", card).unwrap();
        // dry-run 发卡的 message_id 恒为 `dry-msg`。
        assert!(live.is_ours("dry-msg"), "发出去的卡要记住");
        live.update("oc_dm", "om_live_card", card).unwrap();
        for (chat_type, chat_id) in [("p2p", "ou_owner"), ("p2p", "oc_dm")] {
            let raw = archived(&agent, chat_type, chat_id);
            assert!(!raw.is_empty(), "{chat_id} 要有归档");
            for leak in ["dry-msg", "om_live_card", "/t/"] {
                assert!(!raw.contains(leak), "{leak}:{raw}");
            }
        }
    }

    /// 「>_ 打开终端」的 handler 按卡所在 chat 的群语义判权:名单全空的 agent 在群里谁都不放行
    /// (私聊里放行),不能靠集中闸兜着。
    #[test]
    fn opening_a_terminal_in_a_group_is_refused_when_no_one_is_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: open-agent
feishu: {}
",
        )
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        for (chat_id, chat_type) in [("oc_group", "group"), ("oc_dm", "p2p")] {
            super::super::chat_meta::write_chat_meta(
                &agent,
                &format!("soul-open-agent-{chat_id}"),
                chat_id,
                chat_type,
                None,
                "someone",
                None,
            );
        }
        let open = serde_json::json!({"action": "term_open"});

        let cards = FakeCards::default();
        open_terminal_with(
            &HashMap::new(),
            &agent,
            &fc,
            &click("oc_group", "ou_anyone", open.clone()),
            &cards,
        );
        assert!(
            archived(&agent, "group", "oc_group")
                .contains(::summon_core::i18n::label(OPERATE_DENIED)),
            "{}",
            archived(&agent, "group", "oc_group")
        );
        assert_eq!(cards.rendered.get(), 0, "没签发");
        assert!(cards.sent_to.borrow().is_empty());

        let cards = FakeCards::default();
        open_terminal_with(
            &HashMap::new(),
            &agent,
            &fc,
            &click("oc_dm", "ou_anyone", open),
            &cards,
        );
        assert_eq!(*cards.sent_to.borrow(), ["ou_anyone"], "私聊里放行");
    }

    /// 「🔄 换一组新链接」的回复只私聊给点的人,不发往卡所在的 chat(那张卡可能已被转发进群)。
    #[test]
    fn regeneration_replies_go_to_the_operator_dm_only() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(&tmp);
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let chats = HashMap::new();
        let mut v = value();
        v["bound_agent"] = "claude-agent".into();
        handle_card_terminal_regenerate(&chats, &agent, &fc, &click("oc_group_copy", "ou_b", v));
        assert!(
            archived(&agent, "p2p", "ou_b").contains("这张终端卡不属于你"),
            "回复私聊给点的人"
        );
        for chat_type in ["p2p", "group"] {
            assert!(
                archived(&agent, chat_type, "oc_group_copy").is_empty(),
                "卡所在的 chat 里什么都不发"
            );
        }
    }

    #[test]
    fn headless_refusal_points_to_agents_tail() {
        let text = headless_refusal("codex-bot", "gabc");
        assert!(text.contains("summon agents tail codex-bot --chat gabc"));
        // 无头 agent 没有可 attach 的会话,指去 attach 是一条走不通的路。
        assert!(!text.contains("attach"));
    }

    /// 服务没起时先说服务:这时候让人去把会话拉起来是白费功夫。测试进程从不装终端服务。
    #[test]
    fn a_missing_service_is_reported_before_a_missing_session_with_its_reason() {
        let agent: Agent = serde_yaml::from_str("name: claude-agent").unwrap();
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let req = IssueRequest {
            agent: &agent,
            chat_id: "oc_1",
            chat_type: "group",
            tmux_session: None,
            recipient_open_id: "ou_a",
        };
        let Err(reason) = terminal_card_for(&fc, &req) else {
            panic!("测试进程里没有终端服务");
        };
        assert!(reason.starts_with("终端服务没有启动"), "{reason}");
        assert!(
            reason.contains("daemon 没有起终端服务"),
            "要带上原因:{reason}"
        );
        assert_eq!(
            service_absent_refusal("网页终端绑定 0.0.0.0:7766 失败: Address already in use"),
            "终端服务没有启动:网页终端绑定 0.0.0.0:7766 失败: Address already in use。"
        );
    }

    #[test]
    fn term_is_recognised_only_in_its_exact_form() {
        let agent: Agent = serde_yaml::from_str("name: claude-agent").unwrap();
        let blacklist = SlashBlacklist::default();
        assert!(is_terminal_command(&agent, "/term", &[], &blacklist));
        for other in ["/TERM", "/term!", "/term now", "/terminal-setup", "term"] {
            assert!(
                !is_terminal_command(&agent, other, &[], &blacklist),
                "{other}"
            );
        }
    }

    #[test]
    fn the_text_command_success_keeps_only_the_private_card() {
        assert_eq!(terminal_command_reply(IssueOutcome::Sent), None);
        assert_eq!(
            terminal_command_reply(IssueOutcome::Refused("x".into())).as_deref(),
            Some("x")
        );
    }
}
