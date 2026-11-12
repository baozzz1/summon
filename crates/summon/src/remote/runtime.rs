//! 遥控运行时:每个绑定一个 worker 线程,按到达顺序串行把群消息打进 cmux 窗口;worker 自带
//! 一个回传线程跟踪转录。daemon 启动时复验身份后恢复。

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use summon_core::ipc::Resource;
use summon_core::registry::Agent;
use summon_core::remote::{CmuxHookSessions, RemoteBinding};

use super::cmux::{load_cmux_sessions, CmuxCli};
use super::inject::{self, DeliverOutcome};
use super::relay::run_relay;
use super::store::{check_liveness, same_binding, Liveness, RemoteStore};
use crate::bridge::process_probe::process_start_epoch_secs;
use crate::frontend::client::FeishuClient;
use crate::frontend::outbound::send_markdown_archived_anchored;
use crate::observe::log::log;
use crate::runtime::daemon::{fetch_inbound_resources, resolve_messaging, DrainAgents};
use crate::runtime::outbox_store::OutboxStore;

/// 打字后等输入框稳定再读屏的间隔。
const SETTLE: Duration = Duration::from_millis(400);
/// worker 等消息的单次上限,到点复查 stop 与停机位。
#[cfg(not(test))]
const RECV_TICK: Duration = Duration::from_secs(1);
// 这是检查停机位的轮询节拍，测试的失败上限仍由 wait_until 单独提供。
#[cfg(test)]
const RECV_TICK: Duration = Duration::from_millis(5);

const SESSIONS_UNREADABLE: &str = "rfd4dc2fc65";
const PROCESS_UNREADABLE: &str = "rab85fe683b";
// 结束原因会原样发进群(「遥控已结束:{原因}」),一律是固定短语、不带错误链或路径;
// 细节只进日志。
const RELAY_FAILED: &str = "r4c7d0a45f9";
const SCOPE_CHANGED: &str = "rdbf183ff37";
const ATTACHMENT_BACKSLASH: &str = "r8d96e151c6";
const ATTACHMENT_UNAVAILABLE: &str = "r9c17147543";

pub(crate) struct RemoteJob {
    pub message_id: String,
    pub anchor: Option<String>,
    pub lines: Vec<String>,
    pub resources: Vec<Resource>,
}

