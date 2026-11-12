use anyhow::{Context, Result};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// 仅持有本模块创建的独立进程组；不接收外部 PID，也不触碰 tmux。
pub(super) struct ProcessGroup {
    pub child: Child,
}

impl ProcessGroup {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        use std::os::unix::process::CommandExt;
        Ok(Self {
            child: command
                .process_group(0)
                .spawn()
                .context(summon_i18n::text!("ra25ce157ac"))?,
        })
    }

    pub fn exit_code(&self) -> Result<Option<i32>> {
        // WNOWAIT 保留 leader，直到 Drop 清理整组；macOS 与 Linux 都提供 POSIX waitid。
        let mut info = unsafe { std::mem::zeroed::<nix::libc::siginfo_t>() };
        let result = unsafe {
            nix::libc::waitid(
                nix::libc::P_PID,
                self.child.id(),
                &mut info,
                nix::libc::WEXITED | nix::libc::WNOHANG | nix::libc::WNOWAIT,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        Ok(Some(if info.si_code == nix::libc::CLD_EXITED {
            unsafe { info.si_status() }
        } else {
            128 + unsafe { info.si_status() }
        }))
    }
}

pub(super) fn temporary_state(prefix: &str) -> Result<TempDir> {
    use std::os::unix::fs::PermissionsExt;
    Ok(tempfile::Builder::new()
        .prefix(prefix)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?)
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let pid = Pid::from_raw(self.child.id() as i32);
        // 先终止整组，再回收 leader；保持 leader 未 reap，避免清理窗口中的 PID 复用。
        let _ = killpg(pid, Signal::SIGTERM);
        std::thread::sleep(Duration::from_millis(100));
        let _ = killpg(pid, Signal::SIGKILL);
        let _ = self.child.wait();
    }
}

pub(super) fn provider_command(executable: &Path, work_dir: &Path, state: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .args([
            "--bare",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "mcp",
            "serve",
        ])
        .current_dir(work_dir)
        .env_clear()
        .env("HOME", state)
        .env("CLAUDE_CONFIG_DIR", state)
        .env("TMPDIR", state)
        .env("CLAUDE_CODE_SHELL", "/bin/bash")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

pub(super) fn provider_version(executable: &Path) -> Result<String> {
    let state = temporary_state("summon-tools-version-")?;
    let output = tempfile::tempfile()?;
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .env_clear()
        .env("HOME", state.path())
        .env("CLAUDE_CONFIG_DIR", state.path())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null());
    let child = ProcessGroup::spawn(&mut command)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.exit_code()? {
            Some(0) => break,
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            other => anyhow::bail!(summon_i18n::format!("r9658823bd6")),
        }
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut output = output;
    output.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    output.take(4096).read_to_string(&mut text)?;
    anyhow::ensure!(!text.trim().is_empty(), summon_i18n::format!("rdd65d99cf1"));
    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_exit_preserves_the_shell_status() {
        let child =
            ProcessGroup::spawn(Command::new("/bin/sh").args(["-c", "kill -TERM $$"])).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(code) = child.exit_code().unwrap() {
                assert_eq!(code, 143);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn provider_has_only_ephemeral_identity_environment() {
        let cmd = provider_command(
            Path::new("/bin/claude"),
            Path::new("/work"),
            Path::new("/temporary"),
        );
        let keys: Vec<_> = cmd
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert!(keys.contains(&"HOME".into()));
        assert!(keys.contains(&"CLAUDE_CONFIG_DIR".into()));
        assert!(!keys.iter().any(|key| key.starts_with("ANTHROPIC")
            || key.starts_with("SUMMON")
            || key.starts_with("SSH")));
        assert!(cmd.get_args().any(|arg| arg == "--bare"));
    }
}
