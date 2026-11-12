//! 出站公共投递原语(A6):「真实发到飞书 + 归档落 `outbound.ndjson`」这条不变量的唯一收口。
//! `Frontend` trait 之下的底座——turn 流经 `FeishuFrontend` 间接走这里,面板/回执/闸门等
//! 非 turn 出站由 daemon 各 handler 直调。从 daemon.rs 纯平移下沉,依赖箭头只从 runtime
//! 指向 frontend,不再倒置。

use crate::frontend::client::FeishuClient;
use std::io::Write;
use summon_core::registry::chat_tag;

/// 把真实发到飞书的出站消息/卡片追加到 `<agent.dir>/threads/<chat-tag>/outbound.ndjson`。
/// 只在发送成功后调用;失败静默忽略,日志归档不能影响主链路。
pub(crate) fn append_outbound(
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    kind: &str,
    payload: serde_json::Value,
) {
    let path = agent_dir
        .join("threads")
        .join(chat_tag(chat_type, chat_id))
        .join("outbound.ndjson");
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    // 与同目录 events.jsonl/transcript.md 共用单代上限，避免高频卡片全文归档无界增长。
    crate::observe::event_log::rotate_if_oversized(
        &path,
        crate::observe::event_log::CHAT_LOG_MAX_BYTES,
    );
    let entry = serde_json::json!({
        "ts": chrono::Local::now().to_rfc3339(),
        "kind": kind,
        "chat_type": chat_type,
        "chat_id": chat_id,
        "payload": payload,
    });
    let line = format!("{entry}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

pub(crate) fn card_payload(content: &str) -> serde_json::Value {
    serde_json::from_str(content).unwrap_or_else(|_| serde_json::json!(content))
}

pub(crate) fn send_markdown_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    markdown: &str,
) -> anyhow::Result<()> {
    send_markdown_archived_anchored(fc, agent_dir, chat_type, chat_id, None, markdown)
}

/// 同上,但可以把这条消息**锚进发起者所在的 thread**。
///
/// 用途:属于某一轮的回执(命令结果、拒绝原因、执行失败说明)应该跟着用户那条消息走 ——
/// 用户在 thread 里 @ bot,回执却掉到群根部,他得翻出 thread 才看得到。
///
/// `anchor` 有两套约定,按回执属于哪一类选:
/// - 普通轮的回复(`thread_anchor_for` / `turn_reply_target`):**只有原消息确实在 thread 里才传
///   Some**,不在 thread 里的传 `None` 走普通发送 —— 否则 reply 会把一句回答切成话题串。
/// - summon 自己处理的指令回执(`command_anchor` / `command_anchor_for`):群里**一律**传那条指令
///   消息,让回执挂进它的 thread(主干上的指令会因此新开一条),群主干只留对话(owner 2026-09-02)。
///   只确认照做了的回执不为它开 thread,群里改贴完成表情(`daemon::command_ack`)。
///
/// ## 卡片回调的回执**有意维持不锚**(owner 2026-08-14 裁决)
///
/// `cards.rs` 里那批「点了面板按钮之后的回应」没有走本函数的锚定版,是**决定**不是遗漏:
/// 卡片回调的 payload 不带被点卡片的 thread 信息,要锚就得多查一次消息详情。owner 判断
/// 「为这个观感多打一次 API 不优雅」,故维持发到群根部。
///
/// 将来若要改,先解决信息来源问题——**不要**用「一律 reply 被点卡片」来绕过:那会把群里
/// 本来就在根部的卡片回执统统变成引用回复,是另一个方向的行为变化。
pub(crate) fn send_markdown_archived_anchored(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    anchor: Option<&str>,
    markdown: &str,
) -> anyhow::Result<()> {
    send_markdown_archived_replying(
        fc,
        agent_dir,
        chat_type,
        chat_id,
        anchor.map(|message_id| (message_id, true)),
        markdown,
    )
}

/// 同上,但回复目标由调用方给全:`(message_id, reply_in_thread)`。`false` 是不开 thread 的引用
/// 回复,私聊里要指明「是哪条」只能靠它。
pub(crate) fn send_markdown_archived_replying(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    reply_target: Option<(&str, bool)>,
    markdown: &str,
) -> anyhow::Result<()> {
    let msg_id = fc.send_card_id_replying(
        chat_id,
        reply_target,
        &summon_core::feishu::card_markdown_v2(markdown),
    )?;
    append_outbound(
        agent_dir,
        chat_type,
        chat_id,
        "markdown_card",
        serde_json::json!({ "message_id": msg_id, "markdown": markdown }),
    );
    Ok(())
}

/// 会话镜像 terminal/通知卡专用:幂等(uuid)+ 受信 `<at>` 不二次转义。
///
/// `uuid` 由 transcript sid + terminal boundary + 块序号稳定派生;飞书已收但本地超时、或发送
/// 成功后 offset 落盘失败时,重放仍只产生一条消息。
///
/// **只保留这一个入口(裸 `&str` 版已删除)**:此前那版把文本直接交给 `card_markdown`,`<at>`
/// 被当外部文本转义掉,`@某bot` 以纯文字发出 —— 飞书据此认定没 @ 任何人,被 @ 的 bot 的 app
/// 收不到消息事件(2026-08-12 owner 实测)。参数类型收成 `RenderedLarkMd` 之后,新增镜像发送点
/// 在编译期就不可能再绕过那层渲染。
pub(crate) fn send_rendered_markdown_idempotent_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    markdown: &summon_core::feishu::RenderedLarkMd,
    uuid: &str,
) -> anyhow::Result<()> {
    let message_id = fc.send_rendered_markdown_idempotent_id(chat_id, markdown, uuid)?;
    append_outbound(
        agent_dir,
        chat_type,
        chat_id,
        "markdown_card",
        serde_json::json!({
            "message_id": message_id,
            "uuid": uuid,
            "markdown": markdown.as_str(),
        }),
    );
    Ok(())
}

