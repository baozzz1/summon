//! 一条 WebSocket 对应一个挂在 pty 上的 tmux 客户端,两种页面都一样:它只负责显示。客户端不标
//! 只读(tmux 3.7 起,只读客户端会让同一 tmux 服务上别的程序不带 `-c` 的 `send-keys` 被拒),
//! 但 pty 主端没有任何写入方,浏览器发来的字节永远到不了客户端:tmux 前缀键与命令(切到别的
//! 会话、销毁别的会话——同一 tmux 服务上挂着所有 agent 与 chat 的会话)浏览器直接够不着。窗格里
//! 的程序自己调 tmux 是另一回事,见 spec「安全边界」。可操作页的按键与两种页面的滚动都不经过
//! 这个客户端,而是用 `send-keys -H` 送进目标窗格(见 `input`)。客户端的进程号登记在
//! `tmux::spawn_web_client` 的表里,注入闸据此不把它当成有人在键盘前。三个线程:pty → 浏览器
//! (本线程)、浏览器 → 窗格、pty 读取。结束时只停这个 attach 客户端进程,tmux 会话不动。

use std::fs::File;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use summon_core::web_terminal::grants::BrowserSession;
use summon_core::web_terminal::ws::{self, ClientMessage, Control, FrameDecoder};

use super::input::{PageInput, Restore, Scrolled};
use super::pins::TmuxWindow;
use super::WebTerminal;
use crate::observe::log::log;
use crate::session::tmux::{spawn_web_client, Tmux, WebClientRegistration};

/// 输出空闲时多久看一次停机标志、链接是否仍有效、浏览器是否已断开、客户端是否还在目标会话上。
const TICK: Duration = Duration::from_millis(500);
/// pty 输出在内存里最多攒这么多块(每块至多 8KB)。满了读线程就停下不读,背压经 pty 传回 tmux,
/// 由 tmux 自己对慢终端丢帧重画;不设上限的话,慢浏览器会让 daemon 替它无限缓存输出。
const OUTPUT_BACKLOG: usize = 64;

/// attach 客户端从 daemon 继承的环境变量(有才传)。`TERM` 固定另设;区域变量不传,客户端带 `-u`
/// 按 UTF-8 画屏(见 `Tmux::attach_command`)。
const CLIENT_ENV: &[&str] = &["HOME", "PATH", "USER", "TMUX_TMPDIR"];

/// 显示客户端的命令(stdio 与控制终端由 `AttachClient::spawn` 接上)。
fn display_command(tmux: &Tmux) -> std::process::Command {
    let mut cmd = tmux.attach_command(false);
    // 客户端只拿它需要的环境:tmux 的 `update-environment` 会把客户端的变量抄进会话,daemon
    // 自己的环境不该这样流进 agent 的会话。daemon 若起在 tmux 里,带着 TMUX 的 attach 会被
    // 拒绝(嵌套),清空环境顺带去掉了它。
    cmd.env_clear().env("TERM", "xterm-256color");
    for key in CLIENT_ENV {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd
}

struct AttachClient {
    child: Child,
    master: File,
    /// 在 `Drop` 跑完(客户端进程已回收)之后才随字段析构注销:注销早了,还挂在 tmux 上的客户端
    /// 会被注入闸算成有人在键盘前。
    _registration: WebClientRegistration,
}

impl AttachClient {
    /// 起显示客户端并登记它的进程号。pty 主端此后没有任何写入方。
    fn spawn(tmux: &Tmux, cols: u16, rows: u16) -> std::io::Result<Self> {
        let size = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = nix::pty::openpty(Some(&size), None).map_err(std::io::Error::from)?;
        // openpty 给的两个 fd 不带 CLOEXEC,daemon 此后起的每个子进程都会继承——包括这个 tmux
        // 客户端自己:它攥着自己 pty 的 master,daemon 崩了也收不到挂断,永远挂在会话上;别的长寿
        // 子进程攥着 slave,客户端退出后这边也读不到 EOF。换成带 CLOEXEC 的副本,原件当场关掉。
        let master = pty.master.try_clone()?;
        let slave = pty.slave.try_clone()?;
        drop(pty);
        let mut cmd = display_command(tmux);
        cmd.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY:pre_exec 在 fork 后、exec 前运行,只调 async-signal-safe 的 setsid 与 ioctl。
        // 新会话 + TIOCSCTTY 让 pty 成为 tmux 客户端的控制终端,它才能读到窗口尺寸、收到挂断。
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::from)?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // cmd 里握着的 slave 副本随本函数返回释放:客户端退出后 master 才读得到 EOF。
        let (child, registration) = spawn_web_client(&mut cmd)?;
        Ok(Self {
            child,
            master: File::from(master),
            _registration: registration,
        })
    }

    /// 结束这个 attach 客户端。动手的是 [`Drop`],这里只是让调用点读起来是一个动作。
    fn stop(self) {}

    /// 仅测试:关掉这边的 pty 主端副本(换成 `/dev/null`),客户端随即收到挂断。
    #[cfg(test)]
    fn close_master(&mut self) {
        self.master = File::open("/dev/null").expect(summon_i18n::text!("r44b1c761e2"));
    }

    /// 仅测试:用任意命令当客户端进程,走与 [`Self::spawn`] 相同的登记与析构路径(不需要 tmux)。
    #[cfg(test)]
    fn probe(cmd: &mut std::process::Command) -> std::io::Result<Self> {
        let (child, registration) = spawn_web_client(cmd)?;
        Ok(Self {
            child,
            master: File::open("/dev/null")?,
            _registration: registration,
        })
    }
}

