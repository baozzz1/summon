//! 主 Agent 让出本轮后，仍在跑的后台任务归谁。
//!
//! claude 主 Agent 结束输出、只剩后台任务在跑时，调度器让出本轮去处理排队消息。让出的只是
//! 调度：后台任务仍代表原轮发起人在跑，它的工具调用要按原发起人判权，会话也不能因为「没有
//! 在跑的轮」被空闲回收拆掉。所以让出前先落一条持有者记录，落不成就不让出。
//!
//! 与 current-turn 不同，这份记录跨 daemon 重启保留：重启不打断 tmux 里的后台任务，清掉记录
//! 就等于把它们交给下一轮的发起人。任务全部收到完成通知、会话换代或被拆、超过续跑上限时清理。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::capture::{BackgroundIdle, HeldBackgroundTask, HeldTaskKind};
use crate::observe::log::log;
use crate::runtime::current_turn_store::{CurrentTurnContext, CurrentTurnOrigin, TurnActor};
use crate::runtime::fsio::atomic_write;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackgroundHolder {
    pub(crate) turn_id: String,
    pub(crate) message_id: String,
    pub(crate) chat_id: String,
    pub(crate) chat_type: String,
    pub(crate) origin: CurrentTurnOrigin,
    pub(crate) requester_open_id: String,
    pub(crate) requester_union_id: String,
    pub(crate) requester_is_peer: bool,
    pub(crate) tmux_session: String,
    pub(crate) session_id: String,
    pub(crate) transcript: PathBuf,
    pub(crate) tasks: Vec<HeldBackgroundTask>,
    pub(crate) created_at_ms: i64,
    pub(crate) expires_at_ms: i64,
}

impl BackgroundHolder {
    pub(crate) fn from_turn(
        context: &CurrentTurnContext,
        tmux_session: &str,
        idle: &BackgroundIdle,
        now_ms: i64,
        cap: Duration,
    ) -> Self {
        Self {
            turn_id: context.turn_id.clone(),
            message_id: context.message_id.clone(),
            chat_id: context.chat_id.clone(),
            chat_type: context.chat_type.clone(),
            origin: context.origin,
            requester_open_id: context.requester_open_id.clone(),
            requester_union_id: context.requester_union_id.clone(),
            requester_is_peer: context.requester_is_peer,
            tmux_session: tmux_session.to_string(),
            session_id: idle.session_id.to_string(),
            transcript: idle.transcript.to_path_buf(),
            tasks: idle.tasks.clone(),
            created_at_ms: now_ms,
            expires_at_ms: now_ms
                .saturating_add(i64::try_from(cap.as_millis()).unwrap_or(i64::MAX)),
        }
    }

    pub(crate) fn actor(&self) -> TurnActor {
        TurnActor {
            open_id: self.requester_open_id.clone(),
            union_id: self.requester_union_id.clone(),
            is_peer: self.requester_is_peer,
            chat_type: self.chat_type.clone(),
            origin: self.origin,
        }
    }

    pub(crate) fn holds_agent(&self, agent_id: &str) -> bool {
        self.tasks
            .iter()
            .any(|task| task.kind == HeldTaskKind::Agent && task.task_id == agent_id)
    }

    pub(crate) fn holds_workflow(&self) -> bool {
        self.tasks
            .iter()
            .any(|task| task.kind == HeldTaskKind::Workflow)
    }

    fn expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

/// 记录是否仍代表在跑的后台工作：没超过续跑上限、tmux 会话还在、原生会话还是让轮时那一个、
/// 转录里它起的任务还没跑完或主 Agent 还没对完成通知收尾。转录读不出时按在跑算，提前清掉
/// 会把后台工作交给下一轮的发起人。
pub(crate) fn still_running(
    holder: &BackgroundHolder,
    current_session_id: Option<&str>,
    tmux_alive: bool,
    now_ms: i64,
) -> bool {
    if holder.expired(now_ms)
        || !tmux_alive
        || current_session_id != Some(holder.session_id.as_str())
    {
        return false;
    }
    let task_ids: Vec<&str> = holder
        .tasks
        .iter()
        .map(|task| task.task_id.as_str())
        .collect();
    match crate::capture::background_work_outstanding(&holder.transcript, &task_ids) {
        Ok(outstanding) => outstanding,
        Err(error) => {
            log(&summon_i18n::format!(
                "r476c386c52",
                holder.turn_id,
                holder.transcript.display()
            ));
            true
        }
    }
}

#[derive(Clone)]
pub(crate) struct BackgroundHolderStore {
    root: PathBuf,
}

impl BackgroundHolderStore {
    #[cfg(test)]
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub(crate) fn for_agent(state_dir: &Path) -> Self {
        Self {
            root: state_dir.join("runtime").join("background-holders"),
        }
    }

