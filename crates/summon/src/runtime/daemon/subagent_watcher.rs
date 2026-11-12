//! Subagent watcher:detached 线程 tail claude 转录 JSONL,检测 Agent/Workflow tool_use 的
//! spawn / 完成事件,周期性扫描 subagents/ 目录兜底,把结果维护进与 chat 线程共享的
//! `Arc<Mutex<SubagentState>>`。
//!
//! 与 session mirror watcher 平级、独立运行,互不阻塞。
//!
//! ## watcher 是**状态源**,不是投递方(owner 2026-08-14)
//!
//! 本 watcher **只写状态,不推任何东西**——它连 chat 线程的 `Sender` 都不持有。谁需要
//! subagent 信息,谁在自己的节奏上现读这份共享状态:
//!
//! - **live 流式卡尾部**:`TurnProjector::refresh_subagent_tail` 每次 emit 前现读 → 持续更新;
//! - **已定格的卡**(turn 卡或 `/session` 面板卡):由 chat 线程的刷新 tick 现读,但只刷群里
//!   **最新那张**——不是焦点的那些卡就此冻住。
//!
//! 早先这里有一个 `PanelSync`(快照比对 + trailing-flush debounce),每有状态变化就投
//! `RefreshSubagentPanel` 驱动面板 PATCH。owner 2026-08-14 要求退役的是这条**推送**通路
//! (每次状态变化都推,不管那张卡还是不是焦点);拉动式的「只刷最新那张」保留。
//! **退役的只有投递,不是状态机**
//! ——下面整套重放/对账/sid 跟随一行没减,它仍是 live 卡尾部与空闲判定的数据源。
//! 由此也不再需要记住面板卡的 message_id:渲染时机就是被告知卡 id 的那一刻。
//!
//! ## 历史正确性(2026-08-14 真机对账后重写)
//!
//! - watcher 冷启从 offset=0 **重放全量转录**——这是刻意的:launched/notified 的配对关系
//!   只有全量重放才能重建(中途开 tail 会把「早已完成」误判成「在跑」)。重放安全的前提是
//!   状态迁移带**事件时间戳**(summon-core 侧铁律),展示层按时效窗过滤,历史不会渲成「刚刚」。
//! - **sid 跟随**:每次目录扫描 tick 重读 `.session`。sid 轮换(tmux 内手动 /clear、
//!   /bridge 召回绑了别的会话)→ 整个状态机换血重放新转录,绝不把旧会话的子代理挂在新会话上。
//! - **权威对账**:主转录 `turn_duration` 行的 pendingBackgroundAgentCount/pendingWorkflowCount
//!   是 claude 自己的计数;「状态机 > 权威」方向的漂移落日志(权威 ≥ 状态机是通知排队语义的
//!   常态,不报,详见 `SubagentState::count_drift`)。

use super::*;
use summon_core::subagent::{
    agent_id_from_meta_filename, detect_launch, detect_resume, detect_send_message, detect_spawn,
    detect_sync_completed, detect_task_notification, detect_turn_duration, journal_counts,
    parse_subagent_meta, subagent_completed_from_last_line, workflow_name_from_script_filename,
    workflow_phases_from_script, DirSubagent, LaunchEvent, SubagentState, SubagentStatus,
    WorkflowDirInfo,
};

/// Subagent watcher 的 RAII guard:chat 线程退出时 Drop 停掉并 join watcher。
pub(crate) struct SubagentWatcherGuard {
    pub(super) stop: Arc<std::sync::atomic::AtomicBool>,
    pub(super) handle: Option<thread::JoinHandle<()>>,
}

impl Drop for SubagentWatcherGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 启动 subagent watcher detached 线程。返回 guard(Drop 时停掉 join)。
///
/// 仅对 claude Transcript 模式有意义;无头模式不调用。
///
/// **不收 chat 线程的 `Sender`**:watcher 只写共享状态,没有任何向面板投递的通路(见模块文档)。
pub(crate) fn spawn_subagent_watcher(
    agent: &Agent,
    sess: &Session,
    state: Arc<std::sync::Mutex<SubagentState>>,
) -> Option<SubagentWatcherGuard> {
    if agent.capture_mode() != CaptureMode::Transcript {
        return None;
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let a = agent.clone();
    let tmux_name = sess.tmux.session.clone();
    let chat_id = sess.chat_id.clone();
    let poll = sess.poll;
    let st = state.clone();
    let stop_clone = stop.clone();

    let handle = thread::spawn(move || {
        run_subagent_watcher(a, tmux_name, chat_id, poll, st, stop_clone);
    });

    Some(SubagentWatcherGuard {
        stop,
        handle: Some(handle),
    })
}

/// 一个 sid 对应的观测目标(转录路径 + subagents 目录)。
struct WatchTarget {
    sid: String,
    transcript_path: std::path::PathBuf,
    subagents_dir: std::path::PathBuf,
}

/// 解析当前 sid 的观测目标。转录路径解析失败 → None(下个 tick 再试)。
fn resolve_target(
    agent: &Agent,
    tmux_name: &str,
    chat_id: Option<&str>,
    sid: &str,
) -> Option<WatchTarget> {
    let transcript_path =
        crate::capture::resolve_transcript_path(agent, tmux_name, chat_id, None, sid).ok()?;
    let subagents_dir = transcript_path
        .parent()
        .map(|p| p.join(sid).join("subagents"))?;
    Some(WatchTarget {
        sid: sid.to_string(),
        transcript_path,
        subagents_dir,
    })
}

/// 生命周期动作同步读证据，不能把 watcher 尚未追平的空快照当成「没有子代理」。
pub(super) fn session_children_finished(agent: &Agent, sess: &Session) -> anyhow::Result<bool> {
    let sid = sess
        .native_session_id()
        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rff9ae719a6")))?;
    let target = resolve_target(agent, &sess.tmux.session, sess.chat_id.as_deref(), &sid)
        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r294dd17d8b")))?;
    let finished = session_children_finished_at(&target.transcript_path, &target.subagents_dir)?;
    anyhow::ensure!(
        sess.native_session_id().as_deref() == Some(&sid),
        summon_i18n::format!("rd1525fbac4")
    );
    Ok(finished)
}

struct ExitEvidence {
    files: std::collections::HashMap<PathBuf, (String, std::fs::Metadata)>,
    directories: Vec<(PathBuf, std::fs::Metadata)>,
    absent: Vec<PathBuf>,
    remaining: u64,
}

fn same_exit_file(before: &std::fs::Metadata, after: &std::fs::Metadata) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.modified()? == after.modified()?
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec())
}

