//! Cursor 个人额度的纯逻辑层:从登录凭据拼会话 cookie、从 `usage-summary` 响应算剩余百分比。
//!
//! Cursor 的 ACP、`-p stream-json` 与 statusLine 输入都不带额度,额度只在 cursor.com 设置页
//! 自己用的 `GET /api/usage-summary` 里。这个接口只认 `WorkosCursorSessionToken` cookie
//! (同一枚 token 放 `Authorization: Bearer` 实测 401)。本模块不碰网络、文件与时钟。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::Value;

use crate::config::CursorQuota;
use crate::driver::CliDriver;
use crate::registry::Agent;

/// 额度查询的端点。无公开文档,字段变了就解析不出额度,由调用方按「不显示」处理。
pub const USAGE_SUMMARY_URL: &str = "https://cursor.com/api/usage-summary";

/// cursor-agent 在 macOS 上把登录 token 存进钥匙串的条目名(服务 / 账户),与它自己的实现对齐。
pub const KEYCHAIN_SERVICE: &str = "cursor-access-token";
pub const KEYCHAIN_ACCOUNT: &str = "cursor-user";

/// 额度凭据从哪来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// agent.yaml 的 `runtime.cursor_quota_token_command`,在该 agent 自己的环境里跑。
    AgentCommand { agent: String, command: String },
    /// config.yaml 的 `cursor.quota.token_command`,只带系统基础环境。
    GlobalCommand(String),
    /// cursor-agent 自己的登录态。
    CursorAgentLogin,
}

impl TokenSource {
    /// 频控与结果共享的键:同一来源拿到的是同一个账号,几个 agent 共用一份查询结果。
    /// agent 级命令跑在各自的环境里,同一条命令在两个 agent 下未必是同一个账号,所以键里带 agent。
    pub fn key(&self) -> String {
        match self {
            TokenSource::AgentCommand { agent, command } => format!("agent\0{agent}\0{command}"),
            TokenSource::GlobalCommand(command) => format!("global\0{command}"),
            TokenSource::CursorAgentLogin => "cursor-agent-login".to_string(),
        }
    }
}

/// 这个 agent 的额度该拿哪份凭据查。证明不了凭据与 agent 实际登录的是同一个 Cursor 账号时
/// 返回 None —— 拿错账号的额度比不显示更糟。
///
/// 判据与 Codex 的默认账号判据同构:配了 `cli_cmd` 或 `env` 的 agent 可能换了账号
/// (`HOME`、`XDG_CONFIG_HOME`、`AGENT_CLI_CREDENTIAL_STORE`、包装脚本都做得到),只有它自己的
/// `runtime.cursor_quota_token_command` 说了算。
pub fn token_source(agent: &Agent, cfg: &CursorQuota) -> Option<TokenSource> {
    if CliDriver::from_kind(agent.cli_kind()) != CliDriver::Cursor {
        return None;
    }
    let runtime = agent.runtime.as_ref();
    if let Some(command) = runtime.and_then(|r| non_empty(r.cursor_quota_token_command.as_deref()))
    {
        return Some(TokenSource::AgentCommand {
            agent: agent.name.clone(),
            command: command.to_string(),
        });
    }
    let customized = runtime
        .is_some_and(|r| r.cli_cmd.is_some() || r.env.as_ref().is_some_and(|env| !env.is_empty()));
    if customized {
        return None;
    }
    Some(match non_empty(cfg.token_command.as_deref()) {
        Some(command) => TokenSource::GlobalCommand(command.to_string()),
        None => TokenSource::CursorAgentLogin,
    })
}

/// 这一轮要不要为额度动凭据:整机开关开着、这个会话打开了「额度显示」、且来源证得了账号。
/// 任一条不满足就返回 None —— 一次也不查,也不占频控名额。没人看的额度,不值得把凭据发出去。
pub fn source_for_turn(agent: &Agent, show_quota: bool, cfg: &CursorQuota) -> Option<TokenSource> {
    if !cfg.enabled || !show_quota {
        return None;
    }
    token_source(agent, cfg)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// cursor-agent 登录态的落点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStore {
    Keychain {
        service: &'static str,
        account: &'static str,
    },
    File(PathBuf),
}

