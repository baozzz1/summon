//! events.jsonl 的诊断投影:保留时间、类型、工具名、流来源这些结构字段,正文只留长度。
//! 同一轮里与之前某条逐字相同的正文记 `repeat_of`,推理循环(正文反复重复)不看正文也认得出。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use serde::Serialize;

use crate::observe::event_log::{EventRecord, UsageRec};

/// 诊断包里的一条事件。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiagEvent {
    pub ts: u64,
    pub seq: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageRec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_len: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_len: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_len: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<&'static str>,
    /// 同一轮里正文与这条 seq 逐字相同。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_of: Option<u64>,
}

/// 逐条投影;记着本次导出里见过的正文指纹,用来标重复。
#[derive(Debug, Default)]
pub struct EventProjector {
    seen: HashMap<(String, String, u64), u64>,
}

impl EventProjector {
    /// 投影一行 events.jsonl。不是事件记录的行返回 `None`。
    pub fn project_line(&mut self, line: &str) -> Option<DiagEvent> {
        let rec: EventRecord = serde_json::from_str(line).ok()?;
        Some(self.project(&rec))
    }

    pub fn project(&mut self, rec: &EventRecord) -> DiagEvent {
        let text = rec.text.as_deref().filter(|t| !t.is_empty());
        let repeat_of = text
            .filter(|_| rec.partial != Some(true))
            .and_then(|t| self.repeat_of(rec, t));
        DiagEvent {
            ts: rec.ts,
            seq: rec.seq,
            kind: rec.kind.clone(),
            turn: rec.turn.clone(),
            model: rec.model.clone(),
            session_id: rec.session_id.clone(),
            tool: rec.tool.clone(),
            tool_id: rec.tool_id.clone(),
            stop_reason: rec.stop_reason.clone(),
            partial: rec.partial,
            stream_source: rec.stream_source.clone(),
            source_item_id: rec.source_item_id.clone(),
            usage: rec.usage.clone(),
            text_len: text.map(|t| t.chars().count()),
            input_len: rec.tool_input.as_ref().map(json_len),
            result_len: rec.tool_result.as_ref().map(json_len),
            error_class: (rec.kind == "error").then(|| super::error_class(text.unwrap_or(""))),
            repeat_of,
        }
    }

    fn repeat_of(&mut self, rec: &EventRecord, text: &str) -> Option<u64> {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let key = (
            rec.turn.clone().unwrap_or_default(),
            rec.kind.clone(),
            hasher.finish(),
        );
        match self.seen.get(&key) {
            Some(first) => Some(*first),
            None => {
                self.seen.insert(key, rec.seq);
                None
            }
        }
    }
}

/// 工具入参 / 结果的大小:字符串按字符数,其余按序列化后的字节数。
fn json_len(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => s.chars().count(),
        other => serde_json::to_string(other).map_or(0, |s| s.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: serde_json::Value) -> String {
        v.to_string()
    }

    #[test]
    fn content_fields_become_lengths() {
        let mut p = EventProjector::default();
        let ev = p
            .project_line(&line(serde_json::json!({
                "ts": 10, "seq": 1, "kind": "tool_use", "tool": "Bash",
                "tool_input": {"command": "echo CANARY"}, "turn": "t1",
                "unknown_future_field": "CANARY",
            })))
            .unwrap();
        let out = serde_json::to_string(&ev).unwrap();
        assert!(!out.contains("CANARY"), "{out}");
        assert_eq!(ev.tool.as_deref(), Some("Bash"));
        assert_eq!(ev.input_len, Some(r#"{"command":"echo CANARY"}"#.len()));

        let res = p
            .project_line(&line(serde_json::json!({
                "ts": 11, "seq": 2, "kind": "tool_result", "tool_result": "CANARY output",
            })))
            .unwrap();
        assert_eq!(res.result_len, Some("CANARY output".chars().count()));
        assert!(!serde_json::to_string(&res).unwrap().contains("CANARY"));
    }

    #[test]
    fn identical_text_in_the_same_turn_points_back_to_its_first_occurrence() {
        let mut p = EventProjector::default();
        let ev = |seq: u64, turn: &str, text: &str| {
            line(
                serde_json::json!({"ts": seq, "seq": seq, "kind": "thinking", "text": text, "turn": turn}),
            )
        };
        assert_eq!(
            p.project_line(&ev(1, "t1", "looping")).unwrap().repeat_of,
            None
        );
        assert_eq!(
            p.project_line(&ev(2, "t1", "other")).unwrap().repeat_of,
            None
        );
        assert_eq!(
            p.project_line(&ev(3, "t1", "looping")).unwrap().repeat_of,
            Some(1)
        );
        assert_eq!(
            p.project_line(&ev(4, "t2", "looping")).unwrap().repeat_of,
            None
        );
    }

    #[test]
    fn streaming_deltas_are_not_checked_for_repeats() {
        let mut p = EventProjector::default();
        let delta = |seq: u64| {
            line(
                serde_json::json!({"ts": seq, "seq": seq, "kind": "assistant", "text": "the", "partial": true}),
            )
        };
        assert_eq!(p.project_line(&delta(1)).unwrap().repeat_of, None);
        assert_eq!(p.project_line(&delta(2)).unwrap().repeat_of, None);
    }

    #[test]
    fn errors_keep_only_their_class() {
        let mut p = EventProjector::default();
        let ev = p
            .project_line(&line(serde_json::json!({
                "ts": 1, "seq": 1, "kind": "error",
                "text": "CANARY: model requires a newer version of Codex",
            })))
            .unwrap();
        assert_eq!(ev.error_class, Some("client_version"));
        assert!(!serde_json::to_string(&ev).unwrap().contains("CANARY"));
    }

    #[test]
    fn lines_that_are_not_events_are_skipped() {
        let mut p = EventProjector::default();
        assert!(p.project_line("not json").is_none());
        assert!(p.project_line("{\"seq\": 1}").is_none());
    }
}
