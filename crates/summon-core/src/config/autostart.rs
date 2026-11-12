//! 启动项(login item / autostart)的**纯逻辑**:开机自启的状态 schema + launchd plist 渲染。
//! 不碰文件系统 / launchctl(那些在 `summon` 的 `setup::autostart`)。
//!
//! 设计点:
//! - 两个后端:macOS launchd LaunchAgent、Linux systemd `--user` service。`LoginItemBackend`
//!   按平台分派,CLI / Core 的调用面不变。
//! - 状态(开没开、是否 KeepAlive)存**专属小文件**(`~/.summon/run/autostart.json`),不写进满是注释、
//!   手维护的 `config.yaml`(serde 往返会抹注释)。
//! - 单元自带**崩溃拉起**:launchd `KeepAlive.SuccessfulExit=false`、systemd `Restart=on-failure`,
//!   只在异常退出(panic、被信号杀死、出错退出)时拉起。`summon daemon stop` 让 daemon 优雅退出、
//!   退出码 0,不会被复活;daemon 自己判定「不该起」时也以 0 退出,单元就此停手(见 daemon 的
//!   `crash_guard`)。
//! - `keep_alive` 是更强的 opt-in:连正常退出也拉起,`daemon stop` 因此停不下来,
//!   `deploy.sh` 遇到它会拒绝部署。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// launchd LaunchAgent 的 Label(= plist 文件名去掉 `.plist`)。bootout 时也用它定位。
pub const LAUNCHD_LABEL: &str = "com.summon.daemon";

/// systemd `--user` 单元名(含 `.service` 后缀,systemctl 直接用它定位)。
pub const SYSTEMD_UNIT_NAME: &str = "summon-daemon.service";

/// daemon 进程的 argv[0]。进程按功能命名:ps 里 daemon 是 `summond`,CLI 是 `summon`,
/// 无论它是 `daemon start` 起的还是 launchd 拉起的。
pub const DAEMON_PROCESS_NAME: &str = "summond";

/// 开机自启的持久状态(存专属小文件)。两者默认 false = 没开自启。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AutostartState {
    /// 是否已装开机自启(写了 plist 并 load)。
    #[serde(default)]
    pub enabled: bool,
    /// 是否连**正常退出**也拉起。默认 false:macOS 上仍会在崩溃时拉起,`daemon stop` 能停。
    #[serde(default)]
    pub keep_alive: bool,
}

/// 登录项后端(平台接缝)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginItemBackend {
    /// macOS:`~/Library/LaunchAgents/<label>.plist`,`launchctl bootstrap/bootout gui/$UID`。
    LaunchdUser,
    /// Linux:`~/.config/systemd/user/<unit>`,`systemctl --user enable/disable` + `loginctl enable-linger`。
    SystemdUser,
}

impl LoginItemBackend {
    /// 本机默认后端。
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            LoginItemBackend::LaunchdUser
        } else {
            LoginItemBackend::SystemdUser
        }
    }
}

/// plist 文件名(`<label>.plist`)。
pub fn plist_file_name() -> String {
    format!("{LAUNCHD_LABEL}.plist")
}

