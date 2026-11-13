//! 自动升级的守望进程:由 daemon 在关上准入闸之后拉起(新会话,升级锁作为 stdin 交进来,跑完才
//! 释放),跑的是换代前快照下来的旧版本身。
//!
//! 它先用与 `summon upgrade` 同一段脚本([`upgrade_restart::RESTART_SCRIPT`])让 daemon 换代,再
//! 观察新版 [`OBSERVE`] 这么久:新版没起来,或观察期内退出了而不是人经 `summon daemon stop` 停的
//! (崩溃、被杀、自己有序退出),就退回 —— 先记下这一版不再自动采用(运行期发布也据此不再把它装进
//! 落点),再停掉还在跑的新版,把快照里的旧文件放回落点,钉住落点起旧版。退回只靠这份快照,不经任何
//! 包管理器。
//! 人在观察期内停掉 daemon 是人的决定,不退回。

use super::github;
use super::state::{previous_dir, unix_now, Store};
use crate::setup::upgrade_restart;
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;
use summon_core::companion;
use summon_core::install_stamp;

/// 新版换代后观察多久,与崩溃熔断的稳定判据同长。
pub(crate) const OBSERVE: Duration = Duration::from_secs(10 * 60);
/// 观察期里多久看一次 daemon。
const OBSERVE_POLL: Duration = Duration::from_secs(10);

#[derive(clap::Args, Debug)]
pub struct WatchArgs {
    #[arg(help = summon_i18n::text!("r069a729998"))]
    /// 新版的入口
    #[arg(long)]
    pub entry: PathBuf,
    #[arg(help = summon_i18n::text!("r38468e4c6f"))]
    /// 换代到的版本
    #[arg(long)]
    pub version: String,
    #[arg(help = summon_i18n::text!("r083bffe7b9"))]
    /// 换代前在跑的版本
    #[arg(long)]
    pub previous: String,
    /// 新版的暂存目录:换代后核对落点里的文件都已是它
    #[arg(long)]
    pub stage: PathBuf,
    #[arg(help = summon_i18n::text!("rcb3e341539"))]
    /// 装过 Menu Bar
    #[arg(long)]
    pub menubar: bool,
}

pub fn run(args: &WatchArgs) -> Result<()> {
    let home = dirs::home_dir().context(summon_i18n::text!("rc086b7ca57"))?;
    Watch {
        home: &home,
        args,
        probe: &LiveProbe,
        platform: summon_core::release::platform(std::env::consts::OS, std::env::consts::ARCH),
        observe: OBSERVE,
        poll: OBSERVE_POLL,
        restore_delay_secs: upgrade_restart::RESTORE_DELAY_SECS,
    }
    .run(&mut std::io::stdout(), std::thread::sleep)
}

/// 看 daemon 的几样东西;测试换成假的。
pub(crate) trait Probe {
    /// 在跑的 daemon。
    fn running(&self) -> Option<u32>;
    /// 它自报的版本。
    fn version(&self, pid: u32) -> Option<String>;
    /// 这一代没走完停机就没了。
    fn left_unfinished(&self, pid: u32) -> bool;
    /// 这一代是人经 `summon daemon stop` 停的。
    fn stop_requested(&self, pid: u32) -> bool;
}

struct LiveProbe;

impl Probe for LiveProbe {
    fn running(&self) -> Option<u32> {
        crate::runtime::daemon::running_pid()
    }

    fn version(&self, pid: u32) -> Option<String> {
        crate::runtime::daemon::daemon_version(pid)
    }

    fn left_unfinished(&self, pid: u32) -> bool {
        crate::runtime::daemon::left_unfinished(pid)
    }

