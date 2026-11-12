//! 会话退出回执的**唯一组装入口**:CLI(`agents kill`)与 daemon(面板 Clean/Reset、群解散、
//! 离线归档、停机打断)共用。
//!
//! 为什么收口:历史上两侧各写一份「文案渲染 → chat label 回退 → 卡片渲染 → 时间戳 → 随机 id →
//! 写去重标记」,底层只共用到 `summon_core::outbox::build_notify_event`。分头组装已经付过一次
//! 代价——`09b2ada` 修的「chat_type 缺失导致去重键不匹配 → 同一次退出发两条」只动了 CLI 一侧。
//!
//! 分层:纯组装 `compose_event` 不碰 IO(可确定性断言),`enqueue` 负责注入时间/随机数、落盘、
//! 写去重标记。

use crate::runtime::outbox_store::OutboxStore;
use summon_core::config::Messaging;
use summon_core::exit::ExitReason;
use summon_core::outbox::OutboxEvent;
use summon_core::registry::{chat_tag, Agent};

/// 一次退出回执的完整载荷。字段即「这次退出是谁、发给哪个 chat、为什么」——
/// 组装所需信息全在这里,调用方不再各自拼装中间产物。
pub(crate) struct ExitReceipt<'a> {
    pub(crate) agent: &'a Agent,
    /// 出站身份闸门用的 scope。停机路径传 pending turn 记的历史 scope(不能按当前配置重算),
    /// 其余路径传 `feishu_delivery_scope(agent)`,故必须是入参而非现算。
    pub(crate) delivery_scope: &'a str,
    pub(crate) chat_type: &'a str,
    pub(crate) chat_id: &'a str,
    pub(crate) chat_name: Option<&'a str>,
    /// `.meta` 记的群主近似;仅「发原 chat 失败回退 p2p」时用得上,读不到降级 None 可接受。
    pub(crate) creator: Option<&'a str>,
    /// 飞书事件带的操作者(仅群解散 / bot 被移除路径有)。
    pub(crate) event_operator: Option<&'a str>,
    pub(crate) reason: ExitReason,
    /// 本会话**首次冷启**时间(`%Y-%m-%d %H:%M:%S`),用于退出卡片展示「起始」。
    /// resume 不更新;reset 后新会话重写。外部接管/历史会话可能为 None。
    pub(crate) session_created_at: Option<&'a str>,
}

/// 入队结果。调用方据此决定怎么告诉人:daemon 写日志,CLI 打印到终端。
/// 「文案被配空」不是失败——是用户显式配置的「不发」,不该报错。
pub(crate) enum EnqueueOutcome {
    Enqueued,
    SkippedEmptyText,
    Failed(anyhow::Error),
}

impl EnqueueOutcome {
    /// daemon 侧统一的收尾约定:入队失败只 log,**绝不阻塞拆除/停机**(这条回执就丢了)。
    /// 收在这里而不是各调用点,是因为它是一条策略而非五份代码——停机路径尤其不能因为
    /// 某个调用点忘了兜底而在 2s 宽限里冒出个 `?`。
    /// CLI 不用它:终端要区分「跳过」与「失败」并当场讲给人听。
    pub(crate) fn log_if_failed(self, agent_name: &str) {
        if let EnqueueOutcome::Failed(e) = self {
            crate::observe::log::log(&summon_i18n::format!("r7560284473"));
        }
    }
}

/// 这次退出要不要写去重标记。标记语义是**本 chat 的这次退出已经有人负责回执了**,
/// 只对「会话真的结束了」成立——把它做成 reason 的函数,而不是入队后的无条件副作用。
///
/// `ShutdownInterrupt` 不写:停机中断不是退出。tmux 与 `.meta` 都还在,chat 继续存在,
/// 重启即续跑;而标记的清除钩子只有「新入站写 `.meta`」和「idle-scan 且仅 Transcript」两处,
/// 续跑路径(`resume_pending_turn`)两处都不经过 → 标记会一直留在盘上,把之后一次真实的
/// offline kill(daemon 未运行时)回执静默吞掉。
pub(crate) fn writes_exit_mark(reason: ExitReason) -> bool {
    !matches!(reason, ExitReason::ShutdownInterrupt)
}

