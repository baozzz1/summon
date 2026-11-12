//! `chats.*`:按群看,一个群里每个 agent 各一节独立配置 —— 按群开关、免 @、本群操作名单、
//! 工作目录、本群说明文件、本群定时任务。agent 页签与群页面看的是同一份数据(都从这里取)。
//!
//! 群的来源是各 agent 盘上的按群状态(`.meta`、事件目录、tmux),加上 agent.yaml 里提到过的
//! chat id(本群操作名单、免 @、按群工作目录)。群名取 `.meta` 记下的那个,取不到只显示 chat id。

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;
use serde_yaml::Value as Yaml;
use summon_core::dashboard::authz::{Decision, Target};
use summon_core::registry::{expand_tilde, Agent};
use summon_core::settings::ChatOverrides;
use summon_core::yaml_edit::Edit;

use super::config::{self, yaml_at};
use super::types::{
    ChatAgentBrief, ChatAgentView, ChatDetail, ChatKind, ChatSummary, DocExists, JobStatus,
    KeyEffect, MentionlessJob, MentionlessView, OperatorsView, SaveResult, Scope, WorkDirSource,
    WorkDirView,
};
use super::{internal, invalid, outcome, params, reply, rfc3339_system, ApiEnv, MentionlessAsk};
use crate::runtime::daemon::dashboard_port;
use crate::runtime::dashboard::server::{ApiCtx, ApiError, ErrorCode};
use crate::runtime::dashboard::sessions::Principal;
use crate::runtime::dashboard::writer::{self, Change, WriteError};
use summon_core::dashboard::fields::Effect;
use summon_core::dashboard::RawKind;

// ── 盘上的群 ────────────────────────────────────────────────────────────

/// 一份 `.meta`(会话名 → chat)。
struct MetaRow {
    session: String,
    meta: summon_core::chat_meta::ChatMeta,
}

fn metas(agent: &Agent) -> Vec<MetaRow> {
    let dir = agent.state_dir().join("runtime").join("chats");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<MetaRow> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let session = path
                .file_name()?
                .to_str()?
                .strip_suffix(".meta")?
                .to_string();
            let text = std::fs::read_to_string(&path).ok()?;
            Some(MetaRow {
                session,
                meta: summon_core::chat_meta::ChatMeta::parse(&text),
            })
        })
        .collect();
    // 同一个 chat 有多份时最新的在后(与 daemon 挑会话的次序一致)。
    rows.sort_by(|a, b| (&a.meta.updated_at, &a.session).cmp(&(&b.meta.updated_at, &b.session)));
    rows
}

/// 盘上记下的 chat 类型;没有记录时按 id 猜(`ou_` 是私聊的对端,其余按群算 —— 群的判权更严)。
pub(crate) fn chat_type_of(agent: &Agent, chat_id: &str) -> String {
    metas(agent)
        .into_iter()
        .rev()
        .find(|row| row.meta.chat_id.as_deref() == Some(chat_id))
        .and_then(|row| row.meta.chat_type)
        .unwrap_or_else(|| {
            if chat_id.starts_with("ou_") {
                "p2p".to_string()
            } else {
                "group".to_string()
            }
        })
}

/// 这个 agent 记过的各个群的群名:同一个群有多份时取最新一份里有名字的(与 [`chat_name_of`] 一致)。
pub(crate) fn chat_names(agent: &Agent) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for row in metas(agent) {
        if let (Some(id), Some(name)) = (row.meta.chat_id, row.meta.chat_name) {
            out.insert(id, name);
        }
    }
    out
}

pub(crate) fn chat_name_of(agent: &Agent, chat_id: &str) -> Option<String> {
    metas(agent)
        .into_iter()
        .rev()
        .filter(|row| row.meta.chat_id.as_deref() == Some(chat_id))
        .find_map(|row| row.meta.chat_name)
}

/// 这个 agent 有本群开关值的群:`(群名或 chat id, 覆盖)`。
pub(crate) fn chat_overrides(agent: &Agent) -> Vec<(String, ChatOverrides)> {
    let mut out = Vec::new();
    for row in metas(agent) {
        let overrides = dashboard_port::read_chat_settings(agent, &row.session);
        if summon_core::settings::chat_scoped().all(|d| overrides.get(d.id).is_none()) {
            continue;
        }
        let label = row
            .meta
            .chat_name
            .clone()
            .or(row.meta.chat_id.clone())
            .unwrap_or(row.session);
        out.push((label, overrides));
    }
    out
}

/// agent.yaml 里提到过的 chat id。
fn configured_chats(agent: &Agent) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    if let Some(f) = &agent.feishu {
        ids.extend(f.chat_operators.keys().cloned());
        ids.extend(f.chat_reply_without_mention.keys().cloned());
    }
    if let Some(map) = agent
        .runtime
        .as_ref()
        .and_then(|r| r.chat_working_dirs.as_ref())
    {
        ids.extend(map.keys().cloned());
    }
    ids
}

