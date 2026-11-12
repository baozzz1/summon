//! Claude Code 自己等待额度恢复时，由它持有续跑权，Summon 只保留独立的错误收尾。

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Context;
use summon_core::registry::Agent;

const KEY: &str = "autoContinueAtUsageLimit";
const MAX_SETTINGS_BYTES: u64 = 1_048_576;

/// 只在确认版本支持且配置开启时抑制恢复定时卡；未知配置沿用 Summon 提醒。
pub(crate) fn native_auto_continue_enabled(agent: &Agent, cwd: &Path, session_id: &str) -> bool {
    if agent.cli_kind() != "claude" {
        return false;
    }
    match read_native_auto_continue(agent, cwd, session_id) {
        Ok(enabled) => enabled,
        Err(error) => {
            crate::observe::log::log(&summon_i18n::format!("r4442637385", agent.name));
            false
        }
    }
}

fn read_native_auto_continue(agent: &Agent, cwd: &Path, session_id: &str) -> anyhow::Result<bool> {
    let env = settings_env(agent)?;
    let env_value = |key: &str| {
        env.iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
    };
    let config_dir = env_value("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env_value("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .context(summon_i18n::text!("rfed1811085"))?;
    let config_dir = if config_dir.is_absolute() {
        config_dir
    } else {
        cwd.join(config_dir)
    };
    let home = env_value("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .context(summon_i18n::text!("ra122f93b5e"))?;
    let (launch, version, running_cwd) =
        crate::bridge::process_probe::observe_session_launch(session_id, &config_dir, &home)?;
    if !version_supports_auto_continue(&format!("{version} (Claude Code)")) {
        return Ok(false);
    }
    let flag = flag_setting(&launch.args, &running_cwd)?;
    // 远端策略与 MDM 有额外签名、账户和归属校验；本地文件不能替代 Claude 的裁决。
    anyhow::ensure!(
        !config_dir.join("remote-settings.json").try_exists()?,
        summon_i18n::format!("r7a22554b84")
    );
    #[cfg(target_os = "macos")]
    {
        for path in [
            PathBuf::from("/Library/Managed Preferences/com.anthropic.claudecode.plist"),
            PathBuf::from("/Library/Preferences/com.anthropic.claudecode.plist"),
        ] {
            anyhow::ensure!(!path.try_exists()?, summon_i18n::format!("re09962ce49"));
        }
    }
    let policy_dir = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    } else if cfg!(target_os = "windows") {
        PathBuf::from("C:/Program Files/ClaudeCode")
    } else {
        PathBuf::from("/etc/claude-code")
    };
    read_settings_with_flag(&config_dir, &running_cwd, &policy_dir, flag)
}

fn settings_env(agent: &Agent) -> anyhow::Result<Vec<(String, String)>> {
    let env = crate::runtime::daemon::resolve_extra_env(agent);
    anyhow::ensure!(
        !env.iter()
            .any(|(key, value)| { key == "CLAUDE_CODE_USE_COWORK_PLUGINS" && !value.is_empty() }),
        summon_i18n::format!("re6c3127f8f")
    );
    Ok(env)
}

fn version_supports_auto_continue(text: &str) -> bool {
    let Some(version) = text.trim().strip_suffix(" (Claude Code)") else {
        return false;
    };
    let values = version
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>();
    matches!(values.as_deref(), Ok([major, minor, patch]) if (*major, *minor, *patch) >= (2, 1, 234))
}

/// `None` 表示文件中没有该键，非布尔值不是开启凭据。
fn setting(path: &Path) -> anyhow::Result<Option<bool>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context(summon_i18n::text!("r199f8d3da7")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_SETTINGS_BYTES,
        summon_i18n::format!("r20f3e37ea4")
    );
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let settings: serde_json::Value =
        serde_json::from_slice(&bytes).context(summon_i18n::text!("rd114620862"))?;
    let object = settings
        .as_object()
        .context(summon_i18n::text!("r1ff6963957"))?;
    object
        .get(KEY)
        .map(|value| value.as_bool().context(summon_i18n::text!("r38987d0934")))
        .transpose()
}

fn flag_setting(args: &[String], cwd: &Path) -> anyhow::Result<Option<bool>> {
    let mut flag = None;
    let mut seen = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        anyhow::ensure!(
            !matches!(
                arg.as_str(),
                "--setting-sources" | "--settings-sources" | "--print" | "-p"
            ) && !arg.starts_with("--setting-sources=")
                && !arg.starts_with("--settings-sources="),
            summon_i18n::format!("r5b78f584a1")
        );
        let value = if arg == "--settings" {
            Some(
                args.next()
                    .context(summon_i18n::text!("r825391c1b5"))?
                    .as_str(),
            )
        } else {
            arg.strip_prefix("--settings=")
        };
        let Some(value) = value else { continue };
        anyhow::ensure!(!seen, summon_i18n::format!("rdb14b39028"));
        seen = true;
        if value.trim_start().starts_with('{') {
            let document: serde_json::Value = serde_json::from_str(value)?;
            let object = document
                .as_object()
                .context(summon_i18n::text!("rc7f7c45c07"))?;
            flag = object
                .get(KEY)
                .map(|value| value.as_bool().context(summon_i18n::text!("r2ed4a3c228")))
                .transpose()?;
        } else {
            let path = cwd.join(value);
            anyhow::ensure!(path.try_exists()?, summon_i18n::format!("ra462b4115f"));
            flag = setting(&path)?;
        }
    }
    Ok(flag)
}

