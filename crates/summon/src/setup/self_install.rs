//! 把当前这份 summon 及随身二进制发布到稳定落点 `~/.summon/bin/`。
//!
//! 开机自启单元里只能写死一个路径,登记的就是落点。所以启动 daemon(与开自启)时先把当前这份
//! 发布进落点,登记的那份就总是最新的;别处装的 summon 也经这一步收敛到落点。
//!
//! **绝不拿旧的覆盖新的**:落点里可能已经是更新的那份(别的安装渠道刚装过)。发布前先认出落点
//! 那份的构建(判据见 `summon_core::install_stamp`):落点更新、或判不出新旧,都不碰它,daemon 用
//! 落点那份起(自启单元登记的就是它)。发布后在落点旁写构建戳,下一次发布据此比新旧。
//!
//! **有 daemon 在跑时不换落点里已有的文件**:跑着的 daemon 每次重启 sidecar(崩溃、agent 热加载)
//! 都按路径重新 exec 落点里的 summon-sidecar,而协议版本不同的 sidecar 会被它拒掉 —— 这时换掉
//! 落点,飞书长连接可能在下一次重启 sidecar 时断开,直到 daemon 也换代。所以这时只记下「该发布」,
//! 留给下一次 `daemon start`(那一刻没有 daemon 在跑)。

use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use summon_core::companion;
use summon_core::install_stamp::{self, BuildInfo, InstallStamp, Installed, Plan};

/// 问落点那份 `--version` 的总时限。它只在没有可信构建戳时才问,正常就是几十毫秒。
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// 一次自发布的结果。
#[derive(Debug)]
pub struct Outcome {
    /// 落点里的 summon —— 调用方拿它去 spawn / 登记自启。
    pub target: PathBuf,
    /// 这次发布进落点的构建;没换落点时为空。
    pub published: Option<BuildInfo>,
    /// 要告诉人的事:为什么没换落点(落点更新、判不出、daemon 在跑),或构建戳没写成。
    pub notice: Option<String>,
}

impl Outcome {
    pub(crate) fn untouched(target: PathBuf, notice: Option<String>) -> Self {
        Self {
            target,
            published: None,
            notice,
        }
    }
}

/// 运行期自发布,返回落点路径。落点认不出构建(起不来、非零退出、输出认不出)或安装源不完整时
/// 返回错误、不碰落点,调用方这一程用当前这份。
pub fn publish_current_exe() -> Result<Outcome> {
    let exe = current_exe()?;
    let home = dirs::home_dir().context(summon_i18n::text!("rc086b7ca57"))?;
    publish_for(
        &exe,
        &home,
        &crate::version::build_info(),
        &current_daemon(),
    )
}

/// 此刻 daemon 的状态。在跑时带上它自报的版本(读不出为空):推迟发布的提示要说它还在跑哪一版。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Daemon {
    Stopped,
    Running(Option<String>),
}

/// [`publish_current_exe`] 的编排:当前这份不在落点就把自己发布进去;就是落点时交给
/// [`refresh_install`]。
fn publish_for(exe: &Path, home: &Path, mine: &BuildInfo, daemon: &Daemon) -> Result<Outcome> {
    if companion::needs_install(exe, home) {
        return publish_from(exe, home, mine, daemon);
    }
    refresh_install(home, mine, daemon)
}

/// 当前这份就是落点:带 npm 分发渠道的构建去看 PATH 上的 npm 包是否严格更新(见
/// `setup::npm_dist`)。
#[cfg(npm_dist)]
fn refresh_install(home: &Path, mine: &BuildInfo, daemon: &Daemon) -> Result<Outcome> {
    crate::setup::npm_dist::pull_newer(home, mine, daemon)
}

/// 当前这份就是落点:没有别的渠道可比,落点原样。
#[cfg(not(npm_dist))]
fn refresh_install(home: &Path, _mine: &BuildInfo, _daemon: &Daemon) -> Result<Outcome> {
    Ok(Outcome::untouched(companion::install_target(home), None))
}

