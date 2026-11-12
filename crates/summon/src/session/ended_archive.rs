//! 自动退出说明卡的归档凭据与已结束会话的盘上提交。

use std::path::PathBuf;

use anyhow::Context;
use summon_core::registry::{chat_tag, Agent};
use summon_core::session_id::{format_full_record, parse_record};

use super::{lifecycle, resume_store};
use crate::runtime::fsio::atomic_write;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct EndedArchive {
    pub token: String,
    pub chat_id: String,
    pub chat_type: String,
    pub session: String,
    pub record: String,
    pub message_id: String,
    pub card_json: String,
}

fn path(agent: &Agent, tag: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime/chats")
        .join(format!("{tag}.ended-archive"))
}

pub(crate) fn prepare(
    agent: &Agent,
    session: &str,
    chat_type: &str,
    chat_id: &str,
) -> anyhow::Result<EndedArchive> {
    let tag = chat_tag(chat_type, chat_id);
    anyhow::ensure!(
        lifecycle::read(agent, &tag).is_closed(),
        summon_i18n::format!("r54aa394ffe")
    );
    let record = resume_store::read_record_checked(agent, session)?
        .context(summon_i18n::text!("re7de0a7df5"))?;
    anyhow::ensure!(!record.fork_pending, summon_i18n::format!("r172085d584"));
    Ok(EndedArchive {
        token: uuid::Uuid::new_v4().to_string(),
        chat_id: chat_id.to_string(),
        chat_type: chat_type.to_string(),
        session: session.to_string(),
        record: format_full_record(&record),
        message_id: String::new(),
        card_json: String::new(),
    })
}

pub(crate) fn save(agent: &Agent, ticket: &EndedArchive) -> anyhow::Result<()> {
    atomic_write(
        &path(agent, &chat_tag(&ticket.chat_type, &ticket.chat_id)),
        &serde_json::to_vec(ticket)?,
    )
}

/// 新一轮会话准入先作废退出凭据，避免同一 SID 续聊再退出后旧按钮重新生效。
pub(crate) fn invalidate_checked(agent: &Agent, tag: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(path(agent, tag)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context(summon_i18n::text!("rb5a3d0ec3f")),
    }
}

