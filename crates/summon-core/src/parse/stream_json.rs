//! 解析 `claude -p --output-format stream-json --verbose` 及 `agent --output-format stream-json`
//! (cursor-agent)的 stdout 逐行 JSON。两者 schema 相同,仅 usage 字段命名方式有别:
//! claude 用 snake_case(`input_tokens`/`output_tokens`),cursor 用 camelCase(`inputTokens`/`outputTokens`)。
//! 一行可能含多个内容块(assistant.message.content 是数组),故返回 Vec。
//! 噪音(system/hook_*、rate_limit_event、坏行)返回空。比抓 TUI 可靠:信 CLI 自己的协议标注。

use crate::parse::agent_msg::{AgentKind, AgentMessage, Usage};
use serde_json::Value;

pub fn parse_stream_json_line(line: &str) -> Vec<AgentMessage> {
    let line = line.trim();
    if line.is_empty() {
        return vec![];
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![], // 坏行容错:跳过
    };
    let sid = v
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    match v.get("type").and_then(Value::as_str) {
        Some("system") => {
            // 只认 init 子类型(带规范 session_id);hook_started/hook_response 等噪音忽略
            if v.get("subtype").and_then(Value::as_str) == Some("init") {
                let mut m = AgentMessage::of(AgentKind::System);
                m.session_id = sid;
                vec![m]
            } else {
                vec![]
            }
        }
        Some("assistant") => {
            // stop_reason 挂在 message 层;所有同行 content 块共享同一 stop_reason
            let stop_reason = v
                .get("message")
                .and_then(|msg| msg.get("stop_reason"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // API 错误(model_not_found 等)也在 message 层;有值表示本轮是错误终态
            let api_error = v
                .get("message")
                .and_then(|msg| msg.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // cursor `--stream-partial-output` 的增量 delta 行带顶层 `timestamp_ms`;流末尾的完整
            // 快照行无此字段。claude/codex 的 assistant 行从不带 → partial 恒 false,行为不变。
            let partial = v.get("timestamp_ms").is_some();
            content_blocks(&v)
                .into_iter()
                .filter_map(|b| {
                    assistant_block(
                        &b,
                        sid.clone(),
                        stop_reason.clone(),
                        api_error.clone(),
                        partial,
                    )
                })
                .collect()
        }
        // cursor 无头用顶层 `type:"thinking"` 发 thinking(claude 嵌在 assistant.content 里)。
        // 只认 `subtype=="delta"` 且有 text 的行(completed 无文本、忽略);partial 同 assistant 逻辑。
        Some("thinking") if v.get("subtype").and_then(Value::as_str) == Some("delta") => {
            let text = v.get("text").and_then(Value::as_str);
            if text.is_some_and(|t| !t.trim().is_empty()) {
                let mut m = AgentMessage::of(AgentKind::Thinking);
                m.text = text.map(str::to_string);
                m.session_id = sid;
                m.partial = v.get("timestamp_ms").is_some();
                vec![m]
            } else {
                vec![]
            }
        }
        Some("user") => {
            let async_launch = async_launch(&v);
            content_blocks(&v)
                .into_iter()
                .filter_map(|b| tool_result_block(&b, sid.clone()))
                .map(|mut m| {
                    m.async_launch = async_launch.clone();
                    m
                })
                .collect()
        }
        // cursor 无头用顶层 `tool_call` 行报工具(claude 走 assistant.tool_use 块,codex 走 item)。
        // started 产 ToolUse 作为段边界；completed 产 ToolResult，让过程投影能展示执行结果。
        // 认不出工具体时,started 仍产不带工具名的 ToolUse(段边界照切,投影不建步骤),
        // completed 不产事件:它若照常产 ToolResult,下游按「最后一个未完成的工具」兜底配对,
        // 会替别的工具销账。
        Some("tool_call") if v.get("subtype").and_then(Value::as_str) == Some("started") => {
            let entry = cursor_tool_entry(&v);
            let mut m = AgentMessage::of(AgentKind::ToolUse);
            m.tool = entry.map(|(name, _)| name.to_string());
            m.tool_id = v.get("call_id").and_then(Value::as_str).map(str::to_string);
            m.tool_input = entry.and_then(|(_, tool)| cursor_tool_args(tool));
            m.session_id = sid;
            vec![m]
        }
        Some("tool_call") if v.get("subtype").and_then(Value::as_str) == Some("completed") => {
            let Some((name, tool)) = cursor_tool_entry(&v) else {
                return vec![];
            };
            let mut m = AgentMessage::of(AgentKind::ToolResult);
            m.tool = Some(name.to_string());
            m.tool_id = v.get("call_id").and_then(Value::as_str).map(str::to_string);
            m.session_id = sid;
            m.tool_result = tool.get("result").cloned();
            m.tool_is_error = m
                .tool_result
                .as_ref()
                .and_then(Value::as_object)
                .is_some_and(|result| result.contains_key("failure"));
            vec![m]
        }
        Some("result") => {
            let mut m = AgentMessage::of(AgentKind::Result);
            m.text = v.get("result").and_then(Value::as_str).map(str::to_string);
            m.session_id = sid;
            m.usage = v.get("usage").map(parse_usage);
            vec![m]
        }
        _ => vec![], // rate_limit_event 等
    }
}

/// cursor `tool_call` 对象里的那个工具:`(去掉 ToolCall 后缀的名字, 工具体)`。
///
/// 工具体按 `ToolCall` 后缀认,不能按位置取:同级还挂着 `hookAdditionalContexts`、`toolCallId`、
/// `startedAtMs` 这些元数据键,serde_json 的 Map 按键名排序后它们可能排在前面。认不出工具体
/// 就没有工具。
fn cursor_tool_entry(v: &Value) -> Option<(&str, &Value)> {
    v.get("tool_call")?
        .as_object()?
        .iter()
        .find_map(|(key, tool)| {
            key.strip_suffix("ToolCall")
                .filter(|name| !name.is_empty() && tool.is_object())
                .map(|name| (name, tool))
        })
}

/// cursor 工具的入参。`description` 去掉:description 是给 claude 那一档用的思考正文替身
/// (它自家模型的 thinking 为空),cursor 有自己的思考正文,再推一遍只是重复。
fn cursor_tool_args(tool: &Value) -> Option<Value> {
    let mut args = tool.get("args")?.clone();
    if let Some(args) = args.as_object_mut() {
        args.remove("description");
    }
    Some(args)
}

/// claude 转录的工具结果行顶层 `toolUseResult`:`status == "async_launched"` 是后台派出的
/// 回执(`isAsync` 与它同在;workflow 形态只有 status)。stream-json 的 stdout 行没有这个字段。
fn async_launch(v: &Value) -> Option<crate::parse::agent_msg::AsyncLaunch> {
    let result = v.get("toolUseResult")?;
    let launched = result.get("status").and_then(Value::as_str) == Some("async_launched")
        || result.get("isAsync").and_then(Value::as_bool) == Some(true);
    launched.then(|| crate::parse::agent_msg::AsyncLaunch {
        agent_id: result
            .get("agentId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
    })
}

fn content_blocks(v: &Value) -> Vec<Value> {
    v.get("message")
        .and_then(|msg| msg.get("content"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn assistant_block(
    b: &Value,
    sid: Option<String>,
    stop_reason: Option<String>,
    api_error: Option<String>,
    partial: bool,
) -> Option<AgentMessage> {
    match b.get("type").and_then(Value::as_str)? {
        "text" => {
            let mut m = AgentMessage::of(AgentKind::Assistant);
            m.text = b.get("text").and_then(Value::as_str).map(str::to_string);
            m.session_id = sid;
            m.stop_reason = stop_reason;
            m.error = api_error;
            m.partial = partial;
            Some(m)
        }
        "thinking" => {
            let mut m = AgentMessage::of(AgentKind::Thinking);
            m.text = b
                .get("thinking")
                .and_then(Value::as_str)
                .map(str::to_string);
            m.session_id = sid;
            m.stop_reason = stop_reason;
            m.error = api_error;
            Some(m)
        }
        "tool_use" => {
            let mut m = AgentMessage::of(AgentKind::ToolUse);
            m.tool = b.get("name").and_then(Value::as_str).map(str::to_string);
            m.tool_id = b.get("id").and_then(Value::as_str).map(str::to_string);
            // input 是任意 JSON 对象,原样保留供调用方展示 `$ git status` 等
            m.tool_input = b.get("input").cloned();
            m.session_id = sid;
            m.stop_reason = stop_reason;
            m.error = api_error;
            Some(m)
        }
        _ => None,
    }
}

fn tool_result_block(b: &Value, sid: Option<String>) -> Option<AgentMessage> {
    if b.get("type").and_then(Value::as_str)? == "tool_result" {
        let mut m = AgentMessage::of(AgentKind::ToolResult);
        m.session_id = sid;
        // content 是字符串或数组,统一包成 JSON Value 存储
        m.tool_result = b.get("content").cloned();
        // tool_use_id 配对 tool_use(Transcript 据此认后台 Bash 的失败回执);is_error 标记
        // 工具执行失败(后台 Bash 启动失败时不会有 task-notification,必须靠它递减计数)。
        m.tool_id = b
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        m.tool_is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        Some(m)
    } else {
        None
    }
}

/// 解析 `codex exec --json` 的 stdout 逐行 JSONL。schema 与 claude/cursor 不同:
/// - `thread.started`(带 `thread_id` = 可 resume 的 session id)→ System;
/// - `item.started` 且 `item.type` 为工具类(command_execution/file_change 等)→ ToolUse(**运行中**
///   信号,驱动「🔧 执行 X 中…」指示);agent_message/reasoning 的 started 无文本、忽略;
/// - `item.completed` 且 `item.type=="agent_message"`(助手回复,text 在 item.text)→ Assistant;
/// - `item.completed` 且 `item.type=="reasoning"`(推理摘要,text 在 item.text)→ Thinking;
/// - `item.completed` 的工具类 item.type → ToolResult(**结束**信号,让指示消失);
/// - `turn.completed`(带 usage)→ Result,但**不带 text**:codex 没有独立的「最终回复」字段,
///   最终回复就是最后一条 agent_message,故让 runner 用累积的最后 Assistant 文本回退即得正确终态。
///
/// > Step 0 实测(codex exec --json,2026-06):工具类 item 先发 `item.started`、后发 `item.completed`,
/// > 给出准确的 运行→结束 窗口;故 started=ToolUse / completed=ToolResult,而非旧的「completed=ToolUse」。
///
/// 未知/坏行 → 空 vec。
pub fn parse_codex_line(line: &str) -> Vec<AgentMessage> {
    let line = line.trim();
    if line.is_empty() {
        return vec![];
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![], // 坏行容错:跳过
    };
    match v.get("type").and_then(Value::as_str) {
        Some("thread.started") => {
            let mut m = AgentMessage::of(AgentKind::System);
            m.session_id = v
                .get("thread_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            vec![m]
        }
        // 工具类 item 开始 → ToolUse(运行中信号)。agent_message/reasoning 的 started 无文本 → 忽略,
        // 文本仍由各自 item.completed 产出(否则会渲出空块 / 重复)。
        Some("item.started") => match v.get("item") {
            Some(item)
                if item
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !is_codex_text_item(t)) =>
            {
                let mut m = AgentMessage::of(AgentKind::ToolUse);
                m.tool = item.get("type").and_then(Value::as_str).map(str::to_string);
                m.tool_id = item.get("id").and_then(Value::as_str).map(str::to_string);
                // 带出明细供卡片渲具体命令(command_execution 形如 "/bin/zsh -lc '<cmd>'")。
                m.tool_input = item.get("command").cloned();
                vec![m]
            }
            _ => vec![],
        },
        Some("item.completed") => {
            let Some(item) = v.get("item") else {
                return vec![];
            };
            match item.get("type").and_then(Value::as_str) {
                Some("agent_message") => {
                    let mut m = AgentMessage::of(AgentKind::Assistant);
                    m.text = item.get("text").and_then(Value::as_str).map(str::to_string);
                    vec![m]
                }
                // reasoning 摘要:exec 把 summary.join 放进 ReasoningItem.text(字段名与 agent_message
                // 同为 text,只能靠 item.type 区分)。映射成 Thinking 才能进双卡折叠的「思考过程」。
                Some("reasoning") => {
                    let mut m = AgentMessage::of(AgentKind::Thinking);
                    m.text = item.get("text").and_then(Value::as_str).map(str::to_string);
                    vec![m]
                }
                // 工具类 item 完成 → ToolResult(结束信号,与 started 的 ToolUse 配对、关掉运行指示)。
                Some(_) => {
                    let mut m = AgentMessage::of(AgentKind::ToolResult);
                    m.tool_id = item.get("id").and_then(Value::as_str).map(str::to_string);
                    m.tool_result = ["aggregated_output", "output", "stdout"]
                        .iter()
                        .find_map(|key| item.get(key).filter(|value| !value.is_null()).cloned());
                    m.tool_is_error = item
                        .get("exit_code")
                        .and_then(Value::as_i64)
                        .is_some_and(|code| code != 0);
                    vec![m]
                }
                None => vec![],
            }
        }
        Some("turn.completed") => {
            let mut m = AgentMessage::of(AgentKind::Result);
            // codex 的 usage 用 snake_case(input_tokens/output_tokens);text 留空让 runner 回退
            m.usage = v.get("usage").map(parse_usage);
            vec![m]
        }
        _ => vec![], // turn.started 等噪音忽略
    }
}

/// codex item.type 是否是「带文本的内容项」(agent_message=回复 / reasoning=思考)。它们的 started
/// 无文本要忽略;其它(command_execution/file_change 等)是工具项,started=运行中、completed=结束。
fn is_codex_text_item(item_type: &str) -> bool {
    matches!(item_type, "agent_message" | "reasoning")
}

/// usage 字段同时兼容 claude(snake_case)、cursor(camelCase)、codex(cached_input_tokens)。
fn parse_usage(u: &Value) -> Usage {
    // claude 用 input_tokens/output_tokens;cursor 用 inputTokens/outputTokens
    let input_tokens = u
        .get("input_tokens")
        .or_else(|| u.get("inputTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = u
        .get("output_tokens")
        .or_else(|| u.get("outputTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // claude: cache_read_input_tokens / cache_creation_input_tokens
    // cursor: cacheReadTokens / cacheWriteTokens
    // codex:  cached_input_tokens(读缓存),无写缓存字段
    let cache_read = u
        .get("cache_read_input_tokens")
        .or_else(|| u.get("cacheReadTokens"))
        .or_else(|| u.get("cached_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = u
        .get("cache_creation_input_tokens")
        .or_else(|| u.get("cacheWriteTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens,
        output_tokens,
        cache_read,
        cache_write,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::agent_msg::{RunId, TurnStatus};
    use std::time::SystemTime;

    const PONG: &str = include_str!("../../tests/fixtures/claude-stream-pong.jsonl");

    #[test]
    fn pong_stream_yields_session_id_and_result_text() {
        let msgs: Vec<AgentMessage> = PONG.lines().flat_map(parse_stream_json_line).collect();
        // init 事件带出 session_id
        let sid = msgs.iter().find_map(|m| m.session_id.clone());
        assert_eq!(sid.as_deref(), Some("07778859-d337-4a01-a41d-e4c11ae7be81"));
        // 至少一个 assistant 文本块 "pong"
        assert!(msgs
            .iter()
            .any(|m| m.kind == AgentKind::Assistant && m.text.as_deref() == Some("pong")));
        // result 事件是最终交付物
        let result = msgs
            .iter()
            .find(|m| m.kind == AgentKind::Result)
            .expect("有 result");
        assert_eq!(result.text.as_deref(), Some("pong"));
        assert_eq!(result.usage.as_ref().map(|u| u.output_tokens), Some(4));
        assert_eq!(result.usage.as_ref().map(|u| u.cache_read), Some(0));
        assert_eq!(result.usage.as_ref().map(|u| u.cache_write), Some(0));
        assert_eq!(result.usage.as_ref().map(|u| u.input_tokens), Some(15274));
    }

    #[test]
    fn skips_hook_noise_and_bad_lines() {
        // system/hook_* 与非 JSON 行都不产出事件
        assert!(parse_stream_json_line(
            r#"{"type":"system","subtype":"hook_started","session_id":"x"}"#
        )
        .is_empty());
        assert!(parse_stream_json_line("not json at all").is_empty());
        assert!(parse_stream_json_line(r#"{"type":"rate_limit_event"}"#).is_empty());
    }

    const TOOLUSE: &str = include_str!("../../tests/fixtures/claude-stream-tooluse.jsonl");

    #[test]
    fn tooluse_stream_yields_tool_events_and_final_result() {
        let msgs: Vec<AgentMessage> = TOOLUSE.lines().flat_map(parse_stream_json_line).collect();
        // 有一个 Bash 工具调用
        assert!(msgs
            .iter()
            .any(|m| m.kind == AgentKind::ToolUse && m.tool.as_deref() == Some("Bash")));
        // 有一个工具结果
        assert!(msgs.iter().any(|m| m.kind == AgentKind::ToolResult));
        // 中间 assistant 文本块被保留
        assert!(msgs.iter().any(|m| m.kind == AgentKind::Assistant
            && m.text.as_deref() == Some("Let me check the file.")));
        // 终态 result
        let r = msgs.iter().find(|m| m.kind == AgentKind::Result).unwrap();
        assert_eq!(r.text.as_deref(), Some("There are 2 files."));
        // tool_use 块应填 tool_id("tu_1")和 tool_input({"command":"ls"})
        let tu = msgs
            .iter()
            .find(|m| m.kind == AgentKind::ToolUse && m.tool.as_deref() == Some("Bash"))
            .expect("有 Bash tool_use");
        assert_eq!(tu.tool_id.as_deref(), Some("tu_1"));
        assert_eq!(
            tu.tool_input
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str()),
            Some("ls")
        );
        // tool_result 块应填 tool_result content
        let tr = msgs
            .iter()
            .find(|m| m.kind == AgentKind::ToolResult)
            .expect("有 tool_result");
        // fixture content 是字符串 "a.txt\nb.txt"
        assert!(tr.tool_result.is_some());
    }

    /// claude 2.1.274 的 `Agent` 调用默认异步:工具结果几毫秒内就到,内容只是「已派到后台」。
    /// 这类结果不是子 agent 的产出,顶层 `toolUseResult` 的启动标记与 agentId 要带出来。
    /// 形状取自真机主转录(正文已截短)。
    #[test]
    fn async_agent_launch_result_carries_the_launch_marker() {
        let launched = r#"{"parentUuid":"6c41","isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01GU","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully.\nagentId: a1c7b6848c7434fad (internal ID - do not mention to user.)"}]}]},"uuid":"4b25","timestamp":"2026-09-17T08:56:02.110Z","toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a1c7b6848c7434fad","description":"审一下","resolvedModel":"claude-opus-5","prompt":"p","outputFile":"/tmp/a.output","canReadOutputFile":false}}"#;
        let msgs = parse_stream_json_line(launched);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].tool_id.as_deref(), Some("toolu_01GU"));
        assert_eq!(
            msgs[0].async_launch,
            Some(crate::parse::agent_msg::AsyncLaunch {
                agent_id: Some("a1c7b6848c7434fad".into())
            })
        );

        // 同步完成与 TaskOutput 的结果都是任务的产出,不带启动标记。
        let sync_done = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_sync","type":"tool_result","content":[{"type":"text","text":"入口在 main.rs"}]}]},"toolUseResult":{"status":"completed","agentId":"a9","description":"找入口"}}"#;
        let task_output = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"call_eylk","type":"tool_result","content":"<retrieval_status>timeout</retrieval_status>"}]},"toolUseResult":{"retrieval_status":"timeout","task":{"task_id":"b8bm","task_type":"local_bash","status":"running"}}}"#;
        for line in [sync_done, task_output] {
            let msgs = parse_stream_json_line(line);
            assert_eq!(msgs.len(), 1, "{line}");
            assert_eq!(msgs[0].async_launch, None, "{line}");
        }
    }

    const CURSOR_PONG: &str = include_str!("../../tests/fixtures/cursor-stream-pong.jsonl");

    #[test]
    fn cursor_stream_pong_parses() {
        let msgs: Vec<AgentMessage> = CURSOR_PONG
            .lines()
            .flat_map(parse_stream_json_line)
            .collect();
        // session_id 来自 init 事件
        let sid = msgs.iter().find_map(|m| m.session_id.clone());
        assert_eq!(sid.as_deref(), Some("0d799a9c-ed4e-4e9e-a56e-ac0d8e37ff67"));
        // 至少一个 assistant 文本块 "pong"
        assert!(msgs
            .iter()
            .any(|m| m.kind == AgentKind::Assistant && m.text.as_deref() == Some("pong")));
        // result 事件(cursor 用 camelCase usage)
        let result = msgs
            .iter()
            .find(|m| m.kind == AgentKind::Result)
            .expect("有 result");
        assert_eq!(result.text.as_deref(), Some("pong"));
        // outputTokens(camelCase)应被正确解析为 4
        assert_eq!(result.usage.as_ref().map(|u| u.output_tokens), Some(4));
        assert_eq!(result.usage.as_ref().map(|u| u.cache_read), Some(0));
        assert_eq!(result.usage.as_ref().map(|u| u.cache_write), Some(56996));
    }

    /// cursor 真机(2026.09.10)的 `tool_call` 对象:工具体旁边并排挂着元数据键,按键名排序后
    /// `hookAdditionalContexts` 排在 `readToolCall` / `shellToolCall` 前面。
    #[test]
    fn cursor_tool_call_is_found_beside_its_metadata_keys() {
        let started = r#"{"type":"tool_call","subtype":"started","call_id":"c1","tool_call":{"shellToolCall":{"args":{"command":"echo summon-probe","workingDirectory":"","description":"Echo summon-probe exactly"},"description":"Echo summon-probe exactly"},"hookAdditionalContexts":[],"toolCallId":"c1","startedAtMs":"1789637364863"},"session_id":"s1"}"#;
        let msgs = parse_stream_json_line(started);
        assert_eq!(msgs[0].tool.as_deref(), Some("shell"));
        assert_eq!(
            msgs[0].tool_input,
            Some(serde_json::json!({"command":"echo summon-probe","workingDirectory":""})),
            "description 不随入参下传"
        );

        let unknown = |subtype: &str| {
            parse_stream_json_line(&format!(
                r#"{{"type":"tool_call","subtype":"{subtype}","call_id":"c3","tool_call":{{"hookAdditionalContexts":[{{"hookEventName":"x","content":"y"}}],"toolCallId":"c3","extra":{{"args":{{}},"result":{{}}}}}},"session_id":"s1"}}"#
            ))
        };
        let started = unknown("started");
        assert_eq!(started.len(), 1, "认不出工具体的 started 仍是段边界");
        assert_eq!(started[0].kind, AgentKind::ToolUse);
        assert_eq!(started[0].tool, None);
        assert_eq!(started[0].tool_input, None);
        assert!(
            unknown("completed").is_empty(),
            "认不出工具体的 completed 不产事件"
        );

        let completed = r#"{"type":"tool_call","subtype":"completed","call_id":"c2","tool_call":{"readToolCall":{"args":{"path":"/tmp/note.txt"},"result":{"success":{"content":"hi"}}},"hookAdditionalContexts":[],"toolCallId":"c2","startedAtMs":"1","completedAtMs":"2"},"session_id":"s1"}"#;
        let msgs = parse_stream_json_line(completed);
        assert_eq!(
            msgs[0].tool_result.as_ref(),
            Some(&serde_json::json!({"success":{"content":"hi"}}))
        );
    }

    #[test]
    fn cursor_tool_call_started_yields_tooluse() {
        // cursor 真机用顶层 `type:"tool_call"` 报工具(不像 claude 的 assistant.tool_use 块)。
        // 只认 subtype=="started"(段边界 + 活动一次),工具名取 `*ToolCall` 键去掉后缀。
        let started = r#"{"type":"tool_call","subtype":"started","call_id":"toolu_1","tool_call":{"shellToolCall":{"args":{"command":"echo hi"}}},"session_id":"s1"}"#;
        let msgs = parse_stream_json_line(started);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, AgentKind::ToolUse);
        assert_eq!(msgs[0].tool.as_deref(), Some("shell"));
        assert_eq!(msgs[0].tool_id.as_deref(), Some("toolu_1"));
        // completed 带回结果并与 started 的 call_id 配对。
        let completed = r#"{"type":"tool_call","subtype":"completed","call_id":"toolu_1","tool_call":{"shellToolCall":{}},"session_id":"s1"}"#;
        let completed_msgs = parse_stream_json_line(completed);
        assert_eq!(completed_msgs.len(), 1);
        assert_eq!(completed_msgs[0].kind, AgentKind::ToolResult);
        assert_eq!(completed_msgs[0].tool_id.as_deref(), Some("toolu_1"));
    }

    #[test]
    fn cursor_thinking_delta_yields_thinking_event() {
        // cursor 用顶层 `type:"thinking"` + `subtype:"delta"` 发 thinking 增量(不像 claude 嵌在
        // assistant.content 里)。带 timestamp_ms → partial=true(与 assistant delta 同语义)。
        let delta = r#"{"type":"thinking","subtype":"delta","text":"分析中","session_id":"s1","timestamp_ms":1783323074767}"#;
        let msgs = parse_stream_json_line(delta);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, AgentKind::Thinking);
        assert_eq!(msgs[0].text.as_deref(), Some("分析中"));
        assert!(
            msgs[0].partial,
            "带 timestamp_ms 的 thinking delta 应标 partial=true"
        );
        assert_eq!(msgs[0].session_id.as_deref(), Some("s1"));
        // completed 子类型无文本 → 忽略(段边界由后续 assistant/tool 自然分隔)
        let completed = r#"{"type":"thinking","subtype":"completed","session_id":"s1","timestamp_ms":1783323075063}"#;
        assert!(parse_stream_json_line(completed).is_empty());
        // 空文本 delta → 忽略
        let empty = r#"{"type":"thinking","subtype":"delta","text":"","session_id":"s1"}"#;
        assert!(parse_stream_json_line(empty).is_empty());
    }

    const CODEX_PONG: &str = include_str!("../../tests/fixtures/codex-exec-pong.jsonl");

    #[test]
    fn codex_pong_stream_yields_thread_id_and_assistant() {
        let msgs: Vec<AgentMessage> = CODEX_PONG.lines().flat_map(parse_codex_line).collect();
        // thread.started 带出 session_id(= thread_id)
        let sid = msgs.iter().find_map(|m| m.session_id.clone());
        assert_eq!(sid.as_deref(), Some("019e88c7-9226-76a3-b034-caa151b4909a"));
        // agent_message → Assistant 文本 "pong"
        assert!(msgs
            .iter()
            .any(|m| m.kind == AgentKind::Assistant && m.text.as_deref() == Some("pong")));
        // turn.completed → Result 带 usage(text 留空,由 runner 回退到最后 assistant)
        let result = msgs
            .iter()
            .find(|m| m.kind == AgentKind::Result)
            .expect("有 result");
        assert_eq!(result.text, None);
        assert_eq!(result.usage.as_ref().map(|u| u.output_tokens), Some(21));
        assert_eq!(result.usage.as_ref().map(|u| u.cache_read), Some(3456));
        assert_eq!(result.usage.as_ref().map(|u| u.cache_write), Some(0));
    }

    const CODEX_TOOLUSE: &str = include_str!("../../tests/fixtures/codex-exec-tooluse.jsonl");

    #[test]
    fn codex_tooluse_stream_yields_tool_and_assistant() {
        let msgs: Vec<AgentMessage> = CODEX_TOOLUSE.lines().flat_map(parse_codex_line).collect();
        // session id 来自 thread.started
        let sid = msgs.iter().find_map(|m| m.session_id.clone());
        assert_eq!(sid.as_deref(), Some("tid-2"));
        // command_execution 的 item.started → ToolUse(运行中),tool 名是 item.type 字符串
        assert!(msgs.iter().any(
            |m| m.kind == AgentKind::ToolUse && m.tool.as_deref() == Some("command_execution")
        ));
        // 同工具的 item.completed → ToolResult(结束信号)
        assert!(msgs.iter().any(|m| m.kind == AgentKind::ToolResult));
        // 最终 agent_message → Assistant
        assert!(msgs
            .iter()
            .any(|m| m.kind == AgentKind::Assistant
                && m.text.as_deref() == Some("There are 2 files.")));
    }

    #[test]
    fn codex_tool_item_started_is_tooluse_completed_is_toolresult() {
        // Step 0 实测(codex exec --json,2026-06):工具类 item 先 `item.started` 后 `item.completed`。
        // 故 started → ToolUse(运行中信号,驱动「🔧 执行 X 中…」指示),completed → ToolResult(结束信号,
        // 让指示消失)。agent_message/reasoning 仍只在 completed 出 Assistant/Thinking。
        let started = r#"{"type":"item.started","item":{"id":"i1","type":"command_execution"}}"#;
        let started_msgs = parse_codex_line(started);
        assert_eq!(started_msgs.len(), 1);
        assert_eq!(started_msgs[0].kind, AgentKind::ToolUse);
        assert_eq!(started_msgs[0].tool.as_deref(), Some("command_execution"));

        let completed =
            r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution"}}"#;
        let completed_msgs = parse_codex_line(completed);
        assert_eq!(completed_msgs.len(), 1);
        assert_eq!(
            completed_msgs[0].kind,
            AgentKind::ToolResult,
            "工具类 item.completed 是结束信号(ToolResult),不再产 ToolUse"
        );
    }

    #[test]
    fn codex_item_started_ignored_for_agent_message_and_reasoning() {
        // started 只对工具类 item 有意义;agent_message/reasoning 的 started 无文本、忽略,
        // 文本仍由各自的 item.completed 产出(否则会重复 / 渲出空块)。
        assert!(parse_codex_line(
            r#"{"type":"item.started","item":{"id":"a","type":"agent_message"}}"#
        )
        .is_empty());
        assert!(parse_codex_line(
            r#"{"type":"item.started","item":{"id":"r","type":"reasoning"}}"#
        )
        .is_empty());
    }

    #[test]
    fn codex_tool_started_captures_command_detail() {
        // item.started 的工具类 item 带 command 字段 → 落进 tool_input,供卡片渲具体命令。
        let line = r#"{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"/bin/zsh -lc 'echo hi'"}}"#;
        let msgs = parse_codex_line(line);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, AgentKind::ToolUse);
        assert_eq!(msgs[0].tool.as_deref(), Some("command_execution"));
        assert_eq!(
            msgs[0].tool_input.as_ref().and_then(|v| v.as_str()),
            Some("/bin/zsh -lc 'echo hi'")
        );
    }

    #[test]
    fn codex_reasoning_item_maps_to_thinking() {
        // 源码确认(openai/codex v0.139,exec/src/exec_events.rs):codex exec --json 把 reasoning
        // 摘要放进 item.completed 的 `ReasoningItem { text }`——字段名与 agent_message 同为 `text`,
        // 只能靠 item.type 判别符区分。必须映射成 Thinking(而非 ToolUse),否则双卡收尾抓不到
        // 思考过程(process 恒空 → 退化单卡)。reasoning 只在 item.completed 出现、无流式 delta。
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"先看 X 再看 Y"}}"#;
        let msgs = parse_codex_line(line);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, AgentKind::Thinking);
        assert_eq!(msgs[0].text.as_deref(), Some("先看 X 再看 Y"));
    }

    #[test]
    fn codex_skips_noise_and_bad_lines() {
        // turn.started 与坏行都不产出事件
        assert!(parse_codex_line(r#"{"type":"turn.started"}"#).is_empty());
        assert!(parse_codex_line("not json").is_empty());
        // item.completed 缺 item 字段也不崩
        assert!(parse_codex_line(r#"{"type":"item.completed"}"#).is_empty());
    }

    #[test]
    fn agent_message_of_has_r1_defaults() {
        // 验证 of() 产出的新字段全为 None/默认,runner 不受影响
        let m = AgentMessage::of(AgentKind::Assistant);
        assert_eq!(m.run_id, RunId::default());
        assert_eq!(m.ts, SystemTime::UNIX_EPOCH);
        assert_eq!(m.status, None::<TurnStatus>);
        assert_eq!(m.parent_run_id, None);
        assert_eq!(m.tool_id, None);
        assert_eq!(m.tool_input, None);
        assert_eq!(m.tool_result, None);
        assert_eq!(m.stop_reason, None);
        assert_eq!(m.error, None);
    }

    #[test]
    fn assistant_api_error_fills_error_field() {
        // 模拟 Claude API 错误(model_not_found 404):JSONL 的 assistant 消息带
        // `message.error` + `isApiErrorMessage` + `apiErrorStatus`,stop_reason=stop_sequence。
        // 解析器应把 error 提取到 AgentMessage.error,供上层立即终止本轮而非等 stall 超时。
        let line = r#"{"type":"assistant","session_id":"s1","message":{"id":"m1","role":"assistant","stop_reason":"stop_sequence","error":"model_not_found","isApiErrorMessage":true,"apiErrorStatus":404,"content":[{"type":"text","text":"There's an issue with the selected model."}]}}"#;
        let msgs = parse_stream_json_line(line);
        assert_eq!(msgs.len(), 1, "应产出一条 assistant 消息");
        assert_eq!(msgs[0].kind, AgentKind::Assistant);
        assert_eq!(
            msgs[0].error.as_deref(),
            Some("model_not_found"),
            "message.error 应被提取"
        );
        assert_eq!(
            msgs[0].stop_reason.as_deref(),
            Some("stop_sequence"),
            "stop_reason 仍正常解析"
        );
        assert_eq!(
            msgs[0].text.as_deref(),
            Some("There's an issue with the selected model."),
            "错误文本也应作为 text 被提取(作为回复发出)"
        );
    }

    #[test]
    fn assistant_without_error_leaves_error_none() {
        // 正常 assistant 消息不带 error 字段,AgentMessage.error 应为 None。
        let line = r#"{"type":"assistant","session_id":"s1","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"hello"}]}}"#;
        let msgs = parse_stream_json_line(line);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].error, None);
        assert_eq!(msgs[0].text.as_deref(), Some("hello"));
    }
}
