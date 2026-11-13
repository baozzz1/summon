//! 自动升级的后台线程与 `summon doctor` 里的那一节。

use super::channel::{self, Channel, Fetched};
use super::decide::{check_due, decide, machine_quiet, IdleClock, Verdict};
use super::state::{
    legacy_restore_pid_paths, reconcile_state, today, unix_now, Pending, State, Store,
};
use super::swap::{attempt_restart, inbox_wal, settle, GuardFiles};
use crate::setup::upgrade_restart;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use summon_core::config::{Config, Upgrade, UpgradeRange};
use summon_core::install_stamp;

/// daemon 起来后多久做第一次检查:让启动期的补发、重注入先走完。
const FIRST_DELAY: Duration = Duration::from_secs(60);
/// 读配置、问状态的节拍。问发布源的间隔另由 `check_interval_hours` 管。
const TICK: Duration = Duration::from_secs(30);
/// 有待换代的版本时多久看一次整机空不空。
const QUIET_POLL: Duration = Duration::from_secs(2);

/// daemon 就绪后起。不 join:它可能正在取新版(有时限,但可能长达几分钟),停机不等它;
/// 它关着闸等停机时,进程退出前闸一直关着。
pub(crate) fn spawn(shutdown: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let Some(home) = dirs::home_dir() else {
            crate::observe::log::log(summon_i18n::text!("r3f9eccf1e9"));
            return;
        };
        Runner::new(home, crate::version::VERSION).run(&shutdown);
    });
}

/// 按此刻的本机与配置开一条渠道;生产上是 [`channel::open`],测试换成指定平台的。
pub(super) type Opener = Box<dyn Fn(&Path, &Upgrade) -> Result<Box<dyn Channel>, String> + Send>;

pub(super) struct Runner {
    home: PathBuf,
    open: Opener,
    /// 本代在跑的版本(生产上就是这份二进制的版本)。
    running: String,
    store: Store,
    log: PathBuf,
    idle: IdleClock,
    /// 最近一次读到的配置;配置每个 [`TICK`] 重读一次,看整机空不空时不重读。
    policy: Upgrade,
    /// 最近一次读到的待换代版本;有它时才高频看整机空不空。
    pending: Option<Pending>,
    /// 上一条记过日志的状态文件错误:同样的错误不反复写。
    load_error: Option<String>,
}

