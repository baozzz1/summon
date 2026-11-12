//! 飞书原生 CoT 消息的事件投影（纯件，无网络与时钟）。
//!
//! 子 agent 的过程也经这里投影。飞书客户端把嵌套 run、工具挂工具一律平铺,归属只能靠「每个
//! 子 agent 在 CoT 里是连续的一段」表达:每个子 agent 一个独立的 reasoning 块,同一时刻最多
//! 一个块在实时推,其余攒着(见 [`CotProjector::project`])。

use crate::progress::{ProtocolTool, SubagentItem, SubagentLane, TurnProgress};
use crate::segment::TurnSegment;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[path = "cot_paging.rs"]
pub mod paging;

/// 原生 CoT 的借用快照：过程段、工具进度与回复放行界。
pub struct CotView<'a> {
    pub segments: &'a [crate::segment::TurnSegment],
    pub progress: Option<&'a crate::progress::TurnProgress>,
    pub query: Option<&'a str>,
    /// `segments[..n]` 里的回复段已确认属于过程,CoT 把它们作为过程回复呈现;`None` 时回复段
    /// 只属于终态消息。
    pub released_replies: Option<usize>,
}

impl<'a> CotView<'a> {
    pub fn new(
        segments: &'a [crate::segment::TurnSegment],
        progress: Option<&'a crate::progress::TurnProgress>,
    ) -> Self {
        Self {
            segments,
            progress,
            query: None,
            released_replies: None,
        }
    }

    pub fn with_query(mut self, query: Option<&'a str>) -> Self {
        self.query = query;
        self
    }

    pub fn with_released_replies(mut self, released: Option<usize>) -> Self {
        self.released_replies = released;
        self
    }
}

const DELTA_CHARS: usize = 512;

/// 每个子 agent 块最多展示的工具行数;超出的只在块尾计数。
pub const MAX_LANE_TOOL_ROWS: usize = 20;
/// 每个子 agent 块最多展示的思考段数;超出的只在块尾计数。
pub const MAX_LANE_THINKING: usize = 20;
const LANE_CONTINUED: &str = "rd065195833";
const LANE_STILL_RUNNING: &str = "r9bd9c15b97";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CotEvent {
    pub event_type: String,
    pub content: String,
    pub timestamp: String,
    /// 本地送达记账，不进入飞书协议。
    #[serde(skip)]
    pub reply_delivery: Option<ReplyDelivery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyDelivery {
    pub generation: u64,
    pub segment: usize,
    /// 已去除 feed 标签的正文 UTF-8 字节位置。
    pub bytes: usize,
}

impl CotEvent {
    fn new(event_type: &str, content: serde_json::Value, timestamp: &str) -> Self {
        Self {
            event_type: event_type.to_string(),
            content: content.to_string(),
            timestamp: timestamp.to_string(),
            reply_delivery: None,
        }
    }
}

/// 快照要连它一起过河:`text` 是当前 Thinking segment 已推到哪、`has_content` 决定下一段
/// 要不要前导空行 —— 少任何一个,接管后的第一笔增量就会重推或粘连。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenMessage {
    id: String,
    text: String,
    /// 这条消息是否已经推过任何 CONTENT。客户端把增量原样拼接,段落之间要靠投影方自己
    /// 空一行 —— 第一段不带前导空行,之后每开一个段落(新 Thinking segment、工具意图)都要。
    has_content: bool,
}

/// 一行已排定的工具行。标题与图标在排定时就按当时的开关算好:收尾时手里没有进度,
/// 攒着的行只能靠它自己推出去。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ToolRow {
    /// wire 上的 toolCallId。
    id: String,
    name: String,
    icon: String,
    command: Option<String>,
    language: Option<String>,
    secs: u64,
    done: bool,
    result: Option<String>,
}

/// 主 agent 工具的 toolCallId:进度里的轮内编号。
fn main_call_id(id: u64) -> String {
    format!("tool-{id}")
}

/// 子 agent 工具的 toolCallId:由它在 CoT 里的排位(`LaneSchedule::lanes` 的下标,随快照过河、
/// 只追加)与 lane 内序号决定。两者都不受主 agent 或别的子 agent 的到达顺序影响,接管重放时
/// 同一次调用拿到的还是同一个 id。
fn lane_call_id(slot: usize, seq: u64) -> String {
    format!("tool-s{}-{seq}", slot + 1)
}

fn result_message_id(call_id: &str) -> String {
    match call_id.strip_prefix("tool-") {
        Some(rest) => format!("tool-result-{rest}"),
        None => format!("{call_id}-result"),
    }
}

impl ToolRow {
    fn refresh(&mut self, tool: &ProtocolTool) {
        self.secs = tool.secs;
        self.done = tool.done;
        self.result = tool.result.clone();
    }
}

/// 子 agent 块里已排定、还没推出去的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
enum LaneLine {
    /// 一个段落:思考文本,或工具意图。
    Text(String),
    Tool(ToolRow),
}

/// 一个子 agent 在 CoT 里的排播状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct LaneState {
    key: String,
    /// 块首行(不含「（续）」)。每次投影按最新的标题/说明与思考开关重算。
    header: String,
    finished: bool,
    /// 进度里这个子 agent 的账本已经取到第几项。
    taken: usize,
    queued: Vec<LaneLine>,
    /// 已推上 wire、结果还没推的工具行。
    awaiting: Vec<ToolRow>,
    /// 已排定的工具行数,计入 [`MAX_LANE_TOOL_ROWS`]。
    rows: usize,
    /// 超出上限、不展示的工具调用数。
    unshown: usize,
    /// 已排定的思考段数,计入 [`MAX_LANE_THINKING`]。
    thoughts: usize,
    /// 超出上限、不展示的思考段数。
    unshown_thoughts: usize,
    /// 块首行上过 wire:之后再开块要标「（续）」。
    opened: bool,
    /// 块已收尾,不再收新内容。
    closed: bool,
}

impl LaneState {
    /// 把进度里新到的项排进队列,并刷新已排定、已推出的工具行的结果。`slot` 是这个 lane 在
    /// CoT 里的排位,工具行的 toolCallId 由它派生。
    fn take_from(
        &mut self,
        slot: usize,
        lane: &SubagentLane,
        show_tools: bool,
        thinking_visible: bool,
    ) {
        let has_rows = !self.awaiting.is_empty()
            || self
                .queued
                .iter()
                .any(|line| matches!(line, LaneLine::Tool(_)));
        if has_rows {
            let tools: BTreeMap<String, &ProtocolTool> = lane
                .items()
                .iter()
                .filter_map(|item| match item {
                    SubagentItem::Tool(tool) => Some((lane_call_id(slot, tool.id), tool)),
                    SubagentItem::Thinking(_) => None,
                })
                .collect();
            let queued_rows = self.queued.iter_mut().filter_map(|line| match line {
                LaneLine::Tool(row) => Some(row),
                LaneLine::Text(_) => None,
            });
            for row in self.awaiting.iter_mut().chain(queued_rows) {
                if let Some(tool) = tools.get(&row.id) {
                    row.refresh(tool);
                }
            }
        }
        if self.closed {
            return;
        }
        self.header = lane_header(lane, thinking_visible);
        self.finished = lane.finished();
        for item in lane.items().iter().skip(self.taken) {
            match item {
                SubagentItem::Thinking(text) => {
                    if thinking_visible {
                        self.push_thought(text.clone());
                    }
                }
                SubagentItem::Tool(tool) => {
                    // 超出上限的连意图一起不展示:它属于那次没展示的调用,由块尾计数带过。
                    if show_tools && self.rows >= MAX_LANE_TOOL_ROWS {
                        self.unshown += 1;
                        continue;
                    }
                    let call_id = lane_call_id(slot, tool.id);
                    let plan = plan_tool(tool, call_id, show_tools, thinking_visible);
                    match plan.row {
                        Some(row) => {
                            if let Some(text) = plan.narration {
                                self.queued.push(LaneLine::Text(text));
                            }
                            self.rows += 1;
                            self.queued.push(LaneLine::Tool(row));
                        }
                        // 没有工具行时,意图就是一段思考正文,按思考计数。
                        None => {
                            if let Some(text) = plan.narration {
                                self.push_thought(text);
                            }
                        }
                    }
                }
            }
        }
        self.taken = self.taken.max(lane.items().len());
    }

    fn push_thought(&mut self, text: String) {
        if self.thoughts >= MAX_LANE_THINKING {
            self.unshown_thoughts += 1;
            return;
        }
        self.thoughts += 1;
        self.queued.push(LaneLine::Text(text));
    }

    /// 块尾:没展示的思考与工具并成一行计数(思考不可见、工具详情关时各自不计);轮收尾时还没
    /// 结束的再标一句。
    fn tail(&self, turn_over: bool) -> Vec<String> {
        let mut lines = Vec::new();
        match (self.unshown_thoughts, self.unshown) {
            (0, 0) => {}
            (thoughts, 0) => lines.push(summon_i18n::format!("r8f7adb19bd")),
            (0, tools) => lines.push(summon_i18n::format!("r3794d49eb3")),
            (thoughts, tools) => lines.push(summon_i18n::format!("r7b7fdeacb1")),
        }
        if turn_over && !self.finished {
            lines.push(::summon_core::i18n::label(LANE_STILL_RUNNING).to_string());
        }
        lines
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneEnd {
    /// 暂时让位:块关掉,子 agent 之后以「（续）」重开。
    Paused,
    /// 子 agent 结束:补块尾,此块到此为止。
    Finished,
    /// 轮结束:补块尾,还没结束的再标「（仍在运行）」。
    TurnOver,
}

/// 子 agent 块的排播状态。整体随快照过河;字段全带默认值,旧快照读回来就是「没有子 agent」。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct LaneSchedule {
    /// 按首次见到的顺序,只追加,下标稳定。排播的先后与工具行 id 的排位都只认它。
    lanes: Vec<LaneState>,
    /// 此刻开着的是哪个子 agent 的块(`CotProjector::open` 就是它)。
    live: Option<usize>,
    /// 主 agent 的消息没开着时,它当前 Thinking segment 推到哪了(`OpenMessage::text` 的停放处)。
    parked_text: String,
}

/// 预演主 agent 投影的结论。
#[derive(Debug, Default)]
struct MainNews {
    any: bool,
    /// 除工具结果外还有别的:工具结果按 toolCallId 挂回自己那一行,不需要开着的消息。
    needs_message: bool,
}

/// 把累积的 `CotView` 快照投成只追加的 AG-UI 事件。
///
/// 全字段 serde:daemon 换代后接管轮要从盘上这份快照续投同一条 CoT,而 CoT 是只追加的
/// 事件流 —— 没有游标就只能重推已发过的段,或者关不掉上一代开着的 message / tool。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CotProjector {
    thread_id: String,
    run_id: String,
    started: bool,
    terminal_emitted: bool,
    open: Option<OpenMessage>,
    segment_cursor: usize,
    #[serde(default)]
    reply_generation: u64,
    #[serde(default)]
    projected_reply_bytes: BTreeMap<usize, usize>,
    next_message_id: u64,
    tools: BTreeMap<u64, bool>,
    query: String,
    last_timestamp: u64,
    #[serde(default)]
    subagents: LaneSchedule,
}