/// 本机「工具目录」候选(纯数据,平台相关)。
///
/// 这些是 `tmux` 和 agent CLI(claude / codex / cursor-agent)的常住地:
/// - macOS:Homebrew 默认前缀 `/opt/homebrew`(Apple Silicon)、`/usr/local`(Intel)。
/// - Linux:Homebrew(Linuxbrew)默认前缀 `/home/linuxbrew/.linuxbrew`(共享装)、`~/.linuxbrew`(单用户装)。
/// - 两边通用:`~/.local/bin`(pipx / 官方安装脚本 / `npm --prefix`)。
///
/// **只列官方默认前缀。** Homebrew 允许装到任意自定义前缀(实测 devbox 上装在 `~/homebrew`),
/// 那种情况靠 `summon::setup::env` 的登录 shell 兜底,不在这里堆个人路径 —— 枚举永远追不完,
/// 而且每加一条都是把「某台机器的偶然」固化成代码里的常量。
pub fn tool_path_candidates(home: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push("/opt/homebrew/bin".to_string());
    } else {
        dirs.push("/home/linuxbrew/.linuxbrew/bin".to_string());
        if !home.is_empty() {
            dirs.push(format!("{home}/.linuxbrew/bin"));
            // **homebrew 直接装在家目录**(`~/homebrew`,不带点)。这是 2026-08-07 devbox 现场
            // 实际用的布局:tmux 与 claude 都只在 `~/homebrew/bin`,而 `.linuxbrew` 两条都不命中。
            // 登录 shell 兜底在那台机器上也补不出来(实测其 PATH 里同样没有),所以这一条必须显式列。
            dirs.push(format!("{home}/homebrew/bin"));
        }
    }
    dirs.push("/usr/local/bin".to_string());
    if !home.is_empty() {
        dirs.push(format!("{home}/.local/bin"));
    }
    dirs
}

/// 把常见工具目录补进 `current` PATH(纯函数,daemon 启动期用)。
///
/// daemon 常被 launchd(给的是最小 PATH `/usr/bin:/bin:/usr/sbin:/sbin`)、cron、GUI 启动器、
/// 非交互 ssh 拉起,这种 PATH 里**没有** Homebrew 的 `tmux`、也没有 `~/.local/bin` 的 `claude`,
/// 于是 `Command::new("tmux")` / tmux 内起 `claude` 都 ENOENT(实测「起会话失败: No such file or
/// directory」)。这里在现有 PATH **前置**缺失的工具目录(已存在则不动、保序、去重),让 daemon 及其
/// 整棵子进程树(tmux + tmux 内的 CLI)都继承到可用 PATH。`home` 空则跳过带 `~` 的项;`current`
/// 空(病态)则回退基础系统目录。
pub fn ensure_tool_path(current: &str, home: &str) -> String {
    let tool_dirs = tool_path_candidates(home);
    let existing: Vec<String> = if current.is_empty() {
        ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        current
            .split(':')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    };
    // 只前置 existing 里没有的工具目录;其余按原序在后。
    let mut out: Vec<String> = tool_dirs
        .into_iter()
        .filter(|d| !existing.contains(d))
        .collect();
    out.extend(existing);
    out.join(":")
}

/// 把 `extra` 里 `current` 没有的目录**追加到末尾**(保序、去重)。
///
/// 与 `ensure_tool_path` 的「前置」相反,这是有意的:`extra` 来自登录 shell 那种兜底来源,只该补
/// 「当前 PATH 一个都找不到」的缺口,不该抢在既有条目前面改变已经生效的解析优先级。
pub fn append_missing_dirs(current: &str, extra: &str) -> String {
    let mut out: Vec<String> = current
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    for dir in extra.split(':').filter(|s| !s.is_empty()) {
        if !out.iter().any(|e| e == dir) {
            out.push(dir.to_string());
        }
    }
    out.join(":")
}

/// 渲染一份 launchd LaunchAgent plist(纯字符串,不落盘)。
/// - `exe`:summon 可执行文件绝对路径(`Program`);argv 是 `summond daemon run` = daemon 真入口,
///   与 `daemon start` 自己 spawn 时的进程名一致。
/// - `run_at_load`:登录即起(开机自启的核心)。
/// - `keep_alive`:`true` = 任何退出都拉起;`false` = 只在异常退出时拉起(崩溃自动恢复)。
/// - `log`:stdout/stderr 落盘路径(与 `daemon_start` 的 `~/.summon/run/daemon.log` 一致)。
pub fn render_launchd_plist(
    label: &str,
    exe: &Path,
    run_at_load: bool,
    keep_alive: bool,
    log: &Path,
) -> String {
    // plist 是 XML:路径里若含 `&`/`<`/`>`(macOS 路径合法)直接插值会生成坏 plist,launchd 加载失败。
    // 先转义(`&` 必须最先替,否则把后续实体的 `&` 二次转义)。label 是固定常量,无需转义。
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    let exe = xml_escape(&exe.to_string_lossy());
    let log = xml_escape(&log.to_string_lossy());
    let bool_tag = |b: bool| if b { "<true/>" } else { "<false/>" };
    let keep = if keep_alive {
        "<true/>".to_string()
    } else {
        // 退出码 0 不拉起:`daemon stop` 的优雅退出与 daemon 自己的「不该起」都以 0 退出。
        "<dict>\n        <key>SuccessfulExit</key>\n        <false/>\n    </dict>".to_string()
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>Program</key>
    <string>{exe}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{DAEMON_PROCESS_NAME}</string>
        <string>daemon</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key>
    {run}
    <key>KeepAlive</key>
    {keep}
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        run = bool_tag(run_at_load),
    )
}