impl ExitEvidence {
    fn read(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let before = file.metadata()?;
        anyhow::ensure!(
            before.is_file() && before.len() <= self.remaining,
            summon_i18n::format!("rebefb831c1")
        );
        let mut raw = String::new();
        (&file).take(self.remaining + 1).read_to_string(&mut raw)?;
        anyhow::ensure!(
            raw.len() as u64 <= self.remaining,
            summon_i18n::format!("re40049a37b")
        );
        anyhow::ensure!(
            same_exit_file(&before, &file.metadata()?)?,
            summon_i18n::format!("r561b150b68")
        );
        self.remaining -= raw.len() as u64;
        if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            anyhow::ensure!(
                raw.is_empty() || raw.ends_with('\n'),
                summon_i18n::format!("r1358f8b1e6")
            );
            for line in raw.lines().filter(|line| !line.trim().is_empty()) {
                serde_json::from_str::<serde_json::Value>(line)?;
            }
        } else {
            serde_json::from_str::<serde_json::Value>(&raw)?;
        }
        self.files.insert(path.to_path_buf(), (raw, before));
        Ok(())
    }

    fn collect(&mut self, dir: &std::path::Path, depth: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            depth <= 32 && self.files.len() + self.directories.len() < 10_000,
            summon_i18n::format!("ra11b4bb546")
        );
        let before = match std::fs::symlink_metadata(dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.absent.push(dir.to_path_buf());
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            before.is_dir() && !before.file_type().is_symlink(),
            summon_i18n::format!("r85f9b5cb83")
        );
        self.directories.push((dir.to_path_buf(), before));
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            anyhow::ensure!(!kind.is_symlink(), summon_i18n::format!("r6d1d880df4"));
            if kind.is_dir() {
                self.collect(&path, depth + 1)?;
            } else if matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("json" | "jsonl")
            ) {
                anyhow::ensure!(
                    self.files.len() + self.directories.len() < 10_000,
                    summon_i18n::format!("ra11b4bb546")
                );
                self.read(&path)?;
            }
        }
        Ok(())
    }

    fn ensure_unchanged(&self) -> anyhow::Result<()> {
        for (path, before) in self
            .files
            .iter()
            .map(|(path, (_, metadata))| (path, metadata))
            .chain(
                self.directories
                    .iter()
                    .map(|(path, metadata)| (path, metadata)),
            )
        {
            anyhow::ensure!(
                same_exit_file(before, &std::fs::metadata(path)?)?,
                summon_i18n::format!("r0fdf02068d")
            );
        }
        for path in &self.absent {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => anyhow::bail!(summon_i18n::format!("rfed76369f9")),
            }
        }
        Ok(())
    }
}

fn session_children_finished_at(
    transcript: &std::path::Path,
    subagents_dir: &std::path::Path,
) -> anyhow::Result<bool> {
    let mut evidence = ExitEvidence {
        files: Default::default(),
        directories: Vec::new(),
        absent: Vec::new(),
        remaining: 64 * 1024 * 1024,
    };
    evidence.read(transcript)?;
    evidence.collect(subagents_dir, 0)?;
    let raw = &evidence
        .files
        .get(transcript)
        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r249c3dad53")))?
        .0;
    anyhow::ensure!(!raw.trim().is_empty(), summon_i18n::format!("rbcfee38b59"));
    let mut state = SubagentState::new();
    for line in raw.lines() {
        consume_transcript_line(&mut state, line);
    }
    let mut entries = Vec::new();
    let mut ids = std::collections::HashSet::new();
    let mut pending_journal = false;
    let mut pending_child_log = false;
    for (path, (raw, metadata)) in &evidence.files {
        if path != transcript
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("agent-") && name.ends_with(".jsonl"))
        {
            pending_child_log |= !raw
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .is_some_and(subagent_completed_from_last_line);
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("journal.jsonl") {
            let counts = journal_counts(raw);
            pending_journal |= counts.started > counts.done;
        }
        let Some(id) = agent_id_from_meta_filename(path) else {
            continue;
        };
        anyhow::ensure!(ids.insert(id.clone()), summon_i18n::format!("ra2ce8059cc"));
        let child_log = path.with_file_name(format!("agent-{id}.jsonl"));
        let completed = evidence
            .files
            .get(&child_log)
            .and_then(|(raw, _)| raw.lines().rev().find(|line| !line.trim().is_empty()))
            .is_some_and(subagent_completed_from_last_line);
        let at = chrono::DateTime::<chrono::Utc>::from(metadata.modified()?);
        let entry = parse_subagent_meta(
            raw,
            &id,
            completed,
            at,
            evidence
                .files
                .get(&child_log)
                .map(|(_, metadata)| metadata.modified())
                .transpose()?
                .map(chrono::DateTime::<chrono::Utc>::from),
            summon_i18n::text!("racd37b4c79"),
        )
        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rea53b91962")))?;
        entries.push(entry);
    }
    state.reconcile_from_dir(entries);
    let unconfirmed_workflow = evidence.directories.iter().any(|(path, _)| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("wf-")
                    && !state
                        .workflows
                        .iter()
                        .any(|workflow| workflow.run_id == name && workflow.is_finished())
            })
    });
    // 展示窗口过期不等于工作已完成；生命周期动作只接受明确终态。
    let pending = state.agents.iter().any(SubagentStatus::is_active)
        || state
            .workflows
            .iter()
            .any(|workflow| !workflow.is_finished())
        || pending_journal
        || pending_child_log
        || unconfirmed_workflow
        || state.authority.as_ref().is_some_and(|authority| {
            authority.pending_background.unwrap_or(0) > 0
                || authority.pending_workflows.unwrap_or(0) > 0
        });
    evidence.ensure_unchanged()?;
    Ok(!pending)
}

/// watcher 主循环。
fn run_subagent_watcher(
    agent: Agent,
    tmux_name: String,
    chat_id: Option<String>,
    poll: Duration,
    state: Arc<std::sync::Mutex<SubagentState>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering::Relaxed;

    // 建 Session(带 chat_id 以便 native_session_id 读 .session 文件)。
    let mut sess = Session::with_name(&agent, tmux_name.clone(), poll.as_millis() as u64, 800);
    if let Some(cid) = &chat_id {
        sess = sess.with_chat_id(cid);
    }

    // 等 native_session_id 就位(与 mirror watcher 同构的退避等待)。
    let mut wait = Duration::from_millis(500);
    while !stop.load(Relaxed) {
        if sess.native_session_id().is_some() {
            break;
        }
        thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_secs(5));
    }
    if stop.load(Relaxed) {
        return;
    }
    let Some(sid) = sess.native_session_id() else {
        return;
    };
    let Some(mut target) = resolve_target(&agent, &tmux_name, chat_id.as_deref(), &sid) else {
        log(&summon_i18n::format!("r358ef32281", agent.name));
        return;
    };

    log(&summon_i18n::format!(
        "r326c984fc0",
        agent.name,
        target.sid,
        target.subagents_dir.display()
    ));

    let mut offset: u64 = 0;
    let mut last_dir_scan = Instant::now();
    // 上次已上报的漂移文案:同一漂移只报一次,变化了才再报(避免每 poll 刷屏)。
    let mut last_drift: Option<String> = None;
    let dir_scan_interval = Duration::from_secs(5);

    while !stop.load(Relaxed) {
        // 1) tail 转录,检测 spawn / completion / 权威计数事件。
        match tail_jsonl_lines(&target.transcript_path, offset) {
            Ok((lines, new_offset)) => {
                if !lines.is_empty() {
                    {
                        let mut st = lock_state(&state);
                        for line in &lines {
                            consume_transcript_line(&mut st, line);
                        }
                    }
                    // 权威对账:漂移变化才落日志(排队语义方向不报,见 count_drift)。
                    let drift = lock_state(&state).count_drift();
                    if drift != last_drift {
                        if let Some(d) = &drift {
                            log(&summon_i18n::format!("r4152b7a0a9", agent.name, target.sid));
                        }
                        last_drift = drift;
                    }
                }
                offset = new_offset;
            }
            Err(e) => {
                // 转录文件可能还没创建(冷启中),best-effort log
                if offset == 0 {
                    // 首次就读不到,可能 session 还没真正起来,不刷屏
                } else {
                    log(&summon_i18n::format!("ra5dfddc9c4", agent.name));
                }
            }
        }

        // 2) 每 5s 一次:sid 跟随检查 + subagents/ 目录兜底扫描。
        if last_dir_scan.elapsed() >= dir_scan_interval {
            last_dir_scan = Instant::now();

            // sid 轮换(/clear、/bridge 召回)→ 换血:清状态、换目标、重放新转录。
            // 旧会话的子代理挂在新会话上,正是「面板列表与实际不一致」的历史事故形态之一。
            // 清状态是**就地写共享状态**,读侧(live 卡尾部 / 下次唤面板)下一次现读就看到空,
            // 不经任何队列,所以不存在「清屏消息恰逢 debounce 窗内被吞」这类投递丢失。
            if let Some(current_sid) = sess.native_session_id() {
                if current_sid != target.sid {
                    if let Some(new_target) =
                        resolve_target(&agent, &tmux_name, chat_id.as_deref(), &current_sid)
                    {
                        log(&summon_i18n::format!(
                            "r10e0524aee",
                            agent.name,
                            target.sid,
                            current_sid
                        ));
                        target = new_target;
                        offset = 0;
                        last_drift = None;
                        *lock_state(&state) = SubagentState::new();
                        continue; // 立刻从新转录开始重放
                    }
                }
            }

            let scan = scan_subagents_dir(&target.subagents_dir);
            {
                let mut st = lock_state(&state);
                st.reconcile_from_dir(scan.top);
                for wf in scan.workflows {
                    st.reconcile_workflow_dir(wf);
                }
            }
            // 展示窗到期(Completed 30s / Failed 30min / 活跃 2h)不需要 watcher 做任何事:
            // 过滤发生在**读侧**的 `display(now)`,时间一到读到的就是过滤后的结果。
            // 早先要在这里无条件重估快照,是因为面板卡靠 push 刷新——push 没了,这一步也就没了。
        }

        thread::sleep(poll);
    }

    log(&summon_i18n::format!("r04103e04c1", agent.name, target.sid));
}

