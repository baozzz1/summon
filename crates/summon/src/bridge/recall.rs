//! 会话召回的 IO 侧:跑 `claude agents --json`(活跃)、扫盘 `~/.claude/projects/`(历史)。
//! 纯合并/排序/分页在 `summon_core::recall`,这里只采集事实喂给它。

use serde::{Deserialize, Serialize};
use std::path::Path;
use summon_core::bridge::ProcessFingerprint;
use summon_core::recall::{
    paginate_candidates, recall_page, ClaudeAgentEntry, DiskSessionEntry, RecallPage,
    RecallProvider, SessionCandidate,
};

pub(crate) const RECALL_SOURCE_OBSERVATION_SCHEMA_VERSION: u32 = 1;

/// `claude agents --json` 的硬 deadline。整条 `/bridge` 发现是在 agent dispatcher 线程上**同步**跑的,
/// 外部 CLI 一旦挂住(等 TTY、网络重试、卡住的 node 进程)就会把该 agent 的整条派发路径堵死,所以
/// 裸 `.output()` 不可接受。5s 与 `handoff::TARGET_AUTH_TIMEOUT` 对齐(同样是本机 claude 只读查询),
/// 该命令正常在百毫秒级返回,这个值只用来兜「挂死」,不是正常路径的余量。
const CLAUDE_AGENTS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 发现面输出上界。活跃会话每条 JSON 约几百字节,1 MiB 足够容纳数千条;超过即视为异常输出,
/// 不再读进内存去解析(tempfile 承接不撑爆内存,serde 解析会)。
const MAX_CLAUDE_AGENTS_BYTES: u64 = 1024 * 1024;

/// 等待子进程退出的轮询间隔,与 handoff 的目标 Bot 预检一致;对 5s deadline 足够精细。
const CLAUDE_AGENTS_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// 召回卡渲染时刻的 exact source 指纹，只落本机 0600 文件。飞书按钮仅携带 `id`，既避免
/// 泄露 executable 路径，也让点击时能用“发现时刻基线”识别 PID reuse，而非现场自证自洽。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecallSourceObservation {
    pub schema_version: u32,
    pub id: String,
    pub sid: String,
    pub process: ProcessFingerprint,
    pub created_at_ms: u64,
}

impl RecallSourceObservation {
    pub(crate) fn new(sid: &str, process: ProcessFingerprint, created_at_ms: u64) -> Self {
        Self {
            schema_version: RECALL_SOURCE_OBSERVATION_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            sid: sid.to_string(),
            process,
            created_at_ms,
        }
    }
}

/// 解析 `claude agents --json` 的输出。坏 JSON 不 panic——发现面坏了不该带崩整个 `/bridge`,
/// 降级为「没有活跃会话」(历史扫盘仍独立可用)。
#[cfg(test)]
pub fn parse_claude_agents(json: &[u8]) -> Vec<ClaudeAgentEntry> {
    match serde_json::from_slice::<Vec<ClaudeAgentEntry>>(json) {
        Ok(entries) => entries,
        Err(e) => {
            crate::observe::log::log(&format!(
                "[recall] 解析 claude agents --json 失败(忽略): {e}"
            ));
            Vec::new()
        }
    }
}

fn run_claude_agents_strict() -> anyhow::Result<Vec<ClaudeAgentEntry>> {
    run_claude_agents_bounded(
        &summon_core::tools::resolve(summon_core::tools::Tool::Claude),
        CLAUDE_AGENTS_TIMEOUT,
        MAX_CLAUDE_AGENTS_BYTES,
    )
}

/// 有界执行 `<program> agents --json`:stdout 落 tempfile(不用 pipe——超时后没人读 pipe,子进程写满
/// 缓冲会反过来卡死终止路径)、stdin 置空(只读查询绝不该等 TTY 输入)、到点终止。
///
/// 终止对象只有**本函数自己 spawn 的这个只读查询子进程**,与 tmux / `soul-*` 会话无关。
fn run_claude_agents_bounded(
    program: &Path,
    timeout: std::time::Duration,
    max_bytes: u64,
) -> anyhow::Result<Vec<ClaudeAgentEntry>> {
    use anyhow::Context as _;
    use std::io::{Read as _, Seek as _};

    let mut sink = tempfile::tempfile().context(summon_i18n::text!("r161094df7d"))?;
    let mut child = crate::runtime::process::retry_text_file_busy(|| {
        let writer = sink.try_clone()?;
        std::process::Command::new(program)
            .args(["agents", "--json"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(writer))
            .stderr(std::process::Stdio::null())
            .spawn()
    })
    .context(summon_i18n::text!("r5a3ca2de35"))?;
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context(summon_i18n::text!("r0e576e371f"))?
        {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            // kill 后必须 wait 回收,否则 daemon 里留一串僵尸。两处失败都无从补救,忽略。
            let _ = child.wait();
            anyhow::bail!(summon_i18n::format!("racbe10d00a", timeout.as_millis()));
        }
        std::thread::sleep(CLAUDE_AGENTS_POLL);
    };
    anyhow::ensure!(status.success(), summon_i18n::format!("r1f91bf6cdd"));
    sink.rewind().context(summon_i18n::text!("r30eb5ca572"))?;
    let mut bytes = Vec::new();
    sink.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .context(summon_i18n::text!("re496d732c6"))?;
    anyhow::ensure!(
        bytes.len() as u64 <= max_bytes,
        summon_i18n::format!("rf30d8b5f40")
    );
    serde_json::from_slice(&bytes).context(summon_i18n::text!("rd02f96082d"))
}

/// 跑 `claude agents --json` 拿活跃会话,**再排除 summon 此刻在维护的托管会话**。
/// 命令失败(claude 不在 PATH、非零退出)→ 空 + log,**不整批失败**:历史扫盘独立可用。
pub fn discover_active() -> Vec<ClaudeAgentEntry> {
    discover_active_with_process_observations(true)
}

/// Menu Bar 只展示外部会话，不提供 takeover，因此无需每 10 秒为每个 Claude 采 exact
/// 进程指纹。托管会话排除规则仍与 Bridge 共用，避免两套发现语义漂移。
pub fn discover_active_for_display() -> Vec<ClaudeAgentEntry> {
    discover_active_with_process_observations(false)
}

fn discover_active_with_process_observations(observe_processes: bool) -> Vec<ClaudeAgentEntry> {
    let all = match run_claude_agents_strict() {
        Ok(all) => all,
        Err(error) => {
            crate::observe::log::log(&summon_i18n::format!("red61f44c78"));
            return Vec::new();
        }
    };
    let live_tmux: std::collections::HashSet<String> = crate::session::tmux::list_all_sessions()
        .into_iter()
        .collect();
    let mut external = all
        .into_iter()
        .filter(|a| !is_summon_managed(a.name.as_deref(), &live_tmux))
        .collect::<Vec<_>>();
    if !observe_processes {
        return external;
    }
    for entry in &mut external {
        let Some(pid) = entry.pid else {
            continue;
        };
        match crate::bridge::process_probe::observe_exact_fingerprint(pid) {
            Ok(Some(process)) => entry.observed_process = Some(process),
            Ok(None) => {}
            Err(error) => {
                crate::observe::log::log(&summon_i18n::format!("r4134a24fe1", entry.session_id))
            }
        }
    }
    external
}

/// 杀源完成后必须严格证明该 SID 没有第二个 live writer。与列表展示的降级语义不同：命令
/// 失败/坏 JSON 不能解释成“没有活跃会话”，否则会在不可证明时启动第二个 Claude。
fn ensure_no_live_source_for_sid(sid: &str) -> anyhow::Result<()> {
    let active = run_claude_agents_strict()?;
    anyhow::ensure!(
        !active
            .iter()
            .any(|entry| entry.session_id == sid && entry.pid.is_some()),
        summon_i18n::format!("rf4284fd322")
    );
    Ok(())
}

/// 发 SIGTERM 的最后一刻重新读取 `claude agents`:exact SID/PID 必须仍在严格发现面上;命令失败、
/// SID/PID 已不在发现面都不能沿用更早的结论。忙碌会话的代价(打断这一轮、停掉后台任务)由接管
/// 确认框说明。
fn ensure_source_listed_at_signal(sid: &str, pid: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        source_listed(&run_claude_agents_strict()?, sid, pid),
        summon_i18n::format!("r2d9c489c53")
    );
    Ok(())
}

fn source_listed(active: &[ClaudeAgentEntry], sid: &str, pid: u32) -> bool {
    active
        .iter()
        .any(|entry| entry.session_id == sid && entry.pid == Some(pid))
}

