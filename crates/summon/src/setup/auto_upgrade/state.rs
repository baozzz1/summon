//! 自动升级的状态文件、退回名单与启动对账。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 状态文件名(在 `~/.summon/run/` 下)。
pub(super) const STATE_FILE: &str = "auto-upgrade.json";
/// 换代前留下的旧版三件套与构建戳(在 `~/.summon/run/` 下):退回时装回去,守望进程也从这里起。
const PREVIOUS_DIR: &str = "auto-upgrade-previous";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct State {
    /// 上次问发布源的时刻(unix 秒)。
    pub(crate) last_check_secs: Option<u64>,
    /// 上次问到的最新版。
    pub(crate) latest: Option<Seen>,
    /// 已取到本机、等整机空闲换代的版本。
    pub(crate) pending: Option<Pending>,
    /// 换代后退回过的版本:不再自动采用。
    pub(crate) rolled_back: Vec<String>,
    /// 当天不再试的版本(取不到、换不进落点、换代没开始)。
    pub(crate) held: Option<Held>,
    /// 最近一件事。
    pub(crate) last_event: Option<Event>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Seen {
    pub(crate) version: String,
    /// 发布时刻(unix 秒);发布源没给为空。
    pub(crate) published_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub(crate) version: String,
    /// 换代前在跑的版本,退回到它。
    pub(crate) previous: String,
    /// 取到本机的新版所在的暂存目录;新版留在别处(比如包管理器的全局目录)的渠道没有它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stage: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Held {
    pub(crate) version: String,
    /// 本地日期 `YYYY-MM-DD`。
    pub(crate) day: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Event {
    pub(crate) at_secs: u64,
    pub(crate) text: String,
    /// 同一件事连续出现的次数(见 [`State::note_repeated`]);只记一次的为 0。`at_secs` 是第一次的时刻。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) count: u32,
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

impl State {
    pub(crate) fn note(&mut self, now_secs: u64, text: impl Into<String>) {
        self.last_event = Some(Event {
            at_secs: now_secs,
            text: text.into(),
            count: 0,
        });
    }

    /// 同一件事接连出现(比如每次检查都取不到发布源):保留第一次的时刻,只把次数加一,不刷新。
    pub(crate) fn note_repeated(&mut self, now_secs: u64, text: impl Into<String>) {
        let text = text.into();
        match &mut self.last_event {
            Some(event) if event.text == text => event.count = event.count.max(1) + 1,
            _ => {
                self.last_event = Some(Event {
                    at_secs: now_secs,
                    text,
                    count: 1,
                })
            }
        }
    }

    pub(crate) fn hold(&mut self, version: &str, today: &str, reason: impl Into<String>) {
        self.held = Some(Held {
            version: version.to_string(),
            day: today.to_string(),
            reason: reason.into(),
        });
    }
}

/// 状态文件。daemon 与守望进程都会写(新版起来后两者可能同时在跑),改写一律在文件锁里读改写。
pub(crate) struct Store {
    path: PathBuf,
}

impl Store {
    pub(crate) fn at_home(home: &Path) -> Self {
        Self {
            path: run_dir(home).join(STATE_FILE),
        }
    }

    /// 没有文件是初始状态。读不出(损坏)报错:退回名单丢了就可能把退回过的版本再装一遍,
    /// 所以调用方这一轮什么都不做。
    pub(crate) fn load(&self) -> Result<State> {
        match std::fs::read(&self.path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .with_context(|| summon_i18n::format!("rc0e84e7578", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => {
                Err(error).with_context(|| summon_i18n::format!("re43971131a", self.path.display()))
            }
        }
    }

    pub(crate) fn update<T>(&self, change: impl FnOnce(&mut State) -> T) -> Result<T> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| summon_i18n::format!("rc18f37e5e1", dir.display()))?;
        }
        let lock_path = self.path.with_extension("lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| summon_i18n::format!("rf7f1f01aff", lock_path.display()))?;
        lock.lock()
            .with_context(|| summon_i18n::format!("r856098e50e", lock_path.display()))?;
        let mut state = self.load()?;
        let out = change(&mut state);
        let mut raw = serde_json::to_vec_pretty(&state)?;
        raw.push(b'\n');
        crate::runtime::fsio::atomic_write(&self.path, &raw)?;
        Ok(out)
    }
}

/// 运行期发布(启动时从别的安装源拉新、安装入口自发布)之前查:这一版换代后退回过,就不发布进
/// 落点。退回之后别的安装源里可能还留着这一版,不挡的话下一次启动会把它重新拉起来。状态文件读不出
/// 时不挡:自动升级那时已暂停、doctor 摆着,不让一份坏文件把所有升级都卡住。
pub(crate) fn refuses_publish(home: &Path, version: &str) -> Option<String> {
    let state = Store::at_home(home).load().ok()?;
    state
        .rolled_back
        .iter()
        .any(|v| v == version)
        .then(|| summon_i18n::format!("r16718f501d"))
}

/// 人明确要装这一版(`summon upgrade`):从退回名单里拿掉,之后的运行期发布与自动升级不再挡它。
/// 返回它原先在不在名单里。状态文件读不出时名单本就不挡发布,当作不在。
pub(crate) fn forget_rollback(home: &Path, version: &str) -> Result<bool> {
    let store = Store::at_home(home);
    let listed = store
        .load()
        .is_ok_and(|state| state.rolled_back.iter().any(|v| v == version));
    if !listed {
        return Ok(false);
    }
    store.update(|state| {
        state.rolled_back.retain(|v| v != version);
        true
    })
}

/// 人明确要装这一版:自动升级退回过它也照装,从退回名单里拿掉,运行期发布才不挡它。名单写不进去
/// 不挡升级,只说清后果。
pub(crate) fn lift_rollback(home: &Path, version: &str, out: &mut dyn Write) -> Result<()> {
    match forget_rollback(home, version) {
        Ok(true) => writeln!(out, "{}", summon_i18n::format!("re6b244a2bf"))?,
        Ok(false) => {}
        Err(error) => writeln!(out, "{}", summon_i18n::format!("r10d0236fb0"))?,
    }
    Ok(())
}

pub(super) fn run_dir(home: &Path) -> PathBuf {
    home.join(".summon/run")
}

pub(crate) fn previous_dir(home: &Path) -> PathBuf {
    run_dir(home).join(PREVIOUS_DIR)
}

/// 守望进程退回时为装回旧版另起的进程(自成一组)的 pid 记在这里。
pub(crate) fn restore_pid_path(home: &Path) -> PathBuf {
    run_dir(home).join("auto-upgrade.restore.pid")
}

/// 更早的版本给 [`restore_pid_path`] 用的文件名。只有那一版的守望进程与兜底进程读写它(它们都
/// 出自换代前那一版的同一个二进制),新版不认它;启动对账清痕迹时一并删掉。
pub(super) fn legacy_restore_pid_path(home: &Path) -> PathBuf {
    run_dir(home).join("auto-upgrade.npm.pid")
}

/// 启动对账的判定:改状态,返回这次换代留下的痕迹(守望进程 pid 文件、快照)能不能清掉。
///
/// `interrupted`:上一次换代起过守望进程、它已不在、快照还在。这时在跑的是换代前那一版,说明换代
/// 被中断、兜底进程放回了旧版 —— 记进退回名单,不再自动采用。
pub(super) fn reconcile_state(
    state: &mut State,
    running: &str,
    interrupted: bool,
    now_secs: u64,
) -> bool {
    let Some(pending) = state.pending.clone() else {
        return interrupted;
    };
    if pending.version == running {
        state.pending = None;
        state.note(now_secs, summon_i18n::format!("r842a5dd4ac"));
        return interrupted;
    }
    if interrupted && pending.previous == running {
        if !state.rolled_back.contains(&pending.version) {
            state.rolled_back.push(pending.version.clone());
        }
        state.pending = None;
        state.note(
            now_secs,
            summon_i18n::format!("r142e4f2270", pending.version),
        );
        return true;
    }
    false
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn today() -> String {
    chrono::Local::now().format("%F").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 2_000_000_000;

    #[test]
    fn the_state_file_round_trips_and_a_corrupt_one_pauses_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at_home(tmp.path());
        assert_eq!(store.load().unwrap(), State::default());
        store
            .update(|state| {
                state.rolled_back.push("0.6.35".into());
                state.note(7, "退回");
            })
            .unwrap();
        let state = store.load().unwrap();
        assert_eq!(state.rolled_back, ["0.6.35"]);
        assert_eq!(state.last_event.unwrap().text, "退回");
        std::fs::write(tmp.path().join(".summon/run").join(STATE_FILE), "{坏").unwrap();
        assert!(store.load().is_err());
        assert!(
            store.update(|_| ()).is_err(),
            "读不懂就不改写,退回名单不能被清掉"
        );
    }

    #[test]
    fn a_rolled_back_version_is_refused_until_someone_installs_it_by_hand() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(refuses_publish(tmp.path(), "0.6.35").is_none());
        assert!(!forget_rollback(tmp.path(), "0.6.35").unwrap());
        Store::at_home(tmp.path())
            .update(|state| state.rolled_back = vec!["0.6.35".into(), "0.6.36".into()])
            .unwrap();
        assert!(refuses_publish(tmp.path(), "0.6.35").is_some());
        assert!(refuses_publish(tmp.path(), "0.6.37").is_none());
        assert!(forget_rollback(tmp.path(), "0.6.35").unwrap());
        assert!(refuses_publish(tmp.path(), "0.6.35").is_none());
        assert_eq!(
            Store::at_home(tmp.path()).load().unwrap().rolled_back,
            ["0.6.36"]
        );
        // 状态文件读不出:不挡发布,也不让手动升级失败。
        std::fs::write(tmp.path().join(".summon/run").join(STATE_FILE), "{坏").unwrap();
        assert!(refuses_publish(tmp.path(), "0.6.36").is_none());
        assert!(!forget_rollback(tmp.path(), "0.6.36").unwrap());
    }

    fn swapping(previous: &str, version: &str) -> State {
        State {
            pending: Some(Pending {
                version: version.into(),
                previous: previous.into(),
                stage: None,
            }),
            ..State::default()
        }
    }

    /// 同一件事接连出现只计次、保留第一次的时刻;换了一件事就从头记。
    #[test]
    fn a_repeated_event_keeps_its_first_time_and_counts() {
        let mut state = State::default();
        state.note_repeated(10, "取不到");
        state.note_repeated(20, "取不到");
        state.note_repeated(30, "取不到");
        let event = state.last_event.clone().unwrap();
        assert_eq!((event.at_secs, event.count), (10, 3));
        state.note_repeated(40, "不成形");
        let event = state.last_event.clone().unwrap();
        assert_eq!(
            (event.at_secs, event.count, event.text.as_str()),
            (40, 1, "不成形")
        );
        state.note(50, "已装好");
        state.note_repeated(60, "已装好");
        assert_eq!(state.last_event.unwrap().count, 2);
    }

    /// 旧版本写下的状态文件(没有 `stage`、`count`)照样读得出;只取一次的记录写回时也不多出字段。
    #[test]
    fn state_files_from_earlier_versions_still_load() {
        let old = r#"{"pending":{"version":"0.6.35","previous":"0.6.34"},
            "last_event":{"at_secs":7,"text":"x"}}"#;
        let state: State = serde_json::from_str(old).unwrap();
        assert_eq!(state.pending.as_ref().unwrap().stage, None);
        assert_eq!(state.last_event.as_ref().unwrap().count, 0);
        let written = serde_json::to_string(&state).unwrap();
        assert!(
            !written.contains("stage") && !written.contains("count"),
            "{written}"
        );
    }

    /// 启动对账:待换代的那一版就是本代,换代成了;换代被中断、本代是换代前那一版,这一版记进退回
    /// 名单;没有中断的证据就什么都不动。
    #[test]
    fn startup_reconciliation_tells_a_swap_from_an_interrupted_one() {
        let mut done = swapping("0.6.34", "0.6.35");
        assert!(!reconcile_state(&mut done, "0.6.35", false, NOW));
        assert!(done.pending.is_none());
        assert!(done.rolled_back.is_empty());

        let mut interrupted = swapping("0.6.34", "0.6.35");
        assert!(reconcile_state(&mut interrupted, "0.6.34", true, NOW));
        assert!(interrupted.pending.is_none());
        assert_eq!(interrupted.rolled_back, ["0.6.35"]);

        let mut waiting = swapping("0.6.34", "0.6.35");
        assert!(!reconcile_state(&mut waiting, "0.6.34", false, NOW));
        assert_eq!(waiting, swapping("0.6.34", "0.6.35"), "还在等空闲换代,不动");

        let mut leftover = State::default();
        assert!(
            reconcile_state(&mut leftover, "0.6.34", true, NOW),
            "痕迹可以清"
        );
        assert_eq!(leftover, State::default());
    }
}
