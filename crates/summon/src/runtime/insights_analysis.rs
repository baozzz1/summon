//! 洞察分析:由主 agent 对一段时间的洞察汇总做一次分析。
//!
//! 通路与文档评论起草相同:在临时目录里以主 agent 自己的 CLI、模型与账号环境起一次性进程,关掉工具、
//! 不持久化会话,从输出里取结果。不进 chat 线程、不落入站 WAL、不碰 tmux 会话、不发飞书消息。交给
//! 模型的只有固定提示词与这段时间的汇总 JSON(计数、时长与比例)。
//!
//! - 结果按「槽」(时间段、agent 筛选、语言)存取,完整缓存键只用来判新旧:槽里的结果与此刻的键
//!   不同就是过期了,照样给页面看。同键已有成功结果不重跑(`force` 除外),过期了直接重跑。
//! - 全机同一时刻最多一个分析在跑;同槽在跑时返回进行中,不管数据变没变。
//! - 重新生成失败不覆盖槽里已有的成功结果,只在它上面记下这次失败。
//! - 进程自成进程组;超时或 daemon 停机时先 SIGINT 整组,宽限之后才 SIGKILL 兜底,不发 SIGTERM。
//! - 结果落 `run/insights-analysis/<槽>.json`,一个槽一份,保留最近 [`KEEP`] 个槽。
//!
//! 这次调用的 token 记在结果里,同时记一行轮外用量(`run/usage/`),归到跑它的主 agent,用量页与
//! `summon usage` 照常合计、计价。CLI 报了用量就记,成败不论(模型回了话、结果被判不合格也花了钱);
//! 没报(超时被杀、进程没起来)不记。每次运行至多一行;写不进只记日志,不影响结果。一次性进程不落会话、
//! 临时 thread 不进会话池,转录与 RPC 两条既有的用量采集都读不到它,不会重复计。

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use summon_core::driver::CliDriver;
use summon_core::i18n::{with_language, Language};
use summon_core::insights_analysis::{
    self as core, AnalysisUsage, CacheKey, Candidate, PrimarySource, Unsupported,
};
use summon_core::registry::{Agent, AgentRole};
use summon_core::turn_ledger::{self as ledger, ModelUsage, OutsideUsage};

use crate::observe::log::log;
use crate::runtime::executor::ChildKiller;

/// 一次分析最多跑这么久。
pub(crate) const TIMEOUT: Duration = Duration::from_secs(300);
/// 盘上保留最近这么多个槽的结果。
pub(crate) const KEEP: usize = 20;
/// 进程输出(claude 的结果 JSON)的上限。
const MAX_OUTPUT: u64 = 256 * 1024;
/// SIGINT 之后等它自己退出的宽限,与 daemon 停机打断无头轮的宽限相同。
const GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);
/// 分析的会话名:过准入闸时登记它,让 daemon 换代方看得见它在忙;记轮外用量时它是那一行的 `tag`,
/// 用量页凭它认出这一行来自洞察分析。
pub(crate) const SESSION_TAG: &str = "insights-analysis";

/// 此刻活着的分析工作线程数。daemon 停机时等它归零(有上限)。
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

// ── 主 agent ────────────────────────────────────────────────────────────

/// 能代表这台机器跑模型:常规 agent、在用、可路由、没被停用。
pub(crate) fn usable(agent: &Agent, enabled: bool) -> bool {
    agent.role == AgentRole::Agent && agent.is_routable() && enabled
}

/// 配置时刻:agent 目录的创建时刻。改 agent.yaml、写会话状态都不动它;拿不到(文件系统不记)为 `None`。
fn created_ms(dir: &Path) -> Option<i64> {
    let created = std::fs::metadata(dir).ok()?.created().ok()?;
    let ms = created
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    i64::try_from(ms).ok()
}

pub(crate) struct PrimaryAgent {
    pub(crate) agent: Agent,
    pub(crate) source: PrimarySource,
}

/// 解析主 agent。用量与用量页同一份数据:近 [`core::USAGE_WINDOW_DAYS`] 天结束的轮与轮外用量。
pub(crate) fn primary(
    configured: Option<&str>,
    agents: Vec<Agent>,
    enabled: &dyn Fn(&str) -> bool,
    run_dir: &Path,
    now_ms: i64,
) -> Option<PrimaryAgent> {
    let since = now_ms - core::USAGE_WINDOW_DAYS * 24 * 3600 * 1000;
    let (records, outside) =
        crate::runtime::dashboard::api::usage::load_rows(run_dir, since, now_ms + 1);
    let mut tokens: BTreeMap<String, u64> = BTreeMap::new();
    for (agent, usage) in records
        .iter()
        .map(|r| (&r.agent, &r.usage))
        .chain(outside.iter().map(|o| (&o.agent, &o.usage)))
    {
        *tokens.entry(agent.clone()).or_default() += usage.iter().map(|u| u.tokens()).sum::<u64>();
    }
    let candidates: Vec<Candidate<'_>> = agents
        .iter()
        .map(|a| Candidate {
            alias: &a.name,
            usable: usable(a, enabled(&a.name)),
            created_ms: created_ms(&a.dir),
        })
        .collect();
    let picked = core::resolve_primary(configured, &candidates, &tokens)?;
    let source = picked.source;
    agents
        .into_iter()
        .find(|a| a.name == picked.alias)
        .map(|agent| PrimaryAgent { agent, source })
}

// ── 计划 ────────────────────────────────────────────────────────────────

/// 一次分析请求:时间段、筛选、语言与这段时间的汇总 JSON。
pub(crate) struct Request {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) agent_filter: Option<String>,
    pub(crate) lang: Language,
    pub(crate) summary_json: String,
}

#[derive(Clone)]
pub(crate) struct Job {
    pub(crate) agent: Agent,
    /// 此刻的完整缓存键。
    pub(crate) key: CacheKey,
    /// 槽(时间段、筛选、语言)的哈希:结果文件名,也是 single-flight 的身份。
    pub(crate) slot: String,
    pub(crate) prompt: String,
    pub(crate) lang: Language,
}

pub(crate) enum Plan {
    /// 没有可用的 agent,或主 agent 的 CLI 不支持一次性运行。原因已按请求的语言写好。
    Unsupported(String),
    Ready(Box<Job>),
}

/// 按请求与主 agent 定下要跑的那一次;不支持时不起任何进程。
pub(crate) fn plan(primary: Option<&PrimaryAgent>, req: Request) -> Plan {
    let lang = req.lang;
    let Some(primary) = primary else {
        return Plan::Unsupported(with_language(lang, || {
            summon_i18n::text!("r64bd2f2333").to_string()
        }));
    };
    let agent = &primary.agent.name;
    let cli = primary.agent.cli_kind();
    if let Err(why) = core::support(CliDriver::from_kind(cli)) {
        return Plan::Unsupported(with_language(lang, || match why {
            Unsupported::Cursor => summon_i18n::format!("r3d73b2606b"),
            Unsupported::UnknownCli => summon_i18n::format!("r8e5390eaee"),
        }));
    }
    let model = crate::runtime::chat_model::effective(&primary.agent, None)
        .model()
        .map(str::to_string);
    let key = CacheKey {
        from: req.from,
        to: req.to,
        agent_filter: req.agent_filter,
        data: crate::runtime::document_comments::hex_digest(req.summary_json.as_bytes()),
        prompt_version: core::PROMPT_VERSION,
        lang,
        primary: agent.clone(),
        model,
    };
    let slot = crate::runtime::document_comments::hex_digest(key.slot_canonical().as_bytes())[..32]
        .to_string();
    Plan::Ready(Box::new(Job {
        agent: primary.agent.clone(),
        key,
        slot,
        prompt: core::compose(lang, &req.summary_json),
        lang,
    }))
}

// ── 结果 ────────────────────────────────────────────────────────────────

/// 重新生成没成功:记在槽里那份成功结果上。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RetryFailure {
    /// 按请求的语言写好。
    pub(crate) reason: String,
    pub(crate) at_ms: i64,
}

/// 一个槽里的结果(成功或失败),落盘的就是它。不存输入:只有缓存键里的指纹。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) slot: String,
    /// 生成这份结果时的完整缓存键;与此刻的键不同即过期。
    pub(crate) key: CacheKey,
    pub(crate) ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) truncated: bool,
    /// 失败原因,按请求的语言写好。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    pub(crate) generated_at_ms: i64,
    pub(crate) agent: String,
    pub(crate) cli: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<AnalysisUsage>,
    /// 成功结果之后最近一次重新生成失败了;下一次成功时清掉。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_failure: Option<RetryFailure>,
}