/// 这个 agent 在这个群有几项独立配置。
fn override_count(agent: &Agent, chat_id: &str, session: &str) -> usize {
    let settings = dashboard_port::read_chat_settings(agent, session);
    let switches = summon_core::settings::chat_scoped()
        .filter(|d| settings.get(d.id).is_some())
        .count();
    let feishu = agent.feishu.as_ref();
    let flags = [
        feishu.is_some_and(|f| f.chat_reply_without_mention.get(chat_id) == Some(&true)),
        feishu.is_some_and(|f| f.chat_operators.get(chat_id).is_some_and(|v| !v.is_empty())),
        mapping_of(agent, chat_id).is_some(),
        dashboard_port::chat_cwd_override(agent, session).is_some(),
        chat_doc_path(agent, chat_id).is_some_and(|p| p.is_file()),
    ];
    switches + flags.iter().filter(|f| **f).count()
}

fn mapping_of(agent: &Agent, chat_id: &str) -> Option<String> {
    agent
        .runtime
        .as_ref()
        .and_then(|r| r.chat_working_dirs.as_ref())
        .and_then(|m| m.get(chat_id))
        .cloned()
}

/// 本群说明文件 `agents/<alias>/chats/<chat_id>/CLAUDE.md`(chat id 带分隔符的一律不认)。
pub(crate) fn chat_doc_path(agent: &Agent, chat_id: &str) -> Option<std::path::PathBuf> {
    crate::runtime::daemon::chat_instruction_path(agent, chat_id)
}

// ── 方法:列表与详情 ────────────────────────────────────────────────────

/// 群列表里的一行:类型、群名、在这个群里的 agent 与各自的最近活跃。
#[derive(Default)]
struct ChatRow {
    chat_type: String,
    name: Option<String>,
    members: Vec<(String, Option<SystemTime>)>,
}

pub(crate) fn list(ctx: &ApiCtx, _p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let tmux = env.port.tmux_sessions();
    let now = SystemTime::now();
    let mut chats: BTreeMap<String, ChatRow> = BTreeMap::new();
    let agents: Vec<Agent> = env
        .agents()
        .into_iter()
        .filter(|a| !a.is_steward())
        .collect();
    for agent in &agents {
        let mut seen = BTreeSet::new();
        for view in crate::session::discovery::discover_sessions(
            agent,
            &tmux,
            now,
            Duration::from_secs(600),
        ) {
            let Some(chat_id) = view.chat_id else {
                continue;
            };
            if !seen.insert(chat_id.clone()) {
                continue;
            }
            let entry = chats.entry(chat_id).or_insert_with(|| ChatRow {
                chat_type: view.chat_type.clone().unwrap_or_else(|| "group".into()),
                ..ChatRow::default()
            });
            if entry.name.is_none() {
                entry.name = view.chat_name.clone();
            }
            entry.members.push((agent.name.clone(), view.last_active));
        }
        for chat_id in configured_chats(agent) {
            if seen.insert(chat_id.clone()) {
                let chat_type = chat_type_of(agent, &chat_id);
                chats
                    .entry(chat_id)
                    .or_insert_with(|| ChatRow {
                        chat_type,
                        ..ChatRow::default()
                    })
                    .members
                    .push((agent.name.clone(), None));
            }
        }
    }
    let mut out: Vec<ChatSummary> = chats
        .into_iter()
        .map(
            |(
                chat_id,
                ChatRow {
                    chat_type,
                    name,
                    members,
                },
            )| {
                let briefs: Vec<ChatAgentBrief> = members
                    .iter()
                    .filter_map(|(alias, last)| {
                        let agent = agents.iter().find(|a| &a.name == alias)?;
                        let (session, live) = env
                            .port
                            .chat_state_among(agent, &chat_type, &chat_id, &tmux);
                        Some(ChatAgentBrief {
                            agent: alias.clone(),
                            overrides: override_count(agent, &chat_id, &session),
                            live,
                            last_active: last.map(rfc3339_system),
                        })
                    })
                    .collect();
                let last = members.iter().filter_map(|(_, t)| *t).max();
                ChatSummary {
                    kind: ChatKind::of(&chat_type),
                    name,
                    agents: briefs,
                    last_active: last.map(rfc3339_system),
                    chat_id,
                }
            },
        )
        .collect();
    // 最近活跃的在前;没有活动时间的按 chat id 排在后面。
    out.sort_by(|a, b| {
        b.last_active
            .cmp(&a.last_active)
            .then_with(|| a.chat_id.cmp(&b.chat_id))
    });
    reply(&out)
}

fn reason_of(decision: &Decision) -> Option<String> {
    match decision {
        Decision::Allow => None,
        Decision::Deny(why) => Some(why.clone()),
    }
}

const LIVE_REASON: &str = "r4e708cc592";

