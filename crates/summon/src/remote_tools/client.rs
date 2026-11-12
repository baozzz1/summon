use super::process::{provider_command, provider_version, temporary_state, ProcessGroup};
use super::wire;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::Duration;
use summon_core::remote_tools::{
    self as protocol, Frame, Provider, MAX_SESSIONS, PROTOCOL_VERSION,
};
use tempfile::TempDir;

enum Event {
    Peer(Frame),
    PeerClosed(String),
    Tool(u64, Value),
    ToolClosed(u64, String),
}

struct ToolSession {
    _process: ProcessGroup,
    _shells: super::shell::ShellLease,
    _state: TempDir,
    input: SyncSender<Value>,
    failed: Arc<AtomicBool>,
}

impl ToolSession {
    fn start(id: u64, executable: &Path, directory: &Path, tx: SyncSender<Event>) -> Result<Self> {
        let state = temporary_state("srt-")?;
        let shells = super::shell::ShellLease::start(state.path())?;
        let mut command = provider_command(executable, directory, state.path());
        command.env("CLAUDE_CODE_SHELL_PREFIX", shells.prefix()?);
        let mut process = ProcessGroup::spawn(&mut command)?;
        let input = process
            .child
            .stdin
            .take()
            .context(summon_i18n::text!("r4bd655810c"))?;
        let output = process
            .child
            .stdout
            .take()
            .context(summon_i18n::text!("r0225d5966e"))?;
        let failed = Arc::new(AtomicBool::new(false));
        let input = wire::writer(input, Arc::clone(&failed));
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            let error = loop {
                match wire::read_json::<Value>(&mut reader) {
                    Ok(Some(message)) => {
                        if tx.send(Event::Tool(id, message)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break summon_i18n::text!("rc9611d842f").to_string(),
                    Err(error) => break error.to_string(),
                }
            };
            // 主循环退出后不再有消费者，清理不依赖此通知成功。
            let _ = tx.send(Event::ToolClosed(id, error));
        });
        Ok(Self {
            _process: process,
            _shells: shells,
            _state: state,
            input,
            failed,
        })
    }
}

pub(super) fn ssh_args(host: &str, name: &str, remote_summon: &str) -> Result<Vec<String>> {
    protocol::validate_name(name).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        !host.is_empty() && !host.starts_with('-') && !host.contains('\0'),
        summon_i18n::format!("rad26fa5ebd")
    );
    anyhow::ensure!(
        !remote_summon.is_empty() && !remote_summon.contains('\0'),
        summon_i18n::format!("rb3891fa4a7")
    );
    let quote = summon_core::launch_env::sh_single_quote;
    let command = format!(
        "{} remote-tools server --name {}",
        quote(remote_summon),
        quote(name)
    );
    Ok([
        "-T",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "--",
        host,
        &command,
    ]
    .into_iter()
    .map(str::to_string)
    .collect())
}

pub(super) fn connect(
    host: &str,
    name: &str,
    remote_summon: &str,
    claude: PathBuf,
    directory: Option<PathBuf>,
) -> Result<()> {
    let shutdown = wire::shutdown_flag()?;
    let directory = std::fs::canonicalize(directory.unwrap_or(std::env::current_dir()?))?;
    anyhow::ensure!(directory.is_dir(), summon_i18n::format!("ref9a6e6849"));
    let version = provider_version(&claude)?;
    verify_provider(&claude, &directory, &shutdown)?;
    let mut command = Command::new(summon_core::tools::resolve(summon_core::tools::Tool::Ssh));
    command
        .args(ssh_args(host, name, remote_summon)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut ssh = ProcessGroup::spawn(&mut command)?;
    let input = ssh
        .child
        .stdin
        .take()
        .context(summon_i18n::text!("r0f3f883edc"))?;
    let output = ssh
        .child
        .stdout
        .take()
        .context(summon_i18n::text!("r6f4f7b2eae"))?;
    let failed = Arc::new(AtomicBool::new(false));
    let peer = wire::writer(input, Arc::clone(&failed));
    let (tx, rx) = sync_channel(wire::QUEUE_SIZE);
    let reader_tx = tx.clone();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(output);
        let error = loop {
            match wire::read_json::<Frame>(&mut reader) {
                Ok(Some(frame)) => {
                    if reader_tx.send(Event::Peer(frame)).is_err() {
                        return;
                    }
                }
                Ok(None) => break summon_i18n::text!("r3c96f46fb4").to_string(),
                Err(error) => break error.to_string(),
            }
        };
        let _ = reader_tx.send(Event::PeerClosed(error));
    });
    wire::send(
        &peer,
        Frame::Hello {
            protocol: PROTOCOL_VERSION,
            provider: Provider::ClaudeCode,
            provider_version: version,
            working_dir: directory.to_string_lossy().into_owned(),
        },
    )?;
    match rx
        .recv_timeout(Duration::from_secs(30))
        .context(summon_i18n::text!("r25888fffda"))?
    {
        Event::Peer(Frame::Welcome { protocol }) => wire::check_protocol(protocol)?,
        Event::PeerClosed(error) => anyhow::bail!(error),
        _ => anyhow::bail!(summon_i18n::format!("r39b6f26ce3")),
    }
    eprintln!("{}", summon_i18n::format!("r771b9a9f03"));
    run_sessions(&claude, &directory, peer, failed, tx, rx, shutdown)
}

