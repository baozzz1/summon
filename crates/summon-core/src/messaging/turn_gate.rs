//! 一轮过程展示的设置门控(纯件):事件流里哪些进 CoT、以什么形态进,工具起止怎么记进进度账本。
//!
//! chat 线程的投影器(`TurnProjector::project_event`)与控制台的飞书效果预览共用这一份。预览用
//! 示例事件走同一套门控与账本,拿到的过程就是 summon 此刻会发出去的那条;两边若各写一份,开关
//! 的效果迟早对不上(spec「飞书效果预览」反目标)。
//!
//! 这里只放判定与账本写法;卡片刷新、日志、节流、提问切段这些副作用留在调用方。

use std::collections::{HashMap, HashSet};

use crate::agent_msg::{AgentKind, AgentMessage};
use crate::config::settings::ResolvedSettings;
use crate::config::Messaging;
use crate::progress::TurnProgress;
use crate::segment::{SegKind, TurnSegment};
use crate::subagent_projection::SubagentProjection;
use crate::tool_detail::{
    cot_result_language, tool_command_detail, tool_description, tool_result_summary,
    tool_target_path,
};

/// 一轮内与过程展示有关的设置快照。快照边界是一轮(同 [`ResolvedSettings`]):轮中途改了开关,
/// 下一轮才看得到。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateSettings {
    /// CoT 过程内容是否外显(`cot.visible`)。关时只留运行状态。
    pub cot_visible: bool,
    /// 思考正文不上 CoT(`cot.thinking` 关,即 `messaging.hide_thinking`)。
    pub hide_thinking: bool,
    /// CoT 是否展示工具命令、参数与结果(`cot.tools`)。
    pub tool_details: bool,
    /// 思考可见时也渲染工具轨迹(`messaging.tool_trail`)。
    pub tool_trail: bool,
    /// 轨迹保留的最大步数(`messaging.progress_max_steps`)。
    pub progress_max_steps: usize,
    /// 子 agent 的过程只在汇总卡里展示(codex / cursor 的 CLI 特性,不是开关)。
    pub child_summary_only: bool,
}

impl GateSettings {
    /// 按前三层叠好的 [`Messaging`] 与本会话的开关快照取值。与 chat 线程两条流式入口取设置的
    /// 写法一一对应;`child_summary_only` 随 CLI 定,由 [`Self::with_child_summary_only`] 设。
    pub fn from(messaging: &Messaging, settings: &ResolvedSettings) -> Self {
        Self {
            cot_visible: settings.cot_visible(),
            hide_thinking: settings.hide_thinking(),
            tool_details: settings.cot_tool_details(),
            tool_trail: messaging.tool_trail,
            progress_max_steps: messaging.progress_max_steps,
            child_summary_only: false,
        }
    }

    /// 从已建好的进度账本读回它承载的那几项(账本是这几项的唯一持有者);CoT 外显默认开。
    pub fn of_progress(progress: &TurnProgress) -> Self {
        Self {
            cot_visible: true,
            hide_thinking: progress.hide_thinking(),
            tool_details: progress.tool_details(),
            tool_trail: progress.tool_trail(),
            progress_max_steps: progress.max_steps(),
            child_summary_only: false,
        }
    }

    pub fn with_cot_visible(mut self, visible: bool) -> Self {
        self.cot_visible = visible;
        self
    }

    pub fn with_child_summary_only(mut self, summary_only: bool) -> Self {
        self.child_summary_only = summary_only;
        self
    }

    /// 本轮进度账本的起点(不含 fence token,由调用方补)。
    pub fn progress(&self) -> TurnProgress {
        TurnProgress::new()
            .with_max_steps(self.progress_max_steps)
            .with_hide_thinking(self.hide_thinking)
            .with_tool_trail(self.tool_trail)
            .with_tool_details(self.tool_details)
    }
}

/// CoT 标题用的任务摘要:CoT 关时不带(标题退回「处理请求」),免得过程不外显却把用户原话挂在
/// 过程消息上。
pub fn cot_task_summary(cot_visible: bool, hint: Option<&str>) -> Option<&str> {
    cot_visible.then_some(hint).flatten()
}

/// 一条事件按本轮设置该怎么投。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gated {
    /// 子 agent 的事件,投进 CoT 的子 agent 段。
    Subagent,
    /// 子 agent 的事件,本轮不展示(CoT 关,或子 agent 过程只进汇总卡)。
    SubagentHidden,
    /// CoT 关:不投内容,只刷运行状态。
    StatusOnly,
    /// 工具开始。没有工具名的事件也落在这里:调用方照样要关上一段回复,只是不记账本。
    ToolStart,
    /// 工具结束。
    ToolEnd,
    /// 思考正文不上 CoT:只在账本上留时长痕。
    HiddenThinking,
    /// 正文进段。
    Content(SegKind),
    /// 不投的事件:没有正文的内容块与其它种类。
    Skip,
}

