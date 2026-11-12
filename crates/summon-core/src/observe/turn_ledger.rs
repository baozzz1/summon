//! 逐轮台账:一轮一行,写在 `~/.summon/run/turns/<agent>/<YYYY-MM>.ndjson`,按月一个文件、只追加。
//!
//! 控制台的会话详情、用量统计、定时任务的执行记录都从这里读。只记这一轮的结构化事实(谁触发、
//! 什么结果、多久、用了多少),不记对话正文:触发消息只留开头几十个字,内部构造的 prompt 不留。
//! 保留天数之外的整月文件在 daemon 启动时删掉。
//!
//! 不在任何一轮里发生的模型请求(两轮之间还在跑的后台子 agent、不经 summon 唤起的轮)记作
//! [`OutsideUsage`],写在 `~/.summon/run/usage/<agent>/<YYYY-MM>.ndjson`,同样按月、只追加。

use chrono::{Datelike, Local, TimeZone};
use serde::{Deserialize, Serialize};

/// 台账保留多久(owner 2026-10-21 定)。
pub const KEEP_DAYS: i64 = 90;
/// 触发消息留多少个字。
const PROMPT_HEAD_CHARS: usize = 60;

/// 这一轮是谁触发的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOrigin {
    /// 真人发的消息。
    Human,
    /// 另一个 bot 发的消息。
    Bot,
    /// 定时任务。
    Schedule,
    /// sentinel 召唤(拉进群、建群)。
    Sentinel,
    /// 斜杠命令透传。
    Slash,
    /// summon 自己构造的 prompt(交接、新会话这类)。
    Internal,
    /// daemon 换代后接着跑完的上一代的轮。
    Recovery,
}

/// 这一轮怎么收尾的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnd {
    /// 正常完成并发出了回复。
    Done,
    /// 正常完成但没有发出内容。
    Empty,
    /// 正常完成,回复要求静默重置会话。
    SilentReset,
    /// 出错(捕获失败、provider 报错、静默超时)。
    Failed,
    /// 被用户打断。
    Interrupted,
}

/// 长上下文分档的门槛(千 token,从大到小)。公开价格表里的加价门槛就是这几个(Anthropic 100k /
/// 200k、OpenAI 272k),而且按单次请求的输入总量判,所以分档要在采集时按请求做,汇总之后就分不出了。
pub const PROMPT_BANDS_K: [u32; 3] = [272, 200, 100];

/// 单次请求的输入总量落在哪一档:超过的最大门槛(千 token),都没超过是 0。
pub fn band_of(prompt_tokens: u64) -> u32 {
    PROMPT_BANDS_K
        .into_iter()
        .find(|k| prompt_tokens > u64::from(*k) * 1_000)
        .unwrap_or(0)
}

/// 一轮里同一模型、同一输入档位、同一服务档的请求合在一起的 token。口径同
/// [`RequestUsage`](crate::agent_msg::RequestUsage):`input` 是未命中缓存的输入。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// 模型 id;CLI 没报时为空。
    pub model: String,
    /// 单次请求的输入总量超过的分档门槛(千 token),见 [`band_of`]。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub band: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub requests: u32,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write_5m: u64,
    #[serde(default)]
    pub cache_write_1h: u64,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl ModelUsage {
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

/// 一笔用量:一次模型请求,或同一次请求后来又补上的那一截(`new_request == false`,不再计请求数)。
/// 档位按这次请求的输入总量定,补上的那一截沿用请求本身的档位。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsagePiece {
    pub model: Option<String>,
    pub band: u32,
    pub variant: Option<String>,
    pub new_request: bool,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
}

impl UsagePiece {
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

impl From<&crate::agent_msg::RequestUsage> for UsagePiece {
    fn from(r: &crate::agent_msg::RequestUsage) -> Self {
        Self {
            model: r.model.clone(),
            band: band_of(r.prompt_tokens()),
            variant: r.variant.clone(),
            new_request: true,
            input: r.input,
            output: r.output,
            cache_read: r.cache_read,
            cache_write_5m: r.cache_write_5m,
            cache_write_1h: r.cache_write_1h,
        }
    }
}

/// 把用量按 (模型, 档位, 服务档) 合并。没报模型的记在 `fallback_model` 名下。
pub fn merge_pieces<'a>(
    pieces: impl IntoIterator<Item = &'a UsagePiece>,
    fallback_model: Option<&str>,
) -> Vec<ModelUsage> {
    let mut merged: std::collections::BTreeMap<(String, u32, Option<String>), ModelUsage> =
        Default::default();
    for p in pieces {
        let model = p
            .model
            .as_deref()
            .or(fallback_model)
            .unwrap_or_default()
            .to_string();
        let slot = merged
            .entry((model.clone(), p.band, p.variant.clone()))
            .or_insert_with(|| ModelUsage {
                model,
                band: p.band,
                variant: p.variant.clone(),
                ..Default::default()
            });
        slot.requests += u32::from(p.new_request);
        slot.input += p.input;
        slot.output += p.output;
        slot.cache_read += p.cache_read;
        slot.cache_write_5m += p.cache_write_5m;
        slot.cache_write_1h += p.cache_write_1h;
    }
    merged.into_values().filter(|m| m.tokens() > 0).collect()
}

