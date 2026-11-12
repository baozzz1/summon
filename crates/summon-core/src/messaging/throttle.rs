//! 流式卡片 patch 的时间窗节流纯决策(U1)。
//!
//! 飞书「更新卡片 5 QPS/消息」,无节流的逐行 PATCH(cursor 还按 token 级 delta 出行)在长回复里
//! 极易撞频控。每次 patch 都带**累积全文**,窗口内被跳过的内容会被后续放行的 patch / turn 终态
//! 对账(`Frontend::stream_finish`)带上,**不丢内容只降刷新频率**。时间由调用方注入(单调毫秒),
//! 保持确定性可单测(本仓惯例:时间/随机不进纯件)。

/// 单张流式卡片的 patch 节流器:`admit(now_ms)` 判定是否放行本次 patch,放行即记账。
#[derive(Debug, Clone)]
pub struct PatchThrottle {
    min_interval_ms: u64,
    last_ms: Option<u64>,
}

impl PatchThrottle {
    /// 新建,`last_ms = None`——首次 `admit` 恒放行。用于 daemon 重启后续传同一张卡:
    /// 卡片已冻结一段时间,首个 patch 应立即发出,不被节流窗口挡住。
    pub fn new(min_interval_ms: u64) -> Self {
        PatchThrottle {
            min_interval_ms,
            last_ms: None,
        }
    }

    /// 新建,且视为「`now_ms` 刚发过一次」——`stream_begin` 本身已对该消息发出一次写操作,
    /// 首个 patch 也要等满窗口,避免 begin+patch 连发撞频控。
    pub fn primed(min_interval_ms: u64, now_ms: u64) -> Self {
        PatchThrottle {
            min_interval_ms,
            last_ms: Some(now_ms),
        }
    }

    /// 距上次放行 ≥ 窗口 → 放行并记账;否则拒绝(调用方直接跳过本次 patch,等下一条累积全文)。
    pub fn admit(&mut self, now_ms: u64) -> bool {
        if let Some(last) = self.last_ms {
            if now_ms.saturating_sub(last) < self.min_interval_ms {
                return false;
            }
        }
        self.last_ms = Some(now_ms);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primed_denies_within_window_then_admits() {
        let mut t = PatchThrottle::primed(500, 1_000);
        assert!(!t.admit(1_001), "begin 刚发过,窗口内的 patch 应被跳过");
        assert!(!t.admit(1_499), "差 1ms 满窗也不放行");
        assert!(t.admit(1_500), "满窗放行");
        assert!(!t.admit(1_900), "放行后重新记账,窗口重置");
        assert!(t.admit(2_000));
    }

    #[test]
    fn admit_is_monotonic_safe_on_clock_regression() {
        // 单调钟理论上不回退;saturating_sub 兜底:回退时视为窗口内,拒绝而非 panic/放行洪峰。
        let mut t = PatchThrottle::primed(500, 1_000);
        assert!(!t.admit(900));
        assert!(t.admit(1_500));
    }
}
