//! 会话历史投影：把高频 `events.jsonl` 事件流归并成人可读的轮次。
//!
//! provider 可能逐字符产生 assistant/thinking 事件，一轮数千行很常见。UI 若直接消费原始流，
//! 不仅慢，还会复制“哪个事件才是最终回复”的判定。这里把事件收口成稳定 `HistoryTurn`。

use crate::event_log::{EventRecord, UsageRec};
use crate::parse::agent_msg::is_turn_end;
use serde::{Deserialize, Serialize};

const HISTORY_TEXT_CAP: usize = 200_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryTurnStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryTurn {
    /// 归并后的单调轮次号。原始 EventRecord.seq 每个 sink 会重置，不能用于分页。
    pub ordinal: u64,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub status: HistoryTurnStatus,
    pub tool_uses: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageRec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub target: String,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before: Option<u64>,
    pub turns: Vec<HistoryTurn>,
}

#[derive(Debug, Default)]
struct PendingTurn {
    started_at_ms: u64,
    ended_at_ms: u64,
    user: Option<String>,
    assistant_segments: Vec<String>,
    assistant_segment: String,
    assistant_text_len: usize,
    final_assistant: Option<String>,
    error: Option<String>,
    status: Option<HistoryTurnStatus>,
    tool_uses: usize,
    usage: Option<UsageRec>,
    saw_agent_event: bool,
    /// 已见本轮终态信号(转录路径的 `end_turn`),但还没 flush —— 留一拍等可能紧随的 `result`
    /// (无头路径把最终正文/usage 放在那条)。除 result 外的任何后续事件都属于下一轮。
    closed: bool,
}

impl PendingTurn {
    fn has_content(&self) -> bool {
        self.user.is_some()
            || !self.assistant_segments.is_empty()
            || !self.assistant_segment.is_empty()
            || self.final_assistant.is_some()
            || self.error.is_some()
            || self.tool_uses > 0
            || self.usage.is_some()
    }

    fn start_at(&mut self, ts: u64) {
        if self.started_at_ms == 0 {
            self.started_at_ms = ts;
        }
        self.ended_at_ms = ts;
    }

    fn append_assistant(&mut self, text: &str) {
        if self.assistant_text_len >= HISTORY_TEXT_CAP {
            return;
        }
        let remaining = HISTORY_TEXT_CAP - self.assistant_text_len;
        let mut end = text.len().min(remaining);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let text = &text[..end];
        let old_len = self.assistant_segment.len();
        merge_stream_segment(&mut self.assistant_segment, text);
        self.assistant_text_len += self.assistant_segment.len().saturating_sub(old_len);
    }

    fn finish_assistant_segment(&mut self) {
        if !self.assistant_segment.is_empty() {
            self.assistant_segments
                .push(std::mem::take(&mut self.assistant_segment));
        }
    }

    fn finish(mut self, ordinal: u64) -> Option<HistoryTurn> {
        if !self.has_content() {
            return None;
        }
        self.finish_assistant_segment();
        let assistant_stream = self.assistant_segments.join("\n\n");
        let assistant = self
            .final_assistant
            .or_else(|| (!assistant_stream.is_empty()).then_some(assistant_stream));
        Some(HistoryTurn {
            ordinal,
            started_at_ms: self.started_at_ms,
            ended_at_ms: self.ended_at_ms,
            user: self.user,
            assistant,
            error: self.error,
            status: self.status.unwrap_or(HistoryTurnStatus::Running),
            tool_uses: self.tool_uses,
            usage: self.usage,
        })
    }
}

/// 流式归并器；CLI 可边读大文件边喂，不需要把原始事件全部留在内存。
#[derive(Debug, Default)]
pub struct HistoryBuilder {
    current: PendingTurn,
    turns: Vec<HistoryTurn>,
    next_ordinal: u64,
}

