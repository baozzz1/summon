//! 日志进诊断包的规则。
//!
//! 日志是自由文本:写日志的地方上千处,工具那一行直接带命令原文,多行命令还会带出续行,所以不能
//! 按行原样收。诊断包只收模板白名单里的行:模板的占位符只装 id、计数、状态与时长;装着错误原文的
//! 占位符列进 `hide`,值换成 [`HIDDEN`]。不在白名单里的行只计数,续行一律不收。
//!
//! daemon 的模板逐条是源码里某个格式串的原文,`summon` crate 的守卫测试核对它们仍在源码里:格式串
//! 改了或删了,测试就会失败,白名单不会悄悄失效。

use crate::observe::export::People;

/// 被隐去的占位符值。
pub const HIDDEN: &str = "r3fea2c45af";

/// 一条白名单模板。
#[derive(Debug, Clone, Copy)]
pub struct Template {
    /// 源码里格式串的原文。`{…}` 是占位符,`{{` / `}}` 是字面花括号。
    pub text: &'static str,
    /// 值要换成 [`HIDDEN`] 的占位符序号(从 0 数)。
    pub hide: &'static [usize],
    /// (名字, 飞书 id) 两个占位符是同一个人:名字换成与 id 相同的编号。
    pub person: Option<(usize, usize)>,
}

impl Template {
    /// Recognize historical records independently of the currently selected language.
    pub fn localized(self) -> [Self; 2] {
        [
            crate::i18n::Language::Chinese,
            crate::i18n::Language::English,
        ]
        .map(|language| Self {
            text: crate::i18n::label_in(language, self.text),
            ..self
        })
    }
}

const fn t(text: &'static str) -> Template {
    Template {
        text,
        hide: &[],
        person: None,
    }
}

const fn hiding(text: &'static str, hide: &'static [usize]) -> Template {
    Template {
        text,
        hide,
        person: None,
    }
}

/// daemon.log 的白名单。新增前先确认每个占位符装的是什么;装错误原文的列进 `hide`。
pub const DAEMON_TEMPLATES: &[Template] = &[
    t("rdb7ecd6719"),
    t("r2ab0fbbbe2"),
    t("ra43e582263"),
    Template {
        text: "rb0bb1f0190",
        hide: &[],
        person: Some((3, 4)),
    },
    t("rf6efeac2f4"),
    t("r1ee7df782c"),
    t("r94979d38eb"),
    t("rba7f2449c7"),
    t("r30a78ba6cd"),
    t("r4920f2e294"),
    t("r9ae8d8c755"),
    t("r306bcf36a6"),
    t("rc0a834f9ed"),
    t("[{name}] headless-watchdog chat={tag} silence_secs={} active_secs={}"),
    t("rdfd63fe894"),
    t("raf5c4c9184"),
    t("r93c4235762"),
    t("rbe1151f1ed"),
    t("r54f954627c"),
    t("r3d27d651e6"),
    t("r7dcf2690bb"),
    t("re722a6535f"),
    t("rbf263c9ce0"),
    t("r55f94b5d48"),
    t("rfd086700e4"),
    t("ree0a51c905"),
    t("r6c75613119"),
    t("r538e825211"),
    t("r63cd323d75"),
    t("r650759e49d"),
    hiding("rcd4bb55099", &[3]),
    t("rd45485f67f"),
    t("rc1a3d4a860"),
    t("r56ca44effb"),
    t("r669e439b69"),
    t("r89ce9dfd2a"),
    t("CoT delivery chat={chat} cot={cot} batch={batch} events={} outcome={} confirmed={:?}"),
];

/// sidecar.log 的白名单(去掉行首时间戳之后)。`[summon-sidecar]` 两条出自
/// `sidecar/feishu-connector/main.go`,其余出自飞书 SDK。
pub const SIDECAR_TEMPLATES: &[Template] = &[
    t("[Info] [event-dispatch is ready]"),
    t("[Error] {} receive message failed, err: {}"),
    t("[Error] {} connect failed, err: {}"),
    t("re7e6dbe4b0"),
    t("r8adecc4682"),
];

