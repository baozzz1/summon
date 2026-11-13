//! 一次换代:落点快照、兜底进程、关闸等本代进入停机、拉起守望进程,以及换代尝试之后的落账。

use super::channel::{self, Rechecked};
use super::decide::machine_quiet;
use super::state::{previous_dir, run_dir, Pending, Store};
use crate::runtime::admission::{Freeze, Gate};
use crate::runtime::inbox_wal::InboxWal;
use crate::setup::self_install;
use crate::setup::upgrade_restart;
use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use summon_core::companion;
use summon_core::config::Upgrade;
use summon_core::install_stamp;

/// 关闸后等本代进入停机的上限。
const STOP_LIMIT: Duration = Duration::from_secs(60);
/// 超时结束守望进程之后,再等多久确认本代没有恰在那一刻进入停机:SIGTERM 送达到信号处理里置位
/// 停机标志之间隔着一次线程调度。
const KILL_SETTLE: Duration = Duration::from_millis(500);

/// 守望进程命令行里认它的那一段。
const WATCHER_MARK: &str = "auto-upgrade-watch --entry";

/// 兜底进程要用到的几样:守望进程的 pid 文件,换代前的落点快照。
pub(super) struct GuardFiles {
    pub(super) watcher: PathBuf,
    pub(super) snapshot: PathBuf,
}

impl GuardFiles {
    pub(super) fn at_home(home: &Path) -> Self {
        Self {
            watcher: run_dir(home).join("auto-upgrade.watcher.pid"),
            snapshot: previous_dir(home),
        }
    }
}

/// pid 文件里的守望进程还活着:进程在,命令行也还是守望进程(pid 被复用了不算)。
pub(super) fn watcher_alive(pid_file: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
    else {
        return false;
    };
    if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
        return false;
    }
    Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(WATCHER_MARK))
}

/// 快照里的清单:做快照那一刻从落点拷进来的文件与各自的字节数,退回之前据它确认快照完整
/// (见 [`snapshot_ready`])。名字以点开头,兜底脚本按 `"$snapshot"/*` 放回文件时碰不到它。
const SNAPSHOT_LIST: &str = ".files";

/// 快照里可能有的文件:三件套与描述它们的构建戳。
fn snapshot_names() -> impl Iterator<Item = &'static str> {
    companion::TRIO
        .into_iter()
        .chain([install_stamp::STAMP_FILE])
}

/// 把落点里的三件套与构建戳硬链进 [`previous_dir`]:换代把落点里的文件 rename 掉之后,这里仍是
/// 旧的那几个文件。链不上(跨文件系统)就复制。落点里本来就没有的(比如没装 Menu Bar)不拷,
/// 拷了什么记进清单。
pub(crate) fn snapshot_install(home: &Path) -> Result<PathBuf> {
    let from = companion::install_dir(home);
    let to = previous_dir(home);
    if to.exists() {
        std::fs::remove_dir_all(&to)
            .with_context(|| summon_i18n::format!("rb75c37f79b", to.display()))?;
    }
    std::fs::create_dir_all(&to)
        .with_context(|| summon_i18n::format!("rc18f37e5e1", to.display()))?;
    for name in snapshot_names() {
        let src = from.join(name);
        if !src.is_file() {
            continue;
        }
        let dst = to.join(name);
        if std::fs::hard_link(&src, &dst).is_err() {
            std::fs::copy(&src, &dst).with_context(|| {
                summon_i18n::format!("rd08d8839d8", src.display(), dst.display())
            })?;
        }
    }
    anyhow::ensure!(
        to.join(companion::SUMMON).is_file(),
        summon_i18n::format!("rddfcf21691", from.display())
    );
    seal_snapshot(&to)?;
    Ok(to)
}

/// 给快照目录写清单:此刻目录里有哪些快照文件、各多少字节。
pub(crate) fn seal_snapshot(dir: &Path) -> Result<()> {
    let mut list = String::new();
    for name in snapshot_names() {
        if let Some(len) = file_len(&dir.join(name)) {
            list.push_str(&format!("{name}\t{len}\n"));
        }
    }
    let path = dir.join(SNAPSHOT_LIST);
    std::fs::write(&path, list).with_context(|| summon_i18n::format!("rc9d7c3c029", path.display()))
}

/// 普通文件的字节数;不在或不是普通文件为空。
fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
}

/// 快照能不能拿来退回:清单在,清单上的每个文件都在、字节数对得上,且有 summon。不能用时说明
/// 缺了什么 —— 拿残缺的快照退回,落点会新旧混合,或者干脆还是新版而记成「已退回」。
pub(crate) fn snapshot_ready(snapshot: &Path) -> Result<(), String> {
    let no_list = || summon_i18n::text!("re96dae316d").to_string();
    let list = std::fs::read_to_string(snapshot.join(SNAPSHOT_LIST)).map_err(|_| no_list())?;
    let mut has_summon = false;
    for line in list.lines().filter(|line| !line.is_empty()) {
        let (name, size) = line.split_once('\t').ok_or_else(no_list)?;
        let size: u64 = size.parse().map_err(|_| no_list())?;
        if !snapshot_names().any(|known| known == name) {
            return Err(no_list());
        }
        match file_len(&snapshot.join(name)) {
            None => return Err(summon_i18n::format!("r5cb41db903")),
            Some(len) if len != size => return Err(summon_i18n::format!("r75f765e5d0")),
            Some(_) => {}
        }
        has_summon |= name == companion::SUMMON;
    }
    if !has_summon {
        let name = companion::SUMMON;
        return Err(summon_i18n::format!("r5cb41db903"));
    }
    Ok(())
}

/// 守望进程的把手。
pub(crate) trait Watcher {
    /// 它已经结束了。
    fn exited(&mut self) -> bool;
    /// 连同它起的子进程一起结束掉。
    fn kill(&mut self);
}

impl Watcher for std::process::Child {
    fn exited(&mut self) -> bool {
        !matches!(self.try_wait(), Ok(None))
    }

