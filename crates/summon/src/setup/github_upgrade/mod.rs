//! `summon upgrade` 的 GitHub Releases 渠道:从 Release 取 manifest 与本平台的包,核对后换进落点
//! `~/.summon/bin/`,用新的那份重启 daemon。没有 npm 分发渠道的构建里它就是 `summon upgrade`,
//! 带 npm 渠道的构建经 `summon upgrade --channel github` 走它。
//!
//! 顺序(前面任何一步不成立都不碰落点,暂存目录随之删掉,退出码非零):
//!
//! 1. 取 manifest(默认 `releases/latest/download/manifest.json`,`--version` 时取那个 tag 下的,
//!    `--from` 时取给定的 URL 前缀或本地目录下的),校验字段;给了 `--version` 时它的版本必须正是那一版。
//! 2. 与落点的构建戳比新旧(`install_stamp` 的判据)。不更新就结束、不下载;`--version` 指向更旧的
//!    版本时拒绝,不降级。
//! 3. manifest 没列本平台就报「这一版没有本平台的包」。
//! 4. 下载到 `~/.summon/run/` 下的暂存目录,边下边核字节数与 SHA-256。
//! 5. 解包:成员恰为本平台应有的文件,平铺的普通文件,0755。
//! 6. macOS:三件套过 `codesign --verify --strict`、签名团队一致且与当前在跑的这份相同 —— 在执行
//!    暂存里的任何东西之前。
//! 7. 暂存那份 `summon install-stamp print` 自报的构建戳必须与 manifest 一致。
//! 8. 这时重新看 daemon(下载可能花了几分钟,期间它可能被起来或停掉):没在跑就经运行期发布逻辑换进
//!    落点,不启动它;在跑就用暂存那份的 `daemon restart` 换代(停机之后由它自己发布进落点,再用落点
//!    那份起),重启段脱离当前会话([`upgrade_restart`])。
//!
//! 不发飞书消息,不碰 tmux 里的 agent 会话。

pub(crate) mod source;

use crate::setup::self_install::{self, Daemon};
use crate::setup::upgrade_restart::{self, Restart};
use anyhow::{bail, Context, Result};
use source::{DirSource, HttpSource};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use summon_core::companion;
use summon_core::install_stamp::{self, BuildInfo, Installed, Relation};
use summon_core::release::{self, Asset, Manifest, Source};

#[derive(clap::Args, Debug, Clone, Default)]
pub struct UpgradeArgs {
    #[arg(help = summon_i18n::text!("r7b5b28a11a"))]
    /// 要装的版本(纯 x.y.z);省略即最新版。给了 --from 时要求那里的 manifest 正是这一版
    #[arg(long, value_name = "x.y.z")]
    pub version: Option<String>,
    #[arg(help = summon_i18n::text!("r0a5282deb4"))]
    /// Release 的来源:URL 前缀或本地目录,其下直接有 manifest.json 与各平台的包
    #[arg(long, value_name = "URL|DIR")]
    pub from: Option<String>,
    #[arg(help = summon_i18n::text!("raa9fbf7bba"))]
    /// 只报告有没有新版,不下载
    #[arg(long)]
    pub check: bool,
}

/// 暂存那份 `install-stamp print` 的时限。刚解出的二进制在 macOS 首次执行要过一遍系统评估。
pub(crate) const STAMP_TIMEOUT: Duration = Duration::from_secs(30);
/// 每次调 codesign 的时限。
const CODESIGN_TIMEOUT: Duration = Duration::from_secs(60);
/// 暂存目录名的前缀,在 `~/.summon/run/` 下。
const STAGE_PREFIX: &str = "upgrade-stage.";
/// 暂存目录里有它:换代后 daemon 可能正从这里运行,daemon 在跑时不清理。
const KEEP_MARKER: &str = "KEEP";

