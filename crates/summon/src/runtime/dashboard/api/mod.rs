//! 控制台接口的实现。页面只展示与收集输入,分层合并、生效时机、校验、写入、判权全在这里。
//!
//! 结构:[`ApiEnv`] 是一个 daemon 一份的环境(`~/.summon` 的根、启动存档、daemon 侧的端口);
//! [`Port`] 是接口实现够不着、只有 daemon 才知道的事实与动作(会话活没活着、飞书应用所有者、
//! 唤醒 dispatcher、免 @ 核验队列)。测试换一个桩端口,在临时目录里造 `~/.summon`。

pub(crate) mod agents;
pub(crate) mod attention;
pub(crate) mod chats;
pub(crate) mod cleanup;
pub(crate) mod config;
pub(crate) mod docs;
pub(crate) mod insights;
pub(crate) mod logs;
pub(crate) mod names;
pub(crate) mod preview;
pub(crate) mod schedules;
pub(crate) mod sessions;
pub(crate) mod status;
pub(crate) mod types;
pub(crate) mod usage;

#[cfg(test)]
mod contract;
#[cfg(test)]
pub(crate) mod testkit;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use summon_core::config::Config;
use summon_core::dashboard::authz::{self, AgentLists, Decision, Snapshot, Target};
use summon_core::registry::{Agent, AppOwnerSlot, AppOwnerState};

use super::boot_snapshot::BootSnapshot;
use super::server::{ApiCtx, ApiError, ErrorCode, Registry};
use super::sessions::{ChatKind as SessionChatKind, Gate, Issuer, Principal};
use super::writer::{self, WriteError};
use types::{Autostart, JobStatus, LarkUserView, MentionlessJob};

/// 控制台全部业务接口登记在这里(会话两个方法在 `server` 里)。
pub(crate) fn register(r: &mut Registry) {
    r.register("status.get", status::get);
    r.register("status.issues", status::issues);
    r.register("config.schema", config::schema);
    r.register("config.get", config::get);
    r.register("config.patch", config::patch);
    r.register("config.raw.get", config::raw_get);
    r.register("config.raw.check", config::raw_check);
    r.register("config.raw.put", config::raw_put);
    r.register("agents.list", agents::list);
    r.register("agents.get", agents::get);
    r.register("agents.set_enabled", agents::set_enabled);
    r.register("agents.cleanup.prepare", cleanup::prepare);
    r.register("agents.cleanup.status", cleanup::status);
    r.register("agents.cleanup.download", cleanup::download);
    r.register("agents.cleanup.confirm", cleanup::confirm);
    r.register("chats.list", chats::list);
    r.register("chats.get", chats::get);
    r.register("chats.set_mentionless", chats::set_mentionless);
    r.register("chats.mentionless_job", chats::mentionless_job);
    r.register("chats.set_operators", chats::set_operators);
    r.register("chats.set_work_dir", chats::set_work_dir);
    r.register("schedules.list", schedules::list);
    r.register("schedules.check_when", schedules::check_when);
    r.register("schedules.edit", schedules::edit);
    r.register("schedules.set_enabled", schedules::set_enabled);
    r.register("docs.get", docs::get);
    r.register("docs.put", docs::put);
    r.register("preview.render", preview::render);
    r.register("names.resolve", names::resolve);
    r.register("sentinel.lark_user", config::lark_user);
    r.register("comments.stop", sessions::stop_comment);
    r.register("attention.list", attention::list);
    r.register("sessions.list", sessions::list);
    r.register("sessions.get", sessions::get);
    r.register("sessions.act", sessions::act);
    r.register("turns.list", sessions::turns);
    r.register("usage.summary", usage::summary);
    r.register("insights.summary", insights::summary);
    r.register("insights.analysis.get", insights::analysis_get);
    r.register("insights.analysis.start", insights::analysis_start);
    r.register("logs.tail", logs::tail);
}

// ── daemon 侧端口 ───────────────────────────────────────────────────────

/// 免 @ 开启请求:交给该 agent 的 dispatcher,进与 `/mentionless on` 同一条核验队列。
pub(crate) struct MentionlessAsk {
    pub(crate) chat_id: String,
    pub(crate) chat_type: String,
    /// 只在签发 agent 的飞书应用内有效(核验「群里唯一的真人就是你」靠它)。
    pub(crate) open_id: String,
    pub(crate) union_id: String,
    pub(crate) reply: Sender<MentionlessDone>,
}

