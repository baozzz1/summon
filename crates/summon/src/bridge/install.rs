use crate::bridge::HOOK_PROTOCOL;
use crate::runtime::fsio::atomic_write_mode;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const HOOK_EVENTS: [&str; 6] = [
    "SessionStart",
    "CwdChanged",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStatus {
    Absent,
    Partial {
        installed: usize,
    },
    Installed,
    Stale {
        configured: usize,
        current: usize,
    },
    Disabled,
    /// settings 可读但 JSON/schema 损坏；status 只诊断，install 仍拒绝覆盖。
    Corrupt,
    /// settings 存在但当前进程无权读取或底层 IO 不可用。
    Unavailable,
}

pub fn settings_path() -> Result<PathBuf> {
    let config_dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => dirs::home_dir()
            .context(summon_i18n::text!("rccb8efa449"))?
            .join(".claude"),
    };
    let config_dir = normalize_config_dir(&config_dir)?;
    Ok(config_dir.join("settings.json"))
}

fn normalize_config_dir(path: &Path) -> Result<PathBuf> {
    use std::path::Component;

    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        bail!(summon_i18n::format!("rd2031521fa"));
    }
    match std::fs::canonicalize(path) {
        Ok(canonical) if canonical.is_dir() => Ok(canonical),
        Ok(canonical) => bail!(summon_i18n::format!("rb8d22587a6")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(err) => Err(err).with_context(|| summon_i18n::format!("r70261948d4")),
    }
}

pub fn install() -> Result<PathBuf> {
    let path = settings_path()?;
    let executable = std::env::current_exe().context(summon_i18n::text!("r613ae8c646"))?;
    install_at(&path, &executable)?;
    Ok(path)
}

pub fn uninstall() -> Result<PathBuf> {
    let path = settings_path()?;
    uninstall_at(&path)?;
    Ok(path)
}

pub fn status() -> Result<(PathBuf, InstallStatus)> {
    let path = settings_path()?;
    let executable = std::env::current_exe().context(summon_i18n::text!("r613ae8c646"))?;
    let status = match status_at_with_executable(&path, Some(&executable)) {
        Ok(status) => status,
        Err(_) if std::fs::File::open(&path).is_err() => InstallStatus::Unavailable,
        Err(_) => InstallStatus::Corrupt,
    };
    Ok((path, status))
}

pub(crate) fn install_at(path: &Path, executable: &Path) -> Result<()> {
    with_settings_lock(path, || install_locked(path, executable))
}

fn install_locked(path: &Path, executable: &Path) -> Result<()> {
    let original = read_optional(path)?;
    let mut root = read_settings(path)?;
    let root_object = root
        .as_object_mut()
        .context(summon_i18n::text!("r825463db33"))?;
    let hooks = object_entry(root_object, "hooks")?;
    let handler = handler_value(executable);
    for event in HOOK_EVENTS {
        let groups = array_entry(hooks, event)?;
        validate_groups(event, groups)?;
        let _ = remove_ours(groups);
        groups.push(json!({
            "matcher": "",
            "hooks": [handler.clone()]
        }));
    }
    write_if_changed(path, &root, original.as_deref())
}

pub(crate) fn uninstall_at(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    with_settings_lock(path, || uninstall_locked(path))
}

fn uninstall_locked(path: &Path) -> Result<()> {
    let original = read_optional(path)?;
    let mut root = read_settings(path)?;
    let Some(root_object) = root.as_object_mut() else {
        bail!(summon_i18n::format!("r825463db33"));
    };
    let Some(hooks_value) = root_object.get_mut("hooks") else {
        return Ok(());
    };
    let Some(hooks) = hooks_value.as_object_mut() else {
        bail!(summon_i18n::format!("r9a118cf533"));
    };
    let mut empty_managed_events = Vec::new();
    let mut removed_any = false;
    for event in HOOK_EVENTS {
        let Some(groups_value) = hooks.get_mut(event) else {
            continue;
        };
        let Some(groups) = groups_value.as_array_mut() else {
            bail!(summon_i18n::format!("ra764925e5b"));
        };
        validate_groups(event, groups)?;
        let removed = remove_ours(groups);
        removed_any |= removed;
        if removed && groups.is_empty() {
            empty_managed_events.push(event);
        }
    }
    for event in empty_managed_events {
        hooks.remove(event);
    }
    if removed_any && hooks.is_empty() {
        root_object.remove("hooks");
    }
    write_if_changed(path, &root, original.as_deref())
}

