//! 外部 Claude transcript 的隐私收缩解析器。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, BufRead};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsuleLimits {
    pub max_messages: usize,
    pub max_output_bytes: usize,
    pub max_line_bytes: usize,
}

impl Default for CapsuleLimits {
    fn default() -> Self {
        Self {
            max_messages: 40,
            max_output_bytes: 64 * 1024,
            max_line_bytes: 1024 * 1024,
        }
    }
}

/// 召回卡“起始任务”的读取预算。只扫 transcript 前缀并返回第一条可见用户输入：它比最后一句
/// 更稳定，也避免为一张列表卡读取完整长会话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptPreviewLimits {
    pub max_scan_bytes: u64,
    pub max_line_bytes: usize,
    pub max_preview_chars: usize,
}

impl Default for PromptPreviewLimits {
    fn default() -> Self {
        Self {
            max_scan_bytes: 256 * 1024,
            max_line_bytes: 64 * 1024,
            max_preview_chars: 240,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleMessage {
    pub role: CapsuleRole,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContextCapsule {
    pub messages: Vec<CapsuleMessage>,
    pub truncated: bool,
    pub skipped_lines: u64,
}

/// 流式扫描 transcript，但内存只保留一条有界行与最后一圈可见消息。畸形/半写行只跳过，
/// 不让 context capsule 成为读取 thinking/tool/system/sidechain 的旁路。
pub fn read_context_capsule<R: BufRead>(
    mut reader: R,
    limits: CapsuleLimits,
) -> io::Result<ContextCapsule> {
    if limits.max_messages == 0 || limits.max_output_bytes == 0 || limits.max_line_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            summon_i18n::text!("r1bea82ff83"),
        ));
    }
    let mut ring = VecDeque::new();
    let mut output_bytes = 0usize;
    let mut capsule = ContextCapsule::default();

    while let Some((line, oversized)) = read_bounded_line(&mut reader, limits.max_line_bytes)? {
        if oversized {
            capsule.skipped_lines = capsule.skipped_lines.saturating_add(1);
            capsule.truncated = true;
            continue;
        }
        let Some(mut message) = parse_visible_message(&line) else {
            if serde_json::from_slice::<Value>(&line).is_err() {
                capsule.skipped_lines = capsule.skipped_lines.saturating_add(1);
            }
            continue;
        };
        if message.text.len() > limits.max_output_bytes {
            message.text = clip_utf8(&message.text, limits.max_output_bytes).to_string();
            capsule.truncated = true;
        }
        output_bytes = output_bytes.saturating_add(message.text.len());
        ring.push_back(message);
        while ring.len() > limits.max_messages || output_bytes > limits.max_output_bytes {
            if let Some(removed) = ring.pop_front() {
                output_bytes = output_bytes.saturating_sub(removed.text.len());
                capsule.truncated = true;
            } else {
                break;
            }
        }
    }
    capsule.messages = ring.into_iter().collect();
    Ok(capsule)
}

/// 读取第一条外部、非支线、纯文本 user 输入并压成一行。复用 context capsule 的可见性规则，
/// 因此不会把 thinking、tool result、system/internal 或 sidechain 当作会话标题。
pub fn read_first_user_prompt<R: BufRead>(
    reader: R,
    limits: PromptPreviewLimits,
) -> io::Result<Option<String>> {
    if limits.max_scan_bytes == 0 || limits.max_line_bytes == 0 || limits.max_preview_chars == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            summon_i18n::text!("rb696f0c5e2"),
        ));
    }
    let mut reader = reader.take(limits.max_scan_bytes);
    while let Some((line, oversized)) = read_bounded_line(&mut reader, limits.max_line_bytes)? {
        if oversized {
            continue;
        }
        let Some(message) = parse_visible_message(&line) else {
            continue;
        };
        if message.role != CapsuleRole::User {
            continue;
        }
        if is_claude_control_user_text(&message.text) {
            continue;
        }
        let compact = message
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if compact.is_empty() {
            continue;
        }
        return Ok(Some(clip_chars_with_ellipsis(
            &compact,
            limits.max_preview_chars,
        )));
    }
    Ok(None)
}

