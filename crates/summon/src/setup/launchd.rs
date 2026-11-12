//! launchd 用户域(`gui/<uid>`)的共用操作。summon 管两个 LaunchAgent —— Menu Bar 与 daemon 的
//! 开机自启 —— 装载、查状态、等卸载完成都走这里。

use anyhow::Context;
use std::process::Command;
use std::time::{Duration, Instant};

/// `bootout` 后等服务从域里消失的上限。
pub(crate) const BOOTOUT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn gui_domain() -> String {
    format!("gui/{}", nix::unistd::Uid::current().as_raw())
}

/// `gui/<uid>/<label>`,launchctl 定位单个服务的写法。
pub(crate) fn service_target(label: &str) -> String {
    format!("{}/{label}", gui_domain())
}

/// 调 `launchctl <args>`;非零退出 → Err(带 stderr)。
pub(crate) fn launchctl(args: &[&str]) -> anyhow::Result<()> {
    let output = run(args).context(summon_i18n::text!("r74dfc404c4"))?;
    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!(summon_i18n::format!(
            "r90d6f6b5fd",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(summon_core::tools::resolve(
        summon_core::tools::Tool::Launchctl,
    ))
    .args(args)
    .output()
}

/// `launchctl print <target>` 的原始输出。
pub(crate) fn print_service(target: &str) -> std::io::Result<std::process::Output> {
    run(&["print", target])
}

/// `launchctl print <target>` 成功 = 服务还在域里(已装载)。
pub(crate) fn service_present(target: &str) -> bool {
    print_service(target)
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// launchctl 眼里一个服务此刻的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceState {
    /// 域里没有这个服务。
    NotLoaded,
    /// 已装载,没有进程在跑。
    Idle,
    /// 已装载,有进程在跑。
    Running,
}

/// `launchctl print` 的退出码(实测):113 = 域里没有这个服务;112 = 连这个域都没有(没有 GUI
/// 登录的 ssh 会话里 `gui/<uid>` 不存在)—— 域都没有,里面不可能有进程在跑。
const PRINT_EXIT_NOT_FOUND: i32 = 113;
const PRINT_EXIT_NO_DOMAIN: i32 = 112;

/// 查服务状态。查不清(launchctl 调不起来、报别的错)是 `Err` —— 调用方拿它挡 bootout 时,
/// 不能把「看不清」当成「没在跑」。
pub(crate) fn service_state(target: &str) -> anyhow::Result<ServiceState> {
    let out = print_service(target).context(summon_i18n::text!("r37211e128d"))?;
    classify_print(out.status.code(), &out.stdout, &out.stderr)
}

fn classify_print(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> anyhow::Result<ServiceState> {
    match code {
        Some(0) if reports_running(true, stdout) => Ok(ServiceState::Running),
        Some(0) => Ok(ServiceState::Idle),
        Some(PRINT_EXIT_NOT_FOUND | PRINT_EXIT_NO_DOMAIN) => Ok(ServiceState::NotLoaded),
        _ => anyhow::bail!(summon_i18n::format!(
            "r6a26277236",
            String::from_utf8_lossy(stderr).trim()
        )),
    }
}

/// `launchctl print` 的输出是否表示「已装载且在跑」。
pub(crate) fn reports_running(success: bool, stdout: &[u8]) -> bool {
    success
        && String::from_utf8_lossy(stdout)
            .lines()
            .any(|line| line.trim() == "state = running")
}

/// 等服务从 launchd 域里彻底消失。
///
/// `bootout` 是**异步**的:它只是请求卸载,服务真正从域里消失还要等进程终止 + launchd 清理。
/// 紧接着 bootstrap 会撞上「服务还在域里」→ `Bootstrap failed: 5: Input/output error`。
/// 超时也照常返回、继续往下走 —— 让 bootstrap 自己去报真正的错,而不是在这里编一个
/// 「等超时了」的失败:服务没消失未必等于装不上,可能只是 launchd 慢了半拍。
pub(crate) fn wait_until_service_gone(
    target: &str,
    timeout: Duration,
    present: impl Fn(&str) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !present(target) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 不等 bootout 完成就 bootstrap 会偶发失败 —— 而偶发的失败最难查。
    #[test]
    fn waits_for_a_lingering_service_to_leave_the_domain() {
        let probes = AtomicUsize::new(0);
        // 前两次探测服务还在(launchd 还没清理完),第三次才消失。
        let present = |_: &str| probes.fetch_add(1, Ordering::SeqCst) < 2;

        assert!(wait_until_service_gone(
            "gui/501/com.summon.menubar",
            Duration::from_secs(5),
            present
        ));
        assert_eq!(probes.load(Ordering::SeqCst), 3, "应一直等到服务真的消失");
    }

    #[test]
    fn returns_immediately_when_nothing_is_loaded() {
        let started = Instant::now();
        assert!(wait_until_service_gone("x", Duration::from_secs(5), |_| {
            false
        }));
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "没装载就别空等"
        );
    }

    #[test]
    fn gives_up_after_timeout_instead_of_hanging() {
        // 服务始终不消失:超时后照常返回 false,让 bootstrap 去报真正的错 ——
        // 而不是在这里挂死,或者编一个「等超时了」的假失败。
        let started = Instant::now();
        assert!(!wait_until_service_gone(
            "x",
            Duration::from_millis(200),
            |_| true
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn print_results_map_to_service_states_and_unknowns_are_errors() {
        let running = b"x = {\n\tstate = running\n}\n";
        let waiting = b"x = {\n\tstate = not running\n}\n";
        assert_eq!(
            classify_print(Some(0), running, b"").unwrap(),
            ServiceState::Running
        );
        assert_eq!(
            classify_print(Some(0), waiting, b"").unwrap(),
            ServiceState::Idle
        );
        assert_eq!(
            classify_print(Some(113), b"", b"Could not find service").unwrap(),
            ServiceState::NotLoaded
        );
        assert_eq!(
            classify_print(Some(112), b"", b"Bad request").unwrap(),
            ServiceState::NotLoaded
        );
        // 查不清不能被当成「没在跑」—— 那会让 bootout 停掉正在托管着的 daemon。
        assert!(classify_print(Some(1), b"", b"boom").is_err());
        assert!(classify_print(None, b"", b"").is_err());
    }

    #[test]
    fn running_requires_a_loaded_service_in_the_running_state() {
        assert!(reports_running(
            true,
            b"com.summon.menubar = {\n state = running\n}\n"
        ));
        assert!(!reports_running(
            true,
            b"com.summon.menubar = {\n state = waiting\n}\n"
        ));
        assert!(!reports_running(false, b"state = running\n"));
    }
}
