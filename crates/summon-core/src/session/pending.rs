//! 被打断/停机时未完成的无头一轮的持久化形状 + 重启重跑的纯决策。
//!
//! 优雅停机(或 pause+append)时,daemon 把正在跑的无头一轮落成 `threads/<tag>/pending.json`
//! (本模块的 [`PendingTurn`]);下次 daemon 启动,chat 线程起来前先看有没有 `pending.json`,
//! 有就**自动重跑**这一轮。怎么重跑(codex `resume <thread_id>` / cursor 有 sid 走 resume + 重喂
//! / cursor 无 sid 冷启)是逐 CLI 发散的**纯决策**,收在 [`resume_decision`],方便单测、不碰 IO。
//!
//! 这是纯逻辑(序列化形状 + 决策),不做任何文件读写——落盘/读取/删除的 IO 在 `summon` 侧。

use serde::{Deserialize, Serialize};

use crate::driver::CliDriver;

/// PendingTurn 的来源种类:决定停机时是否需要人可读的通知、是否落 pending.json 供 resume。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingTurnKind {
    /// 真飞书 chat(p2p/group/topic_group):停机时需要通知 owner、pending.json 供 chat 线程 resume。
    Chat,
    /// workflow worker 子任务:一次性无状态子任务,停机不需要通知、不 resume。
    Worker,
}

impl PendingTurnKind {
    /// 是否需要在停机时入队 NotifyChat 通知 owner(只有真 Chat 需要)。
    pub fn needs_shutdown_notify(self) -> bool {
        matches!(self, PendingTurnKind::Chat)
    }

    /// 是否需要落 pending.json 供 resume(只有 Chat;worker 是一次性子任务,重跑无意义)。
    pub fn persists_pending(self) -> bool {
        matches!(self, PendingTurnKind::Chat)
    }
}

/// 被打断/停机时未完成的一轮的快照,落成 `threads/<tag>/pending.json`。
///
/// 字段都是「重跑这一轮所需的最小信息」:哪个 chat、什么 CLI、原始 prompt(信封后的正文)、
/// 打断前已增量落盘的 session_id(可能没有——cursor 早打断时 sid 还没来),以及已流式累积的
/// 部分回答(cursor 重喂时用;codex 不需要,它自己保留了 `<turn_aborted>`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingTurn {
    /// 这轮的种类(真 chat / workflow worker),决定停机行为。
    #[serde(default = "default_kind")]
    pub kind: PendingTurnKind,
    /// 这轮创建时冻结的飞书投递身份。只对 Chat 有意义；Worker 保持空串。
    /// 旧 schema 缺失时默认空，恢复端必须 fail-closed，不能把旧 app 的轮猜成当前 app。
    #[serde(default)]
    pub delivery_scope: String,
    /// 创建这轮的 agent。checkpoint 位于 agent 目录仍不够：目录被复制或配置改名时也必须
    /// fail-closed，不能把旧轮猜给另一个身份。
    #[serde(default)]
    pub agent_name: String,
    /// 这轮已经 durable `started` 的 Inbox 消息全集（首条消息 + 所有并入的 steer/`!` followup）。
    /// 自动续跑与 Inbox ACK 都以该集合做精确提交；旧 schema 默认为空，禁止自动续跑。
    #[serde(default)]
    pub inbox_message_ids: Vec<String>,
    /// 只有 daemon 优雅停机明确接管过的 checkpoint 才能自动续跑。运行中落盘的普通 checkpoint
    /// 保持 false：若正常终态删文件失败，下次启动也不会把它误当成待续跑工作。
    #[serde(default)]
    pub resume_authorized: bool,
    /// 飞书 chat_id(配 chat_type 还原 chat tag / 重投路由)。Worker 种类下存的是
    /// TurnKey.tag(如 "worker-{parent}-{idx}"),仅供调试用。
    pub chat_id: String,
    /// chat 类型(p2p / group / topic_group),还原 tag 与会话路由用。
    /// Worker 种类下无意义(不用于路由),保留为兼容字段。
    #[serde(default)]
    pub chat_type: String,
    /// 跑这一轮的 CLI kind 字符串(`cursor` / `codex`),与 `resume_store` 的 cli guard 同义。
    pub cli: String,
    /// 被打断那轮喂进 CLI 的 prompt(信封后的正文;冷启轮可能含 PRIMER 前缀,这里只存正文即可,
    /// 重跑时由上层按冷启/resume 决定要不要再前置 PRIMER)。
    pub prompt: String,
    /// 打断前已增量落盘的 session_id(None = 打断太早还没拿到,cursor 退化为冷启)。
    pub session_id: Option<String>,
    /// 打断前已流式累积的部分回答(可能空);cursor 重喂时拼进 prompt,codex 用不到。
    pub partial: String,
}

