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
use crate::runtime::daemon::{env_from_file, expand_env_value, pid_is_alive};
use crate::runtime::fsio::atomic_write;
use crate::runtime::paths::{run_dir, summon_home};
use crate::runtime::process::retry_text_file_busy;
use nix::errno::Errno;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::{getpgid, Pid};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SERVICES_SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
use summon_core::agent_identity::is_runtime_owned_env_key;
use summon_core::config::Config;
use summon_core::registry::{collapse_tilde, expand_tilde, Agent};
use summon_core::services::{
    is_valid_service_name, parse_tcp_host_port, HealthcheckKind, ServiceSpec,
};

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(150);

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
    /// 一次性启动命令（如 `launchctl kickstart` / `systemctl start`）以 0 退出并由外部管理器接管，
    /// 后续探活在超时预算内就绪；summon 不保留进程租约。
    StartedExternal {
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
    #[error("{}", summon_i18n::format!("services.start_exited_early", service = .service, status = .status, log = collapse_tilde(&.log_path.display().to_string())))]
    ExitedEarly {
        service: String,
        status: String,
        log_path: PathBuf,
    },
    #[error("{}", summon_i18n::format!("services.start_timeout", service = .service, secs = .secs, detail = .detail, log = collapse_tilde(&.log_path.display().to_string())))]
    StartTimeout {
        service: String,
        secs: u64,
        detail: String,
        log_path: PathBuf,
    },
    #[error("{}", summon_i18n::format!("services.stop_failed", service = .service, pid = .pid))]
    StopFailed { service: String, pid: u32 },
    #[error("{}", summon_i18n::format!("services.existing_still_running", service = .service, pid = .pid, detail = .detail))]
    ExistingInstanceStillRunning {
        service: String,
        pid: u32,
        detail: String,
    },
    #[error("{}", summon_i18n::format!("services.shutting_down", service = .service))]
    ShuttingDown { service: String },
    #[error("{}", summon_i18n::format!("services.lease_ownership_invalid", service = .service, pid = .pid, reason = .reason))]
    LeaseOwnershipInvalid {
        service: String,
        pid: u32,
        reason: String,
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

/// 共享前置服务允许继承的最小系统环境白名单：不含任何 `SUMMON_*` / `FEISHU_*` 键，
/// 杜绝调用方身份、控制 socket 或 bridge handoff token 泄入共享服务进程。
fn service_base_system_env() -> Vec<(String, String)> {
    const KEYS: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LC_COLLATE",
        "LC_MESSAGES",
        "LC_NUMERIC",
        "LC_TIME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "GPG_AGENT_INFO",
        "DBUS_SESSION_BUS_ADDRESS",
        "__CF_USER_TEXT_ENCODING",
    ];
    let mut out = Vec::new();
    for k in KEYS {
        if let Ok(v) = std::env::var(k) {
            if *k == "PATH" {
                let clean =
                    crate::setup::env::path_without_lark_cli_wrappers(std::ffi::OsStr::new(&v))
                        .to_string_lossy()
                        .into_owned();
                out.push(((*k).to_string(), clean));
            } else {
                out.push(((*k).to_string(), v));
            }
        }
    }
    out
}

/// 仅记录脱敏后的服务生命周期状态行（时间、程序名、退出码/探活结果/耗时）；
/// 绝不把子进程原始 stdout/stderr 或环境变量值落入本机日志。
fn append_service_lifecycle_log(path: &Path, line: &str) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{ts}] {line}");
    }
}

/// 构造隔离后的服务进程环境：
/// 最小系统白名单（不含任何 `SUMMON_*`/`FEISHU_*`，并剔除 lark-cli wrapper 目录）+ 服务自身 `spec.env`。
fn build_service_env(
    spec: &ServiceSpec,
    global_env: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut env_map = service_base_system_env();

    let lookup = |name: &str| -> Option<String> {
        if is_runtime_owned_env_key(name) {
            return None;
        }
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
        Err(e) => {
            let clean = e.without_url();
            if clean.is_timeout() {
                (false, "HTTP request timed out".to_string())
            } else if clean.is_connect() {
                (false, "HTTP connect failed".to_string())
            } else {
                (false, format!("HTTP request failed: {clean}"))
            }
        }
    }
}

fn probe_tcp(addr: &str, timeout: Duration) -> (bool, String) {
    probe_tcp_with_resolver(addr, timeout, |host, port| {
        (host, port)
            .to_socket_addrs()
            .map(|iter| iter.collect())
            .map_err(|e| e.to_string())
    })
}

