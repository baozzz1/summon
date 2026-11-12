//! Cursor/Codex 无头会话召回的本机发现面。
//!
//! 两类 CLI 都已有稳定的原生 resume id，故恢复不需要也不允许复制 Claude 的 tmux staging /
//! 杀源协议：这里只发现历史会话并证明 source path 没有 writer，真正绑定由 daemon 把
//! `(cli, id, cwd)` 原子落到目标 chat 的既有 resume 契约。

use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use summon_core::recall::{RecallProvider, SessionCandidate};

use crate::observe::external_sessions::{
    cursor_resume_id, discover_terminal_processes, is_rollout, parse_codex_recall_metadata,
    parse_codex_recall_session, CodexPhase, TerminalProcess, TerminalProvider,
};

const CURSOR_WORKER_LOG_PREFIX_BYTES: u64 = 256 * 1024;
const MAX_SCAN_DEPTH: usize = 6;

#[derive(Debug, Clone)]
pub(crate) struct HeadlessRecallCandidate {
    pub(crate) candidate: SessionCandidate,
    /// Cursor=`store.db`，Codex=`rollout-*.jsonl`。只在本机点击时复验 writer，不进卡片。
    pub(crate) source_path: PathBuf,
}

/// 按 agent 的 CLI 发现其真正能 resume 的会话。未知/Claude 不应调用这里。
pub(crate) fn discover(provider: RecallProvider) -> Result<Vec<HeadlessRecallCandidate>> {
    anyhow::ensure!(
        matches!(provider, RecallProvider::Cursor | RecallProvider::Codex),
        summon_i18n::format!("r8abd0a04ec", provider.as_str())
    );
    let home = dirs::home_dir().context(summon_i18n::text!("rde14ef2c2a"))?;
    let processes = discover_terminal_processes();
    let managed = managed_session_ids(provider)?;
    match provider {
        RecallProvider::Cursor => Ok(scan_cursor_at(
            &home.join(".cursor/chats"),
            &home.join(".cursor/projects"),
            &processes,
            &managed,
        )),
        RecallProvider::Codex => {
            let codex_home = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"));
            Ok(scan_codex_at(&codex_home, &processes, &managed))
        }
        RecallProvider::Claude => unreachable!("{}", summon_i18n::format!("r89f7850505")),
    }
}

pub(crate) fn find(provider: RecallProvider, sid: &str) -> Result<Option<HeadlessRecallCandidate>> {
    Ok(discover(provider)?
        .into_iter()
        .find(|entry| entry.candidate.sid == sid))
}

/// 点击时对 selected source path 做独立 lsof 复验。卡片上的 `live_pid=None` 只是展示快照，
/// 绝不能被解释成“现在没有 writer”。
pub(crate) fn ensure_not_open(candidate: &HeadlessRecallCandidate) -> Result<()> {
    anyhow::ensure!(
        !candidate.candidate.is_active(),
        summon_i18n::format!(
            "r5fa5a8e234",
            candidate.candidate.provider.label(),
            candidate.candidate.live_pid.unwrap_or_default()
        )
    );
    anyhow::ensure!(
        !path_has_open_process(&candidate.source_path)?,
        summon_i18n::format!("rf051d7353f", candidate.candidate.provider.label())
    );
    if candidate.candidate.provider == RecallProvider::Codex {
        let parsed = parse_codex_recall_session(&candidate.source_path)
            .with_context(|| summon_i18n::format!("rb5a832194f", candidate.source_path.display()))?
            .context(summon_i18n::text!("rf4b009c675"))?;
        anyhow::ensure!(
            matches!(parsed.phase, CodexPhase::Completed(_) | CodexPhase::Aborted),
            summon_i18n::format!("rfe3e73030e")
        );
    }
    Ok(())
}

fn path_has_open_process(path: &Path) -> Result<bool> {
    let output = std::process::Command::new("lsof")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args(["-Fn", "--"])
        .arg(path)
        .output()
        .with_context(|| summon_i18n::format!("r62ead5f8ce", path.display()))?;
    if output.status.success() {
        return Ok(output.stdout.split(|byte| *byte == b'\n').any(|line| {
            line.strip_prefix(b"p")
                .is_some_and(|pid| !pid.is_empty() && pid.iter().all(u8::is_ascii_digit))
        }));
    }
    // lsof 对“没有任何进程打开该文件”返回 1 且无 stdout；其它失败一律 fail-closed。
    if output.status.code() == Some(1) && output.stdout.is_empty() {
        return Ok(false);
    }
    anyhow::bail!(summon_i18n::format!(
        "r6e1087555e",
        path.display(),
        output.status
    ))
}

