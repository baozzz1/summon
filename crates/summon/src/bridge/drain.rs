//! Hook spool 的本地单写者归约。不联网、不调用 tmux；仅在同 SID 新进程碰撞时对旧
//! fingerprint 做只读 exact OS probe，证明 crash 后才允许 generation rollover。

use crate::bridge::store::{BridgeStore, HookEnvelope, HookEvent, HOOK_ENVELOPE_VERSION};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;
use summon_core::bridge::{
    BridgeError, ExternalHookApply, ExternalHookEvent, ExternalHookObservation,
};

const MAX_DRAIN_BATCH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrainStats {
    pub applied: u64,
    pub duplicate: u64,
    pub stale: u64,
    pub conflict: u64,
    pub dead: u64,
}

/// 每个 tick 按 store 分配的全局 sequence 处理 backlog。坏事件移 dead 后继续；合法事件若
/// state commit 失败则停在 pending 并终止本 tick，避免后续 sequence 越过未提交事件。
pub fn drain_tick(store: &BridgeStore) -> Result<DrainStats> {
    drain_tick_with_probe(store, &crate::bridge::process_probe::probe_exact_process)
}

fn drain_tick_with_probe<F>(store: &BridgeStore, probe: &F) -> Result<DrainStats>
where
    F: Fn(
        &summon_core::bridge::ProcessFingerprint,
    ) -> Result<crate::bridge::process_probe::ExactProcessStatus>,
{
    let mut stats = DrainStats::default();
    let mut batch = Vec::new();
    for path in store.pending_paths()?.into_iter().take(MAX_DRAIN_BATCH) {
        let sequence = match BridgeStore::event_sequence(&path) {
            Some(sequence) if sequence > 0 => sequence,
            _ => {
                move_bad(store, &path, &mut stats)?;
                continue;
            }
        };
        let envelope = match store.read_hook(&path) {
            Ok(envelope) => envelope,
            Err(_) => {
                move_bad(store, &path, &mut stats)?;
                continue;
            }
        };
        if !envelope_matches_path(&envelope, &path)
            || envelope.schema_version != HOOK_ENVELOPE_VERSION
        {
            move_bad(store, &path, &mut stats)?;
            continue;
        }
        batch.push((path, into_observation(envelope, sequence)));
    }

    // 一批 event 只做一次 flocked load→save，冷启动成本与 backlog 线性。若领域校验发现坏
    // observation，本次不提交任何后序 event；先把坏项移 dead，下一 tick 再从余下最小序列开始。
    while !batch.is_empty() {
        let mut invalid_path = None;
        let rollover_sessions = batch
            .iter()
            .filter(|(_, observation)| {
                matches!(observation.event, ExternalHookEvent::SessionStart { .. })
            })
            .map(|(_, observation)| observation.session.storage_key())
            .collect::<BTreeSet<_>>();
        let gate_targets = store
            .load_state()?
            .into_iter()
            .flat_map(|state| state.slots.into_values())
            .filter_map(|slot| slot.binding().cloned())
            .filter(|binding| rollover_sessions.contains(&binding.session.storage_key()))
            .map(|binding| binding.target)
            .collect::<Vec<_>>();
        let gated_keys = gate_targets
            .iter()
            .map(|target| target.storage_key())
            .collect::<BTreeSet<_>>();
        let outcomes = match crate::bridge::runtime::with_chat_delivery_gates(&gate_targets, || {
            store.with_state(|state| {
                // 目标可能在“无锁快照→取得 gates”之间被新建/替换；这种情况不在未持 gate 下
                // 猜测，整批不提交，下一 tick 重新快照即可。
                anyhow::ensure!(
                    state
                        .slots
                        .values()
                        .filter_map(|slot| slot.binding())
                        .filter(|binding| {
                            rollover_sessions.contains(&binding.session.storage_key())
                        })
                        .all(|binding| gated_keys.contains(&binding.target.storage_key())),
                    summon_i18n::format!("r9421ead26b")
                );
                let mut outcomes = Vec::with_capacity(batch.len());
                for (path, observation) in &batch {
                    retire_crashed_epoch_before_start(state, observation, probe);
                    match state.apply_external_hook(observation) {
                        Ok(outcome) => outcomes.push(outcome),
                        Err(BridgeError::InvalidObservation) => {
                            invalid_path = Some(path.clone());
                            return Err(anyhow::Error::new(BridgeError::InvalidObservation));
                        }
                        Err(error) => return Err(anyhow::Error::new(error)),
                    }
                }
                Ok(outcomes)
            })
        }) {
            Ok(outcomes) => outcomes,
            Err(_error) if invalid_path.is_some() => {
                let path = invalid_path.expect("guarded by is_some");
                move_bad(store, &path, &mut stats)?;
                batch.retain(|(candidate, _)| candidate != &path);
                continue;
            }
            Err(error) => return Err(error).context(summon_i18n::text!("r6e8fb3f62a")),
        };

        // 整批 state 已经 durable commit 后才逐个归档。中途失败时余项仍在 pending，下次全是
        // Duplicate，不会重复生成 mirror turn。
        for ((path, observation), outcome) in batch.iter().zip(outcomes) {
            store.archive_pending(path, observation)?;
            match outcome {
                ExternalHookApply::Applied => stats.applied = stats.applied.saturating_add(1),
                ExternalHookApply::Duplicate => stats.duplicate = stats.duplicate.saturating_add(1),
                ExternalHookApply::Stale => stats.stale = stats.stale.saturating_add(1),
                ExternalHookApply::Conflict => stats.conflict = stats.conflict.saturating_add(1),
            }
        }
        break;
    }
    Ok(stats)
}