/// 一个 agent 在一个群的全部独立配置。
pub(crate) fn agent_view(
    env: &ApiEnv,
    ctx: &ApiCtx,
    agent: &Agent,
    chat_id: &str,
) -> Result<ChatAgentView, ApiError> {
    let chat_type = chat_type_of(agent, chat_id);
    let kind = ChatKind::of(&chat_type);
    let session = env.port.session_name(agent, &chat_type, chat_id);
    let chat_target = Target::Chat {
        agent: &agent.name,
        chat_id,
    };
    let chat_decision = env.decide(ctx, &chat_target);
    let agent_decision = env.decide(ctx, &Target::Agent(&agent.name));
    let feishu = agent.feishu.as_ref();

    let enabled = feishu.is_some_and(|f| f.chat_reply_without_mention.get(chat_id) == Some(&true));
    let enable_block = enable_refusal(ctx, agent, kind);
    let mentionless = MentionlessView {
        enabled,
        can_disable: enabled && chat_decision.is_allowed(),
        can_enable: !enabled && chat_decision.is_allowed() && enable_block.is_none(),
        reason: if chat_decision.is_allowed() {
            (!enabled).then_some(enable_block).flatten()
        } else {
            reason_of(&chat_decision)
        },
    };

    let ids: Vec<String> = feishu
        .and_then(|f| f.chat_operators.get(chat_id))
        .cloned()
        .unwrap_or_default();
    let manifest = std::fs::read(agent.manifest_path()).ok();
    let operators = OperatorsView {
        members: super::agents::members(env, agent, ids),
        revision: writer::revision(manifest.as_deref()),
        editable: agent_decision.is_allowed(),
        reason: reason_of(&agent_decision),
    };

    let live = env.port.chat_live(agent, &chat_type, chat_id);
    let effective =
        crate::runtime::daemon::effective_work_dir_with_source(agent, &session, Some(chat_id));
    let work_dir = WorkDirView {
        effective: effective
            .as_ref()
            .map(|(p, _)| summon_core::registry::collapse_tilde(p)),
        source: effective.as_ref().and_then(|(_, source)| match source {
            summon_core::work_dir::WorkDirSource::ChatOverride => Some(WorkDirSource::ChatOverride),
            summon_core::work_dir::WorkDirSource::ChatMapping => Some(WorkDirSource::ChatMapping),
            summon_core::work_dir::WorkDirSource::AgentDefault => Some(WorkDirSource::AgentDefault),
            summon_core::work_dir::WorkDirSource::Explicit => None,
        }),
        mapping: mapping_of(agent, chat_id),
        r#override: dashboard_port::chat_cwd_override(agent, &session)
            .map(|p| summon_core::registry::collapse_tilde(&p)),
        live,
        editable: !live && chat_decision.is_allowed(),
        reason: if live {
            Some(::summon_core::i18n::label(LIVE_REASON).to_string())
        } else {
            reason_of(&chat_decision)
        },
    };

    Ok(ChatAgentView {
        agent: agent.name.clone(),
        config: config::view(
            env,
            ctx,
            &Scope::Chat {
                agent: agent.name.clone(),
                chat_id: chat_id.to_string(),
            },
        )?,
        mentionless,
        operators,
        work_dir,
        doc: DocExists {
            exists: chat_doc_path(agent, chat_id).is_some_and(|p| p.is_file()),
        },
        schedules: super::schedules::views_for(env, ctx, &agent.name, chat_id),
    })
}

/// 开启免 @ 要证明「群里唯一的真人就是你」,只有从这个 agent 的飞书里打开时才证得了。
fn enable_refusal(ctx: &ApiCtx, agent: &Agent, kind: ChatKind) -> Option<String> {
    if agent.is_steward() {
        return Some(summon_i18n::text!("r2e703122b2").into());
    }
    if kind == ChatKind::P2p {
        return Some(summon_i18n::text!("r2f9a4ebbc9").into());
    }
    match &ctx.principal {
        Principal::Owner => Some(summon_i18n::text!("rc0a9c2e4cc").into()),
        Principal::Feishu(issuer) if issuer.agent != agent.name => {
            Some(summon_i18n::format!("r6302396d6a", agent.name, agent.name))
        }
        Principal::Feishu(_) => None,
    }
}

#[derive(Deserialize)]
struct ChatParams {
    chat_id: String,
}

pub(crate) fn get(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let ChatParams { chat_id } = params(p)?;
    let mut agents = Vec::new();
    let mut kind = None;
    let mut name = None;
    for agent in env.agents().into_iter().filter(|a| !a.is_steward()) {
        let on_disk = metas(&agent)
            .iter()
            .any(|row| row.meta.chat_id.as_deref() == Some(chat_id.as_str()));
        if !on_disk && !configured_chats(&agent).contains(&chat_id) {
            continue;
        }
        kind.get_or_insert_with(|| ChatKind::of(&chat_type_of(&agent, &chat_id)));
        if name.is_none() {
            name = chat_name_of(&agent, &chat_id);
        }
        agents.push(agent_view(env, ctx, &agent, &chat_id)?);
    }
    let Some(kind) = kind else {
        return Err(ApiError::new(
            ErrorCode::NotFound,
            summon_i18n::format!("r935f5b584f"),
        ));
    };
    reply(&ChatDetail {
        chat_id,
        kind,
        name,
        agents,
    })
}

