//! 控制台的链接、浏览器会话与本机码。全部只在内存:daemon 重启即失效,重新发 `/dash` 或
//! 重新 `summon dashboard open` 即可。时钟与随机数由调用方注入,过期可以直接测。
//!
//! 进入的两条路:`/dash` 私聊给发起人的一次性链接(落地页 POST 进入时按此刻名单重判签发人),
//! 或 control.sock 上本机 owner 要来的一次性本机码。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

/// 会话 cookie 的名字;与预览的 `summon_preview`、终端的 `st_*` 不重名(cookie 不按端口隔离)。
pub(crate) const COOKIE: &str = "summon_dash";
/// 浏览器会话的绝对有效期,与文件预览相同。
pub(crate) const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// 本机码的有效期;第一次使用即消耗。
pub(crate) const LOCAL_CODE_TTL: Duration = Duration::from_secs(60);

/// 签发 chat 的类型:判权按它的语义走(私聊名单为空时人人可操作那条只对私聊成立)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChatKind {
    P2p,
    Group,
}

/// 签发链接的那一次 `/dash`:谁、经哪个 agent、在哪个 chat 上发起。身份只来自飞书事件里的
/// 发起人,浏览器提交的任何身份信息都不采信。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issuer {
    pub(crate) agent: String,
    pub(crate) chat_id: String,
    pub(crate) chat_kind: ChatKind,
    /// 只在签发 agent 的飞书应用内有效;跨 agent 判权用 `union_id`。
    pub(crate) open_id: String,
    pub(crate) union_id: String,
    /// 飞书名字,只做展示。
    pub(crate) name: String,
}

/// 会话的主体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Principal {
    /// control.sock 上的本机 owner(经本机码进入)。
    Owner,
    /// 飞书卡片按钮的点击人,即签发人本人。
    Feishu(Issuer),
}

/// 每个请求重判进入权限:签发人此刻是否仍过签发 agent 在签发 chat 上的 operate-gate。
/// 实现要现读盘上名单(撤掉的 operator 当场失效)。
pub(crate) trait Gate: Send + Sync {
    fn still_allowed(&self, issuer: &Issuer) -> bool;
}

/// 一个浏览器会话。密钥只在 HttpOnly cookie 里;`csrf` 由 `session.whoami` 交给页面,每个写请求
/// 放进 `X-Summon-Csrf` 头。
#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub(crate) principal: Principal,
    pub(crate) csrf: String,
    created: Instant,
    /// 只做展示(`WhoAmI.expires_at`);过期按 `created` 判。
    pub(crate) expires_at: chrono::DateTime<chrono::Utc>,
}

struct Link {
    issuer: Issuer,
    created: Instant,
}

/// 用一条链接进入的结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// 不认识、已过期或已用过的链接。
    Spent,
    /// 新会话的密钥(调用方发 cookie)。
    Granted(String),
    /// 链接已用掉,但随机源出错、发不出会话。重新发 `/dash` 即可。
    Failed,
}

pub(crate) struct Sessions {
    link_ttl: Duration,
    links: HashMap<String, Link>,
    sessions: HashMap<String, Session>,
    local_codes: HashMap<String, Instant>,
}

impl Sessions {
    /// `link_ttl` 是链接的有效期,与文件预览的 `preview_server.token_ttl_secs` 相同。
    pub(crate) fn new(link_ttl: Duration) -> Self {
        Self {
            link_ttl: link_ttl.max(Duration::from_secs(1)),
            links: HashMap::new(),
            sessions: HashMap::new(),
            local_codes: HashMap::new(),
        }
    }

    /// 链接的有效期(私聊卡上写给人看)。
    pub(crate) fn link_ttl(&self) -> Duration {
        self.link_ttl
    }

    /// 签一条链接。随机源出错时什么也不出。
    pub(crate) fn issue_link(
        &mut self,
        issuer: Issuer,
        now: Instant,
        mut token: impl FnMut() -> Option<String>,
    ) -> Option<String> {
        self.prune(now);
        let t = token()?;
        self.links.insert(
            t.clone(),
            Link {
                issuer,
                created: now,
            },
        );
        Some(t)
    }

    fn live_link(&self, token: &str, now: Instant) -> Option<&Link> {
        self.links
            .get(token)
            .filter(|link| now.duration_since(link.created) <= self.link_ttl)
    }

    /// 链接还有效时,它的签发人。
    pub(crate) fn link_issuer(&self, token: &str, now: Instant) -> Option<&Issuer> {
        self.live_link(token, now).map(|link| &link.issuer)
    }