/// 全 fleet 的有效 resume 记录都是 summon 正在维护或可恢复的会话，必须从外部召回列表排除。
/// strict registry 失败时整次发现失败：漏排一个坏清单下的 SID 比少展示一张卡危险。
fn managed_session_ids(provider: RecallProvider) -> Result<HashSet<String>> {
    let config = summon_core::config::Config::load();
    let registry = summon_core::registry::Registry::load(&config.agents_dir)
        .context(summon_i18n::text!("r6251c1f738"))?;
    let mut managed = HashSet::new();
    for agent in registry.agents {
        collect_session_records(
            &agent.state_dir().join("runtime").join("chats"),
            "session",
            provider,
            &mut managed,
        )?;
        collect_legacy_session_records(
            &agent.state_dir().join("sessions"),
            provider,
            &mut managed,
        )?;
    }
    Ok(managed)
}

fn collect_session_records(
    dir: &Path,
    extension: &str,
    provider: RecallProvider,
    out: &mut HashSet<String>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| summon_i18n::format!("rb21fe09397")),
    };
    for entry in entries {
        let entry = entry.with_context(|| summon_i18n::format!("r1400487130"))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some(extension) {
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| summon_i18n::format!("rbe9c24be7a"))?;
            anyhow::ensure!(metadata.is_file(), summon_i18n::format!("rbba757f3ea"));
            collect_record_file(&path, provider, out)?;
        }
    }
    Ok(())
}

fn collect_legacy_session_records(
    root: &Path,
    provider: RecallProvider,
    out: &mut HashSet<String>,
) -> Result<()> {
    let tags = match std::fs::read_dir(root) {
        Ok(tags) => tags,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| summon_i18n::format!("r31bd5fa75d")),
    };
    for tag in tags {
        let tag = tag.with_context(|| summon_i18n::format!("rb6f577fe99"))?;
        let tag_path = tag.path();
        let file_type = tag
            .file_type()
            .with_context(|| summon_i18n::format!("ra11a474ddd"))?;
        anyhow::ensure!(file_type.is_dir(), summon_i18n::format!("r542956c871"));
        let path = tag_path.join("session_id");
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| summon_i18n::format!("r46e5d1ebc1")),
        };
        anyhow::ensure!(metadata.is_file(), summon_i18n::format!("rab699be92e"));
        collect_legacy_record_file(&path, provider, out)?;
    }
    Ok(())
}

fn collect_record_file(
    path: &Path,
    provider: RecallProvider,
    out: &mut HashSet<String>,
) -> Result<()> {
    let content =
        std::fs::read_to_string(path).with_context(|| summon_i18n::format!("raaa161585d"))?;
    let record = summon_core::session_id::parse_record(&content)
        .with_context(|| summon_i18n::format!("rd5e396378f"))?;
    if record_matches_provider(&record.cli, provider) {
        out.insert(record.id);
    }
    Ok(())
}

/// `sessions/<tag>/session_id` 在最早一版只存裸 UUID，随后才升级成带 `cli=` 的记录。
/// 裸格式没有 provider 字段，不能猜它属于 Cursor 还是 Codex；对两个外部发现面都排除这个
/// SID 才不会把 summon 曾持有的 owner 重新展示成外部历史。只有历史实现真实写过的标准
/// hyphenated UUID 被接受；其它无 schema 内容仍视为损坏并让整次发现 fail-closed。
fn collect_legacy_record_file(
    path: &Path,
    provider: RecallProvider,
    out: &mut HashSet<String>,
) -> Result<()> {
    let content =
        std::fs::read_to_string(path).with_context(|| summon_i18n::format!("raaa161585d"))?;
    if let Some(record) = summon_core::session_id::parse_record(&content) {
        if record_matches_provider(&record.cli, provider) {
            out.insert(record.id);
        }
        return Ok(());
    }

    let raw = content.trim();
    let legacy_uuid = uuid::Uuid::parse_str(raw)
        .ok()
        .filter(|id| raw.len() == 36 && id.hyphenated().to_string().eq_ignore_ascii_case(raw));
    anyhow::ensure!(legacy_uuid.is_some(), summon_i18n::format!("r10d65134b4"));
    out.insert(raw.to_string());
    Ok(())
}

