//! summon-owned RPC 会话的 Bridge 候选发现。
//!
//! 外部 recall 扫 provider 自己的历史库；本模块只扫当前 agent 的 `.session + .meta`，并要求
//! 同一个 SID 此刻仍由 summon 的活池独占持有。卡片只是快照，点击时 coordinator 仍会完整重验。

use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{ensure, Context as _, Result};
use summon_core::chat_meta::ChatMeta;
use summon_core::recall::{RecallOwnership, RecallProvider, SessionCandidate};
use summon_core::registry::{chat_tag, Agent, CaptureMode};
use summon_core::session_id::{parse_record, SessionRecord};

use crate::capture::rpc_pool::{RpcHandoffSnapshot, RpcPool};

pub(crate) fn durable_source_ref(
    agent_name: &str,
    source_session: &str,
    source_chat_id: &str,
    source_chat_type: &str,
    source_tag: &str,
    record: &SessionRecord,
) -> String {
    let mut input = Vec::new();
    for field in [
        agent_name,
        source_session,
        source_chat_id,
        source_chat_type,
        source_tag,
        record.cli.as_str(),
        record.id.as_str(),
        record.cwd.as_deref().unwrap_or(""),
        record.created_at.as_deref().unwrap_or(""),
    ] {
        input.extend_from_slice(&(field.len() as u64).to_be_bytes());
        input.extend_from_slice(field.as_bytes());
    }
    ring::digest::digest(&ring::digest::SHA256, &input)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 不依赖本代进程身份的 durable source ownership。live handoff 还会另外 exact 校验 pool
/// snapshot；这个引用刻意只覆盖盘上唯一 owner，使已 fsync 的 callback 在 daemon 崩溃、旧
/// pool 被 boot 收敛后仍能证明自己指向同一条 source `.session`，而不是永久失去重放能力。
#[derive(Debug, Clone)]
pub(crate) struct DurableRpcHandoffSource {
    pub(crate) source_chat_id: String,
    pub(crate) source_chat_type: String,
    pub(crate) source_session: String,
    pub(crate) source_tag: String,
    pub(crate) record: SessionRecord,
    pub(crate) name: String,
    pub(crate) last_active_ms: u64,
    pub(crate) source_ref: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedRpcRecallCandidate {
    pub(crate) candidate: SessionCandidate,
    pub(crate) source_chat_id: String,
    pub(crate) source_chat_type: String,
    pub(crate) source_session: String,
    pub(crate) source_tag: String,
    pub(crate) record: SessionRecord,
    pub(crate) pool: RpcHandoffSnapshot,
}

pub(crate) fn discover(
    agent: &Agent,
    target_chat_id: &str,
    pool: &RpcPool,
) -> Result<Vec<ManagedRpcRecallCandidate>> {
    discover_with(agent, target_chat_id, &|agent_name, tag| {
        pool.handoff_snapshot(agent_name, tag)
    })
}

fn discover_with(
    agent: &Agent,
    target_chat_id: &str,
    snapshot: &dyn Fn(&str, &str) -> Result<Option<RpcHandoffSnapshot>>,
) -> Result<Vec<ManagedRpcRecallCandidate>> {
    let (provider, sources) = scan_durable_sources(agent, target_chat_id)?;
    let Some(provider) = provider else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    for source in sources {
        let cwd = source
            .record
            .cwd
            .as_deref()
            .context(summon_i18n::text!("r447e9cd7be"))?;
        let Some(pool) = snapshot(&agent.name, &source.source_tag)? else {
            continue;
        };
        if !pool.running
            || !pool.idle
            || pool.session_id != source.record.id
            || pool.spawn_cwd.as_deref() != Some(cwd)
        {
            continue;
        }
        candidates.push(ManagedRpcRecallCandidate {
            candidate: SessionCandidate {
                provider,
                ownership: RecallOwnership::ManagedRpc,
                sid: source.record.id.clone(),
                cwd: cwd.to_string(),
                name: source.name,
                prompt_preview: String::new(),
                last_active_ms: source.last_active_ms,
                live_pid: u32::try_from(pool.pgid).ok(),
                live_status: Some("idle".to_string()),
                source_observation_id: Some(source.source_ref),
                background_tasks: Vec::new(),
            },
            source_chat_id: source.source_chat_id,
            source_chat_type: source.source_chat_type,
            source_session: source.source_session,
            source_tag: source.source_tag,
            record: source.record,
            pool,
        });
    }
    candidates.sort_by(|left, right| {
        right
            .candidate
            .last_active_ms
            .cmp(&left.candidate.last_active_ms)
            .then_with(|| left.source_session.cmp(&right.source_session))
    });
    Ok(candidates)
}

pub(crate) fn find_durable_source(
    agent: &Agent,
    target_chat_id: &str,
    sid: &str,
    expected_source_ref: &str,
) -> Result<Option<DurableRpcHandoffSource>> {
    let (_, sources) = scan_durable_sources(agent, target_chat_id)?;
    let mut matches = sources
        .into_iter()
        .filter(|source| source.record.id == sid && source.source_ref == expected_source_ref)
        .collect::<Vec<_>>();
    ensure!(matches.len() <= 1, summon_i18n::format!("r233fe9cf1c"));
    Ok(matches.pop())
}

/// target 已经持有 `.session` 时，durable callback replay 用它确认没有另一条盘上 source 仍
/// 声称同一 SID。target canonical 本身由扫描器排除，不会把已提交 owner 算进来。
pub(crate) fn durable_source_count_by_sid(
    agent: &Agent,
    target_chat_id: &str,
    sid: &str,
) -> Result<usize> {
    let (_, sources) = scan_durable_sources(agent, target_chat_id)?;
    Ok(sources
        .into_iter()
        .filter(|source| source.record.id == sid)
        .count())
}

fn scan_durable_sources(
    agent: &Agent,
    target_chat_id: &str,
) -> Result<(Option<RecallProvider>, Vec<DurableRpcHandoffSource>)> {
    if agent.capture_mode() != CaptureMode::Rpc || target_chat_id.trim().is_empty() {
        return Ok((None, Vec::new()));
    }
    let provider = RecallProvider::from_cli_kind(agent.cli_kind())
        .context(summon_i18n::text!("r15f081f913"))?;
    ensure!(
        matches!(provider, RecallProvider::Codex | RecallProvider::Cursor),
        summon_i18n::format!("r1b7dbca214")
    );
    let channel = agent.resume_cli_kind();
    ensure!(
        matches!(channel.as_str(), "codex" | "cursor-acp"),
        summon_i18n::format!("rc2193f1ba2")
    );

    let dir = agent.state_dir().join("runtime").join("chats");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Some(provider), Vec::new()))
        }
        Err(error) => return Err(error).with_context(|| summon_i18n::format!("r6933501957")),
    };
    let mut sources = Vec::new();
    let target_session = agent.tmux_name_for_chat("group", target_chat_id, false);
    for entry in entries {
        let entry = entry.with_context(|| summon_i18n::format!("r4d74ea70a4"))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("session") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| summon_i18n::format!("rfef9abd607"))?;
        ensure!(
            metadata.file_type().is_file(),
            summon_i18n::format!("r2f0b3263b2")
        );
        let content =
            std::fs::read_to_string(&path).with_context(|| summon_i18n::format!("r2f3f3862c6"))?;
        let record = parse_record(&content).with_context(|| summon_i18n::format!("r87504297e8"))?;
        if record.cli != channel {
            continue;
        }
        let source_session = path
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .with_context(|| summon_i18n::format!("r53a23258ce"))?
            .to_string();
        if source_session == target_session {
            // callback 崩在 `.session` commit 之后、target meta/cwd 写入之前时，target owner 可能
            // 暂时没有 meta。它不是 source 候选，必须在读 meta 前排除，交给 replay 幂等收口。
            continue;
        }
        let meta = read_source_meta(&path.with_extension("meta"))?;
        let source_chat_id = meta
            .chat_id
            .filter(|value| !value.trim().is_empty())
            .context(summon_i18n::text!("r6ef42c1f5a"))?;
        let source_chat_type = meta.chat_type.context(summon_i18n::text!("r9618d1292a"))?;
        if !matches!(source_chat_type.as_str(), "group" | "topic_group") {
            continue;
        }
        if source_chat_id == target_chat_id {
            continue;
        }
        let source_tag = chat_tag(&source_chat_type, &source_chat_id);
        ensure!(
            meta.tag.as_deref() == Some(source_tag.as_str()),
            summon_i18n::format!("r697f26ee35")
        );
        match crate::session::resume_store::inspect_legacy(agent, &source_tag) {
            crate::session::resume_store::ResumePresence::Missing => {}
            crate::session::resume_store::ResumePresence::Valid(_) => {
                // 新旧记录并存时，move 新 `.session` 后 source 会从 legacy fallback 复活同一 SID；
                // 它不是一个可安全迁移的单 owner，候选层直接隐藏。
                continue;
            }
            crate::session::resume_store::ResumePresence::Corrupt => {
                anyhow::bail!(summon_i18n::format!("rc8b770c096"))
            }
            crate::session::resume_store::ResumePresence::Unreadable => {
                anyhow::bail!(summon_i18n::format!("ra7f89df8f2"))
            }
        }
        ensure!(
            source_session == agent.tmux_name_for_chat(&source_chat_type, &source_chat_id, false),
            summon_i18n::format!("ra80df1a647")
        );
        record
            .cwd
            .as_deref()
            .filter(|cwd| Path::new(cwd).is_absolute() && Path::new(cwd).is_dir())
            .context(summon_i18n::text!("rac396dbf6f"))?;
        let last_active_ms = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_millis() as u64)
            .unwrap_or(0);
        let revision = durable_source_ref(
            &agent.name,
            &source_session,
            &source_chat_id,
            &source_chat_type,
            &source_tag,
            &record,
        );
        sources.push(DurableRpcHandoffSource {
            source_chat_id,
            source_chat_type,
            source_session,
            source_tag,
            record,
            name: meta
                .chat_name
                .unwrap_or_else(|| summon_i18n::text!("rb3bfb39f4c").to_string()),
            last_active_ms,
            source_ref: revision,
        });
    }
    sources.sort_by(|left, right| {
        right
            .last_active_ms
            .cmp(&left.last_active_ms)
            .then_with(|| left.source_session.cmp(&right.source_session))
    });
    Ok((Some(provider), sources))
}