pub(crate) enum State {
    None,
    Running { since_ms: i64 },
    Finished(Box<Record>),
}

pub(crate) enum Started {
    /// 开跑了,或同槽本来就在跑。
    Running,
    /// 槽里已有同键的成功结果,没有重跑。
    Cached(Box<Record>),
}

/// 一次运行的结局:落盘的那份,以及这次失败的原因(失败时;槽里原有的成功结果保留着)。
pub(crate) struct Ran {
    pub(crate) record: Record,
    pub(crate) error: Option<String>,
}

/// 另一项分析正在跑。
#[derive(Debug)]
pub(crate) struct Busy;

#[derive(Clone)]
struct Running {
    slot: String,
    since_ms: i64,
}

/// 分析服务:一个 daemon 一份(挂在控制台的接口环境上)。
pub(crate) struct Service {
    dir: PathBuf,
    usage_dir: PathBuf,
    stop: Arc<AtomicBool>,
    timeout_ms: AtomicU64,
    running: Arc<Mutex<Option<Running>>>,
}

impl Service {
    /// `dir` 是结果目录,`usage_dir` 是轮外用量目录;`stop` 置位即中止在跑的分析(daemon 停机)。
    pub(crate) fn new(dir: PathBuf, usage_dir: PathBuf, stop: Arc<AtomicBool>) -> Self {
        Self {
            dir,
            usage_dir,
            stop,
            timeout_ms: AtomicU64::new(TIMEOUT.as_millis() as u64),
            running: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn with_stop(mut self, stop: Arc<AtomicBool>) -> Self {
        self.stop = stop;
        self
    }

    #[cfg(test)]
    pub(crate) fn set_timeout(&self, timeout: Duration) {
        self.timeout_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.load(Ordering::Relaxed))
    }

    /// 槽此刻的状态:在跑优先(不管数据变没变),其次是槽里的那份结果。
    pub(crate) fn state(&self, slot: &str) -> State {
        if let Some(running) = lock(&self.running).as_ref().filter(|r| r.slot == slot) {
            return State::Running {
                since_ms: running.since_ms,
            };
        }
        match read(&self.dir, slot) {
            Some(record) => State::Finished(Box::new(record)),
            None => State::None,
        }
    }

    /// 占住唯一的那个位置。`Ok(Some)` = 不必跑:同槽在跑,或槽里已有同键的成功结果且没要求重跑。
    /// 槽里的结果过期了(键不同)就照样跑。
    fn claim(&self, job: &Job, force: bool) -> Result<Option<Started>, Busy> {
        let mut running = lock(&self.running);
        if let Some(current) = running.as_ref() {
            return if current.slot == job.slot {
                Ok(Some(Started::Running))
            } else {
                Err(Busy)
            };
        }
        if !force {
            if let Some(record) = read(&self.dir, &job.slot).filter(|r| r.ok && r.key == job.key) {
                return Ok(Some(Started::Cached(Box::new(record))));
            }
        }
        *running = Some(Running {
            slot: job.slot.clone(),
            since_ms: now_ms(),
        });
        Ok(None)
    }

    fn worker(&self, slot: &str) -> Worker {
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        Worker {
            dir: self.dir.clone(),
            usage_dir: self.usage_dir.clone(),
            stop: Arc::clone(&self.stop),
            timeout: self.timeout(),
            claim: Claim {
                running: Arc::clone(&self.running),
                slot: slot.to_string(),
            },
        }
    }

    /// 在后台跑一次。
    pub(crate) fn start(&self, job: Job, force: bool) -> Result<Started, Busy> {
        if let Some(started) = self.claim(&job, force)? {
            return Ok(started);
        }
        let worker = self.worker(&job.slot);
        let spawned = std::thread::Builder::new()
            .name("insights-analysis".into())
            .spawn(move || worker.run(&job));
        if let Err(error) = spawned {
            // 线程起不来时 worker 随闭包一起被丢掉,位置随之释放。
            log(&format!("insights-analysis: {error}"));
        }
        Ok(Started::Running)
    }

    /// 在当前线程跑完(调试命令用)。
    #[cfg_attr(not(debug_assertions), allow(dead_code))] // 只有 debug 构建里的 `summon dev` 调它
    pub(crate) fn run_blocking(&self, job: Job, force: bool) -> Result<Ran, Busy> {
        match self.claim(&job, force)? {
            Some(Started::Cached(record)) => Ok(Ran {
                record: *record,
                error: None,
            }),
            Some(Started::Running) => Err(Busy),
            None => Ok(self.worker(&job.slot).run(&job)),
        }
    }

    #[cfg(test)]
    pub(crate) fn save(&self, record: &Record) {
        save(&self.dir, record);
    }
}

/// daemon 停机:等在跑的分析收手(它看停机标志自己打断进程),至多等 `limit`。
pub(crate) fn wait_stopped(limit: Duration) {
    let started = Instant::now();
    while ACTIVE.load(Ordering::SeqCst) > 0 && started.elapsed() < limit {
        std::thread::sleep(POLL);
    }
}

/// 占着的位置:落地(含 panic 展开)即释放,不会留下「进行中」。
struct Claim {
    running: Arc<Mutex<Option<Running>>>,
    slot: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut running = lock(&self.running);
        if running.as_ref().is_some_and(|r| r.slot == self.slot) {
            *running = None;
        }
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Worker {
    dir: PathBuf,
    usage_dir: PathBuf,
    stop: Arc<AtomicBool>,
    timeout: Duration,
    claim: Claim,
}

impl Worker {
    fn run(self, job: &Job) -> Ran {
        let agent = &job.agent.name;
        let cli = job.agent.cli_kind().to_string();
        log(&summon_i18n::format!("r32aa0076d7"));
        let started_ms = now_ms();
        let started = Instant::now();
        let mut spent = Vec::new();
        let outcome = execute(job, &self.stop, started + self.timeout, &mut spent);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let now = now_ms();
        self.bill(agent, &cli, spent, started_ms, now);
        let ran = match outcome {
            Ok(output) => {
                let (text, truncated) = core::clip(&output.text);
                log(&summon_i18n::format!("rc27184b028"));
                Ran {
                    record: Record {
                        slot: job.slot.clone(),
                        key: job.key.clone(),
                        ok: true,
                        text: Some(text),
                        truncated,
                        error: None,
                        generated_at_ms: now,
                        agent: agent.clone(),
                        cli: cli.clone(),
                        model: job.key.model.clone(),
                        elapsed_ms,
                        usage: output.usage,
                        last_failure: None,
                    },
                    error: None,
                }
            }
            Err(failure) => {
                let detail = failure.detail();
                log(&summon_i18n::format!("rd41f1b1e19"));
                let reason = with_language(job.lang, || failure.reason(&cli, self.timeout));
                let record = match read(&self.dir, &job.slot).filter(|r| r.ok) {
                    // 槽里已有成功结果:留着它,只记下这次没成功。
                    Some(mut previous) => {
                        previous.last_failure = Some(RetryFailure {
                            reason: reason.clone(),
                            at_ms: now,
                        });
                        previous
                    }
                    None => Record {
                        slot: job.slot.clone(),
                        key: job.key.clone(),
                        ok: false,
                        text: None,
                        truncated: false,
                        error: Some(reason.clone()),
                        generated_at_ms: now,
                        agent: agent.clone(),
                        cli: cli.clone(),
                        model: job.key.model.clone(),
                        elapsed_ms,
                        usage: None,
                        last_failure: None,
                    },
                };
                Ran {
                    record,
                    error: Some(reason),
                }
            }
        };
        save(&self.dir, &ran.record);
        // 先落盘再释放位置:读的一方不会在两者之间看到「没有结果」。
        drop(self.claim);
        ran
    }

