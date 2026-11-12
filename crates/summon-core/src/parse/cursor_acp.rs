//! cursor ACP(`cursor-agent acp`,Agent Client Protocol v1)的 `session/update` 通知模型 +
//! 到 `AgentMessage` 的映射。纯数据 / 纯函数,零 IO —— 进程编排、握手、ServerRequest 应答留在
//! `summon` 侧的客户端。
//!
//! 与 `stream_json::parse_stream_json_line`(`cursor -p --output-format stream-json
//! --stream-partial-output`)是**同一语义的两条通道**:同一轮对话经两条路解析出的 `AgentMessage`
//! 序列必须语义等价(kind 序列 / 拼接正文 / 工具名)。理由同 codex 侧(见 `codex_rpc` 模块头):
//! (`runtime.capture: cli` 曾是 ACP 路的通道切换项,2026-08-20 与 `CliSession` 一起删;两条路语义一漂，
//! 同一个 agent 切一下捕获模式卡片
//! 形态就变了。本文件末尾的一致性测试拿三对 fixture 逐项对照钉死这条不变量。
//!
//! **与 codex RPC 的结构性差异:一轮的终态不在通知流里,而是 `session/prompt` 的响应**
//! (`{"stopReason":"end_turn"}`)。所以终态事件由 [`prompt_stop_to_agent_messages`] 单独产出,
//! 而不是某条通知的映射结果。

use crate::parse::agent_msg::{AgentKind, AgentMessage};
use serde_json::Value;

/// 一条 `session/update` 通知携带的更新种类。
///
/// 形状取自本机实测(cursor-agent 2026.07.23):`agent_message_chunk` / `agent_thought_chunk` /
/// `tool_call` / `tool_call_update` / `user_message_chunk` / `session_info_update` /
/// `available_commands_update` / `current_mode_update` / `plan`。未知种类归 [`Other`] 而不是解析
/// 失败 —— 协议仍在演进,多一个 UI 事件不该把一轮对话拆流。
///
/// [`Other`]: CursorAcpUpdate::Other
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorAcpUpdate {
    /// 助手正文增量。**是真 delta 不是累积快照**(实测逐块互不包含),流式打字机的来源。
    AgentMessageChunk(String),
    /// 思考增量。同为真 delta。
    AgentThoughtChunk(String),
    /// 用户消息回显。只在 `session/load` 的历史重放里出现(见客户端的重放丢弃)。
    UserMessageChunk(String),
    /// 工具开始。`tool` 已归一到 exec 流的工具名(见 [`tool_name_from_kind`])。
    ToolCall {
        tool: String,
        tool_id: Option<String>,
        input: Option<Value>,
    },
    /// 工具状态更新(`in_progress` / `completed` / `failed`)。
    ToolCallUpdate {
        tool_id: Option<String>,
        status: Option<String>,
        result: Option<Value>,
    },
    /// ACP `usage_update`：当前 session 的上下文占用与有效窗口。
    Usage {
        used_tokens: u64,
        window_tokens: u64,
    },
    /// 其它控制面更新(标题、可用命令、模式、计划……),不属于一轮对话的内容流。
    Other(String),
}

/// 一条来自 ACP server 的通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorAcpNotification {
    SessionUpdate {
        session_id: String,
        update: CursorAcpUpdate,
    },
    /// 非 `session/update` 的通知方法。协议演进的容忍位。
    Other { method: String },
}