/// `cursor-acp` 是 resume channel，不是可配置 CLI/provider；只在 managed index 内把它归到
/// Cursor，避免 summon-owned ACP session 被重新展示成“外部 Cursor 会话”。
fn record_matches_provider(cli: &str, provider: RecallProvider) -> bool {
    (cli == "cursor-acp" && provider == RecallProvider::Cursor)
        || RecallProvider::from_cli_kind(cli) == Some(provider)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorMetadata {
    agent_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    created_at: u64,
}

fn scan_cursor_at(
    chats_root: &Path,
    projects_root: &Path,
    processes: &[TerminalProcess],
    managed: &HashSet<String>,
) -> Vec<HeadlessRecallCandidate> {
    let mut workspace_by_hash = cursor_workspace_map(projects_root);
    for process in processes
        .iter()
        .filter(|process| process.provider == TerminalProvider::Cursor)
    {
        if let Some(cwd) = process.cwd.as_deref() {
            workspace_by_hash.insert(cursor_workspace_hash(cwd), cwd.to_string());
        }
    }
    let active = cursor_active_sessions(chats_root, processes);
    let mut candidates = Vec::new();
    let Ok(buckets) = std::fs::read_dir(chats_root) else {
        return candidates;
    };
    for bucket in buckets.flatten() {
        let hash = bucket.file_name().to_string_lossy().into_owned();
        let Some(cwd) = workspace_by_hash
            .get(&hash)
            .filter(|cwd| Path::new(cwd).is_dir())
        else {
            // Cursor 的 resume 查找以 workspace 分桶；cwd 无法还原时不能用当前进程目录碰运气。
            continue;
        };
        let Ok(sessions) = std::fs::read_dir(bucket.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            let db_path = session.path().join("store.db");
            if !db_path.is_file() {
                continue;
            }
            let Some(metadata) = read_cursor_metadata(&db_path) else {
                continue;
            };
            let dir_sid = session.file_name().to_string_lossy().into_owned();
            if metadata.agent_id != dir_sid
                || managed.contains(&metadata.agent_id)
                || metadata.agent_id.trim().is_empty()
            {
                continue;
            }
            let mtime = cursor_last_active_ms(&db_path).unwrap_or(metadata.created_at);
            candidates.push(HeadlessRecallCandidate {
                candidate: SessionCandidate {
                    provider: RecallProvider::Cursor,
                    ownership: summon_core::recall::RecallOwnership::External,
                    sid: metadata.agent_id.clone(),
                    cwd: cwd.clone(),
                    name: metadata.name,
                    prompt_preview: String::new(),
                    last_active_ms: mtime,
                    live_pid: active.get(&metadata.agent_id).copied(),
                    live_status: None,
                    source_observation_id: None,
                    background_tasks: Vec::new(),
                },
                source_path: db_path,
            });
        }
    }
    dedupe_latest(candidates)
}

fn read_cursor_metadata(db_path: &Path) -> Option<CursorMetadata> {
    use rusqlite::{Connection, OpenFlags};
    let connection = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let encoded: String = connection
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })
        .ok()?;
    let decoded = decode_hex(encoded.trim())?;
    serde_json::from_slice(&decoded).ok()
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
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            Some((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn cursor_workspace_map(projects_root: &Path) -> HashMap<String, String> {
    let mut mapped = HashMap::new();
    let Ok(projects) = std::fs::read_dir(projects_root) else {
        return mapped;
    };
    for project in projects.flatten() {
        let path = project.path().join("worker.log");
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        let mut text = String::new();
        if file
            .take(CURSOR_WORKER_LOG_PREFIX_BYTES)
            .read_to_string(&mut text)
            .is_err()
        {
            continue;
        }
        for line in text.lines() {
            let Some(cwd) = line
                .split_once("workspacePath=")
                .map(|(_, value)| value.trim())
                .filter(|value| Path::new(value).is_absolute())
            else {
                continue;
            };
            mapped.insert(cursor_workspace_hash(cwd), cwd.to_string());
            break;
        }
    }
    mapped
}

fn cursor_workspace_hash(cwd: &str) -> String {
    format!("{:x}", md5::compute(cwd.as_bytes()))
}

fn cursor_active_sessions(
    chats_root: &Path,
    processes: &[TerminalProcess],
) -> HashMap<String, u32> {
    let mut active = HashMap::new();
    for process in processes
        .iter()
        .filter(|process| process.provider == TerminalProvider::Cursor)
    {
        if let Some(sid) = cursor_resume_id(&process.command) {
            active.insert(sid, process.pid);
        }
        for path in &process.open_files {
            let Some(sid) = cursor_sid_from_store_path(chats_root, path) else {
                continue;
            };
            active.insert(sid, process.pid);
        }
    }
    active
}

fn cursor_sid_from_store_path(chats_root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(chats_root).ok()?;
    let components = relative.components().collect::<Vec<_>>();
    if components.len() != 3
        || components[2].as_os_str() != "store.db"
        || components[0].as_os_str().is_empty()
    {
        return None;
    }
    components[1]
        .as_os_str()
        .to_str()
        .filter(|sid| !sid.is_empty())
        .map(str::to_string)
}

fn scan_codex_at(
    codex_home: &Path,
    processes: &[TerminalProcess],
    managed: &HashSet<String>,
) -> Vec<HeadlessRecallCandidate> {
    let mut files = Vec::new();
    collect_rollouts(&codex_home.join("sessions"), 0, &mut files);
    collect_rollouts(&codex_home.join("archived_sessions"), 0, &mut files);
    let active = codex_active_sessions(codex_home, processes);
    let mut candidates = Vec::new();
    for path in files {
        let Ok(Some(metadata)) = parse_codex_recall_metadata(&path) else {
            continue;
        };
        let Some(cwd) = metadata
            .cwd
            .filter(|cwd| Path::new(cwd).is_absolute() && Path::new(cwd).is_dir())
        else {
            continue;
        };
        if managed.contains(&metadata.id) {
            continue;
        }
        candidates.push(HeadlessRecallCandidate {
            candidate: SessionCandidate {
                provider: RecallProvider::Codex,
                ownership: summon_core::recall::RecallOwnership::External,
                sid: metadata.id.clone(),
                cwd,
                name: metadata.title,
                prompt_preview: String::new(),
                last_active_ms: modified_ms(&path).unwrap_or_default(),
                live_pid: active.get(&metadata.id).copied(),
                live_status: None,
                source_observation_id: None,
                background_tasks: Vec::new(),
            },
            source_path: path,
        });
    }
    dedupe_latest(candidates)
}

fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_rollouts(&path, depth + 1, out);
        } else if file_type.is_file() && is_rollout(&path) {
            out.push(path);
        }
    }
}