    fn stop_requested(&self, pid: u32) -> bool {
        crate::runtime::daemon::stop_requested(pid)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Observed {
    Stable,
    /// 人经 `summon daemon stop` 停了 daemon。
    StoppedByHuman,
    /// daemon 没走完停机就没了。
    Crashed,
    /// daemon 走完停机退出了,却不是人停的。
    ExitedOnItsOwn,
}

/// 观察 `pid` 起头的这一代 daemon。中途换了一代(人重启、自启单元拉起)就接着看新的那一代。
/// 没了的时候先看停机走没走完,再看是不是人停的:只有人停的才不算新版的问题。
pub(crate) fn observe(
    probe: &dyn Probe,
    mut pid: u32,
    window: Duration,
    poll: Duration,
    mut sleep: impl FnMut(Duration),
) -> Observed {
    let mut waited = Duration::ZERO;
    while waited < window {
        sleep(poll);
        waited += poll;
        match probe.running() {
            Some(now) => pid = now,
            None if probe.left_unfinished(pid) => return Observed::Crashed,
            None if probe.stop_requested(pid) => return Observed::StoppedByHuman,
            None => return Observed::ExitedOnItsOwn,
        }
    }
    Observed::Stable
}

pub(crate) struct Watch<'a> {
    pub(crate) home: &'a Path,
    pub(crate) args: &'a WatchArgs,
    pub(crate) probe: &'a dyn Probe,
    /// 本机的发布平台:换代后按它的文件清单核对落点。
    pub(crate) platform: Option<&'static str>,
    pub(crate) observe: Duration,
    pub(crate) poll: Duration,
    pub(crate) restore_delay_secs: u32,
}

impl Watch<'_> {
    pub(crate) fn run(&self, out: &mut dyn Write, sleep: impl FnMut(Duration)) -> Result<()> {
        let result = self.watch(out, sleep);
        if let Err(error) = &result {
            // 换代前就出错时旧 daemon 还在跑,它据待换代记录还挂着判这一版今天不再试;这里留下原因。
            let _ = self.note(out, &summon_i18n::format!("r6466e44238"));
        }
        // 快照硬链着旧版的文件,留着就一直占着那份盘;换代成了、人停了或退回已放回落点,就用不着了。
        // 本进程跑的就是快照里那份,删掉目录项不影响它跑完。出错时留着:退回可能只做了一半,兜底进程
        // 还要拿它放回换代前的落点,启动对账也据它认出换代被中断。
        if result.is_ok() {
            let _ = std::fs::remove_dir_all(previous_dir(self.home));
        }
        result
    }

    fn watch(&self, out: &mut dyn Write, sleep: impl FnMut(Duration)) -> Result<()> {
        let (new, previous) = (&self.args.version, &self.args.previous);
        writeln!(out, "{}", summon_i18n::format!("r44cc0ea24c"))?;
        out.flush()?;
        let status = self.restart()?;
        let running = self.probe.running();
        let took = running.and_then(|pid| self.probe.version(pid));
        let restarted = matches!(
            status.code(),
            Some(0) | Some(upgrade_restart::MENUBAR_FAILED)
        );
        let pid = match running {
            Some(pid) if restarted && took.as_deref() == Some(new.as_str()) => pid,
            _ => {
                let why = summon_i18n::format!(
                    "r14ab596202",
                    took.as_deref().unwrap_or(summon_i18n::text!("rd9072b793b"))
                );
                return self.roll_back(&why, out);
            }
        };
        // daemon 报的是新版还不够,落点里本平台的每个文件都得已是暂存里的那份。
        if let Err(why) = github::landed(self.home, &self.args.stage, self.platform) {
            return self.roll_back(&why, out);
        }
        writeln!(
            out,
            "{}",
            summon_i18n::format!("rd592f6f649", self.observe.as_secs() / 60)
        )?;
        out.flush()?;
        match observe(self.probe, pid, self.observe, self.poll, sleep) {
            Observed::Stable => self.note(out, &summon_i18n::format!("ra66fd5525e")),
            Observed::StoppedByHuman => self.note(out, &summon_i18n::format!("r5e03645738")),
            Observed::Crashed => self.roll_back(summon_i18n::text!("r61732ac967"), out),
            Observed::ExitedOnItsOwn => self.roll_back(summon_i18n::text!("rd32f9e8ae7"), out),
        }
    }

    fn note(&self, out: &mut dyn Write, text: &str) -> Result<()> {
        writeln!(out, "[auto-upgrade] {text}")?;
        Store::at_home(self.home).update(|state| state.note(unix_now(), text))?;
        Ok(())
    }

