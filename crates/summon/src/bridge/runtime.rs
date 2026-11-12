//! daemon 生命周期内的 bridge Hook drain 线程。

use crate::bridge::drain::drain_tick;
use crate::bridge::store::BridgeStore;
use crate::observe::log::log;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const DRAIN_POLL: Duration = Duration::from_millis(100);

type DeliveryGate = Mutex<()>;
static DELIVERY_GATES: OnceLock<Mutex<HashMap<String, Weak<DeliveryGate>>>> = OnceLock::new();

/// 同一目标群的正文 POST 与 stop/chat-gone/rebind 线性化；不同群仍可并行。此锁只存在 daemon
/// 进程内，绝不与 Hook spool/state flock 共用，因此慢网络不会卡住 Claude 的 2s Hook 热路径。
pub fn with_chat_delivery_gate<T>(
    target: &summon_core::bridge::ScopedChatKey,
    action: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let gate_key = target.storage_key();
    let gate = {
        let gates = DELIVERY_GATES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut gates = gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        gates.retain(|_, gate| gate.strong_count() > 0);
        match gates.get(&gate_key).and_then(Weak::upgrade) {
            Some(gate) => gate,
            None => {
                let gate = Arc::new(Mutex::new(()));
                gates.insert(gate_key, Arc::downgrade(&gate));
                gate
            }
        }
    };
    let _guard = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    action()
}

/// Hook batch 可能同时让多个 SessionKey 跨 generation；按 target storage key 排序后逐把获取，
/// 保持与单群 delivery 相同的 gate→state 锁序，避免多群之间 ABBA。
pub fn with_chat_delivery_gates<T>(
    targets: &[summon_core::bridge::ScopedChatKey],
    action: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let unique = targets
        .iter()
        .cloned()
        .map(|target| (target.storage_key(), target))
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_values()
        .collect::<Vec<_>>();
    let mut action = Some(action);
    with_gate_chain(&unique, &mut action)
}

fn with_gate_chain<T, F>(
    targets: &[summon_core::bridge::ScopedChatKey],
    action: &mut Option<F>,
) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T>,
{
    let Some((target, rest)) = targets.split_first() else {
        return action.take().expect(summon_i18n::text!("r409f17dfd2"))();
    };
    with_chat_delivery_gate(target, || with_gate_chain(rest, action))
}

pub fn stop_mirror_exact(
    store: &BridgeStore,
    target: &summon_core::bridge::ScopedChatKey,
    binding_id: &str,
    binding_revision: u64,
) -> anyhow::Result<summon_core::bridge::ChatBinding> {
    with_chat_delivery_gate(target, || {
        store.with_state(|state| {
            state
                .stop_mirror(target, binding_id, binding_revision)
                .map_err(anyhow::Error::new)
        })
    })
}

pub struct BridgeDrain {
    handle: JoinHandle<()>,
}

impl BridgeDrain {
    pub fn join(self) -> thread::Result<()> {
        self.handle.join()
    }
}

/// 启动前先同步消费一次 cold-start backlog，再起只看共享 shutdown flag 的本地线程。
/// 本函数及 worker 都不持有 frontend/tmux 句柄。
pub fn spawn_bridge_drain(shutdown: Arc<AtomicBool>) -> BridgeDrain {
    spawn_bridge_drain_with_stores(
        BridgeStore::runtime_default(),
        crate::runtime::outbox_store::OutboxStore::at_run_dir(),
        shutdown,
    )
}

fn spawn_bridge_drain_with_stores(
    store: BridgeStore,
    outbox: crate::runtime::outbox_store::OutboxStore,
    shutdown: Arc<AtomicBool>,
) -> BridgeDrain {
    drain_once(&store, &outbox);
    let handle = thread::spawn(move || {
        while !shutdown.load(Ordering::SeqCst) {
            drain_once(&store, &outbox);
            // 一觉睡满一个 poll 周期,不再拆成 10 段只为提前几十毫秒看到 shutdown flag——
            // 那是 10 倍的定时器唤醒,常驻空转的代价远高于它买到的那点停机延迟。停机 join
            // 的上界因此变成「一次 drain + 一个 DRAIN_POLL」,仍远小于 CLI 的 10s 停机预算。
            thread::sleep(DRAIN_POLL);
        }
    });
    BridgeDrain { handle }
}

