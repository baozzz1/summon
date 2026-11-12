//! 接口测试的夹具:临时目录里造一个 `~/.summon`(config.yaml、agents、按群状态、定时器),
//! 加一个可控的桩端口。不起 chat 线程、不连 tmux、不联网。样例数据全是虚构 ID。

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use summon_core::config::Config;
use summon_core::registry::{Agent, AppOwner, AppOwnerSlot, AppOwnerState};

use super::types::{Autostart, LarkUserView};
use super::{ApiEnv, DashInterrupt, MentionlessAsk, Port, RunningTurn};
use crate::runtime::dashboard::boot_snapshot::BootSnapshot;
use crate::runtime::dashboard::server::{ApiCtx, ApiError, Handler};
use crate::runtime::dashboard::sessions::{ChatKind, Issuer, Principal};
use crate::runtime::menubar_session::EndSessionOutcome;
use summon_core::fleet::{AgentStatus, ChatStatus, DaemonStatus, FleetSnapshot, VisibleState};

pub(crate) const ME_OPEN: &str = "ou_00000000000000000000000000000001";
pub(crate) const ME_UNION: &str = "on_00000000000000000000000000000001";
pub(crate) const PEER_UNION: &str = "on_00000000000000000000000000000002";
pub(crate) const GROUP: &str = "oc_00000000000000000000000000000001";
pub(crate) const P2P: &str = "oc_00000000000000000000000000000009";
/// lark-cli 那个应用里的 owner(与 agent 应用里的 open_id 不同)。
pub(crate) const LARK_OWNER_OPEN: &str = "ou_00000000000000000000000000000003";
/// 会话页样例里已经结束、只剩一条回复等人看的那个群。
pub(crate) const ENDED_GROUP: &str = "oc_00000000000000000000000000000002";
const SAMPLE_TARGET: &str = "sample-group-session";

pub(crate) fn ended_tag() -> String {
    summon_core::registry::chat_tag("group", ENDED_GROUP)
}

/// 样例快照里那条已结束、只剩 waiting 的会话:群 id 记在盘上的标记里(快照里没有)。
pub(crate) fn mark_sample_waiting(agent: &Agent) {
    crate::runtime::waiting_store::set(
        &agent.state_dir(),
        &ended_tag(),
        &summon_core::waiting::WaitingMark {
            chat_id: ENDED_GROUP.to_string(),
            reply_message_id: "om_reply".to_string(),
            link: String::new(),
            since_unix: 1,
        },
    )
    .unwrap();
}

pub(crate) fn sample_target() -> String {
    SAMPLE_TARGET.to_string()
}

/// 会话页样例:群里一条在跑的会话、私聊一条空闲的,另有一条已结束、只剩回复等人看的。
pub(crate) fn sample_fleet() -> FleetSnapshot {
    let chat = |chat_type: &str, chat_id: &str, name: &str, busy: bool, target: &str| ChatStatus {
        tag: summon_core::registry::chat_tag(chat_type, chat_id),
        target: target.to_string(),
        name: Some(name.to_string()),
        chat_type: Some(chat_type.to_string()),
        chat_id: Some(chat_id.to_string()),
        busy,
        capture: "transcript".to_string(),
        liveness: "live".to_string(),
        last_active_secs: Some(30),
        last_active_text: None,
        last_active_text_en: None,
        waiting: false,
        state: if busy {
            VisibleState::Working
        } else {
            VisibleState::Idle
        },
        idle_tmux: !busy,
    };
    FleetSnapshot {
        comment_tasks: Vec::new(),
        comment_tasks_error: None,
        daemon: DaemonStatus {
            running: true,
            pid: Some(4242),
            version: None,
            uptime_secs: Some(60),
        },
        agents: vec![AgentStatus {
            alias: "starlight".to_string(),
            busy: true,
            sessions: 2,
            last_active_secs: Some(30),
            last_active_text: None,
            last_active_text_en: None,
            state: VisibleState::Working,
            idle_tmux: true,
            chats: vec![
                chat("group", GROUP, "示例群", true, SAMPLE_TARGET),
                chat("p2p", P2P, "示例用户", false, "sample-p2p-session"),
            ],
        }],
        groups: Vec::new(),
        sentinel: None,
        waiting: vec![summon_core::waiting::WaitingStatus {
            alias: "starlight".to_string(),
            tag: ended_tag(),
            name: Some("另一个群".to_string()),
            chat_type: Some("group".to_string()),
            reply_message_id: "om_reply".to_string(),
            since_secs: 600,
            since_text: String::new(),
            since_text_en: String::new(),
            link: String::new(),
        }],
        menubar_icon: None,
        menubar_show_counts: None,
    }
}

