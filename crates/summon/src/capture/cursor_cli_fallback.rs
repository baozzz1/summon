//! Cursor 的参数目录超时后，只允许读取与本轮请求完全一致的持久模型配置。

use serde_json::Value;
use std::io::Read as _;
use std::path::Path;
use std::time::SystemTime;

const MAX_BYTES: u64 = 1024 * 1024;
// 文件系统 mtime 与日志时间精度各异；两道时间闸使用同一有限容差。
const LOG_TIME_TOLERANCE: std::time::Duration = std::time::Duration::from_secs(1);

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_BYTES).then_some(bytes)
}

#[cfg(test)]
fn saved_model_matches(root: &Path, requested: &str) -> bool {
    let Some(bytes) = read_bounded(&root.join("cli-config.json")) else {
        return false;
    };
    config_matches(&bytes, requested)
}

fn config_matches(bytes: &[u8], requested: &str) -> bool {
    let Ok(config) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    let Some(model) = config["model"]["modelId"].as_str() else {
        return false;
    };
    if config["selectedModel"]["modelId"].as_str() != Some(model)
        || config["hasChangedDefaultModel"].as_bool() != Some(true)
        || config["maxMode"].as_bool() != Some(true)
        || config["model"]["maxMode"].as_bool() != Some(true)
    {
        return false;
    }
    let Some(parameters) = config["selectedModel"]["parameters"].as_array() else {
        return false;
    };
    let mut pairs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for parameter in parameters {
        let (Some(id), Some(value)) = (parameter["id"].as_str(), parameter["value"].as_str())
        else {
            return false;
        };
        if !seen.insert(id)
            || id.is_empty()
            || value.is_empty()
            || id.contains(['[', ']', ',', '='])
            || value.contains(['[', ']', ',', '='])
        {
            return false;
        }
        pairs.push(format!("{id}={value}"));
    }
    !pairs.is_empty() && requested == format!("{model}[{}]", pairs.join(","))
}

/// 校验与启动使用同一份配置快照；并发切模型不能让重试悄悄变成别的模型。
/// 临时根只有私有配置副本，chats 仍指向原库，SID 与后续轮的存储位置不变。
pub(super) fn prepare(env: &[(String, String)], requested: &str) -> Option<tempfile::TempDir> {
    use std::os::unix::fs::{symlink, PermissionsExt as _};
    let root = super::cursor_context::config_root(env)?;
    let bytes = read_bounded(&root.join("cli-config.json"))?;
    if !config_matches(&bytes, requested) || !root.join("chats").is_dir() {
        return None;
    }
    let tmp = tempfile::Builder::new()
        .prefix("summon-cursor-fallback-")
        .tempdir()
        .ok()?;
    let path = tmp.path().join("cli-config.json");
    std::fs::write(&path, bytes).ok()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok()?;
    symlink(root.join("chats"), tmp.path().join("chats")).ok()?;
    Some(tmp)
}

