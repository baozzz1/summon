//! 会话开场加载的消息协议与每轮数据共用的版本。

pub const VERSION: u8 = 1;

pub fn instructions(reactions: bool) -> String {
    summon_i18n::include_text!("message-protocol.md").replace(
        "{scheduled_reactions}",
        if reactions {
            summon_i18n::text!("rd075af580c")
        } else {
            ""
        },
    )
}

pub fn details(reactions: bool) -> String {
    let scheduled_reactions = if reactions {
        summon_i18n::text!("r0cada9ef00")
    } else {
        ""
    };
    summon_i18n::include_text!("message-protocol-detail.md")
        .replace("{react_cli}", if reactions { "、`react`" } else { "" })
        .replace("{scheduled_reactions}", scheduled_reactions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_defines_credentials_quotes_and_scheduled_limits() {
        let text = details(true);
        for rule in [
            "turn_id",
            "--turn-id",
            "工具列表",
            "不向用户展示",
            "origin=schedule",
            "output=result",
            "summon ask",
            "summon react",
            "自续上限",
            "summon guide quotes",
            "source=self",
            "body=unavailable",
        ] {
            assert!(text.contains(rule), "消息协议缺 {rule}");
        }
        assert!(!text.contains("+messages-mget"));
        assert!(!text.contains("{scheduled_reactions}"));
        assert!(text.contains(&format!("消息协议 v{VERSION}")));
    }

    #[test]
    fn protocol_injection_is_short_and_points_to_complete_rules() {
        let text = instructions(true);
        let detail = summon_i18n::include_text!("message-protocol-detail.md");
        assert!(text.chars().count() * 100 < detail.chars().count() * 65);
        assert!(text.contains("summon guide protocol"));
    }

    #[test]
    fn protocol_respects_the_reactions_switch() {
        let text = instructions(false);
        assert!(!text.contains("react"));
        assert!(text.contains("summon ask"));
        assert!(text.contains("send-image"));
        assert!(!details(false).contains("react"));
    }
}
