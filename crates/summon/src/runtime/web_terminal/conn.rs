//! 单条连接:读请求头 → 路由 → 响应。所有响应 `Connection: close`,一条连接只服务一个请求;
//! WebSocket 升级后交给 `attach`。

use base64::Engine as _;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use summon_core::web_terminal::grants::SESSION_TTL_MS;
use summon_core::web_terminal::http::{self, Asset, Route};
use summon_core::web_terminal::page;
use summon_core::web_terminal::ws;

use crate::runtime::http_conn::{self, Head, Limits, Reply, ReplyDeadline, Request};
// 读写的通用部分已搬到 `http_conn`;本文件的测试按原样保留,下面几项只有它们还在用。
#[cfg(test)]
use {crate::runtime::http_conn::write_within_deadline, std::io::Read, std::time::Duration};

use super::{Timeouts, WebTerminal};

/// 请求头总字节上限、POST 体上限(交付冻结的数值,见 spec「链接与凭证」)。
const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 1024;
/// 一个请求最多认这么多条头。超了不是静默丢弃:手机浏览器带一堆头过来时,没有响应也没有日志
/// 的连接查不出任何东西。
const MAX_HEADERS: usize = 32;
const LIMITS: Limits = Limits {
    head: MAX_HEAD,
    headers: MAX_HEADERS,
    body: MAX_BODY,
};

const XTERM_JS: &[u8] = include_bytes!("../../../assets/web_terminal/xterm.js");
const XTERM_CSS: &[u8] = include_bytes!("../../../assets/web_terminal/xterm.css");

pub(super) fn serve(mut stream: TcpStream, wt: &WebTerminal, shutdown: &AtomicBool) {
    let accepted = Instant::now();
    if prepare(&stream, &wt.timeouts).is_err() {
        return;
    }
    let deadline = accepted + wt.timeouts.request;
    let by = ReplyDeadline {
        at: accepted + wt.timeouts.response,
        stall: wt.timeouts.write_stall,
    };
    let req = match read_head(&mut stream, deadline) {
        Head::Ok(req) => req,
        Head::TooManyHeaders => {
            crate::observe::log::log(&summon_i18n::format!("r639cc65a26"));
            return respond(
                &mut stream,
                by,
                "431 Request Header Fields Too Large",
                "text/plain; charset=utf-8",
                b"too many headers",
                &[],
            );
        }
        Head::Abandon => return,
    };
    match http::route(&req.method, &req.target) {
        Route::Asset(Asset::XtermJs) => respond(
            &mut stream,
            by,
            "200 OK",
            "application/javascript",
            XTERM_JS,
            &[],
        ),
        Route::Asset(Asset::XtermCss) => {
            respond(&mut stream, by, "200 OK", "text/css", XTERM_CSS, &[])
        }
        Route::Landing(code) => match wt.peek(code) {
            Some(mode) => html(&mut stream, by, "200 OK", &page::landing_page(mode, code)),
            None => html(&mut stream, by, "404 Not Found", &page::invalid_page()),
        },
        Route::Enter(code) => {
            if !drain_body(&mut stream, &req, deadline) {
                return;
            }
            match wt.consume(code) {
                Some(session) => {
                    let location = format!("/t/s/{}", session.handle);
                    let cookie =
                        http::set_cookie(&session.handle, session.secret(), SESSION_TTL_MS / 1000);
                    respond(
                        &mut stream,
                        by,
                        "303 See Other",
                        "text/plain; charset=utf-8",
                        b"",
                        &[("Location", &location), ("Set-Cookie", &cookie)],
                    );
                    crate::observe::log::log(&summon_i18n::format!(
                        "r4a00eba48d",
                        session.target.agent,
                        session.target.chat_id,
                        session.mode.as_str()
                    ));
                }
                None if wt.awaiting_activation(code) => {
                    html(&mut stream, by, "409 Conflict", &page::unconfirmed_page())
                }
                None => html(&mut stream, by, "404 Not Found", &page::invalid_page()),
            }
        }
        Route::Page(handle) => match authed(wt, &req, handle) {
            Some(s) => html(
                &mut stream,
                by,
                "200 OK",
                &page::terminal_page(handle, s.mode, &s.target.agent, &s.target.chat_label),
            ),
            None => html(&mut stream, by, "404 Not Found", &page::invalid_page()),
        },
        Route::Socket(handle) => {
            let handle = handle.to_string();
            upgrade(stream, wt, req, &handle, by, shutdown);
        }
        Route::NotFound => respond(
            &mut stream,
            by,
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"not found",
            &[],
        ),
    }
}

