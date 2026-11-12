//! Language preferences supplied by the host's CLI and operating system.
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use summon_core::config::Config;
use summon_core::i18n::{self, LanguagePreference};

pub(crate) fn configure_system() {
    i18n::set_language(i18n::resolve(
        LanguagePreference::Auto,
        None,
        sys_locale::get_locale().as_deref(),
    ));
}

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static APPLY: std::sync::Mutex<()> = std::sync::Mutex::new(());
static DISPLAY_CACHE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
static LAST_INPUT: std::sync::Mutex<Option<(LanguagePreference, std::path::PathBuf)>> =
    std::sync::Mutex::new(None);

struct Request {
    generation: u64,
    preference: LanguagePreference,
    program: std::path::PathBuf,
}

fn request(config: &Config, force: bool) -> Option<Request> {
    let _apply = APPLY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let input = (
        config.language,
        summon_core::tools::resolve_from(config, summon_core::tools::Tool::LarkCli),
    );
    let mut previous = LAST_INPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !force && previous.as_ref() == Some(&input) {
        return None;
    }
    *previous = Some(input.clone());
    Some(Request {
        generation: GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1,
        preference: input.0,
        program: input.1,
    })
}

fn resolve(request: &Request) -> i18n::Language {
    let lark = (request.preference == LanguagePreference::Auto)
        .then(|| read_lark_language(&request.program, Duration::from_secs(2)))
        .flatten();
    i18n::resolve(
        request.preference,
        lark.as_deref(),
        sys_locale::get_locale().as_deref(),
    )
}

fn apply(generation: u64, language: i18n::Language) {
    let _guard = APPLY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if generation != GENERATION.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    i18n::set_language(language);
    if let Some(path) = DISPLAY_CACHE.get() {
        let _ = publish(path, language);
    }
}

fn publish(path: &Path, language: i18n::Language) -> anyhow::Result<()> {
    crate::runtime::fsio::atomic_write_mode(path, language.code().as_bytes(), 0o600)
}

/// This file is a derived display cache for the sidecar; config.yaml remains authoritative.
pub(crate) fn enable_daemon_display_cache(path: std::path::PathBuf) {
    if DISPLAY_CACHE.set(path).is_ok() {
        if let Some(path) = DISPLAY_CACHE.get() {
            let _ = publish(path, i18n::language());
        }
    }
}

pub(crate) fn configure(config: &Config) {
    let request = request(config, true).expect("forced language request");
    apply(request.generation, resolve(&request));
}

/// Coalesce auto-language lookups on one worker; the dispatcher never waits for the CLI.
pub(crate) fn reload(config: &Config) {
    let Some(request) = request(config, false) else {
        return;
    };
    if request.preference != LanguagePreference::Auto {
        apply(request.generation, resolve(&request));
        return;
    }
    type Pending = std::sync::Arc<(std::sync::Mutex<Option<Request>>, std::sync::Condvar)>;
    static WORKER: std::sync::OnceLock<Option<Pending>> = std::sync::OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let pending: Pending =
            std::sync::Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
        let queue = std::sync::Arc::clone(&pending);
        std::thread::Builder::new()
            .name("summon-language".into())
            .spawn(move || loop {
                let (lock, wake) = &*queue;
                let request = {
                    let mut slot = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    while slot.is_none() {
                        slot = wake
                            .wait(slot)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    slot.take().unwrap()
                };
                apply(request.generation, resolve(&request));
            })
            .ok()
            .map(|_| pending)
    });
    if let Some(worker) = worker {
        let (slot, wake) = &**worker;
        let mut slot = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_ref()
            .is_none_or(|queued| queued.generation < request.generation)
        {
            *slot = Some(request);
            wake.notify_one();
        }
    } else {
        apply(
            request.generation,
            i18n::resolve(
                LanguagePreference::Auto,
                None,
                sys_locale::get_locale().as_deref(),
            ),
        );
    }
}

// The CLI may start a shell wrapper; descendants must not outlive the bounded query.
struct Query(Child);
impl Drop for Query {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.0.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.0.wait();
    }
}