    /// 这次运行 CLI 报出的 token 记成一行轮外用量。不属于任何群,`chat_id` 留空,`tag` 记成分析
    /// 的会话名。
    fn bill(&self, agent: &str, cli: &str, usage: Vec<ModelUsage>, started_ms: i64, ended_ms: i64) {
        if usage.is_empty() {
            return;
        }
        let (from_ms, to_ms) = one_day(started_ms, ended_ms);
        let row = OutsideUsage {
            agent: agent.to_string(),
            chat_id: String::new(),
            tag: SESSION_TAG.to_string(),
            cli: cli.to_string(),
            from_ms,
            to_ms,
            usage,
        };
        if let Err(error) = crate::runtime::turn_ledger::append_outside(&self.usage_dir, &row) {
            // 记账是旁路:写不进只是用量页少计这一次,分析结果照常落盘、照常返回。
            log(&format!("insights-analysis: usage: {error:#}"));
        }
    }
}

/// 一行轮外用量不跨本机的自然日(按天统计时整行落在 `from_ms` 那天):跑过零点的那次整行记在结束那一刻。
fn one_day(started_ms: i64, ended_ms: i64) -> (i64, i64) {
    let day = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|t| t.with_timezone(&chrono::Local).date_naive())
    };
    if day(started_ms) == day(ended_ms) {
        (started_ms, ended_ms)
    } else {
        (ended_ms, ended_ms)
    }
}

fn path_of(dir: &Path, slot: &str) -> Option<PathBuf> {
    (slot.len() == 32 && slot.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| dir.join(format!("{slot}.json")))
}

fn read(dir: &Path, slot: &str) -> Option<Record> {
    let text = std::fs::read_to_string(path_of(dir, slot)?).ok()?;
    serde_json::from_str::<Record>(&text)
        .ok()
        .filter(|r| r.slot == slot)
}

/// 写结果并只留最近 [`KEEP`] 个槽(一个槽一个文件)。失败只记日志:结果没落盘不影响这一次的返回。
fn save(dir: &Path, record: &Record) {
    let Some(path) = path_of(dir, &record.slot) else {
        return;
    };
    let written = std::fs::create_dir_all(dir)
        .map_err(anyhow::Error::from)
        .and_then(|_| Ok(serde_json::to_vec_pretty(record)?))
        .and_then(|bytes| crate::runtime::fsio::atomic_write(&path, &bytes));
    if let Err(error) = written {
        log(&format!("insights-analysis: {error:#}"));
    }
    prune(dir, KEEP);
}

fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, path) in files.into_iter().skip(keep) {
        // 删不掉的留到下一次再删。
        let _ = std::fs::remove_file(path);
    }
}

// ── 跑一次 ──────────────────────────────────────────────────────────────

struct Output {
    text: String,
    usage: Option<AnalysisUsage>,
}

/// 失败的类别;界面上只显示类别对应的原因,细节进日志。
enum Failure {
    Stopped,
    Timeout,
    Spawn(String),
    Exit(String),
    Incomplete(String),
    TooLarge,
}

impl Failure {
    fn detail(&self) -> &str {
        match self {
            Failure::Spawn(detail) | Failure::Exit(detail) | Failure::Incomplete(detail) => detail,
            Failure::Stopped => "stopped",
            Failure::Timeout => "timeout",
            Failure::TooLarge => "output too large",
        }
    }

    fn reason(&self, cli: &str, timeout: Duration) -> String {
        let secs = timeout.as_secs().max(1);
        match self {
            Failure::Stopped => summon_i18n::text!("r1f8a3b1d15").to_string(),
            Failure::Timeout => summon_i18n::format!("rab0bdfe9f6"),
            Failure::Spawn(_) => summon_i18n::format!("r7cc0e5f9ba"),
            Failure::Exit(_) => summon_i18n::format!("r4544b39f87"),
            Failure::Incomplete(_) => summon_i18n::format!("r0484b08ab3"),
            Failure::TooLarge => summon_i18n::format!("rcf316696c6"),
        }
    }
}

/// 停机与时限:停机优先(它说明的是这台机器在换代,不是分析本身慢)。
struct Control<'a> {
    stop: &'a AtomicBool,
    deadline: Instant,
}

impl Control<'_> {
    fn check(&self) -> Result<(), Failure> {
        if self.stop.load(Ordering::SeqCst) {
            Err(Failure::Stopped)
        } else if Instant::now() >= self.deadline {
            Err(Failure::Timeout)
        } else {
            Ok(())
        }
    }

    /// 出错之后先看是不是停机或超时造成的。
    fn explain(&self, failure: Failure) -> Failure {
        self.check().err().unwrap_or(failure)
    }
}

/// `spent` 收 CLI 报出的 token(按模型合并):失败的运行报了也留着,由调用方记账。
fn execute(
    job: &Job,
    stop: &AtomicBool,
    deadline: Instant,
    spent: &mut Vec<ModelUsage>,
) -> Result<Output, Failure> {
    // 与一轮一样过准入闸:换代前关闸时在这里等,停机开始后不再开跑。
    let Some(_handling) = crate::runtime::admission::GATE.admit(
        &job.agent.name,
        SESSION_TAG,
        crate::runtime::admission::Kind::Task,
    ) else {
        return Err(Failure::Stopped);
    };
    let control = Control { stop, deadline };
    control.check()?;
    let driver = CliDriver::from_kind(job.agent.cli_kind());
    let env = crate::runtime::daemon::document_comment_draft::backend_env(&job.agent, driver);
    let temp = tempfile::tempdir().map_err(|e| Failure::Spawn(e.to_string()))?;
    match driver {
        CliDriver::Claude => run_claude(&job.agent, &job.prompt, env, temp.path(), &control, spent),
        CliDriver::Codex | CliDriver::Traex => {
            run_codex(&job.agent, &job.prompt, env, temp.path(), &control, spent)
        }
        // `plan` 已挡下其余 CLI;这里不起进程。
        _ => Err(Failure::Spawn(job.agent.cli_kind().to_string())),
    }
}

/// claude 一次性运行:不带任何工具与 MCP、不读用户与项目设置、关 hooks、不落会话。
fn claude_args(agent: &Agent) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "json",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--setting-sources",
        "",
        "--settings",
        "{\"disableAllHooks\":true}",
        "--no-session-persistence",
        "--permission-mode",
        "dontAsk",
    ]
    .map(str::to_string)
    .to_vec();
    let effective = crate::runtime::chat_model::effective(agent, None);
    let (model_args, _) =
        CliDriver::Claude.resolve_launch_lenient(effective.model(), effective.effort());
    args.extend(model_args);
    args
}

fn run_claude(
    agent: &Agent,
    prompt: &str,
    env: Vec<(String, String)>,
    cwd: &Path,
    control: &Control<'_>,
    spent: &mut Vec<ModelUsage>,
) -> Result<Output, Failure> {
    let io = |e: std::io::Error| Failure::Spawn(e.to_string());
    let input = cwd.join("prompt.txt");
    crate::runtime::fsio::atomic_write_mode(&input, prompt.as_bytes(), 0o600)
        .map_err(|e| Failure::Spawn(format!("{e:#}")))?;
    let output_path = cwd.join("result.json");
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&output_path)
        .map_err(io)?;
    let mut command = Command::new(agent.cli_cmd());
    command
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .args(claude_args(agent))
        .stdin(std::fs::File::open(&input).map_err(io)?)
        .stdout(output)
        .stderr(Stdio::null())
        .process_group(0);
    let exited = supervise(command, &output_path, control)?;
    if let Ok(raw) = &exited.output {
        let configured = crate::runtime::chat_model::effective(agent, None);
        *spent = claude_spent(raw, configured.model());
    }
    if !exited.status.success() {
        return Err(control.explain(Failure::Exit(exited.status.to_string())));
    }
    parse_claude(&exited.output?)
}

