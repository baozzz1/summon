//! 子进程捕获源(cursor/codex 无头,claude `-p`):直起 CLI 子进程(完整 program+args 由
//! `driver.headless_command` 构造,prompt 已就位),按行读 stdout 的结构化 JSONL,经
//! `driver.parse_headless_line` 解析成 AgentMessage,驱动回调(on_message=最终回复、
//! on_stream=流式累积文本、on_tool=逐工具进度),捕获 session_id。不抓 TUI、不用 tmux。
//!
//! 这里是无头一轮的唯一实现处:`drive`(给定显式 program/args/parse)是核心循环,`run_turn` 先由
//! driver 算出 program/args 再走 `drive`。起进程经 `Executor`,故同一套逻辑跑本机或(将来)远端
//! 只换 executor。

use std::io::{BufRead, BufReader, Read};

use summon_core::agent_msg::{AgentKind, AgentMessage};
use summon_core::driver::CliDriver;
use summon_core::registry::Agent;

use super::{TurnCtx, TurnOutcome};
use crate::runtime::executor::{ChildKiller, Executor, SpawnSpec};

const STDERR_TAIL_LIMIT_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct StartupFailure {
    message: String,
    code: Option<i32>,
    stdout_seen: bool,
    stderr: String,
    pid: i32,
    started: std::time::SystemTime,
}

/// stderr 必须与 stdout 并发 drain，否则 CLI 写满 pipe 后会在退出前死锁。线程只保留固定大小尾部；
/// 接收端仅在「非零退出 + 无结构化输出」时读取并上报，正常轮不污染事件流。
fn spawn_stderr_tail_reader(
    stderr: Option<std::process::ChildStderr>,
) -> Option<std::sync::mpsc::Receiver<Vec<u8>>> {
    let mut stderr = stderr?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut tail = Vec::with_capacity(STDERR_TAIL_LIMIT_BYTES);
        let mut chunk = [0_u8; 4096];
        loop {
            let read = match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            tail.extend_from_slice(&chunk[..read]);
            if tail.len() > STDERR_TAIL_LIMIT_BYTES {
                let overflow = tail.len() - STDERR_TAIL_LIMIT_BYTES;
                tail.drain(..overflow);
            }
        }
        let _ = tx.send(tail);
    });
    Some(rx)
}

fn receive_stderr_tail(rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>) -> String {
    let Some(rx) = rx else {
        return String::new();
    };
    // 防逃逸孙进程仅持 stderr fd 导致 join 永久阻塞；启动错误的原进程退出后 pipe 会立即 EOF。
    let Ok(bytes) = rx.recv_timeout(std::time::Duration::from_secs(1)) else {
        return String::new();
    };
    let decoded = String::from_utf8_lossy(&bytes);
    summon_core::scrubber::strip_ansi(&decoded)
        .trim()
        .to_string()
}

/// 算出该 agent 本轮要给 codex 系 CLI 注入的 `model_reasoning_summary` 值(None=不注入)。
/// 仅 codex/traex:其它 CLI 无此轴。优先级解析(agent.yaml > 用户全局配置(traecli.toml 同构)>
/// 兜底 detailed)收口在 `driver::reasoning_summary_decision`,本函数只补「读盘」这一 IO 半。
pub(crate) fn codex_reasoning_summary_arg(agent: &Agent) -> Option<String> {
    let driver = CliDriver::from_kind(agent.cli_kind());
    if !matches!(driver, CliDriver::Codex | CliDriver::Traex) {
        return None;
    }
    let user_has = read_codex_config_declares_summary(driver);
    summon_core::driver::reasoning_summary_decision(agent.reasoning_summary(), user_has)
}

/// 用户的 codex 系全局配置是否已声明 `model_reasoning_summary`(声明了就尊重、不注入)。
/// 各 driver 的 home/配置文件名不同(codex=~/.codex/config.toml,traex=~/.trae/traecli.toml),
/// 由 `private_home`/`config_filename` 分派。best-effort:读不到(无 HOME / 无文件 / 读失败)
/// 当作未声明,回退到兜底注入,绝不阻断本轮。
fn read_codex_config_declares_summary(driver: CliDriver) -> bool {
    let (Some(home), Some(filename)) = (driver.private_home(), driver.config_filename()) else {
        return false;
    };
    match std::fs::read_to_string(home.join(filename)) {
        Ok(s) => summon_core::driver::config_declares_summary(&s),
        Err(_) => false,
    }
}

/// kill-on-drop RAII guard:持 `Child` 与可 kill 句柄。`drive` 正常走到 EOF 后 `reap()` 显式
/// 回收并解除守卫 → drop 无操作。但若读循环路径 panic/unwind(如 `on_stream` 回调 panic),
/// guard 的 `Drop` 兜底 `killpg(SIGINT)` 整组 + `wait()` 回收,绝不把子进程(及其工具孙子)漏成
/// 孤儿、也不留僵尸。
struct KillOnDrop {
    child: Option<std::process::Child>,
    killer: ChildKiller,
}

impl KillOnDrop {
    fn new(child: std::process::Child, killer: ChildKiller) -> Self {
        Self {
            child: Some(child),
            killer,
        }
    }

    /// 正常收尾:wait 回收子进程并取走 Child(drop 时不再杀)。返回退出状态(review #9:据它判异常退出)。
    fn reap(&mut self) -> Option<std::process::ExitStatus> {
        self.child.take().and_then(|mut c| c.wait().ok())
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // child 仍在 → 异常路径(panic/early return):杀整组并 wait 回收,不留孤儿/僵尸。
        if let Some(mut c) = self.child.take() {
            self.killer.interrupt();
            let _ = c.wait();
        }
    }
}

/// 子进程源:持一个 `Executor`(起进程的宿主缝)。
pub struct Subprocess<E: Executor> {
    exec: E,
}

impl<E: Executor> Subprocess<E> {
    pub fn new(exec: E) -> Self {
        Self { exec }
    }

