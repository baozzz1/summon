//! workflow step 的 **worker 临时 run**。
//!
//! 一个 workflow step = 起 `<worker>` 一次**临时 run**,按其 cli 模式分流(收口在 `WorkerLaunch`
//! 枚举,别在多处散落 `match capture_mode`):
//! - **cursor/codex → 无头一次性**:`headless_capture_once` 起一次性子进程(`headless_command`
//!   落各 CLI flag),冷启一轮、读 stdout 取 result。
//! - **claude → 临时交互 run**:建临时 tmux(`Session::with_name`,名带 `worker-<8hex>`,与持久
//!   `soul-<alias>` 天然不撞)→ 启 claude → prime(SOUL/精简系统提示落
//!   `--append-system-prompt-file`)→ 注入子任务 → Transcript 抓 `end_turn` 取 result →
//!   **主动拆会话**(临时,用完即 kill;绝不碰别人的持久会话)。
//!
//! **共享父 work_dir**:worker 跑在父的 work_dir(链可接力、并行面向读/分析)。并行不加写锁
//! (面向读;并发写自行规避)。

use anyhow::Context;
use summon_core::registry::{Agent, CaptureMode};

use crate::capture::{Subprocess, Transcript};
use crate::runtime::executor::LocalExecutor;
use crate::runtime::inflight::InFlightTurn;
use crate::session::session::Session;

/// worker 一次临时 run 的产出:终态回复文本(空=worker 没给文本回复)。
pub struct WorkerOutcome {
    pub reply: Option<String>,
}

/// 跑一个 worker 子任务一次,返回它的 result 文本。
///
/// 入参:
/// - `worker`:子 agent 的身份(SOUL/人格/cli 都来自它)。
/// - `subtask`:子任务正文(链里 `{previous}` 已由调用方替换好)。
/// - `shared_work_dir`:父的 work_dir(覆盖 worker 自己的;None=用 worker 默认)。
/// - `worker_env`:注入子进程的 env(由调用方给,本模块只透传给子进程 / tmux 前缀)。
/// - `poll`/`debounce`:claude 临时 tmux 的轮询/防抖参数。
///
/// best-effort 拆会话:claude 临时 tmux 用完 kill;失败仅 log(不影响已取到的 result)。
pub fn run_worker_once(
    worker: &Agent,
    subtask: &str,
    shared_work_dir: Option<&str>,
    worker_env: &[(String, String)],
    poll: u64,
    debounce: u64,
    // 可选 InFlight 注册:Some 时登记 killer,停机时 SIGINT 整组避免孤儿进程组泄漏。
    inflight_key: Option<(
        crate::runtime::inflight::InFlight,
        crate::runtime::inflight::TurnKey,
    )>,
) -> anyhow::Result<WorkerOutcome> {
    match worker.capture_mode() {
        // Rpc 也走 exec:临时 worker 是**一次性**身份(冷启一轮、无 resume、跑完即弃),
        // 常驻协议省下的那点冷启开销在这里等于零,却要多引一个跨轮会话池 —— 不值当。
        CaptureMode::Rpc | CaptureMode::CliSession => {
            run_worker_headless(worker, subtask, shared_work_dir, worker_env, inflight_key)
        }
        CaptureMode::Transcript => {
            run_worker_transient_tmux(worker, subtask, shared_work_dir, worker_env, poll, debounce)
        }
    }
}