/// 接到连接先做的设置:阻塞模式(macOS 上 accept 出来的 socket 继承监听端的非阻塞标志)与写期限。
/// 写期限对这条连接上的每次写都生效,含升级后的 WebSocket。
fn prepare(stream: &TcpStream, timeouts: &Timeouts) -> std::io::Result<()> {
    http_conn::prepare(stream, timeouts.write_stall)
}

/// WebSocket 握手,成功后把连接交给 `attach`。同源、会话 cookie 两道都过了才升级。
fn upgrade(
    mut stream: TcpStream,
    wt: &WebTerminal,
    req: Request,
    handle: &str,
    by: ReplyDeadline,
    shutdown: &AtomicBool,
) {
    if !http::same_origin(req.header("Origin"), req.header("Host")) {
        return respond(
            &mut stream,
            by,
            "403 Forbidden",
            "text/plain; charset=utf-8",
            b"forbidden",
            &[],
        );
    }
    let Some(session) = authed(wt, &req, handle) else {
        return respond(
            &mut stream,
            by,
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"not found",
            &[],
        );
    };
    let Some(key) = req.header("Sec-WebSocket-Key") else {
        return respond(
            &mut stream,
            by,
            "400 Bad Request",
            "text/plain; charset=utf-8",
            b"bad request",
            &[],
        );
    };
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        ws::accept_key_source(key).as_bytes(),
    );
    let accept = base64::engine::general_purpose::STANDARD.encode(digest.as_ref());
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    // 读超时仍是整体请求期限剩下的那点,`attach::run` 换成对端静默上限。
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    super::attach::run(stream, wt, session, req.rest, shutdown);
}

pub(super) fn authed(
    wt: &WebTerminal,
    req: &Request,
    handle: &str,
) -> Option<summon_core::web_terminal::grants::BrowserSession> {
    let cookie = req.header("Cookie")?;
    let secret = http::cookie_value(cookie, &http::cookie_name(handle))?;
    wt.session(handle, secret)
}

/// 读请求头,上限取终端冻结的那几个数值。期限见 [`http_conn::read_within_deadline`]。
fn read_head(stream: &mut TcpStream, deadline: Instant) -> Head {
    http_conn::read_head(stream, &LIMITS, deadline)
}

/// 把 POST 体读完(进入表单是空的);超过上限或期限已到都放弃这条连接。
fn drain_body(stream: &mut TcpStream, req: &Request, deadline: Instant) -> bool {
    http_conn::read_body(stream, req, MAX_BODY, deadline).is_ok()
}

fn html(stream: &mut TcpStream, by: ReplyDeadline, status: &str, body: &str) {
    respond(
        stream,
        by,
        status,
        "text/html; charset=utf-8",
        body.as_bytes(),
        &[],
    );
}