/// 一次升级要看的本机条件;真实路径上取自这一刻的本机。
pub(crate) struct Host {
    pub home: PathBuf,
    /// 发布包的平台名;不在发布范围内时为空。
    pub platform: Option<&'static str>,
    /// 此刻 daemon 的状态。每次要据它动手时现取,不用开头的快照。
    pub daemon: Box<dyn Fn() -> Daemon>,
    pub menubar_installed: bool,
    pub restore_delay_secs: u32,
    pub stamp_timeout: Duration,
    /// 要核对代码签名时(macOS)用的 codesign 与当前在跑的这份。
    pub codesign: Option<Codesign>,
}

pub(crate) struct Codesign {
    pub tool: PathBuf,
    /// 当前在跑的这份 summon:新包的签名团队要与它的相同。
    pub current: PathBuf,
}

impl Codesign {
    /// 本机要不要核代码签名:macOS 上用系统的 codesign,与当前在跑的这份比签名团队;别的平台不核。
    pub(crate) fn for_current_exe() -> Result<Option<Self>> {
        if !cfg!(target_os = "macos") {
            return Ok(None);
        }
        let current = std::env::current_exe().context(summon_i18n::text!("r3f14f2b415"))?;
        Ok(Some(Self {
            tool: PathBuf::from("/usr/bin/codesign"),
            current: current.canonicalize().unwrap_or(current),
        }))
    }
}

/// 核对暂存里的新版要用的几样:问它构建戳的时限、要不要核代码签名。
pub(crate) struct Verify<'a> {
    pub stamp_timeout: Duration,
    pub codesign: Option<&'a Codesign>,
}

impl Host {
    fn verify(&self) -> Verify<'_> {
        Verify {
            stamp_timeout: self.stamp_timeout,
            codesign: self.codesign.as_ref(),
        }
    }
}

pub fn run(args: &UpgradeArgs) -> Result<()> {
    let codesign = Codesign::for_current_exe()?;
    let host = Host {
        home: dirs::home_dir().context(summon_i18n::text!("rc086b7ca57"))?,
        platform: release::platform(std::env::consts::OS, std::env::consts::ARCH),
        daemon: Box::new(self_install::current_daemon),
        menubar_installed: upgrade_restart::menubar_installed(),
        restore_delay_secs: upgrade_restart::RESTORE_DELAY_SECS,
        stamp_timeout: STAMP_TIMEOUT,
        codesign,
    };
    upgrade(&host, args, &mut std::io::stdout())
}

fn is_url(text: &str) -> bool {
    ["https://", "http://"].iter().any(|scheme| {
        text.strip_prefix(scheme)
            .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
    })
}

/// 联网之前校验参数。
fn check_args(args: &UpgradeArgs) -> Result<()> {
    if let Some(version) = &args.version {
        anyhow::ensure!(
            install_stamp::is_release_version(version),
            summon_i18n::format!("r0b290cdcaf")
        );
    }
    if let Some(from) = &args.from {
        anyhow::ensure!(
            is_url(from) || Path::new(from).is_dir(),
            summon_i18n::format!("r18fb0097a5")
        );
        // 明文链路上 manifest 与包能被一起换掉,核对就没有意义:http 只许本机,在联网之前拒绝。
        anyhow::ensure!(
            !is_url(from) || source::source_allowed(from),
            summon_i18n::format!("r9eb2c7d955")
        );
    }
    Ok(())
}

fn source_for(args: &UpgradeArgs) -> Result<Box<dyn Source>> {
    Ok(match args.from.as_deref() {
        None => Box::new(HttpSource::new(&source::release_base(
            args.version.as_deref(),
        ))?),
        Some(from) if is_url(from) => Box::new(HttpSource::new(from)?),
        Some(dir) => Box::new(DirSource(PathBuf::from(dir))),
    })
}

/// 前面的步骤没过时的报法:原因之后说清落点与 daemon 都没动。
fn untouched(error: impl Into<anyhow::Error>) -> anyhow::Error {
    let error = error.into();
    anyhow::anyhow!(summon_i18n::format!("r107a62a273"))
}