    /// 核心循环:按 `spec` 经 executor 起进程,逐行 `parse` stdout,驱动回调,收尾返回 outcome。
    /// `run_turn`(driver 算出 program/args)与单测(显式 program/args/parse)共用此实现,故行为
    /// 单一真相源、不漂移。
    #[allow(clippy::too_many_arguments)] // 各回调职责独立(消息/工具/流式/全量事件 tee + 句柄/sid),合并反而晦涩
    pub fn drive(
        &mut self,
        spec: SpawnSpec,
        parse: &dyn Fn(&str) -> Vec<AgentMessage>,
        on_message: &mut dyn FnMut(&str),
        on_tool: &mut dyn FnMut(&str),
        on_stream: &mut dyn FnMut(&str),
        on_event: &mut dyn FnMut(&AgentMessage),
        // spawn 后立刻发布的可 kill 句柄:让 chat 线程 / 停机 hook 够得着这一轮的子进程组去打断。
        on_spawn: &mut dyn FnMut(&ChildKiller),
        // session_id 首次出现即回传(cursor ~20s、codex 立即):支持「被打断仍能 resume」的增量落盘。
        on_session_id: &mut dyn FnMut(&str),
    ) -> anyhow::Result<TurnOutcome> {
        let started = std::time::SystemTime::now();
        let mut child = self.exec.spawn(&spec)?;
        // pgid:因 own_process_group,leader==child pid;非进程组场景退化为单 pid(killpg(-pid) 仍只命中它)。
        let pgid = child.id() as i32;
        let killer = ChildKiller::new(pgid);
        on_spawn(&killer); // 立即发布句柄(早于读第一行),让上层能在任何时刻打断本轮
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r96f7ade421")))?;
        let stderr_tail_rx = spawn_stderr_tail_reader(child.stderr.take());
        // kill-on-drop 兜底:持 Child + 句柄。正常 EOF 后 reap() 显式回收 + 解除;panic/unwind 时
        // Drop 杀整组 + wait,不留孤儿/僵尸。
        let mut guard = KillOnDrop::new(child, killer);

        let mut outcome = TurnOutcome::default();
        // 流式累积:已收尾的 assistant 段落("\n\n" 分段)。段边界 = `partial=false` 终止行 或
        // 工具调用(cursor 一轮可有多段,被工具切开;claude/codex 各 assistant 块即一段)。
        // result 优先用于终态回复。
        let mut accumulated_assistant: Vec<String> = Vec::new();
        // 进行中的当前段:经 `merge_segment` 归并(去重 cursor 的段重发 / 兼容老版 token delta),
        // 段终止时提交进 accumulated_assistant 并清空。claude/codex 每块 partial=false 即进即出,buf 恒空。
        let mut stream_buf = String::new();
        let mut result_text: Option<String> = None;

        let mut stdout_seen = false;
        for line in BufReader::new(stdout).lines() {
            // 未识别帧、空行或读错误也不授予重试权：不能证明消息尚未被消费就不重喂。
            stdout_seen = true;
            let line = match line {
                Ok(l) => l,
                Err(_) => continue, // 读行出错:跳过(子进程仍在跑)
            };
            for msg in parse(&line) {
                let had_sid = outcome.session_id.is_some();
                apply(
                    &msg,
                    AssistantTextMode::MixedSnapshots,
                    &mut outcome,
                    &mut accumulated_assistant,
                    &mut stream_buf,
                    &mut result_text,
                    on_tool,
                    on_stream,
                    on_event,
                );
                // sid 首次出现即回传(增量落盘,支持被打断后 resume);幂等覆盖由上层处理。
                if !had_sid {
                    if let Some(sid) = outcome.session_id.as_deref() {
                        on_session_id(sid);
                    }
                }
            }
        }
        let status = guard.reap(); // 正常 EOF 收尾:wait 回收子进程 + 取退出状态,解除 kill-on-drop

        // 防御:turn 结束时仍有未收尾的 delta(没收到完整快照/result)→ 提交,避免丢字。
        if !stream_buf.is_empty() {
            accumulated_assistant.push(std::mem::take(&mut stream_buf));
        }

        // 终态回复:优先 result,退化为累积的 assistant 文本
        let last_assistant = if accumulated_assistant.is_empty() {
            None
        } else {
            Some(accumulated_assistant.join("\n\n"))
        };
        outcome.reply = result_text.or(last_assistant);
        // 异常退出判定(review #9):子进程**非零退出 且 本轮零输出 且 非被外部打断** → 真失败,返回
        // Err 让上层映射 Failed 贴 ERROR,而不是被 Empty→DONE 掩盖成功。被打断(停机/agents kill)
        // 退非零属预期,由打断/停机路另处理,这里放过;有输出(哪怕随后崩)也按已出内容收尾、不报错。
        let empty = outcome
            .reply
            .as_deref()
            .map(|r| r.trim().is_empty())
            .unwrap_or(true);
        if empty && !guard.killer.was_killed() {
            if let Some(st) = status {
                if !st.success() {
                    let stderr_tail = receive_stderr_tail(stderr_tail_rx);
                    let mut message = summon_i18n::format!("r0d5cb4d158");
                    if !stderr_tail.is_empty() {
                        message.push_str(&summon_i18n::format!("rb92ea5bce2"));
                    }
                    return Err(StartupFailure {
                        message,
                        code: st.code(),
                        stdout_seen,
                        stderr: stderr_tail,
                        pid: pgid,
                        started,
                    }
                    .into());
                }
            }
        }
        if let Some(reply) = outcome.reply.as_deref() {
            if !reply.trim().is_empty() {
                on_message(reply);
            }
        }
        Ok(outcome)
    }
}

impl<E: Executor> Subprocess<E> {
    /// 由 driver + ctx 算出无头一轮的 SpawnSpec(program/args/工作目录/进程组)。`run_turn` 与
    /// 带 kill-hook 的 `run_turn_with_hooks` 共用,行为单一真相源。
    fn build_spec(driver: CliDriver, prompt: &str, ctx: &TurnCtx) -> SpawnSpec {
        // headless_command 对 Claude/Cursor/Codex 恒 Some(prompt 已就位);拿不到只能是逻辑错(Other),
        // 降级为裸 program+空参数(与原 runner 调用点一致,gating 已在上游过滤)。
        // model/effort 经 LaunchOpts → resolve_launch_lenient:坏 effort 不 panic、丢 effort 降级。
        let opts = summon_core::driver::LaunchOpts {
            resume: ctx.session_id,
            sys_prompt_file: ctx.sys_prompt_file,
            model: ctx.model,
            effort: ctx.effort,
            codex_reasoning_summary: ctx.codex_reasoning_summary,
            codex_service_tier: ctx.codex_service_tier,
            codex_skip_git_repo_check: ctx.codex_skip_git_repo_check,
        };
        let (mut program, args) = driver
            .headless_command(prompt, &opts)
            .unwrap_or_else(|| (driver.default_cmd().to_string(), vec![]));
        if program == driver.default_cmd() {
            if let Some(tool) = summon_core::tools::Tool::for_driver(driver) {
                program = summon_core::tools::resolve(tool)
                    .to_string_lossy()
                    .into_owned();
            }
        }
        // stdin=null:codex 无头否则会等 stdin 输入,永不退出;claude/cursor 不受影响。
        // own_process_group:无头一轮自成组,便于 killpg 整组(打断/停机不留孤儿)。
        SpawnSpec {
            program,
            args,
            work_dir: ctx.work_dir.map(|s| s.to_string()),
            stdin_null: true,
            env: ctx.run_env.clone(),
            own_process_group: true,
        }
    }

    /// 带 kill-hook 的一轮:生产无头 chat(CliSession)走这里,要登记 kill 句柄 + 增量落 sid。
    #[allow(clippy::too_many_arguments)]
    pub fn run_turn_with_hooks(
        &mut self,
        driver: CliDriver,
        prompt: &str,
        ctx: &TurnCtx,
        on_message: &mut dyn FnMut(&str),
        on_stream: &mut dyn FnMut(&str),
        on_tool: &mut dyn FnMut(&str),
        on_event: &mut dyn FnMut(&AgentMessage),
        on_spawn: &mut dyn FnMut(&ChildKiller),
        on_session_id: &mut dyn FnMut(&str),
    ) -> anyhow::Result<TurnOutcome> {
        let spec = Self::build_spec(driver, prompt, ctx);
        let mut observe_session = |sid: &str| {
            if driver == CliDriver::Cursor {
                super::cursor_context::register_session(&ctx.run_env, sid);
            }
            on_session_id(sid);
        };
        let mut previous_killer = None;
        let result = self.drive(
            spec.clone(),
            &|l| driver.parse_headless_line(l),
            on_message,
            on_tool,
            on_stream,
            on_event,
            &mut |killer| {
                previous_killer = Some(killer.clone());
                on_spawn(killer);
            },
            &mut observe_session,
        );
        let Err(error) = result else { return result };
        let Some(failure) = error.downcast_ref::<StartupFailure>() else {
            return Err(error);
        };
        let Some(model) = ctx.model.filter(|_| driver == CliDriver::Cursor) else {
            return Err(error);
        };
        let Some(previous_killer) = previous_killer else {
            return Err(error);
        };
        let eligible = failure.code == Some(1)
            && !failure.stdout_seen
            && !previous_killer
                .cancellation_flag()
                .load(std::sync::atomic::Ordering::Acquire)
            && failure.stderr.starts_with(&format!(
                "Cannot use this model: {model}. Available models:"
            ))
            && super::cursor_cli_fallback::timed_out(&ctx.run_env, failure.pid, failure.started);
        if !eligible {
            return Err(error);
        }
        let Some(saved_config) = super::cursor_cli_fallback::prepare(&ctx.run_env, model) else {
            return Err(error);
        };
        let mut fallback = spec;
        let Some(index) = fallback
            .args
            .windows(2)
            .position(|pair| pair[0] == "--model" && pair[1] == model)
        else {
            return Err(error);
        };
        fallback.args.drain(index..index + 2);
        fallback.env.push((
            "CURSOR_CONFIG_DIR".into(),
            saved_config.path().display().to_string(),
        ));
        crate::observe::log::log(&summon_i18n::format!("rf8120b319e", failure.pid));
        // 第一轮没有任何 stdout，第二轮保留同一 SID、prompt、cwd，配置读取私有快照。只降级一次。
        self.drive(
            fallback,
            &|line| driver.parse_headless_line(line),
            on_message,
            on_tool,
            on_stream,
            on_event,
            &mut |killer| {
                previous_killer.forward_to(killer);
                on_spawn(killer);
            },
            &mut observe_session,
        )
        .map_err(|retry| retry.context(summon_i18n::format!("r69fdedfd15")))
    }

