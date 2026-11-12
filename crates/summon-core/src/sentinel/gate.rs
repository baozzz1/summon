//! sentinel 的三道纯判定闸:时效、去重水位、小时限额,外加冷启判定。
//!
//! 全部纯函数 + 可序列化状态,落盘/加载归调用方(core 不碰 IO)。这些闸防的是同一类事故:
//! 水位错乱/首启无水位时把**历史**触发消息当新指令复放——boot 补发事故(2026-08-07,见仓根
//! CLAUDE.md)的同构风险。判据必须落"时间窗 + 已见集合",不能只写技术条件。

use std::collections::{BTreeMap, VecDeque};

/// 闸门状态。调用方(summon 侧 poller)负责持久化到 `~/.summon/run/` 下并在每 tick 回写。
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct GateState {
    /// 已处理的触发 message_id(有界 FIFO)。线性查找 O(n),n=[`SEEN_CAP`],每 tick 只有
    /// 个位数查询,不值得为它上索引结构。
    #[serde(default)]
    pub seen: VecDeque<String>,
    /// 已执行自动动作的 epoch 秒(限额窗口用,一小时外的会被剪掉)。
    #[serde(default)]
    pub actions: VecDeque<i64>,
    /// tail 通道 per-chat 水位:chat_id → 已见最大 create_time_ms。无水位会话由 poller 同时用
    /// 时效窗与本次启动边界约束，既不复放启动前消息，也不吞掉启动后把会话顶进活跃集的首条触发。
    #[serde(default)]
    pub chat_watermarks: BTreeMap<String, i64>,
    /// 搜索通道时间窗起点(epoch 秒)。0 = 全局冷启:本轮只建立水位,禁止任何动作。
    #[serde(default)]
    pub search_since: i64,
}

/// 已见 message_id 的容量上限。远大于时效窗(5min)内可能出现的触发数,纯粹是防状态无界。
pub const SEEN_CAP: usize = 4096;
/// 限额窗口长度(秒)。
const RATE_WINDOW_SECS: i64 = 3600;

/// 触发消息新鲜吗(距今 ≤ max_age_secs)。`create_time_ms<=0`(未知)→ 不新鲜:判不出
/// 新鲜度就不动作。轻微时钟偏差(create_time 略在未来)按新鲜处理。
pub fn fresh(create_time_ms: i64, now_ms: i64, max_age_secs: i64) -> bool {
    if create_time_ms <= 0 {
        return false;
    }
    now_ms - create_time_ms <= max_age_secs * 1000
}

/// 未见过则登记并返回 true;见过返回 false。
pub fn dedup_admit(state: &mut GateState, message_id: &str) -> bool {
    if state.seen.iter().any(|m| m == message_id) {
        return false;
    }
    state.seen.push_back(message_id.to_string());
    while state.seen.len() > SEEN_CAP {
        state.seen.pop_front();
    }
    true
}

/// 限额窗口内还有名额则登记并返回 true;满额返回 false(调用方记录并私聊告知,不执行)。
pub fn rate_admit(state: &mut GateState, now_epoch_secs: i64, per_hour: usize) -> bool {
    while let Some(&front) = state.actions.front() {
        if now_epoch_secs - front >= RATE_WINDOW_SECS {
            state.actions.pop_front();
        } else {
            break;
        }
    }
    if state.actions.len() >= per_hour {
        return false;
    }
    state.actions.push_back(now_epoch_secs);
    true
}

/// 全局冷启判定:搜索窗起点还没建立过 → 本轮只推水位,禁止动作(首启复放防御)。
/// per-chat 的冷启(无水位 chat)由 poller 按 [`GateState::chat_watermarks`] 单独执行。
pub fn cold_start(state: &GateState) -> bool {
    state.search_since == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 时效_五分钟内新鲜() {
        assert!(fresh(1_000_000, 1_000_000 + 4 * 60 * 1000, 300));
        assert!(!fresh(1_000_000, 1_000_000 + 6 * 60 * 1000, 300));
    }

    #[test]
    fn 时效_未知时间不新鲜() {
        assert!(!fresh(0, 1_000_000, 300)); // 判不出新鲜度 = 不动作
        assert!(!fresh(-5, 1_000_000, 300));
    }

    #[test]
    fn 时效_轻微未来时间戳按新鲜() {
        assert!(fresh(1_000_000 + 30_000, 1_000_000, 300));
    }

    #[test]
    fn 去重_二次见到拒绝() {
        let mut s = GateState::default();
        assert!(dedup_admit(&mut s, "om_1"));
        assert!(!dedup_admit(&mut s, "om_1"));
    }

    #[test]
    fn 去重_有界淘汰() {
        let mut s = GateState::default();
        for i in 0..(SEEN_CAP + 10) {
            assert!(dedup_admit(&mut s, &format!("om_{i}")));
        }
        assert!(s.seen.len() <= SEEN_CAP);
        // 最老的被挤出后可以再进(时效闸另行兜底,不依赖 seen 永久记忆)。
        assert!(dedup_admit(&mut s, "om_0"));
    }

    #[test]
    fn 限额_窗口内计数() {
        let mut s = GateState::default();
        for _ in 0..6 {
            assert!(rate_admit(&mut s, 1000, 6));
        }
        assert!(!rate_admit(&mut s, 1000, 6));
        assert!(rate_admit(&mut s, 1000 + 3601, 6)); // 旧记录滑出窗口
    }

    #[test]
    fn 冷启判定() {
        let s = GateState::default();
        assert!(cold_start(&s)); // 首启无水位 → 只推水位不动作(boot 事故同构防御)
        let s2 = GateState {
            search_since: 123,
            ..Default::default()
        };
        assert!(!cold_start(&s2));
    }

    #[test]
    fn 状态_serde_roundtrip() {
        let mut s = GateState::default();
        dedup_admit(&mut s, "om_1");
        rate_admit(&mut s, 42, 6);
        s.chat_watermarks.insert("oc_x".into(), 999);
        s.search_since = 7;
        let json = serde_json::to_string(&s).unwrap();
        let back: GateState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seen, s.seen);
        assert_eq!(back.actions, s.actions);
        assert_eq!(back.chat_watermarks, s.chat_watermarks);
        assert_eq!(back.search_since, 7);
    }
}
