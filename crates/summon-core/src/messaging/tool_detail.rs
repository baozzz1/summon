//! 从工具调用的入参与结果里取卡片 / CoT 要用的几项(纯函数)。
//!
//! 主 agent 的轮内投影与子 agent 的投影([`crate::messaging::subagent_projection`])共用这一份,
//! 两边各写一份的话,取值规则迟早会漂。

/// 非 shell 类工具的目标路径(`file_path` / `path` / `notebook_path`),给「读取 xxx.rs」这类标签用。
/// 只认这几个已知键,**不遍历整个 tool_input** —— 那会把任意入参(可能含敏感内容)漏上卡片。
pub fn tool_target_path(tool_input: Option<&serde_json::Value>) -> Option<&str> {
    let v = tool_input?;
    ["file_path", "path", "notebook_path"]
        .iter()
        .find_map(|k| v.get(k).and_then(serde_json::Value::as_str))
}

/// CoT 工具结果只在文件扩展名或 shell 命令能证实类型时声明 fenced-code language。
/// 未知扩展名保持 None，避免错误高亮比纯文本更难读。
pub fn cot_result_language(
    tool_input: Option<&serde_json::Value>,
    command: Option<&str>,
) -> Option<&'static str> {
    let extension = tool_target_path(tool_input)
        .and_then(|path| path.rsplit_once('.').map(|(_, extension)| extension))
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("rs") => Some("rust"),
        Some("md" | "markdown") => Some("markdown"),
        Some("json" | "jsonl") => Some("json"),
        Some("yaml" | "yml") => Some("yaml"),
        Some("toml") => Some("toml"),
        Some("py" | "pyi") => Some("python"),
        Some("js" | "mjs" | "cjs" | "jsx") => Some("javascript"),
        Some("ts" | "mts" | "cts" | "tsx") => Some("typescript"),
        Some("sh" | "bash" | "zsh") => Some("bash"),
        Some("go") => Some("go"),
        Some("swift") => Some("swift"),
        Some("c") => Some("c"),
        Some("cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx") => Some("cpp"),
        Some("java") => Some("java"),
        Some("kt" | "kts") => Some("kotlin"),
        Some("rb") => Some("ruby"),
        Some("php") => Some("php"),
        Some("html" | "htm") => Some("html"),
        Some("css") => Some("css"),
        Some("sql") => Some("sql"),
        Some("xml") => Some("xml"),
        _ if command.is_some() => Some("bash"),
        _ => None,
    }
}

/// 工具调用的**人话摘要**(claude 自家模型在 tool_use 的 `input.description` 里给的一句话,
/// 如「合并后隔离验证」)。
///
/// 卡片上的工具轨迹优先显示它:命令原文又长又是给机器读的,而这句是模型自己写的意图说明,
/// 一眼就知道这步在干什么。owner 2026-08-12 从转录里认出这个字段并提的。
///
/// **只有 claude 有**:codex / cursor 的 tool_use 不带 `description`,取不到就回落命令原文,
/// 两条路各自完整,不需要按 CLI 分叉。空串按「没有」处理 —— 它和缺字段是同一个意思。
pub fn tool_description(tool_input: Option<&serde_json::Value>) -> Option<&str> {
    tool_input
        .and_then(|v| v.get("description"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty())
}

/// 从 ToolUse 的 `tool_input` 取「可读命令」给运行指示用:codex 直接是命令串、claude 是
/// `{"command": "..."}`。取不到 → None(指示回退友好工具名)。
pub fn tool_command_detail(tool_input: Option<&serde_json::Value>) -> Option<&str> {
    tool_input.and_then(|v| {
        v.as_str()
            .or_else(|| v.get("command").and_then(serde_json::Value::as_str))
    })
}

/// 工具结果里给人看的正文:字符串原样,内容块数组取文本拼行,对象按已知字段取;都取不到就
/// 整体序列化。空白结果按「没有」处理。
pub fn tool_result_summary(result: Option<&serde_json::Value>) -> Option<String> {
    let result = result?;
    let summary = match result {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .filter_map(|value| {
                value.as_str().map(str::to_string).or_else(|| {
                    value
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
            })
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Object(value) => value
            .get("content")
            .or_else(|| value.get("text"))
            .or_else(|| value.get("stdout"))
            .or_else(|| value.get("interleavedOutput"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                value
                    .get("success")
                    .and_then(|success| tool_result_summary(Some(success)))
            })
            .or_else(|| {
                value
                    .get("stderr")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| result.to_string()),
        _ => result.to_string(),
    };
    (!summary.trim().is_empty()).then_some(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cot_result_language_uses_verified_file_extensions_only() {
        let rust = serde_json::json!({"file_path": "/tmp/lib.RS"});
        let markdown = serde_json::json!({"path": "docs/README.md"});
        let unknown = serde_json::json!({"path": "artifact.custom"});
        assert_eq!(cot_result_language(Some(&rust), None), Some("rust"));
        assert_eq!(cot_result_language(Some(&markdown), None), Some("markdown"));
        assert_eq!(cot_result_language(Some(&unknown), None), None);
        assert_eq!(cot_result_language(None, Some("cargo test")), Some("bash"));
    }

    #[test]
    fn tool_result_summary_extracts_textual_content() {
        let result = serde_json::json!([{"type": "text", "text": "完成读取"}, "第二段"]);
        assert_eq!(
            tool_result_summary(Some(&result)).as_deref(),
            Some("完成读取\n第二段")
        );
        let cursor = serde_json::json!({
            "success": {"stdout": "hello\n", "stderr": "", "exitCode": 0}
        });
        assert_eq!(
            tool_result_summary(Some(&cursor)).as_deref(),
            Some("hello\n")
        );
    }

    /// 工具卡的 preview 优先用 claude 给的 `input.description`(人话摘要),没有才回落命令原文。
    ///
    /// owner 2026-08-12 从转录里认出这个字段:同一次 Bash 调用,`input.description` 是
    /// 「合并后隔离验证」,而 `command` 是一长串 `CARGO_TARGET_DIR=… cargo nextest run …`。
    /// 卡片位置就一行,显意图比显命令有用得多。codex/cursor 不带这个字段,自动回落。
    #[test]
    fn tool_preview_prefers_description_over_command() {
        let claude = serde_json::json!({
            "command": "CARGO_TARGET_DIR=/tmp/x cargo nextest run",
            "description": "合并后隔离验证"
        });
        assert_eq!(tool_description(Some(&claude)), Some("合并后隔离验证"));
        // 命令原文仍取得到——日志与 events.jsonl 靠它,排查时不丢真实命令。
        assert_eq!(
            tool_command_detail(Some(&claude)),
            Some("CARGO_TARGET_DIR=/tmp/x cargo nextest run")
        );

        // codex/cursor:没有 description → None,调用点据此回落命令原文。
        let headless = serde_json::json!({ "command": "ls -la" });
        assert_eq!(tool_description(Some(&headless)), None);

        // 空串/纯空白按「没有」处理:它和缺字段是同一个意思,不该渲染成空白标签。
        let blank = serde_json::json!({ "command": "ls", "description": "   " });
        assert_eq!(tool_description(Some(&blank)), None);
        assert_eq!(tool_description(None), None);

        // 超长 description 与命令原文共用同一套裁剪,免得同一张卡里出现两种宽度。
        let long = "验".repeat(80);
        let clipped = crate::messaging::segment::clip_tool_label(&long);
        assert!(
            clipped.chars().count() <= 51,
            "实得 {}",
            clipped.chars().count()
        );
        assert!(clipped.ends_with('…'));
    }
}
