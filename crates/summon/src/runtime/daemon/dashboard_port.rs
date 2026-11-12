//! 控制台(`runtime::dashboard`)够不着的 daemon 侧事实与动作。
//!
//! 两类:一是按群状态文件的读写入口(与 `/set`、`/cd`、面板共用同一组函数与锁),只碰盘、不碰
//! 进程,接口测试在临时目录里直接用;二是 [`DaemonPort`],把会话存活、应用所有者、唤醒 dispatcher、
//! 免 @ 核验队列这些只有 daemon 才有的东西交给控制台。

use std::path::PathBuf;
use std::sync::Arc;

use summon_core::config::Config;
use summon_core::registry::{chat_tag, Agent};
use summon_core::settings::{ChatOverrides, SettingKey};

use super::{mentionless_setup, DispatchMsg, Inbound};
use crate::runtime::dashboard::api::types::{Autostart, LarkUserView};
use crate::runtime::dashboard::api::{DashInterrupt, MentionlessAsk, Port, RunningTurn};
use crate::runtime::menubar_session::EndSessionOutcome;

// ── 按群状态文件 ────────────────────────────────────────────────────────

/// 按群开关文件(`/set` 写的那份)。
pub(crate) fn chat_settings_path(agent: &Agent, session: &str) -> PathBuf {
    super::chat_meta::chat_settings_path(agent, session)
}

/// 读按群开关(认不出的项跳过)。
pub(crate) fn read_chat_settings(agent: &Agent, session: &str) -> ChatOverrides {
    super::chat_meta::read_chat_settings(agent, session)
}

/// 锁内确认文件仍是 `unchanged` 认可的那一版再改;对不上返回 `Ok(None)`。
pub(crate) fn update_chat_settings_if(
    agent: &Agent,
    session: &str,
    changes: &[(SettingKey, Option<bool>)],
    unchanged: impl FnOnce(Option<&[u8]>) -> bool,
) -> anyhow::Result<Option<ChatOverrides>> {
    super::chat_meta::update_chat_settings_if(agent, session, changes, unchanged)
}

/// `/cd` 设的本群工作目录覆盖。
pub(crate) fn chat_cwd_override(agent: &Agent, session: &str) -> Option<String> {
    super::chat_meta::read_chat_cwd(agent, session)
}