/// 历史路径也不能盲信列表展示时的 `live_pid=None`：点击时发现命令可能瞬时降级，或该 SID
/// 已被别处重新 resume。进入 Stage 1 前统一要求 transcript 稳定且严格发现面无 live writer。
pub(crate) fn prove_no_source_writer(
    sid: &str,
    config_dir: &Path,
    transcript_path: &Path,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    crate::bridge::handoff::verify_stable_transcript(transcript_path, config_dir, sid)
        .context(summon_i18n::text!("re94f64412b"))?;
    ensure_no_live_source_for_sid(sid)
}

/// 判定一条活跃会话是否 summon **此刻在维护**(该排除,接管自己没意义、还会杀自己的会话)。
///
/// `claude agents` 的 name 若是 `summon:soul-<tmux>`,那是 summon 起会话时打的标记。但**只看名字不够**:
/// summon 在 `/session` 退出后 tmux 被杀,用户可能手动 `claude --resume "summon:soul-..."` 重新起——
/// 那时 summon 已放弃它,应可召回(如 arcadia)。真信号是**对应 tmux 此刻是否存在**:
/// 存在 = summon 在维护 → 排除;不存在 = 已放弃 → 保留可召回。纯判定,`live_tmux` 由调用方给。
fn is_summon_managed(name: Option<&str>, live_tmux: &std::collections::HashSet<String>) -> bool {
    match name.and_then(|n| n.strip_prefix("summon:")) {
        Some(tmux) if tmux.starts_with("soul-") => live_tmux.contains(tmux),
        _ => false,
    }
}

/// 从 transcript 内容读第一个 `cwd`。**不从文件名/目录名解码**——claude 把 `/` 编码成 `-`,
/// 下划线也变 `-`,`disk_clean` 与 `disk-clean` 编码后同名不可逆。逐行找首个含 cwd 的记录。
/// 只对当前页的候选调用(O(page_size)),不全盘读。
pub fn read_transcript_cwd(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        // 快速预筛,避免对每行 JSON 全量解析。
        if !line.contains("\"cwd\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                return Some(cwd.to_string());
            }
        }
    }
    None
}

/// 召回列表的稳定任务线索：只读 transcript 前缀里的第一条可见用户输入。预算和隐私过滤均在
/// summon-core 解析器内，避免把 tool result/thinking/sidechain 带进私聊列表卡。
pub fn read_transcript_prompt_preview(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    summon_core::claude_transcript::read_first_user_prompt(
        std::io::BufReader::new(file),
        summon_core::claude_transcript::PromptPreviewLimits::default(),
    )
    .ok()
    .flatten()
}

/// 扫 `~/.claude/projects/*/*.jsonl` 发现历史会话。**只 stat 取 mtime,不读内容**——cwd 延后到
/// 当前页再逐个读(见 `read_transcript_cwd`),避免对 938 个文件全读。文件名去 `.jsonl` = sid。
/// 坏名/stat 失败的条目跳过,不中断整批。
pub fn scan_disk_sessions(projects_dir: &Path) -> Vec<DiskSessionEntry> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(projects_dir) else {
        return out;
    };
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            let Some(sid) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let Some(mtime) = f
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
            else {
                continue;
            };
            out.push(DiskSessionEntry {
                sid: sid.to_string(),
                cwd: String::new(), // 延后:当前页再读 read_transcript_cwd
                name: None,
                last_active_ms: mtime,
            });
        }
    }
    out
}

/// 只为当前页补 transcript 元数据：历史候选补 cwd，所有候选补“起始任务”预览。全量扫盘仍只
/// stat，不把读取成本乘到 1000+ 会话；每条 prompt 又受 256KiB 前缀预算约束。
pub fn fill_metadata_for_page(result: &mut RecallPage, projects_dir: &Path) {
    for item in &mut result.items {
        let Some(path) = transcript_path(projects_dir, &item.sid) else {
            continue;
        };
        if item.cwd.is_empty() {
            if let Some(cwd) = read_transcript_cwd(&path) {
                item.cwd = cwd;
            }
        }
        item.prompt_preview = read_transcript_prompt_preview(&path).unwrap_or_default();
    }
}

/// 给页上忙碌、带接管凭证的活跃会话列出账上还在跑的后台任务,供接管确认框点名。账本要从头读
/// 整份转录(常驻任务可能是很久以前起的),所以只读接管真能执行的那几条。只是展示,读不出就不列。
fn fill_background_tasks_for_page(result: &mut RecallPage, projects_dir: &Path) {
    for item in &mut result.items {
        if item.live_status.as_deref() != Some("busy") || item.source_observation_id.is_none() {
            continue;
        }
        let Some(path) = transcript_path(projects_dir, &item.sid) else {
            continue;
        };
        match crate::capture::pending_background_tasks(&path) {
            Ok(tasks) => item.background_tasks = tasks.iter().map(background_task_label).collect(),
            Err(error) => crate::observe::log::log(&summon_i18n::format!("r271e8b033c", item.sid)),
        }
    }
}

/// 接管确认框里一个后台任务的说法:种类,加上发起调用时写的 description(过长截断)。
fn background_task_label(task: &crate::capture::PendingBackgroundTask) -> String {
    const MAX_CHARS: usize = 24;
    let kind = match task.kind {
        crate::capture::BackgroundKind::Shell => summon_i18n::text!("r24675de025"),
        crate::capture::BackgroundKind::Agent => summon_i18n::text!("r58138d0b95"),
    };
    let Some(description) = task.description.as_deref() else {
        return kind.to_string();
    };
    let compact = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped = if compact.chars().count() > MAX_CHARS {
        let mut head: String = compact.chars().take(MAX_CHARS - 1).collect();
        head.push('…');
        head
    } else {
        compact
    };
    format!("{kind}「{clipped}」")
}

