//! waiting:agent 回完、轮到 owner 处理的 (agent, 会话)。
//!
//! 标记是盘上事实 —— daemon 在人触发的对话轮结束时置位,owner 的明确动作清除(见
//! `docs/spec/2026-10-15-menubar-waiting-design.md`)。这里只放纯数据与投影:标记的形状、
//! 跳转链接怎么拼、标记怎么并进 fleet 快照。读写盘在 summon 侧。

use crate::observe::fleet::FleetSnapshot;
use crate::session::session_view::{
    chat_type_from_tag, humanize_elapsed_secs, humanize_elapsed_secs_en, SessionView,
};
use serde::{Deserialize, Serialize};

/// 盘上的 waiting 标记(`<state_dir>/threads/<tag>/waiting.json`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingMark {
    pub chat_id: String,
    /// 这一轮登记的终态回复。「Handled」按它比较:对不上说明期间又有了新回复。
    pub reply_message_id: String,
    /// 跳到这条回复的链接。置位时由 daemon 拼好:只有它知道这个 agent 用的飞书域名,
    /// 也只有它拿得到回复的会话内序号。
    pub link: String,
    /// 置位时刻(unix 秒)。
    pub since_unix: u64,
}

/// fleet 快照里的一条 waiting。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingStatus {
    pub alias: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_type: Option<String>,
    pub reply_message_id: String,
    pub since_secs: u64,
    pub since_text: String,
    pub since_text_en: String,
    pub link: String,
}

/// 飞书 AppLink 域名。入参可以是 config 的 `default_domain`,也可以是开放平台 API 根。
pub fn applink_host(domain_or_api_base: &str) -> &'static str {
    if domain_or_api_base.contains("larksuite") {
        "applink.larksuite.com"
    } else {
        "applink.feishu.cn"
    }
}

/// 飞书桌面版会话链接。用 `lark://` 而不是 `https://applink…`:后者先开浏览器页再唤起客户端。
/// `position` 只认纯数字 —— 它会原样拼进 URL。
pub fn feishu_chat_link(applink_host: &str, chat_id: &str, position: Option<&str>) -> String {
    let mut link = format!(
        "lark://{applink_host}/client/chat/open?openChatId={}",
        encode_component(chat_id)
    );
    if let Some(position) =
        position.filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        link.push_str("&position=");
        link.push_str(position);
    }
    link
}

/// 飞书卡片里用的同一条跳转。快照里存的是桌面版注册给系统的 `lark://`;卡片在飞书客户端里
/// (含手机端)被点,用 AppLink 文档里的 `https://applink…` 形态,客户端内点它直接跳转。
/// 只换协议头;不是本模块拼出来的 AppLink 一律不认,免得把任意链接渲进卡片。
pub fn feishu_web_link(link: &str) -> Option<String> {
    let rest = link.strip_prefix("lark://")?;
    ["applink.feishu.cn/", "applink.larksuite.com/"]
        .iter()
        .any(|host| rest.starts_with(host))
        .then(|| format!("https://{rest}"))
}

/// Codex 桌面版(ChatGPT 应用)打开某个会话的链接。这是应用自己注册的内部路由,新版本若不认,
/// 系统只会拉起应用。`thread_id` 是 rollout 首行 `session_meta` 的 id。
pub fn codex_thread_link(thread_id: &str) -> String {
    format!("codex://threads/{}", encode_component(thread_id))
}

/// AppLink 文档要求参数值按 encodeURIComponent 编码。
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 把盘上标记并进快照:置位的会话与群成员标 `waiting`,另列顶层 `waiting`。
///
/// `sessions` 必须是**未过滤**的会话发现结果(含 Gone):agent 自己归档后会话已退出,
/// 那条回复 owner 可能还没看,名字仍要从盘上的会话事实里取。
pub fn attach_waiting(
    snapshot: &mut FleetSnapshot,
    sessions: &[(String, Vec<SessionView>)],
    marks: &[(String, String, WaitingMark)],
    now_unix: u64,
) {
    let is_waiting = |alias: &str, tag: &str| marks.iter().any(|(a, t, _)| a == alias && t == tag);
    for agent in &mut snapshot.agents {
        for chat in &mut agent.chats {
            chat.waiting = is_waiting(&agent.alias, &chat.tag);
        }
    }
    for group in &mut snapshot.groups {
        for member in &mut group.members {
            member.waiting = is_waiting(&member.alias, &group.tag);
        }
    }
    let mut items: Vec<WaitingStatus> = marks
        .iter()
        .map(|(alias, tag, mark)| {
            let view = sessions
                .iter()
                .filter(|(a, _)| a == alias)
                .flat_map(|(_, views)| views.iter())
                .find(|v| &v.tag == tag);
            let since_secs = now_unix.saturating_sub(mark.since_unix);
            WaitingStatus {
                alias: alias.clone(),
                tag: tag.clone(),
                name: view.and_then(|v| v.chat_name.clone()),
                chat_type: view
                    .and_then(|v| v.chat_type.clone())
                    .or_else(|| chat_type_from_tag(tag).map(str::to_string)),
                reply_message_id: mark.reply_message_id.clone(),
                since_secs,
                since_text: humanize_elapsed_secs(since_secs),
                since_text_en: humanize_elapsed_secs_en(since_secs),
                link: mark.link.clone(),
            }
        })
        .collect();
    items.sort_by(|a, b| {
        b.since_secs
            .cmp(&a.since_secs)
            .then(a.alias.cmp(&b.alias))
            .then(a.tag.cmp(&b.tag))
    });
    snapshot.waiting = items;
}