/// 带文件按钮的已渲染正文:卡片由调用方挂好按钮,幂等投递并归档正文。
pub(crate) fn send_card_idempotent_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    markdown: &summon_core::feishu::RenderedLarkMd,
    card: &str,
    uuid: &str,
) -> anyhow::Result<()> {
    let message_id = fc.send_card_idempotent_id(chat_id, card, uuid)?;
    append_outbound(
        agent_dir,
        chat_type,
        chat_id,
        "markdown_card",
        serde_json::json!({
            "message_id": message_id,
            "uuid": uuid,
            "markdown": markdown.as_str(),
        }),
    );
    Ok(())
}

/// 已安全渲染的外部文本投递。与 [`send_markdown_archived`] 分开，避免普通调用方绕过转义，
/// 同时防止受信 `<at>` 被 `card_markdown` 二次转义。返回发出的 message_id。
pub(crate) fn send_rendered_markdown_archived_id(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    reply_target: Option<(&str, bool)>,
    markdown: &summon_core::feishu::RenderedLarkMd,
) -> anyhow::Result<String> {
    let msg_id = fc.send_rendered_markdown_with_id(chat_id, reply_target, markdown)?;
    append_outbound(
        agent_dir,
        chat_type,
        chat_id,
        "markdown_card",
        serde_json::json!({ "message_id": msg_id, "markdown": markdown.as_str() }),
    );
    Ok(msg_id)
}

/// 冻结通知在群发送与私聊回退时共用同一份原始内容。
pub(crate) fn send_frozen_card_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    receive_id: &str,
    receive_id_type: &str,
    kind: &str,
    content: &str,
) -> anyhow::Result<()> {
    let message_id = fc.send_frozen_card_id(receive_id, receive_id_type, content)?;
    append_outbound(
        agent_dir,
        chat_type,
        receive_id,
        kind,
        serde_json::json!({
            "message_id": message_id, "card": card_payload(content), "receive_id_type": receive_id_type,
        }),
    );
    Ok(())
}

pub(crate) fn send_card_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    anchor: Option<&str>,
    kind: &str,
    content: &str,
) -> anyhow::Result<()> {
    send_card_archived_id(fc, agent_dir, chat_type, chat_id, anchor, kind, content).map(|_| ())
}

