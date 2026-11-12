//! cmux 遥控的读屏判定。cmux `read-screen` 只给纯文本(没有样式),判定只能看字形。
//!
//! 与 tmux 注入共用 `pane_input::classify_pane` 的五态,再补一道面板检测:一轮结束后 Claude
//! Code 会在输入框**上方**弹出带编号选项的面板,此时输入框是空的、`classify_pane` 判
//! `InputReady`,但按键会落进面板(打一个数字就选中一项)。

use super::pane_input::{
    classify_pane, is_full_rule, is_menu_option, prompt_rest, ModalKind, PaneState,
};
use super::scrubber::strip_ansi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteRefusal {
    /// 输入框里有字:用户打了一半的草稿,或 Claude 的占位提示(纯文本里两者分不开)。
    Draft,
    /// 编号选项面板或确认门在等终端里的选择。
    Menu,
    /// 全屏面板(`/status` 一类)。
    Panel,
    /// 屏幕读不出可证明安全的状态。
    Unknown,
}

const PANEL_FOOTERS: [&str; 2] = ["enter to confirm", "esc to cancel"];

/// 输入框 = 最后两条满宽框线之间的行,且首行是 `❯` 提示行。
fn input_box_lines<'a>(lines: &[&'a str]) -> Option<(usize, Vec<&'a str>)> {
    let bottom = lines.iter().rposition(|l| is_full_rule(l))?;
    let top = lines[..bottom].iter().rposition(|l| is_full_rule(l))?;
    let inner = &lines[top + 1..bottom];
    let first = inner.first()?;
    prompt_rest(first)?;
    Some((top, inner.to_vec()))
}

pub fn input_box_text(screen: &str) -> Option<String> {
    let plain = strip_ansi(screen);
    let lines: Vec<&str> = plain.lines().collect();
    let (_, inner) = input_box_lines(&lines)?;
    let mut out = Vec::with_capacity(inner.len());
    for (i, line) in inner.iter().enumerate() {
        let text = if i == 0 {
            prompt_rest(line)?.to_string()
        } else {
            line.strip_prefix("  ")
                .unwrap_or(line)
                .trim_end()
                .to_string()
        };
        out.push(text);
    }
    Some(out.join("\n").trim_end().to_string())
}

fn without_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 输入框此刻是否恰好是这条消息。折行在屏宽处直接断开、续行缩进两格,所以按去掉全部
/// 空白之后的字序比。
pub fn input_box_holds(screen: &str, message: &str) -> bool {
    input_box_text(screen)
        .is_some_and(|text| without_whitespace(&text) == without_whitespace(message))
}

/// 面板里的编号选项行(`  ❯ 1. Yes`):光标前有缩进。转录里用户自己消息的回显是顶格的
/// `❯ 1. …`,不能算,否则用户发过一条「1. …」开头的消息后这条会话就永远被拒。
fn is_panel_option_line(line: &str) -> bool {
    line.starts_with(char::is_whitespace) && prompt_rest(line).is_some_and(is_menu_option)
}

fn panel_blocks_input(lines: &[&str], box_top: Option<usize>) -> bool {
    let above = &lines[..box_top.unwrap_or(lines.len())];
    above.iter().any(|line| {
        let low = line.to_lowercase();
        PANEL_FOOTERS.iter().any(|f| low.contains(f)) || is_panel_option_line(line)
    })
}

pub fn remote_gate(screen: &str) -> Result<(), RemoteRefusal> {
    let plain = strip_ansi(screen);
    let lines: Vec<&str> = plain.lines().collect();
    let input = input_box_lines(&lines);
    if panel_blocks_input(&lines, input.as_ref().map(|(top, _)| *top)) {
        return Err(RemoteRefusal::Menu);
    }
    match classify_pane(screen) {
        PaneState::InputReady | PaneState::Running => {}
        PaneState::Draft => return Err(RemoteRefusal::Draft),
        PaneState::Modal(ModalKind::Menu) => return Err(RemoteRefusal::Menu),
        PaneState::Modal(ModalKind::Panel) => return Err(RemoteRefusal::Panel),
        PaneState::Unknown => return Err(RemoteRefusal::Unknown),
    }
    // 打字前输入框必须证得是空的:框线认不出时字会落到哪里不可知。
    match input_box_text(screen).as_deref() {
        Some("") => Ok(()),
        _ => Err(RemoteRefusal::Unknown),
    }
}