/// 解析一条通知帧。**不会失败**:坏形状降级成 [`CursorAcpNotification::Other`] 或
/// `CursorAcpUpdate::Other`。与 codex 侧 `parse_notification` 返回 `Result` 的取舍不同,理由是
/// 两者的消费者不同:codex 那份还要服务额度快照与 5h 限额卡的判定(解错=判错额度),而 ACP 通知流
/// 纯粹是 UI 内容流,为一条读不懂的 UI 事件把整轮打成错误只会更糟。
///
/// 返回 `None` = 这一帧不是通知(没有 `method`)。
pub fn parse_notification(value: &Value) -> Option<CursorAcpNotification> {
    let method = value.get("method").and_then(Value::as_str)?;
    if method != "session/update" {
        return Some(CursorAcpNotification::Other {
            method: method.to_string(),
        });
    }
    let params = value.get("params");
    let session_id = params
        .and_then(|p| p.get("sessionId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let update = params
        .and_then(|p| p.get("update"))
        .map(parse_update)
        .unwrap_or_else(|| CursorAcpUpdate::Other(String::new()));
    Some(CursorAcpNotification::SessionUpdate { session_id, update })
}

fn parse_update(update: &Value) -> CursorAcpUpdate {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_id = update
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(str::to_string);
    match kind {
        "agent_message_chunk" => CursorAcpUpdate::AgentMessageChunk(content_text(update)),
        "agent_thought_chunk" => CursorAcpUpdate::AgentThoughtChunk(content_text(update)),
        "user_message_chunk" => CursorAcpUpdate::UserMessageChunk(content_text(update)),
        "tool_call" => CursorAcpUpdate::ToolCall {
            tool: tool_name_from_kind(update.get("kind").and_then(Value::as_str).unwrap_or("")),
            tool_id,
            input: update
                .get("rawInput")
                .filter(|value| !value.is_null())
                .cloned(),
        },
        "tool_call_update" => CursorAcpUpdate::ToolCallUpdate {
            tool_id,
            status: update
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            result: update
                .get("rawOutput")
                .or_else(|| update.get("content"))
                .filter(|value| !value.is_null())
                .cloned(),
        },
        "usage_update" => match (
            update.get("used").and_then(Value::as_u64),
            update.get("size").and_then(Value::as_u64),
        ) {
            (Some(used_tokens), Some(window_tokens)) if window_tokens > 0 => {
                CursorAcpUpdate::Usage {
                    used_tokens,
                    window_tokens,
                }
            }
            _ => CursorAcpUpdate::Other(kind.to_string()),
        },
        other => CursorAcpUpdate::Other(other.to_string()),
    }
}

/// 取 `update.content` 的文本。ACP 的 content block 与 prompt 侧同构(`{type,text}`),
/// 非文本块(image/audio)取不到文本 → 空串 → 上层不产事件。
fn content_text(update: &Value) -> String {
    update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// ACP 的 `toolCall.kind` 是一个**固定小枚举**(read/edit/delete/move/search/execute/think/
/// fetch/switch_mode/other),而 `cursor -p` 的 stream-json 报的是具体工具名(`shellToolCall`
/// → `shell`)。两边要归一,否则卡片上的中文标签(`segment::tool_label`)会随捕获模式漂移。
///
/// 只映射 `tool_label` 认识的那几个;其余原样透传(不丢信息,标签退化为英文原名 —— 与 exec 路
/// 遇到没见过的工具名时的行为一致)。
fn tool_name_from_kind(kind: &str) -> String {
    match kind {
        // exec 流里执行类工具叫 `shell`(`shellToolCall`),ACP 叫 `execute`。
        "execute" => "shell".to_string(),
        other => other.to_string(),
    }
}

/// 把一条 `session/update` 映射成 0..N 条 `AgentMessage`,语义对齐 `parse_stream_json_line`
/// 对 cursor 无头流的处置。
///
/// 「0 条」是常态:ACP 比 stream-json 多出一批**控制面**更新(标题、可用命令、模式、计划),
/// 它们在 exec 那条路上根本不存在,产事件反而会让两条流发散。逐条依据写在各分支上。
pub fn acp_update_to_agent_messages(update: &CursorAcpUpdate) -> Vec<AgentMessage> {
    match update {
        // 正文/思考增量:`partial=true` = 「同一段的 token 增量,须原地拼接」,消费方是
        // `segment::push_segment` / `subprocess::merge_segment`。与 cursor exec 的
        // `--stream-partial-output` delta 行(带 timestamp_ms → partial=true)逐条同语义。
        //
        // 例外:cursor-agent 偶发把**自己的传输层错误串**当 `agent_message_chunk` 吐出来
        // (stopReason 仍是正常 end_turn、不是协议错误),漏进去就成了给用户回复的尾巴。
        // 识别并滤掉(见 [`is_cursor_transport_error`]),只认已知噪音、绝不误伤真正文。
        CursorAcpUpdate::AgentMessageChunk(text) if is_cursor_transport_error(text) => vec![],
        CursorAcpUpdate::AgentMessageChunk(text) => text_event(AgentKind::Assistant, text),
        CursorAcpUpdate::AgentThoughtChunk(text) => text_event(AgentKind::Thinking, text),
        // 工具开始 → ToolUse:既触发工作表情/可观测,又给捕获层一个**段边界**(工具前的文本段
        // 到此定稿,不与工具后的新段拼成一坨)。与 exec 的 `tool_call{subtype:started}` 对齐。
        CursorAcpUpdate::ToolCall {
            tool,
            tool_id,
            input,
        } => {
            let mut m = AgentMessage::of(AgentKind::ToolUse);
            m.tool = Some(tool.clone());
            m.tool_id = tool_id.clone();
            m.tool_input = input.clone();
            vec![m]
        }
        CursorAcpUpdate::ToolCallUpdate {
            tool_id,
            status,
            result,
        } if status
            .as_deref()
            .is_some_and(|status| matches!(status, "completed" | "failed")) =>
        {
            let mut message = AgentMessage::of(AgentKind::ToolResult);
            message.tool_id = tool_id.clone();
            message.tool_result = result
                .clone()
                .or_else(|| status.as_ref().map(|status| Value::String(status.clone())));
            message.tool_is_error = status.as_deref() == Some("failed");
            vec![message]
        }
        CursorAcpUpdate::ToolCallUpdate { .. } => vec![],
        // 用户回显是 ACP 独有的(exec 流里没有)。不挡掉就会把用户自己的话当成助手正文渲出来 ——
        // `session/load` 的历史重放里全是它。
        CursorAcpUpdate::UserMessageChunk(_) => vec![],
        CursorAcpUpdate::Usage { .. } => vec![],
        CursorAcpUpdate::Other(_) => vec![],
    }
}

/// 一轮的终态。ACP 把它放在 `session/prompt` 的**响应**里而不是通知流里,所以它不是任何一条
/// 通知的映射结果,得单独产出。
///
/// 与 `parse_codex_line` 的 `turn.completed` 同款:**text 留空**,让 runner 回退到累积正文
/// (cursor exec 的 `result` 行带 `result` 文本,但那只是同一段正文的另一种送达方式,
/// 回退路径给出的结果逐字相同)。
///
/// 非 `end_turn` 的 stopReason(`cancelled` / `max_tokens` / `refusal` …)**不映射成 Error**:
/// ACP 把它们都算作正常完成(`cancelled` 就是用户自己按的 `!`),真正的失败会以 JSON-RPC error
/// 的形式出现在 prompt 响应上、由客户端直接报错。stop_reason 原样带出供观测。
pub fn prompt_stop_to_agent_messages(stop_reason: &str) -> Vec<AgentMessage> {
    let mut m = AgentMessage::of(AgentKind::Result);
    if !stop_reason.is_empty() {
        m.stop_reason = Some(stop_reason.to_string());
    }
    vec![m]
}

fn text_event(kind: AgentKind, text: &str) -> Vec<AgentMessage> {
    if text.is_empty() {
        return vec![]; // 空增量不产事件:空块会在卡上留空段
    }
    let mut m = AgentMessage::of(kind);
    m.text = Some(text.to_string());
    m.partial = true;
    vec![m]
}

/// 这一段助手正文是不是 cursor-agent 运行时**传输层错误串**(而非模型真回复)。
///
/// 现场:真机 cursor 回复末尾出现独立成段的 `Error: RetriableError: WritableIterable is
/// closed` —— 这是 cursor-agent 自己以 `agent_message_chunk` 吐的错误(stopReason 仍是正常
/// `end_turn`),不该漏进给用户的正文。
///
/// **判据刻意保守**,只认已知噪音,宁可漏滤也不误伤用户内容里真含「Error:」的正文:
/// - trim 后必须是**独立一行**(多行说明混着真正文,不碰);
/// - 以 `Error:` 起头;
/// - 且带 cursor 传输层错误标记(`RetriableError` / `WritableIterable is closed`)。
///
/// 三条同时满足才判噪音。用户正常写的「Error: 数组越界」这类(不带上述标记)不会命中。
pub fn is_cursor_transport_error(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with("Error:") {
        return false;
    }
    // 独立成段:一旦跨行,说明这一块里还夹着别的内容,滤整块就会连真正文一起吃掉。
    if trimmed.lines().count() != 1 {
        return false;
    }
    trimmed.contains("RetriableError") || trimmed.contains("WritableIterable is closed")
}

/// 把 agent.yaml 顶层 `model`(**exec 命名空间**名,如 `gpt-5.6-sol-high`)best-effort 映射到
/// cursor ACP 模型目录。旧目录把参数烤进方括号 id，新 parameterized 目录只给 base id。
///
/// **为什么要映射**:cursor RPC 只认 ACP 命名空间的 id;
/// 现网 cursor agent 多数只配了顶层 `model`、没配 `runtime.rpc_model`。不映射就退回
/// `~/.cursor/acp-config.json` 的 auto-smart 自动路由(实测能路到 Grok),等于用户配的模型
/// **静默失效**。显式 `rpc_model` 仍优先(它就是 ACP id),这里只兜「只配了 exec 名」这一档。
///
/// 匹配规则(保守、确定性;按真机 `availableModels` 形态定):
/// - `normalize` = 小写后只留 `[a-z0-9]`(吃掉 `-`/`.`/方括号里的标点)。
/// - ① 某候选整串 normalize 后与 exec 名相等 → 用它(用户直接把 ACP id 填进了 `model`)。
/// - ② 候选 `<base>`(方括号前)与其 `reasoning=<r>` 拼接 normalize 后等于 exec 名 → 命中
///   (`gpt-5.6-sol` + `high` == `gpt-5.6-sol-high`)。这是主规则。
/// - ③ 恰好只有一个候选的 `<base>` normalize 后等于 exec 名(exec 名没带 effort 后缀、
///   该 base 仅一个变体)→ 用它。多于一个则不猜,交给 warn。
/// - ④ parameterized 候选没有方括号时，允许 exec 名在 base 后多一个 `-`/`_` 分隔的档位
///   后缀；多个候选命中时只取唯一最长 base，等长歧义拒绝。
///
/// 都不中 → `None`,调用方 warn(**不静默回落 auto**)。
pub fn match_cursor_model(exec_name: &str, available: &[String]) -> Option<String> {
    let target = normalize_model_token(exec_name);
    if target.is_empty() {
        return None;
    }
    // ① 候选整串 normalize 后完全相等(用户直接填了 ACP id)。
    if let Some(hit) = available
        .iter()
        .find(|cand| normalize_model_token(cand) == target)
    {
        return Some(hit.clone());
    }
    // ② base + reasoning;顺带记下「base 相等但没带 reasoning 后缀」的候选给 ③ 用。
    let mut base_only: Vec<&String> = Vec::new();
    for cand in available {
        let (base, reasoning) = split_acp_model(cand);
        let base_norm = normalize_model_token(base);
        if base_norm.is_empty() {
            continue;
        }
        if let Some(r) = reasoning {
            if format!("{base_norm}{}", normalize_model_token(r)) == target {
                return Some(cand.clone());
            }
        }
        if base_norm == target {
            base_only.push(cand);
        }
    }
    // ③ exec 名恰是某个 base(无 effort)、且该 base 唯一 → 用它;多变体则不猜。
    if let [only] = base_only.as_slice() {
        return Some((*only).clone());
    }

    // ④ parameterized 目录只有 base id，而 exec 名仍把 effort/thinking 等档位烤在尾部。
    // 只接受显式 token 边界，并选唯一最长 base；这能让 `gpt-5.6-sol` 胜过 `gpt-5.6`，
    // 同时避免 `gpt-5.6` 误匹配 `gpt-5.60-*`。
    let target_raw = exec_name.trim().to_ascii_lowercase();
    let mut best: Option<(&String, usize)> = None;
    let mut ambiguous = false;
    for candidate in available
        .iter()
        .filter(|candidate| !candidate.contains('['))
    {
        let base = candidate.trim().to_ascii_lowercase();
        let Some(suffix) = target_raw.strip_prefix(&base) else {
            continue;
        };
        if !(suffix.starts_with('-') || suffix.starts_with('_')) {
            continue;
        }
        match best {
            None => {
                best = Some((candidate, base.len()));
                ambiguous = false;
            }
            Some((_, length)) if base.len() > length => {
                best = Some((candidate, base.len()));
                ambiguous = false;
            }
            Some((_, length)) if base.len() == length => ambiguous = true,
            Some(_) => {}
        }
    }
    match (best, ambiguous) {
        (Some((candidate, _)), false) => Some(candidate.clone()),
        _ => None,
    }
}

/// 小写后只留 ascii 字母数字。跨命名空间比对的归一器:`gpt-5.6-sol-high` → `gpt56solhigh`。
fn normalize_model_token(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 拆 ACP 完整 id 成 `(base, reasoning)`。
/// `gpt-5.6-sol[context=272k,reasoning=high,fast=false]` → `("gpt-5.6-sol", Some("high"))`;
/// 无方括号 → `(整串, None)`。
fn split_acp_model(id: &str) -> (&str, Option<&str>) {
    match id.split_once('[') {
        Some((base, rest)) => {
            let reasoning = rest
                .trim_end_matches(']')
                .split(',')
                .find_map(|kv| kv.trim().strip_prefix("reasoning="))
                .map(str::trim);
            (base.trim(), reasoning)
        }
        None => (id.trim(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::segment::{push_segment, SegKind, TurnSegment};
    use crate::parse::stream_json::parse_stream_json_line;
    use serde_json::json;

    /// 把一份「客户端从 stdout 读到的行」的 fixture 折成事件序列。通知走映射,`session/prompt`
    /// 的响应帧走 [`prompt_stop_to_agent_messages`] —— 这正是引擎里两条来源的合流方式。
    fn acp_events(stream: &str) -> Vec<AgentMessage> {
        let events: Vec<AgentMessage> = stream
            .lines()
            .filter(|l| !l.trim().is_empty())
            .flat_map(|line| {
                let value: Value = serde_json::from_str(line).expect("fixture 必须是合法 JSON");
                match parse_notification(&value) {
                    Some(CursorAcpNotification::SessionUpdate { update, .. }) => {
                        acp_update_to_agent_messages(&update)
                    }
                    Some(CursorAcpNotification::Other { .. }) => vec![],
                    // 响应帧:一轮的终态。
                    None => value
                        .pointer("/result/stopReason")
                        .and_then(Value::as_str)
                        .map(prompt_stop_to_agent_messages)
                        .unwrap_or_default(),
                }
            })
            .collect();
        assert!(
            events.iter().all(|m| m.kind != AgentKind::System),
            "通知流不该产 System"
        );
        events
    }

    fn exec_events(stream: &str) -> Vec<AgentMessage> {
        stream.lines().flat_map(parse_stream_json_line).collect()
    }

    /// 语义骨架:折叠掉「连续同 kind 的增量」后的 kind 序列。流式切分粒度允许不同,骨架不许不同。
    fn kind_skeleton(msgs: &[AgentMessage]) -> Vec<AgentKind> {
        let mut out: Vec<AgentKind> = Vec::new();
        for m in msgs {
            // System 只在 exec 流里有(`system.init` 带 session_id);ACP 侧 sessionId 来自
            // `session/new` / `session/load` 的**响应**,不在通知流里 —— 通道差异,不是语义差异。
            if m.kind == AgentKind::System {
                continue;
            }
            if out.last() != Some(&m.kind) {
                out.push(m.kind.clone());
            }
        }
        out
    }

    /// 用**生产同款**的段模型累积正文:`push_segment` 就是 live 卡的累积器。
    fn segments(msgs: &[AgentMessage]) -> Vec<TurnSegment> {
        let mut segs = Vec::new();
        for m in msgs {
            let kind = match m.kind {
                AgentKind::Assistant => SegKind::Reply,
                AgentKind::Thinking => SegKind::Thinking,
                _ => continue,
            };
            if let Some(text) = &m.text {
                push_segment(&mut segs, kind, text, m.partial);
            }
        }
        segs
    }

    fn tools(msgs: &[AgentMessage]) -> Vec<String> {
        msgs.iter()
            .filter(|m| m.kind == AgentKind::ToolUse)
            .map(|m| m.tool.clone().unwrap_or_default())
            .collect()
    }

    const EXEC_PONG: &str = include_str!("../../tests/fixtures/cursor-stream-pong.jsonl");
    const ACP_PONG: &str = include_str!("../../tests/fixtures/cursor-acp-pong.jsonl");
    const EXEC_MULTISEG: &str =
        include_str!("../../tests/fixtures/cursor-stream-partial-multiseg.jsonl");
    const ACP_MULTISEG: &str =
        include_str!("../../tests/fixtures/cursor-acp-partial-multiseg.jsonl");
    const EXEC_TOOLUSE: &str = include_str!("../../tests/fixtures/cursor-stream-tool-dup.jsonl");
    const ACP_TOOLUSE: &str = include_str!("../../tests/fixtures/cursor-acp-tooluse.jsonl");

    /// 一致性断言(本文件存在的理由):同一语义的一轮,ACP 通知流与 `cursor -p --output-format
    /// stream-json` 必须解析出等价的事件序列。ACP fixture 按本机实测序列构造
    /// (cursor-agent 2026.07.23,见 §2 探针记录)。
    #[test]
    fn acp_pong_turn_matches_exec_pong_turn() {
        let exec = exec_events(EXEC_PONG);
        let acp = acp_events(ACP_PONG);
        assert_eq!(
            kind_skeleton(&acp),
            kind_skeleton(&exec),
            "kind 骨架必须一致(ACP={acp:?})"
        );
        assert_eq!(
            kind_skeleton(&exec),
            vec![AgentKind::Assistant, AgentKind::Result],
            "骨架本身也钉住:一段回复 + 一个终态"
        );
        assert_eq!(segments(&acp), segments(&exec), "拼接正文必须一致");
        assert_eq!(segments(&acp), vec![TurnSegment::Reply("pong".into())]);
    }

    /// 多段 delta:exec 侧混着「真 delta + 段末整段重发快照」,ACP 侧是纯 delta。两条流的
    /// 切分粒度不同,**折出的正文必须逐字相同** —— 这正是 `merge_delta` 那套去重规则的用武之地。
    #[test]
    fn acp_multi_delta_turn_matches_exec_partial_snapshots() {
        let exec = exec_events(EXEC_MULTISEG);
        let acp = acp_events(ACP_MULTISEG);
        assert_eq!(kind_skeleton(&acp), kind_skeleton(&exec));
        assert_eq!(
            segments(&acp),
            segments(&exec),
            "ACP 纯 delta 与 exec 的 delta+快照必须折出同一段正文"
        );
        assert_eq!(segments(&acp), vec![TurnSegment::Reply("AABBCCDD".into())]);
    }

    /// 带工具的一轮:工具是段边界,工具名要归一到 exec 的写法。
    #[test]
    fn acp_tool_turn_matches_exec_tool_turn() {
        let exec = exec_events(EXEC_TOOLUSE);
        let acp = acp_events(ACP_TOOLUSE);
        assert_eq!(
            kind_skeleton(&acp),
            kind_skeleton(&exec),
            "kind 骨架必须一致(ACP={acp:?})"
        );
        assert_eq!(
            kind_skeleton(&exec),
            vec![
                AgentKind::Assistant,
                AgentKind::ToolUse,
                AgentKind::ToolResult,
                AgentKind::Assistant,
                AgentKind::Result,
            ]
        );
        assert_eq!(segments(&acp), segments(&exec));
        assert_eq!(
            segments(&acp),
            vec![TurnSegment::Reply("Step one done.Step two done.".into())]
        );
        // 工具名归一:ACP 的 `kind:"execute"` 必须与 exec 的 `shellToolCall` → `shell` 同名,
        // 否则卡片上的中文标签会随捕获模式退回英文原名。
        assert_eq!(tools(&acp), tools(&exec));
        assert_eq!(tools(&acp), vec!["shell".to_string()]);
    }

    /// `session/load` 会把整段历史当通知重放(实测:user_message_chunk + tool_call +
    /// tool_call_update + agent_message_chunk,toolCallId 前缀 `replay-`)。用户回显必须
    /// **一条事件都不产**,否则重放会把用户自己的话渲成助手回复。
    #[test]
    fn replayed_user_message_chunk_produces_nothing() {
        let n = parse_notification(&json!({
            "method": "session/update",
            "params": {"sessionId":"s","update":{"sessionUpdate":"user_message_chunk",
                       "content":{"type":"text","text":"我刚才说的话"}}}
        }))
        .unwrap();
        let CursorAcpNotification::SessionUpdate { update, .. } = n else {
            panic!("应解析成 SessionUpdate");
        };
        assert_eq!(
            update,
            CursorAcpUpdate::UserMessageChunk("我刚才说的话".into())
        );
        assert!(acp_update_to_agent_messages(&update).is_empty());
    }

    #[test]
    fn control_plane_updates_produce_nothing() {
        let cases = [
            json!({"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"session_info_update","title":"T"}}}),
            json!({"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"available_commands_update","availableCommands":[]}}}),
            json!({"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"current_mode_update","currentModeId":"agent"}}}),
            json!({"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"plan","entries":[]}}}),
        ];
        for case in cases {
            let Some(CursorAcpNotification::SessionUpdate { update, .. }) =
                parse_notification(&case)
            else {
                panic!("应解析成 SessionUpdate: {case}");
            };
            assert!(
                acp_update_to_agent_messages(&update).is_empty(),
                "控制面更新不该产内容事件:{case}"
            );
        }
    }

    #[test]
    fn completed_tool_update_preserves_result_and_identity() {
        let n = parse_notification(&json!({
            "method":"session/update",
            "params":{"sessionId":"s","update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"c1",
                "status":"completed",
                "rawOutput":{"stdout":"ok\n","exitCode":0}
            }}
        }))
        .unwrap();
        let CursorAcpNotification::SessionUpdate { update, .. } = n else {
            panic!("应解析成 SessionUpdate");
        };
        let messages = acp_update_to_agent_messages(&update);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, AgentKind::ToolResult);
        assert_eq!(messages[0].tool_id.as_deref(), Some("c1"));
        assert_eq!(messages[0].tool_result.as_ref().unwrap()["stdout"], "ok\n");
        assert!(!messages[0].tool_is_error);
    }

    /// 工具 kind 的归一表。只有 `execute` 需要改名(exec 侧叫 shell);其余 ACP kind 与
    /// `tool_label` 的键同名,原样透传即可。
    #[test]
    fn tool_kinds_normalize_to_exec_tool_names() {
        for (acp_kind, exec_name) in [
            ("execute", "shell"),
            ("read", "read"),
            ("edit", "edit"),
            ("search", "search"),
            ("brand_new_kind", "brand_new_kind"),
        ] {
            assert_eq!(tool_name_from_kind(acp_kind), exec_name);
        }
        // 归一后的名字必须真的能被卡片标签认出来(否则归一等于白做)。
        assert_eq!(crate::messaging::segment::tool_label("shell"), "命令");
    }

    /// 未知通知方法 / 未知更新种类都不拆流(协议演进容忍位)。
    #[test]
    fn unknown_methods_and_updates_degrade_instead_of_failing() {
        assert_eq!(
            parse_notification(&json!({"method":"session/heartbeat","params":{}})),
            Some(CursorAcpNotification::Other {
                method: "session/heartbeat".into()
            })
        );
        let Some(CursorAcpNotification::SessionUpdate { update, .. }) = parse_notification(
            &json!({"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"brand_new"}}}),
        ) else {
            panic!("应解析成 SessionUpdate");
        };
        assert_eq!(update, CursorAcpUpdate::Other("brand_new".into()));
        assert!(acp_update_to_agent_messages(&update).is_empty());
        // 响应帧不是通知。
        assert_eq!(parse_notification(&json!({"id":1,"result":{}})), None);
    }

    #[test]
    fn usage_update_reports_current_context_tokens_and_window() {
        let frame = json!({
            "method": "session/update",
            "params": {
                "sessionId": "sess-1",
                "update": {"sessionUpdate": "usage_update", "used": 53_000, "size": 200_000}
            }
        });
        let Some(CursorAcpNotification::SessionUpdate { update, .. }) = parse_notification(&frame)
        else {
            panic!("usage_update 必须被识别")
        };
        assert_eq!(
            update,
            CursorAcpUpdate::Usage {
                used_tokens: 53_000,
                window_tokens: 200_000,
            }
        );
        assert!(acp_update_to_agent_messages(&update).is_empty());
    }

    /// 终态:text 留空(回退到累积正文),stop_reason 原样带出。
    #[test]
    fn prompt_stop_yields_a_textless_result_event() {
        let msgs = prompt_stop_to_agent_messages("end_turn");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, AgentKind::Result);
        assert_eq!(msgs[0].text, None, "终态不带文本,由累积正文回退");
        assert_eq!(msgs[0].stop_reason.as_deref(), Some("end_turn"));
        // 被打断也是正常终态(不是 Error):`cancelled` 就是用户自己按的 `!`。
        assert_eq!(
            prompt_stop_to_agent_messages("cancelled")[0].kind,
            AgentKind::Result
        );
    }

    /// cursor 传输层错误串(`Error: RetriableError: WritableIterable is closed`)当
    /// `agent_message_chunk` 吐出来时必须**滤掉、不进正文**;同一轮里真正的回复段照常保留。
    #[test]
    fn cursor_transport_error_chunk_is_filtered_but_real_reply_kept() {
        let stream = concat!(
            r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"真正的回复内容"}}}}"#,
            "\n",
            r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Error: RetriableError: WritableIterable is closed"}}}}"#,
            "\n",
            r#"{"id":1,"result":{"stopReason":"end_turn"}}"#,
        );
        let msgs = acp_events(stream);
        // 折出的正文只剩真回复,不含传输层错误串。
        assert_eq!(
            segments(&msgs),
            vec![TurnSegment::Reply("真正的回复内容".into())]
        );
        assert!(
            !msgs.iter().any(|m| m
                .text
                .as_deref()
                .is_some_and(|t| t.contains("RetriableError"))),
            "错误串不该以任何事件形式漏出"
        );
        // stopReason 仍是正常 end_turn(证明这不是协议错误,只是噪音串)。
        assert_eq!(msgs.last().map(|m| m.kind.clone()), Some(AgentKind::Result));
    }

    /// 前后带空行、独立成段的错误串同样被滤(真机形态是「正文之后空一行再来一行 Error:」)。
    #[test]
    fn standalone_error_chunk_with_surrounding_blank_lines_is_filtered() {
        assert!(
            acp_update_to_agent_messages(&CursorAcpUpdate::AgentMessageChunk(
                "\n\nError: RetriableError: WritableIterable is closed\n".into()
            ))
            .is_empty()
        );
    }

    /// 保守边界:用户正文里正常出现的「Error:」**不能**被误伤 —— 不带 cursor 传输层标记、
    /// 或跨多行、或只是正文的一部分,都照常当回复渲出。
    #[test]
    fn genuine_user_error_text_is_not_filtered() {
        for text in [
            "Error: 数组越界,请检查下标",                 // 不带 cursor 传输层标记
            "编译报错 Error: RetriableError 是什么意思?", // 不以 Error: 起头(问题里提到)
            "Error: 第一行\n第二行还有内容",              // 多行:夹着真正文
        ] {
            let msgs =
                acp_update_to_agent_messages(&CursorAcpUpdate::AgentMessageChunk(text.into()));
            assert_eq!(msgs.len(), 1, "用户正文不该被当噪音滤掉: {text:?}");
            assert_eq!(msgs[0].text.as_deref(), Some(text));
        }
    }

    /// 空文本增量不产事件(空块会在卡上留空段);非文本 content block 同理。
    #[test]
    fn empty_or_non_text_chunks_produce_nothing() {
        assert!(
            acp_update_to_agent_messages(&CursorAcpUpdate::AgentMessageChunk(String::new()))
                .is_empty()
        );
        let Some(CursorAcpNotification::SessionUpdate { update, .. }) = parse_notification(
            &json!({"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"image","data":"…"}}}}),
        ) else {
            panic!("应解析成 SessionUpdate");
        };
        assert!(acp_update_to_agent_messages(&update).is_empty());
    }

    /// 本机 availableModels 形态(方括号 + reasoning=）。
    fn sample_available() -> Vec<String> {
        vec![
            "gpt-5.6-sol[context=272k,reasoning=low,fast=false]".into(),
            "gpt-5.6-sol[context=272k,reasoning=medium,fast=false]".into(),
            "gpt-5.6-sol[context=272k,reasoning=high,fast=false]".into(),
            "composer-2.5[fast=true]".into(),
        ]
    }

    /// P0-B 主规则:exec 名 `gpt-5.6-sol-high` 应映射到带 `reasoning=high` 的那条完整 ACP id
    /// (而不是 low/medium),这样 set_model 收到的是规范化后的正确 id。
    #[test]
    fn exec_name_maps_to_matching_reasoning_candidate() {
        let av = sample_available();
        assert_eq!(
            match_cursor_model("gpt-5.6-sol-high", &av).as_deref(),
            Some("gpt-5.6-sol[context=272k,reasoning=high,fast=false]")
        );
        assert_eq!(
            match_cursor_model("gpt-5.6-sol-low", &av).as_deref(),
            Some("gpt-5.6-sol[context=272k,reasoning=low,fast=false]")
        );
    }

    /// 单变体 base:exec 名没带 effort 后缀、该 base 只有一个候选 → 命中(规则③)。
    #[test]
    fn exec_name_without_effort_maps_to_unique_base() {
        let av = sample_available();
        assert_eq!(
            match_cursor_model("composer-2.5", &av).as_deref(),
            Some("composer-2.5[fast=true]")
        );
    }

    /// parameterizedModelPicker 开启后目录只给 base id；exec 名仍可能带档位后缀。
    /// 必须选最长 base，避免 `gpt-5.6` 抢走更精确的 `gpt-5.6-sol`。
    #[test]
    fn exec_variant_maps_to_longest_parameterized_base() {
        let av = vec![
            "gpt-5.6".into(),
            "gpt-5.6-sol".into(),
            "composer-2.5".into(),
        ];
        assert_eq!(
            match_cursor_model("gpt-5.6-sol-xhigh", &av).as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            match_cursor_model("composer-2.5-high", &av).as_deref(),
            Some("composer-2.5")
        );
    }

    /// 前缀只能在 token 边界命中；相同最长长度的歧义候选必须拒绝，不能猜。
    #[test]
    fn parameterized_base_prefix_is_boundary_checked_and_unambiguous() {
        assert_eq!(
            match_cursor_model("gpt-5.60-high", &["gpt-5.6".into()]),
            None
        );
        assert_eq!(
            match_cursor_model("model-a-high", &["model-a".into(), "MODEL-A".into()]),
            None
        );
    }

    /// 用户直接把 ACP 完整 id 填进了顶层 `model` → 整串归一相等,原样命中(规则①)。
    #[test]
    fn exec_name_that_is_already_an_acp_id_matches_itself() {
        let av = sample_available();
        assert_eq!(
            match_cursor_model("gpt-5.6-sol[context=272k,reasoning=medium,fast=false]", &av)
                .as_deref(),
            Some("gpt-5.6-sol[context=272k,reasoning=medium,fast=false]")
        );
    }

    /// 匹配不到就返回 None(调用方据此 warn,而不是静默回落 auto):
    /// - 候选空(会话还没报 availableModels);
    /// - exec 名的 base 在候选里根本没有;
    /// - exec 名 == 某 base 但该 base 有多个 reasoning 变体、又没给 effort → 不猜。
    #[test]
    fn no_match_returns_none_instead_of_guessing() {
        let av = sample_available();
        assert_eq!(match_cursor_model("gpt-5.6-sol-high", &[]), None);
        assert_eq!(match_cursor_model("claude-opus-4.8", &av), None);
        // `gpt-5.6-sol` 有 low/medium/high 三个变体,没给 effort → 不唯一 → 不猜。
        assert_eq!(match_cursor_model("gpt-5.6-sol", &av), None);
        // 空 exec 名。
        assert_eq!(match_cursor_model("", &av), None);
    }
}