#[cfg(test)]
pub(crate) fn status_at(path: &Path) -> Result<InstallStatus> {
    status_at_with_executable(path, None)
}

fn status_at_with_executable(path: &Path, executable: Option<&Path>) -> Result<InstallStatus> {
    if !path.exists() {
        return Ok(InstallStatus::Absent);
    }
    let root = read_settings(path)?;
    let root = root
        .as_object()
        .context(summon_i18n::text!("r825463db33"))?;
    let hooks = match root.get("hooks") {
        None => None,
        Some(value) => Some(
            value
                .as_object()
                .context(summon_i18n::text!("r9a118cf533"))?,
        ),
    };
    let mut installed = 0;
    let mut current = 0;
    for event in HOOK_EVENTS {
        let Some(value) = hooks.and_then(|hooks| hooks.get(event)) else {
            continue;
        };
        let groups = value
            .as_array()
            .with_context(|| summon_i18n::format!("ra764925e5b"))?;
        validate_groups(event, groups)?;
        let handlers: Vec<&Value> = group_handlers(groups)
            .filter(|handler| is_ours(handler))
            .collect();
        if !handlers.is_empty() {
            installed += 1;
        }
        if executable.is_some_and(|executable| {
            handlers
                .iter()
                .any(|handler| handler_targets_executable(handler, executable))
        }) {
            current += 1;
        }
    }
    if installed > 0
        && root
            .get("disableAllHooks")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Ok(InstallStatus::Disabled);
    }
    if executable.is_some() && installed > 0 && current != installed {
        return Ok(InstallStatus::Stale {
            configured: installed,
            current,
        });
    }
    Ok(match installed {
        0 => InstallStatus::Absent,
        count if count == HOOK_EVENTS.len() => InstallStatus::Installed,
        installed => InstallStatus::Partial { installed },
    })
}

fn handler_targets_executable(handler: &Value, executable: &Path) -> bool {
    let Some(command) = handler
        .as_object()
        .and_then(|object| object.get("command"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let Ok(configured) = std::fs::canonicalize(command) else {
        return false;
    };
    let Ok(expected) = std::fs::canonicalize(executable) else {
        return false;
    };
    configured == expected && configured.is_file()
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read(path) {
        Ok(content) => {
            serde_json::from_slice(&content).with_context(|| summon_i18n::format!("r4d49905988"))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(err) => Err(err).with_context(|| summon_i18n::format!("rf45ace8440")),
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| summon_i18n::format!("rf45ace8440")),
    }
}

fn with_settings_lock<T>(path: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let parent = path.parent().context(summon_i18n::text!("r508096839a"))?;
    let parent_existed = parent.exists();
    std::fs::create_dir_all(parent).with_context(|| summon_i18n::format!("r72ea1f78da"))?;
    if !parent_existed {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| summon_i18n::format!("r2d8e3c4f68"))?;
    }
    // 锁文件不参与 atomic rename，多个 summon install/uninstall 始终竞争同一 inode。
    let lock_path = parent.join(".summon-bridge-settings.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| summon_i18n::format!("rb05ca57f5f"))?;
    lock.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| summon_i18n::format!("rbff37c8d4b"))?;
    lock.lock()
        .with_context(|| format!("flock {lock_path:?}"))?;
    action()
}

fn write_if_changed(path: &Path, root: &Value, expected: Option<&[u8]>) -> Result<()> {
    let mut rendered =
        serde_json::to_vec_pretty(root).context(summon_i18n::text!("r9d8e3822d8"))?;
    rendered.push(b'\n');
    // dotfile manager 常把 settings.json 做成 symlink；原子 rename 若直接写 link 路径会把
    // symlink 本身替换掉。已有 link 必须写它解析后的真实文件，且解析失败时 fail-closed。
    let target = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::canonicalize(path).with_context(|| summon_i18n::format!("r51339fcf95"))?
        }
        Ok(_) => path.to_path_buf(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(err) => return Err(err).with_context(|| summon_i18n::format!("r09420def47")),
    };
    let current = read_optional(&target)?;
    if current.as_deref() == Some(rendered.as_slice()) {
        return Ok(());
    }
    anyhow::ensure!(
        current.as_deref() == expected,
        summon_i18n::format!("r805489e7d0")
    );
    let mode = match std::fs::metadata(&target) {
        Ok(metadata) => {
            anyhow::ensure!(metadata.is_file(), summon_i18n::format!("rc2d98fd272"));
            anyhow::ensure!(
                metadata.uid() == nix::unistd::getuid().as_raw(),
                summon_i18n::format!("r3fa8e99a51")
            );
            metadata.mode() & 0o7777
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0o600,
        Err(err) => return Err(err).with_context(|| summon_i18n::format!("rb07d8547d8")),
    };
    atomic_write_mode(&target, &rendered, mode).with_context(|| summon_i18n::format!("r66fadbfd38"))
}

fn object_entry<'a>(
    root: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    let value = root
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    value
        .as_object_mut()
        .with_context(|| summon_i18n::format!("r3bd982612b"))
}

