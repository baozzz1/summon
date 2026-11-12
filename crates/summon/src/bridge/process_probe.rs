//! Hook 子进程的 Claude 祖先进程探测。
//!
//! 探测是 capability，不是身份来源：任何字段无法交叉验证时仍返回可观察信息，但
//! `process_verified=false`，handoff 会由 core fail-closed 降级为 Mirror-only。

use anyhow::{bail, Context as _, Result};
use serde::Deserialize;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;
#[cfg(not(target_os = "linux"))]
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use summon_core::bridge::{ProcessFingerprint, ResumeProfile};

const MAX_ANCESTORS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub process: Option<ProcessFingerprint>,
    pub profile: ResumeProfile,
    /// 兼容 bridge 上线前已经存活的 summon tmux：旧进程没有 managed env marker，但既有
    /// 启动协议会带 `--name summon:<tmux>`。命中时 Hook 必须跳过，避免 daemon 重启保留的
    /// `soul-*` 被误发现成外部会话。
    pub managed_origin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExactProcessStatus {
    AliveExact,
    Gone,
    PidReused(ProcessFingerprint),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactProcessLaunch {
    pub program: String,
    pub args: Vec<String>,
}

pub fn observe_exact_launch(expected: &ProcessFingerprint) -> Result<ExactProcessLaunch> {
    if !expected.is_complete() {
        bail!(summon_i18n::format!("r17855f0b6c"));
    }
    let snapshot = read_process(expected.pid).context(summon_i18n::text!("rfc0d1420cc"))?;
    let observed = ProcessFingerprint {
        pid: snapshot.pid,
        start_time: snapshot.start_time.clone(),
        executable: snapshot.executable.clone(),
    };
    let argv_program = snapshot
        .argv
        .first()
        .and_then(|program| std::fs::canonicalize(program).ok())
        .map(|program| program.to_string_lossy().into_owned());
    if observed != *expected || argv_program.as_deref() != Some(snapshot.executable.as_str()) {
        bail!(summon_i18n::format!("rbc27acd2dc"));
    }
    Ok(ExactProcessLaunch {
        program: snapshot.executable,
        args: snapshot.argv.into_iter().skip(1).collect(),
    })
}

/// 对 ticket registry 中保存的进程指纹做一次实时 exact probe。只有 `AliveExact` 能继续激活；
/// 读权限、输出解析、PID 越界等异常都返回 Err，由调用方 fail-closed 取消 binding。
pub fn probe_exact_process(expected: &ProcessFingerprint) -> Result<ExactProcessStatus> {
    if !expected.is_complete() {
        bail!(summon_i18n::format!("r17855f0b6c"));
    }
    match observe_fingerprint(expected.pid)? {
        None => Ok(ExactProcessStatus::Gone),
        Some(observed) if observed == *expected => Ok(ExactProcessStatus::AliveExact),
        Some(observed) => Ok(ExactProcessStatus::PidReused(observed)),
    }
}

/// 构造 OS exact snapshot 的只读接缝。Stage 2 召回卡在**渲染时刻**调用并把结果只落本机，
/// 点击时再以它为基线复验，避免“点击时现采 → 同一回收进程自证自洽”的 PID reuse 漏洞。
pub(crate) fn observe_exact_fingerprint(pid: u32) -> Result<Option<ProcessFingerprint>> {
    observe_fingerprint(pid)
}

/// Stage 2 活跃召回的三重身份闸。`expected` 必须来自**卡片渲染时刻**的本机 durable
/// observation，而非点击时现采；这里再交叉验证当前 OS 身份、目标 Bot 的 canonical Claude
/// executable、Claude per-PID registry 的 sid/procStart 与新鲜度。任一证据缺失都 fail-closed。
pub(crate) fn verify_active_recall_source(
    expected: &ProcessFingerprint,
    sid: &str,
    config_dir: &Path,
    expected_executable: &Path,
) -> Result<()> {
    if probe_exact_process(expected)? != ExactProcessStatus::AliveExact {
        bail!(summon_i18n::format!("r3bab33c53c"));
    }
    // 源 executable 与目标 canonical Claude 必须来自**同一安装目录**。
    // 不要求全路径相等:Claude 升级后老会话仍跑旧版本(如 2.1.222 vs 2.1.224),
    // 两者都在 `~/.local/share/claude/versions/` 下,属同一安装。registry 三重校验
    // (PID+sid+procStart)已证明源进程身份,这里只防跨安装串线。
    let source_dir = Path::new(&expected.executable).parent();
    let target_dir = expected_executable.parent();
    if source_dir != target_dir {
        bail!(summon_i18n::format!(
            "r9552f1208f",
            expected.executable,
            expected_executable.display()
        ));
    }

    let canonical_config =
        std::fs::canonicalize(config_dir).context("canonicalize Claude config")?;
    let path = config_dir
        .join("sessions")
        .join(format!("{}.json", expected.pid));
    let symlink = std::fs::symlink_metadata(&path).context("stat Claude session registry")?;
    if symlink.file_type().is_symlink() || !symlink.is_file() {
        bail!(summon_i18n::format!("rcd0f88f07a"));
    }
    let canonical = std::fs::canonicalize(&path).context("canonicalize Claude session registry")?;
    if !canonical.starts_with(&canonical_config) {
        bail!(summon_i18n::format!("rab04bcd5b6"));
    }
    let before = std::fs::metadata(&canonical).context("stat registry before")?;
    let content = std::fs::read(&canonical).context(summon_i18n::text!("rd8e0b057ef"))?;
    let after = std::fs::metadata(&canonical).context("stat registry after")?;
    if registry_metadata_identity(&before) != registry_metadata_identity(&after) {
        bail!(summon_i18n::format!("rab75244aa9"));
    }
    let entry: ClaudeSessionProcess =
        serde_json::from_slice(&content).context(summon_i18n::text!("r4f5f1e4eb3"))?;
    validate_active_recall_registry(
        expected,
        sid,
        &entry,
        after
            .modified()
            .context(summon_i18n::text!("rb86164aa49"))?,
        process_started_at(expected)?,
    )
}

fn validate_active_recall_registry(
    expected: &ProcessFingerprint,
    sid: &str,
    entry: &ClaudeSessionProcess,
    registry_modified: SystemTime,
    process_started: SystemTime,
) -> Result<()> {
    if entry.pid != expected.pid
        || entry.session_id != sid
        || !same_proc_start(&entry.proc_start, &expected.start_time)
    {
        bail!(summon_i18n::format!("r65667ca17e"));
    }
    if registry_modified <= process_started {
        bail!(summon_i18n::format!("r23435791d4"));
    }
    Ok(())
}

/// 跨源比较进程启动时刻：Claude registry 存的是 `ps -o lstart=` 原文(个位数日期双空格补位,
/// 如 `Aug  3`),summon 自己解析 ps 时按空白切分再单空格重拼。两者是同一串时刻的不同排版,
/// 裸 `==` 会让每月 1–9 号启动的会话全部被误判成 PID reuse。只归一化空白,其余仍逐字比。
fn same_proc_start(left: &str, right: &str) -> bool {
    left.split_whitespace().eq(right.split_whitespace())
}

fn registry_metadata_identity(metadata: &std::fs::Metadata) -> (u64, u64, u64, i64, i64) {
    use std::os::unix::fs::MetadataExt as _;
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    )
}

