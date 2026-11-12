//! 测试夹具(T6):统一构造测试用 `Agent`,消除散落各处的手搓 `serde_yaml::from_str("name: …")`
//! 样板——schema 演进(字段改名/加必填项)时只改这一处,而非全量散改几十处。新测试一律走这里;
//! 存量随触碰渐进迁移即可,不必一次清完。
//!
//! 整模块 `#[cfg(test)]`,仅测试可见(`crate::test_support::*`)。

use summon_core::registry::Agent;

fn agent_yaml(cli: &str, extra_yaml: &str) -> String {
    format!("name: test\nruntime:\n  cli: {cli}\n{extra_yaml}")
}

/// 用指定 `cli` 与追加的顶层 YAML 片段构造测试 `Agent`(名字固定 `test`)。
///
/// `extra_yaml` 作为**顶层键**拼在 `runtime.cli` 之后(如 `"display: Hi\nmodel: gpt-5.5-high\n"`、
/// `"feishu:\n  app_id_env: X_APP\n"`)。需要 `runtime` 内更多字段(working_dir 等)或多 agent 不同
/// 名字的测试,直接手搓 YAML 即可——本 helper 收的是最常见的「只关心 cli + 几个顶层字段」场景。
pub(crate) fn agent_with(cli: &str, extra_yaml: &str) -> Agent {
    let yaml = agent_yaml(cli, extra_yaml);
    serde_yaml::from_str(&yaml).expect("test_support::agent_with 产出的 YAML 应可解析")
}

/// 在临时目录里落一个 agent 自我认知目录,返回 `(Agent, TempDir)`:`Agent.dir` 指向该临时目录,
/// 供需要落盘读路径(`runtime/chats`、`threads/` 等)的集成测用。`TempDir` 随返回值存活,drop 即清。
pub(crate) fn agent_dir(cli: &str, extra_yaml: &str) -> (Agent, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("建临时 agent 目录");
    let mut agent = agent_with(cli, extra_yaml);
    // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
    // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串 resume/meta 记录。
    let agent_dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent_dir).expect("测试 agent 目录应可建");
    agent.dir = agent_dir;
    // **必须先把 state_dir 建出来**:`agent_state_dir()` 是个 fallback resolver ——
    // 新位置存在就返回新的,否则旧位置有任一 STATE_SUBDIRS 就返回旧的。也就是说**它的返回值
    // 随文件系统状态漂移**。两个目录都不存在时它返回新路径,可一旦中途有代码在旧位置建了
    // `threads/`,之后同一个 `state_dir()` 就改口返回旧路径 —— 同一个测试里写在一处、读在另一处。
    // 生产环境没这个问题(迁移后新位置恒存在);测试里必须显式钉住,否则是隐蔽的偶发。
    std::fs::create_dir_all(agent.state_dir()).expect("测试 state 目录应可建");
    std::fs::write(agent.manifest_path(), agent_yaml(cli, extra_yaml))
        .expect("测试 agent manifest 应可写");
    (agent, tmp)
}

/// 起隔离 tmux 服务的办法,拒绝时原样告诉跑测试的人。
const ISOLATED_TMUX_HOWTO: &str = "先起一台隔离的 tmux 服务再跑:\n  \
export TMUX_TMPDIR=$(mktemp -d /tmp/smx.XXXX); unset TMUX; tmux new-session -d -s placeholder sh\n\
(路径要短,否则 tmux 的 socket 名超过 unix socket 上限;跑完 `tmux kill-server` 并删掉这个目录)";

/// 真 tmux 测试的机器闸:证不了这是一台隔离服务就 panic,不跑。
///
/// 这些测试会挂客户端、发按键、改窗口选项。默认服务上跑着 daemon 的 `soul-*` 会话:挂一个只读
/// 客户端就会让同一服务上**别的进程**不带 `-c` 的 `send-keys` 全被拒(tmux 3.7 实测),而改窗口
/// 选项、送按键更是直接打到真在跑的 agent 会话上。判据两条缺一不可 —— `TMUX_TMPDIR` 指了另一台
/// 服务,且那台服务上没有 `soul-` 开头的会话。
pub(crate) fn require_isolated_tmux_server() {
    if let Some(reason) = isolated_server_refusal(
        std::env::var("TMUX_TMPDIR").ok().as_deref(),
        tmux_session_names().as_deref(),
    ) {
        panic!("{reason}");
    }
}

/// [`require_isolated_tmux_server`] 的判定本体。拒绝时给出原因,能跑时返回 None。
fn isolated_server_refusal(
    tmux_tmpdir: Option<&str>,
    sessions: Option<&[String]>,
) -> Option<String> {
    if !tmux_tmpdir.is_some_and(|dir| !dir.trim().is_empty()) {
        return Some(format!(
            "TMUX_TMPDIR 没设:这条测试会挂客户端 / 发按键 / 改窗口选项,跑在默认 tmux 服务上会打到\
             真在跑的 agent 会话。{ISOLATED_TMUX_HOWTO}"
        ));
    }
    let Some(sessions) = sessions else {
        return Some(format!(
            "问不出这台 tmux 服务上有哪些会话,就证不了它不是 daemon 那台,所以不跑。\
             {ISOLATED_TMUX_HOWTO}"
        ));
    };
    let souls: Vec<&str> = sessions
        .iter()
        .filter(|name| name.starts_with("soul-"))
        .map(String::as_str)
        .collect();
    if !souls.is_empty() {
        return Some(format!(
            "这台 tmux 服务上有 daemon 的会话({}):不在它上面跑真 tmux 测试。{ISOLATED_TMUX_HOWTO}",
            souls.join(" ")
        ));
    }
    None
}