/// SessionEnd 不是 kill -9 的可靠前提。只有 OS exact probe 证明旧 fingerprint 已消失或 PID
/// 已复用，才把旧 generation 标 Ended，让同 SID 的新 SessionStart 走正常 rollover；probe
/// 失败或旧进程仍活一律交给 core 标 Conflict，不能“最后写入者获胜”。
fn retire_crashed_epoch_before_start<F>(
    state: &mut summon_core::bridge::BridgeState,
    observation: &ExternalHookObservation,
    probe: &F,
) where
    F: Fn(
        &summon_core::bridge::ProcessFingerprint,
    ) -> Result<crate::bridge::process_probe::ExactProcessStatus>,
{
    if !matches!(observation.event, ExternalHookEvent::SessionStart { .. }) {
        return;
    }
    if state.processed_events.contains_key(&observation.event_id)
        || state
            .session(&observation.session)
            .is_some_and(|record| observation.sequence <= record.last_applied_sequence)
    {
        return;
    }
    let Some(existing) = state.session(&observation.session).cloned() else {
        return;
    };
    if existing.lifecycle == summon_core::bridge::ExternalLifecycle::Ended
        || existing.process == observation.process
    {
        return;
    }
    let Some(old_process) = existing
        .process
        .as_ref()
        .filter(|process| process.is_complete())
    else {
        return;
    };
    let retired = matches!(
        probe(old_process),
        Ok(crate::bridge::process_probe::ExactProcessStatus::Gone)
            | Ok(crate::bridge::process_probe::ExactProcessStatus::PidReused(
                _
            ))
    );
    if retired {
        let _ = state.end_session(
            &existing.key,
            existing.generation,
            Some(old_process),
            observation.observed_at_ms,
        );
    }
}

fn move_bad(store: &BridgeStore, path: &Path, stats: &mut DrainStats) -> Result<()> {
    store.dead_letter(path)?;
    stats.dead = stats.dead.saturating_add(1);
    Ok(())
}

fn envelope_matches_path(envelope: &HookEnvelope, path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.split_once('-'))
        .is_some_and(|(_, event_id)| event_id == envelope.event_id)
}