    /// 与 `summon upgrade` 同一段脚本:新入口 `daemon restart`,失败用落点兜底拉起,装过 Menu Bar
    /// 的一并重启。升级锁就是本进程的 stdin,原样交给它;它的输出直接进日志。
    fn restart(&self) -> Result<ExitStatus> {
        Command::new("/bin/sh")
            .args([
                "-c",
                ::summon_core::i18n::label(upgrade_restart::RESTART_SCRIPT),
                "summon-upgrade",
            ])
            .arg(&self.args.entry)
            .arg(if self.args.menubar { "1" } else { "0" })
            .arg(companion::install_target(self.home))
            .arg(self.restore_delay_secs.to_string())
            .status()
            .context(summon_i18n::text!("rbdfd665537"))
    }

    fn roll_back(&self, why: &str, out: &mut dyn Write) -> Result<()> {
        let (new, previous) = (&self.args.version, &self.args.previous);
        writeln!(out, "{}", summon_i18n::format!("rbcc0b81622"))?;
        out.flush()?;
        // 先记账:后面哪一步失败,这一版都不会再被自动装回来,运行期发布也不再把它装进落点。
        Store::at_home(self.home).update(|state| {
            if !state.rolled_back.contains(new) {
                state.rolled_back.push(new.clone());
            }
            state.pending = None;
            state.note(unix_now(), summon_i18n::format!("rd0c3e13e9d"));
        })?;
        let install = companion::install_target(self.home);
        let snapshot = previous_dir(self.home);
        // 快照不完整就不退回:停了新版再用残缺的快照放回,落点会新旧混合,或者仍是新版却记成已退回。
        // 不停在跑的、不动落点;快照删掉,免得兜底进程拿它放回半截。新版本来就没起来时用落点现有的
        // 那份拉起 daemon,机器上不能没有 daemon。
        if let Err(missing) = super::swap::snapshot_ready(&snapshot) {
            let _ = std::fs::remove_dir_all(&snapshot);
            if self.probe.running().is_some() {
                bail!(summon_i18n::format!("r44dbb630d8"));
            }
            self.summon(&install, &["daemon", "start"], true);
            if self.args.menubar {
                self.summon(&install, &["menubar", "restart"], false);
            }
            self.summon(&install, &["daemon", "status"], false);
            bail!(summon_i18n::format!("r672e87e576"));
        }
        if self.probe.running().is_some() {
            self.summon(&install, &["daemon", "stop"], false);
        }
        restore_install(self.home, &snapshot)?;
        self.summon(&install, &["daemon", "start"], true);
        if self.args.menubar {
            self.summon(&install, &["menubar", "restart"], false);
        }
        self.summon(&install, &["daemon", "status"], false);
        self.note(out, &summon_i18n::format!("rb00395ce9a"))
    }

    /// 跑落点那份 summon;输出进日志,失败只记下来、不中断退回。`pinned`:钉住落点,不从别的安装源拉。
    fn summon(&self, install: &Path, args: &[&str], pinned: bool) {
        let mut command = Command::new(install);
        command.args(args).stdin(Stdio::null());
        if pinned {
            command.env(upgrade_restart::PIN_ENV, "1");
        }
        if let Err(error) = command.status() {
            println!(
                "{}",
                summon_i18n::format!("r62ebda3485", install.display(), args.join(" "))
            );
        }
    }
}

/// 把快照里的旧文件放回落点:硬链成暂存名再 rename 过去,落点里任何时刻都是一个完整的文件。
/// 构建戳一起放回,它描述的正是这些文件。
fn restore_install(home: &Path, snapshot: &Path) -> Result<()> {
    let dir = companion::install_dir(home);
    for name in companion::TRIO
        .into_iter()
        .chain([install_stamp::STAMP_FILE])
    {
        let src = snapshot.join(name);
        if !src.is_file() {
            continue;
        }
        let staged = dir.join(format!(".{name}.rollback"));
        let _ = std::fs::remove_file(&staged);
        if std::fs::hard_link(&src, &staged).is_err() {
            std::fs::copy(&src, &staged).with_context(|| {
                summon_i18n::format!("rd08d8839d8", src.display(), staged.display())
            })?;
        }
        std::fs::rename(&staged, dir.join(name))
            .with_context(|| summon_i18n::format!("rc9d7c3c029", dir.join(name).display()))?;
    }
    Ok(())
}