/// 模板切成字面段:`n` 个占位符对应 `n + 1` 段(首尾可以是空串)。
fn segments(template: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.last_mut().unwrap().push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.last_mut().unwrap().push('}');
            }
            '{' => {
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                }
                out.push(String::new());
            }
            other => out.last_mut().unwrap().push(other),
        }
    }
    out
}

/// 模板里最长的一段字面文字的字符数。守卫测试据此拒绝过于宽泛的模板。
pub fn longest_literal(template: &str) -> usize {
    segments(template)
        .iter()
        .map(|s| s.trim().chars().count())
        .max()
        .unwrap_or(0)
}

/// 按模板切出各占位符的值;对不上返回 `None`。首段从行首对齐,末段对齐到行尾。
pub fn capture<'a>(template: &str, line: &'a str) -> Option<Vec<&'a str>> {
    let segs = segments(template);
    let (first, rest) = segs.split_first()?;
    let mut pos = first.len();
    if !line.starts_with(first.as_str()) {
        return None;
    }
    let mut values = Vec::with_capacity(rest.len());
    for (i, seg) in rest.iter().enumerate() {
        let last = i + 1 == rest.len();
        let at = if last {
            if seg.is_empty() {
                line.len()
            } else {
                if !line.ends_with(seg.as_str()) || line.len() - seg.len() < pos {
                    return None;
                }
                line.len() - seg.len()
            }
        } else if seg.is_empty() {
            // 两个占位符紧挨着:分不出边界,前一个取空。
            pos
        } else {
            pos + line[pos..].find(seg.as_str())?
        };
        values.push(&line[pos..at]);
        pos = at + seg.len();
    }
    Some(values)
}

/// 按模板重写一行:占位符的值经 `rewrite` 处理后放回原位。
fn rebuild(template: &Template, line: &str, people: &mut People, home: &str) -> Option<String> {
    let mut values: Vec<String> = capture(template.text, line)?
        .into_iter()
        .map(str::to_string)
        .collect();
    let segs = segments(template.text);
    // 行首 `[{}]` 是 agent 别名:不许跨过 `] ` 吞进别的文字,否则一条恰好含模板字样的命令行
    // 也能对上,把命令原文带进来。
    if template.text.starts_with("[{")
        && values
            .first()
            .is_some_and(|v| v.is_empty() || v.contains([' ', ']']))
    {
        return None;
    }
    if let Some((name, id)) = template.person {
        // 显示名自带括号(如「示例(产品)」)时按第一个 `(` 切会切错:名字与 id 合起来按最后
        // 一个分隔重新切;切出来的 id 不像一个 id 就整行不收。
        let sep = &segs[name + 1];
        let joined = format!("{}{sep}{}", values[name], values[id]);
        let (n, i) = joined.rsplit_once(sep.as_str())?;
        if i.is_empty() || i.contains([' ', '(', ')']) {
            return None;
        }
        values[name] = n.to_string();
        values[id] = i.to_string();
    }
    let mut out = segs[0].clone();
    for (i, value) in values.iter().enumerate() {
        let shown = if template.hide.contains(&i) {
            ::summon_core::i18n::label(HIDDEN).to_string()
        } else if let Some((_, id)) = template.person.filter(|(name, _)| *name == i) {
            people.link(value, &values[id])
        } else {
            super::tilde(&people.replace_ids(value), home)
        };
        out.push_str(&shown);
        out.push_str(&segs[i + 1]);
    }
    Some(out)
}

/// 一条日志在诊断包里的样子:命中白名单就是重写后的那一行,否则 `None`。只看首行,续行不收。
pub fn diag_line(
    templates: &[Template],
    first_line: &str,
    people: &mut People,
    home: &str,
) -> Option<String> {
    templates
        .iter()
        .flat_map(|template| template.localized())
        .find_map(|template| rebuild(&template, first_line, people, home))
}

/// daemon.log 一条的首行去掉 `[时间] [summon] ` 前缀,剩下的是 `log()` 收到的原文。
pub fn daemon_message(first_line: &str) -> Option<&str> {
    let rest = first_line.strip_prefix('[')?;
    let (_, rest) = rest.split_once("] ")?;
    rest.strip_prefix("[summon] ")
}

