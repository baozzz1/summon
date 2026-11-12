//! 终态回复卡的组装(纯件):正文、角标行与时刻、会话列表预览、本地文件按钮。
//!
//! chat 线程的两条终态投递(单卡收尾 `deliver_final_reply`、总结卡 `finalize_turn_summary`)与
//! 控制台的飞书效果预览共用这一份,预览里看到的就是 summon 此刻会发出去的那张卡。

use crate::feishu::{
    card_turn_summary_pieces, card_with_feed_summary, card_with_preview_buttons, PreviewButton,
};
use crate::image::ReplyPiece;

/// 组一张回复卡要的全部输入。正文已去掉 feed 标签、按单卡预算切好(切分要按飞书前端的转义
/// 预算算,不在这里)。
#[derive(Debug, Clone, Copy)]
pub struct ReplyInput<'a> {
    /// 卡里的正文:纯文本,或与内联图片按原文顺序交替的片段。
    pub pieces: &'a [ReplyPiece],
    /// 已渲染的角标行(见 `TurnBadge::render`);`None` = 尾部只有时刻。
    pub badge: Option<&'a str>,
    /// 尾部的本地时刻(`HH:MM`)。
    pub timestamp: &'a str,
    /// agent 在回复末尾给的合格总结;有就用它做会话列表预览,没有就保留按正文生成的预览。
    pub feed: Option<&'a str>,
    /// 回复里提到的本地文件。投递路径传空:按钮由飞书前端在发送时挂(那里才有预览服务签链接)。
    pub preview_buttons: &'a [PreviewButton],
}

/// 组好的回复卡。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedReply {
    pub card_json: String,
    /// 会话列表里这条消息的预览(卡片 `config.summary.content`);卡片没有预览时为 `None`。
    pub feed: Option<String>,
}

/// 组回复卡:正文打头,分隔线后一行灰色小字「角标 · 时刻」,会话列表预览优先用 agent 的总结,
/// 末尾挂文件按钮。
pub fn compose_reply_card(input: &ReplyInput<'_>) -> ComposedReply {
    let mut card = card_turn_summary_pieces(input.pieces, Some(input.timestamp), input.badge);
    if let Some(summary) = input.feed {
        card = card_with_feed_summary(&card, summary);
    }
    if !input.preview_buttons.is_empty() {
        card = card_with_preview_buttons(&card, input.preview_buttons).unwrap_or(card);
    }
    let feed = serde_json::from_str::<serde_json::Value>(&card)
        .ok()
        .and_then(|value| {
            value
                .pointer("/config/summary/content")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    ComposedReply {
        card_json: card,
        feed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed_tag::split_feed_tag;
    use crate::feishu::TurnBadge;

    fn text(body: &str) -> Vec<ReplyPiece> {
        vec![ReplyPiece::Text(body.to_string())]
    }

    fn compose(body: &str, badge: Option<&str>, feed: Option<&str>) -> ComposedReply {
        compose_reply_card(&ReplyInput {
            pieces: &text(body),
            badge,
            timestamp: "10:24",
            feed,
            preview_buttons: &[],
        })
    }

    #[test]
    fn plain_text_card_is_the_turn_summary_card() {
        let composed = compose("结论", Some("claude · Opus 5"), None);
        assert_eq!(
            composed.card_json,
            crate::feishu::card_turn_summary("结论", Some("10:24"), Some("claude · Opus 5"))
        );
    }

    #[test]
    fn feed_tag_becomes_summary() {
        let tagged =
            split_feed_tag("CI 挂在单测上,已修好。\n\n<summon-feed>CI 已修复</summon-feed>");
        let composed = compose(&tagged.body, None, tagged.summary.as_deref());
        assert_eq!(composed.feed.as_deref(), Some("CI 已修复"));
        assert!(
            !composed.card_json.contains("summon-feed"),
            "标签不进正文: {}",
            composed.card_json
        );
    }

    #[test]
    fn no_tag_falls_back_to_body_head() {
        let tagged = split_feed_tag("CI 挂在单测上,已修好。");
        let composed = compose(&tagged.body, None, tagged.summary.as_deref());
        assert_eq!(composed.feed.as_deref(), Some("CI 挂在单测上,已修好。"));
    }

    #[test]
    fn effort_badge_follows_switch() {
        let badge = |hide_effort: bool| TurnBadge {
            cli: "claude".into(),
            model: Some("Opus 5".into()),
            effort: Some("high".into()),
            hide_effort,
            ..Default::default()
        };
        let shown = badge(false).render();
        let hidden = badge(true).render();
        let on = compose("结论", shown.as_deref(), None).card_json;
        let off = compose("结论", hidden.as_deref(), None).card_json;
        assert!(on.contains("claude · Opus 5 · high · 10:24"), "{on}");
        assert!(off.contains("claude · Opus 5 · 10:24"), "{off}");
        assert!(!off.contains("high"), "{off}");
    }

    #[test]
    fn preview_buttons_go_at_the_end() {
        let buttons = [PreviewButton {
            label: "report.md".into(),
            token: "t".into(),
            is_dir: false,
        }];
        let composed = compose_reply_card(&ReplyInput {
            pieces: &text("日志在 `/tmp/report.md`"),
            badge: None,
            timestamp: "10:24",
            feed: None,
            preview_buttons: &buttons,
        });
        let card: serde_json::Value = serde_json::from_str(&composed.card_json).unwrap();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements.last().unwrap()["element_id"],
            "preview_files",
            "{}",
            composed.card_json
        );
    }
}