/// 同 `send_card_archived`,但返回飞书 message_id(供后续 `update_card_archived` 原地刷新用)。
/// 面板卡片等需要后续 PATCH 的场景走这个。
pub(crate) fn send_card_archived_id(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    anchor: Option<&str>,
    kind: &str,
    content: &str,
) -> anyhow::Result<String> {
    let msg_id = fc.send_card_id_anchored(chat_id, anchor, content)?;
    append_outbound(
        agent_dir,
        chat_type,
        chat_id,
        kind,
        serde_json::json!({
            "message_id": msg_id,
            "card": card_payload(content),
        }),
    );
    Ok(msg_id)
}

/// Bridge durable outbox 专用。卡片已由 bridge renderer 完成外部文本转义；这里不做 mention
/// 替换，只把确定性 uuid 交给飞书。external transcript 已有 Claude 原始文件，summon 的
/// outbound 审计只留 uuid/kind，不再保存第三份完整正文。
pub(crate) fn send_card_idempotent_archived_id(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    kind: &str,
    content: &str,
    uuid: &str,
) -> anyhow::Result<String> {
    let message_id = fc.send_card_idempotent_id(chat_id, content, uuid)?;
    append_outbound(
        agent_dir,
        chat_type,
        chat_id,
        kind,
        serde_json::json!({
            "uuid": uuid,
            "content_redacted": true,
        }),
    );
    Ok(message_id)
}

/// 主动向用户 open_id 私聊发送 interactive 卡片并归档；返回 message_id 供 ticket 做 first-write-wins
/// 登记。归档键使用 p2p+open_id，不假造未知的 private chat_id。
pub(crate) fn send_card_to_open_id_archived_id(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    open_id: &str,
    kind: &str,
    content: &str,
) -> anyhow::Result<String> {
    let message_id = fc.send_card_to_open_id_id(open_id, content)?;
    append_outbound(
        agent_dir,
        "p2p",
        open_id,
        kind,
        serde_json::json!({
            "card": card_payload(content),
            "receive_id_type": "open_id",
        }),
    );
    Ok(message_id)
}

/// 私聊发卡,归档只记「发过一张什么卡」,卡面与 message_id 都不记。终端卡上是一次性链接:归档
/// 会被引用解析读回并喂进 agent 上下文,链接落进去就等于交给了 agent;有了 message_id,agent 也能
/// 用 bot 身份把卡面取回来。要认卡的地方用返回值自己记。
pub(crate) fn send_card_to_open_id_redacted_archived_id(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    open_id: &str,
    kind: &str,
    content: &str,
) -> anyhow::Result<String> {
    let message_id = fc.send_card_to_open_id_id(open_id, content)?;
    append_outbound(
        agent_dir,
        "p2p",
        open_id,
        kind,
        serde_json::json!({
            "receive_id_type": "open_id",
            "content_redacted": true,
        }),
    );
    Ok(message_id)
}

/// 私聊发一个文件,归档只记「发过一个什么文件」,file_key 与 message_id 都不记:有了它们,agent
/// 能用 bot 身份把文件取回来。
pub(crate) fn send_file_to_open_id_redacted_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    open_id: &str,
    kind: &str,
    bytes: Vec<u8>,
    file_name: &str,
) -> anyhow::Result<String> {
    let file_key = fc.upload_file(bytes, file_name)?;
    let message_id = fc.send_file_to_open_id(open_id, &file_key)?;
    append_outbound(
        agent_dir,
        "p2p",
        open_id,
        kind,
        serde_json::json!({
            "receive_id_type": "open_id",
            "file_name": file_name,
            "content_redacted": true,
        }),
    );
    Ok(message_id)
}

/// 主动向用户 open_id 私聊发 markdown 并归档(sentinel 失败告知等轻量通知用)。
/// 归档键同上:p2p+open_id,不假造未知的 private chat_id。
pub(crate) fn send_markdown_to_open_id_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    open_id: &str,
    kind: &str,
    markdown: &str,
) -> anyhow::Result<()> {
    fc.send_markdown_to_open_id(open_id, markdown)?;
    append_outbound(
        agent_dir,
        "p2p",
        open_id,
        kind,
        serde_json::json!({
            "markdown": markdown,
            "receive_id_type": "open_id",
        }),
    );
    Ok(())
}

