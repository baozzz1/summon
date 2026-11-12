//! 遥控绑定的落盘记录与"这条绑定还指着同一个进程吗"的复验。
//! 每个群一份 `<root>/<chat_id>.json`,配套已转发进度 `<root>/<chat_id>.offset`。

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use summon_core::registry::Agent;
use summon_core::remote::{
    binding_matches, surface_holds_other_session, CmuxHookSessions, RemoteBinding,
};

use crate::runtime::fsio::atomic_write_mode;

pub(crate) struct RemoteStore {
    root: PathBuf,
}

/// 写盘串行:dispatcher(绑定、解除)、worker(结束)、回传线程(offset)、启动恢复都在同一个
/// 进程里写同一批文件。「盘上还是这份绑定才写」的比对与写入要在同一把锁里做完,否则比对之后、
/// 写入之前被解除或换绑,写下去的结束记录会压掉新绑定。
static WRITES: Mutex<()> = Mutex::new(());

fn write_lock() -> MutexGuard<'static, ()> {
    WRITES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 同一份绑定:创建时刻与会话号都相同。换绑后的记录至少有一项不同。
pub(crate) fn same_binding(a: &RemoteBinding, b: &RemoteBinding) -> bool {
    a.created_at_ms == b.created_at_ms && a.session_id == b.session_id
}

/// chat_id 直接拼进文件名,只放行 `[A-Za-z0-9_-]`,挡住 `../` 之类的路径穿越。
fn checked(chat_id: &str) -> Result<&str> {
    if chat_id.is_empty()
        || !chat_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!(summon_i18n::format!("rc54bce3382"));
    }
    Ok(chat_id)
}