/// claude 结果里报的 token,不论成败。`modelUsage` 按实际用的模型分开报(含 claude 自己的辅助
/// 请求);没有它时退回顶层 `usage`,记在配置的模型名下。只有合计、分不出单次请求的输入量,不分档、
/// 每个模型记一次请求(同运行记录里只有按轮合计时的处置)。
fn claude_spent(raw: &str, configured: Option<&str>) -> Vec<ModelUsage> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let n = |u: &Value, k: &str| u[k].as_u64().unwrap_or(0);
    let groups: Vec<ModelUsage> = match value["modelUsage"].as_object() {
        Some(models) if !models.is_empty() => models
            .iter()
            .map(|(model, u)| {
                let split = u.get("cacheCreationEphemeral5mInputTokens").is_some()
                    || u.get("cacheCreationEphemeral1hInputTokens").is_some();
                let (write_5m, write_1h) = if split {
                    (
                        n(u, "cacheCreationEphemeral5mInputTokens"),
                        n(u, "cacheCreationEphemeral1hInputTokens"),
                    )
                } else {
                    (n(u, "cacheCreationInputTokens"), 0)
                };
                ModelUsage {
                    model: model.clone(),
                    requests: 1,
                    input: n(u, "inputTokens"),
                    output: n(u, "outputTokens"),
                    cache_read: n(u, "cacheReadInputTokens"),
                    cache_write_5m: write_5m,
                    cache_write_1h: write_1h,
                    ..Default::default()
                }
            })
            .collect(),
        _ => {
            let u = &value["usage"];
            if !u.is_object() {
                return Vec::new();
            }
            let (write_5m, write_1h) = match u.get("cache_creation") {
                Some(c) => (
                    n(c, "ephemeral_5m_input_tokens"),
                    n(c, "ephemeral_1h_input_tokens"),
                ),
                None => (n(u, "cache_creation_input_tokens"), 0),
            };
            vec![ModelUsage {
                model: configured.unwrap_or_default().to_string(),
                requests: 1,
                input: n(u, "input_tokens"),
                output: n(u, "output_tokens"),
                cache_read: n(u, "cache_read_input_tokens"),
                cache_write_5m: write_5m,
                cache_write_1h: write_1h,
                ..Default::default()
            }]
        }
    };
    groups.into_iter().filter(|g| g.tokens() > 0).collect()
}

/// 只认完整成功的结果。
fn parse_claude(raw: &str) -> Result<Output, Failure> {
    let incomplete = |why: &str| Failure::Incomplete(why.to_string());
    let value: Value = serde_json::from_str(raw).map_err(|e| incomplete(&e.to_string()))?;
    if !(value["type"] == "result" && value["subtype"] == "success" && value["is_error"] == false) {
        return Err(incomplete("not a successful result"));
    }
    let text = value["result"].as_str().unwrap_or_default().trim();
    if text.is_empty() {
        return Err(incomplete("empty result"));
    }
    let usage = value.get("usage").filter(|u| u.is_object()).map(|u| {
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        AnalysisUsage {
            input: n("input_tokens"),
            output: n("output_tokens"),
            cache_read: n("cache_read_input_tokens"),
            cache_write: n("cache_creation_input_tokens"),
        }
    });
    Ok(Output {
        text: text.to_string(),
        usage,
    })
}

/// 一次性子进程:落地前没被回收的,先 SIGINT 整组,宽限之后再 SIGKILL。
struct Process {
    child: std::process::Child,
    killer: ChildKiller,
    reaped: bool,
}

