use super::wire;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

pub(super) fn validate_args(args: &[String]) -> Result<()> {
    anyhow::ensure!(
        args.len() <= 64 && args.iter().all(|arg| arg.len() <= 65536),
        summon_i18n::format!("rb62549807a")
    );
    let name = args
        .first()
        .map(String::as_str)
        .context(summon_i18n::text!("rb69a845ba3"))?;
    anyhow::ensure!(
        [
            "react",
            "preview",
            "send-image",
            "remind",
            "chats",
            "chat-id",
            "ask",
            "guide"
        ]
        .contains(&name),
        summon_i18n::format!("red087fc622")
    );
    if name != "guide" {
        anyhow::ensure!(
            args.windows(2)
                .any(|pair| pair[0] == "--turn-id" && !pair[1].is_empty()),
            summon_i18n::format!("r8111dff0ed")
        );
        anyhow::ensure!(
            !args
                .iter()
                .any(|arg| arg == "--agent" || arg.starts_with("--agent=")),
            summon_i18n::format!("r9d75698c44")
        );
    }
    Ok(())
}

fn invoke(args: Vec<String>, shutdown: &AtomicBool) -> Result<String> {
    validate_args(&args)?;
    let mut output = tempfile::tempfile()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output.try_clone()?);
    let process = super::process::ProcessGroup::spawn(&mut command)?;
    let deadline = Instant::now() + Duration::from_secs(12 * 3600);
    let status = loop {
        anyhow::ensure!(
            !shutdown.load(Ordering::SeqCst),
            summon_i18n::format!("rcd3a078fab")
        );
        anyhow::ensure!(
            Instant::now() < deadline,
            summon_i18n::format!("r2aafcc35f3")
        );
        anyhow::ensure!(
            output.metadata()?.len() < (summon_core::remote_tools::MAX_FRAME_BYTES / 2) as u64,
            summon_i18n::format!("r05811290d5")
        );
        if let Some(status) = process.exit_code()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    use std::io::{Read, Seek};
    output.rewind()?;
    let mut bytes = Vec::new();
    output
        .take((summon_core::remote_tools::MAX_FRAME_BYTES / 2) as u64)
        .read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    anyhow::ensure!(status == 0, "{text}");
    Ok(text)
}

pub(super) fn run() -> Result<()> {
    let shutdown = wire::shutdown_flag()?;
    let closed = Arc::clone(&shutdown);
    let (tx, rx) = mpsc::sync_channel(wire::QUEUE_SIZE);
    std::thread::spawn(move || {
        let mut input = BufReader::new(std::io::stdin());
        while let Ok(Some(message)) = wire::read_json::<Value>(&mut input) {
            if tx.try_send(message).is_err() {
                break;
            }
        }
        closed.store(true, Ordering::SeqCst);
    });
    let mut output = std::io::stdout();
    while !shutdown.load(Ordering::SeqCst) {
        let message = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(message) => message,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let Some(id) = message.get("id") else {
            continue;
        };
        let result = match message["method"].as_str() {
            Some("initialize") => {
                json!({"protocolVersion":message["params"]["protocolVersion"],"capabilities":{"tools":{}},
                                         "serverInfo":{"name":"summon-platform","version":crate::version::VERSION}})
            }
            Some("ping") => json!({}),
            Some("tools/list") => {
                json!({"tools":[{"name":"invoke","description":summon_i18n::text!("r8f8c1f1bb9"),
                "inputSchema":{"type":"object","properties":{"argv":{"type":"array","items":{"type":"string"},"maxItems":64}},"required":["argv"],"additionalProperties":false}}]})
            }
            Some("tools/call") if message["params"]["name"] == "invoke" => {
                let result = serde_json::from_value::<Vec<String>>(
                    message["params"]["arguments"]["argv"].clone(),
                )
                .map_err(anyhow::Error::from)
                .and_then(|args| invoke(args, &shutdown));
                match result {
                    Ok(text) => json!({"content":[{"type":"text","text":text}]}),
                    Err(error) => {
                        json!({"isError":true,"content":[{"type":"text","text":error.to_string()}]})
                    }
                }
            }
            _ => {
                wire::write_json(
                    &mut output,
                    &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":summon_i18n::text!("rcdefe1b9f6")}}),
                )?;
                continue;
            }
        };
        wire::write_json(
            &mut output,
            &json!({"jsonrpc":"2.0","id":id,"result":result}),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_requires_turn_identity_and_excludes_shell() {
        let args = |values: &[&str]| values.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(validate_args(&args(&["react", "--turn-id", "token", "OK"])).is_ok());
        assert!(validate_args(&args(&["guide", "protocol"])).is_ok());
        assert!(validate_args(&args(&["react", "OK"])).is_err());
        assert!(validate_args(&args(&["chats", "--turn-id", "token", "--agent=other"])).is_err());
        assert!(validate_args(&args(&["daemon", "stop", "--turn-id", "token"])).is_err());
        assert!(validate_args(&args(&["sh", "-c", "echo secret"])).is_err());
    }
}
