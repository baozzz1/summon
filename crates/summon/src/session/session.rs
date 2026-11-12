use crate::capture::tmux_driver::TmuxDriver;
use crate::runtime::executor::LocalExecutor;
use crate::runtime::fsio::atomic_write;
use crate::session::tmux::Tmux;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// 多行 prompt 补发 Enter 的间隔。
///
/// **要等的是 TUI 处理完一次粘贴(渲染一拍),不是 CLI 冷启。** 此前这里复用了
/// `driver.startup_grace()`(1500ms)—— 那是给「进程刚起来、还没进入可交互状态」用的量级,
/// 放在这里语义错配:每条多行消息固定多花 **3 秒**,而 steer 注入的 prompt 是多条消息
/// `join("\n")`,只要带引用/附件说明或一次注入多条就必然命中。owner 2026-08-10 实测
/// 「插入一条消息很晚才出现在 tmux 里」正是这个。
///
/// 120ms × 2 次把固定成本压到 240ms,同时保留原设计的**两次冗余**(防紧跟 Enter 被竞态吞掉;
/// 已提交时空输入上的 Enter 是 no-op,无害)。
const MULTILINE_SUBMIT_GRACE: Duration = Duration::from_millis(120);
const MULTILINE_SUBMIT_RETRIES: usize = 2;
use summon_core::config::Config;
use summon_core::driver::{cursor_input_is_clear, CliDriver, IdAcquire};
use summon_core::pane_input::{claude_startup, ClaudeStartup, InjectGate, TrustCursorMove};
use summon_core::registry::{Agent, CaptureMode};
use summon_core::scrubber::pane_proves_session_gone;
use summon_core::session_id::{codex_session_meta, parse_codex_rollout_uuid};
use summon_core::start_plan::{plan_start, StartPlan};

use crate::session::resume_store;
use summon_core::turn::{StallWatch, TurnDetector};

/// 冷启 seed(claude `/summon` bootstrap)等待的总时长封顶兜底。转录 end_turn 为正常收尾信号,
/// idle 不算失败(慢工具可跑很久);这个上限只防「进程还活着却永远收不到 end_turn」(卡死的工具)
/// 时无限阻塞冷启线程。取 10 分钟:远宽于任何正常 bootstrap,又不至无限等。
const SEED_OVERALL_TIMEOUT: Duration = Duration::from_secs(600);

/// 冷启后「等转录文件出现」的窗口。claude 起轮时就会建文件,正常 1~2s 内可见;给 30s 已很宽。
/// 超窗说明推导路径与按 SID 搜都没命中,此时**必须尽快改走抓屏看门狗**,不能拿
/// `SEED_OVERALL_TIMEOUT` 去赌 —— 那正是「blue-wing 白等 600s」的形态。
const SEED_LOCATE_WINDOW: Duration = Duration::from_secs(30);

/// 启动期 system prompt 必须按 tmux session 隔离；同一 Bot 的多个群会并发冷启，若共用
/// agent 级文件，后写群会在前一群 exec 前覆盖其 SOUL/peers 上下文。
pub(crate) fn system_prompt_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{session}.system-prompt.txt"))
}

/// summon 起的 claude 会话专属 settings:注册 tool-guard 的 PreToolUse hook、把授权请求与
/// AskUserQuestion 转飞书卡片的 PermissionRequest hook,以及压缩完成后推上下文长度卡的
/// PostCompact hook。
///
/// PermissionRequest hook 的参数写死本会话的 agent 与 tmux 会话名:hook 进程据此路由,不读自己
/// 的环境变量(会话迁移后会过期)。它会阻塞到有人在飞书上作答,所以超时给得很长 —— 主 agent 的
/// 对话框有 Claude 自己的两分钟自动拒绝,子 agent 则先等 hook 再弹框,人迟早会来答。
/// 见 `docs/spec/2026-10-16-claude-permission-hook-design.md`。
///
/// `--settings` 是**合并**语义(实测:带它的会话里全局 `~/.claude/settings.json` 的 bridge hook
/// 照常加载),所以这份只需要声明增量。
///
/// matcher **枚举受闸工具**而不是匹配全部再在 hook 里判:只读工具连 hook 进程都不启动,省掉每次
/// 工具调用的进程开销。代价是新增写类工具时要同步改这里 —— 漏了就是静默放行,所以这份清单在
/// `docs/spec/2026-08-05-feishu-identity-and-tool-guard-design.md` 与
/// `skills/feishu-agent/SKILL.md` 里各有一份副本,三处必须一起改。
///
/// PostCompact 只在 `post_compact` 为真(这台机器上的 claude 认得这个事件)时写:认不得它的
/// claude 会因为这一个事件名丢掉整份 settings,连同执行闸与授权 hook。
pub(crate) fn render_guard_settings(alias: &str, session: &str, post_compact: bool) -> String {
    // 二进制路径与 claude-quota / autostart 同源:`current_exe()`。取不到就退回 PATH 名,
    // hook 仍能跑(装机路径 ~/.local/bin/summon 在 PATH 里)。
    let binary = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "summon".to_string());
    let mut settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash|BashOutput|Write|Edit|NotebookEdit|KillShell|mcp__execution__Bash|mcp__execution__Write|mcp__execution__Edit|mcp__execution__NotebookEdit",
                "hooks": [{
                    "type": "command",
                    "command": binary,
                    "args": ["daemon", "tool-guard-hook"],
                    "timeout": 5
                }]
            }],
            "PermissionRequest": [{
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": binary,
                    "args": ["daemon", "permission-hook", "--agent", alias, "--session", session],
                    "timeout": PERMISSION_HOOK_TIMEOUT_SECS
                }]
            }]
        }
    });
    if post_compact {
        settings["hooks"]["PostCompact"] = serde_json::json!([{
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": binary,
                "args": ["daemon", "compact-hook", "--agent", alias, "--session", session],
                "timeout": COMPACT_HOOK_TIMEOUT_SECS
            }]
        }]);
    }
    settings.to_string()
}

/// PostCompact hook 的超时(秒)。hook 只往 daemon 报一声就退,卡片由 daemon 另起线程发。
const COMPACT_HOOK_TIMEOUT_SECS: u64 = 10;

/// PermissionRequest hook 的超时(秒)。到点 Claude 杀掉 hook,summon 那边的闸门随连接断开作废,
/// 子 agent 落回终端对话框。
const PERMISSION_HOOK_TIMEOUT_SECS: u64 = 3600;

/// tool-guard settings 路径(与 system prompt 同目录,按会话名区分)。
fn guard_settings_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{session}.guard-settings.json"))
}

/// 落盘会话专属 settings 并返回路径,供启动命令以 `--settings` 引用。每次起会话都重写:
/// 二进制路径可能因重装而变,内容小、幂等,不做 diff。
///
/// **自由函数而不是 `Session` 方法**:Claude 的启动参数有**两条**构造路径 —— 交互会话的
/// [`Session::augment_launch`],以及 `/bridge` 接管目标的 `HandoffTargetProfile::for_agent`。
/// 后者曾镜像前者却漏注 `--settings`,于是经 bridge 拉起的会话整条没有执行闸。两条路径共用
/// 这一个函数,漂移才有底;`bridge::handoff` 的守护测钉死「两处都调用它」。
///
/// `program` 是这次启动实际会执行的 claude,由调用方按自己的启动方式解析;只有问得出它认得
/// PostCompact 才写那个 hook,解析不出(`None`)就不写。
pub(crate) fn write_guard_settings(
    agent: &Agent,
    session: &str,
    program: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let path = guard_settings_path(agent, session);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let post_compact = program.is_some_and(crate::runtime::claude_hook::claude_knows_post_compact);
    atomic_write(
        &path,
        render_guard_settings(&agent.name, session, post_compact).as_bytes(),
    )?;
    Ok(path)
}

/// 按启动 env 起会话时 `cli_cmd` 实际会执行的程序。env 文件逐行载入、后写覆盖,所以取最后一个
/// PATH:lark-cli 隔离注入的 PATH 排在 `runtime.env` 之后,前置了 wrapper 目录。
fn launch_program_in_env(cli_cmd: &str, pairs: &[(String, String)]) -> Option<PathBuf> {
    let path = pairs
        .iter()
        .rev()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.as_str())
        .unwrap_or_default();
    crate::runtime::claude_hook::launch_program(cli_cmd, std::ffi::OsStr::new(path))
}

/// 启动目录只在真实存在时规范化。macOS 的 `/var` 实际指向 `/private/var`；若把前者交给
/// Claude，CLI 会在转录里记录后者，读取侧按原字符串编码就永远找不到同一份 jsonl。
fn canonical_start_work_dir(work_dir: String) -> String {
    std::fs::canonicalize(&work_dir)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(work_dir)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexStartupAction {
    AcceptUnsupportedTerminal,
    AcceptTrust,
    Ready,
    Wait,
}

/// Codex 交互启动前可能先出现终端兼容性确认，再出现目录信任确认；两层必须分别消费。
fn codex_startup_action(pane: &str) -> CodexStartupAction {
    if pane.contains("trust the contents of this directory") || pane.contains("Do you trust") {
        CodexStartupAction::AcceptTrust
    } else if pane.contains("OpenAI Codex") {
        // capture-pane 可能仍保留已消费的 TERM warning；出现主界面时必须以新状态为准。
        CodexStartupAction::Ready
    } else if pane.contains("TERM is set to \"dumb\"") && pane.contains("Continue anyway?") {
        CodexStartupAction::AcceptUnsupportedTerminal
    } else {
        CodexStartupAction::Wait
    }
}

/// `ensure_started` 的结果:会话此前是否已在跑、本次是冷启还是 resume 续接。
/// 调用方据此决定 PRIMER 注入:ColdStarted / Resumed 都把 system_prompt 前置进**首条消息**刷新一次
/// (冷启首次教;resume 防 PRIMER 被 compact/淡忘);AlreadyRunning(tmux 内存上下文完整)则跳过。
/// 注:不再单独成一轮 prime 等"好"回执 —— 见 daemon::handle_task 的 resume_primer。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// tmux 会话本就活着,直接复用其内存上下文(无需再注入 PRIMER)。
    AlreadyRunning,
    /// 冷启了一个全新会话(seed 后,PRIMER 前置进首条消息)。
    ColdStarted,
    /// tmux 会话已死,但用 CLI 原生 resume 续接回了上次的上下文(PRIMER 前置进首条消息、不重 seed)。
    Resumed,
}

/// 驱动某个 agent 的持久 tmux + claude 会话。
pub struct Session<'a> {
    pub agent: &'a Agent,
    pub tmux: Tmux,
    pub poll: Duration,
    pub debounce: Duration,
    /// 冷启命令前缀注入的环境变量(chat 轮给 claude tmux 注入 `SUMMON_CHAT_ID` 等)。
    /// chat 会话恒空、行为不变;非空时 `augment_launch` 把它们并进冷启的一次性 env 文件。
    run_env: Vec<(String, String)>,
    /// R6:run_turn 已消费到的转录**绝对 offset**,与会话镜像共享同一 `Arc`。run_turn 每读一块就写,
    /// mirror 的 `tail_forward_turns` 据此跳过「watermark 之下」的 turn(那是飞书触发轮、run_turn 自己
    /// 发过的)。这是 mirror 防双发的唯一机制(不再用 turn_active 布尔)。None = 非镜像会话
    /// (task/worker/未启用镜像),不参与、行为不变。
    mirror_watermark: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// 本会话对应的飞书 chat_id(per-chat 会话才有;task run/worker 为 None)。
    /// 用于 `effective_work_dir` 查 `chat_working_dirs` 映射配置。
    pub(crate) chat_id: Option<String>,
    /// 临时 run 的显式工作目录。只用于 task/worker 这类非 chat Session；chat 仍走 per-chat 配置。
    work_dir_override: Option<String>,
    /// 这条会话已经推到飞书的菜单。所有发菜单卡的路径共用,谁发出去谁记。
    pub(crate) menu_ledger: std::sync::Arc<summon_core::interactive::MenuLedger>,
}

impl<'a> Session<'a> {
    pub fn for_agent(agent: &'a Agent, poll_ms: u64, debounce_ms: u64) -> Self {
        Self::with_name(agent, agent.tmux_name(), poll_ms, debounce_ms)
    }

    /// 本会话所属 chat 的 tag(chat 线程注入的 `SUMMON_CONVERSATION_TAG`);task/worker 会话为 None。
    pub(crate) fn conversation_tag(&self) -> Option<&str> {
        self.run_env
            .iter()
            .find(|(key, _)| key == "SUMMON_CONVERSATION_TAG")
            .map(|(_, value)| value.as_str())
            .filter(|tag| !tag.is_empty())
    }

    /// 用显式 tmux 会话名建 Session(per-chat 用)。
    pub fn with_name(agent: &'a Agent, tmux_name: String, poll_ms: u64, debounce_ms: u64) -> Self {
        Session {
            agent,
            tmux: Tmux::new(tmux_name),
            poll: Duration::from_millis(poll_ms),
            debounce: Duration::from_millis(debounce_ms),
            run_env: Vec::new(),
            mirror_watermark: None,
            chat_id: None,
            work_dir_override: None,
            menu_ledger: Default::default(),
        }
    }

    /// 用 dispatcher 手里同一份菜单账本:卡片上的单选在 dispatcher 线程注入,答过的菜单要让 chat
    /// 线程的轮外守望知道。
    pub(crate) fn with_menu_ledger(
        mut self,
        ledger: std::sync::Arc<summon_core::interactive::MenuLedger>,
    ) -> Self {
        self.menu_ledger = ledger;
        self
    }

    /// 绑定 chat_id(per-chat 会话用,供 chat_working_dirs 映射查工作目录)。
    pub fn with_chat_id(mut self, chat_id: &str) -> Self {
        self.chat_id = Some(chat_id.to_string());
        self
    }

    /// 临时会话显式继承父任务工作目录，避免 Transcript worker 回落到 daemon cwd。
    pub fn with_work_dir(mut self, work_dir: Option<&str>) -> Self {
        self.work_dir_override = work_dir.map(str::to_string);
        self
    }

    /// 本会话显式指定的工作目录(task/worker 继承、bridge 恢复)。转录定位侧用它对齐
    /// `effective_start_work_dir` 的第一优先级,避免显式启动的会话首轮转录定位错位。
    pub(crate) fn work_dir_override(&self) -> Option<&str> {
        self.work_dir_override.as_deref()
    }

    /// 本次起会话真正用的工作目录。**必须走 `start_work_dir`(带存在性校验)**:
    /// `tmux new-session -c <不存在的目录>` 会静默成功并把 pane 落到 `$HOME`(实测退出码 0),
    /// 陈旧的 per-chat `.cwd` 就这样让 CLI 在整个家目录里跑。配了目录却一个都不存在时宁可
    /// 起不来 —— 调用方 `ensure_started` 的 Err 会走 chat 线程既有的「起会话失败」回执路径。
    pub(crate) fn effective_start_work_dir(&self) -> anyhow::Result<String> {
        let configured = crate::runtime::daemon::start_work_dir(
            self.agent,
            &self.tmux.session,
            self.chat_id.as_deref(),
            self.work_dir_override.as_deref(),
        )?;
        let configured = match configured {
            None if crate::remote_tools::integration::binding(self.agent).is_some() => {
                let path = self
                    .agent
                    .state_dir()
                    .join("runtime/workspaces")
                    .join(&self.tmux.session);
                std::fs::create_dir_all(&path)?;
                Some(path.to_string_lossy().into_owned())
            }
            value => value,
        };
        let work_dir = configured.unwrap_or_else(|| {
            // 无配置工作目录:兜底用本进程**绝对** cwd,与 tmux `-c "."` 解析成 client cwd 的语义
            // 一致。绝不用字面 "."——读取侧 project_dirname(".")="-" 会 munge 出垃圾转录路径。
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".to_string())
        });
        Ok(canonical_start_work_dir(work_dir))
    }

    /// 往会话注入一行命令(如 `/compact`),不抓取/不等待输出。
    ///
    /// Claude 的斜杠命令有两个坑:① 打完会弹自动补全菜单,首个 Enter 可能只是「选中」而没执行;
    /// ② 紧跟字面文本的 Enter 偶被 TUI 竞态吞掉(与 Cursor 同病)。故打字提交后再补发两次 Enter
    /// (各隔一个 grace)兜底 —— 空输入 / 运行中的 Enter 都是 no-op,多按无害。
    ///
    /// 闸恒取 `IdleOnly`:斜杠命令在运行中注入不会被当命令执行,只会作为一段 prompt 文本排进
    /// claude 的队列(`/compact` 变成聊天内容),那是静默的语义损坏,比拒绝糟得多。
    pub fn inject_command(&self, cmd: &str) -> anyhow::Result<()> {
        self.send_line_for_driver(cmd, InjectGate::IdleOnly)?;
        let driver = self.driver();
        for _ in 0..2 {
            sleep(driver.startup_grace());
            self.tmux.send_enter()?;
        }
        Ok(())
    }