impl PendingTurn {
    /// 只有优雅停机授权、且完整属于当前 agent/chat/飞书 epoch 的真 Chat 轮才允许自动恢复。
    /// Inbox 记录的 started 集合还要由 IO 层精确核验；这里只验证 checkpoint 自身身份。
    pub fn resumable_in_context(
        &self,
        current_scope: &str,
        agent_name: &str,
        chat_id: &str,
        chat_type: &str,
    ) -> bool {
        self.kind == PendingTurnKind::Chat
            && self.resume_authorized
            && !self.delivery_scope.is_empty()
            && self.delivery_scope == current_scope
            && !self.agent_name.is_empty()
            && self.agent_name == agent_name
            && self.chat_id == chat_id
            && self.chat_type == chat_type
            && !self.inbox_message_ids.is_empty()
            && self
                .inbox_message_ids
                .iter()
                .all(|message_id| !message_id.trim().is_empty())
            && self
                .inbox_message_ids
                .iter()
                .enumerate()
                .all(|(index, id)| !self.inbox_message_ids[..index].contains(id))
    }
}

fn default_kind() -> PendingTurnKind {
    PendingTurnKind::Chat
}

/// 重启时如何重跑一个 [`PendingTurn`] 的纯决策(不含 IO、不拼最终 prompt 文本——文本合并在
/// `followup`/上层做,这里只决定 resume 还是冷启、要不要重喂 partial)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeDecision {
    /// codex:CLI 自己保留了打断上下文(`<turn_aborted>` + 部分输出),直接 `resume <thread_id>`
    /// 重发原 prompt 即可,**不必**重喂 partial。要求有 thread_id(codex 第 1 行就给)。
    CodexResume,
    /// cursor:有 sid → resume 轮,但 CLI **不保留**打断内容,summon 必须把原 prompt(+ partial)
    /// 重喂进去(`restate=true`)。
    CursorResume,
    /// cursor:无 sid(打断太早)→ 只能冷启一轮,prompt 仍带上原 prompt(+ partial),且会重注
    /// PRIMER(冷启语义,由上层处理)。
    CursorColdStart,
    /// 无法重跑(未知 CLI / 形态不匹配):上层应丢弃 pending、顶多发一句「上轮被中断」。
    Drop,
}

impl ResumeDecision {
    // 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
    #[cfg(test)]
    /// 是否走 resume(否则冷启)。
    pub(crate) fn is_resume(self) -> bool {
        matches!(
            self,
            ResumeDecision::CodexResume | ResumeDecision::CursorResume
        )
    }

    /// 重跑时是否需要 summon 自己重喂原 prompt+partial(cursor 两种情形都要;codex 不要)。
    pub fn restates_original(self) -> bool {
        matches!(
            self,
            ResumeDecision::CursorResume | ResumeDecision::CursorColdStart
        )
    }
}

