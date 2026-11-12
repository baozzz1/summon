//! `summon doctor` 里与配置有关的检查,先产出结构化结果,再分别渲染成 doctor 的文本行与控制台
//! 「需要处理」的条目 —— 控制台不另写一份判据。
//!
//! 覆盖:预览 / 终端 / 控制台的监听地址与对外地址是否自洽,Codex 策略的非法值与危险组合。
//! 端口在不在听、二进制在不在这类运行状态不在这里(它们不是配置问题)。

use std::net::Ipv4Addr;

use summon_core::config::Config;
use summon_core::registry::Agent;

/// 一条配置问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigFinding {
    /// 预览:查不到收件设备能路由到的地址,预览链接一条也出不来。
    PreviewNoRoutableHost,
    /// 预览:只听回环,对外地址却是别的 → 链接指向一个没在监听的地址。
    PreviewLoopbackBind { base: String },
    /// 终端:绑定地址沿用 127.0.0.1,手机与别的机器打不开。
    TerminalLoopbackBind,
    /// 控制台:只听回环又没配 `dashboard.public_base_url`,`/dash` 不出链接。
    DashboardLoopbackBind,
    /// 控制台:查不到可路由地址,`/dash` 不出链接。
    DashboardNoRoutableHost,
    /// Codex 策略写了非法值,已降级为默认。
    CodexInvalid {
        agent: String,
        field: String,
        rejected: String,
    },
    /// Codex 策略的危险组合(会暂停等审批、写不了文件又没法升级)。
    CodexWarning { agent: String, warning: String },
}

/// 控制台「需要处理」里的一条:是什么、在哪、怎么改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FindingView {
    pub(crate) error: bool,
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) fix: String,
    /// 跳转到的设置:`None` 的 agent = 全局设置页。
    pub(crate) agent: Option<String>,
    pub(crate) key: Option<&'static str>,
}

impl ConfigFinding {
    /// doctor 里的那一行(含行首缩进与 ⚠)。
    pub(crate) fn doctor_line(&self) -> String {
        match self {
            Self::PreviewNoRoutableHost => summon_i18n::text!("r4404165442").to_string(),
            Self::PreviewLoopbackBind { base } => summon_i18n::format!("r4422e88746"),
            Self::TerminalLoopbackBind => summon_i18n::text!("rf5d3f9270f").to_string(),
            Self::DashboardLoopbackBind => summon_i18n::text!("r034e7d94bf").to_string(),
            Self::DashboardNoRoutableHost => summon_i18n::text!("r5965e20e8c").to_string(),
            Self::CodexInvalid {
                agent,
                field,
                rejected,
            } => {
                summon_i18n::format!("r093889e8b4")
            }
            Self::CodexWarning { agent, warning } => format!("  {agent:<12} ↳ {warning}"),
        }
    }

    pub(crate) fn view(&self) -> FindingView {
        let warn = |title: &str, detail: String, fix: &str, key| FindingView {
            error: false,
            title: title.to_string(),
            detail,
            fix: fix.to_string(),
            agent: None,
            key: Some(key),
        };
        match self {
            Self::PreviewNoRoutableHost => warn(
                summon_i18n::text!("r798b259de4"),
                summon_i18n::text!("r12b335191c").into(),
                summon_i18n::text!("r9032fc51cb"),
                "preview_server.public_base_url",
            ),
            Self::PreviewLoopbackBind { base } => warn(
                summon_i18n::text!("r82c79e14fb"),
                summon_i18n::format!("redb814f15d"),
                summon_i18n::text!("r0b5b480cde"),
                "preview_server.bind",
            ),
            Self::TerminalLoopbackBind => warn(
                summon_i18n::text!("r55507be175"),
                summon_i18n::text!("r9ba6eef1a0").into(),
                summon_i18n::text!("r0b5b480cde"),
                "preview_server.bind",
            ),
            Self::DashboardLoopbackBind => warn(
                summon_i18n::text!("r1526ea3d77"),
                summon_i18n::text!("rcb2af5d747").into(),
                summon_i18n::text!("r928efae2fe"),
                "preview_server.bind",
            ),
            Self::DashboardNoRoutableHost => warn(
                summon_i18n::text!("refa3fc27e1"),
                summon_i18n::text!("r2d39afcefe").into(),
                summon_i18n::text!("rbaceed405f"),
                "dashboard.public_base_url",
            ),
            Self::CodexInvalid {
                agent,
                field,
                rejected,
            } => FindingView {
                error: true,
                title: summon_i18n::format!("r8c4bfc514d"),
                detail: summon_i18n::format!("r141c54abce"),
                fix: summon_i18n::format!(
                    "r75f50243c3",
                    summon_core::codex_policy::SANDBOX_VALUES,
                    summon_core::codex_policy::APPROVAL_VALUES
                ),
                agent: Some(agent.clone()),
                key: Some(if field.contains("approval") {
                    "runtime.codex_approval_policy"
                } else {
                    "runtime.codex_sandbox"
                }),
            },
            Self::CodexWarning { agent, warning } => FindingView {
                error: false,
                title: summon_i18n::format!("rb506f40acd"),
                detail: warning.clone(),
                fix: summon_i18n::text!("re6d4c49fb1").into(),
                agent: Some(agent.clone()),
                key: Some("runtime.codex_sandbox"),
            },
        }
    }
}

