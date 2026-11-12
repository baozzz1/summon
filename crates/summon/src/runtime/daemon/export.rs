//! 会话诊断包导出(IO 半):找到一个会话的各份材料,按时间窗读出,交给 `summon_core::export` 投影,
//! 打成 tar.gz 写进 `~/.summon/run/exports/`。CLI `summon session export` 与飞书 `/summon-export` 共用这一份。
//! 设计见 `docs/spec/2026-10-29-session-export-design.md`。
//!
//! 只读:不改任何状态文件,不碰会话。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context as _, Result};
use serde::Serialize;
use serde_json::{json, Value};
use summon_core::chat_meta::ChatMeta;
use summon_core::export::events::EventProjector;
use summon_core::export::native::{line_time_ms, NativeProjector};
use summon_core::export::{self, logs, Mode, People, Window, WindowSpec};
use summon_core::log_view::{self, LogFile};
use summon_core::registry::Agent;
use summon_core::turn_ledger::{self, TurnRecord};

/// 包的默认体积上限(未压缩的内容合计)。飞书文件消息上限 30MB,留出余量。
pub(crate) const DEFAULT_MAX_BYTES: u64 = 25 * 1024 * 1024;
/// 导出目录里最多留几个包。
const KEEP_BUNDLES: usize = 20;
/// 包最多留几天。
const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 3600);
/// 临时文件超过这么久还在,就是写到一半被打断留下的。
const STALE_TMP: Duration = Duration::from_secs(3600);
/// 问 CLI / tmux 版本的上限:卡住的 CLI 不能把导出一起拖住。
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// 导出读写的根:运行时是 `~/.summon/run` 与家目录,测试指向临时目录。
pub(crate) struct Roots {
    pub run: PathBuf,
    pub home: PathBuf,
    /// 问在跑的 daemon、tmux 与 CLI 版本。测试里关掉:它们读的是本机的真实状态。
    pub probe: bool,
}

impl Roots {
    pub(crate) fn real() -> Self {
        Roots {
            run: crate::runtime::paths::run_dir(),
            home: dirs::home_dir().unwrap_or_default(),
            probe: true,
        }
    }

    fn exports(&self) -> PathBuf {
        self.run.join("exports")
    }
}

/// 要导出的会话。
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub agent: Agent,
    pub tag: String,
    /// 会话名(`.meta` / resume 记录的键);盘上没有 `.meta` 时为 `None`。
    pub session: Option<String>,
    pub meta: ChatMeta,
}

impl Target {
    pub(crate) fn label(&self) -> String {
        format!("{}/{}", self.agent.name, self.tag)
    }
}

/// 这个 agent 盘上所有会话:(会话名, `.meta`, tag)。
fn sessions_of(agent: &Agent) -> Vec<(String, ChatMeta, String)> {
    let dir = agent.state_dir().join("runtime").join("chats");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let base = agent.tmux_name();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(session) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".meta"))
        else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let meta = ChatMeta::parse(&content);
        let tag = crate::session::discovery::chat_tag_for_session(&base, session, &meta);
        out.push((session.to_string(), meta, tag));
    }
    out
}

/// 某个 agent 的某个 tag。同一个 chat 有过几份 `.meta`(裸名与带 tag)时取最近更新的那份。
pub(crate) fn target_for(agent: &Agent, tag: &str) -> Target {
    let mut found: Vec<(String, ChatMeta)> = sessions_of(agent)
        .into_iter()
        .filter(|(_, _, t)| t == tag)
        .map(|(session, meta, _)| (session, meta))
        .collect();
    found.sort_by(|a, b| a.1.updated_at.cmp(&b.1.updated_at));
    let (session, meta) = found
        .pop()
        .map_or((None, ChatMeta::default()), |(s, m)| (Some(s), m));
    Target {
        agent: agent.clone(),
        tag: tag.to_string(),
        session,
        meta,
    }
}

/// 按用户给的写法找会话:`<agent>/<tag 或 chat_id>`,或者只给 tag / chat_id(只在一个 agent 下出现时)。
pub(crate) fn find_target(agents: &[Agent], spec: &str) -> Result<Target> {
    let (agent_name, key) = match spec.split_once('/') {
        Some((agent, key)) => (Some(agent), key),
        None => (None, spec),
    };
    if key.is_empty() || key.contains('/') || key.starts_with('.') {
        bail!(summon_i18n::format!("r3759f6912e"));
    }
    let mut hits: Vec<Target> = Vec::new();
    for agent in agents {
        if agent_name.is_some_and(|name| name != agent.name) {
            continue;
        }
        let mut tags: Vec<String> = sessions_of(agent)
            .into_iter()
            .filter(|(_, meta, tag)| tag == key || meta.chat_id.as_deref() == Some(key))
            .map(|(_, _, tag)| tag)
            .collect();
        if agent.state_dir().join("threads").join(key).is_dir() {
            tags.push(key.to_string());
        }
        tags.sort();
        tags.dedup();
        hits.extend(tags.iter().map(|tag| target_for(agent, tag)));
    }
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => match agent_name {
            Some(name) if !agents.iter().any(|a| a.name == name) => {
                bail!(summon_i18n::format!("rdd57e6aaea"))
            }
            _ => bail!(summon_i18n::format!("r012553a587")),
        },
        _ => bail!(summon_i18n::format!(
            "r20913e0ef9",
            hits.iter()
                .map(Target::label)
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

/// 一次导出要什么。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Request {
    pub mode: Mode,
    pub window: WindowSpec,
    pub max_bytes: u64,
}

/// 导出的结果。
#[derive(Debug)]
pub(crate) struct Bundle {
    pub path: PathBuf,
    pub manifest: Manifest,
}

/// 包里每个文件的来历。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FileNote {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// 窗内的条目数。
    pub in_window: usize,
    /// 收进包里的条目数。
    pub included: usize,
    /// 因为不在白名单 / 不是本会话 / 不是本 agent 而没收的条目数。
    pub skipped: usize,
    /// 因为体积上限从最早一端裁掉的条目数。
    pub trimmed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Manifest {
    pub format: u32,
    pub mode: Mode,
    pub summon: String,
    pub generated_at: String,
    pub agent: String,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_type: Option<String>,
    pub window: Window,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_turn: Option<String>,
    pub files: Vec<FileNote>,
    pub notes: Vec<String>,
}

/// 打包中的一个文件:按时间先后排好的条目,外加每条的时刻(在跑那一轮的条目不裁)。
struct Entry {
    note: FileNote,
    lines: Vec<String>,
    times: Vec<Option<i64>>,
}

impl Entry {
    fn new(name: &str, source: &Path, home: &str) -> Self {
        Entry {
            note: FileNote {
                name: name.to_string(),
                source: export::tilde(&source.to_string_lossy(), home),
                in_window: 0,
                included: 0,
                skipped: 0,
                trimmed: 0,
            },
            lines: Vec::new(),
            times: Vec::new(),
        }
    }

    fn generated(name: &str, body: String) -> Self {
        let mut entry = Entry::new(name, Path::new(""), "");
        entry.lines.push(body);
        entry.times.push(None);
        entry.note.in_window = 1;
        entry.note.included = 1;
        entry
    }

    fn push(&mut self, line: String, at_ms: Option<i64>) {
        self.lines.push(line);
        self.times.push(at_ms);
        self.note.included += 1;
    }

    fn bytes(&self) -> u64 {
        self.lines.iter().map(|l| l.len() as u64 + 1).sum()
    }

    fn body(&self) -> Vec<u8> {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.into_bytes()
    }
}

/// 收集材料时要用到的事实。
struct Ctx<'a> {
    roots: &'a Roots,
    target: &'a Target,
    window: Window,
    home: String,
    /// 认本会话日志用的键:tag、chat_id、窗内各轮的 turn id 与 message_id。
    keys: Vec<String>,
    /// 其它 agent 的别名:日志行首写着它们的不算本会话。
    other_agents: Vec<String>,
    running_since: Option<i64>,
}

impl Ctx<'_> {
    fn pinned(&self, at_ms: Option<i64>) -> bool {
        matches!((self.running_since, at_ms), (Some(start), Some(at)) if at >= start)
    }
}

