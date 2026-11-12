//! `schedules.*`:查看、停用 / 重新启用、编辑。全部经定时器存储层(`ScheduleStore`)的既有接口与锁,
//! 与 `summon schedules` 同一套代码;控制台从不直接写 `schedules.yaml`,也不新建定时器
//! (归属来自轮凭证,只有 agent 经 `summon remind` 能建)。

use serde::Deserialize;
use serde_json::Value;
use summon_core::dashboard::authz::Target;
use summon_core::schedule::{self, next_fire::next_fires, Kind, Schedule};
use summon_core::{ScheduleContext, ScheduleOutput};

use super::types::{ScheduleEdit, ScheduleView, WhenCheck};
use super::{internal, invalid, outcome, params, reply, rfc3339, ApiEnv};
use crate::runtime::dashboard::server::{ApiCtx, ApiError, ErrorCode};
use crate::schedule::schedule_store::SchedulePatch;

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Once => "once",
        Kind::Interval => "interval",
        Kind::Cron => "cron",
    }
}

fn output_name(output: ScheduleOutput) -> &'static str {
    match output {
        ScheduleOutput::Progress => "progress",
        ScheduleOutput::Result => "result",
    }
}

fn context_name(context: ScheduleContext) -> &'static str {
    match context {
        ScheduleContext::Resume => "resume",
        ScheduleContext::Fresh => "fresh",
    }
}

/// 一次请求里渲染多条定时任务:判权快照与群名只取一次(一次性定时器触发后也归档,条数会很多)。
pub(crate) struct Viewer<'a> {
    ctx: &'a ApiCtx,
    agents: Vec<summon_core::registry::Agent>,
    snapshot: summon_core::dashboard::authz::Snapshot,
    names: std::cell::RefCell<std::collections::HashMap<(String, String), Option<String>>>,
}

impl<'a> Viewer<'a> {
    pub(crate) fn new(env: &ApiEnv, ctx: &'a ApiCtx) -> Self {
        let agents = env.agents();
        Self {
            ctx,
            snapshot: env.snapshot_of(&agents),
            agents,
            names: Default::default(),
        }
    }

    fn chat_name(&self, agent: &str, chat_id: &str) -> Option<String> {
        self.names
            .borrow_mut()
            .entry((agent.to_string(), chat_id.to_string()))
            .or_insert_with(|| {
                self.agents
                    .iter()
                    .find(|a| a.name == agent)
                    .and_then(|a| super::chats::chat_name_of(a, chat_id))
            })
            .clone()
    }

    pub(crate) fn view(&self, s: &Schedule) -> ScheduleView {
        let decision = summon_core::dashboard::authz::decide(
            &super::authz_principal(&self.ctx.principal),
            &Target::Agent(&s.agent),
            &self.snapshot,
        );
        ScheduleView {
            id: s.id.clone(),
            agent: s.agent.clone(),
            chat_id: s.chat_id.clone(),
            chat_name: self.chat_name(&s.agent, &s.chat_id),
            kind: kind_name(s.kind).to_string(),
            spec: s.spec.clone(),
            human: schedule::human_readable(s),
            next_fire: next_fires(s, self.ctx.now, 1).first().map(|t| rfc3339(*t)),
            last_fired: s.last_fired.clone(),
            prompt: s.prompt.clone(),
            output: output_name(s.output).to_string(),
            context: context_name(s.context).to_string(),
            enabled: s.enabled,
            created_by: s.created_by.clone(),
            editable: decision.is_allowed(),
        }
    }
}

pub(crate) fn view_of(env: &ApiEnv, ctx: &ApiCtx, s: &Schedule) -> ScheduleView {
    Viewer::new(env, ctx).view(s)
}

fn load(env: &ApiEnv) -> Result<Vec<Schedule>, ApiError> {
    env.schedule_store()
        .load()
        .map_err(|e| internal(summon_i18n::format!("r571b247187")))
}

/// 某个 agent 在某个群的定时任务(群详情页用)。
pub(crate) fn views_for(
    env: &ApiEnv,
    ctx: &ApiCtx,
    agent: &str,
    chat_id: &str,
) -> Vec<ScheduleView> {
    let mut all: Vec<Schedule> = load(env)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.agent == agent && s.chat_id == chat_id)
        .collect();
    all.sort_by_key(|s| !s.enabled);
    let viewer = Viewer::new(env, ctx);
    all.iter().map(|s| viewer.view(s)).collect()
}

pub(crate) fn list(ctx: &ApiCtx, _p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let mut all = load(env)?;
    // 启用中的在前;同组内按 agent、群、编号稳定排序。
    all.sort_by(|a, b| {
        (!a.enabled, &a.agent, &a.chat_id, &a.id).cmp(&(!b.enabled, &b.agent, &b.chat_id, &b.id))
    });
    let viewer = Viewer::new(env, ctx);
    let views: Vec<ScheduleView> = all.iter().map(|s| viewer.view(s)).collect();
    reply(&views)
}