/// 按 cursor-agent 自己的存储选择定落点:macOS 只用钥匙串(没有文件回退 —— 盘上即使留着
/// `~/.cursor/auth.json`,那也是别的存储模式或旧版本留下的,不是 agent 此刻在用的账号),
/// 其余平台是 `$XDG_CONFIG_HOME/cursor/auth.json`(未设或为空时 `~/.config`)。
/// summon 起 cursor 时清空环境,切换存储模式的 `AGENT_CLI_CREDENTIAL_STORE` 传不到 agent,
/// 所以这里只按默认模式对齐。
pub fn cursor_agent_login_store(
    macos: bool,
    home: &Path,
    xdg_config_home: Option<&Path>,
) -> LoginStore {
    if macos {
        return LoginStore::Keychain {
            service: KEYCHAIN_SERVICE,
            account: KEYCHAIN_ACCOUNT,
        };
    }
    let config = xdg_config_home
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"));
    LoginStore::File(config.join("cursor").join("auth.json"))
}

/// 一次失败该怎么对待。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 网络抖动、服务端错误、限流:过一个间隔再试,期间保留已知值。
    Transient,
    /// 凭据缺失/失效、接口拒绝或改版:已知值不再可信,清掉。
    Unavailable,
}

/// HTTP 状态 → 失败类别;2xx 返回 None。重定向同样算拒绝:cookie 只该发给这一个端点,
/// 不跟随跳转,跳走就说明接口变了。
pub fn classify_http_status(code: u16) -> Option<FailureKind> {
    match code {
        200..=299 => None,
        429 | 500..=599 => Some(FailureKind::Transient),
        _ => Some(FailureKind::Unavailable),
    }
}

/// 一个来源最近一次查询的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceResult {
    Quota(IndividualQuota),
    /// 凭据缺失/失效、接口拒绝或响应里没有个人额度:不该再显示任何旧值。
    Unavailable,
}

/// 轮开跑时该做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Begin {
    /// 名额归本轮:去查。
    Query,
    /// 间隔内已查过:沿用该来源最近的结论(查询还在路上时为 None,等它回来会一并写给本 agent)。
    Reuse(Option<SourceResult>),
}

#[derive(Debug, Default)]
struct SourceEntry {
    last_attempt: Option<Instant>,
    result: Option<SourceResult>,
}

/// 各凭据来源的频控与结果。同一来源 `interval` 内只查一次,失败也计入;结果回来时写给此刻
/// 仍用这个来源的每个 agent,所以同一账号下的几个 agent 不会各查各的、也不会有人一直拿不到值。
#[derive(Debug, Default)]
pub struct QuotaSources {
    entries: HashMap<String, SourceEntry>,
    /// 每个 agent 当前用的来源。agent 改配置换了来源(热重载不重启 daemon),旧来源的结果
    /// 不能再落到它头上 —— 那是另一个账号的额度。
    current: HashMap<String, String>,
}

impl QuotaSources {
    pub fn begin(&mut self, key: &str, agent: &str, now: Instant, interval: Duration) -> Begin {
        self.current.insert(agent.to_string(), key.to_string());
        let entry = self.entries.entry(key.to_string()).or_default();
        let recent = entry
            .last_attempt
            .is_some_and(|last| now.saturating_duration_since(last) < interval);
        if recent {
            return Begin::Reuse(entry.result);
        }
        entry.last_attempt = Some(now);
        Begin::Query
    }

    /// 记下查询结论,返回该写给哪些 agent(此刻仍用这个来源的那些)。
    pub fn finish(&mut self, key: &str, result: SourceResult) -> Vec<String> {
        let Some(entry) = self.entries.get_mut(key) else {
            return Vec::new();
        };
        entry.result = Some(result);
        let mut agents: Vec<String> = self
            .current
            .iter()
            .filter(|(_, source)| source.as_str() == key)
            .map(|(agent, _)| agent.clone())
            .collect();
        agents.sort();
        agents
    }

    /// agent 不再有可证的来源(换了账号配置、整机关掉、不再是 cursor):解除归属。
    /// 返回它原本是否挂在某个来源上 —— 挂过的,账号缓存里那份额度也该清掉。
    pub fn detach(&mut self, agent: &str) -> bool {
        self.current.remove(agent).is_some()
    }
}

