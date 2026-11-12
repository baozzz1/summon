//! 终端链接的一次性码与浏览器会话。时间与随机数由调用方注入,本模块不碰 IO。

use std::collections::HashMap;

/// 没打开过的链接多久失效。
pub const CODE_TTL_MS: i64 = 10 * 60 * 1000;
/// 进入之后,这个浏览器多久内可以刷新 / 重连。
pub const SESSION_TTL_MS: i64 = 2 * 60 * 60 * 1000;
/// 收件人在终端卡上点了按钮之后,多久内能在落地页上「进入」。落地页还要手点一次,比预览的
/// 自动认领宽一些。
pub const ACTIVATION_WINDOW_MS: i64 = 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalMode {
    ReadOnly,
    Operate,
}

impl TerminalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read",
            Self::Operate => "operate",
        }
    }
}

/// 一组链接指向的会话与收件人。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantTarget {
    pub agent: String,
    pub chat_id: String,
    /// 终端页横幅上的会话名(群名或「私聊」),只做展示。
    pub chat_label: String,
    pub tmux_session: String,
    pub recipient_open_id: String,
}

/// 一次签发出来的两个码。`generation` 是这一组的代次:卡片确实送到之后拿它作废更早的那些
/// (见 [`TerminalGrants::revoke_superseded`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedCodes {
    pub read_only: String,
    pub operate: String,
    pub generation: u64,
}

/// 「进入」之后换到的浏览器会话。`handle` 出现在 URL 里,不是秘密;`secret` 只在 HttpOnly
/// cookie 里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSession {
    pub handle: String,
    secret: String,
    pub mode: TerminalMode,
    pub target: GrantTarget,
    pub expires_at_ms: i64,
    /// 这个页面是哪一代链接换来的。作废上一组时认它。
    generation: u64,
}

impl BrowserSession {
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

/// 作废的粒度:同一 agent、同一 chat、同一收件人。
type TargetKey = (String, String, String);

fn key_of(target: &GrantTarget) -> TargetKey {
    (
        target.agent.clone(),
        target.chat_id.clone(),
        target.recipient_open_id.clone(),
    )
}

struct PendingCode {
    mode: TerminalMode,
    target: GrantTarget,
    expires_at_ms: i64,
    generation: u64,
    /// 收件人点卡片按钮的时刻;没点过(或被别人的点击作废)就是 `None`,进不去。
    activated_at_ms: Option<i64>,
}

impl PendingCode {
    fn activated(&self, now_ms: i64) -> bool {
        self.activated_at_ms
            .is_some_and(|at| now_ms - at <= ACTIVATION_WINDOW_MS)
    }
}

#[derive(Default)]
pub struct TerminalGrants {
    codes: HashMap<String, PendingCode>,
    sessions: HashMap<String, BrowserSession>,
    /// 递增的签发代次,只在本进程内有意义。
    issued: u64,
}

impl TerminalGrants {
    /// 出一组新码。**上一组此刻还有效**:卡片还没送出去,这时候作废等于把用户已经开着的页面关掉、
    /// 而新链接一条都没到手,他只会收到一句「卡片没发出去」。送达之后由 [`Self::revoke_superseded`]
    /// 作废,「一次性、不可分享」由那一步保证。随机源失败时什么也不出。
    pub fn issue(
        &mut self,
        target: GrantTarget,
        now_ms: i64,
        mut token: impl FnMut() -> Option<String>,
    ) -> Option<IssuedCodes> {
        let read_only = token()?;
        let operate = token()?;
        self.sweep(now_ms);
        self.issued += 1;
        let generation = self.issued;
        for (code, mode) in [
            (&read_only, TerminalMode::ReadOnly),
            (&operate, TerminalMode::Operate),
        ] {
            self.codes.insert(
                code.clone(),
                PendingCode {
                    mode,
                    target: target.clone(),
                    expires_at_ms: now_ms + CODE_TTL_MS,
                    generation,
                    activated_at_ms: None,
                },
            );
        }
        Some(IssuedCodes {
            read_only,
            operate,
            generation,
        })
    }

