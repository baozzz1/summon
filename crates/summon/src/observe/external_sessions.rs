//! Menu Bar 的外部会话发现面。
//!
//! Claude 复用 Bridge 的 `claude agents --json` 发现和 summon-managed 排除规则；Cursor/Codex
//! CLI 以带 TTY 的真实进程为存活事实，Codex 再用该 PID 打开的 rollout 精确关联线程与本轮状态。
//! Codex App 的运行态仍读 rollout，因为另起一个 App Server 只能看到自己的 loaded 状态；App
//! Server 只负责把首条 prompt 替换成稳定协议里的真正 `Thread.name`。

use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::codex::codex_app_server::{CodexAppServerClient, CodexAppServerConfig};

const CODEX_COMPLETED_LOOKBACK: Duration = Duration::from_secs(24 * 60 * 60);
const CODEX_RUNNING_FRESHNESS: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_CODEX_FILES: usize = 128;
const ROLLOUT_META_BYTES: u64 = 64 * 1024;
const ROLLOUT_PREFIX_BYTES: u64 = 512 * 1024;
const ROLLOUT_TAIL_BYTES: u64 = 2 * 1024 * 1024;
const TITLE_CHARS: usize = 80;
const MAX_TERMINAL_PROCESSES: usize = 64;
const MAX_CODEX_FILE_DESCRIPTOR: u16 = 1023;
const CODEX_TITLE_LIMIT: u32 = 200;
const CODEX_TITLE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalSessionProvider {
    Claude,
    Cursor,
    CodexCli,
    CodexApp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalSessionState {
    Running,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalSessionStatus {
    pub provider: ExternalSessionProvider,
    pub id: String,
    pub title: String,
    pub cwd: Option<String>,
    pub state: ExternalSessionState,
    /// 只有 provider 给出独立证据才算跑一轮；进程存活本身只对应 state=running。
    pub busy: bool,
    pub last_active_secs: Option<u64>,
    /// 跳到这个会话的链接。只有 Codex App 有(`codex://threads/<id>`);其余 provider 没有稳定的跳转目标。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// 外部发现是辅助观测面：任一 provider 不可用都降级为空，不能拖垮 summon fleet 快照。
pub fn discover_external_sessions(now: SystemTime) -> Vec<ExternalSessionStatus> {
    let mut sessions = discover_claude_sessions();
    let terminal_processes = discover_terminal_processes();
    sessions.extend(discover_cursor_sessions(&terminal_processes));

    let codex_home = codex_home();
    let mut codex = discover_codex_app_sessions_at(&codex_home, now);
    codex.extend(discover_codex_cli_sessions(
        &terminal_processes,
        &codex_home,
        now,
    ));
    enrich_codex_titles(&mut codex);
    sessions.extend(codex);
    sessions.sort_by(|left, right| {
        provider_rank(left.provider)
            .cmp(&provider_rank(right.provider))
            .then_with(|| right.busy.cmp(&left.busy))
            .then_with(|| {
                left.last_active_secs
                    .unwrap_or(u64::MAX)
                    .cmp(&right.last_active_secs.unwrap_or(u64::MAX))
            })
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions
}

fn provider_rank(provider: ExternalSessionProvider) -> u8 {
    match provider {
        ExternalSessionProvider::Claude => 0,
        ExternalSessionProvider::Cursor => 1,
        ExternalSessionProvider::CodexCli => 2,
        ExternalSessionProvider::CodexApp => 3,
    }
}

fn discover_claude_sessions() -> Vec<ExternalSessionStatus> {
    let mut sessions = crate::bridge::recall::discover_active_for_display()
        .into_iter()
        // background agent 没有前台进程；用户要的是终端里实际存在的纯 Claude 进程。
        .filter(|entry| entry.pid.is_some())
        .map(|entry| {
            let title = entry
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(display_title)
                .unwrap_or_else(|| fallback_title(&entry.cwd, &entry.session_id));
            ExternalSessionStatus {
                provider: ExternalSessionProvider::Claude,
                id: entry.session_id,
                title,
                cwd: non_empty(entry.cwd),
                state: ExternalSessionState::Running,
                busy: entry.status.as_deref() == Some("busy"),
                // `claude agents` 只有 startedAt，没有 last-active；不能把启动时间冒充活跃时间。
                last_active_secs: None,
                link: None,
            }
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .busy
            .cmp(&left.busy)
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalProvider {
    Cursor,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalProcess {
    pub(crate) provider: TerminalProvider,
    pub(crate) pid: u32,
    pub(crate) started_at: SystemTime,
    pub(crate) command: String,
    pub(crate) cwd: Option<String>,
    pub(crate) open_files: Vec<PathBuf>,
}

/// 一次 ps + 两次 lsof：Menu Bar 展开或 Bridge 召回时才调用，不进入常驻 fleet tick。
pub(crate) fn discover_terminal_processes() -> Vec<TerminalProcess> {
    let Ok(output) = Command::new("ps")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .args([
            "-ww", "-axo", "pid=", "-o", "tty=", "-o", "state=", "-o", "lstart=", "-o", "args=",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    let mut processes = parse_terminal_processes(&text);
    processes.truncate(MAX_TERMINAL_PROCESSES);
    let pids = processes
        .iter()
        .map(|process| process.pid)
        .collect::<Vec<_>>();
    let cwds = lsof_names(&pids, true);
    // 全 FD 只采 Codex：Cursor Node 进程打开的文件很多，对整批 PID 做 0-1023 lsof 会把
    // `/bridge` 放大到十几秒。Cursor 活跃 id 优先读命令行 `--resume`；点击时再对 selected
    // store.db 做独立 lsof 安全复验。
    let codex_pids = processes
        .iter()
        .filter(|process| process.provider == TerminalProvider::Codex)
        .map(|process| process.pid)
        .collect::<Vec<_>>();
    let open_files = lsof_names(&codex_pids, false);
    for process in &mut processes {
        process.cwd = cwds
            .get(&process.pid)
            .and_then(|names| names.first())
            .and_then(|path| path.to_str())
            .map(str::to_string)
            .and_then(non_empty);
        process.open_files = open_files.get(&process.pid).cloned().unwrap_or_default();
    }
    processes
}

fn parse_terminal_processes(text: &str) -> Vec<TerminalProcess> {
    let mut processes = text
        .lines()
        .filter_map(parse_terminal_process)
        .collect::<Vec<_>>();
    processes.sort_by_key(|process| (process.provider as u8, process.pid));
    processes
}

fn parse_terminal_process(line: &str) -> Option<TerminalProcess> {
    // C locale 的 lstart 恒为五段；前面另有 pid/tty/state 三段。
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 9 || matches!(fields[1], "??" | "?" | "-") {
        return None;
    }
    let pid = fields[0].parse().ok()?;
    let started_at = parse_ps_start(&fields[3..8])?;
    let command = fields[8..].join(" ");
    let provider = classify_terminal_command(&command)?;
    Some(TerminalProcess {
        provider,
        pid,
        started_at,
        command,
        cwd: None,
        open_files: Vec::new(),
    })
}

fn parse_ps_start(fields: &[&str]) -> Option<SystemTime> {
    let value = fields.join(" ");
    let timestamp = chrono::NaiveDateTime::parse_from_str(&value, "%a %b %e %T %Y")
        .ok()?
        .and_utc()
        .timestamp();
    let seconds = u64::try_from(timestamp).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

fn classify_terminal_command(command: &str) -> Option<TerminalProvider> {
    if let Some(args) = program_args(command, "codex") {
        let first = first_positional(
            args,
            &[
                "-c",
                "--config",
                "--remote",
                "--remote-auth-token-env",
                "-i",
                "--image",
                "-m",
                "--model",
                "--local-provider",
                "-p",
                "--profile",
                "-s",
                "--sandbox",
                "-C",
                "--cd",
                "--add-dir",
                "-a",
                "--ask-for-approval",
            ],
        );
        // 无子命令、resume、fork 才是交互会话；exec/review/app-server 等即使恰好继承
        // TTY 也不是用户要在菜单里观察的终端会话。
        let excluded = first.is_some_and(|value| {
            matches!(
                value,
                "exec"
                    | "e"
                    | "review"
                    | "login"
                    | "logout"
                    | "mcp"
                    | "plugin"
                    | "mcp-server"
                    | "app-server"
                    | "remote-control"
                    | "app"
                    | "completion"
                    | "update"
                    | "doctor"
                    | "sandbox"
                    | "debug"
                    | "apply"
                    | "a"
                    | "archive"
                    | "delete"
                    | "unarchive"
                    | "cloud"
                    | "exec-server"
                    | "features"
                    | "help"
            )
        });
        return (!excluded).then_some(TerminalProvider::Codex);
    }

    let cursor_args = cursor_cli_args(command)?;
    // `agent` 名字过于通用；官方当前包装器会 exec 自带 node + cursor-agent/index.js。
    // `cursor-agent` 本身则是明确名字，兼容未来回到原生单二进制的安装形态。
    let explicitly_cursor = program_args(command, "cursor-agent").is_some()
        || (command.contains("/cursor-agent/") && command.contains("/index.js"));
    let first = first_positional(
        cursor_args,
        &[
            "--api-key",
            "-H",
            "--header",
            "--output-format",
            "--mode",
            "--resume",
            "--model",
            "--sandbox",
            "--workspace",
            "--add-dir",
            "--plugin-dir",
            "-w",
            "--worktree",
            "--worktree-base",
        ],
    );
    let utility = first.is_some_and(|value| {
        matches!(
            value,
            "install-shell-integration"
                | "uninstall-shell-integration"
                | "login"
                | "logout"
                | "mcp"
                | "plugin"
                | "worker"
                | "status"
                | "whoami"
                | "models"
                | "bedrock"
                | "about"
                | "update"
                | "create-chat"
                | "generate-rule"
                | "rule"
                | "help"
        )
    });
    (explicitly_cursor && !utility).then_some(TerminalProvider::Cursor)
}

/// ps args 没有 NUL 分隔，但这里只需定位 argv[0] 的 basename；路径中即使有空格，也能以
/// `/name` 后紧跟空白/行尾作为边界。返回值只用于识别固定子命令，不解释引号中的 prompt。
fn program_args<'a>(command: &'a str, name: &str) -> Option<&'a str> {
    if let Some(rest) = command.strip_prefix(name) {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Some(rest.trim_start());
        }
    }
    let needle = format!("/{name}");
    for (offset, _) in command.match_indices(&needle) {
        let end = offset + needle.len();
        let rest = &command[end..];
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Some(rest.trim_start());
        }
    }
    None
}

fn first_positional<'a>(args: &'a str, options_with_value: &[&str]) -> Option<&'a str> {
    let fields = args.split_whitespace().collect::<Vec<_>>();
    let mut index = 0;
    while let Some(field) = fields.get(index).copied() {
        if field == "--" {
            return fields.get(index + 1).copied();
        }
        if field.starts_with('-') {
            let has_inline_value = field.contains('=');
            let takes_value = options_with_value.contains(&field);
            index += if takes_value && !has_inline_value {
                2
            } else {
                1
            };
            continue;
        }
        return Some(field);
    }
    None
}

fn cursor_cli_args(command: &str) -> Option<&str> {
    let wrapper_args =
        program_args(command, "cursor-agent").or_else(|| program_args(command, "agent"))?;
    if let Some(offset) = wrapper_args.find("/index.js") {
        let rest = &wrapper_args[offset + "/index.js".len()..];
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Some(rest.trim_start());
        }
    }
    Some(wrapper_args)
}

fn lsof_names(pids: &[u32], cwd_only: bool) -> HashMap<u32, Vec<PathBuf>> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let pid_list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut command = Command::new("lsof");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args(["-a", "-p", &pid_list]);
    if cwd_only {
        command.args(["-d", "cwd"]);
    } else {
        // lsof 的全量输出没有天然上界；rollout 是常规低位 FD，截到 1023 可把最坏输出
        // 限在 `MAX_TERMINAL_PROCESSES * 1024` 条记录以内。
        command.args(["-d", &format!("0-{MAX_CODEX_FILE_DESCRIPTOR}")]);
    }
    command.arg("-Fn");
    let Ok(output) = command.output() else {
        return HashMap::new();
    };
    // 某个 PID 在采样间退出时 lsof 可返回非零但仍给其它 PID 输出，保留可解析事实。
    let Ok(text) = String::from_utf8(output.stdout) else {
        return HashMap::new();
    };
    parse_lsof_names(&text)
}

fn parse_lsof_names(text: &str) -> HashMap<u32, Vec<PathBuf>> {
    let mut names: HashMap<u32, Vec<PathBuf>> = HashMap::new();
    let mut current = None;
    for line in text.lines() {
        if let Some(pid) = line.strip_prefix('p').and_then(|value| value.parse().ok()) {
            current = Some(pid);
        } else if let (Some(pid), Some(path)) = (current, line.strip_prefix('n')) {
            if path.starts_with('/') {
                names.entry(pid).or_default().push(PathBuf::from(path));
            }
        }
    }
    names
}

fn discover_cursor_sessions(processes: &[TerminalProcess]) -> Vec<ExternalSessionStatus> {
    processes
        .iter()
        .filter(|process| process.provider == TerminalProvider::Cursor)
        .map(|process| {
            let process_id = process_identity("cursor", process);
            let id = cursor_resume_id(&process.command).unwrap_or(process_id);
            ExternalSessionStatus {
                provider: ExternalSessionProvider::Cursor,
                id: id.clone(),
                title: terminal_title("Cursor", process.cwd.as_deref(), process.pid),
                cwd: process.cwd.clone(),
                state: ExternalSessionState::Running,
                // Cursor 的公开 CLI 没有 JSON 状态接口；不拿 sleeping/CPU 猜 agent turn。
                busy: false,
                last_active_secs: None,
                link: None,
            }
        })
        .collect()
}

pub(crate) fn cursor_resume_id(command: &str) -> Option<String> {
    let args = cursor_cli_args(command)?;
    let fields = args.split_whitespace().collect::<Vec<_>>();
    for (index, field) in fields.iter().enumerate() {
        if let Some(id) = field.strip_prefix("--resume=") {
            return non_empty(id.to_string());
        }
        if *field == "--resume" {
            return fields
                .get(index + 1)
                .filter(|id| !id.starts_with('-'))
                .map(|id| (*id).to_string())
                .and_then(non_empty);
        }
    }
    None
}

fn discover_codex_cli_sessions(
    processes: &[TerminalProcess],
    codex_home: &Path,
    now: SystemTime,
) -> Vec<ExternalSessionStatus> {
    processes
        .iter()
        .filter(|process| process.provider == TerminalProvider::Codex)
        .map(|process| {
            let parsed = process
                .open_files
                .iter()
                .filter(|path| is_rollout(path) && path.starts_with(codex_home))
                .filter_map(|path| {
                    let parsed = parse_codex_cli_rollout(path).ok().flatten()?;
                    let modified = path.metadata().and_then(|meta| meta.modified()).ok();
                    let event_at = parsed.last_event_at.or(modified);
                    Some((event_at, parsed))
                })
                .max_by_key(|(event_at, _)| *event_at)
                .map(|(_, parsed)| parsed);
            let fallback_id = process_identity("codex", process);
            let (id, title, cwd, busy, last_active_secs) = match parsed {
                Some(parsed) => {
                    let busy = parsed.phase == CodexPhase::Running;
                    let last_active = parsed.last_event_at.map(|at| elapsed_secs(now, at));
                    (parsed.id, parsed.title, parsed.cwd, busy, last_active)
                }
                None => (
                    fallback_id,
                    terminal_title("Codex", process.cwd.as_deref(), process.pid),
                    process.cwd.clone(),
                    false,
                    None,
                ),
            };
            ExternalSessionStatus {
                provider: ExternalSessionProvider::CodexCli,
                id,
                title,
                cwd,
                state: ExternalSessionState::Running,
                busy,
                last_active_secs,
                link: None,
            }
        })
        .collect()
}

fn process_identity(prefix: &str, process: &TerminalProcess) -> String {
    let started = process
        .started_at
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("{prefix}-{}-{started}", process.pid)
}

fn terminal_title(provider: &str, cwd: Option<&str>, pid: u32) -> String {
    cwd.and_then(|cwd| {
        Path::new(cwd)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    })
    .unwrap_or_else(|| format!("{provider} PID {pid}"))
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn discover_codex_app_sessions_at(
    codex_home: &Path,
    now: SystemTime,
) -> Vec<ExternalSessionStatus> {
    let mut candidates = Vec::new();
    collect_rollouts(&codex_home.join("sessions"), 0, &mut candidates);
    collect_rollouts(&codex_home.join("archived_sessions"), 0, &mut candidates);
    candidates.retain(|(modified, _)| within(now, *modified, CODEX_COMPLETED_LOOKBACK));
    candidates.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    // session_meta 是首条记录：预筛只读有界首行，避免大量 CLI/subagent rollout 把菜单
    // 展开的 IO 放大；截断后才为 App 候选读取标题前缀与状态尾部。
    candidates.retain(|(_, path)| codex_app_identity(path));
    candidates.truncate(MAX_CODEX_FILES);

    let mut by_id: HashMap<String, CodexObservation> = HashMap::new();
    for (modified, path) in candidates {
        let Ok(Some(parsed)) = parse_codex_rollout(&path) else {
            continue;
        };
        let (event_at, lookback) = match parsed.phase {
            CodexPhase::Running => (
                parsed.last_event_at.unwrap_or(modified),
                CODEX_RUNNING_FRESHNESS,
            ),
            CodexPhase::Completed(completed_at) => (completed_at, CODEX_COMPLETED_LOOKBACK),
            // Aborted 是同 ID 的删除标记，必须参与 normal/archive 去重；否则较旧的 Running
            // 副本会在新副本终止后继续误报。它本身最终不展示。
            CodexPhase::Aborted => (
                parsed.last_event_at.unwrap_or(modified),
                CODEX_COMPLETED_LOOKBACK,
            ),
            CodexPhase::Unknown => continue,
        };
        if !within(now, event_at, lookback) {
            continue;
        }
        let observation = CodexObservation { parsed, event_at };
        match by_id.get(&observation.parsed.id) {
            Some(existing) if prefer_existing(existing, &observation) => {}
            _ => {
                by_id.insert(observation.parsed.id.clone(), observation);
            }
        }
    }

    let mut sessions = by_id
        .into_values()
        .filter_map(|observation| {
            let parsed = observation.parsed;
            let (state, busy) = match parsed.phase {
                CodexPhase::Running => (ExternalSessionState::Running, true),
                CodexPhase::Completed(_) => (ExternalSessionState::Completed, false),
                CodexPhase::Aborted | CodexPhase::Unknown => return None,
            };
            Some(ExternalSessionStatus {
                provider: ExternalSessionProvider::CodexApp,
                link: Some(summon_core::waiting::codex_thread_link(&parsed.id)),
                id: parsed.id,
                title: parsed.title,
                cwd: parsed.cwd,
                state,
                busy,
                last_active_secs: Some(elapsed_secs(now, observation.event_at)),
            })
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .busy
            .cmp(&left.busy)
            .then_with(|| {
                left.last_active_secs
                    .unwrap_or(u64::MAX)
                    .cmp(&right.last_active_secs.unwrap_or(u64::MAX))
            })
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions
}

fn enrich_codex_titles(sessions: &mut [ExternalSessionStatus]) {
    let wanted = sessions
        .iter()
        .map(|session| session.id.clone())
        .collect::<HashSet<_>>();
    if wanted.is_empty() {
        return;
    }
    let Some(program) = resolve_codex_program() else {
        return;
    };
    let config = CodexAppServerConfig {
        program: program.to_string_lossy().into_owned(),
        request_timeout: CODEX_TITLE_TIMEOUT,
        client_name: "summon-menubar".into(),
        client_title: "Summon Menu Bar".into(),
        ..CodexAppServerConfig::default()
    };
    let Ok(mut client) = CodexAppServerClient::spawn(config) else {
        return;
    };
    let mut titles = HashMap::new();
    for archived in [false, true] {
        let Ok(page) =
            client.thread_list(archived, CODEX_TITLE_LIMIT, None, Some(CODEX_TITLE_TIMEOUT))
        else {
            continue;
        };
        for thread in page.data {
            if !wanted.contains(&thread.id) {
                continue;
            }
            let Some(name) = thread.name.as_deref().map(display_title) else {
                continue;
            };
            if !name.is_empty() {
                titles.insert(thread.id, name);
            }
        }
    }
    for session in sessions {
        if let Some(title) = titles.get(&session.id) {
            session.title.clone_from(title);
        }
    }
}

fn resolve_codex_program() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("codex")));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/codex"));
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
        PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex"),
        PathBuf::from("/Applications/Codex.app/Contents/Resources/codex"),
    ]);
    candidates.into_iter().find(|path| path.is_file())
}

fn prefer_existing(existing: &CodexObservation, incoming: &CodexObservation) -> bool {
    existing.event_at > incoming.event_at
        || (existing.event_at == incoming.event_at
            // 同一事件时间却一边仍 Running、一边已有终态时，终态是更强证据；Aborted
            // 再优先于 Completed，以免终止副本仍留下可见会话。
            && phase_rank(existing.parsed.phase) >= phase_rank(incoming.parsed.phase))
}

fn phase_rank(phase: CodexPhase) -> u8 {
    match phase {
        CodexPhase::Unknown => 0,
        CodexPhase::Running => 1,
        CodexPhase::Completed(_) => 2,
        CodexPhase::Aborted => 3,
    }
}

fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<(SystemTime, PathBuf)>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_rollouts(&entry.path(), depth + 1, out);
            continue;
        }
        if !file_type.is_file() || !is_rollout(&entry.path()) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        out.push((modified, entry.path()));
    }
}

