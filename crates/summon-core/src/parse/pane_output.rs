//! 从抓屏里取**这条命令产生的可见输出**。纯文本、零 IO。
//!
//! 为什么要有这一层(设计见 `docs/spec/2026-08-14-transcript-slash-passthrough-design.md` 第四节):
//! 透传若只有副作用没有回显,用户还得 attach 一次,透传就没意义。命令输出不经 transcript
//! (slash 一律不附 marker、也不产 assistant 轮),抓屏是唯一来源。
//!
//! 两种形态各有自己的锚:
//! - **print 类**(`/context`):输出落进转录区,输入框还在底部 → 用「上一屏的结尾」当锚,取其后。
//!   真机注意:`/context` **不留命令回显**(不像 `❯ /cd …` 那样),所以锚只能取上一屏、不能取回显。
//! - **panel 类**(`/status` `/cost`):面板**底部锚定**覆盖屏幕下部(2026-08-14 全高抓屏实证,
//!   横杠上方还留着陈旧转录),顶部有一条满宽 `▔` 横杠 → 取其后,上方残留自动被切掉。
//!   真机验证过面板退出后 scrollback 零残留,所以「屏上有横杠+tab 栏」就等于「此刻是面板」。

use crate::scrubber::strip_ansi;

/// 取「上一屏结尾」当锚时用几行。1 行太容易在长输出里撞上重复行;3 行连续相同已足够唯一。
const ANCHOR_LINES: usize = 3;

/// 面板顶部那条满宽横杠(U+2594)至少这么长才算数,避免正文里的短装饰线误锚。
const PANEL_RULE_MIN: usize = 10;

/// 面板 tab 栏与页脚是 chrome,不进回传正文(与 `pane_input` 的特征库同源,改一处要一起改)。
const PANEL_TABS: &[&str] = &["Settings", "Status", "Config", "Usage", "Stats"];
const PANEL_TABS_MIN_HITS: usize = 3;
const PANEL_FOOTER_HINTS: &[&str] = &[
    "esc to cancel",
    "esc to clear",
    "esc to dismiss",
    "to select",
];

/// pane 的**转录区**:输入框(最后一个 `❯` 行)及其下方的框线/状态行全部切掉。
/// 底部那几行每一拍都在变(计时、用量、token 数),留着会让任何「和上一屏比」的判据永远不相等。
pub fn transcript_region(pane: &str) -> Vec<String> {
    let plain = strip_ansi(pane);
    let lines: Vec<&str> = plain.lines().collect();
    let cut = lines
        .iter()
        .rposition(|l| is_prompt_line(l))
        .unwrap_or(lines.len());
    let mut region: Vec<String> = lines[..cut]
        .iter()
        .map(|l| l.trim_end().to_string())
        .collect();
    // 输入框上框线属于 chrome,一并去掉。
    while region.last().is_some_and(|l| is_chrome_tail(l)) {
        region.pop();
    }
    region
}

/// 这一屏相对上一屏新增的可见输出,**外带一条「这到底是不是本条命令的输出」的凭据**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOutput {
    pub text: String,
    /// 锚是否在新一屏里找到。
    ///
    /// `false` 时 [`text`](Self::text) 是**整块可见转录区**,不是「这条命令打印的东西」——
    /// 里面可能有上一轮对话正文、工具输出、文件片段。调用方**必须**据此改口径:
    /// 既不能对用户宣称「这就是 `/context` 的输出」(谎报),也不能把它当命令回显发进群
    /// (owner 2026-08-13 裁决 2 要挡的泄漏,只是漏在 print 桶而不是 panel 桶)。
    pub anchored: bool,
}

/// 这一屏相对上一屏**新增**的可见输出。
///
/// 锚 = 上一屏转录区末尾 [`ANCHOR_LINES`] 行(非空)。在新一屏里找到它们最后一次连续出现的位置,
/// 取其后即新增部分。**找不到锚仍返回整块转录区** —— 屏幕滚动过多、或命令把整屏重画了(真机
/// `/context` 就是这样:欢迎屏被整个顶掉),这时宁可多回传一些,也不要因为「对不齐」就把用户
/// 要的答案吞掉。但**必须把「对不齐」这件事一并交出去**,由调用方降级口径与投递范围。
pub fn new_output_since(before: &str, after: &str) -> NewOutput {
    let prev = transcript_region(before);
    let now = transcript_region(after);
    let anchor: Vec<&String> = prev
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(ANCHOR_LINES)
        .collect();
    let anchor: Vec<&str> = anchor.into_iter().rev().map(|s| s.as_str()).collect();
    match find_last_run(&now, &anchor) {
        Some(end) => NewOutput {
            text: join_trimmed(&now[end..]),
            anchored: true,
        },
        None => NewOutput {
            text: join_trimmed(&now[..]),
            anchored: false,
        },
    }
}

