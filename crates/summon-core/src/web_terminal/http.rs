//! 终端服务的路由、cookie 与同源判断。只认 `/t/` 前缀下几条固定形状的路径。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asset {
    XtermJs,
    XtermCss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route<'a> {
    Landing(&'a str),
    Enter(&'a str),
    Page(&'a str),
    Socket(&'a str),
    Asset(Asset),
    NotFound,
}

/// 码、会话句柄、会话密钥都是 32 位小写 hex(128-bit)。
pub fn is_token(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn route<'a>(method: &str, target: &'a str) -> Route<'a> {
    let path = target.split('?').next().unwrap_or(target);
    let Some(rest) = path.strip_prefix("/t/") else {
        return Route::NotFound;
    };
    let parts: Vec<&'a str> = rest.split('/').collect();
    match (method, parts.as_slice()) {
        ("GET", ["assets", "xterm.js"]) => Route::Asset(Asset::XtermJs),
        ("GET", ["assets", "xterm.css"]) => Route::Asset(Asset::XtermCss),
        ("GET", ["s", h]) if is_token(h) => Route::Page(h),
        ("GET", ["s", h, "ws"]) if is_token(h) => Route::Socket(h),
        ("GET", [c]) if is_token(c) => Route::Landing(c),
        ("POST", [c, "enter"]) if is_token(c) => Route::Enter(c),
        _ => Route::NotFound,
    }
}

pub fn cookie_name(handle: &str) -> String {
    format!("st_{handle}")
}

pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header
        .split(';')
        .map(str::trim)
        .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
}

/// Path 收窄到这个会话自己的页面:同一浏览器开着几个终端,cookie 互不串。
pub fn set_cookie(handle: &str, secret: &str, max_age_secs: i64) -> String {
    format!(
        "{}={secret}; HttpOnly; SameSite=Strict; Path=/t/s/{handle}; Max-Age={max_age_secs}",
        cookie_name(handle)
    )
}

/// WebSocket 不受同源策略约束,任何网页都能向它发起连接并带上 cookie;只认页面自己发起的。
pub fn same_origin(origin: Option<&str>, host: Option<&str>) -> bool {
    matches!((origin, host), (Some(o), Some(h)) if o == format!("http://{h}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn routes_only_accept_well_formed_tokens_and_the_right_method() {
        assert_eq!(route("GET", &format!("/t/{T}")), Route::Landing(T));
        assert_eq!(
            route("GET", &format!("/t/{T}?from=feishu")),
            Route::Landing(T)
        );
        assert_eq!(route("POST", &format!("/t/{T}/enter")), Route::Enter(T));
        assert_eq!(route("GET", &format!("/t/s/{T}")), Route::Page(T));
        assert_eq!(route("GET", &format!("/t/s/{T}/ws")), Route::Socket(T));
        assert_eq!(
            route("GET", "/t/assets/xterm.js"),
            Route::Asset(Asset::XtermJs)
        );
        assert_eq!(
            route("GET", "/t/assets/xterm.css"),
            Route::Asset(Asset::XtermCss)
        );
        assert_eq!(
            route("GET", &format!("/t/{T}/enter")),
            Route::NotFound,
            "进入必须是 POST"
        );
        assert_eq!(route("POST", &format!("/t/{T}")), Route::NotFound);
        assert_eq!(route("GET", "/t/NOT-A-TOKEN"), Route::NotFound);
        assert_eq!(
            route("GET", &format!("/t/{}", T.to_uppercase())),
            Route::NotFound
        );
        assert_eq!(route("GET", &format!("/p/{T}")), Route::NotFound);
    }

    #[test]
    fn cookie_lookup_matches_the_exact_name() {
        let header = format!("st_{T}x=nope; st_{T}=secret1; other=1");
        assert_eq!(cookie_value(&header, &cookie_name(T)), Some("secret1"));
        assert_eq!(cookie_value("other=1", &cookie_name(T)), None);
    }

    #[test]
    fn the_session_cookie_is_http_only_strict_and_scoped_to_its_page() {
        let c = set_cookie(T, "s3cret", 7200);
        assert!(c.starts_with(&format!("st_{T}=s3cret;")));
        for attr in [
            "HttpOnly",
            "SameSite=Strict",
            &format!("Path=/t/s/{T}"),
            "Max-Age=7200",
        ] {
            assert!(c.contains(attr), "{c} 缺 {attr}");
        }
    }

    #[test]
    fn websocket_origin_must_be_the_page_itself() {
        assert!(same_origin(
            Some("http://10.0.0.5:7766"),
            Some("10.0.0.5:7766")
        ));
        assert!(!same_origin(
            Some("http://evil.example"),
            Some("10.0.0.5:7766")
        ));
        assert!(!same_origin(None, Some("10.0.0.5:7766")));
        assert!(!same_origin(Some("http://10.0.0.5:7766"), None));
    }
}
