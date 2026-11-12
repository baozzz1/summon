//! Menu Bar 上可改的整机设置:读出当前值,按封闭白名单写回 config.yaml。
//!
//! 只有整机这一层:消息层的几个开关改的是 config.yaml 的 `messaging:`,哪些 agent 在自己的
//! agent.yaml 里覆盖了同一项,只报出名字,不替它们改。开机自启不在 config.yaml 里,由 CLI 走
//! `autostart` 那条路。

use anyhow::{bail, Context};
use serde::Serialize;
use std::path::Path;
use summon_core::config::{builtin_messaging, Config, Messaging, MessagingOverride};
use summon_core::registry::Registry;

/// `summon menubar settings --json` 的契约。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct MenubarSettings {
    /// "off" / "dry-run" / "on"。
    pub sentinel: &'static str,
    pub show_counts: bool,
    pub launch_at_login: bool,
    pub cot_visible: Layered<bool>,
    pub hide_thinking: Layered<bool>,
    pub cot_tool_details: Layered<bool>,
    pub badge_effort: Layered<bool>,
    pub idle_ttl_secs: Layered<u64>,
    /// 0 = 关。
    pub idle_retire_secs: Layered<u64>,
}

/// 整机这一层的值,加上自己覆盖了这一项的 agent。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct Layered<T> {
    pub value: T,
    pub overridden_by: Vec<String>,
}

/// Menu Bar 能改的设置。键名就是 CLI 参数,也是契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingKey {
    Sentinel,
    ShowCounts,
    LaunchAtLogin,
    CotVisible,
    HideThinking,
    CotToolDetails,
    BadgeEffort,
    IdleTtlSecs,
    IdleRetireSecs,
}

impl SettingKey {
    pub(crate) fn parse(key: &str) -> anyhow::Result<Self> {
        Ok(match key {
            "sentinel" => Self::Sentinel,
            "show_counts" => Self::ShowCounts,
            "launch_at_login" => Self::LaunchAtLogin,
            "cot_visible" => Self::CotVisible,
            "hide_thinking" => Self::HideThinking,
            "cot_tool_details" => Self::CotToolDetails,
            "badge_effort" => Self::BadgeEffort,
            "idle_ttl_secs" => Self::IdleTtlSecs,
            "idle_retire_secs" => Self::IdleRetireSecs,
            other => bail!(summon_i18n::format!("rc3b9a9a902")),
        })
    }
}

