/// 去掉 ANSI/VT 转义序列(用成熟 crate,避免手写 UTF-8 游走)。
pub fn strip_ansi(input: &str) -> String {
    String::from_utf8_lossy(&strip_ansi_escapes::strip(input)).into_owned()
}

/// 已知的 spinner / 状态字形(Claude Code TUI 用于运行动画与完成标记)。
///
/// 这张表是**枚举**,所以每漏一个字形就等于按帧概率把「在跑」看成「空闲」——
/// 真机 2026-09-09 抓 `/compact` 的 184 帧:`✶`(U+2736)占 15.6%,当时不在表里,于是每六帧
/// 就有一帧让 `is_ready` 谎报就绪。枚举补得再全也只是把概率压小,**兜底是
/// [`is_progress_bar_line`]** —— 它不看字形。
const SPINNER_GLYPHS: &[char] = &[
    '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '·', '✢', '✳', '∗', '✻', '✽', '✶',
];

/// Claude Code 把助手回复行以此字形起头。
const ASSISTANT_MARKER: char = '⏺';

fn starts_with_spinner(trimmed: &str) -> bool {
    trimmed
        .chars()
        .next()
        .map(|c| SPINNER_GLYPHS.contains(&c))
        .unwrap_or(false)
}

/// 一行是否"只有框线 / 空白"(box-drawing、空格)。
fn is_box_or_blank(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true;
    }
    t.chars()
        .all(|c| c == ' ' || ('\u{2500}'..='\u{257F}').contains(&c))
}

/// 非空、且整行只有框线/空格(如满宽 ──── 分隔线)。用于 extract 的停止判定(不含空行)。
fn is_rule_line(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c == ' ' || ('\u{2500}'..='\u{257F}').contains(&c))
}

/// 一行是否"运行中"指示:trim 后以 spinner 字形起头,且以 … 结尾(如 `✽ Composing…`)。
/// 完成标记 `✻ Crunched for 22s` 不以 … 结尾,故不算运行中。
fn is_working_line(line: &str) -> bool {
    let t = line.trim();
    // `contains` 而非 `ends_with`:Claude Code 现在把计时/用量跟在省略号后面 ——
    // 真机形态 `✢ Prestidigitating… (11m 6s · ↓ 34.5k tokens · thought for 5s)`。
    // 用 `ends_with` 的旧判据对它恒为 false,于是 `is_ready` 在 claude **正在跑**时也返回 true,
    // 「无 spinner」这条就绪信号整个变成死代码(2026-08-12 抓真实 pane 实证)。
    // 完成标记行(`✻ Worked for 7m 25s`)不含省略号,不会被这条误吞。
    (starts_with_spinner(t) && t.contains('…')) || is_progress_bar_line(t)
}

/// 一行是否 Claude Code 的**进度条**(`▰▰▰▱▱▱ 39%`)。
///
/// `/compact` 用进度条而不是 spinner 表达进度,而进度条**整段压缩期间一直在**:真机
/// 2026-09-09 的 184 帧里 179 帧有它,同期 spinner 行的字形逐帧轮换、漏一个字形就有 15.6%
/// 的帧被判成空闲。所以它是比逐帧字形更稳的「在跑」信号,也是 [`SPINNER_GLYPHS`] 那张枚举表
/// 将来再漏字形时的兜底。
///
/// 阈值 8 是为了不误吞正文:真机是 40 格,而散落在回复里的一两个方块不该让整轮判成运行中。
fn is_progress_bar_line(line: &str) -> bool {
    line.chars().filter(|c| matches!(c, '▰' | '▱')).count() >= 8
}

fn is_spinner_line(line: &str) -> bool {
    starts_with_spinner(line.trim_start())
}

/// 综合清洗:去 ANSI,逐行丢弃框线/空白/spinner 行,折叠多余空行。
/// (通用清洗器;答案提取用 `extract_answer`。)
pub fn scrub_chrome(raw: &str) -> String {
    let plain = strip_ansi(raw);
    let mut out: Vec<String> = Vec::new();
    let mut prev_blank = false;
    for line in plain.lines() {
        if is_box_or_blank(line) || is_spinner_line(line) {
            if !prev_blank {
                out.push(String::new());
                prev_blank = true;
            }
            continue;
        }
        out.push(line.trim_end().to_string());
        prev_blank = false;
    }
    out.join("\n").trim().to_string()
}

/// 抓屏结果是否证明「tmux 会话已死」。**两条都要成立**,判错最贵(把还在跑的一轮判死),故抽成
/// 纯函数单测。
///
/// `session_present` 是唯一可信的信号 —— `Tmux::capture` 不检查 tmux 退出码,对不存在的会话返回
/// `Ok("")` 而非 `Err`(真机实测 `capture-pane` exit=1/stdout 空,而 `Command::output()` 本身成功),
/// 所以「抓屏失败」这个判据根本不存在。空 pane 只是前置筛:活着的 CLI pane 永远在画东西。
///
/// 取**闭包**而非 bool:短路由本函数保证,调用方不必先付 `has-session` 的 fork-exec。启动期等待是
/// 200ms 一轮的热循环,无条件探测会把 fork-exec 放大两个数量级。
pub fn pane_proves_session_gone(pane: &str, session_present: impl FnOnce() -> bool) -> bool {
    pane.trim().is_empty() && !session_present()
}

