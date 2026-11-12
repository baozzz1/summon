//! 飞书身份缓存 IO。open_id 是 per-app 的，故生产文件按 agent+app scope 隔离：
//! `~/.summon/run/identity-cache/<scope>.json`。
//!
//! 更新走 flock 下的 read-modify-write + 同目录原子 rename，多个 chat 线程同时记录 mention 时不会
//! 互相覆盖。升级前的全局 `identity_cache.json` 只有在“注册表里恰好一个飞书 agent”时才 copy-on-read；
//! 多 agent 下旧数据归属不可判定，fail-closed 不导入，后续入站会自然重建。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use summon_core::identity_cache::IdentityCache;

use crate::runtime::fsio::atomic_write;
use crate::runtime::paths::run_dir;

#[derive(Debug, Clone)]
pub struct IdentityCacheStore {
    path: PathBuf,
    legacy_path: Option<PathBuf>,
    legacy_claim: Option<crate::runtime::feishu_legacy_claim::FeishuLegacyClaim>,
    allow_legacy_import: bool,
}

impl IdentityCacheStore {
    /// 显式路径接缝只供同 crate 的隔离运行环境使用；普通生产路径仍由 `at_run_dir` 定位。
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            legacy_path: None,
            legacy_claim: None,
            allow_legacy_import: false,
        }
    }

    pub fn at_run_dir(scope: &str, allow_legacy_import: bool) -> Self {
        let run = run_dir();
        Self {
            path: run.join("identity-cache").join(format!("{scope}.json")),
            legacy_path: Some(run.join("identity_cache.json")),
            legacy_claim: Some(crate::runtime::feishu_legacy_claim::FeishuLegacyClaim::new(
                run.join("feishu-legacy.scope"),
                scope,
            )),
            allow_legacy_import,
        }
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }

    fn with_lock<T>(&self, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
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
        f()
    }

    fn read(path: &Path) -> Option<IdentityCache> {
        let json = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&json).ok()
    }

    fn load_unlocked(&self) -> IdentityCache {
        if let Some(cache) = Self::read(&self.path) {
            return cache;
        }
        let Some(cache) = self.legacy_path.as_deref().and_then(Self::read) else {
            return IdentityCache::default();
        };
        if !self.claim_legacy_import() {
            return IdentityCache::default();
        }
        // copy-on-read，旧文件保留供旧版本回滚；新版本后续只读 scoped 文件。
        let _ = self.save_unlocked(&cache);
        cache
    }

    fn claim_legacy_import(&self) -> bool {
        self.allow_legacy_import
            && self
                .legacy_claim
                .as_ref()
                .is_none_or(|claim| claim.claim().unwrap_or(false))
    }

    /// 读缓存。缺失/损坏返回空；若需要安全迁移，会在 flock 内 recheck 后复制。
    pub fn load(&self) -> IdentityCache {
        if let Some(cache) = Self::read(&self.path) {
            return cache;
        }
        if !self.allow_legacy_import {
            return IdentityCache::default();
        }
        self.with_lock(|| Ok(self.load_unlocked()))
            .unwrap_or_default()
    }

    fn save_unlocked(&self, cache: &IdentityCache) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(cache)?;
        atomic_write(&self.path, &json)
    }

    /// 批量记录身份。整个读-合并-裁剪-写事务持有 scope 锁，防并发 lost update。
    pub fn record_identities(&self, items: &[(String, String)], now_ms: i64) -> anyhow::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.with_lock(|| {
            let mut cache = self.load_unlocked();
            cache.record(items, now_ms);
            cache.prune(now_ms, 2_592_000_000); // 30 天
            self.save_unlocked(&cache)
        })
    }

    pub fn has_ambiguous_legacy_data(&self) -> bool {
        let cannot_import = !self.allow_legacy_import
            || self
                .legacy_claim
                .as_ref()
                .is_some_and(|claim| claim.claimed_by_other());
        cannot_import && self.legacy_path.as_ref().is_some_and(|path| path.is_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let a = IdentityCacheStore::new(dir.path().join("a.json"));
        let b = IdentityCacheStore::new(dir.path().join("b.json"));
        a.record_identities(&[("ou_a".into(), "A".into())], 1000)
            .unwrap();
        let loaded = a.load();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.name_for("ou_a"), Some("A"));
        assert_eq!(loaded.entries["ou_a"].last_seen_ms, 1000);
        assert!(b.load().entries.is_empty());
    }

    #[test]
    fn concurrent_records_keep_every_identity() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(IdentityCacheStore::new(dir.path().join("cache.json")));
        let workers = 12;
        let barrier = Arc::new(Barrier::new(workers));
        let mut joins = Vec::new();
        for i in 0..workers {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            joins.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .record_identities(
                        &[(format!("ou_{i}"), format!("user-{i}"))],
                        10_000 + i as i64,
                    )
                    .unwrap();
            }));
        }
        for join in joins {
            join.join().unwrap();
        }
        assert_eq!(store.load().entries.len(), workers);
    }

    #[test]
    fn legacy_import_requires_unambiguous_scope() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("identity_cache.json");
        let legacy = IdentityCacheStore::new(&legacy_path);
        legacy
            .record_identities(&[("ou_legacy".into(), "旧用户".into())], 1000)
            .unwrap();

        let safe = IdentityCacheStore {
            path: dir.path().join("scoped/single.json"),
            legacy_path: Some(legacy_path.clone()),
            legacy_claim: None,
            allow_legacy_import: true,
        };
        assert_eq!(safe.load().name_for("ou_legacy"), Some("旧用户"));
        assert!(safe.path.is_file());

        let ambiguous = IdentityCacheStore {
            path: dir.path().join("scoped/other.json"),
            legacy_path: Some(legacy_path),
            legacy_claim: None,
            allow_legacy_import: false,
        };
        assert!(ambiguous.load().entries.is_empty());
        assert!(ambiguous.has_ambiguous_legacy_data());
    }

    #[test]
    fn app_rebind_does_not_reimport_legacy_identity_cache() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("identity_cache.json");
        let legacy = IdentityCacheStore::new(&legacy_path);
        legacy
            .record_identities(&[("ou_old_app".into(), "旧身份".into())], 1000)
            .unwrap();
        let claim_path = dir.path().join("feishu-legacy.scope");
        let app_a = IdentityCacheStore {
            path: dir.path().join("scoped/app-a.json"),
            legacy_path: Some(legacy_path.clone()),
            legacy_claim: Some(crate::runtime::feishu_legacy_claim::FeishuLegacyClaim::new(
                &claim_path,
                "agent-app-a",
            )),
            allow_legacy_import: true,
        };
        let app_b = IdentityCacheStore {
            path: dir.path().join("scoped/app-b.json"),
            legacy_path: Some(legacy_path),
            legacy_claim: Some(crate::runtime::feishu_legacy_claim::FeishuLegacyClaim::new(
                &claim_path,
                "agent-app-b",
            )),
            allow_legacy_import: true,
        };

        assert_eq!(app_a.load().name_for("ou_old_app"), Some("旧身份"));
        assert!(app_b.load().entries.is_empty());
        assert!(app_b.has_ambiguous_legacy_data());
    }
}
