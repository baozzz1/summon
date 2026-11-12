//! 控制台入口:`/dash` 私聊发起人一张控制台卡,卡上只有一个打开链接的按钮。
//! 卡只私聊发给发起人,链接绝不进群,群里发的命令也不在群里应答(私聊本身发不出去时才在群里说一句原因);
//! 判权与 `/term` 同一口径(发起人过该 agent 在这个 chat 上的
//! operate-gate)。链接一次性、按 `preview_server.token_ttl_secs` 过期,落地页 POST 进入时按此刻
//! 的名单重判签发人(见 `runtime::dashboard::server`)。设计见
//! `docs/spec/2026-10-20-dashboard-console-design.md`「访问与鉴权」。

use std::sync::Arc;

use summon_core::driver::slash_pool::{self, SlashBlacklist, SlashPlan, SlashToggles};
use summon_core::driver::CliDriver;
use summon_core::registry::Agent;

use crate::frontend::client::FeishuClient;
use crate::frontend::outbound::send_card_to_open_id_redacted_archived_id;
use crate::observe::log::log;
use crate::runtime::dashboard::sessions::{ChatKind, Issuer};
use crate::runtime::dashboard::Dashboard;
use crate::runtime::web_terminal::CardMemory;

/// 文本 `/dash` 无权限时说给发起人的话,与 `/term` 同一句式。
pub(super) const DASH_DENIED: &str = "r51eb9042d6";
const DASH_SEND_FAILED: &str = "r8cb5de1980";

/// 私聊发出去的控制台卡。引用解析据此把卡挡在 agent 之外:卡上的链接只给收件人本人,bot 身份
/// 按 message_id 能把卡面取回来。
static DASHBOARD_CARDS: CardMemory = CardMemory::new();

/// 这张卡是不是 summon 私聊发出去的控制台卡。
pub(super) fn is_dashboard_card(message_id: &str) -> bool {
    DASHBOARD_CARDS.contains(message_id)
}

#[cfg(test)]
pub(super) fn remember_card_for_test(message_id: &str) {
    DASHBOARD_CARDS.remember(message_id);
}

/// exact 无参 `/dash`。判定与 chat 线程共用 `plan_slash`:同形变体与带参形态由规划器拒绝。
pub(super) fn is_dashboard_command(
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
        ) == SlashPlan::Dashboard
    })
}

/// 一次已过 operate-gate 的 `/dash`。身份全部来自飞书入站事件。
pub(super) struct DashRequest<'a> {
    pub agent: &'a Agent,
    pub chat_id: &'a str,
    pub is_group: bool,
    pub sender_open_id: &'a str,
    /// 判 operate-gate 时用的那个 union_id(事件自带,或联系人 API 补的)。
    pub sender_union_id: &'a str,
    /// 只做展示;取不到为空。
    pub sender_name: &'a str,
}

fn issuer_for(req: &DashRequest) -> Issuer {
    Issuer {
        agent: req.agent.name.clone(),
        chat_id: req.chat_id.to_string(),
        chat_kind: if req.is_group {
            ChatKind::Group
        } else {
            ChatKind::P2p
        },
        open_id: req.sender_open_id.to_string(),
        union_id: req.sender_union_id.to_string(),
        name: req.sender_name.to_string(),
    }
}

/// `/dash` 之后还要告诉发起人的话。群里发的命令不在群里应答,只有私聊本身发不出去时例外。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DashNotice {
    /// 卡已私聊送到:私聊里卡就是回复,群里什么都不发。
    Delivered,
    /// 说给发起人:私聊里就地回;群里发的私聊给他,私聊也发不出去才回到群里。
    ToSender(String),
    /// 私聊本身发不出去,只能在原会话里说。
    InChat(String),
}

/// 签出的一条链接与它的有效期(秒,写在卡上)。
struct Issued {
    url: String,
    ttl_secs: u64,
}

/// 签一条链接;服务没起、或出不了远程链接时给出人能照着做的原因。
fn issue_with(service: Result<Arc<Dashboard>, String>, issuer: Issuer) -> Result<Issued, String> {
    let dash = service.map_err(|reason| summon_i18n::format!("r8936bb3222"))?;
    let url = dash
        .issue_link(issuer)
        .map_err(|refusal| refusal.reason().to_string())?;
    Ok(Issued {
        url,
        ttl_secs: dash.link_ttl().as_secs(),
    })
}

/// 私聊发起人一张控制台卡,返回还要告诉他的话。
pub(super) fn send_dashboard_card(fc: &FeishuClient, req: &DashRequest) -> DashNotice {
    send_dashboard_card_with(
        req,
        |issuer| issue_with(crate::runtime::dashboard::service(), issuer),
        |card| {
            send_card_to_open_id_redacted_archived_id(
                fc,
                &req.agent.state_dir(),
                req.sender_open_id,
                "dashboard_card",
                card,
            )
        },
    )
}