/// 一次查询的结局 → 该记下的结论。暂时性失败返回 None:什么也不改,已知值保留到下次查询。
pub fn settle(outcome: Result<IndividualQuota, FailureKind>) -> Option<SourceResult> {
    match outcome {
        Ok(quota) => Some(SourceResult::Quota(quota)),
        Err(FailureKind::Transient) => None,
        Err(FailureKind::Unavailable) => Some(SourceResult::Unavailable),
    }
}

impl SourceResult {
    /// 写进账号缓存的值:`None` = 清掉。
    pub fn cached_quota(self) -> Option<(u8, Option<i64>)> {
        match self {
            SourceResult::Quota(quota) => Some((quota.left_pct, quota.resets_at)),
            SourceResult::Unavailable => None,
        }
    }
}

/// 一次查询得到的个人额度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndividualQuota {
    /// 剩余百分比 0..=100,与角标 `quota_left_pct` 同一口径。
    pub left_pct: u8,
    /// 计费周期结束时刻(Unix 秒)。额度缓存靠它分辨新旧周期:周期一换,旧观测不再有效。
    pub resets_at: Option<i64>,
}

/// access token(JWT)→ 请求头里那条 cookie(`WorkosCursorSessionToken=<user id>%3A%3A<token>`)。
///
/// user id 取 JWT `sub` 最后一个 `|` 之后那段(`auth0|user_xxx` → `user_xxx`)。
/// token 与 user id 都会原样进请求头,所以字符集卡死在 JWT / id 实际用到的范围内:不是三段式、
/// 解不出 `sub`、或带了别的字符 → None。宁可这次不查,也不拼出一条畸形的头。
pub fn session_cookie(access_token: &str) -> Option<String> {
    let token = access_token.trim();
    let is_token_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.');
    if token.is_empty() || !token.bytes().all(is_token_byte) {
        return None;
    }
    let mut segments = token.split('.');
    let (Some(_), Some(payload), Some(_), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return None;
    };
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&claims).ok()?;
    let user = claims.get("sub")?.as_str()?.rsplit('|').next()?;
    let is_user_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_');
    if user.is_empty() || !user.bytes().all(is_user_byte) {
        return None;
    }
    Some(format!("WorkosCursorSessionToken={user}%3A%3A{token}"))
}

