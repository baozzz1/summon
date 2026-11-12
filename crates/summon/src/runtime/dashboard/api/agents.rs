//! `agents.*`:列表、详情、启用 / 禁用。启停走 availability(`summon agents enable/disable` 的
//! 同一条路径,即时生效);飞书身份只读,更换请在本机 `summon bind`。

use serde::Deserialize;
use serde_json::Value;
use summon_core::dashboard::authz::{Decision, Target};
use summon_core::registry::Agent;

use super::types::{AgentDetail, AgentSummary, Credentials, Member};
use super::{internal, outcome, params, reply, ApiEnv, Loaded};
use crate::runtime::dashboard::server::{ApiCtx, ApiError};

/// 这个 agent 待过的群数(盘上记过 `.meta` 的不同 chat)。
fn chat_count(agent: &Agent) -> usize {
    let dir = agent.state_dir().join("runtime").join("chats");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "meta"))
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            summon_core::chat_meta::ChatMeta::parse(&text).chat_id
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids.len()
}

pub(crate) fn summary(env: &ApiEnv, agent: &Agent) -> AgentSummary {
    let (id, secret) = crate::runtime::daemon::resolve_creds(agent);
    AgentSummary {
        alias: agent.name.clone(),
        name: agent.name.clone(),
        display: agent.display.clone(),
        cli: agent.cli_kind().to_string(),
        model: agent.model.clone().filter(|m| !m.is_empty()),
        effort: agent.effort.clone().filter(|e| !e.is_empty()),
        enabled: env.enabled(&agent.name),
        routable: agent.is_routable(),
        load_error: None,
        credentials: if id.is_some() && secret.is_some() {
            Credentials::Ready
        } else {
            Credentials::Missing
        },
        chats: chat_count(agent),
    }
}

fn broken_summary(env: &ApiEnv, alias: &str, error: &str) -> AgentSummary {
    AgentSummary {
        alias: alias.to_string(),
        name: alias.to_string(),
        display: String::new(),
        cli: String::new(),
        model: None,
        effort: None,
        enabled: env.enabled(alias),
        routable: false,
        load_error: Some(summon_i18n::format!("rd7efc46599")),
        credentials: Credentials::Missing,
        chats: 0,
    }
}

pub(crate) fn summaries(env: &ApiEnv) -> Vec<AgentSummary> {
    env.scan()
        .into_iter()
        .map(|loaded| match loaded {
            Loaded::Ok(agent) => summary(env, &agent),
            Loaded::Broken { alias, error } => broken_summary(env, &alias, &error),
        })
        .collect()
}

/// 名单里的 ID 与能查到的飞书名字(agent.yaml 的称呼优先,其次是 daemon 的身份缓存)。
pub(crate) fn members(env: &ApiEnv, agent: &Agent, ids: Vec<String>) -> Vec<Member> {
    ids.into_iter()
        .map(|id| Member {
            name: agent
                .aliases
                .get(&id)
                .cloned()
                .or_else(|| env.port.member_name(agent, &id)),
            id,
        })
        .collect()
}

pub(crate) fn list(ctx: &ApiCtx, _p: Value) -> Result<Value, ApiError> {
    reply(&summaries(ctx.env()?))
}

#[derive(Deserialize)]
struct AgentParams {
    agent: String,
}

