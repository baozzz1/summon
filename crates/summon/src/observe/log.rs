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
    let line = format!(
        "[{}] [summon] {msg}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    emit(&line);
}

/// 写一行到 stderr(daemon 里即 daemon.log),写失败丢弃这一行。
///
/// `eprintln!` 在写失败时 panic:磁盘满时 daemon.log 写不进去,调日志的线程(chat 线程、
/// 看门狗)会当场死掉,这一轮既不收尾也不发卡。空间不足记进 [`storage_full`] 观测,
/// 在跑的无头轮据此结束并告知用户。
///
/// [`storage_full`]: crate::observe::storage_full
fn write_line(out: &mut dyn std::io::Write, line: &str) {
    if let Err(error) = out.write_all(line.as_bytes()) {
        crate::observe::storage_full::note(&error);
    }
}

#[cfg(not(test))]
fn emit(line: &str) {
    write_line(&mut std::io::stderr().lock(), line);
}

// 测试进程默认走 `eprint!`,让 libtest 照常捕获日志;需要验证真实 stderr 写失败的测试
// 把本线程切到生产写法。
#[cfg(test)]
thread_local! {
    static DIRECT_STDERR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn emit(line: &str) {
    if DIRECT_STDERR.with(std::cell::Cell::get) {
        write_line(&mut std::io::stderr().lock(), line);
    } else {
        eprint!("{line}");
    }
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

    struct FullDisk;

    impl std::io::Write for FullDisk {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_full_log_target_drops_the_line_and_records_storage_full() {
        let start = crate::observe::storage_full::now_ms();
        write_line(&mut FullDisk, "[a] line\n");
        assert!(crate::observe::storage_full::seen_since(start));
    }

    /// 子进程入口:由下一条测试在 stderr 指向 `/dev/full` 时单独运行。
    #[test]
    #[ignore = "由 log_survives_a_full_stderr 在子进程中运行"]
    fn log_to_full_stderr_child() {
        DIRECT_STDERR.with(|direct| direct.set(true));
        let start = crate::observe::storage_full::now_ms();
        log("disk-full probe");
        assert!(crate::observe::storage_full::seen_since(start));
    }

    /// stderr 是写满的文件时 `log` 不能 panic:`/dev/full` 对每次写返回 ENOSPC。
    #[cfg(target_os = "linux")]
    #[test]
    fn log_survives_a_full_stderr() {
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("/dev/full");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "observe::log::tests::log_to_full_stderr_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .stderr(full)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        assert!(stdout.contains("1 passed"), "{stdout}");
    }
}
