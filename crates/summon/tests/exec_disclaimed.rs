//! `summon __exec-disclaimed -- <program> <args…>`:summon 起 agent CLI 时的转手层。
//!
//! 它原地 exec 成 program,所以参数、环境、pid、退出码、信号都与直接起 program 一致;macOS 上
//! program 另外成为自己的隐私授权责任进程,它 fork 出的子进程把它当责任进程。

use std::ffi::OsStr;
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, Stdio};

const SUMMON: &str = env!("CARGO_BIN_EXE_summon");

fn via_launcher(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(SUMMON);
    command.arg("__exec-disclaimed").arg("--").arg(program);
    command
}

fn quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}

#[test]
fn arguments_pass_through_verbatim() {
    let args = ["", "a b", "it's", "$HOME", "--", "-x", "中文", "\"q\"", "*"];
    let output = via_launcher("/bin/sh")
        .arg("-c")
        .arg(r#"for a in "$@"; do printf '[%s]\n' "$a"; done"#)
        .arg("sh")
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let expected: String = args.iter().map(|arg| format!("[{arg}]\n")).collect();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
}

/// 转手层不能像 summon 主流程那样整理 PATH 或补环境变量:CLI 拿到的环境就是起它时给的那份。
#[test]
fn environment_passes_through_unchanged() {
    let output = via_launcher("/usr/bin/env")
        .env_clear()
        .env("PATH", "/nonexistent-summon-test")
        .env("SUMMON_TEST_VALUE", "x y")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut lines: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        // macOS 上 CoreFoundation 在加载它的进程里补上这个变量(值是本用户的默认文本编码,
        // 不含凭据);CLI 自己加载 CoreFoundation 时同样会补。
        .filter(|line| !line.starts_with("__CF_USER_TEXT_ENCODING="))
        .collect();
    lines.sort();
    assert_eq!(
        lines,
        ["PATH=/nonexistent-summon-test", "SUMMON_TEST_VALUE=x y"]
    );
}

#[test]
fn program_is_found_on_the_path_it_is_given() {
    let output = via_launcher("sh")
        .env_clear()
        .env("PATH", "/nonexistent-summon-test:/bin:/usr/bin")
        .args(["-c", "echo found"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"found\n");
}

#[test]
fn exit_code_passes_through() {
    let status = via_launcher("/bin/sh")
        .args(["-c", "exit 7"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(7));
}

/// exec 原地替换:CLI 的 pid 就是起转手层时拿到的 pid,发给它的信号直接到 CLI,退出状态也是
/// 被该信号终止。
#[test]
fn pid_is_kept_and_signals_reach_the_program() {
    let mut child = via_launcher("/bin/sh")
        .args(["-c", "echo $$; exec sleep 30"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    assert_eq!(line.trim(), child.id().to_string());
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(nix::libc::SIGTERM));
}

/// Rust 运行时把 SIGPIPE 设成忽略;转手层必须恢复默认处理,否则 CLI 写断开的管道时收不到
/// SIGPIPE、行为与直接起它不同。`yes` 被 SIGPIPE 终止时 shell 报 141,忽略时它自己以 1 退出。
#[test]
fn sigpipe_disposition_is_default_in_the_program() {
    let dir = tempfile::tempdir().unwrap();
    let status_file = dir.path().join("status");
    let script = r#"(yes; echo $? >&3) 3>"$1" | head -c 1 >/dev/null"#;
    for mut command in [Command::new("/bin/sh"), via_launcher("/bin/sh")] {
        let status = command
            .args(["-c", script, "sh"])
            .arg(&status_file)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&status_file).unwrap().trim(), "141");
    }
}

#[test]
fn missing_program_exits_127() {
    let output = via_launcher("summon-test-no-such-program")
        .env("PATH", "/nonexistent-summon-test")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&output.stderr).contains("summon-test-no-such-program"));
}

#[test]
fn missing_separator_is_a_usage_error() {
    let output = Command::new(SUMMON)
        .args(["__exec-disclaimed", "/bin/true"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

/// tmux pane 里的形态:shell 在命令串最后 `exec` 转手层,转手层再 exec 成 CLI,整条链同一个 pid。
#[test]
fn pane_shell_exec_chain_keeps_one_pid() {
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "exec {} __exec-disclaimed -- /bin/sh -c 'echo $$'",
            quote(SUMMON)
        ))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id().to_string();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), pid);
}

/// 探针:由下面的 macOS 测试以子进程方式重新执行本测试二进制,报告自己与父进程的 pid 和
/// 责任进程。直接运行时没有输出文件,什么也不做。
#[cfg(target_os = "macos")]
#[test]
fn responsibility_probe() {
    let Some(out) = std::env::var_os(macos::PROBE_OUT) else {
        return;
    };
    let pid = std::process::id() as i32;
    let parent = nix::unistd::getppid().as_raw();
    std::fs::write(
        out,
        format!(
            "{pid} {} {parent} {}",
            macos::responsible_pid(pid),
            macos::responsible_pid(parent)
        ),
    )
    .unwrap();
}

