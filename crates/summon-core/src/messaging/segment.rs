//! 一轮内 thinking/reply **按到达顺序**的有序段模型(纯件,无 IO)。
//!
//! live 卡从「只刷累积 reply」改成「交织渲染 thinking + reply」后,需要一个能保留二者相对顺序的
//! 段序列。`on_event` 是唯一能按到达顺序看到 thinking + assistant 的回调;每来一块就 `push_segment`。
//! 渲染器(messaging/feishu.rs)消费 `&[TurnSegment]` 出卡。

use std::collections::HashMap;

/// Codex app-server 一轮内所有 reasoning summary 标题共用的 UI 组。`SourceItemSegments` 本来就
/// 是 per-turn 状态，因此无需再把 turn id 编进 key。
pub const CODEX_REASONING_SUMMARY_GROUP: &str = "codex_rpc_reasoning_summaries";
pub const CODEX_REASONING_SUMMARY_SEPARATOR: &str = "\n";

/// 一段有序内容:要么是一块 thinking(分析/判断),要么是一段 reply(可见回复)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnSegment {
    Thinking(String),
    Reply(String),
}

/// push_segment 的入参类别(避免直接耦合 `AgentKind`,纯件自洽、好单测)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegKind {
    Thinking,
    Reply,
}

/// 一块内容进入段模型后的确定性判定。调用方可据此观测归并，不必复制正文或重写规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    IgnoredEmpty,
    Opened,
    Appended,
    Replaced,
    IgnoredDuplicate,
}

impl PushOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IgnoredEmpty => "ignored_empty",
            Self::Opened => "opened",
            Self::Appended => "appended",
            Self::Replaced => "replaced",
            Self::IgnoredDuplicate => "ignored_duplicate",
        }
    }
}

/// 带稳定 provider item id 的流式内容在段序列中的位置。
///
/// Codex RPC 明确区分同一 item 的 token delta 与 completed 权威快照；这类事件不能再借助文本
/// 前缀猜测边界。索引只在一轮内存活，`TurnSegment` 不需要承担 provider 元数据，也不改变渲染契约。
#[derive(Debug, Default)]
pub struct SourceItemSegments {
    segment_by_item: HashMap<String, usize>,
    grouped: HashMap<String, GroupedSourceSegment>,
}

#[derive(Debug)]
struct GroupedSourceSegment {
    segment_index: usize,
    item_order: Vec<String>,
    text_by_item: HashMap<String, String>,
    separator: String,
}

/// provider 对同一文本 item 的事件阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceItemUpdate<'a> {
    Delta(&'a str),
    Completed(&'a str),
    /// 多个 provider item 仍投影到同一个 UI 段；组内每个 item 独立增量/完成对账，最终按到达
    /// 顺序用 separator 拼起来。Codex RPC 的 summary 标题用它避免「一个标题一个折叠块」。
    GroupedDelta {
        group_id: &'a str,
        item_id: &'a str,
        separator: &'a str,
    },
    GroupedCompleted {
        group_id: &'a str,
        item_id: &'a str,
        separator: &'a str,
    },
}

#[derive(Debug, Clone, Copy)]
struct GroupedSourceItemUpdate<'a> {
    group_id: &'a str,
    item_id: &'a str,
    separator: &'a str,
    completed: bool,
}

/// 优先按稳定 item id 归并；没有 item 语义时退回既有 [`push_segment`] 文本规则。
///
/// - 新 item 的 delta 总是新开一段，避免短 token 与上一段前缀碰撞后被误丢。
/// - 同 item 后续 delta 原样追加；provider 已声明它是 token delta，不再猜累计快照。
/// - completed 是该 item 的权威快照：相同则忽略，不同则原位校正，绝不追加第二份。
///
/// **Thinking 与 Reply 同权**:普通内容继续按 provider item 独立归并；展示层需要合并的同类
/// item（如 Codex summary 标题）显式走 grouped update，不能靠文本前缀猜边界。
pub fn push_segment_with_source_item(
    segs: &mut Vec<TurnSegment>,
    source_items: &mut SourceItemSegments,
    kind: SegKind,
    text: &str,
    partial: bool,
    update: Option<SourceItemUpdate<'_>>,
) -> PushOutcome {
    // token delta 中的空白也是正文；离散空块则没有可展示内容。
    let token_delta = matches!(
        update,
        Some(SourceItemUpdate::Delta(_) | SourceItemUpdate::GroupedDelta { .. })
    );
    if text.is_empty() || (!token_delta && text.trim().is_empty()) {
        return PushOutcome::IgnoredEmpty;
    }
    let Some(update) = update else {
        return push_segment(segs, kind, text, partial);
    };

    let (item_id, completed) = match update {
        SourceItemUpdate::Delta(item_id) => (item_id, false),
        SourceItemUpdate::Completed(item_id) => (item_id, true),
        SourceItemUpdate::GroupedDelta {
            group_id,
            item_id,
            separator,
        } => {
            return push_grouped_source_item(
                segs,
                source_items,
                kind,
                text,
                GroupedSourceItemUpdate {
                    group_id,
                    item_id,
                    separator,
                    completed: false,
                },
            );
        }
        SourceItemUpdate::GroupedCompleted {
            group_id,
            item_id,
            separator,
        } => {
            return push_grouped_source_item(
                segs,
                source_items,
                kind,
                text,
                GroupedSourceItemUpdate {
                    group_id,
                    item_id,
                    separator,
                    completed: true,
                },
            );
        }
    };
    if item_id.is_empty() {
        return push_segment(segs, kind, text, partial);
    }

    if let Some(index) = source_items.segment_by_item.get(item_id).copied() {
        // 段类型必须与本次 kind 相符:同一条索引下不能把 thinking 写进 reply 段(反之亦然)。
        let buf = match (kind, segs.get_mut(index)) {
            (SegKind::Reply, Some(TurnSegment::Reply(buf)))
            | (SegKind::Thinking, Some(TurnSegment::Thinking(buf))) => Some(buf),
            _ => None,
        };
        if let Some(buf) = buf {
            if completed {
                if buf == text {
                    return PushOutcome::IgnoredDuplicate;
                }
                buf.clear();
                buf.push_str(text);
                return PushOutcome::Replaced;
            }
            buf.push_str(text);
            return PushOutcome::Appended;
        }
        // 防御性自愈：调用方若未来重排/裁剪 segments，陈旧索引不得把文本写进别的 item。
        source_items.segment_by_item.remove(item_id);
    }

    let index = segs.len();
    segs.push(match kind {
        SegKind::Reply => TurnSegment::Reply(text.to_string()),
        SegKind::Thinking => TurnSegment::Thinking(text.to_string()),
    });
    source_items
        .segment_by_item
        .insert(item_id.to_string(), index);
    PushOutcome::Opened
}

