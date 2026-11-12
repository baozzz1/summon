//! 流式卡片的可恢复状态:daemon 重启后,`resume_pending_turn` 读它来回填
//! stream_map,让重跑续刷同一张卡(不发新卡)。
//!
//! 纯 schema + 路径计算,不做 IO——落盘/读取/删除在 `summon` 侧的 `inflight.rs`。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 一张正在流式推送的卡片的可恢复状态。
///
/// 核心锚点是 `message_id`:续跑时把它回填进 `stream_map`,后续 `stream_patch_card`
/// 就能原地刷新同一张飞书卡片,而不是新发一张。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCardState {
    /// 飞书 message_id——续传的核心锚点
    pub message_id: String,
    /// chat tag(g+8hex / p+8hex),供 @mention 替换查 peers
    pub chat_tag: String,
    /// 本卡是否内嵌「⏸ 打断」按钮
    pub with_interrupt: bool,
    /// 写入时刻(unix ms),供未来做 stale 检测
    pub created_ms: u64,
}

/// `threads/<tag>/stream_state.json` 路径。
pub fn stream_state_path(base: &Path, tag: &str) -> PathBuf {
    base.join("threads").join(tag).join("stream_state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_state_path_under_threads_tag() {
        let p = stream_state_path(Path::new("/tmp/agent"), "g1a2b3c4d");
        assert_eq!(
            p,
            PathBuf::from("/tmp/agent/threads/g1a2b3c4d/stream_state.json")
        );
    }
}