#[cfg(test)]
fn read_settings(config: &Path, cwd: &Path, policy: &Path) -> anyhow::Result<bool> {
    read_settings_with_flag(config, cwd, policy, None)
}

fn read_settings_with_flag(
    config: &Path,
    cwd: &Path,
    policy: &Path,
    flag: Option<bool>,
) -> anyhow::Result<bool> {
    // Cowork 也可由原生 host 状态选择，文件存在时仅凭普通 settings 无法证明生效来源。
    anyhow::ensure!(
        !config.join("cowork_settings.json").try_exists()?,
        summon_i18n::format!("r33f0b01432")
    );
    let mut managed = setting(&policy.join("managed-settings.json"))?;
    let drop_ins = policy.join("managed-settings.d");
    let mut paths = match std::fs::read_dir(drop_ins) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    paths.sort();
    for path in paths
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
    {
        if let Some(value) = setting(&path)? {
            anyhow::ensure!(
                managed.is_none_or(|previous| previous == value),
                summon_i18n::format!("r9d13e1d3eb")
            );
            managed = Some(value);
        }
    }
    let user = setting(&config.join("settings.json"))?;
    // Claude 的此项只从 policy / flag / user 取有效值，项目中的同名键只影响缺省开启判定。
    if managed == Some(true) && (flag == Some(false) || user == Some(false)) {
        anyhow::bail!(summon_i18n::format!("rfd48e568c7"));
    }
    if let Some(enabled) = managed.or(flag).or(user) {
        return Ok(enabled);
    }
    let project = setting(&cwd.join(".claude/settings.json"))?;
    let local = setting(&cwd.join(".claude/settings.local.json"))?;
    let root_local = repository_local_settings(cwd)?
        .map(|path| setting(&path))
        .transpose()?
        .flatten();
    Ok(project.is_none() && local.is_none() && root_local.is_none())
}

