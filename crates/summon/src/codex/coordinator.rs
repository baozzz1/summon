//! Codex 额度快照观测器。
//!
//! daemon 在 chat/task 热路径之外持续轮询 `$CODEX_HOME/sessions` rollout，把
//! `token_count` 行里的 rate_limits 快照喂给 rpc_pool 的账号额度存储，供菜单栏角标与额度卡片读取。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::codex_rollout::{
    CodexRolloutWatcher, CodexRolloutWatcherConfig, CodexRolloutWatcherEvent,
};
use crate::observe::log::log;

const LOOP_INTERVAL: Duration = Duration::from_millis(250);
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
const WATCH_LOOKBACK: Duration = Duration::from_secs(8 * 24 * 60 * 60);
const WATCH_MAX_FILES: usize = 2_048;

/// 启动额度观测线程。持续把 Codex rollout 里的最新额度快照喂给 rpc_pool，供角标/卡片读取；
/// shutdown 置位后收尾退出。
pub fn spawn(shutdown: Arc<AtomicBool>) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("quota-observe".into())
        .spawn(move || run(shutdown))
}

fn run(shutdown: Arc<AtomicBool>) {
    let (events, watcher) = spawn_codex_watcher(Arc::clone(&shutdown));
    while !shutdown.load(Ordering::Acquire) {
        match events.recv_timeout(LOOP_INTERVAL) {
            Ok(event) => handle_event(&event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if !shutdown.load(Ordering::Acquire) {
                    log(summon_i18n::text!("rb983506df0"));
                }
                break;
            }
        }
        for event in events.try_iter().take(512) {
            handle_event(&event);
        }
    }
    if watcher.join().is_err() {
        log(summon_i18n::text!("r324955ee47"));
    }
}

fn handle_event(event: &CodexRolloutWatcherEvent) {
    match event {
        CodexRolloutWatcherEvent::RateLimits(observation) => {
            crate::capture::rpc_pool::record_codex_quota_snapshot_at(
                None,
                &observation.snapshot,
                observation.observed_at,
            );
        }
        CodexRolloutWatcherEvent::Error { path, message } => {
            log(&summon_i18n::format!("r5911ea8148", path.display()));
        }
    }
}

fn spawn_codex_watcher(
    shutdown: Arc<AtomicBool>,
) -> (Receiver<CodexRolloutWatcherEvent>, JoinHandle<()>) {
    let sessions_dir = codex_home().join("sessions");
    CodexRolloutWatcher::new(CodexRolloutWatcherConfig {
        sessions_dir,
        lookback: WATCH_LOOKBACK,
        poll_interval: WATCH_INTERVAL,
        max_files_per_poll: WATCH_MAX_FILES,
    })
    .spawn_channel(shutdown)
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}