/// 当前 `TMUX_TMPDIR` 指的那台服务上的会话名。服务没起(没有任何会话)时是空表;tmux 跑不起来
/// 时是 None——问不出会话名就证不了这台不是 daemon 那台,判据据此拒绝。
fn tmux_session_names() -> Option<Vec<String>> {
    let out = std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .ok()?;
    // 服务没起时 tmux 以非零码退出并在 stderr 说 "no server running",那是「没有会话」不是问不出。
    if !out.status.success() && !String::from_utf8_lossy(&out.stderr).contains("no server running")
    {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// 测试自建的 tmux 会话。析构时清掉输入行、让里面的 shell 自己 exit——断言失败 panic 也照样
/// 收尾,不留残局。清理不用销毁命令:守护测试钉死了那条命令的调用点。
pub(crate) struct ThrowawaySession(pub(crate) crate::session::tmux::Tmux);

impl ThrowawaySession {
    /// shell 带 UTF-8 区域起:tmux 服务的全局环境里常常没有 LANG,那样 shell 的行编辑器会把
    /// 多字节输入当成 meta 键改写掉,测的就不是 tmux 送没送到了。
    ///
    /// 闸在**建会话之前**过:用到这个夹具的测试无一例外要挂客户端或发按键(会话自己的收尾就是
    /// 发 `exit`),闸放在这里,新写的测试不会漏掉它。
    pub(crate) fn start(prefix: &str) -> Self {
        require_isolated_tmux_server();
        let tmux = crate::session::tmux::Tmux::new(format!("{prefix}-{}", std::process::id()));
        tmux.new_session("/tmp", "env LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 sh")
            .unwrap();
        Self(tmux)
    }
}

impl Drop for ThrowawaySession {
    fn drop(&mut self) {
        let _ = self.0.clear_input();
        let _ = self.0.send_text("exit");
        let _ = self.0.send_enter();
    }
}

/// 真 tmux 测试用:窗格里跑一个把收到的原始字节原样记进文件的程序(`cat`,终端设成逐字节、不回显、
/// 不改写回车),`mouse` 时先开鼠标跟踪与 SGR 汇报(`?1000h` `?1006h`),像 Claude Code 那样。
/// 析构时先退出窗格的模式,再发 C-c 让 `cat` 退出、shell 回到提示符,随后由会话自己的析构 exit。
pub(crate) struct Recorder {
    session: ThrowawaySession,
    file: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Recorder {
    pub(crate) fn start(prefix: &str, mouse: bool) -> Self {
        Self::start_showing(prefix, mouse, "")
    }

    /// 同 [`Self::start`],开始记录之前先在窗格里打出 `screen`(`printf` 的格式串,不能含单引号),
    /// 让屏上显示出指定的画面。
    pub(crate) fn start_showing(prefix: &str, mouse: bool, screen: &str) -> Self {
        assert!(!screen.contains('\''), "画面里不能有单引号");
        let session = ThrowawaySession::start(prefix);
        let dir = tempfile::tempdir().expect("建记录目录");
        let file = dir.path().join("stdin.bin");
        let enable = if mouse {
            r"printf '\033[?1000h\033[?1006h'; "
        } else {
            ""
        };
        let show = if screen.is_empty() {
            String::new()
        } else {
            format!("printf '{screen}'; ")
        };
        session
            .0
            .send_text(&format!(
                "{show}{enable}stty -icanon -echo -icrnl -ixon min 1 time 0; cat > '{}'; stty sane",
                file.display()
            ))
            .unwrap();
        session.0.send_enter().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while current_command(&session.0).as_deref() != Some("cat") {
            assert!(
                std::time::Instant::now() < deadline,
                "记录程序 5 秒内没跑起来"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Self {
            session,
            file,
            _dir: dir,
        }
    }

    pub(crate) fn tmux(&self) -> &crate::session::tmux::Tmux {
        &self.session.0
    }

    /// 程序到此刻收到的全部字节。
    pub(crate) fn received(&self) -> Vec<u8> {
        std::fs::read(&self.file).unwrap_or_default()
    }

    /// 等到收齐 `expected` 那么多字节(至多 `within`),再多等一会儿把多出来的也收进来,返回全部。
    /// 判「什么都没收到」时传空切片:整段 `within` 都等满。
    pub(crate) fn wait_for(&self, expected: &[u8], within: std::time::Duration) -> Vec<u8> {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline
            && (expected.is_empty() || self.received().len() < expected.len())
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        self.received()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.session.0.exit_copy_mode();
        let _ = self.session.0.send_key("C-c");
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// 窗格此刻前台程序的名字。
fn current_command(t: &crate::session::tmux::Tmux) -> Option<String> {
    let out = std::process::Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            &format!("={}:", t.session),
            "-F",
            "#{pane_current_command}",
        ])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 测试挂上的本地 attach 客户端(不经网页终端,不在它的登记表里)。析构时结束这个客户端进程
/// (会话不动):断言失败 panic 也照样收尾。要声明在 `ThrowawaySession` 之后,先于会话析构——
/// 会话的 shell 退出前客户端已经不在了。
pub(crate) struct AttachedClient {
    child: std::process::Child,
    _master: std::os::fd::OwnedFd,
}

impl AttachedClient {
    pub(crate) fn spawn(t: &crate::session::tmux::Tmux, read_only: bool) -> Self {
        let pty = nix::pty::openpty(None, None).unwrap();
        let slave = pty.slave;
        let mut cmd = t.attach_command(read_only);
        cmd.env("TERM", "xterm-256color")
            .env_remove("TMUX")
            .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
            .stdout(std::process::Stdio::from(slave.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(slave));
        Self {
            child: cmd.spawn().unwrap(),
            _master: pty.master,
        }
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 同时持闸的测试进程上限。
///
/// 取 4:同时存活的夹具进程组远少于 PID/PGID 复用的量级,计时敏感的打断测试在整套测试负载下
/// 也保持稳定;再往上它们会开始抖。`.config/nextest.toml` 的 `process-tests` 组按同一个数调度,
/// 由 `nextest_group_schedules_as_many_process_tests_as_there_are_slots` 钉住。
const PROCESS_TEST_SLOTS: usize = 4;

/// 会起真子进程 + 对自有进程组发信号的测试的闸。
///
/// 跨模块、跨测试进程共用**同一组**槽位是刻意的:codex app-server 客户端测和会话池测都在
/// killpg 自己 spawn 的进程组,无限并发时 PID/PGID 快速复用会造出无意义的抖动。nextest 每条
/// 测试各起一个进程,所以只用静态 Mutex 等于没锁;固定 temp 文件上的 flock 才能覆盖它。每个槽位
/// 配一份自己的隔离运行时根,持同一槽位的测试才会看到彼此的登记簿 —— 而那正是被 flock 串开的。
pub(crate) struct ProcessTestGuard {
    _process: std::fs::File,
    _thread: std::sync::MutexGuard<'static, ()>,
    rpc_runtime_root: std::path::PathBuf,
}

impl ProcessTestGuard {
    /// 本次持闸用的隔离运行时根。
    pub(crate) fn rpc_runtime_root(&self) -> &std::path::Path {
        &self.rpc_runtime_root
    }
}

/// 槽位锁与隔离运行时根所在的目录。默认是系统临时目录;测试闸自己的测试经这个环境变量把
/// 子进程引到私有目录里,不和别的测试抢槽位。
const PROCESS_GUARD_DIR_ENV: &str = "SUMMON_PROCESS_GUARD_DIR";

fn process_guard_dir() -> std::path::PathBuf {
    std::env::var_os(PROCESS_GUARD_DIR_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// 在 `dir` 里拿一个空闲槽位的跨进程锁,全满就稍等再轮询。
fn acquire_process_slot(dir: &std::path::Path) -> (std::fs::File, usize) {
    loop {
        for slot in 0..PROCESS_TEST_SLOTS {
            let path = dir.join(format!("summon-rpc-process-tests-{slot}.lock"));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .unwrap_or_else(|error| panic!("开跨进程测试锁 {path:?} 失败:{error}"));
            match file.try_lock() {
                Ok(()) => return (file, slot),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    panic!("flock 跨进程测试锁 {path:?} 失败:{error}")
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

pub(crate) fn process_test_guard() -> ProcessTestGuard {
    process_test_guard_in(&process_guard_dir())
}

fn process_test_guard_in(dir: &std::path::Path) -> ProcessTestGuard {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    // 单个失败不该把同组剩余测试全变成 PoisonError,保留最初的真实失败证据。
    // 同一进程里的测试(`cargo test`)共用进程内的会话池等静态状态,逐条串行。
    let thread = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (process, slot) = acquire_process_slot(dir);
    let rpc_runtime_root = dir.join(format!("summon-rpc-process-tests-runtime-{slot}"));
    crate::capture::rpc_procs::set_test_runtime_root(rpc_runtime_root.clone());
    match std::fs::remove_dir_all(&rpc_runtime_root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!(
            "清理隔离 RPC 测试根 {} 失败:{error}",
            rpc_runtime_root.display()
        ),
    }
    std::fs::create_dir_all(rpc_runtime_root.join("run"))
        .unwrap_or_else(|error| panic!("创建隔离 RPC 测试 run 目录失败:{error}"));
    std::fs::create_dir_all(rpc_runtime_root.join("agents"))
        .unwrap_or_else(|error| panic!("创建隔离 RPC 测试 agents 目录失败:{error}"));
    ProcessTestGuard {
        _process: process,
        _thread: thread,
        rpc_runtime_root,
    }
}

impl Drop for ProcessTestGuard {
    fn drop(&mut self) {
        // 测试夹具清理不能掩盖被测断言；下一位持锁者还会在进入时严格清一次。
        let _ = std::fs::remove_dir_all(&self.rpc_runtime_root);
        // 放闸后本进程不再拥有这个槽位:之后没拿闸的测试回到默认根,碰不到别人正持有的槽位。
        crate::capture::rpc_procs::clear_test_runtime_root();
    }
}

#[cfg(test)]
mod process_guard_tests {
    use super::*;

    const CHILD_MARKER_ENV: &str = "SUMMON_PROCESS_GUARD_CHILD_MARKER";

    const CHILD_STARTED_ENV: &str = "SUMMON_PROCESS_GUARD_CHILD_STARTED";

    #[test]
    fn process_guard_child() {
        let Some(marker) = std::env::var_os(CHILD_MARKER_ENV) else {
            return;
        };
        if let Some(started) = std::env::var_os(CHILD_STARTED_ENV) {
            std::fs::write(started, b"started").unwrap();
        }
        let guard = process_test_guard();
        std::fs::write(
            marker,
            guard.rpc_runtime_root().to_string_lossy().as_bytes(),
        )
        .unwrap();
    }

    fn spawn_guard_child(
        dir: &std::path::Path,
        started: &std::path::Path,
        marker: &std::path::Path,
    ) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "test_support::process_guard_tests::process_guard_child",
            ])
            .env(CHILD_MARKER_ENV, marker)
            .env(CHILD_STARTED_ENV, started)
            .env(PROCESS_GUARD_DIR_ENV, dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// 等子测试进程退出;超时就收掉它并失败,不让一把坏掉的闸把整套测试挂死。
    fn wait_child(mut child: std::process::Child) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                panic!("子测试进程 30s 内没有退出");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn wait_for(path: &std::path::Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "等不到 {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// 两个测试进程同时持闸时各用各的隔离运行时根:同一份登记簿只会被同一槽位上先后持闸的
    /// 测试看到。
    #[test]
    fn concurrent_guard_holders_never_share_a_runtime_root() {
        let tmp = tempfile::tempdir().unwrap();
        let started = tmp.path().join("child-started");
        let marker = tmp.path().join("child-root");
        let guard = process_test_guard_in(tmp.path());
        let status = wait_child(spawn_guard_child(tmp.path(), &started, &marker));

        assert!(status.success(), "还有空槽时子测试进程应拿到闸并退出");
        let child_root = std::fs::read_to_string(&marker).unwrap();
        assert_ne!(
            std::path::Path::new(&child_root),
            guard.rpc_runtime_root(),
            "同时持闸的两个进程不能共用隔离运行时根"
        );
    }

    /// 槽位全被占着时,其它测试进程要等到有槽放出来;进程内 Mutex 挡不住 nextest 的其它测试
    /// 进程,这道上限只能靠跨进程锁。子进程先报到再去拿闸,「没拿到」才是被挡住而不是还没起来。
    #[test]
    fn guard_blocks_another_process_while_every_slot_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let started = tmp.path().join("child-started");
        let marker = tmp.path().join("child-root");
        let held: Vec<_> = (0..PROCESS_TEST_SLOTS)
            .map(|_| acquire_process_slot(tmp.path()).0)
            .collect();
        let child = spawn_guard_child(tmp.path(), &started, &marker);

        wait_for(&started);
        std::thread::sleep(std::time::Duration::from_millis(250));
        let child_was_blocked = !marker.exists();
        drop(held);
        let status = wait_child(child);

        assert!(status.success(), "有槽放出后子测试进程应拿到闸并退出");
        assert!(marker.exists());
        assert!(child_was_blocked, "槽位全满时其它测试进程不能拿到闸");
    }

    /// nextest 一次调度的子进程测试数与槽位数一致,否则多出来的会占着测试线程空等槽位。
    /// 持闸却不在这个组里的测试照样正确,只是排队时占着线程。
    #[test]
    fn nextest_group_schedules_as_many_process_tests_as_there_are_slots() {
        let config = include_str!("../../../.config/nextest.toml");
        assert!(
            config.contains(&format!(
                "process-tests = {{ max-threads = {PROCESS_TEST_SLOTS} }}"
            )),
            "{config}"
        );
    }

    #[test]
    fn process_guard_removes_the_isolated_rpc_runtime_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let root = {
            let guard = process_test_guard_in(tmp.path());
            let registry = crate::capture::rpc_procs::registry_path();
            assert!(registry.starts_with(guard.rpc_runtime_root()));
            std::fs::write(&registry, b"[]").unwrap();
            assert!(registry.exists());
            guard.rpc_runtime_root().to_path_buf()
        };
        assert!(!root.exists(), "释放测试闸时必须清掉整份隔离运行时");
    }
}

/// 在 `dir` 里落一个假 codex app-server(POSIX sh 脚本),返回可执行文件路径。
///
/// 形状:逐行读 stdin、原样追加进 `<dir>/requests.jsonl`(测试据此断言**发出去的**帧),再按
/// `cases`(shell `case` 分支片段)回帧。`initialize` 已内置应答,`cases` 只写各测自己关心的方法。
/// 用真进程而不是 mock,是因为要验的东西(进程组信号、stdout EOF、写端分帧)全在进程边界上。
///
/// `cases` 里可以用 `$id` 引用**本条请求的 id**(脚本已从行首 `{"id":N,` 抠出来)。多轮测试请一律
/// 用它:客户端的 id 是单调递增的,写死 `"id":3` 的分支在第二次同名请求上必然对不上、然后超时。
///
/// 脚本显式 `trap 'exit 130' INT`。**这是夹具的可靠性补丁,不是被测语义**:macOS `/bin/sh`(bash 3.2)
/// 在默认处置下、SIGINT 恰好落在前台 `sleep` 前后的窄窗口时会**幸存**(实测约 1/4 概率),把「打断
/// 后进程组收敛」的断言变成掷骰子。真 `codex app-server` 是普通 Rust 二进制、SIGINT 走默认处置,
/// 没有这个问题;装个 trap 只是把 shell 的这段不确定性去掉,信号仍是同一条 SIGINT 整组路径。
pub(crate) fn write_fake_app_server(dir: &std::path::Path, cases: &str) -> std::path::PathBuf {
    write_app_server_with_trap(dir, cases, "trap 'exit 130' INT")
}

/// 同 [`write_fake_app_server`],但**屏蔽 SIGINT**(`trap '' INT`):只有 SIGKILL 收得掉它。
///
/// 用途只有一个 —— 证明某条回收路径**真的补了第二枪**。SIGINT 就能收掉的夹具在这件事上零信息量:
/// 进程照样会没,分不清是第一枪还是第二枪打的。
pub(crate) fn write_sigint_immune_app_server(
    dir: &std::path::Path,
    cases: &str,
) -> std::path::PathBuf {
    write_app_server_with_trap(dir, cases, "trap '' INT")
}

fn write_app_server_with_trap(
    dir: &std::path::Path,
    cases: &str,
    trap: &str,
) -> std::path::PathBuf {
    let script = dir.join("fake-app-server.sh");
    // `account/rateLimits/read` 与 `model/list` 兜底放在 `{cases}` **之后**:开会话时会顺带问一次额度,夹具不答
    // 就得白等它的超时(2026-08-08 实测把一条 5s 断言拖到 6.2s)。放在后面是为了让用例仍能
    // 自己覆盖它 —— shell `case` 第一个匹配生效。兜底答 `-32601`,正好也是「codex 版本不支持
    // 这个方法」的真实降级形态。
    let respond = format!(
        "respond() {{\n  case \"$line\" in\n    *initialize*) printf '%s\\n' '{{\"id\":1,\"result\":{{\"userAgent\":\"fake\"}}}}' ;;\n    {cases}\n    *'account/rateLimits/read'*) printf '%s\\n' '{{\"id\":'\"$id\"',\"error\":{{\"code\":-32601,\"message\":\"method not found\"}}}}' ;;\n    *'model/list'*) printf '%s\\n' '{{\"id\":'\"$id\"',\"error\":{{\"code\":-32601,\"message\":\"method not found\"}}}}' ;;\n  esac\n}}\n"
    );
    std::fs::write(dir.join("fake-app-server.cases"), respond).expect("写假 app-server 应答表");
    let body = format!(
        "#!/bin/sh\n{trap}\nDIR=\"${{0%/*}}\"\nLOG=\"$DIR/requests.jsonl\"\n. \"$DIR/fake-app-server.cases\"\nprintf 'SUMMON_AGENT=%s\\nSUMMON_CHAT_ID=%s\\nSUMMON_CONVERSATION_TAG=%s\\nACP_LEAK_PROBE=%s\\nFEISHU_APP_ID=%s\\n' \"${{SUMMON_AGENT-}}\" \"${{SUMMON_CHAT_ID-}}\" \"${{SUMMON_CONVERSATION_TAG-}}\" \"${{ACP_LEAK_PROBE-}}\" \"${{FEISHU_APP_ID-}}\" > \"${{LOG}}.child-env\"\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> \"$LOG\"\n  id=$(printf '%s' \"$line\" | sed -n 's/^{{\"id\":\\([0-9]*\\).*/\\1/p')\n  respond\ndone\n"
    );
    write_test_executable(&script, &body);
    script
}

/// 在 `dir` 里落一个假 cursor ACP server(POSIX sh 脚本),返回可执行文件路径。
///
/// 与 [`write_fake_app_server`] 同一形态,两处刻意的差异都来自 ACP 的帧形状:
/// - **id 不在行首**:ACP 客户端发 `{"jsonrpc":"2.0","id":N,…}`,所以 `$id` 的抠取改成
///   「行内第一个 `"id":N`」。沿用 app-server 那条 `^{"id":` 的正则会永远抠不到,
///   所有带 `$id` 的分支都对不上然后超时。
/// - **initialize 回 ACP 形状**(`protocolVersion` + `agentCapabilities`)。
///
/// 进程启动时还会把 `CURSOR_CONFIG_DIR` 追加到 `requests.jsonl.config-dirs`，供会话池
/// 验证“意图 per-agent 共享，可写物理根 per-process 隔离”，不记录其它环境变量。
pub(crate) fn write_fake_acp_server(dir: &std::path::Path, cases: &str) -> std::path::PathBuf {
    let script = dir.join("fake-acp-server.sh");
    let respond = format!(
        "respond() {{\n  case \"$line\" in\n    *initialize*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":'\"$id\"',\"result\":{{\"protocolVersion\":1,\"agentCapabilities\":{{\"loadSession\":true}}}}}}' ;;\n    {cases}\n  esac\n}}\n"
    );
    std::fs::write(dir.join("fake-acp-server.cases"), respond).expect("写假 ACP server 应答表");
    let body = "#!/bin/sh\ntrap 'exit 130' INT\nDIR=\"${0%/*}\"\nLOG=\"$DIR/requests.jsonl\"\n. \"$DIR/fake-acp-server.cases\"\nprintf '%s\\n' \"${CURSOR_CONFIG_DIR-}\" >> \"${LOG}.config-dirs\"\nprintf 'SUMMON_AGENT=%s\\nSUMMON_CHAT_ID=%s\\nSUMMON_CONVERSATION_TAG=%s\\nACP_LEAK_PROBE=%s\\n' \"${SUMMON_AGENT-}\" \"${SUMMON_CHAT_ID-}\" \"${SUMMON_CONVERSATION_TAG-}\" \"${ACP_LEAK_PROBE-}\" > \"${LOG}.child-env\"\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> \"$LOG\"\n  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n  respond\ndone\n";
    write_test_executable(&script, body);
    script
}

/// 假 RPC server 落请求日志的路径(app-server / ACP 两份夹具同目录同名)。
pub(crate) fn fake_rpc_server_log(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("requests.jsonl")
}

/// 在 `path` 放一个内容为 `content` 的可执行文件。macOS 上它是按内容共享的一份只读文件的硬链接;
/// 别的平台直接写在 `path`。**测试里要执行的脚本一律经它落盘。**
///
/// macOS 头一次执行一个新写出来的可执行文件时要做一次安全评估,约 0.4s,而且全机串行 —— 8 个
/// 测试同时起新脚本,最后一个排了 3.2s(实测);负载一高,带超时的测试就被拖成偶发失败。评估跟着
/// 文件走:同一份文件经别的硬链接再执行不再评估。所以内容相同的脚本只落一份,跨测试、跨轮复用。
/// 内容里因此不能带每个测试各不相同的路径;要用就从 `$0` 推 —— `$0` 是 `path`,在调用方的目录里。
/// `path` 是普通文件:被测代码会 canonicalize 它,结果得落在调用方自己的目录里。
/// 别的平台没有这笔开销,共享目录又可能是多用户共用的 `/tmp`,所以不共享。
///
/// 共享那份只读,每次取用前核对内容与权限:改写它会当场报错;改了它的权限位,下一个取用的会换
/// 一份新的。要换 `path` 的内容或权限,再调一次本函数,别直接动它。
pub(crate) fn write_test_executable(path: &std::path::Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path); // 同一目录里换一份夹具时先摘掉旧的
    if let Some(shared) = shared_test_executable(content) {
        if std::fs::hard_link(&shared, path).is_ok() {
            return;
        }
    }
    std::fs::write(path, content).expect("写可执行夹具");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("给可执行夹具加执行位");
}

/// 内容为 `content` 的共享那份。落不成、或核对不过又换不掉(哈希撞车)就是 `None`,调用方退回
/// 自己写一份。
fn shared_test_executable(content: &str) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::fs::PermissionsExt;
    if !cfg!(target_os = "macos") {
        return None;
    }
    static STAGING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    // macOS 的临时目录是每个用户自己的。
    let cache = std::env::temp_dir().join("summon-test-executables");
    std::fs::create_dir_all(&cache).ok()?;
    let shared = cache.join(format!("{:016x}-{}", hasher.finish(), content.len()));
    // `None` = 还没有;`Some(false)` = 有但内容或权限对不上。
    let intact = |shared: &std::path::Path| {
        let meta = std::fs::symlink_metadata(shared).ok()?;
        Some(
            meta.permissions().mode() & 0o7777 == 0o555
                && std::fs::read_to_string(shared).ok().as_deref() == Some(content),
        )
    };
    match intact(&shared) {
        Some(true) => return Some(shared),
        // 只摘核对不过的那份:已经链着它的测试手里那份不受影响。
        Some(false) => {
            let _ = std::fs::remove_file(&shared);
        }
        None => {}
    }
    // 各进程写自己的暂存名再硬链接到位:并发时先链上的那份生效。不用 rename 覆盖,那会把别的
    // 测试刚评估过的文件换成一份新的。暂存名带纳秒:被杀的进程留下的只读暂存,不会被复用了
    // 同一个 pid 的进程撞上。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let staging = cache.join(format!(
        "staging-{}-{}-{nanos}",
        std::process::id(),
        STAGING.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = std::fs::write(&staging, content)
        .and_then(|()| std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o555)))
        .and_then(|()| match std::fs::hard_link(&staging, &shared) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            linked => linked,
        });
    let _ = std::fs::remove_file(&staging); // 链没链上都只留共享那份
    (written.is_ok() && intact(&shared) == Some(true)).then_some(shared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::registry::CaptureMode;

    #[test]
    fn agent_with_sets_cli_and_top_level_fields() {
        let a = agent_with("cursor", "display: Hi\nmodel: gpt-5.5-high\n");
        assert_eq!(a.name, "test");
        assert_eq!(a.cli_kind(), "cursor");
        assert_eq!(a.display, "Hi");
        assert_eq!(a.model.as_deref(), Some("gpt-5.5-high"));
    }

    #[test]
    fn agent_with_empty_extra_defaults_to_given_cli() {
        let a = agent_with("claude", "");
        assert_eq!(a.cli_kind(), "claude");
        // claude 非 headless → Transcript(确认拼出的 YAML 解析为可用 Agent)。
        assert_eq!(a.capture_mode(), CaptureMode::Transcript);
    }

    /// 真 tmux 测试的闸:两条判据各自能拒,都满足才放行。
    #[test]
    fn a_real_tmux_test_runs_only_on_an_isolated_server_without_daemon_sessions() {
        let none: Vec<String> = Vec::new();
        let refused = isolated_server_refusal(None, Some(&none)).expect("没设 TMUX_TMPDIR 要拒");
        assert!(refused.contains("TMUX_TMPDIR"), "{refused}");
        assert!(
            refused.contains("mktemp -d /tmp/smx"),
            "要说怎么起:{refused}"
        );
        assert!(
            isolated_server_refusal(Some("  "), Some(&none)).is_some(),
            "空值不算设过"
        );

        let souls = vec!["placeholder".to_string(), "soul-arcadia-gabc".to_string()];
        let refused = isolated_server_refusal(Some("/tmp/smx.aaaa"), Some(&souls))
            .expect("有 soul- 会话要拒");
        assert!(
            refused.contains("soul-arcadia-gabc"),
            "要指名道姓:{refused}"
        );

        let unknown =
            isolated_server_refusal(Some("/tmp/smx.aaaa"), None).expect("问不出会话名要拒");
        assert!(unknown.contains("证不了"), "{unknown}");

        assert_eq!(
            isolated_server_refusal(Some("/tmp/smx.aaaa"), Some(&["placeholder".to_string()])),
            None,
            "隔离服务上没有 daemon 的会话就放行"
        );
    }

    #[test]
    fn agent_dir_points_agent_dir_at_tempdir() {
        let (a, tmp) = agent_dir("claude", "");
        assert!(a.dir.starts_with(tmp.path()));
        assert!(a.dir.is_dir(), "agent.dir 应是真实存在的临时目录");
        assert_eq!(a.reread().unwrap().name, a.name);
    }

    /// 测试里要执行的脚本一律经 [`write_test_executable`] 落盘:直接写文件再加执行位,macOS 上
    /// 每份新文件头一次执行都要排一次全机串行的安全评估,负载一高带超时的测试就偶发失败。
    ///
    /// 扫两个 crate 的整个 `src` 里设属主执行位的字面写法,不区分测试与生产代码。生产代码与
    /// 确实不执行文件的测试都在下面登记;新增一处就登记并写明为什么不经它。扫不到的:变量形式
    /// 的权限、复制已有的可执行文件。
    #[test]
    fn tests_do_not_write_executables_by_hand() {
        const ALLOWED: &[(&str, usize)] = &[
            // ── 生产代码 ──
            ("summon/src/bridge/install.rs", 1), // 收紧 settings 目录
            ("summon/src/bridge/store.rs", 1),   // 收紧状态目录
            ("summon/src/capture/cursor_config.rs", 2), // 收紧配置目录
            ("summon/src/runtime/document_comments.rs", 1), // 收紧评论与审批存根目录
            ("summon/src/remote_tools/process.rs", 1), // 临时状态目录
            ("summon/src/remote_tools/registry.rs", 1), // 收紧注册目录
            ("summon/src/remote_tools/shell.rs", 1), // 远端 shell 前缀脚本
            ("summon/src/runtime/card_action_wal.rs", 1), // 收紧 WAL 目录
            ("summon/src/runtime/daemon/agent_env.rs", 1), // lark-cli wrapper
            ("summon/src/runtime/daemon/boot.rs", 1), // 收紧运行目录
            ("summon/src/runtime/daemon/export.rs", 1), // 收紧导出包目录
            ("summon/src/runtime/dashboard/agent_archive.rs", 1), // 收紧清理隔离目录，内含凭据
            ("summon/src/setup/init.rs", 1),     // 收紧 ~/.summon
            ("summon/src/setup/self_install.rs", 1), // 发布到落点的暂存文件
            // ── 测试:不执行文件,或改的是目录 ──
            ("summon-core/src/tools.rs", 1),           // 只做路径解析
            ("summon/src/bridge/handoff.rs", 1),       // PATH 扫描只读文件头
            ("summon/src/runtime/outbox_store.rs", 3), // 生产 1 处收紧目录;测试 2 处改目录权限造写入失败
            ("summon/src/setup/env.rs", 1),            // 无执行位 → 有执行位的判定
            ("summon/src/setup/npm.rs", 3),            // 2 处改目录权限;1 处只看执行位
            ("summon/src/setup/onboard.rs", 1),        // 只做路径探测
            // 本文件的三处:兜底直接写、共享那份只读、用例里改共享那份的权限位
            ("summon/src/test_support.rs", 3),
        ];
        // `from_mode(…)` / `.mode(…)` / `set_mode(…)` 的八进制字面量,倒数第三位(属主)是奇数;
        // 外加直接调 chmod 命令。
        let exec_bits = |text: &str| {
            let literal = ["from_mode(0o", ".mode(0o", "set_mode(0o"]
                .iter()
                .flat_map(|needle| text.match_indices(needle).map(|(at, n)| at + n.len()))
                .filter(|&at| {
                    let digits: Vec<u8> = text.as_bytes()[at..]
                        .iter()
                        .copied()
                        .take_while(|b| matches!(b, b'0'..=b'7' | b'_'))
                        .filter(|b| *b != b'_')
                        .collect();
                    digits.len() >= 3 && (digits[digits.len() - 3] - b'0') % 2 == 1
                })
                .count();
            literal + text.matches("Command::new(\"chmod\")").count()
        };
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut found = std::collections::BTreeMap::<String, usize>::new();
        let mut stack = vec![crates.join("summon/src"), crates.join("summon-core/src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let count = exec_bits(&std::fs::read_to_string(&path).unwrap());
                    if count > 0 {
                        let name = path.strip_prefix(&crates).unwrap().to_string_lossy();
                        found.insert(name.into_owned(), count);
                    }
                }
            }
        }
        let allowed: std::collections::BTreeMap<String, usize> = ALLOWED
            .iter()
            .map(|(name, count)| (name.to_string(), *count))
            .collect();
        assert_eq!(
            found, allowed,
            "新增了属主带执行位的权限设置:测试里要执行的脚本请用 test_support::write_test_executable 落盘;生产代码(含收紧目录权限)或确实不执行的,在 ALLOWED 登记并写明理由"
        );
    }

    /// 可执行夹具从 `$0` 认出调用方的目录;macOS 上同内容共用一份文件,共享那份改不动、权限位被
    /// 改了下一次取用就换一份。
    #[test]
    fn test_executables_share_one_file_and_still_see_their_own_directory() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let content = "#!/bin/sh\nprintf '%s' \"${0%/*}\"\n";
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (path_a, path_b) = (a.path().join("tool"), b.path().join("tool"));
        write_test_executable(&path_a, content);
        write_test_executable(&path_b, content);
        // 同一目录里换一份内容:旧的被换掉,不报「已存在」。
        write_test_executable(&path_b, "#!/bin/sh\nexit 3\n");
        write_test_executable(&path_b, content);

        for (path, dir) in [(&path_a, a.path()), (&path_b, b.path())] {
            let out = std::process::Command::new(path).output().unwrap();
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                dir.to_string_lossy()
            );
            assert_eq!(
                std::fs::canonicalize(path).unwrap(),
                std::fs::canonicalize(dir).unwrap().join("tool"),
                "path 本身是普通文件"
            );
        }
        if !cfg!(target_os = "macos") {
            return;
        }
        let ino = |path: &std::path::Path| std::fs::metadata(path).unwrap().ino();
        assert_eq!(ino(&path_a), ino(&path_b), "同内容只落一份");
        if !nix::unistd::geteuid().is_root() {
            assert!(
                std::fs::write(&path_a, "#!/bin/sh\n").is_err(),
                "改写共享那份必须失败"
            );
        }
        let out = std::process::Command::new(&path_b).output().unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            b.path().to_string_lossy()
        );

        // 改权限位那段用只属于本进程的内容:同一用户的别的 worktree 可能正在用 `content` 那份。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let own = format!("#!/bin/sh\n# {} {nanos}\n", std::process::id());
        let d = tempfile::tempdir().unwrap();
        let (path_d, path_c) = (d.path().join("tool"), d.path().join("again"));
        write_test_executable(&path_d, &own);
        std::fs::set_permissions(&path_d, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_test_executable(&path_c, &own);
        assert_ne!(ino(&path_c), ino(&path_d), "权限被改过的那份不再分出去");
        assert_eq!(
            std::fs::metadata(&path_c).unwrap().permissions().mode() & 0o7777,
            0o555
        );
        // 这份内容只此一回,别留在缓存里。
        let cache = std::env::temp_dir().join("summon-test-executables");
        for entry in std::fs::read_dir(cache).unwrap() {
            let entry = entry.unwrap().path();
            if std::fs::metadata(&entry).is_ok_and(|meta| meta.ino() == ino(&path_c)) {
                let _ = std::fs::remove_file(entry);
            }
        }
    }
}