/// daemon 在跑、却证不了跑的就是 `new` 这一版(报的版本不同,或报不出版本):落点已是这一版时
/// 也要用落点那份换代。判据与 npm 渠道的 `next_step` 相同。
fn daemon_is_stale(daemon: &Daemon, new: &BuildInfo) -> bool {
    match daemon {
        Daemon::Stopped => false,
        Daemon::Running(Some(version)) => *version != new.version,
        Daemon::Running(None) => true,
    }
}

/// Release 的这一版相对落点该怎么做。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// 落点为空或更旧:装。
    Install,
    /// 落点已是这一版。
    UpToDate,
    /// 落点更新,没指定版本:不替换。
    InstallIsNewer,
    /// 指定的版本比落点旧:不降级。
    Downgrade,
    /// 判不出谁新:不替换。
    Undecided,
}

/// `pinned`:人指定了这一版(更旧时是降级请求,拒绝);没指定时更旧只说明、不替换。
pub(crate) fn decide(new: &BuildInfo, installed: &Installed, pinned: bool) -> Step {
    match install_stamp::relation(new, installed) {
        Relation::Absent | Relation::Newer => Step::Install,
        Relation::Same => Step::UpToDate,
        Relation::Older if pinned => Step::Downgrade,
        Relation::Older => Step::InstallIsNewer,
        Relation::Unknown => Step::Undecided,
    }
}

/// 取到的 manifest 与据此做出的判断。
struct Plan {
    source: Box<dyn Source>,
    platform: &'static str,
    manifest: Manifest,
    installed: Installed,
    step: Step,
}

impl Plan {
    fn fetch(host: &Host, args: &UpgradeArgs) -> Result<Self> {
        let platform = host
            .platform
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::text!("rf3c0e98ae6")))?;
        let source = source_for(args)?;
        let manifest = release::read_manifest(source.as_ref()).map_err(untouched)?;
        if let Some(wanted) = &args.version {
            let found = &manifest.version;
            if found != wanted {
                return Err(untouched(anyhow::anyhow!(summon_i18n::format!(
                    "r408363c366"
                ))));
            }
        }
        let installed = self_install::installed_build(&host.home);
        let step = decide(&manifest.build(), &installed, args.version.is_some());
        Ok(Self {
            source,
            platform,
            manifest,
            installed,
            step,
        })
    }

    fn asset(&self) -> Option<&Asset> {
        self.manifest.asset(self.platform)
    }
}

fn describe(installed: &Installed) -> String {
    match installed {
        Installed::Absent => summon_i18n::text!("r2f8267a89a").to_string(),
        Installed::Known(build) => build.describe(),
        Installed::Unanswered(why) | Installed::Unreadable(why) => why.clone(),
    }
}

