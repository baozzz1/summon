//! 面向终端的会话状态与动作规划；执行复用 Menu Bar 的控制入口。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use anyhow::{bail, Result};
use serde::Serialize;
use summon_core::fleet::{FleetSnapshot, VisibleState};

use super::cli::{MenubarAction, SessionAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum State {
    Leaked,
    Failed,
    Waiting,
    Working,
    Queued,
    Idle,
    Completed,
    Exited,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Leaked => "leaked",
            Self::Failed => "failed",
            Self::Queued => "queued",
            Self::Completed => "completed",
            Self::Waiting => "waiting",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Exited => "exited",
        }
    }

    fn from_visible(state: VisibleState, waiting: bool) -> Self {
        match state {
            VisibleState::Leaked => Self::Leaked,
            VisibleState::Working => Self::Working,
            VisibleState::Idle | VisibleState::Exited if waiting => Self::Waiting,
            VisibleState::Idle => Self::Idle,
            VisibleState::Exited => Self::Exited,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Row {
    pub target: String,
    pub agent: String,
    pub tag: String,
    pub chat: String,
    pub state: State,
    pub busy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_task: Option<summon_core::fleet::CommentTask>,
    pub tmux_idle: bool,
    pub age: String,
    pub actions: Vec<&'static str>,
    pub disabled: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
}

pub(super) fn rows(snapshot: &FleetSnapshot) -> Vec<Row> {
    let mut waiting = BTreeMap::new();
    for (index, mark) in snapshot.waiting.iter().enumerate() {
        waiting
            .entry((mark.alias.as_str(), mark.tag.as_str()))
            .or_insert((index, mark));
    }
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for group in &snapshot.groups {
        for member in &group.members {
            let target = format!("{}/{}", member.alias, group.tag);
            if !seen.insert(target.clone()) {
                continue;
            }
            let mark = waiting
                .get(&(member.alias.as_str(), group.tag.as_str()))
                .filter(|_| member.waiting)
                .map(|(_, mark)| *mark);
            let state = State::from_visible(member.state, mark.is_some());
            let mut actions = Vec::new();
            let mut disabled = Vec::new();
            if mark.is_some() {
                actions.push("read");
            }
            if group.chat_id.is_some() {
                actions.extend(["exit", "archive"]);
                if !snapshot.daemon.running {
                    disabled.extend(["exit", "archive"]);
                }
            }
            rows.push(Row {
                comment_task: None,
                target,
                agent: member.alias.clone(),
                tag: group.tag.clone(),
                chat: group.name.clone().unwrap_or_else(|| group.tag.clone()),
                state,
                busy: member.busy,
                tmux_idle: member.idle_tmux,
                age: if state == State::Waiting {
                    mark.map(|mark| mark.since_text.clone()).unwrap_or_default()
                } else {
                    member
                        .last_active_text
                        .clone()
                        .unwrap_or_else(|| summon_i18n::text!("r4d8c1c5b42").into())
                },
                actions,
                disabled,
                reply_message_id: mark.map(|mark| mark.reply_message_id.clone()),
                session: Some(member.target.clone()),
                chat_id: group.chat_id.clone(),
            });
        }
    }
    for mark in &snapshot.waiting {
        let target = format!("{}/{}", mark.alias, mark.tag);
        if !seen.insert(target.clone()) {
            continue;
        }
        rows.push(Row {
            comment_task: None,
            target,
            agent: mark.alias.clone(),
            tag: mark.tag.clone(),
            chat: mark.name.clone().unwrap_or_else(|| mark.tag.clone()),
            state: State::Waiting,
            busy: false,
            tmux_idle: false,
            age: mark.since_text.clone(),
            actions: vec!["read"],
            disabled: Vec::new(),
            reply_message_id: Some(mark.reply_message_id.clone()),
            session: None,
            chat_id: None,
        });
    }
    for task in &snapshot.comment_tasks {
        let state = match task.stage.as_str() {
            "failed" | "approval_failed" | "expired" | "sending" => State::Failed,
            "draft_ready" => State::Waiting,
            "closed" | "replied" => State::Completed,
            _ if task.busy => State::Working,
            _ => State::Queued,
        };
        rows.push(Row {
            target: format!("{}/comment-{}", task.agent, task.id),
            agent: task.agent.clone(),
            tag: format!("comment-{}", task.id),
            chat: task.title.clone(),
            state,
            busy: task.busy,
            tmux_idle: false,
            age: task.status.clone(),
            actions: Vec::new(),
            disabled: Vec::new(),
            reply_message_id: None,
            session: None,
            chat_id: None,
            comment_task: Some(task.clone()),
        });
    }
    rows.sort_by_key(|row| {
        (
            row.state,
            if row.state == State::Waiting {
                waiting
                    .get(&(row.agent.as_str(), row.tag.as_str()))
                    .map_or(usize::MAX, |(index, _)| *index)
            } else {
                usize::MAX
            },
        )
    });
    rows
}

pub(super) fn snapshot_rows(cfg: &summon_core::config::Config) -> Result<Vec<Row>> {
    let reg = super::cli::load_reg(cfg)?;
    let runtime = crate::observe::fleet_snapshot::fetch_runtime_status();
    let snapshot = crate::observe::fleet_snapshot::build(
        cfg,
        &reg,
        runtime.as_ref(),
        std::time::SystemTime::now(),
    );
    if let Some(error) = &snapshot.comment_tasks_error {
        eprintln!("{}", summon_i18n::format!("r41cf514c6b"));
    }
    Ok(rows(&snapshot))
}

pub(super) fn list(cfg: &summon_core::config::Config, json: bool, watch: bool) -> Result<()> {
    use std::io::IsTerminal;
    let mut stdout = std::io::stdout().lock();
    let terminal = watch && stdout.is_terminal();
    let result = list_with(
        &mut stdout,
        terminal,
        json,
        || snapshot_rows(cfg),
        || {
            if watch {
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            watch
        },
    );
    match result {
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe) =>
        {
            Ok(())
        }
        result => result,
    }
}

fn list_with(
    output: &mut impl std::io::Write,
    terminal: bool,
    json: bool,
    mut snapshot: impl FnMut() -> Result<Vec<Row>>,
    mut wait: impl FnMut() -> bool,
) -> Result<()> {
    loop {
        let rows = snapshot()?;
        if json {
            writeln!(output, "{}", serde_json::to_string(&rows)?)?;
        } else {
            if terminal {
                write!(output, "\x1b[H\x1b[2J")?;
            }
            write!(output, "{}", format_rows(&rows))?;
        }
        output.flush()?;
        if !wait() {
            return Ok(());
        }
    }
}

pub(super) fn format_rows(rows: &[Row]) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str(summon_i18n::text!("r678340b7e9"));
    } else {
        out.push_str(summon_i18n::text!("r640297aff6"));
        for row in rows {
            let chat = row.chat.replace(char::is_control, " ");
            if let Some(task) = &row.comment_task {
                let error = task
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .replace(char::is_control, " ");
                let pid = task.pid.map(|p| format!(" · PID {p}")).unwrap_or_default();
                let _ = writeln!(
                    out,
                    "{}  {} · {}{} {}",
                    row.target, task.status, chat, pid, error
                );
                continue;
            }
            let available: Vec<_> = row
                .actions
                .iter()
                .filter(|action| !row.disabled.contains(action))
                .copied()
                .collect();
            let _ = writeln!(
                out,
                "{:<35} {:<9} {} · {}  [{}]",
                row.target,
                row.state.label(),
                chat,
                row.age,
                available.join(" ")
            );
        }
    }
    out.push_str(summon_i18n::text!("rb428d893d6"));
    out
}

