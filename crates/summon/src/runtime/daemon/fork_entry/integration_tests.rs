use super::*;
use crate::session::resume_store;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

struct Fixture {
    agent: Agent,
    _dir: tempfile::TempDir,
    client: FeishuClient,
    chats: HashMap<String, ChatHandle>,
    scope: String,
    source_session: String,
    calls: Arc<Mutex<Vec<(String, String, Value)>>>,
    fail_messages: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let (agent, dir) = crate::test_support::agent_dir("codex", "feishu:\n  app_id_env: FORK_APP\n  app_secret_env: FORK_SECRET\n  allowed_users: [ou_owner, ou_operator]\n  operators: [ou_owner, ou_operator]\n");
        std::fs::write(
            agent.dir.join(".env"),
            "FORK_APP=test-app\nFORK_SECRET=test-secret\n",
        )
        .unwrap();
        let session = fork::target_session_name(&agent, "group", "oc_source").unwrap();
        super::super::chat_meta::write_chat_meta_checked(
            &agent,
            &session,
            "oc_source",
            "group",
            Some("Source"),
            "owner",
            None,
        )
        .unwrap();
        resume_store::write_checked(&agent, &session, "codex", "source-sid", Some("/repo"), None)
            .unwrap();
        let scope = super::super::agent_env::feishu_delivery_scope(&agent);
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let client = FeishuClient::with_test_base_connection_close(format!(
            "http://{}",
            server.server_addr().to_ip().unwrap()
        ));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let fail_messages = Arc::new(AtomicBool::new(false));
        let (seen, stopped, fail) = (calls.clone(), stop.clone(), fail_messages.clone());
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let Some(mut request) = server
                    .recv_timeout(std::time::Duration::from_millis(20))
                    .unwrap()
                else {
                    continue;
                };
                let method = request.method().to_string();
                let url = request.url().to_string();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
                seen.lock()
                    .unwrap()
                    .push((method.clone(), url.clone(), body));
                let response = if url.contains("auth/v3/tenant_access_token") {
                    json!({"tenant_access_token":"token", "expire":7200})
                } else if url.contains("/messages") {
                    if fail.load(Ordering::SeqCst) && method == "PATCH" {
                        json!({"code":230002,"msg":"simulated update failure"})
                    } else {
                        json!({"code":0,"data":{"message_id":"om_card"}})
                    }
                } else if method == "POST" && url.starts_with("/open-apis/im/v1/chats?") {
                    json!({"code":0,"data":{"chat_id":"oc_new"}})
                } else if url.contains("/members?") {
                    json!({"code":0,"data":{"items":[{"member_id":"ou_owner"},{"member_id":"ou_operator"}],"has_more":false}})
                } else if url.starts_with("/open-apis/im/v1/chats?") {
                    json!({"code":0,"data":{"items":[{"chat_id":"oc_source","name":"Target"},{"chat_id":"oc_new","name":"Target"},{"chat_id":"oc_other","name":"Other"}],"has_more":false}})
                } else if url.contains("/chats/oc_source") {
                    json!({"code":0,"data":{"name":"Source","chat_mode":"group"}})
                } else if url.contains("/chats/oc_other") {
                    json!({"code":0,"data":{"name":"Other","chat_mode":"group"}})
                } else if url.contains("/chats/oc_new") {
                    json!({"code":0,"data":{"name":"Target","chat_mode":"group"}})
                } else {
                    panic!("unexpected HTTP request: {method} {url}");
                };
                let status = if fail.load(Ordering::SeqCst) && method == "PATCH" {
                    400
                } else {
                    200
                };
                request
                    .respond(
                        tiny_http::Response::from_string(response.to_string())
                            .with_status_code(status),
                    )
                    .unwrap();
            }
        });
        Self {
            agent,
            _dir: dir,
            client,
            chats: HashMap::new(),
            scope,
            source_session: session,
            calls,
            fail_messages,
            stop,
            server: Some(thread),
        }
    }

    fn env(&self) -> Environment<'_> {
        Environment {
            agent: &self.agent,
            fc: &self.client,
            chats: &self.chats,
            scope: &self.scope,
        }
    }
    fn source(&self, requester: &'static str) -> Source<'_> {
        Source {
            chat_id: "oc_source",
            chat_type: "group",
            message_id: "om_source",
            requester,
            union_id: "on_owner",
        }
    }
    fn ticket(&self) -> String {
        summon_core::registry::chat_tag("group", "om_source")
    }
    fn read(&self) -> Request {
        with_record_lock(&self.agent, "fork-requests", &self.ticket(), |path| {
            Ok(serde_json::from_slice(&std::fs::read(path)?)?)
        })
        .unwrap()
    }
    fn card(&self, action: &str) -> CardActionMsg {
        let request = self.read();
        CardActionMsg {
            chat_id: request.source_chat,
            message_id: request.card_id.unwrap(),
            operator_open_id: request.requester,
            event_id: "event".into(),
            value: json!({"action":action,"ticket":self.ticket()}),
            wal_id: None,
            wal_replayed: false,
        }
    }
    fn creations(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, url, _)| {
                method == "POST" && url.starts_with("/open-apis/im/v1/chats?")
            })
            .count()
    }
    fn target(&self) -> String {
        fork::target_session_name(&self.agent, "group", "oc_new").unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.server.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn source_command_creates_owner_bot_group_and_retries_without_a_second_fork() {
    let f = Fixture::new();
    let source_before =
        std::fs::read(resume_store::record_path(&f.agent, &f.source_session)).unwrap();
    start(&f.env(), f.source("ou_owner"), "new Target").unwrap();
    let record = resume_store::read_record(&f.agent, &f.target()).unwrap();
    assert!(record.fork_pending);
    assert_ne!(record.id, "source-sid");
    assert_eq!(record.forked_from.unwrap().sid, "source-sid");
    assert_eq!(record.cwd.as_deref(), Some("/repo"));
    callback(&f.env(), &f.card("fork_execute")).unwrap();
    start(&f.env(), f.source("ou_owner"), "new Target").unwrap();
    assert_eq!(f.creations(), 1);
    assert_eq!(
        resume_store::read_record(&f.agent, &f.target()).unwrap().id,
        record.id
    );
    assert_eq!(
        std::fs::read(resume_store::record_path(&f.agent, &f.source_session)).unwrap(),
        source_before
    );
    let calls = f.calls.lock().unwrap();
    let create = &calls
        .iter()
        .find(|(m, u, _)| m == "POST" && u.starts_with("/open-apis/im/v1/chats?"))
        .unwrap()
        .2;
    assert_eq!(create["owner_id"], "ou_owner");
    assert_eq!(create["user_id_list"], json!(["ou_owner"]));
    assert_eq!(create["bot_id_list"], json!(["test-app"]));
    assert!(
        calls
            .iter()
            .filter(|(_, u, _)| u.contains("/messages"))
            .all(|(_, _, v)| v.get("receive_id").is_none_or(|id| id == "oc_source")),
        "Fork must not send parent history to the target"
    );
}

#[test]
fn source_picker_rechecks_sender_source_and_permissions_before_creating() {
    for change in ["sender", "card", "source", "permission", "scope", "expired"] {
        let f = Fixture::new();
        start(&f.env(), f.source("ou_owner"), "").unwrap();
        let mut card = f.card("fork_submit");
        card.value["mode"] = json!("new");
        card.value["form_value"] = json!({"fork_name":"Target"});
        match change {
            "sender" => card.operator_open_id = "ou_operator".into(),
            "card" => card.message_id = "om_forged".into(),
            "source" => resume_store::write_checked(
                &f.agent,
                &f.source_session,
                "codex",
                "different-sid",
                Some("/repo"),
                None,
            )
            .unwrap(),
            "permission" => {
                let raw = std::fs::read_to_string(f.agent.manifest_path()).unwrap();
                std::fs::write(
                    f.agent.manifest_path(),
                    raw.replace(
                        "operators: [ou_owner, ou_operator]",
                        "operators: [ou_other]",
                    ),
                )
                .unwrap();
            }
            "scope" => std::fs::write(
                f.agent.dir.join(".env"),
                "FORK_APP=other-app\nFORK_SECRET=test-secret\n",
            )
            .unwrap(),
            "expired" => with_record_lock(&f.agent, "fork-requests", &f.ticket(), |path| {
                let mut r: Request = serde_json::from_slice(&std::fs::read(path)?)?;
                r.issued_at -= TTL_SECONDS + 1;
                save(path, &r)
            })
            .unwrap(),
            _ => unreachable!(),
        }
        assert!(callback(&f.env(), &card).is_err(), "{change}");
        assert_eq!(f.creations(), 0, "{change}");
        assert!(matches!(
            resume_store::inspect_record(&f.agent, &f.target()),
            resume_store::ResumePresence::Missing
        ));
    }
}

#[test]
fn non_owner_can_select_an_existing_group_but_cannot_create_one() {
    let f = Fixture::new();
    assert!(start(&f.env(), f.source("ou_operator"), "new Target").is_err());
    assert_eq!(f.creations(), 0);
    start(&f.env(), f.source("ou_operator"), "to Target").unwrap();
    assert!(
        resume_store::read_record(&f.agent, &f.target())
            .unwrap()
            .fork_pending
    );
    assert_eq!(f.creations(), 0);
}

#[test]
fn existing_target_session_is_preserved_even_when_idle() {
    let f = Fixture::new();
    resume_store::write_checked(
        &f.agent,
        &f.target(),
        "codex",
        "existing-sid",
        Some("/other"),
        None,
    )
    .unwrap();
    let before = std::fs::read(resume_store::record_path(&f.agent, &f.target())).unwrap();
    assert!(start(&f.env(), f.source("ou_operator"), "to Target").is_err());
    assert_eq!(
        before,
        std::fs::read(resume_store::record_path(&f.agent, &f.target())).unwrap()
    );
    assert_eq!(f.creations(), 0);
}

#[test]
fn failed_receipt_can_resume_the_committed_request_without_recreating_the_group() {
    let f = Fixture::new();
    f.fail_messages.store(true, Ordering::SeqCst);
    assert!(start(&f.env(), f.source("ou_owner"), "new Target").is_err());
    assert!(f.read().state.is_committed());
    let sid = resume_store::read_record(&f.agent, &f.target()).unwrap().id;
    f.fail_messages.store(false, Ordering::SeqCst);
    callback(&f.env(), &f.card("fork_execute")).unwrap();
    assert_eq!(f.creations(), 1);
    assert_eq!(
        resume_store::read_record(&f.agent, &f.target()).unwrap().id,
        sid
    );
}

#[test]
fn committed_fork_is_recovered_after_a_missing_request_receipt() {
    let f = Fixture::new();
    start(&f.env(), f.source("ou_owner"), "new Target").unwrap();
    with_record_lock(&f.agent, "fork-requests", &f.ticket(), |path| {
        let mut wire: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        wire["committed"] = json!(false);
        atomic_write(path, &serde_json::to_vec(&wire)?)
    })
    .unwrap();
    callback(&f.env(), &f.card("fork_execute")).unwrap();
    assert!(f.read().state.is_committed());
    assert_eq!(f.creations(), 1);
}

#[test]
fn fork_candidate_selection_only_uses_the_saved_choice() {
    let f = Fixture::new();
    start(&f.env(), f.source("ou_operator"), "").unwrap();
    with_record_lock(&f.agent, "fork-requests", &f.ticket(), |path| {
        let mut request: Request = serde_json::from_slice(&std::fs::read(path)?)?;
        request.candidates = vec![
            GroupCandidate {
                chat_id: "oc_other".into(),
                name: "Target".into(),
                description: "Other".into(),
                member_count: None,
            },
            GroupCandidate {
                chat_id: "oc_new".into(),
                name: "Target".into(),
                description: "Selected".into(),
                member_count: None,
            },
        ];
        save(path, &request)
    })
    .unwrap();
    let mut card = f.card("fork_choose");
    card.value["index"] = json!(9);
    assert!(callback(&f.env(), &card).is_err());
    card.value["index"] = json!(1);
    card.value["chat_id"] = json!("oc_forged");
    callback(&f.env(), &card).unwrap();
    assert!(
        matches!(f.read().state, RequestState::Committed { target, .. } if target.id == "oc_new")
    );
    assert!(
        resume_store::read_record(&f.agent, &f.target())
            .unwrap()
            .fork_pending
    );
    assert_eq!(f.creations(), 0);
    assert!(f
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|(_, url, _)| !url.contains("oc_forged")));
}