impl Drop for AttachClient {
    /// 先 SIGTERM 让 tmux 客户端正常断开,2 秒不退再强制结束客户端进程。
    ///
    /// **收尾必须挂在析构上**:显示客户端不标只读,漏在外面的孤儿客户端会被注入闸算成有人在键盘
    /// 前,那个 chat 的飞书消息从此按草稿被拒。连接线程 panic 展开时没人调 `stop`,只有析构还会跑。
    fn drop(&mut self) {
        // 已经退出并被回收过的子进程,`id()` 给的是系统可能已经释放的 pid,不再往它发信号。
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let pid = nix::unistd::Pid::from_raw(self.child.id() as i32);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 这个页面还该不该连着:链接是一次性的,同一收件人在同一 chat 上**换一组新链接就作废上一组**
/// (连同已经打开的页面),浏览器会话满 2 小时也一样。两种都由 `is_live` 一起判掉,页面拿到的
/// 结束原因也是同一句——用户看到的是「这条链接不作数了」,分不分得清是哪种没有意义。
fn revoked_reason(live: bool) -> Option<&'static str> {
    (!live).then_some(summon_i18n::text!("r2534022ee7"))
}

/// 按键送不进窗格时给页面的结束原因。只有会话确实不在了才说「会话已结束」;会话还在却送不进
/// (tmux 报别的错),页面该让人去看日志,而不是以为会话没了。
fn send_failure_reason(session_alive: bool) -> &'static str {
    if session_alive {
        summon_i18n::text!("rcba6b55392")
    } else {
        summon_i18n::text!("r8c662c6438")
    }
}

/// 接入后这么久客户端还没出现在 `list-clients` 里,或 `list-clients` 连续失败这么久,就结束连接。
const CLIENT_CONFIRM_LIMIT: Duration = Duration::from_secs(10);

/// 核对我们的 attach 客户端仍挂在目标会话上。证不了就结束(fail closed):挂在别的会话上(会话
/// 被拆后 tmux 把它切走了)、进过列表又不见了、接入后迟迟不进列表、列表一直读不出来。
struct ClientWatch {
    attached_at: Instant,
    seen_on_target: bool,
    /// 最近一次 `list-clients` 成功的时刻;还没成功过时是接入时刻。
    last_listed: Instant,
}

impl ClientWatch {
    fn new(attached_at: Instant) -> Self {
        Self {
            attached_at,
            seen_on_target: false,
            last_listed: attached_at,
        }
    }

    /// 一拍核对。`listed` 是这拍查到的客户端所在会话(不在列表里为 None)。返回 true 即结束连接。
    fn should_end(
        &mut self,
        listed: &std::io::Result<Option<String>>,
        target: &str,
        now: Instant,
    ) -> bool {
        match listed {
            Ok(Some(session)) => {
                self.last_listed = now;
                self.seen_on_target = true;
                session != target
            }
            Ok(None) => {
                self.last_listed = now;
                self.seen_on_target || now.duration_since(self.attached_at) >= CLIENT_CONFIRM_LIMIT
            }
            Err(_) => now.duration_since(self.last_listed) >= CLIENT_CONFIRM_LIMIT,
        }
    }
}

