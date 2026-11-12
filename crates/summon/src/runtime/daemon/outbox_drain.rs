//! durable outbox 消费 + inbox WAL 启动恢复(A1 拆分):spawn_outbox_drain/deliver_outbox_event/boot_recover_inbox。

use super::*;
use crate::frontend::outbound::send_card_idempotent_archived_id;

fn current_bridge_operator_authorized_at_epoch(
    snapshot: &Agent,
    epoch: &FeishuEpochIdentity,
    requester_open_id: &str,
    requester_union_id: &str,
) -> anyhow::Result<bool> {
    let current = snapshot
        .reread()
        .context(summon_i18n::text!("r9ae33c1572"))?;
    anyhow::ensure!(
        epoch.matches_current(&current),
        summon_i18n::format!("r9d3e42c87a")
    );
    anyhow::ensure!(current.is_routable(), summon_i18n::format!("r0ee9efa7a0"));
    Ok(super::bridge::bridge_operator_allowed(
        &current,
        requester_open_id,
        requester_union_id,
    ))
}

fn ensure_bridge_epoch_current(
    snapshot: &Agent,
    epoch: &FeishuEpochIdentity,
) -> anyhow::Result<()> {
    let current = snapshot
        .reread()
        .context(summon_i18n::text!("r2bd0fc53c2"))?;
    anyhow::ensure!(current.is_routable(), summon_i18n::format!("r7aaf48b3d9"));
    anyhow::ensure!(
        epoch.matches_current(&current),
        summon_i18n::format!("r9a293a51c7")
    );
    Ok(())
}

fn ensure_local_delivery_bridge_clear(
    epoch: &FeishuEpochIdentity,
    chat_type: &str,
    chat_id: &str,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    if !matches!(chat_type, "group" | "topic_group") {
        return Ok(());
    }
    let target = summon_core::bridge::ScopedChatKey::new(
        &epoch.storage_scope().key,
        epoch.agent_name(),
        chat_id,
    );
    let state = store.load_state()?.unwrap_or_default();
    anyhow::ensure!(
        state.slot(&target).is_none(),
        summon_i18n::format!("re3c750a4d6")
    );
    Ok(())
}

fn deliver_turn_reply(frontend: &dyn Frontend, chat: &ChatRef, text: &str) -> anyhow::Result<()> {
    // 事件保留权威全文，消费时仍必须按 adapter 的最终编码预算分段。
    // 任一段失败则整个事件重试：可能重复前缀，但不会把尾段静默送进死信。
    let text = summon_core::feed_tag::strip_feed_tag(text);
    for segment in frontend.split_final_reply(chat, &text) {
        super::chat_thread::send_reply_or_explain_rejection(
            frontend,
            chat,
            chat.agent_name,
            || frontend.send_message(chat, &segment),
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryDeliveryPreflight {
    Ready,
    LocalAck,
}

/// RecoveryCard 的网络发送权必须来自 exact active manifest；completed 或已记录 message_id 的
/// active batch 只补本地 ACK。Missing/损坏/字段漂移都在 HTTP 前 fail-closed。
fn recovery_delivery_preflight(
    event: &summon_core::outbox::OutboxEvent,
    wal: &crate::runtime::inbox_wal::InboxWal,
) -> anyhow::Result<RecoveryDeliveryPreflight> {
    let summon_core::outbox::OutboxKind::RecoveryCard {
        batch_id,
        scope,
        chat_id,
        chat_type,
        items,
        stage,
        card_version,
        rendered_card,
    } = &event.kind
    else {
        anyhow::bail!(summon_i18n::format!("r7a0b7b0875"))
    };
    anyhow::ensure!(
        !batch_id.is_empty()
            && batch_id == &event.id
            && !scope.is_empty()
            && scope == &event.delivery_scope
            && !event.agent.is_empty()
            && !chat_id.is_empty()
            && !items.is_empty()
            && summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card),
        summon_i18n::format!("r9156cc9c53")
    );
    if wal.recovery_outbox_completed_exact(batch_id, &event.id, scope, &event.agent, chat_id)? {
        return Ok(RecoveryDeliveryPreflight::LocalAck);
    }
    let batch = wal
        .load_recovery_batch(batch_id)?
        .context(summon_i18n::text!("r42266593f9"))?;
    let expected_items = batch
        .items
        .iter()
        .map(|item| summon_core::outbox::RecoveryItem {
            message_id: item.record.inbound.message_id.clone(),
            preview: item.preview.clone(),
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        batch.id == batch_id.as_str()
            && batch.outbox_id == event.id.as_str()
            && batch.scope == scope.as_str()
            && batch.agent == event.agent.as_str()
            && batch.chat_id == chat_id.as_str()
            && batch.chat_type == chat_type.as_str()
            && batch.stage.as_ref() == stage.as_ref()
            && expected_items.as_slice() == items.as_slice(),
        summon_i18n::format!("r578b5dcfaf")
    );
    if wal.recovery_outbox_delivered(batch_id, &event.id, scope, &event.agent, chat_id)? {
        return Ok(RecoveryDeliveryPreflight::LocalAck);
    }
    Ok(RecoveryDeliveryPreflight::Ready)
}

fn deliver_recovery_card_with_wal(
    agent: &Agent,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
    wal: &crate::runtime::inbox_wal::InboxWal,
) -> anyhow::Result<()> {
    if recovery_delivery_preflight(event, wal)? == RecoveryDeliveryPreflight::LocalAck {
        return Ok(());
    }
    let summon_core::outbox::OutboxKind::RecoveryCard {
        batch_id,
        scope,
        chat_id,
        chat_type,
        items,
        card_version,
        rendered_card,
        ..
    } = &event.kind
    else {
        anyhow::bail!(summon_i18n::format!("r7a0b7b0875"))
    };
    anyhow::ensure!(
        !batch_id.is_empty()
            && batch_id == &event.id
            && scope == &event.delivery_scope
            && !scope.is_empty()
            && !items.is_empty(),
        summon_i18n::format!("r990fc5aa94")
    );
    anyhow::ensure!(
        summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card),
        summon_i18n::format!("rc88b1ed252")
    );
    let message_id = send_card_idempotent_archived_id(
        fc,
        &agent.state_dir(),
        chat_type,
        chat_id,
        "crash_recovery",
        rendered_card,
        &event.id,
    )?;
    wal.record_recovery_card_delivery(batch_id, &event.id, &message_id)
}

/// 投递一条 outbox 事件(消费者用)。本期只有 `NotifyChat`:re-resolve 该 agent 的 messaging,
/// 复用 [`deliver_notify`] 发飞书(dry-run agent 短路 `Ok` → 视为成功 ACK,不无限重试)。`ResumeTurn`
/// 本期 schema-only、不构造也不投递,遇到只 log + 视为成功归档(防卡死,不该出现)。
///
#[derive(Clone)]
pub(crate) struct OutboxDrainAgent {
    pub(crate) agent: Agent,
    pub(crate) client: Arc<FeishuClient>,
    pub(super) epoch: FeishuEpochIdentity,
}

impl OutboxDrainAgent {
    /// 盘上这份 agent 与本代投递身份(含 `.env` 凭据)是否仍是同一个。
    pub(crate) fn still_matches(&self, current: &Agent) -> bool {
        self.epoch.matches_current(current)
    }

    /// 本代 daemon 持有的飞书投递身份;outbox 事件的 `delivery_scope` 与它一致才投递。
    pub(crate) fn delivery_scope(&self) -> &str {
        &self.epoch.storage_scope().key
    }
}

/// 测试用:以给定凭据冻结身份的投递句柄(dry-run client)。
#[cfg(test)]
pub(crate) fn drain_agent_for_test(
    agent: &Agent,
    app_id: &str,
    app_secret: &str,
) -> OutboxDrainAgent {
    OutboxDrainAgent {
        agent: agent.clone(),
        client: Arc::new(FeishuClient::new(
            "feishu.cn",
            app_id.to_string(),
            app_secret.to_string(),
            true,
        )),
        epoch: FeishuEpochIdentity::from_resolved(agent, Some(app_id), Some(app_secret)),
    }
}

/// `drain_agents`:`ev.agent` → daemon epoch 快照。找不到该 agent → Err(让其重试/封顶)。
pub(super) fn deliver_outbox_event(
    drain_agents: &HashMap<String, OutboxDrainAgent>,
    ev: &summon_core::outbox::OutboxEvent,
) -> anyhow::Result<()> {
    deliver_outbox_event_with_wal(
        drain_agents,
        ev,
        &crate::runtime::inbox_wal::InboxWal::new(run_dir().join("inbox")),
    )
}

fn deliver_outbox_event_with_wal(
    drain_agents: &HashMap<String, OutboxDrainAgent>,
    ev: &summon_core::outbox::OutboxEvent,
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
) -> anyhow::Result<()> {
    use summon_core::outbox::OutboxKind;
    if matches!(
        &ev.kind,
        summon_core::outbox::OutboxKind::RecoveryCard { .. }
    ) && recovery_delivery_preflight(ev, inbox_wal)? == RecoveryDeliveryPreflight::LocalAck
    {
        // callback 已终态化，或 POST message_id 已 durable 但 outbox 尚未 archive：只补本地
        // ACK，绝不再次联网，也不要求旧 agent 仍在 registry 或 scope 匹配当前 app。
        return Ok(());
    }
    let Some(drain_agent) = drain_agents.get(&ev.agent) else {
        anyhow::bail!(summon_i18n::format!("r8729e02fde", ev.agent));
    };
    let agent = &drain_agent.agent;
    let fc = &drain_agent.client;
    ensure_bridge_epoch_current(agent, &drain_agent.epoch)?;
    anyhow::ensure!(
        !ev.delivery_scope.is_empty() && ev.delivery_scope == drain_agent.epoch.storage_scope().key,
        summon_i18n::format!("rc396a14396")
    );
    let bridge_store = crate::bridge::store::BridgeStore::runtime_default();
    match &ev.kind {
        OutboxKind::AgentDisabled {
            chat_id,
            chat_type,
            rendered_card,
        } => crate::frontend::outbound::send_card_idempotent_archived_id(
            fc,
            &agent.state_dir(),
            chat_type,
            chat_id,
            "agent_disabled",
            rendered_card,
            &ev.id,
        )
        .map(|_| ()),
        OutboxKind::NotifyChat {
            chat_id,
            chat_type,
            creator,
            event_operator,
            reason,
            text,
            rendered_card,
        } => {
            ensure_local_delivery_bridge_clear(
                &drain_agent.epoch,
                chat_type,
                chat_id,
                &bridge_store,
            )?;
            // text 与卡片都用入队快照(跨重启模板可能变);`Some(空)` 代表 daemon 升级前入队的
            // 旧事件,由 deliver_notify 回退纯文本,不丢消息。chat_name 传 None:卡片已定格,
            // 回退文本也不需要它。
            deliver_notify(
                fc,
                agent,
                chat_type,
                chat_id,
                None,
                creator.as_deref(),
                event_operator.as_deref(),
                *reason,
                text,
                Some(rendered_card.as_str()),
                None,
            )
        }
        OutboxKind::ResumeTurn {
            chat_id, chat_type, ..
        } => {
            ensure_local_delivery_bridge_clear(
                &drain_agent.epoch,
                chat_type,
                chat_id,
                &bridge_store,
            )?;
            // 本期不产 ResumeTurn(续跑仍走 pending.json,见设计 §6 决策 B)。万一出现:不重投,归档。
            log(&summon_i18n::format!("r896dccf3e5", ev.id));
            Ok(())
        }
        OutboxKind::RecoveryCard {
            batch_id,
            scope,
            chat_id,
            chat_type,
            ..
        } => {
            anyhow::ensure!(
                !batch_id.is_empty()
                    && batch_id == &ev.id
                    && scope == &ev.delivery_scope
                    && scope == &drain_agent.epoch.storage_scope().key,
                summon_i18n::format!("r1d90b10fc0")
            );
            ensure_local_delivery_bridge_clear(
                &drain_agent.epoch,
                chat_type,
                chat_id,
                &bridge_store,
            )?;
            deliver_recovery_card_with_wal(agent, fc, ev, inbox_wal)
        }
        OutboxKind::ProcessSupplement {
            chat_id,
            chat_type,
            thread_anchor,
            rendered_card,
        } => {
            ensure_local_delivery_bridge_clear(
                &drain_agent.epoch,
                chat_type,
                chat_id,
                &bridge_store,
            )?;
            let chat = ChatRef {
                thread_anchor: thread_anchor.as_deref(),
                chat_id,
                chat_type,
                agent_dir: &agent.state_dir(),
                agent_name: &agent.name,
                delivery_scope: &drain_agent.epoch.storage_scope().key,
            };
            crate::frontend::outbound::send_process_supplement_archived(
                fc,
                &chat,
                rendered_card,
                &ev.id,
            )
        }
        OutboxKind::TurnReply {
            chat_id,
            chat_type,
            text,
            parent_message_id: _,
        } => {
            ensure_local_delivery_bridge_clear(
                &drain_agent.epoch,
                chat_type,
                chat_id,
                &bridge_store,
            )?;
            // 网络恢复后补发 agent 回复:作为新 markdown 消息发到原 chat。
            // 不做流式卡片对账(可能已经过了很久)，但必须复用真实飞书
            // frontend 的转义/mention 预算分段，不能把全文再塞回一张超限卡。
            let frontend = record_frontend(agent, fc.as_ref());
            let chat = ChatRef {
                thread_anchor: None,
                chat_id,
                chat_type,
                agent_dir: &agent.state_dir(),
                agent_name: &agent.name,
                delivery_scope: &drain_agent.epoch.storage_scope().key,
            };
            deliver_turn_reply(frontend.as_ref(), &chat, text)
                .with_context(|| summon_i18n::format!("r3f1c296a69"))
        }
        OutboxKind::BridgeActivationAudit { .. } => {
            deliver_bridge_activation(agent, &drain_agent.epoch, fc, ev)
        }
        OutboxKind::ExternalBridgeTurn { .. } => {
            deliver_external_bridge_turn(agent, &drain_agent.epoch, fc, ev)
        }
        OutboxKind::BridgeContextCapsule { .. } => {
            deliver_bridge_context(agent, &drain_agent.epoch, fc, ev)
        }
        OutboxKind::BridgeHandoffStatus { .. } => {
            deliver_bridge_handoff_status(agent, &drain_agent.epoch, fc, ev)
        }
    }
}

fn exact_bridge_state<'a>(
    epoch: &FeishuEpochIdentity,
    event: &summon_core::outbox::OutboxEvent,
    fence: &summon_core::outbox::BridgeDeliveryFence,
    state: &'a summon_core::bridge::BridgeState,
    required_slot: fn(&summon_core::bridge::ChatSlot) -> Option<&summon_core::bridge::ChatBinding>,
    allow_historical_generation: bool,
) -> anyhow::Result<Option<&'a summon_core::bridge::ChatBinding>> {
    use summon_core::bridge::ScopedChatKey;
    if event.agent != epoch.agent_name()
        || event.delivery_scope != fence.scope
        || fence.agent != event.agent
        || fence.scope != epoch.storage_scope().key
        || !matches!(fence.chat_type.as_str(), "group" | "topic_group")
        || fence.lease_binding_id != fence.binding_id
        || fence.sequence == 0
    {
        return Ok(None);
    }
    let target = ScopedChatKey::new(&fence.scope, &fence.agent, &fence.chat_id);
    let Some(binding) = state.slot(&target).and_then(required_slot) else {
        return Ok(None);
    };
    if binding.id != fence.binding_id
        || binding.revision != fence.binding_revision
        || binding.target != target
        || binding.target_chat_type != fence.chat_type
        || binding.session != fence.session
    {
        return Ok(None);
    }
    let record = state
        .session(&binding.session)
        .context(summon_i18n::text!("r91d26896db"))?;
    let generation_matches = if allow_historical_generation {
        fence.generation > 0 && fence.generation <= record.generation
    } else {
        fence.generation == record.generation
    };
    if !generation_matches {
        return Ok(None);
    }
    match state.session_leases.get(&binding.session.storage_key()) {
        Some(lease)
            if lease.binding_id == fence.lease_binding_id && lease.target == binding.target => {}
        _ => anyhow::bail!(summon_i18n::format!("r69594e81ae")),
    }
    Ok(Some(binding))
}

fn activating_slot(
    slot: &summon_core::bridge::ChatSlot,
) -> Option<&summon_core::bridge::ChatBinding> {
    match slot {
        summon_core::bridge::ChatSlot::Activating(binding) => Some(binding),
        _ => None,
    }
}

fn mirror_slot(slot: &summon_core::bridge::ChatSlot) -> Option<&summon_core::bridge::ChatBinding> {
    match slot {
        summon_core::bridge::ChatSlot::MirrorBound(binding) => Some(binding),
        _ => None,
    }
}

/// 正文发送与所有 stop/chat-gone CAS 共用 daemon 内 per-binding gate。网络期间不持
/// BridgeStore flock，Claude Hook 热路径不会被慢 POST 卡住。
fn with_serialized_bridge_delivery<T>(
    fence: &summon_core::outbox::BridgeDeliveryFence,
    deliver: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let target =
        summon_core::bridge::ScopedChatKey::new(&fence.scope, &fence.agent, &fence.chat_id);
    crate::bridge::runtime::with_chat_delivery_gate(&target, deliver)
}

fn deliver_bridge_activation(
    agent: &Agent,
    epoch: &FeishuEpochIdentity,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
) -> anyhow::Result<()> {
    let store = crate::bridge::store::BridgeStore::runtime_default();
    deliver_bridge_activation_with_store_at_epoch(agent, epoch, fc, event, &store)
}

#[cfg(test)]
pub(super) fn deliver_bridge_activation_with_store(
    agent: &Agent,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    let epoch = FeishuEpochIdentity::capture(agent);
    deliver_bridge_activation_with_store_at_epoch(agent, &epoch, fc, event, store)
}

fn deliver_bridge_activation_with_store_at_epoch(
    agent: &Agent,
    epoch: &FeishuEpochIdentity,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    use summon_core::bridge::{ActivationAuditReceipt, BridgeError, TicketStatus};
    use summon_core::outbox::{bridge_delivery_id, BridgeDeliveryKind, OutboxKind};
    let OutboxKind::BridgeActivationAudit {
        fence,
        ticket_id,
        choice_id,
        requester_open_id,
        card_version,
        rendered_card,
        control_card,
    } = &event.kind
    else {
        unreachable!()
    };
    anyhow::ensure!(
        event.id == bridge_delivery_id(BridgeDeliveryKind::ActivationAudit, &fence.receipt_id),
        summon_i18n::format!("rd7a23cda26")
    );
    anyhow::ensure!(
        summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card)
            && summon_core::outbox::valid_frozen_bridge_card(*card_version, control_card),
        summon_i18n::format!("raa7c9257bc")
    );
    ensure_bridge_epoch_current(agent, epoch)?;
    with_serialized_bridge_delivery(fence, || {
        let Some(state) = store.load_state()? else {
            return Ok(());
        };

        // 已完成但 outbox ACK 尚未落盘：不重复发审计，只补控制卡/ACK。
        let target =
            summon_core::bridge::ScopedChatKey::new(&fence.scope, &fence.agent, &fence.chat_id);
        if let Some(summon_core::bridge::ChatSlot::MirrorBound(binding)) = state.slot(&target) {
            let receipt_exact = state
                .activation_audit_receipts
                .get(&fence.binding_id)
                .is_some_and(|receipt| {
                    receipt.id == fence.receipt_id
                        && receipt.outbox_id == event.id
                        && receipt.binding_revision == fence.binding_revision
                        && receipt.sequence == fence.sequence
                });
            if binding.id == fence.binding_id
                && binding.revision == fence.binding_revision
                && receipt_exact
            {
                update_mirror_control_card(
                    agent,
                    fc,
                    &state,
                    ticket_id,
                    control_card,
                    &event.id,
                    store,
                )?;
                return Ok(());
            }
            return Ok(());
        }

        let Some(binding) =
            exact_bridge_state(epoch, event, fence, &state, activating_slot, false)?
        else {
            // scope/binding 已撤销或替换：审计事件没有正文，直接 ACK/drop。
            return Ok(());
        };
        let Some(ticket) = state.tickets.get(ticket_id) else {
            return Ok(());
        };
        if ticket.requester_open_id != *requester_open_id
            || ticket.target != binding.target
            || !ticket
                .choices
                .iter()
                .any(|choice| choice.id == *choice_id && choice.session == binding.session)
            || !matches!(
                &ticket.status,
                TicketStatus::Activating {
                    binding_id,
                    choice_id: current_choice,
                    mode: summon_core::bridge::BridgeMode::Mirror,
                } if binding_id == &binding.id && current_choice == choice_id
            )
        {
            return Ok(());
        }
        if !current_bridge_operator_authorized_at_epoch(
            agent,
            epoch,
            &ticket.requester_open_id,
            &ticket.requester_union_id,
        )? {
            cancel_exact_activation(store, ticket_id, &fence.binding_id)?;
            return Ok(());
        }
        let record = state
            .session(&binding.session)
            .context(summon_i18n::text!("re61e3599cb"))?;
        if !record.locator.profile.process_verified
            || record.process.as_ref().is_none_or(|process| {
                !process.is_complete() || process.executable != record.locator.profile.claude_bin
            })
            || matches!(
                record.lifecycle,
                summon_core::bridge::ExternalLifecycle::Unverified
                    | summon_core::bridge::ExternalLifecycle::Ended
                    | summon_core::bridge::ExternalLifecycle::Conflict
            )
        {
            cancel_exact_activation(store, ticket_id, &fence.binding_id)?;
            return Ok(());
        }
        anyhow::ensure!(
            record.lifecycle == summon_core::bridge::ExternalLifecycle::ActiveIdle,
            summon_i18n::format!("ra96169e368")
        );
        let expected_process = record
            .process
            .as_ref()
            .context(summon_i18n::text!("rd47676d0f3"))?;
        if !activation_process_is_alive(expected_process)? {
            cancel_exact_activation(store, ticket_id, &fence.binding_id)?;
            return Ok(());
        }
        let generation = record.generation;
        let receipt_exists = state
            .activation_audit_receipts
            .get(&binding.id)
            .is_some_and(|receipt| {
                receipt.id == fence.receipt_id
                    && receipt.outbox_id == event.id
                    && receipt.ticket_id == *ticket_id
                    && receipt.binding_revision == binding.revision
                    && receipt.target == binding.target
                    && receipt.session == binding.session
                    && receipt.generation == generation
                    && receipt.sequence == fence.sequence
            });
        if !receipt_exists {
            send_card_idempotent_archived_id(
                fc,
                &agent.state_dir(),
                &fence.chat_type,
                &fence.chat_id,
                "bridge_activation_audit",
                rendered_card,
                &event.id,
            )?;
            let receipt = ActivationAuditReceipt {
                id: fence.receipt_id.clone(),
                outbox_id: event.id.clone(),
                ticket_id: ticket_id.clone(),
                binding_id: binding.id.clone(),
                binding_revision: binding.revision,
                target: binding.target.clone(),
                session: binding.session.clone(),
                generation,
                sequence: fence.sequence,
                delivered_at_ms: now_unix_ms(),
            };
            match store.with_state(|state| {
                state
                    .record_activation_audit(receipt.clone())
                    .map_err(anyhow::Error::new)
            }) {
                Ok(()) => {}
                Err(error)
                    if error.downcast_ref::<BridgeError>().is_some_and(|error| {
                        matches!(
                            error,
                            BridgeError::BindingMismatch
                                | BridgeError::TicketMissing
                                | BridgeError::TicketFinal
                        )
                    }) =>
                {
                    return Ok(())
                }
                Err(error) => return Err(error).context(summon_i18n::text!("r630fd630ef")),
            }
        }

        // 最终 CAS 前再次探测源进程；registry 的 ActiveIdle 不是存活证明。
        let expected_process = store.load_state()?.and_then(|state| {
            state
                .session(&fence.session)
                .and_then(|r| r.process.clone())
        });
        let live = match expected_process.as_ref() {
            Some(process) => activation_process_is_alive(process)?,
            None => false,
        };
        if !live {
            log(&summon_i18n::format!(
                "r641ff6bfe6",
                agent.name,
                fence.binding_id
            ));
            cancel_exact_activation(store, ticket_id, &fence.binding_id)?;
            return Ok(());
        }

        let completed = store.with_state(|state| {
            state
                .complete_mirror_after_audit(ticket_id, &fence.binding_id, &fence.receipt_id)
                .map_err(anyhow::Error::new)
        });
        match completed {
            Ok(_) => {
                let state = store
                    .load_state()?
                    .context(summon_i18n::text!("rb229515ea1"))?;
                update_mirror_control_card(
                    agent,
                    fc,
                    &state,
                    ticket_id,
                    control_card,
                    &event.id,
                    store,
                )
            }
            Err(error) => handle_activation_completion_error(error, || {
                cancel_exact_activation(store, ticket_id, &fence.binding_id)
            }),
        }
    })
}

