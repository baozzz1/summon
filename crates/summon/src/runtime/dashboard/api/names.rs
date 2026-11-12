//! 页面上的 ID 反查名字:群 ID → 群名,open_id / union_id → 人名。先查本机已知的(会话记录里的群名、
//! agent.yaml 的称呼、飞书身份与名字缓存),都没有再经 agent 自己的应用问一次飞书。查不到的不返回,
//! 页面照旧显示 ID。
//!
//! 联网这一步用的是 agent 应用的身份、ID 会拼进飞书接口的路径,所以几道闸:ID 形态不对的不查;
//! 只替能操作这个 agent 的人问;一次请求至多真正发 [`MAX_REMOTE`] 个请求(问过的走缓存,不计数)、
//! 联网部分至多 [`REMOTE_DEADLINE`];每个 agent 的应用在一段时间里被问的总次数另有上限(见 daemon 端口)。

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use summon_core::dashboard::authz::{Decision, Target};
use summon_core::registry::Agent;

use super::{invalid, params, reply, ApiEnv, Loaded};
use crate::runtime::dashboard::server::{ApiCtx, ApiError};

/// 一次最多反查多少个 ID(一页上的名单与群远少于这个数)。
const MAX_IDS: usize = 200;
/// 一次请求里最多真正问飞书几次;其余这次只查本机记录。
pub(crate) const MAX_REMOTE: usize = 50;
/// 一次请求里联网部分最多花这么久,过了剩下的只查本机记录。
const REMOTE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// 飞书 ID 的形态:`oc_` / `ou_` / `on_` 加 1–64 个字母数字。
fn is_feishu_id(id: &str) -> bool {
    ["oc_", "ou_", "on_"].iter().any(|prefix| {
        id.strip_prefix(prefix).is_some_and(|rest| {
            (1..=64).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_alphanumeric())
        })
    })
}

#[derive(Deserialize)]
struct NamesParams {
    /// ID 出自哪个 agent 的名单:open_id 按应用区分,先用这个 agent 的应用查。
    #[serde(default)]
    agent: Option<String>,
    ids: Vec<String>,
}

pub(crate) fn resolve(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let NamesParams { agent, ids } = params(p)?;
    if ids.len() > MAX_IDS {
        return Err(invalid(summon_i18n::format!("r7f6eaf5a26")));
    }
    let mut agents: Vec<Agent> = env
        .scan()
        .into_iter()
        .filter_map(|loaded| match loaded {
            Loaded::Ok(agent) => Some(*agent),
            Loaded::Broken { .. } => None,
        })
        .collect();
    if let Some(alias) = &agent {
        agents.sort_by_key(|a| a.name != *alias);
    }
    // 联网只用排在最前的那个 agent:每个 ID 至多问一次飞书。
    let remote = agents
        .first()
        .filter(|a| matches!(env.decide(ctx, &Target::Agent(&a.name)), Decision::Allow));
    let mut budget = MAX_REMOTE;
    let deadline = std::time::Instant::now() + REMOTE_DEADLINE;
    let mut names = BTreeMap::new();
    for id in ids.into_iter().filter(|id| is_feishu_id(id)) {
        let name = local_name(env, &agents, &id).or_else(|| {
            let agent = remote.filter(|_| std::time::Instant::now() < deadline)?;
            env.port.remote_name(agent, &id, &mut budget)
        });
        if let Some(name) = name {
            names.insert(id, name);
        }
    }
    reply(&names)
}

