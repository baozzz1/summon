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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_INFLIGHT_DNS_RESOLVERS: usize = 16;
static INFLIGHT_DNS_RESOLVERS: AtomicUsize = AtomicUsize::new(0);

/// daemon 停机协调器：先冻结新的临时服务启动，再等待所有在途临时服务完成撤销与进程组回收。
struct ShutdownCoordinator {
    state: Mutex<ShutdownState>,
    cv: Condvar,
}

struct ShutdownState {
    shutting_down: bool,
    inflight_ephemeral: usize,
}

struct EphemeralStartGuard<'a> {
    coordinator: &'a ShutdownCoordinator,
}

impl Drop for EphemeralStartGuard<'_> {
    fn drop(&mut self) {
        let mut st = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        st.inflight_ephemeral = st.inflight_ephemeral.saturating_sub(1);
        if st.inflight_ephemeral == 0 {
            self.coordinator.cv.notify_all();
        }
    }
}

impl ShutdownCoordinator {
    fn new() -> Self {
        Self {
            state: Mutex::new(ShutdownState {
                shutting_down: false,
                inflight_ephemeral: 0,
            }),
            cv: Condvar::new(),
        }
    }

    fn is_shutting_down(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutting_down
    }

    fn begin_ephemeral<'a>(
        &'a self,
        service: &str,
    ) -> Result<EphemeralStartGuard<'a>, ServiceError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.shutting_down {
            return Err(ServiceError::ShuttingDown {
                service: service.to_string(),
            });
        }
        st.inflight_ephemeral += 1;
        Ok(EphemeralStartGuard { coordinator: self })
    }

    /// 置位停机并等待所有在途临时服务启动完成撤销回收；返回 `true` 表示全部归零。
    fn begin_shutdown_and_wait_inflight(&self, timeout: Duration) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.shutting_down = true;
        let deadline = Instant::now() + timeout;
        while st.inflight_ephemeral > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, _) = self
                .cv
                .wait_timeout(st, remaining)
                .unwrap_or_else(|e| e.into_inner());
            st = guard;
        }
        true
    }
}

static SHUTDOWN_COORDINATOR: LazyLock<ShutdownCoordinator> =
    LazyLock::new(ShutdownCoordinator::new);
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
    #[error("{}", summon_i18n::format!("services.cleanup_incomplete", service = .service, pid = .pid, reason = .reason))]
    CleanupIncomplete {
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

/// 按 `services_dir`（`<summon_root>/run/services`）定位对应的 `agents` 目录并收集全部在册飞书凭据键名。
fn blocked_identity_keys_for_services_dir(services_dir: Option<&Path>) -> HashSet<String> {
    let agents_dir = services_dir
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|root| root.join("agents"))
        .unwrap_or_else(|| summon_home().join("agents"));
    crate::runtime::daemon::registered_feishu_identity_keys(&agents_dir)
}

/// 构造隔离后的服务进程环境：
/// 最小系统白名单（不含任何 `SUMMON_*`/`FEISHU_*`，并剔除 lark-cli wrapper 目录）+ 服务自身 `spec.env`
/// （禁止继承、覆盖或换名引用任何运行时保留键或在册 agent 自定义飞书凭据键）。
fn build_service_env(
    spec: &ServiceSpec,
    global_env: &HashMap<String, String>,
    blocked_identity_keys: &HashSet<String>,
) -> Vec<(String, String)> {
    let mut env_map = service_base_system_env();

    let lookup = |name: &str| -> Option<String> {
        if is_runtime_owned_env_key(name) || blocked_identity_keys.contains(name) {
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
        if is_runtime_owned_env_key(key) || blocked_identity_keys.contains(key) {
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

fn probe_kind_label(spec: &ServiceSpec) -> &'static str {
    match spec.healthcheck.kind() {
        Some(HealthcheckKind::Http(_)) => "http",
        Some(HealthcheckKind::Tcp(_)) => "tcp",
        Some(HealthcheckKind::Command(_)) => "command",
        None => "invalid",
    }
}

/// 执行一次服务健康探测（有界超时，不产生持久副作用）。
#[cfg(test)]
pub(crate) fn probe_service(
    spec: &ServiceSpec,
    global_env: &HashMap<String, String>,
) -> ProbeResult {
    let blocked = blocked_identity_keys_for_services_dir(None);
    probe_service_with_timeout(spec, global_env, &blocked, spec.healthcheck.timeout()).0
}

fn probe_service_with_timeout(
    spec: &ServiceSpec,
    global_env: &HashMap<String, String>,
    blocked_identity_keys: &HashSet<String>,
    timeout: Duration,
) -> (ProbeResult, &'static str) {
    let started = Instant::now();
    let timeout = timeout.max(Duration::from_millis(100));
    let Some(kind) = spec.healthcheck.kind() else {
        return (
            ProbeResult {
                healthy: false,
                detail: "invalid healthcheck configuration".to_string(),
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
            "invalid_config",
        );
    };

    let (healthy, detail, category) = match kind {
        HealthcheckKind::Http(url) => probe_http(url, timeout),
        HealthcheckKind::Tcp(addr) => probe_tcp(addr, timeout),
        HealthcheckKind::Command(argv) => probe_command(
            argv,
            spec.working_dir.as_deref(),
            spec,
            global_env,
            blocked_identity_keys,
            timeout,
        ),
    };

    (
        ProbeResult {
            healthy,
            detail,
            elapsed_ms: started.elapsed().as_millis() as u64,
        },
        category,
    )
}

fn probe_http(url: &str, timeout: Duration) -> (bool, String, &'static str) {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    // 回环地址绕过全局 HTTP_PROXY/HTTPS_PROXY，避免代理环境变量误拦截本机健康探针。
    if url_is_loopback(url) {
        builder = builder.no_proxy();
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(_) => return (false, "HTTP client build error".to_string(), "client_error"),
    };
    match client.get(url).send() {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                (true, format!("HTTP {}", status.as_u16()), "ok")
            } else {
                (false, format!("HTTP {}", status.as_u16()), "status_error")
            }
        }
        Err(e) => {
            let clean = e.without_url();
            if clean.is_timeout() {
                (false, "HTTP request timed out".to_string(), "timeout")
            } else if clean.is_connect() {
                (false, "HTTP connect failed".to_string(), "connect_failed")
            } else {
                (false, "HTTP request failed".to_string(), "request_failed")
            }
        }
    }
}

struct DnsSlotGuard<'a> {
    counter: &'a AtomicUsize,
}

impl Drop for DnsSlotGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_acquire_dns_slot<'a>(counter: &'a AtomicUsize, cap: usize) -> Option<DnsSlotGuard<'a>> {
    let mut cur = counter.load(Ordering::Acquire);
    loop {
        if cur >= cap {
            return None;
        }
        match counter.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(DnsSlotGuard { counter }),
            Err(next) => cur = next,
        }
    }
}

