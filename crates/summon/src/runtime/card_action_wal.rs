//! 飞书 card callback 的 daemon durable inbox。sidecar 只有收到本 WAL fsync 后的 ACK 才向
//! 飞书确认成功；dispatcher 正常处理完再写脱敏终态并删除 pending。持久 receive sequence 保证
//! daemon 崩溃后的重放顺序与崩溃前 ACK 顺序一致。

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::runtime::daemon::CardActionMsg;
use crate::runtime::fsio::atomic_write_mode;
use crate::runtime::paths::run_dir;

const SCHEMA_VERSION: u32 = 2;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_PENDING_FILES: usize = 4_096;
const MAX_PENDING_BYTES: u64 = 64 * 1024 * 1024;
const SEQUENCE_FILE: &str = "next-sequence";
static WAL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CardActionRecord {
    schema_version: u32,
    /// 同 app_id + Feishu event_id 的稳定幂等键；不承担排序语义。
    pub(crate) id: String,
    /// daemon durable 接收顺序；pending 文件名以固定宽度序号开头，冷启按它重放。
    pub(crate) sequence: u64,
    pub(crate) app_id: String,
    pub(crate) card: CardActionMsg,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompletedCardAction {
    schema_version: u32,
    id: String,
    sequence: u64,
    app_id: String,
    event_id: String,
    completed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PersistOutcome {
    Pending {
        record: Box<CardActionRecord>,
        created: bool,
    },
    Consumed,
}

#[derive(Debug, Clone)]
pub(crate) struct CardActionWal {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CardActionWalHealth {
    pub(crate) pending: usize,
    pub(crate) consumed: usize,
    pub(crate) corrupt: usize,
}

#[derive(Debug)]
struct PendingEntry {
    sequence: u64,
    id: String,
    path: PathBuf,
}

impl CardActionWal {
    pub(crate) fn at_run_dir() -> Self {
        Self::new(run_dir().join("card-inbox"))
    }

    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn pending_dir(&self) -> PathBuf {
        self.root.join("pending")
    }

    fn consumed_dir(&self) -> PathBuf {
        self.root.join("consumed")
    }

    fn sequence_path(&self) -> PathBuf {
        self.root.join(SEQUENCE_FILE)
    }

    fn ensure_layout(&self) -> Result<()> {
        std::fs::create_dir_all(self.pending_dir()).context(summon_i18n::text!("r253f6509dc"))?;
        std::fs::create_dir_all(self.consumed_dir()).context(summon_i18n::text!("r709520a51f"))?;
        for path in [&self.root, &self.pending_dir(), &self.consumed_dir()] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// 运维只读快照；不建目录、不清理 tombstone。任何 body/identity/sequence 异常都计入
    /// corrupt，供 status/doctor 明确提示 callback 顺序已 fail-closed。
    pub(crate) fn inspect_health(&self) -> Result<CardActionWalHealth> {
        let _guard = wal_lock();
        if !self.root.exists() {
            return Ok(CardActionWalHealth::default());
        }
        let mut health = CardActionWalHealth::default();
        let mut sequences = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut pending_records = BTreeMap::new();
        for path in json_paths_read_only(&self.pending_dir())? {
            match read_record(&path) {
                Ok(record) => {
                    let sequence_unique = sequences.insert(record.sequence);
                    let id_unique = ids.insert(record.id.clone());
                    let unique = sequence_unique && id_unique;
                    if unique {
                        pending_records.insert(record.id.clone(), record);
                        health.pending = health.pending.saturating_add(1);
                    } else {
                        health.corrupt = health.corrupt.saturating_add(1);
                    }
                }
                Err(_) => health.corrupt = health.corrupt.saturating_add(1),
            }
        }
        for path in json_paths_read_only(&self.consumed_dir())? {
            match read_completed(&path) {
                Ok(completed) => {
                    if pending_records.get(&completed.id).is_some_and(|pending| {
                        completed.sequence != pending.sequence
                            || completed.app_id != pending.app_id
                            || completed.event_id != pending.card.event_id
                    }) {
                        health.corrupt = health.corrupt.saturating_add(1);
                    }
                    health.consumed = health.consumed.saturating_add(1);
                }
                Err(_) => health.corrupt = health.corrupt.saturating_add(1),
            }
        }
        if health.corrupt == 0
            && self
                .sequence_high_watermark()
                .and_then(|max_seen| self.read_next_sequence(max_seen))
                .is_err()
        {
            health.corrupt = health.corrupt.saturating_add(1);
        }
        Ok(health)
    }

    /// daemon ready 前的严格屏障。允许修复“consumed tombstone 已提交、plaintext pending 尚未删”
    /// 的幂等残留；除此之外，body/identity/sequence 任一不一致都向上传播并拒绝启动。
    pub(crate) fn preflight(&self) -> Result<CardActionWalHealth> {
        let _guard = wal_lock();
        if !self.root.exists() {
            return Ok(CardActionWalHealth::default());
        }
        self.ensure_layout()?;
        let pending = self.validated_pending()?;
        let max_seen = self.sequence_high_watermark()?;
        self.read_next_sequence(max_seen)
            .context(summon_i18n::text!("r4b230e458e"))?;
        let consumed = json_paths_read_only(&self.consumed_dir())?.len();
        Ok(CardActionWalHealth {
            pending: pending.len(),
            consumed,
            corrupt: 0,
        })
    }

    pub(crate) fn persist(&self, app_id: &str, card: &CardActionMsg) -> Result<PersistOutcome> {
        anyhow::ensure!(!app_id.is_empty(), summon_i18n::format!("rf64a451195"));
        anyhow::ensure!(
            !card.event_id.is_empty() && card.event_id.len() <= 256,
            summon_i18n::format!("ra9e13c47bd")
        );
        anyhow::ensure!(is_durable_card(card), summon_i18n::format!("rdb0d5e5eb5"));
        let _guard = wal_lock();
        self.ensure_layout()?;
        let id = stable_id(app_id, &card.event_id);
        let consumed_path = self.consumed_dir().join(format!("{id}.json"));
        if consumed_path.exists() {
            let consumed = read_completed(&consumed_path)?;
            anyhow::ensure!(
                consumed.id == id
                    && consumed.app_id == app_id
                    && consumed.event_id == card.event_id,
                summon_i18n::format!("r927c8a2a70")
            );
            if let Some(entry) = self.find_pending(&id)? {
                let _ = std::fs::remove_file(entry.path);
            }
            return Ok(PersistOutcome::Consumed);
        }

        // 接受并 ACK 新 callback 前验证全部更早 pending body。只验文件名会让早期 JSON bitrot
        // 被 live dispatch 越过，破坏“崩溃重放顺序=durable ACK 顺序”。目录有 64MiB 硬上限。
        let pending = self.validated_pending()?;
        if let Some((_, existing)) = pending.iter().find(|(entry, _)| entry.id == id) {
            let mut expected_card = card.clone();
            expected_card.wal_id = Some(id);
            anyhow::ensure!(
                existing.app_id == app_id && existing.card == expected_card,
                summon_i18n::format!("r02a3b921e9")
            );
            return Ok(PersistOutcome::Pending {
                record: Box::new(existing.clone()),
                created: false,
            });
        }

        let max_sequence = self.sequence_high_watermark()?;
        let sequence = self.read_next_sequence(max_sequence)?;
        let mut durable_card = card.clone();
        durable_card.wal_id = Some(id.clone());
        let record = CardActionRecord {
            schema_version: SCHEMA_VERSION,
            id: id.clone(),
            sequence,
            app_id: app_id.to_string(),
            card: durable_card,
        };
        let content = serde_json::to_vec(&record)?;
        let (files, bytes) = pending_usage(&self.pending_dir())?;
        anyhow::ensure!(
            content.len() as u64 <= MAX_RECORD_BYTES,
            summon_i18n::format!("rf323b5c6b8")
        );
        anyhow::ensure!(
            files < MAX_PENDING_FILES
                && bytes
                    .checked_add(content.len() as u64)
                    .is_some_and(|total| total <= MAX_PENDING_BYTES),
            summon_i18n::format!("rf6abe2f651")
        );

        // 先持久化 next，再提交 record；中途崩溃只会产生序号空洞，不会复用序号导致乱序。
        let next = sequence
            .checked_add(1)
            .context(summon_i18n::text!("r3b6d152eb1"))?;
        atomic_write_mode(&self.sequence_path(), next.to_string().as_bytes(), 0o600)?;
        let path = self
            .pending_dir()
            .join(pending_file_name(sequence, &record.id));
        atomic_write_mode(&path, &content, 0o600)?;
        Ok(PersistOutcome::Pending {
            record: Box::new(record),
            created: true,
        })
    }

    /// 返回完整、严格按 receive sequence 排序的快照。任一 pending 损坏都会整体失败，绝不跳过
    /// 较早 callback 后继续执行较晚 callback。
    pub(crate) fn pending(&self) -> Result<Vec<CardActionRecord>> {
        let _guard = wal_lock();
        self.ensure_layout()?;
        let records = self
            .validated_pending()?
            .into_iter()
            .map(|(_, record)| record)
            .collect::<Vec<_>>();
        let max_seen = self.sequence_high_watermark()?;
        self.read_next_sequence(max_seen)
            .context(summon_i18n::text!("r4b230e458e"))?;
        Ok(records)
    }

    /// 先写只含 identity 的脱敏终态，再删除含正文 pending。ACK 丢失后的 Feishu 重投会命中终态，
    /// 不会再次执行 stop/context 等非幂等动作。
    pub(crate) fn ack(&self, id: &str) -> Result<()> {
        anyhow::ensure!(valid_id(id), summon_i18n::format!("r5ea3ba4680"));
        let _guard = wal_lock();
        self.ensure_layout()?;
        let consumed_path = self.consumed_dir().join(format!("{id}.json"));
        if consumed_path.exists() {
            let completed = read_completed(&consumed_path)?;
            anyhow::ensure!(completed.id == id, summon_i18n::format!("rbc3e73f6d2"));
            if let Some(entry) = self.find_pending(id)? {
                let _ = std::fs::remove_file(entry.path);
            }
            return Ok(());
        }
        let entry = self
            .find_pending(id)?
            .with_context(|| summon_i18n::format!("rb788140249"))?;
        let record = read_record(&entry.path)?;
        let completed = CompletedCardAction {
            schema_version: SCHEMA_VERSION,
            id: id.to_string(),
            sequence: record.sequence,
            app_id: record.app_id,
            event_id: record.card.event_id,
            completed_at_ms: now_ms(),
        };
        atomic_write_mode(&consumed_path, &serde_json::to_vec(&completed)?, 0o600)?;
        std::fs::remove_file(&entry.path).context(summon_i18n::text!("r964152b56e"))
    }

    fn read_next_sequence(&self, max_pending: u64) -> Result<u64> {
        let path = self.sequence_path();
        if !path.exists() {
            return max_pending
                .checked_add(1)
                .context(summon_i18n::text!("r3b6d152eb1"));
        }
        let raw = std::fs::read_to_string(&path).context(summon_i18n::text!("r589c075a09"))?;
        let next = raw
            .trim()
            .parse::<u64>()
            .context(summon_i18n::text!("re797a9d9dc"))?;
        anyhow::ensure!(
            next > 0 && next > max_pending,
            summon_i18n::format!("r55345ec694")
        );
        Ok(next)
    }

    fn pending_entries(&self) -> Result<Vec<PendingEntry>> {
        let mut entries = Vec::new();
        let mut sequences = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for entry in std::fs::read_dir(self.pending_dir())? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(metadata.is_file(), summon_i18n::format!("r84c6ad5708"));
            anyhow::ensure!(
                metadata.len() <= MAX_RECORD_BYTES,
                summon_i18n::format!("r4b1573510f")
            );
            let (sequence, id) = parse_pending_file_name(&path)?;
            anyhow::ensure!(
                sequences.insert(sequence),
                summon_i18n::format!("ra4ced5ba1a")
            );
            anyhow::ensure!(ids.insert(id.clone()), summon_i18n::format!("r808f864cc3"));
            entries.push(PendingEntry { sequence, id, path });
        }
        entries.sort_by_key(|entry| entry.sequence);
        Ok(entries)
    }

    fn find_pending(&self, id: &str) -> Result<Option<PendingEntry>> {
        Ok(self
            .pending_entries()?
            .into_iter()
            .find(|entry| entry.id == id))
    }

    fn validated_pending(&self) -> Result<Vec<(PendingEntry, CardActionRecord)>> {
        let mut pending = Vec::new();
        for entry in self.pending_entries()? {
            let record = read_record(&entry.path)?;
            let consumed_path = self.consumed_dir().join(format!("{}.json", record.id));
            if consumed_path.exists() {
                let consumed = read_completed(&consumed_path)?;
                anyhow::ensure!(
                    consumed.id == record.id
                        && consumed.sequence == record.sequence
                        && consumed.app_id == record.app_id
                        && consumed.event_id == record.card.event_id,
                    summon_i18n::format!("rbdc336ab7f")
                );
                let _ = std::fs::remove_file(&entry.path);
                continue;
            }
            pending.push((entry, record));
        }
        Ok(pending)
    }

    /// pending 与长期 consumed tombstone 共用一个单调 receive sequence。exact 的
    /// consumed+pending 双份只可能来自“终态已 fsync、plaintext unlink 失败”，允许作为同一事实；
    /// 其余 sequence/id 冲突全部 fail-closed。
    fn sequence_high_watermark(&self) -> Result<u64> {
        let mut by_sequence = BTreeMap::<u64, (String, String, String)>::new();
        let mut by_id = BTreeMap::<String, u64>::new();
        let mut insert = |sequence: u64, id: &str, app_id: &str, event_id: &str| -> Result<()> {
            anyhow::ensure!(sequence > 0, summon_i18n::format!("r5d908d49bd"));
            let identity = (id.to_string(), app_id.to_string(), event_id.to_string());
            if let Some(existing) = by_sequence.get(&sequence) {
                anyhow::ensure!(existing == &identity, summon_i18n::format!("rcf1e31d683"));
            } else {
                by_sequence.insert(sequence, identity);
            }
            if let Some(existing) = by_id.get(id) {
                anyhow::ensure!(*existing == sequence, summon_i18n::format!("r2d1d12adc9"));
            } else {
                by_id.insert(id.to_string(), sequence);
            }
            Ok(())
        };

        for entry in self.pending_entries()? {
            let record = read_record(&entry.path)?;
            insert(
                record.sequence,
                &record.id,
                &record.app_id,
                &record.card.event_id,
            )?;
        }
        for path in json_paths_read_only(&self.consumed_dir())? {
            let completed = read_completed(&path)?;
            insert(
                completed.sequence,
                &completed.id,
                &completed.app_id,
                &completed.event_id,
            )?;
        }
        Ok(by_sequence.keys().next_back().copied().unwrap_or(0))
    }
}

fn wal_lock() -> std::sync::MutexGuard<'static, ()> {
    WAL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stable_id(app_id: &str, event_id: &str) -> String {
    let mut bytes = Vec::with_capacity(app_id.len() + event_id.len() + 1);
    bytes.extend_from_slice(app_id.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(event_id.as_bytes());
    let hash = |seed: u64| {
        bytes.iter().fold(seed, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    format!(
        "card-{:016x}{:016x}",
        hash(0xcbf2_9ce4_8422_2325),
        hash(0x8422_2325_cbf2_9ce4)
    )
}

fn valid_id(id: &str) -> bool {
    id.len() == 37
        && id.starts_with("card-")
        && id[5..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn pending_file_name(sequence: u64, id: &str) -> String {
    format!("{sequence:020}-{id}.json")
}

fn parse_pending_file_name(path: &Path) -> Result<(u64, String)> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context(summon_i18n::text!("r9a890a926c"))?;
    let (raw_sequence, id) = stem
        .split_once('-')
        .context(summon_i18n::text!("r242c26d38e"))?;
    anyhow::ensure!(
        raw_sequence.len() == 20 && raw_sequence.bytes().all(|byte| byte.is_ascii_digit()),
        summon_i18n::format!("r7a6e1b9c22")
    );
    let sequence = raw_sequence.parse::<u64>()?;
    anyhow::ensure!(
        sequence > 0 && valid_id(id),
        summon_i18n::format!("r25bb8945f9")
    );
    Ok((sequence, id.to_string()))
}

fn read_record(path: &Path) -> Result<CardActionRecord> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(metadata.is_file(), summon_i18n::format!("r84c6ad5708"));
    anyhow::ensure!(
        metadata.len() <= MAX_RECORD_BYTES,
        summon_i18n::format!("r4b1573510f")
    );
    let content = std::fs::read(path)?;
    anyhow::ensure!(
        content.len() as u64 <= MAX_RECORD_BYTES,
        summon_i18n::format!("r0df3439e88")
    );
    let record: CardActionRecord = serde_json::from_slice(&content)?;
    let (sequence, id) = parse_pending_file_name(path)?;
    anyhow::ensure!(
        record.schema_version == SCHEMA_VERSION
            && record.sequence == sequence
            && record.id == id
            && record.card.wal_id.as_deref() == Some(record.id.as_str())
            && is_durable_card(&record.card),
        summon_i18n::format!("r6e60f60ab4")
    );
    Ok(record)
}

pub(super) fn is_durable_card(card: &CardActionMsg) -> bool {
    let action = card.value.get("action").and_then(serde_json::Value::as_str);
    action.is_some_and(|action| {
        matches!(
            summon_core::feishu::CardAction::from_action(action),
            summon_core::feishu::CardAction::BridgeFamily
                | summon_core::feishu::CardAction::Fork
                | summon_core::feishu::CardAction::DocumentComment
                | summon_core::feishu::CardAction::RecallRpcHandoff
                | summon_core::feishu::CardAction::Recover
                | summon_core::feishu::CardAction::DismissRecovery
        )
    })
}

fn read_completed(path: &Path) -> Result<CompletedCardAction> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(metadata.is_file(), summon_i18n::format!("r501ec34e5f"));
    anyhow::ensure!(
        metadata.len() <= MAX_RECORD_BYTES,
        summon_i18n::format!("rbed8a8619c")
    );
    let content = std::fs::read(path)?;
    let completed: CompletedCardAction = serde_json::from_slice(&content)?;
    anyhow::ensure!(
        completed.schema_version == SCHEMA_VERSION
            && completed.sequence > 0
            && valid_id(&completed.id)
            && path.file_stem().and_then(|stem| stem.to_str()) == Some(completed.id.as_str()),
        summon_i18n::format!("rbd78af2952")
    );
    Ok(completed)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn pending_usage(directory: &Path) -> Result<(usize, u64)> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(directory)? {
        let metadata = entry?.metadata()?;
        if metadata.is_file() {
            files = files.saturating_add(1);
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok((files, bytes))
}

fn json_paths_read_only(directory: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| summon_i18n::format!("rd6c984a2f9")),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(event_id: &str) -> CardActionMsg {
        CardActionMsg {
            chat_id: "chat".into(),
            message_id: "message".into(),
            operator_open_id: "owner".into(),
            event_id: event_id.into(),
            value: serde_json::json!({"action":"bridge_stop","text":"SECRET_FORM"}),
            wal_id: None,
            wal_replayed: false,
        }
    }

    #[test]
    fn callback_is_durable_idempotent_and_deleted_only_after_ack() {
        let dir = tempfile::tempdir().unwrap();
        let wal = CardActionWal::new(dir.path().join("wal"));
        let PersistOutcome::Pending {
            record: first,
            created,
        } = wal.persist("app", &card("event")).unwrap()
        else {
            panic!("首次必须 pending")
        };
        let PersistOutcome::Pending {
            record: second,
            created: created_again,
        } = wal.persist("app", &card("event")).unwrap()
        else {
            panic!("重复未处理 callback 仍应 pending")
        };
        assert!(created);
        assert!(!created_again);
        assert_eq!(first, second);
        assert_eq!(first.sequence, 1);
        assert_eq!(wal.pending().unwrap(), std::slice::from_ref(first.as_ref()));
        let entry = wal.find_pending(&first.id).unwrap().unwrap();
        let raw = std::fs::read_to_string(entry.path).unwrap();
        assert!(raw.contains("SECRET_FORM"));
        wal.ack(&first.id).unwrap();
        assert!(wal.pending().unwrap().is_empty());
        assert!(matches!(
            wal.persist("app", &card("event")).unwrap(),
            PersistOutcome::Consumed
        ));
        let consumed =
            std::fs::read_to_string(wal.consumed_dir().join(format!("{}.json", first.id))).unwrap();
        assert!(!consumed.contains("SECRET_FORM"));
    }

    #[test]
    fn consumed_callback_identity_does_not_expire_without_protocol_replay_bound() {
        let dir = tempfile::tempdir().unwrap();
        let wal = CardActionWal::new(dir.path().join("wal"));
        let record = match wal.persist("app", &card("event-old")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("首次不能 consumed"),
        };
        wal.ack(&record.id).unwrap();

        let completed_path = wal.consumed_dir().join(format!("{}.json", record.id));
        let mut completed = read_completed(&completed_path).unwrap();
        completed.completed_at_ms = 0;
        atomic_write_mode(
            &completed_path,
            &serde_json::to_vec(&completed).unwrap(),
            0o600,
        )
        .unwrap();

        assert!(matches!(
            wal.persist("app", &card("event-old")).unwrap(),
            PersistOutcome::Consumed
        ));
        assert!(wal.pending().unwrap().is_empty());
    }

    #[test]
    fn fork_callbacks_survive_restart_and_are_consumed_once() {
        let dir = tempfile::tempdir().unwrap();
        let wal = CardActionWal::new(dir.path().join("wal"));
        for action in ["fork_submit", "fork_choose", "fork_execute"] {
            let mut message = card(action);
            message.value = serde_json::json!({"action":action,"ticket":"g0123456789abcdef","form_value":{"fork_name":"Target"}});
            let PersistOutcome::Pending { record, .. } = wal.persist("app", &message).unwrap()
            else {
                panic!()
            };
            let reopened = CardActionWal::new(dir.path().join("wal"));
            assert_eq!(reopened.pending().unwrap()[0].card.value, message.value);
            reopened.ack(&record.id).unwrap();
            assert!(matches!(
                wal.persist("app", &message).unwrap(),
                PersistOutcome::Consumed
            ));
        }
    }

    #[test]
    fn missing_sequence_rebuilds_above_pending_and_consumed_high_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let wal = CardActionWal::new(dir.path().join("wal"));
        let first = match wal.persist("app", &card("event-first")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("首次不能 consumed"),
        };
        let second = match wal.persist("app", &card("event-second")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("首次不能 consumed"),
        };
        wal.ack(&first.id).unwrap();
        std::fs::remove_file(wal.sequence_path()).unwrap();

        let third = match wal.persist("app", &card("event-third")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("新事件不能 consumed"),
        };
        assert_eq!((first.sequence, second.sequence, third.sequence), (1, 2, 3));
    }

    #[test]
    fn sequence_conflict_between_pending_and_consumed_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let wal = CardActionWal::new(dir.path().join("wal"));
        let first = match wal.persist("app", &card("event-first")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("首次不能 consumed"),
        };
        let second = match wal.persist("app", &card("event-second")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("首次不能 consumed"),
        };
        wal.ack(&first.id).unwrap();

        let second_path = wal.find_pending(&second.id).unwrap().unwrap().path;
        let mut conflicting = read_record(&second_path).unwrap();
        conflicting.sequence = first.sequence;
        let conflicting_path = wal
            .pending_dir()
            .join(pending_file_name(first.sequence, &second.id));
        atomic_write_mode(
            &conflicting_path,
            &serde_json::to_vec(&conflicting).unwrap(),
            0o600,
        )
        .unwrap();
        std::fs::remove_file(second_path).unwrap();

        let error = wal.persist("app", &card("event-third")).unwrap_err();
        assert!(format!("{error:#}").contains("sequence 跨目录冲突"));
        assert!(wal.preflight().is_err());
    }

    #[test]
    fn restart_replay_uses_durable_receive_sequence_not_hash_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("wal");
        let wal = CardActionWal::new(&root);
        let first = match wal.persist("app", &card("event-z")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("首次不能 consumed"),
        };
        let second = match wal.persist("app", &card("event-a")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("首次不能 consumed"),
        };
        assert_eq!((first.sequence, second.sequence), (1, 2));
        let replayed = CardActionWal::new(root).pending().unwrap();
        assert_eq!(
            replayed
                .iter()
                .map(|record| record.card.event_id.as_str())
                .collect::<Vec<_>>(),
            ["event-z", "event-a"]
        );
    }

    #[test]
    fn corrupted_sequence_fails_closed_without_acknowledging_new_callback() {
        let dir = tempfile::tempdir().unwrap();
        let wal = CardActionWal::new(dir.path().join("wal"));
        wal.ensure_layout().unwrap();
        atomic_write_mode(&wal.sequence_path(), b"broken", 0o600).unwrap();
        let error = wal.persist("app", &card("event")).unwrap_err();
        assert!(format!("{error:#}").contains("解析 card callback sequence"));
        assert!(
            wal.pending().is_err(),
            "冷启 replay 也必须被坏 sequence 阻断"
        );
        assert!(wal.pending_entries().unwrap().is_empty());
    }

    #[test]
    fn corrupted_earlier_body_blocks_ack_and_dispatch_of_later_callback() {
        let dir = tempfile::tempdir().unwrap();
        let wal = CardActionWal::new(dir.path().join("wal"));
        let first = match wal.persist("app", &card("event-first")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("首次不能 consumed"),
        };
        let path = wal.find_pending(&first.id).unwrap().unwrap().path;
        atomic_write_mode(&path, br#"{"broken":true"#, 0o600).unwrap();
        let error = wal.persist("app", &card("event-later")).unwrap_err();
        assert!(format!("{error:#}").contains("EOF"));
        assert_eq!(wal.pending_entries().unwrap().len(), 1);
    }

    #[test]
    fn non_durable_callbacks_never_enter_replay_wal() {
        let dir = tempfile::tempdir().unwrap();
        let wal = CardActionWal::new(dir.path().join("wal"));
        let mut non_bridge = card("event");
        non_bridge.value = serde_json::json!({"action":"tui_text","text":"repeat me"});
        let error = wal.persist("app", &non_bridge).unwrap_err();
        assert!(format!("{error:#}").contains("不支持 durable WAL"));
    }

    #[test]
    fn health_reports_pending_consumed_identity_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let wal = CardActionWal::new(dir.path().join("wal"));
        let record = match wal.persist("app", &card("event")).unwrap() {
            PersistOutcome::Pending { record, .. } => record,
            PersistOutcome::Consumed => panic!("首次不能 consumed"),
        };
        let conflicting = CompletedCardAction {
            schema_version: SCHEMA_VERSION,
            id: record.id.clone(),
            sequence: record.sequence + 1,
            app_id: record.app_id.clone(),
            event_id: record.card.event_id.clone(),
            completed_at_ms: now_ms(),
        };
        atomic_write_mode(
            &wal.consumed_dir().join(format!("{}.json", record.id)),
            &serde_json::to_vec(&conflicting).unwrap(),
            0o600,
        )
        .unwrap();
        let health = wal.inspect_health().unwrap();
        assert_eq!(health.pending, 1);
        assert_eq!(health.consumed, 1);
        assert_eq!(health.corrupt, 1);
    }
}
