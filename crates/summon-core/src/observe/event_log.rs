//! 无头全量事件流的**磁盘记录**(纯逻辑,无 IO):把内部 `AgentMessage` 投影成稳定的
//! 行式 JSON 记录 `EventRecord`，并由 `TailGrouper` 归并为可读显示块。
//!
//! 为何独立一个磁盘 schema、不直接 `serde(AgentMessage)`:磁盘格式要长期稳定,不该被内部
//! 结构(含 `SystemTime`/`RunId` 这类运行期字段)绑死;`from_event` 只挑磁盘需要的字段,
//! 由 IO 源(`summon::event_log::EventSink`)在落盘时盖上 `ts`/`seq`。

use serde::{Deserialize, Serialize};

use crate::parse::agent_msg::{AgentKind, AgentMessage};

/// `tool_input`/`tool_result` 序列化后超过此字节数就截断(防一次大文件写把单行撑爆)。
const TOOL_VALUE_CAP: usize = 2048;
/// 截断标记:替换掉超长 value,保留可读提示而非整段 JSON。
const TRUNC_MARKER: &str = "…[truncated]";

/// 用量精简记录(磁盘版,字段名短而稳定)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRec {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// 一条磁盘事件记录(events.jsonl 一行一个)。`ts`/`seq` 由 IO 源在 tee 时盖,
/// 其余字段从 `AgentMessage` 投影;`Option` 字段为 None 时不序列化,保持行紧凑。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// unix 毫秒,落盘时盖(非解析器的 EPOCH)。
    pub ts: u64,
    /// 单文件内单调自增,ts 撞车时定序。
    pub seq: u64,
    /// 事件类型:AgentKind 的小写名(system/assistant/thinking/tool_use/tool_result/result/error)。
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageRec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// 流式内容块是否为增量；Option 保留「旧日志没有该字段」与 false 的区别。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<bool>,
    /// 上游事件阶段（例如 codex_rpc_delta / codex_rpc_completed）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_source: Option<String>,
    /// provider 内容 item id，用于关联同一段的 delta 与完成快照。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_item_id: Option<String>,
    /// 写下这条记录的 agent(alias)。路径已按 agent 分目录,但记录离开目录(拷贝、汇总、
    /// 跨 agent 检索)后仍要能认出是谁做的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// 这一轮驱动 agent 的模型。身份不随模型换,记录里要能看出每一轮是哪个模型在跑。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 本轮 turn 凭证(`summon <cmd> --turn-id` 用的那枚),把记录与该轮的 CLI 动作对上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
}

/// 一轮里所有记录共用的出处:谁、用哪个模型、哪一轮。由 IO 源在开 sink 时定下,逐条盖上。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventStamp {
    pub agent: Option<String>,
    pub model: Option<String>,
    pub turn: Option<String>,
}

/// AgentKind → 稳定的小写磁盘 kind 名。
fn kind_name(kind: &AgentKind) -> &'static str {
    match kind {
        AgentKind::System => "system",
        AgentKind::Assistant => "assistant",
        AgentKind::Thinking => "thinking",
        AgentKind::ToolUse => "tool_use",
        AgentKind::ToolResult => "tool_result",
        AgentKind::Result => "result",
        AgentKind::Error => "error",
    }
}

/// 把一个工具 value 做容量上限处理:序列化超 `TOOL_VALUE_CAP` 字节就替换成带截断标记的字符串,
/// 让单行有界。复制语义(返回新 Value),不改原事件。
fn cap_value(v: &serde_json::Value) -> serde_json::Value {
    match serde_json::to_string(v) {
        Ok(s) if s.len() > TOOL_VALUE_CAP => {
            // 按字符边界截到上限附近,避免切坏 UTF-8;再缀截断标记。
            let mut cut = TOOL_VALUE_CAP;
            while cut > 0 && !s.is_char_boundary(cut) {
                cut -= 1;
            }
            serde_json::Value::String(format!("{}{TRUNC_MARKER}", &s[..cut]))
        }
        // 在容量内 / 序列化失败:原样保留(失败极罕见,保留原 value 不致命)。
        _ => v.clone(),
    }
}

impl EventRecord {
    /// 把内部事件投影成磁盘记录;`ts_ms`/`seq` 由调用方(IO 源)在 tee 时盖。纯函数。
    pub fn from_event(ev: &AgentMessage, ts_ms: u64, seq: u64) -> Self {
        Self {
            ts: ts_ms,
            seq,
            kind: kind_name(&ev.kind).to_string(),
            text: ev.text.clone(),
            tool: ev.tool.clone(),
            tool_id: ev.tool_id.clone(),
            tool_input: ev.tool_input.as_ref().map(cap_value),
            tool_result: ev.tool_result.as_ref().map(cap_value),
            session_id: ev.session_id.clone(),
            usage: ev.usage.as_ref().map(|u| UsageRec {
                input: u.input_tokens,
                output: u.output_tokens,
                cache_read: u.cache_read,
                cache_write: u.cache_write,
            }),
            stop_reason: ev.stop_reason.clone(),
            partial: matches!(ev.kind, AgentKind::Assistant | AgentKind::Thinking)
                .then_some(ev.partial),
            stream_source: ev.stream_source.map(|source| source.as_str().to_string()),
            source_item_id: ev.source_item_id.clone(),
            agent: None,
            model: None,
            turn: None,
        }
    }

    /// 盖上本轮出处。三项整体替换:同一条记录不该混着两轮的出处。
    pub fn with_stamp(mut self, stamp: &EventStamp) -> Self {
        self.agent = stamp.agent.clone();
        self.model = stamp.model.clone();
        self.turn = stamp.turn.clone();
        self
    }

    /// 入站用户消息记录(kind=`user_input`)。events.jsonl 原本只 tee agent 输出,加这条让
    /// `agents tail` 也能看到「是哪条用户消息触发了本轮」;ts/seq 由 IO 源盖。
    pub fn user_input(text: &str, ts_ms: u64, seq: u64) -> Self {
        Self::note("user_input", text, ts_ms, seq)
    }

    /// 会话建立时 provider 下发的候选模型(kind=`model_catalog`)。只供 `agents tail` 排查
    /// 「面板 / 开跑前检查认的是哪份目录」,不是 agent 输出,读轮的消费方一律跳过它。
    pub fn model_catalog(text: &str, ts_ms: u64, seq: u64) -> Self {
        Self::note("model_catalog", text, ts_ms, seq)
    }

    fn note(kind: &str, text: &str, ts_ms: u64, seq: u64) -> Self {
        Self {
            ts: ts_ms,
            seq,
            kind: kind.to_string(),
            text: Some(text.to_string()),
            tool: None,
            tool_id: None,
            tool_input: None,
            tool_result: None,
            session_id: None,
            usage: None,
            stop_reason: None,
            partial: None,
            stream_source: None,
            source_item_id: None,
            agent: None,
            model: None,
            turn: None,
        }
    }
}

// ── 人读渲染(tail 用):一条记录 → 一行 `HH:MM:SS  kind  <内容>` ────────────────────

/// 极简 ANSI:color=false 时返回裸文本(无依赖、可单测)。
fn paint(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// unix 毫秒 → **本地时区**的 `HH:MM:SS`。
///
/// why 用 Local 而不是原来的「纯算术取 UTC 日内秒」:同一条 `summon agents tail` 的表头走
/// `chrono::Local`(cli.rs 的「该会话最后活跃 …」),事件行却是 UTC —— UTC+8 下一份输出里
/// 差 8 小时,且与 `daemon.log`(也是 Local)完全对不上,排障时没法把两边对齐。
/// 原注释说「日内时分秒足够定位」,但那只在 UTC 机器上成立。
/// 解析失败(理论上不可能:u64 毫秒恒在 chrono 范围内)回退到原来的纯算术,绝不 panic。
fn hms(ts_ms: u64) -> String {
    use chrono::TimeZone;
    if let chrono::LocalResult::Single(dt) = chrono::Local.timestamp_millis_opt(ts_ms as i64) {
        return dt.format("%H:%M:%S").to_string();
    }
    let secs = ts_ms / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// 把一行内容压成单行并截到 `max` 列(超出缀 `…`)。换行/回车折成空格,便于 tail 一行一事件。
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

/// tool_input 的紧凑一行展示:序列化成 JSON 再压平截断。
fn compact_json(v: &serde_json::Value, max: usize) -> String {
    let s = serde_json::to_string(v).unwrap_or_else(|_| "<?>".to_string());
    one_line(&s, max)
}

/// 单记录渲染夹具：主体行与 usage 行共同验证格式。
#[cfg(test)]
fn render_line(rec: &EventRecord, color: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(body) = render_body(rec, color) {
        lines.push(body);
    }
    if let Some(usage) = render_usage(rec, color) {
        lines.push(usage);
    }
    lines.join("\n")
}

/// 记录的主体行(不含 usage)。成块文本(thinking/assistant/result)的流式归并由
/// [`TailGrouper`] 负责,它复用本函数渲染工具/用户/错误等段边界行。
fn render_body(rec: &EventRecord, color: bool) -> Option<String> {
    let time = paint(&hms(rec.ts), "90", color); // 灰
    let body = match rec.kind.as_str() {
        "user_input" => {
            let t = one_line(rec.text.as_deref().unwrap_or(""), 200);
            Some(format!("{:<10} ← {}", "user", paint(&t, "36", color))) // 青色,入站箭头
        }
        "thinking" => {
            let t = one_line(rec.text.as_deref().unwrap_or(""), 200);
            Some(format!("{:<10} {}", "thinking", paint(&t, "2;37", color))) // dim
        }
        // 目录要整份看得到才有排查价值,不按 200 字截断。
        "model_catalog" => {
            let t = rec.text.as_deref().unwrap_or("").trim();
            Some(format!("{:<10} {}", "models", paint(t, "33", color))) // 黄
        }
        "tool_use" => {
            let name = rec.tool.as_deref().unwrap_or("?");
            let input = rec
                .tool_input
                .as_ref()
                .map(|v| compact_json(v, 120))
                .unwrap_or_default();
            Some(format!(
                "{:<10} {} {}",
                "tool",
                paint(name, "36", color),
                input
            )) // 青
        }
        "tool_result" => {
            let r = rec
                .tool_result
                .as_ref()
                .map(|v| compact_json(v, 120))
                .unwrap_or_default();
            Some(format!("{:<10} {} {}", "tool↩", "↩", one_line(&r, 120)))
        }
        "assistant" | "result" => {
            // result 可能只带 usage 不带 text(codex):无 text 就不出 reply 行,只留 usage。
            let t = rec.text.as_deref().unwrap_or("");
            (!t.trim().is_empty())
                .then(|| format!("{:<10} {}", "reply", paint(&one_line(t, 400), "1", color)))
        }
        _ => {
            // system/error 等:展示 text(若有),否则只标 kind。
            let t = one_line(rec.text.as_deref().unwrap_or(""), 200);
            Some(format!("{:<10} {}", rec.kind, t))
        }
    };
    body.map(|b| format!("{time}  {b}"))
}

/// usage 摘要行(任何带 usage 的记录都可能追加;通常是 result)。
fn render_usage(rec: &EventRecord, color: bool) -> Option<String> {
    let u = rec.usage.as_ref()?;
    let time = paint(&hms(rec.ts), "90", color); // 灰
    let usage = format!(
        "in {} / out {} / cache_read {}",
        u.input, u.output, u.cache_read
    );
    Some(format!(
        "{time}  {:<10} {}",
        "usage",
        paint(&usage, "90", color)
    )) // 灰
}

// ── tail 流式归并:同一轮逐 token 落盘的 thinking/reply 碎片合成一个可原地刷新的块 ──────

/// 能成块的文本种类:思考与回复。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TailBlockKind {
    Thinking,
    Reply,
}

/// 一条已归并的人读文本块;`text` 是该段截至目前的全文,保留正文里的真实换行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailBlock {
    pub ts: u64,
    pub kind: TailBlockKind,
    pub text: String,
}

/// 归并器逐条喂入 [`EventRecord`] 后吐出的显示事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailOut {
    /// 活动块建立或增长:同一物理块会反复来。tty 应原地重画;非 tty 忽略,等 [`TailOut::Frozen`]
    /// 再成行。
    Live(TailBlock),
    /// 活动块定稿:tty 换行把它留在滚动区;非 tty 此刻把最后一版 `Live` 打成一条。
    Frozen,
    /// 带 item 的活动块被工具记录(`tool_use`/`tool_result`)穿插时先让出屏幕:tty 清掉当前
    /// 预览行(工具行随后正常打印);非 tty 忽略。同 item 续写时再发 `Live` 重绘,completed
    /// 到达后仍是一个定稿块 —— codex/trae 一条消息的 token 流常被后台工具结果拦腰打断。
    LiveClear,
    /// 不成块的独立行(工具/用户入站/错误/usage 等),已是渲染好的字符串(可能多行)。
    Line(String),
}