fn handle_activation_completion_error(
    error: anyhow::Error,
    cancel: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use summon_core::bridge::BridgeError;

    if error.downcast_ref::<BridgeError>().is_some_and(|error| {
        matches!(
            error,
            BridgeError::BindingMismatch
                | BridgeError::TicketMissing
                | BridgeError::TicketFinal
                | BridgeError::StaleSession
                | BridgeError::SessionUnavailable
        )
    }) {
        // 只有取消状态已持久化后才能 ACK activation outbox；否则下一轮必须继续重试，避免
        // audit 已归档但 slot 永久停在 Activating。
        cancel().context(summon_i18n::text!("r0e30353eaf"))?;
        return Ok(());
    }
    Err(error).context("MirrorBound final CAS")
}

fn activation_process_is_alive(
    expected: &summon_core::bridge::ProcessFingerprint,
) -> anyhow::Result<bool> {
    match crate::bridge::process_probe::probe_exact_process(expected) {
        Ok(crate::bridge::process_probe::ExactProcessStatus::AliveExact) => Ok(true),
        Ok(crate::bridge::process_probe::ExactProcessStatus::Gone)
        | Ok(crate::bridge::process_probe::ExactProcessStatus::PidReused(_)) => Ok(false),
        Err(error) => Err(error).context(summon_i18n::text!("r58c9563b0c")),
    }
}

fn cancel_exact_activation(
    store: &crate::bridge::store::BridgeStore,
    ticket_id: &str,
    binding_id: &str,
) -> anyhow::Result<()> {
    store.with_state(|state| {
        let exact = state.tickets.get(ticket_id).is_some_and(|ticket| {
            matches!(
                &ticket.status,
                summon_core::bridge::TicketStatus::Activating {
                    binding_id: current,
                    ..
                } if current == binding_id
            )
        });
        if exact {
            state.cancel_ticket(ticket_id).map_err(anyhow::Error::new)?;
        }
        Ok(())
    })
}

fn update_mirror_control_card(
    agent: &Agent,
    fc: &FeishuClient,
    state: &summon_core::bridge::BridgeState,
    ticket_id: &str,
    control_card: &str,
    activation_outbox_id: &str,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    let Some(ticket) = state.tickets.get(ticket_id) else {
        return Ok(());
    };
    let (Some(private_chat_id), Some(message_id)) = (
        ticket.private_chat_id.as_deref(),
        ticket.control_card_message_id.as_deref(),
    ) else {
        anyhow::bail!(summon_i18n::format!("re7fc529f3f"))
    };
    let binding_id = match &ticket.status {
        summon_core::bridge::TicketStatus::Committed {
            binding_id,
            mode: summon_core::bridge::BridgeMode::Mirror,
            ..
        } => binding_id,
        _ => anyhow::bail!(summon_i18n::format!("ra2f7439ee5")),
    };
    if update_card_archived(
        fc,
        &agent.state_dir(),
        "p2p",
        private_chat_id,
        message_id,
        control_card,
    )
    .is_ok()
    {
        return Ok(());
    }

    // 旧卡过期/不可 PATCH 时发一张新控制卡，再以 committed binding 做 exact replacement。
    // 新卡发送或登记失败时保留 audit 队首，后续重试不会让正文越过控制面收口。
    let control_uuid = summon_core::outbox::bridge_delivery_id(
        summon_core::outbox::BridgeDeliveryKind::ActivationAudit,
        &format!("control-{activation_outbox_id}"),
    );
    let new_message_id = send_card_idempotent_archived_id(
        fc,
        &agent.state_dir(),
        "p2p",
        private_chat_id,
        "bridge_mirror_control",
        control_card,
        &control_uuid,
    )?;
    store.with_state(|state| {
        state
            .replace_bound_control_card(
                ticket_id,
                binding_id,
                &ticket.requester_open_id,
                private_chat_id,
                message_id,
                &new_message_id,
            )
            .map_err(anyhow::Error::new)
    })
}

fn deliver_external_bridge_turn(
    agent: &Agent,
    epoch: &FeishuEpochIdentity,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
) -> anyhow::Result<()> {
    let store = crate::bridge::store::BridgeStore::runtime_default();
    deliver_external_bridge_turn_with_store_at_epoch(agent, epoch, fc, event, &store)
}

#[cfg(test)]
pub(super) fn deliver_external_bridge_turn_with_store(
    agent: &Agent,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    let epoch = FeishuEpochIdentity::capture(agent);
    deliver_external_bridge_turn_with_store_at_epoch(agent, &epoch, fc, event, store)
}

fn deliver_external_bridge_turn_with_store_at_epoch(
    agent: &Agent,
    epoch: &FeishuEpochIdentity,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    use summon_core::outbox::{bridge_delivery_id, BridgeDeliveryKind, OutboxKind};
    let OutboxKind::ExternalBridgeTurn {
        fence,
        turn_id,
        card_version,
        rendered_card,
    } = &event.kind
    else {
        unreachable!()
    };
    anyhow::ensure!(
        event.id == bridge_delivery_id(BridgeDeliveryKind::ExternalTurn, &fence.receipt_id),
        summon_i18n::format!("r19c16b95c5")
    );
    anyhow::ensure!(
        summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card),
        summon_i18n::format!("r09e3bc5de9")
    );
    ensure_bridge_epoch_current(agent, epoch)?;
    with_serialized_bridge_delivery(fence, || {
        let Some(state) = store.load_state()? else {
            return Ok(());
        };
        let Some(binding) = exact_bridge_state(epoch, event, fence, &state, mirror_slot, true)?
        else {
            return Ok(());
        };
        let ticket = state.tickets.values().find(|ticket| {
            ticket.target == binding.target
                && ticket.requester_open_id == binding.actor_open_id
                && matches!(
                    &ticket.status,
                    summon_core::bridge::TicketStatus::Committed {
                        binding_id,
                        mode: summon_core::bridge::BridgeMode::Mirror,
                        ..
                    } if binding_id == &binding.id
                )
        });
        let Some(ticket) = ticket else {
            anyhow::bail!(summon_i18n::format!("r4710e35061"))
        };
        if !current_bridge_operator_authorized_at_epoch(
            agent,
            epoch,
            &ticket.requester_open_id,
            &ticket.requester_union_id,
        )? {
            let target = binding.target.clone();
            let binding_id = binding.id.clone();
            let revision = binding.revision;
            store.with_state(|state| {
                state
                    .stop_mirror(&target, &binding_id, revision)
                    .map(|_| ())
                    .map_err(anyhow::Error::new)
            })?;
            return Ok(());
        }
        let Some(receipt) = state.mirror_export_receipts.get(&fence.receipt_id) else {
            if state.pending_mirror_turns.get(turn_id).is_some_and(|turn| {
                turn.binding_id == fence.binding_id
                    && turn.binding_revision == fence.binding_revision
            }) {
                anyhow::bail!(summon_i18n::format!("rcd255d7cb4"))
            }
            anyhow::bail!(summon_i18n::format!("rbd61089183"))
        };
        anyhow::ensure!(
            receipt.outbox_id == event.id
                && receipt.turn_id == *turn_id
                && receipt.binding_id == fence.binding_id
                && receipt.binding_revision == fence.binding_revision
                && receipt.target.scope == fence.scope
                && receipt.target.agent == fence.agent
                && receipt.target.chat_id == fence.chat_id
                && receipt.session == fence.session
                && receipt.generation == fence.generation
                && receipt.sequence == fence.sequence,
            summon_i18n::format!("r03c4949bed")
        );
        if receipt.delivered_at_ms.is_some() {
            return Ok(());
        }
        drop(state);
        send_card_idempotent_archived_id(
            fc,
            &agent.state_dir(),
            &fence.chat_type,
            &fence.chat_id,
            "external_bridge_turn",
            rendered_card,
            &event.id,
        )?;
        store.with_state(|state| {
            anyhow::ensure!(
                exact_bridge_state(epoch, event, fence, state, mirror_slot, true)?.is_some(),
                summon_i18n::format!("r7d5439233a")
            );
            state
                .mark_mirror_export_delivered(&fence.receipt_id, &event.id, now_unix_ms())
                .map_err(anyhow::Error::new)
                .context(summon_i18n::text!("r4fe4f7506d"))
        })
    })
}

fn deliver_bridge_context(
    agent: &Agent,
    epoch: &FeishuEpochIdentity,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
) -> anyhow::Result<()> {
    use summon_core::outbox::{bridge_delivery_id, BridgeDeliveryKind, OutboxKind};
    let OutboxKind::BridgeContextCapsule {
        fence,
        ticket_id,
        choice_id,
        requester_open_id,
        card_version,
        rendered_card,
    } = &event.kind
    else {
        unreachable!()
    };
    anyhow::ensure!(
        event.id == bridge_delivery_id(BridgeDeliveryKind::ContextCapsule, &fence.receipt_id),
        summon_i18n::format!("rd5c0d0d0f4")
    );
    anyhow::ensure!(
        summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card),
        summon_i18n::format!("r97cbd1451e")
    );
    ensure_bridge_epoch_current(agent, epoch)?;
    let store = crate::bridge::store::BridgeStore::runtime_default();
    with_serialized_bridge_delivery(fence, || {
        let Some(state) = store.load_state()? else {
            return Ok(());
        };
        let Some(_) = exact_bridge_state(epoch, event, fence, &state, mirror_slot, true)? else {
            return Ok(());
        };
        let Some(ticket) = state.tickets.get(ticket_id) else {
            return Ok(());
        };
        if ticket.requester_open_id != *requester_open_id
            || !matches!(
                &ticket.status,
                summon_core::bridge::TicketStatus::Committed {
                    binding_id,
                    choice_id: current_choice,
                    mode: summon_core::bridge::BridgeMode::Mirror,
                } if binding_id == &fence.binding_id && current_choice == choice_id
            )
        {
            return Ok(());
        }
        if !current_bridge_operator_authorized_at_epoch(
            agent,
            epoch,
            &ticket.requester_open_id,
            &ticket.requester_union_id,
        )? {
            let binding = state
                .slot(&ticket.target)
                .and_then(summon_core::bridge::ChatSlot::binding)
                .context(summon_i18n::text!("r5ede14f094"))?
                .clone();
            store.with_state(|state| {
                state
                    .stop_mirror(&binding.target, &binding.id, binding.revision)
                    .map(|_| ())
                    .map_err(anyhow::Error::new)
            })?;
            return Ok(());
        }
        let Some(receipt) = state.context_export_receipts.get(&fence.receipt_id) else {
            anyhow::bail!(summon_i18n::format!("r99d8b51641"))
        };
        anyhow::ensure!(
            receipt.outbox_id == event.id
                && receipt.ticket_id == *ticket_id
                && receipt.choice_id == *choice_id
                && receipt.requester_open_id == *requester_open_id
                && receipt.binding_id == fence.binding_id
                && receipt.binding_revision == fence.binding_revision
                && receipt.target.scope == fence.scope
                && receipt.target.agent == fence.agent
                && receipt.target.chat_id == fence.chat_id
                && receipt.target_chat_type == fence.chat_type
                && receipt.session == fence.session
                && receipt.generation == fence.generation
                && receipt.sequence == fence.sequence,
            summon_i18n::format!("raff8a98271")
        );
        if receipt.delivered_at_ms.is_some() {
            return Ok(());
        }
        drop(state);
        send_card_idempotent_archived_id(
            fc,
            &agent.state_dir(),
            &fence.chat_type,
            &fence.chat_id,
            "bridge_context_capsule",
            rendered_card,
            &event.id,
        )?;
        store.with_state(|state| {
            anyhow::ensure!(
                exact_bridge_state(epoch, event, fence, state, mirror_slot, true)?.is_some(),
                summon_i18n::format!("rc9a101e404")
            );
            state
                .mark_context_export_delivered(&fence.receipt_id, &event.id, now_unix_ms())
                .map_err(anyhow::Error::new)
                .context(summon_i18n::text!("r005a2e121d"))
        })
    })
}

fn deliver_bridge_handoff_status(
    agent: &Agent,
    epoch: &FeishuEpochIdentity,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
) -> anyhow::Result<()> {
    let store = crate::bridge::store::BridgeStore::runtime_default();
    deliver_bridge_handoff_status_with_store_at_epoch(agent, epoch, fc, event, &store)
}

#[cfg(test)]
fn deliver_bridge_handoff_status_with_store(
    agent: &Agent,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    let epoch = FeishuEpochIdentity::capture(agent);
    deliver_bridge_handoff_status_with_store_at_epoch(agent, &epoch, fc, event, store)
}

fn deliver_bridge_handoff_status_with_store_at_epoch(
    agent: &Agent,
    epoch: &FeishuEpochIdentity,
    fc: &FeishuClient,
    event: &summon_core::outbox::OutboxEvent,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    use summon_core::outbox::{bridge_delivery_id, BridgeDeliveryKind, OutboxKind};
    let OutboxKind::BridgeHandoffStatus {
        fence,
        attempt_id,
        notice,
        card_version,
        rendered_card,
    } = &event.kind
    else {
        unreachable!()
    };
    anyhow::ensure!(
        event.id == bridge_delivery_id(BridgeDeliveryKind::HandoffStatus, &fence.receipt_id),
        summon_i18n::format!("r4a68ece42f")
    );
    anyhow::ensure!(
        summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card),
        summon_i18n::format!("r67fb65a0d5")
    );
    anyhow::ensure!(
        event.agent == epoch.agent_name()
            && event.delivery_scope == fence.scope
            && fence.agent == event.agent
            && fence.scope == epoch.storage_scope().key,
        summon_i18n::format!("rcc08359cbf")
    );
    ensure_bridge_epoch_current(agent, epoch)?;
    with_serialized_bridge_delivery(fence, || {
        let Some(attempt) = store
            .load_handoff_attempts()?
            .into_iter()
            .find(|attempt| attempt.attempt_id() == attempt_id)
        else {
            return Ok(());
        };
        anyhow::ensure!(
            attempt.binding_id() == fence.binding_id
                && attempt.binding_revision() == fence.binding_revision
                && attempt.target().scope == fence.scope
                && attempt.target().agent == fence.agent
                && attempt.target().chat_id == fence.chat_id
                && attempt.target_chat_type() == fence.chat_type
                && attempt.source().session() == &fence.session
                && attempt.source().generation() == fence.generation
                && fence.lease_binding_id == fence.binding_id,
            summon_i18n::format!("r62c78da03d")
        );
        if !crate::bridge::handoff::handoff_notice_is_current(&attempt, *notice) {
            return Ok(());
        }
        let state = store.load_state()?.unwrap_or_default();
        match notice {
            summon_core::outbox::BridgeHandoffNotice::Requested
            | summon_core::outbox::BridgeHandoffNotice::TerminatingSource
            | summon_core::outbox::BridgeHandoffNotice::Resuming => {
                let binding = match state.slot(attempt.target()) {
                    Some(summon_core::bridge::ChatSlot::HandoffPending(binding))
                        if binding.id == fence.binding_id
                            && binding.revision == fence.binding_revision
                            && binding.session == fence.session =>
                    {
                        binding
                    }
                    _ => return Ok(()),
                };
                match state.session_leases.get(&fence.session.storage_key()) {
                    Some(lease)
                        if lease.binding_id == binding.id && lease.target == binding.target => {}
                    _ => anyhow::bail!(summon_i18n::format!("r7ee4ce202d")),
                }
            }
            summon_core::outbox::BridgeHandoffNotice::Completed => {
                let pending_exact = match state.slot(attempt.target()) {
                    Some(summon_core::bridge::ChatSlot::HandoffPending(binding))
                        if binding.id == fence.binding_id
                            && binding.revision == fence.binding_revision
                            && binding.session == fence.session =>
                    {
                        match state.session_leases.get(&fence.session.storage_key()) {
                            Some(lease)
                                if lease.binding_id == binding.id
                                    && lease.target == binding.target => {}
                            _ => anyhow::bail!(summon_i18n::format!("r1c8930a4ee")),
                        }
                        true
                    }
                    _ => false,
                };
                let managed_exact = state
                    .managed_takeovers
                    .get(&attempt.target().storage_key())
                    .is_some_and(|receipt| {
                        receipt.binding_id == fence.binding_id
                            && receipt.binding_revision == fence.binding_revision
                            && receipt.session == fence.session
                    });
                if !pending_exact && !managed_exact {
                    return Ok(());
                }
            }
            summon_core::outbox::BridgeHandoffNotice::Failed => {
                if state.slot(attempt.target()).is_some()
                    || state
                        .managed_takeovers
                        .contains_key(&attempt.target().storage_key())
                    || state
                        .session_leases
                        .contains_key(&fence.session.storage_key())
                {
                    return Ok(());
                }
            }
        }
        drop(state);
        send_card_idempotent_archived_id(
            fc,
            &agent.state_dir(),
            &fence.chat_type,
            &fence.chat_id,
            "bridge_handoff_status",
            rendered_card,
            &event.id,
        )?;
        Ok(())
    })
}

/// 一个 chat 的入站恢复分类结果(纯决策产物,见 [`classify_inbox_recovery`])。
struct InboxDisposition {
    /// 优雅重启 + 未开跑:重注入 dispatcher。已把 `create_time_ms` 置 0(豁免时效/水位闸,只留
    /// 去重;与 `cards.rs` 恢复按钮统一),并按原 create_time 升序(处理顺序=到达顺序)。
    /// **不立即销账**——留在 `pending/`,由 chat 线程处理完再 `mark_consumed`(投递成功才 ack,
    /// 顺带避开「去重预热竞态」:未迁到 consumed 就不会被 preheat_seen 误当已消费而 DropDuplicate)。
    reinject: Vec<Inbound>,
    /// schedule 合成入站无论优雅/崩溃、started 与否都自动重注入。它的 fire_id 是持久幂等键，
    /// 不进真人恢复卡，也不提前销账；chat turn 完成后统一 mark_consumed。
    scheduled_reinject: Vec<crate::runtime::inbox_wal::ScheduledInbox>,
    /// 已开跑但没跑完的定时 fire:**退票,不补跑**(owner 2026-08-13 定)。销账后不再重注入。
    scheduled_retire: Vec<crate::runtime::inbox_wal::ScheduledInbox>,
    /// sentinel 合成入站,恢复语义与 scheduled 一致:fire_id 幂等、自动重注入、不进恢复卡。
    sentinel_reinject: Vec<crate::runtime::inbox_wal::SentinelInbox>,
    /// 需要进入 durable RecoveryBatch 的完整 WAL 记录。不能只保留 preview/mid：恢复按钮必须
    /// 从 exact batch 取回原 sender/chat/scope，并且 outbox durable 前不得删除 source。
    recovery_records: Vec<crate::runtime::inbox_wal::InboxRecord>,
    /// 优雅重启 + 已开跑 + Transcript + 盘上有效锚点:那一轮还在 tmux 里跑,以 `reattach=true`
    /// 重注入交新一代接管。**不销账、不发卡、不进 `reinject`** —— 一条 WAL 记录只走一条路。
    /// 与 `reinject` 一样打 `replayed`(入站闸对两者都免检),但 `create_time_ms` 保真:锚点要拿
    /// 原始 message_id 与 create_time 跟 WAL 对账。
    reattach: Vec<Inbound>,
    /// 已恢复 replay 上次开跑后结果未知：原 batch 已退回 Awaiting，必须 hold 等用户再次确认。
    /// 不能造嵌套 RecoveryBatch，也不能让 headless pending.json 自动 resume。
    holds_interrupted_recovery: bool,
}

const RECOVERY_CARD_MAX_ITEMS: usize = 12;
const RECOVERY_CARD_MAX_BYTES: usize = 24 * 1024;