/// cursor/codex(及显式 headless 的 claude):无头一次性子进程。冷启一轮(无 resume),
/// PRIMER 前置进 prompt(worker 临时身份,无续接上下文,故每次都 prime)。
fn run_worker_headless(
    worker: &Agent,
    subtask: &str,
    shared_work_dir: Option<&str>,
    worker_env: &[(String, String)],
    inflight_key: Option<(
        crate::runtime::inflight::InFlight,
        crate::runtime::inflight::TurnKey,
    )>,
) -> anyhow::Result<WorkerOutcome> {
    let driver = summon_core::driver::CliDriver::from_kind(worker.cli_kind());
    // worker 是临时身份、无 resume:PRIMER 注入决策收口 CliDriver(A3),一次性路径
    // prefer_file=false → 前置进首条 prompt。
    let (_, primed) =
        driver.compose_first_prompt(system_prompt_text(worker).as_deref(), subtask, false, false);
    // 共享父 work_dir 覆盖 worker 自己的;父没给则用 worker 默认。
    let work_dir = shared_work_dir
        .map(str::to_string)
        .or_else(|| worker.working_dir());
    let mut reply: Option<String> = None;
    let mut on_message = |body: &str| reply = Some(body.to_string());
    let mut on_stream = |_acc: &str| {};
    let mut on_tool = |_tool: &str| {}; // 工具日志由 on_event 统一打(含入参)
    let mut on_event = |ev: &summon_core::agent_msg::AgentMessage| {
        use summon_core::agent_msg::AgentKind;
        if matches!(ev.kind, AgentKind::ToolUse) {
            if let Some(tool) = ev.tool.as_deref() {
                use crate::runtime::daemon::tool_input_brief;
                use summon_core::tool_detail::tool_command_detail;
                let cmd = tool_command_detail(ev.tool_input.as_ref());
                match cmd {
                    Some(c) => {
                        crate::observe::log::log(&summon_i18n::format!("r3670865876", worker.name))
                    }
                    None => {
                        let input_summary = ev
                            .tool_input
                            .as_ref()
                            .map(tool_input_brief)
                            .unwrap_or_default();
                        crate::observe::log::log(&summon_i18n::format!("rfe3f658d40", worker.name));
                    }
                }
            }
        }
    };
    // program 解析(cli_cmd 覆盖)、SpawnSpec(stdin_null/own_process_group)、FEISHU_* 身份
    // 隔离注入都收在 `headless_capture_once` 一处,不再手抄。
    // 传 inflight_key:Some 时 on_spawn 登记 killer,停机时 shutdown_interrupt_all 能打断。
    headless_capture_once(
        worker,
        &primed,
        None,
        work_dir,
        worker_env.to_vec(),
        &mut on_message,
        &mut on_stream,
        &mut on_tool,
        &mut on_event,
        inflight_key.as_ref().map(|(i, k)| (i, k.clone())),
        summon_core::pending::PendingTurnKind::Worker,
    )
    .context(summon_i18n::text!("rf2f9ef0ee0"))?;
    Ok(WorkerOutcome { reply })
}

/// claude → 临时交互 run:建临时 tmux → 启 claude → prime(SOUL 落 sys-prompt 文件)→ 注入子任务 →
/// Transcript 抓 `end_turn` 取 result → 主动拆会话(临时,用完即 kill)。
///
/// **panic 安全**:`TransientSessionGuard` 在 drop 时兜底拆会话,即便 `run_turn` panic / 线程 unwind,
/// 临时 tmux 也不会泄漏成孤儿会话。
fn run_worker_transient_tmux(
    worker: &Agent,
    subtask: &str,
    shared_work_dir: Option<&str>,
    worker_env: &[(String, String)],
    poll: u64,
    debounce: u64,
) -> anyhow::Result<WorkerOutcome> {
    let tmux_name = transient_tmux_name(&worker.name);
    let sess = Session::with_name(worker, tmux_name.clone(), poll, debounce)
        .with_work_dir(shared_work_dir)
        .with_run_env(worker_env.to_vec());
    // RAII guard:无论正常返回还是 panic,drop 时都拆临时会话(teardown_transient 内 has_session 判空)。
    let _guard = TransientSessionGuard::new(tmux_name.clone(), worker.name.clone());
    // PRIMER(SOUL/精简系统提示)落盘供 claude 启动期 --append-system-prompt-file 注入。
    if let Some(primer) = system_prompt_text(worker) {
        if let Err(e) = sess.write_system_prompt_file(&primer) {
            crate::observe::log::log(&summon_i18n::format!("rdab504d639", worker.name));
        }
    }
    let started = sess
        .ensure_started()
        .context(summon_i18n::text!("r3bac9e1015"));
    let outcome = started.and_then(|_| {
        let mut reply: Option<String> = None;
        let mut on_message = |body: &str| reply = Some(body.to_string());
        let mut on_stream = |_acc: &str| {};
        let mut on_tool = |tool: &str| {
            crate::observe::log::log(&summon_i18n::format!("r1d1a4c5c29", worker.name));
        };
        let mut on_event = |_ev: &summon_core::agent_msg::AgentMessage| {};
        Transcript::new(&sess, LocalExecutor)
            .run_turn(
                subtask,
                crate::capture::TurnInjection::Correlated,
                &mut on_message,
                &mut on_stream,
                &mut on_tool,
                &mut on_event,
                // worker 临时 run 不做交互中介:没有卡,也就没有可重发的东西。
                &mut |_| crate::capture::MediationCard::Settled,
                &mut || None,      // worker 临时 run 不接 mid-turn 打断/steer
                &mut || {},        // worker 临时 run 无对应飞书消息,不贴表情
                &mut |_, _, _| {}, // worker 临时 run 不参与 chat mirror marker 仲裁
                &mut |_| {},       // worker 临时 run 不记录 marker 消费边界
                &mut || true,      // worker 临时 run 没有并发 mirror,主路径恒取得输出权
                &mut || {},        // worker 临时 run 没有 mirror terminal tracker
                &mut |_, _, _| {}, // 同上:没有 mirror 等主路径的结论
                &mut |_| {},       // worker 临时 run 不接 steer
                &mut |_| {},       // worker 临时 run 不接 steer,亦无 mirror steer marker 登记
                &mut |_| {},       // worker 临时 run 无 live 卡,不需要计时心跳
                &mut |_| {},       // worker 临时 run 不直发后台任务通知
                &mut || {},        // worker 临时 run 不落 mirror 续跑 claim(无 mirror)
                &mut |_| false,    // worker 临时 run 不跨后台任务续跑,没有段终点回复
                &mut |_| false,    // worker 独占临时会话，没有 chat 队列需要让出
                // worker 是临时会话,不跨后台任务续跑:ZERO 让首个 end_turn 即收尾(与原行为一致)。
                std::time::Duration::ZERO,
            )
            .context(summon_i18n::text!("r4885037563"))?;
        Ok(WorkerOutcome { reply })
    });
    outcome
}

