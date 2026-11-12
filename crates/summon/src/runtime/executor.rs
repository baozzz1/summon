//! 执行宿主缝(Executor):捕获源的一切宿主操作(起进程、读文件尾、tmux)都经它,
//! 不直接碰本机。现仅 `LocalExecutor`(本机 Command / fs / tmux);SSH 远端端点缓建(R9),
//! 但 trait 形状现在就钉死,`Subprocess` 从出生就经它 `spawn`。

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;

// 录制-回放转换:整组录制地基,debug-only(release 编译掉,生产零录制代码)。
#[cfg(all(test, debug_assertions))]
use summon_core::record::{is_safe_env_key, SpawnSpecRecord};

/// `SpawnSpec` ↔ `SpawnSpecRecord`(cassette serde 镜像)互转。`SpawnSpec` 含运行期语义(进程组等)
/// 不便直接 serde,故录制走镜像。env 录制经 `is_safe_env_key` 白名单过滤(只留 `SUMMON_*`,丢凭据)。
#[cfg(all(test, debug_assertions))]
impl SpawnSpec {
    /// 从运行期 `SpawnSpec` 抽出可序列化镜像:env 只保白名单键(凭据从源头不落 raw)。
    /// `own_process_group` 不入镜像——回放本身 spawn `cat`、不真起 CLI,无需复刻进程组语义。
    pub fn to_record(&self) -> SpawnSpecRecord {
        SpawnSpecRecord {
            program: self.program.clone(),
            args: self.args.clone(),
            work_dir: self.work_dir.clone(),
            stdin_null: self.stdin_null,
            env: self
                .env
                .iter()
                .filter(|(k, _)| is_safe_env_key(k))
                .cloned()
                .collect(),
        }
    }

    /// 由镜像回构 `SpawnSpec`(回放/比对用)。`own_process_group` 取默认 false(回放不起真子进程组)。
    // 仅 `#[cfg(test)]` 用:转换测 + RR4 `replay_scenario` harness 由录制镜像回构请求 spec(再交
    // ReplayExecutor 比对)。生产录制只 `to_record`(写),不回构,故标 allow。
    #[allow(dead_code)]
    pub fn from_record(rec: &SpawnSpecRecord) -> Self {
        SpawnSpec {
            program: rec.program.clone(),
            args: rec.args.clone(),
            work_dir: rec.work_dir.clone(),
            stdin_null: rec.stdin_null,
            env: rec.env.clone(),
            own_process_group: false,
        }
    }
}

/// 起一个子进程的描述:二进制 + 完整参数 + 工作目录 + 是否把 stdin 接 null + 额外环境变量。
/// `stdin_null` 给 codex 无头用(否则它会等 stdin 永不退出);stdout/stderr 恒 piped，捕获层消费
/// stdout 结构化事件，并只在异常退出时展示有界 stderr 尾部。
/// `env` 是叠加到子进程的环境(继承父进程后再覆盖):chat 轮据此注入 `SUMMON_CHAT_ID` 等,
/// 让轮内的 `summon ask` 等命令知道自己属于哪个 agent/会话、往哪条 control.sock 投。
#[derive(Clone, Default)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub work_dir: Option<String>,
    pub stdin_null: bool,
    pub env: Vec<(String, String)>,
    /// 无头一轮置 true:子进程自成进程组(组 id==pid,经 `process_group(0)`),便于 `killpg` 一次
    /// 命中 CLI 本体 + 它 fork 的工具孙子进程(从根上避免孤儿)。tmux/Transcript 路不用(claude
    /// 跑 tmux 持久层,不经此 spawn)。
    pub own_process_group: bool,
}

/// 正在跑的无头子进程的可 kill 句柄:只暴露 pgid + 一个 kill 方法,不暴露 `Child`(`Child` 仍归
/// `drive` 独占、由它 `wait` 回收)。clone 廉价(`Arc`),可塞进注册表给停机 hook / pause 路。
#[derive(Clone)]
pub struct ChildKiller {
    /// 进程组 id == 子进程 pid(因 `own_process_group` 时 `process_group(0)` 令 leader==child)。
    pgid: i32,
    /// 幂等:多次 `interrupt()` 只发一次信号。
    killed: Arc<AtomicBool>,
    /// 与信号幂等闸分离的逻辑取消位：Core gate waiter 不在子进程组里，必须靠它被唤醒。
    /// 所有 clone（含 InFlight 停机快照）共享，daemon stop 直接调 ChildKiller 也不会漏通知。
    cancelled: Arc<AtomicBool>,
    /// 启动失败后换进程时，UI 可能仍持旧句柄；旧把手必须把打断转交给新进程。
    successor: Arc<std::sync::Mutex<Option<ChildKiller>>>,
}