    fn kill(&mut self) {
        // 守望进程是新会话的首进程,进程组号就是它的 pid;重启脚本与它起的 `daemon restart` 都在组里。
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.wait();
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Hold {
    /// 本代已进入停机:闸封到进程退出,兜底进程留着。
    Stopping,
    /// 本代已进入停机,而守望进程已经不在(超时那一刻被结束,或发出停机后自己结束了);兜底进程会在
    /// 本代退出后起 daemon。闸封到进程退出。
    StoppingUnattended,
    /// 守望进程没让本代停下就结束了:闸已打开。
    WatcherEnded,
    /// 时限内没进入停机:守望进程已结束,闸已打开。
    TimedOut,
}

/// 兜底进程的把手:本代退出后保证有 daemon 在跑。
pub(crate) trait Guardian {
    /// 本代没进入停机、用不着它了。
    fn cancel(&mut self);
}

impl Guardian for std::process::Child {
    fn cancel(&mut self) {
        // 兜底进程是新会话的首进程,连同它起的子进程整组结束。
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.wait();
    }
}

/// 关着闸等本代进入停机。兜底进程在守望进程之前就已起好,交进来。
///
/// - 本代进入停机:封闸,兜底进程留着:新一代起来它就退,守望进程起不来新一代时由它起。
/// - 守望进程结束或超时:超时先结束守望进程,再看一次停机标志 —— 重启段可能恰在那一刻发出了停机,
///   那就封闸、留着兜底进程;没在停机就撤掉兜底进程、开闸。
///
/// 结束守望进程之前兜底进程已经起好,所以闸至多关 `limit` 加 `settle`,不会碰上过闸方的等待上限。
pub(crate) fn hold_until_stopping(
    freeze: Freeze<'_>,
    watcher: &mut impl Watcher,
    guardian: &mut impl Guardian,
    shutdown: &AtomicBool,
    limit: Duration,
    poll: Duration,
    settle: Duration,
) -> Hold {
    let started = Instant::now();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            freeze.seal();
            return Hold::Stopping;
        }
        let ended = watcher.exited();
        if ended || started.elapsed() >= limit {
            if !ended {
                watcher.kill();
            }
            std::thread::sleep(settle);
            if shutdown.load(Ordering::SeqCst) {
                freeze.seal();
                return Hold::StoppingUnattended;
            }
            guardian.cancel();
            return if ended {
                Hold::WatcherEnded
            } else {
                Hold::TimedOut
            };
        }
        std::thread::sleep(poll);
    }
}

/// 兜底进程:本代一退出就保证有下一代。先等本代退出 —— 进程没了,或 `daemon status` 不再报它
/// (僵尸、pid 复用都卡不住)。然后守望进程还在就由它起新一代、兜底进程不动手,免得和它的退回争着
/// 起 daemon;过了 [`watcher_cap`] 它还在、新一代还没起来,就结束它整组(先确认组长仍是那个进程,
/// pid 被复用了就不动)。守望进程不在了,快照还在就先把换代前的落点放回去,再钉住落点起 daemon
/// (不从别的安装源拉),直到 `daemon status` 报出一个不是旧 pid 的 daemon;起不来就退避重试,不设
/// 上限。新一代起来就退。守望进程的 pid 文件读不到时当它还在,同样至多等 [`watcher_cap`]。认守望
/// 进程的命令行片段经环境变量交进来,不出现在自己的命令行里。参数:旧 daemon 的 pid、落点 summon、
/// 守望进程的 pid 文件、落点快照、看的间隔、首次重试间隔(整秒,翻倍到 60)、等守望进程的上限(秒)。
const GUARD_SCRIPT: &str = "r7d3ca328e7";

/// 兜底进程等守望进程起新一代的上限:盖住守望进程正常要做的全部 —— 观察新版,再留出几次停起
/// daemon 与重启 Menu Bar 的余量。过了它还没有新一代,就当守望进程卡住了。
fn watcher_cap() -> Duration {
    super::watch::OBSERVE + Duration::from_secs(10 * 60)
}

/// 生产上的兜底进程参数:每 0.5 秒看一次旧进程,重试间隔 3 秒起、翻倍到 60 秒。
fn guard_args() -> [String; 3] {
    [
        "0.5".to_string(),
        "3".to_string(),
        watcher_cap().as_secs().to_string(),
    ]
}

/// 起兜底进程,脱离本代会话。`old` 是要等它退出的那一代 daemon。日志打不开就不留输出,也要起。
fn spawn_guardian(
    home: &Path,
    log: &Path,
    old: u32,
    files: &GuardFiles,
    args: [String; 3],
) -> Result<std::process::Child> {
    let sink = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .ok();
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            ::summon_core::i18n::label(GUARD_SCRIPT),
            "summon-upgrade-guard",
        ])
        .arg(old.to_string())
        .arg(companion::install_target(home))
        .arg(&files.watcher)
        .arg(&files.snapshot)
        .args(args)
        .env("GUARD_WATCHER_MARK", WATCHER_MARK)
        .stdin(Stdio::null());
    match sink {
        Some(sink) => command.stdout(sink.try_clone()?).stderr(sink),
        None => command.stdout(Stdio::null()).stderr(Stdio::null()),
    };
    // SAFETY: fork 后只调用 async-signal-safe 的 setsid。
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    command.spawn().context(summon_i18n::text!("r506871f7ae"))
}

/// 换代没开始时删掉这次的落点快照(见 [`attempt_restart`])。
struct UnusedSnapshot<'a>(Option<&'a Path>);

impl Drop for UnusedSnapshot<'_> {
    fn drop(&mut self) {
        if let Some(snapshot) = self.0 {
            // 删不掉只是留着一份旧文件;启动对账只在守望进程的 pid 文件也在时才看它。
            let _ = std::fs::remove_dir_all(snapshot);
        }
    }
}

/// 一次换代尝试的结局。
#[derive(Debug)]
pub(super) enum Attempt {
    /// 另一次升级正拿着升级锁。
    Busy,
    /// 新版入口已不是待换的那一版(被手动动过),待换代记录作废。
    Stale(String),
    /// 关闸后确认不了空闲。
    NotQuiet(String),
    /// 换代后退回不到在跑的这一版(落点里的不是它),这一版今天不再试。
    Unsafe(String),
    Hold(Hold),
}