#[test]
fn fork_preflight_rejection_allows_another_target_from_the_same_card() {
    let f = Fixture::new();
    resume_store::write_checked(
        &f.agent,
        &f.target(),
        "codex",
        "existing",
        Some("/other"),
        None,
    )
    .unwrap();
    start(&f.env(), f.source("ou_operator"), "").unwrap();
    let mut card = f.card("fork_submit");
    card.value["mode"] = json!("existing");
    card.value["form_value"] = json!({"fork_name":"Target"});
    assert!(callback(&f.env(), &card).is_err());
    assert_eq!(f.read().state, RequestState::Choosing);
    card.value["form_value"] = json!({"fork_name":"Other"});
    callback(&f.env(), &card).unwrap();
    assert!(
        matches!(f.read().state, RequestState::Committed { target, .. } if target.id == "oc_other")
    );
    assert_eq!(
        resume_store::read_record(&f.agent, &f.target()).unwrap().id,
        "existing"
    );
    assert_eq!(f.creations(), 0);
}

#[test]
fn fork_pending_external_operation_keeps_the_original_target() {
    let f = Fixture::new();
    start(&f.env(), f.source("ou_operator"), "").unwrap();
    f.fail_messages.store(true, Ordering::SeqCst);
    let mut card = f.card("fork_submit");
    card.value["mode"] = json!("existing");
    card.value["form_value"] = json!({"fork_name":"Target"});
    assert!(callback(&f.env(), &card).is_err());
    assert!(
        matches!(f.read().state, RequestState::Locked { selection: Selection::Existing(g), .. } if g.chat_id == "oc_new")
    );
    card.value["form_value"] = json!({"fork_name":"Other"});
    assert!(callback(&f.env(), &card).is_err());
    f.fail_messages.store(false, Ordering::SeqCst);
    callback(&f.env(), &f.card("fork_execute")).unwrap();
    assert!(
        matches!(f.read().state, RequestState::Committed { target, .. } if target.id == "oc_new")
    );
    assert_eq!(f.creations(), 0);
}

