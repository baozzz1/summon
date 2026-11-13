//! 进程启动期的环境自愈。
//!
//! summon 的进程常不是从人的终端起的:daemon 被 launchd/systemd/cron 拉起,被非交互 ssh 拉起,
//! CLI 被 launchd 起的 Menu Bar spawn。这些环境的 `PATH` 只有 `/usr/bin:/bin:/usr/sbin:/sbin`,
//! 或者一份少了用户 rc 文件那半截的登录 PATH —— 没有 Homebrew 的 tmux,也没有 `~/.local/bin` 的 claude。
//!
//! 后果是静默的、看起来像功能坏了:daemon 那边 `Command::new("tmux")` ENOENT,所有 claude 会话起不来;
//! CLI 这边更阴险 —— `tmux list-sessions` 失败被当成「一个 tmux 会话都没有」,于是 `agents status`
//! 把所有 claude/Transcript 会话判为 Gone 全部过滤,Menu Bar 上只剩无头 agent(它们靠读盘发现会话,
//! 不依赖 tmux)。**报的不是错,是一份少了一半的真相。**
//!
//! 自愈分两层,顺序有讲究:
//! 1. **静态候选**(`autostart::ensure_tool_path`,纯函数):补各平台 Homebrew / `~/.local/bin` 这些
//!    默认前缀。零开销,覆盖绝大多数机器。
//! 2. **登录 shell 兜底**(本模块):静态候选补完仍找不到 `tmux` 才走 —— 说明工具装在枚举不出来的
//!    地方(Homebrew 自定义前缀、asdf/mise/nix 的 shim 目录)。这种只有用户自己的 rc 文件知道,
//!    问它要一份 PATH 比继续往代码里堆常量正确。

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 登录 shell 兜底的硬超时。rc 文件里 nvm/conda/asdf 初始化慢是常态,但 daemon 不能卡在启动期 ——
/// 宁可拿不到兜底 PATH 按原样继续(该报的 ENOENT 照样报),也不要一个起不来又不说话的 daemon。
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(3);

/// 探针二进制。summon 的所有 claude 形态 agent 都跑在 tmux 里,`tmux` 找不到 = 整个 summon 不可用,
/// 所以它是判断「PATH 到底够不够用」最省事也最贴目的的单一探针。
const PROBE_BIN: &str = "tmux";

/// 登录 shell 回吐 PATH 时的哨兵前缀。rc 文件里 echo 欢迎语 / 版本提示 / 更新通知是常态,
/// 不能把整段 stdout 当 PATH 用。
const PATH_SENTINEL: &str = "__summon_path__=";

/// 把常见工具目录补进 `PATH`(幂等)。
///
/// **必须在任何线程/spawn 之前调用** —— `set_var` 只在单线程期安全。补完之后整棵子进程树都继承。
/// 从人的终端起时 PATH 本就完整,此调用是空操作。
pub fn heal_tool_path() {
    let home = std::env::var("HOME").unwrap_or_default();
    let current = std::env::var("PATH").unwrap_or_default();
    let healed = summon_core::autostart::ensure_tool_path(&current, &home);
    // 静态候选够用就到此为止(从人的终端起时几乎总是这一支),不付登录 shell 那份启动开销。
    if resolves_on_path(&healed, PROBE_BIN) {
        std::env::set_var("PATH", healed);
        return;
    }
    let merged = match login_shell_path() {
        Some(from_shell) => summon_core::autostart::append_missing_dirs(&healed, &from_shell),
        None => healed,
    };
    std::env::set_var("PATH", merged);
}

