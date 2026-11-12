//! 邮箱打码:飞书内容审核以「含邮箱」整条拒发时,把消息里的邮箱打码后重发。
//!
//! 打码只动 `@` 前面的用户名:保留首字母、其余换成 `***`(`probe.user@example.com` →
//! `p***@example.com`),2026-10-16 真机实测这种形态能过审。审核把 `git@github.com:org/repo`、
//! `ssh://git@…`、`https://token@…` 也当邮箱拒,所以凡长得像邮箱的一律打码 —— 只有被拒的
//! 消息才走到这里,打码是它送达的唯一办法。

/// 卡片里承载回调值与表单预填值的键。点按钮 / 提交表单时飞书把它们原样回传,打码会把用户的
/// 选择或原有内容静默写坏,所以不碰;它们不显示,审核仍拒就照常走被拒说明。
const VALUE_KEYS: &[&str] = &[
    "value",
    "default_value",
    "initial_value",
    "initial_option",
    "initial_options",
];

/// 把文本里的邮箱打码;没有可打的返回 `None`。
pub fn mask_email_addresses(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    for (at, _) in text.match_indices('@') {
        if at < copied {
            continue;
        }
        let Some((local_start, domain_end)) = email_span(bytes, at) else {
            continue;
        };
        if local_start < copied {
            continue;
        }
        out.push_str(&text[copied..local_start]);
        out.push(char::from(bytes[local_start]));
        out.push_str("***");
        out.push_str(&text[at..domain_end]);
        copied = domain_end;
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

/// 卡片 JSON 里显示用的字符串值打码:键不动,回调值与表单预填值不动(见 [`VALUE_KEYS`])。
/// 按解析后的值处理,不在 JSON 文本上替换 —— 转义序列(`\n`、`\u…`)紧挨着邮箱时,文本层面
/// 会把转义符当成用户名的一部分。重新序列化不保留键序,飞书卡片不依赖键序(2026-10-16 实测
/// 键排过序的卡片照常发出)。解析不了或没有可打的返回 `None`。
pub fn mask_emails_in_card(card: &str) -> Option<String> {
    mask_json(card, Shape::Card)
}

/// CoT 事件内容(JSON)里的字符串值全部打码;字符串本身又是 JSON 的(工具参数)递归进去,
/// 内层转义不被打断。CoT 里没有回传的值,不跳过任何键。
pub fn mask_emails_in_json(json: &str) -> Option<String> {
    mask_json(json, Shape::Event)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Card,
    Event,
}

fn mask_json(json: &str, shape: Shape) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(json).ok()?;
    mask_value(&mut value, shape).then(|| value.to_string())
}

fn mask_value(value: &mut serde_json::Value, shape: Shape) -> bool {
    let mut changed = false;
    match value {
        serde_json::Value::String(text) => {
            let nested = (shape == Shape::Event && text.trim_start().starts_with(['{', '[']))
                .then(|| mask_json(text, shape))
                .flatten();
            if let Some(masked) = nested.or_else(|| mask_email_addresses(text)) {
                *text = masked;
                changed = true;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                changed |= mask_value(item, shape);
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if shape == Shape::Event || !VALUE_KEYS.contains(&key.as_str()) {
                    changed |= mask_value(field, shape);
                }
            }
        }
        _ => {}
    }
    changed
}

fn is_local_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_domain_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
}