/// sidecar.log 一条的首行去掉行首的 `YYYY/MM/DD HH:MM:SS[.ffffff] `。
pub fn sidecar_message(first_line: &str) -> &str {
    let bytes = first_line.as_bytes();
    let stamped = bytes.len() > 20 && bytes[4] == b'/' && bytes[7] == b'/' && bytes[10] == b' ';
    if !stamped {
        return first_line;
    }
    match first_line[11..].find(' ') {
        Some(space) => &first_line[11 + space + 1..],
        None => first_line,
    }
}

/// 行首 `[别名] ` 写的是哪个 agent;不是这个形态返回 `None`。
pub fn agent_prefix(message: &str) -> Option<&str> {
    let rest = message.strip_prefix('[')?;
    let (name, _) = rest.split_once("] ")?;
    (!name.is_empty() && !name.contains(' ')).then_some(name)
}

/// 这条日志说的是不是本会话:正文里出现本会话的任一个键(tag、chat_id、turn id、message_id)。
pub fn mentions_any(text: &str, keys: &[String]) -> bool {
    keys.iter()
        .any(|key| !key.is_empty() && text.contains(key.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_splits_placeholders_between_literals() {
        assert_eq!(
            capture(
                "[{}] 新会话 chat={chat_id} → {tmux_name}",
                "[a] 新会话 chat=oc_1 → soul-a-g1"
            ),
            Some(vec!["a", "oc_1", "soul-a-g1"])
        );
        assert_eq!(
            capture(
                "[{}] 新会话 chat={chat_id} → {tmux_name}",
                "[a] 别的 chat=oc_1"
            ),
            None
        );
        assert_eq!(
            capture(
                "[{}] 会话已清理摘除 chat={chat_id}",
                "[a] 会话已清理摘除 chat=oc_1 多出来"
            ),
            Some(vec!["a", "oc_1 多出来"])
        );
        assert_eq!(
            capture(
                "[{}] 孤儿消息重投 {redelivered}/{count} chat={chat_id}",
                "[a] 孤儿消息重投 1/2 chat=oc_1"
            ),
            Some(vec!["a", "1", "2", "oc_1"])
        );
    }

    #[test]
    fn escaped_braces_are_literal() {
        assert_eq!(
            segments("a {{x}} {} b"),
            vec!["a {x} ".to_string(), " b".to_string()]
        );
    }

    #[test]
    fn tool_lines_never_match_the_allowlist() {
        let mut people = People::default();
        let line = "[a] 工具: Bash(echo CANARY)";
        assert_eq!(
            diag_line(DAEMON_TEMPLATES, line, &mut people, "/home/x"),
            None
        );
    }

    #[test]
    fn hidden_placeholders_drop_their_value_and_people_become_numbers() {
        let mut people = People::default();
        let out = diag_line(
            DAEMON_TEMPLATES,
            "[a] 接管失败(注入于 10-29 10:00:00),转人工恢复卡 chat=g1: CANARY detail",
            &mut people,
            "/home/x",
        )
        .unwrap();
        assert!(!out.contains("CANARY"), "{out}");
        assert!(out.ends_with(::summon_core::i18n::label(HIDDEN)), "{out}");

        let out = diag_line(
            DAEMON_TEMPLATES,
            "[a] 处理任务 chat=oc_1 msg=om_1 from=示例用户(ou_0123456789abcdef)",
            &mut people,
            "/home/x",
        )
        .unwrap();
        assert_eq!(out, "[a] 处理任务 chat=oc_1 msg=om_1 from=user-1(user-1)");

        let out = diag_line(
            DAEMON_TEMPLATES,
            "收到 message: msg=om_2 sender=ou_0123456789abcdef chat=oc_1 type=group textlen=3 mentions=1 附件=0 投递延迟=2s",
            &mut people,
            "/home/x",
        )
        .unwrap();
        assert!(out.contains("sender=user-1 "), "{out}");
    }

    #[test]
    fn the_agent_placeholder_cannot_swallow_a_command_line() {
        let mut people = People::default();
        let line = "[a] 工具: Bash(echo CANARY] 控制卡打断 → chat=oc_1)";
        assert_eq!(
            capture("[{}] 控制卡打断 → chat={}", line).map(|v| v.len()),
            Some(2)
        );
        assert_eq!(
            diag_line(DAEMON_TEMPLATES, line, &mut people, "/home/x"),
            None
        );
    }

    #[test]
    fn display_names_with_parentheses_are_numbered_whole() {
        let mut people = People::default();
        let out = diag_line(
            DAEMON_TEMPLATES,
            "[a] 处理任务 chat=oc_1 msg=om_1 from=示例(产品)(ou_0123456789abcdef)",
            &mut people,
            "/home/x",
        )
        .unwrap();
        assert_eq!(out, "[a] 处理任务 chat=oc_1 msg=om_1 from=user-1(user-1)");
        assert_eq!(people.alias("示例(产品)"), "user-1");
        assert_eq!(
            diag_line(
                DAEMON_TEMPLATES,
                "[a] 处理任务 chat=oc_1 msg=om_1 from=示例(not an id)",
                &mut people,
                "/home/x",
            ),
            None
        );
    }

    #[test]
    fn message_prefixes_are_stripped() {
        assert_eq!(
            daemon_message("[2026-10-29 10:00:00] [summon] [a] 新会话 chat=x → y"),
            Some("[a] 新会话 chat=x → y")
        );
        assert_eq!(daemon_message("continuation line"), None);
        assert_eq!(
            sidecar_message("2026/09/16 18:06:58.399626 [summon-sidecar] websocket 断开 app=cli_x 已连=1s 3s 后重连"),
            "[summon-sidecar] websocket 断开 app=cli_x 已连=1s 3s 后重连"
        );
        assert_eq!(
            sidecar_message("[Error] 2026/07/31 05:35:25.2 x"),
            "[Error] 2026/07/31 05:35:25.2 x"
        );
        assert_eq!(agent_prefix("[alpha] 新会话"), Some("alpha"));
        assert_eq!(agent_prefix("收到 message: msg=1"), None);
    }

    #[test]
    fn translated_log_slots_keep_redaction_and_person_positions() {
        fn slots(value: &str) -> Vec<&str> {
            let mut result = Vec::new();
            let mut rest = value;
            while let Some(start) = rest.find('{') {
                rest = &rest[start + 1..];
                if let Some(literal) = rest.strip_prefix('{') {
                    rest = literal;
                    continue;
                }
                let end = rest.find('}').expect("balanced format placeholder");
                result.push(&rest[..end]);
                rest = &rest[end + 1..];
            }
            result
        }
        for template in DAEMON_TEMPLATES.iter().chain(SIDECAR_TEMPLATES) {
            let [zh, en] = template.localized();
            assert_eq!(slots(zh.text), slots(en.text), "{}", template.text);
        }
        for language in [
            crate::i18n::Language::Chinese,
            crate::i18n::Language::English,
        ] {
            crate::i18n::with_language(language, || {
                let mut people = People::default();
                for line in [
                    "[a] 接管失败(注入于 time),转人工恢复卡 chat=g1: PRIVATE-CANARY",
                    "[a] Reattachment failed (injected at time); using manual recovery card chat=g1: PRIVATE-CANARY",
                ] {
                    let output = diag_line(DAEMON_TEMPLATES, line, &mut people, "/home/x").unwrap();
                    assert!(!output.contains("PRIVATE-CANARY"));
                    assert!(output.ends_with(crate::i18n::label(HIDDEN)));
                }
            });
        }
    }

    #[test]
    fn every_template_has_a_distinctive_literal() {
        for template in DAEMON_TEMPLATES
            .iter()
            .chain(SIDECAR_TEMPLATES)
            .flat_map(|template| template.localized())
        {
            assert!(
                longest_literal(template.text) >= 6,
                "模板太宽泛,几乎什么行都能对上:{}",
                template.text
            );
            for &i in template.hide {
                assert!(i < segments(template.text).len() - 1, "{}", template.text);
            }
            if let Some((name, id)) = template.person {
                assert_eq!(id, name + 1, "名字与 id 两个占位符要相邻:{}", template.text);
            }
        }
    }
}