/// 就绪态:pane 中不存在"运行中"行(`… 结尾的 spinner 行`)。
pub fn is_ready(pane: &str) -> bool {
    let plain = strip_ansi(pane);
    !plain.lines().any(is_working_line)
}

/// [`is_ready`] 的按行版:调用方已经切好「该看的那部分屏」再来问 —— `pane_input` 只看面板
/// 地平线以下(面板底部锚定,上方陈旧转录里的历史 spinner 不该把「此刻」判成运行中,否则
/// `PromptQueue` 会放行、把 prompt 注进面板)。行须已去过 ANSI。
pub fn is_ready_lines(lines: &[&str]) -> bool {
    !lines.iter().any(|l| is_working_line(l))
}

/// 整轮完成标记行:Claude Code 在一轮结束打印 `✻ Crunched for 22s` / `✽ Brewed for 54s`
/// (spinner 字形 + " for " + 以 Ns 结尾)。多步 turn(如 /summon bootstrap)中途不会出现,
/// 只在整轮真正结束时出现 —— 是比"无 spinner"更可靠的完成信号。
fn is_done_marker_line(line: &str) -> bool {
    let t = line.trim();
    starts_with_spinner(t) && t.contains(" for ") && t.ends_with('s')
}

/// pane 中完成标记的数量。scrollback 里会累积历史 turn 的标记,故用"计数增加"判定新一轮结束。
pub fn count_done_markers(pane: &str) -> usize {
    strip_ansi(pane)
        .lines()
        .filter(|l| is_done_marker_line(l))
        .count()
}

