//! Bridge 本地 crash reconciler：把领域中间态补齐为 durable outbox/receipt。
//!
//! 本模块只读写 bridge state 与 outbox，不联网、不探测进程、更不触碰 tmux。网络 delivery 会在
//! 真发送前再次做完整 exact gate；这里的职责只是关闭 enqueue/state 两阶段之间的崩溃窗口。

use crate::bridge::store::BridgeStore;
use crate::runtime::outbox_store::{
    CorruptBridgePending, EnqueueOnceOutcome, OutboxLocation, OutboxStore,
};
use anyhow::{bail, Context as _, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use summon_core::bridge::{
    ChatSlot, ContextExportReceipt, ExternalVisibleRole, MirrorExportReceipt, PendingMirrorTurn,
    TicketStatus,
};
use summon_core::outbox::{
    bridge_delivery_id, BridgeDeliveryFence, BridgeDeliveryKind, OutboxEvent, OutboxKind,
};

const SESSION_GC_INTERVAL_MS: u64 = 5 * 60 * 1_000;
const STALE_SESSION_AGE_MS: u64 = 24 * 60 * 60 * 1_000;
const ENDED_SESSION_RETENTION_MS: u64 = 7 * STALE_SESSION_AGE_MS;
const MAX_UNREFERENCED_SESSIONS: usize = 1_024;
const MAX_SESSION_GC_BATCH: usize = 32;
static LAST_SESSION_GC_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileStats {
    pub expired: u64,
    pub audits_enqueued: u64,
    pub turns_exported: u64,
    pub contexts_recovered: u64,
    pub dead_dropped: u64,
    pub sessions_pruned: u64,
}

pub fn reconcile_once(
    bridge: &BridgeStore,
    outbox: &OutboxStore,
    now_ms: u64,
) -> Result<ReconcileStats> {
    let scan = outbox.scan_pending()?;
    let mut stats = ReconcileStats::default();
    reconcile_corrupt_bridge_files(bridge, outbox, &scan.corrupt_bridge, &mut stats)?;
    if session_gc_due(now_ms) {
        stats.sessions_pruned = gc_sessions_once(bridge, now_ms)?;
    }
    // 这里只做「本 tick 有没有活要干」的判断,纯只读。空闲时 state 每 tick 都一样,走
    // 缓存读避免每 100ms 白解析一份 200KB+ 的 state.json;真正的写仍在下面各自的 with_state 里。
    let Some(initial) = bridge.load_state_cached()? else {
        return Ok(stats);
    };
    let has_reconcile_work = initial.tickets.values().any(|ticket| {
        matches!(
            &ticket.status,
            TicketStatus::Pending
                | TicketStatus::Activating { .. }
                | TicketStatus::Committed { .. }
        )
    }) || !initial.slots.is_empty()
        || !initial.pending_mirror_turns.is_empty()
        || !initial.activation_audit_receipts.is_empty()
        || !initial.mirror_export_receipts.is_empty()
        || !initial.context_export_receipts.is_empty();
    if !has_reconcile_work {
        return Ok(stats);
    }
    // reconcile 每 100ms 运行一次。没有到期 ticket 时不能走 with_state，否则即使完全空闲也会
    // 每 tick 序列化 + fsync 整份 state，既放大磁盘写入，也会与 Hook writer 争同一把 flock。
    let has_expired_pending = initial.tickets.values().any(|ticket| {
        matches!(&ticket.status, TicketStatus::Pending) && ticket.expires_at_ms < now_ms
    });
    let expired = if has_expired_pending {
        bridge.with_state(|state| Ok(state.expire_pending(now_ms).len() as u64))?
    } else {
        0
    };
    stats.expired = expired;
    reconcile_activations(bridge, outbox, &mut stats)?;
    reconcile_dead_committed_activations(bridge, outbox, &mut stats)?;
    // durable context event 可能已占用“旧 cursor + 1”但 state 尚未提交；必须先恢复 cursor，
    // 再给 Hook turn 分配后续 sequence，不能让两类事件在 crash recovery 中撞号。
    reconcile_context_receipts(bridge, outbox, &mut stats)?;
    reconcile_finalized_receipts(bridge, outbox)?;
    reconcile_turns(bridge, outbox, &mut stats)?;
    Ok(stats)
}

fn session_gc_due(now_ms: u64) -> bool {
    let mut last = LAST_SESSION_GC_MS.load(Ordering::Relaxed);
    loop {
        if now_ms.saturating_sub(last) < SESSION_GC_INTERVAL_MS {
            return false;
        }
        match LAST_SESSION_GC_MS.compare_exchange_weak(
            last,
            now_ms,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(current) => last = current,
        }
    }
}

fn gc_sessions_once(bridge: &BridgeStore, now_ms: u64) -> Result<u64> {
    let Some(state) = bridge.load_state()? else {
        return Ok(0);
    };
    let mut unreferenced = state
        .sessions
        .values()
        .filter(|record| !state.session_is_referenced(&record.key))
        .cloned()
        .collect::<Vec<_>>();
    unreferenced.sort_by_key(|record| (record.last_seen_ms, record.key.storage_key()));
    let pressure_excess = unreferenced.len().saturating_sub(MAX_UNREFERENCED_SESSIONS);
    let mut removals = Vec::new();
    for (index, record) in unreferenced.into_iter().enumerate() {
        let age = now_ms.saturating_sub(record.last_seen_ms);
        let under_pressure = index < pressure_excess;
        let remove = match record.lifecycle {
            summon_core::bridge::ExternalLifecycle::Ended => {
                under_pressure || age >= ENDED_SESSION_RETENTION_MS
            }
            summon_core::bridge::ExternalLifecycle::Unverified
            | summon_core::bridge::ExternalLifecycle::Conflict
            | summon_core::bridge::ExternalLifecycle::ActiveIdle
            | summon_core::bridge::ExternalLifecycle::ActiveBusy
                if under_pressure || age >= STALE_SESSION_AGE_MS =>
            {
                match record.process.as_ref() {
                    Some(process) => matches!(
                        crate::bridge::process_probe::probe_exact_process(process),
                        Ok(crate::bridge::process_probe::ExactProcessStatus::Gone)
                            | Ok(crate::bridge::process_probe::ExactProcessStatus::PidReused(
                                _
                            ))
                    ),
                    None => matches!(
                        record.lifecycle,
                        summon_core::bridge::ExternalLifecycle::Unverified
                            | summon_core::bridge::ExternalLifecycle::Conflict
                    ),
                }
            }
            _ => false,
        };
        if remove {
            removals.push((record.key, record.generation));
            if removals.len() >= MAX_SESSION_GC_BATCH {
                break;
            }
        }
    }
    if removals.is_empty() {
        return Ok(0);
    }
    bridge.with_state(|state| {
        let mut removed = 0u64;
        for (key, generation) in &removals {
            match state.remove_unreferenced_session(key, *generation) {
                Ok(true) => removed = removed.saturating_add(1),
                Ok(false) | Err(summon_core::bridge::BridgeError::SessionLeased) => {}
                Err(error) => return Err(anyhow::Error::new(error)),
            }
        }
        Ok(removed)
    })
}

#[derive(Debug)]
enum CorruptBridgeAction {
    CancelActivation {
        target: summon_core::bridge::ScopedChatKey,
        ticket_id: String,
        binding_id: String,
    },
    StopMirror {
        target: summon_core::bridge::ScopedChatKey,
        binding_id: String,
        binding_revision: u64,
    },
    FinishTurnReceipt {
        target: summon_core::bridge::ScopedChatKey,
        receipt_id: String,
        outbox_id: String,
    },
    DiscardPendingTurn {
        target: summon_core::bridge::ScopedChatKey,
        turn_id: String,
        binding_id: String,
        binding_revision: u64,
    },
    FinishContextReceipt {
        target: summon_core::bridge::ScopedChatKey,
        receipt_id: String,
        outbox_id: String,
    },
    Orphan,
}

/// 坏 Bridge 文件不能被解析，因此只允许用文件名与 durable state 中可重算的确定性 id 对照。
/// 先在目标群 delivery gate 内收口 state，再写无正文 quarantine tombstone；若中途崩溃，下次
/// 会从已收口 state 把它识别为 orphan，绝不会恢复或发送不可验证的正文。
fn reconcile_corrupt_bridge_files(
    bridge: &BridgeStore,
    outbox: &OutboxStore,
    corrupt_files: &[CorruptBridgePending],
    stats: &mut ReconcileStats,
) -> Result<()> {
    for corrupt in corrupt_files {
        let action = match corrupt.id.as_deref() {
            Some(corrupt_id) => corrupt_bridge_action(bridge, corrupt_id)?,
            None => CorruptBridgeAction::Orphan,
        };
        match action {
            CorruptBridgeAction::CancelActivation {
                target,
                ticket_id,
                binding_id,
            } => crate::bridge::runtime::with_chat_delivery_gate(&target, || {
                bridge.with_state(|state| {
                    let exact = state.tickets.get(&ticket_id).is_some_and(|ticket| {
                        matches!(
                            &ticket.status,
                            TicketStatus::Activating {
                                binding_id: current,
                                mode: summon_core::bridge::BridgeMode::Mirror,
                                ..
                            } if current == &binding_id
                        )
                    });
                    if exact {
                        state
                            .cancel_ticket(&ticket_id)
                            .map_err(anyhow::Error::new)?;
                    }
                    Ok(())
                })
            })?,
            CorruptBridgeAction::StopMirror {
                target,
                binding_id,
                binding_revision,
            } => crate::bridge::runtime::with_chat_delivery_gate(&target, || {
                bridge.with_state(|state| {
                    if matches!(
                        state.slot(&target),
                        Some(ChatSlot::MirrorBound(binding))
                            if binding.id == binding_id && binding.revision == binding_revision
                    ) {
                        state
                            .stop_mirror(&target, &binding_id, binding_revision)
                            .map_err(anyhow::Error::new)?;
                    }
                    Ok(())
                })
            })?,
            CorruptBridgeAction::FinishTurnReceipt {
                target,
                receipt_id,
                outbox_id,
            } => crate::bridge::runtime::with_chat_delivery_gate(&target, || {
                bridge.with_state(|state| {
                    state
                        .finish_mirror_export(&receipt_id, &outbox_id)
                        .map(|_| ())
                        .map_err(anyhow::Error::new)
                })
            })?,
            CorruptBridgeAction::DiscardPendingTurn {
                target,
                turn_id,
                binding_id,
                binding_revision,
            } => crate::bridge::runtime::with_chat_delivery_gate(&target, || {
                bridge.with_state(|state| {
                    state
                        .discard_pending_mirror_turn(&turn_id, &binding_id, binding_revision)
                        .map(|_| ())
                        .map_err(anyhow::Error::new)
                })
            })?,
            CorruptBridgeAction::FinishContextReceipt {
                target,
                receipt_id,
                outbox_id,
            } => crate::bridge::runtime::with_chat_delivery_gate(&target, || {
                bridge.with_state(|state| {
                    state
                        .finish_context_export(&receipt_id, &outbox_id)
                        .map(|_| ())
                        .map_err(anyhow::Error::new)
                })
            })?,
            CorruptBridgeAction::Orphan => {}
        }
        outbox.quarantine_corrupt_bridge(corrupt)?;
        stats.dead_dropped = stats.dead_dropped.saturating_add(1);
    }
    Ok(())
}

fn corrupt_bridge_action(bridge: &BridgeStore, corrupt_id: &str) -> Result<CorruptBridgeAction> {
    let Some(state) = bridge.load_state()? else {
        return Ok(CorruptBridgeAction::Orphan);
    };
    for ticket in state.tickets.values() {
        let binding_id = match &ticket.status {
            TicketStatus::Activating {
                binding_id,
                mode: summon_core::bridge::BridgeMode::Mirror,
                ..
            }
            | TicketStatus::Committed {
                binding_id,
                mode: summon_core::bridge::BridgeMode::Mirror,
                ..
            } => binding_id,
            _ => continue,
        };
        let expected = bridge_delivery_id(
            BridgeDeliveryKind::ActivationAudit,
            &format!("activation-{binding_id}"),
        );
        if expected != corrupt_id {
            continue;
        }
        return match (&ticket.status, state.slot(&ticket.target)) {
            (TicketStatus::Activating { .. }, Some(ChatSlot::Activating(binding)))
                if binding.id == *binding_id =>
            {
                Ok(CorruptBridgeAction::CancelActivation {
                    target: ticket.target.clone(),
                    ticket_id: ticket.id.clone(),
                    binding_id: binding_id.clone(),
                })
            }
            (TicketStatus::Committed { .. }, Some(ChatSlot::MirrorBound(binding)))
                if binding.id == *binding_id =>
            {
                Ok(CorruptBridgeAction::StopMirror {
                    target: ticket.target.clone(),
                    binding_id: binding.id.clone(),
                    binding_revision: binding.revision,
                })
            }
            _ => Ok(CorruptBridgeAction::Orphan),
        };
    }
    if let Some(receipt) = state
        .mirror_export_receipts
        .values()
        .find(|receipt| receipt.outbox_id == corrupt_id)
    {
        return Ok(CorruptBridgeAction::FinishTurnReceipt {
            target: receipt.target.clone(),
            receipt_id: receipt.id.clone(),
            outbox_id: receipt.outbox_id.clone(),
        });
    }
    if let Some(turn) = state.pending_mirror_turns.values().find(|turn| {
        bridge_delivery_id(
            BridgeDeliveryKind::ExternalTurn,
            &format!("turn-{}", turn.id),
        ) == corrupt_id
    }) {
        return Ok(CorruptBridgeAction::DiscardPendingTurn {
            target: turn.target.clone(),
            turn_id: turn.id.clone(),
            binding_id: turn.binding_id.clone(),
            binding_revision: turn.binding_revision,
        });
    }
    if let Some(receipt) = state
        .context_export_receipts
        .values()
        .find(|receipt| receipt.outbox_id == corrupt_id)
    {
        return Ok(CorruptBridgeAction::FinishContextReceipt {
            target: receipt.target.clone(),
            receipt_id: receipt.id.clone(),
            outbox_id: receipt.outbox_id.clone(),
        });
    }
    Ok(CorruptBridgeAction::Orphan)
}

/// 群审计已成功、MirrorBound 已提交，但私聊控制卡持续失败时 activation event 会进 dead。
/// daemon 可能恰在 dead tombstone durable、on_dead CAS 前崩溃，因此 reconciler 必须补做 exact stop。
fn reconcile_dead_committed_activations(
    bridge: &BridgeStore,
    outbox: &OutboxStore,
    stats: &mut ReconcileStats,
) -> Result<()> {
    let Some(state) = bridge.load_state()? else {
        return Ok(());
    };
    let committed = state
        .tickets
        .values()
        .filter_map(|ticket| {
            let binding_id = match &ticket.status {
                TicketStatus::Committed {
                    binding_id,
                    mode: summon_core::bridge::BridgeMode::Mirror,
                    ..
                } => binding_id,
                _ => return None,
            };
            let binding = match state.slot(&ticket.target) {
                Some(ChatSlot::MirrorBound(binding)) if binding.id == *binding_id => binding,
                _ => return None,
            };
            Some((ticket.id.clone(), binding.clone()))
        })
        .collect::<Vec<_>>();
    for (_ticket_id, binding) in committed {
        let receipt_id = format!("activation-{}", binding.id);
        let outbox_id = bridge_delivery_id(BridgeDeliveryKind::ActivationAudit, &receipt_id);
        if matches!(outbox.find(&outbox_id)?, Some((OutboxLocation::Dead, _))) {
            match crate::bridge::runtime::stop_mirror_exact(
                bridge,
                &binding.target,
                &binding.id,
                binding.revision,
            ) {
                Ok(_) => {}
                Err(error)
                    if error.downcast_ref::<summon_core::bridge::BridgeError>()
                        == Some(&summon_core::bridge::BridgeError::BindingMismatch) => {}
                Err(error) => return Err(error),
            }
            stats.dead_dropped = stats.dead_dropped.saturating_add(1);
        }
    }
    Ok(())
}

fn reconcile_finalized_receipts(bridge: &BridgeStore, outbox: &OutboxStore) -> Result<()> {
    let Some(state) = bridge.load_state()? else {
        return Ok(());
    };
    let turns = state
        .mirror_export_receipts
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let contexts = state
        .context_export_receipts
        .values()
        .map(|receipt| (receipt.id.clone(), receipt.outbox_id.clone()))
        .collect::<Vec<_>>();
    for receipt in turns {
        if outbox
            .find(&receipt.outbox_id)?
            .is_some_and(|(location, _)| {
                matches!(location, OutboxLocation::Archive | OutboxLocation::Dead)
            })
        {
            bridge.with_state(|state| {
                state
                    .finish_mirror_export(&receipt.id, &receipt.outbox_id)
                    .map_err(anyhow::Error::new)?;
                state
                    .discard_pending_mirror_turn(
                        &receipt.turn_id,
                        &receipt.binding_id,
                        receipt.binding_revision,
                    )
                    .map_err(anyhow::Error::new)?;
                Ok(())
            })?;
        }
    }
    for (receipt_id, outbox_id) in contexts {
        if outbox.find(&outbox_id)?.is_some_and(|(location, _)| {
            matches!(location, OutboxLocation::Archive | OutboxLocation::Dead)
        }) {
            bridge.with_state(|state| {
                state
                    .finish_context_export(&receipt_id, &outbox_id)
                    .map(|_| ())
                    .map_err(anyhow::Error::new)
            })?;
        }
    }
    Ok(())
}

#[derive(Clone)]
struct ActivationSnapshot {
    ticket_id: String,
    choice_id: String,
    requester_open_id: String,
    binding: summon_core::bridge::ChatBinding,
    generation: u64,
}

fn reconcile_activations(
    bridge: &BridgeStore,
    outbox: &OutboxStore,
    stats: &mut ReconcileStats,
) -> Result<()> {
    let Some(state) = bridge.load_state()? else {
        return Ok(());
    };
    let snapshots = state
        .tickets
        .values()
        .filter_map(|ticket| {
            let (binding_id, choice_id) = match &ticket.status {
                TicketStatus::Activating {
                    binding_id,
                    choice_id,
                    mode: summon_core::bridge::BridgeMode::Mirror,
                } => (binding_id, choice_id),
                _ => return None,
            };
            let binding = match state.slot(&ticket.target) {
                Some(ChatSlot::Activating(binding)) if binding.id == *binding_id => binding,
                _ => return None,
            };
            let generation = state.session(&binding.session)?.generation;
            Some(ActivationSnapshot {
                ticket_id: ticket.id.clone(),
                choice_id: choice_id.clone(),
                requester_open_id: ticket.requester_open_id.clone(),
                binding: binding.clone(),
                generation,
            })
        })
        .collect::<Vec<_>>();

    for snapshot in snapshots {
        let receipt_id = format!("activation-{}", snapshot.binding.id);
        let outbox_id = bridge_delivery_id(BridgeDeliveryKind::ActivationAudit, &receipt_id);
        match outbox.find(&outbox_id)? {
            Some((OutboxLocation::Dead, _)) => {
                cancel_exact_activation(bridge, &snapshot)?;
                stats.dead_dropped = stats.dead_dropped.saturating_add(1);
            }
            Some((OutboxLocation::Pending, event)) => {
                ensure_activation_event(&event, &snapshot, &receipt_id)?;
            }
            Some((OutboxLocation::Archive, event)) => {
                // archive 只保留 fence/票据身份，wire 已按隐私契约清空。此时 slot 仍 Activating
                // 只能来自“安全拒绝后取消写盘失败”的崩溃窗口；不能尝试 complete，也不能拿
                // 脱敏终态走 pending wire 校验，必须 exact 验证后取消 reservation。
                ensure_archived_activation_event(&event, &snapshot, &receipt_id)?;
                cancel_exact_activation(bridge, &snapshot)?;
            }
            None => {
                let sequence = bridge.with_state(|state| {
                    state
                        .allocate_activation_delivery_sequence(
                            &snapshot.ticket_id,
                            &snapshot.binding.id,
                        )
                        .map_err(anyhow::Error::new)
                })?;
                let event = activation_event(&snapshot, receipt_id, outbox_id, sequence);
                match outbox.enqueue_once(&event)? {
                    EnqueueOnceOutcome::Enqueued => {
                        stats.audits_enqueued = stats.audits_enqueued.saturating_add(1)
                    }
                    EnqueueOnceOutcome::Existing(_, existing) => {
                        ensure_activation_event(
                            &existing,
                            &snapshot,
                            event
                                .kind
                                .bridge_fence()
                                .expect(summon_i18n::text!("rc90606692d"))
                                .receipt_id
                                .as_str(),
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn activation_event(
    snapshot: &ActivationSnapshot,
    receipt_id: String,
    outbox_id: String,
    sequence: u64,
) -> OutboxEvent {
    OutboxEvent {
        id: outbox_id,
        agent: snapshot.binding.target.agent.clone(),
        delivery_scope: snapshot.binding.target.scope.clone(),
        created_ms: snapshot.binding.created_at_ms,
        attempts: 0,
        next_attempt_ms: snapshot.binding.created_at_ms,
        last_error: None,
        kind: OutboxKind::BridgeActivationAudit {
            fence: build_fence(&snapshot.binding, snapshot.generation, receipt_id, sequence),
            ticket_id: snapshot.ticket_id.clone(),
            choice_id: snapshot.choice_id.clone(),
            requester_open_id: snapshot.requester_open_id.clone(),
            card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
            rendered_card: summon_core::feishu::card_bridge_activation_audit(
                &summon_core::feishu::bridge_target_label(
                    &snapshot.binding.target.agent,
                    &snapshot.binding.target.chat_id,
                ),
            ),
            control_card: summon_core::feishu::card_bridge_mirror_control(
                &snapshot.ticket_id,
                &snapshot.choice_id,
                &summon_core::feishu::bridge_target_label(
                    &snapshot.binding.target.agent,
                    &snapshot.binding.target.chat_id,
                ),
            ),
        },
    }
}

fn ensure_activation_event(
    event: &OutboxEvent,
    snapshot: &ActivationSnapshot,
    receipt_id: &str,
) -> Result<()> {
    match &event.kind {
        OutboxKind::BridgeActivationAudit {
            fence,
            ticket_id,
            choice_id,
            requester_open_id,
            card_version,
            rendered_card,
            control_card,
        } if event.agent == snapshot.binding.target.agent
            && ticket_id == &snapshot.ticket_id
            && choice_id == &snapshot.choice_id
            && requester_open_id == &snapshot.requester_open_id
            && summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card)
            && summon_core::outbox::valid_frozen_bridge_card(*card_version, control_card)
            && fence
                == &build_fence(
                    &snapshot.binding,
                    snapshot.generation,
                    receipt_id.to_string(),
                    fence.sequence,
                ) =>
        {
            Ok(())
        }
        _ => bail!(summon_i18n::format!("r471eb4d62e")),
    }
}

fn ensure_archived_activation_event(
    event: &OutboxEvent,
    snapshot: &ActivationSnapshot,
    receipt_id: &str,
) -> Result<()> {
    match &event.kind {
        OutboxKind::BridgeActivationAudit {
            fence,
            ticket_id,
            choice_id,
            requester_open_id,
            rendered_card,
            control_card,
            ..
        } if event.id == bridge_delivery_id(BridgeDeliveryKind::ActivationAudit, receipt_id)
            && event.agent == snapshot.binding.target.agent
            && event.delivery_scope == snapshot.binding.target.scope
            && ticket_id == &snapshot.ticket_id
            && choice_id == &snapshot.choice_id
            && requester_open_id == &snapshot.requester_open_id
            && rendered_card.is_empty()
            && control_card.is_empty()
            && fence.sequence > 0
            && fence
                == &build_fence(
                    &snapshot.binding,
                    snapshot.generation,
                    receipt_id.to_string(),
                    fence.sequence,
                ) =>
        {
            Ok(())
        }
        _ => bail!(summon_i18n::format!("r407981bbfd")),
    }
}

fn cancel_exact_activation(bridge: &BridgeStore, snapshot: &ActivationSnapshot) -> Result<()> {
    bridge.with_state(|state| {
        let exact = state
            .tickets
            .get(&snapshot.ticket_id)
            .is_some_and(|ticket| {
                matches!(
                    &ticket.status,
                    TicketStatus::Activating { binding_id, .. }
                        if binding_id == &snapshot.binding.id
                )
            });
        if exact {
            state
                .cancel_ticket(&snapshot.ticket_id)
                .map_err(anyhow::Error::new)?;
        }
        Ok(())
    })
}

fn reconcile_turns(
    bridge: &BridgeStore,
    outbox: &OutboxStore,
    stats: &mut ReconcileStats,
) -> Result<()> {
    let Some(state) = bridge.load_state()? else {
        return Ok(());
    };
    let mut turns = state
        .pending_mirror_turns
        .values()
        .cloned()
        .collect::<Vec<_>>();
    turns.sort_by_key(|turn| (turn.sequence, turn.id.clone()));
    for turn in turns {
        let receipt_id = format!("turn-{}", turn.id);
        let outbox_id = bridge_delivery_id(BridgeDeliveryKind::ExternalTurn, &receipt_id);
        let event = match outbox.find(&outbox_id)? {
            Some((OutboxLocation::Dead, _)) => {
                bridge.with_state(|state| {
                    state
                        .discard_pending_mirror_turn(
                            &turn.id,
                            &turn.binding_id,
                            turn.binding_revision,
                        )
                        .map_err(anyhow::Error::new)
                })?;
                stats.dead_dropped = stats.dead_dropped.saturating_add(1);
                continue;
            }
            Some((OutboxLocation::Archive, archived)) => {
                validate_terminal_turn_event(&archived, &turn, &receipt_id)?;
                bridge.with_state(|state| {
                    state
                        .discard_pending_mirror_turn(
                            &turn.id,
                            &turn.binding_id,
                            turn.binding_revision,
                        )
                        .map_err(anyhow::Error::new)
                })?;
                continue;
            }
            Some((OutboxLocation::Pending, event)) => event,
            None => {
                let sequence = bridge.with_state(|state| {
                    state
                        .allocate_mirror_turn_delivery_sequence(&turn.id)
                        .map_err(anyhow::Error::new)
                })?;
                let event = turn_event(&turn, receipt_id.clone(), outbox_id.clone(), sequence)?;
                match outbox.enqueue_once(&event)? {
                    EnqueueOnceOutcome::Enqueued => event,
                    EnqueueOnceOutcome::Existing(_, existing) => *existing,
                }
            }
        };
        let receipt = receipt_from_turn_event(&event, &turn, &receipt_id)?;
        bridge.with_state(|state| {
            state
                .export_pending_mirror_turn(receipt.clone())
                .map_err(anyhow::Error::new)
        })?;
        stats.turns_exported = stats.turns_exported.saturating_add(1);
    }
    Ok(())
}

fn turn_event(
    turn: &PendingMirrorTurn,
    receipt_id: String,
    outbox_id: String,
    sequence: u64,
) -> Result<OutboxEvent> {
    let [user, assistant] = turn.messages.as_slice() else {
        bail!(summon_i18n::format!("rb00b075b51"))
    };
    if user.role != ExternalVisibleRole::User || assistant.role != ExternalVisibleRole::Assistant {
        bail!(summon_i18n::format!("r0786dced3c"))
    }
    let binding = summon_core::bridge::ChatBinding {
        id: turn.binding_id.clone(),
        target: turn.target.clone(),
        target_chat_type: turn.target_chat_type.clone(),
        session: turn.session.clone(),
        mode: summon_core::bridge::BridgeMode::Mirror,
        actor_open_id: String::new(),
        created_at_ms: turn.observed_at_ms,
        revision: turn.binding_revision,
    };
    Ok(OutboxEvent {
        id: outbox_id,
        agent: turn.target.agent.clone(),
        delivery_scope: turn.target.scope.clone(),
        created_ms: turn.observed_at_ms,
        attempts: 0,
        next_attempt_ms: turn.observed_at_ms,
        last_error: None,
        kind: OutboxKind::ExternalBridgeTurn {
            fence: build_fence(&binding, turn.generation, receipt_id, sequence),
            turn_id: turn.id.clone(),
            card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
            rendered_card: summon_core::feishu::card_external_bridge_turn(
                &user.text,
                &assistant.text,
            ),
        },
    })
}

fn receipt_from_turn_event(
    event: &OutboxEvent,
    turn: &PendingMirrorTurn,
    receipt_id: &str,
) -> Result<MirrorExportReceipt> {
    match &event.kind {
        OutboxKind::ExternalBridgeTurn {
            fence,
            turn_id,
            card_version,
            rendered_card,
        } if event.id == bridge_delivery_id(BridgeDeliveryKind::ExternalTurn, receipt_id)
            && event.agent == turn.target.agent
            && event.delivery_scope == turn.target.scope
            && fence.receipt_id == receipt_id
            && fence.scope == turn.target.scope
            && fence.agent == turn.target.agent
            && fence.chat_id == turn.target.chat_id
            && fence.chat_type == turn.target_chat_type
            && fence.binding_id == turn.binding_id
            && fence.lease_binding_id == turn.binding_id
            && fence.binding_revision == turn.binding_revision
            && fence.session == turn.session
            && fence.generation == turn.generation
            && turn_id == &turn.id
            && turn
                .messages
                .first()
                .is_some_and(|message| message.role == ExternalVisibleRole::User)
            && turn
                .messages
                .get(1)
                .is_some_and(|message| message.role == ExternalVisibleRole::Assistant)
            && summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card) =>
        {
            Ok(MirrorExportReceipt {
                id: receipt_id.to_string(),
                outbox_id: event.id.clone(),
                turn_id: turn.id.clone(),
                binding_id: turn.binding_id.clone(),
                binding_revision: turn.binding_revision,
                target: turn.target.clone(),
                session: turn.session.clone(),
                generation: turn.generation,
                sequence: fence.sequence,
                delivered_at_ms: None,
            })
        }
        _ => bail!(summon_i18n::format!("r5f188038c0")),
    }
}

fn validate_terminal_turn_event(
    event: &OutboxEvent,
    turn: &PendingMirrorTurn,
    receipt_id: &str,
) -> Result<()> {
    match &event.kind {
        OutboxKind::ExternalBridgeTurn {
            fence,
            turn_id,
            card_version,
            rendered_card,
        } if event.id == bridge_delivery_id(BridgeDeliveryKind::ExternalTurn, receipt_id)
            && event.agent == turn.target.agent
            && event.delivery_scope == turn.target.scope
            && fence.receipt_id == receipt_id
            && fence.scope == turn.target.scope
            && fence.agent == turn.target.agent
            && fence.chat_id == turn.target.chat_id
            && fence.chat_type == turn.target_chat_type
            && fence.binding_id == turn.binding_id
            && fence.lease_binding_id == turn.binding_id
            && fence.binding_revision == turn.binding_revision
            && fence.session == turn.session
            && fence.generation == turn.generation
            && turn_id == &turn.id
            && *card_version > 0
            && *card_version <= summon_core::outbox::BRIDGE_CARD_WIRE_VERSION
            && (rendered_card.is_empty()
                || summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card)) =>
        {
            Ok(())
        }
        _ => bail!(summon_i18n::format!("r45e7af7448")),
    }
}

fn reconcile_context_receipts(
    bridge: &BridgeStore,
    outbox: &OutboxStore,
    stats: &mut ReconcileStats,
) -> Result<()> {
    let pending = outbox.pending_events_checked()?;
    let mut sequence_owners = BTreeMap::<(String, u64), usize>::new();
    for event in &pending {
        if let Some(fence) = event.kind.bridge_fence() {
            *sequence_owners
                .entry((fence.binding_id.clone(), fence.sequence))
                .or_default() += 1;
        }
    }
    let snapshot = bridge.load_state()?;
    for event in pending {
        if let OutboxKind::BridgeContextCapsule { fence, .. } = &event.kind {
            anyhow::ensure!(
                sequence_owners.get(&(fence.binding_id.clone(), fence.sequence)) == Some(&1),
                summon_i18n::format!("r9e3d20c5d4")
            );
        }
        let Some(receipt) = context_receipt_from_event(&event)? else {
            continue;
        };
        if snapshot
            .as_ref()
            .and_then(|state| state.context_export_receipts.get(&receipt.id))
            == Some(&receipt)
        {
            continue;
        }
        match bridge.with_state(|state| {
            state
                .recover_context_export(receipt.clone())
                .map_err(anyhow::Error::new)
        }) {
            Ok(true) => stats.contexts_recovered = stats.contexts_recovered.saturating_add(1),
            Ok(false) => {}
            Err(error)
                if error
                    .downcast_ref::<summon_core::bridge::BridgeError>()
                    .is_some_and(|error| {
                        matches!(
                            error,
                            summon_core::bridge::BridgeError::BindingMismatch
                                | summon_core::bridge::BridgeError::TicketMissing
                                | summon_core::bridge::BridgeError::TicketFinal
                        )
                    }) => {}
            Err(error) => return Err(error).context(summon_i18n::text!("rc99d7b2236")),
        }
    }
    Ok(())
}

fn context_receipt_from_event(event: &OutboxEvent) -> Result<Option<ContextExportReceipt>> {
    let OutboxKind::BridgeContextCapsule {
        fence,
        ticket_id,
        choice_id,
        requester_open_id,
        card_version,
        rendered_card,
    } = &event.kind
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        event.id == bridge_delivery_id(BridgeDeliveryKind::ContextCapsule, &fence.receipt_id)
            && event.agent == fence.agent
            && fence.binding_id == fence.lease_binding_id
            && !fence.scope.is_empty()
            && !fence.agent.is_empty()
            && !fence.chat_id.is_empty()
            && !fence.chat_type.is_empty()
            && !fence.binding_id.is_empty()
            && fence.binding_revision > 0
            && fence.generation > 0
            && fence.sequence > 0
            && !ticket_id.is_empty()
            && !choice_id.is_empty()
            && !requester_open_id.is_empty()
            && summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card),
        summon_i18n::format!("rd84c099065")
    );
    Ok(Some(ContextExportReceipt {
        id: fence.receipt_id.clone(),
        outbox_id: event.id.clone(),
        ticket_id: ticket_id.clone(),
        choice_id: choice_id.clone(),
        requester_open_id: requester_open_id.clone(),
        binding_id: fence.binding_id.clone(),
        binding_revision: fence.binding_revision,
        target: summon_core::bridge::ScopedChatKey::new(&fence.scope, &fence.agent, &fence.chat_id),
        target_chat_type: fence.chat_type.clone(),
        session: fence.session.clone(),
        generation: fence.generation,
        sequence: fence.sequence,
        delivered_at_ms: None,
    }))
}

fn build_fence(
    binding: &summon_core::bridge::ChatBinding,
    generation: u64,
    receipt_id: String,
    sequence: u64,
) -> BridgeDeliveryFence {
    BridgeDeliveryFence {
        scope: binding.target.scope.clone(),
        agent: binding.target.agent.clone(),
        chat_id: binding.target.chat_id.clone(),
        chat_type: binding.target_chat_type.clone(),
        binding_id: binding.id.clone(),
        binding_revision: binding.revision,
        session: binding.session.clone(),
        generation,
        lease_binding_id: binding.id.clone(),
        receipt_id,
        sequence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::bridge::{
        ActivationAuditReceipt, BeginChoiceOutcome, BridgeChoice, BridgeMode, BridgeState,
        ExternalLifecycle, ExternalLocator, ExternalSessionRecord, ExternalVisibleMessage,
        ProcessFingerprint, ResumeProfile, ScopedChatKey, SessionKey, TicketRequest,
    };

    fn session() -> ExternalSessionRecord {
        let key = SessionKey::new("/tmp/config", "sid");
        ExternalSessionRecord {
            key: key.clone(),
            generation: 1,
            locator: ExternalLocator {
                key,
                transcript_path: "/tmp/config/sid.jsonl".into(),
                resume_lookup_cwd: "/tmp/project".into(),
                current_cwd: "/tmp/project".into(),
                profile: ResumeProfile {
                    claude_bin: "/opt/claude".into(),
                    home: "/tmp/home".into(),
                    config_dir: "/tmp/config".into(),
                    version: "1".into(),
                    process_verified: true,
                    environment_verified: false,
                    ..ResumeProfile::default()
                },
                revision: 7,
            },
            process: Some(ProcessFingerprint {
                pid: 42,
                start_time: "start".into(),
                executable: "/opt/claude".into(),
            }),
            lifecycle: ExternalLifecycle::ActiveIdle,
            first_seen_ms: 1,
            last_seen_ms: 2,
            last_applied_sequence: 1,
            pending_prompt: None,
        }
    }

    fn activating_state() -> (BridgeState, summon_core::bridge::ChatBinding) {
        let mut state = BridgeState::default();
        let record = session();
        state.upsert_session(record.clone());
        let target = ScopedChatKey::new("scope", "agent", "chat");
        state
            .reserve_ticket(TicketRequest {
                ticket_id: "ticket".into(),
                short_code: None,
                target,
                target_chat_type: "group".into(),
                target_provider: summon_core::bridge::ClaudeProvider::Default,
                requester_open_id: "owner".into(),
                requester_union_id: "union".into(),
                choices: vec![BridgeChoice {
                    id: "choice".into(),
                    session: record.key,
                    registry_revision: 7,
                }],
                issued_at_ms: 10,
                expires_at_ms: 1_000,
            })
            .unwrap();
        state
            .attach_control_card("ticket", "owner", Some("private"), "message")
            .unwrap();
        let binding = match state
            .begin_choice(
                "ticket",
                "choice",
                "owner",
                "message",
                BridgeMode::Mirror,
                "binding",
                20,
            )
            .unwrap()
        {
            BeginChoiceOutcome::Started(binding) => binding,
            BeginChoiceOutcome::AlreadyStarted(_) => unreachable!(),
        };
        (state, binding)
    }

    #[test]
    fn idle_tick_cache_does_not_hide_out_of_process_state_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bridge");
        let bridge = BridgeStore::new(&root);
        let outbox = OutboxStore::new(dir.path().join("outbox"));
        // 先跑一轮空闲 tick,把 state 读缓存填满(has_reconcile_work == false 的 early return)。
        bridge.with_state(|_| Ok(())).unwrap();
        assert_eq!(
            reconcile_once(&bridge, &outbox, 30).unwrap(),
            ReconcileStats::default()
        );

        // Claude Hook 在别的进程里写 state;drain 侧的缓存必须在下一 tick 就失效。
        let out_of_process = BridgeStore::new(&root);
        let (state, _) = activating_state();
        out_of_process
            .with_state(|current| {
                *current = state;
                Ok(())
            })
            .unwrap();

        assert_eq!(
            reconcile_once(&bridge, &outbox, 31)
                .unwrap()
                .audits_enqueued,
            1
        );
    }

    #[test]
    fn activating_crash_reconcile_enqueues_one_audit_before_any_body() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let outbox = OutboxStore::new(dir.path().join("outbox"));
        let (state, _) = activating_state();
        bridge
            .with_state(|current| {
                *current = state;
                Ok(())
            })
            .unwrap();

        let first = reconcile_once(&bridge, &outbox, 30).unwrap();
        assert_eq!(first.audits_enqueued, 1);
        let second = reconcile_once(&bridge, &outbox, 31).unwrap();
        assert_eq!(second.audits_enqueued, 0);
        let events = outbox.pending_events();
        assert_eq!(events.len(), 1);
        let fence = events[0].kind.bridge_fence().unwrap();
        assert_eq!(fence.sequence, 1);
        assert!(matches!(
            events[0].kind,
            OutboxKind::BridgeActivationAudit { .. }
        ));
        let state = bridge.load_state().unwrap().unwrap();
        assert!(state.pending_mirror_turns.is_empty());
        assert!(state.activation_audit_receipts.is_empty());
        assert!(matches!(
            state.slot(&ScopedChatKey::new("scope", "agent", "chat")),
            Some(ChatSlot::Activating(_))
        ));
    }

    #[test]
    fn archived_activation_while_still_activating_is_exactly_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let outbox = OutboxStore::new(dir.path().join("outbox"));
        let (state, binding) = activating_state();
        bridge
            .with_state(|current| {
                *current = state;
                Ok(())
            })
            .unwrap();

        reconcile_once(&bridge, &outbox, 30).unwrap();
        let event = outbox.pending_events_checked().unwrap().pop().unwrap();
        assert_eq!(
            outbox.drain_tick_with_hooks(30, &|_| Ok(()), &|_| Ok(()), &|_| Ok(())),
            1
        );
        let (location, archived) = outbox.find(&event.id).unwrap().unwrap();
        assert_eq!(location, OutboxLocation::Archive);
        assert!(matches!(
            archived.kind,
            OutboxKind::BridgeActivationAudit {
                ref rendered_card,
                ref control_card,
                ..
            } if rendered_card.is_empty() && control_card.is_empty()
        ));

        reconcile_once(&bridge, &outbox, 31).unwrap();
        let state = bridge.load_state().unwrap().unwrap();
        assert!(state.slot(&binding.target).is_none());
        assert!(matches!(
            state.tickets["ticket"].status,
            TicketStatus::Cancelled
        ));
    }

    #[test]
    fn crash_reconcile_accepts_valid_legacy_frozen_wire_without_rerendering() {
        let (state, binding) = activating_state();
        let snapshot = ActivationSnapshot {
            ticket_id: "ticket".into(),
            choice_id: "choice".into(),
            requester_open_id: "owner".into(),
            binding: binding.clone(),
            generation: state.session(&binding.session).unwrap().generation,
        };
        let mut audit = activation_event(
            &snapshot,
            "activation-binding".into(),
            bridge_delivery_id(BridgeDeliveryKind::ActivationAudit, "activation-binding"),
            1,
        );
        if let OutboxKind::BridgeActivationAudit { rendered_card, .. } = &mut audit.kind {
            *rendered_card = r#"{"schema":"2.0","body":{"elements":[]},"legacy":true}"#.into();
        }
        ensure_activation_event(&audit, &snapshot, "activation-binding").unwrap();

        let turn = PendingMirrorTurn {
            id: "turn-legacy".into(),
            session: binding.session.clone(),
            generation: snapshot.generation,
            binding_id: binding.id.clone(),
            binding_revision: binding.revision,
            target: binding.target.clone(),
            target_chat_type: "group".into(),
            observed_at_ms: 20,
            sequence: 20,
            messages: vec![
                summon_core::bridge::ExternalVisibleMessage {
                    role: ExternalVisibleRole::User,
                    text: "old user".into(),
                },
                summon_core::bridge::ExternalVisibleMessage {
                    role: ExternalVisibleRole::Assistant,
                    text: "old assistant".into(),
                },
            ],
        };
        let receipt_id = "turn-turn-legacy";
        let mut event = turn_event(
            &turn,
            receipt_id.into(),
            bridge_delivery_id(BridgeDeliveryKind::ExternalTurn, receipt_id),
            2,
        )
        .unwrap();
        if let OutboxKind::ExternalBridgeTurn { rendered_card, .. } = &mut event.kind {
            *rendered_card = r#"{"schema":"2.0","body":{"elements":[]},"legacy":true}"#.into();
        }
        receipt_from_turn_event(&event, &turn, receipt_id).unwrap();
    }

    #[test]
    fn dead_activation_after_commit_exactly_stops_uncontrollable_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let outbox = OutboxStore::new(dir.path().join("outbox"));
        let (state, binding) = activating_state();
        bridge
            .with_state(|current| {
                *current = state;
                Ok(())
            })
            .unwrap();
        reconcile_once(&bridge, &outbox, 30).unwrap();
        let event = outbox.pending_events_checked().unwrap().pop().unwrap();
        let fence = event.kind.bridge_fence().unwrap().clone();
        bridge
            .with_state(|state| {
                state
                    .record_activation_audit(summon_core::bridge::ActivationAuditReceipt {
                        id: fence.receipt_id.clone(),
                        outbox_id: event.id.clone(),
                        ticket_id: "ticket".into(),
                        binding_id: binding.id.clone(),
                        binding_revision: binding.revision,
                        target: binding.target.clone(),
                        session: binding.session.clone(),
                        generation: fence.generation,
                        sequence: fence.sequence,
                        delivered_at_ms: 31,
                    })
                    .map_err(anyhow::Error::new)?;
                state
                    .complete_mirror_after_audit("ticket", &binding.id, &fence.receipt_id)
                    .map_err(anyhow::Error::new)?;
                Ok(())
            })
            .unwrap();
        outbox.to_dead(&event.id);

        let stats = reconcile_once(&bridge, &outbox, 32).unwrap();
        assert_eq!(stats.dead_dropped, 1);
        let state = bridge.load_state().unwrap().unwrap();
        assert!(state.slot(&binding.target).is_none());
        assert!(!state
            .session_leases
            .contains_key(&binding.session.storage_key()));
    }

    #[test]
    fn idle_reconcile_does_not_create_or_rewrite_state() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let outbox = OutboxStore::new(dir.path().join("outbox"));

        assert_eq!(
            reconcile_once(&bridge, &outbox, 30).unwrap(),
            ReconcileStats::default()
        );
        assert!(
            !bridge.state_path().exists(),
            "空闲 tick 只能读状态，不能创建并 fsync 空 state"
        );
    }

    #[test]
    fn session_gc_removes_only_old_unreferenced_records() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let mut unreferenced = session();
        unreferenced.lifecycle = ExternalLifecycle::Ended;
        unreferenced.last_seen_ms = 1;
        let mut referenced = unreferenced.clone();
        referenced.key = SessionKey::new("/tmp/config", "referenced");
        referenced.locator.key = referenced.key.clone();
        referenced.locator.transcript_path = "/tmp/config/referenced.jsonl".into();
        let target = ScopedChatKey::new("scope", "agent", "chat");
        let binding = summon_core::bridge::ChatBinding {
            id: "binding-ref".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: referenced.key.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "owner".into(),
            created_at_ms: 1,
            revision: 1,
        };
        bridge
            .with_state(|state| {
                state.upsert_session(unreferenced.clone());
                state.upsert_session(referenced.clone());
                state
                    .slots
                    .insert(target.storage_key(), ChatSlot::MirrorBound(binding.clone()));
                Ok(())
            })
            .unwrap();

        assert_eq!(
            gc_sessions_once(&bridge, ENDED_SESSION_RETENTION_MS + 2).unwrap(),
            1
        );
        let state = bridge.load_state().unwrap().unwrap();
        assert!(state.session(&unreferenced.key).is_none());
        assert!(state.session(&referenced.key).is_some());
    }

    #[test]
    fn session_gc_pressure_is_bounded_by_batch_size() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let total = MAX_UNREFERENCED_SESSIONS + MAX_SESSION_GC_BATCH + 5;
        bridge
            .with_state(|state| {
                for index in 0..total {
                    let sid = format!("pressure-{index:04}");
                    let mut record = session();
                    record.key = SessionKey::new("/tmp/config", &sid);
                    record.locator.key = record.key.clone();
                    record.locator.transcript_path =
                        format!("/tmp/config/projects/demo/{sid}.jsonl");
                    record.lifecycle = ExternalLifecycle::Ended;
                    record.last_seen_ms = index as u64 + 1;
                    state.upsert_session(record);
                }
                Ok(())
            })
            .unwrap();

        assert_eq!(gc_sessions_once(&bridge, total as u64 + 1).unwrap(), 32);
        assert_eq!(
            bridge.load_state().unwrap().unwrap().sessions.len(),
            total - 32
        );
        assert_eq!(gc_sessions_once(&bridge, total as u64 + 1).unwrap(), 5);
        assert_eq!(
            bridge.load_state().unwrap().unwrap().sessions.len(),
            MAX_UNREFERENCED_SESSIONS
        );
    }

    #[test]
    fn session_gc_preserves_exact_live_process() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let process = crate::bridge::process_probe::observe_exact_fingerprint(std::process::id())
            .unwrap()
            .expect("测试进程应可被 exact probe 观察");
        let mut record = session();
        record.process = Some(process);
        record.last_seen_ms = 1;
        let key = record.key.clone();
        bridge
            .with_state(|state| {
                state.upsert_session(record.clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(
            gc_sessions_once(&bridge, STALE_SESSION_AGE_MS + 2).unwrap(),
            0
        );
        assert!(bridge
            .load_state()
            .unwrap()
            .unwrap()
            .session(&key)
            .is_some());
    }

    #[test]
    fn session_gc_removes_pid_reuse_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let mut process =
            crate::bridge::process_probe::observe_exact_fingerprint(std::process::id())
                .unwrap()
                .expect("测试进程应可被 exact probe 观察");
        process.start_time.push_str("-stale");
        let mut record = session();
        record.process = Some(process);
        record.last_seen_ms = 1;
        let key = record.key.clone();
        bridge
            .with_state(|state| {
                state.upsert_session(record.clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(
            gc_sessions_once(&bridge, STALE_SESSION_AGE_MS + 2).unwrap(),
            1
        );
        assert!(bridge
            .load_state()
            .unwrap()
            .unwrap()
            .session(&key)
            .is_none());
    }

    #[test]
    fn corrupt_activation_is_cancelled_before_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let outbox_root = dir.path().join("outbox");
        let outbox = OutboxStore::new(&outbox_root);
        let (state, binding) = activating_state();
        bridge
            .with_state(|current| {
                *current = state.clone();
                Ok(())
            })
            .unwrap();
        let outbox_id = bridge_delivery_id(
            BridgeDeliveryKind::ActivationAudit,
            &format!("activation-{}", binding.id),
        );
        std::fs::create_dir_all(outbox_root.join("pending")).unwrap();
        std::fs::write(
            outbox_root
                .join("pending")
                .join(format!("{outbox_id}.json")),
            b"{broken",
        )
        .unwrap();

        let stats = reconcile_once(&bridge, &outbox, 30).unwrap();
        assert_eq!(stats.dead_dropped, 1);
        let state = bridge.load_state().unwrap().unwrap();
        assert!(state.slot(&binding.target).is_none());
        assert!(matches!(
            state.tickets["ticket"].status,
            TicketStatus::Cancelled
        ));
        assert!(!outbox_root
            .join("pending")
            .join(format!("{outbox_id}.json"))
            .exists());
        assert_eq!(
            std::fs::read_dir(outbox_root.join("quarantine"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn context_outbox_crash_recovers_cursor_before_allocating_pending_turn_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let outbox = OutboxStore::new(dir.path().join("outbox"));
        let (mut state, binding) = activating_state();
        let audit_sequence = state
            .allocate_activation_delivery_sequence("ticket", "binding")
            .unwrap();
        state
            .record_activation_audit(ActivationAuditReceipt {
                id: "activation-binding".into(),
                outbox_id: bridge_delivery_id(
                    BridgeDeliveryKind::ActivationAudit,
                    "activation-binding",
                ),
                ticket_id: "ticket".into(),
                binding_id: binding.id.clone(),
                binding_revision: binding.revision,
                target: binding.target.clone(),
                session: binding.session.clone(),
                generation: 1,
                sequence: audit_sequence,
                delivered_at_ms: 21,
            })
            .unwrap();
        state
            .complete_mirror_after_audit("ticket", "binding", "activation-binding")
            .unwrap();
        state.pending_mirror_turns.insert(
            "turn-after-context".into(),
            PendingMirrorTurn {
                id: "turn-after-context".into(),
                session: binding.session.clone(),
                generation: 1,
                binding_id: binding.id.clone(),
                binding_revision: binding.revision,
                target: binding.target.clone(),
                target_chat_type: binding.target_chat_type.clone(),
                observed_at_ms: 30,
                sequence: 30,
                messages: vec![
                    ExternalVisibleMessage {
                        role: ExternalVisibleRole::User,
                        text: "user".into(),
                    },
                    ExternalVisibleMessage {
                        role: ExternalVisibleRole::Assistant,
                        text: "assistant".into(),
                    },
                ],
            },
        );
        bridge
            .with_state(|current| {
                *current = state;
                Ok(())
            })
            .unwrap();

        let receipt_id = "context-crash";
        let context_id = bridge_delivery_id(BridgeDeliveryKind::ContextCapsule, receipt_id);
        let context = OutboxEvent {
            id: context_id,
            agent: binding.target.agent.clone(),
            delivery_scope: binding.target.scope.clone(),
            created_ms: 25,
            attempts: 0,
            next_attempt_ms: 25,
            last_error: None,
            kind: OutboxKind::BridgeContextCapsule {
                fence: BridgeDeliveryFence {
                    scope: binding.target.scope.clone(),
                    agent: binding.target.agent.clone(),
                    chat_id: binding.target.chat_id.clone(),
                    chat_type: binding.target_chat_type.clone(),
                    binding_id: binding.id.clone(),
                    binding_revision: binding.revision,
                    session: binding.session.clone(),
                    generation: 1,
                    lease_binding_id: binding.id.clone(),
                    receipt_id: receipt_id.into(),
                    sequence: 2,
                },
                ticket_id: "ticket".into(),
                choice_id: "choice".into(),
                requester_open_id: "owner".into(),
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card: r#"{"schema":"2.0","body":{"elements":[]}}"#.into(),
            },
        };
        outbox.enqueue_once(&context).unwrap();

        let stats = reconcile_once(&bridge, &outbox, 40).unwrap();
        assert_eq!(stats.contexts_recovered, 1);
        assert_eq!(stats.turns_exported, 1);
        let state = bridge.load_state().unwrap().unwrap();
        assert_eq!(state.context_export_receipts[receipt_id].sequence, 2);
        assert_eq!(
            state.mirror_export_receipts["turn-turn-after-context"].sequence, 3,
            "context crash receipt 必须先占回 sequence=2"
        );
        assert_eq!(state.delivery_sequence_cursors[&binding.id], 3);
        let before = std::fs::read(bridge.state_path()).unwrap();
        let second = reconcile_once(&bridge, &outbox, 41).unwrap();
        assert_eq!(second.contexts_recovered, 0);
        assert_eq!(second.turns_exported, 0);
        assert_eq!(std::fs::read(bridge.state_path()).unwrap(), before);
    }

    #[test]
    fn turn_enqueue_then_state_crash_is_recovered_without_clobbering_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let outbox = OutboxStore::new(dir.path().join("outbox"));
        let (mut state, binding) = activating_state();
        let audit_sequence = state
            .allocate_activation_delivery_sequence("ticket", "binding")
            .unwrap();
        state
            .record_activation_audit(ActivationAuditReceipt {
                id: "activation-binding".into(),
                outbox_id: bridge_delivery_id(
                    BridgeDeliveryKind::ActivationAudit,
                    "activation-binding",
                ),
                ticket_id: "ticket".into(),
                binding_id: binding.id.clone(),
                binding_revision: binding.revision,
                target: binding.target.clone(),
                session: binding.session.clone(),
                generation: 1,
                sequence: audit_sequence,
                delivered_at_ms: 21,
            })
            .unwrap();
        state
            .complete_mirror_after_audit("ticket", "binding", "activation-binding")
            .unwrap();
        let turn = PendingMirrorTurn {
            id: "stop-event".into(),
            session: binding.session.clone(),
            generation: 1,
            binding_id: binding.id.clone(),
            binding_revision: binding.revision,
            target: binding.target.clone(),
            target_chat_type: "group".into(),
            observed_at_ms: 30,
            sequence: 30,
            messages: vec![
                ExternalVisibleMessage {
                    role: ExternalVisibleRole::User,
                    text: "user secret".into(),
                },
                ExternalVisibleMessage {
                    role: ExternalVisibleRole::Assistant,
                    text: "assistant secret".into(),
                },
            ],
        };
        state
            .pending_mirror_turns
            .insert(turn.id.clone(), turn.clone());
        bridge
            .with_state(|current| {
                *current = state;
                Ok(())
            })
            .unwrap();

        reconcile_once(&bridge, &outbox, 40).unwrap();
        let event = outbox.pending_events().pop().unwrap();
        let receipt_id = event.kind.bridge_fence().unwrap().receipt_id.clone();
        // 模拟 enqueue 已 durable、state receipt 尚未提交的 crash，并让事件已有 backoff。
        bridge
            .with_state(|state| {
                state.mirror_export_receipts.remove(&receipt_id);
                state
                    .pending_mirror_turns
                    .insert(turn.id.clone(), turn.clone());
                Ok(())
            })
            .unwrap();
        let mut failed = event.clone();
        failed.attempts = 7;
        failed.next_attempt_ms = 9_999;
        failed.last_error = Some("network".into());
        outbox.record_failure(&failed).unwrap();

        reconcile_once(&bridge, &outbox, 41).unwrap();
        let saved = outbox.find(&event.id).unwrap().unwrap().1;
        assert_eq!(saved.attempts, 7, "enqueue_once 不得清空 attempts");
        assert_eq!(saved.next_attempt_ms, 9_999);
        let state = bridge.load_state().unwrap().unwrap();
        assert!(!state.pending_mirror_turns.contains_key(&turn.id));
        assert!(state.mirror_export_receipts.contains_key(&receipt_id));
    }

    #[test]
    fn archived_turn_before_domain_ack_atomically_drops_receipt_and_crash_left_body() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = BridgeStore::new(dir.path().join("bridge"));
        let outbox = OutboxStore::new(dir.path().join("outbox"));
        let (mut state, binding) = activating_state();
        let audit_sequence = state
            .allocate_activation_delivery_sequence("ticket", "binding")
            .unwrap();
        state
            .record_activation_audit(ActivationAuditReceipt {
                id: "activation-binding".into(),
                outbox_id: bridge_delivery_id(
                    BridgeDeliveryKind::ActivationAudit,
                    "activation-binding",
                ),
                ticket_id: "ticket".into(),
                binding_id: binding.id.clone(),
                binding_revision: binding.revision,
                target: binding.target.clone(),
                session: binding.session.clone(),
                generation: 1,
                sequence: audit_sequence,
                delivered_at_ms: 21,
            })
            .unwrap();
        state
            .complete_mirror_after_audit("ticket", "binding", "activation-binding")
            .unwrap();
        let turn = PendingMirrorTurn {
            id: "archive-crash".into(),
            session: binding.session.clone(),
            generation: 1,
            binding_id: binding.id.clone(),
            binding_revision: binding.revision,
            target: binding.target.clone(),
            target_chat_type: "group".into(),
            observed_at_ms: 30,
            sequence: 30,
            messages: vec![
                ExternalVisibleMessage {
                    role: ExternalVisibleRole::User,
                    text: "ARCHIVE_USER_SECRET".into(),
                },
                ExternalVisibleMessage {
                    role: ExternalVisibleRole::Assistant,
                    text: "ARCHIVE_ASSISTANT_SECRET".into(),
                },
            ],
        };
        state
            .pending_mirror_turns
            .insert(turn.id.clone(), turn.clone());
        bridge
            .with_state(|current| {
                *current = state;
                Ok(())
            })
            .unwrap();

        reconcile_once(&bridge, &outbox, 40).unwrap();
        let event = outbox.pending_events().pop().unwrap();
        let receipt_id = event.kind.bridge_fence().unwrap().receipt_id.clone();
        bridge
            .with_state(|state| {
                state
                    .mark_mirror_export_delivered(&receipt_id, &event.id, 41)
                    .map_err(anyhow::Error::new)?;
                // 模拟 archive durable、on_acked 前一代异常留下的双事实。
                state
                    .pending_mirror_turns
                    .insert(turn.id.clone(), turn.clone());
                Ok(())
            })
            .unwrap();
        outbox.ack(&event.id);

        reconcile_once(&bridge, &outbox, 42).unwrap();
        let state = bridge.load_state().unwrap().unwrap();
        assert!(!state.mirror_export_receipts.contains_key(&receipt_id));
        assert!(!state.pending_mirror_turns.contains_key(&turn.id));
        let disk = std::fs::read_to_string(bridge.state_path()).unwrap();
        assert!(!disk.contains("ARCHIVE_USER_SECRET"));
        assert!(!disk.contains("ARCHIVE_ASSISTANT_SECRET"));
    }
}