/// 纯决策:给定 CLI 与是否拿到了 sid,决定如何重跑被打断的一轮。
///
/// - codex:有 sid(=thread_id)→ `CodexResume`;无 sid → codex 第 1 行就给 thread_id,理论上
///   不该缺,缺了说明打断早到 thread.started 之前 → 退化为 `Drop`(无 resume 锚点,codex 冷启重跑
///   会丢上下文且本设计不为 codex 做冷启重喂——交给上层「上轮被中断」提示)。
/// - cursor:有 sid → `CursorResume`(resume + 重喂);无 sid → `CursorColdStart`(冷启 + 重喂)。
/// - 其他:`Drop`。
pub fn resume_decision(cli: CliDriver, has_session_id: bool) -> ResumeDecision {
    match cli {
        CliDriver::Codex => {
            if has_session_id {
                ResumeDecision::CodexResume
            } else {
                ResumeDecision::Drop
            }
        }
        CliDriver::Cursor => {
            if has_session_id {
                ResumeDecision::CursorResume
            } else {
                ResumeDecision::CursorColdStart
            }
        }
        // claude(Transcript)本轮不走 headless pending;Other 无捕获路径。
        _ => ResumeDecision::Drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_turn_round_trip_without_session_id() {
        // cursor 早打断:没 sid,partial 也可能空 —— 仍须完整 round-trip。
        let p = PendingTurn {
            kind: PendingTurnKind::Chat,
            delivery_scope: "tenant-app".into(),
            agent_name: "coder".into(),
            inbox_message_ids: vec!["om_1".into()],
            resume_authorized: true,
            chat_id: "ou_1".into(),
            chat_type: "p2p".into(),
            cli: "cursor".into(),
            prompt: "写个函数".into(),
            session_id: None,
            partial: String::new(),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: PendingTurn = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
        assert!(back.session_id.is_none());
    }

    #[test]
    fn pending_turn_kind_defaults_to_chat() {
        // 旧 pending.json(无 kind 字段)应反序列化为 Chat(向后兼容)。
        let legacy = r#"{"chat_id":"oc_x","chat_type":"p2p","cli":"cursor","prompt":"hi","session_id":null,"partial":""}"#;
        let p: PendingTurn = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.kind, PendingTurnKind::Chat);
        assert!(p.delivery_scope.is_empty());
        assert!(p.agent_name.is_empty());
        assert!(p.inbox_message_ids.is_empty());
        assert!(!p.resume_authorized);
        assert!(
            !p.resumable_in_context("tenant-app", "coder", "oc_x", "p2p"),
            "旧 checkpoint 缺失 scope 时不得猜成当前 app"
        );
    }

    #[test]
    fn only_exact_chat_scope_is_resumable() {
        let mut p = PendingTurn {
            kind: PendingTurnKind::Chat,
            delivery_scope: "app-old".into(),
            agent_name: "coder".into(),
            inbox_message_ids: vec!["om_1".into()],
            resume_authorized: true,
            chat_id: "oc_x".into(),
            chat_type: "p2p".into(),
            cli: "cursor".into(),
            prompt: "hi".into(),
            session_id: None,
            partial: String::new(),
        };
        assert!(p.resumable_in_context("app-old", "coder", "oc_x", "p2p"));
        assert!(!p.resumable_in_context("app-new", "coder", "oc_x", "p2p"));
        assert!(!p.resumable_in_context("app-old", "other", "oc_x", "p2p"));
        p.delivery_scope.clear();
        assert!(!p.resumable_in_context("app-old", "coder", "oc_x", "p2p"));
        p.delivery_scope = "app-old".into();
        p.kind = PendingTurnKind::Worker;
        assert!(!p.resumable_in_context("app-old", "coder", "oc_x", "p2p"));
    }

    #[test]
    fn resume_requires_shutdown_authorization_and_unique_inbox_ids() {
        let mut p = PendingTurn {
            kind: PendingTurnKind::Chat,
            delivery_scope: "app".into(),
            agent_name: "coder".into(),
            inbox_message_ids: vec!["om_1".into()],
            resume_authorized: false,
            chat_id: "oc_x".into(),
            chat_type: "p2p".into(),
            cli: "codex".into(),
            prompt: "hi".into(),
            session_id: Some("th_1".into()),
            partial: String::new(),
        };
        assert!(!p.resumable_in_context("app", "coder", "oc_x", "p2p"));
        p.resume_authorized = true;
        assert!(p.resumable_in_context("app", "coder", "oc_x", "p2p"));
        p.inbox_message_ids.push("om_1".into());
        assert!(!p.resumable_in_context("app", "coder", "oc_x", "p2p"));
    }

    #[test]
    fn worker_kind_skips_notify_and_persist() {
        assert!(PendingTurnKind::Chat.needs_shutdown_notify());
        assert!(PendingTurnKind::Chat.persists_pending());
        assert!(!PendingTurnKind::Worker.needs_shutdown_notify());
        assert!(!PendingTurnKind::Worker.persists_pending());
    }

    #[test]
    fn codex_with_sid_resumes_without_restate() {
        let d = resume_decision(CliDriver::Codex, true);
        assert_eq!(d, ResumeDecision::CodexResume);
        assert!(d.is_resume());
        assert!(!d.restates_original(), "codex 自己保留上下文,不重喂");
    }

    #[test]
    fn codex_without_sid_drops() {
        // thread_id 第 1 行就来,缺了说明打断早到 thread.started 之前:无 resume 锚点 → Drop。
        assert_eq!(
            resume_decision(CliDriver::Codex, false),
            ResumeDecision::Drop
        );
    }

    #[test]
    fn cursor_with_sid_resumes_and_restates() {
        let d = resume_decision(CliDriver::Cursor, true);
        assert_eq!(d, ResumeDecision::CursorResume);
        assert!(d.is_resume());
        assert!(
            d.restates_original(),
            "cursor 不保留打断内容,summon 必须重喂"
        );
    }

    #[test]
    fn cursor_without_sid_cold_starts_and_restates() {
        let d = resume_decision(CliDriver::Cursor, false);
        assert_eq!(d, ResumeDecision::CursorColdStart);
        assert!(!d.is_resume(), "无 sid 只能冷启");
        assert!(d.restates_original());
    }

    #[test]
    fn claude_and_other_drop() {
        assert_eq!(
            resume_decision(CliDriver::Claude, true),
            ResumeDecision::Drop
        );
    }
}