/// 解析好的一次修改。值在这里校验完,写盘时不再猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettingChange {
    Sentinel(SentinelMode),
    ShowCounts(bool),
    LaunchAtLogin(bool),
    Messaging(MessagingKey, MessagingValue),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SentinelMode {
    Off,
    DryRun,
    On,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessagingKey {
    CotVisible,
    HideThinking,
    CotToolDetails,
    BadgeEffort,
    IdleTtlSecs,
    IdleRetireSecs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessagingValue {
    Flag(bool),
    Secs(u64),
}

impl MessagingKey {
    fn field(self) -> &'static str {
        match self {
            Self::CotVisible => "cot_visible",
            Self::HideThinking => "hide_thinking",
            Self::CotToolDetails => "cot_tool_details",
            Self::BadgeEffort => "badge_effort",
            Self::IdleTtlSecs => "idle_ttl_secs",
            Self::IdleRetireSecs => "idle_retire_secs",
        }
    }

    /// 这一项在合并后的消息层里是什么值。`idle_retire_secs` 关 = 0。
    fn resolved(self, m: &Messaging) -> MessagingValue {
        match self {
            Self::CotVisible => MessagingValue::Flag(m.cot_visible),
            Self::HideThinking => MessagingValue::Flag(m.hide_thinking),
            Self::CotToolDetails => MessagingValue::Flag(m.cot_tool_details),
            Self::BadgeEffort => MessagingValue::Flag(m.badge_effort),
            Self::IdleTtlSecs => MessagingValue::Secs(m.idle_ttl_secs),
            Self::IdleRetireSecs => {
                MessagingValue::Secs(m.idle_retire().map(|d| d.as_secs()).unwrap_or(0))
            }
        }
    }

    fn overridden_in(self, o: &MessagingOverride) -> bool {
        match self {
            Self::CotVisible => o.cot_visible.is_some(),
            Self::HideThinking => o.hide_thinking.is_some(),
            Self::CotToolDetails => o.cot_tool_details.is_some(),
            Self::BadgeEffort => o.badge_effort.is_some(),
            Self::IdleTtlSecs => o.idle_ttl_secs.is_some(),
            Self::IdleRetireSecs => o.idle_retire_secs.is_some(),
        }
    }
}

impl SettingChange {
    pub(crate) fn parse(key: SettingKey, value: &str) -> anyhow::Result<Self> {
        let flag = || -> anyhow::Result<bool> {
            match value {
                "on" | "true" => Ok(true),
                "off" | "false" => Ok(false),
                other => bail!(summon_i18n::format!("r9493cf9bcb")),
            }
        };
        let secs = || -> anyhow::Result<u64> {
            value
                .parse::<u64>()
                .with_context(|| summon_i18n::format!("rdfab9789b5"))
        };
        let messaging_flag = |k: MessagingKey| -> anyhow::Result<Self> {
            Ok(Self::Messaging(k, MessagingValue::Flag(flag()?)))
        };
        Ok(match key {
            SettingKey::Sentinel => Self::Sentinel(match value {
                "off" => SentinelMode::Off,
                "dry-run" => SentinelMode::DryRun,
                "on" => SentinelMode::On,
                other => bail!(summon_i18n::format!("r598bc1d40d")),
            }),
            SettingKey::ShowCounts => Self::ShowCounts(flag()?),
            SettingKey::LaunchAtLogin => Self::LaunchAtLogin(flag()?),
            SettingKey::CotVisible => messaging_flag(MessagingKey::CotVisible)?,
            SettingKey::HideThinking => messaging_flag(MessagingKey::HideThinking)?,
            SettingKey::CotToolDetails => messaging_flag(MessagingKey::CotToolDetails)?,
            SettingKey::BadgeEffort => messaging_flag(MessagingKey::BadgeEffort)?,
            SettingKey::IdleTtlSecs => {
                // 0 不是关,是「空闲一刻就发提醒卡」(判据是 空闲时长 > ttl)。
                let secs = secs()?;
                anyhow::ensure!(secs > 0, summon_i18n::format!("r3293e9386a"));
                Self::Messaging(MessagingKey::IdleTtlSecs, MessagingValue::Secs(secs))
            }
            SettingKey::IdleRetireSecs => {
                Self::Messaging(MessagingKey::IdleRetireSecs, MessagingValue::Secs(secs()?))
            }
        })
    }
}

/// 读出 Menu Bar 设置区要显示的全部值。`launch_at_login` 由调用方读(它在自启状态文件里)。
pub(crate) fn read(cfg: &Config, reg: &Registry, launch_at_login: bool) -> MenubarSettings {
    let mut global = builtin_messaging();
    global.apply(&cfg.messaging);
    let overridden_by = |key: MessagingKey| -> Vec<String> {
        reg.agents
            .iter()
            .filter(|agent| !agent.is_steward())
            .filter(|agent| {
                agent
                    .messaging
                    .as_ref()
                    .is_some_and(|o| key.overridden_in(o))
            })
            .map(|agent| agent.name.clone())
            .collect()
    };
    let layered = |value, key| Layered {
        value,
        overridden_by: overridden_by(key),
    };
    MenubarSettings {
        sentinel: crate::observe::fleet_snapshot::sentinel_mode(&cfg.sentinel),
        show_counts: cfg.menubar.show_counts(),
        launch_at_login,
        cot_visible: layered(global.cot_visible, MessagingKey::CotVisible),
        hide_thinking: layered(global.hide_thinking, MessagingKey::HideThinking),
        cot_tool_details: layered(global.cot_tool_details, MessagingKey::CotToolDetails),
        badge_effort: layered(global.badge_effort, MessagingKey::BadgeEffort),
        idle_ttl_secs: Layered {
            value: global.idle_ttl_secs,
            overridden_by: overridden_by(MessagingKey::IdleTtlSecs),
        },
        idle_retire_secs: Layered {
            value: global.idle_retire().map(|d| d.as_secs()).unwrap_or(0),
            overridden_by: overridden_by(MessagingKey::IdleRetireSecs),
        },
    }
}

/// 把一次修改写进 config.yaml。开机自启不在这里(见模块注释),传进来就报错。
pub(crate) fn apply_to_config(path: &Path, change: &SettingChange) -> anyhow::Result<()> {
    use crate::setup::config_edit::set_section_key;
    use crate::setup::sentinel_config::set_sentinel_flag;
    match *change {
        // 先动 dry_run 再动 enabled:从 off 切到 dry-run 时,中间不会有一刻是真开着的。
        SettingChange::Sentinel(SentinelMode::Off) => set_sentinel_flag(path, "enabled", false),
        SettingChange::Sentinel(SentinelMode::DryRun) => {
            set_sentinel_flag(path, "dry_run", true)?;
            set_sentinel_flag(path, "enabled", true)
        }
        SettingChange::Sentinel(SentinelMode::On) => {
            set_sentinel_flag(path, "dry_run", false)?;
            set_sentinel_flag(path, "enabled", true)
        }
        SettingChange::ShowCounts(on) => {
            set_section_key(path, "menubar", "show_counts", &on.to_string(), |c| {
                c.menubar.show_counts() == on
            })
        }
        SettingChange::LaunchAtLogin(_) => bail!(summon_i18n::format!("r180279261a")),
        SettingChange::Messaging(key, value) => {
            let literal = match value {
                MessagingValue::Flag(on) => on.to_string(),
                MessagingValue::Secs(secs) => secs.to_string(),
            };
            set_section_key(path, "messaging", key.field(), &literal, |c| {
                let mut m = builtin_messaging();
                m.apply(&c.messaging);
                key.resolved(&m) == value
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(yaml: &str) -> summon_core::registry::Agent {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn registry(agents: Vec<summon_core::registry::Agent>) -> Registry {
        Registry { agents }
    }

    fn config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn reads_the_machine_layer_and_names_agents_that_override_it() {
        let cfg = config(
            "sentinel:\n  enabled: true\n  dry_run: true\nmenubar:\n  show_counts: false\n\
             messaging:\n  cot_tool_details: true\n  idle_ttl_secs: 3600\n",
        );
        let reg = registry(vec![
            agent("name: a\nmessaging:\n  cot_tool_details: false\n"),
            agent("name: b\n"),
            agent("name: c\nmessaging:\n  cot_tool_details: true\n  idle_retire_secs: 60\n"),
        ]);
        let s = read(&cfg, &reg, true);
        assert_eq!(s.sentinel, "dry-run");
        assert!(!s.show_counts);
        assert!(s.launch_at_login);
        assert_eq!(
            s.cot_tool_details,
            Layered {
                value: true,
                overridden_by: vec!["a".into(), "c".into()]
            }
        );
        assert_eq!(s.idle_ttl_secs.value, 3600);
        assert!(s.idle_ttl_secs.overridden_by.is_empty());
        assert_eq!(s.idle_retire_secs.value, 0, "没配 = 关");
        assert_eq!(s.idle_retire_secs.overridden_by, vec!["c".to_string()]);
        // 没写的项取内置默认。
        assert!(s.cot_visible.value);
    }

    #[test]
    fn only_whitelisted_keys_and_values_parse() {
        assert!(SettingKey::parse("poll_ms").is_err());
        assert!(SettingKey::parse("system_prompt").is_err());
        let key = SettingKey::parse("cot_visible").unwrap();
        assert_eq!(
            SettingChange::parse(key, "off").unwrap(),
            SettingChange::Messaging(MessagingKey::CotVisible, MessagingValue::Flag(false))
        );
        assert!(SettingChange::parse(key, "maybe").is_err());
        assert!(SettingChange::parse(SettingKey::Sentinel, "yes").is_err());
        assert!(
            SettingChange::parse(SettingKey::IdleTtlSecs, "0").is_err(),
            "0 不是关"
        );
        assert!(SettingChange::parse(SettingKey::IdleTtlSecs, "-1").is_err());
        assert_eq!(
            SettingChange::parse(SettingKey::IdleRetireSecs, "0").unwrap(),
            SettingChange::Messaging(MessagingKey::IdleRetireSecs, MessagingValue::Secs(0))
        );
    }

    #[test]
    fn writes_each_setting_into_its_section_and_reads_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        std::fs::write(&path, "# owner 的注释\npoll_ms: 400\n").unwrap();
        let set = |key: &str, value: &str| {
            let change = SettingChange::parse(SettingKey::parse(key).unwrap(), value).unwrap();
            apply_to_config(&path, &change).unwrap();
        };
        set("sentinel", "dry-run");
        set("show_counts", "off");
        set("hide_thinking", "on");
        set("idle_ttl_secs", "604800");
        set("idle_retire_secs", "3600");
        let cfg: Config = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let s = read(&cfg, &registry(Vec::new()), false);
        assert_eq!(s.sentinel, "dry-run");
        assert!(!s.show_counts);
        assert!(s.hide_thinking.value);
        assert_eq!(s.idle_ttl_secs.value, 604_800);
        assert_eq!(s.idle_retire_secs.value, 3600);

        set("sentinel", "on");
        set("idle_retire_secs", "0");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# owner 的注释\npoll_ms: 400\n"), "{text}");
        let cfg: Config = serde_yaml::from_str(&text).unwrap();
        let s = read(&cfg, &registry(Vec::new()), false);
        assert_eq!(s.sentinel, "on");
        assert_eq!(s.idle_retire_secs.value, 0);

        set("sentinel", "off");
        let cfg: Config = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read(&cfg, &registry(Vec::new()), false).sentinel, "off");
    }

    #[test]
    fn launch_at_login_is_not_a_config_file_setting() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        assert!(apply_to_config(&path, &SettingChange::LaunchAtLogin(true)).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn json_contract_uses_the_cli_key_names() {
        let s = read(&config(""), &registry(Vec::new()), false);
        let json = serde_json::to_value(&s).unwrap();
        for key in [
            "sentinel",
            "show_counts",
            "launch_at_login",
            "cot_visible",
            "hide_thinking",
            "cot_tool_details",
            "badge_effort",
            "idle_ttl_secs",
            "idle_retire_secs",
        ] {
            assert!(json.get(key).is_some(), "缺 {key}");
            // 读出来的键都能原样拿去写。
            SettingKey::parse(key).unwrap();
        }
        assert!(json["cot_visible"]["overridden_by"].is_array());
    }
}