pub(crate) fn update_card_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    message_id: &str,
    content: &str,
) -> anyhow::Result<()> {
    fc.update_card(message_id, content)?;
    append_outbound(
        agent_dir,
        chat_type,
        chat_id,
        "card_update",
        serde_json::json!({
            "message_id": message_id,
            "card": card_payload(content),
        }),
    );
    Ok(())
}

/// RecoveryCard/Bridge 正文来自其他权威持久层时使用：真实 PATCH 不变，但本地 outbound 审计
/// 只记 message_id，不再复制 preview/stage/transcript 正文。
pub(crate) fn update_card_redacted_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    message_id: &str,
    content: &str,
) -> anyhow::Result<()> {
    fc.update_card(message_id, content)?;
    append_outbound(
        agent_dir,
        chat_type,
        chat_id,
        "card_update",
        serde_json::json!({
            "message_id": message_id,
            "content_redacted": true,
        }),
    );
    Ok(())
}

/// 原地更新一张卡,归档同上:卡面与 message_id 都不记。
pub(crate) fn update_card_unlinked_archived(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    kind: &str,
    message_id: &str,
    content: &str,
) -> anyhow::Result<()> {
    fc.update_card(message_id, content)?;
    append_outbound(
        agent_dir,
        chat_type,
        chat_id,
        kind,
        serde_json::json!({ "content_redacted": true }),
    );
    Ok(())
}