/// 活进程的启动时刻(epoch 秒);进程不存在返回 `None`。与 cmux 会话记录的
/// `pidStartSeconds` 比对用。
pub(crate) fn process_start_epoch_secs(pid: u32) -> Result<Option<i64>> {
    let Some(fingerprint) = observe_exact_fingerprint(pid)? else {
        return Ok(None);
    };
    let started = process_started_at(&fingerprint)?;
    let secs = started
        .duration_since(UNIX_EPOCH)
        .context(summon_i18n::text!("r02391aaf67"))?
        .as_secs();
    Ok(Some(secs as i64))
}

#[cfg(not(target_os = "linux"))]
fn process_started_at(expected: &ProcessFingerprint) -> Result<SystemTime> {
    use chrono::TimeZone as _;
    let naive = chrono::NaiveDateTime::parse_from_str(&expected.start_time, "%a %b %e %H:%M:%S %Y")
        .context(summon_i18n::text!("r426a82cf05"))?;
    let seconds = chrono::Utc.from_utc_datetime(&naive).timestamp();
    let seconds = u64::try_from(seconds).context(summon_i18n::text!("r73132ee5ba"))?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .context(summon_i18n::text!("r7566e17f8f"))
}

#[cfg(target_os = "linux")]
fn process_started_at(expected: &ProcessFingerprint) -> Result<SystemTime> {
    let ticks = expected
        .start_time
        .parse::<u64>()
        .context(summon_i18n::text!("rdd2faed294"))?;
    let boot_seconds = std::fs::read_to_string("/proc/stat")
        .context(summon_i18n::text!("rd01dbfbd7b"))?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .context(summon_i18n::text!("r17b470f66b"))?
        .parse::<u64>()
        .context(summon_i18n::text!("r5068902a45"))?;
    // SAFETY:sysconf 只读进程常量，不持有返回指针。
    let ticks_per_second = unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        bail!(summon_i18n::format!("r14cc12d7f6"));
    }
    let ticks_per_second = ticks_per_second as u64;
    let seconds = boot_seconds.saturating_add(ticks / ticks_per_second);
    let nanos = (ticks % ticks_per_second).saturating_mul(1_000_000_000) / ticks_per_second;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .and_then(|time| time.checked_add(Duration::from_nanos(nanos)))
        .context(summon_i18n::text!("r7566e17f8f"))
}

#[derive(Debug)]
struct ProcessSnapshot {
    pid: u32,
    ppid: u32,
    start_time: String,
    executable: String,
    argv: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSessionProcess {
    pid: u32,
    proc_start: String,
    session_id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    version: String,
}

pub fn probe(session_id: &str, config_dir: &Path, home: &Path) -> ProbeResult {
    probe_from_pid(
        std::process::id(),
        session_id,
        config_dir,
        home,
        read_process,
    )
}

/// 按原生 SID 读取仍存活的 CLI 启动参数和版本；PID、启动时刻与 registry 必须交叉一致。
pub(crate) fn observe_session_launch(
    session_id: &str,
    config_dir: &Path,
    home: &Path,
) -> Result<(ExactProcessLaunch, String, std::path::PathBuf)> {
    let mut found = None;
    for entry in std::fs::read_dir(config_dir.join("sessions"))? {
        let entry = entry?;
        let Some(pid) = entry
            .path()
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(registry) = read_session_process(config_dir, pid) else {
            continue;
        };
        if registry.session_id != session_id {
            continue;
        }
        let observed = probe_from_pid(pid, session_id, config_dir, home, read_process);
        if !observed.profile.process_verified || !observed.profile.environment_verified {
            continue;
        }
        let fingerprint = observed
            .process
            .context(summon_i18n::text!("rb33b693e8d"))?;
        let argv = read_exact_argv(fingerprint.pid)?;
        anyhow::ensure!(
            observe_fingerprint(fingerprint.pid)?.as_ref() == Some(&fingerprint),
            summon_i18n::format!("re6654c8115")
        );
        let launch = ExactProcessLaunch {
            program: fingerprint.executable,
            args: argv.into_iter().skip(1).collect(),
        };
        if found.is_some() {
            bail!(summon_i18n::format!("r2a482a6358"));
        }
        found = Some((launch, observed.profile.version, registry.cwd.into()));
    }
    found.context(summon_i18n::text!("re369887564"))
}

#[cfg(target_os = "macos")]
fn read_exact_argv(pid: u32) -> Result<Vec<String>> {
    let mut mib = [
        nix::libc::CTL_KERN,
        nix::libc::KERN_PROCARGS2,
        i32::try_from(pid)?,
    ];
    let mut size = 0usize;
    // SAFETY:只查询本进程可读取的内核参数长度，mib 和 size 均为有效可写地址。
    let status = unsafe {
        nix::libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context(summon_i18n::text!("r04f8c15302"));
    }
    anyhow::ensure!(
        size > 4 && size <= 2 * 1024 * 1024,
        summon_i18n::format!("r713dd1c88a")
    );
    let mut bytes = vec![0u8; size];
    // SAFETY:输出缓冲区独占且按内核返回的长度分配；调用后仅解析实际写入的字节。
    let status = unsafe {
        nix::libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context(summon_i18n::text!("rd833fd8891"));
    }
    bytes.truncate(size);
    decode_procargs2(&bytes)
}

#[cfg(target_os = "linux")]
fn read_exact_argv(pid: u32) -> Result<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= 2 * 1024 * 1024 && bytes.last() == Some(&0),
        summon_i18n::format!("r9dad44e912")
    );
    bytes[..bytes.len() - 1]
        .split(|byte| *byte == 0)
        .map(|part| String::from_utf8(part.to_vec()).context(summon_i18n::text!("r4e452cd5b6")))
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_exact_argv(_pid: u32) -> Result<Vec<String>> {
    bail!(summon_i18n::format!("r369edeeb8b"))
}