/// Claude 会把 slash command、local command 输出和异步通知伪装成 external user 字符串写进
/// transcript。它们不是用户任务，整条跳过；不能只删尖括号，否则 caveat/command 名仍会被
/// 当成标题。这里只识别 Claude 自己已知的顶层 envelope，不泛化过滤用户正常的 HTML/XML。
fn is_claude_control_user_text(text: &str) -> bool {
    let Some(after_open) = text.trim_start().strip_prefix('<') else {
        return false;
    };
    let tag = after_open
        .split(|character: char| character == '>' || character.is_whitespace())
        .next()
        .unwrap_or_default();
    matches!(
        tag,
        "local-command-caveat"
            | "local-command-stdout"
            | "local-command-stderr"
            | "command-message"
            | "command-name"
            | "command-args"
            | "task-notification"
            | "bash-input"
            | "bash-stdout"
            | "bash-stderr"
            | "system-reminder"
    )
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    limit: usize,
) -> io::Result<Option<(Vec<u8>, bool)>> {
    let mut line = Vec::new();
    let mut oversized = false;
    let mut saw_bytes = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(saw_bytes.then_some((line, oversized)));
        }
        saw_bytes = true;
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        let chunk = &available[..end];
        let content = chunk.strip_suffix(b"\n").unwrap_or(chunk);
        let remaining = limit.saturating_sub(line.len());
        line.extend_from_slice(&content[..content.len().min(remaining)]);
        if content.len() > remaining {
            oversized = true;
        }
        let ended = chunk.ends_with(b"\n");
        reader.consume(end);
        if ended {
            return Ok(Some((line, oversized)));
        }
    }
}

fn parse_visible_message(line: &[u8]) -> Option<CapsuleMessage> {
    let value: Value = serde_json::from_slice(line).ok()?;
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || value.get("userType").and_then(Value::as_str) != Some("external")
    {
        return None;
    }
    let event_type = value.get("type").and_then(Value::as_str)?;
    let message = value.get("message")?;
    match event_type {
        "user"
            if message.get("role").and_then(Value::as_str) == Some("user")
                && message.get("content").and_then(Value::as_str).is_some() =>
        {
            let text = message.get("content")?.as_str()?.trim();
            (!text.is_empty()).then(|| CapsuleMessage {
                role: CapsuleRole::User,
                text: text.to_string(),
            })
        }
        "assistant"
            if message.get("role").and_then(Value::as_str) == Some("assistant")
                && message.get("stop_reason").and_then(Value::as_str) == Some("end_turn") =>
        {
            let text = message
                .get("content")?
                .as_array()?
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(CapsuleMessage {
                role: CapsuleRole::Assistant,
                text,
            })
        }
        _ => None,
    }
}

fn clip_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
}

