//! summon 原生 macOS Menu Bar 的安装生命周期。
//!
//! Swift 可执行文件随 summon 一起分发（安装包里与 summon 并排，源码构建由 deploy 脚本发布到
//! `~/.summon/bin/`）；这里仅管理 summon 自己的 LaunchAgent，不依赖第三方 CLI，也不开放
//! HTTP/Web 控制面。

use crate::setup::launchd::{
    gui_domain, launchctl, service_present, wait_until_service_gone, BOOTOUT_TIMEOUT,
};
use anyhow::Context;
use std::path::PathBuf;
use summon_core::menubar::{plist_file_name, render_launchd_plist, LAUNCHD_LABEL};

#[derive(Debug)]
pub struct Status {
    pub binary_exists: bool,
    pub plist_exists: bool,
    pub running: bool,
}

fn home() -> anyhow::Result<PathBuf> {
    dirs::home_dir().context(summon_i18n::text!("rde14ef2c2a"))
}

pub fn binary_path() -> Option<PathBuf> {
    summon_core::companion::locate(summon_core::companion::MENUBAR)
}

pub(crate) fn plist_path() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("Library/LaunchAgents").join(plist_file_name()))
}

fn log_path() -> anyhow::Result<PathBuf> {
    Ok(home()?.join(".summon/run/menubar.log"))
}

fn service_target() -> String {
    crate::setup::launchd::service_target(LAUNCHD_LABEL)
}

pub fn install() -> anyhow::Result<()> {
    ensure_macos()?;
    let binary = binary_path().ok_or_else(|| {
        anyhow::anyhow!(summon_i18n::format!(
            "r16627fd950",
            summon_core::companion::MENUBAR,
            ::summon_core::i18n::label(summon_core::companion::WHERE)
        ))
    })?;
    let summon_cli = std::env::current_exe().context(summon_i18n::text!("rfc40696aad"))?;
    let plist = render_launchd_plist(&binary, &summon_cli, &log_path()?);
    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = log_path()?.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("plist.tmp");
    std::fs::write(&temporary, plist)
        .with_context(|| summon_i18n::format!("r43b471bc3a", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .with_context(|| summon_i18n::format!("rad6b116835", path.display()))?;

    let _ = launchctl(&["bootout", &service_target()]);
    // bootout 是**异步**的:它只是请求卸载,服务真正从域里消失还要等进程终止 + launchd 清理。
    // 紧接着 bootstrap 会撞上「服务还在域里」→ `Bootstrap failed: 5: Input/output error`。
    // 这是竞态,所以它偶发 —— 重跑一次 deploy 往往就好了,最容易被当成玄学。
    wait_until_service_gone(&service_target(), BOOTOUT_TIMEOUT, service_present);
    launchctl(&["bootstrap", &gui_domain(), path.to_string_lossy().as_ref()])
        .context(summon_i18n::text!("r021956c22a"))?;
    launchctl(&["kickstart", "-k", &service_target()])
        .context(summon_i18n::text!("re3c1b8ef19"))?;
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    ensure_macos()?;
    let _ = launchctl(&["bootout", &service_target()]);
    let path = plist_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| summon_i18n::format!("r96a6eb5265", path.display()))?;
    }
    Ok(())
}

pub fn status() -> anyhow::Result<Status> {
    ensure_macos()?;
    let launchd = crate::setup::launchd::print_service(&service_target())
        .context(summon_i18n::text!("r492c6b03df"))?;
    let running = crate::setup::launchd::reports_running(launchd.status.success(), &launchd.stdout);
    Ok(Status {
        binary_exists: binary_path().is_some(),
        plist_exists: plist_path()?.is_file(),
        running,
    })
}

fn ensure_macos() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        anyhow::bail!(summon_i18n::format!("r2ddb29b21f"))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn deploy_signs_staged_menubar_with_stable_identifier() {
        let deploy = include_str!("../../../../scripts/deploy.sh");
        assert!(deploy.contains("source scripts/codesign.sh"));
        assert!(deploy.contains("summon_sign_path \"$STAGED_MENUBAR\" \"com.summon.menubar\""));
    }

    #[test]
    fn deploy_reexecs_bash_when_invoked_with_sh() {
        let deploy =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/deploy.sh");
        let output = std::process::Command::new("sh")
            .arg(deploy)
            .arg("--invalid-test-argument")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains("未知参数: --invalid-test-argument"));
        assert!(!stderr.contains("Illegal option"));
    }

    #[test]
    fn deploy_preserves_menubar_installation_topology() {
        let deploy = include_str!("../../../../scripts/deploy.sh");

        assert!(deploy.contains("--menubar)"));
        assert!(deploy.contains("MENUBAR_FORCE_INSTALL"));
        assert!(deploy.contains("MENUBAR_FORCE_SKIP"));
        assert!(deploy.contains("MENUBAR_WAS_INSTALLED"));
        assert!(deploy.contains("MENUBAR_PLIST"));
        assert!(deploy.contains("[ -f \"$MENUBAR_PLIST\" ] && MENUBAR_WAS_INSTALLED=1"));
        assert!(deploy
            .contains("[ \"$MENUBAR_FORCE_INSTALL\" = 1 ] || [ \"$MENUBAR_WAS_INSTALLED\" = 1 ]"));
        assert!(deploy.contains("首次安装加 --menubar"));
    }

    #[test]
    fn deploy_waits_for_menubar_readiness() {
        let deploy = include_str!("../../../../scripts/deploy.sh");

        assert!(deploy.contains("wait_for_menubar"));
        assert!(deploy.contains("MENUBAR_READY_ATTEMPTS"));
        assert!(deploy.contains("sleep 0.2"));
        assert!(deploy.contains("核心部署已完成,但 Menu Bar 未进入运行态"));
    }
}