/// watcher 线程 panic 会毒化锁;状态是纯观测副作用,恢复内层值继续。
fn lock_state(state: &std::sync::Mutex<SubagentState>) -> std::sync::MutexGuard<'_, SubagentState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 消费一行主转录:spawn / launch / 同步完成 / 通知 / 权威计数。返回是否引起展示状态变化。
/// **所有迁移带行时间戳**(缺席回落「现在」——真机验证事件行 100% 带 timestamp)。
fn consume_transcript_line(st: &mut SubagentState, line: &str) -> bool {
    let mut changed = false;
    if let Some(ev) = detect_spawn(line) {
        let at = ev.at.unwrap_or_else(chrono::Utc::now);
        changed |= st.insert_spawning(ev.description, ev.agent_type, ev.tool_use_id, ev.model, at);
    }
    if let Some(launch) = detect_launch(line) {
        match launch {
            LaunchEvent::Agent {
                agent_id,
                description,
                tool_use_id,
                model,
                at,
            } => {
                let at = at.unwrap_or_else(chrono::Utc::now);
                changed |= st.promote_to_running(
                    &agent_id,
                    &description,
                    &tool_use_id,
                    model.as_deref(),
                    at,
                );
            }
            LaunchEvent::Workflow {
                task_id,
                run_id,
                name,
                tool_use_id,
                at,
            } => {
                let at = at.unwrap_or_else(chrono::Utc::now);
                changed |= st.workflow_launched(&task_id, &run_id, &name, &tool_use_id, at);
            }
        }
    }
    if let Some(sync) = detect_sync_completed(line) {
        let at = sync.at.unwrap_or_else(chrono::Utc::now);
        changed |=
            st.mark_sync_completed(&sync.agent_id, &sync.description, sync.model.as_deref(), at);
    }
    // SendMessage 续用/改名(assistant 行):summary 是该 subagent 的最新活动标签。
    if let Some(sm) = detect_send_message(line) {
        if let Some(summary) = &sm.summary {
            changed |= st.rename(&sm.to, summary);
        }
    }
    // 续用确认(user 行):已终态的 subagent 被重新激活。Form C(queued)对已终态 agent
    // 意味着必然复活且之后不再留记录——入队即激活;发给还在跑的/未知 id 天然 no-op。
    if let Some(rs) = detect_resume(line) {
        let at = rs.at.unwrap_or_else(chrono::Utc::now);
        changed |= st.reactivate(&rs.agent_id, at);
    }
    if let Some((task_id, status, summary, at)) = detect_task_notification(line) {
        let at = at.unwrap_or_else(chrono::Utc::now);
        changed |= match status.as_str() {
            // 未知 id(后台 shell 任务等)在状态机侧被拒,不建条目;workflow 按 taskId 收敛。
            "completed" => {
                st.mark_completed(&task_id, at) || st.finish_workflow(&task_id, false, at)
            }
            "failed" => {
                st.mark_failed(&task_id, &summary, at) || st.finish_workflow(&task_id, true, at)
            }
            _ => false,
        };
    }
    if let Some(auth) = detect_turn_duration(line) {
        // 权威计数只做对账,不驱动展示 → 不算 changed。
        st.note_authority(auth);
    }
    changed
}

/// 读取转录文件从 offset 开始的**完整**新行。返回 (行列表, 新 offset)。
/// EOF 处半写行既不交给解析器也不推进 offset；下轮从原位置重读，避免永久吞掉 spawn/完成事件。
fn tail_jsonl_lines(path: &std::path::Path, offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    use std::io::{BufRead, Seek};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if offset == 0 => return Err(e), // 文件还不存在
        Err(e) => return Err(e),
    };
    f.seek(std::io::SeekFrom::Start(offset))?;
    let mut reader = std::io::BufReader::new(f);
    let mut lines = Vec::new();
    let mut new_offset = offset;
    loop {
        let mut raw = Vec::new();
        let n = reader.read_until(b'\n', &mut raw)?;
        if n == 0 {
            break;
        }
        if raw.last() != Some(&b'\n') {
            break; // 半写行：不消费，下一 poll 原位重读。
        }
        new_offset += n as u64;
        raw.pop();
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
        let line = String::from_utf8(raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if !line.is_empty() {
            lines.push(line);
        }
    }
    Ok((lines, new_offset))
}

/// 目录里一个 `agent-*.meta.json` 对应的原始条目(未套 fallback 描述、未过时效闸)。
struct RawAgentEntry {
    agent_id: String,
    meta_content: String,
    completed: bool,
    started_at: chrono::DateTime<chrono::Utc>,
    last_activity: Option<chrono::DateTime<chrono::Utc>>,
}

/// 收集单个目录(不递归)下的 agent 条目。
fn collect_agent_entries(dir: &std::path::Path) -> Vec<RawAgentEntry> {
    let mut out = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(agent_id) = agent_id_from_meta_filename(&path) else {
            continue;
        };

        // 读 meta.json
        let meta_content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // 查对应的 .jsonl:末行判是否完成;mtime 当 last_activity(最近一次写转录 ≈ 最近活跃)。
        let jsonl_path = path.with_file_name(format!("agent-{agent_id}.jsonl"));
        let completed = std::fs::File::open(&jsonl_path)
            .ok()
            .and_then(|f| {
                use std::io::BufRead;
                let reader = std::io::BufReader::new(f);
                let mut last = String::new();
                for line in reader.lines().map_while(Result::ok) {
                    if !line.is_empty() {
                        last = line;
                    }
                }
                if last.is_empty() {
                    None
                } else {
                    Some(subagent_completed_from_last_line(&last))
                }
            })
            .unwrap_or(false);
        let last_activity = std::fs::metadata(&jsonl_path)
            .and_then(|m| m.modified())
            .ok()
            .map(chrono::DateTime::<chrono::Utc>::from);

        // 用 meta.json 的 mtime 作为 started_at
        let started_at = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(chrono::DateTime::<chrono::Utc>::from)
            .unwrap_or_else(|_| chrono::Utc::now());

        out.push(RawAgentEntry {
            agent_id,
            meta_content,
            completed,
            started_at,
            last_activity,
        });
    }
    out
}