/// 退出卡片上的「会话」列:群名优先,无群名/空群名回退稳定短标识——不能空着让人不知道
/// 说的是哪个会话。**必须 trim + 判空**:`.meta` 里的 chat_name 可能是空串。
pub(crate) fn exit_chat_label(chat_name: Option<&str>, chat_type: &str, chat_id: &str) -> String {
    chat_name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| chat_tag(chat_type, chat_id))
}

/// 退出卡片上的「时间」列:本地 `MM-DD HH:MM`。只给人看,不参与任何判定。
pub(crate) fn now_local_hm() -> String {
    chrono::Local::now().format("%m-%d %H:%M").to_string()
}

/// 8 位随机十六进制(outbox 事件 id 末段);uuid 已是本仓依赖,复用之。
pub(crate) fn rand_hex8() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// 纯组装:时间与随机源由调用方注入,故可被测试逐字节钉住。
/// `None` = 文案被配空,这条回执不该存在(不写一条注定 no-op 的事件)。
///
/// 卡片 wire 在**入队这一刻**渲染并随事件定格:跨重启模板可能变,而回执描述的是入队那刻的事实。
fn compose_event(
    messaging: &Messaging,
    receipt: &ExitReceipt<'_>,
    now_ms: u64,
    at_hm: &str,
    id_suffix: &str,
) -> Option<OutboxEvent> {
    let text =
        messaging.render_session_exited_text(receipt.reason, receipt.chat_name, receipt.chat_id);
    if text.trim().is_empty() {
        return None;
    }
    let card = summon_core::feishu::card_session_exited(
        receipt.reason,
        &receipt.agent.name,
        &exit_chat_label(receipt.chat_name, receipt.chat_type, receipt.chat_id),
        at_hm,
        &text,
        receipt.session_created_at,
    );
    Some(summon_core::outbox::build_notify_event(
        now_ms,
        &receipt.agent.name,
        receipt.delivery_scope,
        receipt.chat_id,
        receipt.chat_type,
        receipt.creator,
        receipt.event_operator,
        receipt.reason,
        text,
        card,
        id_suffix,
    ))
}

