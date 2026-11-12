//! Claude 的透明 PTY 代理。能力应答先于转发给外层终端的查询回复。
use nix::libc;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::collections::VecDeque;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use summon_core::parse::program_status::Event;

const CAPACITY: usize = 64 * 1024;
struct ChildGuard(Child, bool);
impl ChildGuard {
    fn cleanup_group(&mut self) {
        if !self.1 {
            let _ = killpg(Pid::from_raw(self.0.id() as i32), Signal::SIGKILL);
            self.1 = true;
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.cleanup_group();
        let _ = self.0.wait();
    }
}
struct Terminal {
    term: libc::termios,
    flags: libc::c_int,
}
impl Terminal {
    fn raw() -> io::Result<Self> {
        // SAFETY: 初始化后才读取 termios；fd 0 是外层终端。
        unsafe {
            let mut term = std::mem::zeroed();
            if libc::tcgetattr(0, &mut term) < 0 {
                return Err(io::Error::last_os_error());
            }
            let flags = libc::fcntl(1, libc::F_GETFL);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            let guard = Self { term, flags };
            let mut raw = term;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) < 0
                || libc::fcntl(1, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(guard)
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: 保存的是本进程进入代理前的终端属性和文件标志。
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.term);
            libc::fcntl(1, libc::F_SETFL, self.flags);
        }
    }
}
fn size() -> libc::winsize {
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: size 是有效输出存储，失败时使用初值。
    unsafe {
        libc::ioctl(0, libc::TIOCGWINSZ, &mut size);
    }
    size
}
fn copy_read(fd: i32, bytes: &mut [u8]) -> io::Result<usize> {
    // SAFETY: bytes 是长度正确的可写切片。
    let n = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    if n >= 0 {
        Ok(n as usize)
    } else {
        Err(io::Error::last_os_error())
    }
}
fn flush(fd: i32, queue: &mut VecDeque<u8>) -> io::Result<()> {
    let (bytes, _) = queue.as_slices();
    if bytes.is_empty() {
        return Ok(());
    }
    // SAFETY: bytes 在 write 返回前有效。
    let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if n >= 0 {
        queue.drain(..n as usize);
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    ) {
        Ok(())
    } else {
        Err(error)
    }
}

pub(super) fn run(command: &str, mut observe: impl FnMut(Option<Event>)) -> anyhow::Result<i32> {
    let mut dimensions = size();
    let pty = nix::pty::openpty(Some(&dimensions), None)?;
    let master = pty.master.try_clone()?;
    let slave = pty.slave.try_clone()?;
    drop(pty);
    // fd 0/1 必须保持在外层 pane，子进程三个标准流都指向内层 PTY。
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", &format!("exec {command}")])
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: fork 后只使用 async-signal-safe 的系统调用。
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _terminal = Terminal::raw()?;
    let mut child = ChildGuard(cmd.spawn()?, false);
    drop(cmd);
    let fd = master.as_raw_fd();
    // SAFETY: master 属于本进程；仅为该描述符设置非阻塞。
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let hangup = Arc::new(AtomicBool::new(false));
    let term = signal_hook::flag::register(libc::SIGTERM, stop.clone())?;
    let hup = signal_hook::flag::register(libc::SIGHUP, hangup.clone())?;
    struct Signals(signal_hook::SigId, signal_hook::SigId);
    impl Drop for Signals {
        fn drop(&mut self) {
            signal_hook::low_level::unregister(self.0);
            signal_hook::low_level::unregister(self.1);
        }
    }
    let _signals = Signals(term, hup);
    let mut decoder = summon_core::parse::program_status::Decoder::default();
    let mut to_child = VecDeque::new();
    let mut to_terminal = VecDeque::new();
    let started = Instant::now();
    let mut answered = false;
    let mut stopping = None;
    let mut stop_signal = Signal::SIGTERM;
    let mut eof = false;
    let mut exit = None;
    let mut buf = [0u8; 8192];
    loop {
        if (stop.load(Ordering::Relaxed) || hangup.load(Ordering::Relaxed)) && stopping.is_none() {
            stop_signal = if hangup.load(Ordering::Relaxed) {
                Signal::SIGHUP
            } else {
                Signal::SIGTERM
            };
            let _ = killpg(Pid::from_raw(child.0.id() as i32), stop_signal);
            stopping = Some(Instant::now());
        }
        if stopping.is_some_and(|s: Instant| s.elapsed() > Duration::from_secs(2)) {
            return Ok(128 + stop_signal as i32);
        }
        if exit.is_none() {
            exit = child.0.try_wait()?;
            if exit.is_some() {
                child.cleanup_group();
            }
        }
        if eof && to_terminal.is_empty() && exit.is_some() {
            return Ok(exit
                .map(|status| {
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
                })
                .unwrap_or(1));
        }
        let current = size();
        if current.ws_row != dimensions.ws_row || current.ws_col != dimensions.ws_col {
            // SAFETY: dimensions 是有效 winsize，master 属于本进程。
            unsafe {
                libc::ioctl(fd, libc::TIOCSWINSZ, &current);
            }
            dimensions = current;
        }
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: if to_child.len() < CAPACITY - 8192 - 16 && stopping.is_none() {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            },
            libc::pollfd {
                fd,
                events: if !eof && to_terminal.len() < CAPACITY - 8192 {
                    libc::POLLIN
                } else {
                    0
                } | if to_child.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
                revents: 0,
            },
            libc::pollfd {
                fd: 1,
                events: if to_terminal.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
                revents: 0,
            },
        ];
        // SAFETY: fds 的三个元素在 poll 返回前有效，等待有界。
        if unsafe { libc::poll(fds.as_mut_ptr(), 3, 100) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let n = copy_read(0, &mut buf)?;
            if n == 0 {
                stop.store(true, Ordering::Relaxed);
            } else {
                to_child.extend(&buf[..n]);
            }
        }
        if !eof
            && to_terminal.len() < CAPACITY - 8192
            && fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0
        {
            match copy_read(fd, &mut buf) {
                Ok(0) => eof = true,
                Ok(n) => {
                    decoder.feed(&buf[..n], |event| {
                        if event == Event::Query
                            && !answered
                            && started.elapsed() < Duration::from_secs(10)
                        {
                            to_child.extend(b"\x1b]7501;?\x1b\\");
                            answered = true;
                        }
                        observe(Some(event));
                    });
                    to_terminal.extend(&buf[..n]);
                }
                Err(e) if e.raw_os_error() == Some(libc::EIO) => eof = true,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        // 优先将能力应答交给 Claude，随后才让外层终端看到同批查询。
        if !to_child.is_empty() {
            flush(fd, &mut to_child)?;
        }
        if fds[2].revents & libc::POLLOUT != 0 {
            flush(1, &mut to_terminal)?;
        }
        observe(None);
    }
}