impl RemoteJob {
    /// 回复、贴表情、下载附件都要真实的飞书消息 id:恢复重放时 `message_id` 是本地幂等键,
    /// 真 id 在 `anchor`。
    fn feishu_id(&self) -> Option<&str> {
        self.anchor
            .as_deref()
            .filter(|id| !id.is_empty())
            .or_else(|| Some(self.message_id.as_str()).filter(|id| !id.is_empty()))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JobResult {
    /// 固定短语,会原样写进绑定记录并发进群。
    Ended(&'static str),
    Submitted,
    Refused(String),
}

pub(crate) struct JobDeps<'a> {
    /// `Err` 是复验本身没做成时给群里的说明:它证明不了会话已结束,只拒这一条、绑定照旧。
    pub liveness: &'a dyn Fn(&RemoteBinding) -> Result<Liveness, String>,
    pub deliver: &'a dyn Fn(&[String]) -> DeliverOutcome,
    pub fetch_attachments: &'a dyn Fn(&RemoteJob) -> Option<String>,
}

/// 一条群消息的编排:先复验身份,再拼附件说明,最后打进输入框。
pub(crate) fn handle_job(binding: &RemoteBinding, job: RemoteJob, deps: &JobDeps) -> JobResult {
    match (deps.liveness)(binding) {
        Err(notice) => return JobResult::Refused(notice),
        Ok(Liveness::Ended(reason)) => return JobResult::Ended(reason),
        Ok(Liveness::Alive) => {}
    }
    let note = (deps.fetch_attachments)(&job);
    let mut lines = job.lines;
    if let Some(note) = note.filter(|note| !note.trim().is_empty()) {
        // cmux send 把反斜杠序列当按键;正文在入站时已挡掉,附件说明拼在这里,要再挡一道。
        if note.contains('\\') {
            return JobResult::Refused(::summon_core::i18n::label(ATTACHMENT_BACKSLASH).into());
        }
        // 只有附件的消息没有正文,说明行直接打,不留前导空行。
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(note.lines().map(str::to_string));
    }
    // 只有附件、附件又没取到:没有任何字可打,对空输入框按回车会被当成「已提交」。
    if lines.is_empty() {
        return JobResult::Refused(::summon_core::i18n::label(ATTACHMENT_UNAVAILABLE).into());
    }
    match (deps.deliver)(&lines) {
        DeliverOutcome::Submitted => JobResult::Submitted,
        DeliverOutcome::Refused(notice) => JobResult::Refused(notice),
    }
}

/// 身份复验(不变量 5)。`Err` 是 ps 本身出错:它证明不了进程已退出,调用方不得据此结束绑定。
fn verify_identity(
    binding: &RemoteBinding,
    hooks: &CmuxHookSessions,
    process_start: &dyn Fn(u32) -> anyhow::Result<Option<i64>>,
) -> anyhow::Result<Liveness> {
    let started = process_start(binding.pid)?;
    Ok(check_liveness(binding, hooks, &|_| started))
}

/// worker 每条消息前的复验。cmux 记录或进程状态读不出时只拒这一条:瞬时的读盘或 ps 失败
/// 不能把活着的会话判成已结束。
fn job_liveness(
    binding: &RemoteBinding,
    load_sessions: &dyn Fn() -> anyhow::Result<CmuxHookSessions>,
    process_start: &dyn Fn(u32) -> anyhow::Result<Option<i64>>,
) -> Result<Liveness, String> {
    let hooks = load_sessions().map_err(|error| {
        log(&summon_i18n::format!(
            "r133440628c",
            binding.agent,
            binding.chat_id
        ));
        ::summon_core::i18n::label(SESSIONS_UNREADABLE).to_string()
    })?;
    verify_identity(binding, &hooks, process_start).map_err(|error| {
        log(&summon_i18n::format!(
            "rde9c599ac1",
            binding.agent,
            binding.chat_id,
            binding.pid
        ));
        ::summon_core::i18n::label(PROCESS_UNREADABLE).to_string()
    })
}

pub(crate) fn ended_notice(reason: &str) -> String {
    summon_i18n::format!("re37dfb087c")
}

/// worker 的外部 IO。生产走 cmux CLI 与飞书,测试换替身。
trait WorkerIo: Send {
    fn liveness(&self, binding: &RemoteBinding) -> Result<Liveness, String>;
    fn deliver(&self, binding: &RemoteBinding, lines: &[String]) -> DeliverOutcome;
    fn fetch_attachments(&self, binding: &RemoteBinding, job: &RemoteJob) -> Option<String>;
    /// 给已提交的那条群消息贴「已送达」表情。
    fn mark_delivered(&self, message_id: &str);
    /// 在群里回一句,锚在那条群消息上。
    fn reply(&self, binding: &RemoteBinding, anchor: Option<&str>, text: &str);
}

struct LiveIo {
    agent: Agent,
    fc: Arc<FeishuClient>,
}

impl WorkerIo for LiveIo {
    fn liveness(&self, binding: &RemoteBinding) -> Result<Liveness, String> {
        job_liveness(binding, &load_cmux_sessions, &process_start_epoch_secs)
    }

    fn deliver(&self, binding: &RemoteBinding, lines: &[String]) -> DeliverOutcome {
        inject::deliver(
            &CmuxCli::installed(),
            &binding.surface_id,
            lines,
            SETTLE,
            &format!("[{}] chat={}", binding.agent, binding.chat_id),
        )
    }

    fn fetch_attachments(&self, binding: &RemoteBinding, job: &RemoteJob) -> Option<String> {
        if job.resources.is_empty() {
            return None;
        }
        let note = fetch_inbound_resources(
            &self.agent,
            &self.fc,
            &binding.chat_type,
            &binding.chat_id,
            job.feishu_id()?,
            &job.resources,
        );
        (!note.is_empty()).then_some(note)
    }

    fn mark_delivered(&self, message_id: &str) {
        let emoji = resolve_messaging(&self.agent).done_reaction;
        if let Err(error) = self.fc.react(message_id, &emoji) {
            log(&summon_i18n::format!("r908d4fd2c6", self.agent.name));
        }
    }

    fn reply(&self, binding: &RemoteBinding, anchor: Option<&str>, text: &str) {
        if let Err(error) = send_markdown_archived_anchored(
            &self.fc,
            &self.agent.state_dir(),
            &binding.chat_type,
            &binding.chat_id,
            anchor,
            text,
        ) {
            log(&summon_i18n::format!(
                "rf67e2d9301",
                self.agent.name,
                binding.chat_id
            ));
        }
    }
}

/// 回传线程的本体。生产是 `run_relay`,测试换成会出错或 panic 的替身。
type RelayFn = fn(&RemoteBinding, &RemoteStore, &OutboxStore, &AtomicBool) -> anyhow::Result<()>;

struct Worker {
    binding: RemoteBinding,
    store: RemoteStore,
    outbox: OutboxStore,
    io: Box<dyn WorkerIo>,
    relay: RelayFn,
}

struct WorkerHandle {
    tx: Sender<RemoteJob>,
    stop: Arc<AtomicBool>,
    /// 留着给下一次 `start` 等它退出;worker 退出前会先等自己的回传线程退出。
    thread: JoinHandle<()>,
}

type WorkerKey = (String, String);

static WORKERS: OnceLock<Mutex<HashMap<WorkerKey, WorkerHandle>>> = OnceLock::new();
/// 换线程(叫停旧的、等它退出、起新的)整段串行:回传的「查重 + 入队」不是原子的,同一个群
/// 不能有两个回传线程同时读同一份 offset。
static RESTARTS: Mutex<()> = Mutex::new(());
static SHUTDOWN: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn workers() -> MutexGuard<'static, HashMap<WorkerKey, WorkerHandle>> {
    WORKERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// 回传从哪里读起。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayFrom {
    /// 落盘 offset:绑定时写下的转录末尾,或启动复验通过后上一代推进到的位置。
    SavedOffset,
    /// 此刻的转录末尾:按需拉起前没有经过复验的那段一律不补发。
    CurrentEnd,
}

fn live_worker(agent: &Agent, fc: Arc<FeishuClient>, binding: RemoteBinding) -> Worker {
    Worker {
        store: RemoteStore::for_agent(agent),
        outbox: OutboxStore::at_run_dir(),
        io: Box::new(LiveIo {
            agent: agent.clone(),
            fc,
        }),
        relay: run_relay,
        binding,
    }
}

fn shutdown_flag(agent_name: &str, chat_id: &str) -> Arc<AtomicBool> {
    // daemon 启动时 `boot` 先于入站恢复、dispatcher 与卡片回放登记停机位,其余调用方都在那之后。
    match SHUTDOWN.get() {
        Some(shutdown) => Arc::clone(shutdown),
        None => {
            log(&summon_i18n::format!("r4df3075b37"));
            Arc::new(AtomicBool::new(false))
        }
    }
}

/// 为一个绑定起 worker 与回传线程,回传从落盘 offset 读起(绑定与启动恢复用);同一个群已有的
/// 那一对先叫停并等它们退出。盘上记录已不是这条未结束的绑定时什么都不做。返回是否起了 worker。
pub(crate) fn start(agent: &Agent, fc: Arc<FeishuClient>, binding: RemoteBinding) -> bool {
    let shutdown = shutdown_flag(&agent.name, &binding.chat_id);
    start_worker(
        &agent.name,
        live_worker(agent, fc, binding),
        shutdown,
        RelayFrom::SavedOffset,
    )
}

/// 群消息到来时按需拉起的结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OnDemandStart {
    Started,
    /// 复验判定会话已结束,结束记录已落盘;调用方告知并删记录。
    Ended(&'static str),
    /// 复验本身读不出:只拒这一条,不起 worker。
    Unverifiable,
    /// 盘上记录已变(结束、解除、换绑或读不出),或线程起不来;调用方按重读的记录回复。
    NotStarted,
}

/// 未结束的绑定没有在跑的 worker 时,由群消息按需拉起。先做与 worker 相同的身份复验:这段时间
/// 没有任何复验,转录里积累的轮不知道来自哪个进程,通过后回传从此刻的转录末尾读起、不补发。
pub(crate) fn start_on_demand(
    agent: &Agent,
    fc: Arc<FeishuClient>,
    binding: RemoteBinding,
) -> OnDemandStart {
    let shutdown = shutdown_flag(&agent.name, &binding.chat_id);
    start_on_demand_worker(&agent.name, live_worker(agent, fc, binding), shutdown)
}

fn start_on_demand_worker(
    agent_name: &str,
    worker: Worker,
    shutdown: Arc<AtomicBool>,
) -> OnDemandStart {
    let binding = &worker.binding;
    match worker.io.liveness(binding) {
        Err(_) => {
            log(&summon_i18n::format!("rc2bc5d6d91", binding.chat_id));
            OnDemandStart::Unverifiable
        }
        Ok(Liveness::Ended(reason)) => {
            log(&summon_i18n::format!(
                "re48b38370b",
                binding.chat_id,
                binding.session_id
            ));
            match worker.store.mark_ended_if_current(binding, reason) {
                Ok(true) => OnDemandStart::Ended(reason),
                Ok(false) => OnDemandStart::NotStarted,
                Err(error) => {
                    log(&summon_i18n::format!("r076794297a", binding.chat_id));
                    OnDemandStart::NotStarted
                }
            }
        }
        Ok(Liveness::Alive) => {
            if start_worker(agent_name, worker, shutdown, RelayFrom::CurrentEnd) {
                OnDemandStart::Started
            } else {
                OnDemandStart::NotStarted
            }
        }
    }
}

fn start_worker(
    agent_name: &str,
    worker: Worker,
    shutdown: Arc<AtomicBool>,
    from: RelayFrom,
) -> bool {
    let key = (agent_name.to_string(), worker.binding.chat_id.clone());
    let _restart = RESTARTS.lock().unwrap_or_else(PoisonError::into_inner);
    // 调用方手里的绑定可能已经过期(结束、解除或换绑):只按盘上仍有效的同一份记录起。过期的
    // 调用不碰正在跑的 worker,它可能属于换绑后的新记录。
    if !record_backs(&worker.binding, &worker.store) {
        return false;
    }
    // 先出表再 join:join 期间不能拿着表锁,否则其它群的 submit 跟着等。
    let previous = workers().remove(&key);
    if let Some(previous) = previous {
        previous.stop.store(true, Ordering::SeqCst);
        if previous.thread.join().is_err() {
            log(&summon_i18n::format!("rd4bb580893", key.1));
        }
        // 旧 worker 收尾时可能刚把这份记录标成结束或删掉。
        if !record_backs(&worker.binding, &worker.store) {
            return false;
        }
    }
    // 旧的回传线程已经退出,这里重写 offset 不会被它写回去。
    if from == RelayFrom::CurrentEnd && !relay_from_current_end(&worker.binding, &worker.store) {
        return false;
    }
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name(format!("remote-{}", key.1))
        .spawn(move || run_worker(worker, rx, &thread_stop, &shutdown));
    match spawned {
        Ok(thread) => {
            workers().insert(key, WorkerHandle { tx, stop, thread });
            true
        }
        Err(error) => {
            log(&summon_i18n::format!("r21ac082bd7", key.1));
            false
        }
    }
}

/// 把回传起点挪到此刻的转录末尾,跳过的字节区间记日志。读不到转录或写不进 offset 就不起。
fn relay_from_current_end(binding: &RemoteBinding, store: &RemoteStore) -> bool {
    let end = match std::fs::metadata(&binding.transcript_path) {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        other => {
            log(&summon_i18n::format!(
                "rd70d8c81d8",
                binding.agent,
                binding.chat_id,
                binding.transcript_path,
                other.err()
            ));
            return false;
        }
    };
    let saved = store.read_offset(&binding.chat_id);
    if let Err(error) = store.write_offset(&binding.chat_id, end) {
        log(&summon_i18n::format!(
            "r3f409a8b91",
            binding.agent,
            binding.chat_id
        ));
        return false;
    }
    let skipped = match saved {
        Ok(Some(saved)) => format!("{saved}..{end}"),
        Ok(None) => summon_i18n::format!("r0b4834fa91"),
        Err(error) => summon_i18n::format!("r7a929e9cea"),
    };
    log(&summon_i18n::format!(
        "r6cf4a8c7e2",
        binding.agent,
        binding.chat_id
    ));
    true
}

/// 盘上记录是否仍是这条未结束的绑定;不是就记日志。
fn record_backs(binding: &RemoteBinding, store: &RemoteStore) -> bool {
    let verdict = match store.load(&binding.chat_id) {
        Ok(Some(current)) if !same_binding(&current, binding) => summon_i18n::text!("r1c8ff73c6c"),
        Ok(Some(current)) if current.ended.is_some() => summon_i18n::text!("r6c0db65785"),
        Ok(Some(_)) => return true,
        Ok(None) => summon_i18n::text!("r7eb2a62d9b"),
        Err(error) => {
            log(&summon_i18n::format!(
                "rbd9e89daf7",
                binding.agent,
                binding.chat_id
            ));
            return false;
        }
    };
    log(&summon_i18n::format!(
        "rf4ec99b8d0",
        binding.agent,
        binding.chat_id
    ));
    false
}

/// 把一条群消息排进该群的 worker;没有在跑的 worker 返回 false。
pub(crate) fn submit(agent_name: &str, chat_id: &str, job: RemoteJob) -> bool {
    let table = workers();
    let Some(handle) = table.get(&(agent_name.to_string(), chat_id.to_string())) else {
        return false;
    };
    !handle.stop.load(Ordering::SeqCst) && handle.tx.send(job).is_ok()
}

/// 叫停该群的 worker 与回传线程,不等它们退出(调用方在 dispatcher 线程上)。句柄留在表里,
/// 下一次 `start` 同一个群时等它们退出。没有在跑的 worker 时什么都不做。
pub(crate) fn stop(agent_name: &str, chat_id: &str) {
    if let Some(handle) = workers().get(&(agent_name.to_string(), chat_id.to_string())) {
        handle.stop.store(true, Ordering::SeqCst);
    }
}

/// worker 线程本体。回传线程开在 scope 里:worker 退出前一定等它退出,会话结束时删 offset
/// 之后不会再有人把 offset 写回来。
fn run_worker(worker: Worker, rx: Receiver<RemoteJob>, stop: &AtomicBool, shutdown: &AtomicBool) {
    let ended = std::thread::scope(|scope| {
        // 回传线程只拿绑定、存储与 outbox,拿不到 worker 的 IO,结构上发不出群消息。
        let (relay, binding, store, outbox) =
            (worker.relay, &worker.binding, &worker.store, &worker.outbox);
        scope.spawn(move || relay_until_stopped(relay, binding, store, outbox, stop));
        // 不论 serve_jobs 怎么离开(含 panic)都叫停回传,scope 才等得到它退出。
        let _stop_relay = RaiseOnDrop(stop);
        serve_jobs(&worker, &rx, stop, shutdown)
    });
    // 叫停或结束时还排在队里的消息不再打进去;只记日志,不往群里发。
    for job in rx.try_iter() {
        log_unprocessed(&worker.binding, &job);
    }
    if ended {
        remove_ended_binding(&worker);
    }
}

fn log_unprocessed(binding: &RemoteBinding, job: &RemoteJob) {
    log(&summon_i18n::format!(
        "rd304340e6b",
        binding.agent,
        binding.chat_id,
        job.feishu_id().unwrap_or(&job.message_id)
    ));
}

/// 跑回传直到叫停。出错与 panic 一样按回传出错结束遥控:panic 若任它冒出去,worker 跟着
/// 退出,留下一个没结束、却没有回传的绑定。
fn relay_until_stopped(
    relay: RelayFn,
    binding: &RemoteBinding,
    store: &RemoteStore,
    outbox: &OutboxStore,
    stop: &AtomicBool,
) {
    let outcome =
        std::panic::catch_unwind(AssertUnwindSafe(|| relay(binding, store, outbox, stop)));
    let error = match outcome {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|text| text.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            anyhow::anyhow!(summon_i18n::format!("r310f890a3b"))
        }
    };
    end_after_relay_error(binding, store, stop, &error);
}