/// 渲染一份 systemd `--user` service 单元(纯字符串,不落盘)。
///
/// - `exe`:summon 可执行文件绝对路径;argv 是 `summond daemon run`(`@` 前缀让第二个词成为
///   argv[0]),与 `daemon start` 自己 spawn 时的进程名一致。前台跑,故 `Type=simple`。
/// - `restart_always`:`true` = 任何退出都拉起(`Restart=always`);`false` = 只在异常退出时拉起。
/// - `log`:stdout/stderr 落盘路径(与 `daemon_start` 的 `~/.summon/run/daemon.log` 一致)。
///
/// **`KillMode=process` 是硬要求**:systemd 默认在主进程退出、被重启或被 stop 时杀掉整个 cgroup,
/// 而 daemon 起的 tmux server 就在这个 cgroup 里 —— 默认值下每次 `daemon stop` / 崩溃拉起都会带走
/// 全部 agent 会话(devbox 实测:`control-group` 下 tmux 随之消失,`process` 下存活)。只杀主进程,
/// 其余子进程(无头轮自成进程组、RPC 会话)由 daemon 自己的停机与孤儿回收收拾。
///
/// **没开 linger 不起**(`ConditionPathExists=/var/lib/systemd/linger/%u`):user 实例随最后一个
/// 登录会话停止,停止时把自己 cgroup 里剩下的进程整组 SIGKILL,`KillMode=process` 挡不住这一层,
/// daemon 起的 tmux 会话全在里面。条件写在单元里,登录时经 `default.target` 拉起这条入口也一样被挡。
///
/// OOM 调整值写 0:user 实例给单元的默认值是 200(直接 spawn 的 daemon 是 0),daemon 及其整棵
/// 子进程树(tmux、agent)会因此先于同机别的进程被选中。非特权单元调不到 user 管理器自己的值以下
/// —— devbox 上它是 100,写 0 得到的是 100,单元照常起;管理器是 0 的机器上就是 0。
///
/// **`WantedBy=default.target` 只保证「用户实例起来时跟着起」,不保证开机就起** —— user 实例
/// 默认要等这个用户第一次登录才创建,最后一个会话退出就销毁。所以 IO 侧还要 `loginctl
/// enable-linger`,否则在 devbox 这种「ssh 上去干活、干完就断」的机器上,daemon 会跟着 ssh 一起没。
pub fn render_systemd_unit(exe: &Path, restart_always: bool, log: &Path) -> String {
    // systemd 单元文件里 `%` 是说明符(`%h` 等)的引导字符,路径里若真有 `%` 必须写成 `%%`。
    // 其余字符不需要转义:ExecStart 走的是 systemd 自己的分词,不经 shell。
    fn escape(s: &str) -> String {
        s.replace('%', "%%")
    }
    let exe = escape(&exe.to_string_lossy());
    let log = escape(&log.to_string_lossy());
    let restart = if restart_always {
        "always"
    } else {
        "on-failure"
    };
    format!(
        "[Unit]\n\
         Description=summon daemon\n\
         After=default.target\n\
         ConditionPathExists=/var/lib/systemd/linger/%u\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=@{exe} {DAEMON_PROCESS_NAME} daemon run\n\
         Restart={restart}\n\
         RestartSec=3\n\
         KillMode=process\n\
         OOMScoreAdjust=0\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn plist_reflects_flags_and_paths() {
        let exe = PathBuf::from("/usr/local/bin/summon");
        let log = PathBuf::from("/Users/x/.summon/run/daemon.log");
        let p = render_launchd_plist(LAUNCHD_LABEL, &exe, true, false, &log);
        assert!(p.contains("<key>Label</key>\n    <string>com.summon.daemon</string>"));
        assert!(p.contains("<key>Program</key>\n    <string>/usr/local/bin/summon</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(p.contains("/Users/x/.summon/run/daemon.log"));
        // keep_alive=true → 任何退出都拉起。
        let p2 = render_launchd_plist(LAUNCHD_LABEL, &exe, true, true, &log);
        assert!(p2.contains("<key>KeepAlive</key>\n    <true/>"), "{p2}");
    }

    #[test]
    fn plist_runs_the_daemon_under_its_process_name() {
        // launchd 拉起的与 `daemon start` spawn 的必须同名:deploy 与 ps 都按 `summond daemon run` 认。
        let p = render_launchd_plist(
            LAUNCHD_LABEL,
            &PathBuf::from("/x/summon"),
            true,
            false,
            &PathBuf::from("/x.log"),
        );
        assert!(
            p.contains(
                "<array>\n        <string>summond</string>\n        <string>daemon</string>\n        <string>run</string>\n    </array>"
            ),
            "{p}"
        );
    }

    #[test]
    fn default_plist_restarts_only_after_an_abnormal_exit() {
        let p = render_launchd_plist(
            LAUNCHD_LABEL,
            &PathBuf::from("/x/summon"),
            true,
            false,
            &PathBuf::from("/x.log"),
        );
        assert!(
            p.contains(
                "<key>KeepAlive</key>\n    <dict>\n        <key>SuccessfulExit</key>\n        <false/>\n    </dict>"
            ),
            "{p}"
        );
        // deploy.sh 的闸只拦「KeepAlive 键后紧跟 <true/>」(任何退出都拉起,`daemon stop` 停不下来)。
        // 默认单元必须不触发它,否则开了自启的机器每次部署都被拒。
        let lines: Vec<&str> = p.lines().collect();
        let key = lines
            .iter()
            .position(|line| line.trim() == "<key>KeepAlive</key>")
            .unwrap();
        assert_ne!(lines[key + 1].trim(), "<true/>", "{p}");
    }

    #[test]
    fn systemd_unit_carries_the_daemon_entrypoint_and_restart_policy() {
        let exe = PathBuf::from("/home/x/.summon/bin/summon");
        let log = PathBuf::from("/home/x/.summon/run/daemon.log");
        let u = render_systemd_unit(&exe, false, &log);
        assert!(
            u.contains("ExecStart=@/home/x/.summon/bin/summon summond daemon run"),
            "{u}"
        );
        // 前台入口 → Type=simple;写成 forking 会让 systemd 等一个永不到来的父进程退出。
        assert!(u.contains("Type=simple"), "{u}");
        // 默认只在异常退出时拉起:`daemon stop` 的优雅退出码 0 不会被复活。
        assert!(u.contains("Restart=on-failure"), "{u}");
        // 没有这一段,`systemctl --user enable` 会报 "unit is not enabled" —— 装不上。
        assert!(u.contains("[Install]\nWantedBy=default.target"), "{u}");
        assert!(
            u.contains("StandardOutput=append:/home/x/.summon/run/daemon.log"),
            "{u}"
        );

        let keep = render_systemd_unit(&exe, true, &log);
        assert!(keep.contains("Restart=always"), "{keep}");
    }

    #[test]
    fn systemd_unit_never_kills_the_tmux_server_with_the_daemon() {
        // 默认 KillMode=control-group 会连同 cgroup 里的 tmux server 一起杀:agent 会话全灭。
        for restart_always in [false, true] {
            let u = render_systemd_unit(
                &PathBuf::from("/x/summon"),
                restart_always,
                &PathBuf::from("/x.log"),
            );
            assert!(u.contains("\nKillMode=process\n"), "{u}");
            // 写 0(与直接 spawn 同值),不沿用 user 实例给单元的 200。
            assert!(u.contains("\nOOMScoreAdjust=0\n"), "{u}");
            // 没 linger 时 user 实例登出即停,停时整组 SIGKILL,tmux 同样逃不掉:不起。
            assert!(
                u.contains("\nConditionPathExists=/var/lib/systemd/linger/%u\n"),
                "{u}"
            );
        }
    }

    #[test]
    fn systemd_unit_escapes_percent_which_systemd_reads_as_a_specifier() {
        // `%` 在单元文件里引导说明符(`%h`=家目录等)。路径里的字面 `%` 不转义就会被展开成
        // 别的东西,ExecStart 指向一个不存在的可执行文件 —— 且只在**含 `%` 的路径上**才犯。
        let exe = PathBuf::from("/home/x/100%dir/summon");
        let log = PathBuf::from("/tmp/x.log");
        let u = render_systemd_unit(&exe, false, &log);
        assert!(u.contains("/home/x/100%%dir/summon"), "{u}");
    }

    #[test]
    fn the_backend_follows_the_platform() {
        // 回归:`current()` 曾无条件返回 LaunchdUser,Linux 上 enable 因此直接 bail
        // (devbox 开不了机自启的根因)。
        let want = if cfg!(target_os = "macos") {
            LoginItemBackend::LaunchdUser
        } else {
            LoginItemBackend::SystemdUser
        };
        assert_eq!(LoginItemBackend::current(), want);
    }

    #[test]
    fn plist_file_name_is_label_dot_plist() {
        assert_eq!(plist_file_name(), "com.summon.daemon.plist");
    }

    /// 测试里的 HOME:跟着本平台的惯例走,免得断言读起来像另一个系统的。
    const HOME: &str = if cfg!(target_os = "macos") {
        "/Users/me"
    } else {
        "/home/me"
    };

    #[test]
    fn tool_path_candidates_follow_the_platform_not_a_fixed_list() {
        // 回归:候选表曾经只有 macOS 的 `/opt/homebrew/bin`,Linux 上 daemon 因此找不到 tmux
        // (2026-08-07 devbox 全部 agent 挂掉的根因)。两个平台各自的默认前缀都必须在。
        let dirs = tool_path_candidates(HOME);
        assert!(
            dirs.contains(&format!("{HOME}/.local/bin")),
            "两平台通用: {dirs:?}"
        );
        assert!(dirs.contains(&"/usr/local/bin".to_string()), "{dirs:?}");
        if cfg!(target_os = "macos") {
            assert!(dirs.contains(&"/opt/homebrew/bin".to_string()), "{dirs:?}");
            assert!(
                !dirs.iter().any(|d| d.contains("linuxbrew")),
                "macOS 不该带 linuxbrew: {dirs:?}"
            );
        } else {
            assert!(
                dirs.contains(&"/home/linuxbrew/.linuxbrew/bin".to_string()),
                "缺共享装 Linuxbrew: {dirs:?}"
            );
            assert!(
                dirs.contains(&format!("{HOME}/.linuxbrew/bin")),
                "缺单用户装 Linuxbrew: {dirs:?}"
            );
            // 2026-08-07 devbox 实际布局:homebrew 直接装在 `~/homebrew`(不带点),
            // `.linuxbrew` 两条都不命中,登录 shell 兜底也补不出来(实测其 PATH 里同样没有)。
            // 少这一条 = 那台机器上所有 claude agent 起不了会话,且静默到定时任务炸掉才暴露。
            assert!(
                dirs.contains(&format!("{HOME}/homebrew/bin")),
                "缺 homebrew 直装家目录(devbox 现场就是这种): {dirs:?}"
            );
            assert!(
                !dirs.iter().any(|d| d == "/opt/homebrew/bin"),
                "Linux 不该带 macOS 前缀: {dirs:?}"
            );
        }
    }

    #[test]
    fn ensure_tool_path_prepends_tool_dirs_for_launchd_minimal() {
        // launchd 拉起的 daemon 只有最小 PATH;补上 Homebrew(tmux)/~/.local/bin(claude)。
        let p = ensure_tool_path("/usr/bin:/bin:/usr/sbin:/sbin", HOME);
        let dirs: Vec<&str> = p.split(':').collect();
        for want in tool_path_candidates(HOME) {
            assert!(dirs.contains(&want.as_str()), "缺 {want}: {p}");
        }
        assert!(dirs.contains(&"/usr/bin"), "原系统目录应保留: {p}");
        // 工具目录前置,优先命中。
        assert!(p.starts_with(&tool_path_candidates(HOME)[0]), "{p}");
    }

    #[test]
    fn ensure_tool_path_no_duplicates_keeps_existing_order() {
        let first = tool_path_candidates(HOME)[0].clone();
        let local = format!("{HOME}/.local/bin");
        let cur = format!("{first}:/usr/bin:{local}");
        let p = ensure_tool_path(&cur, HOME);
        assert_eq!(p.matches(&first).count(), 1, "不应重复: {p}");
        assert_eq!(p.matches(&local).count(), 1, "不应重复: {p}");
        // 已存在的工具目录不重排到最前(保持原序),只补缺失的 /usr/local/bin。
        assert!(
            p.split(':').any(|d| d == "/usr/local/bin"),
            "缺失工具目录应补上: {p}"
        );
        assert!(p.contains(&format!("{first}:/usr/bin")), "原序保持: {p}");
    }

    #[test]
    fn ensure_tool_path_empty_falls_back_to_base_dirs() {
        // PATH 全空(病态)→ 回退基础系统目录 + 工具目录;home 空时不拼出 `/.local/bin`。
        let p = ensure_tool_path("", "");
        let dirs: Vec<&str> = p.split(':').collect();
        assert!(dirs.contains(&"/usr/bin"), "{p}");
        assert!(dirs.contains(&tool_path_candidates("")[0].as_str()), "{p}");
        assert!(
            !dirs.iter().any(|d| d.starts_with("/.")),
            "home 空不该拼出以 `/.` 开头的路径: {p}"
        );
    }

    #[test]
    fn append_missing_dirs_appends_only_the_gap_and_keeps_order() {
        // 兜底来源只补缺口:已有条目不动、不重排、不重复,新目录一律落在末尾。
        let cur = "/usr/bin:/bin";
        let p = append_missing_dirs(cur, "/home/me/homebrew/bin:/usr/bin:/opt/x/bin");
        assert_eq!(p, "/usr/bin:/bin:/home/me/homebrew/bin:/opt/x/bin", "{p}");
    }

    #[test]
    fn append_missing_dirs_tolerates_empty_and_dirty_input() {
        assert_eq!(append_missing_dirs("/usr/bin", ""), "/usr/bin");
        assert_eq!(append_missing_dirs("", "/a:/b"), "/a:/b");
        // 空段(`::`、首尾冒号)是真实 PATH 里的常见脏数据,不该变成空目录条目。
        assert_eq!(append_missing_dirs("/a::", ":/b:"), "/a:/b");
    }

    #[test]
    fn plist_xml_escapes_path_metacharacters() {
        // review 修复:路径含 XML 元字符(&/</>)要转义,否则坏 plist、launchd 加载失败。
        let exe = PathBuf::from("/Users/a&b/<bin>/summon");
        let log = PathBuf::from("/tmp/x.log");
        let p = render_launchd_plist(LAUNCHD_LABEL, &exe, true, false, &log);
        assert!(p.contains("/Users/a&amp;b/&lt;bin&gt;/summon"));
        assert!(!p.contains("a&b"), "裸 & 不该出现在 plist XML");
    }
}