/// 导出一个会话。
pub(crate) fn export(target: &Target, all_agents: &[String], req: &Request) -> Result<Bundle> {
    export_in(&Roots::real(), target, all_agents, req)
}

fn export_in(
    roots: &Roots,
    target: &Target,
    all_agents: &[String],
    req: &Request,
) -> Result<Bundle> {
    let now = SystemTime::now();
    let now_ms = chrono::Local::now().timestamp_millis();
    let home = roots.home.to_string_lossy().into_owned();
    let state = target.agent.state_dir();
    let thread_dir = state.join("threads").join(&target.tag);

    let turns = read_turns(&roots.run, &target.agent.name, &target.tag);
    let current = read_current_turn(&state, &target.tag)
        .filter(|(turn, _)| !turns.iter().any(|r| r.turn.as_deref() == Some(turn)));
    let starts: Vec<i64> = turns.iter().map(|r| r.started_at_ms).collect();
    let window = export::window(req.window, &starts, current.as_ref().map(|c| c.1), now_ms);

    let in_window: Vec<&TurnRecord> = turns
        .iter()
        .filter(|r| r.ended_at_ms >= window.start_ms)
        .collect();
    let mut keys = vec![target.tag.clone()];
    keys.extend(target.meta.chat_id.clone());
    for rec in &in_window {
        keys.extend(rec.turn.clone());
        keys.push(rec.message_id.clone());
    }
    keys.extend(current.as_ref().map(|c| c.0.clone()));
    keys.retain(|k| k.len() >= 6);
    keys.sort();
    keys.dedup();

    let ctx = Ctx {
        roots,
        target,
        window,
        home: home.clone(),
        keys,
        other_agents: all_agents
            .iter()
            .filter(|a| **a != target.agent.name)
            .cloned()
            .collect(),
        running_since: current.as_ref().map(|c| c.1),
    };
    let mut people = People::default();
    let mut notes = Vec::new();
    let mut files: Vec<Entry> = Vec::new();

    files.push(Entry::generated(
        "env.json",
        pretty(&env_json(&target.agent, &home, roots.probe)),
    ));
    files.push(Entry::generated(
        "config.json",
        pretty(&config_json(target, &home)),
    ));
    files.push(Entry::generated(
        "session.json",
        pretty(&session_json(
            target,
            &thread_dir,
            current.as_ref(),
            &home,
            roots.probe,
        )),
    ));

    let turns_source = roots.run.join("turns").join(&target.agent.name);
    let mut turn_entry = Entry::new("turns.ndjson", &turns_source, &home);
    for rec in &in_window {
        turn_entry.note.in_window += 1;
        let line = serde_json::to_string(&export::diag_turn(rec, &mut people))?;
        turn_entry.push(line, Some(rec.started_at_ms));
    }
    files.push(turn_entry);

    files.push(events_entry(&ctx, &thread_dir)?);
    files.push(log_entry(&ctx, LogFile::Daemon, &mut people, false));
    files.push(log_entry(&ctx, LogFile::Sidecar, &mut people, false));
    let native = native_source(target, &roots.home);
    match &native {
        Some(Native::Lines { kind, files: paths }) => {
            files.push(native_entry(&ctx, *kind, paths));
        }
        Some(Native::CursorStore(db)) => notes.push(summon_i18n::format!(
            "r5cfb4a7c98",
            export::tilde(&db.to_string_lossy(), &home)
        )),
        None => notes.push(summon_i18n::text!("r2f1450ea0a").into()),
    }

    if req.mode.is_full() {
        files.extend(full_entries(
            &ctx,
            &thread_dir,
            &in_window,
            native.as_ref(),
        )?);
    }

    let trimmed = trim_to(&mut files, &ctx, req.max_bytes);
    if !trimmed {
        notes.push(summon_i18n::format!(
            "r8d0e82c71f",
            req.max_bytes / (1024 * 1024)
        ));
    }

    let build = crate::version::build_info();
    let manifest = Manifest {
        format: 1,
        mode: req.mode,
        summon: build.describe(),
        generated_at: chrono::Local::now().to_rfc3339(),
        agent: target.agent.name.clone(),
        tag: target.tag.clone(),
        chat_id: target.meta.chat_id.clone(),
        chat_type: target.meta.chat_type.clone(),
        window,
        running_turn: current.as_ref().map(|c| c.0.clone()),
        files: files.iter().map(|f| f.note.clone()).collect(),
        notes,
    };
    let path = write_bundle(&roots.exports(), target, req.mode, &manifest, &files)?;
    prune(&roots.exports(), now);
    Ok(Bundle { path, manifest })
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// 本会话的轮记录,按开始时刻排序。台账按月分文件、只留 90 天,全读也就几个文件。
fn read_turns(run: &Path, agent: &str, tag: &str) -> Vec<TurnRecord> {
    let dir = run.join("turns").join(agent);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".ndjson"))
                .collect()
        })
        .unwrap_or_default();
    let mut out: Vec<TurnRecord> = names
        .iter()
        .flat_map(|name| {
            let text = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
            turn_ledger::parse_lines(&text)
        })
        .filter(|rec| rec.tag == tag)
        .collect();
    out.sort_by_key(|rec| rec.started_at_ms);
    out
}