/// 正文起始列:`HH:MM:SS`(8)+ 两空格 + label(10)+ 一空格。
const BLOCK_INDENT: usize = 8 + 2 + 10 + 1;

impl TailBlock {
    /// 渲染成可打印字符串(不带尾换行):正文真实换行保留,续行缩进到正文起始列。
    pub fn render(&self, color: bool) -> String {
        let time = paint(&hms(self.ts), "90", color); // 灰
        let (label, code) = match self.kind {
            TailBlockKind::Thinking => ("thinking", "2;37"), // dim
            TailBlockKind::Reply => ("reply", "1"),          // bold
        };
        let indent = " ".repeat(BLOCK_INDENT);
        let mut body = String::new();
        for (i, seg) in normalize_newlines(&self.text).split('\n').enumerate() {
            if i > 0 {
                body.push('\n');
                // 段内空行(原文的空行)就留空,不补一串缩进空格。
                if !seg.is_empty() {
                    body.push_str(&indent);
                }
            }
            body.push_str(seg);
        }
        format!("{time}  {label:<10} {}", paint(&body, code, color))
    }
}

/// `\r\n` 与孤立 `\r` 统一成 `\n`,让正文里的回车都按换行显示。
fn normalize_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// 流式进行中的单行预览:正文压成一行(真实换行折空格),按终端列宽截断,供 tty 原地刷新;
/// 定稿时再由 [`TailBlock::render`] 输出保留换行的全文。`max_cols` 为终端列数(0 按 80 兜底)。
pub fn render_live_preview(block: &TailBlock, color: bool, max_cols: usize) -> String {
    let time = paint(&hms(block.ts), "90", color); // 灰
    let (label, code) = match block.kind {
        TailBlockKind::Thinking => ("thinking", "2;37"), // dim
        TailBlockKind::Reply => ("reply", "1"),          // bold
    };
    let cols = if max_cols == 0 { 80 } else { max_cols };
    // 留一列防止终端在行尾自动换行；窄屏同时截断固定前缀。
    let available = cols.saturating_sub(1);
    if available <= BLOCK_INDENT {
        let prefix = format!("{}  {label:<10} ", hms(block.ts));
        let short = prefix
            .chars()
            .take(available.saturating_sub(1))
            .collect::<String>();
        return paint(
            &format!("{short}{}", if available > 0 { "…" } else { "" }),
            "90",
            color,
        );
    }
    let budget = available.saturating_sub(BLOCK_INDENT + 1);
    let flat = normalize_newlines(block.text.trim())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut used = 0usize;
    let mut body = String::new();
    let mut overflow = false;
    for ch in flat.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            overflow = true;
            break;
        }
        used += w;
        body.push(ch);
    }
    if overflow {
        body.push('…');
    }
    format!("{time}  {label:<10} {}", paint(&body, code, color))
}

/// 归并器内部的活动段。
#[derive(Debug)]
struct ActiveRun {
    kind: TailBlockKind,
    /// provider item id;codex 的 delta 与 completed 快照共享,cursor ACP/CLI 无此物(None)。
    item: Option<String>,
    text: String,
    ts: u64,
    source_items: crate::segment::SourceItemSegments,
}

impl ActiveRun {
    fn core_kind(kind: TailBlockKind) -> crate::segment::SegKind {
        match kind {
            TailBlockKind::Thinking => crate::segment::SegKind::Thinking,
            TailBlockKind::Reply => crate::segment::SegKind::Reply,
        }
    }

    fn segment(kind: TailBlockKind, text: String) -> crate::segment::TurnSegment {
        match kind {
            TailBlockKind::Thinking => crate::segment::TurnSegment::Thinking(text),
            TailBlockKind::Reply => crate::segment::TurnSegment::Reply(text),
        }
    }

    fn new(kind: TailBlockKind, item: Option<&str>, text: &str, ts: u64) -> Self {
        let mut source_items = crate::segment::SourceItemSegments::default();
        let mut segments = Vec::new();
        crate::segment::push_segment_with_source_item(
            &mut segments,
            &mut source_items,
            Self::core_kind(kind),
            text,
            true,
            Some(crate::segment::SourceItemUpdate::Delta(
                item.unwrap_or("tail-active-run"),
            )),
        );
        Self {
            kind,
            item: item.map(str::to_string),
            text: text.to_string(),
            ts,
            source_items,
        }
    }

    fn update(&mut self, text: &str, completed: bool, heuristic_snapshot: bool) {
        if text.trim().is_empty() {
            if completed {
                self.text = text.to_string();
            } else {
                self.text.push_str(text);
            }
            return;
        }
        if self.text.trim().is_empty() {
            let combined = if completed {
                text.to_string()
            } else {
                format!("{}{text}", self.text)
            };
            *self = Self::new(self.kind, self.item.as_deref(), &combined, self.ts);
            return;
        }
        let mut segments = vec![Self::segment(self.kind, std::mem::take(&mut self.text))];
        if heuristic_snapshot {
            crate::segment::push_segment(&mut segments, Self::core_kind(self.kind), text, true);
        } else {
            let item = self.item.as_deref().unwrap_or("tail-active-run");
            let update = if completed {
                crate::segment::SourceItemUpdate::Completed(item)
            } else {
                crate::segment::SourceItemUpdate::Delta(item)
            };
            crate::segment::push_segment_with_source_item(
                &mut segments,
                &mut self.source_items,
                Self::core_kind(self.kind),
                text,
                !completed,
                Some(update),
            );
        }
        self.text = match segments
            .into_iter()
            .next()
            .expect(summon_i18n::text!("r6cbb973d64"))
        {
            crate::segment::TurnSegment::Thinking(text)
            | crate::segment::TurnSegment::Reply(text) => text,
        };
    }

    fn merge_unmarked(&mut self, text: &str) {
        let snapshot = self.text.chars().count() >= 8
            && (self.text.starts_with(text) || text.starts_with(&self.text));
        self.update(text, false, snapshot);
    }
}

/// 一轮内已显示内容的记账,轮边界(user_input)清空。
///
/// 解决三类「同段重影」:
/// - codex/trae 一条消息的 token 流会被其它 item / 后台工具结果**打断**:同 item 先定稿一片、
///   增量稍后才续上。`emitted` 按 item 累计已显示各片的拼接,迟到的 completed 全文据此只补
///   未显示的「续片」,而不是把全文再打一遍。
/// - completed 全文可能重发:`completed` 登记权威全文,同文重发压掉。
/// - 旧采集版 cursor 的 `result` 文本是本轮各 reply 段的**无分隔聚合**:`reply_concat` 是已显示
///   reply 的同口径拼接,聚合回执整体压掉。
#[derive(Debug, Clone, Default)]
struct TurnIndex {
    /// (块种类, item) → 已进滚动区的各片文本拼接。
    emitted: std::collections::HashMap<(TailBlockKind, String), String>,
    /// (块种类, item) → completed 权威全文。
    completed: std::collections::HashMap<(TailBlockKind, String), String>,
    /// 稳定 item 在正文到达前被穿插的空白前缀。
    pending_whitespace: std::collections::HashMap<(TailBlockKind, String), String>,
    /// 无 item 通道(cursor)最近一个定稿块,用于紧随其后的同文/前缀快照去重。
    last_no_item: Option<(TailBlockKind, String)>,
    /// 无 item 通道本轮已显示 reply 各块的无分隔拼接。
    reply_concat: String,
    no_item_replies: Vec<String>,
    segments: Vec<crate::segment::TurnSegment>,
}

impl TurnIndex {
    /// 记下一个已经显示给用户的块。
    fn record(&mut self, kind: TailBlockKind, item: Option<&str>, text: &str) {
        self.segments
            .push(ActiveRun::segment(kind, text.to_string()));
        match item {
            Some(id) => {
                self.emitted
                    .entry((kind, id.to_string()))
                    .or_default()
                    .push_str(text);
            }
            None => {
                self.last_no_item = Some((kind, text.to_string()));
                if kind == TailBlockKind::Reply {
                    self.reply_concat.push_str(text);
                    self.no_item_replies.push(text.to_string());
                }
            }
        }
    }

    fn replays_reply(&self, text: &str, active: Option<&ActiveRun>) -> bool {
        let mut segments = self.segments.clone();
        if let Some(active) = active {
            segments.push(ActiveRun::segment(active.kind, active.text.clone()));
        } else {
            segments.push(crate::segment::TurnSegment::Thinking(String::new()));
        }
        let squash =
            |text: &str| -> String { text.chars().filter(|c| !c.is_whitespace()).collect() };
        let snapshot = squash(text);
        let replies: Vec<&str> = segments
            .iter()
            .filter_map(|segment| match segment {
                crate::segment::TurnSegment::Reply(text) => Some(text.as_str()),
                crate::segment::TurnSegment::Thinking(_) => None,
            })
            .collect();
        let prior_count = if matches!(segments.last(), Some(crate::segment::TurnSegment::Reply(_)))
        {
            replies.len().saturating_sub(1)
        } else {
            replies.len()
        };
        !snapshot.is_empty()
            && (replies[..prior_count].iter().any(|earlier| {
                let earlier = squash(earlier);
                earlier == snapshot
                    || (snapshot.chars().count() >= 8 && earlier.starts_with(&snapshot))
            }) || (replies.len() > 1 && squash(&replies.concat()) == snapshot))
    }

    /// 无 item 全文快照的已显示前缀，按同轮各回复段的顺序对账。
    fn reply_snapshot_suffix<'a>(&self, full: &'a str) -> Option<&'a str> {
        let mut consumed = 0;
        for start in 0..self.no_item_replies.len() {
            let first = &self.no_item_replies[start];
            if first.chars().count() < 8 || !full.starts_with(first) {
                continue;
            }
            let mut remaining = full;
            for shown in &self.no_item_replies[start..] {
                if let Some(rest) = remaining.strip_prefix(shown) {
                    remaining = rest;
                }
            }
            consumed = consumed.max(full.len() - remaining.len());
        }
        (consumed > 0).then(|| &full[consumed..])
    }

    fn covers_reply_aggregate(&self, text: &str, active: Option<&ActiveRun>) -> bool {
        let squash =
            |text: &str| -> String { text.chars().filter(|c| !c.is_whitespace()).collect() };
        let Some(first) = self.segments.iter().find_map(|segment| match segment {
            crate::segment::TurnSegment::Reply(text) => Some(squash(text)),
            _ => None,
        }) else {
            return false;
        };
        let snapshot = squash(text);
        if first.chars().count() < 8 || !snapshot.starts_with(&first) {
            return false;
        }
        let mut shown = self
            .segments
            .iter()
            .filter_map(|segment| match segment {
                crate::segment::TurnSegment::Reply(text) => Some(squash(text)),
                _ => None,
            })
            .collect::<String>();
        if let Some(active) = active.filter(|run| run.kind == TailBlockKind::Reply) {
            shown.push_str(&squash(&active.text));
        }
        let mut shown = shown.chars();
        snapshot
            .chars()
            .all(|ch| shown.by_ref().any(|seen| seen == ch))
    }

    /// 无 item 通道的候选文本是否与最近定稿块重复(同文或更短前缀重发)。
    ///
    /// 前缀判定要求候选本身足够长(≥ 8 字符):短碎片可能恰好与上一条长回复同首字符
    /// (新消息以 `"S"` 开头、上一条恰以 `S` 开头),短串做前缀压制会丢整条新消息。
    fn last_no_item_redundant(&self, kind: TailBlockKind, text: &str) -> bool {
        const RESEND_PREFIX_MIN_CHARS: usize = 8;
        self.last_no_item
            .as_ref()
            .filter(|(k, _)| *k == kind)
            .is_some_and(|(_, shown)| {
                text == shown
                    || (text.chars().count() >= RESEND_PREFIX_MIN_CHARS && shown.starts_with(text))
            })
    }
}

