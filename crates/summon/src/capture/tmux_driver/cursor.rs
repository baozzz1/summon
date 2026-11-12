//! Cursor 的 tmux turn-detection 原语:判就绪/输入清空/答案清洗,复用 summon-core 仍保留的纯
//! cursor 助手(`is_cursor_ready`/`cursor_input_is_clear`/`scrub_cursor_chrome`)。

use std::time::Duration;
use summon_core::driver::{cursor_input_is_clear, is_cursor_ready, scrub_cursor_chrome};
use summon_core::scrubber::strip_ansi;

pub fn ready_timeout() -> Duration {
    Duration::from_secs(60)
}

pub fn seed_stall_window() -> Duration {
    Duration::from_secs(180)
}

pub fn snapshot(pane: &str) -> String {
    scrub_cursor_chrome(pane)
}

pub fn done_count(_pane: &str) -> usize {
    0
}

pub fn is_turn_ready(pane: &str, _prev_done: usize) -> bool {
    is_cursor_ready(pane) && cursor_input_is_clear(pane)
}

pub fn is_startup_ready(pane: &str) -> bool {
    is_cursor_ready(pane) && strip_ansi(pane).contains("Cursor Agent")
}