fn push_grouped_source_item(
    segs: &mut Vec<TurnSegment>,
    source_items: &mut SourceItemSegments,
    kind: SegKind,
    text: &str,
    update: GroupedSourceItemUpdate<'_>,
) -> PushOutcome {
    let GroupedSourceItemUpdate {
        group_id,
        item_id,
        separator,
        completed,
    } = update;
    if group_id.is_empty() || item_id.is_empty() {
        return push_segment(segs, kind, text, !completed);
    }

    if !source_items.grouped.contains_key(group_id) {
        let segment_index = segs.len();
        segs.push(match kind {
            SegKind::Reply => TurnSegment::Reply(text.to_string()),
            SegKind::Thinking => TurnSegment::Thinking(text.to_string()),
        });
        source_items.grouped.insert(
            group_id.to_string(),
            GroupedSourceSegment {
                segment_index,
                item_order: vec![item_id.to_string()],
                text_by_item: HashMap::from([(item_id.to_string(), text.to_string())]),
                separator: separator.to_string(),
            },
        );
        return PushOutcome::Opened;
    }

    let (part_outcome, combined, segment_index) = {
        let group = source_items
            .grouped
            .get_mut(group_id)
            .expect(summon_i18n::text!("r20a96e08af"));
        // 一个 group 的 separator 是展示契约的一部分，首条确定后不能被后续事件悄悄改写。
        if group.separator != separator {
            return push_segment(segs, kind, text, !completed);
        }

        let part_outcome = if let Some(buf) = group.text_by_item.get_mut(item_id) {
            if completed {
                if buf == text {
                    PushOutcome::IgnoredDuplicate
                } else {
                    buf.clear();
                    buf.push_str(text);
                    PushOutcome::Replaced
                }
            } else {
                merge_delta(buf, text)
            }
        } else {
            group.item_order.push(item_id.to_string());
            group
                .text_by_item
                .insert(item_id.to_string(), text.to_string());
            PushOutcome::Appended
        };

        let combined = group
            .item_order
            .iter()
            .filter_map(|id| group.text_by_item.get(id))
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(&group.separator);
        (part_outcome, combined, group.segment_index)
    };

    if part_outcome == PushOutcome::IgnoredDuplicate {
        return part_outcome;
    }
    let target = match (kind, segs.get_mut(segment_index)) {
        (SegKind::Reply, Some(TurnSegment::Reply(buf)))
        | (SegKind::Thinking, Some(TurnSegment::Thinking(buf))) => Some(buf),
        _ => None,
    };
    if let Some(target) = target {
        target.clear();
        target.push_str(&combined);
        return part_outcome;
    }

    // 防御性自愈：segments 若被调用方重排/裁剪，陈旧索引不能覆盖别的段。丢掉旧组并把当前
    // 文本作为普通新段保住；下一条同组事件会重新建组。
    source_items.grouped.remove(group_id);
    push_segment(segs, kind, text, !completed)
}

/// 把一块文本按 kind 累进有序段序列(纯累积,无 IO):
/// - 空文本(trim 后)忽略——空块不该在卡上留空段。
/// - `partial=true`(cursor `--stream-partial-output` 的增量 token delta):追加到上一段同 kind
///   段(无前缀关系的 token 碎片也能正确拼接)。上一段不同 kind 或序列空 → 新开段(后续 delta
///   继续追加到它)。
/// - `partial=false`(claude/codex 离散块、cursor 段边界完整快照):
///   - Thinking → 总是新开一段(离散 thinking 块不拼)。
///   - Reply → 上一段也是 Reply 且本文是其延伸(完全重复 / 前缀增长快照)→ 归并(cursor 同段
///     重发去重);否则(序列空 / 上一段是 Thinking / 离散块)→ 新开一段。
///
/// > 与 capture 侧 `merge_segment` 的关键区别:那里在「单段 stream_buf」内,互不包含按老版 token delta
/// > **拼接**;这里是**段级**模型,`partial=false` 时互不包含 = codex/claude 各自成段,**不拼**(否则相邻
/// > 两块离散 reply 会被粘成一坨,丢交织粒度)。
pub fn push_segment(
    segs: &mut Vec<TurnSegment>,
    kind: SegKind,
    text: &str,
    partial: bool,
) -> PushOutcome {
    if text.trim().is_empty() {
        return PushOutcome::IgnoredEmpty;
    }
    if partial {
        // cursor 增量 delta:归并进上一段同 kind。**不能盲 push_str**——cursor 段末会把整段全文
        // 再发一次、且该行带 timestamp_ms → 仍是 partial=true,盲拼会整段重复(owner 实测卡片重复
        // 根因)。merge_delta 统一处理「重发去重 / 累积增长 / 真 token 增量拼接」,与 subprocess 路的
        // merge_segment 对齐(两条累积路对同一份 cursor 流必须同解,否则卡片与转录/回复发散)。
        match kind {
            SegKind::Thinking => match segs.last_mut() {
                Some(TurnSegment::Thinking(buf)) => merge_delta(buf, text),
                _ => {
                    segs.push(TurnSegment::Thinking(text.to_string()));
                    PushOutcome::Opened
                }
            },
            SegKind::Reply => match segs.last_mut() {
                Some(TurnSegment::Reply(buf)) => merge_delta(buf, text),
                _ => {
                    segs.push(TurnSegment::Reply(text.to_string()));
                    PushOutcome::Opened
                }
            },
        }
    } else {
        match kind {
            // 非增量 thinking(claude/codex 离散块)→ 总是新开段(不拼)。
            SegKind::Thinking => {
                segs.push(TurnSegment::Thinking(text.to_string()));
                PushOutcome::Opened
            }
            // 非增量 reply:前缀关系检测合并(cursor 段边界重发去重 / claude 累积快照)。
            SegKind::Reply => match segs.last_mut() {
                Some(TurnSegment::Reply(buf)) if is_extension(buf, text) => {
                    merge_extension(buf, text)
                }
                _ => {
                    segs.push(TurnSegment::Reply(text.to_string()));
                    PushOutcome::Opened
                }
            },
        }
    }
}

/// 把一块 partial=true delta 归并进当前段 buf:消除 cursor「段末把整段全文当 delta 再发一次」的重发
/// (该行带 timestamp_ms 故 partial=true,盲 push_str 会整段重复)。规则同 subprocess 的 merge_segment:
/// 完全重复 / 更短重发(buf 已含 text 为前缀)→ 忽略;累积增长式重发(text 含 buf 为前缀)→ 取更长
/// 整段替换;其余(互不含)= 真 token 增量 → 拼接。
fn merge_delta(buf: &mut String, text: &str) -> PushOutcome {
    if text == buf.as_str() || buf.starts_with(text) {
        // 重复 / 更短重发,已含 → 忽略
        PushOutcome::IgnoredDuplicate
    } else if text.starts_with(buf.as_str()) {
        // 累积增长式重发 → 取更长整段
        buf.clear();
        buf.push_str(text);
        PushOutcome::Replaced
    } else {
        // 真 token 增量(互不含)→ 拼接
        buf.push_str(text);
        PushOutcome::Appended
    }
}

/// 本块是否是上一 Reply 段的**延伸**(merge ①②):完全重复 / 更短重发(已含)/ 累积增长快照
/// (上一段为前缀)/ 段尾重述(buf 以 text 结尾——cursor 段末快照重述当前段尾,尤其前面已有 delta
/// 拼装内容时,快照不是整段前缀而是尾部子串)。互不包含且非段尾(真新块)→ 由调用方新开段。
fn is_extension(buf: &str, t: &str) -> bool {
    t == buf || buf.starts_with(t) || t.starts_with(buf) || buf.ends_with(t)
}

