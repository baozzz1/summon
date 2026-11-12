//! stdio 上跑 JSON-RPC 的**传输层**原语:换行分帧的共享写端、stderr 并发 drain 留尾、
//! reader 线程的事件信封、进程组信号。
//!
//! 三个客户端共用同一份:`codex::codex_app_server`(codex app-server)、
//! `capture::cursor_acp`(cursor ACP),以及它们各自的 chat 捕获路径。抽出来的理由不是洁癖 ——
//! 这里的每一条都是踩过的坑(整帧一次 write、pipe 了必须有人读、SIGINT 只发给自有进程组),
//! 抄第二份必然漂移,漂移的表现是「某一条通道偶发挂死 / 丢诊断信息」这种最难归因的故障。
//!
//! **不含协议语义**:握手、方法名、ServerRequest 应答表全在各自的客户端里。这一层只认字节和进程。

use std::io::{Read, Write};
use std::process::{ChildStderr, ChildStdin};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// reader 线程 → 消费者的事件信封。坏 JSON 不在 reader 线程里判死,原样带出让消费者决定。
pub enum ReaderEvent {
    Message(Value),
    InvalidJson(String),
}

pub const STDERR_TAIL_LIMIT_BYTES: usize = 16 * 1024;

/// 进程已退出后等 stderr EOF 的上界。只花在错误路径上,且此时进程已经死了。
pub const STDERR_EOF_GRACE: Duration = Duration::from_millis(500);

/// 传输层错误。**只描述字节与管道**,不掺协议语义 —— 各客户端把它映射进自己的错误枚举,
/// 对外错误面与调用点都不变。
#[derive(Debug)]
pub enum RpcStdioError {
    /// 请求序列化失败(实际上只可能是自己造的 Value 有问题)。
    Serialize(String),
    /// 写端已关(进程走了 / 主动 close)。
    Closed,
    Io(std::io::Error),
    /// 有界抢写锁失败:写端被别的线程长时间占着。
    WriteBusy,
}

/// 并发 drain 的 stderr 尾部。两条约束同时成立,所以既不能 null 也不能 pipe 了不读:
/// - **必须有人读**:CLI 的 stderr 是 tracing 日志,pipe 了没人 drain,写满内核缓冲后
///   进程会卡在写 stderr 上退不出去。
/// - **不能丢**:启动失败的真因(登录过期/配置坏)只写在 stderr。
///
/// 留尾策略同 `capture/subprocess.rs`,但投递方式不同:那边一轮一进程、EOF 时一次性取走即可;
/// 这里 client 是长命的,故用共享缓冲 —— 任何时刻都能读到当前尾部。
pub struct StderrTail {
    buf: Mutex<Vec<u8>>,
    at_eof: AtomicBool,
}