    /// 用掉一条链接,开一个属于签发人的会话。调用方已按此刻的名单判过签发人仍有权限。链接不论
    /// 成败都作废(一次性);浏览器带来的旧会话(若有)一并作废 —— 新 cookie 会盖掉它。
    pub(crate) fn redeem_link(
        &mut self,
        token: &str,
        presented: Option<&str>,
        now: Instant,
        mint: impl FnMut() -> Option<String>,
    ) -> Entry {
        let link = match self.links.remove(token) {
            Some(link) if now.duration_since(link.created) <= self.link_ttl => link,
            _ => return Entry::Spent,
        };
        if let Some(old) = presented {
            self.sessions.remove(old);
        }
        match self.open(Principal::Feishu(link.issuer), now, mint) {
            Some(secret) => Entry::Granted(secret),
            None => Entry::Failed,
        }
    }

    /// 签一个本机码(调用方已确认请求来自 control.sock 上的本机 owner)。
    pub(crate) fn issue_local(
        &mut self,
        now: Instant,
        mut token: impl FnMut() -> Option<String>,
    ) -> Option<String> {
        self.prune(now);
        let code = token()?;
        self.local_codes.insert(code.clone(), now);
        Some(code)
    }

    /// 本机码还能用(落地页据此决定给不给「进入」)。不消耗。
    pub(crate) fn local_code_live(&self, code: &str, now: Instant) -> bool {
        self.local_codes
            .get(code)
            .is_some_and(|issued| now.duration_since(*issued) <= LOCAL_CODE_TTL)
    }

    /// 用掉一个本机码,换一个本机 owner 的会话。码不论成败都作废。
    pub(crate) fn consume_local(
        &mut self,
        code: &str,
        now: Instant,
        mint: impl FnMut() -> Option<String>,
    ) -> Option<String> {
        if !self.local_code_live(code, now) {
            self.local_codes.remove(code);
            return None;
        }
        self.local_codes.remove(code);
        self.open(Principal::Owner, now, mint)
    }

    fn open(
        &mut self,
        principal: Principal,
        now: Instant,
        mut mint: impl FnMut() -> Option<String>,
    ) -> Option<String> {
        let secret = mint()?;
        let csrf = mint()?;
        let ttl = chrono::Duration::from_std(SESSION_TTL).unwrap_or(chrono::Duration::zero());
        self.sessions.insert(
            secret.clone(),
            Session {
                principal,
                csrf,
                created: now,
                expires_at: chrono::Utc::now() + ttl,
            },
        );
        Some(secret)
    }

    /// 凭 cookie 里的密钥找会话(未过期)。
    pub(crate) fn session(&self, secret: &str, now: Instant) -> Option<&Session> {
        self.sessions
            .get(secret)
            .filter(|s| now.duration_since(s.created) <= SESSION_TTL)
    }

    /// 作废一个会话(进入权限已被收回)。
    pub(crate) fn end(&mut self, secret: &str) {
        self.sessions.remove(secret);
    }

    /// 清掉过期的链接、会话与本机码。
    pub(crate) fn prune(&mut self, now: Instant) {
        let ttl = self.link_ttl;
        self.links
            .retain(|_, link| now.duration_since(link.created) <= ttl);
        self.sessions
            .retain(|_, s| now.duration_since(s.created) <= SESSION_TTL);
        self.local_codes
            .retain(|_, issued| now.duration_since(*issued) <= LOCAL_CODE_TTL);
    }
}