/// 预览服务:`base` 是此刻会用的对外地址(查不到为 `None`)。关着时不查。
pub(crate) fn preview_findings(cfg: &Config, base: Option<&str>) -> Vec<ConfigFinding> {
    let pv = &cfg.preview_server;
    if !pv.enabled {
        return Vec::new();
    }
    match base {
        None => vec![ConfigFinding::PreviewNoRoutableHost],
        Some(base) if pv.public_base_url.trim().is_empty() && pv.bind == "127.0.0.1" => {
            vec![ConfigFinding::PreviewLoopbackBind {
                base: base.to_string(),
            }]
        }
        Some(_) => Vec::new(),
    }
}

pub(crate) fn terminal_findings(cfg: &Config) -> Vec<ConfigFinding> {
    if cfg.web_terminal.enabled && cfg.preview_server.bind == "127.0.0.1" {
        vec![ConfigFinding::TerminalLoopbackBind]
    } else {
        Vec::new()
    }
}

/// Codex 策略:只看 codex / traex 的 agent。
pub(crate) fn codex_findings(
    agents: &[&Agent],
    global: &summon_core::codex_policy::CodexPolicyOverride,
) -> Vec<ConfigFinding> {
    let mut out = Vec::new();
    for agent in agents {
        if !matches!(agent.cli_kind(), "codex" | "traex") {
            continue;
        }
        let (policy, fallbacks) = agent.codex_policy(Some(global));
        for fallback in &fallbacks {
            out.push(ConfigFinding::CodexInvalid {
                agent: agent.name.clone(),
                field: fallback.field.to_string(),
                rejected: fallback.rejected.to_string(),
            });
        }
        for warning in [policy.approval_warning(), policy.write_blocked_warning()]
            .into_iter()
            .flatten()
        {
            out.push(ConfigFinding::CodexWarning {
                agent: agent.name.clone(),
                warning: warning.to_string(),
            });
        }
    }
    out
}

/// 控制台「需要处理」用的全部配置问题(地址判定用现查的本机地址)。
pub(crate) fn config_findings(
    cfg: &Config,
    agents: &[&Agent],
    probe: Option<Ipv4Addr>,
) -> Vec<ConfigFinding> {
    let base = cfg.preview_server.base_url_with_host(probe);
    let mut out = preview_findings(cfg, base.as_deref());
    out.extend(terminal_findings(cfg));
    out.extend(crate::runtime::dashboard::dashboard_findings(cfg, probe));
    out.extend(codex_findings(agents, &cfg.codex));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: Ipv4Addr = Ipv4Addr::new(10, 1, 2, 3);

    #[test]
    fn address_findings_follow_bind_and_public_address() {
        let mut cfg = Config::default();
        let found = config_findings(&cfg, &[], Some(LAN));
        assert!(
            found.contains(&ConfigFinding::PreviewLoopbackBind {
                base: "http://10.1.2.3:7765".into()
            }),
            "{found:?}"
        );
        assert!(found.contains(&ConfigFinding::TerminalLoopbackBind));
        assert!(found.contains(&ConfigFinding::DashboardLoopbackBind));
        assert!(config_findings(&cfg, &[], None).contains(&ConfigFinding::PreviewNoRoutableHost));
        cfg.preview_server.bind = "0.0.0.0".into();
        assert!(config_findings(&cfg, &[], Some(LAN)).is_empty());
        cfg.preview_server.enabled = false;
        cfg.web_terminal.enabled = false;
        cfg.dashboard.enabled = false;
        assert!(config_findings(&cfg, &[], None).is_empty());
    }

    #[test]
    fn codex_findings_cover_invalid_values_and_risky_combos() {
        let agent: Agent = serde_yaml::from_str(
            "name: coder\nruntime:\n  cli: codex\n  codex_sandbox: everything\n  codex_approval_policy: on-request\n",
        )
        .unwrap();
        let found = codex_findings(&[&agent], &Default::default());
        assert!(found
            .iter()
            .any(|f| matches!(f, ConfigFinding::CodexInvalid { rejected, .. } if rejected == "everything")));
        assert!(found
            .iter()
            .any(|f| matches!(f, ConfigFinding::CodexWarning { .. })));
        let view = found[0].view();
        assert_eq!(view.agent.as_deref(), Some("coder"));
        assert!(view.error);
    }
}
