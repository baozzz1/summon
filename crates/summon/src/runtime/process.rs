//! 本机子进程启动的窄重试原语。

use std::io;
use std::time::Duration;

/// CLI 正好被原地升级、或刚写入的测试夹具尚未脱离写入窗口时，Linux `exec` 会短暂返回
/// `ETXTBSY`。只重试这一种 errno，且总等待严格有界；权限、路径等确定性错误仍立即上报。
const TEXT_FILE_BUSY_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(10),
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
];

pub(crate) fn retry_text_file_busy<T>(operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    retry_text_file_busy_with_delays(&TEXT_FILE_BUSY_RETRY_DELAYS, operation)
}

fn retry_text_file_busy_with_delays<T>(
    delays: &[Duration],
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut delays = delays.iter();
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if error.raw_os_error() == Some(nix::libc::ETXTBSY) => {
                let Some(delay) = delays.next() else {
                    return Err(error);
                };
                std::thread::sleep(*delay);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_recovers_after_transient_text_file_busy() {
        let mut busy_then_ok_calls = 0;
        let value = retry_text_file_busy(|| {
            busy_then_ok_calls += 1;
            if busy_then_ok_calls < 3 {
                Err(io::Error::from_raw_os_error(nix::libc::ETXTBSY))
            } else {
                Ok(7)
            }
        })
        .unwrap();
        assert_eq!(value, 7);
        assert_eq!(busy_then_ok_calls, 3);
    }

    #[test]
    fn retry_exhausts_five_attempts_with_a_185ms_wait_budget() {
        assert_eq!(
            TEXT_FILE_BUSY_RETRY_DELAYS
                .iter()
                .copied()
                .sum::<Duration>(),
            Duration::from_millis(185)
        );
        let started = std::time::Instant::now();
        let mut exhausted_calls = 0;
        let error = retry_text_file_busy(|| {
            exhausted_calls += 1;
            Err::<(), _>(io::Error::from_raw_os_error(nix::libc::ETXTBSY))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::ETXTBSY));
        assert_eq!(exhausted_calls, 5);
        // 调度会延长实际耗时，只检查等待下界；上面的预算断言约束主动等待总和。
        assert!(started.elapsed() >= Duration::from_millis(185));
    }

    #[test]
    fn retry_returns_permission_denied_without_another_attempt() {
        let mut denied_calls = 0;
        let error = retry_text_file_busy(|| {
            denied_calls += 1;
            Err::<(), _>(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(denied_calls, 1);
    }
}
