//! daemon 换代后接管上一代那一轮:它的收尾与三条去处。
//!
//! claude 的 tmux 会话是持久层,跨 daemon 重启活着,轮也跟着继续跑;接住输出的 chat 线程却
//! 随 daemon 一起没了。接管就是让新一代按上一代落的 [`summon_core::TurnAnchor`] **零注入**
//! 地接住那一轮 —— 重新注入等于同一句话跑两遍。
//!
//! 一条 WAL 记录终态唯一,三条去处互斥 —— 接管成功销掉锚点认领的**全组** id
//! ([`consume_reattached_turn`]);`NotDelivered` 按未开跑重投一次;其余一律转人工恢复卡且
//! **不销账**。

use super::*;

/// 一轮 Transcript 消费的收尾。
///
/// 接管失败单独成一支:三类失败各有各的补救动作(未送达重投 / 其余退恢复卡),消费核不替
/// 调用方决定,也不发那张通用的「本轮没正常完成」卡 —— 它对三类失败全是错的。
pub(super) enum TranscriptTurnEnd {
    Completed(TurnResult),
    ReattachFailed(anyhow::Error),
}

impl TranscriptTurnEnd {
    /// 非接管形态的调用方用它取终态。接管失败那一支它们结构上到不了(接管形态只从锚点来),
    /// 真到了也只按普通失败收 —— 热路径不 panic。
    pub(super) fn settled(self) -> TurnResult {
        match self {
            Self::Completed(result) => result,
            Self::ReattachFailed(_) => TurnResult::Failed,
        }
    }
}

/// 接管失败后的补救动作。三类失败各有各的去处,合成一种就必错一边。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReattachNextStep {
    /// 上一代那条 prompt 一个字节都没进 claude(pane 已被正向证明回到空输入框):按普通轮
    /// 重新注入一次。这是唯一能安全重跑的一类。
    ReinjectMarked,
    /// 会话已经不在 / 消费窗内没等到 / 认不出的错:证不了这一轮的下落,交人工恢复卡。
    RecoveryCard,
}

/// 只有「从没进过 claude」才重投。判反了两边都是事故:未送达退恢复卡是让用户手点一条本可
/// 自动重投的消息;会话没了或消费超时当未送达重投,就是同一句话跑两遍。
pub(super) fn reattach_next_step(error: &anyhow::Error) -> ReattachNextStep {
    match error.downcast_ref::<crate::capture::ReattachError>() {
        Some(crate::capture::ReattachError::NotDelivered) => ReattachNextStep::ReinjectMarked,
        Some(_) | None => ReattachNextStep::RecoveryCard,
    }
}

/// 从 pending WAL 里挑出本轮名下那些记录,交给恢复卡。挑宽了会把同一 agent 别的会话的消息
/// 一起塞进卡里,挑窄了则漏掉被 steer 进本轮的那几条 —— 它们与主消息同生共死。
pub(super) fn reattach_recovery_records(
    pending: Vec<crate::runtime::inbox_wal::InboxRecord>,
    agent: &str,
    scope: &str,
    chat_id: &str,
    message_ids: &[String],
) -> Vec<crate::runtime::inbox_wal::InboxRecord> {
    pending
        .into_iter()
        .filter(|record| {
            record.agent == agent
                && record.delivery_scope == scope
                && record.inbound.chat_id == chat_id
                && message_ids.contains(&record.inbound.message_id)
        })
        .collect()
}

/// 接管轮正常收尾:把锚点认领的**全部**入站记录销账。
///
/// 普通轮的收尾只销主消息的账,轮内 steer 进来的那几条由本代的 followup 收尾负责。接管轮的
/// steer 发生在**上一代**,本代手里没有它们的 followup 记录 —— 不在这里销,它们就永远留在
/// pending:下次冷启 `resume_sources_match` 恒判不等,这个 chat 从此再也进不了接管,还会给
/// 已经答过的消息发恢复卡。
///
/// 只在正常收尾调用。失败路归恢复卡,一条都不能销 —— 销了用户就没有恢复入口了。
pub(super) fn consume_reattached_turn(
    wal: &crate::runtime::inbox_wal::InboxWal,
    agent_name: &str,
    delivery_scope: &str,
    message_ids: &[String],
    result: TurnResult,
) -> TurnResult {
    let mut all_consumed = true;
    for message_id in message_ids {
        match wal.mark_consumed_after_start(agent_name, delivery_scope, message_id) {
            Ok(crate::runtime::inbox_wal::MarkConsumedOutcome::Consumed)
            | Ok(crate::runtime::inbox_wal::MarkConsumedOutcome::AlreadyConsumed) => {}
            Ok(outcome) => {
                all_consumed = false;
                log(&summon_i18n::format!("r91a6e24b0a"));
            }
            Err(error) => {
                all_consumed = false;
                log(&summon_i18n::format!("rf6354afa4d"));
            }
        }
    }
    if all_consumed {
        result
    } else {
        // 主消息可能已经销账，不能让外层的 AlreadyConsumed 掩盖其余来源的提交失败。
        TurnResult::RecoveryDeferred
    }
}

