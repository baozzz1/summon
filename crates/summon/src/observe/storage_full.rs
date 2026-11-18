//! 会话自己的写盘报「空间不足」(ENOSPC / 配额满)。
//!
//! 只从实际发生的写失败得知,不主动查询剩余空间。观测按会话目录(`<agent>/threads/<tag>`)
//! 记:事件流、出站归档与对话记录都写在这里,本会话的写入报空间不足,只结束本会话在跑的轮,
//! 不波及别的会话。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// 会话目录 → 最近一次观测到空间不足的 unix 毫秒。条目数以会话数为上限。
static OBSERVED: Mutex<Option<HashMap<PathBuf, u64>>> = Mutex::new(None);

/// 这个错误是不是空间不足(磁盘满或配额满)。
pub(crate) fn is_storage_full(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
}

/// 记下会话目录 `scope` 里的一次写失败;是空间不足时返回 true。
pub(crate) fn note(scope: &Path, error: &io::Error) -> bool {
    if !is_storage_full(error) {
        return false;
    }
    let now = now_ms().max(1);
    let mut observed = OBSERVED.lock().unwrap_or_else(PoisonError::into_inner);
    let last = observed
        .get_or_insert_with(HashMap::new)
        .entry(scope.to_path_buf())
        .or_insert(0);
    *last = (*last).max(now);
    true
}

/// 写结果直通;失败且是空间不足时先记到 `scope` 上。
pub(crate) fn noted<T>(scope: &Path, result: io::Result<T>) -> io::Result<T> {
    if let Err(error) = &result {
        note(scope, error);
    }
    result
}

/// 会话目录 `scope` 在 `since_ms` 起(含)是否观测到过空间不足。
pub(crate) fn seen_since(scope: &Path, since_ms: u64) -> bool {
    OBSERVED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|observed| observed.get(scope))
        .is_some_and(|last| *last >= since_ms)
}

/// 会话目录:`<agent_dir>/threads/<tag>`。写入方与轮的判定用同一种拼法。
pub(crate) fn thread_scope(agent_dir: &Path, tag: &str) -> PathBuf {
    agent_dir.join("threads").join(tag)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_space_exhaustion_counts() {
        assert!(is_storage_full(&io::Error::from(
            io::ErrorKind::StorageFull
        )));
        assert!(is_storage_full(&io::Error::from(
            io::ErrorKind::QuotaExceeded
        )));
        assert!(is_storage_full(&io::Error::from_raw_os_error(
            nix::libc::ENOSPC
        )));
        assert!(is_storage_full(&io::Error::from_raw_os_error(
            nix::libc::EDQUOT
        )));
        let dir = tempfile::tempdir().unwrap();
        let start = now_ms();
        for other in [
            io::Error::from(io::ErrorKind::PermissionDenied),
            io::Error::from_raw_os_error(nix::libc::EIO),
            io::Error::other("No space left on device (free text is not evidence)"),
        ] {
            assert!(!is_storage_full(&other), "{other}");
            assert!(!note(dir.path(), &other));
        }
        assert!(!seen_since(dir.path(), start));
    }

    #[test]
    fn observations_stay_with_their_conversation_and_time() {
        let dir = tempfile::tempdir().unwrap();
        let mine = thread_scope(dir.path(), "g-mine");
        let other = thread_scope(dir.path(), "g-other");
        let start = now_ms();
        let full: io::Result<()> = Err(io::Error::from_raw_os_error(nix::libc::ENOSPC));
        assert!(noted(&other, full).is_err());
        assert!(seen_since(&other, start));
        assert!(!seen_since(&mine, start), "别的会话的写失败不算本轮的");
        assert!(
            !seen_since(&other, now_ms() + 1_000),
            "轮开始之前的观测不算"
        );
        assert!(note(&mine, &io::Error::from(io::ErrorKind::StorageFull)));
        assert!(seen_since(&mine, start));
    }
}
