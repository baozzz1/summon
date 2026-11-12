//! 只读 Cursor CLI 自己的会话库，不启动第二个 provider，也不消费计费用量。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension as _};
use serde::Deserialize;
use summon_core::context_budget::ContextBudget;

const MAX_ROOT_BYTES: u32 = 4 * 1024 * 1024;
const MAX_METADATA_HEX_BYTES: u32 = 128 * 1024;
const MAX_BUCKETS: usize = 4096;

#[derive(Clone)]
enum StorageRoot {
    CliChats(Option<PathBuf>),
    AcpSessions(Option<PathBuf>),
}

#[derive(Clone)]
struct Source {
    session_id: String,
    storage: StorageRoot,
}

type Sources = HashMap<(String, String), Source>;

fn sources() -> &'static Mutex<Sources> {
    static SOURCES: OnceLock<Mutex<Sources>> = OnceLock::new();
    SOURCES.get_or_init(Mutex::default)
}

pub(super) fn env_value(env: &[(String, String)], name: &str) -> Option<String> {
    env.iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var(name).ok())
}

pub(super) fn config_root(env: &[(String, String)]) -> Option<PathBuf> {
    let root = if let Some(config) = env_value(env, "CURSOR_CONFIG_DIR") {
        PathBuf::from(config)
    } else if cfg!(target_os = "linux") {
        if let Some(xdg) = env_value(env, "XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("cursor")
        } else {
            PathBuf::from(env_value(env, "HOME")?).join(".cursor")
        }
    } else {
        PathBuf::from(env_value(env, "HOME")?).join(".cursor")
    };
    // 相对路径受启动 cwd 影响，不能在 daemon 的 cwd 下擅自解释。
    root.is_absolute().then_some(root)
}

fn chats_root(env: &[(String, String)]) -> Option<PathBuf> {
    config_root(env).map(|root| root.join("chats"))
}

/// 由真正启动 CLI 的环境绑定来源；下一条会话替换旧绑定，不按模型名猜窗口。
pub(crate) fn register_session(env: &[(String, String)], session_id: &str) {
    let agent = env.iter().find(|(key, _)| key == "SUMMON_AGENT");
    let tag = env.iter().find(|(key, _)| key == "SUMMON_CONVERSATION_TAG");
    let (Some((_, agent)), Some((_, tag))) = (agent, tag) else {
        return;
    };
    if let Ok(mut sources) = sources().lock() {
        sources.insert(
            (agent.clone(), tag.clone()),
            Source {
                session_id: session_id.into(),
                storage: StorageRoot::CliChats(chats_root(env)),
            },
        );
    }
}

/// 绑定 ACP 会话的配置根；Cursor ACP 会话库位于 `<config_dir>/acp-sessions/<sid>/store.db`。
pub(crate) fn register_acp_session(agent: &str, tag: &str, config_dir: &Path, session_id: &str) {
    let sessions_root = config_dir
        .is_absolute()
        .then(|| config_dir.join("acp-sessions"));
    if let Ok(mut sources) = sources().lock() {
        sources.insert(
            (agent.into(), tag.into()),
            Source {
                session_id: session_id.into(),
                storage: StorageRoot::AcpSessions(sessions_root),
            },
        );
    }
}

/// CLI 会话缺本地快照时返回 `Some(空预算)`，禁止回落到旧 ACP 池里的占用；
/// ACP 会话缺本地快照时返回 `None`，保留同会话經 `usage_update` 推入池的预算。
pub(crate) fn registered_budget(agent: &str, tag: &str, session_id: &str) -> Option<ContextBudget> {
    let source = sources()
        .lock()
        .ok()?
        .get(&(agent.into(), tag.into()))
        .filter(|source| source.session_id == session_id)
        .cloned()?;
    match source.storage {
        StorageRoot::CliChats(root) => Some(
            root.and_then(|root| read_at(&root, session_id))
                .unwrap_or_default(),
        ),
        StorageRoot::AcpSessions(root) => root.and_then(|root| read_acp_at(&root, session_id)),
    }
}

pub(crate) fn budget_for_env(env: &[(String, String)], session_id: &str) -> ContextBudget {
    chats_root(env)
        .and_then(|root| read_at(&root, session_id))
        .unwrap_or_default()
}