/// 在跑那一轮的凭证租约:(turn id, 开始时刻)。租约是尽力清理的,可能是早已结束的轮留下的,
/// 调用方再用轮记录排除一遍。
fn read_current_turn(state: &Path, tag: &str) -> Option<(String, i64)> {
    let path = state
        .join("runtime")
        .join("current-turns")
        .join(format!("{tag}.json"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some((
        v.get("turn_id")?.as_str()?.to_string(),
        v.get("started_at_ms")?.as_i64()?,
    ))
}

/// 跑一个命令的 `--version` / `-V`,只取第一行;超时或失败给 `None`。
fn version_line(program: &str, flag: &str) -> Option<String> {
    let mut child = std::process::Command::new(program)
        .arg(flag)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                // 恰好在这之间退出的话 kill 会报错,可忽略;wait 只为回收。
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// `cli_cmd` 只在是单独一个程序名或路径、且只含 ASCII 字母数字与 `/._~+-` 时收。带参数、环境
/// 赋值或引号的命令行可能夹着密钥,而按空白切词分不清哪一段是程序,整条不收。
fn program_name(cli_cmd: &str) -> Option<&str> {
    let cmd = cli_cmd.trim();
    let plain = !cmd.is_empty()
        && !cmd.starts_with('-')
        && cmd
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._~+-".contains(c));
    plain.then_some(cmd)
}

fn env_json(agent: &Agent, home: &str, probe: bool) -> Value {
    let build = crate::version::build_info();
    let cli_cmd = agent.cli_cmd();
    let program = program_name(&cli_cmd).unwrap_or_default().to_string();
    let daemon = probe.then(|| match crate::setup::self_install::current_daemon() {
        crate::setup::self_install::Daemon::Stopped => json!({"running": false}),
        crate::setup::self_install::Daemon::Running(version) => {
            json!({"running": true, "version": version})
        }
    });
    let version = |program: &str, flag: &str| probe.then(|| version_line(program, flag)).flatten();
    let exe = std::env::current_exe()
        .map(|p| export::tilde(&p.to_string_lossy(), home))
        .unwrap_or_default();
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "summon": {"version": build.version, "commit": build.commit},
        "exe": exe,
        "cli": {
            "kind": agent.cli_kind(),
            "command": export::tilde(&program, home),
            "version": version(&program, "--version"),
        },
        "tmux": version("tmux", "-V"),
        "daemon": daemon,
    })
}

fn config_json(target: &Target, home: &str) -> Value {
    let agent = &target.agent;
    let effective = crate::runtime::chat_model::effective(agent, Some(&target.tag));
    let messaging = super::agent_env::resolve_messaging(agent);
    let overrides = target
        .session
        .as_deref()
        .map(|s| super::dashboard_port::read_chat_settings(agent, s))
        .unwrap_or_default();
    let resolved = summon_core::settings::ResolvedSettings::resolve(&messaging, &overrides);
    let settings: serde_json::Map<String, Value> = summon_core::settings::SETTINGS
        .iter()
        .map(|d| (d.key.to_string(), Value::Bool(resolved.get(d.id))))
        .collect();
    let chat_overrides: Vec<&str> = summon_core::settings::SETTINGS
        .iter()
        .filter(|d| overrides.get(d.id).is_some())
        .map(|d| d.key)
        .collect();
    let runtime = agent.runtime.as_ref();
    let mut env_keys: Vec<&String> = runtime
        .and_then(|r| r.env.as_ref())
        .map(|env| env.keys().collect())
        .unwrap_or_default();
    env_keys.sort();
    let feishu = agent.feishu.as_ref();
    json!({
        "cli": agent.cli_kind(),
        "capture": format!("{:?}", agent.capture_mode()),
        "model": {"value": effective.model, "layer": format!("{:?}", effective.model_layer)},
        "effort": {"value": effective.effort, "layer": format!("{:?}", effective.effort_layer)},
        "settings": settings,
        "chat_overrides": chat_overrides,
        "agent_yaml": {
            "model": agent.model,
            "effort": agent.effort,
            "reasoning_summary": agent.reasoning_summary,
            "runtime": {
                "cli": runtime.and_then(|r| r.cli.clone()),
                "cli_cmd": runtime
                    .and_then(|r| r.cli_cmd.as_deref())
                    .and_then(program_name)
                    .map(|program| export::tilde(program, home)),
                "working_dir": agent.working_dir().map(|d| export::tilde(&d, home)),
                "resume": runtime.and_then(|r| r.resume),
                "headless": runtime.and_then(|r| r.headless),
                "capture": runtime.and_then(|r| r.capture.clone()),
                "codex_sandbox": runtime.and_then(|r| r.codex_sandbox.clone()),
                "codex_approval_policy": runtime.and_then(|r| r.codex_approval_policy.clone()),
                "model_context_window": runtime.and_then(|r| r.model_context_window),
                "env_keys": env_keys,
            },
            "allowed_users": feishu.map_or(0, |f| f.allowed_users.len()),
            "operators": agent.effective_operators().map_or(0, |list| list.len()),
        },
    })
}

fn session_json(
    target: &Target,
    thread_dir: &Path,
    current: Option<&(String, i64)>,
    home: &str,
    probe: bool,
) -> Value {
    let agent = &target.agent;
    let record = |rec: Option<summon_core::session_id::SessionRecord>| {
        rec.map(|r| {
            json!({
                "cli": r.cli,
                "id": r.id,
                "cwd": r.cwd.map(|c| export::tilde(&c, home)),
                "created_at": r.created_at,
                "strict_resume": r.strict_resume,
                "primer_slot": r.primer_slot,
            })
        })
    };
    let (resume, previous, tmux_live) = match target.session.as_deref() {
        Some(session) => (
            record(crate::session::resume_store::read_record(agent, session)),
            record(crate::session::resume_store::previous_checked(agent, session).ok()),
            probe && crate::session::tmux::Tmux::new(session.to_string()).has_session(),
        ),
        None => (None, None, false),
    };
    let anchor = std::fs::read_to_string(thread_dir.join("turn_anchor.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|a| {
            let mut out = serde_json::Map::new();
            for key in [
                "native_session_id",
                "inbox_message_ids",
                "inject_offset",
                "consumed_boundary",
                "injected_at_ms",
            ] {
                if let Some(v) = a.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            Value::Object(out)
        });
    let status = probe
        .then(crate::observe::fleet_snapshot::fetch_runtime_status)
        .flatten();
    let daemon = status.map(|status| {
        let is_me = |agent_name: &str, tag: &str| agent_name == agent.name && tag == target.tag;
        json!({
            "busy": status.busy.iter().any(|b| is_me(&b.agent, &b.tag)),
            "subagents": status.subagents.iter().any(|b| is_me(&b.agent, &b.tag)),
            "live_headless": status
                .live_headless
                .as_ref()
                .map(|live| live.iter().any(|l| is_me(&l.agent, &l.tag))),
        })
    });
    json!({
        "session": target.session,
        "tmux_live": tmux_live,
        "resume": resume,
        "previous": previous,
        "anchor": anchor,
        "current_turn": current.map(|(turn, started)| json!({"turn": turn, "started_at_ms": started})),
        "daemon": daemon,
    })
}

/// events.jsonl 及其轮转出去的 `.1`,按落盘顺序(旧的在前)。
fn event_files(thread_dir: &Path) -> Vec<PathBuf> {
    [
        thread_dir.join("events.jsonl.1"),
        thread_dir.join("events.jsonl"),
    ]
    .into_iter()
    .filter(|p| p.is_file())
    .collect()
}

/// 逐行读一个文本文件,跳过不在窗内的(行时刻由 `time_of` 给,给不出时刻的行跟着上一行)。
fn window_lines(
    path: &Path,
    window: &Window,
    mut time_of: impl FnMut(&str) -> Option<i64>,
    mut each: impl FnMut(&str, Option<i64>),
) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut inside = false;
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)).map_while(Result::ok) {
        let at = time_of(&line);
        if let Some(at) = at {
            inside = window.contains(at);
        }
        if inside {
            each(&line, at);
        }
    }
}

