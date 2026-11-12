//! 控制台的路由、安全头与接口闸门。每条连接只服务一个请求(`Connection: close`)。
//!
//! - 页面:未认证的 GET 只给远程链接与本机码的落地页、或「请从飞书或本机打开」的提示页,都不含
//!   数据,也都不改任何状态(飞书与安全扫描会预取链接);进入一律是落地页上的 POST。静态资源公开、
//!   长缓存。
//! - 接口:`POST /api/<方法>`,JSON 进出,响应是 `{ok:true,data}` / `{ok:false,error}`。
//!   Host、Origin、Content-Type、会话、CSRF 全过才进处理函数;每个请求按盘上当前名单重判进入权限。

use std::borrow::Cow;
use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};
use summon_core::dashboard::authz::{Decision, Target};
use summon_core::web_terminal::http::{cookie_value, is_token};

use super::api::ApiEnv;
use super::assets::{self, Asset, Page};
use super::sessions::{constant_time_eq, ChatKind, Entry, Principal, Session, COOKIE, SESSION_TTL};
use super::{local_entry_allowed, token, Dashboard};
use crate::observe::log::log;
use crate::runtime::http_conn::{self, Limits, ReadOutcome, Reply, Request};

/// 请求头 16 KiB / 64 条(同一主机上预览与终端的 cookie 也会带过来),请求体 1 MiB。
const LIMITS: Limits = Limits {
    head: 16 * 1024,
    headers: 64,
    body: 1024 * 1024,
};

/// 全部响应都带的内容安全策略:只认本站的脚本、样式与连接,不许被嵌进别的页面。
pub(crate) const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
pub(crate) const CSRF_HEADER: &str = "X-Summon-Csrf";
/// 页面拿 CSRF 的那个方法自己不要求 CSRF 头:它只读,且 Origin 闸已挡住别的站点。
const WHOAMI: &str = "session.whoami";
const JSON_TYPE: &str = "application/json; charset=utf-8";
const TEXT_TYPE: &str = "text/plain; charset=utf-8";
const HTML_TYPE: &str = "text/html; charset=utf-8";

// ── 接口的错误、上下文与方法表 ─────────────────────────────────────────────

/// 与 `dashboard/src/api/types.ts` 的 `ErrorCode` 逐项对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    Unauthenticated,
    Forbidden,
    Conflict,
    Invalid,
    NotFound,
    Busy,
    Unavailable,
    Internal,
}

impl ErrorCode {
    fn status(self) -> &'static str {
        match self {
            Self::Unauthenticated => "401 Unauthorized",
            Self::Forbidden => "403 Forbidden",
            Self::Conflict | Self::Busy => "409 Conflict",
            Self::Invalid => "400 Bad Request",
            Self::NotFound => "404 Not Found",
            Self::Unavailable => "503 Service Unavailable",
            Self::Internal => "500 Internal Server Error",
        }
    }
}

/// 与 `types.ts` 的 `ApiError` 对应。`key` 是出错那一项的键路径(表单把错误挂到那一行下面)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ApiError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) key: Option<String>,
}

impl ApiError {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            key: None,
        }
    }

    pub(crate) fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }
}

/// 一次接口调用的上下文。主体只来自会话(飞书点击人或本机 owner),浏览器提交的身份不采信。
pub(crate) struct ApiCtx {
    pub(crate) principal: Principal,
    /// 这次请求的时刻(定时任务的下次触发时间这类按它算)。
    pub(crate) now: chrono::DateTime<chrono::Local>,
    csrf: String,
    expires_at: chrono::DateTime<chrono::Utc>,
    pub(crate) hostname: String,
    /// 接口实现的环境;daemon 启动时接上,没接上时业务接口回 unavailable。
    env: Option<Arc<ApiEnv>>,
}

impl ApiCtx {
    fn new(session: Session, hostname: &str, env: Option<Arc<ApiEnv>>) -> Self {
        Self {
            principal: session.principal,
            now: chrono::Local::now(),
            csrf: session.csrf,
            expires_at: session.expires_at,
            hostname: hostname.to_string(),
            env,
        }
    }

    pub(crate) fn env_arc(&self) -> Result<Arc<ApiEnv>, ApiError> {
        self.env().map(|_| self.env.as_ref().unwrap().clone())
    }

    /// 接口实现的环境。
    pub(crate) fn env(&self) -> Result<&ApiEnv, ApiError> {
        self.env
            .as_deref()
            .ok_or_else(|| ApiError::new(ErrorCode::Unavailable, summon_i18n::text!("r8215e2c231")))
    }

    /// 给接口实现的单测造一个上下文。
    #[cfg(test)]
    pub(crate) fn for_test(principal: Principal) -> Self {
        Self {
            principal,
            now: chrono::Local::now(),
            csrf: "c".repeat(32),
            expires_at: chrono::Utc::now(),
            hostname: "test-host".to_string(),
            env: None,
        }
    }

    /// 带环境的测试上下文。
    #[cfg(test)]
    pub(crate) fn for_test_env(principal: Principal, env: Arc<ApiEnv>) -> Self {
        Self {
            env: Some(env),
            ..Self::for_test(principal)
        }
    }
}

/// 接口处理函数:参数是请求体解析出的 JSON 对象,返回值放进 `{ok:true,data}`。
pub(crate) type Handler = fn(&ApiCtx, Value) -> Result<Value, ApiError>;

/// 方法表:方法名 → 处理函数,方法名与 `types.ts` 的 `Methods` 一致。
#[derive(Default)]
pub(crate) struct Registry {
    methods: HashMap<&'static str, Handler>,
}

impl Registry {
    pub(crate) fn register(&mut self, method: &'static str, handler: Handler) {
        let previous = self.methods.insert(method, handler);
        debug_assert!(
            previous.is_none(),
            "{}",
            summon_i18n::format!("r538652b651")
        );
    }

    fn get(&self, method: &str) -> Option<Handler> {
        self.methods.get(method).copied()
    }

    #[cfg(test)]
    pub(crate) fn handler(&self, method: &str) -> Option<Handler> {
        self.get(method)
    }

