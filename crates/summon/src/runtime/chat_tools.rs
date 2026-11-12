//! 轮内聊天工具的共享业务入口。CLI 和 Codex dynamic tools 只负责各自的身份适配，
//! 表情、图片校验、飞书调用与审计在这里保持单一实现。

use anyhow::Context as _;
use summon_core::registry::Agent;

use crate::frontend::client::FeishuClient;

/// 字段私有,唯一构造路径是 [`ChatToolTarget::new`] —— 跨模块拼不出字面量。
///
/// 凭证复验由调用方负责:`react` / `send_image` 会直接打飞书,而「验过没有」在签名上看不出来。
/// 守它的是行为判据 —— `dynamic_tool_entrypoint_touches_nothing_without_an_active_turn` 断言
/// 凭证失效时飞书侧一个请求都没收到。
pub(crate) struct ChatToolTarget<'a> {
    agent: &'a Agent,
    chat_id: &'a str,
    chat_type: &'a str,
    message_id: &'a str,
}

/// 「这一轮跑在 codex 的 app-server 通道上」的见证。
///
/// 字段与构造器都私有,且**定义在本模块**:想拿到它只能走 [`codex_rpc_channel`],
/// 那是唯一写着通道判据的地方。别的模块即便能看见类型也拼不出值,所以「持有见证」
/// 等价于「判据过了」。
#[derive(Clone, Copy, Debug)]
pub(crate) struct CodexRpcChannel(());

/// codex 平台工具与审批的通道判据。它们只存在于 app-server 协议里,挂到别的通道上等于把
/// 飞书副作用接到一个不会发这些请求的连接。
///
/// traex 认同一套协议(同 `codex_and_traex_can_fork_because_they_share_the_app_server_protocol`),
/// 所以两者一起放行 —— 少认一个的后果不是更安全,而是它的会话照样注册工具、调用却拿不到处理器。
pub(crate) fn codex_rpc_channel(agent: &Agent) -> Option<CodexRpcChannel> {
    let app_server = matches!(
        summon_core::driver::CliDriver::from_kind(agent.cli_kind()),
        summon_core::driver::CliDriver::Codex | summon_core::driver::CliDriver::Traex
    );
    (app_server && agent.capture_mode() == summon_core::registry::CaptureMode::Rpc)
        .then_some(CodexRpcChannel(()))
}

impl<'a> ChatToolTarget<'a> {
    pub(crate) fn new(
        agent: &'a Agent,
        chat_id: &'a str,
        chat_type: &'a str,
        message_id: &'a str,
    ) -> Self {
        ChatToolTarget {
            agent,
            chat_id,
            chat_type,
            message_id,
        }
    }
}

pub(crate) fn chat_owned_by_agent(target: &ChatToolTarget<'_>) -> bool {
    let dir = target.agent.state_dir().join("runtime").join("chats");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("meta") {
            return false;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return false;
        };
        let meta = summon_core::chat_meta::ChatMeta::parse(&content);
        meta.chat_id.as_deref() == Some(target.chat_id)
            && meta.chat_type.as_deref().unwrap_or("group") == target.chat_type
    })
}

pub(crate) fn react(
    target: &ChatToolTarget<'_>,
    fc: &FeishuClient,
    emoji: Option<&str>,
    clear: bool,
) -> anyhow::Result<String> {
    let store = crate::runtime::reaction_store::ReactionStore::for_agent(&target.agent.state_dir());
    let Some(mut rec) = store.find_by_message(target.message_id) else {
        return Ok(summon_i18n::text!("rc73b92afbb").into());
    };
    anyhow::ensure!(
        rec.chat_id == target.chat_id && rec.chat_type == target.chat_type,
        summon_i18n::format!("r01ed6b3d7c")
    );
    let messaging = crate::runtime::daemon::resolve_messaging(target.agent);
    let allowed = &messaging.reactions;
    let working_in_flight =
        (!rec.onit_reaction_id.is_empty()).then_some(messaging.working_reaction.as_str());
    let plan = summon_core::reactions::plan_agent_react(
        rec.agent_reaction_id.as_deref(),
        emoji,
        clear,
        allowed,
        working_in_flight,
    );
    match &plan {
        summon_core::reactions::ReactPlan::Reject { emoji } => {
            anyhow::bail!(summon_i18n::format!("r05eb8f1b37", allowed.join(" ")))
        }
        summon_core::reactions::ReactPlan::RejectWorkingCollision { emoji } => {
            anyhow::bail!(summon_i18n::format!("rcfe2701d6e"))
        }
        _ => {}
    }

    let unreact_old = |old: &str, rec: &crate::runtime::reaction_store::ReactionRecord| {
        if old != rec.onit_reaction_id {
            if let Err(error) = fc.unreact(rec.anchor(), old) {
                crate::observe::log::log(&summon_i18n::format!("r2c31815ed5"));
            }
        }
    };
    match plan {
        summon_core::reactions::ReactPlan::Reject { .. }
        | summon_core::reactions::ReactPlan::RejectWorkingCollision { .. } => unreachable!(),
        summon_core::reactions::ReactPlan::Clear { unreact } => {
            if let Some(old) = unreact {
                unreact_old(&old, &rec);
            }
            rec.agent_reaction_id = None;
            store.enqueue(&rec);
            Ok(summon_i18n::text!("r08c424b8b1").into())
        }
        summon_core::reactions::ReactPlan::Set { unreact, emoji } => {
            if let Some(old) = unreact {
                unreact_old(&old, &rec);
            }
            let reaction_id = fc
                .react(rec.anchor(), &emoji)
                .with_context(|| summon_i18n::format!("r17d5508c59", rec.message_id))?;
            rec.agent_reaction_id = Some(reaction_id);
            store.enqueue(&rec);
            Ok(summon_i18n::format!("r1ba21511f6"))
        }
    }
}

