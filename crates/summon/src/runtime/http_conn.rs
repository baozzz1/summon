//! 与业务无关的 HTTP/1.1 连接层:每条连接一个线程,读一个请求、回一个响应即关闭
//! (`Connection: close`)。浏览器终端与控制台共用;各项上限与期限由各服务自己定。
//!
//! 不用 tiny_http:它升级后的连接拆不开读写、设不了超时,做不了双向实时收发;流式响应又要
//! 攒满 8KB 才发出去。

use std::borrow::Cow;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::observe::log::log;

/// 普通响应每次写的块大小。块越小,临近期限时一次写能多拖的时间越短。
const RESPONSE_CHUNK: usize = 4096;

pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    /// 请求头之后已经读进来的字节(POST 体的开头,或升级后浏览器立刻发来的第一帧)。
    pub(crate) rest: Vec<u8>,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 去掉查询串的路径。
    pub(crate) fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }
}

/// 一个请求的大小上限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// 请求头总字节数。
    pub(crate) head: usize,
    /// 请求头条数。超了不是静默丢弃:手机浏览器带一堆头过来时,没有响应也没有日志的连接查不出
    /// 任何东西。
    pub(crate) headers: usize,
    /// 请求体字节数(按 `Content-Length` 声明判,超了不去读)。
    pub(crate) body: usize,
}

/// 读请求头的结果。
pub(crate) enum Head {
    Ok(Request),
    /// 头的条数超过上限:回 431 并记一笔。
    TooManyHeaders,
    /// 对端没了、超时、头超过字节上限、不合 HTTP:放弃这条连接,不回任何东西。
    Abandon,
}

/// 读一个完整请求(头 + 体)的结果。
pub(crate) enum ReadOutcome {
    Ok(Request, Vec<u8>),
    TooManyHeaders,
    /// 声明的请求体超过上限:回 413,不去读它。
    BodyTooLarge,
    Abandon,
}

/// 读请求体失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyError {
    TooLarge,
    Abandon,
}

/// 在整体请求期限内读一次:期限已到(或读失败)都返回 `None`,调用方据此放弃这条连接。
///
/// 整条连接(读头 + 读体)共用一个钟表期限,从连接开始那一刻算一次,每次读只拿剩下的那点。
/// 每次 read 各给一份固定超时的话,每隔几秒发一个字节的慢速客户端(slowloris)每来一个字节就把
/// 计时器摸一遍,能无限占住一个线程和一个连接位。`set_read_timeout` 不收零时长,故提前短路。
pub(crate) fn read_within_deadline(
    stream: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
) -> Option<usize> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    stream.set_read_timeout(Some(remaining)).ok()?;
    stream.read(buf).ok()
}

pub(crate) fn read_head(stream: &mut TcpStream, limits: &Limits, deadline: Instant) -> Head {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let Some(n) = read_within_deadline(stream, &mut chunk, deadline) else {
            return Head::Abandon;
        };
        if n == 0 {
            return Head::Abandon;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let mut headers = vec![httparse::EMPTY_HEADER; limits.headers];
            let mut parsed = httparse::Request::new(&mut headers);
            match parsed.parse(&buf) {
                Ok(httparse::Status::Complete(_)) => {}
                Err(httparse::Error::TooManyHeaders) => return Head::TooManyHeaders,
                _ => return Head::Abandon,
            }
            let (Some(method), Some(path)) = (parsed.method, parsed.path) else {
                return Head::Abandon;
            };
            return Head::Ok(Request {
                method: method.to_string(),
                target: path.to_string(),
                headers: parsed
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_string(),
                            String::from_utf8_lossy(h.value).into_owned(),
                        )
                    })
                    .collect(),
                rest: buf[end + 4..].to_vec(),
            });
        }
        if buf.len() > limits.head {
            return Head::Abandon;
        }
    }
}

