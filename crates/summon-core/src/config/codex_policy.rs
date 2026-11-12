//! codex 托管会话的**权限策略**:sandbox 档位 + approval 策略。
//!
//! 这两个值是**同一条策略的两半**,必须一起表达。只钉一半、另一半交给环境,是这个仓
//! 反复吃过亏的形状:codex 的 `SandboxMode` 默认是 `read-only`,若只钉
//! `approvalPolicy: never` 而放任 sandbox,一台没配过的机器上 agent 就会**写不了任何文件、
//! 又没有审批通道可以升级**,且没有任何提示。
//!
//! 分层与 `resume_sessions` 一致:内置默认 → `~/.summon/config.yaml` 全局 → agent.yaml 覆盖。

use serde::{Deserialize, Serialize};

/// 内置默认。owner 2026-08-21 裁定:按本机 `codex config/read` 的实际状态对齐,
/// 让行为不再取决于每台机器的 `~/.codex/config.toml`。
pub const DEFAULT_SANDBOX: &str = "danger-full-access";
/// 默认仍是 never：飞书审批通道在真实 E2E 与 owner 灰度裁决前不能自动改变既有权限行为。
/// 显式改成别的值会启用当前会话的人工审批，注意事项见 [`CodexPolicy::approval_warning`]。
pub const DEFAULT_APPROVAL_POLICY: &str = "never";

/// codex `SandboxMode` 的全部合法取值(app-server schema)。
pub const SANDBOX_VALUES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];
/// codex `AskForApproval` 的字符串形态取值。
///
/// schema 里还有一个 `{granular: {...}}` 的对象形态,这里**不支持** —— 它需要稳定配置模型、
/// 向后兼容和逐项策略测试，不能把对象压成字符串后静默丢语义。
pub const APPROVAL_VALUES: &[&str] = &["untrusted", "on-request", "never"];

/// 解析好的策略。字段恒为合法值 —— 非法输入在 [`resolve`] 里已降级成默认。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexPolicy {
    pub sandbox: String,
    pub approval_policy: String,
}

impl Default for CodexPolicy {
    fn default() -> Self {
        Self {
            sandbox: DEFAULT_SANDBOX.to_string(),
            approval_policy: DEFAULT_APPROVAL_POLICY.to_string(),
        }
    }
}

impl CodexPolicy {
    /// 非 `never` 的 approval 策略要给出的警告文案;`never` → None。
    ///
    /// 非默认策略会在 Codex 请求批准时暂停当前 turn，等待飞书会话中的 operator 裁决；
    /// 无人处理会超时取消。真实飞书 E2E 与灰度仍由 owner 决定，所以 doctor 保留显式提醒。
    pub fn approval_warning(&self) -> Option<String> {
        (self.approval_policy != DEFAULT_APPROVAL_POLICY)
            .then(|| summon_i18n::format!("r7bca7bbb5e", self.approval_policy))
    }

    /// sandbox 与 approval 的组合是否会让 agent「写不了又无法升级」。
    pub fn write_blocked_warning(&self) -> Option<String> {
        (self.sandbox == "read-only" && self.approval_policy == DEFAULT_APPROVAL_POLICY)
            .then(|| summon_i18n::text!("ra7e82b3ac0").to_string())
    }
}

/// 一次降级的记录 —— 调用方负责把它写进日志。
///
/// 单独返回而不是在这里 log:core 不做 IO(见 crate 边界约定),而且纯函数才好被单测钉住。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyFallback {
    pub field: &'static str,
    pub rejected: String,
}

/// 内置默认 → 全局 → agent,逐字段覆盖。
///
/// **非法值降级而不是报错**:这两个值最终进 `thread/start` 的 params,写错一个字母
/// (`read_only` vs `read-only`)会让整个会话起不来。而 `load/reread 永不 panic` 是仓里
/// 既有的分层约定(见 model/effort 那条:坏 effort 丢 effort 降级)。降级必须留痕,
/// 否则「我明明配了」就查不下去了 —— 所以把 [`PolicyFallback`] 一并返回。
pub fn resolve(
    global: Option<&CodexPolicyOverride>,
    agent_sandbox: Option<&str>,
    agent_approval: Option<&str>,
) -> (CodexPolicy, Vec<PolicyFallback>) {
    let mut fallbacks = Vec::new();
    let mut pick = |field: &'static str,
                    agent: Option<&str>,
                    global: Option<&str>,
                    allowed: &[&str],
                    default: &str|
     -> String {
        for candidate in [agent, global].into_iter().flatten() {
            let value = candidate.trim();
            if value.is_empty() {
                continue;
            }
            if allowed.contains(&value) {
                return value.to_string();
            }
            fallbacks.push(PolicyFallback {
                field,
                rejected: value.to_string(),
            });
        }
        default.to_string()
    };
    let sandbox = pick(
        "sandbox",
        agent_sandbox,
        global.and_then(|g| g.sandbox.as_deref()),
        SANDBOX_VALUES,
        DEFAULT_SANDBOX,
    );
    let approval_policy = pick(
        "approval_policy",
        agent_approval,
        global.and_then(|g| g.approval_policy.as_deref()),
        APPROVAL_VALUES,
        DEFAULT_APPROVAL_POLICY,
    );
    (
        CodexPolicy {
            sandbox,
            approval_policy,
        },
        fallbacks,
    )
}