/// `running`:本代在跑的这份的构建(生产上就是这份二进制的),落点必须正是它。
pub(super) fn attempt_restart(
    home: &Path,
    running: &install_stamp::BuildInfo,
    policy: &Upgrade,
    pending: &Pending,
    gate: &Gate,
    shutdown: &AtomicBool,
    log: &Path,
) -> Result<Attempt> {
    let Ok(lock) = upgrade_restart::lock(home) else {
        return Ok(Attempt::Busy);
    };
    let channel = channel::open(home, policy).map_err(anyhow::Error::msg)?;
    // 新版入口此刻必须仍是待换的那一版:取到之后可能有人手动升级、降级或卸载过。
    let Rechecked { entry, watch_args } = match channel.recheck(pending)? {
        Ok(rechecked) => rechecked,
        Err(why) => return Ok(Attempt::Stale(why)),
    };
    // 退回靠落点快照、守望进程也从快照起,所以落点里必须正是在跑的这一份:换代前记下的旧版本号
    // 是本代的,放回的文件才对得上,快照里的二进制也才认得守望命令。
    let installed = self_install::installed_build(home);
    if install_stamp::relation(running, &installed) != install_stamp::Relation::Same {
        return Ok(Attempt::Unsafe(summon_i18n::format!(
            "rfc082eba7a",
            running.describe()
        )));
    }
    let previous = snapshot_install(home)?;
    // 守望进程起来之前的任何一步没走成(关闸后整机不空、起不来兜底进程……),这次的快照都删掉:
    // 留着它,配上上一次换代留下的守望进程 pid 文件,下一次启动对账会把它当成换代被中断的证据。
    let mut unused_snapshot = UnusedSnapshot(Some(&previous));

    let freeze = gate.freeze();
    if let Err(why) = machine_quiet(&inbox_wal(home), SystemTime::now()) {
        return Ok(Attempt::NotQuiet(why));
    }
    let mut sink = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| summon_i18n::format!("rad09049d43", log.display()))?;
    writeln!(
        sink,
        "{}",
        summon_i18n::format!(
            "r3dec847d48",
            chrono::Local::now().format("%F %T"),
            pending.previous,
            pending.version
        )
    )?;
    // 兜底进程起不来就不换代:没有它,结束守望进程就可能留下没有 daemon 的机器。
    let files = GuardFiles::at_home(home);
    // 上一次尝试留下的不作数;删不掉时兜底进程按「读不到」处理,至多等上限。
    let _ = std::fs::remove_file(&files.watcher);
    let mut guardian = spawn_guardian(home, log, std::process::id(), &files, guard_args())?;
    let mut command = Command::new(previous.join(companion::SUMMON));
    command
        .args(["auto-upgrade-watch", "--entry"])
        .arg(&entry)
        .args(["--version", &pending.version])
        .args(["--previous", &pending.previous])
        .args(watch_args);
    if upgrade_restart::menubar_installed() {
        command.arg("--menubar");
    }
    command.stdin(lock).stdout(sink.try_clone()?).stderr(sink);
    // SAFETY: fork 后只调用 async-signal-safe 的 setsid。
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    let mut watcher = match command.spawn() {
        Ok(watcher) => watcher,
        Err(error) => {
            guardian.cancel();
            return Err(error).context(summon_i18n::text!("rdc00f5eac4"));
        }
    };
    // 守望进程从快照起来了,快照从此归它与兜底进程。
    unused_snapshot.0 = None;
    // 写不成时兜底进程把守望进程当作还在,等满上限再自己起,不影响别的。
    let _ = crate::runtime::fsio::atomic_write(&files.watcher, watcher.id().to_string().as_bytes());
    let hold = hold_until_stopping(
        freeze,
        &mut watcher,
        &mut guardian,
        shutdown,
        STOP_LIMIT,
        Duration::from_millis(100),
        KILL_SETTLE,
    );
    if matches!(hold, Hold::TimedOut | Hold::WatcherEnded) {
        // 本代接着跑,守望进程已不在:它的 pid 文件与快照不再是这次换代的证据。
        let _ = std::fs::remove_file(&files.watcher);
        let _ = std::fs::remove_dir_all(&files.snapshot);
    }
    Ok(Attempt::Hold(hold))
}

/// 一次换代尝试之后记账,返回此后内存里该挂着的待换代版本。凡是没换成、又不是一时不空的结局,
/// 这一版当天都不再试 —— 否则每攒满一次空闲就关一次闸、挡一次所有会话,永远循环。
pub(super) fn settle(
    store: &Store,
    pending: &Pending,
    attempt: Result<Attempt>,
    now_secs: u64,
    today: &str,
) -> Option<Pending> {
    let version = &pending.version;
    let hold = |reason: String| -> Option<Pending> {
        let _ = store.update(|state| {
            state.hold(version, today, reason.clone());
            state.pending = None;
            state.note(now_secs, summon_i18n::format!("ra4951a0579"));
        });
        None
    };
    match attempt {
        Ok(Attempt::Busy) => Some(pending.clone()),
        Ok(Attempt::NotQuiet(why)) => {
            crate::observe::log::log(&summon_i18n::format!("r542beddc71"));
            Some(pending.clone())
        }
        Ok(Attempt::Stale(why)) => {
            let _ = store.update(|state| {
                state.pending = None;
                state.note(now_secs, why);
            });
            None
        }
        Ok(Attempt::Unsafe(why)) => hold(why),
        Ok(Attempt::Hold(Hold::Stopping)) => {
            crate::observe::log::log(&summon_i18n::format!("r902a005e10"));
            None
        }
        Ok(Attempt::Hold(Hold::StoppingUnattended)) => {
            hold(summon_i18n::text!("re10e091086").to_string())
        }
        Ok(Attempt::Hold(Hold::WatcherEnded)) => {
            // 守望进程退回过就已撤下待换代记录;还挂着,说明它没换代就结束了。
            let still = store
                .load()
                .ok()
                .and_then(|state| state.pending)
                .is_some_and(|p| &p.version == version);
            if still {
                hold(summon_i18n::text!("rde5b66edc0").to_string())
            } else {
                None
            }
        }
        Ok(Attempt::Hold(Hold::TimedOut)) => hold(summon_i18n::text!("r22df7c7768").to_string()),
        Err(error) => hold(summon_i18n::format!("r3ee28bbd2d")),
    }
}

