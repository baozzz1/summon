//! Codex CLI 额度快照的纯解析层。
//!
//! 任意外部 Codex CLI 没有可附着的全局事件总线；兼容层只能 tail
//! `$CODEX_HOME/sessions/**/rollout-*.jsonl`。本模块只理解 JSONL，不碰文件、进程或时钟。
//! 窗口语义按窗长判（短窗 = 5 小时级、长窗 = 周额度），不按 limit_id 或字段位置判。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一个 primary/secondary 时间窗口。rollout 的 `used_percent` 历史上是浮点数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexRateLimitWindow {
    pub used_percent: f64,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<i64>,
}

/// 额度桶的最小快照。`rate_limit_reached_type` 区分时间窗口限额与 credits/workspace 上限，
/// 供卡片决定是否提供定时恢复入口。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexRateLimitSnapshot {
    pub limit_id: String,
    pub limit_name: Option<String>,
    pub primary: Option<CodexRateLimitWindow>,
    pub secondary: Option<CodexRateLimitWindow>,
    pub rate_limit_reached_type: Option<String>,
}

/// 一个窗口够不够长,算得上「周额度」而不是短时限流窗。
///
/// **按窗长判,不按字段位置判** —— 这不是洁癖:2026-08-08 真机探针(`account/rateLimits/read`,
/// prolite 计划)实测 `primary.windowDurationMins = 10080`(一周)、`secondary = null`,
/// 与「primary=5 小时、secondary=周」的直觉恰好相反。字段名只是位置,窗长才是语义。
const WEEKLY_WINDOW_MIN_MINUTES: u64 = 24 * 60;

impl CodexRateLimitSnapshot {
    fn weekly_window(&self) -> Option<&CodexRateLimitWindow> {
        [self.secondary.as_ref(), self.primary.as_ref()]
            .into_iter()
            .flatten()
            .filter(|window| {
                window
                    .window_minutes
                    .is_some_and(|minutes| minutes >= WEEKLY_WINDOW_MIN_MINUTES)
            })
            .max_by_key(|window| window.window_minutes.unwrap_or(0))
    }

    /// 周额度**剩余**百分比(0..=100),给角标显示用。
    ///
    /// 取 primary/secondary 里窗长 ≥ 一天的那个(都满足则取更长的)。一个都不满足 → `None`:
    /// 只有 5 小时窗时不拿它冒充周额度 —— 显示一个语义错的数字比不显示更糟。
    ///
    /// **口径转换在这里做一次**:上游 `used_percent` 是「已用」,角标那一项是「还剩」。
    /// 两个口径长得一样(都是 0-100 的百分比),混用不会报错、只会把 5% 剩余显示成 5% 用量,
    /// 所以转换只留这一个入口。
    pub fn weekly_left_pct(&self) -> Option<u8> {
        self.weekly_left_pct_and_reset_at().map(|(left, _)| left)
    }

    /// 周额度剩余百分比与本周期结束时刻。周期时刻用于合并不同 chat 的并发观测。
    pub fn weekly_left_pct_and_reset_at(&self) -> Option<(u8, Option<i64>)> {
        self.weekly_window().map(|window| {
            (
                (100.0 - window.used_percent).round().clamp(0.0, 100.0) as u8,
                window.resets_at,
            )
        })
    }

    /// 5 小时级短窗：primary/secondary 里窗长 < 一天的那个(都满足取更长的)；只有周额度窗或
    /// 快照缺窗长时不认。
    fn five_hour_window(&self) -> Option<&CodexRateLimitWindow> {
        [self.primary.as_ref(), self.secondary.as_ref()]
            .into_iter()
            .flatten()
            .filter(|w| {
                w.window_minutes
                    .is_some_and(|m| m < WEEKLY_WINDOW_MIN_MINUTES)
            })
            .max_by_key(|w| w.window_minutes.unwrap_or(0))
    }