    /// 按 CliDriver 的唯一策略打断交互式当前轮；无头打断走 ChildKiller，不到这里。
    pub fn interrupt_turn(&self) -> std::io::Result<()> {
        self.tmux.send_key(self.driver().tmux_interrupt_key())
    }

    /// 注入 `/compact` 并**阻塞到压缩真正结束**。供卡片 compact 动作用:先贴 working 表情、
    /// 再调本方法、完成后才发「已压缩」——否则压缩还在跑就回执已完成,误导用户。
    ///
    /// **不能复用 `wait_turn`**:它要求出现「与上一轮不同的新答案」(`answer != prev`),而 `/compact`
    /// 不产出 `⏺` 助手答案,该条件永不满足、只能死等到 stall 误判后以错误退出。这里改成只看
    /// 「新的完成标记(`✱ Baked for Ns` 之类,`is_turn_ready` 据 done-marker 计数增长判定)+ 稳定
    /// debounce」,不比对答案内容。再加**硬超时**:无论 pane 是否还在动都绝不无限等(防会动的状态行
    /// 让 stall 永不触发)。仅能抓屏判 turn 的 CLI 有意义;不能抓屏的由调用方降级(注入即返回)。
    pub fn compact_and_wait(&self, command: &str) -> anyhow::Result<()> {
        // compact 是**慢命令**(实测 2.5 分钟量级),必须用宽窗口。飞书入口只开放这一条
        // driver 声明过完成语义的命令；`command_and_wait` 仍是 Session 内部通用原语。
        //
        // cap 从 300s 放宽到 600s:判据坏掉那阵(见 `is_command_done`)cap 是唯一出路,压得紧只是
        // 让误报来得早些;判据修好后它回归「硬顶兜底」本职,而满上下文的压缩越过 5 分钟并不稀奇。
        // stall 维持 240s —— 真机确认压缩期间 pane 一直在刷计时,静止 4 分钟确属异常。
        self.command_and_wait(command, Duration::from_secs(240), Duration::from_secs(600))
    }

    /// 输入行上那段字是不是 claude 自己画的(而不是用户的草稿)。
    ///
    /// 抓一次带 SGR 的屏来判(见 `pane_input::prompt_text_is_dim`)。**先看纯文本判据再调**:
    /// 只有纯文本判成 `Draft` 的那一拍才值得多付一次 fork-exec。抓不到样式 / 判不出 →
    /// false,回落原判据(fail-closed:宁可多等到硬顶,不可把用户的草稿当空输入框)。
    fn input_holds_only_ghost(&self, pane: &str) -> bool {
        use summon_core::pane_input::{classify_pane, prompt_text_is_dim, PaneState};
        if classify_pane(pane) != PaneState::Draft {
            return false;
        }
        self.tmux
            .capture_styled()
            .is_ok_and(|styled| prompt_text_is_dim(&styled))
    }

    /// 注入一条 CLI 命令并等它跑完(抓屏判 turn 边界)。`stall` = 画面静止多久算收尾,
    /// `cap` = 总时长硬顶。**命令一律原样注入,不附任何 transport marker** —— marker 会被
    /// claude 解析进 `<command-args>`(实测 `/compact <!-- summon-transport-turn:… -->`),
    /// 污染命令语义。命令与本轮的关联因此不走转录,只走这里的抓屏。
    ///
    /// **注入前先过闸**(经 `inject_command` 的 `IdleOnly`):`/cost` 面板吃掉紧跟的 `/compact`
    /// 是实测事故。闸拒绝时本函数 Err 且**一个键都没发出去**,调用方据 `refusal_notice_of` 把
    /// 「没有执行」与「等待超时」两种收尾分开外显 —— 它们对用户的下一步动作要求完全不同。
    pub fn command_and_wait(
        &self,
        command: &str,
        stall_window: Duration,
        cap: Duration,
    ) -> anyhow::Result<()> {
        let driver = self.driver();
        let scd = self.tmux_driver();
        let pre = self.tmux.capture()?;
        let prev_done = scd.done_count(&pre);
        self.inject_command(command)?;
        sleep(driver.startup_grace());
        let mut det = TurnDetector::new(self.debounce);
        let mut stall = StallWatch::new(Instant::now(), stall_window);
        let mut prev_pane = self.tmux.capture()?;
        let deadline = Instant::now() + cap;
        loop {
            let pane = self.tmux.capture()?;
            let changed = pane != prev_pane;
            stall.observe(changed, Instant::now());
            if changed {
                prev_pane = pane.clone();
            }
            // ready = 完成标记增长(有答案的命令)**或**回到空闲提示符(`/compact` 这类不打完成
            // 标记的命令)。只用前者时对 compact 恒 false,只能等到超时——见 `is_command_done`。
            // snapshot 仅作稳定锚点,不要求其变化。
            //
            // 第三种可能:输入行**空着但画着字** —— claude 答完一轮会在里面画一句建议下一句
            // (ghost text),纯文本抓屏里它与用户草稿逐字节同形,于是「回到空闲提示符」这条
            // 判不出来,慢命令只能白等到硬顶、把成功的压缩报成「可能未正常完成」(真机日志
            // 2026-09-07 17:28、09-08 00:05 两次)。样式分得出(见 `prompt_text_is_dim`),
            // 但**只在纯文本已经判成 Draft 时才多抓一次** —— 每拍都抓等于把 fork-exec 翻倍。
            let ready = scd.is_command_done(&pane, prev_done) || self.input_holds_only_ghost(&pane);
            if det.observe(&scd.snapshot(&pane), Instant::now(), ready) {
                return Ok(());
            }
            let now = Instant::now();
            if stall.stalled(now) || now >= deadline {
                anyhow::bail!(summon_i18n::format!("r3af5ffdb4d"));
            }
            sleep(self.poll);
        }
    }

    pub(crate) fn driver(&self) -> CliDriver {
        CliDriver::from_kind(self.agent.cli_kind())
    }

    /// 本会话已落盘的 CLI 原生 session id(无 / cli kind 不符则 None)。
    /// Transcript 捕获源据它钉死要 tail 的 jsonl(`~/.claude/projects/<dir>/<sid>.jsonl`)。
    pub(crate) fn native_session_id(&self) -> Option<String> {
        resume_store::read_id(self.agent, &self.tmux.session, self.agent.cli_kind())
    }

    /// tmux turn-detection 原语的入口(per-CLI 分派收口在 `capture::tmux_driver::TmuxDriver`)。
    /// claude 在 Transcript 模式仍跑在 tmux,冷启 seed / 启动就绪 / `/compact` 等待靠它判 turn 边界。
    pub(crate) fn tmux_driver(&self) -> TmuxDriver {
        TmuxDriver::for_cli(self.driver())
    }

    /// 该 agent 的 CLI 是否在**启动期**用 flag 注入 system prompt(Claude 走
    /// `--append-system-prompt-file`,每轮自动加)。true → daemon 不必再把 PRIMER 前置进首条消息。
    pub fn injects_system_prompt_at_launch(&self) -> bool {
        self.driver().system_prompt_file_flag().is_some()
    }

    /// Claude `--append-system-prompt-file` 读的文件路径(每 session 一份,避免多群覆盖)。
    fn system_prompt_path(&self) -> PathBuf {
        system_prompt_path(self.agent, &self.tmux.session)
    }

    /// 无头续接时供 `--append-system-prompt-file` 引用的 PRIMER 文件路径(落盘了才返回)。
    /// 复用上面的内部路径,不存在(尚未 write_system_prompt_file)则 None,让 runner 冷起不带该 flag。
    pub fn system_prompt_file(&self) -> Option<String> {
        let p = self.system_prompt_path();
        if p.is_file() {
            Some(p.to_string_lossy().into_owned())
        } else {
            None
        }
    }

    /// PRIMER 文件的文本内容(cursor 无头首轮前置用);读不到(文件不存在或 IO 失败)返回 None。
    /// 与 `system_prompt_file` 复用同一路径,不重复定义路径逻辑。
    pub fn system_prompt_text(&self) -> Option<String> {
        let p = self.system_prompt_path();
        std::fs::read_to_string(&p).ok()
    }

    /// 把合成好的 system prompt 落盘,供下次/本次冷启或 resume 的启动命令以
    /// `--append-system-prompt-file` 引用。仅对启动期注入的 CLI(Claude)有意义;daemon 据
    /// `injects_system_prompt_at_launch` 决定是否调用。失败向上抛(调用方按需降级为忽略)。
    pub fn write_system_prompt_file(&self, content: &str) -> anyhow::Result<()> {
        let path = self.system_prompt_path();
        let mut content = content.to_string();
        if let Some(binding) = crate::remote_tools::integration::binding(self.agent) {
            content.push_str(&crate::remote_tools::integration::instructions(binding));
        }
        atomic_write(&path, content.as_bytes())
    }

    /// 给启动/续接命令追加 per-agent / per-driver 的额外 flag:
    /// - **model/effort**:经 `resolve_launch_lenient` 落成各 CLI 片段(claude `--model`/`--effort`、
    ///   cursor `--model`);坏 effort 降级丢掉(不 panic,记日志)。claude 在 Transcript 模式跑 tmux,
    ///   故 model/effort 在这条路径生效;cursor 通常走 headless,但保留以兼容 tmux 跑法。
    /// - **system prompt**:Claude `--append-system-prompt-file <文件>`(文件须已落盘,见上)。
    ///
    /// 值用双引号包裹即可:model/effort/flag 都是简单 token、文件是受控绝对路径(无 `"`/`$`/空格),
    /// 不会被 shell 拆。
    ///
    /// 环境**不进命令行**:见下方 env 文件一段。写文件可能失败,故返回 `Result`——失败必须让整次
    /// 起会话失败(fail-closed),绝不能退回「把凭据内联进命令」的老路。
    fn augment_launch(&self, driver: CliDriver, mut cmd: String) -> anyhow::Result<String> {
        // 启动 env 先组装:下面判 PostCompact 要按它里面最终生效的 PATH 找 claude。
        let pairs = self.launch_env_pairs();
        for arg in crate::remote_tools::integration::launch_args(self.agent, &self.tmux.session)? {
            cmd.push(' ');
            cmd.push_str(&summon_core::launch_env::sh_single_quote(
                &arg.to_string_lossy(),
            ));
        }
        // 本群生效值:任一层有值就显式带参数。claude 会话内的 `/model`、`/effort` 会改写用户级默认,
        // 不带参数起的会话会被它带动(见按群模型设计文档的真机核实)。
        let effective = crate::runtime::chat_model::effective(self.agent, self.conversation_tag());
        let (model_args, dropped) =
            driver.resolve_launch_lenient(effective.model(), effective.effort());
        if dropped {
            crate::observe::log::log(&summon_i18n::format!(
                "r9235267421",
                self.agent.name,
                effective.effort().unwrap_or(""),
                self.agent.cli_kind()
            ));
        }
        for arg in &model_args {
            cmd = format!("{cmd} \"{arg}\"");
        }
        // 交互路权限 flag(review #4):claude 加 `--permission-mode bypassPermissions`,与无头一致,
        // 免得工具权限弹窗在 tmux 里卡住整轮(尤其 task run 无人点卡、会空等到上限)。
        for arg in driver.interactive_permission_args() {
            cmd = format!("{cmd} \"{arg}\"");
        }
        // Claude session 命名:让 summon 起的会话在 ~/.claude/sessions/ 里可精确识别,
        // 方便 subagent watcher 等组件按名关联。格式 `summon:<tmux_session>` 天然唯一。
        if matches!(driver, CliDriver::Claude) {
            let name = format!("summon:{}", self.tmux.session);
            cmd = format!("{cmd} \"--name\" \"{name}\"");
            // 执行闸只对 claude 生效:cursor/codex 走无头子进程,没有 PreToolUse hook 机制,
            // 这层边界覆盖不到它们(已知缺口,记在 spec)。
            //
            // 写失败只跳过注入,不 fail-closed:起不了会话比少一层闸更糟,而 owner 路径本来就放行。
            let program = launch_program_in_env(&self.agent.cli_cmd(), &pairs);
            match write_guard_settings(self.agent, &self.tmux.session, program.as_deref()) {
                Ok(path) => cmd = format!("{cmd} \"--settings\" \"{}\"", path.display()),
                Err(error) => {
                    crate::observe::log::log(&summon_i18n::format!("r7deb54b44f", self.agent.name))
                }
            }
        }
        if let Some(flag) = driver.system_prompt_file_flag() {
            let path = self.system_prompt_path();
            if path.is_file() {
                cmd = format!("{cmd} {flag} \"{}\"", path.display());
            }
        }
        let env_file = self.write_launch_env_file(&pairs)?;
        cmd =
            summon_core::launch_env::wrap_command_with_env_file(&env_file.to_string_lossy(), &cmd);
        Ok(cmd)
    }

    /// 冷启 claude 的启动 env,按写进 env 文件的顺序排列(pane 里的 sh 逐行载入,后写覆盖)。
    ///
    /// 飞书身份隔离 + task run 注入。干净启动:`env -i` 清空全部继承环境,只显式注入:
    ///   ① 系统基础变量(PATH/HOME/TERM 等 CLI 运行必需)
    ///   ② 已过滤内部命名空间的普通 runtime.env
    ///   ③ 本 agent 自己的飞书凭据(规范名 + alias 名)与 lark-cli 隔离(前置 wrapper 目录的 PATH)
    /// 这样 ANTHROPIC_* 等 daemon 环境里的变量不会泄漏到 tmux pane——agent 需要的在自己
    /// agent.yaml 的 runtime.env 里显式配(经 with_run_env 注入到 ②)。
    ///
    /// **键值对绝不内联进命令行**:tmux `new-session <cmd>` 的整条命令行会成为 tmux server
    /// (ppid=1 的常驻进程)的 argv,老写法 `env -i K="V" … claude` 等于把 app_secret 永久摊在
    /// `ps` 里,同 uid 的任何兄弟 agent 都读得到 —— `env -i` 的身份隔离被这条旁路整个绕开。
    /// 现在写进一份 0600 的一次性 env 文件,命令行只留路径;pane 里的 sh 载入后立刻删文件再 exec。
    /// 详见 `summon_core::launch_env`。
    fn launch_env_pairs(&self) -> Vec<(String, String)> {
        let essentials = crate::runtime::daemon::essential_system_env();
        let (_, inject) = crate::runtime::daemon::agent_feishu_isolation_tmux(self.agent);
        let mut pairs: Vec<(String, String)> = essentials
            .into_iter()
            .chain(self.run_env.iter().cloned())
            .chain(inject)
            .collect();
        // summon-owned Claude 仍会命中用户级 Hook；保留 marker 必须最后赋值，禁止 agent/run env
        // 用同名键覆盖成 0 后把内部 tmux 冒充外部会话(env 文件按行顺序后写覆盖,语义同前缀)。
        pairs.push(("SUMMON_MANAGED_SESSION".to_string(), "1".to_string()));
        if crate::session::resume_store::takeover_marker_path(self.agent, &self.tmux.session)
            .is_file()
        {
            pairs.push(("SUMMON_MANAGED_TAKEOVER".to_string(), "1".to_string()));
        }
        pairs
    }

    /// 本会话冷启用的一次性 env 文件路径(每 session 一份,与 system-prompt 同目录同命名法)。
    fn launch_env_path(&self) -> PathBuf {
        self.agent
            .state_dir()
            .join("runtime")
            .join("chats")
            .join(format!("{}.launch-env", self.tmux.session))
    }

    /// 落一次性启动 env 文件(0600),返回其路径。非法键名(手写坏了的 `runtime.env`)只跳过 + 记日志:
    /// 让它连累整个会话起不来代价更大,而漏一个变量的后果只是那一项没生效,可诊断。
    fn write_launch_env_file(&self, pairs: &[(String, String)]) -> anyhow::Result<PathBuf> {
        let (body, skipped) = summon_core::launch_env::render_launch_env_file(pairs);
        if !skipped.is_empty() {
            crate::observe::log::log(&summon_i18n::format!("rf7456cb95f", self.agent.name));
        }
        let path = self.launch_env_path();
        crate::runtime::fsio::atomic_write_mode(&path, body.as_bytes(), 0o600)?;
        Ok(path)
    }