    /// 新一组链接确实送到之后:作废同一 (agent, chat, 收件人) 下**比它早**的码与已打开的页面。
    /// 比它晚的那些不碰——那是另一次签发,各自的送达各自作废,先送到的一次不该把后签的作废掉。
    pub fn revoke_superseded(&mut self, target: &GrantTarget, generation: u64) {
        let key = key_of(target);
        self.codes
            .retain(|_, c| c.generation >= generation || key_of(&c.target) != key);
        self.sessions
            .retain(|_, s| s.generation >= generation || key_of(&s.target) != key);
    }

    /// 落地页只需要知道模式,不消耗码:飞书与安全扫描会预取链接。
    pub fn peek(&self, code: &str, now_ms: i64) -> Option<TerminalMode> {
        self.codes
            .get(code)
            .filter(|c| c.expires_at_ms > now_ms)
            .map(|c| c.mode)
    }

    /// 终端卡按钮的点击回调:点击人是收件人且仍有权限(`allowed`)就激活这个码;否则把已有的
    /// 激活作废 —— 链接可能被转发了,宁可让收件人再点一次。返回是否激活。
    pub fn activate(
        &mut self,
        code: &str,
        operator_open_id: &str,
        allowed: bool,
        now_ms: i64,
    ) -> bool {
        let Some(pending) = self
            .codes
            .get_mut(code)
            .filter(|c| c.expires_at_ms > now_ms)
        else {
            return false;
        };
        let ok = allowed && pending.target.recipient_open_id == operator_open_id;
        pending.activated_at_ms = ok.then_some(now_ms);
        ok
    }

    /// 码还有效、只差收件人在飞书里点按钮激活。落地页据此提示,而不是说链接失效。
    pub fn awaiting_activation(&self, code: &str, now_ms: i64) -> bool {
        self.codes
            .get(code)
            .is_some_and(|c| c.expires_at_ms > now_ms && !c.activated(now_ms))
    }

    /// 「进入」:码须已被收件人激活,当场作废,换成一个浏览器会话。随机源失败时码保持原样,用户能再点一次。
    pub fn consume(
        &mut self,
        code: &str,
        now_ms: i64,
        mut token: impl FnMut() -> Option<String>,
    ) -> Option<BrowserSession> {
        let pending = self.codes.get(code)?;
        if pending.expires_at_ms <= now_ms || !pending.activated(now_ms) {
            return None;
        }
        let target = pending.target.clone();
        let mode = pending.mode;
        let generation = pending.generation;

        let handle = token()?;
        let secret = token()?;

        self.codes.remove(code).expect("code was just validated");
        let session = BrowserSession {
            handle,
            secret,
            mode,
            target,
            expires_at_ms: now_ms + SESSION_TTL_MS,
            generation,
        };
        self.sessions
            .insert(session.handle.clone(), session.clone());
        Some(session)
    }

    /// cookie 校验:句柄存在、密钥相等(常量时间比较)、未过期。
    pub fn session(&self, handle: &str, secret: &str, now_ms: i64) -> Option<&BrowserSession> {
        self.sessions.get(handle).filter(|s| {
            s.expires_at_ms > now_ms && constant_time_eq(s.secret.as_bytes(), secret.as_bytes())
        })
    }

    /// 连接存续期间周期性地问:这个页面还该不该连着(没过期、没被新一组作废)。
    pub fn is_live(&self, handle: &str, now_ms: i64) -> bool {
        self.sessions
            .get(handle)
            .is_some_and(|s| s.expires_at_ms > now_ms)
    }