fn events_entry(ctx: &Ctx, thread_dir: &Path) -> Result<Entry> {
    let mut entry = Entry::new("events.ndjson", &thread_dir.join("events.jsonl"), &ctx.home);
    let mut projector = EventProjector::default();
    let ts_of = |line: &str| {
        serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| v.get("ts")?.as_i64())
    };
    for path in event_files(thread_dir) {
        window_lines(&path, &ctx.window, ts_of, |line, at| {
            entry.note.in_window += 1;
            match projector.project_line(line) {
                Some(ev) => match serde_json::to_string(&ev) {
                    Ok(json) => entry.push(json, at),
                    Err(_) => entry.note.skipped += 1,
                },
                None => entry.note.skipped += 1,
            }
        });
    }
    Ok(entry)
}

/// 读一份日志(含 `.1`),窗内的条目按本会话筛过。`raw` 时收原文(含续行、密钥形态的串打码),
/// 否则只收白名单模板的重写结果。
fn log_entry(ctx: &Ctx, file: LogFile, people: &mut People, raw: bool) -> Entry {
    let run = &ctx.roots.run;
    let name = file.file_name();
    let shown = if raw {
        format!("raw/{name}")
    } else {
        name.to_string()
    };
    let mut entry = Entry::new(&shown, &run.join(name), &ctx.home);
    for path in [run.join(format!("{name}.1")), run.join(name)] {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let (_, entries) = log_view::split(file, &bytes);
        for item in entries {
            let Some(at) = item.at_ms.filter(|at| ctx.window.contains(*at)) else {
                continue;
            };
            let first = item.text.lines().next().unwrap_or_default();
            let message = match file {
                LogFile::Daemon => logs::daemon_message(first).unwrap_or(first),
                _ => logs::sidecar_message(first),
            };
            if file == LogFile::Daemon {
                let foreign = logs::agent_prefix(message)
                    .is_some_and(|name| ctx.other_agents.iter().any(|a| a == name));
                if foreign || !logs::mentions_any(&item.text, &ctx.keys) {
                    continue;
                }
            }
            entry.note.in_window += 1;
            if raw {
                entry.push(log_view::redact(&item.text), Some(at));
                continue;
            }
            let templates = match file {
                LogFile::Daemon => logs::DAEMON_TEMPLATES,
                _ => logs::SIDECAR_TEMPLATES,
            };
            match logs::diag_line(templates, message, people, &ctx.home) {
                Some(line) => {
                    let stamp = chrono::DateTime::from_timestamp_millis(at)
                        .map(|t| {
                            t.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M:%S")
                                .to_string()
                        })
                        .unwrap_or_default();
                    entry.push(format!("[{stamp}] {line}"), Some(at));
                }
                None => entry.note.skipped += 1,
            }
        }
    }
    entry
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeKind {
    Codex,
    Claude,
}

#[derive(Debug)]
enum Native {
    /// 按行的 JSON 记录(codex rollout;claude 转录及其子 agent 转录)。
    Lines {
        kind: NativeKind,
        files: Vec<PathBuf>,
    },
    /// cursor 的会话库。
    CursorStore(PathBuf),
}

/// 本会话当前原生会话的记录文件。
fn native_source(target: &Target, home: &Path) -> Option<Native> {
    let session = target.session.as_deref()?;
    let rec = crate::session::resume_store::read_record(&target.agent, session)?;
    match rec.cli.as_str() {
        "claude" => {
            let cwd = rec.cwd.clone().or_else(|| target.agent.working_dir())?;
            let main = summon_core::claude_log::claude_transcript_path(&cwd, &rec.id)?;
            let mut files = vec![main.clone()];
            let subagents = main.with_extension("").join("subagents");
            if let Ok(entries) = std::fs::read_dir(subagents) {
                let mut subs: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
                    .collect();
                subs.sort();
                files.extend(subs);
            }
            files.retain(|p| p.is_file());
            (!files.is_empty()).then_some(Native::Lines {
                kind: NativeKind::Claude,
                files,
            })
        }
        "codex" | "traex" => {
            let codex_home = target
                .agent
                .runtime
                .as_ref()
                .and_then(|r| r.env.as_ref())
                .and_then(|env| env.get("CODEX_HOME"))
                .map(|p| PathBuf::from(summon_core::registry::expand_tilde(p)))
                .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
                .unwrap_or_else(|| home.join(".codex"));
            let suffix = format!("-{}.jsonl", rec.id);
            ["sessions", "archived_sessions"]
                .iter()
                .find_map(|dir| {
                    find_file(&codex_home.join(dir), 4, &|name| name.ends_with(&suffix))
                })
                .map(|path| Native::Lines {
                    kind: NativeKind::Codex,
                    files: vec![path],
                })
        }
        "cursor" => {
            let chats = home.join(".cursor").join("chats");
            std::fs::read_dir(chats)
                .ok()?
                .flatten()
                .find_map(|workspace| {
                    let db = workspace.path().join(&rec.id).join("store.db");
                    db.is_file().then_some(Native::CursorStore(db))
                })
        }
        _ => None,
    }
}

/// 在目录树里(至多 `depth` 层)找第一个文件名满足条件的文件。
fn find_file(dir: &Path, depth: usize, wanted: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_file() && entry.file_name().to_str().is_some_and(wanted) {
            return Some(path);
        }
        if kind.is_dir() && depth > 0 {
            dirs.push(path);
        }
    }
    // 按日期分目录:新的在后,倒着找先碰到最近的。
    dirs.sort();
    dirs.iter()
        .rev()
        .find_map(|d| find_file(d, depth - 1, wanted))
}

