//! macOS:summon 拉起的 agent CLI 自己当隐私授权的责任进程。
//!
//! TCC 把隐私授权(语音识别、麦克风、桌面与外接盘等文件夹……)记在发起请求那个进程的**责任进程**
//! 上,弹窗也以它的名义出现。责任关系随 fork/exec 一路继承:daemon 由 launchd 拉起、自己负责自己,
//! 它起的 tmux server、无头 CLI 以及 CLI 跑的每个工具子进程都追到 daemon,于是 agent 申请的权限
//! 全都显示并记在 summon 名下。
//!
//! 起 agent 时经 `summon __exec-disclaimed -- <program> <args…>` 转一手:这一层用
//! `posix_spawn` + `POSIX_SPAWN_SETEXEC` 原地换成 program,同时以
//! `responsibility_spawnattrs_setdisclaim` 声明不再替它负责。pid 不变,program 成为自己的责任进程;
//! 它之后 exec 成的映像、fork 出的工具子进程都继承这份责任,TCC 按责任进程当时的可执行文件认人,
//! 弹窗与授权记录于是落在 claude / codex / cursor-agent 等各自的 CLI 上。
//!
//! 转手程序用 exec 原地替换而不是再起子进程:pid、进程组、父子关系、信号与退出码都与直接起 CLI
//! 一致,按 pid / pgid / 进程树做的存活判定、打断与回收不受影响。
//!
//! `responsibility_spawnattrs_setdisclaim` 是 libsystem 未公开的符号,只按名字在运行时解析;解析
//! 不到就照原样起 CLI(记一次日志),绝不因此让 agent 起不来。其余平台没有这层,命令逐字节不变。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 转手起 agent 的隐藏子命令。`main` 在解析参数、改环境之前就分流到 [`exec_main`]。
pub(crate) const SUBCOMMAND: &str = "__exec-disclaimed";

/// 现在起 agent 要不要转手、经谁转手。不是 macOS、配置关掉、系统没有那个符号或找不到 summon
/// 可执行文件时为 `None`,调用方照原样起。每次起 agent 现读配置,改了下一次启动就生效。
pub(crate) fn launcher() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(launcher) = tests::launcher_override() {
        return Some(launcher);
    }
    if !cfg!(target_os = "macos")
        || !summon_core::config::Config::load()
            .macos
            .agent_privacy_identity
    {
        return None;
    }
    if !sys::disclaim_supported() {
        static MISSING: std::sync::Once = std::sync::Once::new();
        MISSING.call_once(|| {
            crate::observe::log::log(
                "agent privacy identity: responsibility_spawnattrs_setdisclaim unavailable; \
                 agents keep summon as their responsible process",
            )
        });
        return None;
    }
    // libtest 二进制不认这个子命令,测试进程只经上面的覆盖转手。
    if cfg!(test) {
        return None;
    }
    let binary = std::env::current_exe().ok()?;
    is_executable(&binary).then_some(binary)
}

/// tmux pane 里 `exec` 的那段命令串:转手时在最前面加上 `<summon> __exec-disclaimed --`,
/// 后面原样不动,由同一个 shell 照旧拆词、展开、按 PATH 找程序。
pub(crate) fn shell_command(cmd: &str) -> String {
    shell_command_via(launcher().as_deref(), cmd)
}

fn shell_command_via(launcher: Option<&Path>, cmd: &str) -> String {
    match launcher {
        Some(launcher) => format!(
            "{} {SUBCOMMAND} -- {cmd}",
            summon_core::launch_env::sh_single_quote(&launcher.to_string_lossy())
        ),
        None => cmd.to_string(),
    }
}

/// 起 agent CLI 的 `Command`,用法同 `Command::new(program)`。`path` 是子进程环境里的 PATH
/// (程序名不带 `/` 时按它找)。找不到程序时不转手,原样交给 `Command::new`:spawn 照旧报
/// NotFound,不变成转手程序的退出码 127。
pub(crate) fn command(program: impl AsRef<OsStr>, path: Option<&OsStr>) -> Command {
    command_via(launcher().as_deref(), program.as_ref(), path)
}

fn command_via(launcher: Option<&Path>, program: &OsStr, path: Option<&OsStr>) -> Command {
    match launcher {
        Some(launcher) if resolve(program, path).is_some() => {
            let mut command = Command::new(launcher);
            command.arg(SUBCOMMAND).arg("--").arg(program);
            command
        }
        _ => Command::new(program),
    }
}

