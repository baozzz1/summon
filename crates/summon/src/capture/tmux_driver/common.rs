//! 通用(codex/Other)的 tmux turn-detection 原语。不懂的 TUI 一律保守:不数完成标记、永远
//! 「就绪」,scrub/snapshot 退化为通用 chrome 清洗。

use std::time::Duration;
use summon_core::scrubber::scrub_chrome;

pub fn ready_timeout() -> Duration {
    Duration::from_secs(40)
}

pub fn seed_stall_window() -> Duration {
    // 同 claude:重 agent 冷启 bootstrap 慢工具调用让 raw pane 静默,150s 太紧会误判卡死起会话失败。
    Duration::from_secs(300)
}

pub fn snapshot(pane: &str) -> String {
    scrub_chrome(pane)
}

pub fn done_count(_pane: &str) -> usize {
    0
}

pub fn is_turn_ready(_pane: &str, _prev_done: usize) -> bool {
    true
}

pub fn is_startup_ready(_pane: &str) -> bool {
    true
}