    pub(crate) fn program_status_path(&self) -> PathBuf {
        self.agent
            .state_dir()
            .join("runtime")
            .join("chats")
            .join(format!("{}.program-status.json", self.tmux.session))
    }

    /// 起 tmux 会话,并在**起失败时**把一次性 env 文件收掉。正常路径由 pane 里的加载器自删;
    /// tmux 根本没起来时没人替我们删,凭据会一直躺在盘上直到下次冷启覆盖 —— 必须在这里兜住。
    fn start_tmux(&self, wd: &str, cmd: &str) -> anyhow::Result<()> {
        let wrapped = if self.driver() == CliDriver::Claude {
            match crate::capture::program_status::wrap_launch(cmd, &self.program_status_path()) {
                Ok(wrapped) => Some(wrapped),
                Err(error) => {
                    crate::observe::log::log(&format!(
                        "program status observer unavailable: {error}"
                    ));
                    None
                }
            }
        } else {
            None
        };
        match self.tmux.new_session(wd, wrapped.as_deref().unwrap_or(cmd)) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(self.launch_env_path());
                Err(e.into())
            }
        }
    }

    /// 给本会话挂上 task run 的环境注入(并进冷启的一次性 env 文件)。链式构造,chat 路径不调用。
    pub fn with_run_env(mut self, env: Vec<(String, String)>) -> Self {
        self.run_env = env;
        self
    }

    /// R6:注入与会话镜像共享的 watermark(run_turn 写已消费 offset,mirror 读它跳过本轮)。
    pub fn with_mirror_watermark(
        mut self,
        wm: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        self.mirror_watermark = Some(wm);
        self
    }

    /// 本会话与镜像共享的 watermark(`run_turn` 写、`mirror_tail` 读)。非镜像会话为 None。
    pub(crate) fn mirror_watermark(&self) -> Option<&std::sync::atomic::AtomicU64> {
        self.mirror_watermark.as_deref()
    }

    /// 往 pane 投喂一行文本。**`gate` 没有默认值是有意的**:注入前的输入态证明收口在这里,
    /// 新增注入口必须在编译期显式表态用哪道闸(见 `input_gate` 与 spec 2026-08-14 第二节)。
    pub(crate) fn send_line_for_driver(&self, text: &str, gate: InjectGate) -> anyhow::Result<()> {
        self.send_line_for_driver_inner(text, true, gate, true)
    }

    /// 只提交一次，不补发回车：命令之后可能弹对话框，补发的回车会落在对话框上、替人选掉默认项。
    /// 回车被吞、命令停在输入框里的情形由调用方看着屏补按。
    pub(crate) fn send_command_once(&self, text: &str, gate: InjectGate) -> anyhow::Result<()> {
        self.send_line_for_driver_inner(text, true, gate, false)
    }

    fn send_line_for_driver_inner(
        &self,
        text: &str,
        clear_first: bool,
        gate: InjectGate,
        resubmit: bool,
    ) -> anyhow::Result<()> {
        let driver = self.driver();
        // 注入闸只对 claude 生效:判据(空 `❯` 输入行、面板页脚、编号菜单光标)全部是 claude TUI
        // 的真机特征,别的 CLI 一个都不命中 —— 套上去等于把没坏的路径 fail-closed 打死。
        // Cursor/Codex 的采样与分类是 spec 第七节的 non-goal,等它们各自有证据再开。
        if matches!(driver, CliDriver::Claude) {
            crate::session::input_gate::prove_injectable(&self.tmux, gate, Default::default())?;
        }
        // copy-mode(滚屏/鼠标选择)会吃掉所有 send-keys,prompt 注入静默落空、本轮空等超时;
        // 发任何键前先无脑退出一次(幂等)。详见 Tmux::exit_copy_mode。
        self.tmux.exit_copy_mode();
        if clear_first && driver.clear_input_before_send() {
            self.tmux.clear_input()?;
        }
        match driver {
            CliDriver::Cursor => self.send_cursor_input(text)?,
            _ => {
                if driver.injects_by_paste() {
                    self.tmux.paste_text(text)?;
                } else {
                    self.tmux.send_text(text)?;
                }
                self.tmux.send_enter()?;
                // 一大块正文(粘贴、或多行的「引用上下文」回复)进 TUI 后需一拍才 settle,紧跟的
                // 那个单 Enter 偶被竞态吞掉 → 内容停在输入框、**不自动发送**(owner 报的引用回复
                // 不发送 bug)。这两种形态都补发 graced Enter 兜底提交(若已提交,空输入上的 Enter
                // 是 no-op、无害)。同 `inject_command` 已知的「紧跟 Enter 被吞」竞态。
                if resubmit && (driver.injects_by_paste() || text.contains('\n')) {
                    for _ in 0..MULTILINE_SUBMIT_RETRIES {
                        sleep(MULTILINE_SUBMIT_GRACE);
                        self.tmux.send_enter()?;
                    }
                }
            }
        }
        Ok(())
    }

    fn send_cursor_input(&self, text: &str) -> anyhow::Result<()> {
        // Cursor Agent folds dense multiline pastes into a placeholder. Type
        // line-by-line and use C-j for soft newlines, following botmux's adapter.
        let throttle = Duration::from_millis(30);
        for (idx, line) in text.split('\n').enumerate() {
            if idx > 0 {
                self.tmux.send_key("C-j")?;
                sleep(throttle);
            }
            if !line.is_empty() {
                self.tmux.send_text(line)?;
                sleep(throttle);
            }
        }
        // 提交:Cursor 偶尔在刚打进去的字面文本还没消化完时**吞掉 Enter**,把输入停在框里没发出去
        // (现象:输入框显示「→ <文本>」/「enter send now」却不走)。不靠固定延时,改成
        // 「按 Enter → 抓屏确认输入框已清空 → 没清就再按」,最多几次。空框 Enter 是 no-op,多按无害。
        for _ in 0..8 {
            sleep(Duration::from_millis(200));
            self.tmux.send_enter()?;
            sleep(Duration::from_millis(250));
            if self
                .tmux
                .capture_visible()
                .map(|p| cursor_input_is_clear(&p))
                .unwrap_or(false)
            {
                return Ok(());
            }
        }
        anyhow::bail!(summon_i18n::format!("r756ccf183f"))
    }

    /// 惰性起会话。优先级:已在跑 → 直接复用;启用 resume 且存有 id → 用 CLI 原生 resume 续接
    /// (起不来则杀残留 + 清坏记录,兜底冷启);否则冷启。冷启时按各 CLI 形态取得可日后续接的
    /// session id 落盘 `.session`,供下次 tmux 会话死后 resume。
    pub fn ensure_started(&self) -> anyhow::Result<Start> {
        self.ensure_started_inner(false)
    }

    /// 用户明确恢复指定 SID 时，失败须保留目标并报错。
    pub(crate) fn ensure_resumed(&self) -> anyhow::Result<Start> {
        self.ensure_started_inner(true)
    }

    fn ensure_started_inner(&self, explicit_resume: bool) -> anyhow::Result<Start> {
        // Transcript/TUI 从不消费 RPC handoff 的 strict owner。即使 CLI 名相同（Codex），把
        // app-server SID 喂给交互 CLI、失败后清记录并 cold start，都会越过 RPC→RPC 的唯一
        // owner 边界；Cursor 虽通道名不同，也不能把 strict 记录当成“无历史”后冷启。
        if let Some(record) = resume_store::read_record_checked(self.agent, &self.tmux.session)? {
            anyhow::ensure!(!record.strict_resume, summon_i18n::format!("r8698173978"));
        }
        let driver = self.driver();
        // ① 探测:strict 安全闸之后，tmux 是否活着是唯一无条件探测。会话活着时后面几项一概
        // 不做——ensure_started 在 chat 热路径上每条消息都过一次，不该再为一个已经确定的结论
        // 白付 Config::load / cwd 解析(canonicalize)的代价。
        let has_session = self.tmux.has_session();
        if crate::remote_tools::integration::binding(self.agent).is_some()
            || crate::remote_tools::integration::binding_path(self.agent, &self.tmux.session)
                .exists()
        {
            crate::remote_tools::integration::check_binding(
                self.agent,
                &self.tmux.session,
                has_session,
                resume_store::read_record_checked(self.agent, &self.tmux.session)?.is_some(),
            )?;
        }
        let (resume_enabled, stored_id) = if has_session {
            (false, None)
        } else {
            let enabled = driver.supports_resume()
                && (explicit_resume || self.agent.resume_enabled(Config::load().resume_sessions));
            let id = enabled
                .then(|| {
                    resume_store::read_id(self.agent, &self.tmux.session, self.agent.cli_kind())
                })
                .flatten();
            (enabled, id)
        };
        // 待 fork 的源 sid **与它要落到的新 sid**。**只在会话不活着时读**:tmux 活着说明第一轮
        // 已经开始或跑完,此时按 fork 重起会把内存里的上下文顶掉。
        //
        // 两个 sid 都从**同一条 record** 取,不复用上面那个 `stored_id` —— 那个变量被两道过滤
        // 缩窄过:`resume_enabled` 关掉时它恒为 None,换过 cli 时 `read_id` 的 kind guard 也会
        // 把它滤掉。拿它当 fork 的目标,fork 记录明明完好却因为「没有 new_sid」落进冷启 ——
        // 而两个群刚各发过一条 `/fork`、目标群已经收到「已接上上下文副本」的回执。
        // fork 是显式命令,它的执行不该被 resume 开关或 cli 变更悄悄取消。
        let fork_plan = if has_session {
            None
        } else {
            resume_store::read_record(self.agent, &self.tmux.session)
                .filter(|rec| rec.fork_pending)
                .and_then(|rec| {
                    rec.forked_from
                        .as_ref()
                        .map(|origin| (origin.sid.clone(), rec.id.clone()))
                })
        };
        let fork_from = fork_plan.as_ref().map(|(source, _)| source.clone());
        // ② 探测 resume:只能「先按 driver 语法起、再看起没起来」,故起命令属于探测而非执行。
        // per-chat cwd 覆盖优先(`/cd`/面板设的),没有才回退 agent 级 working_dir;claude tmux
        // 冷启同样认本会话工作目录,与 headless 一致。算出来的 wd 冷启兜底时原样复用。
        //
        // 待 fork 时**跳过这一步**:记录里那个 sid 在 provider 侧还不存在,起 resume 只会失败,
        // 白付一次 tmux 启动 + 探活等待。
        let mut launched_wd: Option<String> = None;
        let resume_ok = match stored_id.as_deref() {
            Some(id) if !has_session && resume_enabled && fork_from.is_none() => {
                let wd = self.effective_start_work_dir()?;
                let cmd =
                    self.augment_launch(driver, driver.resume_cmd(&self.agent.cli_cmd(), id))?;
                self.start_tmux(&wd, &cmd)?;
                let came_up = self.resume_came_up(driver);
                launched_wd = Some(wd);
                came_up
            }
            _ => false,
        };
        // ③ 执行。判定本身在 summon-core 的 `plan_start` 里,由非 ignored 单测钉死全部入参组合
        // (此前只有 4 个真机 `#[ignore]` 覆盖,判定取反默认门禁全绿)。
        match plan_start(
            has_session,
            resume_enabled,
            stored_id.as_deref(),
            resume_ok,
            fork_plan
                .as_ref()
                .map(|(source, new)| (source.as_str(), new.as_str())),
        ) {
            StartPlan::AlreadyRunning => Ok(Start::AlreadyRunning),
            StartPlan::ForkFrom {
                source_sid,
                new_sid,
            } => self.fork_start(driver, source_sid, new_sid, launched_wd),
            StartPlan::Resume(_) => {
                // 捕获侧用本次启动目录验证转录候选路径。
                let wd = match launched_wd {
                    Some(wd) => wd,
                    None => self.effective_start_work_dir()?,
                };
                resume_store::update_cwd(self.agent, &self.tmux.session, &wd);
                Ok(Start::Resumed)
            }
            StartPlan::KillStaleThenCold if explicit_resume => {
                let _ = self.tmux.kill_session();
                anyhow::bail!(summon_i18n::format!("rae8cf30c4a"))
            }
            StartPlan::Cold if explicit_resume => {
                anyhow::bail!(summon_i18n::format!("r807b10467c"))
            }
            StartPlan::KillStaleThenCold => {
                // stale id(CLI 历史被外部删等):杀残留 + 清坏记录,落到冷启。杀的是上一步刚起、
                // 已确认没续接成功的残壳,不是用户的活会话(has_session 为真时根本走不到这里)。
                let _ = self.tmux.kill_session();
                resume_store::clear(self.agent, &self.tmux.session);
                self.cold_start_and_record(driver, launched_wd, resume_enabled)
            }
            StartPlan::Cold => self.cold_start_and_record(driver, launched_wd, resume_enabled),
        }
    }

    /// 按 fork 形态起会话:从 `source_sid` 复制历史,落到 `new_sid` 上。
    ///
    /// 与冷启的差别只有启动命令,后续 trust/ready 等待完全复用 —— fork 出来的会话在 TUI 里
    /// 与普通会话没有区别。**不发 seed**:副本自带上下文,再灌一遍 bootstrap 是重复劳动。
    ///
    /// 成功后清 `fork_pending`(血缘保留)。清不掉就 bail:留着这一位意味着**下一轮会再 fork 一次**,
    /// 每轮从源会话造一份新副本 —— 与其让它悄悄发生,不如这一轮就报出来。
    ///
    /// 起不来时**不清记录、不 kill**:与 `KillStaleThenCold` 相反。那条路的前提是「resume 起不来
    /// 说明 id 已 stale,记录没用了」;这里起不来可能只是源会话此刻正被 claude 自己写着,清掉
    /// 记录就把两个群刚谈成的那次 fork 丢了,而它本来下一条消息就能成功。
    /// 起 tmux,**但先把会话镜像的水位顶到最大**。fork 只能经这个入口起会话。
    ///
    /// fork 与 seed 在这件事上语义完全一致:接下来落进转录的字节不是本群产生的,一个都不该转发。
    /// 而 fork 比 seed 更急 —— claude 在**启动那一刻**就把父会话的全部历史写进子转录(几十 KB),
    /// 而 mirror watcher 的 `initial_eof` 是并行冻结的:它先冻结(此时子转录还是 0B),那些历史
    /// 字节就全部落在起点右侧、被当成「本轮新产出」发进目标飞书群 —— **而且撤不回来**
    /// (CLAUDE.md 第二红线,2026-08-07 那次 14 条历史转发就是这个形状)。
    ///
    /// **为什么是一个函数而不是「记得先顶一下」**:两件事必须原子地一起发生,而「顺序对不对」
    /// 靠读代码或 grep 都保不住 —— 中间插一行、把顶水位包进一个恒假的分支,顺序看着还对,
    /// 实际已经失守。收成一个入口之后,「起了会话但没顶水位」在这条路上不可表达。
    fn start_tmux_with_mirror_muted(&self, wd: &str, cmd: &str) -> anyhow::Result<()> {
        if let Some(wm) = self.mirror_watermark() {
            wm.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        }
        self.start_tmux(wd, cmd)
    }

    fn fork_start(
        &self,
        driver: CliDriver,
        source_sid: &str,
        new_sid: &str,
        wd: Option<String>,
    ) -> anyhow::Result<Start> {
        use anyhow::Context as _;

        let wd = match wd {
            Some(wd) => wd,
            None => self.effective_start_work_dir()?,
        };
        let base = self.agent.cli_cmd();
        let cmd = driver
            .fork_cmd_with_id(&base, source_sid, new_sid)
            .with_context(|| summon_i18n::format!("re2bf4fa2f0", self.agent.cli_kind()))?;
        let cmd = self.augment_launch(driver, cmd)?;
        self.start_tmux_with_mirror_muted(&wd, &cmd)?;
        if driver.captures() {
            self.accept_trust_prompt(driver)?;
            self.wait_ready(self.tmux_driver().ready_timeout())?;
        }
        anyhow::ensure!(self.tmux.has_session(), summon_i18n::format!("ra2d5b3dd51"));
        resume_store::clear_fork_pending_checked(
            self.agent,
            &self.tmux.session,
            self.agent.cli_kind(),
            new_sid,
        )
        .context(summon_i18n::text!("r3049c98458"))?;
        // 副本落在 new_sid 的转录里,而那个文件由启动目录推导。cwd 与记录对不上时转录定位不到,
        // 于是整条会话的输出都看不见 —— 与 resume 成功后更新 cwd 同一个理由。
        resume_store::update_cwd(self.agent, &self.tmux.session, &wd);
        // 副本已落盘,把水位从 MAX 落到转录当前长度:「到这为止是副本,别转发;之后的新 turn 正常发」。
        // 与 `cold_seed_wait` 收尾同一个动作。定位不到转录时**保持 MAX** —— 那种情况下宁可这个
        // 会话的镜像整段哑掉,也不能把一段来源不明的历史发进群。
        let wm_value = crate::capture::resolve_seed_transcript(
            &wd,
            new_sid,
            self.poll,
            SEED_LOCATE_WINDOW,
            &mut || self.tmux.has_session(),
        )?
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len());
        if let (Some(wm), Some(value)) = (self.mirror_watermark(), wm_value) {
            wm.store(value, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(Start::ColdStarted)
    }

    /// 冷启:按各 CLI 形态取得新 id,落盘后供下次 resume。`wd` 复用 resume 探测时已算出的启动
    /// 目录(没探测过就现算),避免同一次 ensure_started 里解析两遍 cwd。
    fn cold_start_and_record(
        &self,
        driver: CliDriver,
        wd: Option<String>,
        resume_enabled: bool,
    ) -> anyhow::Result<Start> {
        let wd = match wd {
            Some(wd) => wd,
            None => self.effective_start_work_dir()?,
        };
        // 本会话首次冷启时间:resume 不更新、reset 后新会话重写。退出回执卡片展示用。
        let created_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let new_id = self.cold_start(
            driver,
            &self.agent.cli_cmd(),
            &wd,
            resume_enabled,
            &created_at,
        )?;
        if let Some(id) = new_id {
            resume_store::write(
                self.agent,
                &self.tmux.session,
                self.agent.cli_kind(),
                &id,
                Some(&wd),
                Some(&created_at),
            );
        }
        Ok(Start::ColdStarted)
    }

    /// 冷启一个全新会话:决定启动命令(claude 预生成 `--session-id`;cursor `create-chat` 后
    /// `--resume=`;codex/Other 裸启动)→ new_session →(capture)信任/就绪/seed/等 turn,
    /// (非 capture)best-effort sleep + seed → 取得本次 session id(预定的优先,否则 codex 回捞)。
    /// `created_at` 是本会话首次冷启时间,预铸 id 落盘时一并写入;resume 路径不经过本函数。
    fn cold_start(
        &self,
        driver: CliDriver,
        base: &str,
        wd: &str,
        resume_enabled: bool,
        created_at: &str,
    ) -> anyhow::Result<Option<String>> {
        let (launch_cmd, preassigned) = self.plan_cold_launch(driver, base, resume_enabled);
        let launch_cmd = self.augment_launch(driver, launch_cmd)?;
        // 记在 new_session 之前:codex 的 rollout 回捞按「本次启动后新建」过滤,避免误抓历史会话。
        let launched = std::time::SystemTime::now();
        self.start_tmux(wd, &launch_cmd)?;
        if driver.captures() {
            let scd = self.tmux_driver();
            self.accept_trust_prompt(driver)?;
            self.wait_ready(scd.ready_timeout())?;
            // claude/cursor 的 session_id 是预铸的(launch arg);会话一旦 startup-ready,id 即有效。
            // **在 seed 等待之前**就落盘:把落盘与 seed 成败解耦——即便本轮 seed 等待失败(进程死/超时,
            // 首条消息失败、用户重发),会话活着 + id 已落 → 重发走 AlreadyRunning 续上,不会「每条消息
            // 都缺少原生 session_id」(实测 arcadia)。seed 等待本身的卡死误判已由 `cold_seed_wait`
            // 改读转录 end_turn 根治(见下),这里的早落盘是第二道保险。
            if let Some(id) = preassigned.as_deref() {
                resume_store::write(
                    self.agent,
                    &self.tmux.session,
                    self.agent.cli_kind(),
                    id,
                    Some(wd),
                    Some(created_at),
                );
            }
            // 有 seed 才等它跑完:claude/Transcript 读转录判 end_turn(根因修:慢工具阻塞期
            // raw pane 静默不再被看门狗误判卡死);仅在会话已退/总时长封顶才 Err。
            //
            // **claude 默认已无 seed**(2026-08-08):`agent.seed()` 返回 None → 整段跳过,
            // 首条用户消息直接进,省掉一整轮 bootstrap(实测 41s)。`runtime.seed` 显式配了
            // 才会走回等待路径。
            if let Some(seed) = self.agent.seed() {
                self.cold_seed_wait(driver, &seed, preassigned.as_deref(), wd)?;
            }
        } else {
            // 非 capture(codex/Other):不懂其 TUI,但 codex 也有信任门、且会话要起来后才落 rollout。
            // 先过信任门(capture() 仅用于探 trust,不解析回复),再 best-effort 发 seed 触发会话初始化。
            let _ = self.accept_trust_prompt(driver);
            if let Some(seed) = self.agent.seed() {
                let _ = self.tmux.send_line(&seed);
            }
        }
        // codex/traex 的 id 由它自己生成,启动后轮询回捞本次新建的 rollout;预定的(claude/cursor)直接用。
        let id = match preassigned {
            Some(id) => Some(id),
            None if resume_enabled
                && matches!(driver.id_acquisition(), IdAcquire::CaptureRollout) =>
            {
                self.capture_codex_id(driver, wd, launched)
            }
            _ => None,
        };
        Ok(id)
    }

    /// 冷启发 seed(`/summon` bootstrap)并等它跑完。**根因修**:claude/Transcript 读自己的结构化
    /// 转录判 `end_turn`(`wait_first_turn_end`)取代抓屏看门狗 —— bootstrap 跑慢工具(日历 API 等)
    /// 阻塞期 raw pane 静默 >300s 会被 `StallWatch` 误判卡死、整轮冷启 Err →「起会话失败」(arcadia
    /// 群/私聊唤起失败的直接原因)。转录里 end_turn 是确定性收尾、tool_use 是真进度,不靠抓屏猜稳定;
    /// idle 不再算失败(只会话已退 / 总时长封顶才 Err)。其它 capture CLI(实际不经此路:Subprocess
    /// 跳过 ensure_started)或定位窗口内文件尚未出现时使用抓屏 `wait_turn`；IO/歧义错误直接失败。
    fn cold_seed_wait(
        &self,
        driver: CliDriver,
        seed: &str,
        preassigned: Option<&str>,
        wd: &str,
    ) -> anyhow::Result<()> {
        if self.agent.capture_mode() == CaptureMode::Transcript {
            if let Some(sid) = preassigned {
                // 发 seed 前:把 mirror_watermark 顶到最大,告诉会话镜像 "seed 期间的所有 turn
                // 都别转发"。否则 mirror watcher 可能在我们设水位前就把 seed 回复发去飞书了。
                if let Some(wm) = self.mirror_watermark() {
                    wm.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
                }
                // 冷启:会话刚由我们自己拉起,pane 里不可能有 attach 用户的草稿/面板,且
                // 前面已过 wait_ready / 信任目录等待。压闸只会把「起不来」换成新的失败态。
                self.send_line_for_driver(seed, InjectGate::StartupUnproven)?;
                sleep(driver.startup_grace());
                // 定位转录:推导路径优先,没出现就按 SID 搜(自愈)。**先定位再等**——此前直接拿
                // 推导路径去 poll,推导错了就对着不存在的文件等满 SEED_OVERALL_TIMEOUT。
                // 定位不到 → 落到下面的抓屏看门狗,而不是傻等。
                let Some(path) = crate::capture::resolve_seed_transcript(
                    wd,
                    sid,
                    self.poll,
                    SEED_LOCATE_WINDOW,
                    &mut || self.tmux.has_session(),
                )?
                else {
                    crate::observe::log::log(&summon_i18n::format!(
                        "r7ae817de4d",
                        self.agent.name,
                        SEED_LOCATE_WINDOW.as_secs()
                    ));
                    if let Some(wm) = self.mirror_watermark() {
                        wm.store(0, std::sync::atomic::Ordering::Relaxed);
                    }
                    return self.seed_wait_by_screen(driver, seed, /* already_sent */ true);
                };
                // 基线 0:本次冷启用的是新铸 sid,转录必是新文件,从头等本轮 end_turn。
                let baseline = 0_u64;
                let deadline = Instant::now() + SEED_OVERALL_TIMEOUT;
                // 只在 tmux 会话已退 / 总时长封顶才 Err(idle 不算);propagate 给 cold_start。
                // seed 等待:end_turn→Ended;总时长封顶→TimedOut(降级、不丢消息);会话已退→Err。
                let outcome = crate::capture::wait_first_turn_end(
                    &LocalExecutor,
                    &path,
                    baseline,
                    self.poll,
                    crate::capture::END_TURN_GRACE,
                    deadline,
                    &mut || self.tmux.has_session(),
                );
                // seed 完成/降级后:把转录当前大小写入 mirror_watermark("到这为止算 seed,别转发;之后
                // 的新 turn 正常发")。会话已退(Err)→ 回退 0;TimedOut 也算会话已起,同样写当前大小。
                let wm_value = match &outcome {
                    Ok(_) => std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                    Err(_) => 0,
                };
                if let Some(wm) = self.mirror_watermark() {
                    wm.store(wm_value, std::sync::atomic::Ordering::Relaxed);
                }
                // 结局映射:Ended/TimedOut 都=「会话已起」→ Ok(());TimedOut 记降级日志、继续注入用户
                // 消息(真机 bug:blue-wing /summon 1min 收尾却因转录定位未命中傻等超时,第二条丢失);
                // 会话已退 → Err 传播。
                return match outcome? {
                    crate::capture::SeedWait::Ended => Ok(()),
                    crate::capture::SeedWait::TimedOut => {
                        crate::observe::log::log(&summon_i18n::format!(
                            "r71775e336c",
                            self.agent.name,
                            SEED_OVERALL_TIMEOUT.as_secs()
                        ));
                        Ok(())
                    }
                };
            }
        }
        // 退回抓屏看门狗(兜底路径:非 Transcript / 无预指定 sid)。
        self.seed_wait_by_screen(driver, seed, /* already_sent */ false)
    }

    /// 抓屏看门狗版 seed 等待。两个入口共用:①非 Transcript / 无预指定 sid(此时 seed 还没发);
    /// ②Transcript 但转录**定位不到**(seed 已经发出去了,不能再发一次,否则 claude 会收到两条)。
    /// best-effort:超时/失败都不致命(会话已起),不丢后续用户消息。
    fn seed_wait_by_screen(
        &self,
        driver: CliDriver,
        seed: &str,
        already_sent: bool,
    ) -> anyhow::Result<()> {
        let scd = self.tmux_driver();
        let p = self.tmux.capture()?;
        let (before, before_done) = (scd.snapshot(&p), scd.done_count(&p));
        if !already_sent {
            self.send_line_for_driver(seed, InjectGate::StartupUnproven)?;
            sleep(driver.startup_grace());
        }
        let _ = self.wait_turn(scd, &before, before_done, scd.seed_stall_window());
        Ok(())
    }

    /// 规划冷启命令 + 预先确定的 id(仅 claude/cursor 可预指定)。create-chat/不支持时退回裸启动、不记录。
    fn plan_cold_launch(
        &self,
        driver: CliDriver,
        base: &str,
        resume_enabled: bool,
    ) -> (String, Option<String>) {
        if !resume_enabled {
            return (base.to_string(), None);
        }
        let id = match driver.id_acquisition() {
            IdAcquire::PreMintUuid => Some(uuid::Uuid::new_v4().to_string()),
            IdAcquire::CreateChat => self.create_cursor_chat(base),
            IdAcquire::CaptureRollout | IdAcquire::Unsupported => None,
        };
        match id.and_then(|id| driver.fresh_cmd_with_id(base, &id).map(|cmd| (cmd, id))) {
            Some((cmd, id)) => (cmd, Some(id)),
            None => (base.to_string(), None),
        }
    }

    /// resume 命令起后会话是否真续起来了(stale id 会让 CLI 立刻报错退出 → tmux 会话消失)。
    fn resume_came_up(&self, driver: CliDriver) -> bool {
        sleep(driver.startup_grace());
        if !self.tmux.has_session() {
            return false; // 进程已退(多半 stale id)
        }
        if driver.captures() {
            let _ = self.accept_trust_prompt(driver);
            self.wait_ready(self.tmux_driver().ready_timeout()).is_ok() && self.tmux.has_session()
        } else {
            // 非 capture(codex):仍要过信任门,否则 resume 会卡在 trust 页;过门后坏 id 才会
            // 报错退出 → has_session 转 false,stale 兜底得以触发。
            let _ = self.accept_trust_prompt(driver);
            sleep(Duration::from_secs(2));
            self.tmux.has_session()
        }
    }

    /// cursor:跑 `create-chat` 子命令建空会话并取回其 id(失败 → None,退回裸启动、不记录)。
    fn create_cursor_chat(&self, base: &str) -> Option<String> {
        let bin = base.split_whitespace().next().unwrap_or(base);
        let mut cmd = Command::new(bin);
        cmd.arg("create-chat");
        if let Some(wd) = self.agent.working_dir() {
            cmd.current_dir(wd);
        }
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let id = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()?
            .to_string();
        (!id.is_empty()).then_some(id)
    }

    /// codex/traex:启动后轮询回捞本次会话的 rollout id。CLI 起来后(过 MCP 初始化/首轮)才落
    /// rollout,故需轮询;只认 `since` 之后新建且 cwd 匹配的转录,超时仍无 → None(下次再冷启)。
    /// 家目录与子目录按 driver 解析:codex = `~/.codex/sessions`,traex = `~/.trae/cli/sessions`
    /// (traex 不认 CODEX_HOME,实测;子目录多一层 `cli/`)。
    fn capture_codex_id(
        &self,
        driver: summon_core::driver::CliDriver,
        working_dir: &str,
        since: std::time::SystemTime,
    ) -> Option<String> {
        let home = driver.private_home()?;
        let subdir = driver.sessions_subdir()?;
        let dir = home.join(subdir);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(id) = newest_codex_rollout_id(&dir, working_dir, since) {
                return Some(id);
            }
            if Instant::now() >= deadline {
                return None;
            }
            sleep(Duration::from_millis(500));
        }
    }

    /// 首次在某目录启动时,交互式 CLI 可能要求确认工作区信任。
    /// Claude 的选中项要从屏上读(见 [`claude_startup`]);Cursor 明确要求按 `a`;
    /// Codex 默认项 "Yes, continue",回车即可。
    fn accept_trust_prompt(&self, driver: CliDriver) -> anyhow::Result<()> {
        let start = Instant::now();
        let mut accepted_codex_terminal = false;
        loop {
            let pane = self.capture_live()?;
            match driver {
                CliDriver::Claude => match claude_startup(&pane) {
                    ClaudeStartup::ConfirmTrust => {
                        self.tmux.send_enter()?;
                        sleep(Duration::from_millis(500));
                        return Ok(());
                    }
                    // 挪完不确认:下一轮重新读屏,由那时候的光标位置决定能不能按 Enter。
                    // 少了这次复核,一次没被 TUI 消费的方向键就会让 Enter 落在「No, exit」上。
                    ClaudeStartup::MoveTrustCursor(dir) => {
                        self.tmux.send_key(match dir {
                            TrustCursorMove::Down => "Down",
                            TrustCursorMove::Up => "Up",
                        })?;
                        sleep(Duration::from_millis(200));
                    }
                    ClaudeStartup::TrustCursorUnknown | ClaudeStartup::Wait => {}
                    ClaudeStartup::Ready => return Ok(()),
                },
                CliDriver::Cursor => {
                    if pane.contains("Workspace Trust Required")
                        || pane.contains("Trust this workspace")
                    {
                        self.tmux.send_key("a")?;
                        sleep(Duration::from_millis(500));
                        return Ok(());
                    }
                    if pane.contains("Cursor Agent") {
                        return Ok(());
                    }
                }
                // traex 是 codex fork,TUI 启动提示同构(2026-08-18 app-server 协议已实测一致;
                // TUI 文本未真机验证,按同构处理,真机首跑时留意)。
                CliDriver::Codex | CliDriver::Traex => {
                    match codex_startup_action(&pane) {
                        CodexStartupAction::AcceptUnsupportedTerminal => {
                            if !accepted_codex_terminal {
                                self.tmux.send_key("y")?;
                                self.tmux.send_enter()?;
                                accepted_codex_terminal = true;
                                sleep(Duration::from_millis(500));
                            }
                        }
                        CodexStartupAction::AcceptTrust => {
                            self.tmux.send_enter()?; // 默认项 "1. Yes, continue"
                            sleep(Duration::from_millis(500));
                            return Ok(());
                        }
                        CodexStartupAction::Ready => return Ok(()),
                        CodexStartupAction::Wait => {}
                    }
                }
                _ => return Ok(()),
            }
            if start.elapsed() > Duration::from_secs(20) {
                return Ok(());
            }
            sleep(self.poll);
        }
    }

    /// 送一行文本,抓取并返回本轮干净回复。唯一调用方是 debug-only 的 dev 命令,故同步 gate。
    #[cfg(debug_assertions)]
    pub fn send_and_capture(&self, text: &str) -> anyhow::Result<String> {
        if !self.agent.captures() {
            anyhow::bail!(summon_i18n::format!(
                "rec06939a52",
                self.agent.cli_kind(),
                self.agent.name
            ));
        }
        let driver = self.driver();
        let scd = self.tmux_driver();
        let p = self.tmux.capture()?;
        let (before, before_done) = (scd.snapshot(&p), scd.done_count(&p));
        self.send_line_for_driver(text, InjectGate::IdleOnly)?;
        sleep(driver.startup_grace()); // grace:让运行态起来,避免误判上一轮就绪
        let final_pane = self.wait_turn(scd, &before, before_done, Duration::from_secs(180))?;
        Ok(scd.snapshot(&final_pane))
    }

    /// 启动期等待的抓屏闸:抓屏 +(仅在空屏时)复验会话存活,会话已消亡就即刻 Err。
    ///
    /// 缺这道闸时,`start_tmux` 成功但 CLI 起来就退(work_dir 进不去、exec 失败等)会让
    /// `accept_trust_prompt` / `wait_ready` 对着一具尸体各自空转满窗口 —— 实测 2026-08-05 的
    /// flare-5:每条消息空等 62s(trust 20 + ready 40)才报「等待就绪超时」,而这个错误方向指向
    /// 「CLI 起得慢 / 就绪判定失配」,与真因(会话压根没活过来)南辕北辙,排查被带偏。
    ///
    /// 判据与 `run_turn` 侧的 `pane_proves_session_gone` 同源(那里早就做对了),这里只是把同一套
    /// 判据补到启动路径:`capture` 对死会话返回 `Ok("")` 是有意设计,`has_session` 才是权威信号。
    fn capture_live(&self) -> anyhow::Result<String> {
        let pane = self.tmux.capture()?;
        if pane_proves_session_gone(&pane, || self.tmux.has_session()) {
            anyhow::bail!(summon_i18n::format!("r1654ce04df"));
        }
        Ok(pane)
    }

    fn wait_ready(&self, timeout: Duration) -> anyhow::Result<()> {
        let scd = self.tmux_driver();
        let start = Instant::now();
        loop {
            let pane = self.capture_live()?;
            if scd.is_startup_ready(&pane) {
                return Ok(());
            }
            if start.elapsed() > timeout {
                anyhow::bail!(summon_i18n::format!("r28d9a4553a"));
            }
            sleep(self.poll);
        }
    }

    /// 轮询直到本轮结束:出现**新的**完成标记(count > prev_done)+ 答案稳定 + 非空 +
    /// 不同于 `prev`(避免锁回上一轮或 /summon 中途的子消息)。返回结束时的整屏 pane。
    ///
    /// `stall_window` 不是硬总时长上限,而是**无进展看门狗**:只在 raw pane 连续 `stall_window`
    /// 毫无变化(会话疑似卡死)时才放弃。重 agent 的 `/summon` bootstrap 只要还在流式输出 / 思考
    /// (spinner 计时每帧在变),就不会被误掐,想跑多久跑多久。
    fn wait_turn(
        &self,
        scd: TmuxDriver,
        prev: &str,
        prev_done: usize,
        stall_window: Duration,
    ) -> anyhow::Result<String> {
        let mut det = TurnDetector::new(self.debounce);
        let mut prev_pane = self.tmux.capture()?;
        let mut stall = StallWatch::new(Instant::now(), stall_window);
        loop {
            let pane = self.tmux.capture()?;
            let changed = pane != prev_pane;
            stall.observe(changed, Instant::now());
            if changed {
                prev_pane = pane.clone();
            }
            let answer = scd.snapshot(&pane); // 用抓屏快照判稳定(不含动态 footer)
            let ready = scd.is_turn_ready(&pane, prev_done); // 本轮真正结束才 ready
            let stable = det.observe(&answer, Instant::now(), ready);
            if stable && !answer.is_empty() && answer != prev {
                return Ok(pane);
            }
            if stall.stalled(Instant::now()) {
                anyhow::bail!(summon_i18n::format!("r5c20b219b8", stall_window.as_secs()));
            }
            sleep(self.poll);
        }
    }
}