/// 核验队列交回来的结果。成功时已经写进 agent.yaml。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MentionlessDone {
    pub(crate) ok: bool,
    pub(crate) message: String,
    pub(crate) apply_url: Option<String>,
}

pub(crate) use crate::runtime::daemon::DashInterrupt;
use crate::runtime::menubar_session::EndSessionOutcome;

/// 此刻在跑的那一轮。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningTurn {
    pub(crate) started_at_ms: i64,
    /// 本轮凭证;斜杠命令与接管轮没有。
    pub(crate) turn_id: Option<String>,
}

/// 只有 daemon 才知道的事实与动作。
pub(crate) trait Port: Send + Sync {
    /// 现读的 agent.yaml(带 daemon 查到的飞书应用所有者);飞书身份与本代上线时不一致、或 daemon
    /// 不认识这个 agent 时为 `None` —— 判权一律按拒绝处理。
    fn attention_requests(&self) -> Result<Vec<crate::runtime::core::AttentionRequest>, String> {
        Err("Pending requests are unavailable".into())
    }
    fn gate_agent(&self, alias: &str) -> Option<Agent>;
    /// 这个 chat 有没有活会话:tmux 会话在、RPC 会话进程在,或此刻在跑一轮。
    fn chat_live(&self, agent: &Agent, chat_type: &str, chat_id: &str) -> bool;
    /// 群列表用:同 [`Port::chat_live`] 与 [`Port::session_name`],tmux 会话清单由调用方一次取好。
    /// 返回(会话名, 有没有活会话)。
    fn chat_state_among(
        &self,
        agent: &Agent,
        chat_type: &str,
        chat_id: &str,
        tmux: &[String],
    ) -> (String, bool) {
        let _ = tmux;
        (
            self.session_name(agent, chat_type, chat_id),
            self.chat_live(agent, chat_type, chat_id),
        )
    }
    /// chat 当前的会话名(按群设置文件、`.cwd` 的键),与 daemon 自己的推导同一份。
    fn session_name(&self, agent: &Agent, chat_type: &str, chat_id: &str) -> String;
    fn tmux_sessions(&self) -> Vec<String>;
    /// agent.yaml 刚被控制台改过:刷新 daemon 的快照、唤醒它的 dispatcher。
    fn agent_written(&self, alias: &str);
    fn agent_idle(&self, agent: &Agent) -> Result<(), String>;
    fn retire_agent(&self, agent: &Agent) -> Result<(), String>;
    fn finish_agent_cleanup(&self, alias: &str);
    fn restore_agent(&self, alias: &str);
    fn mentionless(&self, alias: &str, ask: MentionlessAsk) -> Result<(), String>;
    /// 开 sentinel 之前核验 lark-cli 的用户身份(与 Menu Bar 同一道检查,非交互)。
    fn sentinel_identity(&self, cfg: &Config) -> Result<(), String>;
    /// lark-cli 这份配置(`None` = 默认配置)里登录的飞书用户,与 sentinel 认 owner 同一个口径。
    fn lark_user(&self, cfg: &Config, profile: Option<&str>) -> Result<LarkUserView, String>;
    fn model_candidates(&self, agent: &Agent, from_config: Vec<String>) -> Vec<String>;
    /// 本机已知的人名(agent 的飞书身份缓存、各 agent 飞书客户端的名字缓存),不联网。
    fn member_name(&self, agent: &Agent, id: &str) -> Option<String>;
    /// 经这个 agent 的应用问飞书:群 ID 查群名,open_id / union_id 查人名。查不到为 `None`。
    /// 问过的走缓存;真要发请求时先从 `budget` 里扣一次,扣不动就不问。
    fn remote_name(&self, agent: &Agent, id: &str, budget: &mut usize) -> Option<String> {
        let _ = (agent, id, budget);
        None
    }
    /// 会话一栏:与 Menu Bar 同一份 fleet 快照。拿不到为 `None`。
    fn fleet(&self) -> Option<summon_core::fleet::FleetSnapshot>;
    /// 此刻在跑的那一轮。
    fn running_turn(&self, agent: &str, tag: &str) -> Option<RunningTurn>;
    /// claude 停在授权确认 / 提问 / 登录上的那一类(`permission` / `question` / `auth`)。
    fn blocked(&self, agent: &Agent, session: &str) -> Option<String>;
    /// 退出或归档一条会话(与 Menu Bar 同一流程:在跑且没带 `interrupt` 时不动手、回 `Running`)。
    fn end_session(
        &self,
        agent: &Agent,
        tag: &str,
        archive: bool,
        interrupt: bool,
    ) -> Result<EndSessionOutcome, String>;
    /// 打断在跑的那一轮(本轮凭证要对得上)。
    fn interrupt(&self, alias: &str, chat_id: &str, turn_id: &str)
        -> Result<DashInterrupt, String>;
    fn autostart(&self) -> Autostart;
    /// 此刻收件设备能路由到的本机地址(与文件预览出链接时现查的同一个)。
    fn public_host(&self) -> Option<std::net::Ipv4Addr>;
}

