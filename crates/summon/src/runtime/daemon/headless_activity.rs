//! 本轮无头执行的工作状态；生命周期事件决定存活，展示配置不参与判断。

use std::collections::{BTreeMap, HashMap, HashSet};
use summon_core::agent_msg::{is_turn_end, AgentKind, AgentMessage, ContentStreamSource};
use summon_core::segment::{
    push_segment_with_source_item, PushOutcome, SegKind, SourceItemSegments, SourceItemUpdate,
    TurnSegment,
};

#[derive(Default)]
pub(super) struct HeadlessActivity {
    tools: HashSet<(String, String)>,
    anonymous_tools: Vec<String>,
    // 终态保留到本轮结束，用于识别子线程迟到的事件；显式 started 开始下一次执行。
    subagents: HashMap<String, ChildWork>,
    next_child: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

pub(super) struct ChildWork {
    pub(super) order: usize,
    pub(super) name: String,
    pub(super) status: ChildStatus,
    pub(super) updated: String,
    pub(super) tools: BTreeMap<String, String>,
    pub(super) anonymous_tools: Vec<String>,
    body_segments: Vec<TurnSegment>,
    body_sources: SourceItemSegments,
    pub(super) body_updated: Option<String>,
}

impl ChildWork {
    pub(super) fn latest_body(&self) -> Option<&str> {
        match self.body_segments.last() {
            Some(TurnSegment::Reply(text)) => Some(text),
            _ => None,
        }
    }

    fn observe_body(&mut self, ev: &AgentMessage, updated: &str) {
        let Some(text) = ev.text.as_deref() else {
            return;
        };
        let item = ev.source_item_id.as_deref().filter(|id| !id.is_empty());
        let update = match (ev.stream_source, item) {
            (Some(ContentStreamSource::CodexRpcDelta), Some(id)) => {
                Some(SourceItemUpdate::Delta(id))
            }
            (Some(ContentStreamSource::CodexRpcCompleted), Some(id)) => {
                Some(SourceItemUpdate::Completed(id))
            }
            _ => None,
        };
        let outcome = push_segment_with_source_item(
            &mut self.body_segments,
            &mut self.body_sources,
            SegKind::Reply,
            text,
            ev.partial,
            update,
        );
        if !matches!(
            outcome,
            PushOutcome::IgnoredEmpty | PushOutcome::IgnoredDuplicate
        ) {
            self.body_updated = Some(updated.into());
        }
    }
}

impl HeadlessActivity {
    pub(super) fn observe(&mut self, ev: &AgentMessage) {
        self.observe_at(ev, &chrono::Local::now().format("%H:%M:%S").to_string());
    }

    fn child(&mut self, key: &str, updated: &str) -> &mut ChildWork {
        self.subagents.entry(key.to_string()).or_insert_with(|| {
            self.next_child += 1;
            ChildWork {
                order: self.next_child,
                name: summon_i18n::format!("r10d172ceeb", self.next_child),
                status: ChildStatus::Running,
                updated: updated.into(),
                tools: BTreeMap::new(),
                anonymous_tools: Vec::new(),
                body_segments: Vec::new(),
                body_sources: SourceItemSegments::default(),
                body_updated: None,
            }
        })
    }