pub(crate) fn upgrade(host: &Host, args: &UpgradeArgs, out: &mut dyn Write) -> Result<()> {
    check_args(args)?;
    if args.check {
        return report_check(&Plan::fetch(host, args)?, host, out);
    }
    let lock = upgrade_restart::lock(&host.home)?;
    let plan = Plan::fetch(host, args)?;
    let new = plan.manifest.build();
    let target = companion::install_target(&host.home);
    let installed = describe(&plan.installed);
    let new_text = new.describe();
    match plan.step {
        Step::Install => {}
        Step::UpToDate => {
            writeln!(
                out,
                "{}",
                summon_i18n::format!("rc74a1463a6", target = target.display())
            )?;
            let daemon = (host.daemon)();
            if !daemon_is_stale(&daemon, &new) {
                return Ok(());
            }
            // 落点已是这一版而 daemon 还在跑别的版本(落点由别的途径换过、上一次换代没起来):不下载,
            // 用落点那份走同一段脱离会话的重启。
            let (status, log) = restart_daemon(&target, &target, &daemon, host, lock, out)?;
            upgrade_restart::report_restart(status, &log, &new, &host.home)?;
            writeln!(out, "{}", summon_i18n::text!("rde21c42bb3"))?;
            return Ok(());
        }
        Step::InstallIsNewer => {
            writeln!(
                out,
                "{}",
                summon_i18n::format!("raf495a5ad7", target = target.display())
            )?;
            return Ok(());
        }
        Step::Downgrade => {
            return Err(untouched(anyhow::anyhow!(summon_i18n::format!(
                "r6a689deaa5",
                target = target.display()
            ))))
        }
        Step::Undecided => {
            return Err(untouched(anyhow::anyhow!(undecided_message(
                &plan.installed,
                &target,
                &new
            ))))
        }
    }
    let platform = plan.platform;
    let asset = plan
        .asset()
        .ok_or_else(|| untouched(anyhow::anyhow!(summon_i18n::format!("r86aae26146"))))?;
    writeln!(out, "{}", summon_i18n::format!("r161115b015"))?;
    let stage = Stage::create(&host.home, &(host.daemon)()).map_err(untouched)?;
    let exe = stage
        .prepare(
            plan.source.as_ref(),
            &plan.manifest,
            asset,
            platform,
            &host.verify(),
            out,
        )
        .map_err(untouched)?;
    // 人明确要装这一版:自动升级退回过它也照装,运行期发布才不挡它。取到并核对过包之后才解除,
    // 下载或校验没过不动退回名单。
    crate::setup::auto_upgrade::lift_rollback(&host.home, &new.version, out)?;
    // 下载可能花了几分钟:按此刻的 daemon 选路,开头的快照可能已经过期。重取之后到换文件之间仍有
    // 一个窗口,与运行期发布本身的窗口相同。
    let daemon = (host.daemon)();
    match &daemon {
        Daemon::Stopped => publish(&exe, &new, &daemon, host, out),
        Daemon::Running(_) => {
            // 重启之前就留下暂存并打上标记:发起者可能随停机被打断(无头轮里的 SIGINT),来不及清理,
            // 而 daemon 若因停机后的发布没成退回从暂存运行,下一次升级不能把它删掉。确认落点已是
            // 新版之后再删。
            let kept = stage.keep();
            let (status, log) = restart_daemon(&exe, &target, &daemon, host, lock, out)?;
            let reported = upgrade_restart::report_restart(status, &log, &new, &host.home);
            let landed = self_install::installed_build(&host.home);
            if install_stamp::relation(&new, &landed) == Relation::Same {
                // 落点已是新版,daemon 不会再从暂存运行。删不掉只是多占一份盘,之后 daemon 没在跑时
                // 的那次升级会清掉它。
                let _ = std::fs::remove_dir_all(&kept);
            } else {
                // 新版没进落点:停机之后的发布没成,daemon 可能退回用暂存那份起了。删掉它,daemon
                // 重启 sidecar 时就会配到落点里旧版的 sidecar。
                let landed = describe(&landed);
                let detail = reported.err().map(|error| format!("{error:#}"));
                let detail = detail.unwrap_or_default();
                bail!(summon_i18n::format!(
                    "reb46343aef",
                    target = target.display(),
                    kept = kept.display()
                ));
            }
            reported?;
            writeln!(out, "{}", summon_i18n::text!("rde21c42bb3"))?;
            Ok(())
        }
    }
}

/// 用 `entry` 的 `daemon restart` 换代(脱离当前会话,见 [`upgrade_restart`]),返回重启段的退出状态
/// 与日志;结论由调用方判。
fn restart_daemon(
    entry: &Path,
    target: &Path,
    daemon: &Daemon,
    host: &Host,
    lock: std::fs::File,
    out: &mut dyn Write,
) -> Result<(std::process::ExitStatus, String)> {
    let running = match daemon {
        Daemon::Running(Some(version)) => summon_i18n::format!("rbfd48ede62"),
        _ => summon_i18n::text!("r04ea81768a").to_string(),
    };
    writeln!(
        out,
        "{}",
        summon_i18n::format!(
            "r7b6a730810",
            if host.menubar_installed {
                summon_i18n::text!("r69ded844e8")
            } else {
                ""
            }
        )
    )?;
    let restart = Restart {
        entry,
        install: target,
        home: &host.home,
        menubar_installed: host.menubar_installed,
        restore_delay_secs: host.restore_delay_secs,
    };
    upgrade_restart::restart_detached(&restart, lock, out)
}