/// 门控判定。顺序与 chat 线程的投影一致:先认子 agent(它们不碰主轮任何状态),再看 CoT 总开关,
/// 最后按事件种类分。
pub fn gate_event(settings: &GateSettings, ev: &AgentMessage) -> Gated {
    if ev.parent_run_id.is_some() {
        return if settings.cot_visible && !settings.child_summary_only {
            Gated::Subagent
        } else {
            Gated::SubagentHidden
        };
    }
    if !settings.cot_visible {
        return Gated::StatusOnly;
    }
    match ev.kind {
        AgentKind::ToolUse => Gated::ToolStart,
        AgentKind::ToolResult => Gated::ToolEnd,
        AgentKind::Thinking if ev.text.is_some() => {
            if settings.hide_thinking {
                Gated::HiddenThinking
            } else {
                Gated::Content(SegKind::Thinking)
            }
        }
        AgentKind::Assistant if ev.text.is_some() => Gated::Content(SegKind::Reply),
        _ => Gated::Skip,
    }
}

/// 派子 agent / 等子 agent 的工具。子 agent 过程只进汇总卡时,它们的返回里可能嵌着整段子对话,
/// 结果正文不进 CoT。
pub fn is_child_call_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Agent" | "Task" | "task" | "TaskOutput" | "sub_agent_activity" | "wait"
    )
}

/// 放行界可以前移到哪:回复段后面跟上了别的动作才算完结;末段是回复且还没确认完结
/// (`trailing_done` 为假)时扣住它 —— CoT 只追加、撤不回,它可能就是终态回复。
/// 返回 `None` = 不前移。
pub fn release_end(
    segments: &[TurnSegment],
    released: usize,
    trailing_done: bool,
) -> Option<usize> {
    let end = match segments.last() {
        Some(TurnSegment::Reply(_)) if !trailing_done => segments.len() - 1,
        _ => segments.len(),
    };
    (end > released).then_some(end)
}

/// 一次工具开始记进账本后,调用方打日志要的两项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolStarted<'a> {
    pub tool: &'a str,
    pub command: Option<&'a str>,
}

/// 主 agent 工具起止的配对状态:provider 的调用 id → 账本步号,以及子 agent 派出调用。
/// 每轮一份,不跨轮。
#[derive(Debug, Default)]
pub struct ToolLedger {
    provider_tools: HashMap<String, u64>,
    child_calls: HashSet<String>,
}

