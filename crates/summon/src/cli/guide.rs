//! `summon guide` —— agent 视角的 summon 使用手册。
//!
//! 内容是**编译进二进制的内置资产**(`locales/assets/<language>/guide/*.md`),与版本同生共死:不存在
//! 「盘上旧手册」,也不需要联网。设计为 JIT 知识交付 —— 不写 agent 目录、不占常驻上下文,
//! agent 在用户需要时跑一条命令取走(见 `docs/spec/2026-08-20-oss-onboarding-experience-design.md`)。
//!
//! agent 友好硬约束:纯 markdown 到 stdout、无交互、无颜色码;未知章节 exit 非 0 并列出
//! 可用章节(让 agent 知道自己拼错了,而不是拿到一段错误文本当答案)。

/// 手册里的占位符换成升级说明:`{{upgrade}}`(manage 的升级条目)与 `{{upgrade_fix}}`
/// (troubleshooting 的「升级到最新版」)。
const UPGRADE: &str = "r948e9b5246";
const UPGRADE_FIX: &str = "re506263175";

fn fill_upgrade(text: &'static str) -> std::borrow::Cow<'static, str> {
    if !text.contains("{{upgrade") {
        return text.into();
    }
    text.replace("{{upgrade}}", ::summon_core::i18n::label(UPGRADE))
        .replace("{{upgrade_fix}}", ::summon_core::i18n::label(UPGRADE_FIX))
        .into()
}

/// 可用章节名(小写)。裸 `summon guide` = 索引。
const SECTIONS: &[&str] = &[
    "protocol",
    "quotes",
    "reactions",
    "commands",
    "handoff",
    "config",
    "mentionless",
    "troubleshooting",
    "manage",
];

/// 打印指定章节。`None` = 索引;未知章节返回 `false`(调用方 exit 非 0)。
/// 章节名大小写不敏感(agent 可能打成 `Config`)。
pub fn print(section: Option<&str>) -> bool {
    let Some(body) = resolve(section) else {
        return false;
    };
    // 版本头:让 agent 知道自己读的手册对应哪个二进制(排查「升级后行为变了」时的锚点)。
    println!(
        "{}",
        summon_i18n::format!("r299eedec5c", crate::version::VERSION)
    );
    println!("{body}");
    true
}

/// 章节解析(纯函数,便于单测):`None` = 索引;未知章节 = `None`。
fn resolve(section: Option<&str>) -> Option<std::borrow::Cow<'static, str>> {
    match section.map(str::to_ascii_lowercase) {
        None => Some(summon_i18n::include_text!("guide/index.md").into()),
        Some(s) if s == "protocol" => Some(
            format!(
                "{}\n\n{}",
                summon_core::messaging::prompt_protocol::details(false).trim_end(),
                summon_i18n::include_text!("guide/protocol.md")
            )
            .into(),
        ),
        Some(s) if s == "quotes" => Some(summon_i18n::include_text!("guide/quotes.md").into()),
        Some(s) if s == "reactions" => {
            Some(summon_i18n::include_text!("guide/reactions.md").into())
        }
        Some(s) if s == "commands" => Some(summon_i18n::include_text!("guide/commands.md").into()),
        Some(s) if s == "handoff" => Some(summon_i18n::include_text!("guide/handoff.md").into()),
        Some(s) if s == "mentionless" => {
            Some(summon_i18n::include_text!("guide/mentionless.md").into())
        }
        Some(s) if s == "config" => Some(summon_i18n::include_text!("guide/config.md").into()),
        Some(s) if s == "troubleshooting" => Some(fill_upgrade(summon_i18n::include_text!(
            "guide/troubleshooting.md"
        ))),
        Some(s) if s == "manage" => {
            Some(fill_upgrade(summon_i18n::include_text!("guide/manage.md")))
        }
        Some(_) => None,
    }
}