/// 把延伸块并入上一 Reply 段 buf(已由 `is_extension` 保证前缀关系):重复/更短 → 忽略;增长 → 取更长。
fn merge_extension(buf: &mut String, t: &str) -> PushOutcome {
    if t.starts_with(buf.as_str()) && t.len() > buf.len() {
        // 累积增长快照:用更长的整段替换
        buf.clear();
        buf.push_str(t);
        PushOutcome::Replaced
    } else {
        PushOutcome::IgnoredDuplicate
    }
    // 否则(完全重复 / 更短重发,已含)→ 忽略
}

/// 工具名 → 友好中文标签(给 live 卡的「🔧 执行 X 中…」运行指示用)。跨 cli 的归一名都覆盖:
/// claude=`Bash`/`Read`/…,codex=`command_execution`/`file_change`,cursor=`shell`/`read`/…。
/// 未识别名原样返回(不丢信息);**只接受工具名,绝不接受入参**(指示行只显工具,不暴露 tool_input)。
pub fn tool_label(name: &str) -> String {
    match name {
        "command_execution" | "shell" | "Bash" => summon_i18n::text!("r928f87d450"),
        "read" | "Read" => summon_i18n::text!("r534cb3fa8f"),
        "edit" | "Edit" | "file_change" => summon_i18n::text!("r1d5b437a3b"),
        "search" | "Search" => summon_i18n::text!("r44ce7ae909"),
        // 以下几种是 codex app-server 的 item.type。它们不是「工具」,但走同一条工具项通道,
        // 没有条目就会原样漏成英文,卡上读作「🔧 执行 plan 中…」—— 既像出错又没信息。
        "plan" => summon_i18n::text!("ra2fdfd0e8f"),
        "context_compaction" => summon_i18n::text!("rd36946e07f"),
        "web_search" => summon_i18n::text!("rf30b4fa151"),
        "mcp_tool_call" => summon_i18n::text!("r7d7f913591"),
        "image_generation" => summon_i18n::text!("r71c9cf3da9"),
        "image_view" => summon_i18n::text!("ref2e03c779"),
        "sub_agent_activity" => summon_i18n::text!("r58138d0b95"),
        other => other,
    }
    .to_string()
}

/// CoT 工具行的「工具名 + 对象」,如 `WebFetch docs.rs/serde`、`grep ViTContain`。
///
/// CoT 标题保留工具的原始英文名(owner 2026-09-17 定);live 卡的中文友好名见 [`tool_label`]。
/// 对象按工具从入参里取最能说明这一步的字段,取不到就只有工具名。
pub fn tool_title(tool: &str, input: Option<&serde_json::Value>) -> String {
    let name = tool_title_name(tool, input);
    match tool_target(tool, input) {
        Some(target) => clip_cmd(&format!("{name} {target}")),
        None => clip_cmd(&name),
    }
}

/// 工具名。MCP 工具显示成 `server · tool`:claude 的全名是 `mcp__<server>__<tool>`,前缀只说明
/// 「这是 MCP」;codex 的 mcpToolCall 与 collabAgentToolCall 是泛类型,名字在 item 自己的字段里。
fn tool_title_name(tool: &str, input: Option<&serde_json::Value>) -> String {
    let field = |key: &str| input.and_then(|input| input_text(input, key));
    let derived = match tool {
        "mcp_tool_call" => field("server")
            .zip(field("tool"))
            .map(|(server, tool)| format!("{server} · {tool}")),
        "collab_agent_tool_call" => field("tool").map(str::to_string),
        _ => tool
            .strip_prefix("mcp__")
            .and_then(|rest| rest.rsplit_once("__"))
            .filter(|(server, tool)| !server.is_empty() && !tool.is_empty())
            .map(|(server, tool)| format!("{server} · {tool}")),
    };
    derived.unwrap_or_else(|| tool.to_string())
}

fn input_text<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// 这一步作用的对象,已按类型脱敏:网址只留 host + path(查询串与片段里常带签名、token),
/// 文件只留文件名(整条路径当一个整体取末段,含空格的目录名也不外露),其余文本经
/// [`redact_tool_output`]。
fn tool_target(tool: &str, input: Option<&serde_json::Value>) -> Option<String> {
    use serde_json::Value;
    let input = input?;
    let text = |key: &str| input_text(input, key).map(redact_tool_output);
    let target = match tool {
        "WebFetch" => input_text(input, "url").map(url_target),
        "WebSearch" => text("query"),
        "web_search" => text("query").or_else(|| web_search_action_target(input.get("action")?)),
        "ToolSearch" => input_text(input, "query")
            .map(|query| redact_tool_output(query.strip_prefix("select:").unwrap_or(query))),
        "Skill" => text("skill"),
        "SendMessage" => text("to"),
        "Agent" | "Task" => text("subagent_type"),
        "Grep" | "grep" | "Glob" => text("pattern"),
        "glob" => text("globPattern"),
        "getMcpTools" => text("server"),
        "file_change" => changed_files(input.get("changes")?),
        "sleep" => input
            .get("durationMs")
            .and_then(Value::as_u64)
            .map(format_duration_ms),
        "sub_agent_activity" => sub_agent_activity(input),
        "mcp_tool_call" => input.get("arguments").and_then(path_target),
        _ => None,
    };
    target.or_else(|| path_target(input))
}

fn path_target(input: &serde_json::Value) -> Option<String> {
    ["file_path", "path", "notebook_path"]
        .into_iter()
        .find_map(|key| input_text(input, key))
        .and_then(file_name)
}

