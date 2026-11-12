use super::types::{AttentionItem, AttentionView};
use super::{params, reply};
use crate::runtime::core::AttentionSource;
use crate::runtime::dashboard::server::{ApiCtx, ApiError, ErrorCode};
use serde_json::Value;
use summon_core::dashboard::authz::{Decision, Target};

fn summary(text: &str) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let redacted = summon_core::log_view::redact(&clean);
    redacted
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

pub(crate) fn list(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let _: std::collections::HashMap<String, Value> = params(p)?;
    let env = ctx.env()?;
    let requests = env
        .port
        .attention_requests()
        .map_err(|_| ApiError::new(ErrorCode::Internal, "Pending requests are unavailable"))?;
    let mut partial = false;
    let mut items = Vec::new();
    for request in requests {
        let alias = &request.origin.alias;
        let agent = match env.agent(alias) {
            Ok(a) => a,
            Err(_) => {
                partial = true;
                continue;
            }
        };
        if request.origin.chat_id.is_empty() {
            partial = true;
            continue;
        }
        if !matches!(
            env.decide(
                ctx,
                &Target::Chat {
                    agent: alias,
                    chat_id: &request.origin.chat_id
                }
            ),
            Decision::Allow
        ) {
            continue;
        }
        let text = match request.source {
            AttentionSource::CodexApproval => String::new(),
            _ => summary(&request.text),
        };
        let link = summon_core::waiting::feishu_chat_link(
            &request.applink_host,
            &request.origin.chat_id,
            request.position.as_deref(),
        );
        items.push(AttentionItem {
            id: request.id,
            agent: alias.clone(),
            agent_display: agent.display.clone(),
            chat_name: super::chats::chat_name_of(&agent, &request.origin.chat_id)
                .or_else(|| env.port.member_name(&agent, &request.origin.chat_id))
                .unwrap_or(request.origin.chat_id),
            source: request.source.name().into(),
            summary: text,
            question_count: request.question_count,
            registered_at: request.registered_at.to_rfc3339(),
            waited_secs: request.waited_secs,
            link,
            link_target: if request.position.is_some() {
                "message"
            } else {
                "chat"
            }
            .into(),
        });
    }
    reply(&AttentionView {
        items,
        observed_at: ctx.now.to_rfc3339(),
        coverage: "structured_requests".into(),
        partial,
    })
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::runtime::core::AttentionRequest;
    use summon_core::core_port::GateOrigin;
    fn request(source: AttentionSource) -> AttentionRequest {
        AttentionRequest {
            id: "request-example".into(),
            origin: GateOrigin {
                alias: "starlight".into(),
                chat_id: GROUP.into(),
                chat_type: "group".into(),
                requester_open_id: ME_OPEN.into(),
            },
            source,
            text: "API_KEY=sk-0123456789abcdef <script> hello\nworld".into(),
            question_count: 1,
            waited_secs: 60,
            registered_at: "2026-11-09T00:00:00Z".parse().unwrap(),
            position: Some("77".into()),
            applink_host: "applink.feishu.cn".into(),
        }
    }
    #[test]
    fn attention_content_requires_current_chat_operate_permission() {
        let kit = Kit::new();
        kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(
            &kit.env().agent("starlight").unwrap(),
            "group",
            GROUP,
            "示例群",
        );
        *kit.port.attention.lock().unwrap() = Some(Ok(vec![request(AttentionSource::AgentAsk)]));
        let context = kit.feishu(crate::runtime::dashboard::sessions::tests::issuer());
        let allowed = list(&context, serde_json::json!({})).unwrap();
        assert_eq!(allowed["items"].as_array().unwrap().len(), 1);
        assert!(!allowed.to_string().contains("sk-0123456789abcdef"));
        assert_eq!(allowed["items"][0]["chat_name"], "示例群");
        let file = kit.agents_dir().join("starlight/agent.yaml");
        let yaml = std::fs::read_to_string(&file).unwrap().replace(
            &format!("operators: [{ME_UNION}]"),
            &format!("operators: [{PEER_UNION}]"),
        );
        std::fs::write(file, yaml).unwrap();
        let denied = list(&context, serde_json::json!({})).unwrap();
        assert_eq!(denied["items"], serde_json::json!([]));
        assert!(!denied.to_string().contains("request-example"));
    }
    #[test]
    fn attention_registration_time_survives_wall_clock_changes() {
        let kit = Kit::new();
        kit.add_agent("starlight", &agent_yaml("starlight", "星光", "codex", ""));
        let request = request(AttentionSource::AgentAsk);
        let registered_at = request.registered_at.to_rfc3339();
        *kit.port.attention.lock().unwrap() = Some(Ok(vec![request]));
        let mut context = kit.owner();
        let first = list(&context, serde_json::json!({})).unwrap();
        for delta in [-3600, 7200] {
            context.now += chrono::Duration::seconds(delta);
            let later = list(&context, serde_json::json!({})).unwrap();
            assert_eq!(later["items"][0]["registered_at"], registered_at);
            assert_eq!(later["items"][0]["waited_secs"], 60);
            assert_ne!(later["observed_at"], first["observed_at"]);
        }
    }

    #[test]
    fn attention_read_failure_and_approval_contents_are_separate_from_empty() {
        let kit = Kit::new();
        kit.add_agent("starlight", &agent_yaml("starlight", "星光", "codex", ""));
        *kit.port.attention.lock().unwrap() = Some(Err("private error".into()));
        assert!(list(&kit.owner(), serde_json::json!({})).is_err());
        *kit.port.attention.lock().unwrap() =
            Some(Ok(vec![request(AttentionSource::CodexApproval)]));
        let result = list(&kit.owner(), serde_json::json!({})).unwrap();
        assert_eq!(result["items"][0]["summary"], "");
        assert_eq!(result["items"][0]["source"], "codex_approval");
        assert!(kit.port.remote_asked.lock().unwrap().is_empty());
        assert!(kit.port.asks.lock().unwrap().is_empty());
    }
}
