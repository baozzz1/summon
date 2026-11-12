//! daemon 进程日志(A1:从 daemon.rs 下沉)。stderr 一行一条带时间戳;daemon_start 把
//! stdout/stderr 重定向到 `~/.summon/run/daemon.log`,故这里只管格式;轮转见下。
//!
//! PF6 运行期轮转:daemon 以周计常驻、日志啰嗦(每工具调用一行),单程无限涨。每 512 条
//! stat 一次,超 10MB → rename 成 `.1`(保留一代)+ 重开新文件 dup2 回 fd1/fd2。
//! **仅 daemon 进程启用**(`enable_stderr_rotation`,由 `daemon::run` 置位)——CLI 进程的
//! stderr 是用户终端,绝不能被 dup2 改写。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 仅 daemon 进程置位(stderr 已重定向到 daemon.log);默认关。
static ROTATE_STDERR: AtomicBool = AtomicBool::new(false);
static LOG_SEQ: AtomicU64 = AtomicU64::new(0);
const ROTATE_CHECK_EVERY: u64 = 512;
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

pub(crate) fn enable_stderr_rotation() {
    ROTATE_STDERR.store(true, Ordering::Relaxed);
}

/// 轮转检查节拍判定(纯):启用 + 到节拍才去 stat(每条日志都 stat 太贵)。
fn should_check(enabled: bool, seq: u64, every: u64) -> bool {
    enabled && seq.is_multiple_of(every)
}

pub(crate) fn log(msg: &str) {
    maybe_rotate();
    eprintln!(
        "[{}] [summon] {msg}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}

/// 到节拍且超限 → rename 成 .1 + 重开新文件,把 fd1/fd2 重指过去(daemon_start 把两者都指向
/// daemon.log)。全程 best-effort:任何一步失败都继续写老 fd,日志绝不致崩。
fn maybe_rotate() {
    let seq = LOG_SEQ.fetch_add(1, Ordering::Relaxed);
    if !should_check(
        ROTATE_STDERR.load(Ordering::Relaxed),
        seq,
        ROTATE_CHECK_EVERY,
    ) {
        return;
    }
    let path = crate::runtime::paths::run_dir().join("daemon.log");
    let oversized = std::fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false);
    if !oversized {
        return;
    }
    let _ = std::fs::rename(&path, crate::runtime::paths::run_dir().join("daemon.log.1"));
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::os::fd::AsRawFd;
        // dup2 把 fd1/fd2 原子地重指到新文件;f drop 只关临时 fd,1/2 各持引用。
        unsafe {
            nix::libc::dup2(f.as_raw_fd(), 1);
            nix::libc::dup2(f.as_raw_fd(), 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_check_requires_enabled_and_beat() {
        assert!(!should_check(false, 0, 512), "未启用(CLI 进程)恒不查");
        assert!(should_check(true, 0, 512), "到节拍才 stat");
        assert!(!should_check(true, 1, 512));
        assert!(should_check(true, 1024, 512));
    }
}
