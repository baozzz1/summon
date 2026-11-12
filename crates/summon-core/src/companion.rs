//! summon 随身二进制(Go sidecar / macOS Menu Bar)的定位。
//!
//! 规则:**与 summon 真身同目录优先,找不到再回退 `~/.summon/bin/`**。
//!
//! 为什么同目录优先:安装包把 `summon` / `summon-sidecar` / `summon-menubar` 三个文件并排装在
//! 同一个目录里,而那个目录在哪由包管理器决定(全局装、项目内装、各家 store 各不相同),包外
//! 没有任何约定位置可写,所以「同目录」是唯一稳定的锚点。
//!
//! `~/.summon/bin/` 有两重身份。一是这里的回退位置:`cargo run` 跑的 `target/release/summon`
//! 身边没有 sidecar,得去 `summon dev sidecar build` 装的那个固定位置取。二是三件套的**稳定
//! 落点**(见 [`install_dir`]),每条安装渠道都往这里发布。deploy.sh 的布局(三个产物真身都在
//! `~/.summon/bin/`、`~/.local/bin/summon` 只是软链)在「同目录」规则下本来就命中,不靠回退。
//!
//! **`current_exe` 必须 canonicalize**:`~/.local/bin/summon` 和包管理器放进 PATH 的入口都是软链,
//! 不解引用拿到的是软链所在目录,那里一个随身二进制都没有。

use std::path::{Path, PathBuf};

/// summon 自身的文件名。
pub const SUMMON: &str = "summon";
/// Go sidecar 的文件名(进程按功能命名,见仓库 CLAUDE.md「术语 / 命名」)。
pub const SIDECAR: &str = "summon-sidecar";
/// 原生 macOS Menu Bar 的文件名。
pub const MENUBAR: &str = "summon-menubar";

/// 三件套一起发布 —— `locate` 同目录优先,少发一个就会取到跨版本的组合。
pub const TRIO: [&str; 3] = [SUMMON, SIDECAR, MENUBAR];

/// 缺失时给用户看的位置说明。**渠道中性**:同一份措辞对安装包、源码 deploy.sh 装都成立。
pub const WHERE: &str = "r520e4700d9";

/// 定位一个随身二进制;两处都没有就返回 `None`(调用方决定这算不算致命)。
pub fn locate(name: &str) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let home = dirs::home_dir();
    locate_in(exe_dir.as_deref(), home.as_deref(), name)
}

/// [`locate`] 的可注入版本:把「summon 真身所在目录」和「home」当参数收进来,便于单测。
/// 存在性判据仍是真实文件系统 —— 测试拿临时目录里的真文件跑,不 mock。
pub fn locate_in(exe_dir: Option<&Path>, home: Option<&Path>, name: &str) -> Option<PathBuf> {
    let candidates = [
        exe_dir.map(|dir| dir.join(name)),
        home.map(|dir| dir.join(".summon/bin").join(name)),
    ];
    candidates.into_iter().flatten().find(|path| path.is_file())
}

/// 三件套的稳定落点。**开机自启只认这里**:各安装渠道装的位置各不相同,
/// 而 launchd/systemd 单元里的路径得是一个不随安装渠道变的常量,否则最后一次装的人
/// 改不到已经登记的那份(本机 0.6.0 跑了两天正是如此)。
pub fn install_dir(home: &Path) -> PathBuf {
    home.join(".summon/bin")
}

/// 开机自启单元该指向的 summon 路径。
pub fn install_target(home: &Path) -> PathBuf {
    install_dir(home).join(SUMMON)
}

/// 这份二进制要不要发布到稳定落点。`exe` 必须已 canonicalize —— 软链不解引用会把
/// `~/.local/bin/summon` 判成「不是落点」,于是每次启动都白发布一遍。
pub fn needs_install(exe: &Path, home: &Path) -> bool {
    exe != install_target(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        path
    }

    #[test]
    fn prefers_sibling_of_summon_over_home_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("lib/summon-pkg");
        let home = tmp.path().join("home");
        let sibling = touch(&exe_dir, SIDECAR);
        touch(&home.join(".summon/bin"), SIDECAR);

        assert_eq!(
            locate_in(Some(&exe_dir), Some(&home), SIDECAR),
            Some(sibling)
        );
    }

    #[test]
    fn falls_back_to_home_bin_when_no_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("target/release");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let home = tmp.path().join("home");
        let fallback = touch(&home.join(".summon/bin"), SIDECAR);

        assert_eq!(
            locate_in(Some(&exe_dir), Some(&home), SIDECAR),
            Some(fallback)
        );
    }

    #[test]
    fn returns_none_when_neither_location_has_it() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("target/release");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".summon/bin")).unwrap();

        assert_eq!(locate_in(Some(&exe_dir), Some(&home), SIDECAR), None);
    }

    /// 目录存在但同名的是目录而不是文件时不能命中 —— `is_file` 而非 `exists`。
    #[test]
    fn ignores_a_directory_with_the_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("bin");
        std::fs::create_dir_all(exe_dir.join(MENUBAR)).unwrap();

        assert_eq!(locate_in(Some(&exe_dir), None, MENUBAR), None);
    }

    #[test]
    fn install_target_sits_in_home_summon_bin() {
        let home = Path::new("/home/someone");
        assert_eq!(
            install_target(home),
            Path::new("/home/someone/.summon/bin/summon")
        );
    }

    #[test]
    fn the_target_itself_needs_no_install() {
        let home = Path::new("/home/someone");
        assert!(!needs_install(&install_target(home), home));
    }

    #[test]
    fn a_binary_installed_elsewhere_needs_install() {
        let home = Path::new("/home/someone");
        let packaged = Path::new("/opt/homebrew/lib/summon-pkg/summon");
        assert!(needs_install(packaged, home));
    }

    /// 三件套少一个,`locate` 就会在落点取到上一版的随身二进制。
    #[test]
    fn the_trio_covers_every_companion_locate_can_return() {
        assert!(TRIO.contains(&SUMMON));
        assert!(TRIO.contains(&SIDECAR));
        assert!(TRIO.contains(&MENUBAR));
    }
}