/// 从 pane 抽取最后一段助手回复:锚定最后一个 `⏺` 行,
/// 收集其后内容,遇到 spinner 行(运行中/完成标记)、满宽分隔线、或输入提示 `❯` 即停。
pub fn extract_answer(pane: &str) -> String {
    let plain = strip_ansi(pane);
    let lines: Vec<&str> = plain.lines().collect();
    let Some(start) = lines
        .iter()
        .rposition(|l| l.trim_start().starts_with(ASSISTANT_MARKER))
    else {
        return String::new();
    };
    let mut answer: Vec<String> = Vec::new();
    for (i, raw) in lines.iter().enumerate().skip(start) {
        let t = raw.trim_end();
        let tt = t.trim_start();
        if i == start {
            let body = tt.strip_prefix(ASSISTANT_MARKER).unwrap_or(tt).trim_start();
            answer.push(body.to_string());
            continue;
        }
        if starts_with_spinner(tt) || is_rule_line(t) || tt.starts_with('❯') {
            break;
        }
        answer.push(t.to_string());
    }
    answer.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机 2026-09-09 抓的 `/compact` 三帧(200×50 pane,只留判据看的那几行)。
    ///
    /// 钉的是 owner 报的「compact 秒返回」:压缩期间 claude 用**进度条**表达进度,spinner 行的
    /// 字形逐帧轮换,而输入行**始终是空的**。184 帧里 `✶` 占 15.6%,当时不在 `SPINNER_GLYPHS`
    /// 里 —— 抓到那一帧就判就绪,`is_command_done` 于是把还在跑的压缩报成完成。
    ///
    /// 变异自验:把 `✶` 从 `SPINNER_GLYPHS` 里删掉,第一条断言仍绿(进度条兜住);再把
    /// `is_working_line` 里的 `|| is_progress_bar_line(t)` 去掉,它必红。
    #[test]
    fn real_compact_frames_stay_busy_until_the_progress_bar_is_gone() {
        // 压缩中,spinner 恰好是漏网的 `✶`。
        let star = "❯ /compact\n✶ Compacting conversation…\n  ▰▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱ 2%\n❯ \n";
        assert!(
            !is_ready(star),
            "压缩进行中不得判就绪(spinner 字形漏网时靠进度条兜住)"
        );
        // 压缩中,spinner 是表里已有的字形。
        let known = "❯ /compact\n✽ Compacting conversation… (43s)\n  ▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱ 39%\n❯ \n";
        assert!(!is_ready(known), "压缩进行中不得判就绪");
        // 压缩完成:进度条与 spinner 一起消失,只剩收尾行与空输入行。
        let done = "❯ /compact\n  ⎿  Compacted (ctrl+o to see full summary)\n❯ \n";
        assert!(
            is_ready(done),
            "压缩完成后必须判就绪,否则等到 cap 把成功报成超时"
        );
    }

    /// 进度条判据的边界:整条进度条算「在跑」,回复正文里散落的一两个方块不算。
    #[test]
    fn progress_bar_needs_a_real_bar_not_a_stray_glyph() {
        assert!(is_progress_bar_line("  ▰▰▰▰▱▱▱▱ 12%"));
        assert!(!is_progress_bar_line("对比 ▰ 与 ▱ 两个字符的渲染"));
        assert!(!is_progress_bar_line(""));
    }

    #[test]
    fn strips_ansi_color() {
        let s = "\u{1b}[31mhello\u{1b}[0m world";
        assert_eq!(strip_ansi(s), "hello world");
    }

    /// 实测事故一(`/quit`):群里 `@bot /quit` 被斜杠透传进 tmux,claude 自己退出、会话消亡,本轮
    /// 一直空转到 240s stall 才收尾,期间该 chat 新消息全部排队。
    /// 实测事故二(2026-08-05 flare-5):CLI 起来即退,启动期等待对着尸体空转 62s 才报「就绪超时」。
    /// 两者判据同源,都必须是「空 pane + 复验不在」——「抓屏 Err」这个判据根本不会触发。
    #[test]
    fn session_gone_needs_both_empty_pane_and_absent_session() {
        assert!(
            pane_proves_session_gone("", || false),
            "空 pane + 会话不在 = 已死"
        );
        assert!(
            pane_proves_session_gone("  \n\t ", || false),
            "全空白也算空 pane"
        );
        assert!(
            !pane_proves_session_gone("", || true),
            "会话还在就绝不能判死:has_session 是唯一可信信号"
        );
        assert!(
            !pane_proves_session_gone("✽ Composing…", || false),
            "pane 有内容说明抓到了真屏,不走这条路径"
        );
        assert!(!pane_proves_session_gone("❯ ", || true));
    }

    /// pane 非空时必须**短路**掉 `session_present`:启动期 200ms 一轮,无条件探测会把
    /// `has-session` 的 fork-exec 放大两个数量级。这条钉住惰性求值,防止有人改回 bool 参数。
    #[test]
    fn non_empty_pane_short_circuits_the_session_probe() {
        let mut probed = false;
        let gone = pane_proves_session_gone("❯ ", || {
            probed = true;
            false
        });
        assert!(!gone);
        assert!(!probed, "pane 非空时不该去探会话存活");
    }

    #[test]
    fn drops_box_and_blank_lines() {
        let raw = "╭──────────╮\n│ ignored  │\nreal content\n╰──────────╯\n";
        let out = scrub_chrome(raw);
        assert!(out.contains("real content"));
        assert!(!out.contains('╭'));
        assert!(!out.contains('╰'));
    }

    #[test]
    fn drops_spinner_line() {
        let raw = "⠋ Thinking…\nactual answer\n";
        let out = scrub_chrome(raw);
        assert!(out.contains("actual answer"));
        assert!(!out.contains("Thinking"));
    }

    #[test]
    fn ready_rule_uses_working_spinner() {
        assert!(is_ready("⏺ done\n❯ "));
        assert!(!is_ready("✽ Composing…")); // 运行中
        assert!(is_ready("✻ Crunched for 22s")); // 完成标记,非 …,视作就绪
    }

    /// 2026-08-12 真机 pane:运行中的 spinner 行在省略号后还挂着计时/用量,`ends_with('…')`
    /// 的旧判据对它恒 false —— claude 明明在跑,`is_ready` 却说就绪。这条钉死新形态。
    #[test]
    fn working_spinner_with_trailing_stats_is_not_ready() {
        let pane = "\
✢ Prestidigitating… (11m 6s · ↓ 34.5k tokens · thought for 5s)\n\
\n\
───────────────── summon:soul-arcadia-g00000000a11ce03c ──\n\
❯ \n";
        assert!(!is_ready(pane), "带计时后缀的运行中 spinner 必须判未就绪");
        // 对照:同一张 pane 跑完后只剩完成标记 + 提示符,才算就绪。
        let idle = "✻ Worked for 7m 25s\n\n──────────────\n❯ \n";
        assert!(is_ready(idle), "完成标记不含省略号,不该被当成运行中");
    }

    #[test]
    fn extract_answer_takes_assistant_block() {
        let pane = "❯ hi\n\n⏺ hello there\n\n✻ Crunched for 3s\n────\n❯ ";
        assert_eq!(extract_answer(pane), "hello there");
    }

    #[test]
    fn real_ready_snapshot_is_ready() {
        let raw = include_str!("../../tests/fixtures/pane_ready.txt");
        assert!(is_ready(raw), "ready 快照应判为就绪");
    }

    #[test]
    fn real_thinking_snapshot_not_ready() {
        let raw = include_str!("../../tests/fixtures/pane_thinking.txt");
        assert!(!is_ready(raw), "thinking 快照应判为未就绪");
    }

    #[test]
    fn counts_done_markers() {
        let pane = "⏺ a\n✻ Crunched for 3s\n⏺ b\n✽ Brewed for 10s\n";
        assert_eq!(count_done_markers(pane), 2);
        assert_eq!(count_done_markers("✽ Composing…"), 0); // 运行中,非完成
        assert_eq!(count_done_markers("⏺ just an answer\n❯ "), 0);
    }

    #[test]
    fn real_ready_extracts_greeting() {
        let raw = include_str!("../../tests/fixtures/pane_ready.txt");
        let ans = extract_answer(raw);
        assert!(ans.contains("你好"), "got: {ans}");
        assert!(!ans.contains("Crunched"));
        assert!(!ans.contains("Model:")); // 不含 footer
    }
}