pub(crate) fn is_rollout(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedCodexSession {
    pub(crate) id: String,
    pub(crate) cwd: Option<String>,
    pub(crate) title: String,
    pub(crate) phase: CodexPhase,
    pub(crate) last_event_at: Option<SystemTime>,
}

type CodexIdentity = (String, Option<String>);
type CodexMetaParser = fn(&[u8]) -> Option<CodexIdentity>;

#[derive(Debug)]
struct CodexObservation {
    parsed: ParsedCodexSession,
    event_at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexPhase {
    Running,
    Completed(SystemTime),
    Aborted,
    Unknown,
}

fn parse_codex_rollout(path: &Path) -> std::io::Result<Option<ParsedCodexSession>> {
    parse_codex_rollout_with(path, parse_codex_session_meta)
}

fn parse_codex_cli_rollout(path: &Path) -> std::io::Result<Option<ParsedCodexSession>> {
    parse_codex_rollout_with(path, parse_codex_cli_session_meta)
}

/// Bridge 召回只需 identity + 首条用户任务，不读 rollout 尾部。它接受 Codex CLI/exec 与
/// Codex Desktop 的顶层用户线程；subagent/automation 永远排除。三者共用 Codex 原生 UUID，
/// 当前 `codex exec resume <UUID>` 都能消费。
pub(crate) fn parse_codex_recall_metadata(
    path: &Path,
) -> std::io::Result<Option<ParsedCodexSession>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file).take(ROLLOUT_META_BYTES);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    let Some((id, cwd)) = parse_codex_recall_session_meta(&line) else {
        return Ok(None);
    };
    let mut title = None;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("event_msg")
            && value.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
        {
            title = value
                .pointer("/payload/message")
                .and_then(Value::as_str)
                .map(display_title)
                .filter(|value| !value.is_empty());
            break;
        }
    }
    Ok(Some(ParsedCodexSession {
        id,
        cwd: cwd.clone(),
        title: title.unwrap_or_else(|| fallback_title(cwd.as_deref().unwrap_or(""), "codex")),
        phase: CodexPhase::Unknown,
        last_event_at: None,
    }))
}