/// daemon 没在跑:当前进程经运行期发布逻辑把暂存的三件套换进落点(暂存 + rename,sidecar 缺失就
/// 整体放弃),不启动 daemon。
fn publish(
    exe: &Path,
    new: &BuildInfo,
    daemon: &Daemon,
    host: &Host,
    out: &mut dyn Write,
) -> Result<()> {
    let target = companion::install_target(&host.home);
    let outcome = self_install::publish_from(exe, &host.home, new, daemon)?;
    let landed = self_install::installed_build(&host.home);
    if install_stamp::relation(new, &landed) != Relation::Same {
        let landed = describe(&landed);
        let notice = outcome.notice.unwrap_or_default();
        bail!(summon_i18n::format!(
            "rb34ba0ae7e",
            target = target.display()
        ));
    }
    let new = new.describe();
    writeln!(
        out,
        "{}",
        summon_i18n::format!("r15ae4b3509", target = target.display())
    )?;
    Ok(())
}

fn undecided_message(installed: &Installed, target: &Path, new: &BuildInfo) -> String {
    let dir = target.parent().unwrap_or(target);
    let new = new.describe();
    match installed {
        Installed::Unanswered(why) => {
            summon_i18n::format!("r35e9fce7a8", target = target.display())
        }
        Installed::Unreadable(why) => summon_i18n::format!(
            "ra8e78a05af",
            target = target.display(),
            dir = dir.display()
        ),
        Installed::Absent | Installed::Known(_) => {
            let installed = describe(installed);
            summon_i18n::format!(
                "rd2135f3542",
                target = target.display(),
                dir = dir.display()
            )
        }
    }
}

/// `--check`:当前落点、Release 的这一版、结论。不拿升级锁、不下载、不写任何东西。
fn report_check(plan: &Plan, host: &Host, out: &mut dyn Write) -> Result<()> {
    let installed = describe(&plan.installed);
    let new = plan.manifest.build().describe();
    let at = plan.source.locate(release::MANIFEST);
    let target = companion::install_target(&host.home);
    writeln!(
        out,
        "{}",
        summon_i18n::format!("r90035ae21f", target = target.display())
    )?;
    writeln!(out, "{}", summon_i18n::format!("r24f68c9b2b"))?;
    let platform = plan.platform;
    let verdict = match plan.step {
        Step::Install if plan.asset().is_some() => summon_i18n::text!("r7857f9ee2d"),
        Step::Install => summon_i18n::text!("re8ca9f7837"),
        Step::UpToDate => summon_i18n::text!("r5985ee9a3c"),
        Step::InstallIsNewer => summon_i18n::text!("rdb5d8aae94"),
        Step::Downgrade => summon_i18n::text!("r18b4b48bb3"),
        Step::Undecided => summon_i18n::text!("r9c64e32118"),
    };
    let verdict = verdict.replace("{platform}", platform);
    writeln!(out, "{}", summon_i18n::format!("r5eae13af4b"))?;
    let daemon = (host.daemon)();
    if plan.step == Step::UpToDate && daemon_is_stale(&daemon, &plan.manifest.build()) {
        let running = match &daemon {
            Daemon::Running(Some(version)) => version.clone(),
            _ => summon_i18n::text!("r04ea81768a").to_string(),
        };
        writeln!(out, "{}", summon_i18n::format!("r737d67f0f3"))?;
    }
    Ok(())
}

/// 一次升级的暂存目录;丢掉即删(含所有出错返回的路径)。
pub(crate) struct Stage {
    dir: tempfile::TempDir,
}

