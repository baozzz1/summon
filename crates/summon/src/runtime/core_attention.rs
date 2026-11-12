use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttentionSource {
    AgentAsk,
    ClaudeQuestion,
    ClaudePermission,
    CodexQuestion,
    CodexApproval,
}
impl AttentionSource {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::AgentAsk => "agent_ask",
            Self::ClaudeQuestion => "claude_question",
            Self::ClaudePermission => "claude_permission",
            Self::CodexQuestion => "codex_question",
            Self::CodexApproval => "codex_approval",
        }
    }
}
pub(super) struct AttentionMeta {
    since: Instant,
    registered_at: chrono::DateTime<chrono::Utc>,
    source: AttentionSource,
    position_requested: bool,
    applink_host: String,
    position: Option<String>,
}
impl Default for AttentionMeta {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            registered_at: chrono::Utc::now(),
            source: AttentionSource::AgentAsk,
            position_requested: false,
            applink_host: "applink.feishu.cn".into(),
            position: None,
        }
    }
}
impl AttentionMeta {
    pub(super) fn for_hook(hook: Option<&summon_core::core_port::ClaudeHookRequest>) -> Self {
        Self {
            source: match hook {
                Some(h) if h.tool_name == "AskUserQuestion" => AttentionSource::ClaudeQuestion,
                Some(_) => AttentionSource::ClaudePermission,
                None => AttentionSource::AgentAsk,
            },
            ..Self::default()
        }
    }
}

/// Minimal in-memory projection; card contents and callback values stay in Core.
#[derive(Clone, Debug)]
pub(crate) struct AttentionRequest {
    pub(crate) id: String,
    pub(crate) origin: GateOrigin,
    pub(crate) source: AttentionSource,
    pub(crate) text: String,
    pub(crate) question_count: usize,
    pub(crate) waited_secs: u64,
    pub(crate) registered_at: chrono::DateTime<chrono::Utc>,
    pub(crate) position: Option<String>,
    pub(crate) applink_host: String,
}