// ── 环境 ────────────────────────────────────────────────────────────────

/// 免 @ 核验任务。
enum Job {
    Pending {
        rx: Receiver<MentionlessDone>,
        since: Instant,
    },
    Finished {
        view: MentionlessJob,
        at: Instant,
    },
}

/// 核验最长等这么久(队列里 10 秒有效,加上联网);过了就当失败。
const JOB_TIMEOUT: Duration = Duration::from_secs(60);
/// 结束了的任务留这么久供页面轮询。
const JOB_KEEP: Duration = Duration::from_secs(600);

pub(crate) struct ApiEnv {
    /// `~/.summon`。
    pub(crate) home: PathBuf,
    /// daemon 启动时的 `agents_dir`(改它要重启)。
    pub(crate) agents_dir: PathBuf,
    pub(crate) boot: BootSnapshot,
    pub(crate) started_at: chrono::DateTime<chrono::Local>,
    pub(crate) port: Arc<dyn Port>,
    jobs: Mutex<HashMap<String, Job>>,
    /// 说明文件的写锁。写者都在 daemon 进程内(手改不持锁,由提交前的复核挡住)。
    docs_lock: Mutex<()>,
    pub(crate) cleanup: Mutex<HashMap<String, Arc<cleanup::Job>>>,
    pub(crate) operations: std::sync::RwLock<()>,
    /// 洞察分析:全机同一时刻最多一个在跑,结果落 `run/insights-analysis/`。
    pub(crate) analysis: crate::runtime::insights_analysis::Service,
}

/// 盘上一个 agent 目录。
pub(crate) enum Loaded {
    Ok(Box<Agent>),
    Broken { alias: String, error: String },
}

