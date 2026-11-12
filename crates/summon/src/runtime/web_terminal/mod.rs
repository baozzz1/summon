//! 浏览器内终端服务:独占一个端口,每条连接一个线程。页面与 WebSocket 都只认一次性码换来的
//! 浏览器会话。设计见 `docs/spec/2026-09-22-web-terminal-design.md`。
//!
//! 连接层(读请求、带期限写响应、accept 循环)在 [`crate::runtime::http_conn`],与控制台共用。

mod attach;
mod conn;
mod input;
mod pins;

use std::collections::VecDeque;
use std::net::{Ipv4Addr, TcpListener};
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use summon_core::registry::CaptureMode;
use summon_core::web_terminal::grants::{
    BrowserSession, GrantTarget, TerminalGrants, TerminalMode,
};

use crate::observe::log::log;

/// 同时在处理的连接上限。每条连接一个线程(WebSocket 是三个),超出的直接关掉。
const MAX_CONNECTIONS: usize = 32;

/// 记住最近签发的这么多张终端卡。「换一组新链接」只原地更新记得住的卡。
const REMEMBERED_CARDS: usize = 256;

/// 停机时恢复钉住窗口最多等这么久。tmux 命令没有超时,服务卡住时不能拖住 daemon 退出。
const PIN_RESTORE_BUDGET: Duration = Duration::from_secs(2);

/// 连接上的各个期限。生产取默认值;测试注入短值,不必真等。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timeouts {
    /// 单条连接读完请求(头 + POST 体)的整体期限,见 `http_conn::read_within_deadline`。
    pub(crate) request: Duration,
    /// 一次写整整这么久都写不进去(对端一个字节都不收)就断开这条连接。普通响应与 WebSocket
    /// 都受它管:请求 xterm.js 后不读的客户端,否则能把线程和连接位一直占着。
    pub(crate) write_stall: Duration,
    /// 普通 HTTP 连接从接入到响应写完的总期限,见 `http_conn::write_within_deadline`。WebSocket 升级后
    /// 不受它管。
    pub(crate) response: Duration,
    /// WebSocket 上多久发一次 ping。浏览器自动回 pong,对端静默计时因此一直有字节刷新。
    pub(crate) ping: Duration,
    /// WebSocket 上这么久一个字节都没收到,就当对端已经没了:手机断网、切走不会发 FIN,
    /// 不设的话这条连接会占着窗口钉住和 tmux 客户端直到浏览器会话过期。
    pub(crate) peer_silence: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(10),
            write_stall: Duration::from_secs(10),
            response: Duration::from_secs(30),
            ping: Duration::from_secs(30),
            peer_silence: Duration::from_secs(90),
        }
    }
}

pub(crate) struct TerminalLinks {
    pub(crate) read_only: String,
    pub(crate) operate: String,
    /// 这一组的代次。卡片确实送到之后交给 [`WebTerminal::revoke_superseded`] 作废上一组。
    pub(crate) generation: u64,
}

/// 出不了链接的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkRefusal {
    /// 本机没有收件设备能路由到的地址。
    NoRoutableHost,
    /// 服务只听回环地址,而链接指向的是对外地址:点开也连不上。
    LoopbackBind,
    /// 随机源或内部状态出错。
    Internal,
}

impl LinkRefusal {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::NoRoutableHost => {
                summon_i18n::text!("r2c0c72c63e")
            }
            Self::LoopbackBind => {
                summon_i18n::text!("r0451f3df1a")
            }
            Self::Internal => summon_i18n::text!("rc2e8375c5f"),
        }
    }
}

/// 出链接前的判定:查不到对外地址,或服务只听回环而对外地址不是回环,都不出。
fn link_refusal(loopback_bind: bool, host: Option<Ipv4Addr>) -> Option<LinkRefusal> {
    match host {
        None => Some(LinkRefusal::NoRoutableHost),
        Some(ip) if loopback_bind && !ip.is_loopback() => Some(LinkRefusal::LoopbackBind),
        Some(_) => None,
    }
}

/// 本 daemon 的终端服务:启动时定一次,之后不变。
pub(crate) enum Service {
    Running(Arc<WebTerminal>),
    /// 没在跑,带原因(配置关着,或绑定失败的错误文本)。
    Absent(String),
}

/// 放在进程级而不是 `Core`:要据它决定显不显示「>_」的渲染点(卡片 handler 里的面板刷新、
/// 轮里的回执与菜单卡)都拿不到 `Core`,而服务状态本来就是整个 daemon 一份。只有 daemon 启动时
/// 写一次,测试从不写它。
static SERVICE: OnceLock<Service> = OnceLock::new();

/// daemon 启动时调用一次;之后的调用被忽略。
pub(crate) fn install(service: Service) {
    let _ = SERVICE.set(service);
}

/// 在跑的服务;没在跑时给出原因。
pub(crate) fn service() -> Result<Arc<WebTerminal>, String> {
    match SERVICE.get() {
        Some(Service::Running(wt)) => Ok(Arc::clone(wt)),
        Some(Service::Absent(reason)) => Err(reason.clone()),
        None => Err(summon_i18n::text!("r6cb8e75e20").to_string()),
    }
}

/// 终端服务在跑,且出得了点得开的链接。
pub(crate) fn serving_links() -> bool {
    serves_links(SERVICE.get())
}

/// 只听回环的服务出不了一条点得开的链接:对外地址从不取回环(`pick_public_host`),而回环监听
/// 接不到对外地址上的连接。监听地址在 daemon 启动时定下、之后不变,所以按钮与服务没起时一样
/// 不显示;`/term` 照常回复原因与修法。
fn serves_links(service: Option<&Service>) -> bool {
    matches!(service, Some(Service::Running(wt)) if !wt.loopback_bind)
}

/// 这类会话此刻该不该带「>_ 终端」按钮:会话在 tmux 里,且终端服务出得了链接。
pub(crate) fn offered_for(capture: CaptureMode) -> bool {
    offers(capture, serving_links())
}

/// `offered_for` 的判定本体。无头会话没有终端;服务出不了链接时按钮点了也只能回原因。
pub(crate) fn offers(capture: CaptureMode, serving_links: bool) -> bool {
    capture == CaptureMode::Transcript && serving_links
}

/// daemon 优雅停机时调用:把还钉着的窗口恢复原取值,最多等 `PIN_RESTORE_BUDGET`。连接线程不被
/// join,不在这里恢复的话进程可能先退出,窗口留在 `manual`,下一次钉住又把 `manual` 当原取值
/// 记下,窗口从此一直钉着。
pub(crate) fn restore_pins_on_shutdown() {
    let Some(Service::Running(wt)) = SERVICE.get() else {
        return;
    };
    let wt = Arc::clone(wt);
    let finished = within(PIN_RESTORE_BUDGET, move || {
        wt.pins.release_all(&pins::TmuxWindow)
    });
    match finished {
        Some(n) => log(&summon_i18n::format!("r0e5fa75489")),
        None => log(&summon_i18n::format!(
            "re0b993aeac",
            PIN_RESTORE_BUDGET.as_secs()
        )),
    }
}

/// 在独立线程里跑 `work`,最多等 `budget`;超时返回 None,线程随进程退出。
fn within<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("web-terminal-unpin".to_string())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.recv_timeout(budget).ok()
}

/// 最近签发的终端卡 message_id,旧的在前。进程级、与服务实例分开:记的是飞书上发出去的卡,与
/// 监听和连接无关;「换一组新链接」认卡、引用解析把终端卡挡在 agent 之外,都直接查这里。
/// 控制台卡(`/dash`)用另一份实例,判据相同。
pub(crate) struct CardMemory {
    ids: Mutex<VecDeque<String>>,
}