fn verify_provider(executable: &Path, directory: &Path, shutdown: &AtomicBool) -> Result<()> {
    use serde_json::json;
    let (tx, rx) = sync_channel(wire::QUEUE_SIZE);
    let session = ToolSession::start(0, executable, directory, tx)?;
    wire::send(
        &session.input,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"summon-preflight","version":"1"}}}),
    )?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline && !shutdown.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Tool(_, message)) if message["id"] == 1 => {
                anyhow::ensure!(
                    message.get("error").is_none(),
                    summon_i18n::format!("r4ba0a21ef7")
                );
                wire::send(
                    &session.input,
                    json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                )?;
                wire::send(
                    &session.input,
                    json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                    "name":"Bash","arguments":{"command":"true","description":"verify execution lifecycle"}}}),
                )?;
            }
            Ok(Event::Tool(_, message)) if message["id"] == 2 => {
                anyhow::ensure!(
                    message.get("error").is_none() && message["result"]["isError"] != true,
                    summon_i18n::format!("r1caf6782ce")
                );
                anyhow::ensure!(
                    session._shells.has_started_command(),
                    summon_i18n::format!("re54f92b0af")
                );
                return Ok(());
            }
            Ok(Event::ToolClosed(_, error)) => anyhow::bail!(error),
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!(summon_i18n::format!("r7bdca3fa5b"))
            }
            _ => {}
        }
    }
    anyhow::bail!(summon_i18n::format!("r7b42a165b5"))
}