impl Process {
    /// 组长没退:SIGINT 整组,宽限内不退再 SIGKILL。组长退了而组里还有进程(组 id 还被占着,
    /// 不会是别人的组):SIGKILL 收掉。可重复调用。
    fn stop(&mut self) {
        if !self.reaped {
            self.killer.interrupt();
            let started = Instant::now();
            while started.elapsed() < GRACE {
                if let Ok(Some(_)) = self.child.try_wait() {
                    self.reaped = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if !self.reaped || crate::runtime::inflight::group_alive(self.killer.pgid()) {
            self.killer.kill();
        }
        if !self.reaped {
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 进程自己退出了:退出状态,与它写下的输出(读不出、超出上限时是对应的失败)。
struct Exited {
    status: std::process::ExitStatus,
    output: Result<String, Failure>,
}

/// 跑到进程自己退出。退出码非零也读输出:模型回了话却判为失败时,输出里仍报着用量。
fn supervise(
    mut command: Command,
    output: &Path,
    control: &Control<'_>,
) -> Result<Exited, Failure> {
    control.check()?;
    let child = command.spawn().map_err(|e| Failure::Spawn(e.to_string()))?;
    let killer = ChildKiller::new(child.id() as i32);
    let mut process = Process {
        child,
        killer,
        reaped: false,
    };
    let status = loop {
        match process.child.try_wait() {
            Ok(Some(status)) => {
                process.reaped = true;
                break status;
            }
            Ok(None) => {}
            Err(e) => return Err(Failure::Exit(e.to_string())),
        }
        control.check()?;
        if std::fs::metadata(output).map_or(0, |m| m.len()) > MAX_OUTPUT {
            return Err(Failure::TooLarge);
        }
        std::thread::sleep(POLL);
    };
    // 组长退了,组里可能还有它起的进程。
    process.stop();
    let mut raw = String::new();
    let read =
        std::fs::File::open(output).and_then(|f| f.take(MAX_OUTPUT + 1).read_to_string(&mut raw));
    let output = match read {
        Err(e) => Err(Failure::Incomplete(e.to_string())),
        Ok(_) if raw.len() as u64 > MAX_OUTPUT => Err(Failure::TooLarge),
        Ok(_) => Ok(raw),
    };
    Ok(Exited { status, output })
}

/// codex / traex:app-server 的临时只读 thread,所有 MCP 关掉、不开 shell 工具、不落会话。
fn run_codex(
    agent: &Agent,
    prompt: &str,
    env: Vec<(String, String)>,
    cwd: &Path,
    control: &Control<'_>,
    spent: &mut Vec<ModelUsage>,
) -> Result<Output, Failure> {
    use crate::codex::codex_app_server::{
        CodexAppServerClient, CodexAppServerConfig, CodexAppServerError, TurnOptions,
    };
    use summon_core::codex_rpc::CodexAppServerNotification;
    let rpc = |e: CodexAppServerError| control.explain(Failure::Exit(e.to_string()));
    let mut args = vec![
        "app-server".into(),
        "--listen".into(),
        "stdio://".into(),
        "-c".into(),
        "features.hooks=false".into(),
        "-c".into(),
        "features.plugins=false".into(),
        "-c".into(),
        "features.apps=false".into(),
    ];
    if let Some(window) = agent.runtime.as_ref().and_then(|r| r.model_context_window) {
        args.extend(["-c".into(), format!("model_context_window={window}")]);
    }
    let mut config = CodexAppServerConfig {
        program: agent.cli_cmd(),
        args,
        work_dir: Some(cwd.into()),
        env,
        request_timeout: Duration::from_secs(5),
        process_deadline: Some(control.deadline),
        experimental_api: true,
        ..Default::default()
    };
    control.check()?;
    let mut preflight = CodexAppServerClient::spawn(config.clone())
        .map_err(|e| control.explain(Failure::Spawn(e.to_string())))?;
    config
        .args
        .extend(preflight.draft_mcp_overrides(None).map_err(rpc)?);
    drop(preflight);
    control.check()?;
    let mut client = CodexAppServerClient::spawn(config)
        .map_err(|e| control.explain(Failure::Spawn(e.to_string())))?;
    let thread = client
        .thread_start_draft(&cwd.to_string_lossy(), None)
        .map_err(rpc)?;
    let effective = crate::runtime::chat_model::effective(agent, None);
    let driver = CliDriver::from_kind(agent.cli_kind());
    let effort = effective
        .effort()
        .filter(|effort| driver.resolve_launch(None, Some(effort)).is_ok());
    control.check()?;
    let turn = client
        .turn_start(
            &thread.id,
            prompt,
            &TurnOptions {
                model: effective.model(),
                effort,
                approval_policy: Some("never"),
                ..Default::default()
            },
            None,
        )
        .map_err(rpc)?;
    let mut text: Option<String> = None;
    let mut final_seen = false;
    let mut usage = None;
    // 记账的口径与会话轮相同:每次模型请求一笔、按请求分档,模型记 `turn/start` 下发的那个。
    let model = effective.model().map(str::to_string);
    let mut tally = crate::capture::rpc::RequestTally::new(&turn.id);
    let mut requests = Vec::new();
    loop {
        if let Err(failure) = control.check() {
            let _ = client.interrupt(GRACE);
            return Err(failure);
        }
        let next = client.recv_notification(Some(POLL));
        if let Ok(notification @ CodexAppServerNotification::TokenUsage { thread_id, .. }) = &next {
            if *thread_id == thread.id {
                if let Some(request) = tally.observe(notification, model.clone(), None) {
                    requests.push(request);
                    *spent = ledger::merge_requests(&requests, None);
                }
            }
        }
        match next {
            Ok(CodexAppServerNotification::ItemCompleted {
                thread_id,
                turn_id,
                item,
            }) if thread_id == thread.id && turn_id == turn.id => {
                if matches!(
                    item["type"].as_str(),
                    Some("agentMessage" | "agent_message")
                ) && (item["phase"].as_str() == Some("final_answer")
                    || (item["phase"].is_null() && !final_seen))
                {
                    text = item["text"].as_str().map(str::to_string);
                    final_seen = item["phase"].as_str() == Some("final_answer");
                }
            }
            Ok(CodexAppServerNotification::TokenUsage {
                thread_id,
                last: Some(last),
                ..
            }) if thread_id == thread.id => {
                let cache_read = last.cached_input_tokens;
                let cache_write = last.cache_write_input_tokens;
                usage = Some(AnalysisUsage {
                    input: last
                        .input_tokens
                        .saturating_sub(cache_read)
                        .saturating_sub(cache_write),
                    output: last.output_tokens,
                    cache_read,
                    cache_write,
                });
            }
            Ok(CodexAppServerNotification::TurnCompleted {
                thread_id,
                turn: completed,
            }) if thread_id == thread.id && completed.id == turn.id => {
                if completed.status != "completed" || completed.error.is_some() {
                    return Err(Failure::Incomplete(completed.status));
                }
                let text = text.unwrap_or_default();
                if text.trim().is_empty() {
                    return Err(Failure::Incomplete("empty result".into()));
                }
                return Ok(Output { text, usage });
            }
            Ok(CodexAppServerNotification::Error {
                thread_id,
                turn_id,
                will_retry: false,
                ..
            }) if thread_id == thread.id && turn_id == turn.id => {
                return Err(Failure::Incomplete("turn error".into()));
            }
            Ok(_) | Err(CodexAppServerError::Timeout { .. }) => {}
            Err(error) => return Err(rpc(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_test_executable;
    use serde_json::json;

    const SUMMARY: &str = "{\"turns\":3,\"gaps\":{\"n\":2}}";

    /// 假 claude:记下参数、标准输入与看得到的凭据,按 `$0` 旁的 `mode` 文件决定行为。
    const FAKE_CLAUDE: &str = r#"#!/bin/sh
root="${0%/*}"
printf '%s\n' "$@" > "$root/args"
cat > "$root/stdin"
printf '%s|%s|%s' "${ANTHROPIC_AUTH_TOKEN-}" "${OPENAI_API_KEY-}" "${SUMMON_AGENT-}" > "$root/env"
n=$(cat "$root/count" 2>/dev/null || echo 0); echo $((n+1)) > "$root/count"
mode=$(cat "$root/mode" 2>/dev/null || echo ok)
case "$mode" in
ok) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"  观察:间隔集中在长尾。  ","usage":{"input_tokens":120,"output_tokens":45,"cache_read_input_tokens":300,"cache_creation_input_tokens":7},"modelUsage":{"claude-analysis-1":{"inputTokens":120,"outputTokens":45,"cacheReadInputTokens":300,"cacheCreationInputTokens":7,"cacheCreationEphemeral5mInputTokens":3,"cacheCreationEphemeral1hInputTokens":4},"claude-helper-1":{"inputTokens":9,"outputTokens":1}}}' ;;
refused) printf '%s\n' '{"type":"result","subtype":"success","is_error":true,"result":"refused","usage":{"input_tokens":11,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":5}}'; exit 1 ;;
empty) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"  ","modelUsage":{"claude-analysis-1":{"inputTokens":6,"outputTokens":1}}}' ;;
zero) printf '%s\n' '{"type":"result","subtype":"success","is_error":true,"result":"no credit","usage":{"input_tokens":0,"output_tokens":0},"modelUsage":{}}'; exit 1 ;;
long) awk 'BEGIN{s="{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\""; for(i=0;i<7000;i++) s=s "x"; print s "\"}"}' ;;
fail) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"partial"}'; exit 1 ;;
garbage) printf 'not json\n' ;;
sleep) exec sleep 30 ;;
trap) trap 'echo int > "$root/signal"; exit 130' INT; echo ready > "$root/ready"; while :; do sleep 0.05; done ;;
gate) while [ ! -f "$root/release" ]; do sleep 0.05; done; printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"released"}' ;;
esac
"#;

    struct Fixture {
        bin: tempfile::TempDir,
        agent: Agent,
        _agent_dir: tempfile::TempDir,
        results: tempfile::TempDir,
        /// 轮外用量写在这下面的 `usage/`:测试不碰真实的 `~/.summon`。
        run: tempfile::TempDir,
    }

    impl Fixture {
        fn new(cli: &str) -> Self {
            let bin = tempfile::tempdir().unwrap();
            let script = bin.path().join("backend");
            write_test_executable(&script, FAKE_CLAUDE);
            let (agent, dir) = crate::test_support::agent_dir(
                cli,
                "model: analysis-model\nfeishu:\n  app_id_env: ANALYSIS_APP\n",
            );
            let mut config: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap())
                    .unwrap();
            config["runtime"]["cli_cmd"] = serde_yaml::Value::String(script.display().to_string());
            config["runtime"]["env"] = serde_yaml::to_value(json!({
                "ANTHROPIC_AUTH_TOKEN": "claude-token", "OPENAI_API_KEY": "codex-token",
            }))
            .unwrap();
            std::fs::write(
                agent.manifest_path(),
                serde_yaml::to_string(&config).unwrap(),
            )
            .unwrap();
            Self {
                bin,
                agent: agent.reread().unwrap(),
                _agent_dir: dir,
                results: tempfile::tempdir().unwrap(),
                run: tempfile::tempdir().unwrap(),
            }
        }

        fn usage_dir(&self) -> PathBuf {
            crate::runtime::turn_ledger::outside_dir_in(self.run.path())
        }

        /// 记下的轮外用量。
        fn spent(&self) -> Vec<OutsideUsage> {
            let now = now_ms();
            let day = 24 * 3600 * 1000;
            crate::runtime::turn_ledger::read_outside(&self.usage_dir(), None, now - day, now + day)
        }

        /// 换成别的可执行文件(假 app-server)。
        fn use_cmd(&mut self, cmd: &Path) {
            let mut config: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(self.agent.manifest_path()).unwrap())
                    .unwrap();
            config["runtime"]["cli_cmd"] = serde_yaml::Value::String(cmd.display().to_string());
            std::fs::write(
                self.agent.manifest_path(),
                serde_yaml::to_string(&config).unwrap(),
            )
            .unwrap();
            self.agent = self.agent.reread().unwrap();
        }

        fn mode(&self, mode: &str) {
            std::fs::write(self.bin.path().join("mode"), mode).unwrap();
        }

        fn file(&self, name: &str) -> String {
            std::fs::read_to_string(self.bin.path().join(name)).unwrap_or_default()
        }

        fn runs(&self) -> u32 {
            self.file("count").trim().parse().unwrap_or(0)
        }

        fn service(&self) -> Service {
            Service::new(
                self.results.path().to_path_buf(),
                self.usage_dir(),
                Arc::new(AtomicBool::new(false)),
            )
        }

        fn job(&self, summary: &str, lang: Language) -> Job {
            let primary = PrimaryAgent {
                agent: self.agent.clone(),
                source: PrimarySource::Configured,
            };
            match plan(
                Some(&primary),
                Request {
                    from: "2026-10-01".into(),
                    to: "2026-10-30".into(),
                    agent_filter: None,
                    lang,
                    summary_json: summary.into(),
                },
            ) {
                Plan::Ready(job) => *job,
                Plan::Unsupported(why) => panic!("{why}"),
            }
        }
    }

    fn wait_done(service: &Service, slot: &str) -> Record {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match service.state(slot) {
                State::Finished(record) => return *record,
                State::Running { .. } => {}
                State::None => panic!("分析既不在跑也没有结果"),
            }
            assert!(Instant::now() < deadline, "分析没在时限内结束");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn claude_runs_once_without_tools_or_session_and_keeps_text_and_usage() {
        let fx = Fixture::new("claude");
        let service = fx.service();
        let job = fx.job(SUMMARY, Language::Chinese);
        let ran = service.run_blocking(job.clone(), false).unwrap();
        assert!(ran.error.is_none());
        let record = ran.record;
        assert!(record.ok, "{record:?}");
        assert_eq!(record.text.as_deref(), Some("观察:间隔集中在长尾。"));
        assert_eq!(
            record.usage,
            Some(AnalysisUsage {
                input: 120,
                output: 45,
                cache_read: 300,
                cache_write: 7
            })
        );
        assert_eq!(
            (record.agent.as_str(), record.cli.as_str()),
            (fx.agent.name.as_str(), "claude")
        );
        assert_eq!(record.model.as_deref(), Some("analysis-model"));
        let args = fx.file("args");
        for flag in [
            "-p\n",
            "--tools\n\n",
            "--strict-mcp-config\n",
            "--setting-sources\n\n",
            "--no-session-persistence\n",
            "--model\nanalysis-model\n",
        ] {
            assert!(args.contains(flag), "{flag:?} 不在参数里:{args}");
        }
        assert!(!args.contains("--resume") && !args.contains("--allowedTools"));
        // 输入只有固定提示词与汇总
        assert_eq!(fx.file("stdin"), core::compose(Language::Chinese, SUMMARY));
        // 只带自己这一家 CLI 的凭据,不带 summon 的会话变量
        assert_eq!(fx.file("env"), "claude-token||");
        // 落盘的就是返回的那份,且不含输入
        let disk =
            std::fs::read_to_string(fx.results.path().join(format!("{}.json", job.slot))).unwrap();
        assert_eq!(serde_json::from_str::<Record>(&disk).unwrap(), record);
        assert!(!disk.contains("\"turns\"") && !disk.contains(core::prompt(Language::Chinese)));
        assert!(matches!(service.state(&job.slot), State::Finished(r) if r.ok));
    }

    /// 一行用量里各模型的数:(模型, 请求数, 输入, 输出, 缓存读, 5 分钟写, 1 小时写),按模型排好。
    fn groups(row: &OutsideUsage) -> Vec<(String, u32, u64, u64, u64, u64, u64)> {
        let mut groups: Vec<_> = row
            .usage
            .iter()
            .map(|u| {
                (
                    u.model.clone(),
                    u.requests,
                    u.input,
                    u.output,
                    u.cache_read,
                    u.cache_write_5m,
                    u.cache_write_1h,
                )
            })
            .collect();
        groups.sort();
        groups
    }

    #[test]
    fn a_successful_analysis_is_billed_once_to_its_agent_under_the_models_it_used() {
        let fx = Fixture::new("claude");
        let before = now_ms();
        let ran = fx
            .service()
            .run_blocking(fx.job(SUMMARY, Language::Chinese), false)
            .unwrap();
        let after = now_ms();
        assert!(ran.error.is_none() && ran.record.ok, "{:?}", ran.record);
        let rows = fx.spent();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(
            (
                row.agent.as_str(),
                row.cli.as_str(),
                row.chat_id.as_str(),
                row.tag.as_str()
            ),
            (fx.agent.name.as_str(), "claude", "", SESSION_TAG)
        );
        assert!(before <= row.from_ms && row.from_ms <= row.to_ms && row.to_ms <= after);
        // 实际用的模型来自 `modelUsage`,不是配置里的名字;缓存写入按两档拆开
        assert_eq!(
            groups(row),
            [
                ("claude-analysis-1".into(), 1, 120, 45, 300, 3, 4),
                ("claude-helper-1".into(), 1, 9, 1, 0, 0, 0),
            ]
        );
        // 结果里的用量照旧
        assert_eq!(
            ran.record.usage,
            Some(AnalysisUsage {
                input: 120,
                output: 45,
                cache_read: 300,
                cache_write: 7
            })
        );
        // 用量页与 `summon usage` 读的就是这一份
        let (_, outside) =
            crate::runtime::dashboard::api::usage::load_rows(fx.run.path(), before, after + 1);
        assert_eq!(outside, rows);
    }

    #[test]
    fn failed_runs_are_billed_when_the_cli_reported_usage() {
        // 退出码非零、结果不合格:模型回过话,输出里报了用量
        for (mode, fragment, expected) in [
            (
                "refused",
                "异常退出",
                ("analysis-model".to_string(), 1, 11, 2, 0, 5, 0),
            ),
            (
                "empty",
                "没有给出完整的结果",
                ("claude-analysis-1".to_string(), 1, 6, 1, 0, 0, 0),
            ),
        ] {
            let fx = Fixture::new("claude");
            fx.mode(mode);
            let ran = fx
                .service()
                .run_blocking(fx.job(SUMMARY, Language::Chinese), false)
                .unwrap();
            let error = ran.error.unwrap();
            assert!(error.contains(fragment), "{mode}: {error}");
            assert!(!ran.record.ok && ran.record.usage.is_none(), "{mode}");
            let rows = fx.spent();
            assert_eq!(rows.len(), 1, "{mode}: {rows:?}");
            assert_eq!(groups(&rows[0]), [expected], "{mode}");
        }
        // codex:报过用量之后这一轮没完成
        let mut fx = Fixture::new("codex");
        let server = crate::test_support::write_fake_app_server(
            fx.bin.path(),
            r#"
*'config/read'*) printf '%s\n' '{"id":'"$id"',"result":{"config":{}}}' ;;
*'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"analysis-thread"}}}' ;;
*'turn/start'*)
printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"analysis-turn","status":"inProgress"}}}'
printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"analysis-thread","turnId":"analysis-turn","tokenUsage":{"total":{"totalTokens":90},"last":{"inputTokens":80,"cachedInputTokens":30,"outputTokens":10,"totalTokens":90}}}}'
printf '%s\n' '{"method":"turn/completed","params":{"threadId":"analysis-thread","turn":{"id":"analysis-turn","status":"failed"}}}' ;;
"#,
        );
        fx.use_cmd(&server);
        let ran = fx
            .service()
            .run_blocking(fx.job(SUMMARY, Language::Chinese), false)
            .unwrap();
        assert!(ran.error.is_some() && !ran.record.ok);
        let rows = fx.spent();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].cli, "codex");
        assert_eq!(
            groups(&rows[0]),
            [("analysis-model".into(), 1, 50, 10, 30, 0, 0)]
        );
    }

    #[test]
    fn runs_without_reported_usage_are_not_billed() {
        for mode in ["garbage", "fail", "zero", "sleep"] {
            let fx = Fixture::new("claude");
            fx.mode(mode);
            let service = fx.service();
            service.set_timeout(Duration::from_secs(1));
            let ran = service
                .run_blocking(fx.job(SUMMARY, Language::Chinese), false)
                .unwrap();
            assert!(ran.error.is_some() && !ran.record.ok, "{mode}");
            assert!(fx.spent().is_empty(), "{mode}: {:?}", fx.spent());
            assert!(!fx.usage_dir().exists(), "{mode}: 没有用量就不建目录");
        }
    }

    #[test]
    fn a_usage_write_failure_keeps_the_result() {
        let fx = Fixture::new("claude");
        // 轮外用量目录被一个普通文件占着:写不进
        std::fs::write(fx.usage_dir(), "").unwrap();
        let service = fx.service();
        let job = fx.job(SUMMARY, Language::Chinese);
        let ran = service.run_blocking(job.clone(), false).unwrap();
        assert!(ran.error.is_none(), "{:?}", ran.error);
        assert!(ran.record.ok);
        assert_eq!(ran.record.text.as_deref(), Some("观察:间隔集中在长尾。"));
        assert!(ran.record.usage.is_some());
        assert_eq!(wait_done(&service, &job.slot), ran.record, "结果照常落盘");
        assert!(fx.usage_dir().is_file());
    }

    #[test]
    fn a_row_never_spans_two_local_days() {
        use chrono::TimeZone;
        let at = |d, h, m| {
            chrono::Local
                .with_ymd_and_hms(2026, 10, d, h, m, 0)
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        assert_eq!(
            one_day(at(21, 10, 0), at(21, 10, 4)),
            (at(21, 10, 0), at(21, 10, 4))
        );
        assert_eq!(
            one_day(at(21, 23, 58), at(22, 0, 2)),
            (at(22, 0, 2), at(22, 0, 2))
        );
    }

    #[test]
    fn same_key_runs_once_while_stale_or_forced_results_run_again() {
        let fx = Fixture::new("claude");
        let service = fx.service();
        let job = fx.job(SUMMARY, Language::Chinese);
        assert!(matches!(
            service.start(job.clone(), false),
            Ok(Started::Running)
        ));
        let first = wait_done(&service, &job.slot);
        assert!(matches!(
            service.start(job.clone(), false),
            Ok(Started::Cached(r)) if *r == first
        ));
        assert_eq!(fx.runs(), 1, "同键已有结果:不重跑");
        assert!(matches!(
            service.start(job.clone(), true),
            Ok(Started::Running)
        ));
        let second = wait_done(&service, &job.slot);
        assert_eq!(fx.runs(), 2, "force:重跑");
        assert!(second.generated_at_ms >= first.generated_at_ms);
        // 数据变了:同一个槽,槽里的结果过期,不带 force 也重跑
        let newer = fx.job("{\"turns\":4}", Language::Chinese);
        assert_eq!(newer.slot, job.slot);
        let State::Finished(stale) = service.state(&newer.slot) else {
            panic!("槽里应有上一份结果");
        };
        assert!(stale.ok && stale.key != newer.key, "过期的结果照样取得到");
        assert!(matches!(
            service.start(newer.clone(), false),
            Ok(Started::Running)
        ));
        let fresh = wait_done(&service, &newer.slot);
        assert_eq!((fx.runs(), &fresh.key), (3, &newer.key));
        assert!(matches!(
            service.start(newer, false),
            Ok(Started::Cached(_))
        ));
        assert_eq!(fx.runs(), 3);
        // 失败的结果不算数:不带 force 也重跑
        fx.mode("garbage");
        let other = fx.job(SUMMARY, Language::English);
        assert!(
            !service
                .run_blocking(other.clone(), false)
                .unwrap()
                .record
                .ok
        );
        fx.mode("ok");
        assert!(service.run_blocking(other, false).unwrap().record.ok);
        assert_eq!(fx.runs(), 5);
    }

    #[test]
    fn data_changing_mid_run_keeps_the_slot_running_and_leaves_a_stale_result() {
        let fx = Fixture::new("claude");
        fx.mode("gate");
        let service = fx.service();
        let job = fx.job(SUMMARY, Language::Chinese);
        service.start(job.clone(), false).unwrap();
        let newer = fx.job("{\"turns\":4}", Language::Chinese);
        assert!(matches!(service.state(&newer.slot), State::Running { .. }));
        assert!(
            matches!(service.start(newer.clone(), false), Ok(Started::Running)),
            "同槽在跑:进行中,不是忙"
        );
        std::fs::write(fx.bin.path().join("release"), "").unwrap();
        let done = wait_done(&service, &newer.slot);
        assert!(done.ok && done.key == job.key && done.key != newer.key);
        assert_eq!(fx.runs(), 1);
    }

    #[test]
    fn a_failed_retry_keeps_the_previous_result_until_the_next_success() {
        let fx = Fixture::new("claude");
        let service = fx.service();
        let job = fx.job(SUMMARY, Language::Chinese);
        let first = service.run_blocking(job.clone(), false).unwrap().record;
        fx.mode("fail");
        let ran = service.run_blocking(job.clone(), true).unwrap();
        let reason = ran.error.unwrap();
        assert!(reason.contains("异常退出"), "{reason}");
        let kept = wait_done(&service, &job.slot);
        assert_eq!(kept, ran.record);
        assert!(kept.ok && kept.error.is_none());
        assert_eq!(
            (&kept.text, kept.generated_at_ms, &kept.key),
            (&first.text, first.generated_at_ms, &first.key)
        );
        let failure = kept.last_failure.unwrap();
        assert_eq!(failure.reason, reason);
        assert!(failure.at_ms >= first.generated_at_ms);
        fx.mode("ok");
        let again = service.run_blocking(job.clone(), true).unwrap().record;
        assert!(again.ok && again.last_failure.is_none());
        assert_eq!(wait_done(&service, &job.slot), again);
    }

    #[test]
    fn only_one_analysis_runs_at_a_time() {
        let fx = Fixture::new("claude");
        fx.mode("gate");
        let service = fx.service();
        let a = fx.job(SUMMARY, Language::Chinese);
        let b = fx.job(SUMMARY, Language::English);
        assert_ne!(a.slot, b.slot);
        assert!(matches!(
            service.start(a.clone(), false),
            Ok(Started::Running)
        ));
        let State::Running { since_ms } = service.state(&a.slot) else {
            panic!("第一项应在跑");
        };
        assert!(
            matches!(service.start(a.clone(), true), Ok(Started::Running)),
            "同槽在跑:返回进行中,不另起"
        );
        assert!(matches!(service.state(&a.slot), State::Running { since_ms: s } if s == since_ms));
        assert!(service.start(b.clone(), false).is_err(), "别的槽:忙");
        assert!(service.run_blocking(b.clone(), false).is_err());
        assert!(matches!(service.state(&b.slot), State::None));
        std::fs::write(fx.bin.path().join("release"), "").unwrap();
        assert_eq!(
            wait_done(&service, &a.slot).text.as_deref(),
            Some("released")
        );
        assert_eq!(fx.runs(), 1);
        assert!(matches!(
            service.start(b.clone(), false),
            Ok(Started::Running)
        ));
        wait_done(&service, &b.slot);
    }

    #[test]
    fn timeouts_and_process_failures_end_failed_without_running_left() {
        for (mode, fragment) in [
            ("sleep", "超过 1 秒"),
            ("fail", "异常退出"),
            ("garbage", "没有给出完整的结果"),
        ] {
            let fx = Fixture::new("claude");
            fx.mode(mode);
            let service = fx.service();
            service.set_timeout(Duration::from_secs(1));
            let job = fx.job(SUMMARY, Language::Chinese);
            let started = Instant::now();
            service.start(job.clone(), false).unwrap();
            let record = wait_done(&service, &job.slot);
            assert!(!record.ok && record.text.is_none(), "{mode}");
            let error = record.error.unwrap();
            assert!(error.contains(fragment), "{mode}: {error}");
            assert!(started.elapsed() < Duration::from_secs(6), "{mode}");
            assert!(lock(&service.running).is_none(), "{mode}: 不留下进行中");
        }
        // 失败原因跟随请求的语言
        let fx = Fixture::new("claude");
        fx.mode("fail");
        let ran = fx
            .service()
            .run_blocking(fx.job(SUMMARY, Language::English), false)
            .unwrap();
        assert_eq!(ran.error.as_deref(), Some("claude exited abnormally"));
        assert_eq!(
            ran.record.error, ran.error,
            "槽里没有成功结果:落一条失败记录"
        );
    }

    #[test]
    fn stopping_interrupts_the_group_with_sigint_first() {
        let fx = Fixture::new("claude");
        fx.mode("trap");
        let stop = Arc::new(AtomicBool::new(false));
        let service = fx.service().with_stop(Arc::clone(&stop));
        let job = fx.job(SUMMARY, Language::Chinese);
        service.start(job.clone(), false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        // 等脚本装好 trap 再停:信号早到一步,shell 按默认动作退出,测到的就不是「先发 SIGINT」。
        while fx.file("ready").is_empty() {
            assert!(Instant::now() < deadline, "进程没起来");
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        let record = wait_done(&service, &job.slot);
        assert!(!record.ok);
        assert!(record.error.unwrap().contains("停机"));
        assert_eq!(fx.file("signal").trim(), "int", "先收到的是 SIGINT");
    }

    #[test]
    fn codex_runs_an_ephemeral_read_only_thread_and_reports_usage() {
        let mut fx = Fixture::new("codex");
        let server = crate::test_support::write_fake_app_server(
            fx.bin.path(),
            r#"
*'config/read'*) printf '%s\n' '{"id":'"$id"',"result":{"config":{"mcp_servers":{"write-tool":{"enabled":false}}}}}' ;;
*'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"analysis-thread"}}}' ;;
*'turn/start'*)
printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"analysis-turn","status":"inProgress"}}}'
printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"analysis-thread","turnId":"analysis-turn","tokenUsage":{"total":{"totalTokens":540},"last":{"inputTokens":500,"cachedInputTokens":200,"outputTokens":40,"totalTokens":540}}}}'
printf '%s\n' '{"method":"item/completed","params":{"threadId":"analysis-thread","turnId":"analysis-turn","item":{"id":"a","type":"agentMessage","phase":"final_answer","text":"codex analysis"}}}'
printf '%s\n' '{"method":"turn/completed","params":{"threadId":"analysis-thread","turn":{"id":"analysis-turn","status":"completed"}}}' ;;
"#,
        );
        fx.use_cmd(&server);
        let job = fx.job(SUMMARY, Language::English);
        let record = fx.service().run_blocking(job, false).unwrap().record;
        assert!(record.ok, "{record:?}");
        assert_eq!(record.text.as_deref(), Some("codex analysis"));
        // 记一行轮外用量,口径同会话轮:未命中缓存的输入、记在下发的模型名下
        let rows = fx.spent();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (rows[0].agent.as_str(), rows[0].cli.as_str()),
            (fx.agent.name.as_str(), "codex")
        );
        assert_eq!(
            groups(&rows[0]),
            [("analysis-model".into(), 1, 300, 40, 200, 0, 0)]
        );
        assert_eq!(
            record.usage,
            Some(AnalysisUsage {
                input: 300,
                output: 40,
                cache_read: 200,
                cache_write: 0
            })
        );
        let log = std::fs::read_to_string(crate::test_support::fake_rpc_server_log(fx.bin.path()))
            .unwrap();
        let requests: Vec<Value> = log
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let start = requests
            .iter()
            .find(|r| r["method"] == "thread/start")
            .unwrap();
        assert_eq!(start["params"]["ephemeral"], true);
        assert_eq!(start["params"]["sandbox"], "read-only");
        assert_eq!(start["params"]["approvalPolicy"], "never");
        assert_eq!(start["params"]["config"]["features"]["shell_tool"], false);
        assert_eq!(
            start["params"]["config"]["mcp_servers"]["write-tool"]["enabled"],
            false
        );
        let turn = requests
            .iter()
            .find(|r| r["method"] == "turn/start")
            .unwrap();
        assert_eq!(turn["params"]["model"], "analysis-model");
        assert_eq!(
            turn["params"]["input"][0]["text"],
            core::compose(Language::English, SUMMARY)
        );
        assert!(!log.contains("thread/resume"));
    }

    #[test]
    fn long_text_is_clipped_and_marked() {
        let fx = Fixture::new("claude");
        fx.mode("long");
        let record = fx
            .service()
            .run_blocking(fx.job(SUMMARY, Language::Chinese), false)
            .unwrap()
            .record;
        assert!(record.ok && record.truncated);
        assert_eq!(
            record.text.unwrap().chars().count(),
            core::MAX_TEXT_CHARS + 1
        );
    }

    #[test]
    fn unsupported_clis_never_start_a_process() {
        for cli in ["cursor", "unknown-backend"] {
            let fx = Fixture::new(cli);
            let primary = PrimaryAgent {
                agent: fx.agent.clone(),
                source: PrimarySource::Configured,
            };
            let request = || Request {
                from: "2026-10-01".into(),
                to: "2026-10-30".into(),
                agent_filter: None,
                lang: Language::English,
                summary_json: SUMMARY.into(),
            };
            let Plan::Unsupported(why) = plan(Some(&primary), request()) else {
                panic!("{cli} 应不支持");
            };
            assert!(why.contains(&fx.agent.name), "{why}");
            assert!(
                matches!(plan(None, request()), Plan::Unsupported(w) if w == "No usable agent")
            );
            assert_eq!(fx.runs(), 0);
        }
    }

    #[test]
    fn slots_follow_period_filter_and_language_while_keys_follow_everything() {
        let fx = Fixture::new("claude");
        let base = fx.job(SUMMARY, Language::Chinese);
        let primary = |agent: Agent| PrimaryAgent {
            agent,
            source: PrimarySource::Usage,
        };
        let request = |from: &str, filter: Option<&str>, summary: &str| Request {
            from: from.into(),
            to: "2026-10-30".into(),
            agent_filter: filter.map(str::to_string),
            lang: Language::Chinese,
            summary_json: summary.into(),
        };
        let job = |p: &PrimaryAgent, r: Request| match plan(Some(p), r) {
            Plan::Ready(job) => *job,
            Plan::Unsupported(why) => panic!("{why}"),
        };
        let p = primary(fx.agent.clone());
        let same = job(&p, request("2026-10-01", None, SUMMARY));
        assert_eq!((&same.slot, &same.key), (&base.slot, &base.key));
        // 换槽:时间段、筛选、语言
        for other in [
            job(&p, request("2026-10-02", None, SUMMARY)),
            job(&p, request("2026-10-01", Some("x"), SUMMARY)),
            fx.job(SUMMARY, Language::English),
        ] {
            assert_ne!(other.slot, base.slot);
        }
        // 同槽、键不同(槽里的结果算过期):数据、主 agent、模型
        let mut renamed = fx.agent.clone();
        renamed.name = "other".into();
        let mut remodeled = fx.agent.clone();
        remodeled.model = Some("another-model".into());
        for other in [
            job(&p, request("2026-10-01", None, "{\"turns\":4}")),
            job(&primary(renamed), request("2026-10-01", None, SUMMARY)),
            job(&primary(remodeled), request("2026-10-01", None, SUMMARY)),
        ] {
            assert_eq!(other.slot, base.slot);
            assert_ne!(other.key, base.key);
        }
    }

    #[test]
    fn only_the_newest_results_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..KEEP + 3 {
            let path = dir.path().join(format!("{i:032x}.json"));
            std::fs::write(&path, "{}").unwrap();
            let at = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + i as u64);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(at)
                .unwrap();
        }
        std::fs::write(dir.path().join("note.txt"), "").unwrap();
        prune(dir.path(), KEEP);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(left.len(), KEEP + 1);
        assert!(!left.contains(&format!("{:032x}.json", 0)));
        assert!(left.contains(&format!("{:032x}.json", KEEP + 2)));
        assert!(left.contains(&"note.txt".to_string()));
    }

    #[test]
    fn results_are_kept_one_per_slot_up_to_keep_slots() {
        let fx = Fixture::new("claude");
        let service = fx.service();
        let job = fx.job(SUMMARY, Language::Chinese);
        let record = service.run_blocking(job, false).unwrap().record;
        for i in 0..KEEP + 2 {
            for text in ["older", "newer"] {
                service.save(&Record {
                    slot: format!("{i:032x}"),
                    text: Some(text.into()),
                    ..record.clone()
                });
            }
        }
        let files: Vec<String> = std::fs::read_dir(fx.results.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(files.len(), KEEP, "一个槽一份,按槽计数:{files:?}");
        assert!(!files.contains(&format!("{:032x}.json", 0)));
        assert!(files.contains(&format!("{:032x}.json", KEEP + 1)));
        let newest = read(fx.results.path(), &format!("{:032x}", KEEP + 1)).unwrap();
        assert_eq!(newest.text.as_deref(), Some("newer"), "同槽再写是覆盖");
    }

    #[test]
    fn primary_counts_tokens_from_turns_and_outside_usage() {
        use summon_core::turn_ledger::{ModelUsage, OutsideUsage};
        let fx_a = Fixture::new("claude");
        let fx_b = Fixture::new("codex");
        let mut a = fx_a.agent.clone();
        a.name = "alpha".into();
        let mut b = fx_b.agent.clone();
        b.name = "beta".into();
        let run = tempfile::tempdir().unwrap();
        let now = now_ms();
        let usage = |output: u64| {
            vec![ModelUsage {
                model: "m".into(),
                band: 0,
                variant: None,
                requests: 1,
                input: 0,
                output,
                cache_read: 0,
                cache_write_5m: 0,
                cache_write_1h: 0,
            }]
        };
        let none =
            primary(None, vec![a.clone(), b.clone()], &|_| true, run.path(), now).map(|p| p.source);
        assert_eq!(none, Some(PrimarySource::First));
        crate::runtime::turn_ledger::append_outside(
            &crate::runtime::turn_ledger::outside_dir_in(run.path()),
            &OutsideUsage {
                agent: "beta".into(),
                chat_id: "oc_x".into(),
                tag: "t".into(),
                cli: "codex".into(),
                from_ms: now - 60_000,
                to_ms: now - 30_000,
                usage: usage(500),
            },
        )
        .unwrap();
        let p = primary(None, vec![a.clone(), b.clone()], &|_| true, run.path(), now).unwrap();
        assert_eq!(
            (p.agent.name.as_str(), p.source),
            ("beta", PrimarySource::Usage)
        );
        // 停用的不参与;配置指向停用的就回落
        let p = primary(
            Some("beta"),
            vec![a.clone(), b.clone()],
            &|n| n != "beta",
            run.path(),
            now,
        )
        .unwrap();
        assert_eq!(
            (p.agent.name.as_str(), p.source),
            ("alpha", PrimarySource::First)
        );
        let p = primary(Some("alpha"), vec![a, b], &|_| true, run.path(), now).unwrap();
        assert_eq!(p.source, PrimarySource::Configured);
    }
}
