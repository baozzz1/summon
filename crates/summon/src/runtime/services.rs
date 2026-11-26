//! 前置依赖服务（Managed Prerequisite Services）运行时：健康探测、单飞按需拉起兜底、
//! 租约/进程指纹归属校验、以及守护进程停机时的选择性清理。
//!
//! 核心约束：
//! - 偏好外部服务管理器（launchd / systemd）主托管；外部已健康的服务直接复用，永不擅杀。
//! - 仅对 summon 自己兜底拉起且 PID + 进程启动指纹吻合的进程执行 `stop`。
//! - `survive_daemon_restart` 默认 `true`：按需拉起时经 `setsid` 脱离 daemon 会话与进程组，
//!   daemon 重启/停机不中断仍在服务活跃会话的前置代理。
//! - 探针与服务启动均采用 `env_clear()` + 脱去 lark-cli wrapper 后的系统基线 + 服务自身 `env`
//!   （仅从全局 `~/.summon/.env` 与进程环境展开，绝不读任何 agent 的 `.env`）。

use crate::observe::log::log;
use crate::runtime::daemon::{env_from_file, essential_system_env, expand_env_value, pid_is_alive};
use crate::runtime::fsio::atomic_write;
use crate::runtime::paths::{run_dir, summon_home};
use crate::runtime::process::retry_text_file_busy;
use nix::errno::Errno;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use summon_core::agent_identity::is_runtime_owned_env_key;
use summon_core::config::Config;
use summon_core::registry::{expand_tilde, Agent};
use summon_core::services::{
    is_valid_service_name, parse_tcp_host_port, HealthcheckKind, ServiceSpec,
};

const MAX_PROBE_OUTPUT_BYTES: u64 = 4096;
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(150);
const LOG_TAIL_LINES: usize = 8;
const LOG_TAIL_MAX_CHARS: usize = 600;

/// 兜底拉起服务租约与日志目录：`~/.summon/run/services`。
pub(crate) fn services_run_dir() -> PathBuf {
    run_dir().join("services")
}

/// 按全局配置路径定位对应的 `run/services` 目录（缺省回退 `~/.summon/run/services`）。
pub(crate) fn services_run_dir_for(config_path: Option<&Path>) -> PathBuf {
    config_path
        .and_then(Path::parent)
        .map(|dir| dir.join("run").join("services"))
        .unwrap_or_else(services_run_dir)
}

