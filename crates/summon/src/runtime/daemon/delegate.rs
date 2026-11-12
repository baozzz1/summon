//! 轮内跨群委派：凭证与 owner 复验、可选预演、单轮持久占位、来源卡与入站 WAL。
//!
//! Owner 授权范围内直接投递，群卡只展示公开摘要，私有正文通过内部任务传递。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use summon_core::registry::Agent;
use summon_core::session::group_target::{validate_name, GroupTarget};

use super::agent_env::{agent_owner, owner_id_matches, resolve_creds};
use super::sentinel::lark::LarkUser;
use crate::frontend::{client::FeishuClient, outbound};
use crate::runtime::core::Core;
use crate::runtime::current_turn_store::{CurrentTurnContext, CurrentTurnOrigin};
use crate::runtime::fsio::atomic_write;
use crate::runtime::inbox_wal::{InboxWal, ScheduledAppend, SentinelInbox};

const MAX_CONTENT_BYTES: usize = 16_000;

pub(crate) fn authorize(agent: &Agent, turn: &CurrentTurnContext) -> Result<()> {
    ensure!(
        turn.origin == CurrentTurnOrigin::Feishu,
        summon_i18n::format!("r2f53ecc910")
    );
    ensure!(
        !turn.message_id.starts_with("delegate-"),
        summon_i18n::format!("r6319313f7e")
    );
    ensure!(!turn.requester_is_bot, summon_i18n::format!("r5cd3b7037e"));
    let owner = agent_owner(agent).context(summon_i18n::text!("rdfe2170308"))?;
    ensure!(
        !owner.is_empty()
            && !turn.requester_open_id.is_empty()
            && owner_id_matches(&owner, &turn.requester_open_id, &turn.requester_union_id),
        summon_i18n::format!("r662bf1926c")
    );
    Ok(())
}

fn fire_id(turn_id: &str) -> String {
    format!("delegate-{turn_id}")
}