#[cfg(any(target_os = "macos", test))]
fn decode_procargs2(bytes: &[u8]) -> Result<Vec<String>> {
    let header: [u8; 4] = bytes
        .get(..4)
        .context(summon_i18n::text!("rd6ac7bde27"))?
        .try_into()?;
    let count = i32::from_ne_bytes(header);
    anyhow::ensure!(
        count > 0 && count <= 65_536,
        summon_i18n::format!("r067d2403e8")
    );
    let mut remaining = &bytes[4..];
    let executable_end = remaining
        .iter()
        .position(|byte| *byte == 0)
        .context(summon_i18n::text!("r07c9d99376"))?;
    remaining = &remaining[executable_end + 1..];
    let start = remaining
        .iter()
        .position(|byte| *byte != 0)
        .context(summon_i18n::text!("r2f2097e973"))?;
    remaining = &remaining[start..];
    let mut argv = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let end = remaining
            .iter()
            .position(|byte| *byte == 0)
            .context(summon_i18n::text!("rc0efff33c9"))?;
        argv.push(
            String::from_utf8(remaining[..end].to_vec())
                .context(summon_i18n::text!("r4e452cd5b6"))?,
        );
        remaining = &remaining[end + 1..];
    }
    Ok(argv)
}

fn probe_from_pid<F>(
    mut pid: u32,
    session_id: &str,
    config_dir: &Path,
    home: &Path,
    mut read: F,
) -> ProbeResult
where
    F: FnMut(u32) -> Option<ProcessSnapshot>,
{
    let mut claude = None;
    for _ in 0..MAX_ANCESTORS {
        let Some(snapshot) = read(pid) else {
            break;
        };
        let next = snapshot.ppid;
        let registry_matches =
            read_session_process(config_dir, snapshot.pid).is_some_and(|entry| {
                entry.pid == snapshot.pid
                    && entry.session_id == session_id
                    && same_proc_start(&entry.proc_start, &snapshot.start_time)
            });
        // 官方安装的 canonical 版本文件名可能是 `2.1.207` 而不是 `claude`；此时必须用
        // Claude 自己的 per-PID session registry 交叉识别，不能退回 argv 名字猜测。
        if is_claude_executable(&snapshot.executable, &snapshot.argv) || registry_matches {
            claude = Some(snapshot);
            break;
        }
        if next == 0 || next == pid {
            break;
        }
        pid = next;
    }

    let Some(snapshot) = claude else {
        return ProbeResult {
            process: None,
            profile: ResumeProfile {
                home: path_text(home),
                config_dir: path_text(config_dir),
                ..ResumeProfile::default()
            },
            managed_origin: false,
        };
    };
    // argv0 可能只是 PATH 中的 `claude`，不能拿它当 exact process identity。再从 OS 读取
    // pid/start/executable；读不到时仍保留可观察信息供 Mirror 展示，但 handoff proof 置 false。
    let exact_process = observe_fingerprint(snapshot.pid)
        .ok()
        .flatten()
        .filter(|observed| observed.start_time == snapshot.start_time);
    let process = exact_process.clone().unwrap_or_else(|| ProcessFingerprint {
        pid: snapshot.pid,
        start_time: snapshot.start_time.clone(),
        executable: snapshot.executable.clone(),
    });
    let (safe_args, unsupported_args) = parse_resume_args(&snapshot.argv);
    let registry = read_session_process(config_dir, snapshot.pid);
    let registry_verified = exact_process.is_some()
        && registry.as_ref().is_some_and(|entry| {
            entry.pid == snapshot.pid
                && entry.session_id == session_id
                && same_proc_start(&entry.proc_start, &snapshot.start_time)
                && Path::new(&entry.cwd).is_absolute()
        });
    let version = registry.map(|entry| entry.version).unwrap_or_default();
    let environment_verified = registry_verified
        && config_dir.is_absolute()
        && home.is_absolute()
        && !path_text(config_dir).is_empty()
        && !path_text(home).is_empty();
    let claude_bin = process.executable.clone();
    ProbeResult {
        process: Some(process),
        profile: ResumeProfile {
            claude_bin,
            home: path_text(home),
            config_dir: path_text(config_dir),
            version,
            safe_args,
            unsupported_args,
            process_verified: registry_verified,
            environment_verified,
            config_source: summon_core::bridge::ClaudeConfigSource::Unknown,
            provider: summon_core::bridge::ClaudeProvider::Unknown,
        },
        managed_origin: has_summon_session_name(&snapshot.argv),
    }
}

