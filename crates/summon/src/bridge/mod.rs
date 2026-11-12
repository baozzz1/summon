//! 外部 Claude 会话桥接的本机入口。
//!
//! Hook 进程只负责采样并写不可变 spool；daemon 后续以单写者归约事件，避免多个 Claude
//! 进程并发改同一份状态。

pub mod context;
pub mod drain;
pub mod handoff;
pub mod hook;
pub mod install;
pub mod process_probe;
pub mod recall;
pub mod recall_headless;
pub mod recall_resume;
pub mod recall_rpc;
pub mod reconcile;
pub mod runtime;
pub mod store;

pub const HOOK_PROTOCOL: &str = "v1";
pub const MANAGED_SESSION_ENV: &str = "SUMMON_MANAGED_SESSION";
pub const MANAGED_TAKEOVER_ENV: &str = "SUMMON_MANAGED_TAKEOVER";
pub const HANDOFF_TOKEN_ENV: &str = "SUMMON_BRIDGE_HANDOFF_TOKEN";
pub const HANDOFF_ATTEMPT_ENV: &str = "SUMMON_BRIDGE_HANDOFF_ATTEMPT";