/// 节律文本 → (kind, spec)。整段都必须是节律,多出来的 token 拒绝(与 `summon schedules edit`
/// 同一判据:多半是把提示词也写进来了)。
fn parse_when(when: &str, now: chrono::DateTime<chrono::Local>) -> Result<(Kind, String), String> {
    let tokens: Vec<&str> = when.split_whitespace().collect();
    let parsed = schedule::parse_when(&tokens, now)?;
    if parsed.consumed != tokens.len() {
        return Err(summon_i18n::format!(
            "r1fd5f95a04",
            &tokens[parsed.consumed..]
        ));
    }
    Ok((parsed.kind, parsed.spec))
}

#[derive(Deserialize)]
struct WhenParams {
    when: String,
}

pub(crate) fn check_when(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let WhenParams { when } = params(p)?;
    let out = match parse_when(&when, ctx.now) {
        Ok((kind, spec)) => {
            // 改完从此刻起算(存储层 edit 对周期节律把 last_fired 置为 now)。
            let probe = Schedule {
                id: String::new(),
                kind,
                spec,
                agent: String::new(),
                chat_id: String::new(),
                chat_type: String::new(),
                prompt: String::new(),
                created_by: String::new(),
                created_at: rfc3339(ctx.now),
                output: ScheduleOutput::default(),
                context: ScheduleContext::default(),
                enabled: true,
                last_fired: match kind {
                    Kind::Once => None,
                    Kind::Interval | Kind::Cron => Some(ctx.now.to_rfc3339()),
                },
                revision: 0,
            };
            WhenCheck {
                ok: true,
                human: Some(schedule::human_readable(&probe)),
                next: next_fires(&probe, ctx.now, 3)
                    .into_iter()
                    .map(rfc3339)
                    .collect(),
                error: None,
                forms: None,
            }
        }
        Err(error) => WhenCheck {
            ok: false,
            human: None,
            next: Vec::new(),
            error: Some(error),
            forms: Some(::summon_core::i18n::label(schedule::RHYTHM_FORMS).to_string()),
        },
    };
    reply(&out)
}

fn find(env: &ApiEnv, id: &str) -> Result<Schedule, ApiError> {
    load(env)?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, summon_i18n::format!("rc7b0c49dde")))
}

#[derive(Deserialize)]
struct EditParams {
    id: String,
    edit: ScheduleEdit,
}

pub(crate) fn edit(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: EditParams = params(p)?;
    let mut keys = Vec::new();
    for (key, set) in [
        ("when", req.edit.when.is_some()),
        ("prompt", req.edit.prompt.is_some()),
        ("output", req.edit.output.is_some()),
        ("context", req.edit.context.is_some()),
    ] {
        if set {
            keys.push(format!("{}.{key}", req.id));
        }
    }
    let result = (|| {
        let current = find(env, &req.id)?;
        env.require(ctx, &Target::Agent(&current.agent))?;
        let patch = SchedulePatch {
            rhythm: req
                .edit
                .when
                .as_deref()
                .map(|w| parse_when(w, ctx.now))
                .transpose()
                .map_err(|e| invalid(e).with_key("when"))?,
            prompt: req
                .edit
                .prompt
                .as_deref()
                .map(|p| {
                    let p = p.trim();
                    if p.is_empty() {
                        Err(invalid(summon_i18n::text!("re35619733c")).with_key("prompt"))
                    } else {
                        Ok(p.to_string())
                    }
                })
                .transpose()?,
            output: req
                .edit
                .output
                .as_deref()
                .map(|o| o.parse::<ScheduleOutput>())
                .transpose()
                .map_err(|e| invalid(e).with_key("output"))?,
            context: req
                .edit
                .context
                .as_deref()
                .map(|c| c.parse::<ScheduleContext>())
                .transpose()
                .map_err(|e| invalid(e).with_key("context"))?,
            enabled: None,
        };
        if patch.rhythm.is_none()
            && patch.prompt.is_none()
            && patch.output.is_none()
            && patch.context.is_none()
        {
            return Err(invalid(summon_i18n::text!("ra5bd26bb42")));
        }
        let (updated, _) = env
            .schedule_store()
            .edit(&req.id, patch, &ctx.now.to_rfc3339())
            .map_err(|e| internal(format!("{e:#}")))?;
        Ok(view_of(env, ctx, &updated))
    })();
    env.audit(ctx, "schedules", &keys, &outcome(&result));
    reply(&result?)
}

#[derive(Deserialize)]
struct EnableParams {
    id: String,
    enabled: bool,
}