fn array_entry<'a>(root: &'a mut Map<String, Value>, key: &str) -> Result<&'a mut Vec<Value>> {
    let value = root
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    value
        .as_array_mut()
        .with_context(|| summon_i18n::format!("r2a532787e6"))
}

fn handler_value(executable: &Path) -> Value {
    json!({
        "type": "command",
        "command": executable.to_string_lossy(),
        "args": ["bridge", "hook", "--protocol", HOOK_PROTOCOL],
        "timeout": 2
    })
}

fn is_ours(handler: &Value) -> bool {
    let Some(object) = handler.as_object() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) != Some("command") {
        return false;
    }
    let expected = ["bridge", "hook", "--protocol", HOOK_PROTOCOL];
    object
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(|args| {
            args.len() == expected.len()
                && args
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| actual.as_str() == Some(expected))
        })
}

fn group_handlers(groups: &[Value]) -> impl Iterator<Item = &Value> {
    groups
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|group| group.get("hooks"))
        .filter_map(Value::as_array)
        .flatten()
}

fn validate_groups(event: &str, groups: &[Value]) -> Result<()> {
    for (index, group) in groups.iter().enumerate() {
        let object = group
            .as_object()
            .with_context(|| summon_i18n::format!("r96ce39f734"))?;
        let handlers = object
            .get("hooks")
            .and_then(Value::as_array)
            .with_context(|| summon_i18n::format!("reda00d6e48"))?;
        for (handler_index, handler) in handlers.iter().enumerate() {
            anyhow::ensure!(handler.is_object(), summon_i18n::format!("rcc2c576c59"));
        }
    }
    Ok(())
}

