use crate::driver::{LaunchModel, ModelEffortError};

/// Codex 每轮使用的服务档位。持久化值是稳定的用户语义,与 app-server 内部可能返回的
/// `priority` 名称解耦；只接受规范值,避免拼写错误静默回退到继承全局配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexServiceTier {
    Standard,
    Fast,
}

impl CodexServiceTier {
    /// 写入 chat 设置文件的规范值。
    pub const fn as_persisted_str(self) -> &'static str {
        match self {
            Self::Standard => "default",
            Self::Fast => "fast",
        }
    }

    /// Codex exec 级 config 覆盖；调用方必须把整段放在 `resume` 子命令前。
    pub(crate) const fn exec_args(self) -> &'static [&'static str] {
        match self {
            Self::Standard => &["-c", "service_tier=default"],
            Self::Fast => &["-c", "features.fast_mode=true", "-c", "service_tier=fast"],
        }
    }
}

impl std::fmt::Display for CodexServiceTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_persisted_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", summon_i18n::format!("r270136bdf7", value = .value))]
pub struct ParseCodexServiceTierError {
    value: String,
}

impl std::str::FromStr for CodexServiceTier {
    type Err = ParseCodexServiceTierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "default" => Ok(Self::Standard),
            "fast" => Ok(Self::Fast),
            _ => Err(ParseCodexServiceTierError {
                value: value.to_string(),
            }),
        }
    }
}

#[derive(serde::Deserialize)]
struct ModelCatalog {
    models: Vec<ModelCatalogEntry>,
}

#[derive(serde::Deserialize)]
struct ModelCatalogEntry {
    slug: String,
    #[serde(default)]
    service_tiers: Vec<ModelCatalogServiceTier>,
    #[serde(default)]
    additional_speed_tiers: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ModelCatalogServiceTier {
    id: String,
}

/// 从 Codex 本地 model catalog 判断指定模型是否支持 Fast。catalog 不可解析或找不到模型时
/// 返回 None；找到模型但未声明 priority/fast tier 时返回 Some(false)。
pub fn codex_model_supports_fast(catalog_json: &str, model: &str) -> Option<bool> {
    let catalog: ModelCatalog = serde_json::from_str(catalog_json).ok()?;
    let entry = catalog
        .models
        .into_iter()
        .find(|entry| entry.slug == model)?;
    Some(
        entry.service_tiers.iter().any(|tier| tier.id == "priority")
            || entry
                .additional_speed_tiers
                .iter()
                .any(|tier| tier == "fast"),
    )
}

/// codex 的独立 effort 合法集。取 CLI 侧 catalog 的**并集**(校 CLI 级,不追 per-model 漂移,
/// 见设计 §3.2):`~/.codex/models_cache.json`(client 0.146.0)里 gpt-5.6-sol/terra 公布到 `ultra`、
/// luna 到 `max`、gpt-5.5 及更早到 `xhigh`;CLI 另仍接受旧模型的 none/minimal。
/// 并集意味着「合法 ≠ 当前 model 一定支持」——per-model 不支持的档位由 codex 自己拒,这是刻意的
/// 取舍:summon 不复制会漂移的 per-model catalog。
pub const EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// codex:model→`-m <id>`;effort 无独立 flag,走 `-c model_reasoning_effort=<v>`(config 覆盖,
/// 值是 TOML 裸词无需引号)。注意 P3 拼 headless 时这些是 exec 级 flag,须排在 `resume` 子命令前。
pub fn resolve_launch(
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<LaunchModel, ModelEffortError> {
    let mut args = Vec::new();
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        if !EFFORTS.contains(&e) {
            return Err(ModelEffortError::InvalidEffort {
                cli: "codex",
                value: e.to_string(),
                allowed: EFFORTS.join("/"),
            });
        }
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={e}"));
    }
    Ok(LaunchModel { args })
}

/// codex `model_reasoning_summary` 合法值(none/concise/auto/detailed)。
pub const SUMMARIES: &[&str] = &["none", "concise", "auto", "detailed"];

/// 决定**注入哪个** `model_reasoning_summary`(None=不注入)。优先级:agent.yaml 显式且合法 >
/// 用户 `~/.codex/config.toml` 已声明(尊重之、不注入)> 兜底 `detailed`。
/// why:gpt-5.x 的 per-model 默认是 `model_reasoning_summary=none` → `exec --json` 一条 reasoning
/// 都不吐,summon 抓不到思考过程、双卡退化成单卡;故未声明时主动开 detailed(比 auto 更稳产非空
/// 摘要)。launch 期 fail-closed:agent 给了非法值就忽略它、继续往下走 config/default,绝不 panic。
pub fn reasoning_summary_decision(
    agent_setting: Option<&str>,
    user_config_has_summary: bool,
) -> Option<String> {
    if let Some(s) = agent_setting.map(str::trim).filter(|s| !s.is_empty()) {
        if SUMMARIES.contains(&s) {
            return Some(s.to_string());
        }
    }
    if user_config_has_summary {
        None
    } else {
        Some("detailed".to_string())
    }
}

/// 用户 `~/.codex/config.toml` 是否已声明 `model_reasoning_summary`(声明了就尊重、不注入)。
/// why 行扫而非引 toml 依赖:只需判断「是否出现该 key」,逐行剔注释行后比对左值即可,不必为这一个
/// 布尔判断给 summon-core 加 TOML 解析依赖。
pub fn config_declares_summary(toml_text: &str) -> bool {
    toml_text.lines().any(|l| {
        let t = l.trim_start();
        !t.starts_with('#') && t.split('=').next().map(str::trim) == Some("model_reasoning_summary")
    })
}

/// 从用户 `~/.codex/config.toml` 读 `model_context_window`(该模型的上下文窗口 token 数)。
///
/// why 行扫而非引 toml 依赖:同 [`config_declares_summary`],只取一个整数,不值得给 summon-core
/// 加 TOML 解析依赖。TOML 的整数允许下划线分隔(`1_000_000`),所以比对前先剔掉。
///
/// **读不出就是 None,绝不回退到猜测值** —— 上层 `ContextBudget` 拿到 None 会退化成「不显示
/// 百分比、不提醒」,那是诚实的;填一个猜的窗口会让百分比变成看起来精确、实际错误的数字。
pub fn parse_context_window(toml_text: &str) -> Option<u64> {
    toml_text.lines().find_map(|l| {
        let t = l.trim_start();
        if t.starts_with('#') {
            return None;
        }
        let (k, v) = t.split_once('=')?;
        if k.trim() != "model_context_window" {
            return None;
        }
        // 去行尾注释,再剔下划线分隔符。
        let v = v.split('#').next()?.trim().replace('_', "");
        // 0 视同未配置:除零在 ContextBudget 里已挡,但这里就地当「不知道」更贴语义。
        v.parse::<u64>().ok().filter(|&n| n > 0)
    })
}

pub fn default_cmd() -> &'static str {
    "codex"
}

