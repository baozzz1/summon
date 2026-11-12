//! 运行期把新建的 agent 接上线 —— 不重启 daemon。
//!
//! why:`summon onboard` 建完 agent 后要让它能收飞书消息,以前只能 `daemon restart`。而
//! `daemon stop` 会 SIGTERM 掉**所有**在跑的无头子进程组(见 CLAUDE.md 硬规则),等于为了让第 N 个
//! agent 上线,打断另外 N-1 个正在干活的 agent,还连带丢掉 daemon 全部内存态(闸门挂起表、
//! 在跑 run 注册表、卡片锁)。agent 数量越多,这个代价越荒谬。
//!
//! 接一个 agent 上线要动的东西,按「不接会怎样」排:
//! - 三张路由表(`Core::admit_agent`):不接 → 飞书事件找不到 dispatcher,消息直接丢;
//! - operate-gate 快照(同上,`admit_agent` 里一并做):不接 → 破坏性动作 fail-closed 被拒;
//! - dispatcher 线程:不接 → 通道没人收;
//! - outbox 投递句柄表:不接 → 它的退出回执一路重试到死信,**静默**丢;
//! - scheduler 的 delivery scope 表:不接 → 它的定时器 tick 被静默跳过;
//! - sidecar:app 列表是启动时经 stdin 一次性下发的,不重启 → 新 app 的飞书长连接根本不存在。
//!
//! 最后一条决定了「完全不重启任何进程」做不到,但重启 sidecar ≪ 重启 daemon:前者只断几秒长连接
//! (而 daemon 重启本来也会连带重启 sidecar),后者要打断在跑的轮 + 重建全部状态。
//!
//! 已停用且会话结束的 agent 可经 Dashboard 清理屏障注销，路由与后台投递同步移除。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock, Weak};
use std::thread;

use summon_core::registry::{Agent, Registry};

use crate::observe::log::log;
use crate::runtime::core::Core;
use crate::runtime::daemon::boot::SidecarControl;
use crate::runtime::daemon::outbox_drain::DrainAgents;
use crate::runtime::daemon::scheduler::DeliveryScopes;
use crate::runtime::inflight::InFlight;

/// 接新 agent 上线所需的全部上下文。daemon 启动时装配一次,之后每次 `ReloadAgents` 复用。
///
/// 持 `Weak<Core>` 而不是 `Arc`:Core 反过来持有本结构(Intent 要能调它),用 Arc 会成引用环、
/// daemon 停机后 Core 永不析构。upgrade 失败 = Core 已析构 = daemon 正在退,此时拒绝上线是对的。
pub(crate) struct AgentAdmitter {
    core: Weak<Core>,
    agents_dir: PathBuf,
    default_domain: String,
    poll_ms: u64,
    debounce_ms: u64,
    inflight: InFlight,
    drain_agents: DrainAgents,
    scheduler_scopes: DeliveryScopes,
    sidecar: Arc<SidecarControl>,
    retiring: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// 一次 reload 的结果。`admitted` 为空且 `already_known` 非空 = 盘上没有新东西(幂等重跑)。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AdmitReport {
    pub(crate) admitted: Vec<String>,
    pub(crate) already_known: usize,
    /// 有新 agent 上线时是否成功要求 sidecar 重启(false = 它此刻没在跑,需人工 `daemon restart`)。
    pub(crate) sidecar_restarted: bool,
}

impl AgentAdmitter {
    #[allow(clippy::too_many_arguments)] // 各参数是互不相关的运行时句柄,合并成结构反而更绕
    pub(super) fn new(
        core: Weak<Core>,
        agents_dir: PathBuf,
        default_domain: String,
        poll_ms: u64,
        debounce_ms: u64,
        inflight: InFlight,
        drain_agents: DrainAgents,
        scheduler_scopes: DeliveryScopes,
        sidecar: Arc<SidecarControl>,
    ) -> Self {
        Self {
            core,
            agents_dir,
            default_domain,
            poll_ms,
            debounce_ms,
            inflight,
            drain_agents,
            scheduler_scopes,
            sidecar,
            retiring: std::sync::Mutex::default(),
        }
    }