/// 给每个群填飞书桌面版的会话链接(只打开会话,不定位):Menu Bar 会话行的点击目标。
pub fn attach_group_links(snapshot: &mut FleetSnapshot, applink_host: &str) {
    for group in &mut snapshot.groups {
        group.link = group
            .chat_id
            .as_deref()
            .map(|chat_id| feishu_chat_link(applink_host, chat_id, None));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::registry::CaptureMode;
    use crate::observe::fleet::{build_fleet, RuntimeStatus};
    use crate::session_view::{Liveness, SessionView};
    use std::time::{Duration, SystemTime};

    const NOW: u64 = 1_800_000_000;

    fn view(agent: &str, tag: &str, name: Option<&str>, liveness: Liveness) -> SessionView {
        SessionView {
            agent: agent.into(),
            tag: tag.into(),
            session: format!("soul-{agent}-{tag}"),
            chat_id: Some(format!("oc_{tag}")),
            chat_type: Some(if tag.starts_with('g') { "group" } else { "p2p" }.into()),
            chat_name: name.map(str::to_string),
            capture: CaptureMode::Transcript,
            liveness,
            last_active: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 60)),
        }
    }

    fn mark(reply: &str, since_unix: u64) -> WaitingMark {
        WaitingMark {
            chat_id: "oc_x".into(),
            reply_message_id: reply.into(),
            link: "lark://applink.feishu.cn/client/chat/open?openChatId=oc_x".into(),
            since_unix,
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW)
    }

    #[test]
    fn feishu_link_carries_position_only_when_known() {
        assert_eq!(
            feishu_chat_link("applink.feishu.cn", "oc_ab12", Some("1485")),
            "lark://applink.feishu.cn/client/chat/open?openChatId=oc_ab12&position=1485"
        );
        assert_eq!(
            feishu_chat_link("applink.feishu.cn", "oc_ab12", None),
            "lark://applink.feishu.cn/client/chat/open?openChatId=oc_ab12"
        );
    }

    #[test]
    fn feishu_web_link_swaps_only_the_scheme_of_an_applink() {
        assert_eq!(
            feishu_web_link(
                "lark://applink.feishu.cn/client/chat/open?openChatId=oc_ab12&position=1485"
            )
            .as_deref(),
            Some("https://applink.feishu.cn/client/chat/open?openChatId=oc_ab12&position=1485")
        );
        assert_eq!(
            feishu_web_link("lark://applink.larksuite.com/client/chat/open?openChatId=oc_1")
                .as_deref(),
            Some("https://applink.larksuite.com/client/chat/open?openChatId=oc_1")
        );
        for other in [
            "https://applink.feishu.cn/client/chat/open?openChatId=oc_1",
            "lark://evil.example/client/chat/open",
            "lark://applink.feishu.cn.evil.example/x",
            "codex://threads/t1",
            "",
        ] {
            assert_eq!(feishu_web_link(other), None, "{other}");
        }
    }

    #[test]
    fn feishu_link_ignores_non_numeric_position() {
        assert_eq!(
            feishu_chat_link("applink.feishu.cn", "oc_ab12", Some("12&x=1")),
            "lark://applink.feishu.cn/client/chat/open?openChatId=oc_ab12"
        );
        assert_eq!(
            feishu_chat_link("applink.feishu.cn", "oc ab", None),
            "lark://applink.feishu.cn/client/chat/open?openChatId=oc%20ab",
            "参数值按 encodeURIComponent 编码"
        );
    }

    #[test]
    fn applink_host_follows_the_feishu_domain() {
        assert_eq!(applink_host("https://open.feishu.cn"), "applink.feishu.cn");
        assert_eq!(applink_host("feishu.cn"), "applink.feishu.cn");
        assert_eq!(
            applink_host("https://open.larksuite.com"),
            "applink.larksuite.com"
        );
        assert_eq!(applink_host("larksuite.com"), "applink.larksuite.com");
    }

    #[test]
    fn codex_link_targets_the_thread() {
        assert_eq!(
            codex_thread_link("01a13d7b-780f-7f30-b852-4b46fa59d770"),
            "codex://threads/01a13d7b-780f-7f30-b852-4b46fa59d770"
        );
    }

    #[test]
    fn attach_marks_chats_and_members_and_lists_oldest_first() {
        let sessions = vec![
            (
                "saturn".to_string(),
                vec![view("saturn", "g1", Some("研发群"), Liveness::LiveTmux)],
            ),
            (
                "arcadia".to_string(),
                vec![view("arcadia", "g1", Some("研发群"), Liveness::LiveTmux)],
            ),
        ];
        let mut snap = build_fleet(&sessions, None::<&RuntimeStatus>, now());
        let marks = vec![
            (
                "saturn".to_string(),
                "g1".to_string(),
                mark("om_new", NOW - 60),
            ),
            (
                "arcadia".to_string(),
                "g1".to_string(),
                mark("om_old", NOW - 7_200),
            ),
        ];
        attach_waiting(&mut snap, &sessions, &marks, NOW);

        let order: Vec<&str> = snap
            .waiting
            .iter()
            .map(|w| w.reply_message_id.as_str())
            .collect();
        assert_eq!(order, ["om_old", "om_new"], "等得最久的在前");
        assert_eq!(snap.waiting[0].since_secs, 7_200);
        assert_eq!(snap.waiting[0].since_text_en, "2h ago");
        assert_eq!(snap.waiting[0].name.as_deref(), Some("研发群"));
        assert!(snap
            .agents
            .iter()
            .all(|a| a.chats.iter().all(|c| c.waiting)));
        assert!(snap.groups[0].members.iter().all(|m| m.waiting));
    }

    #[test]
    fn exited_session_still_lists_with_its_name() {
        let sessions = vec![(
            "saturn".to_string(),
            vec![view("saturn", "g1", Some("研发群"), Liveness::Gone)],
        )];
        let mut snap = build_fleet(&sessions, None::<&RuntimeStatus>, now());
        attach_waiting(
            &mut snap,
            &sessions,
            &[(
                "saturn".to_string(),
                "g1".to_string(),
                mark("om_1", NOW - 30),
            )],
            NOW,
        );
        assert!(snap.groups.is_empty(), "Gone 会话不进群组列表");
        assert_eq!(snap.waiting.len(), 1, "会话退出了,waiting 照样列出");
        assert_eq!(snap.waiting[0].name.as_deref(), Some("研发群"));
    }

    /// 无头会话进程没了(daemon 重启过)就不在会话列表里,它的回复照样等着处理。
    #[test]
    fn exited_headless_session_still_lists_its_waiting_reply() {
        let mut rpc = view("saturn", "g1", Some("研发群"), Liveness::RecentHeadless);
        rpc.capture = CaptureMode::Rpc;
        let sessions = vec![("saturn".to_string(), vec![rpc])];
        let runtime = RuntimeStatus {
            pid: 1,
            version: None,
            uptime_secs: 1,
            busy: Vec::new(),
            credentials: Vec::new(),
            live_headless: Some(Vec::new()),
            subagents: Vec::new(),
        };
        let mut snap = build_fleet(&sessions, Some(&runtime), now());
        attach_waiting(
            &mut snap,
            &sessions,
            &[(
                "saturn".to_string(),
                "g1".to_string(),
                mark("om_1", NOW - 30),
            )],
            NOW,
        );
        assert!(snap.agents[0].chats.is_empty(), "进程没了 = 已退出");
        assert!(snap.groups.is_empty());
        assert_eq!(snap.waiting.len(), 1, "会话退出了,waiting 照样列出");
        assert_eq!(snap.waiting[0].name.as_deref(), Some("研发群"));
        assert_eq!(snap.waiting[0].reply_message_id, "om_1");
    }

    #[test]
    fn waiting_item_without_session_falls_back_to_tag() {
        let mut snap = build_fleet(&[], None::<&RuntimeStatus>, now());
        attach_waiting(
            &mut snap,
            &[],
            &[(
                "saturn".to_string(),
                "p9f8".to_string(),
                mark("om_1", NOW + 5),
            )],
            NOW,
        );
        assert_eq!(snap.waiting[0].name, None);
        assert_eq!(
            snap.waiting[0].chat_type.as_deref(),
            Some("p2p"),
            "类型从 tag 首字母推"
        );
        assert_eq!(snap.waiting[0].since_secs, 0, "时钟回拨钳到 0,不溢出");
    }

    #[test]
    fn group_links_open_the_chat_without_position() {
        let sessions = vec![(
            "saturn".to_string(),
            vec![view("saturn", "g1", Some("研发群"), Liveness::LiveTmux)],
        )];
        let mut snap = build_fleet(&sessions, None::<&RuntimeStatus>, now());
        attach_group_links(&mut snap, "applink.feishu.cn");
        assert_eq!(
            snap.groups[0].link.as_deref(),
            Some("lark://applink.feishu.cn/client/chat/open?openChatId=oc_g1")
        );
    }
}
