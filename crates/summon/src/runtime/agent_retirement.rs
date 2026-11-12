//! agent 后台写者的代际围栏。清理等待当前写者退出，旧线程不能写入后来同名的 agent。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

pub(crate) static EVENT_WRITERS: std::sync::RwLock<()> = std::sync::RwLock::new(());
pub(crate) static OUTBOX_WRITERS: Mutex<()> = Mutex::new(());
pub(crate) static COMMENT_WRITERS: Mutex<()> = Mutex::new(());

type Epoch = Arc<RwLock<bool>>;
static EPOCHS: LazyLock<Mutex<HashMap<PathBuf, Epoch>>> = LazyLock::new(Mutex::default);

pub(crate) fn activate(dir: &Path) -> Epoch {
    let epoch = Arc::new(RwLock::new(false));
    EPOCHS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(dir.to_path_buf(), epoch.clone());
    epoch
}

pub(crate) fn epoch(dir: &Path) -> Epoch {
    EPOCHS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(dir.to_path_buf())
        .or_insert_with(|| Arc::new(RwLock::new(false)))
        .clone()
}

pub(crate) fn retired(dir: &Path) -> bool {
    let epoch = EPOCHS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(dir)
        .cloned();
    epoch.is_some_and(|e| *e.read().unwrap_or_else(|e| e.into_inner()))
}

pub(crate) fn retire(dir: &Path) {
    let epoch = EPOCHS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(dir.to_path_buf())
        .or_insert_with(|| Arc::new(RwLock::new(false)))
        .clone();
    *epoch.write().unwrap_or_else(|e| e.into_inner()) = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retirement_waits_for_current_background_writer_to_finish() {
        let dir = tempfile::tempdir().unwrap();
        let epoch = activate(dir.path());
        let writer = epoch.read().unwrap();
        let path = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        let retired = std::thread::spawn(move || {
            retire(&path);
            tx.send(()).unwrap();
        });
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err());
        drop(writer);
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        retired.join().unwrap();
        assert!(*epoch.read().unwrap());
    }

    #[test]
    fn retired_writer_cannot_join_a_new_agent_generation() {
        let dir = tempfile::tempdir().unwrap();
        let old = activate(dir.path());
        retire(dir.path());
        let new = activate(dir.path());
        assert!(*old.read().unwrap());
        assert!(!*new.read().unwrap());
    }
}