impl CardMemory {
    pub(crate) const fn new() -> Self {
        Self {
            ids: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn remember(&self, message_id: &str) {
        if message_id.is_empty() {
            return;
        }
        if let Ok(mut ids) = self.ids.lock() {
            if ids.iter().any(|id| id == message_id) {
                return;
            }
            if ids.len() >= REMEMBERED_CARDS {
                ids.pop_front();
            }
            ids.push_back(message_id.to_string());
        }
    }

    pub(crate) fn contains(&self, message_id: &str) -> bool {
        self.ids
            .lock()
            .map(|ids| ids.iter().any(|id| id == message_id))
            .unwrap_or(false)
    }
}

static TERMINAL_CARDS: CardMemory = CardMemory::new();

/// 记下一张私聊发出去的终端卡。
pub(crate) fn remember_card(message_id: &str) {
    TERMINAL_CARDS.remember(message_id);
}

/// 这张卡是不是 summon 私聊发出去的终端卡。转发出去的副本 message_id 不同,认不出来。
pub(crate) fn is_terminal_card(message_id: &str) -> bool {
    TERMINAL_CARDS.contains(message_id)
}

pub(crate) struct WebTerminal {
    grants: Mutex<TerminalGrants>,
    cfg: summon_core::config::WebTerminal,
    active: Arc<AtomicUsize>,
    local_port: u16,
    /// 监听在回环地址上。
    loopback_bind: bool,
    timeouts: Timeouts,
    /// 有网页连着的会话窗口钉住计数;跨连接共享,最后一条断开才恢复。
    pins: pins::WindowPins,
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn token() -> Option<String> {
    let t = crate::runtime::preview_server::new_token();
    (!t.is_empty()).then_some(t)
}

impl WebTerminal {
    pub(crate) fn spawn(
        cfg: &summon_core::config::WebTerminal,
        bind: &str,
        shutdown: Arc<AtomicBool>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::spawn_with(cfg, bind, shutdown, Timeouts::default())
    }

    fn spawn_with(
        cfg: &summon_core::config::WebTerminal,
        bind: &str,
        shutdown: Arc<AtomicBool>,
        timeouts: Timeouts,
    ) -> anyhow::Result<Arc<Self>> {
        let listener = TcpListener::bind((bind, cfg.port))
            .with_context(|| summon_i18n::format!("r5c85f1fce7", cfg.port))?;
        // 非阻塞 accept 才能定期看停机标志。
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let this = Arc::new(Self {
            grants: Mutex::new(TerminalGrants::default()),
            cfg: cfg.clone(),
            active: Arc::new(AtomicUsize::new(0)),
            local_port: local.port(),
            loopback_bind: local.ip().is_loopback(),
            timeouts,
            pins: pins::WindowPins::default(),
        });
        let serve = Arc::clone(&this);
        let active = Arc::clone(&this.active);
        std::thread::Builder::new()
            .name("web-terminal".to_string())
            .spawn(move || {
                let conn_shutdown = Arc::clone(&shutdown);
                crate::runtime::http_conn::accept_loop(
                    listener,
                    active,
                    MAX_CONNECTIONS,
                    shutdown,
                    "web-terminal",
                    move |stream| conn::serve(stream, &serve, &conn_shutdown),
                )
            })?;
        log(&summon_i18n::format!(
            "re75861d2e4",
            this.local_port,
            this.base_url()
                .unwrap_or_else(|| summon_i18n::text!("r429dae7320").to_string())
        ));
        Ok(this)
    }

    fn base_url(&self) -> Option<String> {
        self.cfg
            .base_url_with_host(crate::runtime::preview_server::detect_public_host())
    }

    /// 出一组链接。上一组这时还有效:卡片送到之后才作废(见 [`Self::revoke_superseded`])。
    pub(crate) fn issue(&self, target: GrantTarget) -> Result<TerminalLinks, LinkRefusal> {
        let host = crate::runtime::preview_server::detect_public_host();
        if let Some(refusal) = link_refusal(self.loopback_bind, host) {
            return Err(refusal);
        }
        let base = self
            .cfg
            .base_url_with_host(host)
            .ok_or(LinkRefusal::NoRoutableHost)?;
        let codes = self
            .grants
            .lock()
            .ok()
            .and_then(|mut g| g.issue(target, now_ms(), token))
            .ok_or(LinkRefusal::Internal)?;
        Ok(TerminalLinks {
            read_only: format!("{base}/t/{}", codes.read_only),
            operate: format!("{base}/t/{}", codes.operate),
            generation: codes.generation,
        })
    }

    /// 新一组链接确实送到之后:作废同一收件人在同一 chat 上更早的那些码与已经打开的页面。
    pub(crate) fn revoke_superseded(&self, target: &GrantTarget, generation: u64) {
        if let Ok(mut grants) = self.grants.lock() {
            grants.revoke_superseded(target, generation);
        }
    }

    fn peek(&self, code: &str) -> Option<TerminalMode> {
        self.grants.lock().ok()?.peek(code, now_ms())
    }

    fn consume(&self, code: &str) -> Option<BrowserSession> {
        self.grants.lock().ok()?.consume(code, now_ms(), token)
    }

    /// 终端卡按钮的点击回调:见 [`summon_core::web_terminal::grants::TerminalGrants::activate`]。
    pub(crate) fn activate(&self, code: &str, operator_open_id: &str, allowed: bool) -> bool {
        self.grants
            .lock()
            .is_ok_and(|mut g| g.activate(code, operator_open_id, allowed, now_ms()))
    }

    fn awaiting_activation(&self, code: &str) -> bool {
        self.grants
            .lock()
            .is_ok_and(|g| g.awaiting_activation(code, now_ms()))
    }

    fn session(&self, handle: &str, secret: &str) -> Option<BrowserSession> {
        self.grants
            .lock()
            .ok()?
            .session(handle, secret, now_ms())
            .cloned()
    }

    fn is_live(&self, handle: &str) -> bool {
        self.grants
            .lock()
            .map(|g| g.is_live(handle, now_ms()))
            .unwrap_or(false)
    }
}

#[cfg(test)]
impl WebTerminal {
    /// 出一组码,当作卡片已经送到、收件人也已在卡片上点了按钮(生产里这三步分开:送到了才作废
    /// 上一组,收件人点了才激活)。
    pub(crate) fn issue_codes_for_test(
        &self,
        target: GrantTarget,
    ) -> summon_core::web_terminal::grants::IssuedCodes {
        let codes = self.issue_unconfirmed_codes_for_test(target.clone());
        let mut grants = self.grants.lock().unwrap();
        for code in [&codes.read_only, &codes.operate] {
            assert!(grants.activate(code, &target.recipient_open_id, true, now_ms()));
        }
        codes
    }

    /// 出一组码、当作卡片已经送到,但收件人还没点按钮。
    pub(crate) fn issue_unconfirmed_codes_for_test(
        &self,
        target: GrantTarget,
    ) -> summon_core::web_terminal::grants::IssuedCodes {
        let mut grants = self.grants.lock().unwrap();
        let codes = grants.issue(target.clone(), now_ms(), token).unwrap();
        grants.revoke_superseded(&target, codes.generation);
        codes
    }

    pub(crate) fn local_port(&self) -> u16 {
        self.local_port
    }

    pub(crate) fn spawn_for_test(
        cfg: &summon_core::config::WebTerminal,
        bind: &str,
        shutdown: Arc<AtomicBool>,
        timeouts: Timeouts,
    ) -> anyhow::Result<Arc<Self>> {
        Self::spawn_with(cfg, bind, shutdown, timeouts)
    }

    /// 不开监听的实例,只用来测「监听在不在回环」这类判定。
    fn unbound_for_test(loopback_bind: bool) -> Arc<Self> {
        Arc::new(Self {
            grants: Mutex::new(TerminalGrants::default()),
            cfg: summon_core::config::WebTerminal::default(),
            active: Arc::new(AtomicUsize::new(0)),
            local_port: 0,
            loopback_bind,
            timeouts: Timeouts::default(),
            pins: pins::WindowPins::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Recorder, ThrowawaySession};
    use std::io::{Read, Write};

    fn spawn_local() -> Arc<WebTerminal> {
        spawn_with(Timeouts::default())
    }

    fn spawn_with(timeouts: Timeouts) -> Arc<WebTerminal> {
        let cfg = summon_core::config::WebTerminal {
            enabled: true,
            port: 0,
        };
        WebTerminal::spawn_for_test(
            &cfg,
            "127.0.0.1",
            Arc::new(AtomicBool::new(false)),
            timeouts,
        )
        .unwrap()
    }

    /// 等到服务端把所有连接都处理完(连接位全部归还)。
    fn wait_until_idle(wt: &WebTerminal, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if wt.active.load(Ordering::SeqCst) == 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn links_are_refused_without_a_routable_host_or_when_only_loopback_listens() {
        let lan: Ipv4Addr = "10.1.2.3".parse().unwrap();
        assert_eq!(link_refusal(false, None), Some(LinkRefusal::NoRoutableHost));
        assert_eq!(link_refusal(true, None), Some(LinkRefusal::NoRoutableHost));
        assert_eq!(
            link_refusal(true, Some(lan)),
            Some(LinkRefusal::LoopbackBind)
        );
        assert_eq!(link_refusal(false, Some(lan)), None);
        assert!(LinkRefusal::LoopbackBind
            .reason()
            .contains("preview_server.bind: \"0.0.0.0\""));
    }

    #[test]
    fn the_terminal_is_offered_only_for_tmux_sessions_while_the_service_runs() {
        assert!(offers(CaptureMode::Transcript, true));
        assert!(!offers(CaptureMode::Transcript, false));
        assert!(!offers(CaptureMode::Rpc, true));
        assert!(!offers(CaptureMode::CliSession, true));
    }

    /// 只听回环的服务出不了点得开的链接,按钮与服务没起时一样不显示。
    #[test]
    fn a_loopback_only_listener_offers_no_buttons() {
        let loopback = Service::Running(WebTerminal::unbound_for_test(true));
        let lan = Service::Running(WebTerminal::unbound_for_test(false));
        assert!(!serves_links(Some(&loopback)));
        assert!(serves_links(Some(&lan)));
        assert!(!serves_links(Some(&Service::Absent("关着".into()))));
        assert!(!serves_links(None));
        assert!(!serving_links(), "测试进程从不装终端服务");
    }

    #[test]
    fn a_real_loopback_bind_is_recorded_as_loopback() {
        assert!(spawn_local().loopback_bind);
    }

    #[test]
    fn only_remembered_cards_count_as_ours_and_the_memory_is_bounded() {
        let cards = CardMemory::new();
        cards.remember("om_first");
        assert!(cards.contains("om_first"));
        assert!(!cards.contains("om_forwarded_copy"));
        cards.remember("");
        assert!(!cards.contains(""), "空 id 不记");
        for i in 0..REMEMBERED_CARDS {
            cards.remember(&format!("om_{i}"));
        }
        assert!(!cards.contains("om_first"), "最旧的被挤掉");
        assert!(cards.contains(&format!("om_{}", REMEMBERED_CARDS - 1)));
    }

    /// 优雅停机序列里确实恢复钉住的窗口:收到停机信号之后、写干净退出哨兵之前。
    #[test]
    fn the_daemon_stop_sequence_restores_pinned_windows() {
        let boot = include_str!("../daemon/boot.rs");
        let signal = boot
            .find("shutdown_waiter.wait(&shutdown);")
            .expect("停机等待点");
        let restore = boot
            .find("web_terminal::restore_pins_on_shutdown();")
            .expect("停机时恢复钉住的窗口");
        let marker = boot
            .find("atomic_write(&clean_shutdown_marker()")
            .expect("干净退出哨兵");
        assert!(signal < restore && restore < marker);
    }

    #[test]
    fn the_shutdown_restore_gives_up_after_its_budget() {
        let started = std::time::Instant::now();
        let slow = within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(5));
        });
        assert!(slow.is_none());
        assert!(started.elapsed() < Duration::from_secs(2), "不等慢的那一步");
        assert_eq!(within(Duration::from_secs(2), || 3), Some(3));
    }

    fn target() -> GrantTarget {
        GrantTarget {
            agent: "arcadia".into(),
            chat_id: "oc_1".into(),
            chat_label: "开发群".into(),
            tmux_session: "summon-wt-no-such-session".into(),
            recipient_open_id: "ou_a".into(),
        }
    }

    /// 发一个原始 HTTP 请求,读回整个响应(服务端一律 Connection: close)。
    fn request(wt: &WebTerminal, raw: &str) -> String {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", wt.local_port())).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn get(wt: &WebTerminal, path: &str, extra: &str) -> String {
        request(
            wt,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}\r\n"),
        )
    }

    #[test]
    fn landing_shows_the_mode_without_spending_the_code() {
        let wt = spawn_local();
        let codes = wt.issue_codes_for_test(target());
        let first = get(&wt, &format!("/t/{}", codes.operate), "");
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(first.contains("可操作"));
        assert!(get(&wt, &format!("/t/{}", codes.operate), "").starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn entering_waits_for_the_recipients_click_on_the_card() {
        let wt = spawn_local();
        let t = target();
        let codes = wt.issue_unconfirmed_codes_for_test(t.clone());
        let enter = format!(
            "POST /t/{}/enter HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n",
            codes.operate
        );
        let early = request(&wt, &enter);
        assert!(early.starts_with("HTTP/1.1 409"), "{early}");
        assert!(early.contains("还没有确认是你"), "{early}");

        assert!(!wt.activate(&codes.operate, "ou_someone_else", true));
        assert!(
            request(&wt, &enter).starts_with("HTTP/1.1 409"),
            "别人的点击不算"
        );

        assert!(wt.activate(&codes.operate, &t.recipient_open_id, true));
        let entered = request(&wt, &enter);
        assert!(entered.starts_with("HTTP/1.1 303"), "{entered}");
    }

    #[test]
    fn entering_spends_the_code_and_sets_a_scoped_cookie() {
        let wt = spawn_local();
        let codes = wt.issue_codes_for_test(target());
        let enter = format!(
            "POST /t/{}/enter HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n",
            codes.read_only
        );
        let resp = request(&wt, &enter);
        assert!(resp.starts_with("HTTP/1.1 303"), "{resp}");
        let location = header(&resp, "Location").unwrap();
        let handle = location.trim_start_matches("/t/s/").to_string();
        let cookie = header(&resp, "Set-Cookie").unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        let pair = cookie.split(';').next().unwrap().to_string();

        let page = get(&wt, &location, &format!("Cookie: {pair}\r\n"));
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("只读查看"));
        assert!(
            get(&wt, &format!("/t/s/{handle}"), "").starts_with("HTTP/1.1 404"),
            "没 cookie 不给页面"
        );

        let again = request(&wt, &enter);
        assert!(again.starts_with("HTTP/1.1 404"), "码只能进入一次:{again}");
        assert!(again.contains("链接已失效"));
    }

    #[test]
    fn assets_are_served_from_the_binary_and_everything_else_404s() {
        let wt = spawn_local();
        let js = get(&wt, "/t/assets/xterm.js", "");
        assert!(js.starts_with("HTTP/1.1 200"));
        assert!(js.contains("application/javascript"));
        assert!(get(&wt, "/", "").starts_with("HTTP/1.1 404"));
        assert!(get(&wt, &format!("/t/{}", "a".repeat(32)), "").starts_with("HTTP/1.1 404"));
    }

    /// 头太多的请求要得到答复,而不是一条无声关掉的连接:手机浏览器带一堆头过来时,这是唯一的
    /// 诊断信息(日志里另有一行)。
    #[test]
    fn a_request_with_too_many_headers_is_answered_instead_of_dropped() {
        let wt = spawn_local();
        let extra: String = (0..40).map(|i| format!("X-H{i}: v\r\n")).collect();
        let resp = get(&wt, "/t/assets/xterm.css", &extra);
        assert!(resp.starts_with("HTTP/1.1 431"), "{resp}");
    }

    /// 同时连接上限(spec 冻结的 32)在 accept 那一步就生效:占满之后再来的连接当场被关掉,
    /// 不排队也不占线程,而已经占着位的那条照常等它的请求。取值本身也钉在这里——按常量写边界的
    /// 测试跟着常量走,测不出取值被改。
    #[test]
    fn the_connection_limit_is_enforced_at_accept_and_keeps_its_frozen_value() {
        assert_eq!(MAX_CONNECTIONS, 32);
        let wt = spawn_local();
        let addr = ("127.0.0.1", wt.local_port());
        // 占满全部连接位:每条只连不发,服务端线程停在读请求头上。
        let held: Vec<std::net::TcpStream> = (0..MAX_CONNECTIONS)
            .map(|_| std::net::TcpStream::connect(addr).unwrap())
            .collect();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while wt.active.load(Ordering::SeqCst) < MAX_CONNECTIONS
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            wt.active.load(Ordering::SeqCst),
            MAX_CONNECTIONS,
            "前提:连接位已占满"
        );

        let mut extra = std::net::TcpStream::connect(addr).unwrap();
        extra
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0u8; 1];
        assert!(
            matches!(extra.read(&mut byte), Ok(0)),
            "超出上限的连接当场关掉"
        );
        let mut first = &held[0];
        first
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert!(first.read(&mut byte).is_err(), "占着位的连接照常等它的请求");
        drop(held);
    }

    #[test]
    fn every_response_forbids_caching_and_referrers() {
        let wt = spawn_local();
        let codes = wt.issue_codes_for_test(target());
        let resp = get(&wt, &format!("/t/{}", codes.read_only), "");
        assert_eq!(header(&resp, "Cache-Control").as_deref(), Some("no-store"));
        assert_eq!(
            header(&resp, "Referrer-Policy").as_deref(),
            Some("no-referrer")
        );
    }

    /// 挂在这个会话上的客户端数,只读与可写都算。
    pub(super) fn clients_on(tmux: &crate::session::tmux::Tmux) -> usize {
        let out = std::process::Command::new("tmux")
            .args(["list-clients", "-t", &format!("={}", tmux.session)])
            .args(["-F", "#{client_pid}"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    }

    /// 走完「进入 → 带 cookie 升级」,返回已完成 101 握手的连接。
    fn ws_connect(wt: &WebTerminal, code: &str) -> std::net::TcpStream {
        let enter = request(
            wt,
            &format!(
                "POST /t/{code}/enter HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n"
            ),
        );
        let location = header(&enter, "Location").unwrap();
        let pair = header(&enter, "Set-Cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let host = format!("127.0.0.1:{}", wt.local_port());
        let mut s = std::net::TcpStream::connect(&host).unwrap();
        s.write_all(
            format!(
                "GET {location}/ws HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\n\
Cookie: {pair}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
        // 逐字节读到头部结束,别把紧跟其后的首帧吞掉。
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            assert_eq!(s.read(&mut byte).unwrap(), 1, "握手响应没读完就断了");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        // RFC 6455 §1.3 的示例握手:这个 key 的 accept 值是固定的。
        assert!(
            head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            "{head}"
        );
        s
    }

    /// 像本地终端那样接一个普通客户端把窗口撑到 `cols`x`rows`,再结束这个客户端进程。
    /// 必须是客户端撑的:`resize-window` 会顺手改掉 manual 尺寸,测不出「钉住弹回创建尺寸」。
    fn grow_window_with_a_local_client(tmux: &crate::session::tmux::Tmux, cols: u16, rows: u16) {
        let size = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = nix::pty::openpty(Some(&size), None).unwrap();
        let slave = pty.slave.try_clone().unwrap();
        let _master = pty.master.try_clone().unwrap();
        drop(pty);
        let mut cmd = tmux.attach_command(false);
        cmd.env("TERM", "xterm-256color")
            .env_remove("TMUX")
            .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
            .stdout(std::process::Stdio::from(slave.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(slave));
        let mut client = cmd.spawn().unwrap();
        std::thread::sleep(Duration::from_millis(800));
        // 只结束这个客户端进程。
        let _ = client.kill();
        let _ = client.wait();
        std::thread::sleep(Duration::from_millis(300));
    }

    /// 在 `within` 内读这条 WebSocket,拼出所有二进制帧的负载(服务端的帧不带掩码)。
    fn screen_output(s: &mut std::net::TcpStream, within: Duration) -> Vec<u8> {
        let started = std::time::Instant::now();
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        while started.elapsed() < within {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
        }
        let mut payload = Vec::new();
        let mut at = 0;
        while at + 2 <= raw.len() {
            let opcode = raw[at] & 0x0f;
            let (len, head) = match raw[at + 1] & 0x7f {
                126 if at + 4 <= raw.len() => {
                    (u16::from_be_bytes([raw[at + 2], raw[at + 3]]) as usize, 4)
                }
                127 if at + 10 <= raw.len() => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&raw[at + 2..at + 10]);
                    (u64::from_be_bytes(b) as usize, 10)
                }
                n if n < 126 => (n as usize, 2),
                _ => break,
            };
            let Some(body) = raw.get(at + head..at + head + len) else {
                break;
            };
            if opcode == 2 {
                payload.extend_from_slice(body);
            }
            at += head + len;
        }
        payload
    }

    /// 浏览器发的帧必须带掩码:复用服务端的编码再补掩码位与全零掩码(全零掩码 = 原文)。
    /// 测试载荷都不到 126 字节,长度就在第 2 字节。
    fn masked(mut frame: Vec<u8>) -> Vec<u8> {
        assert!(frame[1] < 126);
        frame[1] |= 0x80;
        frame.splice(2..2, [0, 0, 0, 0]);
        frame
    }

    /// 页面发的按键:二进制帧。
    fn keys(text: &str) -> Vec<u8> {
        masked(summon_core::web_terminal::ws::encode_binary(
            text.as_bytes(),
        ))
    }

    /// 页面发的控制消息:文本帧。
    fn control(json: &str) -> Vec<u8> {
        masked(summon_core::web_terminal::ws::encode_text(json))
    }

    /// 真 tmux 端到端:可操作页的输入(含多字节 UTF-8)进得去、只读页的进不去、窗口尺寸不被浏览器
    /// 改、断开后恢复。窗口先被本地客户端撑离创建尺寸(agent 会话的常态):钉住不许把它弹回去。
    #[test]
    #[ignore]
    fn operate_input_reaches_the_pane_and_read_only_input_does_not() {
        let session = ThrowawaySession::start("summon-wt-e2e");
        let tmux = &session.0;
        let window = tmux.current_window().unwrap();
        let created = window.geometry().unwrap();
        grow_window_with_a_local_client(tmux, 150, 40);
        let before = window.geometry().unwrap();
        assert_eq!(before.cols, 150, "测试前提:窗口被撑离创建尺寸 {created:?}");
        assert_eq!(window.size_option().unwrap(), None, "测试前提:窗口级没设");
        let wt = spawn_local();
        let mut t = target();
        t.tmux_session = tmux.session.clone();

        let codes = wt.issue_codes_for_test(t);
        let mut ro = ws_connect(&wt, &codes.read_only);
        ro.write_all(&keys("echo RO_$((6*7))\r")).unwrap();
        let mut op = ws_connect(&wt, &codes.operate);
        op.write_all(&keys("echo OP_$((6*7))\r")).unwrap();
        op.write_all(&keys("echo 中文\r")).unwrap();
        let shown = screen_output(&mut ro, Duration::from_millis(1500));
        let pane = tmux.capture().unwrap();
        assert!(pane.contains("OP_42"), "可操作输入应被执行:{pane}");
        assert!(
            pane.lines().any(|l| l.trim() == "中文"),
            "多字节输入应逐字节到达并被执行:{pane}"
        );
        assert!(!pane.contains("RO_"), "只读输入不应到达 pane:{pane}");
        assert!(
            shown.windows("中文".len()).any(|w| w == "中文".as_bytes()),
            "页面收到的画面按 UTF-8 画出多字节字符(测试进程没有区域变量):{}",
            String::from_utf8_lossy(&shown)
        );
        assert_eq!(window.geometry().unwrap(), before, "钉住不改窗口尺寸");
        assert_eq!(window.size_option().unwrap().as_deref(), Some("manual"));

        drop(ro);
        drop(op);
        std::thread::sleep(Duration::from_millis(3000));
        assert_eq!(
            window.size_option().unwrap(),
            None,
            "最后一个页面断开后恢复"
        );
        assert_eq!(window.geometry().unwrap(), before, "断开后窗口尺寸不变");
    }

    /// 真 tmux:可操作页上敲 tmux 前缀键加命令,够不到 tmux 客户端这一层:既销毁不了别的会话,
    /// 也切不走自己的客户端——按键只作为字节进了本会话的窗格(shell 把它当成一条找不到的命令)。
    #[test]
    #[ignore]
    fn prefix_commands_typed_on_the_operate_page_never_reach_the_tmux_client() {
        let session = ThrowawaySession::start("summon-wt-prefix");
        let victim = ThrowawaySession::start("summon-wt-victim");
        let tmux = &session.0;
        let wt = spawn_local();
        let mut t = target();
        t.tmux_session = tmux.session.clone();
        let codes = wt.issue_codes_for_test(t);
        let mut op = ws_connect(&wt, &codes.operate);
        std::thread::sleep(Duration::from_millis(800));
        let attached = clients_on(tmux);
        // 命令名拼出来:守护测试按子串扫描全仓源码,销毁命令的字面串只许出现在白名单文件里。
        let destroy = format!("kill{}session", '-');
        let victim_target = format!("={}", victim.0.session);
        // 按人手的节奏分开发:tmux 的命令提示符是排队打开的,和 `:` 同一批到达的字节会先落进窗格,
        // 一口气发出去就测不出客户端到底接不接前缀键。
        for command in [
            format!("{destroy} -t {victim_target}"),
            format!("switch-client -t {victim_target}"),
        ] {
            for part in ["\x02", ":", &command, "\r"] {
                op.write_all(&keys(part)).unwrap();
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
        let victim_alive = victim.0.has_session();
        let on_target = clients_on(tmux);
        let on_victim = clients_on(&victim.0);
        drop(op);
        drop(victim);
        assert_eq!(attached, 1, "前提:网页的 tmux 客户端已挂上目标会话");
        assert!(victim_alive, "别的会话不该被网页里敲的 tmux 命令销毁");
        assert_eq!(on_victim, 0, "客户端不该被切到别的会话");
        assert_eq!(on_target, 1, "客户端仍在目标会话上");
    }

    /// `n` 个 SGR 滚轮事件(64 往上、65 往下),落在 (`x`, `y`)。
    fn wheel(button: u8, n: usize, (x, y): (u16, u16)) -> Vec<u8> {
        format!("\x1b[<{button};{x};{y}M").repeat(n).into_bytes()
    }

    /// 窗格正中那一格(从 1 起,偶数取靠前的),滚轮事件落在这里。
    fn centre(tmux: &crate::session::tmux::Tmux) -> (u16, u16) {
        let (_, mouse) = tmux.pane_mouse().unwrap();
        (mouse.width.div_ceil(2), mouse.height.div_ceil(2))
    }

    /// 等服务端把断开的页面都收完尾(含往下翻回去)。不等的话,翻回去的滚轮事件会在记录程序退出
    /// 之后才到,落进 shell 的输入行,测试会话就退不掉了。
    fn settle(wt: &WebTerminal) {
        assert!(
            wait_until_idle(wt, Duration::from_secs(10)),
            "页面 10 秒内没收完尾"
        );
    }

    /// 起一个指向这个会话的网页终端服务并出一组码。
    fn pages_for(
        tmux: &crate::session::tmux::Tmux,
    ) -> (
        Arc<WebTerminal>,
        summon_core::web_terminal::grants::IssuedCodes,
    ) {
        let wt = spawn_local();
        let mut t = target();
        t.tmux_session = tmux.session.clone();
        let codes = wt.issue_codes_for_test(t);
        (wt, codes)
    }

    /// 真 tmux 端到端:窗格里的程序像 Claude Code 那样开着鼠标跟踪与 SGR 汇报。只读页的滚动以
    /// 服务端生成的滚轮事件到达,它发的按键一个字节都不到;可操作页滚动与按键都到。
    #[test]
    #[ignore]
    fn both_pages_scroll_and_only_the_operate_page_types() {
        let rec = Recorder::start("summon-wt-scroll", true);
        let tmux = rec.tmux();
        let (wt, codes) = pages_for(tmux);
        let at = centre(tmux);

        let mut ro = ws_connect(&wt, &codes.read_only);
        ro.write_all(&control(r#"{"scroll": -2}"#)).unwrap();
        ro.write_all(&keys("echo hi\r")).unwrap();
        // 按键之后再滚一次:收到它就说明夹在中间的按键已经被处理过(并丢掉)了。
        ro.write_all(&control(r#"{"scroll": -1}"#)).unwrap();
        let read_only = [wheel(64, 2, at), wheel(64, 1, at)].concat();
        let got_ro = rec.wait_for(&read_only, Duration::from_secs(3));

        let mut op = ws_connect(&wt, &codes.operate);
        op.write_all(&control(r#"{"scroll": 3}"#)).unwrap();
        op.write_all(&keys("echo hi\r")).unwrap();
        let operate = [read_only.clone(), wheel(65, 3, at), b"echo hi\r".to_vec()].concat();
        let got_op = rec.wait_for(&operate, Duration::from_secs(3));
        drop(ro);
        drop(op);
        settle(&wt);

        let show = |b: &[u8]| String::from_utf8_lossy(b).escape_debug().to_string();
        assert_eq!(
            got_ro,
            read_only,
            "只读页:只有滚轮事件,没有按键:{}",
            show(&got_ro)
        );
        assert_eq!(got_op, operate, "可操作页:{}", show(&got_op));
    }

    /// 真 tmux:窗格里的程序没开鼠标跟踪,滚动一个字节都不送——那些字节它不当鼠标事件读,就成了
    /// 输入框里的字。最后发的按键到了,说明前面的滚动已经处理过。
    #[test]
    #[ignore]
    fn a_scroll_sends_nothing_to_a_pane_that_does_not_track_the_mouse() {
        let rec = Recorder::start("summon-wt-nomouse", false);
        let tmux = rec.tmux();
        let (wt, codes) = pages_for(tmux);
        let mut ro = ws_connect(&wt, &codes.read_only);
        ro.write_all(&control(r#"{"scroll": -2}"#)).unwrap();
        let got_ro = rec.wait_for(b"", Duration::from_millis(1000));
        let mut op = ws_connect(&wt, &codes.operate);
        op.write_all(&control(r#"{"scroll": -2}"#)).unwrap();
        op.write_all(&keys("k")).unwrap();
        let got_op = rec.wait_for(b"k", Duration::from_secs(3));
        drop(ro);
        drop(op);
        settle(&wt);
        assert_eq!(got_ro, b"", "只读页的滚动");
        assert_eq!(got_op, b"k", "可操作页的滚动不送,按键照常到");
    }

    /// 真 tmux:窗格在 copy-mode 里(本地有人在滚屏),两种页面的滚动都不送,也不替他退出
    /// copy-mode;程序什么都没收到。之后可操作页的按键照常退出 copy-mode 送到(连接是活的)。
    #[test]
    #[ignore]
    fn a_scroll_leaves_a_pane_in_copy_mode_alone() {
        let rec = Recorder::start("summon-wt-copymode", true);
        let tmux = rec.tmux();
        let (wt, codes) = pages_for(tmux);
        let mut ro = ws_connect(&wt, &codes.read_only);
        let mut op = ws_connect(&wt, &codes.operate);
        std::thread::sleep(Duration::from_millis(500));
        let entered = std::process::Command::new("tmux")
            .args(["copy-mode", "-t", &format!("={}:", tmux.session)])
            .status()
            .unwrap();
        ro.write_all(&control(r#"{"scroll": -2}"#)).unwrap();
        op.write_all(&control(r#"{"scroll": 3}"#)).unwrap();
        let got = rec.wait_for(b"", Duration::from_millis(1000));
        let still_in_mode = tmux.pane_mouse().unwrap().1.in_mode;
        op.write_all(&keys("k")).unwrap();
        let after = rec.wait_for(b"k", Duration::from_secs(3));
        drop(ro);
        drop(op);
        settle(&wt);
        assert!(entered.success(), "前提:进了 copy-mode");
        assert_eq!(got, b"", "程序什么都没收到");
        assert!(still_in_mode, "copy-mode 不该被滚动取消");
        assert_eq!(after, b"k");
    }

    /// 真 tmux:页面往上翻过、连接结束时,服务端把这条连接净往上翻的格数往下翻回去——Claude Code
    /// 翻在历史里时新输出来了也停在原处,注入闸读到的就是历史。净往下翻的连接结束时什么都不补。
    #[test]
    #[ignore]
    fn a_page_that_scrolled_up_is_scrolled_back_down_when_it_disconnects() {
        let rec = Recorder::start("summon-wt-restore", true);
        let tmux = rec.tmux();
        let (wt, codes) = pages_for(tmux);
        let at = centre(tmux);
        let mut ro = ws_connect(&wt, &codes.read_only);
        ro.write_all(&control(r#"{"scroll": -3}"#)).unwrap();
        let up = wheel(64, 3, at);
        let got_up = rec.wait_for(&up, Duration::from_secs(3));
        drop(ro);
        let restored = [up.clone(), wheel(65, 3, at)].concat();
        let got_restored = rec.wait_for(&restored, Duration::from_secs(5));

        let mut op = ws_connect(&wt, &codes.operate);
        op.write_all(&control(r#"{"scroll": 2}"#)).unwrap();
        let down = [restored.clone(), wheel(65, 2, at)].concat();
        let got_down = rec.wait_for(&down, Duration::from_secs(3));
        drop(op);
        settle(&wt);
        let settled = rec.wait_for(b"", Duration::from_millis(500));

        let show = |b: &[u8]| String::from_utf8_lossy(b).escape_debug().to_string();
        assert_eq!(got_up, up, "{}", show(&got_up));
        assert_eq!(
            got_restored,
            restored,
            "断开后往下翻回 3 格:{}",
            show(&got_restored)
        );
        assert_eq!(got_down, down, "{}", show(&got_down));
        assert_eq!(settled, down, "净往下的连接断开时不补:{}", show(&settled));
    }

    /// 真 tmux:网页(只读与可操作)开着都不算「有人在键盘前」,注入闸照常做空输入框探测;再挂一个
    /// 本地普通客户端,它照常算数。网页的显示客户端不标只读,认它靠的是 summon 登记的进程号。
    #[test]
    #[ignore]
    fn open_pages_are_not_counted_as_someone_at_the_keyboard_but_a_local_client_is() {
        let session = ThrowawaySession::start("summon-wt-gate");
        let tmux = &session.0;
        let wt = spawn_local();
        let mut t = target();
        t.tmux_session = tmux.session.clone();
        let codes = wt.issue_codes_for_test(t);
        let ro = ws_connect(&wt, &codes.read_only);
        let op = ws_connect(&wt, &codes.operate);
        std::thread::sleep(Duration::from_millis(800));
        let pages = clients_on(tmux);
        let with_pages = tmux.writable_clients().unwrap();
        let local = crate::test_support::AttachedClient::spawn(tmux, false);
        std::thread::sleep(Duration::from_millis(800));
        let with_local = tmux.writable_clients().unwrap();
        drop(local);
        drop(ro);
        drop(op);
        assert_eq!(pages, 2, "前提:两个网页的 tmux 客户端都已挂上");
        assert_eq!(with_pages, 0, "网页不算有人在键盘前");
        assert_eq!(with_local, 1, "本地普通客户端照常算数");
    }

    /// 真 tmux:网页(只读与可操作)开着时,同一 tmux 服务上别的程序不带 `-c` 的 `send-keys` 照常
    /// 送达。不带 `-c` 时目标客户端是全服务最近活跃的那个,刚挂上的网页客户端正是它;tmux 3.7 起
    /// 那个客户端是只读的就整条拒绝。
    #[test]
    #[ignore]
    fn other_tools_send_keys_still_work_while_a_page_is_open() {
        let session = ThrowawaySession::start("summon-wt-third");
        let other = ThrowawaySession::start("summon-wt-other");
        let tmux = &session.0;
        let wt = spawn_local();
        let mut t = target();
        t.tmux_session = tmux.session.clone();
        let codes = wt.issue_codes_for_test(t);
        let other_target = format!("={}:", other.0.session);
        let mut results = Vec::new();
        for (mode, code) in [("ro", &codes.read_only), ("op", &codes.operate)] {
            let page = ws_connect(&wt, code);
            std::thread::sleep(Duration::from_millis(800));
            let attached = clients_on(tmux);
            let sent = std::process::Command::new("tmux")
                .args([
                    "send-keys",
                    "-t",
                    &other_target,
                    "-l",
                    &format!("THIRD_{mode}"),
                ])
                .output()
                .unwrap();
            drop(page);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while clients_on(tmux) != 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            results.push((
                mode,
                attached,
                sent.status.success(),
                String::from_utf8_lossy(&sent.stderr).into_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(300));
        let pane = other.0.capture().unwrap();
        for (mode, attached, ok, stderr) in results {
            assert_eq!(attached, 1, "前提:{mode} 页的 tmux 客户端已挂上");
            assert!(ok, "{mode} 页开着时第三方 send-keys 被拒:{stderr}");
            assert!(pane.contains(&format!("THIRD_{mode}")), "{mode}:{pane}");
        }
    }

    /// 真 tmux:会话开着 destroy-unattached 就不接——网页是最后一个客户端时,断开会连带销毁会话。
    /// 用 keep-last:它对不在会话组里的会话不会真的销毁,测试会话安全;而取值不是 off 就该拒。
    #[test]
    #[ignore]
    fn a_session_that_dies_with_its_last_client_is_never_attached() {
        let session = ThrowawaySession::start("summon-wt-dua");
        let tmux = &session.0;
        let set = std::process::Command::new("tmux")
            .args([
                "set-option",
                "-t",
                &format!("={}:", tmux.session),
                "destroy-unattached",
                "keep-last",
            ])
            .status()
            .unwrap();
        assert!(set.success());
        assert!(tmux.destroyed_on_last_detach().unwrap());
        let wt = spawn_local();
        let mut t = target();
        t.tmux_session = tmux.session.clone();
        let codes = wt.issue_codes_for_test(t);

        let mut s = ws_connect(&wt, &codes.operate);
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest);
        let rest = String::from_utf8_lossy(&rest);
        assert!(
            rest.contains("destroy-unattached"),
            "应当说明为什么不接:{rest}"
        );
        assert_eq!(clients_on(tmux), 0, "不该挂上任何客户端");
        assert!(tmux.has_session());
        assert_eq!(
            tmux.current_window().unwrap().size_option().unwrap(),
            None,
            "拒接时不钉窗口"
        );
    }

    /// 真 tmux:页面往上翻过、断开时屏上是一张待选的菜单,翻回去整个不做——滚轮可能挪动菜单光标,
    /// 之后飞书卡片上选的就成了另一项。
    #[test]
    #[ignore]
    fn a_page_disconnecting_over_a_menu_does_not_scroll_back() {
        let menu = r" Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel\n";
        let rec = Recorder::start_showing("summon-wt-menu", true, menu);
        let tmux = rec.tmux();
        let (wt, codes) = pages_for(tmux);
        let at = centre(tmux);
        let (pane, _) = tmux.pane_mouse().unwrap();
        let screen = pane.capture_visible().unwrap();
        let mut ro = ws_connect(&wt, &codes.read_only);
        ro.write_all(&control(r#"{"scroll": -3}"#)).unwrap();
        let up = wheel(64, 3, at);
        let got_up = rec.wait_for(&up, Duration::from_secs(3));
        drop(ro);
        settle(&wt);
        let settled = rec.wait_for(b"", Duration::from_millis(500));
        assert!(
            summon_core::interactive::parse_interactive(&screen).is_some(),
            "前提:屏上认得出菜单:{screen}"
        );
        assert_eq!(got_up, up);
        assert_eq!(settled, up, "屏上有菜单,断开后一格都不翻回");
    }

    /// 真 xterm.js(需要 node):页面给终端装上 `quiet` 之后,tmux 客户端接入时发来的终端查询
    /// 得不到任何答复,鼠标与焦点汇报模式开不起来,其余模式照常生效。可操作页的输入直接进窗格,
    /// xterm.js 的答复与汇报一旦发出,就成了 agent 输入框里的乱码。
    #[test]
    #[ignore]
    fn the_page_terminal_answers_no_query_and_reports_no_mouse() {
        let page = summon_core::web_terminal::page::terminal_page(
            "h",
            summon_core::web_terminal::grants::TerminalMode::Operate,
            "a",
            "b",
        );
        let start = page.find("function quiet(term) {").expect("页面里的 quiet");
        let end = start + page[start..].find("\n}\n").expect("quiet 的结尾") + 3;
        let quiet = &page[start..end];
        let xterm =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/web_terminal/xterm.js");
        // tmux 3.7b 客户端接入时实测发出的查询与模式(`mouse on`,只读与普通客户端相同),
        // 外加几种别的查询。
        let script = format!(
            r#"globalThis.window = globalThis; globalThis.self = globalThis;
const {{ Terminal }} = require({xterm:?});
const term = new Terminal({{}});
{quiet}
quiet(term);
const out = [];
term.onData((d) => out.push(d));
term.write("\x1b[c\x1b[>c\x1b[>q\x1b[?996n\x1b[18t\x1b[14t\x1b[6n\x1b[?6n\x1b]11;?\x1b\\"
  + "\x1b[?2004$p\x1bP$qm\x1b\\\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h"
  + "\x1b[?2004h\x1b[?7l\x1b[?1003;7h", () => setTimeout(() => {{
  console.log(JSON.stringify({{ out, mouse: term.modes.mouseTrackingMode,
    focus: term.modes.sendFocusMode, paste: term.modes.bracketedPasteMode,
    wrap: term.modes.wraparoundMode }}));
}}, 50));
"#,
            xterm = xterm.to_string_lossy(),
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("quiet.js");
        std::fs::write(&file, script).unwrap();
        let run = std::process::Command::new("node")
            .arg(&file)
            .output()
            .expect("需要 node");
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(
            run.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let seen: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(seen["out"], serde_json::json!([]), "查询不作答:{seen}");
        assert_eq!(seen["mouse"], "none", "{seen}");
        assert_eq!(seen["focus"], false, "{seen}");
        assert_eq!(seen["paste"], true, "别的模式照常生效:{seen}");
        assert_eq!(
            seen["wrap"], true,
            "与汇报模式同一串里的别的模式照常生效:{seen}"
        );
    }

    /// 真 JS(需要 node):页面里的 `scroller` 按 xterm.js 对开着鼠标汇报的程序的换算,一个滚轮事件
    /// 至多出一格,至多每 50ms 发一条,换方向丢掉没发出去的;捏合、横向、终端区域还能滚时不拦;
    /// 竖着拖(手指往下是往上翻)一行高一格,横着拖或区域还能滚时整段留给浏览器。时钟与定时器由
    /// 测试驱动。
    #[test]
    #[ignore]
    fn the_page_turns_wheel_and_swipes_into_coalesced_scroll_messages() {
        let page = summon_core::web_terminal::page::terminal_page(
            "h",
            summon_core::web_terminal::grants::TerminalMode::ReadOnly,
            "a",
            "b",
        );
        let start = page.find("function scroller(").expect("页面里的 scroller");
        let end = start + page[start..].find("\n}\n").expect("scroller 的结尾") + 3;
        let scroller = &page[start..end];
        // 一行 20px,终端 30 行。`can` 是终端区域自己还能不能滚。
        let script = format!(
            r#"{scroller}
let t = 0, can = false;
const posts = [], timers = [], taken = [], swipes = [];
const s = scroller((n) => posts.push([t, n]), () => 20, () => 30, () => t,
  (f, ms) => timers.push([t + ms, f]), () => can);
function at(to) {{
  timers.sort((a, b) => a[0] - b[0]);
  while (timers.length && timers[0][0] <= to) {{
    const [due, f] = timers.shift();
    t = due;
    f();
  }}
  t = to;
}}
const wheel = (deltaY, deltaMode, extra) => s.wheel({{ deltaY, deltaMode, deltaX: 0, ...extra }});
taken.push(wheel(3, 1));
at(10); wheel(3, 1);
at(20); wheel(3, 1);
at(200); wheel(100, 0);
at(300); for (let i = 0; i < 6; i++) wheel(10, 0);
at(310); wheel(10, 0);
at(400); wheel(1, 2);
at(500);
taken.push(wheel(100, 0, {{ ctrlKey: true }}));
taken.push(wheel(10, 0, {{ deltaX: 40 }}));
taken.push(wheel(100, 0, {{ shiftKey: true }}));
can = true; taken.push(wheel(-100, 0)); can = false;
at(600); wheel(100, 0);
at(610); wheel(100, 0);
at(620); wheel(-100, 0);
at(800); s.touchStart(50, 100); swipes.push(s.touchMove(50, 130));
at(900); swipes.push(s.touchMove(51, 160)); s.touchEnd();
at(1000); s.touchStart(50, 100); swipes.push(s.touchMove(80, 105)); swipes.push(s.touchMove(120, 160));
s.touchEnd();
can = true; s.touchStart(50, 100); swipes.push(s.touchMove(50, 130)); s.touchEnd(); can = false;
at(1100);
console.log(JSON.stringify({{ posts, taken, swipes }}));
"#
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("scroller.js");
        std::fs::write(&file, script).unwrap();
        let run = std::process::Command::new("node")
            .arg(&file)
            .output()
            .expect("需要 node");
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(
            run.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let seen: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(
            seen["posts"],
            serde_json::json!([
                [0, 1],    // 按行:3 行也只出一格,前面没发过就立刻发
                [50, 2],   // 50ms 内的两下合并成一条,到点才发
                [200, 1],  // 按像素的滚轮:100px / 20px = 5 行,只出一格
                [310, 1],  // 触控板:10px 是 0.5 行再乘 0.3,攒到第 7 下才满一格
                [400, 1],  // 按页:一屏 30 行,也只出一格
                [600, 1],  // 往下一格;10ms 后又一格还没发,20ms 后换成往上
                [650, -1], // 换方向:没发出去的那格整格丢掉,只发往上的一格
                [800, -1], // 手指往下 30px:往上 1.5 格,出 1 格、留 0.5
                [900, -2], // 再往下 30px:攒着的 0.5 加 1.5
            ]),
            "{seen}"
        );
        assert_eq!(
            seen["taken"],
            serde_json::json!([true, false, false, false, false]),
            "捏合、横向、shift、区域还能滚:不拦"
        );
        assert_eq!(
            seen["swipes"],
            serde_json::json!([true, true, false, false, false]),
            "竖着拖归滚动;第一下横着拖整段留给浏览器;区域还能滚时也留给浏览器"
        );
    }

    fn header(resp: &str, name: &str) -> Option<String> {
        resp.split("\r\n\r\n").next()?.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }

    /// slowloris 防护:一个整体期限管到底,不是「每来一个字节就重置计时」。用一个 500ms 的短
    /// 期限起服务,客户端每 100ms 才挤一个字节——不到半句请求就该被服务端主动切断,而不是等
    /// 40 字节的完整请求线以 4 秒trickle 完才有动静。
    #[test]
    fn a_trickling_client_is_cut_off_by_the_overall_deadline() {
        let cfg = summon_core::config::WebTerminal {
            enabled: true,
            port: 0,
        };
        let wt = WebTerminal::spawn_for_test(
            &cfg,
            "127.0.0.1",
            Arc::new(AtomicBool::new(false)),
            Timeouts {
                request: Duration::from_millis(500),
                ..Timeouts::default()
            },
        )
        .unwrap();
        let mut s = std::net::TcpStream::connect(("127.0.0.1", wt.local_port())).unwrap();
        let full = b"GET /t/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let start = std::time::Instant::now();
        let mut cut_off = false;
        for &b in full {
            if s.write_all(&[b]).is_err() {
                cut_off = true;
                break;
            }
            // 短读超时探测:服务端一断开,本地就能读到 EOF(Ok(0))而不必等下一次写才发现。
            let _ = s.set_read_timeout(Some(Duration::from_millis(30)));
            let mut peek = [0u8; 1];
            if matches!(s.read(&mut peek), Ok(0)) {
                cut_off = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            cut_off,
            "整体期限到了服务端该主动断开这条连接,而不是每来一个字节就重置计时、\
             无限期陪着慢速客户端耗着一个连接位"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "该在期限(500ms)附近就被切断,不该拖到完整请求(在 100ms/字节下要 4 秒)trickle 完"
        );
    }

    /// 请求 488KB 的 xterm.js 后一个字节都不读的客户端:服务端写不进去,到写期限就放弃这条连接、
    /// 归还连接位。没有写期限的话 write 永远阻塞,32 条这样的连接就占满全部连接位。
    ///
    /// macOS 的回环连接会把两端缓冲自动放大到装得下整个文件(实测约 660KB),这里的写根本不阻塞,
    /// 本测试在这台机器上不设写期限也会通过;接收缓冲设小就有效的系统(Linux)上它才起作用。
    /// 写期限本身由 `conn` 里的 `every_connection_gets_the_write_deadline_and_blocking_mode` 钉住。
    #[test]
    fn a_client_that_never_reads_the_response_gives_its_slot_back() {
        use nix::sys::socket::{
            connect, setsockopt, socket, sockopt, AddressFamily, SockFlag, SockType, SockaddrIn,
        };
        use std::os::fd::AsRawFd;
        let wt = spawn_with(Timeouts {
            write_stall: Duration::from_millis(300),
            ..Timeouts::default()
        });
        // 接收缓冲在连接前就设小:窗口一开始就小,服务端很快写满。
        let fd = socket(
            AddressFamily::Inet,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .unwrap();
        setsockopt(&fd, sockopt::RcvBuf, &1024).unwrap();
        connect(
            fd.as_raw_fd(),
            &SockaddrIn::new(127, 0, 0, 1, wt.local_port()),
        )
        .unwrap();
        let mut s = std::net::TcpStream::from(fd);
        s.write_all(b"GET /t/assets/xterm.js HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        assert!(
            wait_until_idle(&wt, Duration::from_secs(5)),
            "客户端不读,服务端也该在写期限后放手"
        );
        drop(s);
    }

    /// 走完「进入」,返回 (页面地址, cookie 键值对)。
    fn enter(wt: &WebTerminal, code: &str) -> (String, String) {
        let resp = request(
            wt,
            &format!(
                "POST /t/{code}/enter HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n"
            ),
        );
        let location = header(&resp, "Location").unwrap();
        let pair = header(&resp, "Set-Cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        (location, pair)
    }

    /// 别的源的页面拿着(被带上的)cookie 也升级不了。
    #[test]
    fn a_socket_opened_from_another_origin_is_forbidden() {
        let wt = spawn_local();
        let codes = wt.issue_codes_for_test(target());
        let (location, pair) = enter(&wt, &codes.operate);
        let host = format!("127.0.0.1:{}", wt.local_port());
        // 升级成功后跟着的是二进制帧,按字节读再宽松转文本。
        let upgrade = |origin: &str| {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", wt.local_port())).unwrap();
            s.write_all(
                format!(
                    "GET {location}/ws HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\n\
Cookie: {pair}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
            let mut out = Vec::new();
            let _ = s.read_to_end(&mut out);
            String::from_utf8_lossy(&out).into_owned()
        };
        let foreign = upgrade("http://evil.example");
        assert!(foreign.starts_with("HTTP/1.1 403"), "{foreign}");
        // 同一 cookie、同源时能走到下一步(会话不在 → 升级后告知结束),说明拦住它的是 Origin。
        let same = upgrade(&format!("http://{host}"));
        assert!(same.starts_with("HTTP/1.1 101"), "{same}");
    }

    /// 真 tmux:浏览器不回 pong、也不发任何东西(手机断网不发 FIN),服务端在静默上限后结束连接,
    /// 归还连接位与 tmux 客户端。
    #[test]
    #[ignore]
    fn a_silent_peer_is_dropped_after_the_silence_limit() {
        let session = ThrowawaySession::start("summon-wt-silent");
        let tmux = &session.0;
        let wt = spawn_with(Timeouts {
            ping: Duration::from_millis(100),
            peer_silence: Duration::from_millis(1000),
            ..Timeouts::default()
        });
        let mut t = target();
        t.tmux_session = tmux.session.clone();
        let codes = wt.issue_codes_for_test(t);
        let started = std::time::Instant::now();
        let mut s = ws_connect(&wt, &codes.operate);
        // 服务端的 ping 会一直有字节进来,不能靠读超时收尾:读到 EOF 或到 6 秒为止。
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while started.elapsed() < Duration::from_secs(6) {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
        let elapsed = started.elapsed();
        let idle = wait_until_idle(&wt, Duration::from_secs(3));
        let attached = clients_on(tmux);
        assert!(
            elapsed < Duration::from_secs(4),
            "服务端该在静默上限后断开,实际 {elapsed:?}"
        );
        assert!(
            got.windows(2).any(|w| w == [0x89, 0]),
            "服务端应发过 ping:{elapsed:?} {} 字节,末尾 {:?}",
            got.len(),
            &got[got.len().saturating_sub(40)..]
        );
        assert!(idle, "连接位应归还");
        assert_eq!(attached, 0, "tmux 客户端应随之结束");
    }

    /// 真 tmux:会话的 detach-on-destroy 不是 on 就不接——会话被拆时 tmux 会把网页切到别的会话上。
    #[test]
    #[ignore]
    fn a_session_that_hands_clients_to_another_session_is_never_attached() {
        let session = ThrowawaySession::start("summon-wt-dod");
        let tmux = &session.0;
        let set = std::process::Command::new("tmux")
            .args([
                "set-option",
                "-t",
                &format!("={}:", tmux.session),
                "detach-on-destroy",
                "off",
            ])
            .status()
            .unwrap();
        assert!(set.success());
        assert!(!tmux.detaches_on_destroy().unwrap());
        let wt = spawn_local();
        let mut t = target();
        t.tmux_session = tmux.session.clone();
        let codes = wt.issue_codes_for_test(t);

        let mut s = ws_connect(&wt, &codes.operate);
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest);
        let rest = String::from_utf8_lossy(&rest);
        let attached = clients_on(tmux);
        let pinned = tmux.current_window().unwrap().size_option().unwrap();
        assert!(
            rest.contains("detach-on-destroy"),
            "应当说明为什么不接:{rest}"
        );
        assert_eq!(attached, 0, "不该挂上任何客户端");
        assert_eq!(pinned, None, "拒接时不钉窗口");
    }

    /// 真 tmux:同一收件人在同一 chat 上再出一组链接,已经打开的页面在一两秒内收到结束通知。
    #[test]
    #[ignore]
    fn issuing_again_ends_an_already_open_page() {
        let session = ThrowawaySession::start("summon-wt-reissue");
        let tmux = &session.0;
        let wt = spawn_local();
        let mut t = target();
        t.tmux_session = tmux.session.clone();
        let codes = wt.issue_codes_for_test(t.clone());
        let mut s = ws_connect(&wt, &codes.read_only);
        std::thread::sleep(Duration::from_millis(500));
        let reissued = std::time::Instant::now();
        let _ = wt.issue_codes_for_test(t);
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while reissued.elapsed() < Duration::from_secs(3) {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
            if String::from_utf8_lossy(&got).contains("\"ended\"") {
                break;
            }
        }
        let elapsed = reissued.elapsed();
        let text = String::from_utf8_lossy(&got);
        assert!(text.contains("\"ended\""), "{text}");
        assert!(elapsed < Duration::from_secs(2), "实际 {elapsed:?}");
    }
}
