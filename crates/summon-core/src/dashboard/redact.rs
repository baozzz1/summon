//! 敏感值的脱敏往返:下发前把密钥换成占位符,保存时占位符原样带回就还原原值,改动了才写新值。
//!
//! 目前唯一的敏感字段是 agent.yaml 的 `runtime.env`:值可以写 `${VAR}` 引用(真值在已
//! gitignore 的 `.env` 里),这种照常显示;其余直接写在 yaml 里的字面值一律当密钥。
//!
//! **fail-closed**:原文的结构判不准、又可能含 `runtime.env` 时,返回错误而不是原文 —— 宁可
//! 原文模式打不开,也不让密钥出现在响应里。

use super::RawKind;
use crate::yaml_edit::{child_values, ChildLookup};
use serde_yaml::Value;

/// 占位符。故意选一个不会是真实取值、写进 YAML 又是合法裸值的串。
pub const PLACEHOLDER: &str = "«summon:redacted»";

/// 被藏起来的一项:键名与原文里那段值的文本(含引号与冒号后的空白),还原时逐字放回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hidden {
    pub key: String,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RedactError {
    /// 判不准结构、又可能含密钥:不下发原文。
    #[error("{0}")]
    Unsupported(String),
    /// 占位符被挪到了别的键上,或出现在敏感值之外:不知道该还原成什么。
    #[error("{0}")]
    StrayPlaceholder(String),
}

/// 这个值要不要藏:只由 `${VAR}` 引用拼成(或为空)的照常显示,其余都藏。
/// 判法与 daemon 展开引用的规则一致:未闭合的 `${` 不算引用。
pub fn is_secret(value: &str) -> bool {
    let mut rest = value;
    while !rest.is_empty() {
        let Some(after) = rest.strip_prefix("${") else {
            return true;
        };
        match after.find('}') {
            Some(end) if end > 0 => rest = &after[end + 1..],
            _ => return true,
        }
    }
    false
}

/// 表单下发用:要藏的值换成占位符。
pub fn mask(value: &str) -> &str {
    if is_secret(value) {
        PLACEHOLDER
    } else {
        value
    }
}

/// 表单保存用:提交的是占位符就还原成原值;原来没有这一项却提交了占位符则拒绝。
pub fn unmask(key: &str, submitted: &str, original: Option<&str>) -> Result<String, RedactError> {
    if submitted != PLACEHOLDER {
        return Ok(submitted.to_string());
    }
    original
        .map(str::to_string)
        .ok_or_else(|| RedactError::StrayPlaceholder(summon_i18n::format!("rc012b58a6f")))
}

const ENV_PATH: [&str; 2] = ["runtime", "env"];

fn env_path() -> Vec<String> {
    ENV_PATH.iter().map(|s| s.to_string()).collect()
}

/// 原文下发用:返回脱敏后的全文与被藏起来的值(留在服务端,不下发)。
pub fn redact(kind: RawKind, text: &str) -> Result<(String, Vec<Hidden>), RedactError> {
    if kind == RawKind::Config {
        return Ok((text.to_string(), Vec::new()));
    }
    let children = match child_values(text, &env_path()) {
        Ok(ChildLookup::Children(children)) => children,
        Ok(ChildLookup::Missing) => return Ok((text.to_string(), Vec::new())),
        Ok(ChildLookup::NotBlockMap { .. }) | Err(_) => {
            return fallback(text).map(|()| (text.to_string(), Vec::new()))
        }
    };
    let mut out = text.to_string();
    let mut hidden = Vec::new();
    // 从后往前换,前面的字节位置不受影响。
    for child in children.iter().rev() {
        let raw = &text[child.range.clone()];
        if !value_is_secret(raw) {
            continue;
        }
        hidden.push(Hidden {
            key: child.key.clone(),
            raw: raw.to_string(),
        });
        out.replace_range(child.range.clone(), &format!(" {PLACEHOLDER}"));
    }
    hidden.reverse();
    Ok((out, hidden))
}