/// 面板正文:顶部满宽横杠之后、去掉 tab 栏与页脚提示的部分。横杠找不到就退回整屏
/// (仍是真实屏幕内容,只是可能带一点残留转录 —— 比返回空强)。
pub fn panel_body(pane: &str) -> String {
    let plain = strip_ansi(pane);
    let lines: Vec<&str> = plain.lines().collect();
    let start = lines
        .iter()
        .rposition(|l| is_panel_rule(l))
        .map_or(0, |i| i + 1);
    let body: Vec<String> = lines[start..]
        .iter()
        .filter(|l| !is_tab_bar(l) && !is_panel_footer_hint(l))
        .map(|l| l.trim_end().to_string())
        .collect();
    join_trimmed(&body)
}

/// 超长回传截断。**必须报数** —— 悄悄截断等于谎报「这就是全部输出」。
pub fn clip_with_notice(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    let total = text.chars().count();
    summon_i18n::format!("rd5cb21cb41")
}

fn join_trimmed(lines: &[String]) -> String {
    let mut out: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    while out.first().is_some_and(|l| l.trim().is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// `anchor` 在 `hay` 里最后一次连续出现的**结束位置**(不含)。anchor 为空返回 None。
fn find_last_run(hay: &[String], anchor: &[&str]) -> Option<usize> {
    if anchor.is_empty() || hay.len() < anchor.len() {
        return None;
    }
    (0..=hay.len() - anchor.len())
        .rev()
        .find(|&i| {
            hay[i..i + anchor.len()]
                .iter()
                .zip(anchor)
                .all(|(a, b)| a == b)
        })
        .map(|i| i + anchor.len())
}

fn is_prompt_line(line: &str) -> bool {
    line.trim_end()
        .trim_start_matches(|c: char| c.is_whitespace() || is_box_drawing(c))
        .starts_with('❯')
}

fn is_box_drawing(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// 输入框上框线 / 空行这类尾部 chrome。
fn is_chrome_tail(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || (!t.is_empty() && t.chars().all(is_box_drawing))
}

/// 面板顶栏那条满宽 `▔` 横杠。`pane_input` 的面板特征库拿它当合取项(tab 栏不设窗口的代价补偿),
/// 所以判据只能有这一份实现 —— 两处各写一遍迟早会漂。
pub fn is_panel_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= PANEL_RULE_MIN && t.chars().all(|c| c == '\u{2594}')
}

fn is_tab_bar(line: &str) -> bool {
    line.split_whitespace()
        .filter(|w| PANEL_TABS.contains(w))
        .count()
        >= PANEL_TABS_MIN_HITS
}

fn is_panel_footer_hint(line: &str) -> bool {
    let low = line.to_lowercase();
    PANEL_FOOTER_HINTS.iter().any(|h| low.contains(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机(2026-08-14,claude 2.1.231):`/context` 之前的欢迎屏 + 空输入框。
    const BEFORE_CONTEXT: &str = "╭─── Claude Code v2.1.231 ────────────╮\n\
│                 Welcome back Alice!                │\n\
╰────────────────────────────────────╯\n\n\n\
────────────────────────────────────\n\
❯ \n\
────────────────────────────────────\n   \
󱙺 Opus 5 (xhigh)      󰝰 scratchpad/probe\n  \
⏵⏵ auto mode on (shift+tab to cycle) · ← for agents\n";

    /// 真机(同次):`/context` 跑完,输出留在转录区,输入框回到空 —— 欢迎屏已被顶掉。
    /// 注意**没有 `❯ /context` 回显**:print 类的锚不能取命令回显。
    const AFTER_CONTEXT: &str = "     ⛶ ⛶ ⛶ ⛶ ⛶   claude-opus-5\n\
     ⛶ ⛶ ⛶ ⛶ ⛶   29.1k/1m tokens (3%)\n\n\
     Memory files · /memory\n     └ 1 file · 2.2k tokens\n\n\
     /context all to expand\n\
────────────────────────────────────\n\
❯ \n\
────────────────────────────────────\n   \
󱙺 Opus 5 (xhigh)      󰝰 scratchpad/probe\n  \
⏵⏵ auto mode on (shift+tab to cycle) · ← for agents\n";

    /// 真机(同日,`/status`):顶部满宽 ▔ 横杠 + tab 栏,底部页脚。
    const STATUS_PANEL: &str = "│      someone1@example.com's Organization           │\n\
▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n   \
Settings  Status   Config   Usage   Stats\n\n   \
Version:          2.1.231\n   Email:            someone1@example.com\n\n   \
Esc to cancel\n";

    /// 新增输出必须是**这次**跑出来的那块,不能把上一屏的内容也算进去。
    #[test]
    fn print_output_is_what_appeared_after_the_previous_screen() {
        let out = new_output_since(BEFORE_CONTEXT, AFTER_CONTEXT).text;
        assert!(out.contains("29.1k/1m tokens"), "拿到的不是本次输出: {out}");
        assert!(out.contains("/context all to expand"));
        assert!(
            !out.contains("Welcome back Alice"),
            "上一屏的内容不该混进来"
        );
        assert!(!out.contains('❯'), "输入框与其下的状态行不进回传: {out}");
        assert!(
            !out.contains("auto mode on"),
            "底部状态行每拍都在变,必须切掉"
        );
    }

    /// 锚失配这件事**必须外显**:仓内唯一那份 `/context` 真机样本走的就是兜底路径(欢迎屏被整个
    /// 顶掉,锚必然找不到),兜底返回的是**整块转录区** —— 上一轮对话正文、工具输出、文件片段都在
    /// 里面。把它当「命令输出」贴进群 = 一次公开泄漏 + 一次谎报。判据不能只在注释里,得能被读出来。
    #[test]
    fn a_lost_anchor_is_reported_so_the_caller_can_downgrade() {
        let real_context = new_output_since(BEFORE_CONTEXT, AFTER_CONTEXT);
        assert!(
            !real_context.anchored,
            "真机 /context 样本本来就走兜底,这条测的就是它"
        );
        let scrolled = new_output_since(
            "上一轮对话正文:密钥是 hunter2\n────\n❯ \n────\n",
            "别的内容\n────\n❯ \n────\n",
        );
        assert!(!scrolled.anchored);
        // 锚对上的正常情形照旧标 true。
        let before = "第一行\n第二行\n第三行\n────\n❯ \n────\n";
        let after = "第一行\n第二行\n第三行\n新输出\n────\n❯ \n────\n";
        let ok = new_output_since(before, after);
        assert!(ok.anchored);
        assert_eq!(ok.text, "新输出");
    }

    /// 上一屏内容仍留在屏上时,只取锚之后的新增部分。
    #[test]
    fn unchanged_head_of_the_screen_is_not_reported_as_new() {
        let before = "上一轮的回答第一行\n上一轮的回答第二行\n上一轮的回答第三行\n\
────\n❯ \n────\n status\n";
        let after = "上一轮的回答第一行\n上一轮的回答第二行\n上一轮的回答第三行\n\
新命令的输出 A\n新命令的输出 B\n────\n❯ \n────\n status\n";
        assert_eq!(
            new_output_since(before, after).text,
            "新命令的输出 A\n新命令的输出 B"
        );
    }

    /// 对不齐(滚动过多 / 整屏重画)时宁可多回传,也不能吞掉用户要的答案。
    #[test]
    fn losing_the_anchor_returns_the_whole_screen_rather_than_nothing() {
        let before = "完全不同的老内容\n────\n❯ \n────\n";
        let after = "只剩新内容 A\n只剩新内容 B\n────\n❯ \n────\n";
        assert_eq!(
            new_output_since(before, after).text,
            "只剩新内容 A\n只剩新内容 B"
        );
    }

    #[test]
    fn panel_body_starts_below_the_rule_and_drops_chrome() {
        let body = panel_body(STATUS_PANEL);
        assert!(
            body.starts_with("   Version:"),
            "面板正文应从横杠之后开始: {body}"
        );
        assert!(body.contains("someone1@example.com"), "正文本身要留下");
        assert!(
            !body.contains("Organization           │"),
            "横杠以上的残留转录要切掉"
        );
        assert!(!body.contains("Settings  Status"), "tab 栏是 chrome");
        assert!(
            !body.to_lowercase().contains("esc to cancel"),
            "页脚是 chrome"
        );
    }

    /// 截断必须报数:悄悄截断等于谎报「这就是全部输出」。
    #[test]
    fn clipping_always_reports_how_much_was_cut() {
        let long = "字".repeat(100);
        let clipped = clip_with_notice(&long, 10);
        assert!(clipped.contains("共 100 字"), "{clipped}");
        assert!(clipped.contains("已截断"));
        assert_eq!(clip_with_notice("短", 10), "短", "不超长就原样返回");
    }
}