/// `at` 处的 `@` 两侧构成邮箱时返回 [用户名起点, 域名终点)。用户名不以 `.` 开头;域名至少
/// 两段、顶级域全是字母且不短于两位 —— 这样 `pkg@1.2.3`、`user@localhost`、`@scope/name`
/// 都不算。
fn email_span(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let mut local_start = at;
    while local_start > 0 && is_local_byte(bytes[local_start - 1]) {
        local_start -= 1;
    }
    while local_start < at && bytes[local_start] == b'.' {
        local_start += 1;
    }
    if local_start == at {
        return None;
    }
    let mut domain_end = at + 1;
    while domain_end < bytes.len() && is_domain_byte(bytes[domain_end]) {
        domain_end += 1;
    }
    while domain_end > at + 1 && matches!(bytes[domain_end - 1], b'.' | b'-') {
        domain_end -= 1;
    }
    let domain = &bytes[at + 1..domain_end];
    let tld_start = domain.iter().rposition(|byte| *byte == b'.')?;
    let tld = &domain[tld_start + 1..];
    let labels_ok = domain
        .split(|byte| *byte == b'.')
        .all(|label| !label.is_empty());
    (labels_ok && tld.len() >= 2 && tld.iter().all(u8::is_ascii_alphabetic))
        .then_some((local_start, domain_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_the_user_name_and_keeps_the_domain() {
        assert_eq!(
            mask_email_addresses("联系 probe.user@example.com 确认").as_deref(),
            Some("联系 p***@example.com 确认")
        );
        assert_eq!(
            mask_email_addresses("a@b.cn, zhang.san+tag@mail.example.org.").as_deref(),
            Some("a***@b.cn, z***@mail.example.org.")
        );
        assert_eq!(
            mask_email_addresses("..alice@example.com").as_deref(),
            Some("..a***@example.com"),
            "用户名不以点开头"
        );
    }

    /// 审核把这些地址也当邮箱拒(2026-10-16 实测),打码是它们送达的唯一办法。
    #[test]
    fn addresses_the_audit_rejects_are_masked_too() {
        for (text, masked) in [
            (
                "git clone git@github.com:org/repo.git",
                "git clone g***@github.com:org/repo.git",
            ),
            (
                "ssh://git@github.com/org/repo",
                "ssh://g***@github.com/org/repo",
            ),
            (
                "https://token@github.com/org/repo",
                "https://t***@github.com/org/repo",
            ),
            ("alice@example.com:)", "a***@example.com:)"),
        ] {
            assert_eq!(
                mask_email_addresses(text).as_deref(),
                Some(masked),
                "{text}"
            );
        }
    }

    #[test]
    fn leaves_text_without_emails_untouched() {
        for text in [
            "没有邮箱",
            "npm i -g @scope/pkg@0.6.9",
            "@larksuite/cli@latest",
            "user@localhost",
            "pkg@1.2.3",
            "<at user_id=\"ou_1\">王小明</at>",
            "foo @ bar.com",
        ] {
            assert_eq!(mask_email_addresses(text), None, "{text}");
        }
    }

    #[test]
    fn masking_is_idempotent() {
        let once = mask_email_addresses("probe.user@example.com").unwrap();
        assert_eq!(mask_email_addresses(&once), None);
    }

    #[test]
    fn json_strings_are_masked_after_unescaping() {
        let card = serde_json::json!({
            "schema": "2.0",
            "body": {"elements": [
                {"tag": "markdown", "content": "第一行\nprobe.user@example.com"},
                {"tag": "markdown", "content": "\u{00e9}alice@example.com"},
            ]},
        })
        .to_string();
        let masked: serde_json::Value =
            serde_json::from_str(&mask_emails_in_card(&card).unwrap()).unwrap();
        assert_eq!(
            masked["body"]["elements"][0]["content"],
            "第一行\np***@example.com"
        );
        assert_eq!(
            masked["body"]["elements"][1]["content"],
            "\u{00e9}a***@example.com"
        );
        assert_eq!(mask_emails_in_card(r#"{"body":"无邮箱"}"#), None);
        assert_eq!(mask_emails_in_card("not json"), None);
        assert_eq!(mask_emails_in_json("not json"), None);
    }

    /// 回调值与表单预填值会被飞书原样回传,打码会把用户的选择或原有内容写坏。
    #[test]
    fn callback_and_form_values_are_left_intact() {
        let card = serde_json::json!({
            "options": [{
                "text": {"tag": "plain_text", "content": "发给 alice@example.com"},
                "value": "发给 alice@example.com",
            }],
            "default_value": "每天把日报发给 alice@example.com",
            "behaviors": [{"type": "callback", "value": {"to": "alice@example.com"}}],
        })
        .to_string();
        let masked: serde_json::Value =
            serde_json::from_str(&mask_emails_in_card(&card).unwrap()).unwrap();
        assert_eq!(
            masked["options"][0]["text"]["content"],
            "发给 a***@example.com"
        );
        assert_eq!(masked["options"][0]["value"], "发给 alice@example.com");
        assert_eq!(masked["default_value"], "每天把日报发给 alice@example.com");
        assert_eq!(masked["behaviors"][0]["value"]["to"], "alice@example.com");
    }

    /// 卡片里长得像 JSON 的 markdown 正文按文本打码,不被解析后压成紧凑格式。
    #[test]
    fn card_text_that_looks_like_json_stays_text() {
        let card = serde_json::json!({
            "tag": "markdown",
            "content": "{\n  \"to\": \"alice@example.com\"\n}",
        })
        .to_string();
        let masked: serde_json::Value =
            serde_json::from_str(&mask_emails_in_card(&card).unwrap()).unwrap();
        assert_eq!(masked["content"], "{\n  \"to\": \"a***@example.com\"\n}");
    }

    /// CoT 事件没有回传的值,名为 value 的键照样打码,否则重发仍被拒。
    #[test]
    fn event_values_are_masked() {
        let event =
            serde_json::json!({"key": "user.email", "value": "alice@example.com"}).to_string();
        let masked: serde_json::Value =
            serde_json::from_str(&mask_emails_in_json(&event).unwrap()).unwrap();
        assert_eq!(masked["value"], "a***@example.com");
    }

    /// CoT 工具参数是 JSON 里再套一层 JSON 字符串;按解析后的值递归,内层转义不被打断。
    #[test]
    fn nested_json_strings_are_masked_without_breaking_escapes() {
        let args = serde_json::json!({"command": "\u{1b}alice@example.com"}).to_string();
        let event = serde_json::json!({"delta": args}).to_string();
        let masked: serde_json::Value =
            serde_json::from_str(&mask_emails_in_json(&event).unwrap()).unwrap();
        let inner: serde_json::Value =
            serde_json::from_str(masked["delta"].as_str().unwrap()).unwrap();
        assert_eq!(inner["command"], "\u{1b}a***@example.com");
    }
}