fn clip_chars_with_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars()
        .take(max_chars.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const FIXTURE: &str = include_str!("../../tests/fixtures/claude-transcript-external.jsonl");

    #[test]
    fn capsule_keeps_only_external_user_and_final_assistant_text() {
        let capsule = read_context_capsule(Cursor::new(FIXTURE), CapsuleLimits::default()).unwrap();
        assert_eq!(capsule.messages.len(), 2);
        assert_eq!(capsule.messages[0].role, CapsuleRole::User);
        assert_eq!(capsule.messages[0].text, "请概括当前改动。");
        assert_eq!(capsule.messages[1].role, CapsuleRole::Assistant);
        assert_eq!(capsule.messages[1].text, "已完成改动并通过测试。");
        let visible = serde_json::to_string(&capsule).unwrap();
        for hidden in ["内部推理", "private.txt", "工具结果", "支线"] {
            assert!(!visible.contains(hidden), "胶囊泄漏了 {hidden}");
        }
    }

    #[test]
    fn malformed_partial_and_oversized_lines_do_not_panic_or_escape_bounds() {
        let huge = "x".repeat(10_000);
        let input = format!(
            "not-json\n{{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{{\"role\":\"user\",\"content\":\"{huge}\"}}}}\n{{\"type\":"
        );
        let limits = CapsuleLimits {
            max_messages: 2,
            max_output_bytes: 64,
            max_line_bytes: 128,
        };
        let capsule = read_context_capsule(Cursor::new(input), limits).unwrap();
        assert!(capsule.messages.is_empty());
        assert!(capsule.skipped_lines >= 3);
    }

    #[test]
    fn bounded_ring_keeps_the_newest_visible_messages() {
        let input = (0..5)
            .map(|n| format!("{{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{{\"role\":\"user\",\"content\":\"message-{n}\"}}}}\n"))
            .collect::<String>();
        let capsule = read_context_capsule(
            Cursor::new(input),
            CapsuleLimits {
                max_messages: 2,
                max_output_bytes: 100,
                max_line_bytes: 1024,
            },
        )
        .unwrap();
        assert_eq!(capsule.messages.len(), 2);
        assert_eq!(capsule.messages[0].text, "message-3");
        assert_eq!(capsule.messages[1].text, "message-4");
        assert!(capsule.truncated);
    }

    #[test]
    fn first_user_prompt_is_private_stable_and_bounded() {
        let input = concat!(
            "{\"type\":\"user\",\"isSidechain\":true,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"支线秘密\"}}\n",
            "{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"content\":\"工具秘密\"}]}}\n",
            "{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"<local-command-caveat>Caveat: generated messages</local-command-caveat>\"}}\n",
            "{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"<command-message>summon</command-message> <command-name>/summon</command-name> <command-args>starlight</command-args>\"}}\n",
            "{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"<task-notification><status>completed</status></task-notification>\"}}\n",
            "{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"  修复   登录页\\n的崩溃，并补回归测试；这里还有一段超过旧版四十八字限制、但仍应完整显示的任务背景。  \"}}\n",
            "{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"继续\"}}\n"
        );
        let preview = read_first_user_prompt(
            Cursor::new(input),
            PromptPreviewLimits {
                max_scan_bytes: 64 * 1024,
                max_line_bytes: 8 * 1024,
                max_preview_chars: 240,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            preview,
            "修复 登录页 的崩溃，并补回归测试；这里还有一段超过旧版四十八字限制、但仍应完整显示的任务背景。"
        );
        for hidden in [
            "支线秘密",
            "工具秘密",
            "继续",
            "local-command-caveat",
            "command-message",
            "task-notification",
        ] {
            assert!(!preview.contains(hidden));
        }
    }

    #[test]
    fn first_user_prompt_clips_only_extreme_input_to_explicit_budget() {
        let long = "任".repeat(40);
        let input = format!(
            "{{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{{\"role\":\"user\",\"content\":\"{long}\"}}}}\n"
        );
        let preview = read_first_user_prompt(
            Cursor::new(input),
            PromptPreviewLimits {
                max_scan_bytes: 64 * 1024,
                max_line_bytes: 8 * 1024,
                max_preview_chars: 20,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(preview, format!("{}…", "任".repeat(19)));
    }

    #[test]
    fn first_user_prompt_preserves_unknown_user_markup() {
        let input = "{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"<View>修复这个布局</View>\"}}\n";
        let preview = read_first_user_prompt(
            Cursor::new(input),
            PromptPreviewLimits {
                max_scan_bytes: 64 * 1024,
                max_line_bytes: 8 * 1024,
                max_preview_chars: 240,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(preview, "<View>修复这个布局</View>");
    }

    #[test]
    fn first_user_prompt_stops_at_scan_budget() {
        let input = format!(
            "{}\n{{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{{\"role\":\"user\",\"content\":\"太晚出现\"}}}}\n",
            "x".repeat(256)
        );
        let preview = read_first_user_prompt(
            Cursor::new(input),
            PromptPreviewLimits {
                max_scan_bytes: 64,
                max_line_bytes: 32,
                max_preview_chars: 20,
            },
        )
        .unwrap();
        assert_eq!(preview, None);
    }
}