fn local_name(env: &ApiEnv, agents: &[Agent], id: &str) -> Option<String> {
    if id.starts_with("oc_") {
        agents
            .iter()
            .find_map(|agent| super::chats::chat_name_of(agent, id))
    } else {
        agents
            .iter()
            .find_map(|agent| agent.aliases.get(id).cloned())
            .or_else(|| {
                agents
                    .iter()
                    .find_map(|agent| env.port.member_name(agent, id))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::dashboard::api::testkit::{
        agent_yaml, issuer, Kit, PEER_UNION, REMOTE_KNOWN,
    };
    use crate::runtime::dashboard::sessions::{ChatKind, Issuer};
    use serde_json::json;

    const OTHER: &str = "ou_00000000000000000000000000000002";
    const GROUP: &str = "oc_00000000000000000000000000000001";

    /// 群名来自会话记录,人名来自 agent.yaml 的称呼;认不出的、不是 ID 的都不返回。
    #[test]
    fn names_come_from_local_records_and_unknown_ids_are_left_out() {
        let kit = Kit::new();
        let agent = kit.add_agent(
            "starlight",
            &agent_yaml(
                "starlight",
                "星光",
                "claude",
                &format!("aliases:\n  {OTHER}: 另一位用户\n"),
            ),
        );
        kit.add_chat(&agent, "group", GROUP, "示例群");
        let out = kit
            .call(
                &kit.owner(),
                resolve,
                json!({"agent": "starlight", "ids": [
                    OTHER, GROUP, "ou_00000000000000000000000000000003", "not-an-id",
                ]}),
            )
            .unwrap();
        assert_eq!(out, json!({ OTHER: "另一位用户", GROUP: "示例群" }));
    }

    fn kit_with_starlight() -> Kit {
        let kit = Kit::new();
        kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit
    }

    /// 形态不对的 ID 会被拼进飞书接口的路径,一个都不能走到联网那一步。
    #[test]
    fn malformed_ids_never_reach_feishu() {
        let kit = kit_with_starlight();
        let long = format!("ou_{}", "a".repeat(65));
        let out = kit
            .call(
                &kit.owner(),
                resolve,
                json!({"agent": "starlight", "ids": [
                    "ou_/../../open-apis/x", "ou_a?b=1", "oc_", "on_a b", long, "OU_abc", REMOTE_KNOWN,
                ]}),
            )
            .unwrap();
        assert_eq!(out, json!({ REMOTE_KNOWN: "远端用户" }));
        assert_eq!(*kit.port.remote_asked.lock().unwrap(), [REMOTE_KNOWN]);
    }

    /// 联网用的是 agent 应用的身份:操作不了这个 agent 的人只拿到本机记录里的名字。
    #[test]
    fn only_operators_of_the_agent_get_remote_lookups() {
        let kit = kit_with_starlight();
        let stranger = kit.feishu(Issuer {
            open_id: "ou_00000000000000000000000000000004".into(),
            union_id: PEER_UNION.into(),
            ..issuer("starlight", GROUP, ChatKind::Group)
        });
        let ask = json!({"agent": "starlight", "ids": [REMOTE_KNOWN]});
        assert_eq!(
            kit.call(&stranger, resolve, ask.clone()).unwrap(),
            json!({})
        );
        assert!(kit.port.remote_asked.lock().unwrap().is_empty());
        let me = kit.feishu(issuer("starlight", GROUP, ChatKind::Group));
        assert_eq!(
            kit.call(&me, resolve, ask).unwrap(),
            json!({ REMOTE_KNOWN: "远端用户" })
        );
    }

    #[test]
    fn one_request_asks_feishu_at_most_max_remote_times() {
        let kit = kit_with_starlight();
        let ids: Vec<String> = (0..MAX_IDS).map(|i| format!("ou_{i:032}")).collect();
        kit.call(
            &kit.owner(),
            resolve,
            json!({"agent": "starlight", "ids": ids}),
        )
        .unwrap();
        assert_eq!(kit.port.remote_asked.lock().unwrap().len(), MAX_REMOTE);
    }

    #[test]
    fn too_many_ids_are_refused() {
        let kit = Kit::new();
        let ids: Vec<String> = (0..=MAX_IDS).map(|i| format!("ou_{i:032}")).collect();
        let err = kit
            .call(&kit.owner(), resolve, json!({ "ids": ids }))
            .unwrap_err();
        assert!(err.message.contains("单次最多"), "{}", err.message);
    }
}
