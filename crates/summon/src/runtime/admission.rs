//! 全局准入闸:daemon 换代之前先关上它,不再开新的一轮,也不再起新的文档评论起草进程。
//!
//! 关闸的一方在关上之后再确认一次整机空闲,确认不了就开闸作罢。chat 线程取到消息、评论线程起草
//! 之前都过闸:闸开着就先登记「正在处理」([`runtime_status::enter_handling`]),登记之后再看一眼闸,
//! 这时才关上的就撤掉登记、接着等。关闸与登记各自先写后读(都是 SeqCst),所以两边至少有一边看得见
//! 对方:要么关闸方的空闲确认看到这条登记而作罢,要么过闸的一方看到闸已关而等待 —— 不会有一轮在
//! 「确认空闲」之后悄悄开跑。
//!
//! 停机开始后闸被封上。任务与起草不在本代开始:任务留在入站 WAL 里(未开跑)由下一代重注入,起草
//! 存根留在起草阶段由下一代接着做。压缩与其余控制消息不在 WAL 里,当场处理,随后照常被停机收尾。
//!
//! [`runtime_status::enter_handling`]: crate::observe::runtime_status::enter_handling

use crate::observe::runtime_status::{self, HandlingGuard};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

const OPEN: u8 = 0;
const FROZEN: u8 = 1;
const SEALED: u8 = 2;

/// 闸关着时过闸的一方多久看一次。
const POLL: Duration = Duration::from_millis(100);

/// 过闸的一方最多等多久。关闸方自己 60 秒内必定开闸或进入停机;这一道只防它出错时把所有会话一起挂住。
const MAX_WAIT: Duration = Duration::from_secs(90);

/// 进程里唯一的那道闸。
pub(crate) static GATE: Gate = Gate::new();

pub(crate) struct Gate {
    state: AtomicU8,
}

/// 过闸的是什么,决定它要不要等、封闸后怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// 入站任务(人、定时器、代投)或文档评论起草:要让 agent 跑起来,且有盘上记录由下一代接着做。
    /// 闸关着时等;封闸后不在本代开始。
    Task,
    /// 压缩:要让 agent 跑起来,但不在 WAL 里,不做就丢了。闸关着时等;封闸后照样开始 ——
    /// claude 的压缩在 tmux 里跑完、不受停机影响(只是长度卡可能补不上),无头会话的压缩会随停机
    /// 中止、这次压缩作罢。
    Compact,
    /// 其余控制消息:不开新的一轮,不过闸。
    Other,
}

/// 关着的闸。落地即开闸;[`Freeze::seal`] 把它封到进程退出。
pub(crate) struct Freeze<'a> {
    gate: &'a Gate,
}

