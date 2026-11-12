//! 每 agent 一条有界验证队列，免 @ 成员 HTTP 与 dispatcher 分离。

use super::{Agent, DispatchMsg, FeishuClient, Inbound};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Sender, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const QUEUE_CAPACITY: usize = 16;
/// 从验证完成起算:查询耗时不占有效期,只约束结论在 dispatcher 里排队多久。
const PROOF_TTL: Duration = Duration::from_secs(30);

pub(super) fn candidate(agent: &Agent, msg: &Inbound) -> bool {
    !agent.is_steward()
        && matches!(msg.chat_type.as_deref(), Some("group" | "topic_group"))
        && agent.replies_without_mention(&msg.chat_id)
        && msg.sender_type == "user"
        && !msg.sender.is_empty()
        && agent.allows(&msg.sender, &msg.sender_union_id)
        && msg.mentions.is_empty()
}

pub(crate) struct Checked {
    msg: Inbound,
    manifest_mtime: Option<SystemTime>,
    /// 验证完成时刻,或验证不通过的原因。
    proof: Result<Instant, String>,
}

/// 免 @ 消息回到 dispatcher 后不放行的原因,进日志供排查。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Rejection {
    Unverified(String),
    PolicyChanged,
    Expired,
    NotCandidate,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unverified(reason) => write!(f, "{}", summon_i18n::format!("r312a0a863e")),
            Self::PolicyChanged => f.write_str(summon_i18n::text!("r4506f242b9")),
            Self::Expired => write!(
                f,
                "{}",
                summon_i18n::format!("rdb02499607", PROOF_TTL.as_secs())
            ),
            Self::NotCandidate => f.write_str(summon_i18n::text!("rd99e455e85")),
        }
    }
}

impl Checked {
    /// 按 dispatcher 当前的 agent 与 agent.yaml 代际重判,交回消息与放行结论。
    pub(super) fn into_verdict(
        self,
        agent: &Agent,
        manifest_mtime: Option<SystemTime>,
    ) -> (Inbound, Result<(), Rejection>) {
        let verdict = match &self.proof {
            Err(reason) => Err(Rejection::Unverified(reason.clone())),
            Ok(_) if self.manifest_mtime.is_none() || self.manifest_mtime != manifest_mtime => {
                Err(Rejection::PolicyChanged)
            }
            Ok(verified_at) if verified_at.elapsed() > PROOF_TTL => Err(Rejection::Expired),
            Ok(_) if !candidate(agent, &self.msg) => Err(Rejection::NotCandidate),
            Ok(_) => Ok(()),
        };
        (self.msg, verdict)
    }
}

struct Request {
    msg: Inbound,
    manifest_mtime: Option<SystemTime>,
}

pub(super) struct Queue {
    tx: SyncSender<Request>,
    stopped: Arc<AtomicBool>,
}

impl Queue {
    pub fn new(fc: Arc<FeishuClient>, dispatch: Sender<DispatchMsg>) -> Self {
        Self::with_verifier(dispatch, move |msg| {
            fc.prove_mentionless_chat(&msg.chat_id, &msg.sender)
        })
    }

