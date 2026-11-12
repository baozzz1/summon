//! per-agent+app+chat peers 文件存储 IO：
//! `~/.summon/run/peers/<agent-app-scope>/<chat-tag>.yaml`。
//!
//! 纯 schema 与匹配/合并逻辑在 `summon_core::peers`;本模块只做副作用:原子写(tmp+rename)、
//! 目录扫描、round-trip 读写。镜像 `outbox_store` 的 best-effort 纪律:**损坏/缺失全容忍、
//! 失败只 log,绝不 panic、绝不阻塞调用方**。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use summon_core::peers::{merge_peers, with_observed_kind, Peer, PeerKind, PeersFile};

use crate::runtime::fsio::atomic_write;
use crate::runtime::paths::run_dir;

/// per-chat peers 存储句柄。clone 廉价(只是路径)。
#[derive(Debug, Clone)]
pub struct PeersStore {
    /// 当前 agent+app 作用域目录：`~/.summon/run/peers/<scope>/`。
    root: PathBuf,
    /// 升级前的全局目录：`~/.summon/run/peers/`。只在确认单飞书 agent 时允许 copy-on-read。
    legacy_root: Option<PathBuf>,
    legacy_claim: Option<crate::runtime::feishu_legacy_claim::FeishuLegacyClaim>,
    allow_legacy_import: bool,
}

