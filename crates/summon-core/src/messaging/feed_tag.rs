//! 最终回复末尾的两种标签:feed 总结 `<summon-feed>…</summon-feed>` 与交接文件
//! `<summon-handoff>…</summon-handoff>`。
//!
//! agent 按常驻约定在本轮最终回复的末尾写一句不超过 [`FEED_TAG_MAX_CHARS`] 字的总结,summon 把它从
//! 用户可见的正文里拿掉,放进飞书会话列表的预览(卡片 `config.summary`)。`/handoff` 那一轮 agent
//! 还会在末尾写交接文件的绝对路径,它同样不进正文,由 [`handoff_path`] 取出。两种标签先后不限;
//! 只认回复末尾的那几个:正文中间、代码块里出现的同名标签是在讨论或举例,原样保留。

use std::borrow::Cow;

pub const FEED_TAG_OPEN: &str = "<summon-feed>";
pub const FEED_TAG_CLOSE: &str = "</summon-feed>";
pub const HANDOFF_TAG_OPEN: &str = "<summon-handoff>";
pub const HANDOFF_TAG_CLOSE: &str = "</summon-handoff>";

/// 总结的字数上限,按字符计(汉字、字母、标点都算一个)。会话列表那一行只放得下这么多。
pub const FEED_TAG_MAX_CHARS: usize = 14;

/// 拆出来的回复:去掉标签后的正文,以及合格的总结。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedTagged<'a> {
    pub body: Cow<'a, str>,
    /// 标签内容合格时才有;不合格(空、过长、带换行或尖括号)时标签照样从正文拿掉,但不拿来当预览,
    /// 预览回到正文摘要。
    pub summary: Option<String>,
}

/// 拆出回复末尾的总结标签,末尾的交接标签一并从正文拿掉。末尾没有总结标签时 `summary` 为 None。
///
/// 闭标签必须是回复的最后一样东西,开标签不在代码块里。开标签与闭标签同在最后一行时,开标签前
/// 可以有正文;标签分行写时(模型常这么写),开标签必须在它那一行的开头 —— 否则正文里提到开标签
/// 的一句话会让中间几段正文被当成标签内容吞掉。拿掉标签后正文为空时,正文用标签里的话(合格时
/// 就是总结本身):一张空卡比一句短话更糟;标签里也没有话时整段原样保留。
pub fn split_feed_tag(reply: &str) -> FeedTagged<'_> {
    let reply = match without_handoff_tag(reply) {
        "" => reply,
        body => body,
    };
    let untouched = || FeedTagged {
        body: Cow::Borrowed(reply),
        summary: None,
    };
    let Some((open, inner)) = trailing_tag(reply, FEED_TAG_OPEN, FEED_TAG_CLOSE) else {
        return untouched();
    };
    let summary = normalize_summary(inner);
    let body = without_handoff_tag(reply[..open].trim_end());
    let body = if body.is_empty() {
        let spoken = collapse_whitespace(inner);
        if spoken.is_empty() {
            return untouched();
        }
        summary.clone().unwrap_or(spoken)
    } else {
        body.to_string()
    };
    FeedTagged {
        body: Cow::Owned(body),
        summary,
    }
}

/// `/handoff` 那一轮回复末尾写下的交接文件路径(去掉首尾空白与反引号)。交接标签可以在总结标签
/// 前后任一侧;没有、为空或跨行时为 None。
pub fn handoff_path(reply: &str) -> Option<String> {
    let (_, inner) = trailing_tag(reply, HANDOFF_TAG_OPEN, HANDOFF_TAG_CLOSE).or_else(|| {
        let (feed_open, _) = trailing_tag(reply, FEED_TAG_OPEN, FEED_TAG_CLOSE)?;
        trailing_tag(&reply[..feed_open], HANDOFF_TAG_OPEN, HANDOFF_TAG_CLOSE)
    })?;
    let path = inner.trim().trim_matches('`').trim();
    (!path.is_empty() && !path.contains('\n')).then(|| path.to_string())
}

/// 去掉末尾交接标签后的正文(可能为空);末尾没有交接标签时原样返回。
fn without_handoff_tag(text: &str) -> &str {
    match trailing_tag(text, HANDOFF_TAG_OPEN, HANDOFF_TAG_CLOSE) {
        Some((open, _)) => text[..open].trim_end(),
        None => text,
    }
}

/// 回复末尾那个标签:开标签的字节位置与标签内容。规则见 [`split_feed_tag`]。
fn trailing_tag<'a>(reply: &'a str, open_tag: &str, close_tag: &str) -> Option<(usize, &'a str)> {
    let before_close = reply.trim_end().strip_suffix(close_tag)?;
    let open = before_close.rfind(open_tag)?;
    let line_start = before_close[..open].rfind('\n').map_or(0, |i| i + 1);
    let inner = &before_close[open + open_tag.len()..];
    let opens_its_line = before_close[line_start..open].trim().is_empty();
    if (inner.contains('\n') && !opens_its_line) || inside_code_fence(&reply[..line_start]) {
        return None;
    }
    Some((open, inner))
}