    fn observe_at(&mut self, ev: &AgentMessage, updated: &str) {
        let scope = ev.parent_run_id.as_ref().map_or("", |id| id.0.as_str());
        let id = ev.tool_id.as_deref().filter(|id| !id.is_empty());
        // Cursor 的调用 ID 跨 started/completed 稳定，args 与 result 的 agentId 可能不同。
        if ev.tool.as_deref() == Some("task") {
            if let Some(call) = id {
                let key = format!("cursor-task:{scope}:{call}");
                match ev.kind {
                    AgentKind::ToolUse => {
                        if let Some(agent) = ev
                            .tool_input
                            .as_ref()
                            .and_then(|v| v.get("agentId"))
                            .and_then(|v| v.as_str())
                            .filter(|v| !v.is_empty())
                        {
                            let work = self.child(&key, updated);
                            work.name = summon_i18n::format!(
                                "r94d37464f6",
                                agent
                                    .chars()
                                    .filter(|c| !c.is_control())
                                    .take(12)
                                    .collect::<String>()
                            );
                            work.updated = updated.into();
                        }
                    }
                    AgentKind::ToolResult if self.subagents.contains_key(&key) => {
                        // conversationSteps 中有类型的 assistantMessage 才是正文，其余工具返回不用于展示。
                        if let Some(text) = ev
                            .tool_result
                            .as_ref()
                            .and_then(|v| v.get("success"))
                            .and_then(|v| v.get("conversationSteps"))
                            .and_then(|v| v.as_array())
                            .and_then(|steps| {
                                steps.iter().rev().find_map(|step| {
                                    step.get("assistantMessage")
                                        .and_then(|v| v.get("text"))
                                        .and_then(|v| v.as_str())
                                })
                            })
                        {
                            let mut body = AgentMessage::of(AgentKind::Assistant);
                            body.text = Some(text.to_string());
                            self.child(&key, updated).observe_body(&body, updated);
                        }
                        if ev.tool_is_error {
                            self.finish_child(&key, ChildStatus::Failed, updated);
                        } else if ev
                            .tool_result
                            .as_ref()
                            .and_then(|v| v.get("success"))
                            .and_then(|v| v.get("isBackground"))
                            .and_then(|v| v.as_bool())
                            == Some(false)
                        {
                            self.finish_child(&key, ChildStatus::Completed, updated);
                        }
                    }
                    _ => {}
                }
            }
        }
        if ev.kind == AgentKind::ToolUse && ev.tool.as_deref() == Some("sub_agent_activity") {
            if let Some(input) = ev.tool_input.as_ref() {
                if let Some(child) = input
                    .get("agentThreadId")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    match input.get("kind").and_then(|v| v.as_str()) {
                        Some("started") => {
                            let work = self.child(child, updated);
                            if work.status != ChildStatus::Running {
                                work.body_segments.clear();
                                work.body_sources = SourceItemSegments::default();
                                work.body_updated = None;
                            }
                            work.status = ChildStatus::Running;
                            if let Some(name) = input
                                .get("agentPath")
                                .and_then(|v| v.as_str())
                                .and_then(|path| path.trim_end_matches('/').rsplit('/').next())
                                .filter(|s| !s.is_empty())
                            {
                                work.name =
                                    name.chars().filter(|c| !c.is_control()).take(80).collect();
                            }
                            work.updated = updated.into();
                        }
                        Some("completed") => {
                            self.finish_child(child, ChildStatus::Completed, updated)
                        }
                        Some("interrupted") => {
                            self.finish_child(child, ChildStatus::Interrupted, updated)
                        }
                        _ => {}
                    }
                }
            }
            return;
        }
        if !scope.is_empty() {
            if ev.kind == AgentKind::Assistant {
                let work = self.child(scope, updated);
                if work.status == ChildStatus::Running {
                    work.observe_body(ev, updated);
                }
            }
            if matches!(ev.kind, AgentKind::Result | AgentKind::Error)
                || (ev.kind == AgentKind::Assistant && is_turn_end(ev.stop_reason.as_deref()))
            {
                let status = if ev.kind == AgentKind::Error
                    || ev.error.is_some()
                    || matches!(ev.status, Some(summon_core::agent_msg::TurnStatus::Failed))
                {
                    ChildStatus::Failed
                } else if matches!(
                    ev.status,
                    Some(
                        summon_core::agent_msg::TurnStatus::Aborted
                            | summon_core::agent_msg::TurnStatus::Timeout
                    )
                ) {
                    ChildStatus::Interrupted
                } else {
                    ChildStatus::Completed
                };
                self.finish_child(scope, status, updated);
                return;
            }
            let work = self.child(scope, updated);
            if work.status != ChildStatus::Running {
                return;
            }
            work.updated = updated.into();
            match ev.kind {
                AgentKind::ToolUse => {
                    let name = ev
                        .tool
                        .as_deref()
                        .unwrap_or(summon_i18n::text!("r5ca6730d44"))
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(80)
                        .collect();
                    if let Some(id) = id {
                        work.tools.insert(id.into(), name);
                    } else {
                        work.anonymous_tools.push(name);
                    }
                }
                AgentKind::ToolResult => {
                    if let Some(id) = id {
                        work.tools.remove(id);
                    } else if !work.anonymous_tools.is_empty() {
                        work.anonymous_tools.remove(0);
                    }
                }
                _ => {}
            }
        }
        match ev.kind {
            AgentKind::ToolUse => {
                if let Some(id) = id {
                    self.tools.insert((scope.to_string(), id.to_string()));
                } else {
                    self.anonymous_tools.push(scope.to_string());
                }
            }
            AgentKind::ToolResult => {
                if let Some(id) = id {
                    self.tools.remove(&(scope.to_string(), id.to_string()));
                } else if let Some(index) = self.anonymous_tools.iter().position(|s| s == scope) {
                    self.anonymous_tools.remove(index);
                }
            }
            _ => {}
        }
    }

