//! 洞察分析的纯逻辑:主 agent 的解析、交给模型的提示词、结果缓存键、结果文本的上限。
//!
//! 分析只把一段时间的洞察汇总(计数、时长与比例)连同固定提示词交给主 agent 的模型,跑一次、取输出。
//! 起进程、落盘与判权在 `summon` 侧;这里不碰文件与进程。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::driver::CliDriver;
use crate::i18n::Language;

/// 提示词的版本:改了提示词就加一,旧结果随缓存键一起失效。
pub const PROMPT_VERSION: u32 = 1;

/// 结果文本至多这么多个字符,超出的截掉并标记。
pub const MAX_TEXT_CHARS: usize = 6_000;

/// 「用量最多」看最近这么多天。
pub const USAGE_WINDOW_DAYS: i64 = 30;

const PROMPT_ZH: &str =
    include_str!("../../../../locales/assets/zh-CN/insights/analysis-prompt.md");
const PROMPT_EN: &str = include_str!("../../../../locales/assets/en/insights/analysis-prompt.md");

/// 主 agent 是怎么定下来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimarySource {
    /// 整机配置 `primary_agent` 显式指定。
    Configured,
    /// 近 [`USAGE_WINDOW_DAYS`] 天 token 用量最多。
    Usage,
    /// 还没有用量:最早配置的那个。
    First,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Primary {
    pub alias: String,
    pub source: PrimarySource,
}

/// 参与解析的一个 agent。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate<'a> {
    pub alias: &'a str,
    /// 能代表这台机器跑模型:常规 agent、在用、没被停用。不可用的不参与任何一条路径。
    pub usable: bool,
    /// 配置时刻(agent 目录的创建时刻,毫秒);拿不到为 `None`,排在拿得到的之后。
    pub created_ms: Option<i64>,
}

/// 主 agent:配置显式指定且可用 → 用量最多 → 最早配置。同用量、同时刻按别名排。没有可用的 agent 为 `None`。
pub fn resolve_primary(
    configured: Option<&str>,
    agents: &[Candidate<'_>],
    tokens: &BTreeMap<String, u64>,
) -> Option<Primary> {
    let usable: Vec<&Candidate<'_>> = agents.iter().filter(|a| a.usable).collect();
    let pick = |alias: &str, source| {
        Some(Primary {
            alias: alias.to_string(),
            source,
        })
    };
    if let Some(name) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        if usable.iter().any(|a| a.alias == name) {
            return pick(name, PrimarySource::Configured);
        }
    }
    fn earliest<'a>(a: &Candidate<'a>) -> (i64, &'a str) {
        (a.created_ms.unwrap_or(i64::MAX), a.alias)
    }
    let busiest = usable
        .iter()
        .filter_map(|a| {
            let used = tokens.get(a.alias).copied().unwrap_or(0);
            (used > 0).then_some((used, *a))
        })
        .min_by(|(x, a), (y, b)| y.cmp(x).then_with(|| earliest(a).cmp(&earliest(b))));
    if let Some((_, a)) = busiest {
        return pick(a.alias, PrimarySource::Usage);
    }
    usable
        .iter()
        .min_by_key(|a| earliest(a))
        .and_then(|a| pick(a.alias, PrimarySource::First))
}

/// 主 agent 的 CLI 为什么不能跑一次性分析。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// cursor 的 Ask / 一次性运行关不掉用户、企业与团队的 hooks。
    Cursor,
    /// 不认识的 CLI。
    UnknownCli,
}

/// claude 走 `-p` 一次性运行,codex / traex 走 app-server 的临时只读 thread;其余不支持。
pub fn support(driver: CliDriver) -> Result<(), Unsupported> {
    match driver {
        CliDriver::Claude | CliDriver::Codex | CliDriver::Traex => Ok(()),
        CliDriver::Cursor => Err(Unsupported::Cursor),
        CliDriver::Other => Err(Unsupported::UnknownCli),
    }
}

/// 固定提示词。
pub fn prompt(lang: Language) -> &'static str {
    match lang {
        Language::Chinese => PROMPT_ZH,
        Language::English => PROMPT_EN,
    }
}