fn codex_active_sessions(codex_home: &Path, processes: &[TerminalProcess]) -> HashMap<String, u32> {
    let mut active = HashMap::new();
    for process in processes
        .iter()
        .filter(|process| process.provider == TerminalProvider::Codex)
    {
        for path in process
            .open_files
            .iter()
            .filter(|path| path.starts_with(codex_home) && is_rollout(path))
        {
            let Ok(Some(metadata)) = parse_codex_recall_metadata(path) else {
                continue;
            };
            active.insert(metadata.id, process.pid);
        }
    }
    active
}

fn dedupe_latest(candidates: Vec<HeadlessRecallCandidate>) -> Vec<HeadlessRecallCandidate> {
    let mut by_sid: HashMap<String, HeadlessRecallCandidate> = HashMap::new();
    for candidate in candidates {
        match by_sid.get(&candidate.candidate.sid) {
            Some(existing)
                if existing.candidate.is_active() && !candidate.candidate.is_active() => {}
            Some(existing)
                if existing.candidate.is_active() == candidate.candidate.is_active()
                    && existing.candidate.last_active_ms >= candidate.candidate.last_active_ms => {}
            _ => {
                by_sid.insert(candidate.candidate.sid.clone(), candidate);
            }
        }
    }
    by_sid.into_values().collect()
}