    /// 5 小时窗口的重置时刻（Unix 秒）。
    pub fn five_hour_resets_at(&self) -> Option<i64> {
        self.five_hour_window().and_then(|w| w.resets_at)
    }

    /// 5 小时窗口**剩余**百分比与重置时刻，口径与 [`Self::weekly_left_pct_and_reset_at`] 一致。
    pub fn five_hour_left_pct_and_reset_at(&self) -> Option<(u8, Option<i64>)> {
        self.five_hour_window().map(|window| {
            (
                (100.0 - window.used_percent).round().clamp(0.0, 100.0) as u8,
                window.resets_at,
            )
        })
    }

    /// credits/workspace cap 等非时间型限额没有可靠的时间恢复点，不提供定时恢复入口。
    pub fn is_temporal_limit(&self) -> bool {
        !self.rate_limit_reached_type.as_deref().is_some_and(|t| {
            matches!(
                t,
                "workspace_owner_credits_depleted"
                    | "workspace_member_credits_depleted"
                    | "workspace_owner_usage_limit_reached"
                    | "workspace_member_usage_limit_reached"
            )
        })
    }
}

/// rollout 一行的解析结果：一行 rate_limits 快照及其观测时刻。
#[derive(Debug, Clone, PartialEq)]
pub struct CodexRolloutRateLimits {
    pub snapshot: CodexRateLimitSnapshot,
    /// 这一行的 `timestamp` 换算成 Unix 秒;账号级额度缓存靠它给回放的历史行排先后。
    pub observed_at: Option<i64>,
}

/// 解析 rollout 一行。只产出 rate_limits 快照；未知/坏行返回 None；
/// 字段新增由 serde_json::Value 自然兼容。
pub fn parse_codex_rollout_line(line: &str) -> Option<CodexRolloutRateLimits> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(Value::as_str)? != "event_msg" {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str)? != "token_count" {
        return None;
    }
    let snapshot = parse_rate_limits(payload.get("rate_limits")?)?;
    let observed_at = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(rfc3339_secs);
    Some(CodexRolloutRateLimits {
        snapshot,
        observed_at,
    })
}

fn rfc3339_secs(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.timestamp())
}