    /// 跑一轮(无 kill-hook 版):无需打断/停机登记的一次性调用方(以及 `real_*` 冒烟测试)用。
    /// 等价于 `run_turn_with_hooks` 但 on_spawn/on_session_id 给 no-op。生产无头一轮(daemon chat)
    /// 走 `run_turn_with_hooks`(要登记 kill 句柄 + 增量落 sid);task run/蒸馏直调 `drive`(见 run.rs)。
    /// 现仅 `#[cfg(test)]` 调用,故 cfg 收窄免 dead_code。
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)] // 各回调职责独立(消息/流式/工具/事件),合并反而晦涩
    pub fn run_turn(
        &mut self,
        driver: CliDriver,
        prompt: &str,
        ctx: &TurnCtx,
        on_message: &mut dyn FnMut(&str),
        on_stream: &mut dyn FnMut(&str),
        on_tool: &mut dyn FnMut(&str),
        on_event: &mut dyn FnMut(&AgentMessage),
    ) -> anyhow::Result<TurnOutcome> {
        self.run_turn_with_hooks(
            driver,
            prompt,
            ctx,
            on_message,
            on_stream,
            on_tool,
            on_event,
            &mut |_killer| {},
            &mut |_sid| {},
        )
    }
}

/// 把一块 assistant 文本归并进**当前段** buf,消除 cursor 的重发重复。cursor 无头流里同一段会以
/// 多种形态出现:① 重复重发(段边界/工具前再发一遍同文)② 累积增长快照(后块含前块为前缀)
/// ③ 老版/真 token 增量(互不包含的碎片)。规则:重复/更短重发 → 忽略;增长快照 → 取更长者替换;
/// 其余 → 拼接(保老版 delta 行为)。claude/codex 每块独立、buf 进时恒空 → 直接落 buf,不受影响。
fn merge_segment(buf: &mut String, t: &str) {
    if buf.is_empty() {
        buf.push_str(t);
    } else if t == buf.as_str() || buf.starts_with(t) {
        // 完全重复 / 更短的重发:已含,忽略
    } else if t.starts_with(buf.as_str()) {
        // 累积增长快照:用更长的整段替换
        buf.clear();
        buf.push_str(t);
    } else {
        // 互不包含 → 真增量 token,拼接续接
        buf.push_str(t);
    }
}

/// RPC 协议明确区分 delta 与完成快照；CLI stream-json 的 partial 帧兼有重发快照。
#[derive(Clone, Copy)]
pub(crate) enum AssistantTextMode {
    Delta,
    MixedSnapshots,
}

impl AssistantTextMode {
    fn merge(self, buf: &mut String, text: &str, partial: bool) {
        match self {
            Self::Delta => {
                if !partial {
                    // 完成快照是当前段的权威全文，允许它修正此前的增量。
                    buf.clear();
                }
                buf.push_str(text);
            }
            Self::MixedSnapshots => merge_segment(buf, text),
        }
    }
}

/// 流式全文 = 已收尾段落("\n\n" 分隔)+ 进行中的 delta buf(同段续接,不加空行)。
fn render(segments: &[String], stream_buf: &str) -> String {
    let mut s = segments.join("\n\n");
    if !stream_buf.is_empty() {
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str(stream_buf);
    }
    s
}