/// 交给模型的全部输入:固定提示词,后面跟洞察汇总的 JSON。
pub fn compose(lang: Language, summary_json: &str) -> String {
    format!(
        "{}\n```json\n{}\n```\n",
        prompt(lang).trim_end(),
        summary_json
    )
}

/// 结果缓存键的组成。同键的分析只跑一次。
///
/// 前四项(时间段、筛选、语言)定下结果所在的「槽」:页面按槽取结果,一个槽同一时刻最多一个分析在跑。
/// 其余几项只用来判新旧 —— 时间段含今天时每结束一轮汇总就变,按完整键取结果会让在跑的分析与刚生成的
/// 结果一转眼就「找不到」;槽里的结果与此刻的完整键不同就是过期了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    /// 时间段(本机日历,含两端)。
    pub from: String,
    pub to: String,
    /// 洞察页的 agent 筛选。
    pub agent_filter: Option<String>,
    /// 汇总 JSON 的指纹:同一时间段里有了新的运行记录,汇总变了,就是另一份分析。
    pub data: String,
    pub prompt_version: u32,
    pub lang: Language,
    /// 跑分析的主 agent 与模型(没配模型时为 CLI 默认,记 `None`)。
    pub primary: String,
    pub model: Option<String>,
}

impl CacheKey {
    /// 字段顺序固定的序列化,供调用方取哈希。
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 槽的序列化:只有时间段、筛选与语言。
    pub fn slot_canonical(&self) -> String {
        serde_json::to_string(&(&self.from, &self.to, &self.agent_filter, self.lang))
            .unwrap_or_default()
    }
}

/// 截到 [`MAX_TEXT_CHARS`] 个字符;截了就在末尾加省略号并返回 `true`。
pub fn clip(text: &str) -> (String, bool) {
    let text = text.trim();
    match text.char_indices().nth(MAX_TEXT_CHARS) {
        None => (text.to_string(), false),
        Some((at, _)) => (format!("{}…", text[..at].trim_end()), true),
    }
}

