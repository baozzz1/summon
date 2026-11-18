//! 写盘失败里的「空间不足」(ENOSPC / 配额满)。
//!
//! 只从实际发生的写失败得知,不主动查询剩余空间。daemon 的落盘目标(`~/.summon` 下的事件流、
//! 出站归档、台账与 daemon.log)共处一处:任何一处写失败报空间不足,在跑的无头轮也无法再落下
//! 自己的记录,provider 往往同样写不动会话文件而停止输出。这里记下最近一次观测的时刻,
//! 无头轮的 select 循环据此结束本轮并发失败卡。

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 最近一次观测到空间不足的 unix 毫秒;0 = 没有观测。
#[derive(Debug, Default)]
pub(crate) struct Observed {
    last_ms: AtomicU64,
}

impl Observed {
    pub(crate) const fn new() -> Self {
        Self {
            last_ms: AtomicU64::new(0),
        }
    }

    fn note_at(&self, at_ms: u64) {
        self.last_ms.fetch_max(at_ms.max(1), Ordering::Relaxed);
    }

    fn seen_since(&self, since_ms: u64) -> bool {
        let last = self.last_ms.load(Ordering::Relaxed);
        last != 0 && last >= since_ms
    }
}

/// 生产进程共用一份观测。测试进程按线程隔离:测试并行执行,一条测试模拟的空间不足不能结束
/// 同进程里别的测试正在跑的轮。
#[cfg(not(test))]
fn current() -> &'static Observed {
    static OBSERVED: Observed = Observed::new();
    &OBSERVED
}

#[cfg(test)]
fn current() -> &'static Observed {
    thread_local! {
        static OBSERVED: &'static Observed = Box::leak(Box::new(Observed::new()));
    }
    OBSERVED.with(|observed| *observed)
}

/// 这个错误是不是空间不足(磁盘满或配额满)。
pub(crate) fn is_storage_full(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
}

/// 记下一次写失败;是空间不足时返回 true。
pub(crate) fn note(error: &io::Error) -> bool {
    if !is_storage_full(error) {
        return false;
    }
    current().note_at(now_ms());
    true
}

/// 写结果直通;失败且是空间不足时先记下。
pub(crate) fn noted<T>(result: io::Result<T>) -> io::Result<T> {
    if let Err(error) = &result {
        note(error);
    }
    result
}

/// `since_ms` 起(含)是否观测到过空间不足。
pub(crate) fn seen_since(since_ms: u64) -> bool {
    current().seen_since(since_ms)
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
        let start = now_ms();
        for other in [
            io::Error::from(io::ErrorKind::PermissionDenied),
            io::Error::from_raw_os_error(nix::libc::EIO),
            io::Error::other("No space left on device (free text is not evidence)"),
        ] {
            assert!(!is_storage_full(&other), "{other}");
            assert!(!note(&other));
        }
        assert!(!seen_since(start));
    }

    #[test]
    fn observation_is_ordered_by_time() {
        let observed = Observed::new();
        assert!(!observed.seen_since(0));
        observed.note_at(1_000);
        assert!(observed.seen_since(1_000));
        assert!(!observed.seen_since(1_001), "轮开始之前的观测不算本轮的");
        observed.note_at(500);
        assert!(observed.seen_since(1_000), "较早的观测不回退最近时刻");
    }

    #[test]
    fn noted_results_record_only_space_exhaustion() {
        let start = now_ms();
        assert!(!seen_since(start));
        let failed: io::Result<()> = Err(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(noted(failed).is_err());
        assert!(!seen_since(start));
        let full: io::Result<()> = Err(io::Error::from_raw_os_error(nix::libc::ENOSPC));
        assert!(noted(full).is_err());
        assert!(seen_since(start));
    }
}
