//! 主线程的停机等待器。
//!
//! SIGTERM/SIGINT 各挂两个 action:`flag::register` 置停机 flag(后台 producer 轮询它),以及往
//! 这里的自管道写一个字节(主线程阻塞读它,零空唤醒)。**flag 是停机的唯一事实源**,自管道只是
//! 省掉轮询的手段,所以它坏了 daemon 也必须活着:读自管道出错或读到 EOF 时记一条带 fd 快照的
//! 日志,退回按 [`FALLBACK_POLL`] 轮询 flag。
//!
//! 自管道确实会坏:进程里别处的代码可能关掉它的读端。2026-09-22 与 09-28 两次夜间实测都是读端
//! 拿到 EBADF,主线程随之退出,整个 daemon 下线。关 fd 的是谁没查清,快照就是留给下次对照的。

use super::*;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

/// 自管道失效后轮询停机 flag 的间隔。只影响「收到信号到开始收尾」的延迟。
const FALLBACK_POLL: Duration = Duration::from_millis(500);

/// 快照覆盖的 fd 编号上界。daemon 常驻的 fd 在几十个量级,留足余量。
const SNAPSHOT_FD_LIMIT: RawFd = 256;

pub(super) struct ShutdownWaiter {
    /// 读端,**从不 close**。编号可能被别处关掉、甚至复用成别的 fd,而等待器未必看得出来 ——
    /// Linux 上阻塞中的 recv 不会因编号被关而醒来,照常读到信号字节。析构时再 close 就是关别人的
    /// fd,debug 构建里还会 IO safety abort(一次异常退出,自启单元会把刚被 `daemon stop` 停掉的
    /// daemon 又拉起来)。一个进程只注册一次、析构就在退出前,漏关这一个 fd 无妨。
    read: std::mem::ManuallyDrop<UnixStream>,
    fd: RawFd,
}

impl ShutdownWaiter {
    /// 为 SIGTERM/SIGINT 注册自管道写入。须紧跟 `flag::register` 调用:注册之后到达的信号留在
    /// 管道里不会丢,之前到达的由 [`Self::wait`] 开头的 flag 复查兜住。
    pub(super) fn register() -> std::io::Result<Self> {
        Self::register_for(&[signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT])
    }

    fn register_for(signals: &[i32]) -> std::io::Result<Self> {
        let (read, write) = UnixStream::pair()?;
        for &signal in signals {
            // 每个信号持一份写端副本;原件随本函数返回关闭,读端因此只会在副本都没了时读到 EOF。
            signal_hook::low_level::pipe::register(signal, write.try_clone()?)?;
        }
        let fd = read.as_raw_fd();
        Ok(Self {
            read: std::mem::ManuallyDrop::new(read),
            fd,
        })
    }

    /// 读端的 fd 编号,启动日志里记下来,出事时能与快照对照。
    pub(super) fn read_fd(&self) -> RawFd {
        self.fd
    }

    /// 阻塞到停机信号到达。返回时停机 flag 一定已置位。
    pub(super) fn wait(&mut self, shutdown: &AtomicBool) {
        let fd = self.fd;
        wait_for_shutdown(shutdown, &mut *self.read, FALLBACK_POLL, |broken| {
            log(&summon_i18n::format!(
                "rddf1bf040a",
                FALLBACK_POLL.as_millis(),
                crate::observe::fd_snapshot::fd_snapshot(SNAPSHOT_FD_LIMIT)
            ));
        });
    }
}

/// 自管道为什么不能再用了。
#[derive(Debug)]
enum Broken {
    /// 读到 EOF:写端已全部关闭。
    Eof,
    Io(std::io::Error),
}

impl std::fmt::Display for Broken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Broken::Eof => f.write_str(summon_i18n::text!("rba68093638")),
            Broken::Io(error) => write!(f, "{error}"),
        }
    }
}