impl Runner {
    pub(super) fn new(home: PathBuf, running: &str) -> Self {
        Self {
            running: running.to_string(),
            open: Box::new(channel::open),
            store: Store::at_home(&home),
            log: upgrade_restart::log_path(&home),
            home,
            idle: IdleClock::default(),
            policy: Upgrade::default(),
            pending: None,
            load_error: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_opener(mut self, open: Opener) -> Self {
        self.open = open;
        self
    }

    fn run(&mut self, shutdown: &AtomicBool) {
        self.reconcile();
        let mut next_tick = Instant::now() + FIRST_DELAY;
        let mut next_quiet = Instant::now();
        while !shutdown.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= next_tick {
                self.tick();
                next_tick = now + TICK;
            }
            if self.pending.is_some() && now >= next_quiet {
                self.watch_quiet(shutdown);
                next_quiet = now + QUIET_POLL;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// 本代起来时对一遍账,见 [`reconcile_state`]。守望进程还在(新版正被观察)时不动它的痕迹。
    pub(super) fn reconcile(&self) {
        let files = GuardFiles::at_home(&self.home);
        let interrupted = files.watcher.exists()
            && files.snapshot.exists()
            && !super::swap::watcher_alive(&files.watcher);
        // 只在对账确实要改状态时写盘:自动升级默认关,daemon 每次启动都去建 run/、建锁文件、
        // 重写一遍状态文件是没来由的副作用。没有状态文件读出来就是初始状态,对账改不了它。
        let current = match self.store.load() {
            Ok(state) => state,
            Err(error) => {
                crate::observe::log::log(&summon_i18n::format!("r6fcf9e4c80"));
                return;
            }
        };
        let mut next = current.clone();
        let mut clear = reconcile_state(&mut next, &self.running, interrupted, unix_now());
        if next != current {
            // 读与写之间状态可能被守望进程改过:在锁里按那一刻的状态重新对一遍。
            let result = self.store.update(|state| {
                clear = reconcile_state(state, &self.running, interrupted, unix_now());
            });
            if let Err(error) = result {
                crate::observe::log::log(&summon_i18n::format!("r6fcf9e4c80"));
                return;
            }
        }
        if clear {
            // 删不掉留着无妨:下一次换代开始前会重做快照、重写 pid 文件。
            let _ = std::fs::remove_dir_all(&files.snapshot);
            let _ = std::fs::remove_file(&files.watcher);
            for legacy in legacy_restore_pid_paths(&self.home) {
                let _ = std::fs::remove_file(legacy);
            }
        } else if !files.snapshot.exists() && !super::swap::watcher_alive(&files.watcher) {
            // 上一次换代成了、守望进程已经走了,它的 pid 文件只是残留;不清掉,下一次换代尝试若在
            // 起守望进程之前留下了快照,两者一凑就像一次被中断的换代。
            let _ = std::fs::remove_file(&files.watcher);
        }
        self.sweep();
    }

    /// 清掉待换代记录不再引用的自动升级暂存。守望进程还在时不动:它换代后要拿暂存核对落点。
    /// 状态文件读不出时什么都不删:说不清哪个还在用。
    fn sweep(&self) {
        if super::swap::watcher_alive(&GuardFiles::at_home(&self.home).watcher) {
            return;
        }
        if let Ok(state) = self.store.load() {
            let keep = state.pending.and_then(|pending| pending.stage);
            let current = std::env::current_exe().ok();
            super::github::sweep_stages(&self.home, keep.as_deref(), current.as_deref());
        }
    }

    fn load(&mut self) -> Option<State> {
        match self.store.load() {
            Ok(state) => {
                self.load_error = None;
                Some(state)
            }
            Err(error) => {
                let text = format!("{error:#}");
                if self.load_error.as_deref() != Some(&text) {
                    crate::observe::log::log(&summon_i18n::format!("rf56ea71831"));
                }
                self.load_error = Some(text);
                None
            }
        }
    }

    fn tick(&mut self) {
        self.tick_with(Config::load().upgrade);
    }

    /// 一个节拍。自动升级关着时只读配置与状态、清掉没人用的暂存,不写盘、不建文件。
    pub(super) fn tick_with(&mut self, policy: Upgrade) {
        self.policy = policy.clone();
        let Some(state) = self.load() else {
            self.pending = None;
            return;
        };
        self.sweep();
        if !policy.auto {
            self.pending = None;
            self.idle.reset();
            return;
        }
        self.pending = state.pending.clone();
        if let Some(pending) = &state.pending {
            let newer = install_stamp::release_version(&pending.version)
                > install_stamp::release_version(&self.running);
            if !newer || state.rolled_back.contains(&pending.version) {
                let running = self.running.clone();
                self.drop_pending(summon_i18n::format!(
                    "r62fab9b6b0",
                    pending.version,
                    running
                ));
            }
            return;
        }
        let now_secs = unix_now();
        if check_due(&state, &policy, now_secs) {
            self.check_and_install(&policy, &state, now_secs);
        }
    }

    fn drop_pending(&mut self, why: String) {
        self.pending = None;
        self.idle.reset();
        let _ = self.store.update(|state| {
            state.pending = None;
            state.note(unix_now(), why);
        });
    }

    pub(super) fn check_and_install(&mut self, policy: &Upgrade, state: &State, now_secs: u64) {
        let mut channel = match (self.open)(&self.home, policy) {
            Ok(channel) => channel,
            Err(why) => {
                let _ = self.store.update(|state| {
                    state.last_check_secs = Some(now_secs);
                    state.note(now_secs, why);
                });
                return;
            }
        };
        let latest = match channel.latest() {
            Ok(latest) => latest,
            Err(failed) => {
                let _ = self.store.update(|state| {
                    state.last_check_secs = Some(now_secs);
                    if failed.repeats {
                        state.note_repeated(now_secs, failed.text);
                    } else {
                        state.note(now_secs, failed.text);
                    }
                });
                return;
            }
        };
        let today = today();
        let verdict = decide(&self.running, &latest, policy, state, now_secs, &today);
        let _ = self.store.update(|state| {
            state.last_check_secs = Some(now_secs);
            state.latest = Some(latest.clone());
            if let Verdict::Skip(why) = &verdict {
                state.note(now_secs, why.clone());
            }
        });
        if verdict != Verdict::Install {
            return;
        }
        let version = latest.version;
        match channel.platform_ready(&version) {
            Ok(true) => {}
            Ok(false) => {
                let _ = self.store.update(|state| {
                    state.note(
                        now_secs,
                        summon_i18n::format!("r814d4ff9f7", channel.platform()),
                    )
                });
                return;
            }
            Err(error) => {
                let _ = self
                    .store
                    .update(|state| state.note(now_secs, summon_i18n::format!("r38f6ea7d49")));
                return;
            }
        }
        // 另一次升级拿着锁:不等它,下次检查再看。
        let Ok(_lock) = upgrade_restart::lock(&self.home) else {
            return;
        };
        let outcome = channel.fetch(&version, &self.log);
        let _ = self.store.update(|state| match outcome {
            Ok(Fetched::Ready { stage }) => {
                state.pending = Some(Pending {
                    version: version.clone(),
                    previous: self.running.clone(),
                    stage: Some(stage),
                });
                state.note(
                    now_secs,
                    summon_i18n::format!("r1232c064a8", policy.idle_minutes),
                );
            }
            Ok(Fetched::InstallIsNewer) => {
                state.note(now_secs, summon_i18n::format!("r30a04dd861"));
            }
            Ok(Fetched::Blocked) => {
                state.hold(&version, &today, summon_i18n::text!("r4e5bb3b21c"));
                state.note(now_secs, summon_i18n::format!("r4af816c977"));
            }
            Ok(Fetched::UpToDate) => {
                state.note(now_secs, summon_i18n::format!("rd7d23ea2f8"));
            }
            Err(error) => {
                let reason = summon_i18n::format!("r7979f220ce");
                state.hold(&version, &today, reason.clone());
                state.note(now_secs, summon_i18n::format!("ra4951a0579"));
            }
        });
        self.pending = self.store.load().ok().and_then(|state| state.pending);
    }

    fn watch_quiet(&mut self, shutdown: &AtomicBool) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let now = SystemTime::now();
        let quiet = machine_quiet(&inbox_wal(&self.home), now).is_ok();
        let need = Duration::from_secs(self.policy.idle_minutes.saturating_mul(60));
        let last_activity = crate::observe::runtime_status::last_handling_change();
        if !self.policy.auto || !self.idle.tick(quiet, last_activity, now, need) {
            return;
        }
        self.idle.reset();
        let gate = &crate::runtime::admission::GATE;
        let attempt = attempt_restart(
            &self.home,
            &crate::version::build_info(),
            &self.policy,
            &pending,
            gate,
            shutdown,
            &self.log,
        );
        self.pending = settle(&self.store, &pending, attempt, unix_now(), &today());
    }
}

pub(crate) fn doctor_lines(cfg: &Config) -> Vec<String> {
    if !cfg.upgrade.auto {
        return vec![summon_i18n::text!("rf2bbf42711").to_string()];
    }
    let Some(home) = dirs::home_dir() else {
        return vec![summon_i18n::text!("r641fb3d16c").to_string()];
    };
    doctor_lines_at(cfg, &home)
}

pub(super) fn doctor_lines_at(cfg: &Config, home: &Path) -> Vec<String> {
    let policy = &cfg.upgrade;
    if !policy.auto {
        return vec![summon_i18n::text!("rf2bbf42711").to_string()];
    }
    let channel = match channel::open(home, policy) {
        Ok(channel) => channel,
        Err(why) => return vec![summon_i18n::format!("r95e651843d")],
    };
    let range = match policy.range {
        UpgradeRange::Minor => summon_i18n::text!("re17af74161"),
        UpgradeRange::Patch => summon_i18n::text!("r2d5262b4cb"),
    };
    let mut lines = vec![summon_i18n::format!(
        "re0f8c8520c",
        policy.check_interval_hours.max(1),
        policy.min_age_hours,
        policy.idle_minutes
    )];
    lines.extend(channel.doctor_notes());
    lines.extend(channel::retired_note(policy));
    let state = match Store::at_home(home).load() {
        Ok(state) => state,
        Err(error) => {
            lines.push(summon_i18n::format!("r5662fce304"));
            return lines;
        }
    };
    let at = |secs: u64| {
        chrono::DateTime::from_timestamp(secs as i64, 0)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%F %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| secs.to_string())
    };
    match (&state.last_check_secs, &state.latest) {
        (Some(when), Some(latest)) => lines.push(summon_i18n::format!(
            "rec8fc7add6",
            at(*when),
            latest.version
        )),
        (Some(when), None) => lines.push(summon_i18n::format!("r4439fa950d", at(*when))),
        (None, _) => lines.push(summon_i18n::text!("rf70ea64bee").to_string()),
    }
    if let Some(pending) = &state.pending {
        lines.push(summon_i18n::format!("r8918a68a21", pending.version));
    }
    if let Some(held) = state.held.as_ref().filter(|held| held.day == today()) {
        lines.push(summon_i18n::format!(
            "r5b388ab9a9",
            held.version,
            held.reason
        ));
    }
    if !state.rolled_back.is_empty() {
        lines.push(summon_i18n::format!(
            "r3d5c4c94a4",
            state.rolled_back.join("、")
        ));
    }
    if let Some(event) = &state.last_event {
        let count = event.count;
        lines.push(if count > 1 {
            summon_i18n::format!("r171b051383", at(event.at_secs), event.text)
        } else {
            summon_i18n::format!("re11ee70dfa", at(event.at_secs), event.text)
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::auto_upgrade::state::previous_dir;

    fn waiting() -> Pending {
        Pending {
            version: "0.6.35".into(),
            previous: "0.6.34".into(),
            stage: None,
        }
    }

    /// 上一次换代成了、守望进程已走:它留下的 pid 文件在启动对账时清掉;待换代的记录不受影响。
    #[test]
    fn a_leftover_watcher_pid_without_a_snapshot_is_cleared_at_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let store = Store::at_home(&home);
        store
            .update(|state| state.pending = Some(waiting()))
            .unwrap();
        let pid_file = GuardFiles::at_home(&home).watcher;
        std::fs::write(&pid_file, "999999999").unwrap();

        Runner::new(home.clone(), "0.6.34").reconcile();

        assert!(!pid_file.exists());
        let state = store.load().unwrap();
        assert_eq!(state.pending, Some(waiting()), "还在等空闲换代,不动");
        assert!(state.rolled_back.is_empty());
    }

    /// 快照也在、守望进程不在:那才是一次被中断的换代(旧版回来了),这一版记进退回名单。
    #[test]
    fn an_interrupted_swap_is_still_recognised_by_its_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let store = Store::at_home(&home);
        store
            .update(|state| state.pending = Some(waiting()))
            .unwrap();
        std::fs::write(GuardFiles::at_home(&home).watcher, "999999999").unwrap();
        std::fs::create_dir_all(previous_dir(&home)).unwrap();

        Runner::new(home.clone(), "0.6.34").reconcile();

        let state = store.load().unwrap();
        assert_eq!(state.rolled_back, ["0.6.35"]);
        assert!(!previous_dir(&home).exists());
    }

    /// 自动升级默认关:不写 `upgrade:` 就是关的,doctor 只报一句关闭。
    #[test]
    fn auto_upgrade_is_off_unless_configured() {
        assert!(!Upgrade::default().auto);
        let written: Upgrade = serde_yaml::from_str("check_interval_hours: 6\n").unwrap();
        assert!(!written.auto, "upgrade 段里没写 auto 也是关");
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let lines = doctor_lines_at(&cfg, tmp.path());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("upgrade.auto: false"), "{lines:?}");
    }

    /// 自动升级的共用代码不往飞书发东西、不碰 tmux、不发 SIGTERM:换代只经公开的 summon 命令,
    /// 结束的只有自己起的守望 / 兜底进程组(SIGKILL)。
    #[test]
    fn auto_upgrade_never_messages_touches_tmux_or_sends_sigterm() {
        for (name, source) in [
            ("mod.rs", include_str!("mod.rs")),
            ("channel.rs", include_str!("channel.rs")),
            ("decide.rs", include_str!("decide.rs")),
            ("github.rs", include_str!("github.rs")),
            ("runner.rs", include_str!("runner.rs")),
            ("state.rs", include_str!("state.rs")),
            ("swap.rs", include_str!("swap.rs")),
            ("watch.rs", include_str!("watch.rs")),
        ] {
            let code = source.split("\n#[cfg(test)]\nmod tests").next().unwrap();
            for line in code.lines().map(str::trim) {
                if line.starts_with("//") {
                    continue;
                }
                let lower = line.to_lowercase();
                for needle in ["feishu", "lark", "outbox", "tmux", "sigterm"] {
                    assert!(!lower.contains(needle), "{name}: {line}");
                }
            }
        }
    }

    /// 自动升级关着、机器上什么都没有:daemon 起来对账、跑几个节拍都不写盘、不建任何文件。
    #[test]
    fn a_fresh_machine_with_auto_upgrade_off_gets_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let mut runner = Runner::new(home.clone(), "0.6.34");
        runner.reconcile();
        for _ in 0..3 {
            runner.tick_with(Upgrade::default());
        }
        assert!(
            !home.join(".summon").exists(),
            "不建 run/、状态文件与锁文件"
        );
    }

    /// 状态文件在、对账改不了什么(还在等空闲换代):不重写它。
    #[test]
    fn a_state_file_reconciliation_does_not_change_is_left_untouched() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let store = Store::at_home(&home);
        store
            .update(|state| state.pending = Some(waiting()))
            .unwrap();
        let path = home.join(".summon/run/auto-upgrade.json");
        let before = (
            std::fs::read(&path).unwrap(),
            std::fs::metadata(&path).unwrap().ino(),
        );

        Runner::new(home.clone(), "0.6.34").reconcile();
        Runner::new(home.clone(), "0.6.34").tick_with(Upgrade::default());

        let after = (
            std::fs::read(&path).unwrap(),
            std::fs::metadata(&path).unwrap().ino(),
        );
        assert_eq!(after, before, "原子写会换 inode:没重写过");
    }
}