fn probe_tcp(addr: &str, timeout: Duration) -> (bool, String, &'static str) {
    probe_tcp_with_resolver_and_cap(
        addr,
        timeout,
        MAX_INFLIGHT_DNS_RESOLVERS,
        &INFLIGHT_DNS_RESOLVERS,
        |host, port| {
            (host, port)
                .to_socket_addrs()
                .map(|iter| iter.collect())
                .map_err(|e| format!("{:?}", e.kind()))
        },
    )
}

fn probe_tcp_with_resolver_and_cap<F>(
    addr: &str,
    timeout: Duration,
    max_inflight: usize,
    inflight_counter: &'static AtomicUsize,
    resolver: F,
) -> (bool, String, &'static str)
where
    F: FnOnce(String, u16) -> Result<Vec<SocketAddr>, String> + Send + 'static,
{
    let Some((host, port)) = parse_tcp_host_port(addr) else {
        return (false, "invalid TCP address".to_string(), "invalid_addr");
    };
    let deadline = Instant::now() + timeout;
    let addrs: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let Some(slot) = try_acquire_dns_slot(inflight_counter, max_inflight) else {
            return (
                false,
                "TCP DNS resolver capacity exhausted".to_string(),
                "dns_capacity_exhausted",
            );
        };
        let host_owned = host.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("svc-dns-probe".to_string())
            .spawn(move || {
                let _slot = slot;
                let _ = tx.send(resolver(host_owned, port));
            })
            .is_err()
        {
            return (
                false,
                "TCP DNS resolver thread spawn failed".to_string(),
                "dns_spawn_failed",
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Ok(list)) => list,
            Ok(Err(_)) => {
                return (false, "TCP DNS resolution failed".to_string(), "dns_failed");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return (
                    false,
                    "TCP DNS resolution timed out".to_string(),
                    "dns_timeout",
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return (
                    false,
                    "TCP DNS resolution aborted".to_string(),
                    "dns_aborted",
                );
            }
        }
    };
    if addrs.is_empty() {
        return (
            false,
            "TCP DNS resolution returned no addresses".to_string(),
            "dns_empty",
        );
    }
    let mut last_kind = std::io::ErrorKind::Other;
    for sock_addr in addrs {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (
                false,
                "TCP connect timed out".to_string(),
                "connect_timeout",
            );
        }
        match TcpStream::connect_timeout(&sock_addr, remaining) {
            Ok(_) => return (true, format!("TCP {sock_addr} reachable"), "ok"),
            Err(e) => last_kind = e.kind(),
        }
    }
    (
        false,
        format!("TCP connect failed ({last_kind:?})"),
        "connect_failed",
    )
}

