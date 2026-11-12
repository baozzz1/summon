//! 源群的 Fork 表单、同名选择与直达命令。提交在 dispatcher 内串行执行。

use super::group_target::{with_record_lock, Groups};
use super::{fork, CardActionMsg, ChatHandle};
use crate::frontend::{client::FeishuClient, outbound};
use crate::runtime::fsio::atomic_write;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use summon_core::driver::slash_pool::{self, SlashBlacklist, SlashPlan, SlashToggles};
use summon_core::registry::{Agent, OperateScope};
use summon_core::session::group_target::{self as ui, ForkTargetInput, GroupCandidate};

const TTL_SECONDS: i64 = 600;

pub(super) struct Environment<'a> {
    pub agent: &'a Agent,
    pub fc: &'a FeishuClient,
    pub chats: &'a HashMap<String, ChatHandle>,
    pub scope: &'a str,
}

pub(super) struct Source<'a> {
    pub chat_id: &'a str,
    pub chat_type: &'a str,
    pub message_id: &'a str,
    pub requester: &'a str,
    pub union_id: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Selection {
    New(String),
    Existing(GroupCandidate),
}

#[derive(Clone, Serialize, Deserialize)]
struct Request {
    source_chat: String,
    source_type: String,
    source_session: String,
    source_tag: String,
    source_sid: String,
    source_name: String,
    scope: String,
    requester: String,
    union_id: String,
    issued_at: i64,
    card_id: Option<String>,
    candidates: Vec<GroupCandidate>,
    #[serde(flatten)]
    state: RequestState,
    new_sid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    id: String,
    name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "LegacyPhase")]
enum RequestState {
    Choosing,
    // 预检尚未通过的选择只在内存中流转，不能写成已锁定记录。
    Selected(Selection),
    Locked {
        selection: Selection,
        target: Option<Target>,
    },
    Committed {
        selection: Selection,
        target: Target,
    },
}

#[derive(Serialize, Deserialize)]
struct LegacyPhase {
    selection: Option<Selection>,
    target_id: Option<String>,
    target_name: Option<String>,
    committed: bool,
}

impl TryFrom<LegacyPhase> for RequestState {
    type Error = anyhow::Error;

    fn try_from(wire: LegacyPhase) -> Result<Self> {
        let target = match (wire.target_id, wire.target_name) {
            (None, None) => None,
            (Some(id), Some(name)) => {
                super::group_target::valid_chat_id(&id)?;
                Some(Target { id, name })
            }
            _ => bail!(summon_i18n::format!("r6f7f8fc874")),
        };
        let Some(selection) = wire.selection else {
            ensure!(
                !wire.committed && target.is_none(),
                summon_i18n::format!("r18a6526821")
            );
            return Ok(Self::Choosing);
        };
        match &selection {
            Selection::New(name) => {
                ui::validate_name(name).map_err(anyhow::Error::msg)?;
            }
            Selection::Existing(group) => {
                super::group_target::valid_chat_id(&group.chat_id)?;
                ui::validate_name(&group.name).map_err(anyhow::Error::msg)?;
            }
        }
        if let Some(target) = &target {
            Self::validate_target(&selection, target)?;
        }
        if wire.committed {
            Ok(Self::Committed {
                selection,
                target: target.context(summon_i18n::text!("r899a70a5a9"))?,
            })
        } else {
            // 旧实现只会在外部操作开始前持久化 selection；恢复时必须保留这道锁。
            Ok(Self::Locked { selection, target })
        }
    }
}

impl Serialize for RequestState {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let (selection, target, committed) = match self {
            Self::Choosing => (None, None, false),
            Self::Selected(_) => {
                return Err(serde::ser::Error::custom(summon_i18n::text!("r5742254030")))
            }
            Self::Locked { selection, target } => (Some(selection), target.as_ref(), false),
            Self::Committed { selection, target } => (Some(selection), Some(target), true),
        };
        LegacyPhase {
            selection: selection.cloned(),
            target_id: target.map(|target| target.id.clone()),
            target_name: target.map(|target| target.name.clone()),
            committed,
        }
        .serialize(serializer)
    }
}

impl RequestState {
    fn selection(&self) -> Option<&Selection> {
        match self {
            Self::Choosing => None,
            Self::Selected(selection)
            | Self::Locked { selection, .. }
            | Self::Committed { selection, .. } => Some(selection),
        }
    }

