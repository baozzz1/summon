//! `docs.*`:三层说明文件(`~/.summon/CLAUDE.md`、agent 的 `CLAUDE.md` / `SOUL.md`、本群
//! `chats/<chat_id>/CLAUDE.md`)。按版本校验后原子替换,新会话生效。

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;
use summon_core::dashboard::authz::Target;
use summon_core::dashboard::fields::Effect;

use super::types::{AgentDocFile, DocRef, DocView};
use super::{internal, invalid, outcome, params, reply, shown_path, ApiEnv};
use crate::runtime::dashboard::server::{ApiCtx, ApiError, ErrorCode};
use crate::runtime::dashboard::writer;

struct Doc {
    path: PathBuf,
    /// 改它要过的判权:`None` = 整机;`(agent, None)` = agent;`(agent, Some(chat))` = 本群。
    owner: Option<(String, Option<String>)>,
    label: String,
}

impl Doc {
    fn target(&self) -> Target<'_> {
        match &self.owner {
            None => Target::Machine,
            Some((agent, None)) => Target::Agent(agent),
            Some((agent, Some(chat_id))) => Target::Chat { agent, chat_id },
        }
    }
}

/// 说明文件的路径与改它要过的判权。
fn resolve(env: &ApiEnv, doc: &DocRef) -> Result<Doc, ApiError> {
    Ok(match doc {
        DocRef::Global => Doc {
            path: env.home.join("CLAUDE.md"),
            owner: None,
            label: "doc:global".into(),
        },
        DocRef::Agent { agent, file } => {
            let a = env.agent(agent)?;
            let name = match file {
                AgentDocFile::Claude => "CLAUDE.md",
                AgentDocFile::Soul => "SOUL.md",
            };
            Doc {
                path: a.dir.join(name),
                owner: Some((a.name.clone(), None)),
                label: format!("doc:{agent}:{name}"),
            }
        }
        DocRef::Chat { agent, chat_id } => {
            let a = env.agent(agent)?;
            let path = super::chats::chat_doc_path(&a, chat_id)
                .ok_or_else(|| invalid(summon_i18n::text!("r5169498fc1")))?;
            Doc {
                path,
                owner: Some((a.name.clone(), Some(chat_id.clone()))),
                label: format!("doc:{agent}:{chat_id}"),
            }
        }
    })
}

/// `StatusView.revisions` 里说明文件的键:`doc:<显示路径>`。
pub(crate) fn revision_key(path: &std::path::Path) -> String {
    format!("doc:{}", shown_path(path))
}

fn view(env: &ApiEnv, ctx: &ApiCtx, doc: &Doc) -> Result<DocView, ApiError> {
    let text = writer::read_optional(&doc.path).map_err(internal)?;
    Ok(DocView {
        exists: text.is_some(),
        revision: text
            .as_deref()
            .map(|t| writer::revision(Some(t.as_bytes()))),
        text: text.unwrap_or_default(),
        path: shown_path(&doc.path),
        editable: env.decide(ctx, &doc.target()).is_allowed(),
        effect: Effect::NewSession,
    })
}

#[derive(Deserialize)]
struct GetParams {
    doc: DocRef,
}

pub(crate) fn get(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let GetParams { doc } = params(p)?;
    let doc = resolve(env, &doc)?;
    reply(&view(env, ctx, &doc)?)
}

#[derive(Deserialize)]
struct PutParams {
    doc: DocRef,
    revision: Option<String>,
    text: String,
}