/// [`ShutdownWaiter::wait`] 的本体,自管道与失效回调可注入。
fn wait_for_shutdown(
    shutdown: &AtomicBool,
    pipe: &mut impl Read,
    poll: Duration,
    on_broken: impl FnOnce(&Broken),
) {
    if !shutdown.load(Ordering::SeqCst) {
        if let Err(broken) = read_one_signal(pipe) {
            on_broken(&broken);
            poll_until_set(shutdown, poll);
        }
    }
    // 自管道醒来时不依赖两个 action 的执行顺序:显式置位,保证后台 producer 一定看得到。
    shutdown.store(true, Ordering::SeqCst);
}

fn poll_until_set(shutdown: &AtomicBool, poll: Duration) {
    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(poll);
    }
}

/// 读到一个信号字节 → `Ok`;自管道不可用 → `Err`。
fn read_one_signal(pipe: &mut impl Read) -> Result<(), Broken> {
    let mut byte = [0u8; 1];
    loop {
        match pipe.read(&mut byte) {
            Ok(0) => return Err(Broken::Eof),
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Broken::Io(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// 每次 read 依次吐出预设结果;用完再读说明被测代码多读了,直接判失败。
    struct Scripted(std::collections::VecDeque<std::io::Result<usize>>);

    impl Read for Scripted {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.pop_front().expect("不该再读自管道")
        }
    }

    fn scripted(results: Vec<std::io::Result<usize>>) -> Scripted {
        Scripted(results.into())
    }

    /// 在 `after` 之后把 flag 置位,模拟 signal handler。
    fn set_later(flag: &Arc<AtomicBool>, after: Duration) {
        let flag = Arc::clone(flag);
        thread::spawn(move || {
            thread::sleep(after);
            flag.store(true, Ordering::SeqCst);
        });
    }

    #[test]
    fn returns_without_reading_when_the_flag_is_already_set() {
        // 覆盖「信号早于自管道注册就到达」:那种信号不在管道里,不查 flag 就会永久阻塞。
        let shutdown = AtomicBool::new(true);
        wait_for_shutdown(&shutdown, &mut scripted(vec![]), FALLBACK_POLL, |_| {
            panic!("flag 已置位时不该判失效")
        });
        assert!(shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn a_signal_byte_ends_the_wait_and_sets_the_flag() {
        let shutdown = AtomicBool::new(false);
        wait_for_shutdown(&shutdown, &mut scripted(vec![Ok(1)]), FALLBACK_POLL, |_| {
            panic!("正常收到信号不该判失效")
        });
        assert!(shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn an_interrupted_read_is_retried() {
        let shutdown = AtomicBool::new(false);
        let interrupted = std::io::Error::from(std::io::ErrorKind::Interrupted);
        wait_for_shutdown(
            &shutdown,
            &mut scripted(vec![Err(interrupted), Ok(1)]),
            FALLBACK_POLL,
            |_| panic!("EINTR 不是失效"),
        );
        assert!(shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn a_bad_fd_degrades_to_polling_the_flag_instead_of_dying() {
        // 两次夜间崩溃的原样:读端拿到 EBADF。等待器必须活下来,并在 flag 置位后照常返回。
        let shutdown = Arc::new(AtomicBool::new(false));
        let reported = AtomicUsize::new(0);
        set_later(&shutdown, Duration::from_millis(50));
        let started = Instant::now();
        wait_for_shutdown(
            &shutdown,
            &mut scripted(vec![Err(std::io::Error::from_raw_os_error(
                nix::libc::EBADF,
            ))]),
            Duration::from_millis(5),
            |broken| {
                reported.fetch_add(1, Ordering::SeqCst);
                assert!(
                    matches!(broken, Broken::Io(e) if e.raw_os_error() == Some(nix::libc::EBADF)),
                    "{broken}"
                );
            },
        );
        assert_eq!(reported.load(Ordering::SeqCst), 1, "失效只报一次");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "轮询必须有界地看到 flag"
        );
    }

    #[test]
    fn a_pipe_whose_writers_are_gone_also_degrades_to_polling() {
        // 真管道:写端全关后读端恒读到 EOF。不当失效处理就是 100% CPU 的空转。
        let (mut read, write) = UnixStream::pair().unwrap();
        drop(write);
        let shutdown = Arc::new(AtomicBool::new(false));
        let reported = AtomicUsize::new(0);
        set_later(&shutdown, Duration::from_millis(50));
        wait_for_shutdown(&shutdown, &mut read, Duration::from_millis(5), |broken| {
            reported.fetch_add(1, Ordering::SeqCst);
            assert!(matches!(broken, Broken::Eof), "{broken}");
        });
        assert_eq!(reported.load(Ordering::SeqCst), 1);
    }

    /// 等待器不论怎么返回都不关读端;每条测试最后由测试自己关它,关得掉才说明等待器没碰过。
    /// 编号全程有主:`cargo test` 同进程多线程,一旦腾空就会被别的测试拿走。
    fn close_is_ours(fd: RawFd) {
        // SAFETY:`fd` 一直由本测试持有(等待器从不关它),此处是它唯一一次 close。
        assert_eq!(unsafe { nix::libc::close(fd) }, 0, "等待器不该关读端");
    }

    #[test]
    fn the_read_end_is_left_open_after_a_normal_wake() {
        // 块结束即析构等待器,之后再由测试自己关。
        let fd = {
            let mut waiter =
                ShutdownWaiter::register_for(&[signal_hook::consts::SIGWINCH]).unwrap();
            nix::sys::signal::raise(nix::sys::signal::Signal::SIGWINCH).unwrap();
            let shutdown = AtomicBool::new(false);
            waiter.wait(&shutdown);
            assert!(shutdown.load(Ordering::SeqCst));
            waiter.read_fd()
        };
        close_is_ours(fd);
    }

    /// 读端编号被别处占了(这里用 dup2 换成 /dev/null,recv 它得到 ENOTSOCK):等待器退回轮询,
    /// 也不关这个编号 —— 否则关掉的是别人的 fd。
    #[test]
    fn a_read_end_taken_over_elsewhere_is_left_alone() {
        // 块结束即析构等待器,之后再由测试自己关。
        let fd = {
            let mut waiter = ShutdownWaiter::register_for(&[signal_hook::consts::SIGUSR2]).unwrap();
            let fd = waiter.read_fd();
            let write_only = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .unwrap();
            // SAFETY:两个编号都开着;dup2 原子地让 `fd` 改指 /dev/null,原来的读端随之关闭。
            assert_eq!(unsafe { nix::libc::dup2(write_only.as_raw_fd(), fd) }, fd);
            drop(write_only);
            let shutdown = Arc::new(AtomicBool::new(false));
            set_later(&shutdown, Duration::from_millis(50));
            waiter.wait(&shutdown);
            assert!(shutdown.load(Ordering::SeqCst));
            fd
        };
        close_is_ours(fd);
    }

    /// 核心假设:同一信号上 `flag::register` 与自管道两个 action **并存且都执行**。不成立的话,
    /// 要么主线程醒了而 flag 没置位(后台 producer 永不退出),要么反过来永久阻塞。用 SIGUSR1 做
    /// 探针,不碰生产的 SIGTERM/SIGINT;raise 是同步的,handler 返回前字节已写进管道。
    #[test]
    fn flag_and_self_pipe_both_fire_for_the_same_signal() {
        let flag = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGUSR1, Arc::clone(&flag)).unwrap();
        let mut waiter = ShutdownWaiter::register_for(&[signal_hook::consts::SIGUSR1]).unwrap();

        nix::sys::signal::raise(nix::sys::signal::Signal::SIGUSR1).unwrap();

        assert!(flag.load(Ordering::SeqCst), "flag action 必须执行");
        let unrelated = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            waiter.wait(&unrelated);
            let _ = tx.send(unrelated.load(Ordering::SeqCst));
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "自管道 action 必须与 flag 并存执行,等待器读到字节即返回"
        );
    }
}