fn drain_once(store: &BridgeStore, outbox: &crate::runtime::outbox_store::OutboxStore) {
    match drain_tick(store) {
        Ok(stats) if stats != Default::default() => log(&format!(
            "bridge drain: applied={} duplicate={} stale={} conflict={} dead={}",
            stats.applied, stats.duplicate, stats.stale, stats.conflict, stats.dead
        )),
        Ok(_) => {}
        Err(error) => log(&summon_i18n::format!("r5b8bc0f75a")),
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64);
    match crate::bridge::reconcile::reconcile_once(store, outbox, now_ms) {
        Ok(stats) if stats != Default::default() => log(&format!(
            "bridge reconcile: expired={} audits={} turns={} contexts={} dead_drop={} sessions_pruned={}",
            stats.expired,
            stats.audits_enqueued,
            stats.turns_exported,
            stats.contexts_recovered,
            stats.dead_dropped,
            stats.sessions_pruned
        )),
        Ok(_) => {}
        Err(error) => log(&summon_i18n::format!(
            "rafba58f035"
        )),
    }
    match crate::bridge::handoff::coordinator_tick(store, outbox) {
        Ok(stats) if stats != Default::default() => log(&format!(
            "takeover coordinator: signals={} exits={} resumes={} resume_retries={} acks={} completed={} sentinels_released={}",
            stats.source_signals,
            stats.source_exits,
            stats.resumes_started,
            stats.resume_retries,
            stats.acks_verified,
            stats.completed,
            stats.sentinels_released
        )),
        Ok(_) => {}
        Err(error) => log(&summon_i18n::format!(
            "rf5f9103216"
        )),
    }
    // recall 历史全托管与 takeover coordinator 并列、互不影响:它只驱动自己的 recall_resume
    // attempt(起 staging 在飞书回调侧;这里只做 ACK→rename→绑群),不碰 handoff.rs 状态机。
    match crate::bridge::recall_resume::recall_resume_tick(store, outbox) {
        Ok(stats) if stats != Default::default() => log(&format!(
            "recall resume: acked={} bound={} failed={} reaped={}",
            stats.acked, stats.bound, stats.failed, stats.reaped
        )),
        Ok(_) => {}
        Err(error) => log(&summon_i18n::format!("rea49f6ecab")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_thread_observes_shutdown_and_is_joinable() {
        let shutdown = Arc::new(AtomicBool::new(true));
        let dir = tempfile::tempdir().unwrap();
        let drain = spawn_bridge_drain_with_stores(
            BridgeStore::new(dir.path().join("bridge")),
            crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox")),
            shutdown,
        );
        drain.join().unwrap();
    }

    #[test]
    fn drain_thread_exits_within_one_poll_after_shutdown_flips() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let dir = tempfile::tempdir().unwrap();
        let drain = spawn_bridge_drain_with_stores(
            BridgeStore::new(dir.path().join("bridge")),
            crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox")),
            Arc::clone(&shutdown),
        );
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let joiner = thread::spawn(move || {
            let _ = done_tx.send(drain.join().is_ok());
        });
        shutdown.store(true, Ordering::SeqCst);
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(2)),
            Ok(true),
            "drain 线程必须在一个 poll 周期内看到 shutdown 并退出"
        );
        joiner.join().unwrap();
    }

    #[test]
    fn binding_gate_does_not_block_hook_state_spool_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(BridgeStore::new(dir.path().join("bridge")));
        store.with_state(|_| Ok(())).unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let gate_entered = Arc::clone(&entered);
        let gate_release = Arc::clone(&release);
        let holder = std::thread::spawn(move || {
            with_chat_delivery_gate(
                &summon_core::bridge::ScopedChatKey::new("scope", "agent", "chat"),
                || {
                    gate_entered.wait();
                    gate_release.wait();
                    Ok(())
                },
            )
            .unwrap();
        });
        entered.wait();
        let writer_store = Arc::clone(&store);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            writer_store
                .with_state(|state| {
                    state.revision = state.revision.saturating_add(1);
                    Ok(())
                })
                .unwrap();
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("per-binding gate 不能占用 Hook/state flock");
        release.wait();
        holder.join().unwrap();
        writer.join().unwrap();
    }
}