/// 无头一轮打断使用的信号——**红线:只 SIGINT,不许换成 SIGTERM**。
///
/// why:codex 收到 SIGINT 会写 `<turn_aborted>` 把被打断的上下文留在 transcript 里;换成 SIGTERM 走
/// 默认动作是硬终止,「⏸ 打断」/ `agents kill` / `daemon stop` 之后下一轮 resume 拿到的是空上下文——
/// **不崩、不报错,只是 agent 悄悄失忆**,靠人眼几乎发现不了。
///
/// 抽成具名函数只为给这条红线一个可断言的锚点(`interrupt_signal_is_sigint` 钉常量,
/// `interrupt_delivers_sigint_not_sigterm` 钉真实送达的信号)。
fn interrupt_signal() -> nix::sys::signal::Signal {
    nix::sys::signal::Signal::SIGINT
}

impl ChildKiller {
    pub fn new(pgid: i32) -> Self {
        Self {
            pgid,
            killed: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            successor: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// 进程组 id(= 子进程 pid,因 spawn 时 `process_group(0)` 令 leader==child)。
    /// - shutdown 宽限轮询 `group_alive(pgid)` 判定子进程是否仍在
    /// - SIGKILL 兜底 target(subprocess.rs 也记录 pgid 供观测)
    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// 判断两个句柄是否指向同一轮子进程。不只比 pgid(它可被 OS 复用),
    /// 还比较每轮独有的 Arc 身份。有界打断的延迟 SIGKILL 用它避免误杀下一轮。
    pub(crate) fn same_handle(&self, other: &Self) -> bool {
        self.pgid == other.pgid && Arc::ptr_eq(&self.killed, &other.killed)
    }

    /// 发打断信号到整个进程组(cursor 干净退、codex 写 `<turn_aborted>` 保留上下文)。幂等、绝不
    /// panic:killpg 失败(进程可能已自行退出)只吞掉——本身就是 best-effort 兜底。
    pub fn interrupt(&self) {
        self.cancel_waiters();
        if let Some(next) = self.successor.lock().ok().and_then(|slot| slot.clone()) {
            next.interrupt();
            return;
        }
        self.signal(interrupt_signal());
    }

    /// 仅在旧 Child 已 wait 回收后调用；先接上旧把手，再向上发布新把手，消除控制面交接空窗。
    pub(crate) fn forward_to(&self, next: &Self) {
        let Ok(mut slot) = self.successor.lock() else {
            next.interrupt();
            return;
        };
        *slot = Some(next.clone());
        if self.cancelled.load(Ordering::Acquire) {
            next.interrupt();
        }
    }

    /// 只发布逻辑取消，不抢占 SIGINT 的一次性闸。RPC 软打断在等待 `turn/interrupt` 收敛前
    /// 先用它唤醒 native ask；若宽限到期，后续 `interrupt()` 仍必须真的能发 SIGINT。
    pub(crate) fn cancel_waiters(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn cancellation_flag(&self) -> &AtomicBool {
        &self.cancelled
    }

    /// RPC 会话进程跨轮复用；上一轮若只完成了软打断，进程仍活着，但取消位不能泄漏到下一轮。
    /// 只能在新一轮尚未发布到 InFlight 之前调用。
    pub(crate) fn reset_waiter_cancellation(&self) {
        self.cancelled.store(false, Ordering::Release);
    }

    /// 是否已被打断过(interrupt/kill 调用过)。无头收尾据此区分「被外部打断」(预期,打断/停机路另处理,
    /// 不算失败)与「子进程自己异常退出」(review #9:非零退出 + 零输出才映射 Failed 贴 ERROR)。
    pub fn was_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    /// SIGKILL 整组:停机宽限超时后的最后兜底(SIGINT 没杀死才用)。不走幂等闸(可在 SIGINT 之后
    /// 再补一刀),失败只吞。daemon 停机(shutdown_interrupt_all)和 task run 超时(kill_one)均使用。
    pub fn kill(&self) {
        self.cancel_waiters();
        if let Some(next) = self.successor.lock().ok().and_then(|slot| slot.clone()) {
            next.kill();
            return;
        }
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    fn signal(&self, sig: nix::sys::signal::Signal) {
        // 幂等:已发过就直接返回(避免对已 reuse 的 pid 误伤)。
        if self.killed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(self.pgid), sig);
    }
}

/// 宿主原语抽象:Local 与(将来)Ssh 各一实现。捕获源只认这三个动作,不硬编码本机。
pub trait Executor {
    /// 起子进程。Local:`Command`;Ssh(将来):`ssh host -- <cmd>`。
    fn spawn(&self, spec: &SpawnSpec) -> anyhow::Result<Child>;

    // read_file_tail / tmux 是为 R6(Transcript 源:tail 钉死 sid 的 jsonl + tmux send-keys 注入)
    // 现在就钉死的 trait 形状,Subprocess 暂只用 spawn,故下面两个方法尚无生产调用方(单测已覆盖
    // LocalExecutor 的实现)。R6 接上即消;现允许 dead_code、不删,以免届时改 trait 形状。
    /// 从 `from_byte` 偏移读到文件尾(R6 转录 tail 用)。Local:fs seek+read。
    #[allow(dead_code)]
    fn read_file_tail(&self, path: &Path, from_byte: u64) -> anyhow::Result<Vec<u8>>;

    /// 从偏移起最多读取 `max_bytes`。checkpoint/救援探针只需要短窗口，不能借
    /// `read_file_tail` 把 offset 后的整份长寿命 transcript 搬进内存。
    fn read_file_range(
        &self,
        path: &Path,
        from_byte: u64,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let mut bytes = self.read_file_tail(path, from_byte)?;
        bytes.truncate(max_bytes);
        Ok(bytes)
    }

    /// 只取文件长度，不把长寿命转录整文件读进内存。默认实现保兼容，Local/Ssh 覆盖为 O(1)/远端 wc。
    fn file_len(&self, path: &Path) -> anyhow::Result<u64> {
        Ok(self.read_file_tail(path, 0)?.len() as u64)
    }

    /// 目录下的文件名(不递归)。拿不到就是空:子 agent 转录这类旁路观测在远端执行器上直接不做,
    /// 而不是猜。
    fn list_dir(&self, _path: &Path) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// 文件对象身份。Transcript tail 用它识别「路径没变、底下文件却被原子替换」；只比较长度
    /// 不够，因为替换后的 JSONL 可以很快长回旧 offset 之上。远端/回放实现拿不到稳定身份时返回
    /// `None`，调用方仍会保留长度回退与 deadline 前的 exact-marker 重扫。
    fn file_identity(&self, _path: &Path) -> anyhow::Result<Option<FileIdentity>> {
        Ok(None)
    }

    /// 跑一条 tmux 命令、返回 stdout(将来给 scrape/transcript 注入用)。Local:本机 tmux。
    #[allow(dead_code)]
    fn tmux(&self, args: &[&str]) -> anyhow::Result<String>;
}

/// 同一路径当前指向的文件对象。只承载比较语义，不把平台 metadata 泄漏到捕获层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

/// 本机执行器:Command / fs / tmux 直连。
pub struct LocalExecutor;

impl Executor for LocalExecutor {
    fn spawn(&self, spec: &SpawnSpec) -> anyhow::Result<Child> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(wd) = &spec.work_dir {
            cmd.current_dir(wd);
        }
        // 干净启动(fail-closed):清空全部继承环境,只显式注入系统基础变量 + spec.env。
        // 避免 ANTHROPIC_* 等 daemon 环境变量泄漏到无头子进程——agent 需要的在 spec.env 里
        // (agent 身份由 agent_identity_env 注入、task run env 由 build_run_env 注入)。
        cmd.env_clear();
        for (k, v) in crate::runtime::daemon::essential_system_env() {
            cmd.env(k, v);
        }
        // 叠加上下文环境(agent 自己的飞书凭据 + task run 的 SUMMON_*)。
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        // 所有普通 summon-owned 无头 CLI 都必须绕过用户级 external bridge Hook。
        // 放在 spec.env 后强制覆盖：这是保留安全键，调用方不能把它改回 0 后冒充外部会话。
        // 严格 handoff 不经本执行器，会单独携带 token 并保留 Hook ACK 能力。
        cmd.env(crate::bridge::MANAGED_SESSION_ENV, "1");
        // stdin=null:codex 无头否则会等 stdin 输入,永不退出;claude/cursor 不受影响。
        if spec.stdin_null {
            cmd.stdin(Stdio::null());
        }
        // 无头一轮自成进程组(setpgid(0,0):新组,leader==child pid),便于 killpg 整组(含工具孙子)。
        if spec.own_process_group {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = cmd
            .spawn()
            .with_context(|| summon_i18n::format!("r6c67504058", spec.program))?;
        Ok(child)
    }

    fn list_dir(&self, path: &Path) -> anyhow::Result<Vec<String>> {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| summon_i18n::format!("r9e4e3402e2", path.display()))
            }
        };
        Ok(entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect())
    }

