//! Claude 会话专属 settings 里挂的两个 hook 的进程侧。
//!
//! PermissionRequest(`summon daemon permission-hook`)与 daemon 侧的「屏上对话框归谁」登记。
//! 设计见 `docs/spec/2026-10-16-claude-permission-hook-design.md`。**hook 永不替人作答**:任何
//! 一步不成立(认不出请求、daemon 不在、被拒、请求已在别处结束)都是什么也不输出地退出,Claude
//! 走自己的终端对话框,抓屏中介照旧兜底。
//!
//! PostCompact(`summon daemon compact-hook`):压缩完成后报给 daemon,由它推压缩后的上下文长度卡。
//! 只在认得这个事件的 claude 版本上写进 settings,见 [`claude_knows_post_compact`]。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use summon_core::claude_hook::{
    self, CallState, CallWatch, HookDecision, HookInput, HookKind, ASK_SKIPPED_MESSAGE,
    PERMISSION_DENIED_MESSAGE,
};
use summon_core::core_port::{Actor, Command, CommandOutcome, Intent};

/// 第一次连不上 daemon 时最多等多久。只为接住 deploy 那几秒的停机窗口;daemon 真的不在时,
/// 子 agent 是先等 hook 再弹框的,等太久就是让它白白多卡一阵。
const FIRST_CONNECT_WINDOW: Duration = Duration::from_secs(15);
/// 已连上的 daemon 中途消失(重启)后,最多等新 daemon 多久再重新登记。
const RECONNECT_WINDOW: Duration = Duration::from_secs(120);
const RETRY_EVERY: Duration = Duration::from_secs(2);
/// 旁路线程的节拍:看父进程、读转录新增的行。
const WATCH_EVERY: Duration = Duration::from_millis(500);
/// hook 触发时这次 tool_use 可能还没落盘;过了这个时长还认不出就不再找。
const CALL_SEEN_WITHIN: Duration = Duration::from_secs(20);
/// 主 agent 的请求在转录里认不出时最多挂这么久:它的终端对话框两分钟就被 Claude 自动拒绝。
const MAIN_DIALOG_WINDOW: Duration = Duration::from_secs(130);
/// 首次读转录只读末尾这么多字节:这次调用只会在尾部,长会话的转录动辄几十 MB。
const TRANSCRIPT_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// hook 进程入口。stdout 只写一行裁决;其余诊断写 `run/claude-hook.log`,不写 stderr ——
/// stderr 会进 Claude 的界面。
pub fn run_permission_hook(alias: &str, session: &str) {
    let mut raw = String::new();
    let _ = std::io::stdin().lock().read_to_string(&mut raw);
    let Some(input) = claude_hook::parse_hook_input(&raw) else {
        hook_log(alias, session, summon_i18n::text!("ra9599df63d"));
        return;
    };
    let kind = claude_hook::classify(&input.request.tool_name);
    if kind == HookKind::Unsupported {
        return;
    }
    let command = Command {
        actor: Actor::Owner,
        intent: Intent::ClaudeHookAsk {
            alias: alias.to_string(),
            session: session.to_string(),
            request: input.request.clone(),
        },
    };
    let watcher = Watcher::spawn(&input, alias, session);
    let outcome = ask_daemon(
        &command,
        || UnixStream::connect(crate::runtime::paths::control_sock()),
        &watcher,
        Windows {
            first: FIRST_CONNECT_WINDOW,
            reconnect: RECONNECT_WINDOW,
            retry_every: RETRY_EVERY,
        },
    );
    watcher.finish();
    if let Some(reason) = watcher.ended() {
        hook_log(
            alias,
            session,
            &summon_i18n::format!("r468172341f", input.request.tool_name),
        );
        return;
    }
    match outcome.as_ref().map(|o| decision_for(kind, &input, o)) {
        Some(Some(decision)) => {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{}", claude_hook::hook_stdout(&decision));
            let _ = stdout.flush();
        }
        Some(None) => hook_log(
            alias,
            session,
            &summon_i18n::format!(
                "r2d69737cce",
                input.request.tool_name,
                describe(outcome.as_ref())
            ),
        ),
        None => hook_log(
            alias,
            session,
            &summon_i18n::format!("r9d77e8cbcd", input.request.tool_name),
        ),
    }
}