/// 当前 config.yaml。
pub(crate) struct ConfigFile {
    pub(crate) text: Option<String>,
    pub(crate) revision: String,
    pub(crate) parsed: Config,
    /// 解析失败的原因(此时 `parsed` 是内置默认)。
    pub(crate) error: Option<String>,
    /// 原文的通用值(判断「这一层写没写某个键」);读不出为 null。
    pub(crate) raw: serde_yaml::Value,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ApiEnv {
    pub(crate) fn new(
        home: PathBuf,
        agents_dir: PathBuf,
        boot: BootSnapshot,
        port: Arc<dyn Port>,
    ) -> Self {
        Self {
            agents_dir,
            boot,
            started_at: chrono::Local::now(),
            port,
            jobs: Mutex::new(HashMap::new()),
            docs_lock: Mutex::new(()),
            cleanup: Mutex::new(HashMap::new()),
            operations: std::sync::RwLock::new(()),
            analysis: crate::runtime::insights_analysis::Service::new(
                home.join("run").join("insights-analysis"),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ),
            home,
        }
    }

    /// daemon 的停机标志:置位时中止在跑的洞察分析。
    pub(crate) fn with_analysis_stop(mut self, stop: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.analysis = self.analysis.with_stop(stop);
        self
    }

    pub(crate) fn config_path(&self) -> PathBuf {
        self.home.join("config.yaml")
    }

    pub(crate) fn run_dir(&self) -> PathBuf {
        self.home.join("run")
    }

    fn audit_path(&self) -> PathBuf {
        self.run_dir().join("dashboard-audit.ndjson")
    }

    pub(crate) fn schedule_store(&self) -> crate::schedule::schedule_store::ScheduleStore {
        crate::schedule::schedule_store::ScheduleStore::new(&self.home)
    }

    pub(crate) fn read_config(&self) -> Result<ConfigFile, ApiError> {
        let text = writer::read_optional(&self.config_path()).map_err(internal)?;
        let revision = writer::revision(text.as_deref().map(str::as_bytes));
        let (parsed, error) = match text.as_deref() {
            None => (Config::default(), None),
            Some(t) => match serde_yaml::from_str::<Config>(t) {
                Ok(cfg) => (cfg, None),
                Err(e) => (Config::default(), Some(e.to_string())),
            },
        };
        let raw = text
            .as_deref()
            .and_then(|t| serde_yaml::from_str(t).ok())
            .unwrap_or(serde_yaml::Value::Null);
        Ok(ConfigFile {
            text,
            revision,
            parsed,
            error,
            raw,
        })
    }

    /// 扫 agents 目录:读得出的与读不出的都列出来(坏了的那份要能在原文模式里修)。
    pub(crate) fn scan(&self) -> Vec<Loaded> {
        let Ok(entries) = std::fs::read_dir(&self.agents_dir) else {
            return Vec::new();
        };
        let mut out: Vec<(String, Loaded)> = Vec::new();
        for entry in entries.flatten() {
            let dir = entry.path();
            let manifest = dir.join("agent.yaml");
            if !manifest.is_file() {
                continue;
            }
            let alias = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let loaded = match std::fs::read_to_string(&manifest)
                .map_err(|e| e.to_string())
                .and_then(|text| serde_yaml::from_str::<Agent>(&text).map_err(|e| e.to_string()))
            {
                Ok(mut agent) => {
                    agent.dir = dir;
                    agent.global_config_path = Some(self.config_path());
                    self.attach_app_owner(&mut agent);
                    Loaded::Ok(Box::new(agent))
                }
                Err(error) => Loaded::Broken {
                    alias: alias.clone(),
                    error,
                },
            };
            let key = match &loaded {
                Loaded::Ok(agent) => agent.name.clone(),
                Loaded::Broken { alias, .. } => alias.clone(),
            };
            out.push((key, loaded));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.into_iter().map(|(_, loaded)| loaded).collect()
    }

    /// 读得出的 agent。
    pub(crate) fn agents(&self) -> Vec<Agent> {
        self.scan()
            .into_iter()
            .filter_map(|l| match l {
                Loaded::Ok(agent) => Some(*agent),
                Loaded::Broken { .. } => None,
            })
            .collect()
    }

    /// 按别名取一个 agent(现读盘)。没有这个 agent 回 not_found,读不出回 invalid。
    pub(crate) fn agent(&self, alias: &str) -> Result<Agent, ApiError> {
        for loaded in self.scan() {
            match loaded {
                Loaded::Ok(agent) if agent.name == alias => return Ok(*agent),
                Loaded::Broken { alias: a, error } if a == alias => {
                    return Err(ApiError::new(
                        ErrorCode::Invalid,
                        summon_i18n::format!("rc870b8a8d7"),
                    ))
                }
                _ => {}
            }
        }
        Err(ApiError::new(
            ErrorCode::NotFound,
            summon_i18n::format!("r487205d295"),
        ))
    }

    /// agent.yaml 的路径(读不出的那份也给:原文模式要能打开它)。
    pub(crate) fn manifest_path(&self, alias: &str) -> Result<PathBuf, ApiError> {
        if alias.is_empty() || alias.contains(['/', '\\']) || alias == "." || alias == ".." {
            return Err(ApiError::new(
                ErrorCode::Invalid,
                summon_i18n::text!("rf4fc8ca9ea"),
            ));
        }
        for loaded in self.scan() {
            match loaded {
                Loaded::Ok(agent) if agent.name == alias => return Ok(agent.manifest_path()),
                Loaded::Broken { alias: a, .. } if a == alias => {
                    return Ok(self.agents_dir.join(alias).join("agent.yaml"))
                }
                _ => {}
            }
        }
        Err(ApiError::new(
            ErrorCode::NotFound,
            summon_i18n::format!("r487205d295"),
        ))
    }

    /// 应用所有者只有 daemon 查得到;换一个新槽,别写到 daemon 正在用的那个。
    fn attach_app_owner(&self, agent: &mut Agent) {
        let owner = self
            .port
            .gate_agent(&agent.name)
            .map(|a| a.app_owner.get())
            .unwrap_or(AppOwnerState::Unknown);
        let slot = AppOwnerSlot::default();
        slot.set(owner);
        agent.app_owner = slot;
    }

    /// availability 的启停状态。读不出按启用算，避免放行停用 agent 的清理。
    pub(crate) fn enabled(&self, alias: &str) -> bool {
        crate::runtime::agent_availability::AgentAvailability::lock(
            &self.run_dir().join("inbox"),
            alias,
        )
        .and_then(|gate| gate.enabled())
        .unwrap_or(true)
    }

    /// 已经扫过一遍 agent 时,用它们建判权快照(省一次扫盘)。
    pub(crate) fn snapshot_of(&self, agents: &[Agent]) -> Snapshot {
        Snapshot {
            agents: agents
                .iter()
                .map(|a| AgentLists::from_agent(a, self.enabled(&a.name)))
                .collect(),
        }
    }

    pub(crate) fn decide(&self, ctx: &ApiCtx, target: &Target<'_>) -> Decision {
        let mut agents = Vec::new();
        let mut broken = Vec::new();
        for loaded in self.scan() {
            match loaded {
                Loaded::Ok(agent) => agents.push(*agent),
                Loaded::Broken { alias, .. } => broken.push(alias),
            }
        }
        // 整机要求对每个 agent 都有明确授权;读不出的那份证明不了,飞书身份就不能改整机。
        if matches!(target, Target::Machine)
            && !matches!(ctx.principal, Principal::Owner)
            && !broken.is_empty()
        {
            return Decision::Deny(summon_i18n::format!("r759127a3ac", broken.join("、")));
        }
        authz::decide(
            &authz_principal(&ctx.principal),
            target,
            &self.snapshot_of(&agents),
        )
    }

    /// 改不了就回 forbidden。
    pub(crate) fn require(&self, ctx: &ApiCtx, target: &Target<'_>) -> Result<(), ApiError> {
        match self.decide(ctx, target) {
            Decision::Allow => Ok(()),
            Decision::Deny(why) => Err(ApiError::new(ErrorCode::Forbidden, why)),
        }
    }

    pub(crate) fn audit(&self, ctx: &ApiCtx, file: &str, keys: &[String], result: &str) {
        super::audit::record(&self.audit_path(), &ctx.principal, file, keys, result);
    }

    pub(crate) fn restart_pending(&self) -> Vec<types::RestartItem> {
        let cfg = self
            .read_config()
            .map(|c| c.parsed)
            .unwrap_or_else(|_| Config::default());
        self.boot.restart_pending(&cfg, &self.agents())
    }

    pub(crate) fn docs_lock(&self) -> MutexGuard<'_, ()> {
        lock(&self.docs_lock)
    }

    // ── 免 @ 核验任务 ──

    pub(crate) fn add_job(&self, rx: Receiver<MentionlessDone>) -> Result<String, ApiError> {
        let id = crate::runtime::preview_server::new_token();
        if id.is_empty() {
            return Err(ApiError::new(
                ErrorCode::Internal,
                summon_i18n::text!("r2e6a156880"),
            ));
        }
        let mut jobs = lock(&self.jobs);
        jobs.retain(|_, job| match job {
            Job::Finished { at, .. } => at.elapsed() < JOB_KEEP,
            Job::Pending { since, .. } => since.elapsed() < JOB_KEEP,
        });
        jobs.insert(
            id.clone(),
            Job::Pending {
                rx,
                since: Instant::now(),
            },
        );
        Ok(id)
    }

    pub(crate) fn finished_job(&self, view: MentionlessJob) {
        lock(&self.jobs).insert(
            view.job.clone(),
            Job::Finished {
                view,
                at: Instant::now(),
            },
        );
    }

    pub(crate) fn poll_job(&self, id: &str) -> Option<MentionlessJob> {
        let mut jobs = lock(&self.jobs);
        let job = jobs.get_mut(id)?;
        let view = match job {
            Job::Finished { view, .. } => return Some(view.clone()),
            Job::Pending { rx, since } => match rx.try_recv() {
                Ok(done) => MentionlessJob {
                    job: id.to_string(),
                    status: if done.ok {
                        JobStatus::Done
                    } else {
                        JobStatus::Failed
                    },
                    message: Some(done.message),
                    apply_url: done.apply_url,
                },
                Err(std::sync::mpsc::TryRecvError::Disconnected) => MentionlessJob {
                    job: id.to_string(),
                    status: JobStatus::Failed,
                    message: Some(summon_i18n::text!("r47a3cf4376").into()),
                    apply_url: None,
                },
                Err(std::sync::mpsc::TryRecvError::Empty) if since.elapsed() > JOB_TIMEOUT => {
                    MentionlessJob {
                        job: id.to_string(),
                        status: JobStatus::Failed,
                        message: Some(summon_i18n::text!("r03c161b812").into()),
                        apply_url: None,
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return Some(MentionlessJob {
                        job: id.to_string(),
                        status: JobStatus::Pending,
                        message: None,
                        apply_url: None,
                    })
                }
            },
        };
        *job = Job::Finished {
            view: view.clone(),
            at: Instant::now(),
        };
        Some(view)
    }
}

/// 每个请求重判进入权限:按签发 agent 现读的名单,在签发 chat 上判 operate-gate。
impl Gate for ApiEnv {
    fn still_allowed(&self, issuer: &Issuer) -> bool {
        let Some(agent) = self.port.gate_agent(&issuer.agent) else {
            return false;
        };
        let lists = AgentLists::from_agent(&agent, true);
        authz::may_enter(
            &authz_principal(&Principal::Feishu(issuer.clone())),
            &Snapshot {
                agents: vec![lists],
            },
        )
        .is_allowed()
    }
}

pub(crate) fn authz_principal(principal: &Principal) -> authz::Principal {
    match principal {
        Principal::Owner => authz::Principal::Owner,
        Principal::Feishu(issuer) => authz::Principal::Feishu {
            issuer_agent: issuer.agent.clone(),
            issuer_chat_id: issuer.chat_id.clone(),
            issuer_is_group: issuer.chat_kind == SessionChatKind::Group,
            open_id: issuer.open_id.clone(),
            union_id: issuer.union_id.clone(),
        },
    }
}

// ── 小工具 ──────────────────────────────────────────────────────────────

pub(crate) fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorCode::Internal, e.to_string())
}