fn native_entry(ctx: &Ctx, kind: NativeKind, paths: &[PathBuf]) -> Entry {
    let mut entry = Entry::new(
        "native.ndjson",
        paths.first().map_or(Path::new(""), |p| p.as_path()),
        &ctx.home,
    );
    let mut projector = NativeProjector::default();
    for path in paths {
        let file_tag = export::tilde(&path.to_string_lossy(), &ctx.home);
        let mut first = true;
        each_native_line(path, &ctx.window, |value, at| {
            if first {
                first = false;
                entry.push(json!({"file": file_tag}).to_string(), at);
            }
            entry.note.in_window += 1;
            let projected = match kind {
                NativeKind::Codex => projector.codex(value),
                NativeKind::Claude => projector.claude(value),
            };
            entry.push(projected.to_string(), at);
        });
    }
    entry
}

/// 原生记录逐行:窗内的行,加上开头的会话元数据行(codex 的 session_meta 记着 CLI 版本)。
fn each_native_line(path: &Path, window: &Window, mut each: impl FnMut(&Value, Option<i64>)) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut inside = false;
    for (index, line) in std::io::BufRead::lines(std::io::BufReader::new(file))
        .map_while(Result::ok)
        .enumerate()
    {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let at = line_time_ms(&value);
        if let Some(at) = at {
            inside = window.contains(at);
        }
        let head = index == 0 && value.get("type").and_then(Value::as_str) == Some("session_meta");
        if inside || head {
            each(&value, at);
        }
    }
}

/// 完整包另附的原件。
fn full_entries(
    ctx: &Ctx,
    thread_dir: &Path,
    in_window: &[&TurnRecord],
    native: Option<&Native>,
) -> Result<Vec<Entry>> {
    let mut out = Vec::new();

    let turns_source = ctx.roots.run.join("turns").join(&ctx.target.agent.name);
    let mut turns = Entry::new("raw/turns.ndjson", &turns_source, &ctx.home);
    for rec in in_window {
        turns.note.in_window += 1;
        turns.push(serde_json::to_string(rec)?, Some(rec.started_at_ms));
    }
    out.push(turns);

    let mut events = Entry::new(
        "raw/events.jsonl",
        &thread_dir.join("events.jsonl"),
        &ctx.home,
    );
    let ts_of = |line: &str| {
        serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| v.get("ts")?.as_i64())
    };
    for path in event_files(thread_dir) {
        window_lines(&path, &ctx.window, ts_of, |line, at| {
            events.note.in_window += 1;
            events.push(log_view::redact(line), at);
        });
    }
    out.push(events);

    let mut outbound = Entry::new(
        "raw/outbound.ndjson",
        &thread_dir.join("outbound.ndjson"),
        &ctx.home,
    );
    let rfc_of = |line: &str| {
        serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| v.get("ts")?.as_str().map(str::to_string))
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(&ts).ok())
            .map(|t| t.timestamp_millis())
    };
    window_lines(
        &thread_dir.join("outbound.ndjson"),
        &ctx.window,
        rfc_of,
        |line, at| {
            outbound.note.in_window += 1;
            outbound.push(log_view::redact(line), at);
        },
    );
    out.push(outbound);

    let transcript = thread_dir.join("transcript.md");
    if let Ok(text) = std::fs::read_to_string(&transcript) {
        let mut entry = Entry::new("raw/transcript.md", &transcript, &ctx.home);
        for line in text.lines() {
            entry.note.in_window += 1;
            entry.push(log_view::redact(line), None);
        }
        out.push(entry);
    }

    let mut people = People::default();
    out.push(log_entry(ctx, LogFile::Daemon, &mut people, true));
    out.push(log_entry(ctx, LogFile::Sidecar, &mut people, true));

    match native {
        Some(Native::Lines { files, .. }) => {
            for path in files {
                let name = format!(
                    "raw/native/{}",
                    path.file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default()
                );
                let mut entry = Entry::new(&name, path, &ctx.home);
                each_native_line(path, &ctx.window, |value, at| {
                    entry.note.in_window += 1;
                    entry.push(log_view::redact(&value.to_string()), at);
                });
                out.push(entry);
            }
        }
        Some(Native::CursorStore(_)) | None => {}
    }
    Ok(out)
}

/// 裁到体积上限以内:每次从当前最大的文件最早的一端裁掉一成,在跑那一轮的条目不裁。
/// 裁不到上限以内返回 false。
fn trim_to(files: &mut [Entry], ctx: &Ctx, max_bytes: u64) -> bool {
    loop {
        let total: u64 = files.iter().map(Entry::bytes).sum();
        if total <= max_bytes {
            return true;
        }
        let candidate = files
            .iter_mut()
            .filter(|f| f.lines.len() > 1 && f.times.first().is_some_and(|t| !ctx.pinned(*t)))
            .max_by_key(|f| f.bytes());
        let Some(file) = candidate else {
            return false;
        };
        let trimmable = file.times.iter().take_while(|t| !ctx.pinned(**t)).count();
        let cut = (trimmable / 10).max(1).min(trimmable);
        file.lines.drain(..cut);
        file.times.drain(..cut);
        file.note.trimmed += cut;
        file.note.included -= cut;
    }
}

/// 写 tar.gz:先写临时文件再改名,权限 0600。
fn write_bundle(
    dir: &Path,
    target: &Target,
    mode: Mode,
    manifest: &Manifest,
    files: &[Entry],
) -> Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    std::fs::create_dir_all(dir)
        .with_context(|| summon_i18n::format!("rc18f37e5e1", dir.display()))?;
    // 目录权限收不紧不影响包本身(包文件自己是 0600)。
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let stem = format!(
        "summon-export-{}-{}-{}{}",
        target.agent.name,
        target.tag,
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        mode.file_suffix()
    );
    let path = dir.join(format!("{stem}.tar.gz"));
    let tmp = dir.join(format!(".{stem}.{}.tmp", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| summon_i18n::format!("r765b94b354", tmp.display()))?;
    let result = (|| -> Result<()> {
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mtime = chrono::Local::now().timestamp().max(0) as u64;
        let mut add = |name: &str, body: &[u8]| -> Result<()> {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o600);
            header.set_mtime(mtime);
            header.set_cksum();
            tar.append_data(&mut header, format!("{stem}/{name}"), body)?;
            Ok(())
        };
        add(
            "manifest.json",
            serde_json::to_string_pretty(manifest)?.as_bytes(),
        )?;
        for file in files {
            add(&file.note.name, &file.body())?;
        }
        let mut gz = tar.into_inner()?;
        gz.flush()?;
        gz.finish()?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| summon_i18n::format!("rec481e5734", path.display()))?;
    Ok(path)
}