fn describe(outcome: Option<&CommandOutcome>) -> String {
    match outcome {
        Some(CommandOutcome::Denied(why)) => why.clone(),
        Some(other) => format!("{other:?}"),
        None => summon_i18n::text!("r583c9d9a2c").into(),
    }
}

/// daemon 的应答 → 回给 Claude 的裁决。只有人点出来的结果才算数,其余一律 None(不作答)。
fn decision_for(
    kind: HookKind,
    input: &HookInput,
    outcome: &CommandOutcome,
) -> Option<HookDecision> {
    let CommandOutcome::Value(value) = outcome else {
        return None;
    };
    let answer = value.get("answer")?.as_str()?;
    match kind {
        HookKind::Permission => match answer {
            "allow" => Some(HookDecision::Allow {
                updated_input: None,
            }),
            "deny" => Some(HookDecision::Deny {
                message: ::summon_core::i18n::label(PERMISSION_DENIED_MESSAGE).into(),
            }),
            _ => None,
        },
        HookKind::Ask => {
            let answers: Vec<Option<String>> = serde_json::from_str(answer).ok()?;
            Some(
                match claude_hook::ask_updated_input(&input.request.tool_input, &answers) {
                    Some(updated) => HookDecision::Allow {
                        updated_input: Some(updated),
                    },
                    None => HookDecision::Deny {
                        message: ::summon_core::i18n::label(ASK_SKIPPED_MESSAGE).into(),
                    },
                },
            )
        }
        HookKind::Unsupported => None,
    }
}

#[derive(Clone, Copy)]
struct Windows {
    first: Duration,
    reconnect: Duration,
    retry_every: Duration,
}

/// 经 control.sock 登记并阻塞等应答。连接断开而请求仍在(旁路线程没判结束)= daemon 换代,
/// 在窗口内重连、重新登记;窗口过了或旁路线程判了结束就返回 None。
fn ask_daemon(
    command: &Command,
    connect: impl Fn() -> std::io::Result<UnixStream>,
    watcher: &Watcher,
    windows: Windows,
) -> Option<CommandOutcome> {
    let line = serde_json::to_string(command).ok()?;
    let mut down_since = Instant::now();
    let mut window = windows.first;
    loop {
        if watcher.ended().is_some() {
            return None;
        }
        if let Ok(mut stream) = connect() {
            let sent = stream
                .write_all(line.as_bytes())
                .and_then(|_| stream.write_all(b"\n"))
                .and_then(|_| stream.flush());
            if sent.is_ok() {
                if let Ok(clone) = stream.try_clone() {
                    watcher.attach(clone);
                }
                let mut response = String::new();
                let read = BufReader::new(stream).read_line(&mut response);
                watcher.detach();
                if matches!(read, Ok(n) if n > 0) && !response.trim().is_empty() {
                    return serde_json::from_str(response.trim()).ok();
                }
                // 连上过又断了:换成「等新 daemon」的窗口,从此刻起算。
                down_since = Instant::now();
                window = windows.reconnect;
            }
        }
        if watcher.ended().is_some() || down_since.elapsed() >= window {
            return None;
        }
        std::thread::sleep(windows.retry_every);
    }
}

/// 旁路线程判定的结束原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// 转录里出现了这次调用的结果。
    Elsewhere,
    /// claude 进程没了(hook 被过继给别的父进程)。
    ParentGone,
    /// 主 agent 的请求在转录里认不出,已过了它对话框的自动拒绝窗口。
    WindowPassed,
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Ended::Elsewhere => summon_i18n::text!("rcf05c908bd"),
            Ended::ParentGone => summon_i18n::text!("r07330393e7"),
            Ended::WindowPassed => summon_i18n::text!("r8d7b03014f"),
        })
    }
}

#[derive(Default)]
struct WatchShared {
    stream: Option<UnixStream>,
    ended: Option<Ended>,
    finished: bool,
}

/// 旁路线程:看父进程还在不在、读转录新增的行。判了结束就关掉正在等应答的连接 ——
/// daemon 见对端断开即取消闸门、把卡片定格成「已在别处结束」。
struct Watcher {
    shared: Arc<Mutex<WatchShared>>,
}

/// 旁路线程要盯的东西。
struct WatchJob {
    parent: u32,
    transcript: Option<PathBuf>,
    call: CallWatch,
    main_agent: bool,
    alias: String,
    session: String,
    tool: String,
}

