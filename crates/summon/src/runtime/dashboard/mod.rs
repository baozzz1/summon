//! 控制台服务:独占 `dashboard.port`,绑定地址沿用 `preview_server.bind`,每条连接一个线程。
//! 页面与接口只认一次性链接或本机码换来的浏览器会话;连接层与浏览器终端共用
//! [`crate::runtime::http_conn`]。设计见 `docs/spec/2026-10-20-dashboard-console-design.md`。

pub(crate) mod agent_archive;

pub(crate) mod api;
mod assets;
mod audit;
pub(crate) mod boot_snapshot;
pub(crate) mod emoji;
pub(crate) mod server;
pub(crate) mod sessions;
pub(crate) mod writer;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::observe::log::log;
use crate::runtime::http_conn::Deadlines;

use sessions::{Gate, Issuer, Principal};

/// 同时在处理的连接上限,与浏览器终端相同。
const MAX_CONNECTIONS: usize = 32;

/// 出不了远程链接的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkRefusal {
    /// 只听回环,又没配 `dashboard.public_base_url`:链接指向的地址点开也连不上。
    LoopbackBind,
    /// 本机没有收件设备能路由到的地址。
    NoRoutableHost,
    /// 随机源或内部状态出错。
    Internal,
}

impl LinkRefusal {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::LoopbackBind => {
                summon_i18n::text!("r3006d365bb")
            }
            Self::NoRoutableHost => {
                summon_i18n::text!("r29d12c8df8")
            }
            Self::Internal => summon_i18n::text!("r72ae644406"),
        }
    }
}

/// 远程链接的 base(不带尾斜杠)。配了 `dashboard.public_base_url` 就用它(反向代理、端口转发时
/// 只有人知道对外地址);没配时主机取与文件预览相同的那个(预览配了 `public_base_url` 取它的主机,
/// 否则用现查的本机可路由地址),端口换成控制台自己的。只听回环又没配时不出:对外地址从不取回环,
/// 回环监听接不到对外地址上的连接。
fn link_base(
    own_public: &str,
    preview_public: &str,
    probed: Option<Ipv4Addr>,
    port: u16,
    loopback_bind: bool,
) -> Result<String, LinkRefusal> {
    let own = own_public.trim().trim_end_matches('/');
    if !own.is_empty() {
        return Ok(own.to_string());
    }
    if loopback_bind {
        return Err(LinkRefusal::LoopbackBind);
    }
    let host = url_host(preview_public)
        .or_else(|| probed.map(|ip| ip.to_string()))
        .ok_or(LinkRefusal::NoRoutableHost)?;
    Ok(format!("http://{host}:{port}"))
}

fn parse_url(url: &str) -> Option<reqwest::Url> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    reqwest::Url::parse(url).ok()
}

/// URL 里的主机(IPv6 带方括号);写不成 URL 的当没写。
fn url_host(url: &str) -> Option<String> {
    parse_url(url)?.host_str().map(str::to_string)
}

/// URL 在 `Host` 头里的写法:主机,加上非默认端口。
fn url_authority(url: &str) -> Option<String> {
    let url = parse_url(url)?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// URL 的源(`scheme://host[:port]`),即浏览器在 `Origin` 头里写的那个。
fn url_origin(url: &str) -> Option<String> {
    let origin = parse_url(url)?.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// 本机码只能从本机用:对端是回环地址,或与这条连接本端是同一个地址(从本机连本机的对外地址时,
/// 内核取的源地址就是它)。外部机器伪造不了 TCP 握手的源地址。
fn local_entry_allowed(peer: IpAddr, local: IpAddr) -> bool {
    peer.is_loopback() || peer == local
}

fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|name| name.into_string().ok())
        .unwrap_or_default()
}

fn token() -> Option<String> {
    let t = crate::runtime::preview_server::new_token();
    (!t.is_empty()).then_some(t)
}