fn send(writer: &Mutex<TcpStream>, frame: &[u8]) -> bool {
    writer
        .lock()
        .map(|mut s| s.write_all(frame).is_ok())
        .unwrap_or(false)
}

fn end(writer: &Mutex<TcpStream>, reason: &str) {
    let _ = send(
        writer,
        &ws::encode_text(&serde_json::json!({ "ended": reason }).to_string()),
    );
    let _ = send(writer, &ws::encode_close());
}

pub(super) fn run(
    stream: TcpStream,
    wt: &WebTerminal,
    session: BrowserSession,
    early: Vec<u8>,
    shutdown: &AtomicBool,
) {
    // 浏览器每 `ping` 回一个 pong,这么久一个字节都没来就是对端没了:输入线程的读超时即结束连接。
    if stream
        .set_read_timeout(Some(wt.timeouts.peer_silence))
        .is_err()
    {
        return;
    }
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let writer = Arc::new(Mutex::new(stream));
    let tmux = Tmux::new(session.target.tmux_session.clone());
    if !tmux.has_session() {
        return end(&writer, summon_i18n::text!("rb07e80b0e6"));
    }
    // 网页常常是会话上唯一的客户端:开着 destroy-unattached 时,它一断开 tmux 就把会话销毁。
    // 读不到取值同样不接——判错的代价是拆掉一个活会话。
    match tmux.destroyed_on_last_detach() {
        Ok(false) => {}
        Ok(true) => {
            log(&summon_i18n::format!(
                "r4a3cc73d61",
                session.target.agent,
                session.target.chat_id
            ));
            return end(&writer, summon_i18n::text!("rd965cc59d7"));
        }
        Err(e) => {
            log(&summon_i18n::format!("rbd26b92fc5"));
            return end(&writer, summon_i18n::text!("r21fec6343e"));
        }
    }
    // 会话被拆时 tmux 按 detach-on-destroy 处置挂着的客户端:不是 `on` 就把它切到别的会话上,
    // 网页随之看到另一个 agent 或 chat 的屏幕。读不到同样不接。
    match tmux.detaches_on_destroy() {
        Ok(true) => {}
        Ok(false) => {
            log(&summon_i18n::format!(
                "ra9ce80b548",
                session.target.agent,
                session.target.chat_id
            ));
            return end(&writer, summon_i18n::text!("r6f8257c8b2"));
        }
        Err(e) => {
            log(&summon_i18n::format!("r1639d8889b"));
            return end(&writer, summon_i18n::text!("r719f2bffa1"));
        }
    }
    let window = TmuxWindow;
    let pin = match wt.pins.acquire(&window, &tmux.session) {
        Ok(pin) => pin,
        Err(e) => {
            log(&summon_i18n::format!("r9dfb269842"));
            return end(&writer, summon_i18n::text!("rb80dc1e649"));
        }
    };
    // 按钉住的那个窗口读尺寸,不按会话的当前窗口:两者在这中间就可能已经不是同一个了。
    let geometry = match pin.window().geometry() {
        Ok(g) => g,
        Err(e) => {
            log(&summon_i18n::format!("rf128470fec"));
            return end(&writer, summon_i18n::text!("r46ecb3c24a"));
        }
    };
    let (cols, rows) = (geometry.cols, geometry.rows + geometry.status_lines);
    let client = match AttachClient::spawn(&tmux, cols, rows) {
        Ok(c) => c,
        Err(e) => {
            log(&summon_i18n::format!("r3d179e38dd"));
            return end(&writer, summon_i18n::text!("r6b09d18fa0"));
        }
    };
    let hello = serde_json::json!({ "cols": cols, "rows": rows, "mode": session.mode.as_str() });
    if !send(&writer, &ws::encode_text(&hello.to_string())) {
        return client.stop();
    }
    log(&summon_i18n::format!(
        "ra906e25b78",
        session.target.agent,
        session.target.chat_id,
        session.mode.as_str()
    ));

    // pty → 通道。客户端退出后读到 EOF / EIO,通道随之断开。
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(OUTPUT_BACKLOG);
    let Ok(mut pty_reader) = client.master.try_clone() else {
        return client.stop();
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = pty_reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    // 浏览器 → 窗格。只读连接只有滚动那条路,见 `PageInput`。
    let mut input = PageInput::for_mode(session.mode, &tmux.session);
    let closed = Arc::new(AtomicBool::new(false));
    // 输入线程因按键送不进去而收尾时,留给页面的结束原因。
    let input_end: Arc<OnceLock<&'static str>> = Arc::new(OnceLock::new());
    let input_bytes = Arc::new(AtomicU64::new(0));
    let scrolls = Arc::new(ScrollCounts::default());
    // 输入线程收尾时交回 `input`:连接结束后要用它把这条连接往上翻过的翻回去。
    let input_thread = {
        let closed = Arc::clone(&closed);
        let input_end = Arc::clone(&input_end);
        let input_bytes = Arc::clone(&input_bytes);
        let scrolls = Arc::clone(&scrolls);
        let writer = Arc::clone(&writer);
        let mut read_half = read_half;
        std::thread::spawn(move || {
            let mut decoder = FrameDecoder::default();
            let mut pending = early;
            let mut buf = [0u8; 4096];
            // 帧不合协议就断开这条连接。
            'outer: while let Ok(messages) = decoder.push(&pending) {
                pending.clear();
                for message in messages {
                    match message {
                        ClientMessage::Data(bytes) => {
                            if let Some(keys) = input.keys() {
                                input_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                                if let Err(e) = keys.send(&bytes) {
                                    let alive = keys.has_session();
                                    log(&summon_i18n::format!(
                                        "r7b92d10ef0",
                                        if alive {
                                            summon_i18n::text!("r83eca8f0b0")
                                        } else {
                                            summon_i18n::text!("r0cde131710")
                                        }
                                    ));
                                    let _ = input_end.set(send_failure_reason(alive));
                                    break 'outer;
                                }
                            }
                        }
                        ClientMessage::Control(Control::Scroll(notches)) => {
                            scrolls.count(input.scroll().scroll(notches, Instant::now()));
                        }
                        ClientMessage::Ping(p) => {
                            let _ = send(&writer, &ws::encode_pong(&p));
                        }
                        ClientMessage::Close => break 'outer,
                    }
                }
                match read_half.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => pending.extend_from_slice(&buf[..n]),
                }
            }
            closed.store(true, Ordering::SeqCst);
            input
        })
    };

    // 本线程:把输出推给浏览器,并定期检查会话是否还该连着。
    let client_pid = client.child.id();
    let mut next_ping = Instant::now() + wt.timeouts.ping;
    let mut next_check = Instant::now() + TICK;
    let mut watch = ClientWatch::new(Instant::now());
    let reason = loop {
        let wait = TICK.min(next_ping.saturating_duration_since(Instant::now()));
        match rx.recv_timeout(wait) {
            Ok(chunk) => {
                if !send(&writer, &ws::encode_binary(&chunk)) {
                    break None;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Some(summon_i18n::text!("r8c662c6438"))
            }
        }
        if closed.load(Ordering::SeqCst) {
            break input_end.get().copied();
        }
        if shutdown.load(Ordering::Relaxed) {
            break Some(summon_i18n::text!("rcce15be879"));
        }
        if let Some(reason) = revoked_reason(wt.is_live(&session.handle)) {
            break Some(reason);
        }
        let now = Instant::now();
        if now >= next_ping {
            next_ping = now + wt.timeouts.ping;
            if !send(&writer, &ws::encode_ping(b"")) {
                break None;
            }
        }
        // 输出密集时本循环转得很快,查客户端列表要起一次 tmux 进程,按 TICK 限频。
        if now >= next_check {
            next_check = now + TICK;
            let listed = crate::session::tmux::client_session(client_pid);
            if watch.should_end(&listed, &tmux.session, now) {
                if listed.is_ok() && watch.seen_on_target {
                    break Some(summon_i18n::text!("r8c662c6438"));
                }
                log(&summon_i18n::format!(
                    "r77cca85443",
                    CLIENT_CONFIRM_LIMIT.as_secs()
                ));
                break Some(summon_i18n::text!("r93449fdc83"));
            }
        }
    };
    // 读线程可能正堵在满了的通道上;收端一丢,它的 send 立即出错退出。
    drop(rx);
    if let Some(reason) = reason {
        end(&writer, reason);
    }
    // 关掉 socket 让「浏览器 → 窗格」线程从阻塞的 read 里出来。
    if let Ok(s) = writer.lock() {
        let _ = s.shutdown(Shutdown::Both);
    }
    client.stop();
    // 输入线程此刻至多还在送最后一条按键或滚动,等它交回 `input` 再翻回去:页面关掉时停在历史里,
    // 注入闸与轮内的菜单识别读到的就是历史而不是输入框。输入线程 panic 了就不翻。
    let restored = input_thread
        .join()
        .map(|mut input| input.scroll().restore())
        .unwrap_or(Restore::Skipped(summon_i18n::text!("r65cbd250ea")));
    log(&summon_i18n::format!(
        "rc0203e9c70",
        session.target.agent,
        session.target.chat_id,
        session.mode.as_str(),
        input_bytes.load(Ordering::Relaxed),
        scrolls.sent.load(Ordering::Relaxed),
        scrolls.dropped.load(Ordering::Relaxed),
        restored,
    ));
}

