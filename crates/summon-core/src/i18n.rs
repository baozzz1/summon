//! 内置文案的语言选择与资源读取。机器协议、用户内容与授权判断使用各自的原始值。
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LanguagePreference {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "zh-CN", alias = "zh_cn", alias = "zh")]
    Chinese,
    #[serde(rename = "en", alias = "en-US", alias = "en_us")]
    English,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "zh-CN")]
    Chinese,
    #[serde(rename = "en")]
    English,
}

impl Language {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Chinese => "zh-CN",
            Self::English => "en",
        }
    }

    /// 本轮支持中文和英语，其它首选语言使用英语；不跳到用户的次选语言。
    pub fn from_preferred(value: &str) -> Option<Self> {
        let base = value
            .trim()
            .split(['-', '_', '.', '@'])
            .next()?
            .to_ascii_lowercase();
        if !((2..=3).contains(&base.len()) && base.bytes().all(|b| b.is_ascii_alphabetic())) {
            return None;
        }
        Some(if base == "zh" {
            Self::Chinese
        } else {
            Self::English
        })
    }
}

pub fn resolve(
    preference: LanguagePreference,
    lark: Option<&str>,
    system_first: Option<&str>,
) -> Language {
    match preference {
        LanguagePreference::Chinese => Language::Chinese,
        LanguagePreference::English => Language::English,
        LanguagePreference::Auto => lark
            .and_then(Language::from_preferred)
            .or_else(|| system_first.and_then(Language::from_preferred))
            .unwrap_or(Language::English),
    }
}

thread_local! {
    static SCOPED: std::cell::Cell<Option<Language>> = const { std::cell::Cell::new(None) };
}

/// Freeze the language of one synchronous rendering operation.
pub fn with_language<T>(language: Language, render: impl FnOnce() -> T) -> T {
    struct Restore(Option<Language>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCOPED.set(self.0);
        }
    }
    let previous = SCOPED.replace(Some(language));
    let _restore = Restore(previous);
    render()
}

static CURRENT: AtomicU8 = AtomicU8::new(0);
pub fn set_language(language: Language) {
    CURRENT.store(u8::from(language == Language::English), Ordering::Relaxed);
}
pub fn language() -> Language {
    if let Some(language) = SCOPED.get() {
        return language;
    }
    if CURRENT.load(Ordering::Relaxed) == 0 {
        Language::Chinese
    } else {
        Language::English
    }
}

fn catalog(language: Language) -> &'static BTreeMap<String, String> {
    static ZH: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    static EN: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    let (slot, shared, rust) = match language {
        Language::Chinese => (
            &ZH,
            include_str!("../../../locales/shared/zh-CN.json"),
            include_str!("../../../locales/rust/zh-CN.json"),
        ),
        Language::English => (
            &EN,
            include_str!("../../../locales/shared/en.json"),
            include_str!("../../../locales/rust/en.json"),
        ),
    };
    slot.get_or_init(|| {
        let mut catalog: BTreeMap<String, String> =
            serde_json::from_str(shared).expect("valid shared locale catalog");
        catalog.extend(
            serde_json::from_str::<BTreeMap<String, String>>(rust)
                .expect("valid Rust locale catalog"),
        );
        catalog
    })
}

pub fn text_in(language: Language, key: &str) -> &'static str {
    catalog(language)
        .get(key)
        .or_else(|| catalog(Language::Chinese).get(key))
        .map(String::as_str)
        .unwrap_or("[missing translation]")
}
pub fn text(key: &str) -> &'static str {
    text_in(language(), key)
}

pub fn label_in(language: Language, value: &str) -> &str {
    catalog(language)
        .get(value)
        .or_else(|| catalog(Language::Chinese).get(value))
        .map(String::as_str)
        .unwrap_or(value)
}

