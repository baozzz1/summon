//! 开机自启(启动项)的 IO 侧:写/删单元文件、调平台命令、落「专属状态文件」。
//! 纯渲染 + 状态 schema 在 `summon_core::autostart`;这里只管落盘和进程副作用。
//!
//! 状态存 `~/.summon/run/autostart.json`(**不写 config.yaml**,后者手维护、有注释)。
//! 两个后端:macOS launchd LaunchAgent、Linux systemd `--user` service。

use crate::setup::launchd::{
    gui_domain, launchctl, service_present, service_state, service_target, wait_until_service_gone,
    ServiceState, BOOTOUT_TIMEOUT,
};
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::Command;
use summon_core::autostart::{
    plist_file_name, render_launchd_plist, render_systemd_unit, AutostartState, LoginItemBackend,
    LAUNCHD_LABEL, SYSTEMD_UNIT_NAME,
};

fn launch_agents_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents")
}
fn plist_path() -> PathBuf {
    launch_agents_dir().join(plist_file_name())
}
fn systemd_user_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config/systemd/user")
}
fn unit_path() -> PathBuf {
    systemd_user_dir().join(SYSTEMD_UNIT_NAME)
}
/// 本后端「装没装」的盘上事实:launchd 看 plist,systemd 看单元文件。
fn unit_file_installed() -> bool {
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => plist_path().exists(),
        LoginItemBackend::SystemdUser => unit_path().exists(),
    }
}
fn state_path() -> PathBuf {
    crate::runtime::paths::run_dir().join("autostart.json")
}
fn log_path() -> PathBuf {
    crate::runtime::paths::run_dir().join("daemon.log")
}
fn daemon_target() -> String {
    service_target(LAUNCHD_LABEL)
}

/// 读专属状态文件(缺 / 损坏 → 默认 both-false,即没开)。
pub fn read_state() -> AutostartState {
    read_state_at(&state_path())
}

fn read_state_at(p: &Path) -> AutostartState {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 原子写状态文件(tmp+rename)。
fn write_state_at(p: &Path, st: &AutostartState) -> anyhow::Result<()> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(st)?)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

/// 本后端单元文件的叫法,给人看的。两个平台的东西不同名,status 里说「plist」在 Linux 上是错的。
pub fn unit_kind_label() -> &'static str {
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => "plist",
        LoginItemBackend::SystemdUser => summon_i18n::text!("r875751c93b"),
    }
}

/// 当前状态:`(持久状态, 单元文件是否在盘上)`。CLI status / doctor / GUI 读它。
pub fn status() -> (AutostartState, bool) {
    (read_state(), unit_file_installed())
}

/// 单元文件登记的 summon 路径:**稳定落点** `~/.summon/bin/summon`。安装渠道不止一条(安装包、
/// `deploy.sh`),各自的路径互不相干,而单元里只能写死一个 —— 登记谁的路径,别的渠道更新了就
/// 拉不起来。落点由 `self_install` 在每次启动 daemon 时刷新,登记它哪条渠道都算数。
///
/// 落点还不在盘上(自安装失败,或从没走过 daemon 启动)时退回当前这份:自启能起旧版,总好过起不来。
fn autostart_exe() -> anyhow::Result<PathBuf> {
    // 首次开自启可能发生在任何一次 daemon 启动之前,那时落点还是空的。先发布一次,免得
    // 登记成当前渠道的路径 —— 那正是要根治的形态。
    match crate::setup::self_install::publish_current_exe() {
        Ok(outcome) => {
            if let Some(notice) = outcome.notice {
                eprintln!("note: {notice}");
            }
        }
        Err(e) => {
            eprintln!("{}", summon_i18n::format!("r4ad7c87f28"))
        }
    }
    let target = dirs::home_dir().map(|home| summon_core::companion::install_target(&home));
    match target {
        Some(path) if path.is_file() => Ok(path),
        _ => std::env::current_exe().context(summon_i18n::text!("rfc40696aad")),
    }
}

/// 开启开机自启:写 plist(`RunAtLoad`,`keep_alive` 决定 KeepAlive)→ `launchctl bootstrap` 装载 →
/// 落状态。
pub fn enable(keep_alive: bool) -> anyhow::Result<()> {
    let exe = autostart_exe()?;
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => enable_launchd(&exe, keep_alive)?,
        LoginItemBackend::SystemdUser => enable_systemd(&exe, keep_alive)?,
    }
    write_state_at(
        &state_path(),
        &AutostartState {
            enabled: true,
            keep_alive,
        },
    )?;
    Ok(())
}