pub fn refusal_notice(refusal: RemoteRefusal) -> &'static str {
    match refusal {
        RemoteRefusal::Draft => {
            summon_i18n::text!("r3cc53facf0")
        }
        RemoteRefusal::Menu => {
            summon_i18n::text!("r0afa2cc772")
        }
        RemoteRefusal::Panel => summon_i18n::text!("rb1b191e058"),
        RemoteRefusal::Unknown => summon_i18n::text!("r467420c6ee"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/parse/testdata/remote")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"))
    }

    #[test]
    fn running_and_empty_input_are_accepted() {
        assert_eq!(remote_gate(&sample("screen-04-running.txt")), Ok(()));
    }

    #[test]
    fn after_turn_panel_blocks_input() {
        // 输入框是空的,但上方的编号选项面板会吃掉按键。
        assert_eq!(
            remote_gate(&sample("screen-05-after-turn.txt")),
            Err(RemoteRefusal::Menu)
        );
    }

    #[test]
    fn trust_gate_is_a_menu() {
        assert_eq!(
            remote_gate(&sample("screen-01-trust-menu.txt")),
            Err(RemoteRefusal::Menu)
        );
    }

    #[test]
    fn placeholder_and_typed_text_are_drafts() {
        assert_eq!(
            remote_gate(&sample("screen-02-idle.txt")),
            Err(RemoteRefusal::Draft)
        );
        assert_eq!(
            remote_gate(&sample("screen-03-typed.txt")),
            Err(RemoteRefusal::Draft)
        );
    }

    #[test]
    fn empty_screen_is_unknown() {
        assert_eq!(remote_gate(""), Err(RemoteRefusal::Unknown));
        assert_eq!(
            remote_gate("random text\nno box"),
            Err(RemoteRefusal::Unknown)
        );
    }

    #[test]
    fn input_box_text_joins_wrapped_and_newline_lines() {
        assert_eq!(
            input_box_text(&sample("screen-06-shift-enter.txt")).as_deref(),
            Some("第一行\n第二行")
        );
        assert_eq!(
            input_box_text(&sample("screen-04-running.txt")).as_deref(),
            Some("")
        );
    }

    #[test]
    fn input_box_holds_ignores_whitespace_and_wrapping() {
        let long = format!("测试折行{}END", "一二三四五六七八九十".repeat(20));
        let typed = sample("screen-07-long.txt");
        // 屏上是「第一行/第二行」草稿后接长文本:只有完整草稿 + 长文本才相等。
        assert!(!input_box_holds(&typed, &long));
        assert!(input_box_holds(&typed, &format!("第一行\n第二行{long}")));
        assert!(input_box_holds(
            &sample("screen-06-shift-enter.txt"),
            "第一行\n 第二行"
        ));
        assert!(!input_box_holds(
            &sample("screen-06-shift-enter.txt"),
            "第一行"
        ));
    }

    #[test]
    fn every_refusal_has_a_notice() {
        for r in [
            RemoteRefusal::Draft,
            RemoteRefusal::Menu,
            RemoteRefusal::Panel,
            RemoteRefusal::Unknown,
        ] {
            assert!(refusal_notice(r).contains("没有送进去"));
        }
    }

    #[test]
    fn transcript_echo_of_numbered_prompt_is_not_a_panel() {
        // 用户上一条消息本身以「1. 」开头时,转录回显顶格渲染成 `❯ 1. …`;真面板的选项行
        // 带缩进。顶格回显不能被当成面板,否则这条会话从此被误拒。
        let running = sample("screen-04-running.txt");
        let echoed = running.replace("❯ 只回复两个字：好的", "❯ 1. 先做这个");
        assert_ne!(echoed, running, "替换没生效,样本变了");
        assert_eq!(remote_gate(&echoed), Ok(()));
        // 去掉 spinner 行,走「空输入框就绪」那条分支。
        let idle = echoed
            .lines()
            .filter(|l| !l.contains("Sprouting"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(remote_gate(&idle), Ok(()));
    }

    #[test]
    fn running_with_a_numbered_draft_in_the_box_is_refused() {
        // 运行中、输入框里是 `1. 先做这个` 一类草稿:classify_pane 把编号行当菜单光标、判
        // Running,框上方也没有面板。挡住它的只有「打字前输入框必须为空」这一道。
        let running = sample("screen-04-running.txt");
        // 真机输入框的提示符后跟的是不换行空格。
        let drafted = running.replacen("\n❯\u{a0}\n", "\n❯\u{a0}1. 先做这个\n", 1);
        assert_ne!(drafted, running, "替换没生效,样本变了");
        assert_eq!(classify_pane(&drafted), PaneState::Running);
        assert_eq!(input_box_text(&drafted).as_deref(), Some("1. 先做这个"));
        assert_eq!(remote_gate(&drafted), Err(RemoteRefusal::Unknown));
    }

    fn without_lines_containing(screen: &str, needle: &str) -> String {
        screen
            .lines()
            .filter(|l| !l.to_lowercase().contains(needle))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn indented_numbered_option_alone_blocks_input() {
        // 没有页脚时,面板只剩缩进的 `❯ 1. Yes` 能证明它在吃按键;此时输入框本身是空的,
        // classify_pane 看不出异样,拒绝只能来自选项行判定。
        let screen =
            without_lines_containing(&sample("screen-05-after-turn.txt"), "enter to confirm");
        assert!(
            !screen.to_lowercase().contains("esc to cancel"),
            "页脚没删干净"
        );
        assert_eq!(classify_pane(&screen), PaneState::InputReady);
        assert_eq!(remote_gate(&screen), Err(RemoteRefusal::Menu));
    }

    #[test]
    fn empty_prompt_without_a_provable_box_is_unknown() {
        // classify_pane 认得出空的 `❯` 行(就绪 / 运行中),但框线配不成对:字会落到哪里不可知,
        // 必须拒绝而不是放行。
        let running = sample("screen-04-running.txt");
        let is_rule = |l: &&str| l.trim().starts_with('─');
        // 一条框线都没有。
        let no_rules = running
            .lines()
            .filter(|l| !is_rule(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(classify_pane(&no_rules), PaneState::Running);
        assert_eq!(remote_gate(&no_rules), Err(RemoteRefusal::Unknown));
        // 只剩下框线,上框线缺失。
        let mut seen = 0;
        let lower_only = running
            .lines()
            .filter(|l| {
                let rule = is_rule(l);
                seen += usize::from(rule);
                !(rule && seen == 1)
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(classify_pane(&lower_only), PaneState::Running);
        assert_eq!(remote_gate(&lower_only), Err(RemoteRefusal::Unknown));
    }
}