fn read_source_meta(path: &Path) -> Result<ChatMeta> {
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| summon_i18n::format!("rff8addf851"))?;
    ensure!(
        metadata.file_type().is_file(),
        summon_i18n::format!("r928ae37788")
    );
    let content =
        std::fs::read_to_string(path).with_context(|| summon_i18n::format!("rd38be33f01"))?;
    Ok(ChatMeta::parse(&content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::chat_meta::ChatMeta;
    use summon_core::session_id::format_record;

    fn agent_at(dir: &Path, cli: &str) -> Agent {
        let yaml = format!("name: test\nruntime:\n  cli: {cli}\n  capture: rpc\n");
        let mut agent: Agent = serde_yaml::from_str(&yaml).unwrap();
        // agents_dir 必须是 tempdir 的子目录:state_root(agents_dir) = agents_dir.parent()/state/,
        // 这样 state 根落在 tempdir 内,不跨测试串号(直接用 tempdir 根会让 state 解析到系统 temp)。
        let agents_dir = dir.join("agents");
        let agent_dir = agents_dir.join("test");
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        agent
    }

    fn seed_source(
        agent: &Agent,
        chat_id: &str,
        chat_type: &str,
        sid: &str,
        cwd: &Path,
    ) -> (String, String) {
        let tag = chat_tag(chat_type, chat_id);
        let session = agent.tmux_name_for_chat(chat_type, chat_id, false);
        let dir = agent.state_dir().join("runtime/chats");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{session}.session")),
            format_record(
                &agent.resume_cli_kind(),
                sid,
                Some(cwd.to_str().unwrap()),
                None,
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{session}.meta")),
            ChatMeta {
                chat_id: Some(chat_id.to_string()),
                chat_type: Some(chat_type.to_string()),
                chat_name: Some("源群".to_string()),
                tag: Some(tag.clone()),
                ..Default::default()
            }
            .render(),
        )
        .unwrap();
        (session, tag)
    }

    fn idle_snapshot(sid: &str, cwd: &Path) -> RpcHandoffSnapshot {
        RpcHandoffSnapshot {
            session_id: sid.to_string(),
            spawn_cwd: Some(cwd.to_string_lossy().into_owned()),
            pgid: 42,
            running: true,
            idle: true,
        }
    }

    #[test]
    fn discovers_only_exact_idle_group_owned_by_the_current_rpc_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let agent = agent_at(tmp.path(), "cursor");
        let (session, tag) = seed_source(&agent, "oc_source", "group", "acp-1", &cwd);

        let found = discover_with(&agent, "oc_target", &|agent_name, asked_tag| {
            assert_eq!(agent_name, "test");
            assert_eq!(asked_tag, tag);
            Ok(Some(idle_snapshot("acp-1", &cwd)))
        })
        .unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source_session, session);
        assert_eq!(found[0].source_chat_id, "oc_source");
        assert_eq!(found[0].source_chat_type, "group");
        assert_eq!(found[0].candidate.ownership, RecallOwnership::ManagedRpc);
        assert_eq!(found[0].record.cli, "cursor-acp");
        assert_eq!(
            found[0]
                .candidate
                .source_observation_id
                .as_deref()
                .unwrap()
                .len(),
            64
        );
    }

    #[test]
    fn durable_source_revision_ignores_process_generation_but_fences_owner_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work-a");
        let changed_cwd = tmp.path().join("work-b");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&changed_cwd).unwrap();
        let agent = agent_at(tmp.path(), "codex");
        let (session, _) = seed_source(&agent, "oc_source", "group", "th-1", &cwd);

        let revision = discover_with(&agent, "oc_target", &|_, _| {
            Ok(Some(idle_snapshot("th-1", &cwd)))
        })
        .unwrap()[0]
            .candidate
            .source_observation_id
            .clone()
            .unwrap();
        let mut next_generation = idle_snapshot("th-1", &cwd);
        next_generation.pgid = 43;
        let after_restart = discover_with(&agent, "oc_target", &|_, _| {
            Ok(Some(next_generation.clone()))
        })
        .unwrap()[0]
            .candidate
            .source_observation_id
            .clone()
            .unwrap();
        assert_eq!(
            after_restart, revision,
            "durable WAL callback 不能只因 daemon 收敛旧进程就永久失效"
        );
        assert!(find_durable_source(&agent, "oc_target", "th-1", &revision)
            .unwrap()
            .is_some());

        std::fs::write(
            agent
                .state_dir()
                .join("runtime/chats")
                .join(format!("{session}.session")),
            format_record("codex", "th-1", Some(changed_cwd.to_str().unwrap()), None),
        )
        .unwrap();
        assert!(
            find_durable_source(&agent, "oc_target", "th-1", &revision)
                .unwrap()
                .is_none(),
            "cwd/owner identity 变化后旧卡必须失效"
        );
    }

    #[test]
    fn same_target_private_source_and_non_idle_pool_never_become_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let agent = agent_at(tmp.path(), "codex");
        let (_, tag) = seed_source(&agent, "oc_same", "group", "th-1", &cwd);
        assert!(discover_with(&agent, "oc_same", &|_, _| {
            Ok(Some(idle_snapshot("th-1", &cwd)))
        })
        .unwrap()
        .is_empty());

        let mut busy = idle_snapshot("th-1", &cwd);
        busy.idle = false;
        assert!(discover_with(&agent, "oc_other", &|_, asked| {
            assert_eq!(asked, tag);
            Ok(Some(busy.clone()))
        })
        .unwrap()
        .is_empty());

        let private = agent.tmux_name_for_chat("p2p", "ou_private", false);
        let dir = agent.state_dir().join("runtime/chats");
        std::fs::remove_file(dir.join(format!(
            "{}.session",
            agent.tmux_name_for_chat("group", "oc_same", false)
        )))
        .unwrap();
        std::fs::remove_file(dir.join(format!(
            "{}.meta",
            agent.tmux_name_for_chat("group", "oc_same", false)
        )))
        .unwrap();
        std::fs::write(
            dir.join(format!("{private}.session")),
            format_record("codex", "private", Some(cwd.to_str().unwrap()), None),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{private}.meta")),
            ChatMeta {
                chat_id: Some("ou_private".into()),
                chat_type: Some("p2p".into()),
                tag: Some(chat_tag("p2p", "ou_private")),
                ..Default::default()
            }
            .render(),
        )
        .unwrap();
        assert!(discover_with(&agent, "oc_other", &|_, _| {
            Ok(Some(idle_snapshot("private", &cwd)))
        })
        .unwrap()
        .is_empty());
    }

    #[test]
    fn corrupt_session_or_missing_meta_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path(), "codex");
        let dir = agent.state_dir().join("runtime/chats");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("soul-test-bad.session"), "half-written").unwrap();
        assert!(discover_with(&agent, "oc_target", &|_, _| Ok(None)).is_err());

        std::fs::write(
            dir.join("soul-test-bad.session"),
            format_record("codex", "th-1", Some(tmp.path().to_str().unwrap()), None),
        )
        .unwrap();
        assert!(discover_with(&agent, "oc_target", &|_, _| Ok(None)).is_err());
    }

    #[test]
    fn new_and_legacy_resume_coexistence_is_never_handoff_capable() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let agent = agent_at(tmp.path(), "codex");
        let (_, tag) = seed_source(&agent, "oc_source", "group", "th-legacy", &cwd);
        let legacy = agent
            .state_dir()
            .join("sessions")
            .join(&tag)
            .join("session_id");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(
            legacy,
            format_record("codex", "th-legacy", Some(cwd.to_str().unwrap()), None),
        )
        .unwrap();

        let found = discover_with(&agent, "oc_target", &|_, _| {
            Ok(Some(idle_snapshot("th-legacy", &cwd)))
        })
        .unwrap();
        assert!(
            found.is_empty(),
            "legacy fallback 仍存在时迁移会让 source 复活第二个 owner"
        );
    }
}