fn run_sessions(
    executable: &Path,
    default_directory: &Path,
    peer: SyncSender<Frame>,
    failed: Arc<AtomicBool>,
    tx: SyncSender<Event>,
    rx: Receiver<Event>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let mut sessions: HashMap<u64, ToolSession> = HashMap::new();
    while !shutdown.load(Ordering::SeqCst) {
        anyhow::ensure!(
            !failed.load(Ordering::SeqCst),
            summon_i18n::format!("rdb2cf721dd")
        );
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Peer(Frame::Open { id, working_dir })) => {
                anyhow::ensure!(
                    !sessions.contains_key(&id),
                    summon_i18n::format!("r60016fdcd7")
                );
                let opened = (|| -> Result<PathBuf> {
                    anyhow::ensure!(
                        sessions.len() < MAX_SESSIONS,
                        summon_i18n::format!("r6b4e35a86e")
                    );
                    if let Some(path) = &working_dir {
                        protocol::validate_working_dir(path).map_err(anyhow::Error::msg)?;
                    }
                    let path = working_dir
                        .as_deref()
                        .map(Path::new)
                        .unwrap_or(default_directory);
                    let path = std::fs::canonicalize(path)?;
                    anyhow::ensure!(path.is_dir(), summon_i18n::format!("rd6ebd527a1"));
                    let session = ToolSession::start(id, executable, &path, tx.clone())?;
                    sessions.insert(id, session);
                    Ok(path)
                })();
                match opened {
                    Ok(path) => wire::send(
                        &peer,
                        Frame::Opened {
                            id,
                            working_dir: path.to_string_lossy().into_owned(),
                        },
                    )?,
                    Err(error) => wire::send(
                        &peer,
                        Frame::Close {
                            id,
                            error: Some(error.to_string()),
                        },
                    )?,
                }
            }
            Ok(Event::Peer(Frame::Data { id, message })) => {
                if !protocol::allowed_request(&message) {
                    if let Some(error) =
                        protocol::rpc_error(&message, summon_i18n::text!("rff4ff7b4c1"))
                    {
                        wire::send(&peer, Frame::Data { id, message: error })?;
                    }
                    continue;
                }
                if let Some(session) = sessions.get(&id) {
                    if wire::send(&session.input, message).is_err() {
                        sessions.remove(&id);
                        wire::send(
                            &peer,
                            Frame::Close {
                                id,
                                error: Some(summon_i18n::text!("rbd1bc132ae").into()),
                            },
                        )?;
                    }
                }
            }
            Ok(Event::Peer(Frame::Close { id, .. })) => {
                sessions.remove(&id);
            }
            Ok(Event::Tool(id, mut message)) if sessions.contains_key(&id) => {
                protocol::filter_tools(&mut message);
                wire::send(&peer, Frame::Data { id, message })?;
            }
            Ok(Event::ToolClosed(id, error)) => {
                if sessions.remove(&id).is_some() {
                    wire::send(
                        &peer,
                        Frame::Close {
                            id,
                            error: Some(error),
                        },
                    )?;
                }
            }
            Ok(Event::PeerClosed(error)) => anyhow::bail!(error),
            Ok(Event::Tool(_, _)) => {}
            Ok(Event::Peer(_)) => anyhow::bail!(summon_i18n::format!("rda545fb1ee")),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!(summon_i18n::format!("r1eb2caf5e6"))
            }
        }
        let dead: Vec<_> = sessions
            .iter()
            .filter(|(_, session)| session.failed.load(Ordering::SeqCst))
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            sessions.remove(&id);
            wire::send(
                &peer,
                Frame::Close {
                    id,
                    error: Some(summon_i18n::text!("re78d5b06f3").into()),
                },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn isolates_sessions_and_cleans_state_on_close_and_disconnect() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("provider");
        crate::test_support::write_test_executable(&executable, "#!/bin/sh\nwhile IFS= read -r line; do\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"state\":\"%s\",\"cwd\":\"%s\",\"pid\":%s}}\\n' \"$HOME\" \"$PWD\" \"$$\"\ndone\n");
        let (peer, frames) = sync_channel(wire::QUEUE_SIZE);
        let (tx, rx) = sync_channel(wire::QUEUE_SIZE);
        let worker_tx = tx.clone();
        let directory = temp.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            run_sessions(
                &executable,
                &directory,
                peer,
                Arc::new(AtomicBool::new(false)),
                worker_tx,
                rx,
                Arc::new(AtomicBool::new(false)),
            )
        });
        let mut states = Vec::new();
        for id in [1, 2] {
            tx.send(Event::Peer(Frame::Open {
                id,
                working_dir: None,
            }))
            .unwrap();
            assert!(
                matches!(frames.recv_timeout(Duration::from_secs(3)).unwrap(), Frame::Opened { id:got, .. } if got == id)
            );
            tx.send(Event::Peer(Frame::Data {
                id,
                message: json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            }))
            .unwrap();
            let Frame::Data { message, .. } = frames.recv_timeout(Duration::from_secs(3)).unwrap()
            else {
                panic!("expected response");
            };
            let state = PathBuf::from(message["result"]["state"].as_str().unwrap());
            assert_eq!(
                std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
                0o700
            );
            states.push(state);
        }
        assert_ne!(states[0], states[1]);
        tx.send(Event::Peer(Frame::Close { id: 1, error: None }))
            .unwrap();
        tx.send(Event::Peer(Frame::Data {
            id: 2,
            message: json!({"id":9,"method":"tools/call","params":{"name":"WebFetch"}}),
        }))
        .unwrap();
        let Frame::Data { message, .. } = frames.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("expected rejection");
        };
        assert!(message.get("error").is_some());
        assert!(!states[0].exists());
        assert!(states[1].exists());
        tx.send(Event::PeerClosed("test disconnect".into()))
            .unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(!states[1].exists());
    }

    #[test]
    fn ssh_connection_is_outbound_command_channel_only() {
        let args = ssh_args("cloud-alias", "laptop", "/opt/My Tools/summon").unwrap();
        for forbidden in ["-R", "-L", "-D", "-A", "-X", "-W"] {
            assert!(!args.iter().any(|arg| arg == forbidden));
        }
        assert!(args.iter().any(|arg| arg == "ClearAllForwardings=yes"));
        assert!(args.iter().any(|arg| arg == "ForwardAgent=no"));
        assert!(args
            .last()
            .unwrap()
            .starts_with("'/opt/My Tools/summon' remote-tools server"));
        assert!(ssh_args("-oProxyCommand=evil", "laptop", "summon").is_err());
    }
}