    #[cfg(test)]
    pub(crate) fn methods(&self) -> Vec<&'static str> {
        self.methods.keys().copied().collect()
    }
}

/// 控制台的全部接口在这里登记。
pub(crate) fn registry() -> Registry {
    let mut registry = Registry::default();
    registry.register(WHOAMI, whoami);
    super::api::register(&mut registry);
    registry
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Identity<'a> {
    Owner,
    Feishu {
        name: &'a str,
        agent: &'a str,
        chat_id: &'a str,
        chat_kind: ChatKind,
    },
}

#[derive(Serialize)]
struct WhoAmI<'a> {
    identity: Identity<'a>,
    csrf: &'a str,
    expires_at: String,
    hostname: &'a str,
    version: &'static str,
    /// 能不能改整机设置;日志页也只给这些人。
    machine: bool,
}

fn whoami(ctx: &ApiCtx, _params: Value) -> Result<Value, ApiError> {
    let identity = match &ctx.principal {
        Principal::Owner => Identity::Owner,
        Principal::Feishu(issuer) => Identity::Feishu {
            name: &issuer.name,
            agent: &issuer.agent,
            chat_id: &issuer.chat_id,
            chat_kind: issuer.chat_kind,
        },
    };
    serde_json::to_value(WhoAmI {
        identity,
        csrf: &ctx.csrf,
        expires_at: ctx
            .expires_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        hostname: &ctx.hostname,
        version: env!("CARGO_PKG_VERSION"),
        // 没接上环境(只起了服务的测试)时按不能算。
        machine: ctx
            .env()
            .is_ok_and(|env| matches!(env.decide(ctx, &Target::Machine), Decision::Allow)),
    })
    .map_err(|e| ApiError::new(ErrorCode::Internal, e.to_string()))
}

// ── 连接与路由 ───────────────────────────────────────────────────────────

/// accept 循环交来的一条连接。
pub(super) fn serve(mut stream: TcpStream, dash: &Dashboard) {
    let Some((read_by, reply_by)) = dash.deadlines.start(&stream) else {
        return;
    };
    let from_this_machine = match (stream.peer_addr(), stream.local_addr()) {
        (Ok(peer), Ok(local)) => local_entry_allowed(peer.ip(), local.ip()),
        _ => false,
    };
    let reply = match http_conn::read_request(&mut stream, &LIMITS, read_by) {
        ReadOutcome::Ok(req, body) => handle(dash, &req, &body, from_this_machine),
        ReadOutcome::TooManyHeaders => {
            log(&summon_i18n::format!("r48e8e68e2e", LIMITS.headers));
            secured(text(
                "431 Request Header Fields Too Large",
                "too many headers",
            ))
        }
        ReadOutcome::BodyTooLarge => secured(
            api_failure(ApiError::new(
                ErrorCode::Invalid,
                summon_i18n::text!("r7f00103b1b"),
            ))
            .with_status("413 Payload Too Large"),
        ),
        ReadOutcome::Abandon => return,
    };
    // 客户端提前断开不影响服务。
    let _ = http_conn::write_reply(&mut stream, &reply, reply_by);
}

/// 一个读完的请求 → 响应(带上全部安全头)。`from_this_machine`:对端是本机(本机码只认它)。
pub(super) fn handle(
    dash: &Dashboard,
    req: &Request,
    body: &[u8],
    from_this_machine: bool,
) -> Reply<'static> {
    secured(route(dash, req, body, from_this_machine))
}

fn route(dash: &Dashboard, req: &Request, body: &[u8], from_this_machine: bool) -> Reply<'static> {
    let path = req.path();
    let api = path.strip_prefix("/api/");
    if !host_allowed(dash, req) {
        let message = summon_i18n::text!("r65b59b21ed");
        return match api {
            Some(_) => api_failure(ApiError::new(ErrorCode::Forbidden, message)),
            None => text("403 Forbidden", "unknown host"),
        };
    }
    if let Some(method) = api {
        return call(dash, req, body, method);
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["_", name]) => match assets::builtin(name) {
            Some(asset) => serve_asset(asset),
            None => not_found(),
        },
        ("GET", ["assets", ..]) => match assets::dist(&path[1..]) {
            Some(asset) => {
                // 许可声明地址固定,每次使用前要重新验证;其余资源的文件名含内容摘要。
                let cache = if path == "/assets/third-party-licenses.txt" {
                    "no-cache"
                } else {
                    "public, max-age=31536000, immutable"
                };
                serve_asset(asset).header("Cache-Control", cache)
            }
            None => not_found(),
        },
        ("GET", ["emoji", key]) => match authenticate(dash, req) {
            Ok(_) => serve_emoji(dash, key),
            Err(_) => text("401 Unauthorized", "unauthorized"),
        },
        ("GET", ["l", link]) if is_token(link) => {
            // 只看链接还在不在:GET 不改任何状态(飞书与安全扫描会预取链接)。
            if dash.sessions().link_issuer(link, Instant::now()).is_some() {
                Reply::new("200 OK", HTML_TYPE, assets::landing_page(link))
            } else {
                html("410 Gone", assets::invalid_page())
            }
        }
        ("POST", ["l", link, "enter"]) if is_token(link) => enter(dash, req, link),
        ("GET" | "POST", ["local", code])
            if is_token(code) && (forwarded(req) || !local_host(dash, req)) =>
        {
            text("403 Forbidden", summon_i18n::text!("r6ec73476c7"))
        }
        ("GET", ["local", code]) if is_token(code) => {
            if dash.sessions().local_code_live(code, Instant::now()) {
                html("200 OK", assets::page(Page::Local))
            } else {
                html("404 Not Found", assets::invalid_page())
            }
        }
        ("POST", ["local", code]) if is_token(code) => {
            enter_locally(dash, req, code, from_this_machine)
        }
        ("GET", segments) if is_app_route(segments) => match authenticate(dash, req) {
            Ok(_) => html("200 OK", assets::page(Page::Index)),
            Err(_) => html("200 OK", assets::hint_page()),
        },
        _ => not_found(),
    }
}