impl Stage {
    /// `summon upgrade` 的暂存,在 `~/.summon/run/` 下。拿着升级锁建:同名前缀的别的暂存目录都是
    /// 之前留下的,按 [`sweep`] 的规则清掉。
    fn create(home: &Path, daemon: &Daemon) -> Result<Self> {
        let run = home.join(".summon/run");
        std::fs::create_dir_all(&run)
            .with_context(|| summon_i18n::format!("rc18f37e5e1", run.display()))?;
        sweep(&run, &companion::install_target(home), daemon);
        Self::create_in(&run, STAGE_PREFIX)
    }

    /// 在 `run` 下建一个名字以 `prefix` 开头的新暂存目录;清不清之前留下的由调用方决定。
    pub(crate) fn create_in(run: &Path, prefix: &str) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(run)
            .with_context(|| summon_i18n::format!("re40061e385", run.display()))?;
        Ok(Self { dir })
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// 留下暂存目录(不再随丢弃删除)并打上标记,返回它的路径。
    pub(crate) fn keep(self) -> PathBuf {
        // 先打标记再放手:标记写不成只是下一次升级在 daemon 在跑时也会清它。
        let _ = std::fs::write(self.path().join(KEEP_MARKER), "");
        self.dir.keep()
    }

    /// 下载、解包、核对签名与构建戳,返回暂存里的 summon。
    pub(crate) fn prepare(
        &self,
        source: &dyn Source,
        manifest: &Manifest,
        asset: &Asset,
        platform: &str,
        verify: &Verify<'_>,
        out: &mut dyn Write,
    ) -> Result<PathBuf> {
        let tarball = self.path().join(&asset.name);
        let at = source.locate(&asset.name);
        let size = asset.size;
        writeln!(out, "{}", summon_i18n::format!("r977f73f0ec"))?;
        out.flush()?;
        release::download(source, asset, &tarball)?;
        let pkg = self.path().join("pkg");
        std::fs::create_dir(&pkg)
            .with_context(|| summon_i18n::format!("rc18f37e5e1", pkg.display()))?;
        release::unpack(&tarball, platform, &pkg)?;
        // 解完就用不上了,不多占一份盘;删不掉也随暂存目录一起删。
        let _ = std::fs::remove_file(&tarball);
        // 签名先于执行:暂存里的东西通过签名核对之前,一个都不运行。
        if let Some(codesign) = verify.codesign {
            check_signatures(codesign, &pkg, platform, out)?;
        }
        let exe = pkg.join(companion::SUMMON);
        let reported = reported_build(&exe, verify.stamp_timeout)?;
        release::check_reported(&reported, manifest)?;
        writeln!(out, "{}", summon_i18n::text!("r37eaff52b4"))?;
        Ok(exe)
    }
}

/// 清掉之前留下的暂存。没有保留标记的(中途退出留下的)一律清。带标记的是换代前保留的:daemon 没在跑
/// 时清;daemon 在跑时,只有它的 `pkg/summon` 与落点里的 summon 逐字节相同才清 —— 那说明停机之后的发布
/// 成功了,daemon 是从落点起的(发起者随停机被打断、没来得及自己删时就是这样)。不相同或读不了就留着。
///
/// 残余:那次发布没成、daemon 从暂存运行,之后落点又被别的升级换成更新的版本时,这份暂存与落点不同,
/// 要等到一次 daemon 没在跑时的升级才清。
fn sweep(run: &Path, install: &Path, daemon: &Daemon) {
    let Ok(entries) = std::fs::read_dir(run) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(STAGE_PREFIX));
        if !ours || !path.is_dir() {
            continue;
        }
        let landed = || same_contents(&path.join("pkg").join(companion::SUMMON), install);
        if path.join(KEEP_MARKER).exists() && *daemon != Daemon::Stopped && !landed() {
            continue;
        }
        // 清不掉只是多占一份盘,不挡这次升级。
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// 两个文件逐字节相同。流式比较(先比长度),任一边读不了都算不同。
pub(crate) fn same_contents(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    let open = |path: &Path| {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok().filter(|meta| meta.is_file())?.len();
        Some((std::io::BufReader::new(file), len))
    };
    let (Some((mut a, len_a)), Some((mut b, len_b))) = (open(a), open(b)) else {
        return false;
    };
    if len_a != len_b {
        return false;
    }
    let (mut buf_a, mut buf_b) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let Ok(n) = a.read(&mut buf_a) else {
            return false;
        };
        if n == 0 {
            // 长度相同且 a 已读完:b 也该读完。
            return matches!(b.read(&mut buf_b[..1]), Ok(0));
        }
        if b.read_exact(&mut buf_b[..n]).is_err() || buf_a[..n] != buf_b[..n] {
            return false;
        }
    }
}

