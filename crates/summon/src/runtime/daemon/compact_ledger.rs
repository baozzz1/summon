//! 压缩后的上下文长度卡只推一张。
//!
//! 同一次压缩有两条路会报:summon 自己发起的压缩(原地交接、Compact 按钮、`/compact` 文本)由
//! 发起方量完推卡,好让卡排在它自己的结果说明之后;claude 的 PostCompact hook 对每次压缩都报一次,
//! 包括 summon 发起的那些。所以 summon 发起时先登记在途,hook 那条路见到在途就让给发起方;
//! 两条路推卡前都按「转录 + 边界结束位置」认领,同一条边界只有第一个认领的推。
//!
//! 只在本代 daemon 内存里:换代后旧压缩的 hook 不会再来,无需落盘。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

#[derive(Default)]
struct Ledger {
    /// tmux 会话名 → 在途的 summon 发起的压缩数。
    compacting: HashMap<String, usize>,
    /// 转录 → 已报告的最后一条边界的结束位置。边界只会往后追加,位置更靠后的才是新的。
    reported: HashMap<PathBuf, u64>,
}

impl Ledger {
    fn begin(&mut self, session: &str) {
        *self.compacting.entry(session.to_string()).or_default() += 1;
    }

    fn end(&mut self, session: &str) {
        if let Some(count) = self.compacting.get_mut(session) {
            *count -= 1;
            if *count == 0 {
                self.compacting.remove(session);
            }
        }
    }

    fn compacting(&self, session: &str) -> bool {
        self.compacting.contains_key(session)
    }

    fn claim(&mut self, transcript: &Path, boundary_end: u64) -> bool {
        let reported = self.reported.entry(transcript.to_path_buf()).or_default();
        if boundary_end <= *reported {
            return false;
        }
        *reported = boundary_end;
        true
    }
}

static LEDGER: LazyLock<Mutex<Ledger>> = LazyLock::new(|| Mutex::new(Ledger::default()));

/// summon 发起的一次压缩,从注入压缩命令之前到认领完边界。drop 时撤销在途登记。
pub(super) struct SummonCompaction {
    session: String,
}

impl Drop for SummonCompaction {
    fn drop(&mut self) {
        if let Ok(mut ledger) = LEDGER.lock() {
            ledger.end(&self.session);
        }
    }
}

/// 登记 summon 要在这个会话里发起压缩。必须在注入压缩命令之前调用。
pub(super) fn begin_summon_compaction(session: &str) -> SummonCompaction {
    if let Ok(mut ledger) = LEDGER.lock() {
        ledger.begin(session);
    }
    SummonCompaction {
        session: session.to_string(),
    }
}

/// 这个会话此刻有没有 summon 发起的压缩在途。锁坏了按在途处理:宁可少推一张卡,不推重复的。
pub(super) fn summon_compacting(session: &str) -> bool {
    LEDGER
        .lock()
        .map(|ledger| ledger.compacting(session))
        .unwrap_or(true)
}

/// 认领 `transcript` 里结束于 `boundary_end` 的那条边界。返回 true 的那一方推卡;这条边界已经
/// 报过(或更靠后的边界已经报过)返回 false。锁坏了不认领。
pub(super) fn claim(transcript: &Path, boundary_end: u64) -> bool {
    LEDGER
        .lock()
        .map(|mut ledger| ledger.claim(transcript, boundary_end))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boundary_is_claimed_once_and_only_newer_ones_after_it() {
        let mut ledger = Ledger::default();
        let a = Path::new("/p/a.jsonl");
        let b = Path::new("/p/b.jsonl");
        assert!(ledger.claim(a, 100));
        assert!(!ledger.claim(a, 100), "同一条边界只认领一次");
        assert!(!ledger.claim(a, 90), "更早的边界不再报");
        assert!(ledger.claim(b, 100), "不同转录各自计");
        assert!(ledger.claim(a, 250));
    }

    #[test]
    fn summon_compactions_are_counted_per_session() {
        let mut ledger = Ledger::default();
        assert!(!ledger.compacting("soul-k-g1"));
        ledger.begin("soul-k-g1");
        ledger.begin("soul-k-g1");
        assert!(ledger.compacting("soul-k-g1"));
        assert!(!ledger.compacting("soul-k-g2"));
        ledger.end("soul-k-g1");
        assert!(ledger.compacting("soul-k-g1"));
        ledger.end("soul-k-g1");
        assert!(!ledger.compacting("soul-k-g1"));
        ledger.end("soul-k-g1");
        assert!(!ledger.compacting("soul-k-g1"));
    }

    #[test]
    fn the_guard_withdraws_the_registration_when_dropped() {
        let session = "soul-ledger-guard-test";
        let guard = begin_summon_compaction(session);
        assert!(summon_compacting(session));
        drop(guard);
        assert!(!summon_compacting(session));
    }
}
