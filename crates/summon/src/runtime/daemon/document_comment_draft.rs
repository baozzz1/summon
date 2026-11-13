//! 文档 owner bot 的受限起草进程，backend、模型与凭据来自该 agent 的配置。

use crate::frontend::client::FeishuClient;
use crate::runtime::document_comment_reader::ReaderServer;
use anyhow::{Context, Result};
use serde_json::json;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use summon_core::driver::CliDriver;
use summon_core::registry::Agent;

use crate::runtime::document_comments::{read_only_context_prompt, Record};
use crate::runtime::executor::ChildKiller;

const MAX_OUTPUT: u64 = 128 * 1024;

fn claude_args(agent: &Agent, reader: &ReaderServer) -> Result<Vec<String>> {
    let mut args = [
        "-p",
        "--output-format",
        "json",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--setting-sources",
        "",
        "--settings",
        "{\"disableAllHooks\":true}",
        "--no-session-persistence",
        "--permission-mode",
        "dontAsk",
    ]
    .map(str::to_string)
    .to_vec();
    args[7] = reader.claude_config()?;
    args.extend([
        "--allowedTools".into(),
        format!("mcp__{}__read_document", reader.name),
    ]);
    let effective = crate::runtime::chat_model::effective(agent, None);
    let (model_args, _) =
        CliDriver::Claude.resolve_launch_lenient(effective.model(), effective.effort());
    args.extend(model_args);
    Ok(args)
}

fn completed_text(raw: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context(summon_i18n::text!("re5c0aaaa1a"))?;
    anyhow::ensure!(
        value["type"] == "result" && value["subtype"] == "success" && value["is_error"] == false,
        summon_i18n::format!("r2f859f4f77")
    );
    let text = value["result"]
        .as_str()
        .context(summon_i18n::text!("r1d5525d6c4"))?
        .trim();
    anyhow::ensure!(
        !text.is_empty() && text.chars().count() <= 8_000,
        summon_i18n::format!("rd96592ea9b")
    );
    Ok(text.to_string())
}

/// 一次性起草 / 分析进程的环境:系统基本变量加上该 agent 自己的环境,去掉 summon 自己管的键与别的
/// CLI 的凭据。洞察分析用的是同一套。
pub(crate) fn backend_env(agent: &Agent, driver: CliDriver) -> Vec<(String, String)> {
    let mut env = super::essential_system_env();
    for (key, value) in super::resolve_extra_env(agent) {
        if summon_core::agent_identity::is_runtime_owned_env_key(&key) {
            continue;
        }
        let foreign = match driver {
            CliDriver::Claude => ["OPENAI_", "CODEX_", "CURSOR_", "TRAE_"]
                .iter()
                .any(|prefix| key.starts_with(prefix)),
            CliDriver::Codex => ["ANTHROPIC_", "CLAUDE_", "CURSOR_", "TRAE_"]
                .iter()
                .any(|prefix| key.starts_with(prefix)),
            CliDriver::Traex => ["ANTHROPIC_", "CLAUDE_", "CURSOR_", "CODEX_"]
                .iter()
                .any(|prefix| key.starts_with(prefix)),
            CliDriver::Cursor => ["ANTHROPIC_", "CLAUDE_", "OPENAI_", "CODEX_", "TRAE_"]
                .iter()
                .any(|prefix| key.starts_with(prefix)),
            CliDriver::Other => true,
        };
        if !foreign {
            env.retain(|(existing, _)| existing != &key);
            env.push((key, value));
        }
    }
    env.push((crate::bridge::MANAGED_SESSION_ENV.into(), "1".into()));
    env.sort_by(|a, b| a.0.cmp(&b.0));
    env
}

fn backend_fingerprint(agent: &Agent) -> Result<String> {
    let driver = CliDriver::from_kind(agent.cli_kind());
    let effective = crate::runtime::chat_model::effective(agent, None);
    Ok(crate::runtime::document_comments::hex_digest(
        &serde_json::to_vec(&(
            agent.cli_kind(),
            agent.cli_cmd(),
            effective.model(),
            effective.effort(),
            backend_env(agent, driver),
        ))?,
    ))
}

#[cfg(test)]
pub(super) fn generate(agent: &Agent, record: &Record, stop: &AtomicBool) -> Result<String> {
    generate_observed(agent, record, stop, &|_| Ok(()))
}

#[cfg(test)]
fn generate_observed(
    agent: &Agent,
    record: &Record,
    stop: &AtomicBool,
    on_process: &dyn Fn(i32) -> Result<()>,
) -> Result<String> {
    let client = Arc::new(FeishuClient::new(
        "feishu.cn",
        String::new(),
        String::new(),
        true,
    ));
    generate_with_client(
        agent,
        record,
        client,
        &|| stop.load(Ordering::Relaxed),
        None,
        on_process,
    )
}

struct DraftControl<'a> {
    cancelled: &'a dyn Fn() -> bool,
    deadline: Option<Instant>,
}

