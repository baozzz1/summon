//! npm 全局安装位置。lark-cli 官方安装器、claude / codex 的推荐装法都把可执行文件放进
//! `$(npm prefix -g)/bin`,而这个目录不一定在 PATH 上:Linux 常把前缀设成 `~/.npm-global`,只在
//! 交互 shell 的 rc 里加进 PATH,daemon、自启单元、非交互 ssh 都看不到它。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// npm 与 npx 出自同一份 node 安装,优先取 npx 同目录那一个;那里没有就按 PATH 找。
fn npm_command(npx: &Path) -> PathBuf {
    npx.parent()
        .map(|dir| dir.join("npm"))
        .filter(|npm| summon_core::tools::is_executable(npm))
        .unwrap_or_else(|| PathBuf::from("npm"))
}

/// `npm prefix -g`;npm 不可用或输出为空时为 `None`。
pub(crate) fn global_prefix(npx: &Path) -> Option<PathBuf> {
    prefix_of(&npm_command(npx))
}

/// 给定那份 npm 的 `npm prefix -g`;不可用或输出为空时为 `None`。
pub(crate) fn prefix_of(npm: &Path) -> Option<PathBuf> {
    let output = Command::new(npm)
        .args(["prefix", "-g", "--no-update-notifier"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!prefix.is_empty()).then(|| PathBuf::from(prefix))
}

/// npm 全局 bin 目录。
pub(crate) fn global_bin(npx: &Path) -> Option<PathBuf> {
    global_prefix(npx).map(|prefix| prefix.join("bin"))
}

/// 在 npm 全局 bin 里找一个可执行的 `command`。
pub(crate) fn find_in_global_bin(bin_dir: &Path, command: &str) -> Option<PathBuf> {
    let candidate = bin_dir.join(command);
    summon_core::tools::is_executable(&candidate).then_some(candidate)
}

/// 当前用户能不能往这个前缀里 `npm install -g`:要写 `<prefix>/lib/node_modules` 与
/// `<prefix>/bin`。还不存在的目录看它最近一个已存在的上级 —— npm 会在那里把它建出来。
pub(crate) fn prefix_writable(prefix: &Path) -> bool {
    [prefix.join("lib").join("node_modules"), prefix.join("bin")]
        .iter()
        .all(|dir| nearest_existing(dir).is_some_and(writable))
}

/// 用户自己的 npm 全局目录,npm 官方文档给的位置。
pub(crate) fn user_prefix(home: &Path) -> PathBuf {
    home.join(".npm-global")
}

/// 把 npm 全局目录改到 `prefix`(写进用户级 `~/.npmrc`)。环境变量 `NPM_CONFIG_PREFIX` /
/// `npm_config_prefix` 比它优先,所以改完要用 [`global_prefix`] 读回确认。
pub(crate) fn set_global_prefix(npx: &Path, prefix: &Path) -> anyhow::Result<()> {
    let prefix = prefix.display().to_string();
    run_npm_config(npx, &["config", "set", "prefix", &prefix])
}

/// 撤掉用户级 `~/.npmrc` 里的 prefix。
pub(crate) fn unset_global_prefix(npx: &Path) -> anyhow::Result<()> {
    run_npm_config(npx, &["config", "delete", "prefix"])
}

fn run_npm_config(npx: &Path, args: &[&str]) -> anyhow::Result<()> {
    let npm = npm_command(npx);
    let command = format!("{} {}", npm.display(), args.join(" "));
    let status = Command::new(&npm)
        .args(args)
        .arg("--no-update-notifier")
        .status()
        .map_err(|error| anyhow::anyhow!(summon_i18n::format!("rf75d791a08")))?;
    if !status.success() {
        anyhow::bail!(summon_i18n::format!("r613d6d9769"));
    }
    Ok(())
}

/// 新开的交互 shell 会读的 rc:zsh 读 `~/.zshrc`,bash 读 `~/.bashrc`,sh 一族与没设 `SHELL`
/// 时读 `~/.profile`。fish、tcsh 这类不读这些文件的 shell 为 `None`,PATH 由人自己加。
pub(crate) fn shell_rc(home: &Path, shell: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let name = match shell {
        Some(shell) => Path::new(shell).file_name()?.to_str()?,
        None => "sh",
    };
    let rc = match name {
        "zsh" => ".zshrc",
        "bash" => ".bashrc",
        "sh" | "dash" | "ksh" => ".profile",
        _ => return None,
    };
    Some(home.join(rc))
}

/// 让 rc 把 `bin` 放到 PATH 最前面:旧前缀里装过的同名命令(如 root 装的 summon)不再先被找到。
/// rc 里已经有把这个目录放在原 PATH 前面的赋值就不动,返回有没有写。
pub(crate) fn ensure_path_in_rc(rc: &Path, home: &Path, bin: &Path) -> anyhow::Result<bool> {
    use std::io::Write;
    let existing = match std::fs::read_to_string(rc) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => anyhow::bail!(summon_i18n::format!("r0f1ddd51b2", rc.display())),
    };
    // 家目录下的写成 `$HOME/…`,rc 不绑死某个绝对路径;认已有的那行时 `~/…` 与绝对路径同样算。
    let (needle, entry) = match bin.strip_prefix(home) {
        Ok(relative) => {
            let relative = relative.display().to_string();
            (format!("/{relative}"), format!("$HOME/{relative}"))
        }
        Err(_) => {
            let absolute = bin.display().to_string();
            (absolute.clone(), absolute)
        }
    };
    if existing
        .lines()
        .any(|line| puts_first_on_path(line, &needle))
    {
        return Ok(false);
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(rc)
        .map_err(|error| anyhow::anyhow!(summon_i18n::format!("rad352e3770", rc.display())))?;
    write!(file, "{}", summon_i18n::format!("r65ce130f6a"))
        .map_err(|error| anyhow::anyhow!(summon_i18n::format!("r9736d836b5", rc.display())))?;
    Ok(true)
}

/// 这一行是不是把含 `needle` 的目录放在原 PATH 前面的赋值。追加在 `$PATH` 后面的不算:旧前缀里
/// 的同名命令仍会先被找到。
fn puts_first_on_path(line: &str, needle: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with('#') || !line.contains("PATH=") {
        return false;
    }
    let Some(at) = line.find(needle) else {
        return false;
    };
    match line.find("$PATH").or_else(|| line.find("${PATH}")) {
        Some(existing) => at < existing,
        None => true,
    }
}

/// 建出 `npm install -g` 会往里写的 `<prefix>/lib` 与 `<prefix>/bin`(已在的不动)。
pub(crate) fn ensure_layout(prefix: &Path) -> anyhow::Result<()> {
    for dir in [prefix.join("lib"), prefix.join("bin")] {
        std::fs::create_dir_all(&dir)
            .map_err(|error| anyhow::anyhow!(summon_i18n::format!("refe913b421", dir.display())))?;
    }
    Ok(())
}

fn nearest_existing(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| ancestor.is_dir())
}