/// 可用章节列表(未知章节报错时用)。
pub fn available_sections() -> &'static [&'static str] {
    SECTIONS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写交接的指令与手册列的是同一份模板、同一个标签:手册与指令分开写,改了一边另一边就过时。
    #[test]
    fn handoff_guide_carries_the_builtin_template_and_tag() {
        let guide = resolve(Some("handoff")).unwrap();
        for item in summon_core::handoff::TEMPLATE {
            let item = summon_core::i18n::label(item);
            assert!(guide.contains(item), "handoff 节缺模板项 {item}");
        }
        assert!(guide.contains(&format!(
            "{}交接文件的绝对路径{}",
            summon_core::feed_tag::HANDOFF_TAG_OPEN,
            summon_core::feed_tag::HANDOFF_TAG_CLOSE
        )));
        assert!(guide.contains("tasks/<日期-主题>/HANDOFF.md"));
    }

    #[test]
    fn guide_sections_resolve_and_cover_key_surface() {
        // 裸 guide = 索引,含分节导航
        let index = resolve(None).unwrap();
        assert!(index.contains("summon guide protocol"));
        assert!(index.contains("summon guide quotes"));
        assert!(index.contains("summon guide commands"));
        assert!(index.contains("summon guide config"));
        assert!(index.contains("summon guide troubleshooting"));
        assert!(index.contains("summon guide manage"));
        assert!(index.contains("summon guide handoff"));
        // 命令面覆盖关键命令(漏了说明手册与命令面脱节)
        let commands = resolve(Some("commands")).unwrap();
        for cmd in [
            "/help",
            "/session",
            "/quota",
            "/model",
            "/fast",
            "/bridge",
            "/quit",
            "/reset",
            "/handoff",
            "/term",
            "/dash",
            "/summon-export",
            "summon session export",
            "summon dashboard open",
            "summon usage",
            "summon insights",
            "summon preview",
            "summon schedules edit",
        ] {
            assert!(commands.contains(cmd), "commands 节缺 {cmd}");
        }
        // 管理面覆盖新建 agent 的三条路
        let manage = resolve(Some("manage")).unwrap();
        for cmd in ["summon onboard", "agents add", "summon bind", "agents.yaml"] {
            assert!(manage.contains(cmd), "manage 节缺 {cmd}");
        }
        // 配置面覆盖高频旋钮 + 「什么时候建议配」
        let config = resolve(Some("config")).unwrap();
        for key in [
            "idle_ttl_secs",
            "idle_retire_secs",
            "steer_transcript",
            "hide_thinking",
        ] {
            assert!(config.contains(key), "config 节缺 {key}");
        }
        for setting in summon_core::settings::SETTINGS {
            assert!(
                config.contains(setting.key),
                "config 节缺分级开关 {}",
                setting.key
            );
            assert!(
                config.contains(setting.config_key),
                "config 节缺 YAML 字段 {}",
                setting.config_key
            );
        }
        // 操作规程节覆盖三件事各自的入口
        let protocol = resolve(Some("protocol")).unwrap();
        for key in ["summon remind", "summon chat-id", "Read"] {
            assert!(protocol.contains(key), "protocol 节缺 {key}");
        }
        // 排错节覆盖最常见症状
        let trouble = resolve(Some("troubleshooting")).unwrap();
        assert!(trouble.contains("不回消息"));
        assert!(trouble.contains("summon doctor"));
    }

    /// guide config 节写的每个 `messaging.<字段>` 都必须是 `Messaging` 的真实字段。
    ///
    /// 2026-08-20 真实踩到:config 节写着 `messaging.continuation_cap_secs`,而真实字段叫
    /// `background_continuation_max_secs`。手册是给 agent 读的,agent 会照它建议用户改配置
    /// —— 名字错了,用户改完不生效,还会以为是自己配错。**字段名必须机器可核,不能靠人眼**。
    #[test]
    fn guide_config_field_names_exist_in_messaging_struct() {
        let known: std::collections::HashSet<String> =
            match serde_json::to_value(summon_core::config::builtin_messaging()) {
                Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
                other => panic!("Messaging 应序列化成对象,实际:{other:?}"),
            };
        let mut missing = Vec::new();
        for tail in resolve(Some("config"))
            .unwrap()
            .split("`messaging.")
            .skip(1)
        {
            let field: String = tail
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '_')
                .collect();
            if !field.is_empty() && !known.contains(&field) {
                missing.push(field);
            }
        }
        assert!(
            missing.is_empty(),
            "guide config 节引用了 Messaging 里不存在的字段:{missing:?}"
        );
    }

    /// 按需加载的契约:操作细节搬进 `guide protocol` 后,每条的**禁令**必须留在常驻模板里。
    ///
    /// agent 只有意识到「我要做这件事」时才会去查 guide。而最严重的错 —— 手改 `schedules.yaml`
    /// 搞挂全部定时器、拿 `$SUMMON_CHAT_ID` 读到别人的群 —— 恰恰发生在它**没意识到**要查的时候。
    /// 所以禁令留常驻、细节进 guide:不查也不会犯致命错,查了才知道怎么做对。
    /// 这个分工不能靠人眼维护,下次整理模板时很容易把禁令一起搬走。
    #[test]
    fn guide_protocol_keeps_prohibitions_resident_and_details_out_of_line() {
        let core = summon_core::config::builtin_messaging().system_prompt_core;
        let protocol = resolve(Some("protocol")).unwrap();

        // 禁令 + 索引必须留在常驻模板
        for ban in ["schedules.yaml", "SUMMON_CHAT_ID", "summon guide protocol"] {
            assert!(core.contains(ban), "常驻模板缺禁令或索引:{ban}");
        }
        // 操作细节只能在 guide 里:留在常驻就等于没搬
        for detail in [
            "--page-token",
            "threads-messages-list",
            "--output progress",
            "连续 3 轮",
        ] {
            assert!(protocol.contains(detail), "protocol 节缺操作细节:{detail}");
            assert!(!core.contains(detail), "操作细节仍留在常驻模板:{detail}");
        }
    }

    /// 表情段走同一条分工,但**它的 guide 章节必须独立**。
    ///
    /// 命令和指路句常驻在表情段里;「贴哪个」是用到才查的参考表。
    ///
    /// 关键在章节归属:`agent_reactions` 关掉时常驻模板里的表情段整段消失,agent 因此
    /// **不知道有这个章节**;而 `summon guide protocol` 是常驻模板主动要求它去跑的 ——
    /// 速查表若混在 protocol 里,开关关掉后 agent 照样会读到「每次回复都必须贴」。
    #[test]
    fn reaction_command_stays_resident_and_the_lookup_table_lives_in_its_own_section() {
        let reactions = summon_core::config::builtin_messaging().system_prompt_reactions;
        let protocol = resolve(Some("protocol")).unwrap();
        let table = resolve(Some("reactions")).unwrap();

        for rule in ["summon react", "summon guide reactions"] {
            assert!(reactions.contains(rule), "常驻表情段缺命令或索引:{rule}");
        }
        for key in ["THUMBSUP", "OneSecond", "CrossMark"] {
            assert!(table.contains(key), "reactions 节缺表情速查:{key}");
            assert!(!reactions.contains(key), "速查表仍留在常驻表情段:{key}");
            // 开关关掉后 agent 仍会按常驻约定去跑 `guide protocol` —— 速查表混在那里就等于
            // 「关了还教」。
            assert!(!protocol.contains(key), "速查表混进了 protocol 节:{key}");
        }
        // 指路句只在表情段里 —— 段随开关消失,指路也就消失了。
        assert!(!protocol.contains("summon guide reactions"));
    }

    /// 随渠道填的空位都要填上:漏一个,agent 读到的就是一串 `{{…}}`。
    #[test]
    fn channel_slots_are_filled_in_every_section() {
        for section in SECTIONS {
            let body = resolve(Some(section)).unwrap();
            assert!(!body.contains("{{upgrade"), "{section}: {body}");
        }
        assert!(resolve(Some("troubleshooting"))
            .unwrap()
            .contains("scripts/deploy.sh --install"));
    }

    #[test]
    fn guide_unknown_section_is_none_and_case_insensitive() {
        assert!(resolve(Some("Config")).is_some(), "大小写不敏感");
        assert!(resolve(Some("CONFIG")).is_some());
        assert!(resolve(Some("nonsense")).is_none(), "未知章节 = None");
    }

    #[test]
    fn guide_assets_contain_no_ansi_escapes() {
        // agent 友好硬约束:纯文本,无颜色码(agent 会把转义序列当内容读)
        for asset in [
            summon_i18n::include_text!("guide/index.md"),
            summon_i18n::include_text!("guide/commands.md"),
            summon_i18n::include_text!("guide/config.md"),
            summon_i18n::include_text!("guide/troubleshooting.md"),
            summon_i18n::include_text!("guide/manage.md"),
            summon_i18n::include_text!("guide/quotes.md"),
        ] {
            assert!(!asset.contains('\u{1b}'), "guide 资产含 ANSI 转义");
        }
    }

    #[test]
    fn guide_explains_short_metadata_and_fetches_quotes_only_when_needed() {
        let protocol = resolve(Some("protocol")).unwrap();
        let quotes = resolve(Some("quotes")).unwrap();
        let filled = summon_core::config::builtin_messaging().system_prompt_filled(
            "/tmp/emoji.md",
            true,
            summon_core::config::PrimerSections {
                reactions: false,
                ..summon_core::config::PrimerSections::ALL
            },
        );
        assert!(filled.contains("【Summon 消息协议 v1】"));
        assert!(!filled.contains("{message_protocol}"));
        assert!(!filled.contains("+messages-mget"));
        assert!(protocol.contains("turn_id"));
        assert!(protocol.contains("origin=schedule"));
        assert!(protocol.contains("output=result"));
        assert!(!protocol.contains("summon react"));
        assert!(!protocol.contains("summon.react"));
        for detail in [
            "lark-cli im +messages-mget --as bot --message-ids <message_id>",
            "需要原文才能判断时",
            "发送人、时间及正文",
            "终端卡",
        ] {
            assert!(quotes.contains(detail), "引用手册缺 {detail}");
        }
    }
}