/// 把一条 `AgentMessage` 折进本轮累积态并驱动回调。
///
/// **`pub(crate)` 是给 RPC 引擎复用的**:`capture/rpc.rs` 的一轮消费面必须与无头 exec 路
/// 逐字节同规则(段边界、去重、result 优先、sid/api_error 首见即记),否则同一个 agent 切一下
/// `runtime.capture` 卡片形态就变了 —— 那正是 `codex_rpc` 一致性断言要防的语义漂移,
/// 只不过那边钉的是「事件序列」,这边钉的是「事件怎么折成卡」。抄一份必漂,故共用。
#[allow(clippy::too_many_arguments)] // 各累积态/回调职责独立,打成结构体反而晦涩(同 drive 的取舍)
pub(crate) fn apply(
    msg: &AgentMessage,
    text_mode: AssistantTextMode,
    outcome: &mut TurnOutcome,
    accumulated_assistant: &mut Vec<String>,
    stream_buf: &mut String,
    result_text: &mut Option<String>,
    on_tool: &mut dyn FnMut(&str),
    on_stream: &mut dyn FnMut(&str),
    on_event: &mut dyn FnMut(&AgentMessage),
) {
    // tee 在 drain 最上游:UI 投影前先把 typed 全量事件喂给 on_event(thinking/工具入参/结果/usage
    // 等都在此完整捕获),后续 match 只做 UI 回调的有损投影,二者解耦。
    on_event(msg);
    if outcome.session_id.is_none() {
        if let Some(sid) = &msg.session_id {
            outcome.session_id = Some(sid.clone());
        }
    }
    // API 错误(model_not_found 等):从 JSONL 解析器带出的 error 字段检测,首次见到时记录。
    if outcome.api_error.is_none() {
        if let Some(err) = &msg.error {
            if !err.is_empty() {
                outcome.api_error = Some(err.clone());
            }
        }
    }
    if outcome.api_error_kind.is_none() {
        outcome.api_error_kind = msg.error_kind;
    }
    match msg.kind {
        AgentKind::Assistant => {
            if let Some(t) = &msg.text {
                text_mode.merge(stream_buf, t, msg.partial);
                if !msg.partial {
                    // `partial=false` 是该段的权威终止行:定稿当前段、清 buf 起下一段。
                    // claude/codex 各 assistant 块均 partial=false → 每块自成一段(buf 始终空进空出)。
                    accumulated_assistant.push(std::mem::take(stream_buf));
                }
                // 把当前全文(已收尾段 + 进行中的 buf)传给流式回调(打字机更新用)
                on_stream(&render(accumulated_assistant, stream_buf));
            }
        }
        AgentKind::ToolUse => {
            // 工具调用是段边界:cursor 工具前的文本段到此结束 → 先定稿,避免与工具后新段拼成一坨
            // 或被新段的 partial=false 快照误覆盖丢字(claude/codex buf 恒空,此处为 no-op)。
            if !stream_buf.is_empty() {
                accumulated_assistant.push(std::mem::take(stream_buf));
            }
            if let Some(tool) = &msg.tool {
                on_tool(tool);
            }
        }
        AgentKind::Result => {
            if let Some(t) = &msg.text {
                *result_text = Some(t.clone());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::executor::LocalExecutor;

    const PARAMETER_MODEL: &str = "claude-opus-5-5[context=1m,effort=high,fast=false]";

    struct TimeoutExecutor {
        calls: std::cell::RefCell<Vec<SpawnSpec>>,
        timeout: bool,
        stdout: &'static str,
        retry_fails: bool,
        real_retry: bool,
    }

    impl TimeoutExecutor {
        fn new() -> Self {
            Self {
                calls: Default::default(),
                timeout: true,
                stdout: "",
                retry_fails: false,
                real_retry: false,
            }
        }
    }

    impl Executor for TimeoutExecutor {
        fn spawn(&self, spec: &SpawnSpec) -> anyhow::Result<std::process::Child> {
            let first = self.calls.borrow().is_empty();
            self.calls.borrow_mut().push(spec.clone());
            if !first && self.real_retry {
                return LocalExecutor.spawn(spec);
            }
            let mut stub = spec.clone();
            stub.program = "/bin/sh".into();
            let script = if first {
                let time = chrono::Utc::now().to_rfc3339();
                let event = if self.timeout {
                    "models.fetchAvailableModelsParameterized.timeout"
                } else {
                    "models.fetchAvailableModelsParameterized.error"
                };
                format!(
                    r#"
                    dir="$TMPDIR/cursor-agent-logs-{}"
                    mkdir -p "$dir"
                    printf '{{"event":"debug-session-start","pid":%s,"startTime":"{time}"}}\n{{"event":"{event}"}}\n' "$$" > "$dir/session-test-$$-1.log"
                    printf '%s' '{}'
                    printf '%s\n' 'Cannot use this model: {PARAMETER_MODEL}. Available models: auto, claude-opus-5-5-high' >&2
                    exit 1
                "#,
                    nix::unistd::Uid::current(),
                    self.stdout
                )
            } else if self.retry_fails {
                "printf 'retry failed' >&2; exit 1".into()
            } else {
                r#"printf '%s\n' '{"type":"result","subtype":"success","result":"fallback-ok","session_id":"same-sid"}'"#.into()
            };
            stub.args = vec!["-c".into(), script];
            LocalExecutor.spawn(&stub)
        }
        fn read_file_tail(&self, path: &std::path::Path, offset: u64) -> anyhow::Result<Vec<u8>> {
            LocalExecutor.read_file_tail(path, offset)
        }
        fn tmux(&self, args: &[&str]) -> anyhow::Result<String> {
            LocalExecutor.tmux(args)
        }
    }

    fn fallback_ctx(root: &std::path::Path) -> TurnCtx<'static> {
        std::fs::create_dir_all(root.join("chats")).unwrap();
        std::fs::write(
            root.join("cli-config.json"),
            super::super::cursor_cli_fallback::tests::config().to_string(),
        )
        .unwrap();
        TurnCtx {
            session_id: Some("same-sid"),
            model: Some(PARAMETER_MODEL),
            run_env: vec![
                ("CURSOR_CONFIG_DIR".into(), root.display().to_string()),
                ("TMPDIR".into(), root.display().to_string()),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn cursor_timeout_retries_once_without_model_and_preserves_the_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = fallback_ctx(tmp.path());
        let mut process = Subprocess::new(TimeoutExecutor::new());
        let registry = crate::runtime::inflight::InFlight::new();
        let key = crate::runtime::inflight::TurnKey::new("proof", "g-fallback");
        let mut handles: Vec<ChildKiller> = Vec::new();
        let out = process
            .run_turn_with_hooks(
                CliDriver::Cursor,
                "exact prompt",
                &ctx,
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |killer| {
                    if let Some(previous) = handles.last() {
                        assert!(registry.replace_running(&key, previous, killer.clone()));
                    } else {
                        assert!(registry.insert(
                            key.clone(),
                            crate::runtime::inflight::InFlightTurn {
                                killer: killer.clone(),
                                base: tmp.path().into(),
                                pending: std::sync::Arc::new(std::sync::Mutex::new(
                                    summon_core::pending::PendingTurn {
                                        kind: summon_core::pending::PendingTurnKind::Chat,
                                        delivery_scope: "proof".into(),
                                        agent_name: "proof".into(),
                                        inbox_message_ids: vec![],
                                        resume_authorized: false,
                                        chat_id: "proof".into(),
                                        chat_type: "group".into(),
                                        cli: "cursor".into(),
                                        prompt: "exact prompt".into(),
                                        session_id: Some("same-sid".into()),
                                        partial: String::new(),
                                    }
                                )),
                                shape: crate::runtime::inflight::TurnShape::OwnedProcess,
                            }
                        ));
                    }
                    handles.push(killer.clone());
                },
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(out.reply.as_deref(), Some("fallback-ok"));
        assert_eq!(out.session_id.as_deref(), Some("same-sid"));
        let calls = process.exec.calls.borrow();
        assert_eq!(calls.len(), 2);
        let mut expected = calls[0].args.clone();
        let index = expected.iter().position(|arg| arg == "--model").unwrap();
        expected.drain(index..index + 2);
        assert_eq!(calls[1].args, expected);
        assert_eq!(calls[0].env, calls[1].env[..calls[0].env.len()]);
        assert_eq!(calls[1].env.last().unwrap().0, "CURSOR_CONFIG_DIR");
        assert_eq!(calls[0].work_dir, calls[1].work_dir);
        assert_eq!(
            registry.finish(&key, &handles[0]),
            crate::runtime::inflight::FinishDisposition::Missing
        );
        assert_eq!(
            registry.finish(&key, &handles[1]),
            crate::runtime::inflight::FinishDisposition::Completed
        );
        handles[0].interrupt();
        assert!(handles[1].was_killed(), "旧 UI 把手也必须能打断重试进程");
    }

    #[test]
    fn cursor_fallback_requires_timeout_zero_stdout_and_matching_saved_config() {
        for scenario in ["error", "stdout", "config", "cancel", "retry-fails"] {
            let tmp = tempfile::tempdir().unwrap();
            let ctx = fallback_ctx(tmp.path());
            let mut exec = TimeoutExecutor::new();
            match scenario {
                "error" => exec.timeout = false,
                "stdout" => exec.stdout = "unrecognized stdout",
                "config" => {
                    std::fs::write(tmp.path().join("cli-config.json"), "{}").unwrap();
                }
                "retry-fails" => exec.retry_fails = true,
                _ => {}
            }
            let mut process = Subprocess::new(exec);
            let result = process.run_turn_with_hooks(
                CliDriver::Cursor,
                "exact prompt",
                &ctx,
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |killer| {
                    if scenario == "cancel" {
                        killer.cancel_waiters();
                    }
                },
                &mut |_| {},
            );
            assert!(result.is_err(), "{scenario}");
            assert_eq!(
                process.exec.calls.borrow().len(),
                if scenario == "retry-fails" { 2 } else { 1 },
                "{scenario}"
            );
        }
    }

    #[test]
    fn cursor_fallback_transfers_a_stop_between_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = fallback_ctx(tmp.path());
        let mut process = Subprocess::new(TimeoutExecutor::new());
        let mut previous: Option<ChildKiller> = None;
        process
            .run_turn_with_hooks(
                CliDriver::Cursor,
                "exact prompt",
                &ctx,
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |killer| {
                    if let Some(old) = &previous {
                        old.interrupt();
                        assert!(killer.was_killed());
                    } else {
                        previous = Some(killer.clone());
                    }
                },
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(process.exec.calls.borrow().len(), 2);
    }

    #[test]
    fn apply_preserves_structured_provider_error_kind() {
        let mut message = AgentMessage::of(AgentKind::Error);
        message.error = Some("localized quota message".to_string());
        message.error_kind = Some(summon_core::agent_msg::AgentErrorKind::UsageLimitExceeded);
        let mut outcome = TurnOutcome::default();
        apply(
            &message,
            AssistantTextMode::MixedSnapshots,
            &mut outcome,
            &mut Vec::new(),
            &mut String::new(),
            &mut None,
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
        );

        assert_eq!(
            outcome.api_error.as_deref(),
            Some("localized quota message")
        );
        assert_eq!(
            outcome.api_error_kind,
            Some(summon_core::agent_msg::AgentErrorKind::UsageLimitExceeded)
        );
    }

    /// cursor 认不出工具体的 tool_call 照样把前后两段正文隔开。
    #[test]
    fn unrecognized_cursor_tool_still_splits_reply_segments() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"前文。"}]},"session_id":"s1","timestamp_ms":1}"#,
            r#"{"type":"tool_call","subtype":"started","call_id":"c1","tool_call":{"hookAdditionalContexts":[],"toolCallId":"c1"},"session_id":"s1"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"后文。"}]},"session_id":"s1","timestamp_ms":2}"#,
        ];
        let mut outcome = TurnOutcome::default();
        let mut accumulated = Vec::new();
        let mut stream_buf = String::new();
        for line in lines {
            for message in summon_core::stream_json::parse_stream_json_line(line) {
                apply(
                    &message,
                    AssistantTextMode::MixedSnapshots,
                    &mut outcome,
                    &mut accumulated,
                    &mut stream_buf,
                    &mut None,
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                );
            }
        }
        if !stream_buf.is_empty() {
            accumulated.push(stream_buf);
        }
        assert_eq!(accumulated, ["前文。", "后文。"]);
    }

    /// 造一个假 CLI:无视参数,把 fixture 原样喷到 stdout。返回脚本路径(SpawnSpec.program 用)。
    fn fake_cli(jsonl: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("out.jsonl");
        std::fs::write(&data, jsonl).unwrap();
        let script = dir.path().join("fakecli.sh");
        crate::test_support::write_test_executable(
            &script,
            "#!/bin/sh\ncat \"${0%/*}/out.jsonl\"\n",
        );
        (dir, script.to_string_lossy().into_owned())
    }

    /// Codex 对系统临时目录有单独信任策略；在 HOME 下构造「外层非 Git + 内层 Git 仓库」才能
    /// 稳定复刻 `~/workspace/web` 这类多仓聚合目录。
    fn non_git_home_workspace() -> tempfile::TempDir {
        let home = std::env::var_os("HOME").unwrap();
        let workspace = tempfile::Builder::new()
            .prefix("summon-codex-non-git-")
            .tempdir_in(home)
            .unwrap();
        let nested_repo = workspace.path().join("nested-repo");
        std::fs::create_dir(&nested_repo).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&nested_repo)
            .status()
            .unwrap();
        assert!(status.success(), "应成功创建聚合目录内的嵌套 Git 仓库");
        workspace
    }

    /// 轮询 `pgrep -g <pgid>` 直到整组为空或超时:SIGINT 杀整组后,被 reparent 的孙子(sleep)
    /// 可能要几十毫秒才被回收,固定 sleep 易抖动,故 retry。返回 true=已空。
    fn wait_group_empty(pgid: i32, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let out = std::process::Command::new("pgrep")
                .args(["-g", &pgid.to_string()])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .is_none()
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// 造一个「慢 CLI」:先吐若干行(可带 session_id),再 sleep 很久。用于验打断/句柄/早回传 sid:
    /// 不打断的话 drive 会卡在 sleep 上 N 秒。返回脚本路径。
    fn slow_cli(head_lines: &str, sleep_secs: u32) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("slowcli.pl");
        // 用 perl(经 sigaction)显式把 SIGINT 装回 DEFAULT,再 print 头部行(模拟 init 行已来)、
        // 长 sleep。关键:非交互 sh 会把 SIGINT 设成 SIG_IGN 并被子进程继承,且 POSIX 规定「入口即被
        // 忽略的信号在非交互 shell 里 trap 不回来」——这是 fixture 假象(非 kill 原语问题),会让
        // killpg 的 SIGINT 被吞、白等 sleep(实测 flake 根因)。`$SIG{INT}='DEFAULT'` 用 sigaction
        // 强制复位,不受继承的 SIG_IGN 影响,精准复刻真实 cursor/codex(自己装 SIGINT handler、收即退)。
        // $|=1 行缓冲立即 flush;single process(无 fork)→ 杀它即整组清空。
        let head = head_lines.replace('\\', "\\\\").replace('\'', "\\'");
        crate::test_support::write_test_executable(
            &script,
            &format!(
                "#!/usr/bin/env perl\n$SIG{{INT}}='DEFAULT';\n$|=1;\nprint '{head}';\nsleep {sleep_secs};\n"
            ),
        );
        (dir, script.to_string_lossy().into_owned())
    }

    /// on_spawn 应在读到第一行之前就被调用一次,且拿到的 pgid > 0(子进程自成组)。
    #[test]
    fn drive_publishes_killer_on_spawn() {
        // 这条 fixture 立即 cat 完即退出(无 sleep),足够验证 on_spawn 至少被调一次、pgid 合法。
        let (_d, prog) = slow_cli(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"t1\"}\n",
            0,
        );
        let mut spawned_pgid: Option<i32> = None;
        let mut src = Subprocess::new(LocalExecutor);
        src.drive(
            SpawnSpec {
                program: prog,
                stdin_null: true,
                own_process_group: true,
                ..Default::default()
            },
            &summon_core::stream_json::parse_stream_json_line,
            &mut |_b| {},
            &mut |_t| {},
            &mut |_a| {},
            &mut |_e| {},
            &mut |killer| spawned_pgid = Some(killer.pgid()),
            &mut |_sid| {},
        )
        .unwrap();
        assert!(spawned_pgid.unwrap_or(0) > 0, "on_spawn 应回传合法 pgid");
    }

    /// on_session_id 应在 turn 干净结束前就被触发(slow CLI 先吐带 thread_id 的行再 sleep):
    /// 主线程拿到句柄打断后 drive 返回,断言 sid 已在打断前回传过。
    #[test]
    fn drive_emits_session_id_early() {
        use std::sync::mpsc;
        let (_d, prog) = slow_cli(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"t-early\"}\n",
            30,
        );
        let (tx_killer, rx_killer) = mpsc::channel::<ChildKiller>();
        let (tx_sid, rx_sid) = mpsc::channel::<String>();
        let handle = std::thread::spawn(move || {
            let mut src = Subprocess::new(LocalExecutor);
            src.drive(
                SpawnSpec {
                    program: prog,
                    stdin_null: true,
                    own_process_group: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |_b| {},
                &mut |_t| {},
                &mut |_a| {},
                &mut |_e| {},
                &mut |killer| {
                    let _ = tx_killer.send(killer.clone());
                },
                &mut |sid| {
                    let _ = tx_sid.send(sid.to_string());
                },
            )
            .unwrap()
        });
        // sid 应在 sleep 前就回传。预算放宽到 20s(T5:并行套件 CPU 争抢下 5s 偶发假阴;sid 到达不是
        // 「延迟」断言、只是「turn 结束前拿到」,放宽不损检出力——真不回传仍会超时失败)。
        let sid = rx_sid
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("session_id 应在 turn 结束前回传");
        assert_eq!(sid, "t-early");
        // 打断让 drive 从 sleep 里早退(否则要等 30s)。
        let killer = rx_killer
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap();
        killer.interrupt();
        let out = handle.join().unwrap();
        assert_eq!(out.session_id.as_deref(), Some("t-early"));
    }

    /// 核心回归:drive 在一个线程跑慢 CLI,主线程拿到 ChildKiller 后 interrupt(),drive 应秒级返回
    /// (不是 30s)、拿到早回传的 sid、子进程组被回收无孤儿(pgrep -g 空)。
    #[test]
    fn interrupt_during_drive_breaks_loop() {
        use std::sync::mpsc;
        let (_d, prog) = slow_cli(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"t1\"}\n",
            30,
        );
        let (tx, rx) = mpsc::channel::<ChildKiller>();
        let (tx_sid, rx_sid) = mpsc::channel::<String>();
        let start = std::time::Instant::now();
        let handle = std::thread::spawn(move || {
            let mut src = Subprocess::new(LocalExecutor);
            src.drive(
                SpawnSpec {
                    program: prog,
                    stdin_null: true,
                    own_process_group: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |_b| {},
                &mut |_t| {},
                &mut |_a| {},
                &mut |_e| {},
                &mut |killer| {
                    let _ = tx.send(killer.clone());
                },
                &mut |sid| {
                    let _ = tx_sid.send(sid.to_string());
                },
            )
            .unwrap()
        });
        // 句柄/ sid 回传预算放宽到 20s(T5:并行套件 CPU 争抢下 5s 偶发假阴;两者都不是「延迟」断言)。
        let killer = rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
        let pgid = killer.pgid();
        // 确定性等到 sid 已被解析(头部行已处理),再打断:避免「init 行还没读到就杀」的竞态。
        let _ = rx_sid
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("sid 应在打断前先被解析");
        killer.interrupt();
        let out = handle.join().unwrap();
        // 核心行为断言:打断必须从 30s sleep 里早退。阈值 25s 远低于 30s,既钉死「打断生效」(真生效
        // ~0s,真失效 ~30s,两态清晰可分),又容下并行负载抖动(T5:旧 10s 在重载下偶发假阴)。
        assert!(
            start.elapsed() < std::time::Duration::from_secs(25),
            "打断后应远早于 30s sleep 返回,实测 {:?}",
            start.elapsed()
        );
        assert_eq!(out.session_id.as_deref(), Some("t1"), "打断前应已拿到 sid");
        // 无孤儿:整组应已空(轮询等被 reparent 的孙子被回收)。
        assert!(
            wait_group_empty(pgid, std::time::Duration::from_secs(3)),
            "子进程组应被回收(无孤儿)"
        );
    }

    /// review #9:子进程**非零退出 + 零输出 + 非被打断** → drive 返回 Err(上层映射 Failed),
    /// 不再被 Empty→DONE 掩盖。对照:零退出 + 零输出 = 正常静默成功,返回 Ok。
    #[test]
    fn drive_errors_on_abnormal_exit_with_no_output() {
        let run = |args: Vec<&str>| {
            Subprocess::new(LocalExecutor).drive(
                SpawnSpec {
                    program: "sh".into(),
                    args: args.into_iter().map(String::from).collect(),
                    stdin_null: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |_b| {},
                &mut |_t| {},
                &mut |_a| {},
                &mut |_e| {},
                &mut |_k| {},
                &mut |_s| {},
            )
        };
        let error = run(vec![
            "-c",
            "printf 'Not inside a trusted directory' >&2; exit 3",
        ])
        .expect_err("非零退出 + 无输出应 Err(Failed)");
        assert!(
            format!("{error:#}").contains("Not inside a trusted directory"),
            "错误应保留 CLI stderr，实得: {error:#}"
        );
        assert!(
            run(vec!["-c", "true"]).is_ok(),
            "零退出 + 无输出 = 正常静默成功,应 Ok(Empty→DONE)"
        );
    }

    #[test]
    fn abnormal_exit_reports_only_bounded_stderr_tail() {
        let error = Subprocess::new(LocalExecutor)
            .drive(
                SpawnSpec {
                    program: "sh".into(),
                    args: vec![
                        "-c".into(),
                        "printf 'discard-me-' >&2; yes x | head -c 20000 >&2; printf 'tail-marker' >&2; exit 4"
                            .into(),
                    ],
                    stdin_null: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |_b| {},
                &mut |_t| {},
                &mut |_a| {},
                &mut |_e| {},
                &mut |_k| {},
                &mut |_s| {},
            )
            .expect_err("非零退出应返回 stderr 尾部");
        let message = format!("{error:#}");
        assert!(message.contains("tail-marker"), "必须保留 stderr 末尾");
        assert!(!message.contains("discard-me-"), "超限前缀必须被丢弃");
        assert!(
            message.len() <= STDERR_TAIL_LIMIT_BYTES + 256,
            "错误消息必须保持有界，实得 {} bytes",
            message.len()
        );
    }

    /// kill-on-drop guard:on_stream 回调里 panic,catch_unwind 兜住后子进程组应已被杀(无孤儿)。
    #[test]
    fn drive_kills_child_on_panic_unwind() {
        use std::sync::mpsc;
        let (_d, prog) = slow_cli(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"t1\"}\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
            30,
        );
        let (tx, rx) = mpsc::channel::<i32>();
        let prog2 = prog.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut src = Subprocess::new(LocalExecutor);
            let _ = src.drive(
                SpawnSpec {
                    program: prog2,
                    stdin_null: true,
                    own_process_group: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |_b| {},
                &mut |_t| {},
                &mut |_acc| panic!("模拟 on_stream 回调 panic"),
                &mut |_e| {},
                &mut |killer| {
                    let _ = tx.send(killer.pgid());
                },
                &mut |_sid| {},
            );
        }));
        assert!(result.is_err(), "应捕获到 panic");
        // 句柄回传预算放宽到 20s(T5:并行套件 CPU 争抢下 5s 偶发假阴,非「延迟」断言)。
        let pgid = rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
        assert!(
            wait_group_empty(pgid, std::time::Duration::from_secs(3)),
            "panic unwind 后子进程组应被 guard 杀掉(无孤儿)"
        );
    }

    /// RR1.5 反向:不装 sink(默认)时不产生任何录制文件、行为与原来一致。
    #[test]
    fn drive_without_sink_does_not_record() {
        let pong = include_str!("../../../summon-core/tests/fixtures/claude-stream-pong.jsonl");
        let (_d, prog) = fake_cli(pong);
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .drive(
                SpawnSpec {
                    program: prog,
                    args: vec!["x".into()],
                    stdin_null: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |_b| {},
                &mut |_tool| {},
                &mut |_acc| {},
                &mut |_ev| {},
                &mut |_killer| {},
                &mut |_sid| {},
            )
            .unwrap();
        assert_eq!(out.reply.as_deref(), Some("pong"));
    }

    #[test]
    fn drive_captures_reply_and_session_id() {
        let pong = include_str!("../../../summon-core/tests/fixtures/claude-stream-pong.jsonl");
        let (_d, prog) = fake_cli(pong);
        let mut messages: Vec<String> = vec![];
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .drive(
                SpawnSpec {
                    program: prog,
                    args: vec!["x".into()],
                    stdin_null: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |body| messages.push(body.to_string()),
                &mut |_tool| {},
                &mut |_acc| {},
                &mut |_ev| {},
                &mut |_killer| {},
                &mut |_sid| {},
            )
            .unwrap();
        assert_eq!(out.reply.as_deref(), Some("pong"));
        assert_eq!(
            out.session_id.as_deref(),
            Some("07778859-d337-4a01-a41d-e4c11ae7be81")
        );
        assert_eq!(messages, vec!["pong".to_string()]);
    }

    #[test]
    fn drive_on_stream_called_incrementally() {
        let tooluse =
            include_str!("../../../summon-core/tests/fixtures/claude-stream-tooluse.jsonl");
        let (_d, prog) = fake_cli(tooluse);
        let mut stream_calls: Vec<String> = vec![];
        let mut final_message = String::new();
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .drive(
                SpawnSpec {
                    program: prog,
                    args: vec!["x".into()],
                    stdin_null: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |body| final_message = body.to_string(),
                &mut |_tool| {},
                &mut |acc| stream_calls.push(acc.to_string()),
                &mut |_ev| {},
                &mut |_killer| {},
                &mut |_sid| {},
            )
            .unwrap();
        assert_eq!(stream_calls[0], "Let me check the file.");
        assert_eq!(
            stream_calls[1],
            "Let me check the file.\n\nThere are 2 files."
        );
        assert_eq!(stream_calls.len(), 2);
        assert_eq!(final_message, "There are 2 files.");
        assert_eq!(out.session_id.as_deref(), Some("sid-tool-1"));
    }

    /// cursor `--stream-partial-output`:delta 块须拼接(原地增长),末尾完整快照不得叠加。
    /// 回归用例——曾把每个 delta 当独立段落 `\n\n` join,导致卡片出现 token 间空行 + 整句重复。
    #[test]
    fn cursor_partial_deltas_concatenate_without_dup_or_blank_lines() {
        let partial =
            include_str!("../../../summon-core/tests/fixtures/cursor-stream-partial.jsonl");
        let (_d, prog) = fake_cli(partial);
        let mut stream_calls: Vec<String> = vec![];
        let mut final_message = String::new();
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .drive(
                SpawnSpec {
                    program: prog,
                    args: vec!["x".into()],
                    stdin_null: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |body| final_message = body.to_string(),
                &mut |_tool| {},
                &mut |acc| stream_calls.push(acc.to_string()),
                &mut |_ev| {},
                &mut |_killer| {},
                &mut |_sid| {},
            )
            .unwrap();
        // 终态回复干净、不重复
        assert_eq!(out.reply.as_deref(), Some("1 2 3 4 5 6 7 8"));
        assert_eq!(final_message, "1 2 3 4 5 6 7 8");
        // 任何一帧流式文本都不得含 token 间空行,也不得把整句重复一遍
        for c in &stream_calls {
            assert!(!c.contains("\n\n"), "delta 间不应有空行,实得: {c:?}");
        }
        // 打字机最后一帧 = 完整一句,且只出现一次
        let last = stream_calls.last().expect("应有流式帧");
        assert_eq!(last, "1 2 3 4 5 6 7 8");
        assert_eq!(last.matches("1 2 3 4 5 6 7 8").count(), 1, "整句不应重复");
        assert_eq!(out.session_id.as_deref(), Some("part-sid-1"));
    }

    /// cursor 真机:每个文本段是**完整块**(非 token delta),且会在段边界/工具调用前把同一段**重发一遍**
    /// (`partial=true` 两行同文,一条带 `model_call_id`)。回归用例(owner 实测的 dup bug)——曾把每条
    /// `partial` 当增量盲拼,导致卡片整段重复:"Step one done.Step one done."。fixture 取自真机捕获。
    #[test]
    fn cursor_repeated_segments_dont_duplicate_in_stream() {
        let dup = include_str!("../../../summon-core/tests/fixtures/cursor-stream-tool-dup.jsonl");
        let (_d, prog) = fake_cli(dup);
        let mut stream_calls: Vec<String> = vec![];
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .drive(
                SpawnSpec {
                    program: prog,
                    args: vec!["x".into()],
                    stdin_null: true,
                    ..Default::default()
                },
                &summon_core::stream_json::parse_stream_json_line,
                &mut |_b| {},
                &mut |_tool| {},
                &mut |acc| stream_calls.push(acc.to_string()),
                &mut |_ev| {},
                &mut |_killer| {},
                &mut |_sid| {},
            )
            .unwrap();
        // 任何一帧流式文本都不得把同一段重复("Step one done." 连写两遍)
        for c in &stream_calls {
            assert!(
                !c.contains("Step one done.Step one done."),
                "段不应重复,实得: {c:?}"
            );
            assert!(
                !c.contains("Step two done.Step two done."),
                "段不应重复,实得: {c:?}"
            );
        }
        // 最后一帧 = 两段各一次,工具前后分段
        let last = stream_calls.last().expect("应有流式帧");
        assert_eq!(
            last.matches("Step one done.").count(),
            1,
            "段一应只出现一次"
        );
        assert_eq!(
            last.matches("Step two done.").count(),
            1,
            "段二应只出现一次"
        );
        assert!(last.contains("Step one done.") && last.contains("Step two done."));
        // 终态回复来自 result 行,干净不重复
        assert_eq!(out.reply.as_deref(), Some("Step one done.Step two done."));
    }

    /// tee seam 验证:喂 tooluse fixture,经 EventSink 落盘后 events.jsonl 应含一条带 tool_input 的
    /// tool_use 记录,以及 thinking 之外的全量事件(此 fixture 无 thinking,验工具入参即可)。
    #[test]
    fn on_event_tees_full_stream_to_events_jsonl() {
        use crate::observe::event_log::EventSink;
        use summon_core::event_log::EventRecord;

        let tooluse =
            include_str!("../../../summon-core/tests/fixtures/claude-stream-tooluse.jsonl");
        let (_d, prog) = fake_cli(tooluse);
        let outdir = tempfile::tempdir().unwrap();
        let mut sink = EventSink::open(
            outdir.path(),
            "p_tee",
            summon_core::event_log::EventStamp::default(),
        )
        .unwrap();
        let mut src = Subprocess::new(LocalExecutor);
        src.drive(
            SpawnSpec {
                program: prog,
                args: vec!["x".into()],
                stdin_null: true,
                ..Default::default()
            },
            &summon_core::stream_json::parse_stream_json_line,
            &mut |_b| {},
            &mut |_t| {},
            &mut |_a| {},
            &mut |ev| sink.write(ev),
            &mut |_killer| {},
            &mut |_sid| {},
        )
        .unwrap();

        let path = outdir
            .path()
            .join("threads")
            .join("p_tee")
            .join("events.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let recs: Vec<EventRecord> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let tooluse_rec = recs
            .iter()
            .find(|r| r.kind == "tool_use")
            .expect("应有一条 tool_use 记录");
        assert_eq!(tooluse_rec.tool.as_deref(), Some("Bash"));
        assert_eq!(
            tooluse_rec.tool_input.as_ref().unwrap()["command"],
            serde_json::json!("ls"),
            "tool_input 应完整落盘"
        );
        // 全量流也含最终 result(带 usage)与中途 assistant 文本。
        assert!(recs.iter().any(|r| r.kind == "result"));
        assert!(recs.iter().any(|r| r.kind == "assistant"));
    }

    #[test]
    #[ignore = "需要本机已登录 cursor-agent;跑法 cargo test -p summon -- --ignored real_cursor"]
    fn real_cursor_subprocess_replies() {
        let mut got = String::new();
        let ctx = TurnCtx {
            session_id: None,
            work_dir: None,
            sys_prompt_file: None,
            ..Default::default()
        };
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .run_turn(
                CliDriver::Cursor,
                "reply with exactly the single word: pong",
                &ctx,
                &mut |body| got = body.to_string(),
                &mut |_a| {},
                &mut |_t| {},
                &mut |_e| {},
            )
            .unwrap();
        assert!(out.session_id.is_some(), "应捕获 session_id");
        assert!(
            got.to_lowercase().contains("pong"),
            "回复含 pong, 实得: {got}"
        );
    }

    #[test]
    #[ignore = "需要本机已登录 Cursor，真实付费验证 1M CLI 会话与原生 ctx 采集"]
    fn real_cursor_cli_reports_native_one_million_context() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let source = dirs::home_dir().unwrap().join(".cursor/cli-config.json");
        if source.is_file() {
            use std::os::unix::fs::PermissionsExt as _;
            let target = config.join("cli-config.json");
            std::fs::copy(source, &target).unwrap();
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let env = vec![
            ("CURSOR_CONFIG_DIR".into(), config.display().to_string()),
            ("SUMMON_AGENT".into(), "cursor-cli-one-million-proof".into()),
            ("SUMMON_CONVERSATION_TAG".into(), "g-proof".into()),
        ];
        let work_dir = work.display().to_string();
        let ctx = TurnCtx {
            work_dir: Some(&work_dir),
            model: Some("claude-opus-5-5[context=1m,effort=high,fast=false]"),
            run_env: env,
            ..Default::default()
        };
        let mut tools = 0;
        let out = Subprocess::new(LocalExecutor)
            .run_turn_with_hooks(
                CliDriver::Cursor,
                "Reply exactly SUMMON_CURSOR_1M_OK. Do not use tools.",
                &ctx,
                &mut |_| {},
                &mut |_| {},
                &mut |_| tools += 1,
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(out.reply.as_deref(), Some("SUMMON_CURSOR_1M_OK"));
        assert_eq!(tools, 0);
        let sid = out.session_id.as_deref().unwrap();
        let badge = crate::runtime::daemon::completion_badge_with_session_for_test(
            &summon_core::feishu::TurnBadge {
                cli: "cursor".into(),
                ..Default::default()
            },
            "cursor-cli-one-million-proof",
            tmp.path(),
            "g-proof",
            sid,
        );
        assert_eq!(badge.ctx.window_tokens, Some(1_000_000));
        assert!(badge.ctx.used_tokens.is_some_and(|used| used > 0));
        eprintln!(
            "[real] session={sid} ctx={:?} badge={:?}",
            badge.ctx,
            badge.render()
        );
    }

    #[test]
    #[ignore = "需要已登录 Cursor；模拟首轮目录超时后真实续接，验证原 SID 与 1M"]
    fn real_cursor_timeout_fallback_resumes_same_context_at_one_million() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let source = dirs::home_dir().unwrap().join(".cursor/cli-config.json");
        let target = config.join("cli-config.json");
        std::fs::copy(source, &target).unwrap();
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let env = vec![
            ("CURSOR_CONFIG_DIR".into(), config.display().to_string()),
            ("TMPDIR".into(), tmp.path().display().to_string()),
        ];
        let work_dir = work.display().to_string();
        let fresh = TurnCtx {
            work_dir: Some(&work_dir),
            run_env: env.clone(),
            ..Default::default()
        };
        let marker = format!("SUMMON_FALLBACK_{}", uuid::Uuid::new_v4().simple());
        let first = Subprocess::new(LocalExecutor)
            .run_turn_with_hooks(
                CliDriver::Cursor,
                &format!("Remember the marker {marker}. Reply exactly STORED. Do not use tools."),
                &fresh,
                &mut |_| {},
                &mut |_| {},
                &mut |_| panic!("不允许工具"),
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(first.reply.as_deref(), Some("STORED"));
        let sid = first.session_id.as_deref().unwrap();
        let ctx = TurnCtx {
            session_id: Some(sid),
            work_dir: Some(&work_dir),
            model: Some(PARAMETER_MODEL),
            run_env: env.clone(),
            ..Default::default()
        };
        let mut exec = TimeoutExecutor::new();
        exec.real_retry = true;
        let mut process = Subprocess::new(exec);
        let recovered = process
            .run_turn_with_hooks(
                CliDriver::Cursor,
                "Reply exactly with the marker I asked you to remember. Do not use tools.",
                &ctx,
                &mut |_| {},
                &mut |_| {},
                &mut |_| panic!("不允许工具"),
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(recovered.reply.as_deref(), Some(marker.as_str()));
        assert_eq!(recovered.session_id.as_deref(), Some(sid));
        assert_eq!(process.exec.calls.borrow().len(), 2);
        let budget = super::super::cursor_context::budget_for_env(&env, sid);
        assert_eq!(budget.window_tokens, Some(1_000_000));
        assert!(budget.used_tokens.is_some_and(|used| used > 0));
        eprintln!("[real fallback] same_sid={sid} model={PARAMETER_MODEL} ctx={budget:?}");
    }

    /// Cursor Headless CLI Session 的真实契约探针：首轮拿到 SID，下一轮只靠 `--resume=<sid>`
    /// 读回上一轮上下文。它冻结的是 transport 能力，不是模型输出格式；失败时不能退回 ACP 冒充通过。
    #[test]
    #[ignore = "需要本机已登录 cursor-agent;跑法 cargo test -p summon -- --ignored real_cursor_cli_session_resumes_context"]
    fn real_cursor_cli_session_resumes_context() {
        let marker = format!("SUMMON_CURSOR_SESSION_{}", std::process::id());
        let mut first_reply = String::new();
        let mut source = Subprocess::new(LocalExecutor);
        let first = source
            .run_turn(
                CliDriver::Cursor,
                &format!("Remember this exact token for the next turn: {marker}. Reply only ACK."),
                &TurnCtx::default(),
                &mut |body| first_reply = body.to_string(),
                &mut |_stream| {},
                &mut |_tool| {},
                &mut |_event| {},
            )
            .unwrap();
        let session_id = first.session_id.expect("首轮必须返回 Cursor session id");
        assert!(first_reply.to_ascii_lowercase().contains("ack"));

        let mut resumed_reply = String::new();
        let second = source
            .run_turn(
                CliDriver::Cursor,
                "What exact token did I ask you to remember? Reply with the token only.",
                &TurnCtx {
                    session_id: Some(&session_id),
                    ..Default::default()
                },
                &mut |body| resumed_reply = body.to_string(),
                &mut |_stream| {},
                &mut |_tool| {},
                &mut |_event| {},
            )
            .unwrap();
        assert_eq!(second.session_id.as_deref(), Some(session_id.as_str()));
        assert!(
            resumed_reply.contains(&marker),
            "`--resume` 必须接回上一轮上下文，实得: {resumed_reply}"
        );
    }

    #[test]
    #[ignore = "需要本机已登录 claude;跑法 cargo test -p summon -- --ignored real_claude"]
    fn real_claude_subprocess_replies() {
        let mut got = String::new();
        let ctx = TurnCtx {
            session_id: None,
            work_dir: None,
            sys_prompt_file: None,
            ..Default::default()
        };
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .run_turn(
                CliDriver::Claude,
                "reply with exactly the single word: pong",
                &ctx,
                &mut |body| got = body.to_string(),
                &mut |_a| {},
                &mut |_t| {},
                &mut |_e| {},
            )
            .unwrap();
        assert!(out.session_id.is_some(), "应捕获 session_id");
        assert!(
            got.to_lowercase().contains("pong"),
            "回复含 pong, 实得: {got}"
        );
    }

    #[test]
    #[ignore = "需要本机已登录 codex;跑法 cargo test -p summon -- --ignored real_codex"]
    fn real_codex_subprocess_replies() {
        let mut got = String::new();
        let ctx = TurnCtx {
            session_id: None,
            work_dir: None,
            sys_prompt_file: None,
            ..Default::default()
        };
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .run_turn(
                CliDriver::Codex,
                "reply with exactly the single word: pong",
                &ctx,
                &mut |body| got = body.to_string(),
                &mut |_a| {},
                &mut |_t| {},
                &mut |_e| {},
            )
            .unwrap();
        assert!(out.session_id.is_some(), "应捕获 session_id");
        assert!(
            got.to_lowercase().contains("pong"),
            "回复含 pong, 实得: {got}"
        );
    }

    #[test]
    #[ignore = "需要本机已登录 codex;跑法 cargo test -p summon -- --ignored real_codex_skips_git"]
    fn real_codex_skips_git_repo_check_outside_repository() {
        let workspace = non_git_home_workspace();
        let mut got = String::new();
        let ctx = TurnCtx {
            work_dir: Some(workspace.path().to_str().unwrap()),
            codex_skip_git_repo_check: true,
            ..Default::default()
        };
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .run_turn(
                CliDriver::Codex,
                "reply with exactly the single word: pong",
                &ctx,
                &mut |body| got = body.to_string(),
                &mut |_a| {},
                &mut |_t| {},
                &mut |_e| {},
            )
            .unwrap();
        assert!(out.session_id.is_some(), "应捕获 session_id");
        assert!(
            got.to_lowercase().contains("pong"),
            "非 Git 目录应正常回复 pong，实得: {got}"
        );
    }

    #[test]
    #[ignore = "需要本机已登录 codex;跑法 cargo test -p summon -- --ignored real_codex_non_git_failure"]
    fn real_codex_non_git_failure_reports_stderr() {
        let workspace = non_git_home_workspace();
        let ctx = TurnCtx {
            work_dir: Some(workspace.path().to_str().unwrap()),
            ..Default::default()
        };
        let error = Subprocess::new(LocalExecutor)
            .run_turn(
                CliDriver::Codex,
                "reply with exactly the single word: pong",
                &ctx,
                &mut |_body| {},
                &mut |_a| {},
                &mut |_t| {},
                &mut |_e| {},
            )
            .expect_err("未开启 skip 时非 Git 目录应被 Codex 拒绝");
        let message = format!("{error:#}");
        assert!(message.contains("stderr 尾部"), "应标明错误来自 stderr");
        assert!(
            message.contains("trusted directory"),
            "应透传 Codex 的真实失败原因，实得: {message}"
        );
    }

    /// P3 真机冒烟:codex 带 model+effort 经 summon 自己的 Subprocess 路径跑一轮,验真实 codex CLI
    /// 接受 `-m <model>` + `-c model_reasoning_effort=<v>` 这两条 **exec 级** flag(summon 摆在 `exec`
    /// 之后:`codex exec --json -m <m> -c model_reasoning_effort=<e> <prompt>`)。
    ///
    /// 真机验证记录(诚实,2026-06-07):codex-cli **0.137.0**(`/opt/homebrew/bin/codex`)。
    /// - 人读形 `codex exec -m gpt-5.5 -c model_reasoning_effort=high "Reply with exactly: ok"` 的启动
    ///   banner 回显 `model: gpt-5.5` 与 `reasoning effort: high`,模型正常作答、exit 0 —— 证明两条 flag
    ///   都被解析并生效。
    /// - summon 实拼的 `--json` 形(本测走的就是它)输出干净 JSONL(thread.started/turn.started/
    ///   item.completed/turn.completed),无参数错。
    /// - `codex exec --help` 同时列出 `-m`/`-c`(top-level 与 exec 级都收),故摆 `exec` 之后合法。
    ///
    /// 注:`gpt-5.5` 是 owner 当前模型,换机/换模型时改这里的 model 串即可。
    #[test]
    #[ignore = "需要本机已登录 codex;跑法 cargo test -p summon -- --ignored real_codex_effort"]
    fn real_codex_effort_subprocess_replies() {
        let mut got = String::new();
        let ctx = TurnCtx {
            session_id: None,
            work_dir: None,
            sys_prompt_file: None,
            model: Some("gpt-5.5"),
            effort: Some("high"),
            ..Default::default()
        };
        let mut src = Subprocess::new(LocalExecutor);
        let out = src
            .run_turn(
                CliDriver::Codex,
                "reply with exactly the single word: pong",
                &ctx,
                &mut |body| got = body.to_string(),
                &mut |_a| {},
                &mut |_t| {},
                &mut |_e| {},
            )
            .unwrap();
        assert!(out.session_id.is_some(), "应捕获 session_id");
        assert!(
            got.to_lowercase().contains("pong"),
            "回复含 pong, 实得: {got}"
        );
    }
}