/// `agents tail` 的逐记录归并器。
///
/// 盘上同一段 thinking/reply 会以多条记录到达:cursor ACP 逐词落盘(不带 partial 标记)、
/// cursor/codex 流式逐 token(`partial=true`)再跟一条 completed 全文快照。逐条成行会把
/// 一句话刷成几十行;本归并器把它们合成一个块 —— 碎片增量拼接、权威快照替换/去重。
///
/// 工具穿插不必然是段边界:codex/trae 一条消息的 token 流会被后台 `tool_use`/`tool_result`
/// 拦腰打断,delta 与 completed 快照共享同一个 item id。带 item 的活动段遇到工具记录时只让出
/// 屏幕(`LiveClear`),不冻结;同 item 的后续 delta 续上后仍是同一块,completed 全文一次定稿。
/// 换 item、换块种类、用户入站、`result` 等才是真边界,立即定稿。无 item 通道(cursor)无法
/// 区分「续写」与「下一条」,工具记录一律按边界定稿。
///
/// Cursor 无标记 thinking/reply 按碎片处理；Codex 的历史 assistant 碎片同样归并，
/// 历史无来源 thinking 保持独立段。稳定 item 的增量与完成快照复用核心段模型，
/// 无 item 的长前缀快照复用核心文本规则；原始空白保留在显示记账中。
pub struct TailGrouper {
    color: bool,
    merge_unmarked: bool,
    merge_unmarked_reply: bool,
    run: Option<ActiveRun>,
    /// 活动段是否因工具记录穿插而处于让出屏幕状态(已发 LiveClear、等待同 item 续写)。
    hidden: bool,
    idx: TurnIndex,
}

impl TailGrouper {
    pub fn new(merge_unmarked: bool, color: bool) -> Self {
        Self {
            color,
            merge_unmarked,
            merge_unmarked_reply: merge_unmarked,
            run: None,
            hidden: false,
            idx: TurnIndex::default(),
        }
    }

    pub fn for_driver(driver: crate::driver::CliDriver, color: bool) -> Self {
        let mut grouper = Self::new(driver == crate::driver::CliDriver::Cursor, color);
        grouper.merge_unmarked_reply |= driver == crate::driver::CliDriver::Codex;
        grouper
    }

    /// 喂入一条记录,返回本次产生的显示事件(通常 0~2 个:块事件之外可能再跟一条 usage 行)。
    pub fn push(&mut self, rec: &EventRecord) -> Vec<TailOut> {
        let mut out = Vec::with_capacity(2);
        match classify(rec) {
            Some(t) => self.push_text(t, &mut out),
            None => {
                // 带 item 的活动段被工具记录穿插:不定稿,只清一次预览行让出屏幕;同 item 续写
                // 后由 completed 合成一块。其它记录(用户入站/system/error…)仍是硬边界。
                let tool_pass_through = rec.kind == "tool_use" || rec.kind == "tool_result";
                let parkable =
                    tool_pass_through && self.run.as_ref().is_some_and(|r| r.item.is_some());
                if parkable {
                    if !self.hidden {
                        out.push(TailOut::LiveClear);
                        self.hidden = true;
                    }
                } else {
                    self.freeze(&mut out);
                }
                // 新一轮入站消息 = 轮边界:去重记账只对同轮有效,不能把下一轮恰好同文的短回复
                // (如连续两个「收到」)压掉。
                if rec.kind == "user_input" {
                    self.idx = TurnIndex::default();
                }
                if let Some(body) = render_body(rec, self.color) {
                    out.push(TailOut::Line(body));
                }
            }
        }
        // usage 对任何记录都可能带(通常是 result):文本归并之后独立成行,不被吞掉。
        if let Some(usage) = render_usage(rec, self.color) {
            out.push(TailOut::Line(usage));
        }
        out
    }

    /// 回放历史线程追到文件尾且文件已久无更新时,把未闭合的碎片段冲成一个定稿块(供 CLI 标注
    /// 「未定稿」)。CLI 在文件尾持续检查陈旧程度，后续新段可再次冲刷。
    pub fn flush_pending(&mut self) -> Option<TailBlock> {
        let run = self.run.take()?;
        self.hidden = false;
        if run.text.trim().is_empty() {
            self.run = Some(run);
            return None;
        }
        let text = run.text;
        self.idx.record(run.kind, run.item.as_deref(), &text);
        Some(TailBlock {
            ts: run.ts,
            kind: run.kind,
            text,
        })
    }

    fn push_text(&mut self, t: TextRec<'_>, out: &mut Vec<TailOut>) {
        let TextRec {
            kind,
            partial,
            item,
            text,
            stream_source,
            terminal,
            ts,
        } = t;

        if kind == TailBlockKind::Reply
            && item.is_none()
            && (self.merge_unmarked || self.merge_unmarked_reply)
            && partial != Some(true)
            && (terminal || text.chars().count() >= 8)
            && (self.idx.replays_reply(text, self.run.as_ref())
                || (terminal && self.idx.covers_reply_aggregate(text, self.run.as_ref())))
        {
            // 重放快照也能结束同一回复的短重发前缀，避免残留 token 在下一边界单独定稿。
            if self.run.as_ref().is_some_and(|run| {
                run.kind == kind
                    && run.item.is_none()
                    && !run.text.trim().is_empty()
                    && text.starts_with(run.text.trim())
                    && self.idx.replays_reply(text, None)
            }) {
                self.run = None;
                self.hidden = false;
                out.push(TailOut::LiveClear);
            } else if terminal {
                self.freeze(out);
            }
            return;
        }
        if kind == TailBlockKind::Reply
            && item.is_none()
            && (self.merge_unmarked || self.merge_unmarked_reply)
            && partial != Some(true)
            && text.chars().count() >= 8
        {
            if let Some(suffix) = self.idx.reply_snapshot_suffix(text) {
                let suffix = suffix.to_string();
                let same_run = self
                    .run
                    .as_ref()
                    .is_some_and(|run| run.kind == kind && run.item.is_none());
                if same_run {
                    let run = self.run.as_mut().expect("checked");
                    if terminal || partial == Some(false) {
                        run.text = suffix;
                        self.freeze(out);
                    } else {
                        if !run.text.starts_with(&suffix) {
                            if suffix.starts_with(&run.text) {
                                run.text = suffix;
                            } else {
                                run.merge_unmarked(&suffix);
                            }
                        }
                        out.push(self.live());
                    }
                } else {
                    self.freeze(out);
                    if terminal || partial == Some(false) {
                        self.emit_block(kind, None, &suffix, ts, out);
                    } else {
                        self.start_run(kind, None, &suffix, ts);
                        out.push(self.live());
                    }
                }
                return;
            }
        }
        if terminal {
            if text.trim().is_empty() {
                self.freeze(out);
            } else {
                self.handle_result(kind, item, text, ts, out);
            }
            return;
        }
        let merge_unmarked =
            self.merge_unmarked || (kind == TailBlockKind::Reply && self.merge_unmarked_reply);

        // 空白增量也属于正文；先建立段，等待正文或权威快照使它可显示。
        if text.trim().is_empty() {
            if Self::is_fragment(partial, item, stream_source, merge_unmarked) {
                if self
                    .run
                    .as_ref()
                    .is_some_and(|run| run.kind == kind && run.item.as_deref() == item)
                {
                    self.run
                        .as_mut()
                        .expect("checked")
                        .update(text, false, false);
                    out.push(self.live());
                } else {
                    self.freeze(out);
                    self.start_run(kind, item, text, ts);
                }
            }
            return;
        }

        let Some(run) = self.run.as_mut() else {
            if terminal {
                self.handle_result(kind, item, text, ts, out);
            } else if Self::is_fragment(partial, item, stream_source, merge_unmarked) {
                self.start_run(kind, item, text, ts);
                out.push(self.live());
            } else if partial == Some(false) {
                self.finish_completed(kind, item, text, ts, out);
            } else {
                self.emit_standalone(kind, item, text, ts, out);
            }
            return;
        };

        if run.kind != kind {
            // thinking 段与 reply 段互为边界:定稿旧段,按无活动段重新处理本条。
            self.freeze(out);
            self.push_text(t, out);
            return;
        }

        let same_item = run.item.as_deref() == item;
        match partial {
            Some(true) => {
                if !Self::is_fragment(partial, item, stream_source, merge_unmarked) {
                    self.freeze(out);
                    self.emit_standalone(kind, item, text, ts, out);
                } else if same_item {
                    run.update(text, false, false);
                    out.push(self.live());
                } else {
                    // 别的 item 插进来:定稿当前片(其续片/完成全文稍后按 item 对账),起新 item 段。
                    self.freeze(out);
                    if Self::is_fragment(partial, item, stream_source, merge_unmarked) {
                        self.start_run(kind, item, text, ts);
                        out.push(self.live());
                    } else {
                        self.emit_standalone(kind, item, text, ts, out);
                    }
                }
            }
            Some(false) => {
                if same_item {
                    self.finish_active_completed(text, out);
                } else {
                    self.freeze(out);
                    self.finish_completed(kind, item, text, ts, out);
                }
            }
            None if terminal => {
                self.handle_result(kind, item, text, ts, out);
            }
            None => {
                // 无标记记录:cursor 通道按增长快照/碎片续接;独立段通道先定稿再各成一块。
                if same_item && merge_unmarked {
                    run.merge_unmarked(text);
                    out.push(self.live());
                } else {
                    self.freeze(out);
                    if merge_unmarked {
                        self.start_run(kind, item, text, ts);
                        out.push(self.live());
                    } else {
                        self.emit_standalone(kind, item, text, ts, out);
                    }
                }
            }
        }
    }

    fn start_run(&mut self, kind: TailBlockKind, item: Option<&str>, text: &str, ts: u64) {
        let prefix = item
            .and_then(|id| self.idx.pending_whitespace.remove(&(kind, id.to_string())))
            .unwrap_or_default();
        self.run = Some(ActiveRun::new(kind, item, &format!("{prefix}{text}"), ts));
    }

    /// 该记录是否按「同段增量」处理(续接/起活动段)。
    fn is_fragment(
        partial: Option<bool>,
        item: Option<&str>,
        stream_source: Option<&str>,
        merge_unmarked: bool,
    ) -> bool {
        match partial {
            // 稳定 item、stream 来源或该内容类别的历史碎片模式提供增量语义。
            Some(true) => merge_unmarked || item.is_some() || stream_source.is_some(),
            // 历史碎片模式按当前 backend 与内容类别确定。
            None => merge_unmarked,
            Some(false) => false,
        }
    }

    /// completed 权威全文到达且活动段属于该 item:用全文给当前片段定稿,只显示此前未显示的部分。
    fn finish_active_completed(&mut self, full: &str, out: &mut Vec<TailOut>) {
        let run_kind;
        let run_item;
        {
            let run = self.run.as_mut().expect(summon_i18n::text!("rb3df530785"));
            run_kind = run.kind;
            run_item = run.item.clone();
            let prior = match &run_item {
                Some(id) => self
                    .idx
                    .emitted
                    .get(&(run_kind, id.clone()))
                    .cloned()
                    .unwrap_or_default(),
                None => String::new(),
            };
            let expected = format!("{prior}{}", run.text);
            if full == expected.trim_end() || full == run.text.trim() {
                // 增量拼接正好等于全文(无 item 时 prior 恒空):当前片原样定稿。
            } else if let Some(rest) = full.strip_prefix(&prior) {
                // 此前已先定稿过一片:当前片只显示「续片」,各片拼起来恰为全文。
                run.update(rest, true, false);
            } else {
                // 对不上账(极罕见的协议修正):全文整块定稿。
                run.update(full, true, false);
            }
        }
        if let Some(id) = &run_item {
            self.idx
                .completed
                .insert((run_kind, id.clone()), full.to_string());
        }
        self.freeze(out);
        if let Some(id) = &run_item {
            // fallback 分支可能让 emitted 多出旧前缀,以权威全文为准修正记账。
            if self
                .idx
                .emitted
                .get(&(run_kind, id.clone()))
                .is_some_and(|e| e != full)
            {
                self.idx
                    .emitted
                    .insert((run_kind, id.clone()), full.to_string());
            }
        }
    }

    /// completed 全文到达但活动段不属于它(已先定稿,或从未出现增量):按 item 对账,
    /// 只补未显示的续片;同文重发压掉。
    fn finish_completed(
        &mut self,
        kind: TailBlockKind,
        item: Option<&str>,
        full: &str,
        ts: u64,
        out: &mut Vec<TailOut>,
    ) {
        let Some(id) = item else {
            // 无 item(cursor)的完成快照:走独立块(含最近定稿块同文/前缀去重)。
            self.emit_standalone(kind, None, full, ts, out);
            return;
        };
        let key = (kind, id.to_string());
        self.idx.pending_whitespace.remove(&key);
        if self.idx.completed.get(&key).is_some_and(|f| f == full) {
            return; // 权威全文重发
        }
        let emitted = self.idx.emitted.get(&key).cloned().unwrap_or_default();
        if full == emitted || emitted.starts_with(full) {
            return; // 已整片显示(或显示过更长的版本)
        }
        let block_text = match full.strip_prefix(&emitted) {
            Some(rest) if rest.trim().is_empty() => return,
            Some(rest) => rest,
            None => full,
        };
        self.emit_block(kind, Some(id), block_text, ts, out);
        self.idx.completed.insert(key, full.to_string());
    }