/// 单页应用的路由(History API):`/`、`/agents/x/runtime` 这类,不含保留前缀,最后一段不像文件名。
fn is_app_route(segments: &[&str]) -> bool {
    let reserved = matches!(
        segments.first().copied(),
        Some("_" | "assets" | "emoji" | "l" | "local" | "api")
    );
    !reserved && !segments.last().is_some_and(|last| last.contains('.'))
}

/// 落地页的「进入」:链接还在、签发人此刻仍过签发 chat 上的 operate-gate,就当场用掉链接、发
/// 12 小时的会话 cookie 并 303 到 `/`。判权要读盘,在锁外做;没权限时链接留着,过期作废。
fn enter(dash: &Dashboard, req: &Request, link: &str) -> Reply<'static> {
    if !origin_allowed_if_present(dash, req) {
        return text("403 Forbidden", "forbidden");
    }
    let issuer = dash.sessions().link_issuer(link, Instant::now()).cloned();
    let Some(issuer) = issuer else {
        return html("410 Gone", assets::invalid_page());
    };
    if !dash.still_allowed(&Principal::Feishu(issuer.clone())) {
        log(&summon_i18n::format!(
            "r043b306f63",
            issuer.agent,
            issuer.chat_id
        ));
        return html("403 Forbidden", assets::forbidden_page());
    }
    let entry = dash
        .sessions()
        .redeem_link(link, session_secret(req), Instant::now(), token);
    match entry {
        Entry::Granted(secret) => text("303 See Other", "")
            .header("Location", "/")
            .header("Set-Cookie", session_cookie(&secret)),
        Entry::Spent => html("410 Gone", assets::invalid_page()),
        Entry::Failed => text("500 Internal Server Error", "internal"),
    }
}

/// 本机码落地页的「进入」:只认本机发来的请求,码当场作废,换一个本机 owner 的会话。
fn enter_locally(
    dash: &Dashboard,
    req: &Request,
    code: &str,
    from_this_machine: bool,
) -> Reply<'static> {
    if !origin_allowed_if_present(dash, req) {
        return text("403 Forbidden", "forbidden");
    }
    if !from_this_machine {
        return text("403 Forbidden", summon_i18n::text!("r6ec73476c7"));
    }
    match dash.sessions().consume_local(code, Instant::now(), token) {
        Some(secret) => text("303 See Other", "")
            .header("Location", "/")
            .header("Set-Cookie", session_cookie(&secret)),
        None => html("404 Not Found", assets::invalid_page()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AuthFailure {
    NoSession,
    Revoked,
}

/// 凭 cookie 找会话,并按盘上当前名单重判进入权限;不过就当场作废会话。判权在锁外做(要读盘)。
fn authenticate(dash: &Dashboard, req: &Request) -> Result<Session, AuthFailure> {
    let secret = session_secret(req).ok_or(AuthFailure::NoSession)?;
    let session = dash
        .sessions()
        .session(secret, Instant::now())
        .cloned()
        .ok_or(AuthFailure::NoSession)?;
    if !dash.still_allowed(&session.principal) {
        dash.sessions().end(secret);
        if let Principal::Feishu(issuer) = &session.principal {
            log(&summon_i18n::format!(
                "re313bae267",
                issuer.agent,
                issuer.chat_id
            ));
        }
        return Err(AuthFailure::Revoked);
    }
    Ok(session)
}

/// `POST /api/<方法>`。
fn call(dash: &Dashboard, req: &Request, body: &[u8], method: &str) -> Reply<'static> {
    let fail = |code, message: &str| api_failure(ApiError::new(code, message));
    if req.method != "POST" {
        return fail(ErrorCode::Invalid, summon_i18n::text!("r1cb480a9cf"))
            .with_status("405 Method Not Allowed")
            .header("Allow", "POST");
    }
    if !origin_allowed(dash, req.header("Origin")) {
        return fail(ErrorCode::Forbidden, summon_i18n::text!("rc556a7a79e"));
    }
    if !is_json(req.header("Content-Type")) {
        return fail(ErrorCode::Invalid, summon_i18n::text!("r349cb61b71"))
            .with_status("415 Unsupported Media Type");
    }
    let env = dash.env();
    let _operation = env
        .as_ref()
        .map(|env| env.operations.read().unwrap_or_else(|e| e.into_inner()));
    let session = match authenticate(dash, req) {
        Ok(found) => found,
        Err(AuthFailure::NoSession) => {
            return fail(
                ErrorCode::Unauthenticated,
                summon_i18n::text!("r9e15eca4db"),
            )
        }
        Err(AuthFailure::Revoked) => {
            return fail(
                ErrorCode::Unauthenticated,
                summon_i18n::text!("r9574607163"),
            )
        }
    };
    let Some(handler) = dash.registry.get(method) else {
        return fail(ErrorCode::NotFound, &summon_i18n::format!("ra6f91b7e63"));
    };
    let csrf_ok = req
        .header(CSRF_HEADER)
        .is_some_and(|h| constant_time_eq(h.as_bytes(), session.csrf.as_bytes()));
    if method != WHOAMI && !csrf_ok {
        return fail(ErrorCode::Forbidden, summon_i18n::text!("r440c058c66"));
    }
    let params = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice::<Value>(body) {
            Ok(params) if params.is_object() => params,
            _ => return fail(ErrorCode::Invalid, summon_i18n::text!("rf6aa797c89")),
        }
    };
    let ctx = ApiCtx::new(session, &dash.hostname, dash.env());
    match summon_core::i18n::with_language(summon_core::i18n::language(), || handler(&ctx, params))
    {
        Ok(data) => json_reply("200 OK", &json!({ "ok": true, "data": data })),
        Err(error) => api_failure(error),
    }
}

// ── 判据 ────────────────────────────────────────────────────────────────

