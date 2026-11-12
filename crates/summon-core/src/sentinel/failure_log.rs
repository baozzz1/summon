//! sentinel tick 失败日志的去重。一轮 5 秒,同一个失败原样照记,一小时就是七百多行一模一样的
//! 报错,把日志里别的东西全淹掉;而只记第一次又会让人以为它早就好了。所以:首次与错误变化时记,
//! 同一错误每隔一段时间提醒一次(带连续轮数),恢复时记一行。

use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct FailureLog {
    current: Option<String>,
    rounds: u32,
    logged_at: Option<Instant>,
}

impl FailureLog {
    /// 同一错误重复提醒的间隔。
    pub const REMIND_EVERY: Duration = Duration::from_secs(600);

    /// 本轮失败:该写日志就返回那一行。
    pub fn failed(&mut self, error: &str, now: Instant) -> Option<String> {
        if self.current.as_deref() != Some(error) {
            self.current = Some(error.to_string());
            self.rounds = 1;
            self.logged_at = Some(now);
            return Some(summon_i18n::format!("r2981cb8cd2"));
        }
        self.rounds += 1;
        let due = self
            .logged_at
            .is_none_or(|at| now.saturating_duration_since(at) >= Self::REMIND_EVERY);
        if !due {
            return None;
        }
        self.logged_at = Some(now);
        Some(summon_i18n::format!("r61f9e4f5ce", self.rounds))
    }

    /// 本轮成功:此前在失败就返回一行恢复日志。
    pub fn succeeded(&mut self) -> Option<String> {
        self.current.take()?;
        let rounds = std::mem::take(&mut self.rounds);
        self.logged_at = None;
        Some(summon_i18n::format!("rd1a85e6515"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_failure_is_logged_once_then_reminded_periodically() {
        let mut log = FailureLog::default();
        let t0 = Instant::now();
        assert!(log.failed("无 user openId", t0).is_some());
        for i in 1..=10 {
            assert_eq!(
                log.failed("无 user openId", t0 + Duration::from_secs(5 * i)),
                None,
                "同一错误不再逐轮刷"
            );
        }
        let reminder = log
            .failed("无 user openId", t0 + FailureLog::REMIND_EVERY)
            .expect("满间隔要再提一次");
        assert!(reminder.contains("连续 12 轮"), "{reminder}");
    }

    #[test]
    fn a_different_error_is_logged_immediately() {
        let mut log = FailureLog::default();
        let t0 = Instant::now();
        log.failed("a", t0);
        assert!(log.failed("b", t0 + Duration::from_secs(5)).is_some());
    }

    #[test]
    fn recovery_is_logged_once_with_the_streak_length() {
        let mut log = FailureLog::default();
        assert_eq!(log.succeeded(), None, "一直正常就什么都不记");
        let t0 = Instant::now();
        log.failed("x", t0);
        log.failed("x", t0 + Duration::from_secs(5));
        let line = log.succeeded().expect("恢复要记一行");
        assert!(line.contains("连续失败 2 轮"), "{line}");
        assert_eq!(log.succeeded(), None);
        assert!(
            log.failed("x", t0 + Duration::from_secs(10)).is_some(),
            "恢复后再失败算新一段"
        );
    }
}