fn read_lark_language(program: &Path, timeout: Duration) -> Option<String> {
    const LIMIT: usize = 64 * 1024;
    let mut query = Query(
        Command::new(program)
            .args(["config", "show"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?,
    );
    let mut stdout = query.0.stdout.take()?;
    let fd = stdout.as_raw_fd();
    // Nonblocking reads keep both memory and elapsed time bounded, including when
    // a descendant keeps stdout open after the CLI itself exits.
    unsafe {
        let flags = nix::libc::fcntl(fd, nix::libc::F_GETFL);
        if flags < 0 || nix::libc::fcntl(fd, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) < 0
        {
            return None;
        }
    }
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut eof = false;
    loop {
        let mut buffer = [0; 4096];
        match stdout.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(n) => {
                if bytes.len() + n > LIMIT {
                    return None;
                }
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
        // WNOWAIT preserves the PID until Drop kills the group and reaps it.
        let mut status: nix::libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            nix::libc::waitid(
                nix::libc::P_PID,
                query.0.id() as nix::libc::id_t,
                &mut status,
                nix::libc::WEXITED | nix::libc::WNOHANG | nix::libc::WNOWAIT,
            )
        };
        if result != 0 {
            return None;
        }
        if unsafe { status.si_pid() } != 0 {
            if status.si_code != nix::libc::CLD_EXITED || unsafe { status.si_status() } != 0 {
                return None;
            }
            if eof {
                return parse_lark_language(&bytes);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn parse_lark_language(bytes: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Preferences {
        lang: Option<String>,
    }
    let preferences: Preferences = serde_json::from_slice(bytes).ok()?;
    preferences.lang.filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(dir: &Path, source: &str) -> std::path::PathBuf {
        let path = dir.join("lark-cli");
        crate::test_support::write_test_executable(&path, &format!("#!/bin/sh\n{source}\n"));
        path
    }

    #[test]
    fn a_stale_auto_result_cannot_overwrite_an_explicit_switch() {
        let mut config = Config::default();
        let old = request(&config, true).unwrap();
        config.language = LanguagePreference::English;
        let recent = request(&config, true).unwrap();
        apply(recent.generation, i18n::Language::English);
        apply(old.generation, i18n::Language::Chinese);
        assert_eq!(i18n::language(), i18n::Language::English);
        assert!(request(&config, false).is_none());
        i18n::set_language(i18n::Language::Chinese);
    }

    #[test]
    fn sidecar_display_cache_is_atomic_private_and_contains_only_the_language() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("display-language");
        for language in [i18n::Language::English, i18n::Language::Chinese] {
            publish(&path, language).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), language.code());
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn reads_config_show_language_without_interpreting_cli_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let program = cli(
            dir.path(),
            r#"[ "$#" = 2 ] && [ "$1" = config ] && [ "$2" = show ] || exit 3
printf '%s' '{"lang":"en_us","currentApp":"ignored","apps":[{"lang":"zh_cn"}]}'"#,
        );
        assert_eq!(
            read_lark_language(&program, Duration::from_secs(1)).as_deref(),
            Some("en_us")
        );
        assert_eq!(
            parse_lark_language(br#"{"lang":"zh_cn"}"#).as_deref(),
            Some("zh_cn")
        );
        for source in [
            b"not JSON".as_slice(),
            br#"{}"#,
            br#"{"lang":null}"#,
            br#"{"lang":" "}"#,
            br#"{"lang":7}"#,
        ] {
            assert_eq!(parse_lark_language(source), None);
        }
    }

    #[test]
    fn failures_and_oversized_output_fall_back_without_exposing_output() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_lark_language(&dir.path().join("missing"), Duration::from_secs(1)),
            None
        );
        let program = cli(dir.path(), "printf '%s' '{\"lang\":\"en\"}'; exit 1");
        assert_eq!(read_lark_language(&program, Duration::from_secs(1)), None);
        let program = cli(dir.path(), "head -c 70000 /dev/zero");
        assert_eq!(read_lark_language(&program, Duration::from_secs(1)), None);
    }

    #[test]
    fn timeout_covers_running_processes_and_inherited_stdout() {
        let dir = tempfile::tempdir().unwrap();
        for source in ["sleep 30", "sleep 30 &\nexit 0"] {
            let program = cli(dir.path(), source);
            let start = Instant::now();
            assert_eq!(
                read_lark_language(&program, Duration::from_millis(50)),
                None
            );
            assert!(start.elapsed() < Duration::from_secs(1));
        }
    }
}