fn has_summon_session_name(argv: &[String]) -> bool {
    argv.iter().enumerate().any(|(index, arg)| {
        arg.strip_prefix("--name=")
            .is_some_and(|name| name.starts_with("summon:") && name.len() > "summon:".len())
            || (arg == "--name"
                && argv.get(index + 1).is_some_and(|name| {
                    name.starts_with("summon:") && name.len() > "summon:".len()
                }))
    })
}

fn read_session_process(config_dir: &Path, pid: u32) -> Option<ClaudeSessionProcess> {
    let content = std::fs::read(config_dir.join("sessions").join(format!("{pid}.json"))).ok()?;
    serde_json::from_slice(&content).ok()
}

fn path_text(path: &Path) -> String {
    path.to_str().unwrap_or_default().to_string()
}

fn is_claude_executable(executable: &str, argv: &[String]) -> bool {
    let executable_name = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str());
    let argv_name = argv
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str());
    executable_name == Some("claude") || argv_name == Some("claude")
}

/// 只记录值域简单的参数；未知参数只记 flag 名或占位符，不把 `--settings <json>`、prompt
/// 等潜在 secret 原文写入 bridge state。Takeover 不重放这些 source argv，恢复命令只来自
/// 目标 Bot profile；这里的结果仅用于脱敏诊断与 managed-origin 识别。
fn parse_resume_args(argv: &[String]) -> (Vec<String>, Vec<String>) {
    let args = argv.get(1..).unwrap_or_default();
    let mut safe = Vec::new();
    let mut unsupported = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (flag, inline_value) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(flag, value)| (flag, Some(value)));
        let safe_flag = matches!(flag, "--model" | "--effort" | "--permission-mode");
        if safe_flag {
            let value = inline_value.or_else(|| args.get(index + 1).map(String::as_str));
            if value.is_some_and(|value| is_safe_value(flag, value)) {
                safe.push(flag.to_string());
                safe.push(value.unwrap_or_default().to_string());
                if inline_value.is_none() {
                    index += 1;
                }
            } else {
                push_unique(&mut unsupported, flag);
            }
        } else if flag.starts_with('-') {
            // 未知 flag 可能把 secret 直接粘在短参数后（如 `-pSECRET`）；不保存原 token。
            push_unique(&mut unsupported, "<unsupported-flag>");
        } else {
            push_unique(&mut unsupported, "<positional>");
        }
        index += 1;
    }
    (safe, unsupported)
}

fn is_safe_value(flag: &str, value: &str) -> bool {
    let simple = !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'));
    simple
        && match flag {
            "--effort" => matches!(value, "low" | "medium" | "high" | "max"),
            "--permission-mode" => matches!(
                value,
                "default" | "acceptEdits" | "plan" | "bypassPermissions" | "dontAsk"
            ),
            "--model" => true,
            _ => false,
        }
}

fn push_unique(items: &mut Vec<String>, value: &str) {
    if !items.iter().any(|item| item == value) {
        items.push(value.to_string());
    }
}

#[cfg(target_os = "linux")]
fn observe_fingerprint(pid: u32) -> Result<Option<ProcessFingerprint>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context(summon_i18n::text!("r8c4b036b05")),
    };
    let close = stat.rfind(')').context(summon_i18n::text!("rad82992306"))?;
    let fields = stat
        .get(close + 2..)
        .context(summon_i18n::text!("r89ca6a6963"))?
        .split_whitespace()
        .collect::<Vec<_>>();
    let start_time = fields.get(19).context(summon_i18n::text!("rdd26434707"))?;
    let executable = std::fs::read_link(format!("/proc/{pid}/exe"))
        .context(summon_i18n::text!("rdfa89f2e01"))?
        .to_string_lossy()
        .into_owned();
    Ok(Some(ProcessFingerprint {
        pid,
        start_time: (*start_time).to_string(),
        executable,
    }))
}

#[cfg(not(target_os = "linux"))]
fn observe_fingerprint(pid: u32) -> Result<Option<ProcessFingerprint>> {
    let output = Command::new("ps")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .args([
            "-ww",
            "-o",
            "lstart=",
            "-o",
            "comm=",
            "-p",
            &pid.to_string(),
        ])
        .output()
        .context(summon_i18n::text!("r4931fdcdfc"))?;
    if !output.status.success() {
        let stderr =
            std::str::from_utf8(&output.stderr).context(summon_i18n::text!("r6d706e34ab"))?;
        // macOS 对合法但不存在的 PID 返回 status=1 且 stdout/stderr 都空；任何其它失败都不能
        // 被解释为进程退出，否则权限/解析故障会把陈旧 ticket 错当安全。
        if output.stdout.is_empty() && stderr.is_empty() {
            return Ok(None);
        }
        bail!(summon_i18n::format!("r461f7b16a1", stderr.trim()));
    }
    let text = String::from_utf8(output.stdout).context(summon_i18n::text!("rd9a84e47fd"))?;
    let (start_time, _ps_executable) =
        parse_exact_ps_snapshot(text.trim()).context(summon_i18n::text!("r13c3ce8b60"))?;
    #[cfg(target_os = "macos")]
    // None = 两步探测之间进程退出了(ESRCH)。这不是「读不到」,是「它不在了」—— 直接判 Gone,
    // 别让一个正在死的进程把接管卡在「无法证明已退出」上。
    let executable = match macos_running_executable(pid)? {
        Some(exe) => exe,
        None => return Ok(None),
    };
    #[cfg(not(target_os = "macos"))]
    let executable = _ps_executable;
    Ok(Some(ProcessFingerprint {
        pid,
        start_time,
        executable,
    }))
}