    fn with_verifier(
        dispatch: Sender<DispatchMsg>,
        verify: impl Fn(&Inbound) -> anyhow::Result<()> + Send + 'static,
    ) -> Self {
        let (tx, rx) = sync_channel::<Request>(QUEUE_CAPACITY);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        std::thread::spawn(move || {
            while let Ok(request) = rx.recv() {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let proof = verify(&request.msg)
                    .map(|()| Instant::now())
                    .map_err(|error| format!("{error:#}"));
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let checked = Checked {
                    msg: request.msg,
                    manifest_mtime: request.manifest_mtime,
                    proof,
                };
                if dispatch
                    .send(DispatchMsg::MentionlessChecked(checked))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self { tx, stopped }
    }

    pub fn submit(&self, agent: &Agent, msg: Inbound, mtime: Option<SystemTime>) -> bool {
        if !candidate(agent, &msg) {
            return false;
        }
        self.tx
            .try_send(Request {
                msg,
                manifest_mtime: mtime,
            })
            .is_ok()
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn agent() -> Agent {
        crate::test_support::agent_with("claude", "feishu:\n  allowed_users: [ou_owner]\n  chat_reply_without_mention: {oc_target: true}\n")
    }

    fn message() -> Inbound {
        serde_json::from_value(serde_json::json!({
            "chat_id":"oc_target", "chat_type":"group", "text":"hello", "sender":"ou_owner",
            "sender_union_id":"", "sender_type":"user", "message_id":"om_test", "parent_id":null,
            "thread_id":"omt_thread", "root_id":null, "mentions":[],
            "resources":[{"file_key":"image_key", "kind":"image", "name":"image"}]
        }))
        .unwrap()
    }

    #[test]
    fn mentionless_candidates_require_a_configured_human_conversation() {
        let agent = agent();
        let original = message();
        assert!(candidate(&agent, &original));
        for (field, value) in [
            ("chat_id", "oc_other"),
            ("chat_type", "p2p"),
            ("sender_type", "bot"),
            ("sender_type", "app"),
            ("sender_type", ""),
            ("sender", "ou_other"),
            ("sender", ""),
        ] {
            let mut json = serde_json::to_value(&original).unwrap();
            json[field] = value.into();
            let msg = serde_json::from_value(json).unwrap();
            assert!(!candidate(&agent, &msg), "{field}={value}");
        }
        // 斜杠命令与以 `/` 开头的普通内容同样经验证免 @。
        for text in [
            " /reset",
            "/model x",
            "/queue hi",
            "/Users/me/err.log 这个报错",
        ] {
            let mut msg = original.clone();
            msg.text = text.into();
            assert!(candidate(&agent, &msg), "{text}");
        }
        let mut msg = original;
        msg.mentions.push(summon_core::ipc::Mention {
            open_id: "ou_other".into(),
            ..Default::default()
        });
        assert!(!candidate(&agent, &msg));
    }

    #[test]
    fn mentionless_proofs_expire_and_bind_to_current_policy() {
        let mtime = Some(SystemTime::now());
        let verdict = |agent: &Agent, proof: Result<Instant, String>, current| {
            let checked = Checked {
                msg: message(),
                manifest_mtime: mtime,
                proof,
            };
            let (msg, verdict) = checked.into_verdict(agent, current);
            assert_eq!(msg, message());
            verdict
        };
        let agent = agent();
        assert_eq!(verdict(&agent, Ok(Instant::now()), mtime), Ok(()));
        assert_eq!(
            verdict(&agent, Ok(Instant::now()), None),
            Err(Rejection::PolicyChanged)
        );
        assert_eq!(
            verdict(
                &agent,
                Ok(Instant::now()),
                mtime.map(|t| t + Duration::from_secs(1))
            ),
            Err(Rejection::PolicyChanged)
        );
        let expired = Instant::now() - PROOF_TTL - Duration::from_millis(1);
        assert_eq!(verdict(&agent, Ok(expired), mtime), Err(Rejection::Expired));
        let unverified = verdict(
            &agent,
            Err("目标群不满足一人一 Bot: bot_count=2".into()),
            mtime,
        );
        assert_eq!(
            unverified,
            Err(Rejection::Unverified(
                "目标群不满足一人一 Bot: bot_count=2".into()
            ))
        );
        assert!(unverified.unwrap_err().to_string().contains("bot_count=2"));

        let mut disabled = self::agent();
        disabled
            .feishu
            .as_mut()
            .unwrap()
            .chat_reply_without_mention
            .clear();
        assert_eq!(
            verdict(&disabled, Ok(Instant::now()), mtime),
            Err(Rejection::NotCandidate)
        );
        let mut revoked = self::agent();
        revoked.feishu.as_mut().unwrap().allowed_users = vec!["ou_other".into()];
        assert_eq!(
            verdict(&revoked, Ok(Instant::now()), mtime),
            Err(Rejection::NotCandidate)
        );
    }

    /// 有效期从查询返回后起算:慢查询本身不能把通过的结论耗成过期。
    #[test]
    fn mentionless_queue_stamps_proofs_after_verification_and_keeps_failure_reasons() {
        let (dispatch, rx) = channel();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel::<anyhow::Result<()>>();
        let queue = Queue::with_verifier(dispatch, move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap()
        });
        let agent = agent();
        let mtime = Some(SystemTime::now());
        assert!(queue.submit(&agent, message(), mtime));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let released = Instant::now();
        release_tx.send(Ok(())).unwrap();
        let DispatchMsg::MentionlessChecked(checked) =
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected result")
        };
        assert!(*checked.proof.as_ref().unwrap() >= released);

        let mut failing = message();
        failing.message_id = "om_failing".into();
        assert!(queue.submit(&agent, failing, mtime));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        release_tx
            .send(Err(anyhow::anyhow!("目标群不满足一人一 Bot: bot_count=2")))
            .unwrap();
        let DispatchMsg::MentionlessChecked(checked) =
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected result")
        };
        let (_, verdict) = checked.into_verdict(&agent, mtime);
        let reason = verdict.unwrap_err().to_string();
        assert!(reason.contains("bot_count=2"), "{reason}");
    }

    #[test]
    fn mentionless_queue_preserves_candidate_order_and_payloads() {
        let (dispatch, rx) = channel();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let queue = Queue::with_verifier(dispatch, move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        });
        let agent = agent();
        let first = message();
        let mut second = first.clone();
        second.message_id = "om_second".into();
        second.text = "second message".into();
        assert!(queue.submit(&agent, first.clone(), Some(SystemTime::now())));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(queue.submit(&agent, second.clone(), Some(SystemTime::now())));
        assert!(rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        for expected in [first, second] {
            let DispatchMsg::MentionlessChecked(checked) =
                rx.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("expected result")
            };
            assert_eq!(checked.msg, expected);
            assert!(checked.proof.is_ok());
        }
        assert!(started_rx.try_recv().is_ok());
        assert!(started_rx.try_recv().is_err());
    }

