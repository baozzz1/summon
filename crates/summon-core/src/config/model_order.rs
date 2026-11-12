//! Cursor 模型目录的展示顺序。
//!
//! `cursor-agent models` 与 ACP `model` 轴都按服务端顺序给出上百个 id,同一模型的各档位、
//! `-fast` 变体与新旧版本混在一起,面板只放得下开头一截时,人最常用的那几个往往排不进来。
//! 这里把目录重排成:`auto` → 每个家族最新版本里最强的一个(推荐段)→ 其余按家族分组。
//! 只重排、不增删(完全相同的 id 只留第一个),认不出的 id 保持原相对顺序排在最后。

use std::cmp::Ordering;

/// 优先展示的家族,其余家族按字母序排在它们后面。
const PREFERRED_FAMILIES: [&str; 5] = ["gpt", "claude", "gemini", "grok", "composer"];

/// 会被认成档位的末段。
const TIERS: [&str; 6] = ["xhigh", "high", "max", "medium", "low", "minimal"];

/// 档位排名:越小越靠前。推荐段与完整列表共用一张表,所以推荐出来的那一项也正是它所在
/// 模型组的第一项。推荐偏好是 xhigh > high > 无档位(服务端默认档)> max > medium > low > minimal。
fn tier_rank(tier: Option<&str>) -> u8 {
    match tier {
        Some("xhigh") => 0,
        Some("high") => 1,
        None => 2,
        Some("max") => 3,
        Some("medium") => 4,
        Some("low") => 5,
        Some("minimal") => 6,
        // 只有 TIERS 里的会被解析成档位;兜底排最后,保证全序。
        Some(_) => 7,
    }
}

/// 一个可解析 id 的排序要素。
#[derive(Debug)]
struct Parsed {
    family: String,
    /// 家族前缀之后的数字段(去前导零的十进制串),按数值逐段比较。
    version: Vec<String>,
    /// 去掉档位与 `-fast` 后的模型名(小写),同名的各档位归一组。
    model: String,
    tier: Option<&'static str>,
    fast: bool,
    bracketed: bool,
}

fn parse(id: &str) -> Option<Parsed> {
    let lower = id.trim().to_ascii_lowercase();
    // `claude-opus-4-8[context=1m,effort=high]` 这类参数化 id:方括号部分不参与解析。
    let (base, bracketed) = match lower.split_once('[') {
        Some((base, _)) => (base, true),
        None => (lower.as_str(), false),
    };
    let base = base.strip_prefix("cursor-").unwrap_or(base);
    let family: String = base
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if family.is_empty() {
        return None;
    }
    let mut tokens: Vec<&str> = base.split('-').collect();
    let mut fast = false;
    let mut tier = None;
    // 至少留一段作模型名:`fast`、`high` 这种单段 id 本身就是模型名。
    if tokens.len() > 1 && tokens.last() == Some(&"fast") {
        tokens.pop();
        fast = true;
    }
    if tokens.len() > 1 {
        if let Some(found) = tokens
            .last()
            .and_then(|last| TIERS.iter().find(|tier| *tier == last))
        {
            tier = Some(*found);
            tokens.pop();
        }
    }
    let model = tokens.join("-");
    let version = parse_version(&model, &family);
    Some(Parsed {
        family,
        version,
        model,
        tier,
        fast,
        bracketed,
    })
}

/// 版本号:家族名之后先跳过模型名单词(纯字母段,如 `opus`)与参数量段(如 `120b`),
/// 再取紧接着的连续版本段(`5.3`、`4`、`v41`),遇到别的段即停。日期、参数量、
/// 子型号里的数字都不算版本,否则 `gpt-oss-120b` 会压过 `gpt-5.6`。
fn parse_version(model: &str, family: &str) -> Vec<String> {
    let mut tokens = model.split('-');
    // family 是首段的 ASCII 前缀,按字节切不会落在字符中间;仍用 get 兜底。
    let head = tokens
        .next()
        .unwrap_or_default()
        .get(family.len()..)
        .unwrap_or_default();
    let mut version = Vec::new();
    for (position, segment) in std::iter::once(head).chain(tokens).enumerate() {
        if position == 0 && segment.is_empty() {
            continue;
        }
        if let Some(parts) = version_parts(segment) {
            version.extend(parts);
            continue;
        }
        let name_word = !segment.is_empty() && segment.chars().all(|c| c.is_ascii_alphabetic());
        if version.is_empty() && (name_word || is_param_size(segment)) {
            continue;
        }
        break;
    }
    version
}