/// 按 `Content-Length` 读完请求体(没写或写错按 0 算)。声明超过 `limit` 就不去读;期限到了或
/// 对端提前断开都放弃这条连接。
pub(crate) fn read_body(
    stream: &mut TcpStream,
    req: &Request,
    limit: usize,
    deadline: Instant,
) -> Result<Vec<u8>, BodyError> {
    let len: usize = req
        .header("Content-Length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if len > limit {
        return Err(BodyError::TooLarge);
    }
    let mut body = req.rest[..req.rest.len().min(len)].to_vec();
    let mut chunk = [0u8; 4096];
    while body.len() < len {
        match read_within_deadline(stream, &mut chunk, deadline) {
            Some(0) | None => return Err(BodyError::Abandon),
            Some(n) => body.extend_from_slice(&chunk[..n.min(len - body.len())]),
        }
    }
    Ok(body)
}

/// 读一个完整请求:头与体共用 `deadline`。
pub(crate) fn read_request(
    stream: &mut TcpStream,
    limits: &Limits,
    deadline: Instant,
) -> ReadOutcome {
    let req = match read_head(stream, limits, deadline) {
        Head::Ok(req) => req,
        Head::TooManyHeaders => return ReadOutcome::TooManyHeaders,
        Head::Abandon => return ReadOutcome::Abandon,
    };
    match read_body(stream, &req, limits.body, deadline) {
        Ok(body) => ReadOutcome::Ok(req, body),
        Err(BodyError::TooLarge) => ReadOutcome::BodyTooLarge,
        Err(BodyError::Abandon) => ReadOutcome::Abandon,
    }
}

/// 一条连接上的期限。终端另有 WebSocket 的那几项,自己管。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadlines {
    /// 读完请求(头 + 体)的整体期限,见 [`read_within_deadline`]。
    pub(crate) request: Duration,
    /// 一次写整整这么久都写不进去(对端一个字节都不收)就断开。
    pub(crate) write_stall: Duration,
    /// 从接入到响应写完的总期限,见 [`write_within_deadline`]。
    pub(crate) response: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(10),
            write_stall: Duration::from_secs(10),
            response: Duration::from_secs(30),
        }
    }
}

impl Deadlines {
    /// 接到连接时调用:设好阻塞模式与写期限,返回读请求的期限与写响应的期限(都从此刻起算)。
    pub(crate) fn start(&self, stream: &TcpStream) -> Option<(Instant, ReplyDeadline)> {
        let accepted = Instant::now();
        prepare(stream, self.write_stall).ok()?;
        Some((
            accepted + self.request,
            ReplyDeadline {
                at: accepted + self.response,
                stall: self.write_stall,
            },
        ))
    }
}

/// 接到连接先做的设置:阻塞模式(macOS 上 accept 出来的 socket 继承监听端的非阻塞标志)与写期限。
/// 写期限对这条连接上的每次写都生效,含升级后的 WebSocket。
pub(crate) fn prepare(stream: &TcpStream, write_stall: Duration) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(write_stall))
}

/// 普通响应的写期限。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReplyDeadline {
    /// 整条连接从接入起算的总期限。
    pub(crate) at: Instant,
    /// 单次写的停滞上限。
    pub(crate) stall: Duration,
}