fn modified_ms(path: &Path) -> Option<u64> {
    path.metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn cursor_last_active_ms(db_path: &Path) -> Option<u64> {
    ["", "-wal", "-shm"]
        .into_iter()
        .filter_map(|suffix| {
            if suffix.is_empty() {
                modified_ms(db_path)
            } else {
                let name = db_path.file_name()?.to_str()?;
                modified_ms(&db_path.with_file_name(format!("{name}{suffix}")))
            }
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::io::Write as _;

    fn write_cursor_db(path: &Path, id: &str, name: &str, created_at: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
            .unwrap();
        let json = serde_json::json!({
            "agentId": id,
            "name": name,
            "createdAt": created_at,
            "blobEncryptionKey": "不得泄露"
        })
        .to_string();
        let encoded = json
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        connection
            .execute("INSERT INTO meta (key, value) VALUES ('0', ?1)", [&encoded])
            .unwrap();
    }

    #[test]
    fn cursor_acp_managed_record_is_excluded_from_external_cursor_scan() {
        let temp = tempfile::tempdir().unwrap();
        let managed_dir = temp.path().join("runtime/chats");
        std::fs::create_dir_all(&managed_dir).unwrap();
        let managed_id = "22222222-2222-4222-8222-222222222222";
        std::fs::write(
            managed_dir.join("soul-test-gmanaged.session"),
            summon_core::session_id::format_record("cursor-acp", managed_id, None, None),
        )
        .unwrap();

        let mut managed = HashSet::new();
        collect_session_records(
            &managed_dir,
            "session",
            RecallProvider::Cursor,
            &mut managed,
        )
        .unwrap();
        assert_eq!(managed, HashSet::from([managed_id.to_string()]));
        assert_eq!(
            RecallProvider::from_cli_kind("cursor-acp"),
            None,
            "resume channel 只能在 managed index 内归一，不能变成可配置 provider"
        );

        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.to_string_lossy().into_owned();
        let chats = temp.path().join("chats");
        let projects = temp.path().join("projects");
        let hash = cursor_workspace_hash(&cwd);
        write_cursor_db(
            &chats.join(&hash).join(managed_id).join("store.db"),
            managed_id,
            "托管 ACP",
            20,
        );
        let project = projects.join("repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("worker.log"),
            format!("[info] workspacePath={cwd}\n"),
        )
        .unwrap();

        assert!(scan_cursor_at(&chats, &projects, &[], &managed).is_empty());
    }

    #[test]
    fn corrupt_managed_record_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let managed_dir = temp.path().join("runtime/chats");
        std::fs::create_dir_all(&managed_dir).unwrap();
        let path = managed_dir.join("soul-test-gbroken.session");
        std::fs::write(&path, "cli=cursor-acp\n").unwrap();

        let error = collect_session_records(
            &managed_dir,
            "session",
            RecallProvider::Cursor,
            &mut HashSet::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("解析 managed session"),
            "损坏记录必须中止整个 discover，而不是静默漏排:{error:#}"
        );
    }

    #[test]
    fn bare_uuid_legacy_record_is_excluded_from_both_headless_providers() {
        let temp = tempfile::tempdir().unwrap();
        let legacy_root = temp.path().join("sessions");
        let legacy_dir = legacy_root.join("gold");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy_id = "22222222-2222-4222-8222-222222222222";
        std::fs::write(legacy_dir.join("session_id"), legacy_id).unwrap();

        for provider in [RecallProvider::Cursor, RecallProvider::Codex] {
            let mut managed = HashSet::new();
            collect_legacy_session_records(&legacy_root, provider, &mut managed).unwrap();
            assert_eq!(managed, HashSet::from([legacy_id.to_string()]));
        }
    }

    #[test]
    fn malformed_legacy_record_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let legacy_root = temp.path().join("sessions");
        let legacy_dir = legacy_root.join("gbroken");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("session_id"), "not-a-session-id").unwrap();

        let error = collect_legacy_session_records(
            &legacy_root,
            RecallProvider::Cursor,
            &mut HashSet::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("既不是结构化记录，也不是旧版裸 UUID"),
            "未知 legacy 内容必须中止整个 discover:{error:#}"
        );
    }

    #[test]
    fn cursor_scan_maps_workspace_filters_managed_and_marks_exact_open_store() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.to_string_lossy().into_owned();
        let chats = temp.path().join("chats");
        let projects = temp.path().join("projects");
        let hash = cursor_workspace_hash(&cwd);
        let external_id = "11111111-1111-4111-8111-111111111111";
        let managed_id = "22222222-2222-4222-8222-222222222222";
        let external_db = chats.join(&hash).join(external_id).join("store.db");
        write_cursor_db(&external_db, external_id, "修 Cursor", 10);
        write_cursor_db(
            &chats.join(&hash).join(managed_id).join("store.db"),
            managed_id,
            "托管",
            20,
        );
        let project = projects.join("repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("worker.log"),
            format!("[info] workspacePath={cwd}\n"),
        )
        .unwrap();
        let processes = vec![TerminalProcess {
            provider: TerminalProvider::Cursor,
            pid: 42,
            started_at: std::time::UNIX_EPOCH,
            command: "agent".into(),
            cwd: Some(cwd.clone()),
            open_files: vec![external_db.clone()],
        }];
        let managed = HashSet::from([managed_id.to_string()]);
        let found = scan_cursor_at(&chats, &projects, &processes, &managed);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].candidate.sid, external_id);
        assert_eq!(found[0].candidate.cwd, cwd);
        assert_eq!(found[0].candidate.name, "修 Cursor");
        assert_eq!(found[0].candidate.live_pid, Some(42));
        assert_eq!(found[0].source_path, external_db);
    }

    fn write_codex_rollout(path: &Path, id: &str, source: &str, thread_source: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(path).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"{}","source":"{source}","originator":"Codex Desktop","thread_source":"{thread_source}"}}}}"#,
            path.parent().unwrap().display()
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"修复 Codex 召回"}}}}"#
        )
        .unwrap();
    }

    #[test]
    fn codex_scan_keeps_top_level_cli_and_desktop_but_excludes_subagents_and_managed() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions/2026/07/31");
        let cli = sessions.join("rollout-cli.jsonl");
        let app = sessions.join("rollout-app.jsonl");
        let sub = sessions.join("rollout-sub.jsonl");
        let managed_path = sessions.join("rollout-managed.jsonl");
        write_codex_rollout(&cli, "cli-id", "cli", "user");
        write_codex_rollout(&app, "app-id", "vscode", "user");
        write_codex_rollout(&sub, "sub-id", "cli", "subagent");
        write_codex_rollout(&managed_path, "managed-id", "exec", "user");
        let managed = HashSet::from(["managed-id".to_string()]);
        let found = scan_codex_at(temp.path(), &[], &managed);
        let ids = found
            .iter()
            .map(|candidate| candidate.candidate.sid.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(ids, HashSet::from(["cli-id", "app-id"]));
        assert!(found
            .iter()
            .all(|candidate| candidate.candidate.provider == RecallProvider::Codex));
    }

    #[test]
    fn malformed_cursor_metadata_never_leaks_or_becomes_candidate() {
        assert!(decode_hex("xyz").is_none());
        assert!(decode_hex("0").is_none());
    }

    #[test]
    fn headless_recall_has_no_source_termination_primitive() {
        let source = include_str!("recall_headless.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in ["nix::sys::signal", "killpg", "SIGTERM", "SIGKILL"] {
            assert!(
                !production.contains(forbidden),
                "Cursor/Codex 召回发现层不得拥有 source termination primitive:{forbidden}"
            );
        }
    }

    #[test]
    #[ignore = "读取本机 Cursor/Codex 真会话存储，只在对应召回链路验收时显式运行"]
    fn discovers_real_headless_sessions_without_cross_provider_candidates() {
        for provider in [RecallProvider::Cursor, RecallProvider::Codex] {
            let started = std::time::Instant::now();
            let found = discover(provider).expect("本机发现不应失败");
            let discovery_elapsed = started.elapsed();
            assert!(
                !found.is_empty(),
                "本机应至少有一个 {} 会话",
                provider.label()
            );
            assert!(found.iter().all(|entry| {
                entry.candidate.provider == provider
                    && std::path::Path::new(&entry.candidate.cwd).is_dir()
            }));
            let proof_started = std::time::Instant::now();
            assert!(
                found
                    .iter()
                    .filter(|entry| !entry.candidate.is_active())
                    .any(|entry| ensure_not_open(entry).is_ok()),
                "本机至少应有一个通过 selected-path writer 复验的 {} 历史会话",
                provider.label()
            );
            eprintln!(
                "{} recall: {} candidates discovered in {:?}, selected proof in {:?}",
                provider.label(),
                found.len(),
                discovery_elapsed,
                proof_started.elapsed()
            );
        }
    }
}