/// 此刻 daemon 的状态。锁的状态读不出时按「在跑」算:宁可这次不发布,也不在 daemon 底下换文件。
pub(crate) fn current_daemon() -> Daemon {
    match crate::runtime::daemon::running_pid() {
        Some(pid) => Daemon::Running(crate::runtime::daemon::daemon_version(pid)),
        None if crate::runtime::daemon::daemon_lock_held().unwrap_or(true) => Daemon::Running(None),
        None => Daemon::Stopped,
    }
}

fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context(summon_i18n::text!("r3f14f2b415"))?;
    // 软链不解引用会把 `~/.local/bin/summon` 判成「不是落点」,于是每次启动都白发一遍。
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// 把 `exe` 所在安装源的三件套发布进落点(落点那份不比它新、且没有 daemon 底下的文件要换时)。
/// 存在性、内容与构建戳都走真实文件系统,不 mock;`build` 是 `exe` 的构建信息。
pub fn publish_from(
    exe: &Path,
    home: &Path,
    build: &BuildInfo,
    daemon: &Daemon,
) -> Result<Outcome> {
    if !companion::needs_install(exe, home) {
        return Ok(Outcome::untouched(companion::install_target(home), None));
    }
    // 先于新旧比较拒掉不完整的源,免得为它去问落点的 `--version`。
    complete_source(exe, home)?;
    publish_over(exe, home, build, installed_build(home), daemon)
}

/// 安装源所在目录;sidecar 缺失就整体放弃:落点可能留着上一版的 sidecar,只换 summon 会配出
/// 跨版本的一对。`cargo run` 直接跑 `target/release/summon` 就是这种不完整的源 —— 它不是安装
/// 渠道,开发构建用自己起。
fn complete_source<'a>(exe: &'a Path, home: &Path) -> Result<&'a Path> {
    let src_dir = exe.parent().context(summon_i18n::text!("r5a08e8d4ac"))?;
    anyhow::ensure!(
        src_dir.join(companion::SIDECAR).is_file(),
        summon_i18n::format!(
            "rc217c135c2",
            exe.display(),
            companion::SIDECAR,
            companion::install_target(home).display()
        )
    );
    Ok(src_dir)
}

/// [`publish_from`] 的本体,落点那份的构建由调用方给出。
pub(crate) fn publish_over(
    exe: &Path,
    home: &Path,
    build: &BuildInfo,
    installed: Installed,
    daemon: &Daemon,
) -> Result<Outcome> {
    let target = companion::install_target(home);
    let src_dir = complete_source(exe, home)?;
    // 换代后退回过的版本不发布进落点;落点为空时不挡 —— 没有别的可起,挡住只会让机器没有 daemon。
    if let Some(notice) = (installed != Installed::Absent)
        .then(|| crate::setup::auto_upgrade::refuses_publish(home, &build.version))
        .flatten()
    {
        return Ok(Outcome::untouched(target, Some(notice)));
    }
    match install_stamp::plan(build, &installed) {
        Plan::Publish => {}
        Plan::KeepInstalled => {
            let notice = summon_i18n::format!(
                "r501f4f60ff",
                target.display(),
                describe_installed(&installed),
                build.describe()
            );
            return Ok(Outcome::untouched(target, Some(notice)));
        }
        Plan::Undecided => {
            // 这句也会打到别的渠道装的用户眼前,他们手里不一定有 deploy.sh:只给哪条渠道都成立的办法。
            // 没答出构建多半只是慢(首次执行被系统扫描、机器忙),先建议重试,别让人去删一个好文件。
            let retry = if matches!(installed, Installed::Unanswered(_)) {
                summon_i18n::text!("rdcee597429")
            } else {
                ""
            };
            let notice = summon_i18n::format!(
                "raac805ecaf",
                target.display(),
                describe_installed(&installed),
                build.describe(),
                target.display()
            );
            return Ok(Outcome::untouched(target, Some(notice)));
        }
        Plan::Unusable => anyhow::bail!(summon_i18n::format!(
            "r417fc6dd05",
            target.display(),
            describe_installed(&installed),
            build.describe()
        )),
    }
    // daemon 在跑时不换落点里已有的文件:它重启 sidecar 时按路径起落点那份(见模块文档),
    // 留给下一次 daemon start。落点就是这一份时没有要换的,不必提示。
    if let (Daemon::Running(version), false) = (daemon, installed == Installed::Absent) {
        if install_stamp::relation(build, &installed) == install_stamp::Relation::Same {
            return Ok(Outcome::untouched(target, None));
        }
        let running = version.as_deref().map_or_else(
            || summon_i18n::text!("r04ea81768a").to_string(),
            |v| summon_i18n::format!("r46078edab3"),
        );
        let notice = summon_i18n::format!("r2833bc2067", build.describe());
        return Ok(Outcome::untouched(target, Some(notice)));
    }
    let dir = companion::install_dir(home);
    std::fs::create_dir_all(&dir)
        .with_context(|| summon_i18n::format!("rc18f37e5e1", dir.display()))?;
    for name in companion::TRIO {
        let src = if name == companion::SUMMON {
            exe.to_path_buf()
        } else {
            src_dir.join(name)
        };
        // menubar 只在 macOS 上有,缺了不影响 daemon。
        if !src.is_file() {
            continue;
        }
        publish_one(&src, &dir.join(name))?;
    }
    // 三件套已经就位,戳写不成只让下一次发布退回「判不出」,不该把这次发布算作失败。
    let notice = write_stamp(home, build)
        .err()
        .map(|e| summon_i18n::format!("ra80e534192"));
    Ok(Outcome {
        target,
        published: Some(build.clone()),
        notice,
    })
}

