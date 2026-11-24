//! claude 写进转录的 API 错误记录。
//!
//! 请求失败时 claude 往转录追加一条合成的 assistant 记录:`message.model` 是 `<synthetic>`,
//! 正文是给人看的错误说明,结构化标记挂在**记录最外层** —— `isApiErrorMessage: true` 与错误
//! 类别 `error`(`server_error` / `rate_limit` / `authentication_failed` / `unknown` …),
//! `message.error` 为 null。较早的版本把类别写在 `message.error`,两种都认。
//!
//! 流中断时这条记录紧跟在一条 `end_turn` 的半截回复之后,所以识别不能依赖它落在哪一轮里。

use serde_json::Value;

/// 流中断这一类错误的类别名:会话还活着、上下文都在,发一句「继续」就能接着跑。
const STREAM_INTERRUPTED_CATEGORY: &str = "server_error";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeApiError {
    /// 记录自己的 `uuid`,同一条错误在不同观察路径之间去重用。
    pub uuid: Option<String>,
    /// 错误类别。记录没给出类别时为 `None`。
    pub category: Option<String>,
    /// 记录正文(各 text 块按行拼接、去掉首尾空白)。
    pub text: String,
}

impl ClaudeApiError {
    /// 一行转录是不是 API 错误记录。坏行、非 assistant 记录、普通回复都返回 `None`。
    pub fn from_line(line: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(line.trim()).ok()?;
        Self::from_value(&value)
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        // 子 agent 的记录不代表主轮的结局(主 agent 可以自己处理子 agent 的失败):按它收主轮会
        // 截断仍在运行的主 agent。子 agent 的错误另经 [`Self::from_subagent_line`] 单独上报。
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        Self::from_record(value)
    }

    /// 子 agent 转录(`<sid>/subagents/agent-<id>.jsonl`,记录都带 `isSidechain: true`)里的一行
    /// 是不是 API 错误记录。判据与主转录相同,只是不排除子 agent 的记录。
    pub fn from_subagent_line(line: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(line.trim()).ok()?;
        Self::from_record(&value)
    }

