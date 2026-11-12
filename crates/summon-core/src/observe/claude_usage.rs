//! claude 会话转录里的模型请求用量:逐个文件按字节游标往后读,每次请求产出一笔 [`UsagePiece`]。
//!
//! claude 的计费事实只在它自己的转录里:主会话 `<sid>.jsonl` 与每个子 agent 的
//! `<sid>/subagents/agent-*.jsonl`(workflow 扇出的在 `<sid>/subagents/workflows/<wf>/` 下)。assistant 行的 `message.usage` 是这一行所属那次请求的用量;同一次
//! 请求按内容块拆成多行、`message.id` 相同,而且是边流式输出边写,后写的那行 `output_tokens` 更大。
//! 同一文件里同一 id 的行总是连着的(实测近两天 2 万多个 id 没有一次被别的 id 隔开),所以每个文件
//! 只需记「上一个 id 与已计的数」:同 id 的后续行只补增量,换了 id 就是一次新请求。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_msg::RequestUsage;
use crate::turn_ledger::{band_of, UsagePiece};

/// 一个转录文件读到哪儿了。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCursor {
    /// 已处理到的字节(总在一行的末尾)。
    pub offset: u64,
    /// 最后一次请求的 id 与已经计过的数:它的后续行只补增量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
    #[serde(default)]
    pub counted: Counted,
}

/// 一次请求已经计过的五个数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counted {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
}

impl Counted {
    fn of(r: &RequestUsage) -> Self {
        Self {
            input: r.input,
            output: r.output,
            cache_read: r.cache_read,
            cache_write_5m: r.cache_write_5m,
            cache_write_1h: r.cache_write_1h,
        }
    }

    fn max(self, other: Self) -> Self {
        Self {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
            cache_read: self.cache_read.max(other.cache_read),
            cache_write_5m: self.cache_write_5m.max(other.cache_write_5m),
            cache_write_1h: self.cache_write_1h.max(other.cache_write_1h),
        }
    }

    fn prompt_tokens(self) -> u64 {
        self.input + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

/// assistant 行的 `message.usage`:这一行所属那次请求的用量,口径统一成 [`RequestUsage`]。
/// `input_tokens` 本就不含缓存;缓存写入按 5 分钟 / 1 小时两档拆,没有拆分时整数记 5 分钟档。
/// `<synthetic>` 是 claude 自己合成的消息,不是模型请求。
pub fn request_usage(message: &Value) -> Option<RequestUsage> {
    let u = message.get("usage")?;
    let id = message.get("id").and_then(Value::as_str)?;
    let model = message.get("model").and_then(Value::as_str);
    if model == Some("<synthetic>") {
        return None;
    }
    let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    let (cache_write_5m, cache_write_1h) = match u.get("cache_creation") {
        Some(b) => (
            n(b.get("ephemeral_5m_input_tokens")),
            n(b.get("ephemeral_1h_input_tokens")),
        ),
        None => (n(u.get("cache_creation_input_tokens")), 0),
    };
    let tier = |key: &str| {
        u.get(key)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && *t != "standard")
    };
    Some(RequestUsage {
        id: id.to_string(),
        model: model.map(str::to_string),
        input: n(u.get("input_tokens")),
        output: n(u.get("output_tokens")),
        cache_read: n(u.get("cache_read_input_tokens")),
        cache_write_5m,
        cache_write_1h,
        variant: tier("speed")
            .or_else(|| tier("service_tier"))
            .map(str::to_string),
    })
}

/// 转录的一行:是 assistant 行且带用量时,给出它的时刻(毫秒)与用量。
pub fn parse_line(line: &str) -> Option<(i64, RequestUsage)> {
    // 绝大多数行是工具结果与附件:先按字面筛,省掉整行 JSON 解析。
    if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let at = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())?
        .timestamp_millis();
    Some((at, request_usage(v.get("message")?)?))
}

