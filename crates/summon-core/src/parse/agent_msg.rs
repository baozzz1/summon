//! 各 CLI 的结构化输出归一后的统一事件。纯数据,无 IO。
//!
//! `AgentEvent`/`AgentEventKind` 是 R1 起的规范别名;历史调用点继续用 `AgentMessage`/`AgentKind`
//! 不必改——别名共享同一类型定义。`status`/`run_id`/`ts` 由 IO 源在 R2+ 阶段设置,
//! 解析器(stream_json.rs 等)保持确定性,不触碰这三个字段。

use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentKind {
    System,
    Assistant,
    Thinking,
    ToolUse,
    ToolResult,
    Result,
    Error,
}

/// Provider 的结构化失败类型。这里只保留会改变上层产品行为的稳定类别；展示文案仍用
/// [`AgentMessage::error`]，但上层绝不能靠本地化文案反推错误种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentErrorKind {
    UsageLimitExceeded,
}

/// R1 规范别名;新代码用 `AgentEventKind`,老代码的 `AgentKind` 不须改。
pub type AgentEventKind = AgentKind;

/// 一轮的终态结局。比旧 `final_turn:bool` 细:能区分干净结束与崩溃结束。
/// `status`/`run_id`/`ts` 由 IO 源(R2+)设置,解析器不设;字段默认 None/空/EPOCH。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStatus {
    /// 子进程退出码 0 且见 result/end_turn。
    Completed,
    /// 退出码非 0 或 EOF 无 result。
    Failed,
    /// 被中断。
    Aborted,
    /// 看门狗超时(连续无进展)。
    Timeout,
    /// tool_use 久无配对 tool_result(等人工授权)。
    Awaiting,
}

/// 内容块来自哪一种上游流事件。只描述传输语义，不承载正文；用于把 RPC 原始事件、
/// 段归并判定和 live-card PATCH 串成一条可观测链路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentStreamSource {
    CodexRpcDelta,
    CodexRpcCompleted,
    /// `item/reasoning/summaryTextDelta`：可公开的 reasoning 摘要，不等于明文推理细节。
    CodexRpcSummaryDelta,
    /// `item/completed(reasoning).summary`：摘要的完成快照。
    CodexRpcSummaryCompleted,
}

impl ContentStreamSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CodexRpcDelta => "codex_rpc_delta",
            Self::CodexRpcCompleted => "codex_rpc_completed",
            Self::CodexRpcSummaryDelta => "codex_rpc_summary_delta",
            Self::CodexRpcSummaryCompleted => "codex_rpc_summary_completed",
        }
    }

    pub fn is_completed(self) -> bool {
        matches!(
            self,
            Self::CodexRpcCompleted | Self::CodexRpcSummaryCompleted
        )
    }
}

/// 本轮/本子任务标识,并行扇出 demux 用。默认空字符串表示"未分配"。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunId(pub String);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// claude: `cache_read_input_tokens`;cursor: `cacheReadTokens`
    pub cache_read: u64,
    /// claude: `cache_creation_input_tokens`;cursor: `cacheWriteTokens`
    pub cache_write: u64,
}

/// 一次模型请求的 token,各 CLI 统一成同一口径:`input` 是未命中缓存的输入(OpenAI 口径的输入
/// 含缓存,换算时减掉),推理 token 含在 `output` 里。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestUsage {
    /// 去重键:同一次请求在流里会出现不止一次(claude 按内容块拆成多条,codex 会重复推送)。
    pub id: String,
    pub model: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    /// 非标准的服务档(如 `priority`);标准档为 `None`。
    pub variant: Option<String>,
}

impl RequestUsage {
    /// 同一次请求又见了一次:claude 后写的那条 output 更大(流式写入),各项取较大值。
    pub fn merge(&mut self, other: &RequestUsage) {
        self.input = self.input.max(other.input);
        self.output = self.output.max(other.output);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write_5m = self.cache_write_5m.max(other.cache_write_5m);
        self.cache_write_1h = self.cache_write_1h.max(other.cache_write_1h);
        if self.model.is_none() {
            self.model.clone_from(&other.model);
        }
        if self.variant.is_none() {
            self.variant.clone_from(&other.variant);
        }
    }