pub(crate) fn send_image(
    target: &ChatToolTarget<'_>,
    fc: &FeishuClient,
    path: &str,
    caption: Option<&str>,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        chat_owned_by_agent(target),
        summon_i18n::format!("rce30044829")
    );
    let file = std::path::Path::new(path);
    anyhow::ensure!(file.is_absolute(), summon_i18n::format!("re7abc248b0"));
    let link_metadata =
        std::fs::symlink_metadata(file).with_context(|| summon_i18n::format!("r26306e03d9"))?;
    anyhow::ensure!(
        !link_metadata.file_type().is_symlink(),
        summon_i18n::format!("r6eb72d1090")
    );
    let metadata = std::fs::metadata(file).with_context(|| summon_i18n::format!("r26306e03d9"))?;
    if !metadata.is_file() {
        anyhow::bail!(summon_i18n::format!("r3418abbf80"));
    }
    let extension = file.extension().and_then(|value| value.to_str());
    if let Err(error) = summon_core::image::validate(extension, metadata.len()) {
        anyhow::bail!("{error}");
    }
    let bytes = std::fs::read(file).with_context(|| summon_i18n::format!("rd948facbdb"))?;
    let filename = file
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("image.png");
    let image_key = fc.upload_image(bytes, filename)?;
    let message_id = fc.send_image(target.chat_id, &image_key)?;
    crate::frontend::outbound::append_outbound(
        &target.agent.state_dir(),
        target.chat_type,
        target.chat_id,
        "image",
        serde_json::json!({
            "message_id": message_id,
            "image_key": image_key,
            "path": path,
        }),
    );
    if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
        crate::frontend::outbound::send_markdown_archived(
            fc,
            &target.agent.state_dir(),
            target.chat_type,
            target.chat_id,
            caption,
        )?;
    }
    Ok(summon_i18n::format!("r2638ba7a98"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn agent_with_chat(chat_id: &str, chat_type: &str) -> (tempfile::TempDir, Agent) {
        let temp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str("name: a").unwrap();
        agent.dir = temp.path().join("summon").join("agents").join("a");
        std::fs::create_dir_all(&agent.dir).unwrap();
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            chats.join("soul-a.meta"),
            format!("chat_id={chat_id}\nchat_type={chat_type}\ntag=test\n"),
        )
        .unwrap();
        (temp, agent)
    }

    fn enqueue_reaction(agent: &Agent, chat_id: &str, chat_type: &str, message_id: &str) {
        crate::runtime::reaction_store::ReactionStore::for_agent(&agent.state_dir()).enqueue(
            &crate::runtime::reaction_store::ReactionRecord {
                message_id: message_id.into(),
                anchor_message_id: None,
                delivered_at_ms: 1,
                onit_reaction_id: String::new(),
                chat_id: chat_id.into(),
                chat_type: chat_type.into(),
                agent_reaction_id: None,
            },
        );
    }

    #[test]
    fn chat_ownership_requires_matching_agent_chat_and_type() {
        let (_temp, agent) = agent_with_chat("oc_mine", "group");
        let target = |chat_id, chat_type| ChatToolTarget::new(&agent, chat_id, chat_type, "om_1");
        assert!(chat_owned_by_agent(&target("oc_mine", "group")));
        assert!(!chat_owned_by_agent(&target("oc_other", "group")));
        assert!(!chat_owned_by_agent(&target("oc_mine", "p2p")));
    }

    #[test]
    fn react_uses_the_exact_bound_message_and_persists_the_reaction_id() {
        let (_temp, agent) = agent_with_chat("oc_mine", "group");
        enqueue_reaction(&agent, "oc_mine", "group", "om_source");
        let target = ChatToolTarget::new(&agent, "oc_mine", "group", "om_source");
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server_seen = Arc::clone(&seen);
        let server_thread = std::thread::spawn(move || {
            for index in 0..2 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("token 和 reaction 请求都应到达");
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                server_seen
                    .lock()
                    .unwrap()
                    .push((request.url().to_string(), body));
                let response = if index == 0 {
                    r#"{"code":0,"tenant_access_token":"token","expire":7200}"#
                } else {
                    r#"{"code":0,"data":{"reaction_id":"rid_semantic"}}"#
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
        });

        let client = FeishuClient::with_test_base(base);
        assert_eq!(
            react(&target, &client, Some("OK"), false).unwrap(),
            "已贴语义表情 OK"
        );
        server_thread.join().unwrap();

        let seen = seen.lock().unwrap();
        assert!(seen[1].0.contains("/messages/om_source/reactions"));
        assert!(seen[1].1.contains("\"emoji_type\":\"OK\""));
        let record = crate::runtime::reaction_store::ReactionStore::for_agent(&agent.state_dir())
            .find_by_message("om_source")
            .unwrap();
        assert_eq!(record.agent_reaction_id.as_deref(), Some("rid_semantic"));
    }

    #[test]
    fn react_rejects_a_cross_chat_record_before_network_access() {
        let (_temp, agent) = agent_with_chat("oc_mine", "group");
        enqueue_reaction(&agent, "oc_other", "group", "om_source");
        let target = ChatToolTarget::new(&agent, "oc_mine", "group", "om_source");
        let stored = crate::runtime::reaction_store::ReactionStore::for_agent(&agent.state_dir())
            .find_by_message("om_source")
            .unwrap();
        assert_eq!(stored.chat_id, "oc_other");
        let client = FeishuClient::with_test_base("http://127.0.0.1:1".into());
        let error = react(&target, &client, Some("OK"), false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("归属与活跃轮次不一致"), "{error}");
    }

    #[test]
    fn send_image_rejects_unowned_chat_before_file_or_network_access() {
        let (_temp, agent) = agent_with_chat("oc_mine", "group");
        let target = ChatToolTarget::new(&agent, "oc_other", "group", "om_1");
        let client = FeishuClient::with_test_base("http://127.0.0.1:1".into());
        let error = send_image(&target, &client, "/does/not/exist.png", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("不在该 agent 名下"), "{error}");
    }

    #[test]
    fn send_image_rejects_relative_and_symlink_paths_before_network_access() {
        let (_temp, agent) = agent_with_chat("oc_mine", "group");
        let target = ChatToolTarget::new(&agent, "oc_mine", "group", "om_1");
        let client = FeishuClient::with_test_base("http://127.0.0.1:1".into());
        let relative = send_image(&target, &client, "image.png", None)
            .unwrap_err()
            .to_string();
        assert!(relative.contains("绝对路径"), "{relative}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let real = agent.dir.join("real.png");
            let link = agent.dir.join("linked.png");
            std::fs::write(&real, b"png-probe").unwrap();
            symlink(&real, &link).unwrap();
            let error = send_image(&target, &client, link.to_str().unwrap(), None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("符号链接"), "{error}");
        }
    }

    #[test]
    fn send_image_rejects_directory_unsupported_format_and_oversize_locally() {
        let (_temp, agent) = agent_with_chat("oc_mine", "group");
        let target = ChatToolTarget::new(&agent, "oc_mine", "group", "om_1");
        let client = FeishuClient::with_test_base("http://127.0.0.1:1".into());

        let directory = send_image(&target, &client, agent.dir.to_str().unwrap(), None)
            .unwrap_err()
            .to_string();
        assert!(directory.contains("不是一个文件"), "{directory}");

        let text = agent.dir.join("not-image.txt");
        std::fs::write(&text, b"not an image").unwrap();
        let unsupported = send_image(&target, &client, text.to_str().unwrap(), None)
            .unwrap_err()
            .to_string();
        assert!(unsupported.contains("不是支持的图片格式"), "{unsupported}");

        let oversized = agent.dir.join("oversized.png");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(summon_core::image::MAX_IMAGE_BYTES + 1)
            .unwrap();
        let too_large = send_image(&target, &client, oversized.to_str().unwrap(), None)
            .unwrap_err()
            .to_string();
        assert!(too_large.contains("超过飞书上限"), "{too_large}");
    }

    #[test]
    fn send_image_uploads_to_bound_chat_and_archives_success() {
        let (_temp, agent) = agent_with_chat("oc_mine", "group");
        let image = agent.dir.join("probe.png");
        std::fs::write(&image, b"png-probe").unwrap();
        let target = ChatToolTarget::new(&agent, "oc_mine", "group", "om_source");

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server_seen = Arc::clone(&seen);
        let server_thread = std::thread::spawn(move || {
            for index in 0..4 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("token、upload、send、caption 四次请求都应到达");
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                server_seen
                    .lock()
                    .unwrap()
                    .push((request.url().to_string(), body));
                let response = match index {
                    0 => r#"{"code":0,"tenant_access_token":"token","expire":7200}"#,
                    1 => r#"{"code":0,"data":{"image_key":"img_probe"}}"#,
                    2 => r#"{"code":0,"data":{"message_id":"om_image"}}"#,
                    _ => r#"{"code":0,"data":{"message_id":"om_caption"}}"#,
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
        });

        let client = FeishuClient::with_test_base(base);
        assert_eq!(
            send_image(&target, &client, image.to_str().unwrap(), Some("图片说明"),).unwrap(),
            "已发送图片 probe.png"
        );
        server_thread.join().unwrap();

        let seen = seen.lock().unwrap();
        assert!(seen[1].0.ends_with("/im/v1/images"), "{:?}", seen[1]);
        assert!(
            seen[2]
                .0
                .ends_with("/im/v1/messages?receive_id_type=chat_id"),
            "{:?}",
            seen[2]
        );
        let send_body: serde_json::Value = serde_json::from_str(&seen[2].1).unwrap();
        assert_eq!(send_body["receive_id"], "oc_mine");
        assert_eq!(send_body["msg_type"], "image");
        assert!(seen[3].1.contains("图片说明"), "{:?}", seen[3]);

        let outbound = std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(summon_core::registry::chat_tag("group", "oc_mine"))
                .join("outbound.ndjson"),
        )
        .unwrap();
        let entry: serde_json::Value = outbound
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .find(|entry: &serde_json::Value| entry["kind"] == "image")
            .expect("成功发送必须追加 image outbound 记录");
        assert_eq!(entry["kind"], "image");
        assert_eq!(entry["payload"]["message_id"], "om_image");
        assert_eq!(entry["payload"]["image_key"], "img_probe");
        assert!(outbound
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|entry| {
                entry["kind"] == "markdown_card" && entry["payload"]["markdown"] == "图片说明"
            }));
    }

    #[test]
    fn send_image_does_not_archive_failed_upload_or_failed_send() {
        fn outbound_path(agent: &Agent) -> std::path::PathBuf {
            agent
                .state_dir()
                .join("threads")
                .join(summon_core::registry::chat_tag("group", "oc_mine"))
                .join("outbound.ndjson")
        }

        for fail_at_send in [false, true] {
            let (_temp, agent) = agent_with_chat("oc_mine", "group");
            let image = agent.dir.join("probe.png");
            std::fs::write(&image, b"png-probe").unwrap();
            let target = ChatToolTarget::new(&agent, "oc_mine", "group", "om_source");
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let expected_requests = if fail_at_send { 3 } else { 2 };
            let server_thread = std::thread::spawn(move || {
                for index in 0..expected_requests {
                    let request = server
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap()
                        .expect("预期的飞书请求应到达");
                    if index == 0 {
                        request
                            .respond(tiny_http::Response::from_string(
                                r#"{"code":0,"tenant_access_token":"token","expire":7200}"#,
                            ))
                            .unwrap();
                    } else if fail_at_send && index == 1 {
                        request
                            .respond(tiny_http::Response::from_string(
                                r#"{"code":0,"data":{"image_key":"img_probe"}}"#,
                            ))
                            .unwrap();
                    } else {
                        request
                            .respond(
                                tiny_http::Response::from_string("upstream failed")
                                    .with_status_code(500),
                            )
                            .unwrap();
                    }
                }
            });

            let client = FeishuClient::with_test_base(base);
            let error = send_image(&target, &client, image.to_str().unwrap(), None)
                .unwrap_err()
                .to_string();
            server_thread.join().unwrap();
            let expected = if fail_at_send {
                "send_image 失败"
            } else {
                "upload_image 失败"
            };
            assert!(error.contains(expected), "{error}");
            assert!(!outbound_path(&agent).exists());
        }
    }
}