/// `Ok(None)` = **进程确已不存在**(`ESRCH`);`Err` = 探测本身失败(权限等),仍 fail-closed。
///
/// 这个区分是必须的。macOS 的指纹探测是**两步**:先 `ps` 看在不在,再 `proc_pidpath` 取可执行
/// 路径。两步之间进程若退出,`proc_pidpath` 返回 0/`ESRCH` —— 旧实现把它和权限失败一样收成 `Err`,
/// 上层再统一映射成 `ProcessCheck::Unreadable`,于是 `/bridge` 接管报
/// 「SIGTERM 后无法证明 source 已退出;拒绝自动 resume」而**进程其实已经退了**(owner 2026-08-07 实测)。
///
/// 而这个竞态窗口恰恰在 SIGTERM 等待循环里最大 —— 那个循环每 50ms 探一次,探的正是**一个正在死的
/// 进程**。ESRCH 不是「读不到」,它就是我们要找的答案本身:它不在了。
///
/// 实测(本机):对已退出 PID,`proc_pidpath` 返回 `0`、`errno=3 (No such process)`。
#[cfg(target_os = "macos")]
fn macos_running_executable(pid: u32) -> Result<Option<String>> {
    let mut buffer = vec![0_u8; nix::libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY:buffer 在调用期间独占且容量按 libproc 上限分配；pid 已由调用方限制为正数。
    let written = unsafe {
        nix::libc::proc_pidpath(
            pid as nix::libc::c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if written <= 0 {
        let err = std::io::Error::last_os_error();
        // 只有「没有这个进程」能被解释成退出;其余(权限不足、参数错)一律保持 fail-closed,
        // 否则一次权限故障就会被当成「源已安全退出」而放行 resume。
        if err.raw_os_error() == Some(nix::libc::ESRCH) {
            return Ok(None);
        }
        return Err(err).context("proc_pidpath exact executable");
    }
    let mut len = written as usize;
    while len > 0 && buffer[len - 1] == 0 {
        len -= 1;
    }
    let path = std::ffi::OsString::from_vec(buffer[..len].to_vec());
    path.into_string()
        .map(Some)
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r9d153bae02")))
}

#[cfg(target_os = "linux")]
fn read_process(pid: u32) -> Option<ProcessSnapshot> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat.get(close + 2..)?.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let start_time = fields.get(19)?.to_string();
    let executable = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()?
        .to_string_lossy()
        .into_owned();
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    Some(ProcessSnapshot {
        pid,
        ppid,
        start_time,
        executable,
        argv,
    })
}

#[cfg(not(target_os = "linux"))]
fn read_process(pid: u32) -> Option<ProcessSnapshot> {
    // 单次 ps 快照，避免 ppid/lstart/argv 分四次采样时的 PID-reuse TOCTOU。daemon 的 PATH
    // 已在单线程启动段净化；不再写死某个平台的绝对位置。
    let output = Command::new("ps")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .args([
            "-ww",
            "-o",
            "ppid=",
            "-o",
            "lstart=",
            "-o",
            "args=",
            "-p",
            &pid.to_string(),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let snapshot = String::from_utf8(output.stdout).ok()?;
    let (ppid, start_time, argv) = parse_ps_snapshot(snapshot.trim())?;
    let executable = argv.first()?.clone();
    Some(ProcessSnapshot {
        pid,
        ppid,
        start_time,
        executable,
        argv,
    })
}

#[cfg(not(target_os = "linux"))]
fn parse_ps_snapshot(line: &str) -> Option<(u32, String, Vec<String>)> {
    // lstart 在 C locale 恒为五段：`Fri Jul 10 23:24:07 2026`。余下部分原样重新拼接后交给
    // quote-aware lexer；这样 argv 中的空格/引号不会与前置固定字段混淆。
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 7 {
        return None;
    }
    let ppid = fields[0].parse().ok()?;
    let start_time = fields[1..6].join(" ");
    let argv = split_command_line(&fields[6..].join(" "))?;
    (!argv.is_empty()).then_some((ppid, start_time, argv))
}

#[cfg(not(target_os = "linux"))]
fn parse_exact_ps_snapshot(line: &str) -> Option<(String, String)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 6 {
        return None;
    }
    let start_time = fields[..5].join(" ");
    // macOS `comm=` 返回完整 executable 路径但不会为其中空格加引号；五段 lstart 之后的
    // 全部原样拼回路径。这里没有 argv，不能交给命令行 lexer 按空格再拆。
    let executable = fields[5..].join(" ");
    (!executable.is_empty()).then_some((start_time, executable))
}

/// `ps args` 没有 NUL 分隔；这个 lexer 只为恢复普通引号/反斜杠。无法闭合时返回 None，
/// 让整个 process profile 未验证，而不是猜测参数边界。
#[cfg(not(target_os = "linux"))]
fn split_command_line(input: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        args.push(current);
    }
    Some(args)
}

#[cfg(test)]
mod tests {

    /// 2026-08-07 owner 实测:`/bridge` 本地接力报「SIGTERM 后无法证明 source 已退出;拒绝自动
    /// resume」,而终端进程其实**早就退了**。
    ///
    /// 根因是 macOS 指纹探测的两步竞态:先 `ps` 看在不在,再 `proc_pidpath` 取可执行路径;两步之间
    /// 进程退出 → `proc_pidpath` 返回 0/`ESRCH` → 旧实现和权限失败一样收成 `Err` → 上层映射成
    /// `Unreadable` → 拒绝 resume。而这个窗口恰恰在 SIGTERM 等待循环里最大 —— 那循环每 50ms 探一次,
    /// 探的正是一个**正在死的**进程。
    ///
    /// 本测试用一个真实已退出的 PID 走完整探测链,断言它判 `Gone` 而不是 Err。
    #[test]
    fn procargs2_preserves_spaces_quotes_empty_arguments_and_excludes_environment() {
        let args = [
            "/bin/claude",
            "--settings",
            "/tmp/config with space.json",
            "",
            r#"{"autoContinueAtUsageLimit":false}"#,
        ];
        let mut bytes = (args.len() as i32).to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/claude\0\0\0");
        for arg in args {
            bytes.extend_from_slice(arg.as_bytes());
            bytes.push(0);
        }
        bytes.extend_from_slice(b"KEY=private\0");
        assert_eq!(decode_procargs2(&bytes).unwrap(), args);
        assert!(decode_procargs2(&[]).is_err());
        bytes[..4].copy_from_slice(&99i32.to_ne_bytes());
        assert!(decode_procargs2(&bytes).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn exact_os_argv_preserves_settings_path_and_inline_json_boundaries() {
        let args = [
            "-c",
            "read value",
            "probe",
            "--settings",
            "/tmp/config with space.json",
            r#"{"autoContinueAtUsageLimit":false}"#,
            "",
        ];
        let mut child = std::process::Command::new("/bin/sh")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let observed = read_exact_argv(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            observed.unwrap().into_iter().skip(1).collect::<Vec<_>>(),
            args
        );
    }

    #[test]
    fn session_launch_uses_exact_live_registry_version_and_rejects_stale_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        std::fs::create_dir_all(config.join("sessions")).unwrap();
        let process = observe_exact_fingerprint(std::process::id())
            .unwrap()
            .unwrap();
        let path = config
            .join("sessions")
            .join(format!("{}.json", process.pid));
        let mut entry = serde_json::json!({
            "pid": process.pid, "procStart": process.start_time,
            "sessionId": "sid-test-settings", "cwd": tmp.path(), "version": "2.1.233",
        });
        std::fs::write(&path, entry.to_string()).unwrap();
        let (launch, version, cwd) =
            observe_session_launch("sid-test-settings", &config, tmp.path()).unwrap();
        assert_eq!(version, "2.1.233");
        assert_eq!(cwd, tmp.path());
        assert_eq!(launch.program, process.executable);
        assert!(observe_session_launch("another-sid", &config, tmp.path()).is_err());
        entry["procStart"] = "stale-start".into();
        std::fs::write(path, entry.to_string()).unwrap();
        assert!(observe_session_launch("sid-test-settings", &config, tmp.path()).is_err());
    }

    #[test]
    fn exited_process_probes_as_gone_not_unreadable() {
        // 起一个立刻退出的子进程,拿到它退出后的 pid —— 真实的「已退出 PID」,不是编造的数字。
        let child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn /usr/bin/true");
        let pid = child.id();
        let mut child = child;
        child.wait().expect("wait");
        // 收割后 pid 已不存在(除非被立刻复用 —— 那种情况下面的断言会看到 PidReused,同样不是 Err)。
        let observed = observe_fingerprint(pid);
        assert!(
            observed.is_ok(),
            "已退出进程不该让探测整体失败(那会被上层判成 Unreadable 并拒绝 resume):{:?}",
            observed.err()
        );
        // 指纹用一个不可能匹配的 start_time,确保走到「Gone / PidReused」而非 AliveExact。
        let expected = summon_core::bridge::ProcessFingerprint {
            pid,
            start_time: "Thu Jan  1 00:00:00 1970".to_string(),
            executable: "/usr/bin/true".to_string(),
        };
        match probe_exact_process(&expected) {
            Ok(ExactProcessStatus::Gone) | Ok(ExactProcessStatus::PidReused(_)) => {}
            other => panic!("已退出 PID 应判 Gone(或 PID 复用),不该是 {other:?}"),
        }
    }
    use super::*;

    fn find_real_transcript(config_dir: &Path, session_id: &str) -> Option<std::path::PathBuf> {
        let expected = format!("{session_id}.jsonl");
        let mut stack = vec![config_dir.join("projects")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().and_then(|name| name.to_str()) == Some(&expected) {
                    return std::fs::canonicalize(path).ok();
                }
            }
        }
        None
    }

    fn real_claude_contexts() -> Vec<(ClaudeSessionProcess, ProcessSnapshot)> {
        let home = dirs::home_dir().expect("真实 smoke 需要 HOME");
        let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let entries = std::fs::read_dir(config_dir.join("sessions"))
            .expect("真实 smoke 需要至少一个 Claude sessions registry");
        entries
            .flatten()
            .filter_map(|entry| {
                let registry = std::fs::read(entry.path()).ok()?;
                let registry: ClaudeSessionProcess = serde_json::from_slice(&registry).ok()?;
                let snapshot = read_process(registry.pid)?;
                is_claude_executable(&snapshot.executable, &snapshot.argv)
                    .then_some((registry, snapshot))
            })
            .collect()
    }

    #[test]
    fn exact_probe_accepts_only_the_same_live_fingerprint() {
        let observed = observe_fingerprint(std::process::id())
            .unwrap()
            .expect("测试进程应存活");
        assert_eq!(
            probe_exact_process(&observed).unwrap(),
            ExactProcessStatus::AliveExact
        );
        let mut stale = observed;
        stale.start_time.push_str("-stale");
        assert!(matches!(
            probe_exact_process(&stale).unwrap(),
            ExactProcessStatus::PidReused(_)
        ));
        assert!(probe_exact_process(&ProcessFingerprint {
            pid: 0,
            start_time: String::new(),
            executable: String::new(),
        })
        .is_err());
    }

    #[test]
    fn active_recall_registry_requires_exact_sid_process_and_fresh_mtime() {
        let expected = ProcessFingerprint {
            pid: 42,
            start_time: "start-42".into(),
            executable: "/opt/claude".into(),
        };
        let entry = ClaudeSessionProcess {
            pid: 42,
            proc_start: "start-42".into(),
            session_id: "sid-42".into(),
            cwd: "/tmp".into(),
            version: "test".into(),
        };
        let started = UNIX_EPOCH + Duration::from_secs(100);
        let fresh = UNIX_EPOCH + Duration::from_secs(101);
        assert!(
            validate_active_recall_registry(&expected, "sid-42", &entry, fresh, started).is_ok()
        );

        let stale = UNIX_EPOCH + Duration::from_secs(99);
        assert!(
            validate_active_recall_registry(&expected, "sid-42", &entry, stale, started).is_err()
        );
        assert!(
            validate_active_recall_registry(&expected, "other-sid", &entry, fresh, started)
                .is_err()
        );
        let mut reused = entry;
        reused.proc_start = "other-start".into();
        assert!(
            validate_active_recall_registry(&expected, "sid-42", &reused, fresh, started).is_err()
        );
    }

    /// 与 cmux 会话记录的 `pidStartSeconds` 比对的前提:epoch 必须是真 UTC 秒,
    /// 不能被本机时区或 lstart 排版带偏。
    #[cfg(target_os = "macos")]
    #[test]
    fn current_process_start_epoch_is_recent_utc_seconds() {
        let started = process_start_epoch_secs(std::process::id())
            .expect("probe")
            .expect("current process is alive");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(started <= now, "启动时间不能在未来:{started} > {now}");
        assert!(now - started < 86_400, "测试进程不会跑超过一天:{started}");
    }

    /// `ps -o lstart=` 对个位数日期用双空格补位(`Aug  3`),Claude 把这串原样写进 registry;
    /// summon 自己解析 ps 时按空白切分再单空格重拼。两者指同一时刻,不能判成 PID reuse ——
    /// 否则每月 1–9 号的活跃接管必挂。
    #[test]
    fn registry_proc_start_matches_across_ps_day_padding() {
        let expected = ProcessFingerprint {
            pid: 42,
            start_time: "Mon Aug 3 02:27:32 2026".into(),
            executable: "/opt/claude".into(),
        };
        let entry = ClaudeSessionProcess {
            pid: 42,
            proc_start: "Mon Aug  3 02:27:32 2026".into(),
            session_id: "sid-42".into(),
            cwd: "/tmp".into(),
            version: "test".into(),
        };
        let started = UNIX_EPOCH + Duration::from_secs(100);
        let fresh = UNIX_EPOCH + Duration::from_secs(101);
        assert!(
            validate_active_recall_registry(&expected, "sid-42", &entry, fresh, started).is_ok()
        );

        // 归一化只吃空白,真实时刻不同仍必须拒绝。
        let mut other_day = entry;
        other_day.proc_start = "Mon Aug  4 02:27:32 2026".into();
        assert!(
            validate_active_recall_registry(&expected, "sid-42", &other_day, fresh, started)
                .is_err()
        );
    }

    #[test]
    fn canonical_version_binary_is_identified_across_ps_day_padding() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".claude");
        std::fs::create_dir_all(config.join("sessions")).unwrap();
        std::fs::write(
            config.join("sessions/30.json"),
            r#"{"pid":30,"procStart":"Mon Aug  3 02:27:32 2026","sessionId":"sid","cwd":"/work","version":"2.1.220"}"#,
        )
        .unwrap();
        let result = probe_from_pid(30, "sid", &config, root.path(), |pid| {
            (pid == 30).then(|| ProcessSnapshot {
                pid,
                ppid: 1,
                start_time: "Mon Aug 3 02:27:32 2026".into(),
                executable: "/home/user/.local/share/claude/versions/2.1.220".into(),
                argv: vec![
                    "/home/user/.local/share/claude/versions/2.1.220".into(),
                    "--resume".into(),
                    "sid".into(),
                ],
            })
        });
        assert_eq!(
            result.process.as_ref().map(|process| process.pid),
            Some(30),
            "registry 交叉识别不能被 ps 日期补位破坏"
        );
    }

    #[test]
    fn argv_allowlist_never_persists_unknown_values() {
        let argv = vec![
            "/opt/claude".into(),
            "--model=sonnet".into(),
            "--effort".into(),
            "high".into(),
            "--settings".into(),
            "{\"apiKey\":\"secret\"}".into(),
            "private prompt".into(),
            "-pVERY_SECRET".into(),
        ];
        let (safe, unsupported) = parse_resume_args(&argv);
        assert_eq!(safe, ["--model", "sonnet", "--effort", "high"]);
        assert_eq!(unsupported, ["<unsupported-flag>", "<positional>"]);
        let debug = format!("{unsupported:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("private prompt"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn splits_quoted_ps_command_line() {
        assert_eq!(
            split_command_line("/opt/claude --model 'claude sonnet' --effort=high").unwrap(),
            ["/opt/claude", "--model", "claude sonnet", "--effort=high"]
        );
        assert!(split_command_line("claude 'broken").is_none());
        let (ppid, started, argv) =
            parse_ps_snapshot("  42 Fri Jul 10 23:24:07 2026 /opt/claude --model 'claude sonnet'")
                .unwrap();
        assert_eq!(ppid, 42);
        assert_eq!(started, "Fri Jul 10 23:24:07 2026");
        assert_eq!(argv, ["/opt/claude", "--model", "claude sonnet"]);
        let (started, executable) =
            parse_exact_ps_snapshot("Fri Jul 10 23:24:07 2026 /Applications/Claude Code/claude")
                .unwrap();
        assert_eq!(started, "Fri Jul 10 23:24:07 2026");
        assert_eq!(executable, "/Applications/Claude Code/claude");
    }

    #[test]
    fn unverifiable_ancestor_is_mirror_only() {
        let result =
            probe_from_pid(
                40,
                "sid",
                Path::new("/cfg"),
                Path::new("/home"),
                |pid| match pid {
                    40 => Some(ProcessSnapshot {
                        pid,
                        ppid: 30,
                        start_time: "child".into(),
                        executable: "/bin/sh".into(),
                        argv: vec!["sh".into()],
                    }),
                    30 => Some(ProcessSnapshot {
                        pid,
                        ppid: 1,
                        start_time: "start".into(),
                        executable: "/opt/claude".into(),
                        argv: vec!["/opt/claude".into()],
                    }),
                    _ => None,
                },
            );
        assert_eq!(result.process.as_ref().map(|p| p.pid), Some(30));
        assert!(!result.profile.process_verified);
        assert!(!result.profile.environment_verified);
        assert!(!result.managed_origin);
    }

    #[test]
    fn pre_upgrade_summon_name_is_recognized_as_managed() {
        let result = probe_from_pid(30, "sid", Path::new("/cfg"), Path::new("/home"), |pid| {
            (pid == 30).then(|| ProcessSnapshot {
                pid,
                ppid: 1,
                start_time: "start".into(),
                executable: "/opt/claude".into(),
                argv: vec![
                    "/opt/claude".into(),
                    "--name".into(),
                    "summon:soul-coder-chat-1".into(),
                ],
            })
        });
        assert!(result.managed_origin);
        assert!(has_summon_session_name(&[
            "claude".into(),
            "--name=summon:soul-a".into()
        ]));
        assert!(!has_summon_session_name(&[
            "claude".into(),
            "--name".into(),
            "personal".into()
        ]));
    }

    #[test]
    fn canonical_version_binary_is_identified_by_exact_session_registry() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".claude");
        std::fs::create_dir_all(config.join("sessions")).unwrap();
        std::fs::write(
            config.join("sessions/30.json"),
            r#"{"pid":30,"procStart":"start","sessionId":"sid","cwd":"/work","version":"2.1.207"}"#,
        )
        .unwrap();
        let result = probe_from_pid(30, "sid", &config, root.path(), |pid| {
            (pid == 30).then(|| ProcessSnapshot {
                pid,
                ppid: 1,
                start_time: "start".into(),
                executable: "/home/user/.local/share/claude/versions/2.1.207".into(),
                argv: vec![
                    "/home/user/.local/share/claude/versions/2.1.207".into(),
                    "--resume".into(),
                    "sid".into(),
                ],
            })
        });
        assert_eq!(result.process.as_ref().map(|process| process.pid), Some(30));
        assert!(
            !result.profile.process_verified,
            "fake PID 没有 OS exact proof"
        );
    }

    /// 只读真机 smoke：不启动 Claude、不发送 prompt、不修改 settings。它把 Claude 自己的
    /// sessions registry、OS 进程指纹和 transcript locator 做三方交叉验证。
    #[test]
    #[ignore = "需要至少一个本机存活的 Claude Code；只读且不发送模型请求"]
    fn external_bridge_real_claude_registry_process_smoke() {
        let home = std::fs::canonicalize(dirs::home_dir().expect("真实 smoke 需要 HOME")).unwrap();
        let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let config_dir = std::fs::canonicalize(config_dir).unwrap();
        let contexts = real_claude_contexts();
        assert!(!contexts.is_empty(), "没有可验证的存活 Claude Code");

        let mut transcript_verified = false;
        for (registry, snapshot) in contexts {
            let result = probe_from_pid(
                snapshot.pid,
                &registry.session_id,
                &config_dir,
                &home,
                read_process,
            );
            assert!(result.profile.process_verified);
            assert!(result.profile.environment_verified);
            let process = result.process.unwrap();
            // 跨源比较,排版可能不同(见 same_proc_start)。
            assert!(same_proc_start(&process.start_time, &registry.proc_start));
            assert!(Path::new(&process.executable).is_absolute());
            assert!(Path::new(&registry.cwd).is_absolute());
            transcript_verified |=
                find_real_transcript(&config_dir, &registry.session_id).is_some();
        }
        assert!(
            transcript_verified,
            "存活 registry 未找到任何同 SID transcript"
        );
    }

    /// 只读验证 Stage 2 的 registry mtime + exact executable 新鲜度闸；不发信号、不启动会话。
    #[test]
    #[ignore = "需要至少一个本机存活的 Claude Code；只读且不发送模型请求"]
    fn active_recall_real_source_identity_guard_smoke() {
        let home = std::fs::canonicalize(dirs::home_dir().expect("真实 smoke 需要 HOME")).unwrap();
        let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let config_dir = std::fs::canonicalize(config_dir).unwrap();
        let (registry, snapshot) = real_claude_contexts()
            .into_iter()
            .next()
            .expect("没有可验证的存活 Claude Code");
        let expected = observe_exact_fingerprint(snapshot.pid)
            .unwrap()
            .expect("Claude source 应存活");
        let canonical_program =
            std::fs::canonicalize(snapshot.argv.first().expect("Claude source argv0 应存在"))
                .unwrap();
        verify_active_recall_source(
            &expected,
            &registry.session_id,
            &config_dir,
            &canonical_program,
        )
        .unwrap();
    }

    /// 本 smoke 只验证真实 `--resume SID` 进程可由 locator/profile 精确识别；生产 Takeover
    /// 仍必须逐次取得 source-exit、staging ownership 与本次 SessionStart ACK，不能复用 smoke。
    #[test]
    #[ignore = "需要至少一个以 --resume 启动的存活 Claude Code；只读且不发送模型请求"]
    fn external_bridge_real_resumed_process_locator_smoke() {
        let home = std::fs::canonicalize(dirs::home_dir().expect("真实 smoke 需要 HOME")).unwrap();
        let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let config_dir = std::fs::canonicalize(config_dir).unwrap();
        let resumed = real_claude_contexts()
            .into_iter()
            .find(|(registry, snapshot)| {
                snapshot.argv.windows(2).any(|args| {
                    args.first().is_some_and(|arg| arg == "--resume")
                        && args.get(1) == Some(&registry.session_id)
                }) || snapshot
                    .argv
                    .iter()
                    .any(|arg| arg == &format!("--resume={}", registry.session_id))
            });
        let (registry, snapshot) = resumed.expect("没有存活的 `claude --resume SID` 进程");
        let result = probe_from_pid(
            snapshot.pid,
            &registry.session_id,
            &config_dir,
            &home,
            read_process,
        );
        assert!(result.profile.process_verified);
        assert!(result.profile.environment_verified);
        let process = result.process.unwrap();
        // 跨源比较,排版可能不同(见 same_proc_start)。
        assert!(same_proc_start(&process.start_time, &registry.proc_start));
        assert!(Path::new(&process.executable).is_absolute());
        assert!(find_real_transcript(&config_dir, &registry.session_id).is_some());
    }
}