    #[test]
    fn mentionless_queue_is_bounded_and_stops_without_dispatching() {
        let (dispatch, rx) = channel();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let queue = Queue::with_verifier(dispatch, move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        });
        let agent = agent();
        assert!(queue.submit(&agent, message(), None));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..QUEUE_CAPACITY {
            assert!(queue.submit(&agent, message(), None));
        }
        assert!(!queue.submit(&agent, message(), None));
        drop(queue);
        release_tx.send(()).unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_err());
        assert!(started_rx.try_recv().is_err());
    }

    fn production_dispatcher_case(saturated: bool) {
        use super::super::{dispatcher, FeishuEpochIdentity};
        use crate::runtime::{agent_availability::AgentAvailability, inbox_wal::InboxWal};
        use std::collections::HashMap;

        let (agent, root) = crate::test_support::agent_dir("claude", "feishu:\n  allowed_users: [ou_owner]\n  chat_reply_without_mention: {oc_target: true}\nmessaging:\n  session_mirror: false\n");
        let inbox = InboxWal::new(root.path().join("inbox"));
        AgentAvailability::lock(&root.path().join("inbox"), &agent.name)
            .unwrap()
            .set_enabled(false)
            .unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (query_started, query_started_rx) = channel();
        let (release_query, release_query_rx) = channel();
        let http = std::thread::spawn(move || {
            let responses = if saturated {
                vec![
                    r#"{"code":0,"tenant_access_token":"t","expire":7200}"#,
                    r#"{"code":0,"bot":{"open_id":"ou_bot","app_name":"test"}}"#,
                    r#"{"code":232011,"msg":"operator not in chat"}"#,
                ]
            } else {
                vec![
                    r#"{"code":0,"tenant_access_token":"t","expire":7200}"#,
                    r#"{"code":0,"bot":{"open_id":"ou_bot","app_name":"test"}}"#,
                    r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":"1"}}"#,
                    r#"{"code":0,"data":{"items":[{"member_id_type":"open_id","member_id":"ou_owner"}],"has_more":false}}"#,
                    r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":"2"}}"#,
                    r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":"1"}}"#,
                    r#"{"code":0,"data":{"items":[{"member_id_type":"open_id","member_id":"ou_owner"}],"has_more":false}}"#,
                    r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":"1"}}"#,
                    r#"{"code":0,"data":{"items":[{"member_id_type":"open_id","member_id":"ou_owner"}],"has_more":false}}"#,
                ]
            };
            let mut urls = Vec::new();
            for (index, response) in responses.into_iter().enumerate() {
                let request = server
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .expect("路由应查询对应 HTTP");
                urls.push(request.url().to_string());
                if saturated && index == 2 {
                    query_started.send(()).unwrap();
                    release_query_rx
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
            urls
        });
        let stores = dispatcher::DispatcherStores {
            bridge: crate::bridge::store::BridgeStore::new(root.path().join("bridge")),
            outbox: crate::runtime::outbox_store::OutboxStore::new(root.path().join("outbox")),
            inbox: InboxWal::new(root.path().join("inbox")),
            card_actions: crate::runtime::card_action_wal::CardActionWal::new(
                root.path().join("card-wal"),
            ),
            support_root: Some(root.path().join("support")),
        };
        let (tx, rx) = channel();
        let dispatcher_thread = {
            let agent = agent.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let epoch = FeishuEpochIdentity::capture(&agent);
                dispatcher::agent_dispatcher_with_stores(
                    agent,
                    20,
                    20,
                    Arc::new(FeishuClient::with_test_base(base)),
                    epoch,
                    rx,
                    tx,
                    Arc::new(crate::runtime::core::Core::for_routes(HashMap::new())),
                    crate::runtime::inflight::InFlight::new(),
                    stores,
                );
            })
        };
        let wait_consumed = |id: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while inbox.read_consumed(&agent.name, id).unwrap().is_none() {
                assert!(Instant::now() < deadline, "没有通过路由: {id}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        if saturated {
            tx.send(DispatchMsg::Inbound(message())).unwrap();
            query_started_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            for index in 0..=QUEUE_CAPACITY {
                let mut queued = message();
                queued.message_id = format!("om_queued_{index}");
                tx.send(DispatchMsg::Inbound(queued)).unwrap();
            }
            for (id, text) in [
                ("om_explicit", "@_user_1 hello"),
                ("om_control", "@_user_1 /reset"),
            ] {
                let mut msg = message();
                msg.message_id = id.into();
                msg.text = text.into();
                msg.mentions.push(summon_core::ipc::Mention {
                    open_id: "ou_bot".into(),
                    key: "@_user_1".into(),
                    ..Default::default()
                });
                tx.send(DispatchMsg::Inbound(msg)).unwrap();
                wait_consumed(id);
            }
            tx.send(DispatchMsg::Shutdown).unwrap();
            dispatcher_thread.join().unwrap();
            release_query.send(()).unwrap();
            assert_eq!(http.join().unwrap().len(), 3);
            assert!(inbox
                .read_consumed(&agent.name, "om_test")
                .unwrap()
                .is_none());
            assert!(!agent.state_dir().join("threads").exists());
            return;
        }
        let first = message();
        tx.send(DispatchMsg::Inbound(first.clone())).unwrap();
        wait_consumed(&first.message_id);
        tx.send(DispatchMsg::Inbound(first)).unwrap();
        let mut failed = message();
        failed.message_id = "om_multi_bot".into();
        tx.send(DispatchMsg::Inbound(failed)).unwrap();
        let mut slash = message();
        slash.message_id = "om_slash".into();
        slash.text = "/reset".into();
        tx.send(DispatchMsg::Inbound(slash)).unwrap();
        let mut explicit = message();
        explicit.message_id = "om_explicit".into();
        explicit.text = "@_user_1 hello".into();
        explicit.mentions.push(summon_core::ipc::Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            ..Default::default()
        });
        tx.send(DispatchMsg::Inbound(explicit)).unwrap();
        wait_consumed("om_explicit");
        // 验证队列与 dispatcher 通道都按序:后一条免 @ 消息被放行时,多 Bot 那条的结论已处理完。
        let mut after = message();
        after.message_id = "om_after_multi_bot".into();
        tx.send(DispatchMsg::Inbound(after)).unwrap();
        wait_consumed("om_after_multi_bot");
        let urls = http.join().unwrap();
        tx.send(DispatchMsg::Shutdown).unwrap();
        dispatcher_thread.join().unwrap();
        assert_eq!(urls.len(), 9, "重复与显式 @ 不应额外查成员");
        assert!(inbox
            .read_consumed(&agent.name, "om_multi_bot")
            .unwrap()
            .is_none());
        assert!(
            inbox
                .read_consumed(&agent.name, "om_slash")
                .unwrap()
                .is_some(),
            "斜杠命令经验证后免 @ 放行"
        );
        assert!(inbox.scan_pending().is_empty());
        assert!(
            !agent.state_dir().join("threads").exists(),
            "验证不应启动会话或下载附件"
        );
    }

    #[test]
    fn mentionless_production_dispatcher_applies_topology_and_keeps_explicit_mentions() {
        production_dispatcher_case(false);
    }

    #[test]
    fn mentionless_saturated_dispatcher_keeps_explicit_messages_and_control_commands() {
        production_dispatcher_case(true);
    }

    /// 起一个真实 dispatcher,群里只有 ou_owner 和本 bot;返回发往群里的消息正文。
    fn dispatch_unmentioned(feishu_yaml: &str, text: &str, expect: &str) -> Vec<String> {
        use super::super::{dispatcher, FeishuEpochIdentity};
        use std::collections::HashMap;

        let (agent, root) = crate::test_support::agent_dir(
            "codex",
            &format!("feishu:\n{feishu_yaml}  chat_reply_without_mention: {{oc_target: true}}\n"),
        );
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (stop, stopped) = channel::<()>();
        let (sent_tx, sent) = channel::<String>();
        let http = std::thread::spawn(move || {
            while stopped.try_recv().is_err() {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap()
                else {
                    continue;
                };
                let url = request.url().to_string();
                let response = if url.contains("tenant_access_token") {
                    r#"{"code":0,"tenant_access_token":"t","expire":7200}"#.to_string()
                } else if url.contains("bot/v3/info") {
                    r#"{"code":0,"bot":{"open_id":"ou_bot","app_name":"test"}}"#.to_string()
                } else if url.contains("/chats/oc_target/members") {
                    r#"{"code":0,"data":{"items":[{"member_id_type":"open_id","member_id":"ou_owner"}],"has_more":false}}"#.to_string()
                } else if url.contains("/chats/oc_target") {
                    r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":"1"}}"#
                        .to_string()
                } else if url.contains("/messages") && !url.contains("/reactions") {
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    let _ = sent_tx.send(body);
                    r#"{"code":0,"data":{"message_id":"om_reply"}}"#.to_string()
                } else {
                    r#"{"code":0,"data":{}}"#.to_string()
                };
                let _ = request.respond(tiny_http::Response::from_string(response));
            }
        });
        let stores = dispatcher::DispatcherStores {
            bridge: crate::bridge::store::BridgeStore::new(root.path().join("bridge")),
            outbox: crate::runtime::outbox_store::OutboxStore::new(root.path().join("outbox")),
            inbox: crate::runtime::inbox_wal::InboxWal::new(root.path().join("inbox")),
            card_actions: crate::runtime::card_action_wal::CardActionWal::new(
                root.path().join("card-wal"),
            ),
            support_root: Some(root.path().join("support")),
        };
        let (tx, rx) = channel();
        let dispatcher_thread = {
            let agent = agent.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let epoch = FeishuEpochIdentity::capture(&agent);
                dispatcher::agent_dispatcher_with_stores(
                    agent,
                    20,
                    20,
                    Arc::new(FeishuClient::with_test_base(base)),
                    epoch,
                    rx,
                    tx,
                    Arc::new(crate::runtime::core::Core::for_routes(HashMap::new())),
                    crate::runtime::inflight::InFlight::new(),
                    stores,
                );
            })
        };
        let mut msg = message();
        msg.text = text.into();
        msg.resources.clear();
        tx.send(DispatchMsg::Inbound(msg)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bodies = Vec::new();
        while !bodies.iter().any(|body: &String| body.contains(expect)) {
            let left = deadline.saturating_duration_since(Instant::now());
            match sent.recv_timeout(left) {
                Ok(body) => bodies.push(body),
                Err(_) => break,
            }
        }
        tx.send(DispatchMsg::Shutdown).unwrap();
        dispatcher_thread.join().unwrap();
        stop.send(()).unwrap();
        http.join().unwrap();
        bodies
    }

    /// 已开启且验证通过的群里,不带 @ 的 `/bridge` 进入 Bridge 发起分支:能对话、不能操作的人
    /// 由它自己的授权闸拒绝。整条路径停在 dispatcher 内,不起 chat 线程。
    #[test]
    fn mentionless_bridge_command_reaches_its_handler_without_mention() {
        let refused = "仅授权用户可发起 Bridge";
        let bodies = dispatch_unmentioned(
            "  allowed_users: [ou_owner]\n  operators: [ou_admin]\n",
            "/bridge",
            refused,
        );
        assert!(
            bodies.iter().any(|body| body.contains(refused)),
            "{bodies:?}"
        );
    }
}