/// 把一轮的请求按 (模型, 档位, 服务档) 合并。
pub fn merge_requests<'a>(
    requests: impl IntoIterator<Item = &'a crate::agent_msg::RequestUsage>,
    fallback_model: Option<&str>,
) -> Vec<ModelUsage> {
    let pieces: Vec<UsagePiece> = requests.into_iter().map(UsagePiece::from).collect();
    merge_pieces(&pieces, fallback_model)
}

/// 一段轮外用量:同一个会话在 `[from_ms, to_ms]` 之间、不在任何一轮里的模型请求。一行不跨本机的
/// 自然日,按天统计时整行落在 `from_ms` 那一天。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutsideUsage {
    pub agent: String,
    pub chat_id: String,
    pub tag: String,
    pub cli: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub usage: Vec<ModelUsage>,
}

/// 一段轮外用量文本里认得出的行;写坏的行跳过。
pub fn parse_outside_lines(text: &str) -> Vec<OutsideUsage> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// 按本机自然日把一批带时刻的用量切成若干段:每段是同一天里的那些,`(from, to, 合并后的用量)`。
pub fn split_by_day<Tz: TimeZone>(
    tz: &Tz,
    pieces: &[(i64, UsagePiece)],
) -> Vec<(i64, i64, Vec<ModelUsage>)> {
    let day = |ms: i64| tz.timestamp_millis_opt(ms).single().map(|t| t.date_naive());
    let mut days: std::collections::BTreeMap<Option<chrono::NaiveDate>, Vec<&(i64, UsagePiece)>> =
        Default::default();
    for p in pieces {
        days.entry(day(p.0)).or_default().push(p);
    }
    days.into_values()
        .filter_map(|group| {
            let from = group.iter().map(|p| p.0).min()?;
            let to = group.iter().map(|p| p.0).max()?;
            let usage = merge_pieces(group.iter().map(|p| &p.1), None);
            (!usage.is_empty()).then_some((from, to, usage))
        })
        .collect()
}

/// 轮末看到的上下文用量。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCtx {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct: Option<u8>,
}

/// 轮末看到的额度(剩余百分比与重置时刻,Unix 秒)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnQuota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour_left: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour_resets_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_left: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_resets_at: Option<i64>,
}

/// 台账里的一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub agent: String,
    pub chat_id: String,
    pub tag: String,
    /// 本轮凭证(与 events.jsonl 里的 `turn` 同一个值);斜杠命令、接管轮没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    pub origin: TurnOrigin,
    /// 定时任务触发时是哪条任务(`SCH-…`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// 发消息的人(open_id)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    pub message_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_head: String,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub end: TurnEnd,
    /// 失败的类别(provider 错误类别、`timed_out`、`capture`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 这一轮是 daemon 换代后接管的:`started_at_ms` 是这一代接手的时刻。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reattached: bool,
    #[serde(default)]
    pub tool_steps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<TurnCtx>,
    /// 这一轮的模型请求(含子 agent 的)按模型与档位合并的 token;CLI 没报就没有。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<ModelUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<TurnQuota>,
    /// 回复末尾的 feed 总结。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
}

/// 本机时区下这个时刻落在哪个月的文件里(`2026-10.ndjson`)。
pub fn file_name(at_ms: i64) -> String {
    let at = Local
        .timestamp_millis_opt(at_ms)
        .single()
        .unwrap_or_else(Local::now);
    format!("{:04}-{:02}.ndjson", at.year(), at.month())
}

/// 文件名对应的那个月整月都在 `keep_days` 天之前:可以删。认不出的文件名不删。
pub fn expired(file: &str, now_ms: i64, keep_days: i64) -> bool {
    let Some((year, month)) = file
        .strip_suffix(".ndjson")
        .and_then(|stem| stem.split_once('-'))
        .and_then(|(y, m)| Some((y.parse::<i32>().ok()?, m.parse::<u32>().ok()?)))
    else {
        return false;
    };
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let Some(month_end) = Local
        .with_ymd_and_hms(next_year, next_month, 1, 0, 0, 0)
        .earliest()
    else {
        return false;
    };
    month_end.timestamp_millis() <= now_ms - keep_days * 24 * 3600 * 1000
}