fn target_id(agent: &Agent, turn: &CurrentTurnContext, target: &str) -> Result<String> {
    let target = target.trim();
    let id = if target.starts_with("oc_") {
        ensure!(
            target.len() <= 128
                && target
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            summon_i18n::format!("r04004d5be3")
        );
        target.to_string()
    } else {
        let digests = crate::session::cross_chat::collect_digests(&agent.state_dir());
        let (listed, _) =
            summon_core::cross_chat::select_for_index(&digests, &turn.conversation_tag, usize::MAX);
        let selected = crate::session::cross_chat::resolve_target(&listed, target)?;
        ensure!(selected.is_group, summon_i18n::format!("r9a9cc5056c"));
        selected
            .chat_id
            .context(summon_i18n::text!("rcf3161911a"))?
    };
    ensure!(id != turn.chat_id, summon_i18n::format!("rcb80f19694"));
    Ok(id)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Plan {
    turn_id: String,
    #[serde(default)]
    delivery_scope: String,
    #[serde(default)]
    details_key: Option<String>,
    #[serde(default)]
    execution_mode: String,
    source_chat_id: String,
    source_name: String,
    target_chat_id: String,
    target_name: String,
    chat_type: String,
    content: String,
    #[serde(default)]
    summary: Option<String>,
    who: String,
    who_id: String,
    who_union_id: String,
}

impl Plan {
    fn execution_prompt(&self) -> String {
        summon_i18n::format!("rd098be6a9b", self.content)
    }

    fn payload(&self) -> SentinelInbox {
        SentinelInbox {
            chat_id: self.target_chat_id.clone(),
            chat_type: self.chat_type.clone(),
            prompt: self.execution_prompt(),
            fire_id: fire_id(&self.turn_id),
            who: self.who.clone(),
            who_id: self.who_id.clone(),
            who_union_id: self.who_union_id.clone(),
            handoff_note: None,
        }
    }

    fn card(&self) -> String {
        let summary = self
            .summary
            .as_deref()
            .unwrap_or(summon_i18n::text!("r9831b0bb10"));
        match &self.details_key {
            Some(key) => summon_core::feishu::card_delegate_summary(summary, key),
            None => summon_core::feishu::card_markdown_v2(summary),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Inviting,
    Ready,
    OpeningPrivate,
    PrivateNotified,
    PrivateReady,
    Publishing,
    Announced,
    Accepted,
}

#[derive(Serialize, Deserialize)]
struct Record {
    plan: Plan,
    phase: Phase,
    #[serde(default)]
    private_message_id: Option<String>,
    #[serde(default)]
    private_chat_id: Option<String>,
}

struct Store {
    root: PathBuf,
}
impl Store {
    fn new(agent: &Agent) -> Self {
        Self {
            root: agent.state_dir().join("runtime/delegations"),
        }
    }
    fn with_lock<T>(&self, turn_id: &str, f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        // 凭证来自 daemon，但路径仍只接受 UUID，避免跨进程输入成为路径片段。
        uuid::Uuid::parse_str(turn_id).context(summon_i18n::text!("rc5045f2daf"))?;
        std::fs::create_dir_all(&self.root)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.root.join(format!("{turn_id}.lock")))?;
        lock.lock().context(summon_i18n::text!("r178f67e320"))?;
        f(&self.root.join(format!("{turn_id}.json")))
    }
}
fn load(path: &Path) -> Result<Option<Record>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context(summon_i18n::text!("r2608bf97e9"))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn save(path: &Path, record: &Record) -> Result<()> {
    atomic_write(path, &serde_json::to_vec(record)?)?;
    if let Some(key) = &record.plan.details_key {
        uuid::Uuid::parse_str(key)?;
        let dir = path
            .parent()
            .context(summon_i18n::text!("r2d7fd8fb0d"))?
            .join("details");
        std::fs::create_dir_all(&dir)?;
        atomic_write(
            &dir.join(format!("{key}.json")),
            &serde_json::to_vec(&record.plan.turn_id)?,
        )?;
    }
    Ok(())
}

trait Delivery {
    fn check_turn(&self) -> Result<()>;
    fn invite(&self, chat: &str) -> Result<()>;
    fn prove_member(&self, chat: &str) -> Result<()>;
    fn announce(&self, plan: &Plan) -> Result<()>;
    fn route(&self, payload: &SentinelInbox) -> bool;
}

struct DeliveryContext<'a, D> {
    agent: &'a Agent,
    scope: &'a str,
    wal: &'a InboxWal,
    io: &'a D,
}

fn accepted(record: &Record) -> serde_json::Value {
    let message = if record.plan.execution_mode == "target_group" {
        summon_i18n::format!("rb1323d8caa", record.plan.target_name)
    } else {
        summon_i18n::text!("ra32e8d9d7f").into()
    };
    serde_json::json!({"mode":"accepted", "message":message})
}

fn commit(
    path: &Path,
    plan: &Plan,
    dry_run: bool,
    needs_invite: bool,
    delivery: &DeliveryContext<'_, impl Delivery>,
) -> Result<serde_json::Value> {
    let DeliveryContext {
        agent,
        scope,
        wal,
        io,
    } = delivery;
    io.check_turn()?;
    let old = load(path)?;
    if dry_run {
        if let Some(record) = &old {
            bail!(summon_i18n::format!("rde87088e1b", record.plan.target_name));
        }
        return Ok(
            serde_json::json!({"mode":"dry-run", "target":plan.target_name, "chat_id":plan.target_chat_id,
            "content_bytes":plan.content.len(), "summary":plan.summary, "execution":"target_group", "invite_bot":needs_invite,
            "next":summon_i18n::text!("rcf45ae007a")}),
        );
    }
    let mut record = match old {
        Some(mut record) => {
            ensure!(record.plan == *plan, summon_i18n::format!("rd791bf29aa"));
            if record.phase == Phase::Accepted {
                return Ok(accepted(&record));
            }
            ensure!(
                !matches!(record.phase, Phase::Publishing | Phase::OpeningPrivate),
                summon_i18n::format!("r4711af960e")
            );
            if record.phase == Phase::Inviting {
                io.prove_member(&plan.target_chat_id)
                    .context(summon_i18n::text!("r89399eb2f5"))?;
                record.phase = Phase::Ready;
                save(path, &record)?;
            }
            record
        }
        None => {
            let mut record = Record {
                plan: plan.clone(),
                phase: Phase::Inviting,
                private_message_id: None,
                private_chat_id: None,
            };
            if needs_invite {
                save(path, &record)?;
                io.invite(&plan.target_chat_id)
                    .context(summon_i18n::text!("rf70e7c1476"))?;
            }
            io.prove_member(&plan.target_chat_id)?;
            record.phase = Phase::Ready;
            save(path, &record)?;
            record
        }
    };
    ensure!(
        !plan.delivery_scope.is_empty()
            && plan.delivery_scope == *scope
            && plan.details_key.is_some(),
        summon_i18n::format!("r89a4d0aded")
    );
    ensure!(
        plan.execution_mode == "target_group"
            && record.private_message_id.is_none()
            && record.private_chat_id.is_none()
            && matches!(record.phase, Phase::Ready | Phase::Announced),
        summon_i18n::format!("r9507f6cd7d")
    );
    if record.phase == Phase::Ready {
        io.check_turn()?;
        io.prove_member(&plan.target_chat_id)?;
        record.phase = Phase::Publishing;
        save(path, &record)?;
        io.announce(plan)
            .context(summon_i18n::text!("rbef4e29b54"))?;
        record.phase = Phase::Announced;
        save(path, &record)?;
    }
    io.check_turn()?;
    io.prove_member(&plan.target_chat_id)?;
    let payload = plan.payload();
    let appended =
        wal.append_sentinel(&agent.name, scope, &payload, super::now_unix_ms() as i64)?;
    let routed = appended != ScheduledAppend::Inserted || io.route(&payload);
    record.phase = Phase::Accepted;
    save(path, &record)?;
    ensure!(routed, summon_i18n::format!("re3f3a087f4"));
    Ok(accepted(&record))
}

struct LiveDelivery<'a, F> {
    core: &'a Core,
    agent: &'a Agent,
    fc: &'a FeishuClient,
    lark: LarkUser,
    app_id: String,
    scope: &'a str,
    target: &'a str,
    requester: &'a str,
    union_id: &'a str,
    check: F,
}
impl<F: Fn() -> Result<()>> Delivery for LiveDelivery<'_, F> {
    fn check_turn(&self) -> Result<()> {
        (self.check)()?;
        let target =
            summon_core::bridge::ScopedChatKey::new(self.scope, &self.agent.name, self.target);
        let bridge = crate::bridge::store::BridgeStore::runtime_default();
        ensure!(
            super::bridge::bridge_slot_message(&bridge, &target)?.is_none(),
            summon_i18n::format!("r1590afcdec")
        );
        ensure!(
            !crate::remote::inbound::remote_bound(self.agent, self.target),
            summon_i18n::format!("r1700778531")
        );
        Ok(())
    }
    fn invite(&self, chat: &str) -> Result<()> {
        self.check_turn()?;
        self.lark.verified_requester(self.union_id)?;
        self.lark.add_bot(chat, &self.app_id)
    }
    fn prove_member(&self, chat: &str) -> Result<()> {
        ensure!(
            super::group_target::Groups::new(self.agent, self.fc, self.requester, self.union_id)
                .bot_present(chat)?,
            summon_i18n::format!("re7f33953c3")
        );
        Ok(())
    }
    fn announce(&self, plan: &Plan) -> Result<()> {
        outbound::send_card_idempotent_archived_id(
            self.fc,
            &self.agent.state_dir(),
            &plan.chat_type,
            &plan.target_chat_id,
            "delegate_source",
            &plan.card(),
            &fire_id(&plan.turn_id),
        )?;
        Ok(())
    }
    fn route(&self, payload: &SentinelInbox) -> bool {
        self.core
            .dispatch_scheduled(&self.agent.name, payload.fire())
    }
}

fn resume_target(
    record: &Record,
    same_request: bool,
    turn: &CurrentTurnContext,
    target: &GroupTarget,
    content: &str,
) -> Result<String> {
    let plan = &record.plan;
    ensure!(
        plan.turn_id == turn.turn_id
            && plan.source_chat_id == turn.chat_id
            && plan.content == content
            && plan.who_id == turn.requester_open_id
            && plan.who_union_id == turn.requester_union_id,
        summon_i18n::format!("rd791bf29aa")
    );
    // 没有请求副本的旧记录，只接受能直接与持久计划对上的名称或 ID。
    ensure!(
        same_request
            || matches!(target, GroupTarget::Existing(value) if value == &plan.target_chat_id || value == &plan.target_name),
        summon_i18n::format!("r05a083dbe1")
    );
    Ok(plan.target_chat_id.clone())
}

pub(crate) struct Request<'a> {
    pub target: &'a GroupTarget,
    pub content: &'a str,
    pub summary: Option<&'a str>,
    pub dry_run: bool,
}