pub fn resume_cmd(base: &str, id: &str) -> String {
    format!("{base} resume {id}")
}

/// 无头一轮的完整参数(不含 `codex` 程序名,prompt 是末位位置参数)。
/// 冷启:`exec --json [exec_args] -- <prompt>`;resume:`exec --json [exec_args] resume <id> -- <prompt>`。
/// `--json` 与 `exec_args`(`-m`/`-c`,含 model/effort/summary/service tier)都是 exec 级 flag,
/// 必须在 `resume` 子命令前(codex CLI 解析要求)；`--` 截断选项解析，保证以 `-` 开头的
/// 用户文本不会被误当成 CLI flag。
pub fn headless_command_args(
    prompt: &str,
    resume: Option<&str>,
    model_args: &[String],
) -> Vec<String> {
    let mut a = vec!["exec".to_string(), "--json".to_string()];
    a.extend_from_slice(model_args);
    if let Some(id) = resume {
        a.push("resume".to_string());
        a.push(id.to_string());
    }
    a.push("--".to_string());
    a.push(prompt.to_string());
    a
}

pub fn interrupts_followup() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_catalog_detects_fast_in_current_and_legacy_fields() {
        let current = r#"{
            "models": [
                {"slug":"gpt-fast","service_tiers":[{"id":"priority"}]},
                {"slug":"gpt-standard","service_tiers":[]}
            ]
        }"#;
        assert_eq!(codex_model_supports_fast(current, "gpt-fast"), Some(true));
        assert_eq!(
            codex_model_supports_fast(current, "gpt-standard"),
            Some(false)
        );

        let legacy = r#"{
            "models": [
                {"slug":"gpt-fast","additional_speed_tiers":["fast"]}
            ]
        }"#;
        assert_eq!(codex_model_supports_fast(legacy, "gpt-fast"), Some(true));
    }

    #[test]
    fn model_catalog_returns_none_for_invalid_json_or_missing_model() {
        assert_eq!(codex_model_supports_fast("not json", "gpt-fast"), None);
        assert_eq!(
            codex_model_supports_fast(r#"{"models":[{"slug":"other"}]}"#, "gpt-fast"),
            None
        );
        assert_eq!(
            codex_model_supports_fast(r#"{"models":[{"slug":"gpt-standard"}]}"#, "gpt-standard"),
            Some(false)
        );
    }

    /// 真机已验摆法(codex-cli 0.137.0,2026-06-07):`-m`/`-c` 是 exec 级 flag,排在 `exec` 之后、
    /// prompt 末位。本测把这个 argv 形状钉死;端到端真机冒烟见 summon crate 的
    /// `real_codex_effort_subprocess_replies`(`#[ignore]`)。
    #[test]
    fn cold_args_put_model_and_effort_after_exec_before_prompt() {
        let model_args = resolve_launch(Some("gpt-5.5"), Some("high")).unwrap().args;
        let got = headless_command_args("数到 3", None, &model_args);
        let want = [
            "exec",
            "--json",
            "-m",
            "gpt-5.5",
            "-c",
            "model_reasoning_effort=high",
            "--",
            "数到 3",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn resume_args_keep_model_effort_before_resume_subcommand() {
        let model_args = resolve_launch(Some("gpt-5.5"), Some("high")).unwrap().args;
        let got = headless_command_args("继续", Some("th_abc"), &model_args);
        let want = [
            "exec",
            "--json",
            "-m",
            "gpt-5.5",
            "-c",
            "model_reasoning_effort=high",
            "resume",
            "th_abc",
            "--",
            "继续",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn effort_only_and_model_only_and_invalid_effort() {
        // effort-only(用 CLI 默认 model)+ model-only 都合法。
        assert_eq!(
            headless_command_args("x", None, &resolve_launch(None, Some("high")).unwrap().args),
            [
                "exec",
                "--json",
                "-c",
                "model_reasoning_effort=high",
                "--",
                "x"
            ]
        );
        assert_eq!(
            headless_command_args(
                "x",
                None,
                &resolve_launch(Some("gpt-5.5"), None).unwrap().args
            ),
            ["exec", "--json", "-m", "gpt-5.5", "--", "x"]
        );
        assert!(resolve_launch(Some("gpt-5.6-sol"), Some("max")).is_ok());
        // ultra:sol/terra 的 catalog 公布了它,合法集必须收(否则面板/`/effort ultra` 被误拒)。
        assert!(resolve_launch(Some("gpt-5.6-sol"), Some("ultra")).is_ok());
        assert!(resolve_launch(Some("gpt-5.6-sol"), Some("warp")).is_err());
    }

    #[test]
    fn leading_dash_prompt_is_after_option_terminator() {
        assert_eq!(
            headless_command_args("--help", None, &[]),
            ["exec", "--json", "--", "--help"]
        );
        assert_eq!(
            headless_command_args("-继续", Some("th_abc"), &[]),
            ["exec", "--json", "resume", "th_abc", "--", "-继续"]
        );
    }

    #[test]
    fn summary_decision_precedence() {
        // agent 显式且合法 → 用它(压过 config/default)
        assert_eq!(
            reasoning_summary_decision(Some("concise"), true),
            Some("concise".to_string())
        );
        // agent 非法 → 忽略,落到 config/default 这一档
        assert_eq!(
            reasoning_summary_decision(Some("bogus"), true),
            None // config 已声明 → 尊重、不注入
        );
        assert_eq!(
            reasoning_summary_decision(Some("bogus"), false),
            Some("detailed".to_string()) // config 未声明 → 兜底 detailed
        );
        // agent 未设 + config 已声明 → None(尊重用户)
        assert_eq!(reasoning_summary_decision(None, true), None);
        // agent 未设 + config 未声明 → 兜底 detailed
        assert_eq!(
            reasoning_summary_decision(None, false),
            Some("detailed".to_string())
        );
        // 空字符串视同未设
        assert_eq!(
            reasoning_summary_decision(Some("  "), false),
            Some("detailed".to_string())
        );
    }

    #[test]
    fn parse_context_window_reads_plain_and_underscored() {
        assert_eq!(
            parse_context_window("model_context_window = 1000000"),
            Some(1_000_000)
        );
        assert_eq!(
            parse_context_window("model_context_window = 1_000_000"),
            Some(1_000_000)
        );
        // 行尾注释、缩进都不影响
        assert_eq!(
            parse_context_window("  model_context_window = 256000  # 小窗口"),
            Some(256_000)
        );
        // 真实配置形态:多行里挑出那一行
        let real = "model = \"gpt-5.6\"\nmodel_context_window = 1000000\nmodel_auto_compact_token_limit = 900000\n";
        assert_eq!(parse_context_window(real), Some(1_000_000));
    }

    #[test]
    fn parse_context_window_is_none_when_absent_commented_or_zero() {
        assert_eq!(parse_context_window(""), None);
        assert_eq!(
            parse_context_window("# model_context_window = 1000000"),
            None
        );
        assert_eq!(parse_context_window("model_context_window = 0"), None);
        assert_eq!(parse_context_window("model_context_window = abc"), None);
        // 别把前缀相同的另一个 key 认成它
        assert_eq!(parse_context_window("model_context_window_extra = 5"), None);
    }

    #[test]
    fn config_declares_summary_detects_key() {
        assert!(config_declares_summary(
            "model_reasoning_summary = \"auto\""
        ));
        // 注释掉的不算声明
        assert!(!config_declares_summary(
            "# model_reasoning_summary = \"x\""
        ));
        // 完全不出现
        assert!(!config_declares_summary(
            "model = \"gpt-5.5\"\nmodel_reasoning_effort = \"high\""
        ));
    }
}
