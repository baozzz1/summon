//! 旧版全局飞书身份数据的一次性迁移认领。
//!
//! 旧 `peers/` 与 `identity_cache.json` 都没有 agent/app owner 元数据。即使当前只有一个飞书
//! agent，也只能把它们迁入一个确定的 agent+app scope；agent 后续换绑 app 时不得再次复制。
//! 两类 store 共用同一 claim 文件和 flock，避免各自认领给不同 scope。

use std::fs::OpenOptions;
use std::path::PathBuf;

use crate::runtime::fsio::atomic_write;

#[derive(Debug, Clone)]
pub(crate) struct FeishuLegacyClaim {
    path: PathBuf,
    scope: String,
}

impl FeishuLegacyClaim {
    pub(crate) fn new(path: impl Into<PathBuf>, scope: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            scope: scope.into(),
        }
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }

    /// 原子认领旧数据。首次调用写入 scope；同 scope 可重复使用，其他 scope 一律 fail-closed。
    pub(crate) fn claim(&self) -> anyhow::Result<bool> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.lock_path())?;
        lock.lock()?;

        if self.path.is_file() {
            let owner = std::fs::read_to_string(&self.path)?;
            return Ok(owner.trim() == self.scope);
        }
        atomic_write(&self.path, format!("{}\n", self.scope).as_bytes())?;
        Ok(true)
    }

    /// 只读判断 claim 是否已属于另一 scope；用于把换绑后的旧数据标为“有但不可导入”。
    pub(crate) fn claimed_by_other(&self) -> bool {
        std::fs::read_to_string(&self.path)
            .ok()
            .map(|owner| owner.trim() != self.scope)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_scope_claims_and_rebind_cannot_reimport() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-feishu.scope");
        let app_a = FeishuLegacyClaim::new(&path, "agent-app-a");
        let app_b = FeishuLegacyClaim::new(&path, "agent-app-b");

        assert!(app_a.claim().unwrap());
        assert!(app_a.claim().unwrap(), "同 scope 重复读取应保持可迁移");
        assert!(!app_b.claim().unwrap(), "换绑 app 不得重新导入旧 open_id");
        assert!(app_b.claimed_by_other());
    }
}
