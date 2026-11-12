//! 会话退出的纯逻辑:退出原因枚举、通知目标回退链。
//!
//! IO(真发飞书消息)在 `summon` 层的 `notify_session_exited`;这里只放可单测的判定:
//! - `ExitReason`:谁触发的退出(决定文案 + 群在不在的预判)。
//! - `resolve_notify_target`:群主 open_id 的回退链(.meta creator → 事件 operator → 配置 owner)。
//!
//! 文案模板走 messaging 配置(`config::Messaging::session_exited`),不在代码里 hardcode。

use serde::{Deserialize, Serialize};

/// 会话被强制退出的原因。决定回执文案,且 `group_gone()` 预判 bot 是否还在群里。
///
/// 加 serde:outbox 事件要把 reason 持久化进 `run/outbox/*.json`(选文案 + 选 group_gone
/// 分支),沿用既有纯枚举、不另造 wire 类型,避免双类型漂移。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// `agents kill` / `agents down`(不带 `--reset`),以及**探测到 tmux 被外部销毁**时的兜底合成
    /// (如用户在会话里敲 `/quit`)。这两种情况都**不清 resume 记录**,下次发消息续接原上下文。
    ManualKill,
    /// `agents kill --reset`:额外清掉 resume 记录,下次是全新会话。
    ///
    /// **why 与 `ManualKill` 分开**:此前两者共用一条文案「下次给我发消息会开一段新的对话」,而
    /// `resume_store::clear_chat` 只在 `--reset` 时调用 —— 不带 reset 的 kill 实际是续上下文的,
    /// 回执把它说成「新对话」是错的(实测)。文案必须跟着真实行为分叉。
    ManualKillReset,
    /// 面板「退出(归档续聊)」。
    PanelClean,
    /// 面板「退出(唤醒时新建)」。
    PanelReset,
    /// 空闲扫描触发的自动退出(归档续聊)。行为与 `PanelClean` 完全一致(留 resume 记录),
    /// **分开只为文案**:用户没点任何按钮,回执必须说清「为什么它自己退了、上下文还在不在」,
    /// 否则看起来像会话莫名其妙丢了。
    IdleRetired,
    /// agent 判断话题已经结束,本轮完成后自行归档(`summon session end`)。行为与 `PanelClean`
    /// 一致(留 resume 记录),分开是因为发起人既不是用户也不是空闲扫描。
    AgentArchived,
    /// 飞书「群解散」事件。
    GroupDisbanded,
    /// 飞书「bot 被移出群」事件。
    BotRemoved,
    /// daemon 重启停机时打断了在跑的无头一轮(bot 仍在群里,非群散)。
    ShutdownInterrupt,
}

/// `agents kill` 的退出原因:唯一区别就是**清没清 resume 记录**。抽成一处,避免各 kill 站点
/// 各自 `if reset {..} else {..}` 漂移(实测:此前三处站点全写死 `ManualKill`,文案与真实行为不符)。
pub fn manual_kill_reason(reset: bool) -> ExitReason {
    if reset {
        ExitReason::ManualKillReset
    } else {
        ExitReason::ManualKill
    }
}

impl ExitReason {
    /// bot 是否已确定不在原 chat 里。群解散 / bot 被移除 ⇒ 发不进原 chat,只能 p2p 群主;
    /// 其余(手动 / 面板 / 停机打断)bot 仍在,先试发原 chat、失败再回退 p2p。
    pub fn group_gone(self) -> bool {
        matches!(self, ExitReason::GroupDisbanded | ExitReason::BotRemoved)
    }
}

/// 通知投递目标:发原 chat,还是按 open_id 私聊某人(群没了时通知群主)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyTarget {
    /// 发进原 chat(群仍在 / 私聊本身)。
    Chat,
    /// 按 open_id 私聊(群已散,通知群主)。
    OpenId(String),
}

/// 解析「群已不在」时的 p2p 通知目标:.meta 记的 creator → 事件 operator → 配置 owner。
/// 三者皆空 → None(诚实放弃,调用方只 log、不 panic、不瞎发)。
/// 入参均为「空串视作无」,便于直接喂 `.meta`/事件里可能缺失的字段。
pub fn resolve_notify_target(
    creator: Option<&str>,
    event_operator: Option<&str>,
    owner: Option<&str>,
) -> Option<NotifyTarget> {
    [creator, event_operator, owner]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(|s| NotifyTarget::OpenId(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_gone_only_for_disband_and_bot_removed() {
        assert!(ExitReason::GroupDisbanded.group_gone());
        assert!(ExitReason::BotRemoved.group_gone());
        assert!(!ExitReason::ManualKill.group_gone());
        assert!(!ExitReason::PanelClean.group_gone());
        assert!(!ExitReason::PanelReset.group_gone());
        // 停机打断:bot 仍在群,先发原 chat。
        assert!(!ExitReason::ShutdownInterrupt.group_gone());
    }

    #[test]
    fn exit_reason_json_round_trip_snake_case() {
        // outbox 持久化 reason,确认各 variant snake_case 往返稳定(向后兼容文件)。
        for (r, wire) in [
            (ExitReason::ManualKill, "\"manual_kill\""),
            (ExitReason::PanelClean, "\"panel_clean\""),
            (ExitReason::PanelReset, "\"panel_reset\""),
            (ExitReason::AgentArchived, "\"agent_archived\""),
            (ExitReason::GroupDisbanded, "\"group_disbanded\""),
            (ExitReason::BotRemoved, "\"bot_removed\""),
            (ExitReason::ShutdownInterrupt, "\"shutdown_interrupt\""),
        ] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(s, wire);
            let back: ExitReason = serde_json::from_str(&s).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn notify_target_falls_back_creator_then_operator_then_owner() {
        // creator 优先
        assert_eq!(
            resolve_notify_target(Some("ou_creator"), Some("ou_op"), Some("ou_owner")),
            Some(NotifyTarget::OpenId("ou_creator".to_string()))
        );
        // creator 缺 → 事件 operator
        assert_eq!(
            resolve_notify_target(None, Some("ou_op"), Some("ou_owner")),
            Some(NotifyTarget::OpenId("ou_op".to_string()))
        );
        // 前两者缺 → 配置 owner
        assert_eq!(
            resolve_notify_target(None, None, Some("ou_owner")),
            Some(NotifyTarget::OpenId("ou_owner".to_string()))
        );
    }

    #[test]
    fn notify_target_treats_empty_as_missing() {
        // 空串 / 纯空白视作无,继续回退
        assert_eq!(
            resolve_notify_target(Some("  "), Some(""), Some("ou_owner")),
            Some(NotifyTarget::OpenId("ou_owner".to_string()))
        );
    }

    #[test]
    fn notify_target_all_empty_is_none() {
        assert_eq!(resolve_notify_target(None, None, None), None);
        assert_eq!(resolve_notify_target(Some(""), Some("  "), Some("")), None);
    }
}