    fn finish_child(&mut self, scope: &str, status: ChildStatus, updated: &str) {
        let work = self.child(scope, updated);
        if status == ChildStatus::Failed
            || work.status == ChildStatus::Running
            || (work.status == ChildStatus::Completed && status == ChildStatus::Interrupted)
        {
            work.status = status;
        }
        work.updated = updated.into();
        work.tools.clear();
        work.anonymous_tools.clear();
        self.tools.retain(|(owner, _)| owner != scope);
        self.anonymous_tools.retain(|owner| owner != scope);
    }

    pub(super) fn children(&self) -> Vec<&ChildWork> {
        let mut children: Vec<_> = self.subagents.values().collect();
        children.sort_by_key(|work| (work.status != ChildStatus::Running, work.order));
        children
    }

    pub(super) fn has_tools(&self) -> bool {
        !self.tools.is_empty() || !self.anonymous_tools.is_empty()
    }

    pub(super) fn has_subagents(&self) -> bool {
        self.subagents
            .values()
            .any(|work| work.status == ChildStatus::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::agent_msg::RunId;

    fn tool(kind: AgentKind, scope: &str, id: &str) -> AgentMessage {
        let mut ev = AgentMessage::of(kind);
        ev.tool = Some("shell".into());
        ev.tool_id = (!id.is_empty()).then(|| id.into());
        ev.parent_run_id = (!scope.is_empty()).then(|| RunId(scope.into()));
        ev
    }

    fn activity(kind: &str, child: &str) -> AgentMessage {
        let mut ev = tool(AgentKind::ToolUse, "", "activity");
        ev.tool = Some("sub_agent_activity".into());
        ev.tool_input = Some(serde_json::json!({"kind": kind, "agentThreadId": child}));
        ev
    }

    #[test]
    fn parallel_tools_survive_text_and_out_of_order_results() {
        let mut state = HeadlessActivity::default();
        state.observe(&tool(AgentKind::ToolUse, "", "a"));
        state.observe(&tool(AgentKind::ToolUse, "", "b"));
        state.observe(&tool(AgentKind::ToolUse, "", "a"));
        state.observe(&AgentMessage::of(AgentKind::Thinking));
        state.observe(&tool(AgentKind::ToolResult, "", "b"));
        state.observe(&tool(AgentKind::ToolResult, "", "unknown"));
        assert!(state.has_tools());
        let mut failed = tool(AgentKind::ToolResult, "", "a");
        failed.tool_is_error = true;
        state.observe(&failed);
        assert!(!state.has_tools());
        assert!(!HeadlessActivity::default().has_tools());
    }

    #[test]
    fn tool_ids_are_scoped_and_anonymous_results_do_not_close_named_tools() {
        let mut state = HeadlessActivity::default();
        state.observe(&tool(AgentKind::ToolUse, "", "same"));
        state.observe(&tool(AgentKind::ToolUse, "child", "same"));
        state.observe(&tool(AgentKind::ToolResult, "child", "same"));
        state.observe(&tool(AgentKind::ToolResult, "", ""));
        assert!(state.has_tools());
        state.observe(&tool(AgentKind::ToolUse, "", ""));
        state.observe(&tool(AgentKind::ToolResult, "", "same"));
        assert!(state.has_tools());
        state.observe(&tool(AgentKind::ToolResult, "", ""));
        assert!(!state.has_tools());
    }

    #[test]
    fn silent_subagent_survives_dispatch_completion() {
        let mut state = HeadlessActivity::default();
        state.observe(&activity("started", "child"));
        state.observe(&tool(AgentKind::ToolResult, "", "activity"));
        assert!(!state.has_tools());
        assert!(state.has_subagents());
        state.observe(&activity("completed", "child"));
        state.observe(&tool(AgentKind::ToolResult, "", "activity"));
        assert!(!state.has_subagents());
        assert!(!state.has_tools());
    }

    #[test]
    fn child_failure_closes_its_tools_but_keeps_grandchild_running() {
        let mut state = HeadlessActivity::default();
        state.observe(&tool(AgentKind::ToolUse, "child", "shell"));
        let mut spawn = activity("started", "grandchild");
        spawn.parent_run_id = Some(RunId("child".into()));
        state.observe(&spawn);
        let mut end = AgentMessage::of(AgentKind::Result);
        end.parent_run_id = Some(RunId("child".into()));
        end.error = Some("failed".into());
        state.observe(&end);
        assert!(!state.has_tools());
        assert!(state.has_subagents());
        state.observe(&activity("interrupted", "grandchild"));
        state.observe(&tool(AgentKind::ToolResult, "", "activity"));
        assert!(!state.has_subagents());
    }
    #[test]
    fn cursor_shell_events_select_work_timeout_until_completion() {
        use crate::runtime::daemon::chat_thread::{headless_stall_bound, headless_stalled};
        use std::time::Duration;
        use summon_core::stream_json::parse_stream_json_line;
        let mut state = HeadlessActivity::default();
        let started = r#"{"type":"tool_call","subtype":"started","call_id":"shell-1","tool_call":{"shellToolCall":{"args":{"command":"sleep 300"}}}}"#;
        for ev in parse_stream_json_line(started) {
            state.observe(&ev);
        }
        let bound = headless_stall_bound(
            Duration::from_secs(240),
            state.has_tools() || state.has_subagents(),
            false,
        );
        assert_eq!(bound, Duration::from_secs(1800));
        assert!(!headless_stalled(
            Duration::from_secs(300),
            bound,
            true,
            false,
            false
        ));
        assert!(headless_stalled(
            Duration::from_secs(1800),
            bound,
            true,
            false,
            false
        ));
        let completed = r#"{"type":"tool_call","subtype":"completed","call_id":"shell-1","tool_call":{"shellToolCall":{"result":{"success":{"exitCode":0}}}}}"#;
        for ev in parse_stream_json_line(completed) {
            state.observe(&ev);
        }
        assert_eq!(
            headless_stall_bound(
                Duration::from_secs(240),
                state.has_tools() || state.has_subagents(),
                false
            ),
            Duration::from_secs(240)
        );
    }
    #[test]
    fn restarted_child_is_active() {
        let mut state = HeadlessActivity::default();
        state.observe(&activity("started", "child"));
        state.observe(&tool(AgentKind::ToolUse, "child", "first"));
        state.observe(&activity("completed", "child"));
        assert!(!state.has_subagents());
        state.observe(&activity("started", "child"));
        state.observe(&tool(AgentKind::ToolUse, "child", "second"));
        assert!(state.has_tools(), "new tool in restarted child is running");
        assert!(state.has_subagents(), "restarted child is running");
    }

    #[test]
    fn late_child_event_does_not_reopen_completed_child() {
        let mut state = HeadlessActivity::default();
        state.observe(&activity("started", "child"));
        state.observe(&activity("completed", "child"));
        assert!(!state.has_subagents());
        state.observe(&tool(AgentKind::ToolResult, "child", "done"));
        assert!(
            !state.has_subagents(),
            "late result cannot reopen completed child"
        );
    }
}