impl StderrTail {
    pub fn spawn(stderr: Option<ChildStderr>) -> Arc<Self> {
        let tail = Arc::new(Self {
            buf: Mutex::new(Vec::new()),
            at_eof: AtomicBool::new(stderr.is_none()),
        });
        let Some(mut stderr) = stderr else {
            return tail;
        };
        let sink = Arc::clone(&tail);
        thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            loop {
                let read = match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut buf = sink.buf.lock().unwrap_or_else(PoisonError::into_inner);
                buf.extend_from_slice(&chunk[..read]);
                if buf.len() > STDERR_TAIL_LIMIT_BYTES {
                    let overflow = buf.len() - STDERR_TAIL_LIMIT_BYTES;
                    buf.drain(..overflow);
                }
            }
            sink.at_eof.store(true, Ordering::Release);
        });
        tail
    }

    pub fn snapshot(&self) -> String {
        let bytes = self
            .buf
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        summon_core::scrubber::strip_ansi(&String::from_utf8_lossy(&bytes))
            .trim()
            .to_string()
    }

    /// 有界等 EOF:进程刚死时 drain 线程可能还没读到最后几行,不等就会拿到**不是最后**的尾部。
    /// 绝不无限等 —— 逃逸的孙进程可能一直持着 stderr fd。
    pub fn wait_for_eof(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.at_eof.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// 客户端 → server 的写端。抽成独立共享对象(而不是 client 私有的 `Option<ChildStdin>`)只为一件事:
/// **打断要能从别的线程写进去**。chat 线程在一轮里独占 client(流式消费通知),
/// 打断若也得先拿到 client 就永远轮不上。
///
/// 消费权由各客户端管理；Codex 的 reader 会单独路由带外 steer 的响应。
pub struct RpcWriter {
    stdin: Mutex<Option<ChildStdin>>,
    /// request id 分配器。放这里而不是 client 里,是因为打断帧也要一个 id —— 两个分配器迟早撞号,
    /// 撞了就是「响应对错请求」。
    next_id: AtomicU64,
}

impl RpcWriter {
    pub fn new(stdin: ChildStdin) -> Self {
        Self {
            stdin: Mutex::new(Some(stdin)),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn send(&self, value: &Value) -> Result<(), RpcStdioError> {
        let frame = frame_bytes(value)?;
        let mut guard = self.stdin.lock().unwrap_or_else(PoisonError::into_inner);
        write_frame(&mut guard, &frame)
    }

    /// 有界抢锁版,只给打断用:chat 线程若正卡在写(对端不读、管道写满),打断不该跟着一起卡死。
    /// 拿不到锁就放弃软打断,由调用方升级成信号 —— 那正是这种情形该走的路。
    pub fn send_bounded(&self, value: &Value, wait: Duration) -> Result<(), RpcStdioError> {
        let frame = frame_bytes(value)?;
        let deadline = Instant::now() + wait;
        loop {
            match self.stdin.try_lock() {
                Ok(mut guard) => return write_frame(&mut guard, &frame),
                Err(TryLockError::Poisoned(poisoned)) => {
                    return write_frame(&mut poisoned.into_inner(), &frame)
                }
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(RpcStdioError::WriteBusy);
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    /// 关写端 = 给对端 EOF。幂等。
    pub fn close(&self) {
        self.stdin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// **整帧一次 write**:JSON-RPC 靠换行分帧,先写正文再写 `\n` 在多写者下会被另一帧插进中间,
/// 对端从此再也对不齐。打断把手是另一个线程,这条于是从风格问题变成了正确性要求。
fn frame_bytes(value: &Value) -> Result<Vec<u8>, RpcStdioError> {
    let mut frame =
        serde_json::to_vec(value).map_err(|error| RpcStdioError::Serialize(error.to_string()))?;
    frame.push(b'\n');
    Ok(frame)
}

fn write_frame(stdin: &mut Option<ChildStdin>, frame: &[u8]) -> Result<(), RpcStdioError> {
    let stdin = stdin.as_mut().ok_or(RpcStdioError::Closed)?;
    stdin.write_all(frame).map_err(RpcStdioError::Io)?;
    stdin.flush().map_err(RpcStdioError::Io)
}

/// 一帧到底是不是 server→client 的**请求**(必须应答,漏一个就是整轮永久挂死)。
/// 判据:有 id、有 method、且既没有 result 也没有 error(后两者是响应)。
pub fn is_server_request(value: &Value) -> bool {
    value.get("id").is_some()
        && value.get("method").is_some()
        && value.get("result").is_none()
        && value.get("error").is_none()
}

/// 一次软打断实际发生了什么。codex(`turn/interrupt`)与 cursor(`session/cancel`)两条路
/// 逐字同义,所以住在传输层而不是任一客户端里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcInterruptOutcome {
    /// 此刻没有在跑的轮,什么都没做。**这是常态竞态而非异常**:轮刚跑完、打断刚好这时到。
    /// 此时若照发 SIGINT,杀掉的是整个会话进程 —— 连同下一轮。
    Idle,
    /// 软打断生效,轮在宽限内进入终态。进程仍活着,下一轮直接复用。
    Settled,
    /// 宽限内未见终态,已 SIGINT 整组。会话进程随之退出,下条消息经 resume / load 懒重连。
    Escalated,
}

/// 一次**轮内 steer**(往正在跑的那一轮追加用户输入,不打断)实际发生了什么。
///
/// 已接受、确定未接受、结果不明必须分开：只有确定未接受才允许自动补跑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcSteerOutcome {
    /// 服务端确认请求 id 对应的输入已追加到 expectedTurnId。
    Accepted,
    /// 此刻没有在跑的轮，没有发送。
    Idle,
    /// 这条 RPC 路没有 steer 能力。
    Unsupported,
    /// 确认尚未发送（写端关闭、抢锁失败或序列化失败）。
    Failed,
    /// 服务端明确拒绝请求。
    Rejected,
    /// 可能已经执行：I/O 写入失败、超时、断连或响应不符合协议。禁止自动重发。
    Unconfirmed,
}

/// 软打断的宽限:到点仍未见终态就升级 SIGINT。1s 取自设计文档的收敛上界
/// (验收是消费线程 2s 内见终态)。
pub const SOFT_INTERRUPT_GRACE: Duration = Duration::from_secs(1);

/// 抢写锁的上界,见 [`RpcWriter::send_bounded`]。
pub const INTERRUPT_WRITE_WAIT: Duration = Duration::from_millis(200);

/// `Drop` 里等 reader 线程收尾的上界,见 [`join_reader_bounded`]。
pub const READER_JOIN_GRACE: Duration = Duration::from_secs(2);

/// 有界 join reader 线程:超时就 detach(丢掉 handle)并留一条日志。
///
/// **为什么不能裸 `join()`**:reader 卡在 `stdout.read()` 上,而 EOF 要等**所有**持着那个 fd 的
/// 后代都退出。逃逸的孙进程(工具起的守护、被 nohup 的东西)持着它,EOF 就永远不来 —— 裸 join
/// 于是把调用线程永久钉死。这条路今天已经跑在 dispatcher 线程上(池的 `evict`/重开),卡住一次
/// 就是那个 agent 的全部路由停摆。
///
/// 与旁边「stderr drain 线程干脆不 join」是同一条理由的两种强度:那边完全等不起,这边 2s 内
/// 通常就结束了(进程组已被信号收掉),等一下能拿到干净的收尾,等不到就走人。
pub fn join_reader_bounded(reader: thread::JoinHandle<()>, what: &str) {
    let deadline = Instant::now() + READER_JOIN_GRACE;
    while !reader.is_finished() {
        if Instant::now() >= deadline {
            crate::observe::log::log(&summon_i18n::format!(
                "rd4e7abb940",
                READER_JOIN_GRACE.as_secs()
            ));
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = reader.join();
}

/// 给**自有**进程组发信号。`ESRCH`(组已经没了)不算错 —— 幂等收敛是调用方要的语义。
///
/// **绝不碰 tmux**:这里的 pgid 永远来自自己 `spawn` 且 `process_group(0)` 的子进程。
pub fn signal_group(pgid: i32, signal: nix::sys::signal::Signal) -> Result<(), std::io::Error> {
    use nix::errno::Errno;
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(std::io::Error::from_raw_os_error(error as i32)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn server_request_is_distinguished_from_response_and_notification() {
        assert!(is_server_request(
            &json!({"id":1,"method":"fs/read_text_file"})
        ));
        assert!(!is_server_request(&json!({"id":1,"result":{}})));
        assert!(!is_server_request(&json!({"id":1,"error":{"code":-1}})));
        assert!(!is_server_request(&json!({"method":"session/update"})));
    }

    #[test]
    fn frames_end_with_exactly_one_newline() {
        // 整帧一次 write 的前提:帧自带结尾换行,调用方不再补第二次写。
        let frame = frame_bytes(&json!({"a":1})).unwrap();
        assert_eq!(frame.last(), Some(&b'\n'));
        assert_eq!(frame.iter().filter(|b| **b == b'\n').count(), 1);
    }

    /// 造一个真 `ChildStdin`(`cat` 子进程)。不用裸 fd 拼:`RpcWriter` 的语义是「写给一个
    /// 会被读走的管道」,真进程才验得到 close 之后的行为。
    fn writer_on_a_real_child() -> (std::process::Child, RpcWriter) {
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("起 cat 子进程");
        let stdin = child.stdin.take().expect("cat 应有 stdin");
        (child, RpcWriter::new(stdin))
    }

    #[test]
    fn ids_are_monotonic_and_never_repeat() {
        // 撞号 = 「响应对错请求」。打断帧与正常请求共用同一个分配器就是为了这条。
        let (mut child, w) = writer_on_a_real_child();
        let ids: Vec<u64> = (0..5).map(|_| w.next_id()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
        w.close();
        let _ = child.wait();
    }

    #[test]
    fn writing_after_close_reports_closed_instead_of_panicking() {
        let (mut child, w) = writer_on_a_real_child();
        assert!(w.send(&json!({"id":1})).is_ok());
        w.close();
        assert!(matches!(
            w.send(&json!({"id":2})),
            Err(RpcStdioError::Closed)
        ));
        let _ = child.wait();
    }

    #[test]
    fn signalling_a_dead_group_is_not_an_error() {
        // 幂等收敛:组已经没了(ESRCH)不该报错,否则停机路径会因为「杀晚了」而失败。
        assert!(signal_group(999_999, nix::sys::signal::Signal::SIGINT).is_ok());
    }
}