fn probe_command(
    argv: &[String],
    working_dir: Option<&str>,
    spec: &ServiceSpec,
    global_env: &HashMap<String, String>,
    blocked_identity_keys: &HashSet<String>,
    timeout: Duration,
) -> (bool, String, &'static str) {
    let Some(program) = argv.first().filter(|p| !p.trim().is_empty()) else {
        return (false, "empty healthcheck command".to_string(), "empty_cmd");
    };
    let program = expand_tilde(program);
    let mut cmd = Command::new(&program);
    // 探针输出可能包含由 `services.env` 传入的敏感值；不采集也不回显 stdout/stderr，
    // 错误详情仅保留退出状态或超时分类，杜绝凭据进入 daemon 日志或群聊卡片。
    cmd.args(&argv[1..])
        .env_clear()
        .envs(build_service_env(spec, global_env, blocked_identity_keys))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(dir) = working_dir.filter(|d| !d.trim().is_empty()) {
        cmd.current_dir(expand_tilde(dir));
    }

    let mut child = match retry_text_file_busy(|| cmd.spawn()) {
        Ok(c) => c,
        Err(e) => {
            return (
                false,
                format!("probe command spawn failed: {:?}", e.kind()),
                "spawn_failed",
            );
        }
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
                        "timeout",
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = killpg(pgid, Signal::SIGKILL);
                let _ = child.wait();
                return (
                    false,
                    format!("probe command wait failed: {:?}", e.kind()),
                    "wait_failed",
                );
            }
        }
    };

    if status.success() {
        (true, "command exit 0".to_string(), "ok")
    } else {
        (
            false,
            format!("command exited with {status}"),
            "nonzero_exit",
        )
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

#[derive(Debug)]
enum LeaseRead {
    Missing,
    Present(ServiceLease),
    Unreadable(String),
}

fn read_lease(path: &Path) -> LeaseRead {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LeaseRead::Missing,
        Err(_) => {
            return LeaseRead::Unreadable(
                summon_i18n::text!("services.lease_reason_unreadable").to_string(),
            );
        }
    };
    match serde_json::from_str::<ServiceLease>(&raw) {
        Ok(lease) => LeaseRead::Present(lease),
        Err(_) => LeaseRead::Unreadable(
            summon_i18n::text!("services.lease_reason_unreadable").to_string(),
        ),
    }
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
    /// 进程存活但归属无法核实（缺少或读不出指纹、服务名不符、pgid != pid 或系统 PGID 不符），
    /// 必须 fail-closed：绝不发信号，保留租约并返回错误。
    UnverifiedLive { pid: u32, reason: String },
}

/// 严格校验租约归属：将服务名、PID、租约 PGID、系统 `getpgid(pid)` 与启动指纹绑定在同一进程上。
fn verify_lease_ownership(expected_service: &str, lease: &ServiceLease) -> LeaseOwnership {
    verify_lease_ownership_with_fingerprint(expected_service, lease, read_process_fingerprint)
}

fn verify_lease_ownership_with_fingerprint<F>(
    expected_service: &str,
    lease: &ServiceLease,
    read_fp: F,
) -> LeaseOwnership
where
    F: FnOnce(u32) -> Option<String>,
{
    if !pid_is_alive(lease.pid) {
        return LeaseOwnership::Stale;
    }
    let Some(expected_fp) = lease.process_fingerprint.as_deref() else {
        return LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: summon_i18n::text!("services.lease_reason_missing_fingerprint").to_string(),
        };
    };
    let Some(actual_fp) = read_fp(lease.pid) else {
        // 进程若此刻已退出则确认为陈旧；若仍存活但读不出指纹则无法证明归属，必须保留租约。
        if !pid_is_alive(lease.pid) {
            return LeaseOwnership::Stale;
        }
        return LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: summon_i18n::text!("services.lease_reason_fingerprint_unreadable").to_string(),
        };
    };
    if actual_fp != expected_fp {
        return LeaseOwnership::Stale;
    }
    if lease.service != expected_service {
        return LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: summon_i18n::format!(
                "services.lease_reason_service_mismatch",
                actual = lease.service,
                expected = expected_service
            ),
        };
    }
    if lease.pgid != lease.pid {
        return LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: summon_i18n::format!(
                "services.lease_reason_pgid_mismatch",
                pgid = lease.pgid,
                pid = lease.pid
            ),
        };
    }
    let Ok(raw_pid) = i32::try_from(lease.pid) else {
        return LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: summon_i18n::text!("services.lease_reason_pid_out_of_range").to_string(),
        };
    };
    match getpgid(Some(Pid::from_raw(raw_pid))) {
        Ok(actual_pgid) if actual_pgid.as_raw() == raw_pid => {
            LeaseOwnership::Owned { pid: lease.pid }
        }
        Ok(actual_pgid) => LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: summon_i18n::format!(
                "services.lease_reason_os_pgid_mismatch",
                actual = actual_pgid.as_raw(),
                pid = lease.pid
            ),
        },
        Err(Errno::ESRCH) => LeaseOwnership::Stale,
        Err(e) => LeaseOwnership::UnverifiedLive {
            pid: lease.pid,
            reason: summon_i18n::format!("services.lease_reason_os_pgid_error", err = e),
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
    ensure_service_at_inner(
        name,
        spec,
        services_dir,
        global_env,
        &SHUTDOWN_COORDINATOR,
        write_lease,
    )
}