/// 常量时间比较:CSRF 头逐字节比完,不因第一个不同的字节提前返回。
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const ME_OPEN: &str = "ou_00000000000000000000000000000001";
    pub(crate) const GROUP: &str = "oc_00000000000000000000000000000001";

    pub(crate) fn issuer() -> Issuer {
        Issuer {
            agent: "starlight".into(),
            chat_id: GROUP.into(),
            chat_kind: ChatKind::Group,
            open_id: ME_OPEN.into(),
            union_id: "on_00000000000000000000000000000001".into(),
            name: "示例用户".into(),
        }
    }

    /// 按调用次序出 `000…1`、`000…2` 这样的 token。
    fn counter() -> impl FnMut() -> Option<String> {
        let mut n = 0u32;
        move || {
            n += 1;
            Some(format!("{n:032x}"))
        }
    }

    fn granted(entry: Entry) -> String {
        match entry {
            Entry::Granted(secret) => secret,
            other => panic!("期望进入,实际 {other:?}"),
        }
    }

    /// 链接只能用一次:进入后开一个属于签发人的会话,再用就是失效;浏览器带来的旧会话作废。
    #[test]
    fn a_link_opens_one_session_for_the_issuer_and_is_spent() {
        let t0 = Instant::now();
        let mut s = Sessions::new(Duration::from_secs(3600));
        let mut mint = counter();
        let old = s.issue_local(t0, &mut mint).unwrap();
        let old = s.consume_local(&old, t0, &mut mint).unwrap();
        let link = s.issue_link(issuer(), t0, &mut mint).unwrap();
        assert_eq!(s.link_issuer(&link, t0), Some(&issuer()));

        let secret = granted(s.redeem_link(&link, Some(&old), t0, &mut mint));
        let session = s.session(&secret, t0).unwrap();
        assert_eq!(session.principal, Principal::Feishu(issuer()));
        assert_ne!(session.csrf, secret, "CSRF 与会话密钥不同");
        assert!(s.session(&old, t0).is_none(), "旧会话作废");

        assert!(s.link_issuer(&link, t0).is_none(), "用过即作废");
        assert_eq!(s.redeem_link(&link, None, t0, &mut mint), Entry::Spent);
        assert_eq!(
            s.redeem_link(&"f".repeat(32), None, t0, &mut mint),
            Entry::Spent,
            "不认识的链接"
        );
    }

    #[test]
    fn links_expire_with_their_ttl() {
        let t0 = Instant::now();
        let ttl = Duration::from_secs(600);
        let mut s = Sessions::new(ttl);
        assert_eq!(s.link_ttl(), ttl);
        let link = s.issue_link(issuer(), t0, counter()).unwrap();
        assert!(s.link_issuer(&link, t0 + ttl).is_some());
        let after = t0 + ttl + Duration::from_secs(1);
        assert!(s.link_issuer(&link, after).is_none());
        assert_eq!(s.redeem_link(&link, None, after, counter()), Entry::Spent);
    }

    #[test]
    fn sessions_expire_after_twelve_hours() {
        assert_eq!(SESSION_TTL, Duration::from_secs(43_200));
        let t0 = Instant::now();
        let mut s = Sessions::new(Duration::from_secs(3600));
        let code = s.issue_local(t0, counter()).unwrap();
        let secret = s.consume_local(&code, t0, counter()).unwrap();
        assert!(s.session(&secret, t0 + SESSION_TTL).is_some());
        assert!(s
            .session(&secret, t0 + SESSION_TTL + Duration::from_secs(1))
            .is_none());
    }

    #[test]
    fn local_code_expires_after_60s() {
        assert_eq!(LOCAL_CODE_TTL, Duration::from_secs(60));
        let t0 = Instant::now();
        let mut s = Sessions::new(Duration::from_secs(3600));
        let code = s.issue_local(t0, counter()).unwrap();
        assert!(s.local_code_live(&code, t0 + LOCAL_CODE_TTL));
        let late = t0 + LOCAL_CODE_TTL + Duration::from_millis(1);
        assert!(!s.local_code_live(&code, late));
        assert!(s.consume_local(&code, late, counter()).is_none());
    }

    #[test]
    fn local_code_is_single_use_and_opens_an_owner_session() {
        let t0 = Instant::now();
        let mut s = Sessions::new(Duration::from_secs(3600));
        let mut mint = counter();
        let code = s.issue_local(t0, &mut mint).unwrap();
        let secret = s.consume_local(&code, t0, &mut mint).unwrap();
        assert_eq!(s.session(&secret, t0).unwrap().principal, Principal::Owner);
        assert!(
            s.consume_local(&code, t0, &mut mint).is_none(),
            "只能用一次"
        );
        assert!(!s.local_code_live(&code, t0));
    }

    #[test]
    fn a_failing_random_source_issues_nothing() {
        let t0 = Instant::now();
        let mut s = Sessions::new(Duration::from_secs(3600));
        assert!(s.issue_link(issuer(), t0, || None).is_none());
        assert!(s.issue_local(t0, || None).is_none());
        let link = s.issue_link(issuer(), t0, counter()).unwrap();
        assert_eq!(s.redeem_link(&link, None, t0, || None), Entry::Failed);
        assert_eq!(
            s.redeem_link(&link, None, t0, counter()),
            Entry::Spent,
            "失败也用掉了链接"
        );
    }

    #[test]
    fn ending_a_session_forgets_it() {
        let t0 = Instant::now();
        let mut s = Sessions::new(Duration::from_secs(3600));
        let code = s.issue_local(t0, counter()).unwrap();
        let secret = s.consume_local(&code, t0, counter()).unwrap();
        s.end(&secret);
        assert!(s.session(&secret, t0).is_none());
    }

    #[test]
    fn constant_time_eq_compares_whole_values() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