    fn read_file_tail(&self, path: &Path, from_byte: u64) -> anyhow::Result<Vec<u8>> {
        let mut f = std::fs::File::open(path)
            .with_context(|| summon_i18n::format!("ref20c81fa7", path.display()))?;
        f.seek(SeekFrom::Start(from_byte))
            .with_context(|| summon_i18n::format!("r1cebcd6d44", path.display()))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .with_context(|| summon_i18n::format!("r12a88f49b0", path.display()))?;
        Ok(buf)
    }

    fn read_file_range(
        &self,
        path: &Path,
        from_byte: u64,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let mut f = std::fs::File::open(path)
            .with_context(|| summon_i18n::format!("ref20c81fa7", path.display()))?;
        f.seek(SeekFrom::Start(from_byte))
            .with_context(|| summon_i18n::format!("r1cebcd6d44", path.display()))?;
        let mut buf = Vec::with_capacity(max_bytes.min(64 * 1024));
        f.take(max_bytes as u64)
            .read_to_end(&mut buf)
            .with_context(|| summon_i18n::format!("r5afaf5f3a2", path.display()))?;
        Ok(buf)
    }

    fn file_len(&self, path: &Path) -> anyhow::Result<u64> {
        std::fs::metadata(path)
            .map(|metadata| metadata.len())
            .with_context(|| summon_i18n::format!("rd61949da6c", path.display()))
    }