pub(super) fn inbox_wal(home: &Path) -> InboxWal {
    InboxWal::new(run_dir(home).join("inbox"))
}

#[cfg(test)]
mod tests {
    use super::super::decide::{decide, Verdict};
    use super::super::state::Seen;
    use super::*;
    use std::process::ExitStatus;
    use summon_core::config::Upgrade;

    const NOW: u64 = 2_000_000_000;
    const TODAY: &str = "2033-05-18";

    fn skipped(v: Verdict, needle: &str) {
        match v {
            Verdict::Skip(why) => assert!(why.contains(needle), "{why}"),
            Verdict::Install => panic!("不该装(期望:{needle})"),
        }
    }

    fn seen(version: &str, age_hours: u64) -> Seen {
        Seen {
            version: version.to_string(),
            published_secs: Some(NOW - age_hours * 3600),
        }
    }

    #[derive(Default)]
    struct FakeWatcher<'a> {
        exits_after: Option<usize>,
        polls: usize,
        killed_while_frozen: Option<bool>,
        gate: Option<&'static Gate>,
        /// 被结束的那一刻本代恰好进入停机:重启段在被结束之前已经发出了停机。
        stops_daemon_as_killed: Option<&'a AtomicBool>,
        /// 结束时本代已进入停机:守望进程发出停机后自己结束了。
        stops_daemon_as_it_ends: Option<&'a AtomicBool>,
        killed: bool,
    }

    #[derive(Default)]
    struct FakeGuardian<'a> {
        cancelled: Option<&'a std::cell::Cell<bool>>,
    }

    impl Guardian for FakeGuardian<'_> {
        fn cancel(&mut self) {
            if let Some(cancelled) = self.cancelled {
                cancelled.set(true);
            }
        }
    }

    impl Watcher for FakeWatcher<'_> {
        fn exited(&mut self) -> bool {
            self.polls += 1;
            let ended = self.exits_after.is_some_and(|n| self.polls > n);
            if let (true, Some(shutdown)) = (ended, self.stops_daemon_as_it_ends) {
                shutdown.store(true, Ordering::SeqCst);
            }
            ended
        }

        fn kill(&mut self) {
            self.killed = true;
            if let Some(gate) = self.gate {
                self.killed_while_frozen = Some(!gate.is_open());
            }
            if let Some(shutdown) = self.stops_daemon_as_killed {
                shutdown.store(true, Ordering::SeqCst);
            }
        }
    }

    fn leaked_gate() -> &'static Gate {
        Box::leak(Box::new(Gate::new()))
    }

    fn hold(
        gate: &'static Gate,
        watcher: &mut FakeWatcher<'_>,
        cancelled: &std::cell::Cell<bool>,
        shutdown: &AtomicBool,
        limit: Duration,
    ) -> Hold {
        hold_until_stopping(
            gate.freeze(),
            watcher,
            &mut FakeGuardian {
                cancelled: Some(cancelled),
            },
            shutdown,
            limit,
            Duration::from_millis(10),
            Duration::from_millis(10),
        )
    }

    #[test]
    fn a_daemon_that_starts_stopping_keeps_the_gate_sealed_and_the_guardian() {
        let gate = leaked_gate();
        let shutdown = AtomicBool::new(false);
        let cancelled = std::cell::Cell::new(false);
        let mut watcher = FakeWatcher::default();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                shutdown.store(true, Ordering::SeqCst);
            });
            let held = hold(
                gate,
                &mut watcher,
                &cancelled,
                &shutdown,
                Duration::from_secs(5),
            );
            assert_eq!(held, Hold::Stopping);
        });
        assert!(!gate.is_open(), "停机开始后闸封到进程退出");
        assert!(
            !cancelled.get(),
            "兜底进程留着,守望进程起不来新一代时由它起"
        );
        assert!(!watcher.killed);
    }

    #[test]
    fn a_watcher_that_ends_on_its_own_reopens_the_gate_and_drops_the_guardian() {
        let gate = leaked_gate();
        let cancelled = std::cell::Cell::new(false);
        let mut watcher = FakeWatcher {
            exits_after: Some(2),
            ..FakeWatcher::default()
        };
        let held = hold(
            gate,
            &mut watcher,
            &cancelled,
            &AtomicBool::new(false),
            Duration::from_secs(5),
        );
        assert_eq!(held, Hold::WatcherEnded);
        assert!(cancelled.get(), "本代还在跑,用不着兜底");
        assert!(gate.is_open());
    }

    /// 守望进程发出停机后自己结束了:按停机处理,兜底进程留着。
    #[test]
    fn a_watcher_that_ends_after_stopping_the_daemon_leaves_the_guardian() {
        let gate = leaked_gate();
        let shutdown = AtomicBool::new(false);
        let cancelled = std::cell::Cell::new(false);
        let mut watcher = FakeWatcher {
            exits_after: Some(0),
            stops_daemon_as_it_ends: Some(&shutdown),
            ..FakeWatcher::default()
        };
        let held = hold(
            gate,
            &mut watcher,
            &cancelled,
            &shutdown,
            Duration::from_secs(5),
        );
        assert_eq!(held, Hold::StoppingUnattended);
        assert!(!cancelled.get());
        assert!(!gate.is_open());
    }

    /// 时限内没进入停机:结束守望进程,再开闸 —— 反过来的话开闸后它仍可能把 daemon 停掉。本代
    /// 没在停机,兜底进程撤掉。
    #[test]
    fn a_restart_that_never_begins_is_killed_before_the_gate_reopens() {
        let gate = leaked_gate();
        let cancelled = std::cell::Cell::new(false);
        let mut watcher = FakeWatcher {
            gate: Some(gate),
            ..FakeWatcher::default()
        };
        let held = hold(
            gate,
            &mut watcher,
            &cancelled,
            &AtomicBool::new(false),
            Duration::from_millis(100),
        );
        assert_eq!(held, Hold::TimedOut);
        assert_eq!(watcher.killed_while_frozen, Some(true));
        assert!(cancelled.get(), "本代没在停机,兜底进程撤掉");
        assert!(gate.is_open());
    }

    /// 超时那一刻重启段已发出停机:结束它之后再看一次标志,按「停机中、守望进程不在」处理,
    /// 闸封住、兜底进程留着。
    #[test]
    fn a_stop_that_lands_as_the_watcher_is_killed_is_not_mistaken_for_a_timeout() {
        let gate = leaked_gate();
        let shutdown = AtomicBool::new(false);
        let cancelled = std::cell::Cell::new(false);
        let mut watcher = FakeWatcher {
            stops_daemon_as_killed: Some(&shutdown),
            ..FakeWatcher::default()
        };
        let held = hold(
            gate,
            &mut watcher,
            &cancelled,
            &shutdown,
            Duration::from_millis(50),
        );
        assert_eq!(held, Hold::StoppingUnattended);
        assert!(!cancelled.get(), "兜底进程留着,本代退出后由它起 daemon");
        assert!(!gate.is_open(), "本代在停机,闸封到进程退出");
    }

    #[test]
    fn the_install_snapshot_keeps_the_old_files_after_they_are_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let bin = companion::install_dir(home);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(companion::SUMMON), "old summon").unwrap();
        std::fs::write(bin.join(companion::SIDECAR), "old sidecar").unwrap();
        std::fs::write(bin.join(install_stamp::STAMP_FILE), "old stamp").unwrap();
        let snapshot = snapshot_install(home).unwrap();
        // 换代按暂存 + rename 换掉落点里的文件。
        for name in [companion::SUMMON, companion::SIDECAR] {
            let staged = bin.join(format!(".{name}.new"));
            std::fs::write(&staged, "new").unwrap();
            std::fs::rename(&staged, bin.join(name)).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(snapshot.join(companion::SUMMON)).unwrap(),
            "old summon"
        );
        assert_eq!(
            std::fs::read_to_string(snapshot.join(install_stamp::STAMP_FILE)).unwrap(),
            "old stamp"
        );
        assert!(!snapshot.join(companion::MENUBAR).exists());
        // 再做一次快照不留上一次的东西。
        std::fs::remove_file(bin.join(install_stamp::STAMP_FILE)).unwrap();
        let again = snapshot_install(home).unwrap();
        assert!(!again.join(install_stamp::STAMP_FILE).exists());

        std::fs::remove_file(bin.join(companion::SUMMON)).unwrap();
        assert!(
            snapshot_install(home).is_err(),
            "落点没有 summon 就无从退回"
        );
    }

    fn pending() -> Pending {
        Pending {
            version: "0.6.35".into(),
            previous: "0.6.34".into(),
            stage: None,
        }
    }

    fn store_with_pending() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at_home(tmp.path());
        store
            .update(|state| state.pending = Some(pending()))
            .unwrap();
        (tmp, store)
    }

    /// 守望进程没换代就结束了(重启脚本起不来、快照里的二进制不认守望命令……):这一版当天不再试,
    /// 否则每攒满一次空闲就关一次闸、挡一次所有会话,永远循环。
    #[test]
    fn a_watcher_that_ends_before_the_swap_is_not_retried_the_same_day() {
        let (_tmp, store) = store_with_pending();
        let next = settle(
            &store,
            &pending(),
            Ok(Attempt::Hold(Hold::WatcherEnded)),
            NOW,
            TODAY,
        );
        assert_eq!(next, None, "不再挂着待换代");
        let state = store.load().unwrap();
        assert!(state.pending.is_none());
        assert!(state
            .last_event
            .as_ref()
            .is_some_and(|event| event.text.contains("今天不再试")));
        skipped(
            decide(
                "0.6.34",
                &seen("0.6.35", 9),
                &Upgrade::default(),
                &state,
                NOW,
                TODAY,
            ),
            "今天不再试",
        );

        // 守望进程退回过就已撤下待换代记录:不另记。
        let (_tmp, store) = store_with_pending();
        store.update(|state| state.pending = None).unwrap();
        let next = settle(
            &store,
            &pending(),
            Ok(Attempt::Hold(Hold::WatcherEnded)),
            NOW,
            TODAY,
        );
        assert_eq!(next, None);
        assert!(store.load().unwrap().held.is_none());
    }

    #[test]
    fn each_restart_outcome_is_accounted_for() {
        let held = |attempt: Result<Attempt>| {
            let (_tmp, store) = store_with_pending();
            let next = settle(&store, &pending(), attempt, NOW, TODAY);
            let state = store.load().unwrap();
            (
                next,
                state
                    .held
                    .is_some_and(|h| h.version == "0.6.35" && h.day == TODAY),
                state.pending,
            )
        };
        // 一时不空、锁被占:接着等。
        for attempt in [Ok(Attempt::Busy), Ok(Attempt::NotQuiet("在跑一轮".into()))] {
            let (next, is_held, stored) = held(attempt);
            assert_eq!(next, Some(pending()));
            assert!(!is_held);
            assert_eq!(stored, Some(pending()));
        }
        // 新版入口被人动过:作废,不罚。
        let (next, is_held, stored) = held(Ok(Attempt::Stale("暂存里已是 0.6.36".into())));
        assert_eq!((next, is_held, stored), (None, false, None));
        // 其余没换成的结局:当天不再试。
        for attempt in [
            Ok(Attempt::Unsafe("落点里不是在跑的这一份".into())),
            Ok(Attempt::Hold(Hold::TimedOut)),
            Ok(Attempt::Hold(Hold::StoppingUnattended)),
            Err(anyhow::anyhow!("起守望进程失败")),
        ] {
            let (next, is_held, stored) = held(attempt);
            assert_eq!((next, is_held, stored), (None, true, None));
        }
        // 换代开始了:本代停机,不记罚。
        let (next, is_held, _) = held(Ok(Attempt::Hold(Hold::Stopping)));
        assert_eq!((next, is_held), (None, false));
    }

    /// 假落点 summon:`daemon status` 在 `old-alive` 在时报旧 daemon(pid 取自 `old-pid`),
    /// `up` 在时报新起的那个,否则报未运行;`daemon start` 记账(连同钉住),第 2 次起才起得来。
    const FAKE_GUARDED_INSTALL: &str = r#"#!/bin/sh