impl Watcher {
    fn spawn(input: &HookInput, alias: &str, session: &str) -> Self {
        let shared = Arc::new(Mutex::new(WatchShared::default()));
        let thread_shared = Arc::clone(&shared);
        let job = WatchJob {
            parent: std::os::unix::process::parent_id(),
            transcript: claude_hook::call_transcript_path(input),
            call: CallWatch::new(&input.request),
            main_agent: input.request.agent_id.is_none(),
            alias: alias.to_string(),
            session: session.to_string(),
            tool: input.request.tool_name.clone(),
        };
        std::thread::spawn(move || watch_loop(&thread_shared, job));
        Self { shared }
    }

    #[cfg(test)]
    fn idle() -> Self {
        Self {
            shared: Arc::new(Mutex::new(WatchShared::default())),
        }
    }

    fn attach(&self, stream: UnixStream) {
        if let Ok(mut shared) = self.shared.lock() {
            if shared.ended.is_some() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            shared.stream = Some(stream);
        }
    }

    fn detach(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.stream = None;
        }
    }

    fn ended(&self) -> Option<Ended> {
        self.shared.lock().ok().and_then(|shared| shared.ended)
    }

    fn end(shared: &Mutex<WatchShared>, why: Ended) {
        if let Ok(mut shared) = shared.lock() {
            if shared.ended.is_none() {
                shared.ended = Some(why);
            }
            if let Some(stream) = shared.stream.take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    fn finish(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.finished = true;
        }
    }
}

fn watch_loop(shared: &Mutex<WatchShared>, mut job: WatchJob) {
    let started = Instant::now();
    let mut tail = job.transcript.take().map(TranscriptTail::new);
    let mut seen = false;
    loop {
        if shared.lock().map(|s| s.finished).unwrap_or(true) {
            return;
        }
        if std::os::unix::process::parent_id() != job.parent {
            Watcher::end(shared, Ended::ParentGone);
            return;
        }
        if let Some(reader) = tail.as_mut() {
            reader.read_new(|line| job.call.feed_line(line));
            match job.call.state() {
                CallState::Finished(_) => {
                    Watcher::end(shared, Ended::Elsewhere);
                    return;
                }
                CallState::Pending(_) => seen = true,
                CallState::Unseen if started.elapsed() >= CALL_SEEN_WITHIN => {
                    hook_log(
                        &job.alias,
                        &job.session,
                        &summon_i18n::format!("r108377424b", job.tool),
                    );
                    tail = None;
                }
                CallState::Unseen => {}
            }
        }
        // 主 agent 的请求认不出时不能一直挂着:挂着的闸门压住这个会话的抓屏中介,而它的对话框
        // 两分钟就被自动拒绝了。认出来了就以转录为准,不受这个窗口约束。
        if job.main_agent && !seen && started.elapsed() >= MAIN_DIALOG_WINDOW {
            Watcher::end(shared, Ended::WindowPassed);
            return;
        }
        std::thread::sleep(WATCH_EVERY);
    }
}

/// 只往后读的转录读取器:首次从末尾 `TRANSCRIPT_TAIL_BYTES` 起(跳过被截断的第一行),之后
/// 每次只读新增的完整行。文件还没出现(子 agent 的转录稍后才建)就下次再试。
struct TranscriptTail {
    path: PathBuf,
    offset: Option<u64>,
    partial: String,
}

impl TranscriptTail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: None,
            partial: String::new(),
        }
    }

    fn read_new(&mut self, mut on_line: impl FnMut(&str)) {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return;
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return;
        };
        let (start, skip_first) = match self.offset {
            Some(offset) if offset <= len => (offset, false),
            // 文件比上次短(被重写):从头读。
            Some(_) => {
                self.partial.clear();
                (0, false)
            }
            None => {
                let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
                (start, start > 0)
            }
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            return;
        }
        let mut chunk = Vec::new();
        if file.take(len - start).read_to_end(&mut chunk).is_err() {
            return;
        }
        self.offset = Some(start + chunk.len() as u64);
        self.partial.push_str(&String::from_utf8_lossy(&chunk));
        let Some(last_newline) = self.partial.rfind('\n') else {
            return;
        };
        let complete: String = self.partial.drain(..=last_newline).collect();
        let mut lines = complete.lines();
        if skip_first {
            lines.next();
        }
        for line in lines {
            on_line(line);
        }
    }
}