impl Gate {
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU8::new(OPEN),
        }
    }

    /// 关闸。封着的闸不会被打开。
    #[cfg_attr(not(npm_dist), allow(dead_code))] // 只有自动升级在换代前关闸,不带它的构建里没有调用方
    pub(crate) fn freeze(&self) -> Freeze<'_> {
        let _ = self
            .state
            .compare_exchange(OPEN, FROZEN, Ordering::SeqCst, Ordering::SeqCst);
        Freeze { gate: self }
    }

    /// 闸此刻开着。
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.state.load(Ordering::SeqCst) == OPEN
    }

    /// 过闸。返回 `None`:停机已经开始,这条任务不在本代开跑。
    pub(crate) fn admit(&self, agent: &str, tag: &str, kind: Kind) -> Option<HandlingGuard> {
        self.admit_with(kind, MAX_WAIT, || {
            runtime_status::enter_handling(agent, tag)
        })
    }

    fn admit_with<G>(&self, kind: Kind, max_wait: Duration, enter: impl Fn() -> G) -> Option<G> {
        if kind == Kind::Other {
            return Some(enter());
        }
        let started = Instant::now();
        loop {
            match self.state.load(Ordering::SeqCst) {
                OPEN => {
                    let guard = enter();
                    if self.state.load(Ordering::SeqCst) == OPEN {
                        return Some(guard);
                    }
                }
                SEALED => return (kind != Kind::Task).then(&enter),
                _ if started.elapsed() >= max_wait => {
                    crate::observe::log::log(&summon_i18n::format!(
                        "r2baa6058d2",
                        max_wait.as_secs()
                    ));
                    return Some(enter());
                }
                _ => {}
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Freeze<'_> {
    /// 停机已经开始:保持关闭直到进程退出。
    #[cfg_attr(not(npm_dist), allow(dead_code))] // 只有自动升级在停机时封闸,不带它的构建里没有调用方
    pub(crate) fn seal(self) {
        self.gate.state.store(SEALED, Ordering::SeqCst);
        std::mem::forget(self);
    }
}

impl Drop for Freeze<'_> {
    fn drop(&mut self) {
        let _ = self
            .gate
            .state
            .compare_exchange(FROZEN, OPEN, Ordering::SeqCst, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    fn admit(gate: &Gate, kind: Kind, entered: &AtomicUsize) -> Option<()> {
        gate.admit_with(kind, Duration::from_secs(5), || {
            entered.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn an_open_gate_admits_at_once() {
        let gate = Gate::new();
        let entered = AtomicUsize::new(0);
        for kind in [Kind::Task, Kind::Compact, Kind::Other] {
            assert_eq!(admit(&gate, kind, &entered), Some(()));
        }
        assert_eq!(entered.load(Ordering::SeqCst), 3);
    }

    /// 关着的闸挡住任务,开闸后放行;控制消息不过闸。
    #[test]
    fn a_frozen_gate_holds_tasks_until_it_opens() {
        let gate: &'static Gate = Box::leak(Box::new(Gate::new()));
        let freeze = gate.freeze();
        assert!(!gate.is_open());
        let entered = Arc::new(AtomicUsize::new(0));
        assert_eq!(admit(gate, Kind::Other, &entered), Some(()));
        let waiter = {
            let entered = Arc::clone(&entered);
            std::thread::spawn(move || admit(gate, Kind::Task, &entered))
        };
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(entered.load(Ordering::SeqCst), 1, "闸关着时任务不该开始");
        drop(freeze);
        assert!(gate.is_open());
        assert_eq!(waiter.join().unwrap(), Some(()));
        assert_eq!(entered.load(Ordering::SeqCst), 2);
    }

    /// 封闸后任务不在本代开跑;压缩与控制消息照常处理。
    #[test]
    fn a_sealed_gate_turns_tasks_away() {
        let gate: &'static Gate = Box::leak(Box::new(Gate::new()));
        let freeze = gate.freeze();
        let entered = Arc::new(AtomicUsize::new(0));
        let waiter = {
            let entered = Arc::clone(&entered);
            std::thread::spawn(move || admit(gate, Kind::Task, &entered))
        };
        std::thread::sleep(Duration::from_millis(200));
        freeze.seal();
        assert_eq!(waiter.join().unwrap(), None);
        assert_eq!(entered.load(Ordering::SeqCst), 0);
        assert_eq!(admit(gate, Kind::Task, &entered), None);
        assert_eq!(admit(gate, Kind::Compact, &entered), Some(()));
        assert_eq!(admit(gate, Kind::Other, &entered), Some(()));
        drop(gate.freeze());
        assert_eq!(
            admit(gate, Kind::Task, &entered),
            None,
            "封着的闸不会被打开"
        );
    }

    /// 登记之后才发现闸关了:撤掉登记接着等,关闸方据登记表看到的就不会是一条已开跑的任务。
    #[test]
    fn a_task_that_registered_as_the_gate_closed_backs_off() {
        let gate: &'static Gate = Box::leak(Box::new(Gate::new()));
        let live = Arc::new(AtomicUsize::new(0));
        let freezes = Arc::new(std::sync::Mutex::new(None));
        let waiter = {
            let (live, freezes) = (Arc::clone(&live), Arc::clone(&freezes));
            let froze = std::sync::atomic::AtomicBool::new(false);
            std::thread::spawn(move || {
                struct Live(Arc<AtomicUsize>);
                impl Drop for Live {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                gate.admit_with(Kind::Task, Duration::from_secs(5), || {
                    live.fetch_add(1, Ordering::SeqCst);
                    // 第一次登记时恰好关闸。
                    if !froze.swap(true, Ordering::SeqCst) {
                        *freezes.lock().unwrap() = Some(gate.freeze());
                    }
                    Live(Arc::clone(&live))
                })
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(live.load(Ordering::SeqCst), 0, "闸关上后撤掉了那次登记");
        drop(freezes.lock().unwrap().take());
        let admitted = waiter.join().unwrap();
        assert!(admitted.is_some());
        assert_eq!(live.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_gate_left_frozen_stops_holding_after_the_limit() {
        let gate = Gate::new();
        let _freeze = gate.freeze();
        let started = Instant::now();
        assert_eq!(
            gate.admit_with(Kind::Task, Duration::from_millis(200), || ()),
            Some(())
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
    }
}