pub(crate) struct Dashboard {
    state: Mutex<sessions::Sessions>,
    cfg: summon_core::config::Dashboard,
    /// 文件预览的 `public_base_url`:没配控制台自己的对外地址时,主机跟它走。
    preview_public_base_url: String,
    /// 实际监听的地址(端口是绑定后的真实端口)。
    local: SocketAddr,
    active: Arc<AtomicUsize>,
    deadlines: Deadlines,
    /// 每个请求重判进入权限的那道闸;没接上之前飞书身份一律不放行。
    gate: OnceLock<Arc<dyn Gate>>,
    /// 接口实现的环境;没接上之前业务接口回 unavailable。
    env: OnceLock<Arc<api::ApiEnv>>,
    registry: server::Registry,
    /// 现查本机对外地址。VPN 地址会在 daemon 运行期间变,每次用都现查;测试换成固定值。
    probe: fn() -> Option<Ipv4Addr>,
    hostname: String,
}

/// 本 daemon 的控制台服务:启动时定一次,之后不变。放在进程级:签发入口(dispatcher 的 `/dash`、
/// control.sock 的本机码)与服务实例的生命周期都是整个 daemon 一份。测试从不写它。
static SERVICE: OnceLock<Result<Arc<Dashboard>, String>> = OnceLock::new();

/// daemon 启动时调用一次;之后的调用被忽略。`Err` 带没在跑的原因(配置关着,或绑定失败的错误文本)。
pub(crate) fn install(service: Result<Arc<Dashboard>, String>) {
    let _ = SERVICE.set(service);
}

/// 在跑的服务;没在跑时给出原因。
pub(crate) fn service() -> Result<Arc<Dashboard>, String> {
    match SERVICE.get() {
        Some(Ok(dash)) => Ok(Arc::clone(dash)),
        Some(Err(reason)) => Err(reason.clone()),
        None => Err(summon_i18n::text!("r53d4cdfcf2").to_string()),
    }
}

impl Dashboard {
    /// 绑定端口、起 accept 线程。`shutdown` 置位后线程在 100ms 内退出,不挡 daemon 停机。
    pub(crate) fn spawn(
        cfg: &summon_core::config::Dashboard,
        bind: &str,
        preview: &summon_core::config::PreviewServer,
        shutdown: Arc<AtomicBool>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::spawn_with(
            cfg,
            bind,
            preview,
            shutdown,
            Deadlines::default(),
            crate::runtime::preview_server::detect_public_host,
        )
    }

    fn spawn_with(
        cfg: &summon_core::config::Dashboard,
        bind: &str,
        preview: &summon_core::config::PreviewServer,
        shutdown: Arc<AtomicBool>,
        deadlines: Deadlines,
        probe: fn() -> Option<Ipv4Addr>,
    ) -> anyhow::Result<Arc<Self>> {
        let listener = TcpListener::bind((bind, cfg.port))
            .with_context(|| summon_i18n::format!("rf25b9a52c6", cfg.port))?;
        // 非阻塞 accept 才能定期看停机标志。
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let this = Arc::new(Self::build(cfg, preview, local, deadlines, probe));
        let serve = Arc::clone(&this);
        let active = Arc::clone(&this.active);
        std::thread::Builder::new()
            .name("dashboard".to_string())
            .spawn(move || {
                crate::runtime::http_conn::accept_loop(
                    listener,
                    active,
                    MAX_CONNECTIONS,
                    shutdown,
                    "dashboard",
                    move |stream| server::serve(stream, &serve),
                )
            })?;
        log(&summon_i18n::format!(
            "r827ef1ed07",
            this.link_base()
                .unwrap_or_else(|refusal| summon_i18n::format!("r52dd5e1178", refusal.reason()))
        ));
        Ok(this)
    }