fn repository_local_settings(cwd: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut repository = None;
    for path in cwd.ancestors() {
        if path.join(".git").try_exists()? {
            repository = Some(path);
            break;
        }
    }
    let Some(root) = repository else {
        return Ok(None);
    };
    let git = root.join(".git");
    if git.is_dir() {
        return Ok(Some(root.join(".claude/settings.local.json")));
    }
    let marker = std::fs::read_to_string(git)?;
    let dir = PathBuf::from(
        marker
            .trim()
            .strip_prefix("gitdir: ")
            .context(summon_i18n::text!("r914a41df80"))?,
    );
    let dir = if dir.is_absolute() {
        dir
    } else {
        root.join(dir)
    };
    let common_path = dir.join("commondir");
    if !common_path.try_exists()? {
        return Ok(Some(root.join(".claude/settings.local.json")));
    }
    let common = std::fs::read_to_string(common_path)?;
    let common = dir.join(common.trim()).canonicalize()?;
    Ok(Some(
        common
            .parent()
            .context(summon_i18n::text!("r2c017e478f"))?
            .join(".claude/settings.local.json"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, relative: &str, value: &str) {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value).unwrap();
    }

    #[test]
    fn version_gate_accepts_supported_claude_only() {
        assert!(version_supports_auto_continue("2.1.302 (Claude Code)\n"));
        assert!(version_supports_auto_continue("2.1.234 (Claude Code)"));
        for old in [
            "2.1.233 (Claude Code)",
            "2.1.302",
            "unknown",
            "2.1.302-beta (Claude Code)",
        ] {
            assert!(!version_supports_auto_continue(old));
        }
    }

    #[test]
    fn settings_are_read_fresh_and_managed_value_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let config = root.join("config");
        let cwd = root.join("project");
        let policy = root.join("policy");
        assert!(read_settings(&config, &cwd, &policy).unwrap());
        write(
            &config,
            "settings.json",
            r#"{"autoContinueAtUsageLimit":false}"#,
        );
        assert!(!read_settings(&config, &cwd, &policy).unwrap());
        write(
            &config,
            "settings.json",
            r#"{"autoContinueAtUsageLimit":true}"#,
        );
        assert!(read_settings(&config, &cwd, &policy).unwrap());
        write(
            &policy,
            "managed-settings.json",
            r#"{"autoContinueAtUsageLimit":false}"#,
        );
        assert!(!read_settings(&config, &cwd, &policy).unwrap());
        write(
            &policy,
            "managed-settings.d/10-team.json",
            r#"{"autoContinueAtUsageLimit":true}"#,
        );
        assert!(read_settings(&config, &cwd, &policy).is_err());
    }

    #[test]
    fn project_key_does_not_opt_in_and_unreadable_settings_preserve_reminder() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let cwd = tmp.path().join("project");
        let policy = tmp.path().join("policy");
        write(
            &cwd,
            ".claude/settings.json",
            r#"{"autoContinueAtUsageLimit":true}"#,
        );
        assert!(!read_settings(&config, &cwd, &policy).unwrap());
        write(
            &config,
            "settings.json",
            r#"{"autoContinueAtUsageLimit":"true"}"#,
        );
        assert!(read_settings(&config, &cwd, &policy).is_err());
        write(&config, "settings.json", "not json");
        assert!(read_settings(&config, &cwd, &policy).is_err());
    }

    #[test]
    fn cowork_settings_preserve_recovery_when_the_live_source_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let policy = tmp.path().join("policy");
        write(
            &config,
            "settings.json",
            r#"{"autoContinueAtUsageLimit":true}"#,
        );
        write(
            &config,
            "cowork_settings.json",
            r#"{"autoContinueAtUsageLimit":false}"#,
        );
        assert!(read_settings(&config, tmp.path(), &policy).is_err());
        assert!(read_settings_with_flag(&config, tmp.path(), &policy, Some(true)).is_err());
    }

    #[test]
    fn settings_environment_uses_expanded_launch_values_and_rejects_cowork() {
        let (mut agent, _agent_dir) = crate::test_support::agent_dir("claude", "");
        std::fs::write(
            agent.dir.join(".env"),
            "QUOTA_TEST_COWORK_MODE=0\nQUOTA_TEST_CONFIG=/tmp/claude-quota-config\n",
        )
        .unwrap();
        let runtime = agent.runtime.as_mut().unwrap();
        let env = runtime.env.get_or_insert_with(Default::default);
        env.insert("CLAUDE_CONFIG_DIR".into(), "${QUOTA_TEST_CONFIG}".into());
        let expanded = settings_env(&agent).unwrap();
        assert!(expanded.contains(&(
            "CLAUDE_CONFIG_DIR".into(),
            "/tmp/claude-quota-config".into()
        )));
        agent
            .runtime
            .as_mut()
            .unwrap()
            .env
            .as_mut()
            .unwrap()
            .insert(
                "CLAUDE_CODE_USE_COWORK_PLUGINS".into(),
                "${QUOTA_TEST_COWORK_MODE}".into(),
            );
        let error =
            read_native_auto_continue(&agent, std::path::Path::new("/tmp"), "unknown").unwrap_err();
        assert!(error.to_string().contains("Cowork 设置来源"));
        assert!(!native_auto_continue_enabled(
            &agent,
            std::path::Path::new("/tmp"),
            "unknown"
        ));
    }
    #[test]
    fn repository_local_key_prevents_absent_default_in_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let cwd = repo.join("src/module");
        std::fs::create_dir_all(&cwd).unwrap();
        write(
            &repo,
            ".claude/settings.local.json",
            r#"{"autoContinueAtUsageLimit":false}"#,
        );
        assert!(
            !read_settings(&tmp.path().join("config"), &cwd, &tmp.path().join("policy")).unwrap()
        );
    }

    #[test]
    fn flag_settings_override_user_and_missing_sources_preserve_reminder() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let policy = tmp.path().join("policy");
        write(
            &config,
            "settings.json",
            r#"{"autoContinueAtUsageLimit":true}"#,
        );
        let args = vec![
            "--settings".to_string(),
            r#"{"autoContinueAtUsageLimit":false}"#.to_string(),
        ];
        let flag = flag_setting(&args, tmp.path()).unwrap();
        assert_eq!(flag, Some(false));
        assert!(!read_settings_with_flag(&config, tmp.path(), &policy, flag).unwrap());
        std::fs::remove_file(config.join("settings.json")).unwrap();
        assert!(!read_settings_with_flag(&config, tmp.path(), &policy, flag).unwrap());
        write(
            tmp.path(),
            "flags.json",
            r#"{"autoContinueAtUsageLimit":false}"#,
        );
        assert_eq!(
            flag_setting(&["--settings=flags.json".to_string()], tmp.path()).unwrap(),
            Some(false)
        );
        write(tmp.path(), "flags", r#"{"autoContinueAtUsageLimit":true}"#);
        write(
            tmp.path(),
            "flags with spaces.json",
            r#"{"autoContinueAtUsageLimit":false}"#,
        );
        assert_eq!(
            flag_setting(
                &["--settings".into(), "flags with spaces.json".into()],
                tmp.path()
            )
            .unwrap(),
            Some(false)
        );
        assert!(flag_setting(&["--settings=missing.json".to_string()], tmp.path()).is_err());
        assert!(flag_setting(&["--setting-sources=user".to_string()], tmp.path()).is_err());
        let (agent, _agent_dir) = crate::test_support::agent_dir("claude", "");
        assert!(!native_auto_continue_enabled(
            &agent,
            tmp.path(),
            "nonexistent-session-id"
        ));
    }
}
