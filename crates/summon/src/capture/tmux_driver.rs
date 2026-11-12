//! tmux TUI 的逐 CLI 「轮次检测」原语:claude 在 Transcript 模式下仍交互式跑在 tmux,故冷启
//! seed、启动就绪、`/compact` 等待这些**靠抓屏判 turn 边界**的原语仍需要。把「一种 CLI 的 TUI
//! 抓屏知识」收口在这一个类型里,集中分派到 `tmux_driver::{claude,cursor,codex,common}` 子模块,
//! 而不是散落在 `CliDriver` 上。由 `CliDriver` 一对一构造(`Session::tmux_driver`)。
//!
//! 注:这里只做 turn-detection(snapshot/scrub/done_count/is_turn_ready/is_startup_ready/计时窗口),
//! 不解析输出内容(回灌走结构化转录/子进程)。

mod claude;
mod codex;
mod common;
mod cursor;

use std::time::Duration;

use summon_core::driver::CliDriver;

/// tmux turn-detection 是 per-CLI 的(诚实建模):把一种 CLI 的抓屏 turn 判定收口在这一个新类型里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TmuxDriver(CliDriver);

impl TmuxDriver {
    pub fn for_cli(cli: CliDriver) -> Self {
        TmuxDriver(cli)
    }

    pub fn ready_timeout(self) -> Duration {
        match self.0 {
            CliDriver::Claude => claude::ready_timeout(),
            CliDriver::Cursor => cursor::ready_timeout(),
            CliDriver::Codex | CliDriver::Traex | CliDriver::Other => common::ready_timeout(),
        }
    }

    pub fn seed_stall_window(self) -> Duration {
        match self.0 {
            CliDriver::Claude => claude::seed_stall_window(),
            CliDriver::Cursor => cursor::seed_stall_window(),
            CliDriver::Codex | CliDriver::Traex | CliDriver::Other => common::seed_stall_window(),
        }
    }

    pub fn snapshot(self, pane: &str) -> String {
        match self.0 {
            CliDriver::Claude => claude::snapshot(pane),
            CliDriver::Cursor => cursor::snapshot(pane),
            CliDriver::Codex | CliDriver::Traex => codex::snapshot(pane),
            CliDriver::Other => common::snapshot(pane),
        }
    }

    pub fn done_count(self, pane: &str) -> usize {
        match self.0 {
            CliDriver::Claude => claude::done_count(pane),
            CliDriver::Cursor => cursor::done_count(pane),
            CliDriver::Codex | CliDriver::Traex | CliDriver::Other => common::done_count(pane),
        }
    }

    pub fn is_turn_ready(self, pane: &str, prev_done: usize) -> bool {
        match self.0 {
            CliDriver::Claude => claude::is_turn_ready(pane, prev_done),
            CliDriver::Cursor => cursor::is_turn_ready(pane, prev_done),
            CliDriver::Codex | CliDriver::Traex | CliDriver::Other => {
                common::is_turn_ready(pane, prev_done)
            }
        }
    }

    /// 注入的 CLI 命令跑完没有(`Session::command_and_wait` 用)。与 `is_turn_ready` 分开:命令
    /// 不产出助手答案,拿「有新答案/新完成标记」判它必然等到超时。只有 claude 有受控命令通道
    /// (`CompactSpec::Tmux`),故只有它放宽;其余 CLI 是 `Unsupported`,保持原判据不动。
    pub fn is_command_done(self, pane: &str, prev_done: usize) -> bool {
        match self.0 {
            CliDriver::Claude => claude::is_command_done(pane, prev_done),
            CliDriver::Cursor | CliDriver::Codex | CliDriver::Traex | CliDriver::Other => {
                self.is_turn_ready(pane, prev_done)
            }
        }
    }

