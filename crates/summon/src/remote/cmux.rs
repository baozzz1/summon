use anyhow::{bail, Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use summon_core::remote::{parse_claude_hook_sessions, CmuxHookSessions};

use crate::observe::log::log;

const CMUX_PROGRAM: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";
const CMUX_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

pub(crate) trait CmuxPort: Send + Sync {
    fn ping(&self) -> Result<()>;
    fn read_screen(&self, surface: &str) -> Result<String>;
    fn send_text(&self, surface: &str, text: &str) -> Result<()>;
    fn send_key(&self, surface: &str, key: &str) -> Result<()>;
}

pub(crate) struct CmuxCli {
    program: PathBuf,
    timeout: Duration,
}

pub(crate) fn cmux_args_send_text(surface: &str, text: &str) -> Vec<String> {
    ["send", "--surface", surface, "--", text]
        .map(String::from)
        .to_vec()
}

impl CmuxCli {
    pub(crate) fn installed() -> Self {
        Self::with_program(PathBuf::from(CMUX_PROGRAM), CMUX_TIMEOUT)
    }

    pub(crate) fn with_program(program: PathBuf, timeout: Duration) -> Self {
        Self { program, timeout }
    }

    /// 跑一次 cmux CLI。密码由 CLI 自己从 cmux 设置里读,这里不传 `--password`、
    /// 也不设 `CMUX_SOCKET_PASSWORD`。
    fn run(&self, args: &[String]) -> Result<String> {
        let mut stdout = tempfile::tempfile().context(summon_i18n::text!("r0857f3d58b"))?;
        let mut stderr = tempfile::tempfile().context(summon_i18n::text!("rc5d49c1065"))?;
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .env("CMUX_QUIET", "1")
            .env_remove("CMUX_SOCKET_PASSWORD")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?));
        let mut child = crate::runtime::process::retry_text_file_busy(|| command.spawn())
            .with_context(|| summon_i18n::format!("rcebdeabf6e", self.program.display()))?;
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .context(summon_i18n::text!("r5d7b4ce5f8"))?
            {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(summon_i18n::format!(
                    "r70e6480f0d",
                    args.first().map(String::as_str).unwrap_or("")
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let read = |file: &mut std::fs::File| -> Result<String> {
            file.seek(SeekFrom::Start(0))?;
            let mut buf = Vec::new();
            file.take(MAX_OUTPUT_BYTES).read_to_end(&mut buf)?;
            Ok(String::from_utf8_lossy(&buf).into_owned())
        };
        let out = read(&mut stdout)?;
        if !status.success() {
            bail!(summon_i18n::format!(
                "r214abe0080",
                args.first().map(String::as_str).unwrap_or(""),
                read(&mut stderr)?.trim()
            ));
        }
        Ok(out)
    }
}

impl CmuxPort for CmuxCli {
    fn ping(&self) -> Result<()> {
        self.run(&["ping".into()]).map(|_| ())
    }

    fn read_screen(&self, surface: &str) -> Result<String> {
        self.run(&["read-screen".into(), "--surface".into(), surface.into()])
    }

    fn send_text(&self, surface: &str, text: &str) -> Result<()> {
        self.run(&cmux_args_send_text(surface, text)).map(|_| ())
    }

    fn send_key(&self, surface: &str, key: &str) -> Result<()> {
        self.run(&[
            "send-key".into(),
            "--surface".into(),
            surface.into(),
            key.into(),
        ])
        .map(|_| ())
    }
}

/// cmux 报错是否确认是鉴权失败(socket 密码模式没开、密码不对):只有这时才值得让人去查
/// 密码设置,超时、窗口不存在之类照实说。
pub(crate) fn is_auth_failure(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    text.contains("access denied") || text.contains("password")
}

pub(crate) fn cmux_sessions_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".cmuxterm/claude-hook-sessions.json"))
}

pub(crate) fn load_cmux_sessions() -> Result<CmuxHookSessions> {
    let Some(path) = cmux_sessions_path() else {
        return Ok(CmuxHookSessions::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let parsed = parse_claude_hook_sessions(&text)
                .with_context(|| summon_i18n::format!("r7a19386521", path.display()))?;
            if !parsed.skipped.is_empty() {
                log(&summon_i18n::format!("r078f549faf", parsed.skipped));
            }
            Ok(parsed)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(CmuxHookSessions::default())
        }
        Err(error) => {
            Err(error).with_context(|| summon_i18n::format!("r6da9ed443d", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_text_separates_text_from_flags() {
        assert_eq!(
            cmux_args_send_text("S-1", "-rf 看看"),
            vec!["send", "--surface", "S-1", "--", "-rf 看看"]
        );
    }

    fn script(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("cmux");
        crate::test_support::write_test_executable(&path, &format!("#!/bin/sh\n{body}\n"));
        path
    }

    #[test]
    fn read_screen_returns_stdout_and_passes_quiet_env() {
        let dir = tempfile::tempdir().unwrap();
        let cli = CmuxCli::with_program(
            script(dir.path(), r#"printf '%s|%s|%s' "$1" "$3" "$CMUX_QUIET""#),
            Duration::from_secs(10),
        );
        assert_eq!(cli.read_screen("S-9").unwrap(), "read-screen|S-9|1");
    }

    #[test]
    fn socket_password_from_the_daemon_environment_is_not_passed_on() {
        // 改的是进程全局环境,与其它改环境的测试共用一把锁。
        let _guard = crate::test_support::process_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let cli = CmuxCli::with_program(
            script(dir.path(), r#"printf '%s' "${CMUX_SOCKET_PASSWORD-unset}""#),
            Duration::from_secs(10),
        );
        std::env::set_var("CMUX_SOCKET_PASSWORD", "from-the-daemon-env");
        let seen = cli.read_screen("S-1");
        std::env::remove_var("CMUX_SOCKET_PASSWORD");
        assert_eq!(
            seen.unwrap(),
            "unset",
            "密码只由 cmux CLI 从它自己的设置里读,daemon 环境里的不能传下去"
        );
    }

    #[test]
    fn only_access_denied_or_password_errors_count_as_auth_failures() {
        assert!(is_auth_failure(&anyhow::anyhow!(
            "cmux ping 失败:ERROR: Access denied"
        )));
        assert!(is_auth_failure(&anyhow::anyhow!(
            "cmux ping 失败:socket password required"
        )));
        assert!(!is_auth_failure(&anyhow::anyhow!("cmux read-screen 超时")));
        assert!(!is_auth_failure(&anyhow::anyhow!(
            "cmux send 失败:surface not found"
        )));
    }

    #[test]
    fn failure_carries_stderr_and_timeout_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let fail = CmuxCli::with_program(
            script(dir.path(), "echo 'ERROR: Access denied' >&2; exit 1"),
            Duration::from_secs(10),
        );
        let msg = format!("{:#}", fail.ping().unwrap_err());
        assert!(msg.contains("Access denied"), "{msg}");

        let dir2 = tempfile::tempdir().unwrap();
        let slow = CmuxCli::with_program(
            script(dir2.path(), "exec /bin/sleep 30"),
            Duration::from_millis(300),
        );
        let started = std::time::Instant::now();
        assert!(slow.ping().is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
