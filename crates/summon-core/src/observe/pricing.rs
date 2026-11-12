//! 按公开价格表给运行记录里的 token 计价。纯计算。
//!
//! 价格表取自 LiteLLM 的 `model_prices_and_context_window.json`:它收录 Anthropic 与 OpenAI 直连的
//! 单价,含缓存读取、5 分钟与 1 小时缓存写入、长上下文分档与 priority 等服务档,字段名是
//! `<基础>[_above_1hr][_above_<N>k_tokens][_<服务档>]`。计价在读取时做:新模型往往要晚几天才收录,
//! 写入时计价会让这几天的轮永远没有费用。
//!
//! 价格表里缺的不猜:模型认不出,或某一项有 token 而没有对应单价,这一组请求记为未计价,token 照计。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::turn_ledger::ModelUsage;

/// 价格表的来源。
pub const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

const INPUT: &str = "input_cost_per_token";
const OUTPUT: &str = "output_cost_per_token";
const CACHE_READ: &str = "cache_read_input_token_cost";
const CACHE_WRITE: &str = "cache_creation_input_token_cost";
const CACHE_WRITE_1H: &str = "cache_creation_input_token_cost_above_1hr";

/// 盘上那份价格表:只留不带 `/` 前缀的条目里的数值单价(美元 / token)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PriceTable {
    pub source: String,
    pub fetched_at_ms: i64,
    pub models: BTreeMap<String, BTreeMap<String, f64>>,
}

impl PriceTable {
    /// 从 LiteLLM 的原文取出计价用得上的部分。一个带输入单价的模型都没有、或有负的单价,就当拉坏了
    /// (比如拿到的是错误页),不能拿它顶掉盘上那份好的。
    pub fn from_litellm(raw: &str, source: &str, fetched_at_ms: i64) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| summon_i18n::format!("rc9ac201c06"))?;
        let entries = value.as_object().ok_or(summon_i18n::text!("r4f31229c45"))?;
        let models: BTreeMap<String, BTreeMap<String, f64>> = entries
            .iter()
            .filter(|(key, _)| !key.contains('/'))
            .filter_map(|(key, entry)| {
                let prices: BTreeMap<String, f64> = entry
                    .as_object()?
                    .iter()
                    .filter(|(field, _)| field.contains("cost"))
                    .filter_map(|(field, v)| Some((field.clone(), v.as_f64()?)))
                    .collect();
                prices.contains_key(INPUT).then(|| (key.clone(), prices))
            })
            .collect();
        if models.is_empty() {
            return Err(summon_i18n::text!("r04cd33857c").into());
        }
        for (model, prices) in &models {
            if let Some((field, price)) = prices.iter().find(|(_, p)| **p < 0.0) {
                return Err(summon_i18n::format!("r50a308a690"));
            }
        }
        Ok(Self {
            source: source.to_string(),
            fetched_at_ms,
            models,
        })
    }

    /// 能不能顶掉盘上那份:模型数掉到旧表一半以下,多半是上游表被截断或改了格式,宁可留着旧的。
    pub fn can_replace(&self, old: Option<&PriceTable>) -> Result<(), String> {
        match old {
            Some(old) if self.models.len() * 2 < old.models.len() => Err(summon_i18n::format!(
                "r3c8f1663b2",
                self.models.len(),
                old.models.len()
            )),
            _ => Ok(()),
        }
    }

    /// 模型 id 对应的单价:先原样找,再去掉 `[…]` 参数段与日期后缀找。
    fn entry(&self, model: &str) -> Option<&BTreeMap<String, f64>> {
        model_candidates(model)
            .into_iter()
            .find_map(|m| self.models.get(&m))
    }

    /// 一组请求的费用。认不出模型、或有 token 的某一项没有单价时为 `None`。
    pub fn price(&self, usage: &ModelUsage) -> Option<Cost> {
        let fields = self.entry(&usage.model)?;
        let tier = tier_suffix(fields, usage.band);
        let variant = usage
            .variant
            .as_deref()
            .or_else(|| param_variant(&usage.model))
            .map(|v| format!("_{v}"))
            .unwrap_or_default();
        let rate = |base: &str, tokens: u64| -> Option<f64> {
            if tokens == 0 {
                return Some(0.0);
            }
            let price = fields.get(&format!("{base}{tier}{variant}"))?;
            Some(price * tokens as f64)
        };
        let input = rate(INPUT, usage.input)?;
        let output = rate(OUTPUT, usage.output)?;
        let cache_read = rate(CACHE_READ, usage.cache_read)?;
        let cache_write =
            rate(CACHE_WRITE, usage.cache_write_5m)? + rate(CACHE_WRITE_1H, usage.cache_write_1h)?;
        Some(Cost {
            total: input + output + cache_read + cache_write,
            input,
            output,
            cache_read,
            cache_write,
        })
    }
}