impl PeersStore {
    /// 在显式根目录上建 store；dispatcher 集成测试用它隔离用户状态，其余生产路径用
    /// [`PeersStore::at_run_dir`]。
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            legacy_root: None,
            legacy_claim: None,
            allow_legacy_import: false,
        }
    }

    /// 生产入口:落在 `~/.summon/run/peers/<scope>/`。旧全局文件只在单 agent 场景安全导入；
    /// 多 agent 下归属不可判定，宁可要求重新 `/introduce`，也不把另一 app 的 open_id 注入当前 app。
    pub fn at_run_dir(scope: &str, allow_legacy_import: bool) -> Self {
        let legacy_root = run_dir().join("peers");
        Self {
            root: legacy_root.join(scope),
            legacy_root: Some(legacy_root),
            legacy_claim: Some(crate::runtime::feishu_legacy_claim::FeishuLegacyClaim::new(
                run_dir().join("feishu-legacy.scope"),
                scope,
            )),
            allow_legacy_import,
        }
    }

    pub(crate) fn matches_scope(&self, scope: &str) -> bool {
        self.root.file_name().and_then(|name| name.to_str()) == Some(scope)
    }

    fn file_path(&self, chat_tag: &str) -> PathBuf {
        self.root.join(format!("{chat_tag}.yaml"))
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join(".lock")
    }

    fn legacy_file_path(&self, chat_tag: &str) -> Option<PathBuf> {
        self.legacy_root
            .as_ref()
            .map(|root| root.join(format!("{chat_tag}.yaml")))
    }

    fn with_lock<T>(&self, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        std::fs::create_dir_all(&self.root)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.lock_path())?;
        lock.lock()?;
        f()
    }

    fn load_scoped(&self, chat_tag: &str) -> Option<PeersFile> {
        read_peers_file(&self.file_path(chat_tag))
    }

    fn load_with_legacy_unlocked(&self, chat_tag: &str) -> Option<PeersFile> {
        if let Some(current) = self.load_scoped(chat_tag) {
            return Some(current);
        }
        let legacy = self
            .legacy_file_path(chat_tag)
            .and_then(|path| read_peers_file(&path))?;
        if !self.claim_legacy_import() {
            return None;
        }
        // copy-on-read：保留旧文件便于回滚；新版本后续只读 scoped 副本。
        let _ = self.save_unlocked(chat_tag, &legacy);
        Some(legacy)
    }

    fn claim_legacy_import(&self) -> bool {
        self.allow_legacy_import
            && self
                .legacy_claim
                .as_ref()
                .is_none_or(|claim| claim.claim().unwrap_or(false))
    }

    /// 读某 chat 的 peers 文件;不存在/损坏 → None(不 panic)。
    pub fn load(&self, chat_tag: &str) -> Option<PeersFile> {
        if let Some(current) = self.load_scoped(chat_tag) {
            return Some(current);
        }
        if !self.allow_legacy_import {
            return None;
        }
        self.with_lock(|| Ok(self.load_with_legacy_unlocked(chat_tag)))
            .ok()
            .flatten()
    }

    /// 原子写 peers 文件(tmp+rename)。父目录不存在则建（测试直接验证 round-trip）。
    #[cfg(test)]
    pub fn save(&self, chat_tag: &str, peers: &PeersFile) -> anyhow::Result<()> {
        self.with_lock(|| self.save_unlocked(chat_tag, peers))
    }

    fn save_unlocked(&self, chat_tag: &str, peers: &PeersFile) -> anyhow::Result<()> {
        let path = self.file_path(chat_tag);
        let yaml = serde_yaml::to_string(peers)?;
        atomic_write(&path, yaml.as_bytes())
    }

    /// 合并新 peers 到现有(按 open_id 去重,new 覆盖 existing),写回并返回合并结果。
    ///
    /// 无现有文件时直接以 new 创建。
    ///
    /// `intro` 区分两条来路:`Some(note)` = 走 `/introduce` 的正式介绍,刷新 `introduced_at`
    /// 并记下这次交代的分工说明(空串视作没说明,保留旧 note);`None` = 入站消息顺带记身份,
    /// 既不算介绍也不动 note。合成一个参数而非 bool + note 两个:两者本就同源,分开传会出现
    /// 「不是介绍却带 note」这种表达不了的中间态。
    pub fn add_peers(
        &self,
        chat_tag: &str,
        new: Vec<Peer>,
        intro: Option<&str>,
    ) -> anyhow::Result<PeersFile> {
        self.with_lock(|| {
            let existing = self
                .load_with_legacy_unlocked(chat_tag)
                .unwrap_or(PeersFile {
                    peers: Vec::new(),
                    introduced_at: None,
                    note: None,
                });
            let merged = merge_peers(&existing.peers, &new);
            let introduced_at = match intro {
                Some(_) => Some(chrono::Local::now().to_rfc3339()),
                None => existing.introduced_at,
            };
            // 这次没写说明就沿用上次的分工,别把已有信息冲掉。
            let note = intro
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .or(existing.note);
            let out = PeersFile {
                peers: merged,
                introduced_at,
                note,
            };
            self.save_unlocked(chat_tag, &out)?;
            Ok(out)
        })
    }

    /// 按观察到的身份补记 / 更正某个 peer(见 `summon_core::peers::with_observed_kind`)。
    /// 返回是否落了盘:没有 peers 文件、不在名单里、身份未知或没变,都不写。
    pub fn record_peer_kind(
        &self,
        chat_tag: &str,
        open_id: &str,
        observed: PeerKind,
    ) -> anyhow::Result<bool> {
        self.with_lock(|| {
            let Some(existing) = self.load_with_legacy_unlocked(chat_tag) else {
                return Ok(false);
            };
            let Some(peers) = with_observed_kind(&existing.peers, open_id, observed) else {
                return Ok(false);
            };
            let out = PeersFile { peers, ..existing };
            self.save_unlocked(chat_tag, &out)?;
            Ok(true)
        })
    }

    /// 从本 chat 的 peers 里移除指定 `open_id`,返回 `(实际移除数, 剩余数)`。
    ///
    /// **移除到一个不剩时连文件一起删**:留一个 `peers: []` 的空文件没有意义,而且会让
    /// `load` 返回 `Some(空表)` —— 那与「从没 introduce 过」(`None`)在语义上难以区分,
    /// 上游判据一旦分不清这两者就会出微妙的错。删掉即回到「从没认识过」的干净状态。
    ///
    /// 对不在表里的 open_id 是 no-op(不报错):单向关系(A 认识 B 但 B 不认识 A)解除时,
    /// 两边跑同一套逻辑,其中一边本来就没有对方 —— 那不是错误,是正常情形。
    pub fn remove_peers(
        &self,
        chat_tag: &str,
        open_ids: &[String],
    ) -> anyhow::Result<(usize, usize)> {
        self.with_lock(|| {
            let Some(existing) = self.load_with_legacy_unlocked(chat_tag) else {
                return Ok((0, 0)); // 从没 introduce 过,无事可做
            };
            let before = existing.peers.len();
            let remaining: Vec<Peer> = existing
                .peers
                .into_iter()
                .filter(|p| !open_ids.iter().any(|id| id == &p.open_id))
                .collect();
            let removed = before - remaining.len();
            if removed == 0 {
                return Ok((0, before)); // 没动就不落盘,少一次无谓写
            }
            if remaining.is_empty() {
                self.remove_unlocked(chat_tag)?;
                return Ok((removed, 0));
            }
            let out = PeersFile {
                peers: remaining,
                introduced_at: existing.introduced_at,
                note: existing.note,
            };
            let left = out.peers.len();
            self.save_unlocked(chat_tag, &out)?;
            Ok((removed, left))
        })
    }

    /// 群解散/bot 被移除后的 per-chat 回收。scoped 文件始终删除；旧全局文件只有在当前 app
    /// 能安全认领 legacy 数据时才删，避免多 agent 下误删归属不明的其他 app 数据。
    pub fn remove(&self, chat_tag: &str) -> anyhow::Result<bool> {
        self.with_lock(|| self.remove_unlocked(chat_tag))
    }

    /// [`Self::remove`] 的锁内主体。**必须单独提出来**:`remove_peers` 也在 `with_lock` 里,
    /// 直接调 `remove()` 会重入同一把锁。
    fn remove_unlocked(&self, chat_tag: &str) -> anyhow::Result<bool> {
        {
            let mut removed = match std::fs::remove_file(self.file_path(chat_tag)) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.into()),
            };
            if self.claim_legacy_import() {
                if let Some(path) = self.legacy_file_path(chat_tag) {
                    match std::fs::remove_file(path) {
                        Ok(()) => removed = true,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Ok(removed)
        }
    }

    /// 多 agent 升级时是否检测到无法安全归属的旧全局数据（仅用于一次性告警）。
    pub fn has_ambiguous_legacy_data(&self) -> bool {
        let cannot_import = !self.allow_legacy_import
            || self
                .legacy_claim
                .as_ref()
                .is_some_and(|claim| claim.claimed_by_other());
        if !cannot_import {
            return false;
        }
        self.legacy_root.as_ref().is_some_and(|root| {
            std::fs::read_dir(root).is_ok_and(|entries| {
                entries.flatten().any(|e| {
                    e.path().is_file()
                        && e.path().extension().and_then(|s| s.to_str()) == Some("yaml")
                })
            })
        })
    }
}