/// 子进程环境里最终生效的 PATH(同名键后写覆盖)。
pub(crate) fn path_in<'a, K, V>(env: impl IntoIterator<Item = &'a (K, V)>) -> Option<&'a OsStr>
where
    K: AsRef<OsStr> + 'a,
    V: AsRef<OsStr> + 'a,
{
    env.into_iter()
        .filter(|(key, _)| key.as_ref() == OsStr::new("PATH"))
        .last()
        .map(|(_, value)| value.as_ref())
}

/// 与 `execvp` 同一判据:带 `/` 的按路径,否则逐个 PATH 目录找可执行文件;没有 PATH 时用系统
/// 默认搜索路径。
fn resolve(program: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    if program.as_bytes().contains(&b'/') {
        let candidate = PathBuf::from(program);
        return is_executable(&candidate).then_some(candidate);
    }
    if program.is_empty() {
        return None;
    }
    let search = path.unwrap_or(OsStr::new("/usr/bin:/bin"));
    std::env::split_paths(search)
        .map(|dir| {
            if dir.as_os_str().is_empty() {
                PathBuf::from(".").join(program)
            } else {
                dir.join(program)
            }
        })
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// `summon __exec-disclaimed -- <program> <args…>`:放弃对 program 的责任后原地 exec 成它。
/// 成功不返回;exec 失败按 shell 的惯例退出(找不到 127,其余 126)。
pub(crate) fn exec_main() -> ! {
    let mut args = std::env::args_os().skip(2);
    let program = match (args.next(), args.next()) {
        (Some(separator), Some(program)) if separator == "--" => program,
        _ => {
            eprintln!("usage: summon {SUBCOMMAND} -- <program> [args...]");
            std::process::exit(2);
        }
    };
    let args: Vec<OsString> = args.collect();
    let error = sys::exec_disclaimed(&program, &args);
    eprintln!("summon: {}: {error}", program.to_string_lossy());
    std::process::exit(if error.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    });
}