/// `~/.summon/config.yaml` 的 `codex:` 段。两个键都可选,不写 = 用内置默认。
///
/// ```yaml
/// codex:
///   sandbox: workspace-write        # read-only | workspace-write | danger-full-access
///   approval_policy: never          # untrusted | on-request | never
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexPolicyOverride {
    pub sandbox: Option<String>,
    pub approval_policy: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn over(sandbox: Option<&str>, approval: Option<&str>) -> CodexPolicyOverride {
        CodexPolicyOverride {
            sandbox: sandbox.map(str::to_string),
            approval_policy: approval.map(str::to_string),
        }
    }

    #[test]
    fn nothing_configured_yields_the_builtin_defaults() {
        let (policy, fallbacks) = resolve(None, None, None);
        assert_eq!(policy.sandbox, DEFAULT_SANDBOX);
        assert_eq!(DEFAULT_APPROVAL_POLICY, "never");
        assert_eq!(policy.approval_policy, "never");
        assert!(fallbacks.is_empty());
    }

    #[test]
    fn global_overrides_the_builtin_and_agent_overrides_the_global() {
        let global = over(Some("workspace-write"), Some("on-request"));
        let (only_global, _) = resolve(Some(&global), None, None);
        assert_eq!(only_global.sandbox, "workspace-write");
        assert_eq!(only_global.approval_policy, "on-request");

        let (agent_wins, _) = resolve(Some(&global), Some("read-only"), Some("never"));
        assert_eq!(agent_wins.sandbox, "read-only");
        assert_eq!(agent_wins.approval_policy, "never");
    }

    /// 逐字段覆盖:agent 只配了一个,另一个仍该落到全局,而不是被一起重置成默认。
    #[test]
    fn each_field_falls_back_independently() {
        let global = over(Some("workspace-write"), Some("on-request"));
        let (policy, _) = resolve(Some(&global), Some("read-only"), None);
        assert_eq!(policy.sandbox, "read-only", "agent 覆盖生效");
        assert_eq!(
            policy.approval_policy, "on-request",
            "agent 没配这一项,不该把全局也一起丢掉"
        );
    }

    /// 非法值不能让会话起不来(它最终进 thread/start 的 params,写错一个字母整个 chat 就哑了),
    /// 但必须留痕 —— 静默降级会让「我明明配了」这件事查不下去。
    #[test]
    fn an_illegal_value_degrades_to_the_default_and_is_reported() {
        // 最容易犯的错:下划线写成连字符的反面。
        let (policy, fallbacks) = resolve(None, Some("read_only"), Some("Never"));
        assert_eq!(policy.sandbox, DEFAULT_SANDBOX);
        assert_eq!(policy.approval_policy, DEFAULT_APPROVAL_POLICY);
        assert_eq!(
            fallbacks,
            vec![
                PolicyFallback {
                    field: "sandbox",
                    rejected: "read_only".into()
                },
                PolicyFallback {
                    field: "approval_policy",
                    rejected: "Never".into()
                },
            ]
        );
    }

    /// agent 配了非法值时,应当继续往下找全局,而不是直接跳到内置默认 ——
    /// 否则一个错字会连带把管理员配好的全局策略一起废掉。
    #[test]
    fn an_illegal_agent_value_still_falls_through_to_the_global_one() {
        let global = over(Some("workspace-write"), None);
        let (policy, fallbacks) = resolve(Some(&global), Some("nonsense"), None);
        assert_eq!(policy.sandbox, "workspace-write");
        assert_eq!(fallbacks.len(), 1);
        assert_eq!(fallbacks[0].rejected, "nonsense");
    }

    #[test]
    fn blank_values_are_treated_as_unset_rather_than_illegal() {
        let (policy, fallbacks) = resolve(None, Some("   "), Some(""));
        assert_eq!(policy.sandbox, DEFAULT_SANDBOX);
        assert!(fallbacks.is_empty(), "空值是「没配」,不该报成非法值");
    }

    /// 每一个 schema 合法值都得能配上 —— 漏一个就是「文档说能配、实际被降级」。
    #[test]
    fn every_documented_value_is_accepted() {
        for value in SANDBOX_VALUES {
            let (policy, fallbacks) = resolve(None, Some(value), None);
            assert_eq!(policy.sandbox, *value);
            assert!(fallbacks.is_empty(), "{value} 被拒了");
        }
        for value in APPROVAL_VALUES {
            let (policy, fallbacks) = resolve(None, None, Some(value));
            assert_eq!(policy.approval_policy, *value);
            assert!(fallbacks.is_empty(), "{value} 被拒了");
        }
    }

    /// 非默认批准策略与「只读且不批准」都必须给出明确后果，否则用户配完看不出行为为何变化。
    #[test]
    fn consequential_policy_combinations_are_flagged() {
        let default = CodexPolicy::default();
        assert!(default.approval_warning().is_none());
        assert!(default.write_blocked_warning().is_none());

        let (asks, _) = resolve(None, None, Some("on-request"));
        let warning = asks.approval_warning().expect("非 never 必须警告");
        assert!(warning.contains("operator"), "实得:{warning}");
        assert!(warning.contains("超时取消"), "实得:{warning}");

        let (read_only, _) = resolve(None, Some("read-only"), None);
        assert!(
            read_only.write_blocked_warning().is_some(),
            "read-only + never = 写不了又无法升级,必须警告"
        );
    }
}