/// 本机码只认回环或监听地址上的 `Host`(见 [`Dashboard::local_hosts`])。
fn local_host(dash: &Dashboard, req: &Request) -> bool {
    req.header("Host").is_some_and(|host| {
        dash.local_hosts()
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host.trim()))
    })
}

/// 请求经过了反向代理(带了代理才会加的转发头)。本机码只给直接从本机浏览器来的请求:同机的反向
/// 代理转进来的请求,对端地址也是本机。
fn forwarded(req: &Request) -> bool {
    [
        "X-Forwarded-For",
        "X-Forwarded-Host",
        "X-Real-IP",
        "Forwarded",
    ]
    .iter()
    .any(|name| req.header(name).is_some())
}

fn host_allowed(dash: &Dashboard, req: &Request) -> bool {
    req.header("Host").is_some_and(|host| {
        dash.allowed_hosts()
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host.trim()))
    })
}

/// `Origin` 必须在、且逐字等于控制台自己的源之一。
fn origin_allowed(dash: &Dashboard, origin: Option<&str>) -> bool {
    origin.is_some_and(|origin| dash.allowed_origins().iter().any(|o| o == origin))
}

/// 远程链接与本机码的进入:浏览器发的 POST 都带 `Origin`,带了就得是本站的。
fn origin_allowed_if_present(dash: &Dashboard, req: &Request) -> bool {
    req.header("Origin").is_none() || origin_allowed(dash, req.header("Origin"))
}

fn is_json(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| {
        ct.split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    })
}

fn session_secret(req: &Request) -> Option<&str> {
    cookie_value(req.header("Cookie")?, COOKIE).filter(|secret| is_token(secret))
}

fn session_cookie(secret: &str) -> String {
    format!(
        "{COOKIE}={secret}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_TTL.as_secs()
    )
}

// ── 响应 ────────────────────────────────────────────────────────────────

trait WithStatus {
    fn with_status(self, status: &'static str) -> Self;
}

impl WithStatus for Reply<'static> {
    fn with_status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }
}

/// 全部响应都带的安全头;没指定缓存策略的一律 `no-store`。
///
/// `Referrer-Policy` 取 `same-origin`:跨源跳转不带 Referer,链接里的 token 不外泄;同源的表单 POST
/// 与 fetch 照常带上真实的 `Origin`,过得了同源校验。
fn secured(reply: Reply<'static>) -> Reply<'static> {
    let cached = reply.header_value("Cache-Control").is_some();
    let reply = reply
        .header("Content-Security-Policy", CSP)
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "same-origin")
        .header("X-Frame-Options", "DENY");
    if cached {
        reply
    } else {
        reply.header("Cache-Control", "no-store")
    }
}

fn text(status: &'static str, body: &'static str) -> Reply<'static> {
    Reply::new(status, TEXT_TYPE, body.as_bytes())
}

fn not_found() -> Reply<'static> {
    text("404 Not Found", "not found")
}

fn html(status: &'static str, page: Asset) -> Reply<'static> {
    Reply::new(status, page.ctype, page.bytes)
}

/// 飞书表情图(见 [`super::emoji`])。取不到时页面退回系统 emoji;一代 daemon 只记一次取不到的原因,
/// 免得断网时每张图记一行。
fn serve_emoji(dash: &Dashboard, key: &str) -> Reply<'static> {
    use super::emoji::{fetch_from_cdn, load, LoadError};
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some(env) = dash.env() else {
        return text("503 Service Unavailable", "starting");
    };
    let dir = env.home.join("run").join("dashboard-emoji");
    match load(&dir, key, fetch_from_cdn) {
        Ok(bytes) => Reply::new("200 OK", "image/png", bytes)
            .header("Cache-Control", "private, max-age=604800"),
        Err(LoadError::Unknown) => not_found(),
        Err(LoadError::Unavailable(why)) => {
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log(&summon_i18n::format!("rd95048c361"));
            }
            text("502 Bad Gateway", "emoji unavailable")
        }
    }
}

fn serve_asset(asset: Asset) -> Reply<'static> {
    Reply::new("200 OK", asset.ctype, asset.bytes)
}

fn json_reply(status: &'static str, value: &Value) -> Reply<'static> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Reply::new(status, JSON_TYPE, Cow::Owned(body))
}

