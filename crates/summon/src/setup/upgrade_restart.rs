//! `summon upgrade` 各渠道共用的「用新的那份让 daemon 换代」:升级锁、脱离当前会话的重启段、观察者
//! 的结论。新版从哪来(npm 包、GitHub Release)由各渠道自己负责,换代这一段只有一份。
//!
//! **重启那一段无条件脱离当前会话**:daemon 停机会 `killpg(SIGINT)` 在跑的无头轮。由 codex /
//! cursor agent 在自己的轮里执行时,不脱离就会被自己发起的停机打死,「启动」那一半永远跑不到,
//! 机器上从此没有 daemon。脱离出去的那段在新会话里跑完,本进程只跟随它的日志,被打断也不影响它。
//! 判据不看环境变量:codex 的子进程不继承 summon 注入的环境(同 `scripts/deploy.sh`)。
//!
//! **停机不可撤销**:新入口 `daemon restart` 的启动那一半失败时(就绪截止、新文件首次执行的系统
//! 评估、自启托管超时、新版起来即崩),机器上已经没有 daemon,而发起者多半已随停机被打断。所以脱离
//! 出去的那段自己负责把 daemon 拉回来,与 deploy.sh 的 `restore_daemon` 同一做法。
//!
//! 新版换进落点不在这里:新入口的 `daemon restart` 停机之后、启动之前经运行期自发布把自己的三件套
//! 发布进落点(那一刻没有 daemon 在跑,见 `self_install`),再用落点那份起。

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use summon_core::install_stamp::BuildInfo;

/// 钉住落点:设了它的那次 `daemon start` 用落点里那份起,不去别的安装源里找更新的(带别的安装
/// 渠道的构建才会去找)。deploy.sh、换代失败后的兜底拉起与自动升级的退回都设它。只作用于这一次
/// CLI 调用,`daemon start` 不把它传给 daemon。
pub(crate) const PIN_ENV: &str = "SUMMON_PIN_INSTALL";

/// 启动失败后等多久再用落点那份起一次:慢启动被就绪截止判败、首次执行排系统评估,隔几秒多半就好。
pub(crate) const RESTORE_DELAY_SECS: u32 = 3;

/// [`RESTART_SCRIPT`] 在 daemon 已换代、只有 Menu Bar 没重启成时的退出码。
pub(crate) const MENUBAR_FAILED: i32 = 10;

/// 脱离出去的那段。只用公开命令:新入口的 `daemon restart`(停机之后、启动之前把自己发布进落点),
/// 落点那份的 `menubar restart`(LaunchAgent 登记的是落点路径)、`daemon status` 与兜底的
/// `daemon start`。fd 0 进来时是升级锁:挪到 fd 3 由本脚本持有到结束,子命令一律不继承。
/// 最后一行总是 `daemon status`,观察者据它报 daemon 的实况。`daemon restart` 带免脱离标记
/// ([`RESTART_NO_DETACH_ENV`](crate::cli::cli::RESTART_NO_DETACH_ENV)),就在本段的进程组里重启:
/// 自动升级超时结束守望进程时是整组收掉,再脱离一层就够不着它了。
pub(crate) const RESTART_SCRIPT: &str = "r692fba9bab";

/// 脱离出去那段的日志,与 deploy.log 同目录。
pub(crate) fn log_path(home: &Path) -> PathBuf {
    home.join(".summon/run/upgrade.log")
}

/// 升级锁:同一时刻只有一次升级(不分渠道)。重启段继承它、跑完才释放,发起者先退出也挡得住第二次。
pub(crate) fn lock(home: &Path) -> Result<std::fs::File> {
    let path = home.join(".summon/run/upgrade.lock");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| summon_i18n::format!("rc18f37e5e1", dir.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| summon_i18n::format!("rf7f1f01aff", path.display()))?;
    file.try_lock().map_err(|error| {
        anyhow::anyhow!(summon_i18n::format!(
            "rb2a85ed090",
            log_path(home).display()
        ))
    })?;
    Ok(file)
}

/// `daemon status` 的最后一行报的版本(`running(pid N, version V)` 里的 V)。
pub(crate) fn status_version(log: &str) -> Option<&str> {
    log.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .strip_prefix("running(pid ")?
        .split_once(", version ")?
        .1
        .strip_suffix(')')
}

/// 装过 Menu Bar(LaunchAgent 的 plist 在):据此决定要不要重启它,与 deploy.sh 同一判据。
pub(crate) fn menubar_installed() -> bool {
    cfg!(target_os = "macos")
        && crate::setup::menubar::plist_path().is_ok_and(|path| path.is_file())
}

/// 一次换代的参数。
pub(crate) struct Restart<'a> {
    /// 新的那份 summon:用它的 `daemon restart` 换代。
    pub entry: &'a Path,
    /// 落点里的 summon:重启失败时兜底拉起、重启 Menu Bar、报 daemon 实况。
    pub install: &'a Path,
    pub home: &'a Path,
    pub menubar_installed: bool,
    /// 启动失败后再起一次之前等的秒数。
    pub restore_delay_secs: u32,
}

/// 在新会话里跑 [`RESTART_SCRIPT`],本进程跟随它的日志直到它结束;返回它的退出状态与完整日志。
/// 升级锁作为它的 stdin 交过去,跑完才释放。
pub(crate) fn restart_detached(
    restart: &Restart<'_>,
    lock: std::fs::File,
    out: &mut dyn Write,
) -> Result<(ExitStatus, String)> {
    let log = log_path(restart.home);
    let sink = std::fs::File::create(&log)
        .with_context(|| summon_i18n::format!("r88975b6189", log.display()))?;
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            ::summon_core::i18n::label(RESTART_SCRIPT),
            "summon-upgrade",
        ])
        .arg(restart.entry)
        .arg(if restart.menubar_installed { "1" } else { "0" })
        .arg(restart.install)
        .arg(restart.restore_delay_secs.to_string())
        .stdin(lock)
        .stdout(sink.try_clone()?)
        .stderr(sink);
    // SAFETY: fork 后只调用 async-signal-safe 的 setsid。
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    let child = command.spawn().context(summon_i18n::text!("rc55663f010"))?;
    writeln!(
        out,
        "{}",
        summon_i18n::format!("r1614c97f4e", log.display())
    )?;
    let reader = std::fs::File::open(&log)
        .with_context(|| summon_i18n::format!("rf53f6a49bc", log.display()))?;
    crate::cli::cli::follow_detached(child, reader, out)
}

/// 观察者的最终结论,据脱离那段的退出码与它最后一行 `daemon status` 如实报。
pub(crate) fn report_restart(
    status: ExitStatus,
    log: &str,
    new: &BuildInfo,
    home: &Path,
) -> Result<()> {
    let log_file = log_path(home);
    let running = status_version(log);
    match status.code() {
        Some(0) if running == Some(new.version.as_str()) => Ok(()),
        Some(0) => bail!(summon_i18n::format!(
            "r14f19349df",
            running.unwrap_or(summon_i18n::text!("rc729c18aa0")),
            new.version,
            log_file.display()
        )),
        Some(MENUBAR_FAILED) => bail!(summon_i18n::format!(
            "r87f8c449ce",
            running.unwrap_or(summon_i18n::text!("r92f46d1813")),
            log_file.display()
        )),
        _ => match running {
            Some(version) => bail!(summon_i18n::format!(
                "r663e9b3f9d",
                if version == new.version {
                    ""
                } else {
                    summon_i18n::text!("r05f7a07a74")
                },
                log_file.display()
            )),
            None => bail!(summon_i18n::format!("re7f065bd9e", log_file.display())),
        },
    }
}
