use super::{registry, wire};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufReader, ErrorKind};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use summon_core::remote_tools::{
    Frame, LocalReply, LocalRequest, TargetStatus, MAX_SESSIONS, PROTOCOL_VERSION,
};

enum Event {
    Peer(Frame),
    PeerClosed(String),
    Message(u64, Value),
    Closed(u64),
}

struct CloudSession {
    stream: UnixStream,
    ready: bool,
    opened_at: Instant,
}

impl Drop for CloudSession {
    fn drop(&mut self) {
        // shutdown 也唤醒持有 clone 的读线程，避免断线会话留下线程与 fd。
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

pub(super) fn run(name: &str, config: &summon_core::config::Config) -> Result<()> {
    let target = config
        .execution_targets
        .get(name)
        .context(summon_i18n::text!("r346e4b53c3"))?;
    run_with_io(
        name,
        target,
        &registry::root()?,
        std::io::stdin(),
        std::io::stdout(),
        wire::shutdown_flag()?,
    )
}

pub(super) fn run_with_io(
    name: &str,
    target: &summon_core::remote_tools::ExecutionTarget,
    root: &Path,
    input: impl std::io::Read + Send + 'static,
    output: impl std::io::Write + Send + 'static,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let (tx, rx) = sync_channel(wire::QUEUE_SIZE);
    let reader_tx = tx.clone();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(input);
        let error = loop {
            match wire::read_json::<Frame>(&mut reader) {
                Ok(Some(frame)) => {
                    if reader_tx.send(Event::Peer(frame)).is_err() {
                        return;
                    }
                }
                Ok(None) => break summon_i18n::text!("r096ff35496").to_string(),
                Err(error) => break error.to_string(),
            }
        };
        let _ = reader_tx.send(Event::PeerClosed(error));
    });
    let mut status = match rx
        .recv_timeout(Duration::from_secs(15))
        .context(summon_i18n::text!("r4f10d1f3e2"))?
    {
        Event::Peer(Frame::Hello {
            protocol,
            provider,
            provider_version,
            working_dir,
        }) => {
            wire::check_protocol(protocol)?;
            anyhow::ensure!(
                provider == target.provider,
                summon_i18n::format!("rb7999a7b70")
            );
            summon_core::remote_tools::validate_working_dir(&working_dir)
                .map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                provider_version.len() <= 4096,
                summon_i18n::format!("rf825e26e2e")
            );
            TargetStatus {
                name: name.to_string(),
                protocol,
                provider,
                provider_version,
                working_dir,
                sessions: 0,
            }
        }
        Event::PeerClosed(error) => anyhow::bail!(error),
        _ => anyhow::bail!(summon_i18n::format!("r056ad35702")),
    };
    let registration = registry::Registration::bind(root, name)?;
    registration.publish(&status)?;
    let failed = Arc::new(AtomicBool::new(false));
    let peer = wire::writer(output, Arc::clone(&failed));
    wire::send(
        &peer,
        Frame::Welcome {
            protocol: PROTOCOL_VERSION,
        },
    )?;
    let mut sessions: HashMap<u64, CloudSession> = HashMap::new();
    let mut next_id = 1_u64;
    while !shutdown.load(Ordering::SeqCst) {
        anyhow::ensure!(
            !failed.load(Ordering::SeqCst),
            summon_i18n::format!("r3a73d9f04a")
        );
        // 每拍限流 accept，已接会话和设备失联事件始终有机会被消费。
        for _ in 0..MAX_SESSIONS {
            let (mut stream, _) = match registration.listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            };
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_millis(250)))?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            let mut reader = BufReader::new(stream.try_clone()?);
            let request = wire::read_json::<LocalRequest>(&mut reader);
            match request {
                Ok(Some(LocalRequest::Status)) => {
                    status.sessions = sessions.len();
                    let _ = wire::write_json(
                        &mut stream,
                        &LocalReply::Status {
                            status: status.clone(),
                        },
                    );
                }
                Ok(Some(LocalRequest::Attach { working_dir })) if sessions.len() < MAX_SESSIONS => {
                    if let Some(path) = &working_dir {
                        if let Err(error) = summon_core::remote_tools::validate_working_dir(path) {
                            let _ = wire::write_json(
                                &mut stream,
                                &LocalReply::Error { message: error },
                            );
                            continue;
                        }
                    }
                    stream.set_read_timeout(None)?;
                    let id = next_id;
                    next_id = next_id
                        .checked_add(1)
                        .context(summon_i18n::text!("r6e02f37bed"))?;
                    sessions.insert(
                        id,
                        CloudSession {
                            stream,
                            ready: false,
                            opened_at: Instant::now(),
                        },
                    );
                    read_session(id, reader, tx.clone());
                    wire::send(&peer, Frame::Open { id, working_dir })?;
                }
                _ => {
                    let _ = wire::write_json(
                        &mut stream,
                        &LocalReply::Error {
                            message: summon_i18n::text!("r91b1275656").into(),
                        },
                    );
                }
            }
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(Event::Peer(Frame::Opened { id, working_dir })) => {
                if let Some(session) = sessions.get_mut(&id) {
                    if session.ready
                        || wire::write_json(&mut session.stream, &LocalReply::Ready { working_dir })
                            .is_err()
                    {
                        close_session(id, &mut sessions, &peer)?;
                    } else {
                        session.ready = true;
                    }
                } else {
                    wire::send(&peer, Frame::Close { id, error: None })?;
                }
            }
            Ok(Event::Peer(Frame::Data { id, message })) => {
                if let Some(session) = sessions.get_mut(&id) {
                    if !session.ready || wire::write_json(&mut session.stream, &message).is_err() {
                        close_session(id, &mut sessions, &peer)?;
                    }
                }
            }
            Ok(Event::Peer(Frame::Close { id, error })) => {
                if let Some(mut session) = sessions.remove(&id) {
                    if !session.ready {
                        let _ = wire::write_json(
                            &mut session.stream,
                            &LocalReply::Error {
                                message: error
                                    .unwrap_or_else(|| summon_i18n::text!("r537e5ac251").into()),
                            },
                        );
                    }
                }
            }
            Ok(Event::Message(id, message)) => {
                if sessions.get(&id).is_some_and(|session| session.ready) {
                    wire::send(&peer, Frame::Data { id, message })?;
                } else {
                    close_session(id, &mut sessions, &peer)?;
                }
            }
            Ok(Event::Closed(id)) => close_session(id, &mut sessions, &peer)?,
            Ok(Event::PeerClosed(error)) => anyhow::bail!(error),
            Ok(Event::Peer(_)) => anyhow::bail!(summon_i18n::format!("rbf4e92aa8c")),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!(summon_i18n::format!("rc42a68a04a"))
            }
        }
        let timed_out: Vec<_> = sessions
            .iter()
            .filter(|(_, session)| {
                !session.ready && session.opened_at.elapsed() > Duration::from_secs(15)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in timed_out {
            close_session(id, &mut sessions, &peer)?;
        }
    }
    Ok(())
}

