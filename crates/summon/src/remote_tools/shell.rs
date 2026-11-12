//! Claude 的 shell prefix 把命令交给连接内的监督线程；子进程组的所有者始终是客户端。

use super::{process::ProcessGroup, wire};
use anyhow::{Context, Result};
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{BufReader, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_COMMANDS: usize = 64;

#[derive(Serialize, Deserialize)]
struct Request {
    command: String,
    cwd: PathBuf,
    env: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    code: i32,
    error: Option<String>,
}

pub(super) struct ShellLease {
    supervisor: Option<ShellSupervisor>,
    socket: OwnedSocket,
    started: Arc<AtomicUsize>,
}

struct ShellSupervisor {
    stopped: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

struct OwnedSocket {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl OwnedSocket {
    fn bind(path: PathBuf) -> Result<(Self, UnixListener)> {
        let listener = UnixListener::bind(&path)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok((
            Self {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            listener,
        ))
    }
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            if metadata.dev() == self.device && metadata.ino() == self.inode {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

impl ShellLease {
    pub fn start(state: &Path) -> Result<Self> {
        let path = state.join("shell.sock");
        let (socket, listener) = OwnedSocket::bind(path)?;
        std::fs::set_permissions(&socket.path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let started = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&started);
        let thread = std::thread::Builder::new().spawn(move || {
            if let Err(error) = serve(listener, stop, count) {
                eprintln!("{}", summon_i18n::format!("r2c07eeea47"));
            }
        })?;
        Ok(Self {
            supervisor: Some(ShellSupervisor { stopped, thread }),
            socket,
            started,
        })
    }

    pub fn has_started_command(&self) -> bool {
        self.started.load(Ordering::SeqCst) > 0
    }

    pub fn prefix(&self) -> Result<String> {
        let quote = summon_core::launch_env::sh_single_quote;
        let script = format!(
            "#!/bin/sh\nexec {} remote-tools shell --socket {} -- \"$@\"\n",
            quote(&std::env::current_exe()?.to_string_lossy()),
            quote(&self.socket.path.to_string_lossy())
        );
        let path = self.socket.path.with_file_name("shell-prefix");
        std::fs::write(&path, script)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        Ok(path.to_string_lossy().into_owned())
    }
}

impl Drop for ShellLease {
    fn drop(&mut self) {
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.stopped.store(true, Ordering::SeqCst);
            let _ = supervisor.thread.join();
        }
    }
}

struct Running {
    process: ProcessGroup,
    state: CommandState,
}

enum CommandState {
    Connected(UnixStream),
    SupervisingDescendants,
}

enum CommandPoll {
    Pending,
    LeaderExited,
    Disconnected,
}

impl Running {
    fn poll(&mut self) -> Result<CommandPoll> {
        let CommandState::Connected(caller) = &mut self.state else {
            return Ok(CommandPoll::Pending);
        };
        let mut byte = [0_u8];
        match nix::sys::socket::recv(
            caller.as_raw_fd(),
            &mut byte,
            MsgFlags::MSG_PEEK | MsgFlags::MSG_DONTWAIT,
        ) {
            Ok(0) => return Ok(CommandPoll::Disconnected),
            Err(nix::errno::Errno::EAGAIN) => {}
            Err(_) => return Ok(CommandPoll::Disconnected),
            _ => {}
        }
        let Some(code) = self.process.exit_code()? else {
            return Ok(CommandPoll::Pending);
        };
        if wire::write_json(caller, &Reply { code, error: None }).is_err() {
            return Ok(CommandPoll::Disconnected);
        }
        self.state = CommandState::SupervisingDescendants;
        Ok(CommandPoll::LeaderExited)
    }

    fn needs_supervision(&self, groups: &HashSet<u32>) -> bool {
        matches!(self.state, CommandState::Connected(_))
            || groups.contains(&self.process.child.id())
    }
}

fn receive_stdio(stream: &UnixStream) -> Result<Vec<OwnedFd>> {
    let mut byte = [0_u8];
    let mut slices = [IoSliceMut::new(&mut byte)];
    let mut ancillary = nix::cmsg_space!([RawFd; 3]);
    let message = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut slices,
        Some(&mut ancillary),
        MsgFlags::empty(),
    )?;
    let mut descriptors = Vec::new();
    for control in message.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = control {
            for fd in fds {
                descriptors.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    anyhow::ensure!(
        descriptors.len() == 3 && !message.flags.contains(MsgFlags::MSG_CTRUNC),
        summon_i18n::format!("rc819493c85")
    );
    for fd in &descriptors {
        // recvmsg 的 CLOEXEC 标志并非所有目标平台都有；在 spawn 前显式设置。
        nix::fcntl::fcntl(
            fd,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )?;
    }
    Ok(descriptors)
}

fn accept_command(stream: &UnixStream) -> Result<ProcessGroup> {
    let mut stdio = receive_stdio(stream)?.into_iter();
    let request = wire::read_json::<Request>(&mut BufReader::new(stream.try_clone()?))?
        .context(summon_i18n::text!("r403a6d122c"))?;
    let mut command = Command::new("/bin/bash");
    command
        .arg("-c")
        .arg(request.command)
        .current_dir(request.cwd)
        .env_clear()
        .envs(request.env)
        .stdin(Stdio::from(
            stdio.next().context(summon_i18n::text!("rf9dd64f984"))?,
        ))
        .stdout(Stdio::from(
            stdio.next().context(summon_i18n::text!("re954cc86f9"))?,
        ))
        .stderr(Stdio::from(
            stdio.next().context(summon_i18n::text!("r6698453b64"))?,
        ));
    ProcessGroup::spawn(&mut command)
}

fn groups_with_children() -> Result<HashSet<u32>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,pgid=,stat="])
        .output()?;
    anyhow::ensure!(output.status.success(), summon_i18n::format!("r8a02c315c6"));
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: u32 = fields.next()?.parse().ok()?;
            let group: u32 = fields.next()?.parse().ok()?;
            let state = fields.next()?;
            (pid != group && !state.starts_with('Z')).then_some(group)
        })
        .collect())
}

fn serve(
    listener: UnixListener,
    stopped: Arc<AtomicBool>,
    started: Arc<AtomicUsize>,
) -> Result<()> {
    let mut running: Vec<Running> = Vec::new();
    let mut last_scan = Instant::now();
    while !stopped.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                let child = if running.len() < MAX_COMMANDS {
                    accept_command(&stream)
                } else {
                    Err(anyhow::anyhow!(summon_i18n::format!("r73a5867002")))
                };
                match child {
                    Ok(process) => {
                        started.fetch_add(1, Ordering::SeqCst);
                        running.push(Running {
                            process,
                            state: CommandState::Connected(stream),
                        });
                    }
                    Err(error) => {
                        let _ = wire::write_json(
                            &mut stream,
                            &Reply {
                                code: 1,
                                error: Some(error.to_string()),
                            },
                        );
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        let mut completed = false;
        for index in (0..running.len()).rev() {
            match running[index].poll()? {
                CommandPoll::Pending => {}
                CommandPoll::LeaderExited => completed = true,
                CommandPoll::Disconnected => {
                    running.swap_remove(index);
                }
            }
        }
        if completed || last_scan.elapsed() > Duration::from_secs(1) {
            let groups = groups_with_children()?;
            // 未 reap 的 leader 固定 PGID，后台子进程结束后即可安全释放监督记录。
            running.retain(|command| command.needs_supervision(&groups));
            last_scan = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

pub(super) fn run(path: &Path, args: Vec<String>) -> Result<()> {
    anyhow::ensure!(args.len() == 1, summon_i18n::format!("r030ba5676b"));
    let mut socket = UnixStream::connect(path).context(summon_i18n::text!("r32d805d639"))?;
    sendmsg::<()>(
        socket.as_raw_fd(),
        &[IoSlice::new(&[0])],
        &[ControlMessage::ScmRights(&[0, 1, 2])],
        MsgFlags::empty(),
        None,
    )?;
    wire::write_json(
        &mut socket,
        &Request {
            command: args[0].clone(),
            cwd: std::env::current_dir()?,
            env: std::env::vars().collect(),
        },
    )?;
    let reply = wire::read_json::<Reply>(&mut BufReader::new(socket))?
        .context(summon_i18n::text!("r96c01d623b"))?;
    if let Some(error) = reply.error {
        anyhow::bail!(error);
    }
    if reply.code != 0 {
        std::process::exit(reply.code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek};

    fn request(lease: &ShellLease, cwd: &Path, command: &str) -> (UnixStream, std::fs::File) {
        let mut socket = UnixStream::connect(&lease.socket.path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let input = std::fs::File::open("/dev/null").unwrap();
        let output = tempfile::tempfile().unwrap();
        sendmsg::<()>(
            socket.as_raw_fd(),
            &[IoSlice::new(&[0])],
            &[ControlMessage::ScmRights(&[
                input.as_raw_fd(),
                output.as_raw_fd(),
                output.as_raw_fd(),
            ])],
            MsgFlags::empty(),
            None,
        )
        .unwrap();
        wire::write_json(
            &mut socket,
            &Request {
                command: command.into(),
                cwd: cwd.into(),
                env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            },
        )
        .unwrap();
        (socket, output)
    }

    fn output_pid(output: &mut std::fs::File) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            output.rewind().unwrap();
            let mut text = String::new();
            output.read_to_string(&mut text).unwrap();
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
            assert!(Instant::now() < deadline, "shell did not publish PID");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_stopped(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let listing = Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&listing.stdout);
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "surviving process {pid}: {state}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn completed_command_replies_once_and_closes_the_connection() {
        let state = super::super::process::temporary_state("srt-").unwrap();
        let lease = ShellLease::start(state.path()).unwrap();
        assert!(!lease.has_started_command());
        let (socket, _) = request(&lease, state.path(), "exit 7");
        let mut reader = BufReader::new(socket);
        let reply = wire::read_json::<Reply>(&mut reader).unwrap().unwrap();
        assert_eq!(reply.code, 7);
        assert!(reply.error.is_none());
        assert!(wire::read_json::<Reply>(&mut reader).unwrap().is_none());
        assert!(lease.has_started_command());
    }

    #[test]
    fn caller_disconnect_reclaims_its_process_group() {
        let state = super::super::process::temporary_state("srt-").unwrap();
        let lease = ShellLease::start(state.path()).unwrap();
        let (socket, mut output) =
            request(&lease, state.path(), "printf '%s' \"$$\"; exec sleep 90");
        let pid = output_pid(&mut output);
        drop(socket);
        assert_stopped(pid);
    }

    #[test]
    fn lease_drop_reclaims_a_connected_command() {
        let state = super::super::process::temporary_state("srt-").unwrap();
        let lease = ShellLease::start(state.path()).unwrap();
        let (socket, mut output) =
            request(&lease, state.path(), "printf '%s' \"$$\"; exec sleep 90");
        let pid = output_pid(&mut output);
        drop(lease);
        assert_stopped(pid);
        assert!(wire::read_json::<Reply>(&mut BufReader::new(socket))
            .unwrap()
            .is_none());
    }

    #[test]
    fn completed_background_group_is_reaped_after_its_descendants_exit() {
        let state = super::super::process::temporary_state("srt-").unwrap();
        let lease = ShellLease::start(state.path()).unwrap();
        let (socket, mut output) = request(
            &lease,
            state.path(),
            "printf '%s' \"$$\"; sleep 0.3 & exit 0",
        );
        assert_eq!(
            wire::read_json::<Reply>(&mut BufReader::new(socket))
                .unwrap()
                .unwrap()
                .code,
            0
        );
        let pid = output_pid(&mut output);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
                == Err(nix::errno::Errno::ESRCH)
            {
                break;
            }
            assert!(Instant::now() < deadline, "leader {pid} was not reaped");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn spawn_failure_replies_without_starting_a_command_and_keeps_serving() {
        let state = super::super::process::temporary_state("srt-").unwrap();
        let lease = ShellLease::start(state.path()).unwrap();
        let (socket, _) = request(&lease, &state.path().join("missing"), "exit 0");
        let reply = wire::read_json::<Reply>(&mut BufReader::new(socket))
            .unwrap()
            .unwrap();
        assert_eq!(reply.code, 1);
        assert!(reply.error.is_some());
        assert!(!lease.has_started_command());
        let (socket, _) = request(&lease, state.path(), "exit 0");
        assert_eq!(
            wire::read_json::<Reply>(&mut BufReader::new(socket))
                .unwrap()
                .unwrap()
                .code,
            0
        );
    }

    #[test]
    fn reply_write_failure_reclaims_the_background_group() {
        let state = super::super::process::temporary_state("srt-").unwrap();
        let mut output = tempfile::tempfile().unwrap();
        let mut command = Command::new("/bin/bash");
        command
            .args(["-c", "sleep 90 & printf '%s' \"$!\""])
            .current_dir(state.path())
            .stdout(output.try_clone().unwrap());
        let process = ProcessGroup::spawn(&mut command).unwrap();
        let pid = output_pid(&mut output);
        let deadline = Instant::now() + Duration::from_secs(5);
        while process.exit_code().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let (server, _caller) = UnixStream::pair().unwrap();
        server.shutdown(std::net::Shutdown::Write).unwrap();
        let mut running = Running {
            process,
            state: CommandState::Connected(server),
        };
        assert!(matches!(running.poll().unwrap(), CommandPoll::Disconnected));
        drop(running);
        assert_stopped(pid);
    }

    #[test]
    fn lease_drop_preserves_a_replacement_socket() {
        let state = super::super::process::temporary_state("srt-").unwrap();
        let lease = ShellLease::start(state.path()).unwrap();
        let path = lease.socket.path.clone();
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        drop(lease);
        assert!(path.exists(), "lease deleted another listener's socket");
        drop(replacement);
    }

    #[test]
    fn background_command_lives_until_its_execution_lease_closes() {
        let state = super::super::process::temporary_state("srt-").unwrap();
        let lease = ShellLease::start(state.path()).unwrap();
        let mut socket = UnixStream::connect(&lease.socket.path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let input = std::fs::File::open("/dev/null").unwrap();
        let mut output = tempfile::tempfile().unwrap();
        sendmsg::<()>(
            socket.as_raw_fd(),
            &[IoSlice::new(&[0])],
            &[ControlMessage::ScmRights(&[
                input.as_raw_fd(),
                output.as_raw_fd(),
                output.as_raw_fd(),
            ])],
            MsgFlags::empty(),
            None,
        )
        .unwrap();
        wire::write_json(
            &mut socket,
            &Request {
                command: "sleep 90 & printf '%s' \"$!\"".into(),
                cwd: state.path().to_path_buf(),
                env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            },
        )
        .unwrap();
        let reply = wire::read_json::<Reply>(&mut BufReader::new(socket))
            .unwrap()
            .unwrap();
        assert_eq!(reply.code, 0);
        assert!(reply.error.is_none());
        output.rewind().unwrap();
        let mut text = String::new();
        output.read_to_string(&mut text).unwrap();
        let pid: i32 = text.parse().unwrap();
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok());
        drop(lease);
        let listing = Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&listing.stdout);
        assert!(
            state.trim().is_empty() || state.trim().starts_with('Z'),
            "surviving process {pid}: {state}"
        );
    }
}