/// 触发消息的开头:第一段非空文字,空白折成一个空格,密钥形态的片段换成 `[REDACTED]`,至多几十个字。
pub fn prompt_head(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    // 先脱敏再截断:省略号之前留下的都是脱敏过的字。
    let collapsed = crate::messaging::segment::redact_secrets(
        &line.split_whitespace().collect::<Vec<_>>().join(" "),
    );
    let mut head: String = collapsed.chars().take(PROMPT_HEAD_CHARS).collect();
    if collapsed.chars().count() > PROMPT_HEAD_CHARS {
        head.push('…');
    }
    head
}

/// 一段台账文本里认得出的行;写坏的行跳过。
pub fn parse_lines(text: &str) -> Vec<TurnRecord> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> TurnRecord {
        TurnRecord {
            agent: "starlight".into(),
            chat_id: "oc_00000000000000000000000000000001".into(),
            tag: "g0000000000000001".into(),
            turn: Some("turn-1".into()),
            origin: TurnOrigin::Human,
            schedule: None,
            requester: Some("ou_00000000000000000000000000000001".into()),
            message_id: "om_1".into(),
            prompt_head: "帮我看下 CI".into(),
            started_at_ms: 1_000,
            ended_at_ms: 61_000,
            end: TurnEnd::Done,
            error: None,
            reattached: false,
            tool_steps: 3,
            cli: Some("claude".into()),
            model: None,
            effort: Some("high".into()),
            ctx: Some(TurnCtx {
                used: None,
                window: Some(200_000),
                pct: Some(43),
            }),
            usage: Vec::new(),
            quota: None,
            feed: Some("修好了".into()),
        }
    }

    #[test]
    fn records_round_trip_and_skip_absent_fields() {
        let line = serde_json::to_string(&record()).unwrap();
        assert!(
            !line.contains("schedule") && !line.contains("usage") && !line.contains("reattached")
        );
        assert!(line.contains(r#""origin":"human""#) && line.contains(r#""end":"done""#));
        assert_eq!(
            parse_lines(&format!("{line}\nnot json\n\n{line}\n")),
            [record(), record()]
        );
    }

    #[test]
    fn rows_written_with_the_retired_tokens_field_still_read() {
        let line = serde_json::to_string(&record()).unwrap().replacen(
            "{",
            r#"{"tokens":{"input":1,"output":2,"cache_read":3,"cache_write":4},"#,
            1,
        );
        assert_eq!(parse_lines(&line), [record()]);
    }

    #[test]
    fn bands_are_the_largest_threshold_the_prompt_exceeds() {
        assert_eq!(band_of(0), 0);
        assert_eq!(band_of(100_000), 0);
        assert_eq!(band_of(100_001), 100);
        assert_eq!(band_of(200_001), 200);
        assert_eq!(band_of(272_000), 200);
        assert_eq!(band_of(272_001), 272);
        assert_eq!(band_of(900_000), 272);
    }

    #[test]
    fn requests_merge_by_model_band_and_service_tier() {
        use crate::agent_msg::RequestUsage;
        let req = |id: &str, model: Option<&str>, input: u64, cache_read: u64| RequestUsage {
            id: id.into(),
            model: model.map(str::to_string),
            input,
            output: 10,
            cache_read,
            cache_write_5m: 1,
            cache_write_1h: 2,
            variant: None,
        };
        let mut priority = req("d", Some("gpt-x"), 5, 0);
        priority.variant = Some("priority".into());
        let requests = [
            req("a", Some("claude-a"), 5, 50_000),
            req("b", Some("claude-a"), 5, 150_000),
            req("c", Some("claude-a"), 7, 60_000),
            req("e", None, 1, 0),
            priority,
        ];
        let merged = merge_requests(&requests, Some("gpt-x"));
        let keys: Vec<(&str, u32, Option<&str>, u32)> = merged
            .iter()
            .map(|m| (m.model.as_str(), m.band, m.variant.as_deref(), m.requests))
            .collect();
        assert_eq!(
            keys,
            [
                ("claude-a", 0, None, 2),
                ("claude-a", 100, None, 1),
                ("gpt-x", 0, None, 1),
                ("gpt-x", 0, Some("priority"), 1),
            ]
        );
        assert_eq!(merged[0].input, 12);
        assert_eq!(merged[0].cache_read, 110_000);
        assert_eq!(merged[0].cache_write_1h, 4);
        assert_eq!(merged[0].tokens(), 12 + 20 + 110_000 + 2 + 4);
        assert!(merge_requests(&[], None).is_empty());
    }

    fn piece(model: &str, band: u32, new_request: bool, output: u64) -> UsagePiece {
        UsagePiece {
            model: Some(model.into()),
            band,
            new_request,
            input: u64::from(new_request),
            output,
            ..Default::default()
        }
    }

    #[test]
    fn later_parts_of_a_request_add_tokens_without_counting_another_request() {
        let pieces = [
            piece("claude-a", 100, true, 10),
            piece("claude-a", 100, false, 25),
            piece("claude-a", 0, true, 3),
        ];
        let merged = merge_pieces(&pieces, None);
        assert_eq!(merged.len(), 2);
        assert_eq!((merged[0].band, merged[0].requests), (0, 1));
        assert_eq!((merged[1].band, merged[1].requests), (100, 1));
        assert_eq!(merged[1].output, 35);
        assert_eq!(merged[1].input, 1);
        // 一个 token 都没有的组不留
        assert!(merge_pieces(&[UsagePiece::default()], Some("m")).is_empty());
    }

    #[test]
    fn outside_usage_is_split_at_local_midnight() {
        let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let at = |d, h, m| {
            tz.with_ymd_and_hms(2026, 10, d, h, m, 0)
                .unwrap()
                .timestamp_millis()
        };
        let pieces = vec![
            (at(21, 23, 50), piece("claude-a", 0, true, 1)),
            (at(21, 23, 59), piece("claude-a", 0, false, 2)),
            (at(22, 0, 5), piece("claude-a", 0, true, 4)),
        ];
        let rows = split_by_day(&tz, &pieces);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].0, rows[0].1), (at(21, 23, 50), at(21, 23, 59)));
        assert_eq!((rows[0].2[0].requests, rows[0].2[0].output), (1, 3));
        assert_eq!((rows[1].0, rows[1].1), (at(22, 0, 5), at(22, 0, 5)));
        assert!(split_by_day(&tz, &[]).is_empty());
        let row = OutsideUsage {
            agent: "starlight".into(),
            chat_id: "oc_00000000000000000000000000000001".into(),
            tag: "g0000000000000001".into(),
            cli: "claude".into(),
            from_ms: rows[1].0,
            to_ms: rows[1].1,
            usage: rows[1].2.clone(),
        };
        let line = serde_json::to_string(&row).unwrap();
        assert_eq!(parse_outside_lines(&format!("{line}\nbroken\n")), [row]);
    }

    #[test]
    fn months_expire_only_when_the_whole_month_is_older_than_the_window() {
        let at = |y, m, d| {
            Local
                .with_ymd_and_hms(y, m, d, 12, 0, 0)
                .unwrap()
                .timestamp_millis()
        };
        let now = at(2026, 10, 21);
        assert_eq!(file_name(now), "2026-10.ndjson");
        // 界是 7/23:6 月整月都在界之前,删
        assert!(expired("2026-06.ndjson", now, KEEP_DAYS));
        // 7 月还有界之后的日子,留着
        assert!(!expired("2026-07.ndjson", now, KEEP_DAYS));
        assert!(!expired("2026-10.ndjson", now, KEEP_DAYS));
        assert!(expired("2025-12.ndjson", now, KEEP_DAYS));
        for odd in ["notes.txt", "2026-13x.ndjson", "x-y.ndjson", "2026.ndjson"] {
            assert!(!expired(odd, now, KEEP_DAYS), "{odd}");
        }
    }

    #[test]
    fn prompt_head_keeps_the_first_line_and_caps_its_length() {
        assert_eq!(
            prompt_head("\n\n  帮我   看下\tCI \n第二行"),
            "帮我 看下 CI"
        );
        let long = "字".repeat(80);
        let head = prompt_head(&long);
        assert_eq!(head.chars().count(), PROMPT_HEAD_CHARS + 1);
        assert!(head.ends_with('…'));
        assert_eq!(prompt_head(""), "");
    }

    #[test]
    fn prompt_head_masks_secret_shaped_fragments() {
        let head = prompt_head("用 API_KEY=sk-0123456789abcdef 跑一下");
        assert!(head.contains("API_KEY=[REDACTED]"), "{head}");
        assert!(!head.contains("sk-0123"), "{head}");
    }
}