    fn file_identity(&self, path: &Path) -> anyhow::Result<Option<FileIdentity>> {
        use std::os::unix::fs::MetadataExt as _;

        std::fs::metadata(path)
            .map(|metadata| {
                Some(FileIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                })
            })
            .with_context(|| summon_i18n::format!("r86bbfc1d31", path.display()))
    }

    fn tmux(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = Command::new(summon_core::tools::resolve(summon_core::tools::Tool::Tmux))
            .args(args)
            .output()
            .context(summon_i18n::text!("recca1074d4"))?;
        // 退出码非 0 也把 stdout 带回(调用方按需判定);stderr 这里不抛,保持轻量。
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// 远端执行器(R9):把 `Executor` 三动作经 `ssh` 跑到远端 host —— spawn=`ssh … -- env… prog args`、
/// read_file_tail=`ssh … -- tail -c +N path`、tmux=`ssh … -- tmux args`。**argv/命令构造是确定性的、
/// 已单测**;但**运行语义(远端进程组 killpg、stdout 流式、tail 字节偏移在远端 shell 的行为)真机待验**
/// —— 接第一台远端机时按真机校准。`#[allow(dead_code)]`:运行 wiring(哪个 agent 走远端、端点配置)
/// 待第一台远端机接,现仅本模块单测构造它(同 trait 里 `read_file_tail`/`tmux` 的「接上即消」allow)。
#[allow(dead_code)]
pub struct SshExecutor {
    host: String,
    port: Option<u16>,
    identity: Option<String>,
}

#[allow(dead_code)]
impl SshExecutor {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: None,
            identity: None,
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// ssh 命令前缀(不含远端命令):`ssh [-p N] [-i id] -o BatchMode=yes <host>`。
    /// `BatchMode=yes` 非交互(不卡密码提示,fail-fast)。纯件、可单测。
    fn ssh_prefix(&self) -> Vec<String> {
        let mut v = vec![summon_core::tools::resolve(summon_core::tools::Tool::Ssh)
            .to_string_lossy()
            .to_string()];
        if let Some(p) = self.port {
            v.push("-p".to_string());
            v.push(p.to_string());
        }
        if let Some(id) = &self.identity {
            v.push("-i".to_string());
            v.push(id.clone());
        }
        v.push("-o".to_string());
        v.push("BatchMode=yes".to_string());
        v.push(self.host.clone());
        v
    }

    /// 把 env + program + args 拼成远端 shell 命令串(`K=V … 'prog' 'arg'…`,单引号转义防注入)。
    /// 纯件、可单测。`stdin_null`/`own_process_group` 在远端语义不同(真机待验),v1 不复刻。
    fn remote_cmd(spec: &SpawnSpec) -> String {
        let mut parts: Vec<String> = spec
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", sh_quote(v)))
            .collect();
        // 与 LocalExecutor 一致，保留 marker 最后赋值，禁止 spec.env 覆盖。
        parts.push(format!(
            "{}={}",
            crate::bridge::MANAGED_SESSION_ENV,
            sh_quote("1")
        ));
        parts.push(sh_quote(&spec.program));
        for a in &spec.args {
            parts.push(sh_quote(a));
        }
        parts.join(" ")
    }

    /// 在远端跑一条命令、同步取 stdout(read_file_tail/tmux 复用)。
    fn ssh_output(&self, remote: &str) -> anyhow::Result<Vec<u8>> {
        let prefix = self.ssh_prefix();
        let out = Command::new(&prefix[0])
            .args(&prefix[1..])
            .arg("--")
            .arg(remote)
            .output()
            .with_context(|| summon_i18n::format!("rc9afdff310", self.host))?;
        Ok(out.stdout)
    }
}

/// 单引号 shell 转义(`'` → `'\''`),把任意串安全嵌进远端 `'…'`。纯件。
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[allow(dead_code)]
impl Executor for SshExecutor {
    fn spawn(&self, spec: &SpawnSpec) -> anyhow::Result<Child> {
        let prefix = self.ssh_prefix();
        let mut cmd = Command::new(&prefix[0]);
        cmd.args(&prefix[1..]).arg("--").arg(Self::remote_cmd(spec));
        if spec.stdin_null {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.spawn()
            .with_context(|| summon_i18n::format!("r2e4a520855", self.host))
    }

    fn read_file_tail(&self, path: &Path, from_byte: u64) -> anyhow::Result<Vec<u8>> {
        // 远端 `tail -c +N`:N 是 1-based 字节起点,故 from_byte+1。
        let remote = format!(
            "tail -c +{} {}",
            from_byte + 1,
            sh_quote(&path.display().to_string())
        );
        self.ssh_output(&remote)
    }

    fn read_file_range(
        &self,
        path: &Path,
        from_byte: u64,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let remote = format!(
            "tail -c +{} {} | head -c {}",
            from_byte + 1,
            sh_quote(&path.display().to_string()),
            max_bytes
        );
        self.ssh_output(&remote)
    }

    fn file_len(&self, path: &Path) -> anyhow::Result<u64> {
        let remote = format!("wc -c < {}", sh_quote(&path.display().to_string()));
        let raw = self.ssh_output(&remote)?;
        String::from_utf8_lossy(&raw)
            .trim()
            .parse::<u64>()
            .with_context(|| summon_i18n::format!("r253ba74c0b", path.display()))
    }

    fn tmux(&self, args: &[&str]) -> anyhow::Result<String> {
        let joined = std::iter::once("tmux".to_string())
            .chain(args.iter().map(|a| sh_quote(a)))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(String::from_utf8_lossy(&self.ssh_output(&joined)?).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 飞书身份隔离(回归):spawn 起的子进程**不得**继承父进程里兄弟 agent 的 `FEISHU_*`,且
    /// `spec.env` 注入的本 agent 自己凭据(规范名)可见。即「先清 FEISHU_* 继承、再叠加自己的」。
    ///
    /// 标 `#[ignore]`:本测要在父进程 env 里塞兄弟键(`set_var`),而进程 env 是全局可变态——并行
    /// 跑时 `set_var`/`remove_var` 会与别的测试 `fork`/`execve` 读 `environ` 抢,造成无关测试偶发崩。
    /// 故隔离单跑:`cargo test -p summon -- --ignored --test-threads=1`。日常默认覆盖靠纯函数
    /// `is_feishu_identity_key` 的单测(不碰全局 env)。
    #[test]
    #[ignore]
    fn spawn_scrubs_inherited_feishu_and_injects_own() {
        use std::io::Read as _;
        // 父进程里放一个兄弟 agent 的身份键(模拟 owner 把所有 FEISHU_* export 进了 shell)。
        std::env::set_var("FEISHU_OTHERBOT_APP_ID", "sibling-leak");
        let spec = SpawnSpec {
            program: "sh".into(),
            // 子进程打印「兄弟键|自己的规范键」。兄弟键应被清成空,规范键应是注入值。
            args: vec![
                "-c".into(),
                "printf '%s|%s' \"$FEISHU_OTHERBOT_APP_ID\" \"$FEISHU_APP_ID\"".into(),
            ],
            stdin_null: true,
            env: vec![("FEISHU_APP_ID".into(), "my-own-id".into())],
            ..Default::default()
        };
        let mut child = LocalExecutor.spawn(&spec).unwrap();
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let _ = child.wait();
        std::env::remove_var("FEISHU_OTHERBOT_APP_ID");
        assert_eq!(
            out, "|my-own-id",
            "兄弟 FEISHU_* 必须被清(空),自己的规范键必须可见"
        );
    }

    /// `to_record`:env 只留白名单(`SUMMON_*`),凭据键(`FEISHU_*`/`*_SECRET`/`*_TOKEN`)被丢;
    /// `from_record` 回构后非 env 字段逐字一致。
    // `SpawnSpec::{to_record,from_record}` 是 debug-only 镜像转换,release 下不存在。
    #[cfg(all(test, debug_assertions))]
    #[test]
    fn spawn_spec_record_drops_secrets_and_round_trips() {
        let spec = SpawnSpec {
            program: "codex".into(),
            args: vec!["exec".into(), "--json".into()],
            work_dir: Some("/tmp/wd".into()),
            stdin_null: true,
            env: vec![
                ("SUMMON_CHAT_ID".into(), "oc_1".into()),
                ("FEISHU_APP_SECRET".into(), "shh".into()),
                ("OPENAI_API_KEY".into(), "sk-xxx".into()),
                ("SUMMON_CONVERSATION_TAG".into(), "g1".into()),
            ],
            own_process_group: true,
        };
        let rec = spec.to_record();
        // 只保白名单键、保顺序;凭据被丢。
        assert_eq!(
            rec.env,
            vec![
                ("SUMMON_CHAT_ID".to_string(), "oc_1".to_string()),
                ("SUMMON_CONVERSATION_TAG".to_string(), "g1".to_string()),
            ]
        );
        // 回构:非 env 字段一致(env 已被白名单缩减,故只对回构后的 record 自洽断言)。
        let back = SpawnSpec::from_record(&rec);
        assert_eq!(back.program, spec.program);
        assert_eq!(back.args, spec.args);
        assert_eq!(back.work_dir, spec.work_dir);
        assert_eq!(back.stdin_null, spec.stdin_null);
        assert_eq!(back.env, rec.env);
        // own_process_group 不入镜像,回构恒 false(回放不复刻进程组)。
        assert!(!back.own_process_group);
    }

    #[test]
    fn read_file_tail_reads_from_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.txt");
        std::fs::write(&p, b"hello world").unwrap();
        let exec = LocalExecutor;
        // 从 0 读 = 全文
        assert_eq!(exec.read_file_tail(&p, 0).unwrap(), b"hello world");
        // 从 6 读 = 尾巴(模拟增量 tail)
        assert_eq!(exec.read_file_tail(&p, 6).unwrap(), b"world");
        // 偏移到尾后 = 空
        assert_eq!(exec.read_file_tail(&p, 11).unwrap(), b"");
        assert_eq!(exec.file_len(&p).unwrap(), 11);
    }

    #[test]
    fn read_file_range_never_reads_past_the_requested_window() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("range.txt");
        std::fs::write(&p, b"0123456789").unwrap();
        let exec = LocalExecutor;
        assert_eq!(exec.read_file_range(&p, 3, 4).unwrap(), b"3456");
        assert_eq!(exec.read_file_range(&p, 8, 99).unwrap(), b"89");
        assert!(exec.read_file_range(&p, 4, 0).unwrap().is_empty());
    }

    /// R9:ssh 前缀含 port/identity/BatchMode、host 在末;裸 host 不带 -p/-i。
    #[test]
    fn ssh_prefix_builds_flags() {
        let e = SshExecutor::new("user@host")
            .with_port(2222)
            .with_identity("/k/id");
        let p = e.ssh_prefix();
        assert_eq!(p[0], "ssh");
        assert!(p.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(p.windows(2).any(|w| w == ["-i", "/k/id"]));
        assert!(p.contains(&"BatchMode=yes".to_string()));
        assert_eq!(p.last().unwrap(), "user@host");
        let bare = SshExecutor::new("h").ssh_prefix();
        assert!(!bare.iter().any(|x| x == "-p" || x == "-i"));
    }

    /// R9:远端命令把 env/program/args 单引号转义(含空格 arg 必须被引,防注入/被 shell 拆词)。
    #[test]
    fn ssh_remote_cmd_quotes() {
        let spec = SpawnSpec {
            program: "codex".into(),
            args: vec!["exec".into(), "a b".into()],
            env: vec![
                ("SUMMON_CHAT_ID".into(), "oc_1".into()),
                ("SUMMON_MANAGED_SESSION".into(), "0".into()),
            ],
            ..Default::default()
        };
        let cmd = SshExecutor::remote_cmd(&spec);
        assert!(cmd.contains("SUMMON_MANAGED_SESSION='1'"));
        assert!(
            cmd.rfind("SUMMON_MANAGED_SESSION='1'") > cmd.rfind("SUMMON_MANAGED_SESSION='0'"),
            "保留 marker 必须最后覆盖调用方 env:{cmd}"
        );
        assert!(cmd.contains("SUMMON_CHAT_ID='oc_1'"));
        assert!(cmd.contains("'codex'"));
        assert!(cmd.contains("'a b'"), "含空格 arg 必须单引号包裹");
    }

    #[test]
    fn sh_quote_escapes_single_quote() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(sh_quote("plain"), "'plain'");
    }

    #[test]
    fn spawn_runs_and_streams_stdout() {
        use std::io::Read as _;
        let exec = LocalExecutor;
        let spec = SpawnSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "echo hi".into()],
            work_dir: None,
            stdin_null: true,
            ..Default::default()
        };
        let mut child = exec.spawn(&spec).unwrap();
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let _ = child.wait();
        assert_eq!(out.trim(), "hi");
    }

    #[test]
    fn local_spawn_marks_summon_owned_sessions() {
        use std::io::Read as _;
        let spec = SpawnSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "printf '%s' \"$SUMMON_MANAGED_SESSION\"".into(),
            ],
            stdin_null: true,
            env: vec![("SUMMON_MANAGED_SESSION".into(), "0".into())],
            ..Default::default()
        };
        let mut child = LocalExecutor.spawn(&spec).unwrap();
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let _ = child.wait();
        assert_eq!(out, "1");
    }

    /// `own_process_group=true` → 子进程自成组,pgid == 子进程 pid,且 != daemon 自身 pgid。
    #[test]
    fn spawn_in_own_process_group_sets_pgid() {
        let exec = LocalExecutor;
        let spec = SpawnSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 5".into()],
            stdin_null: true,
            own_process_group: true,
            ..Default::default()
        };
        let mut child = exec.spawn(&spec).unwrap();
        let pid = child.id() as i32;
        let child_pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid)))
            .unwrap()
            .as_raw();
        let self_pgid = nix::unistd::getpgid(None).unwrap().as_raw();
        // 收尾:杀掉 sleep,别留后台进程。
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(child_pgid, pid, "子进程应自成组,pgid==pid");
        assert_ne!(child_pgid, self_pgid, "应与 daemon 自身组隔离");
    }

    /// killpg 应一次命中**整组**:含 CLI fork 出的工具孙子进程(无孤儿)。这里验「组级到达性」——
    /// 父 sh fork 两个后台 sleep 孙子都在同一组,killpg 一发把整组清掉。用 `kill()`(SIGKILL)而非
    /// `interrupt()`:SIGKILL 不可被 SIG_IGN 屏蔽,排除「非交互 sh 把后台作业 SIGINT 设成 SIG_IGN」
    /// 这一 fixture 假象,纯粹验证信号确实送达了组里每个成员。SIGINT 的「干净打断」语义由
    /// subprocess 的 interrupt_during_drive_breaks_loop / drive_kills_child_on_panic_unwind 覆盖。
    #[test]
    fn killpg_reaps_whole_group() {
        let exec = LocalExecutor;
        let spec = SpawnSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 30 & sleep 30 & wait".into()],
            stdin_null: true,
            own_process_group: true,
            ..Default::default()
        };
        let mut child = exec.spawn(&spec).unwrap();
        let pid = child.id() as i32;
        let killer = ChildKiller::new(pid);
        killer.kill(); // SIGKILL 整组(不可屏蔽),验组级到达
        let _ = child.wait();
        // 轮询断言整组已空(被 reparent 的孙子可能要几十毫秒才回收,固定 sleep 易抖动)。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut empty = false;
        while std::time::Instant::now() < deadline {
            let out = Command::new("pgrep")
                .args(["-g", &pid.to_string()])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .is_none()
            {
                empty = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(empty, "killpg 应命中整组、全部清空(无孤儿)");
    }

    /// 红线守护(源码锚点):无头打断的信号**只能是 SIGINT**。零成本断言,改 `interrupt_signal()`
    /// 必红。行为侧证明见 `interrupt_delivers_sigint_not_sigterm`(两条一起才封死这条红线:
    /// 这条钉常量,那条钉 `interrupt()` 真发的是它)。
    #[test]
    fn interrupt_signal_is_sigint() {
        assert_eq!(
            interrupt_signal(),
            nix::sys::signal::Signal::SIGINT,
            "红线:无头打断只 SIGINT 不 SIGTERM——SIGTERM 是硬终止,codex 写不出 <turn_aborted>,\
             下一轮 resume 拿到空上下文(不崩不报错,agent 悄悄失忆)"
        );
    }

    /// 红线守护(行为):`interrupt()` 送达子进程的确实是 **SIGINT** 而非 SIGTERM。
    ///
    /// 判别原理:fixture 装 INT handler 后 `exit 7`,故收到 INT → 正常退出码 7(`code()==Some(7)`);
    /// 收到 TERM 走默认动作 → 被信号杀死,`code()` 为 `None`。两者可区分。
    ///
    /// 坑:非交互 sh 会把后台作业的 SIGINT 设成 SIG_IGN 并被子进程继承,且 POSIX 规定入口即被忽略的
    /// 信号在非交互 shell 里 trap 不回来——用 sh 写 fixture 会得到「INT 被吞」的假象。故照
    /// `subprocess.rs::slow_cli` 的手法用 perl:`$SIG{INT}` 经 sigaction 装,不受继承的 SIG_IGN 影响。
    #[test]
    fn interrupt_delivers_sigint_not_sigterm() {
        use std::io::BufRead as _;
        let spec = SpawnSpec {
            program: "perl".into(),
            args: vec![
                "-e".into(),
                // $|=1 行缓冲,ready 立即可读;单进程无 fork → 组内只此一员。
                r#"$SIG{INT} = sub { exit 7 }; $| = 1; print "ready\n"; sleep 30;"#.into(),
            ],
            stdin_null: true,
            own_process_group: true,
            ..Default::default()
        };
        let mut child = LocalExecutor.spawn(&spec).unwrap();
        let pid = child.id() as i32;
        // 等 perl 明确回报 handler 已装好,避免固定 sleep 造成的调度假阳性。
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");

        ChildKiller::new(pid).interrupt();

        // 有界等待:信号没送到时不要挂满 30s(直接判失败,再收尾进程)。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            match child.try_wait().unwrap() {
                Some(s) => break Some(s),
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                None => break None,
            }
        };
        if status.is_none() {
            // 收尾:别把 sleep 30 留成后台孤儿。
            let _ = child.kill();
            let _ = child.wait();
        }
        let status = status.expect("interrupt() 后子进程应在 5s 内退出");
        assert_eq!(
            status.code(),
            Some(7),
            "红线:interrupt() 必须发 SIGINT(handler 退出码 7)。若为 None 说明进程被信号默认动作杀死\
             (SIGTERM/SIGKILL),codex 将丢失 <turn_aborted> 上下文"
        );
    }

    /// `interrupt()` 幂等:连调两次只发一次信号(`killed` 闸住第二次)。
    #[test]
    fn kill_idempotent() {
        let killer = ChildKiller::new(999_999); // 不存在的 pgid:不真发信号也无副作用
        assert!(!killer.killed.load(Ordering::SeqCst));
        killer.interrupt();
        assert!(killer.killed.load(Ordering::SeqCst), "首次置位");
        killer.interrupt(); // 第二次应被幂等闸短路(swap 返回 true)
        assert!(killer.killed.load(Ordering::SeqCst));
    }

    // 注:`tmux` 留作集成验证(需本机 tmux server),此处不单测,避免 CI 无 tmux 时假阳。
    #[test]
    fn spawn_respects_work_dir() {
        let dir = tempfile::tempdir().unwrap();
        let exec = LocalExecutor;
        let spec = SpawnSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "pwd".into()],
            work_dir: Some(dir.path().to_string_lossy().into_owned()),
            stdin_null: true,
            ..Default::default()
        };
        let mut child = exec.spawn(&spec).unwrap();
        let mut out = String::new();
        use std::io::Read as _;
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let _ = child.wait();
        // macos /var → /private/var symlink:用 contains 兜底而非全等
        let want = dir.path().to_string_lossy();
        assert!(
            out.trim()
                .ends_with(want.trim_start_matches("/private").trim_start_matches('/'))
                || out.trim() == want,
            "pwd={} want={want}",
            out.trim()
        );
    }
}