fn parse_rate_limits(value: &Value) -> Option<CodexRateLimitSnapshot> {
    let limit_id = value.get("limit_id").and_then(Value::as_str)?.trim();
    if limit_id.is_empty() {
        return None;
    }
    Some(CodexRateLimitSnapshot {
        limit_id: limit_id.to_string(),
        limit_name: value
            .get("limit_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        primary: value.get("primary").and_then(parse_window),
        secondary: value.get("secondary").and_then(parse_window),
        rate_limit_reached_type: value
            .get("rate_limit_reached_type")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn parse_window(value: &Value) -> Option<CodexRateLimitWindow> {
    if value.is_null() {
        return None;
    }
    Some(CodexRateLimitWindow {
        used_percent: value.get("used_percent")?.as_f64()?,
        window_minutes: value.get("window_minutes").and_then(Value::as_u64),
        resets_at: value.get("resets_at").and_then(Value::as_i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(used: f64, minutes: Option<u64>) -> CodexRateLimitWindow {
        CodexRateLimitWindow {
            used_percent: used,
            window_minutes: minutes,
            resets_at: None,
        }
    }

    fn snap(
        primary: Option<CodexRateLimitWindow>,
        secondary: Option<CodexRateLimitWindow>,
    ) -> CodexRateLimitSnapshot {
        CodexRateLimitSnapshot {
            limit_id: "codex".into(),
            limit_name: None,
            primary,
            secondary,
            rate_limit_reached_type: None,
        }
    }

    /// 周额度取的是长周期窗,且口径从「已用」翻成「还剩」。
    #[test]
    fn 周额度取长窗并翻成剩余口径() {
        let s = snap(
            Some(window(80.0, Some(300))),         // 5 小时窗:不是周额度
            Some(window(13.0, Some(7 * 24 * 60))), // 一周窗
        );
        assert_eq!(s.weekly_left_pct(), Some(87));
    }

    /// **真机形态**(2026-08-08 探针,prolite 计划):周窗在 `primary`、`secondary` 是 null。
    /// 与「primary=短窗、secondary=长窗」的直觉相反 —— 按字段位置判会在这里静默返回 None。
    #[test]
    fn 真机形态_周窗在primary而secondary为空() {
        let mut weekly = window(95.0, Some(10080));
        weekly.resets_at = Some(200);
        let s = snap(Some(weekly), None);
        assert_eq!(s.weekly_left_pct(), Some(5));
        assert_eq!(s.weekly_left_pct_and_reset_at(), Some((5, Some(200))));
    }

    /// 只有 5 小时窗时不拿它冒充周额度 —— 宁可不显示,也不显示一个语义错的数字。
    #[test]
    fn 没有长窗时不返回短窗充数() {
        assert_eq!(
            snap(Some(window(80.0, Some(300))), None).weekly_left_pct(),
            None
        );
        // 窗长缺失同样不认:认不出语义就不猜。
        assert_eq!(snap(Some(window(80.0, None)), None).weekly_left_pct(), None);
        assert_eq!(snap(None, None).weekly_left_pct(), None);
    }

    /// 两个窗都够长时取更长的那个(按窗长判,不按字段位置判)。
    #[test]
    fn 两个长窗时取更长的() {
        let s = snap(
            Some(window(10.0, Some(30 * 24 * 60))), // 月窗:更长
            Some(window(60.0, Some(7 * 24 * 60))),
        );
        assert_eq!(s.weekly_left_pct(), Some(90));
    }

    /// 用满与越界:钳到 0..=100,不产出负数或 >100。
    #[test]
    fn 剩余百分比钳在合法区间() {
        let full = snap(None, Some(window(100.0, Some(7 * 24 * 60))));
        assert_eq!(full.weekly_left_pct(), Some(0));
        let over = snap(None, Some(window(120.0, Some(7 * 24 * 60))));
        assert_eq!(over.weekly_left_pct(), Some(0));
        let unused = snap(None, Some(window(0.0, Some(7 * 24 * 60))));
        assert_eq!(unused.weekly_left_pct(), Some(100));
    }

    fn rate(limit_id: &str, used: f64, reset: i64) -> String {
        format!(
            r#"{{"timestamp":"t","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"limit_id":"{limit_id}","limit_name":null,"primary":{{"used_percent":{used},"window_minutes":300,"resets_at":{reset}}},"secondary":null,"rate_limit_reached_type":null}}}}}}"#
        )
    }

    #[test]
    fn rate_limits_record_carries_the_line_timestamp_as_unix_seconds() {
        let line = r#"{"timestamp":"2026-07-12T00:00:01Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":1.0,"window_minutes":10080,"resets_at":200},"secondary":null,"rate_limit_reached_type":null}}}"#;
        let parsed = parse_codex_rollout_line(line).unwrap();
        assert_eq!(parsed.observed_at, Some(1783814401));
        assert_eq!(parsed.snapshot.limit_id, "codex");

        let undated = line.replace("2026-07-12T00:00:01Z", "t");
        let parsed = parse_codex_rollout_line(&undated).unwrap();
        assert_eq!(parsed.observed_at, None);
    }

    #[test]
    fn non_rate_limit_lines_return_none() {
        let meta = r#"{"timestamp":"t","type":"session_meta","payload":{"id":"thread-1","cwd":"/repo","source":"cli"}}"#;
        assert!(parse_codex_rollout_line(meta).is_none());

        let error = r#"{"timestamp":"t","type":"event_msg","payload":{"type":"error","codex_error_info":"usage_limit_exceeded","message":"localized"}}"#;
        assert!(parse_codex_rollout_line(error).is_none());

        assert!(parse_codex_rollout_line("not json").is_none());
        assert!(parse_codex_rollout_line("").is_none());
    }

    #[test]
    fn premium_bucket_does_not_replace_codex() {
        // 解析层忠实还原每个 limit_id 的快照；窗口语义（5h/周）由窗长决定，与 limit_id 无关。
        let codex = parse_codex_rollout_line(&rate("codex", 98.0, 200)).unwrap();
        assert_eq!(codex.snapshot.limit_id, "codex");
        assert_eq!(
            codex.snapshot.primary.as_ref().unwrap().resets_at,
            Some(200)
        );

        let premium = parse_codex_rollout_line(&rate("premium", 100.0, 999)).unwrap();
        assert_eq!(premium.snapshot.limit_id, "premium");
        assert_eq!(
            premium.snapshot.primary.as_ref().unwrap().resets_at,
            Some(999)
        );
    }

    #[test]
    fn five_hour_resets_at_takes_the_short_window() {
        // primary=5h、secondary=周 → 取 primary 的 reset
        let s = snap(
            Some(CodexRateLimitWindow {
                used_percent: 100.0,
                window_minutes: Some(300),
                resets_at: Some(200),
            }),
            Some(window(13.0, Some(7 * 24 * 60))),
        );
        assert_eq!(s.five_hour_resets_at(), Some(200));

        // 只有周窗 → None
        let s = snap(None, Some(window(13.0, Some(7 * 24 * 60))));
        assert_eq!(s.five_hour_resets_at(), None);

        // 短窗缺 resets_at → None
        let s = snap(Some(window(100.0, Some(300))), None);
        assert_eq!(s.five_hour_resets_at(), None);
    }

    #[test]
    fn five_hour_resets_at_prefers_longer_of_two_short_windows() {
        // 两个窗都 < 一天：取窗长更长的那个的 reset（平手时 secondary 胜）
        let mut primary = window(100.0, Some(180));
        primary.resets_at = Some(100);
        let mut secondary = window(100.0, Some(300));
        secondary.resets_at = Some(200);
        let s = snap(Some(primary), Some(secondary));
        assert_eq!(s.five_hour_resets_at(), Some(200));
    }

    #[test]
    fn five_hour_left_pct_reads_the_short_window_as_remaining() {
        let s = snap(
            Some(window(7.0, Some(7 * 24 * 60))),
            Some(CodexRateLimitWindow {
                used_percent: 12.4,
                window_minutes: Some(300),
                resets_at: Some(200),
            }),
        );
        assert_eq!(s.five_hour_left_pct_and_reset_at(), Some((88, Some(200))));
        assert_eq!(s.weekly_left_pct(), Some(93));

        let weekly_only = snap(Some(window(7.0, Some(7 * 24 * 60))), None);
        assert_eq!(weekly_only.five_hour_left_pct_and_reset_at(), None);
    }

    #[test]
    fn five_hour_resets_at_rejects_exactly_one_day_window() {
        // 窗长恰好一天（1440 分钟）不认成 5h 窗
        let s = snap(Some(window(100.0, Some(24 * 60))), None);
        assert_eq!(s.five_hour_resets_at(), None);
    }

    #[test]
    fn temporal_limit_distinguishes_credits_cap_from_time_window() {
        let temporal = snap(Some(window(100.0, Some(300))), None);
        assert!(temporal.is_temporal_limit());

        for non_temporal in [
            "workspace_owner_credits_depleted",
            "workspace_member_credits_depleted",
            "workspace_owner_usage_limit_reached",
            "workspace_member_usage_limit_reached",
        ] {
            let mut cap = snap(Some(window(100.0, Some(300))), None);
            cap.rate_limit_reached_type = Some(non_temporal.into());
            assert!(!cap.is_temporal_limit(), "{non_temporal} 必须判为非时间型");
        }
    }
}
