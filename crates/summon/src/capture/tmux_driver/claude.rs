//! Claude 的 tmux turn-detection 原语:抓干净答案 / 数完成标记 / 判轮次就绪。底层文本处理用
//! summon-core 的纯 `scrubber` 助手。Transcript 模式下 claude 仍跑在 tmux,故冷启 seed / 启动
//! 就绪 / `/compact` 等待仍需要这些。

use std::time::Duration;
use summon_core::scrubber::{count_done_markers, extract_answer, is_ready};

pub fn ready_timeout() -> Duration {
    Duration::from_secs(40)
}

pub fn seed_stall_window() -> Duration {
    // 冷启 seed(`/summon` bootstrap)用抓屏看门狗判完成,但重 agent 的 bootstrap 会读 SOUL/memory、
    // 跑慢工具调用(如日历 API),raw pane 可能静默数分钟 → 150s 太紧、误判卡死、整轮起会话失败
    // (实测 arcadia 群/私聊唤起失败)。放宽到 300s:真卡死仍兜底,正常重 bootstrap 不再被误掐。
    // 注:根因是「seed 完成用抓屏而非读转录」,彻底修需把 seed 等待也改成 tail 转录(见 TODO)。
    Duration::from_secs(300)
}

pub fn snapshot(pane: &str) -> String {
    extract_answer(pane)
}

pub fn done_count(pane: &str) -> usize {
    count_done_markers(pane)
}

pub fn is_turn_ready(pane: &str, prev_done: usize) -> bool {
    count_done_markers(pane) > prev_done
}

/// 注入的 slash 命令(`/compact`)是否跑完了。
///
/// **不能只看 done-marker**:命令不产出助手答案,claude 也不为它打完成标记 —— 真机收尾只有一行
/// `⎿  Compacted (ctrl+o to see full summary)`。于是 `is_turn_ready` 对它恒为 false,等待只能靠
/// stall/cap 超时收场,把**成功的**压缩回执成「可能未正常完成(等待超时)」,顺带把 chat 线程
/// 白占满一个 cap(2026-08-12 真机:15:30:18 发起,15:35:24 报超时,期间新消息全排队)。
///
/// 追加的这条出路不依赖任何文案:**回到空的输入行**(无 spinner + 末尾有一个空 `❯` 行)。
/// 残余竞态:注入后若 claude 尚未开始渲染 spinner,pane 会短暂停在空闲态。靠 `inject_command`
/// 的 2×grace + 本轮 grace(共 4.5s)与 `TurnDetector` 的 debounce 压窗口,没有彻底消除。
///
/// **判据从 `is_startup_ready`(「`❯` 存在即可」)收紧到 `is_input_ready`(「空 `❯` 输入行」)**:
/// 前者把菜单光标 `❯ 1. …`、scrollback 里的历史回显 `❯ /clear`、attach 用户没发送的草稿一律
/// 算成就绪 —— 命令其实卡在菜单上,却当成跑完了发成功回执,这正是 spec 第一节「静止 ≠ 完成」
/// 那类事故。收紧后这些形态一律等到 stall/cap,回执如实说「不确定」。
pub fn is_command_done(pane: &str, prev_done: usize) -> bool {
    is_turn_ready(pane, prev_done) || summon_core::pane_input::is_input_ready(pane)
}

pub fn is_startup_ready(pane: &str) -> bool {
    is_ready(pane) && pane.contains('❯')
}