pub(crate) fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

/// 请求参数 → 结构;形状不对回 invalid。
pub(crate) fn params<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|e| invalid(summon_i18n::format!("r51a9689e43")))
}

pub(crate) fn reply<T: Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(internal)
}

impl From<WriteError> for ApiError {
    fn from(e: WriteError) -> Self {
        let message = e.to_string();
        match e {
            WriteError::Conflict => ApiError::new(ErrorCode::Conflict, message),
            WriteError::Invalid { key, .. } => {
                let error = ApiError::new(ErrorCode::Invalid, message);
                match key {
                    Some(key) => error.with_key(key),
                    None => error,
                }
            }
            WriteError::Refused(_) => ApiError::new(ErrorCode::Forbidden, message),
            WriteError::Busy(_) => ApiError::new(ErrorCode::Busy, message),
            WriteError::Io(_) => ApiError::new(ErrorCode::Internal, message),
        }
    }
}

/// 审计里的结果:成功写 `ok`,失败写错误码。
pub(crate) fn outcome<T>(result: &Result<T, ApiError>) -> String {
    match result {
        Ok(_) => "ok".to_string(),
        Err(e) => serde_json::to_value(e.code)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "error".to_string()),
    }
}

/// 展示用的路径:家目录折成 `~`。
pub(crate) fn shown_path(path: &Path) -> String {
    summon_core::registry::collapse_tilde(&path.to_string_lossy())
}