/// 从 sid 反查 transcript 文件路径(遍历项目目录找 `<sid>.jsonl`)。
fn transcript_path(projects_dir: &Path, sid: &str) -> Option<std::path::PathBuf> {
    let projects = std::fs::read_dir(projects_dir).ok()?;
    let target = format!("{sid}.jsonl");
    for project in projects.flatten() {
        let candidate = project.path().join(&target);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 把一个历史会话(硬盘 transcript,无进程)恢复为该 agent 的 tmux 会话。
///
/// 手法:把外部 sid 写进 recall 专用 tmux 名的 resume_store,再 `ensure_started`——它读到记录就走
/// resume 分支起 `claude --resume <sid>`,续接原会话。tmux 名用 `soul-<alias>-recall-<sid8>`,
/// 不冲突现有 agent+chat 会话,且带 `soul-<alias>` 前缀可被 `summon agents attach` 找到。
///
/// 返回起好的 tmux 会话名。`ColdStarted`(resume 没续上,claude 冷启了新会话)视为失败——
/// 那不是用户要恢复的会话。
pub fn resume_historical(
    agent: &summon_core::registry::Agent,
    sid: &str,
    cwd: &str,
    poll_ms: u64,
    debounce_ms: u64,
) -> anyhow::Result<String> {
    use crate::session::session::{Session, Start};
    let short = &sid[..sid.len().min(8)];
    let tmux_name = format!("{}-recall-{}", agent.tmux_name(), short);
    // 外部 sid 写进 resume_store → ensure_started 据此 `claude --resume <sid>` 续接。
    // created_at 留空:外部历史会话的原始起始时间未知,退出卡片不展示。
    crate::session::resume_store::write(agent, &tmux_name, agent.cli_kind(), sid, Some(cwd), None);
    // ★ 起会话必须用**会话原 cwd**(candidate.cwd),不能用 agent 的 working_dir。
    // `claude --resume <sid>` 只在会话原 project 目录下才找得到那条 transcript;若在 agent 的
    // workspace 里跑,claude 找不到 → 冷启空会话(实测:所有 resume 都因此冷启,从没真正续接过)。
    // with_work_dir 是 effective_start_work_dir 的第一优先。
    let sess =
        Session::with_name(agent, tmux_name.clone(), poll_ms, debounce_ms).with_work_dir(Some(cwd));
    match sess.ensure_started()? {
        Start::Resumed | Start::AlreadyRunning => Ok(tmux_name),
        Start::ColdStarted => {
            // resume 没续上 → claude 在会话原 cwd 也找不到这个 sid(多半是该会话被反复 resume/fork
            // 折腾坏了,如 b4008aca)。清 resume 记录避免下次用坏记录;冷启的 tmux 会话留给用户
            // `summon agents down` 清——**此处不擅杀 tmux,守第一红线**(recall 不在合法销毁途径内)。
            crate::session::resume_store::clear(agent, &tmux_name);
            anyhow::bail!(summon_i18n::format!("rfb0804ce99", agent.name))
        }
    }
}

/// Stage 2 杀源前置步的结果。两种结果都已经证明 source Gone + transcript 稳定 + 无同 SID
/// live writer，调用方此后才能落 Stage 1 attempt。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// exact SIGTERM 后确认源已退出。
    Killed,
    /// 点击前源已自然退出。
    AlreadyGone,
}

/// 把一个活跃外部 Claude 安全降为“历史会话”。顺序是安全证明的一部分：
/// 1) 发现时刻指纹 + executable + registry sid/procStart/mtime 三重身份闸；
/// 2) SIGTERM 原语内部强制 transcript 资产守卫；
/// 3) 只发一次 exact SIGTERM，5s 未退拒绝升级；
/// 4) Gone 后 transcript 500ms 稳定窗 + 严格 `claude agents` 无同 SID writer。
///
/// 本函数**不落 recall_resume attempt、不起 staging**。调用方只有收到 Ok 后才能进入 Stage 1，
/// 从结构上保证 attempt 永远不会在源仍活着时触发 `claude --resume` 双写。
pub(crate) fn terminate_active_source(
    observation: &RecallSourceObservation,
    sid: &str,
    config_dir: &Path,
    transcript_path: &Path,
    expected_executable: &Path,
) -> anyhow::Result<KillOutcome> {
    use crate::bridge::handoff::{
        check_source_process, signal_exact_source_termination, ProcessCheck, SourceTranscriptGuard,
    };
    use crate::bridge::process_probe::{
        probe_exact_process, verify_active_recall_source, ExactProcessStatus,
    };
    use anyhow::Context as _;

    anyhow::ensure!(observation.sid == sid, summon_i18n::format!("r6c6e906632"));
    let expected = &observation.process;
    let outcome = match probe_exact_process(expected)? {
        ExactProcessStatus::Gone => KillOutcome::AlreadyGone,
        ExactProcessStatus::PidReused(_) => {
            anyhow::bail!(summon_i18n::format!("red8cea3a70"))
        }
        ExactProcessStatus::AliveExact => {
            verify_active_recall_source(expected, sid, config_dir, expected_executable)?;
            let guard = SourceTranscriptGuard::new(transcript_path, config_dir, sid);
            signal_exact_source_termination(expected, &guard, || {
                ensure_source_listed_at_signal(sid, expected.pid)
            })?;
            let mut gone = false;
            for _ in 0..100 {
                match check_source_process(expected) {
                    ProcessCheck::Gone => {
                        gone = true;
                        break;
                    }
                    ProcessCheck::AliveExact => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    ProcessCheck::PidReused(_) => {
                        anyhow::bail!(summon_i18n::format!("r1f4ebdd029"))
                    }
                    ProcessCheck::Unreadable => {
                        anyhow::bail!(summon_i18n::format!("r479461ddf0"))
                    }
                }
            }
            anyhow::ensure!(gone, summon_i18n::format!("r6fbf6f21e9", expected.pid));
            KillOutcome::Killed
        }
    };

    prove_no_source_writer(sid, config_dir, transcript_path)
        .context(summon_i18n::text!("r437d40bf24"))?;
    Ok(outcome)
}

/// 采集召回全集并渲染成私聊用的 `card_recall` 交互卡片(群 `@Bot /bridge` 私聊入口用)。
///
/// IO(跑 `claude agents`、扫盘、取时钟)在此聚合;纯合并/分页/渲染委托 core。相对时间仅用于展示,
/// 不需可复现,故内部取真实时钟。空集也返回合法卡片(card_recall 内含「没有可召回的会话」文案)。
/// `origin` = 发起 `/bridge` 的原群 `(chat_id, chat_type)`,进按钮 value 让回调知道绑哪个群;
/// `None`(DM 或旧卡)→ 按钮不带群,回调回落本地 attach。
pub fn recall_card(
    store: &crate::bridge::store::BridgeStore,
    page: usize,
    page_size: usize,
    origin: Option<(&str, &str)>,
    remote_controlled: &RemoteControlled,
    finish: &dyn Fn(String) -> String,
) -> String {
    let projects = dirs::home_dir()
        .map(|home| home.join(".claude/projects"))
        .unwrap_or_default();
    // 遥控绑到发起 `/bridge` 的群,没有目标群时用不上 cmux 记录。
    let cmux_sessions = if origin.is_some() {
        crate::remote::cmux::load_cmux_sessions()
            .map(|hooks| hooks.sessions)
            .unwrap_or_else(|error| {
                crate::observe::log::log(&summon_i18n::format!("r4546486e62"));
                Vec::new()
            })
    } else {
        Vec::new()
    };
    recall_card_from(
        &discover_active(),
        &projects,
        page,
        page_size,
        now_unix_ms(),
        origin,
        store,
        &cmux_sessions,
        remote_controlled,
        finish,
    )
}

/// 哪些 Claude 会话正在被遥控(本 agent 名下未结束的遥控绑定)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteControlled {
    Known(std::collections::BTreeSet<String>),
    /// 遥控记录读不出:哪条活跃会话都可能在遥控中。
    Unreadable,
}

impl Default for RemoteControlled {
    fn default() -> Self {
        Self::Known(Default::default())
    }
}

fn remote_controlled_for(agent: &summon_core::registry::Agent) -> RemoteControlled {
    match crate::remote::store::RemoteStore::for_agent(agent).list() {
        Ok(bindings) => RemoteControlled::Known(
            bindings
                .into_iter()
                .filter(|binding| binding.ended.is_none())
                .map(|binding| binding.session_id)
                .collect(),
        ),
        Err(error) => {
            crate::observe::log::log(&summon_i18n::format!("rf07dfdcaea", agent.name));
            RemoteControlled::Unreadable
        }
    }
}

/// `/bridge` 的 provider 单一分派点：每个 agent 只看到自己 CLI 能消费的原生会话。
/// Claude 保留原有 transcript takeover；Cursor/Codex 走无头历史绑定。
pub(crate) struct RecallCardRequest<'a> {
    pub(crate) page: usize,
    pub(crate) page_size: usize,
    pub(crate) origin: Option<(&'a str, &'a str)>,
    pub(crate) binding_scope: &'a str,
    pub(crate) requester_open_id: &'a str,
    pub(crate) target_lifecycle_token: &'a str,
    pub(crate) card_issued_at_ms: u64,
    /// 主动私聊时卡顶标注的来源群;翻页 PATCH 不带。
    pub(crate) source_note: Option<&'a str>,
}