/// `5.3` / `4` / `v41` 这类版本段,拆成去前导零的数字串;不是版本段返回 None。
fn version_parts(segment: &str) -> Option<Vec<String>> {
    let digits = segment.strip_prefix('v').unwrap_or(segment);
    if !digits.starts_with(|c: char| c.is_ascii_digit())
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '.')
    {
        return None;
    }
    Some(
        digits
            .split('.')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let trimmed = part.trim_start_matches('0');
                if trimmed.is_empty() { "0" } else { trimmed }.to_string()
            })
            .collect(),
    )
}

/// `120b` / `1.5b` 这类参数量段。
fn is_param_size(segment: &str) -> bool {
    segment
        .strip_suffix('b')
        .is_some_and(|size| version_parts(size).is_some() && !size.starts_with('v'))
}

/// 去前导零的十进制串按数值比较:先比位数再比字面,任意长度都不会溢出。
fn cmp_number(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn cmp_version(a: &[String], b: &[String]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        match cmp_number(x, y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

fn family_rank(family: &str) -> (usize, &str) {
    let preferred = PREFERRED_FAMILIES
        .iter()
        .position(|f| *f == family)
        .unwrap_or(PREFERRED_FAMILIES.len());
    (preferred, family)
}

/// 同一模型组里的先后:档位 → 非 fast 在前 → 不带方括号在前。
fn cmp_variant(a: &Parsed, b: &Parsed) -> Ordering {
    tier_rank(a.tier)
        .cmp(&tier_rank(b.tier))
        .then_with(|| a.fast.cmp(&b.fast))
        .then_with(|| a.bracketed.cmp(&b.bracketed))
}

/// 按展示顺序重排 Cursor 模型目录(见模块说明)。对任意输入都是全序、确定、不 panic。
pub fn order_cursor_models(models: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<&String> = models.iter().filter(|m| seen.insert(m.as_str())).collect();

    let mut auto = Vec::new();
    let mut unparsed = Vec::new();
    let mut parsed: Vec<(usize, Parsed)> = Vec::new();
    for (index, id) in unique.iter().enumerate() {
        if id.trim().eq_ignore_ascii_case("auto") {
            auto.push(index);
        } else if let Some(p) = parse(id) {
            parsed.push((index, p));
        } else {
            unparsed.push(index);
        }
    }

    // 模型组按首次出现的位置定先后:同版本的不同模型(sol / luna)保持服务端顺序。
    let mut group_first: std::collections::HashMap<(&str, &str), usize> =
        std::collections::HashMap::new();
    for (index, p) in &parsed {
        group_first
            .entry((p.family.as_str(), p.model.as_str()))
            .or_insert(*index);
    }
    let group_of = |p: &Parsed| {
        group_first
            .get(&(p.family.as_str(), p.model.as_str()))
            .copied()
            .unwrap_or(usize::MAX)
    };
    let mut order: Vec<&(usize, Parsed)> = parsed.iter().collect();
    order.sort_by(|(ia, a), (ib, b)| {
        family_rank(&a.family)
            .cmp(&family_rank(&b.family))
            .then_with(|| cmp_version(&b.version, &a.version))
            .then_with(|| group_of(a).cmp(&group_of(b)))
            .then_with(|| cmp_variant(a, b))
            .then_with(|| ia.cmp(ib))
    });

    // 推荐段:每个家族最新版本里档位最优的一项(同版本跨模型组也比档位)。
    // 推荐不选 `-fast`,最新版本里全是 fast 时才回落到它。
    let mut recommended: Vec<usize> = Vec::new();
    let mut rest = order.as_slice();
    while let Some((_, head)) = rest.first() {
        let family_len = rest
            .iter()
            .position(|(_, p)| p.family != head.family)
            .unwrap_or(rest.len());
        let (family, tail) = rest.split_at(family_len);
        let best = family
            .iter()
            .take_while(|(_, p)| cmp_version(&p.version, &head.version) == Ordering::Equal)
            .min_by(|(ia, a), (ib, b)| {
                a.fast
                    .cmp(&b.fast)
                    .then_with(|| cmp_variant(a, b))
                    .then_with(|| ia.cmp(ib))
            });
        if let Some((index, _)) = best {
            recommended.push(*index);
        }
        rest = tail;
    }

    let positions = auto
        .iter()
        .copied()
        .chain(recommended.iter().copied())
        .chain(
            order
                .iter()
                .map(|(index, _)| *index)
                .filter(|index| !recommended.contains(index)),
        )
        .chain(unparsed.iter().copied());
    positions
        .filter_map(|index| unique.get(index).map(|id| (*id).clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn order(list: &[&str]) -> Vec<String> {
        order_cursor_models(&ids(list))
    }

    fn assert_same_set(input: &[String], output: &[String]) {
        let mut a = input.to_vec();
        a.sort();
        a.dedup();
        let mut b = output.to_vec();
        b.sort();
        assert_eq!(a, b, "不能丢 id,也不能多出 id");
    }

    const REAL: &[&str] = &[
        "auto",
        "gpt-5.3-codex-low",
        "gpt-5.3-codex",
        "gpt-5.3-codex-high",
        "gpt-5.3-codex-xhigh",
        "gpt-5.2",
        "composer-2.5",
        "composer-2.5-fast",
        "gpt-5.6-sol-high",
        "gpt-5.6-sol-xhigh",
        "cursor-grok-4.5-high",
        "cursor-grok-4.5-high-fast",
        "gpt-5.6-luna-high",
        "grok-4.7-low",
        "grok-4.7-low-fast",
        "grok-4.7-xhigh",
        "grok-4.7-xhigh-fast",
        "cursor-grok-4.6-low",
        "gemini-4-carbon-low",
        "gemini-4-carbon-medium",
        "gemini-4-carbon-high",
        "gemini-4-argon-low",
        "gemini-4-argon-medium",
        "gemini-4-argon-high",
        "muse-wonder-1.0-minimal",
        "muse-wonder-1.0-low",
        "muse-wonder-1.0-medium",
        "muse-wonder-1.0-high",
        "muse-wonder-1.0-xhigh",
        "claude-opus-4-8-thinking-high",
        "claude-opus-4-8[context=1m,effort=high]",
        "deepseek-v41-flash-1017-low",
        "deepseek-v41-flash-1017-medium",
        "deepseek-v41-flash-1017-high",
        "deepseek-v41-flash-1017-max",
        "kimi-k2",
    ];

    #[test]
    fn orders_realistic_catalog_auto_then_recommended_then_families() {
        let out = order(REAL);
        assert_same_set(&ids(REAL), &out);
        let got: Vec<&str> = out.iter().map(String::as_str).collect();
        assert_eq!(
            got,
            vec![
                "auto",
                // 推荐段:gpt / claude / gemini / grok / composer,其余家族按字母序。
                "gpt-5.6-sol-xhigh",
                // claude 两个都是 4.8:thinking-high 的档位高于无档位的参数化 id。
                "claude-opus-4-8-thinking-high",
                "gemini-4-carbon-high",
                "grok-4.7-xhigh",
                "composer-2.5",
                "deepseek-v41-flash-1017-high",
                "kimi-k2",
                "muse-wonder-1.0-xhigh",
                // 完整列表。
                "gpt-5.6-sol-high",
                "gpt-5.6-luna-high",
                "gpt-5.3-codex-xhigh",
                "gpt-5.3-codex-high",
                "gpt-5.3-codex",
                "gpt-5.3-codex-low",
                "gpt-5.2",
                "claude-opus-4-8[context=1m,effort=high]",
                "gemini-4-carbon-medium",
                "gemini-4-carbon-low",
                "gemini-4-argon-high",
                "gemini-4-argon-medium",
                "gemini-4-argon-low",
                "grok-4.7-xhigh-fast",
                "grok-4.7-low",
                "grok-4.7-low-fast",
                "cursor-grok-4.6-low",
                "cursor-grok-4.5-high",
                "cursor-grok-4.5-high-fast",
                "composer-2.5-fast",
                "deepseek-v41-flash-1017-max",
                "deepseek-v41-flash-1017-medium",
                "deepseek-v41-flash-1017-low",
                "muse-wonder-1.0-high",
                "muse-wonder-1.0-medium",
                "muse-wonder-1.0-low",
                "muse-wonder-1.0-minimal",
            ]
        );
    }

    #[test]
    fn handles_empty_and_auto_only() {
        assert!(order(&[]).is_empty());
        assert_eq!(order(&["AUTO"]), ids(&["AUTO"]));
        assert_eq!(order(&["x-1", "Auto"]), ids(&["Auto", "x-1"]));
    }

    #[test]
    fn works_without_preferred_families_or_tiers() {
        let out = order(&["zeta-1", "alpha-2", "alpha-3", "mid"]);
        assert_eq!(out, ids(&["alpha-3", "mid", "zeta-1", "alpha-2"]));
    }

    #[test]
    fn version_compares_numerically_not_lexically() {
        let out = order(&["gpt-5.2", "gpt-5.10", "gpt-5.9"]);
        assert_eq!(out, ids(&["gpt-5.10", "gpt-5.9", "gpt-5.2"]));
    }

    #[test]
    fn untiered_sits_between_high_and_max() {
        let out = order(&["m-1-low", "m-1-max", "m-1", "m-1-high", "m-1-minimal"]);
        assert_eq!(
            out,
            ids(&["m-1-high", "m-1", "m-1-max", "m-1-low", "m-1-minimal"])
        );
    }

    #[test]
    fn unparseable_ids_keep_relative_order_last() {
        let input = ids(&["9b", "", "模型-1", "gpt-5", "[x]", "-high", "  "]);
        let out = order_cursor_models(&input);
        assert_same_set(&input, &out);
        assert_eq!(
            out,
            ids(&["gpt-5", "9b", "", "模型-1", "[x]", "-high", "  "])
        );
    }

    #[test]
    fn exact_duplicates_collapse_but_case_variants_stay() {
        assert_eq!(
            order(&["gpt-5", "gpt-5", "GPT-5"]),
            ids(&["gpt-5", "GPT-5"])
        );
    }

    #[test]
    fn fast_follows_its_sibling_and_non_fast_is_recommended() {
        // 推荐位跳过档位更高的 fast;完整列表里 fast 仍紧跟各自的非 fast 兄弟。
        let out = order(&[
            "grok-4.7-high-fast",
            "grok-4.7-low-fast",
            "grok-4.7-low",
            "grok-4.7-high",
        ]);
        assert_eq!(
            out,
            ids(&[
                "grok-4.7-high",
                "grok-4.7-high-fast",
                "grok-4.7-low",
                "grok-4.7-low-fast"
            ])
        );
        let out = order(&["grok-4.7-high-fast", "grok-4.7-low-fast", "grok-4.7-low"]);
        assert_eq!(
            out,
            ids(&["grok-4.7-low", "grok-4.7-high-fast", "grok-4.7-low-fast"])
        );
        // 最新版本里全是 fast 时才推荐 fast。
        let out = order(&["x-1-low-fast", "x-1-high-fast", "x-0.9-high"]);
        assert_eq!(out, ids(&["x-1-high-fast", "x-1-low-fast", "x-0.9-high"]));
        let out = order(&["grok-4.7-high-fast", "grok-4.7-high"]);
        assert_eq!(out, ids(&["grok-4.7-high", "grok-4.7-high-fast"]));
    }

    #[test]
    fn parameter_sizes_and_sub_model_numbers_are_not_versions() {
        let out = order(&[
            "gpt-oss-120b",
            "gpt-5.2",
            "gpt-5.6-sol-high",
            "qwen3-coder-480b",
            "qwen2.5-coder-32b",
            "claude-3-5-sonnet-20241022",
            "claude-opus-4-8-thinking-high",
        ]);
        assert_eq!(
            out,
            ids(&[
                "gpt-5.6-sol-high",
                "claude-opus-4-8-thinking-high",
                "qwen3-coder-480b",
                "gpt-5.2",
                "gpt-oss-120b",
                "claude-3-5-sonnet-20241022",
                "qwen2.5-coder-32b",
            ])
        );
        assert_eq!(parse_version("gpt-oss-120b", "gpt"), Vec::<String>::new());
        assert_eq!(parse_version("qwen3-coder-480b", "qwen"), ["3"]);
        assert_eq!(parse_version("deepseek-v41-flash-1017", "deepseek"), ["41"]);
        assert_eq!(
            parse_version("claude-opus-4-8-thinking", "claude"),
            ["4", "8"]
        );
    }

    #[test]
    fn weird_shapes_never_panic_and_keep_every_id() {
        let long_digits = format!("gpt-{}", "9".repeat(500));
        let input: Vec<String> = vec![
            "gpt-5".into(),
            long_digits.clone(),
            "a".repeat(10_000),
            "fast".into(),
            "high".into(),
            "x-fast-fast".into(),
            "cursor-".into(),
            "cursor-cursor-1".into(),
            "a[".into(),
            "a]b[c".into(),
            "gpt-5-high-fast[mode=x]".into(),
            "émoji-🙂-high".into(),
            "é".into(),
            "---".into(),
            "gpt-0005".into(),
            "[".into(),
        ];
        let out = order_cursor_models(&input);
        assert_same_set(&input, &out);
        assert_eq!(out.len(), input.len());
        // 超长数字版本按数值最大,占 gpt 家族的推荐位。
        assert_eq!(out[0], long_digits);
        assert_eq!(order_cursor_models(&input), out, "必须确定");
        // 打乱输入顺序后集合不变、不 panic。
        let mut reversed = input.clone();
        reversed.reverse();
        assert_same_set(&input, &order_cursor_models(&reversed));
    }
}