#[test]
fn new_target_locks_before_the_pending_card_or_group_creation() {
    let f = Fixture::new();
    start(&f.env(), f.source("ou_owner"), "").unwrap();
    f.fail_messages.store(true, Ordering::SeqCst);
    let mut card = f.card("fork_submit");
    card.value["mode"] = json!("new");
    card.value["form_value"] = json!({"fork_name":"Target"});
    assert!(callback(&f.env(), &card).is_err());
    assert_eq!(f.creations(), 0);
    assert!(
        matches!(f.read().state, RequestState::Locked { selection: Selection::New(name), target: None } if name == "Target")
    );
    card.value["form_value"] = json!({"fork_name":"Other"});
    assert!(callback(&f.env(), &card).is_err());
    f.fail_messages.store(false, Ordering::SeqCst);
    callback(&f.env(), &f.card("fork_execute")).unwrap();
    assert!(f.read().state.is_committed());
    assert_eq!(f.creations(), 1);
}

#[test]
fn recovery_requires_both_the_exact_preminted_sid_and_fork_pending() {
    for change in ["sid", "fork_pending"] {
        let f = Fixture::new();
        start(&f.env(), f.source("ou_operator"), "to Target").unwrap();
        let sid = resume_store::read_record(&f.agent, &f.target()).unwrap().id;
        with_record_lock(&f.agent, "fork-requests", &f.ticket(), |path| {
            let mut wire: Value = serde_json::from_slice(&std::fs::read(path)?)?;
            wire["committed"] = json!(false);
            atomic_write(path, &serde_json::to_vec(&wire)?)
        })
        .unwrap();
        let record_path = resume_store::record_path(&f.agent, &f.target());
        let record = std::fs::read_to_string(&record_path).unwrap();
        let record = match change {
            "sid" => record.replace(&format!("id={sid}\n"), "id=someone-elses-sid\n"),
            _ => record.replace("fork_pending=true\n", ""),
        };
        std::fs::write(&record_path, record).unwrap();
        let before = std::fs::read(&record_path).unwrap();
        assert!(
            callback(&f.env(), &f.card("fork_execute")).is_err(),
            "{change}"
        );
        assert!(!f.read().state.is_committed());
        assert_eq!(std::fs::read(&record_path).unwrap(), before);
        if change == "fork_pending" {
            assert_eq!(
                resume_store::read_record(&f.agent, &f.target()).unwrap().id,
                sid
            );
        }
    }
}

#[test]
fn corrupt_completion_is_rejected_before_any_network_effect() {
    let f = Fixture::new();
    start(&f.env(), f.source("ou_operator"), "").unwrap();
    let card = f.card("fork_execute");
    with_record_lock(&f.agent, "fork-requests", &f.ticket(), |path| {
        let mut wire: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        wire["committed"] = json!(true);
        wire["target_id"] = json!("oc_new");
        wire["target_name"] = json!("Target");
        atomic_write(path, &serde_json::to_vec(&wire)?)
    })
    .unwrap();
    let calls_before = f.calls.lock().unwrap().len();
    assert!(callback(&f.env(), &card).is_err());
    assert_eq!(f.calls.lock().unwrap().len(), calls_before);
    assert!(matches!(
        resume_store::inspect_record(&f.agent, &f.target()),
        resume_store::ResumePresence::Missing
    ));
}