    fn path(&self, conversation_tag: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", safe_file_name(conversation_tag)))
    }

    pub(crate) fn add(&self, conversation_tag: &str, holder: BackgroundHolder) -> Result<()> {
        let _guard = write_lock();
        std::fs::create_dir_all(&self.root)
            .with_context(|| summon_i18n::format!("raf1a89e1a0", self.root))?;
        let path = self.path(conversation_tag);
        let mut holders = read_file(&path);
        holders.push(holder);
        write_file(&path, &holders)
    }

    /// 只看在不在，与 current-turn 的 `has_turn_record` 同一口径：给「能不能拆这条会话」当闸。
    pub(crate) fn has_any(&self, conversation_tag: &str) -> bool {
        self.path(conversation_tag).exists()
    }

    pub(crate) fn clear(&self, conversation_tag: &str) {
        let _guard = write_lock();
        // 会话已拆，后台任务随之结束；删除失败的残留由下一次 prune 按 tmux 不在清掉。
        let _ = std::fs::remove_file(self.path(conversation_tag));
    }

    /// 本 chat 未过期的持有者。PreToolUse hook 每次工具调用读一遍，只读不写。
    pub(crate) fn for_chat(&self, chat_id: &str, now_ms: i64) -> Vec<BackgroundHolder> {
        if chat_id.is_empty() {
            return Vec::new();
        }
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|v| v.to_str()) == Some("json"))
            .flat_map(|entry| read_file(&entry.path()))
            .filter(|holder| holder.chat_id == chat_id && !holder.expired(now_ms))
            .collect()
    }

    pub(crate) fn prune(
        &self,
        conversation_tag: &str,
        keep: impl FnMut(&BackgroundHolder) -> bool,
    ) -> Result<()> {
        let _guard = write_lock();
        prune_file(&self.path(conversation_tag), keep)
    }

    /// daemon 启动时整理上一代留下的全部记录。
    pub(crate) fn prune_all(&self, mut keep: impl FnMut(&BackgroundHolder) -> bool) -> Result<()> {
        let _guard = write_lock();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| summon_i18n::format!("ra8dd7dfb83", self.root))
            }
        };
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|v| v.to_str()) == Some("json") {
                prune_file(&entry.path(), &mut keep)?;
            }
        }
        Ok(())
    }
}

/// 本 chat 还有让轮后在跑的后台工作。给「能不能拆这条会话」当闸用。
pub(crate) fn background_running(agent: &summon_core::registry::Agent, chat_id: &str) -> bool {
    !BackgroundHolderStore::for_agent(&agent.state_dir())
        .for_chat(chat_id, now_ms())
        .is_empty()
}

/// 按会话现状清理本 chat 的持有者。
pub(crate) fn prune_for_session(
    agent: &summon_core::registry::Agent,
    conversation_tag: &str,
    tmux_session: &str,
) -> Result<()> {
    let store = BackgroundHolderStore::for_agent(&agent.state_dir());
    if !store.has_any(conversation_tag) {
        return Ok(());
    }
    let tmux_alive = crate::session::tmux::Tmux::new(tmux_session.to_string()).has_session();
    let session_id =
        crate::session::resume_store::read_id(agent, tmux_session, &agent.resume_cli_kind());
    let now_ms = now_ms();
    store.prune(conversation_tag, |holder| {
        still_running(holder, session_id.as_deref(), tmux_alive, now_ms)
    })
}

/// daemon 启动时按各记录自己的 tmux 会话整理。
pub(crate) fn prune_on_boot(agent: &summon_core::registry::Agent) -> Result<()> {
    let now_ms = now_ms();
    BackgroundHolderStore::for_agent(&agent.state_dir()).prune_all(|holder| {
        let tmux_alive = crate::session::tmux::Tmux::new(holder.tmux_session.clone()).has_session();
        let session_id = crate::session::resume_store::read_id(
            agent,
            &holder.tmux_session,
            &agent.resume_cli_kind(),
        );
        still_running(holder, session_id.as_deref(), tmux_alive, now_ms)
    })
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn prune_file(path: &Path, mut keep: impl FnMut(&BackgroundHolder) -> bool) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let holders = read_file(path);
    let before = holders.len();
    let kept: Vec<_> = holders.into_iter().filter(|holder| keep(holder)).collect();
    if kept.len() == before && before > 0 {
        return Ok(());
    }
    write_file(path, &kept)
}

fn read_file(path: &Path) -> Vec<BackgroundHolder> {
    let Ok(raw) = std::fs::read(path) else {
        return Vec::new();
    };
    serde_json::from_slice(&raw).unwrap_or_else(|error| {
        log(&summon_i18n::format!("r800255f2b7", path.display()));
        Vec::new()
    })
}

