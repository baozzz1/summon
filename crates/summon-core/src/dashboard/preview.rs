//! 控制台的飞书效果预览(纯件)。
//!
//! 一段固定的示例对话走 summon 真实发消息时的那几步 —— 轮内门控与工具账本([`crate::turn_gate`])、
//! 原生 CoT 投影([`CotProjector`])、回复卡组装([`crate::reply_card`])、语义表情校验
//! ([`plan_agent_react`])—— 产出此刻会出现在飞书里的几条消息:用户那条消息上的表情、过程、回复卡、
//! 会话列表的预览行。前端只负责按飞书的样子画它们。
//!
//! 「每一项设置管哪一块」由 [`regions`] 算:把这一项换成对照取值再渲染一次,两次结果里不同的
//! 元素就是它管的区块。不联网、不发送、不读盘。

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_msg::{AgentKind, AgentMessage};
use crate::config::settings::{chat_scoped, ChatOverrides, ResolvedSettings, SETTINGS};
use crate::config::{Messaging, MessagingOverride};
use crate::context_budget::ContextBudget;
use crate::cot::{CotEvent, CotProjector};
use crate::feed_tag::split_feed_tag;
use crate::feishu::{PreviewButton, TurnBadge};
use crate::file_preview::links::extract_local_links;
use crate::image::ReplyPiece;
use crate::messaging::result_control::visible_progress;
use crate::reactions::{plan_agent_react, ReactPlan};
use crate::reply_card::{compose_reply_card, ReplyInput};
use crate::segment::{push_segment_with_source_item, SegKind, SourceItemSegments, TurnSegment};
use crate::subagent_projection::SubagentProjection;
use crate::turn_gate::{
    cot_task_summary, gate_event, release_end, GateSettings, Gated, ToolLedger,
};

/// 示例对话里的一步工具。
#[derive(Debug, Clone, Copy)]
pub struct ScenarioTool {
    pub tool: &'static str,
    pub call_id: &'static str,
    /// 入参里唯一的那个键(`command` / `file_path`)与它的值。
    pub input_key: &'static str,
    pub input: &'static str,
    pub result: &'static str,
}

