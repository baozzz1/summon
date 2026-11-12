//! 停用 agent 的可验证备份。只按已知存储归属选文件，不跟随符号链接。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use summon_core::registry::Agent;
use summon_core::Schedule;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    path: PathBuf,
    kind: String,
    size: u64,
    mode: u32,
    modified: (i64, i64),
    inode: u64,
    device: u64,
    digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Plan {
    pub(crate) agent: String,
    pub(crate) roots: Vec<PathBuf>,
    entries: Vec<Entry>,
    pub(crate) schedules: Vec<Schedule>,
}

pub(crate) struct Backup {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) plan: Plan,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}
impl Backup {
    pub(crate) fn path(&self) -> PathBuf {
        self.dir.path().join("backup.tar.gz")
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn hash_reader(mut reader: impl Read) -> Result<String> {
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn hash_file(path: &Path) -> Result<String> {
    hash_reader(File::open(path)?)
}
fn safe_ancestors(home: &Path, relative: &Path) -> Result<()> {
    let mut path = home.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for c in components.iter().take(components.len().saturating_sub(1)) {
        ensure!(
            matches!(c, std::path::Component::Normal(_)),
            summon_i18n::format!("r0182415a5a")
        );
        path.push(c);
        match fs::symlink_metadata(&path) {
            Ok(m) => ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                summon_i18n::format!("r05cc45dfe4", path.display())
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn entries(home: &Path, relative: &Path, out: &mut Vec<Entry>) -> Result<()> {
    safe_ancestors(home, relative)?;
    let path = home.join(relative);
    let m = fs::symlink_metadata(&path)?;
    let (kind, digest) = if m.file_type().is_symlink() {
        (
            "symlink",
            fs::read_link(&path)?.to_string_lossy().into_owned(),
        )
    } else if m.is_dir() {
        ("directory", String::new())
    } else {
        ensure!(
            m.is_file(),
            summon_i18n::format!("r557ccd4c28", relative.display())
        );
        ("file", hash_file(&path)?)
    };
    out.push(Entry {
        path: relative.to_path_buf(),
        kind: kind.into(),
        size: if m.is_file() { m.len() } else { 0 },
        mode: m.mode(),
        modified: (m.mtime(), m.mtime_nsec()),
        inode: m.ino(),
        device: m.dev(),
        digest,
    });
    if m.is_dir() {
        let mut children = fs::read_dir(path)?
            .map(|e| e.map(|e| relative.join(e.file_name())))
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort();
        for child in children {
            entries(home, &child, out)?;
        }
    }
    Ok(())
}
fn add(home: &Path, path: PathBuf, roots: &mut BTreeSet<PathBuf>) -> Result<()> {
    safe_ancestors(home, &path)?;
    match fs::symlink_metadata(home.join(&path)) {
        Ok(_) => {
            roots.insert(path);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn owns(value: &Value, agent: &str, scope: &str, app: &str) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    for key in ["agent", "agent_name"] {
        if let Some(owner) = map
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return owner == agent;
        }
    }
    if let Some(owner) = map
        .get("delivery_scope")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return owner == scope;
    }
    !app.is_empty()
        && (map.get("app_id").and_then(Value::as_str) == Some(app)
            || value.pointer("/notice/app_id").and_then(Value::as_str) == Some(app))
}
fn scan_records(
    home: &Path,
    relative: &Path,
    agent: &str,
    scope: &str,
    app: &str,
    roots: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let path = home.join(relative);
    if !path.exists() {
        return Ok(());
    }
    safe_ancestors(home, &relative.join("child"))?;
    for item in fs::read_dir(path)? {
        let item = item?;
        let rel = relative.join(item.file_name());
        let ty = item.file_type()?;
        if ty.is_dir() {
            scan_records(home, &rel, agent, scope, app, roots)?;
        } else if ty.is_file() && rel.extension().is_some_and(|s| s == "json") {
            let value: Value = serde_json::from_slice(&fs::read(item.path())?)
                .with_context(|| summon_i18n::format!("r04268c84b2", rel.display()))?;
            if owns(&value, agent, scope, app) {
                ensure!(
                    !relative.starts_with("run/sidecar-merge-outbox"),
                    summon_i18n::format!("r65b84906a0")
                );
                let live_queue = rel
                    .components()
                    .any(|c| c.as_os_str() == "pending" || c.as_os_str() == "active");
                ensure!(
                    !live_queue,
                    summon_i18n::format!("r4355d99d6b", rel.display())
                );
                if relative.starts_with("run/document-comments") {
                    if let Some(process) = value.get("draft_process").filter(|p| !p.is_null()) {
                        let process: crate::runtime::document_comments::DraftProcessIdentity =
                            serde_json::from_value(process.clone())?;
                        ensure!(
                            process.live_pid(Some(std::process::id())).is_none(),
                            summon_i18n::format!("rd0632aebd2")
                        );
                    }
                    ensure!(
                        matches!(
                            value.get("stage").and_then(Value::as_str),
                            Some("replied" | "closed")
                        ),
                        summon_i18n::format!("rcc991f9062")
                    );
                }
                roots.insert(rel);
            }
        }
    }
    Ok(())
}

pub(crate) fn plan(home: &Path, agent: &Agent) -> Result<Plan> {
    let schedules = crate::schedule::schedule_store::ScheduleStore::new(home)
        .load()?
        .into_iter()
        .filter(|s| s.agent == agent.name)
        .collect();
    plan_with_schedules(home, agent, schedules)
}

pub(crate) fn plan_with_schedules(
    home: &Path,
    agent: &Agent,
    schedules: Vec<Schedule>,
) -> Result<Plan> {
    ensure!(
        !agent.name.is_empty()
            && !agent.name.contains(['/', '\\'])
            && agent.name != "."
            && agent.name != "..",
        summon_i18n::format!("rce61ae4be1")
    );
    ensure!(
        agent.dir == home.join("agents").join(&agent.name),
        summon_i18n::format!("r13377b5b0c")
    );
    let m = fs::symlink_metadata(&agent.dir)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink(),
        summon_i18n::format!("rf49f7ad510")
    );
    let mut roots = BTreeSet::new();
    for base in ["agents", "state", "run/turns", "run/usage"] {
        add(home, Path::new(base).join(&agent.name), &mut roots)?;
    }
    let scope = crate::runtime::daemon::feishu_delivery_scope(agent);
    let app = crate::runtime::daemon::resolve_creds(agent)
        .0
        .unwrap_or_default();
    if !app.is_empty() {
        for entry in fs::read_dir(home.join("agents"))? {
            let entry = entry?;
            if entry.path() == agent.dir {
                continue;
            }
            if let Ok(text) = fs::read_to_string(entry.path().join("agent.yaml")) {
                if let Ok(mut other) = serde_yaml::from_str::<Agent>(&text) {
                    other.dir = entry.path();
                    ensure!(
                        crate::runtime::daemon::resolve_creds(&other).0.as_deref()
                            != Some(app.as_str()),
                        summon_i18n::format!("r3aa936e639")
                    );
                }
            }
        }
    }
    for path in [
        format!("run/identity-cache/{scope}.json"),
        format!("run/peers/{scope}"),
        format!("run/inbox/availability/{}.json", agent.name),
    ] {
        add(home, PathBuf::from(path), &mut roots)?;
    }
    let bucket = format!("agent-{}", sha256(agent.name.as_bytes()));
    for kind in ["pending", "consumed", "recalled"] {
        let rel = Path::new("run/inbox").join(kind).join(&bucket);
        if kind == "pending" && home.join(&rel).exists() {
            ensure!(
                fs::read_dir(home.join(&rel))?.next().is_none(),
                summon_i18n::format!("r66ca43c42e")
            );
        }
        add(home, rel, &mut roots)?;
    }
    for base in [
        "run/inbox",
        "run/outbox",
        "run/card-inbox",
        "run/document-comments",
        "run/sidecar-merge-outbox",
    ] {
        scan_records(home, Path::new(base), &agent.name, &scope, &app, &mut roots)?;
    }
    // Bridge 关联持有外部会话的身份与租约，必须通过其自身的解绑入口结束。
    let bridge = home.join("run/external-bridge/state.json");
    if bridge.exists() {
        let value: Value = serde_json::from_slice(&fs::read(&bridge)?)?;
        fn referenced(v: &Value, alias: &str) -> bool {
            match v {
                Value::Object(m) => {
                    m.get("agent").and_then(Value::as_str) == Some(alias)
                        || m.values().any(|v| referenced(v, alias))
                }
                Value::Array(a) => a.iter().any(|v| referenced(v, alias)),
                _ => false,
            }
        }
        ensure!(
            !referenced(&value, &agent.name),
            summon_i18n::format!("rac16ee01b6")
        );
    }
    let all: Vec<_> = roots.iter().cloned().collect();
    roots.retain(|r| {
        !all.iter()
            .any(|parent| parent != r && r.starts_with(parent))
    });
    let mut files = Vec::new();
    for root in &roots {
        entries(home, root, &mut files)?;
    }
    ensure!(
        schedules.iter().all(|s| !s.enabled),
        summon_i18n::format!("rbe9e3d231a")
    );
    Ok(Plan {
        agent: agent.name.clone(),
        roots: roots.into_iter().collect(),
        entries: files,
        schedules,
    })
}

pub(crate) fn backup(home: &Path, agent: &Agent) -> Result<Backup> {
    let plan = plan(home, agent)?;
    let dir = tempfile::Builder::new()
        .prefix("summon-agent-backup-")
        .tempdir()?;
    let path = dir.path().join("backup.tar.gz");
    let file = File::create(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gzip);
    tar.follow_symlinks(false);
    for e in &plan.entries {
        tar.append_path_with_name(home.join(&e.path), Path::new("summon").join(&e.path))?;
    }
    let manifest = serde_json::to_vec_pretty(&plan)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, "manifest.json", manifest.as_slice())?;
    let schedules = serde_yaml::to_string(&plan.schedules)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(schedules.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(
        &mut header,
        "summon/schedules-agent.yaml",
        schedules.as_bytes(),
    )?;
    let file = tar.into_inner()?.finish()?;
    file.sync_all()?;
    ensure!(
        plan == self::plan(home, agent)?,
        summon_i18n::format!("r92481635f2")
    );
    // 逐个读回 tar 内容并验证摘要，不能把 gzip 能结束等同于备份完整。
    let mut seen = BTreeMap::new();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(File::open(&path)?));
    for item in archive.entries()? {
        let mut item = item?;
        let path = item.path()?.to_path_buf();
        if let Ok(rel) = path.strip_prefix("summon") {
            if item.header().entry_type().is_file() {
                seen.insert(rel.to_path_buf(), hash_reader(&mut item)?);
            }
        }
    }
    for entry in &plan.entries {
        if entry.kind == "file" {
            ensure!(
                seen.get(&entry.path) == Some(&entry.digest),
                summon_i18n::format!("r2a0dc3935e")
            );
        }
    }
    Ok(Backup {
        bytes: fs::metadata(&path)?.len(),
        sha256: hash_file(&path)?,
        dir,
        plan,
    })
}

/// 文件先移入隔离目录。成功前的错误会原路退回；回滚遇到同名新文件时保留隔离原件。
pub(crate) fn clean_files(
    home: &Path,
    expected: &Plan,
    commit_shared: impl FnOnce() -> Result<()>,
) -> Result<Option<String>> {
    let home_meta = fs::symlink_metadata(home)?;
    ensure!(
        home_meta.is_dir() && !home_meta.file_type().is_symlink(),
        summon_i18n::format!("r78062feeaa")
    );
    let stage_parent = Path::new("run/agent-cleanup");
    safe_ancestors(home, &stage_parent.join("entry"))?;
    fs::create_dir_all(home.join(stage_parent))?;
    safe_ancestors(home, &stage_parent.join("entry"))?;
    let stage = home
        .join("run/agent-cleanup")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&stage)?;
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o700))?;
    fs::write(
        stage.join("manifest.json"),
        serde_json::to_vec_pretty(expected)?,
    )?;
    let mut moved: Vec<PathBuf> = Vec::new();
    let result = (|| {
        for rel in &expected.roots {
            safe_ancestors(home, rel)?;
            safe_ancestors(home, &stage.strip_prefix(home)?.join("entry"))?;
            let dst = stage.join("files").join(rel);
            fs::create_dir_all(dst.parent().context(summon_i18n::text!("ra0fbc6a08f"))?)?;
            fs::rename(home.join(rel), dst)?;
            moved.push(rel.clone());
        }
        let mut observed = Vec::new();
        for rel in &expected.roots {
            entries(&stage.join("files"), rel, &mut observed)?;
        }
        ensure!(
            observed == expected.entries,
            summon_i18n::format!("r82b07bdc31")
        );
        commit_shared()?;
        Ok(())
    })();
    if let Err(error) = result {
        for rel in moved.iter().rev() {
            safe_ancestors(home, &stage.strip_prefix(home)?.join("entry"))?;
            safe_ancestors(home, rel)?;
            let original = home.join(rel);
            ensure!(
                fs::symlink_metadata(&original).is_err(),
                summon_i18n::format!("rcc85babf18", stage.display())
            );
            fs::create_dir_all(
                original
                    .parent()
                    .context(summon_i18n::text!("r6ba86b117d"))?,
            )?;
            fs::rename(stage.join("files").join(rel), original)?;
        }
        fs::remove_dir_all(&stage)?;
        return Err(error);
    }
    let cleanup =
        fs::write(stage.join("committed"), b"complete\n").and_then(|()| fs::remove_dir_all(&stage));
    Ok(cleanup
        .err()
        .map(|e| summon_i18n::format!("rfc49394acb", stage.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn fixture() -> (tempfile::TempDir, Agent) {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("agents/sample");
        fs::create_dir_all(&dir).unwrap();
        let yaml = "name: sample\n";
        fs::write(dir.join("agent.yaml"), yaml).unwrap();
        fs::write(dir.join(".env"), b"EXAMPLE_TOKEN=test\n").unwrap();
        let mut agent: Agent = serde_yaml::from_str(yaml).unwrap();
        agent.dir = dir;
        (home, agent)
    }

    fn put(home: &Path, relative: &str, bytes: &[u8]) {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn cleanup_rejects_external_staging_ancestors_without_moving_data() {
        for linked in ["run", "run/agent-cleanup"] {
            let (home, agent) = fixture();
            let outside = tempfile::tempdir().unwrap();
            let backup = backup(home.path(), &agent).unwrap();
            let path = home.path().join(linked);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(outside.path(), &path).unwrap();
            let committed = std::cell::Cell::new(false);
            assert!(clean_files(home.path(), &backup.plan, || {
                committed.set(true);
                Ok(())
            })
            .is_err());
            assert!(!committed.get());
            assert_eq!(
                fs::read(agent.dir.join(".env")).unwrap(),
                b"EXAMPLE_TOKEN=test\n"
            );
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
            assert_eq!(fs::read_link(path).unwrap(), outside.path());
        }
    }

    #[test]
    fn backup_and_cleanup_preserve_external_links_and_other_agents() {
        let (home, agent) = fixture();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), agent.dir.join("workspace")).unwrap();
        put(home.path(), "agents/other/agent.yaml", b"name: other\n");
        put(home.path(), "state/sample/runtime/state", b"session");
        put(home.path(), "run/turns/sample/2026-11.jsonl", b"turn\n");
        put(
            home.path(),
            "run/outbox/archive/a.json",
            br#"{"agent":"sample"}"#,
        );
        put(
            home.path(),
            "run/outbox/archive/b.json",
            br#"{"agent":"other"}"#,
        );
        let b = backup(home.path(), &agent).unwrap();
        let mut archive =
            tar::Archive::new(flate2::read::GzDecoder::new(File::open(b.path()).unwrap()));
        let mut members = BTreeMap::new();
        for item in archive.entries().unwrap() {
            let mut item = item.unwrap();
            let path = item.path().unwrap().to_path_buf();
            let mut bytes = Vec::new();
            item.read_to_end(&mut bytes).unwrap();
            members.insert(path, (item.header().entry_type(), bytes));
        }
        assert_eq!(
            members[Path::new("summon/agents/sample/.env")].1,
            b"EXAMPLE_TOKEN=test\n"
        );
        assert!(members[Path::new("summon/agents/sample/workspace")]
            .0
            .is_symlink());
        assert!(!members.contains_key(Path::new("summon/agents/sample/workspace/keep")));
        assert!(members.contains_key(Path::new("summon/run/outbox/archive/a.json")));
        assert!(!members.contains_key(Path::new("summon/run/outbox/archive/b.json")));
        clean_files(home.path(), &b.plan, || Ok(())).unwrap();
        assert!(!agent.dir.exists());
        assert!(!home.path().join("state/sample").exists());
        assert!(!home.path().join("run/outbox/archive/a.json").exists());
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"outside");
        assert!(home.path().join("agents/other/agent.yaml").is_file());
        assert!(home.path().join("run/outbox/archive/b.json").is_file());
    }

    #[test]
    fn cleanup_rolls_back_changed_data_and_failed_shared_commit() {
        let (home, agent) = fixture();
        let b = backup(home.path(), &agent).unwrap();
        fs::write(agent.dir.join("new"), b"new data").unwrap();
        assert!(clean_files(home.path(), &b.plan, || panic!(
            "must not commit changed data"
        ))
        .is_err());
        assert_eq!(fs::read(agent.dir.join("new")).unwrap(), b"new data");
        let b = backup(home.path(), &agent).unwrap();
        assert!(clean_files(home.path(), &b.plan, || anyhow::bail!(
            "injected storage error"
        ))
        .is_err());
        assert!(agent.dir.join(".env").exists());
        assert_eq!(fs::read(agent.dir.join("new")).unwrap(), b"new data");
    }

    #[test]
    fn backup_rejects_pending_work_and_symlinked_storage_parent() {
        let (home, agent) = fixture();
        put(
            home.path(),
            "run/outbox/pending/a.json",
            br#"{"agent":"sample"}"#,
        );
        assert!(format!("{:#}", backup(home.path(), &agent).err().unwrap()).contains("未完成"));
        fs::remove_file(home.path().join("run/outbox/pending/a.json")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), home.path().join("state")).unwrap();
        assert!(backup(home.path(), &agent).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