    fn is_committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    fn select(&mut self, selection: Selection) -> Result<()> {
        match self {
            Self::Choosing | Self::Selected(_) => *self = Self::Selected(selection),
            Self::Locked { selection: old, .. } | Self::Committed { selection: old, .. } => {
                ensure!(*old == selection, summon_i18n::format!("r5d0a3c71da"));
            }
        }
        Ok(())
    }

    fn lock(&mut self) -> Result<()> {
        match self {
            Self::Selected(selection) => {
                *self = Self::Locked {
                    selection: selection.clone(),
                    target: None,
                };
            }
            Self::Locked { .. } => {}
            _ => bail!(summon_i18n::format!("rf9a2226dbe")),
        }
        Ok(())
    }

    fn validate_target(selection: &Selection, target: &Target) -> Result<()> {
        let expected_name = match selection {
            Selection::New(name) => name,
            Selection::Existing(group) => {
                ensure!(
                    group.chat_id == target.id,
                    summon_i18n::format!("r97712bb19b")
                );
                &group.name
            }
        };
        ensure!(
            *expected_name == target.name,
            summon_i18n::format!("r5e2b023814")
        );
        Ok(())
    }

    fn identify_target(&mut self, id: String, name: String) -> Result<()> {
        let target = Target { id, name };
        let Self::Locked {
            selection,
            target: old,
        } = self
        else {
            bail!(summon_i18n::format!("r3cb16a037c"));
        };
        Self::validate_target(selection, &target)?;
        ensure!(
            old.as_ref().is_none_or(|old| *old == target),
            summon_i18n::format!("r1437831cd9")
        );
        *old = Some(target);
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let Self::Locked {
            selection,
            target: Some(target),
        } = self
        else {
            bail!(summon_i18n::format!("rc5969c69b8"));
        };
        *self = Self::Committed {
            selection: selection.clone(),
            target: target.clone(),
        };
        Ok(())
    }
}

pub(super) fn command_argument(
    agent: &Agent,
    text: &str,
    mentions: &[summon_core::ipc::Mention],
    blacklist: &SlashBlacklist,
) -> Option<String> {
    let cli = summon_core::driver::CliDriver::from_kind(agent.cli_kind());
    let command = summon_core::ipc::slash_command_text(text, mentions, cli)?;
    match slash_pool::plan_slash(
        cli,
        agent.capture_mode(),
        &command,
        blacklist,
        &SlashToggles::default(),
    ) {
        SlashPlan::Fork { target } if summon_core::fork::parse_invite_code(&target).is_none() => {
            Some(target)
        }
        _ => None,
    }
}

fn save(path: &Path, request: &Request) -> Result<()> {
    atomic_write(path, &serde_json::to_vec(request)?)
}

fn live_agent(env: &Environment<'_>, request: &Request) -> Result<Agent> {
    let agent = env
        .agent
        .reread()
        .context(summon_i18n::text!("rd94b05ec51"))?;
    ensure!(agent.is_routable(), summon_i18n::format!("r84bc460630"));
    ensure!(
        request.scope == env.scope && super::agent_env::feishu_delivery_scope(&agent) == env.scope,
        summon_i18n::format!("r1eb1eee787")
    );
    ensure!(
        agent.can_operate(
            &request.requester,
            &request.union_id,
            true,
            OperateScope::Chat(&request.source_chat)
        ),
        summon_i18n::format!("rc8627704f1")
    );
    Ok(agent)
}

fn validate_callback(request: &Request, card: &CardActionMsg, now: i64) -> Result<()> {
    ensure!(
        request.card_id.as_deref() == Some(card.message_id.as_str())
            && request.source_chat == card.chat_id
            && request.requester == card.operator_open_id,
        summon_i18n::format!("raf32ebf9aa")
    );
    ensure!(
        now >= request.issued_at && now - request.issued_at <= TTL_SECONDS,
        summon_i18n::format!("rae702eeff7")
    );
    Ok(())
}