/// 模型 id 参数段里声明的服务档:cursor 的 `…[fast=true]` 是快速档,与标准档不同价。
fn param_variant(model: &str) -> Option<&'static str> {
    let params = model.split_once('[')?.1.split(']').next()?;
    params
        .split(',')
        .any(|p| p.trim() == "fast=true")
        .then_some("fast")
}

/// 找单价时依次试的写法。日期后缀能去掉,是因为同一模型的带日期 id 与不带日期的别名同价(价格表
/// 里两者并列、单价相同,只是收录的写法不全)。
fn model_candidates(model: &str) -> Vec<String> {
    let mut out = vec![model.to_string()];
    let mut push = |m: &str| {
        if !m.is_empty() && !out.iter().any(|seen| seen == m) {
            out.push(m.to_string());
        }
    };
    // cursor 的 `claude-opus-5-5[context=1m,…]`、claude 的 `[1m]`。
    let bare = model.split('[').next().unwrap_or(model).trim();
    push(bare);
    push(strip_date_suffix(bare));
    out
}

/// 去掉 `-20251101` 或 `-2026-04-23` 这样的日期后缀。
fn strip_date_suffix(model: &str) -> &str {
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    if let Some((head, tail)) = model.rsplit_once('-') {
        if digits(tail, 8) {
            return head;
        }
    }
    let parts: Vec<&str> = model.rsplitn(4, '-').collect();
    if parts.len() == 4 && digits(parts[0], 2) && digits(parts[1], 2) && digits(parts[2], 4) {
        return parts[3];
    }
    model
}

/// 这一档该用哪个长上下文后缀:模型有的门槛里不超过档位的最大那个;没有就是空串。
fn tier_suffix(fields: &BTreeMap<String, f64>, band: u32) -> String {
    if band == 0 {
        return String::new();
    }
    fields
        .keys()
        .filter_map(|field| {
            let (_, rest) = field.split_once("_above_")?;
            let rest = rest.strip_prefix("1hr_above_").unwrap_or(rest);
            rest.split_once("k_tokens")?.0.parse::<u32>().ok()
        })
        .filter(|k| *k <= band)
        .max()
        .map(|k| format!("_above_{k}k_tokens"))
        .unwrap_or_default()
}

/// 四项费用(美元)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub total: f64,
    /// 未命中缓存的输入。
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Cost {
    fn add(&mut self, other: &Cost) {
        self.total += other.total;
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }
}

/// 四项 token;`cache_write` 是两档缓存写入之和。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    /// 未命中缓存的输入。
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// 没能计价的那部分。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unpriced {
    pub tokens: u64,
    /// 涉及的模型 id(去重、排序);CLI 没报模型的记为空串。
    pub models: Vec<String>,
}

/// 一组轮的 token 与费用。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Spend {
    pub tokens: TokenTotals,
    /// 计了价的那部分的费用。
    pub cost: Cost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpriced: Option<Unpriced>,
}