    pub fn is_startup_ready(self, pane: &str) -> bool {
        match self.0 {
            CliDriver::Claude => claude::is_startup_ready(pane),
            CliDriver::Cursor => cursor::is_startup_ready(pane),
            CliDriver::Codex | CliDriver::Traex | CliDriver::Other => {
                common::is_startup_ready(pane)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn td(cli: CliDriver) -> TmuxDriver {
        TmuxDriver::for_cli(cli)
    }

    /// `/compact` 的真机收尾只有一行 `⎿  Compacted (…)`,**没有**完成标记 —— 旧判据(done-marker
    /// 增长)对它恒 false,必然白等到 cap 再把成功的压缩报成失败。`is_command_done` 靠「回到空闲
    /// 提示符」认出它,同时不能把压缩进行中误判成完成。
    #[test]
    fn claude_compact_finishes_without_a_done_marker() {
        let running = "✢ Compacting… (2m 1s · ↓ 12.0k tokens)\n──────\n❯ \n";
        let finished = "\
❯ /compact\n\
  ⎿  Compacted (ctrl+o to see full summary)\n\
  ⎿  Skills restored (claude-api)\n\
──────\n\
❯ \n";
        let claude = td(CliDriver::Claude);
        assert_eq!(claude.done_count(finished), 0, "compact 不打完成标记");
        assert!(
            !claude.is_turn_ready(finished, 0),
            "旧判据认不出 compact 收尾——这正是超时误报的根因"
        );
        assert!(!claude.is_command_done(running, 0), "压缩进行中不得判完成");
        assert!(
            claude.is_command_done(finished, 0),
            "回到空闲提示符 = 命令跑完"
        );
    }

    /// 收紧弱判据的回归:命令注入后弹了菜单 / 输入框里留着草稿时,画面同样静止且同样含 `❯`,
    /// 但命令**没跑完**。旧判据(`❯` 存在即可)会在这里发成功回执,同时把会话锁死在菜单上。
    #[test]
    fn claude_command_is_not_done_while_a_menu_or_draft_holds_the_prompt() {
        let claude = td(CliDriver::Claude);
        let menu = "❯ /model\n\nSelect model:\n❯ 1. opus-5\n  2. sonnet-5\n";
        let draft = "❯ /compact\n  ⎿  Compacted\n──────\n❯ 用户回头又打了半句\n──────\n";
        assert!(menu.contains('❯') && draft.contains('❯'), "样本必须含 ❯");
        assert!(!claude.is_command_done(menu, 0), "菜单没结束不算命令跑完");
        assert!(
            !claude.is_command_done(draft, 0),
            "输入框有草稿不算回到空闲"
        );
    }

    #[test]
    fn cursor_startup_not_ready_while_working() {
        assert!(!td(CliDriver::Cursor).is_startup_ready(" ⠰⠳ Working\n"));
    }

    #[test]
    fn cursor_turn_not_ready_while_input_still_pending() {
        let pane = "\
  已收到任务 C\n\
\n\
 ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄\n\
  → !快速测试任务D                                                      ctrl+c to stop\n\
 ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n\
  GPT-5.5 1M High · 12%                                               Auto-run\n";

        assert!(!td(CliDriver::Cursor).is_turn_ready(pane, 0));
    }

    #[test]
    fn cursor_turn_ready_when_completed_prompt_is_echoed() {
        let pane = "\
  pong\n\
\n\
 ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄\n\
  → 用一句话回复：ping\n\
 ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n\
  GPT-5.5 1M High · 12%                                               Auto-run\n";

        assert!(td(CliDriver::Cursor).is_turn_ready(pane, 0));
    }

    #[test]
    fn cursor_snapshot_removes_status_chrome_but_keeps_answer() {
        let pane = "\
Tip: You can start the Cursor CLI with `agent` (same as `cursor-agent`).\n\
\n\
  Cursor Agent\n\
  v2026.05.28-418efe5\n\
  Use /skills to give Cursor specialized knowledge for tasks.\n\
\n\
  用一句话回复：ping\n\
\n\
\n\
  pong\n\
\n\
\n\
 ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄\n\
  → 用一句话回复：ping\n\
 ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n\
  GPT-5.5 1M High · 12%                                               Auto-run\n\
  ~/Desktop/personal/summon · develop\n";
        let snap = td(CliDriver::Cursor).snapshot(pane);
        assert!(snap.contains("pong"));
        assert!(!snap.contains("Auto-run"));
        assert!(!snap.contains("Cursor Agent"));
        assert!(!snap.contains("▄▄"));
    }

    #[test]
    fn claude_done_count_and_turn_ready_track_markers() {
        // 完成标记计数驱动轮次就绪:出现比基线更多的 `✻ … for Ns` 才算本轮结束。
        let one = "⏺ a\n✻ Crunched for 3s\n";
        assert_eq!(td(CliDriver::Claude).done_count(one), 1);
        assert!(td(CliDriver::Claude).is_turn_ready(one, 0));
        assert!(!td(CliDriver::Claude).is_turn_ready(one, 1));
    }

    #[test]
    fn common_driver_is_always_ready_and_counts_nothing() {
        // 不懂的 TUI 保守:不数标记、永远就绪(codex/Other 走非 capture 路径)。
        assert_eq!(td(CliDriver::Other).done_count("anything"), 0);
        assert!(td(CliDriver::Other).is_turn_ready("anything", 99));
        assert!(td(CliDriver::Other).is_startup_ready("anything"));
        assert_eq!(td(CliDriver::Codex).done_count("x"), 0);
    }
}