fn validate_source(agent: &Agent, request: &Request) -> Result<()> {
    let plan = summon_core::fork::evaluate_fork_source(
        &request.source_tag,
        summon_core::driver::CliDriver::from_kind(agent.cli_kind()),
        &fork::source_state(agent, &request.source_session),
    )
    .map_err(|e| anyhow::anyhow!(e.message()))?;
    ensure!(
        plan.source_sid == request.source_sid,
        summon_i18n::format!("r5f4271e7b6")
    );
    ensure!(
        fork::source_work_dir(agent, &request.source_session).is_some(),
        summon_i18n::format!("r4a6d10ec07")
    );
    Ok(())
}

fn render(
    env: &Environment<'_>,
    ticket: &str,
    request: &mut Request,
    card: &str,
    path: &Path,
) -> Result<()> {
    if let Some(id) = &request.card_id {
        outbound::update_card_archived(
            env.fc,
            &env.agent.state_dir(),
            &request.source_type,
            &request.source_chat,
            id,
            card,
        )?;
    } else {
        let id = outbound::send_card_idempotent_archived_id(
            env.fc,
            &env.agent.state_dir(),
            &request.source_type,
            &request.source_chat,
            "fork_picker",
            card,
            &format!("fork-{ticket}"),
        )?;
        request.card_id = Some(id);
        save(path, request)?;
    }
    Ok(())
}

fn select(env: &Environment<'_>, request: &mut Request, input: ForkTargetInput) -> Result<()> {
    let agent = live_agent(env, request)?;
    validate_source(&agent, request)?;
    let groups = Groups::new(&agent, env.fc, &request.requester, &request.union_id);
    groups.prove_requester(&request.source_chat)?;
    let selection = match input {
        ForkTargetInput::New(name) => {
            ensure!(groups.is_owner(), summon_i18n::format!("rd11322f79d"));
            Some(Selection::New(name))
        }
        ForkTargetInput::Existing(name) => {
            let found = groups.search(&name, &request.source_chat)?;
            ensure!(!found.is_empty(), summon_i18n::format!("r28e5f47d4a"));
            ensure!(found.len() <= 20, summon_i18n::format!("r4b916188dc"));
            if found.len() == 1 {
                Some(Selection::Existing(found[0].clone()))
            } else {
                ensure!(
                    matches!(
                        request.state,
                        RequestState::Choosing | RequestState::Selected(_)
                    ),
                    summon_i18n::format!("r14c3e4f7c8")
                );
                request.candidates = found;
                request.state = RequestState::Choosing;
                None
            }
        }
        ForkTargetInput::Picker => None,
        ForkTargetInput::Code(_) => bail!(summon_i18n::format!("rd004a5265e")),
    };
    if let Some(selection) = selection {
        request.state.select(selection)?;
    }
    Ok(())
}

fn check_target(
    env: &Environment<'_>,
    agent: &Agent,
    request: &Request,
    id: &str,
    kind: &str,
) -> Result<String> {
    ensure!(
        id != request.source_chat,
        summon_i18n::format!("rcb80f19694")
    );
    let session =
        fork::target_session_name(agent, kind, id).context(summon_i18n::text!("r142ae4dd99"))?;
    ensure!(
        !env.chats.get(id).is_some_and(|handle| handle.local_claimed),
        summon_i18n::format!("r5904b79427")
    );
    ensure!(
        agent.can_operate(
            &request.requester,
            &request.union_id,
            true,
            OperateScope::Chat(id)
        ),
        summon_i18n::format!("r2eb03b025d")
    );
    let key = summon_core::bridge::ScopedChatKey::new(env.scope, &agent.name, id);
    ensure!(
        super::bridge::bridge_slot_message(
            &crate::bridge::store::BridgeStore::runtime_default(),
            &key
        )?
        .is_none(),
        summon_i18n::format!("r66d72482d7")
    );
    ensure!(
        !crate::remote::inbound::remote_bound(agent, id),
        summon_i18n::format!("ree9852627b")
    );
    Ok(session)
}

