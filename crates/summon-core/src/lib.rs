extern crate self as summon_core;

pub mod authz;
pub mod bridge;
pub mod codex_quota;
pub mod companion;
pub mod config;
pub mod cursor_quota;
pub mod dashboard;
pub mod driver;
pub mod file_preview;
pub mod i18n;
pub mod identity_cache;
pub mod install_stamp;
pub mod memory;
pub mod messaging;
pub mod observe;
pub mod parse;
pub mod peers;
pub mod port;
pub mod recall;
pub mod release;
pub mod remote;
pub mod remote_tools;
pub mod schedule;
pub mod sentinel;
pub mod session;
pub mod subagent;
pub mod tools;
pub mod web_terminal;
pub mod workflow;
pub mod yaml_edit;

// 顶层 re-export:跨 crate 消费方(summon)沿用 `summon_core::X` 路径,降低主题化下沉的 churn。
// crate 内部一律走真实主题路径(`crate::<theme>::X`),不靠这些别名。
pub use config::{
    agent_identity, autostart, codex_policy, menubar, model_effort, model_order, registry, settings,
};
pub use messaging::{
    cot, email_mask, feed_tag, feishu, fleet_card, handoff, image, inbound, ipc, progress, quote,
    reactions, reply_card, segment, subagent_projection, throttle, tool_detail, turn_gate,
    turn_notice,
};
pub use observe::{
    claude_usage, context_budget, credential_identity, event_log, export, fleet, history, insights,
    insights_analysis, log_view, pricing, record, turn_ledger, turn_usage, waiting,
};
pub use parse::{
    agent_msg, claude_hook, claude_log, claude_session_limit, claude_transcript, codex_rpc,
    cursor_acp, interactive, pane_input, pane_output, retry_wait, scrubber, stream_json, turn,
};
pub use peers::*;
pub use port::ids::MsgRef;
pub use port::{core_port, ids};
pub use recall::{
    resume_mode, ClaudeAgentEntry, DiskSessionEntry, RecallOwnership, RecallPage, RecallResumeMode,
    SessionCandidate,
};
pub use schedule::{
    due_plan, human_readable, interrupted_fire_notice, parse_when, schedule_id_of_fire, FirePlan,
    Kind, Schedule, ScheduleContext, ScheduleOutput, When,
};
pub use session::turn_anchor::{TurnAnchor, TurnAnchorCot, TurnAnchorCredential};
pub use session::{
    attach, chat_meta, cross_chat, exit, followup, fork, launch_env, outbox, pending, session_id,
    session_view, start_plan, work_dir,
};
pub use subagent::*;