fn enable_launchd(exe: &Path, keep_alive: bool) -> anyhow::Result<()> {
    let plist = render_launchd_plist(LAUNCHD_LABEL, exe, true, keep_alive, &log_path());
    let pp = plist_path();
    let current = std::fs::read_to_string(&pp).ok().as_deref() == Some(plist.as_str());
    if plan_enable(service_state(&daemon_target())?, current)? == EnablePlan::AlreadyCurrent {
        return Ok(());
    }
    write_plist(&pp, &plist)?;
    // 装载失败只 warn:plist 已在盘上,下次登录仍会自启。
    if let Err(e) = reload_launchd(&pp) {
        eprintln!("{}", summon_i18n::format!("r9547a6abd5"));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum EnablePlan {
    Reload,
    AlreadyCurrent,
}

/// 重新装载要先 bootout,而 bootout 会停掉 launchd 此刻托管着的 daemon —— 所以托管中的 daemon
/// 在跑时只接受「内容没变」的 enable(什么都不用做),要改就让人先停 daemon。
fn plan_enable(state: ServiceState, plist_current: bool) -> anyhow::Result<EnablePlan> {
    match state {
        ServiceState::Running if plist_current => Ok(EnablePlan::AlreadyCurrent),
        ServiceState::Running => anyhow::bail!(summon_i18n::format!("r494c90b175")),
        ServiceState::Idle | ServiceState::NotLoaded => Ok(EnablePlan::Reload),
    }
}

/// bootout 同样会停掉托管中的 daemon;关自启不该顺手把它打死。
fn check_disable(state: ServiceState) -> anyhow::Result<()> {
    if state == ServiceState::Running {
        anyhow::bail!(summon_i18n::format!("rcb865fd5fd"));
    }
    Ok(())
}

fn write_plist(pp: &Path, plist: &str) -> anyhow::Result<()> {
    if let Some(parent) = pp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(pp, plist).with_context(|| summon_i18n::format!("rb0f6b6620c", pp.display()))
}

/// bootout 旧的(忽略「未装载」错)→ 等它离开域 → bootstrap。`RunAtLoad` 让 bootstrap 当场拉起
/// 一个 daemon;已有 daemon 在跑时它拿不到单例锁,以 0 退出,launchd 不会重试。
fn reload_launchd(pp: &Path) -> anyhow::Result<()> {
    let target = daemon_target();
    let _ = launchctl(&["bootout", &target]);
    wait_until_service_gone(&target, BOOTOUT_TIMEOUT, service_present);
    launchctl(&["bootstrap", &gui_domain(), &pp.to_string_lossy()])
}

/// `daemon start` 调用(调用方已确认没有 daemon 在跑、单例锁空闲):开了自启时交给自启单元
/// (launchd / systemd)起,daemon 从此归它管,崩溃后能被拉起。起之前把登记收敛到当前这一版 ——
/// 单元文件与按状态文件、`exe` 渲染的不一致(或 launchd 服务没装载),就重写并重新装载;一致就
/// 直接起。
///
/// `Ok(false)` = 这台机器不走托管(没开自启;Linux 上没开 linger),调用方自己 spawn。
pub fn start_supervised(exe: &Path) -> anyhow::Result<bool> {
    let state = read_state();
    if !state.enabled {
        return Ok(false);
    }
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => start_launchd(exe, state.keep_alive).map(|()| true),
        LoginItemBackend::SystemdUser => start_systemd(exe, state.keep_alive),
    }
}

/// 这台机器装不了开机自启的原因:Linux 上没有在跑的 systemd(容器、没开 systemd 的 WSL)时,
/// 写了单元也没人加载。`None` = 可以装。
pub fn unavailable_reason() -> Option<&'static str> {
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => None,
        LoginItemBackend::SystemdUser => {
            let systemctl = summon_core::tools::find_executable(
                &summon_core::tools::resolve(summon_core::tools::Tool::Systemctl),
                &std::env::var_os("PATH").unwrap_or_default(),
            )
            .is_some();
            systemd_unavailable(systemctl, Path::new("/run/systemd/system").is_dir())
        }
    }
}