pub(crate) fn get(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let AgentParams { agent: alias } = params(p)?;
    let decision = env.decide(ctx, &Target::Agent(&alias));
    let deny_reason = match &decision {
        Decision::Allow => None,
        Decision::Deny(why) => Some(why.clone()),
    };
    // 读不出的那份也给详情:概要里带错误原文,页面据此链到原文模式。
    let broken = env.scan().into_iter().find_map(|l| match l {
        Loaded::Broken { alias: a, error } if a == alias => Some(error),
        _ => None,
    });
    if let Some(error) = broken {
        return reply(&AgentDetail {
            summary: broken_summary(env, &alias, &error),
            app_id_env: String::new(),
            scope: String::new(),
            model_candidates: Vec::new(),
            effort_options: Vec::new(),
            members: Vec::new(),
            can_toggle_enabled: decision.is_allowed(),
            deny_reason,
        });
    }
    let agent = env.agent(&alias)?;
    let cfg = env.read_config()?;
    let messaging = super::config::agent_messaging(&cfg, &agent);
    let options = summon_core::model_effort::options_for(&messaging, agent.cli_kind());
    let feishu = agent.feishu.as_ref();
    let mut ids: Vec<String> = Vec::new();
    if let Some(f) = feishu {
        let mut chat_lists: Vec<(&String, &Vec<String>)> = f.chat_operators.iter().collect();
        chat_lists.sort();
        for id in f
            .allowed_users
            .iter()
            .chain(agent.effective_operators().unwrap_or_default().iter())
            .chain(chat_lists.into_iter().flat_map(|(_, v)| v))
        {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
    }
    reply(&AgentDetail {
        summary: summary(env, &agent),
        app_id_env: feishu
            .and_then(|f| f.app_id_env.clone())
            .unwrap_or_default(),
        scope: feishu.map(|f| f.scope.clone()).unwrap_or_default(),
        model_candidates: env.port.model_candidates(&agent, options.models),
        effort_options: options.efforts,
        members: members(env, &agent, ids),
        can_toggle_enabled: decision.is_allowed(),
        deny_reason,
    })
}

#[derive(Deserialize)]
struct EnableParams {
    agent: String,
    enabled: bool,
}

pub(crate) fn set_enabled(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: EnableParams = params(p)?;
    let keys = vec!["enabled".to_string()];
    let result = (|| {
        let agent = env.agent(&req.agent)?;
        env.require(ctx, &Target::Agent(&agent.name))?;
        let _manifest = crate::setup::bind::manifest_write_lock(&agent.manifest_path())
            .map_err(|e| internal(format!("{e:#}")))?;
        // 与 `summon agents enable/disable` 同一个存储:禁用后当前轮允许完成,后续任务拒绝并通知。
        let gate = crate::runtime::agent_availability::AgentAvailability::lock(
            &env.run_dir().join("inbox"),
            &agent.name,
        )
        .map_err(|e| internal(format!("{e:#}")))?;
        gate.set_enabled(req.enabled)
            .map_err(|e| internal(format!("{e:#}")))?;
        drop(gate);
        Ok(summary(env, &agent))
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
    use crate::runtime::dashboard::server::ErrorCode;
    use crate::runtime::dashboard::sessions::ChatKind;
    use serde_json::json;

    #[test]
    fn get_lists_candidates_members_and_identity() {
        let kit = Kit::new();
        let a = kit.add_agent(
            "starlight",
            &agent_yaml(
                "starlight",
                "星光",
                "claude",
                &format!("aliases:\n  {ME_UNION}: 我\n"),
            )
            .replace(
                &format!("operators: [{ME_UNION}]"),
                &format!("operators: [{ME_UNION}, {PEER_UNION}]"),
            ),
        );
        kit.add_chat(&a, "group", GROUP, "示例群");
        let detail = kit
            .call(&kit.owner(), get, json!({"agent": "starlight"}))
            .unwrap();
        assert_eq!(detail["summary"]["credentials"], "ready");
        assert_eq!(detail["summary"]["chats"], 1);
        assert_eq!(detail["app_id_env"], "DASH_TEST_STARLIGHT_APP_ID");
        assert_eq!(detail["scope"], "thread");
        assert_eq!(
            detail["members"],
            json!([{"id": ME_UNION, "name": "我"}, {"id": PEER_UNION, "name": "另一位用户"}])
        );
        assert!(!detail["effort_options"].as_array().unwrap().is_empty());
        assert!(
            !detail.to_string().contains("cli_0000000000000001"),
            "凭据值不下发"
        );
        let err = kit
            .call(&kit.owner(), get, json!({"agent": "nobody"}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[test]
    fn set_enabled_uses_availability_and_checks_permission() {
        let kit = Kit::new();
        kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_agent(
            "arcadia",
            &agent_yaml("arcadia", "阿卡迪亚", "codex", "").replace(
                &format!("operators: [{ME_UNION}]"),
                &format!("operators: [{PEER_UNION}]"),
            ),
        );
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        let off = kit
            .call(
                &ctx,
                set_enabled,
                json!({"agent": "starlight", "enabled": false}),
            )
            .unwrap();
        assert_eq!(off["enabled"], false);
        assert!(!kit.env().enabled("starlight"));
        assert_eq!(kit.read("run/inbox/availability/starlight.json"), "false");
        let err = kit
            .call(
                &ctx,
                set_enabled,
                json!({"agent": "arcadia", "enabled": false}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(kit.env().enabled("arcadia"));
    }
}