/// 只要去掉标签后的正文,给转录、重投、镜像这些不管预览的出口用。
pub fn strip_feed_tag(reply: &str) -> Cow<'_, str> {
    split_feed_tag(reply).body
}

/// 标签前的文本走完后是否还停在代码块里。围栏按种类配对:``` 开的块只认 ``` 收,~~~ 同理,
/// 块里出现另一种围栏只是代码内容。
fn inside_code_fence(text: &str) -> bool {
    let mut open: Option<&str> = None;
    for line in text.lines() {
        let line = line.trim_start();
        for fence in ["```", "~~~"] {
            if line.starts_with(fence) {
                match open {
                    None => open = Some(fence),
                    Some(current) if current == fence => open = None,
                    Some(_) => {}
                }
                break;
            }
        }
    }
    open.is_some()
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 合格的总结:去首尾空白、连续空白并成一个后 1 到 [`FEED_TAG_MAX_CHARS`] 个字符(按写下的字计),
/// 不含换行与尖括号(那是没闭合好的标签或误写的 HTML);再把全角标点换成英文标点。
fn normalize_summary(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(['\n', '<', '>']) {
        return None;
    }
    let collapsed = collapse_whitespace(raw);
    if collapsed.chars().count() > FEED_TAG_MAX_CHARS {
        return None;
    }
    let mut out = String::with_capacity(collapsed.len());
    for ch in collapsed.chars() {
        push_ascii_punctuation(&mut out, ch);
    }
    Some(out)
}

/// 中文与全角标点换成英文标点:全角 ASCII 区(U+FF01–U+FF5E)整段平移到 ASCII,其余逐个列出;
/// 不是标点的字符原样写入。
fn push_ascii_punctuation(out: &mut String, ch: char) {
    let code = ch as u32;
    if (0xFF01..=0xFF5E).contains(&code) {
        if let Some(ascii) = char::from_u32(code - 0xFEE0) {
            out.push(ascii);
            return;
        }
    }
    match ch {
        '。' => out.push('.'),
        '、' => out.push(','),
        '“' | '”' | '「' | '」' | '『' | '』' | '《' | '》' | '〈' | '〉' => {
            out.push('"')
        }
        '‘' | '’' => out.push('\''),
        '【' | '〔' => out.push('['),
        '】' | '〕' => out.push(']'),
        '·' | '・' => out.push('.'),
        '—' => out.push('-'),
        '…' => out.push_str("..."),
        _ => out.push(ch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_tag_becomes_the_summary_and_leaves_the_body() {
        let tagged =
            split_feed_tag("修好了卡片额度显示。\n\n<summon-feed>修好卡片额度显示</summon-feed>\n");
        assert_eq!(tagged.body, "修好了卡片额度显示。");
        assert_eq!(tagged.summary.as_deref(), Some("修好卡片额度显示"));
    }

    #[test]
    fn tag_after_text_on_the_last_line_is_still_taken() {
        let tagged = split_feed_tag("搞定了。<summon-feed>已部署两台</summon-feed>");
        assert_eq!(tagged.body, "搞定了。");
        assert_eq!(tagged.summary.as_deref(), Some("已部署两台"));
    }

    #[test]
    fn punctuation_is_normalized_to_ascii() {
        let tagged = split_feed_tag("正文\n<summon-feed>额度：5h，周（剩余）！</summon-feed>");
        assert_eq!(tagged.summary.as_deref(), Some("额度:5h,周(剩余)!"));
        let tagged = split_feed_tag("正文\n<summon-feed>“好”、行。</summon-feed>");
        assert_eq!(tagged.summary.as_deref(), Some("\"好\",行."));
        let tagged = split_feed_tag("正文\n<summon-feed>修好「退出」《卡》·完成</summon-feed>");
        assert_eq!(tagged.summary.as_deref(), Some("修好\"退出\"\"卡\".完成"));
    }

    /// 字数按 agent 写下的字计:一个省略号换成三个点之后不能因此超限。
    #[test]
    fn the_limit_counts_characters_as_written() {
        let reply = "正文\n<summon-feed>一二三四五六七八九十一二三…</summon-feed>";
        assert_eq!(
            split_feed_tag(reply).summary.as_deref(),
            Some("一二三四五六七八九十一二三...")
        );
    }

    #[test]
    fn a_tag_split_across_lines_is_still_taken() {
        let tagged = split_feed_tag("正文\n<summon-feed>\n修好额度\n</summon-feed>");
        assert_eq!(tagged.body, "正文");
        assert_eq!(tagged.summary.as_deref(), Some("修好额度"));
        let tagged = split_feed_tag("正文\n<summon-feed>第一行\n第二行</summon-feed>");
        assert_eq!(tagged.body, "正文");
        assert_eq!(tagged.summary, None, "内容里带换行不当预览");
    }

    #[test]
    fn invalid_summary_is_dropped_but_the_tag_never_reaches_the_body() {
        for inner in ["", "   ", "一二三四五六七八九十一二三四五", "a<b"] {
            let reply = format!("正文\n<summon-feed>{inner}</summon-feed>");
            let tagged = split_feed_tag(&reply);
            assert_eq!(tagged.body, "正文", "{inner:?}");
            assert_eq!(tagged.summary, None, "{inner:?}");
        }
        let exactly_fourteen = "一二三四五六七八九十一二三四";
        let reply = format!("正文\n<summon-feed>{exactly_fourteen}</summon-feed>");
        assert_eq!(
            split_feed_tag(&reply).summary.as_deref(),
            Some(exactly_fourteen)
        );
    }

    #[test]
    fn tags_that_are_not_at_the_very_end_stay_in_the_body() {
        for reply in [
            "<summon-feed>举例</summon-feed>\n后面还有正文",
            "用 `<summon-feed>举例</summon-feed>`",
            "```\n<summon-feed>举例</summon-feed>",
            "~~~\n<summon-feed>举例</summon-feed>",
            "说明格式是 <summon-feed> 这样写\n第二段正文\n闭标签是 </summon-feed>",
            "没有闭合 <summon-feed>举例",
            "<summon-feed></summon-feed>",
        ] {
            let tagged = split_feed_tag(reply);
            assert_eq!(tagged.body, reply);
            assert_eq!(tagged.summary, None);
        }
        let mixed_fences = "```\n~~~ 代码里的一行\n```\n<summon-feed>写完示例</summon-feed>";
        assert_eq!(
            split_feed_tag(mixed_fences).summary.as_deref(),
            Some("写完示例"),
            "块里的另一种围栏只是代码内容"
        );
        let after_fence = "```\ncode\n```\n<summon-feed>写完示例</summon-feed>";
        let tagged = split_feed_tag(after_fence);
        assert_eq!(tagged.body, "```\ncode\n```");
        assert_eq!(tagged.summary.as_deref(), Some("写完示例"));
    }

    #[test]
    fn a_reply_that_is_only_the_tag_keeps_its_words_as_the_body() {
        let tagged = split_feed_tag("<summon-feed>已完成</summon-feed>");
        assert_eq!(tagged.body, "已完成");
        assert_eq!(tagged.summary.as_deref(), Some("已完成"));
        let long = "这一句总结远远超过了十四个字的上限";
        let reply = format!("<summon-feed>{long}</summon-feed>");
        let tagged = split_feed_tag(&reply);
        assert_eq!(tagged.body, long, "不合格也不发空卡");
        assert_eq!(tagged.summary, None);
    }

    /// 交接标签在总结标签前后都不进正文;路径从任一顺序里都取得出来。
    #[test]
    fn the_handoff_tag_leaves_the_body_in_either_order() {
        let path = "/w/tasks/20261015-x/HANDOFF.md";
        for reply in [
            format!("写好了。\n<summon-handoff>{path}</summon-handoff>"),
            format!("写好了。\n<summon-handoff>{path}</summon-handoff>\n<summon-feed>交接已写好</summon-feed>"),
            format!("写好了。\n<summon-feed>交接已写好</summon-feed>\n<summon-handoff>{path}</summon-handoff>\n"),
            format!("写好了。<summon-handoff> `{path}` </summon-handoff>"),
        ] {
            assert_eq!(split_feed_tag(&reply).body, "写好了。", "{reply:?}");
            assert_eq!(handoff_path(&reply).as_deref(), Some(path), "{reply:?}");
        }
        let both = format!("写好了。\n<summon-handoff>{path}</summon-handoff>\n<summon-feed>交接已写好</summon-feed>");
        assert_eq!(split_feed_tag(&both).summary.as_deref(), Some("交接已写好"));
    }

    #[test]
    fn a_handoff_path_must_be_a_single_line_at_the_very_end() {
        for reply in [
            "没有标签",
            "<summon-handoff></summon-handoff>",
            "<summon-handoff>``</summon-handoff>",
            "正文\n<summon-handoff>\n/a\n/b\n</summon-handoff>",
            "<summon-handoff>/a</summon-handoff>\n后面还有正文",
            "```\n<summon-handoff>/a</summon-handoff>",
            "说 <summon-handoff>/a</summon-handoff> 是格式\n<summon-feed>说明格式</summon-feed>\n结尾",
        ] {
            assert_eq!(handoff_path(reply), None, "{reply:?}");
        }
        let only_tag = "<summon-handoff>/a/HANDOFF.md</summon-handoff>";
        assert_eq!(handoff_path(only_tag).as_deref(), Some("/a/HANDOFF.md"));
        assert_eq!(
            split_feed_tag(only_tag).body,
            only_tag,
            "只有标签时不发空卡"
        );
        let tag_then_feed =
            "<summon-handoff>/a/HANDOFF.md</summon-handoff>\n<summon-feed>交接已写好</summon-feed>";
        assert_eq!(split_feed_tag(tag_then_feed).body, "交接已写好");
    }

    #[test]
    fn strip_only_returns_the_visible_body() {
        assert_eq!(
            strip_feed_tag("正文\n<summon-feed>总结</summon-feed>"),
            "正文"
        );
        assert!(matches!(strip_feed_tag("正文"), Cow::Borrowed("正文")));
    }
}
