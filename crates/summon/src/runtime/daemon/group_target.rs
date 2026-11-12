//! Delegate 与 Fork 共用的群解析、成员验证及建群持久记录。

use super::agent_env::{agent_owner, owner_id_matches, resolve_creds};
use super::sentinel::lark::LarkUser;
use crate::frontend::client::FeishuClient;
use crate::runtime::fsio::atomic_write;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::path::Path;
use summon_core::registry::Agent;
use summon_core::session::group_target::{validate_name, GroupCandidate};

pub(crate) struct Groups<'a> {
    pub agent: &'a Agent,
    pub fc: &'a FeishuClient,
    pub requester: &'a str,
    pub union_id: &'a str,
    lark: LarkUser,
}

impl<'a> Groups<'a> {
    pub fn new(
        agent: &'a Agent,
        fc: &'a FeishuClient,
        requester: &'a str,
        union_id: &'a str,
    ) -> Self {
        let cfg = summon_core::config::Config::load();
        let bin = summon_core::tools::resolve_from(&cfg, summon_core::tools::Tool::LarkCli);
        Self {
            agent,
            fc,
            requester,
            union_id,
            lark: LarkUser::new(
                cfg.sentinel.lark_profile,
                Some(bin.to_string_lossy().into_owned()),
            )
            .with_timeout(std::time::Duration::from_secs(30)),
        }
    }

    pub fn is_owner(&self) -> bool {
        agent_owner(self.agent).is_some_and(|owner| {
            !self.requester.is_empty() && owner_id_matches(&owner, self.requester, self.union_id)
        })
    }

    pub fn search(&self, name: &str, source_chat: &str) -> Result<Vec<GroupCandidate>> {
        let name = validate_name(name).map_err(anyhow::Error::msg)?;
        let mut groups = if self.is_owner() {
            self.lark.verified_requester(self.union_id)?;
            self.lark.search_exact_groups(name)?
        } else {
            self.fc.groups_named(name)?
        };
        let had_source = groups.iter().any(|g| g.chat_id == source_chat);
        groups.retain(|g| g.chat_id != source_chat);
        ensure!(
            !groups.is_empty() || !had_source,
            summon_i18n::format!("rcb80f19694")
        );
        if !self.is_owner() {
            for group in &groups {
                self.fc
                    .prove_user_in_chat(&group.chat_id, self.requester)
                    .context(summon_i18n::text!("r5a30eae30c"))?;
            }
        }
        Ok(groups)
    }

    pub fn info(&self, id: &str) -> Result<(String, String)> {
        valid_chat_id(id)?;
        match self.fc.delegation_group_info(id) {
            Ok(info) => Ok(info),
            Err(error) if !self.is_owner() => Err(error),
            Err(_) => {
                self.lark.verified_requester(self.union_id)?;
                let info = self.lark.chat(id)?;
                Ok((info.name, info.chat_mode))
            }
        }
    }

    pub fn prove_requester(&self, id: &str) -> Result<()> {
        if self.fc.prove_user_in_chat(id, self.requester).is_ok() {
            return Ok(());
        }
        ensure!(self.is_owner(), summon_i18n::format!("r71e3d19e5b"));
        let owner = self.lark.verified_requester(self.union_id)?;
        self.lark.prove_user_member(id, &owner)
    }

    pub fn ensure_bot(&self, operation: &str, id: &str) -> Result<()> {
        valid_chat_id(id)?;
        if self.bot_present(id)? {
            return Ok(());
        }
        ensure!(self.is_owner(), summon_i18n::format!("r0a27803db5"));
        self.lark.verified_requester(self.union_id)?;
        let app = resolve_creds(self.agent)
            .0
            .context(summon_i18n::text!("rc58f23f83d"))?;
        with_record_lock(self.agent, "group-invites", operation, |path| {
            if path.exists() {
                let old: String = serde_json::from_slice(&std::fs::read(path)?)?;
                ensure!(old == id, summon_i18n::format!("r5d0a3c71da"));
                ensure!(
                    self.lark.bots_in_chat(id)?.contains(&app),
                    summon_i18n::format!("r80cb0385df")
                );
            } else {
                atomic_write(path, &serde_json::to_vec(id)?)?;
                self.lark.add_bot(id, &app)?;
            }
            ensure!(
                self.lark.bots_in_chat(id)?.contains(&app),
                summon_i18n::format!("r83930fe4e1")
            );
            Ok(())
        })
    }

    pub fn bot_present(&self, id: &str) -> Result<bool> {
        self.prove_requester(id)?;
        if self.fc.prove_bot_in_chat(id).is_ok() {
            return Ok(true);
        }
        ensure!(self.is_owner(), summon_i18n::format!("r0a27803db5"));
        self.lark.verified_requester(self.union_id)?;
        let app = resolve_creds(self.agent)
            .0
            .context(summon_i18n::text!("rc58f23f83d"))?;
        Ok(self.lark.bots_in_chat(id)?.contains(&app))
    }

    pub fn create(
        &self,
        operation: &str,
        name: &str,
        check: impl Fn() -> Result<()>,
    ) -> Result<String> {
        ensure!(self.is_owner(), summon_i18n::format!("ra9b6f1e7bc"));
        let name = validate_name(name).map_err(anyhow::Error::msg)?;
        let app = resolve_creds(self.agent)
            .0
            .context(summon_i18n::text!("rc58f23f83d"))?;
        with_record_lock(self.agent, "group-creates", operation, |path| {
            create_once(path, name, self.requester, &app, &check, || {
                self.fc.create_chat(
                    name,
                    self.requester,
                    &[self.requester.into()],
                    std::slice::from_ref(&app),
                )
            })
        })
    }
}