/// 导出目录只留最近的 [`KEEP_BUNDLES`] 个、且不超过 [`KEEP_FOR`] 的包;写到一半被打断留下的
/// 临时文件超过 [`STALE_TMP`] 也清掉。清理失败不影响这次导出。
fn prune(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut bundles: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Some(modified) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("summon-export-") && name.ends_with(".tar.gz") {
            bundles.push((modified, entry.path()));
        } else if name.starts_with(".summon-export-")
            && name.ends_with(".tmp")
            && now.duration_since(modified).unwrap_or_default() > STALE_TMP
        {
            // 删不掉留到下次再清。
            let _ = std::fs::remove_file(entry.path());
        }
    }
    bundles.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (index, (modified, path)) in bundles.iter().enumerate() {
        let stale = now.duration_since(*modified).unwrap_or_default() > KEEP_FOR;
        if index >= KEEP_BUNDLES || stale {
            // 删不掉留到下次再清,不影响这次导出。
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 给人看的一段摘要:包在哪、每类材料收了多少。
pub(crate) fn summary(bundle: &Bundle, home: &str) -> String {
    let m = &bundle.manifest;
    let fmt_time = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default()
    };
    let mut out = summon_i18n::format!(
        "r1ef326a443",
        match m.mode {
            Mode::Diagnostic => summon_i18n::text!("re82d5f094a"),
            Mode::Full => summon_i18n::text!("rfa65d49c78"),
        },
        export::tilde(&bundle.path.to_string_lossy(), home),
        m.agent,
        m.tag,
        fmt_time(m.window.start_ms),
        fmt_time(m.window.end_ms),
    );
    for file in &m.files {
        if file.in_window == 0 && file.included == 0 {
            continue;
        }
        out.push_str(&summon_i18n::format!(
            "r3422fcace3",
            file.name,
            file.included
        ));
        if file.skipped > 0 {
            out.push_str(&summon_i18n::format!("rb0a0bb29f0", file.skipped));
        }
        if file.trimmed > 0 {
            out.push_str(&summon_i18n::format!("r9144d7ba8c", file.trimmed));
        }
        out.push('\n');
    }
    for note in &m.notes {
        out.push_str(&format!("- {note}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(dir: &Path, name: &str) -> Agent {
        let mut agent: Agent = serde_yaml::from_str(&format!("name: {name}\n")).unwrap();
        agent.dir = dir.join("agents").join(name);
        agent
    }

    #[test]
    fn find_target_rejects_keys_that_could_leave_the_threads_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let agents = vec![agent(tmp.path(), "alpha")];
        std::fs::create_dir_all(agents[0].state_dir().join("threads").join("g1")).unwrap();
        for spec in [
            "alpha/../alpha",
            "alpha/g1/..",
            "..",
            "alpha/",
            "alpha/.",
            ".",
        ] {
            assert!(find_target(&agents, spec).is_err(), "{spec}");
        }
        assert_eq!(find_target(&agents, "alpha/g1").unwrap().tag, "g1");
    }

    #[test]
    fn program_name_is_kept_only_for_a_lone_program() {
        for cmd in [
            "codex",
            "/opt/bin/codex",
            "~/.local/bin/claude",
            " cursor-agent ",
        ] {
            assert_eq!(program_name(cmd), Some(cmd.trim()), "{cmd}");
        }
        for cmd in [
            "codex --api-key sk-secret",
            "OPENAI_API_KEY=sk-secret codex",
            "env OPENAI_API_KEY=sk-secret codex",
            "/opt/a=b/codex --api-key sk-secret",
            "OPENAI_API_KEY=x\u{3000}sk-secret codex",
            "\"codex\" --api-key sk-secret",
            "\"/opt/my tools/codex\" --api-key sk-secret",
            "'codex' --api-key sk-secret",
            "--api-key",
            "",
        ] {
            assert_eq!(program_name(cmd), None, "{cmd}");
        }
    }

    /// cursor 的会话库没法按密钥打码:两档都不带,只在 manifest 里说明它在哪。
    #[test]
    fn neither_bundle_carries_the_cursor_session_store() {
        let (agent, tmp) = crate::test_support::agent_dir("cursor", "");
        let tag = "g0123456789abcdee";
        let session = format!("{}-{tag}", agent.tmux_name());
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            chats.join(format!("{session}.meta")),
            format!("chat_id=oc_test2\nchat_type=group\ntag={tag}\n"),
        )
        .unwrap();
        std::fs::write(
            chats.join(format!("{session}.session")),
            summon_core::session_id::format_record("cursor", "sid-cursor", Some("/tmp/work"), None),
        )
        .unwrap();
        let home = tmp.path().join("home");
        let db = home.join(".cursor/chats/ws1/sid-cursor/store.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, CANARY).unwrap();
        let roots = Roots {
            run: tmp.path().join("run"),
            home,
            probe: false,
        };
        for mode in [Mode::Diagnostic, Mode::Full] {
            let req = Request {
                mode,
                window: WindowSpec::Turns(3),
                max_bytes: DEFAULT_MAX_BYTES,
            };
            let bundle = export_in(
                &roots,
                &target_for(&agent, tag),
                std::slice::from_ref(&agent.name),
                &req,
            )
            .unwrap();
            let files = unpack(&bundle.path);
            assert!(files
                .iter()
                .all(|(name, body)| !name.ends_with("store.db") && !body.contains(CANARY)));
            assert!(
                bundle
                    .manifest
                    .notes
                    .iter()
                    .any(|n| n.contains("cursor") && n.contains("store.db")),
                "{mode:?} {:?}",
                bundle.manifest.notes
            );
        }
    }

    #[test]
    fn prune_keeps_recent_bundles_and_clears_abandoned_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = SystemTime::now();
        let touch = |name: &str, age: Duration| {
            let path = dir.join(name);
            std::fs::write(&path, b"x").unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(now - age).unwrap();
        };
        touch("summon-export-a-g1-1.tar.gz", Duration::from_secs(60));
        touch(
            "summon-export-a-g1-2.tar.gz",
            KEEP_FOR + Duration::from_secs(60),
        );
        touch(
            ".summon-export-a-g1-3.1.tmp",
            STALE_TMP + Duration::from_secs(60),
        );
        touch(".summon-export-a-g1-4.2.tmp", Duration::from_secs(60));
        prune(dir, now);
        let mut left: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![".summon-export-a-g1-4.2.tmp", "summon-export-a-g1-1.tar.gz"]
        );
    }

    #[test]
    fn find_target_accepts_agent_slash_tag_and_bare_chat_id() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent(tmp.path(), "alpha");
        let chats = a.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            chats.join("soul-alpha-g1111111111111111.meta"),
            "chat_id=oc_x\nchat_type=group\ntag=g1111111111111111\n",
        )
        .unwrap();
        let agents = vec![a.clone(), agent(tmp.path(), "beta")];
        let by_tag = find_target(&agents, "alpha/g1111111111111111").unwrap();
        assert_eq!(
            by_tag.session.as_deref(),
            Some("soul-alpha-g1111111111111111")
        );
        let by_chat = find_target(&agents, "oc_x").unwrap();
        assert_eq!(by_chat.tag, "g1111111111111111");
        assert!(find_target(&agents, "beta/g1111111111111111").is_err());
        assert!(find_target(&agents, "gamma/x")
            .unwrap_err()
            .to_string()
            .contains("gamma"));
    }

    #[test]
    fn trimming_drops_the_oldest_lines_of_the_biggest_file_but_never_the_running_turn() {
        let ctx_target = Target {
            agent: serde_yaml::from_str("name: a\n").unwrap(),
            tag: "g1".into(),
            session: None,
            meta: ChatMeta::default(),
        };
        let roots = Roots {
            run: PathBuf::new(),
            home: PathBuf::new(),
            probe: false,
        };
        let ctx = Ctx {
            roots: &roots,
            target: &ctx_target,
            window: Window {
                start_ms: 0,
                end_ms: 1000,
            },
            home: String::new(),
            keys: Vec::new(),
            other_agents: Vec::new(),
            running_since: Some(900),
        };
        let mut big = Entry::new("big", Path::new(""), "");
        for at in 0..100 {
            big.push("x".repeat(99), Some(at * 10));
        }
        let mut small = Entry::new("small", Path::new(""), "");
        small.push("y".repeat(99), Some(5));
        let mut files = vec![big, small];
        assert!(trim_to(&mut files, &ctx, 5_000));
        assert!(files[0].note.trimmed > 0);
        assert_eq!(files[1].note.trimmed, 0);
        assert!(files[0].times.iter().all(|t| t.unwrap() >= 500));

        // 只剩在跑那一轮的条目时不再裁。
        assert!(!trim_to(&mut files, &ctx, 100));
        assert!(files[0].times.iter().any(|t| t.unwrap() >= 900));
    }

    const CANARY: &str = "CANARY-7f3e";

    fn lines(path: &Path, rows: &[Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(path, body).unwrap();
    }

    /// 一个带内容的会话现场:每类带内容的字段都放同一个金丝雀串。返回 tag。
    fn seed_session(agent: &Agent, run: &Path, codex_home: &Path, now_ms: i64) -> String {
        let tag = "g0123456789abcdef".to_string();
        let session = format!("{}-{tag}", agent.tmux_name());
        let state = agent.state_dir();
        let chats = state.join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            chats.join(format!("{session}.meta")),
            format!("chat_id=oc_test1\nchat_type=group\nchat_name={CANARY}\ntag={tag}\nlast_sender={CANARY}\n"),
        )
        .unwrap();
        std::fs::write(
            chats.join(format!("{session}.session")),
            summon_core::session_id::format_record("codex", "th-canary", Some("/tmp/work"), None),
        )
        .unwrap();
        let at = now_ms - 60_000;
        let rfc = chrono::DateTime::from_timestamp_millis(at)
            .unwrap()
            .to_rfc3339();
        let thread = state.join("threads").join(&tag);
        let turn = |seq: u64, kind: &str, extra: Value| {
            let mut v = json!({"ts": at, "seq": seq, "kind": kind, "turn": "turn-1"});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            v
        };
        lines(
            &thread.join("events.jsonl"),
            &[
                turn(1, "user_input", json!({"text": CANARY})),
                turn(2, "thinking", json!({"text": CANARY})),
                turn(3, "thinking", json!({"text": CANARY})),
                turn(
                    4,
                    "tool_use",
                    json!({"tool": "Bash", "tool_input": {"command": CANARY}}),
                ),
                turn(5, "tool_result", json!({"tool_result": CANARY})),
                turn(6, "assistant", json!({"text": CANARY})),
                turn(7, "error", json!({"text": CANARY})),
            ],
        );
        lines(
            &thread.join("outbound.ndjson"),
            &[
                json!({"chat_id": "oc_test1", "kind": "markdown_card", "payload": {"markdown": CANARY}, "ts": rfc}),
            ],
        );
        std::fs::write(thread.join("transcript.md"), format!("# {CANARY}\n")).unwrap();
        std::fs::write(
            thread.join("turn_anchor.json"),
            json!({"native_session_id": "th-canary", "prompt_head": CANARY, "injected_at_ms": at})
                .to_string(),
        )
        .unwrap();
        let lease = state
            .join("runtime")
            .join("current-turns")
            .join(format!("{tag}.json"));
        lines(
            &lease,
            &[json!({"turn_id": "turn-2", "started_at_ms": now_ms - 10_000})],
        );
        lines(
            &run.join("turns")
                .join(&agent.name)
                .join(turn_ledger::file_name(at)),
            &[json!({
                "agent": agent.name, "chat_id": "oc_test1", "tag": tag, "turn": "turn-1",
                "origin": "human", "requester": "ou_0123456789abcdef", "message_id": "om_test1",
                "prompt_head": CANARY, "feed": CANARY, "started_at_ms": at,
                "ended_at_ms": at + 1000, "end": "done",
            })],
        );
        let stamp = chrono::DateTime::from_timestamp_millis(now_ms - 30_000)
            .unwrap()
            .with_timezone(&chrono::Local);
        let daemon = stamp.format("[%Y-%m-%d %H:%M:%S] [summon]").to_string();
        let name = &agent.name;
        std::fs::write(
            run.join("daemon.log"),
            [
                format!("{daemon} [{name}] 新会话 chat=oc_test1 → {session}"),
                format!("{daemon} [{name}] 工具: Bash(echo {CANARY}) chat=oc_test1"),
                format!("{CANARY} continuation"),
                format!("{daemon} [{name}] 接管失败(注入于 x),转人工恢复卡 chat={tag}: {CANARY}"),
                format!("{daemon} [{name}] 处理任务 chat=oc_test1 msg=om_test1 from={CANARY}(ou_0123456789abcdef)"),
                format!("{daemon} [other] 新会话 chat=oc_test1 → soul-other-{tag}"),
                format!("{daemon} [{name}] 新会话 chat=oc_elsewhere → soul-elsewhere"),
                String::new(),
            ]
            .join("\n"),
        )
        .unwrap();
        let go = stamp.format("%Y/%m/%d %H:%M:%S").to_string();
        std::fs::write(
            run.join("sidecar.log"),
            format!(
                "{go} [Info] [event-dispatch is ready]\n{go} [Error] {go} handle message failed, message_type: event, {CANARY}\n"
            ),
        )
        .unwrap();
        lines(
            &codex_home.join("sessions/2026/10/29/rollout-2026-10-29T00-00-00-th-canary.jsonl"),
            &[
                json!({"timestamp": rfc, "type": "session_meta", "payload": {"cli_version": "0.141.0",
                    "base_instructions": CANARY, "cwd": CANARY}}),
                json!({"timestamp": rfc, "type": "response_item", "payload": {"type": "message",
                    "role": "user", "content": [{"type": "input_text", "text": CANARY}]}}),
                json!({"timestamp": rfc, "type": "event_msg", "payload": {"type": "agent_reasoning", "text": CANARY}}),
            ],
        );
        tag
    }

    /// 把包里每个文件读出来:(包内路径, 内容)。
    fn unpack(path: &Path) -> Vec<(String, String)> {
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap());
        let mut archive = tar::Archive::new(gz);
        archive
            .entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let name = e.path().unwrap().to_string_lossy().into_owned();
                let mut body = String::new();
                std::io::Read::read_to_string(&mut e, &mut body).unwrap();
                (name, body)
            })
            .collect()
    }

    fn file<'a>(files: &'a [(String, String)], suffix: &str) -> &'a str {
        &files
            .iter()
            .find(|(name, _)| name.ends_with(suffix))
            .unwrap_or_else(|| panic!("包里没有 {suffix}"))
            .1
    }

    /// 验收:每类带内容的字段放同一个标记串,诊断包里一次都不出现;完整包里出现(阳性对照,
    /// 证明这些字段确实被导出流程读到了)。
    #[test]
    fn the_diagnostic_bundle_carries_no_conversation_content() {
        let (mut agent, tmp) = crate::test_support::agent_dir("codex", "");
        let run = tmp.path().join("run");
        let codex_home = tmp.path().join("codex");
        agent.runtime.as_mut().unwrap().cli_cmd = Some(format!(
            "OPENAI_API_KEY={CANARY} \"/opt/a=b/codex\" --api-key {CANARY}"
        ));
        agent.runtime.as_mut().unwrap().env = Some(
            [(
                "CODEX_HOME".to_string(),
                codex_home.to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
        );
        let now_ms = chrono::Local::now().timestamp_millis();
        let tag = seed_session(&agent, &run, &codex_home, now_ms);
        let roots = Roots {
            run: run.clone(),
            home: tmp.path().join("home"),
            probe: false,
        };
        let target = target_for(&agent, &tag);
        let all = vec![agent.name.clone(), "other".to_string()];
        let req = |mode| Request {
            mode,
            window: WindowSpec::Turns(3),
            max_bytes: DEFAULT_MAX_BYTES,
        };

        let diag = export_in(&roots, &target, &all, &req(Mode::Diagnostic)).unwrap();
        let files = unpack(&diag.path);
        for (name, body) in &files {
            assert!(!body.contains(CANARY), "{name} 带出了对话内容:\n{body}");
        }
        let daemon = file(&files, "/daemon.log");
        assert!(daemon.contains("新会话 chat=oc_test1"), "{daemon}");
        assert!(
            daemon.contains(::summon_core::i18n::label(logs::HIDDEN)),
            "{daemon}"
        );
        assert!(daemon.contains("from=user-1(user-1)"), "{daemon}");
        assert!(
            !daemon.contains("soul-other"),
            "别的 agent 的行不该进来:{daemon}"
        );
        assert!(
            !daemon.contains("oc_elsewhere"),
            "本 agent 别的会话的行不该进来:{daemon}"
        );
        assert!(file(&files, "/sidecar.log").contains("event-dispatch is ready"));
        assert!(file(&files, "/events.ndjson").contains("\"repeat_of\":2"));
        assert!(file(&files, "/native.ndjson").contains("\"cli_version\":\"0.141.0\""));
        assert!(file(&files, "/turns.ndjson").contains("\"requester\":\"user-1\""));
        assert_eq!(diag.manifest.running_turn.as_deref(), Some("turn-2"));
        assert!(diag.path.starts_with(run.join("exports")));
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&diag.path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let full = export_in(&roots, &target, &all, &req(Mode::Full)).unwrap();
        let files = unpack(&full.path);
        for suffix in [
            "/raw/events.jsonl",
            "/raw/turns.ndjson",
            "/raw/outbound.ndjson",
            "/raw/transcript.md",
            "/raw/daemon.log",
            "/raw/native/rollout-2026-10-29T00-00-00-th-canary.jsonl",
        ] {
            assert!(
                file(&files, suffix).contains(CANARY),
                "完整包的 {suffix} 应该带原件"
            );
        }
        assert!(file(&files, "/raw/daemon.log").contains(&format!("{CANARY} continuation")));
        for (name, body) in files.iter().filter(|(name, _)| !name.contains("/raw/")) {
            assert!(
                !body.contains(CANARY),
                "完整包里的诊断部分 {name} 也不该带内容"
            );
        }
    }

    /// 白名单里每条 daemon 模板都必须还是源码里某个格式串的原文:格式串改了或删了,白名单会悄悄
    /// 对不上任何行,这条测试让它当场失败。sidecar 自己的两条对照 Go 源码。
    #[test]
    fn every_log_template_is_still_a_format_string_in_the_source() {
        fn collect(dir: &Path, out: &mut String) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push_str(&std::fs::read_to_string(&path).unwrap());
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut rust = String::new();
        collect(&root.join("src"), &mut rust);
        for template in logs::DAEMON_TEMPLATES {
            assert!(
                rust.contains(&format!("\"{}\"", template.text)),
                "源码里找不到这条格式串,白名单要跟着改:{}",
                template.text
            );
        }
        let go = std::fs::read_to_string(
            root.join("../../sidecar/feishu-connector/localization_resources.go"),
        )
        .unwrap();
        for template in logs::SIDECAR_TEMPLATES
            .iter()
            .filter(|t| t.text.starts_with('r'))
        {
            assert!(
                go.contains(&format!("\"{}\"", template.text)),
                "missing sidecar key {}",
                template.text
            );
            for localized in template.localized() {
                let go_form = format!("\"%s {}\\n\"", localized.text.replace("{}", "%s"));
                assert!(go.contains(&go_form), "missing sidecar template {go_form}");
            }
        }
    }
}