pub(crate) fn recall_card_for_agent(
    agent: &summon_core::registry::Agent,
    inflight: &crate::runtime::inflight::InFlight,
    store: &crate::bridge::store::BridgeStore,
    request: RecallCardRequest<'_>,
) -> String {
    let RecallCardRequest {
        page,
        page_size,
        origin,
        binding_scope,
        requester_open_id,
        target_lifecycle_token,
        card_issued_at_ms,
        source_note,
    } = request;
    // 卡片真正发出去的样子:绑定字段 + 来源标注。召回卡的预算按它量。
    let finish = |card: String| {
        let bound = bind_recall_card(
            card,
            &agent.name,
            binding_scope,
            requester_open_id,
            target_lifecycle_token,
            card_issued_at_ms,
        );
        match source_note {
            Some(note) => summon_core::feishu::card_with_source_note(&bound, note),
            None => bound,
        }
    };
    let Some(provider) = RecallProvider::from_cli_kind(agent.cli_kind()) else {
        return finish(
            serde_json::json!({
                "schema": "2.0",
                "body": {"elements": [{
                    "tag": "markdown",
                    "content": summon_i18n::format!(
                        "rb1043825ce",
                        agent.cli_kind()
                    )
                }]}
            })
            .to_string(),
        );
    };
    if provider == RecallProvider::Claude {
        return recall_card(
            store,
            page,
            page_size,
            origin,
            &remote_controlled_for(agent),
            &finish,
        );
    }
    // 外部 Cursor CLI 与 ACP 的 SID 命名空间不互通。目标 agent 走 `cursor-acp` 时只列
    // summon-owned ACP 会话，绝不把 `~/.cursor/chats` 的 exec SID 交给 session/load。
    let mut candidates = if agent.resume_cli_kind() == agent.cli_kind() {
        match crate::bridge::recall_headless::discover(provider) {
            Ok(candidates) => candidates
                .into_iter()
                .map(|entry| entry.candidate)
                .collect(),
            Err(error) => {
                crate::observe::log::log(&summon_i18n::format!("r6b182add48", provider.label()));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    if let Some((target_chat_id, target_chat_type)) = origin {
        if matches!(target_chat_type, "group" | "topic_group") {
            let pool = crate::capture::rpc_pool::shared(inflight);
            match crate::bridge::recall_rpc::discover(agent, target_chat_id, &pool) {
                Ok(managed) => candidates.extend(managed.into_iter().map(|entry| entry.candidate)),
                Err(error) => {
                    crate::observe::log::log(&summon_i18n::format!("r0c31c2604f", provider.label()))
                }
            }
        }
    }
    let result = paginate_candidates(candidates, page, page_size);
    let card = summon_core::feishu::card_recall(
        provider,
        &result.items,
        result.page,
        result.total,
        result.total_pages,
        now_unix_ms(),
        origin.map(
            |(chat_id, chat_type)| summon_core::feishu::CardRecallOrigin { chat_id, chat_type },
        ),
    );
    finish(card)
}

fn bind_recall_card(
    card: String,
    agent: &str,
    storage_scope: &str,
    requester_open_id: &str,
    target_lifecycle_token: &str,
    card_issued_at_ms: u64,
) -> String {
    fn visit(
        value: &mut serde_json::Value,
        agent: &str,
        storage_scope: &str,
        requester_open_id: &str,
        target_lifecycle_token: &str,
        issued_at_ms: u64,
    ) {
        match value {
            serde_json::Value::Object(object) => {
                if object
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|action| action.starts_with("recall_"))
                {
                    object.insert("bound_agent".into(), serde_json::json!(agent));
                    object.insert("bound_scope".into(), serde_json::json!(storage_scope));
                    object.insert(
                        "bound_requester".into(),
                        serde_json::json!(requester_open_id),
                    );
                    object.insert(
                        "target_lifecycle_token".into(),
                        serde_json::json!(target_lifecycle_token),
                    );
                    object.insert("card_issued_at_ms".into(), serde_json::json!(issued_at_ms));
                }
                for child in object.values_mut() {
                    visit(
                        child,
                        agent,
                        storage_scope,
                        requester_open_id,
                        target_lifecycle_token,
                        issued_at_ms,
                    );
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    visit(
                        child,
                        agent,
                        storage_scope,
                        requester_open_id,
                        target_lifecycle_token,
                        issued_at_ms,
                    );
                }
            }
            _ => {}
        }
    }

    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&card) else {
        return card;
    };
    visit(
        &mut value,
        agent,
        storage_scope,
        requester_open_id,
        target_lifecycle_token,
        card_issued_at_ms,
    );
    serde_json::to_string(&value).unwrap_or(card)
}

/// 渲染召回卡的确定性内核:活跃(注入)+ 扫盘历史 → 合并分页 → 补当前页 cwd/任务线索 →
/// `card_recall_with_remote` → `finish`(绑定字段、来源标注),返回要发出去的卡。
///
/// 把 `active` 与 `cmux_sessions` 提成参数(而非内部跑 `claude agents`、读 cmux 记录),让卡片
/// 渲染能在不依赖本机 CLI 与 cmux 的前提下单测。
#[allow(clippy::too_many_arguments)] // 每个参数是一份独立注入的事实,测试逐个替换
fn recall_card_from(
    active: &[ClaudeAgentEntry],
    projects_dir: &Path,
    page: usize,
    page_size: usize,
    now_ms: u64,
    origin: Option<(&str, &str)>,
    store: &crate::bridge::store::BridgeStore,
    cmux_sessions: &[summon_core::remote::CmuxClaudeSession],
    remote_controlled: &RemoteControlled,
    finish: &dyn Fn(String) -> String,
) -> String {
    let historical = scan_disk_sessions(projects_dir);
    let mut result = recall_page(active, &historical, page, page_size);
    fill_metadata_for_page(&mut result, projects_dir);
    // 只有原群召回才支持活跃接管。把当前页的发现时刻 exact 指纹落本机 0600 observation，
    // 卡片只带 UUID；落盘失败则该活跃项没有 capability，点击时会 fail-closed 要求刷新。
    if origin.is_some() {
        for item in &mut result.items {
            let Some(process) = active
                .iter()
                .find(|entry| entry.session_id == item.sid)
                .and_then(|entry| entry.observed_process.clone())
                .filter(|process| Some(process.pid) == item.live_pid)
            else {
                continue;
            };
            let observation = RecallSourceObservation::new(&item.sid, process, now_ms);
            match store.persist_recall_source_observation(&observation) {
                Ok(()) => item.source_observation_id = Some(observation.id),
                Err(error) => {
                    crate::observe::log::log(&summon_i18n::format!("r402fd8f40e", item.sid))
                }
            }
        }
        fill_background_tasks_for_page(&mut result, projects_dir);
    }
    // 正在遥控的会话不给接管、恢复与遥控;遥控记录读不出时,页上每条活跃会话都可能在遥控中。
    let controlled: std::collections::BTreeSet<String> = result
        .items
        .iter()
        .filter(|item| match remote_controlled {
            RemoteControlled::Known(sids) => sids.contains(&item.sid),
            RemoteControlled::Unreadable => item.is_active(),
        })
        .map(|item| item.sid.clone())
        .collect();
    // 遥控只给活跃且 cmux 记得的会话,且要有目标群;进程身份在点按钮时再逐项核对。按页内顺序。
    let mut remote: Vec<String> = if origin.is_some() {
        result
            .items
            .iter()
            .filter(|item| item.is_active() && !controlled.contains(&item.sid))
            .filter(|item| cmux_sessions.iter().any(|s| s.session_id == item.sid))
            .map(|item| item.sid.clone())
            .collect()
    } else {
        Vec::new()
    };
    let mut listed: Vec<usize> = result
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| !item.background_tasks.is_empty())
        .map(|(index, _)| index)
        .collect();
    // 遥控按钮与忙碌会话的后台任务清单都写在确认文案里,整页都加上时,发出去的卡(含绑定字段与
    // 来源标注)可能超出整卡预算,飞书会整张拒收、连接管一起发不出去。超了先从页尾起逐条去掉
    // 遥控按钮,去完还超再从页尾起逐条去掉后台任务清单(「正在忙、会打断」那句保留),最坏退回
    // 原卡。
    loop {
        let card = finish(summon_core::feishu::card_recall_with_remote(
            summon_core::recall::RecallProvider::Claude,
            &result.items,
            result.page,
            result.total,
            result.total_pages,
            now_ms,
            origin.map(
                |(chat_id, chat_type)| summon_core::feishu::CardRecallOrigin { chat_id, chat_type },
            ),
            &summon_core::feishu::RecallRemoteMarks {
                offer: remote.iter().map(|sid| sid.to_string()).collect(),
                controlled: controlled.clone(),
            },
        ));
        if card.len() <= summon_core::feishu::CARD_TOTAL_SAFE_LIMIT {
            return card;
        }
        if remote.pop().is_some() {
            continue;
        }
        match listed.pop() {
            Some(index) => result.items[index].background_tasks.clear(),
            None => return card,
        }
    }
}