/// 本机没有记录、问飞书才知道名字的人。
pub(crate) const REMOTE_KNOWN: &str = "ou_00000000000000000000000000000005";

#[derive(Default)]
pub(crate) struct TestPort {
    agents_dir: PathBuf,
    pub(crate) attention:
        Mutex<Option<Result<Vec<crate::runtime::core::AttentionRequest>, String>>>,
    pub(crate) live: Mutex<HashSet<(String, String)>>,
    pub(crate) owners: Mutex<HashMap<String, AppOwnerState>>,
    /// 飞书身份已变的 agent:判权一律按拒绝。
    pub(crate) drifted: Mutex<HashSet<String>>,
    pub(crate) written: Mutex<Vec<String>>,
    pub(crate) asks: Mutex<Vec<(String, MentionlessAsk)>>,
    /// lark-cli 没登录用户身份:开 sentinel 被拒,读登录用户也失败。
    pub(crate) sentinel_refused: AtomicBool,
    pub(crate) lark_profiles: Mutex<Vec<Option<String>>>,
    /// 真正问过飞书的 ID(桩端口不缓存,每次都算一次请求)。
    pub(crate) remote_asked: Mutex<Vec<String>>,
    pub(crate) fleet: Mutex<Option<FleetSnapshot>>,
    pub(crate) running: Mutex<HashMap<(String, String), RunningTurn>>,
    /// 停在授权 / 提问 / 登录上的会话(按会话名)。
    pub(crate) blocked: Mutex<HashMap<String, String>>,
    /// 退出 / 归档过的会话:(agent, tag, 归档, 打断也行)。
    pub(crate) ended: Mutex<Vec<(String, String, bool, bool)>>,
    pub(crate) end_outcome: Mutex<Option<EndSessionOutcome>>,
    pub(crate) interrupts: Mutex<Vec<(String, String, String)>>,
}

impl TestPort {
    pub(crate) fn set_fleet(&self, fleet: FleetSnapshot) {
        *self.fleet.lock().unwrap() = Some(fleet);
    }
}

impl TestPort {
    fn read(&self, alias: &str) -> Option<Agent> {
        let dir = self.agents_dir.join(alias);
        let text = std::fs::read_to_string(dir.join("agent.yaml")).ok()?;
        let mut agent: Agent = serde_yaml::from_str(&text).ok()?;
        agent.dir = dir;
        let slot = AppOwnerSlot::default();
        slot.set(
            self.owners
                .lock()
                .unwrap()
                .get(alias)
                .cloned()
                .unwrap_or_default(),
        );
        agent.app_owner = slot;
        Some(agent)
    }
}