fn probe_tcp_with_resolver<F>(addr: &str, timeout: Duration, resolver: F) -> (bool, String)
where
    F: FnOnce(String, u16) -> Result<Vec<SocketAddr>, String> + Send + 'static,
{
    let Some((host, port)) = parse_tcp_host_port(addr) else {
        return (false, format!("invalid TCP address `{addr}`"));
    };
    let deadline = Instant::now() + timeout;
    let addrs: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let host_owned = host.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(resolver(host_owned, port));
        });
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Ok(list)) => list,
            Ok(Err(e)) => return (false, format!("TCP resolve failed for `{addr}`: {e}")),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return (false, format!("TCP resolve timed out for `{addr}`"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return (false, format!("TCP resolve aborted for `{addr}`"));
            }
        }
    };
    if addrs.is_empty() {
        return (
            false,
            format!("TCP resolve returned no addresses for `{addr}`"),
        );
    }
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
    // 探针输出可能包含由 `services.env` 传入的敏感值；不采集也不回显 stdout/stderr，
    // 错误详情仅保留退出状态或超时分类，杜绝凭据进入 daemon 日志或群聊卡片。
    cmd.args(&argv[1..])
        .env_clear()
        .envs(build_service_env(spec, global_env))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(dir) = working_dir.filter(|d| !d.trim().is_empty()) {
        cmd.current_dir(expand_tilde(dir));
    }

    let mut child = match retry_text_file_busy(|| cmd.spawn()) {
        Ok(c) => c,
        Err(e) => return (false, format!("probe command spawn failed: {}", e.kind())),
    };
    let pgid = Pid::from_raw(child.id() as i32);

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
                return (false, format!("probe command wait failed: {}", e.kind()));
            }
        }
    };

    if status.success() {
        (true, "command exit 0".to_string())
    } else {
        (false, format!("command exited with {status}"))
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum LeaseOwnership {
    /// 进程存活，且服务名、租约 PGID、系统 getpgid 与进程启动指纹全部吻合。
    Owned { pid: u32 },
    /// 进程已退出或 PID 已被无关进程复用（指纹不匹配），可安全清理陈旧租约且不得发信号。
    Stale,
    /// 进程存活但归属字段不一致（缺少指纹、服务名不符、pgid != pid 或系统 PGID 不符），
    /// 必须 fail-closed：绝不发信号，保留租约并返回错误。
    UnverifiedLive { pid: u32, reason: String },
}

/// 严格校验租约归属：将服务名、PID、租约 PGID、系统 `getpgid(pid)` 与启动指纹绑定在同一进程上。
fn verify_lease_ownership(expected_service: &str, lease: &ServiceLease) -> LeaseOwnership {
    if !pid_is_alive(lease.pid) {
        return LeaseOwnership::Stale;
    }
    let Some(expected_fp) = lease.process_fingerprint.as_deref() else {
        return LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: "missing process fingerprint".to_string(),
        };
    };
    let Some(actual_fp) = read_process_fingerprint(lease.pid) else {
        return LeaseOwnership::Stale;
    };
    if actual_fp != expected_fp {
        return LeaseOwnership::Stale;
    }
    if lease.service != expected_service {
        return LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: format!("lease service `{}` != `{expected_service}`", lease.service),
        };
    }
    if lease.pgid != lease.pid {
        return LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: format!("lease pgid {} != pid {}", lease.pgid, lease.pid),
        };
    }
    let Ok(raw_pid) = i32::try_from(lease.pid) else {
        return LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: "pid out of i32 range".to_string(),
        };
    };
    match getpgid(Some(Pid::from_raw(raw_pid))) {
        Ok(actual_pgid) if actual_pgid.as_raw() == raw_pid => {
            LeaseOwnership::Owned { pid: lease.pid }
        }
        Ok(actual_pgid) => LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: format!("OS pgid {} != pid {}", actual_pgid.as_raw(), lease.pid),
        },
        Err(Errno::ESRCH) => LeaseOwnership::Stale,
        Err(e) => LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: format!("getpgid failed: {e}"),
        },
    }
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

/// 终止整个服务进程组（SIGTERM → 宽限期内探测整组存活 → 超期 SIGKILL 整组）。
/// 返回 `true` 表示整个进程组已彻底退出（`ESRCH`），`false` 表示仍有成员存活。
fn terminate_process_group(pgid: u32, grace: Duration) -> bool {
    terminate_process_group_reaping(pgid, grace, None)
}