/// 空列表即删文件：「文件在」就是「有后台任务在跑」。
fn write_file(path: &Path, holders: &[BackgroundHolder]) -> Result<()> {
    if holders.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| summon_i18n::format!("r8f779dfd73")),
        };
    }
    let json = serde_json::to_vec(holders).context(summon_i18n::text!("re2907e8055"))?;
    atomic_write(path, &json).with_context(|| summon_i18n::format!("r41730efd48"))
}

/// daemon 内 chat 线程(落记录、拆会话)与 dispatcher(定期清理)并发写同一文件。
fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn safe_file_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;
    const AGENT_LAUNCHED: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_agent","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully.\nagentId: a1c7b6848c7434fad (internal ID - do not mention to user.)"}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a1c7b6848c7434fad","description":"d","prompt":"p","outputFile":"/tmp/a.output","canReadOutputFile":false}}"#;
    const SHELL_STARTED: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_shell","type":"tool_result","content":"Command running in background with ID: bfyy2gdqz.","is_error":false}]},"toolUseResult":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"bfyy2gdqz"}}"#;

    const MAIN_END_TURN: &str = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#;

    fn notice_text(task_id: &str, tool_use_id: &str) -> String {
        format!(
            "<task-notification>\n<task-id>{task_id}</task-id>\n<tool-use-id>{tool_use_id}</tool-use-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>"
        )
    }

    /// 通知入队。
    fn notification(task_id: &str, tool_use_id: &str) -> String {
        serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": notice_text(task_id, tool_use_id),
        })
        .to_string()
    }

    /// 主 Agent 取出并开始处理这条通知。
    fn consumed(task_id: &str, tool_use_id: &str) -> String {
        serde_json::json!({
            "type": "user",
            "isSidechain": false,
            "message": {"role": "user", "content": notice_text(task_id, tool_use_id)},
        })
        .to_string()
    }

    fn holder(chat: &str, transcript: &Path, tasks: Vec<HeldBackgroundTask>) -> BackgroundHolder {
        BackgroundHolder {
            turn_id: format!("turn-{chat}"),
            message_id: format!("om-{chat}"),
            chat_id: chat.into(),
            chat_type: "group".into(),
            origin: CurrentTurnOrigin::Feishu,
            requester_open_id: "ou_requester".into(),
            requester_union_id: "on_requester".into(),
            requester_is_peer: false,
            tmux_session: "soul-test".into(),
            session_id: "sid-1".into(),
            transcript: transcript.to_path_buf(),
            tasks,
            created_at_ms: NOW,
            expires_at_ms: NOW + 60_000,
        }
    }

    fn agent_task(id: &str) -> HeldBackgroundTask {
        HeldBackgroundTask {
            task_id: id.into(),
            kind: HeldTaskKind::Agent,
        }
    }

    fn shell_task(id: &str) -> HeldBackgroundTask {
        HeldBackgroundTask {
            task_id: id.into(),
            kind: HeldTaskKind::Shell,
        }
    }

    #[test]
    fn for_chat_returns_unexpired_holders_of_that_chat_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundHolderStore::new(dir.path().join("holders"));
        let transcript = dir.path().join("t.jsonl");
        store
            .add("g_a", holder("oc_a", &transcript, vec![agent_task("a1")]))
            .unwrap();
        store
            .add("g_a", holder("oc_a", &transcript, vec![shell_task("b1")]))
            .unwrap();
        store
            .add("g_b", holder("oc_b", &transcript, vec![agent_task("a2")]))
            .unwrap();

        let in_a = store.for_chat("oc_a", NOW);
        assert_eq!(in_a.len(), 2);
        assert!(in_a[0].holds_agent("a1"));
        assert!(
            !in_a[1].holds_agent("b1"),
            "后台命令没有工具调用，不按 agent_id 认领"
        );
        assert!(
            store.for_chat("oc_a", NOW + 60_000).is_empty(),
            "过期即失效"
        );
        assert!(store.for_chat("", NOW).is_empty());
        assert!(store.has_any("g_a") && store.has_any("g_b"));
    }

    #[test]
    fn still_running_needs_live_session_same_sid_and_an_unsettled_task() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, format!("{AGENT_LAUNCHED}\n{SHELL_STARTED}\n")).unwrap();
        let held = holder(
            "oc_a",
            &transcript,
            vec![agent_task("a1c7b6848c7434fad"), shell_task("bfyy2gdqz")],
        );
        assert!(still_running(&held, Some("sid-1"), true, NOW));
        assert!(
            !still_running(&held, Some("sid-1"), false, NOW),
            "tmux 不在"
        );
        assert!(!still_running(&held, Some("sid-2"), true, NOW), "会话换代");
        assert!(!still_running(&held, None, true, NOW), "没有会话记录");
        assert!(
            !still_running(&held, Some("sid-1"), true, NOW + 60_000),
            "超上限"
        );

        // 子 agent 先完成，后台命令还在跑。
        let mut appended = std::fs::read_to_string(&transcript).unwrap();
        appended.push_str(&notification("a1c7b6848c7434fad", "tu_agent"));
        appended.push('\n');
        appended.push_str(&consumed("a1c7b6848c7434fad", "tu_agent"));
        appended.push('\n');
        std::fs::write(&transcript, &appended).unwrap();
        assert!(still_running(&held, Some("sid-1"), true, NOW));

        appended.push_str(&notification("bfyy2gdqz", "tu_shell"));
        appended.push('\n');
        appended.push_str(&consumed("bfyy2gdqz", "tu_shell"));
        appended.push('\n');
        std::fs::write(&transcript, &appended).unwrap();
        assert!(
            still_running(&held, Some("sid-1"), true, NOW),
            "主 Agent 还在处理完成通知"
        );
        appended.push_str(MAIN_END_TURN);
        appended.push('\n');
        std::fs::write(&transcript, &appended).unwrap();
        assert!(!still_running(&held, Some("sid-1"), true, NOW));

        let unreadable = holder(
            "oc_a",
            &dir.path().join("missing.jsonl"),
            vec![shell_task("x")],
        );
        assert!(
            still_running(&unreadable, Some("sid-1"), true, NOW),
            "转录读不出时保留"
        );
    }

    #[test]
    fn prune_drops_finished_holders_and_removes_the_empty_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundHolderStore::new(dir.path().join("holders"));
        let transcript = dir.path().join("t.jsonl");
        store
            .add("g_a", holder("oc_a", &transcript, vec![agent_task("keep")]))
            .unwrap();
        store
            .add("g_a", holder("oc_a", &transcript, vec![agent_task("drop")]))
            .unwrap();
        store.prune("g_a", |h| h.holds_agent("keep")).unwrap();
        let left = store.for_chat("oc_a", NOW);
        assert_eq!(left.len(), 1);
        assert!(left[0].holds_agent("keep"));

        store.prune("g_a", |_| false).unwrap();
        assert!(
            !store.has_any("g_a"),
            "清空即删记录：文件在就是有后台任务在跑"
        );
        store.prune("g_missing", |_| true).unwrap();
    }

    #[test]
    fn prune_all_walks_every_record_and_clear_removes_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundHolderStore::new(dir.path().join("holders"));
        store.prune_all(|_| true).unwrap();
        let transcript = dir.path().join("t.jsonl");
        store
            .add("g_a", holder("oc_a", &transcript, vec![agent_task("a1")]))
            .unwrap();
        store
            .add("g_b", holder("oc_b", &transcript, vec![agent_task("a2")]))
            .unwrap();
        store.prune_all(|h| h.chat_id == "oc_b").unwrap();
        assert!(!store.has_any("g_a"));
        assert!(store.has_any("g_b"));
        store.clear("g_b");
        assert!(!store.has_any("g_b"));
    }

    #[test]
    fn prune_for_session_drops_holders_whose_tmux_session_is_gone() {
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let store = BackgroundHolderStore::for_agent(&agent.state_dir());
        let mut held = holder("oc_a", Path::new("/nonexistent"), vec![agent_task("a1")]);
        held.expires_at_ms = i64::MAX;
        store.add("g_a", held).unwrap();
        prune_for_session(&agent, "g_a", "soul-background-holder-no-such-session").unwrap();
        assert!(!store.has_any("g_a"));
    }

    #[test]
    fn prune_on_boot_uses_each_holders_own_session() {
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let store = BackgroundHolderStore::for_agent(&agent.state_dir());
        let mut held = holder("oc_a", Path::new("/nonexistent"), vec![agent_task("a1")]);
        held.tmux_session = "soul-background-holder-boot-gone".into();
        held.expires_at_ms = i64::MAX;
        store.add("g_a", held).unwrap();
        prune_on_boot(&agent).unwrap();
        assert!(!store.has_any("g_a"));
    }

    #[test]
    fn workflow_tasks_are_not_matched_by_agent_id() {
        let held = holder(
            "oc_a",
            Path::new("/t"),
            vec![HeldBackgroundTask {
                task_id: "w1".into(),
                kind: HeldTaskKind::Workflow,
            }],
        );
        assert!(!held.holds_agent("w1"));
        assert!(held.holds_workflow());
    }
}