    /// 这次请求送进模型的输入总量,长上下文分档按它算。
    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

/// 工具结果只是「已把任务派到后台」(claude `toolUseResult.status == "async_launched"`)。
/// 这样的结果不是任务的产出:派出的子 agent 此刻才开始跑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncLaunch {
    /// 后台子 agent 的 agentId。子 agent 结束后状态源只按它认人(不再带派出调用 id)。
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentMessage {
    // ── 原有字段(保持不变) ──────────────────────────────────────────────
    pub kind: AgentKind,
    pub text: Option<String>,       // assistant/thinking/result 的文本
    pub tool: Option<String>,       // 工具名(ToolUse)
    pub session_id: Option<String>, // system/result 带出
    pub usage: Option<Usage>,       // result 带出

    // ── R1 新增:工具调用细节 ──────────────────────────────────────────
    /// 工具调用 id(配对 tool_result;嵌套 parent_tool_use_id 传此字段)
    pub tool_id: Option<String>,
    /// 工具入参;进度行可显示 `$ git status` 而非仅工具名
    pub tool_input: Option<serde_json::Value>,
    /// 工具结果
    pub tool_result: Option<serde_json::Value>,
    /// tool_result 块的 `is_error` 标记(claude:工具执行失败)。仅 ToolResult 事件置,
    /// 其余恒 false。
    pub tool_is_error: bool,

    // ── R1 新增:终态信号 ──────────────────────────────────────────────
    /// 转录源判 turn-end:end_turn/stop_sequence → final;tool_use → 续 tail
    pub stop_reason: Option<String>,
    /// 仅终态事件置;由 IO 源(R2+)映射成结局,解析器不设
    pub status: Option<TurnStatus>,
    /// 失败原因(stderr 尾 / 解析错);由 IO 源设置
    pub error: Option<String>,
    /// Provider 给出的结构化失败类别；没有稳定类别时保持 None。
    pub error_kind: Option<AgentErrorKind>,

    // ── R1 新增:追踪标识 ─────────────────────────────────────────────
    /// 本轮/子任务标识;由 IO 源(R2+)设置,解析器不设(保持 Default 空字符串)
    pub run_id: RunId,
    /// 子 agent 事件并回父流时的归属;由 IO 源设置
    pub parent_run_id: Option<RunId>,

    // ── R1 新增:时间戳 ───────────────────────────────────────────────
    /// 跨 ssh 排序/延迟用;解析器设 UNIX_EPOCH 保持确定性,IO 源(R2+)再盖真实时间
    pub ts: SystemTime,

    /// cursor `--stream-partial-output` 的增量 delta 块标记(原始行带 `timestamp_ms`)。
    /// true = 本块是同一条回复的 token 增量,须**拼接**(原地增长),不可当独立段落用 `\n\n` 分隔;
    /// cursor 在 delta 流末尾还会再发一条无 `timestamp_ms` 的完整快照(partial=false)。
    /// claude/codex 无此概念,assistant 块各为独立段落 → 恒 false。
    pub partial: bool,

    /// 上游内容流事件的来源阶段。非流式来源为 None。
    pub stream_source: Option<ContentStreamSource>,
    /// provider 内容 item 的稳定标识；同一条 Codex delta 与 completed 快照应共享此 id。
    pub source_item_id: Option<String>,
    /// 仅 ToolResult:这次调用只是把任务派到了后台。
    pub async_launch: Option<AsyncLaunch>,
}

impl AgentMessage {
    pub fn of(kind: AgentKind) -> Self {
        Self {
            kind,
            text: None,
            tool: None,
            session_id: None,
            usage: None,
            tool_id: None,
            tool_input: None,
            tool_result: None,
            tool_is_error: false,
            stop_reason: None,
            status: None,
            error: None,
            error_kind: None,
            run_id: RunId::default(),
            parent_run_id: None,
            // 解析器不拿系统时间,确保 parse 函数纯确定;IO 源(R2+)负责盖真实时间戳
            ts: SystemTime::UNIX_EPOCH,
            partial: false,
            stream_source: None,
            source_item_id: None,
            async_launch: None,
        }
    }
}

/// R1 规范别名;新代码用 `AgentEvent`,老代码的 `AgentMessage` 不须改。
pub type AgentEvent = AgentMessage;

/// assistant 的 `stop_reason` 是否表示**本轮终态**(turn 真正结束)。
///
/// **只认 `end_turn`**:summon 用法下 claude 的真正收尾恒是 end_turn;`stop_sequence` 是 claude
/// 内部产物(如 `No response requested.`),当终态会误取它当回复(实测:回复 23 却显示该串)。
///
/// 单独抽出来是因为有**两个消费方**必须同判:捕获层(`capture::transcript` 决定何时收一轮)与
/// 历史投影(`observe::history` 决定一轮何时收口成 completed)。判定分叉就会出现「捕获认为结束了、
/// 历史仍显示进行中」这类不一致。
///
/// **两边都必须与内容块类型解耦**:claude 把同一条消息的 thinking/text 块拆成相邻多条记录,每条都
/// 带同一个 stop_reason,一轮可能只有 thinking 那条带 end_turn。谁在自己的 `assistant` 分支里判,
/// 谁就会在纯 thinking 收尾时永远收不了轮(2026-07-27 现场:chat 线程卡在「跑一轮中」,消息全堵)。
pub fn is_turn_end(stop_reason: Option<&str>) -> bool {
    matches!(stop_reason, Some("end_turn"))
}