/// 临时 worker tmux 会话名:`soul-<alias>-worker-<8hex>`。前缀 `worker-` 让它与持久会话
/// (`soul-<alias>`)、task 会话(`soul-<alias>-task-<id>`)、per-chat 会话(`soul-<alias>-<g|p>…`)
/// 天然不撞;8hex 随机后缀让同一 worker 的并行多次扇出也各占独立会话。
fn transient_tmux_name(alias: &str) -> String {
    let rand = uuid::Uuid::new_v4().simple().to_string();
    format!("soul-{alias}-worker-{}", &rand[..8])
}

/// RAII guard:drop 时兜底拆临时 worker tmux 会话。即便 `run_turn` panic / 线程 unwind,
/// 临时 tmux 也不会泄漏。正常路径也走 drop(无需显式 dismiss),`teardown_transient` 内
/// `has_session()` 判空保证幂等。
struct TransientSessionGuard {
    tmux_name: String,
    alias: String,
}

impl TransientSessionGuard {
    fn new(tmux_name: String, alias: String) -> Self {
        Self { tmux_name, alias }
    }
}

impl Drop for TransientSessionGuard {
    fn drop(&mut self) {
        teardown_transient(&self.tmux_name, &self.alias);
    }
}

/// 拆临时 worker tmux 会话(best-effort)。只 kill 本次起的临时会话名,不触碰任何持久/ task 会话。
fn teardown_transient(tmux_name: &str, alias: &str) {
    let t = crate::session::tmux::Tmux::new(tmux_name.to_string());
    if t.has_session() {
        if let Err(e) = t.kill_session() {
            crate::observe::log::log(&summon_i18n::format!("ra4a70c5217"));
        }
    }
}