/// 原文保存用:把编辑后全文里原样带回的占位符还原成原值。
pub fn restore(kind: RawKind, edited: &str, hidden: &[Hidden]) -> Result<String, RedactError> {
    let stray = || RedactError::StrayPlaceholder(summon_i18n::format!("r69a30f7e5d"));
    let children = match (kind, child_values(edited, &env_path())) {
        (RawKind::Agent, Ok(ChildLookup::Children(children))) => children,
        _ if edited.contains(PLACEHOLDER) => return Err(stray()),
        _ => return Ok(edited.to_string()),
    };
    let mut out = edited.to_string();
    for child in children.iter().rev() {
        let raw = &edited[child.range.clone()];
        if !is_placeholder(raw) {
            continue;
        }
        let Some(original) = hidden.iter().find(|h| h.key == child.key) else {
            return Err(RedactError::StrayPlaceholder(summon_i18n::format!(
                "r816896b342",
                child.key
            )));
        };
        out.replace_range(child.range.clone(), &original.raw);
    }
    if out.contains(PLACEHOLDER) {
        return Err(stray());
    }
    Ok(out)
}

/// 一段值的原文要不要藏:单独解析出来是只含引用的字符串或空值才显示,其余(字面串、数字、
/// 解析不了的跨行写法)都藏。
fn value_is_secret(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    match serde_yaml::from_str::<Value>(trimmed) {
        Ok(Value::String(s)) => is_secret(&s),
        Ok(Value::Null) => false,
        _ => true,
    }
}

fn is_placeholder(raw: &str) -> bool {
    matches!(
        serde_yaml::from_str::<Value>(raw.trim()),
        Ok(Value::String(s)) if s == PLACEHOLDER
    )
}

/// 逐键定位不了 `runtime.env`(行内映射、锚点、缩进坏了)时:能证明里面没有密钥才照原样给。
fn fallback(text: &str) -> Result<(), RedactError> {
    let refuse = || RedactError::Unsupported(summon_i18n::text!("re14691c29c").to_string());
    match serde_yaml::from_str::<Value>(text) {
        Ok(doc) => {
            let env = doc.get("runtime").and_then(|r| r.get("env"));
            let visible = match env {
                None | Some(Value::Null) => true,
                Some(Value::Mapping(map)) => map.values().all(|v| match v {
                    Value::String(s) => !is_secret(s),
                    Value::Null => true,
                    _ => false,
                }),
                Some(_) => false,
            };
            if visible {
                Ok(())
            } else {
                Err(refuse())
            }
        }
        // 解析不了:只要文本里任何地方像是写了 `env:`,就不给。
        Err(_) if mentions_env_key(text) => Err(refuse()),
        Err(_) => Ok(()),
    }
}