fn execute(env: &Environment<'_>, ticket: &str, request: &mut Request, path: &Path) -> Result<()> {
    let agent = live_agent(env, request)?;
    let host =
        summon_core::waiting::applink_host(&summon_core::config::Config::load().default_domain);
    if let RequestState::Committed { target, .. } = &request.state {
        let card = ui::fork_ready_card(&target.name, &target.id, host);
        return render(env, ticket, request, &card, path);
    }
    validate_source(&agent, request)?;
    let requester = request.requester.clone();
    let union_id = request.union_id.clone();
    let groups = Groups::new(&agent, env.fc, &requester, &union_id);
    groups.prove_requester(&request.source_chat)?;
    let selection = request
        .state
        .selection()
        .cloned()
        .context(summon_i18n::text!("rfab459dd3a"))?;
    let (id, expected_name) = match selection {
        Selection::New(name) => {
            request.state.lock()?;
            save(path, request)?;
            let card = ui::fork_pending_card(ticket, &name);
            render(env, ticket, request, &card, path)?;
            let id = groups.create(ticket, &name, || {
                let fresh = live_agent(env, request)?;
                validate_source(&fresh, request)
            })?;
            (id, name)
        }
        Selection::Existing(group) => (group.chat_id, group.name),
    };
    let (name, mode) = groups.info(&id)?;
    ensure!(name == expected_name, summon_i18n::format!("r0c9d9d15c6"));
    let kind = match mode.as_str() {
        "group" => "group",
        "topic" | "topic_group" => "topic_group",
        _ => bail!(summon_i18n::format!("r142ae4dd99")),
    };
    let session = check_target(env, &agent, request, &id, kind)?;
    // 提交后落收据前若进程退出，只认本次预铸的 SID，不能把别人的会话当成本次成功。
    let existing = crate::session::resume_store::inspect_record(&agent, &session);
    if matches!(&existing, crate::session::resume_store::ResumePresence::Valid(record) if record.id == request.new_sid && record.fork_pending)
    {
        request.state.lock()?;
        request.state.identify_target(id, name)?;
        request.state.commit()?;
        save(path, request)?;
        return execute(env, ticket, request, path);
    }
    let state = fork::target_state(&agent, &session, true);
    let target_tag = summon_core::registry::chat_tag(kind, &id);
    let plan = summon_core::fork::evaluate_fork(
        &request.source_tag,
        &target_tag,
        summon_core::driver::CliDriver::from_kind(agent.cli_kind()),
        &fork::source_state(&agent, &request.source_session),
        &state,
    )
    .map_err(|e| anyhow::anyhow!(e.message()))?;
    groups.bot_present(&id)?;
    let cwd = fork::source_work_dir(&agent, &request.source_session)
        .context(summon_i18n::text!("r4a6d10ec07"))?;
    // 预检通过后锁定选择；其后的外部操作只允许重试本次目标。
    request.state.lock()?;
    request.state.identify_target(id.clone(), name.clone())?;
    save(path, request)?;
    if matches!(request.state.selection(), Some(Selection::Existing(_))) {
        let card = ui::fork_pending_card(ticket, &name);
        render(env, ticket, request, &card, path)?;
    }
    groups.ensure_bot(ticket, &id)?;
    let fresh = live_agent(env, request)?;
    validate_source(&fresh, request)?;
    groups.prove_requester(&request.source_chat)?;
    check_target(env, &fresh, request, &id, kind)?;
    request.state.identify_target(id.clone(), name.clone())?;
    save(path, request)?;
    let at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    fork::commit_fork(
        &fresh,
        &plan,
        &fork::ForkTarget {
            session: &session,
            chat_id: &id,
            chat_type: kind,
            chat_name: Some(&name),
            cwd: &cwd,
        },
        &request.new_sid,
        &at,
    )?;
    request.state.commit()?;
    save(path, request)?;
    let card = ui::fork_ready_card(&name, &id, host);
    render(env, ticket, request, &card, path)
}

