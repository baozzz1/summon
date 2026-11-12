//! 无头轮的子任务汇总卡；只使用已观测的子线程状态，按固定节拍更新同一张消息。

use super::headless_activity::{ChildStatus, HeadlessActivity};
use crate::frontend::{ChatRef, Frontend, StreamId, StreamPatchOutcome};
use std::time::{Duration, Instant};

const UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const MAX_ROWS: usize = 12;

pub(super) struct SubagentCard {
    enabled: bool,
    attempted: bool,
    id: Option<StreamId>,
    last_attempt: Option<Instant>,
    delivered: String,
}

impl SubagentCard {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            attempted: false,
            id: None,
            last_attempt: None,
            delivered: String::new(),
        }
    }

    pub(super) fn update(
        &mut self,
        activity: &HeadlessActivity,
        frontend: &dyn Frontend,
        chat: &ChatRef,
        now: Instant,
    ) {
        if !self.enabled || frontend.result_only() || (self.attempted && self.id.is_none()) {
            return;
        }
        if self
            .last_attempt
            .is_some_and(|last| now.saturating_duration_since(last) < UPDATE_INTERVAL)
        {
            return;
        }
        let Some(card) = render(activity, false) else {
            return;
        };
        if card == self.delivered {
            return;
        }
        self.last_attempt = Some(now);
        if let Some(id) = self.id {
            match frontend.stream_patch_card_observed(id, &card) {
                Ok(StreamPatchOutcome::Applied) => self.delivered = card,
                Ok(StreamPatchOutcome::MissingStream) => self.id = None,
                Ok(StreamPatchOutcome::Throttled) => {}
                Err(error) => crate::observe::log::log(&summon_i18n::format!("rc86409496b")),
            }
        } else {
            // 创建请求可能已到达平台，错误返回后重发会产生重复卡片。
            self.attempted = true;
            match frontend.stream_begin_card(chat, &card) {
                Ok(id) => {
                    self.id = Some(id);
                    self.delivered = card;
                }
                Err(error) => crate::observe::log::log(&summon_i18n::format!("ra4145872f8")),
            }
        }
    }

    pub(super) fn finish(
        &mut self,
        activity: &HeadlessActivity,
        frontend: &dyn Frontend,
        chat: &ChatRef,
    ) {
        if !self.enabled || frontend.result_only() {
            return;
        }
        // 很短的轮也交付一次最终观测；已经尝试创建的轮不会再建第二张。
        if !self.attempted {
            self.update(activity, frontend, chat, Instant::now());
        }
        if let (Some(id), Some(card)) = (self.id.take(), render(activity, true)) {
            if let Err(error) = frontend.stream_freeze_card(id, &card) {
                crate::observe::log::log(&summon_i18n::format!("rf8339bdb39"));
            }
        }
    }
}

fn label(text: &str) -> String {
    escaped(text, 80)
}

fn escaped(text: &str, max: usize) -> String {
    let mut safe = String::new();
    for c in text.chars().filter(|c| !c.is_control()).take(max) {
        if "\\*_`[]()!#~|".contains(c) {
            safe.push('\\');
        }
        safe.push(c);
    }
    safe
}

fn body_preview(text: &str) -> String {
    const MAX_BODY_CHARS: usize = 360;
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview = escaped(&normalized, MAX_BODY_CHARS);
    if normalized.chars().count() > MAX_BODY_CHARS {
        preview.push('…');
    }
    preview
}