pub(crate) fn put(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: PutParams = params(p)?;
    let doc = resolve(env, &req.doc)?;
    let keys = vec![doc.label.clone()];
    let result = (|| {
        env.require(ctx, &doc.target())?;
        let _guard = env.docs_lock();
        let current = writer::read_optional(&doc.path).map_err(internal)?;
        let fresh = current
            .as_deref()
            .map(|t| writer::revision(Some(t.as_bytes())));
        // revision 为 null = 新建:文件此刻必须还不存在。
        if fresh != req.revision {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                summon_i18n::text!("r7a1678aef9"),
            ));
        }
        if current.as_deref() != Some(req.text.as_str()) {
            let mode = std::fs::metadata(&doc.path)
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o644);
            let mut raced = false;
            let written = crate::runtime::fsio::atomic_write_mode_checked(
                &doc.path,
                req.text.as_bytes(),
                mode,
                || {
                    // 手改不持锁:提交前再比一次。
                    if writer::read_optional(&doc.path)? != current {
                        raced = true;
                        anyhow::bail!(summon_i18n::format!("r8e82b0c0ac"));
                    }
                    Ok(())
                },
            );
            match written {
                Ok(()) => {}
                Err(_) if raced => {
                    return Err(ApiError::new(
                        ErrorCode::Conflict,
                        summon_i18n::text!("r7a1678aef9"),
                    ))
                }
                Err(e) => return Err(internal(format!("{e:#}"))),
            }
        }
        view(env, ctx, &doc)
    })();
    env.audit(ctx, &doc.label, &keys, &outcome(&result));
    reply(&result?)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::runtime::dashboard::sessions::ChatKind;
    use serde_json::json;

    #[test]
    fn chat_doc_is_created_then_edited_with_revisions() {
        let kit = Kit::new();
        let a = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        // 另一个 agent 的名单里没有我:整机层的改动(全局说明)就不让。
        kit.add_agent(
            "arcadia",
            &agent_yaml("arcadia", "阿卡迪亚", "codex", "").replace(
                &format!("operators: [{ME_UNION}]"),
                &format!("operators: [{PEER_UNION}]"),
            ),
        );
        kit.add_chat(&a, "group", GROUP, "示例群");
        let ctx = kit.feishu(issuer("starlight", GROUP, ChatKind::Group));
        let doc = json!({"level": "chat", "agent": "starlight", "chat_id": GROUP});
        let empty = kit.call(&ctx, get, json!({ "doc": doc })).unwrap();
        assert_eq!(empty["exists"], false);
        assert!(empty.get("revision").is_none());
        assert_eq!(empty["effect"], "new_session");
        let created = kit
            .call(
                &ctx,
                put,
                json!({"doc": doc, "revision": null, "text": "本群只谈 CI。\n"}),
            )
            .unwrap();
        assert_eq!(created["exists"], true);
        let path = a.dir.join("chats").join(GROUP).join("CLAUDE.md");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "本群只谈 CI。\n");
        // 新建时文件已经在了:冲突。
        let err = kit
            .call(
                &ctx,
                put,
                json!({"doc": doc, "revision": null, "text": "x"}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        kit.call(
            &ctx,
            put,
            json!({"doc": doc, "revision": created["revision"], "text": "改过了\n"}),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "改过了\n");
        // 全局说明是整机层:要对每个启用中的 agent 都有明确授权。
        let err = kit
            .call(
                &ctx,
                put,
                json!({"doc": {"level": "global"}, "revision": null, "text": "x"}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        let err = kit
            .call(
                &ctx,
                get,
                json!({"doc": {"level": "chat", "agent": "starlight", "chat_id": "../x"}}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
    }

    #[test]
    fn soul_doc_keeps_mode_bits() {
        let kit = Kit::new();
        let a = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        let soul = a.dir.join("SOUL.md");
        std::fs::write(&soul, "我是谁\n").unwrap();
        std::fs::set_permissions(&soul, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ctx = kit.owner();
        let doc = json!({"level": "agent", "agent": "starlight", "file": "SOUL.md"});
        let view = kit.call(&ctx, get, json!({ "doc": doc })).unwrap();
        kit.call(
            &ctx,
            put,
            json!({"doc": doc, "revision": view["revision"], "text": "我是新的我\n"}),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&soul).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