/// 没有 disclaim 时的 exec:与直接起 CLI 等价。std 在 exec 前把 SIGPIPE 恢复成默认处理
/// (Rust 运行时启动时把它设成了忽略),程序按当前 PATH 查找。
fn plain_exec(program: &OsStr, args: &[OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;
    Command::new(program).args(args).exec()
}

#[cfg(target_os = "macos")]
mod sys {
    use nix::libc;
    use std::ffi::{c_char, c_int, CString, OsStr, OsString};
    use std::os::unix::ffi::OsStrExt as _;

    type SetDisclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, c_int) -> c_int;

    fn setdisclaim() -> Option<SetDisclaim> {
        // SAFETY:RTLD_DEFAULT 按名字在已加载的镜像里查符号;查到的是 libsystem 里签名为
        // `int (posix_spawnattr_t *, int)` 的函数。
        unsafe {
            let symbol = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_spawnattrs_setdisclaim".as_ptr(),
            );
            (!symbol.is_null())
                .then(|| std::mem::transmute::<*mut libc::c_void, SetDisclaim>(symbol))
        }
    }

    pub(super) fn disclaim_supported() -> bool {
        setdisclaim().is_some()
    }

    pub(super) fn exec_disclaimed(program: &OsStr, args: &[OsString]) -> std::io::Error {
        let Some(setdisclaim) = setdisclaim() else {
            return super::plain_exec(program, args);
        };
        let to_c = |raw: &OsStr| {
            CString::new(raw.as_bytes()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "argument contains NUL")
            })
        };
        let argv = match std::iter::once(program)
            .chain(args.iter().map(OsString::as_os_str))
            .map(to_c)
            .collect::<Result<Vec<CString>, _>>()
        {
            Ok(argv) => argv,
            Err(error) => return error,
        };
        let mut pointers: Vec<*mut c_char> =
            argv.iter().map(|arg| arg.as_ptr() as *mut c_char).collect();
        pointers.push(std::ptr::null_mut());
        // SAFETY:attr 先 init 再用、用完 destroy;argv 与 environ 都以 NULL 结尾,且在调用期间
        // 一直存活。POSIX_SPAWN_SETEXEC 成功时不返回,失败时本进程原样继续。
        unsafe {
            let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
            let rc = libc::posix_spawnattr_init(&mut attr);
            if rc != 0 {
                return std::io::Error::from_raw_os_error(rc);
            }
            // Rust 运行时把 SIGPIPE 设成了忽略,exec 会把「忽略」带给 CLI;恢复成默认处理,与
            // std 起子进程时一致。其余被本进程接管的信号在 exec 时本来就回到默认。
            let mut defaults: libc::sigset_t = 0;
            libc::sigemptyset(&mut defaults);
            libc::sigaddset(&mut defaults, libc::SIGPIPE);
            let mut rc = libc::posix_spawnattr_setsigdefault(&mut attr, &defaults);
            if rc == 0 {
                rc = libc::posix_spawnattr_setflags(
                    &mut attr,
                    (libc::POSIX_SPAWN_SETEXEC | libc::POSIX_SPAWN_SETSIGDEF) as libc::c_short,
                );
            }
            if rc == 0 {
                rc = setdisclaim(&mut attr, 1);
            }
            if rc == 0 {
                let mut pid: libc::pid_t = 0;
                rc = libc::posix_spawnp(
                    &mut pid,
                    argv[0].as_ptr(),
                    std::ptr::null(),
                    &attr,
                    pointers.as_ptr(),
                    *libc::_NSGetEnviron(),
                );
            }
            libc::posix_spawnattr_destroy(&mut attr);
            std::io::Error::from_raw_os_error(rc)
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod sys {
    use std::ffi::{OsStr, OsString};

    pub(super) fn disclaim_supported() -> bool {
        false
    }

    pub(super) fn exec_disclaimed(program: &OsStr, args: &[OsString]) -> std::io::Error {
        super::plain_exec(program, args)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn shell_command_is_unchanged_without_launcher() {
        let cmd = "claude --resume id \"--model\" \"m\" --settings '/a b/c.json'";
        assert_eq!(shell_command_via(None, cmd), cmd);
    }

    #[test]
    fn shell_command_prefixes_quoted_launcher_and_keeps_the_rest_verbatim() {
        let cmd = "claude --resume id \"--name\" \"summon:soul-k\"";
        assert_eq!(
            shell_command_via(Some(Path::new("/Apps/it's here/summon")), cmd),
            format!("'/Apps/it'\\''s here/summon' __exec-disclaimed -- {cmd}")
        );
    }

    #[test]
    fn command_without_launcher_is_the_plain_program() {
        let command = command_via(None, OsStr::new("sh"), Some(OsStr::new("/bin:/usr/bin")));
        assert_eq!(command.get_program(), "sh");
        assert_eq!(command.get_args().count(), 0);
    }

    #[test]
    fn command_with_launcher_prepends_the_subcommand() {
        let command = command_via(
            Some(Path::new("/opt/summon")),
            OsStr::new("sh"),
            Some(OsStr::new("/nonexistent:/bin:/usr/bin")),
        );
        assert_eq!(command.get_program(), "/opt/summon");
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            [OsStr::new(SUBCOMMAND), OsStr::new("--"), OsStr::new("sh")]
        );
    }

    /// 找不到程序时不转手:spawn 要照旧以 NotFound 失败,而不是起来一个立刻以 127 退出的转手进程。
    #[test]
    fn unresolvable_program_is_not_wrapped_and_still_fails_to_spawn() {
        let launcher = Path::new("/bin/sh");
        for (program, path) in [
            ("summon-test-no-such-cli", Some(OsStr::new("/bin:/usr/bin"))),
            ("/nonexistent/claude", None),
            ("", None),
        ] {
            let mut command = command_via(Some(launcher), OsStr::new(program), path);
            assert_eq!(command.get_program(), program);
            if !program.is_empty() {
                let error = command.spawn().unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{program}");
            }
        }
    }

    #[test]
    fn resolve_follows_execvp_rules() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "#!/bin/sh\n").unwrap();
        let tool = dir.path().join("tool");
        crate::test_support::write_test_executable(&tool, "#!/bin/sh\n");
        let path = std::env::join_paths([Path::new("/nonexistent"), dir.path()]).unwrap();
        assert_eq!(resolve(OsStr::new("tool"), Some(&path)), Some(tool.clone()));
        assert_eq!(resolve(tool.as_os_str(), None), Some(tool));
        // 不可执行的文件、目录都不算。
        assert_eq!(resolve(OsStr::new("plain"), Some(&path)), None);
        assert_eq!(resolve(plain.as_os_str(), None), None);
        assert_eq!(resolve(dir.path().as_os_str(), None), None);
        assert!(
            resolve(OsStr::new("sh"), None).is_some(),
            "没有 PATH 时按系统默认路径"
        );
    }

    #[test]
    fn path_in_takes_the_last_assignment() {
        let env = vec![
            ("PATH".to_string(), "/first".to_string()),
            ("HOME".to_string(), "/h".to_string()),
            ("PATH".to_string(), "/second".to_string()),
        ];
        assert_eq!(path_in(&env), Some(OsStr::new("/second")));
        let none: Vec<(String, String)> = vec![("HOME".into(), "/h".into())];
        assert_eq!(path_in(&none), None);
    }

    thread_local! {
        static LAUNCHER: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn launcher_override() -> Option<PathBuf> {
        LAUNCHER.with(|launcher| launcher.borrow().clone())
    }

    /// 本线程接下来起 agent 时经 `launcher` 转手(不看平台、配置与符号),guard 析构时撤掉。
    pub(crate) fn override_launcher(launcher: &Path) -> impl Drop {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                LAUNCHER.with(|launcher| launcher.borrow_mut().take());
            }
        }
        LAUNCHER.with(|slot| *slot.borrow_mut() = Some(launcher.to_path_buf()));
        Reset
    }

    /// 测试进程里不转手:libtest 二进制不认这个子命令,其它测试起的子进程与命令串必须与改动前一致。
    #[test]
    fn tests_do_not_wrap_without_an_override() {
        assert_eq!(launcher(), None);
        assert_eq!(shell_command("claude"), "claude");
        let _launcher = override_launcher(Path::new("/opt/summon"));
        assert_eq!(launcher().as_deref(), Some(Path::new("/opt/summon")));
    }

    #[test]
    fn switch_defaults_on_and_can_be_turned_off() {
        assert!(
            summon_core::config::Config::default()
                .macos
                .agent_privacy_identity
        );
        let off: summon_core::config::Config =
            serde_yaml::from_str("macos:\n  agent_privacy_identity: false\n").unwrap();
        assert!(!off.macos.agent_privacy_identity);
        let other: summon_core::config::Config = serde_yaml::from_str("poll_ms: 300\n").unwrap();
        assert!(other.macos.agent_privacy_identity);
    }

    /// 起 agent CLI 的每条路都经本模块:新增或改写这些启动点时漏掉它,agent 的隐私授权就又记回
    /// summon 名下,而 Linux 上的测试看不出差别。
    #[test]
    fn agent_spawn_sites_go_through_this_module() {
        let src = env!("CARGO_MANIFEST_DIR");
        for (file, needle) in [
            (
                "src/session/session.rs",
                "responsibility::shell_command(&cmd)",
            ),
            (
                "src/runtime/executor.rs",
                "responsibility::command(&spec.program",
            ),
            (
                "src/codex/codex_app_server.rs",
                "responsibility::command(\n            &config.program",
            ),
            (
                "src/capture/cursor_acp.rs",
                "responsibility::command(\n            &config.program",
            ),
            (
                "src/runtime/insights_analysis.rs",
                "responsibility::command(\n        agent.cli_cmd()",
            ),
            (
                "src/runtime/daemon/document_comment_draft.rs",
                "responsibility::command(\n        agent.cli_cmd()",
            ),
        ] {
            let body = std::fs::read_to_string(Path::new(src).join(file)).unwrap();
            assert!(
                body.contains(needle),
                "{file} 应经 responsibility 起 agent CLI"
            );
        }
    }

    /// 转手时 executor 起的就是 `<launcher> __exec-disclaimed -- <program> <args…>`,环境、
    /// 工作目录与进程组照旧。
    #[test]
    fn executor_spawns_agents_through_the_launcher() {
        use crate::runtime::executor::{Executor as _, LocalExecutor, SpawnSpec};
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv");
        let launcher = dir.path().join("launcher");
        crate::test_support::write_test_executable(
            &launcher,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nshift 2\nexec \"$@\"\n",
                record.display()
            ),
        );
        let _launcher = override_launcher(&launcher);
        let script = r#"printf '%s\n%s\n%s' "$$" "$(ps -o pgid= -p $$ | tr -d ' ')" "$PWD""#;
        let spec = SpawnSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            work_dir: Some(dir.path().display().to_string()),
            stdin_null: true,
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            own_process_group: true,
        };
        let child = LocalExecutor.spawn(&spec).unwrap();
        let pid = child.id().to_string();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            format!("{SUBCOMMAND}\n--\nsh\n-c\n{script}\n")
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        // 转手层 exec 成 CLI:pid 与进程组都是 executor 起的那个子进程。
        assert_eq!(lines[0], pid);
        assert_eq!(lines[1], pid);
        assert_eq!(
            std::fs::canonicalize(lines[2]).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }
}