/// 落点里那份的构建。先信构建戳(必须证明描述的就是落点那个文件);没有可信的戳 —— 早于构建戳
/// 的版本装的、被别的途径换过 —— 再问它 `--version`。后者要起进程,只在运行期发布与 deploy.sh
/// 比较这类本就要起进程的路径上做。
pub fn installed_build(home: &Path) -> Installed {
    installed_build_within(home, VERSION_PROBE_TIMEOUT)
}

fn installed_build_within(home: &Path, probe_timeout: Duration) -> Installed {
    let target = companion::install_target(home);
    let meta = match std::fs::metadata(&target) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Installed::Absent,
        Err(e) => return Installed::Unreadable(summon_i18n::format!("r137be82d7a")),
    };
    if !meta.is_file() {
        return Installed::Unreadable(summon_i18n::text!("rc9faeca897").to_string());
    }
    if let Some(build) = read_stamp(home).and_then(|stamp| stamp.build_of(&meta).cloned()) {
        return Installed::Known(build);
    }
    match version_of(&target, probe_timeout) {
        Ok(build) => Installed::Known(build),
        Err(Probe::TimedOut(why)) => Installed::Unanswered(why),
        Err(Probe::Failed(e)) => Installed::Unreadable(format!("{e:#}")),
    }
}