pub(crate) fn set_enabled(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: EnableParams = params(p)?;
    let keys = vec![format!("{}.enabled", req.id)];
    let result = (|| {
        let current = find(env, &req.id)?;
        env.require(ctx, &Target::Agent(&current.agent))?;
        // 停用即归档,重新启用移回来:存储层在锁内按启停状态迁移文件。
        env.schedule_store()
            .set_enabled(&req.id, req.enabled)
            .map_err(|e| internal(format!("{e:#}")))?;
        Ok(view_of(env, ctx, &find(env, &req.id)?))
    })();
    env.audit(ctx, "schedules", &keys, &outcome(&result));
    reply(&result?)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::runtime::dashboard::sessions::ChatKind;
    use serde_json::json;

    fn schedule(id: &str, agent: &str, kind: Kind, spec: &str, enabled: bool) -> Schedule {
        Schedule {
            id: id.to_string(),
            kind,
            spec: spec.to_string(),
            agent: agent.to_string(),
            chat_id: GROUP.to_string(),
            chat_type: "group".to_string(),
            prompt: "看一眼 CI".to_string(),
            created_by: ME_OPEN.to_string(),
            created_at: "2026-10-01T09:00:00+08:00".to_string(),
            output: ScheduleOutput::Result,
            context: ScheduleContext::Resume,
            enabled,
            last_fired: None,
            revision: 0,
        }
    }

    fn kit() -> Kit {
        let kit = Kit::new();
        let a = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_agent(
            "arcadia",
            &agent_yaml("arcadia", "阿卡迪亚", "codex", "").replace(
                &format!("operators: [{ME_UNION}]"),
                &format!("operators: [{PEER_UNION}]"),
            ),
        );
        kit.add_chat(&a, "group", GROUP, "示例群");
        let store = kit.env().schedule_store();
        // 刚触发过:下次触发是下一个工作日 09:30,而不是「错过了、立刻补」。
        store
            .add(&Schedule {
                last_fired: Some(chrono::Local::now().to_rfc3339()),
                ..schedule("SCH-1", "starlight", Kind::Cron, "30 9 * * 1-5", true)
            })
            .unwrap();
        store
            .add(&schedule("SCH-2", "arcadia", Kind::Interval, "3600", true))
            .unwrap();
        store
            .add(&schedule(
                "SCH-3",
                "starlight",
                Kind::Cron,
                "0 18 * * *",
                true,
            ))
            .unwrap();
        store.set_enabled("SCH-3", false).unwrap();
        kit
    }

    #[test]
    fn schedules_list_includes_archived_with_next_fire() {
        let kit = kit();
        let list = kit.call(&kit.owner(), super::list, json!({})).unwrap();
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["SCH-2", "SCH-1", "SCH-3"],
            "启用中的在前(同组按 agent 名),归档的在后"
        );
        let first = &list[1];
        assert_eq!(first["human"], "每个工作日 09:30");
        assert!(first["next_fire"].as_str().unwrap().contains("T09:30:00"));
        assert_eq!(first["chat_name"], "示例群");
        assert_eq!(first["output"], "result");
        let archived = &list[2];
        assert_eq!(archived["enabled"], false);
        assert!(archived.get("next_fire").is_none(), "停用的没有下次触发");
    }

    #[test]
    fn set_enabled_moves_between_archive_files() {
        let kit = kit();
        let ctx = kit.owner();
        kit.call(&ctx, set_enabled, json!({"id": "SCH-1", "enabled": false}))
            .unwrap();
        assert!(kit.read("schedules-archive.yaml").contains("SCH-1"));
        assert!(!kit.read("schedules.yaml").contains("SCH-1"));
        let back = kit
            .call(&ctx, set_enabled, json!({"id": "SCH-1", "enabled": true}))
            .unwrap();
        assert_eq!(back["enabled"], true);
        assert!(kit.read("schedules.yaml").contains("SCH-1"));
    }

    #[test]
    fn edit_goes_through_the_store_and_checks_owner() {
        let kit = kit();
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        let edited = kit
            .call(
                &ctx,
                edit,
                json!({"id": "SCH-1", "edit": {"when": "daily 08:00", "prompt": " 新的提示 ", "context": "fresh"}}),
            )
            .unwrap();
        assert_eq!(edited["human"], "每天 08:00");
        assert_eq!(edited["prompt"], "新的提示");
        assert_eq!(edited["context"], "fresh");
        // 别的 agent 的定时任务:只认明确授权。
        let err = kit
            .call(&ctx, edit, json!({"id": "SCH-2", "edit": {"prompt": "x"}}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        let err = kit
            .call(
                &ctx,
                edit,
                json!({"id": "SCH-1", "edit": {"when": "every day"}}),
            )
            .unwrap_err();
        assert_eq!(
            (err.code, err.key.as_deref()),
            (ErrorCode::Invalid, Some("when"))
        );
        let err = kit
            .call(&ctx, set_enabled, json!({"id": "SCH-404", "enabled": true}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[test]
    fn check_when_previews_three_fires_or_lists_forms() {
        let kit = kit();
        let ctx = kit.owner();
        let ok = kit
            .call(&ctx, check_when, json!({"when": "weekday 09:30"}))
            .unwrap();
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["next"].as_array().unwrap().len(), 3);
        let bad = kit
            .call(&ctx, check_when, json!({"when": "sometime soon"}))
            .unwrap();
        assert_eq!(bad["ok"], false);
        assert!(bad["forms"].as_str().unwrap().contains("weekday 09:30"));
        let extra = kit
            .call(&ctx, check_when, json!({"when": "daily 09:00 提醒我"}))
            .unwrap();
        assert_eq!(extra["ok"], false);
    }
}