pub(crate) fn parse_codex_recall_session(
    path: &Path,
) -> std::io::Result<Option<ParsedCodexSession>> {
    parse_codex_rollout_with(path, parse_codex_recall_session_meta)
}

fn parse_codex_rollout_with(
    path: &Path,
    parse_meta: CodexMetaParser,
) -> std::io::Result<Option<ParsedCodexSession>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();

    let mut prefix = Vec::new();
    file.by_ref()
        .take(len.min(ROLLOUT_PREFIX_BYTES))
        .read_to_end(&mut prefix)?;
    let Some((id, cwd, title)) = parse_codex_identity_with(&prefix, parse_meta) else {
        return Ok(None);
    };

    let tail_start = len.saturating_sub(ROLLOUT_TAIL_BYTES);
    let starts_at_line_boundary = if tail_start == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(tail_start - 1))?;
        let mut previous = [0_u8; 1];
        file.read_exact(&mut previous)?;
        previous[0] == b'\n'
    };
    file.seek(SeekFrom::Start(tail_start))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    if !starts_at_line_boundary {
        // seek 可能落在 JSON 行中间；半行绝不能参与状态判断。
        if let Some(newline) = tail.iter().position(|byte| *byte == b'\n') {
            tail.drain(..=newline);
        } else {
            tail.clear();
        }
    }
    let (phase, last_event_at) = parse_codex_phase(&tail);
    Ok(Some(ParsedCodexSession {
        id,
        cwd: cwd.clone(),
        title: title.unwrap_or_else(|| fallback_title(cwd.as_deref().unwrap_or(""), "codex")),
        phase,
        last_event_at,
    }))
}