/// 套 fallback 描述 + 时效闸后收进结果集。
fn push_if_live(
    out: &mut Vec<DirSubagent>,
    raw: RawAgentEntry,
    fallback_description: &str,
    now: chrono::DateTime<chrono::Utc>,
) {
    let Some(entry) = parse_subagent_meta(
        &raw.meta_content,
        &raw.agent_id,
        raw.completed,
        raw.started_at,
        raw.last_activity,
        fallback_description,
    ) else {
        return;
    };
    // 超期且无完成证据 → 是上一个会话(或上一次 workflow)的残留,不是「活跃」子代理。
    // 判活轴是 last_activity(jsonl mtime),拿不到才回落 started_at。
    if summon_core::subagent::dir_subagent_is_live(
        entry.completed,
        entry.started_at,
        entry.last_activity,
        now,
        summon_core::subagent::DIR_SUBAGENT_STALE_AFTER,
    ) {
        out.push(entry);
    }
}

/// 按 `<name>-<wf目录名>.js` 的文件名在会话的 scripts 目录里反解 workflow 名与脚本路径。
fn resolve_workflow_script(
    scripts_dir: &std::path::Path,
    wf_dir_name: &str,
) -> Option<(String, std::path::PathBuf)> {
    for e in std::fs::read_dir(scripts_dir).ok()?.flatten() {
        let file_name = e.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some(wf) = workflow_name_from_script_filename(name, wf_dir_name) {
            return Some((wf, e.path()));
        }
    }
    None
}

/// 目录扫描结果:顶层条目 + workflow 组。
struct DirScan {
    top: Vec<DirSubagent>,
    workflows: Vec<WorkflowDirInfo>,
}

/// 把 DirSubagent 投影成 workflow 组的子代理展示条目。
fn dir_entry_to_status(entry: DirSubagent) -> SubagentStatus {
    if entry.completed {
        SubagentStatus::Completed {
            agent_id: entry.agent_id,
            description: entry.description,
            finished_at: entry.last_activity.unwrap_or(entry.started_at),
            model: entry.model,
        }
    } else {
        SubagentStatus::Running {
            agent_id: entry.agent_id,
            description: entry.description,
            agent_type: entry.agent_type,
            tool_use_id: entry.tool_use_id,
            started_at: entry.started_at,
            last_activity: entry.last_activity,
            model: entry.model,
        }
    }
}

/// 扫描 subagents/ 目录,返回顶层子代理条目 + workflow 组信息。
fn scan_subagents_dir(dir: &std::path::Path) -> DirScan {
    let now = chrono::Utc::now();
    let mut top = Vec::new();
    let mut workflows = Vec::new();

    // 顶层 Agent 工具起的子代理:meta.json 正常带 description(即 Agent 工具的 description 参数)。
    for raw in collect_agent_entries(dir) {
        push_if_live(&mut top, raw, summon_i18n::text!("rb30520db75"), now);
    }

    // workflow 扇出的子代理:meta.json 实测形如 {"agentType":"workflow-subagent","spawnDepth":1,
    // "model":"opus"} —— 无 description → 描述回落「<workflow名> #<序号>」。workflow 名从会话目录
    // workflows/scripts/<name>-<wf目录名>.js 的文件名反解(subagents/ 的兄弟目录);
    // 反解不出(脚本被清/布局变了)再回落「workflow 子代理 #N」,不再出现「(无描述)」。
    // 阶段总数从脚本头 meta.phases 数;journal.jsonl 记 started/result 配对计数。
    let scripts_dir = dir.parent().map(|p| p.join("workflows").join("scripts"));
    if let Ok(wf_read) = std::fs::read_dir(dir.join("workflows")) {
        for wf_entry in wf_read.flatten() {
            let wf_path = wf_entry.path();
            if !wf_path.is_dir() {
                continue;
            }
            let wf_dir_name = wf_entry.file_name().to_string_lossy().into_owned();
            let script = scripts_dir
                .as_deref()
                .and_then(|sd| resolve_workflow_script(sd, &wf_dir_name));
            let wf_name = script.as_ref().map(|(n, _)| n.clone());
            let phase_total = script
                .as_ref()
                .and_then(|(_, p)| std::fs::read_to_string(p).ok())
                .as_deref()
                .and_then(workflow_phases_from_script);

            let journal_path = wf_path.join("journal.jsonl");
            let counts = std::fs::read_to_string(&journal_path)
                .map(|c| journal_counts(&c))
                .unwrap_or_default();
            let journal_mtime = std::fs::metadata(&journal_path)
                .and_then(|m| m.modified())
                .ok()
                .map(chrono::DateTime::<chrono::Utc>::from);
            let started_at = wf_path
                .metadata()
                .and_then(|m| m.created().or_else(|_| m.modified()))
                .map(chrono::DateTime::<chrono::Utc>::from)
                .unwrap_or(now);

            let mut raw_entries = collect_agent_entries(&wf_path);
            // 序号按启动先后(meta mtime)定序,agent_id 兜平局,跨扫描稳定。
            raw_entries.sort_by(|a, b| {
                a.started_at
                    .cmp(&b.started_at)
                    .then_with(|| a.agent_id.cmp(&b.agent_id))
            });
            let mut children_entries = Vec::new();
            for (idx, raw) in raw_entries.into_iter().enumerate() {
                let fallback = match &wf_name {
                    Some(n) => format!("{n} #{}", idx + 1),
                    None => summon_i18n::format!("rc26ad2b6ee", idx + 1),
                };
                push_if_live(&mut children_entries, raw, &fallback, now);
            }
            let children: Vec<SubagentStatus> = children_entries
                .into_iter()
                .map(dir_entry_to_status)
                .collect();

            workflows.push(WorkflowDirInfo {
                run_id: wf_dir_name,
                name: wf_name,
                phase_total,
                journal_started: counts.started,
                journal_done: counts.done,
                journal_mtime,
                started_at,
                children,
            });
        }
    }

    DirScan { top, workflows }
}

#[cfg(test)]
mod tests {
    use super::{consume_transcript_line, scan_subagents_dir, tail_jsonl_lines};
    use std::io::Write as _;
    use summon_core::subagent::{SubagentState, SubagentStatus};

