//! 主轮 result 的控制协议；比较输入必须是渲染前的完整原文。

pub const SILENT_RESET_KEYWORD: &str = "__SUMMON_SILENT_EXIT__";

pub fn is_silent_reset_result(result: &str) -> bool {
    result == SILENT_RESET_KEYWORD
}

/// 展示段的去重可合并重复 token，控制词候选必须在该投影之前按原始流跟踪。
#[derive(Default)]
pub struct ReplyPrefix {
    prefix: String,
    ordinary: bool,
}

impl ReplyPrefix {
    pub fn observe(&mut self, text: &str, partial: bool) -> bool {
        if !partial {
            self.clear();
        } else if self.ordinary {
            return false;
        }
        let appended = format!("{}{text}", self.prefix);
        if !appended.is_empty() && SILENT_RESET_KEYWORD.starts_with(&appended) {
            self.prefix = appended;
            true
        } else if !text.is_empty()
            && text.starts_with(&self.prefix)
            && SILENT_RESET_KEYWORD.starts_with(text)
        {
            // 同一段可同时收到 delta 和完整快照，候选的长度始终受保留词长度约束。
            self.prefix = text.to_string();
            true
        } else {
            self.ordinary = true;
            self.prefix.clear();
            false
        }
    }

    pub fn clear(&mut self) {
        self.prefix.clear();
        self.ordinary = false;
    }
}

/// 最后一个回复段仍可能成为控制结果时先留在内存，其他过程段按原序展示。
pub fn visible_progress(
    segments: &[super::segment::TurnSegment],
) -> &[super::segment::TurnSegment] {
    match segments.last() {
        Some(super::segment::TurnSegment::Reply(text))
            if !text.is_empty() && SILENT_RESET_KEYWORD.starts_with(text.as_str()) =>
        {
            &segments[..segments.len() - 1]
        }
        _ => segments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_reset_prefix_tracks_repeated_delta_characters_and_snapshots() {
        let mut prefix = ReplyPrefix::default();
        for ch in SILENT_RESET_KEYWORD.chars() {
            assert!(prefix.observe(&ch.to_string(), true));
        }
        assert!(prefix.observe(SILENT_RESET_KEYWORD, false));
        assert!(!prefix.observe("\n", true));
        prefix.clear();
        assert!(prefix.observe("__SUM", true));
        assert!(prefix.observe("__SUMMON", true));
        assert!(!prefix.observe("普通回复", true));
        assert!(!prefix.observe("_", true));
    }

    #[test]
    fn progress_holds_only_a_trailing_candidate_reply() {
        use super::super::segment::TurnSegment::{Reply, Thinking};
        for end in 1..=SILENT_RESET_KEYWORD.len() {
            let segments = vec![
                Thinking("检查中".into()),
                Reply(SILENT_RESET_KEYWORD[..end].into()),
            ];
            assert_eq!(visible_progress(&segments), &segments[..1]);
        }
        for segments in [
            vec![Thinking(SILENT_RESET_KEYWORD.into())],
            vec![Reply(SILENT_RESET_KEYWORD.into()), Thinking("继续".into())],
            vec![Reply(format!("{SILENT_RESET_KEYWORD}\n"))],
            vec![Reply("__普通说明".into())],
        ] {
            assert_eq!(visible_progress(&segments), segments);
        }
    }

    #[test]
    fn silent_reset_requires_the_complete_exact_result() {
        assert!(is_silent_reset_result(SILENT_RESET_KEYWORD));
        for body in [
            "",
            "__SUMMON_SILENT_EXIT__\n",
            " __SUMMON_SILENT_EXIT__",
            "__SUMMON_SILENT_EXIT__ ",
            "__summon_silent_exit__",
            "`__SUMMON_SILENT_EXIT__`",
            "```\n__SUMMON_SILENT_EXIT__\n```",
            "说明：__SUMMON_SILENT_EXIT__",
            "__SUMMON_SILENT_EXIT__。",
            "__SUMMON_SILENT_EXIT__\n__SUMMON_SILENT_EXIT__",
        ] {
            assert!(!is_silent_reset_result(body), "{body:?}");
        }
    }
}