/// 签链接 → 渲染 → 私聊发送。私聊发不出去时不退回群里发链接。
fn send_dashboard_card_with(
    req: &DashRequest,
    issue: impl FnOnce(Issuer) -> Result<Issued, String>,
    send: impl FnOnce(&str) -> anyhow::Result<String>,
) -> DashNotice {
    let issued = match issue(issuer_for(req)) {
        Ok(issued) => issued,
        Err(reason) => return DashNotice::ToSender(reason),
    };
    let card = summon_core::feishu::card_dashboard(&issued.url, issued.ttl_secs);
    match send(&card) {
        Ok(message_id) => {
            DASHBOARD_CARDS.remember(&message_id);
            log(&summon_i18n::format!(
                "r735562ed81",
                req.agent.name,
                req.chat_id,
                req.sender_open_id
            ));
            DashNotice::Delivered
        }
        Err(e) => {
            log(&summon_i18n::format!("r0c8fbff2e8", req.agent.name));
            DashNotice::InChat(::summon_core::i18n::label(DASH_SEND_FAILED).to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME_OPEN: &str = "ou_00000000000000000000000000000001";
    const ME_UNION: &str = "on_00000000000000000000000000000001";
    const GROUP: &str = "oc_00000000000000000000000000000001";

    fn agent(name: &str) -> Agent {
        serde_yaml::from_str(&format!("name: {name}\n")).unwrap()
    }

    fn req<'a>(agent: &'a Agent, is_group: bool) -> DashRequest<'a> {
        DashRequest {
            agent,
            chat_id: GROUP,
            is_group,
            sender_open_id: ME_OPEN,
            sender_union_id: ME_UNION,
            sender_name: "示例用户",
        }
    }

    /// 出得了远程链接的控制台实例(不开监听),链接 10 分钟过期。
    fn dashboard() -> Arc<Dashboard> {
        let cfg = summon_core::config::Dashboard {
            public_base_url: "https://dash.example.com".into(),
            ..Default::default()
        };
        let preview = summon_core::config::PreviewServer {
            token_ttl_secs: 600,
            ..Default::default()
        };
        Arc::new(Dashboard::unbound_for_test(
            &cfg,
            &preview,
            "127.0.0.1:7767".parse().unwrap(),
            || None,
        ))
    }

    #[test]
    fn issuer_records_who_and_where_from_the_request() {
        let a = agent("starlight");
        let group = issuer_for(&req(&a, true));
        assert_eq!(group.agent, "starlight");
        assert_eq!(group.chat_id, GROUP);
        assert_eq!(group.chat_kind, ChatKind::Group);
        assert_eq!(
            (group.open_id.as_str(), group.union_id.as_str()),
            (ME_OPEN, ME_UNION)
        );
        assert_eq!(issuer_for(&req(&a, false)).chat_kind, ChatKind::P2p);
    }

    /// `/dash`:卡私聊给发起人(按钮带链接、写明有效期),之后什么都不再说 —— 群里发的不在群里
    /// 应答,私聊里发的卡本身就是回复。
    #[test]
    fn dash_sends_a_private_card_and_says_nothing_else() {
        let a = agent("starlight");
        let dash = dashboard();
        let mut sent = None;
        let reply = send_dashboard_card_with(
            &req(&a, true),
            |issuer| {
                assert_eq!(issuer.open_id, ME_OPEN);
                issue_with(Ok(Arc::clone(&dash)), issuer)
            },
            |card| {
                sent = Some(card.to_string());
                Ok("om_dash_card_group".to_string())
            },
        );
        assert_eq!(reply, DashNotice::Delivered);
        let card = sent.expect("卡要私聊发出去");
        assert!(card.contains("https://dash.example.com/l/"), "{card}");
        assert!(card.contains("打开控制台"), "{card}");
        assert!(
            card.contains("10 分钟内有效"),
            "有效期按 token_ttl_secs 写:{card}"
        );
        assert!(is_dashboard_card("om_dash_card_group"), "记下这张卡");

        let reply = send_dashboard_card_with(
            &req(&a, false),
            |issuer| issue_with(Ok(Arc::clone(&dash)), issuer),
            |_| Ok("om_dash_card_p2p".to_string()),
        );
        assert_eq!(reply, DashNotice::Delivered, "私聊里卡就是回复");
    }

    /// 私聊发不出去:只能在原会话里说明原因,链接不出现在回复里。
    #[test]
    fn a_failed_private_send_never_puts_the_link_in_the_chat() {
        let a = agent("starlight");
        let dash = dashboard();
        let mut url = String::new();
        let reply = send_dashboard_card_with(
            &req(&a, true),
            |issuer| {
                let issued = issue_with(Ok(Arc::clone(&dash)), issuer)?;
                url = issued.url.clone();
                Ok(issued)
            },
            |_| Err(anyhow::anyhow!("230013 bot 不在可用范围")),
        );
        let DashNotice::InChat(reply) = reply else {
            panic!("私聊发不出去只能在原会话里说:{reply:?}");
        };
        assert!(reply.contains("私聊发送失败"), "{reply}");
        assert!(!reply.contains(&url) && !reply.contains("/l/"), "{reply}");
    }

    /// 服务没起、或只听回环又没配对外地址:把原因说给发起人,不发卡。
    #[test]
    fn dash_without_service_or_loopback_only_explains_why() {
        let a = agent("starlight");
        let reply = send_dashboard_card_with(
            &req(&a, true),
            |issuer| issue_with(Err("daemon 没有起控制台服务".into()), issuer),
            |_| panic!("服务没起不该发卡"),
        );
        let DashNotice::ToSender(reply) = reply else {
            panic!("原因说给发起人:{reply:?}");
        };
        assert!(reply.contains("控制台服务没有启动"), "{reply}");
        assert!(reply.contains("daemon 没有起控制台服务"), "{reply}");

        let loopback = Arc::new(Dashboard::unbound_for_test(
            &summon_core::config::Dashboard::default(),
            &summon_core::config::PreviewServer::default(),
            "127.0.0.1:7767".parse().unwrap(),
            || None,
        ));
        let reply = send_dashboard_card_with(
            &req(&a, true),
            |issuer| issue_with(Ok(loopback), issuer),
            |_| panic!("出不了链接不该发卡"),
        );
        assert_eq!(
            reply,
            DashNotice::ToSender(
                crate::runtime::dashboard::LinkRefusal::LoopbackBind
                    .reason()
                    .to_string()
            )
        );
    }
}