    pub fn sweep(&mut self, now_ms: i64) {
        self.codes.retain(|_, c| c.expires_at_ms > now_ms);
        self.sessions.retain(|_, s| s.expires_at_ms > now_ms);
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> impl FnMut() -> Option<String> {
        let mut n = 0u64;
        move || {
            n += 1;
            Some(format!("{n:032x}"))
        }
    }

    fn target(chat: &str, who: &str) -> GrantTarget {
        GrantTarget {
            agent: "arcadia".into(),
            chat_id: chat.into(),
            chat_label: "开发群".into(),
            tmux_session: "soul-arcadia-gabc".into(),
            recipient_open_id: who.into(),
        }
    }

    /// 签发一组码,并按收件人在签发那一刻点了卡片按钮的样子激活它们。
    fn issue_activated(
        g: &mut TerminalGrants,
        target: GrantTarget,
        now_ms: i64,
        token: &mut impl FnMut() -> Option<String>,
    ) -> IssuedCodes {
        let who = target.recipient_open_id.clone();
        let codes = g.issue(target, now_ms, token).unwrap();
        assert!(g.activate(&codes.read_only, &who, true, now_ms));
        assert!(g.activate(&codes.operate, &who, true, now_ms));
        codes
    }

    #[test]
    fn a_code_needs_the_recipients_click_within_the_window() {
        let mut g = TerminalGrants::default();
        let mut t = tokens();
        let codes = g.issue(target("oc_1", "ou_a"), 0, &mut t).unwrap();
        assert!(g.awaiting_activation(&codes.operate, 1));
        assert!(
            g.consume(&codes.operate, 1, &mut t).is_none(),
            "没点按钮进不去"
        );

        assert!(
            !g.activate(&codes.operate, "ou_other", true, 2),
            "别人的点击不算"
        );
        assert!(
            !g.activate(&codes.operate, "ou_a", false, 3),
            "收件人没了权限也不算"
        );
        assert!(g.awaiting_activation(&codes.operate, 4));

        assert!(g.activate(&codes.operate, "ou_a", true, 10));
        assert!(!g.awaiting_activation(&codes.operate, 11));
        assert!(
            g.consume(&codes.operate, 10 + ACTIVATION_WINDOW_MS + 1, &mut t)
                .is_none(),
            "激活过了窗口就失效"
        );

        assert!(g.activate(&codes.operate, "ou_a", true, 100_000));
        assert!(
            !g.activate(&codes.operate, "ou_other", true, 100_001),
            "窗口内别人点一下"
        );
        assert!(
            g.consume(&codes.operate, 100_002, &mut t).is_none(),
            "别人的点击把已有的激活作废"
        );
        assert!(g.activate(&codes.operate, "ou_a", true, 100_003));
        assert!(g.consume(&codes.operate, 100_004, &mut t).is_some());
        assert!(
            !g.awaiting_activation(&codes.operate, 100_005),
            "进过了就不再等"
        );
    }

    #[test]
    fn a_code_can_be_entered_exactly_once() {
        let mut g = TerminalGrants::default();
        let mut t = tokens();
        let codes = issue_activated(&mut g, target("oc_1", "ou_a"), 0, &mut t);
        let s = g
            .consume(&codes.operate, 1, &mut t)
            .expect("第一次进入换成会话");
        assert_eq!(s.mode, TerminalMode::Operate);
        assert!(
            g.consume(&codes.operate, 2, &mut t).is_none(),
            "码只能用一次"
        );
        let r = g.consume(&codes.read_only, 3, &mut t).unwrap();
        assert_eq!(r.mode, TerminalMode::ReadOnly);
    }

    #[test]
    fn peeking_the_landing_page_does_not_spend_the_code() {
        let mut g = TerminalGrants::default();
        let mut t = tokens();
        let codes = issue_activated(&mut g, target("oc_1", "ou_a"), 0, &mut t);
        assert_eq!(g.peek(&codes.read_only, 1), Some(TerminalMode::ReadOnly));
        assert_eq!(g.peek(&codes.read_only, 2), Some(TerminalMode::ReadOnly));
        assert!(g.consume(&codes.read_only, 3, &mut t).is_some());
        assert_eq!(g.peek(&codes.read_only, 4), None);
    }

    #[test]
    fn an_unused_code_expires_after_ten_minutes() {
        let mut g = TerminalGrants::default();
        let mut t = tokens();
        let codes = issue_activated(&mut g, target("oc_1", "ou_a"), 0, &mut t);
        assert_eq!(g.peek(&codes.operate, CODE_TTL_MS), None);
        assert!(g.consume(&codes.operate, CODE_TTL_MS, &mut t).is_none());
    }

    #[test]
    fn a_browser_session_needs_the_exact_secret_and_expires_after_two_hours() {
        let mut g = TerminalGrants::default();
        let mut t = tokens();
        let codes = issue_activated(&mut g, target("oc_1", "ou_a"), 0, &mut t);
        let s = g.consume(&codes.operate, 0, &mut t).unwrap();
        let (handle, secret) = (s.handle.clone(), s.secret().to_string());
        assert!(g.session(&handle, &secret, 1).is_some());
        assert!(g.session(&handle, "0".repeat(32).as_str(), 1).is_none());
        assert!(g.is_live(&handle, SESSION_TTL_MS - 1));
        assert!(!g.is_live(&handle, SESSION_TTL_MS));
        assert!(g.session(&handle, &secret, SESSION_TTL_MS).is_none());
    }

    #[test]
    fn a_delivered_set_revokes_the_previous_codes_and_open_pages_of_the_same_recipient_only() {
        let mut g = TerminalGrants::default();
        let mut t = tokens();
        let first = issue_activated(&mut g, target("oc_1", "ou_a"), 0, &mut t);
        let opened = g.consume(&first.read_only, 1, &mut t).unwrap();
        let other_chat = issue_activated(&mut g, target("oc_2", "ou_a"), 2, &mut t);
        let other_person = issue_activated(&mut g, target("oc_1", "ou_b"), 3, &mut t);

        let second = issue_activated(&mut g, target("oc_1", "ou_a"), 4, &mut t);
        g.revoke_superseded(&target("oc_1", "ou_a"), second.generation);

        assert!(!g.is_live(&opened.handle, 5), "已打开的页面随上一组作废");
        assert!(
            g.consume(&first.operate, 5, &mut t).is_none(),
            "没用过的码随上一组作废"
        );
        assert!(
            g.consume(&second.operate, 5, &mut t).is_some(),
            "新的这一组不作废自己"
        );
        assert!(
            g.consume(&other_chat.operate, 5, &mut t).is_some(),
            "别的 chat 不受影响"
        );
        assert!(
            g.consume(&other_person.operate, 5, &mut t).is_some(),
            "别的收件人不受影响"
        );
    }

    /// 签发本身不作废上一组:卡片没送到的话,用户手里已经开着的页面还得能用——否则他既丢了旧
    /// 链接,又没拿到新的。作废是「送到了」这一步的事。
    #[test]
    fn issuing_alone_leaves_the_previous_set_working() {
        let mut g = TerminalGrants::default();
        let mut t = tokens();
        let first = issue_activated(&mut g, target("oc_1", "ou_a"), 0, &mut t);
        let opened = g.consume(&first.read_only, 1, &mut t).unwrap();

        let _undelivered = issue_activated(&mut g, target("oc_1", "ou_a"), 2, &mut t);

        assert!(g.is_live(&opened.handle, 3), "已打开的页面照常连着");
        assert!(
            g.consume(&first.operate, 3, &mut t).is_some(),
            "没用过的旧码照常能进"
        );
    }

    /// 两次签发挨在一起时,先送到的那一次不作废后签的那一组:各自的送达各自作废。
    #[test]
    fn a_delivered_set_never_revokes_a_newer_one() {
        let mut g = TerminalGrants::default();
        let mut t = tokens();
        let older = issue_activated(&mut g, target("oc_1", "ou_a"), 0, &mut t);
        let newer = issue_activated(&mut g, target("oc_1", "ou_a"), 1, &mut t);

        g.revoke_superseded(&target("oc_1", "ou_a"), older.generation);
        assert!(g.consume(&newer.operate, 2, &mut t).is_some(), "后签的还在");

        g.revoke_superseded(&target("oc_1", "ou_a"), newer.generation);
        assert!(
            g.consume(&older.read_only, 3, &mut t).is_none(),
            "先签的没了"
        );
    }

    #[test]
    fn a_failed_token_source_issues_nothing() {
        let mut g = TerminalGrants::default();
        assert!(g.issue(target("oc_1", "ou_a"), 0, || None).is_none());
    }

    #[test]
    fn a_failed_token_source_does_not_burn_the_code() {
        let mut g = TerminalGrants::default();
        let mut t = tokens();
        let codes = issue_activated(&mut g, target("oc_1", "ou_a"), 0, &mut t);

        // 随机源失败:进不去,码也还在。
        assert!(g.consume(&codes.operate, 1, || None).is_none());
        assert_eq!(g.peek(&codes.operate, 2), Some(TerminalMode::Operate));

        // 随机源恢复后,同一个码照常能进。
        let s = g.consume(&codes.operate, 3, &mut t).unwrap();
        assert_eq!(s.mode, TerminalMode::Operate);
    }
}