/// systemd 判据:有 systemctl,且本机由 systemd 引导(`/run/systemd/system` 在,与 sd_booted 同一判据)。
fn systemd_unavailable(systemctl_found: bool, booted_by_systemd: bool) -> Option<&'static str> {
    if !systemctl_found {
        Some(summon_i18n::text!("r2bc0a5d0aa"))
    } else if !booted_by_systemd {
        Some(summon_i18n::text!("ra4f72c38b6"))
    } else {
        None
    }
}

/// 这台机器上自启单元能不能起、能不能托管 daemon:macOS 恒能;Linux 要开了 linger。
pub fn supervision_possible() -> anyhow::Result<bool> {
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => Ok(true),
        LoginItemBackend::SystemdUser => linger_enabled(),
    }
}

/// 托管 daemon 的是谁,给人看的。
pub fn supervisor_label() -> &'static str {
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => "launchd",
        LoginItemBackend::SystemdUser => "systemd",
    }
}

fn start_launchd(exe: &Path, keep_alive: bool) -> anyhow::Result<()> {
    let target = daemon_target();
    let plist = render_launchd_plist(LAUNCHD_LABEL, exe, true, keep_alive, &log_path());
    let pp = plist_path();
    let current = std::fs::read_to_string(&pp).ok().as_deref() == Some(plist.as_str());
    match plan_supervised_start(service_state(&target)?, current) {
        StartPlan::AlreadyStarting => {}
        StartPlan::Start => launchctl(&["kickstart", &target])?,
        // 重新装载时 `RunAtLoad` 随即拉起 daemon。
        StartPlan::ReloadAndStart => {
            write_plist(&pp, &plist)?;
            reload_launchd(&pp)?;
        }
    }
    Ok(())
}

/// 没开 linger 就不托管:user 实例随最后一个登录会话停止,停止时把自己 cgroup 里剩下的进程
/// 整组 SIGKILL —— 单元的 `KillMode=process` 挡不住这一层,daemon 起的 tmux 会话全在里面。
/// 直接 spawn 的 daemon 落在登录会话的 scope 里,登出后照样活着(`KillUserProcesses=no`)。
fn start_systemd(exe: &Path, keep_alive: bool) -> anyhow::Result<bool> {
    if !linger_enabled()? {
        eprintln!("{}", summon_i18n::format!("r6bbcd017af"));
        return Ok(false);
    }
    let unit = render_systemd_unit(exe, keep_alive, &log_path());
    let up = unit_path();
    let current = std::fs::read_to_string(&up).ok().as_deref() == Some(unit.as_str());
    match plan_supervised_start(unit_state()?, current) {
        StartPlan::AlreadyStarting => {}
        // 盘上文件一致不代表 systemd 内存里也是这一版(上一次写完文件后 reload 可能没成):先
        // reload 再起,绝不起一份旧定义 —— 旧单元的 KillMode 可能会连带杀掉 tmux。
        StartPlan::Start => {
            systemctl(&["--user", "daemon-reload"])?;
            systemctl(&["--user", "start", SYSTEMD_UNIT_NAME])?;
        }
        StartPlan::ReloadAndStart => {
            if let Some(parent) = up.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&up, unit)
                .with_context(|| summon_i18n::format!("rb2ca47595d", up.display()))?;
            systemctl(&["--user", "daemon-reload"])?;
            systemctl(&["--user", "enable", SYSTEMD_UNIT_NAME])?;
            systemctl(&["--user", "start", SYSTEMD_UNIT_NAME])?;
        }
    }
    Ok(true)
}