/// cursor-agent 的 `auth.json`(`{"accessToken": "...", "refreshToken": "..."}`)→ access token。
pub fn access_token_from_auth_json(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    let token = value.get("accessToken")?.as_str()?.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// `usage-summary` 响应 → 个人额度剩余。
///
/// 个人额度在 `individualUsage` 下:企业账号给的是 `overall`,个人订阅给的是 `plan`(套餐内额度)。
/// 同级的 `onDemand` 是用完套餐后的按量付费,不算额度;`teamUsage` 是整个团队的池子,也不是
/// 个人的。`isUnlimited`、桶被关掉、上限缺失或非正 → None:这些情形下没有「剩余百分比」,
/// 显示任何数都是编的。
pub fn quota_from_usage_summary(body: &Value) -> Option<IndividualQuota> {
    if body.get("isUnlimited").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let usage = body.get("individualUsage")?;
    let (limit, bucket) = ["overall", "plan"]
        .into_iter()
        .filter_map(|key| usage.get(key))
        .filter(|bucket| bucket.get("enabled").and_then(Value::as_bool) != Some(false))
        .find_map(|bucket| {
            let limit = bucket.get("limit")?.as_f64()?;
            (limit.is_finite() && limit > 0.0).then_some((limit, bucket))
        })?;
    let remaining = match bucket.get("remaining").and_then(Value::as_f64) {
        Some(remaining) => remaining,
        None => limit - bucket.get("used")?.as_f64()?,
    };
    if !remaining.is_finite() {
        return None;
    }
    let left_pct = (remaining / limit * 100.0).round().clamp(0.0, 100.0) as u8;
    let resets_at = body
        .get("billingCycleEnd")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp());
    Some(IndividualQuota {
        left_pct,
        resets_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt_with_sub(sub: &str) -> String {
        let enc = |v: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string().as_bytes())
        };
        format!(
            "{}.{}.c2lnbmF0dXJl",
            enc(&json!({"alg": "HS256", "typ": "JWT"})),
            enc(&json!({"sub": sub, "exp": 1_800_000_000}))
        )
    }

    /// 2026-09-18 企业账号真机响应(数值原样,仅删掉与额度无关的展示文案)。
    fn enterprise_summary() -> Value {
        json!({
            "billingCycleStart": "2026-09-01T00:00:00.000Z",
            "billingCycleEnd": "2026-10-01T00:00:00.000Z",
            "membershipType": "enterprise",
            "limitType": "team",
            "isUnlimited": false,
            "individualUsage": {
                "overall": {"enabled": true, "used": 3942, "limit": 100000, "remaining": 96058}
            },
            "teamUsage": {
                "onDemand": {"enabled": true, "used": 616643134, "limit": 961870000, "remaining": 345226866}
            }
        })
    }

    fn agent(yaml: &str) -> Agent {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn global(token_command: Option<&str>) -> CursorQuota {
        CursorQuota {
            token_command: token_command.map(str::to_string),
            ..CursorQuota::default()
        }
    }

    #[test]
    fn default_cursor_agent_reads_its_own_login() {
        let plain = agent("name: c\nruntime:\n  cli: cursor\n");
        assert_eq!(
            token_source(&plain, &global(None)),
            Some(TokenSource::CursorAgentLogin)
        );
        assert_eq!(
            token_source(&plain, &global(Some("  "))),
            Some(TokenSource::CursorAgentLogin),
            "空白命令等于没配"
        );
        assert_eq!(
            token_source(&plain, &global(Some("pass show cursor"))),
            Some(TokenSource::GlobalCommand("pass show cursor".into()))
        );
    }

    #[test]
    fn only_cursor_agents_have_a_cursor_quota() {
        for cli in ["claude", "codex", "traex"] {
            let other = agent(&format!("name: x\nruntime:\n  cli: {cli}\n"));
            assert_eq!(token_source(&other, &global(Some("echo t"))), None, "{cli}");
        }
    }

    #[test]
    fn customized_runtime_needs_its_own_token_command() {
        let with_env =
            agent("name: c\nruntime:\n  cli: cursor\n  env:\n    XDG_CONFIG_HOME: /tmp/other\n");
        let with_wrapper = agent("name: c\nruntime:\n  cli: cursor\n  cli_cmd: /opt/cursor-wrap\n");
        for customized in [&with_env, &with_wrapper] {
            assert_eq!(
                token_source(customized, &global(None)),
                None,
                "账号证不了就不查"
            );
            assert_eq!(
                token_source(customized, &global(Some("echo global"))),
                None,
                "全局命令读的是默认账号,同样证不了"
            );
        }

        let declared = agent(
            "name: c\nruntime:\n  cli: cursor\n  cli_cmd: /opt/cursor-wrap\n  cursor_quota_token_command: echo mine\n",
        );
        assert_eq!(
            token_source(&declared, &global(Some("echo global"))),
            Some(TokenSource::AgentCommand {
                agent: "c".into(),
                command: "echo mine".into()
            }),
            "agent 自己声明的来源压过全局"
        );
    }

    #[test]
    fn nothing_is_queried_unless_the_machine_and_the_chat_both_opt_in() {
        let plain = agent("name: c\nruntime:\n  cli: cursor\n");
        let on = CursorQuota::default();
        assert_eq!(
            source_for_turn(&plain, true, &on),
            Some(TokenSource::CursorAgentLogin)
        );
        assert_eq!(
            source_for_turn(&plain, false, &on),
            None,
            "会话没开额度显示"
        );
        let off = CursorQuota {
            enabled: false,
            ..CursorQuota::default()
        };
        assert_eq!(source_for_turn(&plain, true, &off), None, "整机关掉");
    }

    #[test]
    fn agents_on_the_default_login_share_one_source_key() {
        let a = token_source(&agent("name: a\nruntime:\n  cli: cursor\n"), &global(None));
        let b = token_source(&agent("name: b\nruntime:\n  cli: cursor\n"), &global(None));
        assert_eq!(a.unwrap().key(), b.unwrap().key());

        let own = |name: &str| TokenSource::AgentCommand {
            agent: name.into(),
            command: "echo t".into(),
        };
        assert_ne!(
            own("a").key(),
            own("b").key(),
            "agent 级命令跑在各自环境里,不共享"
        );
        assert_ne!(
            TokenSource::GlobalCommand("echo t".into()).key(),
            own("a").key()
        );
    }

    #[test]
    fn login_store_follows_cursor_agent_platform_layout() {
        let home = Path::new("/home/u");
        assert_eq!(
            cursor_agent_login_store(true, home, Some(Path::new("/x"))),
            LoginStore::Keychain {
                service: "cursor-access-token",
                account: "cursor-user"
            },
            "macOS 只有钥匙串,没有文件回退"
        );
        assert_eq!(
            cursor_agent_login_store(false, home, None),
            LoginStore::File(PathBuf::from("/home/u/.config/cursor/auth.json"))
        );
        assert_eq!(
            cursor_agent_login_store(false, home, Some(Path::new("/xdg"))),
            LoginStore::File(PathBuf::from("/xdg/cursor/auth.json"))
        );
        assert_eq!(
            cursor_agent_login_store(false, home, Some(Path::new(""))),
            LoginStore::File(PathBuf::from("/home/u/.config/cursor/auth.json"))
        );
    }

    #[test]
    fn one_query_per_source_per_interval_shared_by_its_agents() {
        let mut sources = QuotaSources::default();
        let interval = Duration::from_secs(600);
        let t0 = Instant::now();
        let quota = SourceResult::Quota(IndividualQuota {
            left_pct: 96,
            resets_at: None,
        });

        assert_eq!(sources.begin("k", "a", t0, interval), Begin::Query);
        assert_eq!(
            sources.begin("k", "b", t0 + Duration::from_secs(1), interval),
            Begin::Reuse(None),
            "查询还在路上:不再发第二次"
        );
        assert_eq!(
            sources.finish("k", quota),
            vec!["a".to_string(), "b".to_string()],
            "结果写给用这个来源的每个 agent"
        );
        assert_eq!(
            sources.begin("k", "c", t0 + Duration::from_secs(599), interval),
            Begin::Reuse(Some(quota))
        );
        assert_eq!(
            sources.begin("other", "d", t0, interval),
            Begin::Query,
            "来源之间互不影响"
        );
        assert_eq!(
            sources.begin("k", "a", t0 + interval, interval),
            Begin::Query
        );
        assert_eq!(sources.finish("k", SourceResult::Unavailable).len(), 3);
        assert!(sources
            .finish("never-begun", SourceResult::Unavailable)
            .is_empty());
    }

    #[test]
    fn an_agent_that_changed_source_no_longer_receives_the_old_accounts_quota() {
        let mut sources = QuotaSources::default();
        let interval = Duration::from_secs(600);
        let t0 = Instant::now();
        let quota = SourceResult::Quota(IndividualQuota {
            left_pct: 80,
            resets_at: None,
        });
        sources.begin("default", "x", t0, interval);
        sources.begin("default", "y", t0, interval);
        sources.begin("x-own", "x", t0 + Duration::from_secs(1), interval);
        assert_eq!(sources.finish("default", quota), vec!["y".to_string()]);

        assert!(sources.detach("y"), "挂过来源的 agent 解除时要报告");
        assert!(!sources.detach("y"));
        assert!(sources.finish("default", quota).is_empty());
    }

    #[test]
    fn only_a_transient_failure_keeps_the_known_quota() {
        let quota = IndividualQuota {
            left_pct: 42,
            resets_at: Some(7),
        };
        assert_eq!(settle(Ok(quota)), Some(SourceResult::Quota(quota)));
        assert_eq!(settle(Err(FailureKind::Transient)), None);
        assert_eq!(
            settle(Err(FailureKind::Unavailable)),
            Some(SourceResult::Unavailable)
        );
        assert_eq!(
            SourceResult::Quota(quota).cached_quota(),
            Some((42, Some(7)))
        );
        assert_eq!(SourceResult::Unavailable.cached_quota(), None);
    }

    #[test]
    fn only_server_side_trouble_keeps_the_known_quota() {
        assert_eq!(classify_http_status(200), None);
        assert_eq!(classify_http_status(429), Some(FailureKind::Transient));
        assert_eq!(classify_http_status(503), Some(FailureKind::Transient));
        for code in [301, 302, 400, 401, 403, 404] {
            assert_eq!(
                classify_http_status(code),
                Some(FailureKind::Unavailable),
                "{code}"
            );
        }
    }

    #[test]
    fn cookie_carries_user_id_after_the_provider_prefix() {
        let token = jwt_with_sub("auth0|user_01ABC");
        assert_eq!(
            session_cookie(&token),
            Some(format!("WorkosCursorSessionToken=user_01ABC%3A%3A{token}"))
        );
    }

    #[test]
    fn cookie_trims_surrounding_whitespace_from_command_output() {
        let token = jwt_with_sub("auth0|user_01ABC");
        let cookie = session_cookie(&format!("  {token}\n")).unwrap();
        assert!(cookie.ends_with(&token));
    }

    #[test]
    fn malformed_tokens_never_become_a_header() {
        let token = jwt_with_sub("auth0|user_01ABC");
        assert_eq!(session_cookie(""), None);
        assert_eq!(session_cookie("not-a-jwt"), None);
        assert_eq!(session_cookie(&format!("{token}.extra")), None);
        assert_eq!(session_cookie(&format!("{token}\r\nX-Evil: 1")), None);
        assert_eq!(session_cookie("a.%%%.c"), None, "payload 不是 base64url");
        assert_eq!(session_cookie(&jwt_with_sub("")), None);
        assert_eq!(session_cookie(&jwt_with_sub("auth0|user;x=1")), None);
    }

    #[test]
    fn auth_json_yields_the_access_token_only() {
        assert_eq!(
            access_token_from_auth_json(r#"{"accessToken":" tok ","refreshToken":"ref"}"#),
            Some("tok".to_string())
        );
        assert_eq!(
            access_token_from_auth_json(r#"{"refreshToken":"ref"}"#),
            None
        );
        assert_eq!(access_token_from_auth_json(r#"{"accessToken":""}"#), None);
        assert_eq!(access_token_from_auth_json("not json"), None);
    }

    #[test]
    fn enterprise_overall_bucket_is_the_individual_quota() {
        assert_eq!(
            quota_from_usage_summary(&enterprise_summary()),
            Some(IndividualQuota {
                left_pct: 96,
                resets_at: Some(1_790_812_800),
            })
        );
    }

    #[test]
    fn personal_plan_bucket_is_used_and_on_demand_is_not() {
        let body = json!({
            "billingCycleEnd": "2026-10-01T00:00:00.000Z",
            "individualUsage": {
                "plan": {"enabled": true, "used": 1500, "limit": 2000, "remaining": 500},
                "onDemand": {"enabled": true, "used": 0, "limit": 10000, "remaining": 10000}
            }
        });
        assert_eq!(quota_from_usage_summary(&body).unwrap().left_pct, 25);
    }

    #[test]
    fn remaining_falls_back_to_limit_minus_used() {
        let body = json!({"individualUsage": {"plan": {"used": 500, "limit": 2000}}});
        assert_eq!(
            quota_from_usage_summary(&body),
            Some(IndividualQuota {
                left_pct: 75,
                resets_at: None,
            })
        );
    }

    #[test]
    fn overspent_bucket_clamps_to_zero() {
        let body =
            json!({"individualUsage": {"plan": {"used": 2500, "limit": 2000, "remaining": -500}}});
        assert_eq!(quota_from_usage_summary(&body).unwrap().left_pct, 0);
    }

    #[test]
    fn no_meaningful_percentage_means_no_quota() {
        let mut unlimited = enterprise_summary();
        unlimited["isUnlimited"] = json!(true);
        assert_eq!(quota_from_usage_summary(&unlimited), None);

        let zero_limit = json!({"individualUsage": {"overall": {"used": 0, "limit": 0}}});
        assert_eq!(quota_from_usage_summary(&zero_limit), None);

        let disabled =
            json!({"individualUsage": {"overall": {"enabled": false, "used": 1, "limit": 10}}});
        assert_eq!(quota_from_usage_summary(&disabled), None);

        let team_only = json!({"teamUsage": {"onDemand": {"used": 1, "limit": 10}}});
        assert_eq!(quota_from_usage_summary(&team_only), None);
    }
}