/// 回一个普通响应(之后连接即关闭)。整个响应受 `by` 的总期限管。
pub(super) fn respond(
    stream: &mut TcpStream,
    by: ReplyDeadline,
    status: &str,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) {
    let mut reply = Reply::new(status, content_type, body)
        .header("Cache-Control", "no-store")
        .header("Referrer-Policy", "no-referrer")
        .header("X-Frame-Options", "DENY");
    for (k, v) in extra {
        reply = reply.header(k, *v);
    }
    // 客户端提前断开不影响服务。
    let _ = http_conn::write_reply(stream, &reply, by);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// 造一对连上的回环连接,把 `raw` 从客户端那一侧写进去。返回服务端那一侧(读它就是读请求)。
    /// 客户端那一侧随返回值一起留着:提前关掉的话服务端读到的是 EOF 而不是「还没读完」。
    fn served(raw: &[u8]) -> (TcpStream, TcpStream) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.write_all(raw).unwrap();
        (server, client)
    }

    fn head_of(raw: &[u8]) -> Head {
        let (mut server, _client) = served(raw);
        read_head(&mut server, Instant::now() + Duration::from_secs(5))
    }

    /// 请求头的两个上限都是交付冻结的数值(spec「链接与凭证」)。头的**字节数**超过 8KB 就放弃
    /// 这条连接;**条数**超过 32 条回 431 —— 静默关掉的话,手机浏览器带一堆头过来时没有任何
    /// 诊断信息。
    #[test]
    fn a_head_over_its_byte_or_header_limits_is_refused_and_one_under_is_served() {
        let with_pad = |pad: usize| {
            let mut raw = b"GET /t/x HTTP/1.1\r\nHost: h\r\nX-Pad: ".to_vec();
            raw.extend(std::iter::repeat_n(b'p', pad));
            raw.extend_from_slice(b"\r\n\r\n");
            raw
        };
        assert!(
            matches!(head_of(&with_pad(MAX_HEAD / 2)), Head::Ok(req) if req.target == "/t/x"),
            "上限之内的头照常读出来"
        );
        assert!(
            matches!(head_of(&with_pad(MAX_HEAD + 2048)), Head::Abandon),
            "头超过 {MAX_HEAD} 字节就放弃这条连接"
        );

        let with_headers = |count: usize| {
            let mut raw = b"GET /t/x HTTP/1.1\r\n".to_vec();
            for i in 0..count {
                raw.extend(format!("X-H{i}: v\r\n").into_bytes());
            }
            raw.extend_from_slice(b"\r\n");
            raw
        };
        assert!(matches!(head_of(&with_headers(MAX_HEADERS)), Head::Ok(_)));
        assert!(
            matches!(
                head_of(&with_headers(MAX_HEADERS + 1)),
                Head::TooManyHeaders
            ),
            "超过 {MAX_HEADERS} 条要能说出是哪种拒绝,才回得了 431"
        );
    }

    /// 两个上限的取值由 spec 冻结(「链接与凭证」:请求头 8KB、POST 体 1KB)。上面两条测试按常量
    /// 自己写边界,改了常量它们跟着改边界、照样绿——取值本身只有这里钉得住。
    #[test]
    fn the_request_limits_keep_the_values_the_spec_froze() {
        assert_eq!(MAX_HEAD, 8 * 1024);
        assert_eq!(MAX_BODY, 1024);
    }

    /// POST 体上限也是冻结的数值:声明超过上限就放弃,正好到上限的照常读完。
    #[test]
    fn a_body_over_its_limit_is_refused_and_one_at_the_limit_is_read() {
        let body_request = |declared: usize, have: usize| Request {
            method: "POST".into(),
            target: "/t/x/enter".into(),
            headers: vec![("Content-Length".into(), declared.to_string())],
            rest: vec![b'x'; have],
        };
        let (mut server, _client) = served(b"");
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(
            drain_body(&mut server, &body_request(MAX_BODY, MAX_BODY), deadline),
            "正好到上限的体照常收"
        );
        assert!(
            !drain_body(&mut server, &body_request(MAX_BODY + 1, 0), deadline),
            "声明超过 {MAX_BODY} 字节就放弃,不去读它"
        );
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

        let clock = Cell::new(t0);
        let mut w = Trickle {
            clock: &clock,
            written: 0,
        };
        assert!(write_within_deadline(
            &mut w,
            &[b'x'; 250],
            t0 + Duration::from_secs(5),
            |_, _| Ok(()),
            || clock.get(),
        ));
        assert_eq!(w.written, 250);
    }

    /// 每条连接一接进来就带写期限:对端一个字节都不收时,写在期限到后出错返回,线程与连接位得以
    /// 归还。不设的话 write 永远阻塞。
    #[test]
    fn every_connection_gets_the_write_deadline_and_blocking_mode() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let accepted = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(e) => panic!("{e}"),
            }
        };
        accepted.set_nonblocking(true).unwrap();
        let timeouts = Timeouts {
            write_stall: std::time::Duration::from_millis(1234),
            ..Timeouts::default()
        };
        prepare(&accepted, &timeouts).unwrap();
        assert_eq!(
            accepted.write_timeout().unwrap(),
            Some(timeouts.write_stall)
        );
        // 阻塞模式下读一个没数据的连接会等到读超时,非阻塞会立刻 WouldBlock。
        accepted
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let started = std::time::Instant::now();
        let _ = (&accepted).read(&mut [0u8; 1]);
        assert!(started.elapsed() >= std::time::Duration::from_millis(40));
    }
}
