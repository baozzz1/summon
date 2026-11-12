//! Shell 补全共用 CLI 定义，目标候选来自会话快照。

use std::ffi::OsStr;

use clap::CommandFactory;
use clap_complete::{CompleteEnv, CompletionCandidate};
use summon_core::config::Config;

pub(super) fn complete() {
    CompleteEnv::with_factory(super::cli::Cli::command)
        .var("SUMMON_COMPLETE")
        .complete();
}

pub(super) fn targets(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let cfg = Config::load();
    summon_core::tools::configure(&cfg);
    let Ok(rows) = super::session::snapshot_rows(&cfg) else {
        return Vec::new();
    };
    candidates(&rows, prefix)
}

fn candidates(rows: &[super::session::Row], prefix: &str) -> Vec<CompletionCandidate> {
    let values: std::collections::BTreeSet<_> = rows
        .iter()
        .filter(|row| row.target.starts_with(prefix))
        .map(|row| row.target.as_str())
        .collect();
    values.into_iter().map(CompletionCandidate::new).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::engine::{complete, ArgValueCompleter};

    fn values(args: &[&str], command: &mut clap::Command) -> Vec<String> {
        complete(
            command,
            args.iter().map(Into::into).collect(),
            args.len() - 1,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|candidate| candidate.get_value().to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn command_and_option_completion_follow_public_cli() {
        let mut command = super::super::cli::Cli::command();
        let root = values(&["summon", "sess"], &mut command);
        assert!(root.contains(&"session".into()));
        let session = values(&["summon", "session", ""], &mut command);
        for name in ["read", "exit", "archive"] {
            assert!(session.contains(&name.into()), "{session:?}");
        }
        assert!(!session.contains(&"end".into()));
        assert!(session.iter().any(|name| name == "ls" || name == "list"));
        assert!(values(&["summon", "session", "ls"], &mut command).contains(&"ls".into()));
        assert!(
            values(&["summon", "session", "ls", "--w"], &mut command).contains(&"--watch".into())
        );
    }

    #[test]
    fn each_session_control_uses_live_target_completion() {
        for action in ["read", "exit", "archive"] {
            let mut command = super::super::cli::Cli::command();
            let session = command.find_subcommand_mut("session").unwrap();
            let control = session.find_subcommand_mut(action).unwrap();
            let configured = control
                .get_arguments()
                .find(|arg| arg.get_id() == "target")
                .unwrap();
            assert!(configured.get::<ArgValueCompleter>().is_some());
            *control = control.clone().mut_arg("target", |arg| {
                arg.add(ArgValueCompleter::new(|current: &OsStr| {
                    ["a/g1", "a/g2", "b/g1"]
                        .into_iter()
                        .filter(|value| value.starts_with(current.to_str().unwrap_or("\0")))
                        .map(CompletionCandidate::new)
                        .collect::<Vec<_>>()
                }))
            });
            assert_eq!(
                values(&["summon", "session", action, "a/"], &mut command),
                ["a/g1", "a/g2"]
            );
        }
    }

    #[test]
    fn target_candidates_follow_headless_runtime_visibility_and_fallback() {
        use summon_core::fleet::{build_fleet, LiveSession, RuntimeStatus};
        use summon_core::registry::CaptureMode;
        use summon_core::session_view::{Liveness, SessionView};
        let views = ["g1", "g2"]
            .into_iter()
            .map(|tag| SessionView {
                agent: "a".into(),
                tag: tag.into(),
                session: format!("soul-a-{tag}"),
                chat_id: Some(format!("oc_{tag}")),
                chat_type: Some("group".into()),
                chat_name: None,
                capture: CaptureMode::Rpc,
                liveness: Liveness::RecentHeadless,
                last_active: None,
            })
            .collect();
        let sessions = [("a".into(), views)];
        let mut runtime = RuntimeStatus {
            pid: 1,
            version: None,
            uptime_secs: 0,
            busy: vec![],
            credentials: vec![],
            live_headless: Some(vec![LiveSession {
                agent: "a".into(),
                tag: "g1".into(),
            }]),
            subagents: vec![],
        };
        let current = |runtime: Option<&RuntimeStatus>| {
            let snapshot = build_fleet(&sessions, runtime, std::time::SystemTime::now());
            let rows = super::super::session::rows(&snapshot);
            candidates(&rows, "a/")
                .into_iter()
                .map(|candidate| candidate.get_value().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(current(Some(&runtime)), ["a/g1"]);
        runtime.live_headless = Some(vec![]);
        assert!(current(Some(&runtime)).is_empty());
        runtime.live_headless = None;
        assert_eq!(current(Some(&runtime)), ["a/g1", "a/g2"]);
        assert_eq!(current(None), ["a/g1", "a/g2"]);
    }

    #[test]
    fn target_candidates_are_canonical_sorted_unique_and_prefix_filtered() {
        let row = |target: &str| super::super::session::Row {
            comment_task: None,
            target: target.into(),
            agent: "agent".into(),
            tag: "g1".into(),
            chat: "群名".into(),
            state: super::super::session::State::Idle,
            busy: false,
            tmux_idle: false,
            age: "1m".into(),
            actions: vec![],
            disabled: vec![],
            reply_message_id: None,
            session: Some("soul-agent-g1".into()),
            chat_id: Some("oc_group".into()),
        };
        let rows = [row("b/g1"), row("a/g2"), row("a/g1"), row("a/g1")];
        let values = |prefix| {
            candidates(&rows, prefix)
                .into_iter()
                .map(|candidate| candidate.get_value().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(values(""), ["a/g1", "a/g2", "b/g1"]);
        assert_eq!(values("a/"), ["a/g1", "a/g2"]);
        for fragment in ["g1", "oc_group", "soul-agent", "missing"] {
            assert!(values(fragment).is_empty());
        }
    }
}
