//! 每条会话连续多少轮由定时器触发、其间没有人类消息。
//!
//! agent 可以在定时轮里再给自己排下一步(自续),但用户一直不回时必须停下:daemon 在定时轮
//! 开跑时加一、真人消息开跑时清零,`summon remind` 在定时轮里据它判断还能不能再建定时器。
//! 文件是 daemon 与 CLI 之间的接缝:CLI 是 agent 的子进程,看不到 daemon 的内存。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::runtime::fsio::atomic_write;

/// 连续这么多轮自续都没等到人类消息,自续暂停:被拒的那一轮直接收尾。
pub(crate) const MAX_UNANSWERED_FOLLOWUPS: u32 = 3;

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
    agent_dir.join("runtime").join("self-followups").join(name)
}

/// 已连续跑了多少轮没有人类消息的定时轮。读不出按 0:计数只用来拦自续,读坏了宁可多放行
/// 一次,也不能因为一个坏文件让会话永远不能自续。
pub(crate) fn unanswered(agent_dir: &Path, conversation_tag: &str) -> u32 {
    std::fs::read_to_string(path(agent_dir, conversation_tag))
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(0)
}

/// 定时器触发的一轮开跑。
pub(crate) fn note_scheduled_turn(agent_dir: &Path, conversation_tag: &str) -> Result<()> {
    let path = path(agent_dir, conversation_tag);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| summon_i18n::format!("rfd5d31dbdb"))?;
    }
    let next = unanswered(agent_dir, conversation_tag).saturating_add(1);
    atomic_write(&path, next.to_string().as_bytes())
        .with_context(|| summon_i18n::format!("r6facb5569e"))
}

/// 人类消息开跑:自续重新可用。
pub(crate) fn note_human_turn(agent_dir: &Path, conversation_tag: &str) -> Result<()> {
    let path = path(agent_dir, conversation_tag);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| summon_i18n::format!("r9ac1b02abf")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_turns_count_up_and_a_human_turn_resets() {
        let dir = tempfile::tempdir().unwrap();
        let tag = "g00000000a11ce02a";
        assert_eq!(unanswered(dir.path(), tag), 0);
        note_scheduled_turn(dir.path(), tag).unwrap();
        note_scheduled_turn(dir.path(), tag).unwrap();
        assert_eq!(unanswered(dir.path(), tag), 2);
        assert_eq!(unanswered(dir.path(), "g_other"), 0, "按会话分开计");
        note_human_turn(dir.path(), tag).unwrap();
        assert_eq!(unanswered(dir.path(), tag), 0);
        note_human_turn(dir.path(), tag).unwrap();
    }
}