fn file_name(path: &str) -> Option<String> {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// 网址只留 host + path:账号密码(`user:pass@`)、查询串、片段里常带凭据,一律去掉;
/// webhook 类地址的 token 就在路径里,只留 host;路径里像 token 的长段折成 `…`。
fn url_target(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let lower = path.to_ascii_lowercase();
    if ["/hook", "/webhook", "/services/"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return redact_tool_output(host);
    }
    let path = path
        .split('/')
        .map(|segment| {
            if looks_like_token(segment) {
                "…"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    redact_tool_output(&format!("{host}{path}"))
}

fn looks_like_token(segment: &str) -> bool {
    segment.len() >= 24
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && segment.chars().any(|c| c.is_ascii_digit())
        && segment.chars().any(|c| c.is_ascii_alphabetic())
}

/// codex 的 webSearch 在开始时 `query` 可能还是空的,动作里才有这一步真正的对象。
fn web_search_action_target(action: &serde_json::Value) -> Option<String> {
    let first_query = || {
        action
            .get("queries")?
            .as_array()?
            .iter()
            .filter_map(|query| query.as_str().map(str::trim))
            .find(|query| !query.is_empty())
    };
    input_text(action, "query")
        .or_else(first_query)
        .or_else(|| input_text(action, "pattern"))
        .map(redact_tool_output)
        .or_else(|| input_text(action, "url").map(url_target))
}

fn changed_files(changes: &serde_json::Value) -> Option<String> {
    let names: Vec<String> = changes
        .as_array()?
        .iter()
        .filter_map(|change| file_name(change.get("path")?.as_str()?))
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let secs = ms / 1000;
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m{s}s"),
    }
}

fn sub_agent_activity(input: &serde_json::Value) -> Option<String> {
    let agent = input_text(input, "agentPath").and_then(file_name);
    let kind = input_text(input, "kind").map(redact_tool_output);
    match (agent, kind) {
        (Some(agent), Some(kind)) => Some(format!("{agent} {kind}")),
        (agent, kind) => agent.or(kind),
    }
}

/// live 卡运行指示的标签:**有具体命令明细就显命令**(更具体,owner 要的「执行 echo hello 中…」),
/// 否则回退到 `tool_label`(友好工具名)。`command` 形如 codex 的 `/bin/zsh -lc '<cmd>'`——剥 shell 壳
/// 取内层、截断到 ~50 列(指示行单行,过长省略)。仍**不暴露完整 tool_input**,只取可读命令。
pub fn tool_running_label(tool: &str, command: Option<&str>) -> String {
    match command.map(str::trim).filter(|c| !c.is_empty()) {
        // 先脱敏再截断:截断在后能保证省略号之前留下的都是已脱敏文本。
        Some(cmd) => clip_cmd(&redact_command(cmd)),
        None => tool_label(tool),
    }
}

/// 给过程载体展示的命令明细。与运行标签共用同一条路径脱敏边界。
pub fn redact_command(command: &str) -> String {
    redact_secrets(&redact_paths(shell_inner(command)))
}

pub fn redact_tool_output(output: &str) -> String {
    redact_secrets(&redact_paths(output))
}

pub(crate) fn redact_secrets(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            let header = [
                "authorization:",
                "x-api-key:",
                "x_api_key:",
                "cookie:",
                "set-cookie:",
            ]
            .iter()
            .find_map(|needle| lower.find(needle).map(|index| (index, *needle)));
            if let Some((index, needle)) = header {
                let prefix_end = index + needle.len();
                let newline = if line.ends_with('\n') { "\n" } else { "" };
                return format!("{} [REDACTED]{newline}", &line[..prefix_end]);
            }
            let contains_cli_secret = [
                "--token ",
                "--password ",
                "--secret ",
                "--api-key ",
                "--access-key ",
            ]
            .iter()
            .any(|needle| lower.contains(needle));
            let contains_structured_secret = lower.contains(':')
                && [
                    "\"access_token\"",
                    "\"refresh_token\"",
                    "\"api_key\"",
                    "\"password\"",
                    "\"secret\"",
                    "\"session\"",
                ]
                .iter()
                .any(|needle| lower.contains(needle));
            if contains_cli_secret || contains_structured_secret {
                return if line.ends_with('\n') {
                    "[REDACTED]\n".to_string()
                } else {
                    "[REDACTED]".to_string()
                };
            }
            line.split_inclusive(|ch: char| ch.is_whitespace())
                .map(|chunk| {
                    let token_end = chunk
                        .char_indices()
                        .find_map(|(index, ch)| ch.is_whitespace().then_some(index))
                        .unwrap_or(chunk.len());
                    let (token, suffix) = chunk.split_at(token_end);
                    let Some(eq) = token.find('=') else {
                        return chunk.to_string();
                    };
                    let key = token[..eq]
                        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                        .to_ascii_uppercase();
                    let sensitive = [
                        "TOKEN",
                        "SECRET",
                        "PASSWORD",
                        "API_KEY",
                        "ACCESS_KEY",
                        "PRIVATE_KEY",
                    ]
                    .iter()
                    .any(|needle| key.contains(needle));
                    if sensitive {
                        format!("{}=[REDACTED]{suffix}", &token[..eq])
                    } else {
                        chunk.to_string()
                    }
                })
                .collect()
        })
        .collect()
}

/// 非 shell 类工具(Read/Edit/Write…)的标签:`友好名 + 目标文件名`,如 `读取 segment.rs`。
///
/// 光一个「读取」信息量太低 —— 一条轨迹上连着三个「✓ 读取」等于什么都没说(owner 2026-08-07 实测
/// 卡片截图)。路径经 [`redact_paths`] 只留末段,所以带上文件名不会把目录布局漏出去。
/// 取不到路径时退回裸友好名(行为同 [`tool_label`])。
pub fn tool_target_label(tool: &str, path: Option<&str>) -> String {
    let name = tool_label(tool);
    match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => clip_cmd(&format!("{name} {}", redact_paths(p))),
        None => name,
    }
}