pub(crate) fn valid_chat_id(id: &str) -> Result<()> {
    ensure!(
        id.starts_with("oc_")
            && id.len() <= 128
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        summon_i18n::format!("rcac179189f")
    );
    Ok(())
}

pub(crate) fn with_record_lock<T>(
    agent: &Agent,
    namespace: &str,
    operation: &str,
    f: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let dir = agent.state_dir().join("runtime").join(namespace);
    std::fs::create_dir_all(&dir)?;
    let key = summon_core::registry::chat_tag("group", operation);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(format!("{key}.lock")))?;
    lock.lock()?;
    f(&dir.join(format!("{key}.json")))
}

#[derive(Serialize, Deserialize)]
struct Creation {
    name: String,
    owner: String,
    app: String,
    chat_id: Option<String>,
}

fn create_once(
    path: &Path,
    name: &str,
    owner: &str,
    app: &str,
    check: &impl Fn() -> Result<()>,
    create: impl FnOnce() -> Result<String>,
) -> Result<String> {
    check()?;
    if path.exists() {
        let record: Creation = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            record.name == name && record.owner == owner && record.app == app,
            summon_i18n::format!("r95d70a050b")
        );
        return record.chat_id.context(summon_i18n::text!("r6a2eda34b9"));
    }
    let mut record = Creation {
        name: name.into(),
        owner: owner.into(),
        app: app.into(),
        chat_id: None,
    };
    atomic_write(path, &serde_json::to_vec(&record)?)?;
    let id = create()?;
    valid_chat_id(&id)?;
    record.chat_id = Some(id.clone());
    atomic_write(path, &serde_json::to_vec(&record)?)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn group_user_fallbacks_verify_account_before_reading_chat_data() {
        let (agent, dir) =
            crate::test_support::agent_dir("codex", "feishu:\n  allowed_users: [ou_owner]\n");
        let exe = dir.path().join("lark");
        crate::test_support::write_test_executable(
            &exe,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "${0}.calls"
printf '%s' '{"ok":true,"data":{"union_id":"on_other","open_id":"ou_other"}}'
"#,
        );
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let client = FeishuClient::with_test_base_connection_close(format!(
            "http://{}",
            server.server_addr().to_ip().unwrap()
        ));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(request) = server
                    .recv_timeout(std::time::Duration::from_millis(10))
                    .unwrap()
                {
                    let body = if request.url().contains("auth/v3/tenant_access_token") {
                        r#"{"tenant_access_token":"token","expire":7200}"#
                    } else if request.url().contains("/members?") {
                        r#"{"code":0,"data":{"items":[{"member_id":"ou_owner"}],"has_more":false}}"#
                    } else {
                        r#"{"code":230001,"msg":"not visible"}"#
                    };
                    request
                        .respond(tiny_http::Response::from_string(body))
                        .unwrap();
                }
            }
        });
        let groups = Groups {
            agent: &agent,
            fc: &client,
            requester: "ou_owner",
            union_id: "on_owner",
            lark: LarkUser::new(None, Some(exe.to_string_lossy().into_owned())),
        };
        for result in [
            groups.info("oc_target").map(|_| ()),
            groups.bot_present("oc_target").map(|_| ()),
            groups.search("Target", "oc_source").map(|_| ()),
        ] {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("登录用户与发起人不一致"));
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        thread.join().unwrap();
        let calls = std::fs::read_to_string(exe.with_extension("calls")).unwrap();
        assert_eq!(calls.lines().count(), 3);
        assert!(calls
            .lines()
            .all(|line| line == "api GET /open-apis/authen/v1/user_info --as user"));
    }

    #[test]
    fn created_group_is_reused_and_changed_request_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("create.json");
        let check = || Ok(());
        assert_eq!(
            create_once(&p, "Alpha", "ou_owner", "app", &check, || Ok(
                "oc_new".into()
            ))
            .unwrap(),
            "oc_new"
        );
        assert_eq!(
            create_once(&p, "Alpha", "ou_owner", "app", &check, || panic!(
                "duplicate create"
            ))
            .unwrap(),
            "oc_new"
        );
        assert!(create_once(&p, "Beta", "ou_owner", "app", &check, || panic!()).is_err());
        assert!(create_once(&p, "Alpha", "ou_other", "app", &check, || panic!()).is_err());
    }
    #[test]
    fn ambiguous_create_and_revoked_authority_never_retry_external_action() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("create.json");
        assert!(
            create_once(&p, "Alpha", "owner", "app", &|| Ok(()), || anyhow::bail!(
                "timeout"
            ))
            .is_err()
        );
        assert!(create_once(&p, "Alpha", "owner", "app", &|| Ok(()), || panic!("retry")).is_err());
        let fresh = dir.path().join("revoked.json");
        assert!(create_once(
            &fresh,
            "Alpha",
            "owner",
            "app",
            &|| anyhow::bail!("revoked"),
            || panic!()
        )
        .is_err());
        assert!(!fresh.exists());
    }

    #[test]
    fn concurrent_group_creates_share_one_external_operation() {
        let (agent, _dir) = crate::test_support::agent_dir("codex", "");
        let count = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|threads| {
            let work = || {
                with_record_lock(&agent, "group-creates", "operation", |path| {
                    create_once(path, "Target", "owner", "app", &|| Ok(()), || {
                        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        Ok("oc_target".into())
                    })
                })
            };
            let a = threads.spawn(work);
            let b = threads.spawn(work);
            assert_eq!(a.join().unwrap().unwrap(), "oc_target");
            assert_eq!(b.join().unwrap().unwrap(), "oc_target");
        });
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