fn ensure_service_at_inner<W>(
    name: &str,
    spec: &ServiceSpec,
    services_dir: &Path,
    global_env: &HashMap<String, String>,
    coordinator: &ShutdownCoordinator,
    lease_writer: W,
) -> Result<EnsureOutcome, ServiceError>
where
    W: Fn(&Path, &ServiceLease) -> anyhow::Result<()>,
{
    let issues = spec.validate(name);
    if !issues.is_empty() {
        return Err(ServiceError::InvalidConfig {
            service: name.to_string(),
            reason: issues.join("; "),
        });
    }

    let blocked_keys = blocked_identity_keys_for_services_dir(Some(services_dir));
    let kind_label = probe_kind_label(spec);

    // 热路径无锁快速探活：服务已健康（launchd/systemd/已拉起）时零锁开销立即放行。
    let (initial, _) =
        probe_service_with_timeout(spec, global_env, &blocked_keys, spec.healthcheck.timeout());
    if initial.healthy {
        return Ok(EnsureOutcome::AlreadyHealthy { probe: initial });
    }

    // 非存活服务先在停机协调器登记在途启动：一旦停机已开始则立即拒绝；
    // 停机方会等待本 guard drop（撤销与整组回收完成后）才结束停机。
    let _ephemeral_guard = if !spec.survive_daemon_restart {
        Some(coordinator.begin_ephemeral(name)?)
    } else {
        None
    };

    // 慢路径：取每服务独占 flock，防多轮并发重复拉起。
    let lock_budget = spec.healthcheck.ready_timeout() + Duration::from_secs(5);
    let _lock = acquire_service_lock(services_dir, name, lock_budget)?;

    if !spec.survive_daemon_restart && coordinator.is_shutting_down() {
        return Err(ServiceError::ShuttingDown {
            service: name.to_string(),
        });
    }

    // 拿锁后双检：等待锁期间可能已被并发轮次或外部拉起就绪。
    let (recheck, mut last_probe_category) =
        probe_service_with_timeout(spec, global_env, &blocked_keys, spec.healthcheck.timeout());
    if recheck.healthy {
        return Ok(EnsureOutcome::AlreadyHealthy { probe: recheck });
    }

    let Some(argv) = spec.start_command() else {
        return Err(ServiceError::ProbeOnlyUnhealthy {
            service: name.to_string(),
            detail: recheck.detail,
        });
    };

    // 已有租约时先严格核验归属（三态区分：缺失 / 可读 / 损坏不可读）：
    // - 损坏或不可读 → 绝不删不覆盖，fail-closed 报错；
    // - 归属吻合（服务名/PID/PGID/指纹全一致）→ 先受控停掉旧组再重启；停不掉则 fail-closed 保租约；
    // - 存活但归属无法核实 → 拒绝发信号、保租约、fail-closed 报错；
    // - 陈旧（进程已死或 PID 被复用）→ 仅删陈旧租约。
    let lp = lease_path(services_dir, name);
    match read_lease(&lp) {
        LeaseRead::Missing => {}
        LeaseRead::Unreadable(reason) => {
            return Err(ServiceError::LeaseOwnershipInvalid {
                service: name.to_string(),
                pid: 0,
                reason,
            });
        }
        LeaseRead::Present(old_lease) => match verify_lease_ownership(name, &old_lease) {
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
        },
    }

    let svc_log = log_path(services_dir, name);
    let program = expand_tilde(&argv[0]);
    let program_label = Path::new(&program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("service")
        .to_string();

    // 子进程 stdout/stderr 一律丢弃，绝不把可能含凭据的原始进程输出落盘；
    // 本机 0600 日志文件只记录脱敏后的结构化生命周期事件。
    let mut cmd = Command::new(&program);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(build_service_env(spec, global_env, &blocked_keys))
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
            error: format!("{:?}", e.kind()),
        })?;
    let pid = child.id();
    append_service_lifecycle_log(
        &svc_log,
        &format!(
            "spawn service={name} program={program_label} pid={pid} oneshot={}",
            spec.oneshot
        ),
    );

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

    // 前台自管模式（`oneshot == false`）下，存活子进程必须取得有效启动指纹并原子落盘租约；
    // 任一环节失败时必须回收整组并校验回收是否收敛。`oneshot == true` 交由外部管理器托管，不写自管租约。
    if !spec.oneshot && !child_exited_cleanly {
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
                    let group_gone = terminate_process_group_reaping(
                        pid,
                        Duration::from_secs(2),
                        Some(&mut child),
                    );
                    append_service_lifecycle_log(
                        &svc_log,
                        &format!(
                            "aborted: failed to read process fingerprint group_gone={group_gone}"
                        ),
                    );
                    if !group_gone {
                        return Err(ServiceError::CleanupIncomplete {
                            service: name.to_string(),
                            pid,
                            reason: "fingerprint_unavailable".to_string(),
                        });
                    }
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
            if let Err(e) = lease_writer(&lp, &lease) {
                let group_gone =
                    terminate_process_group_reaping(pid, Duration::from_secs(2), Some(&mut child));
                if group_gone {
                    let _ = remove_lease_if_pid_matches(&lp, pid);
                }
                append_service_lifecycle_log(
                    &svc_log,
                    &format!("aborted: failed to persist service lease group_gone={group_gone}"),
                );
                if !group_gone {
                    return Err(ServiceError::CleanupIncomplete {
                        service: name.to_string(),
                        pid,
                        reason: "lease_write_failed".to_string(),
                    });
                }
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
    let mut oneshot_healthy_probe: Option<ProbeResult> = None;

    loop {
        if !spec.survive_daemon_restart && coordinator.is_shutting_down() {
            if !child_exited_cleanly {
                let group_gone =
                    terminate_process_group_reaping(pid, Duration::from_secs(2), Some(&mut child));
                if group_gone && !spec.oneshot {
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
                    child_exited_cleanly = true;
                    if !spec.oneshot {
                        let _ = remove_lease_if_pid_matches(&lp, pid);
                    }
                    append_service_lifecycle_log(
                        &svc_log,
                        "launcher exited cleanly (exit 0); waiting for external readiness",
                    );
                }
                Ok(Some(status)) => {
                    let group_gone = terminate_process_group(pid, Duration::from_millis(200));
                    if group_gone && !spec.oneshot {
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
                    last_probe_detail = format!("try_wait failed: {:?}", e.kind());
                    last_probe_category = "wait_failed";
                }
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        // `oneshot: true` 必须同时满足：启动命令以 0 退出 AND 探活健康（无论二者谁先发生）。
        if spec.oneshot {
            if oneshot_healthy_probe.is_none() || child_exited_cleanly {
                let probe_timeout = spec.healthcheck.timeout().min(remaining);
                let (probe, category) =
                    probe_service_with_timeout(spec, global_env, &blocked_keys, probe_timeout);
                if probe.healthy {
                    oneshot_healthy_probe = Some(probe);
                } else {
                    oneshot_healthy_probe = None;
                    last_probe_detail = probe.detail;
                    last_probe_category = category;
                }
            }
            if !child_exited_cleanly {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => {
                        child_exited_cleanly = true;
                        append_service_lifecycle_log(&svc_log, "launcher exited cleanly (exit 0)");
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
            }
            if child_exited_cleanly {
                if let Some(probe) = oneshot_healthy_probe.take() {
                    if !spec.survive_daemon_restart && coordinator.is_shutting_down() {
                        append_service_lifecycle_log(&svc_log, "aborted during daemon shutdown");
                        return Err(ServiceError::ShuttingDown {
                            service: name.to_string(),
                        });
                    }
                    append_service_lifecycle_log(
                        &svc_log,
                        &format!(
                            "ready kind={kind_label} mode=oneshot elapsed_ms={}",
                            spawn_started.elapsed().as_millis()
                        ),
                    );
                    log(&format!(
                        "[services] oneshot launcher for `{name}` handed off to external manager ({})",
                        probe.detail
                    ));
                    return Ok(EnsureOutcome::StartedExternal {
                        probe,
                        log_path: svc_log,
                    });
                }
            }
            let sleep_for =
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now()));
            if sleep_for.is_zero() {
                break;
            }
            std::thread::sleep(sleep_for);
            continue;
        }

        let probe_timeout = spec.healthcheck.timeout().min(remaining);
        let (probe, category) =
            probe_service_with_timeout(spec, global_env, &blocked_keys, probe_timeout);
        if probe.healthy {
            if !spec.survive_daemon_restart && coordinator.is_shutting_down() {
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
            // 探活转绿后再复查一次 `try_wait()`，防止启动命令在探测期间已经退出而误留自管租约。
            if !child_exited_cleanly {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => {
                        child_exited_cleanly = true;
                        let _ = remove_lease_if_pid_matches(&lp, pid);
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
                    Ok(None) | Err(_) => {}
                }
            }
            append_service_lifecycle_log(
                &svc_log,
                &format!(
                    "ready kind={kind_label} mode={} elapsed_ms={}",
                    if child_exited_cleanly {
                        "external"
                    } else {
                        "managed"
                    },
                    spawn_started.elapsed().as_millis()
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
        last_probe_category = category;

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
        if group_gone && !spec.oneshot {
            let _ = remove_lease_if_pid_matches(&lp, pid);
        }
    }
    if spec.oneshot && !child_exited_cleanly && oneshot_healthy_probe.is_some() {
        last_probe_detail = "oneshot launcher did not exit before timeout".to_string();
        last_probe_category = "launcher_timeout";
    }
    append_service_lifecycle_log(
        &svc_log,
        &format!(
            "ready timeout kind={kind_label} secs={} reason={last_probe_category}",
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
    if matches!(read_lease(path), LeaseRead::Present(l) if l.pid == expected_pid) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// 停止指定服务：**仅停止由 summon 兜底拉起且服务名/PID/PGID/指纹全部吻合的进程组**；
/// 外部托管服务一律拒绝操作；损坏租约或归属无法核实的存活租约 fail-closed 报错并保留租约。
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
    match read_lease(&lp) {
        LeaseRead::Missing => {}
        LeaseRead::Unreadable(reason) => {
            return Err(ServiceError::LeaseOwnershipInvalid {
                service: name.to_string(),
                pid: 0,
                reason,
            });
        }
        LeaseRead::Present(lease) => match verify_lease_ownership(name, &lease) {
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
        },
    }

    let blocked_keys = blocked_identity_keys_for_services_dir(Some(services_dir));
    let (probe, _) =
        probe_service_with_timeout(spec, global_env, &blocked_keys, spec.healthcheck.timeout());
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

    let blocked_keys = blocked_identity_keys_for_services_dir(Some(services_dir));
    let (probe, _) =
        probe_service_with_timeout(spec, global_env, &blocked_keys, spec.healthcheck.timeout());
    let lp = lease_path(services_dir, name);
    let ownership = match read_lease(&lp) {
        LeaseRead::Present(lease)
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

/// daemon 优雅停机钩子：先冻结临时服务的新启动入口并等待所有在途启动完成撤销与回收，再逐一停止
/// `survive_daemon_restart: false` 且经完整归属核验的 summon 兜底服务；
/// 返回 `true` 表示所有在途与已落盘临时服务均已确认收敛，可安全写干净退出标记。
pub(crate) fn stop_non_surviving_services() -> bool {
    stop_non_surviving_services_at_with_coordinator(&services_run_dir(), &SHUTDOWN_COORDINATOR)
}

#[cfg(test)]
pub(crate) fn stop_non_surviving_services_at(services_dir: &Path) -> bool {
    let coord = ShutdownCoordinator::new();
    stop_non_surviving_services_at_with_coordinator(services_dir, &coord)
}

fn stop_non_surviving_services_at_with_coordinator(
    services_dir: &Path,
    coordinator: &ShutdownCoordinator,
) -> bool {
    // 给足 35s（覆盖单次探活 30s 上限 + 进程组回收宽限）等待所有在途临时服务启动撤销完毕。
    let mut all_clean = coordinator.begin_shutdown_and_wait_inflight(Duration::from_secs(35));
    let Ok(entries) = std::fs::read_dir(services_dir) else {
        return all_clean;
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
        let lease = match read_lease(&path) {
            LeaseRead::Missing => continue,
            LeaseRead::Unreadable(reason) => {
                log(&format!(
                    "[services] unreadable lease `{file_stem}` on daemon shutdown ({reason}); keeping lease"
                ));
                all_clean = false;
                continue;
            }
            LeaseRead::Present(lease) => lease,
        };
        if lease.survive_daemon_restart {
            continue;
        }
        let Ok(_lock) = acquire_service_lock(services_dir, file_stem, Duration::from_secs(35))
        else {
            log(&format!(
                "[services] timed out waiting for service lock `{file_stem}` on daemon shutdown"
            ));
            all_clean = false;
            continue;
        };
        // 拿锁后重读租约并核对服务名/文件名/PID/PGID/指纹。
        let current = match read_lease(&path) {
            LeaseRead::Missing => continue,
            LeaseRead::Unreadable(reason) => {
                log(&format!(
                    "[services] unreadable lease `{file_stem}` under lock on daemon shutdown ({reason}); keeping lease"
                ));
                all_clean = false;
                continue;
            }
            LeaseRead::Present(current) => current,
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
                    all_clean = false;
                }
            }
            LeaseOwnership::UnverifiedLive { pid, reason } => {
                log(&format!(
                    "[services] refusing to signal unverified lease `{file_stem}` pid={pid} on daemon shutdown ({reason}); keeping lease"
                ));
                all_clean = false;
            }
            LeaseOwnership::Stale => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    all_clean
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
        let LeaseRead::Present(lease) = read_lease(&lease_path(tmp.path(), "restart-svc")) else {
            panic!("expected Present lease")
        };
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
    fn oneshot_launcher_handles_both_orders_and_nonzero_after_ready() {
        // 1. 顺序 A：启动命令先 0 退出，外部服务 250ms 后才探绿。
        let tmp = tempfile::tempdir().unwrap();
        let ready_a = tmp.path().join("order_a.ready");
        let spec_a = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("test -f {}", ready_a.display()),
                ]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                format!(
                    "(sleep 0.25; touch {}) >/dev/null 2>&1 & exit 0",
                    ready_a.display()
                ),
            ]),
            oneshot: true,
            ..Default::default()
        };
        let out_a = ensure_service_at("oneshot-a", &spec_a, tmp.path(), &HashMap::new()).unwrap();
        assert!(
            matches!(out_a, EnsureOutcome::StartedExternal { .. }),
            "{out_a:?}"
        );
        assert!(!lease_path(tmp.path(), "oneshot-a").exists());
        assert_eq!(
            stop_service_at("oneshot-a", &spec_a, tmp.path(), &HashMap::new()).unwrap(),
            StopOutcome::ExternallyManaged
        );

        // 2. 顺序 B：外部服务立刻探绿，但启动命令晚 300ms 才以 0 退出；必须等二者都满足且不留租约。
        let ready_b = tmp.path().join("order_b.ready");
        let spec_b = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("test -f {}", ready_b.display()),
                ]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("touch {}; sleep 0.3; exit 0", ready_b.display()),
            ]),
            oneshot: true,
            ..Default::default()
        };
        let out_b = ensure_service_at("oneshot-b", &spec_b, tmp.path(), &HashMap::new()).unwrap();
        assert!(
            matches!(out_b, EnsureOutcome::StartedExternal { .. }),
            "{out_b:?}"
        );
        assert!(!lease_path(tmp.path(), "oneshot-b").exists());

        // 3. 探绿后启动命令以非 0 退出：必须 fail-closed 返回 ExitedEarly 且不留租约。
        let ready_c = tmp.path().join("order_c.ready");
        let spec_c = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("test -f {}", ready_c.display()),
                ]),
                timeout_secs: 2,
                ready_timeout_secs: 5,
                ..Default::default()
            },
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("touch {}; sleep 0.25; exit 7", ready_c.display()),
            ]),
            oneshot: true,
            ..Default::default()
        };
        let err_c =
            ensure_service_at("oneshot-c", &spec_c, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(
            matches!(err_c, ServiceError::ExitedEarly { .. }),
            "{err_c:?}"
        );
        assert!(!lease_path(tmp.path(), "oneshot-c").exists());
    }

    #[test]
    fn ensure_service_fails_closed_and_cleans_up_when_lease_write_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("spawned.pid");
        let ready_file = tmp.path().join("never-used.ready");

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

        let coord = ShutdownCoordinator::new();
        let err = ensure_service_at_inner(
            "lease-fail-svc",
            &spec,
            tmp.path(),
            &HashMap::new(),
            &coord,
            |_path, _lease| Err(anyhow::anyhow!("injected lease write failure")),
        )
        .unwrap_err();
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
    fn stop_and_ensure_refuse_to_signal_on_pgid_service_name_unreadable_lease_or_unreadable_fp() {
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

        let lp = lease_path(tmp.path(), "tamper-svc");
        let orig_lease = match read_lease(&lp) {
            LeaseRead::Present(l) => l,
            other => panic!("expected Present lease, got {other:?}"),
        };

        // 1. 存活进程读不出指纹时必须判为 UnverifiedLive 而非 Stale。
        let fp_unreadable =
            summon_core::i18n::with_language(summon_core::i18n::Language::Chinese, || {
                verify_lease_ownership_with_fingerprint("tamper-svc", &orig_lease, |_| None)
            });
        assert!(
            matches!(fp_unreadable, LeaseOwnership::UnverifiedLive { ref reason, .. } if reason.contains("指纹")),
            "unreadable fingerprint on live pid must stay UnverifiedLive (localized): {fp_unreadable:?}"
        );

        // 2. 篡改租约 pgid：拒绝发信号并保留租约（且中文错误信息不含英文硬编码）。
        let mut pgid_tampered = orig_lease.clone();
        pgid_tampered.pgid = pid.saturating_add(1);
        write_lease(&lp, &pgid_tampered).unwrap();
        let err = summon_core::i18n::with_language(summon_core::i18n::Language::Chinese, || {
            stop_service_at("tamper-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err()
        });
        assert!(
            matches!(err, ServiceError::LeaseOwnershipInvalid { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("进程组"), "{err}");
        assert!(pid_is_alive(pid));
        assert!(lp.exists());

        // 3. 篡改租约服务名：拒绝发信号并保留租约。
        let mut name_tampered = orig_lease.clone();
        name_tampered.service = "other-svc".to_string();
        write_lease(&lp, &name_tampered).unwrap();
        let err = stop_service_at("tamper-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(
            matches!(err, ServiceError::LeaseOwnershipInvalid { .. }),
            "{err:?}"
        );
        assert!(pid_is_alive(pid));
        assert!(lp.exists());

        // 4. 租约 JSON 损坏不可读：stop 与 ensure 均拒绝删改覆盖。
        std::fs::write(&lp, "{broken-json").unwrap();
        let stop_err =
            stop_service_at("tamper-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(
            matches!(stop_err, ServiceError::LeaseOwnershipInvalid { .. }),
            "{stop_err:?}"
        );
        let _ = std::fs::remove_file(&ready_file);
        let ensure_err =
            ensure_service_at("tamper-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(
            matches!(ensure_err, ServiceError::LeaseOwnershipInvalid { .. }),
            "{ensure_err:?}"
        );
        assert_eq!(std::fs::read_to_string(&lp).unwrap(), "{broken-json");
        assert!(pid_is_alive(pid));

        // 还原原租约以便正常清理测试进程。
        write_lease(&lp, &orig_lease).unwrap();
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

        let coordinator = std::sync::Arc::new(ShutdownCoordinator::new());
        let dir_for_t1 = tmp.path().to_path_buf();
        let spec_for_t1 = spec.clone();
        let coord_for_t1 = std::sync::Arc::clone(&coordinator);
        let t1 = std::thread::spawn(move || {
            ensure_service_at_inner(
                "ephem-inflight",
                &spec_for_t1,
                &dir_for_t1,
                &HashMap::new(),
                &coord_for_t1,
                write_lease,
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

        // 停机函数必须在返回前等待在途临时服务完成撤销与回收（不在 join 之后才成立）。
        let stop_started = Instant::now();
        let all_clean = stop_non_surviving_services_at_with_coordinator(tmp.path(), &coordinator);
        assert!(all_clean, "shutdown should report clean convergence");
        assert!(
            stop_started.elapsed() < Duration::from_secs(5),
            "shutdown should abort in-flight ephemeral start promptly"
        );
        assert!(
            !pid_is_alive(spawned_pid),
            "ephemeral process pid={spawned_pid} must ALREADY be dead when stop_non_surviving_services returns"
        );
        assert!(
            !lease_path(tmp.path(), "ephem-inflight").exists(),
            "ephemeral lease must ALREADY be removed when stop_non_surviving_services returns"
        );

        let res = t1.join().unwrap();
        assert!(
            matches!(res, Err(ServiceError::ShuttingDown { .. })),
            "expected ShuttingDown, got {res:?}"
        );
    }

    #[test]
    fn tcp_probe_bounds_slow_dns_and_caps_concurrent_resolvers_and_sanitizes_log() {
        static TEST_DNS_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
        let started = Instant::now();
        // 连续两次慢解析占满 cap=2，第三次必须在截止时间内立即返回容量耗尽，不无上限起线程。
        let (ok1, detail1, cat1) = probe_tcp_with_resolver_and_cap(
            "synth-secret-host-99.invalid:18999",
            Duration::from_millis(120),
            2,
            &TEST_DNS_INFLIGHT,
            |_host, _port| {
                std::thread::sleep(Duration::from_millis(600));
                Ok(Vec::new())
            },
        );
        assert!(!ok1);
        assert_eq!(cat1, "dns_timeout");
        assert!(
            !detail1.contains("synth-secret-host-99"),
            "TCP error detail must not echo raw addr: {detail1}"
        );

        let (ok2, _, cat2) = probe_tcp_with_resolver_and_cap(
            "synth-secret-host-99.invalid:18999",
            Duration::from_millis(120),
            2,
            &TEST_DNS_INFLIGHT,
            |_host, _port| {
                std::thread::sleep(Duration::from_millis(600));
                Ok(Vec::new())
            },
        );
        assert!(!ok2);
        assert_eq!(cat2, "dns_timeout");

        let cap_started = Instant::now();
        let (ok3, detail3, cat3) = probe_tcp_with_resolver_and_cap(
            "synth-secret-host-99.invalid:18999",
            Duration::from_millis(120),
            2,
            &TEST_DNS_INFLIGHT,
            |_host, _port| Ok(Vec::new()),
        );
        assert!(!ok3);
        assert_eq!(cat3, "dns_capacity_exhausted");
        assert!(detail3.contains("capacity"));
        assert!(cap_started.elapsed() < Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_millis(900));

        // 含合成敏感标记的 TCP 地址超时写生命周期日志时，磁盘日志不得包含该标记。
        let tmp = tempfile::tempdir().unwrap();
        let spec = ServiceSpec {
            healthcheck: ServiceHealthcheck {
                tcp: Some("synth-tcp-log-secret-55.invalid:18999".into()),
                timeout_secs: 1,
                ready_timeout_secs: 1,
                ..Default::default()
            },
            command: Some(vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()]),
            ..Default::default()
        };
        let err = ensure_service_at("tcp-log-svc", &spec, tmp.path(), &HashMap::new()).unwrap_err();
        assert!(matches!(err, ServiceError::StartTimeout { .. }), "{err:?}");
        let log_text = std::fs::read_to_string(log_path(tmp.path(), "tcp-log-svc")).unwrap();
        assert!(
            !log_text.contains("synth-tcp-log-secret-55"),
            "lifecycle log must not contain raw TCP addr: {log_text}"
        );
        assert!(log_text.contains("ready timeout kind=tcp"), "{log_text}");
    }

    #[test]
    fn service_env_never_inherits_or_expands_summon_feishu_or_sibling_custom_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let sibling_dir = tmp.path().join("agents").join("sibling");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        std::fs::write(
            sibling_dir.join("agent.yaml"),
            r#"name: sibling
runtime:
  cli: claude
feishu:
  app_id_env: CUSTOM_SIBLING_SVC_ID
  app_secret_env: CUSTOM_SIBLING_SVC_SECRET
"#,
        )
        .unwrap();
        let services_dir = tmp.path().join("run").join("services");
        let blocked = blocked_identity_keys_for_services_dir(Some(&services_dir));
        assert!(blocked.contains("CUSTOM_SIBLING_SVC_ID"));
        assert!(blocked.contains("CUSTOM_SIBLING_SVC_SECRET"));

        std::env::set_var("SUMMON_TEST_SVC_HANDOFF_TOKEN", "synth-handoff-token-77");
        std::env::set_var("FEISHU_TEST_SVC_APP_SECRET", "synth-feishu-secret-77");
        std::env::set_var("CUSTOM_SIBLING_SVC_SECRET", "synth-custom-sibling-77");
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
        spec.env.insert(
            "RENAMED_CUSTOM_SIBLING".to_string(),
            "${CUSTOM_SIBLING_SVC_SECRET}".to_string(),
        );
        spec.env
            .insert("ALLOWED_OPT".to_string(), "${SAFE_GLOBAL_VAR}".to_string());
        let mut global = HashMap::new();
        global.insert(
            "FEISHU_OTHER_APP_SECRET".to_string(),
            "synth-global-feishu-77".to_string(),
        );
        global.insert("SAFE_GLOBAL_VAR".to_string(), "ok-value".to_string());
        let env_map: HashMap<String, String> = build_service_env(&spec, &global, &blocked)
            .into_iter()
            .collect();
        std::env::remove_var("SUMMON_TEST_SVC_HANDOFF_TOKEN");
        std::env::remove_var("FEISHU_TEST_SVC_APP_SECRET");
        std::env::remove_var("CUSTOM_SIBLING_SVC_SECRET");
        assert!(!env_map.contains_key("SUMMON_TEST_SVC_HANDOFF_TOKEN"));
        assert!(!env_map.contains_key("FEISHU_TEST_SVC_APP_SECRET"));
        assert!(!env_map.contains_key("CUSTOM_SIBLING_SVC_SECRET"));
        assert!(!env_map.contains_key("RENAMED_HANDOFF"));
        assert!(!env_map.contains_key("RENAMED_FEISHU"));
        assert!(!env_map.contains_key("RENAMED_GLOBAL_FEISHU"));
        assert!(!env_map.contains_key("RENAMED_CUSTOM_SIBLING"));
        assert_eq!(
            env_map.get("ALLOWED_OPT").map(String::as_str),
            Some("ok-value")
        );
    }
}