/// 从游标处往后读到的一段字节:只处理到最后一个换行(正在写的那一行留到下次),游标原地推进。
/// 产出每笔用量与它的时刻;时刻早于 `floor_ms` 的照样推进游标、记下已计的数,但不产出。
pub fn feed(cursor: &mut FileCursor, bytes: &[u8], floor_ms: i64) -> Vec<(i64, UsagePiece)> {
    let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut out = Vec::new();
    for raw in bytes[..complete].split(|&b| b == b'\n') {
        let Some((at, req)) = std::str::from_utf8(raw).ok().and_then(parse_line) else {
            continue;
        };
        let seen = Counted::of(&req);
        let piece = if cursor.last_id.as_deref() == Some(req.id.as_str()) {
            let before = cursor.counted;
            let now = before.max(seen);
            cursor.counted = now;
            UsagePiece {
                model: req.model,
                band: band_of(now.prompt_tokens()),
                variant: req.variant,
                new_request: false,
                input: now.input - before.input,
                output: now.output - before.output,
                cache_read: now.cache_read - before.cache_read,
                cache_write_5m: now.cache_write_5m - before.cache_write_5m,
                cache_write_1h: now.cache_write_1h - before.cache_write_1h,
            }
        } else {
            cursor.last_id = Some(req.id.clone());
            cursor.counted = seen;
            UsagePiece::from(&req)
        };
        if at >= floor_ms && (piece.new_request || piece.tokens() > 0) {
            out.push((at, piece));
        }
    }
    cursor.offset += complete as u64;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, at: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{at}","message":{{"id":"{id}","model":"claude-test-1","content":[{{"type":"text","text":"好"}}],"usage":{{"input_tokens":2,"cache_read_input_tokens":150000,"output_tokens":{output},"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":300}}}}}}}}"#
        )
    }

    #[test]
    fn assistant_lines_carry_the_request_usage_split_by_cache_lifetime() {
        let raw = r#"{"type":"assistant","timestamp":"2026-10-22T00:00:01Z","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"thinking","thinking":"想想"},{"type":"tool_use","id":"t1","name":"Bash","input":{}}],"usage":{"input_tokens":2,"cache_creation_input_tokens":3689,"cache_read_input_tokens":156847,"output_tokens":1429,"service_tier":"standard","speed":"standard","cache_creation":{"ephemeral_1h_input_tokens":3600,"ephemeral_5m_input_tokens":89}}}}"#;
        let (at, usage) = parse_line(raw).unwrap();
        assert_eq!(at, 1_792_627_201_000);
        assert_eq!(usage.id, "msg_1");
        assert_eq!(usage.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(
            (usage.input, usage.cache_read, usage.output),
            (2, 156_847, 1_429)
        );
        assert_eq!((usage.cache_write_5m, usage.cache_write_1h), (89, 3_600));
        assert_eq!(usage.variant, None);
        assert_eq!(usage.prompt_tokens(), 2 + 156_847 + 3_689);
    }

    #[test]
    fn request_usage_without_a_lifetime_split_counts_as_five_minute_writes() {
        let raw = r#"{"type":"assistant","timestamp":"2026-10-22T00:00:01Z","message":{"id":"msg_2","model":"claude-sonnet-5","content":[{"type":"text","text":"好"}],"usage":{"input_tokens":10,"cache_creation_input_tokens":500,"output_tokens":3,"service_tier":"priority"}}}"#;
        let (_, usage) = parse_line(raw).unwrap();
        assert_eq!((usage.cache_write_5m, usage.cache_write_1h), (500, 0));
        assert_eq!(usage.variant.as_deref(), Some("priority"));
    }

    #[test]
    fn synthetic_messages_tool_results_and_lines_without_usage_are_not_requests() {
        let synthetic = r#"{"type":"assistant","timestamp":"2026-10-22T00:00:01Z","message":{"id":"msg_3","model":"<synthetic>","content":[],"usage":{"input_tokens":0,"output_tokens":0}}}"#;
        assert_eq!(parse_line(synthetic), None);
        let bare = r#"{"type":"assistant","timestamp":"2026-10-22T00:00:01Z","message":{"id":"m","content":[]}}"#;
        assert_eq!(parse_line(bare), None);
        let user = r#"{"type":"user","timestamp":"2026-10-22T00:00:01Z","message":{"content":"\"usage\" \"assistant\""}}"#;
        assert_eq!(parse_line(user), None);
        let no_time =
            r#"{"type":"assistant","message":{"id":"m","model":"x","usage":{"input_tokens":1}}}"#;
        assert_eq!(parse_line(no_time), None);
    }

    #[test]
    fn later_lines_of_the_same_request_only_add_what_grew() {
        let mut cursor = FileCursor::default();
        let text = [
            line("a", "2026-10-22T00:00:01Z", 10),
            line("a", "2026-10-22T00:00:02Z", 40),
            line("a", "2026-10-22T00:00:02Z", 40),
            line("b", "2026-10-22T00:00:05Z", 7),
        ]
        .join("\n")
            + "\n";
        let pieces = feed(&mut cursor, text.as_bytes(), 0);
        assert_eq!(cursor.offset, text.len() as u64);
        let summary: Vec<(bool, u64, u64, u32)> = pieces
            .iter()
            .map(|(_, p)| (p.new_request, p.output, p.cache_read, p.band))
            .collect();
        assert_eq!(
            summary,
            [
                (true, 10, 150_000, 100),
                (false, 30, 0, 100),
                (true, 7, 150_000, 100)
            ]
        );
        let total: u64 = pieces.iter().map(|(_, p)| p.tokens()).sum();
        assert_eq!(total, (2 + 150_000 + 300) * 2 + 40 + 7);
    }

    #[test]
    fn a_request_split_across_two_reads_is_counted_once() {
        let first = line("a", "2026-10-22T00:00:01Z", 10) + "\n";
        let rest =
            line("a", "2026-10-22T00:00:02Z", 25) + "\n" + &line("b", "2026-10-22T00:00:03Z", 1);
        let mut cursor = FileCursor::default();
        let one = feed(&mut cursor, first.as_bytes(), 0);
        // 游标存盘再读回(重启)也一样
        let mut cursor: FileCursor =
            serde_json::from_str(&serde_json::to_string(&cursor).unwrap()).unwrap();
        let two = feed(&mut cursor, rest.as_bytes(), 0);
        // 末尾没换行的那一行还没写完,留到下次
        assert_eq!(two.len(), 1);
        assert_eq!(
            cursor.offset,
            (first.len() + rest.find('\n').unwrap() + 1) as u64
        );
        let outputs: Vec<u64> = one.iter().chain(&two).map(|(_, p)| p.output).collect();
        assert_eq!(outputs, [10, 15]);
        assert!(!two[0].1.new_request);
    }

    #[test]
    fn requests_before_the_floor_move_the_cursor_without_being_counted() {
        let text = [
            line("a", "2026-10-22T00:00:01Z", 10),
            line("a", "2026-10-22T00:10:00Z", 30),
            line("b", "2026-10-22T00:10:01Z", 5),
        ]
        .join("\n")
            + "\n";
        let floor = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:05:00Z")
            .unwrap()
            .timestamp_millis();
        let mut cursor = FileCursor::default();
        let pieces = feed(&mut cursor, text.as_bytes(), floor);
        // a 在界之前开始:它界之后的增量照计,不再算一次请求
        let summary: Vec<(bool, u64)> = pieces
            .iter()
            .map(|(_, p)| (p.new_request, p.output))
            .collect();
        assert_eq!(summary, [(false, 20), (true, 5)]);
    }
}
