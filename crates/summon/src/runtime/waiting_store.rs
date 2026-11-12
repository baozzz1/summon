//! waiting 标记的盘上存取:`<state_dir>/threads/<tag>/waiting.json`。
//!
//! 写方有两个进程:daemon(轮末置位、owner 的明确动作清除)与 Menu Bar 经 CLI 的「Handled」。
//! 后者要先比较回复 id 再删,若不与置位互斥,就会出现「CLI 读到旧回复 → daemon 写下新回复 → CLI 删掉」,
//! 新回复被静默抹掉。所以三种写操作都在同一把文件锁里做。
//!
//! 放在 `threads/<tag>/` 下:群解散 / bot 被移出时,这个目录随会话状态整体清掉。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use summon_core::waiting::WaitingMark;

use crate::runtime::fsio::atomic_write;

const FILE: &str = "waiting.json";
const LOCK: &str = "waiting.lock";

/// 「Handled」的结果。Menu Bar 按 JSON 里的词给人反馈,词是契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HandledOutcome {
    Cleared,
    /// 标记已换成更新的回复:菜单上看到的那条之后 agent 又回了一次,仍算 waiting。
    Superseded,
    Absent,
}

/// chat tag 是 `g`/`p` + 十六进制;CLI 参数会直接拼进路径,别的形状一律拒绝。
fn thread_dir(agent_dir: &Path, tag: &str) -> Result<PathBuf> {
    if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_alphanumeric()) {
        bail!(summon_i18n::format!("re0971509bc"));
    }
    Ok(agent_dir.join("threads").join(tag))
}

fn with_lock<T>(dir: &Path, f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    std::fs::create_dir_all(dir)
        .with_context(|| summon_i18n::format!("rfb7f036934", dir.display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join(LOCK))
        .with_context(|| summon_i18n::format!("rf7f1f01aff", dir.join(LOCK).display()))?;
    lock.lock().context(summon_i18n::text!("ra07997bc08"))?;
    f(&dir.join(FILE))
}

fn read(path: &Path) -> Option<WaitingMark> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub(crate) fn set(agent_dir: &Path, tag: &str, mark: &WaitingMark) -> Result<()> {
    let dir = thread_dir(agent_dir, tag)?;
    with_lock(&dir, |path| {
        atomic_write(path, &serde_json::to_vec(mark)?)
            .with_context(|| summon_i18n::format!("r6dc29183b4", path.display()))
    })
}

pub(crate) fn clear(agent_dir: &Path, tag: &str) -> Result<bool> {
    let dir = thread_dir(agent_dir, tag)?;
    // 绝大多数清除发生在「本来就没有标记」时(每条人的消息都会清一次),不为它建目录、拿锁。
    if !dir.join(FILE).exists() {
        return Ok(false);
    }
    with_lock(&dir, |path| match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| summon_i18n::format!("re68bb3ab3a", path.display()))
        }
    })
}

pub(crate) fn clear_if_reply(
    agent_dir: &Path,
    tag: &str,
    reply_message_id: &str,
) -> Result<HandledOutcome> {
    let dir = thread_dir(agent_dir, tag)?;
    if !dir.join(FILE).exists() {
        return Ok(HandledOutcome::Absent);
    }
    with_lock(&dir, |path| {
        let Some(mark) = read(path) else {
            return Ok(HandledOutcome::Absent);
        };
        if mark.reply_message_id != reply_message_id {
            return Ok(HandledOutcome::Superseded);
        }
        std::fs::remove_file(path)
            .with_context(|| summon_i18n::format!("re68bb3ab3a", path.display()))?;
        Ok(HandledOutcome::Cleared)
    })
}

/// 该 agent 名下全部 waiting。读不出的跳过 —— 坏一个文件不该让整份快照出不来,下次置位会覆盖它。
pub(crate) fn list(agent_dir: &Path) -> Vec<(String, WaitingMark)> {
    let Ok(entries) = std::fs::read_dir(agent_dir.join("threads")) else {
        return Vec::new();
    };
    let mut marks: Vec<(String, WaitingMark)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let tag = entry.file_name().to_str()?.to_string();
            let mark = read(&entry.path().join(FILE))?;
            Some((tag, mark))
        })
        .collect();
    marks.sort_by(|a, b| a.0.cmp(&b.0));
    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(reply: &str) -> WaitingMark {
        WaitingMark {
            chat_id: "oc_1".into(),
            reply_message_id: reply.into(),
            link: "lark://applink.feishu.cn/client/chat/open?openChatId=oc_1".into(),
            since_unix: 1_800_000_000,
        }
    }

    #[test]
    fn set_then_list_then_clear() {
        let dir = tempfile::tempdir().unwrap();
        set(dir.path(), "g1a2", &mark("om_1")).unwrap();
        assert_eq!(list(dir.path()), vec![("g1a2".to_string(), mark("om_1"))]);
        assert!(clear(dir.path(), "g1a2").unwrap());
        assert!(
            !clear(dir.path(), "g1a2").unwrap(),
            "没有标记时清除是空操作"
        );
        assert!(list(dir.path()).is_empty());
    }

    #[test]
    fn clear_on_a_fresh_chat_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!clear(dir.path(), "gnew").unwrap());
        assert!(
            !dir.path().join("threads").exists(),
            "没有标记就不该为清除建目录"
        );
    }

    #[test]
    fn handled_only_clears_the_reply_the_menu_showed() {
        let dir = tempfile::tempdir().unwrap();
        set(dir.path(), "g1", &mark("om_new")).unwrap();
        assert_eq!(
            clear_if_reply(dir.path(), "g1", "om_old").unwrap(),
            HandledOutcome::Superseded
        );
        assert_eq!(list(dir.path()).len(), 1, "期间有了新回复,仍算 waiting");
        assert_eq!(
            clear_if_reply(dir.path(), "g1", "om_new").unwrap(),
            HandledOutcome::Cleared
        );
        assert_eq!(
            clear_if_reply(dir.path(), "g1", "om_new").unwrap(),
            HandledOutcome::Absent
        );
    }

    #[test]
    fn list_skips_unreadable_marks() {
        let dir = tempfile::tempdir().unwrap();
        set(dir.path(), "g1", &mark("om_1")).unwrap();
        let broken = dir.path().join("threads").join("g2");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("waiting.json"), b"{\"chat_id\":").unwrap();
        assert_eq!(list(dir.path()), vec![("g1".to_string(), mark("om_1"))]);
        set(dir.path(), "g2", &mark("om_2")).unwrap();
        assert_eq!(list(dir.path()).len(), 2, "置位直接覆盖坏文件");
    }

    #[test]
    fn handled_outcome_has_a_stable_json_word() {
        let words: Vec<String> = [
            HandledOutcome::Cleared,
            HandledOutcome::Superseded,
            HandledOutcome::Absent,
        ]
        .iter()
        .map(|o| serde_json::to_string(o).unwrap())
        .collect();
        assert_eq!(words, ["\"cleared\"", "\"superseded\"", "\"absent\""]);
    }

    #[test]
    fn rejects_tags_that_are_not_plain_alphanumeric() {
        let dir = tempfile::tempdir().unwrap();
        for tag in ["", "../g1", "g1/../../x", "g 1", "(base/owner)"] {
            assert!(set(dir.path(), tag, &mark("om_1")).is_err(), "tag={tag:?}");
            assert!(clear(dir.path(), tag).is_err(), "tag={tag:?}");
            assert!(
                clear_if_reply(dir.path(), tag, "om_1").is_err(),
                "tag={tag:?}"
            );
        }
    }
}
