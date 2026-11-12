//! Claude Code 五小时会话额度终态的纯解析。

use chrono::{DateTime, Duration, NaiveTime, Utc};
use chrono_tz::Tz;
use serde_json::Value;

const PREFIX: &str = "You've hit your session limit · resets ";
const MAX_TEXT_CHARS: usize = 160;
const MAX_RESET_AHEAD: Duration = Duration::hours(6);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSessionLimit {
    pub reset_at: DateTime<Utc>,
    pub timezone: String,
    pub reset_time: String,
}

impl ClaudeSessionLimit {
    pub fn reminder_at(&self) -> DateTime<Utc> {
        self.reset_at + Duration::minutes(2)
    }

    pub fn is_actionable_at(&self, now: DateTime<Utc>) -> bool {
        self.reminder_at() > now && self.reset_at - now <= MAX_RESET_AHEAD
    }
}

/// 从完整 transcript JSONL 事件解析额度终态。只接受结构化
/// `quotaLimits.rateLimitType=five_hour` 与绝对 `resetsAt`；展示文本仅用于复核时区和钟点，
/// 不能单独证明绝对恢复时刻。
pub fn parse_claude_session_limit_event(line: &str) -> Option<ClaudeSessionLimit> {
    let event: Value = serde_json::from_str(line).ok()?;
    if event.get("type").and_then(Value::as_str) != Some("assistant")
        || event.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let message = event.get("message")?;
    if message.get("stop_reason").and_then(Value::as_str) != Some("stop_sequence") {
        return None;
    }
    let text = message
        .get("content")?
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let observed_at = DateTime::parse_from_rfc3339(event.get("timestamp")?.as_str()?)
        .ok()?
        .with_timezone(&Utc);
    let quota = event.get("quotaLimits")?;
    if quota.get("rateLimitType").and_then(Value::as_str) != Some("five_hour") {
        return None;
    }
    let (reset_time, timezone, zone, clock) = parse_fields(&text)?;
    let reset_at = DateTime::from_timestamp(quota.get("resetsAt")?.as_i64()?, 0)?;
    if reset_at.with_timezone(&zone).time() != clock
        || reset_at <= observed_at
        || reset_at - observed_at > MAX_RESET_AHEAD
    {
        return None;
    }
    Some(ClaudeSessionLimit {
        reset_at,
        timezone: timezone.to_string(),
        reset_time: reset_time.to_string(),
    })
}

fn parse_fields(text: &str) -> Option<(&str, &str, Tz, NaiveTime)> {
    let text = text.trim();
    if text.chars().count() > MAX_TEXT_CHARS {
        return None;
    }
    let body = text.strip_prefix(PREFIX)?;
    let open = body.rfind(" (")?;
    let reset_time = body[..open].trim();
    let timezone = body[open + 2..].strip_suffix(')')?.trim();
    if reset_time.is_empty() || timezone.is_empty() {
        return None;
    }
    let zone: Tz = timezone.parse().ok()?;
    let clock = parse_clock(reset_time)?;
    Some((reset_time, timezone, zone, clock))
}

fn parse_clock(raw: &str) -> Option<NaiveTime> {
    let normalized = raw.to_ascii_uppercase();
    let (clock, is_pm) = normalized
        .strip_suffix("AM")
        .map(|clock| (clock, false))
        .or_else(|| normalized.strip_suffix("PM").map(|clock| (clock, true)))?;
    let (hour, minute) = match clock.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
        None => (clock.parse::<u32>().ok()?, 0),
    };
    if !(1..=12).contains(&hour) {
        return None;
    }
    let hour = match (hour, is_pm) {
        (12, false) => 0,
        (12, true) => 12,
        (hour, true) => hour + 12,
        (hour, false) => hour,
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn structured_event_uses_absolute_reset_timestamp() {
        let line = r#"{"type":"assistant","timestamp":"2026-09-02T09:20:55.766Z","isSidechain":false,"message":{"stop_reason":"stop_sequence","content":[{"type":"text","text":"You've hit your session limit · resets 6:10pm (Asia/Singapore)"}]},"quotaLimits":{"status":"rejected","resetsAt":1788343800,"unifiedRateLimitFallbackAvailable":false,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false}}"#;
        let limit = parse_claude_session_limit_event(line).expect("结构化五小时额度事件必须命中");
        assert_eq!(limit.reset_at, at("2026-09-02T10:10:00Z"));
        assert_eq!(limit.reminder_at(), at("2026-09-02T10:12:00Z"));
        assert!(limit.is_actionable_at(limit.reset_at + Duration::minutes(1)));
        assert!(!limit.is_actionable_at(limit.reminder_at()));
    }

    #[test]
    fn structured_event_rejects_other_windows_and_mismatched_text() {
        let weekly = r#"{"type":"assistant","timestamp":"2026-09-02T09:20:55Z","message":{"stop_reason":"stop_sequence","content":[{"type":"text","text":"You've hit your session limit · resets 6:10pm (Asia/Singapore)"}]},"quotaLimits":{"rateLimitType":"seven_day","resetsAt":1788343800}}"#;
        assert!(parse_claude_session_limit_event(weekly).is_none());

        let mismatch = r#"{"type":"assistant","timestamp":"2026-09-02T09:20:55Z","message":{"stop_reason":"stop_sequence","content":[{"type":"text","text":"You've hit your session limit · resets 7:10pm (Asia/Singapore)"}]},"quotaLimits":{"rateLimitType":"five_hour","resetsAt":1788343800}}"#;
        assert!(parse_claude_session_limit_event(mismatch).is_none());
    }

    #[test]
    fn event_requires_the_exact_terminal_and_structured_evidence() {
        let base = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-09-02T09:20:55Z",
            "isSidechain": false,
            "message": {
                "stop_reason": "stop_sequence",
                "content": [{
                    "type": "text",
                    "text": "You've hit your session limit · resets 6:10pm (Asia/Singapore)"
                }]
            },
            "quotaLimits": {"rateLimitType": "five_hour", "resetsAt": 1788343800_i64}
        });

        for mutate in [
            |value: &mut Value| value["message"]["stop_reason"] = Value::String("end_turn".into()),
            |value: &mut Value| value["isSidechain"] = Value::Bool(true),
            |value: &mut Value| value["type"] = Value::String("user".into()),
            |value: &mut Value| {
                value.as_object_mut().unwrap().remove("quotaLimits");
            },
        ] {
            let mut event = base.clone();
            mutate(&mut event);
            assert!(parse_claude_session_limit_event(&event.to_string()).is_none());
        }
    }
}