/// 接管证不了、或接管失败:把本轮名下仍在 pending 的 WAL 记录转成一张人工恢复卡,以**非终态**
/// 收尾。
///
/// 非终态是判据不是口味 —— `Failed` 会让 chat 线程的既有收尾 `mark_consumed`,把恢复卡赖以
/// 存在的那条 pending 记录销掉。一条 WAL 记录只能有一个终态,走到这里它归恢复批次。
///
/// 三个 store 由调用方注入而非就地自建:接管是热路径上的旁支,普通轮一个都不该碰,而单测
/// 要能把它们指到 tempdir 上。
#[allow(clippy::too_many_arguments)] // 三个 store + identity 四元组,收 struct 只是换个地方列
pub(super) fn reattach_to_recovery_card(
    frontend: &dyn Frontend,
    wal: &crate::runtime::inbox_wal::InboxWal,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    reactions: &crate::runtime::reaction_store::ReactionStore,
    agent_name: &str,
    delivery_scope: &str,
    chat_id: &str,
    chat_type: &str,
    message_ids: &[String],
) -> TurnResult {
    match wal.scan_pending_strict() {
        Ok(pending) => {
            let records = reattach_recovery_records(
                pending,
                agent_name,
                delivery_scope,
                chat_id,
                message_ids,
            );
            if records.is_empty() {
                log(&summon_i18n::format!("rec11fd7c3b"));
                // 记录去哪了决定 👀 撤不撤:**已 consumed** 证明上一代其实收了尾,这一轮不会再有
                // 人接,表情留着就是在说谎;**查不到**只说明证不了下落(WAL 半写、被别的路径搬走),
                // 那时留着 —— 撤早了用户会以为没人管。
                let settled = message_ids
                    .iter()
                    .filter(|message_id| {
                        matches!(wal.read_consumed(agent_name, message_id), Ok(Some(_)))
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                retract_onit(frontend, reactions, agent_name, &settled);
            } else {
                match super::outbox_drain::emit_recovery_card(
                    wal,
                    outbox,
                    delivery_scope,
                    agent_name,
                    chat_id,
                    chat_type,
                    None,
                    records,
                ) {
                    Ok(true) => retract_onit(frontend, reactions, agent_name, message_ids),
                    // 卡没进 outbox(死信 / 入队失败)时 WAL 记录原样留着,由下次冷启的恢复扫描
                    // 兜底;OnIt 也留着 —— 还有人接得住这一轮。原因 `emit_recovery_card` 已记。
                    Ok(false) => {}
                    Err(e) => log(&summon_i18n::format!("red28f0ecb5")),
                }
            }
        }
        Err(e) => log(&summon_i18n::format!("rd273619ddb")),
    }
    TurnResult::RecoveryDeferred
}

/// 撤掉上一代贴的 OnIt(👀)。这条路**没有下一代续刷**:表情留着就是在说「还在处理」,
/// 而这一轮要么已交给用户手动决定,要么上一代早就收了尾。
///
/// 只对**确定不会再有人接手**的那些 message_id 调用:卡真进了 outbox,或记录已 consumed。
/// 证不了就留着 —— 撤早了用户会以为没人管。表情是旁路,失败只 log:撤不掉不该反过来影响
/// 恢复卡本身。
fn retract_onit(
    frontend: &dyn Frontend,
    reactions: &crate::runtime::reaction_store::ReactionStore,
    agent_name: &str,
    message_ids: &[String],
) {
    for message_id in message_ids {
        let Some(record) = reactions.find_by_message(message_id) else {
            continue;
        };
        let target = MsgRef {
            message_id: record.anchor(),
        };
        if let Err(e) = frontend.unreact(&target, &record.onit_reaction_id) {
            log(&summon_i18n::format!("r3c46b03915"));
        }
        reactions.remove(message_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::{ChatRef, StreamId};
    use crate::runtime::inbox_wal::InboxRecord;
    use std::cell::RefCell;

    /// 三类接管失败各有各的补救,合成一种就必错一边:未送达当恢复卡是让用户手点一条本可自动
    /// 重投的消息,而会话没了 / 消费超时当未送达重投,就是同一句话跑两遍。
    #[test]
    fn reattach_next_step_only_reinjects_when_prompt_never_reached_claude() {
        use crate::capture::ReattachError;
        assert_eq!(
            reattach_next_step(&ReattachError::NotDelivered.into()),
            ReattachNextStep::ReinjectMarked
        );
        assert_eq!(
            reattach_next_step(&ReattachError::SessionGone.into()),
            ReattachNextStep::RecoveryCard
        );
        assert_eq!(
            reattach_next_step(&ReattachError::ConsumeTimeout.into()),
            ReattachNextStep::RecoveryCard
        );
        assert_eq!(
            reattach_next_step(&anyhow::anyhow!("转录已比停机时记下的消费边界短")),
            ReattachNextStep::RecoveryCard,
            "认不出的错误一律按证不了处理"
        );
    }

    fn inbound(chat_id: &str, message_id: &str) -> Inbound {
        Inbound {
            anchor_message_id: None,
            chat_id: chat_id.into(),
            chat_type: Some("p2p".into()),
            text: format!("正文 {message_id}"),
            sender: "ou_1".into(),
            sender_union_id: String::new(),
            sender_type: "user".into(),
            message_id: message_id.into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms: 1,
            replayed: false,
            reattach: false,
        }
    }

    fn record(agent: &str, scope: &str, chat_id: &str, message_id: &str) -> InboxRecord {
        InboxRecord {
            agent: agent.into(),
            delivery_scope: scope.into(),
            inbound: inbound(chat_id, message_id),
            scheduled: None,
            sentinel: None,
            started: true,
            rejected_disabled: false,
            superseded_by_new: false,
            recovery_origin: None,
            recovery_batch_id: None,
        }
    }

    /// 恢复卡只收本轮名下的记录:挑宽了会把同一 agent 别的会话的消息一起塞进卡里(用户看到
    /// 一堆与本轮无关的待恢复消息),挑窄了则漏掉被 steer 进本轮的那几条。
    #[test]
    fn reattach_recovery_records_take_only_this_turns_pending_rows() {
        let pending = vec![
            record("agent-a", "scope-a", "oc_1", "om_1"),
            record("agent-a", "scope-a", "oc_1", "om_2"),
            record("agent-a", "scope-a", "oc_1", "om_other"),
            record("agent-a", "scope-a", "oc_2", "om_1"),
            record("agent-b", "scope-a", "oc_1", "om_1"),
            record("agent-a", "scope-b", "oc_1", "om_2"),
        ];
        let ids = vec!["om_1".to_string(), "om_2".to_string()];
        let picked = reattach_recovery_records(pending, "agent-a", "scope-a", "oc_1", &ids);
        assert_eq!(
            picked
                .iter()
                .map(|r| r.inbound.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["om_1", "om_2"]
        );
    }

    #[derive(Default)]
    struct UnreactSpy {
        unreacted: RefCell<Vec<(String, String)>>,
    }

    impl Frontend for UnreactSpy {
        fn send_message(&self, _chat: &ChatRef, _body: &str) -> anyhow::Result<()> {
            Ok(())
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
        fn react(&self, _msg: &MsgRef, key: &str) -> anyhow::Result<String> {
            Ok(format!("rid-{key}"))
        }
        fn unreact(&self, msg: &MsgRef, reaction_id: &str) -> anyhow::Result<()> {
            self.unreacted
                .borrow_mut()
                .push((msg.message_id.to_string(), reaction_id.to_string()));
            Ok(())
        }
    }

    /// 接管证不了时那条 pending 记录的去处只有恢复卡:outbox 里出现 RecoveryCard 事件、记录
    /// 交给 recovery batch,而**绝不能进 consumed** —— 销账等于把用户唯一的恢复入口删了。
    /// 顺带钉住 OnIt:发卡成功后撤掉(这条路没有下一代续刷,👀 留着就是在说谎)。
    #[test]
    fn anchor_mismatch_hands_the_record_to_a_recovery_card_without_acking_it() {
        let temp = tempfile::tempdir().unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let reactions =
            crate::runtime::reaction_store::ReactionStore::new(temp.path().join("reactions"));
        let msg = inbound("oc_1", "om_1");
        wal.append("agent-a", "scope-a", &msg).unwrap();
        wal.mark_started("agent-a", "scope-a", "om_1").unwrap();
        reactions.enqueue(&crate::runtime::reaction_store::ReactionRecord {
            message_id: "om_1".into(),
            anchor_message_id: None,
            delivered_at_ms: 1,
            onit_reaction_id: "rid-onit".into(),
            chat_id: "oc_1".into(),
            chat_type: "p2p".into(),
            agent_reaction_id: None,
        });

        let frontend = UnreactSpy::default();
        let result = reattach_to_recovery_card(
            &frontend,
            &wal,
            &outbox,
            &reactions,
            "agent-a",
            "scope-a",
            "oc_1",
            "p2p",
            &["om_1".to_string()],
        );

        assert_eq!(
            result,
            TurnResult::RecoveryDeferred,
            "恢复卡路径必须是非终态:终态会让 chat 线程的既有收尾把这条记录销账"
        );
        assert!(
            wal.read_consumed("agent-a", "om_1").unwrap().is_none(),
            "恢复卡路径绝不销账"
        );
        let batches = wal.active_recovery_batches().unwrap();
        assert_eq!(batches.len(), 1, "记录应交给一个 recovery batch");
        assert_eq!(batches[0].items[0].record.inbound.message_id, "om_1");
        assert!(
            outbox.pending_events().iter().any(|event| matches!(
                event.kind,
                summon_core::outbox::OutboxKind::RecoveryCard { .. }
            )),
            "恢复卡必须进 outbox,否则用户永远看不到这一轮的下落"
        );
        assert_eq!(
            frontend.unreacted.borrow().as_slice(),
            [("om_1".to_string(), "rid-onit".to_string())]
        );
        assert!(
            reactions.find_by_message("om_1").is_none(),
            "撤完就把记录清掉,别让下次收尾再撤一次"
        );
    }

    /// 接管轮正常收尾要把**锚点那一整组** id 都销账,不只主消息:上一代 steer 进来的那几条
    /// 本代没有 followup 收尾,漏销就永远留在 pending —— 下次冷启 `resume_sources_match` 判不等,
    /// 这个 chat 从此再也进不了接管,还会给已经答过的消息发恢复卡。
    #[test]
    fn a_reattached_turn_acks_every_message_the_anchor_claimed() {
        let temp = tempfile::tempdir().unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        for message_id in ["om_main", "om_steered"] {
            wal.append("agent-a", "scope-a", &inbound("oc_1", message_id))
                .unwrap();
            wal.mark_started("agent-a", "scope-a", message_id).unwrap();
        }

        consume_reattached_turn(
            &wal,
            "agent-a",
            "scope-a",
            &["om_main".to_string(), "om_steered".to_string()],
            TurnResult::Done,
        );

        for message_id in ["om_main", "om_steered"] {
            assert!(
                wal.read_consumed("agent-a", message_id).unwrap().is_some(),
                "{message_id} 应已销账"
            );
        }
        assert!(
            wal.scan_pending_strict().unwrap().is_empty(),
            "接管轮收尾后本 chat 不该再留 pending —— 留着下次冷启就判不等"
        );
    }

    #[test]
    fn a_reattached_turn_defers_exit_until_every_source_is_consumed() {
        use crate::runtime::inbox_wal::MarkConsumedOutcome;
        for failure in ["missing", "unstarted", "wrong-scope"] {
            let temp = tempfile::tempdir().unwrap();
            let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
            wal.append("agent-a", "scope-a", &inbound("oc_1", "om_main"))
                .unwrap();
            wal.mark_started("agent-a", "scope-a", "om_main").unwrap();
            wal.mark_consumed_after_start("agent-a", "scope-a", "om_main")
                .unwrap();
            if failure != "missing" {
                let scope = if failure == "wrong-scope" {
                    "scope-other"
                } else {
                    "scope-a"
                };
                wal.append("agent-a", scope, &inbound("oc_1", "om_steered"))
                    .unwrap();
                if failure == "wrong-scope" {
                    wal.mark_started("agent-a", scope, "om_steered").unwrap();
                }
            }
            let result = consume_reattached_turn(
                &wal,
                "agent-a",
                "scope-a",
                &["om_main".into(), "om_steered".into()],
                TurnResult::Done,
            );
            assert_eq!(
                wal.mark_consumed_after_start("agent-a", "scope-a", "om_main")
                    .unwrap(),
                MarkConsumedOutcome::AlreadyConsumed
            );
            assert_eq!(result, TurnResult::RecoveryDeferred, "{failure}");
            assert!(
                !result.is_terminal(),
                "外层主消息已销账也不能覆盖全组销账失败"
            );
            assert!(wal
                .read_consumed("agent-a", "om_steered")
                .unwrap()
                .is_none());
            if failure != "missing" {
                assert_eq!(
                    wal.scan_pending_strict().unwrap().len(),
                    1,
                    "保留未销账来源"
                );
            }
        }
    }

    /// 卡没进 outbox 时 👀 必须留着:那条记录仍在 pending,下次冷启的恢复扫描还接得住这一轮,
    /// 而撤了表情等于当面告诉用户「不管了」。这里把 outbox 的根指到一个**普通文件**上,
    /// 任何入队都写不下去 —— 判据是「卡确实进了 outbox」,不是「我们试过发」。
    #[test]
    fn a_card_that_never_reached_the_outbox_keeps_the_onit_reaction() {
        let temp = tempfile::tempdir().unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let blocked = temp.path().join("outbox-is-a-file");
        std::fs::write(&blocked, b"").unwrap();
        let outbox = crate::runtime::outbox_store::OutboxStore::new(blocked);
        let reactions =
            crate::runtime::reaction_store::ReactionStore::new(temp.path().join("reactions"));
        let msg = inbound("oc_1", "om_1");
        wal.append("agent-a", "scope-a", &msg).unwrap();
        wal.mark_started("agent-a", "scope-a", "om_1").unwrap();
        reactions.enqueue(&crate::runtime::reaction_store::ReactionRecord {
            message_id: "om_1".into(),
            anchor_message_id: None,
            delivered_at_ms: 1,
            onit_reaction_id: "rid-onit".into(),
            chat_id: "oc_1".into(),
            chat_type: "p2p".into(),
            agent_reaction_id: None,
        });

        let frontend = UnreactSpy::default();
        let result = reattach_to_recovery_card(
            &frontend,
            &wal,
            &outbox,
            &reactions,
            "agent-a",
            "scope-a",
            "oc_1",
            "p2p",
            &["om_1".to_string()],
        );

        assert_eq!(result, TurnResult::RecoveryDeferred);
        assert_eq!(
            wal.active_recovery_batches().unwrap().len(),
            1,
            "卡在入队这一步才该失败:batch 都没落成的话下面的断言证明不了判据"
        );
        assert!(
            frontend.unreacted.borrow().is_empty(),
            "卡没进 outbox 就撤 OnIt,等于对着一条还有人接的消息说「不管了」"
        );
        assert!(
            reactions.find_by_message("om_1").is_some(),
            "表情记录留着,下次收尾还要用"
        );
        assert!(
            wal.read_consumed("agent-a", "om_1").unwrap().is_none(),
            "更不能销账"
        );
    }

    /// 本轮已无 pending 记录时 👀 撤不撤,判据是**那条记录去哪了**:已 consumed 说明上一代其实
    /// 收了尾,不会再有人接,表情该撤;查不到则证不了下落,留着。
    #[test]
    fn no_pending_rows_retracts_onit_only_for_records_proven_consumed() {
        let temp = tempfile::tempdir().unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let reactions =
            crate::runtime::reaction_store::ReactionStore::new(temp.path().join("reactions"));
        for message_id in ["om_settled", "om_vanished"] {
            reactions.enqueue(&crate::runtime::reaction_store::ReactionRecord {
                message_id: message_id.into(),
                anchor_message_id: None,
                delivered_at_ms: 1,
                onit_reaction_id: format!("rid-{message_id}"),
                chat_id: "oc_1".into(),
                chat_type: "p2p".into(),
                agent_reaction_id: None,
            });
        }
        // om_settled 走完 append → started → consumed;om_vanished 盘上压根没有。
        wal.append("agent-a", "scope-a", &inbound("oc_1", "om_settled"))
            .unwrap();
        wal.mark_started("agent-a", "scope-a", "om_settled")
            .unwrap();
        wal.mark_consumed("agent-a", "om_settled");

        let frontend = UnreactSpy::default();
        let result = reattach_to_recovery_card(
            &frontend,
            &wal,
            &outbox,
            &reactions,
            "agent-a",
            "scope-a",
            "oc_1",
            "p2p",
            &["om_settled".to_string(), "om_vanished".to_string()],
        );

        assert_eq!(result, TurnResult::RecoveryDeferred);
        assert_eq!(
            frontend.unreacted.borrow().as_slice(),
            [("om_settled".to_string(), "rid-om_settled".to_string())],
            "只撤证得已收尾的那条"
        );
        assert!(
            reactions.find_by_message("om_vanished").is_some(),
            "下落不明的那条留着 OnIt"
        );
        assert!(
            outbox.pending_events().is_empty(),
            "没有 pending 记录就没有恢复卡可发"
        );
    }
}