impl RemoteStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn for_agent(agent: &Agent) -> Self {
        Self::new(agent.state_dir().join("remote"))
    }

    fn record_path(&self, chat_id: &str) -> Result<PathBuf> {
        Ok(self.root.join(format!("{}.json", checked(chat_id)?)))
    }

    fn offset_path(&self, chat_id: &str) -> Result<PathBuf> {
        Ok(self.root.join(format!("{}.offset", checked(chat_id)?)))
    }

    /// 记录损坏按错误返回而不是"没绑定":当成没绑定会让群消息落进本地会话。
    pub(crate) fn load(&self, chat_id: &str) -> Result<Option<RemoteBinding>> {
        let path = self.record_path(chat_id)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .with_context(|| summon_i18n::format!("rc28a09cd37", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| summon_i18n::format!("r8b5930c0be", path.display())),
        }
    }

    pub(crate) fn list(&self) -> Result<Vec<RemoteBinding>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e)
                    .with_context(|| summon_i18n::format!("rb527a60143", self.root.display()))
            }
        };
        let mut out = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(chat_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(binding) = self.load(chat_id)? {
                out.push(binding);
            }
        }
        out.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
        Ok(out)
    }

    pub(crate) fn find_by_session(&self, sid: &str) -> Result<Option<RemoteBinding>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|b| b.session_id == sid && b.ended.is_none()))
    }

    pub(crate) fn save(&self, binding: &RemoteBinding) -> Result<()> {
        let _writes = write_lock();
        self.save_locked(binding)
    }

    fn save_locked(&self, binding: &RemoteBinding) -> Result<()> {
        let json = serde_json::to_vec_pretty(binding)?;
        atomic_write_mode(&self.record_path(&binding.chat_id)?, &json, 0o600)
    }

    pub(crate) fn remove(&self, chat_id: &str) -> Result<()> {
        let _writes = write_lock();
        self.remove_locked(chat_id)
    }

    /// 盘上仍是这份绑定时把它标成已结束,返回是否写了。已解除或换绑的不动。
    pub(crate) fn mark_ended_if_current(
        &self,
        binding: &RemoteBinding,
        reason: &str,
    ) -> Result<bool> {
        let _writes = write_lock();
        match self.load(&binding.chat_id)? {
            Some(current) if same_binding(&current, binding) => {
                self.save_locked(&RemoteBinding {
                    ended: Some(reason.to_string()),
                    ..current
                })?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// 盘上仍是这份绑定时删掉它与它的 offset,返回是否删了。已换绑的新记录不动。
    pub(crate) fn remove_if_current(&self, binding: &RemoteBinding) -> Result<bool> {
        let _writes = write_lock();
        match self.load(&binding.chat_id)? {
            Some(current) if same_binding(&current, binding) => {
                self.remove_locked(&binding.chat_id)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn remove_locked(&self, chat_id: &str) -> Result<()> {
        for path in [self.record_path(chat_id)?, self.offset_path(chat_id)?] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| summon_i18n::format!("rbe2d634e79", path.display()))
                }
            }
        }
        Ok(())
    }

    pub(crate) fn read_offset(&self, chat_id: &str) -> Result<Option<u64>> {
        let path = self.offset_path(chat_id)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => text
                .trim()
                .parse()
                .map(Some)
                .with_context(|| summon_i18n::format!("r7a19386521", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| summon_i18n::format!("r6da9ed443d", path.display())),
        }
    }

    pub(crate) fn write_offset(&self, chat_id: &str, offset: u64) -> Result<()> {
        let _writes = write_lock();
        atomic_write_mode(
            &self.offset_path(chat_id)?,
            offset.to_string().as_bytes(),
            0o600,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    Alive,
    Ended(&'static str),
}

/// cmux 记录与实际进程两头都要对得上:记录里 pid/起始时间/窗口/会话号一致,窗口此刻挂着的
/// 仍是这条会话,且这个 pid 此刻真在跑、起始时间相同(防 pid 复用)。
pub(crate) fn check_liveness(
    binding: &RemoteBinding,
    hooks: &CmuxHookSessions,
    process_start: &dyn Fn(u32) -> Option<i64>,
) -> Liveness {
    let Some(record) = hooks
        .sessions
        .iter()
        .find(|s| s.session_id == binding.session_id)
    else {
        return Liveness::Ended(summon_i18n::text!("r708274c935"));
    };
    if !binding_matches(binding, record) {
        return Liveness::Ended(summon_i18n::text!("r4b4e060db4"));
    }
    // 同一个进程里换了会话(`/clear` 之类)时旧记录与 pid 都不变,字会打进新会话、回传却还跟着
    // 旧转录。
    if surface_holds_other_session(
        &hooks.active_by_surface,
        &binding.surface_id,
        &binding.session_id,
    ) {
        return Liveness::Ended(summon_i18n::text!("rd8578af411"));
    }
    match process_start(binding.pid) {
        Some(start) if start == binding.pid_start_seconds => Liveness::Alive,
        Some(_) => Liveness::Ended(summon_i18n::text!("ra32dff4ecb")),
        None => Liveness::Ended(summon_i18n::text!("r4d7569ed4f")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use summon_core::remote::{CmuxClaudeSession, RemoteBinding, REMOTE_BINDING_SCHEMA_VERSION};

    fn binding(chat: &str, sid: &str) -> RemoteBinding {
        RemoteBinding {
            schema_version: REMOTE_BINDING_SCHEMA_VERSION,
            agent: "a".into(),
            scope: "a-s".into(),
            chat_id: chat.into(),
            chat_type: "group".into(),
            session_id: sid.into(),
            session_name: "n".into(),
            surface_id: "S".into(),
            workspace_id: "W".into(),
            pid: 42,
            pid_start_seconds: 1000,
            transcript_path: "/t.jsonl".into(),
            actor_open_id: "ou".into(),
            created_at_ms: 1,
            ended: None,
        }
    }

    #[test]
    fn save_load_list_remove_and_offset() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        assert_eq!(store.load("oc_a").unwrap(), None);
        store.save(&binding("oc_a", "sid-1")).unwrap();
        store.write_offset("oc_a", 123).unwrap();
        assert_eq!(store.load("oc_a").unwrap(), Some(binding("oc_a", "sid-1")));
        assert_eq!(store.read_offset("oc_a").unwrap(), Some(123));
        assert_eq!(
            store.find_by_session("sid-1").unwrap().map(|b| b.chat_id),
            Some("oc_a".into())
        );
        assert_eq!(store.list().unwrap().len(), 1);
        store.remove("oc_a").unwrap();
        assert_eq!(store.load("oc_a").unwrap(), None);
        assert_eq!(store.read_offset("oc_a").unwrap(), None);
    }

    #[test]
    fn conditional_writes_touch_only_the_same_binding() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        let old = binding("oc_a", "sid-1");
        let rebound = RemoteBinding {
            created_at_ms: 2,
            session_id: "sid-2".into(),
            ..old.clone()
        };
        store.save(&rebound).unwrap();
        store.write_offset("oc_a", 9).unwrap();
        assert!(!store
            .mark_ended_if_current(&old, "会话进程已经退出")
            .unwrap());
        assert!(!store.remove_if_current(&old).unwrap());
        assert_eq!(store.load("oc_a").unwrap(), Some(rebound.clone()));
        assert_eq!(store.read_offset("oc_a").unwrap(), Some(9));

        assert!(store
            .mark_ended_if_current(&rebound, "会话进程已经退出")
            .unwrap());
        assert_eq!(
            store.load("oc_a").unwrap().unwrap().ended.as_deref(),
            Some("会话进程已经退出")
        );
        assert!(store.remove_if_current(&rebound).unwrap());
        assert_eq!(store.load("oc_a").unwrap(), None);
        assert_eq!(store.read_offset("oc_a").unwrap(), None);

        assert!(
            !store.mark_ended_if_current(&rebound, "x").unwrap(),
            "已解除的绑定不能被结束记录写回来"
        );
        assert_eq!(store.load("oc_a").unwrap(), None);
    }

    #[test]
    fn rejects_path_like_chat_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteStore::new(dir.path().to_path_buf());
        assert!(store.load("../x").is_err());
    }

    #[test]
    fn corrupt_record_is_an_error_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteStore::new(dir.path().to_path_buf());
        std::fs::write(dir.path().join("oc_a.json"), "{").unwrap();
        assert!(
            store.load("oc_a").is_err(),
            "坏记录不能当成没绑定,否则群消息会落进本地会话"
        );
    }

    fn session() -> CmuxClaudeSession {
        CmuxClaudeSession {
            session_id: "sid-1".into(),
            surface_id: "S".into(),
            workspace_id: "W".into(),
            pid: 42,
            pid_start_seconds: 1000,
            transcript_path: "/t.jsonl".into(),
            cwd: "/".into(),
        }
    }

    fn hooks(sessions: Vec<CmuxClaudeSession>) -> CmuxHookSessions {
        CmuxHookSessions {
            sessions,
            ..CmuxHookSessions::default()
        }
    }

    #[test]
    fn liveness_requires_record_and_same_process() {
        let b = binding("oc_a", "sid-1");
        assert_eq!(
            check_liveness(&b, &hooks(vec![session()]), &|_| Some(1000)),
            Liveness::Alive
        );
        assert!(matches!(
            check_liveness(&b, &hooks(Vec::new()), &|_| Some(1000)),
            Liveness::Ended(_)
        ));
        assert!(matches!(
            check_liveness(&b, &hooks(vec![session()]), &|_| None),
            Liveness::Ended(_)
        ));
        assert!(matches!(
            check_liveness(&b, &hooks(vec![session()]), &|_| Some(2000)),
            Liveness::Ended(_)
        ));
    }

    #[test]
    fn liveness_ends_when_the_window_now_holds_another_session() {
        let b = binding("oc_a", "sid-1");
        let switched = CmuxHookSessions {
            sessions: vec![session()],
            active_by_surface: BTreeMap::from([("S".to_string(), "sid-2".to_string())]),
            skipped: Vec::new(),
        };
        assert_eq!(
            check_liveness(&b, &switched, &|_| Some(1000)),
            Liveness::Ended("cmux 里这个窗口换了会话")
        );
        let same = CmuxHookSessions {
            active_by_surface: BTreeMap::from([("S".to_string(), "sid-1".to_string())]),
            ..switched.clone()
        };
        assert_eq!(check_liveness(&b, &same, &|_| Some(1000)), Liveness::Alive);
        let unlisted = CmuxHookSessions {
            active_by_surface: BTreeMap::from([("S-other".to_string(), "sid-2".to_string())]),
            ..switched
        };
        assert_eq!(
            check_liveness(&b, &unlisted, &|_| Some(1000)),
            Liveness::Alive,
            "表里没有这个窗口不据此判定"
        );
    }
}