/// 在 codex sessions 目录里找**本次启动后新建**(mtime ≥ since)且 cwd 匹配 working_dir 的
/// 最新 rollout 的 session id。recency 过滤掉历史会话(否则误抓全局最新的无关会话,resume 串台);
/// 只读最近一批文件的首行(避免读上千历史转录);无匹配则 None(宁可下次再冷启,也不记错 id)。
fn newest_codex_rollout_id(
    sessions_dir: &Path,
    working_dir: &str,
    since: std::time::SystemTime,
) -> Option<String> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    collect_rollouts(sessions_dir, &mut files, 0);
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime)); // mtime 新→旧
    let want = std::fs::canonicalize(working_dir).ok()?;
    for (mtime, path) in files.iter().take(24) {
        if *mtime < since {
            break; // 已按新→旧排序,再往后只会更旧 → 没有本次新建的了
        }
        let Some(meta) = first_line(path).as_deref().and_then(codex_session_meta) else {
            continue;
        };
        let matched = meta
            .cwd
            .as_deref()
            .and_then(|c| std::fs::canonicalize(c).ok())
            .is_some_and(|c| c == want);
        if !matched {
            continue;
        }
        let id = if meta.id.is_empty() {
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_codex_rollout_uuid)
        } else {
            Some(meta.id)
        };
        if id.is_some() {
            return id;
        }
    }
    None
}