/// dispatcher 串行执行；调用前须证明本 chat 没有 worker 或活跃执行轮。
pub(crate) fn archive_checked(
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    message_id: &str,
    token: &str,
) -> anyhow::Result<EndedArchive> {
    let tag = chat_tag(chat_type, chat_id);
    let raw = std::fs::read(path(agent, &tag)).context(summon_i18n::text!("r3aa6203ecd"))?;
    let ticket: EndedArchive =
        serde_json::from_slice(&raw).context(summon_i18n::text!("re1b55bae79"))?;
    anyhow::ensure!(
        ticket.chat_id == chat_id
            && ticket.chat_type == chat_type
            && ticket.token == token
            && !token.is_empty()
            && ticket.message_id == message_id,
        summon_i18n::format!("r0de49cf7c6")
    );
    anyhow::ensure!(
        lifecycle::read(agent, &tag).is_closed(),
        summon_i18n::format!("redc2191acb")
    );
    anyhow::ensure!(
        !super::tmux::has_session_strict(&ticket.session)?,
        summon_i18n::format!("r1577075435")
    );
    let target = parse_record(&ticket.record).context(summon_i18n::text!("rd18ac62586"))?;
    match resume_store::read_record_checked(agent, &ticket.session)? {
        Some(current) => {
            anyhow::ensure!(current == target, summon_i18n::format!("rebe600b0d1"));
            atomic_write(
                &resume_store::previous_path(agent, &ticket.session),
                ticket.record.as_bytes(),
            )
            .context(summon_i18n::text!("r50276c1167"))?;
            // 先清迁移期入口，主入口仍保留时发生错误可安全重试。
            let legacy = agent
                .state_dir()
                .join("sessions")
                .join(&tag)
                .join("session_id");
            match std::fs::remove_file(legacy) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context(summon_i18n::text!("r52d9c7d588")),
            }
            std::fs::remove_file(resume_store::record_path(agent, &ticket.session))
                .context(summon_i18n::text!("racfc383bea"))?;
            resume_store::clear_chat(agent, &ticket.session, &tag);
        }
        None => {
            anyhow::ensure!(
                resume_store::previous_checked(agent, &ticket.session)? == target,
                summon_i18n::format!("rab432e7553")
            );
        }
    }
    Ok(ticket)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Agent, tempfile::TempDir, EndedArchive) {
        let (agent, tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = format!("oc_ended_{}", uuid::Uuid::new_v4().simple());
        let session = agent.tmux_name_for_chat("group", &chat_id, false);
        let record = summon_core::session_id::SessionRecord {
            cli: "codex".into(),
            id: "thread-ended".into(),
            cwd: Some("/tmp/ended-work".into()),
            created_at: Some("2026-10-13 20:30:00".into()),
            primer_slot: Some("developer".into()),
            strict_resume: true,
            forked_from: Some(summon_core::session_id::ForkOrigin {
                tag: "g1234567890abcdef".into(),
                sid: "parent-thread".into(),
                at: "2026-10-13 19:00:00".into(),
            }),
            ..Default::default()
        };
        atomic_write(
            &resume_store::record_path(&agent, &session),
            format_full_record(&record).as_bytes(),
        )
        .unwrap();
        lifecycle::mark_closed(&agent, &chat_tag("group", &chat_id));
        let mut ticket = prepare(&agent, &session, "group", &chat_id).unwrap();
        ticket.message_id = "om_ended".into();
        ticket.card_json = summon_core::feishu::card_with_exit_archive(
            &summon_core::feishu::card_markdown_v2("结论"),
            "会话已退出",
            &ticket.token,
            false,
        )
        .unwrap();
        save(&agent, &ticket).unwrap();
        (agent, tmp, ticket)
    }

    fn click(agent: &Agent, ticket: &EndedArchive) -> anyhow::Result<EndedArchive> {
        archive_checked(
            agent,
            &ticket.chat_type,
            &ticket.chat_id,
            &ticket.message_id,
            &ticket.token,
        )
    }

    #[test]
    fn ended_archive_preserves_full_native_record_and_replays_idempotently() {
        let (agent, _tmp, ticket) = fixture();
        let target = parse_record(&ticket.record).unwrap();
        for _ in 0..2 {
            click(&agent, &ticket).unwrap();
            assert_eq!(
                resume_store::read_record_checked(&agent, &ticket.session).unwrap(),
                None
            );
            assert_eq!(
                resume_store::previous_checked(&agent, &ticket.session).unwrap(),
                target
            );
        }
        resume_store::restore_previous_checked(&agent, &ticket.session, &target).unwrap();
        assert_eq!(
            resume_store::read_record_checked(&agent, &ticket.session).unwrap(),
            Some(target)
        );
    }

    #[test]
    fn ended_archive_rejects_foreign_tokens_messages_chats_and_changed_records() {
        let (agent, _tmp, ticket) = fixture();
        for (chat, message, token) in [
            (ticket.chat_id.as_str(), "om_foreign", ticket.token.as_str()),
            (ticket.chat_id.as_str(), "om_ended", "foreign-token"),
            ("oc_other", "om_ended", ticket.token.as_str()),
        ] {
            assert!(archive_checked(&agent, "group", chat, message, token).is_err());
            assert!(resume_store::read_record_checked(&agent, &ticket.session)
                .unwrap()
                .is_some());
        }
        let mut changed = parse_record(&ticket.record).unwrap();
        changed.id = "thread-new".into();
        atomic_write(
            &resume_store::record_path(&agent, &ticket.session),
            format_full_record(&changed).as_bytes(),
        )
        .unwrap();
        assert!(click(&agent, &ticket).is_err());
        assert_eq!(
            resume_store::read_id(&agent, &ticket.session, "codex").as_deref(),
            Some("thread-new")
        );
        assert!(!resume_store::previous_path(&agent, &ticket.session).exists());
    }

    #[test]
    fn ended_archive_stays_invalid_after_the_same_sid_resumes_and_exits() {
        let (agent, _tmp, ticket) = fixture();
        let tag = chat_tag(&ticket.chat_type, &ticket.chat_id);
        lifecycle::mark_open(&agent, &tag).unwrap();
        lifecycle::mark_closed(&agent, &tag);
        assert!(click(&agent, &ticket).is_err());
        assert!(resume_store::read_record_checked(&agent, &ticket.session)
            .unwrap()
            .is_some());
    }

    #[test]
    fn ended_archive_save_and_legacy_removal_failures_preserve_the_current_entry() {
        for legacy_failure in [false, true] {
            let (agent, _tmp, ticket) = fixture();
            let blocked = if legacy_failure {
                agent
                    .state_dir()
                    .join("sessions")
                    .join(chat_tag(&ticket.chat_type, &ticket.chat_id))
                    .join("session_id")
            } else {
                resume_store::previous_path(&agent, &ticket.session)
            };
            std::fs::create_dir_all(&blocked).unwrap();
            assert!(click(&agent, &ticket).is_err());
            assert_eq!(
                resume_store::read_record_checked(&agent, &ticket.session).unwrap(),
                parse_record(&ticket.record)
            );
        }
    }

    #[test]
    fn ended_archive_invalidation_failure_blocks_new_admission() {
        let (agent, _tmp, ticket) = fixture();
        let tag = chat_tag(&ticket.chat_type, &ticket.chat_id);
        let cap = path(&agent, &tag);
        std::fs::remove_file(&cap).unwrap();
        std::fs::create_dir(&cap).unwrap();
        assert!(lifecycle::mark_open(&agent, &tag).is_err());
        assert!(lifecycle::read(&agent, &tag).is_closed());
        assert!(resume_store::read_record_checked(&agent, &ticket.session)
            .unwrap()
            .is_some());
    }
}
