//! 退出回执的去重标记。语义:**本次退出的回执已经有人负责了**。
//!
//! 八条回执产出路径(面板 Clean/Reset、群解散、KillTeardown、idle-scan 探测、离线归档、
//! 停机打断、CLI 入队)两两撞在同一次退出上就是双发。判据必须落在盘上:drain 周期 2s
//! 远快于 idle-scan,靠时序推断必然失效;outbox 的 `has_unresolved_local_chat` 也只能看到
//! **尚未投递**的事件,回执早已 ack 时查重必然落空。
//!
//! 独立文件而非 `.meta` 字段:`.meta` 每条入站全量重写,字段会被覆盖(`.cwd` 当初分开放同理)。
//!
//! 键用 **chat 维度**而非 session 维度:回执是发给 chat 的,一个 chat 只该有一条;而历史遗留
//! 会让同一 chat 对应多个 session 名(裸名 + tag 名,见 `gone_session_names`),按 session 记
//! 会漏判成双发。

use std::path::PathBuf;
use summon_core::registry::{chat_tag, Agent};

fn mark_path(agent: &Agent, chat_type: &str, chat_id: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{}.exit-notified", chat_tag(chat_type, chat_id)))
}

/// 记下「回执已负责」。best-effort:写失败只影响去重(退化为可能双发),绝不能中断退出流程。
pub(crate) fn mark(agent: &Agent, chat_type: &str, chat_id: &str) {
    let path = mark_path(agent, chat_type, chat_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, b"") {
        crate::observe::log::log(&summon_i18n::format!("r5411cb85af", agent.name));
    }
}

pub(crate) fn exists(agent: &Agent, chat_type: &str, chat_id: &str) -> bool {
    mark_path(agent, chat_type, chat_id).exists()
}

/// 清标记。会话重新开始 = 新的生命周期,旧标记必须让位,否则同一 chat 第二次 kill 会被静默吞掉。
pub(crate) fn clear(agent: &Agent, chat_type: &str, chat_id: &str) {
    let _ = std::fs::remove_file(mark_path(agent, chat_type, chat_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_then_exists_then_clear_roundtrips() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        assert!(!exists(&agent, "group", "oc_1"), "初始无标记");
        mark(&agent, "group", "oc_1");
        assert!(exists(&agent, "group", "oc_1"), "写后应存在");
        clear(&agent, "group", "oc_1");
        assert!(!exists(&agent, "group", "oc_1"), "清后应消失");
    }

    #[test]
    fn mark_is_keyed_by_chat_not_session() {
        // 同一 chat 在历史上可能对应多个 session 名(裸名 + tag 名)。标记按 chat 记,
        // 任一 session 视角查到的都是同一条,否则会漏判成双发。
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        mark(&agent, "group", "oc_1");
        assert!(exists(&agent, "group", "oc_1"));
        assert!(!exists(&agent, "group", "oc_2"), "不同 chat 不应串");
    }

    #[test]
    fn clear_on_missing_mark_is_noop() {
        // 清一个不存在的标记不能 panic —— 冷启路径会无条件调它。
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        clear(&agent, "group", "oc_never");
    }

    #[test]
    fn mark_survives_when_runtime_chats_dir_absent() {
        // 会话从未冷启过(runtime/chats 还不存在)时 kill,标记仍要写得下去——
        // 否则 idle-scan 探测不到「已有人负责」,会重复发一条。
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        assert!(!agent.state_dir().join("runtime").join("chats").exists());
        mark(&agent, "p2p", "oc_fresh");
        assert!(exists(&agent, "p2p", "oc_fresh"));
    }
}