/// 一组请求的 token 与费用;一组都没有就没有。没有价格表时全部记为未计价。
pub fn spend<'a>(
    table: Option<&PriceTable>,
    usage: impl IntoIterator<Item = &'a ModelUsage>,
) -> Option<Spend> {
    let mut out = Spend::default();
    let mut any = false;
    let mut unpriced_tokens = 0;
    let mut unpriced_models = BTreeSet::new();
    for u in usage {
        any = true;
        out.tokens.input += u.input;
        out.tokens.output += u.output;
        out.tokens.cache_read += u.cache_read;
        out.tokens.cache_write += u.cache_write_5m + u.cache_write_1h;
        match table.and_then(|t| t.price(u)) {
            Some(cost) => out.cost.add(&cost),
            None => {
                unpriced_tokens += u.tokens();
                unpriced_models.insert(u.model.clone());
            }
        }
    }
    if !any {
        return None;
    }
    if !unpriced_models.is_empty() {
        out.unpriced = Some(Unpriced {
            tokens: unpriced_tokens,
            models: unpriced_models.into_iter().collect(),
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 摘自 LiteLLM 表的形状(数值为合成值):带 1 小时缓存与 100k 分档的 claude、带 272k 分档与
    /// priority 档的 OpenAI 模型、带 provider 前缀的条目、没有输入单价的条目。
    const RAW: &str = r#"{
        "sample_spec": {"mode": "chat", "input_cost_per_token": "doc"},
        "claude-test-1": {
            "litellm_provider": "anthropic",
            "input_cost_per_token": 4e-06, "output_cost_per_token": 2e-05,
            "cache_read_input_token_cost": 2e-07, "cache_creation_input_token_cost": 5e-06,
            "cache_creation_input_token_cost_above_1hr": 8e-06,
            "input_cost_per_token_above_100k_tokens": 8e-06,
            "output_cost_per_token_above_100k_tokens": 3e-05,
            "cache_read_input_token_cost_above_100k_tokens": 4e-07,
            "cache_creation_input_token_cost_above_100k_tokens": 1e-05,
            "cache_creation_input_token_cost_above_1hr_above_100k_tokens": 1.6e-05,
            "search_context_cost_per_query": {"search_context_size_low": 0.01}
        },
        "gpt-test-1": {
            "litellm_provider": "openai",
            "input_cost_per_token": 2e-06, "output_cost_per_token": 1e-05,
            "cache_read_input_token_cost": 1e-07,
            "input_cost_per_token_above_272k_tokens": 4e-06,
            "output_cost_per_token_above_272k_tokens": 1.5e-05,
            "cache_read_input_token_cost_above_272k_tokens": 2e-07,
            "input_cost_per_token_priority": 4e-06,
            "output_cost_per_token_priority": 2e-05,
            "cache_read_input_token_cost_priority": 2e-07
        },
        "azure/gpt-test-1": {"input_cost_per_token": 9e-06},
        "embedding-only": {"output_vector_size": 3}
    }"#;

    fn table() -> PriceTable {
        PriceTable::from_litellm(RAW, LITELLM_URL, 7).unwrap()
    }

    fn usage(model: &str, band: u32) -> ModelUsage {
        ModelUsage {
            model: model.into(),
            band,
            variant: None,
            requests: 1,
            input: 1_000,
            output: 100,
            cache_read: 10_000,
            cache_write_5m: 0,
            cache_write_1h: 0,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn the_table_keeps_direct_entries_with_numeric_prices() {
        let t = table();
        assert_eq!(
            t.models.keys().map(String::as_str).collect::<Vec<_>>(),
            ["claude-test-1", "gpt-test-1"]
        );
        assert!(!t.models["claude-test-1"].contains_key("search_context_cost_per_query"));
        assert_eq!(t.fetched_at_ms, 7);
        assert!(PriceTable::from_litellm("{}", LITELLM_URL, 0).is_err());
        assert!(PriceTable::from_litellm("<html>", LITELLM_URL, 0).is_err());
        let negative = RAW.replace(
            "\"output_cost_per_token\": 1e-05",
            "\"output_cost_per_token\": -1e-05",
        );
        assert_ne!(negative, RAW);
        assert!(PriceTable::from_litellm(&negative, LITELLM_URL, 0).is_err());
        let back: PriceTable = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn a_table_with_less_than_half_the_models_does_not_replace_the_old_one() {
        let old = table();
        let mut half = old.clone();
        half.models.pop_last();
        assert!(half.can_replace(Some(&old)).is_ok());
        half.models.pop_last();
        assert!(half.can_replace(Some(&old)).is_err());
        assert!(half.can_replace(None).is_ok());
    }

    #[test]
    fn base_prices_cover_the_four_parts_with_both_cache_lifetimes() {
        let mut u = usage("claude-test-1", 0);
        u.cache_write_5m = 2_000;
        u.cache_write_1h = 3_000;
        let c = table().price(&u).unwrap();
        assert!(close(c.input, 1_000.0 * 4e-6));
        assert!(close(c.output, 100.0 * 2e-5));
        assert!(close(c.cache_read, 10_000.0 * 2e-7));
        assert!(close(c.cache_write, 2_000.0 * 5e-6 + 3_000.0 * 8e-6));
        assert!(close(
            c.total,
            c.input + c.output + c.cache_read + c.cache_write
        ));
    }

    #[test]
    fn long_prompts_use_the_largest_threshold_at_or_below_their_band() {
        let mut u = usage("claude-test-1", 272);
        u.cache_write_1h = 1_000;
        let c = table().price(&u).unwrap();
        assert!(close(c.input, 1_000.0 * 8e-6));
        assert!(close(c.cache_write, 1_000.0 * 1.6e-5));
        // 272k 分档的模型在 200 档还是基础价
        let c = table().price(&usage("gpt-test-1", 200)).unwrap();
        assert!(close(c.input, 1_000.0 * 2e-6));
        let c = table().price(&usage("gpt-test-1", 272)).unwrap();
        assert!(close(c.output, 100.0 * 1.5e-5));
    }

    #[test]
    fn service_tiers_use_their_own_prices_and_missing_ones_are_unpriced() {
        let mut u = usage("gpt-test-1", 0);
        u.variant = Some("priority".into());
        let c = table().price(&u).unwrap();
        assert!(close(c.input, 1_000.0 * 4e-6));
        // 272k 档没有 priority 价
        u.band = 272;
        assert_eq!(table().price(&u), None);
        let mut fast = usage("claude-test-1", 0);
        fast.variant = Some("fast".into());
        assert_eq!(table().price(&fast), None);
        // cursor 在模型 id 的参数段里声明快速档:表里没有快速档单价,不按标准价猜
        assert_eq!(
            table().price(&usage("claude-test-1[context=1m,effort=high,fast=true]", 0)),
            None
        );
        assert!(table()
            .price(&usage(
                "claude-test-1[context=1m,effort=high,fast=false]",
                0
            ))
            .is_some());
    }

    #[test]
    fn a_part_with_tokens_but_no_price_leaves_the_group_unpriced() {
        let mut u = usage("gpt-test-1", 0);
        u.cache_write_1h = 1;
        assert_eq!(table().price(&u), None);
        // 没有 token 的那一项不需要单价:这个模型没有缓存写入价
        assert!(table().price(&usage("gpt-test-1", 0)).is_some());
    }

    #[test]
    fn model_ids_match_after_dropping_parameters_and_dates() {
        let t = table();
        for model in [
            "claude-test-1",
            "claude-test-1[1m]",
            "claude-test-1[context=1m,effort=high,fast=false]",
            "claude-test-1-20261001",
            "gpt-test-1-2026-09-29",
        ] {
            assert!(t.price(&usage(model, 0)).is_some(), "{model}");
        }
        for model in ["claude-test", "gpt-test-1-sol", "", "azure/gpt-test-1"] {
            assert_eq!(t.price(&usage(model, 0)), None, "{model}");
        }
    }

    #[test]
    fn spend_sums_tokens_and_reports_what_could_not_be_priced() {
        let t = table();
        let mut unknown = usage("composer-x", 0);
        unknown.cache_write_5m = 5;
        let groups = [usage("claude-test-1", 0), unknown.clone(), usage("", 0)];
        let s = spend(Some(&t), &groups).unwrap();
        assert_eq!(s.tokens.input, 3_000);
        assert_eq!(s.tokens.cache_write, 5);
        let priced = t.price(&groups[0]).unwrap();
        assert!(close(s.cost.total, priced.total));
        let unpriced = s.unpriced.unwrap();
        assert_eq!(unpriced.tokens, unknown.tokens() + groups[2].tokens());
        assert_eq!(unpriced.models, ["", "composer-x"]);
        // 没有价格表:token 照计,全部未计价
        let none = spend(None, &groups[..1]).unwrap();
        assert_eq!(none.cost, Cost::default());
        assert_eq!(none.unpriced.unwrap().tokens, groups[0].tokens());
        assert_eq!(spend(Some(&t), &[]), None);
    }
}