/// stderr 中的拒绝本身无法区别真无效模型与目录超时；必须核对本次 PID 的原生日志。
/// 日志关闭、目录不可读、PID 重用或格式变化都保留原错误，不凭旧日志重喂消息。
pub(super) fn timed_out(env: &[(String, String)], pid: i32, started: SystemTime) -> bool {
    use super::cursor_context::env_value;
    let root = env_value(env, "TMPDIR")
        .or_else(|| env_value(env, "TMP"))
        .or_else(|| env_value(env, "TEMP"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if !root.is_absolute() {
        return false;
    }
    let root = root.join(format!("cursor-agent-logs-{}", nix::unistd::Uid::current()));
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.take(4096).flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(suffix) = name
            .strip_prefix("session-")
            .and_then(|name| name.strip_suffix(".log"))
        else {
            continue;
        };
        let mut parts = suffix.rsplitn(3, '-');
        if parts
            .next()
            .and_then(|part| part.parse::<u32>().ok())
            .is_none()
            || parts.next().and_then(|part| part.parse::<i32>().ok()) != Some(pid)
        {
            continue;
        }
        let path = entry.path();
        if !entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .is_some_and(|time| {
                time >= started
                    || started
                        .duration_since(time)
                        .is_ok_and(|age| age <= LOG_TIME_TOLERANCE)
            })
        {
            continue;
        }
        let Some(bytes) = read_bounded(&path) else {
            continue;
        };
        let mut session = false;
        let mut timeout = false;
        for line in bytes.split(|byte| *byte == b'\n') {
            let Ok(event) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if event["event"].as_str() == Some("debug-session-start")
                && event["pid"].as_i64() == Some(pid as i64)
            {
                session = event["startTime"]
                    .as_str()
                    .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
                    .map(SystemTime::from)
                    .is_some_and(|time| {
                        time <= SystemTime::now() && time + LOG_TIME_TOLERANCE >= started
                    });
            }
            timeout |=
                event["event"].as_str() == Some("models.fetchAvailableModelsParameterized.timeout");
        }
        if session && timeout {
            return true;
        }
    }
    false
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const MODEL: &str = "claude-opus-5-5[context=1m,effort=high,fast=false]";

    pub(crate) fn config() -> serde_json::Value {
        serde_json::json!({
            "model": {"modelId": "claude-opus-5-5", "maxMode": true},
            "maxMode": true,
            "hasChangedDefaultModel": true,
            "selectedModel": {"modelId": "claude-opus-5-5", "parameters": [
                {"id": "context", "value": "1m"},
                {"id": "effort", "value": "high"},
                {"id": "fast", "value": "false"}
            ]}
        })
    }

    #[test]
    fn preserves_only_the_exact_saved_model_and_parameters() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cli-config.json");
        let original = config();
        std::fs::write(&path, original.to_string()).unwrap();
        assert!(saved_model_matches(tmp.path(), MODEL));
        for (key, value) in [
            ("model", serde_json::json!({"modelId": "auto"})),
            ("hasChangedDefaultModel", serde_json::json!(false)),
            ("maxMode", serde_json::json!(false)),
            (
                "selectedModel",
                serde_json::json!({"modelId": "claude-opus-5-5", "parameters": [
                    {"id": "context", "value": "300k"}, {"id": "effort", "value": "high"},
                    {"id": "fast", "value": "false"}
                ]}),
            ),
        ] {
            let mut changed = original.clone();
            changed[key] = value;
            std::fs::write(&path, changed.to_string()).unwrap();
            assert!(!saved_model_matches(tmp.path(), MODEL), "mismatch: {key}");
        }
        assert!(!saved_model_matches(tmp.path(), "claude-opus-5-5-high"));
    }

    #[test]
    fn freezes_matching_config_but_keeps_the_original_session_store() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("chats")).unwrap();
        let path = tmp.path().join("cli-config.json");
        std::fs::write(&path, config().to_string()).unwrap();
        let env = vec![("CURSOR_CONFIG_DIR".into(), tmp.path().display().to_string())];
        let frozen = prepare(&env, MODEL).unwrap();
        std::fs::write(path, "{}").unwrap();
        assert!(saved_model_matches(frozen.path(), MODEL));
        assert_eq!(
            std::fs::metadata(frozen.path().join("cli-config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::write(frozen.path().join("chats/proof"), "same store").unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("chats/proof")).unwrap(),
            "same store"
        );
        assert!(prepare(&env, MODEL).is_none());
    }

    #[test]
    fn tolerates_filesystem_timestamp_precision_but_rejects_stale_logs() {
        use std::time::Duration;
        let tmp = tempfile::tempdir().unwrap();
        let env = vec![("TMPDIR".into(), tmp.path().display().to_string())];
        let logs = tmp
            .path()
            .join(format!("cursor-agent-logs-{}", nix::unistd::Uid::current()));
        std::fs::create_dir(&logs).unwrap();
        let started = SystemTime::now() - Duration::from_secs(10);
        let path = logs.join("session-test-987654-1.log");
        let start = serde_json::json!({"event": "debug-session-start", "pid": 987654,
            "startTime": chrono::DateTime::<chrono::Utc>::from(started).to_rfc3339()});
        let timeout =
            serde_json::json!({"event": "models.fetchAvailableModelsParameterized.timeout"});
        std::fs::write(&path, format!("{start}\n{timeout}\n")).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        for (lag, expected) in [(1, true), (1000, true), (1001, false), (2000, false)] {
            file.set_times(
                std::fs::FileTimes::new().set_modified(started - Duration::from_millis(lag)),
            )
            .unwrap();
            assert_eq!(
                timed_out(&env, 987654, started),
                expected,
                "mtime lag {lag}ms"
            );
        }
        file.set_times(std::fs::FileTimes::new().set_modified(started))
            .unwrap();
        for event_time in [
            started - Duration::from_secs(2),
            SystemTime::now() + Duration::from_secs(60),
        ] {
            let mut invalid = start.clone();
            invalid["startTime"] =
                serde_json::json!(chrono::DateTime::<chrono::Utc>::from(event_time).to_rfc3339());
            std::fs::write(&path, format!("{invalid}\n{timeout}\n")).unwrap();
            assert!(!timed_out(&env, 987654, started));
        }
    }

    #[test]
    fn requires_the_current_process_timeout_log() {
        let tmp = tempfile::tempdir().unwrap();
        let env = vec![("TMPDIR".into(), tmp.path().display().to_string())];
        let logs = tmp
            .path()
            .join(format!("cursor-agent-logs-{}", nix::unistd::Uid::current()));
        std::fs::create_dir(&logs).unwrap();
        let started = SystemTime::now();
        let path = logs.join("session-test-987654-1.log");
        let start = serde_json::json!({"event": "debug-session-start", "pid": 987654, "startTime": chrono::Utc::now().to_rfc3339()});
        let timeout =
            serde_json::json!({"event": "models.fetchAvailableModelsParameterized.timeout"});
        std::fs::write(&path, format!("{start}\n{timeout}\n")).unwrap();
        assert!(timed_out(&env, 987654, started));
        assert!(!timed_out(&env, 987653, started));
        let mut stale = start.clone();
        stale["startTime"] = serde_json::json!("2000-01-01T00:00:00Z");
        std::fs::write(&path, format!("{stale}\n{timeout}\n")).unwrap();
        assert!(!timed_out(&env, 987654, started));
        std::fs::write(&path, format!("{start}\n")).unwrap();
        assert!(!timed_out(&env, 987654, started));
    }
}
