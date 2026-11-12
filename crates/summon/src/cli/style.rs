//! CLI 输出的语义着色(任务1):统一调色板,**只对 tty 上色**(管道/重定向 / `NO_COLOR` 自动关,
//! `CLICOLOR_FORCE` 强制开)。沿用 `event_log::paint` 的极简 ANSI 约定(无第三方依赖、可单测),按
//! 语义角色给不同 SGR code —— 让用户一眼区分「能跑的指令 / 状态 / 字段标签 / 标题」。
//!
//! 适中风格(owner 选):指令=青、状态=绿·黄·红、标签=暗灰、标题/名=加粗;**内容值不染**(保持终端
//! 前景色,不喧宾夺主)。新增着色点都走本模块的语义函数,别在各处散落裸 `\x1b[..m`。

use std::sync::OnceLock;

/// 进程级算一次:stdout 是否该上色。tty 且未设 `NO_COLOR`;`CLICOLOR_FORCE`(非 "0")强制开。
fn stdout_color() -> bool {
    use std::io::IsTerminal;
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
        return true;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// 缓存的着色开关(首个语义函数调用时定一次)。
fn enabled() -> bool {
    static COLOR: OnceLock<bool> = OnceLock::new();
    *COLOR.get_or_init(stdout_color)
}

/// 极简 ANSI 包装(纯函数、可单测):`color=false` 返回裸文本。`code` 为 SGR 码(可分号串)。
fn paint(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// 会话活跃度色相:live/recent 绿、idle 黄、gone 红+暗。
#[derive(Clone, Copy)]
pub(crate) enum Hue {
    Good,
    Idle,
    Gone,
    /// 事故:需要人去处理。`Gone` 是暗红(死了、不用管),这个是亮红加粗(还活着、正等你处理),
    /// 两者不能共用——把「需处理」渲成「已死」等于劝人别管。
    Alarm,
}

impl Hue {
    fn code(self) -> &'static str {
        match self {
            Hue::Good => "32",    // 绿
            Hue::Idle => "33",    // 黄
            Hue::Gone => "2;31",  // 暗红
            Hue::Alarm => "1;31", // 亮红加粗
        }
    }
}

/// 可直接复制运行的指令(青)。
pub(crate) fn command(s: &str) -> String {
    paint(s, "36", enabled())
}

/// 标题 / agent 名(加粗)。
pub(crate) fn heading(s: &str) -> String {
    paint(s, "1", enabled())
}

/// 字段标签 / 弱提示(暗灰)。
pub(crate) fn label(s: &str) -> String {
    paint(s, "90", enabled())
}

/// 会话状态(按色相)。
pub(crate) fn status(s: &str, hue: Hue) -> String {
    paint(s, hue.code(), enabled())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_wraps_only_when_color_on() {
        assert_eq!(paint("x", "36", false), "x"); // 关:裸文本
        let on = paint("x", "36", true);
        assert!(on.starts_with("\x1b[36m") && on.ends_with("\x1b[0m"));
        assert!(on.contains('x'));
    }

    #[test]
    fn hue_codes_distinct_per_liveness() {
        assert_eq!(Hue::Good.code(), "32");
        assert_eq!(Hue::Idle.code(), "33");
        assert_eq!(Hue::Gone.code(), "2;31");
    }
}