impl CotProjector {
    pub fn new(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        query: impl Into<String>,
    ) -> Self {
        let query = query.into();
        let query = if query.trim().is_empty() {
            summon_i18n::text!("r17ee27d0c3").to_string()
        } else {
            query.chars().take(200).collect()
        };
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            started: false,
            terminal_emitted: false,
            open: None,
            segment_cursor: 0,
            reply_generation: 0,
            projected_reply_bytes: BTreeMap::new(),
            next_message_id: 1,
            tools: BTreeMap::new(),
            query,
            last_timestamp: 0,
            subagents: LaneSchedule::default(),
        }
    }

    /// 接管轮在旧 CoT 上打段边界时用的文案。
    pub const RESUME_BOUNDARY_NOTE: &str = "re8a4a3ba69";
    /// 停机收口时给旧 CoT 的最后一句。**不许诺「下一条续跑」**:硬崩那条要等用户点恢复才有下一条。
    pub const SHUTDOWN_CLOSE_NOTE: &str = "r6b2a539dda";

    /// 已投影到第几个 segment。接管时用它复核段落对齐:重建出的 segments 少于这个游标,
    /// 说明快照与眼前的转录不是同一件事(被截断、解析规则变了),那就不该续投。
    pub fn segment_cursor(&self) -> usize {
        self.segment_cursor
    }

    /// Legacy snapshots have no proven per-carrier event count, so continue on a fresh carrier.
    pub fn unknown_wire_state(&self) -> paging::CotWireState {
        let run = CotEvent::new(
            "RUN_STARTED",
            serde_json::json!({
                "threadId": self.thread_id, "runId": self.run_id, "input": {"query": self.query}
            }),
            &self.last_timestamp.to_string(),
        );
        let reasoning = self.open.as_ref().map(|open| {
            CotEvent::new(
                "REASONING_MESSAGE_START",
                serde_json::json!({"messageId":open.id,"role":"reasoning"}),
                &self.last_timestamp.to_string(),
            )
        });
        paging::CotWireState::unknown(run, reasoning)
    }

    pub fn reply_generation(&self) -> u64 {
        self.reply_generation
    }

    /// 完整推送成功的投影快照可恢复其回复确认范围。
    pub fn projected_replies(&self) -> Vec<ReplyDelivery> {
        self.projected_reply_bytes
            .iter()
            .map(|(&segment, &bytes)| ReplyDelivery {
                generation: self.reply_generation,
                segment,
                bytes,
            })
            .collect()
    }

    /// 投影主 agent 的思考与工具,并排播子 agent 块(每个子 agent 在 CoT 里连续成段):
    ///
    /// - 同一时刻最多一个子 agent 块「在播」(实时推),且只在主 agent 等子 agent 时在播;
    /// - 没有在播块时,先按开始顺序把已结束的整块推出(不越过暂停着的更早者),主 agent 在等
    ///   就挑开始最早的未结束者开播;
    /// - 主 agent 不再等、或这次有主 agent 的新内容(子 agent 的派出/等待记录不算),在播块先推完
    ///   已攒的再暂停,之后以「（续）」重开。
    ///
    /// 回复段一律不进 CoT;要把过程回复投进来用 [`Self::project_with_replies`]。
    pub fn project(
        &mut self,
        segments: &[TurnSegment],
        progress: Option<&TurnProgress>,
        timestamp: &str,
    ) -> Vec<CotEvent> {
        self.project_with_replies(segments, None, progress, timestamp)
    }

    /// 同 [`Self::project`],另把 `segments[..n]` 里的回复段作为过程回复投成文本块
    /// (`released = Some(n)`,见 [`project_segments_into`])。
    pub fn project_with_replies(
        &mut self,
        segments: &[TurnSegment],
        released: Option<usize>,
        progress: Option<&TurnProgress>,
        timestamp: &str,
    ) -> Vec<CotEvent> {
        // 收口之后什么都不投:CoT 已终态,晚到的事件(含子 agent 的)没有载体可去。
        if self.terminal_emitted {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(CotEvent::new(
                "RUN_STARTED",
                serde_json::json!({
                    "threadId": self.thread_id,
                    "runId": self.run_id,
                    "input": {"query": self.query}
                }),
                timestamp,
            ));
        }
        let waiting = progress.is_some_and(TurnProgress::parent_waiting);
        if let Some(progress) = progress {
            self.take_lanes(progress);
        }
        // 主消息开着时不用预演:主 agent 的内容照常落进去,也没有在播的子 agent 块要让位。
        let news = if self.main_is_open() {
            MainNews::default()
        } else {
            self.main_news(segments, released, progress)
        };
        if let Some(index) = self.subagents.live {
            self.drain_live(timestamp, &mut events);
            if self.subagents.lanes[index].finished {
                self.close_live(LaneEnd::Finished, timestamp, &mut events);
            } else if news.any || !waiting {
                self.close_live(LaneEnd::Paused, timestamp, &mut events);
            }
        }
        self.flush_due_results(timestamp, &mut events);
        if news.needs_message {
            self.ensure_main_open(timestamp, &mut events);
        }
        // 先建立 reasoning message，再把同一快照里的工具挂到它下面。反过来会让工具成为
        // 顶层事件，客户端因此把折叠中的 Thinking 顶掉。
        if self.subagents.live.is_none() {
            self.project_segments(segments, released, timestamp, &mut events);
            if progress.is_some_and(|progress| self.main_tools_need_message(progress)) {
                self.ensure_main_open(timestamp, &mut events);
            }
            self.project_tools(progress, timestamp, &mut events);
        }
        self.schedule_lanes(waiting, timestamp, &mut events);
        self.assign_timestamps(timestamp, &mut events);
        events
    }

    /// 轮内追加消息被消费时的**段边界**:关掉当前 reasoning 段,紧接着开一个新的。
    ///
    /// **不关整条 CoT**:它是整轮唯一的过程账本,推 `RUN_FINISHED` 会把仍在跑的轮标成结束,
    /// 还会丢掉未完成工具的结果归属。段边界在客户端渲成两个折叠块,用户看得见
    /// 「从这里起是我那条追加的处理过程」,而载体不变。
    pub fn steer_boundary(&mut self, note: &str, timestamp: &str) -> Vec<CotEvent> {
        if self.terminal_emitted || !self.started {
            return Vec::new();
        }
        let mut events = Vec::new();
        // 段边界是主 agent 的:在播的子 agent 块先收起来,不能被它切成两半。
        self.settle_live(timestamp, &mut events);
        // 已推游标要**搬进**新段。它记的是「当前这个 Thinking segment 推到哪了」,清零会让
        // 同一段思考在新段里从头重推一遍(投影按 `text` 前缀算增量,见 `project_segments`)。
        let carried = self.main_text().to_string();
        self.subagents.parked_text.clear();
        self.close_message(timestamp, &mut events);
        self.ensure_main_open(timestamp, &mut events);
        let Some(open) = self.open.as_mut() else {
            return events;
        };
        open.text = carried;
        let id = open.id.clone();
        if !note.trim().is_empty() {
            push_content(&mut events, &id, note, timestamp);
            open.has_content = true;
        }
        self.assign_timestamps(timestamp, &mut events);
        events
    }

    /// 进程重启续跑(Cursor CLI steer)的段边界:与 [`Self::steer_boundary`] 同一条投影,但续跑那一代
    /// 的 segments 从 0 重新编号,段游标必须归零 —— 留着旧游标,新一代的前若干段会被当成已推过。
    pub fn restart_boundary(&mut self, note: &str, timestamp: &str) -> Vec<CotEvent> {
        let events = self.steer_boundary(note, timestamp);
        self.restart_segments();
        events
    }

    /// 只把段游标归零、不推任何事件:过程内容不外显时续跑也要换代编号。
    pub fn restart_segments(&mut self) {
        self.segment_cursor = 0;
        self.reply_generation = self.reply_generation.saturating_add(1);
        self.projected_reply_bytes.clear();
        self.subagents.parked_text.clear();
        if let Some(open) = self.open.as_mut() {
            open.text.clear();
        }
    }

    /// 接管后的段边界:与 [`Self::steer_boundary`] 同一条投影,只是文案不同 —— 用户看见的是
    /// 「上一段停在哪、从这里接着跑」,而载体仍是整轮那一条 CoT。
    pub fn resume_boundary(&mut self, note: &str, timestamp: &str) -> Vec<CotEvent> {
        self.steer_boundary(note, timestamp)
    }

    /// 收口前的最后一句,落在主 agent **当前段**里、不开新段:紧接着就是 `cot_complete`,
    /// 为一句话单开一个折叠块只会在客户端多出一个空块。例外是子 agent 块开着(或刚收起)
    /// 的时候 —— 这句话不属于子 agent,先收块,再落进主 agent 的块。
    pub fn closing_note(&mut self, note: &str, timestamp: &str) -> Vec<CotEvent> {
        if self.terminal_emitted || note.trim().is_empty() {
            return Vec::new();
        }
        let mut events = Vec::new();
        self.ensure_main_open(timestamp, &mut events);
        self.settle_live(timestamp, &mut events);
        self.ensure_main_open(timestamp, &mut events);
        let Some(open) = self.open.as_mut() else {
            return events;
        };
        push_paragraph(&mut events, open, note, timestamp);
        self.assign_timestamps(timestamp, &mut events);
        events
    }

    /// 显式结束会话时，终态说明作为普通文字留在同一条过程消息里。
    pub fn finish_session(&mut self, timestamp: &str) -> Vec<CotEvent> {
        let mut events = self.finish("interrupted", timestamp);
        if let Some(terminal) = events.pop() {
            push_text_message(
                &mut events,
                &format!("{}-session-ended", self.run_id),
                summon_i18n::text!("r3c30e0b840"),
                timestamp,
            );
            events.push(terminal);
            self.assign_timestamps(timestamp, &mut events);
        }
        events
    }

    pub fn finish(&mut self, status: &str, timestamp: &str) -> Vec<CotEvent> {
        if self.terminal_emitted {
            return Vec::new();
        }
        self.terminal_emitted = true;
        let mut events = Vec::new();
        self.close_all(timestamp, &mut events);
        events.push(CotEvent::new(
            "RUN_FINISHED",
            serde_json::json!({
                "threadId": self.thread_id,
                "runId": self.run_id,
                "status": status
            }),
            timestamp,
        ));
        self.assign_timestamps(timestamp, &mut events);
        events
    }

    pub fn error(&mut self, message: &str, code: &str, timestamp: &str) -> Vec<CotEvent> {
        if self.terminal_emitted {
            return Vec::new();
        }
        self.terminal_emitted = true;
        let mut events = Vec::new();
        self.close_all(timestamp, &mut events);
        events.push(CotEvent::new(
            "RUN_ERROR",
            serde_json::json!({"message": message, "code": code}),
            timestamp,
        ));
        self.assign_timestamps(timestamp, &mut events);
        events
    }

    /// 终态之前:子 agent 按开始顺序整块推完(在播的就地收尾),再关主 agent 的消息,
    /// 最后补上还没推的子 agent 工具结果 —— 终态之后就没有载体了。
    fn close_all(&mut self, timestamp: &str, events: &mut Vec<CotEvent>) {
        if self.subagents.live.is_some() {
            self.drain_live(timestamp, events);
            self.close_live(LaneEnd::TurnOver, timestamp, events);
        }
        for index in self.lanes_in_order() {
            if !self.subagents.lanes[index].closed {
                self.push_whole(index, LaneEnd::TurnOver, timestamp, events);
            }
        }
        self.close_message(timestamp, events);
        self.flush_due_results(timestamp, events);
    }

    fn assign_timestamps(&mut self, timestamp: &str, events: &mut [CotEvent]) {
        let base = timestamp.parse::<u64>().unwrap_or(0);
        for event in events {
            self.last_timestamp = base.max(self.last_timestamp.saturating_add(1));
            event.timestamp = self.last_timestamp.to_string();
        }
    }

    fn project_tools(
        &mut self,
        progress: Option<&TurnProgress>,
        timestamp: &str,
        events: &mut Vec<CotEvent>,
    ) {
        let Some(progress) = progress else { return };
        project_main_tools(
            &mut self.tools,
            self.open.as_mut(),
            progress,
            &|_| true,
            timestamp,
            events,
        );
    }

    /// 开一条新的 reasoning 消息。调用方保证此刻没有开着的消息。
    fn open_message(&mut self, timestamp: &str, events: &mut Vec<CotEvent>) {
        // 开之前正是「没有任何块开着」的时刻,子 agent 攒下的工具结果趁这里推出去。
        self.flush_due_results(timestamp, events);
        let id = format!("reasoning-{}", self.next_message_id);
        self.next_message_id += 1;
        events.push(CotEvent::new(
            "REASONING_START",
            serde_json::json!({"messageId": id}),
            timestamp,
        ));
        events.push(CotEvent::new(
            "REASONING_MESSAGE_START",
            serde_json::json!({"messageId": id, "role": "reasoning"}),
            timestamp,
        ));
        self.open = Some(OpenMessage {
            id,
            // 这里只跟踪当前 Thinking segment 的累积快照；固定锚点已经在 wire 上，不能让
            // 工具标题或 Reply segment 改写它。
            text: String::new(),
            has_content: false,
        });
    }

    /// 主 agent 的段落。放行的回复段投成独立的文本块:先关掉当前 reasoning 消息,文本块之后的
    /// 思考与工具再开新的 reasoning 消息 —— 只在真有内容要写时才开,免得文本块前后夹空块。
    fn project_segments(
        &mut self,
        segments: &[TurnSegment],
        released: Option<usize>,
        timestamp: &str,
        events: &mut Vec<CotEvent>,
    ) {
        while self.segment_cursor < segments.len() {
            if let Some(open) = self.open.as_mut() {
                if !project_segments_into(
                    &mut self.segment_cursor,
                    open,
                    segments,
                    released,
                    timestamp,
                    events,
                ) {
                    return;
                }
                self.close_message(timestamp, events);
                continue;
            }
            let cursor = self.segment_cursor;
            match &segments[cursor] {
                TurnSegment::Reply(text) if released.is_some_and(|n| cursor < n) => {
                    let id = format!("text-{}", self.next_message_id);
                    self.next_message_id += 1;
                    let start = events.len();
                    let text = crate::feed_tag::strip_feed_tag(text);
                    push_text_message(events, &id, &text, timestamp);
                    let mut bytes = 0;
                    for event in &mut events[start..] {
                        if event.event_type == "TEXT_MESSAGE_CONTENT" {
                            let content: serde_json::Value =
                                serde_json::from_str(&event.content).expect("generated CoT JSON");
                            bytes += content["delta"].as_str().unwrap_or_default().len();
                            event.reply_delivery = Some(ReplyDelivery {
                                generation: self.reply_generation,
                                segment: cursor,
                                bytes,
                            });
                        }
                    }
                    self.projected_reply_bytes.insert(cursor, text.len());
                    self.segment_cursor += 1;
                    self.subagents.parked_text.clear();
                }
                TurnSegment::Reply(_) if released.is_none() => {
                    self.segment_cursor += 1;
                    self.subagents.parked_text.clear();
                }
                TurnSegment::Reply(_) => return,
                TurnSegment::Thinking(text) => {
                    if !thinking_delta(text, &self.subagents.parked_text).is_empty() {
                        self.ensure_main_open(timestamp, events);
                        if self.open.is_none() {
                            return;
                        }
                    } else if cursor + 1 < segments.len() {
                        self.segment_cursor += 1;
                        self.subagents.parked_text.clear();
                    } else {
                        return;
                    }
                }
            }
        }
    }

    fn close_message(&mut self, timestamp: &str, events: &mut Vec<CotEvent>) {
        let Some(open) = self.open.take() else { return };
        events.push(CotEvent::new(
            "REASONING_MESSAGE_END",
            serde_json::json!({"messageId": open.id}),
            timestamp,
        ));
        events.push(CotEvent::new(
            "REASONING_END",
            serde_json::json!({"messageId": open.id}),
            timestamp,
        ));
    }

    fn main_is_open(&self) -> bool {
        self.open.is_some() && self.subagents.live.is_none()
    }

    fn main_text(&self) -> &str {
        match &self.open {
            Some(open) if self.subagents.live.is_none() => &open.text,
            _ => &self.subagents.parked_text,
        }
    }

    /// 在草稿上预演一遍主 agent 的投影,看这次有没有新东西、要不要一条开着的消息来装。
    ///
    /// 投出放行回复的那一次,要预开的只有回复之前的思考:回复之后的内容由 `project_segments`
    /// 在文本块之后另开消息来装,工具同理(见 [`Self::project`])。
    fn main_news(
        &self,
        segments: &[TurnSegment],
        released: Option<usize>,
        progress: Option<&TurnProgress>,
    ) -> MainNews {
        let mut cursor = self.segment_cursor;
        let mut scratch = OpenMessage {
            id: String::new(),
            text: self.main_text().to_string(),
            has_content: true,
        };
        let mut events = Vec::new();
        let mut text_block = false;
        let mut news = MainNews::default();
        while project_segments_into(
            &mut cursor,
            &mut scratch,
            segments,
            released,
            "",
            &mut events,
        ) {
            if !text_block {
                news.needs_message = !events.is_empty();
            }
            text_block = true;
            cursor += 1;
            scratch.text.clear();
        }
        news.any = text_block || !events.is_empty();
        if !text_block {
            news.needs_message = !events.is_empty();
        }
        let Some(progress) = progress else {
            return news;
        };
        if !text_block {
            news.needs_message |= self.main_tools_need_message(progress);
        }
        news.any |= !self
            .preview_main_tools(progress, &|tool| !is_subagent_bookkeeping(tool))
            .is_empty();
        news
    }

    /// 主 agent 这次有没有除工具结果之外的工具事件:工具行与意图段落都要一条开着的消息来挂。
    fn main_tools_need_message(&self, progress: &TurnProgress) -> bool {
        self.preview_main_tools(progress, &|_| true)
            .iter()
            .any(|event| event.event_type != "TOOL_CALL_RESULT")
    }

    fn preview_main_tools(
        &self,
        progress: &TurnProgress,
        include: &dyn Fn(&ProtocolTool) -> bool,
    ) -> Vec<CotEvent> {
        let mut tools = self.tools.clone();
        let mut scratch = OpenMessage {
            id: String::new(),
            text: String::new(),
            has_content: true,
        };
        let mut events = Vec::new();
        project_main_tools(
            &mut tools,
            Some(&mut scratch),
            progress,
            include,
            "",
            &mut events,
        );
        events
    }

    /// 子 agent 块收起后,主 agent 的消息不预开:真要往里写时才开,免得两个子 agent 块之间、
    /// 或轮尾,夹一个空块。重开时把停放的游标搬回来。
    fn ensure_main_open(&mut self, timestamp: &str, events: &mut Vec<CotEvent>) {
        if self.open.is_some() {
            return;
        }
        let text = std::mem::take(&mut self.subagents.parked_text);
        self.open_message(timestamp, events);
        if let Some(open) = self.open.as_mut() {
            open.text = text;
        }
    }

    fn take_lanes(&mut self, progress: &TurnProgress) {
        let show_tools = progress.tool_details();
        let thinking_visible = !progress.hide_thinking();
        let lanes = &mut self.subagents.lanes;
        for lane in progress.subagent_lanes() {
            let index = match lanes.iter().position(|state| state.key == lane.key()) {
                Some(index) => index,
                None => {
                    lanes.push(LaneState {
                        key: lane.key().to_string(),
                        ..LaneState::default()
                    });
                    lanes.len() - 1
                }
            };
            lanes[index].take_from(index, lane, show_tools, thinking_visible);
        }
    }

    /// 开始顺序就是 `LaneSchedule::lanes` 的排位:只追加、随快照过河。接管后新一代进度里的先后
    /// 是它自己读到的先后,拿来排就会让新出现的子 agent 插到上一代暂停着的块中间。
    fn lanes_in_order(&self) -> std::ops::Range<usize> {
        0..self.subagents.lanes.len()
    }

    /// 没有在播块(或在播块刚结束)时的排播:已结束的按开始顺序整块推出;主 agent 在等,
    /// 就让开始最早的未结束者开播。
    ///
    /// 开始最早的未结束者若已经上过 wire(暂停着),比它晚开始的一律等它结束 —— 哪怕后者已经
    /// 结束。否则它再开的「（续）」块和前一截之间会夹着别的子 agent,归属就断了。
    fn schedule_lanes(&mut self, waiting: bool, timestamp: &str, events: &mut Vec<CotEvent>) {
        if let Some(index) = self.subagents.live {
            self.drain_live(timestamp, events);
            if !self.subagents.lanes[index].finished {
                return;
            }
            self.close_live(LaneEnd::Finished, timestamp, events);
        }
        for index in self.lanes_in_order() {
            let lane = &self.subagents.lanes[index];
            if lane.closed {
                continue;
            }
            if !lane.finished {
                if lane.opened {
                    break;
                }
                continue;
            }
            self.push_whole(index, LaneEnd::Finished, timestamp, events);
        }
        if !waiting {
            return;
        }
        let next = self.lanes_in_order().find(|&index| {
            let lane = &self.subagents.lanes[index];
            !lane.finished && !lane.closed
        });
        let Some(index) = next else { return };
        // 开始最早的那个优先,哪怕它暂停后还没新内容 —— 让后开始的插队,等它有了内容又得排队。
        // 暂停过又没新内容就先不重开:一个只有「（续）」的块什么也没说。
        let lane = &self.subagents.lanes[index];
        if lane.opened && lane.queued.is_empty() {
            return;
        }
        self.open_lane(index, timestamp, events);
        self.drain_live(timestamp, events);
    }

    /// 把一个不在播的子 agent 整块推出并收尾。暂停过、又没有任何新东西可说的,直接收尾不开块;
    /// 它在飞工具的结果留给下一个没有块开着的时刻。
    fn push_whole(
        &mut self,
        index: usize,
        end: LaneEnd,
        timestamp: &str,
        events: &mut Vec<CotEvent>,
    ) {
        let lane = &mut self.subagents.lanes[index];
        if lane.opened && lane.queued.is_empty() && lane.tail(end == LaneEnd::TurnOver).is_empty() {
            lane.closed = true;
            return;
        }
        self.open_lane(index, timestamp, events);
        self.drain_live(timestamp, events);
        self.close_live(end, timestamp, events);
    }

    /// 开一个子 agent 块:主 agent 的消息先收起(游标停放),再开新消息写块首行。
    /// 调用方保证此刻没有在播块。
    fn open_lane(&mut self, index: usize, timestamp: &str, events: &mut Vec<CotEvent>) {
        if self.main_is_open() {
            if let Some(open) = self.open.as_ref() {
                self.subagents.parked_text = open.text.clone();
            }
            self.close_message(timestamp, events);
        }
        self.open_message(timestamp, events);
        self.subagents.live = Some(index);
        let lane = &mut self.subagents.lanes[index];
        let header = if lane.opened {
            format!(
                "{}{LANE_CONTINUED}",
                lane.header,
                LANE_CONTINUED = ::summon_core::i18n::label(LANE_CONTINUED)
            )
        } else {
            lane.header.clone()
        };
        lane.opened = true;
        if let Some(open) = self.open.as_mut() {
            push_paragraph(events, open, &header, timestamp);
        }
    }

    /// 把在播块攒下的推出去:先补已推工具行的结果,再按到达顺序推段落与工具行。
    fn drain_live(&mut self, timestamp: &str, events: &mut Vec<CotEvent>) {
        let Some(index) = self.subagents.live else {
            return;
        };
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let lane = &mut self.subagents.lanes[index];
        lane.awaiting.retain(|row| {
            if row.done {
                push_row_result(events, row, timestamp);
            }
            !row.done
        });
        for line in std::mem::take(&mut lane.queued) {
            match line {
                LaneLine::Text(text) => push_paragraph(events, open, &text, timestamp),
                LaneLine::Tool(row) => {
                    push_tool_row(events, &row, Some(&open.id), timestamp);
                    if !row.done {
                        lane.awaiting.push(row);
                    }
                }
            }
        }
    }

    /// 关掉在播块。暂停只关块;结束(子 agent 或整轮)先补块尾,此块到此为止。
    fn close_live(&mut self, end: LaneEnd, timestamp: &str, events: &mut Vec<CotEvent>) {
        let Some(index) = self.subagents.live.take() else {
            return;
        };
        if end != LaneEnd::Paused {
            let lane = &mut self.subagents.lanes[index];
            if let Some(open) = self.open.as_mut() {
                for line in lane.tail(end == LaneEnd::TurnOver) {
                    push_paragraph(events, open, &line, timestamp);
                }
            }
            lane.closed = true;
        }
        self.close_message(timestamp, events);
        self.flush_due_results(timestamp, events);
    }

    /// 把在播块收起来:子 agent 结束了就收尾,没结束就暂停。
    fn settle_live(&mut self, timestamp: &str, events: &mut Vec<CotEvent>) {
        let Some(index) = self.subagents.live else {
            return;
        };
        self.drain_live(timestamp, events);
        let end = if self.subagents.lanes[index].finished {
            LaneEnd::Finished
        } else {
            LaneEnd::Paused
        };
        self.close_live(end, timestamp, events);
    }

    /// 子 agent 已推出去的工具行,结果只在它自己的块开着、或没有任何块开着时推 —— 绝不落进
    /// 别人的块里(客户端对「结果落在哪个块下」的渲染没有保证)。这里处理后一种时刻。
    fn flush_due_results(&mut self, timestamp: &str, events: &mut Vec<CotEvent>) {
        if self.open.is_some() {
            return;
        }
        for lane in &mut self.subagents.lanes {
            lane.awaiting.retain(|row| {
                if row.done {
                    push_row_result(events, row, timestamp);
                }
                !row.done
            });
        }
    }
}