/// 读单个 peers 文件;损坏/读不到 → None。
fn read_peers_file(path: &Path) -> Option<PeersFile> {
    let s = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PeersStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = PeersStore::new(dir.path().join("peers"));
        (dir, store)
    }

    fn peer(id: &str, name: &str) -> Peer {
        Peer {
            open_id: id.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    /// 部分移除:只去掉点名的,其余原样留着。`/unintroduce @A @B` 不该波及没被 @ 的旧相识。
    #[test]
    fn remove_peers_drops_only_named_and_keeps_the_rest() {
        let (_dir, store) = store();
        let tag = "g_partial";
        store
            .add_peers(
                tag,
                vec![peer("ou_a", "A"), peer("ou_b", "B"), peer("ou_c", "C")],
                Some("分工说明"),
            )
            .unwrap();

        let (removed, left) = store.remove_peers(tag, &["ou_b".to_string()]).unwrap();
        assert_eq!((removed, left), (1, 2));
        let after = store.load(tag).expect("还有 peer 时文件必须留着");
        assert_eq!(
            after
                .peers
                .iter()
                .map(|p| p.open_id.as_str())
                .collect::<Vec<_>>(),
            vec!["ou_a", "ou_c"]
        );
        assert_eq!(
            after.note.as_deref(),
            Some("分工说明"),
            "note 不该被移除操作冲掉"
        );
    }

    /// 移到一个不剩 → **连文件一起删**。留 `peers: []` 会让 `load` 返回 `Some(空表)`,
    /// 与「从没 introduce 过」(`None`)在语义上分不清,上游判据迟早出微妙的错。
    #[test]
    fn remove_peers_deletes_file_when_nothing_left() {
        let (_dir, store) = store();
        let tag = "g_empty";
        store
            .add_peers(tag, vec![peer("ou_a", "A"), peer("ou_b", "B")], None)
            .unwrap();

        let (removed, left) = store
            .remove_peers(tag, &["ou_a".to_string(), "ou_b".to_string()])
            .unwrap();
        assert_eq!((removed, left), (2, 0));
        assert!(
            store.load(tag).is_none(),
            "全空后必须回到「从没认识过」,而不是留个空表"
        );
    }

    /// 移除不存在的项是 no-op,不报错。单向关系(A 认识 B 但 B 不认识 A)解除时两边跑同一套
    /// 逻辑,其中一边本来就没有对方 —— 那是正常情形,不是错误。
    #[test]
    fn remove_peers_is_noop_for_unknown_ids_and_missing_file() {
        let (_dir, store) = store();
        // 文件都还没有
        assert_eq!(
            store.remove_peers("g_none", &["ou_x".into()]).unwrap(),
            (0, 0)
        );

        let tag = "g_some";
        store.add_peers(tag, vec![peer("ou_a", "A")], None).unwrap();
        assert_eq!(
            store.remove_peers(tag, &["ou_zzz".into()]).unwrap(),
            (0, 1),
            "不在表里的 id 不该动任何东西"
        );
        assert!(store.load(tag).is_some(), "no-op 不该把文件删掉");
    }

    /// 向后兼容:`note` 是后加的字段,盘上已有的 peers 文件没有它。少字段必须照读不误
    /// (`#[serde(default)]`),否则升级后老文件全被当损坏丢弃、群里的 peers 关系凭空消失。
    #[test]
    fn loads_legacy_file_without_note_field() {
        let (_dir, store) = store();
        let tag = "g_legacy";
        std::fs::create_dir_all(&store.root).unwrap();
        // 升级前的落盘格式:只有 peers + introduced_at
        let legacy =
            "peers:\n- open_id: ou_bot\n  name: 星光\nintroduced_at: '2026-07-06T10:00:00Z'\n";
        std::fs::write(store.file_path(tag), legacy).unwrap();

        let loaded = store.load(tag).expect("老格式文件必须仍可读");
        assert_eq!(loaded.peers.len(), 1);
        assert_eq!(loaded.peers[0].name, "星光");
        assert!(loaded.note.is_none(), "老文件没有 note,应降级为 None");

        // 老文件上再跑一次 /introduce:补上 note,peers 不丢
        store
            .add_peers(tag, loaded.peers.clone(), Some("星光开发"))
            .unwrap();
        let after = store.load(tag).unwrap();
        assert_eq!(after.note.as_deref(), Some("星光开发"));
        assert_eq!(after.peers.len(), 1);
    }

    #[test]
    fn save_then_load_round_trip() {
        let (_dir, store) = store();
        let pf = PeersFile {
            peers: vec![
                Peer {
                    open_id: "ou_1".into(),
                    name: "星光".into(),
                    ..Default::default()
                },
                Peer {
                    open_id: "ou_2".into(),
                    name: "黑魔术师".into(),
                    ..Default::default()
                },
            ],
            introduced_at: Some("2026-07-06T10:00:00Z".into()),
            note: None,
        };
        store.save("g1234abcd", &pf).unwrap();
        let loaded = store.load("g1234abcd").unwrap();
        assert_eq!(loaded.peers.len(), 2);
        assert_eq!(loaded.peers[0].open_id, "ou_1");
        assert_eq!(
            loaded.introduced_at.as_deref(),
            Some("2026-07-06T10:00:00Z")
        );
    }

    #[test]
    fn load_missing_returns_none() {
        let (_dir, store) = store();
        assert!(store.load("nonexistent").is_none());
    }

    #[test]
    fn remove_is_idempotent_and_deletes_scoped_chat() {
        let (_dir, store) = store();
        let pf = PeersFile {
            peers: vec![Peer {
                open_id: "ou_1".into(),
                name: "A".into(),
                ..Default::default()
            }],
            introduced_at: None,
            note: None,
        };
        store.save("gone", &pf).unwrap();
        assert!(store.remove("gone").unwrap());
        assert!(store.load("gone").is_none());
        assert!(!store.remove("gone").unwrap());
    }

    #[test]
    fn add_peers_merges_and_dedups() {
        let (_dir, store) = store();
        // 第一次 add
        let r1 = store
            .add_peers(
                "g1",
                vec![Peer {
                    open_id: "ou_1".into(),
                    name: "A".into(),
                    ..Default::default()
                }],
                None,
            )
            .unwrap();
        assert_eq!(r1.peers.len(), 1);
        // 第二次 add:同 open_id 覆盖 + 新增
        let r2 = store
            .add_peers(
                "g1",
                vec![
                    Peer {
                        open_id: "ou_1".into(),
                        name: "A-new".into(),
                        ..Default::default()
                    },
                    Peer {
                        open_id: "ou_2".into(),
                        name: "B".into(),
                        ..Default::default()
                    },
                ],
                None,
            )
            .unwrap();
        assert_eq!(r2.peers.len(), 2);
        assert_eq!(
            r2.peers.iter().find(|p| p.open_id == "ou_1").unwrap().name,
            "A-new"
        );
        // 再 load 确认落盘一致
        let loaded = store.load("g1").unwrap();
        assert_eq!(loaded.peers.len(), 2);
    }

    #[test]
    fn add_peers_touch_introduced_at() {
        let (_dir, store) = store();
        // touch_introduced_at=true → introduced_at 被设置
        let r = store
            .add_peers(
                "g1",
                vec![Peer {
                    open_id: "ou_1".into(),
                    name: "A".into(),
                    ..Default::default()
                }],
                Some(""),
            )
            .unwrap();
        assert!(r.introduced_at.is_some());
        // 再次 add 不带 touch → introduced_at 保留
        let r2 = store
            .add_peers(
                "g1",
                vec![Peer {
                    open_id: "ou_2".into(),
                    name: "B".into(),
                    ..Default::default()
                }],
                None,
            )
            .unwrap();
        assert_eq!(r2.introduced_at, r.introduced_at);
    }

    #[test]
    fn corrupt_file_skipped() {
        let (_dir, store) = store();
        std::fs::create_dir_all(&store.root).unwrap();
        std::fs::write(store.root.join("bad.yaml"), b"not: valid: yaml: [").unwrap();
        assert!(store.load("bad").is_none());
    }

    #[test]
    fn scopes_do_not_share_same_chat_peers() {
        let dir = tempfile::tempdir().unwrap();
        let a = PeersStore::new(dir.path().join("peers/app-a"));
        let b = PeersStore::new(dir.path().join("peers/app-b"));
        a.add_peers(
            "g_same_chat",
            vec![Peer {
                open_id: "ou_a".into(),
                name: "A".into(),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
        assert!(b.load("g_same_chat").is_none());
    }

    #[test]
    fn concurrent_add_peers_keeps_every_update() {
        use std::sync::{Arc, Barrier};

        let (_dir, store) = store();
        let store = Arc::new(store);
        let workers = 12;
        let barrier = Arc::new(Barrier::new(workers));
        let mut joins = Vec::new();
        for i in 0..workers {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            joins.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .add_peers(
                        "g_concurrent",
                        vec![Peer {
                            open_id: format!("ou_{i}"),
                            name: format!("bot-{i}"),
                            ..Default::default()
                        }],
                        None,
                    )
                    .unwrap();
            }));
        }
        for join in joins {
            join.join().unwrap();
        }
        let loaded = store.load("g_concurrent").unwrap();
        assert_eq!(loaded.peers.len(), workers);
    }

    #[test]
    fn legacy_data_is_only_imported_when_scope_is_unambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_root = dir.path().join("peers");
        let legacy = PeersStore::new(&legacy_root);
        legacy
            .add_peers(
                "g1",
                vec![Peer {
                    open_id: "ou_legacy".into(),
                    name: "旧 bot".into(),
                    ..Default::default()
                }],
                None,
            )
            .unwrap();

        let safe_migration = PeersStore {
            root: legacy_root.join("single-agent-scope"),
            legacy_root: Some(legacy_root.clone()),
            legacy_claim: None,
            allow_legacy_import: true,
        };
        assert_eq!(
            safe_migration.load("g1").unwrap().peers[0].open_id,
            "ou_legacy"
        );
        assert!(safe_migration.file_path("g1").is_file());

        let ambiguous = PeersStore {
            root: legacy_root.join("another-app"),
            legacy_root: Some(legacy_root),
            legacy_claim: None,
            allow_legacy_import: false,
        };
        assert!(ambiguous.load("g1").is_none());
        assert!(ambiguous.has_ambiguous_legacy_data());
    }

    #[test]
    fn app_rebind_does_not_reimport_legacy_peers() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_root = dir.path().join("peers");
        let legacy = PeersStore::new(&legacy_root);
        legacy
            .add_peers(
                "g1",
                vec![Peer {
                    open_id: "ou_old_app".into(),
                    name: "旧 bot".into(),
                    ..Default::default()
                }],
                None,
            )
            .unwrap();
        let claim_path = dir.path().join("feishu-legacy.scope");
        let app_a = PeersStore {
            root: legacy_root.join("agent-app-a"),
            legacy_root: Some(legacy_root.clone()),
            legacy_claim: Some(crate::runtime::feishu_legacy_claim::FeishuLegacyClaim::new(
                &claim_path,
                "agent-app-a",
            )),
            allow_legacy_import: true,
        };
        let app_b = PeersStore {
            root: legacy_root.join("agent-app-b"),
            legacy_root: Some(legacy_root),
            legacy_claim: Some(crate::runtime::feishu_legacy_claim::FeishuLegacyClaim::new(
                &claim_path,
                "agent-app-b",
            )),
            allow_legacy_import: true,
        };

        assert_eq!(app_a.load("g1").unwrap().peers[0].open_id, "ou_old_app");
        assert!(app_b.load("g1").is_none());
        assert!(app_b.has_ambiguous_legacy_data());
    }

    /// peer 发消息时补记身份:只在确有变化时落盘,且不动 introduce 时间与分工说明。
    #[test]
    fn record_peer_kind_writes_only_new_information() {
        let (_dir, store) = store();
        store
            .add_peers("gchat", vec![peer("ou_h", "张三")], Some("张三提需求"))
            .unwrap();
        let before = store.load("gchat").unwrap();

        assert!(store
            .record_peer_kind("gchat", "ou_h", PeerKind::Human)
            .unwrap());
        let after = store.load("gchat").unwrap();
        assert_eq!(after.peers[0].kind, PeerKind::Human);
        assert_eq!(after.note, before.note);
        assert_eq!(after.introduced_at, before.introduced_at);

        assert!(
            !store
                .record_peer_kind("gchat", "ou_h", PeerKind::Human)
                .unwrap(),
            "没变不写"
        );
        assert!(
            !store
                .record_peer_kind("gchat", "ou_x", PeerKind::Bot)
                .unwrap(),
            "不在名单"
        );
        assert!(
            !store
                .record_peer_kind("gnone", "ou_h", PeerKind::Human)
                .unwrap(),
            "没有文件"
        );
        assert!(store.load("gnone").is_none(), "不为补记凭空建文件");
    }
}