pub fn label(value: &str) -> &str {
    label_in(language(), value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resource_formats_keep_captures_precision_and_error_fields() {
        let name = "sample";
        let value = 1.234;
        assert_eq!(
            summon_i18n::format!("test.capture", 7),
            "你好 sample，1.23 7"
        );
        #[derive(Debug, thiserror::Error)]
        #[error("{}", summon_i18n::format!("test.error", .0))]
        struct Error(u8);
        #[derive(Debug, thiserror::Error)]
        #[error("{}", summon_i18n::text!("test.unit"))]
        struct Unit;
        assert_eq!(Error(7).to_string(), "错误 7");
        assert_eq!(Unit.to_string(), "不存在");
    }

    #[test]
    fn explicit_language_wins_and_auto_follows_lark_then_first_system_preference() {
        assert_eq!(
            resolve(LanguagePreference::English, Some("zh_cn"), Some("zh-Hans")),
            Language::English
        );
        assert_eq!(
            resolve(LanguagePreference::Chinese, Some("en_us"), Some("en")),
            Language::Chinese
        );
        assert_eq!(
            resolve(LanguagePreference::Auto, Some("zh_cn"), Some("en-US")),
            Language::Chinese
        );
        assert_eq!(
            resolve(LanguagePreference::Auto, Some("en_us"), Some("zh-Hans")),
            Language::English
        );
        assert_eq!(
            resolve(LanguagePreference::Auto, Some(""), Some("zh-Hans-CN")),
            Language::Chinese
        );
        assert_eq!(
            resolve(LanguagePreference::Auto, None, Some("ja-JP")),
            Language::English
        );
        assert_eq!(
            resolve(LanguagePreference::Auto, None, None),
            Language::English
        );
    }
    #[test]
    fn catalogs_cover_the_same_keys_and_keep_protocol_values_outside_resources() {
        let zh = catalog(Language::Chinese);
        let en = catalog(Language::English);
        assert_eq!(zh.keys().collect::<Vec<_>>(), en.keys().collect::<Vec<_>>());
        assert!(zh.values().chain(en.values()).all(|v| !v.is_empty()));
        assert_eq!(text_in(Language::English, "common.enabled"), "Enabled");
        assert_eq!(text_in(Language::Chinese, "common.enabled"), "已启用");
    }
    #[test]
    fn english_rendering_keeps_format_captures_and_machine_configuration() {
        with_language(Language::English, || {
            let name = "sample";
            let value = 1.234;
            assert_eq!(
                summon_i18n::format!("test.capture", 7),
                "Hello sample, 1.23 7"
            );
            let messaging = crate::config::builtin_messaging();
            assert!(messaging
                .system_prompt_core
                .contains("__SUMMON_SILENT_EXIT__"));
            assert!(messaging.system_prompt_core.contains("{ask_protocol}"));
            assert!(messaging.system_prompt_core.contains("Reply naturally"));
            let mut en: serde_yaml::Value =
                serde_yaml::from_str(summon_i18n::include_text!("messaging.default.yaml")).unwrap();
            let mut zh: serde_yaml::Value = with_language(Language::Chinese, || {
                serde_yaml::from_str(summon_i18n::include_text!("messaging.default.yaml")).unwrap()
            });
            for key in [
                "system_prompt_core",
                "system_prompt_reactions",
                "system_prompt_feed",
                "reaction_meanings",
                "prompt_template",
                "session_exited",
            ] {
                en.as_mapping_mut()
                    .unwrap()
                    .remove(serde_yaml::Value::String(key.into()));
                zh.as_mapping_mut()
                    .unwrap()
                    .remove(serde_yaml::Value::String(key.into()));
            }
            assert_eq!(en, zh, "translated defaults must preserve machine settings");
        });
        assert_eq!(language(), Language::Chinese);
    }

    #[test]
    fn a_rendering_scope_restores_the_previous_language_even_after_unwinding() {
        with_language(Language::English, || {
            assert_eq!(language(), Language::English);
            let result = std::panic::catch_unwind(|| {
                with_language(Language::Chinese, || panic!("interrupted render"))
            });
            assert!(result.is_err());
            assert_eq!(language(), Language::English);
        });
        assert_eq!(language(), Language::Chinese);
    }

    #[test]
    fn configuration_values_are_stable_and_reject_unknown_languages() {
        assert_eq!(
            serde_yaml::to_string(&LanguagePreference::English)
                .unwrap()
                .trim(),
            "en"
        );
        assert_eq!(
            serde_yaml::from_str::<LanguagePreference>("zh_cn").unwrap(),
            LanguagePreference::Chinese
        );
        assert!(serde_yaml::from_str::<LanguagePreference>("fr").is_err());
    }
}