/// 回传出错后,这一代 daemon 里回复再也回不到群里,继续注入只会让人以为对面没在答。记为已结束
/// 并叫停 worker;不在这里发群消息,群里下一条消息会读到这条结束记录。先落盘再叫停:叫停之后
/// 群入站看到的必须已是结束记录,而不是一个还没结束、却没有 worker 的绑定。
fn end_after_relay_error(
    binding: &RemoteBinding,
    store: &RemoteStore,
    stop: &AtomicBool,
    error: &anyhow::Error,
) {
    log(&summon_i18n::format!(
        "r461eb79d64",
        binding.agent,
        binding.chat_id,
        binding.transcript_path
    ));
    // 盘上已解除或换绑的不动:结束到落盘之间同一个群可能已经重新绑定。
    if let Err(error) =
        store.mark_ended_if_current(binding, ::summon_core::i18n::label(RELAY_FAILED))
    {
        log(&summon_i18n::format!(
            "rb94dfca9cf",
            binding.agent,
            binding.chat_id
        ));
    }
    stop.store(true, Ordering::SeqCst);
}

/// 删掉这个 worker 自己那份已结束的绑定。结束到删除之间同一个群可能已经重新绑定,那份新记录
/// 与它的 offset 不能动。
fn remove_ended_binding(worker: &Worker) {
    let binding = &worker.binding;
    if let Err(error) = worker.store.remove_if_current(binding) {
        log(&summon_i18n::format!(
            "r0a66c6718f",
            binding.agent,
            binding.chat_id
        ));
    }
}

struct RaiseOnDrop<'a>(&'a AtomicBool);

