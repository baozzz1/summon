use anyhow::Context;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub(crate) struct AgentAvailability {
    path: PathBuf,
    _lock: File,
}

impl AgentAvailability {
    pub(crate) fn lock(inbox_root: &Path, agent: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !agent.is_empty() && agent != "." && agent != ".." && !agent.contains(['/', '\\']),
            summon_i18n::format!("r503bd51dfd")
        );
        let root = inbox_root.join("availability");
        std::fs::create_dir_all(&root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(format!("{agent}.lock")))?;
        lock.lock().context(summon_i18n::text!("r26608eda82"))?;
        Ok(Self {
            path: root.join(format!("{agent}.json")),
            _lock: lock,
        })
    }

    pub(crate) fn enabled(&self) -> anyhow::Result<bool> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context(summon_i18n::text!("r5a3681f60f")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        super::fsio::atomic_write_mode(&self.path, &serde_json::to_vec(&enabled)?, 0o600)
    }
}

/// 本地会话创建与清理共用跨进程屏障，锁内重读身份，避免使用已删除的快照启动进程。
pub(crate) fn with_local_start<T>(
    agent: &summon_core::registry::Agent,
    inbox: &Path,
    start: impl FnOnce(&summon_core::registry::Agent) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _manifest = crate::setup::bind::manifest_write_lock(&agent.manifest_path())?;
    let current = agent.reread().context(summon_i18n::text!("rcc1012ee0a"))?;
    anyhow::ensure!(
        current.name == agent.name,
        summon_i18n::format!("r0e015781de")
    );
    let gate = AgentAvailability::lock(inbox, &current.name)?;
    anyhow::ensure!(
        gate.enabled()?,
        summon_i18n::format!("rfd5c3c38cd", current.name)
    );
    // manifest 锁已排斥清理和启停；执行期间允许工具读取启用状态。
    drop(gate);
    start(&current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_start_cannot_cross_cleanup_lock_disabled_or_removed_manifest() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("agents/sample");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.yaml"), "name: sample\n").unwrap();
        let mut agent: summon_core::registry::Agent =
            serde_yaml::from_str("name: sample\n").unwrap();
        agent.dir = dir.clone();
        let inbox = home.path().join("run/inbox");
        let marker = home.path().join("process-started");
        let cleanup = crate::setup::bind::manifest_write_lock(&agent.manifest_path()).unwrap();
        let worker_agent = agent.clone();
        let worker_inbox = inbox.clone();
        let worker_marker = marker.clone();
        let worker = std::thread::spawn(move || {
            with_local_start(&worker_agent, &worker_inbox, |_| {
                std::fs::write(worker_marker, b"started")?;
                Ok(())
            })
        });
        assert!(worker.join().unwrap().is_err());
        assert!(!marker.exists());
        drop(cleanup);
        AgentAvailability::lock(&inbox, "sample")
            .unwrap()
            .set_enabled(false)
            .unwrap();
        assert!(with_local_start(&agent, &inbox, |_| {
            std::fs::write(&marker, b"started")?;
            Ok(())
        })
        .is_err());
        assert!(!marker.exists());
        AgentAvailability::lock(&inbox, "sample")
            .unwrap()
            .set_enabled(true)
            .unwrap();
        std::fs::remove_file(agent.manifest_path()).unwrap();
        assert!(with_local_start(&agent, &inbox, |_| {
            std::fs::write(&marker, b"started")?;
            Ok(())
        })
        .is_err());
        assert!(!marker.exists());
        std::fs::write(agent.manifest_path(), "name: sample\ndisplay: current\n").unwrap();
        with_local_start(&agent, &inbox, |fresh| {
            assert_eq!(fresh.display.as_str(), "current");
            std::fs::write(&marker, b"started")?;
            Ok(())
        })
        .unwrap();
        assert!(marker.exists());
    }

    #[test]
    fn availability_persists_and_invalid_state_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let gate = AgentAvailability::lock(root.path(), "agent").unwrap();
        assert!(gate.enabled().unwrap());
        gate.set_enabled(false).unwrap();
        drop(gate);
        let gate = AgentAvailability::lock(root.path(), "agent").unwrap();
        assert!(!gate.enabled().unwrap());
        gate.set_enabled(true).unwrap();
        assert!(gate.enabled().unwrap());
        std::fs::write(&gate.path, b"bad").unwrap();
        assert!(gate.enabled().is_err());
    }

    #[test]
    fn availability_serializes_changes_with_admission() {
        let root = tempfile::tempdir().unwrap();
        let gate = AgentAvailability::lock(root.path(), "agent").unwrap();
        let path = root.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let gate = AgentAvailability::lock(&path, "agent").unwrap();
            gate.set_enabled(false).unwrap();
            sender.send(()).unwrap();
        });
        assert!(receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
        assert!(gate.enabled().unwrap());
        drop(gate);
        receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        worker.join().unwrap();
        assert!(!AgentAvailability::lock(root.path(), "agent")
            .unwrap()
            .enabled()
            .unwrap());
    }
}