fn codex_app_identity(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file).take(ROLLOUT_META_BYTES);
    let mut first_line = Vec::new();
    reader.read_until(b'\n', &mut first_line).is_ok()
        && parse_codex_session_meta(&first_line).is_some()
}

/// Codex Desktop 当前沿用 `source=vscode` 历史名；真正的 App 边界还要同时要求
/// `originator=Codex Desktop`。cli/exec/subagent/其它 IDE 即使文件很新也不进入该 section。
fn parse_codex_identity_with(
    bytes: &[u8],
    parse_meta: CodexMetaParser,
) -> Option<(String, Option<String>, Option<String>)> {
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let identity = parse_meta(lines.next()?)?;
    let mut title = None;
    for line in lines {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if title.is_none()
            && value.get("type").and_then(Value::as_str) == Some("event_msg")
            && value.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
        {
            title = value
                .pointer("/payload/message")
                .and_then(Value::as_str)
                .map(display_title)
                .filter(|value| !value.is_empty());
        }
    }
    Some((identity.0, identity.1, title))
}

fn parse_codex_session_meta(line: &[u8]) -> Option<(String, Option<String>)> {
    parse_codex_session_meta_with(line, |payload| {
        payload.get("source").and_then(Value::as_str) == Some("vscode")
            && payload.get("originator").and_then(Value::as_str) == Some("Codex Desktop")
            && payload.get("thread_source").and_then(Value::as_str) != Some("subagent")
    })
}