/// 在总期限内把 `bytes` 写出去:按块写,每次写之前把写超时收紧到剩下的时间,期限一到就放弃。
/// 只有单次写超时的话,每几秒读走一点的客户端让每次写都在推进,一个 488KB 的响应能拖几十分钟;
/// 32 条这样的连接就占满全部连接位,而它们连鉴权都不用过。
pub(crate) fn write_within_deadline<W: Write>(
    w: &mut W,
    bytes: &[u8],
    deadline: Instant,
    mut limit_next_write: impl FnMut(&mut W, Duration) -> std::io::Result<()>,
    now: impl Fn() -> Instant,
) -> bool {
    let mut rest = bytes;
    while !rest.is_empty() {
        let remaining = deadline.saturating_duration_since(now());
        if remaining.is_zero() || limit_next_write(w, remaining).is_err() {
            return false;
        }
        match w.write(&rest[..rest.len().min(RESPONSE_CHUNK)]) {
            Ok(0) => return false,
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

/// 一个普通响应。`Content-Type`、`Content-Length`、`Connection: close` 由 [`write_reply`] 写,
/// 其余头(缓存、安全策略、cookie)都由调用方放进 `headers`。
pub(crate) struct Reply<'a> {
    pub(crate) status: &'a str,
    pub(crate) ctype: &'a str,
    pub(crate) body: Cow<'a, [u8]>,
    pub(crate) headers: Vec<(String, String)>,
}

impl<'a> Reply<'a> {
    pub(crate) fn new(status: &'a str, ctype: &'a str, body: impl Into<Cow<'a, [u8]>>) -> Self {
        Self {
            status,
            ctype,
            body: body.into(),
            headers: Vec::new(),
        }
    }

    pub(crate) fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub(crate) fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 响应头的原文(不含体)。值里带换行的头不写:那会让值拆成第二条头。
    fn head(&self) -> String {
        let mut head = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            self.status,
            self.ctype,
            self.body.len()
        );
        for (k, v) in &self.headers {
            if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
                log(&summon_i18n::format!("r30c02e62f5"));
                continue;
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("Connection: close\r\n\r\n");
        head
    }
}

/// 回一个普通响应(之后连接即关闭)。整个响应受 `by` 的总期限管;返回是否写完。
pub(crate) fn write_reply(stream: &mut TcpStream, reply: &Reply<'_>, by: ReplyDeadline) -> bool {
    // 每次写的超时收紧到剩下的时间;响应写完连接就关,不必恢复。客户端提前断开不影响服务。
    let limit =
        |s: &mut TcpStream, remaining: Duration| s.set_write_timeout(Some(remaining.min(by.stall)));
    write_within_deadline(stream, reply.head().as_bytes(), by.at, limit, Instant::now)
        && write_within_deadline(stream, &reply.body, by.at, limit, Instant::now)
}

/// 连接位:接入时占一个,连接处理完(含处理线程 panic)归还。
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 非阻塞 accept 循环,每 100ms 看一次停机标志;每条连接一个线程(`<name>-conn`),同时在处理的
/// 连接超过 `max` 时新来的当场关掉,不排队也不占线程。`listener` 须已设成非阻塞。
pub(crate) fn accept_loop(
    listener: TcpListener,
    active: Arc<AtomicUsize>,
    max: usize,
    shutdown: Arc<AtomicBool>,
    name: &'static str,
    handle: impl Fn(TcpStream) + Send + Sync + 'static,
) {
    let handle = Arc::new(handle);
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if active.fetch_add(1, Ordering::SeqCst) >= max {
                    active.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let slot = Slot(Arc::clone(&active));
                let conn_handle = Arc::clone(&handle);
                // spawn 失败时闭包随错误一起被丢弃,连接位经 `Slot` 归还。
                let _ = std::thread::Builder::new()
                    .name(format!("{name}-conn"))
                    .spawn(move || {
                        let _slot = slot;
                        conn_handle(stream);
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                log(&summon_i18n::format!("r47c57eea32"));
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const SMALL: Limits = Limits {
        head: 512,
        headers: 4,
        body: 64,
    };

    /// 造一对连上的回环连接,把 `raw` 从客户端那一侧写进去。返回服务端那一侧(读它就是读请求)。
    /// 客户端那一侧随返回值一起留着:提前关掉的话服务端读到的是 EOF 而不是「还没读完」。
    fn served(raw: &[u8]) -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.write_all(raw).unwrap();
        (server, client)
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// 两个头的上限都按参数生效:字节数超了放弃连接,条数超了要能说出是哪种拒绝(才回得了 431)。
    #[test]
    fn head_limits_come_from_the_caller() {
        let with_pad = |pad: usize| {
            let mut raw = b"GET /x?a=1&w=ff HTTP/1.1\r\nHost: h\r\nX-Pad: ".to_vec();
            raw.extend(std::iter::repeat_n(b'p', pad));
            raw.extend_from_slice(b"\r\n\r\n");
            raw
        };
        let (mut s, _c) = served(&with_pad(SMALL.head / 2));
        match read_head(&mut s, &SMALL, soon()) {
            Head::Ok(req) => {
                assert_eq!(req.path(), "/x");
                assert_eq!(req.header("host"), Some("h"));
            }
            _ => panic!("上限之内的头照常读出来"),
        }
        let (mut s, _c) = served(&with_pad(SMALL.head + 2048));
        assert!(matches!(read_head(&mut s, &SMALL, soon()), Head::Abandon));

        let with_headers = |count: usize| {
            let mut raw = b"GET /x HTTP/1.1\r\n".to_vec();
            for i in 0..count {
                raw.extend(format!("X-H{i}: v\r\n").into_bytes());
            }
            raw.extend_from_slice(b"\r\n");
            raw
        };
        let (mut s, _c) = served(&with_headers(SMALL.headers));
        assert!(matches!(read_head(&mut s, &SMALL, soon()), Head::Ok(_)));
        let (mut s, _c) = served(&with_headers(SMALL.headers + 1));
        assert!(matches!(
            read_head(&mut s, &SMALL, soon()),
            Head::TooManyHeaders
        ));
    }

    #[test]
    fn read_request_returns_body_up_to_limit() {
        // 体分两次到:一部分跟着头进来,其余稍后。超出声明长度的字节不算进体。
        let body = "x".repeat(SMALL.body);
        let raw = format!(
            "POST /api/a HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            &body[..10]
        );
        let (mut s, mut c) = served(raw.as_bytes());
        let rest = format!("{}trailing", &body[10..]);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _ = c.write_all(rest.as_bytes());
            std::thread::sleep(Duration::from_millis(500));
        });
        match read_request(&mut s, &SMALL, soon()) {
            ReadOutcome::Ok(req, got) => {
                assert_eq!(req.method, "POST");
                assert_eq!(got, body.as_bytes());
            }
            _ => panic!("正好到上限的体照常读完"),
        }

        let (mut s, _c) = served(b"GET / HTTP/1.1\r\n\r\n");
        assert!(matches!(
            read_request(&mut s, &SMALL, soon()),
            ReadOutcome::Ok(_, body) if body.is_empty()
        ));
    }

    #[test]
    fn read_request_rejects_body_over_limit() {
        let raw = format!(
            "POST /api/a HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            SMALL.body + 1
        );
        let (mut s, _c) = served(raw.as_bytes());
        assert!(
            matches!(
                read_request(&mut s, &SMALL, soon()),
                ReadOutcome::BodyTooLarge
            ),
            "声明超过上限就拒绝,不去读它"
        );
    }

    #[test]
    fn a_body_cut_short_or_past_the_deadline_abandons_the_connection() {
        let (mut s, c) = served(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\n12345");
        drop(c);
        assert!(matches!(
            read_request(&mut s, &SMALL, soon()),
            ReadOutcome::Abandon
        ));

        let (mut s, _c) = served(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\n12345");
        let started = Instant::now();
        assert!(matches!(
            read_request(&mut s, &SMALL, Instant::now() + Duration::from_millis(100)),
            ReadOutcome::Abandon
        ));
        assert!(started.elapsed() < Duration::from_secs(2), "期限一到就放弃");
    }

    /// 慢慢读的客户端:每次写只收 100 字节,每次写耗时 1 秒(假时钟)。
    struct Trickle<'a> {
        clock: &'a Cell<Instant>,
        written: usize,
    }

    impl Write for Trickle<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.clock.set(self.clock.get() + Duration::from_secs(1));
            let n = buf.len().min(100);
            self.written += n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 普通响应有总期限:每次写都在推进的慢读客户端,也拖不过期限;每次写的超时收紧到剩下的时间。
    #[test]
    fn a_slow_reader_cannot_stretch_a_response_past_the_deadline() {
        let t0 = Instant::now();
        let clock = Cell::new(t0);
        let mut w = Trickle {
            clock: &clock,
            written: 0,
        };
        let limits = std::cell::RefCell::new(Vec::new());
        let done = write_within_deadline(
            &mut w,
            &[b'x'; 10_000],
            t0 + Duration::from_secs(5),
            |_, d| {
                limits.borrow_mut().push(d);
                Ok(())
            },
            || clock.get(),
        );
        assert!(!done, "期限到了就放弃");
        assert_eq!(w.written, 500, "期限内的 5 次写之后停下");
        assert_eq!(
            limits.into_inner(),
            (1..=5).rev().map(Duration::from_secs).collect::<Vec<_>>(),
            "每次写的超时是剩下的时间"
        );
    }

    #[test]
    fn write_reply_writes_the_status_line_body_and_given_headers() {
        let (mut server, mut client) = served(b"");
        let reply = Reply::new("201 Created", "application/json", b"{}".to_vec())
            .header("Cache-Control", "no-store")
            .header("X-Bad", "a\r\nInjected: 1");
        let by = ReplyDeadline {
            at: soon(),
            stall: Duration::from_secs(1),
        };
        assert!(write_reply(&mut server, &reply, by));
        drop(server);
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        assert_eq!(
            out,
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\
Cache-Control: no-store\r\nConnection: close\r\n\r\n{}"
        );
        assert_eq!(reply.header_value("cache-control"), Some("no-store"));
    }

    /// 每条连接一接进来就带写期限并切回阻塞模式:对端一个字节都不收时,写在期限到后出错返回。
    #[test]
    fn start_sets_the_write_deadline_and_blocking_mode() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let accepted = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("{e}"),
            }
        };
        accepted.set_nonblocking(true).unwrap();
        let deadlines = Deadlines {
            write_stall: Duration::from_millis(1234),
            ..Deadlines::default()
        };
        let before = Instant::now();
        let (read_by, reply_by) = deadlines.start(&accepted).unwrap();
        assert!(read_by >= before + deadlines.request);
        assert!(reply_by.at >= before + deadlines.response);
        assert_eq!(
            accepted.write_timeout().unwrap(),
            Some(deadlines.write_stall)
        );
        accepted
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let started = Instant::now();
        let _ = (&accepted).read(&mut [0u8; 1]);
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    /// 连接上限在 accept 那一步生效,处理完的连接归还连接位;停机标志让循环退出。
    #[test]
    fn accept_loop_caps_connections_and_returns_slots() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (loop_active, loop_shutdown) = (Arc::clone(&active), Arc::clone(&shutdown));
        let looping = std::thread::spawn(move || {
            accept_loop(listener, loop_active, 2, loop_shutdown, "test", |mut s| {
                // 等对端发来一个字节再结束,好让测试控制连接占位多久。accept 出来的连接继承
                // 监听端的非阻塞标志,先切回阻塞。
                let _ = prepare(&s, Duration::from_secs(5));
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = s.read(&mut [0u8; 1]);
            })
        });
        let held: Vec<TcpStream> = (0..2).map(|_| TcpStream::connect(addr).unwrap()).collect();
        let wait = |n: usize| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while active.load(Ordering::SeqCst) != n && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            active.load(Ordering::SeqCst)
        };
        assert_eq!(wait(2), 2, "前提:连接位已占满");
        let mut extra = TcpStream::connect(addr).unwrap();
        extra
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert!(
            matches!(extra.read(&mut [0u8; 1]), Ok(0)),
            "超出上限的连接当场关掉"
        );
        for mut s in held {
            s.write_all(b"x").unwrap();
        }
        assert_eq!(wait(0), 0, "处理完的连接归还连接位");
        shutdown.store(true, Ordering::SeqCst);
        looping.join().unwrap();
    }
}
