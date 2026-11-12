//! CLI 原生记录的诊断投影:codex rollout 与 claude 转录各一个字段白名单。保留类型、时刻、模型、
//! effort、CLI 版本、token 用量、收尾原因;消息、推理、工具的输入和输出只留长度。cursor 的会话存在
//! sqlite 里,没有投影器,诊断包不带。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use serde_json::{Map, Value};

use super::numeric_skeleton;

/// 原生记录一行的时刻(毫秒)。codex 与 claude 都写 RFC 3339 的 `timestamp`。
pub fn line_time_ms(line: &Value) -> Option<i64> {
    let ts = line.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|at| at.timestamp_millis())
}

/// 逐行投影;记着见过的推理 / 消息正文指纹,用来标重复。
#[derive(Debug, Default)]
pub struct NativeProjector {
    seen: HashMap<(String, u64), usize>,
    lines: usize,
}

impl NativeProjector {
    /// 投影 codex rollout 的一行。
    pub fn codex(&mut self, line: &Value) -> Value {
        self.lines += 1;
        let mut out = Map::new();
        copy_str(&mut out, line, "timestamp");
        copy_str(&mut out, line, "type");
        let payload = line.get("payload").unwrap_or(&Value::Null);
        match line.get("type").and_then(Value::as_str).unwrap_or_default() {
            "session_meta" => {
                for key in [
                    "id",
                    "cli_version",
                    "originator",
                    "model_provider",
                    "source",
                    "history_mode",
                ] {
                    copy_str(&mut out, payload, key);
                }
                copy_num(&mut out, payload, "context_window");
            }
            "turn_context" => {
                for key in ["turn_id", "model", "effort", "approval_policy", "summary"] {
                    copy_str(&mut out, payload, key);
                }
                if let Some(kind) = payload
                    .get("sandbox_policy")
                    .and_then(|p| p.get("type").or_else(|| p.get("mode")))
                    .and_then(Value::as_str)
                {
                    out.insert("sandbox".into(), kind.into());
                }
            }
            "event_msg" => self.codex_event(&mut out, payload),
            "response_item" => self.codex_item(&mut out, payload),
            _ => {
                if let Some(numbers) = numeric_skeleton(payload) {
                    out.insert("numbers".into(), numbers);
                }
            }
        }
        Value::Object(out)
    }

