//! 优雅重启后重新接管一条在跑的 Transcript(claude)轮所需的落盘锚点。
//!
//! claude 的 tmux 会话是持久层,跨 daemon 重启活着,轮也就跟着继续跑——但接住它输出的
//! chat 线程随 daemon 一起没了。本模块的 [`TurnAnchor`] 就是无头轮 `pending.json` 的
//! Transcript 形态:落成 `threads/<tag>/turn_anchor.json`,记下「这一轮是谁的、注入了哪个
//! marker、从转录哪个字节往后找它」,重启后据此接管而**不重新注入**(重注入会让同一句话跑两遍)。
//!
//! 纯 schema + 路径计算 + 三条校验判据,不做 IO——落盘/读取/删除在 `summon` 侧的 `inflight.rs`。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 接管时要续刷的那条原生 CoT 消息。缺失只降级为新起一条,不影响接管本身。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAnchorCot {
    pub cot_id: String,
    pub message_id: String,
}

/// 原轮已经收到的 CLI 凭证。只能在接管证据和当前权限复验后激活。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAnchorCredential {
    pub turn_id: String,
    pub message_id: String,
    pub requester_union_id: String,
    pub started_at_ms: i64,
}

/// 一条在跑的 Transcript 轮的接管锚点。
///
/// 前四个身份字段与 [`crate::session::pending::PendingTurn`] 同义:接管跨的是 daemon 重启,
/// 期间 agent 可能被改名、飞书 app 可能被换掉、目录可能被复制,所以身份必须逐字节复验,
/// 证不了就不接管(退回人工恢复卡),而不是猜。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAnchor {
    pub agent_name: String,
    pub delivery_scope: String,
    pub chat_id: String,
    pub chat_type: String,
    pub inbox_message_ids: Vec<String>,
    pub native_session_id: String,
    pub marker: String,
    /// 注入那一刻算出的 prompt 开头(`prompt_head`)。接管时用它复核「消息有没有被 CLI 截断」——
    /// 判据必须是**当时那份正文**:接管方手里的 prompt 是从入站消息重建的,可能带时间戳之类的
    /// 差异,拿它算 head 会把一条完好的消息误报成截断。空串 = 只认 marker(旧锚点没有这一行)。
    #[serde(default)]
    pub prompt_head: String,
    /// 注入前冻结的转录文件长度(字节)。找消费记录从这里往后扫。
    pub inject_offset: u64,
    /// exact marker 消费记录右侧的绝对字节边界;`None` = 停机前还没观测到消费。
    #[serde(default)]
    pub consumed_boundary: Option<u64>,
    pub injected_at_ms: u64,
    #[serde(default)]
    pub cot: Option<TurnAnchorCot>,
    /// 与 exact marker 同时冻结；旧锚点仍可接输出，但不能恢复 CLI 权限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<TurnAnchorCredential>,
}

impl TurnAnchor {
    /// 与 `PendingTurn::resumable_in_context` 同款:scope/agent/chat 逐字节相同才可接管。
    /// 空串一律不算相等——旧格式或半写文件的空字段不能被当成「碰巧匹配上了」。
    pub fn reattachable_in_context(
        &self,
        scope: &str,
        agent: &str,
        chat_id: &str,
        chat_type: &str,
    ) -> bool {
        !self.delivery_scope.is_empty()
            && self.delivery_scope == scope
            && !self.agent_name.is_empty()
            && self.agent_name == agent
            && !self.chat_id.is_empty()
            && self.chat_id == chat_id
            && !self.chat_type.is_empty()
            && self.chat_type == chat_type
    }

    /// 盘上 `.session` 的原生 sid 与锚点相同。
    /// sid 变了说明这条 tmux 会话已经不是当轮那条(被 `/clear`、被重置、或换了会话),
    /// 接管进去等于往另一条上下文里贴输出。
    pub fn session_matches(&self, on_disk_sid: Option<&str>) -> bool {
        on_disk_sid == Some(self.native_session_id.as_str())
    }

    /// 这一轮能不能真的被接管:身份四元组逐字节相同、盘上 sid 与锚点同一条、锚点确实认领过
    /// 这条入站消息 —— 三条缺一都只能退人工恢复卡。
    ///
    /// 判松的代价不是「少接一轮」而是**双执行**:身份认错就把上一代的输出贴进另一条上下文,
    /// sid 认错就等于往一条已经换过代的会话里接输出。空 ids / 空 message_id 一律判假:
    /// 与 [`Self::reattachable_in_context`] 同一条规矩,空字段不能被当成「碰巧匹配上了」。
    pub fn reattachable_for(
        &self,
        scope: &str,
        agent: &str,
        chat_id: &str,
        chat_type: &str,
        on_disk_sid: Option<&str>,
        message_id: &str,
    ) -> bool {
        self.reattachable_in_context(scope, agent, chat_id, chat_type)
            && self.session_matches(on_disk_sid)
            && !self.inbox_message_ids.is_empty()
            && !message_id.is_empty()
            && self.inbox_message_ids.iter().any(|id| id == message_id)
    }

    /// 停机时刻距今 ≤ max_staleness_secs(时钟回拨按不新鲜)。
    /// `stopped_at_secs == 0` 表示拿不到可信的停机时刻(旧格式/未记录),此时无从判断这轮
    /// 停了多久,fail-closed 当作不新鲜。
    pub fn fresh_at(&self, stopped_at_secs: u64, now_secs: u64, max_staleness_secs: u64) -> bool {
        if stopped_at_secs == 0 || now_secs < stopped_at_secs {
            return false;
        }
        now_secs - stopped_at_secs <= max_staleness_secs
    }
}