impl Port for TestPort {
    fn attention_requests(&self) -> Result<Vec<crate::runtime::core::AttentionRequest>, String> {
        self.attention
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Ok(Vec::new()))
    }
    fn gate_agent(&self, alias: &str) -> Option<Agent> {
        if self.drifted.lock().unwrap().contains(alias) {
            return None;
        }
        self.read(alias)
    }

    fn chat_live(&self, agent: &Agent, _chat_type: &str, chat_id: &str) -> bool {
        self.live
            .lock()
            .unwrap()
            .contains(&(agent.name.clone(), chat_id.to_string()))
    }

    fn session_name(&self, agent: &Agent, chat_type: &str, chat_id: &str) -> String {
        agent.tmux_name_for_chat(chat_type, chat_id, false)
    }

    fn tmux_sessions(&self) -> Vec<String> {
        Vec::new()
    }

    fn agent_written(&self, alias: &str) {
        self.written.lock().unwrap().push(alias.to_string());
    }

    fn agent_idle(&self, agent: &Agent) -> Result<(), String> {
        if self
            .live
            .lock()
            .unwrap()
            .iter()
            .any(|(alias, _)| alias == &agent.name)
        {
            return Err("仍有会话".into());
        }
        Ok(())
    }
    fn retire_agent(&self, agent: &Agent) -> Result<(), String> {
        self.agent_idle(agent)
    }
    fn finish_agent_cleanup(&self, _alias: &str) {}
    fn restore_agent(&self, _alias: &str) {}

    fn mentionless(&self, alias: &str, ask: MentionlessAsk) -> Result<(), String> {
        self.asks.lock().unwrap().push((alias.to_string(), ask));
        Ok(())
    }

    fn sentinel_identity(&self, _cfg: &Config) -> Result<(), String> {
        if self.sentinel_refused.load(Ordering::SeqCst) {
            Err("lark-cli 的用户身份还没登录".into())
        } else {
            Ok(())
        }
    }

    fn lark_user(&self, _cfg: &Config, profile: Option<&str>) -> Result<LarkUserView, String> {
        self.lark_profiles
            .lock()
            .unwrap()
            .push(profile.map(str::to_string));
        if self.sentinel_refused.load(Ordering::SeqCst) {
            return Err("lark-cli 默认 profile 没有登录用户身份".into());
        }
        Ok(LarkUserView {
            open_id: LARK_OWNER_OPEN.to_string(),
            name: Some("示例用户".to_string()),
        })
    }

    fn model_candidates(&self, _agent: &Agent, from_config: Vec<String>) -> Vec<String> {
        from_config
    }

    fn member_name(&self, _agent: &Agent, id: &str) -> Option<String> {
        (id == PEER_UNION).then(|| "另一位用户".to_string())
    }

    fn remote_name(&self, _agent: &Agent, id: &str, budget: &mut usize) -> Option<String> {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        self.remote_asked.lock().unwrap().push(id.to_string());
        (id == REMOTE_KNOWN).then(|| "远端用户".to_string())
    }

    fn fleet(&self) -> Option<FleetSnapshot> {
        self.fleet.lock().unwrap().clone()
    }

    fn running_turn(&self, agent: &str, tag: &str) -> Option<RunningTurn> {
        self.running
            .lock()
            .unwrap()
            .get(&(agent.to_string(), tag.to_string()))
            .cloned()
    }

    fn blocked(&self, _agent: &Agent, session: &str) -> Option<String> {
        self.blocked.lock().unwrap().get(session).cloned()
    }

    fn end_session(
        &self,
        agent: &Agent,
        tag: &str,
        archive: bool,
        interrupt: bool,
    ) -> Result<EndSessionOutcome, String> {
        self.ended
            .lock()
            .unwrap()
            .push((agent.name.clone(), tag.to_string(), archive, interrupt));
        Ok(self
            .end_outcome
            .lock()
            .unwrap()
            .unwrap_or(EndSessionOutcome::Ended))
    }

    fn interrupt(
        &self,
        alias: &str,
        chat_id: &str,
        turn_id: &str,
    ) -> Result<DashInterrupt, String> {
        self.interrupts.lock().unwrap().push((
            alias.to_string(),
            chat_id.to_string(),
            turn_id.to_string(),
        ));
        Ok(DashInterrupt::Sent)
    }

    fn autostart(&self) -> Autostart {
        Autostart::On
    }

    fn public_host(&self) -> Option<std::net::Ipv4Addr> {
        Some(std::net::Ipv4Addr::new(10, 0, 0, 8))
    }
}

pub(crate) struct Kit {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) port: Arc<TestPort>,
    env: OnceCell<Arc<ApiEnv>>,
}

pub(crate) fn agent_yaml(alias: &str, display: &str, cli: &str, extra: &str) -> String {
    let env_prefix = alias.to_uppercase();
    format!(
        "# {display} 的配置\nname: {alias}\ndisplay: {display}\nstatus: active\nruntime:\n  cli: {cli}\n  \
working_dir: /tmp\nfeishu:\n  app_id_env: DASH_TEST_{env_prefix}_APP_ID\n  \
app_secret_env: DASH_TEST_{env_prefix}_APP_SECRET\n  scope: thread\n  allowed_users: [{ME_UNION}]\n  \
operators: [{ME_UNION}]\n{extra}"
    )
}