impl Core {
    pub(super) fn set_attention_source(&self, id: &GateId, source: AttentionSource) {
        if let Ok(mut gates) = self.gates.lock() {
            if let Some(pending) = gates.get_mut(id) {
                pending.attention.source = source;
            }
        }
    }
    pub(crate) fn ask_codex_question(
        &self,
        turn: &str,
        alias: &str,
        tag: &str,
        questions: &[summon_core::core_port::AskQuestion],
    ) -> CommandOutcome {
        self.do_chat_ask_with_source(turn, alias, tag, questions, AttentionSource::CodexQuestion)
    }
    pub(crate) fn ask_codex_approval(
        &self,
        turn: &str,
        alias: &str,
        tag: &str,
        questions: &[summon_core::core_port::AskQuestion],
    ) -> CommandOutcome {
        self.do_chat_ask_with_source(turn, alias, tag, questions, AttentionSource::CodexApproval)
    }
    pub(crate) fn attention_requests(&self) -> Result<Vec<AttentionRequest>, String> {
        // Resolution removes the sender before it stashes an early answer.
        let senders = self
            .gate_senders
            .lock()
            .map_err(|_| "attention unavailable")?;
        let gates = self.gates.lock().map_err(|_| "attention unavailable")?;
        let mut result = Vec::new();
        let mut pending: Vec<_> = gates.iter().collect();
        pending.sort_by(|(a, pa), (b, pb)| {
            pa.attention
                .since
                .cmp(&pb.attention.since)
                .then(a.as_str().cmp(b.as_str()))
        });
        for (id, p) in pending {
            if !senders.contains_key(id)
                || matches!(p.reception, GateReception::Stashed(_))
                || p.message_id.is_none()
            {
                continue;
            }
            if p.view.kind != "ask" && p.view.kind != PERMISSION_GATE_KIND {
                continue;
            }
            let text = match p.attention.source {
                AttentionSource::CodexApproval => String::new(),
                _ => p.view.text.clone(),
            };
            result.push(AttentionRequest {
                id: id.as_str().into(),
                origin: p.view.origin.clone(),
                source: p.attention.source,
                text,
                question_count: p.questions.len(),
                waited_secs: p.attention.since.elapsed().as_secs(),
                registered_at: p.attention.registered_at,
                position: p.attention.position.clone(),
                applink_host: p.attention.applink_host.clone(),
            });
        }
        Ok(result)
    }
    fn claim_attention_position(&self, id: &GateId, message: &str) -> bool {
        let Ok(senders) = self.gate_senders.lock() else {
            return false;
        };
        if !senders.contains_key(id) {
            return false;
        }
        let Ok(mut gates) = self.gates.lock() else {
            return false;
        };
        let Some(p) = gates.get_mut(id).filter(|p| {
            p.message_id.as_deref() == Some(message) && !p.attention.position_requested
        }) else {
            return false;
        };
        p.attention.position_requested = true;
        true
    }
    fn finish_attention_position(&self, id: &GateId, message: &str, position: Option<String>) {
        let Ok(senders) = self.gate_senders.lock() else {
            return;
        };
        if !senders.contains_key(id) {
            return;
        }
        let Ok(mut gates) = self.gates.lock() else {
            return;
        };
        if let Some(p) = gates
            .get_mut(id)
            .filter(|p| p.message_id.as_deref() == Some(message))
        {
            p.attention.position =
                position.filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()));
        }
    }
    pub(crate) fn cache_attention_position(
        self: &Arc<Self>,
        fc: &Arc<crate::frontend::client::FeishuClient>,
        id: &GateId,
        message: &str,
    ) {
        if !self.claim_attention_position(id, message) {
            return;
        }
        if let Ok(mut gates) = self.gates.lock() {
            if let Some(p) = gates.get_mut(id) {
                p.attention.applink_host = fc.applink_host().into();
            }
        }
        let core = Arc::clone(self);
        let fc = Arc::clone(fc);
        let id = id.clone();
        let message = message.to_owned();
        // Network work never holds Core locks or delays the pending card's waiter.
        let _ = std::thread::Builder::new()
            .name("attention-position".into())
            .spawn(move || {
                let position = fc.fetch_message_position(&message).ok().flatten();
                core.finish_attention_position(&id, &message, position);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending(core: &Core) -> GateId {
        let id = core.register_gate(GateView {
            origin: GateOrigin {
                alias: "starlight".into(),
                chat_id: "oc_room".into(),
                chat_type: "group".into(),
                requester_open_id: String::new(),
            },
            kind: "ask".into(),
            text: "Choose A or B".into(),
        });
        assert!(core.bind_gate_message(&id, "om_card"));
        id
    }
    #[test]
    fn attention_filters_unbound_and_stashed_without_losing_early_answer() {
        let core = Core::new(HashMap::new(), HashMap::new(), Vec::new());
        let id = pending(&core);
        assert_eq!(core.attention_requests().unwrap().len(), 1);
        let sender = core.gate_senders.lock().unwrap().remove(&id).unwrap();
        assert!(core.attention_requests().unwrap().is_empty());
        core.gate_senders.lock().unwrap().insert(id.clone(), sender);
        core.deliver_resolution(&id, Resolution::Cancel);
        assert!(core.attention_requests().unwrap().is_empty());
        assert!(matches!(core.wait_gate(&id), CommandOutcome::Denied(_)));
        assert!(core.list_gates().is_empty());
    }
    #[test]
    fn attention_unbound_and_same_chat_questions_share_one_slot_while_permissions_coexist() {
        let core = Core::new(HashMap::new(), HashMap::new(), Vec::new());
        let question = summon_core::core_port::AskQuestion {
            header: None,
            question: "Choose A".into(),
            options: vec!["A".into()],
            descriptions: Vec::new(),
            allow_other: false,
            multi_select: false,
        };
        let view = |chat: &str, kind: &str| GateView {
            origin: GateOrigin {
                alias: "starlight".into(),
                chat_id: chat.into(),
                chat_type: "group".into(),
                requester_open_id: String::new(),
            },
            kind: kind.into(),
            text: "Bash".into(),
        };
        let a = core
            .register_chat_ask_gate(view("oc_a", "ask"), vec![question.clone()], None)
            .unwrap();
        assert!(core.attention_requests().unwrap().is_empty());
        assert!(core
            .register_chat_ask_gate(view("oc_a", "ask"), vec![question.clone()], None)
            .is_none());
        let b = core
            .register_chat_ask_gate(view("oc_b", "ask"), vec![question], None)
            .unwrap();
        core.set_attention_source(&b, AttentionSource::CodexApproval);
        let hook = summon_core::core_port::ClaudeHookRequest {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command":"private command"}),
            agent_id: None,
            agent_type: None,
        };
        let c = core
            .register_permission_gate(view("oc_c", "permission"), hook.clone())
            .unwrap();
        let d = core
            .register_permission_gate(view("oc_c", "permission"), hook)
            .unwrap();
        for id in [&a, &b, &c, &d] {
            assert!(core.bind_gate_message(id, "om_bound"));
        }
        assert_eq!(core.attention_requests().unwrap().len(), 4);
        assert!(!format!("{:?}", core.attention_requests().unwrap()).contains("private command"));
        for id in [&a, &b, &c, &d] {
            core.drop_gate(id);
        }
        assert!(core.attention_requests().unwrap().is_empty());
    }

    #[test]
    fn attention_orders_same_second_requests_by_registration_time() {
        let core = Core::new(HashMap::new(), HashMap::new(), Vec::new());
        let a = pending(&core);
        let b = pending(&core);
        let now = Instant::now();
        {
            let mut gates = core.gates.lock().unwrap();
            gates.get_mut(&a).unwrap().attention.since = now - Duration::from_millis(100);
            gates.get_mut(&b).unwrap().attention.since = now - Duration::from_millis(50);
        }
        assert_eq!(
            core.attention_requests()
                .unwrap()
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>(),
            vec![a.as_str(), b.as_str()]
        );
        core.drop_gate(&a);
        core.drop_gate(&b);
    }

    #[test]
    fn attention_registration_time_is_fixed_while_waiting_uses_monotonic_time() {
        let core = Core::new(HashMap::new(), HashMap::new(), Vec::new());
        let before = chrono::Utc::now();
        let id = pending(&core);
        let first = core.attention_requests().unwrap().remove(0);
        assert!(first.registered_at >= before && first.registered_at <= chrono::Utc::now());
        core.gates
            .lock()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .attention
            .since = Instant::now() - Duration::from_secs(60);
        let later = core.attention_requests().unwrap().remove(0);
        assert_eq!(first.registered_at, later.registered_at);
        assert!(later.waited_secs >= 60);
        core.drop_gate(&id);
    }

    #[test]
    fn attention_position_is_claimed_once_and_cannot_revive_finished_gate() {
        let core = Core::new(HashMap::new(), HashMap::new(), Vec::new());
        let id = pending(&core);
        assert!(!core.claim_attention_position(&id, "om_wrong"));
        assert!(core.claim_attention_position(&id, "om_card"));
        assert!(!core.claim_attention_position(&id, "om_card"));
        core.finish_attention_position(&id, "om_wrong", Some("77".into()));
        assert!(core.attention_requests().unwrap()[0].position.is_none());
        core.finish_attention_position(&id, "om_card", Some("88".into()));
        assert_eq!(
            core.attention_requests().unwrap()[0].position.as_deref(),
            Some("88")
        );
        core.drop_gate(&id);
        core.finish_attention_position(&id, "om_card", Some("99".into()));
        assert!(core.attention_requests().unwrap().is_empty());
    }
    #[test]
    fn attention_poison_is_error_not_an_empty_list() {
        let core = Core::new(HashMap::new(), HashMap::new(), Vec::new());
        let _ = std::panic::catch_unwind(|| {
            let _guard = core.gates.lock().unwrap();
            panic!("poison");
        });
        assert!(core.attention_requests().is_err());
    }
}