fn into_observation(envelope: HookEnvelope, sequence: u64) -> ExternalHookObservation {
    let event = match envelope.event {
        HookEvent::SessionStart { source, .. } => ExternalHookEvent::SessionStart { source },
        HookEvent::CwdChanged { old_cwd, new_cwd } => {
            ExternalHookEvent::CwdChanged { old_cwd, new_cwd }
        }
        HookEvent::UserPromptSubmit { prompt_id, prompt } => {
            ExternalHookEvent::UserPromptSubmit { prompt_id, prompt }
        }
        HookEvent::Stop {
            prompt_id,
            last_assistant_message,
            stop_hook_active,
            has_pending_wakeups,
        } => ExternalHookEvent::Stop {
            prompt_id,
            last_assistant_message,
            stop_hook_active,
            has_pending_wakeups,
        },
        HookEvent::StopFailure {
            prompt_id,
            error,
            error_details,
            last_assistant_message,
        } => ExternalHookEvent::StopFailure {
            prompt_id,
            error,
            error_details,
            last_assistant_message,
        },
        HookEvent::SessionEnd { reason } => ExternalHookEvent::SessionEnd { reason },
    };
    ExternalHookObservation {
        event_id: envelope.event_id,
        sequence,
        observed_at_ms: envelope.observed_at_ms,
        session: envelope.session,
        transcript_path: envelope.transcript_path,
        cwd: envelope.cwd,
        process: envelope.process,
        resume_profile: envelope.resume_profile,
        content_permit: envelope.content_permit,
        managed_takeover: envelope.managed_takeover,
        event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::store::HookEvent;
    use summon_core::bridge::{
        BridgeMode, ChatBinding, ChatSlot, ContentPermit, ExternalVisibleRole, ProcessFingerprint,
        ResumeProfile, ScopedChatKey, SessionKey, SessionLease,
    };

    fn process() -> ProcessFingerprint {
        ProcessFingerprint {
            pid: 42,
            start_time: "start".into(),
            executable: "/opt/claude".into(),
        }
    }

    fn envelope(id: &str, at: u64, event: HookEvent) -> HookEnvelope {
        HookEnvelope {
            schema_version: HOOK_ENVELOPE_VERSION,
            event_id: id.into(),
            observed_at_ms: at,
            session: SessionKey::new("/tmp/config", "sid"),
            transcript_path: "/tmp/config/projects/demo/sid.jsonl".into(),
            cwd: "/tmp/demo".into(),
            process: Some(process()),
            resume_profile: ResumeProfile {
                claude_bin: "/opt/claude".into(),
                home: "/tmp/home".into(),
                config_dir: "/tmp/config".into(),
                version: "2.1.207".into(),
                safe_args: vec![],
                unsupported_args: vec![],
                process_verified: true,
                // strict handoff 仍未验证；mirror origin 只依赖 process proof。
                environment_verified: false,
                config_source: summon_core::bridge::ClaudeConfigSource::Default,
                provider: summon_core::bridge::ClaudeProvider::Default,
            },
            content_permit: None,
            managed_takeover: false,
            handoff_ack: None,
            event,
        }
    }

    fn bind(store: &BridgeStore) -> ContentPermit {
        let target = ScopedChatKey::new("app", "agent", "chat");
        let binding = ChatBinding {
            id: "binding".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: SessionKey::new("/tmp/config", "sid"),
            mode: BridgeMode::Mirror,
            actor_open_id: "owner".into(),
            created_at_ms: 1,
            revision: 7,
        };
        store
            .with_state(|state| {
                state
                    .slots
                    .insert(target.storage_key(), ChatSlot::MirrorBound(binding.clone()));
                state.session_leases.insert(
                    binding.session.storage_key(),
                    SessionLease {
                        binding_id: binding.id.clone(),
                        target: binding.target.clone(),
                    },
                );
                state
                    .delivery_sequence_cursors
                    .insert(binding.id.clone(), 1);
                Ok(())
            })
            .unwrap();
        ContentPermit {
            binding_id: binding.id,
            binding_revision: binding.revision,
            generation: 1,
        }
    }

    #[test]
    fn backlog_reduces_in_sequence_and_archives_only_after_state_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store
            .enqueue_hook(&envelope(
                "start",
                10,
                HookEvent::SessionStart {
                    source: "startup".into(),
                    model: None,
                },
            ))
            .unwrap();
        let start_stats = drain_tick(&store).unwrap();
        assert_eq!(start_stats.applied, 1);
        // 正文 permit 必须来自已归约的 verified active epoch；测试不再手工绕过 store 隐私闸。
        let permit = bind(&store);
        let mut prompt = envelope(
            "prompt",
            20,
            HookEvent::UserPromptSubmit {
                prompt_id: Some("p1".into()),
                prompt: Some("问题".into()),
            },
        );
        prompt.content_permit = Some(permit.clone());
        store.enqueue_hook(&prompt).unwrap();
        let mut stop = envelope(
            "stop",
            30,
            HookEvent::Stop {
                prompt_id: Some("p1".into()),
                last_assistant_message: Some("回答".into()),
                stop_hook_active: false,
                has_pending_wakeups: false,
            },
        );
        stop.content_permit = Some(permit);
        store.enqueue_hook(&stop).unwrap();

        let stats = drain_tick(&store).unwrap();
        assert_eq!(stats.applied, 2);
        assert!(store.pending_paths().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(store.archive_dir()).unwrap().count(), 3);
        let archived = std::fs::read_dir(store.archive_dir())
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<String>();
        assert!(!archived.contains("问题"));
        assert!(!archived.contains("回答"));
        let state = store.load_state().unwrap().unwrap();
        let turn = state.pending_mirror_turns.get("stop").unwrap();
        assert_eq!(turn.messages[0].role, ExternalVisibleRole::User);
        assert_eq!(turn.messages[0].text, "问题");
        assert_eq!(turn.messages[1].role, ExternalVisibleRole::Assistant);
        assert_eq!(turn.messages[1].text, "回答");
    }

    #[test]
    fn corrupt_event_moves_to_dead_without_blocking_later_event() {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.pending_paths().unwrap();
        let bad = store.pending_dir().join("00000000000000000001-bad.json");
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&bad)
            .unwrap();
        file.write_all(br#"{"prompt":"VERY_SECRET",broken"#)
            .unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .unwrap();
        store
            .enqueue_hook(&envelope(
                "good",
                10,
                HookEvent::SessionStart {
                    source: "startup".into(),
                    model: None,
                },
            ))
            .unwrap();

        let stats = drain_tick(&store).unwrap();
        assert_eq!(stats.dead, 1);
        assert_eq!(stats.applied, 1);
        assert_eq!(std::fs::read_dir(store.dead_dir()).unwrap().count(), 1);
        let dead = std::fs::read_dir(store.dead_dir())
            .unwrap()
            .next()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .unwrap();
        assert!(!dead.contains("VERY_SECRET"));
        assert!(store.pending_paths().unwrap().is_empty());
    }

    #[test]
    fn malformed_event_filename_is_quarantined_without_blocking_later_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.pending_paths().unwrap();
        let malformed = store.pending_dir().join("00000000000000000000.json");
        std::fs::write(&malformed, b"VERY_SECRET_MALFORMED_FILENAME_BODY").unwrap();
        store
            .enqueue_hook(&envelope(
                "good-after-malformed-name",
                10,
                HookEvent::SessionStart {
                    source: "startup".into(),
                    model: None,
                },
            ))
            .unwrap();

        let stats = drain_tick(&store).unwrap();
        assert_eq!(stats.dead, 1);
        assert_eq!(stats.applied, 1);
        assert!(store.pending_paths().unwrap().is_empty());
        let dead = std::fs::read_dir(store.dead_dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(dead
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("malformed-") && name.ends_with(".json")));
        let tombstone = std::fs::read_to_string(dead).unwrap();
        assert!(!tombstone.contains("VERY_SECRET"));
        assert!(!tombstone.contains("00000000000000000000"));
    }

    #[test]
    fn generation_rollover_waits_for_target_delivery_gate_without_blocking_spool() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(BridgeStore::new(dir.path().join("bridge")));
        store
            .enqueue_hook(&envelope(
                "start-old",
                10,
                HookEvent::SessionStart {
                    source: "startup".into(),
                    model: None,
                },
            ))
            .unwrap();
        drain_tick(&store).unwrap();
        bind(&store);
        store
            .enqueue_hook(&envelope(
                "end-old",
                20,
                HookEvent::SessionEnd { reason: None },
            ))
            .unwrap();
        drain_tick(&store).unwrap();

        let target = ScopedChatKey::new("app", "agent", "chat");
        let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        let holder_entered = std::sync::Arc::clone(&entered);
        let holder_release = std::sync::Arc::clone(&release);
        let holder_target = target.clone();
        let holder = std::thread::spawn(move || {
            crate::bridge::runtime::with_chat_delivery_gate(&holder_target, || {
                holder_entered.wait();
                holder_release.wait();
                Ok(())
            })
            .unwrap();
        });
        entered.wait();

        let mut rollover = envelope(
            "start-new",
            30,
            HookEvent::SessionStart {
                source: "startup".into(),
                model: None,
            },
        );
        rollover.process = Some(ProcessFingerprint {
            pid: 43,
            start_time: "new-start".into(),
            executable: "/opt/claude".into(),
        });
        // gate 只约束 reducer 的解绑 CAS；Hook 2s 热路径的 spool 写入不持 gate。
        store.enqueue_hook(&rollover).unwrap();
        let drain_store = std::sync::Arc::clone(&store);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let reducer = std::thread::spawn(move || {
            done_tx.send(drain_tick(&drain_store)).unwrap();
        });
        assert!(done_rx
            .recv_timeout(std::time::Duration::from_millis(150))
            .is_err());
        assert!(store
            .pending_paths()
            .unwrap()
            .iter()
            .any(|path| path.to_string_lossy().contains("start-new")));

        release.wait();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        holder.join().unwrap();
        reducer.join().unwrap();
        assert!(store.load_state().unwrap().unwrap().slot(&target).is_none());
    }

    #[test]
    fn crashed_source_exact_probe_allows_same_sid_verified_resume_rollover() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store
            .enqueue_hook(&envelope(
                "start-crashed",
                10,
                HookEvent::SessionStart {
                    source: "startup".into(),
                    model: None,
                },
            ))
            .unwrap();
        drain_tick_with_probe(&store, &|_| {
            Ok(crate::bridge::process_probe::ExactProcessStatus::AliveExact)
        })
        .unwrap();
        bind(&store);

        let old = process();
        let new = ProcessFingerprint {
            pid: 84,
            start_time: "resume-start".into(),
            executable: "/opt/claude".into(),
        };
        let mut resumed = envelope(
            "start-resumed",
            20,
            HookEvent::SessionStart {
                source: "resume".into(),
                model: None,
            },
        );
        resumed.process = Some(new.clone());
        store.enqueue_hook(&resumed).unwrap();

        let stats = drain_tick_with_probe(&store, &|expected| {
            assert_eq!(expected, &old);
            Ok(crate::bridge::process_probe::ExactProcessStatus::Gone)
        })
        .unwrap();
        assert_eq!(stats.applied, 1);
        assert_eq!(stats.conflict, 0);
        let state = store.load_state().unwrap().unwrap();
        let record = state
            .session(&SessionKey::new("/tmp/config", "sid"))
            .unwrap();
        assert_eq!(record.generation, 2);
        assert_eq!(record.process.as_ref(), Some(&new));
        assert_eq!(
            record.lifecycle,
            summon_core::bridge::ExternalLifecycle::ActiveIdle
        );
        let target = ScopedChatKey::new("app", "agent", "chat");
        assert!(matches!(
            state.slot(&target),
            Some(ChatSlot::MirrorBound(_))
        ));
        assert_eq!(
            store
                .content_permit(&record.key)
                .unwrap()
                .unwrap()
                .generation,
            2
        );
    }

    #[test]
    fn live_or_unreadable_old_process_keeps_same_sid_resume_conflicted() {
        for probe_errors in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = BridgeStore::new(dir.path().join("bridge"));
            store
                .enqueue_hook(&envelope(
                    "start-old",
                    10,
                    HookEvent::SessionStart {
                        source: "startup".into(),
                        model: None,
                    },
                ))
                .unwrap();
            drain_tick(&store).unwrap();
            let old = process();
            let mut resumed = envelope(
                "start-collision",
                20,
                HookEvent::SessionStart {
                    source: "resume".into(),
                    model: None,
                },
            );
            resumed.process = Some(ProcessFingerprint {
                pid: 84,
                start_time: "new-start".into(),
                executable: "/opt/claude".into(),
            });
            store.enqueue_hook(&resumed).unwrap();
            let stats = drain_tick_with_probe(&store, &|expected| {
                assert_eq!(expected, &old);
                if probe_errors {
                    anyhow::bail!("probe denied")
                }
                Ok(crate::bridge::process_probe::ExactProcessStatus::AliveExact)
            })
            .unwrap();
            assert_eq!(stats.conflict, 1);
            let state = store.load_state().unwrap().unwrap();
            let record = state
                .session(&SessionKey::new("/tmp/config", "sid"))
                .unwrap();
            assert_eq!(record.generation, 1);
            assert_eq!(record.process.as_ref(), Some(&old));
            assert_eq!(
                record.lifecycle,
                summon_core::bridge::ExternalLifecycle::Conflict
            );
            assert_eq!(
                record.locator.transcript_path,
                "/tmp/config/projects/demo/sid.jsonl"
            );
        }
    }

    #[test]
    fn duplicate_or_stale_start_replay_never_retires_epoch_before_core_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store
            .enqueue_hook(&envelope(
                "start-old",
                10,
                HookEvent::SessionStart {
                    source: "startup".into(),
                    model: None,
                },
            ))
            .unwrap();
        drain_tick(&store).unwrap();
        let old = process();
        let mut resumed = envelope(
            "start-conflict",
            20,
            HookEvent::SessionStart {
                source: "resume".into(),
                model: None,
            },
        );
        resumed.process = Some(ProcessFingerprint {
            pid: 84,
            start_time: "new-start".into(),
            executable: "/opt/claude".into(),
        });
        let observation = into_observation(resumed, 20);
        store
            .with_state(|state| {
                assert_eq!(
                    state.apply_external_hook(&observation)?,
                    summon_core::bridge::ExternalHookApply::Conflict
                );
                let must_not_probe = |_: &ProcessFingerprint| -> Result<
                    crate::bridge::process_probe::ExactProcessStatus,
                > {
                    panic!("dedupe 前置闸命中时不得探测或改状态")
                };
                retire_crashed_epoch_before_start(state, &observation, &must_not_probe);
                assert_eq!(
                    state.apply_external_hook(&observation)?,
                    summon_core::bridge::ExternalHookApply::Duplicate
                );

                let mut stale = observation.clone();
                stale.event_id = "start-stale".into();
                retire_crashed_epoch_before_start(state, &stale, &must_not_probe);
                assert_eq!(
                    state.apply_external_hook(&stale)?,
                    summon_core::bridge::ExternalHookApply::Stale
                );
                let record = state.session(&observation.session).unwrap();
                assert_eq!(record.generation, 1);
                assert_eq!(record.process.as_ref(), Some(&old));
                assert_eq!(
                    record.lifecycle,
                    summon_core::bridge::ExternalLifecycle::Conflict
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn bounded_large_turn_backlog_drains_and_reconciles_without_state_livelock() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox"));
        store
            .enqueue_hook(&envelope(
                "start-backlog",
                10,
                HookEvent::SessionStart {
                    source: "startup".into(),
                    model: None,
                },
            ))
            .unwrap();
        drain_tick(&store).unwrap();
        let permit = bind(&store);
        let user = summon_core::bridge::clip_external_body_for_state(
            &"\0<&问\n".repeat(20_000),
            summon_core::bridge::EXTERNAL_PROMPT_STATE_JSON_BUDGET,
        );
        let assistant = summon_core::bridge::clip_external_body_for_state(
            &"\0<&答\n".repeat(40_000),
            summon_core::bridge::EXTERNAL_ASSISTANT_STATE_JSON_BUDGET,
        );
        const TURNS: u64 = 40;
        for index in 0..TURNS {
            let mut prompt = envelope(
                &format!("large-prompt-{index}"),
                20 + index * 2,
                HookEvent::UserPromptSubmit {
                    prompt_id: Some(format!("p-{index}")),
                    prompt: Some(user.clone()),
                },
            );
            prompt.content_permit = Some(permit.clone());
            store.enqueue_hook(&prompt).unwrap();
            let mut stop = envelope(
                &format!("large-stop-{index}"),
                21 + index * 2,
                HookEvent::Stop {
                    prompt_id: Some(format!("p-{index}")),
                    last_assistant_message: Some(assistant.clone()),
                    stop_hook_active: false,
                    has_pending_wakeups: false,
                },
            );
            stop.content_permit = Some(permit.clone());
            store.enqueue_hook(&stop).unwrap();
        }

        let mut applied = 0u64;
        while !store.pending_paths().unwrap().is_empty() {
            applied = applied.saturating_add(drain_tick(&store).unwrap().applied);
            crate::bridge::reconcile::reconcile_once(&store, &outbox, 1_000).unwrap();
        }
        assert_eq!(applied, TURNS * 2);
        let state = store.load_state().unwrap().unwrap();
        assert!(state.pending_mirror_turns.is_empty());
        assert_eq!(state.mirror_export_receipts.len(), TURNS as usize);
        assert_eq!(
            outbox.pending_events_checked().unwrap().len(),
            TURNS as usize
        );
        assert!(
            std::fs::metadata(store.state_path()).unwrap().len()
                <= crate::bridge::store::MAX_STATE_BYTES as u64
        );
    }
}