/// 假 daemon,供守望进程的测试用(这里与各渠道的测试共用)。
#[cfg(test)]
pub(crate) mod fake {
    use super::Probe;
    use std::cell::RefCell;

    /// 假 daemon 的状态:一串 `running()` 的回答,用完之后一直答最后一个。
    pub(crate) struct FakeProbe {
        running: RefCell<Vec<Option<u32>>>,
        version: Option<String>,
        pub(crate) unfinished: Vec<u32>,
        pub(crate) stopped_by_human: Vec<u32>,
    }

    impl FakeProbe {
        pub(crate) fn new(running: &[Option<u32>], version: Option<&str>) -> Self {
            Self {
                running: RefCell::new(running.iter().rev().copied().collect()),
                version: version.map(str::to_string),
                unfinished: Vec::new(),
                stopped_by_human: Vec::new(),
            }
        }
    }

    impl Probe for FakeProbe {
        fn running(&self) -> Option<u32> {
            let mut answers = self.running.borrow_mut();
            if answers.len() > 1 {
                answers.pop().flatten()
            } else {
                answers.last().copied().flatten()
            }
        }

        fn version(&self, _pid: u32) -> Option<String> {
            self.version.clone()
        }

        fn left_unfinished(&self, pid: u32) -> bool {
            self.unfinished.contains(&pid)
        }

        fn stop_requested(&self, pid: u32) -> bool {
            self.stopped_by_human.contains(&pid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeProbe;
    use super::*;

    fn quick_observe(probe: &FakeProbe, pid: u32) -> Observed {
        observe(
            probe,
            pid,
            Duration::from_secs(60),
            Duration::from_secs(10),
            |_| {},
        )
    }

    #[test]
    fn observation_tells_a_crash_from_a_clean_stop() {
        let stable = FakeProbe::new(&[Some(7)], None);
        assert_eq!(quick_observe(&stable, 7), Observed::Stable);

        let mut crashed = FakeProbe::new(&[Some(7), None], None);
        crashed.unfinished = vec![7];
        assert_eq!(quick_observe(&crashed, 7), Observed::Crashed);

        let mut stopped = FakeProbe::new(&[Some(7), None], None);
        stopped.stopped_by_human = vec![7];
        assert_eq!(quick_observe(&stopped, 7), Observed::StoppedByHuman);

        // 走完了停机,却没有人经 CLI 停它:新版自己退了。
        let gone = FakeProbe::new(&[Some(7), None], None);
        assert_eq!(quick_observe(&gone, 7), Observed::ExitedOnItsOwn);

        // 中途换了一代:崩的是新的那一代,看的就是它。
        let mut relaunched = FakeProbe::new(&[Some(7), Some(8), None], None);
        relaunched.unfinished = vec![8];
        assert_eq!(quick_observe(&relaunched, 7), Observed::Crashed);
    }

    /// 假 summon,落点、快照与暂存里是同一份脚本(末尾一行注释区分新旧)。按所在位置认出家目录与
    /// 角色,把调用记进 `calls.log`;`daemon restart` 把 `publish` 里列的文件从暂存换进落点
    /// (模拟新版停机之后的自发布,列少了就是半发布)。
    const FAKE: &str = r#"#!/bin/sh
here="$(cd "$(dirname "$0")" && pwd)"
case "$here" in
  */.summon/bin) home="$(cd "$here/../.." && pwd)"; role=install ;;
  *) home="$(cd "$here/../../../.." && pwd)"; role=stage ;;
esac
echo "$role $* pin=${SUMMON_PIN_INSTALL:-}" >> "$home/calls.log"
case "$1 ${2:-}" in
  "daemon restart")
    if [ -e "$home/restart-fails" ]; then exit 1; fi
    for f in $(cat "$home/publish" 2>/dev/null); do
      cp "$here/$f" "$home/.summon/bin/.$f.new" && mv -f "$home/.summon/bin/.$f.new" "$home/.summon/bin/$f"
    done ;;
  "daemon status") echo "running(pid 4242, version x)" ;;