fn read_session(id: u64, mut reader: BufReader<UnixStream>, tx: SyncSender<Event>) {
    std::thread::spawn(move || {
        while let Ok(Some(message)) = wire::read_json::<Value>(&mut reader) {
            if tx.send(Event::Message(id, message)).is_err() {
                return;
            }
        }
        let _ = tx.send(Event::Closed(id));
    });
}

fn close_session(
    id: u64,
    sessions: &mut HashMap<u64, CloudSession>,
    peer: &SyncSender<Frame>,
) -> Result<()> {
    if sessions.remove(&id).is_some() {
        wire::send(peer, Frame::Close { id, error: None })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use summon_core::remote_tools::{ExecutionTarget, Provider};

    #[test]
    fn multiplexes_without_crosstalk_and_disconnect_removes_registration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("registry");
        let worker_root = root.clone();
        let (mut peer, remote) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let worker = std::thread::spawn(move || {
            run_with_io(
                "laptop",
                &ExecutionTarget {
                    provider: Provider::ClaudeCode,
                },
                &worker_root,
                remote.try_clone().unwrap(),
                remote,
                Arc::new(AtomicBool::new(false)),
            )
        });
        wire::write_json(
            &mut peer,
            &Frame::Hello {
                protocol: PROTOCOL_VERSION,
                provider: Provider::ClaudeCode,
                provider_version: "test".into(),
                working_dir: "/work".into(),
            },
        )
        .unwrap();
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        assert!(matches!(
            wire::read_json::<Frame>(&mut reader).unwrap(),
            Some(Frame::Welcome { .. })
        ));
        let mut sessions = Vec::new();
        for id in [1, 2] {
            let mut stream = registry::connect(&root, "laptop").unwrap();
            wire::write_json(&mut stream, &LocalRequest::Attach { working_dir: None }).unwrap();
            assert!(
                matches!(wire::read_json::<Frame>(&mut reader).unwrap(), Some(Frame::Open { id:got, .. }) if got == id)
            );
            wire::write_json(
                &mut peer,
                &Frame::Opened {
                    id,
                    working_dir: "/work".into(),
                },
            )
            .unwrap();
            let mut client = BufReader::new(stream);
            let reply = wire::read_json::<LocalReply>(&mut client).unwrap();
            assert!(matches!(reply, Some(LocalReply::Ready { .. })), "{reply:?}");
            sessions.push(client);
        }
        assert_eq!(registry::status(&root, "laptop").unwrap().sessions, 2);
        for (index, client) in sessions.iter_mut().enumerate() {
            let message = json!({"id":index,"method":"ping"});
            wire::write_json(client.get_mut(), &message).unwrap();
            let Some(Frame::Data { id, message: got }) =
                wire::read_json::<Frame>(&mut reader).unwrap()
            else {
                panic!("expected data");
            };
            assert_eq!(id, index as u64 + 1);
            assert_eq!(got, message);
            wire::write_json(
                &mut peer,
                &Frame::Data {
                    id,
                    message: json!({"id":index,"result":{}}),
                },
            )
            .unwrap();
            assert_eq!(
                wire::read_json::<Value>(client).unwrap().unwrap()["id"],
                index
            );
        }
        peer.shutdown(Shutdown::Both).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(!registry::socket_path(&root, "laptop").unwrap().exists());
        for client in &mut sessions {
            assert!(wire::read_json::<Value>(client).unwrap().is_none());
        }
    }
}