    /// 目录扫描:顶层子代理取 meta 里的 description + jsonl mtime 当 last_activity;
    /// workflow 扇出的子代理(meta 无 description、有 model)按 scripts/ 里的脚本文件名回落
    /// 「<workflow名> #<序号>」,并归进该 run 的组(不再平铺进顶层)。
    #[test]
    fn exit_probe_replays_spawn_before_the_watcher_has_caught_up() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        let line = serde_json::json!({
            "type": "assistant", "timestamp": "2020-01-01T00:00:00Z",
            "message": { "content": [{"type": "tool_use", "id": "tu-active",
                "name": "Agent", "input": {"description": "still working", "subagent_type": "general-purpose"}}] }
        });
        std::fs::write(&transcript, format!("{line}\n")).unwrap();
        assert!(
            !super::session_children_finished_at(&transcript, &tmp.path().join("subagents"))
                .unwrap()
        );
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        assert!(
            super::session_children_finished_at(&transcript, &tmp.path().join("subagents"))
                .unwrap()
        );
    }

    #[test]
    fn exit_probe_requires_explicit_child_and_workflow_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        let subagents = tmp.path().join("subagents");
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        std::fs::create_dir(&subagents).unwrap();
        let child = subagents.join("agent-child.jsonl");
        std::fs::write(&child, "{\"type\":\"assistant\"}\n").unwrap();
        assert!(!super::session_children_finished_at(&transcript, &subagents).unwrap());
        let complete =
            serde_json::json!({"type":"assistant", "message":{"stop_reason":"end_turn"}});
        std::fs::write(&child, format!("{complete}\n")).unwrap();
        assert!(super::session_children_finished_at(&transcript, &subagents).unwrap());
        std::fs::create_dir(subagents.join("wf-unconfirmed")).unwrap();
        assert!(!super::session_children_finished_at(&transcript, &subagents).unwrap());
    }

    #[test]
    fn exit_evidence_rejects_changes_after_reading_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        let mut evidence = super::ExitEvidence {
            files: Default::default(),
            directories: Vec::new(),
            absent: Vec::new(),
            remaining: 1024,
        };
        evidence.read(&transcript).unwrap();
        evidence.ensure_unchanged().unwrap();
        std::fs::write(&transcript, "{\"type\":\"user\",\"new_work\":true}\n").unwrap();
        assert!(evidence.ensure_unchanged().is_err());
    }

    #[test]
    fn exit_probe_refuses_missing_corrupt_partial_and_oversized_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        let subagents = tmp.path().join("subagents");
        assert!(super::session_children_finished_at(&transcript, &subagents).is_err());
        for raw in ["", "not-json\n", "{\"type\":\"user\"}"] {
            std::fs::write(&transcript, raw).unwrap();
            assert!(super::session_children_finished_at(&transcript, &subagents).is_err());
        }
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        std::fs::create_dir(&subagents).unwrap();
        std::fs::write(subagents.join("agent-a.meta.json"), "broken").unwrap();
        assert!(super::session_children_finished_at(&transcript, &subagents).is_err());
        let file = std::fs::File::create(&transcript).unwrap();
        file.set_len(64 * 1024 * 1024 + 1).unwrap();
        assert!(super::session_children_finished_at(&transcript, &subagents).is_err());
    }

    #[test]
    fn scan_dir_groups_workflow_children_with_counts_and_model() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path();
        let subagents = session.join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();

        // 顶层子代理:meta 带 description,jsonl 在写
        std::fs::write(
            subagents.join("agent-a111.meta.json"),
            r#"{"agentType":"Explore","description":"顶层任务","toolUseId":"t1"}"#,
        )
        .unwrap();
        std::fs::write(subagents.join("agent-a111.jsonl"), "{\"type\":\"user\"}\n").unwrap();

        // workflow 扇出的子代理:meta 无 description、带 model(实测格式)
        let wf = subagents.join("workflows").join("wf_x1");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(
            wf.join("agent-a222.meta.json"),
            r#"{"agentType":"workflow-subagent","spawnDepth":1,"model":"opus"}"#,
        )
        .unwrap();
        std::fs::write(wf.join("agent-a222.jsonl"), "{\"type\":\"user\"}\n").unwrap();
        // journal:1 started 0 result
        std::fs::write(
            wf.join("journal.jsonl"),
            "{\"type\":\"started\",\"key\":\"v2:k\",\"agentId\":\"a222\"}\n",
        )
        .unwrap();
        // 会话目录下的 workflow 脚本(名字来源:文件名 = <name>-<wf目录名>.js;phases 供总数)
        let scripts = session.join("workflows").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(
            scripts.join("code-review-wf_x1.js"),
            "export const meta = { name: 'code-review', phases: [ { title: 'A' }, { title: 'B' } ] }",
        )
        .unwrap();

        let scan = scan_subagents_dir(&subagents);
        assert_eq!(scan.top.len(), 1, "顶层只有 1 条,workflow 子代理不平铺");
        assert_eq!(scan.top[0].description, "顶层任务");
        assert!(
            scan.top[0].last_activity.is_some(),
            "jsonl 存在应取其 mtime"
        );

        assert_eq!(scan.workflows.len(), 1);
        let wf_info = &scan.workflows[0];
        assert_eq!(wf_info.run_id, "wf_x1");
        assert_eq!(wf_info.name.as_deref(), Some("code-review"));
        assert_eq!(wf_info.phase_total, Some(2));
        assert_eq!((wf_info.journal_started, wf_info.journal_done), (1, 0));
        assert_eq!(wf_info.children.len(), 1);
        assert_eq!(wf_info.children[0].description(), "code-review #1");
        assert_eq!(
            wf_info.children[0].model(),
            Some("opus"),
            "model 采自 meta.json"
        );
    }

    /// scripts 目录缺失(归属反解不出)→ 回落「workflow 子代理 #N」,序号跨扫描稳定;
    /// jsonl 还没落盘的条目 last_activity 是 None。
    #[test]
    fn scan_dir_workflow_without_script_falls_back_to_generic_numbered() {
        let root = tempfile::tempdir().unwrap();
        let subagents = root.path().join("subagents");
        let wf = subagents.join("workflows").join("wf_y2");
        std::fs::create_dir_all(&wf).unwrap();
        let meta = r#"{"agentType":"workflow-subagent","spawnDepth":1}"#;
        std::fs::write(wf.join("agent-aaa.meta.json"), meta).unwrap();
        std::fs::write(wf.join("agent-aaa.jsonl"), "{\"type\":\"user\"}\n").unwrap();
        std::fs::write(wf.join("agent-bbb.meta.json"), meta).unwrap();
        // agent-bbb 的 jsonl 故意不建

        let scan = scan_subagents_dir(&subagents);
        assert_eq!(scan.workflows.len(), 1);
        let children = &scan.workflows[0].children;
        assert_eq!(children.len(), 2);
        // mtime 几乎同时 → 按 agent_id 兜平局,aaa=#1 bbb=#2 稳定
        let a = children
            .iter()
            .find(|e| e.agent_id() == Some("aaa"))
            .unwrap();
        let b = children
            .iter()
            .find(|e| e.agent_id() == Some("bbb"))
            .unwrap();
        assert!(
            a.description().ends_with("#1") && b.description().ends_with("#2"),
            "序号应稳定: {} / {}",
            a.description(),
            b.description()
        );
        assert!(
            a.description().starts_with("workflow 子代理"),
            "{}",
            a.description()
        );
        assert!(scan.workflows[0].name.is_none());
        assert!(
            scan.workflows[0].phase_total.is_none(),
            "拿不到脚本不猜阶段数"
        );
    }

    /// owner 2026-08-14:面板卡只渲一次当时快照,**subagent 状态变化后不得再刷面板**。
    ///
    /// 钉死方式是结构性的:watcher 不持有任何 `Sender<ThreadMsg>`,也就没有向 chat 线程
    /// 投递刷新的通路(旧实现是 `PanelSync` → `ThreadMsg::RefreshSubagentPanel`)。本测试扫
    /// 本文件**非测试部分**的源码,断言零投递面;同时断言状态机锚点仍在——退役的只有投递,
    /// 状态机不许被顺手拆掉(它仍是 live 卡尾部与空闲判定的数据源)。
    ///
    /// 变异自验:把持续驱动接回去(重新 `tx.send(ThreadMsg::…)`)→ 本测试必红。
    #[test]
    fn watcher_has_no_panel_push_channel() {
        let src = include_str!("subagent_watcher.rs");
        // 只看**代码**:注释里出现这些词是在解释设计,不构成通路。
        let prod: String = src
            .split_once("#[cfg(test)]")
            .expect("本文件必须有测试模块分界")
            .0
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // 非空自检:锚点读不到 = 扫描器失效,本测试会恒绿。
        assert!(
            prod.contains("fn run_subagent_watcher"),
            "扫描到的不是 watcher 源码"
        );
        assert!(
            prod.contains("SubagentState") && prod.contains("reconcile_from_dir"),
            "状态机被拆了:watcher 仍必须是 live 卡尾部/空闲判定的数据源"
        );

        for forbidden in ["ThreadMsg", "Sender<", ".send(", "RefreshSubagentPanel"] {
            assert!(
                !prod.contains(forbidden),
                "watcher 不得重新获得面板投递通路,发现 `{forbidden}`:\
                 卡片刷新是 chat 线程 tick 拉动的(只刷最新那张),不是 watcher 推的"
            );
        }
    }

    /// 展示窗到期(Completed 30s / Failed 30min / 活跃 2h)不再需要任何 push:过滤发生在
    /// **读侧** `display(now)`,同一份状态在不同时刻读出不同结果。所以到期本身不用通知谁——
    /// 每个读侧(live 卡每次 emit、已定格卡每个 tick)读到的都是那一刻的正确结果。
    #[test]
    fn display_window_expiry_is_evaluated_at_read_time() {
        let t0 = chrono::Utc::now();
        let mut st = SubagentState::new();
        st.promote_to_running(
            "a1",
            "刚完成的",
            "t1",
            None,
            t0 - chrono::Duration::minutes(1),
        );
        st.mark_completed("a1", t0);

        // 完成后 1s 读:条目可见。
        assert_eq!(
            st.display(t0 + chrono::Duration::seconds(1)).agents.len(),
            1
        );
        // 状态**一行没变**,只是读的时刻晚了 31s:同一份状态读出空快照。
        assert!(
            st.display(t0 + chrono::Duration::seconds(31)).is_empty(),
            "到期过滤必须发生在读侧,不依赖任何 watcher 推送"
        );
    }

    #[test]
    fn tail_keeps_partial_line_for_next_poll() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, b"{\"one\":1}\n{\"two\":").unwrap();

        let (lines, offset) = tail_jsonl_lines(&path, 0).unwrap();
        assert_eq!(lines, ["{\"one\":1}"]);
        assert_eq!(offset, b"{\"one\":1}\n".len() as u64);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"2}\n").unwrap();
        let (lines, next) = tail_jsonl_lines(&path, offset).unwrap();
        assert_eq!(lines, ["{\"two\":2}"]);
        assert_eq!(next, std::fs::metadata(path).unwrap().len());
    }

    /// 真机对账验收(owner 点名的历史正确性验收,可重复执行):
    /// `SUMMON_SUBAGENT_RECON_SESSION=~/.claude/projects/<slug>/<sid>` 指向一个真实 claude
    /// 会话目录(主转录在 `<sid>.jsonl` 同级),对它做**全量重放 + 目录扫描**,然后三方对账:
    /// ① 转录配对(async_launched − task-notification)= 状态机 Running 集合;
    /// ② `turn_duration.pendingBackgroundAgentCount` 权威计数:不得出现「状态机 > 权威」漂移;
    /// ③ 展示层:历史(超时效窗)条目一律不可见。
    /// 跑法:`SUMMON_SUBAGENT_RECON_SESSION=... cargo test -p summon real_machine_subagent_reconciliation -- --ignored --nocapture`
    #[test]
    #[ignore = "真机对账:需要 SUMMON_SUBAGENT_RECON_SESSION 指向 ~/.claude/projects/<slug>/<sid>"]
    fn real_machine_subagent_reconciliation() {
        let Ok(session_dir) = std::env::var("SUMMON_SUBAGENT_RECON_SESSION") else {
            panic!("需要 SUMMON_SUBAGENT_RECON_SESSION");
        };
        let session_dir = std::path::PathBuf::from(session_dir);
        let transcript = session_dir.with_extension("jsonl");
        let content = std::fs::read_to_string(&transcript).expect("主转录必须可读");

        let mut st = SubagentState::new();
        for line in content.lines().filter(|l| !l.is_empty()) {
            consume_transcript_line(&mut st, line);
        }
        let scan = scan_subagents_dir(&session_dir.join("subagents"));
        let n_top = scan.top.len();
        let n_wf = scan.workflows.len();
        st.reconcile_from_dir(scan.top);
        for wf in scan.workflows {
            st.reconcile_workflow_dir(wf);
        }

        let now = chrono::Utc::now();
        let running: Vec<String> = st
            .agents
            .iter()
            .filter(|a| matches!(a, SubagentStatus::Running { .. }))
            .map(|a| format!("{} {:?} act={:?}", a.description(), a.model(), a.agent_id()))
            .collect();
        let display = st.display(now);
        println!("== 真机对账 {} ==", transcript.display());
        println!(
            "状态机全量: agents={} workflows={}",
            st.agents.len(),
            st.workflows.len()
        );
        println!("目录扫描(过闸后): top={n_top} wf_runs={n_wf}");
        println!("Running 集合({}): {running:#?}", running.len());
        println!("权威计数(最后一条 turn_duration): {:?}", st.authority);
        println!(
            "展示层: agents={} workflows={}",
            display.agents.len(),
            display.workflows.len()
        );
        for a in &display.agents {
            println!(
                "  - {} model={:?} {}",
                a.description(),
                a.model(),
                a.elapsed_human(now)
            );
        }
        for w in &display.workflows {
            println!("  - {}", w.header_line(now));
        }
        println!(
            "live 卡尾部: {:#?}",
            summon_core::subagent::subagent_tail_lines(&display, 3)
        );

        // ── 三方对账。期望集合由**独立解析器**重建(raw serde_json 手扒字段,不经生产
        // detect_*/状态机)——生产代码错了它不会跟着错,不存在「状态机验证自己」。
        let ledger = independent_ledger(&content);
        println!(
            "独立台账: launched_agents={} launched_workflows={} notified={} 时序在跑 agents={:?} workflows={:?} pending_bg={:?} pending_wf={:?}",
            ledger.launched_agents.len(),
            ledger.launched_workflows.len(),
            ledger.notified.len(),
            ledger.running_agents,
            ledger.running_workflows,
            ledger.last_pending_bg,
            ledger.last_pending_wf
        );

        // ① 配对基准 = 独立时序台账(launched/resumed/queued 置入、notification 移除,按行序),
        // 与状态机 Running 集合**双向相等**。静态差集 launched−notified 不够:它看不见被
        // SendMessage 续起的 agent(真机 owner 截图事故:TUI 在跑、卡片缺失)。
        let expected_running = ledger.running_agents.clone();
        let machine_running: std::collections::BTreeSet<String> = st
            .agents
            .iter()
            .filter(|a| matches!(a, SubagentStatus::Running { .. }))
            .filter_map(|a| a.agent_id().map(str::to_string))
            .collect();
        assert_eq!(
            machine_running, expected_running,
            "状态机 Running 集合必须与独立时序台账双向相等"
        );
        let expected_wf_running = ledger.running_workflows.clone();
        let machine_wf_running: std::collections::BTreeSet<String> = st
            .workflows
            .iter()
            .filter(|w| !w.is_finished() && !w.task_id.is_empty())
            .map(|w| w.task_id.clone())
            .collect();
        assert_eq!(
            machine_wf_running, expected_wf_running,
            "状态机未完 workflow(taskId)必须与独立配对双向相等"
        );

        // ② 权威计数:用独立集合证成排队语义分类——「权威 ≥ 独立配对」(通知已入队未消费时
        // 计数不减),且状态机计数 == 独立配对。两条合起来独立证明 count_drift 只报
        // 「状态机 > 权威」方向是正确分类,而非拿生产函数自说自话。
        if let Some(bg) = ledger.last_pending_bg {
            assert!(
                expected_running.len() as u64 <= bg,
                "独立配对({})不得超过权威计数({bg})——超过说明排队语义假设不成立",
                expected_running.len()
            );
        }
        if let Some(wf) = ledger.last_pending_wf {
            assert!(expected_wf_running.len() as u64 <= wf);
        }
        assert!(
            st.count_drift().is_none(),
            "对账漂移: {:?}",
            st.count_drift()
        );

        // ③ 展示层窗口校验覆盖**全部状态**(不只 active):历史 Completed/Failed 误入也要抓。
        for a in &display.agents {
            match a {
                SubagentStatus::Completed { finished_at, .. } => assert!(
                    now - *finished_at < summon_core::subagent::COMPLETED_DISPLAY_TTL,
                    "历史 Completed 误入展示层: {} finished_at={finished_at}",
                    a.description()
                ),
                SubagentStatus::Failed { finished_at, .. } => assert!(
                    now - *finished_at < summon_core::subagent::FAILED_DISPLAY_TTL,
                    "历史 Failed 误入展示层: {} finished_at={finished_at}",
                    a.description()
                ),
                SubagentStatus::Running { .. } | SubagentStatus::Spawning { .. } => assert!(
                    now - a.activity_anchor() < summon_core::subagent::DIR_SUBAGENT_STALE_AFTER,
                    "展示层出现超窗活跃条目: {} anchor={}",
                    a.description(),
                    a.activity_anchor()
                ),
            }
        }
        for g in &display.workflows {
            let run = st
                .workflows
                .iter()
                .find(|w| w.run_id == g.run_id)
                .expect("展示组必须能映射回 run");
            match &run.outcome {
                Some(o) if o.failed => assert!(
                    now - o.finished_at < summon_core::subagent::FAILED_DISPLAY_TTL,
                    "历史失败 workflow 误入展示层: {}",
                    g.name
                ),
                Some(o) => assert!(
                    now - o.finished_at < summon_core::subagent::COMPLETED_DISPLAY_TTL,
                    "历史完成 workflow 误入展示层: {}",
                    g.name
                ),
                None => {
                    // 活跃锚在测试内独立重算:journal mtime / 活跃 child 转录 mtime / started_at 取最大。
                    let anchor = run
                        .children
                        .iter()
                        .filter(|c| !c.is_finished())
                        .filter_map(|c| match c {
                            SubagentStatus::Running {
                                started_at,
                                last_activity,
                                ..
                            } => Some(last_activity.unwrap_or(*started_at)),
                            SubagentStatus::Spawning { detected_at, .. } => Some(*detected_at),
                            _ => None,
                        })
                        .chain(run.last_activity)
                        .chain(std::iter::once(run.started_at))
                        .max()
                        .expect("started_at 恒存在");
                    assert!(
                        now - anchor < summon_core::subagent::DIR_SUBAGENT_STALE_AFTER,
                        "陈旧 workflow 误入展示层: {} anchor={anchor}",
                        g.name
                    );
                }
            }
        }
    }

    /// 独立台账:**不经生产 detect_*/状态机**的最小转录解析(raw serde_json 手扒字段)。
    /// 只服务真机对账测试——生产解析错了它不会跟着错。
    struct IndependentLedger {
        launched_agents: std::collections::BTreeSet<String>,
        /// workflow 的 taskId(完成通知的 task-id 用的就是它)。
        launched_workflows: std::collections::BTreeSet<String>,
        notified: std::collections::BTreeSet<String>,
        /// **时序台账**:按行序维护的「此刻在跑」集合——launched/resumed/queued 置入、
        /// notification 移除。resume(含 Form C queued)必须参与:launched−notified 的
        /// 静态差集看不见被 SendMessage 续起的 agent(真机 owner 截图事故)。
        running_agents: std::collections::BTreeSet<String>,
        running_workflows: std::collections::BTreeSet<String>,
        last_pending_bg: Option<u64>,
        last_pending_wf: Option<u64>,
    }

    fn independent_ledger(content: &str) -> IndependentLedger {
        let mut ledger = IndependentLedger {
            launched_agents: Default::default(),
            launched_workflows: Default::default(),
            notified: Default::default(),
            running_agents: Default::default(),
            running_workflows: Default::default(),
            last_pending_bg: None,
            last_pending_wf: None,
        };
        for line in content.lines().filter(|l| !l.is_empty()) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(tur) = v.get("toolUseResult").filter(|t| t.is_object()) {
                if tur.get("status").and_then(|s| s.as_str()) == Some("async_launched") {
                    if tur.get("taskType").and_then(|t| t.as_str()) == Some("local_workflow") {
                        if let Some(tid) = tur.get("taskId").and_then(|t| t.as_str()) {
                            ledger.launched_workflows.insert(tid.to_string());
                            ledger.running_workflows.insert(tid.to_string());
                        }
                    } else if tur.get("isAsync").and_then(|b| b.as_bool()) == Some(true) {
                        if let Some(aid) = tur.get("agentId").and_then(|a| a.as_str()) {
                            ledger.launched_agents.insert(aid.to_string());
                            ledger.running_agents.insert(aid.to_string());
                        }
                    }
                }
                // 续用:Form A/B 带 resumedAgentId;Form C(queued)只有 success+pin.id、无 status。
                // 只认曾 launched 的 id(teammate/后台任务的 pin 不入台账)。
                let resumed = tur
                    .get("resumedAgentId")
                    .and_then(|r| r.as_str())
                    .or_else(|| {
                        (tur.get("success").and_then(|b| b.as_bool()) == Some(true)
                            && tur.get("status").is_none())
                        .then(|| tur.get("pin")?.get("id")?.as_str())
                        .flatten()
                    });
                if let Some(rid) = resumed {
                    if ledger.launched_agents.contains(rid) {
                        ledger.running_agents.insert(rid.to_string());
                    }
                }
            }
            // 完成通知两种形态:queue-operation 的 content / user 行 message.content。
            let notif_text = match v.get("type").and_then(|t| t.as_str()) {
                Some("queue-operation") => v.get("content").and_then(|c| c.as_str()),
                Some("user") => v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str()),
                _ => None,
            };
            if let Some(text) = notif_text {
                if text.contains("<task-notification>") {
                    if let Some(start) = text.find("<task-id>") {
                        let rest = &text[start + "<task-id>".len()..];
                        if let Some(end) = rest.find("</task-id>") {
                            let tid = rest[..end].trim().to_string();
                            ledger.running_agents.remove(&tid);
                            ledger.running_workflows.remove(&tid);
                            ledger.notified.insert(tid);
                        }
                    }
                }
            }
            if v.get("type").and_then(|t| t.as_str()) == Some("system")
                && v.get("subtype").and_then(|s| s.as_str()) == Some("turn_duration")
            {
                ledger.last_pending_bg = v
                    .get("pendingBackgroundAgentCount")
                    .and_then(|n| n.as_u64());
                ledger.last_pending_wf = v.get("pendingWorkflowCount").and_then(|n| n.as_u64());
            }
        }
        ledger
    }

    /// 真机形状的全量重放对账(fixtures 化):对 2026-08-14 真机采样的**同构**事件序列重放,
    /// 状态机的「在跑」集合必须与转录配对(launched − notified)一致,历史完成/失败不在展示层。
    ///
    /// 事件序列还原真机采样(demo-project 会话 186e4d8a,2026-08-13T23:22Z 时刻):
    /// - 25 async_launched、其中 24 已有 completed 通知(时间散布在 10~12h 前)→ 不展示;
    /// - afdcd(通知刚入队)+ a42b(在跑)是权威 pendingBackgroundAgentCount=2 的两员;
    /// - workflow wco3l6qra 已完成(4h 前)→ 不展示;
    /// - 后台 shell 任务通知(bs5 开头)不得建条目。
    #[test]
    fn replay_real_machine_shaped_transcript_matches_pairing() {
        let now = chrono::Utc::now();
        let iso = |mins_ago: i64| {
            (now - chrono::Duration::minutes(mins_ago))
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
        };
        let mut lines: Vec<String> = Vec::new();
        // 24 个历史子代理:spawn → launch → completed 通知,全在 10h 前后
        for i in 0..24 {
            let t_spawn = iso(700 - i);
            let t_done = iso(680 - i);
            lines.push(serde_json::json!({
                "type":"assistant","timestamp": t_spawn,
                "message":{"content":[{"type":"tool_use","id":format!("tu_{i}"),"name":"Agent",
                    "input":{"description":format!("历史任务 {i}"),"subagent_type":"general-purpose","run_in_background":true}}]}
            }).to_string());
            lines.push(serde_json::json!({
                "type":"user","timestamp": t_spawn,
                "toolUseResult":{"isAsync":true,"status":"async_launched","agentId":format!("hist{i}"),
                    "description":format!("历史任务 {i}"),"resolvedModel":"claude-fable-5"},
                "message":{"content":[{"type":"tool_result","tool_use_id":format!("tu_{i}")}]}
            }).to_string());
            lines.push(serde_json::json!({
                "type":"queue-operation","operation":"enqueue","timestamp": t_done,
                "content": format!("<task-notification>\n<task-id>hist{i}</task-id>\n<status>completed</status>\n<summary>Agent finished</summary>\n</task-notification>")
            }).to_string());
        }
        // workflow:4h 前启动、3.5h 前完成
        lines.push(serde_json::json!({
            "type":"user","timestamp": iso(240),
            "toolUseResult":{"status":"async_launched","taskType":"local_workflow","taskId":"wco3l6qra",
                "runId":"wf_8448782b-2ee","workflowName":"slash-passthrough-impl"},
            "message":{"content":[{"type":"tool_result","tool_use_id":"tu_wf"}]}
        }).to_string());
        lines.push(serde_json::json!({
            "type":"queue-operation","operation":"enqueue","timestamp": iso(210),
            "content":"<task-notification>\n<task-id>wco3l6qra</task-id>\n<status>completed</status>\n<summary>Dynamic workflow completed</summary>\n</task-notification>"
        }).to_string());
        // afdcd:18min 前启动 → 15min 前完成 → 12min 前被 SendMessage 续起(Form A,改名)
        // ——真机 2026-08-14 00:22 事故形态:TUI 在跑、旧逻辑卡片缺失。
        lines.push(serde_json::json!({
            "type":"user","timestamp": iso(18),
            "toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"afdcd579162d517eb",
                "description":"开放透传命令有参形态","resolvedModel":"claude-fable-5"},
            "message":{"content":[{"type":"tool_result","tool_use_id":"tu_live1"}]}
        }).to_string());
        lines.push(serde_json::json!({
            "type":"queue-operation","operation":"enqueue","timestamp": iso(15),
            "content":"<task-notification>\n<task-id>afdcd579162d517eb</task-id>\n<status>completed</status>\n<summary>Agent finished</summary>\n</task-notification>"
        }).to_string());
        lines.push(
            serde_json::json!({
                "type":"assistant","timestamp": iso(12),
                "message":{"content":[{"type":"tool_use","id":"tu_sm1","name":"SendMessage",
                    "input":{"to":"afdcd579162d517eb","recipient":"afdcd579162d517eb",
                        "summary":"前台等验证跑完再交付","message":"继续:……","type":"message"}}]}
            })
            .to_string(),
        );
        lines.push(serde_json::json!({
            "type":"user","timestamp": iso(12),
            "toolUseResult":{"success":true,
                "message":"Agent \"afdcd579162d517eb\" was stopped (completed); resumed it in the background with your message.",
                "resumedAgentId":"afdcd579162d517eb",
                "pin":{"id":"afdcd579162d517eb","name":"afdcd579162d517eb","ref":"8d20a5"}}
        }).to_string());
        // a42b:4min 前启动在跑;2min 前一条 queued(Form C)追加消息 + 改名 —— 在跑不改状态,名字取最新。
        lines.push(serde_json::json!({
            "type":"user","timestamp": iso(4),
            "toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a42b155eb070664ea",
                "description":"实施 subagent 可见性 P1P2","resolvedModel":"claude-fable-5"},
            "message":{"content":[{"type":"tool_result","tool_use_id":"tu_live2"}]}
        }).to_string());
        lines.push(serde_json::json!({
            "type":"assistant","timestamp": iso(2),
            "message":{"content":[{"type":"tool_use","id":"tu_sm2","name":"SendMessage",
                "input":{"to":"a42b155eb070664ea","summary":"追加 resume 形态支持与真机样本","message":"……"}}]}
        }).to_string());
        lines.push(serde_json::json!({
            "type":"user","timestamp": iso(2),
            "toolUseResult":{"success":true,
                "message":"Message queued for delivery to a42b155eb070664ea at its next tool round.",
                "pin":{"id":"a42b155eb070664ea","name":"a42b155eb070664ea","ref":"8ddfe3"}}
        }).to_string());
        // 后台 shell 任务失败通知:不得建条目
        lines.push(serde_json::json!({
            "type":"user","origin":{"kind":"task-notification"},"timestamp": iso(3),
            "message":{"content":"<task-notification>\n<task-id>bs5oknztc</task-id>\n<status>failed</status>\n<summary>Background command failed</summary>\n</task-notification>"}
        }).to_string());
        // 轮末权威计数
        lines.push(
            serde_json::json!({
                "type":"system","subtype":"turn_duration","timestamp": iso(0),
                "durationMs":15652,"pendingBackgroundAgentCount":2
            })
            .to_string(),
        );

        let mut st = SubagentState::new();
        for l in &lines {
            consume_transcript_line(&mut st, l);
        }

        // 时序台账基准:24 hist 完成 + afdcd(完成后被续起)+ a42b(在跑)= 2 在跑
        let running: Vec<&str> = st
            .agents
            .iter()
            .filter(|a| matches!(a, SubagentStatus::Running { .. }))
            .filter_map(|a| a.agent_id())
            .collect();
        assert_eq!(
            running,
            vec!["afdcd579162d517eb", "a42b155eb070664ea"],
            "在跑集合必须与转录时序台账一致(含续起的)"
        );
        // 与权威计数一致 → 无漂移
        assert!(st.count_drift().is_none(), "{:?}", st.count_drift());
        // 展示层:恰好 2 条(历史 24 条与已完成 workflow 均不可见),model 已采到,名字取最新
        let display = st.display(now);
        assert_eq!(display.agents.len(), 2, "{display:?}");
        assert!(display
            .agents
            .iter()
            .all(|a| a.model() == Some("claude-fable-5")));
        let descs: Vec<&str> = display.agents.iter().map(|a| a.description()).collect();
        assert_eq!(
            descs,
            vec!["前台等验证跑完再交付", "追加 resume 形态支持与真机样本"],
            "卡片名字必须取 SendMessage 的最新 summary"
        );
        assert!(
            display.workflows.is_empty(),
            "3.5h 前完成的 workflow 不展示"
        );
        // 后台 shell 任务没被当成子代理
        assert!(st.agents.iter().all(|a| a.agent_id() != Some("bs5oknztc")));
    }
}