/// hook 侧诊断日志。hook 是 claude 的子进程,不能写 stderr(会进 Claude 界面),也不能写
/// daemon.log(那是 daemon 的 stdout/stderr 重定向),单独一份。best-effort。
fn hook_log(alias: &str, session: &str, message: &str) {
    let path = crate::runtime::paths::run_dir().join("claude-hook.log");
    append_log(&path, alias, session, message);
}

fn append_log(path: &Path, alias: &str, session: &str, message: &str) {
    let line = format!(
        "[{}] [{alias}/{session}] {message}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    // 诊断日志写不进去不影响作答与否,丢掉即可。
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(line.as_bytes()));
}

/// PostCompact 上报的读写时限。daemon 收到后立即回应,卡片在 daemon 里另起线程发。
const COMPACT_REPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// PostCompact hook 进程入口:把「这个会话刚压缩完」报给 daemon 就退出。stdout / stderr 都不写
/// (会进 Claude 界面);daemon 不在、被拒都只记 hook 日志 —— 长度卡是附加信息,不该拖住 claude。
pub fn run_compact_hook(alias: &str, session: &str) {
    let mut raw = String::new();
    let _ = std::io::stdin().lock().read_to_string(&mut raw);
    let command = Command {
        actor: Actor::Owner,
        intent: Intent::ClaudeCompacted {
            alias: alias.to_string(),
            session: session.to_string(),
            trigger: claude_hook::compact_trigger(&raw),
        },
    };
    if let Err(why) = report_compaction(&command, || {
        UnixStream::connect(crate::runtime::paths::control_sock())
    }) {
        hook_log(alias, session, &summon_i18n::format!("ra7d6795ab1"));
    }
}

/// 经 control.sock 投一次上报,读回应。只试一次:daemon 正在重启时这张卡就不推了。
fn report_compaction(
    command: &Command,
    connect: impl Fn() -> std::io::Result<UnixStream>,
) -> Result<(), String> {
    let line = serde_json::to_string(command).map_err(|e| summon_i18n::format!("r702c1a7986"))?;
    let mut stream = connect().map_err(|e| summon_i18n::format!("r0f5871e23e"))?;
    let _ = stream.set_write_timeout(Some(COMPACT_REPORT_TIMEOUT));
    let _ = stream.set_read_timeout(Some(COMPACT_REPORT_TIMEOUT));
    stream
        .write_all(line.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|e| summon_i18n::format!("r6d48ef2f03"))?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|e| summon_i18n::format!("r994267f688"))?;
    match serde_json::from_str::<CommandOutcome>(response.trim()) {
        Ok(CommandOutcome::Done) => Ok(()),
        Ok(CommandOutcome::Denied(why)) => Err(why),
        Ok(other) => Err(summon_i18n::format!("r0a37abbae2")),
        Err(e) => Err(summon_i18n::format!("r98b530fb1f", response.trim())),
    }
}

/// 按可执行文件(解析符号链接后的路径、大小、修改时间)记下问过的结果。claude 自更新会换文件,
/// 键随之变化,下次起会话重新问。
type VersionKey = (PathBuf, u64, Option<std::time::SystemTime>);
static POST_COMPACT_KNOWN: LazyLock<Mutex<HashMap<VersionKey, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 起会话时实际会执行的那个程序:`cli_cmd` 的第一个词。绝对路径就是它本身;裸名按启动环境的 PATH
/// 逐段找,与 pane 里载入启动 env 文件后 shell 的查找一致。找不到返回 None。
///
/// PATH 为空、或有任何一段是相对的(含空段,即当前目录)时返回 None:shell 按顺序找,命中哪个取决于
/// pane 的工作目录,在 daemon 里判不准 —— 跳过相对段去命中后面的绝对段,探的就可能不是 pane 里跑的
/// 那个(bridge 解析目标 claude 也只认绝对目录)。
pub(crate) fn launch_program(cli_cmd: &str, launch_path: &std::ffi::OsStr) -> Option<PathBuf> {
    let binary = Path::new(cli_cmd.split_whitespace().next()?);
    if binary.is_absolute() {
        return summon_core::tools::find_executable(binary, std::ffi::OsStr::new(""));
    }
    if !path_is_absolute_only(launch_path) {
        return None;
    }
    summon_core::tools::find_executable(binary, launch_path)
}

/// PATH 的每一段都是绝对目录。空 PATH 拆出来是一个空段,不算。
fn path_is_absolute_only(path: &std::ffi::OsStr) -> bool {
    std::env::split_paths(path).all(|directory| directory.is_absolute())
}