/// 读取与 `config.yaml` 同目录的全局 `.env`（缺省回退 `~/.summon/.env`）。
/// 绝不读取任何 per-agent `.env`，守住服务与 agent 凭据隔离边界。
pub(crate) fn global_dotenv(config_path: Option<&Path>) -> HashMap<String, String> {
    let dir = config_path
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(summon_home);
    env_from_file(&dir.join(".env"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeResult {
    pub(crate) healthy: bool,
    pub(crate) detail: String,
    pub(crate) elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ServiceLease {
    pub(crate) service: String,
    pub(crate) pid: u32,
    pub(crate) pgid: u32,
    pub(crate) started_at_unix: i64,
    pub(crate) command: Vec<String>,
    pub(crate) survive_daemon_restart: bool,
    pub(crate) log_path: PathBuf,
    /// 进程启动时间指纹（Linux `/proc/<pid>/stat` starttime；macOS `ps -o lstart=`）。
    /// `stop` 与归属判定必须核对指纹，防止 PID 复用误杀无关进程。
    #[serde(default)]
    pub(crate) process_fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServiceOwnership {
    /// 健康且无匹配本机 summon 租约（例如由 launchd / systemd / 手动拉起）。
    External,
    /// 由 summon 兜底拉起，且租约 PID 与进程启动指纹均吻合。
    SummonManaged {
        pid: u32,
        survive_daemon_restart: bool,
        log_path: PathBuf,
    },
    /// 当前未运行（探活失败且无存活的 summon 租约）。
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceStatus {
    pub(crate) name: String,
    pub(crate) has_command: bool,
    pub(crate) validation_issues: Vec<String>,
    pub(crate) probe: Option<ProbeResult>,
    pub(crate) ownership: ServiceOwnership,
    pub(crate) required_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnsureOutcome {
    AlreadyHealthy {
        probe: ProbeResult,
    },
    Started {
        pid: u32,
        probe: ProbeResult,
        log_path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    Stopped { pid: u32 },
    NotRunning,
    ExternallyManaged,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServiceError {
    #[error("{0}")]
    ConfigLoadFailed(String),
    #[error("{}", summon_i18n::format!("services.undefined", agent = .agent, service = .service))]
    UndefinedService { agent: String, service: String },
    #[error("{}", summon_i18n::format!("services.invalid_spec", service = .service, reason = .reason))]
    InvalidConfig { service: String, reason: String },
    #[error("{}", summon_i18n::format!("services.probe_only_unhealthy", service = .service, detail = .detail))]
    ProbeOnlyUnhealthy { service: String, detail: String },
    #[error("{}", summon_i18n::format!("services.lock_timeout", service = .service, secs = .secs))]
    LockTimeout { service: String, secs: u64 },
    #[error("{}", summon_i18n::format!("services.start_spawn_failed", service = .service, error = .error))]
    SpawnFailed { service: String, error: String },
    #[error("{}", summon_i18n::format!("services.start_exited_early", service = .service, status = .status, log = .log_path.display(), tail = .log_tail))]
    ExitedEarly {
        service: String,
        status: String,
        log_path: PathBuf,
        log_tail: String,
    },
    #[error("{}", summon_i18n::format!("services.start_timeout", service = .service, secs = .secs, detail = .detail, log = .log_path.display(), tail = .log_tail))]
    StartTimeout {
        service: String,
        secs: u64,
        detail: String,
        log_path: PathBuf,
        log_tail: String,
    },
    #[error("service `{service}` I/O error: {error}")]
    Io { service: String, error: String },
}

fn ensure_services_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

fn lock_path(services_dir: &Path, name: &str) -> PathBuf {
    services_dir.join(format!("{name}.lock"))
}

fn lease_path(services_dir: &Path, name: &str) -> PathBuf {
    services_dir.join(format!("{name}.json"))
}

fn log_path(services_dir: &Path, name: &str) -> PathBuf {
    services_dir.join(format!("{name}.log"))
}

/// 构造隔离后的服务进程环境：
/// `essential_system_env()`（剔除 lark-cli wrapper 目录）+ 服务自身 `spec.env`（仅从全局 `.env` 与进程环境展开）。
fn build_service_env(
    spec: &ServiceSpec,
    global_env: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut env_map: Vec<(String, String)> = essential_system_env()
        .into_iter()
        .map(|(k, v)| {
            if k == "PATH" {
                let clean =
                    crate::setup::env::path_without_lark_cli_wrappers(std::ffi::OsStr::new(&v))
                        .to_string_lossy()
                        .into_owned();
                (k, clean)
            } else {
                (k, v)
            }
        })
        .collect();

    let lookup = |name: &str| -> Option<String> {
        global_env
            .get(name)
            .filter(|v| !v.is_empty())
            .cloned()
            .or_else(|| std::env::var(name).ok().filter(|v| !v.is_empty()))
    };

    let mut keys: Vec<&String> = spec.env.keys().collect();
    keys.sort();
    for key in keys {
        if is_runtime_owned_env_key(key) {
            continue;
        }
        let raw = &spec.env[key];
        if let Some(val) = expand_env_value(raw, &lookup) {
            if let Some((_, existing)) = env_map.iter_mut().find(|(k, _)| k == key) {
                *existing = val;
            } else {
                env_map.push((key.clone(), val));
            }
        }
    }
    env_map
}

fn url_is_loopback(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    trimmed.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 执行一次服务健康探测（有界超时，不产生持久副作用）。
pub(crate) fn probe_service(
    spec: &ServiceSpec,
    global_env: &HashMap<String, String>,
) -> ProbeResult {
    probe_service_with_timeout(spec, global_env, spec.healthcheck.timeout())
}

fn probe_service_with_timeout(
    spec: &ServiceSpec,
    global_env: &HashMap<String, String>,
    timeout: Duration,
) -> ProbeResult {
    let started = Instant::now();
    let timeout = timeout.max(Duration::from_millis(100));
    let Some(kind) = spec.healthcheck.kind() else {
        return ProbeResult {
            healthy: false,
            detail: "invalid healthcheck configuration".to_string(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
    };

    let (healthy, detail) = match kind {
        HealthcheckKind::Http(url) => probe_http(url, timeout),
        HealthcheckKind::Tcp(addr) => probe_tcp(addr, timeout),
        HealthcheckKind::Command(argv) => {
            probe_command(argv, spec.working_dir.as_deref(), spec, global_env, timeout)
        }
    };

    ProbeResult {
        healthy,
        detail,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

fn probe_http(url: &str, timeout: Duration) -> (bool, String) {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    // 回环地址绕过全局 HTTP_PROXY/HTTPS_PROXY，避免代理环境变量误拦截本机健康探针。
    if url_is_loopback(url) {
        builder = builder.no_proxy();
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => return (false, format!("HTTP client error: {e}")),
    };
    match client.get(url).send() {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                (true, format!("HTTP {}", status.as_u16()))
            } else {
                (false, format!("HTTP {}", status.as_u16()))
            }
        }
        Err(e) => (false, format!("HTTP request failed: {e}")),
    }
}

fn probe_tcp(addr: &str, timeout: Duration) -> (bool, String) {
    let Some((host, port)) = parse_tcp_host_port(addr) else {
        return (false, format!("invalid TCP address `{addr}`"));
    };
    let addrs: Vec<_> = match (host, port).to_socket_addrs() {
        Ok(iter) => iter.collect(),
        Err(e) => return (false, format!("TCP resolve failed for `{addr}`: {e}")),
    };
    if addrs.is_empty() {
        return (
            false,
            format!("TCP resolve returned no addresses for `{addr}`"),
        );
    }
    let deadline = Instant::now() + timeout;
    let mut last_err = String::new();
    for sock_addr in addrs {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (false, format!("TCP connect timed out for `{addr}`"));
        }
        match TcpStream::connect_timeout(&sock_addr, remaining) {
            Ok(_) => return (true, format!("TCP {sock_addr} reachable")),
            Err(e) => last_err = e.to_string(),
        }
    }
    (
        false,
        format!("TCP connect failed for `{addr}`: {last_err}"),
    )
}

fn probe_command(
    argv: &[String],
    working_dir: Option<&str>,
    spec: &ServiceSpec,
    global_env: &HashMap<String, String>,
    timeout: Duration,
) -> (bool, String) {
    let Some(program) = argv.first().filter(|p| !p.trim().is_empty()) else {
        return (false, "empty healthcheck command".to_string());
    };
    let program = expand_tilde(program);
    let mut cmd = Command::new(&program);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(build_service_env(spec, global_env))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(dir) = working_dir.filter(|d| !d.trim().is_empty()) {
        cmd.current_dir(expand_tilde(dir));
    }

    let mut child = match retry_text_file_busy(|| cmd.spawn()) {
        Ok(c) => c,
        Err(e) => return (false, format!("probe command spawn failed: {e}")),
    };
    let pgid = Pid::from_raw(child.id() as i32);

    let spawn_reader = |stream: Option<Box<dyn std::io::Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stream {
                let _ = s.take(MAX_PROBE_OUTPUT_BYTES).read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let out_rx = spawn_reader(child.stdout.take().map(|s| Box::new(s) as _));
    let err_rx = spawn_reader(child.stderr.take().map(|s| Box::new(s) as _));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = killpg(pgid, Signal::SIGKILL);
                    let _ = child.wait();
                    return (
                        false,
                        format!(
                            "probe command timed out after {:.1}s",
                            timeout.as_secs_f32()
                        ),
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = killpg(pgid, Signal::SIGKILL);
                let _ = child.wait();
                return (false, format!("probe command wait failed: {e}"));
            }
        }
    };

    let rem = deadline.saturating_duration_since(Instant::now());
    let stderr = err_rx.recv_timeout(rem).unwrap_or_default();
    let stdout = out_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or_default();

    if status.success() {
        return (true, "command exit 0".to_string());
    }
    let snippet_src = if !stderr.is_empty() { stderr } else { stdout };
    let snippet = String::from_utf8_lossy(&snippet_src)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join(" | ");
    if snippet.is_empty() {
        (false, format!("command exited with {status}"))
    } else {
        (false, format!("command exited with {status}: {snippet}"))
    }
}

/// 读取进程启动时间指纹，用于防止 PID 复用导致误杀无关进程。
pub(crate) fn read_process_fingerprint(pid: u32) -> Option<String> {
    if !pid_is_alive(pid) {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rsplit_once(") ")?.1;
        // `/proc/<pid>/stat` 字段 3 起（state 是 index 0），starttime 是字段 22（index 19）。
        let starttime = after_comm.split_whitespace().nth(19)?;
        if starttime.is_empty() {
            return None;
        }
        Some(format!("linux:{starttime}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = Command::new("ps")
            .env("LC_ALL", "C")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let lstart = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if lstart.is_empty() {
            return None;
        }
        Some(format!("unix:{lstart}"))
    }
}

fn read_lease(path: &Path) -> Option<ServiceLease> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_lease(path: &Path, lease: &ServiceLease) -> anyhow::Result<()> {
    let body = serde_json::to_vec_pretty(lease)?;
    atomic_write(path, &body)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

/// 校验租约所指进程仍存活且启动指纹完全一致（fail-closed：无指纹一律视为不可验证）。
fn lease_is_live_match(lease: &ServiceLease) -> bool {
    let Some(expected) = lease.process_fingerprint.as_deref() else {
        return false;
    };
    if !pid_is_alive(lease.pid) {
        return false;
    }
    read_process_fingerprint(lease.pid).as_deref() == Some(expected)
}

fn acquire_service_lock(
    services_dir: &Path,
    name: &str,
    wait_budget: Duration,
) -> Result<File, ServiceError> {
    ensure_services_dir(services_dir).map_err(|e| ServiceError::Io {
        service: name.to_string(),
        error: e.to_string(),
    })?;
    let path = lock_path(services_dir, name);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| ServiceError::Io {
            service: name.to_string(),
            error: e.to_string(),
        })?;
    let deadline = Instant::now() + wait_budget;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(ServiceError::LockTimeout {
                        service: name.to_string(),
                        secs: wait_budget.as_secs().max(1),
                    });
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(ServiceError::Io {
                    service: name.to_string(),
                    error: e.to_string(),
                });
            }
        }
    }
}

fn read_log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(LOG_TAIL_LINES)
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut joined = lines.into_iter().rev().collect::<Vec<_>>().join(" | ");
    if joined.chars().count() > LOG_TAIL_MAX_CHARS {
        let truncated: String = joined.chars().take(LOG_TAIL_MAX_CHARS).collect();
        joined = format!("{truncated}...");
    }
    format!("\n{joined}")
}

fn terminate_process_group(pgid: u32, grace: Duration) {
    let Ok(raw) = i32::try_from(pgid) else {
        return;
    };
    if raw <= 0 {
        return;
    }
    let group = Pid::from_raw(raw);
    match killpg(group, Signal::SIGTERM) {
        Ok(()) => {}
        Err(Errno::ESRCH) => return,
        Err(_) => {}
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !pid_is_alive(pgid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = killpg(group, Signal::SIGKILL);
    let kill_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < kill_deadline {
        if !pid_is_alive(pgid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// 探测单个服务并在不健康且配置了 `command` 时按需单飞拉起兜底。
pub(crate) fn ensure_service_at(
    name: &str,
    spec: &ServiceSpec,
    services_dir: &Path,
    global_env: &HashMap<String, String>,
) -> Result<EnsureOutcome, ServiceError> {
    let issues = spec.validate(name);
    if !issues.is_empty() {
        return Err(ServiceError::InvalidConfig {
            service: name.to_string(),
            reason: issues.join("; "),
        });
    }

    // 热路径无锁快速探活：服务已健康（launchd/systemd/已拉起）时零锁开销立即放行。
    let initial = probe_service(spec, global_env);
    if initial.healthy {
        return Ok(EnsureOutcome::AlreadyHealthy { probe: initial });
    }

    // 慢路径：取每服务独占 flock，防多轮并发重复拉起。
    let lock_budget = spec.healthcheck.ready_timeout() + Duration::from_secs(5);
    let _lock = acquire_service_lock(services_dir, name, lock_budget)?;

    // 拿锁后双检：等待锁期间可能已被并发轮次或外部拉起就绪。
    let recheck = probe_service(spec, global_env);
    if recheck.healthy {
        return Ok(EnsureOutcome::AlreadyHealthy { probe: recheck });
    }

    let Some(argv) = spec.start_command() else {
        return Err(ServiceError::ProbeOnlyUnhealthy {
            service: name.to_string(),
            detail: recheck.detail,
        });
    };

    // 清理陈旧租约（若存在且已失效）。
    let lp = lease_path(services_dir, name);
    if let Some(old_lease) = read_lease(&lp) {
        if !lease_is_live_match(&old_lease) {
            let _ = std::fs::remove_file(&lp);
        }
    }

    let svc_log = log_path(services_dir, name);
    let log_out = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&svc_log)
        .map_err(|e| ServiceError::Io {
            service: name.to_string(),
            error: e.to_string(),
        })?;
    let log_err = log_out.try_clone().map_err(|e| ServiceError::Io {
        service: name.to_string(),
        error: e.to_string(),
    })?;

    let program = expand_tilde(&argv[0]);
    let mut cmd = Command::new(&program);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(build_service_env(spec, global_env))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err));

    if let Some(dir) = spec.working_dir.as_deref().filter(|d| !d.trim().is_empty()) {
        cmd.current_dir(expand_tilde(dir));
    }

    if spec.survive_daemon_restart {
        // SAFETY: `pre_exec` 仅调用 POSIX async-signal-safe 的 `setsid(2)`，
        // 让兜底拉起的服务自成会话与进程组，不受 daemon 重启或 SIGHUP 影响。
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::from)?;
                Ok(())
            });
        }
    } else {
        cmd.process_group(0);
    }

    let mut child =
        retry_text_file_busy(|| cmd.spawn()).map_err(|e| ServiceError::SpawnFailed {
            service: name.to_string(),
            error: e.to_string(),
        })?;
    let pid = child.id();
    let fingerprint = read_process_fingerprint(pid);
    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let lease = ServiceLease {
        service: name.to_string(),
        pid,
        pgid: pid,
        started_at_unix,
        command: argv.to_vec(),
        survive_daemon_restart: spec.survive_daemon_restart,
        log_path: svc_log.clone(),
        process_fingerprint: fingerprint,
    };
    if let Err(e) = write_lease(&lp, &lease) {
        log(&format!(
            "[services] failed to write lease for `{name}` (pid={pid}): {e}"
        ));
    }

    let ready_budget = spec.healthcheck.ready_timeout();
    let deadline = Instant::now() + ready_budget;
    let mut last_probe_detail = recheck.detail;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = remove_lease_if_pid_matches(&lp, pid);
                return Err(ServiceError::ExitedEarly {
                    service: name.to_string(),
                    status: status.to_string(),
                    log_path: svc_log.clone(),
                    log_tail: read_log_tail(&svc_log),
                });
            }
            Ok(None) => {}
            Err(e) => {
                last_probe_detail = format!("try_wait failed: {e}");
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let probe_timeout = spec.healthcheck.timeout().min(remaining);
        let probe = probe_service_with_timeout(spec, global_env, probe_timeout);
        if probe.healthy {
            // 后台回收线程防止长驻 daemon 下子进程后续退出变成僵尸进程。
            let thread_name = format!("svc-reap-{name}");
            let _ = std::thread::Builder::new()
                .name(thread_name)
                .spawn(move || {
                    let _ = child.wait();
                });
            log(&format!(
                "[services] started fallback service `{name}` pid={pid} ({})",
                probe.detail
            ));
            return Ok(EnsureOutcome::Started {
                pid,
                probe,
                log_path: svc_log,
            });
        }
        last_probe_detail = probe.detail;

        let sleep_for = READY_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()));
        if sleep_for.is_zero() {
            break;
        }
        std::thread::sleep(sleep_for);
    }

    // 超时未就绪：清理本次启动的进程组与租约，不留孤儿。
    terminate_process_group(pid, Duration::from_secs(2));
    let _ = child.wait();
    let _ = remove_lease_if_pid_matches(&lp, pid);

    Err(ServiceError::StartTimeout {
        service: name.to_string(),
        secs: ready_budget.as_secs(),
        detail: last_probe_detail,
        log_path: svc_log.clone(),
        log_tail: read_log_tail(&svc_log),
    })
}

fn remove_lease_if_pid_matches(path: &Path, expected_pid: u32) -> std::io::Result<()> {
    if read_lease(path).is_some_and(|l| l.pid == expected_pid) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// 停止指定服务：**仅停止由 summon 兜底拉起且 PID/指纹吻合的进程**；外部托管服务一律拒绝操作。
pub(crate) fn stop_service_at(
    name: &str,
    spec: &ServiceSpec,
    services_dir: &Path,
    global_env: &HashMap<String, String>,
) -> Result<StopOutcome, ServiceError> {
    if !is_valid_service_name(name) {
        return Err(ServiceError::InvalidConfig {
            service: name.to_string(),
            reason: summon_i18n::text!("services.name_invalid").to_string(),
        });
    }
    let _lock = acquire_service_lock(services_dir, name, Duration::from_secs(10))?;
    let lp = lease_path(services_dir, name);
    if let Some(lease) = read_lease(&lp) {
        if lease_is_live_match(&lease) {
            terminate_process_group(lease.pgid, Duration::from_secs(5));
            let _ = std::fs::remove_file(&lp);
            log(&format!(
                "[services] stopped summon-managed service `{name}` pid={}",
                lease.pid
            ));
            return Ok(StopOutcome::Stopped { pid: lease.pid });
        }
        // 租约陈旧（进程已死或 PID 被复用）：仅清理租约，绝不发信号。
        let _ = std::fs::remove_file(&lp);
    }

    let probe = probe_service(spec, global_env);
    if probe.healthy {
        Ok(StopOutcome::ExternallyManaged)
    } else {
        Ok(StopOutcome::NotRunning)
    }
}

/// 查询单个服务的配置合法性、当前健康态与进程归属。
pub(crate) fn service_status_at(
    name: &str,
    spec: &ServiceSpec,
    services_dir: &Path,
    global_env: &HashMap<String, String>,
    required_by: Vec<String>,
) -> ServiceStatus {
    let validation_issues = spec.validate(name);
    let has_command = spec.start_command().is_some();
    if !validation_issues.is_empty() {
        return ServiceStatus {
            name: name.to_string(),
            has_command,
            validation_issues,
            probe: None,
            ownership: ServiceOwnership::Stopped,
            required_by,
        };
    }

    let probe = probe_service(spec, global_env);
    let lp = lease_path(services_dir, name);
    let live_lease = read_lease(&lp).filter(lease_is_live_match);
    let ownership = match live_lease {
        Some(lease) => ServiceOwnership::SummonManaged {
            pid: lease.pid,
            survive_daemon_restart: lease.survive_daemon_restart,
            log_path: lease.log_path,
        },
        None if probe.healthy => ServiceOwnership::External,
        None => ServiceOwnership::Stopped,
    };

    ServiceStatus {
        name: name.to_string(),
        has_command,
        validation_issues,
        probe: Some(probe),
        ownership,
        required_by,
    }
}

/// 对 `agent.runtime.requires` 声明的全部前置服务执行 fail-closed 准入检查与按需兜底拉起。
/// 没配 `requires` 的 agent 零开销立即放行。
pub(crate) fn ensure_agent_services(agent: &Agent) -> Result<(), ServiceError> {
    if agent.required_services().is_empty() {
        return Ok(());
    }
    let cfg_path = agent.effective_global_config_path();
    let cfg = agent
        .load_global_config()
        .map_err(ServiceError::ConfigLoadFailed)?;
    let services_dir = services_run_dir_for(cfg_path.as_deref());
    let global_env = global_dotenv(cfg_path.as_deref());
    ensure_agent_services_at(agent, &cfg, &services_dir, &global_env)
}

pub(crate) fn ensure_agent_services_at(
    agent: &Agent,
    cfg: &Config,
    services_dir: &Path,
    global_env: &HashMap<String, String>,
) -> Result<(), ServiceError> {
    let mut seen = HashSet::new();
    for raw_name in agent.required_services() {
        let name = raw_name.trim();
        if !seen.insert(name.to_string()) {
            continue;
        }
        let Some(spec) = cfg.services.get(name) else {
            return Err(ServiceError::UndefinedService {
                agent: agent.name.clone(),
                service: name.to_string(),
            });
        };
        ensure_service_at(name, spec, services_dir, global_env)?;
    }
    Ok(())
}

/// daemon 优雅停机钩子：只停止 `survive_daemon_restart: false` 且经指纹验真的 summon 兜底服务；
/// 默认 `survive_daemon_restart: true` 的服务与任何外部托管服务一律不碰。
pub(crate) fn stop_non_surviving_services() {
    stop_non_surviving_services_at(&services_run_dir());
}

pub(crate) fn stop_non_surviving_services_at(services_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(services_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(lease) = read_lease(&path) else {
            continue;
        };
        if lease.survive_daemon_restart {
            continue;
        }
        if !is_valid_service_name(&lease.service) {
            continue;
        }
        let Ok(_lock) = acquire_service_lock(services_dir, &lease.service, Duration::from_secs(3))
        else {
            continue;
        };
        // 拿锁后重读租约并核对指纹。
        let Some(current) = read_lease(&path) else {
            continue;
        };
        if current.survive_daemon_restart {
            continue;
        }
        if lease_is_live_match(&current) {
            terminate_process_group(current.pgid, Duration::from_secs(3));
            log(&format!(
                "[services] stopped non-surviving service `{}` pid={} on daemon shutdown",
                current.service, current.pid
            ));
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;
    use summon_core::services::ServiceHealthcheck;

    fn http_spec(url: String) -> ServiceSpec {
        ServiceSpec {
            healthcheck: ServiceHealthcheck {
                http: Some(url),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn tcp_spec(addr: String) -> ServiceSpec {
        ServiceSpec {
            healthcheck: ServiceHealthcheck {
                tcp: Some(addr),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn http_probe_succeeds_on_200_and_fails_on_503() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                if let Ok(req) = server.recv() {
                    if req.url() == "/health" {
                        let _ = req
                            .respond(tiny_http::Response::from_string("ok").with_status_code(200));
                    } else {
                        let _ = req.respond(
                            tiny_http::Response::from_string("down").with_status_code(503),
                        );
                    }
                }
            }
        });

        let ok_probe = probe_service(&http_spec(format!("http://{addr}/health")), &HashMap::new());
        assert!(ok_probe.healthy, "{ok_probe:?}");

        let bad_probe = probe_service(&http_spec(format!("http://{addr}/fail")), &HashMap::new());
        assert!(!bad_probe.healthy, "{bad_probe:?}");
        assert!(bad_probe.detail.contains("503"), "{bad_probe:?}");
        handle.join().unwrap();
    }

    #[test]
    fn tcp_probe_detects_open_and_closed_ports() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let ok = probe_service(&tcp_spec(addr.to_string()), &HashMap::new());
        assert!(ok.healthy, "{ok:?}");

        drop(listener);
        let closed = probe_service(&tcp_spec(addr.to_string()), &HashMap::new());
        assert!(!closed.healthy, "{closed:?}");
    }

    #[test]
    fn command_probe_scrubs_runtime_env_injects_service_env_and_times_out() {
        let mut spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "test \"$SVC_FLAG\" = \"yes\" && test -z \"$SUMMON_AGENT\"".into(),
                ]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            ..Default::default()
        };
        spec.env
            .insert("SVC_FLAG".into(), "${GLOBAL_SVC_FLAG}".into());

        let mut global = HashMap::new();
        global.insert("GLOBAL_SVC_FLAG".into(), "yes".into());
        let ok = probe_service(&spec, &global);
        assert!(ok.healthy, "{ok:?}");

        // 超时探测必须有界返回且杀掉探测进程。
        let slow = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec!["/bin/sleep".into(), "10".into()]),
                timeout_secs: 1,
                ready_timeout_secs: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        let started = Instant::now();
        let res = probe_service(&slow, &HashMap::new());
        assert!(!res.healthy);
        assert!(res.detail.contains("timed out"), "{res:?}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn probe_only_service_fails_closed_when_unhealthy() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec!["/bin/sh".into(), "-c".into(), "exit 1".into()]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            ..Default::default()
        };
        let err = ensure_service_at("ext-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(
            matches!(err, ServiceError::ProbeOnlyUnhealthy { .. }),
            "unexpected err: {err:?}"
        );
    }

    #[test]
    fn fallback_start_becomes_ready_reuses_when_healthy_and_stops_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let ready_file = tmp.path().join("svc.ready");
        let start_script = format!(
            "sleep 0.15; touch '{}'; exec sleep 60",
            ready_file.display()
        );
        let probe_script = format!("test -f '{}'", ready_file.display());
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec!["/bin/sh".into(), "-c".into(), probe_script]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec!["/bin/sh".into(), "-c".into(), start_script]),
            survive_daemon_restart: true,
            ..Default::default()
        };

        let first = ensure_service_at("demo-svc", &spec, tmp.path(), &HashMap::new()).unwrap();
        let pid = match first {
            EnsureOutcome::Started { pid, .. } => pid,
            other => panic!("expected Started, got {other:?}"),
        };
        assert!(pid_is_alive(pid));
        assert!(lease_path(tmp.path(), "demo-svc").exists());

        // 第二次调用应直接复用，不重起。
        let second = ensure_service_at("demo-svc", &spec, tmp.path(), &HashMap::new()).unwrap();
        assert!(matches!(second, EnsureOutcome::AlreadyHealthy { .. }));

        // status 应识别为 SummonManaged。
        let status = service_status_at(
            "demo-svc",
            &spec,
            tmp.path(),
            &HashMap::new(),
            vec!["a".into()],
        );
        assert!(status.probe.as_ref().is_some_and(|p| p.healthy));
        assert!(matches!(
            status.ownership,
            ServiceOwnership::SummonManaged { pid: p, .. } if p == pid
        ));

        // stop 应精准停掉由 summon 拉起的进程并删除租约。
        let stopped = stop_service_at("demo-svc", &spec, tmp.path(), &HashMap::new()).unwrap();
        assert_eq!(stopped, StopOutcome::Stopped { pid });
        assert!(!pid_is_alive(pid));
        assert!(!lease_path(tmp.path(), "demo-svc").exists());
    }

    #[test]
    fn stop_refuses_to_kill_externally_managed_service() {
        let tmp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let spec = tcp_spec(addr.to_string());

        let outcome =
            stop_service_at("external-proxy", &spec, tmp.path(), &HashMap::new()).unwrap();
        assert_eq!(outcome, StopOutcome::ExternallyManaged);
        // 外部监听器仍然存活。
        assert!(probe_service(&spec, &HashMap::new()).healthy);
        drop(listener);
    }

    #[test]
    fn concurrent_ensure_is_single_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let ready_file = tmp.path().join("sf.ready");
        let count_file = tmp.path().join("sf.starts");
        let start_script = format!(
            "echo x >> '{}'; sleep 0.3; touch '{}'; exec sleep 60",
            count_file.display(),
            ready_file.display()
        );
        let probe_script = format!("test -f '{}'", ready_file.display());
        let spec = Arc::new(ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec!["/bin/sh".into(), "-c".into(), probe_script]),
                timeout_secs: 2,
                ready_timeout_secs: 8,
                ..Default::default()
            },
            command: Some(vec!["/bin/sh".into(), "-c".into(), start_script]),
            ..Default::default()
        });

        let dir = tmp.path().to_path_buf();
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = Arc::clone(&spec);
            let d = dir.clone();
            handles.push(std::thread::spawn(move || {
                ensure_service_at("sf-svc", &s, &d, &HashMap::new()).unwrap()
            }));
        }
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let started_count = outcomes
            .iter()
            .filter(|o| matches!(o, EnsureOutcome::Started { .. }))
            .count();
        assert_eq!(
            started_count, 1,
            "only one caller should spawn: {outcomes:?}"
        );
        let lines = std::fs::read_to_string(&count_file).unwrap();
        assert_eq!(lines.lines().count(), 1, "start script must execute once");

        let _ = stop_service_at("sf-svc", &spec, tmp.path(), &HashMap::new());
    }

    #[test]
    fn early_exit_fails_fast_with_log_tail_and_cleans_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec!["/bin/sh".into(), "-c".into(), "exit 1".into()]),
                timeout_secs: 2,
                ready_timeout_secs: 10,
                ..Default::default()
            },
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo 'boom-diagnostic-line' >&2; exit 42".into(),
            ]),
            ..Default::default()
        };

        let started = Instant::now();
        let err = ensure_service_at("crash-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4));
        match err {
            ServiceError::ExitedEarly { log_tail, .. } => {
                assert!(log_tail.contains("boom-diagnostic-line"), "{log_tail}");
            }
            other => panic!("expected ExitedEarly, got {other:?}"),
        }
        assert!(!lease_path(tmp.path(), "crash-svc").exists());
    }

    #[test]
    fn stop_non_surviving_services_only_stops_survive_false() {
        let tmp = tempfile::tempdir().unwrap();
        let ready_keep = tmp.path().join("keep.ready");
        let ready_drop = tmp.path().join("drop.ready");

        let keep_spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("test -f '{}'", ready_keep.display()),
                ]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("touch '{}'; exec sleep 60", ready_keep.display()),
            ]),
            survive_daemon_restart: true,
            ..Default::default()
        };
        let drop_spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("test -f '{}'", ready_drop.display()),
                ]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("touch '{}'; exec sleep 60", ready_drop.display()),
            ]),
            survive_daemon_restart: false,
            ..Default::default()
        };

        let keep_pid =
            match ensure_service_at("keep-svc", &keep_spec, tmp.path(), &HashMap::new()).unwrap() {
                EnsureOutcome::Started { pid, .. } => pid,
                other => panic!("expected Started: {other:?}"),
            };
        let drop_pid =
            match ensure_service_at("drop-svc", &drop_spec, tmp.path(), &HashMap::new()).unwrap() {
                EnsureOutcome::Started { pid, .. } => pid,
                other => panic!("expected Started: {other:?}"),
            };

        stop_non_surviving_services_at(tmp.path());

        assert!(
            pid_is_alive(keep_pid),
            "survive_daemon_restart=true must stay alive"
        );
        assert!(
            !pid_is_alive(drop_pid),
            "survive_daemon_restart=false must be stopped"
        );
        assert!(lease_path(tmp.path(), "keep-svc").exists());
        assert!(!lease_path(tmp.path(), "drop-svc").exists());

        // 清理测试进程
        let _ = stop_service_at("keep-svc", &keep_spec, tmp.path(), &HashMap::new());
    }
}