pub(super) fn generate_with_client(
    agent: &Agent,
    record: &Record,
    client: Arc<FeishuClient>,
    stop: &dyn Fn() -> bool,
    store: Option<crate::runtime::document_comments::Store>,
    on_process: &dyn Fn(i32) -> Result<()>,
) -> Result<String> {
    let owns_record = |current: &Agent| {
        record.agent_name == current.name
            && super::resolve_creds(current).0.as_deref() == Some(record.notice.app_id.as_str())
            && record.epoch == super::FeishuEpochIdentity::capture(current).fingerprint()
            && !record.bot_open_id.is_empty()
            && record.notice.recipient_open_id == record.bot_open_id
    };
    anyhow::ensure!(owns_record(agent), summon_i18n::format!("r53d7f20912"));
    let timeout = Duration::from_secs(
        super::resolve_messaging(agent)
            .document_comment_timeout_secs
            .get(),
    );
    let started = Instant::now();
    let cancelled = || {
        stop()
            || started.elapsed() >= timeout
            || !super::document_comments::enabled_for_record(agent, record)
    };
    anyhow::ensure!(!cancelled(), summon_i18n::format!("r9745139d37"));
    let fingerprint = backend_fingerprint(agent)?;
    let control = DraftControl {
        cancelled: &cancelled,
        deadline: started.checked_add(timeout),
    };
    let result = generate_for_backend(agent, record, client, store, &control, on_process);
    anyhow::ensure!(
        started.elapsed() < timeout,
        summon_i18n::format!("r93347f73c4", timeout.as_secs())
    );
    let text = result?;
    let current = agent.reread().context(summon_i18n::text!("r389dcede81"))?;
    anyhow::ensure!(
        owns_record(&current)
            && current.is_routable()
            && backend_fingerprint(&current)? == fingerprint,
        summon_i18n::format!("rabbb926689")
    );
    Ok(text)
}

fn generate_for_backend(
    agent: &Agent,
    record: &Record,
    client: Arc<FeishuClient>,
    store: Option<crate::runtime::document_comments::Store>,
    control: &DraftControl<'_>,
    on_process: &dyn Fn(i32) -> Result<()>,
) -> Result<String> {
    let cancelled = control.cancelled;
    let driver = CliDriver::from_kind(agent.cli_kind());
    anyhow::ensure!(
        driver != CliDriver::Other,
        summon_i18n::format!("r7e507285a2")
    );
    // Cursor Ask 的权限不控制用户、Claude、企业和团队 hooks。
    anyhow::ensure!(
        driver != CliDriver::Cursor,
        summon_i18n::format!("r94aea73818")
    );
    let temp = tempfile::tempdir()?;
    let reader_agent = agent.clone();
    let reader_record = record.clone();
    let reader_client = client.clone();
    let load_agent = agent.clone();
    let load_record = record.clone();
    let reader = ReaderServer::start(
        temp.path(),
        Box::new(move || {
            if let Some(store) = &store {
                anyhow::ensure!(
                    store.get(&reader_record.id)?.stage
                        == crate::runtime::document_comments::Stage::Drafting,
                    summon_i18n::format!("rdc5dfd3aff")
                );
            }
            validate_document_read(&reader_agent, &reader_record, &reader_client)
        }),
        Box::new(move || {
            super::document_comments::read_document_text(&load_agent, &load_record, &client)
        }),
    )?;
    let prompt = summon_i18n::format!("rf04354b53a", agent.name, read_only_context_prompt(record)?);
    let env = backend_env(agent, driver);
    if matches!(driver, CliDriver::Codex | CliDriver::Traex) {
        return generate_codex(
            agent,
            &prompt,
            &env,
            temp.path(),
            &reader,
            control,
            on_process,
        );
    }
    let output_path = temp.path().join("result.json");
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&output_path)?;
    let mut command = Command::new(agent.cli_cmd());
    command
        .current_dir(temp.path())
        .env_clear()
        .envs(env.clone())
        .stdout(output)
        .stderr(Stdio::null())
        .process_group(0);
    match driver {
        CliDriver::Claude => {
            let input_path = temp.path().join("prompt.txt");
            crate::runtime::fsio::atomic_write_mode(&input_path, prompt.as_bytes(), 0o600)?;
            command
                .args(claude_args(agent, &reader)?)
                .stdin(std::fs::File::open(input_path)?);
        }
        _ => unreachable!(),
    }
    let raw = run_command(command, &output_path, cancelled, on_process)?;
    completed_text(&raw)
}

pub(super) fn validate_document_read(
    agent: &Agent,
    record: &Record,
    client: &FeishuClient,
) -> Result<()> {
    let current = agent.reread().context(summon_i18n::text!("r40419e6d76"))?;
    anyhow::ensure!(
        super::document_comments::enabled_for_record(&current, record)
            && record.agent_name == current.name
            && super::resolve_creds(&current).0.as_deref() == Some(record.notice.app_id.as_str())
            && record.epoch == super::FeishuEpochIdentity::capture(&current).fingerprint()
            && super::agent_owner(&current).as_deref() == Some(record.owner_entry.as_str()),
        summon_i18n::format!("r02fc9e2996")
    );
    let frozen = record
        .context
        .as_ref()
        .context(summon_i18n::text!("r4826c9cae1"))?;
    let fresh = super::document_comments::read_context(&current, record, client)?;
    anyhow::ensure!(
        fresh.reply_allowed
            && fresh.fingerprint == frozen.fingerprint
            && fresh.document_modified_time == frozen.document_modified_time,
        summon_i18n::format!("r3b232b2464")
    );
    Ok(())
}