esac
exit 0
"#;

    struct Github {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        stage: PathBuf,
        args: WatchArgs,
    }

    impl Github {
        /// 落点与快照里是旧版,暂存里是新版;`publish` 是 `daemon restart` 换进落点的文件。
        fn new(publish: &str) -> Self {
            use crate::test_support::write_test_executable;
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().canonicalize().unwrap();
            let bin = companion::install_dir(&home);
            let stage = home.join(".summon/run/auto-upgrade-stage.t");
            let previous = previous_dir(&home);
            for (dir, tag) in [
                (&bin, "old"),
                (&previous, "old"),
                (&stage.join("pkg"), "new"),
            ] {
                std::fs::create_dir_all(dir).unwrap();
                write_test_executable(&dir.join(companion::SUMMON), &format!("{FAKE}# {tag}\n"));
                std::fs::write(dir.join(companion::SIDECAR), format!("{tag} sidecar")).unwrap();
            }
            super::super::swap::seal_snapshot(&previous).unwrap();
            std::fs::write(home.join("publish"), publish).unwrap();
            let args = WatchArgs {
                entry: stage.join("pkg").join(companion::SUMMON),
                version: "0.6.35".into(),
                previous: "0.6.34".into(),
                stage: stage.clone(),
                menubar: false,
            };
            Self {
                _tmp: tmp,
                home,
                stage,
                args,
            }
        }

        fn run(&self, probe: &FakeProbe) -> Result<()> {
            self.run_with_output(probe).0
        }

        fn run_with_output(&self, probe: &FakeProbe) -> (Result<()>, String) {
            let mut out = Vec::new();
            let result = Watch {
                home: &self.home,
                args: &self.args,
                probe,
                platform: Some("linux-x64"),
                observe: Duration::from_secs(60),
                poll: Duration::from_secs(10),
                restore_delay_secs: 0,
            }
            .run(&mut out, |_| {});
            (result, String::from_utf8(out).unwrap())
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(self.home.join("calls.log")).unwrap_or_default()
        }

        fn installed(&self, name: &str) -> String {
            std::fs::read_to_string(companion::install_dir(&self.home).join(name)).unwrap()
        }

        fn state(&self) -> super::super::state::State {
            Store::at_home(&self.home).load().unwrap()
        }
    }

    /// 新版报的版本对、落点里每个文件都已是暂存里那份:换代成了,不退回。
    #[test]
    fn a_github_swap_is_kept_once_every_file_has_landed() {
        let github = Github::new("summon summon-sidecar");
        github
            .run(&FakeProbe::new(&[Some(9)], Some("0.6.35")))
            .unwrap();
        assert!(github.state().rolled_back.is_empty());
        assert_eq!(github.installed(companion::SIDECAR), "new sidecar");
        assert!(github.installed(companion::SUMMON).ends_with("# new\n"));
        let calls = github.calls();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            ["stage daemon restart pin=", "install daemon status pin="],
            "没退回就不停、不起:{calls}"
        );
    }

    /// daemon 报新版,落点却没换全(半发布、或停机后的发布没成、daemon 从暂存起):退回。退回只靠换代
    /// 前的落点快照 —— 停掉在跑的、放回快照、钉住落点起;除了落点那份 summon 不跑任何东西。
    #[test]
    fn a_github_swap_that_did_not_land_is_rolled_back_from_the_snapshot_alone() {
        for (publish, want) in [("summon", "summon-sidecar"), ("", "summon")] {
            let github = Github::new(publish);
            github
                .run(&FakeProbe::new(&[Some(9)], Some("0.6.35")))
                .unwrap();
            let state = github.state();
            assert_eq!(state.rolled_back, ["0.6.35"], "{publish:?}");
            let event = state.last_event.unwrap().text;
            assert!(
                event.contains(want) && event.contains("已退回 0.6.34"),
                "{event}"
            );
            assert_eq!(github.installed(companion::SIDECAR), "old sidecar");
            assert!(github.installed(companion::SUMMON).ends_with("# old\n"));
            let calls = github.calls();
            let lines: Vec<&str> = calls.lines().collect();
            assert_eq!(
                lines,
                [
                    "stage daemon restart pin=",
                    "install daemon status pin=",
                    "install daemon stop pin=",
                    "install daemon start pin=1",
                    "install daemon status pin=",
                ],
                "{publish:?}"
            );
            assert!(github.stage.join("pkg/summon").exists(), "暂存留给对账去清");
        }
    }

    /// 换代成了还要观察:观察期内运行正常就留着新版,收尾删掉旧版快照。
    #[test]
    fn a_new_version_that_stays_up_is_kept() {
        let github = Github::new("summon summon-sidecar");
        let (result, out) = github.run_with_output(&FakeProbe::new(&[Some(9)], Some("0.6.35")));
        result.unwrap();
        assert!(out.contains("观察 1 分钟"), "{out}");
        let state = github.state();
        assert!(state.rolled_back.is_empty());
        assert!(state.last_event.unwrap().text.contains("观察期内运行正常"));
        assert!(!previous_dir(&github.home).exists(), "收尾删掉旧版快照");
    }

    /// 新版没起来(重启失败、没有 daemon):不停、放回快照、钉住落点起旧版,这一版进退回名单。
    #[test]
    fn a_new_version_that_does_not_come_up_is_rolled_back() {
        let github = Github::new("summon summon-sidecar");
        std::fs::write(github.home.join("restart-fails"), "").unwrap();
        let (result, out) = github.run_with_output(&FakeProbe::new(&[None], None));
        result.unwrap();
        assert!(out.contains("0.6.35 没起来"), "{out}");
        let calls = github.calls();
        assert!(
            !calls.contains("install daemon stop"),
            "没在跑就不停:{calls}"
        );
        assert!(calls.contains("install daemon start pin=1"), "{calls}");
        assert_eq!(github.installed(companion::SIDECAR), "old sidecar");
        assert!(github.installed(companion::SUMMON).ends_with("# old\n"));
        assert!(!previous_dir(&github.home).exists());
        let state = github.state();
        assert_eq!(state.rolled_back, ["0.6.35"]);
        assert!(state.pending.is_none());
        assert!(state.last_event.unwrap().text.contains("已退回 0.6.34"));
    }

    /// 起来了,观察期里没走完停机就没了:退回。
    #[test]
    fn a_new_version_that_crashes_while_watched_is_rolled_back() {
        let github = Github::new("summon summon-sidecar");
        let mut probe = FakeProbe::new(&[Some(9), Some(9), None], Some("0.6.35"));
        probe.unfinished = vec![9];
        let (result, out) = github.run_with_output(&probe);
        result.unwrap();
        assert!(out.contains("在观察期内异常退出"), "{out}");
        assert!(github.calls().contains("install daemon start pin=1"));
        assert_eq!(github.state().rolled_back, ["0.6.35"]);
        assert_eq!(github.installed(companion::SIDECAR), "old sidecar");
    }

    /// 起来的不是新版(重启没换进去、兜底起了旧的):照样退回,先停掉在跑的那一个再起。
    #[test]
    fn a_restart_that_did_not_take_the_new_version_is_rolled_back() {
        let github = Github::new("summon summon-sidecar");
        let (result, out) = github.run_with_output(&FakeProbe::new(&[Some(5)], Some("0.6.34")));
        result.unwrap();
        assert!(out.contains("daemon 当前 0.6.34"), "{out}");
        let calls = github.calls();
        let stop = calls.find("install daemon stop").expect("先停在跑的");
        let start = calls.find("install daemon start pin=1").expect("再起旧版");
        assert!(stop < start, "{calls}");
    }

    /// 人在观察期内停掉 daemon 是人的决定:不退回。
    #[test]
    fn a_human_stop_during_observation_is_left_alone() {
        let github = Github::new("summon summon-sidecar");
        let mut probe = FakeProbe::new(&[Some(9), None], Some("0.6.35"));
        probe.stopped_by_human = vec![9];
        github.run(&probe).unwrap();
        let state = github.state();
        assert!(state.rolled_back.is_empty());
        assert!(state.last_event.unwrap().text.contains("被人停止,不退回"));
        assert!(!github.calls().contains("daemon start"));
    }

    /// 走完停机退出、却没有人停它:机器上没有 daemon 了,按新版出了问题退回。
    #[test]
    fn a_new_version_that_exits_on_its_own_is_rolled_back() {
        let github = Github::new("summon summon-sidecar");
        let (result, out) =
            github.run_with_output(&FakeProbe::new(&[Some(9), None], Some("0.6.35")));
        result.unwrap();
        assert!(out.contains("不是人停的"), "{out}");
        assert!(github.calls().contains("install daemon start pin=1"));
        assert_eq!(github.state().rolled_back, ["0.6.35"]);
    }

    /// 守望命令在顶层(隐藏),必须带新版的暂存目录 `--stage`。
    #[test]
    fn the_watch_command_takes_the_stage_in_every_build() {
        use clap::Parser;
        let cli = crate::cli::cli::Cli::try_parse_from([
            "summon",
            "auto-upgrade-watch",
            "--entry",
            "/s/pkg/summon",
            "--version",
            "0.6.35",
            "--previous",
            "0.6.34",
            "--stage",
            "/s",
        ])
        .unwrap();
        let crate::cli::cli::Cmd::AutoUpgradeWatch(args) = cli.cmd else {
            panic!("守望命令没落到顶层");
        };
        assert_eq!(args.stage, Path::new("/s"));
        let without_stage = crate::cli::cli::Cli::try_parse_from([
            "summon",
            "auto-upgrade-watch",
            "--entry",
            "/s/pkg/summon",
            "--version",
            "0.6.35",
            "--previous",
            "0.6.34",
        ]);
        assert!(without_stage.is_err(), "没有暂存就核对不了落点");
    }

    /// 守望进程自己出错(这里是退回时写不进状态文件)就留着快照:兜底进程还要拿它放回换代前的落点。
    #[test]
    fn a_watcher_that_fails_keeps_the_snapshot_for_the_guardian() {
        let github = Github::new("");
        std::fs::create_dir_all(github.home.join(".summon/run/auto-upgrade.json")).unwrap();
        let result = github.run(&FakeProbe::new(&[Some(9)], Some("0.6.35")));
        assert!(result.is_err());
        assert!(previous_dir(&github.home).join(companion::SUMMON).exists());
    }

    /// 快照不完整(这里少了 sidecar)而新版在跑:不停它、不动落点,如实报退回不了;这一版照样进退回
    /// 名单。残缺的快照删掉,兜底进程不会拿它放回半截。
    #[test]
    fn a_rollback_without_a_complete_snapshot_leaves_the_new_version_running() {
        let github = Github::new("summon");
        let previous = previous_dir(&github.home);
        std::fs::remove_file(previous.join(companion::SIDECAR)).unwrap();
        let result = github.run(&FakeProbe::new(&[Some(9)], Some("0.6.35")));
        let error = format!("{:#}", result.unwrap_err());
        assert!(
            error.contains("快照") && error.contains("没有停新版"),
            "{error}"
        );
        assert_eq!(
            github.calls().lines().collect::<Vec<_>>(),
            ["stage daemon restart pin=", "install daemon status pin="],
            "不停、不起"
        );
        assert!(
            github.installed(companion::SUMMON).ends_with("# new\n"),
            "落点不动"
        );
        assert_eq!(github.installed(companion::SIDECAR), "old sidecar");
        let state = github.state();
        assert_eq!(state.rolled_back, ["0.6.35"]);
        assert!(state.last_event.unwrap().text.contains("快照"));
        assert!(!previous.exists());
    }

    /// 新版本来就没起来、快照又残缺(字节数对不上):不放回,用落点现有的那份拉起 daemon。
    #[test]
    fn a_new_version_that_never_came_up_without_a_usable_snapshot_starts_what_is_installed() {
        let github = Github::new("summon");
        let previous = previous_dir(&github.home);
        std::fs::write(previous.join(companion::SIDECAR), "old").unwrap();
        let result = github.run(&FakeProbe::new(&[None], None));
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("快照"), "{error}");
        let calls = github.calls();
        assert!(!calls.contains("daemon stop"), "{calls}");
        assert!(calls.contains("install daemon start pin=1"), "{calls}");
        assert!(
            github.installed(companion::SUMMON).ends_with("# new\n"),
            "落点不动"
        );
        assert_eq!(github.state().rolled_back, ["0.6.35"]);
    }
}