/// Unix 毫秒时钟。取不到(系统时间早于 epoch)按 0 兜底,绝不 panic——卡片相对时间坏了也不该崩会话。
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 按 sid 在召回全集(活跃 + 历史)里找候选(CLI/卡片恢复前定位用)。补上 cwd。
pub fn find_candidate(projects_dir: &Path, sid: &str) -> Option<SessionCandidate> {
    let active = discover_active();
    let historical = scan_disk_sessions(projects_dir);
    let page = recall_page(&active, &historical, 1, usize::MAX);
    let mut found = page.items.into_iter().find(|c| c.sid == sid)?;
    if found.cwd.is_empty() {
        if let Some(p) = transcript_path(projects_dir, sid) {
            if let Some(cwd) = read_transcript_cwd(&p) {
                found.cwd = cwd;
            }
        }
    }
    Some(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn recall_card_binding_uses_the_original_issue_time_for_every_page_action() {
        let card = serde_json::json!({
            "body": {
                "elements": [
                    {"value": {"action": "recall_page", "page": 2}},
                    {"value": {"action": "recall_rpc_handoff", "sid": "th-1"}}
                ]
            }
        })
        .to_string();

        let bound = bind_recall_card(card, "blue", "scope-blue", "ou_owner", "lifecycle-1", 42);
        let value: serde_json::Value = serde_json::from_str(&bound).unwrap();
        for index in 0..2 {
            let action = &value["body"]["elements"][index]["value"];
            assert_eq!(action["bound_agent"], "blue");
            assert_eq!(action["bound_scope"], "scope-blue");
            assert_eq!(action["bound_requester"], "ou_owner");
            assert_eq!(action["target_lifecycle_token"], "lifecycle-1");
            assert_eq!(action["card_issued_at_ms"], 42);
        }
    }

    #[test]
    fn summon_managed_excludes_only_sessions_whose_tmux_is_live() {
        let live: std::collections::HashSet<String> =
            ["soul-starlight-gd492".to_string()].into_iter().collect();
        // summon:soul-X 且 tmux 存在 = summon 在维护 → 排除。
        assert!(is_summon_managed(
            Some("summon:soul-starlight-gd492"),
            &live
        ));
        // summon:soul-X 但 tmux 不存在 = 已放弃(用户手动 resume,如 arcadia)→ 保留可召回。
        assert!(!is_summon_managed(Some("summon:soul-arcadia-gdcc"), &live));
        // 非 summon 前缀(外部会话)→ 保留。
        assert!(!is_summon_managed(Some("demo-a7"), &live));
        assert!(!is_summon_managed(None, &live));
        // summon: 但不是 soul-(非会话标记)→ 保留(不误排)。
        assert!(!is_summon_managed(Some("summon:something"), &live));
    }

    /// 造一个假 `claude`:忽略参数,只跑给定脚本正文。约定正文用 `exec` 收尾,让直系子进程就是
    /// 最终进程——超时终止路径才能被精确断言,不存在被 reparent 的孙子进程掩盖泄漏。
    fn fake_claude(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude.sh");
        crate::test_support::write_test_executable(&script, &format!("#!/bin/sh\n{body}\n"));
        (dir, script)
    }

    fn process_is_alive(pid: i32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    #[test]
    fn hanging_claude_agents_times_out_without_leaking_child() {
        // `exec` 前后 `$$` 是同一个 pid,故脚本旁边的 pid 文件里就是我们直系子进程的 pid。`$WARMUP`
        // 分支只为下面的预热用:cargo test 并行 + 沙箱下,首次 exec 这个脚本实测要几百毫秒,不预热
        // 就会出现「子进程还没跑到 echo 就被超时杀掉」的 fixture 假象(与被测逻辑无关)。
        let (dir, script) =
            fake_claude("[ -n \"$WARMUP\" ] && exit 0\necho $$ > \"${0%/*}/pid\"\nexec sleep 60");
        let pidfile = dir.path().join("pid");
        let warmup = std::process::Command::new(&script)
            .env("WARMUP", "1")
            .output()
            .expect("预热 fixture");
        assert!(warmup.status.success());
        assert!(!pidfile.exists(), "预热分支不得写 pid");

        let started = std::time::Instant::now();
        let error = run_claude_agents_bounded(
            &script,
            std::time::Duration::from_secs(1),
            MAX_CLAUDE_AGENTS_BYTES,
        )
        .expect_err("挂死的发现命令必须超时报错,而不是把 dispatcher 线程堵住");
        let elapsed = started.elapsed();

        assert!(
            format!("{error:#}").contains("超时"),
            "错误须明示超时,便于降级日志定位:{error:#}"
        );
        // fixture 睡 60s:没有 deadline 时这一步会整整堵 60s。
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "deadline 必须生效,实际 {elapsed:?}"
        );
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("fixture 应已写下自己的 pid(否则本用例没能真正验到终止)")
            .trim()
            .parse()
            .unwrap();
        assert!(
            !process_is_alive(pid),
            "超时必须终止并回收本函数自己起的查询子进程(pid {pid}),不留孤儿/僵尸"
        );
    }

    #[test]
    fn bounded_run_parses_agents_json_from_fixture() {
        let (_dir, script) = fake_claude(
            "exec cat <<'JSON'\n\
             [{\"pid\":123,\"cwd\":\"/x\",\"startedAt\":1,\"sessionId\":\"s1\",\"name\":\"n\"}]\n\
             JSON",
        );
        let entries =
            run_claude_agents_bounded(&script, CLAUDE_AGENTS_TIMEOUT, MAX_CLAUDE_AGENTS_BYTES)
                .expect("正常输出应被解析");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, Some(123));
    }

    #[test]
    fn bounded_run_rejects_nonzero_exit_and_oversized_output() {
        // 本测验的是「退出码/字节上界」这两条判定,不是「够不够快」——超时必须给足,
        // 否则并行测试负载下 fixture 子进程 spawn 超过生产的 CLAUDE_AGENTS_TIMEOUT 就会
        // 把错误变成「超时」,断言挂掉(主门禁偶发红)。有界性另有专测,不靠本测。
        // 同时显式断言拒绝原因**不是**超时,避免超时冒充成功路径、静默丢掉这两条覆盖。
        let verdict_timeout = std::time::Duration::from_secs(20);
        let (_dir, failing) = fake_claude("exit 3");
        let error = run_claude_agents_bounded(&failing, verdict_timeout, MAX_CLAUDE_AGENTS_BYTES)
            .expect_err("非零退出不能被当成「没有活跃会话」");
        let msg = format!("{error:#}");
        assert!(
            !msg.contains("超时"),
            "必须因非零退出而拒,不能因超时: {msg}"
        );
        assert!(msg.contains("非零退出"), "{msg}");

        // 输出上界与内容无关:即使是合法 JSON,超界也必须在解析前拒绝。
        let (_dir2, chatty) =
            fake_claude("exec printf '[{\"sessionId\":\"s1\",\"cwd\":\"/x\",\"startedAt\":1}]'");
        let error = run_claude_agents_bounded(&chatty, verdict_timeout, 8)
            .expect_err("超过字节上界必须拒绝");
        let msg = format!("{error:#}");
        assert!(!msg.contains("超时"), "必须因超界而拒,不能因超时: {msg}");
        assert!(msg.contains("上界"), "{msg}");
    }

    #[test]
    fn signal_gate_needs_the_exact_sid_and_pid_listed_whatever_the_status() {
        let entry = |sid: &str, pid: u32, status: Option<&str>| ClaudeAgentEntry {
            session_id: sid.into(),
            cwd: "/x".into(),
            name: None,
            started_at: 1,
            pid: Some(pid),
            status: status.map(str::to_string),
            observed_process: None,
        };
        let active = [
            entry("busy", 41, Some("busy")),
            entry("idle", 42, Some("idle")),
            entry("legacy", 43, None),
        ];
        assert!(source_listed(&active, "busy", 41), "忙碌会话同样放行");
        assert!(source_listed(&active, "idle", 42));
        assert!(source_listed(&active, "legacy", 43));
        assert!(!source_listed(&active, "busy", 99), "PID 对不上");
        assert!(!source_listed(&active, "gone", 41), "SID 不在发现面");
    }

    #[test]
    fn parses_real_claude_agents_output() {
        let json = br#"[
          {"pid":46857,"cwd":"/x","kind":"interactive","startedAt":1784084840008,
           "sessionId":"72288607","name":"summon-8b","status":"busy"},
          {"id":"fb","cwd":"/y","kind":"background","startedAt":1783572556797,
           "sessionId":"fb7a","name":"bg","state":"blocked"}
        ]"#;
        let entries = parse_claude_agents(json);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].pid, Some(46857));
        assert_eq!(entries[1].pid, None);
    }

    #[test]
    fn bad_json_yields_empty_not_panic() {
        assert!(parse_claude_agents(b"not json").is_empty());
        assert!(parse_claude_agents(b"").is_empty());
    }

    #[test]
    fn active_termination_primitive_never_kills_scope_or_launches_resume() {
        let source = include_str!("recall.rs");
        let start = source
            .find("pub(crate) fn terminate_active_source")
            .unwrap();
        let end = source[start..]
            .find("/// 采集召回全集")
            .map(|offset| start + offset)
            .unwrap();
        let production = &source[start..end];
        for forbidden in [
            "killpg(",
            "Signal::SIGKILL",
            "ensure_started(",
            "persist_recall_resume_attempt(",
            "launch_staging(",
        ] {
            assert!(
                !production.contains(forbidden),
                "杀源前置步不得包含 {forbidden}"
            );
        }
    }

    /// 发信号原语的安全边界闭包就是「exact SID/PID 仍在严格发现面」这道现测:发现面上 PID
    /// 已换人时不得发信号。
    #[test]
    fn active_termination_checks_the_discovery_surface_inside_the_signal_primitive() {
        let source = include_str!("recall.rs");
        let start = source
            .find("pub(crate) fn terminate_active_source")
            .unwrap();
        let end = start + source[start..].find("/// 采集召回全集").unwrap();
        let squashed: String = source[start..end].split_whitespace().collect();
        assert!(squashed.contains(
            "signal_exact_source_termination(expected,&guard,||{ensure_source_listed_at_signal(sid,expected.pid)})"
        ));
        let gate = source.find("fn ensure_source_listed_at_signal(").unwrap();
        let gate_end = gate + source[gate..].find("\n}\n").unwrap();
        let gate: String = source[gate..gate_end].split_whitespace().collect();
        assert!(gate.contains("source_listed(&run_claude_agents_strict()?,sid,pid)"));
    }

    #[test]
    #[ignore = "真实进程 proof：只 SIGTERM 本测试自己启动的 /bin/sleep，不碰 Claude/tmux"]
    fn guarded_exact_signal_and_stable_transcript_smoke() {
        use crate::bridge::handoff::{
            signal_exact_source_termination, verify_stable_transcript, SourceTranscriptGuard,
        };

        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".claude");
        let project = config.join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let sid = format!("summon-stage2-smoke-{}", uuid::Uuid::new_v4());
        let transcript = project.join(format!("{sid}.jsonl"));
        std::fs::write(
            &transcript,
            format!(r#"{{"type":"assistant","sessionId":"{sid}"}}"#) + "\n",
        )
        .unwrap();

        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let process = crate::bridge::process_probe::observe_exact_fingerprint(pid)
            .unwrap()
            .unwrap();
        // 真实 Claude 的父 shell 会回收退出进程；测试也必须并行 wait，避免直系子进程停在 zombie
        // 状态被 exact probe 误看成 identity drift。
        let waiter = std::thread::spawn(move || child.wait().unwrap());
        let guard = SourceTranscriptGuard::new(&transcript, &config, &sid);
        signal_exact_source_termination(&process, &guard, || Ok(())).unwrap();
        assert!(!waiter.join().unwrap().success(), "sleep 应被 SIGTERM 结束");
        verify_stable_transcript(&transcript, &config, &sid).unwrap();
        assert!(transcript.exists(), "杀源不得删除 transcript");
    }

    #[test]
    fn reads_cwd_from_transcript_content_not_filename() {
        // cwd 必须从内容读:目录名把 disk_clean 和 disk-clean 编码成同一个,不可逆。
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sid1.jsonl");
        let mut w = std::fs::File::create(&f).unwrap();
        // 首行无 cwd,后续行有(与真实 transcript 一致)。
        writeln!(w, r#"{{"type":"summary","sessionId":"sid1"}}"#).unwrap();
        writeln!(
            w,
            r#"{{"type":"attachment","cwd":"/Users/me/disk_clean","sessionId":"sid1"}}"#
        )
        .unwrap();
        assert_eq!(
            read_transcript_cwd(&f),
            Some("/Users/me/disk_clean".to_string())
        );
    }

    #[test]
    fn cwd_absent_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.jsonl");
        std::fs::write(&f, "{\"type\":\"x\"}\n").unwrap();
        assert_eq!(read_transcript_cwd(&f), None);
    }

    #[test]
    fn scans_transcripts_as_sid_with_mtime() {
        let root = tempfile::tempdir().unwrap();
        let proj = root.path().join("-Users-me-foo");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("aaaa-1111.jsonl"), "{}\n").unwrap();
        std::fs::write(proj.join("bbbb-2222.jsonl"), "{}\n").unwrap();
        std::fs::write(proj.join("ignore.txt"), "x").unwrap(); // 非 jsonl 跳过
        let mut got = scan_disk_sessions(root.path());
        got.sort_by(|a, b| a.sid.cmp(&b.sid));
        let sids: Vec<&str> = got.iter().map(|d| d.sid.as_str()).collect();
        assert_eq!(sids, vec!["aaaa-1111", "bbbb-2222"]);
        assert!(got[0].last_active_ms > 0);
        assert!(got[0].cwd.is_empty(), "扫盘阶段 cwd 留空,延后读");
    }

    #[test]
    fn missing_projects_dir_yields_empty() {
        assert!(scan_disk_sessions(Path::new("/no/such/dir")).is_empty());
    }

    #[test]
    fn recall_card_from_renders_historical_session_deterministically() {
        // active 强制为空 → 卡片只出扫盘历史,结果不依赖本机是否装了 claude/有活跃会话。
        let root = tempfile::tempdir().unwrap();
        let proj = root.path().join("-Users-me-foo");
        std::fs::create_dir_all(&proj).unwrap();
        let mut w = std::fs::File::create(proj.join("sid-hist.jsonl")).unwrap();
        writeln!(
            w,
            r#"{{"type":"x","cwd":"/Users/me/foo","sessionId":"sid-hist"}}"#
        )
        .unwrap();
        writeln!(
            w,
            r#"{{"type":"user","isSidechain":false,"userType":"external","cwd":"/Users/me/foo","sessionId":"sid-hist","message":{{"role":"user","content":"<command-message>summon</command-message> <command-name>/summon</command-name> <command-args>starlight</command-args>"}}}}"#
        )
        .unwrap();
        writeln!(
            w,
            r#"{{"type":"user","isSidechain":false,"userType":"external","cwd":"/Users/me/foo","sessionId":"sid-hist","message":{{"role":"user","content":"修复登录页崩溃，并补充一段超过旧版四十八字限制但现在应该由飞书自然换行完整显示的背景信息。"}}}}"#
        )
        .unwrap();

        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let card = recall_card_from(
            &[],
            root.path(),
            1,
            10,
            1_000,
            None,
            &store,
            &[],
            &RemoteControlled::default(),
            &|card| card,
        );
        let value: serde_json::Value = serde_json::from_str(&card).expect("卡片是合法 JSON");
        assert_eq!(value["schema"], "2.0", "card_recall 契约:schema 2.0");
        // sid 进按钮 callback value(Task 3 据此定位候选);cwd 从 transcript 内容补齐后进正文。
        assert!(card.contains("sid-hist"), "候选 sid 出现在卡片里");
        assert!(
            card.contains("修复登录页崩溃，并补充一段超过旧版四十八字限制但现在应该由飞书自然换行完整显示的背景信息。"),
            "起始任务已从 transcript 补齐"
        );
        assert!(
            !card.contains("command-message"),
            "Claude command envelope 不得进卡"
        );
        assert!(card.contains("…/me/foo"), "cwd 已压缩为尾两级");
        // 历史候选(无 live_pid)→「恢复为托管」主按钮,而非活跃的「接管(退出源)」。
        assert!(card.contains("恢复"), "历史会话给恢复按钮");
    }

    #[test]
    fn recall_card_from_empty_yields_no_recallable_card() {
        // 空 projects 目录 + 无活跃 → 仍是合法卡片,含「没有可召回的会话」占位,不 panic。
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let card = recall_card_from(
            &[],
            root.path(),
            1,
            10,
            0,
            None,
            &store,
            &[],
            &RemoteControlled::default(),
            &|card| card,
        );
        let value: serde_json::Value = serde_json::from_str(&card).expect("空集也返回合法卡片");
        assert_eq!(value["schema"], "2.0");
        assert!(card.contains("没有可召回的会话"));
    }

    #[test]
    fn active_card_persists_discovery_fingerprint_and_exposes_only_opaque_id() {
        fn find_source_observation(value: &serde_json::Value) -> Option<&str> {
            match value {
                serde_json::Value::Object(map) => map
                    .get("source_observation")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| map.values().find_map(find_source_observation)),
                serde_json::Value::Array(values) => values.iter().find_map(find_source_observation),
                _ => None,
            }
        }

        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let process = ProcessFingerprint {
            pid: 4242,
            start_time: "Fri Jul 10 23:24:07 2026".into(),
            executable: "/private/claude/versions/2.1.211".into(),
        };
        let active = vec![ClaudeAgentEntry {
            session_id: "sid-active".into(),
            cwd: "/Users/me/project".into(),
            name: Some("active".into()),
            started_at: 100,
            pid: Some(process.pid),
            status: Some("idle".into()),
            observed_process: Some(process.clone()),
        }];
        let card = recall_card_from(
            &active,
            root.path(),
            1,
            10,
            200,
            Some(("oc_group", "group")),
            &store,
            &[],
            &RemoteControlled::default(),
            &|card| card,
        );
        assert!(!card.contains(&process.executable));
        assert!(!card.contains(&process.start_time));
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let id = find_source_observation(&value).expect("活跃接管按钮须带 observation UUID");
        let saved = store.load_recall_source_observation(id).unwrap();
        assert_eq!(saved.sid, "sid-active");
        assert_eq!(saved.process, process);
        assert_eq!(saved.created_at_ms, 200);
    }

    /// 卡里某个 action 的全部 sid,按卡面顺序。
    fn action_sids(card: &str, action: &str) -> Vec<String> {
        fn walk(value: &serde_json::Value, action: &str, out: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("action").and_then(serde_json::Value::as_str) == Some(action) {
                        out.extend(map.get("sid").and_then(|s| s.as_str()).map(String::from));
                    }
                    map.values().for_each(|child| walk(child, action, out));
                }
                serde_json::Value::Array(values) => {
                    values.iter().for_each(|child| walk(child, action, out))
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(&serde_json::from_str(card).unwrap(), action, &mut out);
        out
    }

    /// 极限页:10 条活跃 Claude 候选,起始任务都超过预览上限,会话名偏长,都带发现时刻
    /// observation。`busy_with_tasks = None` 时都空闲(比无清单的忙碌文案长);`Some(n)` 时都在忙,
    /// 各挂着 n 个说明偏长的后台命令。返回活跃列表与它们的 cmux 记录。
    fn worst_case_page(
        projects: &Path,
        busy_with_tasks: Option<usize>,
    ) -> (
        Vec<ClaudeAgentEntry>,
        Vec<summon_core::remote::CmuxClaudeSession>,
    ) {
        let project = projects.join("-Users-me-project");
        std::fs::create_dir_all(&project).unwrap();
        let prompt = "任".repeat(400);
        let mut active = Vec::new();
        let mut cmux = Vec::new();
        for index in 0..10u32 {
            let sid = format!("0000000{index}-1111-4222-8333-444444444444");
            let mut w = std::fs::File::create(project.join(format!("{sid}.jsonl"))).unwrap();
            writeln!(
                w,
                r#"{{"type":"x","cwd":"/Users/me/project","sessionId":"{sid}"}}"#
            )
            .unwrap();
            writeln!(
                w,
                r#"{{"type":"user","isSidechain":false,"userType":"external","cwd":"/Users/me/project","sessionId":"{sid}","message":{{"role":"user","content":"{prompt}"}}}}"#
            )
            .unwrap();
            for task in 0..busy_with_tasks.unwrap_or(0) {
                let description = "后".repeat(60);
                writeln!(
                    w,
                    r#"{{"type":"assistant","isSidechain":false,"message":{{"role":"assistant","content":[{{"type":"tool_use","id":"tu_{task}","name":"Bash","input":{{"command":"sleep 9999","description":"{description}","run_in_background":true}}}}]}}}}"#
                )
                .unwrap();
                writeln!(
                    w,
                    r#"{{"type":"user","isSidechain":false,"message":{{"role":"user","content":[{{"tool_use_id":"tu_{task}","type":"tool_result","content":"Command running in background with ID: b{task}."}}]}},"toolUseResult":{{"backgroundTaskId":"b{task}"}}}}"#
                )
                .unwrap();
            }
            let pid = 40_000 + index;
            active.push(ClaudeAgentEntry {
                session_id: sid.clone(),
                cwd: "/Users/me/project".into(),
                name: Some(format!("会话-{index}-{}", "长".repeat(30))),
                started_at: 100 + u64::from(index),
                pid: Some(pid),
                status: Some(
                    if busy_with_tasks.is_some() {
                        "busy"
                    } else {
                        "idle"
                    }
                    .into(),
                ),
                observed_process: Some(ProcessFingerprint {
                    pid,
                    start_time: "Fri Jul 10 23:24:07 2026".into(),
                    executable: "/private/claude/versions/2.1.289".into(),
                }),
            });
            cmux.push(summon_core::remote::CmuxClaudeSession {
                session_id: sid,
                surface_id: "S".into(),
                workspace_id: "W".into(),
                pid,
                pid_start_seconds: 1000,
                transcript_path: "/t.jsonl".into(),
                cwd: "/Users/me/project".into(),
            });
        }
        (active, cmux)
    }

    /// 与 `/bridge` 主动私聊同一条收尾:绑定字段(取偏长的真实形态)+ 偏长的来源群标注。
    fn send_path_finish(card: String) -> String {
        let bound = bind_recall_card(
            card,
            "agent-with-a-long-alias",
            "agent-with-a-long-alias-0123456789abcdef",
            "ou_0123456789abcdef0123456789abcdef",
            "00000000-0000-4000-8000-000000000001",
            1_759_000_000_000,
        );
        let note = summon_core::feishu::source_group_note(Some(&"群".repeat(60)), "oc_x");
        summon_core::feishu::card_with_source_note(&bound, &note)
    }

    #[test]
    fn recall_card_with_remote_fits_the_budget_as_actually_sent() {
        use summon_core::feishu::CARD_TOTAL_SAFE_LIMIT;
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let (active, cmux) = worst_case_page(root.path(), None);
        let origin = Some(("oc_0123456789abcdef0123456789abcdef", "group"));

        let card = recall_card_from(
            &active,
            root.path(),
            1,
            10,
            200,
            origin,
            &store,
            &cmux,
            &RemoteControlled::default(),
            &send_path_finish,
        );
        assert!(
            card.len() <= CARD_TOTAL_SAFE_LIMIT,
            "带遥控的极限页,发出去的整卡要在预算内:{} > {CARD_TOTAL_SAFE_LIMIT}",
            card.len()
        );
        assert!(card.contains("bound_requester") && card.contains("来自群"));
        let takeover = action_sids(&card, "recall_resume");
        let remote = action_sids(&card, "recall_cmux_remote");
        assert_eq!(takeover.len(), active.len(), "接管按钮一个不少");
        assert!(
            remote.len() < takeover.len(),
            "这组极限数据要走到收缩,却保留了全部 {} 个遥控按钮",
            remote.len()
        );
        assert_eq!(
            remote,
            takeover[..remote.len()],
            "从页尾起去掉遥控按钮,页首的留着"
        );

        // 没有 cmux 候选:不带遥控,只渲染一次,与原流水线(card_recall → 绑定 → 来源标注)
        // 逐字节相同。observation 每次渲染都换新 id,这一段不带它。
        let plain = recall_card_from(
            &active,
            root.path(),
            1,
            10,
            200,
            origin,
            &store,
            &[],
            &RemoteControlled::default(),
            &send_path_finish,
        );
        assert!(action_sids(&plain, "recall_cmux_remote").is_empty());
        assert!(
            plain.len() <= CARD_TOTAL_SAFE_LIMIT,
            "不带遥控的极限页:{} > {CARD_TOTAL_SAFE_LIMIT}",
            plain.len()
        );
        let unobserved: Vec<ClaudeAgentEntry> = active
            .iter()
            .cloned()
            .map(|entry| ClaudeAgentEntry {
                observed_process: None,
                ..entry
            })
            .collect();
        let mut expected_page = recall_page(&unobserved, &scan_disk_sessions(root.path()), 1, 10);
        fill_metadata_for_page(&mut expected_page, root.path());
        let expected = send_path_finish(summon_core::feishu::card_recall(
            RecallProvider::Claude,
            &expected_page.items,
            expected_page.page,
            expected_page.total,
            expected_page.total_pages,
            200,
            origin.map(
                |(chat_id, chat_type)| summon_core::feishu::CardRecallOrigin { chat_id, chat_type },
            ),
        ));
        assert_eq!(
            recall_card_from(
                &unobserved,
                root.path(),
                1,
                10,
                200,
                origin,
                &store,
                &[],
                &RemoteControlled::default(),
                &send_path_finish,
            ),
            expected
        );
    }

    #[test]
    fn recall_card_drops_remote_buttons_from_the_page_tail_first() {
        use summon_core::feishu::CARD_TOTAL_SAFE_LIMIT;
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let (active, cmux) = worst_case_page(root.path(), None);
        // 不加绑定字段与来源标注,这一页能留下一部分遥控按钮,看清去掉的是哪一头。
        let card = recall_card_from(
            &active,
            root.path(),
            1,
            10,
            200,
            Some(("oc_group", "group")),
            &store,
            &cmux,
            &RemoteControlled::default(),
            &|card| card,
        );
        assert!(card.len() <= CARD_TOTAL_SAFE_LIMIT);
        let takeover = action_sids(&card, "recall_resume");
        let remote = action_sids(&card, "recall_cmux_remote");
        assert!(
            !remote.is_empty() && remote.len() < takeover.len(),
            "这组数据要留下一部分遥控按钮:{} / {}",
            remote.len(),
            takeover.len()
        );
        assert_eq!(remote, takeover[..remote.len()], "页首的遥控按钮留着");
    }

    /// 卡上每个「接管」按钮的确认框正文,按页内顺序。
    fn takeover_confirm_texts(card: &str) -> Vec<String> {
        fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    if v["behaviors"][0]["value"]["action"] == "recall_resume" {
                        out.push(
                            v["confirm"]["text"]["content"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        );
                    }
                    for child in map.values() {
                        walk(child, out);
                    }
                }
                serde_json::Value::Array(items) => items.iter().for_each(|child| walk(child, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(&serde_json::from_str(card).unwrap(), &mut out);
        out
    }

    #[test]
    fn recall_card_drops_background_lists_from_the_page_tail_after_every_remote_button() {
        use summon_core::feishu::CARD_TOTAL_SAFE_LIMIT;
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let (active, cmux) = worst_case_page(root.path(), Some(6));
        let card = recall_card_from(
            &active,
            root.path(),
            1,
            10,
            200,
            Some(("oc_0123456789abcdef0123456789abcdef", "group")),
            &store,
            &cmux,
            &RemoteControlled::default(),
            &send_path_finish,
        );
        assert!(
            card.len() <= CARD_TOTAL_SAFE_LIMIT,
            "{} > {CARD_TOTAL_SAFE_LIMIT}",
            card.len()
        );
        assert_eq!(action_sids(&card, "recall_resume").len(), active.len());
        assert!(
            action_sids(&card, "recall_cmux_remote").is_empty(),
            "先去完遥控按钮,才轮到后台任务清单"
        );
        let texts = takeover_confirm_texts(&card);
        assert!(texts.iter().all(|text| text.starts_with("会话正忙:")));
        let listed = texts
            .iter()
            .take_while(|text| text.contains("停掉后台任务:"))
            .count();
        assert!(
            listed > 0 && listed < texts.len(),
            "这组数据要留下一部分清单:{listed} / {}",
            texts.len()
        );
        assert!(
            texts[listed..]
                .iter()
                .all(|text| !text.contains("停掉后台任务:")),
            "从页尾起去掉清单,页首的留着"
        );
        assert!(texts[0].contains(" 等 6 个"));
    }

    #[test]
    fn remote_button_goes_to_active_cmux_sessions_and_only_with_a_target_group() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let entry = |sid: &str, pid: u32| ClaudeAgentEntry {
            session_id: sid.into(),
            cwd: "/Users/me/project".into(),
            name: Some(sid.into()),
            started_at: 100,
            pid: Some(pid),
            status: Some("idle".into()),
            observed_process: None,
        };
        let active = vec![entry("sid-cmux", 4242), entry("sid-plain", 4343)];
        let cmux = vec![summon_core::remote::CmuxClaudeSession {
            session_id: "sid-cmux".into(),
            surface_id: "S".into(),
            workspace_id: "W".into(),
            pid: 4242,
            pid_start_seconds: 1000,
            transcript_path: "/t.jsonl".into(),
            cwd: "/Users/me/project".into(),
        }];
        let remote_sids = |card: &str| action_sids(card, "recall_cmux_remote");

        let card = recall_card_from(
            &active,
            root.path(),
            1,
            10,
            200,
            Some(("oc_group", "group")),
            &store,
            &cmux,
            &RemoteControlled::default(),
            &|card| card,
        );
        assert_eq!(remote_sids(&card), vec!["sid-cmux".to_string()]);

        let card = recall_card_from(
            &active,
            root.path(),
            1,
            10,
            200,
            None,
            &store,
            &cmux,
            &RemoteControlled::default(),
            &|card| card,
        );
        assert!(
            remote_sids(&card).is_empty(),
            "遥控要绑到发起 /bridge 的群,没有目标群的卡不给遥控"
        );
    }

    #[test]
    fn remote_controlled_sessions_get_no_takeover_and_unreadable_records_disable_every_active_one()
    {
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let entry = |sid: &str, pid: u32| ClaudeAgentEntry {
            session_id: sid.into(),
            cwd: "/Users/me/project".into(),
            name: Some(sid.into()),
            started_at: 100,
            pid: Some(pid),
            status: Some("idle".into()),
            observed_process: None,
        };
        let active = vec![entry("sid-remote", 4242), entry("sid-free", 4343)];
        let cmux: Vec<_> = active
            .iter()
            .map(|entry| summon_core::remote::CmuxClaudeSession {
                session_id: entry.session_id.clone(),
                surface_id: "S".into(),
                workspace_id: "W".into(),
                pid: entry.pid.unwrap(),
                pid_start_seconds: 1000,
                transcript_path: "/t.jsonl".into(),
                cwd: "/Users/me/project".into(),
            })
            .collect();
        let render = |controlled: &RemoteControlled| {
            recall_card_from(
                &active,
                root.path(),
                1,
                10,
                200,
                Some(("oc_group", "group")),
                &store,
                &cmux,
                controlled,
                &|card| card,
            )
        };

        let card = render(&RemoteControlled::Known(
            ["sid-remote".to_string()].into_iter().collect(),
        ));
        assert_eq!(action_sids(&card, "recall_resume"), vec!["sid-free"]);
        assert_eq!(action_sids(&card, "recall_cmux_remote"), vec!["sid-free"]);
        assert!(card.contains("遥控中"), "遥控中的会话换成禁用按钮");

        let card = render(&RemoteControlled::Unreadable);
        assert!(
            action_sids(&card, "recall_resume").is_empty()
                && action_sids(&card, "recall_cmux_remote").is_empty(),
            "遥控记录读不出时,页上每条活跃会话都可能在遥控中"
        );
        assert_eq!(card.matches("遥控中").count(), 2);
    }

    #[test]
    fn fills_current_page_cwd_and_prompt_from_transcript() {
        use summon_core::recall::SessionCandidate;
        let root = tempfile::tempdir().unwrap();
        let proj = root.path().join("-Users-me-foo");
        std::fs::create_dir_all(&proj).unwrap();
        // 两个历史会话,各带 cwd 内容。
        for (sid, cwd) in [("aaaa", "/Users/me/foo"), ("bbbb", "/Users/me/bar")] {
            let mut w = std::fs::File::create(proj.join(format!("{sid}.jsonl"))).unwrap();
            writeln!(w, r#"{{"type":"x","cwd":"{cwd}","sessionId":"{sid}"}}"#).unwrap();
            writeln!(
                w,
                r#"{{"type":"user","isSidechain":false,"userType":"external","cwd":"{cwd}","sessionId":"{sid}","message":{{"role":"user","content":"任务-{sid}"}}}}"#
            )
            .unwrap();
        }
        // 构造一页 cwd 空着的历史候选(模拟扫盘阶段的产物,不依赖真实 claude agents)。
        let mut page = RecallPage {
            items: vec![
                SessionCandidate {
                    provider: RecallProvider::Claude,
                    ownership: summon_core::recall::RecallOwnership::External,
                    sid: "aaaa".into(),
                    cwd: String::new(),
                    name: "a".into(),
                    prompt_preview: String::new(),
                    last_active_ms: 2,
                    live_pid: None,
                    live_status: None,
                    source_observation_id: None,
                    background_tasks: Vec::new(),
                },
                SessionCandidate {
                    provider: RecallProvider::Claude,
                    ownership: summon_core::recall::RecallOwnership::External,
                    sid: "bbbb".into(),
                    cwd: String::new(),
                    name: "b".into(),
                    prompt_preview: String::new(),
                    last_active_ms: 1,
                    live_pid: None,
                    live_status: None,
                    source_observation_id: None,
                    background_tasks: Vec::new(),
                },
            ],
            page: 1,
            total: 2,
            total_pages: 1,
        };
        fill_metadata_for_page(&mut page, root.path());
        let cwds: std::collections::HashSet<&str> =
            page.items.iter().map(|c| c.cwd.as_str()).collect();
        assert!(cwds.contains("/Users/me/foo"));
        assert!(cwds.contains("/Users/me/bar"));
        assert!(
            page.items.iter().all(|c| !c.cwd.is_empty()),
            "当前页 cwd 全部补齐"
        );
        let prompts: std::collections::HashSet<&str> = page
            .items
            .iter()
            .map(|candidate| candidate.prompt_preview.as_str())
            .collect();
        assert_eq!(
            prompts,
            std::collections::HashSet::from(["任务-aaaa", "任务-bbbb"])
        );
    }
}
