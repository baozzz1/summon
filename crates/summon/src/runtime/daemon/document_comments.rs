//! 文档评论通知、草稿投递与 owner 批准后的 bot 回复。

use super::outbox_drain::{DrainAgents, OutboxDrainAgent};
use super::{agent_owner, log, owner_id_matches, resolve_creds, CardActionMsg};
use crate::frontend::client::FeishuClient;
use crate::runtime::document_comments::{CommentNotice, Record, Stage, Store};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use summon_core::registry::Agent;

pub(super) fn enabled(agent: &Agent) -> bool {
    agent.reread().is_some_and(|current| {
        current.is_routable() && super::resolve_messaging(&current).document_comments_enabled
    })
}

pub(super) fn receive_notice(
    store: &Store,
    core: &crate::runtime::core::Core,
    notice: CommentNotice,
    at: i64,
) -> Result<String> {
    let agents: Vec<_> = core
        .known_agent_aliases()
        .iter()
        .filter_map(|alias| core.current_gate_agent(alias))
        .filter(|agent| resolve_creds(agent).0.as_deref() == Some(notice.app_id.as_str()))
        .collect();
    let active = matches!(agents.as_slice(), [agent] if enabled(agent));
    store.receive(notice, at, active)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn current_agent(delivery: &OutboxDrainAgent, record: &Record) -> Result<Agent> {
    let current = delivery
        .agent
        .reread()
        .context(summon_i18n::text!("r50c89c40c6"))?;
    anyhow::ensure!(
        current.is_routable() && delivery.still_matches(&current),
        summon_i18n::format!("rdd82a8d072")
    );
    if record.stage != Stage::Received {
        anyhow::ensure!(
            record.agent_name == current.name
                && record.epoch == delivery.epoch.fingerprint()
                && record.delivery_scope == delivery.delivery_scope()
                && agent_owner(&current).as_deref() == Some(record.owner_entry.as_str()),
            summon_i18n::format!("rba920033ee")
        );
    }
    Ok(current)
}

fn owner_ids(agent: &Agent, fc: &FeishuClient) -> Result<(String, String, String)> {
    let entry = agent_owner(agent)
        .filter(|s| !s.is_empty())
        .context(summon_i18n::text!("r0a814c2006"))?;
    if entry.starts_with("on_") {
        let open = fc
            .resolve_open_id_by_union(&entry)
            .context(summon_i18n::text!("r1d04034b16"))?;
        Ok((entry.clone(), open, entry))
    } else {
        anyhow::ensure!(
            entry.starts_with("ou_"),
            summon_i18n::format!("rc2c4f8549a")
        );
        Ok((entry.clone(), entry, String::new()))
    }
}

pub(super) fn spawn(drain: DrainAgents, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let store = Store::at_run_dir();
        let mut reported_unreadable: Vec<String> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            match store.inbox() {
                Ok((records, unreadable)) => {
                    // 读不出的存根每 5 秒都会再扫到,集合变了才重报。
                    if unreadable != reported_unreadable {
                        for error in &unreadable {
                            log(&summon_i18n::format!("r3f751ad734"));
                        }
                        reported_unreadable = unreadable;
                    }
                    for record in records {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let _writer = crate::runtime::agent_retirement::COMMENT_WRITERS
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let Ok(record) = store.get(&record.id) else {
                            continue;
                        };
                        let matched = drain
                            .read()
                            .ok()
                            .map(|agents| {
                                agents
                                    .values()
                                    .filter(|d| {
                                        resolve_creds(&d.agent).0.as_deref()
                                            == Some(record.notice.app_id.as_str())
                                    })
                                    .cloned()
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        let [delivery] = matched.as_slice() else {
                            continue;
                        };
                        if let Err(error) = advance_once(&store, &record, delivery, &stop) {
                            log(&summon_i18n::format!("rb57d0b804f", record.id));
                        } else if record.last_error.is_some() {
                            let _ = store.update(&record.id, |r| {
                                if r.stage != Stage::Closed {
                                    r.last_error = None;
                                }
                                Ok(())
                            });
                        }
                    }
                }
                Err(error) => log(&summon_i18n::format!("r14a0af604e")),
            }
            for _ in 0..5 {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    })
}

fn advance_once(
    store: &Store,
    record: &Record,
    delivery: &OutboxDrainAgent,
    stop: &AtomicBool,
) -> Result<()> {
    if let Err(error) = advance(store, record, delivery, stop) {
        // 失败要告诉 owner,无论草稿出没出来。不在处理范围内的文档(元数据里查不到、不归本 bot)
        // 仍记作处理失败、原因可见,但不通知;功能已关或身份已变时也不发,关闭期间的失败不在
        // 重新开启后补报。
        let notify = !crate::runtime::document_comments::is_out_of_scope(&error)
            && current_agent(delivery, record).is_ok_and(|agent| enabled(&agent));
        store.update(&record.id, |r| {
            if r.stage != Stage::Closed {
                r.last_error = Some(format!("{error:#}").chars().take(2000).collect());
                r.draft_process = None;
                r.failures = r.failures.saturating_add(1);
                r.retry_after_ms = 0;
                r.notify_failure = notify;
                r.close(summon_i18n::text!("r2c06b564c1"));
            } else {
                r.failure_notice_attempts = r.failure_notice_attempts.max(1);
            }
            Ok(())
        })?;
        return Err(error);
    }
    Ok(())
}

fn advance(
    store: &Store,
    record: &Record,
    delivery: &OutboxDrainAgent,
    stop: &AtomicBool,
) -> Result<()> {
    let agent = current_agent(delivery, record)?;
    anyhow::ensure!(
        record.stage == Stage::Closed || record.failures == 0,
        summon_i18n::format!("r8a387d2cb1")
    );
    if !enabled(&agent) {
        if record.stage == Stage::Received {
            store.update(&record.id, |r| {
                if r.stage == Stage::Received {
                    r.close(summon_i18n::text!("r808813edad"));
                }
                Ok(())
            })?;
        } else if record.needs_failure_notice() {
            // 功能关闭时还没发出去的失败通知就此作废,重新开启后不补报。
            store.update(&record.id, |r| {
                r.failure_notice_attempts = r.failure_notice_attempts.max(1);
                Ok(())
            })?;
        }
        return Ok(());
    }
    let fc = &delivery.client;
    match record.stage {
        Stage::Received => {
            let bot = fc.bot_open_id()?;
            if record.notice.sender_open_id == bot || record.notice.recipient_open_id != bot {
                return store.update(&record.id, |r| {
                    r.close(summon_i18n::text!("rbfadc05655"));
                    Ok(())
                });
            }
            let (entry, open, union) = owner_ids(&agent, fc)?;
            store.update(&record.id, |r| {
                r.agent_name = agent.name.clone();
                r.delivery_scope = delivery.delivery_scope().to_string();
                r.epoch = delivery.epoch.fingerprint();
                r.owner_entry = entry.clone();
                r.owner_open_id = open.clone();
                r.owner_union_id = union.clone();
                r.bot_open_id = bot.clone();
                Ok(())
            })?;
            let context = fc.owned_comment_context(&record.notice, &bot, &open)?;
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            current_agent(delivery, record)?;
            store.update(&record.id, |r| {
                if r.stage == Stage::Received {
                    r.agent_name = agent.name.clone();
                    r.delivery_scope = delivery.delivery_scope().to_string();
                    r.epoch = delivery.epoch.fingerprint();
                    r.owner_entry = entry;
                    r.owner_open_id = open;
                    r.owner_union_id = union;
                    r.bot_open_id = bot;
                    r.context = Some(context);
                    r.stage = Stage::Prepared;
                }
                Ok(())
            })
        }
        Stage::Prepared if record.draft.is_none() => store.update(&record.id, |r| {
            if r.stage == Stage::Prepared && r.draft.is_none() {
                r.draft_started = false;
                r.stage = Stage::Drafting;
            }
            Ok(())
        }),
        Stage::Notified => store.update(&record.id, |r| {
            if r.stage == Stage::Notified {
                r.draft_started = false;
                r.stage = Stage::Drafting;
            }
            Ok(())
        }),
        Stage::Drafting => {
            // 起草进程会被 daemon 停机打断、按失败通知 owner,所以和一轮一样过准入闸:换代前关闸时
            // 在这里等,停机开始后本代不起草,存根留在起草阶段由下一代接着做;起草期间的登记让换代
            // 方看得见它还在忙。
            let Some(_drafting) = crate::runtime::admission::GATE.admit(
                &agent.name,
                &format!("comment-{}", record.id.get(..12).unwrap_or(&record.id)),
                crate::runtime::admission::Kind::Task,
            ) else {
                return Ok(());
            };
            store.claim_draft(&record.id)?;
            let draft = super::document_comment_draft::generate_with_client(
                &agent,
                record,
                delivery.client.clone(),
                &|| {
                    stop.load(Ordering::Relaxed)
                        || store
                            .get(&record.id)
                            .map_or(true, |r| r.stage != Stage::Drafting)
                },
                Some(store.clone()),
                &|pid| {
                    store.update(&record.id, |r| {
                        anyhow::ensure!(
                            r.stage == Stage::Drafting,
                            summon_i18n::format!("rdc5dfd3aff")
                        );
                        r.draft_process = Some(
                            crate::runtime::document_comments::DraftProcessIdentity::capture(pid)
                                .context(summon_i18n::text!("rcb9fc9e104"))?,
                        );
                        Ok(())
                    })
                },
            );
            store.update(&record.id, |r| {
                r.draft_process = None;
                Ok(())
            })?;
            let draft = draft?;
            current_agent(delivery, record)?;
            store.set_draft(&record.id, record.revision, &draft, now_ms())
        }
        Stage::Prepared => {
            let uuid = format!("ccd-{}-{}", &record.id[..32], record.revision);
            let card = draft_card(record)?;
            anyhow::ensure!(card.len() <= 28_000, summon_i18n::format!("r95566df714"));
            let message = if let Some(message) = &record.approval_message_id {
                message.clone()
            } else {
                let message = fc.send_comment_owner_card(&record.owner_open_id, &card, &uuid)?;
                store.update(&record.id, |r| {
                    if r.stage == Stage::Prepared
                        && r.revision == record.revision
                        && r.draft == record.draft
                    {
                        r.approval_message_id = Some(message.clone());
                    }
                    Ok(())
                })?;
                message
            };
            let chat = fc.message_chat_id(&message)?;
            store.update(&record.id, |r| {
                if r.stage == Stage::Prepared
                    && r.revision == record.revision
                    && r.draft == record.draft
                    && r.approval_message_id.as_deref() == Some(message.as_str())
                {
                    r.owner_chat_id = Some(chat);
                    r.stage = Stage::DraftReady;
                }
                Ok(())
            })
        }
        Stage::Closed if record.needs_failure_notice() => {
            store.update(&record.id, |r| {
                anyhow::ensure!(
                    r.needs_failure_notice(),
                    summon_i18n::format!("r0d92cfcdc4")
                );
                r.failure_notice_attempts += 1;
                Ok(())
            })?;
            let reason = record
                .last_error
                .as_deref()
                .unwrap_or(summon_i18n::text!("r1c37d3d28c"));
            let card = failure_card(
                record,
                &summon_i18n::format!("r158c801ed4", limited(reason, 1000)),
            );
            let message = fc.send_comment_owner_card(
                &record.owner_open_id,
                &card,
                &format!("ccf-{}", &record.id[..40]),
            )?;
            store.update(&record.id, |r| {
                r.failure_notice_message_id = Some(message);
                Ok(())
            })
        }
        Stage::DraftReady | Stage::Sending | Stage::Replied | Stage::Closed => Ok(()),
    }
}

pub(super) fn handle_card(
    agent: &Agent,
    fc: &FeishuClient,
    scope: &str,
    epoch: &super::FeishuEpochIdentity,
    card: &CardActionMsg,
) -> Result<()> {
    handle_card_with_store(&Store::at_run_dir(), agent, fc, scope, epoch, card)
}

fn handle_card_with_store(
    store: &Store,
    agent: &Agent,
    fc: &FeishuClient,
    scope: &str,
    epoch: &super::FeishuEpochIdentity,
    card: &CardActionMsg,
) -> Result<()> {
    let action = card.value["action"]
        .as_str()
        .context(summon_i18n::text!("rcfb3275606"))?;
    let id = card.value["comment"]
        .as_str()
        .context(summon_i18n::text!("r1399fe6604"))?;
    let record = store.get(id)?;
    let current = agent.reread().context(summon_i18n::text!("r50c89c40c6"))?;
    anyhow::ensure!(
        epoch.matches_current(&current)
            && current.is_routable()
            && record.epoch == epoch.fingerprint()
            && agent_owner(&current).as_deref() == Some(record.owner_entry.as_str()),
        summon_i18n::format!("r509c62b2ce")
    );
    let op_union = fc.resolve_union_id(&card.operator_open_id);
    anyhow::ensure!(
        owner_id_matches(&record.owner_entry, &card.operator_open_id, &op_union),
        summon_i18n::format!("r5e5fa5e405")
    );
    anyhow::ensure!(
        record.agent_name == agent.name
            && record.delivery_scope == scope
            && record.owner_chat_id.as_deref() == Some(card.chat_id.as_str())
            && record.approval_message_id.as_deref() == Some(card.message_id.as_str())
            && record.owner_open_id == card.operator_open_id,
        summon_i18n::format!("re05c7d6119")
    );
    if matches!(
        record.stage,
        Stage::Sending | Stage::Replied | Stage::Closed
    ) {
        let text = record
            .outcome
            .as_deref()
            .unwrap_or(summon_i18n::text!("r1d2a47d85c"));
        return fc.update_card(&card.message_id, &outcome_card(&record, text));
    }
    if record.drafted_ms.is_none_or(|at| {
        at > now_ms() || now_ms() - at >= crate::runtime::document_comments::DRAFT_TTL_MS
    }) {
        store.update(id, |r| {
            if r.stage == Stage::DraftReady {
                r.last_error = None;
                r.close(summon_i18n::text!("r9373299dbb"));
            }
            Ok(())
        })?;
        return fc.update_card(
            &card.message_id,
            &outcome_card(&record, summon_i18n::text!("r7a1100c8af")),
        );
    }
    anyhow::ensure!(
        record.approval_matches(
            &agent.name,
            scope,
            &card.chat_id,
            &card.message_id,
            &card.operator_open_id,
            now_ms()
        ),
        summon_i18n::format!("r48a93eed70")
    );
    if action == "doc_comment_dismiss" {
        store.update(id, |r| {
            anyhow::ensure!(
                r.approval_matches(
                    &agent.name,
                    scope,
                    &card.chat_id,
                    &card.message_id,
                    &card.operator_open_id,
                    now_ms()
                ),
                summon_i18n::format!("r5743b51f70")
            );
            r.last_error = None;
            r.close(summon_i18n::text!("reca9f5aa53"));
            Ok(())
        })?;
        return fc.update_card(
            &card.message_id,
            &outcome_card(&record, summon_i18n::text!("ra1ea031fb0")),
        );
    }
    anyhow::ensure!(
        action == "doc_comment_approve",
        summon_i18n::format!("r8d7fd7336f")
    );
    let context = match (|| -> Result<_> {
        anyhow::ensure!(enabled(&current), summon_i18n::format!("ra116fc6e49"));
        let context =
            fc.owned_comment_context(&record.notice, &record.bot_open_id, &record.owner_open_id)?;
        anyhow::ensure!(enabled(&current), summon_i18n::format!("ra116fc6e49"));
        Ok(context)
    })() {
        Ok(context) => context,
        Err(error) => {
            let failed = store.update(id, |r| {
                if r.approval_matches(
                    &agent.name,
                    scope,
                    &card.chat_id,
                    &card.message_id,
                    &card.operator_open_id,
                    now_ms(),
                ) {
                    r.last_error = Some(format!("{error:#}").chars().take(2000).collect());
                }
                Ok(r.clone())
            })?;
            let latest = agent.reread().context(summon_i18n::text!("r50c89c40c6"))?;
            anyhow::ensure!(
                epoch.matches_current(&latest)
                    && latest.is_routable()
                    && agent_owner(&latest).as_deref() == Some(record.owner_entry.as_str()),
                summon_i18n::format!("r509c62b2ce")
            );
            let card_body = if failed.stage == Stage::DraftReady {
                draft_card(&failed)?
            } else {
                outcome_card(
                    &failed,
                    failed
                        .outcome
                        .as_deref()
                        .unwrap_or(summon_i18n::text!("r1d2a47d85c")),
                )
            };
            fc.update_card(&card.message_id, &card_body)?;
            return Err(error);
        }
    };
    let current = agent.reread().context(summon_i18n::text!("r50c89c40c6"))?;
    anyhow::ensure!(
        epoch.matches_current(&current)
            && current.is_routable()
            && agent_owner(&current).as_deref() == Some(record.owner_entry.as_str()),
        summon_i18n::format!("r1948491c4c")
    );
    let frozen = record
        .context
        .as_ref()
        .context(summon_i18n::text!("r2563ec0f7d"))?;
    if context.fingerprint != frozen.fingerprint
        || context.document_modified_time != frozen.document_modified_time
    {
        store.update(id, |r| {
            if r.stage == Stage::DraftReady {
                r.last_error = None;
                r.close(summon_i18n::text!("re823467064"));
            }
            Ok(())
        })?;
        fc.update_card(
            &card.message_id,
            &outcome_card(&record, summon_i18n::text!("r981278b243")),
        )?;
        return Ok(());
    }
    anyhow::ensure!(context.reply_allowed, summon_i18n::format!("r0a80e3ceab"));
    anyhow::ensure!(enabled(&current), summon_i18n::format!("ra116fc6e49"));
    let draft = store.claim_reply(
        id,
        crate::runtime::document_comments::Approval {
            agent: &agent.name,
            scope,
            chat: &card.chat_id,
            card: &card.message_id,
            operator: &card.operator_open_id,
            now_ms: now_ms(),
        },
        &context,
    )?;
    // Sending 已 durable。请求结果不明确时保留它，任何重放都不会再 POST。
    match fc.comment_reply(&record.notice, &draft) {
        Ok(reply_id) => {
            let verified = fc
                .verify_comment_reply(&record.notice, &reply_id, &record.bot_open_id, &draft)
                .is_ok();
            store.update(id, |r| {
                r.stage = Stage::Replied;
                r.reply_id = Some(reply_id);
                r.outcome = Some(
                    if verified {
                        summon_i18n::text!("r807abafa51")
                    } else {
                        summon_i18n::text!("r13aa2e5fd1")
                    }
                    .into(),
                );
                Ok(())
            })?;
            fc.update_card(
                &card.message_id,
                &outcome_card(
                    &record,
                    if verified {
                        summon_i18n::text!("rcde9e607d8")
                    } else {
                        summon_i18n::text!("r06fe280edc")
                    },
                ),
            )?;
        }
        Err(error) => {
            let _ = store.update(id, |r| {
                r.outcome = Some(summon_i18n::text!("r57ee8db689").into());
                Ok(())
            });
            let _ = fc.update_card(
                &card.message_id,
                &outcome_card(&record, summon_i18n::text!("r1d2a47d85c")),
            );
            return Err(error);
        }
    }
    Ok(())
}

fn limited(text: &str, chars: usize) -> String {
    let mut result: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        result.push_str(summon_i18n::text!("r5517923d5c"));
    }
    summon_core::feishu::escape_lark_md(&result)
}

fn base_card(
    record: &Record,
    header: &str,
    text: &str,
    draft: Option<&str>,
    buttons: Vec<Value>,
) -> Result<String> {
    let context = record
        .context
        .as_ref()
        .context(summon_i18n::text!("r4826c9cae1"))?;
    let mut elements = vec![
        json!({"tag":"markdown", "content": format!("**{}**\n{}\n\n{}",
        limited(&context.title, 150), limited(&context.thread, 1500), text)}),
    ];
    if let Some(draft) = draft {
        elements.push(json!({"tag":"div", "text":{"tag":"plain_text", "content":draft}}));
    }
    elements.push(json!({"tag":"button", "text":{"tag":"plain_text","content":summon_i18n::text!("r2e807db7ce")}, "type":"default", "url":context.url}));
    if !buttons.is_empty() {
        elements.push(json!({"tag":"column_set", "flex_mode":"none", "columns": buttons.into_iter().map(|button| json!({"tag":"column", "width":"auto", "elements":[button]})).collect::<Vec<_>>()}));
    }
    Ok(
        json!({"schema":"2.0", "header":{"title":{"tag":"plain_text","content":header}},
        "body":{"elements":elements}})
        .to_string(),
    )
}

fn draft_card(record: &Record) -> Result<String> {
    let draft = record
        .draft
        .as_deref()
        .context(summon_i18n::text!("re4221ed441"))?;
    let mut text = summon_i18n::text!("rbe0887acd4").to_string();
    if let Some(error) = &record.last_error {
        text.push_str(&summon_i18n::format!("r5407b4a0ac", limited(error, 300)));
    }
    let button = |label, action, kind| {
        json!({"tag":"button", "text":{"tag":"plain_text","content":label}, "type":kind,
        "value":{"action":action, "comment":record.id}})
    };
    let mut buttons = Vec::new();
    if record.context.as_ref().is_some_and(|c| c.reply_allowed) {
        buttons.push(button(
            summon_i18n::text!("r82b2daeb65"),
            "doc_comment_approve",
            "primary",
        ));
    } else {
        text.push_str(summon_i18n::text!("r81574b60c0"));
    }
    buttons.push(button(
        summon_i18n::text!("rd632226978"),
        "doc_comment_dismiss",
        "default",
    ));
    base_card(
        record,
        summon_i18n::text!("r8b85704b7a"),
        &text,
        Some(draft),
        buttons,
    )
}

fn outcome_card(record: &Record, outcome: &str) -> String {
    base_card(
        record,
        summon_i18n::text!("r8b85704b7a"),
        outcome,
        None,
        Vec::new(),
    )
    .unwrap_or_else(|_| summon_core::feishu::card_markdown_v2(outcome))
}

/// 失败通知卡。已核验归属的带文档标题、评论串与原文档入口;核验前就失败的只有原因。
fn failure_card(record: &Record, text: &str) -> String {
    base_card(
        record,
        summon_i18n::text!("r405aad90f1"),
        text,
        None,
        Vec::new(),
    )
    .unwrap_or_else(|_| summon_core::feishu::card_markdown_v2(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::document_comments::CommentNotice;
    use std::sync::Mutex;

    /// 结构守护:起草之前先过准入闸,换代时才不会有起草进程在「确认空闲」之后开跑、随即被停机
    /// 打断。闸本身的语义由 `runtime::admission` 的测试证明。
    #[test]
    fn drafting_passes_the_admission_gate_before_claiming_the_draft() {
        let src = include_str!("document_comments.rs")
            .rsplit_once("\n#[cfg(test)]\nmod tests {")
            .expect("本文件必须有顶格的 mod tests")
            .0;
        let drafting = src.find("Stage::Drafting => {").expect("起草阶段");
        let body = &src[drafting..];
        let gate = body
            .find("crate::runtime::admission::GATE.admit(")
            .expect("准入闸");
        let claim = body.find("store.claim_draft(").expect("认领起草");
        assert!(gate < claim, "过闸必须在认领起草之前");
        assert!(body[gate..claim].contains("crate::runtime::admission::Kind::Task"));
        assert!(body[gate..claim].contains("else {\n                return Ok(());"));
    }

    struct CommentServer {
        stop: Arc<AtomicBool>,
        posts: Arc<Mutex<Vec<Value>>>,
        notices: Arc<Mutex<Vec<Value>>>,
        changed: Arc<AtomicBool>,
        ownership: Arc<std::sync::atomic::AtomicUsize>,
        chat_available: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for CommentServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn fixture(
        unknown_send: bool,
    ) -> (
        Agent,
        tempfile::TempDir,
        Store,
        FeishuClient,
        CommentServer,
        String,
    ) {
        let (agent, dir) = crate::test_support::agent_dir("claude", "messaging:\n  document_comments_enabled: true\nfeishu:\n  app_id_env: COMMENT_TEST_APP\n  app_secret_env: COMMENT_TEST_SECRET\n  allowed_users: [ou_owner]\n");
        std::fs::write(
            agent.dir.join(".env"),
            "COMMENT_TEST_APP=cli_test\nCOMMENT_TEST_SECRET=test_secret\n",
        )
        .unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let client = FeishuClient::with_test_base(format!(
            "http://{}",
            server.server_addr().to_ip().unwrap()
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let posts = Arc::new(Mutex::new(Vec::<Value>::new()));
        let notices = Arc::new(Mutex::new(Vec::<Value>::new()));
        let worker_notices = notices.clone();
        let changed = Arc::new(AtomicBool::new(false));
        let ownership = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_ownership = ownership.clone();
        let chat_available = Arc::new(AtomicBool::new(true));
        let worker_chat_available = chat_available.clone();
        let (worker_stop, worker_posts, worker_changed) =
            (stop.clone(), posts.clone(), changed.clone());
        let worker = std::thread::spawn(move || {
            let reply = |id: &str, user: &str, text: &str| json!({"reply_id":id,"user_id":user,"content":{"elements":[{"type":"text_run","text_run":{"text":text}}]}});
            while !worker_stop.load(Ordering::Relaxed) {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(50)).unwrap()
                else {
                    continue;
                };
                let url = request.url().to_string();
                let mut status = 200;
                let response = if url.contains("/auth/") {
                    json!({"tenant_access_token":"token","expire":7200})
                } else if url.contains("/bot/v3/info") {
                    json!({"code":0,"bot":{"open_id":"ou_bot","app_name":"test"}})
                } else if url.contains("/contact/") {
                    json!({"code":0,"data":{"user":{"union_id":"on_owner"}}})
                } else if url.contains("/metas/") && worker_ownership.load(Ordering::Relaxed) == 4 {
                    json!({"code":99991672})
                } else if url.contains("/metas/") {
                    json!({"code":0,"data":{"metas":[{"doc_token":"doc_test","doc_type":"docx","owner_id":match worker_ownership.load(Ordering::Relaxed) { 0 => "ou_bot", 2 => "ou_other", _ => "ou_owner" },"title":"Title","url":"https://example.feishu.cn/docx/doc_test","latest_modify_time":if worker_changed.load(Ordering::Relaxed) {"101"} else {"100"}}]}})
                } else if url.contains("/wiki/v2/spaces/get_node") {
                    json!({"code":0,"data":{"node":{"obj_token":"doc_test","obj_type":"docx","creator":if worker_ownership.load(Ordering::Relaxed) == 3 {"ou_other"} else {"ou_bot"},"owner":"ou_owner"}}})
                } else if url.contains("/comments/batch_query") {
                    json!({"code":0,"data":{"items":[{"comment_id":"100","is_whole":false,"is_solved":false,"quote":"Quote"}]}})
                } else if url.contains("/raw_content") {
                    json!({"code":0,"data":{"content":"Body"}})
                } else if url.contains("/replies?") && request.method() == &tiny_http::Method::Post
                {
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    worker_posts
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&body).unwrap());
                    if unknown_send {
                        status = 500;
                        json!({"code":1})
                    } else {
                        json!({"code":0,"data":{"reply_id":"300"}})
                    }
                } else if url.contains("/replies?") {
                    let mut items = vec![reply("200", "ou_reader", "Question")];
                    if let Some(post) = worker_posts.lock().unwrap().first() {
                        items.push(reply(
                            "300",
                            "ou_bot",
                            post["content"]["elements"][0]["text_run"]["text"]
                                .as_str()
                                .unwrap(),
                        ));
                    }
                    json!({"code":0,"data":{"items":items,"has_more":false}})
                } else if url.contains("/im/v1/messages")
                    && request.method() == &tiny_http::Method::Post
                {
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    worker_notices
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&body).unwrap());
                    json!({"code":0,"data":{"message_id":"om_failure"}})
                } else if url.contains("/im/v1/messages/mget?")
                    && request.method() == &tiny_http::Method::Get
                {
                    json!({"code":0,"data":{"items":if worker_chat_available.load(Ordering::Relaxed) { vec![json!({"message_id":"om_failure","chat_id":"oc_owner"})] } else { vec![] }}})
                } else if request.method() == &tiny_http::Method::Patch {
                    json!({"code":0})
                } else {
                    panic!("unexpected request {url}")
                };
                request
                    .respond(
                        tiny_http::Response::from_string(response.to_string())
                            .with_status_code(status),
                    )
                    .unwrap();
            }
        });
        let mock = CommentServer {
            notices,
            ownership,
            chat_available,
            stop,
            posts,
            changed,
            worker: Some(worker),
        };
        let notice = CommentNotice {
            app_id: "cli_test".into(),
            event_id: "evt".into(),
            file_token: "doc_test".into(),
            file_type: "docx".into(),
            comment_id: "100".into(),
            reply_id: "200".into(),
            sender_open_id: "ou_reader".into(),
            recipient_open_id: "ou_bot".into(),
        };
        let context = client
            .owned_comment_context(&notice, "ou_bot", "ou_owner")
            .unwrap();
        let store = Store::new(dir.path().join("comments"));
        let id = store.receive(notice, now_ms(), true).unwrap();
        let epoch = super::super::FeishuEpochIdentity::capture(&agent);
        store
            .update(&id, |r| {
                r.stage = Stage::Drafting;
                r.agent_name = agent.name.clone();
                r.delivery_scope = epoch.storage_scope().key.clone();
                r.epoch = epoch.fingerprint();
                r.owner_entry = "ou_owner".into();
                r.owner_open_id = "ou_owner".into();
                r.bot_open_id = "ou_bot".into();
                r.owner_chat_id = Some("oc_owner".into());
                r.context = Some(context);
                Ok(())
            })
            .unwrap();
        store
            .set_draft(
                &id,
                0,
                "[确认](https://example.com/private-payload)\n`代码` \\ <at id=all>\n完整正文",
                now_ms(),
            )
            .unwrap();
        store
            .update(&id, |r| {
                r.stage = Stage::DraftReady;
                r.approval_message_id = Some("om_approval".into());
                Ok(())
            })
            .unwrap();
        (agent, dir, store, client, mock, id)
    }

    fn callback(id: &str) -> CardActionMsg {
        CardActionMsg {
            chat_id: "oc_owner".into(),
            message_id: "om_approval".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "evt_card".into(),
            value: json!({"action":"doc_comment_approve","comment":id,"draft":"callback replacement","file_token":"another_document"}),
            wal_id: None,
            wal_replayed: false,
        }
    }

    #[test]
    fn document_reader_revalidates_feature_owner_identity_and_frozen_version() {
        let (agent, _dir, store, client, mock, id) = fixture(false);
        let record = store.get(&id).unwrap();
        let check = |record: &Record| {
            super::super::document_comment_draft::validate_document_read(&agent, record, &client)
        };
        check(&record).unwrap();
        configure_comments(&agent, false);
        assert!(check(&record).is_err());
        configure_comments(&agent, true);
        for stale in ["owner", "identity", "version", "comment"] {
            let mut changed = record.clone();
            match stale {
                "owner" => changed.owner_entry = "ou_another".into(),
                "identity" => changed.epoch = "another".into(),
                "version" => {
                    changed.context.as_mut().unwrap().document_modified_time = "old".into()
                }
                _ => changed.context.as_mut().unwrap().fingerprint = "old".into(),
            }
            assert!(check(&changed).is_err(), "{stale}");
        }
        mock.ownership.store(3, Ordering::Relaxed);
        assert!(check(&record).is_err());
        assert!(mock.notices.lock().unwrap().is_empty());
        assert!(mock.posts.lock().unwrap().is_empty());
    }

    fn configure_comments(agent: &Agent, active: bool) {
        let mut config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap()).unwrap();
        config["messaging"]["document_comments_enabled"] = serde_yaml::Value::Bool(active);
        std::fs::write(
            agent.manifest_path(),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn interrupted_or_failed_drafts_do_not_start_another_backend() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        let script = dir.path().join("backend");
        crate::test_support::write_test_executable(&script, "#!/bin/sh\nprintf called > \"$(dirname \"$0\")/called\"\nprintf '%s' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"draft\"}'\n");
        let mut config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap()).unwrap();
        config["runtime"]["cli_cmd"] = serde_yaml::Value::String(script.display().to_string());
        std::fs::write(
            agent.manifest_path(),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();
        let agent = agent.reread().unwrap();
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        for (started, failures) in [(true, 0), (false, 1)] {
            store
                .update(&id, |r| {
                    r.stage = Stage::Drafting;
                    r.draft = None;
                    r.draft_started = started;
                    r.failures = failures;
                    Ok(())
                })
                .unwrap();
            let restarted = Store::new(dir.path().join("comments"));
            assert!(advance(
                &restarted,
                &restarted.get(&id).unwrap(),
                &delivery,
                &AtomicBool::new(false)
            )
            .is_err());
            assert!(!dir.path().join("called").exists());
        }
        assert!(mock.notices.lock().unwrap().is_empty());
    }

    #[test]
    fn first_processing_failure_closes_the_task_without_retry() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        store
            .update(&id, |r| {
                r.stage = Stage::Drafting;
                r.draft = None;
                r.draft_started = true;
                Ok(())
            })
            .unwrap();
        assert!(advance_once(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false)
        )
        .is_err());
        let restarted = Store::new(dir.path().join("comments"));
        let failed = restarted.get(&id).unwrap();
        assert_eq!(failed.stage, Stage::Closed);
        assert_eq!(failed.failures, 1);
        assert_eq!(failed.retry_after_ms, 0);
        assert!(failed.last_error.is_some());
        assert!(failed.notify_failure);
        assert!(mock.notices.lock().unwrap().is_empty());
        // 下一轮处理只发失败通知,不再起草
        let pending = restarted.inbox().unwrap().0;
        assert_eq!(pending.len(), 1);
        advance_once(&restarted, &pending[0], &delivery, &AtomicBool::new(false)).unwrap();
        let notices = mock.notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 1);
        let card = notices[0]["content"].as_str().unwrap();
        assert!(card.contains("文档评论任务处理失败"), "{card}");
        assert!(
            card.contains("文档评论处理失败"),
            "失败卡有自己的标题:{card}"
        );
        assert!(!card.contains("文档新评论"), "{card}");
        assert!(
            card.contains("https://example.feishu.cn/docx/doc_test"),
            "已核验归属的带原文档入口:{card}"
        );
        assert!(restarted.inbox().unwrap().0.is_empty());
        advance_once(
            &restarted,
            &restarted.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(mock.notices.lock().unwrap().len(), 1, "失败通知只发一次");
    }

    fn received_record(store: &Store, id: &str) -> Record {
        store
            .update(id, |r| {
                r.stage = Stage::Received;
                r.context = None;
                r.draft = None;
                r.drafted_ms = None;
                r.approval_message_id = None;
                r.owner_chat_id = None;
                Ok(())
            })
            .unwrap();
        store.get(id).unwrap()
    }

    #[test]
    fn metadata_permission_error_notifies_the_owner_without_comment_text() {
        let (agent, _dir, store, client, mock, id) = fixture(false);
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        // 元数据接口报权限错误:故障,要通知
        mock.ownership.store(4, Ordering::Relaxed);
        let record = received_record(&store, &id);
        assert!(advance_once(&store, &record, &delivery, &AtomicBool::new(false)).is_err());
        let failed = store.get(&id).unwrap();
        assert!(failed.context.is_none());
        assert!(failed.needs_failure_notice());
        advance_once(&store, &failed, &delivery, &AtomicBool::new(false)).unwrap();
        let notices = mock.notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 1);
        let card = notices[0]["content"].as_str().unwrap();
        assert!(card.contains("文档评论任务处理失败"), "{card}");
        assert!(!card.contains("Quote"), "未核验归属不带评论内容:{card}");
    }

    #[test]
    fn documents_outside_the_scope_are_rejected_without_a_failure_card() {
        // 2:当前所有者是别人;3:转交文档的创建者不是本 bot
        for ownership in [2, 3] {
            let (agent, _dir, store, client, mock, id) = fixture(false);
            let delivery = OutboxDrainAgent {
                epoch: super::super::FeishuEpochIdentity::capture(&agent),
                agent,
                client: Arc::new(client),
            };
            mock.ownership.store(ownership, Ordering::Relaxed);
            let record = received_record(&store, &id);
            assert!(advance_once(&store, &record, &delivery, &AtomicBool::new(false)).is_err());
            let rejected = store.get(&id).unwrap();
            assert_eq!(rejected.stage, Stage::Closed);
            assert!(rejected.last_error.is_some(), "原因仍在任务状态里可见");
            assert!(!rejected.notify_failure, "ownership={ownership}");
            assert!(!rejected.needs_failure_notice());
            assert!(mock.notices.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn a_pending_failure_notice_is_dropped_once_comments_are_disabled() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        store
            .update(&id, |r| {
                r.draft = None;
                r.failures = 1;
                r.notify_failure = true;
                r.close("评论处理未完成，请打开原文档处理");
                Ok(())
            })
            .unwrap();
        assert!(store.get(&id).unwrap().needs_failure_notice());
        let manifest = agent.manifest_path();
        let mut config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
        config["messaging"]["document_comments_enabled"] = serde_yaml::Value::Bool(false);
        std::fs::write(&manifest, serde_yaml::to_string(&config).unwrap()).unwrap();
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent: agent.reread().unwrap(),
            client: Arc::new(client),
        };
        advance_once(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        config["messaging"]["document_comments_enabled"] = serde_yaml::Value::Bool(true);
        std::fs::write(&manifest, serde_yaml::to_string(&config).unwrap()).unwrap();
        let restarted = Store::new(dir.path().join("comments"));
        let dropped = restarted.get(&id).unwrap();
        assert!(!dropped.needs_failure_notice());
        assert!(restarted.inbox().unwrap().0.is_empty());
        assert!(mock.notices.lock().unwrap().is_empty());
    }

    #[test]
    fn failure_while_comments_are_disabled_is_not_reported_after_reenabling() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        let script = dir.path().join("backend");
        crate::test_support::write_test_executable(
            &script,
            "#!/bin/sh\nroot=\"${0%/*}\"\nprintf x >> \"$root/called\"\nexec sleep 30\n",
        );
        let mut config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap()).unwrap();
        config["runtime"]["cli_cmd"] = serde_yaml::Value::String(script.display().to_string());
        std::fs::write(
            agent.manifest_path(),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();
        let agent = agent.reread().unwrap();
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent: agent.clone(),
            client: Arc::new(client),
        };
        store
            .update(&id, |r| {
                r.stage = Stage::Drafting;
                r.draft = None;
                Ok(())
            })
            .unwrap();
        // 起草进行中 owner 关掉功能:起草被取消,这次失败不登记通知
        let disabler = {
            let marker = dir.path().join("called");
            let manifest = agent.manifest_path();
            let mut config = config.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !marker.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(marker.exists());
                config["messaging"]["document_comments_enabled"] = serde_yaml::Value::Bool(false);
                std::fs::write(manifest, serde_yaml::to_string(&config).unwrap()).unwrap();
            })
        };
        assert!(advance_once(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false)
        )
        .is_err());
        disabler.join().unwrap();
        let failed = store.get(&id).unwrap();
        assert_eq!(failed.stage, Stage::Closed);
        assert!(!failed.notify_failure);
        assert!(!failed.needs_failure_notice());
        assert!(mock.notices.lock().unwrap().is_empty());
    }

    #[test]
    fn timed_out_draft_is_terminal_and_the_next_task_can_complete() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        let script = dir.path().join("backend");
        crate::test_support::write_test_executable(
            &script,
            "#!/bin/sh\nif test \"${1-}\" = --warm-up; then exit 0; fi\nroot=\"${0%/*}\"\nprintf x >> \"$root/called\"\nexec sleep 30\n",
        );
        // macOS executable assessment must finish before exercising the one-second deadline.
        assert!(std::process::Command::new(&script)
            .arg("--warm-up")
            .status()
            .unwrap()
            .success());
        assert!(!dir.path().join("called").exists());
        let mut config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap()).unwrap();
        config["runtime"]["cli_cmd"] = serde_yaml::Value::String(script.display().to_string());
        config["messaging"]["document_comment_timeout_secs"] = serde_yaml::Value::Number(1.into());
        std::fs::write(
            agent.manifest_path(),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();
        let agent = agent.reread().unwrap();
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        store
            .update(&id, |r| {
                r.stage = Stage::Drafting;
                r.draft = None;
                Ok(())
            })
            .unwrap();
        let record = store.get(&id).unwrap();
        assert!(advance_once(&store, &record, &delivery, &AtomicBool::new(false)).is_err());
        let failed = store.get(&id).unwrap();
        assert_eq!(failed.stage, Stage::Closed);
        assert!(failed
            .last_error
            .as_deref()
            .unwrap()
            .contains("超过1秒时限"));
        assert!(failed.draft_process.is_none());
        assert!(failed.needs_failure_notice());
        advance_once(&store, &failed, &delivery, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("called")).unwrap(),
            "x"
        );
        assert_eq!(mock.notices.lock().unwrap().len(), 1, "超时通知 owner 一次");
        mock.notices.lock().unwrap().clear();
        let mut notice = record.notice.clone();
        notice.reply_id = "201".into();
        let next = store.receive(notice, now_ms(), true).unwrap();
        store
            .update(&next, |r| {
                let notice = r.notice.clone();
                *r = record.clone();
                r.id = next.clone();
                r.notice = notice;
                r.draft_started = false;
                Ok(())
            })
            .unwrap();
        crate::test_support::write_test_executable(&script, "#!/bin/sh\nprintf '%s' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"next draft\"}'\n");
        assert!(std::process::Command::new(&script)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success());
        advance_once(
            &store,
            &store.get(&next).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            store.get(&next).unwrap().draft.as_deref(),
            Some("next draft")
        );
        assert!(mock.notices.lock().unwrap().is_empty());
    }

    #[test]
    fn stopping_an_active_comment_reaps_its_process_without_a_retry_or_card() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        let script = dir.path().join("backend");
        crate::test_support::write_test_executable(&script, "#!/bin/sh\nroot=\"${0%/*}\"\nprintf x >> \"$root/called\"\necho $$ > \"$root/pid\"\nsleep 30\n");
        let mut config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap()).unwrap();
        config["runtime"]["cli_cmd"] = serde_yaml::Value::String(script.display().to_string());
        std::fs::write(
            agent.manifest_path(),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();
        let agent = agent.reread().unwrap();
        store
            .update(&id, |r| {
                r.stage = Stage::Drafting;
                r.draft = None;
                Ok(())
            })
            .unwrap();
        let stopper = {
            let store = store.clone();
            let id = id.clone();
            let agent = agent.clone();
            let marker = dir.path().join("pid");
            std::thread::spawn(move || {
                let until = std::time::Instant::now() + Duration::from_secs(5);
                while !marker.exists() && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(marker.exists());
                store.stop_draft(&id, &agent).unwrap();
            })
        };
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        assert!(advance_once(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false)
        )
        .is_err());
        stopper.join().unwrap();
        let record = store.get(&id).unwrap();
        assert_eq!(record.stage, Stage::Closed);
        assert_eq!(record.failures, 0);
        assert!(record.draft_process.is_none() && record.draft.is_none());
        assert!(store.inbox().unwrap().0.is_empty());
        advance_once(&store, &record, &delivery, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("called")).unwrap(),
            "x"
        );
        let pid: i32 = std::fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!crate::runtime::daemon::pid_is_alive(pid as u32));
        assert!(mock.notices.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_and_cancelled_backends_are_started_once_across_restart() {
        for cancel in [false, true] {
            let (agent, dir, store, client, mock, id) = fixture(false);
            let script = dir.path().join("backend");
            crate::test_support::write_test_executable(&script, "#!/bin/sh\nroot=\"${0%/*}\"\nprintf x >> \"$root/called\"\nif test -e \"$root/cancel\"; then sleep 30; else exit 1; fi\n");
            if cancel {
                std::fs::write(dir.path().join("cancel"), "").unwrap();
            }
            let mut config: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap())
                    .unwrap();
            config["runtime"]["cli_cmd"] = serde_yaml::Value::String(script.display().to_string());
            std::fs::write(
                agent.manifest_path(),
                serde_yaml::to_string(&config).unwrap(),
            )
            .unwrap();
            let agent = agent.reread().unwrap();
            let delivery = OutboxDrainAgent {
                epoch: super::super::FeishuEpochIdentity::capture(&agent),
                agent,
                client: Arc::new(client),
            };
            store
                .update(&id, |r| {
                    r.stage = Stage::Drafting;
                    r.draft = None;
                    Ok(())
                })
                .unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let canceller = cancel.then(|| {
                let stop = stop.clone();
                let marker = dir.path().join("called");
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !marker.exists() && std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert!(marker.exists());
                    stop.store(true, Ordering::Relaxed);
                })
            });
            assert!(advance_once(&store, &store.get(&id).unwrap(), &delivery, &stop).is_err());
            if let Some(canceller) = canceller {
                canceller.join().unwrap();
            }
            let restarted = Store::new(dir.path().join("comments"));
            let failed = restarted.get(&id).unwrap();
            assert!(failed.draft_started);
            assert_eq!(failed.failures, 1);
            assert_eq!(failed.stage, Stage::Closed);
            assert!(
                failed.needs_failure_notice(),
                "停机打断的起草也要告诉 owner"
            );
            advance_once(&restarted, &failed, &delivery, &AtomicBool::new(false)).unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.path().join("called")).unwrap(),
                "x"
            );
            assert_eq!(mock.notices.lock().unwrap().len(), 1);
            assert!(restarted.inbox().unwrap().0.is_empty());
            assert!(mock.posts.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn interrupted_failure_notice_is_not_retried() {
        let (_agent, _dir, store, _client, _mock, id) = fixture(false);
        store
            .update(&id, |r| {
                r.close("草稿送审失败");
                r.failures = 1;
                r.notify_failure = true;
                Ok(())
            })
            .unwrap();
        assert!(store.get(&id).unwrap().needs_failure_notice());
        store
            .update(&id, |r| {
                r.failure_notice_attempts = 1;
                Ok(())
            })
            .unwrap();
        assert!(store.inbox().unwrap().0.is_empty());
    }

    #[test]
    fn disabled_comments_pause_jobs_and_resume_on_enable() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        let script = dir.path().join("backend");
        crate::test_support::write_test_executable(&script, "#!/bin/sh\nprintf called > \"$(dirname \"$0\")/called\"\nprintf '%s' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"draft\"}'\n");
        let mut config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap()).unwrap();
        config["runtime"]["cli_cmd"] = serde_yaml::Value::String(script.display().to_string());
        std::fs::write(
            agent.manifest_path(),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();
        let agent = agent.reread().unwrap();
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent: agent.clone(),
            client: Arc::new(client),
        };
        configure_comments(&agent, false);
        for stage in [Stage::Prepared, Stage::Drafting] {
            store
                .update(&id, |r| {
                    r.stage = stage.clone();
                    r.draft = None;
                    r.drafted_ms = None;
                    Ok(())
                })
                .unwrap();
            advance(
                &store,
                &store.get(&id).unwrap(),
                &delivery,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(store.get(&id).unwrap().stage, stage);
            assert!(store.get(&id).unwrap().draft.is_none());
        }
        assert!(!dir.path().join("called").exists());
        assert!(mock.notices.lock().unwrap().is_empty());
        assert!(mock.posts.lock().unwrap().is_empty());
        configure_comments(&agent, true);
        advance(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(store.get(&id).unwrap().draft.as_deref(), Some("draft"));
        assert!(dir.path().join("called").exists());
        assert!(mock.notices.lock().unwrap().is_empty());
    }

    #[test]
    fn comments_pending_when_disabled_do_not_build_a_draft_backlog() {
        let (agent, _dir, store, client, mock, id) = fixture(false);
        store
            .update(&id, |r| {
                r.stage = Stage::Received;
                r.draft = None;
                r.context = None;
                r.drafted_ms = None;
                r.approval_message_id = None;
                Ok(())
            })
            .unwrap();
        configure_comments(&agent, false);
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent: agent.clone(),
            client: Arc::new(client),
        };
        advance(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(store.get(&id).unwrap().stage, Stage::Closed);
        assert!(store.inbox().unwrap().0.is_empty());
        configure_comments(&agent, true);
        advance(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(mock.notices.lock().unwrap().is_empty());
        assert!(mock.posts.lock().unwrap().is_empty());
    }

    #[test]
    fn receipt_while_disabled_is_acknowledged_without_a_stub() {
        let (agent, _dir, store, _client, mock, id) = fixture(false);
        let (tx, _rx) = std::sync::mpsc::channel();
        let core = crate::runtime::core::Core::new(
            std::collections::HashMap::new(),
            std::collections::HashMap::from([(agent.name.clone(), tx)]),
            vec![agent.clone()],
        );
        let mut notice = store.get(&id).unwrap().notice;
        notice.reply_id = "new-reply".into();
        configure_comments(&agent, false);
        let error = receive_notice(&store, &core, notice.clone(), now_ms()).unwrap_err();
        assert!(
            crate::runtime::document_comments::is_rejected(&error),
            "{error:#}"
        );
        assert!(store.get(&notice.key()).is_err());
        assert_eq!(
            store
                .all()
                .unwrap()
                .iter()
                .map(|r| &r.id)
                .collect::<Vec<_>>(),
            [&id]
        );
        assert!(mock.notices.lock().unwrap().is_empty());
        assert!(mock.posts.lock().unwrap().is_empty());

        configure_comments(&agent, true);
        let new_id = receive_notice(&store, &core, notice, now_ms()).unwrap();
        assert_eq!(store.get(&new_id).unwrap().stage, Stage::Received);
    }

    #[test]
    fn enabled_receipt_keeps_its_eligibility_when_replayed_while_disabled() {
        let (agent, _dir, store, _client, _mock, id) = fixture(false);
        let (tx, _rx) = std::sync::mpsc::channel();
        let core = crate::runtime::core::Core::new(
            std::collections::HashMap::new(),
            std::collections::HashMap::from([(agent.name.clone(), tx)]),
            vec![agent.clone()],
        );
        let mut notice = store.get(&id).unwrap().notice;
        notice.reply_id = "enabled-reply".into();
        let new_id = receive_notice(&store, &core, notice.clone(), now_ms()).unwrap();
        assert_eq!(store.get(&new_id).unwrap().stage, Stage::Received);
        configure_comments(&agent, false);
        assert_eq!(
            receive_notice(&store, &core, notice, now_ms()).unwrap(),
            new_id
        );
        assert_eq!(store.get(&new_id).unwrap().stage, Stage::Received);
    }

    #[test]
    fn disabled_comments_preserve_approval_until_enabled() {
        let (agent, _dir, store, client, mock, id) = fixture(false);
        let epoch = super::super::FeishuEpochIdentity::capture(&agent);
        configure_comments(&agent, false);
        assert!(handle_card_with_store(
            &store,
            &agent,
            &client,
            &epoch.storage_scope().key,
            &epoch,
            &callback(&id)
        )
        .is_err());
        let record = store.get(&id).unwrap();
        assert_eq!(record.stage, Stage::DraftReady);
        assert!(record.last_error.as_deref().unwrap().contains("功能已关闭"));
        assert!(mock.posts.lock().unwrap().is_empty());
        configure_comments(&agent, true);
        handle_card_with_store(
            &store,
            &agent,
            &client,
            &epoch.storage_scope().key,
            &epoch,
            &callback(&id),
        )
        .unwrap();
        assert_eq!(mock.posts.lock().unwrap().len(), 1);
        assert_eq!(store.get(&id).unwrap().stage, Stage::Replied);
    }

    #[test]
    fn prepared_comment_enters_drafting_without_a_receipt_card() {
        let (agent, _dir, store, client, mock, id) = fixture(false);
        store
            .update(&id, |r| {
                r.stage = Stage::Prepared;
                r.draft = None;
                r.drafted_ms = None;
                r.notice_message_id = None;
                r.approval_message_id = None;
                r.owner_chat_id = None;
                Ok(())
            })
            .unwrap();
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        advance(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(store.get(&id).unwrap().stage, Stage::Drafting);
        assert!(mock.notices.lock().unwrap().is_empty());
        assert!(mock.posts.lock().unwrap().is_empty());
    }

    #[test]
    fn completed_draft_card_binds_its_chat_without_a_receipt_card() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        store
            .update(&id, |r| {
                r.stage = Stage::Prepared;
                r.notice_message_id = None;
                r.approval_message_id = None;
                r.owner_chat_id = None;
                Ok(())
            })
            .unwrap();
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        advance(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        let restarted = Store::new(dir.path().join("comments"));
        let ready = restarted.get(&id).unwrap();
        assert_eq!(ready.stage, Stage::DraftReady);
        assert_eq!(ready.owner_chat_id.as_deref(), Some("oc_owner"));
        assert_eq!(ready.approval_message_id.as_deref(), Some("om_failure"));
        assert!(ready.notice_message_id.is_none());
        advance(&restarted, &ready, &delivery, &AtomicBool::new(false)).unwrap();
        let cards = mock.notices.lock().unwrap();
        assert_eq!(cards.len(), 1);
        let text = cards[0]["content"].as_str().unwrap();
        assert!(text.contains("完整正文") && text.contains("doc_comment_approve"));
        assert!(mock.posts.lock().unwrap().is_empty());
    }

    #[test]
    fn draft_delivery_recovers_chat_lookup_without_sending_another_card() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        store
            .update(&id, |r| {
                r.stage = Stage::Prepared;
                r.notice_message_id = None;
                r.approval_message_id = None;
                r.owner_chat_id = None;
                Ok(())
            })
            .unwrap();
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        mock.chat_available.store(false, Ordering::Relaxed);
        assert!(advance(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false)
        )
        .is_err());
        let restarted = Store::new(dir.path().join("comments"));
        let sent = restarted.get(&id).unwrap();
        assert_eq!(sent.stage, Stage::Prepared);
        assert_eq!(sent.approval_message_id.as_deref(), Some("om_failure"));
        assert!(sent.owner_chat_id.is_none());
        mock.chat_available.store(true, Ordering::Relaxed);
        advance(&restarted, &sent, &delivery, &AtomicBool::new(false)).unwrap();
        let ready = restarted.get(&id).unwrap();
        assert_eq!(ready.stage, Stage::DraftReady);
        assert_eq!(ready.owner_chat_id.as_deref(), Some("oc_owner"));
        assert_eq!(mock.notices.lock().unwrap().len(), 1);
    }

    /// 有草稿之后才失败(审批卡发出去了,读不回所在会话)也登记通知,下一轮发一张失败卡。
    #[test]
    fn failure_after_the_draft_notifies_the_owner_once() {
        let (agent, _dir, store, client, mock, id) = fixture(false);
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        store
            .update(&id, |r| {
                r.stage = Stage::Prepared;
                r.approval_message_id = None;
                r.owner_chat_id = None;
                Ok(())
            })
            .unwrap();
        mock.chat_available.store(false, Ordering::Relaxed);
        let record = store.get(&id).unwrap();
        assert!(advance_once(&store, &record, &delivery, &AtomicBool::new(false)).is_err());
        let failed = store.get(&id).unwrap();
        assert!(failed.draft.is_some());
        assert!(failed.notify_failure);
        assert_eq!(mock.notices.lock().unwrap().len(), 1, "审批卡已发出");
        advance_once(&store, &failed, &delivery, &AtomicBool::new(false)).unwrap();
        advance_once(
            &store,
            &store.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        let notices = mock.notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 2, "再加恰好一张失败卡");
        assert!(notices[1]["content"]
            .as_str()
            .unwrap()
            .contains("文档评论处理失败"));
    }

    #[test]
    fn drafted_failures_without_a_notice_intent_send_no_card() {
        let (agent, _dir, store, client, mock, id) = fixture(false);
        store
            .update(&id, |r| {
                r.close("草稿送审失败");
                r.failures = 1;
                Ok(())
            })
            .unwrap();
        let record = store.get(&id).unwrap();
        assert!(record.draft.is_some());
        assert!(!record.needs_failure_notice());
        let delivery = OutboxDrainAgent {
            epoch: super::super::FeishuEpochIdentity::capture(&agent),
            agent,
            client: Arc::new(client),
        };
        advance(&store, &record, &delivery, &AtomicBool::new(false)).unwrap();
        assert!(mock.notices.lock().unwrap().is_empty());
    }

    #[test]
    fn failures_recorded_without_a_notice_intent_send_no_card() {
        let (agent, _dir, store, client, mock, id) = fixture(false);
        store
            .update(&id, |r| {
                r.draft = None;
                r.drafted_ms = None;
                r.approval_message_id = None;
                r.owner_chat_id = None;
                r.notice_message_id = None;
                r.close("起草未完成");
                r.failures = 3;
                r.last_error = Some("起草失败".into());
                Ok(())
            })
            .unwrap();
        let epoch = super::super::FeishuEpochIdentity::capture(&agent);
        let identity = crate::runtime::document_comments::TaskIdentity {
            agent: agent.name.clone(),
            app: "cli_test".into(),
            epoch: epoch.fingerprint(),
            owner: "ou_owner".into(),
        };
        let delivery = OutboxDrainAgent {
            epoch,
            agent,
            client: Arc::new(client),
        };
        let record = store.get(&id).unwrap();
        assert!(!record.needs_failure_notice());
        advance(&store, &record, &delivery, &AtomicBool::new(false)).unwrap();
        let tasks = crate::runtime::document_comments::task_snapshot(
            &store,
            &[identity],
            Some(std::process::id()),
            now_ms(),
        )
        .unwrap();
        assert_eq!(tasks[0].stage, "failed");
        assert_eq!(tasks[0].error.as_deref(), Some("起草失败"));
        assert!(mock.notices.lock().unwrap().is_empty());
    }

    #[test]
    fn owner_approves_exact_visible_plaintext_and_replays_never_post_again() {
        let (agent, _dir, store, client, mock, id) = fixture(false);
        let record = store.get(&id).unwrap();
        let expected = record.draft.clone().unwrap();
        let card: Value = serde_json::from_str(&draft_card(&record).unwrap()).unwrap();
        let shown = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "div")
            .unwrap();
        assert_eq!(shown["text"]["tag"], "plain_text");
        assert_eq!(shown["text"]["content"], expected);
        let epoch = super::super::FeishuEpochIdentity::capture(&agent);
        let scope = epoch.storage_scope().key.clone();
        handle_card_with_store(&store, &agent, &client, &scope, &epoch, &callback(&id)).unwrap();
        assert_eq!(
            mock.posts.lock().unwrap().as_slice(),
            [
                json!({"content":{"elements":[{"type":"text_run","text_run":{"text":shown["text"]["content"]}}]}})
            ]
        );
        assert_eq!(store.get(&id).unwrap().stage, Stage::Replied);
        let restarted = Store::new(_dir.path().join("comments"));
        handle_card_with_store(&restarted, &agent, &client, &scope, &epoch, &callback(&id))
            .unwrap();
        assert_eq!(mock.posts.lock().unwrap().len(), 1);
    }

    #[test]
    fn approval_revalidates_creator_and_current_owner_after_transfer() {
        for ownership in [1, 2, 3, 4] {
            let (agent, _dir, store, client, mock, id) = fixture(false);
            mock.ownership.store(ownership, Ordering::Relaxed);
            let epoch = super::super::FeishuEpochIdentity::capture(&agent);
            let result = handle_card_with_store(
                &store,
                &agent,
                &client,
                &epoch.storage_scope().key,
                &epoch,
                &callback(&id),
            );
            assert_eq!(result.is_ok(), ownership == 1);
            if ownership != 1 {
                let restarted = Store::new(_dir.path().join("comments"));
                let failed = restarted.get(&id).unwrap();
                assert!(failed.last_error.is_some());
                assert!(draft_card(&failed).unwrap().contains("上次批准未通过复验"));
                let tasks = crate::runtime::document_comments::task_snapshot(
                    &restarted,
                    &[crate::runtime::document_comments::TaskIdentity {
                        agent: agent.name.clone(),
                        app: "cli_test".into(),
                        epoch: epoch.fingerprint(),
                        owner: "ou_owner".into(),
                    }],
                    Some(std::process::id()),
                    now_ms(),
                )
                .unwrap();
                assert_eq!(tasks[0].stage, "approval_failed");
                assert!(mock.posts.lock().unwrap().is_empty());
                mock.ownership.store(0, Ordering::Relaxed);
                handle_card_with_store(
                    &restarted,
                    &agent,
                    &client,
                    &epoch.storage_scope().key,
                    &epoch,
                    &callback(&id),
                )
                .unwrap();
                assert!(restarted.get(&id).unwrap().last_error.is_none());
            }
            assert_eq!(mock.posts.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn failed_context_read_notifies_configured_owner_and_survives_restart() {
        let (agent, dir, store, client, mock, id) = fixture(false);
        store
            .update(&id, |r| {
                r.close("评论处理未完成");
                r.failures = 3;
                r.last_error = Some("创建者无法验证".into());
                r.context = None;
                r.notify_failure = true;
                Ok(())
            })
            .unwrap();
        let epoch = super::super::FeishuEpochIdentity::capture(&agent);
        let delivery = OutboxDrainAgent {
            agent,
            client: Arc::new(client),
            epoch,
        };
        let r = store.get(&id).unwrap();
        assert!(r.needs_failure_notice());
        advance(&store, &r, &delivery, &AtomicBool::new(false)).unwrap();
        let restarted = Store::new(dir.path().join("comments"));
        assert!(restarted.inbox().unwrap().0.is_empty());
        advance(
            &restarted,
            &restarted.get(&id).unwrap(),
            &delivery,
            &AtomicBool::new(false),
        )
        .unwrap();
        let notices = mock.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["receive_id"], "ou_owner");
        assert!(notices[0]["content"]
            .as_str()
            .unwrap()
            .contains("创建者无法验证"));
        assert!(!notices[0]["content"].as_str().unwrap().contains("完整正文"));
        assert!(mock.posts.lock().unwrap().is_empty());
        store
            .update(&id, |r| {
                r.failure_notice_message_id = None;
                r.failure_notice_attempts = 3;
                Ok(())
            })
            .unwrap();
        assert!(store.inbox().unwrap().0.is_empty());
    }

    #[test]
    fn unknown_publish_result_survives_restart_without_another_post() {
        let (agent, dir, store, client, mock, id) = fixture(true);
        let epoch = super::super::FeishuEpochIdentity::capture(&agent);
        let scope = epoch.storage_scope().key.clone();
        assert!(
            handle_card_with_store(&store, &agent, &client, &scope, &epoch, &callback(&id))
                .is_err()
        );
        assert_eq!(store.get(&id).unwrap().stage, Stage::Sending);
        let restarted = Store::new(dir.path().join("comments"));
        handle_card_with_store(&restarted, &agent, &client, &scope, &epoch, &callback(&id))
            .unwrap();
        assert_eq!(mock.posts.lock().unwrap().len(), 1);
    }

    #[test]
    fn handler_rejects_wrong_identity_expiry_and_changed_document_without_post() {
        for invalid in [
            "owner", "card", "chat", "scope", "epoch", "expired", "changed",
        ] {
            let (agent, _dir, store, client, mock, id) = fixture(false);
            let epoch = super::super::FeishuEpochIdentity::capture(&agent);
            let mut scope = epoch.storage_scope().key.clone();
            let mut callback = callback(&id);
            match invalid {
                "owner" => callback.operator_open_id = "ou_intruder".into(),
                "card" => callback.message_id = "om_other".into(),
                "chat" => callback.chat_id = "oc_other".into(),
                "scope" => scope = "other".into(),
                "epoch" => store
                    .update(&id, |r| {
                        r.epoch = "other".into();
                        Ok(())
                    })
                    .unwrap(),
                "expired" => store
                    .update(&id, |r| {
                        r.drafted_ms =
                            Some(now_ms() - crate::runtime::document_comments::DRAFT_TTL_MS);
                        Ok(())
                    })
                    .unwrap(),
                _ => mock.changed.store(true, Ordering::Relaxed),
            }
            let result = handle_card_with_store(&store, &agent, &client, &scope, &epoch, &callback);
            if matches!(invalid, "expired" | "changed") {
                result.unwrap();
                assert_eq!(store.get(&id).unwrap().stage, Stage::Closed);
            } else {
                assert!(result.is_err(), "{invalid}");
            }
            assert!(mock.posts.lock().unwrap().is_empty(), "{invalid}");
        }
    }

    #[test]
    fn clipping_is_marked_and_external_markup_is_escaped() {
        assert!(limited("abcdef", 3).contains("完整内容见原文档"));
        assert!(!limited("<at id=all>", 100).contains("<at"));
    }
}