fn parse_codex_cli_session_meta(line: &[u8]) -> Option<(String, Option<String>)> {
    parse_codex_session_meta_with(line, |payload| {
        payload.get("source").and_then(Value::as_str) == Some("cli")
            && payload.get("thread_source").and_then(Value::as_str) != Some("subagent")
    })
}

fn parse_codex_recall_session_meta(line: &[u8]) -> Option<(String, Option<String>)> {
    parse_codex_session_meta_with(line, |payload| {
        let source = payload.get("source").and_then(Value::as_str);
        let top_level = !matches!(
            payload.get("thread_source").and_then(Value::as_str),
            Some("subagent" | "automation")
        );
        let cli = matches!(source, Some("cli" | "exec"));
        let desktop = source == Some("vscode")
            && payload.get("originator").and_then(Value::as_str) == Some("Codex Desktop");
        top_level && (cli || desktop)
    })
}

fn parse_codex_session_meta_with(
    line: &[u8],
    accepts: impl FnOnce(&Value) -> bool,
) -> Option<(String, Option<String>)> {
    let value = serde_json::from_slice::<Value>(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    if !accepts(payload) {
        return None;
    }
    let id = payload.get("id").and_then(Value::as_str)?.trim();
    if id.is_empty() {
        return None;
    }
    Some((
        id.to_string(),
        payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
            .and_then(non_empty),
    ))
}

fn parse_codex_phase(bytes: &[u8]) -> (CodexPhase, Option<SystemTime>) {
    let mut phase = CodexPhase::Unknown;
    let mut last_event_at = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let Some(kind) = value.pointer("/payload/type").and_then(Value::as_str) else {
            continue;
        };
        let at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        if at.is_some() {
            last_event_at = at;
        }
        phase = match kind {
            "task_started" | "turn_started" => CodexPhase::Running,
            "task_complete" | "turn_completed" => {
                at.map(CodexPhase::Completed).unwrap_or(CodexPhase::Unknown)
            }
            "turn_aborted" | "task_aborted" => CodexPhase::Aborted,
            kind if is_turn_progress(kind) => CodexPhase::Running,
            _ => phase,
        };
    }
    (phase, last_event_at)
}