    fn from_record(value: &Value) -> Option<Self> {
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        let message = value.get("message");
        let flagged = value.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true);
        let legacy = message
            .and_then(|message| message.get("error"))
            .and_then(Value::as_str)
            .filter(|error| !error.is_empty());
        if !flagged && legacy.is_none() {
            return None;
        }
        let category = value
            .get("error")
            .and_then(Value::as_str)
            .filter(|error| !error.is_empty())
            .or(legacy)
            .map(str::to_string);
        let text = message
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
            .trim()
            .to_string();
        Some(Self {
            uuid: value
                .get("uuid")
                .and_then(Value::as_str)
                .filter(|uuid| !uuid.is_empty())
                .map(str::to_string),
            category,
            text,
        })
    }

    /// 展示给用户的错误原文:优先记录正文,没有正文时退回类别名。
    pub fn reason(&self) -> String {
        if !self.text.is_empty() {
            return self.text.clone();
        }
        self.category.clone().unwrap_or_default()
    }

    /// 发一句「继续」能不能接着跑。以类别为准;没有类别的记录才看正文措辞。
    pub fn stream_interrupted(&self) -> bool {
        match self.category.as_deref() {
            Some(category) => category == STREAM_INTERRUPTED_CATEGORY,
            None => crate::parse::retry_wait::is_stream_stall(&self.text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实测形态:标记与类别在最外层,`message.error` 为 null,stop_reason 是 stop_sequence。
    const STOPPED_ARRIVING: &str = r#"{"parentUuid":"p-1","isSidechain":false,"type":"assistant","uuid":"err-1","timestamp":"2026-11-24T06:26:51.094Z","error":"server_error","isApiErrorMessage":true,"message":{"id":"m-1","container":null,"model":"<synthetic>","role":"assistant","stop_reason":"stop_sequence","stop_sequence":"","type":"message","error":null,"content":[{"type":"text","text":"API Error: The response stopped arriving. The response above may be incomplete."}]}}"#;
    const STOPPED_TEXT: &str =
        "API Error: The response stopped arriving. The response above may be incomplete.";

    #[test]
    fn top_level_flag_and_category_are_read() {
        let error = ClaudeApiError::from_line(STOPPED_ARRIVING).expect("API 错误记录");
        assert_eq!(error.uuid.as_deref(), Some("err-1"));
        assert_eq!(error.category.as_deref(), Some("server_error"));
        assert_eq!(error.reason(), STOPPED_TEXT);
        assert!(error.stream_interrupted());
    }

    /// 流中断的几种措辞都不含 stalled;判据是类别,不是措辞。
    #[test]
    fn every_server_error_wording_is_a_stream_interruption() {
        for text in [
            "API Error: Connection lost mid-response. The response above may be incomplete.",
            "API Error: Your computer went to sleep mid-response. The response above may be incomplete.",
            "API Error: Connection dropped (ECONNRESET)",
        ] {
            let line = STOPPED_ARRIVING.replace(STOPPED_TEXT, text);
            let error = ClaudeApiError::from_line(&line).expect("API 错误记录");
            assert!(error.stream_interrupted(), "{text}");
            assert_eq!(error.reason(), text);
        }
    }

    #[test]
    fn other_categories_are_not_resumable() {
        for (category, text) in [
            (
                "rate_limit",
                "You've hit your weekly limit · resets 3pm (Asia/Singapore)",
            ),
            ("authentication_failed", "Login expired · Please run /login"),
            ("unknown", "API Error: 400 bad request"),
        ] {
            let line = STOPPED_ARRIVING
                .replace("\"server_error\"", &format!("\"{category}\""))
                .replace(STOPPED_TEXT, text);
            let error = ClaudeApiError::from_line(&line).expect("API 错误记录");
            assert_eq!(error.category.as_deref(), Some(category));
            assert!(!error.stream_interrupted(), "{category}");
            assert_eq!(error.reason(), text);
        }
    }

    /// 较早版本把类别写在 `message.error`,没有最外层标记。
    #[test]
    fn legacy_message_error_is_still_recognized() {
        let line = r#"{"type":"assistant","uuid":"old-1","message":{"role":"assistant","stop_reason":"stop_sequence","error":"model_not_found","content":[{"type":"text","text":"There's an issue with the selected model."}]}}"#;
        let error = ClaudeApiError::from_line(line).expect("旧格式 API 错误记录");
        assert_eq!(error.category.as_deref(), Some("model_not_found"));
        assert_eq!(error.reason(), "There's an issue with the selected model.");
        assert!(!error.stream_interrupted());
    }

    /// 没有类别的记录才按措辞判断流中断。
    #[test]
    fn uncategorized_record_falls_back_to_wording() {
        let line = STOPPED_ARRIVING.replace("\"error\":\"server_error\",", "");
        let error = ClaudeApiError::from_line(&line).expect("API 错误记录");
        assert_eq!(error.category, None);
        assert!(error.stream_interrupted());
    }

    #[test]
    fn missing_text_falls_back_to_category() {
        let line = r#"{"type":"assistant","isApiErrorMessage":true,"error":"unknown","message":{"content":[]}}"#;
        assert_eq!(ClaudeApiError::from_line(line).unwrap().reason(), "unknown");
    }

    /// 普通回复、user 记录、正文里谈论 API Error 的回复都不是错误记录。
    #[test]
    fn ordinary_records_are_not_api_errors() {
        for line in [
            r#"{"type":"assistant","uuid":"a","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"API Error: The response stopped arriving 这句报错要怎么处理"}]}}"#,
            r#"{"type":"assistant","uuid":"a","isApiErrorMessage":false,"message":{"content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"user","isApiErrorMessage":true,"error":"server_error","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","isSidechain":true,"isApiErrorMessage":true,"error":"server_error","message":{"content":[{"type":"text","text":"API Error: Connection dropped (ECONNRESET)"}]}}"#,
            "not json",
        ] {
            assert_eq!(ClaudeApiError::from_line(line), None, "{line}");
        }
    }

    /// 实测子 agent 记录(2.1.304):`isSidechain` 与 `agentId` 之外与主转录同形。主转录口径
    /// 不认它(不收主轮),子 agent 口径认它。
    #[test]
    fn subagent_records_are_recognized_only_on_the_subagent_path() {
        let line = r#"{"parentUuid":"p-9","isSidechain":true,"agentId":"a8d4e49d8e32008b9","type":"assistant","uuid":"sub-err-1","timestamp":"2026-10-20T07:19:57.993Z","error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429,"message":{"model":"<synthetic>","role":"assistant","stop_reason":"stop_sequence","error":null,"content":[{"type":"text","text":"You've hit your weekly limit · resets Oct 23 at 3pm (Asia/Singapore)"}]}}"#;
        assert_eq!(
            ClaudeApiError::from_line(line),
            None,
            "主转录口径不认子 agent 记录"
        );
        let error = ClaudeApiError::from_subagent_line(line).expect("子 agent 的 API 错误记录");
        assert_eq!(error.uuid.as_deref(), Some("sub-err-1"));
        assert_eq!(error.category.as_deref(), Some("rate_limit"));
        assert_eq!(
            error.reason(),
            "You've hit your weekly limit · resets Oct 23 at 3pm (Asia/Singapore)"
        );
        for ordinary in [
            r#"{"type":"assistant","isSidechain":true,"agentId":"a1","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"API Error: The response stopped arriving 这句怎么处理"}]}}"#,
            r#"{"type":"user","isSidechain":true,"isApiErrorMessage":true,"error":"server_error","message":{"content":"hi"}}"#,
            "not json",
        ] {
            assert_eq!(
                ClaudeApiError::from_subagent_line(ordinary),
                None,
                "{ordinary}"
            );
        }
    }
}