    fn build(
        cfg: &summon_core::config::Dashboard,
        preview: &summon_core::config::PreviewServer,
        local: SocketAddr,
        deadlines: Deadlines,
        probe: fn() -> Option<Ipv4Addr>,
    ) -> Self {
        Self {
            state: Mutex::new(sessions::Sessions::new(Duration::from_secs(
                preview.token_ttl_secs,
            ))),
            cfg: cfg.clone(),
            preview_public_base_url: preview.public_base_url.clone(),
            local,
            active: Arc::new(AtomicUsize::new(0)),
            deadlines,
            gate: OnceLock::new(),
            env: OnceLock::new(),
            registry: server::registry(),
            probe,
            hostname: hostname(),
        }
    }

    /// 接上每个请求重判进入权限的那道闸(只接一次,之后的调用被忽略)。
    pub(crate) fn attach_gate(&self, gate: Arc<dyn Gate>) {
        let _ = self.gate.set(gate);
    }

    /// 接上接口实现的环境,它同时就是判权那道闸(只接一次)。
    pub(crate) fn attach_env(&self, env: Arc<api::ApiEnv>) {
        let gate: Arc<dyn Gate> = env.clone();
        self.attach_gate(gate);
        let _ = self.env.set(env);
    }

    fn env(&self) -> Option<Arc<api::ApiEnv>> {
        self.env.get().cloned()
    }

    /// 会话状态。锁里只有几张表,持锁时 panic 也不会留下半改的结构,中毒后照常用。
    fn sessions(&self) -> MutexGuard<'_, sessions::Sessions> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn loopback_bind(&self) -> bool {
        self.local.ip().is_loopback()
    }

    fn link_base(&self) -> Result<String, LinkRefusal> {
        link_base(
            &self.cfg.public_base_url,
            &self.preview_public_base_url,
            (self.probe)(),
            self.local.port(),
            self.loopback_bind(),
        )
    }

    /// 给 `/dash` 的发起人签一条远程链接:按 `preview_server.token_ttl_secs` 过期、只能用一次,
    /// 落地页 POST 进入时按此刻的名单重判签发人。
    pub(crate) fn issue_link(&self, issuer: Issuer) -> Result<String, LinkRefusal> {
        let base = self.link_base()?;
        let token = self
            .sessions()
            .issue_link(issuer, Instant::now(), token)
            .ok_or(LinkRefusal::Internal)?;
        Ok(format!("{base}/l/{token}"))
    }

    /// 远程链接的有效期(私聊卡上写给人看)。
    pub(crate) fn link_ttl(&self) -> Duration {
        self.sessions().link_ttl()
    }

    /// 给本机 owner 签一个一次性本机码(60 秒),返回回环地址上的落地页链接。调用方须已确认请求
    /// 来自 control.sock 上的本机 owner。
    pub(crate) fn issue_local(&self) -> anyhow::Result<String> {
        let code = self
            .sessions()
            .issue_local(Instant::now(), token)
            .context(summon_i18n::text!("r2f4cebbe95"))?;
        let ip = self.local.ip();
        let host = if ip.is_unspecified() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            ip
        };
        Ok(format!(
            "http://{}/local/{code}",
            SocketAddr::new(host, self.local.port())
        ))
    }

    /// `Host` 头可以是哪些:监听地址(含回环的几种写法)、本机对外地址、预览与控制台的对外地址。
    /// 不在其中的一律拒(DNS rebinding:别的域名解析到本机时,浏览器带的是那个域名)。
    fn allowed_hosts(&self) -> Vec<String> {
        let port = self.local.port();
        let ip = self.local.ip();
        let mut hosts = Vec::new();
        if ip.is_loopback() || ip.is_unspecified() {
            hosts.push(format!("127.0.0.1:{port}"));
            hosts.push(format!("localhost:{port}"));
            hosts.push(format!("[::1]:{port}"));
        } else {
            hosts.push(SocketAddr::new(ip, port).to_string());
        }
        if let Some(ip) = (self.probe)() {
            hosts.push(format!("{ip}:{port}"));
        }
        if let Some(host) = url_host(&self.preview_public_base_url) {
            hosts.push(format!("{host}:{port}"));
        }
        if let Some(authority) = url_authority(&self.cfg.public_base_url) {
            hosts.push(authority);
        }
        hosts
    }

    /// 本机码能用的 `Host`:回环的几种写法与监听地址本身(`issue_local` 签出的链接只指向它们)。
    /// 对外地址不在其中 —— 同机的反向代理转进来的请求对端也是本机,带的却是对外地址。
    fn local_hosts(&self) -> Vec<String> {
        let port = self.local.port();
        let ip = self.local.ip();
        let mut hosts = vec![
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
        if !ip.is_loopback() && !ip.is_unspecified() {
            hosts.push(SocketAddr::new(ip, port).to_string());
        }
        hosts
    }

    /// 控制台自己的源:每个允许的 `Host` 上的 http 源,加上 `dashboard.public_base_url` 的源
    /// (反向代理终结 TLS 时浏览器写的是 https 源)。
    fn allowed_origins(&self) -> Vec<String> {
        let mut origins: Vec<String> = self
            .allowed_hosts()
            .into_iter()
            .map(|host| format!("http://{host}"))
            .collect();
        if let Some(origin) = url_origin(&self.cfg.public_base_url) {
            origins.push(origin);
        }
        origins
    }

    /// 这个主体此刻还能不能进:本机 owner 恒能;飞书身份按盘上当前名单重判。
    fn still_allowed(&self, principal: &Principal) -> bool {
        match principal {
            Principal::Owner => true,
            Principal::Feishu(issuer) => self
                .gate
                .get()
                .is_some_and(|gate| gate.still_allowed(issuer)),
        }
    }
}