fn is_turn_progress(kind: &str) -> bool {
    matches!(
        kind,
        "agent_message" | "agent_reasoning" | "user_message" | "sub_agent_activity"
    ) || kind.ends_with("_start")
        || kind.ends_with("_end")
}

fn parse_timestamp(value: &str) -> Option<SystemTime> {
    let millis = chrono::DateTime::parse_from_rfc3339(value)
        .ok()?
        .timestamp_millis();
    let millis = u64::try_from(millis).ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(millis))
}

fn within(now: SystemTime, then: SystemTime, limit: Duration) -> bool {
    now.duration_since(then)
        .map(|elapsed| elapsed <= limit)
        .unwrap_or(true)
}

fn elapsed_secs(now: SystemTime, then: SystemTime) -> u64 {
    now.duration_since(then)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn display_title(value: &str) -> String {
    let one_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = one_line.chars();
    let clipped = chars.by_ref().take(TITLE_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

fn fallback_title(cwd: &str, id: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| id.chars().take(8).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn terminal_process_parser_keeps_only_interactive_cursor_and_codex() {
        let ps = concat!(
            "101 ttys001 S+ Mon Jul 20 10:00:00 2026 /Users/me/.local/bin/agent --use-system-ca /Users/me/.local/share/cursor-agent/versions/v1/index.js --resume=chat-1\n",
            "102 ttys002 S+ Mon Jul 20 10:01:00 2026 /Applications/ChatGPT.app/Contents/Resources/codex resume thread-1\n",
            "103 ttys003 S+ Mon Jul 20 10:02:00 2026 /opt/homebrew/bin/codex exec --json prompt\n",
            "104 ?? S Mon Jul 20 10:03:00 2026 /Applications/ChatGPT.app/Contents/Resources/codex app-server --listen stdio://\n",
            "105 ttys004 S+ Mon Jul 20 10:04:00 2026 /usr/local/bin/agent status\n",
            "106 ttys005 S+ Mon Jul 20 10:05:00 2026 /opt/homebrew/bin/codex -c model=foo exec --json prompt\n",
            "107 ttys006 S+ Mon Jul 20 10:06:00 2026 /Users/me/.local/bin/agent --use-system-ca /Users/me/.local/share/cursor-agent/versions/v1/index.js --api-key secret status\n",
        );
        let processes = parse_terminal_processes(ps);
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].provider, TerminalProvider::Cursor);
        assert_eq!(processes[0].pid, 101);
        assert_eq!(processes[1].provider, TerminalProvider::Codex);
        assert_eq!(processes[1].pid, 102);
        assert_eq!(
            cursor_resume_id(&processes[0].command).as_deref(),
            Some("chat-1")
        );
    }

    #[test]
    fn lsof_records_stay_bound_to_their_exact_pid() {
        let parsed = parse_lsof_names(concat!(
            "p101\nfcwd\nn/repo/a\nf9\nn/repo/a/.codex/sessions/rollout-a.jsonl\n",
            "p102\nfcwd\nn/repo/b\n",
        ));
        assert_eq!(
            parsed[&101],
            vec![
                PathBuf::from("/repo/a"),
                PathBuf::from("/repo/a/.codex/sessions/rollout-a.jsonl")
            ]
        );
        assert_eq!(parsed[&102], vec![PathBuf::from("/repo/b")]);
    }

    fn write_rollout(path: &Path, id: &str, source: &str, events: &[(&str, &str)]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(path).unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"1970-01-01T00:00:01Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/repo/{id}","source":{source},"originator":"Codex Desktop","thread_source":"user"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"1970-01-01T00:00:02Z","type":"event_msg","payload":{{"type":"user_message","message":"修复   登录页\n并补测试"}}}}"#
        )
        .unwrap();
        for (timestamp, kind) in events {
            writeln!(
                file,
                r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"{kind}","turn_id":"turn-1"}}}}"#
            )
            .unwrap();
        }
    }

    #[test]
    fn parses_only_top_level_codex_app_rollouts() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app.jsonl");
        write_rollout(
            &app,
            "app",
            r#""vscode""#,
            &[("1970-01-01T00:01:00Z", "task_started")],
        );
        let parsed = parse_codex_rollout(&app).unwrap().unwrap();
        assert_eq!(parsed.id, "app");
        assert_eq!(parsed.title, "修复 登录页 并补测试");
        assert_eq!(parsed.phase, CodexPhase::Running);

        let cli = tmp.path().join("cli.jsonl");
        write_rollout(&cli, "cli", r#""cli""#, &[]);
        assert!(parse_codex_rollout(&cli).unwrap().is_none());
        let parsed_cli = parse_codex_cli_rollout(&cli).unwrap().unwrap();
        assert_eq!(parsed_cli.id, "cli");

        let vscode = tmp.path().join("vscode.jsonl");
        write_rollout(&vscode, "vscode", r#""vscode""#, &[]);
        let content = std::fs::read_to_string(&vscode)
            .unwrap()
            .replace("Codex Desktop", "Codex VS Code");
        std::fs::write(&vscode, content).unwrap();
        assert!(parse_codex_rollout(&vscode).unwrap().is_none());

        let subagent = tmp.path().join("subagent.jsonl");
        write_rollout(&subagent, "sub", r#"{"subagent":{}}"#, &[]);
        assert!(parse_codex_rollout(&subagent).unwrap().is_none());
    }

    #[test]
    fn live_codex_process_uses_its_open_cli_rollout_for_id_and_busy_state() {
        let tmp = tempfile::tempdir().unwrap();
        let rollout = tmp.path().join("sessions/rollout-cli.jsonl");
        write_rollout(
            &rollout,
            "cli-thread",
            r#""cli""#,
            &[("1970-01-01T00:01:00Z", "task_started")],
        );
        let process = TerminalProcess {
            provider: TerminalProvider::Codex,
            pid: 42,
            started_at: at(30),
            command: "codex resume cli-thread".into(),
            cwd: Some("/invocation".into()),
            open_files: vec![rollout],
        };
        let sessions = discover_codex_cli_sessions(&[process], tmp.path(), at(100));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].provider, ExternalSessionProvider::CodexCli);
        assert_eq!(sessions[0].id, "cli-thread");
        assert_eq!(sessions[0].cwd.as_deref(), Some("/repo/cli-thread"));
        assert!(sessions[0].busy);
        assert!(
            sessions.iter().all(|s| s.link.is_none()),
            "Codex CLI 没有桌面版跳转目标"
        );
    }

    #[test]
    fn cursor_without_public_turn_signal_is_running_but_not_busy() {
        let process = TerminalProcess {
            provider: TerminalProvider::Cursor,
            pid: 7,
            started_at: at(30),
            command: "/Users/me/.local/bin/cursor-agent --resume=chat-7".into(),
            cwd: Some("/repo/project".into()),
            open_files: Vec::new(),
        };
        let sessions = discover_cursor_sessions(&[process]);
        assert_eq!(sessions[0].provider, ExternalSessionProvider::Cursor);
        assert_eq!(sessions[0].id, "chat-7");
        assert_eq!(sessions[0].title, "project");
        assert_eq!(sessions[0].state, ExternalSessionState::Running);
        assert!(!sessions[0].busy);
    }

    #[test]
    fn identity_prefilter_reads_only_the_bounded_first_record() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("rollout-app.jsonl");
        write_rollout(&app, "app", r#""vscode""#, &[]);
        assert!(codex_app_identity(&app));

        let cli_first = tmp.path().join("rollout-cli-first.jsonl");
        write_rollout(&cli_first, "cli", r#""cli""#, &[]);
        let desktop_meta = std::fs::read_to_string(&app)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&cli_first)
            .unwrap();
        writeln!(file, "{desktop_meta}").unwrap();
        assert!(!codex_app_identity(&cli_first));

        let oversized_meta = tmp.path().join("rollout-oversized-meta.jsonl");
        let mut file = File::create(&oversized_meta).unwrap();
        file.write_all(&vec![b' '; usize::try_from(ROLLOUT_META_BYTES).unwrap()])
            .unwrap();
        writeln!(file, "{desktop_meta}").unwrap();
        assert!(!codex_app_identity(&oversized_meta));
    }

    #[test]
    fn latest_turn_boundary_decides_running_completed_or_aborted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_rollout(
            &path,
            "thread",
            r#""vscode""#,
            &[
                ("1970-01-01T00:01:00Z", "task_started"),
                ("1970-01-01T00:02:00Z", "task_complete"),
                ("1970-01-01T00:03:00Z", "task_started"),
            ],
        );
        assert_eq!(
            parse_codex_rollout(&path).unwrap().unwrap().phase,
            CodexPhase::Running
        );

        write_rollout(
            &path,
            "thread",
            r#""vscode""#,
            &[
                ("1970-01-01T00:01:00Z", "task_started"),
                ("1970-01-01T00:02:00Z", "turn_aborted"),
            ],
        );
        assert_eq!(
            parse_codex_rollout(&path).unwrap().unwrap().phase,
            CodexPhase::Aborted
        );
    }

    #[test]
    fn bounded_tail_still_recognizes_a_long_running_turn_and_its_terminal_event() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_rollout(
            &path,
            "thread",
            r#""vscode""#,
            &[("1970-01-01T00:01:00Z", "task_started")],
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        for second in 0..12_000 {
            writeln!(
                file,
                r#"{{"timestamp":"1970-01-01T00:02:00Z","type":"event_msg","payload":{{"type":"agent_reasoning","text":"{second:05}-{}"}}}}"#,
                "x".repeat(180)
            )
            .unwrap();
        }
        file.flush().unwrap();
        assert!(path.metadata().unwrap().len() > ROLLOUT_TAIL_BYTES);
        assert_eq!(
            parse_codex_rollout(&path).unwrap().unwrap().phase,
            CodexPhase::Running
        );

        writeln!(
            file,
            r#"{{"timestamp":"1970-01-01T00:03:00Z","type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn-1"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();
        assert!(matches!(
            parse_codex_rollout(&path).unwrap().unwrap().phase,
            CodexPhase::Completed(_)
        ));
    }

    #[test]
    fn bounded_tail_keeps_a_terminal_event_when_seek_lands_exactly_on_its_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_rollout(
            &path,
            "thread",
            r#""vscode""#,
            &[("1970-01-01T00:01:00Z", "task_started")],
        );
        let terminal = concat!(
            r#"{"timestamp":"1970-01-01T00:03:00Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
            "\n"
        );
        assert!(terminal.len() < usize::try_from(ROLLOUT_TAIL_BYTES).unwrap());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(terminal.as_bytes()).unwrap();
        let padding = usize::try_from(ROLLOUT_TAIL_BYTES).unwrap() - terminal.len();
        file.write_all(&vec![b'\n'; padding]).unwrap();
        file.flush().unwrap();

        assert!(matches!(
            parse_codex_rollout(&path).unwrap().unwrap().phase,
            CodexPhase::Completed(_)
        ));
    }

    #[test]
    fn duplicate_normal_and_archive_choose_newer_event_not_running_bias() {
        let tmp = tempfile::tempdir().unwrap();
        let now = at(100_000);
        let normal = tmp.path().join("sessions/rollout-duplicate.jsonl");
        write_rollout(
            &normal,
            "duplicate",
            r#""vscode""#,
            &[("1970-01-02T03:30:00Z", "task_started")],
        );
        let archive = tmp.path().join("archived_sessions/rollout-duplicate.jsonl");
        write_rollout(
            &archive,
            "duplicate",
            r#""vscode""#,
            &[("1970-01-02T03:45:00Z", "task_complete")],
        );

        let sessions = discover_codex_app_sessions_at(tmp.path(), now);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, ExternalSessionState::Completed);
        assert!(!sessions[0].busy);
        assert_eq!(sessions[0].last_active_secs, Some(100));
        assert_eq!(
            sessions[0].link.as_deref(),
            Some("codex://threads/duplicate"),
            "Codex App 条目带桌面版跳转链接"
        );
    }

    #[test]
    fn newer_aborted_duplicate_hides_older_running_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let now = at(100_000);
        let normal = tmp.path().join("sessions/rollout-duplicate.jsonl");
        write_rollout(
            &normal,
            "duplicate",
            r#""vscode""#,
            &[("1970-01-02T03:30:00Z", "task_started")],
        );
        let archive = tmp.path().join("archived_sessions/rollout-duplicate.jsonl");
        write_rollout(
            &archive,
            "duplicate",
            r#""vscode""#,
            &[("1970-01-02T03:45:00Z", "turn_aborted")],
        );

        assert!(discover_codex_app_sessions_at(tmp.path(), now).is_empty());
    }

    #[test]
    fn discovers_running_and_last_day_completed_from_normal_and_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let now = at(100_000);
        let running = tmp.path().join("sessions/2026/01/01/rollout-running.jsonl");
        write_rollout(
            &running,
            "running",
            r#""vscode""#,
            &[("1970-01-02T03:45:00Z", "task_started")],
        );
        let completed = tmp.path().join("archived_sessions/rollout-completed.jsonl");
        write_rollout(
            &completed,
            "completed",
            r#""vscode""#,
            &[
                ("1970-01-02T00:00:00Z", "task_started"),
                ("1970-01-02T03:30:00Z", "task_complete"),
            ],
        );
        // 测试 fixture 的真实 mtime 是未来，collector 按时钟回拨保守纳入；最终窗口用事件时间裁决。
        let sessions = discover_codex_app_sessions_at(tmp.path(), now);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "running");
        assert!(sessions[0].busy);
        assert_eq!(sessions[1].id, "completed");
        assert_eq!(sessions[1].state, ExternalSessionState::Completed);
        assert_eq!(sessions[1].last_active_secs, Some(1_000));
    }

    #[test]
    fn excludes_stale_running_and_completed_older_than_one_day() {
        let tmp = tempfile::tempdir().unwrap();
        let now = at(200_000);
        let stale_running = tmp.path().join("sessions/rollout-stale-running.jsonl");
        write_rollout(
            &stale_running,
            "stale-running",
            r#""vscode""#,
            &[("1970-01-03T00:00:00Z", "task_started")],
        );
        let old_completed = tmp.path().join("sessions/rollout-old-completed.jsonl");
        write_rollout(
            &old_completed,
            "old-completed",
            r#""vscode""#,
            &[("1970-01-02T00:00:00Z", "task_complete")],
        );
        assert!(discover_codex_app_sessions_at(tmp.path(), now).is_empty());
    }

    #[test]
    fn clips_titles_on_unicode_character_boundaries() {
        let title = display_title(&"会".repeat(TITLE_CHARS + 1));
        assert_eq!(title.chars().count(), TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
    }
}