/// `summon daemon start` / `summon autostart enable` 调用:把发起者这一侧的 PATH 存盘,
/// `ask_login_shell` 时再并上登录 shell 的 PATH。
///
/// 自启单元(launchd / systemd)拉起的 daemon 只有单元给的最小环境,用户 rc 文件里登记的目录
/// (npm 全局包、cargo、go、nvm……)一个都没有;而 agent 会话的 PATH 全部继承自 daemon —— 缺了
/// 它们,agent 在会话里连 `cargo` 都找不到。发起者(终端、ssh、deploy)手里有一份完整的,每次都
/// 存,几乎零成本:哪怕此刻还没开自启,之后开了、机器重启,单元拉起的 daemon 也有得补。登录
/// shell 能补上发起者自己也缺的那些,但可能要几秒,只在开了自启时才问(包括暂时没开 linger 的
/// Linux:以后开了、重启了,单元拉起时用得上);也只在这里问,不在 daemon 启动期问 —— 后者有
/// 就绪时限。
///
/// npm 全局 bin 也一并存下:claude / codex / lark-cli 的推荐装法都落在那里,而它常只在交互 shell
/// 的 rc 里进 PATH(Linux 的 `~/.npm-global`),发起者自己的 PATH 里未必有。
pub fn save_user_path(ask_login_shell: bool) -> anyhow::Result<()> {
    // 这里要的是用户登记过的全部目录,第一份问得到的就收,不以「能找到 tmux」为准(那是自愈
    // 兜底的判据)。
    let from_login_shell = if ask_login_shell {
        login_shell_path_where(|_| true)
    } else {
        None
    };
    let npm_global_bin =
        crate::setup::npm::global_bin(&summon_core::tools::resolve(summon_core::tools::Tool::Npx))
            .filter(|dir| dir.is_dir());
    save_user_path_at(
        &crate::runtime::paths::user_path_file(),
        &std::env::var("PATH").unwrap_or_default(),
        from_login_shell,
        npm_global_bin.as_deref(),
    )
}

fn save_user_path_at(
    file: &Path,
    current: &str,
    from_login_shell: Option<String>,
    npm_global_bin: Option<&Path>,
) -> anyhow::Result<()> {
    let own = without_wrappers(current);
    let mut merged = match from_login_shell {
        Some(login) => summon_core::autostart::append_missing_dirs(&own, &without_wrappers(&login)),
        None => own,
    };
    if let Some(dir) = npm_global_bin {
        merged = summon_core::autostart::append_missing_dirs(&merged, &dir.to_string_lossy());
    }
    crate::runtime::fsio::atomic_write(file, merged.as_bytes())
}

/// daemon 启动期调用(单线程段):把存下的用户 PATH 里、当前 PATH 没有的目录补在末尾。
/// 补在末尾不改变已生效的解析优先级。没存过就什么都不做。
pub fn apply_saved_user_path() {
    let saved = std::fs::read_to_string(crate::runtime::paths::user_path_file()).ok();
    let current = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", merged_with_saved(&current, saved.as_deref()));
}

/// daemon 启动期调用(单线程段,在 [`apply_saved_user_path`] 之后):把 summon 自己的落点目录补在
/// PATH 末尾。agent 在会话里要跑 `summon react / remind / preview`,而 `summon` 入口所在目录、
/// `~/.local/bin` 都不一定在自启单元或非交互 ssh 给的 PATH 上;补上落点保证会话里总有一份
/// summon 可用。
pub fn append_summon_bin_dir() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let current = std::env::var("PATH").unwrap_or_default();
    std::env::set_var(
        "PATH",
        with_summon_bin_dir(&current, &summon_core::companion::install_dir(&home)),
    );
}

fn with_summon_bin_dir(current: &str, install_dir: &Path) -> String {
    if !install_dir.join(summon_core::companion::SUMMON).is_file() {
        return current.to_string();
    }
    summon_core::autostart::append_missing_dirs(current, &install_dir.to_string_lossy())
}

fn merged_with_saved(current: &str, saved: Option<&str>) -> String {
    match saved {
        Some(saved) => {
            summon_core::autostart::append_missing_dirs(current, &without_wrappers(saved.trim()))
        }
        None => current.to_string(),
    }
}

fn without_wrappers(path: &str) -> String {
    path_without_lark_cli_wrappers(OsStr::new(path))
        .to_string_lossy()
        .into_owned()
}