/// 本地时区的 RFC3339(秒)。
pub(crate) fn rfc3339(t: chrono::DateTime<chrono::Local>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

pub(crate) fn rfc3339_system(t: std::time::SystemTime) -> String {
    rfc3339(chrono::DateTime::<chrono::Local>::from(t))
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::runtime::dashboard::sessions::ChatKind;
    use crate::runtime::dashboard::Dashboard;
    use crate::runtime::http_conn::Request;

    const HOST: &str = "127.0.0.1:7767";
    const ORIGIN: &str = "http://127.0.0.1:7767";

    fn request(target: &str, cookie: &str, csrf: Option<&str>) -> Request {
        let mut headers = vec![
            ("Host".to_string(), HOST.to_string()),
            ("Origin".to_string(), ORIGIN.to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Cookie".to_string(), cookie.to_string()),
        ];
        if let Some(csrf) = csrf {
            headers.push(("X-Summon-Csrf".to_string(), csrf.to_string()));
        }
        Request {
            method: "POST".to_string(),
            target: target.to_string(),
            headers,
            rest: Vec::new(),
        }
    }

    fn body(reply: &crate::runtime::http_conn::Reply<'_>) -> Value {
        serde_json::from_slice(&reply.body).unwrap()
    }

    /// 接上环境之后,业务接口走的是同一道 Origin / 会话 / CSRF 闸,飞书会话每次请求按盘上名单重判。
    #[test]
    fn attached_env_serves_methods_and_rechecks_lists_on_disk() {
        let kit = Kit::new();
        let agent = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&agent, "group", GROUP, "示例群");
        let dash = Dashboard::unbound_for_test(
            &summon_core::config::Dashboard::default(),
            &summon_core::config::PreviewServer::default(),
            HOST.parse().unwrap(),
            || None,
        );
        let unattached = dash.with_state(|s| {
            let code = s
                .issue_local(Instant::now(), || Some("a".repeat(32)))
                .unwrap();
            s.consume_local(&code, Instant::now(), {
                let mut n = 0;
                move || {
                    n += 1;
                    Some(format!("{n:032x}"))
                }
            })
            .unwrap()
        });
        let cookie = format!("summon_dash={unattached}");
        let csrf = format!("{:032x}", 2);
        let reply = crate::runtime::dashboard::server::handle(
            &dash,
            &request("/api/status.get", &cookie, Some(&csrf)),
            b"{}",
            true,
        );
        assert_eq!(body(&reply)["error"]["code"], "unavailable", "环境没接上");

        dash.attach_env(kit.env());
        let reply = crate::runtime::dashboard::server::handle(
            &dash,
            &request("/api/config.schema", &cookie, Some(&csrf)),
            b"{}",
            true,
        );
        assert_eq!(body(&reply)["ok"], true);

        // 飞书会话:撤掉名单里的我,下一个请求就作废会话。
        let issuer = issuer("starlight", GROUP, ChatKind::Group);
        let feishu = dash.with_state(|s| {
            let link = s
                .issue_link(issuer.clone(), Instant::now(), || Some("b".repeat(32)))
                .unwrap();
            let mut n = 10;
            match s.redeem_link(&link, None, Instant::now(), move || {
                n += 1;
                Some(format!("{n:032x}"))
            }) {
                crate::runtime::dashboard::sessions::Entry::Granted(secret) => secret,
                other => panic!("{other:?}"),
            }
        });
        let cookie = format!("summon_dash={feishu}");
        let csrf = format!("{:032x}", 12);
        let ok = crate::runtime::dashboard::server::handle(
            &dash,
            &request("/api/agents.list", &cookie, Some(&csrf)),
            b"{}",
            true,
        );
        assert_eq!(body(&ok)["ok"], true, "{}", body(&ok));
        let yaml = agent_yaml("starlight", "星光", "claude", "").replace(
            &format!("allowed_users: [{ME_UNION}]\n  operators: [{ME_UNION}]"),
            &format!("allowed_users: [{PEER_UNION}]\n  operators: [{PEER_UNION}]"),
        );
        std::fs::write(agent.manifest_path(), yaml).unwrap();
        let revoked = crate::runtime::dashboard::server::handle(
            &dash,
            &request("/api/agents.list", &cookie, Some(&csrf)),
            b"{}",
            true,
        );
        assert_eq!(body(&revoked)["error"]["code"], "unauthenticated");
    }

    #[test]
    fn gate_fails_closed_when_identity_drifted_or_agent_unknown() {
        let kit = Kit::new();
        kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        let env = kit.env();
        let me = issuer("starlight", GROUP, ChatKind::Group);
        assert!(env.still_allowed(&me));
        kit.port.drifted.lock().unwrap().insert("starlight".into());
        assert!(!env.still_allowed(&me), "飞书身份变了:不放行");
        assert!(!env.still_allowed(&issuer("nobody", GROUP, ChatKind::Group)));
    }

    #[test]
    fn mentionless_jobs_finish_once_and_time_out() {
        let kit = Kit::new();
        let env = kit.env();
        let (tx, rx) = std::sync::mpsc::channel();
        let id = env.add_job(rx).unwrap();
        assert_eq!(env.poll_job(&id).unwrap().status, JobStatus::Pending);
        drop(tx);
        let failed = env.poll_job(&id).unwrap();
        assert_eq!(failed.status, JobStatus::Failed, "发送端没了:核验中断");
        assert_eq!(env.poll_job(&id).unwrap(), failed, "结束后结果不变");
        assert!(env.poll_job("nope").is_none());
    }
}