/// 把一条退出回执**入队 outbox**。**不联网**——纯本地 tmp+rename,由 drain 线程负责投递。
///
/// 入队成功、且这个 reason 确实意味着「会话结束」(见 [`writes_exit_mark`])才写去重标记:
/// 没入队成功 = 没人负责这次退出,让事后探测仍能补一条。
/// 用 `enqueue` 而非 `enqueue_once`:事件 id 带随机 suffix,
/// 确定性幂等根本不会命中,「同一次退出只发一条」由 `exit_mark` 负责。
pub(crate) fn enqueue(
    store: &OutboxStore,
    messaging: &Messaging,
    receipt: &ExitReceipt<'_>,
) -> EnqueueOutcome {
    let Some(ev) = compose_event(
        messaging,
        receipt,
        crate::runtime::daemon::now_unix_ms(),
        &now_local_hm(),
        &rand_hex8(),
    ) else {
        return EnqueueOutcome::SkippedEmptyText;
    };
    match store.enqueue(&ev) {
        Ok(()) => {
            // 已入队 = 这次退出有人负责了(drain 会投),事后探测应让路。
            if writes_exit_mark(receipt.reason) {
                crate::session::exit_mark::mark(receipt.agent, receipt.chat_type, receipt.chat_id);
            }
            EnqueueOutcome::Enqueued
        }
        Err(e) => EnqueueOutcome::Failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::config::builtin_messaging;
    use summon_core::outbox::OutboxKind;

    const NOW_MS: u64 = 1_700_000_000_000;
    const AT: &str = "07-27 21:30";
    const HEX: &str = "deadbeef";

    fn receipt<'a>(
        agent: &'a Agent,
        chat_name: Option<&'a str>,
        reason: ExitReason,
    ) -> ExitReceipt<'a> {
        ExitReceipt {
            agent,
            delivery_scope: "test-abcd1234",
            chat_type: "group",
            chat_id: "oc_1",
            chat_name,
            creator: Some("ou_creator"),
            event_operator: None,
            reason,
            session_created_at: None,
        }
    }

    /// 重构前 daemon 侧 `enqueue_notify_chat` 的组装逐字节复刻(见 e650f19 teardown.rs:318-341)。
    /// 钉住「行为不变」:新入口产出的事件必须与它相等。
    fn legacy_daemon_event(messaging: &Messaging, r: &ExitReceipt<'_>) -> Option<OutboxEvent> {
        // 逐字节复刻旧实现的**函数体**;入参改从 receipt 取(旧签名是 10 个扁平参数)。
        let (agent, delivery_scope, chat_id, chat_type, chat_name, creator, event_operator, reason) = (
            r.agent,
            r.delivery_scope,
            r.chat_id,
            r.chat_type,
            r.chat_name,
            r.creator,
            r.event_operator,
            r.reason,
        );
        let text = messaging.render_session_exited_text(reason, chat_name, chat_id);
        if text.trim().is_empty() {
            return None;
        }
        let rendered_card = summon_core::feishu::card_session_exited(
            reason,
            &agent.name,
            &exit_chat_label(chat_name, chat_type, chat_id),
            AT,
            &text,
            None,
        );
        Some(summon_core::outbox::build_notify_event(
            NOW_MS,
            &agent.name,
            delivery_scope,
            chat_id,
            chat_type,
            creator,
            event_operator,
            reason,
            text,
            rendered_card,
            HEX,
        ))
    }

    /// 重构前 CLI 侧 `enqueue_exit_receipt` 的组装逐字节复刻(见 e650f19 cli.rs:3869-3903)。
    /// 注意 label 回退**不 trim 不判空**——这正是两侧唯一的行为差,由下方专门一条测试钉住。
    fn legacy_cli_event(messaging: &Messaging, r: &ExitReceipt<'_>) -> Option<OutboxEvent> {
        let (agent, delivery_scope, chat_id, chat_type, chat_name, creator) = (
            r.agent,
            r.delivery_scope,
            r.chat_id,
            r.chat_type,
            r.chat_name,
            r.creator,
        );
        let reason = ExitReason::ManualKill;
        let text = messaging.render_session_exited_text(reason, chat_name, chat_id);
        if text.trim().is_empty() {
            return None;
        }
        let fallback_label = chat_tag(chat_type, chat_id);
        let chat_label = chat_name.unwrap_or(&fallback_label);
        let card = summon_core::feishu::card_session_exited(
            reason,
            &agent.name,
            chat_label,
            AT,
            &text,
            None,
        );
        Some(summon_core::outbox::build_notify_event(
            NOW_MS,
            &agent.name,
            delivery_scope,
            chat_id,
            chat_type,
            creator,
            None,
            reason,
            text,
            card,
            HEX,
        ))
    }

    #[test]
    fn composes_byte_identical_event_to_legacy_daemon_assembly() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let m = builtin_messaging();
        for reason in [
            ExitReason::ManualKill,
            ExitReason::ManualKillReset,
            ExitReason::PanelClean,
            ExitReason::PanelReset,
            ExitReason::GroupDisbanded,
            ExitReason::BotRemoved,
            ExitReason::ShutdownInterrupt,
        ] {
            for chat_name in [None, Some("sample开发群2")] {
                let r = receipt(&agent, chat_name, reason);
                assert_eq!(
                    compose_event(&m, &r, NOW_MS, AT, HEX),
                    legacy_daemon_event(&m, &r),
                    "reason={reason:?} chat_name={chat_name:?} 组装结果必须与重构前一致"
                );
            }
        }
    }

    #[test]
    fn composes_byte_identical_event_to_legacy_cli_assembly() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let m = builtin_messaging();
        for chat_name in [None, Some("sample开发群2")] {
            let r = receipt(&agent, chat_name, ExitReason::ManualKill);
            assert_eq!(
                compose_event(&m, &r, NOW_MS, AT, HEX),
                legacy_cli_event(&m, &r),
                "chat_name={chat_name:?} 组装结果必须与重构前 CLI 一致"
            );
        }
    }

    #[test]
    fn blank_chat_name_falls_back_to_tag_like_daemon_side() {
        // 两侧唯一的行为差:旧 CLI 对空白群名原样上卡(会渲染出空的「会话」列),
        // 统一后按 daemon 语义回退到 chat tag。
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let m = builtin_messaging();
        let r = receipt(&agent, Some("   "), ExitReason::ManualKill);
        let ev = compose_event(&m, &r, NOW_MS, AT, HEX).expect("内置文案非空");
        let OutboxKind::NotifyChat { rendered_card, .. } = &ev.kind else {
            panic!("退出回执必须是 NotifyChat");
        };
        assert!(
            rendered_card.contains("card_close"),
            "新通知在冻结时包含关闭入口"
        );
        assert!(
            rendered_card.contains(&chat_tag("group", "oc_1")),
            "空白群名应回退 chat tag,实际卡片: {rendered_card}"
        );
    }

    #[test]
    fn skips_event_when_text_configured_empty() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let mut m = builtin_messaging();
        m.session_exited.manual_kill = String::new();
        let r = receipt(&agent, None, ExitReason::ManualKill);
        assert!(compose_event(&m, &r, NOW_MS, AT, HEX).is_none());
    }

    #[test]
    fn enqueue_writes_event_and_exit_mark() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let store_dir = tempfile::tempdir().expect("建临时 outbox 目录");
        let store = OutboxStore::new(store_dir.path());
        let m = builtin_messaging();
        let r = receipt(&agent, None, ExitReason::ManualKill);
        assert!(matches!(enqueue(&store, &m, &r), EnqueueOutcome::Enqueued));
        assert_eq!(store.pending_events().len(), 1);
        assert!(
            crate::session::exit_mark::exists(&agent, "group", "oc_1"),
            "真实退出必须写去重标记"
        );
    }

    #[test]
    fn only_shutdown_interrupt_skips_exit_mark() {
        assert!(!writes_exit_mark(ExitReason::ShutdownInterrupt));
        for reason in [
            ExitReason::ManualKill,
            ExitReason::PanelClean,
            ExitReason::PanelReset,
            ExitReason::GroupDisbanded,
            ExitReason::BotRemoved,
        ] {
            assert!(writes_exit_mark(reason), "{reason:?} 是真实退出,必须写标记");
        }
    }

    #[test]
    fn enqueue_leaves_no_mark_for_shutdown_interrupt() {
        // 停机中断不是「会话结束」:tmux 与 `.meta` 都还在,重启即续跑。写标记会让之后
        // 一次真实的 offline kill 回执被静默吞掉——续跑路径既不写 `.meta` 也不清标记,
        // 而清除钩子只有「新入站写 `.meta`」和「idle-scan 且仅 Transcript」两处。
        let (agent, _tmp) = crate::test_support::agent_dir("cursor", "");
        let store_dir = tempfile::tempdir().expect("建临时 outbox 目录");
        let store = OutboxStore::new(store_dir.path());
        let m = builtin_messaging();
        let r = receipt(&agent, None, ExitReason::ShutdownInterrupt);
        assert!(matches!(enqueue(&store, &m, &r), EnqueueOutcome::Enqueued));
        assert_eq!(store.pending_events().len(), 1, "回执本身照发");
        assert!(
            !crate::session::exit_mark::exists(&agent, "group", "oc_1"),
            "停机中断不该写退出去重标记"
        );
    }

    #[test]
    fn enqueue_skips_and_leaves_no_mark_when_text_empty() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let store_dir = tempfile::tempdir().expect("建临时 outbox 目录");
        let store = OutboxStore::new(store_dir.path());
        let mut m = builtin_messaging();
        m.session_exited.manual_kill = String::new();
        let r = receipt(&agent, None, ExitReason::ManualKill);
        assert!(matches!(
            enqueue(&store, &m, &r),
            EnqueueOutcome::SkippedEmptyText
        ));
        assert!(store.pending_events().is_empty());
        assert!(!crate::session::exit_mark::exists(&agent, "group", "oc_1"));
    }
}