/// 无头一次性子进程跑一轮(cursor/codex 及显式 headless 的 claude),**尊重 `cli_cmd` 覆盖**:
/// args 由 driver 算(`headless_command`),program 取 `agent.cli_cmd` 的二进制(单测据此指向假脚本;
/// 生产 cursor→cursor-agent 等覆盖也生效)。`resume` 给定则续接、否则冷启。PRIMER 由调用方决定是否
/// 已前置进 `prompt`(本函数不再注入 sys_prompt_file)。回调四件套同捕获引擎。返回 `TurnOutcome`
///(含 session_id)。task run 与 chat→task 蒸馏轮共用此原语,避免重复 program 解析逻辑。
#[allow(clippy::too_many_arguments)] // agent/prompt/resume/work_dir/env/四回调/可选inflight各自独立,合并无意义
fn headless_capture_once(
    agent: &Agent,
    prompt: &str,
    resume: Option<&str>,
    work_dir: Option<String>,
    env: Vec<(String, String)>,
    on_message: &mut dyn FnMut(&str),
    on_stream: &mut dyn FnMut(&str),
    on_tool: &mut dyn FnMut(&str),
    on_event: &mut dyn FnMut(&summon_core::agent_msg::AgentMessage),
    // 可选 InFlight 注册:Some 时 on_spawn 登记子进程 killer,停机时 SIGINT 整组避免孤儿。
    // worker 传 Some;chat 热路径走自己的 InFlight(在 chat_thread 里直接注册)。
    inflight: Option<(
        &crate::runtime::inflight::InFlight,
        crate::runtime::inflight::TurnKey,
    )>,
    // PendingTurn 种类:决定停机时是否落 pending.json / 入队通知。只有 Chat 种类需要;
    // Worker 是临时 run,不产孤儿文件、不误报停机通知。
    pending_kind: summon_core::pending::PendingTurnKind,
) -> anyhow::Result<crate::capture::TurnOutcome> {
    // 飞书身份隔离:叠加本 agent 自己的凭据(规范名 + alias 名);spawn 已先清继承的 FEISHU_*,
    // 故子进程里只剩自己的身份。放在传入 env(SUMMON_* 等)之后,FEISHU 键以自己的为准。
    let mut env = env;
    env.extend(crate::runtime::daemon::agent_identity_env(agent));
    let driver = summon_core::driver::CliDriver::from_kind(agent.cli_kind());
    // codex reasoning 摘要注入值(仅 codex 非 None;优先级解析见 capture helper)。
    let codex_summary = crate::capture::codex_reasoning_summary_arg(agent);
    let opts = summon_core::driver::LaunchOpts {
        resume,
        sys_prompt_file: None,
        model: agent.model(),
        effort: agent.effort(),
        codex_reasoning_summary: codex_summary.as_deref(),
        codex_service_tier: None,
        codex_skip_git_repo_check: agent.codex_skip_git_repo_check(),
    };
    let (_default_prog, args) = driver
        .headless_command(prompt, &opts)
        .unwrap_or_else(|| (driver.default_cmd().to_string(), vec![]));
    let cli_cmd = agent.cli_cmd();
    let program = cli_cmd
        .split_whitespace()
        .next()
        .unwrap_or(&cli_cmd)
        .to_string();
    let spec = crate::runtime::executor::SpawnSpec {
        program,
        args,
        work_dir,
        stdin_null: true, // codex 无头否则等 stdin 永不退出
        env,
        own_process_group: true, // 自成组,便于 killpg 整组(打断/停机不留孤儿)
    };
    // worker:传 Some(inflight, key) 时 on_spawn 登记 killer 进 InFlight,
    // 停机时 shutdown_interrupt_all 能 SIGINT 整组,避免孤儿进程组泄漏烧 provider 配额。
    // chat 热路径传 None(它在 chat_thread 里直接操作 InFlight)。
    // pending 共享锁:on_spawn 创建、on_stream 实时写 partial、on_sid 写 session_id。
    // 与 chat 热路径(chat_thread.rs:1448-1450,2068-2076)同模式——让 SIGINT 打断后 resume 有内容可续。
    use std::sync::Arc as StdArc;
    use std::sync::Mutex as StdMutex;
    use summon_core::pending::PendingTurn;
    let pending_shared: StdArc<StdMutex<Option<StdArc<StdMutex<PendingTurn>>>>> =
        StdArc::new(StdMutex::new(None));
    let ps_for_spawn = StdArc::clone(&pending_shared);
    let ps_for_stream = StdArc::clone(&pending_shared);
    let ps_for_sid = StdArc::clone(&pending_shared);
    // on_spawn 的登记结果必须传回本轮收尾：shutdown 屏障后的 late spawn 会被当场杀死，
    // 但绝不能继续按 `Ok(empty)` 推进 worker 成功态。
    let registration: StdArc<StdMutex<Option<(crate::runtime::executor::ChildKiller, bool)>>> =
        StdArc::new(StdMutex::new(None));
    let registration_for_spawn = StdArc::clone(&registration);
    let base_dir = agent.state_dir().clone();
    let key_tag = inflight.as_ref().map(|(_, k)| k.tag.clone());
    let mut on_spawn = |killer: &crate::runtime::executor::ChildKiller| {
        if let Some((inflight, key)) = &inflight {
            let p = StdArc::new(StdMutex::new(PendingTurn {
                kind: pending_kind,
                // Worker 不从飞书 chat checkpoint 恢复，不能伪造一个可投递身份。
                delivery_scope: String::new(),
                agent_name: agent.name.clone(),
                inbox_message_ids: Vec::new(),
                resume_authorized: false,
                chat_id: key.tag.clone(),
                chat_type: "task".into(), // 非 Chat 种类下无意义,保留兼容
                cli: agent.cli_kind().to_string(),
                prompt: prompt.to_string(),
                session_id: resume.map(|s| s.to_string()),
                partial: String::new(),
            }));
            if let Ok(mut slot) = ps_for_spawn.lock() {
                *slot = Some(StdArc::clone(&p));
            }
            let admitted = inflight.insert(
                key.clone(),
                InFlightTurn {
                    killer: killer.clone(),
                    base: agent.state_dir().clone(),
                    pending: p,
                    shape: crate::runtime::inflight::TurnShape::OwnedProcess,
                },
            );
            if let Ok(mut slot) = registration_for_spawn.lock() {
                *slot = Some((killer.clone(), admitted));
            }
        }
    };
    // on_stream 包装:调原始回调 + 实时写 pending.partial(打断重跑有内容可续)。
    let mut on_stream_wrapped = |acc: &str| {
        on_stream(acc);
        if let Ok(slot) = ps_for_stream.lock() {
            if let Some(p) = slot.as_ref() {
                if let Ok(mut p) = p.lock() {
                    acc.clone_into(&mut p.partial);
                }
            }
        }
    };
    let mut on_sid = |sid: &str| {
        if let Ok(slot) = ps_for_sid.lock() {
            if let Some(p) = slot.as_ref() {
                if let Ok(mut p) = p.lock() {
                    p.session_id = Some(sid.to_string());
                }
                // 增量落盘:拿到 sid = 进入「已起跑」阶段,刷新 pending.json(resume 有 sid)。
                if let Some(tag) = &key_tag {
                    if let Ok(p) = p.lock() {
                        let _ = crate::runtime::inflight::write_pending(&base_dir, tag, &p);
                    }
                }
            }
        }
    };
    let result = Subprocess::new(LocalExecutor)
        .drive(
            spec,
            &|l| driver.parse_headless_line(l),
            on_message,
            on_tool,
            &mut on_stream_wrapped,
            on_event,
            &mut on_spawn,
            &mut on_sid,
        )
        .context(summon_i18n::text!("rcaaa3f2507"));

    if let Some((inflight, key)) = &inflight {
        let registered = registration.lock().ok().and_then(|slot| slot.clone());
        match registered {
            Some((killer, admitted)) => {
                if !admitted {
                    anyhow::bail!(summon_i18n::format!("rd637874610"));
                }
                match inflight.finish(key, &killer) {
                    crate::runtime::inflight::FinishDisposition::Completed => {}
                    crate::runtime::inflight::FinishDisposition::ShutdownClaimed => {
                        anyhow::bail!(summon_i18n::format!("r21a6ab3d1d"))
                    }
                    crate::runtime::inflight::FinishDisposition::Missing => {
                        anyhow::bail!(summon_i18n::format!("rb1a583a281"))
                    }
                }
            }
            None if result.is_ok() => anyhow::bail!(summon_i18n::format!("rc1094ec167")),
            None => {}
        }
    }
    result
}

