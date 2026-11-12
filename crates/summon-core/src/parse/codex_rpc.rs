//! codex `app-server`(JSON-RPC over stdio)的通知模型 + 到 `AgentMessage` 的映射。纯数据 / 纯函数,
//! 零 IO —— 进程编排、握手、ServerRequest 应答留在 `summon` 侧的客户端。
//!
//! 与 `stream_json::parse_codex_line`(`codex exec --json`)是**同一语义的两条通道**:同一轮对话
//! 经两条路解析出的 `AgentMessage` 序列必须语义等价(kind 序列 / 拼接正文 / 工具名)。这不是洁癖:
//! (`runtime.capture: exec` 曾是 RPC 路的逃生门,2026-08-20 与 `Subprocess` 一起删;两条路语义一漂,卡片形态
//! 就变了。本文件末尾的一致性测试拿两份 fixture 逐项对照钉死这条不变量。
//!
//! **正文只认 delta 流 + 完成快照**这一对约定是有来源的(见 `messaging/segment.rs` 与
//! `capture/subprocess.rs` 的归并规则),各分支上都写了 why,改之前先读那两处。

use crate::codex_quota::{CodexRateLimitSnapshot, CodexRateLimitWindow};
use crate::parse::agent_msg::{
    AgentErrorKind, AgentKind, AgentMessage, ContentStreamSource, Usage,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// 通知解析失败:只描述「哪个方法的哪个字段不兼容」。
///
/// core 不掺进程 / IO 语义,所以不复用 summon 侧那个带 `Spawn(io::Error)` 的客户端错误枚举;
/// summon 侧再把它映射成 `CodexAppServerError::InvalidResponse`,对外错误面与调用点都不变。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", summon_i18n::format!("ra78ded9058", operation = .operation, detail = .detail))]
pub struct CodexRpcParseError {
    pub operation: String,
    pub detail: String,
}

impl CodexRpcParseError {
    fn new(operation: &str, detail: impl Into<String>) -> Self {
        Self {
            operation: operation.to_string(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAppServerTurn {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub items: Vec<Value>,
    #[serde(default)]
    pub error: Option<CodexAppServerTurnError>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub completed_at: Option<i64>,
}

impl CodexAppServerTurn {
    pub fn has_client_user_message_id(&self, expected: &str) -> bool {
        self.items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("userMessage")
                && item.get("clientId").and_then(Value::as_str) == Some(expected)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAppServerTurnError {
    pub message: String,
    #[serde(default)]
    pub additional_details: Option<String>,
    #[serde(default)]
    pub codex_error_info: Option<Value>,
}

impl CodexAppServerTurnError {
    pub fn is_usage_limit_exceeded(&self) -> bool {
        self.codex_error_info.as_ref().is_some_and(|value| {
            value
                .as_str()
                .is_some_and(|s| matches!(s, "usageLimitExceeded" | "usage_limit_exceeded"))
        })
    }
}

/// `TokenUsageBreakdown` 的用得上那几项(schema 里字段随版本增删,只挑映射得进 [`Usage`] 的)。
///
/// 存在的理由是**跨通知的状态 join**:`turn/completed` 的 payload 不带 usage(exec 的
/// `turn.completed` 带),usage 由 `thread/tokenUsage/updated` 单独送达。纯映射函数不做跨条 join,
/// 所以这里只负责把数字原样带出来,引擎记住最后一份、终态时挂上去。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RpcTokenBreakdown {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl RpcTokenBreakdown {
    /// 映射到通用 [`Usage`]。字段对应关系与 `stream_json::parse_usage` 对 codex exec 的处置一致:
    /// `cachedInputTokens` → `cache_read`(codex exec 侧叫 `cached_input_tokens`),
    /// `cacheWriteInputTokens` → `cache_write`(exec 侧压根没有这个字段,恒 0;RPC 给了就用)。
    pub fn to_usage(self) -> Usage {
        Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read: self.cached_input_tokens,
            cache_write: self.cache_write_input_tokens,
        }
    }

    /// 这一次模型请求的用量,换成统一口径:OpenAI 口径的 `inputTokens` 含缓存读写,未命中缓存的
    /// 输入要减掉;推理 token 已含在 `outputTokens` 里(实测 `totalTokens = inputTokens + outputTokens`)。
    pub fn to_request_usage(
        self,
        id: String,
        model: Option<String>,
        variant: Option<String>,
    ) -> crate::parse::agent_msg::RequestUsage {
        crate::parse::agent_msg::RequestUsage {
            id,
            model,
            input: self
                .input_tokens
                .saturating_sub(self.cached_input_tokens)
                .saturating_sub(self.cache_write_input_tokens),
            output: self.output_tokens,
            cache_read: self.cached_input_tokens,
            cache_write_5m: self.cache_write_input_tokens,
            cache_write_1h: 0,
            variant,
        }
    }
}

fn parse_token_breakdown(value: &Value) -> Option<RpcTokenBreakdown> {
    let obj = value.as_object()?;
    let field = |key: &str| obj.get(key).and_then(Value::as_u64).unwrap_or(0);
    Some(RpcTokenBreakdown {
        input_tokens: field("inputTokens"),
        cached_input_tokens: field("cachedInputTokens"),
        cache_write_input_tokens: field("cacheWriteInputTokens"),
        output_tokens: field("outputTokens"),
        total_tokens: field("totalTokens"),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodexAppServerNotification {
    TurnCompleted {
        thread_id: String,
        turn: CodexAppServerTurn,
    },
    Error {
        thread_id: String,
        turn_id: String,
        will_retry: bool,
        error: CodexAppServerTurnError,
    },
    RateLimitsUpdated(CodexRateLimitSnapshot),
    /// 一轮开始。`turn.id` 是 `turn/steer` / `turn/interrupt` 要的 `expectedTurnId`/`turnId`。
    TurnStarted {
        thread_id: String,
        turn: CodexAppServerTurn,
    },
    /// 助手正文增量(`item/agentMessage/delta`)。这是流式打字机的来源。
    AgentMessageDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    /// 思考增量。summary 是可公开摘要，content 才是 provider 可选提供的明文推理细节；两者
    /// 必须区分，否则无法做到「无细节时合并标题，有细节时完整渲染」。
    ReasoningDelta {
        thread_id: String,
        turn_id: String,
        /// 所属 reasoning item(`rs_…`)。缺省空串 = 上游没给,下游退回文本规则。
        item_id: String,
        /// true=`summaryTextDelta`；false=`textDelta`。
        summary: bool,
        /// summaryIndex 或 contentIndex。旧版本若缺字段则 None，仍按 item 粒度兜底。
        part_index: Option<u64>,
        delta: String,
    },
    /// 当前 turn 的计划快照。它不是助手正文，也不是一次有始有终的工具调用；下游把它记成
    /// system 事件供 replay / tail 审计，不投进回复段。
    PlanUpdated {
        thread_id: String,
        turn_id: String,
        plan: Vec<CodexTurnPlanStep>,
        explanation: Option<String>,
    },
    /// item 开始 / 结束。`item.type` 区分 userMessage / agentMessage / commandExecution /
    /// fileChange / mcpToolCall…… 原样透传 `Value`:协议仍在演进,这里不替上层做取舍。
    ItemStarted {
        thread_id: String,
        turn_id: String,
        item: Value,
    },
    ItemCompleted {
        thread_id: String,
        turn_id: String,
        item: Value,
    },
    /// token 用量(`thread/tokenUsage/updated`)。`total_*` 是**整个 thread 的累计**,
    /// `last` 是**刚跑完那一轮**的拆分 —— 两个口径别混:`Result.usage` 要的是后者
    /// (`codex exec --json` 的 `turn.completed.usage` 同为「这一轮」,拿累计会随会话越滚越大)。
    TokenUsage {
        thread_id: String,
        turn_id: Option<String>,
        total_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        model_context_window: Option<u64>,
        last: Option<RpcTokenBreakdown>,
    },
    Other {
        method: String,
        params: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTurnPlanStep {
    pub step: String,
    pub status: CodexTurnPlanStepStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexTurnPlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

/// Provider 明确标注的最终答复独立于 commentary；缺失 phase 的模型保留累积结果兼容语义。
pub fn final_answer_text(notification: &CodexAppServerNotification) -> Option<&str> {
    let CodexAppServerNotification::ItemCompleted { item, .. } = notification else {
        return None;
    };
    if normalized_item_type(item).as_deref() != Some("agent_message")
        || item.get("phase").and_then(Value::as_str) != Some("final_answer")
    {
        return None;
    }
    item.get("text").and_then(Value::as_str)
}

/// 把一条 RPC 通知映射成 0..N 条 `AgentMessage`,语义对齐 `parse_codex_line`。
///
/// 「0 条」是常态而非异常:RPC 比 `codex exec --json` 多出一批**控制面**通知(turn/started、
/// tokenUsage、rateLimits、未知方法),它们在 exec 那条路上根本不存在,产事件反而会让两条流发散。
/// 逐条依据写在各分支上。
///
/// **一条已知缺口**:`turn/completed` 的 payload 不带 usage(exec 的 `turn.completed` 带),
/// usage 走 `thread/tokenUsage/updated` 单独送达 —— 这是**跨通知的状态**,纯函数不做 join。
/// 引擎(RPC 捕获层)要自己记住最后一次 TokenUsage 再挂到终态上。
pub fn rpc_notification_to_agent_messages(n: &CodexAppServerNotification) -> Vec<AgentMessage> {
    match n {
        // 正文增量:`partial=true` = 「同一段的 token 增量,须原地拼接」(与 cursor delta 同语义),
        // 消费方是 `segment::push_segment` / `subprocess::merge_segment`。
        CodexAppServerNotification::AgentMessageDelta { item_id, delta, .. } => {
            text_event_with_source(
                AgentKind::Assistant,
                delta,
                true,
                ContentStreamSource::CodexRpcDelta,
                Some(item_id),
            )
        }
        // summary 只是可公开标题，整轮归到一个 UI 块；textDelta 才是明文细节，按 content part
        // 单独保留。两者的 source phase 不同，下游据此选择 grouped / ordinary 归并。
        CodexAppServerNotification::ReasoningDelta {
            item_id,
            summary,
            part_index,
            delta,
            ..
        } => {
            let (source, item_key) = if *summary {
                (
                    ContentStreamSource::CodexRpcSummaryDelta,
                    reasoning_part_key(item_id, "summary", *part_index),
                )
            } else {
                (
                    ContentStreamSource::CodexRpcDelta,
                    reasoning_part_key(item_id, "content", *part_index),
                )
            };
            text_event_with_source(
                AgentKind::Thinking,
                delta,
                true,
                source,
                item_key.as_deref(),
            )
        }
        CodexAppServerNotification::PlanUpdated {
            plan, explanation, ..
        } => vec![plan_updated_event(plan, explanation.as_deref())],
        CodexAppServerNotification::ItemStarted { item, .. } => item_started_events(item),
        CodexAppServerNotification::ItemCompleted { item, .. } => item_completed_events(item),
        // turn 终态只带结局；结果正文由已观察的 final_answer 或兼容的累积正文提供。
        CodexAppServerNotification::TurnCompleted { turn, .. } => match turn.error.as_ref() {
            Some(error) => vec![codex_error_event(error)],
            None => vec![AgentMessage::of(AgentKind::Result)],
        },
        // provider 侧还会自己重试的错误不当失败上报,否则一次可恢复的 429 就把本轮判死。
        CodexAppServerNotification::Error {
            will_retry, error, ..
        } => {
            if *will_retry {
                return vec![];
            }
            vec![codex_error_event(error)]
        }
        // 以下都**无对应产出**,逐条依据:
        // - TurnStarted:exec 的 `turn.started` 在 `parse_codex_line` 里就是噪音(产事件会多一段空块)。
        // - TokenUsage:exec 无对应事件;usage 由引擎 join 到终态(见函数头)。
        // - RateLimitsUpdated:配额面板的量,不属于一轮对话的内容流。
        // - Other:只有低频且影响用户判断的 notice 记成 system 事件；未知/高频方法继续忽略。
        CodexAppServerNotification::TurnStarted { .. }
        | CodexAppServerNotification::TokenUsage { .. }
        | CodexAppServerNotification::RateLimitsUpdated(_) => vec![],
        CodexAppServerNotification::Other { .. } => notable_notice(n)
            .map(|text| text_event(AgentKind::System, &text, false))
            .unwrap_or_default(),
    }
}

fn plan_updated_event(plan: &[CodexTurnPlanStep], explanation: Option<&str>) -> AgentMessage {
    const MAX_VISIBLE_STEPS: usize = 8;
    const MAX_STEP_CHARS: usize = 120;

    let mut lines = vec![summon_i18n::text!("raada0e42f0").to_string()];
    if let Some(explanation) = explanation.filter(|text| !text.trim().is_empty()) {
        lines.push(explanation.trim().chars().take(256).collect());
    }
    for step in plan.iter().take(MAX_VISIBLE_STEPS) {
        let status = match step.status {
            CodexTurnPlanStepStatus::Pending => summon_i18n::text!("r999a459c3f"),
            CodexTurnPlanStepStatus::InProgress => summon_i18n::text!("rdc9591e56d"),
            CodexTurnPlanStepStatus::Completed => summon_i18n::text!("rf28461bb49"),
        };
        let text: String = step.step.trim().chars().take(MAX_STEP_CHARS).collect();
        lines.push(format!("[{status}] {text}"));
    }
    if plan.len() > MAX_VISIBLE_STEPS {
        lines.push(summon_i18n::format!(
            "r35124f210d",
            plan.len() - MAX_VISIBLE_STEPS
        ));
    }

    let mut event = AgentMessage::of(AgentKind::System);
    event.text = Some(lines.join("\n"));
    event.tool = Some("plan".to_string());
    event.tool_input = Some(json!({"plan": plan.iter().map(|step| json!({
        "step": step.step,
        "status": match step.status {
            CodexTurnPlanStepStatus::Pending => "pending",
            CodexTurnPlanStepStatus::InProgress => "inProgress",
            CodexTurnPlanStepStatus::Completed => "completed",
        }
    })).collect::<Vec<_>>(), "explanation": explanation}));
    event
}

/// 未映射成事件的通知里,**哪些值得写进日志**,以及写成什么。
///
/// 背景:`parse_notification` 只认 8 种方法,其余一律折进 [`CodexAppServerNotification::Other`]
/// 然后产 0 条事件 —— 这个「容忍未知方法」的策略本身是对的(协议在演进),但代价是
/// **连一行日志都没有**。实测 codex 0.147 的 `ServerNotification` 共 70 个方法,
/// 也就是说有 62 种信号是完全不可见的,其中包括 codex 主动告诉你「配置有问题」
/// (`configWarning`)、「接口要废弃」(`deprecationNotice`)、「模型被换了」(`model/rerouted`)。
/// summon 有过「重启后模型悄悄换了」的教训,而 `model/rerouted` 恰恰就是那件事的权威信号。
///
/// 返回 `Some` = 值得记一行并落一条 system 事件;`None` = 高频噪音或纯控制面
/// (outputDelta / diff / partAdded…),记了只会淹掉真正的信号。system 事件只进审计流，
/// 不进入助手正文与卡片段。
pub fn notable_notice(notification: &CodexAppServerNotification) -> Option<String> {
    let CodexAppServerNotification::Other { method, params } = notification else {
        return None;
    };
    let field = |key: &str| params.get(key).and_then(Value::as_str).unwrap_or("?");
    match method.as_str() {
        // 模型被换掉了。三个字段都带上 —— 只说「被换了」没法判断要不要管。
        "model/rerouted" => Some(summon_i18n::format!(
            "rf54e21d159",
            field("fromModel"),
            field("toModel"),
            field("reason")
        )),
        // 面向用户的告警,codex 自己判定「简明且该让人看到」。
        "warning" | "guardianWarning" => {
            Some(summon_i18n::format!("r392ca6f789", field("message")))
        }
        // 配置 / 废弃类:面向搭建者,一个会话里可能重复,由调用方按方法名去重。
        "configWarning" => Some(summon_i18n::format!("r29d2ac1e42", field("message"))),
        "deprecationNotice" => Some(summon_i18n::format!("r6703e6b007", field("message"))),
        // 上下文被自动压缩。它同时也会以 `contextCompaction` item 的形态过一遍 item 流,
        // 但那条只够渲一张工具卡,说不出「什么时候压的」。
        "thread/compacted" => Some(summon_i18n::text!("rec14f7a8fa").to_string()),
        // 模型验证 / 安全缓冲:低频且会影响输出,值得留痕。
        "model/verification" => Some(summon_i18n::text!("r11d25ba52a").to_string()),
        _ => None,
    }
}

fn codex_error_event(error: &CodexAppServerTurnError) -> AgentMessage {
    let mut message = AgentMessage::of(AgentKind::Error);
    message.error = Some(match &error.additional_details {
        Some(detail) if !detail.trim().is_empty() => {
            format!("{}: {}", error.message, detail.trim())
        }
        _ => error.message.clone(),
    });
    if error.is_usage_limit_exceeded() {
        message.error_kind = Some(AgentErrorKind::UsageLimitExceeded);
    }
    message
}

fn text_event(kind: AgentKind, text: &str, partial: bool) -> Vec<AgentMessage> {
    if text.is_empty() {
        return vec![]; // 空增量不产事件:空块会在卡上留空段
    }
    let mut m = AgentMessage::of(kind);
    m.text = Some(text.to_string());
    m.partial = partial;
    vec![m]
}

fn text_event_with_source(
    kind: AgentKind,
    text: &str,
    partial: bool,
    source: ContentStreamSource,
    item_id: Option<&str>,
) -> Vec<AgentMessage> {
    let mut events = text_event(kind, text, partial);
    for event in &mut events {
        event.stream_source = Some(source);
        event.source_item_id = item_id.map(str::to_string);
    }
    events
}

/// 工具项开始 → ToolUse(运行中信号,驱动「🔧 执行 X 中…」)。内容项(正文 / 思考 / 用户回显)
/// 的 started 无正文,忽略 —— 与 `parse_codex_line` 的取舍逐条一致。
fn item_started_events(item: &Value) -> Vec<AgentMessage> {
    let Some(item_type) = normalized_item_type(item) else {
        return vec![];
    };
    if is_content_item(&item_type) {
        return vec![];
    }
    let mut m = AgentMessage::of(AgentKind::ToolUse);
    m.tool_id = item.get("id").and_then(Value::as_str).map(str::to_string);
    match item_type.as_str() {
        // 带出明细供卡片渲具体命令(形如 `/bin/zsh -lc '<cmd>'`),同 exec 路。
        "command_execution" => {
            m.tool_input = item.get("command").cloned();
            m.tool = Some(item_type);
        }
        // 动态工具是会话发起方注册的(summon 的 `summon.preview` 等),item 类型名对所有调用都
        // 一样;按被调工具命名,入参交给下游按 `path` 等字段取展示目标,与 claude 工具同一条路。
        "dynamic_tool_call" => {
            m.tool = Some(dynamic_tool_name(item).unwrap_or(item_type));
            m.tool_input = item.get("arguments").cloned();
        }
        // 其余 item 的展示对象就在 item 自己的字段里(`query`、`changes`、mcpToolCall 的
        // `server`/`tool`……),按类型取哪个由 `segment::tool_title` 决定。
        _ => {
            m.tool_input = Some(item.clone());
            m.tool = Some(item_type);
        }
    }
    vec![m]
}

fn dynamic_tool_name(item: &Value) -> Option<String> {
    let tool = item.get("tool").and_then(Value::as_str)?;
    Some(
        match item
            .get("namespace")
            .and_then(Value::as_str)
            .filter(|ns| !ns.is_empty())
        {
            Some(namespace) => format!("{namespace}.{tool}"),
            None => tool.to_string(),
        },
    )
}

fn item_completed_events(item: &Value) -> Vec<AgentMessage> {
    let Some(item_type) = normalized_item_type(item) else {
        return vec![];
    };
    match item_type.as_str() {
        // 正文完成快照。看着像和 delta 流重复,其实是**段终止符**:`partial=false` 才让
        // `subprocess` 把当前段定稿(否则一轮里两段回复会粘成一坨);而 `merge_segment` /
        // `push_segment` 对「完全重复 / 前缀增长」的快照本来就去重,不会渲两遍。
        // 顺带兜住「某个版本不发 delta」的退化:正文仍完整。
        "agent_message" => text_event_with_source(
            AgentKind::Assistant,
            item.get("text").and_then(Value::as_str).unwrap_or_default(),
            false,
            ContentStreamSource::CodexRpcCompleted,
            item.get("id").and_then(Value::as_str),
        ),
        // 思考完成快照。**RPC 的 reasoning item 没有 `text` 字段**,正文在 `summary: [...]` 数组里
        // ——每个元素就是一个 summary part,与 delta 的 `summaryIndex` 一一对应(实测 codex 0.146.0:
        // `{"type":"reasoning","id":"rs_…","summary":["**…**"],"content":[]}`)。此前这里读 `text`
        // 恒取不到,整条兜底其实是死代码;按下标配回同一个段 key 后,它才真正兜住「某个版本不发
        // delta」的退化,且有 delta 时逐段判重复直接忽略。
        "reasoning" => reasoning_completed_events(item),
        // 用户消息回显是 RPC 独有的(exec 流里没有这个 item)。不在这里挡掉,它就会被当成工具
        // 渲出一张「🔧 执行 user_message 中…」的卡。
        "user_message" => vec![],
        // 其余一律工具项完成 → ToolResult(结束信号,与 started 的 ToolUse 配对关掉运行指示)。
        // 未知 item.type 也走这里:与 `parse_codex_line` 对 exec 的处置一致(它把所有非文本项
        // 当工具项),宁可多一张工具卡,也不让两条通道对同一份内容给出不同的事件序列。
        _ => {
            let mut message = AgentMessage::of(AgentKind::ToolResult);
            message.tool_id = item.get("id").and_then(Value::as_str).map(str::to_string);
            message.tool_result = codex_tool_result(item);
            message.tool_is_error = item
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
                || item
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "failed" | "error"));
            vec![message]
        }
    }
}

fn codex_tool_result(item: &Value) -> Option<Value> {
    for key in ["aggregatedOutput", "output", "rawOutput", "stdout"] {
        if let Some(value) = item.get(key).filter(|value| !value.is_null()) {
            return Some(value.clone());
        }
    }
    let mut completion = serde_json::Map::new();
    for key in ["status", "exitCode", "stderr"] {
        if let Some(value) = item.get(key).filter(|value| !value.is_null()) {
            completion.insert(key.to_string(), value.clone());
        }
    }
    (!completion.is_empty()).then_some(Value::Object(completion))
}

/// 一段 reasoning 在段序列里的稳定 key:`<itemId>#<summary|content>:<index>`。
///
/// **为什么用它而不是接 `item/reasoning/summaryPartAdded`**:那条通知确实也标了段边界,但它是
/// 一个**需要状态机**的信号(要记住「下一条 delta 属于新段」);而 `summaryIndex` 挂在每条 delta
/// 自己身上,是自描述的 —— 纯函数一条通知就能定位到段,漏收/乱序任何一条都不会把后面的内容
/// 串到错误的段里。所以 `summaryPartAdded` 继续走 `Other` 忽略,不是遗漏。
///
/// index 没有 → 同类内容在一个 item 内一段；itemId 也没有 → None，下游退回文本规则。
fn reasoning_part_key(item_id: &str, part: &str, index: Option<u64>) -> Option<String> {
    if item_id.is_empty() {
        return None;
    }
    Some(match index {
        Some(index) => format!("{item_id}#{part}:{index}"),
        None => format!("{item_id}#{part}"),
    })
}

/// `item/completed(reasoning)` → summary/content 每个 part 一条权威快照事件，key 与 delta 同源。
///
/// 当前 Codex 0.146.0 真机 `content=[]`，只有标题 summary；模型内部推理在 encrypted_content，
/// 不能也不应解密。协议一旦给明文 content（schema 明确允许），这里必须原样渲染。老形态
/// (`item.text`)也继续兜着。
fn reasoning_completed_events(item: &Value) -> Vec<AgentMessage> {
    let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    let mut events = Vec::new();
    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
        for (index, part) in summary.iter().enumerate() {
            events.extend(text_event_with_source(
                AgentKind::Thinking,
                reasoning_part_text(part),
                true,
                ContentStreamSource::CodexRpcSummaryCompleted,
                reasoning_part_key(item_id, "summary", Some(index as u64)).as_deref(),
            ));
        }
    }
    if let Some(content) = item.get("content").and_then(Value::as_array) {
        for (index, part) in content.iter().enumerate() {
            events.extend(text_event_with_source(
                AgentKind::Thinking,
                reasoning_part_text(part),
                true,
                ContentStreamSource::CodexRpcCompleted,
                reasoning_part_key(item_id, "content", Some(index as u64)).as_deref(),
            ));
        }
    }
    if events.is_empty() {
        events.extend(text_event_with_source(
            AgentKind::Thinking,
            item.get("text").and_then(Value::as_str).unwrap_or_default(),
            true,
            ContentStreamSource::CodexRpcCompleted,
            reasoning_part_key(item_id, "content", Some(0)).as_deref(),
        ));
    }
    events
}

fn reasoning_part_text(part: &Value) -> &str {
    part.as_str()
        .or_else(|| part.get("text").and_then(Value::as_str))
        .unwrap_or_default()
}

/// RPC 的 `item.type` 是 camelCase(`commandExecution`),`codex exec --json` 是 snake_case
/// (`command_execution`)。归一到 snake_case 两条通道的 `tool` 名才真正一致 —— 工具名会进
/// 卡片的运行指示(`segment::tool_label` 按 `command_execution` 匹配中文标签),不归一就会
/// 随捕获模式漂移成原始英文。
fn normalized_item_type(item: &Value) -> Option<String> {
    item.get("type")
        .and_then(Value::as_str)
        .map(camel_to_snake_case)
}

fn camel_to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        if ch.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// 「内容项」= 正文 / 思考 / 用户回显,不是工具项。判定用归一后的 snake_case 名。
fn is_content_item(snake_item_type: &str) -> bool {
    matches!(
        snake_item_type,
        "agent_message" | "reasoning" | "user_message"
    )
}

pub fn parse_notification(
    value: &Value,
) -> Result<Option<CodexAppServerNotification>, CodexRpcParseError> {
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return Ok(None);
    };
    let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
    match method {
        "turn/completed" => Ok(Some(CodexAppServerNotification::TurnCompleted {
            thread_id: required_str(&params, "threadId", method)?,
            turn: required_turn(&params, method)?,
        })),
        "turn/started" => Ok(Some(CodexAppServerNotification::TurnStarted {
            thread_id: required_str(&params, "threadId", method)?,
            turn: required_turn(&params, method)?,
        })),
        "item/agentMessage/delta" => Ok(Some(CodexAppServerNotification::AgentMessageDelta {
            thread_id: required_str(&params, "threadId", method)?,
            turn_id: required_str(&params, "turnId", method)?,
            item_id: optional_str(&params, "itemId").unwrap_or_default(),
            delta: required_str(&params, "delta", method)?,
        })),
        // summary 与 plaintext detail 同为思考，但展示语义不同，不能再丢掉方法差异。
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
            let summary = method.ends_with("summaryTextDelta");
            Ok(Some(CodexAppServerNotification::ReasoningDelta {
                thread_id: required_str(&params, "threadId", method)?,
                turn_id: required_str(&params, "turnId", method)?,
                item_id: optional_str(&params, "itemId").unwrap_or_default(),
                summary,
                part_index: params
                    .get(if summary {
                        "summaryIndex"
                    } else {
                        "contentIndex"
                    })
                    .and_then(Value::as_u64),
                delta: required_str(&params, "delta", method)?,
            }))
        }
        "turn/plan/updated" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct PlanUpdatedNotification {
                thread_id: String,
                turn_id: String,
                plan: Vec<CodexTurnPlanStep>,
                #[serde(default)]
                explanation: Option<String>,
            }
            let parsed: PlanUpdatedNotification = serde_json::from_value(params)
                .map_err(|error| CodexRpcParseError::new(method, error.to_string()))?;
            Ok(Some(CodexAppServerNotification::PlanUpdated {
                thread_id: parsed.thread_id,
                turn_id: parsed.turn_id,
                plan: parsed.plan,
                explanation: parsed.explanation,
            }))
        }
        "item/started" | "item/completed" => {
            let thread_id = required_str(&params, "threadId", method)?;
            let turn_id = required_str(&params, "turnId", method)?;
            let item = params
                .get("item")
                .cloned()
                .ok_or_else(|| CodexRpcParseError::new(method, "missing params.item"))?;
            Ok(Some(if method == "item/started" {
                CodexAppServerNotification::ItemStarted {
                    thread_id,
                    turn_id,
                    item,
                }
            } else {
                CodexAppServerNotification::ItemCompleted {
                    thread_id,
                    turn_id,
                    item,
                }
            }))
        }
        "thread/tokenUsage/updated" => {
            // `tokenUsage.{total,last}.*` 里字段随版本增删,缺字段按 0 容忍,绝不因此拆流。
            let usage = params.get("tokenUsage");
            let total = usage.and_then(|u| u.get("total"));
            Ok(Some(CodexAppServerNotification::TokenUsage {
                thread_id: required_str(&params, "threadId", method)?,
                turn_id: optional_str(&params, "turnId"),
                total_tokens: total
                    .and_then(|t| t.get("totalTokens"))
                    .and_then(Value::as_u64),
                cached_input_tokens: total
                    .and_then(|t| t.get("cachedInputTokens"))
                    .and_then(Value::as_u64),
                model_context_window: usage
                    .and_then(|u| u.get("modelContextWindow"))
                    .and_then(Value::as_u64),
                last: usage
                    .and_then(|u| u.get("last"))
                    .and_then(parse_token_breakdown),
            }))
        }
        "error" => {
            let parsed: ErrorNotification = serde_json::from_value(params)
                .map_err(|error| CodexRpcParseError::new(method, error.to_string()))?;
            Ok(Some(CodexAppServerNotification::Error {
                thread_id: parsed.thread_id,
                turn_id: parsed.turn_id,
                will_retry: parsed.will_retry,
                error: parsed.error,
            }))
        }
        "account/rateLimits/updated" => {
            let snapshot = parse_app_rate_limit_snapshot(
                params
                    .get("rateLimits")
                    .ok_or_else(|| CodexRpcParseError::new(method, "missing params.rateLimits"))?,
            );
            Ok(Some(CodexAppServerNotification::RateLimitsUpdated(
                snapshot,
            )))
        }
        _ => Ok(Some(CodexAppServerNotification::Other {
            method: method.to_string(),
            params,
        })),
    }
}

fn required_turn(params: &Value, method: &str) -> Result<CodexAppServerTurn, CodexRpcParseError> {
    let turn = params
        .get("turn")
        .cloned()
        .ok_or_else(|| CodexRpcParseError::new(method, "missing params.turn"))?;
    serde_json::from_value(turn).map_err(|error| CodexRpcParseError::new(method, error.to_string()))
}

pub fn required_str(params: &Value, key: &str, method: &str) -> Result<String, CodexRpcParseError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CodexRpcParseError::new(method, format!("missing params.{key}")))
}

pub fn optional_str(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(str::to_string)
}

/// App Server 形状的额度快照(camelCase `limitId` / `usedPercent` / `windowDurationMins`);
/// 与 CLI 输出那份不是同一个形状,不能互用。
pub fn parse_app_rate_limit_snapshot(value: &Value) -> CodexRateLimitSnapshot {
    CodexRateLimitSnapshot {
        limit_id: value
            .get("limitId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        limit_name: value
            .get("limitName")
            .and_then(Value::as_str)
            .map(str::to_string),
        primary: value.get("primary").and_then(parse_app_window),
        secondary: value.get("secondary").and_then(parse_app_window),
        rate_limit_reached_type: value
            .get("rateLimitReachedType")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

pub fn parse_app_window(value: &Value) -> Option<CodexRateLimitWindow> {
    if value.is_null() {
        return None;
    }
    Some(CodexRateLimitWindow {
        used_percent: value.get("usedPercent")?.as_f64()?,
        window_minutes: value.get("windowDurationMins").and_then(Value::as_u64),
        resets_at: value.get("resetsAt").and_then(Value::as_i64),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorNotification {
    thread_id: String,
    turn_id: String,
    will_retry: bool,
    error: CodexAppServerTurnError,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::segment::{
        push_segment_with_source_item, SegKind, SourceItemSegments, SourceItemUpdate, TurnSegment,
        CODEX_REASONING_SUMMARY_GROUP, CODEX_REASONING_SUMMARY_SEPARATOR,
    };
    use crate::parse::stream_json::parse_codex_line;

    fn rpc_events(stream: &str) -> Vec<AgentMessage> {
        let events: Vec<AgentMessage> = stream
            .lines()
            .filter(|l| !l.trim().is_empty())
            .flat_map(|line| {
                let value: Value = serde_json::from_str(line).expect("fixture 必须是合法 JSON");
                parse_notification(&value)
                    .expect("fixture 里的通知都应可解析")
                    .map(|n| rpc_notification_to_agent_messages(&n))
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            events.iter().all(|m| m.kind != AgentKind::System),
            "通知流不该产 System"
        );
        events
    }

    fn exec_events(stream: &str) -> Vec<AgentMessage> {
        stream.lines().flat_map(parse_codex_line).collect()
    }

    /// 语义骨架:折叠掉「连续同 kind 的增量」后的 kind 序列。流式切分粒度允许不同,骨架不许不同。
    fn kind_skeleton(msgs: &[AgentMessage]) -> Vec<AgentKind> {
        let mut out: Vec<AgentKind> = Vec::new();
        for m in msgs {
            // System 只在 exec 流里有(`thread.started` 带 thread_id);RPC 侧 thread id 来自
            // `thread/start` / `thread/resume` 的**响应**,不在通知流里 —— 不是语义差异,是通道差异。
            if m.kind == AgentKind::System {
                continue;
            }
            if out.last() != Some(&m.kind) {
                out.push(m.kind.clone());
            }
        }
        out
    }

    /// 用**生产同款**的段模型累积正文:有 provider item id 的 RPC 事件按 item 归并，其它流退回
    /// 文本规则。拿它折叠两条流,比手写 join 更接近真实语义。
    fn segments(msgs: &[AgentMessage]) -> Vec<TurnSegment> {
        let mut segs = Vec::new();
        let mut source_items = SourceItemSegments::default();
        for m in msgs {
            let kind = match m.kind {
                AgentKind::Assistant => SegKind::Reply,
                AgentKind::Thinking => SegKind::Thinking,
                _ => continue,
            };
            if let Some(text) = &m.text {
                let update = match (m.stream_source, m.source_item_id.as_deref()) {
                    (Some(ContentStreamSource::CodexRpcDelta), Some(item_id)) => {
                        Some(SourceItemUpdate::Delta(item_id))
                    }
                    (Some(ContentStreamSource::CodexRpcCompleted), Some(item_id)) => {
                        Some(SourceItemUpdate::Completed(item_id))
                    }
                    (Some(ContentStreamSource::CodexRpcSummaryDelta), Some(item_id)) => {
                        Some(SourceItemUpdate::GroupedDelta {
                            group_id: CODEX_REASONING_SUMMARY_GROUP,
                            item_id,
                            separator: CODEX_REASONING_SUMMARY_SEPARATOR,
                        })
                    }
                    (Some(ContentStreamSource::CodexRpcSummaryCompleted), Some(item_id)) => {
                        Some(SourceItemUpdate::GroupedCompleted {
                            group_id: CODEX_REASONING_SUMMARY_GROUP,
                            item_id,
                            separator: CODEX_REASONING_SUMMARY_SEPARATOR,
                        })
                    }
                    _ => None,
                };
                push_segment_with_source_item(
                    &mut segs,
                    &mut source_items,
                    kind,
                    text,
                    m.partial,
                    update,
                );
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

    const EXEC_PONG: &str = include_str!("../../tests/fixtures/codex-exec-pong.jsonl");
    const RPC_PONG: &str = include_str!("../../tests/fixtures/codex-rpc-pong.jsonl");
    const EXEC_TOOLUSE: &str = include_str!("../../tests/fixtures/codex-exec-tooluse.jsonl");
    const RPC_TOOLUSE: &str = include_str!("../../tests/fixtures/codex-rpc-tooluse.jsonl");

    /// 一致性断言(本文件存在的理由):同一语义的一轮,RPC 通知流与 `codex exec --json` 必须
    /// 解析出等价的事件序列。RPC fixture 按设计文档 §2 的真机序列手工构造(不是抓包原文)。
    #[test]
    fn rpc_pong_turn_matches_exec_pong_turn() {
        let exec = exec_events(EXEC_PONG);
        let rpc = rpc_events(RPC_PONG);
        assert_eq!(
            kind_skeleton(&rpc),
            kind_skeleton(&exec),
            "kind 骨架必须一致(RPC={rpc:?})"
        );
        assert_eq!(
            kind_skeleton(&exec),
            vec![AgentKind::Assistant, AgentKind::Result],
            "骨架本身也钉住:一段回复 + 一个终态"
        );
        assert_eq!(
            segments(&rpc),
            segments(&exec),
            "拼接正文必须一致:delta 流 + 完成快照不该把 pong 渲成两遍"
        );
        assert_eq!(segments(&rpc), vec![TurnSegment::Reply("pong".into())]);
        let assistant: Vec<_> = rpc
            .iter()
            .filter(|event| event.kind == AgentKind::Assistant)
            .collect();
        assert_eq!(assistant.len(), 3);
        assert_eq!(assistant[0].source_item_id.as_deref(), Some("item_1"));
        assert_eq!(assistant[1].source_item_id.as_deref(), Some("item_1"));
        assert_eq!(assistant[2].source_item_id.as_deref(), Some("item_1"));
        assert_eq!(
            assistant[0].stream_source,
            Some(ContentStreamSource::CodexRpcDelta)
        );
        assert_eq!(
            assistant[2].stream_source,
            Some(ContentStreamSource::CodexRpcCompleted)
        );
        assert!(assistant[0].partial);
        assert!(!assistant[2].partial);
    }

    #[test]
    fn rpc_tooluse_turn_matches_exec_tooluse_turn() {
        let exec = exec_events(EXEC_TOOLUSE);
        let rpc = rpc_events(RPC_TOOLUSE);
        assert_eq!(
            kind_skeleton(&rpc),
            kind_skeleton(&exec),
            "kind 骨架必须一致(RPC={rpc:?})"
        );
        assert_eq!(
            kind_skeleton(&exec),
            vec![
                AgentKind::ToolUse,
                AgentKind::ToolResult,
                AgentKind::Assistant,
                AgentKind::Result,
            ]
        );
        assert_eq!(segments(&rpc), segments(&exec));
        assert_eq!(
            segments(&rpc),
            vec![TurnSegment::Reply("There are 2 files.".into())]
        );
        // 工具名也必须一致:RPC 的 `commandExecution` 归一成 exec 的 `command_execution`,
        // 否则卡片上的中文标签(`tool_label`)会随捕获模式退回英文原名。
        assert_eq!(tools(&rpc), tools(&exec));
        assert_eq!(tools(&rpc), vec!["command_execution".to_string()]);
    }

    /// 未映射通知的可见性:值得记的要记出内容(不是光一个方法名),高频噪音不能记。
    #[test]
    fn notable_notices_carry_detail_and_noisy_ones_stay_silent() {
        let notice = |method: &str, params: Value| {
            notable_notice(&CodexAppServerNotification::Other {
                method: method.to_string(),
                params,
            })
        };

        let rerouted = notice(
            "model/rerouted",
            json!({"fromModel": "gpt-5.6-sol", "toModel": "gpt-5.2-codex", "reason": "unavailable"}),
        )
        .expect("模型被换掉必须留痕");
        assert!(rerouted.contains("gpt-5.6-sol"), "实得:{rerouted}");
        assert!(rerouted.contains("gpt-5.2-codex"), "实得:{rerouted}");
        assert!(
            rerouted.contains("unavailable"),
            "只说被换了、不说原因等于没说"
        );

        assert!(notice("warning", json!({"message": "磁盘快满了"}))
            .expect("面向用户的告警要记")
            .contains("磁盘快满了"));
        assert!(notice("thread/compacted", json!({"threadId": "t", "turnId": "u"})).is_some());

        // 高频通知记了会把真信号淹掉 —— outputDelta 是按 chunk 来的。
        for noisy in [
            "item/commandExecution/outputDelta",
            "turn/diff/updated",
            "item/reasoning/summaryPartAdded",
            "thread/status/changed",
        ] {
            assert_eq!(notice(noisy, json!({})), None, "{noisy} 不该记");
        }
    }

    #[test]
    fn plan_updates_are_typed_bounded_system_events_not_reply_text() {
        let notification = parse_notification(&json!({
            "method": "turn/plan/updated",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "explanation": "先建立协议边界",
                "plan": [
                    {"step": "冻结 schema", "status": "completed"},
                    {"step": "接入事件", "status": "inProgress"},
                    {"step": "跑回归", "status": "pending"}
                ]
            }
        }))
        .expect("合法计划通知应可解析")
        .expect("方法通知应存在");
        let CodexAppServerNotification::PlanUpdated {
            thread_id,
            turn_id,
            plan,
            ..
        } = &notification
        else {
            panic!("应解析成 PlanUpdated")
        };
        assert_eq!(thread_id, "thread-1");
        assert_eq!(turn_id, "turn-1");
        assert_eq!(plan.len(), 3);

        let events = rpc_notification_to_agent_messages(&notification);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, AgentKind::System);
        assert_eq!(events[0].tool.as_deref(), Some("plan"));
        let text = events[0].text.as_deref().expect("审计事件应有人读摘要");
        assert!(text.contains("[已完成] 冻结 schema"));
        assert!(text.contains("[进行中] 接入事件"));
        assert!(text.contains("[待处理] 跑回归"));
        assert!(segments(&events).is_empty(), "计划不能混进助手回复正文");
    }

    #[test]
    fn warnings_and_model_reroutes_become_auditable_system_events() {
        for (method, params, expected) in [
            (
                "warning",
                json!({"message": "磁盘快满了", "threadId": "t", "turnId": "u"}),
                "磁盘快满了",
            ),
            (
                "model/rerouted",
                json!({"fromModel":"a", "toModel":"b", "reason":"capacity", "threadId":"t", "turnId":"u"}),
                "a → b",
            ),
        ] {
            let notification = parse_notification(&json!({"method": method, "params": params}))
                .unwrap()
                .unwrap();
            let events = rpc_notification_to_agent_messages(&notification);
            assert_eq!(events.len(), 1, "{method} 应留下事件");
            assert_eq!(events[0].kind, AgentKind::System);
            assert!(events[0].text.as_deref().unwrap().contains(expected));
            assert!(segments(&events).is_empty(), "{method} 不能混进回复正文");
        }
    }

    /// 已经被映射成事件的通知不算「未处理」,不能重复记一遍。
    #[test]
    fn a_handled_notification_is_never_reported_as_an_unhandled_notice() {
        let handled = CodexAppServerNotification::TurnStarted {
            thread_id: "t".into(),
            turn: CodexAppServerTurn {
                id: "u".into(),
                status: "inProgress".into(),
                items: vec![],
                error: None,
                started_at: None,
                completed_at: None,
            },
        };
        assert_eq!(notable_notice(&handled), None);
    }

    /// 字段缺失不该 panic,也不该记出一条空洞的日志 —— 缺什么就显 `?`,至少方法名还在。
    #[test]
    fn a_notice_with_missing_fields_degrades_instead_of_panicking() {
        let notice = notable_notice(&CodexAppServerNotification::Other {
            method: "model/rerouted".to_string(),
            params: json!({}),
        })
        .expect("字段缺了也要记");
        assert!(notice.contains('?'), "实得:{notice}");
    }

    #[test]
    fn tool_started_carries_command_detail() {
        // exec 的 `item.started` 也带 `command`(`parse_codex_line` 专门取它渲「执行 X 中…」),
        // 只是既有 exec fixture 那行没写。这里单独钉住 RPC 侧同样带出。
        let started = rpc_events(RPC_TOOLUSE)
            .into_iter()
            .find(|m| m.kind == AgentKind::ToolUse)
            .expect("有工具开始事件");
        assert_eq!(
            started.tool_input.as_ref().and_then(Value::as_str),
            Some("ls")
        );
    }

    #[test]
    fn user_message_item_is_not_rendered_as_tool() {
        // RPC 独有的用户回显项。漏挡就会渲出一张「🔧 执行 user_message 中…」的工具卡。
        let started = parse_notification(&json!({
            "method": "item/started",
            "params": {"threadId":"t","turnId":"u","item":{"id":"i","type":"userMessage","text":"hi"}}
        }))
        .unwrap()
        .unwrap();
        assert!(rpc_notification_to_agent_messages(&started).is_empty());
        let completed = parse_notification(&json!({
            "method": "item/completed",
            "params": {"threadId":"t","turnId":"u","item":{"id":"i","type":"userMessage","text":"hi"}}
        }))
        .unwrap()
        .unwrap();
        assert!(rpc_notification_to_agent_messages(&completed).is_empty());
    }

    #[test]
    fn dynamic_tool_call_is_named_by_the_tool_it_calls() {
        let started = |item: Value| {
            let n = parse_notification(&json!({
                "method":"item/started",
                "params":{"threadId":"t","turnId":"u","item":item}
            }))
            .unwrap()
            .unwrap();
            rpc_notification_to_agent_messages(&n)
        };
        let namespaced = started(json!({
            "id":"call_1","type":"dynamicToolCall","namespace":"summon","tool":"preview",
            "arguments":{"path":"/tmp/report.md"},"status":"inProgress"
        }));
        assert_eq!(namespaced[0].tool.as_deref(), Some("summon.preview"));
        assert_eq!(
            namespaced[0].tool_input.as_ref(),
            Some(&json!({"path":"/tmp/report.md"}))
        );

        let bare = started(json!({
            "id":"call_2","type":"dynamicToolCall","namespace":null,"tool":"react",
            "arguments":{"emoji":"OK"},"status":"inProgress"
        }));
        assert_eq!(bare[0].tool.as_deref(), Some("react"));

        let empty_namespace = started(json!({
            "id":"call_3","type":"dynamicToolCall","namespace":"","tool":"react",
            "arguments":{"emoji":"OK"},"status":"inProgress"
        }));
        assert_eq!(empty_namespace[0].tool.as_deref(), Some("react"));

        let unnamed = started(json!({
            "id":"call_4","type":"dynamicToolCall","namespace":"summon",
            "arguments":{},"status":"inProgress"
        }));
        assert_eq!(unnamed[0].tool.as_deref(), Some("dynamic_tool_call"));
    }

    #[test]
    fn other_tool_items_keep_their_type_and_hand_their_own_fields_to_the_title() {
        for (item, tool) in [
            (
                json!({
                    "id":"fc_1","type":"fileChange","status":"inProgress",
                    "changes":[{"path":"/repo/a.rs","kind":"update","diff":""}]
                }),
                "file_change",
            ),
            (
                json!({
                    "id":"mcp_1","type":"mcpToolCall","server":"lark","tool":"search_docs",
                    "arguments":{"query":"周报"},"status":"inProgress"
                }),
                "mcp_tool_call",
            ),
            (
                json!({
                    "id":"ca_1","type":"collabAgentToolCall","tool":"spawnAgent",
                    "status":"inProgress"
                }),
                "collab_agent_tool_call",
            ),
        ] {
            let n = parse_notification(&json!({
                "method":"item/started",
                "params":{"threadId":"t","turnId":"u","item":item.clone()}
            }))
            .unwrap()
            .unwrap();
            let messages = rpc_notification_to_agent_messages(&n);
            assert_eq!(messages[0].tool.as_deref(), Some(tool));
            assert_eq!(messages[0].tool_input.as_ref(), Some(&item));
        }
    }

    #[test]
    fn command_completion_preserves_output_and_pairs_with_started_item() {
        let started = parse_notification(&json!({
            "method":"item/started",
            "params":{"threadId":"t","turnId":"u","item":{
                "id":"cmd_1","type":"commandExecution","command":"printf ok"
            }}
        }))
        .unwrap()
        .unwrap();
        let completed = parse_notification(&json!({
            "method":"item/completed",
            "params":{"threadId":"t","turnId":"u","item":{
                "id":"cmd_1","type":"commandExecution","aggregatedOutput":"ok", "exitCode":0
            }}
        }))
        .unwrap()
        .unwrap();
        let start_messages = rpc_notification_to_agent_messages(&started);
        let completed_messages = rpc_notification_to_agent_messages(&completed);
        assert_eq!(start_messages[0].tool_id.as_deref(), Some("cmd_1"));
        assert_eq!(completed_messages[0].tool_id.as_deref(), Some("cmd_1"));
        assert_eq!(
            completed_messages[0].tool_result.as_ref(),
            Some(&json!("ok"))
        );
    }

    #[test]
    fn reasoning_delta_and_completion_fold_into_one_thinking_segment() {
        let deltas = ["先看 X ", "再看 Y"].map(|delta| {
            parse_notification(&json!({
                "method": "item/reasoning/textDelta",
                "params": {
                    "threadId":"t","turnId":"u","itemId":"i",
                    "contentIndex":0,"delta": delta
                }
            }))
            .unwrap()
            .unwrap()
        });
        let completed = parse_notification(&json!({
            "method": "item/completed",
            "params": {"threadId":"t","turnId":"u","item":{"id":"i","type":"reasoning","text":"先看 X 再看 Y"}}
        }))
        .unwrap()
        .unwrap();
        let msgs: Vec<AgentMessage> = deltas
            .iter()
            .chain(std::iter::once(&completed))
            .flat_map(rpc_notification_to_agent_messages)
            .collect();
        assert!(msgs.iter().all(|m| m.kind == AgentKind::Thinking));
        // 与 exec 的单条 `item.completed(reasoning)` 折出同一段思考,不重复。
        assert_eq!(
            segments(&msgs),
            segments(&exec_events(
                r#"{"type":"item.completed","item":{"id":"i","type":"reasoning","text":"先看 X 再看 Y"}}"#
            ))
        );
    }

    /// owner 2026-08-10 实测:app-server 只给 reasoning summary 标题、不给明文细节时，飞书卡
    /// 一口气堆了二十多个「💭 思考」折叠块。标题要保留，但应合成一个块，避免一标题一面板。
    ///
    /// 根因是 codex 把一次思考拆成多个 `summaryIndex` 各自独立的 part 推送,而 summon 把它们
    /// 全当同一段的 token delta 拼接。fixture 取自真机(codex 0.146.0 app-server)。
    #[test]
    fn codex_summary_titles_share_one_thinking_segment() {
        let msgs: Vec<AgentMessage> = [
            (0, "**Planning chat history retrieval**"),
            (1, "**Prioritizing lark-shared must reading**"),
        ]
        .iter()
        .flat_map(|(index, text)| {
            let n = parse_notification(&json!({
                "method": "item/reasoning/summaryTextDelta",
                "params": {
                    "threadId":"t","turnId":"u","itemId":"rs_1",
                    "delta": text, "summaryIndex": index
                }
            }))
            .unwrap()
            .unwrap();
            rpc_notification_to_agent_messages(&n)
        })
        .collect();

        assert_eq!(
            segments(&msgs),
            vec![TurnSegment::Thinking(
                "**Planning chat history retrieval**\n**Prioritizing lark-shared must reading**"
                    .into()
            )],
            "只有标题时必须保留换行但共用一个思考块"
        );
    }

    /// 截图里的标题来自多个 reasoning item，而不只是同一 item 的多个 summaryIndex；整轮所有
    /// summary 标题仍应共用一个块。工具事件不会出现在 segments 中，不应打断这个归并。
    #[test]
    fn summary_titles_across_reasoning_items_still_share_one_segment() {
        let summary = |item_id: &str, text: &str| {
            let n = parse_notification(&json!({
                "method": "item/reasoning/summaryTextDelta",
                "params": {
                    "threadId":"t","turnId":"u","itemId":item_id,
                    "delta":text,"summaryIndex":0
                }
            }))
            .unwrap()
            .unwrap();
            rpc_notification_to_agent_messages(&n)
        };
        let msgs: Vec<AgentMessage> = summary("rs_1", "**第一项**")
            .into_iter()
            .chain(summary("rs_2", "**第二项**"))
            .collect();

        assert_eq!(
            segments(&msgs),
            vec![TurnSegment::Thinking("**第一项**\n**第二项**".into())]
        );
    }

    /// 同一段内的多条 token delta 仍要拼进同一段 —— 分段不能把段内也切碎。
    #[test]
    fn deltas_within_one_summary_part_still_concatenate() {
        let msgs: Vec<AgentMessage> = ["**Plan", "ning chat", " history**"]
            .iter()
            .flat_map(|delta| {
                let n = parse_notification(&json!({
                    "method": "item/reasoning/summaryTextDelta",
                    "params": {"threadId":"t","turnId":"u","itemId":"rs_1","delta":delta,"summaryIndex":0}
                }))
                .unwrap()
                .unwrap();
                rpc_notification_to_agent_messages(&n)
            })
            .collect();
        assert_eq!(
            segments(&msgs),
            vec![TurnSegment::Thinking("**Planning chat history**".into())]
        );
    }

    /// `textDelta` / completed.content 是 provider 真正给出的明文 reasoning 细节。它不能跟 summary
    /// 标题一起被压成标题列表，更不能因为「无细节时合并标题」这条展示规则而丢失。
    #[test]
    fn reasoning_plaintext_details_are_rendered_when_provider_supplies_them() {
        let summary = parse_notification(&json!({
            "method":"item/reasoning/summaryTextDelta",
            "params":{
                "threadId":"t","turnId":"u","itemId":"rs_1",
                "delta":"**检查配置**","summaryIndex":0
            }
        }))
        .unwrap()
        .unwrap();
        let detail = parse_notification(&json!({
            "method":"item/reasoning/textDelta",
            "params":{
                "threadId":"t","turnId":"u","itemId":"rs_1",
                "delta":"这里是 provider 给出的明文细节。","contentIndex":0
            }
        }))
        .unwrap()
        .unwrap();
        let completed = parse_notification(&json!({
            "method":"item/completed",
            "params":{"threadId":"t","turnId":"u","item":{
                "id":"rs_1","type":"reasoning",
                "summary":["**检查配置**"],
                "content":["这里是 provider 给出的明文细节。"]
            }}
        }))
        .unwrap()
        .unwrap();

        let msgs: Vec<AgentMessage> = rpc_notification_to_agent_messages(&summary)
            .into_iter()
            .chain(rpc_notification_to_agent_messages(&detail))
            .chain(rpc_notification_to_agent_messages(&completed))
            .collect();
        assert_eq!(
            segments(&msgs),
            vec![
                TurnSegment::Thinking("**检查配置**".into()),
                TurnSegment::Thinking("这里是 provider 给出的明文细节。".into()),
            ],
            "明文 detail 的 delta 和 completed 快照都必须渲染且去重"
        );
    }

    /// `item/completed(reasoning)` 的正文在 `summary` 数组里(RPC **没有** `text` 字段)。
    /// 它是每段的权威快照:有 delta 时逐段判重复直接忽略,绝不追加第二份。
    #[test]
    fn reasoning_completed_snapshot_matches_the_delta_segments() {
        let delta = |index: u64, text: &str| {
            let n = parse_notification(&json!({
                "method": "item/reasoning/summaryTextDelta",
                "params": {"threadId":"t","turnId":"u","itemId":"rs_1","delta":text,"summaryIndex":index}
            }))
            .unwrap()
            .unwrap();
            rpc_notification_to_agent_messages(&n)
        };
        // 真机形态:summary 元素是裸字符串。
        let completed = parse_notification(&json!({
            "method": "item/completed",
            "params": {"threadId":"t","turnId":"u","item":{
                "id":"rs_1","type":"reasoning",
                "summary":["**第一段**","**第二段**"],"content":[]
            }}
        }))
        .unwrap()
        .unwrap();

        let msgs: Vec<AgentMessage> = delta(0, "**第一段**")
            .into_iter()
            .chain(delta(1, "**第二段**"))
            .chain(rpc_notification_to_agent_messages(&completed))
            .collect();
        assert_eq!(
            segments(&msgs),
            vec![TurnSegment::Thinking("**第一段**\n**第二段**".into())],
            "快照与 delta 同源,不该多出重复段"
        );

        // 没有 delta 时(某版本只发 completed),快照自己也要把两段思考立起来。
        assert_eq!(
            segments(&rpc_notification_to_agent_messages(&completed)),
            vec![TurnSegment::Thinking("**第一段**\n**第二段**".into())]
        );
    }

    /// rollout 里 summary 元素是对象(`{"type":"summary_text","text":"…"}`)。两种形态都出现过,
    /// 认错一种就整段思考不显示 —— 这里钉死两种都吃。
    #[test]
    fn reasoning_summary_accepts_object_shaped_parts() {
        let completed = parse_notification(&json!({
            "method": "item/completed",
            "params": {"threadId":"t","turnId":"u","item":{
                "id":"rs_1","type":"reasoning",
                "summary":[{"type":"summary_text","text":"**对象形态**"}],"content":[]
            }}
        }))
        .unwrap()
        .unwrap();
        assert_eq!(
            segments(&rpc_notification_to_agent_messages(&completed)),
            vec![TurnSegment::Thinking("**对象形态**".into())]
        );
    }

    #[test]
    fn agent_message_completion_terminates_the_reply_segment() {
        // 完成快照必须 `partial=false`:它是段终止符,`subprocess` 靠它定稿当前段。
        let completed = parse_notification(&json!({
            "method": "item/completed",
            "params": {"threadId":"t","turnId":"u","item":{"id":"i","type":"agentMessage","text":"pong"}}
        }))
        .unwrap()
        .unwrap();
        let msgs = rpc_notification_to_agent_messages(&completed);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, AgentKind::Assistant);
        assert_eq!(msgs[0].text.as_deref(), Some("pong"));
        assert!(!msgs[0].partial, "完成快照是段终止符,不能标 partial");
    }

    /// usage 补口(Task 2 遗留):`last` 是这一轮的拆分,引擎据它 join 出 `Result.usage`。
    /// 与 `codex exec --json` 的 `turn.completed.usage` 逐字段同口径 —— 数字对不上,
    /// 卡片上的 token 统计就会随捕获模式漂移。
    #[test]
    fn token_usage_carries_the_per_turn_breakdown_matching_exec_usage() {
        let n = parse_notification(&json!({
            "method":"thread/tokenUsage/updated",
            "params":{"threadId":"t","turnId":"u","tokenUsage":{
                "total":{"inputTokens":99999,"cachedInputTokens":8888,"outputTokens":777,
                         "reasoningOutputTokens":0,"totalTokens":100776},
                "last":{"inputTokens":23061,"cachedInputTokens":3456,"outputTokens":21,
                        "reasoningOutputTokens":0,"totalTokens":23082},
                "modelContextWindow":258400}}
        }))
        .unwrap()
        .unwrap();
        let CodexAppServerNotification::TokenUsage {
            last,
            total_tokens,
            model_context_window,
            ..
        } = &n
        else {
            panic!("应解析成 TokenUsage");
        };
        assert_eq!(*total_tokens, Some(100776), "累计口径仍按 total");
        assert_eq!(
            *model_context_window,
            Some(258400),
            "上下文窗口必须从 app-server 通知进入状态层"
        );
        // 与 codex-exec fixture 的 result.usage 同一组数字(见 stream_json 的 codex usage 测)。
        let exec = exec_events(EXEC_PONG);
        let exec_usage = exec
            .iter()
            .find(|m| m.kind == AgentKind::Result)
            .and_then(|m| m.usage.clone());
        assert!(exec_usage.is_some(), "exec 路的终态确实带 usage");
        assert_eq!(
            last.expect("last 必须解出来").to_usage(),
            Usage {
                input_tokens: 23061,
                output_tokens: 21,
                cache_read: 3456,
                cache_write: 0,
            }
        );
        // 通知本身仍不产内容事件:join 是引擎的职责,纯映射不跨条持状态。
        assert!(rpc_notification_to_agent_messages(&n).is_empty());
    }

    /// OpenAI 口径的输入含缓存读写:未命中缓存的那部分要把两者都减掉,缓存写入单列。
    #[test]
    fn request_usage_takes_cache_reads_and_writes_out_of_the_openai_input() {
        let breakdown = RpcTokenBreakdown {
            input_tokens: 10_000,
            cached_input_tokens: 6_000,
            cache_write_input_tokens: 1_500,
            output_tokens: 300,
            total_tokens: 10_300,
        };
        let u = breakdown.to_request_usage("th-1:10300".into(), Some("gpt-x".into()), None);
        assert_eq!(u.input, 10_000 - 6_000 - 1_500);
        assert_eq!(u.cache_read, 6_000);
        assert_eq!(u.cache_write_5m, 1_500);
        assert_eq!(u.cache_write_1h, 0);
        assert_eq!(u.output, 300);
        assert_eq!(u.prompt_tokens(), 10_000);
    }

    #[test]
    fn missing_token_usage_fields_degrade_to_zero_instead_of_dropping_the_notification() {
        let n = parse_notification(&json!({
            "method":"thread/tokenUsage/updated",
            "params":{"threadId":"t","tokenUsage":{"total":{},"last":{"outputTokens":7}}}
        }))
        .unwrap()
        .unwrap();
        let CodexAppServerNotification::TokenUsage { last, .. } = &n else {
            panic!("应解析成 TokenUsage");
        };
        assert_eq!(last.map(|b| b.output_tokens), Some(7));
        assert_eq!(last.map(|b| b.input_tokens), Some(0));
    }

    #[test]
    fn control_plane_notifications_produce_nothing() {
        let cases = [
            json!({"method":"turn/started","params":{"threadId":"t","turn":{"id":"u","status":"inProgress"}}}),
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"t","tokenUsage":{"total":{"totalTokens":1}}}}),
            json!({"method":"thread/realtime/sdp","params":{"threadId":"t"}}),
            json!({"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex"}}}),
        ];
        for case in cases {
            let n = parse_notification(&case).unwrap().unwrap();
            assert!(
                rpc_notification_to_agent_messages(&n).is_empty(),
                "控制面通知不该产内容事件:{case}"
            );
        }
    }

    #[test]
    fn retryable_error_notification_is_not_reported_as_failure() {
        let make = |will_retry: bool| {
            parse_notification(&json!({
                "method":"error",
                "params":{"threadId":"t","turnId":"u","willRetry":will_retry,
                          "error":{"message":"429","additionalDetails":"retry in 3s"}}
            }))
            .unwrap()
            .unwrap()
        };
        assert!(
            rpc_notification_to_agent_messages(&make(true)).is_empty(),
            "provider 自己会重试的错误不当失败上报"
        );
        let fatal = rpc_notification_to_agent_messages(&make(false));
        assert_eq!(fatal.len(), 1);
        assert_eq!(fatal[0].kind, AgentKind::Error);
        assert_eq!(fatal[0].error.as_deref(), Some("429: retry in 3s"));
    }

    /// 生产事故回归：Codex 额度耗尽时，app-server 可能只在 `turn/completed.turn.error`
    /// 带结构化错误。把所有 turn/completed 都映成 Result 会让定时任务静默失败。
    #[test]
    fn usage_limit_in_completed_turn_is_a_structured_failure() {
        let notification = parse_notification(&json!({
            "method":"turn/completed",
            "params":{"threadId":"t","turn":{
                "id":"u",
                "status":"failed",
                "error":{"message":"localized quota text",
                         "codexErrorInfo":"usageLimitExceeded"}
            }}
        }))
        .unwrap()
        .unwrap();

        let events = rpc_notification_to_agent_messages(&notification);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, AgentKind::Error);
        assert_eq!(events[0].error.as_deref(), Some("localized quota text"));
        assert_eq!(
            events[0].error_kind,
            Some(crate::agent_msg::AgentErrorKind::UsageLimitExceeded),
            "额度判定必须来自结构化 codexErrorInfo，不能解析本地化文案"
        );
    }

    #[test]
    fn camel_item_types_normalize_to_exec_snake_case() {
        for (camel, snake) in [
            ("commandExecution", "command_execution"),
            ("mcpToolCall", "mcp_tool_call"),
            ("fileChange", "file_change"),
            ("webSearch", "web_search"),
            ("reasoning", "reasoning"),
        ] {
            assert_eq!(camel_to_snake_case(camel), snake);
        }
    }

    #[test]
    fn unknown_item_type_follows_exec_tool_convention() {
        // `parse_codex_line` 把所有非文本项都当工具项;RPC 侧同样处理,宁可多一张工具卡,
        // 也不让两条通道对同一份内容给出不同的事件序列。
        let started = parse_notification(&json!({
            "method":"item/started",
            "params":{"threadId":"t","turnId":"u","item":{"id":"i","type":"brandNewThing"}}
        }))
        .unwrap()
        .unwrap();
        let msgs = rpc_notification_to_agent_messages(&started);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].tool.as_deref(), Some("brand_new_thing"));
        let completed = parse_notification(&json!({
            "method":"item/completed",
            "params":{"threadId":"t","turnId":"u","item":{"id":"i","type":"brandNewThing"}}
        }))
        .unwrap()
        .unwrap();
        assert_eq!(
            rpc_notification_to_agent_messages(&completed)[0].kind,
            AgentKind::ToolResult
        );
    }

    #[test]
    fn malformed_item_without_type_is_ignored() {
        for method in ["item/started", "item/completed"] {
            let n = parse_notification(&json!({
                "method": method,
                "params": {"threadId":"t","turnId":"u","item":{"id":"i"}}
            }))
            .unwrap()
            .unwrap();
            assert!(rpc_notification_to_agent_messages(&n).is_empty());
        }
    }
}
