//! 跨 daemon 代际保留的最新会话状态。这里只落卡片需要的归一化事实，不保存 provider 原始事件。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use summon_core::context_budget::ContextBudget;

const VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StatusSnapshot {
    version: u8,
    session_id: String,
    observed_at_secs: u64,
    used_tokens: Option<u64>,
    window_tokens: Option<u64>,
    reported_pct: Option<u8>,
}

fn snapshot_path(agent_state_dir: &Path, tag: &str) -> Result<PathBuf> {
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!(summon_i18n::format!("r46d35d0e64"))
    }
    Ok(agent_state_dir
        .join("runtime")
        .join("status")
        .join(format!("{tag}.json")))
}

/// 提交本会话最新的上下文快照。空预算不覆盖上一份可用观测。
pub(crate) fn record_context(
    agent_state_dir: &Path,
    tag: &str,
    session_id: &str,
    budget: ContextBudget,
) -> Result<bool> {
    if session_id.trim().is_empty()
        || (budget.used_tokens.is_none()
            && budget.window_tokens.is_none()
            && budget.reported_pct.is_none())
    {
        return Ok(false);
    }
    let snapshot = StatusSnapshot {
        version: VERSION,
        session_id: session_id.to_string(),
        observed_at_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        used_tokens: budget.used_tokens,
        window_tokens: budget.window_tokens,
        reported_pct: budget.reported_pct.map(|pct| pct.min(100)),
    };
    let content =
        serde_json::to_vec_pretty(&snapshot).context(summon_i18n::text!("rfe43c7444d"))?;
    crate::runtime::fsio::atomic_write_mode(&snapshot_path(agent_state_dir, tag)?, &content, 0o600)
        .context(summon_i18n::text!("r5483db9855"))?;
    Ok(true)
}

fn load(agent_state_dir: &Path, tag: &str) -> Result<Option<StatusSnapshot>> {
    let path = snapshot_path(agent_state_dir, tag)?;
    let content = match std::fs::read(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| summon_i18n::format!("rf45ace8440")),
    };
    let snapshot: StatusSnapshot =
        serde_json::from_slice(&content).context(summon_i18n::text!("r73da0dccce"))?;
    Ok((snapshot.version == VERSION).then_some(snapshot))
}

fn context_of(snapshot: &StatusSnapshot) -> ContextBudget {
    ContextBudget {
        used_tokens: snapshot.used_tokens,
        window_tokens: snapshot.window_tokens,
        reported_pct: snapshot.reported_pct.map(|pct| pct.min(100)),
    }
}

/// 只返回 exact session 的快照。重置、换 provider 或新建 session 后旧文件自然失效。
pub(crate) fn read_context(
    agent_state_dir: &Path,
    tag: &str,
    session_id: &str,
) -> Result<Option<ContextBudget>> {
    if session_id.trim().is_empty() {
        return Ok(None);
    }
    let Some(snapshot) = load(agent_state_dir, tag)? else {
        return Ok(None);
    };
    Ok((snapshot.session_id == session_id).then(|| context_of(&snapshot)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn round_trip_is_private_and_exact_session_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let budget = ContextBudget::from_tokens(350_000, Some(1_000_000));
        assert!(record_context(dir.path(), "g-chat", "sid-a", budget).unwrap());
        assert_eq!(
            read_context(dir.path(), "g-chat", "sid-a").unwrap(),
            Some(budget)
        );
        assert_eq!(read_context(dir.path(), "g-chat", "sid-b").unwrap(), None);
        let path = snapshot_path(dir.path(), "g-chat").unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn empty_observation_does_not_destroy_the_last_known_status() {
        let dir = tempfile::tempdir().unwrap();
        let known = ContextBudget::from_reported_pct(42, Some(200_000));
        record_context(dir.path(), "p-chat", "sid", known).unwrap();
        assert!(!record_context(dir.path(), "p-chat", "sid", ContextBudget::default()).unwrap());
        assert_eq!(
            read_context(dir.path(), "p-chat", "sid").unwrap(),
            Some(known)
        );
    }

    #[test]
    fn tag_cannot_escape_the_status_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(record_context(
            dir.path(),
            "../outside",
            "sid",
            ContextBudget::from_reported_pct(1, None)
        )
        .is_err());
    }
}