pub(super) fn start(env: &Environment<'_>, source: Source<'_>, argument: &str) -> Result<()> {
    ensure!(
        matches!(source.chat_type, "group" | "topic_group" | "topic"),
        summon_i18n::format!("r2c5aa7f2cf")
    );
    let input = ui::parse_fork_target(argument).map_err(anyhow::Error::msg)?;
    let agent = env
        .agent
        .reread()
        .context(summon_i18n::text!("rd94b05ec51"))?;
    ensure!(
        agent.can_operate(
            source.requester,
            source.union_id,
            true,
            OperateScope::Chat(source.chat_id)
        ),
        summon_i18n::format!("r6ccb60a0ff")
    );
    let tag = summon_core::registry::chat_tag(source.chat_type, source.chat_id);
    let (session, _) =
        fork::find_chat_by_tag(&agent, &tag).context(summon_i18n::text!("r831b5b7720"))?;
    let plan = summon_core::fork::evaluate_fork_source(
        &tag,
        summon_core::driver::CliDriver::from_kind(agent.cli_kind()),
        &fork::source_state(&agent, &session),
    )
    .map_err(|e| anyhow::anyhow!(e.message()))?;
    let ticket = summon_core::registry::chat_tag("group", source.message_id);
    with_record_lock(&agent, "fork-requests", &ticket, |path| {
        let mut request = if path.exists() {
            serde_json::from_slice::<Request>(&std::fs::read(path)?)?
        } else {
            Request {
                source_chat: source.chat_id.into(),
                source_type: source.chat_type.into(),
                source_session: session,
                source_tag: tag,
                source_sid: plan.source_sid,
                source_name: env.fc.resolve_chat_name(source.chat_id),
                scope: env.scope.into(),
                requester: source.requester.into(),
                union_id: source.union_id.into(),
                issued_at: chrono::Utc::now().timestamp(),
                card_id: None,
                candidates: Vec::new(),
                state: RequestState::Choosing,
                new_sid: uuid::Uuid::new_v4().to_string(),
            }
        };
        ensure!(
            request.requester == source.requester
                && request.source_chat == source.chat_id
                && request.scope == env.scope,
            summon_i18n::format!("r7a4d8ceb9b")
        );
        save(path, &request)?;
        if !request.state.is_committed() {
            select(env, &mut request, input)?;
        }
        if request.state.selection().is_some() {
            execute(env, &ticket, &mut request, path)
        } else {
            save(path, &request)?;
            let host = summon_core::waiting::applink_host(
                &summon_core::config::Config::load().default_domain,
            );
            let card = if request.candidates.is_empty() {
                ui::fork_picker_card(&ticket, &request.source_name)
            } else {
                ui::fork_candidates_card(&ticket, &request.candidates, host)
            };
            render(env, &ticket, &mut request, &card, path)
        }
    })
}

pub(super) fn callback(env: &Environment<'_>, card: &CardActionMsg) -> Result<()> {
    let ticket = card.value["ticket"]
        .as_str()
        .context(summon_i18n::text!("r87cdd1c51a"))?;
    ensure!(
        ticket.len() == 17
            && ticket
                .strip_prefix('g')
                .is_some_and(|suffix| suffix.bytes().all(|b| b.is_ascii_hexdigit())),
        summon_i18n::format!("re6f39252ef")
    );
    with_record_lock(env.agent, "fork-requests", ticket, |path| {
        let mut request: Request = serde_json::from_slice(
            &std::fs::read(path).context(summon_i18n::text!("r0f85e22f6d"))?,
        )?;
        validate_callback(&request, card, chrono::Utc::now().timestamp())?;
        live_agent(env, &request)?;
        if request.state.is_committed() {
            return execute(env, ticket, &mut request, path);
        }
        match card.value["action"].as_str() {
            Some("fork_submit") => {
                let name = ui::validate_name(
                    card.value["form_value"]["fork_name"]
                        .as_str()
                        .unwrap_or_default(),
                )
                .map_err(anyhow::Error::msg)?
                .to_string();
                let input = match card.value["mode"].as_str() {
                    Some("new") => ForkTargetInput::New(name),
                    Some("existing") => ForkTargetInput::Existing(name),
                    _ => bail!(summon_i18n::format!("rdf55e0b425")),
                };
                select(env, &mut request, input)?;
            }
            Some("fork_choose") => {
                let index = card.value["index"]
                    .as_u64()
                    .context(summon_i18n::text!("ra5066f62a6"))?;
                let candidate = usize::try_from(index)
                    .ok()
                    .and_then(|i| request.candidates.get(i))
                    .context(summon_i18n::text!("r592b5ff633"))?
                    .clone();
                request.state.select(Selection::Existing(candidate))?;
            }
            Some("fork_execute") => {
                ensure!(
                    request.state.selection().is_some(),
                    summon_i18n::format!("r51601da908")
                );
            }
            _ => bail!(summon_i18n::format!("r62319cf962")),
        }
        if request.state.selection().is_some() {
            execute(env, ticket, &mut request, path)
        } else {
            save(path, &request)?;
            let host = summon_core::waiting::applink_host(
                &summon_core::config::Config::load().default_domain,
            );
            let content = ui::fork_candidates_card(ticket, &request.candidates, host);
            render(env, ticket, &mut request, &content, path)
        }
    })
}