/// 控制台的监听与对外地址是否自洽(与预览、终端同口径)。`probe` 是现查的本机对外地址。
fn dashboard_link(
    cfg: &summon_core::config::Config,
    probe: Option<Ipv4Addr>,
) -> Result<String, LinkRefusal> {
    let dash = &cfg.dashboard;
    let bind = cfg.preview_server.bind.as_str();
    let loopback = bind == "localhost" || bind.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    link_base(
        &dash.public_base_url,
        &cfg.preview_server.public_base_url,
        probe,
        dash.port,
        loopback,
    )
}

/// 控制台这一节的配置问题(doctor 与控制台「需要处理」共用)。关着时不查。
pub(crate) fn dashboard_findings(
    cfg: &summon_core::config::Config,
    probe: Option<Ipv4Addr>,
) -> Vec<crate::setup::config_findings::ConfigFinding> {
    use crate::setup::config_findings::ConfigFinding;
    if !cfg.dashboard.enabled {
        return Vec::new();
    }
    match dashboard_link(cfg, probe) {
        Ok(_) => Vec::new(),
        Err(LinkRefusal::LoopbackBind) => vec![ConfigFinding::DashboardLoopbackBind],
        Err(_) => vec![ConfigFinding::DashboardNoRoutableHost],
    }
}

/// `summon doctor` 的控制台一节:是否在听、远程链接出不出得来,与预览、终端同口径。
pub(crate) fn doctor_lines(cfg: &summon_core::config::Config) -> Vec<String> {
    let dash = &cfg.dashboard;
    if !dash.enabled {
        return vec![summon_i18n::text!("rf8627a2a04").to_string()];
    }
    let bind = cfg.preview_server.bind.as_str();
    let listening = std::net::TcpStream::connect((bind, dash.port)).is_ok();
    let probe = crate::runtime::preview_server::detect_public_host();
    let link = dashboard_link(cfg, probe);
    let mut lines = vec![summon_i18n::format!(
        "r7fe889c130",
        if listening {
            "OK"
        } else {
            summon_i18n::text!("r65364e6fd8")
        },
        dash.port,
        link.as_deref().unwrap_or(summon_i18n::text!("rf9c2dbdb42"))
    )];
    if !listening {
        lines.push(summon_i18n::text!("rfe078ab000").to_string());
    }
    lines.extend(
        dashboard_findings(cfg, probe)
            .iter()
            .map(|finding| finding.doctor_line()),
    );
    lines
}