/// 清掉 `/cd` 的覆盖。返回是否真删了文件。
pub(crate) fn clear_chat_cwd(agent: &Agent, session: &str) -> std::io::Result<bool> {
    match std::fs::remove_file(super::chat_meta::chat_cwd_path(agent, session)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// 免 @ 的行内改写(与 `/mentionless on|off` 同一个函数)。
pub(crate) fn mentionless_patch(
    manifest: &str,
    chat_id: &str,
    enabled: bool,
) -> anyhow::Result<String> {
    mentionless_setup::patch(manifest, chat_id, enabled)
}

// ── daemon 端口 ─────────────────────────────────────────────────────────

/// 此刻在跑一轮,或 RPC 会话进程在。
fn turn_or_process_live(agent: &Agent, chat_type: &str, chat_id: &str) -> bool {
    use crate::observe::runtime_status::{busy_turns, live_headless};
    let tag = chat_tag(chat_type, chat_id);
    busy_turns()
        .iter()
        .any(|turn| turn.agent == agent.name && turn.tag == tag)
        || live_headless()
            .iter()
            .any(|live| live.agent == agent.name && live.tag == tag)
}

/// 这个 chat 可能用过的全部会话名:盘上 `.meta` 记过的,加按规则推导的两种。
fn session_names(
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    is_live: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut names: Vec<String> =
        super::chat_meta::chat_sessions_on_disk_by(agent, chat_id, is_live)
            .into_iter()
            .map(|(session, _, _)| session)
            .collect();
    names.push(agent.tmux_name_for_chat(chat_type, chat_id, false));
    names.push(agent.tmux_name_for_chat(chat_type, chat_id, true));
    names.dedup();
    names
}

/// 问飞书查到的名字,按(agent, ID)记:同一个应用里的 open_id 才有意义,换个应用可能就查得到。
/// 查不到的不记(页面自己会记,不反复问),网络一时失败也就不会被当成「查不到」。
type RemoteNames = std::collections::HashMap<(String, String), (String, std::time::Instant)>;

pub(crate) struct DaemonPort {
    core: Arc<crate::runtime::core::Core>,
    remote_names: std::sync::Mutex<RemoteNames>,
    /// 每个 agent 最近一个窗口里问过飞书的时刻,限住它的应用被控制台用掉的请求数。
    remote_fetches: std::sync::Mutex<
        std::collections::HashMap<String, std::collections::VecDeque<std::time::Instant>>,
    >,
}

/// 查到的名字记多久;改名过这么久就能看到。
const REMOTE_NAME_TTL: std::time::Duration = std::time::Duration::from_secs(600);
/// 缓存最多记这么多条,满了先丢过期的,还满就整个清掉。
const REMOTE_NAMES_MAX: usize = 4096;
/// 每个 agent 的应用在这个窗口里至多替控制台问飞书 [`REMOTE_PER_WINDOW`] 次。
const REMOTE_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);
const REMOTE_PER_WINDOW: usize = 300;

/// 窗口里还有余量就记下这一次并放行。
fn take_slot(
    log: &mut std::collections::VecDeque<std::time::Instant>,
    now: std::time::Instant,
    window: std::time::Duration,
    cap: usize,
) -> bool {
    while log
        .front()
        .is_some_and(|t| now.duration_since(*t) >= window)
    {
        log.pop_front();
    }
    if log.len() >= cap {
        return false;
    }
    log.push_back(now);
    true
}
/// 读 lark-cli 登录用户最多等这么久(它在 HTTP 处理线程里跑)。
const LARK_CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl DaemonPort {
    pub(crate) fn new(core: Arc<crate::runtime::core::Core>) -> Self {
        Self {
            core,
            remote_names: Default::default(),
            remote_fetches: Default::default(),
        }
    }
}

impl Port for DaemonPort {
    fn attention_requests(&self) -> Result<Vec<crate::runtime::core::AttentionRequest>, String> {
        self.core.attention_requests()
    }
    fn gate_agent(&self, alias: &str) -> Option<Agent> {
        self.core.current_gate_agent(alias)
    }

    fn chat_live(&self, agent: &Agent, chat_type: &str, chat_id: &str) -> bool {
        use crate::session::tmux::Tmux;
        // 盘上记过的会话名(含 owner 私聊的裸名)与按规则推导的两种名字都看:证不了不在就当在。
        let names = session_names(agent, chat_type, chat_id, |name| {
            Tmux::new(name.to_string()).has_session()
        });
        turn_or_process_live(agent, chat_type, chat_id)
            || names.into_iter().any(|name| Tmux::new(name).has_session())
    }

    fn chat_state_among(
        &self,
        agent: &Agent,
        chat_type: &str,
        chat_id: &str,
        tmux: &[String],
    ) -> (String, bool) {
        let in_tmux = |name: &str| tmux.iter().any(|t| t == name);
        let session = super::chat_meta::chat_session_name_among(agent, chat_type, chat_id, tmux);
        let live = turn_or_process_live(agent, chat_type, chat_id)
            || session_names(agent, chat_type, chat_id, in_tmux)
                .iter()
                .any(|name| in_tmux(name));
        (session, live)
    }

    fn session_name(&self, agent: &Agent, chat_type: &str, chat_id: &str) -> String {
        super::chat_meta::chat_session_name(agent, chat_type, chat_id)
    }

    fn tmux_sessions(&self) -> Vec<String> {
        crate::session::tmux::list_all_sessions()
    }

    fn agent_written(&self, alias: &str) {
        self.core.dashboard_agent_written(alias);
    }

    fn agent_idle(&self, agent: &Agent) -> Result<(), String> {
        use crate::observe::runtime_status::{busy_turns, live_headless};
        if busy_turns().iter().any(|t| t.agent == agent.name)
            || live_headless().iter().any(|t| t.agent == agent.name)
        {
            return Err(summon_i18n::text!("r9dc227f4c9").into());
        }
        let output =
            std::process::Command::new(summon_core::tools::resolve(summon_core::tools::Tool::Tmux))
                .args(["list-sessions", "-F", "#{session_name}"])
                .output()
                .map_err(|e| summon_i18n::format!("rb6bbe535fe"))?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if !error.contains("no server running") && !error.contains("No such file or directory")
            {
                return Err(summon_i18n::format!("r8f59b2f600"));
            }
        }
        let prefix = agent.tmux_name();
        if String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|s| s == prefix || s.starts_with(&format!("{prefix}-")))
        {
            return Err(summon_i18n::text!("red7d3d57b7").into());
        }
        Ok(())
    }
    fn retire_agent(&self, agent: &Agent) -> Result<(), String> {
        self.agent_idle(agent)?;
        self.core.retire_agent(agent).map_err(|e| format!("{e:#}"))
    }
    fn finish_agent_cleanup(&self, alias: &str) {
        self.core.finish_agent_cleanup(alias);
        if let Ok(mut names) = self.remote_names.lock() {
            names.retain(|(agent, _), _| agent != alias);
        }
        if let Ok(mut fetches) = self.remote_fetches.lock() {
            fetches.remove(alias);
        }
    }
    fn restore_agent(&self, alias: &str) {
        self.core.restore_agent(alias);
    }

    fn mentionless(&self, alias: &str, ask: MentionlessAsk) -> Result<(), String> {
        let msg = Inbound {
            chat_id: ask.chat_id,
            chat_type: Some(ask.chat_type),
            text: "/mentionless on".to_string(),
            sender: ask.open_id,
            sender_union_id: ask.union_id,
            sender_type: "user".to_string(),
            message_id: format!("dashboard-mentionless-{}", super::now_unix_ms()),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms: i64::try_from(super::now_unix_ms()).unwrap_or(i64::MAX),
            replayed: false,
            anchor_message_id: None,
            reattach: false,
        };
        let request = mentionless_setup::DashboardRequest {
            msg,
            reply: ask.reply,
        };
        if self
            .core
            .dispatch_to(alias, DispatchMsg::DashboardMentionless(request))
        {
            Ok(())
        } else {
            Err(summon_i18n::format!("r232d99f72f"))
        }
    }

    fn sentinel_identity(&self, cfg: &Config) -> Result<(), String> {
        let lark_cli = summon_core::tools::resolve_from(cfg, summon_core::tools::Tool::LarkCli);
        crate::setup::lark_cli::ensure_identity(
            &lark_cli,
            cfg.sentinel.lark_profile.as_deref(),
            false,
        )
        .map_err(|e| format!("{e:#}"))
    }

    fn lark_user(&self, cfg: &Config, profile: Option<&str>) -> Result<LarkUserView, String> {
        use super::sentinel::lark::{LarkUser, LarkUserLoginMissing};
        let lark_cli = summon_core::tools::resolve_from(cfg, summon_core::tools::Tool::LarkCli);
        let owner = LarkUser::new(
            profile.map(str::to_string),
            Some(lark_cli.to_string_lossy().into_owned()),
        )
        .owner_within(LARK_CLI_TIMEOUT)
        .map_err(|e| match e.downcast_ref::<LarkUserLoginMissing>() {
            Some(missing) => {
                let prefix = profile.map_or("lark-cli".to_string(), |p| {
                    format!("lark-cli --profile {p}")
                });
                summon_i18n::format!("r1bb621db7b")
            }
            None => format!("{e:#}"),
        })?;
        Ok(LarkUserView {
            open_id: owner.open_id,
            name: owner.name,
        })
    }

    fn model_candidates(&self, agent: &Agent, from_config: Vec<String>) -> Vec<String> {
        // agent 级:没有具体会话,RPC 会话的实时目录取不到时回退配置桶。
        super::model_candidates(agent, "", from_config)
    }

    fn member_name(&self, agent: &Agent, id: &str) -> Option<String> {
        let scope = super::feishu_delivery_scope(agent);
        crate::runtime::identity_cache::IdentityCacheStore::at_run_dir(&scope, false)
            .load()
            .name_for(id)
            .map(str::to_string)
            .or_else(|| {
                self.core
                    .local_agent_identities()
                    .iter()
                    .find_map(|identity| {
                        if id.starts_with("on_") {
                            identity.client.cached_name_by_union(id)
                        } else {
                            identity.client.resolve_name_cached(id)
                        }
                    })
            })
    }

    fn remote_name(&self, agent: &Agent, id: &str, budget: &mut usize) -> Option<String> {
        let key = (agent.name.clone(), id.to_string());
        if let Some((name, at)) = self.remote_names.lock().unwrap().get(&key) {
            if at.elapsed() < REMOTE_NAME_TTL {
                return Some(name.clone());
            }
        }
        if *budget == 0 {
            return None;
        }
        let allowed = take_slot(
            self.remote_fetches
                .lock()
                .unwrap()
                .entry(agent.name.clone())
                .or_default(),
            std::time::Instant::now(),
            REMOTE_WINDOW,
            REMOTE_PER_WINDOW,
        );
        if !allowed {
            return None;
        }
        *budget -= 1;
        let client = self
            .core
            .local_agent_identities()
            .into_iter()
            .find(|identity| identity.agent.name == agent.name)?
            .client;
        let name = if id.starts_with("oc_") {
            client.resolve_source_group(id)
        } else {
            client.fetch_user_name(id)
        };
        let name = name?;
        let mut cache = self.remote_names.lock().unwrap();
        if cache.len() >= REMOTE_NAMES_MAX {
            cache.retain(|_, (_, at)| at.elapsed() < REMOTE_NAME_TTL);
            if cache.len() >= REMOTE_NAMES_MAX {
                cache.clear();
            }
        }
        cache.insert(key, (name.clone(), std::time::Instant::now()));
        Some(name)
    }

    fn fleet(&self) -> Option<summon_core::fleet::FleetSnapshot> {
        let cfg = Config::load();
        let reg = summon_core::registry::Registry::load(&cfg.agents_dir).ok()?;
        let now = std::time::SystemTime::now();
        let runtime = crate::observe::runtime_status::snapshot(
            std::process::id(),
            Some(crate::version::VERSION.to_string()),
            now,
        );
        Some(crate::observe::fleet_snapshot::build(
            &cfg,
            &reg,
            Some(&runtime),
            now,
        ))
    }

    fn running_turn(&self, agent: &str, tag: &str) -> Option<RunningTurn> {
        let started = crate::observe::runtime_status::turn_started(agent, tag)?;
        let context = self.gate_agent(agent).and_then(|a| {
            crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&a.state_dir())
                .active_for_conversation(agent, tag)
        });
        let started_at_ms = context
            .as_ref()
            .map(|c| c.started_at_ms)
            .unwrap_or_else(|| {
                started
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                    .unwrap_or(0)
            });
        Some(RunningTurn {
            started_at_ms,
            turn_id: context.map(|c| c.turn_id),
        })
    }

    fn blocked(&self, agent: &Agent, session: &str) -> Option<String> {
        let path = agent
            .state_dir()
            .join("runtime")
            .join("chats")
            .join(format!("{session}.program-status.json"));
        let kind = crate::capture::program_status::current_blocked(&path)?;
        serde_json::to_value(kind)
            .ok()?
            .as_str()
            .map(str::to_string)
    }

    fn end_session(
        &self,
        agent: &Agent,
        tag: &str,
        archive: bool,
        interrupt: bool,
    ) -> Result<EndSessionOutcome, String> {
        let port = crate::runtime::menubar_session::InProcessPort {
            core: &self.core,
            agent,
            tag,
        };
        crate::runtime::menubar_session::end_session(&port, &agent.name, tag, archive, interrupt)
            .map_err(|e| format!("{e:#}"))
    }

    fn interrupt(
        &self,
        alias: &str,
        chat_id: &str,
        turn_id: &str,
    ) -> Result<DashInterrupt, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let msg = DispatchMsg::DashboardInterrupt {
            chat_id: chat_id.to_string(),
            turn_id: turn_id.to_string(),
            reply: tx,
        };
        if !self.core.dispatch_to(alias, msg) {
            return Err(summon_i18n::format!("r232d99f72f"));
        }
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| summon_i18n::format!("r917c5be6c1"))
    }

    fn public_host(&self) -> Option<std::net::Ipv4Addr> {
        crate::runtime::preview_server::detect_public_host()
    }

    fn autostart(&self) -> Autostart {
        if crate::setup::autostart::status().0.enabled {
            Autostart::On
        } else {
            Autostart::Off
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_lookups_per_agent_are_capped_within_the_window() {
        let window = std::time::Duration::from_secs(600);
        let t0 = std::time::Instant::now();
        let mut log = std::collections::VecDeque::new();
        assert!(take_slot(&mut log, t0, window, 2));
        assert!(take_slot(&mut log, t0, window, 2));
        assert!(!take_slot(
            &mut log,
            t0 + std::time::Duration::from_secs(599),
            window,
            2
        ));
        // 窗口滑过最早的那两次,又有余量
        assert!(take_slot(&mut log, t0 + window, window, 2));
        assert_eq!(log.len(), 1);
    }

    /// 控制台写的按群开关与 `/set` 写的文件逐字相同:同一把锁、同一个渲染。
    #[test]
    fn dashboard_chat_settings_match_set_command_bytes() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let a = "soul-test-ga";
        let b = "soul-test-gb";
        super::super::chat_meta::set_chat_setting(&agent, a, SettingKey::BadgeEffort, Some(false))
            .unwrap();
        super::super::chat_meta::set_chat_setting(&agent, a, SettingKey::AgentFeed, Some(true))
            .unwrap();
        update_chat_settings_if(
            &agent,
            b,
            &[
                (SettingKey::BadgeEffort, Some(false)),
                (SettingKey::AgentFeed, Some(true)),
            ],
            |current| current.is_none(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            std::fs::read(chat_settings_path(&agent, a)).unwrap(),
            std::fs::read(chat_settings_path(&agent, b)).unwrap()
        );
        let before = std::fs::read(chat_settings_path(&agent, b)).unwrap();
        let stale =
            update_chat_settings_if(&agent, b, &[(SettingKey::BadgeEffort, None)], |_| false)
                .unwrap();
        assert!(stale.is_none());
        assert_eq!(
            std::fs::read(chat_settings_path(&agent, b)).unwrap(),
            before
        );
    }
}