pub(crate) fn acp_budget_at(acp_sessions_root: &Path, session_id: &str) -> ContextBudget {
    read_acp_at(acp_sessions_root, session_id).unwrap_or_default()
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

fn read_acp_at(acp_sessions_root: &Path, session_id: &str) -> Option<ContextBudget> {
    if !valid_session_id(session_id) {
        return None;
    }
    let path = acp_sessions_root.join(session_id).join("store.db");
    path.is_file().then(|| read_database(&path, session_id))?
}

fn read_at(chats_root: &Path, session_id: &str) -> Option<ContextBudget> {
    if !valid_session_id(session_id) {
        return None;
    }
    // cwd 经 /cd 或 symlink 规范化后会换桶；只找 exact SID，多个命中则不猜。
    let mut selected = None;
    for (index, bucket) in std::fs::read_dir(chats_root).ok()?.enumerate() {
        if index >= MAX_BUCKETS {
            return None;
        }
        let path = bucket.ok()?.path().join(session_id).join("store.db");
        if path.is_file() && selected.replace(path).is_some() {
            return None;
        }
    }
    read_database(&selected?, session_id)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    agent_id: String,
    latest_root_blob_id: String,
}

fn read_database(path: &Path, session_id: &str) -> Option<ContextBudget> {
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    connection.busy_timeout(Duration::from_millis(25)).ok()?;
    // meta 指针和 blob 必须属于同一 SQLite 快照，CLI 正在写 WAL 时也不能拼两代数据。
    let transaction = connection.transaction().ok()?;
    let encoded: String = transaction
        .query_row(
            "SELECT value FROM meta WHERE key = '0' AND length(value) <= ?1",
            [MAX_METADATA_HEX_BYTES],
            |row| row.get(0),
        )
        .optional()
        .ok()??;
    let metadata: Metadata = serde_json::from_slice(&decode_hex(&encoded)?).ok()?;
    if metadata.agent_id != session_id
        || metadata.latest_root_blob_id.len() != 64
        || !metadata
            .latest_root_blob_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let blob: Vec<u8> = transaction
        .query_row(
            "SELECT data FROM blobs WHERE id = ?1 AND length(data) <= ?2",
            rusqlite::params![metadata.latest_root_blob_id, MAX_ROOT_BYTES],
            |row| row.get(0),
        )
        .optional()
        .ok()??;
    summon_core::parse::cursor_context::parse_context(&blob)
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn write_test_snapshot(root: &Path, sid: &str, used: u64, window: u64) -> PathBuf {
    write_test_snapshot_at(&root.join("workspace-hash").join(sid), sid, used, window)
}

#[cfg(test)]
pub(crate) fn write_test_acp_snapshot(root: &Path, sid: &str, used: u64, window: u64) -> PathBuf {
    write_test_snapshot_at(&root.join(sid), sid, used, window)
}

#[cfg(test)]
fn write_test_snapshot_at(directory: &Path, sid: &str, used: u64, window: u64) -> PathBuf {
    fn varint(mut value: u64, into: &mut Vec<u8>) {
        while value >= 128 {
            into.push((value as u8 & 127) | 128);
            value >>= 7;
        }
        into.push(value as u8);
    }
    std::fs::create_dir_all(directory).unwrap();
    let path = directory.join("store.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT); CREATE TABLE IF NOT EXISTS blobs (id TEXT PRIMARY KEY, data BLOB);")
        .unwrap();
    let blob_id = "a".repeat(64);
    let metadata = serde_json::json!({"agentId": sid, "latestRootBlobId": blob_id}).to_string();
    let encoded = metadata
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    connection
        .execute("INSERT OR REPLACE INTO meta VALUES ('0', ?1)", [encoded])
        .unwrap();
    let mut details = vec![8];
    varint(used, &mut details);
    details.push(16);
    varint(window, &mut details);
    let mut blob = vec![42];
    varint(details.len() as u64, &mut blob);
    blob.extend(details);
    connection
        .execute(
            "INSERT OR REPLACE INTO blobs VALUES (?1, ?2)",
            rusqlite::params![blob_id, blob],
        )
        .unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_exact_session_and_refreshes_after_compaction() {
        let tmp = tempfile::tempdir().unwrap();
        write_test_snapshot(tmp.path(), "sid-a", 850_000, 1_000_000);
        write_test_snapshot(tmp.path(), "sid-b", 3, 300_000);
        assert_eq!(
            read_at(tmp.path(), "sid-a"),
            Some(ContextBudget::from_tokens(850_000, Some(1_000_000)))
        );
        write_test_snapshot(tmp.path(), "sid-a", 24_854, 1_000_000);
        assert_eq!(
            read_at(tmp.path(), "sid-a"),
            Some(ContextBudget::from_tokens(24_854, Some(1_000_000)))
        );
    }

    #[test]
    fn rejects_wrong_identity_missing_blob_and_duplicate_buckets() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_test_snapshot(tmp.path(), "sid-a", 1, 5);
        assert_eq!(read_database(&path, "sid-b"), None);
        let duplicate = tmp.path().join("other-bucket/sid-a");
        std::fs::create_dir_all(&duplicate).unwrap();
        std::fs::copy(&path, duplicate.join("store.db")).unwrap();
        assert_eq!(read_at(tmp.path(), "sid-a"), None);
        let connection = Connection::open(&path).unwrap();
        connection.execute("DELETE FROM blobs", []).unwrap();
        assert_eq!(read_database(&path, "sid-a"), None);
    }

    #[test]
    fn never_creates_a_missing_database_or_accepts_unsafe_session_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.db");
        assert_eq!(read_database(&missing, "sid"), None);
        assert!(!missing.exists());
        for sid in ["", "../sid", "sid/a", ".", "sid\n"] {
            assert_eq!(read_at(tmp.path(), sid), None);
        }
    }

    #[test]
    fn binds_actual_config_root_and_refuses_other_session_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("config");
        let db = write_test_snapshot(&root.join("chats"), "sid-a", 24_854, 1_000_000);
        let env = vec![
            ("CURSOR_CONFIG_DIR".into(), root.display().to_string()),
            ("SUMMON_AGENT".into(), "cursor-context-binding-test".into()),
            ("SUMMON_CONVERSATION_TAG".into(), "g-test".into()),
        ];
        register_session(&env, "sid-a");
        assert_eq!(
            registered_budget("cursor-context-binding-test", "g-test", "sid-a"),
            Some(ContextBudget::from_tokens(24_854, Some(1_000_000)))
        );
        assert_eq!(
            registered_budget("cursor-context-binding-test", "g-test", "sid-b"),
            None
        );
        std::fs::remove_file(db).unwrap();
        assert_eq!(
            registered_budget("cursor-context-binding-test", "g-test", "sid-a"),
            Some(ContextBudget::default())
        );
    }

    #[test]
    fn reads_registered_and_on_disk_acp_session_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("acp-config");
        let acp_sessions = config.join("acp-sessions");
        let db = write_test_acp_snapshot(&acp_sessions, "sid-acp", 42_000, 300_000);
        register_acp_session("cursor-acp-binding-test", "g-acp", &config, "sid-acp");
        assert_eq!(
            registered_budget("cursor-acp-binding-test", "g-acp", "sid-acp"),
            Some(ContextBudget::from_tokens(42_000, Some(300_000)))
        );
        assert_eq!(
            registered_budget("cursor-acp-binding-test", "g-acp", "sid-other"),
            None
        );
        assert_eq!(
            acp_budget_at(&acp_sessions, "sid-acp"),
            ContextBudget::from_tokens(42_000, Some(300_000))
        );
        std::fs::remove_file(db).unwrap();
        assert_eq!(
            registered_budget("cursor-acp-binding-test", "g-acp", "sid-acp"),
            None
        );
        assert_eq!(
            acp_budget_at(&acp_sessions, "sid-acp"),
            ContextBudget::default()
        );
    }

    #[test]
    fn rejects_relative_config_roots_and_oversized_blobs() {
        assert_eq!(
            chats_root(&[("CURSOR_CONFIG_DIR".into(), "relative".into())]),
            None
        );
        let tmp = tempfile::tempdir().unwrap();
        let path = write_test_snapshot(tmp.path(), "sid", 2, 5);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("UPDATE blobs SET data = zeroblob(?1)", [MAX_ROOT_BYTES + 1])
            .unwrap();
        assert_eq!(read_database(&path, "sid"), None);
    }
}