pub(crate) fn run(
    core: &Core,
    agent: &Agent,
    turn: &CurrentTurnContext,
    request: Request<'_>,
    check: impl Fn() -> Result<()>,
) -> Result<serde_json::Value> {
    let Request {
        target,
        content,
        summary,
        dry_run,
    } = request;
    authorize(agent, turn)?;
    ensure!(
        !content.trim().is_empty() && content.len() <= MAX_CONTENT_BYTES,
        summon_i18n::format!("r3a7cb65bfd")
    );
    ensure!(
        summary.is_none_or(|s| !s.trim().is_empty() && s.len() <= 2000),
        summon_i18n::format!("r7369946741")
    );
    let identity = core
        .local_agent_identities()
        .into_iter()
        .find(|i| i.agent.name == agent.name)
        .context(summon_i18n::text!("rfdaab730b9"))?;
    ensure!(
        identity.delivery_scope == turn.delivery_scope,
        summon_i18n::format!("rc1cb753d21")
    );
    let cfg = summon_core::config::Config::load();
    let executable = summon_core::tools::resolve_from(&cfg, summon_core::tools::Tool::LarkCli);
    let wal = InboxWal::new(crate::runtime::paths::run_dir().join("inbox"));
    let check = || {
        check()?;
        let current = agent.reread().context(summon_i18n::text!("rd94b05ec51"))?;
        authorize(&current, turn)?;
        ensure!(
            super::agent_env::feishu_delivery_scope(&current) == identity.delivery_scope,
            summon_i18n::format!("r971f070537")
        );
        Ok(())
    };
    Store::new(agent).with_lock(&turn.turn_id, |path| {
        check()?;
        let request_path = path.with_extension("request.json");
        let request = serde_json::json!({"target":target,"content":content,"summary":summary});
        let saved_request = match std::fs::read(&request_path) {
            Ok(bytes) => Some(serde_json::from_slice::<serde_json::Value>(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(old) = &saved_request {
            ensure!(old == &request, summon_i18n::format!("r6e0d41bd6e"));
        }
        let current = agent.reread().context(summon_i18n::text!("rd94b05ec51"))?;
        let groups = super::group_target::Groups::new(&current, &identity.client, &turn.requester_open_id, &turn.requester_union_id);
        let previous = load(path)?;
        let target = if let Some(record) = &previous {
            ensure!(record.plan.summary.as_deref() == summary, summon_i18n::format!("rb7bb773776"));
            resume_target(record, saved_request.is_some(), turn, target, content)?
        } else {
            match target {
                GroupTarget::New { name } => {
                    let name = validate_name(name).map_err(anyhow::Error::msg)?;
                    if dry_run {
                        return Ok(serde_json::json!({"mode":"dry-run", "new_chat":name, "members":"Owner + current Bot", "execution":"target_group", "content_bytes":content.len(), "summary":summary}));
                    }
                    atomic_write(&request_path, &serde_json::to_vec(&request)?)?;
                    groups.create(&turn.turn_id, name, check)?
                }
                GroupTarget::Existing(input) if input.starts_with("oc_") || input.parse::<usize>().is_ok() => target_id(agent, turn, input)?,
                GroupTarget::Existing(name) => {
                    let found = groups.search(name, &turn.chat_id)?;
                    ensure!(!found.is_empty(), summon_i18n::format!("rd93f3f5525"));
                    ensure!(found.len() == 1, summon_i18n::format!("ra249274860", serde_json::to_string(&found)?));
                    found[0].chat_id.clone()
                }
            }
        };
        ensure!(target != turn.chat_id, summon_i18n::format!("rcb80f19694"));
        let io = LiveDelivery {
            core, agent, fc: &identity.client,
            lark: LarkUser::new(cfg.sentinel.lark_profile.clone(), Some(executable.to_string_lossy().into_owned())).with_timeout(std::time::Duration::from_secs(30)),
            app_id: resolve_creds(agent).0.context(summon_i18n::text!("r021d264030"))?,
            scope: &identity.delivery_scope, target: &target, requester: &turn.requester_open_id, union_id: &turn.requester_union_id, check: &check,
        };
        io.check_turn()?;
        let delivery = DeliveryContext {
            agent,
            scope: &identity.delivery_scope,
            wal: &wal,
            io: &io,
        };
        if let Some(record) = previous {
            return commit(path, &record.plan, dry_run, false, &delivery);
        }
        let (target_name, chat_type) = groups.info(&target)?;
        let chat_type = match chat_type.as_str() {
            "group" => "group",
            "topic" | "topic_group" => "topic_group",
            _ => bail!(summon_i18n::format!("r9a9cc5056c")),
        };
        let needs_invite = !groups.bot_present(&target)?;
        let source_name = io.fc.resolve_chat_name(&turn.chat_id);
        let who = io
            .fc
            .resolve_name_cached(&turn.requester_open_id)
            .unwrap_or_else(|| "owner".into());
        let plan = Plan {
            turn_id: turn.turn_id.clone(),
            delivery_scope: identity.delivery_scope.clone(),
            details_key: Some(uuid::Uuid::new_v4().to_string()),
            execution_mode: "target_group".into(),
            source_chat_id: turn.chat_id.clone(),
            source_name,
            target_chat_id: target.clone(),
            target_name,
            chat_type: chat_type.into(),
            content: content.into(),
            summary: summary.map(str::to_owned),
            who,
            who_id: turn.requester_open_id.clone(),
            who_union_id: turn.requester_union_id.clone(),
        };
        if !dry_run {
            atomic_write(&request_path, &serde_json::to_vec(&request)?)?;
        }
        commit(path, &plan, dry_run, needs_invite, &delivery)
    })
}

pub(super) fn validate_receiver(
    agent: &Agent,
    scope: &str,
    fire: &str,
    chat: &str,
    chat_type: &str,
    who_id: &str,
    union_id: &str,
) -> Result<()> {
    let Some(turn_id) = fire.strip_prefix("delegate-") else {
        return Ok(());
    };
    uuid::Uuid::parse_str(turn_id)?;
    let current = agent.reread().context(summon_i18n::text!("raa8f812705"))?;
    let path = Store::new(&current).root.join(format!("{turn_id}.json"));
    let record = load(&path)?.context(summon_i18n::text!("r2bed0a900e"))?;
    let plan = &record.plan;
    ensure!(
        matches!(record.phase, Phase::Announced | Phase::Accepted)
            && plan.execution_mode == "target_group"
            && matches!(chat_type, "group" | "topic_group")
            && plan.chat_type == chat_type
            && plan.target_chat_id == chat
            && record.private_message_id.is_none()
            && record.private_chat_id.is_none()
            && plan.turn_id == turn_id
            && !scope.is_empty()
            && plan.delivery_scope == scope
            && super::agent_env::feishu_delivery_scope(&current) == scope
            && plan.who_id == who_id
            && plan.who_union_id == union_id
            && agent_owner(&current)
                .is_some_and(|owner| owner_id_matches(&owner, who_id, union_id)),
        summon_i18n::format!("r0f87726e53")
    );
    Ok(())
}

fn details_allowed(
    agent: &Agent,
    plan: &Plan,
    scope: &str,
    chat: &str,
    operator: &str,
    union_id: &str,
) -> bool {
    !operator.is_empty()
        && !scope.is_empty()
        && plan.delivery_scope == scope
        && plan.target_chat_id == chat
        && plan.who_id == operator
        && agent_owner(agent).is_some_and(|owner| owner_id_matches(&owner, operator, union_id))
}

pub(super) fn handle_details(agent: &Agent, fc: &FeishuClient, card: &super::CardActionMsg) {
    let result = (|| -> Result<String> {
        let task_id = card
            .value
            .get("task_id")
            .and_then(|v| v.as_str())
            .context(summon_i18n::text!("ra457d0db1b"))?;
        let current = agent.reread().context(summon_i18n::text!("rd505bd9acd"))?;
        let scope = super::agent_env::feishu_delivery_scope(&current);
        let union_id = fc.resolve_union_id(&card.operator_open_id);
        uuid::Uuid::parse_str(task_id)?;
        let store = Store::new(&current);
        let turn_id: String = serde_json::from_slice(&std::fs::read(
            store.root.join("details").join(format!("{task_id}.json")),
        )?)?;
        store.with_lock(&turn_id, |path| {
            let record = load(path)?.context(summon_i18n::text!("r83fd9a499d"))?;
            ensure!(
                record.plan.details_key.as_deref() == Some(task_id),
                summon_i18n::format!("r0732de14c3")
            );
            ensure!(
                details_allowed(
                    &current,
                    &record.plan,
                    &scope,
                    &card.chat_id,
                    &card.operator_open_id,
                    &union_id
                ),
                summon_i18n::format!("r82f9a42eb9")
            );
            Ok(record.plan.content)
        })
    })();
    let text = match result {
        Ok(text) => text,
        Err(_) => summon_i18n::text!("r136dc60fd5").into(),
    };
    // 私有正文不进入群消息或共享出站归档。
    let parts = summon_core::feishu::split_for_card_by_budget(&text, |part| {
        let rendered = summon_core::feishu::render_external_lark_md(
            part,
            &[],
            &std::collections::HashMap::new(),
        );
        summon_core::feishu::rendered_final_reply_fits(&rendered)
    });
    for part in parts {
        if outbound::send_card_to_open_id_redacted_archived_id(
            fc,
            &agent.state_dir(),
            &card.operator_open_id,
            "delegate_details",
            &summon_core::feishu::card_markdown_v2(&part),
        )
        .is_err()
        {
            super::log(&summon_i18n::format!("r49027158e1", agent.name));
            break;
        }
    }
}

#[derive(Serialize, Deserialize)]
struct AutoPolicy {
    owner: String,
    scope: String,
    enabled: bool,
}

pub(super) fn auto_enabled(agent: &Agent, chat_id: &str) -> bool {
    let tag = summon_core::registry::chat_tag("group", chat_id);
    let path = agent
        .state_dir()
        .join("runtime/delegate-policy")
        .join(format!("{tag}.json"));
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<AutoPolicy>(&bytes).ok())
        .is_some_and(|p| {
            p.enabled
                && agent_owner(agent).as_deref() == Some(p.owner.as_str())
                && p.scope == super::agent_env::feishu_delivery_scope(agent)
        })
}

pub(super) fn set_auto(
    agent: &Agent,
    chat_id: &str,
    requester: &str,
    union_id: &str,
    enabled: Option<bool>,
) -> Result<String> {
    let current = agent.reread().context(summon_i18n::text!("rd94b05ec51"))?;
    let owner = agent_owner(&current).context(summon_i18n::text!("r0beedf7cd6"))?;
    ensure!(
        owner_id_matches(&owner, requester, union_id),
        summon_i18n::format!("ra157d51a71")
    );
    if let Some(enabled) = enabled {
        let dir = agent.state_dir().join("runtime/delegate-policy");
        std::fs::create_dir_all(&dir)?;
        let tag = summon_core::registry::chat_tag("group", chat_id);
        atomic_write(
            &dir.join(format!("{tag}.json")),
            &serde_json::to_vec(&AutoPolicy {
                owner,
                scope: super::agent_env::feishu_delivery_scope(&current),
                enabled,
            })?,
        )?;
    }
    Ok(if auto_enabled(&current, chat_id) {
        summon_i18n::text!("r6dcee087ed")
    } else {
        summon_i18n::text!("r105e3dc34b")
    }
    .into())
}

pub(super) fn auto_prompt(
    agent: &Agent,
    chat_id: &str,
    requester: &str,
    union_id: &str,
) -> Option<&'static str> {
    let current = agent.reread()?;
    let owner = agent_owner(&current)?;
    if !owner_id_matches(&owner, requester, union_id) {
        return None;
    }
    Some(if auto_enabled(&current, chat_id) {
        summon_i18n::text!("r1f9d2b0826")
    } else {
        summon_i18n::text!("r41ca50ebd9")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };

    struct Fake {
        calls: Mutex<Vec<String>>,
        payloads: Mutex<Vec<SentinelInbox>>,
        active: AtomicBool,
        member: AtomicBool,
        fail_invite: bool,
        fail_announce: bool,
    }
    impl Fake {
        fn new(member: bool) -> Self {
            Self {
                calls: Mutex::new(vec![]),
                payloads: Mutex::new(vec![]),
                active: AtomicBool::new(true),
                member: AtomicBool::new(member),
                fail_invite: false,
                fail_announce: false,
            }
        }
    }
    impl Delivery for Fake {
        fn check_turn(&self) -> Result<()> {
            ensure!(self.active.load(Ordering::SeqCst), "凭证失效");
            Ok(())
        }
        fn invite(&self, _: &str) -> Result<()> {
            self.calls.lock().unwrap().push("invite".into());
            ensure!(!self.fail_invite, "invite failed");
            self.member.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn prove_member(&self, _: &str) -> Result<()> {
            ensure!(self.member.load(Ordering::SeqCst), "not a member");
            Ok(())
        }
        fn announce(&self, _: &Plan) -> Result<()> {
            self.calls.lock().unwrap().push("announce".into());
            ensure!(!self.fail_announce, "unknown delivery");
            Ok(())
        }
        fn route(&self, payload: &SentinelInbox) -> bool {
            self.calls.lock().unwrap().push("route".into());
            self.payloads.lock().unwrap().push(payload.clone());
            true
        }
    }
    fn fixture() -> (Agent, tempfile::TempDir, Plan, InboxWal) {
        let (agent, tmp) = crate::test_support::agent_dir(
            "codex",
            "feishu:\n  allowed_users: [ou_owner, ou_other]\n",
        );
        let plan = Plan {
            turn_id: uuid::Uuid::new_v4().to_string(),
            delivery_scope: "scope".into(),
            details_key: Some(uuid::Uuid::new_v4().to_string()),
            execution_mode: "target_group".into(),
            source_chat_id: "oc_source".into(),
            source_name: "源群".into(),
            target_chat_id: "oc_target".into(),
            target_name: "目标群".into(),
            chat_type: "group".into(),
            content: "原样任务\n第二行".into(),
            summary: None,
            who: "测试 owner".into(),
            who_id: "ou_owner".into(),
            who_union_id: "on_owner".into(),
        };
        let wal = InboxWal::new(tmp.path().join("inbox"));
        (agent, tmp, plan, wal)
    }
    fn context(plan: &Plan) -> CurrentTurnContext {
        CurrentTurnContext {
            turn_id: plan.turn_id.clone(),
            agent_alias: "test".into(),
            delivery_scope: "scope".into(),
            chat_id: plan.source_chat_id.clone(),
            chat_type: "group".into(),
            conversation_tag: "g-source".into(),
            message_id: "om_source".into(),
            origin: CurrentTurnOrigin::Feishu,
            schedule_output: summon_core::ScheduleOutput::Progress,
            requester_open_id: plan.who_id.clone(),
            requester_union_id: plan.who_union_id.clone(),
            requester_is_peer: false,
            requester_is_bot: false,
            thread_id: None,
            root_id: None,
            started_at_ms: 1,
            reattached: false,
        }
    }
    fn apply(
        agent: &Agent,
        wal: &InboxWal,
        plan: &Plan,
        dry_run: bool,
        invite: bool,
        io: &Fake,
    ) -> Result<serde_json::Value> {
        Store::new(agent).with_lock(&plan.turn_id, |path| {
            commit(
                path,
                plan,
                dry_run,
                invite,
                &DeliveryContext {
                    agent,
                    scope: "scope",
                    wal,
                    io,
                },
            )
        })
    }

    #[test]
    fn delegate_legacy_records_never_resume_public_execution() {
        let (agent, _tmp, mut plan, wal) = fixture();
        plan.delivery_scope.clear();
        plan.details_key = None;
        let io = Fake::new(true);
        let record = Record {
            plan: plan.clone(),
            phase: Phase::Announced,
            private_message_id: None,
            private_chat_id: None,
        };
        Store::new(&agent)
            .with_lock(&plan.turn_id, |path| save(path, &record))
            .unwrap();
        assert!(apply(&agent, &wal, &plan, false, false, &io).is_err());
        assert!(io.calls.lock().unwrap().is_empty());
        Store::new(&agent)
            .with_lock(&plan.turn_id, |path| {
                save(
                    path,
                    &Record {
                        phase: Phase::Accepted,
                        ..record
                    },
                )
            })
            .unwrap();
        let outcome = apply(&agent, &wal, &plan, false, false, &io).unwrap();
        assert!(!outcome.to_string().contains("私聊执行"));
        assert!(io.calls.lock().unwrap().is_empty());
        assert!(wal
            .recorded_sentinel(&agent.name, &fire_id(&plan.turn_id))
            .unwrap()
            .is_none());
    }

    #[test]
    fn delegate_private_records_cannot_change_execution_scope() {
        for phase in [
            Phase::Ready,
            Phase::OpeningPrivate,
            Phase::PrivateNotified,
            Phase::PrivateReady,
            Phase::Announced,
            Phase::Accepted,
        ] {
            let (agent, _tmp, mut plan, wal) = fixture();
            plan.execution_mode.clear();
            plan.delivery_scope = super::super::agent_env::feishu_delivery_scope(&agent);
            let record = Record {
                plan: plan.clone(),
                phase,
                private_message_id: Some("om_private".into()),
                private_chat_id: Some("oc_private".into()),
            };
            Store::new(&agent)
                .with_lock(&plan.turn_id, |path| save(path, &record))
                .unwrap();
            let io = Fake::new(true);
            let result = apply(&agent, &wal, &plan, false, false, &io);
            assert_eq!(result.is_ok(), phase == Phase::Accepted);
            assert!(io.calls.lock().unwrap().is_empty());
            assert!(wal
                .recorded_sentinel(&agent.name, &fire_id(&plan.turn_id))
                .unwrap()
                .is_none());
            for (chat, kind) in [("oc_target", "group"), ("oc_private", "p2p")] {
                assert!(validate_receiver(
                    &agent,
                    &plan.delivery_scope,
                    &fire_id(&plan.turn_id),
                    chat,
                    kind,
                    &plan.who_id,
                    &plan.who_union_id
                )
                .is_err());
            }
        }
    }

    #[test]
    fn delegate_revalidates_group_receiver_when_replayed_or_dequeued() {
        let (agent, _tmp, mut plan, wal) = fixture();
        plan.delivery_scope = super::super::agent_env::feishu_delivery_scope(&agent);
        let record = Record {
            plan: plan.clone(),
            phase: Phase::Accepted,
            private_message_id: None,
            private_chat_id: None,
        };
        Store::new(&agent)
            .with_lock(&plan.turn_id, |path| save(path, &record))
            .unwrap();
        let check = |scope: &str, chat: &str, mode: &str, who: &str, union: &str| {
            validate_receiver(
                &agent,
                scope,
                &fire_id(&plan.turn_id),
                chat,
                mode,
                who,
                union,
            )
        };
        assert!(check(
            &plan.delivery_scope,
            "oc_target",
            "group",
            "ou_owner",
            "on_owner"
        )
        .is_ok());
        for (chat, mode, who, union) in [
            ("oc_private", "p2p", "ou_owner", "on_owner"),
            ("oc_other", "group", "ou_owner", "on_owner"),
            ("oc_target", "group", "ou_other", "on_owner"),
            ("oc_target", "group", "ou_owner", "on_other"),
        ] {
            assert!(check(&plan.delivery_scope, chat, mode, who, union).is_err());
        }
        assert!(check("other", "oc_target", "group", "ou_owner", "on_owner").is_err());
        let raw = std::fs::read_to_string(agent.manifest_path()).unwrap();
        std::fs::write(
            agent.manifest_path(),
            raw.replace("ou_owner, ou_other", "ou_next"),
        )
        .unwrap();
        assert!(check(
            &plan.delivery_scope,
            "oc_target",
            "group",
            "ou_owner",
            "on_owner"
        )
        .is_err());
        assert!(wal
            .recorded_sentinel(&agent.name, &fire_id(&plan.turn_id))
            .unwrap()
            .is_none());
    }

    #[test]
    fn delegate_routes_execution_and_recovery_to_target_group() {
        for chat_type in ["group", "topic_group"] {
            let (agent, tmp, mut plan, wal) = fixture();
            plan.chat_type = chat_type.into();
            let io = Fake::new(true);
            apply(&agent, &wal, &plan, false, false, &io).unwrap();
            let live = io.payloads.lock().unwrap()[0].clone();
            let replay = InboxWal::new(tmp.path().join("inbox"))
                .recorded_sentinel(&agent.name, &fire_id(&plan.turn_id))
                .unwrap()
                .unwrap()
                .payload
                .unwrap();
            for payload in [live, replay] {
                assert_eq!(payload.chat_id, "oc_target");
                assert_eq!(payload.chat_type, chat_type);
                assert!(payload.prompt.ends_with(&plan.content));
                assert!(payload.prompt.contains("在当前目标群执行并回复"));
                assert_eq!(payload.who_id, plan.who_id);
            }
            assert!(!plan.card().contains("原样任务"));
        }
    }

    #[test]
    fn delegate_details_callback_delivers_privately_and_redacts_archive() {
        for operator in ["ou_owner", "ou_other"] {
            let (agent, _tmp, mut plan, _) = fixture();
            plan.delivery_scope = super::super::agent_env::feishu_delivery_scope(&agent);
            Store::new(&agent)
                .with_lock(&plan.turn_id, |path| {
                    save(
                        path,
                        &Record {
                            plan: plan.clone(),
                            phase: Phase::Accepted,
                            private_message_id: None,
                            private_chat_id: None,
                        },
                    )
                })
                .unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let server_thread = std::thread::spawn(move || {
                let mut sent = None;
                for i in 0..3 {
                    let mut request = server
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap()
                        .expect("token, identity and private message");
                    let response = match i {
                        0 => r#"{"code":0,"tenant_access_token":"t","expire":7200}"#,
                        1 => r#"{"code":0,"data":{"user":{"union_id":"on_owner"}}}"#,
                        _ => {
                            assert!(request.url().contains("receive_id_type=open_id"));
                            let mut body = String::new();
                            request.as_reader().read_to_string(&mut body).unwrap();
                            sent = Some(serde_json::from_str::<serde_json::Value>(&body).unwrap());
                            r#"{"code":0,"data":{"message_id":"om_private"}}"#
                        }
                    };
                    request
                        .respond(tiny_http::Response::from_string(response))
                        .unwrap();
                }
                sent.unwrap()
            });
            let fc = FeishuClient::with_test_base(base);
            let card = super::super::CardActionMsg {
                chat_id: plan.target_chat_id.clone(),
                message_id: "om_summary".into(),
                operator_open_id: operator.into(),
                event_id: "event".into(),
                value: serde_json::json!({"action":"delegate_details", "task_id":plan.details_key}),
                wal_id: None,
                wal_replayed: false,
            };
            handle_details(&agent, &fc, &card);
            let sent = server_thread.join().unwrap();
            assert_eq!(sent["receive_id"], operator);
            let content: serde_json::Value =
                serde_json::from_str(sent["content"].as_str().unwrap()).unwrap();
            let body = content["body"]["elements"].to_string();
            assert_eq!(body.contains("原样任务"), operator == "ou_owner");
            let threads = agent.state_dir().join("threads");
            let archive = std::fs::read_to_string(
                threads
                    .join(summon_core::registry::chat_tag("p2p", operator))
                    .join("outbound.ndjson"),
            )
            .unwrap();
            assert!(archive.contains("content_redacted"));
            assert!(!archive.contains("原样任务"));
            assert!(!archive.contains("om_private"));
            assert!(!threads
                .join(summon_core::registry::chat_tag(
                    "group",
                    &plan.target_chat_id
                ))
                .join("outbound.ndjson")
                .exists());
        }
    }

    #[test]
    fn delegate_details_require_original_and_current_owner_in_same_scope_and_chat() {
        let (agent, _tmp, plan, _) = fixture();
        assert!(details_allowed(
            &agent,
            &plan,
            "scope",
            "oc_target",
            "ou_owner",
            "on_owner"
        ));
        for (scope, chat, operator, union_id) in [
            ("scope", "oc_target", "ou_other", "on_other"),
            ("scope", "oc_target", "ou_other", "on_owner"),
            ("other", "oc_target", "ou_owner", "on_owner"),
            ("scope", "oc_other", "ou_owner", "on_owner"),
            ("", "oc_target", "ou_owner", "on_owner"),
        ] {
            assert!(!details_allowed(
                &agent, &plan, scope, chat, operator, union_id
            ));
        }
        let mut current = agent.clone();
        current.feishu.as_mut().unwrap().allowed_users = vec!["ou_other".into()];
        assert!(!details_allowed(
            &current,
            &plan,
            "scope",
            "oc_target",
            "ou_owner",
            "on_owner"
        ));
        current.feishu.as_mut().unwrap().allowed_users = vec!["on_owner".into()];
        assert!(details_allowed(
            &current,
            &plan,
            "scope",
            "oc_target",
            "ou_owner",
            "on_owner"
        ));
        assert!(!details_allowed(
            &current,
            &plan,
            "scope",
            "oc_target",
            "ou_owner",
            ""
        ));
    }

    #[test]
    fn delegate_card_contains_only_public_summary_and_opaque_details_key() {
        let (_, _tmp, mut plan, _) = fixture();
        plan.summary = Some("公开摘要".into());
        let card = plan.card();
        assert!(card.contains("公开摘要"));
        assert!(card.contains("delegate_details"));
        assert!(!card.contains(&plan.content));
        assert!(!card.contains(&plan.source_name));
        assert!(!card.contains(&plan.source_chat_id));
        assert!(!card.contains(&plan.turn_id));
        plan.summary = None;
        let card = plan.card();
        assert!(card.contains("已接收一项委派任务"));
        assert!(!card.contains(&plan.content));
    }

    #[test]
    fn delegate_inviting_resumes_only_after_membership_is_proven() {
        let (agent, _tmp, plan, wal) = fixture();
        let mut io = Fake::new(false);
        io.fail_invite = true;
        assert!(apply(&agent, &wal, &plan, false, true, &io).is_err());
        let persisted = Store::new(&agent)
            .with_lock(&plan.turn_id, load)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.phase, Phase::Inviting);
        assert!(apply(&agent, &wal, &plan, false, true, &io).is_err());
        let persisted = Store::new(&agent)
            .with_lock(&plan.turn_id, load)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.phase, Phase::Inviting);
        assert_eq!(*io.calls.lock().unwrap(), ["invite"]);
        io.member.store(true, Ordering::SeqCst);
        apply(&agent, &wal, &plan, false, true, &io).unwrap();
        assert_eq!(*io.calls.lock().unwrap(), ["invite", "announce", "route"]);
        let persisted = Store::new(&agent)
            .with_lock(&plan.turn_id, load)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.phase, Phase::Accepted);
        apply(&agent, &wal, &plan, false, true, &io).unwrap();
        assert_eq!(*io.calls.lock().unwrap(), ["invite", "announce", "route"]);
    }

    #[test]
    fn delegate_resume_uses_saved_target_and_rejects_changed_identity_or_task() {
        let (_, _tmp, plan, _) = fixture();
        let turn = context(&plan);
        let record = Record {
            plan: plan.clone(),
            phase: Phase::Inviting,
            private_message_id: None,
            private_chat_id: None,
        };
        let request = GroupTarget::Existing("Original Name".into());
        assert_eq!(
            resume_target(&record, true, &turn, &request, &plan.content).unwrap(),
            plan.target_chat_id
        );
        assert!(resume_target(&record, false, &turn, &request, &plan.content).is_err());
        assert!(resume_target(&record, true, &turn, &request, "different").is_err());
        let mut other = turn.clone();
        other.requester_union_id = "on_other".into();
        assert!(resume_target(&record, true, &other, &request, &plan.content).is_err());
        assert_eq!(
            resume_target(
                &record,
                false,
                &turn,
                &GroupTarget::Existing(plan.target_chat_id.clone()),
                &plan.content
            )
            .unwrap(),
            plan.target_chat_id
        );
    }

    #[test]
    fn delegate_auto_prompt_uses_resolved_union_and_live_owner() {
        let (agent, _tmp, _, _) = fixture();
        let raw = std::fs::read_to_string(agent.manifest_path()).unwrap();
        std::fs::write(
            agent.manifest_path(),
            raw.replace("ou_owner, ou_other", "on_owner"),
        )
        .unwrap();
        set_auto(&agent, "oc_master", "ou_owner", "on_owner", Some(true)).unwrap();
        assert!(auto_prompt(&agent, "oc_master", "ou_owner", "on_owner")
            .unwrap()
            .contains("已开启"));
        assert!(auto_prompt(&agent, "oc_master", "ou_owner", "").is_none());
        assert!(auto_prompt(&agent, "oc_master", "ou_other", "on_other").is_none());
        std::fs::write(
            agent.manifest_path(),
            raw.replace("ou_owner, ou_other", "on_next"),
        )
        .unwrap();
        assert!(auto_prompt(&agent, "oc_master", "ou_owner", "on_owner").is_none());
        assert!(auto_prompt(&agent, "oc_master", "ou_next", "on_next")
            .unwrap()
            .contains("未开启"));
    }

    #[test]
    fn delegate_owner_proof_rejects_bots_schedules_delegated_and_unknown_senders() {
        let (agent, _tmp, plan, _) = fixture();
        let turn = context(&plan);
        assert!(authorize(&agent, &turn).is_ok());
        let mut sentinel = turn.clone();
        sentinel.message_id = "sentinel-om_source".into();
        assert!(authorize(&agent, &sentinel).is_ok());
        for denied in [
            CurrentTurnContext {
                requester_open_id: "ou_other".into(),
                requester_union_id: String::new(),
                ..turn.clone()
            },
            CurrentTurnContext {
                origin: CurrentTurnOrigin::Schedule,
                ..turn.clone()
            },
            CurrentTurnContext {
                message_id: fire_id(&plan.turn_id),
                ..turn.clone()
            },
            CurrentTurnContext {
                requester_is_bot: true,
                ..turn.clone()
            },
        ] {
            assert!(authorize(&agent, &denied).is_err(), "{denied:?}");
        }
        let mut raw = serde_json::to_value(&turn).unwrap();
        raw.as_object_mut().unwrap().remove("requester_is_bot");
        let unproven: CurrentTurnContext = serde_json::from_value(raw).unwrap();
        assert!(authorize(&agent, &unproven).is_err());
        let mut union_agent = agent.clone();
        union_agent.feishu.as_mut().unwrap().allowed_users = vec!["on_owner".into()];
        assert!(authorize(&union_agent, &turn).is_ok());
    }

    #[test]
    fn delegate_dry_run_is_read_only_and_closes_after_delivery() {
        let (agent, _tmp, plan, wal) = fixture();
        let io = Fake::new(false);
        let preview = apply(&agent, &wal, &plan, true, true, &io).unwrap();
        assert_eq!(preview["mode"], "dry-run");
        assert_eq!(preview["content_bytes"], plan.content.len());
        assert!(!preview.to_string().contains(&plan.content));
        assert_eq!(preview["invite_bot"], true);
        assert!(io.calls.lock().unwrap().is_empty());
        assert!(wal
            .recorded_sentinel(&agent.name, &fire_id(&plan.turn_id))
            .unwrap()
            .is_none());
        let record = Store::new(&agent).with_lock(&plan.turn_id, load).unwrap();
        assert!(record.is_none(), "预演不得占用本轮的委派名额");
        apply(&agent, &wal, &plan, false, true, &io).unwrap();
        assert!(apply(&agent, &wal, &plan, true, false, &io)
            .unwrap_err()
            .to_string()
            .contains("每轮最多一次"));
        assert_eq!(*io.calls.lock().unwrap(), ["invite", "announce", "route"]);
    }

    #[test]
    fn delegate_retries_route_one_owner_task_with_exact_body_after_source_card() {
        let (agent, _tmp, plan, wal) = fixture();
        let io = Fake::new(true);
        for _ in 0..3 {
            let outcome = apply(&agent, &wal, &plan, false, false, &io).unwrap();
            assert_eq!(outcome["mode"], "accepted");
        }
        assert_eq!(*io.calls.lock().unwrap(), ["announce", "route"]);
        let payloads = io.payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].prompt, plan.execution_prompt());
        assert_eq!(payloads[0].who_id, "ou_owner");
        assert_eq!(payloads[0].fire_id, format!("delegate-{}", plan.turn_id));
        assert!(wal
            .recorded_sentinel(&agent.name, &format!("delegate-{}", plan.turn_id))
            .unwrap()
            .is_some());
        let mut second = plan.clone();
        second.content = "第二项".into();
        assert!(apply(&agent, &wal, &second, false, false, &io).is_err());
        second = plan.clone();
        second.target_chat_id = "oc_other".into();
        assert!(apply(&agent, &wal, &second, false, false, &io).is_err());
        assert_eq!(*io.calls.lock().unwrap(), ["announce", "route"]);
        let reopened = InboxWal::new(_tmp.path().join("inbox"));
        let replay = reopened
            .recorded_sentinel(&agent.name, &format!("delegate-{}", plan.turn_id))
            .unwrap()
            .unwrap()
            .payload
            .unwrap();
        for payload in [&payloads[0], &replay] {
            let super::super::DispatchMsg::SentinelFire {
                fire_id,
                prompt,
                who,
                who_id,
                who_union_id,
                handoff_note,
                ..
            } = payload.fire()
            else {
                panic!("委派必须沿 sentinel 通道投递");
            };
            let task = super::super::sentinel_chat_task(
                fire_id,
                &prompt,
                handoff_note,
                true,
                who,
                who_id,
                who_union_id,
                summon_core::config::builtin_messaging(),
                false,
            );
            assert_eq!(task.who_id, "ou_owner");
            assert!(!task.sender_is_bot);
            assert!(!task.kind.is_scheduled());
            assert_eq!(task.text, plan.execution_prompt());
            assert!(task.queued);
            assert!(task.is_group);
            assert!(task.hide_context_hint);
            assert_eq!(task.message_id, format!("delegate-{}", plan.turn_id));
            let receiver = CurrentTurnContext {
                turn_id: uuid::Uuid::new_v4().to_string(),
                chat_id: plan.target_chat_id.clone(),
                message_id: task.message_id,
                requester_open_id: task.who_id,
                requester_union_id: task.who_union_id,
                requester_is_bot: task.sender_is_bot,
                ..context(&plan)
            };
            assert!(authorize(&agent, &receiver)
                .unwrap_err()
                .to_string()
                .contains("委派来的轮"));
        }
    }

    #[test]
    fn delegate_invites_only_the_bot_and_stops_before_delivery_on_failure() {
        let (agent, _tmp, plan, wal) = fixture();
        let mut io = Fake::new(false);
        io.fail_invite = true;
        assert!(apply(&agent, &wal, &plan, false, true, &io).is_err());
        assert!(apply(&agent, &wal, &plan, false, true, &io).is_err());
        assert_eq!(*io.calls.lock().unwrap(), ["invite"]);
        assert!(wal
            .recorded_sentinel(&agent.name, &fire_id(&plan.turn_id))
            .unwrap()
            .is_none());
        let (agent, _tmp2, plan, wal) = fixture();
        let io = Fake::new(false);
        apply(&agent, &wal, &plan, false, true, &io).unwrap();
        assert_eq!(*io.calls.lock().unwrap(), ["invite", "announce", "route"]);
    }

    #[test]
    fn delegate_unproven_membership_sends_nothing_and_can_retry() {
        let (agent, _tmp, plan, wal) = fixture();
        let io = Fake::new(false);
        assert!(apply(&agent, &wal, &plan, false, false, &io).is_err());
        assert!(io.calls.lock().unwrap().is_empty());
        io.member.store(true, Ordering::SeqCst);
        apply(&agent, &wal, &plan, false, false, &io).unwrap();
        assert_eq!(*io.calls.lock().unwrap(), ["announce", "route"]);
    }

    #[test]
    fn delegate_uncertain_source_card_and_revoked_turn_cannot_emit_again() {
        let (agent, _tmp, plan, wal) = fixture();
        let mut io = Fake::new(true);
        io.fail_announce = true;
        for _ in 0..2 {
            assert!(apply(&agent, &wal, &plan, false, false, &io).is_err());
        }
        assert_eq!(*io.calls.lock().unwrap(), ["announce"]);
        assert!(wal
            .recorded_sentinel(&agent.name, &fire_id(&plan.turn_id))
            .unwrap()
            .is_none());
        let (agent, _tmp2, plan, wal) = fixture();
        let io = Fake::new(true);
        io.active.store(false, Ordering::SeqCst);
        assert!(apply(&agent, &wal, &plan, false, false, &io).is_err());
        assert!(apply(&agent, &wal, &plan, true, false, &io).is_err());
        assert!(io.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn delegate_concurrent_calls_share_one_durable_slot() {
        let (agent, _tmp, plan, wal) = fixture();
        let io = Fake::new(true);
        std::thread::scope(|scope| {
            let a = scope.spawn(|| apply(&agent, &wal, &plan, false, false, &io));
            let b = scope.spawn(|| apply(&agent, &wal, &plan, false, false, &io));
            a.join().unwrap().unwrap();
            b.join().unwrap().unwrap();
        });
        assert_eq!(*io.calls.lock().unwrap(), ["announce", "route"]);
    }

    #[test]
    fn delegate_target_resolution_rejects_ambiguity_private_and_current_chat() {
        let (agent, _tmp, plan, _) = fixture();
        let turn = context(&plan);
        let root = agent.state_dir().join("runtime/chats");
        std::fs::create_dir_all(&root).unwrap();
        for (tag, name, mode) in [
            ("ga", "开发甲", "group"),
            ("gb", "开发乙", "group"),
            ("pc", "私聊", "p2p"),
        ] {
            let meta = summon_core::chat_meta::ChatMeta {
                chat_id: Some(format!("oc_{tag}")),
                chat_type: Some(mode.into()),
                chat_name: Some(name.into()),
                tag: Some(tag.into()),
                last_sender: None,
                updated_at: None,
                creator: None,
            };
            std::fs::write(root.join(format!("{tag}.meta")), meta.render()).unwrap();
            let transcript = crate::session::cross_chat::transcript_path(&agent.state_dir(), tag);
            std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            std::fs::write(transcript, "索引记录").unwrap();
        }
        let ambiguity = target_id(&agent, &turn, "开发").unwrap_err().to_string();
        assert!(
            ambiguity.contains("开发甲") && ambiguity.contains("开发乙"),
            "{ambiguity}"
        );
        assert_eq!(target_id(&agent, &turn, "开发甲").unwrap(), "oc_ga");
        assert!(target_id(&agent, &turn, "私聊").is_err());
        assert!(target_id(&agent, &turn, "oc_source").is_err());
        assert!(target_id(&agent, &turn, "oc_../../else").is_err());
        assert_eq!(
            target_id(&agent, &turn, "oc_new_group").unwrap(),
            "oc_new_group"
        );
    }

    #[test]
    fn delegate_source_card_escapes_mentions() {
        let (_, _tmp, mut plan, _) = fixture();
        plan.summary = Some("<at id=all></at> 正文".into());
        assert!(!plan.card().contains("<at id=all>"));
        assert!(plan.card().contains("正文"));
    }

    #[test]
    fn delegate_auto_policy_requires_owner_and_is_scoped_to_chat_and_identity() {
        let (agent, _tmp, _, _) = fixture();
        assert!(!auto_enabled(&agent, "oc_master"));
        assert!(set_auto(&agent, "oc_master", "ou_other", "", Some(true)).is_err());
        set_auto(&agent, "oc_master", "ou_owner", "", Some(true)).unwrap();
        assert!(auto_enabled(&agent, "oc_master"));
        assert!(!auto_enabled(&agent, "oc_else"));
        set_auto(&agent, "oc_master", "ou_owner", "", Some(false)).unwrap();
        assert!(!auto_enabled(&agent, "oc_master"));
        set_auto(&agent, "oc_master", "ou_owner", "", Some(true)).unwrap();
        let mut current = agent.clone();
        current.feishu.as_mut().unwrap().allowed_users = vec!["ou_next_owner".into()];
        assert!(!auto_enabled(&current, "oc_master"));
        let tag = summon_core::registry::chat_tag("group", "oc_master");
        let path = agent
            .state_dir()
            .join("runtime/delegate-policy")
            .join(format!("{tag}.json"));
        std::fs::write(
            &path,
            br#"{"owner":"ou_owner","enabled":true,"scope":"other-bot"}"#,
        )
        .unwrap();
        assert!(!auto_enabled(&agent, "oc_master"));
        std::fs::write(path, "corrupt").unwrap();
        assert!(!auto_enabled(&agent, "oc_master"));
    }
}