impl Kit {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let agents_dir = home.join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        // agents 目录由环境直接给(与 daemon 启动时定下的一样),config.yaml 里不写临时路径。
        std::fs::write(
            home.join("config.yaml"),
            "# 整机配置\nmessaging:\n  cot_visible: true  # 显示过程\n",
        )
        .unwrap();
        let port = Arc::new(TestPort {
            agents_dir,
            ..TestPort::default()
        });
        Self {
            dir,
            port,
            env: OnceCell::new(),
        }
    }

    pub(crate) fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    pub(crate) fn agents_dir(&self) -> PathBuf {
        self.home().join("agents")
    }

    /// 落一个 agent(凭据写进它自己的 `.env`)。
    pub(crate) fn add_agent(&self, alias: &str, yaml: &str) -> Agent {
        let dir = self.agents_dir().join(alias);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.yaml"), yaml).unwrap();
        let prefix = alias.to_uppercase();
        std::fs::write(
            dir.join(".env"),
            format!(
                "DASH_TEST_{prefix}_APP_ID=cli_0000000000000001\nDASH_TEST_{prefix}_APP_SECRET=s\n"
            ),
        )
        .unwrap();
        let mut agent: Agent = serde_yaml::from_str(yaml).unwrap();
        agent.dir = dir;
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        agent
    }

    /// 按 daemon 的布局落一份 chat 的 `.meta`。
    pub(crate) fn add_chat(&self, agent: &Agent, chat_type: &str, chat_id: &str, name: &str) {
        let session = agent.tmux_name_for_chat(chat_type, chat_id, false);
        let meta = summon_core::chat_meta::ChatMeta {
            chat_id: Some(chat_id.to_string()),
            chat_type: Some(chat_type.to_string()),
            chat_name: (!name.is_empty()).then(|| name.to_string()),
            tag: Some(summon_core::registry::chat_tag(chat_type, chat_id)),
            last_sender: Some(ME_OPEN.to_string()),
            updated_at: Some("2026-10-20 10:24:00".to_string()),
            creator: None,
        };
        let path = agent
            .state_dir()
            .join(summon_core::chat_meta::meta_rel_path(&session));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, meta.render()).unwrap();
    }

    pub(crate) fn set_owner(&self, alias: &str, union_id: &str) {
        self.port.owners.lock().unwrap().insert(
            alias.to_string(),
            AppOwnerState::Known(Some(AppOwner {
                open_id: String::new(),
                union_id: union_id.to_string(),
            })),
        );
    }

    /// 第一次调用时建环境,启动存档按此刻的盘上内容登记。
    pub(crate) fn env(&self) -> Arc<ApiEnv> {
        self.env
            .get_or_init(|| {
                let cfg: Config = serde_yaml::from_str(
                    &std::fs::read_to_string(self.home().join("config.yaml")).unwrap(),
                )
                .unwrap();
                let agents: Vec<Agent> = std::fs::read_dir(self.agents_dir())
                    .unwrap()
                    .flatten()
                    .filter_map(|e| {
                        let text = std::fs::read_to_string(e.path().join("agent.yaml")).ok()?;
                        let mut agent: Agent = serde_yaml::from_str(&text).ok()?;
                        agent.dir = e.path();
                        Some(agent)
                    })
                    .collect();
                let port: Arc<dyn Port> = self.port.clone();
                Arc::new(ApiEnv::new(
                    self.home(),
                    self.agents_dir(),
                    BootSnapshot::capture(&cfg, &agents),
                    port,
                ))
            })
            .clone()
    }

    pub(crate) fn owner(&self) -> ApiCtx {
        ApiCtx::for_test_env(Principal::Owner, self.env())
    }

    pub(crate) fn feishu(&self, issuer: Issuer) -> ApiCtx {
        ApiCtx::for_test_env(Principal::Feishu(issuer), self.env())
    }

    pub(crate) fn call(
        &self,
        ctx: &ApiCtx,
        handler: Handler,
        params: Value,
    ) -> Result<Value, ApiError> {
        handler(ctx, params)
    }

    pub(crate) fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.home().join(rel)).unwrap()
    }

    pub(crate) fn audit_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(self.home().join("run/dashboard-audit.ndjson"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

/// 签发人:`agent` 在 `chat_id` 上签的链接,点击人是「我」。
pub(crate) fn issuer(agent: &str, chat_id: &str, kind: ChatKind) -> Issuer {
    Issuer {
        agent: agent.to_string(),
        chat_id: chat_id.to_string(),
        chat_kind: kind,
        open_id: ME_OPEN.to_string(),
        union_id: ME_UNION.to_string(),
        name: "示例用户".to_string(),
    }
}