impl ToolLedger {
    /// 工具开始:按入参算出账本上的字(有 description 用它,否则有命令显命令,再否则
    /// 「友好名 + 目标文件名」),记一步。没有工具名的事件不记,返回 `None`。
    pub fn start<'e>(
        &mut self,
        settings: &GateSettings,
        progress: &mut TurnProgress,
        subagents: &mut SubagentProjection,
        ev: &'e AgentMessage,
    ) -> Option<ToolStarted<'e>> {
        let tool = ev.tool.as_deref()?;
        let input = ev.tool_input.as_ref();
        let command = tool_command_detail(input);
        let description = tool_description(input);
        let label = match description {
            Some(desc) => crate::segment::clip_tool_label(desc),
            None => match command {
                Some(_) => crate::segment::tool_running_label(tool, command),
                None => crate::segment::tool_target_label(tool, tool_target_path(input)),
            },
        };
        // CoT 标题另取 `plain_label`:英文工具名 + 对象,不含 description。
        let plain_label = crate::segment::tool_title(tool, input);
        let step_id = progress.tool_started_with_description(
            &label,
            &plain_label,
            tool,
            command,
            cot_result_language(input, command),
            description,
        );
        if let Some(provider_id) = ev.tool_id.as_deref().filter(|id| !id.is_empty()) {
            self.provider_tools.insert(provider_id.to_string(), step_id);
            if settings.child_summary_only && is_child_call_tool(tool) {
                self.child_calls.insert(provider_id.to_string());
            }
        }
        if !settings.child_summary_only {
            subagents.note_main_tool(progress, tool, ev);
        }
        Some(ToolStarted { tool, command })
    }

    /// 工具结束:结果摘要(失败加前缀)按调用 id 挂回它那一步,配不上就挂最后一个在跑的。
    /// `elapsed_secs`:距上一事件多久,近似这一步的耗时。
    pub fn finish(
        &mut self,
        settings: &GateSettings,
        progress: &mut TurnProgress,
        subagents: &mut SubagentProjection,
        ev: &AgentMessage,
        elapsed_secs: u64,
    ) {
        let known_child_call = ev
            .tool_id
            .as_deref()
            .is_some_and(|id| self.child_calls.remove(id));
        let child_result = settings.child_summary_only
            && (known_child_call || ev.tool.as_deref().is_some_and(is_child_call_tool));
        let mut summary = (!child_result)
            .then(|| tool_result_summary(ev.tool_result.as_ref()))
            .flatten();
        if ev.tool_is_error {
            summary = Some(summon_i18n::format!(
                "r4f3e4b6e78",
                summary
                    .as_deref()
                    .unwrap_or(summon_i18n::text!("rbd8394d9a2"))
            ));
        }
        if !settings.child_summary_only {
            subagents.note_main_result(progress, ev);
        }
        match ev
            .tool_id
            .as_deref()
            .and_then(|provider_id| self.provider_tools.remove(provider_id))
        {
            Some(step_id) => {
                progress.tool_finished_by_id(step_id, elapsed_secs, summary.as_deref())
            }
            None => progress.tool_finished_with_result(elapsed_secs, summary.as_deref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_msg::RunId;
    use crate::config::builtin_messaging;
    use crate::config::settings::{ChatOverrides, SettingKey};

    fn settings_with(chat: &[(SettingKey, bool)]) -> GateSettings {
        let messaging = builtin_messaging();
        let mut overrides = ChatOverrides::default();
        for (id, value) in chat {
            overrides.set(*id, *value);
        }
        GateSettings::from(
            &messaging,
            &ResolvedSettings::resolve(&messaging, &overrides),
        )
    }

    fn event(kind: AgentKind, text: Option<&str>) -> AgentMessage {
        let mut ev = AgentMessage::of(kind);
        ev.text = text.map(str::to_string);
        ev
    }

    #[test]
    fn from_reads_the_resolved_switches_and_the_messaging_layer() {
        let gate = settings_with(&[
            (SettingKey::CotVisible, false),
            (SettingKey::CotThinking, false),
            (SettingKey::CotToolDetails, true),
        ]);
        let messaging = builtin_messaging();
        assert!(!gate.cot_visible);
        assert!(gate.hide_thinking, "思考开关关 = 隐藏思考");
        assert!(gate.tool_details);
        assert_eq!(gate.tool_trail, messaging.tool_trail);
        assert_eq!(gate.progress_max_steps, messaging.progress_max_steps);
        assert!(!gate.child_summary_only);
    }

    #[test]
    fn progress_round_trips_through_the_ledger() {
        let gate = GateSettings {
            cot_visible: true,
            hide_thinking: true,
            tool_details: false,
            tool_trail: true,
            progress_max_steps: 7,
            child_summary_only: false,
        };
        assert_eq!(GateSettings::of_progress(&gate.progress()), gate);
    }

    #[test]
    fn subagent_events_are_gated_before_anything_else() {
        let mut ev = event(AgentKind::Thinking, Some("子 agent 在想"));
        ev.parent_run_id = Some(RunId("toolu_1".into()));
        let shown = settings_with(&[]);
        assert_eq!(gate_event(&shown, &ev), Gated::Subagent);
        let hidden = settings_with(&[(SettingKey::CotVisible, false)]);
        assert_eq!(gate_event(&hidden, &ev), Gated::SubagentHidden);
        let summary_only = shown.with_child_summary_only(true);
        assert_eq!(gate_event(&summary_only, &ev), Gated::SubagentHidden);
    }

    #[test]
    fn hidden_cot_turns_every_main_event_into_a_status_refresh() {
        let gate = settings_with(&[(SettingKey::CotVisible, false)]);
        for kind in [
            AgentKind::Thinking,
            AgentKind::Assistant,
            AgentKind::ToolUse,
            AgentKind::ToolResult,
        ] {
            assert_eq!(
                gate_event(&gate, &event(kind, Some("x"))),
                Gated::StatusOnly
            );
        }
    }

    #[test]
    fn hidden_thinking_is_not_content_but_replies_still_are() {
        let gate = settings_with(&[(SettingKey::CotThinking, false)]);
        assert_eq!(
            gate_event(&gate, &event(AgentKind::Thinking, Some("想一想"))),
            Gated::HiddenThinking
        );
        assert_eq!(
            gate_event(&gate, &event(AgentKind::Assistant, Some("答复"))),
            Gated::Content(SegKind::Reply)
        );
        let shown = settings_with(&[]);
        assert_eq!(
            gate_event(&shown, &event(AgentKind::Thinking, Some("想一想"))),
            Gated::Content(SegKind::Thinking)
        );
    }

    #[test]
    fn content_without_text_and_other_kinds_are_skipped() {
        let gate = settings_with(&[]);
        assert_eq!(
            gate_event(&gate, &event(AgentKind::Thinking, None)),
            Gated::Skip
        );
        assert_eq!(
            gate_event(&gate, &event(AgentKind::Assistant, None)),
            Gated::Skip
        );
        assert_eq!(
            gate_event(&gate, &event(AgentKind::ToolUse, None)),
            Gated::ToolStart,
            "没有工具名的工具开始仍要关上一段回复"
        );
    }

    #[test]
    fn task_summary_is_dropped_when_cot_is_hidden() {
        assert_eq!(cot_task_summary(true, Some("看下 CI")), Some("看下 CI"));
        assert_eq!(cot_task_summary(false, Some("看下 CI")), None);
        assert_eq!(cot_task_summary(true, None), None);
    }

    #[test]
    fn release_end_holds_a_trailing_reply_until_it_is_done() {
        let segs = vec![
            TurnSegment::Thinking("想".into()),
            TurnSegment::Reply("答".into()),
        ];
        assert_eq!(release_end(&segs, 0, false), Some(1));
        assert_eq!(release_end(&segs, 0, true), Some(2));
        assert_eq!(release_end(&segs, 1, false), None);
        assert_eq!(release_end(&[], 0, true), None);
    }

    fn tool_use(tool: &str, id: &str, input: serde_json::Value) -> AgentMessage {
        let mut ev = AgentMessage::of(AgentKind::ToolUse);
        ev.tool = Some(tool.into());
        ev.tool_id = Some(id.into());
        ev.tool_input = Some(input);
        ev
    }

    fn tool_result(id: &str, result: &str) -> AgentMessage {
        let mut ev = AgentMessage::of(AgentKind::ToolResult);
        ev.tool_id = Some(id.into());
        ev.tool_result = Some(serde_json::Value::String(result.into()));
        ev
    }

    #[test]
    fn ledger_labels_commands_and_targets_and_pairs_results_by_id() {
        let gate = settings_with(&[(SettingKey::CotToolDetails, true)]);
        let mut progress = gate.progress();
        let mut subagents = SubagentProjection::default();
        let mut ledger = ToolLedger::default();
        let bash = tool_use("Bash", "c1", serde_json::json!({"command": "cargo test"}));
        let started = ledger
            .start(&gate, &mut progress, &mut subagents, &bash)
            .unwrap();
        assert_eq!(started.command, Some("cargo test"));
        let read = tool_use("Read", "c2", serde_json::json!({"file_path": "src/lib.rs"}));
        ledger.start(&gate, &mut progress, &mut subagents, &read);
        // 先到的是后开始那步的结果:按调用 id 配,不按顺序。
        ledger.finish(
            &gate,
            &mut progress,
            &mut subagents,
            &tool_result("c2", "fn main() {}"),
            0,
        );
        let tools = progress.protocol_tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].command.as_deref(), Some("cargo test"));
        assert!(!tools[0].done);
        assert!(tools[1].done);
        assert_eq!(tools[1].result.as_deref(), Some("fn main() {}"));
        assert_eq!(tools[1].language.as_deref(), Some("rust"));
    }

    #[test]
    fn ledger_skips_events_without_a_tool_name() {
        let gate = settings_with(&[]);
        let mut progress = gate.progress();
        let nameless = AgentMessage::of(AgentKind::ToolUse);
        let started = ToolLedger::default().start(
            &gate,
            &mut progress,
            &mut SubagentProjection::default(),
            &nameless,
        );
        assert!(started.is_none());
        assert!(progress.protocol_tools().is_empty());
    }

    #[test]
    fn child_call_results_stay_out_of_the_ledger_in_summary_only_mode() {
        let gate =
            settings_with(&[(SettingKey::CotToolDetails, true)]).with_child_summary_only(true);
        let mut progress = gate.progress();
        let mut subagents = SubagentProjection::default();
        let mut ledger = ToolLedger::default();
        ledger.start(
            &gate,
            &mut progress,
            &mut subagents,
            &tool_use("Task", "t1", serde_json::json!({"description": "查日志"})),
        );
        ledger.finish(
            &gate,
            &mut progress,
            &mut subagents,
            &tool_result("t1", "子对话全文"),
            0,
        );
        let tool = &progress.protocol_tools()[0];
        assert!(tool.done);
        assert_eq!(tool.result, None, "子 agent 的正文由汇总卡展示");
    }

    #[test]
    fn failed_tool_result_is_prefixed() {
        let gate = settings_with(&[(SettingKey::CotToolDetails, true)]);
        let mut progress = gate.progress();
        let mut subagents = SubagentProjection::default();
        let mut ledger = ToolLedger::default();
        ledger.start(
            &gate,
            &mut progress,
            &mut subagents,
            &tool_use("Bash", "c1", serde_json::json!({"command": "false"})),
        );
        let mut failed = tool_result("c1", "exit 1");
        failed.tool_is_error = true;
        ledger.finish(&gate, &mut progress, &mut subagents, &failed, 0);
        assert_eq!(
            progress.protocol_tools()[0].result.as_deref(),
            Some("失败：exit 1")
        );
    }
}
