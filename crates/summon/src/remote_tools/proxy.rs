use super::{registry, wire};
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::BufReader;
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;
use summon_core::remote_tools::{LocalReply, LocalRequest};

enum Event {
    Input(Value),
    Output(Value),
    InputClosed,
    OutputClosed,
}

pub(super) fn run(target: &str, working_dir: Option<String>) -> Result<()> {
    let expected = working_dir.clone();
    let mut socket = registry::connect(&registry::root()?, target)?;
    wire::write_json(&mut socket, &LocalRequest::Attach { working_dir })?;
    let mut reader = BufReader::new(socket.try_clone()?);
    match wire::read_json::<LocalReply>(&mut reader)?.context(summon_i18n::text!("rbd245a4a4d"))? {
        LocalReply::Ready { working_dir } => {
            verify_directory(expected.as_deref(), &working_dir)?;
            eprintln!("{}", summon_i18n::format!("r14ab13b3f8"));
        }
        LocalReply::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!(summon_i18n::format!("rc2a8aeb242")),
    }
    socket.set_read_timeout(None)?;
    let failed = Arc::new(AtomicBool::new(false));
    let peer = wire::writer(socket.try_clone()?, Arc::clone(&failed));
    let output = wire::writer(std::io::stdout(), Arc::clone(&failed));
    let (tx, rx) = sync_channel(wire::QUEUE_SIZE);
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        let mut input = BufReader::new(std::io::stdin());
        while let Ok(Some(message)) = wire::read_json::<Value>(&mut input) {
            if input_tx.send(Event::Input(message)).is_err() {
                return;
            }
        }
        let _ = input_tx.send(Event::InputClosed);
    });
    std::thread::spawn(move || {
        while let Ok(Some(message)) = wire::read_json::<Value>(&mut reader) {
            if tx.send(Event::Output(message)).is_err() {
                return;
            }
        }
        let _ = tx.send(Event::OutputClosed);
    });
    let shutdown = wire::shutdown_flag()?;
    let result = (|| {
        while !shutdown.load(Ordering::SeqCst) {
            anyhow::ensure!(
                !failed.load(Ordering::SeqCst),
                summon_i18n::format!("r3bb88651f8")
            );
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::Input(message)) => wire::send(&peer, message)?,
                Ok(Event::Output(message)) => wire::send(&output, message)?,
                Ok(Event::InputClosed) => return Ok(()),
                Ok(Event::OutputClosed) => {
                    anyhow::bail!(summon_i18n::format!("rb8391a096d"))
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
        Ok(())
    })();
    // 显式 shutdown 穿透 reader/writer 持有的 clone。
    let _ = socket.shutdown(Shutdown::Both);
    result
}

fn verify_directory(expected: Option<&str>, actual: &str) -> Result<()> {
    anyhow::ensure!(
        expected.is_none_or(|path| path == actual),
        summon_i18n::format!("r8bf1df0e9b")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_symlink_retargeting_between_binding_and_attach() {
        assert!(verify_directory(Some("/work/project"), "/other/project").is_err());
        assert!(verify_directory(Some("/work/project"), "/work/project").is_ok());
    }
}