fn render(activity: &HeadlessActivity, settled: bool) -> Option<String> {
    let children = activity.children();
    if children.is_empty() {
        return None;
    }
    let running = children
        .iter()
        .filter(|work| work.status == ChildStatus::Running)
        .count();
    let mut body = summon_i18n::format!("r28c2b4c14d", children.len());
    if settled {
        body.push_str(if running > 0 {
            summon_i18n::text!("rb1c8c14662")
        } else {
            summon_i18n::text!("r851f1ad600")
        });
    }
    for work in children.iter().take(MAX_ROWS) {
        let status = match work.status {
            ChildStatus::Running if settled => summon_i18n::text!("rcdea037991"),
            ChildStatus::Running => summon_i18n::text!("r1f0eb99b7e"),
            ChildStatus::Completed => summon_i18n::text!("rf28461bb49"),
            ChildStatus::Failed => summon_i18n::text!("r28384d7afd"),
            ChildStatus::Interrupted => summon_i18n::text!("r88cf2dc00c"),
        };
        let stamp = work
            .body_updated
            .as_deref()
            .map(|updated| summon_i18n::format!("rf1e2a2cbc7"))
            .unwrap_or_default();
        let latest = work
            .latest_body()
            .map(body_preview)
            .unwrap_or_else(|| summon_i18n::text!("r4ada2dd47d").to_string());
        body.push_str(&format!(
            "\n- {}：{}{stamp}\n  {latest}",
            label(&work.name),
            status
        ));
    }
    if children.len() > MAX_ROWS {
        body.push_str(&summon_i18n::format!(
            "r023ff6fee1",
            children.len() - MAX_ROWS
        ));
    }
    Some(summon_core::feishu::card_markdown_v2(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::MsgRef;
    use std::cell::{Cell, RefCell};
    use summon_core::agent_msg::{AgentKind, AgentMessage, RunId, TurnStatus};

    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<(String, String)>>,
        fail_begin: Cell<bool>,
        throttle: Cell<bool>,
        fail_patch: Cell<bool>,
        result_only: bool,
    }
    impl Frontend for Fake {
        fn result_only(&self) -> bool {
            self.result_only
        }
        fn send_message(&self, _: &ChatRef, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn stream_begin(&self, _: &ChatRef, body: &str, _: bool) -> anyhow::Result<StreamId> {
            self.calls.borrow_mut().push(("begin".into(), body.into()));
            anyhow::ensure!(!self.fail_begin.get(), "begin failed");
            Ok(StreamId(99))
        }
        fn stream_patch(&self, _: StreamId, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn stream_patch_card_observed(
            &self,
            id: StreamId,
            body: &str,
        ) -> anyhow::Result<StreamPatchOutcome> {
            assert_eq!(id, StreamId(99));
            self.calls.borrow_mut().push(("patch".into(), body.into()));
            anyhow::ensure!(!self.fail_patch.replace(false), "patch failed");
            Ok(if self.throttle.replace(false) {
                StreamPatchOutcome::Throttled
            } else {
                StreamPatchOutcome::Applied
            })
        }
        fn stream_finish(&self, _: StreamId, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn stream_freeze_card(&self, id: StreamId, body: &str) -> anyhow::Result<()> {
            assert_eq!(id, StreamId(99));
            self.calls.borrow_mut().push(("freeze".into(), body.into()));
            Ok(())
        }
        fn react(&self, _: &MsgRef, _: &str) -> anyhow::Result<String> {
            unreachable!()
        }
        fn unreact(&self, _: &MsgRef, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
    }
    fn chat() -> ChatRef<'static> {
        ChatRef {
            agent_dir: std::path::Path::new("/tmp/subagent-card-test"),
            chat_type: "group",
            chat_id: "chat",
            agent_name: "test",
            delivery_scope: "test",
            thread_anchor: None,
        }
    }
    fn spawn(id: &str) -> AgentMessage {
        let mut ev = AgentMessage::of(AgentKind::ToolUse);
        ev.tool = Some("sub_agent_activity".into());
        ev.tool_input = Some(
            serde_json::json!({"kind":"started", "agentThreadId":id,"agentPath":format!("/root/{id}")}),
        );
        ev
    }
    fn child(kind: AgentKind, id: &str) -> AgentMessage {
        let mut ev = AgentMessage::of(kind);
        ev.parent_run_id = Some(RunId(id.into()));
        ev.tool_id = Some("shell-1".into());
        ev.tool = Some("shell".into());
        ev
    }
    fn codex_child_event(state: &mut HeadlessActivity, value: serde_json::Value) {
        let event = summon_core::codex_rpc::parse_notification(&value)
            .unwrap()
            .unwrap();
        crate::capture::rpc::forward_subagent_notification(&event, "worker", &mut |ev| {
            state.observe(ev)
        });
    }

    #[test]
    fn codex_card_keeps_only_the_latest_child_body() {
        let mut state = HeadlessActivity::default();
        state.observe(&spawn("worker"));
        for (item, delta) in [("old", "旧正文"), ("new", "新"), ("new", "正文")] {
            codex_child_event(
                &mut state,
                serde_json::json!({
                    "method":"item/agentMessage/delta", "params":{
                        "threadId":"worker", "turnId":"child-turn", "itemId":item, "delta":delta
                    }
                }),
            );
        }
        codex_child_event(
            &mut state,
            serde_json::json!({
                "method":"item/completed", "params":{
                    "threadId":"worker", "turnId":"child-turn", "item":{"id":"new", "type":"agentMessage", "text":"新正文"}
                }
            }),
        );
        codex_child_event(
            &mut state,
            serde_json::json!({
                "method":"item/completed", "params":{
                    "threadId":"worker", "turnId":"child-turn", "item":{"id":"old", "type":"agentMessage", "text":"旧正文补齐"}
                }
            }),
        );
        let mut thinking = child(AgentKind::Thinking, "worker");
        thinking.text = Some("private thinking".into());
        state.observe(&thinking);
        let mut tool = child(AgentKind::ToolUse, "worker");
        tool.tool = Some("command_execution".into());
        tool.tool_input = Some(serde_json::json!({"command":"secret command"}));
        state.observe(&tool);
        assert!(state.has_tools());
        let card = render(&state, false).unwrap();
        assert!(card.contains("新正文"), "{card}");
        for hidden in [
            "旧正文",
            "private thinking",
            "command\\_execution",
            "secret command",
            "执行",
        ] {
            assert!(!card.contains(hidden), "{hidden}: {card}");
        }
    }

    #[test]
    fn child_body_streams_keep_spaces_and_are_isolated_by_child() {
        let mut state = HeadlessActivity::default();
        for child in ["one", "two"] {
            state.observe(&spawn(child));
            for delta in [child, " ", "report"] {
                let event = summon_core::codex_rpc::parse_notification(&serde_json::json!({
                    "method":"item/agentMessage/delta", "params": {
                        "threadId":child, "turnId":"turn", "itemId":"same-item", "delta":delta
                    }
                }))
                .unwrap()
                .unwrap();
                crate::capture::rpc::forward_subagent_notification(&event, child, &mut |ev| {
                    state.observe(ev)
                });
            }
        }
        let card = render(&state, false).unwrap();
        assert!(
            card.contains("one report") && card.contains("two report"),
            "{card}"
        );
        assert_eq!(state.children().len(), 2);
    }

    #[test]
    fn final_assistant_body_is_kept_when_the_child_finishes() {
        let mut state = HeadlessActivity::default();
        state.observe(&spawn("worker"));
        let mut body = child(AgentKind::Assistant, "worker");
        body.text = Some("最新结论".into());
        body.stop_reason = Some("end_turn".into());
        state.observe(&body);
        let card = render(&state, true).unwrap();
        assert!(card.contains("已完成"));
        assert!(card.contains("最新结论"));
    }

    #[test]
    fn cursor_card_reads_latest_assistant_message_from_task_result() {
        let mut rows = cursor_fixture();
        rows[1]["tool_call"]["taskToolCall"]["result"]["success"]["conversationSteps"] = serde_json::json!([
            {"assistantMessage":{"text":"旧报告"}},
            {"thinkingMessage":{"text":"private thinking"}},
            {"toolCall":{"output":"private tool output"}},
            {"assistantMessage":{"text":"最新报告"}}
        ]);
        let mut state = HeadlessActivity::default();
        observe_cursor(&mut state, &rows[0]);
        observe_cursor(&mut state, &rows[1]);
        let card = render(&state, true).unwrap();
        assert!(card.contains("最新报告"), "{card}");
        for hidden in ["旧报告", "private thinking", "private tool output"] {
            assert!(!card.contains(hidden));
        }
    }

    #[test]
    fn one_card_updates_and_freezes_without_marking_unfinished_children_completed() {
        let front = Fake::default();
        let mut state = HeadlessActivity::default();
        let mut card = SubagentCard::new(true);
        let now = Instant::now();
        card.update(&state, &front, &chat(), now);
        assert!(front.calls.borrow().is_empty());
        state.observe(&spawn("worker"));
        card.update(&state, &front, &chat(), now);
        state.observe(&child(AgentKind::ToolUse, "worker"));
        let mut report = child(AgentKind::Assistant, "worker");
        report.text = Some("正在核对正文".into());
        state.observe(&report);
        card.update(&state, &front, &chat(), now + Duration::from_secs(1));
        assert_eq!(front.calls.borrow().len(), 1);
        card.update(&state, &front, &chat(), now + Duration::from_secs(2));
        card.update(&state, &front, &chat(), now + Duration::from_secs(4));
        assert_eq!(front.calls.borrow().len(), 2);
        card.finish(&state, &front, &chat());
        let calls = front.calls.borrow();
        assert_eq!(
            calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            vec!["begin", "patch", "freeze"]
        );
        assert!(calls[1].1.contains("正在核对正文"));
        assert!(!calls[1].1.contains("shell"));
        assert!(calls[2].1.contains("状态待确认"));
        assert!(!calls[2].1.contains("已完成"));
    }
    #[test]
    fn failed_or_throttled_patch_retries_latest_snapshot_but_creation_is_once() {
        let front = Fake::default();
        let mut state = HeadlessActivity::default();
        let mut card = SubagentCard::new(true);
        let now = Instant::now();
        state.observe(&spawn("worker"));
        card.update(&state, &front, &chat(), now);
        state.observe(&child(AgentKind::ToolUse, "worker"));
        let mut report = child(AgentKind::Assistant, "worker");
        report.text = Some("最新进展".into());
        state.observe(&report);
        front.throttle.set(true);
        card.update(&state, &front, &chat(), now + Duration::from_secs(2));
        front.fail_patch.set(true);
        card.update(&state, &front, &chat(), now + Duration::from_secs(4));
        let mut failed = child(AgentKind::Result, "worker");
        failed.status = Some(TurnStatus::Failed);
        state.observe(&failed);
        card.update(&state, &front, &chat(), now + Duration::from_secs(6));
        assert!(front.calls.borrow().last().unwrap().1.contains("失败"));
        assert_eq!(
            front
                .calls
                .borrow()
                .iter()
                .filter(|c| c.0 == "begin")
                .count(),
            1
        );
        let front = Fake::default();
        front.fail_begin.set(true);
        let mut card = SubagentCard::new(true);
        card.update(&state, &front, &chat(), now);
        card.update(&state, &front, &chat(), now + Duration::from_secs(9));
        card.finish(&state, &front, &chat());
        assert_eq!(front.calls.borrow().len(), 1);
    }
    #[test]
    fn hidden_and_result_only_turns_send_no_summary_card() {
        let mut state = HeadlessActivity::default();
        state.observe(&spawn("worker"));
        for (enabled, result_only) in [(false, false), (true, true)] {
            let front = Fake {
                result_only,
                ..Default::default()
            };
            let mut card = SubagentCard::new(enabled);
            card.update(&state, &front, &chat(), Instant::now());
            card.finish(&state, &front, &chat());
            assert!(front.calls.borrow().is_empty());
        }
    }
    #[test]
    fn bounded_rows_prioritize_running_children_and_escape_markup() {
        let mut state = HeadlessActivity::default();
        for i in 0..20 {
            let key = format!("worker{i}");
            state.observe(&spawn(&key));
            state.observe(&child(AgentKind::Result, &key));
        }
        state.observe(&spawn("<at user_id=all>everyone</at>"));
        let card = render(&state, false).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert_eq!(parsed["schema"], "2.0");
        assert!(card.contains("另有 9 个"));
        assert!(!card.contains("<at "));
        assert!(card.len() < 10000);
        let mut state = HeadlessActivity::default();
        state.observe(&child(AgentKind::Result, "worker"));
        let card = render(&state, true).unwrap();
        assert!(card.contains("已完成"));
        assert!(!card.contains("状态待确认"));
    }
    #[test]
    fn rpc_terminal_notifications_reach_the_card_with_their_actual_status() {
        for (status, error, expected) in [
            ("completed", serde_json::Value::Null, "已完成"),
            ("interrupted", serde_json::Value::Null, "已中断"),
            ("failed", serde_json::Value::Null, "失败"),
            (
                "failed",
                serde_json::json!({"message":"provider failed"}),
                "失败",
            ),
        ] {
            let mut state = HeadlessActivity::default();
            state.observe(&spawn("worker"));
            let notification=summon_core::codex_rpc::parse_notification(&serde_json::json!({"method":"turn/completed","params":{"threadId":"worker","turn":{"id":"turn-child","status":status,"error":error}}})).unwrap().unwrap();
            crate::capture::rpc::forward_subagent_notification(
                &notification,
                "worker",
                &mut |ev| state.observe(ev),
            );
            let mut complete = spawn("worker");
            complete.tool_input.as_mut().unwrap()["kind"] = serde_json::json!("completed");
            state.observe(&complete);
            assert!(!state.has_subagents());
            let card = render(&state, true).unwrap();
            assert!(card.contains(expected), "{status}: {card}");
            if expected != "已完成" {
                assert!(!card.contains("已完成"));
            }
        }
    }

    #[test]
    fn anonymous_tool_liveness_is_preserved_without_rendering_tool_names() {
        let mut state = HeadlessActivity::default();
        state.observe(&spawn("worker"));
        let mut first = child(AgentKind::ToolUse, "worker");
        first.tool_id = None;
        state.observe(&first);
        let mut second = first.clone();
        second.tool = Some("read".into());
        state.observe(&second);
        let mut done = child(AgentKind::ToolResult, "worker");
        done.tool_id = None;
        state.observe(&done);
        assert!(state.has_tools());
        let card = render(&state, false).unwrap();
        assert!(card.contains("暂无正文"));
        assert!(!card.contains("read") && !card.contains("shell"));
        state.observe(&done);
        assert!(!state.has_tools());
        assert_eq!(render(&state, false).unwrap(), card);
    }

    fn cursor_fixture() -> Vec<serde_json::Value> {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cursor_subagent.jsonl"
        ))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
    }

    fn observe_cursor(state: &mut HeadlessActivity, row: &serde_json::Value) {
        for ev in summon_core::stream_json::parse_stream_json_line(&row.to_string()) {
            state.observe(&ev);
        }
    }

    #[test]
    fn cursor_task_live_fixture_drives_card_to_completion() {
        let rows = cursor_fixture();
        assert_ne!(
            rows[0]["tool_call"]["taskToolCall"]["args"]["agentId"],
            rows[1]["tool_call"]["taskToolCall"]["result"]["success"]["agentId"]
        );
        let mut state = HeadlessActivity::default();
        let fake = Fake::default();
        let mut card = SubagentCard::new(true);
        let now = Instant::now();
        observe_cursor(&mut state, &rows[0]);
        assert!(state.has_tools() && state.has_subagents());
        card.update(&state, &fake, &chat(), now);
        assert!(render(&state, false).unwrap().contains("运行中"));
        observe_cursor(&mut state, &rows[1]);
        assert!(!state.has_tools() && !state.has_subagents());
        card.update(&state, &fake, &chat(), now + Duration::from_secs(3));
        card.finish(&state, &fake, &chat());
        let calls = fake.calls.borrow();
        assert_eq!(
            calls.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["begin", "patch", "freeze"]
        );
        assert!(calls.last().unwrap().1.contains("已完成"));
        assert!(calls.last().unwrap().1.contains("probe-value"));
    }

    #[test]
    fn cursor_task_background_and_unknown_results_keep_child_pending() {
        for result in [
            serde_json::json!({"success":{"isBackground":true}}),
            serde_json::json!({"success":{}}),
            serde_json::json!({}),
        ] {
            let mut rows = cursor_fixture();
            rows[1]["tool_call"]["taskToolCall"]["result"] = result;
            let mut state = HeadlessActivity::default();
            observe_cursor(&mut state, &rows[0]);
            observe_cursor(&mut state, &rows[1]);
            assert!(!state.has_tools());
            assert!(state.has_subagents());
            let card = render(&state, true).unwrap();
            assert!(card.contains("状态待确认"));
            assert!(!card.contains("已完成"));
        }
    }

    #[test]
    fn cursor_task_failure_and_parallel_calls_are_isolated() {
        let mut rows = cursor_fixture();
        let mut other = rows[0].clone();
        other["call_id"] = serde_json::json!("other-call");
        let mut state = HeadlessActivity::default();
        observe_cursor(&mut state, &rows[0]);
        observe_cursor(&mut state, &other);
        rows[1]["tool_call"]["taskToolCall"]["result"] =
            serde_json::json!({"failure":{"error":"test"}});
        observe_cursor(&mut state, &rows[1]);
        assert!(state.has_tools() && state.has_subagents());
        let card = render(&state, false).unwrap();
        assert!(card.contains("失败") && card.contains("运行中"));
        assert_eq!(state.children().len(), 2);
        let mut unknown = rows[1].clone();
        unknown["call_id"] = serde_json::json!("unknown-call");
        observe_cursor(&mut state, &unknown);
        assert_eq!(state.children().len(), 2);
        assert!(state.has_subagents());
    }
}