/// 块首行:标题;思考可见时接上任务说明。压成一行 —— 它是块的名字,不是正文。
fn lane_header(lane: &SubagentLane, thinking_visible: bool) -> String {
    let title = crate::segment::clip_tool_label(lane.title());
    match lane.description().filter(|_| thinking_visible) {
        Some(description) => format!(
            "{title}: {}",
            description.split_whitespace().collect::<Vec<_>>().join(" ")
        ),
        None => title,
    }
}

/// 把游标处起的思考段推进 `open`。停在放行的回复段上时返回 `true`(游标指着它,由调用方关掉
/// `open` 再投文本块);其余情况返回 `false`。
///
/// `released`:`segments[..n]` 里的回复段已确认属于过程;之后的回复段扣住,游标停在它上面
/// 等确认 —— CoT 只追加、撤不回,末段可能就是终态。`None`:回复一律不进 CoT,直接跳过。
fn project_segments_into(
    cursor: &mut usize,
    open: &mut OpenMessage,
    segments: &[TurnSegment],
    released: Option<usize>,
    timestamp: &str,
    events: &mut Vec<CotEvent>,
) -> bool {
    while *cursor < segments.len() {
        let TurnSegment::Thinking(text) = &segments[*cursor] else {
            match released {
                Some(n) if *cursor < n => return true,
                Some(_) => return false,
                None => {
                    *cursor += 1;
                    open.text.clear();
                    continue;
                }
            }
        };
        let delta = thinking_delta(text, &open.text);
        if !delta.is_empty() {
            // 游标为空 = 这个 segment 的第一笔:消息里已有内容就另起一段;续写不加。
            let prefix = if open.has_content && open.text.is_empty() {
                "\n\n"
            } else {
                ""
            };
            push_content(events, &open.id, &format!("{prefix}{delta}"), timestamp);
            open.text = text.to_string();
            open.has_content = true;
        }
        if *cursor + 1 == segments.len() {
            break;
        }
        *cursor += 1;
        open.text.clear();
    }
    false
}

/// 累积快照 `text` 相对已推部分 `pushed` 的增量;不是续写就整段重推。
fn thinking_delta<'a>(text: &'a str, pushed: &str) -> &'a str {
    text.strip_prefix(pushed).unwrap_or(text)
}

/// codex 主线程上子 agent 的派出(`subAgentActivity`)与等待(`collabAgentToolCall`)记录。它们
/// 是主 agent 的行,但不算主 agent 的新内容:在播的子 agent 块不必为它们让位。
fn is_subagent_bookkeeping(tool: &ProtocolTool) -> bool {
    matches!(
        tool.tool_name.as_str(),
        "sub_agent_activity" | "collab_agent_tool_call"
    )
}

/// `include` 为假的工具这一次不投、也不记为已处理。
fn project_main_tools(
    tools: &mut BTreeMap<u64, bool>,
    mut open: Option<&mut OpenMessage>,
    progress: &TurnProgress,
    include: &dyn Fn(&ProtocolTool) -> bool,
    timestamp: &str,
    events: &mut Vec<CotEvent>,
) {
    let parent_message_id = open.as_ref().map(|open| open.id.clone());
    let show_tools = progress.tool_details();
    let thinking_visible = !progress.hide_thinking();
    for tool in progress
        .protocol_tools()
        .iter()
        .filter(|tool| include(tool))
    {
        match tools.get(&tool.id).copied() {
            None => {
                let plan = plan_tool(tool, main_call_id(tool.id), show_tools, thinking_visible);
                if let (Some(text), Some(open)) = (plan.narration.as_deref(), open.as_deref_mut()) {
                    push_paragraph(events, open, text, timestamp);
                }
                // 工具详情关:CoT 只有思考正文,这一步记为已处理,不投任何工具事件。
                let Some(row) = plan.row else {
                    tools.insert(tool.id, true);
                    continue;
                };
                push_tool_row(events, &row, parent_message_id.as_deref(), timestamp);
                tools.insert(tool.id, row.done);
            }
            Some(false) if tool.done => {
                push_tool_result(
                    events,
                    &main_call_id(tool.id),
                    tool.secs,
                    tool.command.as_deref(),
                    tool.language.as_deref(),
                    tool.result.as_deref(),
                    timestamp,
                );
                tools.insert(tool.id, true);
            }
            _ => {}
        }
    }
}

/// 一个工具在 CoT 里的样子:可选的意图段落 + 可选的工具行。主 agent 与子 agent 共用这一条规则。
struct ToolPlan {
    narration: Option<String>,
    row: Option<ToolRow>,
}

/// 工具的 `description` 是模型自己写的这一步意图。claude 自家模型的转录里 thinking 正文是空的,
/// 这句就是这一档拿得到的全部思考正文,受思考开关管。有命令的步骤按思考正文推进 reasoning 区
/// (落在工具之前,读起来是「想做什么 → 做了什么」);没有命令的步骤它进标题(见
/// `tool_call_name`),只有标题放不下全文时才在 reasoning 区再给完整的一句。工具详情关时没有
/// 工具行,它照样进 reasoning 区。
///
/// 不放进 TOOL_CALL_START 的 `title`:那会覆写客户端原生的「思考中」扫光文案。
fn plan_tool(
    tool: &ProtocolTool,
    call_id: String,
    show_tools: bool,
    thinking_visible: bool,
) -> ToolPlan {
    let (name, title_complete) = if show_tools {
        tool_call_name(tool, thinking_visible)
    } else {
        (String::new(), false)
    };
    let narration = if thinking_visible && !title_complete {
        tool.description.clone()
    } else {
        None
    };
    let row = show_tools.then(|| ToolRow {
        id: call_id,
        name,
        icon: tool_icon(&tool.tool_name, tool.command.as_deref()).to_string(),
        command: tool.command.clone(),
        language: tool.language.clone(),
        secs: tool.secs,
        done: tool.done,
        result: tool.result.clone(),
    });
    ToolPlan { narration, row }
}

/// 在消息里另起一段:已有内容就先空一行 —— 客户端把增量原样拼接。
fn push_paragraph(events: &mut Vec<CotEvent>, open: &mut OpenMessage, text: &str, timestamp: &str) {
    let prefix = if open.has_content { "\n\n" } else { "" };
    push_content(events, &open.id, &format!("{prefix}{text}"), timestamp);
    open.has_content = true;
}

fn push_content(events: &mut Vec<CotEvent>, id: &str, text: &str, timestamp: &str) {
    push_deltas(events, "REASONING_MESSAGE_CONTENT", id, text, timestamp);
}

/// 一整段过程回复:客户端用正常字体渲染文本块,与灰色的思考区分开,并留在折叠里。
fn push_text_message(events: &mut Vec<CotEvent>, id: &str, text: &str, timestamp: &str) {
    events.push(CotEvent::new(
        "TEXT_MESSAGE_START",
        serde_json::json!({"messageId": id, "role": "assistant"}),
        timestamp,
    ));
    push_deltas(events, "TEXT_MESSAGE_CONTENT", id, text, timestamp);
    events.push(CotEvent::new(
        "TEXT_MESSAGE_END",
        serde_json::json!({"messageId": id}),
        timestamp,
    ));
}

fn push_deltas(
    events: &mut Vec<CotEvent>,
    event_type: &str,
    id: &str,
    text: &str,
    timestamp: &str,
) {
    let mut chunk = String::new();
    for ch in text.chars() {
        chunk.push(ch);
        if chunk.chars().count() == DELTA_CHARS {
            events.push(CotEvent::new(
                event_type,
                serde_json::json!({"messageId": id, "delta": chunk}),
                timestamp,
            ));
            chunk = String::new();
        }
    }
    if !chunk.is_empty() {
        events.push(CotEvent::new(
            event_type,
            serde_json::json!({"messageId": id, "delta": chunk}),
            timestamp,
        ));
    }
}

/// 工具行:START(+ARGS)+END,已完成的紧跟结果。
fn push_tool_row(
    events: &mut Vec<CotEvent>,
    row: &ToolRow,
    parent_message_id: Option<&str>,
    timestamp: &str,
) {
    let tool_id = &row.id;
    let mut content = serde_json::json!({
        "toolCallId": tool_id,
        "icon": row.icon,
        "toolCallName": row.name
    });
    if let Some(parent) = parent_message_id {
        content["parentMessageId"] = serde_json::Value::String(parent.to_string());
    }
    events.push(CotEvent::new("TOOL_CALL_START", content, timestamp));
    if let Some(command) = &row.command {
        events.push(CotEvent::new(
            "TOOL_CALL_ARGS",
            serde_json::json!({
                "toolCallId": tool_id,
                "delta": serde_json::json!({"command": command}).to_string()
            }),
            timestamp,
        ));
    }
    events.push(CotEvent::new(
        "TOOL_CALL_END",
        serde_json::json!({"toolCallId": tool_id}),
        timestamp,
    ));
    if row.done {
        push_row_result(events, row, timestamp);
    }
}

fn push_row_result(events: &mut Vec<CotEvent>, row: &ToolRow, timestamp: &str) {
    push_tool_result(
        events,
        &row.id,
        row.secs,
        row.command.as_deref(),
        row.language.as_deref(),
        row.result.as_deref(),
        timestamp,
    );
}

fn push_tool_result(
    events: &mut Vec<CotEvent>,
    call_id: &str,
    secs: u64,
    command: Option<&str>,
    language: Option<&str>,
    result: Option<&str>,
    timestamp: &str,
) {
    let content = match (command, language, result) {
        (_, Some(language), Some(result)) => {
            serde_json::json!({"type": "code", "language": language, "code": result})
        }
        (Some(_), None, Some(result)) => serde_json::json!({"type": "code", "code": result}),
        (_, _, Some(result)) => serde_json::Value::String(result.to_string()),
        _ => serde_json::Value::String(summon_i18n::format!("r7bc5024a1b")),
    };
    events.push(CotEvent::new(
        "TOOL_CALL_RESULT",
        serde_json::json!({
            "messageId": result_message_id(call_id),
            "toolCallId": call_id,
            "role": "tool",
            "content": content
        }),
        timestamp,
    ));
}

/// 工具行的标题,以及 description 是否已完整落在标题里。客户端把 `toolCallName` 原样显示成
/// 这一行的名字,而工具标识(`command_execution`、`Bash`)只说明「调了个工具」,种类已经由
/// icon 表达 —— 所以标题给命令正文(压成一行并截断,全文仍在展开区),没有命令时给「工具名 +
/// 对象」。
///
/// 没有命令的步骤,description 是它唯一的「在做什么」(派子 agent 那句任务说明),思考可见时
/// 接在标题后面;它属于思考正文那一档,思考不可见时不出现。
fn tool_call_name(tool: &ProtocolTool, thinking_visible: bool) -> (String, bool) {
    match (&tool.command, &tool.description) {
        (Some(command), _) => (crate::segment::clip_tool_label(command), false),
        (None, Some(description)) if tool.titled_by_description && thinking_visible => {
            let full = format!("{}: {description}", tool.label);
            (
                crate::segment::clip_tool_label(&full),
                !crate::segment::tool_label_is_clipped(&full),
            )
        }
        _ => (tool.label.clone(), false),
    }
}