/// 取该 agent 的 PRIMER 文本(messaging system_prompt,填 emoji 占位),worker 首轮前置用。
/// best-effort:任一步失败回 None(无 PRIMER 也能跑,只是少了开场约束)。
fn system_prompt_text(agent: &Agent) -> Option<String> {
    let m = crate::runtime::daemon::resolve_messaging(agent);
    // worker 不属于任何 chat,只有前三层 —— per-chat 覆盖对它没有落点。
    let settings = summon_core::settings::ResolvedSettings::resolve(&m, &Default::default());
    // 表情候选文件按内容寻址落盘,路径要真实存在:PRIMER 里那句「完整列表见本地文件」
    // 指向一个不存在的路径,比不给这句更糟。
    //
    // 落盘失败**只牺牲这一段**,绝不连累整份 PRIMER —— 后者还载着 SOUL、输出约定、附件协议等
    // 全部内置协议,为一个表情候选表把它们一起丢掉是极不成比例的失败方向。开关关掉时那一段
    // 根本不进 PRIMER,连写都不必写。
    let emoji = settings
        .agent_reactions()
        .then(|| {
            match crate::runtime::daemon::write_emoji_file(
                agent,
                &m.reactions,
                &m.reaction_meanings,
                [&m.working_reaction, &m.done_reaction],
            ) {
                Ok(path) => Some(path),
                Err(error) => {
                    crate::observe::log::log(&summon_i18n::format!("r34e293ee54", agent.name));
                    None
                }
            }
        })
        .flatten();
    // worker 是一次性子任务,没有可回答提问的人,不给 `summon ask` 段。
    let filled = m.system_prompt_filled(
        &emoji
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        false,
        summon_core::config::PrimerSections {
            reactions: emoji.is_some(),
            ..settings.primer_sections()
        },
    );
    (!filled.trim().is_empty()).then_some(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// worker 的 PRIMER 同样按 agent 级开关取舍 feed 段。表情段关着,免得测试去落表情候选文件。
    #[test]
    fn worker_primer_follows_the_feed_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let feed = summon_core::config::builtin_messaging()
            .system_prompt_feed
            .trim_end()
            .to_string();
        let primer = |agent_feed: bool| {
            let manifest = format!(
                "name: w\nruntime:\n  cli: claude\nmessaging:\n  agent_reactions: false\n  \
                 agent_feed: {agent_feed}\n"
            );
            let mut agent: Agent = serde_yaml::from_str(&manifest).unwrap();
            agent.dir = tmp.path().join(format!("feed-{agent_feed}"));
            std::fs::create_dir_all(&agent.dir).unwrap();
            std::fs::write(agent.manifest_path(), manifest).unwrap();
            system_prompt_text(&agent).unwrap()
        };

        assert!(primer(true).contains(&feed));
        assert!(!primer(false).contains(&feed));
    }

    #[test]
    fn transient_name_is_distinct_from_persistent() {
        let n = transient_tmux_name("ambrose");
        assert!(n.starts_with("soul-ambrose-worker-"));
        // 与持久会话名不撞 —— 临时 worker 用完即拆,撞名就等于拆掉别人的持久会话。
        assert_ne!(n, "soul-ambrose");
        // 两次取名不同(随机后缀)→ 同 worker 并行多扇出各占独立会话。
        assert_ne!(
            transient_tmux_name("ambrose"),
            transient_tmux_name("ambrose")
        );
    }

    /// 目录必须**真实存在**才会被采纳(见 `summon_core::work_dir`),故用真 tempdir;
    /// 断言前先 canonicalize —— macOS 的 `/var` 实际是 `/private/var`,启动目录按真身走。
    #[test]
    fn transcript_worker_inherits_parent_work_dir() {
        let parent = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        let worker: Agent = serde_yaml::from_str(&format!(
            "name: claudebot\nruntime:\n  cli: claude\n  working_dir: {}\n",
            fallback.path().display()
        ))
        .unwrap();
        let sess = Session::with_name(&worker, "worker-test".into(), 100, 100)
            .with_work_dir(Some(&parent.path().to_string_lossy()));
        let want = std::fs::canonicalize(parent.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(sess.effective_start_work_dir().unwrap(), want);
    }

    /// 父任务目录被删掉(worktree 清理后再跑扇出)时必须回退到 worker 自己的 working_dir,
    /// 绝不能让 CLI 落到 daemon 的 cwd(家目录)去跑。
    #[test]
    fn transcript_worker_falls_back_when_parent_work_dir_is_gone() {
        let fallback = tempfile::tempdir().unwrap();
        let worker: Agent = serde_yaml::from_str(&format!(
            "name: claudebot\nruntime:\n  cli: claude\n  working_dir: {}\n",
            fallback.path().display()
        ))
        .unwrap();
        let sess = Session::with_name(&worker, "worker-test".into(), 100, 100)
            .with_work_dir(Some("/definitely/not/here"));
        let want = std::fs::canonicalize(fallback.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(sess.effective_start_work_dir().unwrap(), want);
    }

    /// 显式目录与 agent 级 working_dir 都没了 → 明确失败,而不是悄悄落到进程 cwd。
    #[test]
    fn transcript_worker_refuses_when_no_configured_dir_exists() {
        let worker: Agent = serde_yaml::from_str(
            "name: claudebot\nruntime:\n  cli: claude\n  working_dir: /worker/gone\n",
        )
        .unwrap();
        let sess = Session::with_name(&worker, "worker-test".into(), 100, 100)
            .with_work_dir(Some("/definitely/not/here"));
        let err = sess.effective_start_work_dir().unwrap_err().to_string();
        assert!(
            err.contains("工作目录都不存在"),
            "错误信息要指明是工作目录问题,实得: {err}"
        );
    }

    /// 用假 CLI 脚本驱动一次无头 worker(cursor 路径):吐固定 stream-json,断言 result 被捕获。
    /// 不依赖真 CLI:把 worker.cli_cmd 指到吐 pong 的脚本。验证「单 delegate 阻塞拿到 result」的底座。
    #[test]
    fn headless_worker_captures_reply() {
        let pong = include_str!("../../../summon-core/tests/fixtures/claude-stream-pong.jsonl");
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("out.jsonl");
        std::fs::write(&data, pong).unwrap();
        let script = dir.path().join("fakecli");
        crate::test_support::write_test_executable(
            &script,
            "#!/bin/sh\ncat \"${0%/*}/out.jsonl\"\n",
        );

        let mut worker: Agent = serde_yaml::from_str(&format!(
            "name: reviewer\nruntime:\n  cli: cursor\n  cli_cmd: {}\n",
            script.display()
        ))
        .unwrap();
        // `dir` 留空时 `state_dir()` 解析成相对路径,PRIMER 合成会把表情候选文件写进 crate 根。
        worker.dir = dir.path().join("agent");
        let env: Vec<(String, String)> = Vec::new();
        let out = run_worker_once(&worker, "看下这段代码", None, &env, 200, 800, None).unwrap();
        assert_eq!(out.reply.as_deref(), Some("pong"));
    }

    /// claude 临时交互 run(真 tmux + 真 claude CLI):起临时 `soul-…-worker-<hex>` 会话、prime、
    /// 注入子任务、Transcript 抓 end_turn 取回复,**用完拆会话**。需真 claude 订阅 + tmux,故 #[ignore]
    /// (跑法:`cargo test -p summon -- --ignored`)。验证:① 拿到非空回复;② 临时会话事后被 kill,
    /// 绝不残留(更绝不碰持久 `soul-claudebot` / task 会话)。
    #[test]
    #[ignore = "需真 claude CLI + tmux:cargo test -p summon -- --ignored"]
    fn claude_transient_worker_runs_and_tears_down_session() {
        // cli=claude → Transcript/临时交互 run 路径。让它回一个固定词便于断言。
        let worker: Agent =
            serde_yaml::from_str("name: claudebot\nruntime:\n  cli: claude\n").unwrap();
        assert_eq!(worker.capture_mode(), CaptureMode::Transcript);
        let env: Vec<(String, String)> = Vec::new();
        let out = run_worker_once(&worker, "只回复一个词:pong", None, &env, 300, 1200, None)
            .expect("claude 临时 run 应成功");
        assert!(
            out.reply.is_some_and(|r| !r.trim().is_empty()),
            "claude worker 应给出非空回复"
        );
        // 事后不应残留任何 worker- 临时会话(本测起的那个已在 run_worker_transient_tmux 末尾 kill)。
        let listed = crate::session::tmux::Tmux::new("soul-claudebot-worker-x".to_string());
        assert!(!listed.has_session(), "临时 worker 会话用完即拆,不应残留");
    }
}