/// `program` 认不认得 PostCompact hook 事件。认不出版本、问不到都按不认得:往认不得它的 claude 的
/// settings 里写这个事件,整份 settings 会被丢弃(见
/// [`summon_core::claude_hook::POST_COMPACT_MIN_VERSION`])。问不到的结果不缓存,下次起会话再问。
pub(crate) fn claude_knows_post_compact(program: &Path) -> bool {
    let Ok(real) = std::fs::canonicalize(program) else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(&real) else {
        return false;
    };
    let key = (real.clone(), meta.len(), meta.modified().ok());
    if let Some(known) = POST_COMPACT_KNOWN
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).copied())
    {
        return known;
    }
    let Some(known) =
        claude_version(&real).and_then(|output| claude_hook::post_compact_supported(&output))
    else {
        return false;
    };
    if let Ok(mut cache) = POST_COMPACT_KNOWN.lock() {
        cache.insert(key, known);
    }
    known
}

#[cfg(not(test))]
fn claude_version(exe: &Path) -> Option<String> {
    // 刚更新过的可执行文件头一次运行要过一遍系统安全评估,可能要几秒。
    version_output(exe, Duration::from_secs(5))
}

/// 测试不运行本机装的 claude:只回 [`FAKE_CLAUDE_VERSIONS`] 里登记的输出,其余按问不出 ——
/// 结果不随机器上装没装 claude、装了哪一版而变。
#[cfg(test)]
fn claude_version(exe: &Path) -> Option<String> {
    FAKE_CLAUDE_VERSIONS.lock().unwrap().get(exe).cloned()
}

/// 测试登记的 `--version` 输出,键是可执行文件解析符号链接后的路径。
#[cfg(test)]
pub(crate) static FAKE_CLAUDE_VERSIONS: LazyLock<Mutex<HashMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 运行 `<exe> --version`,有总时限。输出落临时文件而不是管道:管道写端被孙进程握着时,读 EOF
/// 可以无限期地等。
fn version_output(exe: &Path, timeout: Duration) -> Option<String> {
    let mut out = tempfile::tempfile().ok()?;
    let mut child = std::process::Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone().ok()?)
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        if Instant::now() >= deadline {
            // 恰好在这之间退出的话 kill 会报错,可忽略;wait 只为回收。
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return None;
    }
    out.rewind().ok()?;
    let mut text = String::new();
    out.take(4096).read_to_string(&mut text).ok()?;
    Some(text)
}

/// 某个 chat 此刻挂着的 hook 闸门:抓屏中介据此让位(不变量 7)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScreenClaim {
    /// 主 agent 的请求挂着:屏上那个对话框就是它。
    pub(crate) main: bool,
    /// 任何请求挂着(含子 agent):会话看起来卡住的原因已经在卡上了。
    pub(crate) any: bool,
}

/// 挂着的 hook 闸门:gate id → (chat_id, 是否主 agent)。Core 登记与落幕时增删;chat 线程只读。
/// 按 gate id 记,落幕的几条路径(resolve、取消、对端断开)各自释放也不会重复扣减。
static CLAIMS: LazyLock<Mutex<HashMap<String, (String, bool)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn claim_screen(gate_id: &str, chat_id: &str, main: bool) {
    if let Ok(mut claims) = CLAIMS.lock() {
        claims.insert(gate_id.to_string(), (chat_id.to_string(), main));
    }
}

pub(crate) fn release_screen(gate_id: &str) {
    if let Ok(mut claims) = CLAIMS.lock() {
        claims.remove(gate_id);
    }
}