fn resolve<'a>(rows: &'a [Row], target: &str) -> Result<&'a Row> {
    let matches: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.target == target
                || row.agent == target
                || row.tag == target
                || row.session.as_deref() == Some(target)
                || row.chat_id.as_deref() == Some(target)
        })
        .collect();
    match matches.as_slice() {
        [row] => Ok(row),
        [] => bail!(summon_i18n::format!("r514a04d99b")),
        _ => bail!(summon_i18n::format!(
            "r03f22ca151",
            matches
                .iter()
                .map(|row| row.target.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

pub(super) fn menu_action(
    rows: &[Row],
    action: &SessionAction,
    json: bool,
) -> Result<MenubarAction> {
    match action {
        SessionAction::Read { target, reply } => {
            let row = resolve(rows, target)?;
            anyhow::ensure!(
                row.comment_task.is_none(),
                summon_i18n::format!("r773be0016e")
            );
            Ok(MenubarAction::Handled {
                agent: row.agent.clone(),
                tag: row.tag.clone(),
                reply: reply
                    .clone()
                    .or_else(|| row.reply_message_id.clone())
                    .unwrap_or_default(),
                json,
            })
        }
        SessionAction::Exit { target, interrupt }
        | SessionAction::Archive { target, interrupt } => {
            let row = resolve(rows, target)?;
            anyhow::ensure!(
                row.comment_task.is_none(),
                summon_i18n::format!("r773be0016e")
            );
            if row.chat_id.is_none() {
                bail!(summon_i18n::format!("r86fe8690e8", row.target));
            }
            Ok(MenubarAction::EndSession {
                agent: row.agent.clone(),
                tag: row.tag.clone(),
                archive: matches!(action, SessionAction::Archive { .. }),
                interrupt: *interrupt,
                json,
            })
        }
        _ => bail!(summon_i18n::format!("r6aabef7cba")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use serde_json::{json, Value};

    fn member(alias: &str, state: &str, waiting: bool) -> Value {
        json!({"alias":alias,"target":format!("soul-{alias}-ga1"),"busy":state=="working",
            "capture":"rpc","liveness":"live","last_active_secs":5,"last_active_text":"5秒前",
            "waiting":waiting,"state":state,"idle_tmux":false})
    }

    fn group(tag: &str, members: Vec<Value>) -> Value {
        json!({"tag":tag,"name":"开发群","chat_type":"group","chat_id":format!("oc_{tag}"),
            "busy":false,"agents":members.len(),"last_active_secs":5,"last_active_text":"5秒前",
            "state":"idle","idle_tmux":false,"members":members})
    }

    fn mark(alias: &str, tag: &str, reply: &str) -> Value {
        json!({"alias":alias,"tag":tag,"name":"待处理群","reply_message_id":reply,
            "since_secs":60,"since_text":"1分钟前","since_text_en":"1m ago","link":"lark://reply"})
    }

    fn snapshot(groups: Vec<Value>, waiting: Vec<Value>, running: bool) -> FleetSnapshot {
        serde_json::from_value(
            json!({"daemon":{"running":running},"agents":[],"groups":groups,"waiting":waiting}),
        )
        .unwrap()
    }

    #[test]
    fn comment_task_rows_show_failures_and_never_offer_session_actions() {
        let mut snap = snapshot(vec![], vec![], true);
        snap.comment_tasks.push(summon_core::fleet::CommentTask {
            id: "task".into(),
            agent: "writer".into(),
            title: "Document".into(),
            stage: "failed".into(),
            status: "处理失败".into(),
            status_en: "Failed".into(),
            busy: false,
            received_ms: 100,
            updated_ms: 200,
            pid: None,
            error: Some("创建者无法验证".into()),
        });
        let rows = rows(&snap);
        let row = &rows[0];
        assert!(row.actions.is_empty());
        assert_eq!(row.state, State::Failed);
        let text = format_rows(&rows);
        assert!(text.contains("创建者无法验证"));
        assert!(menu_action(
            &rows,
            &SessionAction::Exit {
                target: row.target.clone(),
                interrupt: true
            },
            false
        )
        .is_err());
        assert!(menu_action(
            &rows,
            &SessionAction::Read {
                target: row.target.clone(),
                reply: None
            },
            false
        )
        .is_err());
    }

    #[test]
    fn watch_reads_each_snapshot_and_emits_one_json_array_per_line() {
        let mut generation = 0;
        let mut waits = 0;
        let mut output = Vec::new();
        list_with(
            &mut output,
            true,
            true,
            || {
                let current = match generation {
                    0 => snapshot(
                        vec![group("ga1", vec![member("a", "working", false)])],
                        vec![],
                        true,
                    ),
                    1 => snapshot(
                        vec![group("ga1", vec![member("a", "idle", true)])],
                        vec![mark("a", "ga1", "new")],
                        true,
                    ),
                    2 => snapshot(vec![], vec![], false),
                    _ => panic!("退出后不再取快照"),
                };
                generation += 1;
                Ok(rows(&current))
            },
            || {
                waits += 1;
                waits < 3
            },
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains('\u{1b}'));
        let snapshots: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0][0]["state"], "working");
        assert_eq!(snapshots[1][0]["state"], "waiting");
        assert_eq!(snapshots[1][0]["reply_message_id"], "new");
        assert_eq!(snapshots[2], json!([]));
        assert_eq!(generation, 3);
    }

    #[test]
    fn watch_clears_only_human_terminals_and_stops_on_snapshot_errors() {
        for terminal in [false, true] {
            let mut calls = 0;
            let mut output = Vec::new();
            let error = list_with(
                &mut output,
                terminal,
                false,
                || {
                    calls += 1;
                    if calls == 2 {
                        bail!("快照读取失败");
                    }
                    Ok(vec![])
                },
                || true,
            )
            .unwrap_err();
            assert!(error.to_string().contains("快照读取失败"));
            assert_eq!(calls, 2);
            let text = String::from_utf8(output).unwrap();
            assert_eq!(
                text.matches("\u{1b}[H\u{1b}[2J").count(),
                usize::from(terminal)
            );
            assert!(text.contains("没有活跃会话"));
        }
        let mut command = crate::cli::cli::Cli::command();
        command.build();
        let parsed =
            crate::cli::cli::Cli::try_parse_from(["summon", "session", "ls", "--watch", "--json"])
                .unwrap();
        assert!(matches!(
            parsed.cmd,
            crate::cli::cli::Cmd::Session {
                action: Some(SessionAction::Ls { watch: true }),
                json: true
            }
        ));
        assert!(crate::cli::cli::Cli::try_parse_from([
            "summon", "session", "exit", "a/g1", "--watch"
        ])
        .is_err());
    }

    #[test]
    fn rows_match_menu_state_priority_and_keep_waiting_only_replies() {
        let snapshot = snapshot(
            vec![
                group("gworking", vec![member("busy", "working", true)]),
                group("gidle", vec![member("idle", "idle", false)]),
                group("gwaiting", vec![member("waiting", "idle", true)]),
                group("gleaked", vec![member("leaked", "leaked", true)]),
                group("gexited", vec![member("exited", "exited", false)]),
            ],
            vec![
                mark("orphan", "gorphan", "r0"),
                mark("busy", "gworking", "r1"),
                mark("waiting", "gwaiting", "r2"),
                mark("leaked", "gleaked", "r3"),
            ],
            true,
        );
        let rows = rows(&snapshot);
        assert_eq!(
            rows.iter().map(|row| row.state).collect::<Vec<_>>(),
            vec![
                State::Leaked,
                State::Waiting,
                State::Waiting,
                State::Working,
                State::Idle,
                State::Exited
            ]
        );
        assert_eq!(rows[1].target, "orphan/gorphan");
        assert_eq!(rows[1].actions, vec!["read"]);
        assert_eq!(rows[2].age, "1分钟前");
        assert_eq!(rows[3].age, "5秒前");
        assert!(rows[3].actions.contains(&"read"));
        assert!(menu_action(
            &rows,
            &SessionAction::Exit {
                target: "orphan/gorphan".into(),
                interrupt: false
            },
            false
        )
        .is_err());
        let wire = serde_json::to_value(&rows).unwrap();
        assert_eq!(wire[1]["state"], "waiting");
        assert!(wire[1].get("session").is_none());
        assert!(format_rows(&rows).contains("leaked/gleaked"));
    }

    #[test]
    fn duplicate_rows_and_stale_waiting_flags_do_not_duplicate_sessions() {
        let snapshot = snapshot(
            vec![group(
                "ga1",
                vec![member("a", "idle", false), member("a", "idle", true)],
            )],
            vec![mark("a", "ga1", "r1"), mark("a", "ga1", "r2")],
            false,
        );
        let rows = rows(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, State::Idle);
        assert!(rows[0].reply_message_id.is_none());
        assert_eq!(rows[0].disabled, vec!["exit", "archive"]);
        assert!(!format_rows(&rows).contains("[exit archive]"));
    }

    #[test]
    fn selectors_require_a_unique_target_and_never_accept_path_fragments() {
        let snapshot = snapshot(
            vec![group(
                "ga1",
                vec![member("a", "idle", false), member("b", "idle", false)],
            )],
            vec![],
            true,
        );
        let rows = rows(&snapshot);
        for target in ["ga1", "oc_ga1"] {
            let error = resolve(&rows, target).unwrap_err().to_string();
            assert!(error.contains("a/ga1") && error.contains("b/ga1"));
        }
        for target in ["a/ga1", "soul-a-ga1", "a"] {
            assert_eq!(resolve(&rows, target).unwrap().agent, "a");
        }
        assert!(resolve(&rows, "a/../../threads").is_err());
        assert!(resolve(&rows, "1").is_err());
    }

    #[test]
    fn read_plans_preserve_new_replies_with_the_existing_waiting_store() {
        use crate::runtime::waiting_store::{self, HandledOutcome};
        let snapshot = snapshot(vec![], vec![mark("a", "ga1", "old")], true);
        let rows = rows(&snapshot);
        let action = menu_action(
            &rows,
            &SessionAction::Read {
                target: "a/ga1".into(),
                reply: None,
            },
            true,
        )
        .unwrap();
        let MenubarAction::Handled {
            agent,
            tag,
            reply,
            json,
        } = action
        else {
            panic!("应复用已读入口")
        };
        assert_eq!(
            (agent.as_str(), tag.as_str(), reply.as_str(), json),
            ("a", "ga1", "old", true)
        );
        let dir = tempfile::tempdir().unwrap();
        let newer = summon_core::waiting::WaitingMark {
            chat_id: "oc_ga1".into(),
            reply_message_id: "new".into(),
            link: "link".into(),
            since_unix: 2,
        };
        waiting_store::set(dir.path(), &tag, &newer).unwrap();
        assert_eq!(
            waiting_store::clear_if_reply(dir.path(), &tag, &reply).unwrap(),
            HandledOutcome::Superseded
        );
        assert_eq!(waiting_store::list(dir.path())[0].1.reply_message_id, "new");
        let explicit = menu_action(
            &rows,
            &SessionAction::Read {
                target: "a/ga1".into(),
                reply: Some("new".into()),
            },
            false,
        )
        .unwrap();
        let MenubarAction::Handled { reply, .. } = explicit else {
            panic!("应保留指定回复")
        };
        assert_eq!(
            waiting_store::clear_if_reply(dir.path(), &tag, &reply).unwrap(),
            HandledOutcome::Cleared
        );
    }

    #[test]
    fn exit_and_archive_keep_the_daemon_busy_guard_and_intent() {
        use crate::runtime::menubar_session::{end_session, EndSessionOutcome, EndSessionPort};
        use std::cell::{Cell, RefCell};
        use std::time::{Duration, Instant};
        use summon_core::core_port::{Actor, Command, CommandOutcome, Intent};
        use summon_core::fleet::{BusyTurn, RuntimeStatus};
        struct Port {
            listed: Cell<bool>,
            submitted: RefCell<Vec<Command>>,
        }
        impl EndSessionPort for Port {
            fn listed(&self) -> Option<Option<String>> {
                self.listed.get().then(|| Some("oc_ga1".into()))
            }
            fn runtime(&self) -> Option<RuntimeStatus> {
                Some(RuntimeStatus {
                    pid: 1,
                    version: None,
                    uptime_secs: 1,
                    busy: vec![BusyTurn {
                        agent: "a".into(),
                        tag: "ga1".into(),
                    }],
                    credentials: vec![],
                    live_headless: Some(vec![]),
                    subagents: vec![],
                })
            }
            fn submit(&self, cmd: &Command) -> Result<CommandOutcome> {
                self.submitted.borrow_mut().push(cmd.clone());
                self.listed.set(false);
                Ok(CommandOutcome::Done)
            }
            fn sleep(&self, _: Duration) {
                panic!("已消失的会话立即结束等待")
            }
            fn now(&self) -> Instant {
                Instant::now()
            }
        }
        let snapshot = snapshot(
            vec![group("ga1", vec![member("a", "working", false)])],
            vec![],
            true,
        );
        let rows = rows(&snapshot);
        let port = Port {
            listed: Cell::new(true),
            submitted: RefCell::new(vec![]),
        };
        for (action, expected) in [
            (
                SessionAction::Exit {
                    target: "a/ga1".into(),
                    interrupt: false,
                },
                EndSessionOutcome::Running,
            ),
            (
                SessionAction::Archive {
                    target: "a/ga1".into(),
                    interrupt: true,
                },
                EndSessionOutcome::Ended,
            ),
        ] {
            let plan = menu_action(&rows, &action, true).unwrap();
            let MenubarAction::EndSession {
                agent,
                tag,
                archive,
                interrupt,
                json,
            } = plan
            else {
                panic!("必须复用 Menu Bar 结束入口")
            };
            assert!(json);
            assert_eq!(
                end_session(&port, &agent, &tag, archive, interrupt).unwrap(),
                expected
            );
            if expected == EndSessionOutcome::Running {
                assert!(port.submitted.borrow().is_empty());
            }
        }
        let commands = port.submitted.borrow();
        assert_eq!(commands.len(), 1);
        assert!(
            matches!(&commands[0], Command {actor:Actor::Owner,intent:Intent::EndChatSession {alias,chat_id,archive:true}} if alias=="a" && chat_id.as_str()=="oc_ga1")
        );
    }

    #[test]
    fn public_session_help_is_compact_and_agent_end_keeps_its_credential() {
        use super::super::cli::{Cli, Cmd};
        assert!(matches!(
            Cli::try_parse_from(["summon", "session"]).unwrap().cmd,
            Cmd::Session {
                action: None,
                json: false
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["summon", "session", "ls", "--json"])
                .unwrap()
                .cmd,
            Cmd::Session {
                action: Some(SessionAction::Ls { watch: false }),
                json: true
            }
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "summon",
                "session",
                "archive",
                "a/ga1",
                "--interrupt",
                "--json"
            ])
            .unwrap()
            .cmd,
            Cmd::Session {
                action: Some(SessionAction::Archive {
                    interrupt: true,
                    ..
                }),
                json: true
            }
        ));
        assert!(Cli::try_parse_from(["summon", "session", "end"]).is_err());
        assert!(
            matches!(Cli::try_parse_from(["summon","session","end","--turn-id","current"]).unwrap().cmd,Cmd::Session {action:Some(SessionAction::End {turn_id}),..} if turn_id=="current")
        );
        let mut root = Cli::command();
        let session = root.find_subcommand_mut("session").unwrap();
        let help = session.render_long_help().to_string();
        for name in ["ls", "read", "exit", "archive"] {
            assert!(session.find_subcommand(name).is_some());
        }
        assert!(session.find_subcommand("end").unwrap().is_hide_set());
        assert!(help.contains("autostart"));
        assert!(!help
            .lines()
            .any(|line| line.trim_start().starts_with("end ")));
        assert!(format_rows(&[]).contains("没有活跃会话"));
    }

    #[test]
    fn login_switch_aliases_keep_optional_keep_alive_overrides() {
        use super::super::cli::{AutostartAction, Cli, Cmd};
        for verb in ["enable", "on"] {
            assert!(matches!(
                Cli::try_parse_from(["summon", "autostart", verb])
                    .unwrap()
                    .cmd,
                Cmd::Autostart {
                    action: AutostartAction::Enable { keep_alive: None }
                }
            ));
            assert!(matches!(
                Cli::try_parse_from(["summon", "autostart", verb, "--keep-alive"])
                    .unwrap()
                    .cmd,
                Cmd::Autostart {
                    action: AutostartAction::Enable {
                        keep_alive: Some(true)
                    }
                }
            ));
            assert!(matches!(
                Cli::try_parse_from(["summon", "autostart", verb, "--keep-alive=false"])
                    .unwrap()
                    .cmd,
                Cmd::Autostart {
                    action: AutostartAction::Enable {
                        keep_alive: Some(false)
                    }
                }
            ));
        }
        for verb in ["disable", "off"] {
            assert!(matches!(
                Cli::try_parse_from(["summon", "autostart", verb])
                    .unwrap()
                    .cmd,
                Cmd::Autostart {
                    action: AutostartAction::Disable
                }
            ));
        }
    }
}