fn api_failure(error: ApiError) -> Reply<'static> {
    json_reply(error.code.status(), &json!({ "ok": false, "error": error }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::dashboard::sessions::tests::{issuer, GROUP};
    use crate::runtime::dashboard::sessions::{Gate, Issuer};
    use std::io::{Read, Write};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    const HOST: &str = "127.0.0.1:7767";
    const ORIGIN: &str = "http://127.0.0.1:7767";

    /// 桩判权:翻成 false 就当权限已收回。
    struct Flag(Arc<AtomicBool>);

    impl Gate for Flag {
        fn still_allowed(&self, _issuer: &Issuer) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn dash() -> (Dashboard, Arc<AtomicBool>) {
        let dash = Dashboard::unbound_for_test(
            &summon_core::config::Dashboard::default(),
            &summon_core::config::PreviewServer::default(),
            HOST.parse::<SocketAddr>().unwrap(),
            || None,
        );
        let allowed = Arc::new(AtomicBool::new(true));
        dash.attach_gate(Arc::new(Flag(Arc::clone(&allowed))));
        (dash, allowed)
    }

    fn request(method: &str, target: &str, headers: &[(&str, &str)]) -> Request {
        let mut all = vec![("Host".to_string(), HOST.to_string())];
        for (k, v) in headers {
            all.retain(|(name, _)| !name.eq_ignore_ascii_case(k));
            all.push((k.to_string(), v.to_string()));
        }
        Request {
            method: method.to_string(),
            target: target.to_string(),
            headers: all,
            rest: Vec::new(),
        }
    }

    fn send(dash: &Dashboard, req: Request, body: &[u8]) -> Reply<'static> {
        handle(dash, &req, body, true)
    }

    fn body_text(reply: &Reply<'_>) -> String {
        String::from_utf8_lossy(&reply.body).into_owned()
    }

    fn body_json(reply: &Reply<'_>) -> Value {
        serde_json::from_slice(&reply.body).unwrap()
    }

    fn cookie_pair(reply: &Reply<'_>) -> String {
        reply
            .header_value("Set-Cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    /// 调一个接口。`csrf` 为 None 时不带 CSRF 头。
    fn api(dash: &Dashboard, method: &str, cookie: &str, csrf: Option<&str>) -> Reply<'static> {
        let mut headers = vec![
            ("Origin", ORIGIN),
            ("Content-Type", "application/json"),
            ("Cookie", cookie),
        ];
        if let Some(csrf) = csrf {
            headers.push((CSRF_HEADER, csrf));
        }
        send(
            dash,
            request("POST", &format!("/api/{method}"), &headers),
            b"{}",
        )
    }

    fn issue(dash: &Dashboard) -> String {
        dash.with_state(|s| s.issue_link(issuer(), Instant::now(), token))
            .unwrap()
    }

    fn enter_req(link: &str, cookie: Option<&str>) -> Request {
        let mut headers = vec![("Origin", ORIGIN)];
        if let Some(cookie) = cookie {
            headers.push(("Cookie", cookie));
        }
        request("POST", &format!("/l/{link}/enter"), &headers)
    }

    fn link_live(dash: &Dashboard, link: &str) -> bool {
        dash.with_state(|s| s.link_issuer(link, Instant::now()).is_some())
    }

    /// 落地页提交表单进入,返回 cookie(`summon_dash=…`)。
    fn open_feishu(dash: &Dashboard, link: &str, cookie: Option<&str>) -> String {
        let reply = send(dash, enter_req(link, cookie), b"");
        assert_eq!(reply.status, "303 See Other", "{}", body_text(&reply));
        cookie_pair(&reply)
    }

    /// 走一遍 whoami,拿 CSRF。
    fn csrf_of(dash: &Dashboard, cookie: &str) -> String {
        let reply = api(dash, WHOAMI, cookie, None);
        assert_eq!(reply.status, "200 OK", "{}", body_text(&reply));
        body_json(&reply)["data"]["csrf"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn open_local(dash: &Dashboard) -> String {
        let url = dash.issue_local().unwrap();
        let path = &url[url.find("/local/").unwrap()..];
        let reply = send(dash, request("POST", path, &[("Origin", ORIGIN)]), b"");
        assert_eq!(reply.status, "303 See Other");
        cookie_pair(&reply)
    }

    /// 落地页:GET 不改任何状态,预取几次都不消耗链接;页里只有提交到这条链接自己 enter 的表单,
    /// 不含签发人的任何信息。
    #[test]
    fn landing_get_changes_no_state() {
        let (dash, _) = dash();
        let link = issue(&dash);
        for _ in 0..3 {
            let page = send(&dash, request("GET", &format!("/l/{link}"), &[]), b"");
            assert_eq!(page.status, "200 OK");
            let text = body_text(&page);
            assert!(
                text.contains(&format!("action=\"/l/{link}/enter\"")),
                "{text}"
            );
            assert!(text.contains("method=\"post\""), "{text}");
            assert!(
                !text.contains("示例用户") && !text.contains(GROUP),
                "落地页不含数据"
            );
        }
        assert!(link_live(&dash, &link), "预取不会消耗链接");
        let unknown = send(
            &dash,
            request("GET", &format!("/l/{}", "f".repeat(32)), &[]),
            b"",
        );
        assert_eq!(unknown.status, "410 Gone");
        assert!(body_text(&unknown).contains("重新发送 <code>/dash</code>"));
    }

    /// POST 进入:发 Strict cookie、303 到 `/`,链接当场作废;别的站点发来的不算,也不消耗链接。
    #[test]
    fn enter_sets_strict_cookie_and_spends_the_link() {
        let (dash, _) = dash();
        let link = issue(&dash);
        let foreign = request(
            "POST",
            &format!("/l/{link}/enter"),
            &[("Origin", "http://evil.example")],
        );
        assert_eq!(send(&dash, foreign, b"").status, "403 Forbidden");
        assert!(link_live(&dash, &link), "被拒的请求不消耗链接");
        let get_enter = request("GET", &format!("/l/{link}/enter"), &[]);
        assert_eq!(send(&dash, get_enter, b"").status, "404 Not Found");
        assert!(link_live(&dash, &link), "GET 不进入");

        let entered = send(&dash, enter_req(&link, None), b"");
        assert_eq!(entered.status, "303 See Other");
        assert_eq!(entered.header_value("Location"), Some("/"));
        let cookie = entered.header_value("Set-Cookie").unwrap();
        assert!(cookie.starts_with("summon_dash="), "{cookie}");
        for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=43200"] {
            assert!(cookie.split("; ").any(|a| a == attr), "{cookie} 缺 {attr}");
        }

        let again = send(&dash, enter_req(&link, None), b"");
        assert_eq!(again.status, "410 Gone", "只能用一次");
        assert!(again.header_value("Set-Cookie").is_none());
        assert!(body_text(&again).contains("链接已失效"));
        let landing = send(&dash, request("GET", &format!("/l/{link}"), &[]), b"");
        assert_eq!(landing.status, "410 Gone");
    }

    /// 签发人此刻已没有权限:403、不发 cookie,链接留着;权限回来后照样能进。
    #[test]
    fn enter_refuses_a_revoked_issuer_and_keeps_the_link() {
        let (dash, allowed) = dash();
        let link = issue(&dash);
        allowed.store(false, Ordering::SeqCst);
        let denied = send(&dash, enter_req(&link, None), b"");
        assert_eq!(denied.status, "403 Forbidden");
        assert!(denied.header_value("Set-Cookie").is_none());
        assert!(body_text(&denied).contains("没有权限"));
        assert!(link_live(&dash, &link));

        allowed.store(true, Ordering::SeqCst);
        open_feishu(&dash, &link, None);
    }

    /// 飞书内置浏览器跨站打开链接时不带 SameSite=Strict 的 cookie:落地页同源 POST 进入拿到 cookie,
    /// 303 回 `/` 时带上它;已有会话时用新链接再进,换一个新会话,旧的作废。
    #[test]
    fn strict_cookie_flow_reaches_session_after_enter() {
        let (dash, _) = dash();
        let hint = send(&dash, request("GET", "/", &[]), b"");
        assert_eq!(hint.status, "200 OK");
        assert!(body_text(&hint).contains("请从飞书或本机打开"));

        let link = issue(&dash);
        let landing = send(&dash, request("GET", &format!("/l/{link}"), &[]), b"");
        assert_eq!(landing.status, "200 OK");
        let cookie = open_feishu(&dash, &link, None);

        let index = send(&dash, request("GET", "/", &[("Cookie", &cookie)]), b"");
        assert_eq!(index.status, "200 OK");
        assert!(!body_text(&index).contains("请从飞书或本机打开"));
        let deep = send(
            &dash,
            request("GET", "/agents/starlight/runtime", &[("Cookie", &cookie)]),
            b"",
        );
        assert_eq!(
            body_text(&deep),
            body_text(&index),
            "单页应用的深链接给同一个入口"
        );

        let me = body_json(&api(&dash, WHOAMI, &cookie, None));
        assert_eq!(me["ok"], true);
        assert_eq!(
            me["data"]["identity"],
            json!({"kind": "feishu", "name": "示例用户", "agent": "starlight", "chat_id": GROUP, "chat_kind": "group"})
        );
        assert_eq!(me["data"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(me["data"]["expires_at"].as_str().unwrap().ends_with('Z'));

        let again = open_feishu(&dash, &issue(&dash), Some(&cookie));
        assert_ne!(again, cookie);
        assert_eq!(api(&dash, WHOAMI, &cookie, None).status, "401 Unauthorized");
        assert_eq!(api(&dash, WHOAMI, &again, None).status, "200 OK");
    }

    #[test]
    fn api_rejects_missing_or_foreign_origin() {
        let (dash, _) = dash();
        let cookie = open_local(&dash);
        for origin in [
            None,
            Some("http://evil.example"),
            Some("null"),
            Some("http://127.0.0.1:7768"),
        ] {
            let mut headers = vec![("Content-Type", "application/json"), ("Cookie", &cookie)];
            if let Some(origin) = origin {
                headers.push(("Origin", origin));
            }
            let reply = send(
                &dash,
                request("POST", "/api/session.whoami", &headers),
                b"{}",
            );
            assert_eq!(reply.status, "403 Forbidden", "{origin:?}");
            assert_eq!(body_json(&reply)["error"]["code"], "forbidden");
        }
        assert_eq!(api(&dash, WHOAMI, &cookie, None).status, "200 OK");
    }

    #[test]
    fn api_rejects_unknown_host() {
        let (dash, _) = dash();
        let cookie = open_local(&dash);
        let headers = [
            ("Host", "evil.example:7767"),
            ("Origin", "http://evil.example:7767"),
            ("Content-Type", "application/json"),
            ("Cookie", cookie.as_str()),
        ];
        let reply = send(
            &dash,
            request("POST", "/api/session.whoami", &headers),
            b"{}",
        );
        assert_eq!(reply.status, "403 Forbidden");
        assert_eq!(body_json(&reply)["error"]["code"], "forbidden");
        let page = send(
            &dash,
            request(
                "GET",
                "/",
                &[("Host", "evil.example:7767"), ("Cookie", &cookie)],
            ),
            b"",
        );
        assert_eq!(page.status, "403 Forbidden", "页面同样只认控制台的地址");
        let missing = Request {
            headers: Vec::new(),
            ..request("GET", "/", &[])
        };
        assert_eq!(send(&dash, missing, b"").status, "403 Forbidden");
        let localhost = send(
            &dash,
            request("GET", "/", &[("Host", "localhost:7767")]),
            b"",
        );
        assert_eq!(localhost.status, "200 OK");
    }

    #[test]
    fn api_rejects_bad_csrf() {
        let (dash, _) = dash();
        let cookie = open_local(&dash);
        let csrf = csrf_of(&dash, &cookie);
        for bad in [None, Some("x"), Some("b".repeat(32).as_str())] {
            let reply = api(&dash, "status.get", &cookie, bad);
            assert_eq!(reply.status, "403 Forbidden", "{bad:?}");
            assert_eq!(body_json(&reply)["error"]["code"], "forbidden");
        }
        assert_eq!(
            api(&dash, WHOAMI, &cookie, None).status,
            "200 OK",
            "会话还在"
        );
        assert_ne!(
            api(&dash, "status.get", &cookie, Some(&csrf)).status,
            "403 Forbidden",
            "CSRF 对了就交给处理函数"
        );
    }

    #[test]
    fn api_rejects_get_and_non_json() {
        let (dash, _) = dash();
        let cookie = open_local(&dash);
        let get = send(
            &dash,
            request(
                "GET",
                "/api/session.whoami",
                &[("Origin", ORIGIN), ("Cookie", &cookie)],
            ),
            b"",
        );
        assert_eq!(get.status, "405 Method Not Allowed");
        assert_eq!(body_json(&get)["error"]["code"], "invalid");

        let post = |ctype: &str, body: &[u8]| {
            send(
                &dash,
                request(
                    "POST",
                    "/api/session.whoami",
                    &[
                        ("Origin", ORIGIN),
                        ("Cookie", &cookie),
                        ("Content-Type", ctype),
                    ],
                ),
                body,
            )
        };
        assert_eq!(
            post("text/plain", b"{}").status,
            "415 Unsupported Media Type"
        );
        assert_eq!(
            post("application/x-www-form-urlencoded", b"a=1").status,
            "415 Unsupported Media Type"
        );
        assert_eq!(
            post("application/json; charset=utf-8", b"{}").status,
            "200 OK"
        );
        for bad in [&b"{"[..], b"[1]", b"\"x\""] {
            let reply = post("application/json", bad);
            assert_eq!(reply.status, "400 Bad Request");
            assert_eq!(body_json(&reply)["error"]["code"], "invalid");
        }
        let unknown = api(&dash, "nope.nothing", &cookie, None);
        assert_eq!(unknown.status, "404 Not Found");
        assert_eq!(body_json(&unknown)["error"]["code"], "not_found");
    }

    #[test]
    fn api_requires_a_session() {
        let (dash, _) = dash();
        for cookie in [
            "",
            "summon_dash=ffffffffffffffffffffffffffffffff",
            "summon_dash=../x",
        ] {
            let reply = api(&dash, WHOAMI, cookie, None);
            assert_eq!(reply.status, "401 Unauthorized");
            assert_eq!(body_json(&reply)["error"]["code"], "unauthenticated");
        }
    }

    #[test]
    fn security_headers_on_pages() {
        let (dash, _) = dash();
        let link = issue(&dash);
        let code_url = dash.issue_local().unwrap();
        let code_path = &code_url[code_url.find("/local/").unwrap()..];
        let replies = [
            send(&dash, request("GET", "/", &[]), b""),
            send(&dash, request("GET", &format!("/l/{link}"), &[]), b""),
            send(&dash, request("GET", code_path, &[]), b""),
            send(&dash, request("GET", "/_/enter.js", &[]), b""),
            api(&dash, WHOAMI, "", None),
        ];
        for reply in &replies {
            assert_eq!(
                reply.header_value("Content-Security-Policy"),
                Some(
                    "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
                )
            );
            assert_eq!(
                reply.header_value("X-Content-Type-Options"),
                Some("nosniff")
            );
            assert_eq!(reply.header_value("Referrer-Policy"), Some("same-origin"));
            assert_eq!(reply.header_value("X-Frame-Options"), Some("DENY"));
            assert_eq!(reply.header_value("Cache-Control"), Some("no-store"));
        }
        let js = &replies[3];
        assert!(js.ctype.starts_with("text/javascript"));
    }

    #[test]
    fn request_after_permission_revoked_invalidates_session() {
        let (dash, allowed) = dash();
        let link = issue(&dash);
        let cookie = open_feishu(&dash, &link, None);
        assert_eq!(api(&dash, WHOAMI, &cookie, None).status, "200 OK");

        allowed.store(false, Ordering::SeqCst);
        let revoked = api(&dash, WHOAMI, &cookie, None);
        assert_eq!(revoked.status, "401 Unauthorized");
        assert_eq!(body_json(&revoked)["error"]["code"], "unauthenticated");

        allowed.store(true, Ordering::SeqCst);
        assert_eq!(
            api(&dash, WHOAMI, &cookie, None).status,
            "401 Unauthorized",
            "作废的会话不会因权限恢复而复活"
        );
        let page = send(&dash, request("GET", "/", &[("Cookie", &cookie)]), b"");
        assert!(body_text(&page).contains("请从飞书或本机打开"));
    }

    /// 同机的反向代理转进来的请求,对端也是本机:本机码还要求 `Host` 是回环或监听地址,
    /// 对外地址(本机可路由地址、预览与控制台的对外地址)一律不认。
    #[test]
    fn local_code_refuses_public_hosts_even_from_this_machine() {
        let cfg = summon_core::config::Dashboard {
            public_base_url: "https://dash.example.com".into(),
            ..Default::default()
        };
        let preview = summon_core::config::PreviewServer {
            public_base_url: "http://example-host.internal:7765".into(),
            ..Default::default()
        };
        let dash = Dashboard::unbound_for_test(
            &cfg,
            &preview,
            "0.0.0.0:7767".parse::<SocketAddr>().unwrap(),
            || Some(std::net::Ipv4Addr::new(10, 1, 2, 3)),
        );
        for host in [
            "dash.example.com",
            "10.1.2.3:7767",
            "example-host.internal:7767",
        ] {
            let url = dash.issue_local().unwrap();
            let path = url[url.find("/local/").unwrap()..].to_string();
            for method in ["GET", "POST"] {
                let reply = handle(&dash, &request(method, &path, &[("Host", host)]), b"", true);
                assert_eq!(reply.status, "403 Forbidden", "{method} {host}");
                assert!(reply.header_value("Set-Cookie").is_none());
            }
            assert!(
                dash.with_state(|s| s.local_code_live(&path["/local/".len()..], Instant::now())),
                "被拒的请求不消耗本机码"
            );
        }
        let url = dash.issue_local().unwrap();
        let path = url[url.find("/local/").unwrap()..].to_string();
        for host in ["127.0.0.1:7767", "localhost:7767"] {
            let landing = handle(&dash, &request("GET", &path, &[("Host", host)]), b"", true);
            assert_eq!(landing.status, "200 OK", "{host}");
        }
        let entered = handle(
            &dash,
            &request("POST", &path, &[("Host", "[::1]:7767")]),
            b"",
            true,
        );
        assert_eq!(entered.status, "303 See Other");
    }

    /// 带反向代理转发头的请求不认本机码:同机的反向代理转进来的请求对端也是本机。被拒的请求
    /// 不消耗本机码。
    #[test]
    fn local_code_refuses_forwarded_requests() {
        let (dash, _) = dash();
        let url = dash.issue_local().unwrap();
        let path = url[url.find("/local/").unwrap()..].to_string();
        for header in [
            "X-Forwarded-For",
            "X-Forwarded-Host",
            "X-Real-IP",
            "Forwarded",
        ] {
            for method in ["GET", "POST"] {
                let reply = send(
                    &dash,
                    request(method, &path, &[("Origin", ORIGIN), (header, "127.0.0.1")]),
                    b"",
                );
                assert_eq!(reply.status, "403 Forbidden", "{method} {header}");
                assert!(reply.header_value("Set-Cookie").is_none());
            }
        }
        assert!(
            dash.with_state(|s| s.local_code_live(&path["/local/".len()..], Instant::now())),
            "被拒的请求不消耗本机码"
        );
        let entered = send(&dash, request("POST", &path, &[("Origin", ORIGIN)]), b"");
        assert_eq!(entered.status, "303 See Other");
    }

    #[test]
    fn local_code_is_single_use_and_loopback_only() {
        let (dash, allowed) = dash();
        let url = dash.issue_local().unwrap();
        assert!(url.starts_with("http://127.0.0.1:7767/local/"), "{url}");
        let path = url[url.find("/local/").unwrap()..].to_string();

        let landing = send(&dash, request("GET", &path, &[]), b"");
        assert_eq!(landing.status, "200 OK");
        assert!(body_text(&landing).contains("method=\"post\""));
        let remote = handle(
            &dash,
            &request("POST", &path, &[("Origin", ORIGIN)]),
            b"",
            false,
        );
        assert_eq!(remote.status, "403 Forbidden", "别的机器用不了本机码");
        let foreign = send(
            &dash,
            request("POST", &path, &[("Origin", "http://evil.example")]),
            b"",
        );
        assert_eq!(foreign.status, "403 Forbidden");

        let entered = send(&dash, request("POST", &path, &[("Origin", ORIGIN)]), b"");
        assert_eq!(entered.status, "303 See Other");
        assert_eq!(entered.header_value("Location"), Some("/"));
        let cookie = cookie_pair(&entered);
        assert_eq!(
            send(&dash, request("POST", &path, &[]), b"").status,
            "404 Not Found"
        );
        assert_eq!(
            send(&dash, request("GET", &path, &[]), b"").status,
            "404 Not Found"
        );

        allowed.store(false, Ordering::SeqCst);
        let me = body_json(&api(&dash, WHOAMI, &cookie, None));
        assert_eq!(
            me["data"]["identity"],
            json!({"kind": "owner"}),
            "本机 owner 不经名单重判"
        );
    }

    #[test]
    fn sessions_are_memory_only() {
        let (first, _) = dash();
        let cookie = open_local(&first);
        assert_eq!(api(&first, WHOAMI, &cookie, None).status, "200 OK");
        let (restarted, _) = dash();
        assert_eq!(
            api(&restarted, WHOAMI, &cookie, None).status,
            "401 Unauthorized",
            "新实例(daemon 重启)不认旧 cookie"
        );
    }

    #[test]
    fn license_notices_revalidate_while_hashed_assets_keep_long_cache() {
        let (dash, _) = dash();
        let notice = send(
            &dash,
            request("GET", "/assets/third-party-licenses.txt", &[]),
            b"",
        );
        assert_eq!(notice.status, "200 OK");
        assert_eq!(notice.header_value("Cache-Control"), Some("no-cache"));
        assert!(body_text(&notice).contains("BSD 3-Clause License"));

        let index = std::str::from_utf8(assets::page(assets::Page::Index).bytes).unwrap();
        let script = index
            .split("src=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert!(script.starts_with("/assets/") && script.ends_with(".js"));
        let asset = send(&dash, request("GET", script, &[]), b"");
        assert_eq!(asset.status, "200 OK");
        assert_eq!(
            asset.header_value("Cache-Control"),
            Some("public, max-age=31536000, immutable")
        );
    }

    #[test]
    fn unknown_paths_and_files_404() {
        let (dash, _) = dash();
        for path in [
            "/favicon.ico",
            "/assets/missing.js",
            "/_/nope.js",
            "/l/short",
            "/local/x",
        ] {
            assert_eq!(
                send(&dash, request("GET", path, &[]), b"").status,
                "404 Not Found",
                "{path}"
            );
        }
        assert_eq!(
            send(&dash, request("POST", "/", &[]), b"").status,
            "404 Not Found"
        );
    }

    /// 飞书表情图要先登录;登录后才去查表、取图(测试里没接 daemon 环境,停在 503)。
    #[test]
    fn emoji_images_need_a_session() {
        let (dash, _) = dash();
        let anonymous = send(&dash, request("GET", "/emoji/OK", &[]), b"");
        assert_eq!(anonymous.status, "401 Unauthorized");
        let cookie = open_local(&dash);
        let signed_in = send(
            &dash,
            request("GET", "/emoji/OK", &[("Cookie", cookie.as_str())]),
            b"",
        );
        assert_eq!(signed_in.status, "503 Service Unavailable");
    }

    #[test]
    fn registry_holds_the_session_methods() {
        let registry = registry();
        assert!(registry.get(WHOAMI).is_some());
        assert!(registry.get("status.get").is_some());
        assert!(registry.get("preview.render").is_some());
    }

    /// 走真实连接:请求体上限按声明判(超过 1 MiB 回 413、不去读),正好 1 MiB 照常读完往下走。
    #[test]
    fn api_body_over_1mib_rejected() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let dash = Dashboard::spawn_for_test(
            &summon_core::config::Dashboard {
                port: 0,
                ..Default::default()
            },
            Arc::clone(&shutdown),
        )
        .unwrap();
        let port = dash.local_port();
        let post = |len: usize, send_body: bool| {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let head = format!(
                "POST /api/session.whoami HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
Origin: http://127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n"
            );
            s.write_all(head.as_bytes()).unwrap();
            if send_body {
                let mut body = vec![b' '; len];
                body[0] = b'{';
                body[len - 1] = b'}';
                s.write_all(&body).unwrap();
            }
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            out
        };
        let over = post(LIMITS.body + 1, false);
        assert!(over.starts_with("HTTP/1.1 413"), "{over}");
        assert!(over.contains("\"code\":\"invalid\""), "{over}");
        let at_limit = post(LIMITS.body, true);
        assert!(
            at_limit.starts_with("HTTP/1.1 401"),
            "正好 1 MiB 照常读完,往下走到会话校验:{}",
            &at_limit[..at_limit.len().min(200)]
        );
        assert_eq!(LIMITS.body, 1024 * 1024);

        let many: String = (0..LIMITS.headers + 1)
            .map(|i| format!("X-H{i}: v\r\n"))
            .collect();
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(format!("GET / HTTP/1.1\r\n{many}\r\n").as_bytes())
            .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 431"), "{out}");
        shutdown.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while dash.active_connections() > 0 && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(dash.active_connections(), 0);
    }
}
