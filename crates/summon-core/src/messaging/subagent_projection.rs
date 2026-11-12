//! 子 agent 在 CoT 里的投影状态(纯逻辑)。
//!
//! 主 agent 的派出 / 等待步骤给出子 agent 的标题、任务说明与「主 agent 在等」;子 agent 自己的
//! 事件记进 `TurnProgress` 的子 agent 账本。主轮的投影与主轮结束后的 CoT 跟随线程共用这一份:
//! 两边各写一份,判据迟早会漂。时刻由调用方传入,状态源由调用方读好再给。

use crate::messaging::progress::{ToolCallInput, TurnProgress};
use crate::messaging::segment::tool_title;
use crate::messaging::tool_detail::{
    cot_result_language, tool_command_detail, tool_description, tool_result_summary,
};
use crate::parse::agent_msg::{is_turn_end, AgentKind, AgentMessage};
use crate::subagent::SubagentState;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// 工具入参里的一个非空字符串字段(已去首尾空白)。
fn tool_input_str<'a>(input: Option<&'a serde_json::Value>, key: &str) -> Option<&'a str> {
    input
        .and_then(|input| input.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// 工具入参里的开关。claude 2.1.268 起 `run_in_background` 是字符串 `"true"`,布尔与字符串
/// 两种形态都认;认不出就是没给。
fn tool_input_flag(input: Option<&serde_json::Value>, key: &str) -> Option<bool> {
    match input?.get(key)? {
        serde_json::Value::Bool(flag) => Some(*flag),
        serde_json::Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// 子 agent 是怎么派出去的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launch {
    /// 调用在飞、还不知道同步还是异步:结果到达前主 agent 算在等,结果若不是「已派到后台」
    /// 的回执,那就是子 agent 跑完了。
    Pending,
    /// 在后台跑(显式 `run_in_background`、派出回执、codex 子线程):调用结果不是它的结束。
    Background,
}

#[derive(Debug, Clone)]
struct SubagentMeta {
    title: String,
    description: Option<String>,
    launch: Launch,
}

/// 一轮里子 agent 的投影状态:谁派的、叫什么、主 agent 在不在等、谁已经结束。
#[derive(Debug, Default)]
pub struct SubagentProjection {
    /// 子 agent 的标题与任务说明,从主 agent 派它的那一步里取(claude 的 `Agent`/`Task`
    /// 调用 id、codex 的子线程 id 为键);子 agent 第一次出声时据此在 CoT 里开段。
    meta: HashMap<String, SubagentMeta>,
    /// 主 agent 此刻在等的那几步(派出结果未到的 `Agent`/`Task`、阻塞的 `TaskOutput`、
    /// codex `wait`)。非空才允许子 agent 在 CoT 里实时推,否则主 agent 的行会和它交错。
    waiting_steps: HashSet<String>,
    /// 每个子 agent 最近一段还没推的文字。子 agent 的最后一段是给主 agent 的报告(已是父级
    /// 工具结果),所以一段文字要等到它后面又有动静才算中间过程。
    held_text: HashMap<String, String>,
    /// 子 agent 工具的开始时刻,按 (子 agent, provider 工具 id) 记,结束时给真实耗时。
    tool_started_at: HashMap<(String, String), Instant>,
    /// 已收过完成快照的 (子 agent, provider 内容 item)。
    completed_items: HashSet<(String, String)>,
    /// 已知结束的子 agent,不论账本里有没有它那一段:结束信号可能先于它的行被读到,
    /// 之后开段时照此标结束。
    finished: HashSet<String>,
    /// 子 agent 的 agentId。状态源里结束的条目不再带派出调用 id,只能按它认人。
    agent_ids: HashMap<String, String>,
}

impl SubagentProjection {
    /// 主 agent 的一步工具开始:派子 agent 就记下标题与说明,并维护「主 agent 在等」。
    pub fn note_main_tool(&mut self, progress: &mut TurnProgress, tool: &str, ev: &AgentMessage) {
        let input = ev.tool_input.as_ref();
        let step_id = ev.tool_id.as_deref().filter(|id| !id.is_empty());
        match tool {
            "Agent" | "Task" => {
                let Some(step_id) = step_id else { return };
                let background = tool_input_flag(input, "run_in_background") == Some(true);
                self.meta.insert(
                    step_id.to_string(),
                    SubagentMeta {
                        title: tool_title(tool, input),
                        description: tool_description(input).map(str::to_string),
                        launch: if background {
                            Launch::Background
                        } else {
                            Launch::Pending
                        },
                    },
                );
                if !background {
                    self.waiting_steps.insert(step_id.to_string());
                }
            }
            // 阻塞取某个后台任务的输出(`block` 缺省即阻塞)。等的不一定是子 agent,但这段时间
            // 主 agent 不会写别的,子 agent 的行不会和它交错。
            "TaskOutput" => {
                let Some(step_id) = step_id else { return };
                if tool_input_flag(input, "block") != Some(false) {
                    self.waiting_steps.insert(step_id.to_string());
                }
            }
            "sub_agent_activity" => self.note_codex_activity(progress, ev),
            "collab_agent_tool_call" if tool_input_str(input, "tool") == Some("wait") => {
                if let Some(step_id) = step_id {
                    self.waiting_steps.insert(step_id.to_string());
                }
            }
            _ => return,
        }
        progress.set_parent_waiting(!self.waiting_steps.is_empty());
    }

    /// 主 agent 的一步工具结束。只有同步派出的调用,结果才是子 agent 的结束;「已派到后台」
    /// 的回执与 `TaskOutput` 的结果都不是。
    pub fn note_main_result(&mut self, progress: &mut TurnProgress, ev: &AgentMessage) {
        let Some(step_id) = ev.tool_id.as_deref() else {
            return;
        };
        let was_waiting = self.waiting_steps.remove(step_id);
        if let Some(meta) = self.meta.get_mut(step_id) {
            match (ev.async_launch.as_ref(), meta.launch) {
                (Some(launch), _) => {
                    meta.launch = Launch::Background;
                    if let Some(agent_id) = launch.agent_id.as_deref() {
                        self.agent_ids
                            .insert(step_id.to_string(), agent_id.to_string());
                    }
                }
                (None, Launch::Pending) => self.finish(progress, step_id),
                // 派出本身失败:这个子 agent 再也不会有动静。
                (None, Launch::Background) if ev.tool_is_error => self.finish(progress, step_id),
                (None, Launch::Background) => {}
            }
        }
        if was_waiting {
            progress.set_parent_waiting(!self.waiting_steps.is_empty());
        }
    }

    /// codex 的 `subAgentActivity`:`started` 记下被派线程的标题(取 `agentPath` 末段),
    /// `completed` / `interrupted` 收掉它。主线程与子线程(派孙线程)上都走这一条。
    fn note_codex_activity(&mut self, progress: &mut TurnProgress, ev: &AgentMessage) {
        let text = |key: &str| tool_input_str(ev.tool_input.as_ref(), key);
        let Some(thread) = text("agentThreadId") else {
            return;
        };
        match text("kind") {
            Some("started") => {
                let title = text("agentPath")
                    .and_then(|path| path.trim_end_matches('/').rsplit('/').next())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("sub_agent")
                    .to_string();
                self.meta.entry(thread.to_string()).or_insert(SubagentMeta {
                    title,
                    description: None,
                    launch: Launch::Background,
                });
            }
            Some("completed" | "interrupted") => self.finish(progress, thread),
            _ => {}
        }
    }

    fn finish(&mut self, progress: &mut TurnProgress, key: &str) {
        self.held_text.remove(key);
        self.finished.insert(key.to_string());
        progress.subagent_finished(key);
    }

    /// 子 agent 的一条事件记进账本。第一次出声时按主 agent 派它时的标题开段。
    /// 返回账本有没有变化(调用方据此决定要不要刷)。
    pub fn project(
        &mut self,
        progress: &mut TurnProgress,
        key: &str,
        ev: &AgentMessage,
        now: Instant,
    ) -> bool {
        let (title, description) = match self.meta.get(key) {
            Some(meta) => (meta.title.clone(), meta.description.clone()),
            None => ("sub_agent".to_string(), None),
        };
        progress.subagent_started(key, &title, description.as_deref());
        if self.finished.contains(key) {
            progress.subagent_finished(key);
        }
        match ev.kind {
            AgentKind::ToolUse => {
                let Some(tool) = ev.tool.as_deref() else {
                    return false;
                };
                self.release_held_text(progress, key);
                if tool == "sub_agent_activity" {
                    self.note_codex_activity(progress, ev);
                }
                let provider_id = ev.tool_id.as_deref().unwrap_or("");
                self.tool_started_at
                    .insert((key.to_string(), provider_id.to_string()), now);
                let input = ev.tool_input.as_ref();
                let command = tool_command_detail(input);
                let plain_label = tool_title(tool, input);
                let mut call = ToolCallInput::new(&plain_label, tool);
                if let Some(command) = command {
                    call = call.command(command);
                }
                if let Some(language) = cot_result_language(input, command) {
                    call = call.language(language);
                }
                if let Some(description) = tool_description(input) {
                    call = call.description(description);
                }
                progress.subagent_tool_started(key, provider_id, call);
            }
            AgentKind::ToolResult => {
                let mut summary = tool_result_summary(ev.tool_result.as_ref());
                if ev.tool_is_error {
                    summary = Some(summon_i18n::format!(
                        "r4f3e4b6e78",
                        summary
                            .as_deref()
                            .unwrap_or(summon_i18n::text!("rbd8394d9a2"))
                    ));
                }
                let provider_id = ev.tool_id.as_deref().unwrap_or("");
                let secs = self
                    .tool_started_at
                    .remove(&(key.to_string(), provider_id.to_string()))
                    .map_or(0, |started| {
                        now.saturating_duration_since(started).as_secs()
                    });
                progress.subagent_tool_finished(key, provider_id, secs, summary.as_deref());
            }
            AgentKind::Thinking => {
                let Some(text) = ev.text.as_deref() else {
                    return false;
                };
                if !self.is_new_complete_thinking(key, ev) {
                    return false;
                }
                self.release_held_text(progress, key);
                progress.subagent_thinking(key, text);
            }
            AgentKind::Assistant => {
                if is_turn_end(ev.stop_reason.as_deref()) {
                    self.finish(progress, key);
                } else if !ev.partial {
                    let Some(text) = ev.text.as_deref().filter(|t| !t.trim().is_empty()) else {
                        return false;
                    };
                    self.release_held_text(progress, key);
                    self.held_text.insert(key.to_string(), text.to_string());
                    return false;
                } else {
                    return false;
                }
            }
            AgentKind::Result => self.finish(progress, key),
            _ => return false,
        }
        true
    }

    /// 子 agent 的思考只收完整的一段,流式增量不单独成段。claude 转录的思考块本身就是完整的
    /// (没有流来源、不是增量);codex reasoning 的完成快照与增量共用段 key、也标着 `partial`,
    /// 所以按流来源认,同一个 item 的完成快照只收一次。
    fn is_new_complete_thinking(&mut self, key: &str, ev: &AgentMessage) -> bool {
        match ev.stream_source {
            None => !ev.partial,
            Some(source) if source.is_completed() => match ev.source_item_id.as_deref() {
                Some(item) => self
                    .completed_items
                    .insert((key.to_string(), item.to_string())),
                None => true,
            },
            Some(_) => false,
        }
    }

    fn release_held_text(&mut self, progress: &mut TurnProgress, key: &str) {
        if let Some(text) = self.held_text.remove(key) {
            progress.subagent_thinking(key, &text);
        }
    }

    /// 记下子 agent 的 agentId(子转录 meta 文件名里的那一个)。
    pub fn note_agent_id(&mut self, key: &str, agent_id: &str) {
        self.agent_ids
            .entry(key.to_string())
            .or_insert_with(|| agent_id.to_string());
    }

    /// 按状态源对账:还没结束的子 agent 里,状态源说结束了的就标结束。认不出的不下结论。
    /// 返回有没有子 agent 因此结束。
    pub fn reconcile(&mut self, progress: &mut TurnProgress, state: &SubagentState) -> bool {
        let mut changed = false;
        for key in self.open_keys(progress) {
            if let Some(agent_id) = state.agent_id_for(&key) {
                self.note_agent_id(&key, agent_id);
            }
            let agent_id = self.agent_ids.get(&key).map(String::as_str);
            if state.finished_for(&key, agent_id) == Some(true) {
                self.finish(progress, &key);
                changed = true;
            }
        }
        changed
    }

    /// 还没结束的子 agent:账本里没结束的段,加上已派到后台、还没出过声的。
    fn open_keys(&self, progress: &TurnProgress) -> Vec<String> {
        let mut keys: Vec<String> = progress
            .subagent_lanes()
            .iter()
            .filter(|lane| !lane.finished())
            .map(|lane| lane.key().to_string())
            .collect();
        for (key, meta) in &self.meta {
            if meta.launch == Launch::Background
                && !self.finished.contains(key)
                && !keys.contains(key)
            {
                keys.push(key.clone());
            }
        }
        keys
    }

    pub fn has_unfinished(&self, progress: &TurnProgress) -> bool {
        self.unfinished_count(progress) > 0
    }

    pub fn unfinished_count(&self, progress: &TurnProgress) -> usize {
        self.open_keys(progress).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::progress::SubagentItem;
    use crate::parse::agent_msg::RunId;
    use std::time::Duration;

    fn sub_command(parent: &str, id: &str, command: &str) -> AgentMessage {
        let mut msg = AgentMessage::of(AgentKind::ToolUse);
        msg.parent_run_id = Some(RunId(parent.to_string()));
        msg.tool = Some("Bash".into());
        msg.tool_id = Some(id.into());
        msg.tool_input = Some(serde_json::json!({ "command": command }));
        msg
    }

    fn sub_result(parent: &str, id: &str) -> AgentMessage {
        let mut msg = AgentMessage::of(AgentKind::ToolResult);
        msg.parent_run_id = Some(RunId(parent.to_string()));
        msg.tool_id = Some(id.into());
        msg
    }

    /// 子 agent 工具的耗时按 (lane, provider id) 各自计:两个 lane 的 provider id 撞了也互不影响。
    #[test]
    fn subagent_tool_duration_is_measured_from_its_own_start() {
        let mut projection = SubagentProjection::default();
        let mut progress = TurnProgress::new().with_tool_details(true);
        let start = Instant::now();
        projection.project(
            &mut progress,
            "lane_a",
            &sub_command("lane_a", "t1", "sleep 7"),
            start,
        );
        projection.project(
            &mut progress,
            "lane_b",
            &sub_command("lane_b", "t1", "sleep 3"),
            start + Duration::from_secs(4),
        );
        let end = start + Duration::from_secs(7);
        for lane in ["lane_b", "lane_a"] {
            projection.project(&mut progress, lane, &sub_result(lane, "t1"), end);
        }
        let secs: Vec<(String, u64, bool)> = progress
            .subagent_lanes()
            .iter()
            .flat_map(|lane| {
                lane.items().iter().filter_map(move |item| match item {
                    SubagentItem::Tool(tool) => {
                        Some((lane.key().to_string(), tool.secs, tool.done))
                    }
                    SubagentItem::Thinking(_) => None,
                })
            })
            .collect();
        assert_eq!(
            secs,
            [
                ("lane_a".to_string(), 7, true),
                ("lane_b".to_string(), 3, true)
            ]
        );
    }
}