/// 从 PATH 剔除 summon 为 agent 身份隔离注入的 lark-cli wrapper 目录。summon 自己绝不能
/// 把这类 per-agent wrapper 当成 owner/默认身份的 lark-cli；净化必须在任何线程或 spawn 前做。
pub fn path_without_lark_cli_wrappers(path: &OsStr) -> OsString {
    let kept = std::env::split_paths(path).filter(|entry| {
        !entry
            .components()
            .any(|component| component.as_os_str() == OsStr::new("lark-cli-wrappers"))
    });
    std::env::join_paths(kept).unwrap_or_default()
}

/// 净化当前进程及其整棵子进程树继承的 PATH。和 [`heal_tool_path`] 一样只能在 main 的
/// 单线程启动段调用。
pub fn sanitize_tool_path() {
    let clean = path_without_lark_cli_wrappers(&std::env::var_os("PATH").unwrap_or_default());
    std::env::set_var("PATH", clean);
}

/// `bin` 能否在 `path` 里解析到一个可执行文件(自带 mini-which,不为这点事引第三方 crate)。
fn resolves_on_path(path: &str, bin: &str) -> bool {
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .any(|dir| is_executable_file(&Path::new(dir).join(bin)))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 问登录 shell 要一份**能解析出探针二进制**的 PATH;问不出来就 `None`(调用方按原样继续)。
///
/// 判据故意定成「能解析出 `tmux`」而不是「拿到了非空字符串」:走到这里就说明当前 PATH 已经不够用,
/// 一份同样不够用的 PATH 合进来只是把噪声写进环境,不解决任何问题。
fn login_shell_path() -> Option<String> {
    login_shell_path_where(|path| resolves_on_path(path, PROBE_BIN))
}

/// 依次问候选登录 shell,取第一份 `accept` 认可的 PATH。
fn login_shell_path_where(accept: impl Fn(&str) -> bool) -> Option<String> {
    // 整轮共用一个 deadline:候选 shell 再多,也只可能被拖 `LOGIN_SHELL_TIMEOUT` 这么久。
    let deadline = Instant::now() + LOGIN_SHELL_TIMEOUT;
    login_shell_candidates()
        .into_iter()
        .filter_map(|shell| ask_shell_for_path(&shell, deadline))
        .find(|path| accept(path))
}

/// 候选登录 shell。
///
/// `$SHELL` 优先;但 launchd / systemd `--user` / cron 拉起时它常常压根没有。而 rc 文件是**按
/// `HOME` 走**的 —— 直接拿常见 shell 去读同一份 `~/.zshrc` / `~/.bashrc`,效果一样。
///
/// zsh 排在 bash 前:`brew shellenv` 的惯例落点是 `.zshrc`(macOS 默认 shell 是 zsh,Linux 上装
/// Homebrew 的人也多半在用 zsh)。两个前缀都试 —— macOS 的 zsh 在 `/bin/zsh`,Linux 发行版装的在
/// `/usr/bin/zsh`,少一个就等于在那一类机器上没有兜底(实测 devbox 正是 `/usr/bin/zsh`)。
fn login_shell_candidates() -> Vec<String> {
    shell_candidates(std::env::var("SHELL").ok().as_deref())
}

fn shell_candidates(shell: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(shell) = shell.filter(|s| !s.is_empty()) {
        out.push(shell.to_string());
    }
    for fallback in ["/bin/zsh", "/usr/bin/zsh", "/bin/bash", "/usr/bin/bash"] {
        if !out.iter().any(|s| s == fallback) {
            out.push(fallback.to_string());
        }
    }
    out
}

/// 起一次 `<shell> -lic` 把它的 `$PATH` 打回来。
///
/// 为什么必须 **login + interactive**(`-lic`):zsh 只有交互 shell 才读 `.zshrc`,而
/// `eval "$(brew shellenv)"` 的惯例位置正是 `.zshrc`。非交互地起 shell 拿到的 PATH 和出问题时
/// 一模一样,白问一趟。
///
/// 为什么不起线程做超时:本函数在 `set_var` 之前跑,必须保持单线程(见 `heal_tool_path`)。所以
/// stdout 落临时文件 + 主线程轮询 `try_wait` —— 顺带避开「管道写满、父进程又还没读」的自死锁。
fn ask_shell_for_path(shell: &str, deadline: Instant) -> Option<String> {
    if Instant::now() >= deadline {
        return None;
    }
    // 临时文件而不是管道:超时要杀子进程时,管道那头可能还堵着没读完的输出;文件没有这个问题。
    // `NamedTempFile` 析构即删,不用手写清理路径。
    let sink = tempfile::NamedTempFile::new().ok()?;
    let script = format!("printf '\\n{PATH_SENTINEL}%s\\n' \"$PATH\"");
    let mut child = Command::new(shell)
        .args(["-lic", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::from(sink.reopen().ok()?))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            // 超时或 wait 本身出错:杀掉别留孤儿,当作没问到。
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    parse_sentinel_path(&std::fs::read_to_string(sink.path()).ok()?)
}

/// 从登录 shell 的 stdout 里摘出哨兵那一行的 PATH。
///
/// 取**最后**一条命中:rc 文件里再 echo 什么都盖不掉我们自己最后打的那行。
fn parse_sentinel_path(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix(PATH_SENTINEL))
        .map(str::trim)
        .rfind(|path| !path.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_saved_user_path_joins_the_callers_path_and_the_login_shells() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("user-path");
        save_user_path_at(
            &file,
            "/home/me/.summon/runtime/lark-cli-wrappers/arcadia:/usr/bin:/home/me/.cargo/bin",
            Some("/usr/bin:/home/me/.npm-global/bin".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "/usr/bin:/home/me/.cargo/bin:/home/me/.npm-global/bin",
            "发起者的 lark-cli wrapper 不能被当成用户 PATH 存下"
        );
        save_user_path_at(&file, "/usr/bin", None, None).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "/usr/bin");
    }

    /// npm 全局 bin 不在发起者 PATH 上(只在交互 shell 的 rc 里)时也要存下,daemon 才找得到装在
    /// 那里的 claude / codex / lark-cli;已有的不重复。
    #[test]
    fn the_saved_user_path_includes_the_npm_global_bin() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("user-path");
        let npm_bin = Path::new("/home/me/.npm-global/bin");
        save_user_path_at(&file, "/usr/bin", None, Some(npm_bin)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "/usr/bin:/home/me/.npm-global/bin"
        );
        save_user_path_at(
            &file,
            "/home/me/.npm-global/bin:/usr/bin",
            None,
            Some(npm_bin),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "/home/me/.npm-global/bin:/usr/bin"
        );
    }

    /// daemon 启动期先并上存下的用户 PATH(含 npm 全局 bin),再补 summon 落点:两者都在任何
    /// spawn 之前,agent 会话才继承得到。
    #[test]
    fn daemon_boot_appends_the_saved_path_and_the_summon_landing_dir() {
        let boot = include_str!("../runtime/daemon/boot.rs");
        let saved = boot
            .find("crate::setup::env::apply_saved_user_path();")
            .expect("daemon 启动要并上存下的用户 PATH");
        let landing = boot
            .find("crate::setup::env::append_summon_bin_dir();")
            .expect("daemon 启动要补 summon 落点目录");
        assert!(saved < landing);
    }

    /// agent 会话继承 daemon 的 PATH:落点里有 summon 时补在末尾,让会话里的 `summon` 命令可用;
    /// 落点还没发布过(源码直接 cargo run)就不加一个空目录。
    #[test]
    fn the_summon_landing_dir_joins_the_daemon_path_only_when_published() {
        let dir = tempfile::tempdir().unwrap();
        let landing = dir.path().join(".summon/bin");
        assert_eq!(with_summon_bin_dir("/usr/bin", &landing), "/usr/bin");
        std::fs::create_dir_all(&landing).unwrap();
        std::fs::write(landing.join(summon_core::companion::SUMMON), "").unwrap();
        assert_eq!(
            with_summon_bin_dir("/usr/bin", &landing),
            format!("/usr/bin:{}", landing.display())
        );
        let already = format!("{}:/usr/bin", landing.display());
        assert_eq!(with_summon_bin_dir(&already, &landing), already);

        // daemon 启动时调的入口要真的把落点目录并进进程 PATH;只测纯函数,接线被摘掉也不会红。
        let src = include_str!("env.rs");
        let body = &src[src.find("pub fn append_summon_bin_dir() {").unwrap()..];
        let body: String = body[..body.find("\n}\n").unwrap()]
            .split_whitespace()
            .collect();
        assert!(
            body.contains("std::env::set_var(\"PATH\",with_summon_bin_dir(&current,&summon_core::companion::install_dir(&home)),);"),
            "{body}"
        );
    }

    #[test]
    fn the_saved_user_path_only_fills_gaps_at_the_end() {
        // 自启单元拉起时的 PATH:候选工具目录在前,系统目录在后。
        let current = "/opt/homebrew/bin:/usr/bin:/bin";
        let saved =
            "/Users/me/.nvm/versions/node/v22/bin:/usr/bin:/opt/homebrew/bin:/Users/me/.bun/bin";
        assert_eq!(
            merged_with_saved(current, Some(saved)),
            "/opt/homebrew/bin:/usr/bin:/bin:/Users/me/.nvm/versions/node/v22/bin:/Users/me/.bun/bin"
        );
        assert_eq!(merged_with_saved(current, None), current);
    }

    /// PATH 自愈必须发生在 `main` 的第一步:任何 `cli::parse()` 之后的分支都可能起线程/子进程,
    /// 那时再改 `PATH` 既不安全也太晚。删掉这一行 = Menu Bar 重新看不见 tmux 会话,而且不报错。
    #[test]
    fn main_heals_path_before_anything_else() {
        let main_rs = include_str!("../main.rs");
        let body = main_rs
            .split_once("fn main()")
            .expect("main.rs 必须有 fn main")
            .1;
        let heal = body
            .find("heal_tool_path()")
            .expect("main 必须做 PATH 自愈");
        let parse = body.find("cli::parse()").expect("main 必须解析 CLI");
        assert!(
            heal < parse,
            "PATH 自愈必须在 cli::parse 之前(此后可能已起线程,set_var 不再安全)"
        );
        let sanitize = body
            .find("sanitize_tool_path()")
            .expect("main 必须清掉 lark-cli wrapper PATH");
        assert!(
            heal < sanitize && sanitize < parse,
            "PATH 必须先补齐、再净化，并且都发生在 cli::parse 之前"
        );
    }

    #[test]
    fn sanitizer_drops_all_wrapper_entries_and_keeps_order() {
        let dirty = OsStr::new(
            "/home/me/.summon/runtime/lark-cli-wrappers/a:/opt/bin:\
             /home/me/.summon/runtime/lark-cli-wrappers/b:/usr/bin",
        );
        assert_eq!(
            path_without_lark_cli_wrappers(dirty),
            OsString::from("/opt/bin:/usr/bin")
        );
        assert_eq!(
            path_without_lark_cli_wrappers(OsStr::new("/usr/bin:/bin")),
            OsString::from("/usr/bin:/bin")
        );
    }

    /// 最小 PATH(launchd 环境)下自愈后必须能找到 tmux 所在的 Homebrew 目录 —— 这正是
    /// 「Menu Bar 看不到 claude 会话」那个 bug 的根因。候选表按平台取,别写死某一边。
    #[test]
    fn minimal_launchd_path_gains_tool_dirs() {
        let home = if cfg!(target_os = "macos") {
            "/Users/me"
        } else {
            "/home/me"
        };
        let healed =
            summon_core::autostart::ensure_tool_path("/usr/bin:/bin:/usr/sbin:/sbin", home);
        for want in summon_core::autostart::tool_path_candidates(home) {
            assert!(healed.contains(&want), "缺工具目录 {want}: {healed}");
        }
        assert!(healed.contains("/usr/bin"), "原有条目不得丢");
    }

    #[test]
    fn resolves_on_path_only_counts_executable_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let bin = dir.path().join("faketool");
        std::fs::write(&bin, "#!/bin/sh\n").expect("write");
        let path = dir.path().to_string_lossy().to_string();
        // 还没给执行位:不算数(否则会把一个跑不起来的文件当成工具已就位)。
        assert!(!resolves_on_path(&path, "faketool"), "无执行位不该算命中");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        assert!(resolves_on_path(&path, "faketool"));
        assert!(!resolves_on_path(&path, "no-such-tool"));
        // 目录本身同名也不算(is_file 把它挡掉)。
        std::fs::create_dir(dir.path().join("dirtool")).expect("mkdir");
        assert!(!resolves_on_path(&path, "dirtool"));
    }

    #[test]
    fn parse_sentinel_path_ignores_rc_noise_and_takes_the_last_hit() {
        let stdout = format!(
            "Welcome to your shell!\nUpdate available! Run: brew upgrade\n\
             {PATH_SENTINEL}/stale/bin\nsome trailing chatter\n{PATH_SENTINEL}/opt/x/bin:/usr/bin\n"
        );
        assert_eq!(
            parse_sentinel_path(&stdout).as_deref(),
            Some("/opt/x/bin:/usr/bin")
        );
    }

    #[test]
    fn parse_sentinel_path_rejects_missing_or_empty() {
        // 没有哨兵 = 这趟 shell 根本没跑到我们那行(rc 报错/被 exit 掉),不能拿噪声当 PATH。
        assert_eq!(parse_sentinel_path("no marker here\n"), None);
        assert_eq!(parse_sentinel_path(&format!("{PATH_SENTINEL}\n")), None);
        assert_eq!(parse_sentinel_path(&format!("{PATH_SENTINEL}   \n")), None);
        assert_eq!(parse_sentinel_path(""), None);
    }

    #[test]
    fn login_shell_candidates_keep_shell_first_and_dedupe() {
        // `$SHELL` 没有时也必须留下候选,否则 launchd/systemd 这条最需要兜底的路径直接放弃。
        // 两个前缀都要有:漏掉 `/usr/bin/zsh` 就等于在 Linux 发行版机器上没有兜底(实测踩过)。
        let fallbacks = ["/bin/zsh", "/usr/bin/zsh", "/bin/bash", "/usr/bin/bash"];
        assert_eq!(shell_candidates(None), fallbacks);
        assert_eq!(shell_candidates(Some("")), fallbacks);
        assert_eq!(
            shell_candidates(Some("/bin/bash")),
            ["/bin/bash", "/bin/zsh", "/usr/bin/zsh", "/usr/bin/bash"],
            "$SHELL 排第一,其余兜底保持原序、不重复"
        );
        assert_eq!(
            shell_candidates(Some("/opt/homebrew/bin/fish"))[0],
            "/opt/homebrew/bin/fish"
        );
    }

    /// 真机:跑一次本机登录 shell,确认兜底真能捞回一份「能解析出 tmux」的 PATH —— 这正是
    /// 2026-08-07 devbox 上 daemon 起不来会话的那一步。依赖本机 rc 与已装的 tmux,故标 ignore。
    #[test]
    #[ignore = "起真实登录 shell,依赖本机 rc 与已装 tmux"]
    fn login_shell_path_recovers_probe_bin_on_this_machine() {
        let path = login_shell_path().expect("登录 shell 应能回吐一份含 tmux 的 PATH");
        assert!(
            resolves_on_path(&path, PROBE_BIN),
            "兜底 PATH 必须能解析出 {PROBE_BIN}: {path}"
        );
    }
}