impl HistoryBuilder {
    pub fn push(&mut self, record: EventRecord) {
        // 本轮已收口(见过 end_turn):只有 `result` 还算同一轮的尾巴，其余一律是下一轮的开头。
        // 先收再处理，两轮连续时不会被并成一条。
        if self.current.closed && record.kind != "result" {
            self.flush();
        }
        // 终态信号与事件 kind 解耦(与捕获层 `capture::transcript` 同判):claude 把同一条消息的
        // thinking / text 块拆成相邻多条记录、**每条都带同一个 stop_reason**，一轮可能只有 thinking
        // 那条带 end_turn(真机 2026-07-27 arcadia:该轮在 `agents history` 里永远 running)。
        let end_turn_outside_assistant = marks_turn_end_outside_assistant(&record.kind)
            && is_turn_end(record.stop_reason.as_deref());
        match record.kind.as_str() {
            "user_input" => {
                self.flush();
                self.current.start_at(record.ts);
                self.current.user = non_empty(record.text);
            }
            // 会话建立时的目录记录不是 agent 输出,不开轮也不切轮。
            "model_catalog" => {}
            "system" => {
                // append_user_input 与真正 capture 分别打开 sink，system 紧跟 user_input 且 seq 重置。
                // 只有已经见过 agent 输出时，system 才表示旧格式中的下一轮边界。
                if self.current.saw_agent_event {
                    self.flush();
                }
                self.current.start_at(record.ts);
                self.current.saw_agent_event = true;
            }
            "assistant" => {
                self.current.start_at(record.ts);
                self.current.saw_agent_event = true;
                if let Some(text) = record.text.as_deref() {
                    self.current.append_assistant(text);
                }
                // claude/Transcript 的**唯一**终态信号：转录 jsonl 里没有 `type:"result"` 行，
                // 一轮的结束就写在 assistant 块的 `stop_reason=end_turn` 上。只认 result 会让
                // 这条路径的每一轮永远停在 running(真机 arcadia:29 轮全 running)。
                // 这里只收口不立刻 flush —— 无头路径的 result 紧随其后，要留给它。
                if is_turn_end(record.stop_reason.as_deref()) {
                    self.current.status = Some(HistoryTurnStatus::Completed);
                    self.current.closed = true;
                }
            }
            "tool_use" => {
                self.current.start_at(record.ts);
                self.current.saw_agent_event = true;
                // 工具调用是 assistant 文本段的天然边界；下一段可能恰好以相同 token 开头，
                // 不先收尾会被段内重发去重规则误判。
                self.current.finish_assistant_segment();
                self.current.tool_uses += 1;
            }
            "result" => {
                self.current.start_at(record.ts);
                self.current.saw_agent_event = true;
                if let Some(text) = non_empty(record.text) {
                    // result 是 provider 给出的最终正文；存在时覆盖增量流，防逐字符事件重复。
                    self.current.final_assistant = Some(text);
                }
                self.current.usage = record.usage;
                self.current.status = Some(HistoryTurnStatus::Completed);
                self.flush();
            }
            "error" => {
                self.current.start_at(record.ts);
                self.current.saw_agent_event = true;
                self.current.error = non_empty(record.text)
                    .or_else(|| Some(summon_i18n::text!("r13a46616e1").to_string()));
                self.current.status = Some(HistoryTurnStatus::Failed);
                self.flush();
            }
            _ => {
                self.current.start_at(record.ts);
                self.current.saw_agent_event = true;
                if record.usage.is_some() {
                    self.current.usage = record.usage;
                }
            }
        }
        // 只收口成 completed，**不置 `closed`**:`closed` 是切轮闸门，若 thinking 也置上，同一条消息
        // 紧随其后的 text 记录会被当成下一轮，把一轮劈成两条(真机 60 份转录里 430/788 条 end_turn
        // 消息就是这种 thinking+text 相邻两条的形状)。切轮仍由 assistant / result 那条说了算。
        if end_turn_outside_assistant {
            self.current.status = Some(HistoryTurnStatus::Completed);
        }
    }

    pub fn finish(mut self) -> Vec<HistoryTurn> {
        self.flush();
        self.turns
    }

    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.current);
        if pending.has_content() {
            self.next_ordinal += 1;
            if let Some(turn) = pending.finish(self.next_ordinal) {
                self.turns.push(turn);
            }
        }
    }
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
pub(crate) fn build_history(records: impl IntoIterator<Item = EventRecord>) -> Vec<HistoryTurn> {
    let mut builder = HistoryBuilder::default();
    for record in records {
        builder.push(record);
    }
    builder.finish()
}