/// 这条连接的滚动计数,断开时进日志:真机上「滚不动」时,看得出是消息没来、还是来了被丢掉
/// (窗格在模式里、没开鼠标汇报、限频)。
#[derive(Default)]
struct ScrollCounts {
    sent: AtomicU64,
    dropped: AtomicU64,
}

impl ScrollCounts {
    fn count(&self, outcome: Scrolled) {
        let counter = match outcome {
            Scrolled::Sent => &self.sent,
            Scrolled::Dropped => &self.dropped,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 显示客户端是普通客户端:只读客户端会让同一 tmux 服务上别的程序不带 `-c` 的 `send-keys`
    /// 被拒。它不收任何输入靠的是 pty 主端没有写入方,不靠只读标志。
    #[test]
    fn the_display_client_is_a_plain_utf8_client_with_only_whitelisted_env() {
        let cmd = display_command(&Tmux::new("soul-x"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-u", "attach-session", "-t", "=soul-x"]);
        let envs: Vec<String> = cmd
            .get_envs()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(envs.iter().any(|k| k == "TERM"), "{envs:?}");
        assert!(
            envs.iter()
                .all(|k| k == "TERM" || CLIENT_ENV.contains(&k.as_str())),
            "{envs:?}"
        );
    }

    /// 连接线程 panic 展开时没人调 `stop`,客户端进程只能由析构收掉:漏在外面的孤儿客户端不标
    /// 只读,注入闸会把它算成有人在键盘前,那个 chat 的飞书消息从此按草稿被拒。析构跑完时进程
    /// 必须已经被结束**并回收**(留个僵尸也算没收掉),登记表里也不能再有它。
    #[test]
    fn a_client_dropped_without_stop_is_reaped_and_deregistered() {
        let client = AttachClient::probe(std::process::Command::new("sleep").arg("30")).unwrap();
        let pid = client.child.id();
        assert!(
            crate::session::tmux::web_client_registered(pid),
            "起进程时就登记"
        );
        drop(client);
        assert!(
            !crate::session::tmux::web_client_registered(pid),
            "析构即注销"
        );
        let signalable = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None);
        assert_eq!(
            signalable,
            Err(nix::errno::Errno::ESRCH),
            "客户端进程应当已被结束并回收(僵尸也算没收掉)"
        );
    }

    /// 链接一次性:换一组新链接会作废上一组(连同已经打开的页面),浏览器会话过期同理——两种
    /// 都让这条连接就此结束。判据接在连接循环上,那一句没了页面就会在链接被换掉之后继续连着,
    /// 而能证伪它的只有一条 ignored 的真 tmux 测试。
    #[test]
    fn an_open_page_ends_once_its_links_are_no_longer_live() {
        assert_eq!(revoked_reason(true), None);
        assert!(revoked_reason(false).is_some_and(|r| r.contains("链接已失效")));
        // 针在运行时拼出来:写成整串的话,这行断言自己就是 include_str! 扫得到的那段文本。
        let call = format!("revoked_reason(wt.{}(&session.handle))", "is_live");
        assert!(
            include_str!("attach.rs").contains(&call),
            "连接循环每拍都要问一次这组链接还作不作数"
        );
    }

    /// 按键送不进去时,只有会话确实不在了才说「会话已结束」。
    #[test]
    fn a_failed_send_says_the_session_ended_only_when_it_is_gone() {
        assert_eq!(send_failure_reason(false), "会话已结束。");
        assert_eq!(
            send_failure_reason(true),
            "按键送不进会话(见 daemon 日志)。"
        );
    }

    fn on(session: &str) -> std::io::Result<Option<String>> {
        Ok(Some(session.to_string()))
    }

    fn failed() -> std::io::Result<Option<String>> {
        Err(std::io::Error::other("list-clients 失败"))
    }

    #[test]
    fn the_client_ends_once_it_is_moved_off_the_target_or_gone() {
        let t0 = Instant::now();
        let mut watch = ClientWatch::new(t0);
        assert!(!watch.should_end(&Ok(None), "s", t0), "刚起还没进列表");
        assert!(!watch.should_end(&on("s"), "s", t0));
        assert!(watch.should_end(&Ok(None), "s", t0), "进过列表又没了");
        let mut watch = ClientWatch::new(t0);
        assert!(watch.should_end(&on("other"), "s", t0), "被切到别的会话");
    }

    /// 迟迟不出现在列表里、或列表一直读不出来,都不无限等:接入 10 秒后仍证不了就结束(fail closed)。
    #[test]
    fn the_client_ends_when_its_presence_cannot_be_confirmed_within_ten_seconds() {
        let t0 = Instant::now();
        let s = |secs: u64| t0 + Duration::from_secs(secs);
        let mut watch = ClientWatch::new(t0);
        assert!(!watch.should_end(&Ok(None), "s", s(9)));
        assert!(watch.should_end(&Ok(None), "s", s(10)), "10 秒还没进列表");

        let mut watch = ClientWatch::new(t0);
        assert!(!watch.should_end(&failed(), "s", s(9)));
        assert!(
            watch.should_end(&failed(), "s", s(10)),
            "列表从一开始就读不出"
        );

        let mut watch = ClientWatch::new(t0);
        assert!(!watch.should_end(&on("s"), "s", s(5)));
        assert!(
            !watch.should_end(&failed(), "s", s(14)),
            "上次读到才过 9 秒"
        );
        assert!(watch.should_end(&failed(), "s", s(15)), "连续失败满 10 秒");

        let mut watch = ClientWatch::new(t0);
        assert!(!watch.should_end(&on("s"), "s", s(5)));
        assert!(!watch.should_end(&failed(), "s", s(14)));
        assert!(!watch.should_end(&on("s"), "s", s(15)), "读到了就重新计时");
        assert!(!watch.should_end(&failed(), "s", s(24)));
        assert!(
            !watch.should_end(&on("s"), "s", s(60)),
            "见过之后不再有出现期限"
        );
    }

    /// 真 tmux:attach 客户端不带 daemon 的环境。会话的 `update-environment` 列着的变量,tmux 会在
    /// 客户端接入时抄进会话;daemon 进程里有的变量不能这样流进 agent 的会话。
    #[test]
    #[ignore]
    fn the_attach_client_carries_none_of_the_daemon_environment() {
        const PROBE: &str = "SUMMON_WT_LEAK_PROBE";
        let session = crate::test_support::ThrowawaySession::start("summon-wt-env");
        let tmux = &session.0;
        let target = format!("={}:", tmux.session);
        let set = std::process::Command::new("tmux")
            .args(["set-option", "-t", &target, "update-environment", PROBE])
            .status()
            .unwrap();
        assert!(set.success());
        std::env::set_var(PROBE, "leaked");
        let client = AttachClient::spawn(tmux, 80, 24).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let shown = std::process::Command::new("tmux")
            .args(["show-environment", "-t", &target, PROBE])
            .output()
            .unwrap();
        client.stop();
        std::env::remove_var(PROBE);
        let shown = String::from_utf8_lossy(&shown.stdout);
        assert!(!shown.contains("leaked"), "{shown}");
    }

    /// 真 tmux:daemon 若不经 `stop` 就没了(崩溃),pty master 随之关闭,tmux 客户端必须跟着
    /// 挂断退出。否则孤儿客户端一直挂在会话上,占着窗口钉住。
    #[test]
    #[ignore]
    fn an_attach_client_hangs_up_when_its_pty_master_is_gone() {
        let session = crate::test_support::ThrowawaySession::start("summon-wt-hup");
        let tmux = &session.0;
        let mut client = AttachClient::spawn(tmux, 80, 24).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let attached = super::super::tests::clients_on(tmux);
        client.close_master();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut exited = false;
        while Instant::now() < deadline {
            if matches!(client.child.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // 没自行退出的客户端由 `client` 析构收尾(只结束这个客户端进程);会话由 ThrowawaySession
        // 析构时让 shell 自己 exit。
        drop(client);
        assert_eq!(attached, 1, "客户端应当已挂上会话");
        assert!(
            exited,
            "master 关了,客户端却还挂着:它手里有一份自己 pty 的 master"
        );
    }
}