#[cfg(test)]
impl Dashboard {
    /// 不开监听的实例:`server::handle` 直接喂请求,不走 TCP。
    pub(crate) fn unbound_for_test(
        cfg: &summon_core::config::Dashboard,
        preview: &summon_core::config::PreviewServer,
        local: SocketAddr,
        probe: fn() -> Option<Ipv4Addr>,
    ) -> Self {
        Self::build(cfg, preview, local, Deadlines::default(), probe)
    }

    pub(crate) fn spawn_for_test(
        cfg: &summon_core::config::Dashboard,
        shutdown: Arc<AtomicBool>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::spawn_with(
            cfg,
            "127.0.0.1",
            &summon_core::config::PreviewServer::default(),
            shutdown,
            Deadlines::default(),
            || None,
        )
    }

    pub(crate) fn local_port(&self) -> u16 {
        self.local.port()
    }

    pub(crate) fn active_connections(&self) -> usize {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn with_state<T>(&self, f: impl FnOnce(&mut sessions::Sessions) -> T) -> T {
        f(&mut self.sessions())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: Ipv4Addr = Ipv4Addr::new(10, 1, 2, 3);

    #[test]
    fn link_refusal_when_loopback_bind_without_public_url() {
        assert_eq!(
            link_base("", "", Some(LAN), 7767, true),
            Err(LinkRefusal::LoopbackBind)
        );
        assert_eq!(
            link_base("", "http://10.9.9.9:7765", Some(LAN), 7767, true),
            Err(LinkRefusal::LoopbackBind),
            "预览配了对外地址也不行:控制台自己只听回环"
        );
        assert_eq!(
            link_base("", "", None, 7767, false),
            Err(LinkRefusal::NoRoutableHost)
        );
        assert!(LinkRefusal::LoopbackBind
            .reason()
            .contains("summon dashboard open"));
    }

    #[test]
    fn public_base_url_overrides_derived_host() {
        assert_eq!(
            link_base("https://dash.example.com/", "", Some(LAN), 7767, true).as_deref(),
            Ok("https://dash.example.com"),
            "只听回环也照用:人知道前面有反向代理"
        );
        assert_eq!(
            link_base(
                " http://10.0.0.8:9000 ",
                "http://10.9.9.9:7765",
                Some(LAN),
                7767,
                false
            )
            .as_deref(),
            Ok("http://10.0.0.8:9000")
        );
        assert_eq!(
            link_base("", "", Some(LAN), 7767, false).as_deref(),
            Ok("http://10.1.2.3:7767")
        );
    }

    #[test]
    fn derived_host_follows_preview_public_base_url_host() {
        assert_eq!(
            link_base(
                "",
                "http://example-host.internal:7765/",
                Some(LAN),
                7767,
                false
            )
            .as_deref(),
            Ok("http://example-host.internal:7767"),
            "主机跟预览走,端口换成控制台的"
        );
        assert_eq!(
            link_base("", "not a url", Some(LAN), 7767, false).as_deref(),
            Ok("http://10.1.2.3:7767"),
            "写不成 URL 的预览地址当没写"
        );
    }

    #[test]
    fn url_parts_follow_what_browsers_send() {
        assert_eq!(
            url_authority("https://dash.example.com/x").as_deref(),
            Some("dash.example.com")
        );
        assert_eq!(
            url_authority("http://10.0.0.8:9000").as_deref(),
            Some("10.0.0.8:9000")
        );
        assert_eq!(
            url_origin("https://dash.example.com/x").as_deref(),
            Some("https://dash.example.com")
        );
        assert_eq!(
            url_origin("http://10.0.0.8:80/").as_deref(),
            Some("http://10.0.0.8")
        );
        assert_eq!(url_host("http://[::1]:7765").as_deref(), Some("[::1]"));
        assert_eq!(url_origin(""), None);
    }

    #[test]
    fn local_entry_is_only_from_this_machine() {
        let lan: IpAddr = LAN.into();
        let other: IpAddr = "10.1.2.4".parse().unwrap();
        assert!(local_entry_allowed("127.0.0.1".parse().unwrap(), lan));
        assert!(local_entry_allowed("::1".parse().unwrap(), lan));
        assert!(local_entry_allowed(lan, lan), "本机连本机的对外地址");
        assert!(!local_entry_allowed(other, lan));
    }

    #[test]
    fn local_links_point_at_a_loopback_address_when_listening_everywhere() {
        let cfg = summon_core::config::Dashboard::default();
        let preview = summon_core::config::PreviewServer::default();
        let any =
            Dashboard::unbound_for_test(&cfg, &preview, "0.0.0.0:7767".parse().unwrap(), || None);
        assert!(any
            .issue_local()
            .unwrap()
            .starts_with("http://127.0.0.1:7767/local/"));
        let lan =
            Dashboard::unbound_for_test(&cfg, &preview, "10.1.2.3:7767".parse().unwrap(), || None);
        assert!(lan
            .issue_local()
            .unwrap()
            .starts_with("http://10.1.2.3:7767/local/"));
    }

    #[test]
    fn allowed_hosts_cover_listening_public_and_configured_addresses() {
        let cfg = summon_core::config::Dashboard {
            public_base_url: "https://dash.example.com".into(),
            ..Default::default()
        };
        let preview = summon_core::config::PreviewServer {
            public_base_url: "http://example-host.internal:7765".into(),
            ..Default::default()
        };
        let dash =
            Dashboard::unbound_for_test(&cfg, &preview, "0.0.0.0:7767".parse().unwrap(), || {
                Some(LAN)
            });
        let hosts = dash.allowed_hosts();
        for host in [
            "127.0.0.1:7767",
            "localhost:7767",
            "10.1.2.3:7767",
            "example-host.internal:7767",
            "dash.example.com",
        ] {
            assert!(hosts.iter().any(|h| h == host), "{host} 不在 {hosts:?}");
        }
        let origins = dash.allowed_origins();
        assert!(origins.iter().any(|o| o == "https://dash.example.com"));
        assert!(origins.iter().any(|o| o == "http://10.1.2.3:7767"));
        assert!(!origins.iter().any(|o| o == "http://evil.example"));
    }

    #[test]
    fn feishu_sessions_need_an_attached_gate() {
        let cfg = summon_core::config::Dashboard::default();
        let preview = summon_core::config::PreviewServer::default();
        let dash =
            Dashboard::unbound_for_test(&cfg, &preview, "127.0.0.1:7767".parse().unwrap(), || None);
        let issuer = sessions::tests::issuer();
        assert!(dash.still_allowed(&Principal::Owner));
        assert!(
            !dash.still_allowed(&Principal::Feishu(issuer)),
            "没接上判权时飞书身份一律不放行"
        );
    }

    #[test]
    fn doctor_reports_a_disabled_dashboard() {
        let mut cfg = summon_core::config::Config::default();
        cfg.dashboard.enabled = false;
        assert!(doctor_lines(&cfg)[0].contains("已关闭"));
    }

    /// 只听回环又没配对外地址:报出 /dash 不出链接,并指明本机入口照常可用。
    #[test]
    fn doctor_explains_a_loopback_only_dashboard() {
        let mut cfg = summon_core::config::Config::default();
        // 1 号端口没人听:不受本机是否跑着 daemon 影响。
        cfg.dashboard.port = 1;
        let lines = doctor_lines(&cfg).join("\n");
        assert!(
            lines.contains("dashboard       : 缺失 (127.0.0.1:1"),
            "{lines}"
        );
        assert!(lines.contains("/dash 不出链接") && lines.contains("summon dashboard open"));
        cfg.dashboard.public_base_url = "https://dash.example.com".into();
        let lines = doctor_lines(&cfg).join("\n");
        assert!(
            lines.contains("远程链接 https://dash.example.com"),
            "{lines}"
        );
        assert!(!lines.contains("⚠"), "{lines}");
    }
}