/// 预览用的固定示例对话。内容是内置文案,不取任何真实会话。
#[derive(Debug, Clone, Copy)]
pub struct Scenario {
    pub chat_name: &'static str,
    pub sender: &'static str,
    /// 群里那条消息(带 @)。
    pub message: &'static str,
    /// 交给 agent 的正文(去掉 @ 之后),CoT 标题取它。
    pub task: &'static str,
    pub thinking: [&'static str; 2],
    pub tools: [ScenarioTool; 2],
    /// 最终回复正文,不含 feed 标签。
    pub reply: &'static str,
    /// agent 写在回复末尾的 feed 总结。
    pub feed: &'static str,
    /// agent 给这条消息贴的语义表情。
    pub reaction: &'static str,
    pub cli: &'static str,
    pub model: &'static str,
    pub effort: &'static str,
    pub context_used_tokens: u64,
    pub context_window_tokens: u64,
    /// 回复卡尾部的时刻。
    pub timestamp: &'static str,
}

const SCENARIO_KEYS: Scenario = Scenario {
    chat_name: "ra87ecbbc16",
    sender: "r611382dbe2",
    message: "rfafb0f1f1d",
    task: "r60e53986d6",
    thinking: [
        "r11ff262794",
        "r647583c8b9",
    ],
    tools: [
        ScenarioTool {
            tool: "Bash",
            call_id: "toolu_preview_1",
            input_key: "command",
            input: "cargo test -p app",
            result: "running 12 tests\ntest parse_rejects_empty_input ... FAILED\n\ntest result: FAILED. 11 passed; 1 failed",
        },
        ScenarioTool {
            tool: "Read",
            call_id: "toolu_preview_2",
            input_key: "file_path",
            input: "src/lib.rs",
            result: "pub fn parse(input: &str) -> Result<Ast, ParseError> {\n    if input.is_empty() {\n        return Err(ParseError::Empty);\n    }\n    // …\n}",
        },
    ],
    reply: "rfe73c0cb39",
    feed: "r55a38dd309",
    reaction: "THUMBSUP",
    cli: "claude",
    model: "Opus 5",
    effort: "high",
    context_used_tokens: 86_000,
    context_window_tokens: 200_000,
    timestamp: "10:24",
};

pub fn scenario() -> Scenario {
    let raw = SCENARIO_KEYS;
    Scenario {
        chat_name: crate::i18n::label(raw.chat_name),
        sender: crate::i18n::label(raw.sender),
        message: crate::i18n::label(raw.message),
        task: crate::i18n::label(raw.task),
        thinking: raw.thinking.map(crate::i18n::label),
        reply: crate::i18n::label(raw.reply),
        feed: crate::i18n::label(raw.feed),
        ..raw
    }
}

/// 消息 id。`regions` 里引用的是它们,或它们下面的条目 / 卡片元素 id。
pub const USER_MESSAGE_ID: &str = "user";
pub const COT_MESSAGE_ID: &str = "cot";
pub const CARD_MESSAGE_ID: &str = "reply";
pub const FEED_MESSAGE_ID: &str = "feed";

/// 预览渲染组件逐一支持的卡片元素。回复卡出现了这之外的元素,前端就会画成「未支持」的占位。
pub const SUPPORTED_CARD_TAGS: &[&str] = &[
    "markdown",
    "hr",
    "img",
    "column_set",
    "column",
    "button",
    "collapsible_panel",
];

const PREVIEW_THREAD_ID: &str = "preview";
const PREVIEW_RUN_ID: &str = "preview";
/// 投影用的时间戳。CoT 事件的时间戳只用于排序,预览里不展示。
const PREVIEW_TIMESTAMP: &str = "0";
/// 卡片元素的子元素所在的键:列容器的 `columns`、列与折叠面板的 `elements`。
const CARD_CHILD_KEYS: [&str; 2] = ["elements", "columns"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewView {
    pub title: String,
    pub messages: Vec<PreviewMessage>,
    /// 字段键路径 → 它控制的元素 id(消息 id、CoT 条目 id 或卡片 element_id)。
    pub regions: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviewMessage {
    User {
        id: String,
        sender: String,
        text: String,
        reactions: Vec<String>,
    },
    Cot {
        id: String,
        title: String,
        status: CotStatus,
        items: Vec<CotItem>,
    },
    /// 飞书卡片 2.0 JSON;每个元素带 element_id。
    Card { id: String, card: Value },
    Feed {
        id: String,
        chat_name: String,
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CotStatus {
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CotItem {
    Thinking {
        id: String,
        text: String,
    },
    Tool {
        id: String,
        icon: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    Text {
        id: String,
        text: String,
    },
}

impl PreviewMessage {
    pub fn id(&self) -> &str {
        match self {
            PreviewMessage::User { id, .. }
            | PreviewMessage::Cot { id, .. }
            | PreviewMessage::Card { id, .. }
            | PreviewMessage::Feed { id, .. } => id,
        }
    }
}

impl CotItem {
    pub fn id(&self) -> &str {
        match self {
            CotItem::Thinking { id, .. } | CotItem::Tool { id, .. } | CotItem::Text { id, .. } => {
                id
            }
        }
    }
}

/// 一项要算「管的区块」的设置。
#[derive(Debug, Clone, PartialEq)]
pub struct RegionProbe<'a> {
    /// 字段键路径,如 `messaging.cot_visible`。目前认 `messaging.<字段>`,其余键没有预览效果。
    pub key: &'a str,
    /// 对照取值,按盘上语义(与 YAML 里写的一致,`hide_thinking` 就是「隐藏」)。`None` = 把当前
    /// 生效值取反,只对布尔项有意义。
    pub contrast: Option<Value>,
}

/// 按给定设置渲染示例对话。`title` 是预览栏标题(「按 xx 的设置」),由调用方按页面层级给。
pub fn render(messaging: &Messaging, settings: &ResolvedSettings, title: &str) -> PreviewView {
    PreviewView {
        title: title.to_string(),
        messages: render_messages(messaging, settings),
        regions: BTreeMap::new(),
    }
}

/// 每一项设置管的区块:换成对照取值后与基准渲染不同的元素 id。没有任何变化的项不出现在结果里。
pub fn regions(
    base: &Messaging,
    base_settings: &ResolvedSettings,
    probes: &[RegionProbe<'_>],
) -> BTreeMap<String, Vec<String>> {
    let base_nodes = nodes(&render_messages(base, base_settings));
    probes
        .iter()
        .filter_map(|probe| {
            let value = match &probe.contrast {
                Some(value) => value.clone(),
                None => flipped(base, base_settings, probe.key)?,
            };
            let (messaging, settings) = with_value(base, base_settings, probe.key, &value)?;
            let changed = changed_ids(&base_nodes, &nodes(&render_messages(&messaging, &settings)));
            (!changed.is_empty()).then(|| (probe.key.to_string(), changed))
        })
        .collect()
}

/// 把一项设置换成 `value`(盘上语义)后的生效设置。只改这一项:`messaging.<字段>` 经与配置分层
/// 相同的覆盖(`MessagingOverride` + `apply`)叠上去;本会话的分级开关除这一项外保持原值,这一项
/// 跟着新值走(群页上它可能本来被本群覆盖)。键认不出或取值形状不对时返回 `None`。
pub fn with_value(
    messaging: &Messaging,
    settings: &ResolvedSettings,
    key: &str,
    value: &Value,
) -> Option<(Messaging, ResolvedSettings)> {
    let field = messaging_field(key)?;
    let mut patch = serde_json::Map::new();
    patch.insert(field.to_string(), value.clone());
    let patch: MessagingOverride = serde_json::from_value(Value::Object(patch)).ok()?;
    let mut next = messaging.clone();
    next.apply(&patch);
    let mut pinned = ChatOverrides::default();
    for decl in chat_scoped() {
        pinned.set(decl.id, settings.get(decl.id));
    }
    if let Some(decl) = SETTINGS.iter().find(|decl| decl.config_key == field) {
        pinned.set(decl.id, (decl.get)(&next));
    }
    let resolved = ResolvedSettings::resolve(&next, &pinned);
    Some((next, resolved))
}

/// 示例对话的事件流,与 agent 在这组设置下会产出的一致:feed 约定注入了才在回复末尾写总结标签。
pub fn scenario_events(settings: &ResolvedSettings) -> Vec<AgentMessage> {
    let mut events = Vec::new();
    for (thinking, tool) in scenario().thinking.iter().zip(scenario().tools.iter()) {
        let mut think = AgentMessage::of(AgentKind::Thinking);
        think.text = Some(thinking.to_string());
        events.push(think);
        let mut start = AgentMessage::of(AgentKind::ToolUse);
        start.tool = Some(tool.tool.to_string());
        start.tool_id = Some(tool.call_id.to_string());
        let mut input = serde_json::Map::new();
        input.insert(tool.input_key.to_string(), Value::String(tool.input.into()));
        start.tool_input = Some(Value::Object(input));
        events.push(start);
        let mut finish = AgentMessage::of(AgentKind::ToolResult);
        finish.tool_id = Some(tool.call_id.to_string());
        finish.tool_result = Some(Value::String(tool.result.to_string()));
        events.push(finish);
    }
    let mut reply = AgentMessage::of(AgentKind::Assistant);
    reply.text = Some(reply_text(settings));
    events.push(reply);
    events
}

/// 示例事件按门控投成原生 CoT 事件,直到收口。投影节奏比 chat 线程勤(每条事件都投一次),
/// 但 CoT 只追加、按游标续投,投得勤只会把同样的内容切得更碎,不改变内容与先后。
pub fn project_cot(settings: &GateSettings, events: &[AgentMessage]) -> Vec<CotEvent> {
    let query = cot_task_summary(settings.cot_visible, Some(scenario().task)).unwrap_or_default();
    let mut cot = CotProjector::new(PREVIEW_THREAD_ID, PREVIEW_RUN_ID, query);
    let mut segments: Vec<TurnSegment> = Vec::new();
    let mut source_items = SourceItemSegments::default();
    let mut progress = settings.progress();
    let mut subagents = SubagentProjection::default();
    let mut ledger = ToolLedger::default();
    let mut released = 0usize;
    let mut out = Vec::new();
    fn release(segments: &[TurnSegment], released: &mut usize, trailing_done: bool) {
        if let Some(end) = release_end(segments, *released, trailing_done) {
            *released = end;
        }
    }
    for ev in events {
        let emit = match gate_event(settings, ev) {
            Gated::Subagent | Gated::SubagentHidden | Gated::Skip => false,
            Gated::StatusOnly => true,
            Gated::ToolStart => {
                release(&segments, &mut released, true);
                ledger
                    .start(settings, &mut progress, &mut subagents, ev)
                    .is_some()
            }
            Gated::ToolEnd => {
                release(&segments, &mut released, true);
                ledger.finish(settings, &mut progress, &mut subagents, ev, 0);
                true
            }
            Gated::HiddenThinking => {
                release(&segments, &mut released, true);
                progress.note_thinking(ev.text.as_deref().unwrap_or_default(), 0);
                true
            }
            Gated::Content(kind) => {
                let text = ev.text.as_deref().unwrap_or_default();
                if kind == SegKind::Thinking {
                    release(&segments, &mut released, true);
                    progress.note_thinking(text, 0);
                }
                push_segment_with_source_item(
                    &mut segments,
                    &mut source_items,
                    kind,
                    text,
                    ev.partial,
                    None,
                );
                if kind == SegKind::Reply {
                    release(&segments, &mut released, false);
                }
                true
            }
        };
        if emit {
            let visible = visible_progress(&segments);
            out.extend(cot.project_with_replies(
                visible,
                Some(released.min(visible.len())),
                Some(&progress),
                PREVIEW_TIMESTAMP,
            ));
        }
    }
    // 回复投递时收口过程(与 `stream_finish` 一样按 done 收)。
    out.extend(cot.finish("done", PREVIEW_TIMESTAMP));
    out
}

/// 把原生 CoT 事件折成飞书里看到的条目:思考段落、工具行、过程回复。
///
/// 思考按段落成条:同一条 reasoning 消息里以空行起头的增量是新段落(投影方靠空行分段),
/// 中间插进了工具行的也另起一条。条目 id 只取决于它在本类条目里的次序,开关增删别的条目时不变。
pub fn cot_message(events: &[CotEvent]) -> PreviewMessage {
    let mut title = String::new();
    let mut status = CotStatus::Done;
    let mut items: Vec<CotItem> = Vec::new();
    let mut thinking = 0usize;
    // 最后一条思考条目属于哪条 reasoning 消息:只有它还在末尾时增量才续写进去。
    let mut open_reasoning: Option<String> = None;
    for event in events {
        let Ok(content) = serde_json::from_str::<Value>(&event.content) else {
            continue;
        };
        let text_of = |key: &str| content.get(key).and_then(Value::as_str).unwrap_or_default();
        match event.event_type.as_str() {
            "RUN_STARTED" => {
                title = content
                    .pointer("/input/query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            "REASONING_MESSAGE_CONTENT" => {
                let message = text_of("messageId");
                let delta = text_of("delta");
                let continues = !delta.starts_with("\n\n")
                    && open_reasoning.as_deref() == Some(message)
                    && matches!(items.last(), Some(CotItem::Thinking { .. }));
                if continues {
                    if let Some(CotItem::Thinking { text, .. }) = items.last_mut() {
                        text.push_str(delta);
                    }
                    continue;
                }
                let delta = delta.strip_prefix("\n\n").unwrap_or(delta);
                if delta.is_empty() {
                    continue;
                }
                thinking += 1;
                items.push(CotItem::Thinking {
                    id: format!("{COT_MESSAGE_ID}.thinking-{thinking}"),
                    text: delta.to_string(),
                });
                open_reasoning = Some(message.to_string());
            }
            "TOOL_CALL_START" => items.push(CotItem::Tool {
                id: format!("{COT_MESSAGE_ID}.{}", text_of("toolCallId")),
                icon: text_of("icon").to_string(),
                name: text_of("toolCallName").to_string(),
                args: None,
                result: None,
            }),
            "TOOL_CALL_ARGS" => {
                let command = serde_json::from_str::<Value>(text_of("delta"))
                    .ok()
                    .and_then(|delta| {
                        delta
                            .get("command")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                if let (Some(command), Some(CotItem::Tool { args, .. })) =
                    (command, tool_item(&mut items, text_of("toolCallId")))
                {
                    args.get_or_insert_with(String::new).push_str(&command);
                }
            }
            "TOOL_CALL_RESULT" => {
                let shown = match content.get("content") {
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(Value::Object(code)) => {
                        code.get("code").and_then(Value::as_str).map(str::to_string)
                    }
                    _ => None,
                };
                if let Some(CotItem::Tool { result, .. }) =
                    tool_item(&mut items, text_of("toolCallId"))
                {
                    *result = shown;
                }
            }
            "TEXT_MESSAGE_START" => items.push(CotItem::Text {
                id: format!("{COT_MESSAGE_ID}.{}", text_of("messageId")),
                text: String::new(),
            }),
            "TEXT_MESSAGE_CONTENT" => {
                let id = format!("{COT_MESSAGE_ID}.{}", text_of("messageId"));
                if let Some(CotItem::Text { text, .. }) =
                    items.iter_mut().rev().find(|item| item.id() == id)
                {
                    text.push_str(text_of("delta"));
                }
            }
            "RUN_FINISHED" if text_of("status") != "done" => status = CotStatus::Error,
            "RUN_ERROR" => status = CotStatus::Error,
            _ => {}
        }
    }
    PreviewMessage::Cot {
        id: COT_MESSAGE_ID.to_string(),
        title,
        status,
        items,
    }
}

fn tool_item<'a>(items: &'a mut [CotItem], call_id: &str) -> Option<&'a mut CotItem> {
    let id = format!("{COT_MESSAGE_ID}.{call_id}");
    items.iter_mut().rev().find(|item| item.id() == id)
}

fn render_messages(messaging: &Messaging, settings: &ResolvedSettings) -> Vec<PreviewMessage> {
    let gate = GateSettings::from(messaging, settings);
    let events = scenario_events(settings);
    let cot = cot_message(&project_cot(&gate, &events));
    let (card, feed) = reply_card(settings);
    vec![
        PreviewMessage::User {
            id: USER_MESSAGE_ID.to_string(),
            sender: scenario().sender.to_string(),
            text: scenario().message.to_string(),
            reactions: user_reactions(messaging, settings),
        },
        cot,
        PreviewMessage::Card {
            id: CARD_MESSAGE_ID.to_string(),
            card,
        },
        PreviewMessage::Feed {
            id: FEED_MESSAGE_ID.to_string(),
            chat_name: scenario().chat_name.to_string(),
            text: feed,
        },
    ]
}

/// 最终回复原文:feed 约定注入了(`agent.feed`),agent 才会在末尾写总结标签。
fn reply_text(settings: &ResolvedSettings) -> String {
    if settings.agent_feed() {
        format!(
            "{}\n\n{}{}{}",
            scenario().reply,
            crate::feed_tag::FEED_TAG_OPEN,
            scenario().feed,
            crate::feed_tag::FEED_TAG_CLOSE
        )
    } else {
        scenario().reply.to_string()
    }
}

/// 回复卡与会话列表预览行。角标的档位项跟本会话的 `badge.effort` 走(与轮初角标种子同一取反);
/// 文件按钮按飞书前端的做法从正文认路径,链接用占位。
fn reply_card(settings: &ResolvedSettings) -> (Value, String) {
    let raw = reply_text(settings);
    let tagged = split_feed_tag(&raw);
    let (body, paths) = extract_local_links(&tagged.body, |_| true);
    let buttons: Vec<PreviewButton> = paths
        .iter()
        .map(|path| PreviewButton {
            label: std::path::Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_string(),
            token: String::new(),
            is_dir: false,
        })
        .collect();
    let badge = TurnBadge {
        cli: scenario().cli.to_string(),
        model: Some(scenario().model.to_string()),
        effort: Some(scenario().effort.to_string()),
        hide_effort: !settings.badge_effort(),
        ctx: ContextBudget::from_tokens(
            scenario().context_used_tokens,
            Some(scenario().context_window_tokens),
        ),
        ..TurnBadge::default()
    }
    .render();
    let composed = compose_reply_card(&ReplyInput {
        pieces: &[ReplyPiece::Text(body)],
        badge: badge.as_deref(),
        timestamp: scenario().timestamp,
        feed: tagged.summary.as_deref(),
        preview_buttons: &buttons,
    });
    let mut card: Value = serde_json::from_str(&composed.card_json).unwrap_or(Value::Null);
    if let Some(elements) = card
        .pointer_mut("/body/elements")
        .and_then(Value::as_array_mut)
    {
        stamp_element_ids(elements, CARD_MESSAGE_ID);
    }
    (card, composed.feed.unwrap_or_default())
}

/// 给没有 element_id 的元素按位置补一个(`reply.0`、`preview_files.0.0`),已有的不动;子元素
/// 以父元素的 id 为前缀。
fn stamp_element_ids(elements: &mut [Value], parent: &str) {
    for (index, element) in elements.iter_mut().enumerate() {
        let Some(object) = element.as_object_mut() else {
            continue;
        };
        let id = match object.get("element_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => {
                let id = format!("{parent}.{index}");
                object.insert("element_id".to_string(), Value::String(id.clone()));
                id
            }
        };
        for key in CARD_CHILD_KEYS {
            if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
                stamp_element_ids(children, &id);
            }
        }
    }
}

/// 回复结束时用户那条消息上的表情:agent 贴的语义表情(表情约定注入了它才知道要贴,贴之前过
/// `summon react` 同一道校验),以及工程轨收尾时换上的完成表情。
fn user_reactions(messaging: &Messaging, settings: &ResolvedSettings) -> Vec<String> {
    let mut reactions = Vec::new();
    if settings.agent_reactions() {
        if let ReactPlan::Set { emoji, .. } = plan_agent_react(
            None,
            Some(scenario().reaction),
            false,
            &messaging.reactions,
            Some(messaging.working_reaction.as_str()),
        ) {
            reactions.push(emoji);
        }
    }
    let done = messaging.done_reaction.trim();
    if !done.is_empty() {
        reactions.push(done.to_string());
    }
    reactions
}

fn messaging_field(key: &str) -> Option<&str> {
    key.strip_prefix("messaging.")
        .filter(|field| !field.is_empty() && !field.contains('.'))
}

/// 布尔项的对照值:当前生效值取反,按盘上语义给出。分级开关的生效值看本会话快照(可能被本群覆盖),
/// 其余看叠好的 `Messaging`。
fn flipped(messaging: &Messaging, settings: &ResolvedSettings, key: &str) -> Option<Value> {
    let field = messaging_field(key)?;
    if let Some(decl) = SETTINGS.iter().find(|decl| decl.config_key == field) {
        let stored = settings.get(decl.id) != decl.config_inverted;
        return Some(Value::Bool(!stored));
    }
    let current = serde_json::to_value(messaging)
        .ok()?
        .get(field)?
        .as_bool()?;
    Some(Value::Bool(!current))
}

/// 区块差分的一个节点:消息、CoT 条目或卡片元素。指纹不含子节点,子节点变了不算父节点变了。
struct Node {
    id: String,
    parent: Option<String>,
    fingerprint: String,
}

fn nodes(messages: &[PreviewMessage]) -> Vec<Node> {
    let mut out = Vec::new();
    for message in messages {
        let id = message.id().to_string();
        match message {
            PreviewMessage::User {
                sender,
                text,
                reactions,
                ..
            } => out.push(node(&id, None, &(sender, text, reactions))),
            PreviewMessage::Cot {
                title,
                status,
                items,
                ..
            } => {
                out.push(node(&id, None, &(title, status)));
                for item in items {
                    out.push(node(item.id(), Some(&id), item));
                }
            }
            // 卡片的会话列表预览由 feed 那条消息呈现,卡片本身只比元素。
            PreviewMessage::Card { card, .. } => {
                out.push(node(&id, None, &()));
                if let Some(elements) = card.pointer("/body/elements").and_then(Value::as_array) {
                    element_nodes(elements, &id, &mut out);
                }
            }
            PreviewMessage::Feed {
                chat_name, text, ..
            } => out.push(node(&id, None, &(chat_name, text))),
        }
    }
    out
}

fn element_nodes(elements: &[Value], parent: &str, out: &mut Vec<Node>) {
    for element in elements {
        let Some(object) = element.as_object() else {
            continue;
        };
        let Some(id) = object.get("element_id").and_then(Value::as_str) else {
            continue;
        };
        let own: serde_json::Map<String, Value> = object
            .iter()
            .filter(|(key, _)| !CARD_CHILD_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        out.push(node(id, Some(parent), &own));
        for key in CARD_CHILD_KEYS {
            if let Some(children) = object.get(key).and_then(Value::as_array) {
                element_nodes(children, id, out);
            }
        }
    }
}

fn node(id: &str, parent: Option<&str>, value: &impl Serialize) -> Node {
    Node {
        id: id.to_string(),
        parent: parent.map(str::to_string),
        fingerprint: serde_json::to_string(value).unwrap_or_default(),
    }
}

/// 基准里变了或没了的节点,加上只在对照里出现的节点归到的最近的、基准里也有的祖先;父节点已在
/// 结果里时,子节点不再单列。按基准里的先后排。
fn changed_ids(base: &[Node], contrast: &[Node]) -> Vec<String> {
    let base_by_id: HashMap<&str, &Node> = base.iter().map(|n| (n.id.as_str(), n)).collect();
    let contrast_by_id: HashMap<&str, &Node> =
        contrast.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut hit: Vec<&str> = Vec::new();
    for node in base {
        let same = contrast_by_id
            .get(node.id.as_str())
            .is_some_and(|other| other.fingerprint == node.fingerprint);
        if !same {
            hit.push(&node.id);
        }
    }
    for node in contrast {
        if base_by_id.contains_key(node.id.as_str()) {
            continue;
        }
        let mut parent = node.parent.as_deref();
        while let Some(id) = parent {
            if base_by_id.contains_key(id) {
                if !hit.contains(&id) {
                    hit.push(base_by_id[id].id.as_str());
                }
                break;
            }
            parent = contrast_by_id.get(id).and_then(|n| n.parent.as_deref());
        }
    }
    let parent_of = |id: &str| {
        base_by_id
            .get(id)
            .or_else(|| contrast_by_id.get(id))
            .and_then(|n| n.parent.as_deref())
    };
    let covered = |id: &str| {
        let mut parent = parent_of(id);
        while let Some(ancestor) = parent {
            if hit.contains(&ancestor) {
                return true;
            }
            parent = parent_of(ancestor);
        }
        false
    };
    let order: HashMap<&str, usize> = base
        .iter()
        .enumerate()
        .map(|(index, n)| (n.id.as_str(), index))
        .collect();
    let mut kept: Vec<&str> = hit.iter().copied().filter(|id| !covered(id)).collect();
    kept.sort_by_key(|id| order.get(id).copied().unwrap_or(usize::MAX));
    kept.into_iter().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_messaging;
    use crate::config::settings::{SettingKey, SETTING_COUNT};

    fn settings_with(messaging: &Messaging, chat: &[(SettingKey, bool)]) -> ResolvedSettings {
        let mut overrides = ChatOverrides::default();
        for (id, value) in chat {
            overrides.set(*id, *value);
        }
        ResolvedSettings::resolve(messaging, &overrides)
    }

    fn view(chat: &[(SettingKey, bool)]) -> PreviewView {
        let messaging = builtin_messaging();
        render(&messaging, &settings_with(&messaging, chat), "按整机的设置")
    }

    fn cot_of(view: &PreviewView) -> (&str, &[CotItem]) {
        view.messages
            .iter()
            .find_map(|message| match message {
                PreviewMessage::Cot { title, items, .. } => {
                    Some((title.as_str(), items.as_slice()))
                }
                _ => None,
            })
            .expect("预览里要有过程消息")
    }

    fn card_of(view: &PreviewView) -> &Value {
        view.messages
            .iter()
            .find_map(|message| match message {
                PreviewMessage::Card { card, .. } => Some(card),
                _ => None,
            })
            .expect("预览里要有回复卡")
    }

    fn feed_of(view: &PreviewView) -> &str {
        view.messages
            .iter()
            .find_map(|message| match message {
                PreviewMessage::Feed { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .expect("预览里要有会话列表预览")
    }

    fn reactions_of(view: &PreviewView) -> &[String] {
        view.messages
            .iter()
            .find_map(|message| match message {
                PreviewMessage::User { reactions, .. } => Some(reactions.as_slice()),
                _ => None,
            })
            .expect("预览里要有用户消息")
    }

    fn elements(card: &Value) -> Vec<&Value> {
        fn walk<'a>(list: &'a [Value], out: &mut Vec<&'a Value>) {
            for element in list {
                out.push(element);
                for key in CARD_CHILD_KEYS {
                    if let Some(children) = element.get(key).and_then(Value::as_array) {
                        walk(children, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(card["body"]["elements"].as_array().unwrap(), &mut out);
        out
    }

    fn every_combination() -> impl Iterator<Item = ResolvedSettings> {
        let messaging = builtin_messaging();
        (0..1u32 << SETTING_COUNT).map(move |mask| {
            let mut overrides = ChatOverrides::default();
            for (bit, decl) in chat_scoped().enumerate() {
                overrides.set(decl.id, mask & (1 << bit) != 0);
            }
            ResolvedSettings::resolve(&messaging, &overrides)
        })
    }

    #[test]
    fn render_has_user_cot_card_feed_in_order() {
        let view = view(&[]);
        let kinds: Vec<&str> = view
            .messages
            .iter()
            .map(|message| match message {
                PreviewMessage::User { .. } => "user",
                PreviewMessage::Cot { .. } => "cot",
                PreviewMessage::Card { .. } => "card",
                PreviewMessage::Feed { .. } => "feed",
            })
            .collect();
        assert_eq!(kinds, ["user", "cot", "card", "feed"]);
        let ids: Vec<&str> = view.messages.iter().map(PreviewMessage::id).collect();
        assert_eq!(
            ids,
            [
                USER_MESSAGE_ID,
                COT_MESSAGE_ID,
                CARD_MESSAGE_ID,
                FEED_MESSAGE_ID
            ]
        );
        assert_eq!(view.title, "按整机的设置");
        let (title, items) = cot_of(&view);
        assert_eq!(title, scenario().task);
        assert_eq!(items.len(), 2, "出厂:两段思考,工具详情关: {items:?}");
    }

    #[test]
    fn every_card_element_has_element_id() {
        for settings in every_combination() {
            let messaging = builtin_messaging();
            let view = render(&messaging, &settings, "");
            let card = card_of(&view);
            let all = elements(card);
            assert!(!all.is_empty());
            let mut ids: Vec<&str> = all
                .iter()
                .map(|element| {
                    element["element_id"]
                        .as_str()
                        .unwrap_or_else(|| panic!("元素缺 element_id: {element}"))
                })
                .collect();
            let total = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), total, "element_id 要唯一: {card}");
        }
    }

    #[test]
    fn element_ids_are_stable_paths() {
        let view = view(&[]);
        let ids: Vec<&str> = elements(card_of(&view))
            .iter()
            .map(|element| element["element_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "reply.0",
                "reply.1",
                "reply.2",
                "preview_files",
                "preview_files.0",
                "preview_files.0.0"
            ]
        );
    }

    #[test]
    fn cot_hidden_leaves_only_run_markers() {
        let messaging = builtin_messaging();
        let settings = settings_with(&messaging, &[(SettingKey::CotVisible, false)]);
        let gate = GateSettings::from(&messaging, &settings);
        let events = project_cot(&gate, &scenario_events(&settings));
        let kinds: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(kinds, ["RUN_STARTED", "RUN_FINISHED"]);
        let view = view(&[(SettingKey::CotVisible, false)]);
        let (title, items) = cot_of(&view);
        assert!(items.is_empty());
        assert_eq!(title, "处理请求", "过程不外显时不挂用户原话");
    }

    #[test]
    fn thinking_off_drops_thinking_items() {
        let shown = view(&[(SettingKey::CotToolDetails, true)]);
        let hidden = view(&[
            (SettingKey::CotToolDetails, true),
            (SettingKey::CotThinking, false),
        ]);
        let count = |items: &[CotItem]| {
            items
                .iter()
                .filter(|item| matches!(item, CotItem::Thinking { .. }))
                .count()
        };
        assert_eq!(count(cot_of(&shown).1), 2);
        assert_eq!(count(cot_of(&hidden).1), 0);
        assert_eq!(cot_of(&hidden).1.len(), 2, "工具行还在");
    }

    #[test]
    fn tools_on_adds_tool_items() {
        let off = view(&[]);
        let on = view(&[(SettingKey::CotToolDetails, true)]);
        assert!(cot_of(&off)
            .1
            .iter()
            .all(|item| !matches!(item, CotItem::Tool { .. })));
        let tools: Vec<&CotItem> = cot_of(&on)
            .1
            .iter()
            .filter(|item| matches!(item, CotItem::Tool { .. }))
            .collect();
        assert_eq!(tools.len(), 2);
        let CotItem::Tool {
            icon,
            name,
            args,
            result,
            ..
        } = tools[0]
        else {
            unreachable!()
        };
        assert_eq!(icon, "bash");
        assert_eq!(name, "cargo test -p app");
        assert_eq!(args.as_deref(), Some("cargo test -p app"));
        assert!(result.as_deref().unwrap().contains("FAILED"));
        let CotItem::Tool { icon, result, .. } = tools[1] else {
            unreachable!()
        };
        assert_eq!(icon, "read");
        assert!(result.as_deref().unwrap().contains("ParseError::Empty"));
        // 思考与工具按发生的先后交替。
        let order: Vec<&str> = cot_of(&on).1.iter().map(CotItem::id).collect();
        assert_eq!(
            order,
            [
                "cot.thinking-1",
                "cot.tool-1",
                "cot.thinking-2",
                "cot.tool-2"
            ]
        );
    }

    fn footer(view: &PreviewView) -> String {
        elements(card_of(view))
            .iter()
            .find(|element| element["element_id"] == "reply.2")
            .and_then(|element| element["content"].as_str())
            .unwrap()
            .to_string()
    }

    #[test]
    fn effort_badge_off_removes_badge() {
        let on = footer(&view(&[]));
        let off = footer(&view(&[(SettingKey::BadgeEffort, false)]));
        assert!(on.contains("Opus 5 · high ·"), "{on}");
        assert!(!off.contains("high"), "{off}");
        assert!(off.contains(scenario().timestamp), "{off}");
    }

    #[test]
    fn feed_off_uses_body_head() {
        assert_eq!(feed_of(&view(&[])), scenario().feed);
        let off = view(&[(SettingKey::AgentFeed, false)]);
        assert!(
            feed_of(&off).starts_with("CI 挂在 app 的单测"),
            "{}",
            feed_of(&off)
        );
        let body = elements(card_of(&off))[0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        let on_body = elements(card_of(&view(&[])))[0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(body, on_body, "标签不进正文,正文与开关无关");
    }

    #[test]
    fn reactions_off_removes_semantic_reaction() {
        let messaging = builtin_messaging();
        assert_eq!(
            reactions_of(&view(&[])),
            [
                scenario().reaction.to_string(),
                messaging.done_reaction.clone()
            ]
        );
        assert_eq!(
            reactions_of(&view(&[(SettingKey::AgentReactions, false)])),
            std::slice::from_ref(&messaging.done_reaction)
        );
    }

    #[test]
    fn file_button_comes_from_the_path_in_the_reply() {
        let view = view(&[]);
        let button = elements(card_of(&view))
            .into_iter()
            .find(|element| element["tag"] == "button")
            .expect("回复里的本地文件要出按钮");
        assert_eq!(button["text"]["content"], "📄 report.md");
    }

    fn probe(key: &str) -> RegionProbe<'_> {
        RegionProbe {
            key,
            contrast: None,
        }
    }

    fn base_regions(probes: &[RegionProbe<'_>]) -> BTreeMap<String, Vec<String>> {
        let messaging = builtin_messaging();
        regions(&messaging, &settings_with(&messaging, &[]), probes)
    }

    #[test]
    fn region_of_cot_visible_is_cot_message() {
        let regions = base_regions(&[probe("messaging.cot_visible")]);
        assert_eq!(
            regions["messaging.cot_visible"],
            [COT_MESSAGE_ID.to_string()]
        );
    }

    #[test]
    fn region_of_badge_effort_is_badge_element() {
        let regions = base_regions(&[probe("messaging.badge_effort")]);
        assert_eq!(regions["messaging.badge_effort"], ["reply.2".to_string()]);
        assert!(footer(&view(&[])).contains(scenario().effort));
    }

    #[test]
    fn regions_of_other_visible_switches_match_what_they_control() {
        let regions = base_regions(&[
            probe("messaging.hide_thinking"),
            probe("messaging.cot_tool_details"),
            probe("messaging.agent_feed"),
            probe("messaging.agent_reactions"),
            RegionProbe {
                key: "messaging.done_reaction",
                contrast: Some(Value::String("OK".into())),
            },
        ]);
        assert_eq!(
            regions["messaging.hide_thinking"],
            ["cot.thinking-1", "cot.thinking-2"]
        );
        // 出厂工具详情关:打开后多出来的工具行归到过程消息。
        assert_eq!(regions["messaging.cot_tool_details"], ["cot"]);
        assert_eq!(regions["messaging.agent_feed"], [FEED_MESSAGE_ID]);
        assert_eq!(regions["messaging.agent_reactions"], [USER_MESSAGE_ID]);
        assert_eq!(regions["messaging.done_reaction"], [USER_MESSAGE_ID]);
    }

    #[test]
    fn field_without_visual_effect_has_no_region() {
        let regions = base_regions(&[
            probe("messaging.tool_trail"),
            probe("messaging.completion_summary_card"),
            probe("messaging.timer_idle_retire_reset"),
            probe("messaging.mask_email"),
            RegionProbe {
                key: "messaging.progress_max_steps",
                contrast: Some(Value::from(9)),
            },
            RegionProbe {
                key: "messaging.working_reaction",
                contrast: Some(Value::String("HEART".into())),
            },
            probe("runtime.cli"),
            probe("messaging.no_such_field"),
        ]);
        assert!(regions.is_empty(), "{regions:?}");
    }

    #[test]
    fn regions_flip_the_chat_level_value_on_a_chat_page() {
        // 本群关了思考:对照是「打开」,多出来的思考条目归到过程消息。
        let messaging = builtin_messaging();
        let settings = settings_with(&messaging, &[(SettingKey::CotThinking, false)]);
        let regions = regions(&messaging, &settings, &[probe("messaging.hide_thinking")]);
        assert_eq!(regions["messaging.hide_thinking"], [COT_MESSAGE_ID]);
    }

    #[test]
    fn with_value_changes_only_the_named_setting() {
        let messaging = builtin_messaging();
        let settings = settings_with(&messaging, &[(SettingKey::BadgeEffort, false)]);
        let (next, resolved) = with_value(
            &messaging,
            &settings,
            "messaging.hide_thinking",
            &Value::Bool(true),
        )
        .unwrap();
        assert!(next.hide_thinking);
        assert!(resolved.hide_thinking());
        assert!(!resolved.badge_effort(), "本群别的覆盖保持原样");
        assert!(with_value(
            &messaging,
            &settings,
            "messaging.cot_visible",
            &Value::from("x")
        )
        .is_none());
        assert!(with_value(&messaging, &settings, "runtime.cli", &Value::Bool(true)).is_none());
    }

    #[test]
    fn preview_view_serializes_to_the_dashboard_contract() {
        let json = serde_json::to_value(view(&[(SettingKey::CotToolDetails, true)])).unwrap();
        let messages = json["messages"].as_array().unwrap();
        assert_eq!(messages[0]["kind"], "user");
        assert!(messages[0]["reactions"].is_array());
        assert_eq!(messages[1]["kind"], "cot");
        assert_eq!(messages[1]["status"], "done");
        let items = messages[1]["items"].as_array().unwrap();
        assert_eq!(items[0]["type"], "thinking");
        assert_eq!(items[1]["type"], "tool");
        assert!(items[1]["args"].is_string());
        assert_eq!(messages[2]["kind"], "card");
        assert_eq!(messages[2]["card"]["schema"], "2.0");
        assert_eq!(messages[3]["kind"], "feed");
        assert_eq!(messages[3]["chat_name"], scenario().chat_name);
        assert!(json["regions"].is_object());
        let back: PreviewView = serde_json::from_value(json).unwrap();
        assert_eq!(back.messages.len(), 4);
    }

    /// 渲染预览的调用链上没有任何联网、读盘、起进程的入口:产出只取决于入参。
    #[test]
    fn render_makes_no_network_calls() {
        let sources = [
            include_str!("preview.rs"),
            include_str!("../messaging/turn_gate.rs"),
            include_str!("../messaging/reply_card.rs"),
        ];
        let forbidden = [
            concat!("std::", "net"),
            concat!("std::", "fs"),
            concat!("std::", "process"),
            concat!("Tcp", "Stream"),
            concat!("Command", "::new"),
            concat!("req", "west"),
            concat!("ure", "q::"),
        ];
        for source in sources {
            let production = source.split(concat!("#[cfg(", "test)]")).next().unwrap();
            for token in forbidden {
                assert!(!production.contains(token), "预览链路出现了 {token}");
            }
        }
        // summon-core 本身不依赖任何 HTTP 客户端,预览也就拿不到。
        let manifest = include_str!("../../Cargo.toml");
        for client in ["reqwest", "ureq", "hyper", "isahc", "curl"] {
            assert!(!manifest.contains(client), "summon-core 依赖了 {client}");
        }
        // 同一组入参,两次渲染逐字节相同。
        assert_eq!(view(&[]), view(&[]));
    }

    /// 把可按群设置的布尔开关的全部组合各渲染一次,出现过的卡片元素必须都在前端支持的集合里;
    /// 出现过的集合写进前端的契约文件,前端测试逐个确认能画。
    #[test]
    fn card_tags_are_within_supported_set() {
        let messaging = builtin_messaging();
        let mut seen: Vec<String> = Vec::new();
        for settings in every_combination() {
            let view = render(&messaging, &settings, "");
            for element in elements(card_of(&view)) {
                let tag = element["tag"].as_str().unwrap_or_default().to_string();
                if !seen.contains(&tag) {
                    seen.push(tag);
                }
            }
        }
        seen.sort_unstable();
        for tag in &seen {
            assert!(
                SUPPORTED_CARD_TAGS.contains(&tag.as_str()),
                "回复卡出现了预览不支持的元素 `{tag}`:先在前端渲染组件里支持它,再加进 SUPPORTED_CARD_TAGS"
            );
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dashboard/src/preview/supported-tags.json");
        let text = format!("{}\n", serde_json::to_string_pretty(&seen).unwrap());
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
}