/// 问 `--version` 没问出构建的两种情形,结论不同(见 [`Installed::Unanswered`])。
enum Probe {
    TimedOut(String),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for Probe {
    fn from(e: anyhow::Error) -> Self {
        Probe::Failed(e)
    }
}

impl From<std::io::Error> for Probe {
    fn from(e: std::io::Error) -> Self {
        Probe::Failed(e.into())
    }
}

fn describe_installed(installed: &Installed) -> String {
    match installed {
        Installed::Absent => summon_i18n::text!("r6516ee0da8").to_string(),
        Installed::Known(build) => build.describe(),
        Installed::Unanswered(why) => summon_i18n::format!("r05f2c34d06"),
        Installed::Unreadable(why) => summon_i18n::format!("r76fa69e603"),
    }
}

fn read_stamp(home: &Path) -> Option<InstallStamp> {
    let path = companion::install_dir(home).join(install_stamp::STAMP_FILE);
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// 在落点旁写构建戳,描述**此刻**落点里的 summon。暂存 + rename:读的一方随时可能在读,不能读到半截。
fn write_stamp(home: &Path, build: &BuildInfo) -> Result<()> {
    let target = companion::install_target(home);
    let meta = std::fs::metadata(&target)
        .with_context(|| summon_i18n::format!("re43971131a", target.display()))?;
    let mut text = serde_json::to_string(&InstallStamp::new(build.clone(), &meta))?;
    text.push('\n');
    let dir = companion::install_dir(home);
    let staged = tempfile::Builder::new()
        .prefix(&format!(".{}.new.", install_stamp::STAMP_FILE))
        .tempfile_in(&dir)
        .with_context(|| summon_i18n::format!("re40061e385", dir.display()))?;
    std::fs::write(staged.path(), text).context(summon_i18n::text!("r3e3f7775b5"))?;
    let dst = dir.join(install_stamp::STAMP_FILE);
    staged.persist(&dst).map_err(|e| {
        anyhow::anyhow!(summon_i18n::format!("r4cb405ae19", dst.display(), e.error))
    })?;
    Ok(())
}

/// 问一份 summon 的 `--version`。有总时限:落点那份坏掉卡住时,不能把 `daemon start` 一起挂住。
/// 输出落临时文件而不是管道 —— 管道写端被孙进程握着时,读到 EOF 可以无限期地等。
fn version_of(exe: &Path, timeout: Duration) -> std::result::Result<BuildInfo, Probe> {
    use std::io::{Read, Seek};
    let mut out = tempfile::tempfile().context(summon_i18n::text!("rfbe1665c87"))?;
    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| summon_i18n::format!("r6eb0b528ed", exe.display()))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // 恰好在这之间退出的话 kill 会报错,可忽略;wait 只为回收。
            let _ = child.kill();
            let _ = child.wait();
            return Err(Probe::TimedOut(summon_i18n::format!(
                "rbfc78710ac",
                exe.display(),
                timeout.as_secs_f32()
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return Err(anyhow::anyhow!(summon_i18n::format!("r9ca30823b5", exe.display())).into());
    }
    out.rewind()?;
    let mut bytes = Vec::new();
    out.take(4096).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(install_stamp::parse_version_output(&text)
        .with_context(|| summon_i18n::format!("re4daad9021", exe.display(), text.trim()))?)
}

/// 落点那份登记自己的构建戳 —— deploy.sh 把新 CLI 发布进落点后调用。只有落点里的 summon
/// 有资格描述落点:别的二进制写进来的戳,说的是它自己,不是落点那个文件。
pub fn record_current_exe() -> Result<PathBuf> {
    let home = dirs::home_dir().context(summon_i18n::text!("rc086b7ca57"))?;
    record_from(&current_exe()?, &home, &crate::version::build_info())
}

/// [`record_current_exe`] 的可注入版本。
pub fn record_from(exe: &Path, home: &Path, mine: &BuildInfo) -> Result<PathBuf> {
    let target = companion::install_target(home);
    // HOME 带软链(macOS 的 /var → /private/var)时,落点也要解引用后再和 exe 比。
    let resolved = target.canonicalize().unwrap_or_else(|_| target.clone());
    anyhow::ensure!(
        exe == resolved,
        summon_i18n::format!("r475b25dbcc", exe.display(), target.display())
    );
    write_stamp(home, mine)?;
    Ok(target)
}

/// 当前这份相对落点那份,一行:`<newer|same|older|unknown|absent> <说明>`。deploy.sh 发布前
/// 调用、按第一个词分支;只读,不写任何东西。
pub fn check_current_exe() -> Result<String> {
    let home = dirs::home_dir().context(summon_i18n::text!("rc086b7ca57"))?;
    Ok(check_line(
        &crate::version::build_info(),
        &installed_build(&home),
    ))
}

fn check_line(mine: &BuildInfo, installed: &Installed) -> String {
    summon_i18n::format!(
        "ra82c855c86",
        install_stamp::relation(mine, installed).word(),
        mine.describe(),
        describe_installed(installed)
    )
}

/// 暂存 + rename 发布一个文件。**绝不就地覆盖**:落点那份可能正被跑着的 daemon 用着,
/// 换 inode 才不会打断它。
fn publish_one(src: &Path, dst: &Path) -> Result<()> {
    let parent = dst.parent().context(summon_i18n::text!("r0a7b4845d8"))?;
    let name = dst.file_name().and_then(|n| n.to_str()).unwrap_or("summon");
    let staged = tempfile::Builder::new()
        .prefix(&format!(".{name}.new."))
        .tempfile_in(parent)
        .with_context(|| summon_i18n::format!("re40061e385", parent.display()))?;
    std::fs::copy(src, staged.path())
        .with_context(|| summon_i18n::format!("ra3a896893e", src.display()))?;
    std::fs::set_permissions(staged.path(), std::fs::Permissions::from_mode(0o755))
        .context(summon_i18n::text!("r3a259bdfba"))?;
    staged.persist(dst).map_err(|e| {
        anyhow::anyhow!(summon_i18n::format!("r4cb405ae19", dst.display(), e.error))
    })?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 发布方自报的构建:与下面 `older` / `newer` 同号,只差提交 —— 正是两条渠道同号不同提交
    /// 的那种局面。
    pub(crate) fn mine() -> BuildInfo {
        build("0.6.9", "48698ad7", Some(2_000))
    }

    pub(crate) fn build(version: &str, commit: &str, commit_time: Option<u64>) -> BuildInfo {
        BuildInfo {
            version: version.to_string(),
            commit: commit.to_string(),
            commit_time,
        }
    }

    pub(crate) fn running() -> Daemon {
        Daemon::Running(Some("0.6.0".to_string()))
    }

    pub(crate) fn older() -> BuildInfo {
        build("0.6.9", "a5b51aca", Some(1_000))
    }

    pub(crate) fn newer() -> BuildInfo {
        build("0.6.9", "c0ffee12", Some(3_000))
    }

    /// 造一个「完整的安装源」:三件套并排,内容各自带上标记好认。
    pub(crate) fn source_dir(root: &Path, mark: &str) -> PathBuf {
        let dir = root.join(format!("src-{mark}"));
        std::fs::create_dir_all(&dir).unwrap();
        for name in companion::TRIO {
            std::fs::write(dir.join(name), format!("{name}-{mark}")).unwrap();
        }
        dir
    }

    /// 落点里已经装着一份(summon + sidecar),并带着描述它的构建戳。
    pub(crate) fn installed_as(home: &Path, build: &BuildInfo) -> PathBuf {
        let dir = companion::install_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(companion::SUMMON), "summon-落点").unwrap();
        std::fs::write(dir.join(companion::SIDECAR), "sidecar-落点").unwrap();
        write_stamp(home, build).unwrap();
        dir
    }

    /// 一个只会打 `--version` 的假 summon —— 早于构建戳的版本就是这样被认出来的。
    pub(crate) fn fake_summon(path: &Path, script: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::test_support::write_test_executable(path, &format!("#!/bin/sh\n{script}\n"));
    }

    pub(crate) fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn publishes_the_whole_trio_into_home_summon_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let src = source_dir(tmp.path(), "new");

        let outcome = publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();

        assert_eq!(outcome.target, companion::install_target(&home));
        assert!(outcome.notice.is_none(), "{:?}", outcome.notice);
        for name in companion::TRIO {
            assert_eq!(
                read(&companion::install_dir(&home).join(name)),
                format!("{name}-new")
            );
        }
        assert_eq!(installed_build(&home), Installed::Known(mine()));
    }

    #[test]
    fn published_binaries_are_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let src = source_dir(tmp.path(), "new");

        publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();

        let mode = std::fs::metadata(companion::install_target(&home))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "发布出去的二进制必须可执行");
    }

    /// 落点那份正被跑着的 daemon 用着,所以发布只能换 inode,不能就地改内容。
    #[test]
    fn replaces_the_inode_instead_of_writing_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let dir = installed_as(&home, &older());
        let old = dir.join(companion::SUMMON);
        // 硬链接钉住旧 inode:就地覆盖的话它会跟着变。
        let witness = tmp.path().join("witness");
        std::fs::hard_link(&old, &witness).unwrap();

        let src = source_dir(tmp.path(), "新版");
        publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();

        assert_eq!(read(&old), "summon-新版");
        assert_eq!(read(&witness), "summon-落点", "旧 inode 被就地改写了");
    }

    /// 同号不同提交:落点那份提交得更晚,它就是更新的,运行期发布绝不覆盖它,daemon 改用它起。
    #[test]
    fn a_newer_install_is_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let dir = installed_as(&home, &newer());
        let src = source_dir(tmp.path(), "new");

        let outcome = publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();

        assert_eq!(outcome.target, companion::install_target(&home));
        let notice = outcome.notice.expect("没覆盖落点要说一声");
        assert!(notice.contains("c0ffee12"), "{notice}");
        assert_eq!(read(&dir.join(companion::SUMMON)), "summon-落点");
        assert_eq!(read(&dir.join(companion::SIDECAR)), "sidecar-落点");
        assert!(!dir.join(companion::MENUBAR).exists(), "随身二进制也不能动");
        assert_eq!(installed_build(&home), Installed::Known(newer()));
    }

    /// 同号不同提交:当前这份提交得更晚,照发,并把戳换成描述新那份。
    #[test]
    fn an_older_install_is_replaced_and_restamped() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let dir = installed_as(&home, &older());
        let src = source_dir(tmp.path(), "new");

        let outcome = publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();

        assert!(outcome.notice.is_none(), "{:?}", outcome.notice);
        assert_eq!(read(&dir.join(companion::SUMMON)), "summon-new");
        assert_eq!(read(&dir.join(companion::SIDECAR)), "summon-sidecar-new");
        assert_eq!(installed_build(&home), Installed::Known(mine()));
    }

    /// 早于构建戳的版本装的落点没有戳,就问它 `--version`:版本号分得出高低就照判。
    #[test]
    fn an_unstamped_install_is_judged_by_its_version_output() {
        let tmp = tempfile::tempdir().unwrap();
        let src = source_dir(tmp.path(), "new");

        let home = tmp.path().join("old-home");
        let target = companion::install_target(&home);
        fake_summon(&target, "echo 'summon 0.6.8 (a5b51aca)'");
        publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();
        assert_eq!(read(&target), "summon-new", "更低的版本号该被换掉");

        let home = tmp.path().join("new-home");
        let target = companion::install_target(&home);
        fake_summon(&target, "echo 'summon 0.7.0 (a5b51aca)'");
        let outcome = publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();
        assert!(outcome.notice.is_some());
        assert!(read(&target).contains("0.7.0"), "更高的版本号不能被覆盖");
    }

    /// 判不出新旧(同号不同提交而落点是不带提交时间的老版本 —— deploy.sh 回滚、早于构建戳的
    /// 部署都会留下这种落点):不覆盖它,也照样交出它,daemon 经自启单元用它起;并说清怎么处理。
    #[test]
    fn an_undecidable_install_is_kept_and_still_used() {
        let tmp = tempfile::tempdir().unwrap();
        let src = source_dir(tmp.path(), "new");
        let home = tmp.path().join("home");
        let target = companion::install_target(&home);
        fake_summon(&target, "echo 'summon 0.6.9 (a5b51aca)'");

        let outcome = publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();

        assert_eq!(outcome.target, target);
        assert_eq!(outcome.published, None);
        let notice = outcome.notice.expect("判不出要说一声");
        assert!(notice.contains("判不出"), "{notice}");
        assert!(notice.contains("删掉"), "要说怎么处理:{notice}");
        assert!(
            !notice.contains("deploy.sh"),
            "这句也会打到别的渠道装的用户眼前,他们不一定有 deploy.sh:{notice}"
        );
        assert!(read(&target).contains("a5b51aca"), "落点被覆盖了");
        assert!(!companion::install_dir(&home)
            .join(companion::SIDECAR)
            .exists());
    }

    /// 落点有文件却认不出它的构建(起不来):不覆盖,也不交出它 —— 调用方这一程用当前这份。
    #[test]
    fn an_unusable_install_is_neither_replaced_nor_used() {
        let tmp = tempfile::tempdir().unwrap();
        let src = source_dir(tmp.path(), "new");
        let home = tmp.path().join("home");
        let target = companion::install_target(&home);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "不是可执行文件").unwrap();

        let err = publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap_err();

        assert!(err.to_string().contains("不覆盖它也不用它"), "{err:#}");
        assert_eq!(read(&target), "不是可执行文件");
    }

    /// 有 daemon 在跑时不换落点里已有的文件(它重启 sidecar 时按路径起落点那份);落点还空着时
    /// 照发 —— 那个 daemon 不是从落点起的,换不到它脚下。
    #[test]
    fn a_running_daemon_defers_replacing_an_existing_install() {
        let tmp = tempfile::tempdir().unwrap();
        let src = source_dir(tmp.path(), "new");

        let home = tmp.path().join("installed");
        let dir = installed_as(&home, &older());
        let outcome =
            publish_from(&src.join(companion::SUMMON), &home, &mine(), &running()).unwrap();
        assert_eq!(outcome.published, None);
        let notice = outcome.notice.expect("推迟要说一声");
        assert!(notice.contains(&mine().describe()), "{notice}");
        assert!(notice.contains("仍在运行旧版 0.6.0"), "{notice}");
        assert!(notice.contains("`summon upgrade`"), "{notice}");
        assert!(notice.contains("`summon daemon restart`"), "{notice}");
        assert_eq!(read(&dir.join(companion::SUMMON)), "summon-落点");
        assert_eq!(read(&dir.join(companion::SIDECAR)), "sidecar-落点");

        let home = tmp.path().join("empty");
        let outcome =
            publish_from(&src.join(companion::SUMMON), &home, &mine(), &running()).unwrap();
        assert_eq!(outcome.published, Some(mine()));
        assert_eq!(read(&companion::install_target(&home)), "summon-new");

        // daemon 报不出版本时照样提示;落点已是这一份时没有要换的,不提示。
        let home = tmp.path().join("unversioned");
        installed_as(&home, &older());
        let unversioned = Daemon::Running(None);
        let outcome =
            publish_from(&src.join(companion::SUMMON), &home, &mine(), &unversioned).unwrap();
        let notice = outcome.notice.expect("推迟要说一声");
        assert!(notice.contains("仍在运行旧版:"), "{notice}");

        let home = tmp.path().join("same");
        let dir = installed_as(&home, &mine());
        let outcome =
            publish_from(&src.join(companion::SUMMON), &home, &mine(), &running()).unwrap();
        assert_eq!(outcome.published, None);
        assert_eq!(outcome.notice, None);
        assert_eq!(read(&dir.join(companion::SUMMON)), "summon-落点");
    }

    /// 当前这份不在落点(别处装的、新构建的)就把自己发布进去,不论落点是不是正在跑的那份。
    #[test]
    fn an_exe_outside_the_install_publishes_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        installed_as(&home, &older());
        let src = source_dir(tmp.path(), "new");

        let outcome = publish_for(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();

        assert_eq!(outcome.published, Some(mine()));
        assert_eq!(read(&companion::install_target(&home)), "summon-new");
    }

    /// 戳只为写它那一刻的文件作证:落点被别的途径换过之后,戳说「落点更新」也不算数,
    /// 改问换进来的那份。
    #[test]
    fn a_stamp_for_a_replaced_file_is_not_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        installed_as(&home, &newer());
        let target = companion::install_target(&home);
        let swapped = tmp.path().join("swapped");
        fake_summon(&swapped, "echo 'summon 0.6.8 (a5b51aca)'");
        std::fs::rename(&swapped, &target).unwrap();
        let src = source_dir(tmp.path(), "new");

        let outcome = publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();

        assert!(outcome.notice.is_none(), "{:?}", outcome.notice);
        assert_eq!(read(&target), "summon-new");
    }

    #[test]
    fn the_version_probe_has_a_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let stuck = tmp.path().join("summon");
        fake_summon(&stuck, "sleep 30");

        let started = Instant::now();
        let Err(Probe::TimedOut(why)) = version_of(&stuck, Duration::from_millis(200)) else {
            panic!("超时要单独报出来");
        };

        assert!(why.contains("未返回"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(5), "等待不得无界");
    }

    /// 落点问 `--version` 超时(刚拷进来首次执行被扫描、机器负载高)按「判不出」处理:不覆盖,
    /// 照样交出落点、经自启单元起 —— 慢不等于坏,不能因此丢掉一程托管。
    #[test]
    fn a_slow_install_is_kept_and_still_used() {
        let tmp = tempfile::tempdir().unwrap();
        let src = source_dir(tmp.path(), "new");
        let home = tmp.path().join("home");
        let target = companion::install_target(&home);
        fake_summon(&target, "sleep 30");

        let installed = installed_build_within(&home, Duration::from_millis(200));
        assert!(
            matches!(installed, Installed::Unanswered(_)),
            "{installed:?}"
        );
        let outcome = publish_over(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            installed,
            &Daemon::Stopped,
        )
        .unwrap();

        assert_eq!(outcome.target, target);
        assert_eq!(outcome.published, None);
        let notice = outcome.notice.unwrap();
        assert!(notice.contains("没答出构建"), "{notice}");
        assert!(notice.contains("再运行一次"), "{notice}");
        assert!(read(&target).contains("sleep 30"), "落点被覆盖了");
    }

    #[test]
    fn a_source_without_sidecar_publishes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let dir = companion::install_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(companion::SUMMON), "summon-旧版").unwrap();
        std::fs::write(dir.join(companion::SIDECAR), "sidecar-旧版").unwrap();
        // 开发路径的形状:target/release 下只有 summon 一个。
        let lone = tmp.path().join("target/release");
        std::fs::create_dir_all(&lone).unwrap();
        std::fs::write(lone.join(companion::SUMMON), "summon-新版").unwrap();

        let err = publish_from(
            &lone.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap_err();

        assert!(err.to_string().contains(companion::SIDECAR));
        assert_eq!(read(&dir.join(companion::SUMMON)), "summon-旧版");
        assert_eq!(read(&dir.join(companion::SIDECAR)), "sidecar-旧版");
    }

    /// 落点那份自己启动时不该再发布一遍 —— 那是 launchd 与 deploy.sh 的常态路径。
    #[test]
    fn the_target_itself_is_left_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let dir = companion::install_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let target = companion::install_target(&home);
        std::fs::write(&target, "summon-在位").unwrap();
        let before = std::fs::metadata(&target).unwrap().modified().unwrap();

        let outcome = publish_from(&target, &home, &mine(), &Daemon::Stopped).unwrap();

        assert_eq!(outcome.target, target);
        assert_eq!(read(&target), "summon-在位");
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            before
        );
    }

    /// menubar 只在 macOS 上构建,linux 的安装源里没有它 —— 缺了要照发不误。
    #[test]
    fn a_source_without_menubar_still_publishes_summon_and_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("linux-pkg");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(companion::SUMMON), "summon-linux").unwrap();
        std::fs::write(src.join(companion::SIDECAR), "sidecar-linux").unwrap();

        publish_from(
            &src.join(companion::SUMMON),
            &home,
            &mine(),
            &Daemon::Stopped,
        )
        .unwrap();

        let dir = companion::install_dir(&home);
        assert_eq!(read(&dir.join(companion::SUMMON)), "summon-linux");
        assert_eq!(read(&dir.join(companion::SIDECAR)), "sidecar-linux");
        assert!(!dir.join(companion::MENUBAR).exists());
    }

    /// 只有落点里的那份能登记落点:别的二进制写进来的戳,描述的是它自己。
    #[test]
    fn only_the_install_itself_may_record_the_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let target = companion::install_target(&home);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "summon-deploy").unwrap();
        let elsewhere = tmp.path().join("summon");
        std::fs::write(&elsewhere, "summon-别处").unwrap();

        assert!(record_from(&elsewhere, &home, &mine()).is_err());
        assert!(!companion::install_dir(&home)
            .join(install_stamp::STAMP_FILE)
            .exists());

        record_from(&target.canonicalize().unwrap(), &home, &mine()).unwrap();
        assert_eq!(installed_build(&home), Installed::Known(mine()));
    }

    /// deploy.sh 按第一个词分支(`older` 时提示),词表改了脚本就得跟着改。
    #[test]
    fn the_check_line_starts_with_the_word_deploy_branches_on() {
        let line = check_line(&mine(), &Installed::Known(newer()));
        assert!(line.starts_with("older "), "{line}");
        let line = check_line(&mine(), &Installed::Absent);
        assert!(line.starts_with("absent "), "{line}");
    }
}