impl Drop for RaiseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// 按到达顺序处理该群的消息,直到被叫停、daemon 停机或会话结束;返回 true 表示会话已结束、
/// 结束记录归这个 worker 删。
fn serve_jobs(
    worker: &Worker,
    rx: &Receiver<RemoteJob>,
    stop: &AtomicBool,
    shutdown: &AtomicBool,
) -> bool {
    let halted = || stop.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst);
    let binding = &worker.binding;
    let io = worker.io.as_ref();
    loop {
        let job = match rx.recv_timeout(RECV_TICK) {
            Ok(job) => Some(job),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return false,
        };
        // 排队期间被解除遥控或 daemon 停机,手上这条不再打进去。
        if halted() {
            if let Some(job) = &job {
                log_unprocessed(binding, job);
            }
            return false;
        }
        let Some(job) = job else {
            continue;
        };
        let feishu_id = job.feishu_id().map(str::to_string);
        let deps = JobDeps {
            liveness: &|binding| io.liveness(binding),
            deliver: &|lines| io.deliver(binding, lines),
            fetch_attachments: &|job| io.fetch_attachments(binding, job),
        };
        match handle_job(binding, job, &deps) {
            JobResult::Submitted => {
                if let Some(id) = feishu_id.as_deref() {
                    io.mark_delivered(id);
                }
            }
            JobResult::Refused(notice) => io.reply(binding, feishu_id.as_deref(), &notice),
            JobResult::Ended(reason) => {
                log(&summon_i18n::format!(
                    "r45a3d350d0",
                    binding.agent,
                    binding.chat_id,
                    binding.session_id
                ));
                // 先落「已结束」再告知:删记录之前崩掉,重启与群入站都按已结束处理,不会再往
                // 这个窗口打字。盘上已解除或换绑的,这份结束与那个群无关,不写、不回复。
                let owned = match worker.store.mark_ended_if_current(binding, reason) {
                    Ok(owned) => owned,
                    Err(error) => {
                        log(&summon_i18n::format!(
                            "re95000bfef",
                            binding.agent,
                            binding.chat_id
                        ));
                        false
                    }
                };
                stop.store(true, Ordering::SeqCst);
                if !owned {
                    log(&summon_i18n::format!(
                        "ra74a9632dc",
                        binding.agent,
                        binding.chat_id
                    ));
                    return false;
                }
                io.reply(binding, feishu_id.as_deref(), &ended_notice(reason));
                return true;
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum BootDecision {
    Start,
    /// 固定短语,同 `JobResult::Ended`。
    End(&'static str),
    /// 复验本身读不出:绑定原样保留,本次不起。
    Keep,
}

/// 启动时对一条未结束绑定的裁决。飞书身份换了的,回传事件带的是旧投递身份、永远投不出去,
/// 不论会话还在不在都结束。
fn boot_decision(
    binding: &RemoteBinding,
    current_scope: &str,
    liveness: &anyhow::Result<Liveness>,
) -> BootDecision {
    if binding.scope != current_scope {
        return BootDecision::End(::summon_core::i18n::label(SCOPE_CHANGED));
    }
    match liveness {
        Ok(Liveness::Alive) => BootDecision::Start,
        Ok(Liveness::Ended(reason)) => BootDecision::End(reason),
        Err(_) => BootDecision::Keep,
    }
}

/// 恢复一个 agent 名下的遥控:按 `boot_decision` 起 worker、记为已结束(等群里下一条消息时再
/// 告知)或原样保留。
fn restore_agent(
    agent_name: &str,
    current_scope: &str,
    store: &RemoteStore,
    load_sessions: &dyn Fn() -> anyhow::Result<CmuxHookSessions>,
    process_start: &dyn Fn(u32) -> anyhow::Result<Option<i64>>,
    start: &mut dyn FnMut(RemoteBinding),
) {
    let bindings = match store.list() {
        Ok(bindings) => bindings,
        Err(error) => {
            log(&summon_i18n::format!("ra857df6e29"));
            return;
        }
    };
    let live: Vec<RemoteBinding> = bindings
        .into_iter()
        .filter(|binding| binding.ended.is_none())
        .collect();
    if live.is_empty() {
        return;
    }
    let hooks = load_sessions();
    for binding in live {
        let liveness = match &hooks {
            Ok(hooks) => verify_identity(&binding, hooks, process_start),
            Err(error) => Err(anyhow::anyhow!(summon_i18n::format!("r97f5a4f041"))),
        };
        match boot_decision(&binding, current_scope, &liveness) {
            BootDecision::Start => {
                log(&summon_i18n::format!(
                    "r9cd10dc73a",
                    binding.chat_id,
                    binding.session_id
                ));
                start(binding);
            }
            BootDecision::End(reason) => {
                log(&summon_i18n::format!(
                    "r3bba15399d",
                    binding.chat_id,
                    binding.session_id
                ));
                if let Err(error) = store.mark_ended_if_current(&binding, reason) {
                    log(&summon_i18n::format!("r076794297a", binding.chat_id));
                }
                // 没通过复验的绑定不能有线程在替它打字或回传。
                stop(agent_name, &binding.chat_id);
            }
            BootDecision::Keep => {
                if let Err(error) = &liveness {
                    log(&summon_i18n::format!(
                        "r761c8f6e76",
                        binding.chat_id,
                        binding.pid
                    ));
                }
                stop(agent_name, &binding.chat_id);
            }
        }
    }
}

/// daemon 启动时恢复各 agent 的遥控。只记日志,不往任何群发消息:启动扫描一次覆盖全部绑定,
/// 判据一松就是多个群同时被打扰。
pub(crate) fn boot(drain: &DrainAgents, shutdown: &Arc<AtomicBool>) {
    let _ = SHUTDOWN.set(Arc::clone(shutdown));
    let agents: Vec<(Agent, Arc<FeishuClient>, String)> = drain
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .map(|drain| {
            (
                drain.agent.clone(),
                Arc::clone(&drain.client),
                drain.delivery_scope().to_string(),
            )
        })
        .collect();
    for (agent, fc, scope) in agents {
        restore_agent(
            &agent.name,
            &scope,
            &RemoteStore::for_agent(&agent),
            &load_cmux_sessions,
            &process_start_epoch_secs,
            &mut |binding| {
                start(&agent, Arc::clone(&fc), binding);
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;
    use std::time::Instant;
    use summon_core::remote::{CmuxClaudeSession, REMOTE_BINDING_SCHEMA_VERSION};

    fn binding_for(agent: &str, chat: &str, pid: u32, transcript: &str) -> RemoteBinding {
        RemoteBinding {
            schema_version: REMOTE_BINDING_SCHEMA_VERSION,
            agent: agent.into(),
            scope: "a-s".into(),
            chat_id: chat.into(),
            chat_type: "group".into(),
            session_id: format!("sid-{chat}"),
            session_name: "n".into(),
            surface_id: "S".into(),
            workspace_id: "W".into(),
            pid,
            pid_start_seconds: 1000,
            transcript_path: transcript.into(),
            actor_open_id: "ou".into(),
            created_at_ms: 1,
            ended: None,
        }
    }

    fn binding() -> RemoteBinding {
        binding_for("a", "oc_a", 42, "/t.jsonl")
    }

    /// 与绑定逐项一致的 cmux 会话记录。
    fn session_of(b: &RemoteBinding) -> CmuxClaudeSession {
        CmuxClaudeSession {
            session_id: b.session_id.clone(),
            surface_id: b.surface_id.clone(),
            workspace_id: b.workspace_id.clone(),
            pid: b.pid,
            pid_start_seconds: b.pid_start_seconds,
            transcript_path: b.transcript_path.clone(),
            cwd: "/".into(),
        }
    }

    fn hooks_of(sessions: Vec<CmuxClaudeSession>) -> CmuxHookSessions {
        CmuxHookSessions {
            sessions,
            ..CmuxHookSessions::default()
        }
    }

    fn job(lines: &[&str]) -> RemoteJob {
        RemoteJob {
            message_id: "om_1".into(),
            anchor: None,
            lines: lines.iter().map(|l| l.to_string()).collect(),
            resources: Vec::new(),
        }
    }

    #[test]
    fn ended_session_is_reported_and_nothing_is_typed() {
        let typed = Cell::new(false);
        let deps = JobDeps {
            liveness: &|_| Ok(Liveness::Ended("会话进程已经退出")),
            deliver: &|_| {
                typed.set(true);
                DeliverOutcome::Submitted
            },
            fetch_attachments: &|_| None,
        };
        let r = handle_job(&binding(), job(&["你好"]), &deps);
        assert_eq!(r, JobResult::Ended("会话进程已经退出"));
        assert!(!typed.get());
    }

    #[test]
    fn attachments_are_appended_as_lines() {
        let seen = RefCell::new(Vec::new());
        let deps = JobDeps {
            liveness: &|_| Ok(Liveness::Alive),
            deliver: &|lines| {
                *seen.borrow_mut() = lines.to_vec();
                DeliverOutcome::Submitted
            },
            fetch_attachments: &|_| {
                Some("(用户随这条消息发来 1 个附件)\n- [图片] /tmp/a.png".into())
            },
        };
        assert_eq!(
            handle_job(&binding(), job(&["看图"]), &deps),
            JobResult::Submitted
        );
        assert_eq!(
            *seen.borrow(),
            vec![
                "看图",
                "",
                "(用户随这条消息发来 1 个附件)",
                "- [图片] /tmp/a.png"
            ]
        );
    }

    #[test]
    fn attachment_only_message_types_just_the_note() {
        let seen = RefCell::new(Vec::new());
        let deps = JobDeps {
            liveness: &|_| Ok(Liveness::Alive),
            deliver: &|lines| {
                *seen.borrow_mut() = lines.to_vec();
                DeliverOutcome::Submitted
            },
            fetch_attachments: &|_| {
                Some("(用户随这条消息发来 1 个附件)\n- [图片] /tmp/a.png".into())
            },
        };
        assert_eq!(
            handle_job(&binding(), job(&[]), &deps),
            JobResult::Submitted
        );
        assert_eq!(
            *seen.borrow(),
            vec!["(用户随这条消息发来 1 个附件)", "- [图片] /tmp/a.png"],
            "没有正文时不留前导空行"
        );
    }

    #[test]
    fn attachment_only_message_whose_attachment_is_missing_is_not_typed() {
        let typed = Cell::new(false);
        let deps = JobDeps {
            liveness: &|_| Ok(Liveness::Alive),
            deliver: &|_| {
                typed.set(true);
                DeliverOutcome::Submitted
            },
            fetch_attachments: &|_| None,
        };
        assert_eq!(
            handle_job(&binding(), job(&[]), &deps),
            JobResult::Refused(::summon_core::i18n::label(ATTACHMENT_UNAVAILABLE).into()),
            "空输入框上按回车会被当成已提交"
        );
        assert!(!typed.get());
    }

    #[test]
    fn unverifiable_identity_refuses_this_message_only() {
        let typed = Cell::new(false);
        let fetched = Cell::new(false);
        let deps = JobDeps {
            liveness: &|_| Err(::summon_core::i18n::label(PROCESS_UNREADABLE).to_string()),
            deliver: &|_| {
                typed.set(true);
                DeliverOutcome::Submitted
            },
            fetch_attachments: &|_| {
                fetched.set(true);
                None
            },
        };
        assert_eq!(
            handle_job(&binding(), job(&["你好"]), &deps),
            JobResult::Refused(::summon_core::i18n::label(PROCESS_UNREADABLE).into()),
            "复验读不出只拒这一条,不能当成会话已结束"
        );
        assert!(!typed.get() && !fetched.get());
    }

    #[test]
    fn plain_message_is_typed_as_is_and_refusal_is_passed_through() {
        let seen = RefCell::new(Vec::new());
        let deps = JobDeps {
            liveness: &|_| Ok(Liveness::Alive),
            deliver: &|lines| {
                *seen.borrow_mut() = lines.to_vec();
                DeliverOutcome::Refused("输入框里有未发出的文字".into())
            },
            fetch_attachments: &|_| None,
        };
        assert_eq!(
            handle_job(&binding(), job(&["第一行", "第二行"]), &deps),
            JobResult::Refused("输入框里有未发出的文字".into())
        );
        assert_eq!(*seen.borrow(), vec!["第一行", "第二行"]);
    }

    #[test]
    fn attachment_note_with_backslash_is_not_typed() {
        let typed = Cell::new(false);
        let deps = JobDeps {
            liveness: &|_| Ok(Liveness::Alive),
            deliver: &|_| {
                typed.set(true);
                DeliverOutcome::Submitted
            },
            fetch_attachments: &|_| Some("- [文件] /tmp/a\\n.txt".into()),
        };
        assert!(matches!(
            handle_job(&binding(), job(&["看文件"]), &deps),
            JobResult::Refused(ref t) if t.contains("反斜杠")
        ));
        assert!(!typed.get());
    }

    #[test]
    fn unreadable_process_state_refuses_without_ending() {
        let b = binding();
        assert_eq!(
            job_liveness(&b, &|| Ok(hooks_of(vec![session_of(&b)])), &|_| Err(
                anyhow!("ps 超时")
            )),
            Err(::summon_core::i18n::label(PROCESS_UNREADABLE).to_string())
        );
    }

    #[test]
    fn unreadable_cmux_records_refuse_without_ending() {
        let b = binding();
        let probed = Cell::new(false);
        assert_eq!(
            job_liveness(&b, &|| Err(anyhow!("坏 JSON")), &|_| {
                probed.set(true);
                Ok(Some(1000))
            }),
            Err(::summon_core::i18n::label(SESSIONS_UNREADABLE).to_string())
        );
        assert!(!probed.get());
    }

    #[test]
    fn job_liveness_ends_only_on_proven_mismatch() {
        let b = binding();
        let sessions = || Ok(hooks_of(vec![session_of(&b)]));
        assert_eq!(
            job_liveness(&b, &sessions, &|_| Ok(Some(1000))),
            Ok(Liveness::Alive)
        );
        assert_eq!(
            job_liveness(&b, &sessions, &|_| Ok(None)),
            Ok(Liveness::Ended("会话进程已经退出"))
        );
        assert_eq!(
            job_liveness(&b, &sessions, &|_| Ok(Some(2000))),
            Ok(Liveness::Ended("进程号已被别的进程复用"))
        );
        assert!(matches!(
            job_liveness(&b, &|| Ok(CmuxHookSessions::default()), &|_| Ok(Some(1000))),
            Ok(Liveness::Ended(_))
        ));
    }

    #[test]
    fn ended_notice_points_back_to_bridge() {
        assert_eq!(
            ended_notice("会话进程已经退出"),
            "遥控已结束:会话进程已经退出。要继续请重新 `/bridge`。"
        );
    }

    #[test]
    fn boot_restores_verified_bindings_and_marks_the_rest_ended() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        let alive = binding_for("a", "oc_alive", 1, "/t1.jsonl");
        let dead = binding_for("a", "oc_dead", 2, "/t2.jsonl");
        let unprobed = binding_for("a", "oc_ps", 3, "/t3.jsonl");
        let old = RemoteBinding {
            ended: Some("早先结束".into()),
            ..binding_for("a", "oc_old", 4, "/t4.jsonl")
        };
        let other_app = RemoteBinding {
            scope: "a-old-app".into(),
            ..binding_for("a", "oc_app", 5, "/t5.jsonl")
        };
        for b in [&alive, &dead, &unprobed, &old, &other_app] {
            store.save(b).unwrap();
        }
        let sessions: Vec<_> = [&alive, &dead, &unprobed, &old, &other_app]
            .into_iter()
            .map(session_of)
            .collect();
        let mut started = Vec::new();
        restore_agent(
            "a",
            "a-s",
            &store,
            &|| Ok(hooks_of(sessions.clone())),
            &|pid| match pid {
                1 => Ok(Some(1000)),
                2 => Ok(None),
                3 => Err(anyhow!("ps 失败")),
                5 => Ok(Some(1000)),
                _ => panic!("已结束的绑定不该再复验"),
            },
            &mut |b| started.push(b.chat_id),
        );
        assert_eq!(started, vec!["oc_alive".to_string()]);
        assert_eq!(
            store.load("oc_dead").unwrap().unwrap().ended.as_deref(),
            Some("会话进程已经退出")
        );
        assert_eq!(
            store.load("oc_ps").unwrap().unwrap().ended,
            None,
            "ps 读不出不能当成会话已结束"
        );
        assert_eq!(store.load("oc_old").unwrap(), Some(old));
        assert_eq!(store.load("oc_alive").unwrap(), Some(alive));
        assert_eq!(
            store.load("oc_app").unwrap().unwrap().ended.as_deref(),
            Some("飞书身份已变化"),
            "会话还活着,但飞书身份换了也要结束"
        );
    }

    #[test]
    fn boot_decision_covers_scope_and_liveness() {
        let b = binding();
        assert_eq!(
            boot_decision(&b, "a-s", &Ok(Liveness::Alive)),
            BootDecision::Start
        );
        assert_eq!(
            boot_decision(&b, "a-s", &Ok(Liveness::Ended("会话进程已经退出"))),
            BootDecision::End("会话进程已经退出")
        );
        assert_eq!(
            boot_decision(&b, "a-s", &Err(anyhow!("ps 失败"))),
            BootDecision::Keep
        );
        for liveness in [
            Ok(Liveness::Alive),
            Ok(Liveness::Ended("会话进程已经退出")),
            Err(anyhow!("ps 失败")),
        ] {
            assert_eq!(
                boot_decision(&b, "a-new-app", &liveness),
                BootDecision::End("飞书身份已变化")
            );
        }
    }

    #[test]
    fn boot_ends_other_app_bindings_even_when_cmux_records_are_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        store
            .save(&binding_for("a", "oc_a", 1, "/t.jsonl"))
            .unwrap();
        let mut started = 0;
        restore_agent(
            "a",
            "a-new-app",
            &store,
            &|| Err(anyhow!("坏 JSON")),
            &|_| panic!("读不出 cmux 记录时不该再 ps"),
            &mut |_| started += 1,
        );
        assert_eq!(started, 0);
        assert_eq!(
            store.load("oc_a").unwrap().unwrap().ended.as_deref(),
            Some("飞书身份已变化")
        );
    }

    #[test]
    fn boot_skips_an_agent_whose_records_are_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote");
        let store = RemoteStore::new(root.clone());
        store
            .save(&binding_for("a", "oc_good", 1, "/t.jsonl"))
            .unwrap();
        std::fs::write(root.join("oc_bad.json"), "{").unwrap();
        let loaded = Cell::new(false);
        let mut started = 0;
        restore_agent(
            "a",
            "a-s",
            &store,
            &|| {
                loaded.set(true);
                Ok(CmuxHookSessions::default())
            },
            &|_| Ok(Some(1000)),
            &mut |_| started += 1,
        );
        assert_eq!(started, 0);
        assert!(!loaded.get());
    }

    #[test]
    fn boot_keeps_bindings_when_cmux_records_are_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        let b = binding_for("a", "oc_a", 1, "/t.jsonl");
        store.save(&b).unwrap();
        let mut started = 0;
        restore_agent(
            "a",
            "a-s",
            &store,
            &|| Err(anyhow!("坏 JSON")),
            &|_| Ok(Some(1000)),
            &mut |_| started += 1,
        );
        assert_eq!(started, 0);
        assert_eq!(store.load("oc_a").unwrap(), Some(b));
    }

    /// 截出 `src` 里从 `signature` 起到函数结束(首个顶格 `}`)的源码。
    fn fn_source<'a>(src: &'a str, signature: &str) -> &'a str {
        let start = src.find(signature).expect(signature);
        let body = &src[start..];
        &body[..body.find("\n}\n").expect("函数结尾")]
    }

    #[test]
    fn boot_never_messages_groups() {
        let src = include_str!("runtime.rs");
        for signature in ["pub(crate) fn boot(", "fn restore_agent("] {
            let body = fn_source(src, signature);
            for banned in ["send_markdown", "react(", "reply(", "mark_delivered("] {
                assert!(
                    !body.contains(banned),
                    "{signature} 不得往群里发东西,发现 {banned}"
                );
            }
        }
    }

    // ---- worker 线程 ----

    /// (锚点, 文案, 发这句时盘上记录的 ended)
    type Reply = (Option<String>, String, Option<String>);

    #[derive(Default)]
    struct FakeState {
        ended: Mutex<Option<&'static str>>,
        /// 置上后 deliver 阻塞,直到收到一条放行信号。
        gate: Mutex<Option<Receiver<()>>>,
        delivering: AtomicBool,
        /// 发结束通知的同时有人把同一个群重新绑定。
        rebind_on_reply: Mutex<Option<RemoteBinding>>,
        delivered: Mutex<Vec<Vec<String>>>,
        reacted: Mutex<Vec<String>>,
        replies: Mutex<Vec<Reply>>,
        /// 置上后复验读不出(ps 失败一类)。
        unverifiable: AtomicBool,
        /// 置上后每次复验时记下这个 outbox 里已有几条待投递事件。
        outbox_root: Mutex<Option<PathBuf>>,
        pending_at_liveness: Mutex<Vec<usize>>,
    }

    struct FakeIo {
        state: Arc<FakeState>,
        store_root: PathBuf,
    }

    impl WorkerIo for FakeIo {
        fn liveness(&self, _: &RemoteBinding) -> Result<Liveness, String> {
            if let Some(root) = self.state.outbox_root.lock().unwrap().as_ref() {
                let pending = OutboxStore::new(root.clone())
                    .scan_pending()
                    .unwrap()
                    .events
                    .len();
                self.state.pending_at_liveness.lock().unwrap().push(pending);
            }
            if self.state.unverifiable.load(Ordering::SeqCst) {
                return Err(::summon_core::i18n::label(PROCESS_UNREADABLE).to_string());
            }
            Ok(match *self.state.ended.lock().unwrap() {
                Some(reason) => Liveness::Ended(reason),
                None => Liveness::Alive,
            })
        }
        fn deliver(&self, _: &RemoteBinding, lines: &[String]) -> DeliverOutcome {
            self.state.delivering.store(true, Ordering::SeqCst);
            if let Some(gate) = self.state.gate.lock().unwrap().as_ref() {
                gate.recv_timeout(Duration::from_secs(20)).unwrap();
            }
            self.state.delivered.lock().unwrap().push(lines.to_vec());
            DeliverOutcome::Submitted
        }
        fn fetch_attachments(&self, _: &RemoteBinding, _: &RemoteJob) -> Option<String> {
            None
        }
        fn mark_delivered(&self, message_id: &str) {
            self.state.reacted.lock().unwrap().push(message_id.into());
        }
        fn reply(&self, binding: &RemoteBinding, anchor: Option<&str>, text: &str) {
            let store = RemoteStore::new(self.store_root.clone());
            let on_disk = store.load(&binding.chat_id).unwrap().and_then(|b| b.ended);
            if let Some(rebound) = self.state.rebind_on_reply.lock().unwrap().take() {
                store.save(&rebound).unwrap();
                store.write_offset(&rebound.chat_id, 7).unwrap();
            }
            self.state.replies.lock().unwrap().push((
                anchor.map(str::to_string),
                text.to_string(),
                on_disk,
            ));
        }
    }

    /// tempdir 里的一套绑定:记录、offset、空转录都在,回传线程能正常跑起来。
    struct Rig {
        dir: tempfile::TempDir,
        binding: RemoteBinding,
    }

    impl Rig {
        fn new(agent: &str) -> Self {
            let rig = Self::without_offset(agent);
            rig.store().write_offset("oc_a", 0).unwrap();
            rig
        }

        /// 缺 offset 的绑定:回传线程一起来就报错。
        fn without_offset(agent: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let transcript = dir.path().join("t.jsonl");
            std::fs::write(&transcript, "").unwrap();
            let binding = binding_for(agent, "oc_a", 42, &transcript.display().to_string());
            let rig = Self { dir, binding };
            rig.store().save(&rig.binding).unwrap();
            rig
        }

        fn store_root(&self) -> PathBuf {
            self.dir.path().join("remote")
        }

        fn store(&self) -> RemoteStore {
            RemoteStore::new(self.store_root())
        }

        fn outbox_root(&self) -> PathBuf {
            self.dir.path().join("outbox")
        }

        fn transcript(&self) -> PathBuf {
            PathBuf::from(&self.binding.transcript_path)
        }

        fn worker(&self, state: &Arc<FakeState>) -> Worker {
            Worker {
                binding: self.binding.clone(),
                store: self.store(),
                outbox: OutboxStore::new(self.outbox_root()),
                io: Box::new(FakeIo {
                    state: Arc::clone(state),
                    store_root: self.store_root(),
                }),
                relay: |binding, store, outbox, stop| {
                    super::super::relay::relay_transcript(
                        binding,
                        store,
                        outbox,
                        stop,
                        Duration::from_millis(5),
                        &mut || {},
                    )
                },
            }
        }
    }

    fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !cond() {
            assert!(Instant::now() < deadline, "等不到:{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 该群的 worker 线程是否已自行退出(没被任何人 join)。
    fn worker_finished(agent: &str, chat: &str) -> bool {
        workers()
            .get(&(agent.to_string(), chat.to_string()))
            .is_some_and(|handle| handle.thread.is_finished())
    }

    /// 等该群的 worker 线程(连同它的回传线程)真正退出。
    fn join_worker(agent: &str, chat: &str) {
        let handle = workers()
            .remove(&(agent.to_string(), chat.to_string()))
            .expect("worker 句柄");
        handle.stop.store(true, Ordering::SeqCst);
        handle.thread.join().unwrap();
    }

    fn anchored(lines: &[&str], anchor: &str) -> RemoteJob {
        RemoteJob {
            anchor: Some(anchor.into()),
            ..job(lines)
        }
    }

    #[test]
    fn submit_without_worker_returns_false() {
        assert!(!submit("t-none", "oc_a", job(&["x"])));
    }

    #[test]
    fn submitted_message_gets_the_delivered_reaction_and_stop_closes_the_queue() {
        let rig = Rig::new("t-submit");
        let state = Arc::new(FakeState::default());
        start_saved(
            "t-submit",
            rig.worker(&state),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(submit("t-submit", "oc_a", anchored(&["你好"], "om_real")));
        wait_until("贴已送达表情", || {
            !state.reacted.lock().unwrap().is_empty()
        });
        assert_eq!(*state.reacted.lock().unwrap(), vec!["om_real".to_string()]);
        assert_eq!(
            *state.delivered.lock().unwrap(),
            vec![vec!["你好".to_string()]]
        );
        assert!(state.replies.lock().unwrap().is_empty());

        stop("t-submit", "oc_a");
        assert!(!submit("t-submit", "oc_a", job(&["再来"])));
        assert_eq!(
            rig.store().load("oc_a").unwrap(),
            Some(rig.binding.clone()),
            "解除遥控的记录由调用方删,stop 只停线程"
        );
    }

    #[test]
    fn ended_session_is_recorded_announced_then_unbound() {
        let rig = Rig::new("t-ended");
        let state = Arc::new(FakeState::default());
        *state.ended.lock().unwrap() = Some("会话进程已经退出");
        start_saved(
            "t-ended",
            rig.worker(&state),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(submit("t-ended", "oc_a", job(&["你好"])));
        wait_until("记录与 offset 删除", || {
            rig.store().load("oc_a").unwrap().is_none()
                && rig.store().read_offset("oc_a").unwrap().is_none()
        });
        assert_eq!(
            *state.replies.lock().unwrap(),
            vec![(
                Some("om_1".to_string()),
                ended_notice("会话进程已经退出"),
                Some("会话进程已经退出".to_string()),
            )],
            "先落已结束再告知,锚在触发的那条消息上"
        );
        assert!(state.delivered.lock().unwrap().is_empty());
        wait_until("worker 退出后不再收消息", || {
            !submit("t-ended", "oc_a", job(&["再来"]))
        });
    }

    #[test]
    fn ended_worker_leaves_a_rebound_record_alone() {
        let rig = Rig::new("t-rebound");
        let state = Arc::new(FakeState::default());
        *state.ended.lock().unwrap() = Some("会话进程已经退出");
        let rebound = RemoteBinding {
            created_at_ms: 2,
            session_id: "sid-new".into(),
            ..rig.binding.clone()
        };
        *state.rebind_on_reply.lock().unwrap() = Some(rebound.clone());
        start_saved(
            "t-rebound",
            rig.worker(&state),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(submit("t-rebound", "oc_a", job(&["你好"])));
        wait_until("发出结束通知", || {
            !state.replies.lock().unwrap().is_empty()
        });
        join_worker("t-rebound", "oc_a");
        assert_eq!(rig.store().load("oc_a").unwrap(), Some(rebound));
        assert_eq!(rig.store().read_offset("oc_a").unwrap(), Some(7));
    }

    #[test]
    fn relay_error_ends_the_binding_without_messaging_the_group() {
        let rig = Rig::without_offset("t-relay-err");
        let state = Arc::new(FakeState::default());
        start_saved(
            "t-relay-err",
            rig.worker(&state),
            Arc::new(AtomicBool::new(false)),
        );
        wait_until("worker 自行退出", || {
            worker_finished("t-relay-err", "oc_a")
        });
        assert_eq!(
            rig.store().load("oc_a").unwrap().unwrap().ended.as_deref(),
            Some("回复回传出错"),
            "结束原因会发进群,只能是固定短语,错误链(这里是缺 offset)只进日志"
        );
        assert!(!submit("t-relay-err", "oc_a", job(&["x"])));
        assert!(state.replies.lock().unwrap().is_empty());
        assert!(state.reacted.lock().unwrap().is_empty());
        assert!(state.delivered.lock().unwrap().is_empty());
    }

    #[test]
    fn relay_error_leaves_a_rebound_record_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        let old = binding_for("a", "oc_a", 1, "/t.jsonl");
        let rebound = RemoteBinding {
            created_at_ms: 2,
            session_id: "sid-new".into(),
            ..old.clone()
        };
        store.save(&rebound).unwrap();
        let stop = AtomicBool::new(false);
        end_after_relay_error(&old, &store, &stop, &anyhow!("读转录出错"));
        assert_eq!(store.load("oc_a").unwrap(), Some(rebound));
        assert!(
            stop.load(Ordering::SeqCst),
            "自己的回传出错,worker 照样叫停"
        );
    }

    /// 绑定与启动恢复的起法:回传从落盘 offset 读起。
    fn start_saved(agent: &str, worker: Worker, shutdown: Arc<AtomicBool>) -> bool {
        start_worker(agent, worker, shutdown, RelayFrom::SavedOffset)
    }

    fn running() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn start_refuses_when_the_record_is_missing_or_ended() {
        let state = Arc::new(FakeState::default());
        let missing = Rig::new("t-refuse-missing");
        missing.store().remove("oc_a").unwrap();
        assert!(!start_saved(
            "t-refuse-missing",
            missing.worker(&state),
            running()
        ));
        assert!(!submit("t-refuse-missing", "oc_a", job(&["x"])));

        let ended = Rig::new("t-refuse-ended");
        ended
            .store()
            .save(&RemoteBinding {
                ended: Some("会话进程已经退出".into()),
                ..ended.binding.clone()
            })
            .unwrap();
        assert!(!start_saved(
            "t-refuse-ended",
            ended.worker(&state),
            running()
        ));
        assert!(!submit("t-refuse-ended", "oc_a", job(&["x"])));
        assert!(state.delivered.lock().unwrap().is_empty());
    }

    #[test]
    fn stale_start_leaves_the_rebound_worker_running() {
        let rig = Rig::new("t-refuse-stale");
        let rebound = RemoteBinding {
            created_at_ms: 2,
            session_id: "sid-new".into(),
            ..rig.binding.clone()
        };
        rig.store().save(&rebound).unwrap();
        let current = Arc::new(FakeState::default());
        let mut worker = rig.worker(&current);
        worker.binding = rebound;
        assert!(start_saved("t-refuse-stale", worker, running()));

        let stale = Arc::new(FakeState::default());
        assert!(
            !start_saved("t-refuse-stale", rig.worker(&stale), running()),
            "换绑之后,旧绑定的 start 不能起"
        );
        assert!(submit("t-refuse-stale", "oc_a", job(&["你好"])));
        wait_until("换绑后的 worker 仍在处理", || {
            !current.delivered.lock().unwrap().is_empty()
        });
        assert!(stale.delivered.lock().unwrap().is_empty());
        join_worker("t-refuse-stale", "oc_a");
    }

    #[test]
    fn boot_end_stops_a_worker_already_running_for_that_chat() {
        let rig = Rig::new("t-boot-end");
        let state = Arc::new(FakeState::default());
        assert!(start_saved("t-boot-end", rig.worker(&state), running()));
        let b = rig.binding.clone();
        restore_agent(
            "t-boot-end",
            "a-new-app",
            &rig.store(),
            &|| Ok(hooks_of(vec![session_of(&b)])),
            &|_| Ok(Some(1000)),
            &mut |_| panic!("身份已变,不该起"),
        );
        wait_until("已在跑的 worker 被叫停", || {
            worker_finished("t-boot-end", "oc_a")
        });
        assert_eq!(
            rig.store().load("oc_a").unwrap().unwrap().ended.as_deref(),
            Some("飞书身份已变化")
        );
        assert!(state.replies.lock().unwrap().is_empty());
    }

    #[test]
    fn boot_keep_stops_a_worker_already_running_for_that_chat() {
        let rig = Rig::new("t-boot-keep");
        let state = Arc::new(FakeState::default());
        assert!(start_saved("t-boot-keep", rig.worker(&state), running()));
        restore_agent(
            "t-boot-keep",
            "a-s",
            &rig.store(),
            &|| Err(anyhow!("坏 JSON")),
            &|_| Ok(Some(1000)),
            &mut |_| panic!("复验读不出,不该起"),
        );
        wait_until("已在跑的 worker 被叫停", || {
            worker_finished("t-boot-keep", "oc_a")
        });
        assert_eq!(rig.store().load("oc_a").unwrap(), Some(rig.binding.clone()));
        assert!(state.replies.lock().unwrap().is_empty());
    }

    #[test]
    fn restart_waits_for_the_previous_worker_to_finish() {
        let rig = Rig::new("t-restart");
        let first = Arc::new(FakeState::default());
        let (release, gate) = mpsc::channel();
        *first.gate.lock().unwrap() = Some(gate);
        start_saved(
            "t-restart",
            rig.worker(&first),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(submit("t-restart", "oc_a", job(&["慢的那条"])));
        wait_until("旧 worker 开始打字", || {
            first.delivering.load(Ordering::SeqCst)
        });

        let old_stop = Arc::clone(
            &workers()
                .get(&("t-restart".into(), "oc_a".into()))
                .unwrap()
                .stop,
        );
        let second = Arc::new(FakeState::default());
        let restarted = Arc::new(AtomicBool::new(false));
        let restarter = {
            let (worker, restarted) = (rig.worker(&second), Arc::clone(&restarted));
            std::thread::spawn(move || {
                start_saved("t-restart", worker, Arc::new(AtomicBool::new(false)));
                restarted.store(true, Ordering::SeqCst);
            })
        };
        wait_until("重启已叫停旧 worker", || {
            old_stop.load(Ordering::SeqCst)
        });
        assert!(
            !restarted.load(Ordering::SeqCst),
            "旧 worker 还在处理消息时不能起新的"
        );
        release.send(()).unwrap();
        restarter.join().unwrap();
        assert_eq!(
            *first.reacted.lock().unwrap(),
            vec!["om_1".to_string()],
            "旧 worker 做完手上那条才退出"
        );

        assert!(submit("t-restart", "oc_a", job(&["新的"])));
        wait_until("新 worker 接手", || {
            !second.delivered.lock().unwrap().is_empty()
        });
        assert_eq!(first.delivered.lock().unwrap().len(), 1);
        stop("t-restart", "oc_a");
    }

    #[test]
    fn daemon_shutdown_stops_the_worker_without_typing() {
        let rig = Rig::new("t-shutdown");
        let state = Arc::new(FakeState::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        start_saved("t-shutdown", rig.worker(&state), Arc::clone(&shutdown));
        shutdown.store(true, Ordering::SeqCst);
        wait_until("停机后 worker 退出", || {
            !submit("t-shutdown", "oc_a", job(&["x"]))
        });
        assert!(state.delivered.lock().unwrap().is_empty());
        assert!(state.replies.lock().unwrap().is_empty());
        assert!(
            rig.store().load("oc_a").unwrap().is_some(),
            "停机不解除遥控"
        );
    }

    #[test]
    fn a_message_queued_behind_a_busy_one_is_not_typed_after_stop() {
        let rig = Rig::new("t-queued-stop");
        let state = Arc::new(FakeState::default());
        let (release, gate) = mpsc::channel();
        *state.gate.lock().unwrap() = Some(gate);
        assert!(start_saved("t-queued-stop", rig.worker(&state), running()));
        assert!(submit(
            "t-queued-stop",
            "oc_a",
            anchored(&["第一条"], "om_first")
        ));
        wait_until("第一条开始打字", || {
            state.delivering.load(Ordering::SeqCst)
        });
        assert!(submit(
            "t-queued-stop",
            "oc_a",
            anchored(&["第二条"], "om_second")
        ));
        stop("t-queued-stop", "oc_a");
        // 两份放行:第二条若被处理,也不会卡在替身上,而是真的记成已打字。
        release.send(()).unwrap();
        release.send(()).unwrap();
        wait_until("worker 退出或第二条被打进去", || {
            worker_finished("t-queued-stop", "oc_a") || state.delivered.lock().unwrap().len() > 1
        });
        assert_eq!(
            *state.delivered.lock().unwrap(),
            vec![vec!["第一条".to_string()]],
            "叫停之后,排在队里的第二条不能再打进去"
        );
        assert_eq!(*state.reacted.lock().unwrap(), vec!["om_first".to_string()]);
        assert!(
            state.replies.lock().unwrap().is_empty(),
            "没打进去的那条只记日志,不往群里发"
        );
    }

    #[test]
    fn ended_job_leaves_a_record_rebound_before_it_alone() {
        let rig = Rig::new("t-end-rebound");
        let state = Arc::new(FakeState::default());
        *state.ended.lock().unwrap() = Some("会话进程已经退出");
        assert!(start_saved("t-end-rebound", rig.worker(&state), running()));
        let rebound = RemoteBinding {
            created_at_ms: 2,
            session_id: "sid-new".into(),
            ..rig.binding.clone()
        };
        rig.store().save(&rebound).unwrap();
        rig.store().write_offset("oc_a", 7).unwrap();
        assert!(submit("t-end-rebound", "oc_a", job(&["你好"])));
        wait_until("worker 退出", || worker_finished("t-end-rebound", "oc_a"));
        assert_eq!(
            rig.store().load("oc_a").unwrap(),
            Some(rebound),
            "换绑后的记录不能被旧绑定的结束原因压掉"
        );
        assert_eq!(rig.store().read_offset("oc_a").unwrap(), Some(7));
        assert!(
            state.replies.lock().unwrap().is_empty(),
            "不属于它的结束不回复"
        );
        assert!(state.delivered.lock().unwrap().is_empty());
    }

    #[test]
    fn ended_job_does_not_write_back_an_unbound_record() {
        let rig = Rig::new("t-end-unbound");
        let state = Arc::new(FakeState::default());
        *state.ended.lock().unwrap() = Some("会话进程已经退出");
        assert!(start_saved("t-end-unbound", rig.worker(&state), running()));
        rig.store().remove("oc_a").unwrap();
        assert!(submit("t-end-unbound", "oc_a", job(&["你好"])));
        wait_until("worker 退出", || worker_finished("t-end-unbound", "oc_a"));
        assert_eq!(
            rig.store().load("oc_a").unwrap(),
            None,
            "已解除的绑定不能被结束记录写回来"
        );
        assert!(state.replies.lock().unwrap().is_empty());
    }

    fn panicking_relay(
        _: &RemoteBinding,
        _: &RemoteStore,
        _: &OutboxStore,
        _: &AtomicBool,
    ) -> anyhow::Result<()> {
        panic!("回传替身 panic")
    }

    #[test]
    fn relay_panic_ends_the_binding_like_a_relay_error() {
        let rig = Rig::new("t-relay-panic");
        let state = Arc::new(FakeState::default());
        let mut worker = rig.worker(&state);
        worker.relay = panicking_relay;
        assert!(start_saved("t-relay-panic", worker, running()));
        wait_until("worker 自行退出", || {
            worker_finished("t-relay-panic", "oc_a")
        });
        assert_eq!(
            rig.store().load("oc_a").unwrap().unwrap().ended.as_deref(),
            Some("回复回传出错")
        );
        assert!(!submit("t-relay-panic", "oc_a", job(&["x"])));
        assert!(state.replies.lock().unwrap().is_empty());
        assert!(state.reacted.lock().unwrap().is_empty());
        assert!(state.delivered.lock().unwrap().is_empty());
        join_worker("t-relay-panic", "oc_a");
    }

    fn assistant_end_turn(text: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": "s",
            "message": {"role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": text}]}
        })
        .to_string()
            + "\n"
    }

    fn append(path: &std::path::Path, line: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
    }

    fn pending_texts(outbox: &OutboxStore) -> Vec<String> {
        let mut texts: Vec<String> = outbox
            .scan_pending()
            .unwrap()
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                summon_core::outbox::OutboxKind::TurnReply { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        texts.sort();
        texts
    }

    /// 绑定之后转录里积累了两轮,期间没有任何复验(启动复验读不出而没起 worker)。
    fn rig_with_unverified_turns(agent: &str) -> (Rig, Arc<FakeState>) {
        let rig = Rig::new(agent);
        append(&rig.transcript(), &assistant_end_turn("复验之前第一轮"));
        append(&rig.transcript(), &assistant_end_turn("复验之前第二轮"));
        let state = Arc::new(FakeState::default());
        *state.outbox_root.lock().unwrap() = Some(rig.outbox_root());
        (rig, state)
    }

    #[test]
    fn on_demand_start_verifies_first_and_relays_only_turns_after_the_current_end() {
        let (rig, state) = rig_with_unverified_turns("t-on-demand");
        let end = std::fs::metadata(rig.transcript()).unwrap().len();
        let outbox = OutboxStore::new(rig.outbox_root());
        assert_eq!(
            start_on_demand_worker("t-on-demand", rig.worker(&state), running()),
            OnDemandStart::Started
        );
        assert_eq!(
            *state.pending_at_liveness.lock().unwrap(),
            vec![0],
            "复验通过之前 outbox 里没有任何回传"
        );
        assert_eq!(
            rig.store().read_offset("oc_a").unwrap(),
            Some(end),
            "回传起点是复验通过时的转录末尾"
        );
        append(&rig.transcript(), &assistant_end_turn("复验之后一轮"));
        let deadline = Instant::now() + Duration::from_secs(15);
        while pending_texts(&outbox).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        // offset 在本轮入队后提交，读到末尾就覆盖了此前所有候选回复。
        wait_until("回传已读到转录末尾", || {
            rig.store().read_offset("oc_a").unwrap()
                == Some(std::fs::metadata(rig.transcript()).unwrap().len())
        });
        join_worker("t-on-demand", "oc_a");
        assert_eq!(
            pending_texts(&outbox),
            vec!["复验之后一轮".to_string()],
            "未经复验期间积累的轮一律不补发"
        );
    }

    #[test]
    fn on_demand_start_with_unreadable_identity_starts_nothing() {
        let (rig, state) = rig_with_unverified_turns("t-on-demand-unreadable");
        state.unverifiable.store(true, Ordering::SeqCst);
        assert_eq!(
            start_on_demand_worker("t-on-demand-unreadable", rig.worker(&state), running()),
            OnDemandStart::Unverifiable
        );
        assert!(!submit("t-on-demand-unreadable", "oc_a", job(&["x"])));
        assert_eq!(rig.store().read_offset("oc_a").unwrap(), Some(0));
        assert_eq!(rig.store().load("oc_a").unwrap(), Some(rig.binding.clone()));
        assert!(pending_texts(&OutboxStore::new(rig.outbox_root())).is_empty());
    }

    #[test]
    fn on_demand_start_of_an_ended_session_records_the_end_and_starts_nothing() {
        let (rig, state) = rig_with_unverified_turns("t-on-demand-ended");
        *state.ended.lock().unwrap() = Some("会话进程已经退出");
        assert_eq!(
            start_on_demand_worker("t-on-demand-ended", rig.worker(&state), running()),
            OnDemandStart::Ended("会话进程已经退出")
        );
        assert!(!submit("t-on-demand-ended", "oc_a", job(&["x"])));
        assert_eq!(
            rig.store().load("oc_a").unwrap().unwrap().ended.as_deref(),
            Some("会话进程已经退出")
        );
        assert!(
            state.replies.lock().unwrap().is_empty(),
            "告知与删记录由调用方做"
        );
        assert!(pending_texts(&OutboxStore::new(rig.outbox_root())).is_empty());
    }
}