fn partition_recovery_records(
    records: &[crate::runtime::inbox_wal::InboxRecord],
) -> Vec<Vec<crate::runtime::inbox_wal::InboxRecord>> {
    let mut chunks = Vec::<Vec<crate::runtime::inbox_wal::InboxRecord>>::new();
    let mut current = Vec::new();
    for record in records {
        let mut candidate = current.clone();
        candidate.push(record.clone());
        let pairs = candidate
            .iter()
            .map(|record| {
                (
                    record.inbound.message_id.as_str(),
                    summon_core::quote::quote_preview(&record.inbound.text),
                )
            })
            .collect::<Vec<_>>();
        let refs = pairs
            .iter()
            .map(|(message_id, preview)| (*message_id, preview.as_str()))
            .collect::<Vec<_>>();
        let wire = summon_core::feishu::card_crash_recovery(
            "recovery-card-00000000000000000000000000000000",
            "recovery",
            None,
            &refs,
            &[],
        );
        if !current.is_empty()
            && (candidate.len() > RECOVERY_CARD_MAX_ITEMS || wire.len() > RECOVERY_CARD_MAX_BYTES)
        {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(record.clone());
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn settle_terminal_inbox(
    wal: &crate::runtime::inbox_wal::InboxWal,
    records: Vec<crate::runtime::inbox_wal::InboxRecord>,
) -> anyhow::Result<Vec<crate::runtime::inbox_wal::InboxRecord>> {
    let mut runnable = Vec::new();
    for record in records {
        if record.superseded_by_new {
            wal.discard_superseded(
                &record.agent,
                &record.delivery_scope,
                &record.inbound.message_id,
            )?;
            continue;
        }
        if !wal.reject_disabled(
            &record.agent,
            &record.delivery_scope,
            &record.inbound.message_id,
        )? {
            runnable.push(record);
        }
    }
    Ok(runnable)
}

/// 纯决策:把一个 chat 的 WAL 记录按「消息粒度 started」+ capture 模式分成重注入/恢复卡/续跑保留
/// 三类。替代原 per-chat `has_pending` 粗粒度(C8:粗粒度会让 claude 在跑轮被重复执行、无头排队消息
/// 被整组跳过丢失)。
/// - `was_crash`:上次硬崩(SIGKILL)——一律进恢复卡(什么都不敢确定跑没跑完,#3 不自动重跑)。
/// - `is_headless`:该 agent 是 Subprocess(cursor/codex);Transcript(claude)为 false。
/// - `has_pending_json`:该 chat 盘上有无头续跑快照(dispatcher 会拉起 chat 线程 resume_pending_turn)。
/// - `has_transcript_anchor`:该 chat 盘上有一枚**全部判据都证得**的接管锚点
///   (见 `dispatcher::reattach_anchor_on_disk`:身份/来源/sid/tmux 存活/新鲜度)。它同时保证
///   锚点认领的 id 集合与本 chat 的 started 全集精确相等 —— 接管入口因此只需要一条。
fn classify_inbox_recovery(
    mut records: Vec<crate::runtime::inbox_wal::InboxRecord>,
    was_crash: bool,
    is_headless: bool,
    has_pending_json: bool,
    has_transcript_anchor: bool,
) -> InboxDisposition {
    let mut reinject: Vec<Inbound> = Vec::new();
    let mut reattach: Vec<Inbound> = Vec::new();
    let mut scheduled_reinject = Vec::new();
    let mut scheduled_retire = Vec::new();
    let mut sentinel_reinject = Vec::new();
    let mut recovery_records = Vec::new();
    let mut holds_interrupted_recovery = false;
    // read_dir 无顺序保证。先按原消息时间 + message_id 排序，让真人与 scheduled 各自的重注入
    // 顺序稳定 —— 同毫秒的两条也要有确定次序,否则「哪条当接管入口」会随盘上遍历顺序漂移。
    records.sort_by(|left, right| {
        (left.inbound.create_time_ms, &left.inbound.message_id)
            .cmp(&(right.inbound.create_time_ms, &right.inbound.message_id))
    });
    for rec in records {
        // 用户已点恢复、dispatcher 尚未真正 start 的 durable claim：无论上次是 crash 还是优雅
        // 停机，都自动重注入同一个 replay id。mark_started 后 started=true，会退回人工恢复以
        // 避免 CLI 已产生副作用时静默双跑。
        if rec.recovery_origin.is_some() {
            if rec.started {
                holds_interrupted_recovery = true;
            } else {
                reinject.push(rec.inbound);
            }
            continue;
        }
        if let Some(scheduled) = rec.scheduled {
            // **已开跑的定时 fire 退票,不补跑。**
            //
            // 原设计是「scheduled 无论 started 都按 fire_id 自动重放」,但这条路是死的:chat
            // 线程进 `handle_task` 前要过 `mark_started`,记录已 started 就返回 `AlreadyStarted`
            // 直接跳过。于是记录既跑不掉也销不了账,**每次 daemon 启动重注入一次、每次原样弹回**
            // —— devbox 上一条定时任务就这么空转了一整天(2026-08-12 那次 fire 起了头没跑完,
            // 之后每次重启都复现一遍)。
            //
            // 解开这个死结只有两条路,代价不对称:清掉 start 标记补跑,万一上一轮已经产生了
            // 副作用(定时任务可能做的是预约、下单这类外部动作)就是重复执行;退票不跑,
            // 则那次任务确实没做成。
            // owner 2026-08-13 选了退票 —— 不可逆的外部动作宁可漏做。
            //
            // 所以这里只销账 + 大声记日志,不发卡:启动时主动往群里发东西是红线区,而日常
            // schedule 明天还会照常触发,不需要为一次漏跑再开一条出站路径。
            if rec.started {
                scheduled_retire.push(scheduled);
            } else {
                scheduled_reinject.push(scheduled);
            }
            continue;
        }
        if let Some(sentinel) = rec.sentinel {
            sentinel_reinject.push(sentinel);
            continue;
        }
        if !was_crash && !rec.started {
            // 优雅 + 未开跑:安全重注入(从没跑过,零双执行风险)。不销账。
            reinject.push(rec.inbound);
        } else if !was_crash && rec.started && !is_headless && has_transcript_anchor {
            // 优雅 + 已开跑 + Transcript + 有效锚点:轮还在 tmux 里跑,由新一代 chat 线程接管。
            // 一轮只投一条接管入口:轮内被 steer 进来的那几条与主消息同生共死,由锚点一并持有
            // (接管失败时它们进同一张恢复卡)。多投一条就是对同一轮发起两次接管。
            // 其余同轮记录留在 pending —— 既不发卡也不销账,终态由接管轮给。
            if reattach.is_empty() {
                let mut inbound = rec.inbound;
                inbound.reattach = true;
                reattach.push(inbound);
            }
        } else if !was_crash && rec.started && is_headless && has_pending_json {
            // 优雅 + 已开跑无头轮:精确 checkpoint 会由 dispatcher 拉起 resume_pending_turn。
            // Inbox 必须继续保留到续跑终态；这里既不发卡，也绝不提前销账。
        } else {
            // 崩溃全部 / 优雅已开跑的 claude / 优雅已开跑无头但丢了 pending.json:恢复卡由用户确认。
            recovery_records.push(rec);
        }
    }
    // reinject 按原 create_time 升序(到达顺序),打 replayed 标记(daemon 显式重放的已知消息,
    // 不吃时效/水位闸,只保留 message_id 去重防双注入);create_time_ms 保真,WAL 留证。
    // message_id 是次键:同毫秒的两条否则由 read_dir 顺序决定,同一份盘上事实在两台机器上
    // 会排出不同的处理顺序。
    reinject.sort_by(|left, right| {
        (left.create_time_ms, &left.message_id).cmp(&(right.create_time_ms, &right.message_id))
    });
    for i in reinject.iter_mut().chain(reattach.iter_mut()) {
        i.replayed = true;
    }
    InboxDisposition {
        reinject,
        reattach,
        scheduled_reinject,
        scheduled_retire,
        sentinel_reinject,
        recovery_records,
        holds_interrupted_recovery,
    }
}

/// 本组要重注入 dispatcher 的入站,**接管轮排在排队消息前面**。
///
/// chat 线程是串行的:接管轮还在 tmux 里跑,排队消息得等它收尾。反过来投,排队那条会先被注入
/// 一条正忙着的 pane,而接管轮排在它后面 —— 等轮到时锚点早已过期。
fn injection_order(reattach: Vec<Inbound>, reinject: Vec<Inbound>) -> Vec<Inbound> {
    reattach.into_iter().chain(reinject).collect()
}

/// 接管新鲜度用的停机时刻(unix 秒)。**取 daemon 级那一个**,不是某条 mirror stop fence 的:
/// fence 只有「已提交过 mirror offset」的 chat 才签得出,首轮就被重启的会话一条都没有 ——
/// 拿 fence 的时刻当停机时刻,那种会话会永远判过期而不接管,还不留任何日志。
/// 没有停机记录(硬崩 / 旧格式)→ 0 → `fresh_at` 自动判过期。
fn reattach_stopped_at_secs(clean_record: Option<&super::CleanShutdownRecordV2>) -> u64 {
    clean_record.map_or(0, |record| record.stopped_at_secs)
}

fn recovery_card_event(
    batch: &crate::runtime::inbox_wal::RecoveryBatch,
) -> anyhow::Result<summon_core::outbox::OutboxEvent> {
    let items = batch
        .items
        .iter()
        .map(|item| summon_core::outbox::RecoveryItem {
            message_id: item.record.inbound.message_id.clone(),
            preview: item.preview.clone(),
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(!items.is_empty(), summon_i18n::format!("rc2af7b7cef"));
    let pairs = items
        .iter()
        .map(|item| (item.message_id.as_str(), item.preview.as_str()))
        .collect::<Vec<_>>();
    let tag = summon_core::registry::chat_tag(&batch.chat_type, &batch.chat_id);
    let rendered_card = summon_core::feishu::card_crash_recovery(
        &batch.id,
        &tag,
        batch.stage.as_deref(),
        &pairs,
        &[],
    );
    let card_version = summon_core::outbox::BRIDGE_CARD_WIRE_VERSION;
    anyhow::ensure!(
        summon_core::outbox::valid_frozen_bridge_card(card_version, &rendered_card),
        summon_i18n::format!("r20114797f6")
    );
    Ok(summon_core::outbox::OutboxEvent {
        id: batch.outbox_id.clone(),
        agent: batch.agent.clone(),
        delivery_scope: batch.scope.clone(),
        created_ms: batch.created_ms,
        attempts: 0,
        next_attempt_ms: batch.created_ms,
        last_error: None,
        kind: summon_core::outbox::OutboxKind::RecoveryCard {
            batch_id: batch.id.clone(),
            scope: batch.scope.clone(),
            chat_id: batch.chat_id.clone(),
            chat_type: batch.chat_type.clone(),
            items,
            stage: batch.stage.clone(),
            card_version,
            rendered_card,
        },
    })
}

/// 返回 true 表示 card 已在 pending 或已成功归档，可安全让 batch 接管 source WAL；dead 仍是
/// 未交付事实，必须保留 source + batch 等人工修复，不能谎称等待用户点击。
fn ensure_recovery_card_outbox(
    store: &crate::runtime::outbox_store::OutboxStore,
    batch: &crate::runtime::inbox_wal::RecoveryBatch,
) -> anyhow::Result<bool> {
    use crate::runtime::outbox_store::{EnqueueOnceOutcome, OutboxLocation};
    let event = recovery_card_event(batch)?;
    match store.ensure_recovery_once(&event)? {
        EnqueueOnceOutcome::Enqueued
        | EnqueueOnceOutcome::Existing(OutboxLocation::Pending, _)
        | EnqueueOnceOutcome::Existing(OutboxLocation::Archive, _) => Ok(true),
        EnqueueOnceOutcome::Existing(OutboxLocation::Dead, _) => Ok(false),
    }
}

/// 把一组仍在 pending 的 WAL 记录变成一张恢复卡:先 durable batch(含 exact scope/agent/chat
/// 与完整 Inbound),再按 batch id enqueue_once,只有 outbox 已 pending/archive 才删 source WAL。
/// 任一步失败都保留原记录 —— 恢复卡是这些消息**唯一**的去向,谎称接管等于把它们丢了。
///
/// 冷启扫描与接管失败共用这一份:两条路对「一条 pending 记录怎么转人工恢复」的答案必须逐字
/// 相同,分家复制会从第二天起漂移。`stage` 只有冷启的 partial 有,接管失败恒 `None`。
///
/// 返回 **卡是不是真的进了 outbox**(每一批都 pending/archive 才算)。死信、入队失败、一批都没
/// 生成都返回 `false`:那时记录仍在 pending、还有人接得住这一轮,调用方据此决定要不要撤 OnIt。
#[allow(clippy::too_many_arguments)] // identity 五元组必须逐字段传,收 struct 只是换个地方列
pub(super) fn emit_recovery_card(
    wal: &crate::runtime::inbox_wal::InboxWal,
    store: &crate::runtime::outbox_store::OutboxStore,
    delivery_scope: &str,
    agent_name: &str,
    chat_id: &str,
    chat_type: &str,
    stage: Option<String>,
    records: Vec<crate::runtime::inbox_wal::InboxRecord>,
) -> anyhow::Result<bool> {
    let chunks = partition_recovery_records(&records);
    let now = now_unix_ms();
    let mut batches = Vec::with_capacity(chunks.len());
    for (index, records) in chunks.into_iter().enumerate() {
        match wal.create_recovery_batch(
            delivery_scope,
            agent_name,
            chat_id,
            chat_type,
            (index == 0).then(|| stage.clone()).flatten(),
            records,
            now.saturating_add(index as u64),
        ) {
            Ok(batch) => batches.push(batch),
            Err(error) => {
                return Err(error).with_context(|| summon_i18n::format!("r5d894d7ede"));
            }
        }
    }
    let mut all_enqueued = !batches.is_empty();
    for batch in batches {
        match ensure_recovery_card_outbox(store, &batch) {
            Ok(true) => match wal.finalize_recovery_sources(&batch) {
                Ok(()) => log(&summon_i18n::format!("r58ef389c42")),
                Err(error) => log(&summon_i18n::format!("rcfdfcc8279")),
            },
            Ok(false) => {
                all_enqueued = false;
                log(&summon_i18n::format!("r867ad90b6b"));
            }
            Err(error) => {
                all_enqueued = false;
                log(&summon_i18n::format!("r8565950401"));
            }
        }
    }
    Ok(all_enqueued)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedScopeRelation {
    ExactRoutable,
    ExactUnavailable,
    KnownDifferent,
    Unknown,
}

/// 只有能解析出当前 app_id 时才能证明 scope 相同或已变化。agent 暂时 inactive、缺 env、
/// manifest 被移走都属于 Unknown/Unavailable，不能借一次误配置不可逆删除恢复正文。
fn persisted_scope_relation(
    reg: &Registry,
    agent_name: &str,
    persisted_scope: &str,
) -> PersistedScopeRelation {
    if persisted_scope.is_empty() {
        return PersistedScopeRelation::Unknown;
    }
    let Some(agent) = reg.get(agent_name) else {
        return PersistedScopeRelation::Unknown;
    };
    let (app_id, _) = resolve_creds(agent);
    if app_id.as_deref().is_none_or(str::is_empty) {
        return PersistedScopeRelation::Unknown;
    }
    if feishu_storage_scope(agent).key != persisted_scope {
        return PersistedScopeRelation::KnownDifferent;
    }
    if agent.is_routable() {
        PersistedScopeRelation::ExactRoutable
    } else {
        PersistedScopeRelation::ExactUnavailable
    }
}

/// 恢复卡的有效期。超过它还没人处理的批次,原消息早已失去重放意义(consumed 去重记录同样
/// 只留 7 天),却会一直把本群判成「有本地会话状态」挡住 `/bridge`。
const RECOVERY_BATCH_TTL_MS: u64 = 7 * 24 * 3600 * 1000;

fn recovery_batch_expired(created_ms: u64, now_ms: u64) -> bool {
    created_ms > 0 && now_ms.saturating_sub(created_ms) > RECOVERY_BATCH_TTL_MS
}

fn reconcile_recovery_batches(
    reg: &Registry,
    store: &crate::runtime::outbox_store::OutboxStore,
    wal: &crate::runtime::inbox_wal::InboxWal,
    now_ms: u64,
) -> anyhow::Result<()> {
    let batches = wal
        .active_recovery_batches()
        .context(summon_i18n::text!("r46a9d7b4e3"))?;
    for batch in batches {
        match persisted_scope_relation(reg, &batch.agent, &batch.scope) {
            PersistedScopeRelation::KnownDifferent => {
                abandon_stale_recovery_batch(store, wal, &batch)
                    .with_context(|| summon_i18n::format!("r4d02768e8f", batch.agent, batch.id))?;
                log(&summon_i18n::format!("rb10f01a740", batch.agent, batch.id));
                continue;
            }
            PersistedScopeRelation::ExactUnavailable | PersistedScopeRelation::Unknown => {
                log(&summon_i18n::format!("r4948984822", batch.agent, batch.id));
                continue;
            }
            PersistedScopeRelation::ExactRoutable => {}
        }
        // 只对身份证得一致的批次按时间作废:身份不可证明时连「它是不是这个 bot 的」都不知道,
        // 维持上面的隔离保留。出站侧只归档、不 PATCH 已发出的卡,不往群里发任何东西。
        if recovery_batch_expired(batch.created_ms, now_ms) {
            abandon_stale_recovery_batch(store, wal, &batch)
                .with_context(|| summon_i18n::format!("rc29c2c4a0f", batch.agent, batch.id))?;
            log(&summon_i18n::format!(
                "r148eb8f201",
                batch.agent,
                batch.chat_id,
                batch.id
            ));
            continue;
        }
        let Some(agent) = reg.get(&batch.agent) else {
            log(&summon_i18n::format!("rdddf8577b5", batch.agent, batch.id));
            continue;
        };
        match ensure_recovery_card_outbox(store, &batch) {
            Ok(true) => {
                wal.finalize_recovery_sources(&batch)
                    .with_context(|| summon_i18n::format!("r8431022e11", batch.agent, batch.id))?;
                let tag = chat_tag(&batch.chat_type, &batch.chat_id);
                crate::runtime::inflight::clear_pending_checked(&agent.state_dir(), &tag)
                    .with_context(|| summon_i18n::format!("ra8d948f7a4", batch.agent, batch.id))?;
            }
            Ok(false) => {
                let tag = chat_tag(&batch.chat_type, &batch.chat_id);
                crate::runtime::inflight::clear_pending_checked(&agent.state_dir(), &tag)
                    .with_context(|| summon_i18n::format!("r343e2ffec7", batch.agent, batch.id))?;
                log(&summon_i18n::format!("r8919020177", batch.agent, batch.id));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("[{}] reconcile recovery card {}", batch.agent, batch.id)
                })
            }
        }
    }
    Ok(())
}

/// 跨 app scope 没有安全的重投目标。先以 batch 权威 identity 写脱敏 outbox terminal，再把
/// active batch/replay/source 终态化；两步都幂等，任一步崩溃后冷启会继续补齐。
fn abandon_stale_recovery_batch(
    store: &crate::runtime::outbox_store::OutboxStore,
    wal: &crate::runtime::inbox_wal::InboxWal,
    batch: &crate::runtime::inbox_wal::RecoveryBatch,
) -> anyhow::Result<()> {
    let event = recovery_card_event(batch)?;
    store.archive_abandoned_recovery(&event)?;
    if let Some(outbox_id) = wal.abandon_recovery_batch(&batch.id)? {
        anyhow::ensure!(
            outbox_id == batch.outbox_id,
            summon_i18n::format!("ra01361faa9")
        );
    }
    Ok(())
}

fn migrate_legacy_recovery_cards(
    reg: &Registry,
    store: &crate::runtime::outbox_store::OutboxStore,
    wal: &crate::runtime::inbox_wal::InboxWal,
) {
    let scan = match store.scan_pending() {
        Ok(scan) => scan,
        Err(error) => {
            log(&summon_i18n::format!("r001dd19412"));
            return;
        }
    };
    for corrupt in &scan.corrupt_recovery {
        match store.quarantine_corrupt_recovery(corrupt) {
            Ok(()) => log(summon_i18n::text!("r888e8e4ff5")),
            Err(error) => log(&summon_i18n::format!("rc169158ff8")),
        }
    }
    for event in scan.events {
        let summon_core::outbox::OutboxKind::RecoveryCard {
            batch_id,
            scope,
            chat_id,
            chat_type,
            items,
            stage,
            card_version,
            rendered_card,
        } = &event.kind
        else {
            continue;
        };
        let legacy_wire = *card_version == 0 && rendered_card.is_empty();
        let legacy_unscoped = batch_id.is_empty() && scope.is_empty();
        if !legacy_unscoped && !legacy_wire {
            match wal.load_recovery_batch(batch_id) {
                Ok(Some(batch)) => {
                    if recovery_card_event(&batch)
                        .and_then(|candidate| store.ensure_recovery_once(&candidate).map(|_| ()))
                        .is_err()
                    {
                        if let Err(error) = store.quarantine_orphan_recovery(
                            &event,
                            "recovery_event_manifest_identity_conflict",
                        ) {
                            log(&summon_i18n::format!("rb7ec99825e", event.agent, event.id));
                        }
                    }
                }
                Ok(None) => match wal.recovery_outbox_completed_exact(
                    batch_id,
                    &event.id,
                    scope,
                    &event.agent,
                    chat_id,
                ) {
                    Ok(true) => {
                        if let Err(error) = store.archive_recovery_by_id(&event.id) {
                            log(&summon_i18n::format!("r3bbb4c7e7b", event.agent, event.id));
                        }
                    }
                    Ok(false) | Err(_) => {
                        if let Err(error) = store.quarantine_orphan_recovery(
                            &event,
                            "recovery_manifest_missing_or_corrupt",
                        ) {
                            log(&summon_i18n::format!("r39a99e06cd", event.agent, event.id));
                        }
                    }
                },
                Err(_) => {
                    if let Err(error) = store
                        .quarantine_orphan_recovery(&event, "recovery_manifest_missing_or_corrupt")
                    {
                        log(&summon_i18n::format!("re9d06c9a15", event.agent, event.id));
                    }
                }
            }
            continue;
        }
        let Some(agent) = reg.get(&event.agent).filter(|agent| agent.is_routable()) else {
            if legacy_unscoped {
                match store.archive_migrated_legacy_recovery(&event) {
                    Ok(()) => log(&summon_i18n::format!("rc207fcd865", event.agent, event.id)),
                    Err(error) => log(&summon_i18n::format!("r1618894f37", event.agent, event.id)),
                }
            } else {
                log(&summon_i18n::format!("raa14dbaf18", event.agent, event.id));
            }
            continue;
        };
        let current_scope = feishu_storage_scope(agent);

        // 上一版已经有 durable RecoveryBatch，只是 RecoveryCard 仍在投递时现渲染。必须从
        // 权威 manifest 重建 frozen wire，并严格原地升级同一 pending；不能换 UUID 重发，也
        // 不能让 renderer 缺失的旧事件直接进入网络路径。
        if !legacy_unscoped {
            if batch_id != &event.id
                || scope != &event.delivery_scope
                || scope != &current_scope.key
            {
                log(&summon_i18n::format!("rfd444b0ab6", event.agent, event.id));
                continue;
            }
            match wal.load_recovery_batch(batch_id) {
                Ok(Some(batch)) => match recovery_card_event(&batch).and_then(|replacement| {
                    store.upgrade_legacy_recovery_wire(&event, &replacement)
                }) {
                    Ok(()) => log(&summon_i18n::format!("r12628899d5", event.agent, event.id)),
                    Err(error) => log(&summon_i18n::format!("r2ecb7cb425", event.agent, event.id)),
                },
                Ok(None) => match wal.recovery_outbox_completed(batch_id, &event.id) {
                    Ok(true) => {
                        if let Err(error) = store.archive_recovery_by_id(&event.id) {
                            log(&summon_i18n::format!("re9e2492243", event.agent, event.id));
                        }
                    }
                    Ok(false) => log(&summon_i18n::format!("r20fcea33b5", event.agent, event.id)),
                    Err(error) => log(&summon_i18n::format!("r535f27fa94", event.agent, event.id)),
                },
                Err(error) => log(&summon_i18n::format!("rc53289b327", event.agent, event.id)),
            }
            continue;
        }

        let mut records = Vec::with_capacity(items.len());
        let mut valid = true;
        for item in items {
            match wal.read_consumed(&event.agent, &item.message_id) {
                Ok(Some(record))
                    if record.inbound.chat_id == *chat_id
                        && record.delivery_scope == current_scope.key
                        && record.scheduled.is_none() =>
                {
                    records.push(record)
                }
                Ok(_) | Err(_) => {
                    valid = false;
                    break;
                }
            }
        }
        if !valid || records.is_empty() {
            match store.archive_migrated_legacy_recovery(&event) {
                Ok(()) => log(&summon_i18n::format!("rd1963df902", event.agent, event.id)),
                Err(error) => log(&summon_i18n::format!("r840e20d773", event.agent, event.id)),
            }
            continue;
        }
        let mut batches = Vec::new();
        for (index, chunk) in partition_recovery_records(&records).into_iter().enumerate() {
            match wal.create_recovery_batch(
                &current_scope.key,
                &event.agent,
                chat_id,
                chat_type,
                (index == 0).then(|| stage.clone()).flatten(),
                chunk,
                event.created_ms.saturating_add(index as u64),
            ) {
                Ok(batch) => batches.push(batch),
                Err(error) => {
                    log(&summon_i18n::format!("r9b4c926214", event.agent));
                    batches.clear();
                    break;
                }
            }
        }
        if batches.is_empty()
            || batches
                .iter()
                .any(|batch| !matches!(ensure_recovery_card_outbox(store, batch), Ok(true)))
        {
            continue;
        }
        if let Err(error) = store.archive_migrated_legacy_recovery(&event) {
            log(&summon_i18n::format!("r4c58a9a6fc", event.agent, event.id));
        }
    }
}

fn reconcile_stale_bridge_identities(reg: &Registry, store: &crate::bridge::store::BridgeStore) {
    use summon_core::bridge::{BridgeCloseReason, BridgeMode, ChatSlot};
    match store.load_state() {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(error) => {
            log(&summon_i18n::format!("r9130d3c098"));
            return;
        }
    }
    let result = store.with_state(|state| {
        let mut rollback = Vec::new();
        let mut preserved_sensitive = 0usize;
        let mut preserved_unknown = 0usize;
        for slot in state.slots.values() {
            let target = match slot {
                ChatSlot::Pending { ticket_id, .. } => state
                    .tickets
                    .get(ticket_id)
                    .map(|ticket| ticket.target.clone()),
                ChatSlot::Activating(binding)
                | ChatSlot::MirrorBound(binding)
                | ChatSlot::HandoffPending(binding)
                | ChatSlot::ExternalOwned(binding) => Some(binding.target.clone()),
                ChatSlot::Closing { binding, .. } => Some(binding.target.clone()),
            };
            let Some(target) = target else {
                continue;
            };
            match persisted_scope_relation(reg, &target.agent, &target.scope) {
                PersistedScopeRelation::ExactRoutable => continue,
                PersistedScopeRelation::ExactUnavailable | PersistedScopeRelation::Unknown => {
                    preserved_unknown += 1;
                    continue;
                }
                PersistedScopeRelation::KnownDifferent => {}
            }
            match slot {
                ChatSlot::Pending { .. } => rollback.push(target),
                ChatSlot::Activating(binding) | ChatSlot::MirrorBound(binding)
                    if binding.mode == BridgeMode::Mirror =>
                {
                    rollback.push(target)
                }
                ChatSlot::Activating(_)
                | ChatSlot::MirrorBound(_)
                | ChatSlot::HandoffPending(_)
                | ChatSlot::ExternalOwned(_)
                | ChatSlot::Closing { .. } => preserved_sensitive += 1,
            }
        }
        rollback.sort();
        rollback.dedup();
        let mut released = 0usize;
        for target in rollback {
            match state.chat_gone(&target, BridgeCloseReason::BotRemoved) {
                Ok(
                    summon_core::bridge::BridgeChatGoneAction::PendingCancelled { .. }
                    | summon_core::bridge::BridgeChatGoneAction::MirrorStopped(_),
                ) => released += 1,
                Ok(_) => {}
                Err(error) => return Err(anyhow::Error::new(error)),
            }
        }
        Ok((released, preserved_sensitive, preserved_unknown))
    });
    match result {
        Ok((released, preserved, unknown)) if released > 0 || preserved > 0 || unknown > 0 => {
            log(&summon_i18n::format!("r559021a83d"))
        }
        Ok(_) => {}
        Err(error) => log(&summon_i18n::format!("r86d1386940")),
    }
}

/// 起 outbox 消费者 drain 线程(detached,fire-and-forget)。启动先清一次过期归档,然后每
/// `TICK_MS` 一 tick 投递到期事件,直到 `shutdown` 置位退出。整段错误只 log,绝不 panic/阻塞 daemon。
/// 启动时入站 WAL 收尾。**每次启动都跑**,按**消息粒度**(`classify_inbox_recovery`)决定每条:
/// - **崩溃**(`was_crash=true`):全部聚恢复卡片入队 outbox + 清 pending.json(防 resume 双跑)。
/// - **优雅停机**(`was_crash=false`)按每条 started:
///   - **未开跑**(排队中被停机):`create_time_ms` 归零后**重注入 dispatcher**,留 pending 由 chat
///     线程处理完再销账(修复「优雅重启后排队消息丢失」,且不吃时效/水位/去重预热闸)。
///   - **已开跑**:无头且有精确、已授权的 pending.json → resume_pending_turn 续跑，Inbox 留到终态
///     再销账；Transcript 且盘上有一枚全部判据都证得的接管锚点 → 以 `reattach=true` 重注入，
///     由新一代 chat 线程**零注入**接住那一轮(见 `dispatcher::reattach_anchor_on_disk`)；否则
///     走恢复卡由用户确认,不静默自动重放(避免存活 tmux 里原轮已跑完再被喂一遍双执行)。
///
/// 「恢复」按钮经 `InboxWal::read()` 回退 consumed/ 仍能取回原 Inbound 重注入。
///
/// `clean_record` 是上一代的优雅停机记录:接管的新鲜度判据要它的 `stopped_at_secs`。
/// 没有(硬崩 / 旧格式)则按 0 —— 锚点自动判过期,谁都不接管。
pub(super) fn boot_recover_inbox(
    reg: &Registry,
    store: &crate::runtime::outbox_store::OutboxStore,
    was_crash: bool,
    core: &Arc<crate::runtime::core::Core>,
    bridge_store: &crate::bridge::store::BridgeStore,
    clean_record: Option<&super::CleanShutdownRecordV2>,
) -> anyhow::Result<()> {
    let wal = crate::runtime::inbox_wal::InboxWal::new(run_dir().join("inbox"));
    reconcile_stale_bridge_identities(reg, bridge_store);
    migrate_legacy_recovery_cards(reg, store, &wal);
    // claim_recovery 故意先落 replay pending、后推进 batch；若恰在两次 fsync 间崩溃，必须
    // 在普通 inbox 扫描前把 Awaiting 原子修成 Claimed。identity 冲突时整轮 fail-closed，
    // 绝不能把一个未经 RecoveryBatch 授权的 replay 送进 dispatcher。
    let repaired = wal
        .repair_pending_recovery_claims()
        .context(summon_i18n::text!("r0c6a43bee8"))?;
    if repaired > 0 {
        log(&summon_i18n::format!("r77fc4a6c08"));
    }
    // 先收口此前 crash 窗留下的 prepared batch，再扫 source pending；否则半迁移批次的残余
    // 记录会生成第二张随机恢复卡。
    reconcile_recovery_batches(reg, store, &wal, now_unix_ms())?;
    let records = wal
        .scan_pending_strict()
        .context(summon_i18n::text!("r7eb4cf0fcc"))?;
    if records.is_empty() {
        return Ok(());
    }
    // 按 (agent, chat_id) 聚合(chat_type 取该组首条),保留完整记录(带 started)。
    let mut groups: HashMap<
        (String, String, String),
        (String, Vec<crate::runtime::inbox_wal::InboxRecord>),
    > = HashMap::new();
    for rec in records {
        let chat_type = rec
            .inbound
            .chat_type
            .clone()
            .unwrap_or_else(|| "p2p".to_string());
        let key = (
            rec.delivery_scope.clone(),
            rec.agent.clone(),
            rec.inbound.chat_id.clone(),
        );
        groups
            .entry(key)
            .or_insert_with(|| (chat_type, Vec::new()))
            .1
            .push(rec);
    }
    for ((delivery_scope, agent_name, chat_id), (chat_type, recs)) in groups {
        match persisted_scope_relation(reg, &agent_name, &delivery_scope) {
            PersistedScopeRelation::KnownDifferent => {
                match wal.abandon_stale_pending(&recs) {
                    Ok(count) => log(&summon_i18n::format!("r84683f8695")),
                    Err(error) => log(&summon_i18n::format!("r29e8526778")),
                }
                continue;
            }
            PersistedScopeRelation::ExactUnavailable | PersistedScopeRelation::Unknown => {
                log(&summon_i18n::format!("r9c83204e67"));
                continue;
            }
            PersistedScopeRelation::ExactRoutable => {}
        }
        let recs = settle_terminal_inbox(&wal, recs)?;
        if recs.is_empty() {
            continue;
        }
        let Some(agent) = reg.get(&agent_name) else {
            log(&summon_i18n::format!("r932f9d1e99"));
            continue;
        };
        let tag = chat_tag(&chat_type, &chat_id);
        let agent_dir = Some(agent.state_dir().clone());
        // 三条无头路都算:CliSession/Rpc 的 pending/恢复语义与 Subprocess 完全一致。
        let is_headless = agent.capture_mode().is_headless();
        let pending = agent_dir
            .as_ref()
            .and_then(|d| crate::runtime::inflight::read_pending(d, &tag));
        let has_pending_json = pending.is_some();
        let has_exact_resume = match pending.as_ref() {
            Some(checkpoint)
                if checkpoint.resumable_in_context(
                    &delivery_scope,
                    &agent_name,
                    &chat_id,
                    &chat_type,
                ) && checkpoint.cli == agent.cli_kind() =>
            {
                wal.resume_sources_match(
                    &agent_name,
                    &delivery_scope,
                    &chat_id,
                    &chat_type,
                    &checkpoint.inbox_message_ids,
                )
                .with_context(|| summon_i18n::format!("r130abd554d"))?
            }
            Some(_) | None => false,
        };
        let discard_invalid_pending = has_pending_json && !has_exact_resume;
        // scheduled started 记录必须按 fire_id 自动重放；若同时保留无头 pending.json，dispatcher
        // 启动时会先 resume 旧 prompt、随后再消费 ScheduledFire，形成确定性双跑。清掉 pending，
        // 并把同组异常残留的真人 started 记录降级到恢复卡，而不是误认为已有 resume 兜底。
        let scheduled_started = recs
            .iter()
            .any(|rec| rec.started && rec.scheduled.is_some());

        // 先只读 pending.json 的阶段。删除必须等 durable RecoveryBatch 已接管完整 Inbound；否则
        // manifest/outbox 任一步失败都会同时丢掉部分进度和恢复依据。
        let mut stage: Option<String> = None;
        if was_crash || scheduled_started || discard_invalid_pending {
            if let Some(dir) = &agent_dir {
                if let Some(p) = crate::runtime::inflight::read_pending(dir, &tag) {
                    let part = p.partial.trim();
                    if !part.is_empty() {
                        stage = Some(summon_core::quote::quote_preview(part));
                    }
                }
            }
        }

        // 接管锚点只在优雅 + Transcript 这一格上问盘:硬崩路径一次多余的 IO 都不做,行为逐字节
        // 不变。停机时刻取 clean marker 上那个 **daemon 级**的时刻(与 `plan_mirror_catchup` 同源),
        // 拿不到按 0 → 锚点判过期 → 不接管:mirror fence 只有提交过 offset 的 chat 才签得出,
        // 拿 fence 的时刻当停机时刻会让首轮就被重启的会话永远判过期。
        let has_transcript_anchor = !was_crash
            && !is_headless
            && crate::runtime::daemon::dispatcher::reattach_anchor_on_disk(
                agent,
                &delivery_scope,
                &wal,
                &tag,
                reattach_stopped_at_secs(clean_record),
                unix_now_secs(),
            )
            .is_some();

        let disp = classify_inbox_recovery(
            recs,
            was_crash,
            is_headless,
            has_exact_resume && !scheduled_started,
            has_transcript_anchor,
        );

        if disp.holds_interrupted_recovery && has_pending_json {
            if let Some(dir) = &agent_dir {
                // 该 replay 的上次执行结果未知，repair 已把原 batch 退回 Awaiting。无头 checkpoint
                // 若继续留着，dispatcher 会绕过用户确认自动 resume；先清掉，只保留 batch+replay
                // 证据，等待原恢复卡再次授权。
                crate::runtime::inflight::clear_pending_checked(dir, &tag)
                    .with_context(|| summon_i18n::format!("r18aa23cb42"))?;
                log(&summon_i18n::format!("r0e21350351"));
            }
        }

        // 1) 恢复卡：无法自动重放的那些记录转人工恢复（判据与接管失败共用，见 `emit_recovery_card`）。
        let has_recovery_records = !disp.recovery_records.is_empty();
        let mut recovery_batch_durable = false;
        if has_recovery_records {
            // 冷启只关心 batch 有没有落成:卡进没进 outbox 由 outbox 自己重试,清 pending 的
            // 授权来自 durable batch。返回值是给接管路径判「要不要撤 OnIt」的。
            let _ = emit_recovery_card(
                &wal,
                store,
                &delivery_scope,
                &agent_name,
                &chat_id,
                &chat_type,
                stage,
                disp.recovery_records,
            )?;
            recovery_batch_durable = true;
        }

        // batch 已完整接管，或本组只有可自动重放的 scheduled，才清无头 pending.json，防它与
        // 后面的 WAL 重注入形成双跑。创建 batch 失败时上面已 continue，绝不先清后丢。
        if (recovery_batch_durable || !has_recovery_records)
            && (was_crash || scheduled_started || discard_invalid_pending)
        {
            if let Some(dir) = &agent_dir {
                crate::runtime::inflight::clear_pending_checked(dir, &tag)
                    .with_context(|| summon_i18n::format!("rcb42f87fc4"))?;
            }
        }

        // 2) 重注入。**不销账**——留 pending 由 chat 线程处理完再销(投递成功才 ack;避开去重
        //    预热竞态)。投递次序见 `injection_order`。
        for inbound in injection_order(disp.reattach, disp.reinject) {
            let mid = inbound.message_id.clone();
            let what = if inbound.reattach {
                summon_i18n::text!("r5a15d412b1")
            } else {
                summon_i18n::text!("r3e6a58c61e")
            };
            let ok = core.dispatch_scheduled(
                &agent_name,
                crate::runtime::daemon::DispatchMsg::Inbound(inbound),
            );
            if ok {
                log(&summon_i18n::format!("r1e081442ff"));
            } else {
                log(&summon_i18n::format!("r6d3794948d"));
            }
        }

        // 已开跑没跑完的定时 fire:销账退票,不补跑(判据与理由见 `classify_inbox_recovery`)。
        // 销账是**必须**的 —— 不销就永远留在 pending,每次启动重注入一次、每次被 WAL 弹回,
        // 空转到天荒地老。
        for scheduled in disp.scheduled_retire {
            wal.mark_consumed(&agent_name, &scheduled.fire_id);
            log(&summon_i18n::format!("rfaa2707eb2", scheduled.fire_id));
        }

        // 未开跑的 scheduled 自动重注入；记录继续留 pending，只有 chat 线程完成该 turn 才销账。
        // 路由失败也保留，待下次启动恢复，绝不谎称已消费。
        for scheduled in disp.scheduled_reinject {
            let fire_id = scheduled.fire_id.clone();
            let ok = core.dispatch_scheduled(
                &agent_name,
                crate::runtime::daemon::DispatchMsg::ScheduledFire {
                    chat_id: scheduled.chat_id,
                    chat_type: scheduled.chat_type,
                    prompt: scheduled.prompt,
                    fire_id: fire_id.clone(),
                    output: scheduled.output,
                    context: scheduled.context,
                },
            );
            if ok {
                log(&summon_i18n::format!("r6e6000eea7"));
            } else {
                log(&summon_i18n::format!("rb373de40dc"));
            }
        }

        // sentinel 与 scheduled 同语义:fire_id 幂等,自动重注入;路由失败保留 pending 待下次启动。
        for sentinel in disp.sentinel_reinject {
            let fire_id = sentinel.fire_id.clone();
            let ok = core.dispatch_scheduled(&agent_name, sentinel.fire());
            if ok {
                log(&summon_i18n::format!("re52b73c873"));
            } else {
                log(&summon_i18n::format!("rcb9521bec1"));
            }
        }
    }
    Ok(())
}

/// per-agent 投递句柄表。运行期新增 agent(onboard 免重启上线)要往里插,故共享可变 ——
/// 表里没有该 agent 时 `deliver_outbox_event` 会一路重试到死信,静默丢掉它的退出回执。
pub(crate) type DrainAgents = Arc<std::sync::RwLock<HashMap<String, OutboxDrainAgent>>>;

/// 在读锁里拷一份投递表就放锁,再在锁外跑 `deliver`:联网投递至多 30s,而新 agent 上线、agent.yaml
/// 改了之后的原地刷新都要写这张表,不能被一次投递挡住。表很小(一个 agent 一项)。锁中毒 → Err
/// 走既有重试路径。
fn with_drain_snapshot<R>(
    agents: &DrainAgents,
    deliver: impl FnOnce(&HashMap<String, OutboxDrainAgent>) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    let snapshot = agents
        .read()
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r7d1ffd6562")))?
        .clone();
    deliver(&snapshot)
}

pub(super) fn spawn_outbox_drain(
    drain_agents: DrainAgents,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    use summon_core::outbox::OutboxEvent as Ev;
    thread::spawn(move || {
        let store = crate::runtime::outbox_store::OutboxStore::at_run_dir();
        let bridge_store = crate::bridge::store::BridgeStore::runtime_default();
        // 启动清一次 7 天前归档(best-effort);dead/ 不清。
        store.cleanup_archive(now_unix_ms());
        let deliver = move |ev: &Ev| {
            with_drain_snapshot(&drain_agents, |agents| deliver_outbox_event(agents, ev))
        };
        let ack_store = bridge_store.clone();
        let on_acked = move |ev: &Ev| bridge_outbox_acked_with_store(ev, &ack_store);
        let on_dead = move |ev: &Ev| bridge_outbox_dead_with_store(ev, &bridge_store);
        loop {
            if shutdown.load(Ordering::SeqCst) {
                break; // 停机:drain 也退(随 daemon 死,不持久化线程)
            }
            let writer = crate::runtime::agent_retirement::OUTBOX_WRITERS
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let n = store.drain_tick_with_hooks(now_unix_ms(), &deliver, &on_acked, &on_dead);
            drop(writer);
            if n > 0 {
                log(&summon_i18n::format!("r55f059b226"));
            }
            // 空转 TICK_MS;停机 flag 在下一轮顶部看到即退(粒度可接受)。
            thread::sleep(Duration::from_millis(summon_core::outbox::TICK_MS));
        }
    });
}

pub(super) fn bridge_outbox_acked_with_store(
    event: &summon_core::outbox::OutboxEvent,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    use summon_core::outbox::OutboxKind;
    match &event.kind {
        OutboxKind::ExternalBridgeTurn { fence, .. } => store.with_state(|state| {
            state
                .finish_mirror_export(&fence.receipt_id, &event.id)
                .map(|_| ())
                .map_err(anyhow::Error::new)
        }),
        OutboxKind::BridgeContextCapsule { fence, .. } => store.with_state(|state| {
            state
                .finish_context_export(&fence.receipt_id, &event.id)
                .map(|_| ())
                .map_err(anyhow::Error::new)
        }),
        OutboxKind::AgentDisabled { .. }
        | OutboxKind::NotifyChat { .. }
        | OutboxKind::ResumeTurn { .. }
        | OutboxKind::RecoveryCard { .. }
        | OutboxKind::ProcessSupplement { .. }
        | OutboxKind::TurnReply { .. }
        | OutboxKind::BridgeActivationAudit { .. }
        | OutboxKind::BridgeHandoffStatus { .. } => Ok(()),
    }
}

fn bridge_outbox_dead_with_store(
    event: &summon_core::outbox::OutboxEvent,
    store: &crate::bridge::store::BridgeStore,
) -> anyhow::Result<()> {
    use summon_core::bridge::{BridgeError, TicketStatus};
    use summon_core::outbox::OutboxKind;
    match &event.kind {
        OutboxKind::BridgeActivationAudit {
            fence, ticket_id, ..
        } => {
            let target =
                summon_core::bridge::ScopedChatKey::new(&fence.scope, &fence.agent, &fence.chat_id);
            crate::bridge::runtime::with_chat_delivery_gate(&target, || {
                store.with_state(|state| {
                    let status = state
                        .tickets
                        .get(ticket_id)
                        .map(|ticket| ticket.status.clone());
                    match status {
                        Some(TicketStatus::Activating { binding_id, .. })
                            if binding_id == fence.binding_id =>
                        {
                            state.cancel_ticket(ticket_id).map_err(anyhow::Error::new)?;
                        }
                        Some(TicketStatus::Committed {
                            binding_id,
                            mode: summon_core::bridge::BridgeMode::Mirror,
                            ..
                        }) if binding_id == fence.binding_id => {
                            let target = summon_core::bridge::ScopedChatKey::new(
                                &fence.scope,
                                &fence.agent,
                                &fence.chat_id,
                            );
                            state
                                .stop_mirror(&target, &fence.binding_id, fence.binding_revision)
                                .map_err(anyhow::Error::new)?;
                        }
                        _ => {}
                    }
                    Ok(())
                })
            })
        }
        OutboxKind::ExternalBridgeTurn { fence, turn_id, .. } => store.with_state(|state| {
            state
                .finish_mirror_export(&fence.receipt_id, &event.id)
                .map_err(anyhow::Error::new)?;
            match state.discard_pending_mirror_turn(
                turn_id,
                &fence.binding_id,
                fence.binding_revision,
            ) {
                Ok(_) | Err(BridgeError::MirrorTurnMissing) => Ok(()),
                Err(error) => Err(anyhow::Error::new(error)),
            }
        }),
        OutboxKind::BridgeContextCapsule { fence, .. } => store.with_state(|state| {
            state
                .finish_context_export(&fence.receipt_id, &event.id)
                .map(|_| ())
                .map_err(anyhow::Error::new)
        }),
        OutboxKind::AgentDisabled { .. }
        | OutboxKind::NotifyChat { .. }
        | OutboxKind::ProcessSupplement { .. }
        | OutboxKind::TurnReply { .. } => {
            // 用户可见消息重试耗尽进 dead/ = 永久丢失,且没有任何主动通知。不能静默:
            // 留一条日志让「用户没收到回复」的排查有迹可循(doctor 另计 dead 计数)。
            // 只打 kind 名 + event id,绝不打正文(NotifyChat/TurnReply 都带 text)。
            let kind = match &event.kind {
                OutboxKind::AgentDisabled { .. } => "AgentDisabled",
                OutboxKind::NotifyChat { .. } => "NotifyChat",
                OutboxKind::TurnReply { .. } => "TurnReply",
                OutboxKind::ProcessSupplement { .. } => "ProcessSupplement",
                _ => unreachable!(),
            };
            log(&summon_i18n::format!("rc984852d30", event.id));
            Ok(())
        }
        OutboxKind::ResumeTurn { .. }
        | OutboxKind::RecoveryCard { .. }
        | OutboxKind::BridgeHandoffStatus { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::inbox_wal::InboxRecord;
    use summon_core::outbox::OutboxKind;

    #[test]
    fn boot_recovery_uses_strict_scan_after_reconcile() {
        let source = include_str!("outbox_drain.rs");
        let start = source.find("pub(super) fn boot_recover_inbox(").unwrap();
        let end = source[start..]
            .find("\npub(super) fn spawn_outbox_drain(")
            .map(|offset| start + offset)
            .unwrap();
        let boot = &source[start..end];
        let reconcile = boot
            .find("reconcile_recovery_batches(reg, store, &wal, now_unix_ms())?;")
            .unwrap();
        let strict = boot.find(".scan_pending_strict()").unwrap();
        assert!(
            reconcile < strict,
            "必须在所有启动 mutator 之后重新严格扫描"
        );
        assert!(
            !boot.contains("wal.scan_pending()"),
            "生产恢复路径不得把损坏 WAL 静默解释为空队列"
        );
    }

    #[test]
    fn final_activation_cas_does_not_ack_when_cancel_persistence_fails() {
        let error = anyhow::Error::new(summon_core::bridge::BridgeError::StaleSession);
        let result = handle_activation_completion_error(error, || {
            anyhow::bail!("injected cancel write failure")
        });
        let error = result.expect_err("取消未持久化时必须保留 outbox 重试");
        assert!(format!("{error:#}").contains("injected cancel write failure"));
    }

    #[test]
    fn handoff_status_only_delivers_for_current_exact_attempt() {
        use summon_core::bridge::{
            BridgeMode, ChatBinding, ChatSlot, ExternalLocator, ResumeProfile, ScopedChatKey,
            SessionKey, SessionLease,
        };
        use summon_core::outbox::{
            bridge_delivery_id, BridgeDeliveryFence, BridgeDeliveryKind, BridgeHandoffNotice,
            OutboxEvent,
        };

        let (agent, _agent_dir) =
            crate::test_support::agent_dir("claude", "feishu:\n  app_id_env: TEST_APP\n");
        std::fs::write(agent.dir.join(".env"), "TEST_APP=app-takeover-status\n").unwrap();
        let scope = FeishuEpochIdentity::capture(&agent)
            .storage_scope()
            .key
            .clone();
        let target = ScopedChatKey::new(&scope, &agent.name, "oc_takeover_status");
        let session = SessionKey::new("/home/test/.claude", "sid-takeover");
        let process = crate::bridge::process_probe::observe_exact_fingerprint(std::process::id())
            .unwrap()
            .expect("测试进程应可探测");
        let profile = ResumeProfile {
            claude_bin: process.executable.clone(),
            home: "/home/test".into(),
            config_dir: session.config_dir.clone(),
            version: "test-version".into(),
            process_verified: true,
            environment_verified: true,
            config_source: summon_core::bridge::ClaudeConfigSource::Default,
            provider: summon_core::bridge::ClaudeProvider::Default,
            ..ResumeProfile::default()
        };
        let locator = ExternalLocator {
            key: session.clone(),
            transcript_path: "/home/test/.claude/projects/demo/sid-takeover.jsonl".into(),
            resume_lookup_cwd: "/workspace/demo".into(),
            current_cwd: "/workspace/demo".into(),
            profile: profile.clone(),
            revision: 1,
        };
        let source = crate::bridge::handoff::HandoffSourceSnapshot::new(
            session.clone(),
            3,
            locator.clone(),
            process.clone(),
            profile,
            crate::bridge::handoff::locator_identity_revision(&locator, &process),
        )
        .unwrap();
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let binding = ChatBinding {
            id: "binding-takeover-status".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: session.clone(),
            mode: BridgeMode::Handoff,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            revision: 7,
        };
        let attempt = crate::bridge::handoff::HandoffAttempt::new(
            attempt_id.clone(),
            "ticket-takeover-status".into(),
            binding.id.clone(),
            binding.revision,
            target.clone(),
            "group".into(),
            "soul-claude-takeover".into(),
            summon_core::bridge::ClaudeProvider::Default,
            source,
            1,
        )
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(temp.path().join("bridge"));
        store.persist_handoff_attempt(&attempt).unwrap();
        store
            .with_state(|state| {
                state.slots.insert(
                    target.storage_key(),
                    ChatSlot::HandoffPending(binding.clone()),
                );
                state.session_leases.insert(
                    session.storage_key(),
                    SessionLease {
                        binding_id: binding.id.clone(),
                        target: target.clone(),
                    },
                );
                Ok(())
            })
            .unwrap();

        let event = |notice, slug: &str| {
            let receipt_id = format!("{attempt_id}-{slug}");
            OutboxEvent {
                id: bridge_delivery_id(BridgeDeliveryKind::HandoffStatus, &receipt_id),
                agent: agent.name.clone(),
                delivery_scope: scope.clone(),
                created_ms: 2,
                attempts: 0,
                next_attempt_ms: 2,
                last_error: None,
                kind: OutboxKind::BridgeHandoffStatus {
                    fence: BridgeDeliveryFence {
                        scope: scope.clone(),
                        agent: agent.name.clone(),
                        chat_id: target.chat_id.clone(),
                        chat_type: "group".into(),
                        binding_id: binding.id.clone(),
                        binding_revision: binding.revision,
                        session: session.clone(),
                        generation: 3,
                        lease_binding_id: binding.id.clone(),
                        receipt_id,
                        sequence: 1,
                    },
                    attempt_id: attempt_id.clone(),
                    notice,
                    card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                    rendered_card: summon_core::feishu::card_markdown_v2("takeover status"),
                },
            }
        };
        let outbound_path = agent
            .state_dir()
            .join("threads")
            .join(summon_core::registry::chat_tag("group", &target.chat_id))
            .join("outbound.ndjson");
        let client = FeishuClient::new("feishu.cn", String::new(), String::new(), true);

        let mut wrong_scope = event(BridgeHandoffNotice::Requested, "requested");
        wrong_scope.delivery_scope = "other-app-scope".into();
        assert!(
            deliver_bridge_handoff_status_with_store(&agent, &client, &wrong_scope, &store,)
                .is_err()
        );
        assert!(!outbound_path.exists(), "错 app scope 不得投递状态卡");

        deliver_bridge_handoff_status_with_store(
            &agent,
            &client,
            &event(BridgeHandoffNotice::TerminatingSource, "terminating"),
            &store,
        )
        .unwrap();
        assert!(!outbound_path.exists(), "过期 phase 不得投递状态卡");

        deliver_bridge_handoff_status_with_store(
            &agent,
            &client,
            &event(BridgeHandoffNotice::Requested, "requested"),
            &store,
        )
        .unwrap();
        let outbound = std::fs::read_to_string(outbound_path).unwrap();
        assert_eq!(outbound.lines().count(), 1);
        assert!(outbound.contains("bridge_handoff_status"));
    }

    #[test]
    fn bridge_delivery_reloads_operator_authority_fail_closed() {
        let (agent, _dir) = crate::test_support::agent_dir(
            "claude",
            "feishu:\n  app_id_env: TEST_APP\n  operators: [ou_owner]\n",
        );
        std::fs::write(agent.dir.join(".env"), "TEST_APP=app-old\n").unwrap();
        let epoch = FeishuEpochIdentity::capture(&agent);
        assert!(
            current_bridge_operator_authorized_at_epoch(&agent, &epoch, "ou_owner", "").unwrap()
        );
        std::fs::write(
            agent.manifest_path(),
            "name: test\nruntime:\n  cli: claude\nfeishu:\n  app_id_env: TEST_APP\n  operators: [ou_other]\n",
        )
        .unwrap();
        assert!(
            !current_bridge_operator_authorized_at_epoch(&agent, &epoch, "ou_owner", "").unwrap()
        );
        std::fs::write(
            agent.manifest_path(),
            "name: test\nruntime:\n  cli: claude\nfeishu:\n  app_id_env: TEST_APP\n  operators: [ou_owner]\n",
        )
        .unwrap();
        std::fs::write(agent.dir.join(".env"), "TEST_APP=app-new\n").unwrap();
        assert!(
            current_bridge_operator_authorized_at_epoch(&agent, &epoch, "ou_owner", "").is_err()
        );
        let receipt_id = "env-drift";
        let event = summon_core::outbox::OutboxEvent {
            id: summon_core::outbox::bridge_delivery_id(
                summon_core::outbox::BridgeDeliveryKind::ExternalTurn,
                receipt_id,
            ),
            agent: agent.name.clone(),
            delivery_scope: epoch.storage_scope().key.clone(),
            created_ms: 1,
            attempts: 0,
            next_attempt_ms: 1,
            last_error: None,
            kind: summon_core::outbox::OutboxKind::ExternalBridgeTurn {
                fence: summon_core::outbox::BridgeDeliveryFence {
                    scope: epoch.storage_scope().key.clone(),
                    agent: agent.name.clone(),
                    chat_id: "oc_env_drift".into(),
                    chat_type: "group".into(),
                    binding_id: "binding-env".into(),
                    binding_revision: 1,
                    session: summon_core::bridge::SessionKey::new("/config", "sid-env"),
                    generation: 1,
                    lease_binding_id: "binding-env".into(),
                    receipt_id: receipt_id.into(),
                    sequence: 2,
                },
                turn_id: "turn-env".into(),
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card: summon_core::feishu::card_external_bridge_turn("user", "answer"),
            },
        };
        let temp = tempfile::tempdir().unwrap();
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        outbox.enqueue(&event).unwrap();
        let drain_agents = HashMap::from([(
            agent.name.clone(),
            OutboxDrainAgent {
                agent: agent.clone(),
                client: Arc::new(FeishuClient::with_test_base("http://127.0.0.1:1".into())),
                epoch: epoch.clone(),
            },
        )]);
        let acked = std::sync::atomic::AtomicBool::new(false);
        outbox.drain_tick_with_hooks(
            2,
            &|event| deliver_outbox_event(&drain_agents, event),
            &|_| {
                acked.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            &|_| Ok(()),
        );
        assert!(!acked.load(std::sync::atomic::Ordering::SeqCst));
        assert!(matches!(
            outbox.find(&event.id).unwrap(),
            Some((crate::runtime::outbox_store::OutboxLocation::Pending, _))
        ));
        std::fs::write(agent.dir.join(".env"), "TEST_APP=app-old\n").unwrap();
        std::fs::write(
            agent.manifest_path(),
            "name: test\nstatus: inactive\nruntime:\n  cli: claude\nfeishu:\n  app_id_env: TEST_APP\n  operators: [ou_owner]\n",
        )
        .unwrap();
        assert!(
            current_bridge_operator_authorized_at_epoch(&agent, &epoch, "ou_owner", "").is_err()
        );
        std::fs::write(agent.manifest_path(), "not: [valid").unwrap();
        assert!(
            current_bridge_operator_authorized_at_epoch(&agent, &epoch, "ou_owner", "").is_err()
        );
        std::fs::write(
            agent.manifest_path(),
            "name: test\nruntime:\n  cli: claude\nfeishu:\n  app_id_env: ROTATED_APP\n  app_secret_env: ROTATED_SECRET\n  operators: [ou_owner]\n",
        )
        .unwrap();
        assert!(
            current_bridge_operator_authorized_at_epoch(&agent, &epoch, "ou_owner", "").is_err()
        );
        std::fs::write(
            agent.manifest_path(),
            "name: renamed\nruntime:\n  cli: claude\nfeishu:\n  app_id_env: TEST_APP\n  operators: [ou_owner]\n",
        )
        .unwrap();
        assert!(
            current_bridge_operator_authorized_at_epoch(&agent, &epoch, "ou_owner", "").is_err()
        );

        let production = include_str!("outbox_drain.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        assert_eq!(
            production
                .matches("current_bridge_operator_authorized_at_epoch(")
                .count(),
            4,
            "定义 + activation/turn/context 三条网络路径必须都热重载撤权"
        );
    }

    struct SplitReplyFrontend {
        sent: std::cell::RefCell<Vec<String>>,
    }

    impl Frontend for SplitReplyFrontend {
        fn send_message(&self, _chat: &ChatRef, body: &str) -> anyhow::Result<()> {
            self.sent.borrow_mut().push(body.to_string());
            Ok(())
        }

        fn split_final_reply(&self, _chat: &ChatRef, body: &str) -> Vec<String> {
            assert_eq!(body, "leftright");
            vec!["left".into(), "right".into()]
        }

        fn stream_begin(
            &self,
            _chat: &ChatRef,
            _content: &str,
            _with_interrupt: bool,
        ) -> anyhow::Result<StreamId> {
            Ok(StreamId(1))
        }

        fn stream_patch(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn react(&self, _msg: &MsgRef, _key: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }

        fn unreact(&self, _msg: &MsgRef, _reaction_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn bridge_delivery_gate_revalidates_scope_revision_generation_and_lease() {
        use summon_core::bridge::{
            BridgeMode, ChatBinding, ChatSlot, ExternalLifecycle, ExternalLocator,
            ExternalSessionRecord, ProcessFingerprint, ResumeProfile, ScopedChatKey, SessionKey,
            SessionLease,
        };
        use summon_core::outbox::{BridgeDeliveryFence, OutboxEvent, OutboxKind};
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let epoch = FeishuEpochIdentity::capture(&agent);
        let scope = epoch.storage_scope().key.clone();
        let target = ScopedChatKey::new(&scope, &agent.name, "chat");
        let session = SessionKey::new("/config", "sid");
        let binding = ChatBinding {
            id: "binding".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: session.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "owner".into(),
            created_at_ms: 1,
            revision: 7,
        };
        let mut state = summon_core::bridge::BridgeState::default();
        state.upsert_session(ExternalSessionRecord {
            key: session.clone(),
            generation: 3,
            locator: ExternalLocator {
                key: session.clone(),
                transcript_path: "/config/sid.jsonl".into(),
                resume_lookup_cwd: "/project".into(),
                current_cwd: "/project".into(),
                profile: ResumeProfile {
                    claude_bin: "/bin/claude".into(),
                    home: "/home".into(),
                    config_dir: "/config".into(),
                    version: "1".into(),
                    process_verified: true,
                    ..ResumeProfile::default()
                },
                revision: 1,
            },
            process: Some(ProcessFingerprint {
                pid: 42,
                start_time: "start".into(),
                executable: "/bin/claude".into(),
            }),
            lifecycle: ExternalLifecycle::ActiveIdle,
            first_seen_ms: 1,
            last_seen_ms: 2,
            last_applied_sequence: 1,
            pending_prompt: None,
        });
        state
            .slots
            .insert(target.storage_key(), ChatSlot::MirrorBound(binding.clone()));
        state.session_leases.insert(
            session.storage_key(),
            SessionLease {
                binding_id: binding.id.clone(),
                target: target.clone(),
            },
        );
        let fence = BridgeDeliveryFence {
            scope,
            agent: agent.name.clone(),
            chat_id: "chat".into(),
            chat_type: "group".into(),
            binding_id: binding.id.clone(),
            binding_revision: binding.revision,
            session,
            generation: 3,
            lease_binding_id: binding.id.clone(),
            receipt_id: "receipt".into(),
            sequence: 2,
        };
        let event = OutboxEvent {
            id: "event".into(),
            agent: agent.name.clone(),
            delivery_scope: fence.scope.clone(),
            created_ms: 1,
            attempts: 0,
            next_attempt_ms: 1,
            last_error: None,
            kind: OutboxKind::ExternalBridgeTurn {
                fence: fence.clone(),
                turn_id: "turn".into(),
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card: "card".into(),
            },
        };

        assert!(
            exact_bridge_state(&epoch, &event, &fence, &state, mirror_slot, true)
                .unwrap()
                .is_some()
        );
        let mut stale = fence.clone();
        stale.binding_revision += 1;
        assert!(
            exact_bridge_state(&epoch, &event, &stale, &state, mirror_slot, true)
                .unwrap()
                .is_none()
        );
        state
            .session_leases
            .get_mut(&binding.session.storage_key())
            .unwrap()
            .binding_id = "other".into();
        assert!(exact_bridge_state(&epoch, &event, &fence, &state, mirror_slot, true).is_err());
    }

    #[test]
    fn stop_state_cas_waits_until_serialized_body_delivery_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(crate::bridge::store::BridgeStore::new(
            dir.path().join("bridge"),
        ));
        store.with_state(|_| Ok(())).unwrap();
        let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        let delivery_store = std::sync::Arc::clone(&store);
        let delivery_entered = std::sync::Arc::clone(&entered);
        let delivery_release = std::sync::Arc::clone(&release);
        let delivery = std::thread::spawn(move || {
            with_serialized_bridge_delivery(
                &summon_core::outbox::BridgeDeliveryFence {
                    scope: "scope-stop".into(),
                    agent: "agent-stop".into(),
                    chat_id: "chat-stop".into(),
                    chat_type: "group".into(),
                    binding_id: "binding-stop-test".into(),
                    binding_revision: 1,
                    session: summon_core::bridge::SessionKey::new("/config", "sid"),
                    generation: 1,
                    lease_binding_id: "binding-stop-test".into(),
                    receipt_id: "receipt-stop".into(),
                    sequence: 1,
                },
                || {
                    delivery_entered.wait();
                    delivery_release.wait();
                    delivery_store.with_state(|state| {
                        state.revision = state.revision.saturating_add(1);
                        Ok(())
                    })
                },
            )
            .unwrap();
        });
        entered.wait();

        let stop_store = std::sync::Arc::clone(&store);
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || {
            crate::bridge::runtime::with_chat_delivery_gate(
                &summon_core::bridge::ScopedChatKey::new("scope-stop", "agent-stop", "chat-stop"),
                || {
                    stop_store.with_state(|state| {
                        state.revision = state.revision.saturating_add(1);
                        tx.send(()).unwrap();
                        Ok(())
                    })
                },
            )
            .unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "stop 不能在正文 POST 临界区结束前先返回"
        );
        release.wait();
        delivery.join().unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        stop.join().unwrap();
    }

    #[test]
    fn busy_activation_retries_without_posting_audit_or_cancelling_ticket() {
        use summon_core::bridge::{
            BeginChoiceOutcome, BridgeChoice, BridgeMode, ExternalLifecycle, ExternalLocator,
            ExternalSessionRecord, ProcessFingerprint, ResumeProfile, ScopedChatKey, SessionKey,
            TicketRequest, TicketStatus,
        };
        use summon_core::outbox::{
            bridge_delivery_id, BridgeDeliveryFence, BridgeDeliveryKind, OutboxEvent, OutboxKind,
        };

        let (agent, _agent_dir) = crate::test_support::agent_dir(
            "claude",
            "feishu:\n  app_id_env: TEST_APP\n  operators: [ou_owner]\n",
        );
        let scope = feishu_storage_scope(&agent).key;
        let target = ScopedChatKey::new(&scope, &agent.name, "oc_busy");
        let session = SessionKey::new("/config", "sid-busy");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(dir.path().join("bridge"));
        let (binding, sequence) = store
            .with_state(|state| {
                state.upsert_session(ExternalSessionRecord {
                    key: session.clone(),
                    generation: 1,
                    locator: ExternalLocator {
                        key: session.clone(),
                        transcript_path: "/config/sid-busy.jsonl".into(),
                        resume_lookup_cwd: "/project".into(),
                        current_cwd: "/project".into(),
                        profile: ResumeProfile {
                            claude_bin: "/bin/claude".into(),
                            config_dir: "/config".into(),
                            process_verified: true,
                            ..ResumeProfile::default()
                        },
                        revision: 1,
                    },
                    process: Some(ProcessFingerprint {
                        pid: 42,
                        start_time: "start".into(),
                        executable: "/bin/claude".into(),
                    }),
                    lifecycle: ExternalLifecycle::ActiveIdle,
                    first_seen_ms: 1,
                    last_seen_ms: 1,
                    last_applied_sequence: 1,
                    pending_prompt: None,
                });
                state
                    .reserve_ticket(TicketRequest {
                        ticket_id: "ticket-busy".into(),
                        short_code: None,
                        target: target.clone(),
                        target_chat_type: "group".into(),
                        target_provider: summon_core::bridge::ClaudeProvider::Default,
                        requester_open_id: "ou_owner".into(),
                        requester_union_id: String::new(),
                        choices: vec![BridgeChoice {
                            id: "choice-busy".into(),
                            session: session.clone(),
                            registry_revision: 1,
                        }],
                        issued_at_ms: 1,
                        expires_at_ms: 100,
                    })
                    .map_err(anyhow::Error::new)?;
                state
                    .attach_control_card(
                        "ticket-busy",
                        "ou_owner",
                        Some("oc_private"),
                        "om_control",
                    )
                    .map_err(anyhow::Error::new)?;
                let BeginChoiceOutcome::Started(binding) = state
                    .begin_choice(
                        "ticket-busy",
                        "choice-busy",
                        "ou_owner",
                        "om_control",
                        BridgeMode::Mirror,
                        "binding-busy",
                        2,
                    )
                    .map_err(anyhow::Error::new)?
                else {
                    anyhow::bail!("首次 choice 必须 started")
                };
                let sequence = state
                    .allocate_activation_delivery_sequence("ticket-busy", &binding.id)
                    .map_err(anyhow::Error::new)?;
                state
                    .sessions
                    .get_mut(&session.storage_key())
                    .unwrap()
                    .lifecycle = ExternalLifecycle::ActiveBusy;
                Ok((binding, sequence))
            })
            .unwrap();

        let receipt_id = "receipt-busy".to_string();
        let event = OutboxEvent {
            id: bridge_delivery_id(BridgeDeliveryKind::ActivationAudit, &receipt_id),
            agent: agent.name.clone(),
            delivery_scope: scope.clone(),
            created_ms: 2,
            attempts: 0,
            next_attempt_ms: 2,
            last_error: None,
            kind: OutboxKind::BridgeActivationAudit {
                fence: BridgeDeliveryFence {
                    scope,
                    agent: agent.name.clone(),
                    chat_id: target.chat_id.clone(),
                    chat_type: "group".into(),
                    binding_id: binding.id.clone(),
                    binding_revision: binding.revision,
                    session: session.clone(),
                    generation: 1,
                    lease_binding_id: binding.id.clone(),
                    receipt_id,
                    sequence,
                },
                ticket_id: "ticket-busy".into(),
                choice_id: "choice-busy".into(),
                requester_open_id: "ou_owner".into(),
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card: summon_core::feishu::card_bridge_activation_audit(
                    &summon_core::feishu::bridge_target_label(&target.agent, &target.chat_id),
                ),
                control_card: summon_core::feishu::card_bridge_mirror_control(
                    "ticket-busy",
                    "choice-busy",
                    &summon_core::feishu::bridge_target_label(&target.agent, &target.chat_id),
                ),
            },
        };
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let client = crate::frontend::client::FeishuClient::with_test_base(format!(
            "http://{}",
            server.server_addr().to_ip().unwrap()
        ));
        let error =
            deliver_bridge_activation_with_store(&agent, &client, &event, &store).unwrap_err();
        assert!(format!("{error:#}").contains("activation session 正忙"));
        assert!(server
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap()
            .is_none());
        let state = store.load_state().unwrap().unwrap();
        assert!(matches!(
            state.tickets["ticket-busy"].status,
            TicketStatus::Activating { .. }
        ));
        assert!(state.activation_audit_receipts.is_empty());

        let mut short_lived = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let gone_process =
            crate::bridge::process_probe::observe_exact_fingerprint(short_lived.id())
                .unwrap()
                .expect("短命测试进程应可探测");
        short_lived.kill().unwrap();
        short_lived.wait().unwrap();
        store
            .with_state(|state| {
                let record = state.sessions.get_mut(&session.storage_key()).unwrap();
                record.lifecycle = ExternalLifecycle::ActiveIdle;
                record.locator.profile.claude_bin = gone_process.executable.clone();
                record.process = Some(gone_process.clone());
                Ok(())
            })
            .unwrap();
        deliver_bridge_activation_with_store(&agent, &client, &event, &store).unwrap();
        assert!(server
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap()
            .is_none());
        let state = store.load_state().unwrap().unwrap();
        assert!(matches!(
            state.tickets["ticket-busy"].status,
            TicketStatus::Cancelled
        ));
        assert!(state.slot(&target).is_none());
        assert!(state.activation_audit_receipts.is_empty());
    }

    #[test]
    fn fake_feishu_turn_delivery_freezes_wire_redacts_archive_and_linearizes_stop() {
        use summon_core::bridge::{
            BridgeChoice, BridgeMode, BridgeTicket, ChatBinding, ChatSlot, ExternalLifecycle,
            ExternalLocator, ExternalSessionRecord, MirrorExportReceipt, ProcessFingerprint,
            ResumeProfile, ScopedChatKey, SessionKey, SessionLease, TicketStatus,
        };
        use summon_core::outbox::{
            bridge_delivery_id, BridgeDeliveryFence, BridgeDeliveryKind, OutboxEvent, OutboxKind,
        };

        let (agent, _agent_dir) = crate::test_support::agent_dir(
            "claude",
            "feishu:\n  app_id_env: TEST_APP\n  operators: [ou_owner]\n",
        );
        let scope = feishu_storage_scope(&agent).key;
        let target = ScopedChatKey::new(&scope, &agent.name, "oc_group");
        let session = SessionKey::new("/config", "sid");
        let binding = ChatBinding {
            id: "binding".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: session.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            revision: 7,
        };
        let receipt_id = "turn-receipt";
        let outbox_id = bridge_delivery_id(BridgeDeliveryKind::ExternalTurn, receipt_id);
        let fence = BridgeDeliveryFence {
            scope,
            agent: agent.name.clone(),
            chat_id: target.chat_id.clone(),
            chat_type: "group".into(),
            binding_id: binding.id.clone(),
            binding_revision: binding.revision,
            session: session.clone(),
            generation: 1,
            lease_binding_id: binding.id.clone(),
            receipt_id: receipt_id.into(),
            sequence: 2,
        };
        let frozen_card = r#"{"schema":"2.0","body":{"elements":[{"tag":"markdown","content":"VERY_SECRET_FROZEN"}]}}"#;
        let event = OutboxEvent {
            id: outbox_id.clone(),
            agent: agent.name.clone(),
            delivery_scope: fence.scope.clone(),
            created_ms: 1,
            attempts: 0,
            next_attempt_ms: 1,
            last_error: None,
            kind: OutboxKind::ExternalBridgeTurn {
                fence: fence.clone(),
                turn_id: "turn".into(),
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card: frozen_card.into(),
            },
        };
        let dir = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(dir.path().join("bridge"));
        store
            .with_state(|state| {
                state.upsert_session(ExternalSessionRecord {
                    key: session.clone(),
                    generation: 1,
                    locator: ExternalLocator {
                        key: session.clone(),
                        transcript_path: "/config/sid.jsonl".into(),
                        resume_lookup_cwd: "/project".into(),
                        current_cwd: "/project".into(),
                        profile: ResumeProfile {
                            claude_bin: "/bin/claude".into(),
                            config_dir: "/config".into(),
                            process_verified: true,
                            ..ResumeProfile::default()
                        },
                        revision: 1,
                    },
                    process: Some(ProcessFingerprint {
                        pid: 42,
                        start_time: "start".into(),
                        executable: "/bin/claude".into(),
                    }),
                    lifecycle: ExternalLifecycle::ActiveIdle,
                    first_seen_ms: 1,
                    last_seen_ms: 1,
                    last_applied_sequence: 1,
                    pending_prompt: None,
                });
                state
                    .slots
                    .insert(target.storage_key(), ChatSlot::MirrorBound(binding.clone()));
                state.session_leases.insert(
                    session.storage_key(),
                    SessionLease {
                        binding_id: binding.id.clone(),
                        target: target.clone(),
                    },
                );
                state.tickets.insert(
                    "ticket".into(),
                    BridgeTicket {
                        id: "ticket".into(),
                        short_code: None,
                        target: target.clone(),
                        target_chat_type: "group".into(),
                        target_provider: summon_core::bridge::ClaudeProvider::Default,
                        requester_open_id: "ou_owner".into(),
                        requester_union_id: String::new(),
                        control_card_message_id: Some("om_control".into()),
                        private_chat_id: Some("oc_private".into()),
                        slot_revision: 1,
                        choices: vec![BridgeChoice {
                            id: "choice".into(),
                            session: session.clone(),
                            registry_revision: 1,
                        }],
                        issued_at_ms: 1,
                        expires_at_ms: 10,
                        status: TicketStatus::Committed {
                            binding_id: binding.id.clone(),
                            choice_id: "choice".into(),
                            mode: BridgeMode::Mirror,
                        },
                    },
                );
                state.mirror_export_receipts.insert(
                    receipt_id.into(),
                    MirrorExportReceipt {
                        id: receipt_id.into(),
                        outbox_id: outbox_id.clone(),
                        turn_id: "turn".into(),
                        binding_id: binding.id.clone(),
                        binding_revision: binding.revision,
                        target: target.clone(),
                        session: session.clone(),
                        generation: 1,
                        sequence: 2,
                        delivered_at_ms: None,
                    },
                );
                Ok(())
            })
            .unwrap();

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (post_tx, post_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (body_tx, body_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            for _ in 0..2 {
                let mut request = server.recv().unwrap();
                if request.url().contains("auth/v3/tenant_access_token") {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"tenant_access_token":"token","expire":7200}"#,
                        ))
                        .unwrap();
                } else {
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    body_tx.send(body).unwrap();
                    post_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"code":0,"data":{"message_id":"om_sent"}}"#,
                        ))
                        .unwrap();
                }
            }
        });
        let client = crate::frontend::client::FeishuClient::with_test_base(base);
        let delivery_store = store.clone();
        let delivery_agent = agent.clone();
        let delivery_event = event.clone();
        let delivery = std::thread::spawn(move || {
            deliver_external_bridge_turn_with_store(
                &delivery_agent,
                &client,
                &delivery_event,
                &delivery_store,
            )
        });
        post_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();

        let stop_store = store.clone();
        let stop_binding = binding.clone();
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || {
            crate::bridge::runtime::stop_mirror_exact(
                &stop_store,
                &stop_binding.target,
                &stop_binding.id,
                stop_binding.revision,
            )
            .unwrap();
            stopped_tx.send(()).unwrap();
        });
        assert!(stopped_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        release_tx.send(()).unwrap();
        delivery.join().unwrap().unwrap();
        stopped_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        stop.join().unwrap();
        server_thread.join().unwrap();

        let body: serde_json::Value = serde_json::from_str(&body_rx.recv().unwrap()).unwrap();
        assert_eq!(body["uuid"], event.id);
        assert_eq!(body["content"], frozen_card);
        let outbound = std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(summon_core::registry::chat_tag("group", "oc_group"))
                .join("outbound.ndjson"),
        )
        .unwrap();
        assert!(!outbound.contains("VERY_SECRET_FROZEN"));
        assert!(outbound.contains(&event.id));
        assert!(store.load_state().unwrap().unwrap().slot(&target).is_none());
    }

    #[derive(Clone, Copy)]
    enum BridgeFirstHttpFailure {
        RateLimited,
        DisconnectAfterBody,
    }

    fn bridge_retry_fixture(
        label: &str,
    ) -> (
        Agent,
        tempfile::TempDir,
        tempfile::TempDir,
        crate::bridge::store::BridgeStore,
        summon_core::outbox::OutboxEvent,
        summon_core::bridge::ScopedChatKey,
        String,
    ) {
        use summon_core::bridge::{
            BridgeChoice, BridgeMode, BridgeTicket, ChatBinding, ChatSlot, ExternalLifecycle,
            ExternalLocator, ExternalSessionRecord, MirrorExportReceipt, ProcessFingerprint,
            ResumeProfile, ScopedChatKey, SessionKey, SessionLease, TicketStatus,
        };
        use summon_core::outbox::{
            bridge_delivery_id, BridgeDeliveryFence, BridgeDeliveryKind, OutboxEvent, OutboxKind,
        };

        let (agent, agent_dir) = crate::test_support::agent_dir(
            "claude",
            "feishu:\n  app_id_env: TEST_APP\n  operators: [ou_owner]\n",
        );
        let scope = feishu_storage_scope(&agent).key;
        let target = ScopedChatKey::new(&scope, &agent.name, format!("oc_retry_{label}"));
        let session = SessionKey::new("/config", format!("sid-retry-{label}"));
        let binding = ChatBinding {
            id: format!("binding-retry-{label}"),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: session.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            revision: 7,
        };
        let receipt_id = format!("receipt-retry-{label}");
        let outbox_id = bridge_delivery_id(BridgeDeliveryKind::ExternalTurn, &receipt_id);
        let event = OutboxEvent {
            id: outbox_id.clone(),
            agent: agent.name.clone(),
            delivery_scope: scope.clone(),
            created_ms: 1,
            attempts: 0,
            next_attempt_ms: 1,
            last_error: None,
            kind: OutboxKind::ExternalBridgeTurn {
                fence: BridgeDeliveryFence {
                    scope,
                    agent: agent.name.clone(),
                    chat_id: target.chat_id.clone(),
                    chat_type: "group".into(),
                    binding_id: binding.id.clone(),
                    binding_revision: binding.revision,
                    session: session.clone(),
                    generation: 1,
                    lease_binding_id: binding.id.clone(),
                    receipt_id: receipt_id.clone(),
                    sequence: 2,
                },
                turn_id: format!("turn-retry-{label}"),
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card: format!(
                    r#"{{"schema":"2.0","body":{{"elements":[{{"tag":"markdown","content":"VERY_SECRET_FROZEN_{label}"}}]}}}}"#
                ),
            },
        };
        let frozen_card = match &event.kind {
            OutboxKind::ExternalBridgeTurn { rendered_card, .. } => rendered_card.clone(),
            _ => unreachable!(),
        };
        let bridge_dir = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(bridge_dir.path().join("bridge"));
        store
            .with_state(|state| {
                state.upsert_session(ExternalSessionRecord {
                    key: session.clone(),
                    generation: 1,
                    locator: ExternalLocator {
                        key: session.clone(),
                        transcript_path: format!("/config/sid-retry-{label}.jsonl"),
                        resume_lookup_cwd: "/project".into(),
                        current_cwd: "/project".into(),
                        profile: ResumeProfile {
                            claude_bin: "/bin/claude".into(),
                            config_dir: "/config".into(),
                            process_verified: true,
                            ..ResumeProfile::default()
                        },
                        revision: 1,
                    },
                    process: Some(ProcessFingerprint {
                        pid: 42,
                        start_time: "start".into(),
                        executable: "/bin/claude".into(),
                    }),
                    lifecycle: ExternalLifecycle::ActiveIdle,
                    first_seen_ms: 1,
                    last_seen_ms: 1,
                    last_applied_sequence: 1,
                    pending_prompt: None,
                });
                state
                    .slots
                    .insert(target.storage_key(), ChatSlot::MirrorBound(binding.clone()));
                state.session_leases.insert(
                    session.storage_key(),
                    SessionLease {
                        binding_id: binding.id.clone(),
                        target: target.clone(),
                    },
                );
                state.tickets.insert(
                    format!("ticket-retry-{label}"),
                    BridgeTicket {
                        id: format!("ticket-retry-{label}"),
                        short_code: None,
                        target: target.clone(),
                        target_chat_type: "group".into(),
                        target_provider: summon_core::bridge::ClaudeProvider::Default,
                        requester_open_id: "ou_owner".into(),
                        requester_union_id: String::new(),
                        control_card_message_id: Some("om_control".into()),
                        private_chat_id: Some("oc_private".into()),
                        slot_revision: 1,
                        choices: vec![BridgeChoice {
                            id: format!("choice-retry-{label}"),
                            session: session.clone(),
                            registry_revision: 1,
                        }],
                        issued_at_ms: 1,
                        expires_at_ms: 10,
                        status: TicketStatus::Committed {
                            binding_id: binding.id.clone(),
                            choice_id: format!("choice-retry-{label}"),
                            mode: BridgeMode::Mirror,
                        },
                    },
                );
                state.mirror_export_receipts.insert(
                    receipt_id.clone(),
                    MirrorExportReceipt {
                        id: receipt_id,
                        outbox_id,
                        turn_id: format!("turn-retry-{label}"),
                        binding_id: binding.id.clone(),
                        binding_revision: binding.revision,
                        target: target.clone(),
                        session,
                        generation: 1,
                        sequence: 2,
                        delivered_at_ms: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        (
            agent,
            agent_dir,
            bridge_dir,
            store,
            event,
            target,
            frozen_card,
        )
    }

    fn assert_bridge_http_retry(first_failure: BridgeFirstHttpFailure, label: &str) {
        use crate::runtime::outbox_store::{OutboxLocation, OutboxStore};
        use summon_core::outbox::OutboxKind;

        let (agent, _agent_dir, _bridge_dir, bridge_store, event, _target, frozen_card) =
            bridge_retry_fixture(label);
        let receipt_id = event.kind.bridge_fence().unwrap().receipt_id.clone();
        let outbox_dir = tempfile::tempdir().unwrap();
        let outbox_root = outbox_dir.path().join("outbox");
        let outbox = OutboxStore::new(&outbox_root);
        outbox.enqueue(&event).unwrap();

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let sensitive_response = format!("VERY_SECRET_RESPONSE_{label}");
        let server_secret = sensitive_response.clone();
        let server_thread = std::thread::spawn(move || {
            let mut request_bodies = Vec::new();
            while request_bodies.len() < 2 {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .expect("等待 Bridge HTTP 请求超时");
                if request.url().contains("auth/v3/tenant_access_token") {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"tenant_access_token":"token","expire":7200}"#,
                        ))
                        .unwrap();
                    continue;
                }
                assert!(request.url().contains("receive_id_type=chat_id"));
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                request_bodies.push(body);
                if request_bodies.len() == 1 {
                    match first_failure {
                        BridgeFirstHttpFailure::RateLimited => request
                            .respond(
                                tiny_http::Response::from_string(format!(
                                    "rate limited: {server_secret}"
                                ))
                                .with_status_code(429),
                            )
                            .unwrap(),
                        BridgeFirstHttpFailure::DisconnectAfterBody => {
                            // `into_writer` 避免 Drop 自动回 500；丢弃原始 writer 关闭写端，
                            // 确保 client 看到的是“server 读完请求后断连”而不是 HTTP 失败响应。
                            drop(request.into_writer());
                        }
                    }
                } else {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"code":0,"data":{"message_id":"om_retry_sent"}}"#,
                        ))
                        .unwrap();
                }
            }
            request_bodies
        });
        let client = match first_failure {
            BridgeFirstHttpFailure::RateLimited => {
                crate::frontend::client::FeishuClient::with_test_base(base)
            }
            BridgeFirstHttpFailure::DisconnectAfterBody => {
                crate::frontend::client::FeishuClient::with_test_base_connection_close(base)
            }
        };
        let deliver = |candidate: &summon_core::outbox::OutboxEvent| {
            deliver_external_bridge_turn_with_store(&agent, &client, candidate, &bridge_store)
        };
        let on_acked = |candidate: &summon_core::outbox::OutboxEvent| {
            bridge_outbox_acked_with_store(candidate, &bridge_store)
        };

        assert_eq!(
            outbox.drain_tick_with_hooks(2, &deliver, &on_acked, &|_| Ok(())),
            0
        );
        let (location, saved) = outbox.find(&event.id).unwrap().unwrap();
        assert_eq!(location, OutboxLocation::Pending, "首次网络失败不得归档");
        assert_eq!(saved.attempts, 1);
        assert_eq!(saved.last_error.as_deref(), Some("bridge_delivery_failed"));
        assert!(saved.next_attempt_ms > 2);
        let receipt = bridge_store
            .load_state()
            .unwrap()
            .unwrap()
            .mirror_export_receipts
            .get(&receipt_id)
            .cloned()
            .expect("首次失败必须保留未完成 receipt");
        assert!(
            receipt.delivered_at_ms.is_none(),
            "未收到 2xx 不得写 delivered"
        );
        assert!(!outbox_root
            .join("archive")
            .join(format!("{}.json", event.id))
            .exists());
        let pending_raw = std::fs::read_to_string(
            outbox_root
                .join("pending")
                .join(format!("{}.json", event.id)),
        )
        .unwrap();
        assert!(!pending_raw.contains(&sensitive_response));
        assert!(!pending_raw.contains("rate limited:"));

        assert_eq!(
            outbox.drain_tick_with_hooks(saved.next_attempt_ms, &deliver, &on_acked, &|_| Ok(()),),
            1
        );
        let request_bodies = server_thread.join().unwrap();
        assert_eq!(request_bodies.len(), 2);
        assert_eq!(
            request_bodies[0], request_bodies[1],
            "重试 HTTP body 必须逐字节稳定"
        );
        let body: serde_json::Value = serde_json::from_str(&request_bodies[0]).unwrap();
        assert_eq!(body["uuid"], event.id);
        assert_eq!(body["content"], frozen_card);
        assert!(!bridge_store
            .load_state()
            .unwrap()
            .unwrap()
            .mirror_export_receipts
            .contains_key(&receipt_id));
        let (location, archived) = outbox.find(&event.id).unwrap().unwrap();
        assert_eq!(location, OutboxLocation::Archive);
        assert!(matches!(
            archived.kind,
            OutboxKind::ExternalBridgeTurn {
                ref rendered_card,
                ..
            } if rendered_card.is_empty()
        ));
        let archive_raw = std::fs::read_to_string(
            outbox_root
                .join("archive")
                .join(format!("{}.json", event.id)),
        )
        .unwrap();
        assert!(!archive_raw.contains(&sensitive_response));
        assert!(!archive_raw.contains(&frozen_card));
    }

    #[test]
    fn bridge_http_429_retries_real_frozen_request_without_early_ack_or_error_leak() {
        assert_bridge_http_retry(BridgeFirstHttpFailure::RateLimited, "429");
    }

    #[test]
    fn bridge_disconnect_after_body_retries_real_frozen_request_without_early_ack() {
        assert_bridge_http_retry(BridgeFirstHttpFailure::DisconnectAfterBody, "disconnect");
    }

    #[test]
    fn recovery_card_producer_freezes_wire_and_delivery_never_rerenders() {
        let (agent, _agent_dir) =
            crate::test_support::agent_dir("claude", "feishu:\n  app_id_env: TEST_RECOVERY_APP\n");
        std::fs::write(agent.dir.join(".env"), "TEST_RECOVERY_APP=test-app\n").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let scope = feishu_storage_scope(&agent).key;
        let record = InboxRecord {
            agent: agent.name.clone(),
            delivery_scope: scope.clone(),
            inbound: Inbound {
                anchor_message_id: None,
                chat_id: "oc_recovery".into(),
                chat_type: Some("group".into()),
                text: "VERY_SECRET_SOURCE_TEXT".into(),
                sender: "ou_owner".into(),
                sender_union_id: String::new(),
                sender_type: String::new(),
                message_id: "om_recovery".into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: vec![],
                resources: vec![],
                create_time_ms: 1,
                replayed: false,
                reattach: false,
            },
            scheduled: None,
            sentinel: None,
            started: true,
            rejected_disabled: false,
            superseded_by_new: false,
            recovery_origin: None,
            recovery_batch_id: None,
        };
        let batch = wal
            .create_recovery_batch(
                &scope,
                &agent.name,
                "oc_recovery",
                "group",
                Some("VERY_SECRET_STAGE".into()),
                vec![record],
                1,
            )
            .unwrap();
        let mut event = recovery_card_event(&batch).unwrap();
        let OutboxKind::RecoveryCard {
            card_version,
            rendered_card,
            ..
        } = &event.kind
        else {
            unreachable!()
        };
        assert!(summon_core::outbox::valid_frozen_bridge_card(
            *card_version,
            rendered_card,
        ));
        assert!(rendered_card.contains(&batch.id));

        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let mut legacy = event.clone();
        let OutboxKind::RecoveryCard {
            card_version,
            rendered_card,
            ..
        } = &mut legacy.kind
        else {
            unreachable!()
        };
        *card_version = 0;
        rendered_card.clear();
        outbox.enqueue(&legacy).unwrap();
        migrate_legacy_recovery_cards(
            &Registry {
                agents: vec![agent.clone()],
            },
            &outbox,
            &wal,
        );
        let (location, migrated) = outbox.find(&event.id).unwrap().unwrap();
        assert_eq!(
            location,
            crate::runtime::outbox_store::OutboxLocation::Pending
        );
        assert!(matches!(
            migrated.kind,
            OutboxKind::RecoveryCard {
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                ref rendered_card,
                ..
            } if summon_core::outbox::valid_frozen_bridge_card(
                summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card,
            )
        ));

        // 用一个与 items/stage 完全无关、但协议合法的 frozen wire 覆盖事件。若 delivery 偷偷
        // 调当前 renderer，HTTP body 会出现 source/stage 而不是该 sentinel。
        let frozen = r#"{"schema":"2.0","body":{"elements":[{"tag":"markdown","content":"FROZEN_RECOVERY_SENTINEL"}]}}"#;
        let OutboxKind::RecoveryCard { rendered_card, .. } = &mut event.kind else {
            unreachable!()
        };
        *rendered_card = frozen.into();

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let client = crate::frontend::client::FeishuClient::with_test_base(format!(
            "http://{}",
            server.server_addr().to_ip().unwrap()
        ));
        let server_thread = std::thread::spawn(move || loop {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .expect("等待 RecoveryCard HTTP 请求超时");
            if request.url().contains("auth/v3/tenant_access_token") {
                request
                    .respond(tiny_http::Response::from_string(
                        r#"{"tenant_access_token":"token","expire":7200}"#,
                    ))
                    .unwrap();
                continue;
            }
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            request
                .respond(tiny_http::Response::from_string(
                    r#"{"code":0,"data":{"message_id":"om_card"}}"#,
                ))
                .unwrap();
            break body;
        });

        deliver_recovery_card_with_wal(&agent, &client, &event, &wal).unwrap();
        let request_body = server_thread.join().unwrap();
        let request: serde_json::Value = serde_json::from_str(&request_body).unwrap();
        assert_eq!(request["uuid"], event.id);
        assert_eq!(request["content"], frozen);
        assert!(!request_body.contains("VERY_SECRET_SOURCE_TEXT"));
        assert!(!request_body.contains("VERY_SECRET_STAGE"));
        assert_eq!(
            wal.load_recovery_batch(&batch.id)
                .unwrap()
                .unwrap()
                .card_message_id
                .as_deref(),
            Some("om_card")
        );
        assert_eq!(
            recovery_delivery_preflight(&event, &wal).unwrap(),
            RecoveryDeliveryPreflight::LocalAck
        );
        assert!(deliver_outbox_event_with_wal(&HashMap::new(), &event, &wal).is_ok());
        let mut mismatched = event.clone();
        let OutboxKind::RecoveryCard { items, .. } = &mut mismatched.kind else {
            unreachable!()
        };
        items[0].preview = "DRIFTED_PREVIEW".into();
        assert!(recovery_delivery_preflight(&mismatched, &wal).is_err());

        let missing = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("missing-inbox"));
        let missing_error = recovery_delivery_preflight(&event, &missing).unwrap_err();
        assert!(format!("{missing_error:#}").contains("缺 active/completed manifest"));

        let mut legacy = event;
        let OutboxKind::RecoveryCard {
            card_version,
            rendered_card,
            ..
        } = &mut legacy.kind
        else {
            unreachable!()
        };
        *card_version = 0;
        rendered_card.clear();
        let no_request_server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let no_request_client = crate::frontend::client::FeishuClient::with_test_base(format!(
            "http://{}",
            no_request_server.server_addr().to_ip().unwrap()
        ));
        let error =
            deliver_recovery_card_with_wal(&agent, &no_request_client, &legacy, &wal).unwrap_err();
        assert!(format!("{error:#}").contains("等待显式迁移"));
        assert!(no_request_server
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap()
            .is_none());
    }

    #[test]
    fn unscoped_legacy_recovery_without_authority_is_redacted_not_left_pending() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let event = summon_core::outbox::OutboxEvent {
            id: "legacy-recovery-orphan".into(),
            agent: "missing-agent".into(),
            delivery_scope: String::new(),
            created_ms: 1,
            attempts: 0,
            next_attempt_ms: 1,
            last_error: None,
            kind: OutboxKind::RecoveryCard {
                batch_id: String::new(),
                scope: String::new(),
                chat_id: "oc_legacy".into(),
                chat_type: "group".into(),
                items: vec![summon_core::outbox::RecoveryItem {
                    message_id: "om_legacy".into(),
                    preview: "VERY_SECRET_LEGACY_PREVIEW".into(),
                }],
                stage: Some("VERY_SECRET_LEGACY_STAGE".into()),
                card_version: 0,
                rendered_card: "VERY_SECRET_LEGACY_WIRE".into(),
            },
        };
        outbox.enqueue(&event).unwrap();

        migrate_legacy_recovery_cards(&Registry { agents: vec![] }, &outbox, &wal);

        let (location, archived) = outbox.find(&event.id).unwrap().unwrap();
        assert_eq!(
            location,
            crate::runtime::outbox_store::OutboxLocation::Archive
        );
        assert!(matches!(
            archived.kind,
            OutboxKind::RecoveryCard {
                ref items,
                stage: None,
                ref rendered_card,
                ..
            } if items.is_empty() && rendered_card.is_empty()
        ));
        assert!(!outbox
            .has_unresolved_local_chat("missing-agent", "oc_legacy")
            .unwrap());
        let raw = std::fs::read_to_string(
            temp.path()
                .join("outbox/archive")
                .join(format!("{}.json", event.id)),
        )
        .unwrap();
        assert!(!raw.contains("VERY_SECRET_LEGACY"));
    }

    /// 重投时被飞书内容审核拦下的那一段改发说明、算作送达,事件照常销账。
    #[test]
    fn turn_reply_outbox_replaces_an_audit_rejected_segment_with_a_notice() {
        struct Rejecting {
            sent: std::cell::RefCell<Vec<String>>,
        }
        impl Frontend for Rejecting {
            fn send_message(&self, _chat: &ChatRef, body: &str) -> anyhow::Result<()> {
                if body == "right" {
                    return Err(anyhow::Error::new(
                        crate::frontend::client::ContentRejected::for_test(
                            "contain sensitive data: EMAIL_ADDRESS",
                        ),
                    ));
                }
                self.sent.borrow_mut().push(body.to_string());
                Ok(())
            }
            fn split_final_reply(&self, _chat: &ChatRef, _body: &str) -> Vec<String> {
                vec!["left".into(), "right".into()]
            }
            fn stream_begin(&self, _: &ChatRef, _: &str, _: bool) -> anyhow::Result<StreamId> {
                Ok(StreamId(1))
            }
            fn stream_patch(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn react(&self, _msg: &MsgRef, _key: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
            fn unreact(&self, _msg: &MsgRef, _reaction_id: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let frontend = Rejecting {
            sent: std::cell::RefCell::new(Vec::new()),
        };
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_outbox_rejected",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        deliver_turn_reply(&frontend, &chat, "leftright").expect("说明发出即算送达");

        let sent = frontend.sent.borrow();
        assert_eq!(sent[0], "left");
        assert!(sent[1].contains("飞书内容审核拦下"), "{sent:?}");
    }

    #[test]
    fn turn_reply_outbox_uses_adapter_splitter_before_sending() {
        let frontend = SplitReplyFrontend {
            sent: std::cell::RefCell::new(Vec::new()),
        };
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_outbox_split",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        deliver_turn_reply(&frontend, &chat, "leftright").unwrap();

        assert_eq!(&*frontend.sent.borrow(), &["left", "right"]);
    }

    /// 重投用的是 agent 的原始全文,末尾的总结标签要在分段前拿掉,不能作为正文补发出去。
    #[test]
    fn turn_reply_outbox_drops_the_feed_tag_before_splitting() {
        let frontend = SplitReplyFrontend {
            sent: std::cell::RefCell::new(Vec::new()),
        };
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_outbox_feed",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        deliver_turn_reply(
            &frontend,
            &chat,
            "leftright\n<summon-feed>重投总结</summon-feed>",
        )
        .unwrap();

        assert_eq!(&*frontend.sent.borrow(), &["left", "right"]);
    }

    fn rec(id: &str, create_ms: i64, started: bool) -> InboxRecord {
        InboxRecord {
            agent: "a".into(),
            delivery_scope: "test-scope".into(),
            inbound: Inbound {
                anchor_message_id: None,
                chat_id: "oc_1".into(),
                chat_type: Some("p2p".into()),
                text: format!("消息 {id}"),
                sender: "ou_1".into(),
                sender_union_id: String::new(),
                sender_type: String::new(),
                message_id: id.into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: vec![],
                resources: vec![],
                create_time_ms: create_ms,
                replayed: false,
                reattach: false,
            },
            scheduled: None,
            sentinel: None,
            started,
            rejected_disabled: false,
            superseded_by_new: false,
            recovery_origin: None,
            recovery_batch_id: None,
        }
    }

    fn scheduled_rec(id: &str, started: bool) -> InboxRecord {
        let mut rec = rec(id, 1000, started);
        rec.inbound.sender.clear();
        rec.scheduled = Some(crate::runtime::inbox_wal::ScheduledInbox {
            chat_id: "oc_1".into(),
            chat_type: "p2p".into(),
            prompt: "巡检".into(),
            fire_id: id.into(),
            output: summon_core::ScheduleOutput::Progress,
            context: summon_core::ScheduleContext::Resume,
        });
        rec
    }

    #[test]
    fn superseded_boot_recovery_settles_the_crash_window_without_a_recovery_card() {
        for crash in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let inbox_root = root.path().join("inbox");
            let wal = crate::runtime::inbox_wal::InboxWal::new(inbox_root.clone());
            let inbound = rec("om-old", 1000, false).inbound;
            wal.append("a", "test-scope", &inbound).unwrap();
            wal.discard_superseded("a", "test-scope", &inbound.message_id)
                .unwrap();
            std::fs::remove_dir_all(inbox_root.join("pending")).unwrap();
            std::fs::rename(inbox_root.join("consumed"), inbox_root.join("pending")).unwrap();
            let restarted = crate::runtime::inbox_wal::InboxWal::new(inbox_root);
            let remaining =
                settle_terminal_inbox(&restarted, restarted.scan_pending_strict().unwrap())
                    .unwrap();
            let disposition = classify_inbox_recovery(remaining, crash, true, false, false);
            assert!(disposition.recovery_records.is_empty());
            assert!(disposition.reinject.is_empty());
            assert!(restarted.scan_pending().is_empty());
        }
    }

    #[test]
    fn disabled_boot_recovery_finishes_both_rejection_crash_windows() {
        for notification_written in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let inbox_root = root.path().join("inbox");
            let wal = crate::runtime::inbox_wal::InboxWal::new(inbox_root.clone());
            let inbound = rec("om-disabled", 1000, false).inbound;
            wal.append("agent-a", "scope-a", &inbound).unwrap();
            crate::runtime::agent_availability::AgentAvailability::lock(&inbox_root, "agent-a")
                .unwrap()
                .set_enabled(false)
                .unwrap();
            if notification_written {
                assert!(wal
                    .reject_disabled("agent-a", "scope-a", &inbound.message_id)
                    .unwrap());
                std::fs::remove_dir_all(inbox_root.join("pending")).unwrap();
                std::fs::rename(inbox_root.join("consumed"), inbox_root.join("pending")).unwrap();
            } else {
                let blocked = root.path().join("outbox");
                std::fs::write(&blocked, b"blocked").unwrap();
                assert!(wal
                    .mark_started("agent-a", "scope-a", &inbound.message_id)
                    .is_err());
                std::fs::remove_file(blocked).unwrap();
            }
            assert!(wal.scan_pending()[0].rejected_disabled);
            crate::runtime::agent_availability::AgentAvailability::lock(&inbox_root, "agent-a")
                .unwrap()
                .set_enabled(true)
                .unwrap();
            let restarted = crate::runtime::inbox_wal::InboxWal::new(inbox_root);
            let remaining =
                settle_terminal_inbox(&restarted, restarted.scan_pending_strict().unwrap())
                    .unwrap();
            assert!(remaining.is_empty());
            let disposition = classify_inbox_recovery(remaining, true, false, false, false);
            assert!(disposition.recovery_records.is_empty());
            assert!(disposition.reinject.is_empty());
            assert!(restarted.scan_pending().is_empty());
            assert!(
                restarted
                    .read_consumed("agent-a", &inbound.message_id)
                    .unwrap()
                    .unwrap()
                    .rejected_disabled
            );
            assert_eq!(
                std::fs::read_dir(root.path().join("outbox/pending"))
                    .unwrap()
                    .count(),
                1
            );
        }
    }

    fn ids(v: &[Inbound]) -> Vec<String> {
        v.iter().map(|i| i.message_id.clone()).collect()
    }

    #[test]
    fn graceful_never_started_reinjected_marked_replayed_and_sorted() {
        // C9 + C7:优雅重启、未开跑的排队消息 → 重注入;打 replayed 标记(豁免时效/水位闸,
        // 停机多久都不再被静默丢);按原 create_time 升序(处理顺序=到达顺序);且**不立即销账**
        // (留 pending,由 chat 线程处理完再 mark_consumed —— 投递成功才 ack + 避开去重预热竞态)。
        // 2026-08-05 前用「create_time 归零」当免检信号,WAL 因此丢真实发送时间;现改为
        // 控制信号(replayed)与数据(create_time_ms)分离,两者都要钉住。
        let recs = vec![rec("om_2", 2000, false), rec("om_1", 1000, false)];
        let d = classify_inbox_recovery(recs, false, false, false, false);
        assert_eq!(
            ids(&d.reinject),
            vec!["om_1", "om_2"],
            "按 create_time 升序"
        );
        assert!(
            d.reinject.iter().all(|i| i.replayed),
            "重注入消息打 replayed 标记豁免时效/水位闸"
        );
        assert_eq!(
            d.reinject
                .iter()
                .map(|i| i.create_time_ms)
                .collect::<Vec<_>>(),
            vec![1000, 2000],
            "create_time_ms 保真:WAL 里必须留得住真实发送时间"
        );
        assert!(d.recovery_records.is_empty());
    }

    #[test]
    fn graceful_stale_reinject_still_admitted_no_age_inversion() {
        // C9 时效闸倒挂:停机 30min(或 >24h)本会让原 create_time 落入 DropStale 区间。
        // replayed 标记在 dispatcher 调用点映射成 create_time=0(闸门定义 0=未知→免时效)
        // → 恒 Admit —— 重注入行为对停机时长不再敏感、连续;WAL 里 create_time_ms 保真。
        let old_create = 1_000_000_i64;
        let recs = vec![rec_ct("om_old", old_create, false)];
        let d = classify_inbox_recovery(recs, false, false, false, false);
        assert_eq!(d.reinject.len(), 1);
        let m = &d.reinject[0];
        assert!(m.replayed, "重注入必须带 replayed 标记");
        assert_eq!(m.create_time_ms, old_create, "create_time_ms 保真");
        // 用 dispatcher 入站闸调用点的**那一份**映射,不在这儿手抄一遍。
        let gate_create_ms = super::dispatcher::gate_create_ms(m);
        // 停机 30 分钟(age 落在旧 DropStale 区间)——映射后 Admit。
        let now_30min = old_create + 30 * 60 * 1000;
        assert_eq!(
            summon_core::inbound::admit_inbound(
                &summon_core::inbound::InboundMeta {
                    message_id: &m.message_id,
                    chat_id: &m.chat_id,
                    create_time_ms: gate_create_ms,
                },
                now_30min,
                summon_core::inbound::DEFAULT_MAX_AGE_MS,
                &std::collections::HashSet::new(),
                None,
            ),
            summon_core::inbound::Admission::Admit,
            "replayed 映射后停机 30min 的重注入仍放行(不再被时效闸静默丢)"
        );
    }

    #[test]
    fn graceful_started_claude_goes_to_card_not_reinject() {
        // C8 ①:claude 已开跑的消息——存活 tmux 里原轮可能已跑完(甚至已提交代码),重注入=副作用
        // 双执行 → 走恢复卡由用户确认,不静默自动重放。销账(read 回退 consumed,恢复按钮可取回)。
        let recs = vec![rec("om_run", 1000, true)];
        let d = classify_inbox_recovery(recs, false, /*is_headless*/ false, false, false);
        assert!(d.reinject.is_empty(), "已开跑的 claude 消息不重注入");
        assert_eq!(d.recovery_records.len(), 1);
        assert_eq!(d.recovery_records[0].inbound.message_id, "om_run");
    }

    /// 优雅 + 已开跑 + Transcript + 有效锚点:轮还在 tmux 里跑,交给新一代接管。这条记录既不
    /// 重注入(会同一句话跑两遍)也不发卡(那一轮还有人接),`reattach` 与 `replayed` 一起打 ——
    /// 入站闸对两者都免检,而接管复用原始 message_id/create_time,不免检会被时效闸静默吞掉。
    #[test]
    fn graceful_started_transcript_with_anchor_goes_to_reattach() {
        let d = classify_inbox_recovery(vec![rec("om_1", 1000, true)], false, false, false, true);
        assert_eq!(ids(&d.reattach), vec!["om_1"]);
        assert!(d.reattach[0].reattach && d.reattach[0].replayed);
        assert_eq!(
            d.reattach[0].create_time_ms, 1000,
            "create_time_ms 保真:锚点要拿它与 WAL 对账"
        );
        assert!(d.recovery_records.is_empty() && d.reinject.is_empty());
    }

    /// 接管只在四条同时成立时开:硬崩不敢确定跑没跑完、无头有自己的续跑语义、没有有效锚点
    /// 则证不了那一轮的下落。任一条不成立都退回今天的分支(恢复卡),宁可漏接管。
    #[test]
    fn reattach_branch_is_closed_for_crash_headless_and_missing_anchor() {
        for (was_crash, headless, anchor) in [
            (true, false, true),
            (false, true, true),
            (false, false, false),
        ] {
            let d = classify_inbox_recovery(
                vec![rec("om_1", 1000, true)],
                was_crash,
                headless,
                false,
                anchor,
            );
            assert!(
                d.reattach.is_empty(),
                "case {:?}",
                (was_crash, headless, anchor)
            );
            assert_eq!(ids_of(&d.recovery_records), vec!["om_1"]);
        }
    }

    /// 接管轮必须先投:chat 线程串行处理,排队消息排在接管轮前面就会先撞上一条正忙的 pane,
    /// 而接管轮等到被处理时锚点早已过期。
    #[test]
    fn the_reattached_turn_is_injected_before_any_queued_message() {
        let d = classify_inbox_recovery(
            vec![rec("om_running", 1000, true), rec("om_queued", 2000, false)],
            false,
            false,
            false,
            true,
        );
        assert_eq!(ids(&d.reattach), vec!["om_running"]);
        assert_eq!(ids(&d.reinject), vec!["om_queued"]);
        assert_eq!(
            ids(&injection_order(d.reattach, d.reinject)),
            vec!["om_running", "om_queued"]
        );
    }

    /// 硬崩路径**连锚点都不读**:`&&` 的短路顺序就是判据。顺序反了,崩溃恢复会平白多出一趟
    /// 盘上扫描,还会为一条它根本不会走的路留日志 —— 硬崩行为要逐字节不变。
    #[test]
    fn a_crash_boot_never_reads_a_turn_anchor() {
        let src = include_str!("outbox_drain.rs");
        let body = &src[src
            .find("pub(super) fn boot_recover_inbox(")
            .expect("boot_recover_inbox")..];
        let guard = body
            .find("let has_transcript_anchor = !was_crash")
            .expect("短路守卫");
        let read = body.find("reattach_anchor_on_disk(").expect("读盘调用");
        assert!(guard < read, "`!was_crash` 必须短路在读锚点之前");
    }

    /// 停机时刻是 **daemon 级**事实,不是某条 mirror fence 的。首轮就被重启的 chat 一条 fence
    /// 都签不出来(它还没提交过 mirror offset),按 fence 取就永远判过期、永远不接管。
    #[test]
    fn reattach_freshness_reads_the_daemon_level_stopped_at_not_a_fence() {
        let record = super::super::CleanShutdownRecordV2 {
            version: 2,
            mirror_fences: vec![MirrorStopFence {
                agent: "a".into(),
                feishu_epoch_fingerprint: "fp".into(),
                session: "soul-a".into(),
                tag: "g1".into(),
                chat_id: "oc_1".into(),
                chat_type: "group".into(),
                offset_record: String::new(),
                stop_eof: 0,
                stop_eof_checkpoint: String::new(),
                stopped_at_secs: 111,
            }],
            stopped_at_secs: 999,
        };
        assert_eq!(reattach_stopped_at_secs(Some(&record)), 999);
        assert_eq!(
            reattach_stopped_at_secs(Some(&super::super::CleanShutdownRecordV2 {
                version: 2,
                mirror_fences: Vec::new(),
                stopped_at_secs: 999,
            })),
            999,
            "一条 fence 都没有也照样拿得到停机时刻"
        );
        assert_eq!(
            reattach_stopped_at_secs(None),
            0,
            "硬崩没有停机记录 → 判过期"
        );
    }

    /// 定时 / sentinel / recovery replay 各有各的恢复协议(fire_id 幂等、人工再授权),它们的
    /// 记录**先于**普通分支处置。锚点为真也不得把它们卷进接管 —— 那会绕开各自的幂等边界。
    #[test]
    fn scheduled_sentinel_and_recovery_replay_never_enter_reattach() {
        let mut replay = rec("recovery-replay", 1_000, true);
        replay.recovery_origin = Some("om_original".into());
        let mut sentinel = rec("sentinel-1", 1_000, true);
        sentinel.sentinel = Some(crate::runtime::inbox_wal::SentinelInbox {
            chat_id: "oc_1".into(),
            chat_type: "p2p".into(),
            prompt: "巡检".into(),
            fire_id: "sentinel-1".into(),
            who: "哨兵".into(),
            who_id: String::new(),
            who_union_id: String::new(),
            handoff_note: None,
        });
        for record in [replay, scheduled_rec("sched-SCH-1-0", true), sentinel] {
            let d = classify_inbox_recovery(vec![record], false, false, false, true);
            assert!(d.reattach.is_empty());
        }
    }

    /// 未开跑的记录与锚点无关:它从没进过 claude,重注入零双执行风险,仍走 reinject。
    #[test]
    fn unstarted_record_still_reinjects_even_with_anchor() {
        let d = classify_inbox_recovery(vec![rec("om_1", 1000, false)], false, false, false, true);
        assert_eq!(ids(&d.reinject), vec!["om_1"]);
        assert!(d.reattach.is_empty());
    }

    #[test]
    fn graceful_headless_resume_keeps_inbox_evidence_until_terminal() {
        // C8 ②:无头一轮在跑(started + 有 pending.json,resume_pending_turn 续跑)+ 后续排队消息
        // (未开跑)。旧 per-chat has_pending 会把整组跳过 → 排队消息丢失。消息粒度下:排队消息
        // 重注入不丢；续跑轮的 Inbox 证据必须留到 resume 真正终态。否则“优雅启动续跑 →
        // 续跑中 SIGKILL”会因为 Inbox 已空，在下一代再次静默自动 resume。
        let recs = vec![rec("om_running", 1000, true), rec("om_queued", 2000, false)];
        let d = classify_inbox_recovery(
            recs, false, /*is_headless*/ true, /*has_pending_json*/ true, false,
        );
        assert_eq!(ids(&d.reinject), vec!["om_queued"], "排队消息重注入不丢");
        assert!(
            d.recovery_records.is_empty(),
            "在跑轮走 resume 续跑、不发卡"
        );
        assert_eq!(
            ids_of(&d.recovery_records),
            Vec::<String>::new(),
            "精确续跑轮保留 Inbox，但不生成恢复卡"
        );
    }

    #[test]
    fn crash_routes_all_to_recovery_card() {
        // 崩溃(SIGKILL):什么都不敢确定跑没跑完 → 全部进恢复卡,无重注入(#3 不自动重跑)。
        let recs = vec![rec("om_a", 1000, false), rec("om_b", 2000, true)];
        let d = classify_inbox_recovery(recs, true, false, false, false);
        assert!(d.reinject.is_empty());
        assert_eq!(ids_of(&d.recovery_records), vec!["om_a", "om_b"]);
    }

    #[test]
    fn started_recovery_replay_is_held_without_nested_batch_or_headless_resume() {
        let mut replay = rec("recovery-replay", 1_000, true);
        replay.recovery_origin = Some("om_original".into());
        replay.recovery_batch_id = Some("recovery-card-batch".into());

        for was_crash in [false, true] {
            let disposition = classify_inbox_recovery(
                vec![replay.clone()],
                was_crash,
                /* is_headless */ true,
                /* has_pending_json */ true,
                false,
            );
            assert!(disposition.holds_interrupted_recovery);
            assert!(disposition.reinject.is_empty());
            assert!(disposition.recovery_records.is_empty());
            assert!(disposition.scheduled_reinject.is_empty());
        }
    }

    #[test]
    fn authorized_unstarted_recovery_replay_is_reinjected_once() {
        let mut replay = rec("recovery-replay", 1_000, false);
        replay.recovery_origin = Some("om_original".into());
        replay.recovery_batch_id = Some("recovery-card-batch".into());
        let disposition = classify_inbox_recovery(vec![replay], true, true, true, false);
        assert_eq!(ids(&disposition.reinject), vec!["recovery-replay"]);
        assert!(!disposition.holds_interrupted_recovery);
        assert!(disposition.recovery_records.is_empty());
    }

    /// 没开跑的定时 fire 照常自动重注入 —— 零双执行风险,漏了就是任务没做。
    #[test]
    fn unstarted_scheduled_is_auto_reinjected_even_after_crash() {
        for was_crash in [false, true] {
            let d = classify_inbox_recovery(
                vec![scheduled_rec("sched-SCH-1-0", false)],
                was_crash,
                true,
                true,
                false,
            );
            assert_eq!(d.scheduled_reinject.len(), 1);
            assert_eq!(d.scheduled_reinject[0].fire_id, "sched-SCH-1-0");
            assert!(d.scheduled_retire.is_empty());
            assert!(d.reinject.is_empty());
            assert!(d.recovery_records.is_empty(), "scheduled 不走人工恢复卡");
        }
    }

    /// **已开跑没跑完的定时 fire 退票销账,不补跑。**
    ///
    /// 这条测试替换了原来的 `scheduled_is_auto_reinjected_even_when_crashed_or_started`
    /// —— 那条编码的是「无论 started 都重注入」的旧契约,而那个契约在真机上是死的:chat
    /// 线程进 `handle_task` 前的 `mark_started` 会判 `AlreadyStarted` 原样弹回,记录既跑不掉
    /// 也销不了账,每次 daemon 启动空转一轮(devbox 实测)。
    ///
    /// owner 2026-08-13 在「清 start 标记补跑」与「退票不跑」之间选了后者:定时任务可能带
    /// 不可逆的外部动作,补跑的坏情况是重复执行,漏跑的坏情况只是这次没做成。
    #[test]
    fn started_scheduled_is_retired_instead_of_looping_forever() {
        for was_crash in [false, true] {
            let d = classify_inbox_recovery(
                vec![scheduled_rec("sched-SCH-1-0", true)],
                was_crash,
                true,
                true,
                false,
            );
            assert_eq!(d.scheduled_retire.len(), 1);
            assert_eq!(d.scheduled_retire[0].fire_id, "sched-SCH-1-0");
            assert!(
                d.scheduled_reinject.is_empty(),
                "已开跑的 fire 绝不能再重注入 —— 那正是空转的成因"
            );
            assert!(d.reinject.is_empty());
            assert!(d.recovery_records.is_empty(), "scheduled 不走人工恢复卡");
        }
    }

    #[test]
    fn stale_recovery_batch_is_terminalized_without_new_app_delivery_or_plaintext() {
        let temp = tempfile::tempdir().unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let mut source = rec("om_stale", 1_000, true);
        source.inbound.text = "VERY_SECRET_STALE_INBOX".into();
        wal.append("a", "test-scope", &source.inbound).unwrap();
        assert_eq!(
            wal.mark_started("a", "test-scope", "om_stale").unwrap(),
            crate::runtime::inbox_wal::MarkStartedOutcome::Marked
        );
        let batch = wal
            .create_recovery_batch(
                "test-scope",
                "a",
                "oc_1",
                "p2p",
                Some("VERY_SECRET_STALE_STAGE".into()),
                vec![source],
                1_000,
            )
            .unwrap();
        let event = recovery_card_event(&batch).unwrap();
        outbox.enqueue(&event).unwrap();

        abandon_stale_recovery_batch(&outbox, &wal, &batch).unwrap();

        assert!(wal.load_recovery_batch(&batch.id).unwrap().is_none());
        assert!(wal
            .recovery_outbox_completed(&batch.id, &batch.outbox_id)
            .unwrap());
        assert!(wal.scan_pending().is_empty());
        let (location, terminal) = outbox.find(&event.id).unwrap().unwrap();
        assert_eq!(
            location,
            crate::runtime::outbox_store::OutboxLocation::Archive
        );
        assert!(matches!(
            terminal.kind,
            OutboxKind::RecoveryCard {
                ref items,
                stage: None,
                ref rendered_card,
                ..
            } if items.is_empty() && rendered_card.is_empty()
        ));
        let persisted = format!(
            "{:?}{:?}",
            std::fs::read_dir(temp.path().join("inbox"))
                .unwrap()
                .count(),
            terminal
        );
        assert!(!persisted.contains("VERY_SECRET_STALE"));
    }

    #[test]
    fn persisted_scope_distinguishes_changed_from_temporarily_unknown_identity() {
        let env_name = "SUMMON_TEST_PERSISTED_SCOPE_APP_7D1F";
        let (mut agent, _dir) = crate::test_support::agent_dir(
            "claude",
            &format!("feishu:\n  app_id_env: {env_name}\n"),
        );
        std::fs::write(agent.dir.join(".env"), format!("{env_name}=app-old\n")).unwrap();
        let old_scope = feishu_storage_scope(&agent).key;
        let mut reg = Registry {
            agents: vec![agent.clone()],
        };
        assert_eq!(
            persisted_scope_relation(&reg, &agent.name, &old_scope),
            PersistedScopeRelation::ExactRoutable
        );

        std::fs::write(agent.dir.join(".env"), format!("{env_name}=app-new\n")).unwrap();
        reg.agents[0] = agent.clone();
        assert_eq!(
            persisted_scope_relation(&reg, &agent.name, &old_scope),
            PersistedScopeRelation::KnownDifferent
        );

        std::fs::remove_file(agent.dir.join(".env")).unwrap();
        assert_eq!(
            persisted_scope_relation(&reg, &agent.name, &old_scope),
            PersistedScopeRelation::Unknown,
            "缺 env 只是暂时不可证明，不能不可逆废弃"
        );

        std::fs::write(agent.dir.join(".env"), format!("{env_name}=app-old\n")).unwrap();
        agent.status = "inactive".into();
        reg.agents[0] = agent.clone();
        assert_eq!(
            persisted_scope_relation(&reg, &agent.name, &old_scope),
            PersistedScopeRelation::ExactUnavailable
        );
    }

    fn scoped_recovery_fixture(
        env_name: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Agent,
        crate::runtime::inbox_wal::InboxWal,
        crate::runtime::outbox_store::OutboxStore,
        crate::runtime::inbox_wal::RecoveryBatch,
    ) {
        let (agent, agent_dir) = crate::test_support::agent_dir(
            "claude",
            &format!("feishu:\n  app_id_env: {env_name}\n"),
        );
        std::fs::write(agent.dir.join(".env"), format!("{env_name}=app-old\n")).unwrap();
        let scope = feishu_storage_scope(&agent).key;
        let temp = tempfile::tempdir().unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let mut record = rec("om_scope", 1_000, true);
        record.agent = agent.name.clone();
        record.delivery_scope = scope.clone();
        let batch = wal
            .create_recovery_batch(
                &scope,
                &agent.name,
                "oc_1",
                "p2p",
                None,
                vec![record],
                1_000,
            )
            .unwrap();
        outbox
            .enqueue(&recovery_card_event(&batch).unwrap())
            .unwrap();
        (temp, agent_dir, agent, wal, outbox, batch)
    }

    /// 显式重置作废的是**本群**的恢复批次:inbox 侧不再有未决项(它是 `/bridge` 占用判据之一)、
    /// 对应恢复卡的出站事件归档、之后再点那张卡按「已处理或失效」收尾;同 agent 别的群原样保留。
    #[test]
    fn chat_recovery_terminalization_frees_only_that_chat() {
        let (_temp, _agent_dir, agent, wal, outbox, batch) =
            scoped_recovery_fixture("SUMMON_TEST_RESET_RECOVERY_7C42");
        let scope = batch.scope.clone();
        let mut other = rec("om_other_chat", 2_000, true);
        other.agent = agent.name.clone();
        other.delivery_scope = scope.clone();
        other.inbound.chat_id = "oc_2".into();
        let other_batch = wal
            .create_recovery_batch(&scope, &agent.name, "oc_2", "p2p", None, vec![other], 2_000)
            .unwrap();
        outbox
            .enqueue(&recovery_card_event(&other_batch).unwrap())
            .unwrap();
        assert!(wal
            .has_unresolved_recovery(&scope, &agent.name, "oc_1")
            .unwrap());

        let discarded = crate::runtime::daemon::teardown::terminalize_chat_recovery(
            &wal,
            &outbox,
            &scope,
            &agent.name,
            "oc_1",
        )
        .unwrap();

        assert_eq!(discarded, 1);
        assert!(!wal
            .has_unresolved_recovery(&scope, &agent.name, "oc_1")
            .unwrap());
        assert!(wal.load_recovery_batch(&batch.id).unwrap().is_none());
        assert!(matches!(
            outbox.find(&batch.outbox_id).unwrap(),
            Some((crate::runtime::outbox_store::OutboxLocation::Archive, _))
        ));
        assert!(wal
            .has_unresolved_recovery(&scope, &agent.name, "oc_2")
            .unwrap());
        assert!(matches!(
            outbox.find(&other_batch.outbox_id).unwrap(),
            Some((crate::runtime::outbox_store::OutboxLocation::Pending, _))
        ));
    }

    #[test]
    fn recovery_batch_ttl_is_strict_and_ignores_unknown_creation_time() {
        assert!(!recovery_batch_expired(
            1_000,
            1_000 + RECOVERY_BATCH_TTL_MS
        ));
        assert!(recovery_batch_expired(
            1_000,
            1_000 + RECOVERY_BATCH_TTL_MS + 1
        ));
        assert!(
            !recovery_batch_expired(0, u64::MAX),
            "缺创建时间不能当成无限老"
        );
        assert!(!recovery_batch_expired(5_000, 1_000), "时钟回拨不算超期");
    }

    /// 超期作废只发生在身份证得一致的批次上;身份不可证明时维持隔离保留,超期也不动。
    #[test]
    fn boot_reconcile_abandons_identity_proven_batches_past_ttl_only() {
        let (_temp, _agent_dir, agent, wal, outbox, batch) =
            scoped_recovery_fixture("SUMMON_TEST_RECOVERY_TTL_5B17");
        let reg = Registry {
            agents: vec![agent],
        };
        reconcile_recovery_batches(
            &reg,
            &outbox,
            &wal,
            batch.created_ms + RECOVERY_BATCH_TTL_MS,
        )
        .unwrap();
        assert!(wal.load_recovery_batch(&batch.id).unwrap().is_some());

        reconcile_recovery_batches(
            &reg,
            &outbox,
            &wal,
            batch.created_ms + RECOVERY_BATCH_TTL_MS + 1,
        )
        .unwrap();
        assert!(wal.load_recovery_batch(&batch.id).unwrap().is_none());
        assert!(matches!(
            outbox.find(&batch.outbox_id).unwrap(),
            Some((crate::runtime::outbox_store::OutboxLocation::Archive, _))
        ));

        let (
            _unknown_temp,
            _unknown_agent_dir,
            unknown_agent,
            unknown_wal,
            unknown_outbox,
            unknown_batch,
        ) = scoped_recovery_fixture("SUMMON_TEST_RECOVERY_TTL_UNKNOWN_0D88");
        std::fs::remove_file(unknown_agent.dir.join(".env")).unwrap();
        reconcile_recovery_batches(
            &Registry {
                agents: vec![unknown_agent],
            },
            &unknown_outbox,
            &unknown_wal,
            u64::MAX,
        )
        .unwrap();
        assert!(unknown_wal
            .load_recovery_batch(&unknown_batch.id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn confirmed_app_change_abandons_recovery_but_missing_env_only_quarantines() {
        let (
            _changed_temp,
            _changed_agent_dir,
            changed_agent,
            changed_wal,
            changed_outbox,
            changed_batch,
        ) = scoped_recovery_fixture("SUMMON_TEST_RECOVERY_CHANGED_APP_9A20");
        std::fs::write(
            changed_agent.dir.join(".env"),
            "SUMMON_TEST_RECOVERY_CHANGED_APP_9A20=app-new\n",
        )
        .unwrap();
        reconcile_recovery_batches(
            &Registry {
                agents: vec![changed_agent],
            },
            &changed_outbox,
            &changed_wal,
            changed_batch.created_ms,
        )
        .unwrap();
        assert!(changed_wal
            .load_recovery_batch(&changed_batch.id)
            .unwrap()
            .is_none());
        assert!(matches!(
            changed_outbox.find(&changed_batch.outbox_id).unwrap(),
            Some((crate::runtime::outbox_store::OutboxLocation::Archive, _))
        ));

        let (
            _unknown_temp,
            _unknown_agent_dir,
            unknown_agent,
            unknown_wal,
            unknown_outbox,
            unknown_batch,
        ) = scoped_recovery_fixture("SUMMON_TEST_RECOVERY_UNKNOWN_APP_E3D1");
        std::fs::remove_file(unknown_agent.dir.join(".env")).unwrap();
        reconcile_recovery_batches(
            &Registry {
                agents: vec![unknown_agent],
            },
            &unknown_outbox,
            &unknown_wal,
            unknown_batch.created_ms,
        )
        .unwrap();
        assert!(unknown_wal
            .load_recovery_batch(&unknown_batch.id)
            .unwrap()
            .is_some());
        assert!(matches!(
            unknown_outbox.find(&unknown_batch.outbox_id).unwrap(),
            Some((crate::runtime::outbox_store::OutboxLocation::Pending, _))
        ));
    }

    #[test]
    fn cold_boot_releases_only_stale_mirror_and_preserves_handoff_sensitive_state() {
        use summon_core::bridge::{
            BridgeMode, ChatBinding, ChatSlot, ScopedChatKey, SessionKey, SessionLease,
        };
        let env_name = "SUMMON_TEST_STALE_BRIDGE_APP_6F31";
        let (agent, _agent_dir) = crate::test_support::agent_dir(
            "claude",
            &format!("feishu:\n  app_id_env: {env_name}\n"),
        );
        std::fs::write(agent.dir.join(".env"), format!("{env_name}=app-old\n")).unwrap();
        let old_scope = feishu_storage_scope(&agent).key;
        let mirror_target = ScopedChatKey::new(&old_scope, &agent.name, "oc_mirror");
        let handoff_target = ScopedChatKey::new(&old_scope, &agent.name, "oc_handoff");
        let mirror_session = SessionKey::new("/config", "sid-mirror");
        let handoff_session = SessionKey::new("/config", "sid-handoff");
        let binding = |id: &str, target: ScopedChatKey, session: SessionKey, mode| ChatBinding {
            id: id.into(),
            target,
            target_chat_type: "group".into(),
            session,
            mode,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            revision: 1,
        };
        let mirror = binding(
            "binding-mirror",
            mirror_target.clone(),
            mirror_session.clone(),
            BridgeMode::Mirror,
        );
        let handoff = binding(
            "binding-handoff",
            handoff_target.clone(),
            handoff_session.clone(),
            BridgeMode::Handoff,
        );
        let temp = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(temp.path().join("bridge"));
        store
            .with_state(|state| {
                state.slots.insert(
                    mirror_target.storage_key(),
                    ChatSlot::MirrorBound(mirror.clone()),
                );
                state.slots.insert(
                    handoff_target.storage_key(),
                    ChatSlot::HandoffPending(handoff.clone()),
                );
                state.session_leases.insert(
                    mirror_session.storage_key(),
                    SessionLease {
                        binding_id: mirror.id.clone(),
                        target: mirror_target.clone(),
                    },
                );
                state.session_leases.insert(
                    handoff_session.storage_key(),
                    SessionLease {
                        binding_id: handoff.id.clone(),
                        target: handoff_target.clone(),
                    },
                );
                Ok(())
            })
            .unwrap();
        std::fs::write(agent.dir.join(".env"), format!("{env_name}=app-new\n")).unwrap();

        reconcile_stale_bridge_identities(
            &Registry {
                agents: vec![agent],
            },
            &store,
        );

        let state = store.load_state().unwrap().unwrap();
        assert!(state.slot(&mirror_target).is_none());
        assert!(!state
            .session_leases
            .contains_key(&mirror_session.storage_key()));
        assert!(matches!(
            state.slot(&handoff_target),
            Some(ChatSlot::HandoffPending(binding)) if binding.id == handoff.id
        ));
        assert!(state
            .session_leases
            .contains_key(&handoff_session.storage_key()));
    }

    fn rec_ct(id: &str, ct: i64, started: bool) -> InboxRecord {
        rec(id, ct, started)
    }
    fn ids_of(v: &[InboxRecord]) -> Vec<String> {
        v.iter()
            .map(|record| record.inbound.message_id.clone())
            .collect()
    }
}

#[cfg(test)]
mod drain_snapshot_tests {
    use super::*;

    /// 一次投递卡在联网上时,刷新投递表的写锁仍拿得到。
    #[test]
    fn delivery_runs_outside_the_table_lock() {
        let agents: DrainAgents = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let table = agents.clone();
        let delivering = thread::spawn(move || {
            with_drain_snapshot(&table, |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        entered_rx.recv().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let writable = loop {
            if agents.try_write().is_ok() {
                break true;
            }
            if std::time::Instant::now() > deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(10));
        };
        release_tx.send(()).unwrap();
        delivering.join().unwrap().unwrap();
        assert!(writable, "投递进行中拿不到投递表写锁");
    }
}

#[cfg(test)]
mod refresh_agent_tests {
    use super::*;

    /// agent.yaml 改了、飞书身份没变:投递表里那份 agent 原地换新(sentinel 召唤名单、`/handoff`
    /// 目标都读它),投递用的 client 与身份范围不动。
    #[test]
    fn refreshing_an_agent_updates_the_delivery_table_in_place() {
        let old: Agent = serde_yaml::from_str("name: coder\ndisplay: 旧名\n").unwrap();
        let fresh: Agent = serde_yaml::from_str("name: coder\ndisplay: 新名\n").unwrap();
        let epoch = FeishuEpochIdentity::from_resolved(&old, Some("app-a"), Some("secret-a"));
        let client = Arc::new(FeishuClient::with_test_base("http://127.0.0.1:1".into()));
        let table: DrainAgents = Arc::new(std::sync::RwLock::new(HashMap::from([(
            old.name.clone(),
            OutboxDrainAgent {
                agent: old.clone(),
                client: Arc::clone(&client),
                epoch: epoch.clone(),
            },
        )])));
        let core = crate::runtime::core::Core::new(HashMap::new(), HashMap::new(), vec![old]);
        core.attach_agent_identities(Arc::clone(&table));

        core.refresh_agent(&fresh);

        let identities = core.local_agent_identities();
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].agent.display, "新名");
        assert!(Arc::ptr_eq(&identities[0].client, &client), "client 不动");
        assert_eq!(identities[0].delivery_scope, epoch.storage_scope().key);
    }
}
