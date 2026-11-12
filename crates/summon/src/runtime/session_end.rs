//! agent 登记的「本轮完成后退出这条会话」意图。
//!
//! `summon session end` 是 agent 的子进程,看不到 daemon 的内存,所以意图落成按会话分开的
//! 文件;chat 线程在本轮销账后取走它。意图绑定发起它的那一轮的触发消息:轮末只认同一条消息
//! 的意图,别的轮留下的(崩溃、失败没取走的)一律作废,不会让后来的某一轮莫名退出。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::runtime::fsio::atomic_write;

fn path(agent_dir: &Path, conversation_tag: &str) -> PathBuf {
    let name: String = conversation_tag
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    agent_dir.join("runtime").join("session-end").join(name)
}

/// 登记:触发消息为 `message_id` 的这一轮完成后退出会话。
pub(crate) fn request(agent_dir: &Path, conversation_tag: &str, message_id: &str) -> Result<()> {
    let path = path(agent_dir, conversation_tag);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| summon_i18n::format!("r02e6404994"))?;
    }
    atomic_write(&path, message_id.as_bytes()).with_context(|| summon_i18n::format!("r31492d6592"))
}

/// 取走意图并判断它是否属于触发消息为 `message_id` 的这一轮。不管属不属于都删掉:一条会话
/// 同时只有一轮在跑,轮末还没被取走的意图只可能是这一轮的,或者早已作废。
pub(crate) fn take(agent_dir: &Path, conversation_tag: &str, message_id: &str) -> bool {
    let path = path(agent_dir, conversation_tag);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    let _ = std::fs::remove_file(&path);
    raw.trim() == message_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_intent_belongs_only_to_the_turn_that_registered_it() {
        let dir = tempfile::tempdir().unwrap();
        let tag = "g00000000a11ce02a";
        assert!(!take(dir.path(), tag, "om_1"), "没登记就不退出");

        request(dir.path(), tag, "om_1").unwrap();
        assert!(!take(dir.path(), "g_other", "om_1"), "按会话分开");
        assert!(take(dir.path(), tag, "om_1"));
        assert!(!take(dir.path(), tag, "om_1"), "取走即删");

        request(dir.path(), tag, "om_1").unwrap();
        assert!(!take(dir.path(), tag, "om_2"), "别的轮留下的意图作废");
        assert!(!take(dir.path(), tag, "om_1"), "作废的意图也已删掉");
    }
}