    fn codex_event(&mut self, out: &mut Map<String, Value>, payload: &Value) {
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        out.insert("event".into(), kind.into());
        copy_str(out, payload, "turn_id");
        match kind {
            "agent_message"
            | "user_message"
            | "agent_reasoning"
            | "agent_reasoning_raw_content" => {
                let text = payload
                    .get("message")
                    .or_else(|| payload.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.text_fields(out, kind, text);
            }
            "error" | "stream_error" => {
                let text = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                out.insert("error_class".into(), super::error_class(text).into());
            }
            _ => {
                if let Some(numbers) = numeric_skeleton(payload) {
                    out.insert("numbers".into(), numbers);
                }
            }
        }
    }

    fn codex_item(&mut self, out: &mut Map<String, Value>, payload: &Value) {
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        out.insert("item".into(), kind.into());
        for key in ["role", "name", "status"] {
            copy_str(out, payload, key);
        }
        match kind {
            "message" => {
                let text = content_text(payload.get("content"));
                self.text_fields(out, "message", &text);
            }
            "reasoning" => {
                let summary = content_text(payload.get("summary"));
                self.text_fields(out, "reasoning", &summary);
                if let Some(enc) = payload.get("encrypted_content").and_then(Value::as_str) {
                    out.insert("encrypted_len".into(), enc.len().into());
                }
            }
            _ => {
                for (key, len_key) in [
                    ("arguments", "input_len"),
                    ("input", "input_len"),
                    ("output", "output_len"),
                ] {
                    if let Some(value) = payload.get(key) {
                        out.insert(len_key.into(), value_len(value).into());
                    }
                }
            }
        }
    }

    /// 投影 claude 转录的一行。
    pub fn claude(&mut self, line: &Value) -> Value {
        self.lines += 1;
        let mut out = Map::new();
        for key in [
            "timestamp",
            "type",
            "subtype",
            "version",
            "uuid",
            "parentUuid",
            "level",
        ] {
            copy_str(&mut out, line, key);
        }
        for key in ["isSidechain", "isMeta"] {
            if let Some(flag) = line.get(key).and_then(Value::as_bool) {
                out.insert(key.into(), flag.into());
            }
        }
        for key in [
            "durationMs",
            "thinkingDurationMs",
            "messageCount",
            "hookCount",
        ] {
            copy_num(&mut out, line, key);
        }
        let kind = line.get("type").and_then(Value::as_str).unwrap_or_default();
        if matches!(kind, "assistant" | "user") {
            for key in ["effort", "requestedModel"] {
                copy_str(&mut out, line, key);
            }
            if let Some(message) = line.get("message") {
                for key in ["model", "stop_reason"] {
                    copy_str(&mut out, message, key);
                }
                if let Some(usage) = message.get("usage").and_then(numeric_skeleton) {
                    out.insert("usage".into(), usage);
                }
                let blocks = self.claude_blocks(kind, message.get("content"));
                if !blocks.is_empty() {
                    out.insert("content".into(), Value::Array(blocks));
                }
            }
        }
        Value::Object(out)
    }

    fn claude_blocks(&mut self, kind: &str, content: Option<&Value>) -> Vec<Value> {
        let blocks: Vec<Value> = match content {
            Some(Value::String(text)) => vec![serde_json::json!({"type": "text", "text": text})],
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        blocks
            .iter()
            .map(|block| {
                let mut out = Map::new();
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                out.insert("type".into(), block_type.into());
                copy_str(&mut out, block, "name");
                match block_type {
                    "text" | "thinking" => {
                        let field = if block_type == "text" {
                            "text"
                        } else {
                            "thinking"
                        };
                        let text = block.get(field).and_then(Value::as_str).unwrap_or_default();
                        self.text_fields(&mut out, &format!("{kind}/{block_type}"), text);
                    }
                    "tool_use" => {
                        if let Some(input) = block.get("input") {
                            out.insert("input_len".into(), value_len(input).into());
                        }
                    }
                    "tool_result" => {
                        if let Some(result) = block.get("content") {
                            out.insert("output_len".into(), value_len(result).into());
                        }
                        if let Some(err) = block.get("is_error").and_then(Value::as_bool) {
                            out.insert("is_error".into(), err.into());
                        }
                    }
                    _ => {}
                }
                Value::Object(out)
            })
            .collect()
    }

    /// 正文只留长度;与本文件之前某行(同类)逐字相同时记 `repeat_of_line`(从 1 数的行号)。
    fn text_fields(&mut self, out: &mut Map<String, Value>, kind: &str, text: &str) {
        out.insert("text_len".into(), text.chars().count().into());
        if text.is_empty() {
            return;
        }
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let key = (kind.to_string(), hasher.finish());
        match self.seen.get(&key) {
            Some(first) => {
                out.insert("repeat_of_line".into(), (*first).into());
            }
            None => {
                self.seen.insert(key, self.lines);
            }
        }
    }
}

fn copy_str(out: &mut Map<String, Value>, from: &Value, key: &str) {
    if let Some(value) = from.get(key).and_then(Value::as_str) {
        out.insert(key.into(), value.into());
    }
}

fn copy_num(out: &mut Map<String, Value>, from: &Value, key: &str) {
    if let Some(value) = from.get(key).filter(|v| v.is_number()) {
        out.insert(key.into(), value.clone());
    }
}

/// codex 的 content / summary 数组里各段文字连起来。
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn value_len(value: &Value) -> usize {
    match value {
        Value::String(s) => s.chars().count(),
        other => serde_json::to_string(other).map_or(0, |s| s.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_canary(v: &Value) {
        let s = v.to_string();
        assert!(!s.contains("CANARY"), "{s}");
    }

    #[test]
    fn codex_metadata_survives_and_text_does_not() {
        let mut p = NativeProjector::default();
        let meta = p.codex(&json!({
            "timestamp": "2026-10-29T03:00:07.256Z", "type": "session_meta",
            "payload": {"cli_version": "0.141.0", "originator": "summon", "id": "th-1",
                "base_instructions": "CANARY", "creator_user_id": "CANARY", "cwd": "/CANARY"}
        }));
        no_canary(&meta);
        assert_eq!(meta["cli_version"], "0.141.0");

        let ctx = p.codex(&json!({
            "timestamp": "2026-10-29T03:00:08Z", "type": "turn_context",
            "payload": {"model": "gpt-x", "effort": "low", "cwd": "/CANARY",
                "sandbox_policy": {"type": "danger-full-access"}}
        }));
        no_canary(&ctx);
        assert_eq!(ctx["model"], "gpt-x");
        assert_eq!(ctx["sandbox"], "danger-full-access");

        for line in [
            json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "CANARY"}]}}),
            json!({"type": "response_item", "payload": {"type": "reasoning",
                "summary": [{"type": "summary_text", "text": "CANARY"}], "encrypted_content": "CANARY"}}),
            json!({"type": "response_item", "payload": {"type": "custom_tool_call", "name": "exec",
                "input": "CANARY"}}),
            json!({"type": "response_item", "payload": {"type": "custom_tool_call_output",
                "output": "CANARY"}}),
            json!({"type": "event_msg", "payload": {"type": "agent_reasoning", "text": "CANARY"}}),
            json!({"type": "event_msg", "payload": {"type": "error", "message": "CANARY newer version"}}),
            json!({"type": "event_msg", "payload": {"type": "task_complete",
                "last_agent_message": "CANARY", "duration_ms": 5}}),
            json!({"type": "world_state", "payload": {"state": "CANARY", "n": 1}}),
        ] {
            no_canary(&p.codex(&line));
        }
    }

    #[test]
    fn repeated_reasoning_points_back_to_the_first_line() {
        let mut p = NativeProjector::default();
        let reasoning = |text: &str| json!({"type": "event_msg", "payload": {"type": "agent_reasoning", "text": text}});
        assert!(p.codex(&reasoning("loop")).get("repeat_of_line").is_none());
        assert!(p.codex(&reasoning("else")).get("repeat_of_line").is_none());
        assert_eq!(p.codex(&reasoning("loop"))["repeat_of_line"], 1);
    }

    #[test]
    fn claude_lines_keep_model_usage_and_block_shapes() {
        let mut p = NativeProjector::default();
        let out = p.claude(&json!({
            "type": "assistant", "timestamp": "2026-10-29T03:00:08Z", "version": "2.1.300",
            "cwd": "/CANARY", "gitBranch": "CANARY",
            "message": {"model": "claude-x", "stop_reason": "tool_use",
                "usage": {"input_tokens": 3, "service_tier": "CANARY"},
                "content": [
                    {"type": "thinking", "thinking": "CANARY"},
                    {"type": "text", "text": "CANARY"},
                    {"type": "tool_use", "name": "Bash", "input": {"command": "CANARY"}}
                ]}
        }));
        no_canary(&out);
        assert_eq!(out["message"], Value::Null);
        assert_eq!(out["model"], "claude-x");
        assert_eq!(out["usage"]["input_tokens"], 3);
        assert_eq!(out["content"][2]["name"], "Bash");

        let user = p.claude(&json!({
            "type": "user", "message": {"content": [
                {"type": "tool_result", "content": "CANARY", "is_error": true}]}
        }));
        no_canary(&user);
        let prompt = p.claude(&json!({"type": "user", "message": {"content": "CANARY"}}));
        no_canary(&prompt);
        for other in [
            json!({"type": "last-prompt", "lastPrompt": "CANARY"}),
            json!({"type": "queue-operation", "content": "CANARY"}),
            json!({"type": "attachment", "attachment": {"x": "CANARY"}}),
            json!({"type": "system", "subtype": "stop_hook_summary", "stopReason": "CANARY"}),
        ] {
            no_canary(&p.claude(&other));
        }
    }

    #[test]
    fn line_time_reads_rfc3339() {
        assert_eq!(
            line_time_ms(&json!({"timestamp": "1970-01-01T00:00:01Z"})),
            Some(1000)
        );
        assert_eq!(line_time_ms(&json!({"type": "x"})), None);
    }
}