/// 过程补充保持话题位置与幂等键，归档只记录投递身份。
pub(crate) fn send_process_supplement_archived(
    fc: &FeishuClient,
    chat: &crate::frontend::ChatRef<'_>,
    content: &str,
    uuid: &str,
) -> anyhow::Result<()> {
    fc.send_card_idempotent_anchored(chat.chat_id, chat.thread_anchor, content, uuid)?;
    append_outbound(
        chat.agent_dir,
        chat.chat_type,
        chat.chat_id,
        "process_supplement",
        serde_json::json!({"uuid": uuid, "content_redacted": true}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_outbound_writes_ndjson_under_thread() {
        let tmp = std::env::temp_dir().join(format!("summon-outbound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        append_outbound(
            &tmp,
            "group",
            "oc_g",
            "ask_card",
            serde_json::json!({"card": {"schema": "2.0"}}),
        );
        let p = tmp
            .join("threads")
            .join(chat_tag("group", "oc_g"))
            .join("outbound.ndjson");
        let s = std::fs::read_to_string(&p).unwrap();
        let line = s.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["kind"], "ask_card");
        assert_eq!(v["chat_type"], "group");
        assert_eq!(v["chat_id"], "oc_g");
        assert_eq!(v["payload"]["card"]["schema"], "2.0");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn append_outbound_rotates_oversized_log() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join("threads")
            .join(chat_tag("group", "oc_rotate"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("outbound.ndjson");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(crate::observe::event_log::CHAT_LOG_MAX_BYTES + 1)
            .unwrap();

        append_outbound(
            tmp.path(),
            "group",
            "oc_rotate",
            "markdown_card",
            serde_json::json!({"markdown": "ok"}),
        );

        assert!(path.with_file_name("outbound.ndjson.1").exists());
        assert!(std::fs::metadata(path).unwrap().len() < 1024);
    }

    #[test]
    fn open_id_card_wrapper_is_dry_and_archives_receive_type() {
        let tmp = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let message_id = send_card_to_open_id_archived_id(
            &fc,
            tmp.path(),
            "ou_owner",
            "bridge_ticket",
            r#"{"schema":"2.0"}"#,
        )
        .unwrap();
        assert_eq!(message_id, "dry-msg");
        let path = tmp
            .path()
            .join("threads")
            .join(chat_tag("p2p", "ou_owner"))
            .join("outbound.ndjson");
        let record: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(path).unwrap().trim()).unwrap();
        assert_eq!(record["payload"]["receive_id_type"], "open_id");
        assert_eq!(record["kind"], "bridge_ticket");
    }

    #[test]
    fn group_card_archive_keeps_returned_message_id_for_incident_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let message_id = send_card_archived_id(
            &fc,
            tmp.path(),
            "group",
            "oc_task",
            None,
            "task_card",
            r#"{"schema":"2.0"}"#,
        )
        .unwrap();
        let path = tmp
            .path()
            .join("threads")
            .join(chat_tag("group", "oc_task"))
            .join("outbound.ndjson");
        let record: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(path).unwrap().trim()).unwrap();

        assert_eq!(message_id, "dry-msg");
        assert_eq!(record["payload"]["message_id"], message_id);
        assert_eq!(record["kind"], "task_card");
    }

    /// 终端卡上是一次性链接:归档会被引用解析读回并喂进 agent 上下文,卡面一个字都不能进去。
    #[test]
    fn a_redacted_dm_card_archives_neither_the_body_nor_any_link() {
        let tmp = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let card = summon_core::feishu::card_terminal(&summon_core::feishu::TerminalCard {
            agent: "arcadia",
            chat_label: "开发群",
            read_only_url: "http://10.0.0.5:7766/t/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            operate_url: "http://10.0.0.5:7766/t/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            target_chat_id: "oc_1",
            target_chat_type: "group",
            recipient_open_id: "ou_owner",
        });
        let message_id = send_card_to_open_id_redacted_archived_id(
            &fc,
            tmp.path(),
            "ou_owner",
            "terminal_card",
            &card,
        )
        .unwrap();
        let raw = std::fs::read_to_string(
            tmp.path()
                .join("threads")
                .join(chat_tag("p2p", "ou_owner"))
                .join("outbound.ndjson"),
        )
        .unwrap();
        assert!(!raw.contains("/t/"), "{raw}");
        assert!(!raw.contains("7766"), "{raw}");
        assert!(!raw.contains("只读查看"), "卡面不进归档:{raw}");
        assert!(!message_id.is_empty());
        assert!(
            !raw.contains(&message_id),
            "message_id 不进归档,agent 拿它能取回卡面:{raw}"
        );
        let record: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(record["kind"], "terminal_card");
        assert_eq!(record["payload"]["content_redacted"], true);
        assert!(record["payload"].get("message_id").is_none());
        assert!(record["payload"].get("card").is_none());
    }

    #[test]
    fn an_unlinked_card_update_archives_neither_the_body_nor_the_message_id() {
        let tmp = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        update_card_unlinked_archived(
            &fc,
            tmp.path(),
            "p2p",
            "oc_dm",
            "terminal_card_update",
            "om_terminal_card",
            r#"{"schema":"2.0","url":"http://10.0.0.5:7766/t/cccccccccccccccccccccccccccccccc"}"#,
        )
        .unwrap();
        let raw = std::fs::read_to_string(
            tmp.path()
                .join("threads")
                .join(chat_tag("p2p", "oc_dm"))
                .join("outbound.ndjson"),
        )
        .unwrap();
        assert!(!raw.contains("/t/"), "{raw}");
        assert!(!raw.contains("om_terminal_card"), "{raw}");
        let record: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(record["kind"], "terminal_card_update");
        assert_eq!(record["payload"]["content_redacted"], true);
    }

    #[test]
    fn redacted_card_update_never_archives_recovery_body() {
        let tmp = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        update_card_redacted_archived(
            &fc,
            tmp.path(),
            "group",
            "oc_recovery",
            "om_card",
            r#"{"schema":"2.0","secret":"VERY_SECRET_RECOVERY_UPDATE"}"#,
        )
        .unwrap();
        let raw = std::fs::read_to_string(
            tmp.path()
                .join("threads")
                .join(chat_tag("group", "oc_recovery"))
                .join("outbound.ndjson"),
        )
        .unwrap();
        assert!(!raw.contains("VERY_SECRET_RECOVERY_UPDATE"));
        let record: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(record["payload"]["message_id"], "om_card");
        assert_eq!(record["payload"]["content_redacted"], true);
        assert!(record["payload"].get("card").is_none());
    }
}