pub(crate) fn screen_claim(chat_id: &str) -> ScreenClaim {
    let Ok(claims) = CLAIMS.lock() else {
        return ScreenClaim::default();
    };
    claims.values().filter(|(chat, _)| chat == chat_id).fold(
        ScreenClaim::default(),
        |claim, (_, main)| ScreenClaim {
            main: claim.main || *main,
            any: true,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::net::UnixListener;

    fn input(tool: &str, tool_input: serde_json::Value) -> HookInput {
        claude_hook::parse_hook_input(
            &json!({"session_id": "s", "transcript_path": "/x/s.jsonl",
                    "tool_name": tool, "tool_input": tool_input})
            .to_string(),
        )
        .unwrap()
    }

    fn command() -> Command {
        Command {
            actor: Actor::Owner,
            intent: Intent::ClaudeHookAsk {
                alias: "a".into(),
                session: "soul-a-g1".into(),
                request: input("Bash", json!({"command": "rm x"})).request,
            },
        }
    }

    const FAST: Windows = Windows {
        first: Duration::from_millis(300),
        reconnect: Duration::from_millis(600),
        retry_every: Duration::from_millis(20),
    };

    #[test]
    fn permission_answers_map_to_allow_and_deny_and_nothing_else() {
        let req = input("Bash", json!({"command": "rm x"}));
        let value = |a: &str| CommandOutcome::Value(json!({"answer": a}));
        assert_eq!(
            decision_for(HookKind::Permission, &req, &value("allow")),
            Some(HookDecision::Allow {
                updated_input: None
            })
        );
        assert_eq!(
            decision_for(HookKind::Permission, &req, &value("deny")),
            Some(HookDecision::Deny {
                message: ::summon_core::i18n::label(PERMISSION_DENIED_MESSAGE).into()
            })
        );
        assert_eq!(
            decision_for(HookKind::Permission, &req, &value("yes")),
            None
        );
        assert_eq!(
            decision_for(
                HookKind::Permission,
                &req,
                &CommandOutcome::Denied("闸门已取消".into())
            ),
            None,
            "被拒 / 取消一律不作答,绝不能变成 allow"
        );
    }

    #[test]
    fn ask_answers_become_updated_input_and_empty_answers_become_skip() {
        let req = input(
            "AskUserQuestion",
            json!({"questions": [{"question": "Q", "options": [{"label": "A"}, {"label": "B"}]}]}),
        );
        let value = |a: &str| CommandOutcome::Value(json!({"answer": a}));
        match decision_for(HookKind::Ask, &req, &value(r#"["B"]"#)) {
            Some(HookDecision::Allow {
                updated_input: Some(updated),
            }) => assert_eq!(updated["answers"], json!({"Q": "B"})),
            other => panic!("应带答案放行: {other:?}"),
        }
        assert_eq!(
            decision_for(HookKind::Ask, &req, &value("[]")),
            Some(HookDecision::Deny {
                message: ::summon_core::i18n::label(ASK_SKIPPED_MESSAGE).into()
            })
        );
    }

    fn serve_once(listener: &UnixListener, reply: Option<&str>) {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("ClaudeHookAsk"), "{line}");
        if let Some(reply) = reply {
            let mut stream = stream;
            stream.write_all(reply.as_bytes()).unwrap();
            stream.write_all(b"\n").unwrap();
        }
    }

    #[test]
    fn reconnects_and_resubmits_when_daemon_restarts_mid_wait() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            serve_once(&listener, None); // 第一代 daemon:收下请求后消失
            serve_once(&listener, Some(r#"{"Value":{"answer":"allow"}}"#));
        });
        let watcher = Watcher::idle();
        let outcome = ask_daemon(&command(), || UnixStream::connect(&sock), &watcher, FAST);
        server.join().unwrap();
        assert_eq!(
            outcome,
            Some(CommandOutcome::Value(json!({"answer": "allow"})))
        );
    }

    #[test]
    fn gives_up_without_answer_when_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("absent.sock");
        let started = Instant::now();
        let outcome = ask_daemon(
            &command(),
            || UnixStream::connect(&sock),
            &Watcher::idle(),
            FAST,
        );
        assert_eq!(outcome, None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn compacted_command() -> Command {
        Command {
            actor: Actor::Owner,
            intent: Intent::ClaudeCompacted {
                alias: "a".into(),
                session: "soul-a-g1".into(),
                trigger: Some("manual".into()),
            },
        }
    }

    /// 上报只投一次、读回应:Done 才算报到,被拒带回原因,daemon 不在立刻放弃。
    #[test]
    fn a_compaction_report_is_sent_once_and_reads_the_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        for (reply, expected) in [
            (r#""Done""#, Ok(())),
            (r#"{"Denied":"不属于"}"#, Err("不属于".to_string())),
        ] {
            let server = {
                let listener = listener.try_clone().unwrap();
                std::thread::spawn(move || {
                    let (stream, _) = listener.accept().unwrap();
                    let mut line = String::new();
                    BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap();
                    let mut stream = stream;
                    stream.write_all(reply.as_bytes()).unwrap();
                    stream.write_all(b"\n").unwrap();
                    line
                })
            };
            let outcome = report_compaction(&compacted_command(), || UnixStream::connect(&sock));
            let line = server.join().unwrap();
            assert!(
                line.contains("ClaudeCompacted") && line.contains("soul-a-g1"),
                "{line}"
            );
            assert_eq!(outcome, expected);
        }

        let absent = dir.path().join("absent.sock");
        let started = Instant::now();
        assert!(report_compaction(&compacted_command(), || UnixStream::connect(&absent)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// 只有问得出版本、且版本认得 PostCompact 的 claude 才写这个 hook;问不出一律按不认得。
    #[test]
    fn post_compact_follows_the_launched_claude_version() {
        let dir = tempfile::tempdir().unwrap();
        let fake = |name: &str, version: Option<&str>| {
            let path = dir.path().join(name);
            crate::test_support::write_test_executable(&path, &format!("#!/bin/sh\n# {name}\n"));
            if let Some(version) = version {
                FAKE_CLAUDE_VERSIONS
                    .lock()
                    .unwrap()
                    .insert(std::fs::canonicalize(&path).unwrap(), version.to_string());
            }
            path
        };
        let new = fake("claude-new", Some("2.1.304 (Claude Code)\n"));
        let old = fake("claude-old", Some("2.1.75 (Claude Code)\n"));
        let odd = fake("claude-odd", Some("Claude Code\n"));
        let mute = fake("claude-mute", None);
        assert!(claude_knows_post_compact(&new));
        assert!(!claude_knows_post_compact(&old));
        assert!(!claude_knows_post_compact(&odd));
        assert!(!claude_knows_post_compact(&mute), "问不出版本按不认得");
        assert!(!claude_knows_post_compact(&dir.path().join("missing")));

        // 符号链接按它指向的文件问,与 claude 自更新换链接目标的装法一致。
        let to_new = dir.path().join("claude");
        std::os::unix::fs::symlink(&new, &to_new).unwrap();
        assert!(claude_knows_post_compact(&to_new));
        let to_old = dir.path().join("claude-previous");
        std::os::unix::fs::symlink(&old, &to_old).unwrap();
        assert!(!claude_knows_post_compact(&to_old));
    }

    /// 探测的是启动时会执行的那个程序:裸名按启动环境的 PATH 找,不是 daemon 自己的 PATH。
    #[test]
    fn the_launch_program_is_found_on_the_launch_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let claude = bin.join("claude");
        crate::test_support::write_test_executable(&claude, "#!/bin/sh\n");
        let launch_path = std::env::join_paths([dir.path().join("empty"), bin.clone()]).unwrap();
        assert_eq!(launch_program("claude", &launch_path), Some(claude.clone()));
        assert_eq!(
            launch_program("claude --dangerously-skip-permissions", &launch_path),
            Some(claude.clone()),
            "只看第一个词"
        );
        assert_eq!(
            launch_program(&claude.to_string_lossy(), std::ffi::OsStr::new("")),
            Some(claude.clone()),
            "绝对路径不查 PATH"
        );
        assert_eq!(
            launch_program("claude", std::ffi::OsStr::new("/nonexistent")),
            None
        );
        assert_eq!(launch_program("", &launch_path), None);

        // 相对段排在命中的绝对段之前:pane 里先找相对段,不能跳过它去报后面那个。
        let with_relative = std::env::join_paths([PathBuf::from("rel"), bin.clone()]).unwrap();
        assert_eq!(launch_program("claude", &with_relative), None);
        let with_empty = format!(":{}", bin.display());
        assert_eq!(
            launch_program("claude", std::ffi::OsStr::new(&with_empty)),
            None,
            "空段就是当前目录"
        );
        assert_eq!(launch_program("claude", std::ffi::OsStr::new("")), None);

        // 全是相对段:相对 daemon 当前目录确实找得到同名可执行文件,也不认。
        let cwd = std::env::current_dir().unwrap();
        let relative = format!(
            "{}{}",
            "../".repeat(cwd.components().count()),
            std::fs::canonicalize(&bin)
                .unwrap()
                .strip_prefix("/")
                .unwrap()
                .display()
        );
        assert!(Path::new(&relative).join("claude").is_file(), "{relative}");
        assert_eq!(
            launch_program("claude", std::ffi::OsStr::new(&relative)),
            None
        );
        struct RemoveOnDrop(PathBuf);
        impl Drop for RemoveOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let name = format!("summon-launch-probe-{}", std::process::id());
        let in_cwd = RemoveOnDrop(cwd.join(&name));
        crate::test_support::write_test_executable(&in_cwd.0, "#!/bin/sh\n");
        assert!(Path::new(&name).is_file());
        for all_relative in ["rel", "rel:other"] {
            assert_eq!(
                launch_program(&name, std::ffi::OsStr::new(all_relative)),
                None,
                "{all_relative}"
            );
        }
    }

    #[test]
    fn only_paths_made_of_absolute_directories_are_searched() {
        for (path, searched) in [
            ("/usr/bin:/bin", true),
            ("/usr/bin", true),
            ("", false),
            (":", false),
            ("/usr/bin:", false),
            ("rel:/usr/bin", false),
            ("/usr/bin:rel", false),
            ("./bin", false),
        ] {
            assert_eq!(
                path_is_absolute_only(std::ffi::OsStr::new(path)),
                searched,
                "{path}"
            );
        }
    }

    #[test]
    fn version_output_reads_stdout_and_gives_up_on_failure_or_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let fake = |name: &str, script: &str| {
            let path = dir.path().join(name);
            crate::test_support::write_test_executable(&path, script);
            path
        };
        let ok = fake("ok", "#!/bin/sh\necho '2.1.304 (Claude Code)'\n");
        let failing = fake("failing", "#!/bin/sh\nexit 1\n");
        let slow = fake("slow", "#!/bin/sh\nexec sleep 5\n");
        assert_eq!(
            version_output(&ok, Duration::from_secs(5)).as_deref(),
            Some("2.1.304 (Claude Code)\n")
        );
        assert_eq!(version_output(&failing, Duration::from_secs(5)), None);
        let started = Instant::now();
        assert_eq!(version_output(&slow, Duration::from_millis(200)), None);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn ending_elsewhere_closes_the_pending_connection_and_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (hup_tx, hup_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            // daemon 侧:等对端断开(EOF)—— 这就是闸门取消、卡片作废的信号。
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest);
            hup_tx.send(()).unwrap();
        });
        let watcher = Watcher::idle();
        let shared = Arc::clone(&watcher.shared);
        let ender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            Watcher::end(&shared, Ended::Elsewhere);
        });
        let outcome = ask_daemon(&command(), || UnixStream::connect(&sock), &watcher, FAST);
        ender.join().unwrap();
        assert_eq!(outcome, None);
        hup_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("daemon 应看到对端断开");
        server.join().unwrap();
    }

    #[test]
    fn transcript_tail_reads_only_complete_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "one\ntwo\npart").unwrap();
        let mut tail = TranscriptTail::new(path.clone());
        let mut seen = Vec::new();
        tail.read_new(|l| seen.push(l.to_string()));
        assert_eq!(seen, vec!["one", "two"]);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"ial\nthree\n").unwrap();
        tail.read_new(|l| seen.push(l.to_string()));
        assert_eq!(seen, vec!["one", "two", "partial", "three"]);
    }

    #[test]
    fn transcript_tail_skips_the_cut_first_line_of_a_long_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let filler = "x".repeat((TRANSCRIPT_TAIL_BYTES + 10) as usize);
        std::fs::write(&path, format!("{filler}\nlast\n")).unwrap();
        let mut tail = TranscriptTail::new(path);
        let mut seen = Vec::new();
        tail.read_new(|l| seen.push(l.to_string()));
        assert_eq!(seen, vec!["last"]);
    }

    #[test]
    fn screen_claims_split_main_and_subagent_and_release_by_gate() {
        claim_screen("g-main", "oc_claim_test", true);
        claim_screen("g-sub", "oc_claim_test", false);
        assert_eq!(
            screen_claim("oc_claim_test"),
            ScreenClaim {
                main: true,
                any: true
            }
        );
        release_screen("g-main");
        release_screen("g-main");
        assert_eq!(
            screen_claim("oc_claim_test"),
            ScreenClaim {
                main: false,
                any: true
            }
        );
        release_screen("g-sub");
        assert_eq!(screen_claim("oc_claim_test"), ScreenClaim::default());
        assert_eq!(screen_claim("oc_other"), ScreenClaim::default());
    }

    #[test]
    fn append_log_writes_one_line_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude-hook.log");
        append_log(&path, "a", "s", "one");
        append_log(&path, "a", "s", "two");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("[a/s] two"));
    }
}