    /// `result` 终态文本。result 绝不是「续接 token」:它要么是当前段的同文/增长快照,要么是本轮
    /// 各段的无分隔聚合(旧 cursor),要么是权威最终回复 —— 任何情况下都不与活动段做互不包含拼接。
    fn handle_result(
        &mut self,
        kind: TailBlockKind,
        item: Option<&str>,
        text: &str,
        ts: u64,
        out: &mut Vec<TailOut>,
    ) {
        let full = text;
        let run_is_same = self
            .run
            .as_ref()
            .is_some_and(|r| r.kind == kind && r.item.as_deref() == item);
        if run_is_same {
            if item.is_none() {
                let active = self.run.as_ref().map(|r| r.text.trim()).unwrap_or("");
                let mut agg = self.idx.reply_concat.clone();
                agg.push_str(active);
                if full == agg.trim() || full == active {
                    // 聚合回执 = 已显示各段 + 活动段:正常定稿当前片,回执压掉。
                    self.freeze(out);
                    return;
                }
                if full.chars().count() >= 8 && active.starts_with(full) {
                    // 更短的回执重发(足够长才判定,避免短回执恰好是活动段前缀时误压):定稿当前片。
                    self.freeze(out);
                    return;
                }
                // 增长快照或权威全文:用更长/权威文本定稿当前段,不做拼接。
                self.run
                    .as_mut()
                    .expect("checked")
                    .update(full, true, false);
                self.freeze(out);
            } else {
                self.finish_active_completed(full, out);
            }
        } else {
            self.freeze(out);
            // 无 item 通道且回执内容已被本轮已显示 reply 覆盖(旧 cursor 聚合)→ 整体压掉,
            // usage 由 push 另行输出。
            if item.is_none() && self.idx.reply_concat.contains(full) {
                return;
            }
            self.finish_completed(kind, item, full, ts, out);
        }
    }

    fn emit_standalone(
        &mut self,
        kind: TailBlockKind,
        item: Option<&str>,
        text: &str,
        ts: u64,
        out: &mut Vec<TailOut>,
    ) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(id) = item {
            // item 通道:权威全文重发 / 已整片显示 → 压掉。
            let key = (kind, id.to_string());
            self.idx.pending_whitespace.remove(&key);
            if self.idx.completed.get(&key).is_some_and(|f| f == text) {
                return;
            }
            if let Some(emitted) = self.idx.emitted.get(&key) {
                if emitted == text || emitted.starts_with(text) {
                    return;
                }
            }
        } else if self.merge_unmarked && self.idx.last_no_item_redundant(kind, text) {
            // 无 item 同文/前缀去重是 cursor 专属启发式;独立段落通道(claude 等)不过这道闸,
            // 否则会把跨工具边界的同文独立思考/回复误删。
            return;
        }
        self.emit_block(kind, item, text, ts, out);
    }

    fn emit_block(
        &mut self,
        kind: TailBlockKind,
        item: Option<&str>,
        text: &str,
        ts: u64,
        out: &mut Vec<TailOut>,
    ) {
        self.idx.record(kind, item, text);
        out.push(TailOut::Live(TailBlock {
            ts,
            kind,
            text: text.to_string(),
        }));
        out.push(TailOut::Frozen);
    }

    fn freeze(&mut self, out: &mut Vec<TailOut>) {
        if let Some(run) = self.run.take() {
            self.hidden = false;
            let text = run.text;
            if text.trim().is_empty() {
                if let Some(item) = run.item {
                    self.idx
                        .pending_whitespace
                        .entry((run.kind, item))
                        .or_default()
                        .push_str(&text);
                }
                return;
            }
            self.idx.record(run.kind, run.item.as_deref(), &text);
            // 定稿前补一版最终文本:completed 快照可能刚在定稿时替换/修正了累积文本。
            out.push(TailOut::Live(TailBlock {
                ts: run.ts,
                kind: run.kind,
                text,
            }));
            out.push(TailOut::Frozen);
        }
    }

    fn live(&mut self) -> TailOut {
        let run = self.run.as_ref().expect(summon_i18n::text!("rb7a3622149"));
        // 让出屏幕后同 item 续写:预览行重新上线。
        self.hidden = false;
        TailOut::Live(TailBlock {
            ts: run.ts,
            kind: run.kind,
            text: run.text.clone(),
        })
    }
}

/// 从记录里提取出的可成块文本。
struct TextRec<'a> {
    kind: TailBlockKind,
    partial: Option<bool>,
    item: Option<&'a str>,
    text: &'a str,
    stream_source: Option<&'a str>,
    /// result 记录的 text 是终态回执,不开启碎片段(它后面不会再有同段增量)。
    terminal: bool,
    ts: u64,
}

/// 该记录是否为同 item 的权威完成快照(全文替换、定稿)。
///
/// codex RPC 的 thinking 完成快照在解析层被标成 `partial=true`(与 delta 同值),只靠
/// `stream_source` 区分 —— `*_completed` 即快照;assistant 快照本身 partial=false,此处同样认。
fn is_completed_snapshot(rec: &EventRecord) -> bool {
    rec.stream_source
        .as_deref()
        .is_some_and(|s| s.ends_with("_completed"))
}