/// 命令里的路径**只保留最后一段**——当前在动的那个文件夹/文件名,前缀整条丢掉。
///
/// owner 2026-08-07 实测反馈:卡片是发进群的,`/Users/<用户名>/Desktop/disk_clean` 把用户名和整个
/// 目录布局暴露给群里所有人;而对「它在干什么」而言,`disk_clean` 这一段已经说明一切,前缀是纯负担。
///
/// 作用范围**只限绝对路径(`/…`)与家目录路径(`~/…`)**,外加 claude 转录目录那种把路径压进目录名
/// 的写法(`-Users-<x>-a-b`)。相对路径不动:它本来就不含用户名、也不泄露绝对布局,砍了只丢可读性。
///
/// 只动路径,不动命令动词与参数:`cd disk_clean && ls -1 docs` 仍然读得懂。
fn redact_paths(cmd: &str) -> String {
    cmd.split(' ')
        .map(redact_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// 对单个 token 脱敏。token 可能是 `/a/b`、`--flag=/a/b`、`'/a/b'` 等,故只处理其中的路径片段。
fn redact_token(token: &str) -> String {
    // 压平写法(`-Users-x-a-b`)可能出现在路径中段,先整体折成末段。
    let token = collapse_mangled(token);
    let Some(slash) = token.find('/') else {
        return token;
    };
    let (prefix, path) = token.split_at(slash);
    // 只收绝对路径(prefix 为空)与家目录路径(prefix 以 `~` 收尾)。其余(如 `docs/plans`
    // 这种相对路径、`http://…` 这种 URL)原样放行。
    let is_home = prefix.ends_with('~');
    if !prefix.is_empty() && !is_home {
        return token;
    }
    // 家目录的 `~` 本身也是前缀的一部分,一并丢掉。
    let keep = if is_home {
        &prefix[..prefix.len() - '~'.len_utf8()]
    } else {
        prefix
    };
    format!("{keep}{}", basename_of(path))
}

/// claude 转录目录名把整条路径压成 `-Users-example-Desktop-personal-summon` 这种形式。
/// 同样只留末段 → `summon`。认不出就原样返回。
fn collapse_mangled(s: &str) -> String {
    for marker in ["-Users-", "-home-"] {
        if let Some(i) = s.find(marker) {
            let last = s[i..].rsplit('-').find(|seg| !seg.is_empty()).unwrap_or("");
            return format!("{}{last}", &s[..i]);
        }
    }
    s.to_string()
}

/// 取路径末段,末尾的引号等收尾字符原样保留(否则 `'/a/b'` 会丢掉右引号)。
fn basename_of(path: &str) -> String {
    let core_end = path
        .rfind(|c: char| {
            c.is_alphanumeric() || c == '/' || c == '.' || c == '_' || c == '-' || c == '*'
        })
        .map_or(0, |i| {
            i + path[i..].chars().next().map_or(1, char::len_utf8)
        });
    let (core, tail) = path.split_at(core_end.min(path.len()));
    let last = core.split('/').rfind(|s| !s.is_empty());
    match last {
        Some(name) => format!("{name}{tail}"),
        // 只有斜杠(如 `/`)→ 原样,别渲成空串。
        None => path.to_string(),
    }
}

/// 剥常见 shell 包裹取内层命令:`/bin/zsh -lc '<cmd>'` / `/bin/bash -c '<cmd>'` → `<cmd>`;
/// 认不出就原样返回(不丢信息)。只取首尾单引号之间(够用,不做完整 shell 词法)。
fn shell_inner(cmd: &str) -> &str {
    if cmd.contains(" -lc ") || cmd.contains(" -c ") {
        if let (Some(a), Some(b)) = (cmd.find('\''), cmd.rfind('\'')) {
            if b > a {
                return &cmd[a + 1..b];
            }
        }
    }
    cmd
}

/// 命令截断到 ~50 列(按字符,中文安全),过长加省略号;并把换行压成空格保持单行。
/// 工具轨迹标签的统一收口:压成一行 + 50 字截断。
///
/// description(claude 给的人话摘要)与命令原文共用同一套裁剪 —— 卡片上它们占同一个位置,
/// 长度规则若分叉,同一张卡里就会出现两种宽度。description **不做脱敏**:它是模型自己写的
/// 意图说明,不是从环境里捞来的路径/参数;命令原文那条路仍先 `redact_paths` 再进这里。
pub fn clip_tool_label(text: &str) -> String {
    clip_cmd(text)
}

const MAX_LABEL_CHARS: usize = 50;

fn clip_cmd(cmd: &str) -> String {
    let one_line = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = one_line.chars().collect();
    if chars.len() <= MAX_LABEL_CHARS {
        one_line
    } else {
        format!("{}…", chars[..MAX_LABEL_CHARS].iter().collect::<String>())
    }
}

/// [`clip_tool_label`] 会不会截掉 `text` 的尾部。
pub fn tool_label_is_clipped(text: &str) -> bool {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .count()
        > MAX_LABEL_CHARS
}

/// 完整回复快照是否在重放本轮更早的回复段。
///
/// cursor 会在后续段之后迟到地补发前一段的完整快照,或整轮回复的聚合;空白与原流式增量
/// 不必逐字一致(换行可能被折成空格)。末段是当前段,它的快照照常走 [`push_segment`] 归并。
pub fn replays_earlier_reply(segs: &[TurnSegment], text: &str) -> bool {
    fn squash(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }
    let snapshot = squash(text);
    if snapshot.is_empty() {
        return false;
    }
    let replies: Vec<&str> = segs
        .iter()
        .filter_map(|segment| match segment {
            TurnSegment::Reply(text) => Some(text.as_str()),
            TurnSegment::Thinking(_) => None,
        })
        .collect();
    let current = match segs.last() {
        Some(TurnSegment::Reply(_)) => replies.len() - 1,
        _ => replies.len(),
    };
    replies[..current]
        .iter()
        .any(|earlier| squash(earlier) == snapshot)
        || (replies.len() > 1 && squash(&replies.concat()) == snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_item_deltas_preserve_word_separating_whitespace() {
        let mut segments = Vec::new();
        let mut source = SourceItemSegments::default();
        for text in ["Hello", " ", "world"] {
            push_segment_with_source_item(
                &mut segments,
                &mut source,
                SegKind::Reply,
                text,
                true,
                Some(SourceItemUpdate::Delta("message")),
            );
        }
        assert_eq!(segments, vec![TurnSegment::Reply("Hello world".into())]);
        assert_eq!(
            push_segment_with_source_item(
                &mut segments,
                &mut source,
                SegKind::Reply,
                "Hello world",
                false,
                Some(SourceItemUpdate::Completed("message"))
            ),
            PushOutcome::IgnoredDuplicate
        );
    }

    #[test]
    fn late_snapshot_of_an_earlier_reply_is_a_replay() {
        let segs = vec![
            TurnSegment::Reply("能聚合。\n\nSentry 是按 top frame 聚的。".into()),
            TurnSegment::Thinking("通知到了".into()),
            TurnSegment::Reply("这里不再重复。".into()),
        ];
        assert!(replays_earlier_reply(
            &segs,
            "能聚合。 Sentry 是按 top frame 聚的。"
        ));
        assert!(replays_earlier_reply(
            &segs,
            "能聚合。Sentry 是按 top frame 聚的。这里不再重复。"
        ));
        // 当前段的快照照常归并,不算重放。
        assert!(!replays_earlier_reply(&segs, "这里不再重复。"));
        assert!(!replays_earlier_reply(&segs, "新的一段"));
    }

    #[test]
    fn push_segment_interleaves_thinking_and_reply_in_order() {
        // codex/claude:thinking 与 reply 按到达顺序交替 → 段顺序原样保留。
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Thinking, "先想 A", false);
        push_segment(&mut segs, SegKind::Reply, "答 1", false);
        push_segment(&mut segs, SegKind::Thinking, "再想 B", false);
        push_segment(&mut segs, SegKind::Reply, "答 2", false);
        assert_eq!(
            segs,
            vec![
                TurnSegment::Thinking("先想 A".into()),
                TurnSegment::Reply("答 1".into()),
                TurnSegment::Thinking("再想 B".into()),
                TurnSegment::Reply("答 2".into()),
            ]
        );
    }

    #[test]
    fn push_segment_discrete_replies_each_new_segment_when_no_merge() {
        // claude/codex:相邻两块 reply 互不包含(真离散块)→ 各自成段(不误并)。
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Reply, "第一段", false);
        push_segment(&mut segs, SegKind::Reply, "完全不同的第二段", false);
        assert_eq!(
            segs,
            vec![
                TurnSegment::Reply("第一段".into()),
                TurnSegment::Reply("完全不同的第二段".into()),
            ]
        );
    }

    #[test]
    fn push_segment_cursor_reemit_merges_into_last_reply() {
        // cursor partial=false:同段累积增长快照(后块含前块为前缀)→ 归并进上一 Reply,取更长者。
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Reply, "你好", false);
        push_segment(&mut segs, SegKind::Reply, "你好世界", false); // 增长快照
        push_segment(&mut segs, SegKind::Reply, "你好世界", false); // 完全重发 → 忽略
        assert_eq!(segs, vec![TurnSegment::Reply("你好世界".into())]);
    }

    #[test]
    fn push_partial_cursor_deltas_appends_into_one_segment() {
        // cursor partial=true:增量 token delta 逐块追加到同一段(无前缀关系也能正确拼接)。
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Reply, "分", true);
        push_segment(&mut segs, SegKind::Reply, "析已经做", true);
        push_segment(&mut segs, SegKind::Reply, "完了", true);
        assert_eq!(segs, vec![TurnSegment::Reply("分析已经做完了".into())]);
        // partial=false 的完整快照到来:与累积段为前缀关系 → 被 merge_extension 去重/替换。
        push_segment(&mut segs, SegKind::Reply, "分析已经做完了", false);
        assert_eq!(segs, vec![TurnSegment::Reply("分析已经做完了".into())]);
    }

    #[test]
    fn completed_snapshot_reports_dedup_decision_without_changing_segments() {
        let mut segs = Vec::new();
        assert_eq!(
            push_segment(&mut segs, SegKind::Reply, "回", true),
            PushOutcome::Opened
        );
        assert_eq!(
            push_segment(&mut segs, SegKind::Reply, "复", true),
            PushOutcome::Appended
        );
        assert_eq!(
            push_segment(&mut segs, SegKind::Reply, "回复", false),
            PushOutcome::IgnoredDuplicate
        );
        assert_eq!(segs, vec![TurnSegment::Reply("回复".into())]);
    }

    #[test]
    fn codex_item_completion_reconciles_its_own_delta_segment() {
        let mut segs = vec![TurnSegment::Reply("收到上一条".into())];
        let mut source_items = SourceItemSegments::default();

        // 「收」碰巧也是上一段的前缀。通用 merge_delta 会误判成较短快照并丢掉它；
        // 有稳定 item id 的 Codex delta 必须按 provider 事件语义原样累积，不能猜文本形状。
        assert_eq!(
            push_segment_with_source_item(
                &mut segs,
                &mut source_items,
                SegKind::Reply,
                "收",
                true,
                Some(SourceItemUpdate::Delta("item_current")),
            ),
            PushOutcome::Opened
        );
        assert_eq!(
            push_segment_with_source_item(
                &mut segs,
                &mut source_items,
                SegKind::Reply,
                "尾",
                true,
                Some(SourceItemUpdate::Delta("item_current")),
            ),
            PushOutcome::Appended
        );
        assert_eq!(
            push_segment_with_source_item(
                &mut segs,
                &mut source_items,
                SegKind::Reply,
                "收尾",
                false,
                Some(SourceItemUpdate::Completed("item_current")),
            ),
            PushOutcome::IgnoredDuplicate
        );
        assert_eq!(
            segs,
            vec![
                TurnSegment::Reply("收到上一条".into()),
                TurnSegment::Reply("收尾".into()),
            ]
        );

        // provider 终态若与已收 delta 不同，只能原位校正；没有 delta 的独立 item 才新开段。
        assert_eq!(
            push_segment_with_source_item(
                &mut segs,
                &mut source_items,
                SegKind::Reply,
                "收尾。",
                false,
                Some(SourceItemUpdate::Completed("item_current")),
            ),
            PushOutcome::Replaced
        );
        assert_eq!(
            push_segment_with_source_item(
                &mut segs,
                &mut source_items,
                SegKind::Reply,
                "只有终态",
                false,
                Some(SourceItemUpdate::Completed("item_without_delta")),
            ),
            PushOutcome::Opened
        );
        assert_eq!(
            segs,
            vec![
                TurnSegment::Reply("收到上一条".into()),
                TurnSegment::Reply("收尾。".into()),
                TurnSegment::Reply("只有终态".into()),
            ]
        );
    }

    #[test]
    fn push_partial_full_block_reemission_dedups_not_doubles() {
        // 真机根因(owner 实测卡片整段重复):cursor 段末会把整段全文**再发一次**,且该行带
        // `timestamp_ms` → partial=true。旧实现 partial=true 分支盲 `push_str`,把整段追加到已由
        // delta 拼好的同段上 → 整段重复(卡片路 push_segment 与 subprocess 路 merge_segment 对
        // partial=true 重发处理不一致:后者靠 t==buf 去重、幸免)。现在 partial=true 也做重发去重。
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Reply, "新的两个里", true);
        push_segment(&mut segs, SegKind::Reply, "第 1 个我查过了。", true);
        // 段末把整段全文当 delta 再发一遍(partial=true,= 前面 delta 的字节级拼接)。
        push_segment(
            &mut segs,
            SegKind::Reply,
            "新的两个里第 1 个我查过了。",
            true,
        );
        assert_eq!(
            segs,
            vec![TurnSegment::Reply("新的两个里第 1 个我查过了。".into())]
        );
    }

    #[test]
    fn push_multiseg_cursor_stream_no_dup() {
        // cursor 真实多段流(对应 fixture cursor-stream-partial-multiseg.jsonl):
        // seg1 delta AA/BB + 段末整段 AABB(partial=true 重发);seg2 delta CC/DD + 完整快照 CCDD
        // (partial=false)。正确结果 = 单段 "AABBCCDD"(与 result 行一致),不得出现 AABBAABB 或
        // 重复的 CCDD。
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Reply, "AA", true);
        push_segment(&mut segs, SegKind::Reply, "BB", true);
        push_segment(&mut segs, SegKind::Reply, "AABB", true); // 段末全文重发(partial=true)
        push_segment(&mut segs, SegKind::Reply, "CC", true);
        push_segment(&mut segs, SegKind::Reply, "DD", true);
        push_segment(&mut segs, SegKind::Reply, "CCDD", false); // 完整快照(partial=false)
        assert_eq!(segs, vec![TurnSegment::Reply("AABBCCDD".into())]);
    }

    #[test]
    fn push_partial_thinking_deltas_appends_into_one_segment() {
        // cursor thinking partial=true delta 追加到同一 Thinking 段。
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Thinking, "The", true);
        push_segment(&mut segs, SegKind::Thinking, " analysis", true);
        push_segment(&mut segs, SegKind::Thinking, " is", true);
        assert_eq!(segs, vec![TurnSegment::Thinking("The analysis is".into())]);
    }

    #[test]
    fn push_partial_delta_after_different_kind_starts_new_segment() {
        // partial=true delta 上一段不同 kind → 新开段(不跨 kind 拼接)。
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Thinking, "思考中", true);
        push_segment(&mut segs, SegKind::Reply, "你好", true); // 上一段是 Thinking → 新开
        assert_eq!(
            segs,
            vec![
                TurnSegment::Thinking("思考中".into()),
                TurnSegment::Reply("你好".into()),
            ]
        );
    }

    #[test]
    fn push_segment_thinking_between_replies_prevents_merge() {
        // thinking 夹在两块 reply 之间:第二块 reply 上一段是 Thinking → 必新开段(不并进首块)。
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Reply, "你好", false);
        push_segment(&mut segs, SegKind::Thinking, "想一下", false);
        push_segment(&mut segs, SegKind::Reply, "你好", false); // 同文但隔了 thinking → 新段
        assert_eq!(
            segs,
            vec![
                TurnSegment::Reply("你好".into()),
                TurnSegment::Thinking("想一下".into()),
                TurnSegment::Reply("你好".into()),
            ]
        );
    }

    #[test]
    fn push_segment_ignores_empty_text() {
        let mut segs = Vec::new();
        push_segment(&mut segs, SegKind::Thinking, "   ", false);
        push_segment(&mut segs, SegKind::Reply, "", true);
        push_segment(&mut segs, SegKind::Reply, "\n\t ", false);
        assert!(segs.is_empty(), "空块不该留段");
    }

    #[test]
    fn tool_label_maps_common_names_and_falls_back() {
        // 常见工具名 → 友好中文(跨 cli 的归一名都覆盖)。
        assert_eq!(tool_label("command_execution"), "命令");
        assert_eq!(tool_label("shell"), "命令");
        assert_eq!(tool_label("Bash"), "命令");
        assert_eq!(tool_label("read"), "读取");
        assert_eq!(tool_label("edit"), "改文件");
        assert_eq!(tool_label("file_change"), "改文件");
        assert_eq!(tool_label("search"), "搜索");
        // 未知名 → 原样返回(不丢信息)。
        assert_eq!(tool_label("MysteryTool"), "MysteryTool");
    }

    /// codex app-server 的这几种 item.type 走的是同一条工具项通道,但过去没有标签条目,
    /// 于是卡上直接漏出英文:「🔧 执行 plan 中…」「🔧 执行 context_compaction 中…」。
    /// 它们不是工具,读起来却像工具出错 —— 逐个钉住,免得下次加 item 类型时又漏。
    #[test]
    fn codex_item_types_get_readable_labels_instead_of_leaking_english() {
        assert_eq!(tool_label("plan"), "列计划");
        assert_eq!(tool_label("context_compaction"), "压缩上下文");
        assert_eq!(tool_label("web_search"), "联网搜索");
        assert_eq!(tool_label("mcp_tool_call"), "MCP 工具");
        for name in ["plan", "context_compaction", "web_search", "mcp_tool_call"] {
            assert!(!tool_label(name).contains('_'), "`{name}` 仍在漏原始标识符");
        }
    }

    #[test]
    fn tool_running_label_shows_command_detail_or_falls_back() {
        // 有命令明细 → 剥 shell 壳取内层命令(codex 形态)。
        assert_eq!(
            tool_running_label("command_execution", Some("/bin/zsh -lc 'echo hello'")),
            "echo hello"
        );
        assert_eq!(
            tool_running_label("command_execution", Some("/bin/bash -c 'ls -la'")),
            "ls -la"
        );
        // 无壳的命令原样(认不出包裹)。
        assert_eq!(
            tool_running_label("shell", Some("git status")),
            "git status"
        );
        // 无明细 → 回退友好工具名。
        assert_eq!(tool_running_label("command_execution", None), "命令");
        assert_eq!(tool_running_label("read", Some("  ")), "读取");
        // 过长命令截断加省略号(≤50 字符 + …)。
        let long = format!("/bin/zsh -lc '{}'", "x".repeat(80));
        let got = tool_running_label("command_execution", Some(&long));
        assert!(got.ends_with('…') && got.chars().count() == 51);
    }

    /// owner 2026-08-07 实测反馈的原始现场:卡片轨迹把 `/Users/example/Desktop/disk_clean` 整条打了
    /// 出来。第二版口径(owner 追加)是**只留当前工作的那个文件夹**,前缀一律不显。
    #[test]
    fn only_the_working_folder_survives_on_the_card() {
        let label = tool_running_label(
            "Bash",
            Some("cd /Users/example/Desktop/disk_clean && ls -1 docs"),
        );
        assert_eq!(label, "cd disk_clean && ls -1 docs");
        assert!(!label.contains("example"), "用户名必须脱掉:{label}");
        assert!(!label.contains("Desktop"), "中间层级也不该出现:{label}");
    }

    #[test]
    fn cot_details_redact_auth_headers_and_secret_assignments() {
        let command = redact_command(
            "curl -H 'Authorization: Bearer sk-secret' API_TOKEN=sk-secret /Users/alice/a",
        );
        assert!(!command.contains("sk-secret"));
        assert!(!command.contains("alice"));
        assert!(command.contains("[REDACTED]"));

        let output = redact_tool_output("PASSWORD=hunter2\nAuthorization: Bearer abc\nok");
        assert!(!output.contains("hunter2"));
        assert!(!output.contains("Bearer abc"));
        assert!(output.ends_with("ok"));

        for secret in [
            "X-Api-Key: key-123",
            "Cookie: session=abc123",
            r#"{"access_token":"json-secret"}"#,
            "curl --token cli-secret https://example.com",
        ] {
            let redacted = redact_tool_output(secret);
            assert!(redacted.contains("[REDACTED]"), "必须脱敏:{secret}");
            assert!(
                !redacted.contains("key-123")
                    && !redacted.contains("abc123")
                    && !redacted.contains("json-secret")
                    && !redacted.contains("cli-secret"),
                "密钥正文不得残留:{redacted}"
            );
        }
    }

    /// 家目录写法(`~/…`)同样只留末段,`~` 本身也不留。
    #[test]
    fn tilde_paths_collapse_to_the_basename_too() {
        assert_eq!(
            tool_running_label("Bash", Some("cat ~/.summon/agents/arcadia/SOUL.md")),
            "cat SOUL.md"
        );
    }

    /// linux 家目录同理,且用户名不得残留。
    #[test]
    fn linux_home_is_redacted_too() {
        let label = tool_running_label("Bash", Some("cat /home/alice/.ssh/config"));
        assert_eq!(label, "cat config");
        assert!(!label.contains("alice"));
    }

    /// claude 转录目录把整条路径压进一个目录名(`-Users-<x>-…`),同样折到末段。
    #[test]
    fn mangled_project_dir_names_collapse_to_the_last_segment() {
        let label = tool_running_label(
            "Bash",
            Some("ls ~/.claude/projects/-Users-example-Desktop-personal-summon"),
        );
        assert!(
            !label.contains("example"),
            "压平目录名里的用户名也要脱:{label}"
        );
        assert_eq!(label, "ls summon");
    }

    /// 非家目录的深路径一样只留末段。
    #[test]
    fn deep_absolute_paths_keep_only_the_basename() {
        assert_eq!(
            tool_running_label(
                "Bash",
                Some("ls /opt/homebrew/lib/python3.11/site-packages")
            ),
            "ls site-packages"
        );
    }

    /// 相对路径不动:它不含用户名、也不泄露绝对布局,砍了只丢可读性。
    #[test]
    fn relative_paths_are_left_alone() {
        assert_eq!(
            tool_running_label(
                "Bash",
                Some("ls -1 docs/plans && cargo test -p summon-core")
            ),
            "ls -1 docs/plans && cargo test -p summon-core"
        );
    }

    /// 单段绝对路径退化成裸名字(`/tmp` → `tmp`),不该渲成空串。
    #[test]
    fn single_segment_absolute_path_degrades_to_a_bare_name() {
        assert_eq!(tool_running_label("Bash", Some("ls /tmp")), "ls tmp");
    }

    /// 无命令的工具仍回退友好名(既有行为不变)。
    #[test]
    fn tools_without_a_command_still_use_the_friendly_name() {
        assert_eq!(tool_running_label("Read", None), "读取");
    }

    /// owner 2026-08-07 实测:轨迹上连着三个「✓ 读取」等于什么都没说。带上目标文件名。
    /// 路径同样只留末段,所以不会因此漏出目录布局。
    #[test]
    fn file_tools_show_the_target_basename() {
        assert_eq!(
            tool_target_label(
                "Read",
                Some("/Users/example/Desktop/personal/summon/segment.rs")
            ),
            "读取 segment.rs"
        );
        assert_eq!(
            tool_target_label("Edit", Some("~/.summon/agents/arcadia/SOUL.md")),
            "改文件 SOUL.md"
        );
        // 取不到路径 → 退回裸友好名(与 tool_label 一致)。
        assert_eq!(tool_target_label("Read", None), "读取");
        assert_eq!(tool_target_label("Read", Some("   ")), "读取");
    }

    /// 带上文件名之后,用户名仍然不能出现 —— 脱敏必须先于拼接。
    #[test]
    fn target_label_never_leaks_the_username() {
        let label = tool_target_label("Read", Some("/Users/example/secret/notes.md"));
        assert!(!label.contains("example"), "{label}");
        assert_eq!(label, "读取 notes.md");
    }

    fn titled(tool: &str, input: serde_json::Value) -> String {
        tool_title(tool, Some(&input))
    }

    #[test]
    fn claude_tools_are_titled_by_name_and_what_they_act_on() {
        use serde_json::json;
        assert_eq!(
            titled(
                "WebFetch",
                json!({"url": "https://www.cnbc.com/markets", "prompt": "总结"})
            ),
            "WebFetch www.cnbc.com/markets"
        );
        assert_eq!(
            titled("WebSearch", json!({"query": "Fed decision"})),
            "WebSearch Fed decision"
        );
        assert_eq!(
            titled("Skill", json!({"skill": "haohao-shuohua", "args": "x"})),
            "Skill haohao-shuohua"
        );
        assert_eq!(
            titled(
                "ToolSearch",
                json!({"query": "select:WebSearch,WebFetch", "max_results": 2})
            ),
            "ToolSearch WebSearch,WebFetch"
        );
        assert_eq!(
            titled(
                "SendMessage",
                json!({"to": "reviewer", "message": "继续", "summary": "s"})
            ),
            "SendMessage reviewer"
        );
        assert_eq!(
            titled(
                "Agent",
                json!({"subagent_type": "Explore", "description": "找入口", "prompt": "p"})
            ),
            "Agent Explore"
        );
        assert_eq!(
            titled(
                "Grep",
                json!({"pattern": "fn main", "path": "/Users/x/src"})
            ),
            "Grep fn main"
        );
        assert_eq!(
            titled("Read", json!({"file_path": "/Users/x/lib.rs"})),
            "Read lib.rs"
        );
        assert_eq!(
            titled("TaskOutput", json!({"task_id": "abc", "block": true})),
            "TaskOutput"
        );
    }

    #[test]
    fn cursor_tools_are_titled_by_name_and_what_they_act_on() {
        use serde_json::json;
        assert_eq!(
            titled(
                "grep",
                json!({"pattern": "ViTContain", "path": "/Users/x/repo"})
            ),
            "grep ViTContain"
        );
        assert_eq!(
            titled(
                "glob",
                json!({"globPattern": "**/*.swift", "targetDirectory": "/Users/x/repo"})
            ),
            "glob **/*.swift"
        );
        assert_eq!(
            titled(
                "getMcpTools",
                json!({"server": "iOS_context", "toolCallId": "c"})
            ),
            "getMcpTools iOS_context"
        );
        assert_eq!(
            titled("await", json!({"taskId": "8806", "blockUntilMs": 45000})),
            "await"
        );
        assert_eq!(
            titled("write", json!({"path": "/Users/x/a.md"})),
            "write a.md"
        );
    }

    #[test]
    fn codex_items_are_titled_by_name_and_what_they_act_on() {
        use serde_json::json;
        assert_eq!(
            titled(
                "file_change",
                json!({"changes": [
                    {"path": "/Users/x/cot.rs", "kind": "update", "diff": ""},
                    {"path": "/Users/x/lib.rs", "kind": "update", "diff": ""}
                ]})
            ),
            "file_change cot.rs, lib.rs"
        );
        assert_eq!(
            titled("web_search", json!({"query": "serde preserve_order"})),
            "web_search serde preserve_order"
        );
        assert_eq!(
            titled(
                "web_search",
                json!({"query": "", "action": {"type": "openPage", "url": "https://docs.rs/serde"}})
            ),
            "web_search docs.rs/serde"
        );
        assert_eq!(
            titled(
                "web_search",
                json!({"query": "", "action": {"type": "search", "queries": ["", "rust mcp"]}})
            ),
            "web_search rust mcp"
        );
        assert_eq!(titled("sleep", json!({"durationMs": 30000})), "sleep 30s");
        assert_eq!(
            titled("sleep", json!({"durationMs": 150000})),
            "sleep 2m30s"
        );
        assert_eq!(titled("sleep", json!({"durationMs": 120000})), "sleep 2m");
        assert_eq!(titled("sleep", json!({"durationMs": 250})), "sleep 250ms");
        assert_eq!(
            titled("image_view", json!({"path": "/Users/x/shot.png"})),
            "image_view shot.png"
        );
        assert_eq!(
            titled(
                "sub_agent_activity",
                json!({"agentPath": "/root/worker", "agentThreadId": "t", "kind": "completed"})
            ),
            "sub_agent_activity worker completed"
        );
        assert_eq!(
            titled("context_compaction", json!({})),
            "context_compaction"
        );
    }

    #[test]
    fn mcp_tools_are_titled_by_server_and_tool() {
        use serde_json::json;
        assert_eq!(
            tool_title("mcp__plugin_infra-ios_iOS_context__search_wiki", None),
            "plugin_infra-ios_iOS_context · search_wiki"
        );
        assert_eq!(tool_title("mcp__broken", None), "mcp__broken");
        assert_eq!(
            titled("mcp__docs__read", json!({"path": "/Users/x/a.md"})),
            "docs · read a.md"
        );
        assert_eq!(
            titled(
                "mcp_tool_call",
                json!({"server": "lark", "tool": "docs__create", "arguments": {"path": "/Users/x/b.md"}})
            ),
            "lark · docs__create b.md"
        );
        assert_eq!(tool_title("mcp_tool_call", None), "mcp_tool_call");
        assert_eq!(
            titled(
                "collab_agent_tool_call",
                json!({"tool": "spawnAgent", "prompt": "p"})
            ),
            "spawnAgent"
        );
    }

    #[test]
    fn title_targets_are_redacted_by_kind() {
        use serde_json::json;
        assert_eq!(
            titled(
                "WebFetch",
                json!({"url": "https://a.io/x?id=1&access_token=SEKRIT9#frag"})
            ),
            "WebFetch a.io/x"
        );
        assert_eq!(
            titled(
                "WebFetch",
                json!({"url": "https://s3.io/o?X-Amz-Signature=SIG123&sig=SIG456"})
            ),
            "WebFetch s3.io/o"
        );
        assert_eq!(
            titled(
                "web_search",
                json!({"query": "", "action": {"type": "openPage", "url": "https://d.io/p?token=T1"}})
            ),
            "web_search d.io/p"
        );
        assert_eq!(
            titled(
                "file_change",
                json!({"changes": [{"path": "/Users/John Smith/repo/a.rs", "kind": "add", "diff": ""}]})
            ),
            "file_change a.rs"
        );
        assert_eq!(
            titled("Read", json!({"file_path": "/Users/John Smith/notes.md"})),
            "Read notes.md"
        );
        let title = titled("WebSearch", json!({"query": "curl --token abc123 x"}));
        assert!(!title.contains("abc123"), "{title}");
        assert_eq!(
            titled(
                "WebFetch",
                json!({"url": "https://oauth2:glpat-SEKRIT@gitlab.io/g/p"})
            ),
            "WebFetch gitlab.io/g/p"
        );
        assert_eq!(
            titled(
                "web_search",
                json!({"query": "", "action": {"type": "openPage", "url": "https://u:p@x.io/y?t=1"}})
            ),
            "web_search x.io/y"
        );
        assert_eq!(
            titled(
                "WebFetch",
                json!({"url": "https://open.feishu.cn/open-apis/bot/v2/hook/0a1b2c3d-4e5f"})
            ),
            "WebFetch open.feishu.cn"
        );
        assert_eq!(
            titled(
                "WebFetch",
                json!({"url": "https://api.x.io/v1/files/a8f3e2b19c7d4e5f6a7b8c9d0e1f2a3b/raw"})
            ),
            "WebFetch api.x.io/v1/files/…/raw"
        );
        assert_eq!(
            titled(
                "WebFetch",
                json!({"url": "https://docs.rs/serde_json/latest/serde_json"})
            ),
            "WebFetch docs.rs/serde_json/latest/serde_json"
        );
    }
}