fn writable(dir: &Path) -> bool {
    use nix::unistd::{access, AccessFlags};
    access(dir, AccessFlags::W_OK | AccessFlags::X_OK).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_prefix_the_user_owns_is_writable_even_before_npm_creates_its_dirs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(prefix_writable(&dir.path().join(".npm-global")));
    }

    /// 系统 node 的全局目录属 root:`lib/node_modules` 在、但不可写,装不进去。
    #[test]
    fn a_read_only_node_modules_dir_makes_the_prefix_unwritable() {
        if nix::unistd::Uid::effective().is_root() {
            return; // root 无视权限位,这个场景只对普通用户成立
        }
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("lib").join("node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::set_permissions(&modules, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!prefix_writable(dir.path()));
        std::fs::set_permissions(&modules, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prefix_writable(dir.path()));
    }

    #[test]
    fn an_empty_user_prefix_gets_the_dirs_npx_expects() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join(".npm-global");
        std::fs::create_dir_all(&prefix).unwrap();
        ensure_layout(&prefix).unwrap();
        assert!(prefix.join("lib").is_dir() && prefix.join("bin").is_dir());
        ensure_layout(&prefix).unwrap();
    }

    #[test]
    fn the_rc_follows_the_login_shell() {
        let home = Path::new("/home/u");
        let rc = |shell: Option<&str>| shell_rc(home, shell.map(std::ffi::OsStr::new));
        assert_eq!(rc(Some("/usr/bin/zsh")), Some(home.join(".zshrc")));
        assert_eq!(rc(Some("/bin/bash")), Some(home.join(".bashrc")));
        assert_eq!(rc(Some("/bin/dash")), Some(home.join(".profile")));
        assert_eq!(rc(None), Some(home.join(".profile")));
        assert_eq!(rc(Some("/usr/bin/fish")), None);
    }

    #[test]
    fn the_path_line_is_appended_once_after_the_existing_content() {
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".bashrc");
        std::fs::write(&rc, "alias ll='ls -l'").unwrap();
        let bin = user_prefix(home.path()).join("bin");

        assert!(ensure_path_in_rc(&rc, home.path(), &bin).unwrap());
        assert_eq!(
            std::fs::read_to_string(&rc).unwrap(),
            "alias ll='ls -l'\n\n# npm 全局目录(summon onboard 设置)\nexport PATH=\"$HOME/.npm-global/bin:$PATH\"\n"
        );
        assert!(!ensure_path_in_rc(&rc, home.path(), &bin).unwrap());
    }

    #[test]
    fn a_missing_rc_is_created_and_a_hand_written_entry_is_respected() {
        let home = tempfile::tempdir().unwrap();
        let bin = user_prefix(home.path()).join("bin");
        let created = home.path().join(".zshrc");
        assert!(ensure_path_in_rc(&created, home.path(), &bin).unwrap());
        assert!(std::fs::read_to_string(&created)
            .unwrap()
            .starts_with("\n# npm 全局目录"));

        let hand_written = home.path().join(".profile");
        std::fs::write(&hand_written, "PATH=~/.npm-global/bin:$PATH\n").unwrap();
        assert!(!ensure_path_in_rc(&hand_written, home.path(), &bin).unwrap());

        for not_first in [
            "# PATH=~/.npm-global/bin:$PATH\n",
            "export PATH=$PATH:~/.npm-global/bin\n",
            "export PATH=\"${PATH}:$HOME/.npm-global/bin\"\n",
            "echo ~/.npm-global/bin\n",
        ] {
            let rc = home.path().join(".bashrc");
            std::fs::write(&rc, not_first).unwrap();
            assert!(
                ensure_path_in_rc(&rc, home.path(), &bin).unwrap(),
                "{not_first}"
            );
        }
    }

    /// 改前缀走与 npx 同目录的那个 npm,改完由 `npm prefix -g` 读回。
    #[test]
    fn the_global_prefix_is_set_through_the_npm_next_to_npx() {
        let dir = tempfile::tempdir().unwrap();
        let npm = dir.path().join("npm");
        crate::test_support::write_test_executable(
            &npm,
            "#!/bin/sh\nstate=\"$(dirname \"$0\")/prefix\"\ncase \"$1 $2 $3\" in\n  'config set prefix') echo \"$4\" > \"$state\" ;;\n  'config delete prefix') rm -f \"$state\" ;;\n  'prefix -g '*) cat \"$state\" 2>/dev/null ;;\nesac\n",
        );
        let npx = dir.path().join("npx");
        let target = dir.path().join("home/.npm-global");

        set_global_prefix(&npx, &target).unwrap();
        assert_eq!(global_prefix(&npx), Some(target));
        unset_global_prefix(&npx).unwrap();
        assert_eq!(global_prefix(&npx), None);
    }

    #[test]
    fn only_an_executable_file_in_the_global_bin_counts() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::write(&claude, "#!/bin/sh\n").unwrap();
        assert_eq!(find_in_global_bin(dir.path(), "claude"), None);
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in_global_bin(dir.path(), "claude"), Some(claude));
        assert_eq!(find_in_global_bin(dir.path(), "codex"), None);
    }
}