fn generate_codex(
    agent: &Agent,
    prompt: &str,
    env: &[(String, String)],
    cwd: &std::path::Path,
    reader: &ReaderServer,
    control: &DraftControl<'_>,
    on_process: &dyn Fn(i32) -> Result<()>,
) -> Result<String> {
    let cancelled = control.cancelled;
    use crate::codex::codex_app_server::{
        CodexAppServerClient, CodexAppServerConfig, CodexAppServerError, TurnOptions,
    };
    use summon_core::codex_rpc::CodexAppServerNotification;
    let mut args = vec![
        "app-server".into(),
        "--listen".into(),
        "stdio://".into(),
        "-c".into(),
        "features.hooks=false".into(),
        "-c".into(),
        "features.plugins=false".into(),
        "-c".into(),
        "features.apps=false".into(),
    ];
    if let Some(window) = agent.runtime.as_ref().and_then(|r| r.model_context_window) {
        args.extend(["-c".into(), format!("model_context_window={window}")]);
    }
    let mut config = CodexAppServerConfig {
        program: agent.cli_cmd(),
        args,
        work_dir: Some(cwd.into()),
        env: env.to_vec(),
        request_timeout: Duration::from_secs(5),
        process_deadline: control.deadline,
        experimental_api: true,
        ..Default::default()
    };
    let mut preflight = CodexAppServerClient::spawn(config.clone())?;
    on_process(preflight.pgid())?;
    let reader_config = reader.server_config()?;
    config
        .args
        .extend(preflight.draft_mcp_overrides(Some((&reader.name, &reader_config)))?);
    drop(preflight);
    anyhow::ensure!(!cancelled(), summon_i18n::format!("rec8ea741f4"));
    let mut client = CodexAppServerClient::spawn(config)?;
    on_process(client.pgid())?;
    anyhow::ensure!(!cancelled(), summon_i18n::format!("rec8ea741f4"));
    let thread =
        client.thread_start_draft(&cwd.to_string_lossy(), Some((&reader.name, &reader_config)))?;
    let effective = crate::runtime::chat_model::effective(agent, None);
    let effort = effective.effort().filter(|effort| {
        CliDriver::from_kind(agent.cli_kind())
            .resolve_launch(None, Some(effort))
            .is_ok()
    });
    anyhow::ensure!(!cancelled(), summon_i18n::format!("rec8ea741f4"));
    let turn = client.turn_start(
        &thread.id,
        prompt,
        &TurnOptions {
            model: effective.model(),
            effort,
            approval_policy: Some("never"),
            ..Default::default()
        },
        None,
    )?;
    let mut text = None;
    let mut final_seen = false;
    loop {
        anyhow::ensure!(!cancelled(), summon_i18n::format!("rec8ea741f4"));
        match client.recv_notification(Some(Duration::from_millis(200))) {
            Ok(CodexAppServerNotification::ItemCompleted {
                thread_id,
                turn_id,
                item,
            }) if thread_id == thread.id && turn_id == turn.id => {
                if matches!(
                    item["type"].as_str(),
                    Some("agentMessage" | "agent_message")
                ) && (item["phase"].as_str() == Some("final_answer")
                    || (item["phase"].is_null() && !final_seen))
                {
                    text = item["text"].as_str().map(str::to_string);
                    final_seen = item["phase"].as_str() == Some("final_answer");
                }
            }
            Ok(CodexAppServerNotification::TurnCompleted {
                thread_id,
                turn: completed,
            }) if thread_id == thread.id && completed.id == turn.id => {
                anyhow::ensure!(
                    completed.status == "completed" && completed.error.is_none(),
                    summon_i18n::format!("r3a8a48c35f")
                );
                return completed_text(
                    &json!({"type":"result","subtype":"success","is_error":false,"result":text})
                        .to_string(),
                );
            }
            Ok(CodexAppServerNotification::Error {
                thread_id,
                turn_id,
                will_retry: false,
                ..
            }) if thread_id == thread.id && turn_id == turn.id => {
                anyhow::bail!(summon_i18n::format!("r52f9e40810"))
            }
            Ok(_) => {}
            Err(CodexAppServerError::Timeout { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

struct DraftProcess {
    child: std::process::Child,
    killer: ChildKiller,
    reaped: bool,
}

impl Drop for DraftProcess {
    fn drop(&mut self) {
        if !self.reaped {
            self.killer.kill();
            let _ = self.child.wait();
        }
    }
}

fn run_command(
    mut command: Command,
    output: &std::path::Path,
    cancelled: &dyn Fn() -> bool,
    on_process: &dyn Fn(i32) -> Result<()>,
) -> Result<String> {
    anyhow::ensure!(!cancelled(), summon_i18n::format!("rec8ea741f4"));
    let child = command.spawn().context(summon_i18n::text!("r9d7fe02bbe"))?;
    let killer = ChildKiller::new(child.id() as i32);
    let mut process = DraftProcess {
        child,
        killer,
        reaped: false,
    };
    on_process(process.child.id() as i32)?;
    let status = loop {
        if let Some(status) = process.child.try_wait()? {
            process.reaped = true;
            break status;
        }
        anyhow::ensure!(
            !cancelled() && std::fs::metadata(output)?.len() <= MAX_OUTPUT,
            summon_i18n::format!("r73aec941de")
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    anyhow::ensure!(
        status.success() && !cancelled(),
        summon_i18n::format!("rd5bbe2c715")
    );
    let mut raw = String::new();
    std::fs::File::open(output)?
        .take(MAX_OUTPUT + 1)
        .read_to_string(&mut raw)?;
    anyhow::ensure!(
        raw.len() as u64 <= MAX_OUTPUT,
        summon_i18n::format!("rd8e86b668c")
    );
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn configured_agent(cli: &str, script: &std::path::Path) -> (Agent, tempfile::TempDir) {
        let (agent,dir)=crate::test_support::agent_dir(cli,"messaging:\n  document_comments_enabled: true\n  document_comments_non_owner_enabled: true\nmodel: bot-model\neffort: high\nfeishu:\n  app_id_env: DRAFT_APP\n  app_secret_env: DRAFT_SECRET\n");
        std::fs::write(
            agent.dir.join(".env"),
            format!("DRAFT_APP=cli_{cli}\nDRAFT_SECRET=fake_secret\n"),
        )
        .unwrap();
        let mut config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap()).unwrap();
        if cli == "cursor" {
            config["runtime"]["capture"] = serde_yaml::Value::String("cli".into());
        }
        config["runtime"]["cli_cmd"] = serde_yaml::Value::String(script.display().to_string());
        config["runtime"]["env"]=serde_yaml::to_value(json!({
            "ANTHROPIC_AUTH_TOKEN":"claude-token", "OPENAI_API_KEY":"codex-token", "CURSOR_API_KEY":"cursor-token",
            "CODEX_HOME":dir.path().join("codex-home"), "CLAUDE_CONFIG_DIR":dir.path().join("claude-home"),
            "CURSOR_CONFIG_DIR":dir.path().join("cursor-source"), "DRAFT_MARKER":cli,
        })).unwrap();
        std::fs::write(
            agent.manifest_path(),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();
        (agent.reread().unwrap(), dir)
    }

    fn record(agent: &Agent, root: &std::path::Path) -> Record {
        use crate::runtime::document_comments::{CommentContext, CommentNotice, Store};
        let store = Store::new(root.join("records"));
        let id = store
            .receive(
                CommentNotice {
                    app_id: super::super::resolve_creds(agent).0.unwrap(),
                    event_id: "event".into(),
                    file_token: "doc".into(),
                    file_type: "docx".into(),
                    comment_id: "100".into(),
                    reply_id: "200".into(),
                    sender_open_id: "ou_commenter".into(),
                    recipient_open_id: "ou_bot".into(),
                },
                1,
                true,
            )
            .unwrap();
        store
            .update(&id, |r| {
                r.agent_name = agent.name.clone();
                r.epoch = super::super::FeishuEpochIdentity::capture(agent).fingerprint();
                r.bot_open_id = "ou_bot".into();
                r.owner_open_id = "ou_owner".into();
                r.context = Some(CommentContext {
                    owner_mention: false,
                    title: "Title".into(),
                    url: "https://example.com".into(),
                    quote: "Quote".into(),
                    thread: "Comment".into(),
                    fingerprint: "thread".into(),
                    document_modified_time: "1".into(),
                    document_text: "Body".into(),
                    reply_allowed: true,
                });
                Ok(())
            })
            .unwrap();
        store.get(&id).unwrap()
    }

    fn native_owner_bot_draft(agent_dir_env: &str, driver: CliDriver) {
        let dir = std::path::PathBuf::from(
            std::env::var_os(agent_dir_env).expect("set the native draft agent directory"),
        );
        assert!(dir.is_absolute());
        let mut agent: Agent = serde_yaml::from_str(
            &std::fs::read_to_string(dir.join("agent.yaml")).expect("read native agent manifest"),
        )
        .expect("parse native agent manifest");
        agent.dir = dir;
        assert_eq!(CliDriver::from_kind(agent.cli_kind()), driver);
        assert!(agent.is_routable());
        let scratch = tempfile::tempdir().unwrap();
        let mut record = record(&agent, scratch.path());
        let context = record.context.as_mut().unwrap();
        context.title = "合成评论起草验收".into();
        context.quote = "最大等待时长为 15 秒。".into();
        context.thread = "请确认文档约定的最大等待时长。".into();
        let started = Instant::now();
        let observed = std::sync::atomic::AtomicBool::new(false);
        let draft = generate_observed(&agent, &record, &AtomicBool::new(false), &|pid| {
            assert!(
                crate::runtime::document_comments::DraftProcessIdentity::capture(pid).is_some()
            );
            observed.store(true, Ordering::Relaxed);
            Ok(())
        })
        .expect("native document comment draft must complete successfully");
        assert!(observed.load(Ordering::Relaxed));
        assert!(
            draft.contains("15"),
            "draft must preserve the supplied fact"
        );
        println!(
            "native_comment_draft_ok backend={} chars={} elapsed_ms={}",
            agent.cli_kind(),
            draft.chars().count(),
            started.elapsed().as_millis()
        );
    }

    #[test]
    #[ignore = "needs SUMMON_TEST_CLAUDE_DRAFT_AGENT_DIR; performs one real provider call"]
    fn native_claude_document_comment_draft() {
        native_owner_bot_draft("SUMMON_TEST_CLAUDE_DRAFT_AGENT_DIR", CliDriver::Claude);
    }

    #[test]
    #[ignore = "needs SUMMON_TEST_CODEX_DRAFT_AGENT_DIR; performs one real provider call"]
    fn native_codex_document_comment_draft() {
        native_owner_bot_draft("SUMMON_TEST_CODEX_DRAFT_AGENT_DIR", CliDriver::Codex);
    }

    #[test]
    fn claude_uses_its_own_executable_model_and_credentials() {
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("backend.sh");
        crate::test_support::write_test_executable(
            &script,
            r#"#!/bin/sh
root="${0%/*}"
printf '%s\n' "$@" > "$root/args"
printf '%s|%s|%s|%s|%s|%s' "${ANTHROPIC_AUTH_TOKEN-}" "${OPENAI_API_KEY-}" "${CURSOR_API_KEY-}" "${FEISHU_APP_ID-}" "${SUMMON_AGENT-}" "${DRAFT_MARKER-}" > "$root/env"
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"own backend draft"}'
"#,
        );
        let (agent, dir) = configured_agent("claude", &script);
        let record = record(&agent, dir.path());
        assert_eq!(
            generate(&agent, &record, &AtomicBool::new(false)).unwrap(),
            "own backend draft"
        );
        let args = std::fs::read_to_string(bin.path().join("args")).unwrap();
        assert!(args.contains("--model\nbot-model"));
        assert!(!args.contains("--resume"));
        assert_eq!(
            std::fs::read_to_string(bin.path().join("env")).unwrap(),
            "claude-token|||||claude"
        );
    }

    #[test]
    fn codex_family_uses_own_backend_and_cold_read_only_ephemeral_thread() {
        for (cli, outcome) in [
            ("codex", "completed"),
            ("traex", "completed"),
            ("codex", "failed"),
            ("codex", "partial"),
            ("codex", "foreign"),
            ("codex", "mcp_leak"),
        ] {
            let bin = tempfile::tempdir().unwrap();
            let mut cases = r#"
*'config/read'*)
if test -n "$DRAFT_SERVERS"; then servers="$DRAFT_SERVERS"; else servers='{"write-tool":{"enabled":true}}'; fi
printf '{"id":%s,"result":{"config":{"mcp_servers":%s}}}\n' "$id" "$servers" ;;
*'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"draft-thread"}}}' ;;
*'turn/start'*)
printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"draft-turn","status":"inProgress"}}}'
printf '%s\n' '{"method":"item/completed","params":{"threadId":"draft-thread","turnId":"draft-turn","item":{"id":"answer","type":"agentMessage","phase":"final_answer","text":"codex draft"}}}'
printf '%s\n' '{"method":"turn/completed","params":{"threadId":"draft-thread","turn":{"id":"draft-turn","status":"completed"}}}' ;;
"#.to_string();
            if outcome == "failed" {
                cases = cases.replace("\"status\":\"completed\"", "\"status\":\"failed\"");
            }
            if outcome == "mcp_leak" {
                cases = cases.replace("servers=\"$DRAFT_SERVERS\"", "servers=$(printf '%s' \"$DRAFT_SERVERS\" | sed 's/\"write-tool\":{\"enabled\":false}/\"write-tool\":{\"enabled\":true}/')");
            }
            if outcome == "partial" {
                cases = cases
                    .lines()
                    .map(|line| {
                        if line.contains("turn/completed") {
                            "exit 0 ;;"
                        } else {
                            line
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            if outcome == "foreign" {
                cases = cases.replace(
                    "\"method\":\"item/completed\",\"params\":{\"threadId\":\"draft-thread\"",
                    "\"method\":\"item/completed\",\"params\":{\"threadId\":\"foreign-thread\"",
                );
            }
            crate::test_support::write_fake_app_server(bin.path(), &cases);
            let wrapper = fake_reader_wrapper(bin.path());
            let (agent, dir) = configured_agent(cli, &wrapper);
            let record = record(&agent, dir.path());
            let result = generate(&agent, &record, &AtomicBool::new(false));
            if outcome != "completed" {
                if outcome == "mcp_leak" {
                    assert!(result
                        .as_ref()
                        .unwrap_err()
                        .to_string()
                        .contains("unexpected capability"));
                }
                assert!(result.is_err(), "{outcome}");
                continue;
            }
            assert_eq!(result.unwrap(), "codex draft");
            assert_eq!(
                std::fs::read_to_string(bin.path().join("backend-env")).unwrap(),
                format!("codex-token|||{cli}")
            );
            assert!(std::fs::read_to_string(bin.path().join("launch-args"))
                .unwrap()
                .contains("mcp_servers={\"write-tool\"={enabled=false}"));
            let raw = std::fs::read_to_string(crate::test_support::fake_rpc_server_log(bin.path()))
                .unwrap();
            let requests: Vec<Value> = raw
                .lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect();
            let start = requests
                .iter()
                .find(|r| r["method"] == "thread/start")
                .unwrap();
            assert_eq!(start["params"]["ephemeral"], true);
            assert_eq!(start["params"]["sandbox"], "read-only");
            assert_eq!(start["params"]["approvalPolicy"], "never");
            let servers = start["params"]["config"]["mcp_servers"]
                .as_object()
                .unwrap();
            let readers: Vec<_> = servers
                .values()
                .filter(|server| server["enabled"] == true)
                .collect();
            assert_eq!(readers.len(), 1);
            assert_eq!(readers[0]["enabled_tools"], json!(["read_document"]));
            assert_eq!(
                start["params"]["config"]["mcp_servers"]["write-tool"]["enabled"],
                false
            );
            let turn = requests
                .iter()
                .find(|r| r["method"] == "turn/start")
                .unwrap();
            assert_eq!(turn["params"]["model"], "bot-model");
            assert_eq!(turn["params"]["effort"], "high");
            let env = backend_env(&agent, CliDriver::from_kind(cli));
            assert!(env
                .iter()
                .any(|(k, v)| k == "OPENAI_API_KEY" && v == "codex-token"));
            assert!(!env
                .iter()
                .any(|(k, _)| k.starts_with("ANTHROPIC_") || k.starts_with("CURSOR_")));
            assert!(!raw.contains("thread/resume"));
        }
    }

    fn fake_reader_wrapper(dir: &std::path::Path) -> std::path::PathBuf {
        let wrapper = dir.join("launch.sh");
        crate::test_support::write_test_executable(
            &wrapper,
            r#"#!/bin/sh
export DRAFT_START_ARGS="$*"
export DRAFT_SERVERS=""
for arg in "$@"; do
case "$arg" in
mcp_servers=*) DRAFT_SERVERS=$(printf '%s' "${arg#mcp_servers=}" | sed 's/={/:{/g;s/command=/"command":/g;s/args=/"args":/g;s/enabled=/"enabled":/g;s/enabled_tools=/"enabled_tools":/g'); export DRAFT_SERVERS ;;
esac
done
printf '%s\n' "$@" >> "${0%/*}/launch-args"
printf '%s|%s|%s|%s' "${OPENAI_API_KEY-}" "${ANTHROPIC_AUTH_TOKEN-}" "${CURSOR_API_KEY-}" "${DRAFT_MARKER-}" > "${0%/*}/backend-env"
exec "${0%/*}/fake-app-server.sh" "$@"
"#,
        );
        wrapper
    }

    #[test]
    fn cursor_cli_and_rpc_refuse_drafting_before_any_backend_process() {
        for capture in ["cli", "rpc"] {
            let bin = tempfile::tempdir().unwrap();
            let script = bin.path().join("backend.sh");
            crate::test_support::write_test_executable(
                &script,
                "#!/bin/sh\nprintf called > \"${0%/*}/called\"\nexit 1\n",
            );
            let (agent, dir) = configured_agent("cursor", &script);
            let mut config: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap())
                    .unwrap();
            config["runtime"]["capture"] = serde_yaml::Value::String(capture.into());
            std::fs::write(
                agent.manifest_path(),
                serde_yaml::to_string(&config).unwrap(),
            )
            .unwrap();
            let agent = agent.reread().unwrap();
            let record = record(&agent, dir.path());
            let error = generate(&agent, &record, &AtomicBool::new(false)).unwrap_err();
            assert!(error.to_string().contains("无法可靠关闭 hooks"));
            assert!(!bin.path().join("called").exists());
        }
    }

    #[test]
    fn different_owner_bots_cannot_use_each_others_backend() {
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("backend.sh");
        crate::test_support::write_test_executable(
            &script,
            "#!/bin/sh\nprintf called > \"${0%/*}/called\"\nexit 1\n",
        );
        let (claude, claude_dir) = configured_agent("claude", &script);
        let (cursor, cursor_dir) = configured_agent("cursor", &script);
        let claude_record = record(&claude, claude_dir.path());
        let cursor_record = record(&cursor, cursor_dir.path());
        assert_ne!(claude_record.notice.app_id, cursor_record.notice.app_id);
        assert!(generate(&claude, &cursor_record, &AtomicBool::new(false)).is_err());
        assert!(generate(&cursor, &claude_record, &AtomicBool::new(false)).is_err());
        assert!(!bin.path().join("called").exists());
    }

    #[test]
    fn foreign_bot_or_app_and_unsupported_backend_cannot_start_a_draft() {
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("never.sh");
        crate::test_support::write_test_executable(
            &script,
            "#!/bin/sh\nprintf called > \"${0%/*}/called\"\nexit 1\n",
        );
        let (agent, dir) = configured_agent("cursor", &script);
        let original = record(&agent, dir.path());
        for kind in ["agent", "app", "recipient", "epoch"] {
            let mut record = original.clone();
            match kind {
                "agent" => record.agent_name = "other".into(),
                "app" => record.notice.app_id = "other".into(),
                "recipient" => record.notice.recipient_open_id = "ou_other".into(),
                _ => record.epoch = "other".into(),
            };
            assert!(generate(&agent, &record, &AtomicBool::new(false)).is_err());
        }
        let (unknown, other_dir) = configured_agent("unknown-backend", &script);
        let record = record(&unknown, other_dir.path());
        assert!(generate(&unknown, &record, &AtomicBool::new(false)).is_err());
        assert!(!bin.path().join("called").exists());
    }

    #[test]
    fn disabling_comments_cancels_and_reaps_active_draft() {
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("backend");
        crate::test_support::write_test_executable(&script, "#!/bin/sh\nexec sleep 30\n");
        let (agent, dir) = configured_agent("claude", &script);
        let record = record(&agent, dir.path());
        let pid = std::cell::Cell::new(0);
        let started = Instant::now();
        let result = generate_observed(&agent, &record, &AtomicBool::new(false), &|process| {
            pid.set(process);
            let mut config: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap())
                    .unwrap();
            config["messaging"]["document_comments_enabled"] = serde_yaml::Value::Bool(false);
            std::fs::write(
                agent.manifest_path(),
                serde_yaml::to_string(&config).unwrap(),
            )
            .unwrap();
            Ok(())
        });
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(pid.get() > 0);
        assert!(crate::capture::rpc_procs::probe_start(pid.get()).is_none());
    }

    #[test]
    fn comment_deadline_bounds_codex_handshake_rpc_and_blocked_stdin() {
        for phase in [
            "initialize",
            "config/read",
            "thread/start",
            "turn/start",
            "stdin",
        ] {
            let bin = tempfile::tempdir().unwrap();
            let script = crate::test_support::write_fake_app_server(
                bin.path(),
                r#"
*'config/read'*)
if test -n "$DRAFT_SERVERS"; then servers="$DRAFT_SERVERS"; else servers='{"write-tool":{"enabled":true}}'; fi
printf '{"id":%s,"result":{"config":{"mcp_servers":%s}}}\n' "$id" "$servers" ;;
*'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"draft-thread"}}}' ;;
*'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"draft-turn","status":"inProgress"}}}' ;;
"#,
            );
            let cases_path = bin.path().join("fake-app-server.cases");
            let mut cases = std::fs::read_to_string(&cases_path).unwrap();
            if phase == "initialize" {
                cases = cases.replace("*initialize*)", "*initialize*) sleep 6;");
            } else if phase == "stdin" {
                cases = cases.replace(
                    "\"draft-thread\"}}}' ;;",
                    "\"draft-thread\"}}}'; sleep 6 ;;",
                );
            } else {
                cases = cases.replace(&format!("*'{phase}'*)"), &format!("*'{phase}'*) sleep 6;"));
            }
            std::fs::write(&cases_path, cases).unwrap();
            let body = std::fs::read_to_string(&script).unwrap().replace(
                "while IFS= read -r line; do",
                "printf '%s\n' \"$$\" >> \"$DIR/pids\"\nwhile IFS= read -r line; do",
            );
            // The deadline includes startup, so fixture frame parsing must not spawn extra processes.
            let id_parser = r#"id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9]*\).*/\1/p')"#;
            assert!(body.contains(id_parser));
            let body = body.replace(
                id_parser,
                r#"id=${line#'{"id":'}
id=${id%%[!0-9]*}"#,
            );
            crate::test_support::write_test_executable(&script, &body);
            let wrapper = fake_reader_wrapper(bin.path());
            let (agent, dir) = configured_agent("codex", &wrapper);
            let mut config: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap())
                    .unwrap();
            config["messaging"]["document_comment_timeout_secs"] =
                serde_yaml::Value::Number(1.into());
            std::fs::write(
                agent.manifest_path(),
                serde_yaml::to_string(&config).unwrap(),
            )
            .unwrap();
            let agent = agent.reread().unwrap();
            let mut record = record(&agent, dir.path());
            if phase == "stdin" {
                record.context.as_mut().unwrap().thread = "x".repeat(200_000);
            }
            let started = Instant::now();
            let error = generate(&agent, &record, &AtomicBool::new(false)).unwrap_err();
            assert!(
                format!("{error:#}").contains("超过1秒时限"),
                "{phase}: {error:#}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "{phase}: {:?}",
                started.elapsed()
            );
            let log = std::fs::read_to_string(crate::test_support::fake_rpc_server_log(bin.path()))
                .unwrap();
            if phase == "stdin" {
                assert!(
                    log.contains("thread/start") && !log.contains("turn/start"),
                    "backend must stop reading before the large turn frame"
                );
            } else {
                assert!(
                    log.contains(phase),
                    "{phase}: fault-injected phase must be reached; log={log}"
                );
            }
            for pid in std::fs::read_to_string(bin.path().join("pids"))
                .unwrap()
                .lines()
            {
                assert!(
                    crate::capture::rpc_procs::probe_start(pid.parse().unwrap()).is_none(),
                    "{phase}: child must be reaped"
                );
            }
        }
    }

    #[test]
    fn configured_timeout_reaps_claude_and_codex_drafts() {
        for cli in ["claude", "codex"] {
            let bin = tempfile::tempdir().unwrap();
            let script = if cli == "claude" {
                let script = bin.path().join("backend");
                crate::test_support::write_test_executable(&script, "#!/bin/sh\nexec sleep 30\n");
                script
            } else {
                crate::test_support::write_fake_app_server(
                    bin.path(),
                    r#"
*'config/read'*)
if test -n "$DRAFT_SERVERS"; then servers="$DRAFT_SERVERS"; else servers='{"write-tool":{"enabled":true}}'; fi
printf '{"id":%s,"result":{"config":{"mcp_servers":%s}}}\n' "$id" "$servers" ;;
*'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"draft-thread"}}}' ;;
*'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"draft-turn","status":"inProgress"}}}' ;;
"#,
                );
                fake_reader_wrapper(bin.path())
            };
            let (agent, dir) = configured_agent(cli, &script);
            let mut config: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap())
                    .unwrap();
            config["messaging"]["document_comment_timeout_secs"] =
                serde_yaml::Value::Number(1.into());
            std::fs::write(
                agent.manifest_path(),
                serde_yaml::to_string(&config).unwrap(),
            )
            .unwrap();
            let agent = agent.reread().unwrap();
            let record = record(&agent, dir.path());
            let pids = std::cell::RefCell::new(Vec::new());
            let start = Instant::now();
            let error = generate_observed(&agent, &record, &AtomicBool::new(false), &|pid| {
                pids.borrow_mut().push(pid);
                Ok(())
            })
            .unwrap_err();
            assert!(format!("{error:#}").contains("超过1秒时限"));
            assert!(start.elapsed() >= Duration::from_secs(1));
            assert!(start.elapsed() < Duration::from_secs(4));
            assert_eq!(pids.borrow().len(), if cli == "claude" { 1 } else { 2 });
            for pid in pids.into_inner() {
                assert!(crate::capture::rpc_procs::probe_start(pid).is_none());
            }
        }
    }

    #[test]
    fn disabling_comments_cancels_active_codex_turn() {
        disabling_comment_setting_cancels_active_codex_turn("document_comments_enabled");
    }

    #[test]
    fn disabling_non_owner_comments_cancels_active_codex_turn() {
        disabling_comment_setting_cancels_active_codex_turn("document_comments_non_owner_enabled");
    }

    fn disabling_comment_setting_cancels_active_codex_turn(key: &'static str) {
        let bin = tempfile::tempdir().unwrap();
        crate::test_support::write_fake_app_server(
            bin.path(),
            r#"
*'config/read'*)
if test -n "$DRAFT_SERVERS"; then servers="$DRAFT_SERVERS"; else servers='{"write-tool":{"enabled":true}}'; fi
printf '{"id":%s,"result":{"config":{"mcp_servers":%s}}}\n' "$id" "$servers" ;;
*'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"draft-thread"}}}' ;;
*'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"draft-turn","status":"inProgress"}}}' ;;
"#,
        );
        let script = fake_reader_wrapper(bin.path());
        let (agent, dir) = configured_agent("codex", &script);
        let record = record(&agent, dir.path());
        let controller = {
            let agent = agent.clone();
            let log = crate::test_support::fake_rpc_server_log(bin.path());
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if std::fs::read_to_string(&log)
                        .unwrap_or_default()
                        .contains("turn/start")
                    {
                        let mut config: serde_yaml::Value = serde_yaml::from_str(
                            &std::fs::read_to_string(agent.manifest_path()).unwrap(),
                        )
                        .unwrap();
                        config["messaging"][key] = serde_yaml::Value::Bool(false);
                        std::fs::write(
                            agent.manifest_path(),
                            serde_yaml::to_string(&config).unwrap(),
                        )
                        .unwrap();
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                false
            })
        };
        let pids = std::cell::RefCell::new(Vec::new());
        let started = Instant::now();
        let result = generate_observed(&agent, &record, &AtomicBool::new(false), &|pid| {
            pids.borrow_mut().push(pid);
            Ok(())
        });
        assert!(controller.join().unwrap(), "必须先进入模型 turn 再关闭功能");
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(pids.borrow().len(), 2);
        for pid in pids.into_inner() {
            assert!(crate::capture::rpc_procs::probe_start(pid).is_none());
        }
    }

    #[test]
    fn drafting_exposes_only_document_reader_with_ephemeral_context() {
        let agent: Agent = serde_yaml::from_str("name: writer\nmodel: sonnet\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let reader = ReaderServer::start(
            dir.path(),
            Box::new(|| Ok(())),
            Box::new(|| Ok("body".into())),
        )
        .unwrap();
        let args = claude_args(&agent, &reader).unwrap();
        for pair in [
            ["--tools", ""],
            ["--setting-sources", ""],
            ["--settings", r#"{"disableAllHooks":true}"#],
            ["--permission-mode", "dontAsk"],
        ] {
            assert!(args.windows(2).any(|w| w == pair), "{pair:?}");
        }
        assert!(args.contains(&"--strict-mcp-config".into()));
        assert!(args.contains(&"--no-session-persistence".into()));
        assert!(!args.contains(&"--resume".into()));
        let config: serde_json::Value = serde_json::from_str(&args[7]).unwrap();
        assert_eq!(config["mcpServers"].as_object().unwrap().len(), 1);
        assert!(config["mcpServers"][&reader.name]["args"]
            .to_string()
            .contains("comment-reader"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--allowedTools"
                && w[1] == format!("mcp__{}__read_document", reader.name)));
    }

    #[test]
    fn process_failure_and_cancellation_never_publish_partial_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        for script in ["printf '%s' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"draft\"}'; exit 1", "sleep 30"] {
            let mut command=Command::new("/bin/sh");
            command.args(["-c",script]).stdout(std::fs::File::create(&path).unwrap()).process_group(0);
            let start=Instant::now();
            assert!(run_command(command,&path,&|| start.elapsed() > Duration::from_millis(150), &|_| Ok(())).is_err());
            assert!(start.elapsed()<Duration::from_secs(2));
        }
    }

    #[test]
    fn only_complete_success_is_a_draft() {
        assert_eq!(
            completed_text(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"草稿"}"#
            )
            .unwrap(),
            "草稿"
        );
        for raw in [
            r#"{"type":"assistant","result":"部分正文"}"#,
            r#"{"type":"result","subtype":"error","is_error":true,"result":"部分正文"}"#,
            "{",
            r#"{"type":"result","subtype":"success","is_error":false,"result":" "}"#,
        ] {
            assert!(completed_text(raw).is_err());
        }
    }
}