here="$(cd "$(dirname "$0")" && pwd)"
case "$1 ${2:-}" in
  "daemon status")
    if [ -e "$here/old-alive" ]; then echo "running(pid $(cat "$here/old-pid"), version 0.6.34)"
    elif [ -e "$here/up" ]; then echo "running(pid 222, version 0.6.34)"
    else echo "daemon 未运行"; fi ;;
  "daemon start")
    echo start >> "$here/starts"
    if [ "$(wc -l < "$here/starts")" -ge 2 ]; then touch "$here/up"; exit 0; fi
    exit 1 ;;
esac
"#;

    /// 兜底进程先于守望进程起,起不来就不换代。
    #[test]
    fn the_guardian_is_spawned_before_the_watcher() {
        let source = include_str!("swap.rs");
        let body = &source[source.find("fn attempt_restart(").unwrap()..];
        let body = &body[..body
            .find(
                "
}
",
            )
            .unwrap()];
        let guardian = body.find("spawn_guardian(").expect("换代要起兜底进程");
        let watcher = body.find("command.spawn()").expect("换代要起守望进程");
        assert!(guardian < watcher);
        assert!(
            body[guardian..watcher].contains(")?;"),
            "兜底进程起不来就不换代"
        );
    }

    /// 起一个兜底进程。旧 daemon 与守望进程各用一个真子进程代表;落点里的构建戳是新版的,快照里是
    /// 旧版的,看兜底进程有没有把它放回去。
    fn guardian_fixture(
        watcher_pid: Option<u32>,
        cap: &str,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        std::process::Child,
        std::process::Child,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let install = companion::install_target(&home);
        std::fs::create_dir_all(install.parent().unwrap()).unwrap();
        crate::test_support::write_test_executable(&install, FAKE_GUARDED_INSTALL);
        let bin = companion::install_dir(&home);
        std::fs::write(bin.join(install_stamp::STAMP_FILE), "new stamp").unwrap();
        let old = Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::write(bin.join("old-pid"), old.id().to_string()).unwrap();
        std::fs::write(bin.join("old-alive"), "").unwrap();
        let files = GuardFiles {
            watcher: home.join("watcher.pid"),
            snapshot: home.join("snapshot"),
        };
        std::fs::create_dir_all(&files.snapshot).unwrap();
        std::fs::write(files.snapshot.join(install_stamp::STAMP_FILE), "old stamp").unwrap();
        if let Some(pid) = watcher_pid {
            std::fs::write(&files.watcher, pid.to_string()).unwrap();
        }
        let unopenable_log = home.join("no-such-dir/upgrade.log");
        let guardian = spawn_guardian(
            &home,
            &unopenable_log,
            old.id(),
            &files,
            ["0.05".to_string(), "0".to_string(), cap.to_string()],
        )
        .unwrap();
        (tmp, bin, old, guardian)
    }

    fn stamp(bin: &Path) -> String {
        std::fs::read_to_string(bin.join(install_stamp::STAMP_FILE)).unwrap()
    }

    /// 自成一组、命令行里带给定片段的进程,代表守望进程。
    fn group_leader(args: &[&str]) -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        Command::new("/bin/sh")
            .args(["-c", "sleep 30; :"])
            .args(args)
            .process_group(0)
            .spawn()
            .unwrap()
    }

    fn ended(child: &mut std::process::Child) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    fn reap(child: &mut std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn wait_for_exit(guardian: &mut std::process::Child) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = guardian.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "兜底进程没结束");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 兜底进程:旧 daemon 在就等;退了之后守望进程还在就让它去起;守望进程也不在了才自己起,起不来
    /// 就重试、钉住落点,直到 status 报出新一代。日志打不开也照样起。
    #[test]
    fn the_guardian_starts_a_daemon_once_the_old_one_and_the_watcher_are_gone() {
        let mut watcher = Command::new("sleep").arg("30").spawn().unwrap();
        let (_tmp, bin, mut old, mut guardian) = guardian_fixture(Some(watcher.id()), "30");
        std::thread::sleep(Duration::from_millis(500));
        assert!(!bin.join("starts").exists(), "旧 daemon 还在,不该起");
        // 旧进程还在、status 已不报它(停机时先撤 pid 文件):轮到守望进程,它还在就不起。
        std::fs::remove_file(bin.join("old-alive")).unwrap();
        reap(&mut old);
        std::thread::sleep(Duration::from_millis(3000));
        assert!(!bin.join("starts").exists(), "守望进程还在,让它去起");
        assert!(guardian.try_wait().unwrap().is_none());
        reap(&mut watcher);
        assert!(wait_for_exit(&mut guardian).success());
        assert_eq!(
            std::fs::read_to_string(bin.join("starts")).unwrap(),
            "start\nstart\n",
            "起不来就重试"
        );
        assert!(bin.join("up").exists());
        assert_eq!(stamp(&bin), "old stamp", "新一代没起来,先放回换代前的落点");
    }

    /// 守望进程过了上限还在、新一代没起来:兜底进程结束它整组,再自己起。
    #[test]
    fn a_watcher_stuck_past_the_cap_is_ended_and_the_guardian_starts_a_daemon() {
        let mut watcher = group_leader(&["auto-upgrade-watch", "--entry", "x"]);
        let (_tmp, bin, mut old, mut guardian) = guardian_fixture(Some(watcher.id()), "1");
        std::fs::remove_file(bin.join("old-alive")).unwrap();
        reap(&mut old);
        assert!(wait_for_exit(&mut guardian).success());
        assert!(bin.join("up").exists(), "过了上限自己起");
        assert_eq!(stamp(&bin), "old stamp", "起之前先放回换代前的落点");
        assert!(ended(&mut watcher), "卡住的守望进程应被结束");
    }

    /// pid 文件里的进程已经不是守望进程(pid 被复用):过了上限照样自己起,但不结束那个进程。
    #[test]
    fn a_reused_watcher_pid_is_never_killed() {
        use std::os::unix::process::CommandExt as _;
        let mut stranger = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let (_tmp, bin, mut old, mut guardian) = guardian_fixture(Some(stranger.id()), "1");
        std::fs::remove_file(bin.join("old-alive")).unwrap();
        reap(&mut old);
        assert!(wait_for_exit(&mut guardian).success());
        assert!(bin.join("up").exists());
        assert!(
            stranger.try_wait().unwrap().is_none(),
            "不是守望进程,不动它"
        );
        reap(&mut stranger);
    }

    /// 读不到守望进程的 pid 文件时当它还在,同样至多等上限:过了就自己起。
    #[test]
    fn the_guardian_waits_for_an_unknown_watcher_only_up_to_the_cap() {
        let (_tmp, bin, mut old, mut guardian) = guardian_fixture(None, "1");
        std::fs::remove_file(bin.join("old-alive")).unwrap();
        reap(&mut old);
        assert!(wait_for_exit(&mut guardian).success());
        assert!(bin.join("up").exists(), "上限过了自己起");
    }

    /// 守望进程起来了新一代:兜底进程什么都不做就退。
    #[test]
    fn the_guardian_leaves_once_a_new_daemon_is_running() {
        // 新一代起来了:落点照旧是新版,不放回。
        let mut watcher = Command::new("sleep").arg("30").spawn().unwrap();
        let (_tmp, bin, mut old, mut guardian) = guardian_fixture(Some(watcher.id()), "30");
        std::fs::remove_file(bin.join("old-alive")).unwrap();
        reap(&mut old);
        std::fs::write(bin.join("up"), "").unwrap();
        assert!(wait_for_exit(&mut guardian).success());
        assert!(!bin.join("starts").exists(), "新一代已在跑,不再起");
        assert_eq!(stamp(&bin), "new stamp");
        reap(&mut watcher);
    }

    /// 一行 shell 里 `echo "…"` 之外的部分,与 echo 文案里引用的变量。
    fn shell_shape(line: &str) -> (String, Vec<String>) {
        let Some((head, rest)) = line.split_once("echo \"") else {
            return (line.to_string(), Vec::new());
        };
        let (message, tail) = rest.rsplit_once('"').unwrap_or((rest, ""));
        let mut vars = Vec::new();
        for (at, c) in message.char_indices() {
            if c != '$' {
                continue;
            }
            let rest = &message[at + 1..];
            let name: String = if let Some(braced) = rest.strip_prefix('{') {
                braced.split('}').next().unwrap_or("").to_string()
            } else {
                rest.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect()
            };
            vars.push(name);
        }
        vars.sort();
        (format!("{head}echo \"…\"{tail}"), vars)
    }

    /// 存在 locale 里的 shell 脚本,中英两份只许 echo 的文案不同:代码逐行相同,文案引用的变量
    /// 也相同。改脚本时漏改一份,换代 / 兜底就会按 daemon 的语言走出两种行为。
    #[test]
    fn script_resources_differ_only_in_echoed_text_between_languages() {
        use summon_core::i18n::{label_in, Language};
        for key in [GUARD_SCRIPT, upgrade_restart::RESTART_SCRIPT] {
            let zh = label_in(Language::Chinese, key);
            let en = label_in(Language::English, key);
            assert_ne!(zh, key, "{key} 不在目录里");
            assert_eq!(zh.lines().count(), en.lines().count(), "{key}");
            for (a, b) in zh.lines().zip(en.lines()) {
                assert_eq!(shell_shape(a), shell_shape(b), "{key}:\n{a}\n{b}");
            }
        }
    }

    /// 兜底脚本与 Rust 这一侧说的是同一组参数:位置参数的个数与 `spawn_guardian` 传的一致,认守望
    /// 进程的命令行片段经环境变量交进来;退回只靠快照,脚本里没有任何包管理器。
    #[test]
    fn the_guard_script_uses_the_names_rust_passes() {
        let script = summon_core::i18n::label(GUARD_SCRIPT);
        assert!(script.contains("\"$GUARD_WATCHER_MARK\""), "{script}");
        assert!(
            !script.contains("$8") && script.contains("cap=\"$7\""),
            "{script}"
        );
        assert!(!script.to_lowercase().contains("npm"), "{script}");
        assert!(!script.contains("install -g"), "{script}");
        let source = include_str!("swap.rs");
        let spawn = &source[source.find("fn spawn_guardian(").unwrap()..];
        assert!(spawn.contains(".env(\"GUARD_WATCHER_MARK\", WATCHER_MARK)"));
    }

    /// 一次换代尝试要的本机:落点里正是在跑的这一份(fixture 的 0.6.9),暂存里是待换的 0.7.0。
    /// 本平台不在发布范围内时打不开渠道,返回空。
    struct AttemptSetup {
        fixture: crate::setup::github_upgrade::tests::Fixture,
        running: install_stamp::BuildInfo,
        pending: Pending,
        policy: Upgrade,
    }

    fn attempt_setup() -> Option<AttemptSetup> {
        use crate::setup::github_upgrade::tests::{self as gh, Fixture};
        summon_core::release::platform(std::env::consts::OS, std::env::consts::ARCH)?;
        let fixture = Fixture::new();
        let home = &fixture.home;
        let running = gh::old_build();
        let stage = home.join(".summon/run/auto-upgrade-stage.t");
        std::fs::create_dir_all(stage.join("pkg")).unwrap();
        crate::test_support::write_test_executable(&stage.join("pkg/summon"), gh::FAKE_SUMMON);
        let pending = Pending {
            version: "0.7.0".into(),
            previous: running.version.clone(),
            stage: Some(stage),
        };
        let policy = Upgrade {
            auto: true,
            channel: Some("github".into()),
            from: Some(fixture.release.display().to_string()),
            ..Upgrade::default()
        };
        Some(AttemptSetup {
            fixture,
            running,
            pending,
            policy,
        })
    }

    fn try_attempt(
        setup: &AttemptSetup,
        stopping: bool,
        log: &Path,
    ) -> (Result<Attempt>, &'static Gate) {
        let gate = leaked_gate();
        let attempt = attempt_restart(
            &setup.fixture.home,
            &setup.running,
            &setup.policy,
            &setup.pending,
            gate,
            &AtomicBool::new(stopping),
            log,
        );
        (attempt, gate)
    }

    /// 换代尝试在关闸后发现整机不空就作罢:这次做的落点快照不留下(留着它,配上上一次换代的守望
    /// 进程 pid 文件,启动对账会误判成换代被中断,把待换代的版本记进退回名单)。
    #[test]
    fn an_attempt_that_finds_the_machine_busy_leaves_no_snapshot() {
        let Some(setup) = attempt_setup() else { return };
        let home = &setup.fixture.home;
        let _busy = crate::observe::runtime_status::enter_handling("t-busy", "oc_busy");
        let (attempt, gate) = try_attempt(&setup, false, &home.join("upgrade.log"));
        let attempt = attempt.unwrap();
        assert!(matches!(attempt, Attempt::NotQuiet(_)), "{attempt:?}");
        assert!(!previous_dir(home).exists(), "作罢的换代不留快照");
        assert!(gate.is_open(), "作罢之后开闸");
    }

    /// 守望进程起来之前的另外两条出错路径(日志打不开、守望进程起不来)同样不留快照,并开闸。
    #[test]
    fn an_attempt_that_fails_before_the_watcher_starts_leaves_no_snapshot() {
        use std::os::unix::fs::PermissionsExt;
        let Some(setup) = attempt_setup() else { return };
        let home = &setup.fixture.home;
        let (attempt, gate) = try_attempt(&setup, false, &home.join("no-such-dir/upgrade.log"));
        assert!(attempt.is_err());
        assert!(!previous_dir(home).exists(), "日志打不开:不留快照");
        assert!(gate.is_open());

        // 落点里的 summon 不可执行:快照硬链着它,守望进程起不来。构建戳只看文件身份,权限不影响。
        let Some(setup) = attempt_setup() else { return };
        let home = &setup.fixture.home;
        let target = companion::install_target(home);
        // 可执行夹具按内容硬链接共享；权限故障只施加在落点的独立副本上。
        let private = target.with_extension("private");
        std::fs::copy(&target, &private).unwrap();
        std::fs::rename(&private, &target).unwrap();
        crate::setup::self_install::record_from(&target, home, &setup.running).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (attempt, gate) = try_attempt(&setup, false, &home.join("upgrade.log"));
        assert!(attempt.is_err(), "{attempt:?}");
        assert!(!previous_dir(home).exists(), "守望进程起不来:不留快照");
        assert!(gate.is_open());
    }

    /// 守望进程起来、本代进入停机:快照从此归守望进程与兜底进程,原样留着 —— 退回全靠它。
    #[test]
    fn a_started_swap_keeps_its_snapshot_for_the_watcher_and_the_guardian() {
        let Some(setup) = attempt_setup() else { return };
        let home = &setup.fixture.home;
        let installed = std::fs::read(companion::install_target(home)).unwrap();
        let (attempt, gate) = try_attempt(&setup, true, &home.join("upgrade.log"));
        let attempt = attempt.unwrap();
        assert!(
            matches!(attempt, Attempt::Hold(Hold::Stopping)),
            "{attempt:?}"
        );
        assert!(!gate.is_open(), "停机开始后闸封住");
        let snapshot = previous_dir(home);
        assert_eq!(snapshot_ready(&snapshot), Ok(()), "快照完整");
        assert_eq!(
            std::fs::read(snapshot.join(companion::SUMMON)).unwrap(),
            installed
        );
        assert!(snapshot.join(companion::SIDECAR).is_file());
    }

    /// 守望进程没让本代停下就结束了:本代接着跑,快照不再是换代的证据,删掉。
    #[test]
    fn an_attempt_whose_watcher_ends_by_itself_drops_its_snapshot() {
        let Some(setup) = attempt_setup() else { return };
        let home = &setup.fixture.home;
        let (attempt, gate) = try_attempt(&setup, false, &home.join("upgrade.log"));
        let attempt = attempt.unwrap();
        assert!(
            matches!(attempt, Attempt::Hold(Hold::WatcherEnded)),
            "{attempt:?}"
        );
        assert!(!previous_dir(home).exists());
        assert!(gate.is_open());
    }

    /// 快照带清单,退回前据它确认完整:少文件、字节数不对、清单没了、没有 summon 都不能用。
    #[test]
    fn a_snapshot_is_ready_only_when_its_listed_files_are_all_there() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let bin = companion::install_dir(home);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(companion::SUMMON), "old summon").unwrap();
        std::fs::write(bin.join(companion::SIDECAR), "old sidecar").unwrap();
        let snapshot = snapshot_install(home).unwrap();
        assert_eq!(
            snapshot_ready(&snapshot),
            Ok(()),
            "落点本来就没有的 Menu Bar 不算缺"
        );

        std::fs::write(snapshot.join(companion::SIDECAR), "short").unwrap();
        assert!(snapshot_ready(&snapshot)
            .unwrap_err()
            .contains(companion::SIDECAR));
        std::fs::remove_file(snapshot.join(companion::SIDECAR)).unwrap();
        assert!(snapshot_ready(&snapshot)
            .unwrap_err()
            .contains(companion::SIDECAR));

        let snapshot = snapshot_install(home).unwrap();
        std::fs::remove_file(snapshot.join(SNAPSHOT_LIST)).unwrap();
        assert!(
            snapshot_ready(&snapshot).is_err(),
            "没有清单说不清完整不完整"
        );

        std::fs::create_dir_all(home.join("bare")).unwrap();
        std::fs::write(home.join("bare").join(companion::SIDECAR), "x").unwrap();
        seal_snapshot(&home.join("bare")).unwrap();
        assert!(
            snapshot_ready(&home.join("bare")).is_err(),
            "没有 summon 无从退回"
        );
    }
}