/// 一次分析的 token 用量(CLI 输出里有才记)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl AnalysisUsage {
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(alias: &str, usable: bool, created_ms: Option<i64>) -> Candidate<'_> {
        Candidate {
            alias,
            usable,
            created_ms,
        }
    }

    fn tokens(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(a, n)| (a.to_string(), *n)).collect()
    }

    fn agents() -> Vec<Candidate<'static>> {
        vec![
            candidate("alpha", true, Some(300)),
            candidate("beta", true, Some(100)),
            candidate("gamma", true, None),
            candidate("delta", false, Some(50)),
        ]
    }

    #[test]
    fn configured_agent_wins_when_it_exists_and_is_usable() {
        let used = tokens(&[("alpha", 900)]);
        assert_eq!(
            resolve_primary(Some(" gamma "), &agents(), &used),
            Some(Primary {
                alias: "gamma".into(),
                source: PrimarySource::Configured
            })
        );
    }

    #[test]
    fn configured_agent_that_is_missing_or_unusable_falls_back() {
        let used = tokens(&[("alpha", 900), ("delta", 5_000)]);
        for configured in [Some("missing"), Some("delta"), Some(""), None] {
            assert_eq!(
                resolve_primary(configured, &agents(), &used),
                Some(Primary {
                    alias: "alpha".into(),
                    source: PrimarySource::Usage
                }),
                "{configured:?}"
            );
        }
        assert_eq!(
            resolve_primary(Some("missing"), &agents(), &BTreeMap::new()).map(|p| p.source),
            Some(PrimarySource::First)
        );
    }

    #[test]
    fn most_used_agent_wins_and_ties_go_to_the_earliest() {
        let used = tokens(&[("alpha", 10), ("beta", 10), ("gamma", 9), ("delta", 99)]);
        assert_eq!(
            resolve_primary(None, &agents(), &used).unwrap().alias,
            "beta",
            "同用量取更早配置的;停用的不算"
        );
        let used = tokens(&[("gamma", 11), ("beta", 10)]);
        assert_eq!(
            resolve_primary(None, &agents(), &used).unwrap().alias,
            "gamma"
        );
    }

    #[test]
    fn without_usage_the_earliest_configured_agent_wins() {
        let used = tokens(&[("alpha", 0), ("delta", 7)]);
        let primary = resolve_primary(None, &agents(), &used).unwrap();
        assert_eq!(
            (primary.alias.as_str(), primary.source),
            ("beta", PrimarySource::First)
        );
        // 拿不到配置时刻的排在后面,彼此按别名
        let unknown = [candidate("zeta", true, None), candidate("eta", true, None)];
        assert_eq!(
            resolve_primary(None, &unknown, &BTreeMap::new())
                .unwrap()
                .alias,
            "eta"
        );
        assert_eq!(
            resolve_primary(None, &[candidate("delta", false, Some(1))], &used),
            None
        );
    }

    #[test]
    fn only_one_shot_capable_clis_are_supported() {
        assert!(support(CliDriver::Claude).is_ok());
        assert!(support(CliDriver::Codex).is_ok());
        assert!(support(CliDriver::Traex).is_ok());
        assert_eq!(support(CliDriver::Cursor), Err(Unsupported::Cursor));
        assert_eq!(support(CliDriver::Other), Err(Unsupported::UnknownCli));
    }

    #[test]
    fn prompts_follow_the_language_and_carry_only_the_summary() {
        let zh = compose(Language::Chinese, "{\"turns\":3}");
        let en = compose(Language::English, "{\"turns\":3}");
        assert!(zh.starts_with(prompt(Language::Chinese).trim_end()));
        assert!(en.starts_with(prompt(Language::English).trim_end()));
        assert!(zh.contains("简体中文") && en.contains("English"));
        for text in [&zh, &en] {
            assert!(text.ends_with("```json\n{\"turns\":3}\n```\n"));
        }
    }

    #[test]
    fn long_text_is_clipped_on_a_character_boundary() {
        let (short, cut) = clip("  简短的分析  ");
        assert_eq!((short.as_str(), cut), ("简短的分析", false));
        let long = "间".repeat(MAX_TEXT_CHARS + 10);
        let (clipped, cut) = clip(&long);
        assert!(cut);
        assert_eq!(clipped.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(clipped.ends_with('…'));
        let exact = "x".repeat(MAX_TEXT_CHARS);
        assert_eq!(clip(&exact), (exact.clone(), false));
    }

    #[test]
    fn every_part_changes_the_key_and_only_period_filter_and_language_change_the_slot() {
        let base = CacheKey {
            from: "2026-10-01".into(),
            to: "2026-10-30".into(),
            agent_filter: None,
            data: "d".into(),
            prompt_version: PROMPT_VERSION,
            lang: Language::Chinese,
            primary: "alpha".into(),
            model: None,
        };
        let variants: Vec<CacheKey> = vec![
            CacheKey {
                from: "2026-10-02".into(),
                ..base.clone()
            },
            CacheKey {
                to: "2026-10-29".into(),
                ..base.clone()
            },
            CacheKey {
                agent_filter: Some("alpha".into()),
                ..base.clone()
            },
            CacheKey {
                data: "e".into(),
                ..base.clone()
            },
            CacheKey {
                prompt_version: PROMPT_VERSION + 1,
                ..base.clone()
            },
            CacheKey {
                lang: Language::English,
                ..base.clone()
            },
            CacheKey {
                primary: "beta".into(),
                ..base.clone()
            },
            CacheKey {
                model: Some("m".into()),
                ..base.clone()
            },
        ];
        // 前四项换槽;其余四项同槽、只换完整键(判新旧)
        for (i, v) in variants.iter().enumerate() {
            assert_ne!(v.canonical(), base.canonical(), "{v:?}");
            let same_slot = v.slot_canonical() == base.slot_canonical();
            let slot_part = matches!(i, 0 | 1 | 2 | 5);
            assert_eq!(same_slot, !slot_part, "{v:?}");
        }
        assert_eq!(base.canonical(), base.clone().canonical());
        assert_eq!(base.slot_canonical(), base.clone().slot_canonical());
    }
}