pub fn history_page(
    target: impl Into<String>,
    turns: &[HistoryTurn],
    before: Option<u64>,
    limit: usize,
) -> HistoryPage {
    let limit = limit.max(1);
    let eligible_end =
        turns.partition_point(|turn| before.map(|b| turn.ordinal < b).unwrap_or(true));
    let start = eligible_end.saturating_sub(limit);
    let page_turns = turns[start..eligible_end].to_vec();
    HistoryPage {
        target: target.into(),
        has_more: start > 0,
        next_before: (start > 0)
            .then(|| page_turns.first().map(|turn| turn.ordinal))
            .flatten(),
        turns: page_turns,
    }
}

/// 该 kind 的记录带 end_turn 时，是否需要在 assistant 臂之外补判终态。
/// - `assistant` 自己的臂已经判了(还要负责置 `closed` 切轮)；
/// - `result` / `error` / `user_input` / `system` 会**切轮**，在它们身上补判会把状态记到刚开的新轮上；
/// - 其余(`thinking`，以及将来新增的块类型)都要补判 —— 这正是「判定与 kind 解耦」的落点。
fn marks_turn_end_outside_assistant(kind: &str) -> bool {
    !matches!(
        kind,
        "assistant" | "result" | "error" | "user_input" | "system" | "model_catalog"
    )
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

/// provider 可能先发 token delta，再把同一完整段重发一次；也可能直接发累积快照。
/// 重复/更短快照忽略，增长快照替换，其余视为真增量拼接。
fn merge_stream_segment(buf: &mut String, text: &str) {
    if buf.is_empty() {
        buf.push_str(text);
    } else if text == buf.as_str() || buf.starts_with(text) {
        // 已经完整包含。
    } else if text.starts_with(buf.as_str()) {
        buf.clear();
        buf.push_str(text);
    } else {
        buf.push_str(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seq: u64, kind: &str, text: Option<&str>) -> EventRecord {
        EventRecord {
            ts: 1_000 + seq,
            seq,
            kind: kind.to_string(),
            text: text.map(String::from),
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

    /// claude/Transcript 路径的终态事件：assistant 块带 `stop_reason=end_turn`(**无 result 事件**)。
    fn end_turn(seq: u64, text: &str) -> EventRecord {
        let mut rec = record(seq, "assistant", Some(text));
        rec.stop_reason = Some("end_turn".to_string());
        rec
    }

    /// 中途 assistant 块：claude 在调工具前后写的叙述，`stop_reason` 是 tool_use 而非终态。
    fn mid_turn(seq: u64, text: &str) -> EventRecord {
        let mut rec = record(seq, "assistant", Some(text));
        rec.stop_reason = Some("tool_use".to_string());
        rec
    }

    #[test]
    fn groups_stream_events_into_conversation_turns() {
        let turns = build_history([
            record(1, "user_input", Some("帮我检查")),
            // 新 sink 从 1 重置；system 不能把刚写的 user 拆成空轮。
            record(1, "system", None),
            record(2, "assistant", Some("好")),
            record(3, "assistant", Some("的")),
            record(4, "tool_use", None),
            record(5, "result", Some("检查完成")),
            record(1, "user_input", Some("结论？")),
            record(1, "system", None),
            record(2, "error", Some("被打断")),
        ]);

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].ordinal, 1);
        assert_eq!(turns[0].user.as_deref(), Some("帮我检查"));
        assert_eq!(turns[0].assistant.as_deref(), Some("检查完成"));
        assert_eq!(turns[0].tool_uses, 1);
        assert_eq!(turns[0].status, HistoryTurnStatus::Completed);
        assert_eq!(turns[1].error.as_deref(), Some("被打断"));
        assert_eq!(turns[1].status, HistoryTurnStatus::Failed);
    }

    #[test]
    fn deduplicates_full_segment_reemitted_after_deltas() {
        let turns = build_history([
            record(1, "user_input", Some("分析")),
            record(2, "assistant", Some("实")),
            record(3, "assistant", Some("验开始")),
            record(4, "assistant", Some("实验开始")),
            record(5, "tool_use", None),
            record(6, "assistant", Some("第")),
            record(7, "assistant", Some("二段")),
            record(8, "assistant", Some("第二段")),
        ]);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].assistant.as_deref(), Some("实验开始\n\n第二段"));
    }

    #[test]
    fn system_delimits_legacy_turns_without_user_input() {
        let turns = build_history([
            record(1, "system", None),
            record(2, "assistant", Some("第一轮")),
            record(1, "system", None),
            record(2, "assistant", Some("第二轮进行中")),
        ]);

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].assistant.as_deref(), Some("第一轮"));
        assert_eq!(turns[0].status, HistoryTurnStatus::Running);
        assert_eq!(turns[1].assistant.as_deref(), Some("第二轮进行中"));
    }

    /// 回归(真机 2026-07-27):claude/Transcript 会话的 events.jsonl **一条 `result` 都没有**
    /// (arcadia 同群 440 事件 / 0 result),因为 result 是无头 stream-json 的 `type:"result"` 行，
    /// 而转录 jsonl 里只有 assistant 块。轮次边界若只认 result,整条会话每一轮永远停在 running。
    /// 转录路径的等价终态信号是 assistant 块的 `stop_reason=end_turn`。
    #[test]
    fn transcript_end_turn_completes_turn_without_result_event() {
        let turns = build_history([
            record(1, "user_input", Some("列一下当前目录有几个文件")),
            mid_turn(2, "Let me check the file."),
            record(3, "tool_use", None),
            end_turn(4, "There are 2 files."),
        ]);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, HistoryTurnStatus::Completed);
        assert_eq!(turns[0].tool_uses, 1);
        assert_eq!(
            turns[0].assistant.as_deref(),
            Some("Let me check the file.\n\nThere are 2 files.")
        );
    }

    /// Subprocess(cursor/codex,以及 claude headless)行为**不变**:真机 `claude -p` 的 assistant
    /// 块同样带 `end_turn`,但其后还有一条带 usage 与最终正文的 `result`。end_turn 只能**收口**本轮、
    /// 不能立刻切轮，否则一轮会被劈成两条(第二条只剩 result 正文、丢掉 user)。
    #[test]
    fn subprocess_result_after_end_turn_stays_one_turn() {
        let mut result = record(4, "result", Some("There are 2 files."));
        result.usage = Some(UsageRec {
            input: 100,
            output: 20,
            cache_read: 0,
            cache_write: 0,
        });
        let turns = build_history([
            record(1, "user_input", Some("列一下当前目录有几个文件")),
            record(2, "tool_use", None),
            end_turn(3, "There are 2 files."),
            result,
        ]);

        assert_eq!(turns.len(), 1, "end_turn + result 是同一轮,不能劈成两条");
        assert_eq!(turns[0].user.as_deref(), Some("列一下当前目录有几个文件"));
        assert_eq!(turns[0].status, HistoryTurnStatus::Completed);
        // result 正文仍覆盖增量流(原行为),usage 也仍归到这一轮。
        assert_eq!(turns[0].assistant.as_deref(), Some("There are 2 files."));
        assert_eq!(turns[0].usage.as_ref().map(|u| u.output), Some(20));
    }

    /// 两轮连续(转录路径，无 result):第二轮的 assistant 不得并进已收口的第一轮 —— 哪怕中间
    /// 没有 user_input(镜像轮 / 旧数据里 user_input 缺失时的形状)。
    #[test]
    fn consecutive_transcript_turns_are_not_merged() {
        let turns = build_history([
            record(1, "user_input", Some("第一问")),
            end_turn(2, "第一答"),
            end_turn(3, "第二答"),
        ]);

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].assistant.as_deref(), Some("第一答"));
        assert_eq!(turns[0].status, HistoryTurnStatus::Completed);
        assert_eq!(turns[1].user, None);
        assert_eq!(turns[1].assistant.as_deref(), Some("第二答"));
        assert_eq!(turns[1].status, HistoryTurnStatus::Completed);
    }

    /// 真正在跑的一轮(还没见到 end_turn / result)**仍是 running** —— 修复不能把「没结论」
    /// 也说成完成，否则 Menu Bar / 控制台的忙闲显示反向失真。
    #[test]
    fn in_flight_transcript_turn_stays_running() {
        let turns = build_history([
            record(1, "user_input", Some("跑个长任务")),
            mid_turn(2, "开始了"),
            record(3, "tool_use", None),
        ]);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, HistoryTurnStatus::Running);
    }

    /// 29 轮同构复现(owner 机 arcadia `g00000000a11ce04d` 的形状：user_input + 若干 assistant/
    /// tool_use + end_turn,无任何 result):除最后一轮在跑外，其余全部 completed 且轮数不塌缩。
    #[test]
    fn many_transcript_turns_all_complete_except_the_live_one() {
        let mut records = Vec::new();
        let mut seq = 0;
        for i in 0..29 {
            seq += 1;
            records.push(record(seq, "user_input", Some(&format!("问题{i}"))));
            seq += 1;
            records.push(mid_turn(seq, "思考中"));
            seq += 1;
            records.push(record(seq, "tool_use", None));
            // 最后一轮不写 end_turn:模拟「此刻正在跑」。
            if i < 28 {
                seq += 1;
                records.push(end_turn(seq, &format!("答案{i}")));
            }
        }

        let turns = build_history(records);
        assert_eq!(turns.len(), 29);
        for turn in &turns[..28] {
            assert_eq!(
                turn.status,
                HistoryTurnStatus::Completed,
                "第 {} 轮早已结束,不该显示进行中",
                turn.ordinal
            );
        }
        assert_eq!(turns[28].status, HistoryTurnStatus::Running);
    }

    /// thinking 记录带 end_turn(claude 拆块写转录时，一轮可能只有 thinking 那条带终态)。
    /// 与捕获层同判:这一轮已经结束，不能永远停在 running。
    #[test]
    fn thinking_only_end_turn_completes_turn() {
        let mut thinking = record(2, "thinking", Some("想完了"));
        thinking.stop_reason = Some("end_turn".to_string());
        let turns = build_history([record(1, "user_input", Some("在吗")), thinking]);

        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].status,
            HistoryTurnStatus::Completed,
            "thinking 上的 end_turn 也是终态,否则这一轮在 agents history 里永远 running"
        );
    }

    /// 同一条消息被拆成 thinking + text **两条相邻记录、两条都带 end_turn**(真机 430/788 条
    /// end_turn 消息的形状)。thinking 那条只能收口状态、**不能切轮**，否则一轮被劈成两条、
    /// 正文与 user 分家。
    #[test]
    fn thinking_and_text_records_of_one_message_stay_one_turn() {
        let mut thinking = record(2, "thinking", Some("先想想"));
        thinking.stop_reason = Some("end_turn".to_string());
        let turns = build_history([
            record(1, "user_input", Some("在吗")),
            thinking,
            end_turn(3, "在的"),
        ]);

        assert_eq!(turns.len(), 1, "同一条消息的两条记录不能被劈成两轮");
        assert_eq!(turns[0].user.as_deref(), Some("在吗"));
        assert_eq!(turns[0].assistant.as_deref(), Some("在的"));
        assert_eq!(turns[0].status, HistoryTurnStatus::Completed);
    }

    /// thinking 记录带 stop_sequence(伪影)不是终态 —— 与捕获层同判。
    #[test]
    fn thinking_stop_sequence_does_not_complete_turn() {
        let mut thinking = record(2, "thinking", Some("无需回复"));
        thinking.stop_reason = Some("stop_sequence".to_string());
        let turns = build_history([record(1, "user_input", Some("在吗")), thinking]);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, HistoryTurnStatus::Running);
    }

    #[test]
    fn page_returns_latest_turns_in_chronological_order() {
        let turns: Vec<HistoryTurn> = (1..=5)
            .map(|ordinal| HistoryTurn {
                ordinal,
                started_at_ms: ordinal,
                ended_at_ms: ordinal,
                user: Some(format!("u{ordinal}")),
                assistant: None,
                error: None,
                status: HistoryTurnStatus::Completed,
                tool_uses: 0,
                usage: None,
            })
            .collect();

        let latest = history_page("soul-x-g1", &turns, None, 2);
        assert_eq!(
            latest.turns.iter().map(|t| t.ordinal).collect::<Vec<_>>(),
            [4, 5]
        );
        assert!(latest.has_more);
        assert_eq!(latest.next_before, Some(4));

        let older = history_page("soul-x-g1", &turns, latest.next_before, 2);
        assert_eq!(
            older.turns.iter().map(|t| t.ordinal).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(older.next_before, Some(2));
    }
}