    /// 扫盘,把「registry 里可路由、但 Core 还不认识」的 agent 逐个接上线。
    /// 幂等:没有新 agent 就什么都不做(也不重启 sidecar —— 白断一次长连接是纯损失)。
    pub(crate) fn admit_new(&self) -> anyhow::Result<AdmitReport> {
        let retiring = self
            .retiring
            .lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("rb56e55c27d")))?;
        let core = self
            .core
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rf0b69a37b3")))?;
        let (reg, warnings) = Registry::load_tolerant(&self.agents_dir)?;
        let reg = reg.with_global_config_path(&summon_core::config::config_path());
        for w in warnings {
            log(&summon_i18n::format!("r3890f694b3"));
        }
        let known: std::collections::HashSet<String> =
            core.known_agent_aliases().into_iter().collect();
        let mut report = AdmitReport::default();
        for agent in reg.routable() {
            if retiring.contains(&agent.name) {
                continue;
            }
            if known.contains(&agent.name) {
                report.already_known += 1;
                continue;
            }
            if self.admit_one(agent, &core) {
                report.admitted.push(agent.name.clone());
            }
        }
        if !report.admitted.is_empty() {
            // sidecar 拿的是启动时那份 app 列表,新 app 的长连接只能靠重开进程建立。
            report.sidecar_restarted = self.sidecar.request_restart();
            if !report.sidecar_restarted {
                log(summon_i18n::text!("r0500ed1750"));
            }
        }
        Ok(report)
    }

    pub(crate) fn retire(&self, agent: &Agent) -> anyhow::Result<()> {
        let mut retiring = self
            .retiring
            .lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("rb56e55c27d")))?;
        anyhow::ensure!(
            retiring.insert(agent.name.clone()),
            summon_i18n::format!("r8c6ae33637")
        );
        let core = self
            .core
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rd330f7ff82")))?;
        anyhow::ensure!(
            !self.inflight.has_agent(&agent.name),
            summon_i18n::format!("r9930544943")
        );
        core.retire_routes(agent)?;
        // 注销后新投递不能取得句柄；持久 pending 记录由清理前复验负责拦截。
        self.drain_agents
            .write()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("rcc91236868")))?
            .remove(&agent.name);
        self.scheduler_scopes
            .write()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r5b309a40dc")))?
            .remove(&agent.name);
        crate::runtime::agent_retirement::retire(&agent.dir);
        crate::runtime::claude_usage::forget_agent(&agent.name);
        self.sidecar.quiesce()?;
        Ok(())
    }

    pub(crate) fn finish_retirement(&self, alias: &str) {
        self.retiring
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(alias);
    }

    pub(crate) fn refresh_sidecar(&self) {
        self.sidecar.request_restart();
    }

    /// 接一个 agent 上线。返回 false = 路由表里已有同名(上游算错增量集),此时**不做任何副作用**。
    fn admit_one(&self, agent: &Agent, core: &Arc<Core>) -> bool {
        // 与启动路径同序:先铺常驻要求文件,再清上一代 current-turn 残留。清理失败只记日志——
        // 启动路径拒绝启动是因为「整个 daemon 的一致性」,这里失败只影响这一个新 agent,
        // 不该连累已经在跑的其它 agent。
        let _ = crate::runtime::daemon::ensure_instruction_file(&agent.dir.join("CLAUDE.md"));
        if let Err(e) =
            crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
                .clear_on_daemon_boot()
        {
            log(&summon_i18n::format!("r286417619b", agent.name));
        }
        let (app_id, app_secret) = crate::runtime::daemon::resolve_creds(agent);
        let epoch = crate::runtime::daemon::FeishuEpochIdentity::from_resolved(
            agent,
            app_id.as_deref(),
            app_secret.as_deref(),
        );
        let dry = app_id.is_none() || app_secret.is_none();
        let fc = Arc::new(
            crate::frontend::client::FeishuClient::new(
                &self.default_domain,
                app_id.clone().unwrap_or_default(),
                app_secret.unwrap_or_default(),
                dry,
            )
            .with_email_mask(crate::runtime::daemon::agent_env::email_mask_switch(agent)),
        );
        let (tx, rx) = channel();
        // 路由 + gate 快照必须先成功:失败(重名/锁中毒)就地返回,别留下没人收的 channel 和
        // 半挂进 drain 表的孤儿句柄。
        crate::runtime::app_owner::load_cached(agent, app_id.as_deref());
        if !core.admit_agent(agent, app_id.as_deref(), tx.clone()) {
            return false;
        }
        crate::runtime::app_owner::spawn_refresher(
            agent.clone(),
            Arc::clone(&fc),
            app_id.clone(),
            dry,
        );
        if let Ok(mut drain) = self.drain_agents.write() {
            drain.insert(
                agent.name.clone(),
                crate::runtime::daemon::OutboxDrainAgent {
                    agent: agent.clone(),
                    client: Arc::clone(&fc),
                    epoch: epoch.clone(),
                },
            );
        }
        if let Ok(mut scopes) = self.scheduler_scopes.write() {
            scopes.insert(
                agent.name.clone(),
                crate::runtime::daemon::feishu_delivery_scope(agent),
            );
        }
        let (agent_owned, poll, deb) = (agent.clone(), self.poll_ms, self.debounce_ms);
        let core_d = Arc::clone(core);
        let inflight_d = self.inflight.clone();
        thread::spawn(move || {
            crate::runtime::daemon::agent_dispatcher(
                agent_owned,
                poll,
                deb,
                fc,
                epoch,
                rx,
                tx,
                core_d,
                inflight_d,
            )
        });
        log(&summon_i18n::format!("r2e8619524e", agent.name));
        true
    }
}

/// 供 `Core` 存放的句柄类型别名(Core 只认这个,不依赖 daemon 内部结构细节)。
pub(crate) type SharedAdmitter = Arc<AgentAdmitter>;

/// 空表构造:启动路径先建表再交给各线程,这里统一入口避免每处写 `Arc::new(RwLock::new(...))`。
pub(super) fn shared_drain_agents(
    init: HashMap<String, crate::runtime::daemon::OutboxDrainAgent>,
) -> DrainAgents {
    Arc::new(RwLock::new(init))
}

pub(super) fn shared_delivery_scopes(init: HashMap<String, String>) -> DeliveryScopes {
    Arc::new(RwLock::new(init))
}