/// 当前用户开没开 linger。查不清是 `Err`,调用方按不托管处理。
pub fn linger_enabled() -> anyhow::Result<bool> {
    let user = current_user_name()?;
    let bin = summon_core::tools::resolve(summon_core::tools::Tool::Loginctl);
    let out = Command::new(&bin)
        .args(["show-user", &user, "--property=Linger", "--value"])
        .output()
        .with_context(|| summon_i18n::format!("rcebdeabf6e", bin.display()))?;
    if !out.status.success() {
        anyhow::bail!(summon_i18n::format!(
            "r2e3b1f10ab",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim() == "yes")
}

/// 此刻托管着的 daemon 进程号(自启单元报告的主进程);没有在跑的为 `None`,查不清为 `Err`。
pub fn supervised_pid() -> anyhow::Result<Option<u32>> {
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => {
            if service_state(&daemon_target())? == ServiceState::NotLoaded {
                return Ok(None);
            }
            let out = crate::setup::launchd::print_service(&daemon_target())
                .context(summon_i18n::text!("r37211e128d"))?;
            Ok(launchd_pid(&String::from_utf8_lossy(&out.stdout)))
        }
        LoginItemBackend::SystemdUser => {
            let bin = summon_core::tools::resolve(summon_core::tools::Tool::Systemctl);
            let out = Command::new(&bin)
                .args([
                    "--user",
                    "show",
                    "--property=MainPID",
                    "--value",
                    SYSTEMD_UNIT_NAME,
                ])
                .output()
                .with_context(|| summon_i18n::format!("rcebdeabf6e", bin.display()))?;
            let pid: u32 = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .with_context(|| {
                    summon_i18n::format!("rdfe0e4f641", String::from_utf8_lossy(&out.stderr).trim())
                })?;
            Ok((pid != 0).then_some(pid))
        }
    }
}

/// `launchctl print` 顶层的 `pid = N`(嵌套块里的 pid 缩进更深,不算)。
fn launchd_pid(print: &str) -> Option<u32> {
    print
        .lines()
        .find_map(|line| line.strip_prefix("\tpid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

#[derive(Debug, PartialEq, Eq)]
enum StartPlan {
    /// 单元已有进程在跑:它拉起的那一代正在启动、还没来得及拿锁。别重载它,等它就绪。
    AlreadyStarting,
    Start,
    /// 登记不是当前这一版,或 launchd 服务没装载:重写单元、重新装载,再起。
    ReloadAndStart,
}

fn plan_supervised_start(state: ServiceState, unit_current: bool) -> StartPlan {
    match state {
        ServiceState::Running => StartPlan::AlreadyStarting,
        ServiceState::Idle if unit_current => StartPlan::Start,
        ServiceState::Idle | ServiceState::NotLoaded => StartPlan::ReloadAndStart,
    }
}

/// systemd 单元此刻的状态。查不清(连不上用户实例等)是 `Err`。
fn unit_state() -> anyhow::Result<ServiceState> {
    let bin = summon_core::tools::resolve(summon_core::tools::Tool::Systemctl);
    let out = Command::new(&bin)
        .args(["--user", "is-active", SYSTEMD_UNIT_NAME])
        .output()
        .with_context(|| summon_i18n::format!("rcebdeabf6e", bin.display()))?;
    classify_is_active(&out.stdout, &out.stderr)
}

/// `systemctl is-active` 的回答。停止中(`deactivating`)也算在跑:那一代还没退,不能去重载它。
fn classify_is_active(stdout: &[u8], stderr: &[u8]) -> anyhow::Result<ServiceState> {
    match String::from_utf8_lossy(stdout).trim() {
        "active" | "activating" | "reloading" | "deactivating" => Ok(ServiceState::Running),
        "inactive" | "failed" => Ok(ServiceState::Idle),
        other => anyhow::bail!(summon_i18n::format!(
            "rbbc7e81c5d",
            String::from_utf8_lossy(stderr).trim()
        )),
    }
}

/// `daemon start` 等托管的那一代就绪超时:让单元给它发 SIGTERM,等它离开运行态。优雅退出码是 0,
/// 单元不会再拉;信号到得比 handler 注册还早的话,launchd 会按信号退出拉起它一次,那一次没有
/// 启动请求,由 daemon 的熔断以 0 退出收住(systemd 的 stop 是显式停止,不会再拉)。
pub fn stop_supervised() -> anyhow::Result<()> {
    let state = || match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => service_state(&daemon_target()),
        LoginItemBackend::SystemdUser => unit_state(),
    };
    if state()? != ServiceState::Running {
        return Ok(());
    }
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => launchctl(&["kill", "SIGTERM", &daemon_target()])?,
        LoginItemBackend::SystemdUser => {
            systemctl(&["--user", "stop", "--no-block", SYSTEMD_UNIT_NAME])?
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if state()? != ServiceState::Running {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!(summon_i18n::format!("r54df7634c9", supervisor_label()))
}

/// Linux:写单元 → `daemon-reload` → `enable` → 开 linger。
///
/// **不带 `--now`**:enable 只登记「以后开机起」,不该顺手启动 —— 此刻多半已经有一个手动起的
/// daemon 在跑,再起一个只会撞锁。这和 macOS 那条一致(bootstrap 也不启动已在跑的进程)。
///
/// `enable-linger` 失败只 warn 而不 bail:单元照样写好、登记好,但没有 linger 它不会起(单元里
/// 的 `ConditionPathExists`,理由见 `render_systemd_unit`),`daemon start` 也不交给它 —— 开机
/// 自启与崩溃拉起都要等 linger 开了才生效。有些机器上开 linger 要 root(polkit 策略),那种情况下
/// 让用户自己 `sudo loginctl enable-linger <user>` 比在这里假装失败干净。
fn enable_systemd(exe: &Path, keep_alive: bool) -> anyhow::Result<()> {
    let unit = render_systemd_unit(exe, keep_alive, &log_path());
    let up = unit_path();
    if let Some(parent) = up.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&up, unit).with_context(|| summon_i18n::format!("rb2ca47595d", up.display()))?;
    systemctl(&["--user", "daemon-reload"]).context("systemctl --user daemon-reload")?;
    systemctl(&["--user", "enable", SYSTEMD_UNIT_NAME])
        .context(summon_i18n::text!("re7bfa02307"))?;
    if let Err(e) = enable_linger() {
        eprintln!("{}", summon_i18n::format!("r058d64e44f"));
    }
    Ok(())
}

/// `loginctl enable-linger <当前用户>`。用显式用户名而不是空参:空参依赖 `loginctl` 从调用者的
/// 会话推断,而部署脚本 / cron 这类无 session 的上下文里推断不出来。
fn enable_linger() -> anyhow::Result<()> {
    loginctl(&["enable-linger", &current_user_name()?])
}

fn current_user_name() -> anyhow::Result<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()
        .map(|u| u.name)
        .context(summon_i18n::text!("r96c2bd9db3"))
}

/// 关闭开机自启:`launchctl bootout` 卸载 → 删 plist → 落状态(both-false)。
pub fn disable() -> anyhow::Result<()> {
    match LoginItemBackend::current() {
        LoginItemBackend::LaunchdUser => {
            check_disable(service_state(&daemon_target())?)?;
            let _ = launchctl(&["bootout", &daemon_target()]);
            let _ = std::fs::remove_file(plist_path());
        }
        LoginItemBackend::SystemdUser => {
            // **不带 `--now`**:关自启不该顺手把正在跑的 daemon 打死。这与 macOS 侧对称
            // (bootout 卸载的是 launchd 托管的那一份,手动起的 daemon 不受影响)。
            let _ = systemctl(&["--user", "disable", SYSTEMD_UNIT_NAME]);
            let _ = std::fs::remove_file(unit_path());
            let _ = systemctl(&["--user", "daemon-reload"]);
            // linger 不动:它是用户级设置,可能还有别的常驻服务靠它,关 summon 自启不该顺手掀了。
        }
    }
    write_state_at(&state_path(), &AutostartState::default())?;
    Ok(())
}

/// 调 `systemctl <args>`;非零退出 → Err(带 stderr)。
fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    run_tool(summon_core::tools::Tool::Systemctl, args)
}

/// 调 `loginctl <args>`;非零退出 → Err(带 stderr)。
fn loginctl(args: &[&str]) -> anyhow::Result<()> {
    run_tool(summon_core::tools::Tool::Loginctl, args)
}

fn run_tool(tool: summon_core::tools::Tool, args: &[&str]) -> anyhow::Result<()> {
    let bin = summon_core::tools::resolve(tool);
    let out = Command::new(&bin)
        .args(args)
        .output()
        .with_context(|| summon_i18n::format!("rcebdeabf6e", bin.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!(summon_i18n::format!(
            "r336fb5644d",
            bin.display(),
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_is_skipped_without_a_running_systemd() {
        assert!(systemd_unavailable(false, true).is_some());
        assert!(systemd_unavailable(true, false).is_some());
        assert_eq!(systemd_unavailable(true, true), None);
    }

    #[test]
    fn enable_never_reloads_under_a_running_supervised_daemon() {
        assert_eq!(
            plan_enable(ServiceState::Running, true).unwrap(),
            EnablePlan::AlreadyCurrent
        );
        assert!(plan_enable(ServiceState::Running, false).is_err());
        for state in [ServiceState::Idle, ServiceState::NotLoaded] {
            assert_eq!(plan_enable(state, true).unwrap(), EnablePlan::Reload);
            assert_eq!(plan_enable(state, false).unwrap(), EnablePlan::Reload);
        }
    }

    #[test]
    fn disable_never_boots_out_a_running_supervised_daemon() {
        assert!(check_disable(ServiceState::Running).is_err());
        assert!(check_disable(ServiceState::Idle).is_ok());
        assert!(check_disable(ServiceState::NotLoaded).is_ok());
    }

    #[test]
    fn supervised_start_starts_directly_only_a_current_idle_registration() {
        assert_eq!(
            plan_supervised_start(ServiceState::Running, false),
            StartPlan::AlreadyStarting
        );
        assert_eq!(
            plan_supervised_start(ServiceState::Idle, true),
            StartPlan::Start
        );
        assert_eq!(
            plan_supervised_start(ServiceState::Idle, false),
            StartPlan::ReloadAndStart
        );
        assert_eq!(
            plan_supervised_start(ServiceState::NotLoaded, true),
            StartPlan::ReloadAndStart
        );
    }

    /// deploy.sh 拦「正常退出也会被拉起」的单元:默认单元必须放行,`keep_alive` 的必须拦下。
    /// 用脚本里的原样正则跑 grep,两边谁改了都会在这里对不上。
    #[test]
    fn deploy_gate_blocks_only_units_that_restart_after_a_clean_exit() {
        let deploy = include_str!("../../../../scripts/deploy.sh");
        let start = deploy
            .find("grep -Eq '^[[:space:]]*Restart")
            .expect("deploy.sh 的 systemd 闸")
            + "grep -Eq '".len();
        let pattern = &deploy[start..start + deploy[start..].find('\'').unwrap()];
        let dir = tempfile::tempdir().unwrap();
        let blocked = |keep_alive: bool| {
            let unit = dir.path().join("summon-daemon.service");
            std::fs::write(
                &unit,
                render_systemd_unit(Path::new("/x/summon"), keep_alive, Path::new("/x.log")),
            )
            .unwrap();
            Command::new("grep")
                .args(["-Eq", pattern])
                .arg(&unit)
                .status()
                .unwrap()
                .success()
        };
        assert!(
            !blocked(false),
            "默认单元被拦,开了自启的机器每次部署都会失败"
        );
        assert!(blocked(true), "keep_alive 单元必须拦下");
    }

    #[test]
    fn launchd_pid_is_read_from_the_top_level_only() {
        let print = "gui/501/com.summon.daemon = {\n\tstate = running\n\tpid = 73727\n\tendpoints = {\n\t\tpid = 1\n\t}\n}\n";
        assert_eq!(launchd_pid(print), Some(73727));
        assert_eq!(launchd_pid("x = {\n\tstate = not running\n}\n"), None);
    }

    #[test]
    fn systemd_answers_map_to_states_and_unknowns_are_errors() {
        for running in ["active", "activating", "reloading", "deactivating"] {
            assert_eq!(
                classify_is_active(format!("{running}\n").as_bytes(), b"").unwrap(),
                ServiceState::Running,
                "{running}"
            );
        }
        for idle in ["inactive", "failed"] {
            assert_eq!(
                classify_is_active(format!("{idle}\n").as_bytes(), b"").unwrap(),
                ServiceState::Idle,
                "{idle}"
            );
        }
        // 连不上用户实例时 stdout 是空的:查不清,不能当成「没在跑」。
        assert!(classify_is_active(b"", b"Failed to connect to bus").is_err());
    }

    #[test]
    fn state_file_round_trips_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("autostart.json");
        let off = AutostartState::default();
        assert!(!off.enabled && !off.keep_alive);
        assert_eq!(read_state_at(&p), off);
        // 显式解析防止 read_state_at 的兜底掩盖旧格式解析失败。
        assert_eq!(serde_json::from_str::<AutostartState>("{}").unwrap(), off);
        std::fs::write(&p, "{}").unwrap();
        assert_eq!(read_state_at(&p), off);
        // 写后读回一致。
        let st = AutostartState {
            enabled: true,
            keep_alive: true,
        };
        write_state_at(&p, &st).unwrap();
        assert_eq!(read_state_at(&p), st);
        // 关闭 → 写默认 → 读回 both-false。
        write_state_at(&p, &AutostartState::default()).unwrap();
        assert_eq!(read_state_at(&p), AutostartState::default());
    }
}
