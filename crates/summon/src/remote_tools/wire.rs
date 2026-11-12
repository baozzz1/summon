use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::io::{BufRead, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use summon_core::remote_tools::MAX_FRAME_BYTES;

pub(super) const QUEUE_SIZE: usize = 8;

pub(super) fn read_json<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<Option<T>> {
    let mut bytes = Vec::new();
    let size = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if size == 0 {
        return Ok(None);
    }
    anyhow::ensure!(size <= MAX_FRAME_BYTES, summon_i18n::format!("rc6d3c6723e"));
    anyhow::ensure!(
        bytes.last() == Some(&b'\n'),
        summon_i18n::format!("r2de4c88d8f")
    );
    Ok(Some(
        serde_json::from_slice(&bytes).context(summon_i18n::text!("rc39b121888"))?,
    ))
}

pub(super) fn write_json(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() < MAX_FRAME_BYTES,
        summon_i18n::format!("rc6d3c6723e")
    );
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// 有界写队列使背压变为明确失败；SSH/工具管道阻塞不能拖住清理线程。
pub(super) fn writer<T: Serialize + Send + 'static>(
    mut output: impl Write + Send + 'static,
    failed: Arc<AtomicBool>,
) -> SyncSender<T> {
    let (tx, rx) = sync_channel(QUEUE_SIZE);
    std::thread::spawn(move || {
        for value in rx {
            if write_json(&mut output, &value).is_err() {
                failed.store(true, Ordering::SeqCst);
                break;
            }
        }
    });
    tx
}

pub(super) fn send<T>(writer: &SyncSender<T>, value: T) -> Result<()> {
    writer
        .try_send(value)
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r8dba2aaad7")))
}

pub(super) fn shutdown_flag() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&flag))?;
    }
    Ok(flag)
}

pub(super) fn check_protocol(protocol: u32) -> Result<()> {
    if protocol != summon_core::remote_tools::PROTOCOL_VERSION {
        bail!(summon_i18n::format!("r52693e0d9e"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn bounded_frames_preserve_message_boundaries() {
        let mut reader = std::io::Cursor::new(b"{\"id\":1}\n{\"id\":2}\n");
        assert_eq!(
            read_json::<Value>(&mut reader).unwrap(),
            Some(json!({"id":1}))
        );
        assert_eq!(
            read_json::<Value>(&mut reader).unwrap(),
            Some(json!({"id":2}))
        );
        assert!(read_json::<Value>(&mut reader).unwrap().is_none());
    }

    #[test]
    fn reject_truncated_and_oversized_frames() {
        assert!(read_json::<Value>(&mut std::io::Cursor::new(b"{}")).is_err());
        let oversized = vec![b' '; MAX_FRAME_BYTES + 1];
        assert!(read_json::<Value>(&mut std::io::Cursor::new(oversized)).is_err());
        assert!(check_protocol(0).is_err());
    }
}