// ── 方法:免 @ ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct MentionlessParams {
    agent: String,
    chat_id: String,
    enabled: bool,
}

fn job_view(id: String, status: JobStatus, message: String) -> MentionlessJob {
    MentionlessJob {
        job: id,
        status,
        message: Some(message),
        apply_url: None,
    }
}

pub(crate) fn set_mentionless(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: MentionlessParams = params(p)?;
    let keys = vec![format!("feishu.chat_reply_without_mention.{}", req.chat_id)];
    let result = (|| {
        let agent = env.agent(&req.agent)?;
        env.require(
            ctx,
            &Target::Chat {
                agent: &agent.name,
                chat_id: &req.chat_id,
            },
        )?;
        let (tx, rx) = std::sync::mpsc::channel();
        let id = env.add_job(rx)?;
        if !req.enabled {
            // 关闭直接写,与群里 `/mentionless off` 同一个改写函数。
            let chat_id = req.chat_id.clone();
            let saved = writer::write_yaml(
                &agent.manifest_path(),
                RawKind::Agent,
                None,
                Change::Patch(Box::new(move |current: &str| {
                    dashboard_port::mentionless_patch(current, &chat_id, false)
                })),
                &config::agent_guard(ctx, Some(&agent)),
            )?;
            if !saved.unchanged {
                env.port.agent_written(&agent.name);
            }
            let view = job_view(
                id,
                JobStatus::Done,
                summon_i18n::text!("rc121d87132").into(),
            );
            env.finished_job(view.clone());
            return Ok(view);
        }
        let chat_type = chat_type_of(&agent, &req.chat_id);
        if let Some(why) = enable_refusal(ctx, &agent, ChatKind::of(&chat_type)) {
            return Err(ApiError::new(ErrorCode::Forbidden, why));
        }
        let Principal::Feishu(issuer) = &ctx.principal else {
            return Err(ApiError::new(
                ErrorCode::Forbidden,
                summon_i18n::text!("reec18236cf"),
            ));
        };
        // 核验在该 agent 的有界后台队列里跑(与 `/mentionless on` 同一段代码),结果经 tx 交回。
        let ask = MentionlessAsk {
            chat_id: req.chat_id.clone(),
            chat_type,
            open_id: issuer.open_id.clone(),
            union_id: issuer.union_id.clone(),
            reply: tx,
        };
        if let Err(why) = env.port.mentionless(&agent.name, ask) {
            let view = job_view(id, JobStatus::Failed, summon_i18n::format!("rf7836b833e"));
            env.finished_job(view.clone());
            return Ok(view);
        }
        Ok(MentionlessJob {
            job: id,
            status: JobStatus::Pending,
            message: Some(summon_i18n::text!("rb0d4e257b6").into()),
            apply_url: None,
        })
    })();
    env.audit(
        ctx,
        &format!("agent:{}", req.agent),
        &keys,
        &outcome(&result),
    );
    reply(&result?)
}

#[derive(Deserialize)]
struct JobParams {
    job: String,
}

pub(crate) fn mentionless_job(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let JobParams { job } = params(p)?;
    let view = env
        .poll_job(&job)
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, summon_i18n::text!("r16cc30771d")))?;
    reply(&view)
}

// ── 方法:本群操作名单 ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct OperatorsParams {
    agent: String,
    chat_id: String,
    revision: String,
    members: Vec<String>,
}

pub(crate) fn set_operators(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: OperatorsParams = params(p)?;
    let keys = vec![format!("feishu.chat_operators.{}", req.chat_id)];
    let result = (|| {
        let agent = env.agent(&req.agent)?;
        // 名单存在 agent.yaml 里,效果是 agent 的授权面:agent 范围。
        env.require(ctx, &Target::Agent(&agent.name))?;
        if req.chat_id.is_empty() || req.chat_id.contains(['.', '/']) {
            return Err(invalid(summon_i18n::text!("r5169498fc1")));
        }
        let mut members: Vec<String> = Vec::new();
        for id in &req.members {
            let id = id.trim();
            if id.is_empty() || members.iter().any(|m| m == id) {
                continue;
            }
            if !(id.starts_with("ou_") || id.starts_with("on_")) {
                return Err(
                    invalid(summon_i18n::format!("r1b8479e788")).with_key("feishu.chat_operators")
                );
            }
            members.push(id.to_string());
        }
        let path = vec![
            "feishu".to_string(),
            "chat_operators".to_string(),
            req.chat_id.clone(),
        ];
        let edit = if members.is_empty() {
            Edit::Remove(path)
        } else {
            Edit::Set(
                path,
                Yaml::Sequence(members.into_iter().map(Yaml::String).collect()),
            )
        };
        let saved = writer::write_yaml(
            &agent.manifest_path(),
            RawKind::Agent,
            Some(&req.revision),
            Change::Edits(vec![edit]),
            &config::agent_guard(ctx, Some(&agent)),
        )?;
        if !saved.unchanged {
            env.port.agent_written(&agent.name);
        }
        Ok(SaveResult {
            revision: saved.revision,
            effects: vec![KeyEffect {
                key: "feishu.chat_operators".into(),
                effect: Effect::Immediate,
            }],
            restart_pending: env.restart_pending(),
        })
    })();
    env.audit(
        ctx,
        &format!("agent:{}", req.agent),
        &keys,
        &outcome(&result),
    );
    reply(&result?)
}