/// 取记录里能成块的文本;**完全无 text 字段**才返回 None(落到段边界渲染)。
/// 纯空白增量(codex 有单个空格 token)不能在归并前丢弃 —— 它是活动段的一部分,
/// 丢了会把一句话从空格处劈成两条。
fn classify(rec: &EventRecord) -> Option<TextRec<'_>> {
    // 完成快照优先于 partial 标记(codex thinking 快照的 partial 也是 true)。
    let partial = if is_completed_snapshot(rec) {
        Some(false)
    } else {
        rec.partial
    };
    match rec.kind.as_str() {
        "thinking" => {
            let t = rec.text.as_deref()?;
            Some(TextRec {
                kind: TailBlockKind::Thinking,
                partial,
                item: rec.source_item_id.as_deref(),
                text: t,
                stream_source: rec.stream_source.as_deref(),
                terminal: false,
                ts: rec.ts,
            })
        }
        "assistant" | "result" => Some(TextRec {
            kind: TailBlockKind::Reply,
            partial,
            item: rec.source_item_id.as_deref(),
            text: rec.text.as_deref().unwrap_or(""),
            stream_source: rec.stream_source.as_deref(),
            terminal: rec.kind == "result",
            ts: rec.ts,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod rebuild_tests {
    use super::*;
    use crate::segment::TurnSegment;

    fn rec(kind: &str, text: &str, partial: bool, seq: u64) -> EventRecord {
        EventRecord {
            ts: 1_000 + seq,
            seq,
            kind: kind.to_string(),
            text: Some(text.to_string()),
            tool: None,
            tool_id: None,
            tool_input: None,
            tool_result: None,
            session_id: None,
            usage: None,
            stop_reason: None,
            partial: Some(partial),
            stream_source: None,
            source_item_id: None,
            agent: None,
            model: None,
            turn: None,
        }
    }

    /// **只重建最后一轮**:`user_input` 是轮边界。把上一轮的回复也渲进中断卡,
    /// 等于把陈年内容当成"这一轮已生成的",比空白更误导。
    #[test]
    fn 只取最后一个user_input之后的内容() {
        let records = vec![
            rec("assistant", "上一轮的回答", false, 1),
            rec("user_input", "张三:新问题", false, 2),
            rec("thinking", "在想", false, 3),
            rec("assistant", "这一轮写了一半", false, 4),
        ];
        let segs = rebuild_last_turn_segments(&records);
        let joined = format!("{segs:?}");
        assert!(joined.contains("这一轮写了一半"), "{joined}");
        assert!(joined.contains("在想"), "思考也要留:{joined}");
        assert!(
            !joined.contains("上一轮的回答"),
            "不能把上一轮渲进来:{joined}"
        );
    }

    /// 流式 delta(`partial=true`)必须拼成一段,不能每个 token 一段。
    #[test]
    fn 流式增量拼成一段() {
        let records = vec![
            rec("user_input", "问", false, 1),
            rec("assistant", "我已经", true, 2),
            rec("assistant", "写了一半", true, 3),
        ];
        assert_eq!(
            rebuild_last_turn_segments(&records),
            vec![TurnSegment::Reply("我已经写了一半".into())]
        );
    }

    /// 工具事件不进卡片正文(与实时渲染同口径);全是工具时给空,调用方退化成纯说明卡。
    #[test]
    fn 工具事件不进正文() {
        let records = vec![
            rec("user_input", "问", false, 1),
            rec("tool_use", "Bash(ls)", false, 2),
            rec("tool_result", "a.txt", false, 3),
        ];
        assert!(rebuild_last_turn_segments(&records).is_empty());
    }

    /// 没有 `user_input`(文件被轮转截断)时退回全量 —— 宁可多渲一点历史,
    /// 也好过把「有内容」渲成空白。
    #[test]
    fn 没有轮边界时退回全量而不是给空() {
        let records = vec![rec("assistant", "残存正文", false, 1)];
        assert_eq!(
            rebuild_last_turn_segments(&records),
            vec![TurnSegment::Reply("残存正文".into())]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::agent_msg::{AgentKind, AgentMessage, ContentStreamSource, Usage};

    #[test]
    fn stamp_lands_agent_model_turn_on_the_record() {
        let stamp = EventStamp {
            agent: Some("arcadia".into()),
            model: Some("opus".into()),
            turn: Some("t-1".into()),
        };
        let rec = EventRecord::user_input("张三:hi", 1, 1).with_stamp(&stamp);
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["agent"], "arcadia");
        assert_eq!(v["model"], "opus");
        assert_eq!(v["turn"], "t-1");
    }

    #[test]
    fn unstamped_record_serializes_without_provenance_keys() {
        let rec = EventRecord::from_event(&AgentMessage::of(AgentKind::System), 1, 1)
            .with_stamp(&EventStamp::default());
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        for key in ["agent", "model", "turn"] {
            assert!(v.get(key).is_none(), "没有出处就不写 {key}:{v}");
        }
    }

    /// 出处字段出现之前落盘的行照样读得回,三项为 None。
    #[test]
    fn lines_written_before_provenance_still_parse() {
        let rec: EventRecord =
            serde_json::from_str(r#"{"ts":1,"seq":1,"kind":"user_input","text":"x"}"#).unwrap();
        assert_eq!((rec.agent, rec.model, rec.turn), (None, None, None));
    }

    /// **真实 events.jsonl(脱敏)解析 + 渲染回归**:这些行是从 owner 机
    /// `~/.summon/agents/*/threads/*/events.jsonl` 取的真实 turn 事件形态(session_id/正文已脱敏成
    /// 占位、结构原样)。锁住真实磁盘行能反序列化成 `EventRecord` 且 `render_line` 给出人读输出 ——
    /// 防 `agents tail` 的渲染对真实数据形态回归(合成测里 tool_id 等真实字段易缺)。
    #[test]
    fn real_events_jsonl_parse_and_render() {
        // 四种真实 kind:tool_use(带 tool_id)/ tool_result / result(带正文)/ assistant。
        let lines = [
            r#"{"ts":1781063814480,"seq":2,"kind":"tool_use","tool":"read","tool_id":"toolu_x","session_id":"sid"}"#,
            r#"{"ts":1780658415393,"seq":4,"kind":"tool_result","tool_result":"Launching skill: lark-calendar","session_id":"sid"}"#,
            r#"{"ts":1780645212355,"seq":704,"kind":"result","text":"我会先按本地约定加载记忆,再看 diff。","session_id":"sid"}"#,
            r#"{"ts":1780644461838,"seq":2,"kind":"assistant","text":"我","session_id":"sid"}"#,
        ];
        let recs: Vec<EventRecord> = lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("真实 events.jsonl 行应能反序列化"))
            .collect();
        // 字段投影正确(真实磁盘形态)。
        assert_eq!(recs[0].kind, "tool_use");
        assert_eq!(recs[0].tool.as_deref(), Some("read"));
        assert_eq!(recs[0].tool_id.as_deref(), Some("toolu_x"));
        // render_line(无色)对每种 kind 给出含关键内容的人读行。
        assert!(
            render_line(&recs[0], false).contains("read"),
            "tool_use 渲染含工具名"
        );
        assert!(
            render_line(&recs[1], false).contains("lark-calendar"),
            "tool_result 渲染含结果"
        );
        let r = render_line(&recs[2], false);
        assert!(
            r.contains("reply") && r.contains("加载记忆"),
            "result 渲染成 reply 行带正文"
        );
        assert!(
            render_line(&recs[3], false).contains("我"),
            "assistant 渲染含正文"
        );
    }

    #[test]
    fn from_event_projects_tool_use_fields() {
        let mut ev = AgentMessage::of(AgentKind::ToolUse);
        ev.tool = Some("Read".to_string());
        ev.tool_id = Some("toolu_1".to_string());
        ev.tool_input = Some(serde_json::json!({"file_path": "/etc/foo"}));
        let rec = EventRecord::from_event(&ev, 1234, 7);
        assert_eq!(rec.ts, 1234);
        assert_eq!(rec.seq, 7);
        assert_eq!(rec.kind, "tool_use");
        assert_eq!(rec.tool.as_deref(), Some("Read"));
        assert_eq!(rec.tool_id.as_deref(), Some("toolu_1"));
        assert_eq!(
            rec.tool_input.as_ref().unwrap()["file_path"],
            serde_json::json!("/etc/foo")
        );
    }

    #[test]
    fn from_event_persists_stream_provenance_and_legacy_lines_still_decode() {
        let mut ev = AgentMessage::of(AgentKind::Assistant);
        ev.text = Some("正文不影响元数据断言".to_string());
        ev.partial = false;
        ev.stream_source = Some(ContentStreamSource::CodexRpcCompleted);
        ev.source_item_id = Some("item_42".to_string());
        let rec = EventRecord::from_event(&ev, 1234, 7);
        assert_eq!(rec.partial, Some(false));
        assert_eq!(rec.stream_source.as_deref(), Some("codex_rpc_completed"));
        assert_eq!(rec.source_item_id.as_deref(), Some("item_42"));

        let legacy: EventRecord =
            serde_json::from_str(r#"{"ts":1,"seq":1,"kind":"assistant","text":"旧日志"}"#).unwrap();
        assert_eq!(legacy.partial, None);
        assert_eq!(legacy.stream_source, None);
        assert_eq!(legacy.source_item_id, None);
    }

    #[test]
    fn from_event_caps_huge_tool_input() {
        let mut ev = AgentMessage::of(AgentKind::ToolUse);
        ev.tool = Some("Write".to_string());
        let huge = "x".repeat(10_000);
        ev.tool_input = Some(serde_json::json!({"content": huge}));
        let rec = EventRecord::from_event(&ev, 0, 0);
        // 截断后变成一个带标记的字符串 value,长度远小于原始 10k。
        let serialized = serde_json::to_string(rec.tool_input.as_ref().unwrap()).unwrap();
        assert!(serialized.len() < TOOL_VALUE_CAP + 64, "应截断到上限附近");
        assert!(
            serialized.contains("truncated"),
            "应含截断标记, 实得长度 {}",
            serialized.len()
        );
    }

    #[test]
    fn from_event_projects_usage_and_session() {
        let mut ev = AgentMessage::of(AgentKind::Result);
        ev.text = Some("done".to_string());
        ev.session_id = Some("sid-9".to_string());
        ev.usage = Some(Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read: 8000,
            cache_write: 5,
        });
        let rec = EventRecord::from_event(&ev, 9, 1);
        assert_eq!(rec.session_id.as_deref(), Some("sid-9"));
        let u = rec.usage.unwrap();
        assert_eq!(
            (u.input, u.output, u.cache_read, u.cache_write),
            (100, 20, 8000, 5)
        );
    }

    #[test]
    fn model_catalog_record_renders_every_candidate_untruncated() {
        let models: Vec<String> = (0..60).map(|i| format!("model-{i:02}")).collect();
        let text = models.join(", ");
        let rec = EventRecord::model_catalog(&text, 1000, 1);
        assert_eq!(rec.kind, "model_catalog");
        let line = render_line(&rec, false);
        assert!(line.contains("models"), "应有 models 标签");
        assert!(
            line.contains("model-00") && line.contains("model-59"),
            "候选不能被截断"
        );
        let mut grouper = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
        let out = grouper.push(&rec);
        assert!(
            out.iter()
                .any(|o| matches!(o, TailOut::Line(l) if l.contains("model-59"))),
            "tail 流式归并也要原样输出这一行"
        );
    }

    #[test]
    fn user_input_record_renders_inbound_line() {
        let rec = EventRecord::user_input("张三:你好", 1000, 1);
        assert_eq!(rec.kind, "user_input");
        assert_eq!(rec.text.as_deref(), Some("张三:你好"));
        // tail 渲染:入站箭头 + user 标签 + 消息原文(无 ANSI 便于断言)。
        let line = render_line(&rec, false);
        assert!(line.contains("← "), "应有入站箭头");
        assert!(line.contains("user"), "应有 user 标签");
        assert!(line.contains("张三:你好"), "应含消息原文");
    }

    #[test]
    fn record_round_trips_through_json() {
        let mut ev = AgentMessage::of(AgentKind::Thinking);
        ev.text = Some("let me think".to_string());
        let rec = EventRecord::from_event(&ev, 42, 3);
        let line = serde_json::to_string(&rec).unwrap();
        let back: EventRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(rec, back);
        // None 字段不应出现在序列化里(行紧凑)
        assert!(!line.contains("\"tool\""));
        assert!(!line.contains("\"usage\""));
    }

    #[test]
    fn render_thinking_is_dim_and_labeled() {
        let mut ev = AgentMessage::of(AgentKind::Thinking);
        ev.text = Some("I need to read config first".to_string());
        let rec = EventRecord::from_event(&ev, 0, 0);
        let plain = render_line(&rec, false);
        assert!(plain.contains("thinking"));
        assert!(plain.contains("read config"));
        assert!(!plain.contains('\x1b'), "color=false 不应含 ANSI");
        // 带色版含 ANSI 转义
        assert!(render_line(&rec, true).contains('\x1b'));
    }

    #[test]
    fn render_tool_use_shows_name_and_input() {
        let mut ev = AgentMessage::of(AgentKind::ToolUse);
        ev.tool = Some("Read".to_string());
        ev.tool_input = Some(serde_json::json!({"file_path": "/etc/foo"}));
        let rec = EventRecord::from_event(&ev, 0, 0);
        let plain = render_line(&rec, false);
        assert!(plain.contains("tool"));
        assert!(plain.contains("Read"));
        assert!(plain.contains("/etc/foo"));
    }

    #[test]
    fn render_tool_result_marks_return() {
        let mut ev = AgentMessage::of(AgentKind::ToolResult);
        ev.tool_result = Some(serde_json::json!("42 lines"));
        let rec = EventRecord::from_event(&ev, 0, 0);
        let plain = render_line(&rec, false);
        assert!(plain.contains('↩'));
        assert!(plain.contains("42 lines"));
    }

    #[test]
    fn render_reply_highlighted() {
        let mut ev = AgentMessage::of(AgentKind::Result);
        ev.text = Some("已经修好了".to_string());
        let rec = EventRecord::from_event(&ev, 0, 0);
        assert!(render_line(&rec, false).contains("已经修好了"));
        assert!(render_line(&rec, false).contains("reply"));
    }

    #[test]
    fn render_usage_summary() {
        let mut ev = AgentMessage::of(AgentKind::Result);
        ev.usage = Some(Usage {
            input_tokens: 1200,
            output_tokens: 340,
            cache_read: 8100,
            cache_write: 0,
        });
        let rec = EventRecord::from_event(&ev, 0, 0);
        let plain = render_line(&rec, false);
        assert!(plain.contains("usage"));
        assert!(plain.contains("in 1200"));
        assert!(plain.contains("out 340"));
        assert!(plain.contains("cache_read 8100"));
    }

    /// tail 的事件行时间必须与表头(`chrono::Local`)和 daemon.log 同时区,否则一份输出里
    /// 混两个时区,UTC+8 下差 8 小时。这条钉住「按本地时区渲染」而不是取 UTC 日内秒。
    #[test]
    fn hms_renders_in_local_timezone_not_utc() {
        use chrono::TimeZone;
        let ts_ms = 1_785_237_662_062u64;
        let expect = chrono::Local
            .timestamp_millis_opt(ts_ms as i64)
            .single()
            .expect("毫秒时间戳必然可解析")
            .format("%H:%M:%S")
            .to_string();
        assert_eq!(hms(ts_ms), expect);
        // 只有当本机恰好是 UTC 时两者才该相等;非 UTC 机器上必须不同(证明真做了时区转换)。
        let utc_naive = {
            let secs = ts_ms / 1000;
            format!(
                "{:02}:{:02}:{:02}",
                (secs / 3600) % 24,
                (secs / 60) % 60,
                secs % 60
            )
        };
        let offset = chrono::Local
            .timestamp_millis_opt(ts_ms as i64)
            .single()
            .unwrap()
            .offset()
            .local_minus_utc();
        if offset % 86_400 != 0 {
            assert_ne!(hms(ts_ms), utc_naive, "非 UTC 机器上不该再输出 UTC 日内秒");
        }
    }
}

/// 从落盘事件流里重建**最后一轮**的正文段序列(纯函数,给孤儿卡收敛用)。
///
/// **why 从 events.jsonl 重建、而不是把正文另存一份**(owner 2026-08-10 定的 B 方案):
/// daemon 被杀时,那张流式卡上已经渲出来的正文只存在于飞书服务端,而**飞书不提供读回卡片原文的
/// 能力** —— `im/v1/messages/:id` 对 interactive 返回的是「请升级客户端」这种降级占位,没有
/// `body.elements`。另一条路是每次流式 patch 都把正文快照落盘,但打字机每秒好几次、长回复几十 KB,
/// 为一个极低频的中断场景给高频热路径加 IO 不划算。而 events.jsonl 本来就在全量 tee,
/// 重建它是零新增写入。
///
/// **轮边界靠 `user_input`**:每轮开始时写一条,所以从末尾往回找到最后一条 `user_input`,
/// 它之后的就是这一轮。找不到(极早期文件/被轮转截断)则退回全量——宁可多渲一点历史,
/// 也好过把「有内容」渲成空白。
///
/// 只取 `assistant` / `thinking` 两类;工具事件不进卡片正文(与实时渲染同口径)。
pub fn rebuild_last_turn_segments(records: &[EventRecord]) -> Vec<crate::segment::TurnSegment> {
    let start = records
        .iter()
        .rposition(|r| r.kind == "user_input")
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut segs: Vec<crate::segment::TurnSegment> = Vec::new();
    for rec in &records[start..] {
        let kind = match rec.kind.as_str() {
            "assistant" => crate::segment::SegKind::Reply,
            "thinking" => crate::segment::SegKind::Thinking,
            _ => continue,
        };
        let Some(text) = rec.text.as_deref().filter(|t| !t.trim().is_empty()) else {
            continue;
        };
        // `partial` 缺省(老记录)按 false 处理:那是"离散块"语义,与 push_segment 对
        // 非增量的处置一致 —— 不会把两块不相干的正文粘成一段。
        crate::segment::push_segment(&mut segs, kind, text, rec.partial.unwrap_or(false));
    }
    segs
}

#[cfg(test)]
mod tail_grouper_tests {
    use super::*;

    /// 构造一条文本记录。`partial`:None = 字段缺省(cursor ACP / 老日志),Some 显式标记。
    fn rec(
        seq: u64,
        kind: &str,
        text: &str,
        partial: Option<bool>,
        item: Option<&str>,
    ) -> EventRecord {
        EventRecord {
            ts: 1_000 * (60 + seq),
            seq,
            kind: kind.to_string(),
            text: Some(text.to_string()),
            tool: None,
            tool_id: None,
            tool_input: None,
            tool_result: None,
            session_id: None,
            usage: None,
            stop_reason: None,
            partial,
            stream_source: None,
            source_item_id: item.map(str::to_string),
            agent: None,
            model: None,
            turn: None,
        }
    }

    fn result(seq: u64, text: &str, usage: bool) -> EventRecord {
        let mut r = rec(seq, "result", text, None, None);
        if usage {
            r.usage = Some(UsageRec {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 0,
            });
        }
        r
    }

    fn tool_use(seq: u64) -> EventRecord {
        let mut r = rec(seq, "tool_use", "", None, None);
        r.text = None;
        r.tool = Some("read".to_string());
        r
    }

    fn tool_result(seq: u64) -> EventRecord {
        let mut r = rec(seq, "tool_result", "", None, None);
        r.text = None;
        r.tool_id = Some("t1".to_string());
        r.tool_result = Some(serde_json::json!("ok"));
        r
    }

    fn user(seq: u64) -> EventRecord {
        rec(seq, "user_input", "办个事", None, None)
    }

    fn live_texts(out: &[TailOut]) -> Vec<String> {
        out.iter()
            .filter_map(|e| match e {
                TailOut::Live(b) => Some(b.text.clone()),
                _ => None,
            })
            .collect()
    }

    fn kinds(out: &[TailOut]) -> Vec<&'static str> {
        out.iter()
            .map(|e| match e {
                TailOut::Live(_) => "live",
                TailOut::Frozen => "frozen",
                TailOut::LiveClear => "clear",
                TailOut::Line(_) => "line",
            })
            .collect()
    }

    fn frozen_texts(events: &[TailOut]) -> Vec<String> {
        let mut current = None;
        let mut texts = Vec::new();
        for event in events {
            match event {
                TailOut::Live(block) => current = Some(block.text.clone()),
                TailOut::Frozen => texts.push(current.take().expect("定稿前须有正文")),
                _ => {}
            }
        }
        texts
    }

    #[test]
    fn legacy_codex_reply_fragments_form_one_block_and_summaries_remain_independent() {
        let mut grouper = TailGrouper::for_driver(crate::driver::CliDriver::Codex, false);
        let mut events = Vec::new();
        for (index, text) in ["Inspecting the source", "Checking the build"]
            .into_iter()
            .enumerate()
        {
            events.extend(grouper.push(&rec(index as u64, "thinking", text, Some(true), None)));
        }
        for (index, text) in [
            "Cont",
            "ract read. Window",
            " is 2026-10-01",
            " → 2026-10-",
            "12. Now d",
            "ispatching.",
        ]
        .into_iter()
        .enumerate()
        {
            events.extend(grouper.push(&rec(index as u64 + 2, "assistant", text, None, None)));
        }
        let mut terminal = result(8, "", true);
        terminal.text = None;
        events.extend(grouper.push(&terminal));
        assert_eq!(
            frozen_texts(&events),
            [
                "Inspecting the source",
                "Checking the build",
                "Contract read. Window is 2026-10-01 → 2026-10-12. Now dispatching."
            ]
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, TailOut::Line(line) if line.contains("usage"))));
    }

    #[test]
    fn late_cursor_snapshots_do_not_repeat_earlier_reply_or_turn_aggregate() {
        let first = "Contract read. Window is 2026-10-01 → 2026-10-12.";
        let second = "Now dispatching a fresh-context reviewer.";
        let mut grouper = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", first, None, None),
            rec(1, "thinking", "Preparing the task", None, None),
            rec(2, "assistant", "Now dispatching a ", None, None),
            rec(3, "assistant", "fresh-context reviewer.", None, None),
            rec(4, "assistant", first, Some(false), None),
            rec(
                5,
                "assistant",
                &format!("{first}\n{second}"),
                Some(false),
                None,
            ),
            result(6, "", false),
        ] {
            events.extend(grouper.push(&record));
        }
        assert_eq!(frozen_texts(&events), [first, "Preparing the task", second]);
    }

    #[test]
    fn interleaved_item_completion_preserves_spaces_at_piece_boundaries() {
        let mut grouper = TailGrouper::for_driver(crate::driver::CliDriver::Codex, false);
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", "alpha ", Some(true), Some("a")),
            rec(1, "assistant", "other", Some(true), Some("b")),
            rec(2, "assistant", " beta\n", Some(true), Some("a")),
            rec(3, "assistant", "alpha  beta\n", Some(false), Some("a")),
            result(4, "", false),
        ] {
            events.extend(grouper.push(&record));
        }
        let texts = frozen_texts(&events);
        assert_eq!(texts, ["alpha ", "other", " beta\n"]);
        assert_eq!(format!("{}{}", texts[0], texts[2]), "alpha  beta\n");
    }

    #[test]
    fn unmarked_short_repeated_tokens_are_lossless() {
        let mut grouper = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
        let mut events = Vec::new();
        for (index, text) in ["S", "S", "H", " ", "ok"].into_iter().enumerate() {
            events.extend(grouper.push(&rec(index as u64, "assistant", text, None, None)));
        }
        events.extend(grouper.push(&result(5, "", false)));
        assert_eq!(frozen_texts(&events), ["SSH ok"]);
    }

    #[test]
    fn completed_item_without_delta_is_emitted_once_across_another_item() {
        let mut grouper = TailGrouper::for_driver(crate::driver::CliDriver::Codex, false);
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", "alpha", Some(true), Some("a")),
            rec(1, "assistant", "bravo", Some(false), Some("b")),
            rec(2, "assistant", "bravo", Some(false), Some("b")),
            result(3, "", false),
        ] {
            events.extend(grouper.push(&record));
        }
        assert_eq!(frozen_texts(&events), ["alpha", "bravo"]);
    }

    #[test]
    fn leading_whitespace_delta_is_preserved_until_terminal_boundary() {
        for driver in [
            crate::driver::CliDriver::Codex,
            crate::driver::CliDriver::Cursor,
        ] {
            let mut grouper = TailGrouper::for_driver(driver, false);
            let mut events = Vec::new();
            for record in [
                rec(0, "assistant", "\n  ", Some(true), Some("a")),
                rec(1, "assistant", "line", Some(true), Some("a")),
                result(2, "", false),
            ] {
                events.extend(grouper.push(&record));
            }
            assert_eq!(frozen_texts(&events), ["\n  line"]);
        }
    }

    #[test]
    fn leading_whitespace_survives_item_interleaving_and_stale_eof() {
        let mut grouper = TailGrouper::for_driver(crate::driver::CliDriver::Codex, false);
        let mut events = grouper.push(&rec(0, "assistant", "\n  ", Some(true), Some("a")));
        assert!(grouper.flush_pending().is_none());
        for record in [
            rec(1, "assistant", "other", Some(true), Some("b")),
            rec(2, "assistant", "line", Some(true), Some("a")),
            result(3, "", false),
        ] {
            events.extend(grouper.push(&record));
        }
        assert_eq!(frozen_texts(&events), ["other", "\n  line"]);
    }

    #[test]
    fn completed_item_snapshot_replaces_buffered_whitespace_prefix() {
        let mut grouper = TailGrouper::for_driver(crate::driver::CliDriver::Codex, false);
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", "\n  ", Some(true), Some("a")),
            rec(1, "assistant", "other", Some(true), Some("b")),
            rec(2, "assistant", "final", Some(false), Some("a")),
            rec(3, "assistant", "final", Some(false), Some("a")),
            result(4, "", false),
        ] {
            events.extend(grouper.push(&record));
        }
        assert_eq!(frozen_texts(&events), ["other", "final"]);
    }

    #[test]
    fn live_preview_fits_narrow_terminal_columns() {
        let block = TailBlock {
            ts: 0,
            kind: TailBlockKind::Reply,
            text: "英文中文abcdefghijk".into(),
        };
        for columns in 1..=40 {
            let preview = render_live_preview(&block, false, columns);
            assert!(
                unicode_width::UnicodeWidthStr::width(preview.as_str()) < columns,
                "columns={columns} preview={preview:?}"
            );
        }
    }

    #[test]
    fn completed_suffix_without_active_item_keeps_prefix_and_whitespace() {
        let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Codex, false);
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", "alpha ", Some(true), Some("a")),
            rec(1, "assistant", "other", Some(false), Some("b")),
            rec(2, "assistant", "alpha  beta\n", Some(false), Some("a")),
            rec(3, "assistant", "alpha  beta\n", Some(false), Some("a")),
        ] {
            events.extend(g.push(&record));
        }
        assert_eq!(frozen_texts(&events), ["alpha ", "other", " beta\n"]);
    }

    #[test]
    fn resend_prefix_threshold_keeps_short_reply_and_suppresses_long_prefix() {
        let mut index = TurnIndex::default();
        index.record(TailBlockKind::Reply, None, "12345678 complete");
        assert!(!index.last_no_item_redundant(TailBlockKind::Reply, "1234567"));
        assert!(index.last_no_item_redundant(TailBlockKind::Reply, "12345678"));
        let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", "12345678 complete", Some(false), None),
            tool_use(1),
            rec(2, "assistant", "1234567", Some(false), None),
            rec(3, "assistant", "12345678", Some(false), None),
        ] {
            events.extend(g.push(&record));
        }
        assert_eq!(frozen_texts(&events), ["12345678 complete", "1234567"]);
    }

    #[test]
    fn pending_flush_returns_exact_text_once_and_remembers_item() {
        let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Codex, false);
        g.push(&rec(0, "assistant", "  partial\n", Some(true), Some("a")));
        g.push(&tool_use(1));
        let block = g.flush_pending().expect("unfinished block");
        assert_eq!(block.text, "  partial\n");
        assert_eq!(block.kind, TailBlockKind::Reply);
        assert_eq!(block.ts, 60_000);
        assert!(g.flush_pending().is_none());
        assert!(g
            .push(&rec(2, "assistant", "  partial\n", Some(false), Some("a")))
            .is_empty());
        g.push(&rec(3, "assistant", "second", Some(true), Some("b")));
        assert_eq!(g.flush_pending().unwrap().text, "second");
    }

    #[test]
    fn cursor_late_prefix_snapshot_reconciles_across_multiple_boundaries() {
        let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
        let first = "12345678 complete reply";
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", first, None, None),
            tool_use(1),
            rec(2, "thinking", "thinking fragment", None, None),
            rec(3, "system", "status", None, None),
            rec(4, "assistant", "12345678 complete", None, None),
            tool_use(5),
            rec(6, "assistant", first, None, None),
            result(7, first, true),
        ] {
            events.extend(g.push(&record));
        }
        assert_eq!(frozen_texts(&events), [first, "thinking fragment"]);
        assert!(events
            .iter()
            .any(|e| matches!(e, TailOut::Line(s) if s.contains("usage"))));
    }
    #[test]
    #[ignore = "requires local events.jsonl replay inputs"]
    fn real_event_logs_satisfy_tail_replay_invariants() {
        fn squash(text: &str) -> String {
            text.chars().filter(|c| !c.is_whitespace()).collect()
        }
        fn is_subsequence(needle: &str, haystack: &str) -> bool {
            let mut source = haystack.chars();
            needle
                .chars()
                .all(|ch| source.by_ref().any(|seen| seen == ch))
        }
        let inputs = std::env::var("SUMMON_TAIL_REPLAY_INPUTS")
            .expect("label|driver|path|tiny ceiling, one input per line");
        for input in inputs.lines() {
            let fields: Vec<_> = input.split('|').collect();
            assert_eq!(fields.len(), 4);
            let driver = crate::driver::CliDriver::from_kind(fields[1]);
            let source = std::fs::read(fields[2]).unwrap();
            let mut g = TailGrouper::for_driver(driver, false);
            type ReplayTurn = (Vec<TailBlock>, Vec<(TailBlockKind, String)>);
            let mut turns: Vec<ReplayTurn> = vec![Default::default()];
            let mut pending = None;
            let mut records = 0;
            for line in source
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                let record: EventRecord = serde_json::from_slice(line).unwrap();
                records += 1;
                let events = g.push(&record);
                for event in events {
                    match event {
                        TailOut::Live(block) => pending = Some(block),
                        TailOut::Frozen => {
                            turns.last_mut().unwrap().0.push(pending.take().unwrap())
                        }
                        _ => {}
                    }
                }
                if record.kind == "user_input" {
                    turns.push(Default::default());
                }
                if let Some(text) = classify(&record) {
                    if (text.partial == Some(false) || text.terminal)
                        && !text.text.trim().is_empty()
                    {
                        turns
                            .last_mut()
                            .unwrap()
                            .1
                            .push((text.kind, squash(text.text)));
                    }
                }
            }
            if let Some(block) = g.flush_pending() {
                turns.last_mut().unwrap().0.push(block);
            }
            let mut tiny = 0;
            let mut blocks = 0;
            let mut prefix_pairs = 0;
            let mut uncovered = 0;
            for (shown, snapshots) in &turns {
                blocks += shown.len();
                tiny += shown
                    .iter()
                    .filter(|b| {
                        b.kind == TailBlockKind::Reply && squash(&b.text).chars().count() <= 3
                    })
                    .count();
                for kind in [TailBlockKind::Reply, TailBlockKind::Thinking] {
                    let texts: Vec<_> = shown
                        .iter()
                        .filter(|b| b.kind == kind)
                        .map(|b| squash(&b.text))
                        .collect();
                    let joined = texts.concat();
                    for (_, snapshot) in snapshots.iter().filter(|(k, _)| *k == kind) {
                        if !is_subsequence(snapshot, &joined) {
                            uncovered += 1;
                        }
                    }
                    if kind == TailBlockKind::Reply {
                        for (i, text) in texts.iter().enumerate() {
                            if text.chars().count() < 8 {
                                continue;
                            }
                            prefix_pairs += texts[i + 1..]
                                .iter()
                                .filter(|other| {
                                    other.chars().count() >= 8
                                        && (text.starts_with(other.as_str())
                                            || other.starts_with(text.as_str()))
                                })
                                .count();
                        }
                    }
                }
            }
            eprintln!("replay {} records={records} blocks={blocks} tiny_reply={tiny} prefix_pairs={prefix_pairs} uncovered_snapshots={uncovered}", fields[0]);
            assert!(
                tiny <= fields[3].parse::<usize>().unwrap(),
                "{} tiny block ceiling",
                fields[0]
            );
            assert_eq!(prefix_pairs, 0, "{} turn prefix pairs", fields[0]);
            assert_eq!(uncovered, 0, "{} completed/result coverage", fields[0]);
            assert_eq!(
                std::fs::read(fields[2]).unwrap(),
                source,
                "replay input unchanged"
            );
        }
    }

    #[test]
    fn no_item_snapshot_discards_its_short_replayed_prefix() {
        let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
        let full = "三项授权均已确认，继续处理。";
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", full, None, None),
            tool_use(1),
            rec(2, "assistant", "三", None, None),
            rec(3, "assistant", full, None, None),
            tool_use(4),
        ] {
            events.extend(g.push(&record));
        }
        assert_eq!(frozen_texts(&events), [full]);
        assert!(g.flush_pending().is_none());
    }

    #[test]
    fn result_aggregate_reconciles_visible_resends_and_keeps_new_content() {
        let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
        let first = "Initial reply was displayed.";
        let second = "A repeated repeated word was displayed.";
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", first, None, None),
            tool_use(1),
            rec(2, "assistant", second, None, None),
            result(3, &format!("{first}A repeated word was displayed."), false),
        ] {
            events.extend(g.push(&record));
        }
        assert_eq!(frozen_texts(&events), [first, second]);
        let novel = g.push(&result(4, "Additional final content.", false));
        assert_eq!(frozen_texts(&novel), ["Additional final content."]);
    }

    #[test]
    fn legacy_summary_after_sourced_delta_is_an_independent_block() {
        let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Codex, false);
        let mut delta = rec(0, "thinking", "Sourced delta", Some(true), None);
        delta.stream_source = Some("codex_rpc_summary_delta".into());
        let mut events = g.push(&delta);
        events.extend(g.push(&rec(1, "thinking", "Independent summary", Some(true), None)));
        assert_eq!(
            frozen_texts(&events),
            ["Sourced delta", "Independent summary"]
        );
    }

    #[test]
    fn no_item_growth_snapshot_after_boundary_emits_only_suffix() {
        for partial in [None, Some(false)] {
            let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
            let mut events = Vec::new();
            for record in [
                rec(0, "assistant", "12345678 original", None, None),
                tool_use(1),
                rec(
                    2,
                    "assistant",
                    "12345678 original continued\n",
                    partial,
                    None,
                ),
                tool_use(3),
                rec(
                    4,
                    "assistant",
                    "12345678 original continued\n",
                    partial,
                    None,
                ),
                tool_use(5),
            ] {
                events.extend(g.push(&record));
            }
            assert_eq!(frozen_texts(&events), ["12345678 original", " continued\n"]);
        }
    }

    #[test]
    fn no_item_growth_snapshots_preserve_suffix_with_or_without_active_run() {
        for terminal in [false, true] {
            for active in [false, true] {
                let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
                let mut events = Vec::new();
                events.extend(g.push(&rec(0, "assistant", "12345678 original", None, None)));
                events.extend(g.push(&tool_use(1)));
                if active {
                    events.extend(g.push(&rec(2, "assistant", " continued", None, None)));
                }
                let full = "12345678 original continued with final\n";
                let snapshot = if terminal {
                    result(3, full, false)
                } else {
                    rec(3, "assistant", full, Some(false), None)
                };
                events.extend(g.push(&snapshot));
                assert_eq!(
                    frozen_texts(&events),
                    ["12345678 original", " continued with final\n"]
                );
            }
        }
    }

    #[test]
    fn late_unmarked_growth_snapshot_keeps_newer_active_suffix() {
        let mut g = TailGrouper::for_driver(crate::driver::CliDriver::Cursor, false);
        let mut events = Vec::new();
        for record in [
            rec(0, "assistant", "12345678 original", None, None),
            tool_use(1),
            rec(2, "assistant", " continued with newer text\n", None, None),
            rec(3, "assistant", "12345678 original continued", None, None),
            tool_use(4),
        ] {
            events.extend(g.push(&record));
        }
        assert_eq!(
            frozen_texts(&events),
            ["12345678 original", " continued with newer text\n"]
        );
    }
    #[test]
    fn cursor_acp_word_fragments_merge_until_boundary() {
        // cursor ACP:无 partial 标记的逐词碎片,到 tool_use 定稿为一条。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        for (i, t) in ["I", " need", " to", " read"].iter().enumerate() {
            acc.extend(g.push(&rec(i as u64, "thinking", t, None, None)));
        }
        assert_eq!(
            live_texts(&acc),
            vec!["I", "I need", "I need to", "I need to read"]
        );
        acc.extend(g.push(&tool_use(4)));
        assert_eq!(*kinds(&acc).last().unwrap(), "line");
        assert_eq!(kinds(&acc).iter().filter(|k| **k == "frozen").count(), 1);
        // 时间戳沿用首片。
        let first_live = acc
            .iter()
            .find_map(|e| match e {
                TailOut::Live(b) => Some(b),
                _ => None,
            })
            .unwrap();
        assert_eq!(first_live.ts, 60_000);
        assert_eq!(first_live.kind, TailBlockKind::Thinking);
    }

    #[test]
    fn cursor_streaming_snapshot_replaces_deltas_without_doubling() {
        // cursor CLI:partial=true token 碎片,末尾 partial=false 全文快照;定稿文本只一份。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        for (i, t) in ["Cont", "ract ", "read."].iter().enumerate() {
            acc.extend(g.push(&rec(i as u64, "assistant", t, Some(true), None)));
        }
        acc.extend(g.push(&rec(3, "assistant", "Contract read.", Some(false), None)));
        let frozen_block = acc
            .iter()
            .rev()
            .find_map(|e| match e {
                TailOut::Live(b) => Some(b),
                _ => None,
            })
            .unwrap();
        assert_eq!(frozen_block.text, "Contract read.");
        // 最后两个事件:Live(全文) + Frozen;中间不插独立行。
        let k = kinds(&acc);
        assert_eq!(k[k.len() - 2..], ["live", "frozen"]);
    }

    #[test]
    fn result_full_text_after_snapshot_is_suppressed_usage_kept() {
        // cursor CLI:assistant 快照定稿后,result 又带同一全文 + usage —— 回复不重复,usage 成行。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "assistant", "pong", Some(false), None)));
        acc.extend(g.push(&result(1, "pong", true)));
        let live_count = acc.iter().filter(|e| matches!(e, TailOut::Live(_))).count();
        assert_eq!(live_count, 1, "result 同文回执不另出块");
        assert!(acc
            .iter()
            .any(|e| matches!(e, TailOut::Line(s) if s.contains("usage"))));
    }

    #[test]
    fn claude_whole_blocks_standalone_and_unmarked_not_merged() {
        // claude:merge_unmarked=false。相邻两条独立 assistant 全文各成一块,不拼接。
        let mut g = TailGrouper::new(false, false);
        let acc = g
            .push(&rec(0, "assistant", "第一句完整回复。", Some(false), None))
            .into_iter()
            .chain(g.push(&rec(1, "assistant", "第二句完整回复。", Some(false), None)))
            .collect::<Vec<_>>();
        assert_eq!(live_texts(&acc), ["第一句完整回复。", "第二句完整回复。"]);
        // 无标记老记录(claude 转录)同样按独立块,不并段。
        let mut g2 = TailGrouper::new(false, false);
        let acc2 = g2
            .push(&rec(0, "assistant", "旧记录 A", None, None))
            .into_iter()
            .chain(g2.push(&rec(1, "assistant", "旧记录 B", None, None)))
            .collect::<Vec<_>>();
        assert_eq!(live_texts(&acc2), ["旧记录 A", "旧记录 B"]);
    }

    #[test]
    fn codex_deltas_then_item_snapshot_replaces_and_usage_boundary() {
        // codex RPC:同 item 的 delta 拼接,completed 同 item 全文快照替换定稿;空 result 出 usage。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        for (i, t) in ["已", "补全"].iter().enumerate() {
            acc.extend(g.push(&rec(i as u64, "assistant", t, Some(true), Some("i1"))));
        }
        acc.extend(g.push(&rec(
            2,
            "assistant",
            "已补全表格。",
            Some(false),
            Some("i1"),
        )));
        acc.extend(g.push(&result(3, "", true)));
        let last_live = acc
            .iter()
            .rev()
            .find_map(|e| match e {
                TailOut::Live(b) => Some(b.text.as_str()),
                _ => None,
            })
            .unwrap();
        assert_eq!(last_live, "已补全表格。");
        assert!(acc
            .iter()
            .any(|e| matches!(e, TailOut::Line(s) if s.contains("usage"))));
        // 两次增量预览与一次完成预览，共定稿一个块。
        assert_eq!(
            acc.iter()
                .filter(|e| matches!(e, TailOut::Live(b) if b.kind == TailBlockKind::Reply))
                .count(),
            3
        );
    }

    #[test]
    fn codex_interleaved_items_late_duplicate_snapshot_suppressed() {
        // 交错:delta(i0), delta(i1), completed(i0 同文), completed(i1)。
        // i0 被 i1 打断先定稿;其迟到的同 item 同文快照压掉,不重复成行。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "thinking", "T0", Some(true), Some("i0"))));
        acc.extend(g.push(&rec(1, "thinking", "T1", Some(true), Some("i1"))));
        acc.extend(g.push(&rec(2, "thinking", "T0", Some(false), Some("i0"))));
        acc.extend(g.push(&rec(3, "thinking", "T1", Some(false), Some("i1"))));
        // i0:delta live + 被打断时定稿 live;i1:delta live + 定稿 live;两条 completed 全压掉。
        assert_eq!(live_texts(&acc), ["T0", "T0", "T1", "T1"]);
        assert_eq!(acc.iter().filter(|e| **e == TailOut::Frozen).count(), 2);
    }

    #[test]
    fn tool_interrupted_item_stream_remerges_into_one_block() {
        // 真机形态(后台 tool_use/tool_result 打断一条消息的 token 流):
        // delta(m1) 前缀 → tool_use → tool_result → delta(m1) 续增量 → completed(m1) 全文。
        // 工具穿行期间活动段只让出屏幕(一条 LiveClear),同 item 续上后仍是同一块,
        // completed 到达时一次定稿为全文;completed 重发再压掉。
        let mut g = TailGrouper::new(false, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "assistant", "决策", Some(true), Some("m1"))));
        acc.extend(g.push(&rec(1, "assistant", "明确：", Some(true), Some("m1"))));
        acc.extend(g.push(&tool_use(2)));
        acc.extend(g.push(&tool_result(3)));
        acc.extend(g.push(&rec(4, "assistant", "去做 A", Some(true), Some("m1"))));
        acc.extend(g.push(&rec(5, "assistant", "方案。", Some(true), Some("m1"))));
        acc.extend(g.push(&rec(
            6,
            "assistant",
            "决策明确：去做 A 方案。",
            Some(false),
            Some("m1"),
        )));
        // 工具穿行期间恰好一条 LiveClear(不是每条工具记录一条)。
        assert_eq!(
            kinds(&acc).iter().filter(|k| **k == "clear").count(),
            1,
            "活动段只让出屏幕一次"
        );
        // 只有一次定稿,文本为 completed 全文。
        let frozen_blocks = |acc: &[TailOut]| -> Vec<String> {
            let mut last: Option<&TailBlock> = None;
            let mut blocks = Vec::new();
            for e in acc {
                match e {
                    TailOut::Live(b) => last = Some(b),
                    TailOut::Frozen => {
                        if let Some(b) = last.take() {
                            blocks.push(b.text.clone());
                        }
                    }
                    TailOut::Line(_) | TailOut::LiveClear => last = None,
                }
            }
            blocks
        };
        assert_eq!(
            frozen_blocks(&acc),
            ["决策明确：去做 A 方案。"],
            "被工具打断的同 item token 流最终合成一个块"
        );
        // 同文 completed 再发一遍 → 压掉。
        let again = g.push(&rec(
            7,
            "assistant",
            "决策明确：去做 A 方案。",
            Some(false),
            Some("m1"),
        ));
        assert!(again.is_empty());
    }

    #[test]
    fn parked_run_without_completed_freezes_at_hard_boundary() {
        // item 流在工具穿行后再无 completed(流被截断):下一轮 user_input 是硬边界,
        // 已累积文本必须定稿,不能一直挂着丢失。
        let mut g = TailGrouper::new(false, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "assistant", "先说了半", Some(true), Some("m1"))));
        acc.extend(g.push(&tool_result(1)));
        assert_eq!(*kinds(&acc).last().unwrap(), "line");
        acc.extend(g.push(&user(2)));
        let frozen = acc
            .iter()
            .rev()
            .find_map(|e| match e {
                TailOut::Live(b) => Some(b.text.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(frozen, "先说了半");
    }

    #[test]
    fn different_item_while_parked_freezes_parked_run_first() {
        // 工具穿行后续上的是*另一个* item(真的下一条消息):先定稿挂起段,再起新段。
        let mut g = TailGrouper::new(false, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "assistant", "第一条", Some(true), Some("m1"))));
        acc.extend(g.push(&tool_result(1)));
        acc.extend(g.push(&rec(2, "assistant", "第二条", Some(true), Some("m2"))));
        acc.extend(g.push(&rec(3, "assistant", "第二条", Some(false), Some("m2"))));
        let mut blocks = Vec::new();
        let mut last: Option<&TailBlock> = None;
        for e in &acc {
            match e {
                TailOut::Live(b) => last = Some(b),
                TailOut::Frozen => {
                    if let Some(b) = last.take() {
                        blocks.push(b.text.clone());
                    }
                }
                TailOut::Line(_) | TailOut::LiveClear => last = None,
            }
        }
        assert_eq!(blocks, ["第一条", "第二条"]);
    }

    #[test]
    fn no_item_run_still_freezes_at_tool_boundary() {
        // cursor 通道无 item 标识:工具记录无法判定同段续写,仍是硬边界,立即定稿。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "assistant", "cursor 碎片", None, None)));
        acc.extend(g.push(&tool_result(1)));
        assert!(!kinds(&acc).contains(&"clear"));
        assert_eq!(acc.iter().filter(|e| **e == TailOut::Frozen).count(), 1);
    }

    #[test]
    fn naked_partial_true_without_provenance_are_independent_summaries() {
        // 旧版 codex 日志:partial=true 但无 item/stream_source,每条是独立 summary 句子,
        // 不能粘成巨型块(cursor 通道 merge_unmarked=true 时才按碎片)。
        let mut g = TailGrouper::new(false, false);
        let acc = g
            .push(&rec(0, "thinking", "**Analyzing X**", Some(true), None))
            .into_iter()
            .chain(g.push(&rec(1, "thinking", "**Assessing Y**", Some(true), None)))
            .collect::<Vec<_>>();
        assert_eq!(live_texts(&acc), ["**Analyzing X**", "**Assessing Y**"]);
    }

    #[test]
    fn independent_channel_identical_blocks_across_tools_are_kept() {
        // claude 等独立段落通道:跨工具边界的同文 thinking/reply 必须各自保留,不做去重。
        let mut g = TailGrouper::new(false, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "thinking", "Response", Some(false), None)));
        acc.extend(g.push(&tool_use(1)));
        acc.extend(g.push(&rec(2, "thinking", "Response", Some(false), None)));
        acc.extend(g.push(&tool_use(3)));
        acc.extend(g.push(&rec(4, "assistant", "好的", Some(false), None)));
        acc.extend(g.push(&tool_use(5)));
        acc.extend(g.push(&rec(6, "assistant", "好的", Some(false), None)));
        assert_eq!(live_texts(&acc), ["Response", "Response", "好的", "好的"]);
    }

    #[test]
    fn cursor_aggregate_result_is_suppressed_but_segments_and_usage_kept() {
        // 旧采集版 cursor:多段 reply(被 thinking/tool 分隔),result 文本是各段无分隔聚合。
        // 每段各显示一次,result 聚合整体压掉,usage 行保留。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "assistant", "第一段", None, None)));
        acc.extend(g.push(&rec(1, "assistant", "未完", None, None)));
        acc.extend(g.push(&rec(2, "thinking", "想想", None, None)));
        acc.extend(g.push(&rec(3, "assistant", "第二段", None, None)));
        acc.extend(g.push(&result(4, "第一段未完第二段", true)));
        // 每次定稿(终版 Live 紧邻 Frozen)的 reply 块。
        let mut reply_blocks = Vec::new();
        let mut last: Option<&TailBlock> = None;
        for e in &acc {
            match e {
                TailOut::Live(b) => last = Some(b),
                TailOut::Frozen => {
                    if let Some(b) = last.take() {
                        if b.kind == TailBlockKind::Reply {
                            reply_blocks.push(b.text.clone());
                        }
                    }
                }
                TailOut::Line(_) | TailOut::LiveClear => last = None,
            }
        }
        assert_eq!(reply_blocks, ["第一段未完", "第二段"], "result 聚合不重放");
        assert!(acc
            .iter()
            .any(|e| matches!(e, TailOut::Line(s) if s.contains("usage"))));
    }

    #[test]
    fn thinking_run_finalizes_before_reply_run() {
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "thinking", "想想", Some(true), None)));
        acc.extend(g.push(&rec(1, "assistant", "答答", Some(true), None)));
        assert_eq!(
            acc.iter().filter(|e| **e == TailOut::Frozen).count(),
            1,
            "thinking 段在 reply 开始时定稿一次"
        );
        acc.extend(g.push(&user(2)));
        assert_eq!(
            acc.iter().filter(|e| **e == TailOut::Frozen).count(),
            2,
            "reply 段在用户入站时定稿"
        );
    }

    #[test]
    fn merged_block_renders_real_newlines_with_indent() {
        let b = TailBlock {
            ts: 61_000,
            kind: TailBlockKind::Reply,
            text: "第一行\n第二行\r\n第三行\r第四行".to_string(),
        };
        let s = b.render(false);
        let indent = " ".repeat(BLOCK_INDENT);
        assert_eq!(
            s,
            format!(
                "{}  reply      第一行\n{indent}第二行\n{indent}第三行\n{indent}第四行",
                hms(61_000)
            )
        );
    }

    #[test]
    fn unmarked_fragment_growth_snapshot_takes_longer() {
        // cursor 无 item 通道的增长快照:累积已足够长(≥8 字符)时,后块含前块为前缀
        // → 取更长,不重复拼接。aggregated 快照只在消息接近完整时到达,此时累积必然已长。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "assistant", "Contract", Some(true), None)));
        acc.extend(g.push(&rec(1, "assistant", "Contract read.", None, None)));
        assert_eq!(live_texts(&acc), ["Contract", "Contract read."]);
    }

    #[test]
    fn short_fragments_sharing_first_char_are_concatenated_not_replaced() {
        // 真机回归(cursor ACP):碎片 `"S"` + `"SH "` 的真实文本是 `SSH `。短串上做
        // 「增长快照替换」会丢字符;累积不足 8 字符时一律按增量拼接。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&rec(0, "assistant", "S", None, None)));
        acc.extend(g.push(&rec(1, "assistant", "SH ", None, None)));
        acc.extend(g.push(&rec(2, "assistant", "通了", None, None)));
        assert_eq!(*live_texts(&acc).last().unwrap(), "SSH 通了");
    }

    #[test]
    fn standalone_result_text_without_active_run_emits_block_and_usage() {
        let mut g = TailGrouper::new(true, false);
        let acc = g.push(&result(0, "只有 result 带正文", true));
        assert_eq!(live_texts(&acc), ["只有 result 带正文"]);
        assert_eq!(kinds(&acc), ["live", "frozen", "line"]);
    }

    #[test]
    fn identical_short_reply_in_next_turn_is_not_suppressed() {
        // 无 item 通道:上一轮 result「收到」已显示;新一轮 user_input 后再来一条同文 result,
        // 轮边界已清去重登记,必须照常显示。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&result(0, "收到", false)));
        acc.extend(g.push(&user(1)));
        acc.extend(g.push(&result(2, "收到", false)));
        assert_eq!(live_texts(&acc), ["收到", "收到"]);
    }

    #[test]
    fn whitespace_only_delta_does_not_split_active_run() {
        // codex 真机:token 流里夹着单个空格增量(codex_rpc_delta,text=" ")。
        // 它必须归附活动段,而不是把一句话从空格处劈成两条。
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        for (i, t) in ["新增", " ", "2", " 条", "：\n\n"].iter().enumerate() {
            acc.extend(g.push(&rec(i as u64, "assistant", t, Some(true), Some("i1"))));
        }
        acc.extend(g.push(&rec(
            5,
            "assistant",
            "新增 2 条：\n\n",
            Some(false),
            Some("i1"),
        )));
        acc.extend(g.push(&tool_use(6)));
        let blocks: Vec<_> = acc
            .iter()
            .filter_map(|e| match e {
                TailOut::Live(b) => Some(b.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(*blocks.last().unwrap(), "新增 2 条：\n\n");
        // 只有一个定稿块(tool 边界那次)。
        assert_eq!(acc.iter().filter(|e| **e == TailOut::Frozen).count(), 1);
    }

    #[test]
    fn codex_summary_completed_snapshot_replaces_delta_despite_partial_true() {
        // codex RPC thinking:delta 与 completed 都标 partial=true,只靠 stream_source 区分。
        // 快照到达必须替换而非拼接,且各 summary part(item 不同)各成一条。
        fn summary(seq: u64, text: &str, item: &str, completed: bool) -> EventRecord {
            let mut r = rec(seq, "thinking", text, Some(true), Some(item));
            r.stream_source = Some(if completed {
                "codex_rpc_summary_completed".to_string()
            } else {
                "codex_rpc_summary_delta".to_string()
            });
            r
        }
        let mut g = TailGrouper::new(true, false);
        let mut acc = Vec::new();
        acc.extend(g.push(&summary(0, "**Inspecting**", "rs#summary:0", false)));
        acc.extend(g.push(&summary(1, "**Listing**", "rs#summary:1", false)));
        acc.extend(g.push(&summary(2, "**Inspecting**", "rs#summary:0", true)));
        acc.extend(g.push(&summary(3, "**Listing**", "rs#summary:1", true)));
        acc.extend(g.push(&tool_use(4)));
        assert_eq!(
            live_texts(&acc),
            [
                "**Inspecting**",
                "**Inspecting**",
                "**Listing**",
                "**Listing**"
            ],
            "item 切换时定稿旧块(补发终版);两条 completed 同文快照全部压掉"
        );
    }

    #[test]
    fn live_preview_is_single_line_flattened_and_truncated_to_columns() {
        let b = TailBlock {
            ts: 60_000,
            kind: TailBlockKind::Reply,
            text: "第一段不短的开头\n第二段内容也还在继续加长超出预算".to_string(),
        };
        let s = render_live_preview(&b, false, 30);
        assert!(!s.contains('\n'), "进行中预览恒单行");
        assert!(s.ends_with('…'), "超列宽截断并缀省略号");
        assert!(s.contains("第一段"));
        assert!(!s.contains("第二段"), "折行后的段落不出现在单行预览");
        assert!(s.starts_with(&format!("{}  reply      ", hms(60_000))));
        // 显示宽度(CJK 两列)不得超出给定列宽。
        let visible = strip_ansi_escapes::strip(&s);
        let width: usize = String::from_utf8(visible)
            .unwrap()
            .chars()
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        assert!(width < 30, "预览宽度 {width} 须给 30 列终端留一列");
        // 窄列兜底不 panic。
        let tiny = render_live_preview(&b, false, 0);
        assert!(tiny.contains("reply"));
    }
}