#[cfg(test)]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Request {
        Request {
            source_chat: "oc_source".into(),
            source_type: "group".into(),
            source_session: "soul-test-source".into(),
            source_tag: "ab".into(),
            source_sid: "parent".into(),
            source_name: "Source".into(),
            scope: "scope".into(),
            requester: "ou_owner".into(),
            union_id: "on_owner".into(),
            issued_at: 100,
            card_id: Some("om_card".into()),
            candidates: vec![],
            state: RequestState::Choosing,
            new_sid: "child".into(),
        }
    }
    #[test]
    fn malformed_legacy_phase_combinations_are_rejected() {
        let baseline = serde_json::to_value(request()).unwrap();
        for fields in [
            serde_json::json!({"committed":true}),
            serde_json::json!({"target_id":"oc_target"}),
            serde_json::json!({"target_name":"Target"}),
            serde_json::json!({"target_id":"oc_target","target_name":"Target"}),
            serde_json::json!({"selection":{"New":"Target"},"target_id":"oc_target"}),
            serde_json::json!({"selection":{"New":"Target"},"target_id":"oc_target","target_name":"Other","committed":true}),
            serde_json::json!({"selection":{"Existing":{"chat_id":"oc_target","name":"Target","description":"","member_count":null}},"target_id":"oc_other","target_name":"Target","committed":true}),
        ] {
            let mut record = baseline.clone();
            record
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            assert!(
                serde_json::from_value::<Request>(record.clone()).is_err(),
                "{record}"
            );
        }
    }

    #[test]
    fn legacy_phase_records_round_trip_without_changing_the_wire_format() {
        let baseline = serde_json::to_value(request()).unwrap();
        for fields in [
            serde_json::json!({}),
            serde_json::json!({"selection":{"New":"Target"}}),
            serde_json::json!({"selection":{"New":"Target"},"target_id":"oc_target","target_name":"Target"}),
            serde_json::json!({"selection":{"New":"Target"},"target_id":"oc_target","target_name":"Target","committed":true}),
        ] {
            let mut record = baseline.clone();
            record
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let restored: Request = serde_json::from_value(record.clone()).unwrap();
            assert_eq!(serde_json::to_value(restored).unwrap(), record);
        }
    }

    #[test]
    fn transitions_lock_the_selection_and_require_an_identified_target_to_commit() {
        let mut state = RequestState::Choosing;
        assert!(state.lock().is_err());
        assert!(state.commit().is_err());
        state.select(Selection::New("First".into())).unwrap();
        state.select(Selection::New("Target".into())).unwrap();
        assert!(serde_json::to_value(&state).is_err());
        assert!(state
            .identify_target("oc_target".into(), "Target".into())
            .is_err());
        state.lock().unwrap();
        assert!(state.commit().is_err());
        assert!(state.select(Selection::New("Other".into())).is_err());
        state.select(Selection::New("Target".into())).unwrap();
        state
            .identify_target("oc_target".into(), "Target".into())
            .unwrap();
        assert!(state
            .identify_target("oc_other".into(), "Target".into())
            .is_err());
        state.commit().unwrap();
        assert!(state.is_committed());
        assert!(state.lock().is_err());
        assert!(state
            .identify_target("oc_other".into(), "Target".into())
            .is_err());
    }

    #[test]
    fn callbacks_require_same_sender_card_chat_and_lifetime() {
        let request = request();
        let card = CardActionMsg {
            chat_id: "oc_source".into(),
            message_id: "om_card".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "event".into(),
            value: serde_json::json!({}),
            wal_id: None,
            wal_replayed: false,
        };
        assert!(validate_callback(&request, &card, 101).is_ok());
        for field in ["chat", "card", "sender"] {
            let mut forged = card.clone();
            match field {
                "chat" => forged.chat_id = "oc_else".into(),
                "card" => forged.message_id = "om_else".into(),
                _ => forged.operator_open_id = "ou_else".into(),
            };
            assert!(validate_callback(&request, &forged, 101).is_err());
        }
        assert!(validate_callback(&request, &card, 701).is_err());
        assert!(validate_callback(&request, &card, 99).is_err());
    }
}
