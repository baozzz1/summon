//! summon 自己处理的指令的回执。
//!
//! 群里的指令回执挂进那条指令消息的话题(`command_anchor` / `command_anchor_for`),主干上的
//! 指令会因此新开一条。只确认「照做了」的回执不值得为它开一条话题:群里改为给指令消息贴完成
//! 表情(owner 2026-10-21)。私聊没有话题,照常发文字。

use super::agent_env::resolve_messaging;
use crate::frontend::client::FeishuClient;
use crate::frontend::outbound::send_markdown_archived_anchored;
use crate::observe::log::log;
use summon_core::registry::Agent;

/// 一条指令回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandAck {
    /// 只确认指令照做了:内容从指令本身就推得出(复述新值、通用的生效时机)。
    Done(String),
    /// 用户从指令推不出、又需要知道的内容:与请求不同的结果、要留意的后果、错误、用法。
    Note(String),
}

impl CommandAck {
    pub(super) fn text(&self) -> &str {
        match self {
            CommandAck::Done(text) | CommandAck::Note(text) => text,
        }
    }

    /// 换一句文案,类别不变。
    pub(super) fn with_text(&self, text: String) -> Self {
        match self {
            CommandAck::Done(_) => CommandAck::Done(text),
            CommandAck::Note(_) => CommandAck::Note(text),
        }
    }
}

/// 发一条指令回执。`anchor` 是这条指令的话题锚点:群里是指令消息,私聊为 None。
///
/// [`CommandAck::Done`] 在群里只贴完成表情;表情贴不上时退回发文字,用户总能看到结果。
pub(super) fn send_command_ack(
    fc: &FeishuClient,
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    anchor: Option<&str>,
    ack: &CommandAck,
) -> anyhow::Result<()> {
    if let (CommandAck::Done(_), Some(message_id)) = (ack, anchor) {
        match fc.react(message_id, &resolve_messaging(agent).done_reaction) {
            Ok(_) => return Ok(()),
            Err(error) => log(&summon_i18n::format!("r1a98c84f04", agent.name)),
        }
    }
    send_markdown_archived_anchored(
        fc,
        &agent.state_dir(),
        chat_type,
        chat_id,
        anchor,
        ack.text(),
    )
}

/// 回执测试用的假飞书:记下每个非换 token 请求的(路径, 正文);`react_ok` 为 false 时表情接口回 400。
#[cfg(test)]
pub(super) mod ack_feishu {
    use crate::frontend::client::FeishuClient;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub(in crate::runtime::daemon) struct AckFeishu {
        seen: Arc<Mutex<Vec<(String, String)>>>,
        listener: Arc<tiny_http::Server>,
        handle: std::thread::JoinHandle<()>,
        pub(in crate::runtime::daemon) fc: FeishuClient,
    }

    impl AckFeishu {
        pub(in crate::runtime::daemon) fn start(react_ok: bool) -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let seen: Arc<Mutex<Vec<(String, String)>>> = Default::default();
            let recorded = seen.clone();
            let listener = Arc::clone(&server);
            // 调用方同步发完后 `finish` 唤醒监听器收尾,空闲上限只用于失败兜底。
            let handle = std::thread::spawn(move || {
                while let Ok(Some(mut request)) = server.recv_timeout(Duration::from_secs(5)) {
                    let url = request.url().to_string();
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    let (status, response) = if url.contains("tenant_access_token") {
                        (
                            200,
                            r#"{"code":0,"tenant_access_token":"token","expire":7200}"#,
                        )
                    } else if url.ends_with("/reactions") && !react_ok {
                        (400, r#"{"code":231001,"msg":"reaction type is invalid"}"#)
                    } else if url.ends_with("/reactions") {
                        (200, r#"{"code":0,"data":{"reaction_id":"r_1"}}"#)
                    } else {
                        (200, r#"{"code":0,"data":{"message_id":"om_sent"}}"#)
                    };
                    if !url.contains("tenant_access_token") {
                        recorded.lock().unwrap().push((url, body));
                    }
                    request
                        .respond(
                            tiny_http::Response::from_string(response).with_status_code(status),
                        )
                        .unwrap();
                }
            });
            Self {
                seen,
                listener,
                handle,
                fc: FeishuClient::with_test_base_connection_close(base),
            }
        }

        /// 收尾,返回飞书收到的(路径, 正文)。
        pub(in crate::runtime::daemon) fn finish(self) -> Vec<(String, String)> {
            self.listener.unblock();
            self.handle.join().unwrap();
            let seen = self.seen.lock().unwrap().clone();
            seen
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ack_feishu::AckFeishu;
    use super::*;

    /// 发一条回执,返回飞书收到的请求与这个 agent 生效的完成表情。
    fn send(
        react_ok: bool,
        anchor: Option<&str>,
        ack: CommandAck,
    ) -> (Vec<(String, String)>, String) {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let feishu = AckFeishu::start(react_ok);
        send_command_ack(&feishu.fc, &agent, "group", "oc_chat", anchor, &ack).unwrap();
        (feishu.finish(), resolve_messaging(&agent).done_reaction)
    }

    #[test]
    fn group_confirmation_becomes_a_done_reaction_on_the_command() {
        let (seen, done) = send(true, Some("om_cmd"), CommandAck::Done("已关闭".into()));
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (url, body) = &seen[0];
        assert!(url.ends_with("/messages/om_cmd/reactions"), "{url}");
        assert!(body.contains(&format!("\"{done}\"")), "{body}");
    }

    #[test]
    fn group_confirmation_falls_back_to_text_when_the_reaction_fails() {
        let (seen, _) = send(false, Some("om_cmd"), CommandAck::Done("已关闭".into()));
        assert_eq!(seen.len(), 2, "{seen:?}");
        let (url, body) = &seen[1];
        assert!(url.ends_with("/messages/om_cmd/reply"), "{url}");
        assert!(body.contains("已关闭"), "{body}");
    }

    #[test]
    fn note_still_replies_in_the_command_thread() {
        let (seen, _) = send(true, Some("om_cmd"), CommandAck::Note("改失败:x".into()));
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (url, body) = &seen[0];
        assert!(url.ends_with("/messages/om_cmd/reply"), "{url}");
        assert!(body.contains("改失败"), "{body}");
    }

    #[test]
    fn private_confirmation_is_sent_as_text() {
        let (seen, _) = send(true, None, CommandAck::Done("已关闭".into()));
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (url, body) = &seen[0];
        assert!(!url.contains("/reactions"), "{url}");
        assert!(body.contains("已关闭"), "{body}");
    }

    #[test]
    fn with_text_keeps_the_kind() {
        assert_eq!(
            CommandAck::Done("a".into()).with_text("b".into()),
            CommandAck::Done("b".into())
        );
        assert_eq!(
            CommandAck::Note("a".into()).with_text("b".into()),
            CommandAck::Note("b".into())
        );
    }
}