/// 图标只看工具标识与有没有命令;标题里的对象(查询词、网址)不参与,否则会被内容带偏。
fn tool_icon(tool_name: &str, command: Option<&str>) -> &'static str {
    if command.is_some() {
        return "bash";
    }
    let lower = tool_name.to_ascii_lowercase();
    if lower.contains("mcp")
        || lower.contains("skill")
        || lower.contains("bash")
        || lower.contains("shell")
        || lower.contains("exec")
    {
        "bash"
    } else if lower.contains("read") || tool_name.contains('读') {
        "read"
    } else if lower.contains("write") || lower.contains("edit") || tool_name.contains('写') {
        "write"
    } else if lower.contains("search")
        || lower.contains("find")
        || lower.contains("grep")
        || lower.contains("glob")
        || tool_name.contains('查')
    {
        "search"
    } else if lower.contains("calendar") || tool_name.contains(summon_i18n::source!("ra00f295cfb"))
    {
        "calendar"
    } else if lower.contains("meeting") || tool_name.contains(summon_i18n::source!("r63d30f1a03")) {
        "meeting"
    } else if lower.contains("task") || tool_name.contains(summon_i18n::source!("r5253040db8")) {
        "task"
    } else if lower.contains("doc") || tool_name.contains(summon_i18n::source!("r2687ccdbb1")) {
        "doc"
    } else {
        "default"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{ProgressStep, ToolCallInput, MAX_COMMAND_CHARS};

    #[test]
    fn reply_checkpoints_survive_snapshot_and_change_generation_on_restart() {
        let mut projector = CotProjector::new("chat", "turn", "任务");
        let raw = format!("{}\n\n<summon-feed>摘要</summon-feed>", "甲🙂".repeat(700));
        let events = projector.project_with_replies(
            &[TurnSegment::Reply(raw.clone())],
            Some(1),
            None,
            "1000",
        );
        let progress: Vec<_> = events
            .iter()
            .filter_map(|event| event.reply_delivery)
            .collect();
        assert!(progress.len() > 1);
        let text = crate::feed_tag::strip_feed_tag(&raw);
        assert_eq!(progress.last().unwrap().bytes, text.len());
        assert!(progress
            .iter()
            .all(|reply| text.is_char_boundary(reply.bytes)));
        assert!(events.iter().all(|event| !serde_json::to_string(event)
            .unwrap()
            .contains("reply_delivery")));
        let snapshot = serde_json::to_string(&projector).unwrap();
        let mut restored: CotProjector = serde_json::from_str(&snapshot).unwrap();
        assert_eq!(restored.projected_replies(), [*progress.last().unwrap()]);
        restored.restart_segments();
        assert!(restored.projected_replies().is_empty());
        let events = restored.project_with_replies(
            &[TurnSegment::Reply("新过程".into())],
            Some(1),
            None,
            "1001",
        );
        let next = events
            .iter()
            .find_map(|event| event.reply_delivery)
            .unwrap();
        assert!(next.generation > progress[0].generation);
        assert_eq!(next.segment, 0);
        assert_eq!(next.bytes, "新过程".len());
    }

    #[test]
    fn projector_emits_incremental_reasoning_tool_and_terminal_events() {
        let mut progress = TurnProgress::new().with_turn_id("turn-1");
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let first = projector.project(&[], Some(&progress), "1000");
        assert_eq!(first[0].event_type, "RUN_STARTED");

        progress.tool_started_with_command("运行测试", Some("cargo test"));
        let started = projector.project(&[], Some(&progress), "1001");
        assert_eq!(started[0].event_type, "REASONING_START");
        assert!(started
            .iter()
            .any(|event| event.event_type == "TOOL_CALL_START"));
        assert!(started
            .iter()
            .any(|event| event.event_type == "TOOL_CALL_ARGS"));

        progress.tool_finished(3);
        let finished = projector.project(&[], Some(&progress), "1002");
        assert_eq!(finished[0].event_type, "TOOL_CALL_RESULT");

        let reasoning = vec![TurnSegment::Thinking("先检查边界".into())];
        let opened = projector.project(&reasoning, Some(&progress), "1003");
        assert!(opened
            .iter()
            .any(|event| event.event_type == "REASONING_MESSAGE_CONTENT"
                && event.content.contains("先检查边界")));
        assert!(!opened
            .iter()
            .any(|event| event.event_type == "REASONING_START"));
        let extended = vec![TurnSegment::Thinking("先检查边界，再跑测试".into())];
        let delta = projector.project(&extended, Some(&progress), "1004");
        assert_eq!(delta.len(), 1);
        assert!(delta[0].content.contains("，再跑测试"));

        let terminal = projector.finish("done", "1005");
        assert_eq!(terminal.last().unwrap().event_type, "RUN_FINISHED");
        assert!(terminal
            .iter()
            .any(|event| event.event_type == "REASONING_END"));
    }

    #[test]
    fn reply_segments_never_replace_the_fixed_reasoning_anchor() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let initial = projector.project(&[], None, "1000");
        let segments = vec![
            TurnSegment::Thinking("先分析".into()),
            TurnSegment::Reply("再回答".into()),
        ];
        let events = projector.project(&segments, None, "1001");
        assert!(events.iter().any(|event| {
            event.event_type == "REASONING_MESSAGE_CONTENT" && event.content.contains("先分析")
        }));
        // 思考正文与工具挂载使用同一条 reasoning 消息。
        let anchor = events
            .iter()
            .find(|event| event.event_type == "REASONING_MESSAGE_START")
            .expect("真实思考打开 reasoning 消息");
        let anchor_id: serde_json::Value = serde_json::from_str(&anchor.content).unwrap();
        assert!(!initial
            .iter()
            .any(|event| event.event_type == "REASONING_MESSAGE_CONTENT"));
        let first: serde_json::Value = serde_json::from_str(
            &events
                .iter()
                .find(|event| event.event_type == "REASONING_MESSAGE_CONTENT")
                .unwrap()
                .content,
        )
        .unwrap();
        assert_eq!(first["messageId"], anchor_id["messageId"]);
        assert!(!events.iter().any(|event| event.content.contains("再回答")));
        assert!(!events
            .iter()
            .any(|event| event.event_type.starts_with("TEXT_MESSAGE")));
        assert!(!events
            .iter()
            .any(|event| event.event_type == "REASONING_END"));
    }

    #[test]
    fn redacted_thinking_preserves_run_status_without_an_empty_message() {
        let mut progress = TurnProgress::new();
        progress.note_thinking("", 7);
        let mut projector = CotProjector::new("chat-1", "turn-1", "检查问题");
        let events = projector.project(&[], Some(&progress), "1000");
        // 思考不可见时:reasoning 消息照开(工具要挂在它下面),但**一个字都不编** ——
        // 没有内容就不该造内容。
        assert_eq!(event_types(&events), ["RUN_STARTED"]);
        assert!(
            !events
                .iter()
                .any(|event| event.event_type == "REASONING_MESSAGE_CONTENT"),
            "没有明文思考时不得推 CONTENT:{events:?}"
        );
        assert!(!events.iter().any(|event| event.content.contains("伪造")));
    }

    #[test]
    fn run_started_keeps_the_bounded_task_summary() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "核对飞书 CoT 映射");
        let events = projector.project(&[], None, "1000");
        assert!(events[0].content.contains("核对飞书 CoT 映射"));
    }

    #[test]
    fn command_tool_result_uses_a_code_content_object() {
        let mut progress = TurnProgress::new();
        progress.tool_started_with_command("运行测试", Some("cargo test"));
        progress.tool_finished_with_result(2, Some("test result: ok"));
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let events = projector.project(&[], Some(&progress), "1000");
        let result = events
            .iter()
            .find(|event| event.event_type == "TOOL_CALL_RESULT")
            .expect("应投出工具结果");
        let content: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(content["content"]["type"], "code");
        assert_eq!(content["content"]["language"], "bash");
        assert_eq!(content["content"]["code"], "test result: ok");
    }

    #[test]
    fn file_tool_result_uses_the_verified_language() {
        let mut progress = TurnProgress::new();
        let id =
            progress.tool_started_with_kind_and_language("读取 lib.rs", "Read", None, Some("rust"));
        progress.tool_finished_by_id(id, 1, Some("fn main() {}"));
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let events = projector.project(&[], Some(&progress), "1000");
        let result = events
            .iter()
            .find(|event| event.event_type == "TOOL_CALL_RESULT")
            .expect("应投出工具结果");
        let content: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(content["content"]["type"], "code");
        assert_eq!(content["content"]["language"], "rust");
    }

    #[test]
    fn projector_keeps_results_for_tools_evicted_from_the_display_window() {
        let mut progress = TurnProgress::new().with_max_steps(3);
        let first = progress.tool_started_with_kind("first", "tool_first", None);
        progress.tool_started_with_kind("second", "tool_second", None);
        progress.tool_started_with_kind("third", "tool_third", None);
        progress.tool_started_with_kind("fourth", "tool_fourth", None);
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");

        let started = projector.project(&[], Some(&progress), "1000");
        assert_eq!(
            started
                .iter()
                .filter(|event| event.event_type == "TOOL_CALL_START")
                .count(),
            4
        );

        progress.tool_finished_by_id(first, 4, Some("first result"));
        let finished = projector.project(&[], Some(&progress), "1001");
        assert!(finished.iter().any(|event| {
            event.event_type == "TOOL_CALL_RESULT"
                && event.content.contains("tool-1")
                && event.content.contains("first result")
        }));
    }

    fn tool_call_start(progress: &TurnProgress) -> serde_json::Value {
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let events = projector.project(&[], Some(progress), "1000");
        let started = events
            .iter()
            .find(|event| event.event_type == "TOOL_CALL_START")
            .expect("应投出工具开始");
        serde_json::from_str(&started.content).unwrap()
    }

    #[test]
    fn tool_call_name_shows_the_label_while_the_icon_follows_the_tool_identity() {
        let mut progress = TurnProgress::new();
        progress.tool_started_with_kind("查询资料", "mcp__search_docs", None);
        let content = tool_call_start(&progress);
        assert_eq!(content["toolCallName"], "查询资料");
        assert!(content.get("title").is_none(), "不得覆写原生思考中扫光文案");
        assert_eq!(content["icon"], "bash");
    }

    #[test]
    fn command_tool_call_name_is_the_command_itself() {
        let mut progress = TurnProgress::new();
        progress.tool_started_with_description(
            "合并后隔离验证",
            "Bash",
            "Bash",
            Some("cargo nextest run"),
            Some("bash"),
            Some("合并后隔离验证"),
        );
        assert_eq!(
            tool_call_start(&progress)["toolCallName"],
            "cargo nextest run"
        );
    }

    #[test]
    fn command_tool_call_name_is_one_clipped_line() {
        let mut progress = TurnProgress::new();
        let command = format!(
            "/bin/zsh -lc 'python3 - <<PY\nprint({})\nPY'",
            "1".repeat(80)
        );
        progress.tool_started_with_kind("命令", "command_execution", Some(&command));
        let name = tool_call_start(&progress)["toolCallName"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(name.starts_with("python3 - <<PY print("), "{name}");
        assert!(!name.contains('\n'), "{name}");
        assert_eq!(name.chars().count(), 51, "{name}");
        assert!(name.ends_with('…'), "{name}");
    }

    #[test]
    fn hidden_tool_details_project_no_tool_rows() {
        let mut progress = TurnProgress::new().with_tool_details(false);
        let id = progress.tool_started_with_kind("ls", "command_execution", Some("ls"));
        progress.tool_finished_by_id(id, 1, Some("a.rs"));
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let mut events = projector.project(&[], Some(&progress), "1000");
        events.extend(projector.project(&[], Some(&progress), "1001"));
        assert!(
            !events.iter().any(|e| e.event_type.starts_with("TOOL_CALL")),
            "工具详情关时 CoT 只有思考正文:{events:?}"
        );
    }

    /// description 在「思考 × 详情 × 有无命令」八种组合下的去向:思考关时不出现;思考开时
    /// 恰好出现一次 —— 有工具行且无命令时在标题里,其余在 reasoning 区。
    #[test]
    fn description_appears_once_or_never_across_switches() {
        for thinking in [true, false] {
            for details in [true, false] {
                for command in [Some("grep -rn foo"), None] {
                    let mut progress = TurnProgress::new()
                        .with_hide_thinking(!thinking)
                        .with_tool_details(details);
                    progress.tool_started_with_description(
                        "找入口",
                        "Agent Explore",
                        "Agent",
                        command,
                        command.map(|_| "bash"),
                        Some("找入口"),
                    );
                    let events = CotProjector::new("chat-1", "turn-1", "任务").project(
                        &[],
                        Some(&progress),
                        "1000",
                    );
                    let case = format!("thinking={thinking} details={details} command={command:?}");
                    let in_reasoning = reasoning_deltas(&events)
                        .iter()
                        .filter(|delta| delta.contains("找入口"))
                        .count();
                    let in_title = events
                        .iter()
                        .filter(|e| e.event_type == "TOOL_CALL_START")
                        .filter(|e| e.content.contains("找入口"))
                        .count();
                    let has_rows = events.iter().any(|e| e.event_type.starts_with("TOOL_CALL"));
                    assert_eq!(has_rows, details, "{case}");
                    let expected = match (thinking, details && command.is_none()) {
                        (false, _) => (0, 0),
                        (true, true) => (0, 1),
                        (true, false) => (1, 0),
                    };
                    assert_eq!((in_reasoning, in_title), expected, "{case}");
                }
            }
        }
    }

    /// 工具详情关时,一步工具从在跑到完成、跨快照续投、段边界、收尾,全程不出工具事件,
    /// description 只出现一次。
    #[test]
    fn hidden_tool_details_stay_hidden_across_the_whole_turn() {
        let mut progress = TurnProgress::new().with_tool_details(false);
        let id = progress.tool_started_with_description(
            "找入口",
            "Agent Explore",
            "Agent",
            None,
            None,
            Some("找入口"),
        );
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let mut events = projector.project(&[], Some(&progress), "1000");
        progress.tool_finished_by_id(id, 2, Some("入口在 main.rs"));
        events.extend(projector.project(&[], Some(&progress), "1001"));

        let snapshot = serde_json::to_string(&projector).unwrap();
        let mut resumed: CotProjector = serde_json::from_str(&snapshot).unwrap();
        events.extend(resumed.resume_boundary(
            ::summon_core::i18n::label(CotProjector::RESUME_BOUNDARY_NOTE),
            "1002",
        ));
        events.extend(resumed.project(&[], Some(&progress), "1003"));
        events.extend(resumed.steer_boundary("—— 收到追加 ——", "1004"));
        events.extend(resumed.project(&[], Some(&progress), "1005"));
        events.extend(resumed.finish("done", "1006"));

        assert!(
            !events.iter().any(|e| e.event_type.starts_with("TOOL_CALL")),
            "{events:?}"
        );
        let narrated = events
            .iter()
            .filter(|e| e.event_type == "REASONING_MESSAGE_CONTENT")
            .filter(|e| e.content.contains("找入口"))
            .count();
        assert_eq!(narrated, 1, "{events:?}");
    }

    #[test]
    fn tool_started_in_a_reasoning_snapshot_is_nested_under_that_message() {
        let mut progress = TurnProgress::new();
        progress.tool_started_with_kind("查询资料", "mcp__search_docs", None);
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let events = projector.project(
            &[TurnSegment::Thinking("先确认资料".into())],
            Some(&progress),
            "1000",
        );
        let reasoning_start = events
            .iter()
            .position(|event| event.event_type == "REASONING_MESSAGE_START")
            .unwrap();
        let tool_start = events
            .iter()
            .position(|event| event.event_type == "TOOL_CALL_START")
            .unwrap();
        assert!(reasoning_start < tool_start);
        // 工具挂载认的是锚点的 messageId,与锚点有没有内容无关。
        let content: serde_json::Value = serde_json::from_str(&events[tool_start].content).unwrap();
        assert_eq!(content["parentMessageId"], "reasoning-1");
        assert!(!events[..tool_start]
            .iter()
            .any(|event| event.event_type == "REASONING_END"));
    }

    #[test]
    fn every_tool_is_parented_to_the_fixed_reasoning_anchor() {
        let mut progress = TurnProgress::new();
        progress.tool_started("查询资料");
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let events = projector.project(&[], Some(&progress), "1000");
        let start = events
            .iter()
            .find(|event| event.event_type == "TOOL_CALL_START")
            .unwrap();
        let content: serde_json::Value = serde_json::from_str(&start.content).unwrap();
        assert_eq!(content["parentMessageId"], "reasoning-1");
    }

    #[test]
    fn projector_uses_run_error_instead_of_a_success_terminal_for_failures() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        projector.project(&[TurnSegment::Reply("partial".into())], None, "1000");
        let events = projector.error("provider 执行失败", "PROVIDER_ERROR", "1001");
        assert_eq!(events.last().unwrap().event_type, "RUN_ERROR");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "RUN_FINISHED"));
    }

    #[test]
    fn projector_emits_only_one_terminal_event_across_delivery_retries() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        projector.project(&[], None, "1000");
        let terminal = projector.finish("done", "1001");
        assert_eq!(
            terminal
                .iter()
                .filter(|event| event.event_type == "RUN_FINISHED")
                .count(),
            1
        );
        assert!(projector.finish("done", "1002").is_empty());
        assert!(projector
            .error("late error", "PROVIDER_ERROR", "1003")
            .is_empty());
    }

    #[test]
    fn command_is_clipped_before_it_enters_progress_snapshots() {
        let mut progress = TurnProgress::new();
        progress.tool_started_with_command("执行", Some(&"中".repeat(MAX_COMMAND_CHARS + 10)));
        let ProgressStep::Tool { command, .. } = &progress.steps()[0] else {
            panic!("expected tool")
        };
        let command = command.as_ref().unwrap();
        assert_eq!(command.chars().count(), MAX_COMMAND_CHARS + 1);
        assert!(command.ends_with('…'));
    }

    #[test]
    fn every_projected_event_gets_a_strictly_increasing_timestamp() {
        let mut progress = TurnProgress::new();
        progress.tool_started_with_command("运行测试", Some("cargo test"));
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");

        let mut events = projector.project(&[], Some(&progress), "1000");
        events.extend(projector.project(
            &[TurnSegment::Thinking("继续检查".into())],
            Some(&progress),
            "1000",
        ));
        events.extend(projector.finish("done", "999"));

        let timestamps: Vec<u64> = events
            .iter()
            .map(|event| event.timestamp.parse().unwrap())
            .collect();
        assert!(timestamps.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn tool_result_event_stays_within_the_wire_content_limit() {
        let mut progress = TurnProgress::new();
        progress.tool_started("执行命令");
        progress.tool_finished_with_result(1, Some(&"\\\"".repeat(2_000)));
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let events = projector.project(&[], Some(&progress), "1000");
        let result = events
            .iter()
            .find(|event| event.event_type == "TOOL_CALL_RESULT")
            .expect("应投出工具结果");
        assert!(result.content.chars().count() <= 4096);
    }

    /// 段边界:关掉当前 reasoning 段、开一个新的,**不碰整条 CoT 的终态**。
    ///
    /// 轮还在跑,推 RUN_FINISHED 会把它标成结束。
    #[test]
    fn steer_boundary_opens_a_new_section_without_ending_the_run() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        projector.project(
            &[TurnSegment::Thinking("先看第一个问题".into())],
            None,
            "1000",
        );
        let events = projector.steer_boundary("—— 收到追加 ——", "1001");

        let kinds: Vec<_> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "REASONING_MESSAGE_END",
                "REASONING_END",
                "REASONING_START",
                "REASONING_MESSAGE_START",
                "REASONING_MESSAGE_CONTENT",
            ],
            "先关旧段再开新段,分隔文案落在新段里"
        );
        assert!(
            !events.iter().any(|e| e.event_type == "RUN_FINISHED"),
            "轮还在跑,绝不能推终态"
        );
        // 新段是一条新的 reasoning message,客户端据此渲成两个折叠块。
        let opened: serde_json::Value = serde_json::from_str(&events[2].content).unwrap();
        assert_eq!(opened["messageId"], "reasoning-2");
    }

    /// 切段时已推游标要**搬进**新段:它记的是「当前 Thinking segment 推到哪了」,
    /// 清零会让同一段思考在新段里从头重推一遍。
    #[test]
    fn steer_boundary_carries_the_delta_cursor_into_the_new_section() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        projector.project(&[TurnSegment::Thinking("前半段".into())], None, "1000");
        projector.steer_boundary("—— 收到追加 ——", "1001");
        // 同一个 segment 继续增长:新段里只该出现增量,不该把「前半段」再推一遍。
        let events = projector.project(
            &[TurnSegment::Thinking("前半段后半段".into())],
            None,
            "1002",
        );
        let deltas: Vec<String> = events
            .iter()
            .filter(|e| e.event_type == "REASONING_MESSAGE_CONTENT")
            .map(|e| {
                serde_json::from_str::<serde_json::Value>(&e.content).unwrap()["delta"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(deltas, ["后半段"]);
    }

    /// 续跑那一代的 segments 从 0 重新编号:边界之后的第一段必须完整推出,不能被旧游标吞掉。
    #[test]
    fn restart_boundary_projects_the_renumbered_segments_from_the_start() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        projector.project(
            &[
                TurnSegment::Thinking("第一代思考".into()),
                TurnSegment::Reply("第一代说明".into()),
                TurnSegment::Thinking("第一代收尾".into()),
            ],
            None,
            "1000",
        );
        let boundary = projector.restart_boundary("—— 收到追加 ——", "1001");
        assert!(boundary
            .iter()
            .any(|e| e.event_type == "REASONING_MESSAGE_START"));

        let events = projector.project(&[TurnSegment::Thinking("续跑思考".into())], None, "1002");
        assert_eq!(reasoning_deltas(&events), ["\n\n续跑思考"]);
    }

    #[test]
    fn session_end_closes_reasoning_then_emits_the_notice_before_run_finish() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        projector.project(&[TurnSegment::Thinking("思考".into())], None, "1000");
        let events = projector.finish_session("1001");
        let types = event_types(&events);
        let text = types
            .iter()
            .position(|kind| *kind == "TEXT_MESSAGE_START")
            .unwrap();
        let reasoning = types
            .iter()
            .position(|kind| *kind == "REASONING_MESSAGE_END")
            .unwrap();
        assert!(reasoning < text);
        assert_eq!(types.last(), Some(&"RUN_FINISHED"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "TEXT_MESSAGE_CONTENT"
                && event.content.contains("该会话已结束")));
        assert!(projector.finish_session("1002").is_empty());
    }

    fn event_types(events: &[CotEvent]) -> Vec<&str> {
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect()
    }

    fn message_id(event: &CotEvent) -> String {
        let content: serde_json::Value = serde_json::from_str(&event.content).unwrap();
        content["messageId"].as_str().unwrap().to_string()
    }

    /// 放行的回复段是一个独立的文本块:先关当前 reasoning 消息,文本块之后的思考另开一条。
    #[test]
    fn released_reply_is_a_text_block_between_reasoning_messages() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let events = projector.project_with_replies(
            &[
                TurnSegment::Thinking("先想".into()),
                TurnSegment::Reply("中途说明".into()),
                TurnSegment::Thinking("再想".into()),
            ],
            Some(2),
            None,
            "1000",
        );
        assert_eq!(
            event_types(&events),
            [
                "RUN_STARTED",
                "REASONING_START",
                "REASONING_MESSAGE_START",
                "REASONING_MESSAGE_CONTENT",
                "REASONING_MESSAGE_END",
                "REASONING_END",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "REASONING_START",
                "REASONING_MESSAGE_START",
                "REASONING_MESSAGE_CONTENT",
            ]
        );
        assert!(events[7].content.contains("中途说明"));
        assert_ne!(message_id(&events[1]), message_id(&events[9]));
        assert_eq!(reasoning_deltas(&events), ["先想", "再想"]);
    }

    /// 放行进 CoT 的回复段同样不带末尾的总结标签:CoT 也是用户看得见的正文。
    #[test]
    fn released_reply_drops_the_feed_tag() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let events = projector.project_with_replies(
            &[
                TurnSegment::Reply("中途说明\n<summon-feed>中途总结</summon-feed>".into()),
                TurnSegment::Thinking("再想".into()),
            ],
            Some(1),
            None,
            "1000",
        );
        let text = events
            .iter()
            .find(|event| event.event_type == "TEXT_MESSAGE_CONTENT")
            .expect("放行的回复段要投成文本块");
        assert!(text.content.contains("中途说明"));
        assert!(!text.content.contains("summon-feed"), "{}", text.content);
    }

    /// 放行界之后的回复段扣住:游标停在它上面,放行后才投,且只投一次;之后的思考接着投。
    #[test]
    fn unreleased_reply_is_held_until_released() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let mut segments = vec![
            TurnSegment::Thinking("先想".into()),
            TurnSegment::Reply("可能是终态".into()),
        ];
        let held = projector.project_with_replies(&segments, Some(1), None, "1000");
        assert!(!held
            .iter()
            .any(|e| e.event_type.starts_with("TEXT_MESSAGE")));

        let released = projector.project_with_replies(&segments, Some(2), None, "1001");
        assert_eq!(
            released
                .iter()
                .filter(|e| e.event_type == "TEXT_MESSAGE_START")
                .count(),
            1
        );

        segments.push(TurnSegment::Thinking("后续".into()));
        let after = projector.project_with_replies(&segments, Some(2), None, "1002");
        assert!(!after
            .iter()
            .any(|e| e.event_type.starts_with("TEXT_MESSAGE")));
        assert_eq!(reasoning_deltas(&after), ["后续"]);
    }

    /// 主消息以文本块收尾后,下一段放行回复直接投,前面不预开 reasoning 消息 —— 预开了也只会
    /// 空着被关掉,在两个文本块之间夹一个空折叠块。
    #[test]
    fn a_released_reply_after_a_text_block_opens_no_empty_reasoning_message() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let mut segments = vec![
            TurnSegment::Thinking("先想".into()),
            TurnSegment::Reply("第一段".into()),
        ];
        projector.project_with_replies(&segments, Some(2), None, "1000");
        segments.push(TurnSegment::Reply("第二段".into()));
        segments.push(TurnSegment::Thinking("再想".into()));
        let events = projector.project_with_replies(&segments, Some(3), None, "1001");

        assert_eq!(
            event_types(&events),
            [
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "REASONING_START",
                "REASONING_MESSAGE_START",
                "REASONING_MESSAGE_CONTENT",
            ]
        );
    }

    /// 不放行(`project`)时回复段一律跳过,后面的思考照投、不被它挡住。
    #[test]
    fn replies_are_skipped_without_a_release_bound() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let events = projector.project(
            &[
                TurnSegment::Reply("只属于终态".into()),
                TurnSegment::Thinking("接着想".into()),
            ],
            None,
            "1000",
        );
        assert!(!events
            .iter()
            .any(|e| e.event_type.starts_with("TEXT_MESSAGE")));
        assert!(!events.iter().any(|e| e.content.contains("只属于终态")));
        assert_eq!(reasoning_deltas(&events), ["接着想"]);
    }

    /// 文本块之后的工具挂在新开的 reasoning 消息下:没有父消息的工具是顶层事件,会顶掉折叠。
    #[test]
    fn tools_after_a_text_block_hang_on_a_new_reasoning_message() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let segments = [
            TurnSegment::Thinking("先想".into()),
            TurnSegment::Reply("我去跑测试".into()),
        ];
        projector.project_with_replies(&segments, Some(1), None, "1000");
        let mut progress = TurnProgress::new();
        progress.tool_started_with_command("运行测试", Some("cargo test"));
        let events = projector.project_with_replies(&segments, Some(2), Some(&progress), "1001");

        let text_end = events
            .iter()
            .position(|e| e.event_type == "TEXT_MESSAGE_END")
            .expect("文本块");
        let reopened = events
            .iter()
            .position(|e| e.event_type == "REASONING_MESSAGE_START")
            .expect("文本块之后另开 reasoning 消息");
        let tool = events
            .iter()
            .position(|e| e.event_type == "TOOL_CALL_START")
            .expect("工具行");
        assert!(text_end < reopened && reopened < tool);
        let tool_content: serde_json::Value = serde_json::from_str(&events[tool].content).unwrap();
        assert_eq!(
            tool_content["parentMessageId"].as_str(),
            Some(message_id(&events[reopened]).as_str())
        );
    }

    /// 两种「此刻不该切段」各自守住:锚点没开过(关一个不存在的段会推出孤立的 END),
    /// 以及轮已终态 —— `finish` 保留终态标记,少了终态那半边守卫就会在
    /// RUN_FINISHED 之后再推一对 START。
    #[test]
    fn steer_boundary_is_a_noop_before_the_first_projection_and_after_the_terminal() {
        let mut fresh = CotProjector::new("chat-1", "turn-1", "任务");
        assert!(fresh.steer_boundary("—— 收到追加 ——", "1000").is_empty());

        let mut done = CotProjector::new("chat-1", "turn-1", "任务");
        done.project(&[TurnSegment::Thinking("想想".into())], None, "1000");
        done.finish("done", "1001");
        assert!(
            done.steer_boundary("—— 收到追加 ——", "1002").is_empty(),
            "终态之后不得再开新段"
        );
    }

    /// 锚点只开消息,不写内容。
    ///
    /// 它是为了让工具有 `parentMessageId` 可挂而先开的脚手架,不是思考内容。
    #[test]
    fn an_empty_projection_starts_the_run_without_opening_reasoning() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let events = projector.project(&[], None, "1000");
        assert_eq!(event_types(&events), ["RUN_STARTED"]);
        assert_eq!(
            event_types(&projector.finish("done", "1001")),
            ["RUN_FINISHED"]
        );
    }

    /// 第一段真实思考直接落地,不带前导空行。
    #[test]
    fn the_first_thinking_segment_starts_without_leading_blank_lines() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "测试任务");
        let events = projector.project(&[TurnSegment::Thinking("先确认资料".into())], None, "1000");
        let content = events
            .iter()
            .find(|event| event.event_type == "REASONING_MESSAGE_CONTENT")
            .expect("应投出思考内容");
        let parsed: serde_json::Value = serde_json::from_str(&content.content).unwrap();
        assert_eq!(parsed["delta"], "先确认资料");
    }

    fn reasoning_deltas(events: &[CotEvent]) -> Vec<String> {
        events
            .iter()
            .filter(|e| e.event_type == "REASONING_MESSAGE_CONTENT")
            .map(|e| {
                serde_json::from_str::<serde_json::Value>(&e.content).unwrap()["delta"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// claude 自家模型的转录里 thinking 正文是空的,模型对每一步的意图只写在工具的
    /// `description` 里 —— 那就是这一档拿得到的全部思考正文。它按思考正文进 reasoning 区:
    /// 落在该工具的 TOOL_CALL_START 之前、同一条 reasoning 消息里;不回到 `title` 上去。
    #[test]
    fn tool_description_is_narrated_as_reasoning_before_the_call() {
        let mut progress = TurnProgress::new();
        progress.note_thinking("", 3);
        progress.tool_started_with_description(
            "查这个函数的定义",
            "Bash",
            "Bash",
            Some("grep -rn foo"),
            Some("bash"),
            Some("查这个函数的定义"),
        );
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let events = projector.project(&[], Some(&progress), "1000");
        let anchor = events
            .iter()
            .position(|e| e.event_type == "REASONING_MESSAGE_START")
            .unwrap();
        let narrated = events
            .iter()
            .position(|e| e.event_type == "REASONING_MESSAGE_CONTENT")
            .expect("description 要进 reasoning 区");
        let started = events
            .iter()
            .position(|e| e.event_type == "TOOL_CALL_START")
            .unwrap();
        assert!(
            anchor < narrated && narrated < started,
            "顺序应为 锚点 → 意图 → 工具:{events:?}"
        );
        let anchor_id: serde_json::Value = serde_json::from_str(&events[anchor].content).unwrap();
        let content: serde_json::Value = serde_json::from_str(&events[narrated].content).unwrap();
        assert_eq!(content["messageId"], anchor_id["messageId"]);
        assert_eq!(content["delta"], "查这个函数的定义");
        let start: serde_json::Value = serde_json::from_str(&events[started].content).unwrap();
        assert!(start.get("title").is_none(), "不得覆写原生思考中扫光文案");
        assert_eq!(start["parentMessageId"], anchor_id["messageId"]);
    }

    /// `cot.thinking` 关:description 与思考正文同一档,一起不进 reasoning 区;工具本身照投。
    #[test]
    fn hidden_thinking_keeps_tool_descriptions_out_of_reasoning() {
        let mut progress = TurnProgress::new().with_hide_thinking(true);
        progress.tool_started_with_description(
            "查这个函数的定义",
            "Grep",
            "Grep",
            None,
            None,
            Some("查这个函数的定义"),
        );
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let events = projector.project(&[], Some(&progress), "1000");
        assert!(
            !events
                .iter()
                .any(|e| e.event_type == "REASONING_MESSAGE_CONTENT"),
            "思考正文关着,意图也不该出现:{events:?}"
        );
        let started = events
            .iter()
            .find(|e| e.event_type == "TOOL_CALL_START")
            .expect("应投出工具开始");
        assert!(
            !started.content.contains("查这个函数的定义"),
            "工具标题同样不能带出意图:{}",
            started.content
        );
    }

    #[test]
    fn described_tool_without_command_carries_the_description_in_its_title() {
        let start = |hide_thinking: bool| {
            let mut progress = TurnProgress::new().with_hide_thinking(hide_thinking);
            progress.tool_started_with_description(
                "排查登录失败的根因",
                "Agent Explore",
                "Agent",
                None,
                None,
                Some("排查登录失败的根因"),
            );
            CotProjector::new("chat-1", "turn-1", "任务").project(&[], Some(&progress), "1000")
        };
        let name = |events: &[CotEvent]| {
            let started = events
                .iter()
                .find(|e| e.event_type == "TOOL_CALL_START")
                .expect("应投出工具开始");
            serde_json::from_str::<serde_json::Value>(&started.content).unwrap()["toolCallName"]
                .as_str()
                .unwrap()
                .to_string()
        };

        let visible = start(false);
        assert_eq!(name(&visible), "Agent Explore: 排查登录失败的根因");
        assert!(
            reasoning_deltas(&visible).is_empty(),
            "说明已在标题里,reasoning 区不重复:{visible:?}"
        );

        let hidden = start(true);
        assert_eq!(name(&hidden), "Agent Explore");
    }

    #[test]
    fn clipped_description_title_keeps_the_full_sentence_in_reasoning() {
        let long = "排查飞书群里定时任务尾轮空闲回收没有生效的根因并给出修复方案和回归测试";
        let mut progress = TurnProgress::new();
        progress.tool_started_with_description(
            long,
            "Agent general-purpose",
            "Agent",
            None,
            None,
            Some(long),
        );
        let events =
            CotProjector::new("chat-1", "turn-1", "任务").project(&[], Some(&progress), "1000");
        assert_eq!(reasoning_deltas(&events), [long]);
        let start = events
            .iter()
            .find(|e| e.event_type == "TOOL_CALL_START")
            .expect("应投出工具开始");
        let name = serde_json::from_str::<serde_json::Value>(&start.content).unwrap()
            ["toolCallName"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(name.ends_with('…'), "{name}");
    }

    #[test]
    fn tool_icon_ignores_the_target_in_the_title() {
        let icon = |label: &str, tool: &str| {
            let mut progress = TurnProgress::new();
            progress.tool_started_with_kind(label, tool, None);
            tool_call_start(&progress)["icon"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(icon("Grep readFile", "Grep"), "search");
        assert_eq!(icon("WebSearch how to exec bash", "WebSearch"), "search");
        assert_eq!(icon("glob **/*.rs", "glob"), "search");
    }

    /// `cot.tools` 关收走工具行;description 是思考正文那一档的东西,照样进 reasoning 区。
    #[test]
    fn tool_details_off_still_narrates_the_description() {
        let mut progress = TurnProgress::new().with_tool_details(false);
        progress.tool_started_with_description(
            "查这个函数的定义",
            "Bash",
            "Bash",
            Some("grep -rn foo"),
            Some("bash"),
            Some("查这个函数的定义"),
        );
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let events = projector.project(&[], Some(&progress), "1000");
        assert_eq!(reasoning_deltas(&events), ["查这个函数的定义"]);
        assert!(
            !events.iter().any(|e| e.event_type.starts_with("TOOL_CALL")),
            "工具详情已关:{events:?}"
        );
    }

    /// reasoning 消息里的段落边界:思考正文与工具意图、两条意图、两段思考之间都空一行 ——
    /// 客户端把 CONTENT 增量原样拼接,不空行就连成一句。第一段不带前导空行,同一段的续写也不加。
    #[test]
    fn reasoning_paragraphs_are_separated_by_blank_lines() {
        let mut progress = TurnProgress::new();
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        let first = [TurnSegment::Thinking("先看定义".into())];
        progress.tool_started_with_description(
            "查定义",
            "Bash",
            "Bash",
            Some("grep -rn def"),
            Some("bash"),
            Some("查定义"),
        );
        assert_eq!(
            reasoning_deltas(&projector.project(&first, Some(&progress), "1000")),
            ["先看定义", "\n\n查定义"]
        );
        progress.tool_started_with_description(
            "再看调用点",
            "Bash",
            "Bash",
            Some("grep -rn call"),
            Some("bash"),
            Some("再看调用点"),
        );
        assert_eq!(
            reasoning_deltas(&projector.project(&first, Some(&progress), "1001")),
            ["\n\n再看调用点"]
        );
        let second = [
            TurnSegment::Thinking("先看定义".into()),
            TurnSegment::Thinking("然后改".into()),
        ];
        assert_eq!(
            reasoning_deltas(&projector.project(&second, Some(&progress), "1002")),
            ["\n\n然后改"]
        );
        let grown = [
            TurnSegment::Thinking("先看定义".into()),
            TurnSegment::Thinking("然后改这里".into()),
        ];
        assert_eq!(
            reasoning_deltas(&projector.project(&grown, Some(&progress), "1003")),
            ["这里"],
            "同一段的续写不加空行"
        );
    }

    /// 切段后新段从头算:分隔文案之后的第一段思考要另起一段,而搬进来的游标续写不加。
    #[test]
    fn steer_boundary_note_counts_as_content_of_the_new_section() {
        let mut progress = TurnProgress::new();
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        projector.project(
            &[TurnSegment::Thinking("前半段".into())],
            Some(&progress),
            "1000",
        );
        projector.steer_boundary("—— 收到追加 ——", "1001");
        progress.tool_started_with_description(
            "查定义",
            "Bash",
            "Bash",
            Some("grep -rn def"),
            Some("bash"),
            Some("查定义"),
        );
        assert_eq!(
            reasoning_deltas(&projector.project(
                &[TurnSegment::Thinking("前半段后半段".into())],
                Some(&progress),
                "1002"
            )),
            ["后半段", "\n\n查定义"]
        );
    }
    /// 快照要把「推到哪了」一起带过河:少一个游标,新一代要么把已发过的段重推一遍,
    /// 要么关不掉上一代开着的 message / tool 事件。
    #[test]
    fn projector_snapshot_round_trip_preserves_cursor_and_open_ids() {
        let segments = [
            TurnSegment::Thinking("先看第一个问题".into()),
            TurnSegment::Thinking("再看第二个".into()),
        ];
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        assert!(!projector.project(&segments, None, "1000").is_empty());

        let json = serde_json::to_string(&projector).unwrap();
        let mut restored: CotProjector = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.segment_cursor(), projector.segment_cursor());
        assert!(
            restored.project(&segments, None, "1001").is_empty(),
            "同一份 segments 再投一次不得重复发事件"
        );
    }

    /// 接管的段边界与 steer 同一条投影:开一个新折叠块,文案落在新块里,轮不收终态。
    #[test]
    fn resume_boundary_emits_note_as_new_reasoning_block() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        projector.project(
            &[TurnSegment::Thinking("停机前想到一半".into())],
            None,
            "1000",
        );
        let events = projector.resume_boundary(
            ::summon_core::i18n::label(CotProjector::RESUME_BOUNDARY_NOTE),
            "1001",
        );
        assert!(events
            .iter()
            .any(|e| e.event_type == "REASONING_MESSAGE_CONTENT" && e.content.contains("曾中断")));
        assert!(
            !events.iter().any(|e| e.event_type == "RUN_FINISHED"),
            "轮可能还在跑,段边界绝不能推终态"
        );
    }

    /// 收口那句落在**当前段**里:紧接着就是 complete,为一句话单开一个折叠块只会多出一个空块。
    #[test]
    fn closing_note_lands_in_the_open_section_without_opening_a_new_one() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        projector.project(&[TurnSegment::Thinking("跑到一半".into())], None, "1000");
        let events = projector.closing_note(
            ::summon_core::i18n::label(CotProjector::SHUTDOWN_CLOSE_NOTE),
            "1001",
        );
        assert_eq!(
            events
                .iter()
                .map(|e| e.event_type.as_str())
                .collect::<Vec<_>>(),
            ["REASONING_MESSAGE_CONTENT"]
        );
        assert!(events[0].content.contains("停机中断"));
    }

    /// 终态之后再收口就是重复投递;没有开过段的投影器也没有可落笔的地方。
    #[test]
    fn closing_note_is_a_noop_after_the_terminal() {
        let mut projector = CotProjector::new("chat-1", "turn-1", "任务");
        projector.project(&[TurnSegment::Thinking("想完了".into())], None, "1000");
        projector.finish("done", "1001");
        assert!(projector
            .closing_note(
                ::summon_core::i18n::label(CotProjector::SHUTDOWN_CLOSE_NOTE),
                "1002"
            )
            .is_empty());
    }

    // ---- 子 agent 块 ----

    /// 客户端眼里的一个折叠块。
    #[derive(Debug, Default)]
    struct Block {
        id: String,
        text: String,
        tools: Vec<String>,
        tool_ids: Vec<String>,
        /// 这个块开着时到达的工具结果(toolCallId)。
        results: Vec<String>,
    }

    #[derive(Debug, Default)]
    struct Timeline {
        blocks: Vec<Block>,
        /// 没有任何块开着时到达的工具结果。
        loose_results: Vec<String>,
    }

    impl Timeline {
        fn texts(&self) -> Vec<&str> {
            self.blocks
                .iter()
                .map(|block| block.text.as_str())
                .collect()
        }
    }

    /// 把事件流还原成客户端看到的折叠块,顺带校验结构:同一时刻最多一个开着的块,思考内容与
    /// 工具行只落在开着的那个块里,终态前全部关好。
    fn timeline(events: &[CotEvent]) -> Timeline {
        let mut out = Timeline::default();
        let mut open: Option<usize> = None;
        for event in events {
            let content: serde_json::Value = serde_json::from_str(&event.content).unwrap();
            match event.event_type.as_str() {
                "REASONING_MESSAGE_START" => {
                    assert!(open.is_none(), "上一块没关就开了新块:{events:#?}");
                    out.blocks.push(Block {
                        id: content["messageId"].as_str().unwrap().to_string(),
                        ..Block::default()
                    });
                    open = Some(out.blocks.len() - 1);
                }
                "REASONING_MESSAGE_CONTENT" => {
                    let block = &mut out.blocks[open.expect("CONTENT 落在没开的块上")];
                    assert_eq!(content["messageId"], block.id.as_str());
                    block.text.push_str(content["delta"].as_str().unwrap());
                }
                "TOOL_CALL_START" => {
                    let block = &mut out.blocks[open.expect("工具行落在没开的块上")];
                    assert_eq!(content["parentMessageId"], block.id.as_str());
                    block
                        .tools
                        .push(content["toolCallName"].as_str().unwrap().to_string());
                    block
                        .tool_ids
                        .push(content["toolCallId"].as_str().unwrap().to_string());
                }
                "TOOL_CALL_RESULT" => {
                    let id = content["toolCallId"].as_str().unwrap().to_string();
                    match open {
                        Some(index) => out.blocks[index].results.push(id),
                        None => out.loose_results.push(id),
                    }
                }
                "REASONING_MESSAGE_END" => {
                    let index = open.take().expect("关了一个没开的块");
                    assert_eq!(content["messageId"], out.blocks[index].id.as_str());
                }
                "RUN_FINISHED" | "RUN_ERROR" => {
                    assert!(open.is_none(), "终态前还有块没关:{events:#?}");
                }
                _ => {}
            }
        }
        out
    }

    fn thinking(parts: &[&str]) -> Vec<TurnSegment> {
        parts
            .iter()
            .map(|part| TurnSegment::Thinking((*part).to_string()))
            .collect()
    }

    fn kinds(events: &[CotEvent]) -> Vec<&str> {
        events.iter().map(|e| e.event_type.as_str()).collect()
    }

    fn tool_id(id: u64) -> String {
        format!("tool-{id}")
    }

    /// 第 `slot` 个子 agent(按 CoT 首次见到的顺序,从 1 起)的第 `seq` 个工具。
    fn lane_tool(slot: usize, seq: u64) -> String {
        format!("tool-s{slot}-{seq}")
    }

    fn call<'a>(label: &'a str, tool: &'a str) -> ToolCallInput<'a> {
        ToolCallInput::new(label, tool)
    }

    /// 只有一个子 agent、主 agent 在等:块首行、思考、工具行与结果都实时推;结束即收块,
    /// 不为主 agent 预开空块,主 agent 的下一句才另起一块。
    #[test]
    fn a_single_lane_streams_live_while_the_parent_waits() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let main = thinking(&["派个子 agent"]);
        let mut all = cot.project(&main, Some(&p), "1000");

        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", Some("找入口"));
        p.subagent_thinking("a", "先搜 main");
        let step = cot.project(&main, Some(&p), "1001");
        assert_eq!(
            reasoning_deltas(&step),
            ["Agent Explore: 找入口", "\n\n先搜 main"]
        );
        all.extend(step);

        let grep = p.subagent_tool_started("a", "t1", call("Grep main", "Grep"));
        let step = cot.project(&main, Some(&p), "1002");
        assert!(
            step.iter()
                .any(|e| e.event_type == "TOOL_CALL_START"
                    && e.content.contains(&lane_tool(1, grep))),
            "工具行实时推:{step:#?}"
        );
        all.extend(step);

        p.subagent_tool_finished("a", "t1", 1, Some("src/main.rs"));
        p.subagent_thinking("a", "找到了");
        let step = cot.project(&main, Some(&p), "1003");
        assert!(step.iter().any(|e| e.event_type == "TOOL_CALL_RESULT"));
        assert_eq!(reasoning_deltas(&step), ["\n\n找到了"]);
        all.extend(step);

        p.subagent_finished("a");
        p.set_parent_waiting(false);
        let step = cot.project(&main, Some(&p), "1004");
        assert_eq!(
            kinds(&step),
            ["REASONING_MESSAGE_END", "REASONING_END"],
            "结束即收块,不预开空的主块"
        );
        all.extend(step);

        all.extend(cot.project(&thinking(&["派个子 agent", "拿到结果"]), Some(&p), "1005"));
        all.extend(cot.finish("done", "1006"));
        let timeline = timeline(&all);
        assert_eq!(
            timeline.texts(),
            [
                "派个子 agent",
                "Agent Explore: 找入口\n\n先搜 main\n\n找到了",
                "拿到结果"
            ]
        );
        assert_eq!(timeline.blocks[1].tools, ["Grep main"]);
        assert_eq!(timeline.blocks[1].results, [lane_tool(1, grep)]);
    }

    /// 并发的子 agent 不交错:在播的那个结束前,别的只攒着;它结束后先把已结束的整块推出,
    /// 再让下一个仍在跑的接着播。
    #[test]
    fn concurrent_lanes_never_interleave() {
        let mut p = TurnProgress::new();
        p.set_parent_waiting(true);
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_started("b", "Agent Plan", None);
        p.subagent_started("c", "child_probe", None);
        p.subagent_thinking("b", "b1");
        p.subagent_thinking("a", "a1");
        p.subagent_thinking("c", "c1");
        let mut all = cot.project(&[], Some(&p), "1000");

        p.subagent_thinking("b", "b2");
        let read = p.subagent_tool_started("b", "tb", call("Read plan.md", "Read"));
        p.subagent_tool_finished("b", "tb", 1, Some("# plan"));
        p.subagent_thinking("a", "a2");
        p.subagent_finished("b");
        all.extend(cot.project(&[], Some(&p), "1001"));
        let leaked = ["b1", "c1", "Agent Plan", "child_probe", "plan.md"];
        assert!(
            !all.iter()
                .any(|e| leaked.iter().any(|text| e.content.contains(text))),
            "a 还在播,b/c 一个字都不能出来:{all:#?}"
        );

        p.subagent_thinking("a", "a3");
        p.subagent_finished("a");
        all.extend(cot.project(&[], Some(&p), "1002"));
        p.subagent_thinking("c", "c2");
        all.extend(cot.project(&[], Some(&p), "1003"));
        p.subagent_finished("c");
        all.extend(cot.project(&[], Some(&p), "1004"));
        all.extend(cot.finish("done", "1005"));

        let timeline = timeline(&all);
        assert_eq!(
            timeline.texts(),
            [
                "Agent Explore\n\na1\n\na2\n\na3",
                "Agent Plan\n\nb1\n\nb2",
                "child_probe\n\nc1\n\nc2"
            ]
        );
        assert_eq!(timeline.blocks[1].tools, ["Read plan.md"]);
        assert_eq!(timeline.blocks[1].results, [lane_tool(2, read)]);
    }

    /// 主 agent 不在等(子 agent 在后台跑、主 agent 自己还在干活):子 agent 一律攒到结束再整块推。
    #[test]
    fn lanes_accumulate_until_finished_while_the_parent_is_busy() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.subagent_started("a", "Agent Explore", Some("后台跑测试"));
        p.subagent_thinking("a", "a1");
        let mut all = cot.project(&thinking(&["主线继续"]), Some(&p), "1000");
        let bash = p.subagent_tool_started(
            "a",
            "t",
            call("Bash", "Bash").command("cargo test").language("bash"),
        );
        let busy = thinking(&["主线继续", "再想想"]);
        all.extend(cot.project(&busy, Some(&p), "1001"));
        let lane_text = ["Agent Explore", "a1", "cargo test"];
        assert!(
            !all.iter()
                .any(|e| lane_text.iter().any(|text| e.content.contains(text))),
            "{all:#?}"
        );

        p.subagent_tool_finished("a", "t", 3, Some("ok"));
        p.subagent_finished("a");
        all.extend(cot.project(&busy, Some(&p), "1002"));
        all.extend(cot.finish("done", "1003"));
        let timeline = timeline(&all);
        assert_eq!(
            timeline.texts(),
            ["主线继续\n\n再想想", "Agent Explore: 后台跑测试\n\na1"]
        );
        assert_eq!(timeline.blocks[1].tools, ["cargo test"]);
        assert_eq!(timeline.blocks[1].results, [lane_tool(1, bash)]);
    }

    /// 主 agent 的新内容打断在播块:先把块里已攒的推完、关块,再投主 agent;子 agent 之后有新
    /// 内容时重开一块,块首行后接「（续）」。只有主工具结果时关块但不为它开主块;主 agent 同一段
    /// 思考跨过子 agent 块续写时只推增量。
    #[test]
    fn parent_activity_pauses_the_live_lane_which_resumes_marked_continued() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let ls = p.tool_started_with_kind("ls", "Bash", Some("ls"));
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", Some("找入口"));
        p.subagent_thinking("a", "a1");
        let mut all = cot.project(&[], Some(&p), "1000");

        p.tool_finished_by_id(ls, 1, Some("a.rs"));
        p.subagent_thinking("a", "a2");
        let step = cot.project(&[], Some(&p), "1001");
        assert_eq!(
            kinds(&step),
            [
                "REASONING_MESSAGE_CONTENT",
                "REASONING_MESSAGE_END",
                "REASONING_END",
                "TOOL_CALL_RESULT"
            ],
            "先推完已攒的、关块,再投主工具结果"
        );
        all.extend(step);

        let main = thinking(&["看看 ls 的结果"]);
        all.extend(cot.project(&main, Some(&p), "1002"));
        p.subagent_thinking("a", "a3");
        all.extend(cot.project(&main, Some(&p), "1003"));
        p.subagent_finished("a");
        all.extend(cot.project(&main, Some(&p), "1004"));
        all.extend(cot.project(&thinking(&["看看 ls 的结果，没问题"]), Some(&p), "1005"));
        all.extend(cot.finish("done", "1006"));

        let timeline = timeline(&all);
        assert_eq!(
            timeline.texts(),
            [
                "",
                "Agent Explore: 找入口\n\na1\n\na2",
                "看看 ls 的结果",
                "Agent Explore: 找入口（续）\n\na3",
                "，没问题"
            ]
        );
        assert_eq!(timeline.blocks[0].tools, ["ls"]);
        assert_eq!(timeline.loose_results, [tool_id(ls)]);
    }

    /// 暂停后再没有新内容就结束的子 agent,不补一个只有「（续）」的空块;它在飞工具的结果等到
    /// 没有别的块开着时再推,绝不落进主 agent 的块里。
    #[test]
    fn a_paused_lane_finishing_with_nothing_new_opens_no_empty_block() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        let grep = p.subagent_tool_started("a", "t", call("Grep foo", "Grep"));
        let mut all = cot.project(&[], Some(&p), "1000");
        let main = thinking(&["主 agent 插了一句"]);
        all.extend(cot.project(&main, Some(&p), "1001"));
        p.subagent_tool_finished("a", "t", 2, Some("hit"));
        all.extend(cot.project(&main, Some(&p), "1002"));
        p.subagent_finished("a");
        all.extend(cot.project(&main, Some(&p), "1003"));
        assert!(
            !all.iter().any(|e| e.event_type == "TOOL_CALL_RESULT"),
            "主块开着,子 agent 的结果不能落进去:{all:#?}"
        );
        all.extend(cot.finish("done", "1004"));
        let timeline = timeline(&all);
        assert_eq!(timeline.texts(), ["Agent Explore", "主 agent 插了一句"]);
        assert_eq!(timeline.blocks[0].tool_ids, [lane_tool(1, grep)]);
        assert_eq!(timeline.loose_results, [lane_tool(1, grep)]);
    }

    /// 每个子 agent 最多 20 行工具;超出的不投(结果也不投),块尾一行计数。上限按块各算各的。
    #[test]
    fn each_lane_shows_at_most_twenty_tool_rows_and_counts_the_rest() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_started("b", "Agent Plan", None);
        let mut ids = Vec::new();
        for i in 0..23 {
            ids.push(p.subagent_tool_started(
                "a",
                &format!("a{i}"),
                call(&format!("Read a{i}.rs"), "Read"),
            ));
        }
        for i in 0..21 {
            p.subagent_tool_started(
                "b",
                &format!("b{i}"),
                call(&format!("Read b{i}.rs"), "Read"),
            );
        }
        let mut all = cot.project(&[], Some(&p), "1000");
        assert!(
            !reasoning_deltas(&all)
                .iter()
                .any(|delta| delta.contains("另有")),
            "还在跑,计数行等块尾再给"
        );
        for i in 23..25 {
            ids.push(p.subagent_tool_started(
                "a",
                &format!("a{i}"),
                call(&format!("Read a{i}.rs"), "Read"),
            ));
        }
        p.subagent_tool_finished("a", "a0", 1, Some("first"));
        p.subagent_tool_finished("a", "a21", 1, Some("hidden"));
        all.extend(cot.project(&[], Some(&p), "1001"));
        p.subagent_finished("a");
        p.subagent_finished("b");
        all.extend(cot.project(&[], Some(&p), "1002"));
        all.extend(cot.finish("done", "1003"));

        let timeline = timeline(&all);
        let a = &timeline.blocks[0];
        assert_eq!(a.tools.len(), MAX_LANE_TOOL_ROWS);
        assert_eq!(a.tools.first().map(String::as_str), Some("Read a0.rs"));
        assert_eq!(a.tools.last().map(String::as_str), Some("Read a19.rs"));
        assert_eq!(a.text, "Agent Explore\n\n… 另有 5 次工具调用未展示");
        assert_eq!(a.results, [lane_tool(1, ids[0])]);
        let b = &timeline.blocks[1];
        assert_eq!(b.tools.len(), MAX_LANE_TOOL_ROWS);
        assert_eq!(b.text, "Agent Plan\n\n… 另有 1 次工具调用未展示");
        let hidden = format!("\"{}\"", lane_tool(1, ids[21]));
        assert!(
            !all.iter().any(|e| e.content.contains(&hidden)),
            "超出上限的工具连结果也不投"
        );
    }

    /// 轮收尾:终态前把攒着的子 agent 按开始顺序整块推出,还没结束的块尾标「（仍在运行）」;
    /// 之后再来的子 agent 事件一律不投。
    #[test]
    fn finish_flushes_pending_lanes_and_marks_the_unfinished() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        p.subagent_started("b", "Agent Plan", None);
        p.subagent_thinking("b", "b1");
        let mut all = cot.project(&thinking(&["主线"]), Some(&p), "1000");
        assert!(!all.iter().any(|e| e.content.contains("a1")));
        let terminal = cot.finish("done", "1001");
        assert_eq!(terminal.last().unwrap().event_type, "RUN_FINISHED");
        all.extend(terminal);
        assert_eq!(
            timeline(&all).texts(),
            [
                "主线",
                "Agent Explore\n\na1\n\n（仍在运行）",
                "Agent Plan\n\nb1\n\n（仍在运行）"
            ]
        );

        p.subagent_thinking("a", "迟到的");
        assert!(cot
            .project(&thinking(&["主线"]), Some(&p), "1002")
            .is_empty());
    }

    /// 出错收尾同样先推子 agent:在播的块就地补计数行与「（仍在运行）」,不另开「（续）」块。
    #[test]
    fn error_completes_the_live_lane_in_place_before_the_terminal() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        for i in 0..21 {
            p.subagent_tool_started("a", &format!("t{i}"), call("Read x.rs", "Read"));
        }
        let mut all = cot.project(&[], Some(&p), "1000");
        p.subagent_started("b", "Agent Plan", None);
        p.subagent_thinking("b", "b1");
        all.extend(cot.project(&[], Some(&p), "1001"));
        let terminal = cot.error("provider 执行失败", "PROVIDER_ERROR", "1002");
        assert_eq!(terminal.last().unwrap().event_type, "RUN_ERROR");
        all.extend(terminal);
        let timeline = timeline(&all);
        assert_eq!(
            timeline.texts(),
            [
                "Agent Explore\n\n… 另有 1 次工具调用未展示\n\n（仍在运行）",
                "Agent Plan\n\nb1\n\n（仍在运行）"
            ]
        );
    }

    /// 暂停过、还有攒着内容的子 agent,收尾时重开的块同样带「（续）」。
    #[test]
    fn finish_reopens_a_paused_lane_marked_continued() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        let mut all = cot.project(&[], Some(&p), "1000");
        all.extend(cot.project(&thinking(&["主线"]), Some(&p), "1001"));
        p.set_parent_waiting(false);
        p.subagent_thinking("a", "a2");
        all.extend(cot.project(&thinking(&["主线"]), Some(&p), "1002"));
        assert!(!all.iter().any(|e| e.content.contains("a2")));
        all.extend(cot.finish("done", "1003"));
        assert_eq!(
            timeline(&all).texts(),
            [
                "Agent Explore\n\na1",
                "主线",
                "Agent Explore（续）\n\na2\n\n（仍在运行）"
            ]
        );
    }

    /// 四种开关组合下子 agent 块的内容:块首行永远在;任务说明与思考只在思考可见时出现;工具行、
    /// 结果与计数只在工具详情开时出现;详情关时工具的意图按思考正文进块。
    #[test]
    fn lane_block_contents_follow_the_thinking_and_tool_detail_switches() {
        for thinking_visible in [true, false] {
            for details in [true, false] {
                let case = format!("thinking={thinking_visible} details={details}");
                let mut p = TurnProgress::new()
                    .with_hide_thinking(!thinking_visible)
                    .with_tool_details(details);
                p.set_parent_waiting(true);
                p.subagent_started("a", "Agent Explore", Some("找入口"));
                p.subagent_thinking("a", "先搜");
                p.subagent_tool_started(
                    "a",
                    "t1",
                    call("Grep main", "Grep").description("搜主函数"),
                );
                p.subagent_tool_started(
                    "a",
                    "t2",
                    call("Bash", "Bash")
                        .command("ls src")
                        .language("bash")
                        .description("列目录"),
                );
                p.subagent_tool_finished("a", "t1", 1, Some("hit"));
                for i in 0..20 {
                    p.subagent_tool_started("a", &format!("x{i}"), call("Read x.rs", "Read"));
                }
                p.subagent_finished("a");
                let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
                let mut all = cot.project(&[], Some(&p), "1000");
                all.extend(cot.finish("done", "1001"));

                let timeline = timeline(&all);
                assert_eq!(timeline.blocks.len(), 1, "{case}");
                let lane = &timeline.blocks[0];
                let (text, first_tools) = match (thinking_visible, details) {
                    (true, true) => (
                        "Agent Explore: 找入口\n\n先搜\n\n列目录\n\n… 另有 2 次工具调用未展示",
                        vec!["Grep main: 搜主函数", "ls src"],
                    ),
                    (true, false) => (
                        "Agent Explore: 找入口\n\n先搜\n\n搜主函数\n\n列目录",
                        vec![],
                    ),
                    (false, true) => (
                        "Agent Explore\n\n… 另有 2 次工具调用未展示",
                        vec!["Grep main", "ls src"],
                    ),
                    (false, false) => ("Agent Explore", vec![]),
                };
                assert_eq!(lane.text, text, "{case}");
                assert_eq!(
                    lane.tools
                        .iter()
                        .take(2)
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    first_tools,
                    "{case}"
                );
                assert_eq!(
                    lane.tools.len(),
                    if details { MAX_LANE_TOOL_ROWS } else { 0 },
                    "{case}"
                );
                assert_eq!(
                    all.iter().any(|e| e.event_type.starts_with("TOOL_CALL")),
                    details,
                    "{case}"
                );
                assert_eq!(lane.results.len(), usize::from(details), "{case}");
            }
        }
    }

    /// 子 agent 的工具结果只配本 lane:同名 provider id 的别的 lane、主 agent 的工具都不受影响。
    #[test]
    fn lane_tool_results_pair_only_within_their_lane() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let agent = p.tool_started_with_kind("Agent Explore", "Agent", None);
        p.set_parent_waiting(true);
        let bash = |command| call("Bash", "Bash").command(command).language("bash");
        let a = p.subagent_tool_started("a", "call_1", bash("echo a"));
        let b = p.subagent_tool_started("b", "call_1", bash("echo b"));
        let mut all = cot.project(&[], Some(&p), "1000");
        p.subagent_tool_finished("a", "call_1", 1, Some("a out"));
        p.subagent_tool_finished("x", "call_1", 1, Some("nobody"));
        all.extend(cot.project(&[], Some(&p), "1001"));
        p.subagent_finished("a");
        p.subagent_finished("b");
        all.extend(cot.project(&[], Some(&p), "1002"));
        all.extend(cot.finish("done", "1003"));

        let timeline = timeline(&all);
        assert_eq!(timeline.texts(), ["", "a", "b"]);
        assert_eq!(timeline.blocks[0].tool_ids, [tool_id(agent)]);
        assert_eq!(timeline.blocks[1].tool_ids, [lane_tool(1, a)]);
        assert_eq!(timeline.blocks[1].results, [lane_tool(1, a)]);
        assert_eq!(timeline.blocks[2].tool_ids, [lane_tool(2, b)]);
        assert!(timeline.blocks[2].results.is_empty());
        assert!(timeline.loose_results.is_empty());
        let result = all
            .iter()
            .find(|e| e.event_type == "TOOL_CALL_RESULT")
            .unwrap();
        assert!(result.content.contains("a out"));
        assert!(!all.iter().any(|e| e.content.contains("nobody")));
    }

    fn round_trip(cot: &CotProjector) -> CotProjector {
        serde_json::from_str(&serde_json::to_string(cot).unwrap()).unwrap()
    }

    /// 调度状态随快照过河:接管后同一份进度不重推,在播/攒着的 lane 接着排。
    #[test]
    fn lane_schedule_survives_a_snapshot_round_trip() {
        let mut p = TurnProgress::new();
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        p.subagent_started("b", "Agent Plan", None);
        p.subagent_thinking("b", "b1");
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let mut all = cot.project(&[], Some(&p), "1000");

        let mut cot = round_trip(&cot);
        assert!(
            cot.project(&[], Some(&p), "1001").is_empty(),
            "同一份进度再投不得重复"
        );
        p.subagent_thinking("a", "a2");
        p.subagent_finished("a");
        all.extend(cot.project(&[], Some(&p), "1002"));

        let mut cot = round_trip(&cot);
        p.subagent_thinking("b", "b2");
        p.subagent_finished("b");
        all.extend(cot.project(&[], Some(&p), "1003"));
        all.extend(cot.finish("done", "1004"));
        assert_eq!(
            timeline(&all).texts(),
            ["Agent Explore\n\na1\n\na2", "Agent Plan\n\nb1\n\nb2"]
        );
    }

    /// 接管后的进度从头重放,子 agent 在里面的先后是新一代读到的先后,与快照无关。上一代 a 已
    /// 收块、b 播到一半暂停、c 已派出还没出声;重启后 c 先被读到且已结束 —— 它也只能排在 b 后面,
    /// 不能夹进 b 与 b（续）之间。
    #[test]
    fn a_lane_first_seen_after_takeover_never_cuts_into_a_paused_lane() {
        let mut p = TurnProgress::new();
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        p.subagent_finished("a");
        p.subagent_started("b", "Agent Plan", None);
        p.subagent_thinking("b", "b1");
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let mut all = cot.project(&[], Some(&p), "1000");
        p.set_parent_waiting(false);
        all.extend(cot.project(&[], Some(&p), "1001"));

        let mut cot = round_trip(&cot);
        let mut p = TurnProgress::new();
        p.subagent_started("c", "Agent Review", None);
        p.subagent_thinking("c", "c1");
        p.subagent_finished("c");
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        p.subagent_finished("a");
        p.subagent_started("b", "Agent Plan", None);
        p.subagent_thinking("b", "b1");
        let step = cot.project(&[], Some(&p), "1002");
        assert!(
            !step
                .iter()
                .any(|e| e.content.contains("c1") || e.content.contains("Agent Review")),
            "b 暂停着,c 不许插进来:{step:#?}"
        );
        all.extend(step);

        p.subagent_thinking("b", "b2");
        p.subagent_finished("b");
        all.extend(cot.project(&[], Some(&p), "1003"));
        all.extend(cot.finish("done", "1004"));
        assert_eq!(
            timeline(&all).texts(),
            [
                "Agent Explore\n\na1",
                "Agent Plan\n\nb1",
                "Agent Plan（续）\n\nb2",
                "Agent Review\n\nc1"
            ]
        );
    }

    /// 攒着的内容也在快照里:接管后的收尾手里没有进度,照样能把它推出来。
    #[test]
    fn queued_lane_content_is_flushed_from_the_snapshot_alone() {
        let mut p = TurnProgress::new();
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let mut all = cot.project(&thinking(&["主线"]), Some(&p), "1000");
        let mut cot = round_trip(&cot);
        all.extend(cot.finish("done", "1001"));
        assert_eq!(
            timeline(&all).texts(),
            ["主线", "Agent Explore\n\na1\n\n（仍在运行）"]
        );
    }

    /// 不带子 agent 调度字段的快照照样读得回,其中的游标照常生效。
    #[test]
    fn a_snapshot_from_before_subagent_lanes_still_deserializes() {
        let old = r#"{"thread_id":"chat-1","run_id":"turn-1","started":true,
            "terminal_emitted":false,"reasoning_anchor_opened":true,
            "open":{"id":"reasoning-1","text":"想到一半","has_content":true},
            "segment_cursor":0,"next_message_id":2,"tools":{"1":false},
            "query":"任务","last_timestamp":1000}"#;
        let mut cot: CotProjector = serde_json::from_str(old).unwrap();
        let mut p = TurnProgress::new();
        p.tool_started_with_kind("ls", "Bash", Some("ls"));
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        p.subagent_finished("a");
        let events = cot.project(&thinking(&["想到一半"]), Some(&p), "1001");
        assert_eq!(
            kinds(&events),
            [
                "REASONING_MESSAGE_END",
                "REASONING_END",
                "REASONING_START",
                "REASONING_MESSAGE_START",
                "REASONING_MESSAGE_CONTENT",
                "REASONING_MESSAGE_CONTENT",
                "REASONING_MESSAGE_END",
                "REASONING_END",
            ],
            "旧游标生效:不重推思考、不重开工具,只推新来的子 agent 块"
        );
        let closed: serde_json::Value = serde_json::from_str(&events[0].content).unwrap();
        assert_eq!(closed["messageId"], "reasoning-1");
        let opened: serde_json::Value = serde_json::from_str(&events[3].content).unwrap();
        assert_eq!(opened["messageId"], "reasoning-2");
        assert_eq!(reasoning_deltas(&events), ["Agent Explore", "\n\na1"]);
    }

    /// 在播时来了轮内追加:先关子 agent 块,再在新的主块里写分隔文案;主 agent 的思考游标
    /// 搬进新块,子 agent 之后以「（续）」接着播。
    #[test]
    fn steer_boundary_closes_the_live_lane_before_opening_the_new_section() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let mut all = cot.project(&thinking(&["前半段"]), Some(&p), "1000");
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        all.extend(cot.project(&thinking(&["前半段"]), Some(&p), "1001"));
        let step = cot.steer_boundary("—— 收到追加 ——\n\n", "1002");
        assert_eq!(
            kinds(&step),
            [
                "REASONING_MESSAGE_END",
                "REASONING_END",
                "REASONING_START",
                "REASONING_MESSAGE_START",
                "REASONING_MESSAGE_CONTENT",
            ]
        );
        all.extend(step);
        all.extend(cot.project(&thinking(&["前半段后半段"]), Some(&p), "1003"));
        p.subagent_thinking("a", "a2");
        all.extend(cot.project(&thinking(&["前半段后半段"]), Some(&p), "1004"));
        all.extend(cot.finish("done", "1005"));
        assert_eq!(
            timeline(&all).texts(),
            [
                "前半段",
                "Agent Explore\n\na1",
                "—— 收到追加 ——\n\n后半段",
                "Agent Explore（续）\n\na2\n\n（仍在运行）"
            ]
        );
    }

    /// 停机收口那句不能写进子 agent 的块:先关块,再落在主 agent 的块里。
    #[test]
    fn closing_note_never_lands_inside_a_lane_block() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        let mut all = cot.project(&[], Some(&p), "1000");
        let step = cot.closing_note(
            ::summon_core::i18n::label(CotProjector::SHUTDOWN_CLOSE_NOTE),
            "1001",
        );
        assert_eq!(
            kinds(&step),
            [
                "REASONING_MESSAGE_END",
                "REASONING_END",
                "REASONING_START",
                "REASONING_MESSAGE_START",
                "REASONING_MESSAGE_CONTENT",
            ]
        );
        all.extend(step);
        assert_eq!(
            timeline(&all).texts(),
            [
                "Agent Explore\n\na1",
                ::summon_core::i18n::label(CotProjector::SHUTDOWN_CLOSE_NOTE)
            ]
        );
    }

    /// 主 agent 不再等(`wait` 结束)、也没有新内容时,在播块照样收起;之后子 agent 的内容攒着,
    /// 等下次开播或收尾再以「（续）」推出。
    #[test]
    fn a_live_lane_is_put_away_once_the_parent_stops_waiting() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        let mut all = cot.project(&[], Some(&p), "1000");

        p.set_parent_waiting(false);
        let step = cot.project(&[], Some(&p), "1001");
        assert_eq!(kinds(&step), ["REASONING_MESSAGE_END", "REASONING_END"]);
        all.extend(step);

        p.subagent_thinking("a", "a2");
        assert!(
            cot.project(&[], Some(&p), "1002").is_empty(),
            "不在等就只攒着"
        );
        all.extend(cot.finish("done", "1003"));
        assert_eq!(
            timeline(&all).texts(),
            [
                "Agent Explore\n\na1",
                "Agent Explore（续）\n\na2\n\n（仍在运行）"
            ]
        );
    }

    /// 开始最早、还没结束的子 agent 暂停期间,比它晚开始的哪怕已经结束也不推:它的块会被隔成两截。
    /// 等它结束(或轮收尾)再按开始顺序推。
    #[test]
    fn a_later_lane_never_cuts_into_a_paused_earlier_lane() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.set_parent_waiting(true);
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "a1");
        let mut all = cot.project(&[], Some(&p), "1000");
        let main = thinking(&["主 agent 插话"]);
        all.extend(cot.project(&main, Some(&p), "1001"));

        p.subagent_started("b", "Agent Plan", None);
        p.subagent_thinking("b", "b1");
        p.subagent_finished("b");
        let step = cot.project(&main, Some(&p), "1002");
        assert!(
            !step
                .iter()
                .any(|e| e.content.contains("b1") || e.content.contains("Agent Plan")),
            "a 暂停着,b 不许插进来:{step:#?}"
        );
        all.extend(step);

        p.subagent_thinking("a", "a2");
        all.extend(cot.project(&main, Some(&p), "1003"));
        p.subagent_finished("a");
        all.extend(cot.project(&main, Some(&p), "1004"));
        all.extend(cot.finish("done", "1005"));
        assert_eq!(
            timeline(&all).texts(),
            [
                "Agent Explore\n\na1",
                "主 agent 插话",
                "Agent Explore（续）\n\na2",
                "Agent Plan\n\nb1"
            ]
        );
    }

    /// codex 主线程的 `sub_agent_activity` / `collab_agent_tool_call` 是子 agent 的派出与等待记录,
    /// 不算主 agent 的新内容:在播块不为它们让位,它们等块收起后落进主 agent 的块。
    #[test]
    fn codex_subagent_bookkeeping_rows_do_not_pause_the_live_lane() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        p.set_parent_waiting(true);
        p.subagent_started("a", "child_a", None);
        p.subagent_thinking("a", "a1");
        let mut all = cot.project(&[], Some(&p), "1000");

        let spawn = p.tool_started_with_kind(
            "sub_agent_activity child_b started",
            "sub_agent_activity",
            None,
        );
        let wait = p.tool_started_with_kind("wait", "collab_agent_tool_call", None);
        p.tool_finished_by_id(spawn, 0, None);
        p.subagent_thinking("a", "a2");
        let step = cot.project(&[], Some(&p), "1001");
        assert_eq!(kinds(&step), ["REASONING_MESSAGE_CONTENT"], "{step:#?}");
        all.extend(step);

        p.subagent_thinking("a", "a3");
        p.subagent_finished("a");
        p.tool_finished_by_id(wait, 3, None);
        all.extend(cot.project(&[], Some(&p), "1002"));
        all.extend(cot.finish("done", "1003"));
        let timeline = timeline(&all);
        assert_eq!(timeline.texts(), ["child_a\n\na1\n\na2\n\na3", ""]);
        assert_eq!(
            timeline.blocks[1].tools,
            ["sub_agent_activity child_b started", "wait"]
        );
        assert_eq!(timeline.blocks[1].results, [tool_id(spawn), tool_id(wait)]);
    }

    /// 每个子 agent 最多 20 段思考;超出的不投,块尾与工具计数并成一行。工具详情关时工具的意图
    /// 就是思考正文,同样计入;思考不可见时既不投也不计数。
    #[test]
    fn each_lane_shows_at_most_twenty_thinking_paragraphs_and_counts_the_rest() {
        let mut p = TurnProgress::new();
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_started("b", "Agent Plan", None);
        for i in 0..23 {
            p.subagent_thinking("a", &format!("a{i}"));
        }
        for i in 0..21 {
            p.subagent_tool_started("a", &format!("t{i}"), call("Read x.rs", "Read"));
        }
        for i in 0..21 {
            p.subagent_thinking("b", &format!("b{i}"));
        }
        p.subagent_finished("a");
        p.subagent_finished("b");
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let mut all = cot.project(&[], Some(&p), "1000");
        all.extend(cot.finish("done", "1001"));
        let tl = timeline(&all);
        let shown_a: Vec<String> = (0..20).map(|i| format!("a{i}")).collect();
        assert_eq!(
            tl.blocks[0].text,
            format!(
                "Agent Explore\n\n{}\n\n… 另有 3 段思考、1 次工具调用未展示",
                shown_a.join("\n\n")
            )
        );
        let shown_b: Vec<String> = (0..20).map(|i| format!("b{i}")).collect();
        assert_eq!(
            tl.blocks[1].text,
            format!(
                "Agent Plan\n\n{}\n\n… 另有 1 段思考未展示",
                shown_b.join("\n\n")
            )
        );

        let mut p = TurnProgress::new().with_tool_details(false);
        for i in 0..22 {
            p.subagent_tool_started(
                "a",
                &format!("t{i}"),
                call("Read x.rs", "Read").description(&format!("看 {i}")),
            );
        }
        p.subagent_finished("a");
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let mut all = cot.project(&[], Some(&p), "1000");
        all.extend(cot.finish("done", "1001"));
        let text = &timeline(&all).blocks[0].text;
        assert!(text.ends_with("看 19\n\n… 另有 2 段思考未展示"), "{text}");

        let mut p = TurnProgress::new().with_hide_thinking(true);
        for i in 0..25 {
            p.subagent_thinking("a", &format!("a{i}"));
        }
        p.subagent_finished("a");
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let mut all = cot.project(&[], Some(&p), "1000");
        all.extend(cot.finish("done", "1001"));
        assert_eq!(timeline(&all).blocks[0].text, "a");
    }

    /// 子 agent 工具的 toolCallId 只由它在 CoT 里的排位与 lane 内序号决定,与主 agent 的编号无关。
    #[test]
    fn lane_tool_call_ids_are_independent_of_main_tool_ids() {
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat-1", "turn-1", "任务");
        let main = p.tool_started_with_kind("ls", "Bash", Some("ls"));
        p.subagent_tool_started("a", "c1", call("Read a.rs", "Read"));
        p.subagent_tool_started("b", "c1", call("Read b.rs", "Read"));
        p.subagent_tool_started("a", "c2", call("Read c.rs", "Read"));
        p.subagent_finished("a");
        p.subagent_finished("b");
        let mut all = cot.project(&[], Some(&p), "1000");
        all.extend(cot.finish("done", "1001"));
        let timeline = timeline(&all);
        assert_eq!(timeline.blocks[0].tool_ids, [tool_id(main)]);
        assert_eq!(
            timeline.blocks[1].tool_ids,
            [lane_tool(1, 1), lane_tool(1, 2)]
        );
        assert_eq!(timeline.blocks[2].tool_ids, [lane_tool(2, 1)]);
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    #[derive(Default)]
    struct FuzzLane {
        finished: bool,
        running: Vec<String>,
        tools: usize,
        thoughts: Vec<String>,
        rows: Vec<String>,
        done_rows: Vec<String>,
    }

    /// 块里一段话或一行工具归谁:主 agent 的以 `M` 起头(续写的增量是 `m`),子 agent k 的以
    /// `S{k}` 起头、块首行是 `L{k}`;计数行与「（仍在运行）」不归谁。
    fn fuzz_owner(text: &str) -> Option<String> {
        let text = text.trim();
        if text.is_empty() || text.starts_with('…') || text.starts_with('（') {
            return None;
        }
        if text.starts_with(['M', 'm']) {
            return Some("main".into());
        }
        let rest = text.strip_prefix(['S', 'L']).expect(text);
        let k: String = rest.chars().take_while(char::is_ascii_digit).collect();
        Some(format!("lane{k}"))
    }

    fn run_lane_fuzz(seed: u64) {
        let mut rng = Rng(seed);
        let mut p = TurnProgress::new();
        let mut cot = CotProjector::new("chat", "turn", "任务");
        let mut segs: Vec<TurnSegment> = Vec::new();
        let mut all = Vec::new();
        let mut waiting = false;
        let mut main_running: Vec<u64> = Vec::new();
        let mut lanes: Vec<FuzzLane> = Vec::new();
        let mut t = 1000u64;
        for n in 0..rng.below(150) + 10 {
            match rng.below(14) {
                0 => segs.push(TurnSegment::Thinking(format!("M{n}"))),
                1 => {
                    if let Some(TurnSegment::Thinking(last)) = segs.last_mut() {
                        last.push('m');
                    }
                }
                2 => {
                    let label = format!("M{n}tool");
                    main_running.push(p.tool_started_with_kind(&label, "Bash", Some(&label)));
                }
                3 if !main_running.is_empty() => {
                    let i = rng.below(main_running.len() as u64) as usize;
                    p.tool_finished_by_id(main_running.remove(i), 1, Some("out"));
                }
                4 => {
                    let kind = if rng.below(2) == 0 {
                        "sub_agent_activity"
                    } else {
                        "collab_agent_tool_call"
                    };
                    p.tool_started_with_kind(&format!("M{n}book"), kind, None);
                }
                5 => {
                    waiting = !waiting;
                    p.set_parent_waiting(waiting);
                }
                6 => {
                    let k = lanes.len();
                    p.subagent_started(&format!("k{k}"), &format!("L{k}"), None);
                    lanes.push(FuzzLane::default());
                }
                op @ 7..=10 => {
                    let open: Vec<usize> =
                        (0..lanes.len()).filter(|&k| !lanes[k].finished).collect();
                    if open.is_empty() {
                        continue;
                    }
                    let k = open[rng.below(open.len() as u64) as usize];
                    let key = format!("k{k}");
                    let lane = &mut lanes[k];
                    match op {
                        7 => {
                            let text = format!("S{k}t{n}");
                            p.subagent_thinking(&key, &text);
                            if lane.thoughts.len() < MAX_LANE_THINKING {
                                lane.thoughts.push(text);
                            }
                        }
                        8 if lane.tools < MAX_LANE_TOOL_ROWS => {
                            let row = format!("S{k}r{n}");
                            p.subagent_tool_started(&key, &row, call("x", "Bash").command(&row));
                            lane.tools += 1;
                            lane.running.push(row.clone());
                            lane.rows.push(row);
                        }
                        9 if !lane.running.is_empty() => {
                            let i = rng.below(lane.running.len() as u64) as usize;
                            let row = lane.running.remove(i);
                            p.subagent_tool_finished(&key, &row, 1, Some("r"));
                            lane.done_rows.push(row);
                        }
                        10 => {
                            p.subagent_finished(&key);
                            lane.finished = true;
                        }
                        _ => {}
                    }
                }
                11 if rng.below(4) == 0 => {
                    t += 1;
                    all.extend(cot.steer_boundary("", &t.to_string()));
                }
                12 if rng.below(3) == 0 => cot = round_trip(&cot),
                _ => {}
            }
            t += 1;
            all.extend(cot.project(&segs, Some(&p), &t.to_string()));
        }
        all.extend(cot.finish("done", "99999"));

        let tl = timeline(&all);
        let mut result_count = std::collections::HashMap::new();
        for id in tl
            .blocks
            .iter()
            .flat_map(|b| &b.results)
            .chain(&tl.loose_results)
        {
            *result_count.entry(id.as_str()).or_insert(0usize) += 1;
        }
        let mut owner_of_call = std::collections::HashMap::new();
        for block in &tl.blocks {
            for (id, name) in block.tool_ids.iter().zip(&block.tools) {
                let previous = owner_of_call.insert(id.clone(), fuzz_owner(name).unwrap());
                assert!(previous.is_none(), "seed {seed}: {id} 上了两次 wire");
            }
        }
        let mut owners = Vec::new();
        for (index, block) in tl.blocks.iter().enumerate() {
            let mut found: std::collections::BTreeSet<String> =
                block.text.split("\n\n").filter_map(fuzz_owner).collect();
            found.extend(block.tools.iter().filter_map(|name| fuzz_owner(name)));
            assert!(
                found.len() <= 1,
                "seed {seed}: 块 {index} 混了 {found:?}:{block:?}"
            );
            // 子 agent 的块总有块首行,没有内容的块只能是主 agent 的。
            let owner = found.into_iter().next();
            let block_owner = owner.clone().unwrap_or_else(|| "main".into());
            for result in &block.results {
                assert_eq!(
                    owner_of_call.get(result),
                    Some(&block_owner),
                    "seed {seed}: {result} 落进了块 {index}"
                );
            }
            let Some(owner) = owner else { continue };
            if owner != "main" {
                let header = block.text.split("\n\n").next().unwrap_or_default();
                let title = format!("L{}", &owner["lane".len()..]);
                let continued = owners.contains(&owner);
                assert_eq!(
                    header,
                    if continued {
                        format!("{title}{}", crate::i18n::label(LANE_CONTINUED))
                    } else {
                        title
                    },
                    "seed {seed}: 块 {index}"
                );
                if continued {
                    let last = owners.iter().rposition(|o| *o == owner).unwrap();
                    assert!(
                        owners[last + 1..].iter().all(|o| o == "main"),
                        "seed {seed}: {owner} 被别的子 agent 隔开:{owners:?}"
                    );
                }
            }
            owners.push(owner);
        }
        for (k, lane) in lanes.iter().enumerate() {
            let owner = format!("lane{k}");
            let blocks = || {
                tl.blocks.iter().filter(|b| {
                    b.text.split("\n\n").next().and_then(fuzz_owner) == Some(owner.clone())
                })
            };
            let prefix = format!("S{k}t");
            let thoughts: Vec<&str> = blocks()
                .flat_map(|b| b.text.split("\n\n"))
                .filter(|par| par.starts_with(&prefix))
                .collect();
            assert_eq!(thoughts, lane.thoughts, "seed {seed}: lane {k} 的思考");
            let rows: Vec<&String> = blocks().flat_map(|b| &b.tools).collect();
            assert_eq!(
                rows,
                lane.rows.iter().collect::<Vec<_>>(),
                "seed {seed}: lane {k} 的工具"
            );
            for row in &lane.rows {
                let id = blocks()
                    .flat_map(|b| b.tools.iter().zip(&b.tool_ids))
                    .find(|(name, _)| *name == row)
                    .map(|(_, id)| id.clone())
                    .unwrap();
                let pushed = result_count.get(id.as_str()).copied().unwrap_or(0);
                let expected = usize::from(lane.done_rows.contains(row));
                assert_eq!(pushed, expected, "seed {seed}: {row} 的结果");
            }
        }
    }

    /// 随机序列:任何子 agent 的块不和别人混、不被别的子 agent 的块隔开(被主 agent 隔开时以
    /// 「（续）」接上),内容按序不丢,工具编号不重复,结果只落在自己的块或没有块开着的时刻。
    #[test]
    fn random_sequences_keep_every_lane_contiguous() {
        for seed in 1..1000u64 {
            run_lane_fuzz(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        }
    }
}