// ── 方法:工作目录 ──────────────────────────────────────────────────────

fn work_dir_map(raw: &Yaml) -> BTreeMap<String, String> {
    yaml_at(raw, "runtime.chat_working_dirs")
        .and_then(Yaml::as_mapping)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn changed_chats(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|k| before.get(*k) != after.get(*k))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn require_dir(dir: &str) -> Result<(), ApiError> {
    if std::path::Path::new(&expand_tilde(dir.trim())).is_dir() {
        Ok(())
    } else {
        Err(invalid(summon_i18n::format!("rf091047a8a", dir.trim()))
            .with_key("runtime.chat_working_dirs"))
    }
}

/// 改 `runtime.chat_working_dirs` 前的检查:改到的群都不能有活会话,新目录必须存在。
pub(crate) fn check_work_dir_map(
    env: &ApiEnv,
    agent: &Agent,
    current: &Yaml,
    next: &BTreeMap<&str, Option<Yaml>>,
) -> Result<(), ApiError> {
    let Some(new) = next.get("runtime.chat_working_dirs") else {
        return Ok(());
    };
    let before = work_dir_map(current);
    let mut wrapped = serde_yaml::Mapping::new();
    if let Some(new) = new {
        let mut runtime = serde_yaml::Mapping::new();
        runtime.insert("chat_working_dirs".into(), new.clone());
        wrapped.insert("runtime".into(), Yaml::Mapping(runtime));
    }
    let after = work_dir_map(&Yaml::Mapping(wrapped));
    for chat_id in changed_chats(&before, &after) {
        let chat_type = chat_type_of(agent, &chat_id);
        if env.port.chat_live(agent, &chat_type, &chat_id) {
            return Err(ApiError::new(
                ErrorCode::Busy,
                format!(
                    "{chat_id}:{LIVE_REASON}",
                    LIVE_REASON = ::summon_core::i18n::label(LIVE_REASON)
                ),
            )
            .with_key("runtime.chat_working_dirs"));
        }
        if let Some(dir) = after.get(&chat_id) {
            require_dir(dir)?;
        }
    }
    Ok(())
}

/// 工作目录变了之后,把续聊记录里的 cwd 同步成新的生效目录 —— 下一次会话起进程与定位转录
/// 都从记录的 cwd 出发,两边必须是同一个目录。没有记录时什么也不做。
fn sync_resume_cwd(env: &ApiEnv, agent: &Agent, chat_id: &str) {
    let chat_type = chat_type_of(agent, chat_id);
    let session = env.port.session_name(agent, &chat_type, chat_id);
    if let Some((dir, _)) =
        crate::runtime::daemon::effective_work_dir_with_source(agent, &session, Some(chat_id))
    {
        crate::session::resume_store::update_cwd(agent, &session, &dir);
    }
}

/// `config.patch` 改了整张 `chat_working_dirs` 之后,逐个改到的群同步续聊记录。
pub(crate) fn sync_resume_cwd_all(env: &ApiEnv, alias: &str, before: &Yaml, after_text: &str) {
    let Ok(agent) = env.agent(alias) else { return };
    let after: Yaml = serde_yaml::from_str(after_text).unwrap_or(Yaml::Null);
    for chat_id in changed_chats(&work_dir_map(before), &work_dir_map(&after)) {
        sync_resume_cwd(env, &agent, &chat_id);
    }
}

#[derive(Deserialize)]
struct WorkDirParams {
    agent: String,
    chat_id: String,
    /// agent.yaml 的版本(按群映射存在那里)。
    revision: String,
    mapping: Option<String>,
    clear_override: bool,
}

pub(crate) fn set_work_dir(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: WorkDirParams = params(p)?;
    let mut keys = Vec::new();
    let result = (|| {
        let agent = env.agent(&req.agent)?;
        let chat_id = req.chat_id.as_str();
        env.require(
            ctx,
            &Target::Chat {
                agent: &agent.name,
                chat_id,
            },
        )?;
        if chat_id.is_empty() || chat_id.contains(['.', '/']) {
            return Err(invalid(summon_i18n::text!("r5169498fc1")));
        }
        let chat_type = chat_type_of(&agent, chat_id);
        // 不往在跑的会话里注入 /cd:证不了进程目录与转录定位会一致,有活会话就不改。
        if env.port.chat_live(&agent, &chat_type, chat_id) {
            return Err(ApiError::new(
                ErrorCode::Busy,
                ::summon_core::i18n::label(LIVE_REASON),
            ));
        }
        // 映射与覆盖都是照着页面读到的那一版改的:那一版之后别处改过就整个不动。
        let manifest = std::fs::read(agent.manifest_path()).map_err(internal)?;
        if writer::revision(Some(&manifest)) != req.revision {
            return Err(WriteError::Conflict.into());
        }
        let session = env.port.session_name(&agent, &chat_type, chat_id);
        let mapping = req
            .mapping
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty());
        let current = mapping_of(&agent, chat_id);
        let mut changed = false;
        if mapping != current.as_deref() {
            keys.push(format!("runtime.chat_working_dirs.{chat_id}"));
            let path = vec![
                "runtime".to_string(),
                "chat_working_dirs".to_string(),
                chat_id.to_string(),
            ];
            let edit = match mapping {
                Some(dir) => {
                    require_dir(dir)?;
                    Edit::Set(path, Yaml::String(dir.to_string()))
                }
                None => Edit::Remove(path),
            };
            let saved = writer::write_yaml(
                &agent.manifest_path(),
                RawKind::Agent,
                Some(&req.revision),
                Change::Edits(vec![edit]),
                &config::agent_guard(ctx, Some(&agent)),
            )?;
            changed |= !saved.unchanged;
        }
        if req.clear_override {
            keys.push(format!("chat_cwd.{chat_id}"));
            changed |= dashboard_port::clear_chat_cwd(&agent, &session)
                .map_err(|e| internal(summon_i18n::format!("r6749849980")))?;
        }
        let fresh = env.agent(&agent.name)?;
        if changed {
            sync_resume_cwd(env, &fresh, chat_id);
            env.port.agent_written(&agent.name);
        }
        agent_view(env, ctx, &fresh, chat_id)
    })();
    env.audit(
        ctx,
        &format!("agent:{}", req.agent),
        &keys,
        &outcome(&result),
    );
    reply(&result?)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::runtime::dashboard::sessions::ChatKind as IssuerKind;
    use serde_json::json;

    fn kit() -> (Kit, Agent) {
        let kit = Kit::new();
        let a = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&a, "group", GROUP, "示例群");
        kit.add_chat(&a, "p2p", P2P, "");
        (kit, a)
    }

    #[test]
    fn list_and_get_show_each_agent_section() {
        let (kit, a) = kit();
        let b = kit.add_agent("arcadia", &agent_yaml("arcadia", "阿卡迪亚", "codex", ""));
        kit.add_chat(&b, "group", GROUP, "示例群");
        kit.port
            .live
            .lock()
            .unwrap()
            .insert(("starlight".into(), GROUP.into()));
        let ctx = kit.owner();
        let list = kit.call(&ctx, super::list, json!({})).unwrap();
        let group = list
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["chat_id"] == GROUP)
            .unwrap();
        assert_eq!(group["name"], "示例群");
        assert_eq!(group["kind"], "group");
        let agents: Vec<(&str, bool)> = group["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (a["agent"].as_str().unwrap(), a["live"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            agents,
            [("arcadia", false), ("starlight", true)],
            "按 agent 名排"
        );
        let p2p = list
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["chat_id"] == P2P)
            .unwrap();
        assert_eq!(p2p["kind"], "p2p");

        let detail = kit.call(&ctx, get, json!({ "chat_id": GROUP })).unwrap();
        assert_eq!(detail["agents"].as_array().unwrap().len(), 2);
        let section = detail["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["agent"] == "starlight")
            .unwrap();
        assert_eq!(section["config"]["scope"]["level"], "chat");
        assert_eq!(section["work_dir"]["live"], true);
        assert_eq!(section["work_dir"]["editable"], false);
        assert_eq!(section["work_dir"]["source"], "agent_default");
        assert_eq!(
            section["operators"]["revision"],
            writer::revision(Some(kit.read("agents/starlight/agent.yaml").as_bytes()))
        );
        assert_eq!(
            section["mentionless"]["can_enable"], false,
            "本机打开不能开启"
        );
        let _ = a;
        let err = kit
            .call(
                &ctx,
                get,
                json!({"chat_id": "oc_00000000000000000000000000000003"}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    fn manifest_rev(kit: &Kit) -> String {
        writer::revision(Some(kit.read("agents/starlight/agent.yaml").as_bytes()))
    }

    /// 读取之后 agent.yaml 被别处改过:改映射、清覆盖都回 conflict,什么也不动。
    #[test]
    fn work_dir_rejects_stale_revision() {
        let (kit, a) = kit();
        let stale = manifest_rev(&kit);
        let session = a.tmux_name_for_chat("group", GROUP, false);
        let cwd_file = a
            .state_dir()
            .join("runtime/chats")
            .join(format!("{session}.cwd"));
        std::fs::write(&cwd_file, "/var\n").unwrap();
        let elsewhere = format!(
            "{}model: changed-elsewhere\n",
            kit.read("agents/starlight/agent.yaml")
        );
        std::fs::write(a.manifest_path(), &elsewhere).unwrap();
        for (mapping, clear) in [(json!("/tmp"), false), (Value::Null, true)] {
            let err = kit
                .call(
                    &kit.owner(),
                    set_work_dir,
                    json!({"agent": "starlight", "chat_id": GROUP, "revision": stale, "mapping": mapping, "clear_override": clear}),
                )
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Conflict, "{mapping} {clear}");
            assert_eq!(kit.read("agents/starlight/agent.yaml"), elsewhere);
            assert!(cwd_file.exists(), "清覆盖也要先过版本校验");
        }
    }

    #[test]
    fn work_dir_change_refused_while_session_live() {
        let (kit, _a) = kit();
        kit.port
            .live
            .lock()
            .unwrap()
            .insert(("starlight".into(), GROUP.into()));
        let before = kit.read("agents/starlight/agent.yaml");
        let err = kit
            .call(
                &kit.owner(),
                set_work_dir,
                json!({"agent": "starlight", "chat_id": GROUP, "revision": manifest_rev(&kit), "mapping": "/tmp", "clear_override": false}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
        assert!(err.message.contains("/quit"));
        assert_eq!(kit.read("agents/starlight/agent.yaml"), before);
        // config.patch 改整张映射也一样挡住。
        let scope = json!({"level": "agent", "agent": "starlight"});
        let rev = writer::revision(Some(before.as_bytes()));
        let err = kit
            .call(
                &kit.owner(),
                super::super::config::patch,
                json!({"scope": scope, "revision": rev, "changes": [
                    {"key": "runtime.chat_working_dirs", "value": {GROUP: "/tmp"}}
                ]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
    }

    #[test]
    fn work_dir_mapping_and_override_without_live_session() {
        let (kit, a) = kit();
        let target = kit.dir.path().join("workspace");
        std::fs::create_dir_all(&target).unwrap();
        let session = a.tmux_name_for_chat("group", GROUP, false);
        // 一份续聊记录与一个 /cd 覆盖。
        let cwd_file = a
            .state_dir()
            .join("runtime/chats")
            .join(format!("{session}.cwd"));
        std::fs::write(&cwd_file, "/var\n").unwrap();
        let record = crate::session::resume_store::record_path(&a, &session);
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        std::fs::write(
            &record,
            summon_core::session_id::format_full_record(&summon_core::session_id::SessionRecord {
                cli: "claude".into(),
                id: "00000000-0000-4000-8000-000000000001".into(),
                cwd: Some("/var".into()),
                ..Default::default()
            }),
        )
        .unwrap();
        let ctx = kit.owner();
        let view = kit
            .call(
                &ctx,
                set_work_dir,
                json!({"agent": "starlight", "chat_id": GROUP, "revision": manifest_rev(&kit), "mapping": target.to_str().unwrap(), "clear_override": false}),
            )
            .unwrap();
        assert_eq!(
            view["work_dir"]["source"], "chat_override",
            "/cd 的覆盖仍优先"
        );
        assert_eq!(view["work_dir"]["mapping"], target.to_str().unwrap());
        let view = kit
            .call(
                &ctx,
                set_work_dir,
                json!({"agent": "starlight", "chat_id": GROUP, "revision": manifest_rev(&kit), "mapping": target.to_str().unwrap(), "clear_override": true}),
            )
            .unwrap();
        assert_eq!(view["work_dir"]["source"], "chat_mapping");
        assert!(view["work_dir"].get("override").is_none());
        assert!(!cwd_file.exists());
        let rec = crate::session::resume_store::read_record(&a, &session).unwrap();
        assert_eq!(rec.cwd.as_deref(), target.to_str(), "续聊记录跟着新目录走");
        // 不存在的目录拒绝。
        let err = kit
            .call(
                &ctx,
                set_work_dir,
                json!({"agent": "starlight", "chat_id": GROUP, "revision": manifest_rev(&kit), "mapping": "/no/such/dir", "clear_override": false}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        // 清掉映射。
        let view = kit
            .call(
                &ctx,
                set_work_dir,
                json!({"agent": "starlight", "chat_id": GROUP, "revision": manifest_rev(&kit), "mapping": null, "clear_override": false}),
            )
            .unwrap();
        assert_eq!(view["work_dir"]["source"], "agent_default");
    }

    #[test]
    fn set_operators_writes_agent_yaml_and_refuses_lockout() {
        let (kit, a) = kit();
        let yaml = agent_yaml("starlight", "星光", "claude", "").replace(
            &format!("operators: [{ME_UNION}]"),
            &format!("operators: [{PEER_UNION}]\n  chat_operators:\n    {GROUP}: [{ME_UNION}]"),
        );
        std::fs::write(a.manifest_path(), &yaml).unwrap();
        let rev = writer::revision(Some(yaml.as_bytes()));
        let owner = kit.owner();
        let saved = kit
            .call(
                &owner,
                set_operators,
                json!({"agent": "starlight", "chat_id": GROUP, "revision": rev, "members": [ME_UNION, PEER_UNION, ME_UNION]}),
            )
            .unwrap();
        let text = kit.read("agents/starlight/agent.yaml");
        let parsed: Yaml = serde_yaml::from_str(&text).unwrap();
        assert_eq!(
            yaml_at(&parsed, &format!("feishu.chat_operators.{GROUP}")).unwrap(),
            &serde_yaml::from_str::<Yaml>(&format!("[{ME_UNION}, {PEER_UNION}]")).unwrap()
        );
        // 只在本群名单里的人:改别的群的名单是 agent 范围,不让。
        let feishu = kit.feishu(issuer("starlight", GROUP, IssuerKind::Group));
        let err = kit
            .call(
                &feishu,
                set_operators,
                json!({"agent": "starlight", "chat_id": GROUP, "revision": saved["revision"], "members": [PEER_UNION]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        // 旧版本号:冲突。
        let err = kit
            .call(
                &owner,
                set_operators,
                json!({"agent": "starlight", "chat_id": GROUP, "revision": rev, "members": []}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        let err = kit
            .call(
                &owner,
                set_operators,
                json!({"agent": "starlight", "chat_id": GROUP, "revision": saved["revision"], "members": ["bob"]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
    }

    #[test]
    fn owner_cannot_enable_mentionless() {
        let (kit, _a) = kit();
        let err = kit
            .call(
                &kit.owner(),
                set_mentionless,
                json!({"agent": "starlight", "chat_id": GROUP, "enabled": true}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(err.message.contains("/mentionless on"));
        assert!(kit.port.asks.lock().unwrap().is_empty(), "没进核验队列");
    }

    #[test]
    fn mentionless_enable_goes_through_the_queue_and_disable_writes_directly() {
        let (kit, _a) = kit();
        let ctx = kit.feishu(issuer("starlight", GROUP, IssuerKind::Group));
        let job = kit
            .call(
                &ctx,
                set_mentionless,
                json!({"agent": "starlight", "chat_id": GROUP, "enabled": true}),
            )
            .unwrap();
        assert_eq!(job["status"], "pending");
        let id = job["job"].as_str().unwrap().to_string();
        let polled = kit
            .call(&ctx, mentionless_job, json!({ "job": id }))
            .unwrap();
        assert_eq!(polled["status"], "pending");
        // dispatcher 那头的核验结束(这里由桩端口代答)。
        let (alias, ask) = kit.port.asks.lock().unwrap().pop().unwrap();
        assert_eq!(alias, "starlight");
        assert_eq!(
            (ask.open_id.as_str(), ask.union_id.as_str()),
            (ME_OPEN, ME_UNION)
        );
        assert_eq!(ask.chat_type, "group");
        ask.reply
            .send(super::super::MentionlessDone {
                ok: false,
                message: "当前应用缺少权限".into(),
                apply_url: Some("https://open.feishu.cn/app/x/auth".into()),
            })
            .unwrap();
        let polled = kit
            .call(&ctx, mentionless_job, json!({ "job": id }))
            .unwrap();
        assert_eq!(polled["status"], "failed");
        assert_eq!(polled["apply_url"], "https://open.feishu.cn/app/x/auth");
        assert!(!kit
            .read("agents/starlight/agent.yaml")
            .contains("chat_reply_without_mention"));

        // 关闭:直接写(用 /mentionless off 同一个改写函数)。
        let yaml = format!(
            "{}  chat_reply_without_mention: {{\"{GROUP}\":true}}\n",
            agent_yaml("starlight", "星光", "claude", "")
        );
        std::fs::write(kit.agents_dir().join("starlight/agent.yaml"), yaml).unwrap();
        let done = kit
            .call(
                &ctx,
                set_mentionless,
                json!({"agent": "starlight", "chat_id": GROUP, "enabled": false}),
            )
            .unwrap();
        assert_eq!(done["status"], "done");
        let agent = kit.env().agent("starlight").unwrap();
        assert!(!agent.replies_without_mention(GROUP));
        let other = kit.feishu(issuer("starlight", P2P, IssuerKind::P2p));
        let err = kit
            .call(
                &other,
                set_mentionless,
                json!({"agent": "starlight", "chat_id": P2P, "enabled": true}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "私聊不需要免 @");
        let err = kit
            .call(&ctx, mentionless_job, json!({"job": "f".repeat(32)}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }
}
