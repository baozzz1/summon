//! Codex 的 tmux turn-detection 原语。snapshot 与 common 同;其余(done_count/is_turn_ready
//! /is_startup_ready/计时窗口)也复用 common(codex 走非 capture 路径,这些只在 dispatch 上对齐 Other)。

use super::common;

pub fn snapshot(pane: &str) -> String {
    common::snapshot(pane)
}