/// 扮演 agent CLI:不是系统自带的程序,经 `/bin/sh -c` 跑一条「工具命令」(这里是探针)。
/// 直接运行时没有命令,什么也不做。
#[cfg(target_os = "macos")]
#[test]
fn responsibility_tool_runner() {
    let Some(script) = std::env::var_os(macos::TOOL_SCRIPT) else {
        return;
    };
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg(std::env::current_exe().unwrap())
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use nix::libc;

    pub(super) const PROBE_OUT: &str = "SUMMON_TEST_RESPONSIBILITY_PROBE_OUT";
    pub(super) const TOOL_SCRIPT: &str = "SUMMON_TEST_RESPONSIBILITY_TOOL_SCRIPT";

    /// libsystem 未公开的 `responsibility_get_pid_responsible_for_pid`。找不到就让测试失败:
    /// 这组测试要在真的 macOS 上证明责任关系,不能静默跳过。
    pub(super) fn responsible_pid(pid: i32) -> i32 {
        type Get = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;
        // SAFETY:按名字查 libsystem 里签名为 `pid_t (pid_t)` 的函数,查不到时不调用。
        unsafe {
            let symbol = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_get_pid_responsible_for_pid".as_ptr(),
            );
            assert!(
                !symbol.is_null(),
                "系统里没有 responsibility_get_pid_responsible_for_pid"
            );
            std::mem::transmute::<*mut libc::c_void, Get>(symbol)(pid)
        }
    }

    struct Probe {
        launched: i32,
        pid: i32,
        responsible: i32,
        parent: i32,
        parent_responsible: i32,
    }

    /// 跑 `command`(最终会执行探针),返回直接起的那个子进程 pid 与探针报告。
    fn run_probe(mut command: Command) -> Probe {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("probe");
        let mut child = command.env(PROBE_OUT, &out).spawn().unwrap();
        let launched = child.id() as i32;
        let status = child.wait().unwrap();
        assert!(status.success(), "{status:?}");
        let report = std::fs::read_to_string(&out).unwrap();
        let fields: Vec<i32> = report.split(' ').map(|f| f.parse().unwrap()).collect();
        Probe {
            launched,
            pid: fields[0],
            responsible: fields[1],
            parent: fields[2],
            parent_responsible: fields[3],
        }
    }

    const PROBE: &str = "responsibility_probe --exact --nocapture --test-threads=1";

    fn test_args(name: &str) -> [&str; 4] {
        [name, "--exact", "--nocapture", "--test-threads=1"]
    }

    fn me() -> std::path::PathBuf {
        std::env::current_exe().unwrap()
    }

    /// 不经转手:子进程继承本测试进程的责任进程 —— 这正是 agent 的授权记到 summon 名下的原因。
    #[test]
    fn plain_spawn_inherits_the_parents_responsible_process() {
        let ours = responsible_pid(std::process::id() as i32);
        assert!(ours > 0, "{ours}");
        let mut command = Command::new(me());
        command.args(test_args("responsibility_probe"));
        let probe = run_probe(command);
        assert_eq!(probe.pid, probe.launched);
        assert_eq!(probe.responsible, ours);
        assert_ne!(probe.responsible, probe.pid);
    }

    /// 经转手:CLI(pid 不变)成为自己的责任进程。
    #[test]
    fn launched_program_is_its_own_responsible_process() {
        let mut command = via_launcher(me());
        command.args(test_args("responsibility_probe"));
        let probe = run_probe(command);
        assert_eq!(probe.pid, probe.launched);
        assert_eq!(probe.responsible, probe.pid);
    }

    /// 责任关系挂在 pid 上:shebang 脚本、`env`、shell 这类启动层最后 exec 成 CLI 时,CLI 仍是
    /// 自己负责。
    #[test]
    fn responsibility_survives_exec() {
        let mut command = via_launcher("/bin/sh");
        command
            .args(["-c", &format!(r#"exec /usr/bin/env "$0" {PROBE}"#)])
            .arg(me());
        let probe = run_probe(command);
        assert_eq!(probe.pid, probe.launched);
        assert_eq!(probe.responsible, probe.pid);
    }

    /// agent 跑工具的形态:CLI(非系统程序)经 shell 起工具(`; exit $?` 让 shell 留着、不直接 exec
    /// 成工具),shell 与工具都以 CLI 为责任进程,
    /// 授权弹窗与记录落在 CLI 上,而不是 summon,也不是各个工具。
    #[test]
    fn tools_run_by_the_cli_report_the_cli_as_responsible() {
        let mut command = via_launcher(me());
        command
            .args(test_args("responsibility_tool_runner"))
            .env(TOOL_SCRIPT, format!(r#""$0" {PROBE}; exit $?"#));
        let probe = run_probe(command);
        assert_ne!(probe.pid, probe.launched);
        assert_ne!(probe.parent, probe.launched, "探针的父进程是工具 shell");
        assert_eq!(probe.parent_responsible, probe.launched);
        assert_eq!(probe.responsible, probe.launched);
    }
}
