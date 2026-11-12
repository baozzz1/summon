use std::time::{Duration, Instant};

/// 判定一轮回复是否结束:scrubbed 输出"稳定不变 ≥ debounce" 且 "就绪"。
pub struct TurnDetector {
    debounce: Duration,
    last: Option<String>,
    stable_since: Option<Instant>,
}

impl TurnDetector {
    pub fn new(debounce: Duration) -> Self {
        TurnDetector {
            debounce,
            last: None,
            stable_since: None,
        }
    }

    /// 喂一个 scrubbed 快照;`now` 当前时刻;`ready` 是否就绪态。
    /// 返回 true 表示本轮结束。
    pub fn observe(&mut self, snapshot: &str, now: Instant, ready: bool) -> bool {
        if self.last.as_deref() != Some(snapshot) {
            self.last = Some(snapshot.to_string());
            self.stable_since = Some(now);
            return false;
        }
        if ready {
            if let Some(since) = self.stable_since {
                if now.duration_since(since) >= self.debounce {
                    return true;
                }
            }
        }
        false
    }

    pub fn reset(&mut self) {
        self.last = None;
        self.stable_since = None;
    }
}

/// 无进展看门狗:喂入「本拍 pane 是否相对上一拍有变化」+ 当前时刻,记录最近一次变化的时刻;
/// 连续 `window` 时长毫无变化即判 `stalled`(疑似卡死)。
///
/// 取代「硬总时长上限」——只要会话还在动(Claude 思考时 spinner / 计时每帧在变,工具输出在流),
/// 就一直等,重 agent 的 `/summon` bootstrap 想跑多久跑多久;只有真冻住才放弃。纯逻辑,`now` 由
/// 调用方注入(Instant + Duration 可构造未来时刻),便于确定性单测。
pub struct StallWatch {
    last_change: Instant,
    window: Duration,
}

impl StallWatch {
    pub fn new(now: Instant, window: Duration) -> Self {
        StallWatch {
            last_change: now,
            window,
        }
    }

    /// 喂一拍:`changed` = 本拍 pane 相对上一拍是否有变化。有变化就把「最近变化时刻」推到 `now`。
    pub fn observe(&mut self, changed: bool, now: Instant) {
        if changed {
            self.last_change = now;
        }
    }

    /// 距最近一次变化是否已超过 `window`(连续无进展 → 疑似卡死)。
    pub fn stalled(&self, now: Instant) -> bool {
        now.duration_since(self.last_change) > self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_watch_resets_on_change_and_fires_only_when_frozen() {
        let t0 = Instant::now();
        let w = Duration::from_secs(120);
        let mut s = StallWatch::new(t0, w);
        // 刚建,未到 window
        assert!(!s.stalled(t0 + Duration::from_secs(60)));
        // 期间有变化(如 spinner 计时跳动)→ 推后最近变化时刻
        s.observe(true, t0 + Duration::from_secs(100));
        // 从上次变化(100s)起算,160s 时才过了 60s,未卡死
        assert!(!s.stalled(t0 + Duration::from_secs(160)));
        // 此后一直无变化:喂 false 不改变最近变化时刻
        s.observe(false, t0 + Duration::from_secs(200));
        // 100s + 120s = 220s 仍不算(需 > window),221s 触发
        assert!(!s.stalled(t0 + Duration::from_secs(220)));
        assert!(s.stalled(t0 + Duration::from_secs(221)));
    }

    #[test]
    fn completes_after_stable_and_ready() {
        let mut d = TurnDetector::new(Duration::from_millis(1000));
        let t0 = Instant::now();
        // 变化 → 重置
        assert!(!d.observe("partial", t0, false));
        // 又变 → 重置稳定起点
        assert!(!d.observe("done", t0 + Duration::from_millis(100), false));
        // 此后不变,但还没到 debounce
        assert!(!d.observe("done", t0 + Duration::from_millis(500), true));
        // 稳定满 1000ms 且 ready
        assert!(d.observe("done", t0 + Duration::from_millis(1200), true));
    }

    #[test]
    fn not_complete_if_not_ready() {
        let mut d = TurnDetector::new(Duration::from_millis(500));
        let t0 = Instant::now();
        d.observe("x", t0, false);
        // 稳定够久但 ready=false(仍在跑)
        assert!(!d.observe("x", t0 + Duration::from_millis(2000), false));
    }
}