/// 递归收集 rollout 转录(带 mtime),限制递归深度兜底异常目录结构。
fn collect_rollouts(dir: &Path, out: &mut Vec<(std::time::SystemTime, PathBuf)>, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            collect_rollouts(&path, out, depth + 1);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
        {
            if let Ok(mtime) = e.metadata().and_then(|m| m.modified()) {
                out.push((mtime, path));
            }
        }
    }
}

/// 读文件首行(codex rollout 的 session_meta 恒在第一行)。
fn first_line(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    std::io::BufReader::new(f).lines().next()?.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::registry::Agent;

    #[test]
    fn remote_agent_assembles_cold_resume_primer_and_rejects_live_binding_drift() {
        const MARKER: &str = "SUMMON_REMOTE_SESSION_TEST_HOME";
        let Ok(home) = std::env::var(MARKER) else {
            let temporary = tempfile::Builder::new()
                .prefix("srt-")
                .tempdir_in("/tmp")
                .unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "session::session::tests::remote_agent_assembles_cold_resume_primer_and_rejects_live_binding_drift", "--nocapture"])
                .env("HOME", temporary.path())
                .env(MARKER, temporary.path())
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        };
        // HOME 与工具解析只在隔离子进程中设置，不污染并行测试或真实 tmux。
        let home = std::path::PathBuf::from(home);
        let fake_tmux = home.join("tmux");
        crate::test_support::write_test_executable(
            &fake_tmux,
            "#!/bin/sh\n[ \"$1\" = has-session ] && exit 0\nexit 99\n",
        );
        std::fs::create_dir_all(home.join(".summon")).unwrap();
        std::fs::write(
            home.join(".summon/config.yaml"),
            format!(
                "execution_targets:\n  laptop: {{}}\ntools:\n  tmux: {}\n",
                fake_tmux.display()
            ),
        )
        .unwrap();
        summon_core::tools::configure(&Config::load());
        let fake_claude = home.join("claude");
        crate::test_support::write_test_executable(
            &fake_claude,
            "#!/bin/sh\nprintf '%s\\n' \"$@\"\n",
        );
        let mut agent: Agent = serde_yaml::from_str(
            "name: remote-test\nruntime:\n  cli: claude\n  execution:\n    target: laptop\n",
        )
        .unwrap();
        agent.dir = home.join(".summon/agents/remote-test");
        std::fs::create_dir_all(&agent.dir).unwrap();
        crate::remote_tools::integration::with_test_target(|| {
            let session = Session::for_agent(&agent, 10, 10);
            crate::remote_tools::integration::check_binding(
                &agent,
                &session.tmux.session,
                false,
                false,
            )
            .unwrap();
            session.write_system_prompt_file("BASE PRIMER").unwrap();
            let primer = session.system_prompt_text().unwrap();
            assert!(primer.contains("BASE PRIMER"));
            assert!(primer.contains("execution MCP"));
            assert!(primer.contains("summon MCP"));
            assert!(primer.contains("laptop"));
            let executable = fake_claude.to_str().unwrap();
            let (cold, _) = session.plan_cold_launch(CliDriver::Claude, executable, true);
            let resume = CliDriver::Claude.resume_cmd(executable, "saved-session");
            for base in [cold, resume] {
                let command = session.augment_launch(CliDriver::Claude, base).unwrap();
                for token in [
                    "--mcp-config",
                    "--strict-mcp-config",
                    "--tools",
                    "execution",
                    "summon",
                    "proxy",
                    "platform",
                    "/remote/project",
                    "--append-system-prompt-file",
                ] {
                    assert!(command.contains(token), "missing {token}: {command}");
                }
                let output = std::process::Command::new("/bin/sh")
                    .args(["-c", &command])
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let output = String::from_utf8(output.stdout).unwrap();
                let args: Vec<_> = output.lines().collect();
                let value =
                    |flag: &str| args[args.iter().position(|arg| *arg == flag).unwrap() + 1];
                assert_eq!(value("--tools"), "");
                let config: serde_json::Value =
                    serde_json::from_str(value("--mcp-config")).unwrap();
                assert_eq!(
                    config["mcpServers"]["execution"]["args"],
                    serde_json::json!([
                        "remote-tools",
                        "proxy",
                        "--target",
                        "laptop",
                        "--working-dir",
                        "/remote/project"
                    ])
                );
                assert_eq!(
                    config["mcpServers"]["summon"]["args"],
                    serde_json::json!(["remote-tools", "platform"])
                );
                assert_eq!(
                    value("--append-system-prompt-file"),
                    session.system_prompt_path().to_str().unwrap()
                );
            }
            assert_eq!(session.ensure_started().unwrap(), Start::AlreadyRunning);
            let mut changed = agent.clone();
            changed
                .runtime
                .as_mut()
                .unwrap()
                .execution
                .as_mut()
                .unwrap()
                .working_dir = Some("/remote/other".into());
            let error = Session::for_agent(&changed, 10, 10)
                .ensure_started()
                .unwrap_err();
            assert!(error.to_string().contains("/reset"));
            changed.runtime.as_mut().unwrap().execution = None;
            let error = Session::for_agent(&changed, 10, 10)
                .ensure_started()
                .unwrap_err();
            assert!(error.to_string().contains("/reset"));
        });
    }

    // summon crate 不依赖 toml,直接构造最小 Agent(字段均 pub)。
    fn agent_named(name: &str) -> Agent {
        Agent {
            name: name.to_string(),
            display: String::new(),
            status: "active".to_string(),
            role: Default::default(),
            model: None,
            effort: None,
            reasoning_summary: None,
            feishu: None,
            runtime: None,
            aliases: std::collections::HashMap::new(),
            messaging: None,
            dir: std::path::PathBuf::new(),
            global_config_path: None,
            app_owner: Default::default(),
        }
    }

    /// 只取生产段(顶层 `mod tests` 之前)。**不能按裸 `#[cfg(test)]` 切** —— 这几个文件里都有
    /// 位置更靠前的 test-only 生产函数(如 `transcript.rs:605` 的 `prompt_wait_deadline`),
    /// 按那个切会把真正的注入点整段切掉,测试于是恒绿。
    fn production_of(src: &str) -> &str {
        src.split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(production, _)| production)
            .unwrap_or(src)
    }

    /// 红线:**注入闸装在注入原语上**,不是某个执行器的私有逻辑。
    ///
    /// 这是 spec 2026-08-14 第二节的核心命题,也是 P0「独立有价值」那句话的全部价值:面板/菜单
    /// 进 pane 的三条路径根本不经过飞书透传(attach 用户自己跑 `/status`、没人点的确认菜单、
    /// peers 热更新与普通 prompt 的裸注入),闸只要挪进透传执行器,这三条立刻恢复盲注。
    ///
    /// 对抗审查(2026-08-14)实测:把这两行删掉换成 `let _ = gate;`,**2630 条测试一条都不红** ——
    /// 因为测的全是 `classify_pane` / `gate_accepts` / `prove_injectable` 这些纯函数和一个假 pane,
    /// 没有任何测试守「闸真的挂在注入原语上」。这条补的就是那个缺口。
    #[test]
    fn the_injection_gate_is_installed_on_the_send_primitive_itself() {
        let code = production_of(include_str!("session.rs"));
        let start = code
            .find("fn send_line_for_driver_inner(")
            .expect("send_line_for_driver_inner 不见了(改名了?同步更新本测试)");
        let end = code[start..]
            .find("\n    fn send_cursor_input(")
            .map(|i| start + i)
            .unwrap_or(code.len());
        let body: String = code[start..end]
            .lines()
            .map(|l| l.split("//").next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        let gate = body
            .find("input_gate::prove_injectable(&self.tmux, gate,")
            .expect("注入原语里必须调用注入闸,且把调用方声明的 gate 传下去");
        assert!(
            body[..gate].contains("matches!(driver, CliDriver::Claude)"),
            "闸只对 claude 生效(判据全是 claude TUI 的真机特征),这条限定不能丢"
        );
        // 闸必须先于**任何**可能产生按键的动作 —— 被拒的语义是「一个键都没发出去」。
        for side_effect in [
            "self.tmux.exit_copy_mode()",
            "self.tmux.clear_input()",
            "self.tmux.send_text(",
            "self.tmux.paste_text(",
            "self.send_cursor_input(",
            "self.tmux.send_enter()",
        ] {
            if let Some(at) = body.find(side_effect) {
                assert!(
                    at > gate,
                    "{side_effect} 排在闸前面 —— 被拒时它已经发出去了"
                );
            }
        }
    }

    /// 红线:claude 的注入必须走 bracketed paste,不能退回逐键 send-keys。
    /// 逐键注入会让大 prompt 的**开头**被 TUI 静默吞掉,而送达判据只找位于尾部的 transport
    /// marker —— marker 还在,于是「掐了头的消息」一路判成投递成功。这条损坏没有任何外显信号,
    /// 靠 review 记不住,只能钉在测试里。
    #[test]
    fn claude_injects_by_paste_not_by_keystrokes() {
        assert!(
            CliDriver::Claude.injects_by_paste(),
            "claude 的通道是 Transcript(tmux TUI),它必须按粘贴收正文"
        );
        let code = production_of(include_str!("session.rs"));
        let start = code
            .find("fn send_line_for_driver_inner(")
            .expect("send_line_for_driver_inner 不见了(改名了?同步更新本测试)");
        let end = code[start..]
            .find("\n    fn send_cursor_input(")
            .map(|i| start + i)
            .unwrap_or(code.len());
        let body: String = code[start..end]
            .lines()
            .map(|l| l.split("//").next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("driver.injects_by_paste()") && body.contains("self.tmux.paste_text("),
            "注入原语必须按 driver 的 injects_by_paste 分流到 paste_text"
        );
    }

    /// 红线:**每个注入口选了哪道闸,是被冻结的**。
    ///
    /// `send_line_for_driver` 没有默认 gate 值,新增注入口在编译期就被逼着表态 —— 但「表态成
    /// 哪个」编译器管不了:把 peers 热更新从 `IdleOnly` 改成 `PromptQueue`,或者把 slash 从
    /// `IdleOnly` 放宽,都不会有任何测试变红(对抗审查 2026-08-14 点名的推论)。这里把全仓的
    /// 注入口清单连同各自的闸一起钉住:改一个值就得改这张表,顺带被逼着说明为什么。
    #[test]
    fn every_injection_site_freezes_which_gate_it_picked() {
        /// 扫出一份源码里所有**调用点**(定义本身不算)各自声明的闸。
        fn declared_gates(src: &str) -> Vec<&'static str> {
            const GATES: [&str; 3] = [
                "InjectGate::IdleOnly",
                "InjectGate::PromptQueue",
                "InjectGate::StartupUnproven",
            ];
            let code = production_of(src);
            code.match_indices("send_line_for_driver(")
                .filter(|(i, _)| !code[..*i].ends_with("fn "))
                .map(|(i, _)| {
                    // 按字符取,别按字节切 —— 这个仓的注释是中文,字节切会落在字符中间。
                    let tail: String = code[i..].chars().take(160).collect();
                    *GATES
                        .iter()
                        .find(|g| tail.contains(*g))
                        .unwrap_or_else(|| panic!("注入点没有显式声明闸:{tail}"))
                })
                .collect()
        }
        let mut found: Vec<(&str, &str)> = Vec::new();
        for (file, src) in [
            ("session.rs", include_str!("session.rs")),
            ("slash_exec.rs", include_str!("slash_exec.rs")),
            (
                "chat_thread.rs",
                include_str!("../runtime/daemon/chat_thread.rs"),
            ),
            ("transcript.rs", include_str!("../capture/transcript.rs")),
        ] {
            for gate in declared_gates(src) {
                found.push((file, gate));
            }
        }
        found.sort_unstable();
        // 冻结清单。左边是文件,右边是那个注入口选的闸;新增/改动必须在这里显式登记。
        let expected: Vec<(&str, &str)> = vec![
            // 普通 prompt / 轮内 steer:带 transport marker,有自己的两相送达协议。
            ("transcript.rs", "InjectGate::PromptQueue"),
            ("transcript.rs", "InjectGate::PromptQueue"),
            // peers 热更新 + `/effort` + `/cd`:markerless 后台注入,必须空输入框。
            ("chat_thread.rs", "InjectGate::IdleOnly"),
            ("chat_thread.rs", "InjectGate::IdleOnly"),
            ("chat_thread.rs", "InjectGate::IdleOnly"),
            // `inject_command`(`/compact`)+ debug-only 的 `send_and_capture`。
            ("session.rs", "InjectGate::IdleOnly"),
            ("session.rs", "InjectGate::IdleOnly"),
            // 冷启 seed 两条路径:pane 刚由我们自己拉起,显式豁免。
            ("session.rs", "InjectGate::StartupUnproven"),
            ("session.rs", "InjectGate::StartupUnproven"),
            // 白名单 slash 透传。
            ("slash_exec.rs", "InjectGate::IdleOnly"),
        ];
        let mut expected = expected;
        expected.sort_unstable();
        assert_eq!(
            found, expected,
            "注入口清单变了:新增注入口要在这里登记它选的闸,改闸也要改这张表"
        );
    }

    #[test]
    fn with_name_uses_given_tmux_name() {
        let a = agent_named("x");
        let s = Session::with_name(&a, "soul-x-pdeadbeef".to_string(), 200, 800);
        assert_eq!(s.tmux.session, "soul-x-pdeadbeef");
        // for_agent 仍回退裸名
        let s2 = Session::for_agent(&a, 200, 800);
        assert_eq!(s2.tmux.session, "soul-x");
    }

    #[test]
    fn canonical_start_work_dir_matches_existing_real_path() {
        let tmp = std::env::temp_dir().join(format!("summon-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let expected = std::fs::canonicalize(&tmp)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            canonical_start_work_dir(tmp.to_string_lossy().into_owned()),
            expected
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn codex_startup_action_handles_terminal_warning_before_trust() {
        assert_eq!(
            codex_startup_action(
                "WARNING: TERM is set to \"dumb\". Codex's interactive TUI may not work.\n\
                 Continue anyway? [y/N]:"
            ),
            CodexStartupAction::AcceptUnsupportedTerminal
        );
        assert_eq!(
            codex_startup_action("Do you trust the contents of this directory?"),
            CodexStartupAction::AcceptTrust
        );
        assert_eq!(
            codex_startup_action("OpenAI Codex (v0.144.1)"),
            CodexStartupAction::Ready
        );
        assert_eq!(
            codex_startup_action(
                "TERM is set to \"dumb\". Continue anyway? [y/N]\nOpenAI Codex (v0.144.1)"
            ),
            CodexStartupAction::Ready,
            "已进入主界面后不得反复消费滚屏里残留的旧 warning"
        );
    }

    #[test]
    fn augment_launch_adds_model_for_cursor_and_sysprompt_file_for_claude() {
        let tmp = std::env::temp_dir().join(format!("summon-augment-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Cursor + model → 追加 --model;无启动期 system-prompt flag。
        let mut cur: Agent =
            serde_yaml::from_str("name: c\nmodel: gpt-5.5-high\nruntime:\n  cli: cursor\n")
                .unwrap();
        let cur_dir = tmp.join("agents").join(&cur.name);
        std::fs::create_dir_all(&cur_dir).unwrap();
        cur.dir = cur_dir;
        let s = Session::for_agent(&cur, 200, 800);
        assert!(!s.injects_system_prompt_at_launch());
        // 身份隔离 env 前缀恒在(无 feishu 配置 → 走 env -i 干净启动);断言用
        // starts_with/ends_with 解耦掉「环境里有哪些系统变量」这个不确定项。
        let out = s
            .augment_launch(CliDriver::Cursor, "agent --resume=chat-1".to_string())
            .unwrap();
        assert!(
            out.starts_with("env -i /bin/sh -c "),
            "应有 env 隔离前缀: {out}"
        );
        let env_body = std::fs::read_to_string(s.launch_env_path()).unwrap();
        assert!(
            env_body.contains("SUMMON_MANAGED_SESSION='1'\n"),
            "summon-owned 会话必须让 bridge Hook 立即跳过: {env_body}"
        );
        let sess2 = Session::for_agent(&cur, 200, 800)
            .with_run_env(vec![("SUMMON_MANAGED_SESSION".into(), "0".into())]);
        sess2
            .augment_launch(CliDriver::Cursor, "agent".to_string())
            .unwrap();
        let overridden = std::fs::read_to_string(sess2.launch_env_path()).unwrap();
        assert!(
            overridden.rfind("SUMMON_MANAGED_SESSION='1'")
                > overridden.rfind("SUMMON_MANAGED_SESSION='0'"),
            "保留 marker 必须最后覆盖 run env:{overridden}"
        );
        assert!(
            out.ends_with("exec agent --resume=chat-1 \"--model\" \"gpt-5.5-high\"'"),
            "cursor 尾部 = exec 命令 + model,无权限 flag: {out}"
        );
        // cursor 不带 tool-guard settings:它走无头子进程,没有 PreToolUse 机制,
        // 带上只会给 CLI 一个不认识的 flag。这是已知的 L1 覆盖缺口(记在 spec)。
        assert!(!out.contains("guard-settings.json"), "{out}");

        // Claude:model/effort 在 tmux 路径生效 + 权限 flag(review #4) + --name 命名。
        let mut cla: Agent = serde_yaml::from_str(
            "name: k\nmodel: claude-opus-4-8\neffort: high\nruntime:\n  cli: claude\n",
        )
        .unwrap();
        let cla_dir = tmp.join("agents").join(&cla.name);
        std::fs::create_dir_all(&cla_dir).unwrap();
        cla.dir = cla_dir;
        let s = Session::for_agent(&cla, 200, 800);
        assert!(s.injects_system_prompt_at_launch());
        // 文件未写 → model/effort flag + 权限 flag + --name,无 system-prompt flag。
        let out = s
            .augment_launch(CliDriver::Claude, "claude --resume id".to_string())
            .unwrap();
        assert!(out.ends_with(
            &format!(
                "exec claude --resume id \"--model\" \"claude-opus-4-8\" \"--effort\" \"high\" \"--permission-mode\" \"bypassPermissions\" \"--name\" \"summon:soul-k\" \"--settings\" \"{}\"'",
                cla.state_dir().join("runtime").join("chats").join("soul-k.guard-settings.json").display()
            )
        ), "claude 尾部 = exec 命令 + model/effort + 权限 flag + --name + tool-guard settings: {out}");
        // 写了 PRIMER → 权限 flag 之后再追加文件 flag(--name 在权限 flag 之后、system-prompt 之前)。
        s.write_system_prompt_file("PRIMER").unwrap();
        let out = s
            .augment_launch(CliDriver::Claude, "claude --resume id".to_string())
            .unwrap();
        assert!(
            out.contains("\"--permission-mode\" \"bypassPermissions\""),
            "应有权限 flag: {out}"
        );
        assert!(
            out.contains("\"--name\" \"summon:soul-k\""),
            "应有 --name flag: {out}"
        );
        assert!(
            out.ends_with(&format!(
                "--append-system-prompt-file \"{}\"'",
                cla.state_dir()
                    .join("runtime")
                    .join("chats")
                    .join("soul-k.system-prompt.txt")
                    .display()
            )),
            "PRIMER 落盘后尾部 = system-prompt flag: {out}"
        );
        // 执行闸:claude 必带 --settings,否则外部发起人的写操作没有任何边界。
        assert!(
            out.contains("\"--settings\""),
            "claude 启动缺 --settings,tool-guard 不生效: {out}"
        );
        let guard = std::fs::read_to_string(
            cla.state_dir()
                .join("runtime")
                .join("chats")
                .join("soul-k.guard-settings.json"),
        )
        .unwrap();
        // 漏一个写类工具 = 静默放行。改这份清单必须同步 spec 与 SKILL.md 里的两份副本。
        for tool in [
            "Bash",
            "BashOutput",
            "Write",
            "Edit",
            "NotebookEdit",
            "KillShell",
        ] {
            assert!(guard.contains(tool), "matcher 漏了 {tool}: {guard}");
        }
        assert!(guard.contains("tool-guard-hook"), "{guard}");
        assert!(guard.contains("PreToolUse"), "{guard}");
        // 授权 hook 的路由参数来自启动时的 agent 与会话名,不靠 hook 进程的环境变量。
        let settings: serde_json::Value = serde_json::from_str(&guard).unwrap();
        let permission = &settings["hooks"]["PermissionRequest"][0]["hooks"][0];
        assert_eq!(
            permission["args"],
            serde_json::json!([
                "daemon",
                "permission-hook",
                "--agent",
                cla.name,
                "--session",
                "soul-k"
            ])
        );
        assert!(permission["timeout"].as_u64().unwrap() >= 600, "{guard}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PostCompact 只写给认得它的 claude;写的时候路由参数同授权 hook,来自启动时的 agent 与会话名。
    #[test]
    fn post_compact_hook_is_written_only_when_claude_knows_it() {
        let without: serde_json::Value =
            serde_json::from_str(&render_guard_settings("k", "soul-k", false)).unwrap();
        assert!(without["hooks"].get("PostCompact").is_none(), "{without}");
        assert!(without["hooks"].get("PreToolUse").is_some());
        assert!(without["hooks"].get("PermissionRequest").is_some());

        let with: serde_json::Value =
            serde_json::from_str(&render_guard_settings("k", "soul-k", true)).unwrap();
        let hook = &with["hooks"]["PostCompact"][0]["hooks"][0];
        assert_eq!(
            hook["args"],
            serde_json::json!([
                "daemon",
                "compact-hook",
                "--agent",
                "k",
                "--session",
                "soul-k"
            ])
        );
        assert!(hook["timeout"].as_u64().unwrap() <= 10, "{with}");
        assert_eq!(with["hooks"]["PreToolUse"], without["hooks"]["PreToolUse"]);
        assert_eq!(
            with["hooks"]["PermissionRequest"],
            without["hooks"]["PermissionRequest"]
        );
    }

    /// PostCompact 按这次启动实际会执行的 claude 判:裸名按启动 env 文件里生效的 PATH 找,
    /// `runtime.env` 的 PATH 覆盖 daemon 自己的。
    #[test]
    fn guard_settings_ask_the_claude_found_on_the_launch_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cla: Agent =
            serde_yaml::from_str("name: k\nruntime:\n  cli: claude\n  cli_cmd: claude\n").unwrap();
        let dir = tmp.path().join("agents").join(&cla.name);
        std::fs::create_dir_all(&dir).unwrap();
        cla.dir = dir;
        let fake_claude = |sub: &str, version: &str| {
            let bin = tmp.path().join(sub);
            std::fs::create_dir_all(&bin).unwrap();
            let exe = bin.join("claude");
            crate::test_support::write_test_executable(&exe, &format!("#!/bin/sh\n# {sub}\n"));
            crate::runtime::claude_hook::FAKE_CLAUDE_VERSIONS
                .lock()
                .unwrap()
                .insert(std::fs::canonicalize(&exe).unwrap(), version.to_string());
            bin.to_string_lossy().into_owned()
        };
        for (bin, expected) in [
            (fake_claude("new", "2.1.304 (Claude Code)\n"), true),
            (fake_claude("old", "2.1.75 (Claude Code)\n"), false),
        ] {
            let s = Session::for_agent(&cla, 200, 800).with_run_env(vec![("PATH".into(), bin)]);
            s.augment_launch(CliDriver::Claude, "claude".to_string())
                .unwrap();
            let settings: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(guard_settings_path(&cla, &s.tmux.session)).unwrap(),
            )
            .unwrap();
            assert_eq!(
                settings["hooks"].get("PostCompact").is_some(),
                expected,
                "{settings}"
            );
        }
    }

    /// 启动 env 里有几个 PATH(系统基础变量、runtime.env、lark-cli 隔离注入的)时,探测跟随最后
    /// 那个 —— pane 里逐行载入 env 文件后生效的就是它。augment_launch 拿写进 env 文件的同一份
    /// pairs 去探测。
    #[test]
    fn the_probe_follows_the_last_path_in_the_launch_env() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = |sub: &str| {
            let dir = tmp.path().join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            crate::test_support::write_test_executable(&dir.join("claude"), "#!/bin/sh\n");
            dir.to_string_lossy().into_owned()
        };
        let (system, run_env, inject) = (bin("system"), bin("run-env"), bin("lark-wrapper"));
        let pairs = vec![
            ("PATH".to_string(), system),
            ("HOME".to_string(), "/h".to_string()),
            ("PATH".to_string(), run_env),
            ("PATH".to_string(), format!("{inject}:/usr/bin")),
            ("SUMMON_MANAGED_SESSION".to_string(), "1".to_string()),
        ];
        assert_eq!(
            launch_program_in_env("claude", &pairs),
            Some(tmp.path().join("lark-wrapper").join("claude"))
        );
        assert_eq!(launch_program_in_env("claude", &pairs[1..2]), None);

        let src = include_str!("session.rs");
        let body = &src[src.find("    fn augment_launch(").unwrap()..];
        let body = &body[..body.find("\n    }\n").unwrap()];
        assert!(body.contains("let pairs = self.launch_env_pairs();"));
        assert!(body.contains("launch_program_in_env(&self.agent.cli_cmd(), &pairs)"));
        assert!(body.contains("self.write_launch_env_file(&pairs)"));
    }

    /// P3 fail-closed:claude 配了非法 effort,tmux 路径降级丢 effort(只剩 --model),不 panic。
    #[test]
    fn augment_launch_drops_illegal_effort_on_tmux_path() {
        let tmp = std::env::temp_dir().join(format!("summon-augment-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // minimal 不在 claude 合法集 → 丢 effort,只剩 --model
        let mut cla: Agent = serde_yaml::from_str(
            "name: k\nmodel: claude-opus-4-8\neffort: minimal\nruntime:\n  cli: claude\n",
        )
        .unwrap();
        let cla_dir = tmp.join("agents").join(&cla.name);
        std::fs::create_dir_all(&cla_dir).unwrap();
        cla.dir = cla_dir;
        let s = Session::for_agent(&cla, 200, 800);
        // 非法 effort 丢掉 → 只剩 --model + 权限 flag + --name,绝无 --effort。
        let out = s
            .augment_launch(CliDriver::Claude, "claude --resume id".to_string())
            .unwrap();
        assert!(out.ends_with(
            &format!(
                "exec claude --resume id \"--model\" \"claude-opus-4-8\" \"--permission-mode\" \"bypassPermissions\" \"--name\" \"summon:soul-k\" \"--settings\" \"{}\"'",
                cla.state_dir().join("runtime").join("chats").join("soul-k.guard-settings.json").display()
            )
        ), "丢 effort 后尾部 = model + 权限 flag + --name + tool-guard settings: {out}");
        assert!(!out.contains("--effort"), "非法 effort 必须被丢: {out}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// claude 冷启带本群生效的 `--model` / `--effort`:有本群覆盖的群用覆盖,别的群用 bot 默认。
    #[test]
    fn augment_launch_uses_the_chats_own_model_and_effort() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: examplebot\nmodel: claude-opus-5-5\neffort: high\nruntime:\n  cli: claude\n",
        )
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        crate::runtime::chat_model::set_chat_model(&agent, "g-a", Some("claude-sonnet-5-5"))
            .unwrap();
        crate::runtime::chat_model::set_chat_effort(&agent, "g-a", Some("low")).unwrap();
        let launch = |tag: &str| {
            Session::with_name(&agent, format!("soul-examplebot-{tag}"), 200, 800)
                .with_run_env(vec![("SUMMON_CONVERSATION_TAG".into(), tag.into())])
                .augment_launch(CliDriver::Claude, "claude".to_string())
                .unwrap()
        };
        let a = launch("g-a");
        assert!(a.contains("\"--model\" \"claude-sonnet-5-5\""), "{a}");
        assert!(a.contains("\"--effort\" \"low\""), "{a}");
        let b = launch("g-b");
        assert!(b.contains("\"--model\" \"claude-opus-5-5\""), "{b}");
        assert!(b.contains("\"--effort\" \"high\""), "{b}");
    }

    /// 飞书身份隔离(tmux 路径回归,owner 报的 bug):两个 agent 同处一个 registry,self 起 tmux 会话
    /// 必须 ① `env -i` 干净启动(继承环境全清);② 只注入 **self 自己**的凭据(规范名 + 自己 alias 名);
    /// ③ 兄弟的凭据值绝不出现(根本没去读兄弟 .env);④ **自己的 secret 也不得出现在命令串里**——
    /// 命令串会变成 tmux server 的 argv、被同 uid 的 `ps` 读到(本次修的洞),凭据只走 0600 env 文件。
    #[test]
    fn augment_launch_isolates_feishu_identity_from_sibling() {
        let agents = std::env::temp_dir().join(format!("summon-iso-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&agents);
        let self_dir = agents.join("self");
        let sib_dir = agents.join("sibling");
        std::fs::create_dir_all(&self_dir).unwrap();
        std::fs::create_dir_all(&sib_dir).unwrap();
        std::fs::write(
            self_dir.join("agent.yaml"),
            "name: self\nfeishu:\n  app_id_env: FEISHU_SELF_APP_ID\n  app_secret_env: FEISHU_SELF_APP_SECRET\nruntime:\n  cli: cursor\n",
        )
        .unwrap();
        std::fs::write(
            self_dir.join(".env"),
            "FEISHU_SELF_APP_ID=selfid\nFEISHU_SELF_APP_SECRET=selfsecret\n",
        )
        .unwrap();
        std::fs::write(
            sib_dir.join("agent.yaml"),
            "name: sibling\nfeishu:\n  app_id_env: FEISHU_SIBLING_APP_ID\n  app_secret_env: FEISHU_SIBLING_APP_SECRET\nruntime:\n  cli: cursor\n",
        )
        .unwrap();
        std::fs::write(
            sib_dir.join(".env"),
            "FEISHU_SIBLING_APP_ID=sibid\nFEISHU_SIBLING_APP_SECRET=sibsecret\n",
        )
        .unwrap();

        let mut me: Agent = serde_yaml::from_str(
            "name: self\nfeishu:\n  app_id_env: FEISHU_SELF_APP_ID\n  app_secret_env: FEISHU_SELF_APP_SECRET\nruntime:\n  cli: cursor\n",
        )
        .unwrap();
        me.dir = self_dir.clone();
        let s = Session::for_agent(&me, 200, 800);
        let out = s
            .augment_launch(CliDriver::Cursor, "agent".to_string())
            .unwrap();
        let env_body = std::fs::read_to_string(s.launch_env_path()).unwrap();

        // ① env -i 干净启动(不再用 -u 白名单),兄弟键自然被清空。
        assert!(
            out.starts_with("env -i "),
            "应用 -i 干净启动(取代 -u 白名单): {out}"
        );
        assert!(
            !out.contains("-u FEISHU_SIBLING_APP_ID"),
            "不再用 -u 逐个清: {out}"
        );
        // ② 只注入 self 自己的(规范名 + 自己 alias 名),且注入面是 env 文件而非命令行。
        assert!(
            env_body.contains("FEISHU_APP_ID='selfid'\n"),
            "应注入规范 id: {env_body}"
        );
        assert!(
            env_body.contains("FEISHU_SELF_APP_ID='selfid'\n"),
            "应注入自己 alias id: {env_body}"
        );
        assert!(
            env_body.contains("FEISHU_APP_SECRET='selfsecret'\n"),
            "应注入规范 secret: {env_body}"
        );
        assert!(
            env_body.contains("FEISHU_SELF_APP_SECRET='selfsecret'\n"),
            "应注入自己 alias secret: {env_body}"
        );
        // ③ 兄弟的凭据值绝不出现(根本没读兄弟 .env)。
        assert!(!out.contains("sibid"), "兄弟 id 值不得出现: {out}");
        assert!(!out.contains("sibsecret"), "兄弟 secret 值不得出现: {out}");
        assert!(
            !env_body.contains("sibsecret"),
            "兄弟 secret 值不得出现: {env_body}"
        );
        // ④ 本次修的洞:自己的 secret 也不得进命令串(它会成为 tmux server 的 argv)。
        // 连 argv 拼装的下游一起断言:`tmux new-session` 的每一项都不含 secret。
        assert!(
            !out.contains("selfsecret"),
            "secret 绝不能进 tmux 命令行(会落进 ps): {out}"
        );
        for arg in s.tmux.new_session_argv("/tmp", &out) {
            assert!(
                !arg.contains("selfsecret") && !arg.contains("selfid"),
                "tmux argv 不得含任何凭据值: {arg}"
            );
        }
        // env 文件必须是 0600:同 uid 挡不住,但至少不给同机其它用户。
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(s.launch_env_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "启动 env 文件必须 0600");

        let _ = std::fs::remove_dir_all(&agents);
    }

    /// 结构守护:tmux 启动命令的构造路径上,绝不许再出现「把环境键值对内联进命令串」的模板。
    /// 这是本次安全缺陷的原始形态(`prefix.push_str(&format!("{k}=\"{v}\" "))`),behavioral 测试
    /// 只能覆盖当前入参组合,这条按源码兜底:谁再写回内联模板就变红,逼其显式复核。
    ///
    /// 局限:substring 扫描,只认下面几种最自然的内联写法;真要绕开(改个变量名)是绕得开的。
    /// 它兜的是「无意中改回去」,不是对抗恶意提交——真正的判据仍是同文件的 behavioral 断言。
    #[test]
    fn tmux_launch_command_never_inlines_env_pairs() {
        // 模板在运行时拼出来,否则这个数组自己就会被下面的扫描命中(自指假红)。
        let lb = "{";
        let rb = "}";
        let forbidden: Vec<String> = [("k", "v"), ("key", "value"), ("name", "value")]
            .iter()
            .flat_map(|(a, b)| {
                [
                    // 源码里 `format!("{k}=\"{v}\" ")` 的**字面字节**是带反斜杠的,按字节匹配。
                    format!("{lb}{a}{rb}=\\\"{lb}{b}{rb}\\\""),
                    format!("{lb}{a}{rb}={lb}{b}{rb}"),
                ]
            })
            .collect();
        // 命令串/argv 的构造点(相对 crates/summon/src)。新增构造点请一并登记。
        const LAUNCH_SITES: &[&str] = &["session/session.rs", "session/tmux.rs"];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut saw_augment_launch = false;
        for rel in LAUNCH_SITES {
            let text = std::fs::read_to_string(src.join(rel)).expect("读启动构造点源码失败");
            // 只扫生产代码:测试段里必然要出现假密钥键名做断言。
            let prod = text.split("#[cfg(test)]").next().unwrap_or_default();
            saw_augment_launch |= prod.contains("fn augment_launch");
            for pat in &forbidden {
                assert!(
                    !prod.contains(pat.as_str()),
                    "{rel} 里出现了内联 env 键值对的模板 `{pat}`——凭据会进 tmux server 的 argv"
                );
            }
            // 启动命令的构造路径不该认识任何具体凭据键名:键值对整体走 env 文件,
            // 这里一旦出现 `*_APP_SECRET` 就说明有人又开始按名字往命令行里塞了。
            assert!(
                !prod.contains("APP_SECRET"),
                "{rel} 的启动构造路径不该出现 APP_SECRET 字样"
            );
        }
        // 非空自检:扫描真的读到了启动命令构造函数,否则路径写错会让本测试恒绿。
        assert!(
            saw_augment_launch,
            "扫描没读到 augment_launch,路径可能写错了"
        );
    }

    // `ensure_started` 的 resume/冷启**判定**已抽成纯函数 `summon_core::start_plan::plan_start`,
    // 由该模块的非 ignored 单测钉死 4 个入参的全部 16 种组合;下面 4 个 `#[ignore]` 真机测试保留做
    // 端到端 proof(探测/启动/落盘串起来是否真的对),两者不重复。

    // resume 记录的读写/清理/双清/裸名 round-trip 均已收口进 `resume_store`,由该模块自带的
    // 单测钉死(dual_read_prefers_new_path、lazy_migration_*、clear_chat_wipes_both_new_and_legacy、
    // owner_bare_name_tag_mapping_round_trips);此处不再重复覆盖同一逻辑,避免双份维护。

    #[test]
    fn plan_cold_launch_per_cli() {
        // claude:预生成 uuid,启动命令带 --session-id <uuid>,且记录该 id
        let claude = crate::test_support::agent_with("claude", "");
        let (cmd, id) = Session::for_agent(&claude, 200, 800).plan_cold_launch(
            CliDriver::Claude,
            "claude",
            true,
        );
        let id = id.expect("claude 应预生成 id");
        assert!(cmd.starts_with("claude --session-id "), "cmd was: {cmd}");
        assert!(cmd.ends_with(&id));

        // codex:CaptureRollout,启动命令裸 base、无预定 id(启动后回捞)
        let codex = crate::test_support::agent_with("codex", "");
        assert_eq!(
            Session::for_agent(&codex, 200, 800).plan_cold_launch(CliDriver::Codex, "codex", true),
            ("codex".to_string(), None)
        );

        // resume 关闭:裸启动、不记录
        assert_eq!(
            Session::for_agent(&claude, 200, 800).plan_cold_launch(
                CliDriver::Claude,
                "claude",
                false
            ),
            ("claude".to_string(), None)
        );

        // Other:不支持 resume → 裸启动
        let other = crate::test_support::agent_with("weird", "");
        assert_eq!(
            Session::for_agent(&other, 200, 800).plan_cold_launch(CliDriver::Other, "weird", true),
            ("weird".to_string(), None)
        );
    }

    #[test]
    fn codex_and_cursor_tui_never_consume_rpc_handoff_strict_owner() {
        for (cli, channel, sid) in [
            ("codex", "codex", "thread-strict"),
            ("cursor", "cursor-acp", "session-strict"),
        ] {
            let (agent, _tmp) = crate::test_support::agent_dir(cli, "");
            let session_name = format!("soul-test-strict-{cli}");
            let path = resume_store::record_path(&agent, &session_name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                summon_core::session_id::format_strict_record(
                    channel,
                    sid,
                    Some(agent.dir.to_str().unwrap()),
                    None,
                ),
            )
            .unwrap();

            let error = Session::with_name(&agent, session_name, 10, 10)
                .ensure_started()
                .expect_err("TUI 必须在探测/冷启前拒绝 strict RPC owner");
            assert!(
                error.to_string().contains("strict-resume"),
                "实得:{error:#}"
            );
            assert!(
                resume_store::read_record(&agent, &format!("soul-test-strict-{cli}"))
                    .unwrap()
                    .strict_resume,
                "拒绝后不得清 strict owner"
            );
        }
    }

    /// 真机集成测试的 RAII 清理(T3):断言失败 panic 时尾部清理代码不执行,会泄漏一个活着的
    /// tmux 会话(实测泄漏过 `summon-codex-it`)。guard 在 Drop 里兜底 kill + 删临时目录,
    /// 成功路径与失败路径都干净。
    struct ItCleanup {
        tmux: String,
        tmp: std::path::PathBuf,
    }
    impl Drop for ItCleanup {
        fn drop(&mut self) {
            let _ = crate::session::tmux::Tmux::new(self.tmux.clone()).kill_session();
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }

    /// 端到端回归(2026-08-28 的截断 bug):一条 3 KB 的 prompt 注入 claude TUI 后,转录里的
    /// user 消息必须**逐字节完整**。那天有三条飞书消息掐掉开头才进 claude,而送达判据只找位于
    /// prompt 末尾的 transport marker,marker 还在 → 判成功,于是 agent 收到一条掐头的消息却
    /// 没有任何人报错。断言首/中/尾三个探针都在:单验尾部是抓不到这类损坏的。
    ///
    /// 会话由本测试自己拉起(不走 `ensure_started`):要验的是注入原语在真 claude TUI 上的
    /// 完整性,冷启编排另有测试覆盖,混进来只会让失败原因变模糊。
    /// 需 tmux + 已登录 claude;会真跑一轮(prompt 里要求只回 OK)。
    #[test]
    #[ignore]
    fn a_large_prompt_reaches_the_symlink_cwd_transcript_with_its_head_intact() {
        let tmp = std::env::temp_dir().join(format!("summon-paste-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // 转录目录名是按 claude 眼里的 cwd munge 出来的,macOS 上那是 `/private/var/...`;
        // 拿 `temp_dir()` 的 `/var/...` 去推导会指向一个永远不存在的文件。
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let linked_cwd = tmp.join("work-link");
        let real_cwd = tmp.join("real-work");
        std::fs::create_dir(&real_cwd).unwrap();
        std::os::unix::fs::symlink(&real_cwd, &linked_cwd).unwrap();
        let yaml = format!(
            "name: itp\nruntime:\n  cli: claude\n  working_dir: {}\n  tmux_name: summon-paste-it-{}\n",
            linked_cwd.display(),
            std::process::id()
        );
        let mut a: Agent = serde_yaml::from_str(&yaml).unwrap();
        a.dir = tmp.clone();
        let sess = Session::for_agent(&a, 400, 1200);
        let _cleanup = ItCleanup {
            tmux: sess.tmux.session.clone(),
            tmp: tmp.clone(),
        };
        let _ = sess.tmux.kill_session();

        let sid = uuid::Uuid::new_v4().to_string();
        sess.tmux
            .new_session(
                &linked_cwd.display().to_string(),
                &format!("claude --session-id {sid}"),
            )
            .unwrap();
        sleep(Duration::from_secs(3));
        assert!(
            sess.tmux.has_session(),
            "claude 启动即退出,cwd={} pane={:?}",
            tmp.display(),
            sess.tmux.capture_visible()
        );
        // 新目录必然弹信任门。走生产那条路消费它 —— 测试自己维护一份「信任门长什么样」的认知,
        // 就会有两份各自过期的判据,而过期的那份按下去是「No, exit」。真实 agent 的 working_dir
        // 早已信任,这一步只是把测试拉到与生产同一起点。
        sess.accept_trust_prompt(CliDriver::Claude).unwrap();
        let ready_by = Instant::now() + Duration::from_secs(60);
        loop {
            let pane = sess.tmux.capture_visible().unwrap_or_default();
            assert!(
                Instant::now() < ready_by,
                "60s 内 claude TUI 没就绪,pane:\n{pane}"
            );
            if claude_startup(&pane) == ClaudeStartup::Ready {
                break;
            }
            sleep(Duration::from_millis(500));
        }

        let filler: String = (0..40)
            .map(|i| format!("第{i:03}段中文正文内容用于把这条消息撑到真实量级。"))
            .collect();
        let prompt =
            format!("HEADPROBE {filler} MIDPROBE {filler} 请只回复 OK 两个字。\n\nTAILPROBE");
        assert!(prompt.len() > 3000, "样本要够大才复现得了");
        let path =
            crate::capture::resolve_transcript_path(&a, &sess.tmux.session, None, None, &sid)
                .unwrap();
        assert_eq!(
            path,
            summon_core::claude_log::claude_transcript_path(
                &real_cwd.canonicalize().unwrap().to_string_lossy(),
                &sid
            )
            .unwrap()
        );
        sess.send_line_for_driver(&prompt, InjectGate::PromptQueue)
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(60);
        let mut seen = String::new();
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if text.contains("TAILPROBE") {
                    seen = text;
                    break;
                }
            }
            sleep(Duration::from_millis(300));
        }
        assert!(!seen.is_empty(), "60s 内转录里没出现本轮 prompt");
        for probe in ["HEADPROBE", "MIDPROBE", "TAILPROBE"] {
            assert!(seen.contains(probe), "{probe} 不在转录里 —— prompt 被截了");
        }
    }

    /// 端到端:claude 冷启记录 id → 杀 tmux 模拟死亡 → 再起应 resume 且复用同一 id。
    /// 需 tmux + 已登录 claude；SUMMON_TEST_BIN 指向同一源码构建的 summon CLI。
    #[test]
    #[ignore]
    fn claude_cold_then_resume_reuses_session_id() {
        let tmp = std::env::temp_dir().join(format!("summon-resume-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let yaml = format!(
            "name: itc\nruntime:\n  cli: claude\n  working_dir: {}\n  tmux_name: summon-resume-it\n  seed: \"say ok\"\n",
            tmp.display()
        );
        let mut a: Agent = serde_yaml::from_str(&yaml).unwrap();
        a.dir = tmp.clone();
        let sess = Session::for_agent(&a, 400, 1200);
        let _cleanup = ItCleanup {
            tmux: sess.tmux.session.clone(),
            tmp: tmp.clone(),
        };
        let _ = sess.tmux.kill_session();

        let start = sess.ensure_started().unwrap();
        assert_eq!(start, Start::ColdStarted);
        let id1 = resume_store::read_id(&a, &sess.tmux.session, "claude").expect("冷启应记录 id");

        let already_running = sess.ensure_started().unwrap();
        assert_eq!(already_running, Start::AlreadyRunning);

        sess.tmux.kill_session().unwrap();
        assert!(!sess.tmux.has_session());

        let start2 = sess.ensure_started().unwrap();
        assert_eq!(start2, Start::Resumed, "tmux 死后应 resume 续接");
        let id2 = resume_store::read_id(&a, &sess.tmux.session, "claude").unwrap();
        assert_eq!(id1, id2, "resume 应复用同一 session id");

        // 清理由 _cleanup guard 兜底(panic 路径同样生效)。
    }

    /// 端到端:存了个不存在的 id,resume 起不来 → 自动兜底冷启并覆盖坏 id。
    /// 需 tmux + 已登录 claude;默认 ignore。
    #[test]
    #[ignore]
    fn claude_stale_id_falls_back_to_cold_start() {
        let tmp = std::env::temp_dir().join(format!("summon-stale-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let yaml = format!(
            "name: its\nruntime:\n  cli: claude\n  working_dir: {}\n  tmux_name: summon-stale-it\n  seed: \"say ok\"\n",
            tmp.display()
        );
        let mut a: Agent = serde_yaml::from_str(&yaml).unwrap();
        a.dir = tmp.clone();
        let sess = Session::for_agent(&a, 400, 1200);
        let _cleanup = ItCleanup {
            tmux: sess.tmux.session.clone(),
            tmp: tmp.clone(),
        };
        let _ = sess.tmux.kill_session();

        let bogus = "00000000-0000-4000-8000-000000000000";
        resume_store::write(&a, &sess.tmux.session, "claude", bogus, None, None);

        let start = sess.ensure_started().unwrap();
        assert_eq!(start, Start::ColdStarted, "stale id 应兜底冷启");
        let id = resume_store::read_id(&a, &sess.tmux.session, "claude").unwrap();
        assert_ne!(id, bogus, "应换成一个全新可用的 id");

        // 清理由 _cleanup guard 兜底(panic 路径同样生效)。
    }

    /// 端到端:cursor 冷启(create-chat 取 id + `--resume=` 启)→ 杀 tmux → 再起应 resume 同 id。
    /// 需 tmux + 已登录 cursor-agent;默认 ignore。
    #[test]
    #[ignore]
    fn cursor_cold_then_resume_reuses_session_id() {
        let tmp = std::env::temp_dir().join(format!("summon-cursor-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let yaml = format!(
            "name: itcur\nruntime:\n  cli: cursor\n  cli_cmd: cursor-agent\n  working_dir: {}\n  tmux_name: summon-cursor-it\n  seed: \"say ok\"\n",
            tmp.display()
        );
        let mut a: Agent = serde_yaml::from_str(&yaml).unwrap();
        a.dir = tmp.clone();
        let sess = Session::for_agent(&a, 400, 1200);
        let _cleanup = ItCleanup {
            tmux: sess.tmux.session.clone(),
            tmp: tmp.clone(),
        };
        let _ = sess.tmux.kill_session();

        let start = sess.ensure_started().unwrap();
        assert_eq!(start, Start::ColdStarted);
        let id1 = resume_store::read_id(&a, &sess.tmux.session, "cursor").expect("冷启应记录 id");

        sess.tmux.kill_session().unwrap();
        assert!(!sess.tmux.has_session());

        let start2 = sess.ensure_started().unwrap();
        assert_eq!(start2, Start::Resumed, "tmux 死后应 resume 续接");
        let id2 = resume_store::read_id(&a, &sess.tmux.session, "cursor").unwrap();
        assert_eq!(id1, id2, "resume 应复用同一 session id");

        // 清理由 _cleanup guard 兜底(panic 路径同样生效)。
    }

    /// 端到端:codex 冷启(启动后回捞最新 rollout 的 id)→ 杀 tmux → 再起应 resume 同 id。
    /// 需 tmux + 已登录 codex;默认 ignore。
    #[test]
    #[ignore]
    fn codex_cold_then_resume_reuses_session_id() {
        let tmp = std::env::temp_dir().join(format!("summon-codex-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let yaml = format!(
            "name: itcdx\nruntime:\n  cli: codex\n  cli_cmd: codex\n  working_dir: {}\n  tmux_name: summon-codex-it\n  seed: \"say ok\"\n",
            tmp.display()
        );
        let mut a: Agent = serde_yaml::from_str(&yaml).unwrap();
        a.dir = tmp.clone();
        let sess = Session::for_agent(&a, 400, 1200);
        let _cleanup = ItCleanup {
            tmux: sess.tmux.session.clone(),
            tmp: tmp.clone(),
        };
        let _ = sess.tmux.kill_session();

        let start = sess.ensure_started().unwrap();
        assert_eq!(start, Start::ColdStarted);
        let id1 =
            resume_store::read_id(&a, &sess.tmux.session, "codex").expect("codex 冷启应回捞 id");

        sess.tmux.kill_session().unwrap();
        assert!(!sess.tmux.has_session());

        let start2 = sess.ensure_started().unwrap();
        assert_eq!(start2, Start::Resumed, "tmux 死后应 resume 续接");
        let id2 = resume_store::read_id(&a, &sess.tmux.session, "codex").unwrap();
        assert_eq!(id1, id2, "resume 应复用同一 session id");

        // 清理由 _cleanup guard 兜底(panic 路径同样生效)。
    }

    /// 回归(2026-08-05 flare-5 真机故障):CLI 起来即退(work_dir 进不去等)时 tmux 会话随之消亡,
    /// 启动期等待必须**立刻**认出并如实报「会话已消亡」,而不是对着尸体空转满 ready_timeout。
    /// 现场代价:每条消息空转 62s(trust 20s + ready 40s)才报「等待就绪超时」,错误方向还指错了地方
    /// (让人以为是 CLI 起得慢/就绪判定失配,实际会话压根没活过来)。
    ///
    /// 判据与 `pane_proves_session_gone` 保持同一套:空 pane 只是省 fork-exec 的前置筛,
    /// `has_session` 才是权威信号 —— `capture` 对死会话返回 `Ok("")` 是有意设计,不能靠它判。
    #[test]
    #[ignore = "需要真实 tmux"]
    fn startup_wait_fails_fast_when_session_died() {
        let a = agent_named("waitready");
        let sess = Session::with_name(&a, "summon-test-diedstart".to_string(), 200, 800);
        let _ = sess.tmux.kill_session();
        // 起一个立刻退出的"CLI":会话建起来后随即消亡,精确复现 exec 失败 / CLI 启动即退。
        sess.tmux
            .new_session(".", "/bin/sh -c 'exit 1'")
            .expect("测试会话应能建起来");
        sleep(Duration::from_millis(600));
        assert!(!sess.tmux.has_session(), "程序退出后 tmux 会话应已消亡");

        let t0 = Instant::now();
        let err = sess
            .wait_ready(Duration::from_secs(40))
            .expect_err("对已消亡的会话不该等满超时才失败");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "应快速失败,实测 {:?}",
            t0.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("会话"), "错误应指向会话消亡,实得: {msg}");
        assert!(!msg.contains("就绪超时"), "不该再报成就绪超时,实得: {msg}");
    }

    /// 同一判据也必须覆盖信任提示等待:它排在 `wait_ready` 之前,会话死在这一步时
    /// 同样不能空转满 20s 再把控制权交给下一个空转。
    #[test]
    #[ignore = "需要真实 tmux"]
    fn trust_prompt_wait_fails_fast_when_session_died() {
        let a = agent_named("trustwait");
        let sess = Session::with_name(&a, "summon-test-diedtrust".to_string(), 200, 800);
        let _ = sess.tmux.kill_session();
        sess.tmux
            .new_session(".", "/bin/sh -c 'exit 1'")
            .expect("测试会话应能建起来");
        sleep(Duration::from_millis(600));
        assert!(!sess.tmux.has_session());

        let t0 = Instant::now();
        let err = sess
            .accept_trust_prompt(CliDriver::Claude)
            .expect_err("对已消亡的会话不该空等满信任窗口");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "应快速失败,实测 {:?}",
            t0.elapsed()
        );
        assert!(err.to_string().contains("会话"), "实得: {err}");
    }

    /// 切出 `fork_start` 的函数体。**切法必须能证伪** —— 切空了断言就恒真,
    /// 所以这里对切出的内容做最小完整性检查。
    fn fork_start_body(code: &str) -> &str {
        let start = code.find("fn fork_start(").expect("fork_start 必须在");
        let body = &code[start..];
        let end = body
            .find("\n    /// 冷启:")
            .expect("fork_start 之后应当紧跟 cold_start_and_record 的 doc");
        let body = &body[..end];
        assert!(
            body.contains("fork_cmd_with_id"),
            "切出来的不是 fork_start 的函数体,这几条守卫会恒真"
        );
        body
    }

    /// fork **只能经 `start_tmux_with_mirror_muted` 起会话**,而那个入口**真的**会顶水位。
    ///
    /// 两条断言分工:结构那条保证这条路上没有别的起会话入口(漏顶水位不可表达);
    /// 行为那条真调一次入口、真读 `AtomicU64`,保证入口里那行不是摆设 —— 字符串断言挡不住
    /// 「把顶水位包进一个恒假分支」这种中和,只有真读值才挡得住。
    ///
    /// 失守的后果是 CLAUDE.md 第二红线:claude 在启动那一刻写进子转录的几十 KB 父历史被当成
    /// 本轮新产出发进目标飞书群,**撤不回来**。
    #[test]
    fn a_fork_can_only_start_a_session_through_the_mirror_muted_entry() {
        let code = production_of(include_str!("session.rs"));
        let body = fork_start_body(code);
        assert!(
            body.contains("self.start_tmux_with_mirror_muted("),
            "fork 必须走顶了水位的那个入口"
        );
        assert!(
            !body.contains("self.start_tmux("),
            "fork 路不许直接起 tmux —— 那条路绕过了顶水位,父历史会被发进群"
        );
    }

    /// 入口**真的**把水位顶到 MAX —— 在起会话之前。
    ///
    /// 故意让起会话失败(空命令 + 不存在的目录):水位必须在那次尝试**之前**就已经顶起来,
    /// 所以起没起来不影响这条断言。这是行为判据,不是 grep。
    #[test]
    fn the_mirror_muted_entry_raises_the_watermark_before_it_tries_to_start() {
        let agent = agent_named("probe");
        let wm = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(7));
        let session = Session::with_name(&agent, "soul-probe-mirror-muted".into(), 10, 10)
            .with_mirror_watermark(std::sync::Arc::clone(&wm));
        // 起会话必然失败(目录不存在),返回值不看。
        let _ = session.start_tmux_with_mirror_muted("/nonexistent-dir-for-probe", "false");
        assert_eq!(
            wm.load(std::sync::atomic::Ordering::Relaxed),
            u64::MAX,
            "水位必须在起会话之前就顶到 MAX —— 否则父历史会被当成本轮产出发进群"
        );
    }

    /// fork 收尾时水位必须落到**转录当前长度**,而不是留在 MAX 或落回 0。
    ///
    /// 留在 MAX = 这个会话的镜像从此整段哑掉;落回 0 = 副本历史全部变成待转发 backlog。
    /// 定位不到转录时保持 MAX 是刻意的:宁可哑掉,也不能把来源不明的历史发进群。
    #[test]
    fn a_fork_settles_the_watermark_at_the_copied_transcript_length() {
        let code = production_of(include_str!("session.rs"));
        let body = fork_start_body(code);
        assert!(
            body.contains("resolve_seed_transcript"),
            "收尾要定位副本转录才知道水位落到哪"
        );
        assert!(
            body.contains("meta.len()"),
            "水位落到转录当前长度(「到这为止是副本」)"
        );
        assert!(
            !body.contains("wm.store(0"),
            "绝不能落回 0 —— 那会让副本继承的整段历史变成待转发 backlog"
        );
    }

    /// fork 起来的会话**必须带着 PRIMER**,而 `fork_start` 自己不写那个文件 —— 它靠
    /// `augment_launch` 里的 `system_prompt_file_flag`,而那一支只在文件已存在时才追加。
    ///
    /// 文件由调用方(`chat_thread` 主路径)在 `ensure_started()` **之前**写好。这条依赖是隐式的:
    /// 把两者调换顺序,fork 出的会话就悄悄没有身份 —— 它不知道自己是谁、不知道飞书排版约定,
    /// 而这不会让任何现有测试变红。所以这里钉住「fork 路仍然经过 augment_launch」。
    #[test]
    fn a_forked_session_still_goes_through_augment_launch_for_its_primer() {
        let code = production_of(include_str!("session.rs"));
        let body = fork_start_body(code);
        assert!(
            body.contains("self.augment_launch(driver, cmd)"),
            "fork 的启动命令必须过 augment_launch —— 否则 PRIMER / 权限 flag / 身份隔离全都挂不上"
        );
    }

    /// fork 起不来时**不清记录、不 kill**。
    ///
    /// 与 `KillStaleThenCold` 相反:那条路的前提是「resume 起不来说明 id 已 stale,记录没用了」;
    /// fork 起不来可能只是源会话此刻正被 claude 自己写着,清掉记录就把两个群刚谈成的那次 fork
    /// 丢了,而它下一条消息本来就能成功。
    #[test]
    fn a_failed_fork_start_keeps_the_record_and_never_kills_anything() {
        let code = production_of(include_str!("session.rs"));
        let body = fork_start_body(code);
        assert!(
            !body.contains("kill_session"),
            "fork 路不得销毁 tmux 会话(第一红线)"
        );
        assert!(
            !body.contains("resume_store::clear("),
            "fork 起不来不清记录 —— 那会丢掉用户已经授权过的 fork 意图"
        );
        assert!(
            !body.contains("clear_chat("),
            "fork 起不来不清记录 —— 那会丢掉用户已经授权过的 fork 意图"
        );
    }
}

#[cfg(test)]
mod inject_latency_tests {
    use super::*;

    /// **多行注入的补 Enter 间隔不许复用 `startup_grace`**(owner 2026-08-10 实测:
    /// 「插入一条消息很晚才出现在 tmux 里」)。
    ///
    /// 那个常量是给「CLI 进程冷启」用的 1500ms 量级,而这里要等的只是 TUI 处理完一次粘贴的
    /// 渲染一拍。复用它 = 每条多行消息固定多花 3 秒,而 steer 注入的 prompt 是多条消息
    /// `join("\n")`,只要带引用/附件说明或一次注入多条就必然命中。
    #[test]
    fn 多行提交间隔远小于冷启宽限() {
        let startup = summon_core::driver::CliDriver::Claude.startup_grace();
        assert!(
            MULTILINE_SUBMIT_GRACE * (MULTILINE_SUBMIT_RETRIES as u32) < startup,
            "补 Enter 的总成本({:?} × {}) 必须显著小于一次冷启宽限({startup:?})",
            MULTILINE_SUBMIT_GRACE,
            MULTILINE_SUBMIT_RETRIES
        );
        // 上界:总延迟不该超过半秒,否则 steer 又会变成"很晚才出现"。
        assert!(
            MULTILINE_SUBMIT_GRACE * (MULTILINE_SUBMIT_RETRIES as u32)
                <= Duration::from_millis(500)
        );
        // 下界:保留两次冗余(防紧跟 Enter 被竞态吞掉),不许退化成一次。
        const {
            assert!(
                MULTILINE_SUBMIT_RETRIES >= 2,
                "两次补发是防竞态的冗余,不能删"
            );
        }
    }
}