fn mentions_env_key(text: &str) -> bool {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| {
            let bytes = line.as_bytes();
            line.match_indices("env").any(|(at, _)| {
                let before = at.checked_sub(1).map(|i| bytes[i]);
                let boundary = !before.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
                let rest = line[at + 3..].trim_start_matches(['"', '\'', ' ', '\t']);
                boundary && rest.starts_with(':')
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "name: a\nruntime:\n  cli: codex\n  env:\n    # 代理\n    HTTP_PROXY: \"http://u:p@proxy:8080\"   # 带口令\n    TOKEN: sk-123\n    PORT: 8080\n    REF: \"${CODEX_KEY}\"\n    PAIR: ${A}${B}\n    BEARER: \"Bearer ${T}\"\n    EMPTY: \"\"\n    BLOCK: |\n      多行\n      密钥\n  working_dir: ~/w\n";

    fn redacted() -> (String, Vec<Hidden>) {
        redact(RawKind::Agent, SRC).expect("结构清楚,应能脱敏")
    }

    #[test]
    fn literal_env_values_are_hidden() {
        let (text, hidden) = redacted();
        for secret in ["u:p@proxy", "sk-123", "8080", "Bearer", "多行", "密钥"] {
            assert!(!text.contains(secret), "{secret} 漏出来了:\n{text}");
        }
        let keys: Vec<&str> = hidden.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(keys, ["HTTP_PROXY", "TOKEN", "PORT", "BEARER", "BLOCK"]);
        assert!(
            text.contains("    HTTP_PROXY: «summon:redacted»   # 带口令\n"),
            "{text}"
        );
        assert!(text.contains("    # 代理\n") && text.contains("  working_dir: ~/w\n"));
        // 脱敏后的全文仍是合法的 agent.yaml,原文校验照常跑。
        let parsed: crate::config::registry::Agent = serde_yaml::from_str(&text).unwrap();
        let env = parsed.runtime.unwrap().env.unwrap();
        assert_eq!(env["TOKEN"], PLACEHOLDER);
        // config.yaml 里没有敏感字段,原样返回。
        let cfg = "messaging:\n  cot_visible: true\n";
        assert_eq!(
            redact(RawKind::Config, cfg).unwrap(),
            (cfg.to_string(), Vec::new())
        );
    }

    #[test]
    fn var_references_stay_visible() {
        let (text, _) = redacted();
        assert!(text.contains("    REF: \"${CODEX_KEY}\"\n"));
        assert!(text.contains("    PAIR: ${A}${B}\n"));
        assert!(text.contains("    EMPTY: \"\"\n"));
        assert!(!is_secret("${X}") && !is_secret("${X}${Y}") && !is_secret(""));
        assert!(is_secret("Bearer ${T}") && is_secret("sk-1") && is_secret("${UNCLOSED"));
        assert_eq!(mask("${X}"), "${X}");
        assert_eq!(mask("sk-1"), PLACEHOLDER);
    }

    #[test]
    fn untouched_placeholder_restores_original() {
        let (text, hidden) = redacted();
        assert_eq!(restore(RawKind::Agent, &text, &hidden).unwrap(), SRC);
        assert_eq!(
            unmask("TOKEN", PLACEHOLDER, Some("sk-123")).unwrap(),
            "sk-123"
        );
    }

    #[test]
    fn edited_placeholder_takes_new_value() {
        let (text, hidden) = redacted();
        let edited = text.replace("    TOKEN: «summon:redacted»\n", "    TOKEN: sk-new\n");
        let restored = restore(RawKind::Agent, &edited, &hidden).unwrap();
        assert_eq!(restored, SRC.replace("TOKEN: sk-123", "TOKEN: sk-new"));
        assert_eq!(unmask("TOKEN", "sk-new", Some("sk-123")).unwrap(), "sk-new");
        // 另起一行加了新键,其余照旧还原。
        let edited = text.replace("  working_dir", "  cli_cmd: x\n  working_dir");
        let restored = restore(RawKind::Agent, &edited, &hidden).unwrap();
        assert_eq!(
            restored,
            SRC.replace("  working_dir", "  cli_cmd: x\n  working_dir")
        );
    }

    #[test]
    fn removed_key_stays_removed() {
        let (text, hidden) = redacted();
        let edited = text.replace("    TOKEN: «summon:redacted»\n", "");
        let restored = restore(RawKind::Agent, &edited, &hidden).unwrap();
        assert_eq!(restored, SRC.replace("    TOKEN: sk-123\n", ""));
        assert!(!restored.contains("sk-123"));
    }

    #[test]
    fn stray_placeholder_is_refused() {
        let (text, hidden) = redacted();
        // 挪到一个原来没有的键上:不知道该还原成什么。
        let moved = text.replace("    TOKEN:", "    TOKEN2:");
        assert!(matches!(
            restore(RawKind::Agent, &moved, &hidden),
            Err(RedactError::StrayPlaceholder(_))
        ));
        // 抄到敏感值之外。
        let copied = text.replace("  working_dir: ~/w", "  working_dir: «summon:redacted»");
        assert!(matches!(
            restore(RawKind::Agent, &copied, &hidden),
            Err(RedactError::StrayPlaceholder(_))
        ));
        assert!(unmask("NEW", PLACEHOLDER, None).is_err());
    }

    #[test]
    fn unreadable_structure_fails_closed() {
        let flow = "name: a\nruntime: {env: {TOKEN: sk-1}}\n";
        assert!(matches!(
            redact(RawKind::Agent, flow),
            Err(RedactError::Unsupported(_))
        ));
        let tabbed = "name: a\nruntime:\n\tenv:\n\t\tTOKEN: sk-1\n";
        assert!(redact(RawKind::Agent, tabbed).is_err());
        // 结构坏了但根本没有 env:照原样给,让人能在原文模式里修。
        let broken = "name: a\nfeishu:\n\tscope: x\n";
        assert_eq!(redact(RawKind::Agent, broken).unwrap().0, broken);
        // env 段只有引用:行内写法也照样能给。
        let refs = "name: a\nruntime: {env: {K: \"${K}\"}}\n";
        assert_eq!(redact(RawKind::Agent, refs).unwrap().0, refs);
    }
}