/// 跑一个子进程,至多等 `timeout`,超时结束它并报错。输出落临时文件而不是管道:管道写端被孙进程
/// 握着时,读到 EOF 可以无限期地等。各取前 64 KiB。
fn output_within(mut command: Command, timeout: Duration) -> Result<std::process::Output> {
    use std::io::{Read, Seek};
    let program = command.get_program().to_string_lossy().into_owned();
    let stdout = tempfile::tempfile().context(summon_i18n::text!("rfbe1665c87"))?;
    let stderr = tempfile::tempfile().context(summon_i18n::text!("rfbe1665c87"))?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()
        .with_context(|| summon_i18n::format!("r5dbd00aa78"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // 恰好在这之间退出的话 kill 会报错,可忽略;wait 只为回收。
            let _ = child.kill();
            let _ = child.wait();
            let secs = timeout.as_secs_f32();
            bail!(summon_i18n::format!("r83a54936a6"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read = |mut file: std::fs::File| -> Result<Vec<u8>> {
        file.rewind()?;
        let mut bytes = Vec::new();
        file.take(64 * 1024).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    Ok(std::process::Output {
        status,
        stdout: read(stdout)?,
        stderr: read(stderr)?,
    })
}

/// 暂存那份自报的构建戳。
pub(crate) fn reported_build(exe: &Path, timeout: Duration) -> Result<BuildInfo> {
    let mut command = Command::new(exe);
    command.args(["install-stamp", "print"]);
    let output =
        output_within(command, timeout).with_context(|| summon_i18n::text!("rd56b678d59"))?;
    let status = output.status;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    anyhow::ensure!(status.success(), summon_i18n::format!("r56da1a1a8c"));
    release::parse_reported(&output.stdout)
        .ok_or_else(|| anyhow::anyhow!(summon_i18n::text!("ra6ca30fb57")))
}

/// macOS:三件套的代码签名都要过 `codesign --verify --strict`,签名团队一致;当前在跑的这份有团队时
/// 新包的团队要与它相同,没有团队(源码自建的 ad-hoc)时把新包的团队打印出来。
fn check_signatures(
    codesign: &Codesign,
    pkg: &Path,
    platform: &str,
    out: &mut dyn Write,
) -> Result<()> {
    let names = release::package_files(platform).unwrap_or_default();
    let mut teams = Vec::new();
    for &name in names {
        let path = pkg.join(name);
        let mut verify = Command::new(&codesign.tool);
        verify.args(["--verify", "--strict"]).arg(&path);
        let output = output_within(verify, CODESIGN_TIMEOUT)?;
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::ensure!(output.status.success(), summon_i18n::format!("reac38568fe"));
        teams.push((name, team_of(codesign, &path)?));
    }
    let current = team_of(codesign, &codesign.current)?;
    let team = release::check_teams(current.as_deref(), &teams)?;
    match current {
        Some(_) => writeln!(out, "{}", summon_i18n::format!("r423fd5ae9c"))?,
        None => writeln!(out, "{}", summon_i18n::format!("r0f4c1dec27"))?,
    }
    Ok(())
}

/// 一个文件的签名团队;没签名或 ad-hoc 签名为空。
fn team_of(codesign: &Codesign, path: &Path) -> Result<Option<String>> {
    let mut details = Command::new(&codesign.tool);
    details.arg("-dv").arg(path);
    let output = output_within(details, CODESIGN_TIMEOUT)?;
    Ok(release::team_identifier(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

#[cfg(test)]
pub(crate) mod tests;
