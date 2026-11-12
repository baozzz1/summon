//! runtime 根路径(A1:从 daemon.rs 下沉)。与 daemon 行为无关的纯路径拼装收口一处,
//! 让 cli/setup 等不必反向依赖 daemon god module。

use std::path::PathBuf;

/// summon 根目录 `~/.summon`。
pub fn summon_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".summon")
}

pub fn run_dir() -> PathBuf {
    summon_home().join("run")
}

pub fn events_sock() -> PathBuf {
    run_dir().join("events.sock")
}

/// 外部 Claude bridge 的状态与不可变 Hook spool。
pub fn external_bridge_dir() -> PathBuf {
    run_dir().join("external-bridge")
}

/// 本机命令面 socket：本机 CLI（actor=Owner）经此把 `Command` 发给 daemon
/// → `Core::submit` → 回 `CommandOutcome`。与 events.sock(sidecar 入站)分离:control 是本机出站命令。
pub fn control_sock() -> PathBuf {
    run_dir().join("control.sock")
}

pub fn pid_file() -> PathBuf {
    run_dir().join("daemon.pid")
}

/// daemon 就绪哨兵。内容是本代 daemon PID；只有 signal handler、dispatcher、两条 socket
/// 与 scheduler 都已经就绪后才写。`daemon.pid` 表示进程身份，本文件表示可对外服务。
pub fn ready_file() -> PathBuf {
    run_dir().join("daemon.ready")
}

/// 正在运行的 daemon 代际与构建版本。PID 绑定防止旧版本文件冒充 KeepAlive 拉起的新进程。
pub fn daemon_version_file() -> PathBuf {
    run_dir().join("daemon.version")
}

/// 干净退出哨兵:`run()` 优雅停机末尾写 versioned JSON、启动时读后删。**存在 = 上次正常停机**;
/// **缺失 = 上次崩溃**(SIGKILL/panic-abort 跑不到末尾写)。JSON 内每个 mirror byte fence 只签
/// stop snapshot 时已追平 EOF 的 chat；旧版只有 `clean` 时仍认正常退出，但 fail-closed 不主动补发。
pub fn clean_shutdown_marker() -> PathBuf {
    run_dir().join("clean_shutdown")
}

/// daemon 单例锁文件路径(`~/.summon/run/daemon.lock`)。
pub(crate) fn lock_file() -> PathBuf {
    run_dir().join("daemon.lock")
}

/// 本代启动记录:拿到单例锁后写、就绪时补上就绪时刻、优雅停机末尾删。**还在 = 上一代没走完
/// 停机**(崩溃、被杀、启动期出错退出)。自动拉起的熔断据它判断上一代怎么死的,见 `daemon::crash_guard`。
pub fn boot_record_file() -> PathBuf {
    run_dir().join("daemon.boot")
}

/// 自动拉起熔断最近一次拦下启动的说明(时刻 + 原因)。`summon doctor` 展示,下一次放行的启动删掉它。
pub fn refusal_file() -> PathBuf {
    run_dir().join("daemon.refused")
}

/// 用户那一侧的 PATH:`summon daemon start` 存下,daemon 启动时补齐自己缺的目录(见 `setup::env`)。
pub fn user_path_file() -> PathBuf {
    run_dir().join("user-path")
}

/// 人工启动请求:`summon daemon start` 在交给自启单元或自己 spawn 之前写,daemon 启动时读后删。
/// 有它 = 这次启动是人发起的,不受自动拉起熔断约束。
pub fn start_request_file() -> PathBuf {
    run_dir().join("start-request")
}

/// 人工停机请求:`summon daemon stop` 在发 SIGTERM 之前写,点名被停的那一代。有它 = 那一代是人
/// 经 CLI 停的,不是自己退的。
pub fn stop_request_file() -> PathBuf {
    run_dir().join("stop-request")
}