fn terminate_process_group_reaping(
    pgid: u32,
    grace: Duration,
    mut child: Option<&mut std::process::Child>,
) -> bool {
    let Ok(raw) = i32::try_from(pgid) else {
        return true;
    };
    if raw <= 0 {
        return true;
    }
    let group = Pid::from_raw(raw);
    match killpg(group, Signal::SIGTERM) {
        Ok(()) => {}
        Err(Errno::ESRCH) => {
            if let Some(c) = child.as_deref_mut() {
                let _ = c.try_wait();
            }
            return true;
        }
        Err(_) => {}
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if let Some(c) = child.as_deref_mut() {
            let _ = c.try_wait();
        }
        if !crate::runtime::inflight::group_alive(raw) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = killpg(group, Signal::SIGKILL);
    let kill_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < kill_deadline {
        if let Some(c) = child.as_deref_mut() {
            let _ = c.try_wait();
        }
        if !crate::runtime::inflight::group_alive(raw) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    if let Some(c) = child {
        let _ = c.try_wait();
    }
    !crate::runtime::inflight::group_alive(raw)
}

/// 探测单个服务并在不健康且配置了 `command` 时按需单飞拉起兜底。
pub(crate) fn ensure_service_at(
    name: &str,
    spec: &ServiceSpec,
    services_dir: &Path,
    global_env: &HashMap<String, String>,
) -> Result<EnsureOutcome, ServiceError> {
    ensure_service_at_with_shutdown(
        name,
        spec,
        services_dir,
        global_env,
        &SERVICES_SHUTTING_DOWN,
    )
}

fn ensure_service_at_with_shutdown(
    name: &str,
    spec: &ServiceSpec,
    services_dir: &Path,
    global_env: &HashMap<String, String>,
    shutdown: &AtomicBool,
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

    if !spec.survive_daemon_restart && shutdown.load(Ordering::SeqCst) {
        return Err(ServiceError::ShuttingDown {
            service: name.to_string(),
        });
    }

    // 慢路径：取每服务独占 flock，防多轮并发重复拉起。
    let lock_budget = spec.healthcheck.ready_timeout() + Duration::from_secs(5);
    let _lock = acquire_service_lock(services_dir, name, lock_budget)?;

    if !spec.survive_daemon_restart && shutdown.load(Ordering::SeqCst) {
        return Err(ServiceError::ShuttingDown {
            service: name.to_string(),
        });
    }

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

    // 已有租约时先严格核验归属：
    // - 归属吻合（服务名/PID/PGID/指纹全一致）→ 先受控停掉旧组再重启；停不掉则 fail-closed 保租约；
    // - 存活但归属无法核实 → 拒绝发信号、保租约、fail-closed 报错；
    // - 陈旧（进程已死或 PID 被复用）→ 仅删陈旧租约。
    let lp = lease_path(services_dir, name);
    if let Some(old_lease) = read_lease(&lp) {
        match verify_lease_ownership(name, &old_lease) {
            LeaseOwnership::Owned { pid: owned_pid } => {
                if !terminate_process_group(owned_pid, Duration::from_secs(3)) {
                    return Err(ServiceError::ExistingInstanceStillRunning {
                        service: name.to_string(),
                        pid: owned_pid,
                        detail: recheck.detail,
                    });
                }
                let _ = std::fs::remove_file(&lp);
                log(&format!(
                    "[services] stopped unhealthy summon-managed service `{name}` pid={owned_pid} before fallback restart"
                ));
            }
            LeaseOwnership::UnverifiedLive { pid, reason } => {
                return Err(ServiceError::LeaseOwnershipInvalid {
                    service: name.to_string(),
                    pid,
                    reason,
                });
            }
            LeaseOwnership::Stale => {
                let _ = std::fs::remove_file(&lp);
            }
        }
    }

    let svc_log = log_path(services_dir, name);
    let program = expand_tilde(&argv[0]);
    let program_label = Path::new(&program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("service")
        .to_string();

    // 子进程 stdout/stderr 一律丢弃，绝不把可能含凭据的原始进程输出落盘；
    // 本机 0600 日志文件只记录脱敏后的生命周期事件。
    let mut cmd = Command::new(&program);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(build_service_env(spec, global_env))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

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

    let spawn_started = Instant::now();
    let mut child =
        retry_text_file_busy(|| cmd.spawn()).map_err(|e| ServiceError::SpawnFailed {
            service: name.to_string(),
            error: e.kind().to_string(),
        })?;
    let pid = child.id();
    append_service_lifecycle_log(
        &svc_log,
        &format!("spawn service={name} program={program_label} pid={pid}"),
    );

    // 区分常驻自管进程与一次性启动命令（如 `launchctl kickstart` / `systemctl --user start`）：
    // 若启动命令刚拉起即以 0 退出，则视为外部管理器接管，不写 summon 租约并继续等待探活就绪；
    // 若以非 0 退出则立即报错；若仍在运行则必须取得有效启动指纹并成功持久化租约，否则 fail-closed 清理子进程组。
    let mut child_exited_cleanly = false;
    match child.try_wait() {
        Ok(Some(status)) if status.success() => {
            child_exited_cleanly = true;
            append_service_lifecycle_log(
                &svc_log,
                "launcher exited cleanly (exit 0); waiting for external readiness",
            );
        }
        Ok(Some(status)) => {
            let _ = terminate_process_group(pid, Duration::from_millis(200));
            append_service_lifecycle_log(
                &svc_log,
                &format!(
                    "exited early status={status} elapsed_ms={}",
                    spawn_started.elapsed().as_millis()
                ),
            );
            return Err(ServiceError::ExitedEarly {
                service: name.to_string(),
                status: status.to_string(),
                log_path: svc_log,
            });
        }
        Ok(None) | Err(_) => {}
    }

    if !child_exited_cleanly {
        let fingerprint = match read_process_fingerprint(pid) {
            Some(fp) => Some(fp),
            None => match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    child_exited_cleanly = true;
                    append_service_lifecycle_log(
                        &svc_log,
                        "launcher exited cleanly (exit 0); waiting for external readiness",
                    );
                    None
                }
                Ok(Some(status)) => {
                    let _ = terminate_process_group(pid, Duration::from_millis(200));
                    append_service_lifecycle_log(
                        &svc_log,
                        &format!(
                            "exited early status={status} elapsed_ms={}",
                            spawn_started.elapsed().as_millis()
                        ),
                    );
                    return Err(ServiceError::ExitedEarly {
                        service: name.to_string(),
                        status: status.to_string(),
                        log_path: svc_log,
                    });
                }
                Ok(None) | Err(_) => {
                    let _ = terminate_process_group_reaping(
                        pid,
                        Duration::from_secs(2),
                        Some(&mut child),
                    );
                    append_service_lifecycle_log(
                        &svc_log,
                        "aborted: failed to read process fingerprint",
                    );
                    return Err(ServiceError::Io {
                        service: name.to_string(),
                        error: "failed to read process fingerprint".to_string(),
                    });
                }
            },
        };

        if let Some(fp) = fingerprint {
            let started_at_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let lease = ServiceLease {
                service: name.to_string(),
                pid,
                pgid: pid,
                started_at_unix,
                command: vec![program_label.clone()],
                survive_daemon_restart: spec.survive_daemon_restart,
                log_path: svc_log.clone(),
                process_fingerprint: Some(fp),
            };
            if let Err(e) = write_lease(&lp, &lease) {
                let _ =
                    terminate_process_group_reaping(pid, Duration::from_secs(2), Some(&mut child));
                let _ = remove_lease_if_pid_matches(&lp, pid);
                append_service_lifecycle_log(&svc_log, "aborted: failed to persist service lease");
                return Err(ServiceError::Io {
                    service: name.to_string(),
                    error: format!("failed to persist lease: {e}"),
                });
            }
        }
    }

    let ready_budget = spec.healthcheck.ready_timeout();
    let deadline = Instant::now() + ready_budget;
    let mut last_probe_detail = recheck.detail;

    loop {
        if !spec.survive_daemon_restart && shutdown.load(Ordering::SeqCst) {
            if !child_exited_cleanly {
                let group_gone =
                    terminate_process_group_reaping(pid, Duration::from_secs(2), Some(&mut child));
                if group_gone {
                    let _ = remove_lease_if_pid_matches(&lp, pid);
                }
            }
            append_service_lifecycle_log(&svc_log, "aborted during daemon shutdown");
            return Err(ServiceError::ShuttingDown {
                service: name.to_string(),
            });
        }

        if !child_exited_cleanly {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    // 一次性启动命令正常退出（如 launchctl/systemctl）：撤回 summon 租约，
                    // 继续在就绪超时内等待外部管理器拉起的服务探活转绿。
                    child_exited_cleanly = true;
                    let _ = remove_lease_if_pid_matches(&lp, pid);
                    append_service_lifecycle_log(
                        &svc_log,
                        "launcher exited cleanly (exit 0); waiting for external readiness",
                    );
                }
                Ok(Some(status)) => {
                    let group_gone = terminate_process_group(pid, Duration::from_millis(200));
                    if group_gone {
                        let _ = remove_lease_if_pid_matches(&lp, pid);
                    }
                    append_service_lifecycle_log(
                        &svc_log,
                        &format!(
                            "exited early status={status} elapsed_ms={}",
                            spawn_started.elapsed().as_millis()
                        ),
                    );
                    return Err(ServiceError::ExitedEarly {
                        service: name.to_string(),
                        status: status.to_string(),
                        log_path: svc_log,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    last_probe_detail = format!("try_wait failed: {}", e.kind());
                }
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let probe_timeout = spec.healthcheck.timeout().min(remaining);
        let probe = probe_service_with_timeout(spec, global_env, probe_timeout);
        if probe.healthy {
            if !spec.survive_daemon_restart && shutdown.load(Ordering::SeqCst) {
                if !child_exited_cleanly {
                    let group_gone = terminate_process_group_reaping(
                        pid,
                        Duration::from_secs(2),
                        Some(&mut child),
                    );
                    if group_gone {
                        let _ = remove_lease_if_pid_matches(&lp, pid);
                    }
                }
                append_service_lifecycle_log(&svc_log, "aborted during daemon shutdown");
                return Err(ServiceError::ShuttingDown {
                    service: name.to_string(),
                });
            }
            append_service_lifecycle_log(
                &svc_log,
                &format!(
                    "ready elapsed_ms={} detail={}",
                    spawn_started.elapsed().as_millis(),
                    probe.detail
                ),
            );
            if child_exited_cleanly {
                log(&format!(
                    "[services] launcher for `{name}` handed off to external manager ({})",
                    probe.detail
                ));
                return Ok(EnsureOutcome::StartedExternal {
                    probe,
                    log_path: svc_log,
                });
            }
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

    // 超时未就绪：若子进程仍由 summon 持有则清理整组并回收；确认全组退出后才清租约。
    if !child_exited_cleanly {
        let group_gone =
            terminate_process_group_reaping(pid, Duration::from_secs(2), Some(&mut child));
        if group_gone {
            let _ = remove_lease_if_pid_matches(&lp, pid);
        }
    }
    append_service_lifecycle_log(
        &svc_log,
        &format!(
            "ready timeout secs={} last_probe={last_probe_detail}",
            ready_budget.as_secs()
        ),
    );

    Err(ServiceError::StartTimeout {
        service: name.to_string(),
        secs: ready_budget.as_secs(),
        detail: last_probe_detail,
        log_path: svc_log,
    })
}

fn remove_lease_if_pid_matches(path: &Path, expected_pid: u32) -> std::io::Result<()> {
    if read_lease(path).is_some_and(|l| l.pid == expected_pid) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// 停止指定服务：**仅停止由 summon 兜底拉起且服务名/PID/PGID/指纹全部吻合的进程组**；
/// 外部托管服务一律拒绝操作；归属无法核实的存活租约 fail-closed 报错并保留租约。
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
        match verify_lease_ownership(name, &lease) {
            LeaseOwnership::Owned { pid: owned_pid } => {
                if !terminate_process_group(owned_pid, Duration::from_secs(5)) {
                    return Err(ServiceError::StopFailed {
                        service: name.to_string(),
                        pid: owned_pid,
                    });
                }
                let _ = std::fs::remove_file(&lp);
                log(&format!(
                    "[services] stopped summon-managed service `{name}` pid={owned_pid}"
                ));
                return Ok(StopOutcome::Stopped { pid: owned_pid });
            }
            LeaseOwnership::UnverifiedLive { pid, reason } => {
                return Err(ServiceError::LeaseOwnershipInvalid {
                    service: name.to_string(),
                    pid,
                    reason,
                });
            }
            LeaseOwnership::Stale => {
                // 租约陈旧（进程已死或 PID 被复用）：仅清理租约，绝不发信号。
                let _ = std::fs::remove_file(&lp);
            }
        }
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
    if !validation_issues.is_empty() {
        return ServiceStatus {
            name: name.to_string(),
            has_command: spec.start_command().is_some(),
            validation_issues,
            probe: None,
            ownership: ServiceOwnership::Stopped,
            required_by,
        };
    }

    let probe = probe_service(spec, global_env);
    let lp = lease_path(services_dir, name);
    let ownership = match read_lease(&lp) {
        Some(lease)
            if matches!(
                verify_lease_ownership(name, &lease),
                LeaseOwnership::Owned { .. }
            ) =>
        {
            ServiceOwnership::SummonManaged {
                pid: lease.pid,
                survive_daemon_restart: lease.survive_daemon_restart,
                log_path: lease.log_path,
            }
        }
        _ if probe.healthy => ServiceOwnership::External,
        _ => ServiceOwnership::Stopped,
    };

    ServiceStatus {
        name: name.to_string(),
        has_command: spec.start_command().is_some(),
        validation_issues,
        probe: Some(probe),
        ownership,
        required_by,
    }
}

/// 入口准入：按 `agent.runtime.requires` 声明顺序逐个确保前置依赖服务已就绪。
/// 无依赖时零 I/O 直接返回 `Ok(())`；任一依赖未定义、配置非法或不可用即 fail-closed 返回错误。
pub(crate) fn ensure_agent_services(agent: &Agent) -> Result<(), ServiceError> {
    if agent.required_services().is_empty() {
        return Ok(());
    }
    let cfg = agent
        .load_global_config()
        .map_err(ServiceError::ConfigLoadFailed)?;
    let cfg_path = agent.effective_global_config_path();
    let services_dir = services_run_dir_for(cfg_path.as_deref());
    let global_env = global_dotenv(cfg_path.as_deref());
    ensure_agent_services_with(agent, &cfg, &services_dir, &global_env)
}

pub(crate) fn ensure_agent_services_with(
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

/// daemon 优雅停机钩子：先关停临时服务的新启动入口并通知在途就绪循环撤销，再逐一停止
/// `survive_daemon_restart: false` 且经完整归属核验的 summon 兜底服务；
/// 默认 `survive_daemon_restart: true` 的服务与任何外部托管服务一律不碰。
pub(crate) fn stop_non_surviving_services() {
    stop_non_surviving_services_at_with_shutdown(&services_run_dir(), &SERVICES_SHUTTING_DOWN);
}

#[cfg(test)]
pub(crate) fn stop_non_surviving_services_at(services_dir: &Path) {
    let flag = AtomicBool::new(false);
    stop_non_surviving_services_at_with_shutdown(services_dir, &flag);
}

fn stop_non_surviving_services_at_with_shutdown(services_dir: &Path, shutdown: &AtomicBool) {
    shutdown.store(true, Ordering::SeqCst);
    let Ok(entries) = std::fs::read_dir(services_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_valid_service_name(file_stem) {
            continue;
        }
        let Some(lease) = read_lease(&path) else {
            continue;
        };
        if lease.survive_daemon_restart {
            continue;
        }
        // 在途启动收到 shutdown 后会在一个轮询周期（<= 200ms + 清理宽限）内释放锁，
        // 给足 10s 等待锁以避免漏掉正在启动中的临时服务。
        let Ok(_lock) = acquire_service_lock(services_dir, file_stem, Duration::from_secs(10))
        else {
            log(&format!(
                "[services] timed out waiting for service lock `{file_stem}` on daemon shutdown"
            ));
            continue;
        };
        // 拿锁后重读租约并核对服务名/文件名/PID/PGID/指纹。
        let Some(current) = read_lease(&path) else {
            continue;
        };
        if current.survive_daemon_restart {
            continue;
        }
        match verify_lease_ownership(file_stem, &current) {
            LeaseOwnership::Owned { pid: owned_pid } => {
                if terminate_process_group(owned_pid, Duration::from_secs(3)) {
                    log(&format!(
                        "[services] stopped non-surviving service `{file_stem}` pid={owned_pid} on daemon shutdown"
                    ));
                    let _ = std::fs::remove_file(&path);
                } else {
                    log(&format!(
                        "[services] failed to stop non-surviving service `{file_stem}` pid={owned_pid} on daemon shutdown; keeping lease"
                    ));
                }
            }
            LeaseOwnership::UnverifiedLive { pid, reason } => {
                log(&format!(
                    "[services] refusing to signal unverified lease `{file_stem}` pid={pid} on daemon shutdown ({reason}); keeping lease"
                ));
            }
            LeaseOwnership::Stale => {
                let _ = std::fs::remove_file(&path);
            }
        }
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
    fn early_exit_and_probe_errors_never_leak_process_output_or_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        // 1. 启动早退时，stderr 里的敏感内容落盘在 0600 日志里，绝不进入 ServiceError 文案。
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo 'probe-secret-token-xyz' >&2; exit 1".into(),
                ]),
                timeout_secs: 2,
                ready_timeout_secs: 10,
                ..Default::default()
            },
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo 'startup-secret-token-abc' >&2; exit 42".into(),
            ]),
            ..Default::default()
        };

        let started = Instant::now();
        let err = ensure_service_at("crash-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4));
        let rendered = err.to_string();
        assert!(
            !rendered.contains("startup-secret-token-abc"),
            "ServiceError must never echo raw service stderr: {rendered}"
        );
        assert!(
            !rendered.contains("probe-secret-token-xyz"),
            "ServiceError must never echo raw probe output: {rendered}"
        );
        match err {
            ServiceError::ExitedEarly { log_path, .. } => {
                let logged = std::fs::read_to_string(&log_path).unwrap();
                assert!(
                    !logged.contains("startup-secret-token-abc"),
                    "on-disk service log must never capture raw child stderr/secrets: {logged}"
                );
                assert!(
                    logged.contains("exited early"),
                    "lifecycle log should record sanitized exit status: {logged}"
                );
            }
            other => panic!("expected ExitedEarly, got {other:?}"),
        }
        assert!(!lease_path(tmp.path(), "crash-svc").exists());

        // 2. 纯探活失败时，command 探针的输出也不得进入 ProbeResult / ServiceError。
        let probe_only = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo 'probe-only-secret-999' >&2; exit 1".into(),
                ]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            ..Default::default()
        };
        let probe_err =
            ensure_service_at("probe-only-svc", &probe_only, tmp.path(), &HashMap::new())
                .unwrap_err();
        assert!(
            !probe_err.to_string().contains("probe-only-secret-999"),
            "probe error must not leak stdout/stderr: {probe_err}"
        );
    }

    #[test]
    fn ensure_service_stops_unhealthy_managed_instance_before_restarting() {
        let tmp = tempfile::tempdir().unwrap();
        let ready_file = tmp.path().join("rehealth.ready");
        let start_script = format!("touch '{}'; exec sleep 60", ready_file.display());
        let probe_script = format!("test -f '{}'", ready_file.display());
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec!["/bin/sh".into(), "-c".into(), probe_script]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec!["/bin/sh".into(), "-c".into(), start_script]),
            ..Default::default()
        };

        let first_pid =
            match ensure_service_at("restart-svc", &spec, tmp.path(), &HashMap::new()).unwrap() {
                EnsureOutcome::Started { pid, .. } => pid,
                other => panic!("expected Started, got {other:?}"),
            };
        assert!(pid_is_alive(first_pid));

        // 模拟实例进程仍在运行但健康检查变为不健康。
        std::fs::remove_file(&ready_file).unwrap();
        assert!(pid_is_alive(first_pid));

        let second_pid =
            match ensure_service_at("restart-svc", &spec, tmp.path(), &HashMap::new()).unwrap() {
                EnsureOutcome::Started { pid, .. } => pid,
                other => panic!("expected Started replacement, got {other:?}"),
            };
        assert_ne!(first_pid, second_pid);
        assert!(
            !crate::runtime::inflight::group_alive(first_pid as i32),
            "old unhealthy instance must be terminated before replacement starts"
        );
        assert!(pid_is_alive(second_pid));
        let lease = read_lease(&lease_path(tmp.path(), "restart-svc")).unwrap();
        assert_eq!(lease.pid, second_pid);

        let _ = stop_service_at("restart-svc", &spec, tmp.path(), &HashMap::new());
    }

    #[test]
    fn stop_kills_stubborn_worker_child_even_after_leader_exits_on_sigterm() {
        let tmp = tempfile::tempdir().unwrap();
        let ready_file = tmp.path().join("pg.ready");
        let worker_pid_file = tmp.path().join("worker.pid");
        // 组长收到 SIGTERM 立刻退出，但派生的子进程忽略 SIGTERM；
        // terminate_process_group 必须等到整组退出（升级 SIGKILL 清理子进程）。
        let start_script = format!(
            "trap 'exit 0' TERM; (trap '' TERM; sleep 60) & echo $! > '{}'; touch '{}'; wait",
            worker_pid_file.display(),
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
            ..Default::default()
        };

        let leader_pid =
            match ensure_service_at("pg-svc", &spec, tmp.path(), &HashMap::new()).unwrap() {
                EnsureOutcome::Started { pid, .. } => pid,
                other => panic!("expected Started, got {other:?}"),
            };
        let worker_pid: u32 = std::fs::read_to_string(&worker_pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(pid_is_alive(worker_pid), "worker child should be alive");

        // 使用较短宽限验证：即使组长立刻退出，stop 也必须升级 SIGKILL 杀掉忽略 SIGTERM 的子进程。
        assert!(terminate_process_group(
            leader_pid,
            Duration::from_millis(250)
        ));
        assert!(
            !pid_is_alive(worker_pid),
            "stubborn worker child pid={worker_pid} must be killed by SIGKILL"
        );
        assert!(
            !crate::runtime::inflight::group_alive(leader_pid as i32),
            "entire process group must be gone"
        );
        let _ = std::fs::remove_file(lease_path(tmp.path(), "pg-svc"));
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

    #[test]
    fn one_shot_launcher_exit_zero_waits_for_external_health_and_leaves_no_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let ready_file = tmp.path().join("external.ready");
        // 模拟 launchctl kickstart / systemctl start：主命令立即 0 退出，外部守护进程 250ms 后就绪。
        let launcher_script = format!(
            "(sleep 0.25; touch {}) >/dev/null 2>&1 & exit 0",
            ready_file.display()
        );
        let probe_script = format!("test -f {}", ready_file.display());
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec!["/bin/sh".into(), "-c".into(), probe_script]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec!["/bin/sh".into(), "-c".into(), launcher_script]),
            ..Default::default()
        };

        let outcome = ensure_service_at("oneshot-svc", &spec, tmp.path(), &HashMap::new()).unwrap();
        assert!(
            matches!(outcome, EnsureOutcome::StartedExternal { .. }),
            "expected StartedExternal, got {outcome:?}"
        );
        assert!(
            !lease_path(tmp.path(), "oneshot-svc").exists(),
            "one-shot launcher must not leave a summon lease"
        );
        assert_eq!(
            stop_service_at("oneshot-svc", &spec, tmp.path(), &HashMap::new()).unwrap(),
            StopOutcome::ExternallyManaged
        );
    }

    #[test]
    fn ensure_service_fails_closed_and_cleans_up_when_lease_write_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("spawned.pid");
        let ready_file = tmp.path().join("never-used.ready");
        // 预先把租约目标路径占成目录，使 atomic_write rename 必然失败。
        std::fs::create_dir_all(lease_path(tmp.path(), "lease-fail-svc")).unwrap();

        let start_script = format!(
            "echo $$ > {}; touch {}; exec sleep 60",
            pid_file.display(),
            ready_file.display()
        );
        let probe_script = format!("test -f {}", ready_file.display());
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec!["/bin/sh".into(), "-c".into(), probe_script]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec!["/bin/sh".into(), "-c".into(), start_script]),
            ..Default::default()
        };

        let err =
            ensure_service_at("lease-fail-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(
            matches!(err, ServiceError::Io { .. }),
            "expected Io error on lease write failure, got {err:?}"
        );
        if let Ok(raw_pid) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = raw_pid.trim().parse::<u32>() {
                assert!(
                    !pid_is_alive(pid),
                    "spawned process pid={pid} must be terminated when lease write fails"
                );
            }
        }
    }

    #[test]
    fn stop_and_ensure_refuse_to_signal_when_lease_pgid_or_service_name_mismatches() {
        let tmp = tempfile::tempdir().unwrap();
        let ready_file = tmp.path().join("tamper.ready");
        let start_script = format!("touch {}; exec sleep 60", ready_file.display());
        let probe_script = format!("test -f {}", ready_file.display());
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec!["/bin/sh".into(), "-c".into(), probe_script]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec!["/bin/sh".into(), "-c".into(), start_script]),
            ..Default::default()
        };

        let pid = match ensure_service_at("tamper-svc", &spec, tmp.path(), &HashMap::new()).unwrap()
        {
            EnsureOutcome::Started { pid, .. } => pid,
            other => panic!("expected Started, got {other:?}"),
        };

        // 篡改租约中的 pgid（使其与 pid 不一致），验证 stop 绝不发信号、保留租约并返回错误。
        let lp = lease_path(tmp.path(), "tamper-svc");
        let mut lease = read_lease(&lp).unwrap();
        lease.pgid = pid.saturating_add(1);
        write_lease(&lp, &lease).unwrap();

        let err = stop_service_at("tamper-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(
            matches!(err, ServiceError::LeaseOwnershipInvalid { .. }),
            "expected LeaseOwnershipInvalid, got {err:?}"
        );
        assert!(
            pid_is_alive(pid),
            "process must not be signalled on tampered pgid"
        );
        assert!(
            lp.exists(),
            "tampered lease must be kept for operator inspection"
        );

        // 还原 pgid 以便正常清理测试进程。
        lease.pgid = pid;
        write_lease(&lp, &lease).unwrap();
        assert_eq!(
            stop_service_at("tamper-svc", &spec, tmp.path(), &HashMap::new()).unwrap(),
            StopOutcome::Stopped { pid }
        );
    }

    #[test]
    fn shutdown_cancels_inflight_ephemeral_service_start_and_cleans_process() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("ephem.pid");
        let start_script = format!("echo $$ > {}; exec sleep 60", pid_file.display());
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                // 永远不健康，确保启动方处于持锁等待就绪状态。
                command: Some(vec!["/bin/sh".into(), "-c".into(), "exit 1".into()]),
                timeout_secs: 1,
                ready_timeout_secs: 30,
                ..Default::default()
            },
            command: Some(vec!["/bin/sh".into(), "-c".into(), start_script]),
            survive_daemon_restart: false,
            ..Default::default()
        };

        let shutdown = std::sync::Arc::new(AtomicBool::new(false));
        let dir_for_t1 = tmp.path().to_path_buf();
        let spec_for_t1 = spec.clone();
        let shutdown_for_t1 = std::sync::Arc::clone(&shutdown);
        let t1 = std::thread::spawn(move || {
            ensure_service_at_with_shutdown(
                "ephem-inflight",
                &spec_for_t1,
                &dir_for_t1,
                &HashMap::new(),
                &shutdown_for_t1,
            )
        });

        // 等到子进程真正起起来并落盘 PID。
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        let spawned_pid = loop {
            if let Ok(raw) = std::fs::read_to_string(&pid_file) {
                if let Ok(p) = raw.trim().parse::<u32>() {
                    break p;
                }
            }
            assert!(
                Instant::now() < wait_deadline,
                "timed out waiting for spawned pid"
            );
            std::thread::sleep(Duration::from_millis(25));
        };

        // 模拟 daemon 收到停机信号：立即取消在途临时服务并完成停机清理。
        let stop_started = Instant::now();
        stop_non_surviving_services_at_with_shutdown(tmp.path(), &shutdown);
        let res = t1.join().unwrap();
        assert!(
            matches!(res, Err(ServiceError::ShuttingDown { .. })),
            "expected ShuttingDown, got {res:?}"
        );
        assert!(
            stop_started.elapsed() < Duration::from_secs(5),
            "shutdown should abort in-flight ephemeral start promptly"
        );
        assert!(
            !pid_is_alive(spawned_pid),
            "in-flight ephemeral process pid={spawned_pid} must be terminated on shutdown"
        );
        assert!(
            !lease_path(tmp.path(), "ephem-inflight").exists(),
            "ephemeral lease must be cleaned up"
        );
    }

    #[test]
    fn tcp_probe_bounds_slow_dns_resolution_within_timeout() {
        let started = Instant::now();
        let (ok, detail) = probe_tcp_with_resolver(
            "slow-synth.invalid:18999",
            Duration::from_millis(150),
            |_host, _port| {
                std::thread::sleep(Duration::from_secs(3));
                Ok(Vec::new())
            },
        );
        let elapsed = started.elapsed();
        assert!(!ok);
        assert!(detail.contains("timed out"), "{detail}");
        assert!(
            elapsed < Duration::from_millis(800),
            "slow DNS must be bounded by timeout budget, took {elapsed:?}"
        );
    }

    #[test]
    fn service_env_never_inherits_or_expands_summon_or_feishu_secrets() {
        std::env::set_var("SUMMON_TEST_SVC_HANDOFF_TOKEN", "synth-handoff-token-77");
        std::env::set_var("FEISHU_TEST_SVC_APP_SECRET", "synth-feishu-secret-77");
        let mut spec = ServiceSpec::default();
        spec.env.insert(
            "RENAMED_HANDOFF".to_string(),
            "${SUMMON_TEST_SVC_HANDOFF_TOKEN}".to_string(),
        );
        spec.env.insert(
            "RENAMED_FEISHU".to_string(),
            "${FEISHU_TEST_SVC_APP_SECRET}".to_string(),
        );
        spec.env.insert(
            "RENAMED_GLOBAL_FEISHU".to_string(),
            "${FEISHU_OTHER_APP_SECRET}".to_string(),
        );
        spec.env
            .insert("ALLOWED_OPT".to_string(), "${SAFE_GLOBAL_VAR}".to_string());
        let mut global = HashMap::new();
        global.insert(
            "FEISHU_OTHER_APP_SECRET".to_string(),
            "synth-global-feishu-77".to_string(),
        );
        global.insert("SAFE_GLOBAL_VAR".to_string(), "ok-value".to_string());
        let env_map: HashMap<String, String> =
            build_service_env(&spec, &global).into_iter().collect();
        std::env::remove_var("SUMMON_TEST_SVC_HANDOFF_TOKEN");
        std::env::remove_var("FEISHU_TEST_SVC_APP_SECRET");
        assert!(!env_map.contains_key("SUMMON_TEST_SVC_HANDOFF_TOKEN"));
        assert!(!env_map.contains_key("FEISHU_TEST_SVC_APP_SECRET"));
        assert!(!env_map.contains_key("RENAMED_HANDOFF"));
        assert!(!env_map.contains_key("RENAMED_FEISHU"));
        assert!(!env_map.contains_key("RENAMED_GLOBAL_FEISHU"));
        assert_eq!(
            env_map.get("ALLOWED_OPT").map(String::as_str),
            Some("ok-value")
        );
    }
}