fn remove_ours(groups: &mut Vec<Value>) -> bool {
    let mut removed_any = false;
    groups.retain_mut(|group| {
        let Some(handlers) = group
            .as_object_mut()
            .and_then(|group| group.get_mut("hooks"))
            .and_then(Value::as_array_mut)
        else {
            return true;
        };
        let before = handlers.len();
        handlers.retain(|handler| !is_ours(handler));
        let removed_here = handlers.len() != before;
        removed_any |= removed_here;
        // 只删除“原本含 summon handler、删后为空”的 group；用户自己已有的 empty group 原样保留。
        !(removed_here && handlers.is_empty())
    });
    removed_any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_preserves_existing_session_end_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
  "theme": "dark",
  "hooks": {
    "SessionEnd": [{"matcher":"","hooks":[{"type":"command","command":"keep-me"}]}]
  }
}"#,
        )
        .unwrap();
        install_at(&path, Path::new("/opt/summon")).unwrap();
        let once = std::fs::read(&path).unwrap();
        install_at(&path, Path::new("/opt/summon")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), once);

        let root: Value = serde_json::from_slice(&once).unwrap();
        assert_eq!(root["theme"], "dark");
        let session_end = root["hooks"]["SessionEnd"].as_array().unwrap();
        assert!(group_handlers(session_end)
            .any(|handler| handler["command"].as_str() == Some("keep-me")));
        assert_eq!(
            group_handlers(session_end).filter(|h| is_ours(h)).count(),
            1
        );
        assert_eq!(status_at(&path).unwrap(), InstallStatus::Installed);
    }

    #[test]
    fn uninstall_removes_only_protocol_handlers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        install_at(&path, Path::new("/old/summon")).unwrap();
        let mut root: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        root["hooks"]["SessionEnd"]
            .as_array_mut()
            .unwrap()
            .push(json!({"matcher":"","hooks":[{"type":"command","command":"keep-me"}]}));
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&root).unwrap(), 0o600).unwrap();

        uninstall_at(&path).unwrap();
        let root: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let session_end = root["hooks"]["SessionEnd"].as_array().unwrap();
        assert!(group_handlers(session_end)
            .any(|handler| handler["command"].as_str() == Some("keep-me")));
        assert!(!root["hooks"]
            .as_object()
            .unwrap()
            .contains_key("SessionStart"));
        assert_eq!(status_at(&path).unwrap(), InstallStatus::Absent);
    }

    #[test]
    fn uninstall_preserves_unrelated_empty_groups_and_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            br#"{"hooks":{"CustomEvent":[],"SessionEnd":[{"matcher":"user-empty","hooks":[]}]}}"#,
        )
        .unwrap();
        install_at(&path, Path::new("/opt/summon")).unwrap();
        uninstall_at(&path).unwrap();
        let root: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(root["hooks"]["CustomEvent"], json!([]));
        assert_eq!(
            root["hooks"]["SessionEnd"],
            json!([{"matcher":"user-empty","hooks":[]}])
        );
    }

    #[test]
    fn invalid_json_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{broken").unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(install_at(&path, Path::new("/opt/summon")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(uninstall_at(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn status_rejects_wrong_schema_instead_of_reporting_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, br#"{"hooks": []}"#).unwrap();
        assert!(status_at(&path).is_err());
        std::fs::write(&path, br#"[]"#).unwrap();
        assert!(status_at(&path).is_err());
        std::fs::write(&path, br#"{"hooks":{"SessionStart":{}}}"#).unwrap();
        assert!(status_at(&path).is_err());
    }

    #[test]
    fn install_preserves_existing_mode_and_new_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.json");
        std::fs::write(&existing, b"{}\n").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o640)).unwrap();
        install_at(&existing, Path::new("/opt/summon")).unwrap();
        assert_eq!(std::fs::metadata(&existing).unwrap().mode() & 0o777, 0o640);

        let fresh = dir.path().join("fresh.json");
        install_at(&fresh, Path::new("/opt/summon")).unwrap();
        assert_eq!(std::fs::metadata(&fresh).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(dir.path().join(".summon-bridge-settings.lock"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn install_updates_symlink_target_without_replacing_link() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("managed-settings.json");
        let link = dir.path().join("settings.json");
        std::fs::write(&target, b"{}\n").unwrap();
        symlink(&target, &link).unwrap();

        install_at(&link, Path::new("/opt/summon")).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(status_at(&target).unwrap(), InstallStatus::Installed);
    }

    #[test]
    fn config_dir_rejects_relative_and_parent_aliases() {
        assert!(normalize_config_dir(Path::new("relative/.claude")).is_err());
        assert!(normalize_config_dir(Path::new("/tmp/a/../b")).is_err());
        assert_eq!(
            normalize_config_dir(Path::new("/definitely-missing/summon-bridge-config")).unwrap(),
            Path::new("/definitely-missing/summon-bridge-config")
        );
    }

    #[test]
    fn stale_settings_snapshot_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = b"{}\n";
        std::fs::write(&path, original).unwrap();
        std::fs::write(&path, b"{\"theme\":\"changed\"}\n").unwrap();
        let desired = json!({"hooks": {}});
        assert!(write_if_changed(&path, &desired, Some(original)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"theme\":\"changed\"}\n");
    }

    #[test]
    fn runtime_status_distinguishes_current_stale_and_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let executable = std::env::current_exe().unwrap();
        install_at(&path, &executable).unwrap();
        assert_eq!(
            status_at_with_executable(&path, Some(&executable)).unwrap(),
            InstallStatus::Installed
        );

        install_at(&path, Path::new("/missing/old-summon")).unwrap();
        assert!(matches!(
            status_at_with_executable(&path, Some(&executable)).unwrap(),
            InstallStatus::Stale {
                configured: 6,
                current: 0
            }
        ));

        let mut root: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        root["disableAllHooks"] = Value::Bool(true);
        let original = std::fs::read(&path).unwrap();
        write_if_changed(&path, &root, Some(&original)).unwrap();
        assert_eq!(
            status_at_with_executable(&path, Some(&executable)).unwrap(),
            InstallStatus::Disabled
        );
    }
}