/// `threads/<tag>/turn_anchor.json` 路径。
pub fn turn_anchor_path(base: &Path, tag: &str) -> PathBuf {
    base.join("threads").join(tag).join("turn_anchor.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor() -> TurnAnchor {
        TurnAnchor {
            agent_name: "a".into(),
            delivery_scope: "scope-1".into(),
            chat_id: "oc_1".into(),
            chat_type: "group".into(),
            inbox_message_ids: vec!["om_1".into()],
            native_session_id: "sid-1".into(),
            marker: "mk-1".into(),
            prompt_head: "上一代那条 prompt 的开头".into(),
            inject_offset: 100,
            consumed_boundary: None,
            injected_at_ms: 1_000,
            cot: None,
            credential: None,
        }
    }

    #[test]
    fn turn_anchor_preserves_the_original_cli_credential() {
        let mut raw = serde_json::to_value(anchor()).unwrap();
        let credential = serde_json::json!({
            "turn_id": "original-turn",
            "message_id": "om_1",
            "requester_union_id": "on_original",
            "started_at_ms": 900
        });
        raw["credential"] = credential.clone();
        let parsed: TurnAnchor = serde_json::from_value(raw).unwrap();
        assert_eq!(
            serde_json::to_value(parsed).unwrap()["credential"],
            credential
        );
    }

    #[test]
    fn old_anchor_without_optional_fields_still_parses() {
        let raw = r#"{"agent_name":"a","delivery_scope":"s","chat_id":"c","chat_type":"group","inbox_message_ids":["om_1"],"native_session_id":"sid","marker":"mk","inject_offset":1,"injected_at_ms":2}"#;
        let a: TurnAnchor = serde_json::from_str(raw).unwrap();
        assert_eq!(a.consumed_boundary, None);
        assert!(a.cot.is_none());
        // 旧锚点没有 head:接管时退化成只认 marker(随机 UUID 本身就唯一),不因此拒绝接管。
        assert!(a.prompt_head.is_empty());
    }

    #[test]
    fn reattachable_requires_exact_identity() {
        let a = anchor();
        assert!(a.reattachable_in_context("scope-1", "a", "oc_1", "group"));
        assert!(!a.reattachable_in_context("scope-2", "a", "oc_1", "group"));
        assert!(!a.reattachable_in_context("scope-1", "b", "oc_1", "group"));
        assert!(!a.reattachable_in_context("scope-1", "a", "oc_2", "group"));
        assert!(!a.reattachable_in_context("scope-1", "a", "oc_1", "p2p"));
        assert!(!TurnAnchor {
            delivery_scope: String::new(),
            ..anchor()
        }
        .reattachable_in_context("", "a", "oc_1", "group"));
    }

    /// 接管的三条前置逐条复验,少证一条就退恢复卡。
    #[test]
    fn reattachable_for_requires_identity_session_and_this_message() {
        let a = anchor();
        assert!(a.reattachable_for("scope-1", "a", "oc_1", "group", Some("sid-1"), "om_1"));
        assert!(
            !a.reattachable_for("scope-2", "a", "oc_1", "group", Some("sid-1"), "om_1"),
            "飞书身份换了就不是同一条投递上下文"
        );
        assert!(
            !a.reattachable_for("scope-1", "a", "oc_1", "group", Some("sid-2"), "om_1"),
            "sid 变了说明这条 tmux 会话已经不是当轮那条"
        );
        assert!(
            !a.reattachable_for("scope-1", "a", "oc_1", "group", None, "om_1"),
            "盘上读不到 sid 就证不了会话身份"
        );
        assert!(
            !a.reattachable_for("scope-1", "a", "oc_1", "group", Some("sid-1"), "om_9"),
            "这条消息不在锚点名下,那是另一轮"
        );
        assert!(
            !a.reattachable_for("scope-1", "a", "oc_1", "group", Some("sid-1"), ""),
            "空 message_id 不能被当成匹配"
        );
        assert!(
            !TurnAnchor {
                inbox_message_ids: Vec::new(),
                ..anchor()
            }
            .reattachable_for("scope-1", "a", "oc_1", "group", Some("sid-1"), "om_1"),
            "锚点没认领过任何消息就无从证明这一轮归谁"
        );
    }

    #[test]
    fn session_must_match_on_disk_sid() {
        let a = anchor();
        assert!(a.session_matches(Some("sid-1")));
        assert!(!a.session_matches(Some("sid-2")));
        assert!(!a.session_matches(None));
    }

    #[test]
    fn freshness_rejects_stale_and_clock_rollback() {
        let a = anchor();
        assert!(a.fresh_at(1_000, 1_000 + 1_800, 1_800));
        assert!(!a.fresh_at(1_000, 1_000 + 1_801, 1_800));
        assert!(!a.fresh_at(2_000, 1_000, 1_800));
        assert!(!a.fresh_at(0, 1_000, 1_800));
    }

    #[test]
    fn anchor_path_under_threads_tag() {
        assert_eq!(
            turn_anchor_path(Path::new("/tmp/agent"), "g1a2b3c4d"),
            PathBuf::from("/tmp/agent/threads/g1a2b3c4d/turn_anchor.json")
        );
    }
}
