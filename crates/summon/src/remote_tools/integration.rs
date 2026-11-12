use super::registry;
use anyhow::{Context, Result};
use serde_json::json;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use summon_core::config::Config;
use summon_core::registry::Agent;
use summon_core::remote_tools::ExecutionBinding;

pub(crate) fn binding(agent: &Agent) -> Option<&ExecutionBinding> {
    agent
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.execution.as_ref())
}

fn validate(config: &Config, binding: &ExecutionBinding) -> Result<()> {
    binding.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        config.execution_targets.contains_key(&binding.target),
        summon_i18n::format!("r02f517d004", binding.target)
    );
    Ok(())
}

pub(crate) fn instructions(binding: &ExecutionBinding) -> String {
    summon_i18n::format!(
        "ra3dc160545",
        binding.target,
        binding
            .working_dir
            .as_deref()
            .unwrap_or(summon_i18n::text!("r433f8272b6"))
    )
}

pub(super) fn claude_args(executable: &Path, binding: &ExecutionBinding) -> Vec<OsString> {
    let mut proxy = vec![
        "remote-tools".to_string(),
        "proxy".to_string(),
        "--target".to_string(),
        binding.target.clone(),
    ];
    if let Some(directory) = &binding.working_dir {
        proxy.extend(["--working-dir".into(), directory.clone()]);
    }
    let config = json!({"mcpServers":{
        "execution":{"type":"stdio","command":executable,"args":proxy},
        "summon":{"type":"stdio","command":executable,"args":["remote-tools","platform"]}
    }});
    [
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        &config.to_string(),
        "--setting-sources",
        "",
        "--disable-slash-commands",
        "--no-chrome",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

pub(crate) fn launch_args(agent: &Agent, session: &str) -> Result<Vec<OsString>> {
    let Some(binding) = binding(agent) else {
        return Ok(Vec::new());
    };
    anyhow::ensure!(
        agent.cli_kind() == "claude",
        summon_i18n::format!("r2927d74e19")
    );
    validate(&Config::load(), binding)?;
    let status = registry::status(&registry::root()?, &binding.target)?;
    crate::observe::log::log(&summon_i18n::format!(
        "r42da506118",
        status.name,
        status.provider_version
    ));
    let saved: Option<ExecutionBinding> =
        serde_json::from_slice(&std::fs::read(binding_path(agent, session))?)?;
    let saved = saved.context(summon_i18n::text!("re3f1b7dce8"))?;
    Ok(claude_args(&std::env::current_exe()?, &saved))
}

pub(crate) fn validate_agent(agent: &Agent) -> Result<()> {
    if let Some(binding) = binding(agent) {
        anyhow::ensure!(
            agent.cli_kind() == "claude",
            summon_i18n::format!("r2927d74e19")
        );
        validate(&Config::load(), binding)?;
    }
    Ok(())
}

pub(crate) fn binding_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime/chats")
        .join(format!("{session}.execution.json"))
}

pub(crate) fn check_binding(
    agent: &Agent,
    session: &str,
    has_session: bool,
    has_record: bool,
) -> Result<()> {
    let configured = binding(agent);
    let path = binding_path(agent, session);
    if configured.is_none() && !path.exists() {
        return Ok(());
    }
    let wanted = if let Some(binding) = configured {
        anyhow::ensure!(
            agent.cli_kind() == "claude",
            summon_i18n::format!("r2927d74e19")
        );
        validate(&Config::load(), binding)?;
        Some(resolve_binding(binding)?)
    } else {
        None
    };
    check_binding_at(&path, wanted.as_ref(), has_session || has_record)
}

fn resolve_binding(binding: &ExecutionBinding) -> Result<ExecutionBinding> {
    resolve_binding_at(&registry::root()?, binding)
}

fn resolve_binding_at(root: &Path, binding: &ExecutionBinding) -> Result<ExecutionBinding> {
    Ok(ExecutionBinding {
        target: binding.target.clone(),
        working_dir: Some(registry::resolve_directory(
            root,
            &binding.target,
            binding.working_dir.clone(),
        )?),
    })
}

fn check_binding_at(path: &Path, wanted: Option<&ExecutionBinding>, occupied: bool) -> Result<()> {
    if !occupied {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::runtime::fsio::atomic_write(path, &serde_json::to_vec(&wanted)?)?;
        return Ok(());
    }
    let saved: Option<ExecutionBinding> = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context(summon_i18n::text!("r795a6f6389"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        saved.as_ref() == wanted,
        summon_i18n::format!("r2a2e4706cd")
    );
    Ok(())
}

pub(crate) fn fork_binding(agent: &Agent, source: Option<&str>, target: &str) -> Result<()> {
    match source {
        Some(session) => {
            check_binding(agent, session, true, true)?;
            let path = binding_path(agent, session);
            if path.exists() {
                let destination = binding_path(agent, target);
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::runtime::fsio::atomic_write(&destination, &std::fs::read(path)?)?;
            } else if binding_path(agent, target).exists() {
                check_binding_at(&binding_path(agent, target), None, false)?;
            }
        }
        None => {
            anyhow::ensure!(
                binding(agent).is_none(),
                summon_i18n::format!("ra734eacf7e")
            );
            if binding_path(agent, target).exists() {
                check_binding_at(&binding_path(agent, target), None, false)?;
            }
        }
    }
    Ok(())
}

pub(super) fn launch(
    config: &Config,
    binding: ExecutionBinding,
    claude: PathBuf,
    user_args: Vec<OsString>,
) -> Result<()> {
    validate(config, &binding)?;
    let binding = resolve_binding(&binding)?;
    let mut args = claude_args(&std::env::current_exe()?, &binding);
    append_user_args(&mut args, &binding, user_args);
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(claude).args(args).exec();
    Err(error.into())
}

fn append_user_args(
    args: &mut Vec<OsString>,
    binding: &ExecutionBinding,
    user_args: Vec<OsString>,
) {
    let explicit_prompt = user_args
        .iter()
        .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
        .any(|arg| {
            let arg = arg.to_string_lossy();
            [
                "--system-prompt",
                "--system-prompt-file",
                "--append-system-prompt",
                "--append-system-prompt-file",
            ]
            .contains(&arg.split('=').next().unwrap_or(""))
        });
    if !explicit_prompt {
        args.extend([
            "--append-system-prompt".into(),
            instructions(binding).into(),
        ]);
    }
    args.extend(user_args);
}

#[cfg(test)]
pub(crate) fn with_test_target(test: impl FnOnce()) {
    use super::wire;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use summon_core::remote_tools::{LocalReply, LocalRequest, Provider, TargetStatus};
    let registration = registry::Registration::bind(&registry::root().unwrap(), "laptop").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let peer = std::thread::spawn(move || {
        while !stopped.load(Ordering::Acquire) {
            match registration.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                        .unwrap();
                    let request = wire::read_json::<LocalRequest>(&mut std::io::BufReader::new(
                        stream.try_clone().unwrap(),
                    ))
                    .unwrap()
                    .unwrap();
                    let reply = match request {
                        LocalRequest::Status => LocalReply::Status {
                            status: TargetStatus {
                                name: "laptop".into(),
                                protocol: 1,
                                provider: Provider::ClaudeCode,
                                provider_version: "test-provider".into(),
                                working_dir: "/remote/project".into(),
                                sessions: 0,
                            },
                        },
                        LocalRequest::Attach { working_dir } => LocalReply::Ready {
                            working_dir: working_dir.unwrap_or_else(|| "/remote/project".into()),
                        },
                    };
                    wire::write_json(&mut stream, &reply).unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
    stop.store(true, Ordering::Release);
    peer.join().unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reconnected_default_directory_cannot_replace_the_saved_directory() {
        use super::super::wire;
        use summon_core::remote_tools::{LocalReply, LocalRequest};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("registry");
        let registration = registry::Registration::bind(&root, "laptop").unwrap();
        registration.listener.set_nonblocking(false).unwrap();
        let peer = std::thread::spawn(move || {
            for path in ["/work/a", "/work/b"] {
                let (mut stream, _) = registration.listener.accept().unwrap();
                stream.set_nonblocking(false).unwrap();
                assert!(matches!(
                    wire::read_json::<LocalRequest>(&mut std::io::BufReader::new(
                        stream.try_clone().unwrap()
                    ))
                    .unwrap(),
                    Some(LocalRequest::Attach { working_dir: None })
                ));
                wire::write_json(
                    &mut stream,
                    &LocalReply::Ready {
                        working_dir: path.into(),
                    },
                )
                .unwrap();
            }
        });
        let configured = ExecutionBinding {
            target: "laptop".into(),
            working_dir: None,
        };
        let first = resolve_binding_at(&root, &configured).unwrap();
        let saved = temp.path().join("saved.json");
        check_binding_at(&saved, Some(&first), false).unwrap();
        let changed = resolve_binding_at(&root, &configured).unwrap();
        assert!(check_binding_at(&saved, Some(&changed), true).is_err());
        peer.join().unwrap();
    }

    #[test]
    fn binding_changes_require_an_explicit_empty_session() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("binding.json");
        let mut binding = ExecutionBinding {
            target: "laptop".into(),
            working_dir: None,
        };
        assert!(check_binding_at(&path, Some(&binding), true).is_err());
        check_binding_at(&path, Some(&binding), false).unwrap();
        check_binding_at(&path, Some(&binding), true).unwrap();
        binding.working_dir = Some("/another".into());
        assert!(check_binding_at(&path, Some(&binding), true).is_err());
        assert!(check_binding_at(&path, None, true).is_err());
        check_binding_at(&path, None, false).unwrap();
        check_binding_at(&path, None, true).unwrap();
        std::fs::write(&path, b"broken").unwrap();
        assert!(check_binding_at(&path, None, true).is_err());
    }

    #[test]
    fn launch_preserves_native_arguments_and_prompt_file() {
        let binding = ExecutionBinding {
            target: "laptop".into(),
            working_dir: None,
        };
        let mut args = claude_args(Path::new("/opt/My Tools/summon"), &binding);
        let native: Vec<OsString> = [
            "--resume",
            "sid",
            "--append-system-prompt-file",
            "/a b/primer",
            "--future-option=x",
            "--",
            "prompt $HOME",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        append_user_args(&mut args, &binding, native.clone());
        assert_eq!(&args[args.len() - native.len()..], native.as_slice());
        assert!(!args.iter().any(|arg| arg == "--append-system-prompt"));
        let config = args.iter().position(|arg| arg == "--mcp-config").unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(args[config + 1].to_str().unwrap()).unwrap();
        assert_eq!(
            parsed["mcpServers"]["execution"]["command"],
            "/opt/My Tools/summon"
        );
        assert!(parsed["mcpServers"].get("summon").is_some());
    }
}
