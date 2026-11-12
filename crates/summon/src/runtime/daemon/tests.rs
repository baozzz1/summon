//! daemon 模块测试(A1 拆分自原 daemon.rs 的 `mod tests`;`super::*` 经 mod.rs 胶水可见全部子模块)。
use super::*;
use std::time::{Duration, SystemTime};

/// 测试注入的 outbox store(挂在测试自己的 tempdir 下),绝不触真实 `~/.summon/run/outbox`。
fn test_outbox(dir: &std::path::Path) -> crate::runtime::outbox_store::OutboxStore {
    crate::runtime::outbox_store::OutboxStore::new(dir.join("outbox"))
}

/// 回归 Q2(表情停更):本轮 `Ok` 但没向用户发出内容(交互页面满 stall / 纯命令无文字回复)应判
/// `Empty`——**正常收尾、非失败**,`handle_task` 据此贴 DONE 而非 ERROR / 停在 working。旧逻辑返回
/// `!emitted`(空=failed=true)正是 bug 根源。
#[test]
fn silent_reset_result_has_no_terminal_send_in_a_regular_turn() {
    let keyword = summon_core::messaging::result_control::SILENT_RESET_KEYWORD;
    for (summary, chunked) in [(false, false), (true, false), (false, true), (true, true)] {
        let frontend = EmitterFrontend::new();
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_regular_silent_result");
        let outbox = test_outbox(dir.path());
        let (out, _) = card_stream_consume(
            &frontend,
            &chat,
            summary,
            summon_core::progress::DEFAULT_MAX_STEPS,
            false,
            false,
            None,
            None,
            None,
            &outbox,
            &|| None,
            None,
            &mut |_: &[String]| {},
            |on_msg, _, _, on_event, _, _, _| {
                let parts = if chunked {
                    keyword.chars().map(|ch| ch.to_string()).collect::<Vec<_>>()
                } else {
                    vec![keyword.to_string()]
                };
                for part in parts {
                    let mut event = summon_core::agent_msg::AgentMessage::of(
                        summon_core::agent_msg::AgentKind::Assistant,
                    );
                    event.partial = chunked;
                    event.text = Some(part);
                    on_event(&event);
                }
                on_msg(keyword);
                Ok(crate::capture::TurnOutcome {
                    reply: Some(keyword.to_string()),
                    ..Default::default()
                })
            },
        );
        assert!(out.is_ok());
        assert!(
            frontend.calls.borrow().is_empty(),
            "summary={summary}, chunked={chunked}, calls={:?}, payloads={:?}",
            frontend.calls.borrow(),
            frontend.payloads.borrow()
        );
        assert!(outbox.pending_events().is_empty());
    }
}

#[test]
fn silent_reset_stream_after_completed_progress_keeps_only_progress() {
    use summon_core::agent_msg::{AgentKind, AgentMessage};
    let keyword = summon_core::messaging::result_control::SILENT_RESET_KEYWORD;
    for summary in [false, true] {
        for width in [1, 2, 7, keyword.len()] {
            for (boundary, steer, question) in [
                (None, false, false),
                (Some(AgentKind::Thinking), false, false),
                (Some(AgentKind::ToolUse), false, false),
                (Some(AgentKind::ToolResult), false, false),
                (None, true, false),
                (None, false, true),
            ] {
                for process_only in [false, true] {
                    let frontend = EmitterFrontend::new();
                    frontend.process_only.set(process_only);
                    let dir = tempfile::tempdir().unwrap();
                    let chat = badge_chat(dir.path(), "oc_progress_then_silent_result");
                    let (out, _) = card_stream_consume(
                        &frontend,
                        &chat,
                        summary,
                        summon_core::progress::DEFAULT_MAX_STEPS,
                        false,
                        false,
                        None,
                        None,
                        None,
                        &test_outbox(dir.path()),
                        &|| None,
                        None,
                        &mut |_: &[String]| {},
                        |on_msg, _, _, on_event, on_interactive, _, on_boundary| {
                            let mut event = summon_core::agent_msg::AgentMessage::of(
                                summon_core::agent_msg::AgentKind::Assistant,
                            );
                            event.partial = boundary.is_some() || steer || question;
                            event.text = Some("正在检查".into());
                            on_event(&event);
                            if let Some(kind) = boundary.clone() {
                                let mut marker = AgentMessage::of(kind);
                                marker.text = Some("继续检查".into());
                                marker.tool = Some("Read".into());
                                on_event(&marker);
                            }
                            if steer {
                                on_boundary(&["followup".into()]);
                            }
                            if question {
                                on_interactive(&summon_core::interactive::InteractiveState {
                                    title: "继续？".into(),
                                    options: vec![summon_core::interactive::Choice {
                                        index: 1,
                                        label: "继续".into(),
                                    }],
                                    raw: String::new(),
                                });
                            }
                            event.partial = true;
                            if !question {
                                event.text = Some(String::new());
                                on_event(&event);
                            }
                            for part in keyword.as_bytes().chunks(width) {
                                event.text = Some(std::str::from_utf8(part).unwrap().to_string());
                                on_event(&event);
                            }
                            on_msg(keyword);
                            Ok(crate::capture::TurnOutcome {
                                reply: Some(keyword.into()),
                                ..Default::default()
                            })
                        },
                    );
                    assert!(out.is_ok());
                    let payloads = frontend.payloads.borrow().join("\n");
                    assert!(payloads.contains("正在检查"), "{payloads}");
                    assert!(
                    !payloads.contains("SUMMON"),
                    "summary={summary}, width={width}, steer={steer}, question={question}, process_only={process_only}: {payloads}"
                );
                }
            }
        }
    }
}

#[test]
fn silent_reset_prefixes_are_released_when_the_result_does_not_match() {
    for body in [
        "_",
        "__SUMMON",
        "__SUMMON_SILENT_EXIT__\n",
        " __SUMMON_SILENT_EXIT__",
        "__普通回复",
    ] {
        for progress in ["", "正在检查"] {
            let frontend = EmitterFrontend::new();
            let dir = tempfile::tempdir().unwrap();
            let chat = badge_chat(dir.path(), "oc_nonmatching_prefix");
            let outbox = test_outbox(dir.path());
            card_stream_consume(
                &frontend,
                &chat,
                false,
                summon_core::progress::DEFAULT_MAX_STEPS,
                false,
                false,
                None,
                None,
                None,
                &outbox,
                &|| None,
                None,
                &mut |_: &[String]| {},
                |on_msg, _, _, on_event, _, _, _| {
                    if !progress.is_empty() {
                        let mut event = summon_core::agent_msg::AgentMessage::of(
                            summon_core::agent_msg::AgentKind::Assistant,
                        );
                        event.text = Some(progress.into());
                        on_event(&event);
                    }
                    for ch in body.chars() {
                        let mut event = summon_core::agent_msg::AgentMessage::of(
                            summon_core::agent_msg::AgentKind::Assistant,
                        );
                        event.partial = true;
                        event.text = Some(ch.to_string());
                        on_event(&event);
                    }
                    on_msg(body);
                    Ok(crate::capture::TurnOutcome {
                        reply: Some(body.to_string()),
                        ..Default::default()
                    })
                },
            )
            .0
            .unwrap();
            assert_eq!(
                frontend.payloads.borrow().last().map(String::as_str),
                Some(body)
            );
            assert!(outbox.pending_events().is_empty());
        }
    }
}

#[test]
fn silent_reset_result_is_consumed_before_terminal_rendering_and_outbox() {
    let keyword = summon_core::messaging::result_control::SILENT_RESET_KEYWORD;
    for summary in [false, true] {
        for body in [
            keyword.to_string(),
            format!("{keyword}\n"),
            format!("说明 {keyword}"),
        ] {
            let inner = EmitterFrontend::new();
            let buffered = crate::frontend::ResultOnlyFrontend::new(&inner);
            let dir = tempfile::tempdir().unwrap();
            let chat = badge_chat(dir.path(), "oc_silent_result");
            let outbox = test_outbox(dir.path());
            let (out, _) = card_stream_consume(
                &buffered,
                &chat,
                summary,
                summon_core::progress::DEFAULT_MAX_STEPS,
                false,
                false,
                None,
                None,
                None,
                &outbox,
                &|| None,
                None,
                &mut |_: &[String]| {},
                |on_msg, _, _, on_event, _, _, _| {
                    let mut thinking = summon_core::agent_msg::AgentMessage::of(
                        summon_core::agent_msg::AgentKind::Thinking,
                    );
                    thinking.text = Some(keyword.to_string());
                    on_event(&thinking);
                    on_msg(&body);
                    Ok(crate::capture::TurnOutcome {
                        reply: Some(body.clone()),
                        ..Default::default()
                    })
                },
            );
            assert!(out.is_ok());
            buffered.flush(&chat, crate::frontend::ResultOnlyOutcome::Completed);
            assert_eq!(
                inner.calls.borrow().is_empty(),
                body == keyword,
                "{body:?}, summary={summary}"
            );
            assert!(outbox.pending_events().is_empty());
        }
    }
}

#[test]
fn empty_turn_is_success_not_failure() {
    assert_eq!(TurnResult::from_ok(false), TurnResult::Empty);
    assert!(
        !TurnResult::from_ok(false).is_failure(),
        "Ok 但无输出 ≠ 失败(应贴 DONE)"
    );
    assert_eq!(TurnResult::from_ok(true), TurnResult::Done);
    assert!(!TurnResult::from_ok(true).is_failure());
    // 只有真出错(Err 分支映射的 Failed)才算失败 → 贴 ERROR + 提前返回。
    assert!(TurnResult::Failed.is_failure());
}

#[test]
fn only_durability_deferred_recovery_keeps_checkpoint() {
    assert!(
        recovered_turn_is_terminal(TurnResult::Interrupted),
        "用户主动暂停是终态，应销账"
    );
    assert!(!recovered_turn_is_terminal(TurnResult::RecoveryDeferred));
    assert!(recovered_turn_is_terminal(TurnResult::Done));
    assert!(recovered_turn_is_terminal(TurnResult::Empty));
    assert!(recovered_turn_is_terminal(TurnResult::Failed));
}

#[test]
fn failed_turn_notice_keeps_provider_reason_and_retries_delivery() {
    let reason = "Selected model is at capacity. Please try a different model.";
    for fails in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_failure_notice");
        let frontend = EmitterFrontend::new();
        frontend.fail_card.set(fails);
        let outbox = test_outbox(dir.path());
        let outcome = crate::capture::TurnOutcome {
            api_error: Some(reason.into()),
            ..Default::default()
        };
        notify_failed_turn_outcome(&frontend, &chat, "test", &outbox, &outcome);
        assert_eq!(*frontend.calls.borrow(), vec!["card"]);
        let payload = frontend.payloads.borrow();
        let card: serde_json::Value = serde_json::from_str(&payload[0]).unwrap();
        assert!(card["body"]["elements"].to_string().contains(reason));
        assert!(card["config"]["summary"]["content"]
            .as_str()
            .unwrap()
            .contains("Task failed"));
        let pending = outbox.pending_events();
        assert_eq!(pending.len(), usize::from(fails));
        if let Some(event) = pending.first() {
            assert_eq!(event.delivery_scope, chat.delivery_scope);
            match &event.kind {
                summon_core::outbox::OutboxKind::TurnReply { chat_id, text, .. } => {
                    assert_eq!(chat_id, chat.chat_id);
                    assert!(text.contains(reason));
                }
                _ => panic!("失败原因须进入正文补发队列"),
            }
        }
    }
}

#[test]
fn failed_turn_notice_reports_unpersisted_delivery() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("outbox"), "blocked").unwrap();
    let chat = badge_chat(dir.path(), "oc_failure_disk");
    let frontend = EmitterFrontend::new();
    frontend.fail_card.set(true);
    frontend.fail_send.set(true);
    assert!(!notify_failed_turn_outcome(
        &frontend,
        &chat,
        "test",
        &test_outbox(dir.path()),
        &crate::capture::TurnOutcome {
            api_error: Some("capacity".into()),
            ..Default::default()
        },
    ));
}

#[test]
fn failed_turn_notice_scheduled_delivery_keeps_the_provider_reason() {
    let reason = "Selected model is at capacity. Please try a different model.";
    for (fails, partial) in [(false, false), (true, false), (false, true), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_scheduled_failure");
        let frontend = EmitterFrontend::new();
        frontend.fail_card.set(fails);
        let buffered = crate::frontend::ResultOnlyFrontend::new(&frontend);
        if partial {
            buffered.stage_result_body("已经生成的部分结果");
        }
        let outbox = test_outbox(dir.path());
        assert!(notify_failed_turn_outcome(
            &buffered,
            &chat,
            "test",
            &outbox,
            &crate::capture::TurnOutcome {
                api_error: Some(reason.into()),
                ..Default::default()
            },
        ));
        buffered.flush(&chat, crate::frontend::ResultOnlyOutcome::Failed);
        let expected_calls = if partial {
            vec!["card", "send"]
        } else {
            vec!["card"]
        };
        assert_eq!(*frontend.calls.borrow(), expected_calls);
        if partial {
            assert!(frontend.payloads.borrow()[1].contains("已经生成的部分结果"));
            assert!(frontend.payloads.borrow()[1].contains("本轮未完成"));
        }
        assert!(frontend.payloads.borrow()[0].contains(reason));
        let events = outbox.pending_events();
        assert_eq!(events.len(), usize::from(fails));
        if let Some(event) = events.first() {
            match &event.kind {
                summon_core::outbox::OutboxKind::TurnReply { text, .. } => {
                    assert!(text.contains(reason))
                }
                _ => panic!("原始错误须进入补发正文"),
            }
        }
    }
}

#[test]
fn failed_turn_notice_respects_interrupts_and_specialized_recovery() {
    use crate::capture::TurnOutcome;
    let cases = [
        TurnOutcome::default(),
        TurnOutcome {
            api_error: Some("cancelled".into()),
            interrupted: true,
            ..Default::default()
        },
        TurnOutcome {
            api_error: Some("cancelled".into()),
            session_ended: true,
            ..Default::default()
        },
        TurnOutcome {
            api_error: Some("quota".into()),
            api_error_kind: Some(summon_core::agent_msg::AgentErrorKind::UsageLimitExceeded),
            ..Default::default()
        },
    ];
    for outcome in cases {
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_failure_skip");
        let frontend = EmitterFrontend::new();
        let outbox = test_outbox(dir.path());
        notify_failed_turn_outcome(&frontend, &chat, "test", &outbox, &outcome);
        assert!(frontend.calls.borrow().is_empty(), "{outcome:?}");
        assert!(outbox.pending_events().is_empty());
    }
}

#[test]
fn failed_turn_notice_has_a_reason_when_provider_sends_no_details() {
    for outcome in [
        crate::capture::TurnOutcome {
            api_error: Some(String::new()),
            ..Default::default()
        },
        crate::capture::TurnOutcome {
            timed_out: true,
            ..Default::default()
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_failure_empty");
        let frontend = EmitterFrontend::new();
        notify_failed_turn_outcome(&frontend, &chat, "test", &test_outbox(dir.path()), &outcome);
        assert_eq!(*frontend.calls.borrow(), vec!["card"]);
        let payloads = frontend.payloads.borrow();
        let expected = if outcome.timed_out {
            "provider 执行超时"
        } else {
            "provider 未提供具体错误原因"
        };
        assert!(payloads[0].contains(expected));
    }
}

struct EmitterFrontend {
    next_id: std::cell::Cell<u64>,
    fail_begin: std::cell::Cell<bool>,
    fail_patch: std::cell::Cell<bool>,
    fail_send: std::cell::Cell<bool>,
    fail_send_after: std::cell::Cell<Option<usize>>,
    send_attempts: std::cell::Cell<usize>,
    fail_finish: std::cell::Cell<bool>,
    fail_card: std::cell::Cell<bool>,
    process_only: std::cell::Cell<bool>,
    detachable: std::cell::Cell<bool>,
    calls: std::cell::RefCell<Vec<&'static str>>,
    payloads: std::cell::RefCell<Vec<String>>,
    settled_replies: std::cell::Cell<usize>,
    /// 设了就在这个字节处把终态正文切成两段,模拟超出单卡预算的回复。
    split_reply_at: std::cell::Cell<Option<usize>>,
    /// 每条「带会话列表预览」的消息带的总结。
    feeds: std::cell::RefCell<Vec<String>>,
}

impl EmitterFrontend {
    fn new() -> Self {
        Self {
            next_id: std::cell::Cell::new(0),
            fail_begin: std::cell::Cell::new(false),
            fail_patch: std::cell::Cell::new(false),
            fail_send: std::cell::Cell::new(false),
            fail_send_after: std::cell::Cell::new(None),
            send_attempts: std::cell::Cell::new(0),
            fail_finish: std::cell::Cell::new(false),
            fail_card: std::cell::Cell::new(false),
            process_only: std::cell::Cell::new(false),
            detachable: std::cell::Cell::new(false),
            calls: std::cell::RefCell::new(Vec::new()),
            payloads: std::cell::RefCell::new(Vec::new()),
            settled_replies: std::cell::Cell::new(0),
            split_reply_at: std::cell::Cell::new(None),
            feeds: std::cell::RefCell::new(Vec::new()),
        }
    }
}

/// 把 live 卡的段拼成一行文本,供断言「这张卡上有什么」。思考与正文都收进来 ——
/// 两者要不要外显是渲染层的事,测试关心的是段模型里到底有没有这块内容。
fn segments_text(view: &summon_core::cot::CotView) -> String {
    view.segments
        .iter()
        .map(|segment| match segment {
            summon_core::segment::TurnSegment::Thinking(text)
            | summon_core::segment::TurnSegment::Reply(text) => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Frontend for EmitterFrontend {
    fn settle_last_sent_as_reply(&self) {
        self.settled_replies.set(self.settled_replies.get() + 1);
    }
    fn send_message_with_feed(&self, chat: &ChatRef, body: &str, feed: &str) -> anyhow::Result<()> {
        self.feeds.borrow_mut().push(feed.to_string());
        self.send_message(chat, body)
    }
    fn split_final_reply(&self, _chat: &ChatRef, body: &str) -> Vec<String> {
        match self.split_reply_at.get() {
            Some(at) if at < body.len() => vec![body[..at].to_string(), body[at..].to_string()],
            _ => {
                summon_core::feishu::split_for_card(body, summon_core::feishu::CARD_TEXT_SAFE_LIMIT)
            }
        }
    }
    fn send_message(&self, _chat: &ChatRef, body: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("send");
        self.payloads.borrow_mut().push(body.to_string());
        let attempt = self.send_attempts.get();
        self.send_attempts.set(attempt + 1);
        if self.fail_send.get()
            || self
                .fail_send_after
                .get()
                .is_some_and(|successful_sends| attempt >= successful_sends)
        {
            anyhow::bail!("send failed");
        }
        Ok(())
    }

    fn stream_begin(
        &self,
        _chat: &ChatRef,
        content: &str,
        _with_interrupt: bool,
    ) -> anyhow::Result<StreamId> {
        self.calls.borrow_mut().push("begin");
        self.payloads.borrow_mut().push(content.to_string());
        if self.fail_begin.get() {
            anyhow::bail!("begin failed");
        }
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        Ok(StreamId(id))
    }

    fn stream_patch(&self, _id: StreamId, content: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("patch");
        self.payloads.borrow_mut().push(content.to_string());
        if self.fail_patch.get() {
            anyhow::bail!("patch failed");
        }
        Ok(())
    }

    fn stream_finish(&self, _id: StreamId, content: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("finish_text");
        self.payloads.borrow_mut().push(content.to_string());
        if self.fail_finish.get() {
            anyhow::bail!("finish failed");
        }
        Ok(())
    }

    fn stream_is_process_only(&self, _id: StreamId) -> bool {
        self.process_only.get()
    }

    fn stream_detach_cot(&self, _id: StreamId) -> Option<crate::frontend::DetachedCot> {
        self.calls.borrow_mut().push("detach");
        self.detachable.get().then(|| crate::frontend::DetachedCot {
            carrier: crate::frontend::CotCarrier::for_test("cot_emitter", "om_emitter"),
            projector: summon_core::cot::CotProjector::new("chat", "turn", "任务"),
        })
    }

    fn stream_steer_boundary(&self, _id: StreamId, note: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("steer_boundary");
        self.payloads.borrow_mut().push(note.to_string());
        Ok(())
    }

    fn stream_fail(&self, _id: StreamId, content: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("fail");
        self.payloads.borrow_mut().push(content.to_string());
        if self.fail_finish.get() {
            anyhow::bail!("finish failed");
        }
        Ok(())
    }

    fn stream_timeout(&self, _id: StreamId, content: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("timeout");
        self.payloads.borrow_mut().push(content.to_string());
        Ok(())
    }

    /// 覆盖结构化开卡:默认实现开的是空壳(CoT 是唯一载体,没有第二条渲染路径),
    /// 断言不了「这张卡装了什么」。测试要观测内容,就自己把 `view` 的段文本记下来 ——
    /// 而不是让生产多出一条整卡渲染来迁就测试。
    fn live_begin(
        &self,
        _chat: &ChatRef,
        view: &summon_core::cot::CotView,
    ) -> anyhow::Result<StreamId> {
        self.calls.borrow_mut().push("begin");
        self.payloads.borrow_mut().push(segments_text(view));
        if self.fail_begin.get() {
            anyhow::bail!("begin failed");
        }
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        Ok(StreamId(id))
    }

    fn live_update(
        &self,
        _id: StreamId,
        view: &summon_core::cot::CotView,
    ) -> anyhow::Result<crate::frontend::StreamPatchOutcome> {
        self.calls.borrow_mut().push("patch");
        self.payloads.borrow_mut().push(segments_text(view));
        if self.fail_patch.get() {
            anyhow::bail!("patch failed");
        }
        Ok(crate::frontend::StreamPatchOutcome::Applied)
    }

    fn stream_finish_card(&self, _id: StreamId, card_json: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("finish");
        self.payloads.borrow_mut().push(card_json.to_string());
        if self.fail_finish.get() {
            anyhow::bail!("finish failed");
        }
        Ok(())
    }

    fn send_card(&self, _chat: &ChatRef, card_json: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("card");
        self.payloads.borrow_mut().push(card_json.to_string());
        if self.fail_card.get() {
            anyhow::bail!("card failed");
        }
        Ok(())
    }

    fn react_stream_card(&self, _id: StreamId, _key: &str) -> anyhow::Result<String> {
        self.calls.borrow_mut().push("react");
        Ok("rid".into())
    }

    fn react(&self, _msg: &MsgRef, _key: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn unreact(&self, _msg: &MsgRef, _reaction_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn live_card_emitter_begins_then_patches() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_live",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let segments = vec![summon_core::segment::TurnSegment::Reply("hello".into())];
    let mut emitter = CotEmitter::new(None);
    emitter.emit(&frontend, &chat, "tester", &segments, None);
    emitter.emit(&frontend, &chat, "tester", &segments, None);
    assert_eq!(&*frontend.calls.borrow(), &["begin", "patch"]);
}

#[test]
fn live_card_begin_failure_remains_uncertain_after_transport_recovers() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_begin_uncertain");
    let segments = vec![summon_core::segment::TurnSegment::Thinking(
        "working".into(),
    )];
    let mut emitter = CotEmitter::new(None);
    frontend.fail_begin.set(true);
    emitter.emit(&frontend, &chat, "tester", &segments, None);
    frontend.fail_begin.set(false);
    emitter.emit(&frontend, &chat, "tester", &segments, None);
    assert_eq!(&*frontend.calls.borrow(), &["begin"]);
}

#[test]
fn live_card_patch_failure_recovers_on_the_existing_carrier() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_patch_recovery");
    let segments = vec![summon_core::segment::TurnSegment::Thinking(
        "working".into(),
    )];
    let mut emitter = CotEmitter::new(Some(StreamId(7)));
    frontend.fail_patch.set(true);
    emitter.emit(&frontend, &chat, "tester", &segments, None);
    frontend.fail_patch.set(false);
    emitter.emit(&frontend, &chat, "tester", &segments, None);
    assert_eq!(&*frontend.calls.borrow(), &["patch", "patch"]);
}

#[test]
fn failed_live_card_completion_keeps_the_carrier_until_a_confirmed_completion() {
    let frontend = EmitterFrontend::new();
    let mut projector = TurnProjector::new(
        CotEmitter::new(Some(StreamId(7))),
        summon_core::progress::TurnProgress::new(),
        None,
    );
    frontend.fail_finish.set(true);
    finish_leftover_live_card(
        &frontend,
        "tester",
        &mut projector,
        LeftoverCardOutcome::Completed,
    );
    frontend.fail_finish.set(false);
    finish_leftover_live_card(
        &frontend,
        "tester",
        &mut projector,
        LeftoverCardOutcome::Completed,
    );
    finish_leftover_live_card(
        &frontend,
        "tester",
        &mut projector,
        LeftoverCardOutcome::Completed,
    );
    assert_eq!(&*frontend.calls.borrow(), &["finish_text", "finish_text"]);
}

#[test]
fn completed_live_carriers_cannot_reopen_on_late_projection_events() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_closed_carrier");
    let mut projector = cot_projector(Some(StreamId(7)));
    finish_leftover_live_card(
        &frontend,
        "tester",
        &mut projector,
        LeftoverCardOutcome::Completed,
    );
    projector.live.emit(
        &frontend,
        &chat,
        "tester",
        &[summon_core::segment::TurnSegment::Thinking(
            "late progress".into(),
        )],
        None,
    );
    assert_eq!(&*frontend.calls.borrow(), &["finish_text"]);
}

#[test]
fn repeated_results_keep_one_terminal_delivery_and_one_failure_reconciliation() {
    use summon_core::agent_msg::AgentKind;
    for summary in [false, true] {
        for failed in [false, true] {
            let frontend = EmitterFrontend::new();
            frontend.fail_send.set(failed);
            frontend.fail_card.set(failed);
            let dir = tempfile::tempdir().unwrap();
            let chat = badge_chat(dir.path(), "oc_result_release");
            let outbox = test_outbox(dir.path());
            let (out, emitted) = card_stream_consume(
                &frontend,
                &chat,
                summary,
                summon_core::progress::DEFAULT_MAX_STEPS,
                false,
                false,
                None,
                None,
                None,
                &outbox,
                &|| None,
                None,
                &mut |_: &[String]| {},
                |on_message, _, _, on_event, _, on_tick, on_boundary| {
                    on_event(&process_reply_event(AgentKind::Thinking, "working"));
                    on_event(&process_reply_event(AgentKind::Assistant, "done"));
                    on_message("done");
                    on_boundary(&[]);
                    on_message("done");
                    on_event(&process_reply_event(AgentKind::Thinking, "late progress"));
                    on_tick(None);
                    Ok(crate::capture::TurnOutcome {
                        reply: Some("done".into()),
                        ..Default::default()
                    })
                },
            );
            assert!(out.is_ok());
            assert!(emitted, "过程流已确认创建");
            let calls = frontend.calls.borrow();
            assert_eq!(calls.iter().filter(|call| **call == "begin").count(), 1);
            assert_eq!(
                calls.iter().filter(|call| **call == "card").count(),
                usize::from(summary)
            );
            assert_eq!(
                calls.iter().filter(|call| **call == "send").count(),
                usize::from(!summary)
            );
            assert_eq!(frontend.settled_replies.get(), usize::from(!failed));
            assert_eq!(outbox.pending_events().len(), usize::from(failed));
        }
    }
}

#[test]
fn a_terminal_reply_is_not_released_as_process_text_by_late_tool_events() {
    use summon_core::agent_msg::AgentKind;
    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_result_late_tool");
    let (out, _) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false,
        None,
        None,
        None,
        &test_outbox(dir.path()),
        &|| None,
        None,
        &mut |_: &[String]| {},
        |on_message, _, _, on_event, _, _, on_boundary| {
            on_event(&process_reply_event(AgentKind::Thinking, "working"));
            on_event(&process_reply_event(AgentKind::Assistant, "terminal body"));
            on_message("terminal body");
            for event in process_tool_events() {
                on_event(&event);
            }
            on_boundary(&[]);
            on_message("terminal body");
            Ok(crate::capture::TurnOutcome {
                reply: Some("terminal body".into()),
                ..Default::default()
            })
        },
    );
    assert!(out.is_ok());
    assert!(
        frontend.text_blocks().is_empty(),
        "终态正文不得进入过程 CoT"
    );
    assert_eq!(frontend.sent.borrow().len(), 1);
    assert!(frontend.sent.borrow()[0].contains("terminal body"));
}

#[test]
fn new_live_card_without_export_clears_old_checkpoint() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_checkpoint",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let tag = chat_tag(chat.chat_type, chat.chat_id);
    let stale = summon_core::session::stream_card::StreamCardState {
        message_id: "om_old".into(),
        chat_tag: tag.clone(),
        with_interrupt: true,
        created_ms: now_unix_ms(),
    };
    crate::runtime::inflight::write_stream_state(dir.path(), &tag, &stale).unwrap();

    let segments = vec![summon_core::segment::TurnSegment::Reply("new".into())];
    CotEmitter::new(None).emit(&frontend, &chat, "tester", &segments, None);

    assert!(
        crate::runtime::inflight::read_stream_state(dir.path(), &tag).is_none(),
        "新卡 checkpoint 无法导出时必须清旧卡状态"
    );
}

/// hide_thinking 开:thinking 正文不上卡(经网关接入的模型模拟 claude 自家模型),回复照渲。
#[test]
fn hide_thinking_suppresses_thinking_text_but_keeps_reply() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_hide_thinking",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (_, _) = card_stream_consume(
        &frontend,
        &chat,
        false, // summary_card
        summon_core::progress::DEFAULT_MAX_STEPS,
        true,  // hide_thinking: 开
        false, // tool_trail: 关(本测试只验 hide_thinking)
        None,
        None,
        None, // badge_seed
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        move |on_msg, _on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            let mut thought = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Thinking,
            );
            thought.text = Some("这是一段不该上卡的秘密思考".into());
            on_event(&thought);
            let mut reply = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Assistant,
            );
            reply.text = Some("可见回复".into());
            on_event(&reply);
            on_msg("可见回复");
            Ok(crate::capture::TurnOutcome::default())
        },
    );
    let all = frontend.payloads.borrow().join("\n");
    assert!(
        !all.contains("秘密思考"),
        "hide_thinking 开:thinking 正文绝不能上卡: {all}"
    );
    assert!(
        all.contains("可见回复"),
        "hide_thinking 不该影响回复正文: {all}"
    );
}

/// 对照组:hide_thinking 关(默认)时 thinking 正文照渲 —— 证明上面的隐藏确实是开关在起作用。
#[test]
fn thinking_text_renders_when_hide_thinking_is_off() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_thinking_shown",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (_, _) = card_stream_consume(
        &frontend,
        &chat,
        false,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false, // hide_thinking: 关(默认)
        false, // tool_trail: 关(默认)
        None,
        None,
        None,
        &test_outbox(dir.path()),
        &|| None,
        None,
        &mut |_: &[String]| {},
        move |on_msg, _on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            let mut thought = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Thinking,
            );
            thought.text = Some("这段思考应该可见".into());
            on_event(&thought);
            on_msg("收尾");
            Ok(crate::capture::TurnOutcome::default())
        },
    );
    let all = frontend.payloads.borrow().join("\n");
    assert!(
        all.contains("这段思考应该可见"),
        "hide_thinking 关:thinking 正文必须照渲: {all}"
    );
}

#[test]
fn completed_snapshot_observation_contains_only_metadata_not_reply_text() {
    use summon_core::agent_msg::{AgentKind, AgentMessage, ContentStreamSource};
    use summon_core::segment::{PushOutcome, TurnSegment};

    let secret = "绝不能进入 daemon 日志的回复正文";
    let mut event = AgentMessage::of(AgentKind::Assistant);
    event.text = Some(secret.to_string());
    event.stream_source = Some(ContentStreamSource::CodexRpcCompleted);
    event.source_item_id = Some("item_safe".to_string());
    let before = reply_projection_meta(&[TurnSegment::Reply(secret.to_string())]);
    let observation = rpc_content_observation(
        &event,
        PushOutcome::IgnoredDuplicate,
        before.clone(),
        before,
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_observe",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let merge_line = completed_merge_log_line("tester", &chat, &observation, false);
    let patch_line = patch_observation_log_line(
        "tester",
        &chat,
        &observation,
        "patch",
        "applied",
        &format!(r#"{{"content":"{secret}"}}"#),
    );

    for line in [&merge_line, &patch_line] {
        assert!(!line.contains(secret), "观测日志不得复制回复正文: {line}");
        assert!(!line.contains("item_safe"), "provider item id 也应摘要化");
        assert!(line.contains("item_md5="));
        assert!(line.contains("md5="));
    }

    event.stream_source = Some(ContentStreamSource::CodexRpcDelta);
    let delta = rpc_content_observation(
        &event,
        PushOutcome::Appended,
        reply_projection_meta(&[]),
        reply_projection_meta(&[TurnSegment::Reply(secret.to_string())]),
    )
    .expect("delta 也必须能关联到中间 PATCH");
    let delta_line =
        patch_observation_log_line("tester", &chat, &delta, "patch", "throttled", secret);
    assert!(delta_line.contains("source=codex_rpc_delta"));
    assert!(delta_line.contains("outcome=throttled"));
    assert!(!delta_line.contains(secret));
}

#[test]
fn codex_completed_same_item_keeps_projection_stable() {
    use summon_core::agent_msg::{AgentKind, AgentMessage, ContentStreamSource};
    use summon_core::segment::{
        push_segment_with_source_item, PushOutcome, SegKind, SourceItemSegments, TurnSegment,
    };

    let mut segments = vec![TurnSegment::Reply("收到上一条".into())];
    let mut source_items = SourceItemSegments::default();
    for token in ["收", "尾"] {
        let mut delta = AgentMessage::of(AgentKind::Assistant);
        delta.text = Some(token.to_string());
        delta.partial = true;
        delta.stream_source = Some(ContentStreamSource::CodexRpcDelta);
        delta.source_item_id = Some("item_current".to_string());
        push_segment_with_source_item(
            &mut segments,
            &mut source_items,
            SegKind::Reply,
            token,
            true,
            source_item_update(&delta),
        );
    }

    let before = reply_projection_meta(&segments);
    let mut completed = AgentMessage::of(AgentKind::Assistant);
    completed.text = Some("收尾".to_string());
    completed.stream_source = Some(ContentStreamSource::CodexRpcCompleted);
    completed.source_item_id = Some("item_current".to_string());
    let merge = push_segment_with_source_item(
        &mut segments,
        &mut source_items,
        SegKind::Reply,
        completed.text.as_deref().unwrap(),
        false,
        source_item_update(&completed),
    );
    let after = reply_projection_meta(&segments);

    assert_eq!(merge, PushOutcome::IgnoredDuplicate);
    assert_eq!(before, after, "completed 不得让卡片正文投影发生变化");
    assert_eq!(
        segments,
        vec![
            TurnSegment::Reply("收到上一条".into()),
            TurnSegment::Reply("收尾".into()),
        ]
    );
}

#[test]
fn codex_summary_source_items_map_to_one_runtime_thinking_segment() {
    use summon_core::agent_msg::{AgentKind, AgentMessage, ContentStreamSource};
    use summon_core::segment::{
        push_segment_with_source_item, SegKind, SourceItemSegments, TurnSegment,
    };

    let mut segments = Vec::new();
    let mut source_items = SourceItemSegments::default();
    for (item_id, title) in [
        ("rs_1#summary:0", "**第一项**"),
        ("rs_2#summary:0", "**第二项**"),
    ] {
        let mut event = AgentMessage::of(AgentKind::Thinking);
        event.text = Some(title.to_string());
        event.partial = true;
        event.stream_source = Some(ContentStreamSource::CodexRpcSummaryDelta);
        event.source_item_id = Some(item_id.to_string());
        push_segment_with_source_item(
            &mut segments,
            &mut source_items,
            SegKind::Thinking,
            title,
            true,
            source_item_update(&event),
        );
    }

    assert_eq!(
        segments,
        vec![TurnSegment::Thinking("**第一项**\n**第二项**".into())]
    );
}

#[test]
fn live_card_emitter_notifies_once_after_repeated_patch_failures() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_patch_fail",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let segments = vec![summon_core::segment::TurnSegment::Reply("hello".into())];
    let mut emitter = CotEmitter::new(None);
    emitter.emit(&frontend, &chat, "tester", &segments, None);
    frontend.fail_patch.set(true);
    for _ in 0..(PATCH_FAIL_NOTIFY_THRESHOLD + 2) {
        emitter.emit(&frontend, &chat, "tester", &segments, None);
    }
    let calls = frontend.calls.borrow();
    assert_eq!(calls.iter().filter(|call| **call == "react").count(), 1);
    assert_eq!(calls.iter().filter(|call| **call == "send").count(), 1);
}

#[test]
fn a_failed_cot_append_keeps_the_same_process_carrier() {
    let frontend = EmitterFrontend::new();
    frontend.fail_patch.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_recovered_patch",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let segments = vec![summon_core::segment::TurnSegment::Reply("hello".into())];

    CotEmitter::new(Some(StreamId(8))).emit(&frontend, &chat, "tester", &segments, None);

    assert_eq!(&*frontend.calls.borrow(), &["patch"]);
}

#[test]
fn oversized_final_reply_all_failures_remain_eligible_for_outbox() {
    let frontend = EmitterFrontend::new();
    frontend.fail_begin.set(true);
    frontend.fail_send.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_offline",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let body = "x".repeat(summon_core::feishu::CARD_TEXT_SAFE_LIMIT * 2);
    let mut stream_id = None;
    let mut emitted = false;
    let report = deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut stream_id,
        &mut emitted,
        &body,
        None,
        false,
    );
    assert!(!emitted, "所有分段发送都失败时不能误标已发出");
    assert_eq!(report, DeliveryReport::default());
    assert!(summon_core::outbox::should_enqueue_incomplete_turn_reply(
        report.complete,
        &body
    ));
}

/// 正文里带邮箱就按飞书内容审核拒收的前端(真机:230028 contain sensitive data: EMAIL_ADDRESS)。
#[derive(Default)]
struct AuditingFrontend {
    sent: std::cell::RefCell<Vec<String>>,
}

impl Frontend for AuditingFrontend {
    fn send_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<()> {
        self.send_message(chat, card_json)
    }
    fn split_final_reply(&self, _chat: &ChatRef, body: &str) -> Vec<String> {
        body.split("\n---\n").map(str::to_string).collect()
    }
    fn send_message(&self, _chat: &ChatRef, body: &str) -> anyhow::Result<()> {
        if body.contains("someone@example.com") {
            return Err(anyhow::Error::new(
                crate::frontend::client::ContentRejected::for_test(
                    "contain sensitive data: EMAIL_ADDRESS",
                ),
            ));
        }
        self.sent.borrow_mut().push(body.to_string());
        Ok(())
    }
    fn stream_begin(&self, _: &ChatRef, _: &str, _: bool) -> anyhow::Result<StreamId> {
        Ok(StreamId(1))
    }
    fn stream_patch(&self, _: StreamId, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn react(&self, _: &crate::frontend::MsgRef, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn unreact(&self, _: &crate::frontend::MsgRef, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 被审核拦下的回复原样重发结果都一样:改发一句说明顶替它、算作送达。
/// 角标卡被拒后的普通消息兜底同样被拒,说明只发一次。
#[test]
fn audit_rejected_final_reply_is_replaced_by_a_notice() {
    let frontend = AuditingFrontend::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_audit_rejected");
    let badge = summon_core::feishu::TurnBadge {
        cli: "claude".into(),
        ..Default::default()
    };
    let mut stream_id = None;
    let mut emitted = false;
    let report = deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut stream_id,
        &mut emitted,
        "登录用 someone@example.com 那个账号",
        Some(&badge),
        false,
    );
    assert_eq!(
        report,
        DeliveryReport {
            any_sent: true,
            complete: true
        }
    );
    let sent = frontend.sent.borrow();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("飞书内容审核拦下"));
    assert!(sent[0].contains("EMAIL_ADDRESS"));
}

/// 超长分段里只有被拦的那一段换成说明,其余照发。
#[test]
fn audit_rejected_segment_of_a_long_reply_is_replaced_alone() {
    let frontend = AuditingFrontend::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_audit_segments");
    let report = deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut None,
        &mut false,
        "第一段正常\n---\n第二段写着 someone@example.com",
        None,
        false,
    );
    assert!(report.complete);
    let sent = frontend.sent.borrow();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0], "第一段正常");
    assert!(sent[1].contains("飞书内容审核拦下"));
}

/// 默认的总结卡收尾同一规矩:总结卡或它的超长补段被拦,都只把被拦的那一段换成说明。
#[test]
fn audit_rejected_summary_card_is_replaced_by_a_notice() {
    let frontend = AuditingFrontend::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_audit_summary");
    let reply = "登录用 someone@example.com 那个账号";
    let segments = vec![
        summon_core::segment::TurnSegment::Thinking("想想".into()),
        summon_core::segment::TurnSegment::Reply(reply.into()),
    ];
    let report = finalize_turn_summary(
        &frontend,
        &chat,
        "tester",
        &mut None,
        &segments,
        &segments,
        Some(reply),
        None,
        true,
        None,
        None,
        false,
    )
    .expect("有过程与回复时走总结卡");
    assert!(report.complete);
    {
        let sent = frontend.sent.borrow();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("飞书内容审核拦下"));
    }

    let frontend = AuditingFrontend::default();
    let reply = "结论在这里\n---\n附:someone@example.com";
    let segments = vec![
        summon_core::segment::TurnSegment::Thinking("想想".into()),
        summon_core::segment::TurnSegment::Reply(reply.into()),
    ];
    let report = finalize_turn_summary(
        &frontend,
        &chat,
        "tester",
        &mut None,
        &segments,
        &segments,
        Some(reply),
        None,
        true,
        None,
        None,
        false,
    )
    .expect("有过程与回复时走总结卡");
    assert!(report.complete);
    let sent = frontend.sent.borrow();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[0].contains("结论在这里"));
    assert!(sent[1].contains("飞书内容审核拦下"));
}

#[test]
fn recovered_final_reply_falls_back_when_patch_fails() {
    let frontend = EmitterFrontend::new();
    frontend.fail_finish.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_recovered",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let mut stream_id = Some(StreamId(9));
    let mut emitted = false;
    let report = deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut stream_id,
        &mut emitted,
        "权威终态回复",
        None,
        false,
    );

    assert_eq!(
        report,
        DeliveryReport {
            any_sent: true,
            complete: true,
        }
    );
    assert!(emitted);
    assert_eq!(&*frontend.calls.borrow(), &["finish_text", "send"]);
}

#[test]
fn process_only_stream_is_not_split_by_the_legacy_card_budget() {
    let frontend = EmitterFrontend::new();
    frontend.process_only.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_cot_long",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let segments = vec![summon_core::segment::TurnSegment::Reply(
        "长".repeat(summon_core::feishu::CARD_TOTAL_SAFE_LIMIT),
    )];
    CotEmitter::new(Some(StreamId(9))).emit(&frontend, &chat, "tester", &segments, None);
    assert_eq!(
        &*frontend.calls.borrow(),
        &["patch"],
        "CoT 事件流不应按 legacy 卡片字节预算先定格再开新气泡"
    );
}

#[test]
fn process_only_stream_is_closed_then_final_reply_is_sent_as_a_new_message() {
    let frontend = EmitterFrontend::new();
    frontend.process_only.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_cot",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let mut stream_id = Some(StreamId(10));
    let mut emitted = false;
    let report = deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut stream_id,
        &mut emitted,
        "独立终态回复",
        None,
        false,
    );

    assert_eq!(&*frontend.calls.borrow(), &["finish_text", "send"]);
    assert_eq!(stream_id, None);
    assert_eq!(
        report,
        DeliveryReport {
            any_sent: true,
            complete: true,
        }
    );
}

/// 角标测试共用:claude 单卡路径典型形态(5 小时与周额度剩余,来自 statusLine 快照
/// five_hour / seven_day 的翻面值)。
fn quota_badge() -> summon_core::feishu::TurnBadge {
    summon_core::feishu::TurnBadge {
        cli: "claude".into(),
        model: Some("Opus 5".into()),
        effort: Some("xhigh".into()),
        // 1M 窗口下的 12%:渲染成进度条(见 core 的 `render_bar`)。
        ctx: summon_core::context_budget::ContextBudget::from_reported_pct(12, Some(1_000_000)),
        quota_left_pct: Some(79),
        quota_resets_at: None,
        quota_5h_left_pct: Some(64),
        quota_5h_resets_at: None,
        // 本组测试验证的是「开着时的渲染形态」;默认关的行为由 core 的 badge 测试单独钉。
        show_quota: true,
        merge_default_codex_quota: false,
        hide_effort: false,
        context_remedy: Default::default(),
    }
}

fn assert_compact_summary_footer(card: &str, badge_fragment: Option<&str>) {
    let value: serde_json::Value = serde_json::from_str(card).unwrap();
    let elements = value["body"]["elements"].as_array().unwrap();
    let notation: Vec<_> = elements
        .iter()
        .filter(|element| element["text_size"] == "notation")
        .collect();
    assert_eq!(notation.len(), 1, "底部只能有一个 notation 元素: {card}");
    let footer = notation[0]["content"].as_str().unwrap();
    assert!(
        !footer.contains("关于：") && !footer.contains("回复于"),
        "{footer}"
    );
    if let Some(fragment) = badge_fragment {
        assert!(footer.contains(fragment), "角标缺失: {footer}");
    }
    let timestamp = footer
        .trim_start_matches("<font color='grey'>")
        .trim_end_matches("</font>")
        .rsplit(" · ")
        .next()
        .unwrap();
    assert_eq!(timestamp.len(), 5, "时刻应为 HH:MM: {footer}");
    assert_eq!(timestamp.as_bytes()[2], b':', "时刻应为 HH:MM: {footer}");
    assert!(timestamp
        .chars()
        .enumerate()
        .all(|(index, ch)| index == 2 || ch.is_ascii_digit()));
}

/// 跨档时发的是带按钮的卡:都有交接输入框 + Handoff,能压缩的会话另有 Compact;按钮带着发卡时的
/// 会话 id,点的时候据它确认会话没换过。
#[test]
fn context_warning_is_a_card_with_the_remedy_the_session_has() {
    use summon_core::context_budget::{ContextBudget, ContextRemedy};
    for (remedy, offered, absent) in [
        (
            ContextRemedy::Compact,
            "ctx_compact",
            "\"remedy\":\"handoff\"",
        ),
        (ContextRemedy::Handoff, "ctx_handoff", "ctx_compact"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_ctx_warn");
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let frontend = EmitterFrontend::new();
        maybe_warn_context(
            &tag,
            &chat,
            "tester",
            &frontend,
            ContextBudget::from_tokens(650_000, Some(1_000_000)),
            remedy,
            Some("sid-1"),
        );
        assert_eq!(*frontend.calls.borrow(), vec!["card"], "{remedy:?}");
        let card = frontend.payloads.borrow()[0].clone();
        assert!(card.contains(offered), "{card}");
        assert!(!card.contains(absent), "{card}");
        assert!(card.contains("ctx_ignore"));
        assert!(card.contains("ctx_handoff"), "两种会话都能从卡上发起交接");
        assert!(card.contains("sid-1"));
        assert_eq!(cards::ctx_warned_step(dir.path(), &tag), Some(60));
    }
}

/// 上下文提醒卡的接线:点击进专用 handler;认领表在分派循环之外建(建在循环里就每条消息清空
/// 一次,连点防不住);轮末发卡带着本轮的按钮选择与会话 id(漏了会话 id,旧卡就能作用到新会话)。
#[test]
fn context_warning_is_wired_from_turn_end_to_click() {
    let dispatcher = include_str!("dispatcher.rs");
    let claimed = dispatcher
        .find("let mut context_warn_claimed")
        .expect("认领表");
    let main_loop = claimed
        + dispatcher[claimed..]
            .find("\n    loop {")
            .expect("分派循环");
    let arm = dispatcher
        .find("CardAction::ContextCompact\n")
        .expect("提醒卡的分派臂");
    assert!(claimed < main_loop && main_loop < arm);
    let arm_body =
        &dispatcher[arm..arm + dispatcher[arm..].find("\n                    }\n").unwrap()];
    assert!(arm_body.contains("cards::handle_context_warn_card("));
    assert!(arm_body.contains("claimed: &mut context_warn_claimed"));

    let chat_thread = include_str!("chat_thread.rs");
    let call = chat_thread
        .find("            maybe_warn_context(\n")
        .expect("轮末发卡");
    let call = &chat_thread[call..call + chat_thread[call..].find(");").unwrap()];
    assert!(call.contains("badge.context_remedy"), "{call}");
    assert!(call.contains("proj.turn_sid.as_deref()"), "{call}");
}

/// 提醒卡发起的交接经代投通道进来,SentinelFire 臂要把交接备注原样交给 task —— 丢了它,那一轮
/// 就成了普通的「请写交接」对话:不归档、不开新会话,卡上却写着已发起交接。
#[test]
fn sentinel_fire_arm_hands_the_handoff_note_to_the_task() {
    let dispatcher = include_str!("dispatcher.rs");
    let start = dispatcher.find("Ok(DispatchMsg::SentinelFire {").unwrap();
    let end = start
        + dispatcher[start..]
            .find("Ok(DispatchMsg::RecallMessage {")
            .unwrap();
    let arm = &dispatcher[start..end];
    assert!(
        arm.contains("                handoff_note,\n            }) => {"),
        "SentinelFire 臂要解构出交接备注"
    );
    let call = arm.find("sentinel_chat_task(").unwrap();
    let args: Vec<&str> = arm[call..call + arm[call..].find(");").unwrap()]
        .lines()
        .skip(1)
        .map(str::trim)
        .collect();
    assert_eq!(args[2], "handoff_note,", "第三个参数是交接备注:{args:?}");
}

/// 卡没发出去就不记档位:下一轮还会再发,记了才是真的漏提醒。
#[test]
fn context_warning_card_that_failed_to_send_is_retried_next_turn() {
    use summon_core::context_budget::{ContextBudget, ContextRemedy};
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_ctx_warn_fail");
    let tag = chat_tag(chat.chat_type, chat.chat_id);
    let frontend = EmitterFrontend::new();
    frontend.fail_card.set(true);
    maybe_warn_context(
        &tag,
        &chat,
        "tester",
        &frontend,
        ContextBudget::from_tokens(650_000, Some(1_000_000)),
        ContextRemedy::Handoff,
        None,
    );
    assert_eq!(*frontend.calls.borrow(), vec!["card"]);
    assert_eq!(cards::ctx_warned_step(dir.path(), &tag), None);
}

fn badge_chat<'a>(dir: &'a std::path::Path, id: &'a str) -> ChatRef<'a> {
    ChatRef {
        thread_anchor: None,
        chat_id: id,
        chat_type: "p2p",
        agent_dir: dir,
        agent_name: "tester",
        delivery_scope: "test-scope",
    }
}

/// owner 2026-08-07:claude 原生模型不产 thinking 段 → 走不到总结卡 → 角标一起丢了。
/// 单卡路径必须把 live 卡定格成带角标的卡片(而不是纯文本 patch),周额度才看得见。
#[test]
fn single_card_final_reply_carries_the_badge_when_there_is_no_thinking() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_badge");
    let mut stream_id = Some(StreamId(11));
    let mut emitted = false;

    let report = deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut stream_id,
        &mut emitted,
        "无思考过程的回复",
        Some(&quota_badge()),
        false,
    );

    assert!(report.any_sent && report.complete);
    assert_eq!(
        &*frontend.calls.borrow(),
        &["finish_text", "card"],
        "过程收口后发送带角标的终态卡"
    );
    let card = frontend.payloads.borrow().last().cloned().unwrap();
    assert!(card.contains("无思考过程的回复"), "正文必须原样在卡里");
    assert!(
        card.contains("claude · Opus 5 · xhigh · 120k/1.0M ██░░░░░░░░ 12% · 5h 64% 7d 79%"),
        "角标行缺失或格式漂了: {card}"
    );
    assert_compact_summary_footer(&card, Some("claude · Opus 5"));
}

/// 红线:角标缺失时这条路径必须逐字节等同改动前(纯文本终态对账),
/// 否则「加个装饰」会把所有非 claude / 无快照的轮一起改形态。
#[test]
fn single_card_final_reply_without_a_badge_stays_on_the_plain_text_path() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_nobadge");
    let mut stream_id = Some(StreamId(12));
    let mut emitted = false;

    deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut stream_id,
        &mut emitted,
        "老路径回复",
        None,
        false,
    );

    assert_eq!(&*frontend.calls.borrow(), &["finish_text", "send"]);
    assert_eq!(
        frontend.payloads.borrow().last().map(String::as_str),
        Some("老路径回复"),
        "无角标时刷的仍是裸正文,不该被包成卡片 JSON"
    );
}

/// 角标是装饰:卡片刷不上去(超预算 / 远端拒)必须回退纯文本,绝不能把已生成的回复挡在外面。
#[test]
fn badge_card_failure_falls_back_to_the_plain_text_finish() {
    let frontend = EmitterFrontend::new();
    frontend.fail_card.set(true); // finish_card 与 finish_text 一起失败 → 一路退到 send
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_badge_fail");
    let mut stream_id = Some(StreamId(13));
    let mut emitted = false;

    let report = deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut stream_id,
        &mut emitted,
        "回退也不能丢的回复",
        Some(&quota_badge()),
        false,
    );

    assert_eq!(
        &*frontend.calls.borrow(),
        &["finish_text", "card", "send"],
        "过程收口，终态卡发送失败后发送普通消息"
    );
    assert!(report.any_sent, "最终仍要把回复送出去");
    assert!(emitted);
}

#[test]
fn partial_final_reply_remains_eligible_for_full_body_retry() {
    let frontend = EmitterFrontend::new();
    frontend.fail_send_after.set(Some(1));
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_partial",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let body = "x".repeat(summon_core::feishu::CARD_TEXT_SAFE_LIMIT * 2);
    let mut stream_id = None;
    let mut emitted = false;
    let report = deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut stream_id,
        &mut emitted,
        &body,
        None,
        false,
    );

    assert!(report.any_sent, "首段已经终态发送成功");
    assert!(!report.complete, "后续分段失败不能误报完整送达");
    assert!(summon_core::outbox::should_enqueue_incomplete_turn_reply(
        report.complete,
        &body
    ));
}

/// 终态回复发出去了才登记为本轮的回复卡;一条都没发出去就不登记。
#[test]
fn a_delivered_final_reply_is_registered_and_a_failed_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_final_reply",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    for (fail, registered) in [(false, 1), (true, 0)] {
        let frontend = EmitterFrontend::new();
        frontend.fail_send.set(fail);
        let report = deliver_final_reply(
            &frontend, &chat, "tester", &mut None, &mut false, "结论", None, false,
        );
        assert_eq!(report.any_sent, !fail);
        assert_eq!(frontend.settled_replies.get(), registered);
    }
}

#[test]
fn summary_reply_is_sent_even_without_a_live_stream() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_summary_cold",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let segments = vec![
        summon_core::segment::TurnSegment::Thinking("思考".into()),
        summon_core::segment::TurnSegment::Reply("结论".into()),
    ];

    let report = finalize_turn_summary(
        &frontend,
        &chat,
        "tester",
        &mut None,
        &segments,
        &segments,
        Some("结论"),
        None,
        true,
        None,
        None,
        false,
    )
    .expect("有 thinking 和 reply 时必须产生总结投递结果");

    assert_eq!(
        report,
        DeliveryReport {
            any_sent: true,
            complete: true,
        }
    );
    assert_eq!(&*frontend.calls.borrow(), &["card"]);
    assert_eq!(
        frontend.settled_replies.get(),
        1,
        "总结卡就是本轮的终态回复"
    );
    let card = frontend.payloads.borrow().last().cloned().unwrap();
    assert_compact_summary_footer(&card, None);
}

fn feed_preview(card: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(card).unwrap();
    value["config"]["summary"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// 两种收尾形态都把 agent 给的总结做成会话列表预览,标签本身不进卡片正文。
#[test]
fn final_reply_cards_use_the_agent_feed_summary_and_hide_the_tag() {
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_feed");
    let reply = "修好了卡片额度显示。\n\n<summon-feed>修好卡片额度显示</summon-feed>";

    let single = EmitterFrontend::new();
    deliver_final_reply(
        &single,
        &chat,
        "tester",
        &mut None,
        &mut false,
        reply,
        Some(&quota_badge()),
        false,
    );
    let card = single.payloads.borrow().last().cloned().unwrap();
    assert_eq!(feed_preview(&card), "修好卡片额度显示");
    assert!(card.contains("修好了卡片额度显示。"));
    assert!(!card.contains("summon-feed"), "标签不得出现在卡里:{card}");

    let summary = EmitterFrontend::new();
    let segments = vec![
        summon_core::segment::TurnSegment::Thinking("思考".into()),
        summon_core::segment::TurnSegment::Reply(reply.into()),
    ];
    finalize_turn_summary(
        &summary,
        &chat,
        "tester",
        &mut None,
        &segments,
        &segments,
        Some(reply),
        None,
        true,
        None,
        None,
        false,
    )
    .expect("有 thinking 和 reply 时产生总结卡");
    let card = summary.payloads.borrow().last().cloned().unwrap();
    assert_eq!(feed_preview(&card), "修好卡片额度显示");
    assert!(!card.contains("summon-feed"), "标签不得出现在卡里:{card}");

    // 总结不合格:标签照样拿掉,预览退回正文开头。
    let fallback = EmitterFrontend::new();
    deliver_final_reply(
        &fallback,
        &chat,
        "tester",
        &mut None,
        &mut false,
        "正文开头\n<summon-feed>这一句总结远远超过了十四个字的上限</summon-feed>",
        Some(&quota_badge()),
        false,
    );
    let card = fallback.payloads.borrow().last().cloned().unwrap();
    assert_eq!(feed_preview(&card), "正文开头");
    assert!(!card.contains("summon-feed"));
    assert!(fallback.feeds.borrow().is_empty(), "不合格的总结不设预览");
}

/// 回复拆成多段时,会话列表展示的是最后一段:首段卡自带总结,末段随消息带上,中间段不带。
#[test]
fn split_final_reply_puts_the_agent_feed_on_the_last_segment() {
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_feed_split");
    let reply = "第一段正文第二段正文\n<summon-feed>长报告已发出</summon-feed>";

    let single = EmitterFrontend::new();
    single.split_reply_at.set(Some("第一段正文".len()));
    deliver_final_reply(
        &single,
        &chat,
        "tester",
        &mut None,
        &mut false,
        reply,
        Some(&quota_badge()),
        false,
    );
    assert_eq!(&*single.calls.borrow(), &["card", "send"]);
    assert_eq!(&*single.feeds.borrow(), &["长报告已发出"]);
    assert_eq!(single.payloads.borrow()[1], "第二段正文", "标签不进末段");

    let summary = EmitterFrontend::new();
    summary.split_reply_at.set(Some("第一段正文".len()));
    let segments = vec![
        summon_core::segment::TurnSegment::Thinking("思考".into()),
        summon_core::segment::TurnSegment::Reply(reply.into()),
    ];
    finalize_turn_summary(
        &summary,
        &chat,
        "tester",
        &mut None,
        &segments,
        &segments,
        Some(reply),
        None,
        true,
        None,
        None,
        false,
    )
    .expect("有 thinking 和 reply 时产生总结卡");
    assert_eq!(&*summary.feeds.borrow(), &["长报告已发出"]);
    assert_eq!(summary.settled_replies.get(), 1);
}

#[test]
fn only_the_last_of_three_segments_carries_the_agent_feed() {
    struct ThreeWay(EmitterFrontend);
    impl Frontend for ThreeWay {
        fn split_final_reply(&self, _chat: &ChatRef, body: &str) -> Vec<String> {
            let (a, rest) = body.split_at("一段".len());
            let (b, c) = rest.split_at("二段".len());
            vec![a.into(), b.into(), c.into()]
        }
        fn send_message(&self, chat: &ChatRef, body: &str) -> anyhow::Result<()> {
            self.0.send_message(chat, body)
        }
        fn send_message_with_feed(
            &self,
            chat: &ChatRef,
            body: &str,
            feed: &str,
        ) -> anyhow::Result<()> {
            self.0.send_message_with_feed(chat, body, feed)
        }
        fn send_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<()> {
            self.0.send_card(chat, card_json)
        }
        fn stream_begin(
            &self,
            chat: &ChatRef,
            content: &str,
            with_interrupt: bool,
        ) -> anyhow::Result<StreamId> {
            self.0.stream_begin(chat, content, with_interrupt)
        }
        fn stream_patch(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
            self.0.stream_patch(id, content)
        }
        fn react(&self, msg: &MsgRef, key: &str) -> anyhow::Result<String> {
            self.0.react(msg, key)
        }
        fn unreact(&self, msg: &MsgRef, reaction_id: &str) -> anyhow::Result<()> {
            self.0.unreact(msg, reaction_id)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_feed_three");
    let frontend = ThreeWay(EmitterFrontend::new());
    deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut None,
        &mut false,
        "一段二段三段\n<summon-feed>三段已发</summon-feed>",
        Some(&quota_badge()),
        false,
    );
    assert_eq!(&*frontend.0.calls.borrow(), &["card", "send", "send"]);
    assert_eq!(&*frontend.0.feeds.borrow(), &["三段已发"], "只有末段带");
    assert_eq!(frontend.0.payloads.borrow()[2], "三段");
}

#[test]
fn process_only_summary_keeps_the_context_budget_out_of_the_cot() {
    let frontend = EmitterFrontend::new();
    frontend.process_only.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_cot_summary",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let segments = vec![
        summon_core::segment::TurnSegment::Thinking("过程".into()),
        summon_core::segment::TurnSegment::Reply("结论".into()),
    ];
    let mut stream_id = Some(StreamId(11));
    let badge = summon_core::feishu::TurnBadge {
        ctx: summon_core::context_budget::ContextBudget::from_tokens(50_000, Some(100_000)),
        ..Default::default()
    };
    let report = finalize_turn_summary(
        &frontend,
        &chat,
        "tester",
        &mut stream_id,
        &segments,
        &segments,
        Some("结论"),
        None,
        true,
        Some(&badge),
        None,
        false,
    )
    .unwrap();

    assert_eq!(&*frontend.calls.borrow(), &["finish_text", "card"]);
    // 上下文预算只出现在总结卡的角标行:它不是思考内容,而 CoT 收口后紧跟着的这张卡本来就渲它。
    let payloads = frontend.payloads.borrow().clone();
    let (card, rest) = payloads.split_last().expect("至少有总结卡");
    assert!(
        rest.iter().all(|payload| !payload.contains("50%")),
        "CoT 过程里不得出现上下文预算:{rest:?}"
    );
    assert_compact_summary_footer(card, Some("50k/100k █████░░░░░ 50%"));
    assert_eq!(stream_id, None);
    assert!(report.complete);
}

/// 仅结果模式(定时器 `output: result`)的终态卡同样要带角标:缓冲前端只吸收过程卡,
/// 总结卡是它唯一放行的出站,角标行必须原样穿过去。
#[test]
fn result_only_summary_card_carries_the_badge_after_flush() {
    let inner = EmitterFrontend::new();
    let buffered = crate::frontend::ResultOnlyFrontend::new(&inner);
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_sched_result");
    let segments = vec![
        summon_core::segment::TurnSegment::Thinking("复查触发线".into()),
        summon_core::segment::TurnSegment::Reply("无事".into()),
    ];
    let mut stream_id = Some(StreamId(21));

    finalize_turn_summary(
        &buffered,
        &chat,
        "tester",
        &mut stream_id,
        &segments,
        &segments,
        Some("无事"),
        None,
        true,
        Some(&quota_badge()),
        None,
        false,
    )
    .expect("仅结果模式恒走总结卡");
    buffered.flush(&chat, crate::frontend::ResultOnlyOutcome::Completed);

    let card = inner
        .payloads
        .borrow()
        .last()
        .cloned()
        .expect("flush 必须把缓冲的总结卡交给真实前端");
    assert!(card.contains("无事"), "正文必须原样在卡里: {card}");
    assert_compact_summary_footer(&card, Some("claude · Opus 5"));
}

/// 贴 / 撤表情、回复原消息一律走 `ChatTask::feishu_anchor()`,**不许直接拿 `task.message_id`**。
///
/// 那个字段是本地幂等键:恢复重放轮里它是合成的 `recovery-run-…`,定时 / sentinel 轮是
/// `fire-…`。拿它调飞书一律 400(`99992354 not a valid open_message_id`),而失败全是
/// log-ignore —— 于是表现不是报错,是「表情没了、总结卡走了 outbox 重试、原生 CoT 悄悄退回
/// 卡片形态」。这条守卫钉的就是「别再写回去」。
#[test]
fn feishu_reactions_and_replies_never_take_the_raw_task_message_id() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime/daemon/chat_thread.rs"),
    )
    .expect("读 chat_thread.rs 失败");
    let production = src.split("\n#[cfg(test)]").next().unwrap_or_default();
    let flat: String = production.chars().filter(|c| !c.is_whitespace()).collect();
    for pattern in [
        "react(&task.message_id",
        "unreact(&task.message_id",
        "message_id:&task.message_id",
    ] {
        assert!(
            !flat.contains(pattern),
            "{pattern}:飞书调用要用 task.feishu_anchor(),裸 message_id 在恢复/定时轮上是合成 id"
        );
    }
}

/// 角标种子在每个 turn 入口都必须无条件建。
///
/// 仅结果模式(定时器 `output: result`)约束的是**不推过程**,而角标是终态卡上的一行,
/// 不在此列;要压制的上下文提醒另有 `maybe_warn_context` 开头那道闸,与角标无关。
/// 种子一旦缺失,角标退化成只剩恒有值的时刻,而且不报错、不留日志 —— 靠肉眼看卡片
/// 才发现,所以在源码这层钉死。
#[test]
fn badge_seed_is_built_unconditionally_at_every_turn_entry() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime/daemon/chat_thread.rs"),
    )
    .expect("读 chat_thread.rs 失败");
    // 去掉全部空白再匹配:调用点的实参一多 rustfmt 就会换行,而这条守卫要钉的是「有没有建种子」,
    // 不是它排成几行。带逗号的实参形态只匹配调用点,定义那行去空白后是 `badge_seed_for(agent:&Agent`,
    // 不会被算进来。
    let flat: String = src.chars().filter(|c| !c.is_whitespace()).collect();
    let calls = flat.matches("badge_seed_for(agent,").count();
    let unconditional = flat.matches("Some(badge_seed_for(agent,").count();
    assert!(
        calls >= 3,
        "角标种子调用点少于三处(claude 转录 + 无头可打断 + 无头同步),先确认是不是被删了: {calls}"
    );
    assert_eq!(
        unconditional, calls,
        "有调用点把角标种子做成了条件式,该条路上的轮会退化成只剩时间戳的角标"
    );
}

#[test]
fn file_tool_language_survives_the_production_projector_into_cot_result() {
    use summon_core::agent_msg::{AgentKind, AgentMessage};

    fn project(path: &str) -> serde_json::Value {
        let frontend = EmitterFrontend::new();
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_file_language");
        let mut projector = TurnProjector::new(
            CotEmitter::new(None),
            summon_core::progress::TurnProgress::new(),
            None,
        );
        let mut started = AgentMessage::of(AgentKind::ToolUse);
        started.tool = Some("Read".into());
        started.tool_id = Some("tool-source-1".into());
        started.tool_input = Some(serde_json::json!({"file_path": path}));
        projector.project_event(&frontend, &chat, "tester", &started);

        let mut finished = AgentMessage::of(AgentKind::ToolResult);
        finished.tool_id = Some("tool-source-1".into());
        finished.tool_result = Some(serde_json::Value::String("sample content".into()));
        projector.project_event(&frontend, &chat, "tester", &finished);

        let mut cot = summon_core::cot::CotProjector::new("chat-1", "turn-1", "读取文件");
        let events = cot.project(&projector.segments, Some(&projector.progress), "1000");
        let result = events
            .iter()
            .find(|event| event.event_type == "TOOL_CALL_RESULT")
            .expect("生产投影器应把工具结果交给 CoT");
        serde_json::from_str::<serde_json::Value>(&result.content).unwrap()["content"].clone()
    }

    let rust = project("src/lib.rs");
    assert_eq!(rust["type"], "code");
    assert_eq!(rust["language"], "rust");
    let markdown = project("README.md");
    assert_eq!(markdown["type"], "code");
    assert_eq!(markdown["language"], "markdown");
    assert!(project("artifact.h").is_string(), "不确定的 .h 不得猜语言");
    assert!(
        project("artifact.custom").is_string(),
        "未知扩展名应保持 plaintext"
    );
}

fn process_reply_event(
    kind: summon_core::agent_msg::AgentKind,
    text: &str,
) -> summon_core::agent_msg::AgentMessage {
    let mut event = summon_core::agent_msg::AgentMessage::of(kind);
    event.text = Some(text.into());
    event
}

fn process_tool_events() -> [summon_core::agent_msg::AgentMessage; 2] {
    use summon_core::agent_msg::{AgentKind, AgentMessage};
    let mut started = AgentMessage::of(AgentKind::ToolUse);
    started.tool = Some("Bash".into());
    started.tool_id = Some("call-1".into());
    started.tool_input = Some(serde_json::json!({"command": "ls"}));
    let mut finished = AgentMessage::of(AgentKind::ToolResult);
    finished.tool_id = Some("call-1".into());
    [started, finished]
}

fn cot_projector(stream: Option<StreamId>) -> TurnProjector {
    TurnProjector::new(
        CotEmitter::new(stream),
        summon_core::progress::TurnProgress::new().with_turn_id("t-process-reply"),
        None,
    )
}

/// 把 live 卡交给真实 `CotProjector` 投影、记下全部 CoT 事件的前端:断言「CoT 上到底有什么」。
#[derive(Default)]
struct CotRecorder {
    projector: std::cell::RefCell<Option<summon_core::cot::CotProjector>>,
    events: std::cell::RefCell<Vec<summon_core::cot::CotEvent>>,
    sent: std::cell::RefCell<Vec<String>>,
    append_failed: std::cell::Cell<bool>,
    confirmed: std::cell::RefCell<Vec<summon_core::cot::ReplyDelivery>>,
    supplements: std::cell::RefCell<Vec<String>>,
    fail_supplement: std::cell::Cell<bool>,
    fail_supplement_after: std::cell::Cell<Option<usize>>,
    flush_fails: std::cell::Cell<bool>,
    send_attempts: std::cell::Cell<usize>,
    fail_send_after: std::cell::Cell<Option<usize>>,
    split_at: std::cell::Cell<Option<usize>>,
}

impl CotRecorder {
    fn project(&self, view: &summon_core::cot::CotView) {
        let mut projector = self.projector.borrow_mut();
        let projector = projector
            .get_or_insert_with(|| summon_core::cot::CotProjector::new("chat", "turn", "任务"));
        let events = projector.project_with_replies(
            view.segments,
            view.released_replies,
            view.progress,
            "1000",
        );
        if !self.append_failed.get() && !self.flush_fails.get() {
            for reply in events.iter().filter_map(|event| event.reply_delivery) {
                let mut confirmed = self.confirmed.borrow_mut();
                if let Some(old) = confirmed
                    .iter_mut()
                    .find(|old| old.generation == reply.generation && old.segment == reply.segment)
                {
                    old.bytes = old.bytes.max(reply.bytes);
                } else {
                    confirmed.push(reply);
                }
            }
        }
        self.events.borrow_mut().extend(events);
    }

    fn pushed(&self) -> String {
        self.events
            .borrow()
            .iter()
            .map(|event| event.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// CoT 里的过程回复文本块,按出现顺序。
    fn text_blocks(&self) -> Vec<String> {
        let mut blocks: Vec<(String, String)> = Vec::new();
        for event in self.events.borrow().iter() {
            let content: serde_json::Value = serde_json::from_str(&event.content).unwrap();
            let id = content["messageId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            match event.event_type.as_str() {
                "TEXT_MESSAGE_START" => blocks.push((id, String::new())),
                "TEXT_MESSAGE_CONTENT" => {
                    let block = blocks.iter_mut().find(|(block, _)| *block == id).unwrap();
                    block.1.push_str(content["delta"].as_str().unwrap());
                }
                _ => {}
            }
        }
        blocks.into_iter().map(|(_, text)| text).collect()
    }

    /// 某段文字第一次出现在第几个 CoT 事件里。
    fn position_of(&self, needle: &str) -> usize {
        self.events
            .borrow()
            .iter()
            .position(|event| event.content.contains(needle))
            .unwrap_or_else(|| panic!("CoT 里没有「{needle}」:{}", self.pushed()))
    }
}

impl Frontend for CotRecorder {
    fn send_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<()> {
        self.send_message(chat, card_json)
    }
    fn split_final_reply(&self, _chat: &ChatRef, body: &str) -> Vec<String> {
        match self.split_at.get() {
            Some(at) if at < body.len() => vec![body[..at].to_string(), body[at..].to_string()],
            _ => vec![body.to_string()],
        }
    }
    fn send_message(&self, _chat: &ChatRef, body: &str) -> anyhow::Result<()> {
        let attempt = self.send_attempts.get();
        self.send_attempts.set(attempt + 1);
        if self
            .fail_send_after
            .get()
            .is_some_and(|limit| attempt >= limit)
        {
            anyhow::bail!("send failed");
        }
        self.sent.borrow_mut().push(body.to_string());
        Ok(())
    }
    fn stream_begin(&self, _: &ChatRef, _: &str, _: bool) -> anyhow::Result<StreamId> {
        Ok(StreamId(1))
    }
    fn stream_patch(&self, _: StreamId, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn live_begin(
        &self,
        _chat: &ChatRef,
        view: &summon_core::cot::CotView,
    ) -> anyhow::Result<StreamId> {
        self.project(view);
        Ok(StreamId(1))
    }
    fn live_update(
        &self,
        _id: StreamId,
        view: &summon_core::cot::CotView,
    ) -> anyhow::Result<crate::frontend::StreamPatchOutcome> {
        self.project(view);
        Ok(crate::frontend::StreamPatchOutcome::Applied)
    }
    fn stream_is_process_only(&self, _id: StreamId) -> bool {
        true
    }
    fn stream_confirmed_replies(
        &self,
        _id: StreamId,
    ) -> Option<Vec<summon_core::cot::ReplyDelivery>> {
        Some(self.confirmed.borrow().clone())
    }
    fn send_process_supplement(
        &self,
        _chat: &ChatRef,
        card: &str,
        _uuid: &str,
    ) -> anyhow::Result<()> {
        if self.fail_supplement.get()
            || self
                .fail_supplement_after
                .get()
                .is_some_and(|limit| self.supplements.borrow().len() >= limit)
        {
            anyhow::bail!("supplement failed");
        }
        self.supplements.borrow_mut().push(card.to_string());
        Ok(())
    }
    fn stream_append_failed(&self, _id: StreamId) -> bool {
        self.append_failed.get()
    }
    fn stream_flush(&self, _id: StreamId) -> anyhow::Result<()> {
        if self.flush_fails.get() {
            self.append_failed.set(true);
            anyhow::bail!("cot push failed");
        }
        Ok(())
    }
    fn stream_steer_boundary(&self, _id: StreamId, note: &str) -> anyhow::Result<()> {
        let mut projector = self.projector.borrow_mut();
        if let Some(projector) = projector.as_mut() {
            self.events
                .borrow_mut()
                .extend(projector.steer_boundary(note, "1000"));
        }
        Ok(())
    }
    fn react(&self, _: &crate::frontend::MsgRef, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn unreact(&self, _: &crate::frontend::MsgRef, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

fn run_events(
    frontend: &dyn Frontend,
    chat: &ChatRef,
    projector: &mut TurnProjector,
    events: &[summon_core::agent_msg::AgentMessage],
) {
    for event in events {
        projector.project_event(frontend, chat, "tester", event);
    }
    // 事件路的渲染有节流;收一次尾,模拟节流窗口过后的那次刷新。
    projector.live.emit(
        frontend,
        chat,
        "tester",
        &projector.segments,
        Some(&projector.progress),
    );
}

/// 画图群 2026-10-12 的形状:答复写完后又被并入同一轮的通知拉着调工具、补一句说明。
/// 答复在工具开始那一刻确认属于过程,进 CoT 成为文本块;终态只剩补充说明。
#[test]
fn reply_followed_by_a_tool_goes_into_the_cot_and_out_of_the_final() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply");
    let mut projector = cot_projector(None);

    projector.project_event(
        &frontend,
        &chat,
        "tester",
        &process_reply_event(AgentKind::Assistant, "两件事都做完了。"),
    );
    projector.live.emit(
        &frontend,
        &chat,
        "tester",
        &projector.segments,
        Some(&projector.progress),
    );
    assert!(
        frontend.text_blocks().is_empty(),
        "末段可能就是终态,确认前扣住:{}",
        frontend.pushed()
    );
    run_events(&frontend, &chat, &mut projector, &process_tool_events());
    projector.project_event(
        &frontend,
        &chat,
        "tester",
        &process_reply_event(AgentKind::Assistant, "这条通知不需要处理。"),
    );

    assert_eq!(frontend.text_blocks(), ["两件事都做完了。"]);
    assert!(frontend.sent.borrow().is_empty(), "过程回复不另发消息");
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("这条通知不需要处理。"))
            .as_deref(),
        Some("这条通知不需要处理。")
    );
}

/// 跨后台任务续跑的段终点回复:立刻作为独立回复发出、CoT 不收口;续跑之后它不再作为过程
/// 回复进 CoT,轮末终态也只剩续跑之后的回复。
#[test]
fn continuation_reply_is_sent_on_its_own_and_kept_out_of_the_cot_and_final() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_interim_reply");
    let mut projector = cot_projector(None);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Thinking, "先起个后台任务"),
            process_reply_event(AgentKind::Assistant, "还连不上,我在后台继续试。"),
        ],
    );

    let report = projector
        .deliver_interim_reply(
            &frontend,
            &chat,
            "tester",
            "还连不上,我在后台继续试。",
            None,
        )
        .expect("段终点回复要发出");
    assert!(report.complete);
    assert_eq!(*frontend.sent.borrow(), ["还连不上,我在后台继续试。"]);

    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Thinking, "通知到了"),
            process_reply_event(AgentKind::Assistant, "后台任务结束了,还是失败。"),
        ],
    );
    run_events(&frontend, &chat, &mut projector, &process_tool_events());
    assert_eq!(
        frontend.text_blocks(),
        ["后台任务结束了,还是失败。"],
        "续跑接着进同一条 CoT,发过的回复不再进:{}",
        frontend.pushed()
    );
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("后台任务结束了,还是失败。"))
            .as_deref(),
        Some("后台任务结束了,还是失败。")
    );
}

/// 这一段说过的话在思考收尾前都已作为过程回复进了 CoT:不再另发一遍。CoT 关闭时没有过程
/// 展示,照发捕获层给的正文。
#[test]
fn continuation_reply_already_in_the_cot_is_not_sent_again() {
    use summon_core::agent_msg::AgentKind;

    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_interim_reply_in_cot");
    let events = [
        process_reply_event(AgentKind::Assistant, "还连不上,我在后台继续试。"),
        process_reply_event(AgentKind::Thinking, "等通知"),
    ];

    let frontend = CotRecorder::default();
    let mut projector = cot_projector(None);
    run_events(&frontend, &chat, &mut projector, &events);
    assert_eq!(frontend.text_blocks(), ["还连不上,我在后台继续试。"]);
    assert!(projector
        .deliver_interim_reply(
            &frontend,
            &chat,
            "tester",
            "还连不上,我在后台继续试。",
            None
        )
        .is_none());
    assert!(frontend.sent.borrow().is_empty());

    let hidden = CotRecorder::default();
    let mut projector = cot_projector(None).with_cot_visible(false);
    run_events(&hidden, &chat, &mut projector, &events);
    assert!(projector
        .deliver_interim_reply(&hidden, &chat, "tester", "还连不上,我在后台继续试。", None)
        .is_some());
    assert_eq!(*hidden.sent.borrow(), ["还连不上,我在后台继续试。"]);
}

/// 仅结果模式只交终态,不发段终点回复。
#[test]
fn continuation_reply_is_held_for_result_only() {
    use summon_core::agent_msg::AgentKind;

    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_interim_reply_held");
    let reply = [process_reply_event(AgentKind::Assistant, "还在跑。")];

    let inner = CotRecorder::default();
    let buffered = crate::frontend::ResultOnlyFrontend::new(&inner);
    let mut projector = cot_projector(Some(StreamId(1)));
    run_events(&buffered, &chat, &mut projector, &reply);
    assert!(projector
        .deliver_interim_reply(&buffered, &chat, "tester", "还在跑。", None)
        .is_none());
    assert!(inner.sent.borrow().is_empty());
}

/// 接管重放的轮不收过程回复,段终点回复作为独立回复发出(捕获层只交出接管之后写出的段终点),
/// 不进 CoT;轮末终态只带续跑之后的回复。
#[test]
fn continuation_reply_of_a_reattached_turn_is_sent_on_its_own() {
    use summon_core::agent_msg::AgentKind;

    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_interim_reply_reattached");
    let frontend = CotRecorder::default();
    let mut projector = TurnProjector::new(
        CotEmitter::new(None),
        summon_core::progress::TurnProgress::new().with_turn_id("t-reattached"),
        None,
    )
    .with_process_replies(false);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Thinking, "起了后台任务"),
            process_reply_event(AgentKind::Assistant, "素材还在后台生成。"),
        ],
    );

    let report = projector
        .deliver_interim_reply(&frontend, &chat, "tester", "素材还在后台生成。", None)
        .expect("接管之后写出的段终点回复要发出");
    assert!(report.complete);
    assert_eq!(*frontend.sent.borrow(), ["素材还在后台生成。"]);
    assert!(
        frontend.text_blocks().is_empty(),
        "接管轮不收过程回复,段终点回复不进 CoT:{}",
        frontend.pushed()
    );

    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[process_reply_event(AgentKind::Assistant, "素材生成完了。")],
    );
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("素材生成完了。"))
            .as_deref(),
        Some("素材生成完了。"),
        "轮末终态只带续跑之后的回复"
    );
}

/// 新的思考段与新的回复段同样宣告上一段回复已完结;文本块按发生顺序夹在思考之间。
#[test]
fn thinking_or_a_new_reply_after_a_reply_releases_the_earlier_reply() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_kinds");
    let mut projector = cot_projector(None);
    let events: Vec<_> = [
        (AgentKind::Assistant, "第一段说明。"),
        (AgentKind::Thinking, "接着想"),
        (AgentKind::Assistant, "第二段说明。"),
        (AgentKind::Assistant, "第三段说明。"),
    ]
    .into_iter()
    .map(|(kind, text)| process_reply_event(kind, text))
    .collect();
    run_events(&frontend, &chat, &mut projector, &events);

    assert_eq!(frontend.text_blocks(), ["第一段说明。", "第二段说明。"]);
    assert!(frontend.position_of("第一段说明") < frontend.position_of("接着想"));
    assert!(frontend.position_of("接着想") < frontend.position_of("第二段说明"));
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("第三段说明。"))
            .as_deref(),
        Some("第三段说明。")
    );
}

/// 回复全都进了 CoT(说完最后一句又调了工具)时,终态仍然要有:取最后一段回复。
#[test]
fn turn_whose_replies_are_all_in_the_cot_still_ends_with_the_last_reply() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_all_released");
    let mut projector = cot_projector(None);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Thinking, "先想想"),
            process_reply_event(AgentKind::Assistant, "改完了,再跑一遍测试。"),
        ],
    );
    run_events(&frontend, &chat, &mut projector, &process_tool_events());

    assert_eq!(frontend.text_blocks(), ["改完了,再跑一遍测试。"]);
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("改完了,再跑一遍测试。"))
            .as_deref(),
        Some("改完了,再跑一遍测试。")
    );
}

/// 仅结果模式只交付终态;旧卡片过程卡自己渲染回复。两者都不放行过程回复,终态照旧。
#[test]
fn result_only_and_legacy_cards_do_not_release_process_replies() {
    use summon_core::agent_msg::AgentKind;

    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_skip");
    let mut events = vec![process_reply_event(AgentKind::Assistant, "先看一下目录。")];
    events.extend(process_tool_events());

    let inner = CotRecorder::default();
    let buffered = crate::frontend::ResultOnlyFrontend::new(&inner);
    let mut projector = cot_projector(Some(StreamId(1)));
    run_events(&buffered, &chat, &mut projector, &events);
    assert!(inner.text_blocks().is_empty());
    assert_eq!(
        projector.live.final_reply_body(&projector.segments, None),
        None
    );

    let legacy = EmitterFrontend::new();
    let mut projector = cot_projector(Some(StreamId(1)));
    run_events(&legacy, &chat, &mut projector, &events);
    assert_eq!(
        projector.live.final_reply_body(&projector.segments, None),
        None
    );
}

/// cursor 2026-09-09 真机形态:主回复流完 → 思考 → 补一句说明 → 迟到补发主回复的完整快照
/// (换行折成了空格)。主回复只进 CoT 一次,终态只剩那句说明。
#[test]
fn late_snapshot_of_an_earlier_reply_is_not_shown_again() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_late_snapshot");
    let mut projector = cot_projector(None);
    let mut answer =
        process_reply_event(AgentKind::Assistant, "能聚合。\n\nSentry 按 top frame 聚。");
    answer.partial = true;
    let mut note = process_reply_event(AgentKind::Assistant, "这里不再重复。");
    note.partial = true;
    let snapshot = process_reply_event(AgentKind::Assistant, "能聚合。  Sentry 按 top frame 聚。");
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            answer,
            process_reply_event(AgentKind::Thinking, "通知到了"),
            note,
            snapshot,
        ],
    );

    assert_eq!(
        frontend.text_blocks(),
        ["能聚合。\n\nSentry 按 top frame 聚。"]
    );
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("这里不再重复。"))
            .as_deref(),
        Some("这里不再重复。")
    );
}

/// 过程回复之后的思考与工具照常进 CoT,且落在文本块之后。回复要长到触发一次渲染:那时它已经
/// 在过程展示的坐标里占了位,放行不能把后面段的位置挪走。
#[test]
fn thinking_after_a_process_reply_still_reaches_the_cot() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_cot");
    let mut projector = cot_projector(None);
    let [started, finished] = process_tool_events();
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Thinking, "思考一"),
            process_reply_event(
                AgentKind::Assistant,
                "先把目录列出来,看看两个项目各自的原始素材和产物都放在哪里,再决定怎么挪。",
            ),
            started.clone(),
            finished.clone(),
            process_reply_event(AgentKind::Thinking, "思考二"),
            started,
            finished,
        ],
    );

    assert!(frontend.sent.borrow().is_empty());
    assert_eq!(frontend.text_blocks().len(), 1, "{}", frontend.pushed());
    assert!(frontend.position_of("思考一") < frontend.position_of("先把目录列出来"));
    assert!(frontend.position_of("先把目录列出来") < frontend.position_of("思考二"));
}

/// 接管重放的轮不放行过程回复:重放出来的回复说不清上一代交没交过。
#[test]
fn reattached_replay_keeps_replies_out_of_the_cot() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_reattach");
    let mut projector = cot_projector(None).with_process_replies(false);
    let mut events = vec![
        process_reply_event(AgentKind::Assistant, "上一代已经说过的话。"),
        process_reply_event(AgentKind::Thinking, "接着想"),
    ];
    events.extend(process_tool_events());
    run_events(&frontend, &chat, &mut projector, &events);

    assert!(frontend.text_blocks().is_empty());
    assert!(frontend.pushed().contains("接着想"), "回复之后的思考照投");
    assert_eq!(
        projector.live.final_reply_body(&projector.segments, None),
        None
    );
}

/// claude 的回复块各自完整到达:同一句话隔着思考说两次是两条回复,不是迟到快照。
#[test]
fn identical_discrete_replies_are_both_kept() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_identical");
    let mut projector = cot_projector(None);
    let events: Vec<_> = [
        (AgentKind::Assistant, "好的。"),
        (AgentKind::Thinking, "想想"),
        (AgentKind::Assistant, "好的。"),
    ]
    .into_iter()
    .map(|(kind, text)| process_reply_event(kind, text))
    .collect();
    run_events(&frontend, &chat, &mut projector, &events);

    assert_eq!(frontend.text_blocks(), ["好的。"]);
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("好的。"))
            .as_deref(),
        Some("好的。")
    );
}

/// `/cot off` 只看终态:过程中说的话不进 CoT、不另发,终态照旧按权威正文组装。
#[test]
fn hidden_cot_shows_only_the_final_reply() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_hidden");
    let mut projector = cot_projector(None).with_cot_visible(false);
    let mut events = vec![
        process_reply_event(AgentKind::Thinking, "不应外显的推理"),
        process_reply_event(AgentKind::Assistant, "先看一下目录。"),
    ];
    events.extend(process_tool_events());
    events.push(process_reply_event(AgentKind::Assistant, "做完了。"));
    run_events(&frontend, &chat, &mut projector, &events);

    assert!(frontend.text_blocks().is_empty());
    assert!(frontend.sent.borrow().is_empty());
    assert!(!frontend.pushed().contains("先看一下目录"));
    assert_eq!(
        projector.live.final_reply_body(&projector.segments, None),
        None
    );
}

/// 追加消息的段边界之前说完的话属于上一个问题:放行后先投进 CoT,落在边界之前。
#[test]
fn reply_before_a_steer_boundary_lands_before_the_boundary() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_steer");
    let mut projector = cot_projector(None);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Thinking, "先想想"),
            process_reply_event(AgentKind::Assistant, "第一个问题的答复。"),
        ],
    );
    projector.steer_boundary(&frontend, &chat, "tester");
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[process_reply_event(AgentKind::Thinking, "处理追加")],
    );

    assert_eq!(frontend.text_blocks(), ["第一个问题的答复。"]);
    let note = "收到追加消息";
    assert!(frontend.position_of("第一个问题的答复") < frontend.position_of(note));
    assert!(frontend.position_of(note) < frontend.position_of("处理追加"));
}

/// 放行界按原始段下标记,过程视图里的段是按卡片预算切过块的:超长回复被切成多块后,
/// 放行时每一块都要投进 CoT,终态也不再带它。
#[test]
fn oversized_process_reply_reaches_the_cot_whole() {
    use summon_core::agent_msg::AgentKind;

    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_chunked");
    let mut projector = cot_projector(None);
    let long_reply = "答".repeat(summon_core::feishu::CARD_TEXT_SAFE_LIMIT * 2);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[process_reply_event(AgentKind::Thinking, "开头")],
    );
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Assistant, &long_reply),
            process_reply_event(AgentKind::Thinking, "接着想"),
            process_reply_event(AgentKind::Assistant, "收尾。"),
        ],
    );

    let blocks = frontend.text_blocks();
    assert_eq!(blocks.len(), 1, "过程回复使用同一个 TEXT_MESSAGE");
    assert_eq!(blocks.concat(), long_reply);
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("收尾。"))
            .as_deref(),
        Some("收尾。")
    );
}

/// 尚未确认的过程说明使用独立补充，最终正文只保留结论。
#[test]
fn process_replies_pending_at_the_last_flush_are_supplemented_separately() {
    use summon_core::agent_msg::AgentKind;
    let frontend = CotRecorder::default();
    frontend.flush_fails.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_flush_failed");
    let mut projector = cot_projector(None);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Assistant, "第一段说明。"),
            process_reply_event(AgentKind::Thinking, "接着想"),
            process_reply_event(AgentKind::Assistant, "收尾。"),
        ],
    );
    projector.deliver_process_supplement(
        &frontend,
        &chat,
        "tester",
        &test_outbox(dir.path()),
        Some("收尾。"),
    );
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("收尾。"))
            .as_deref(),
        Some("收尾。")
    );
    let cards = frontend.supplements.borrow();
    assert_eq!(cards.len(), 1);
    let card: serde_json::Value = serde_json::from_str(&cards[0]).unwrap();
    assert_eq!(
        card["body"]["elements"][0]["elements"][0]["content"],
        "第一段说明。"
    );
}

#[test]
fn authoritative_final_text_survives_released_process_replies_and_delivery_failures() {
    use summon_core::agent_msg::AgentKind;
    for summary in [false, true] {
        for cot_failed in [false, true] {
            for delivery_failed in [false, true] {
                for (last, body, good) in [
                    (
                        None,
                        "前半,以及只在终态全文里的尾巴",
                        "前半,以及只在终态全文里的尾巴",
                    ),
                    (None, "独立终态", "独立终态"),
                    (Some("末段"), "前半\n\n末段,还有尾巴", "末段,还有尾巴"),
                    (Some("末段"), "独立终态", "独立终态"),
                ] {
                    let frontend = CotRecorder::default();
                    frontend.flush_fails.set(cot_failed);
                    frontend.fail_send_after.set(delivery_failed.then_some(0));
                    let dir = tempfile::tempdir().unwrap();
                    let chat = badge_chat(dir.path(), "oc_authoritative_final");
                    let outbox = test_outbox(dir.path());
                    let (out, _) = card_stream_consume(
                        &frontend,
                        &chat,
                        summary,
                        summon_core::progress::DEFAULT_MAX_STEPS,
                        false,
                        false,
                        None,
                        None,
                        None,
                        &outbox,
                        &|| None,
                        None,
                        &mut |_: &[String]| {},
                        |on_msg, _, _, on_event, _, _, _| {
                            on_event(&process_reply_event(AgentKind::Assistant, "前半"));
                            on_event(&process_reply_event(
                                AgentKind::Thinking,
                                &"继续思考。".repeat(12),
                            ));
                            if let Some(last) = last {
                                on_event(&process_reply_event(AgentKind::Assistant, last));
                            }
                            assert_eq!(frontend.text_blocks(), ["前半"], "过程回复必须已投进 CoT");
                            on_msg(body);
                            Ok(crate::capture::TurnOutcome {
                                reply: Some(body.into()),
                                ..Default::default()
                            })
                        },
                    );
                    assert!(out.is_ok());
                    let expected = good;
                    let supplements = frontend.supplements.borrow();
                    let should_supplement = cot_failed && (last.is_some() || body == "独立终态");
                    assert_eq!(supplements.len(), usize::from(should_supplement));
                    if should_supplement {
                        let card: serde_json::Value =
                            serde_json::from_str(&supplements[0]).unwrap();
                        assert_eq!(
                            card["body"]["elements"][0]["elements"][0]["content"],
                            "前半"
                        );
                    }
                    let case = format!("summary={summary} cot_failed={cot_failed} delivery_failed={delivery_failed} body={body}");
                    if delivery_failed {
                        let events = outbox.pending_events();
                        assert_eq!(events.len(), 1, "{case}");
                        let summon_core::outbox::OutboxKind::TurnReply { text, .. } =
                            &events[0].kind
                        else {
                            panic!("回复重试事件");
                        };
                        assert_eq!(text, expected, "{case}");
                    } else {
                        let sent = frontend.sent.borrow();
                        assert_eq!(sent.len(), 1, "{case}");
                        let actual = if summary {
                            let card: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
                            card["body"]["elements"][0]["content"]
                                .as_str()
                                .unwrap()
                                .to_string()
                        } else {
                            sent[0].clone()
                        };
                        assert_eq!(actual, expected, "{case}");
                        assert!(outbox.pending_events().is_empty(), "{case}");
                    }
                }
            }
        }
    }
}

#[test]
fn timeout_retry_preserves_the_exact_partial_reply_without_reassembly() {
    use summon_core::agent_msg::AgentKind;
    for cot_failed in [false, true] {
        for delivery_failed in [false, true] {
            let frontend = CotRecorder::default();
            frontend.flush_fails.set(cot_failed);
            frontend.fail_send_after.set(delivery_failed.then_some(0));
            let dir = tempfile::tempdir().unwrap();
            let chat = badge_chat(dir.path(), "oc_timeout_exact_reply");
            let outbox = test_outbox(dir.path());
            let (out, _) = card_stream_consume(
                &frontend,
                &chat,
                true,
                summon_core::progress::DEFAULT_MAX_STEPS,
                false,
                false,
                None,
                None,
                None,
                &outbox,
                &|| None,
                None,
                &mut |_: &[String]| {},
                |_, _, _, on_event, _, _, _| {
                    on_event(&process_reply_event(AgentKind::Assistant, "前半"));
                    on_event(&process_reply_event(
                        AgentKind::Thinking,
                        &"继续思考。".repeat(12),
                    ));
                    on_event(&process_reply_event(AgentKind::Assistant, "末段"));
                    Ok(crate::capture::TurnOutcome {
                        reply: Some("前半\n\n末段".into()),
                        timed_out: true,
                        ..Default::default()
                    })
                },
            );
            assert!(out.unwrap().timed_out);
            let expected = format!("⚠️ 以下是超时前的部分输出：\n\n{}", "末段");
            if delivery_failed {
                let events = outbox.pending_events();
                assert_eq!(events.len(), 1);
                let summon_core::outbox::OutboxKind::TurnReply { text, .. } = &events[0].kind
                else {
                    panic!("回复重试事件");
                };
                assert_eq!(text, &expected);
            } else {
                assert_eq!(*frontend.sent.borrow(), [expected]);
                assert!(outbox.pending_events().is_empty());
            }
        }
    }
}

fn generation_before_a_cursor_restart(frontend: &CotRecorder, chat: &ChatRef) -> TurnProjector {
    use summon_core::agent_msg::AgentKind;
    let mut projector = cot_projector(None);
    run_events(
        frontend,
        chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Assistant, "第一段说明。"),
            process_reply_event(AgentKind::Thinking, "接着想"),
            process_reply_event(AgentKind::Assistant, "收到,先看这个。"),
        ],
    );
    projector
}

/// Cursor steer 续跑:这一代没有终态消息,说完的话(含末段)投进 CoT,不另发消息。
#[test]
fn cursor_restart_hands_the_generation_replies_to_a_working_cot() {
    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_restart_cot_ok");
    let mut projector = generation_before_a_cursor_restart(&frontend, &chat);

    projector.hand_off_generation_replies(&frontend, &chat, "tester", &test_outbox(dir.path()));

    assert_eq!(frontend.text_blocks(), ["第一段说明。", "收到,先看这个。"]);
    assert!(frontend.sent.borrow().is_empty(), "CoT 收下了就不另发");
}

/// 重跑前已确认的范围留在 CoT，后续范围交给独立过程补充。
#[test]
fn cursor_restart_supplements_only_unconfirmed_process_replies() {
    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_restart_cot_failed");
    let mut projector = generation_before_a_cursor_restart(&frontend, &chat);
    frontend.flush_fails.set(true);
    projector.hand_off_generation_replies(&frontend, &chat, "tester", &test_outbox(dir.path()));
    let supplements = frontend.supplements.borrow();
    assert_eq!(supplements.len(), 1);
    let card: serde_json::Value = serde_json::from_str(&supplements[0]).unwrap();
    assert_eq!(
        card["body"]["elements"][0]["elements"][0]["content"],
        "收到,先看这个。"
    );
    assert!(frontend.sent.borrow().is_empty());
}

#[test]
fn cursor_restart_keeps_failed_process_supplement_in_the_durable_outbox() {
    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_restart_retry");
    let mut projector = generation_before_a_cursor_restart(&frontend, &chat);
    frontend.flush_fails.set(true);
    frontend.fail_supplement.set(true);
    let outbox = test_outbox(dir.path());
    projector.hand_off_generation_replies(&frontend, &chat, "tester", &outbox);
    let events = test_outbox(dir.path()).pending_events();
    assert_eq!(events.len(), 1);
    let summon_core::outbox::OutboxKind::ProcessSupplement {
        rendered_card,
        chat_id,
        ..
    } = &events[0].kind
    else {
        panic!("过程补充重试事件");
    };
    let card: serde_json::Value = serde_json::from_str(rendered_card).unwrap();
    assert_eq!(
        card["body"]["elements"][0]["elements"][0]["content"],
        "收到,先看这个。"
    );
    assert_eq!(chat_id, chat.chat_id);
    assert_eq!(events[0].delivery_scope, chat.delivery_scope);
    assert!(frontend.sent.borrow().is_empty());
    assert!(frontend.supplements.borrow().is_empty());
}

#[test]
fn a_reply_first_turn_creates_reasoning_only_when_a_tool_arrives() {
    use summon_core::agent_msg::AgentKind;
    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_reply_first");
    let mut projector = cot_projector(None);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[process_reply_event(AgentKind::Assistant, "先看代码。")],
    );
    assert!(!frontend
        .events
        .borrow()
        .iter()
        .any(|event| event.event_type == "REASONING_MESSAGE_START"));
    run_events(&frontend, &chat, &mut projector, &process_tool_events());
    let events = frontend.events.borrow();
    let text = events
        .iter()
        .position(|event| event.event_type == "TEXT_MESSAGE_CONTENT")
        .unwrap();
    let reasoning = events
        .iter()
        .position(|event| event.event_type == "REASONING_MESSAGE_START")
        .unwrap();
    let tool = events
        .iter()
        .position(|event| event.event_type == "TOOL_CALL_START")
        .unwrap();
    assert!(text < reasoning && reasoning < tool);
    let parent: serde_json::Value = serde_json::from_str(&events[reasoning].content).unwrap();
    let call: serde_json::Value = serde_json::from_str(&events[tool].content).unwrap();
    assert_eq!(call["parentMessageId"], parent["messageId"]);
}

/// 后续失败保留此前成功确认的正文范围。
#[test]
fn later_cot_failure_preserves_confirmed_process_replies() {
    use summon_core::agent_msg::AgentKind;
    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_process_reply_cot_failed");
    let mut projector = cot_projector(None);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Assistant, "第一段说明。"),
            process_reply_event(AgentKind::Thinking, "接着想"),
        ],
    );
    frontend.append_failed.set(true);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[
            process_reply_event(AgentKind::Assistant, "第二段说明。"),
            process_reply_event(AgentKind::Thinking, "继续想"),
            process_reply_event(AgentKind::Assistant, "收尾。"),
        ],
    );
    projector.live.confirm_cot_replies(&frontend);
    assert_eq!(
        projector
            .live
            .unconfirmed_process_replies(&projector.segments, Some("收尾。"))
            .as_deref(),
        Some("第二段说明。")
    );
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("收尾。"))
            .as_deref(),
        Some("收尾。")
    );
}

#[test]
fn headless_child_summary_keeps_child_content_out_of_cot() {
    use summon_core::agent_msg::{AgentKind, AgentMessage, RunId};
    let front = EmitterFrontend::new();
    let root = tempfile::tempdir().unwrap();
    let chat = badge_chat(root.path(), "oc_child_summary");
    let mut projector = TurnProjector::new(
        CotEmitter::new(None),
        summon_core::progress::TurnProgress::new(),
        None,
    )
    .with_child_summary_only(true);
    for kind in [
        AgentKind::ToolUse,
        AgentKind::Thinking,
        AgentKind::Assistant,
        AgentKind::ToolResult,
    ] {
        let mut event = AgentMessage::of(kind);
        event.parent_run_id = Some(RunId("child".into()));
        event.text = Some("child content".into());
        event.tool = Some("command_execution".into());
        event.tool_id = Some("child-call".into());
        projector.project_event(&front, &chat, "tester", &event);
    }
    assert!(front.calls.borrow().is_empty());
    assert!(projector.progress.subagent_lanes().is_empty());
    assert!(projector.segments.is_empty());
    let mut main = AgentMessage::of(AgentKind::ToolUse);
    main.tool = Some("shell".into());
    projector.project_event(&front, &chat, "tester", &main);
    assert_eq!(projector.progress.protocol_tools().len(), 1);
    assert!(!front.calls.borrow().is_empty());
}

#[test]
fn headless_child_task_result_does_not_republish_the_nested_conversation() {
    let front = EmitterFrontend::new();
    let root = tempfile::tempdir().unwrap();
    let chat = badge_chat(root.path(), "oc_task_result");
    let mut projector = TurnProjector::new(
        CotEmitter::new(None),
        summon_core::progress::TurnProgress::new().with_tool_details(true),
        None,
    )
    .with_child_summary_only(true);
    let fixture = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cursor_subagent.jsonl"
    ));
    for line in fixture.lines() {
        let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
        if row["subtype"] == "completed" {
            row["tool_call"]["taskToolCall"]["result"]["success"]["conversationSteps"] = serde_json::json!([
                {"thinkingMessage":{"text":"private child thinking"}},
                {"toolCall":{"output":"private child tool output"}},
                {"assistantMessage":{"text":"latest child body"}}
            ]);
        }
        for event in summon_core::stream_json::parse_stream_json_line(&row.to_string()) {
            projector.project_event(&front, &chat, "tester", &event);
        }
    }
    assert_eq!(projector.progress.protocol_tools().len(), 1);
    assert!(projector.progress.protocol_tools()[0].done);
    assert!(projector.progress.protocol_tools()[0].result.is_none());
    assert!(front
        .payloads
        .borrow()
        .iter()
        .all(|payload| !payload.contains("private child thinking")
            && !payload.contains("private child tool output")
            && !payload.contains("latest child body")));
}

#[test]
fn hidden_cot_keeps_the_working_shell_without_projecting_process_content() {
    use summon_core::agent_msg::{AgentKind, AgentMessage};

    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_hidden_cot");
    let mut projector = TurnProjector::new(
        CotEmitter::new(None),
        summon_core::progress::TurnProgress::new(),
        None,
    )
    .with_cot_visible(false);

    let mut thinking = AgentMessage::of(AgentKind::Thinking);
    thinking.text = Some("不应外显的推理".into());
    projector.project_event(&frontend, &chat, "tester", &thinking);
    let mut tool = AgentMessage::of(AgentKind::ToolUse);
    tool.tool = Some("Bash".into());
    tool.tool_input = Some(serde_json::json!({"command": "secret-command"}));
    projector.project_event(&frontend, &chat, "tester", &tool);
    let mut reply = AgentMessage::of(AgentKind::Assistant);
    reply.text = Some("仍在生成的正文".into());
    projector.project_event(&frontend, &chat, "tester", &reply);

    assert!(projector.segments.is_empty());
    assert!(projector.progress.steps().is_empty());
    assert!(projector.progress.protocol_tools().is_empty());
    assert!(frontend.calls.borrow().contains(&"begin"));
    assert!(frontend
        .payloads
        .borrow()
        .iter()
        .all(|payload| !payload.contains("secret-command") && !payload.contains("不应外显")));
}

#[test]
fn hidden_cot_consumer_drops_the_task_summary_and_retry_detail() {
    use summon_core::agent_msg::{AgentKind, AgentMessage};

    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_hidden_cot_consumer");
    let (out, _) = card_stream_consume_with_cot(
        &frontend,
        &chat,
        false,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false,
        false,
        false,
        None,
        None,
        true,
        Some("secret-task-summary"),
        None,
        summon_core::event_log::EventStamp::default(),
        &test_outbox(dir.path()),
        &|| None,
        None,
        None,
        &mut |_: &[String]| {},
        |on_msg,
         _on_stream,
         _on_tool,
         on_event,
         _on_interactive,
         on_tick,
         _on_steer,
         _on_interim| {
            let mut thinking = AgentMessage::of(AgentKind::Thinking);
            thinking.text = Some("secret-thinking".into());
            on_event(&thinking);
            on_tick(Some(&summon_core::retry_wait::RetryWait {
                remaining_secs: Some(9),
                line: "secret-retry".into(),
            }));
            on_msg("最终结果");
            Ok(crate::capture::TurnOutcome::default())
        },
    );

    assert!(out.is_ok());
    assert!(frontend.payloads.borrow().iter().all(|payload| {
        !payload.contains("secret-task-summary")
            && !payload.contains("secret-thinking")
            && !payload.contains("secret-retry")
    }));
}

/// 把一串原始工具事件经 `TurnProjector` 投到 CoT,取出每条工具行的标题。
fn cot_tool_titles(
    events: Vec<summon_core::agent_msg::AgentMessage>,
    progress: summon_core::progress::TurnProgress,
) -> Vec<String> {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_cot_tool_titles");
    let mut projector = TurnProjector::new(CotEmitter::new(None), progress, None);
    for event in &events {
        projector.project_event(&frontend, &chat, "tester", event);
    }
    let mut cot = summon_core::cot::CotProjector::new("chat", "turn", "任务");
    cot.project(&[], Some(&projector.progress), "1000")
        .iter()
        .filter(|event| event.event_type == "TOOL_CALL_START")
        .map(|event| {
            let content: serde_json::Value = serde_json::from_str(&event.content).unwrap();
            content["toolCallName"].as_str().unwrap().to_string()
        })
        .collect()
}

/// codex 的工具项从 app-server 通知一路投到 CoT:命令行的标题是命令正文,其余是「工具名 + 对象」。
#[test]
fn codex_tool_items_reach_the_cot_titled_by_what_they_run() {
    let items = [
        serde_json::json!({
            "id":"cmd_1","type":"commandExecution",
            "command":"/bin/zsh -lc 'cargo test -p summon-core'","status":"inProgress"
        }),
        serde_json::json!({
            "id":"call_1","type":"dynamicToolCall","namespace":"summon","tool":"preview",
            "arguments":{"path":"/Users/someone/report.md"},"status":"inProgress"
        }),
        serde_json::json!({
            "id":"fc_1","type":"fileChange","status":"inProgress",
            "changes":[
                {"path":"/Users/someone/repo/cot.rs","kind":"update","diff":""},
                {"path":"/Users/someone/repo/lib.rs","kind":"update","diff":""}
            ]
        }),
        serde_json::json!({
            "id":"ws_1","type":"webSearch","query":"serde preserve_order","action":null
        }),
        serde_json::json!({
            "id":"mcp_1","type":"mcpToolCall","server":"lark","tool":"search_docs",
            "arguments":{"query":"周报"},"status":"inProgress"
        }),
        serde_json::json!({
            "id":"ca_1","type":"collabAgentToolCall","tool":"spawnAgent","prompt":"查一下",
            "status":"inProgress","senderThreadId":"t","receiverThreadIds":[],"agentsStates":{}
        }),
        serde_json::json!({"id":"sl_1","type":"sleep","durationMs":30000}),
    ];
    let events = items
        .into_iter()
        .flat_map(|item| {
            let notification = summon_core::codex_rpc::parse_notification(&serde_json::json!({
                "method":"item/started",
                "params":{"threadId":"t","turnId":"u","item":item}
            }))
            .unwrap()
            .unwrap();
            summon_core::codex_rpc::rpc_notification_to_agent_messages(&notification)
        })
        .collect();
    assert_eq!(
        cot_tool_titles(events, summon_core::progress::TurnProgress::new()),
        [
            "cargo test -p summon-core",
            "summon.preview report.md",
            "file_change cot.rs, lib.rs",
            "web_search serde preserve_order",
            "lark · search_docs",
            "spawnAgent",
            "sleep 30s",
        ]
    );
}

/// cursor 无头流的真机形状(工具体旁并排挂元数据键)投到 CoT:命令与读文件都认得出来,
/// 元数据键不会冒充工具。
#[test]
fn cursor_tool_calls_reach_the_cot_titled_by_what_they_run() {
    let lines = [
        r#"{"type":"tool_call","subtype":"started","call_id":"c1","tool_call":{"shellToolCall":{"args":{"command":"echo summon-probe","description":"Echo summon-probe exactly"},"description":"Echo summon-probe exactly"},"hookAdditionalContexts":[],"toolCallId":"c1","startedAtMs":"1"},"session_id":"s1"}"#,
        r#"{"type":"tool_call","subtype":"started","call_id":"c2","tool_call":{"readToolCall":{"args":{"path":"/Users/someone/note.txt"}},"hookAdditionalContexts":[],"toolCallId":"c2","startedAtMs":"2"},"session_id":"s1"}"#,
        r#"{"type":"tool_call","subtype":"started","call_id":"c3","tool_call":{"globToolCall":{"args":{"globPattern":"**/*.swift","targetDirectory":"/Users/someone/repo"}},"hookAdditionalContexts":[],"toolCallId":"c3","startedAtMs":"3"},"session_id":"s1"}"#,
    ];
    let events = lines
        .into_iter()
        .flat_map(summon_core::stream_json::parse_stream_json_line)
        .collect();
    assert_eq!(
        cot_tool_titles(events, summon_core::progress::TurnProgress::new()),
        ["echo summon-probe", "read note.txt", "glob **/*.swift"]
    );
}

/// live 卡沿用中文友好名与命令:codex 的 MCP 调用读作「MCP 工具」,cursor 的 shell 读作命令本身。
#[test]
fn live_card_labels_keep_friendly_names_and_commands() {
    let running = |event: summon_core::agent_msg::AgentMessage| {
        let frontend = EmitterFrontend::new();
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_live_card_labels");
        let mut projector = TurnProjector::new(
            CotEmitter::new(None),
            summon_core::progress::TurnProgress::new(),
            None,
        );
        projector.project_event(&frontend, &chat, "tester", &event);
        summon_core::progress::running_tool_label(&projector.progress)
            .map(str::to_string)
            .unwrap_or_default()
    };
    let notification = summon_core::codex_rpc::parse_notification(&serde_json::json!({
        "method":"item/started",
        "params":{"threadId":"t","turnId":"u","item":{
            "id":"mcp_1","type":"mcpToolCall","server":"lark","tool":"search_docs",
            "arguments":{"query":"周报"},"status":"inProgress"
        }}
    }))
    .unwrap()
    .unwrap();
    let mcp = summon_core::codex_rpc::rpc_notification_to_agent_messages(&notification).remove(0);
    assert_eq!(running(mcp), "MCP 工具");

    let shell = summon_core::stream_json::parse_stream_json_line(
        r#"{"type":"tool_call","subtype":"started","call_id":"c1","tool_call":{"shellToolCall":{"args":{"command":"echo summon-probe","description":"Echo summon-probe exactly"}},"hookAdditionalContexts":[],"toolCallId":"c1","startedAtMs":"1"},"session_id":"s1"}"#,
    )
    .remove(0);
    assert_eq!(running(shell), "echo summon-probe");
}

/// CoT 的工具详情开关从设置一路接到 `TurnProgress`:两条流式入口(转录 / 无头)建进度账本时
/// 都用 `settings.cot_tool_details()`。漏接时账本默认外显工具详情,出厂「详情关」就悄悄失效。
#[test]
fn cot_tool_details_setting_reaches_every_turn_progress() {
    let production = include_str!("chat_thread.rs");
    assert_eq!(
        production
            .matches("let show_tool_details = settings.cot_tool_details();")
            .count(),
        2,
        "两条流式入口都要从设置取工具详情开关"
    );
    let builders = production
        .matches(".with_hide_thinking(hide_thinking)")
        .count();
    assert_eq!(builders, 2, "进度账本的构造点数变了,同步检查工具详情接线");
    for (at, _) in production.match_indices(".with_hide_thinking(hide_thinking)") {
        let chain = &production[at..];
        let chain = &chain[..chain.find(';').unwrap_or(chain.len())];
        assert!(
            chain.contains(".with_tool_details(show_tool_details)"),
            "进度账本漏接工具详情开关:{chain}"
        );
    }
}

fn sub_event(
    parent: &str,
    kind: summon_core::agent_msg::AgentKind,
) -> summon_core::agent_msg::AgentMessage {
    let mut msg = summon_core::agent_msg::AgentMessage::of(kind);
    msg.parent_run_id = Some(summon_core::agent_msg::RunId(parent.to_string()));
    msg
}

fn main_tool(
    tool: &str,
    id: &str,
    input: serde_json::Value,
) -> summon_core::agent_msg::AgentMessage {
    let mut msg =
        summon_core::agent_msg::AgentMessage::of(summon_core::agent_msg::AgentKind::ToolUse);
    msg.tool = Some(tool.to_string());
    msg.tool_id = Some(id.to_string());
    msg.tool_input = Some(input);
    msg
}

fn main_result(id: &str) -> summon_core::agent_msg::AgentMessage {
    let mut msg =
        summon_core::agent_msg::AgentMessage::of(summon_core::agent_msg::AgentKind::ToolResult);
    msg.tool_id = Some(id.to_string());
    msg.tool_result = Some(serde_json::json!("done"));
    msg
}

/// 按顺序把事件投进 `TurnProjector`,每投一条就推一次 CoT(模拟逐条刷新),最后收尾。
fn cot_after(
    events: &[summon_core::agent_msg::AgentMessage],
    progress: summon_core::progress::TurnProgress,
) -> (Vec<summon_core::cot::CotEvent>, TurnProjector) {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_cot_subagents");
    let mut projector = TurnProjector::new(CotEmitter::new(None), progress, None);
    let mut cot = summon_core::cot::CotProjector::new("chat", "turn", "任务");
    let mut out = Vec::new();
    for (n, event) in events.iter().enumerate() {
        projector.project_event(&frontend, &chat, "tester", event);
        out.extend(cot.project(
            &projector.segments,
            Some(&projector.progress),
            &(1000 + n).to_string(),
        ));
    }
    out.extend(cot.finish("done", "9999"));
    (out, projector)
}

fn reasoning_text(events: &[summon_core::cot::CotEvent]) -> String {
    events
        .iter()
        .filter(|e| e.event_type == "REASONING_MESSAGE_CONTENT")
        .map(|e| {
            serde_json::from_str::<serde_json::Value>(&e.content).unwrap()["delta"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn tool_names(events: &[summon_core::cot::CotEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.event_type == "TOOL_CALL_START")
        .map(|e| {
            serde_json::from_str::<serde_json::Value>(&e.content).unwrap()["toolCallName"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// claude 同步子 agent:主 agent 在等,子 agent 的工具实时进它自己的一段;段首是派它时的标题
/// 与说明;子 agent 给主 agent 的最后报告不进 CoT;子 agent 的事件不碰主轮的正文与会话 id。
#[test]
fn claude_sync_subagent_streams_into_its_own_cot_block() {
    use summon_core::agent_msg::AgentKind;
    let mut sub_tool = sub_event("toolu_agent", AgentKind::ToolUse);
    sub_tool.tool = Some("Bash".into());
    sub_tool.tool_id = Some("toolu_sub".into());
    sub_tool.tool_input = Some(serde_json::json!({"command": "echo hi"}));
    sub_tool.session_id = Some("sub-session".into());
    let mut sub_result = sub_event("toolu_agent", AgentKind::ToolResult);
    sub_result.tool_id = Some("toolu_sub".into());
    sub_result.tool_result = Some(serde_json::json!("hi"));
    let mut sub_note = sub_event("toolu_agent", AgentKind::Assistant);
    sub_note.text = Some("先看入口".into());
    let mut sub_more = sub_event("toolu_agent", AgentKind::ToolUse);
    sub_more.tool = Some("Read".into());
    sub_more.tool_id = Some("toolu_sub2".into());
    sub_more.tool_input = Some(serde_json::json!({"file_path": "/Users/x/main.rs"}));
    let mut sub_report = sub_event("toolu_agent", AgentKind::Assistant);
    sub_report.text = Some("最终报告:入口在 main.rs".into());
    sub_report.stop_reason = Some("end_turn".into());

    let events = [
        main_tool(
            "Agent",
            "toolu_agent",
            serde_json::json!({"subagent_type": "Explore", "description": "找入口", "prompt": "p"}),
        ),
        sub_tool,
        sub_result,
        sub_note,
        sub_more,
        sub_report,
        main_result("toolu_agent"),
    ];
    let (cot, projector) = cot_after(
        &events,
        summon_core::progress::TurnProgress::new().with_tool_details(true),
    );
    let text = reasoning_text(&cot);
    assert!(text.contains("Agent Explore: 找入口"), "{text}");
    assert!(text.contains("先看入口"), "{text}");
    assert!(!text.contains("最终报告"), "{text}");
    assert_eq!(
        tool_names(&cot),
        ["Agent Explore: 找入口", "echo hi", "Read main.rs"]
    );
    assert!(projector.segments.is_empty(), "子 agent 文字不进主轮正文");
    assert_eq!(projector.turn_sid, None, "子 agent 的会话 id 不算本轮的");
    assert!(projector.progress.subagent_lanes()[0].finished());
    assert!(!projector.progress.parent_waiting());
}

/// claude 后台子 agent:主 agent 没在等,子 agent 的内容攒到它结束(或轮收尾)才整段推出。
#[test]
fn claude_background_subagent_waits_for_the_turn_end() {
    use summon_core::agent_msg::AgentKind;
    let mut sub_tool = sub_event("toolu_bg", AgentKind::ToolUse);
    sub_tool.tool = Some("Bash".into());
    sub_tool.tool_id = Some("toolu_sub".into());
    sub_tool.tool_input = Some(serde_json::json!({"command": "echo bg"}));
    let events = [
        main_tool(
            "Agent",
            "toolu_bg",
            serde_json::json!({"subagent_type": "reviewer", "description": "审一下", "run_in_background": true}),
        ),
        main_result("toolu_bg"),
        sub_tool,
        main_tool(
            "Bash",
            "toolu_main",
            serde_json::json!({"command": "cargo test"}),
        ),
    ];
    let (cot, projector) = cot_after(
        &events,
        summon_core::progress::TurnProgress::new().with_tool_details(true),
    );
    assert_eq!(
        tool_names(&cot),
        ["Agent reviewer: 审一下", "cargo test", "echo bg"],
        "后台子 agent 的行排在主 agent 之后,整段推出"
    );
    assert!(reasoning_text(&cot).contains("（仍在运行）"));
    assert!(!projector.progress.subagent_lanes()[0].finished());
}

/// codex 子线程:主线程 `subAgentActivity(started)` 给出子 agent 名,`wait` 在飞时子线程的命令
/// 实时进它自己的一段;子线程轮结束即子 agent 结束。
#[test]
fn codex_subagent_thread_streams_into_its_own_cot_block() {
    use summon_core::agent_msg::AgentKind;
    let thread = "01a0aedc-824e-7162-8e2d-1ec835bca318";
    let mut child_delta = sub_event(thread, AgentKind::Assistant);
    child_delta.text = Some("先跑".into());
    child_delta.partial = true;
    let mut child_note = sub_event(thread, AgentKind::Assistant);
    child_note.text = Some("先跑一下 echo".into());
    let mut child_cmd = sub_event(thread, AgentKind::ToolUse);
    child_cmd.tool = Some("command_execution".into());
    child_cmd.tool_id = Some("cmd_1".into());
    child_cmd.tool_input = Some(serde_json::json!("/bin/zsh -lc 'echo child-probe'"));
    let mut child_report = sub_event(thread, AgentKind::Assistant);
    child_report.text = Some("最终报告:child-probe".into());
    let child_done = sub_event(thread, AgentKind::Result);
    let events = [
        main_tool(
            "sub_agent_activity",
            "act_1",
            serde_json::json!({"kind": "started", "agentThreadId": thread, "agentPath": "/root/child_probe"}),
        ),
        main_tool(
            "collab_agent_tool_call",
            "wait_1",
            serde_json::json!({"tool": "wait", "receiverThreadIds": []}),
        ),
        child_delta,
        child_note,
        child_cmd,
        child_report,
        child_done,
        main_result("wait_1"),
    ];
    let (cot, projector) = cot_after(
        &events,
        summon_core::progress::TurnProgress::new().with_tool_details(true),
    );
    let text = reasoning_text(&cot);
    assert!(text.contains("child_probe"), "{text}");
    assert!(text.contains("先跑一下 echo"), "{text}");
    assert!(
        !text.contains("最终报告"),
        "子 agent 最后一段是报告,不进 CoT:{text}"
    );
    assert_eq!(text.matches("先跑").count(), 1, "流式增量不单独成段:{text}");
    assert_eq!(
        tool_names(&cot),
        [
            "sub_agent_activity child_probe started",
            "wait",
            "echo child-probe"
        ]
    );
    assert!(projector.progress.subagent_lanes()[0].finished());
    assert!(!projector.progress.parent_waiting());
}

/// codex 子线程的一条原始通知,走 RPC 捕获层的真实转换(解析 → 转事件 → 盖子 agent 标识)。
fn codex_child_events(
    thread: &str,
    notification: serde_json::Value,
) -> Vec<summon_core::agent_msg::AgentMessage> {
    let notification = summon_core::codex_rpc::parse_notification(&notification)
        .unwrap()
        .unwrap();
    let mut out = Vec::new();
    crate::capture::rpc::forward_subagent_notification(&notification, thread, &mut |msg| {
        out.push(msg.clone())
    });
    out
}

fn lane_thoughts(lane: &summon_core::progress::SubagentLane) -> Vec<&str> {
    lane.items()
        .iter()
        .filter_map(|item| match item {
            summon_core::progress::SubagentItem::Thinking(text) => Some(text.as_str()),
            summon_core::progress::SubagentItem::Tool(_) => None,
        })
        .collect()
}

/// 子 agent 的思考只收完整的一段:codex reasoning 的完成快照(与增量共用段 key、也标着 partial)
/// 进段,增量不进,同一个 item 的完成快照重复到达只收一次;claude 转录的思考照收。
#[test]
fn subagent_thinking_takes_each_completed_snapshot_once() {
    use summon_core::agent_msg::AgentKind;
    let thread = "th-child";
    let mut events = vec![
        main_tool(
            "sub_agent_activity",
            "act_1",
            serde_json::json!({"kind": "started", "agentThreadId": thread, "agentPath": "/root/child_probe"}),
        ),
        main_tool(
            "collab_agent_tool_call",
            "wait_1",
            serde_json::json!({"tool": "wait", "receiverThreadIds": []}),
        ),
    ];
    events.extend(codex_child_events(
        thread,
        serde_json::json!({"method": "item/reasoning/summaryTextDelta", "params": {
            "threadId": thread, "turnId": "tc", "itemId": "rs_1", "summaryIndex": 0,
            "delta": "**Checking echo**"}}),
    ));
    let completed = serde_json::json!({"method": "item/completed", "params": {
        "threadId": thread, "turnId": "tc",
        "item": {"id": "rs_1", "type": "reasoning", "summary": ["**Checking echo**"], "content": []}}});
    events.extend(codex_child_events(thread, completed.clone()));
    events.extend(codex_child_events(thread, completed));
    events.extend(codex_child_events(
        thread,
        serde_json::json!({"method": "item/started", "params": {
            "threadId": thread, "turnId": "tc",
            "item": {"id": "c1", "type": "commandExecution", "command": "/bin/zsh -lc 'echo x'"}}}),
    ));
    events.push(main_tool(
        "Agent",
        "toolu_agent",
        serde_json::json!({"subagent_type": "Explore", "description": "找入口", "prompt": "p"}),
    ));
    let mut claude_thinking = sub_event("toolu_agent", AgentKind::Thinking);
    claude_thinking.text = Some("先看目录".into());
    events.push(claude_thinking);

    let (cot, projector) = cot_after(
        &events,
        summon_core::progress::TurnProgress::new().with_tool_details(true),
    );
    let lanes = projector.progress.subagent_lanes();
    assert_eq!(lane_thoughts(&lanes[0]), ["**Checking echo**"]);
    assert_eq!(lane_thoughts(&lanes[1]), ["先看目录"]);
    let text = reasoning_text(&cot);
    assert_eq!(text.matches("Checking echo").count(), 1, "{text}");
    assert!(text.contains("先看目录"), "{text}");
}

fn sub_command(parent: &str, id: &str, command: &str) -> summon_core::agent_msg::AgentMessage {
    let mut msg = sub_event(parent, summon_core::agent_msg::AgentKind::ToolUse);
    msg.tool = Some("Bash".into());
    msg.tool_id = Some(id.into());
    msg.tool_input = Some(serde_json::json!({ "command": command }));
    msg
}

fn cot_tool_starts(events: &[summon_core::cot::CotEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter(|e| e.event_type == "TOOL_CALL_START")
        .map(|e| {
            let v: serde_json::Value = serde_json::from_str(&e.content).unwrap();
            (
                v["toolCallId"].as_str().unwrap().to_string(),
                v["toolCallName"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// 接管重放:上一代主/子交错到达,这一代先喂完主转录、子转录也换了先后。主工具编号不变,子工具
/// 的 id 不随到达顺序漂,同一个 toolCallId 不会以不同内容上两次 wire。
#[test]
fn reattach_replay_keeps_tool_call_ids_stable_across_arrival_orders() {
    use summon_core::agent_msg::AgentKind;
    let agent_a = main_tool(
        "Agent",
        "toolu_a",
        serde_json::json!({"subagent_type": "Explore", "description": "找入口", "run_in_background": true}),
    );
    let agent_b = main_tool(
        "Agent",
        "toolu_b",
        serde_json::json!({"subagent_type": "Plan", "description": "出计划", "run_in_background": true}),
    );
    let a1 = sub_command("toolu_a", "sa1", "echo a1");
    let a2 = sub_command("toolu_a", "sa2", "echo a2");
    let b1 = sub_command("toolu_b", "sb1", "echo b1");
    let mut b_end = sub_event("toolu_b", AgentKind::Assistant);
    b_end.text = Some("计划写好了".into());
    b_end.stop_reason = Some("end_turn".into());
    let main_bash = main_tool(
        "Bash",
        "toolu_main",
        serde_json::json!({"command": "cargo test"}),
    );
    let main_bash2 = main_tool(
        "Bash",
        "toolu_main2",
        serde_json::json!({"command": "cargo build"}),
    );
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_cot_reattach_ids");
    let progress = || summon_core::progress::TurnProgress::new().with_tool_details(true);
    let mut cot = summon_core::cot::CotProjector::new("chat", "turn", "任务");
    let mut all = Vec::new();

    let first = [
        agent_a.clone(),
        main_result("toolu_a"),
        agent_b.clone(),
        main_result("toolu_b"),
        a1.clone(),
        main_bash.clone(),
        b1.clone(),
        b_end.clone(),
        a2.clone(),
    ];
    let mut projector = TurnProjector::new(CotEmitter::new(None), progress(), None);
    for (n, ev) in first.iter().enumerate() {
        projector.project_event(&frontend, &chat, "tester", ev);
        all.extend(cot.project(
            &projector.segments,
            Some(&projector.progress),
            &(1000 + n).to_string(),
        ));
    }

    let mut cot: summon_core::cot::CotProjector =
        serde_json::from_str(&serde_json::to_string(&cot).unwrap()).unwrap();
    let second = [
        agent_a,
        main_result("toolu_a"),
        agent_b,
        main_result("toolu_b"),
        main_bash,
        b1,
        b_end,
        a1,
        a2,
        main_bash2,
    ];
    let mut projector = TurnProjector::new(CotEmitter::new(None), progress(), None);
    for (n, ev) in second.iter().enumerate() {
        projector.project_event(&frontend, &chat, "tester", ev);
        all.extend(cot.project(
            &projector.segments,
            Some(&projector.progress),
            &(2000 + n).to_string(),
        ));
    }
    all.extend(cot.finish("done", "9999"));

    let starts = cot_tool_starts(&all);
    let pairs = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        starts,
        pairs(&[
            ("tool-1", "Agent Explore: 找入口"),
            ("tool-2", "Agent Plan: 出计划"),
            ("tool-3", "cargo test"),
            ("tool-s2-1", "echo b1"),
            ("tool-4", "cargo build"),
            ("tool-s1-1", "echo a1"),
            ("tool-s1-2", "echo a2"),
        ])
    );
}

/// codex 孙线程:子线程上的 `subAgentActivity(started)` 给孙线程登记标题,孙线程的事件进它自己
/// 的一段;子线程上的 `completed` 把它收掉。
#[test]
fn codex_grandchild_thread_gets_its_own_titled_lane() {
    let child = "th-child";
    let grand = "th-grand";
    let mut grand_started = sub_event(child, summon_core::agent_msg::AgentKind::ToolUse);
    grand_started.tool = Some("sub_agent_activity".into());
    grand_started.tool_id = Some("act_g1".into());
    grand_started.tool_input = Some(serde_json::json!({
        "kind": "started", "agentThreadId": grand, "agentPath": "/root/child_probe/grand_probe"
    }));
    let mut grand_completed = grand_started.clone();
    grand_completed.tool_id = Some("act_g2".into());
    grand_completed.tool_input = Some(serde_json::json!({
        "kind": "completed", "agentThreadId": grand, "agentPath": "/root/child_probe/grand_probe"
    }));
    let events = [
        main_tool(
            "sub_agent_activity",
            "act_1",
            serde_json::json!({"kind": "started", "agentThreadId": child, "agentPath": "/root/child_probe"}),
        ),
        main_tool(
            "collab_agent_tool_call",
            "wait_1",
            serde_json::json!({"tool": "wait", "receiverThreadIds": []}),
        ),
        sub_command(child, "c1", "echo child"),
        grand_started,
        sub_command(grand, "g1", "echo grand"),
        grand_completed,
    ];
    let (cot, projector) = cot_after(
        &events,
        summon_core::progress::TurnProgress::new().with_tool_details(true),
    );
    let lanes = projector.progress.subagent_lanes();
    assert_eq!(
        lanes
            .iter()
            .map(|lane| (lane.key(), lane.title(), lane.finished()))
            .collect::<Vec<_>>(),
        [(child, "child_probe", false), (grand, "grand_probe", true)]
    );
    assert!(
        projector.progress.parent_waiting(),
        "子线程的派出记录不改主 agent 的等待"
    );
    let text = reasoning_text(&cot);
    assert!(text.contains("grand_probe"), "{text}");
    assert!(tool_names(&cot).contains(&"echo grand".to_string()));
}

// ── claude 2.1.274 真机转录形状(正文截短、id 脱敏) ──
// `Agent` 调用不带 `run_in_background` 也是异步:几毫秒后结果就到,只是「已派到后台」的回执。
pub(super) const CLAUDE_AGENT_CALL: &str = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_async","name":"Agent","input":{"description":"审一下","subagent_type":"adversarial-reviewer","prompt":"p"}}],"stop_reason":"tool_use"},"version":"2.1.274"}"#;
pub(super) const CLAUDE_AGENT_LAUNCHED: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"toolu_async","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully.\nagentId: a1c7b6848c7434fad (internal ID - do not mention to user.)"}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a1c7b6848c7434fad","description":"审一下","resolvedModel":"claude-opus-5","prompt":"p","outputFile":"/tmp/a.output","canReadOutputFile":false},"version":"2.1.274"}"#;
// 2.1.268 起 `run_in_background` 是字符串。
pub(super) const CLAUDE_AGENT_BG_STRING_CALL: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_bg_str","name":"Agent","input":{"description":"查资料","subagent_type":"Explore","prompt":"p","run_in_background":"true"}}],"stop_reason":"tool_use"},"version":"2.1.268"}"#;
// 主 agent 阻塞取后台任务输出;结果是「超时,任务仍在跑」。
pub(super) const CLAUDE_TASK_OUTPUT_CALL: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"call_wait","name":"TaskOutput","input":{"task_id":"a1c7b6848c7434fad","block":true,"timeout":60000}}],"stop_reason":"tool_use"}}"#;
pub(super) const CLAUDE_TASK_OUTPUT_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"call_wait","type":"tool_result","content":"<retrieval_status>timeout</retrieval_status>\n\n<task_id>a1c7b6848c7434fad</task_id>\n\n<status>running</status>"}]},"toolUseResult":{"retrieval_status":"timeout","task":{"task_id":"a1c7b6848c7434fad","task_type":"local_agent","status":"running"}}}"#;
// 子 agent 自己的转录(`<sid>/subagents/agent-<id>.jsonl`)。
pub(super) const CLAUDE_SUB_BASH: &str = r#"{"type":"assistant","isSidechain":true,"agentId":"a1c7b6848c7434fad","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_sub_bash","name":"Bash","input":{"command":"git diff --stat","description":"看改动"}}],"stop_reason":"tool_use"}}"#;
pub(super) const CLAUDE_SUB_BASH_RESULT: &str = r#"{"type":"user","isSidechain":true,"agentId":"a1c7b6848c7434fad","message":{"role":"user","content":[{"tool_use_id":"toolu_sub_bash","type":"tool_result","content":"3 files changed"}]}}"#;
pub(super) const CLAUDE_SUB_REPORT: &str = r#"{"type":"assistant","isSidechain":true,"agentId":"a1c7b6848c7434fad","message":{"role":"assistant","content":[{"type":"text","text":"结论:没问题"}],"stop_reason":"end_turn"}}"#;

fn claude_main(lines: &[&str]) -> Vec<summon_core::agent_msg::AgentMessage> {
    lines
        .iter()
        .flat_map(|line| summon_core::stream_json::parse_stream_json_line(line))
        .collect()
}

fn claude_sub(parent: &str, lines: &[&str]) -> Vec<summon_core::agent_msg::AgentMessage> {
    claude_main(lines)
        .into_iter()
        .map(|mut msg| {
            msg.parent_run_id = Some(summon_core::agent_msg::RunId(parent.to_string()));
            msg
        })
        .collect()
}

fn subagent_projector(state: Option<SharedSubagentState>) -> TurnProjector {
    TurnProjector::new(
        CotEmitter::new(None),
        summon_core::progress::TurnProgress::new().with_tool_details(true),
        state,
    )
}

fn feed(
    projector: &mut TurnProjector,
    chat: &ChatRef,
    events: &[summon_core::agent_msg::AgentMessage],
) {
    let frontend = EmitterFrontend::new();
    for event in events {
        projector.project_event(&frontend, chat, "tester", event);
    }
}

/// 异步派出:调用在飞时还不知道是不是同步,算在等;「已派到后台」的结果一到就不再等,
/// 也不是子 agent 的结束 —— 子 agent 此刻才开始跑。
#[test]
fn claude_async_agent_launch_is_neither_its_end_nor_a_wait() {
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_async_launch");
    let mut projector = subagent_projector(None);

    feed(&mut projector, &chat, &claude_main(&[CLAUDE_AGENT_CALL]));
    assert!(projector.progress.parent_waiting(), "结果到达前按同步算");

    feed(
        &mut projector,
        &chat,
        &claude_main(&[CLAUDE_AGENT_LAUNCHED]),
    );
    assert!(
        !projector.progress.parent_waiting(),
        "已派到后台,主 agent 不在等"
    );
    assert!(
        projector.has_unfinished_subagents(),
        "派到后台、还没出声的子 agent 也算没结束"
    );

    feed(
        &mut projector,
        &chat,
        &claude_sub("toolu_async", &[CLAUDE_SUB_BASH, CLAUDE_SUB_BASH_RESULT]),
    );
    let lane = &projector.progress.subagent_lanes()[0];
    assert_eq!(lane.title(), "Agent adversarial-reviewer");
    assert!(!lane.finished(), "派出回执不是子 agent 的结束");
    assert!(projector.has_unfinished_subagents());

    feed(
        &mut projector,
        &chat,
        &claude_sub("toolu_async", &[CLAUDE_SUB_REPORT]),
    );
    assert!(projector.progress.subagent_lanes()[0].finished());
    assert!(!projector.has_unfinished_subagents());
}

/// `run_in_background` 是字符串 `"true"` 时同样是后台派出:调用一开始就不算在等。
#[test]
fn claude_string_run_in_background_is_a_background_launch() {
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_bg_string");
    let mut projector = subagent_projector(None);
    feed(
        &mut projector,
        &chat,
        &claude_main(&[CLAUDE_AGENT_BG_STRING_CALL]),
    );
    assert!(!projector.progress.parent_waiting());
    assert!(projector.has_unfinished_subagents());
    // 显式后台调用的结果即便没带回执标记,也不是子 agent 的结束。
    feed(&mut projector, &chat, &[main_result("toolu_bg_str")]);
    assert!(projector.has_unfinished_subagents());
}

/// 主 agent 卡在 API 重试时,子 agent 出活不是主 agent 恢复的证据:倒计时留着,主 agent 自己的
/// 事件到了才清。
#[test]
fn subagent_events_leave_the_parent_retry_countdown_alone() {
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_sub_retry");
    let mut projector = subagent_projector(None);
    feed(
        &mut projector,
        &chat,
        &claude_main(&[CLAUDE_AGENT_CALL, CLAUDE_AGENT_LAUNCHED]),
    );
    let retry = summon_core::retry_wait::RetryWait {
        remaining_secs: Some(9),
        line: "Retrying in 9s".into(),
    };
    projector.tick(&EmitterFrontend::new(), &chat, "tester", Some(&retry));
    assert!(projector.progress.retry_wait().is_some());

    feed(
        &mut projector,
        &chat,
        &claude_sub("toolu_async", &[CLAUDE_SUB_BASH]),
    );
    assert!(
        projector.progress.retry_wait().is_some(),
        "子 agent 的事件不清主 agent 的重试倒计时"
    );

    feed(
        &mut projector,
        &chat,
        &claude_main(&[CLAUDE_TASK_OUTPUT_CALL]),
    );
    assert!(projector.progress.retry_wait().is_none());
}

/// 主 agent 阻塞取后台输出(`TaskOutput`,`block` 为真或缺省)时它在等;结果一到就不再等,
/// 而那个结果(哪怕说任务结束了)不是子 agent 的结束信号。
#[test]
fn claude_task_output_in_flight_is_a_wait_and_its_result_is_not_an_end() {
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_task_output");
    let mut projector = subagent_projector(None);
    feed(
        &mut projector,
        &chat,
        &claude_main(&[CLAUDE_AGENT_CALL, CLAUDE_AGENT_LAUNCHED]),
    );
    feed(
        &mut projector,
        &chat,
        &claude_sub("toolu_async", &[CLAUDE_SUB_BASH]),
    );
    assert!(!projector.progress.parent_waiting());

    feed(
        &mut projector,
        &chat,
        &claude_main(&[CLAUDE_TASK_OUTPUT_CALL]),
    );
    assert!(projector.progress.parent_waiting(), "阻塞取输出 = 在等");
    feed(
        &mut projector,
        &chat,
        &claude_main(&[CLAUDE_TASK_OUTPUT_RESULT]),
    );
    assert!(!projector.progress.parent_waiting());
    assert!(!projector.progress.subagent_lanes()[0].finished());

    let wait = |id: &str, input: serde_json::Value| {
        let mut projector = subagent_projector(None);
        feed(&mut projector, &chat, &[main_tool("TaskOutput", id, input)]);
        projector.progress.parent_waiting()
    };
    assert!(
        wait("c1", serde_json::json!({"task_id": "t"})),
        "block 缺省即阻塞"
    );
    assert!(!wait(
        "c2",
        serde_json::json!({"task_id": "t", "block": false})
    ));
    assert!(!wait(
        "c3",
        serde_json::json!({"task_id": "t", "block": "false"})
    ));
    assert!(wait(
        "c4",
        serde_json::json!({"task_id": "t", "block": "true"})
    ));
}

/// 子转录不一定写 `end_turn`:状态源(`subagent_watcher` 的通知配对 / 目录对账)说它结束了,
/// 投影就标结束。结束后的条目只带 agentId,靠派出回执里记下的那一个认。
#[test]
fn claude_subagent_end_comes_from_the_shared_state() {
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_state_end");
    let state: SharedSubagentState = Arc::new(std::sync::Mutex::new(
        summon_core::subagent::SubagentState::new(),
    ));
    let mut projector = subagent_projector(Some(state.clone()));
    feed(
        &mut projector,
        &chat,
        &claude_main(&[CLAUDE_AGENT_CALL, CLAUDE_AGENT_LAUNCHED]),
    );
    feed(
        &mut projector,
        &chat,
        &claude_sub("toolu_async", &[CLAUDE_SUB_BASH]),
    );
    assert!(projector.has_unfinished_subagents());

    {
        let mut st = state.lock().unwrap();
        st.promote_to_running(
            "a1c7b6848c7434fad",
            "审一下",
            "toolu_async",
            None,
            chrono::Utc::now(),
        );
        st.mark_completed("a1c7b6848c7434fad", chrono::Utc::now());
    }
    let frontend = EmitterFrontend::new();
    projector.tick(&frontend, &chat, "tester", None);
    assert!(
        projector.progress.subagent_lanes()[0].finished(),
        "状态源的结束要落到账本上"
    );
    assert!(!projector.has_unfinished_subagents());

    // 没出过声就结束的后台子 agent:之后才读到的行开段时照样是结束的。
    let mut quiet = subagent_projector(Some(state.clone()));
    feed(
        &mut quiet,
        &chat,
        &claude_main(&[CLAUDE_AGENT_CALL, CLAUDE_AGENT_LAUNCHED]),
    );
    assert!(!quiet.has_unfinished_subagents());
    feed(
        &mut quiet,
        &chat,
        &claude_sub("toolu_async", &[CLAUDE_SUB_BASH]),
    );
    assert!(quiet.progress.subagent_lanes()[0].finished());
}

// ── 主轮收尾时 CoT 的去处:照常收口,或摘出交给跟随线程 ──

/// 一轮只派出异步子 agent、随后就收尾的 claude 轮。
struct FollowTurn {
    summary_card: bool,
    /// 轮内已读到的子转录行。
    in_turn_sub: &'static [&'static str],
    /// 捕获层有没有交出终态回复。
    deliver_reply: bool,
    outcome: anyhow::Result<crate::capture::TurnOutcome>,
}

impl FollowTurn {
    /// 干净收场、交出回复,子 agent 还在跑。
    fn clean() -> Self {
        Self {
            summary_card: false,
            in_turn_sub: &[CLAUDE_SUB_BASH],
            deliver_reply: true,
            outcome: Ok(crate::capture::TurnOutcome {
                reply: Some("已派出,稍后汇报".into()),
                ..Default::default()
            }),
        }
    }
}

/// 跑一轮 [`FollowTurn`],返回前端上的调用序列。
fn run_follow_turn(
    frontend: &EmitterFrontend,
    dir: &std::path::Path,
    followers: Option<&super::cot_follower::CotFollowers>,
    turn: FollowTurn,
) -> Vec<&'static str> {
    let chat = badge_chat(dir, "oc_cot_follow");
    let mut main_end =
        summon_core::agent_msg::AgentMessage::of(summon_core::agent_msg::AgentKind::Assistant);
    main_end.text = Some("已派出,稍后汇报".into());
    main_end.stop_reason = Some("end_turn".into());
    let mut thinking =
        summon_core::agent_msg::AgentMessage::of(summon_core::agent_msg::AgentKind::Thinking);
    thinking.text = Some("先派个子 agent 去审".into());
    let mut events = vec![thinking];
    events.extend(claude_main(&[CLAUDE_AGENT_CALL, CLAUDE_AGENT_LAUNCHED]));
    events.extend(claude_sub("toolu_async", turn.in_turn_sub));
    events.push(main_end);
    let expect_ok = turn.outcome.is_ok();
    let (out, _) = card_stream_consume_with_cot(
        frontend,
        &chat,
        turn.summary_card,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false,
        true,
        true,
        None,
        None,
        true,
        Some("派个子 agent"),
        None,
        summon_core::event_log::EventStamp::default(),
        &test_outbox(dir),
        &|| None,
        None,
        followers,
        &mut |_: &[String]| {},
        |on_msg,
         _on_stream,
         _on_tool,
         on_event,
         _on_interactive,
         _on_tick,
         _on_steer,
         _on_interim| {
            for event in &events {
                on_event(event);
            }
            if turn.deliver_reply {
                on_msg("已派出,稍后汇报");
            }
            turn.outcome
        },
    );
    assert_eq!(out.is_ok(), expect_ok);
    frontend.calls.borrow().clone()
}

fn position(calls: &[&str], name: &str) -> Option<usize> {
    calls.iter().position(|call| *call == name)
}

fn stop_followers(
    followers: super::cot_follower::CotFollowers,
    gate: std::sync::mpsc::Sender<()>,
    log: &Arc<std::sync::Mutex<super::cot_follower::test_support::SinkLog>>,
) {
    drop(followers);
    drop(gate);
    let deadline = Instant::now() + Duration::from_secs(5);
    while log.lock().unwrap().releases.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(log.lock().unwrap().releases, [true], "跟随线程应自行收口");
}

/// 干净收场、子 agent 还在跑:这一轮的 CoT 不收口,回复照发,CoT 摘出来交给跟随线程。
/// 单卡与总结卡两条收尾路都一样。
#[test]
fn a_clean_turn_with_running_subagents_hands_its_cot_to_a_follower() {
    for summary_card in [false, true] {
        let frontend = EmitterFrontend::new();
        frontend.process_only.set(true);
        frontend.detachable.set(true);
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(std::sync::Mutex::new(Default::default()));
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let followers = super::cot_follower::test_support::followers(log.clone(), gate_rx);
        let calls = run_follow_turn(
            &frontend,
            dir.path(),
            Some(&followers),
            FollowTurn {
                summary_card,
                ..FollowTurn::clean()
            },
        );
        assert!(
            !calls.contains(&"finish_text"),
            "CoT 不在回复时收口:{calls:?}"
        );
        let reply = position(&calls, if summary_card { "card" } else { "send" })
            .unwrap_or_else(|| panic!("回复照发:{calls:?}"));
        let detach = position(&calls, "detach").expect("摘出 CoT");
        assert!(reply < detach, "回复先发:{calls:?}");
        assert_eq!(followers.spawned(), 1);
        stop_followers(followers, gate_tx, &log);
    }
}

/// 其余情形轮末收口,不跟随:子 agent 已结束、没干净收场(失败 / 超时 / 打断)、不允许跟随
/// (codex)、摘不出来、载体不是 CoT。回复时为子 agent 留着的 CoT 在没干净收场时按 `done` 收口;
/// 没交出回复的非干净收场什么也没留,按失败收口。
#[test]
fn turns_that_do_not_hand_off_settle_their_cot_as_before() {
    use crate::capture::TurnOutcome;
    struct Case {
        name: &'static str,
        process_only: bool,
        detachable: bool,
        follow: bool,
        turn: FollowTurn,
        settle: &'static str,
        detach_tried: bool,
    }
    let clean = FollowTurn::clean;
    let with_outcome = |deliver_reply: bool, outcome: anyhow::Result<TurnOutcome>| FollowTurn {
        deliver_reply,
        outcome,
        ..FollowTurn::clean()
    };
    let reply = || Some("已派出,稍后汇报".to_string());
    let cases = [
        Case {
            name: "子 agent 已结束",
            process_only: true,
            detachable: true,
            follow: true,
            turn: FollowTurn {
                in_turn_sub: &[CLAUDE_SUB_BASH, CLAUDE_SUB_REPORT],
                ..clean()
            },
            settle: "finish_text",
            detach_tried: false,
        },
        Case {
            name: "api_error",
            process_only: true,
            detachable: true,
            follow: true,
            turn: with_outcome(
                true,
                Ok(TurnOutcome {
                    reply: reply(),
                    api_error: Some("stream stalled".into()),
                    ..Default::default()
                }),
            ),
            settle: "finish_text",
            detach_tried: false,
        },
        Case {
            name: "api_error 且没有回复",
            process_only: true,
            detachable: true,
            follow: true,
            turn: with_outcome(
                false,
                Ok(TurnOutcome {
                    api_error: Some("stream stalled".into()),
                    ..Default::default()
                }),
            ),
            settle: "fail",
            detach_tried: false,
        },
        Case {
            name: "Err 且没有回复",
            process_only: true,
            detachable: true,
            follow: true,
            turn: with_outcome(false, Err(anyhow::anyhow!("provider 挂了"))),
            settle: "fail",
            detach_tried: false,
        },
        Case {
            name: "超时",
            process_only: true,
            detachable: true,
            follow: true,
            turn: with_outcome(
                false,
                Ok(TurnOutcome {
                    reply: reply(),
                    timed_out: true,
                    ..Default::default()
                }),
            ),
            settle: "timeout",
            detach_tried: false,
        },
        Case {
            name: "打断",
            process_only: true,
            detachable: true,
            follow: true,
            turn: with_outcome(
                false,
                Ok(TurnOutcome {
                    interrupted: true,
                    ..Default::default()
                }),
            ),
            settle: "finish_text",
            detach_tried: false,
        },
        Case {
            name: "不跟随(codex)",
            process_only: true,
            detachable: true,
            follow: false,
            turn: clean(),
            settle: "finish_text",
            detach_tried: false,
        },
        Case {
            name: "摘不出来",
            process_only: true,
            detachable: false,
            follow: true,
            turn: clean(),
            settle: "finish_text",
            detach_tried: true,
        },
        Case {
            name: "不是 CoT",
            process_only: false,
            detachable: true,
            follow: true,
            turn: clean(),
            settle: "finish_text",
            detach_tried: false,
        },
    ];
    for case in cases {
        let frontend = EmitterFrontend::new();
        frontend.process_only.set(case.process_only);
        frontend.detachable.set(case.detachable);
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(std::sync::Mutex::new(Default::default()));
        let (_gate_tx, gate_rx) = std::sync::mpsc::channel();
        let followers = super::cot_follower::test_support::followers(log, gate_rx);
        let calls = run_follow_turn(
            &frontend,
            dir.path(),
            case.follow.then_some(&followers),
            case.turn,
        );
        let name = case.name;
        let settles = ["finish_text", "fail", "timeout", "finish"];
        assert_eq!(
            calls
                .iter()
                .filter(|call| settles.contains(call))
                .collect::<Vec<_>>(),
            [&case.settle],
            "{name}:收口恰好一次且是这一种 {calls:?}"
        );
        assert_eq!(
            calls.contains(&"detach"),
            case.detach_tried,
            "{name}:{calls:?}"
        );
        assert_eq!(followers.spawned(), 0, "{name}");
    }
}

/// 端到端:主转录只有「派出异步子 agent → end_turn」,子转录在回复之后才写出工具与结束。
/// 回复先发;CoT 继续收到子 agent 的工具行,子 agent 结束后以 RUN_FINISHED 收口。
#[test]
fn async_subagent_keeps_streaming_into_the_turn_cot_after_the_reply() {
    let frontend = EmitterFrontend::new();
    frontend.process_only.set(true);
    frontend.detachable.set(true);
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("sid-1.jsonl");
    std::fs::write(&main, "").unwrap();
    let subagents = dir.path().join("sid-1").join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    let mut tail = crate::capture::SubagentTail::new(&main, "sid-1");
    for event in claude_main(&[CLAUDE_AGENT_CALL]) {
        tail.note(&event);
    }
    let log = Arc::new(std::sync::Mutex::new(
        super::cot_follower::test_support::SinkLog::default(),
    ));
    let (gate_tx, gate_rx) = std::sync::mpsc::channel();
    let followers = super::cot_follower::test_support::followers(log.clone(), gate_rx);

    let calls = run_follow_turn(
        &frontend,
        dir.path(),
        Some(&followers),
        FollowTurn {
            in_turn_sub: &[],
            outcome: Ok(crate::capture::TurnOutcome {
                session_id: Some("sid-1".into()),
                reply: Some("已派出,稍后汇报".into()),
                subagent_tail: Some(Box::new(tail)),
                ..Default::default()
            }),
            ..FollowTurn::clean()
        },
    );
    assert!(!calls.contains(&"finish_text"), "{calls:?}");
    assert!(
        position(&calls, "send") < position(&calls, "detach"),
        "{calls:?}"
    );
    assert_eq!(
        log.lock().unwrap().count("RUN_FINISHED"),
        0,
        "子 agent 还没动静,CoT 不能收口"
    );

    // 回复已经发出,子 agent 这才开始写转录。
    std::fs::write(
        subagents.join("agent-a1c7b6848c7434fad.meta.json"),
        r#"{"agentType":"adversarial-reviewer","description":"审一下","toolUseId":"toolu_async"}"#,
    )
    .unwrap();
    std::fs::write(
        subagents.join("agent-a1c7b6848c7434fad.jsonl"),
        format!("{CLAUDE_SUB_BASH}\n{CLAUDE_SUB_BASH_RESULT}\n{CLAUDE_SUB_REPORT}\n"),
    )
    .unwrap();

    let ends = super::cot_follower::test_support::drive_to_end(&followers, &gate_tx);
    assert_eq!(ends, [super::cot_follower::FollowEnd::AllFinished]);
    let log = log.lock().unwrap();
    let events = log.events();
    let starts = cot_tool_starts(&events);
    assert!(
        starts.iter().any(|(_, title)| title == "git diff --stat"),
        "子 agent 的工具行进了这一轮的 CoT:{starts:?}"
    );
    assert_eq!(log.count("RUN_FINISHED"), 1);
    assert_eq!(events.last().unwrap().event_type, "RUN_FINISHED");
    assert!(!reasoning_text(&events).contains("仍在运行"));
    assert_eq!(log.releases, [true]);
}

/// 跟随的接线(静态钉住,去空白比对):只有 claude/Transcript 的消费核把跟随登记交给消费器,
/// codex 等无头路传 `None`;登记由 chat 线程在配置重载循环之外建一份,析构即会话退出。
#[test]
fn only_the_transcript_turn_hands_its_cot_to_followers() {
    fn squash(code: &str) -> String {
        code.split_whitespace().collect()
    }
    let chat = squash(
        include_str!("chat_thread.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap(),
    );
    let consumers: Vec<String> = chat
        .match_indices("card_stream_consume_with_cot(")
        .map(|(at, _)| chat[at..].chars().take(1500).collect())
        .filter(|call: &String| !call.starts_with("card_stream_consume_with_cot(frontend:"))
        .collect();
    assert_eq!(consumers.len(), 3, "消费器调用点数变了,同步检查跟随接线");
    let transcript = consumers
        .iter()
        .filter(|call| {
            call.contains("subagent_state,cot_followers,&muton_steer_delivered_anchored")
        })
        .count();
    let headless = consumers
        .iter()
        .filter(|call| call.contains("None,//无头路没有subagent概念None,"))
        .count();
    let test_wrapper = consumers
        .iter()
        .filter(|call| {
            call.contains("subagent_state,None,on_steer_ack,")
                && call.contains("run_source(on_msg,on_str,on_tool,on_ev,on_int,on_tick,on_steer)")
        })
        .count();
    assert_eq!(
        (transcript, headless, test_wrapper),
        (1, 1, 1),
        "{consumers:#?}"
    );

    let outer = &chat[chat.find("pub(super)fnchat_thread(").unwrap()..];
    let outer = &outer[..outer.find("pub(super)enumChatThreadOutcome").unwrap()];
    let created = outer
        .find("letcot_followers=super::cot_follower::CotFollowers::feishu(fc.clone());")
        .expect("chat 线程建一份跟随登记");
    let reload_loop = outer.find("loop{matchchat_thread_run(").expect("重载循环");
    assert!(created < reload_loop, "登记要活过配置重载");
}

/// 子 agent 的事件不写 events.jsonl。
#[test]
fn subagent_events_stay_out_of_the_event_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("threads").join("g1").join("events.jsonl");
    let mut sink = crate::observe::event_log::EventSink::open(
        dir.path(),
        "g1",
        summon_core::event_log::EventStamp::default(),
    )
    .unwrap();
    let mut sub = sub_event("toolu_agent", summon_core::agent_msg::AgentKind::Assistant);
    sub.text = Some("子 agent".into());
    sub.stop_reason = Some("end_turn".into());
    sink.write(&sub);
    sink.write(&main_result("toolu_main"));
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(written.lines().count(), 1, "{written}");
    assert!(!written.contains("子 agent"));
}

/// claude 的子 agent 行:任务说明只在思考可见时接进标题。
#[test]
fn claude_agent_call_reaches_the_cot_with_its_description_only_when_thinking_is_visible() {
    use summon_core::agent_msg::{AgentKind, AgentMessage};

    let events = || {
        let mut agent = AgentMessage::of(AgentKind::ToolUse);
        agent.tool = Some("Agent".into());
        agent.tool_id = Some("toolu_1".into());
        agent.tool_input = Some(serde_json::json!({
            "subagent_type": "Explore", "description": "找入口", "prompt": "看一下"
        }));
        let mut fetch = AgentMessage::of(AgentKind::ToolUse);
        fetch.tool = Some("WebFetch".into());
        fetch.tool_id = Some("toolu_2".into());
        fetch.tool_input = Some(serde_json::json!({
            "url": "https://docs.rs/serde", "prompt": "总结"
        }));
        vec![agent, fetch]
    };
    assert_eq!(
        cot_tool_titles(events(), summon_core::progress::TurnProgress::new()),
        ["Agent Explore: 找入口", "WebFetch docs.rs/serde"]
    );
    assert_eq!(
        cot_tool_titles(
            events(),
            summon_core::progress::TurnProgress::new().with_hide_thinking(true)
        ),
        ["Agent Explore", "WebFetch docs.rs/serde"]
    );
}

#[test]
fn hidden_cot_begin_failure_is_latched_for_the_rest_of_the_turn() {
    use summon_core::agent_msg::{AgentKind, AgentMessage};

    let frontend = EmitterFrontend::new();
    frontend.fail_begin.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_hidden_cot_begin_failure");
    let mut projector = TurnProjector::new(
        CotEmitter::new(None),
        summon_core::progress::TurnProgress::new(),
        None,
    )
    .with_cot_visible(false);

    for text in ["first", "second", "third"] {
        let mut event = AgentMessage::of(AgentKind::Thinking);
        event.text = Some(text.into());
        projector.project_event(&frontend, &chat, "tester", &event);
        projector.tick(&frontend, &chat, "tester", None);
    }

    assert_eq!(
        frontend
            .calls
            .borrow()
            .iter()
            .filter(|call| **call == "begin")
            .count(),
        1,
        "联网 create 的结果可能不明确，同一轮只能尝试一次"
    );
}

#[test]
fn process_only_provider_failure_uses_the_structured_error_terminal() {
    let frontend = EmitterFrontend::new();
    frontend.process_only.set(true);
    let mut projector = TurnProjector::new(
        CotEmitter::new(Some(StreamId(12))),
        summon_core::progress::TurnProgress::new(),
        None,
    );
    finish_leftover_live_card(
        &frontend,
        "tester",
        &mut projector,
        LeftoverCardOutcome::Failed(AbortReason::ApiError),
    );
    assert_eq!(&*frontend.calls.borrow(), &["fail"]);
}

#[test]
fn process_only_timeout_closes_as_timeout_before_sending_partial_reply() {
    let frontend = EmitterFrontend::new();
    frontend.process_only.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_timeout",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (out, emitted) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false,
        None,
        None,
        None,
        &test_outbox(dir.path()),
        &|| None,
        None,
        &mut |_: &[String]| {},
        |_on_msg, _on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            let mut ev = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Assistant,
            );
            ev.text = Some("超时前的部分回复".into());
            on_event(&ev);
            Ok(crate::capture::TurnOutcome {
                reply: Some("超时前的部分回复".into()),
                timed_out: true,
                ..Default::default()
            })
        },
    );
    assert!(out.unwrap().timed_out);
    assert!(emitted);
    assert_eq!(&*frontend.calls.borrow(), &["begin", "timeout", "send"]);
}

/// 回归 codex 静默轮:reasoning summary 开了流式卡,最终 reply 为空 —— 终态对账 / 双卡收尾 /
/// 暂停定格三类收卡点全走不到,此前这张卡永远停在「进行中」,「⏸ 打断」按钮不消失。
/// 轮末兜底必须把它定格成无按钮终态,且恰好一次。
#[test]
fn silent_turn_backstop_finishes_the_leftover_live_card_exactly_once() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_silent_turn",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };

    let (out, emitted) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false, // tool_trail: 关(测试默认)
        None,
        None,
        None, // badge_seed:测试不校验角标
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        |_on_msg, _on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            // 只有 reasoning summary(Thinking 段)开卡,没有任何终态回复回调 → 静默轮。
            let mut ev = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Thinking,
            );
            ev.text = Some("推理摘要:先看看仓库结构".into());
            on_event(&ev);
            Ok(crate::capture::TurnOutcome::default())
        },
    );

    assert!(out.is_ok());
    assert!(emitted, "卡已发出,不能误判为空轮未发内容");
    assert_eq!(
        &*frontend.calls.borrow(),
        &["begin", "finish_text"],
        "兜底必须收卡且只收一次"
    );
}

/// 正常有回复的轮:收卡点(终态对账 / 双卡折叠定格)成功后已清 stream_id,轮末兜底必须是
/// no-op —— 远端映射在收卡时已摘,二次定格只会报错或改写终态。
#[test]
fn reply_turn_collect_points_clear_stream_id_so_backstop_is_noop() {
    // 形态1:无可折叠过程 → 单卡路径,deliver_final_reply 的纯文本对账收卡。
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_reply_single",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (out, _) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false, // tool_trail: 关(测试默认)
        None,
        None,
        None, // badge_seed:测试不校验角标
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        |on_msg, _on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            let mut ev = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Assistant,
            );
            ev.text = Some("最终回复".into());
            on_event(&ev);
            on_msg("最终回复");
            Ok(crate::capture::TurnOutcome {
                reply: Some("最终回复".into()),
                ..Default::default()
            })
        },
    );
    assert!(out.is_ok());
    assert_eq!(
        &*frontend.calls.borrow(),
        &["begin", "finish_text", "send"],
        "终态对账已收卡,兜底不得再定格"
    );

    // 形态2:有可折叠过程 → 双卡收尾,finalize_turn_summary 的折叠定格收卡。
    let frontend = EmitterFrontend::new();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_reply_split",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (out, _) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false, // tool_trail: 关(测试默认)
        None,
        None,
        None, // badge_seed:测试不校验角标
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        |on_msg, _on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            for (kind, text) in [
                (summon_core::agent_msg::AgentKind::Thinking, "先想一想"),
                (summon_core::agent_msg::AgentKind::Assistant, "结论"),
            ] {
                let mut ev = summon_core::agent_msg::AgentMessage::of(kind);
                ev.text = Some(text.to_string());
                on_event(&ev);
            }
            on_msg("结论");
            Ok(crate::capture::TurnOutcome {
                reply: Some("结论".into()),
                ..Default::default()
            })
        },
    );
    assert!(out.is_ok());
    assert_eq!(
        &*frontend.calls.borrow(),
        &["begin", "finish_text", "card"],
        "折叠定格已收卡(finish 恰一次)+ 总结卡,兜底不得再定格"
    );
}

/// 被「⏸ 打断」纯停的轮:过程卡已定格「已暂停」,轮末兜底不得把这个终态改写成「已完成」。
#[test]
fn backstop_does_not_rewrite_the_paused_terminal_card() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_paused_turn",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (out, _) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false, // tool_trail: 关(测试默认)
        None,
        None,
        None, // badge_seed:测试不校验角标
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        |_on_msg, on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            let mut ev = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Assistant,
            );
            ev.text = Some("写到一半".into());
            on_event(&ev);
            on_stream("写到一半");
            Ok(crate::capture::TurnOutcome {
                interrupted: true,
                ..Default::default()
            })
        },
    );
    assert!(out.is_ok());
    assert_eq!(
        &*frontend.calls.borrow(),
        &["begin", "finish_text"],
        "暂停定格收卡后,兜底不得对同一张卡再来一刀"
    );
    let card = frontend.payloads.borrow().last().cloned().unwrap();
    assert!(card.contains("本轮已打断"), "终态必须是「已暂停」: {card}");
    assert!(
        !card.contains("已完成"),
        "兜底不得把暂停终态改写成完成: {card}"
    );
}

/// claude 静默路径:整轮没开过流式卡(stream_id 恒 None)→ 兜底不得无中生有创建任何卡。
#[test]
fn backstop_creates_nothing_when_no_live_card_was_opened() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_silent_no_card",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (out, emitted) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false, // tool_trail: 关(测试默认)
        None,
        None,
        None, // badge_seed:测试不校验角标
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        |_on_msg, _on_stream, _on_tool, _on_event, _on_interactive, _on_tick, _on_steer| {
            Ok(crate::capture::TurnOutcome::default())
        },
    );
    assert!(out.is_ok());
    assert!(!emitted, "没发过任何内容");
    assert!(
        frontend.calls.borrow().is_empty(),
        "无卡可收时兜底必须零出站: {:?}",
        frontend.calls.borrow()
    );
}

/// 终审阻塞1回归:run_source 以 Err 收尾时,轮末兜底仍要收卡(卡开了就必须有人收),但必须按
/// **中止**语义定格 —— 上层随后还会发失败回执(报因)、贴 ERROR,这张卡若渲成「✅ 已完成」,
/// 用户会同时看到「已完成」和「失败」两个互相矛盾的终态。定格过程卡收过程、回执报原因,互补不矛盾。
#[test]
fn err_turn_backstop_freezes_card_as_aborted_not_completed() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_err_turn",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (out, emitted) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false, // tool_trail: 关(测试默认)
        None,
        None,
        None, // badge_seed:测试不校验角标
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        |_on_msg, _on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            // 思考已开出流式卡,随后引擎失败 → Err 收尾。
            let mut ev = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Thinking,
            );
            ev.text = Some("推理到一半:这段思考已经渲到了流式卡上".into());
            on_event(&ev);
            Err(anyhow::anyhow!("engine exploded"))
        },
    );

    assert!(out.is_err());
    assert!(emitted, "卡已发出,不能误判为空轮未发内容");
    assert_eq!(
        &*frontend.calls.borrow(),
        &["begin", "fail"],
        "Err 收尾也必须收卡,且只收一次"
    );
    let card = frontend.payloads.borrow().last().cloned().unwrap();
    assert!(
        card.contains("provider 执行失败"),
        "Err 收尾应定格为中止终态: {card}"
    );
    assert!(
        !card.contains("已完成"),
        "Err 收尾不得渲「已完成」——上层马上要报失败,两卡不能自相矛盾: {card}"
    );
    assert!(!card.contains("打断"), "终态卡不得再带打断按钮: {card}");
}

/// 终审阻塞3回归(回调路:card_stream_consume,Transcript + 同步无头共用):
/// `TurnOutcome::api_error` 是装在 `Ok` 里的失败通道 —— 只 reasoning、以 api_error 收束的
/// 静默轮,上层按 Failed 贴 ERROR/发回执;此前这里把所有非打断的 `Ok(_)` 当正常完成,
/// 兜底渲「✅ 已完成」,与失败回执同屏。唯一判据 `turn_finished_cleanly` 必须把它拦下:
/// 跳过完成总结,轮末兜底按**中止**定格。
#[test]
fn api_error_turn_backstop_freezes_card_as_aborted_not_completed() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_api_error_turn",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (out, emitted) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false, // tool_trail: 关(测试默认)
        None,
        None,
        None, // badge_seed:测试不校验角标
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        |_on_msg, _on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            // 只有 reasoning 开出流式卡,随后以 API 错误收束 —— 形态上是 Ok,语义上是失败。
            let mut ev = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Thinking,
            );
            ev.text = Some("推理到一半:这段思考已经渲到了流式卡上".into());
            on_event(&ev);
            Ok(crate::capture::TurnOutcome {
                api_error: Some("model_not_found: 请求的模型不存在".into()),
                ..Default::default()
            })
        },
    );

    assert!(
        out.is_ok(),
        "api_error 装在 Ok 里,正是它容易被误判成正常完成的原因"
    );
    assert!(emitted, "卡已发出,不能误判为空轮未发内容");
    assert_eq!(
        &*frontend.calls.borrow(),
        &["begin", "fail"],
        "api_error 轮跳过完成总结,只由兜底收卡一次"
    );
    let card = frontend.payloads.borrow().last().cloned().unwrap();
    assert!(
        card.contains("provider 执行失败"),
        "api_error 轮应定格为中止终态: {card}"
    );
    assert!(
        !card.contains("已完成"),
        "api_error 轮不得渲「已完成」——上层按 Failed 贴 ERROR/发回执,两个终态不能同屏: {card}"
    );
    assert!(!card.contains("打断"), "终态卡不得再带打断按钮: {card}");
}

/// 终审阻塞回归:「跳过完成总结」≠「跳过投递对账」。Transcript 的 stream-stall 会把已生成
/// partial 当权威 final_text 走 deliver_final_reply(首段终态发送成功、超长后续分段发送失败 →
/// report.complete=false),随后本轮以 api_error 收束。完成总结照旧跳过、绝不渲「已完成」,
/// 但这份已产生的 DeliveryReport 必须仍被对账:权威全文入 outbox durable retry。对账若留在
/// 完成 gate 内,report 随 gate 一起被丢,失败尾段永久丢失 —— 事后的 stream-stall 卡只是
/// best-effort 且裁剪超长 partial,替代不了 durable retry(变异自验:把 reconcile 移回
/// gate 内,本测试的 outbox 断言必红)。
#[test]
fn api_error_turn_still_enqueues_partial_delivery_for_durable_retry() {
    let frontend = EmitterFrontend::new();
    // 首段终态 send_message 成功，超长后续分段 send_message 全部失败。
    frontend.fail_send_after.set(Some(1));
    let dir = tempfile::tempdir().unwrap();
    let outbox = test_outbox(dir.path());
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_api_error_stall",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let body = "x".repeat(summon_core::feishu::CARD_TEXT_SAFE_LIMIT * 2);
    let body_run = body.clone();
    let (out, emitted) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false, // tool_trail: 关(测试默认)
        None,
        None,
        None, // badge_seed:测试不校验角标
        &outbox,
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        move |on_msg, _on_stream, _on_tool, _on_event, _on_interactive, _on_tick, _on_steer| {
            // stream-stall:已生成 partial 被当作权威终态投递……
            on_msg(&body_run);
            // ……随后本轮以 API 流中断收束(Ok 里的失败通道)。
            Ok(crate::capture::TurnOutcome {
                reply: Some(body_run),
                api_error: Some("API Error: Response stalled mid-stream".into()),
                ..Default::default()
            })
        },
    );

    assert!(out.is_ok(), "api_error 装在 Ok 里");
    assert!(emitted, "首段已送达,不能误判为空轮");
    for payload in frontend.payloads.borrow().iter() {
        assert!(
            !payload.contains("已完成"),
            "api_error 轮任何出站都不得渲「已完成」: {payload}"
        );
    }
    let due = outbox.list_due(now_unix_ms() + 1);
    assert_eq!(
        due.len(),
        1,
        "未完整送达的终态回复必须入 outbox durable retry"
    );
    match &due[0].kind {
        summon_core::outbox::OutboxKind::TurnReply { chat_id, text, .. } => {
            assert_eq!(chat_id, "oc_api_error_stall");
            assert_eq!(text, &body, "重试载荷必须是权威全文,不得裁剪");
        }
        other => panic!("应为 TurnReply,实为 {other:?}"),
    }
}

/// 支持 checkpoint 回填/导出的测试前端,模拟飞书前端的 stream_state 能力。
/// 刻意不并进 `EmitterFrontend`:`new_live_card_without_export_clears_old_checkpoint`
/// 依赖它「无导出能力」的形态。
struct RecoverableFrontend {
    calls: std::cell::RefCell<Vec<&'static str>>,
}

impl RecoverableFrontend {
    fn new() -> Self {
        Self {
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl Frontend for RecoverableFrontend {
    fn send_message(&self, _chat: &ChatRef, _body: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("send");
        Ok(())
    }

    fn stream_begin(
        &self,
        _chat: &ChatRef,
        _content: &str,
        _with_interrupt: bool,
    ) -> anyhow::Result<StreamId> {
        self.calls.borrow_mut().push("begin");
        Ok(StreamId(1))
    }

    fn stream_patch(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("patch");
        Ok(())
    }

    fn stream_finish(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("finish_text");
        Ok(())
    }

    fn stream_finish_card(&self, _id: StreamId, _card_json: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("finish");
        Ok(())
    }

    fn send_card(&self, _chat: &ChatRef, _card_json: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("card");
        Ok(())
    }

    fn react(&self, _msg: &MsgRef, _key: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn unreact(&self, _msg: &MsgRef, _reaction_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 终审阻塞2回归:同步无头恢复轮(saved_stream_state 接回旧卡续刷)跑到一半,完成权被 daemon
/// shutdown 原子接管(on_finish → ShutdownClaimed,闭包把收尾改写成 Err)。此时轮末兜底必须
/// **零定格**:不许把下一代还要续刷的卡刷成任何终态,盘上 stream_state.json 原样保留 ——
/// 上层同时保留 checkpoint 等下一代续跑,定格与保留不能互相矛盾。
#[test]
fn shutdown_claimed_recovery_turn_preserves_stream_state_with_zero_freeze() {
    let frontend = RecoverableFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_shutdown_claimed",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let tag = chat_tag(chat.chat_type, chat.chat_id);
    let saved = summon_core::session::stream_card::StreamCardState {
        message_id: "om_prev_turn".into(),
        chat_tag: tag.clone(),
        with_interrupt: true,
        created_ms: now_unix_ms(),
    };
    crate::runtime::inflight::write_stream_state(dir.path(), &tag, &saved).unwrap();

    let (out, _emitted) = card_stream_consume(
        &frontend,
        &chat,
        true,
        summon_core::progress::DEFAULT_MAX_STEPS,
        false,
        false, // tool_trail: 关(测试默认)
        Some(&saved),
        None,
        None, // badge_seed:测试不校验角标
        &test_outbox(dir.path()),
        &|| Some(crate::runtime::inflight::FinishDisposition::ShutdownClaimed),
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        |_on_msg, _on_stream, _on_tool, on_event, _on_interactive, _on_tick, _on_steer| {
            // 续刷旧卡(文本须超过 40 字节流式节流阈值才会真的 patch)……
            let mut ev = summon_core::agent_msg::AgentMessage::of(
                summon_core::agent_msg::AgentKind::Thinking,
            );
            ev.text =
                Some("恢复轮续刷:这段思考要足够长,越过流式渲染的四十字节节流阈值刷上旧卡".into());
            on_event(&ev);
            // ……随后完成权被停机接管:同步路把 Ok 改写成 Err、抑制终态副作用(见 stream_turn_headless)。
            Err(anyhow::anyhow!(
                "本轮完成权已由 daemon shutdown 接管，终态副作用已抑制"
            ))
        },
    );

    assert!(out.is_err());
    assert_eq!(
        &*frontend.calls.borrow(),
        &["begin", "finish_text"],
        "新 CoT 在 daemon 停机前收口"
    );
    assert!(
        crate::runtime::inflight::read_stream_state(dir.path(), &tag).is_none(),
        "本 chat 的旧过程卡 checkpoint 已清理"
    );
}

#[test]
fn exclusive_lock_enforces_singleton() {
    let path = std::env::temp_dir().join(format!("summon-lock-test-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let first = try_exclusive_lock(&path).unwrap();
    assert!(first.is_some(), "首个进程应拿到锁");
    let second = try_exclusive_lock(&path).unwrap();
    assert!(second.is_none(), "第二个应被挡(单例)");
    drop(first); // 释放锁
    let third = try_exclusive_lock(&path).unwrap();
    assert!(third.is_some(), "释放后可重新拿到");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn daemon_identity_requires_live_pid_matching_held_lock_owner() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("daemon.lock");
    let pid_path = dir.path().join("daemon.pid");
    let pid = 4242;
    let mut lock = try_exclusive_lock(&lock_path).unwrap().unwrap();
    lock.set_len(0).unwrap();
    lock.write_all(pid.to_string().as_bytes()).unwrap();
    std::fs::write(&pid_path, pid.to_string()).unwrap();

    assert!(identity_matches_at(pid, &pid_path, &lock_path, |_| true));
    std::fs::write(&pid_path, "4343").unwrap();
    assert!(
        !identity_matches_at(pid, &pid_path, &lock_path, |_| true),
        "陈旧/复用 PID 与当前 lock owner 不一致时必须拒绝"
    );
    std::fs::remove_file(&pid_path).unwrap();
    assert!(lock_still_owned_by_at(pid, &lock_path));
    std::fs::write(&pid_path, pid.to_string()).unwrap();
    drop(lock);
    assert!(!lock_still_owned_by_at(pid, &lock_path));
    assert!(
        !identity_matches_at(pid, &pid_path, &lock_path, |_| true),
        "flock 已释放时，即使两份文件仍匹配也不是活 daemon"
    );
}

#[test]
fn identity_cleanup_is_compare_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.pid");
    std::fs::write(&path, "200").unwrap();
    assert!(!remove_identity_file_if_matches(&path, 100));
    assert!(path.exists(), "不得删除新一代 PID 文件");
    assert!(remove_identity_file_if_matches(&path, 200));
    assert!(!path.exists());
}

#[test]
fn daemon_version_identity_is_pid_bound_and_compare_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.version");
    write_daemon_version_identity(&path, 200, "0.1.202607130802").unwrap();

    let identity = read_daemon_version_identity_at(&path).unwrap();
    assert_eq!(identity.pid, 200);
    assert_eq!(identity.version, "0.1.202607130802");
    assert_eq!(
        daemon_version_at(&path, 200).as_deref(),
        Some("0.1.202607130802")
    );
    assert_eq!(
        daemon_version_at(&path, 100),
        None,
        "旧 daemon 不得认领新一代版本"
    );

    let bytes = std::fs::read(&path).unwrap();
    let decoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded["pid"], 200);
    assert_eq!(decoded["version"], "0.1.202607130802");

    assert!(!remove_daemon_version_if_matches_at(&path, 100));
    assert!(path.exists(), "旧 daemon 不得删除新一代版本身份");
    assert!(remove_daemon_version_if_matches_at(&path, 200));
    assert!(!path.exists());
}

#[test]
fn daemon_version_identity_rejects_corrupt_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.version");
    std::fs::write(&path, b"not-json").unwrap();
    assert!(read_daemon_version_identity_at(&path).is_none());
}

#[test]
fn pid_probe_rejects_invalid_pidfile_values() {
    assert!(!pid_is_alive(0), "pid=0 不得退化成探测当前进程组");
    assert!(
        !pid_is_alive(u32::MAX),
        "超出 i32 的损坏 pid 应 fail-closed"
    );
}

/// 回归(fbee83c):events.sock 的 listener 设非阻塞(为 accept 轮询看 shutdown flag),accept
/// 出的连接在 macOS 上**继承非阻塞**;若不置回阻塞,`handle_conn` 的 `lines().map_while(Result::ok)`
/// 会把「客户端刚连上、首条数据还没来」的 `WouldBlock` 误判流结束、提前关连接——sidecar 正是
/// 「先连、几秒后才发首条事件」的持久连接,会被全程误关、入站全丢。本测复现「延迟首行」:置回
/// 阻塞后,延迟写的首行必须能读到(连接没被误关)。
#[test]
fn accepted_conn_set_blocking_survives_delayed_first_line() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ev.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap(); // 复现生产:listener 非阻塞(accept 轮询)
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((c, _)) => {
                // 修复点:置回阻塞(否则继承非阻塞 → 首读 WouldBlock → map_while 误判结束、关连接)。
                c.set_nonblocking(false).unwrap();
                let first = BufReader::new(c).lines().map_while(Result::ok).next();
                let _ = tx.send(first);
                return;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    });
    // 客户端:连上后**延迟**再写首行,模拟 sidecar「先连、几秒后(WS connected)才发首条事件」。
    let mut cli = UnixStream::connect(&path).unwrap();
    std::thread::sleep(Duration::from_millis(80));
    cli.write_all(b"hello\n").unwrap();
    cli.flush().unwrap();
    let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    h.join().unwrap();
    assert_eq!(
        got.as_deref(),
        Some("hello"),
        "延迟写的首行应被读到(连接没被非阻塞 WouldBlock 误关)"
    );
}

/// 真跨语言协议回归：Go production sink → Unix socket → Rust production `handle_conn` →
/// durable WAL → Core route → production dispatcher → Bridge CAS → 同一 WAL consumed。另验证
/// 旧/错误协议即时 NACK。全程只用 tempdir/dry-run Feishu，不读取真实配置或创建 tmux。
#[test]
fn go_sidecar_and_rust_daemon_round_trip_protocol_over_unix_socket() {
    use std::io::ErrorKind;
    use std::process::Command;
    use summon_core::bridge::{
        BridgeChoice, BridgeMode, BridgeTicket, ChatBinding, ChatSlot, ExternalLifecycle,
        ExternalLocator, ExternalSessionRecord, ResumeProfile, ScopedChatKey, SessionKey,
        SessionLease, TicketStatus,
    };

    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("events.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let wal = crate::runtime::card_action_wal::CardActionWal::new(temp.path().join("card-wal"));
    let (agent, _agent_dir) =
        crate::test_support::agent_dir("claude", "feishu:\n  operators: [ou_owner]\n");
    let scope = feishu_storage_scope(&agent).key;
    let target = ScopedChatKey::new(&scope, &agent.name, "oc_cross_group");
    let session = SessionKey::new("/tmp/cross-config", "sid-cross");
    let binding = ChatBinding {
        id: "binding-cross".into(),
        target: target.clone(),
        target_chat_type: "group".into(),
        session: session.clone(),
        mode: BridgeMode::Mirror,
        actor_open_id: "ou_owner".into(),
        created_at_ms: 1,
        revision: 1,
    };
    let bridge_store = crate::bridge::store::BridgeStore::new(temp.path().join("bridge"));
    bridge_store
        .with_state(|state| {
            state.upsert_session(ExternalSessionRecord {
                key: session.clone(),
                generation: 1,
                locator: ExternalLocator {
                    key: session.clone(),
                    transcript_path: "/tmp/cross-config/sid-cross.jsonl".into(),
                    resume_lookup_cwd: "/tmp".into(),
                    current_cwd: "/tmp".into(),
                    profile: ResumeProfile::default(),
                    revision: 1,
                },
                process: None,
                lifecycle: ExternalLifecycle::Ended,
                first_seen_ms: 1,
                last_seen_ms: 1,
                last_applied_sequence: 1,
                pending_prompt: None,
            });
            state
                .slots
                .insert(target.storage_key(), ChatSlot::MirrorBound(binding.clone()));
            state.session_leases.insert(
                session.storage_key(),
                SessionLease {
                    binding_id: binding.id.clone(),
                    target: target.clone(),
                },
            );
            state.tickets.insert(
                "ticket-opaque".into(),
                BridgeTicket {
                    id: "ticket-opaque".into(),
                    short_code: None,
                    target: target.clone(),
                    target_chat_type: "group".into(),
                    target_provider: summon_core::bridge::ClaudeProvider::Default,
                    requester_open_id: "ou_owner".into(),
                    requester_union_id: String::new(),
                    control_card_message_id: Some("om_control".into()),
                    private_chat_id: Some("oc_private".into()),
                    slot_revision: 1,
                    choices: vec![BridgeChoice {
                        id: "choice-opaque".into(),
                        session: session.clone(),
                        registry_revision: 1,
                    }],
                    issued_at_ms: 1,
                    expires_at_ms: 2,
                    status: TicketStatus::Committed {
                        binding_id: binding.id.clone(),
                        choice_id: "choice-opaque".into(),
                        mode: BridgeMode::Mirror,
                    },
                },
            );
            Ok(())
        })
        .unwrap();
    let (dispatch_tx, dispatch_rx) = channel();
    let core = Arc::new(crate::runtime::core::Core::for_routes(HashMap::from([(
        "cross-app".to_string(),
        dispatch_tx.clone(),
    )])));
    let dispatcher = {
        let dispatcher_agent = agent.clone();
        let dispatcher_epoch = super::FeishuEpochIdentity::capture(&dispatcher_agent);
        let dispatcher_core = Arc::clone(&core);
        let dispatcher_tx = dispatch_tx.clone();
        let dispatcher_wal = wal.clone();
        let stores = super::dispatcher::DispatcherStores {
            bridge: bridge_store.clone(),
            outbox: crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox")),
            inbox: crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox")),
            card_actions: dispatcher_wal,
            support_root: Some(temp.path().join("dispatcher-support")),
        };
        let client = Arc::new(FeishuClient::new(
            "feishu.cn",
            "test-app".into(),
            "test-secret".into(),
            true,
        ));
        thread::spawn(move || {
            super::dispatcher::agent_dispatcher_with_stores(
                dispatcher_agent,
                20,
                20,
                client,
                dispatcher_epoch,
                dispatch_rx,
                dispatcher_tx,
                dispatcher_core,
                crate::runtime::inflight::InFlight::new(),
                stores,
            )
        })
    };
    let server_wal = wal.clone();
    let server_core = Arc::clone(&core);
    let server = thread::spawn(move || -> Result<usize, String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut handled = 0usize;
        while handled < 3 && std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((conn, _)) => {
                    // 与 production accept loop 一致：macOS 可能让 accepted fd 继承 nonblocking。
                    conn.set_nonblocking(false)
                        .map_err(|error| error.to_string())?;
                    handle_conn_with_wal(conn, Arc::clone(&server_core), server_wal.clone());
                    handled += 1;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(handled)
    });

    let sidecar_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sidecar/feishu-connector");
    let output = Command::new("go")
        .args(["test", "-run", "^TestRustDaemonProtocolPeer$", "-count=1"])
        .current_dir(&sidecar_dir)
        .env("SUMMON_RUST_DAEMON_PROTOCOL_SOCKET", &socket)
        .output()
        .unwrap_or_else(|error| panic!("启动 Go sidecar 协议 peer 失败: {error}"));
    let handled = server.join().unwrap().unwrap();
    assert_eq!(handled, 3, "Go peer 应建立 v2、legacy、wrong 三条连接");
    assert!(
        output.status.success(),
        "Go sidecar 协议 peer 失败\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let stopped = bridge_store
            .load_state()
            .unwrap()
            .unwrap()
            .slot(&target)
            .is_none();
        let health = wal.inspect_health().unwrap();
        if stopped && health.pending == 0 && health.consumed == 1 && health.corrupt == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "跨语言 callback 未完成 Bridge CAS/WAL consumed: stopped={stopped} health={health:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    dispatch_tx.send(DispatchMsg::Shutdown).unwrap();
    dispatcher.join().unwrap();
    assert!(wal.pending().unwrap().is_empty());
}

#[test]
fn ensure_instruction_file_scaffolds_symlinks_and_reads() {
    // 脚手架必须**逐字节为空**:codex 走 AGENTS.md 软链读原始文件,注释也照样进上下文,
    // 所以「靠 strip_md_comments 兜住」的旧写法在那条路上不成立(见 claude_example_md())。
    assert!(
        claude_example_md().is_empty(),
        "脚手架不许带任何正文/注释,给人看的说明放 ~/.summon/docs/"
    );
    let tmp = std::env::temp_dir().join(format!("summon-instr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let claude = tmp.join("CLAUDE.md");

    // 首次:写脚手架(空文件),旁置 AGENTS.md → CLAUDE.md 软链
    let raw = ensure_instruction_file(&claude);
    assert!(claude.exists(), "CLAUDE.md 应被脚手架创建");
    let agents = tmp.join("AGENTS.md");
    assert!(agents.symlink_metadata().is_ok(), "AGENTS.md 应被建为软链");
    assert_eq!(
        std::fs::read_link(&agents).unwrap(),
        std::path::Path::new("CLAUDE.md"),
        "AGENTS.md 应相对软链到 CLAUDE.md"
    );
    // 脚手架为空 → 无论走 summon 拼 PRIMER(剥注释)还是走 codex 直读 AGENTS.md(不剥),
    // 注入层都为空。**两条路都要断言**,只测剥注释后为空会漏掉 codex 那条。
    assert!(
        raw.trim().is_empty(),
        "脚手架原始文本应为空(codex 直读这份)"
    );
    assert!(
        summon_core::config::strip_md_comments(&raw)
            .trim()
            .is_empty(),
        "脚手架注入层应为空"
    );

    // 已存在则不覆盖:写入真实要求后,再次 ensure 原样读回
    std::fs::write(&claude, "记得先跑测试再说完成").unwrap();
    assert_eq!(ensure_instruction_file(&claude), "记得先跑测试再说完成");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn chat_instruction_path_accepts_only_a_single_plain_component() {
    let mut agent: Agent = serde_yaml::from_str("name: a").unwrap();
    agent.dir = std::path::PathBuf::from("/agents/a");
    assert_eq!(
        chat_instruction_path(&agent, "oc_abc"),
        Some(std::path::PathBuf::from("/agents/a/chats/oc_abc/CLAUDE.md"))
    );
    for bad in ["", ".", "..", "../b", "oc/abc", "/etc"] {
        assert_eq!(chat_instruction_path(&agent, bad), None, "{bad:?}");
    }
}

#[test]
fn control_for_action_maps_known_and_defaults_keepalive() {
    assert_eq!(control_for_action("clean"), Control::Clean);
    assert_eq!(control_for_action("reset"), Control::Archive);
    assert_eq!(control_for_action("compact"), Control::Compact);
    assert_eq!(control_for_action("keepalive"), Control::KeepAlive);
    assert_eq!(control_for_action("garbage"), Control::KeepAlive);
}

#[test]
fn plan_config_switch_keeps_context_when_hot_path_exists() {
    // claude(tmux)在线:注入 `/model <m>` 就地切,不重置。
    let p = plan_config_switch(
        ModelSwitchSpec::TmuxSlash { prefix: "/model" },
        true,
        "模型已设为",
        "claude-opus-4-8",
    );
    match p {
        SwitchPlan::Apply { slash, receipt } => {
            assert_eq!(slash.as_deref(), Some("/model claude-opus-4-8"));
            assert!(matches!(receipt, CommandAck::Done(_)), "就地切换是纯确认");
            assert!(
                receipt.text().contains("上下文保留"),
                "回执须说明不丢上下文"
            );
        }
        other => panic!("claude 在线应走 Apply,得到 {other:?}"),
    }
    // 无头(cursor/codex)在线:不注入,只就地重载,下一轮自带新 flag。
    let p = plan_config_switch(
        ModelSwitchSpec::NextTurn,
        true,
        "模型已设为",
        "gpt-5.5-high",
    );
    match p {
        SwitchPlan::Apply { slash, receipt } => {
            assert_eq!(slash, None);
            assert!(matches!(receipt, CommandAck::Done(_)));
            assert!(receipt.text().contains("下一条消息起生效"));
        }
        other => panic!("无头在线应走 Apply(无 slash),得到 {other:?}"),
    }
}

/// 改默认时追加的影响范围用户从指令里推不出,回执随之照常发文字。
#[test]
fn a_default_scope_note_turns_the_receipt_into_a_note() {
    let mut p = plan_config_switch(
        ModelSwitchSpec::NextTurn,
        true,
        "默认模型已设为",
        "gpt-5.5-high",
    );
    p.append_note("其他没有本群模型的群下一条消息生效。");
    match p {
        SwitchPlan::Apply {
            receipt: CommandAck::Note(text),
            ..
        } => assert!(
            text.ends_with("其他没有本群模型的群下一条消息生效。"),
            "{text}"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn plan_config_switch_offline_writes_only_and_restart_is_fallback() {
    // 不在线:写盘已够,直接回执,绝不为切模型误起会话。
    let p = plan_config_switch(
        ModelSwitchSpec::TmuxSlash { prefix: "/model" },
        false,
        "模型已设为",
        "claude-sonnet-4-6",
    );
    assert!(matches!(p, SwitchPlan::ReceiptOnly(CommandAck::Done(_))));
    let p = plan_config_switch(
        ModelSwitchSpec::NextTurn,
        false,
        "模型已设为",
        "gpt-5.5-codex",
    );
    assert!(matches!(p, SwitchPlan::ReceiptOnly(CommandAck::Done(_))));
    // 无热切通道(claude 的 effort / 未知 CLI):退回快重置——唯一丢上下文的路。
    let p = plan_config_switch(ModelSwitchSpec::Restart, true, "effort 已设为", "high");
    match p {
        // 上下文没了,回执照常发文字。
        SwitchPlan::ResetThenReceipt(CommandAck::Note(text)) => {
            assert!(text.contains("会话已重置"))
        }
        other => panic!("Restart 应走 ResetThenReceipt,得到 {other:?}"),
    }
}

#[test]
fn inbound_serde_round_trips() {
    // RR2.1:入站 tee/回放靠 serde。造一条带富载元数据的入站,JSON round-trip 后逐字段不丢。
    let msg = DispatchMsg::inbound_for_test("oc_chat", "你好");
    let DispatchMsg::Inbound(mut inb) = msg else {
        panic!("inbound_for_test 应造 Inbound");
    };
    inb.chat_type = Some("group".to_string());
    inb.parent_id = Some("om_parent".to_string());
    inb.thread_id = Some("omt_topic".to_string());
    inb.root_id = Some("om_root".to_string());
    inb.mentions = vec![summon_core::ipc::Mention {
        open_id: "ou_bot".to_string(),
        key: "@_user_1".to_string(),
        name: "bot".to_string(),
        ..Default::default()
    }];
    inb.resources = vec![summon_core::ipc::Resource {
        file_key: "img_key".to_string(),
        kind: "image".to_string(),
        name: String::new(),
    }];
    let json = serde_json::to_string(&inb).unwrap();
    let back: Inbound = serde_json::from_str(&json).unwrap();
    assert_eq!(back.chat_id, inb.chat_id);
    assert_eq!(back.chat_type, inb.chat_type);
    assert_eq!(back.text, inb.text);
    assert_eq!(back.sender, inb.sender);
    assert_eq!(back.message_id, inb.message_id);
    assert_eq!(back.parent_id, inb.parent_id);
    assert_eq!(back.thread_id, inb.thread_id);
    assert_eq!(back.root_id, inb.root_id);
    assert_eq!(back.mentions, inb.mentions);
    assert_eq!(back.resources, inb.resources);
}

#[test]
fn card_action_msg_serde_round_trips() {
    let card = CardActionMsg {
        chat_id: "oc_chat".to_string(),
        message_id: "om_card".to_string(),
        operator_open_id: "ou_op".to_string(),
        event_id: "card-event-id".to_string(),
        value: serde_json::json!({"action": "reset", "form_value": {"work_dir": "/tmp"}}),
        wal_id: None,
        wal_replayed: false,
    };
    let json = serde_json::to_string(&card).unwrap();
    let back: CardActionMsg = serde_json::from_str(&json).unwrap();
    assert_eq!(back.chat_id, card.chat_id);
    assert_eq!(back.message_id, card.message_id);
    assert_eq!(back.operator_open_id, card.operator_open_id);
    assert_eq!(back.event_id, card.event_id);
    assert_eq!(back.value, card.value);
}

#[test]
fn finalize_card_updates_in_place_and_archives() {
    // U4:被点过的交互卡片应**原地替换**成终态(update_card 原卡 message_id),并归档一条
    // card_update 到 outbound.ndjson——而不是新发一张、旧卡按钮永远可点。
    let tmp = std::env::temp_dir().join(format!("summon-finalize-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent = serde_yaml::from_str("name: nimbus").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
    let chats: HashMap<String, ChatHandle> = HashMap::new();
    let card = CardActionMsg {
        chat_id: "oc_done".to_string(),
        message_id: "om_card_1".to_string(),
        operator_open_id: "ou_op".to_string(),
        event_id: String::new(),
        value: serde_json::json!({}),
        wal_id: None,
        wal_replayed: false,
    };
    finalize_card(&agent, &fc, &chats, &card, "✅ 计划已通过");
    let p = agent
        .state_dir()
        .join("threads")
        .join(chat_tag("p2p", "oc_done"))
        .join("outbound.ndjson");
    let s = std::fs::read_to_string(&p).expect("终态替换应归档 outbound.ndjson");
    let v: serde_json::Value = serde_json::from_str(s.lines().next().unwrap()).unwrap();
    assert_eq!(v["kind"], "card_update", "应是原地更新而非新发卡");
    assert_eq!(
        v["payload"]["message_id"], "om_card_1",
        "更新目标应是被点的那张卡"
    );
    // 被点的交互卡都是 2.0,飞书拒绝跨 schema 改写;预览跟着终态文字走。
    assert_eq!(v["payload"]["card"]["schema"], "2.0");
    assert_eq!(
        v["payload"]["card"]["config"]["summary"]["content"],
        "✅ 计划已通过"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 卡片按钮点出来的退出:结果已经写在用户刚点的那张卡上,不该再补一张「♻️ 会话已重置」。
/// 只有原卡没承载住(非卡片触发 / PATCH 失败)才回落到退出回执卡——退出不能对用户彻底静默。
#[test]
fn exit_receipt_yields_to_card_that_already_shows_final_state() {
    assert!(
        !should_notify_exit(true, true),
        "原卡已成终态 → 不再发回执卡(同一件事说两遍)"
    );
    assert!(
        should_notify_exit(true, false),
        "原卡没承载住 → 必须回落到回执卡"
    );
    // 配置变更快重置本就不是「退出」,与卡片承载与否无关,一律不发。
    assert!(!should_notify_exit(false, false));
    assert!(!should_notify_exit(false, true));
}

/// 空闲卡点「退出(唤醒时新建session)」:原卡就地 PATCH 成空闲布局终态并报 true,
/// 上层据此吞掉退出回执卡。归档的必须是 `card_update`(原地改)且是**空闲布局**(带 footer 话术),
/// 不能漂成 /session 面板。
#[test]
fn mark_session_card_closed_reports_idle_card_carried_final_state() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: nimbus").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
    let carried = mark_session_card_closed(
        &fc,
        &agent,
        "p2p",
        "oc_idle",
        Some("om_idle_card"),
        &[],
        true,
        CardOrigin::Idle(IdleCardState {
            idle_secs: 86400,
            context: Some(
                summon_core::context_budget::ContextBudget::from_reported_pct(42, Some(200_000)),
            ),
        }),
    );
    assert!(carried, "原卡 PATCH 成功 = 终态已写在用户眼前");
    let p = agent
        .state_dir()
        .join("threads")
        .join(chat_tag("p2p", "oc_idle"))
        .join("outbound.ndjson");
    let s = std::fs::read_to_string(&p).expect("应归档一条原地更新");
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 1, "只该有原地更新这一条,不许再补发任何消息");
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["kind"], "card_update", "应是原地更新而非新发卡");
    assert_eq!(v["payload"]["message_id"], "om_idle_card");
    let card = v["payload"]["card"].to_string();
    assert!(
        card.contains("下次来消息时新建会话"),
        "reset=true 应落 IdleClosedReason::Reset 的 footer"
    );
    assert!(
        card.contains("会话空闲提醒"),
        "必须保留空闲布局,不能漂成 /session 面板"
    );
    assert!(card.contains("当前 ctx"));
    assert!(card.contains("42%"));
}

/// `/session` 面板卡点「退出」同此约定:原卡 PATCH 成面板终态并报 true,回执卡不再重复。
#[test]
fn mark_session_card_closed_reports_panel_card_carried_final_state() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: nimbus").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
    let carried = mark_session_card_closed(
        &fc,
        &agent,
        "group",
        "oc_panel",
        Some("om_panel_card"),
        &[],
        false,
        CardOrigin::Panel,
    );
    assert!(carried);
    let p = agent
        .state_dir()
        .join("threads")
        .join(chat_tag("group", "oc_panel"))
        .join("outbound.ndjson");
    let s = std::fs::read_to_string(&p).expect("应归档一条原地更新");
    let v: serde_json::Value = serde_json::from_str(s.lines().next().unwrap()).unwrap();
    assert_eq!(v["kind"], "card_update");
    assert_eq!(v["payload"]["message_id"], "om_panel_card");
}

/// 非卡片触发(`/session reset` 斜杠命令、`agents kill`):没有原卡可承载终态 → 必须报 false,
/// 让退出回执卡照常发,否则这次退出对用户彻底静默。
#[test]
fn mark_session_card_closed_without_card_message_id_is_not_carried() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: nimbus").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
    assert!(!mark_session_card_closed(
        &fc,
        &agent,
        "p2p",
        "oc_slash",
        None,
        &[],
        true,
        CardOrigin::Panel,
    ));
    assert!(
        !agent.state_dir().join("threads").exists(),
        "无卡片可改就什么都不该发"
    );
}

#[test]
fn peer_uid_is_self_accepts_same_process_pair() {
    // S2:同 uid(本进程对)应放行;跨 uid 在单测里造不出来,fail-closed 分支由「取不到 peer 即拒」兜底。
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(peer_uid_is_self(&a));
    assert!(peer_uid_is_self(&b));
}

#[test]
fn cleanup_inbound_spill_removes_only_expired_entries() {
    // PF2:inbound 附件目录与 quotes spill 按 mtime TTL 清扫;未过期保留,threads/ 不受影响。
    let tmp = std::env::temp_dir().join(format!("summon-inbspill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let att = tmp.join("inbound").join("p-1234").join("om_old");
    std::fs::create_dir_all(&att).unwrap();
    std::fs::write(att.join("image.png"), b"x").unwrap();
    let quotes = tmp.join("inbound").join("quotes");
    std::fs::create_dir_all(&quotes).unwrap();
    std::fs::write(quotes.join("om_q.md"), "长引用").unwrap();
    let threads = tmp.join("threads").join("p-1234");
    std::fs::create_dir_all(&threads).unwrap();
    std::fs::write(threads.join("events.jsonl"), "{}").unwrap();
    // ttl=1 天:刚建的都不过期 → 0 清除。
    assert_eq!(
        cleanup_inbound_spill(&tmp, Duration::from_secs(86400), SystemTime::now()),
        0
    );
    assert!(att.exists() && quotes.join("om_q.md").exists());
    // ttl=0:全部视为过期 → 附件目录与 quote 文件清掉,threads/ 原样。
    let n = cleanup_inbound_spill(&tmp, Duration::ZERO, SystemTime::now());
    assert_eq!(n, 2, "附件目录 + quote 文件各 1 项");
    assert!(!att.exists());
    assert!(!quotes.join("om_q.md").exists());
    assert!(
        threads.join("events.jsonl").exists(),
        "绝不碰 threads/(观测归档另有轮转)"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn quoted_merge_forward_expands_all_nested_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();

    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    // 只剩 token + 取消息两个请求:引用消息里的发送人名字**不再走 contact/v3/users**
    // (那个接口只覆盖 app 的通讯录数据权限范围,范围外的人恒 41050),改由名字缓存供给 ——
    // 缓存来源是入站时拉的群成员列表与 identity-cache 预热,下面 `prime_names` 模拟这一步。
    let server_thread = std::thread::spawn(move || {
        for _ in 0..2 {
            let request = server.recv().unwrap();
            let response = if request.url().contains("auth/v3/tenant_access_token") {
                r#"{"tenant_access_token":"token","expire":7200}"#.to_string()
            } else {
                // 走 mget:单条 `messages/{id}` 对复杂卡只回「请升级客户端」占位,agent 间
                // 引用会因此静默断链(2026-08-12 同身份实测对照)。id 随之从路径段改成查询参数。
                assert!(
                    request.url().contains("/im/v1/messages/mget"),
                    "取被引用消息必须走 mget，实得 {}",
                    request.url()
                );
                assert!(
                    request.url().contains("message_ids=om_merge"),
                    "应按 parent_id 拉取被引用消息，实得 {}",
                    request.url()
                );
                r#"{
                    "code": 0,
                    "data": {
                        "items": [
                            {
                                "msg_type": "merge_forward",
                                "body": {"content": "Merged and Forwarded Message"},
                                "sender": {"id": ""}
                            },
                            {
                                "msg_type": "text",
                                "body": {"content": "{\"text\":\"第一条原文\"}"},
                                "upper_message_id": "om_merge",
                                "sender": {"id": "ou_alice"}
                            },
                            {
                                "msg_type": "text",
                                "body": {"content": "{\"text\":\"第二条原文\"}"},
                                "upper_message_id": "om_merge",
                                "sender": {"id": "ou_bob"}
                            }
                        ]
                    }
                }"#
                .to_string()
            };
            request
                .respond(tiny_http::Response::from_string(response))
                .unwrap();
        }
    });

    let client = FeishuClient::with_test_base_connection_close(base);
    // 这两个人已经在名字缓存里(真实链路:他们发过言 → 入站时拉过群成员列表,或被 @ 过 →
    // identity-cache 预热)。缓存里没有的人会退成占位「用户」—— 这是有意的降级:
    // 引用/合并转发消息只带 open_id、不带来源群,拿不到群成员列表就无从解析。
    client.prime_names([
        ("ou_alice".to_string(), "Alice".to_string()),
        ("ou_bob".to_string(), "Bob".to_string()),
    ]);
    let note = fetch_quote_note(&agent, &client, "om_merge");
    server_thread.join().unwrap();

    assert!(
        note.contains("发送人: Alice\n第一条原文"),
        "第一条发送人或正文归属错误: {note}"
    );
    assert!(
        note.contains("发送人: Bob\n第二条原文"),
        "第二条发送人或正文归属错误: {note}"
    );
    assert!(
        note.contains("转发了 2 条消息"),
        "应保留合并转发语义: {note}"
    );
}

#[test]
fn quoted_long_merge_forward_spills_complete_nested_text() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();

    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let server_thread = std::thread::spawn(move || {
        for _ in 0..2 {
            let request = server.recv().unwrap();
            let response = if request.url().contains("auth/v3/tenant_access_token") {
                r#"{"tenant_access_token":"token","expire":7200}"#.to_string()
            } else {
                let long = "第一条很长的原文".repeat(100);
                serde_json::json!({
                    "code": 0,
                    "data": {
                        "items": [
                            {
                                "msg_type": "merge_forward",
                                "body": {"content": "Merged and Forwarded Message"},
                                "sender": {"id": ""}
                            },
                            {
                                "msg_type": "text",
                                "body": {"content": serde_json::json!({"text": long}).to_string()},
                                "upper_message_id": "om_merge_long"
                            },
                            {
                                "msg_type": "text",
                                "body": {"content": "{\"text\":\"第二条尾部原文\"}"},
                                "upper_message_id": "om_merge_long"
                            }
                        ]
                    }
                })
                .to_string()
            };
            request
                .respond(tiny_http::Response::from_string(response))
                .unwrap();
        }
    });

    let client = FeishuClient::with_test_base_connection_close(base);
    let note = fetch_quote_note(&agent, &client, "om_merge_long");
    server_thread.join().unwrap();

    let quote_path = agent.state_dir().join("inbound/quotes/om_merge_long.md");
    assert!(note.contains("完整原文已存到本地文件"));
    assert!(note.contains(&quote_path.to_string_lossy().into_owned()));
    let stored = std::fs::read_to_string(quote_path).unwrap();
    assert!(stored.contains("第一条很长的原文"));
    assert!(stored.contains("第二条尾部原文"), "spill 不能丢转发尾部");
}

#[test]
fn purge_agent_chat_state_removes_all_owned_paths_only() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let session = "soul-starlight-gdeadbeef";
    let legacy_session = "soul-starlight";
    let tag = "gdeadbeef";
    let paths = chat_disk_paths(&agent, session, tag);
    let legacy_paths = chat_disk_paths(&agent, legacy_session, tag);
    write_chat_meta(
        &agent,
        session,
        "oc_gone",
        "group",
        None,
        "owner",
        Some("ou_owner"),
    );
    write_chat_meta(
        &agent,
        legacy_session,
        "oc_gone",
        "group",
        None,
        "owner",
        Some("ou_owner"),
    );
    for path in paths[1..5].iter().chain(&legacy_paths[1..5]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "state").unwrap();
    }
    for path in paths[5..].iter().chain(&legacy_paths[5..]) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("payload"), "state").unwrap();
    }
    let unrelated = agent.dir.join("threads/gkeep/events.jsonl");
    std::fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
    std::fs::write(&unrelated, "{}").unwrap();

    let mut expected_sessions = vec![
        legacy_session.to_string(),
        session.to_string(),
        agent.tmux_name_for_chat("group", "oc_gone", false),
    ];
    expected_sessions.sort();
    expected_sessions.dedup();
    assert_eq!(
        gone_session_names(&agent, "oc_gone", session),
        expected_sessions,
        "群 Gone 必须规划拆除全部历史 session + 可纯推导的 canonical session"
    );

    let mut all_paths = paths.iter().chain(&legacy_paths).collect::<Vec<_>>();
    all_paths.sort();
    all_paths.dedup();
    assert_eq!(
        purge_agent_chat_state(&agent, "oc_gone", session, tag),
        all_paths.len()
    );
    assert!(all_paths.iter().all(|path| !path.exists()));
    assert!(unrelated.exists(), "只能删除目标 chat 拥有的路径");
    assert_eq!(
        purge_agent_chat_state(&agent, "oc_gone", session, tag),
        0,
        "重复清理必须幂等"
    );
}

#[test]
fn gone_session_names_include_canonical_even_without_meta() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let canonical = agent.tmux_name_for_chat("group", "oc_missing_meta", false);

    assert_eq!(
        gone_session_names(&agent, "oc_missing_meta", ""),
        vec![canonical],
        "write_chat_meta 是 best-effort，缺 meta 也不能漏掉可确定推导的真会话"
    );
}

// `RecordStore`/`InboundRecord` 是 debug-only 录制设施,release 下不存在。
#[cfg(debug_assertions)]
#[test]
fn inbound_record_appends_and_loads_from_cassette() {
    // RR2.2:开录制时,一条合成入站(消息 + 卡片)落 inbound.ndjson 且可按序反序列化回来。
    // 直接经 RecordStore + tempdir 验,绕开真 summon_home(record_inbound 的写盘 IO 在此被验证)。
    use crate::runtime::record_store::RecordStore;
    let tmp = tempfile::tempdir().unwrap();
    let store = RecordStore::new(tmp.path());

    let DispatchMsg::Inbound(inb) = DispatchMsg::inbound_for_test("oc_chat", "你好") else {
        unreachable!()
    };
    let msg_rec = InboundRecord::Message {
        app_id: "cli_app".to_string(),
        inbound: inb,
    };
    let card_rec = InboundRecord::Card {
        app_id: "cli_app".to_string(),
        card: CardActionMsg {
            chat_id: "oc_chat".to_string(),
            message_id: "om_card".to_string(),
            operator_open_id: "ou_op".to_string(),
            event_id: String::new(),
            value: serde_json::json!({"action": "reset"}),
            wal_id: None,
            wal_replayed: false,
        },
    };
    store
        .append_ndjson("scn", "inbound.ndjson", &msg_rec)
        .unwrap();
    store
        .append_ndjson("scn", "inbound.ndjson", &card_rec)
        .unwrap();

    let back: Vec<InboundRecord> = store.read_ndjson("scn", "inbound.ndjson").unwrap();
    assert_eq!(back.len(), 2, "两条入站按到达序落盘");
    match &back[0] {
        InboundRecord::Message { app_id, inbound } => {
            assert_eq!(app_id, "cli_app");
            assert_eq!(inbound.chat_id, "oc_chat");
            assert_eq!(inbound.text, "你好");
        }
        _ => panic!("第一条应为 Message"),
    }
    match &back[1] {
        InboundRecord::Card { card, .. } => assert_eq!(card.message_id, "om_card"),
        _ => panic!("第二条应为 Card"),
    }
}

// `record::scenario_from` 是 debug-only 录制开关解析,release 下不存在。
#[cfg(debug_assertions)]
#[test]
fn scenario_from_normalizes_env_then_flag_file() {
    // 纯函数版:off(未设/空)→ None,handle_conn tee 直接短路、零行为变化;非空 → 场景名;
    // env `1` → default;env 产出 None 时回退文件(文件不退化 `1`,只取非空原文)。
    // 不碰 environ —— 故并行套件里不会毒化别的 fork/execve 测试(T1 根因)。
    assert_eq!(scenario_from(None, None), None, "未设且无文件时不录制");
    assert_eq!(
        scenario_from(Some(""), None),
        None,
        "env 空值且无文件不录制"
    );
    assert_eq!(scenario_from(Some("  "), None), None, "env 纯空白不录制");
    assert_eq!(
        scenario_from(Some("1"), None).as_deref(),
        Some("default"),
        "env `1` 退化 default"
    );
    assert_eq!(
        scenario_from(Some(" myscn "), None).as_deref(),
        Some("myscn"),
        "env 非空即场景名(trim)"
    );
    // env 为 None/空 → 回退文件;文件非空原文(不把 `1` 退化)。
    assert_eq!(
        scenario_from(None, Some("from_file")).as_deref(),
        Some("from_file")
    );
    assert_eq!(
        scenario_from(Some(""), Some("from_file")).as_deref(),
        Some("from_file")
    );
    assert_eq!(
        scenario_from(None, Some("1")).as_deref(),
        Some("1"),
        "文件端不退化 `1`"
    );
    assert_eq!(scenario_from(None, Some("   ")), None, "文件纯空白不录制");
    // env 优先于文件。
    assert_eq!(
        scenario_from(Some("env_wins"), Some("file")).as_deref(),
        Some("env_wins")
    );
}

#[test]
fn owner_matches_by_open_id_or_union_id() {
    // owner 配 open_id:open_id 命中,union_id 不影响。
    assert!(owner_id_matches("ou_owner", "ou_owner", "on_owner"));
    assert!(owner_id_matches("ou_owner", "ou_owner", ""));
    // owner 配 union_id(onboard 现在的默认):open_id 是 per-app 的、对不上,
    // 但 union_id 命中即认 owner —— 这正是回归修复点。
    assert!(owner_id_matches("on_owner", "ou_thisapp", "on_owner"));
    // 都不命中 → 非 owner。
    assert!(!owner_id_matches("on_owner", "ou_thisapp", "on_other"));
    assert!(!owner_id_matches("ou_owner", "ou_someone", ""));
    // union_id 为空时不能拿空串去误配。
    assert!(!owner_id_matches("", "ou_x", ""));
}

#[test]
fn should_prompt_idle_only_when_stale_and_not_yet_prompted() {
    let ttl = Duration::from_secs(24 * 3600);
    let last = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let fresh = last + Duration::from_secs(3600);
    let stale = last + ttl + Duration::from_secs(1);
    assert!(!should_prompt_idle(last, fresh, ttl, false, false, false)); // 未到点
    assert!(should_prompt_idle(last, stale, ttl, false, false, false)); // 到点且未提示
    assert!(!should_prompt_idle(last, stale, ttl, true, false, false)); // 已提示过
                                                                        // 有活跃 turn:agent 在输出,不是「空闲」,到点也不提醒(owner 2026-08-17:连续输出 55min 仍发提醒)。
                                                                        // 变异自验:把 should_prompt_idle 里的 `!turn_in_flight` 门去掉,本断言必红。
    assert!(!should_prompt_idle(last, stale, ttl, false, true, false));
    // 有活跃 subagent/workflow:会话在工作,不是「空闲」,到点也不提醒(owner 2026-08-14)。
    // 变异自验:把 should_prompt_idle 里的 `!subagent_active` 门去掉,本断言必红。
    assert!(!should_prompt_idle(last, stale, ttl, false, false, true));
}

/// 自动退出是**全仓唯一由扫描线程发起的会话终止**,所以判据逐条钉:每条单独设为假都必须
/// 拦下来。合在一个 assert 里会让「哪条闸漏了」查不出来。
/// claude 的收尾从「最终回复覆盖过程」改成双卡:过程折叠 + 新卡放终态回复。
///
/// 钉的是**判据换了源**——旧判据看 `Thinking` 段,而 claude 的 thinking 正文被上游抹空
/// (真机 24/24 个块皆空),segments 里永远没有 Thinking 段,于是每轮都回落单卡覆盖。
/// 新判据看「除末尾那段外还有没有内容」,claude 的中间说明各自成 `Reply` 段,天然满足。
#[test]
fn summary_cards_split_on_leftover_segments_not_on_thinking() {
    use summon_core::segment::TurnSegment as S;
    let r = |t: &str| S::Reply(t.to_string());

    // claude 真实形状:中间说明各自成段 + 末尾终态回复。旧判据(无 Thinking 段)会拒,新判据放行。
    let claude = vec![r("先查一下这个"), r("查到了,继续"), r("最终结论如下")];
    assert!(
        should_split_summary_cards(&claude, None, true, false),
        "有中间段可折叠就该拆双卡——这正是 claude 此前被漏掉的形状"
    );

    // 只有一段终态回复且无轨迹:去掉末尾就空了,拆卡只会得到一张空过程卡。
    assert!(!should_split_summary_cards(
        &[r("就这一句")],
        None,
        true,
        false
    ));
    assert!(!should_split_summary_cards(&[], None, true, false));
    assert!(should_split_summary_cards(
        &[r("就这一句")],
        None,
        true,
        true
    ));

    // thinking 有正文的模型(codex)照旧走双卡,不因判据换源而回归。
    let codex = vec![S::Thinking("Planning the fix".into()), r("改好了")];
    assert!(should_split_summary_cards(&codex, None, true, false));

    // 总开关关掉时一律单卡,与判据无关。
    assert!(!should_split_summary_cards(&claude, None, false, false));
    assert!(!should_split_summary_cards(&codex, None, false, false));
}

/// 纯工具轮(claude 最常见的一类:thinking 正文为空、又没写中间说明)也必须拆双卡 ——
/// 否则终态回复原地覆盖 live 卡,飞书不推新消息,人收不到通知(owner 2026-08-13 实测)。
///
/// 判据源是**工具轨迹**,而轨迹只在「思考不可见」那一档启用,所以这条不会波及 codex/cursor。
#[test]
fn summary_cards_split_on_tool_trail_when_segments_are_bare() {
    use summon_core::segment::TurnSegment as S;
    let r = |t: &str| S::Reply(t.to_string());

    // claude 纯工具轮:每个空 thinking 块喂进 progress 留时长痕,工具 label 是 description。
    let mut redacted = summon_core::progress::TurnProgress::new();
    redacted.note_thinking("", 3); // 空正文 = 命中不可见档
    redacted.tool_started("查这个函数的定义");
    redacted.tool_finished(2);
    assert!(
        should_split_summary_cards(&[r("只有一段终态回复")], Some(&redacted), true, false),
        "段去尾后空,但轨迹有内容 → 折叠区装得下,该拆双卡"
    );

    // 有明文 thinking 的模型:progress 不启用(enabled=false),轨迹不算数,行为与从前逐字节一致。
    let mut plain = summon_core::progress::TurnProgress::new();
    plain.note_thinking("我在想怎么改", 3);
    plain.tool_started("cargo test");
    plain.tool_finished(9);
    assert!(
        !should_split_summary_cards(&[r("只有一段终态回复")], Some(&plain), true, false),
        "明文 thinking 档不该被轨迹带出双卡——过程已由段渲出来了"
    );

    // 轨迹也救不了总开关:kill-switch 优先。
    assert!(!should_split_summary_cards(
        &[r("只有一段终态回复")],
        Some(&redacted),
        false,
        false
    ));
}

/// 定时预告卡必须发在 **inbox WAL 取得 durable start 之后**,不能在 dispatcher 收到 fire 时发。
///
/// 2026-08-12 devbox 实测:一条 08-11 14:00 的 fire 卡在 WAL 里执行不掉,每次 daemon 重启都被
/// 「启动恢复」重注入,而预告卡当时发在 dispatcher 侧 —— 每重启一次就往群里发一张「⏰ 定时任务
/// 触发」,内容是昨天的任务、时刻却是今晚,紧接着日志就是「未取得新 start(AlreadyStarted),
/// 本轮未执行」:卡发了,任务没跑。
///
/// 用源码结构钉:① dispatcher 里不许再有预告卡的出站;② chat_thread 的发送点必须落在
/// `run_after_durable_start` 的闭包内(即在 `handle_task` 之前、闭包起点之后)。
/// 两处指令回执的 thread 锚点判据必须**逐字一致**。
///
/// chat 线程侧 `command_anchor_for(task)` 与 dispatcher 侧 `command_anchor(msg)` 回答的是同一个
/// 问题:「这条指令回执该挂进哪条 thread」。一旦漂移,同一类指令的回执会一半落在 thread 里、
/// 一半掉到群根部 —— 那比两边都不锚更让人困惑。
///
/// 判据本身也钉一下:**只在群里锚**(私聊没有 thread);锚点经真实消息 id 解析(恢复重放的
/// `message_id` 是本地幂等键,不能拿去 reply);**不看原消息在不在 thread 里** —— 指令回执要的
/// 就是「主干上的指令也开一条 thread」,那是与普通轮回复(`thread_anchor_for`)相反的取舍。
#[test]
fn command_anchor_criteria_match_between_dispatcher_and_chat_thread() {
    let chat_src = include_str!("chat_thread.rs");
    let disp_src = include_str!("dispatcher.rs");
    for (name, src, func, group_gate, real_id) in [
        (
            "chat_thread",
            chat_src,
            "fn command_anchor_for(",
            "is_group",
            "feishu_anchor()",
        ),
        (
            "dispatcher",
            disp_src,
            "fn command_anchor(",
            "is_group_chat(",
            "feishu_reply_id(",
        ),
    ] {
        let body = fn_body(src, func).unwrap_or_else(|| panic!("{name} 缺少锚点判据函数"));
        assert!(
            body.contains(group_gate),
            "{name}:只在群里锚,私聊没有 thread"
        );
        assert!(
            !body.contains("thread_id"),
            "{name}:指令回执不看原消息在不在 thread 里"
        );
        assert!(
            body.contains(real_id),
            "{name}:锚点要经真实消息 id 解析(恢复重放的 message_id 是本地幂等键,不能拿去 reply)"
        );
    }
    // dispatcher 取 id 的规矩与陈旧提示共用一份,同样要经真实消息 id、不看 thread。
    let body = fn_body(disp_src, "fn feishu_reply_id(").expect("dispatcher 缺少取回复 id 的函数");
    assert!(body.contains("anchor_message_id") && !body.contains("thread_id"));
    // 普通轮的回复仍是另一套:不在 thread 的消息绝不开新 thread。
    let body =
        fn_body(chat_src, "fn thread_anchor_for(").expect("chat_thread 缺少普通轮的锚点判据");
    assert!(
        body.contains("thread_id"),
        "普通轮回复必须先确认原消息在 thread 里"
    );
}

/// 从源码里截出一个顶层函数的正文:签名起、到第一个顶格 `}` 止。截得太长会把紧邻的下一个函数
/// 卷进来,让「不含某词」类断言恒红或恒绿。切点是 ASCII,不会切进多字节字符中间。
fn fn_body<'a>(src: &'a str, signature: &str) -> Option<&'a str> {
    let at = src.find(signature)?;
    let end = src[at..].find("\n}\n")?;
    Some(&src[at..at + end])
}

/// 终审阻塞 ② 的回归:**不可逆提交之后不许再传播失败**。
///
/// `persist_headless_recall_binding` 里,resume 记录一旦写成就是 durable commit,绑定已经真实
/// 生效。此后若把生命周期状态写失败当成整体失败上抛,用户会看到"提交绑定失败",而再点一次恢复
/// 又会被 `local_chat_occupied` 以"resume 记录已存在"拒掉 —— **界面报失败、绑定却已生效、原操作
/// 还无法重试**。这条路不像 recall/takeover 有 durable 状态机兜重试,只能就地降级、由下一条入站
/// 经 admission 自愈。
///
/// 用源码结构钉死:提交点之后的那次 `mark_open` 必须落在 `if let Err(...)` 的日志分支里,
/// 不得出现 `?` / `context(...)?` 这类上抛。
#[test]
fn headless_recall_must_not_propagate_failures_after_the_durable_commit() {
    let src = include_str!("bridge.rs");
    let commit = src
        .find("summon_i18n::text!(\"rcbf01bc04f\"")
        .expect("durable commit 锚点(resume 记录)");
    let tail = &src[commit..];
    let mark = tail
        .find("lifecycle::mark_open")
        .expect("提交后应写生命周期状态");
    // 取 mark_open 前后的一小段代码来判断它处在什么语法结构里。
    let window = &tail[mark.saturating_sub(80)..(mark + 200).min(tail.len())];
    assert!(
        window.contains("if let Err("),
        "提交后的 mark_open 必须走 `if let Err` 降级分支,不能上抛:\n{window}"
    );
    let stmt_end = tail[mark..]
        .find(';')
        .map(|i| mark + i)
        .unwrap_or(tail.len());
    assert!(
        !tail[mark..stmt_end].contains('?'),
        "durable commit 之后不得用 `?` 传播——那会造成不可重试的部分提交"
    );
}

#[test]
fn schedule_notice_is_sent_only_after_durable_start() {
    let dispatcher = include_str!("dispatcher.rs");
    assert!(
        !dispatcher.contains(
            "send_markdown_archived(&fc, &agent.state_dir(), &chat_type, &chat_id, &notice)"
        ),
        "dispatcher 不得直接发预告卡——它只知道收到了 fire,不知道这轮会不会执行"
    );
    assert!(
        dispatcher.contains("schedule_notice: Some(") || dispatcher.contains("schedule_notice"),
        "dispatcher 仍需把预告文案交给 task"
    );

    // needle 用 concat! 运行时拼:本文件也在 `turn_entry_is_always_wrapped_by_durable_start_barrier`
    // 的扫描范围内,源码里只要出现「执行入口带左括号」那个字面 token(哪怕在注释里)就会被它
    // 当成越界调用点 —— 它扫的是纯文本,不区分代码与注释。实测踩过两次:先是断言里,再是这段
    // 注释自己。
    let chat = include_str!("chat_thread.rs");
    let barrier = concat!("run_after_durable_start", "(");
    let entry = concat!("handle_task", "(");
    let start = chat.find(barrier).expect("durable start 入口");
    let send = chat
        .find("if let Some(notice) = task.schedule_notice.as_deref()")
        .expect("chat 线程必须承担预告卡发送");
    let handle = chat[start..]
        .find(entry)
        .map(|i| i + start)
        .expect("闭包内的执行入口");
    assert!(
        start < send && send < handle,
        "预告卡必须夹在 durable start 与 handle_task 之间(实得 start={start} send={send} handle={handle})"
    );
}

/// 引用取不到正文时**必须仍给出提示**,不能让整个引用块凭空消失。
///
/// 2026-08-12 线上回归:`fetch_message` 改走 mget 后,`quoted_plaintext_full`(要求 content
/// 是 JSON)解析失败返回空串;空既不是 self、也不命中占位判据,一路落到函数末尾的
/// `(true, true) => String::new()`,agent 连"用户引用过东西"都不知道 —— 比修复前的"给一句
/// 自己去取"更糟。
///
/// 用源码结构钉:空值必须和 self / 占位走同一条 hint 分支。
#[test]
fn empty_quote_body_still_yields_a_hint() {
    let src = include_str!("chat_thread.rs");
    let branch = src
        .find("if is_self || full.is_empty()")
        .expect("空正文必须与 self/占位共用 hint 分支——否则引用块会整个消失");
    let hint = src[branch..]
        .find("render_quote_hint")
        .expect("该分支必须产出 hint");
    let tail = src[branch..]
        .find("(true, true) => String::new()")
        .unwrap_or(usize::MAX);
    assert!(hint < tail, "hint 必须在「全空则返回空串」那条兜底之前生效");
}

/// 自动退出必须在**动手那一刻**再确认队列为空,并且只约束自动路径。
///
/// 2026-08-12 星光对抗 review 发现的竞态:七条判据跑在 dispatcher 线程的 idle 扫描,拆除在
/// chat 线程执行,中间隔一次 channel 投递。窗口内到达的用户消息排在 Clean 后面,被
/// `log_dropped_queue_on_teardown` 连队列一起丢掉且不重投(sidecar 已 ack 飞书,WAL 要等
/// daemon 冷启)。判据描述的是判定那一刻,目的却是「动手那一刻这条会话空着」。
///
/// 钉三件:重检只在 `idle_retired` 时做(用户点面板退出不受影响)、发现 Task 要**原样回投**
/// 而不是丢弃、且回投后跳过本次拆除。
#[test]
fn idle_retire_rechecks_queue_before_teardown() {
    let src = include_str!("chat_thread.rs");
    // 重检逻辑收口在 idle_teardown_confirmed:看队列 → 回投 → 跳过(return false)。
    let helper = src
        .find("fn idle_teardown_confirmed(")
        .expect("自动退出动手前重检必须收口在 idle_teardown_confirmed");
    let helper_end = src[helper..]
        .find("\n}\n")
        .map(|i| helper + i)
        .unwrap_or(src.len());
    let body = &src[helper..helper_end];
    let drain = body
        .find("drain_orphaned_tasks(rx)")
        .expect("重检要真的看队列");
    let requeue = body
        .find("self_chat_tx.send(ThreadMsg::Task(")
        .expect("发现排队消息必须原样回投,不能丢");
    let skip = body.find("return false;").expect("回投后必须跳过本次拆除");
    assert!(
        drain < requeue && requeue < skip,
        "顺序:先看队列 → 回投 → 跳过"
    );

    // Clean 臂:idle_retired 时调 helper,且排在会丢消息的收尾之前。
    let clean_arm = src.find("ctrl: Control::Clean").expect("Clean 臂");
    let clean_tail = &src[clean_arm..];
    let call = clean_tail
        .find("idle_teardown_confirmed(")
        .expect("Clean 臂自动退出必须过重检");
    let teardown = clean_tail
        .find("log_dropped_queue_on_teardown")
        .expect("Clean 分支的队列收尾");
    assert!(call < teardown, "重检要排在会丢消息的收尾之前");

    // 只约束自动路径:用户主动点退出时照旧立即拆(他明确要退)。
    assert!(
        !src[clean_arm..clean_arm + call].contains("drain_orphaned_tasks"),
        "重检不得跑在 idle_retired 判断之外——那会改变用户主动退出的语义"
    );

    // Reset 臂同样要过重检(定时尾轮重置走这条自动路径)。
    let reset_arm = src
        .find("ctrl: ctrl @ (Control::Reset | Control::Archive)")
        .expect("Reset 臂");
    assert!(
        src[reset_arm..].contains("idle_teardown_confirmed("),
        "Reset 臂自动退出(定时尾轮重置)同样要过重检"
    );
}

#[test]
fn should_retire_idle_session_requires_every_gate() {
    let retire = Duration::from_secs(24 * 3600);
    let last = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let stale = last + retire + Duration::from_secs(1);
    // 全部满足 = 唯一该退的情形。
    assert!(should_retire_idle_session(
        last,
        stale,
        Some(retire),
        true,
        true,
        true,
        false,
        false
    ));
    // ① 没开(默认):行为与改动前逐字节相同。
    assert!(!should_retire_idle_session(
        last, stale, None, true, true, true, false, false
    ));
    // ② 会话已不在:没什么可拆,发回执只会误导。
    assert!(!should_retire_idle_session(
        last,
        stale,
        Some(retire),
        false,
        true,
        true,
        false,
        false
    ));
    // ③ resume 关着:拆了接不回来,那是重置不是归档续聊。
    assert!(!should_retire_idle_session(
        last,
        stale,
        Some(retire),
        true,
        false,
        true,
        false,
        false
    ));
    // ④ 盘上没有 session id:同上,续不回来。
    assert!(!should_retire_idle_session(
        last,
        stale,
        Some(retire),
        true,
        true,
        false,
        false,
        false
    ));
    // ⑤ 有活跃 turn 记录:Clean 会被 mid-turn 抢占,拆了就打断正在跑的一轮。
    assert!(!should_retire_idle_session(
        last,
        stale,
        Some(retire),
        true,
        true,
        true,
        true,
        false
    ));
    // ⑥ 没到点。
    let fresh = last + Duration::from_secs(3600);
    assert!(!should_retire_idle_session(
        last,
        fresh,
        Some(retire),
        true,
        true,
        true,
        false,
        false
    ));
    // ⑦ 时钟回拨(now < last):按未到点处理,不因为算不出时长就动手。
    let rewound = last - Duration::from_secs(60);
    assert!(!should_retire_idle_session(
        last,
        rewound,
        Some(retire),
        true,
        true,
        true,
        false,
        false
    ));
    // ⑧ 有活跃 subagent/workflow(owner 2026-08-14):主轮结束不等于会话空着 —— 拆会话
    // 会连 CLI 进程一起收掉,后台 subagent/workflow 的在途工作全部丢失。注意本例的空闲
    // 时长已远超窗口(last 未被推进):挡住它的**只有**这道门。
    // 变异自验:把 should_retire_idle_session 里的 `!subagent_active` 去掉,本断言必红。
    assert!(!should_retire_idle_session(
        last,
        stale,
        Some(retire),
        true,
        true,
        true,
        false,
        true
    ));
}

/// 空闲时钟的有效起点:subagent 的最后活动推进「最后活跃」,使 subagent 结束后的空闲期
/// 从它的最后活动起算(而不是主轮/入站起算);历史 subagent 的旧时刻被 max 淹没;
/// 无观测(None)= 时钟不动。变异自验:把 max 改成任一侧直取,两个方向的断言必有一红。
#[test]
fn effective_idle_anchor_advances_with_subagent_activity() {
    let inbound = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let inbound_utc = chrono::DateTime::<chrono::Utc>::from(inbound);

    // subagent 活动晚于入站 → 空闲期从 subagent 的最后活动起算。
    let later = inbound_utc + chrono::Duration::hours(2);
    assert_eq!(
        effective_idle_anchor(inbound, Some(later)),
        SystemTime::from(later)
    );
    // 历史 subagent(早于入站)不把时钟拨回过去。
    let earlier = inbound_utc - chrono::Duration::hours(5);
    assert_eq!(effective_idle_anchor(inbound, Some(earlier)), inbound);
    // 无观测:现行为。
    assert_eq!(effective_idle_anchor(inbound, None), inbound);
}

/// 配 0 的语义是「立刻退出所有会话」——那多半是手误,不替用户执行这种解释。
#[test]
fn idle_retire_treats_zero_as_disabled() {
    let mut m = summon_core::config::builtin_messaging();
    assert_eq!(m.idle_retire(), None, "内置默认必须是关");
    m.idle_retire_secs = Some(0);
    assert_eq!(m.idle_retire(), None);
    m.idle_retire_secs = Some(86400);
    assert_eq!(m.idle_retire(), Some(Duration::from_secs(86400)));
}

/// timer_idle_retire(定时任务尾轮静默回收)的判据真值表。
/// 组合三个纯件:`idle_retire_window`(选窗口)+ `should_retire_idle_session`(要不要退,
/// 与普通 retire 同一函数)+ `timer_quiet_owns_idle`(要不要压 55min 提醒卡)。
#[test]
fn timer_idle_retire_truth_table() {
    let timer = Duration::from_secs(3300);
    let day = Duration::from_secs(86400);
    let last = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let stale = last + timer + Duration::from_secs(1);

    // ① 定时尾轮 + 超时 + 门槛全真 → 产 retire 意图(窗口取 timer 的 55min)且不发提醒卡。
    let w = idle_retire_window(true, Some(timer), None);
    assert_eq!(w, Some(timer), "定时尾轮必须用 timer 窗口");
    assert!(should_retire_idle_session(
        last, stale, w, true, true, true, false, false
    ));
    assert!(
        timer_quiet_owns_idle(true, Some(timer), true, true, true),
        "结构门槛全真时静默回收托管空闲处理,不出提醒卡"
    );

    // ② 人类尾轮 → 原行为:窗口回落普通 idle_retire(默认关 = 不退),提醒卡照发。
    assert_eq!(idle_retire_window(false, Some(timer), None), None);
    assert!(!should_retire_idle_session(
        last, stale, None, true, true, true, false, false
    ));
    assert!(!timer_quiet_owns_idle(false, Some(timer), true, true, true));
    // 人类尾轮 + 开了普通 retire → 窗口仍是普通的(timer 机制不掺和人类会话)。
    assert_eq!(idle_retire_window(false, Some(timer), Some(day)), Some(day));

    // ③ 门槛破缺 → 不退,且不压卡(回退提醒卡),绝不留「既无卡又不回收」的悬空态。
    // resume 记录缺失:
    assert!(!should_retire_idle_session(
        last,
        stale,
        Some(timer),
        true,
        true,
        false,
        false,
        false
    ));
    assert!(!timer_quiet_owns_idle(true, Some(timer), true, true, false));
    // 会话不在 / resume 关着同理:
    assert!(!timer_quiet_owns_idle(true, Some(timer), false, true, true));
    assert!(!timer_quiet_owns_idle(true, Some(timer), true, false, true));

    // ④ 显式关闭(timer_idle_retire_secs: 0 → None)→ 定时尾轮回到旧规则:
    //    窗口回落普通 retire、提醒卡照发。
    assert_eq!(idle_retire_window(true, None, Some(day)), Some(day));
    assert_eq!(idle_retire_window(true, None, None), None);
    assert!(!timer_quiet_owns_idle(true, None, true, true, true));

    // ⑤ 没到点:不退;但静默回收仍托管着(先压卡等到点,而不是先发卡再回收)。
    let fresh = last + Duration::from_secs(60);
    assert!(!should_retire_idle_session(
        last,
        fresh,
        Some(timer),
        true,
        true,
        true,
        false,
        false
    ));

    // ⑥ 有活跃 turn(长定时轮还在跑):本轮不退——暂态,轮结束后下轮扫描再判;卡也不发。
    assert!(!should_retire_idle_session(
        last,
        stale,
        Some(timer),
        true,
        true,
        true,
        true,
        false
    ));
    // ⑥' 有活跃 subagent/workflow(定时轮结束、后台 Agent 还在跑):同为暂态,本轮不退;
    // 静默托管照旧(卡也不发)——会话安静地等 subagent 收尾,再从其最后活动起算空闲期。
    assert!(!should_retire_idle_session(
        last,
        stale,
        Some(timer),
        true,
        true,
        true,
        false,
        true
    ));
    assert!(timer_quiet_owns_idle(true, Some(timer), true, true, true));
}

/// 默认值是 owner 2026-08-13 显式授权的「开,55min」;关闭走 0(同 idle_retire 的 0 当关)。
#[test]
fn timer_idle_retire_default_on_at_55min_and_zero_disables() {
    let mut m = summon_core::config::builtin_messaging();
    assert_eq!(
        m.timer_idle_retire(),
        Some(Duration::from_secs(3300)),
        "内置默认必须是开、55min(owner 2026-08-13 授权)"
    );
    m.timer_idle_retire_secs = 0;
    assert_eq!(m.timer_idle_retire(), None, "0 = 显式关闭,回到旧行为");
}

#[test]
fn emoji_file_doc_lists_reaction_keys_without_prompt_wrapper() {
    let meanings = std::collections::HashMap::new();
    let doc = emoji_file_doc(
        &["OK".to_string(), "THINKING".to_string()],
        &meanings,
        ["OnIt", "DONE"],
    );

    assert!(doc.contains("- OK"));
    assert!(doc.contains("- THINKING"));
}

#[test]
fn emoji_file_doc_includes_meaning_when_available() {
    let mut meanings = std::collections::HashMap::new();
    meanings.insert("OK".to_string(), "好的 / 同意".to_string());
    meanings.insert("THUMBSUP".to_string(), "点赞 / 夸奖".to_string());
    let doc = emoji_file_doc(
        &["OK".to_string(), "THUMBSUP".to_string(), "DONE".to_string()],
        &meanings,
        ["OnIt", "DONE"],
    );

    assert!(doc.contains("- OK（好的 / 同意）"));
    assert!(doc.contains("- THUMBSUP（点赞 / 夸奖）"));
    assert!(!doc.contains("DONE"));
}

#[test]
fn emoji_file_path_is_scoped_by_agent_and_content() {
    let tmp = tempfile::tempdir().unwrap();
    let mut first: Agent = serde_yaml::from_str("name: first\nruntime:\n  cli: claude").unwrap();
    first.dir = tmp.path().join("agents/first");
    let mut second: Agent = serde_yaml::from_str("name: second\nruntime:\n  cli: claude").unwrap();
    second.dir = tmp.path().join("agents/second");

    assert_ne!(
        emoji_file_path(&first, "same"),
        emoji_file_path(&second, "same")
    );
    assert_ne!(
        emoji_file_path(&first, "old"),
        emoji_file_path(&first, "new")
    );
    assert!(emoji_file_path(&first, "same").starts_with(first.state_dir()));
    assert!(emoji_file_path(&second, "same").starts_with(second.state_dir()));
}

#[test]
fn emoji_file_keeps_old_config_generation_immutable() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: test\nruntime:\n  cli: claude").unwrap();
    agent.dir = tmp.path().join("agents/test");
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let reactions = vec!["OK".into(), "THUMBSUP".into(), "OnIt".into(), "DONE".into()];
    let meanings = std::collections::HashMap::new();

    let old = write_emoji_file(&agent, &reactions, &meanings, ["OnIt", "DONE"]).unwrap();
    let old_doc = std::fs::read_to_string(&old).unwrap();
    let new = write_emoji_file(&agent, &reactions, &meanings, ["OK", "THUMBSUP"]).unwrap();

    assert_ne!(old, new, "不同配置代际必须落到不同路径");
    assert_eq!(std::fs::read_to_string(old).unwrap(), old_doc);
    assert!(!old_doc.contains("OnIt"));
    assert!(!old_doc.contains("DONE"));
}

/// 被闸拒绝 ≠ 随机注入故障:前者一个键都没发出去、用户能自己解决(退掉那个面板再重发),
/// 后者才是「可 attach 查看」。两条回执必须分得开,否则真正该做的动作被藏进含糊里。
#[test]
fn refused_injection_receipt_tells_the_user_what_actually_blocked_it() {
    use summon_core::pane_input::{InjectionRefused, ModalKind, PaneState};

    let refused = switch_inject_failure(
        &anyhow::Error::new(InjectionRefused::new(PaneState::Modal(ModalKind::Panel)))
            .context("注入 /model 失败"),
    );
    assert!(refused.contains("全屏面板"), "{refused}");
    assert!(refused.contains("本条没有执行"), "{refused}");
    assert!(
        !refused.contains("可 attach 查看"),
        "别退回通用文案: {refused}"
    );

    let generic = switch_inject_failure(&anyhow::anyhow!("tmux send-keys 返回非零"));
    assert!(generic.contains("可 attach 查看"), "{generic}");
}

/// 轮内抓屏的让位判据:主 agent 的请求挂着时一切中介卡都让;只有子 agent 的请求挂着时,屏上
/// 另有的菜单照发,只让状态提示卡与降级卡;没有请求挂着时什么都不让。
#[test]
fn in_turn_cards_yield_to_claude_hook_by_who_is_asking() {
    use crate::runtime::claude_hook::ScreenClaim;
    use summon_core::interactive::{Choice, InteractiveState};
    let menu = InteractiveState {
        title: "Bash command".into(),
        options: vec![
            Choice {
                index: 1,
                label: "Yes".into(),
            },
            Choice {
                index: 2,
                label: "No".into(),
            },
        ],
        raw: String::new(),
    };
    let hint = InteractiveState {
        title: "Claude 正在等待回答".into(),
        options: Vec::new(),
        raw: String::new(),
    };
    let main = ScreenClaim {
        main: true,
        any: true,
    };
    let sub = ScreenClaim {
        main: false,
        any: true,
    };
    assert!(yields_to_claude_hook(main, &menu) && yields_to_claude_hook(main, &hint));
    assert!(!yields_to_claude_hook(sub, &menu), "屏上另有的菜单照发");
    assert!(yields_to_claude_hook(sub, &hint));
    assert!(!yields_to_claude_hook(ScreenClaim::default(), &menu));
    assert!(!yields_to_claude_hook(ScreenClaim::default(), &hint));

    // 闸门落幕后的第一拍再让一次(对话框可能还没被收掉),第二拍才回到正常发卡。
    let mut state = HookYield::default();
    assert!(
        !state.decide(ScreenClaim::default(), &menu),
        "从没让过位就照常发"
    );
    assert!(state.decide(main, &menu));
    assert!(state.decide(main, &menu));
    assert!(
        state.decide(ScreenClaim::default(), &menu),
        "让位刚结束,这一拍再等一等"
    );
    assert!(
        !state.decide(ScreenClaim::default(), &menu),
        "对话框真还在,下一拍发卡"
    );
}

/// 主 agent 的授权请求经 hook 挂着时,屏上的对话框就是它、卡已经发过:轮外守望整拍让位;闸门落幕
/// 后对话框若还在(hook 没答成),守望照常连续两拍认定后转发。子 agent 的请求不占屏。
#[test]
fn out_of_turn_menus_yield_to_a_pending_main_agent_hook_request() {
    use std::time::{Duration, Instant};
    use summon_core::interactive::MenuLedger;
    let chat = "oc_out_of_turn_hook";
    let idle = "────────── summon:soul-x ──\n❯\u{a0}\n──────────────────────────".to_string();
    let dialog = "──────────────────────────────\n Bash command\n\n   rm -f a\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel".to_string();
    let t0 = Instant::now();
    let beat = |n: u64| t0 + Duration::from_secs(5 * n);
    let tick = |menus: &mut OutOfTurnMenus, ledger: &MenuLedger, at: Instant| {
        let mut cards = 0;
        menus.on_idle_tick(
            at,
            || Some(dialog.clone()),
            ledger,
            "tester",
            chat,
            |_| {
                cards += 1;
                Ok(())
            },
        );
        cards
    };
    let ledger = MenuLedger::default();
    let mut menus = OutOfTurnMenus::start(Some(idle), Some(true), &ledger, t0);
    crate::runtime::claude_hook::claim_screen("gate-main", chat, true);
    for n in 1..=3 {
        assert_eq!(
            tick(&mut menus, &ledger, beat(n)),
            0,
            "第 {n} 拍:授权卡已发,不再抓屏转发"
        );
    }
    crate::runtime::claude_hook::release_screen("gate-main");
    crate::runtime::claude_hook::claim_screen("gate-sub", chat, false);
    assert_eq!(tick(&mut menus, &ledger, beat(4)), 0);
    assert_eq!(
        tick(&mut menus, &ledger, beat(5)),
        1,
        "子 agent 的请求不占屏:屏上另有的对话框照常转发"
    );
    crate::runtime::claude_hook::release_screen("gate-sub");
}

/// 轮外守望在 chat 线程里的节拍:起步那一拍屏上已有的菜单不发;守望期间弹出的菜单连续两拍在屏上
/// 发一张;回到输入框当拍清账;没到节拍不抓屏;抓屏失败、空屏不算观测;别的路径发过的不发;收到
/// 消息后的第一个空闲拍立刻抓屏。
#[test]
fn out_of_turn_menus_follow_the_thread_rhythm() {
    use std::time::{Duration, Instant};
    use summon_core::interactive::MenuLedger;
    let idle = "────────── summon:soul-x ──\n❯\u{a0}\n──────────────────────────".to_string();
    let dialog = |command: &str| {
        format!("──────────────────────────────\n Bash command\n\n   {command}\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel")
    };
    let t0 = Instant::now();
    let beat = |n: u64| t0 + Duration::from_secs(5 * n);
    let tick =
        |menus: &mut OutOfTurnMenus, ledger: &MenuLedger, at: Instant, pane: Option<String>| {
            let mut cards = Vec::new();
            menus.on_idle_tick(
                at,
                || pane,
                ledger,
                "tester",
                "oc_x",
                |card| {
                    cards.push(card.to_string());
                    Ok(())
                },
            );
            cards
        };

    let ledger = MenuLedger::default();
    let mut menus = OutOfTurnMenus::start(Some(idle.clone()), Some(true), &ledger, t0);
    assert!(tick(&mut menus, &ledger, beat(1), Some(dialog("rm -f a"))).is_empty());
    let cards = tick(&mut menus, &ledger, beat(2), Some(dialog("rm -f a")));
    assert_eq!(cards.len(), 1, "守望期间弹出、连续两拍在屏上就发");
    assert!(
        cards[0].contains("rm -f a") && cards[0].contains("tui_choice"),
        "{}",
        cards[0]
    );
    assert!(
        tick(&mut menus, &ledger, beat(3), Some(dialog("rm -f a"))).is_empty(),
        "发过不再发"
    );
    assert!(tick(&mut menus, &ledger, beat(4), Some(idle.clone())).is_empty());
    assert!(tick(&mut menus, &ledger, beat(5), Some(dialog("rm -f a"))).is_empty());
    assert_eq!(
        tick(&mut menus, &ledger, beat(6), Some(dialog("rm -f a"))).len(),
        1,
        "输入框回来之后同一条命令再申请是新问题"
    );

    let ledger = MenuLedger::default();
    let mut menus = OutOfTurnMenus::start(Some(idle.clone()), Some(true), &ledger, t0);
    assert!(tick(&mut menus, &ledger, beat(1), Some(dialog("rm -f b"))).is_empty());
    assert!(
        tick(
            &mut menus,
            &ledger,
            beat(1) + Duration::from_secs(1),
            Some(dialog("rm -f b"))
        )
        .is_empty(),
        "没到节拍不抓屏"
    );
    assert!(tick(&mut menus, &ledger, beat(2), None).is_empty());
    assert!(tick(&mut menus, &ledger, beat(3), Some(String::new())).is_empty());
    assert!(tick(&mut menus, &ledger, beat(4), Some("  \n".into())).is_empty());
    assert_eq!(
        tick(&mut menus, &ledger, beat(5), Some(dialog("rm -f b"))).len(),
        1,
        "抓屏失败、空屏不算观测,候选还在"
    );

    let ledger = MenuLedger::default();
    let mut menus = OutOfTurnMenus::start(Some(dialog("rm -f c")), Some(true), &ledger, t0);
    for n in 1..=4 {
        assert!(
            tick(&mut menus, &ledger, beat(n), Some(dialog("rm -f c"))).is_empty(),
            "起步就在屏上的菜单不发"
        );
    }

    let ledger = MenuLedger::default();
    let mut menus = OutOfTurnMenus::start(Some(idle.clone()), Some(true), &ledger, t0);
    ledger.record(&summon_core::interactive::awaiting_choice(&dialog("rm -f d")).unwrap());
    for n in 1..=4 {
        assert!(
            tick(&mut menus, &ledger, beat(n), Some(dialog("rm -f d"))).is_empty(),
            "别的路径发过的不发"
        );
    }

    let ledger = MenuLedger::default();
    let mut menus = OutOfTurnMenus::start(Some(idle.clone()), Some(true), &ledger, t0);
    let early = t0 + Duration::from_secs(1);
    let probes = std::cell::Cell::new(0);
    let capture = || {
        probes.set(probes.get() + 1);
        Some(idle.clone())
    };
    menus.on_idle_tick(early, capture, &ledger, "tester", "oc_x", |_| Ok(()));
    assert_eq!(probes.get(), 0, "没到节拍不抓屏");
    menus.note_message(early);
    menus.on_idle_tick(early, capture, &ledger, "tester", "oc_x", |_| Ok(()));
    assert_eq!(probes.get(), 1, "收到消息后的第一个空闲拍立刻抓屏");

    for pane in [None, Some(String::new())] {
        let ledger = MenuLedger::default();
        let mut menus = OutOfTurnMenus::start(pane, Some(false), &ledger, t0);
        assert!(tick(&mut menus, &ledger, beat(1), Some(dialog("rm -f e"))).is_empty());
        assert_eq!(
            tick(&mut menus, &ledger, beat(2), Some(dialog("rm -f e"))).len(),
            1,
            "会话还没起就是屏上没有菜单:冷启线程的第一张轮外菜单要发"
        );
    }

    for session_exists in [Some(true), None] {
        let ledger = MenuLedger::default();
        let mut menus = OutOfTurnMenus::start(None, session_exists, &ledger, t0);
        for n in 1..=3 {
            assert!(
                tick(&mut menus, &ledger, beat(n), Some(dialog("rm -f f"))).is_empty(),
                "会话在或探测失败、起步抓不到屏:第一拍可用的抓屏当起步那一拍"
            );
        }
    }

    let ledger = MenuLedger::default();
    let mut menus = OutOfTurnMenus::start(Some(idle.clone()), Some(true), &ledger, t0);
    assert!(tick(&mut menus, &ledger, beat(1), Some(dialog("rm -f g"))).is_empty());
    assert_eq!(
        tick(&mut menus, &ledger, beat(2), Some(dialog("rm -f g"))).len(),
        1
    );
    ledger.note_answered(
        summon_core::interactive::awaiting_choice(&dialog("rm -f g"))
            .unwrap()
            .signature(),
    );
    assert!(tick(&mut menus, &ledger, beat(3), Some(dialog("rm -f g"))).is_empty());
    assert_eq!(
        tick(&mut menus, &ledger, beat(4), Some(dialog("rm -f g"))).len(),
        1,
        "卡片上答过之后,同一条命令紧接着再申请是新问题"
    );
}

/// 发菜单卡的深层接线没法直接单测,静态钉住(比对时去掉全部空白,不受排版影响):
/// - 轮内交互回调记账后才交给尾随循环;轮内与轮间两处注入被拒回执都从整屏重抓;
/// - 切 effort 的确认卡走记账的 `announce_menu`;会话账本用 dispatcher 手里同一份;
/// - 线程起跑先起守望(拿整屏抓屏和会话在不在,赶在任何一轮之前),收到消息、空闲拍都喂它;
/// - 卡片单选注入成功之后才记「答过」。
#[test]
fn menu_card_paths_are_wired_to_the_session_ledger() {
    fn body<'a>(code: &'a str, head: &str) -> &'a str {
        let code = code
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap_or_default();
        let start = code
            .find(head)
            .unwrap_or_else(|| panic!("{head} 不见了(改名了?同步更新本测试)"));
        let end = code[start..]
            .find("\n}\n")
            .map_or(code.len(), |i| start + i);
        &code[start..end]
    }
    fn squash(code: &str) -> String {
        code.split_whitespace().collect()
    }
    let chat = include_str!("chat_thread.rs");

    // 轮内中介回调让位时回 Yielded:回 Settled 会让捕获层记下签名,hook 没答成时这张菜单再也发不出去。
    let consume = squash(body(chat, "pub(crate) fn card_stream_consume_with_cot("));
    assert!(
        consume.contains(
            "ifhook_yield.decide(crate::runtime::claude_hook::screen_claim(chat.chat_id),st){projector.borrow_mut().note_question_pending();returnMediationCard::Yielded;}"
        ),
        "轮内让位必须回 Yielded"
    );

    let turn = squash(body(chat, "fn consume_transcript_turn("));
    let wrapped = turn
        .find("record_delivered_menus(on_int,&sess.menu_ledger)")
        .expect("轮内交互回调必须经记账包装");
    let run_turn = turn.find(".run_turn(").expect("run_turn 调用不见了");
    assert!(wrapped < run_turn, "记账包装要在交给尾随循环之前建好");
    assert!(
        turn[run_turn..].contains("&muton_int_recorded"),
        "尾随循环拿到的必须是记账包装后的回调"
    );
    assert!(
        turn.contains(
            "send_refused_menu_card(&e,&name,&sess.menu_ledger,||sess.tmux.capture().ok(),"
        ),
        "轮内注入被拒回执要从整屏重抓"
    );

    let refused_to = squash(body(chat, "fn send_refused_menu_card_to("));
    assert!(
        refused_to.contains("&sess.menu_ledger,||sess.tmux.capture().ok(),"),
        "轮间注入被拒回执要从整屏重抓"
    );

    let thread = squash(body(chat, "pub(super) fn chat_thread_run("));
    assert!(
        thread.contains("announce_menu(&st,&sess.menu_ledger"),
        "切 effort 的确认卡必须记账"
    );
    let session = squash(body(chat, "fn chat_session<'a>("));
    assert!(
        session.contains(".with_menu_ledger(menu_ledger.clone())")
            && thread.contains("&mirror_watermark,menu_ledger,);"),
        "会话账本要用 dispatcher 手里同一份(起步与就地换 agent 都经 chat_session)"
    );
    let start = thread
        .find("OutOfTurnMenus::start(sess.tmux.capture().ok(),crate::session::tmux::has_session_strict(&sess.tmux.session).ok(),")
        .expect("线程起跑要拿整屏抓屏与会话在不在起轮外守望");
    let resume = thread
        .find("resume_pending_turn(")
        .expect("resume_pending_turn 不见了");
    assert!(start < resume, "守望起步那一拍要赶在任何一轮之前");
    assert!(thread.contains(".note_message("), "收到消息要通知守望");
    assert!(thread.contains(".on_idle_tick("), "空闲拍要喂守望");

    let spawn = squash(body(
        include_str!("dispatcher.rs"),
        "pub(super) fn spawn_chat_thread(",
    ));
    assert!(
        spawn.contains("let(menu_ledger,thread_menu_ledger)=chat_menu_ledgers();")
            && spawn.contains("menu_ledger:Some(menu_ledger)")
            && spawn.contains("thread_menu_ledger,"),
        "dispatcher 句柄与 chat 线程要共用同一份账本"
    );

    let inject = squash(body(include_str!("cards.rs"), "fn handle_tui_inject("));
    let injected = inject.find("ifletErr(e)=res{").expect("注入结果判定不见了");
    let answered = inject
        .find("answered_menu_signature(action,&card.value)")
        .expect("卡片单选注入后要在账本上记「答过」");
    assert!(
        inject[answered..].contains(".note_answered("),
        "要真的记进账本"
    );
    assert!(answered > injected, "注入成功之后才记「答过」");
}

/// dispatcher 句柄与 chat 线程拿到的是同一个账本:点击侧记的「答过」,守望看得到。
#[test]
fn chat_menu_ledgers_share_one_ledger() {
    let (handle, thread) = super::dispatcher::chat_menu_ledgers();
    assert!(std::sync::Arc::ptr_eq(&handle, &thread));
}

/// 轮内交互回调的记账:卡真的到了用户手里才记;没送达、降级卡都不记。
#[test]
fn in_turn_menu_cards_are_recorded_only_when_delivered() {
    use crate::capture::MediationCard;
    use summon_core::interactive::{Choice, InteractiveState, MenuLedger};
    let menu = InteractiveState {
        title: "Do you want to proceed?".into(),
        options: vec![
            Choice {
                index: 1,
                label: "Yes".into(),
            },
            Choice {
                index: 2,
                label: "No".into(),
            },
        ],
        raw: String::new(),
    };
    let degrade = InteractiveState {
        title: String::new(),
        options: Vec::new(),
        raw: "screen".into(),
    };
    let ledger = MenuLedger::default();

    let mut undelivered = |_: &InteractiveState| MediationCard::Undelivered;
    let mut on_interactive = record_delivered_menus(&mut undelivered, &ledger);
    assert_eq!(on_interactive(&menu), MediationCard::Undelivered);
    assert_eq!(ledger.recorded(), None, "没送达不记");

    let mut settled = |_: &InteractiveState| MediationCard::Settled;
    let mut on_interactive = record_delivered_menus(&mut settled, &ledger);
    assert_eq!(on_interactive(&degrade), MediationCard::Settled);
    assert_eq!(ledger.recorded(), None, "降级卡没有菜单可记");
    assert_eq!(on_interactive(&menu), MediationCard::Settled);
    assert_eq!(ledger.recorded(), Some(menu.signature()));
}

/// 菜单拒因的回执要带一张能点的卡:菜单只要一次按键就能解决,只发一句「去 attach」等于把
/// 唯一的出路留在用户够不着的地方。其余拒因不发卡 —— 面板/草稿没有可镜像的问句。
#[test]
fn a_menu_refusal_receipt_carries_a_clickable_card() {
    use summon_core::pane_input::{InjectionRefused, ModalKind, PaneState};

    let menu = "Change effort level?\n❯ 1. Yes, switch to xhigh\n  2. No";
    let refused = anyhow::Error::new(InjectionRefused::on_pane(
        summon_core::pane_input::classify_pane(menu),
        menu,
    ))
    .context("注入本条消息失败");
    let sent = std::cell::RefCell::new(Vec::<String>::new());
    let ledger = summon_core::interactive::MenuLedger::default();
    send_refused_menu_card(
        &refused,
        "tester",
        &ledger,
        || Some(menu.to_string()),
        |card| {
            sent.borrow_mut().push(card.to_string());
            Ok(())
        },
    );
    let cards = sent.borrow();
    let card = cards.first().expect("菜单拒因必须补一张交互卡");
    // 与轮内 on_interactive 同款:同一渲染、同一套回调(tui_choice),点击侧才认得。
    assert_eq!(
        card,
        &summon_core::feishu::card_interactive_select(
            "Change effort level?",
            &[
                (1, "Yes, switch to xhigh".to_string()),
                (2, "No".to_string())
            ],
        )
    );
    assert!(card.contains("tui_choice"), "{card}");
    assert_eq!(
        ledger.recorded(),
        summon_core::interactive::parse_interactive(menu).map(|st| st.signature()),
        "发出去的菜单要记账,轮外守望才不会重发"
    );
    // 卡上的菜单从整屏重抓,重抓得到就以它为准。
    let recaptured = "──────────────────────────────\n Bash command\n\n   rm -f $LOGS/a.bin\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel";
    let mut recaptured_cards = Vec::new();
    send_refused_menu_card(
        &refused,
        "tester",
        &summon_core::interactive::MenuLedger::default(),
        || Some(recaptured.to_string()),
        |card| {
            recaptured_cards.push(card.to_string());
            Ok(())
        },
    );
    assert!(
        recaptured_cards
            .first()
            .is_some_and(|card| card.contains("rm -f $LOGS/a.bin")),
        "{recaptured_cards:?}"
    );
    // 整屏重抓不到菜单就不发卡、不记账。
    let unrecaptured = summon_core::interactive::MenuLedger::default();
    let mut unrecaptured_cards = 0;
    send_refused_menu_card(
        &refused,
        "tester",
        &unrecaptured,
        || None,
        |_| {
            unrecaptured_cards += 1;
            Ok(())
        },
    );
    assert_eq!(unrecaptured_cards, 0);
    assert_eq!(unrecaptured.recorded(), None);

    for other in [
        anyhow::Error::new(InjectionRefused::new(PaneState::Modal(ModalKind::Panel))),
        anyhow::Error::new(InjectionRefused::new(PaneState::Draft)),
        anyhow::anyhow!("tmux send-keys 返回非零"),
    ] {
        let mut sent = 0;
        send_refused_menu_card(
            &other,
            "tester",
            &summon_core::interactive::MenuLedger::default(),
            || None,
            |_| {
                sent += 1;
                Ok(())
            },
        );
        assert_eq!(sent, 0, "非菜单拒因不该发卡: {other:#}");
    }
}

/// AskUserQuestion 形态的拒因卡上不许出现「Type something.」「Chat about this」:这张卡只有
/// 选项按钮,点下去 TUI 进自由文本态,飞书侧既打不了字也退不出来 —— 比不发卡更糟。
#[test]
fn a_refusal_card_never_offers_the_tui_action_rows_as_buttons() {
    use summon_core::pane_input::{classify_pane, InjectionRefused};

    // 真机形态(Claude Code 2.1.269):顶线紧贴着用户那条 prompt 回显,选项 3、4 之间还有一条同宽横线。
    let pane = "❯ 帮我挑个部署环境\n\
────────────────────────────────────────────────────────────────────────\n\
 ☐ 部署环境\n\n你希望部署到哪个环境?\n\n\
❯ 1. Staging\n     预发布环境。\n  2. Production\n     生产环境。\n  3. Type something.\n\
────────────────────────────────────────────────────────────────────────\n\
  4. Chat about this\n\nEnter to select · ↑/↓ to navigate · Esc to cancel\n";
    let refused = anyhow::Error::new(InjectionRefused::on_pane(classify_pane(pane), pane));
    let sent = std::cell::RefCell::new(Vec::<String>::new());
    send_refused_menu_card(
        &refused,
        "tester",
        &summon_core::interactive::MenuLedger::default(),
        || Some(pane.to_string()),
        |card| {
            sent.borrow_mut().push(card.to_string());
            Ok(())
        },
    );
    let cards = sent.borrow();
    let card = cards.first().expect("AskUserQuestion 菜单要发卡");
    assert!(
        card.contains("Staging") && card.contains("Production"),
        "{card}"
    );
    assert!(
        !card.contains("Type something."),
        "陷阱按钮不许上卡: {card}"
    );
    assert!(
        !card.contains("Chat about this"),
        "陷阱按钮不许上卡: {card}"
    );
}

#[test]
fn agent_error_notice_is_user_visible_and_actionable() {
    // U7:按 capture 分流——claude 可 attach;无头没有 tmux,attach 必再撞错,给 tail --chat。
    let msg = agent_error_notice(
        "blue-wing",
        CaptureMode::Transcript,
        "p-12345678",
        "会话静默 240s 无任何输出",
    );
    assert!(msg.contains("blue-wing"));
    assert!(msg.contains("会话异常中断"));
    assert!(msg.contains("summon agents attach blue-wing"));

    let msg = agent_error_notice(
        "arcadia",
        CaptureMode::Rpc,
        "g-87654321",
        "无头子进程异常退出",
    );
    assert!(
        msg.contains("summon agents tail arcadia --chat g-87654321"),
        "无头应建议 tail 并带 --chat"
    );
    assert!(!msg.contains("attach"), "无头绝不建议 attach");

    // Rpc 同无头:app-server 是 stdio 子进程,没有 tmux;建议 attach 只会让用户再撞一次错。
    let msg = agent_error_notice(
        "codexbot",
        CaptureMode::Rpc,
        "g-abcdef12",
        "app-server 一轮静默超时",
    );
    assert!(msg.contains("summon agents tail codexbot --chat g-abcdef12"));
    assert!(!msg.contains("attach"), "Rpc 也绝不建议 attach");
}

#[test]
fn transcript_error_notice_keeps_path_and_marker_in_local_logs_only() {
    let local_diagnostic = "/Users/owner/.claude/projects/private/sid.jsonl marker=secret-marker";
    let notice = agent_error_notice(
        "blue-wing",
        CaptureMode::Transcript,
        "g-12345678",
        transcript_public_failure_reason(),
    );
    assert!(!notice.contains(local_diagnostic));
    assert!(!notice.contains("/Users/owner"));
    assert!(!notice.contains("secret-marker"));
    assert!(notice.contains("详细诊断仅保留在本机日志"));
}

#[test]
fn wants_system_prompt_file_for_launch_inject_or_headless() {
    // claude tmux:启动期注入 → 写文件供 --append-system-prompt-file 引用。
    assert!(wants_system_prompt_file(true, false));
    // cursor/codex 无头:无启动 flag,但 headless 路径读文件前置 PRIMER → 也要写(本次修复点)。
    assert!(wants_system_prompt_file(false, true));
    // claude 无头(显式 headless):两条件都成立。
    assert!(wants_system_prompt_file(true, true));
    // 既非启动注入也非无头(理论上不存在的组合)→ 不写。
    assert!(!wants_system_prompt_file(false, false));
}

#[test]
fn error_reaction_prefers_error_when_available() {
    let mut m = summon_core::config::builtin_messaging();
    m.reactions = vec!["OK".to_string(), "ERROR".to_string()];
    assert_eq!(error_reaction(&m), "ERROR");

    m.reactions = vec!["OK".to_string()];
    m.default_reaction = "OK".to_string();
    assert_eq!(error_reaction(&m), "OK");
}

#[test]
fn write_chat_meta_persists_chat_identity_for_cli_listing() {
    let tmp = std::env::temp_dir().join(format!("summon-chat-meta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();

    write_chat_meta(
        &agent,
        "soul-starlight-gdeadbeef",
        "oc_group",
        "group",
        Some("研发群"),
        "老板",
        Some("ou_creator"),
    );

    let meta = std::fs::read_to_string(
        agent
            .state_dir()
            .join("runtime/chats/soul-starlight-gdeadbeef.meta"),
    )
    .unwrap();
    assert!(meta.contains("chat_id=oc_group"));
    assert!(meta.contains("chat_type=group"));
    assert!(meta.contains("chat_name=研发群"));
    assert!(meta.contains("last_sender=老板"));
    assert!(meta.contains("creator=ou_creator"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn chat_meta_roundtrips_creator() {
    let tmp = std::env::temp_dir().join(format!("summon-creator-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    // 有 creator → 读回。
    write_chat_meta(
        &agent,
        "soul-starlight-gx",
        "oc_x",
        "group",
        None,
        "老板",
        Some("ou_boss"),
    );
    assert_eq!(
        read_chat_meta_creator(&agent, "soul-starlight-gx"),
        Some("ou_boss".to_string())
    );
    // 无 creator(旧 .meta 兼容)→ None。
    write_chat_meta(
        &agent,
        "soul-starlight-gy",
        "oc_y",
        "group",
        None,
        "老板",
        None,
    );
    assert_eq!(read_chat_meta_creator(&agent, "soul-starlight-gy"), None);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn effective_work_dir_prefers_per_chat_cwd_over_agent_level() {
    let tmp = std::env::temp_dir().join(format!("summon-chatcwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent =
        serde_yaml::from_str("name: starlight\nruntime:\n  working_dir: /tmp").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    // 没设 per-chat cwd → 回退 agent 级 working_dir。
    assert_eq!(read_chat_cwd(&agent, "soul-starlight-gz"), None);
    assert_eq!(
        effective_work_dir(&agent, "soul-starlight-gz", None),
        Some("/tmp".to_string())
    );
    // 设了 per-chat cwd → 覆盖 agent 级,且能读回。
    write_chat_cwd(&agent, "soul-starlight-gz", "/var/tmp");
    assert_eq!(
        read_chat_cwd(&agent, "soul-starlight-gz"),
        Some("/var/tmp".to_string())
    );
    assert_eq!(
        effective_work_dir(&agent, "soul-starlight-gz", None),
        Some("/var/tmp".to_string())
    );
    // 另一会话不受影响(按 session 隔离)。
    assert_eq!(
        effective_work_dir(&agent, "soul-starlight-gother", None),
        Some("/tmp".to_string())
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn effective_work_dir_uses_chat_working_dirs_mapping() {
    let tmp = std::env::temp_dir().join(format!("summon-chatwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    // agent 配了 chat_working_dirs:oc_a → /proj-a,oc_b → /proj-b
    let mut agent: Agent = serde_yaml::from_str(
        "name: x\nruntime:\n  working_dir: /default\n  chat_working_dirs:\n    oc_a: /proj-a\n    oc_b: /proj-b\n",
    )
    .unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    // 命中映射:oc_a → /proj-a
    assert_eq!(
        effective_work_dir(&agent, "soul-x-gz", Some("oc_a")),
        Some("/proj-a".to_string())
    );
    // 命中映射:oc_b → /proj-b
    assert_eq!(
        effective_work_dir(&agent, "soul-x-gz", Some("oc_b")),
        Some("/proj-b".to_string())
    );
    // 未命中映射:oc_c → 回退 agent 级 working_dir
    assert_eq!(
        effective_work_dir(&agent, "soul-x-gz", Some("oc_c")),
        Some("/default".to_string())
    );
    // 不传 chat_id → 跳过映射,回退 agent 级
    assert_eq!(
        effective_work_dir(&agent, "soul-x-gz", None),
        Some("/default".to_string())
    );
    // /cd 设的 per-chat cwd 优先级最高(覆盖映射)
    write_chat_cwd(&agent, "soul-x-gz", "/overridden");
    assert_eq!(
        effective_work_dir(&agent, "soul-x-gz", Some("oc_a")),
        Some("/overridden".to_string())
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// work_dir 私信选择器扫盘聚合:同群其他 agent 的 `.cwd` 判为最高档(按 `-<tag>` 会话名后缀识别),
/// 本会话自己的 `.cwd` 跳过,不存在的目录滤除,fleet 其余垫底,rank 后排序正确。
#[test]
fn scan_fleet_work_dir_options_classifies_and_ranks_by_group_proximity() {
    let tmp = tempfile::tempdir().unwrap();
    let agents_dir = tmp.path().join("agents");
    // 四个真实存在的候选目录(is_dir=true 才进候选)。
    let self_default = tmp.path().join("self-default");
    let peer_same = tmp.path().join("peer-same"); // 同群其他 agent 正用
    let peer_other = tmp.path().join("peer-other"); // 其他 chat
    let peer_default = tmp.path().join("peer-default");
    for d in [&self_default, &peer_same, &peer_other, &peer_default] {
        std::fs::create_dir_all(d).unwrap();
    }
    let gone = tmp.path().join("gone"); // 不创建 → 应被滤除

    // 共享群 chat 的 tag;各 agent 的会话名 = soul-<agent>-<tag>。
    let tag = chat_tag("group", "oc_shared");
    let self_session = format!("soul-self-{tag}");
    let peer_same_session = format!("soul-peer-{tag}");

    // self agent:默认目录 + 本会话 .cwd(应跳过)+ 一个指向已删目录的 .cwd(应滤除)。
    // 扫盘读的是**盘上** agent.yaml,故两份 manifest 都要落盘。
    let self_yaml = format!(
        "name: self\nruntime:\n  working_dir: {}\n",
        self_default.display()
    );
    let mut self_agent: Agent = serde_yaml::from_str(&self_yaml).unwrap();
    self_agent.dir = agents_dir.join("self");
    std::fs::create_dir_all(&self_agent.dir).unwrap();
    std::fs::write(self_agent.dir.join("agent.yaml"), &self_yaml).unwrap();
    write_chat_cwd(
        &self_agent,
        &self_session,
        &peer_other.display().to_string(),
    );
    write_chat_cwd(&self_agent, "soul-self-gstale", &gone.display().to_string());

    // peer agent:默认目录(fleet)+ 同群 .cwd(最高档)+ 其他 chat .cwd(fleet)。
    let peer_yaml = format!(
        "name: peer\nruntime:\n  working_dir: {}\n",
        peer_default.display()
    );
    let mut peer_agent: Agent = serde_yaml::from_str(&peer_yaml).unwrap();
    peer_agent.dir = agents_dir.join("peer");
    std::fs::create_dir_all(&peer_agent.dir).unwrap();
    std::fs::write(peer_agent.dir.join("agent.yaml"), &peer_yaml).unwrap();
    write_chat_cwd(
        &peer_agent,
        &peer_same_session,
        &peer_same.display().to_string(),
    );
    write_chat_cwd(
        &peer_agent,
        "soul-peer-gother",
        &peer_other.display().to_string(),
    );

    let options = cards::scan_fleet_work_dir_options(&self_agent, &tag, &self_session, None);
    let ranked = summon_core::work_dir::rank_work_dir_candidates(options);
    let paths: Vec<&str> = ranked.iter().map(|r| r.path.as_str()).collect();

    use summon_core::work_dir::WorkDirBucket;
    // 同群其他 agent 的目录排第一。
    assert_eq!(ranked[0].bucket, WorkDirBucket::SameChatOtherAgent);
    assert_eq!(ranked[0].path, peer_same.display().to_string());
    // 本 agent 默认在其后。
    assert!(
        ranked.iter().any(|r| r.bucket == WorkDirBucket::SelfAgent
            && r.path == self_default.display().to_string()),
        "本 agent 默认应在候选里: {paths:?}"
    );
    // 已删目录(gone)被滤除。
    assert!(
        !paths.contains(&gone.display().to_string().as_str()),
        "不存在的目录必须滤除: {paths:?}"
    );
    // peer 的其他-chat 目录进 fleet 档(peer_other 也是 self 本会话 .cwd 指向,但那条被跳过)。
    assert!(
        ranked
            .iter()
            .any(|r| r.bucket == WorkDirBucket::Fleet
                && r.path == peer_default.display().to_string()),
        "peer 默认应垫底 fleet: {paths:?}"
    );
}

/// 真机缺陷回归:per-chat `.cwd` 指向一个**已被删掉**的目录(owner 的 `summon-dev` worktree
/// 被清理后 `.cwd` 还留着)。修复前 `effective_work_dir` 原样返回它,`tmux new-session -c <不存在>`
/// 静默成功、pane 落到 `$HOME`,CLI 在整个家目录里跑。修复后必须回退到 agent 级 working_dir。
#[test]
fn start_work_dir_falls_back_when_chat_cwd_points_at_deleted_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut agent: Agent = serde_yaml::from_str(&format!(
        "name: arcadia\nruntime:\n  working_dir: {}\n",
        workspace.display()
    ))
    .unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();

    let session = "soul-arcadia-g00000000a11ce04d";
    write_chat_cwd(&agent, session, "/Users/nobody/Desktop/personal/summon-dev");

    // 纯配置视图仍如实回显陈旧覆盖(面板要显示用户配了什么)。
    assert_eq!(
        effective_work_dir(&agent, session, None),
        Some("/Users/nobody/Desktop/personal/summon-dev".to_string()),
        "展示用的配置视图不做存在性裁剪"
    );
    // 起进程用的判定必须回退到存在的 agent 级 working_dir,且绝不是家目录。
    let started = start_work_dir(&agent, session, None, None).unwrap();
    assert_eq!(
        started,
        Some(workspace.to_string_lossy().into_owned()),
        "陈旧 .cwd 必须回退 agent 级 working_dir"
    );
    let home = std::env::var("HOME").unwrap_or_default();
    assert_ne!(started.as_deref(), Some(home.as_str()), "绝不落到家目录");
}

/// `.cwd` 与 agent 级 working_dir 都不存在 → 明确失败(Err),而不是 `Ok(None)`——
/// 后者会被 `effective_start_work_dir` 兜底成进程 cwd,等价于家目录。
#[test]
fn start_work_dir_errors_when_every_configured_dir_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent =
        serde_yaml::from_str("name: arcadia\nruntime:\n  working_dir: /gone/workspace").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let session = "soul-arcadia-gz";
    write_chat_cwd(&agent, session, "/gone/worktree");

    let err = start_work_dir(&agent, session, None, None)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("工作目录都不存在") && err.contains("/gone/worktree"),
        "错误必须点名失效路径,实得: {err}"
    );
}

/// 压根没配工作目录不是错误:沿用调用方既有的进程 cwd 兜底,行为不变。
#[test]
fn start_work_dir_returns_none_when_nothing_configured() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: arcadia\n").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    assert_eq!(
        start_work_dir(&agent, "soul-arcadia-gz", None, None).unwrap(),
        None,
        "没配目录 → Ok(None),调用方兜底进程 cwd"
    );
}

/// `.cwd` 指向的目录还在时照旧优先用它(修复不能把正常覆盖也一并降级)。
#[test]
fn start_work_dir_prefers_live_chat_cwd_over_agent_default() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let mut agent: Agent = serde_yaml::from_str(&format!(
        "name: arcadia\nruntime:\n  working_dir: {}\n",
        workspace.display()
    ))
    .unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let session = "soul-arcadia-gz";
    write_chat_cwd(&agent, session, &project.to_string_lossy());
    assert_eq!(
        start_work_dir(&agent, session, None, None).unwrap(),
        Some(project.to_string_lossy().into_owned())
    );
}

/// 映射层同样参与存在性回退:`chat_working_dirs` 指到已删目录 → 落 agent 级 working_dir。
#[test]
fn start_work_dir_falls_back_from_stale_chat_mapping() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut agent: Agent = serde_yaml::from_str(&format!(
        "name: x\nruntime:\n  working_dir: {}\n  chat_working_dirs:\n    oc_a: /gone/proj-a\n",
        workspace.display()
    ))
    .unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    assert_eq!(
        start_work_dir(&agent, "soul-x-gz", Some("oc_a"), None).unwrap(),
        Some(workspace.to_string_lossy().into_owned())
    );
}

#[test]
fn pending_chats_on_disk_lists_interrupted_chats_for_boot_resume() {
    // R3:dispatcher 启动扫 threads/*/pending.json(优雅停机被打断的无头轮),主动拉起命中 chat
    // 的线程触发续跑——此前只有该 chat 再来新消息才 spawn,等答案的用户恰恰不会再发。
    let tmp = std::env::temp_dir().join(format!("summon-pendscan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent = serde_yaml::from_str("name: nimbus\nruntime:\n  cli: cursor\n").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let wal = crate::runtime::inbox_wal::InboxWal::new(tmp.join("inbox"));
    // 无 threads 目录 → 空,不 panic。
    assert!(pending_chats_on_disk(&agent, "test-scope", &wal).is_empty());
    // 落一份合法 pending.json + 一份损坏的(损坏应跳过)。
    let tag = chat_tag("p2p", "oc_pend");
    let p = summon_core::pending::PendingTurn {
        kind: summon_core::pending::PendingTurnKind::Chat,
        delivery_scope: "test-scope".into(),
        agent_name: "nimbus".into(),
        inbox_message_ids: vec!["om_pend".into()],
        resume_authorized: true,
        chat_id: "oc_pend".into(),
        chat_type: "p2p".into(),
        cli: "cursor".into(),
        prompt: "继续".into(),
        session_id: None,
        partial: String::new(),
    };
    let dir = agent.state_dir().join("threads").join(&tag);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pending.json"), serde_json::to_string(&p).unwrap()).unwrap();
    let inbound = Inbound {
        anchor_message_id: None,
        chat_id: "oc_pend".into(),
        chat_type: Some("p2p".into()),
        text: "继续".into(),
        sender: "ou_1".into(),
        sender_union_id: String::new(),
        sender_type: String::new(),
        message_id: "om_pend".into(),
        parent_id: None,
        thread_id: None,
        root_id: None,
        mentions: Vec::new(),
        resources: Vec::new(),
        create_time_ms: 1,
        replayed: false,
        reattach: false,
    };
    wal.append("nimbus", "test-scope", &inbound).unwrap();
    wal.mark_started("nimbus", "test-scope", "om_pend").unwrap();
    let bad = agent.state_dir().join("threads").join("p-bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("pending.json"), "not-json").unwrap();
    assert_eq!(
        pending_chats_on_disk(&agent, "test-scope", &wal),
        vec![("oc_pend".to_string(), "p2p".to_string())]
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn pending_scan_discards_old_or_legacy_scope_before_worker_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: nimbus").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let wal = crate::runtime::inbox_wal::InboxWal::new(tmp.path().join("inbox"));

    for (tag, scope) in [("p-old", "app-old"), ("p-legacy", "")] {
        let pending = summon_core::pending::PendingTurn {
            kind: summon_core::pending::PendingTurnKind::Chat,
            delivery_scope: scope.into(),
            agent_name: "nimbus".into(),
            inbox_message_ids: vec![format!("om_{tag}")],
            resume_authorized: true,
            chat_id: format!("oc_{tag}"),
            chat_type: "p2p".into(),
            cli: "cursor".into(),
            prompt: "旧 app 内容".into(),
            session_id: Some("sid-old".into()),
            partial: String::new(),
        };
        crate::runtime::inflight::write_pending(&agent.state_dir(), tag, &pending).unwrap();
    }

    assert!(
        pending_chats_on_disk(&agent, "app-new", &wal).is_empty(),
        "旧/缺失 scope 必须在 spawn_chat_thread 之前被过滤，因而零 CLI spawn、零 outbox"
    );
    assert!(!crate::runtime::inflight::pending_exists(&agent.state_dir(), "p-old").unwrap());
    assert!(!crate::runtime::inflight::pending_exists(&agent.state_dir(), "p-legacy").unwrap());
}

#[test]
fn pending_scan_rejects_missing_or_consumed_inbox_sources() {
    for consumed in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: nimbus\nruntime:\n  cli: cursor\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let wal = crate::runtime::inbox_wal::InboxWal::new(tmp.path().join("inbox"));
        let id = if consumed {
            "om_consumed"
        } else {
            "om_missing"
        };
        if consumed {
            let inbound = Inbound {
                anchor_message_id: None,
                chat_id: "oc_resume".into(),
                chat_type: Some("p2p".into()),
                text: "只执行一次".into(),
                sender: "ou_1".into(),
                sender_union_id: String::new(),
                sender_type: String::new(),
                message_id: id.into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: Vec::new(),
                resources: Vec::new(),
                create_time_ms: 1,
                replayed: false,
                reattach: false,
            };
            wal.append("nimbus", "test-scope", &inbound).unwrap();
            wal.mark_started("nimbus", "test-scope", id).unwrap();
            wal.mark_consumed_after_start("nimbus", "test-scope", id)
                .unwrap();
        }
        let pending = summon_core::pending::PendingTurn {
            kind: summon_core::pending::PendingTurnKind::Chat,
            delivery_scope: "test-scope".into(),
            agent_name: "nimbus".into(),
            inbox_message_ids: vec![id.into()],
            resume_authorized: true,
            chat_id: "oc_resume".into(),
            chat_type: "p2p".into(),
            cli: "cursor".into(),
            prompt: "不得重复".into(),
            session_id: Some("sid-1".into()),
            partial: String::new(),
        };
        let tag = chat_tag("p2p", "oc_resume");
        crate::runtime::inflight::write_pending(&agent.state_dir(), &tag, &pending).unwrap();

        assert!(
            pending_chats_on_disk(&agent, "test-scope", &wal).is_empty(),
            "Inbox 缺失或已 consumed 时必须零 worker"
        );
        assert!(
            !crate::runtime::inflight::pending_exists(&agent.state_dir(), &tag).unwrap(),
            "无法证明来源的残留 checkpoint 应失效"
        );
    }
}

#[test]
fn pending_scan_preserves_checkpoint_but_spawns_nothing_when_inbox_is_corrupt() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: nimbus\nruntime:\n  cli: cursor\n").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let wal_root = tmp.path().join("inbox");
    let wal = crate::runtime::inbox_wal::InboxWal::new(wal_root.clone());
    let pending = summon_core::pending::PendingTurn {
        kind: summon_core::pending::PendingTurnKind::Chat,
        delivery_scope: "test-scope".into(),
        agent_name: "nimbus".into(),
        inbox_message_ids: vec!["om_corrupt".into()],
        resume_authorized: true,
        chat_id: "oc_resume".into(),
        chat_type: "p2p".into(),
        cli: "cursor".into(),
        prompt: "不得猜测".into(),
        session_id: Some("sid-1".into()),
        partial: String::new(),
    };
    let tag = chat_tag("p2p", "oc_resume");
    crate::runtime::inflight::write_pending(&agent.state_dir(), &tag, &pending).unwrap();
    let corrupt_dir = wal_root.join("pending").join("agent-corrupt");
    std::fs::create_dir_all(&corrupt_dir).unwrap();
    std::fs::write(corrupt_dir.join("broken.json"), "not-json").unwrap();

    assert!(pending_chats_on_disk(&agent, "test-scope", &wal).is_empty());
    assert!(
        crate::runtime::inflight::pending_exists(&agent.state_dir(), &tag).unwrap(),
        "WAL 不可读时只能隔离保留，不能把未知状态当作可删除"
    );
}

/// 回归(2026-07-21 现场 blue-wing):面板「改工作目录」在会话已被「清理/重置」从内存 `chats`
/// 摘除后仍须生效。复现序列 = `/session` 唤面板 → 点 Clean/Reset(`settle_chat_exit` 做
/// `chats.remove`)→ 在同一张卡上提交工作目录表单。旧实现只认内存 handle,取不到就整个拒绝、
/// `.cwd` 根本不落盘,下次冷启回落 agent 级目录(用户观感:改了目录但 pwd 没变)。
/// 会话键是 (agent, chat) 的**盘上事实**,写 `.cwd` 是纯盘操作,不该依赖线程/tmux 是否在线。
#[test]
fn chat_session_resolves_from_disk_after_handle_evicted() {
    let tmp = std::env::temp_dir().join(format!("summon-evicted-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent =
        serde_yaml::from_str("name: blue\nruntime:\n  working_dir: /default").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let session = agent.tmux_name_for_chat("group", "oc_new", false);
    write_chat_meta(
        &agent,
        &session,
        "oc_new",
        "group",
        Some("新群"),
        "老板",
        None,
    );

    // handle 已被 Clean/Reset 摘除 → 内存 map 为空,但 .meta 还在盘上。
    let chats: HashMap<String, ChatHandle> = HashMap::new();
    assert_eq!(
        resolve_chat_session(&chats, &agent, "oc_new").as_deref(),
        Some(session.as_str()),
        "会话不在线时应回退盘上 .meta 解析会话键,而非拒绝设置工作目录"
    );
    // chat_type 同样必须来自盘上事实:群会话不得被兜底成 p2p(否则 tag 从 g… 变 p…,
    // 回执归档与面板刷新都会落到幽灵 tag 上)。
    assert_eq!(
        resolve_chat_type(&chats, &agent, "oc_new"),
        "group",
        "群会话在 handle 摘除后不得兜底成 p2p"
    );

    // 解析出会话键后写 cwd → 下次冷启 effective_work_dir 读得到(链路闭合)。
    write_chat_cwd(&agent, &session, "/var/tmp");
    assert_eq!(
        effective_work_dir(&agent, &session, Some("oc_new")),
        Some("/var/tmp".to_string()),
        "写入的 per-chat cwd 应压过 agent 级 working_dir"
    );

    // 盘上也没有记录的陌生 chat → 仍返回 None,由调用方给出明确拒绝(不静默瞎猜会话名)。
    assert_eq!(resolve_chat_session(&chats, &agent, "oc_unknown"), None);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 回归(安全边界):operate-gate 的 `is_group` 判定不得因内存 handle 被摘除而 fail-open。
/// `can_operate` 在 operators/allowed_users 皆空时返回 `!is_group`——群里拒、私聊放行。
/// 旧实现 `chats.get(..).map(..).unwrap_or(false)`:群会话被「清理/重置」摘除 handle 后
/// (或 daemon 重启后 `chats` 尚空、旧面板卡仍可点),群被误判成私聊 → 破坏性按钮对任何人放行,
/// 违反 CLAUDE.md「皆空群里 fail-closed」的不变量。
#[test]
fn card_gate_is_group_stays_fail_closed_after_handle_evicted() {
    let tmp = std::env::temp_dir().join(format!("summon-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    // 名单皆空 = allow-all,群里必须 fail-closed(这正是被 is_group 误判毁掉的那条)。
    let mut agent: Agent = serde_yaml::from_str("name: g\nfeishu:\n  app_id_env: A\n").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let session = agent.tmux_name_for_chat("group", "oc_grp", false);
    write_chat_meta(&agent, &session, "oc_grp", "group", Some("群"), "谁", None);

    let chats: HashMap<String, ChatHandle> = HashMap::new();
    assert!(
        card_chat_is_group(&chats, &agent, "oc_grp"),
        "handle 被摘除后仍须凭盘上 .meta 认出这是群"
    );
    assert!(
        !agent.can_operate(
            "ou_stranger",
            "",
            card_chat_is_group(&chats, &agent, "oc_grp"),
            OperateScope::Agent
        ),
        "群会话在 handle 缺失时不得对陌生人放行破坏性动作"
    );

    // 私聊仍按盘上事实放行(裸会话即 owner),不能因为改成 fail-closed 就把私聊一起锁死。
    let p2p_session = agent.tmux_name_for_chat("p2p", "oc_dm", false);
    write_chat_meta(&agent, &p2p_session, "oc_dm", "p2p", None, "owner", None);
    assert!(!card_chat_is_group(&chats, &agent, "oc_dm"));
    assert!(agent.can_operate(
        "ou_owner",
        "",
        card_chat_is_group(&chats, &agent, "oc_dm"),
        OperateScope::Agent
    ));

    // 盘上也无记录的未知 chat → 按群处理(fail-closed),不猜成私聊放行。
    assert!(
        card_chat_is_group(&chats, &agent, "oc_never_seen"),
        "无从判定时必须倒向 fail-closed"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn chat_session_on_disk_finds_session_by_chat_id() {
    // ChatGone(群解散)在 daemon 重启后内存无 handle:从 .meta 按 chat_id 反查 session/type/creator。
    let tmp = std::env::temp_dir().join(format!("summon-ondisk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    write_chat_meta(
        &agent,
        "soul-starlight-gbeef",
        "oc_beef",
        "group",
        Some("研发群"),
        "老板",
        Some("ou_boss"),
    );
    let got = chat_session_on_disk(&agent, "oc_beef").expect("应按 chat_id 命中");
    assert_eq!(got.0, "soul-starlight-gbeef");
    assert_eq!(got.1, "group");
    assert_eq!(got.2.as_deref(), Some("ou_boss"));
    // 历史重复 .meta 不得按 read_dir 随机命中；无活 tmux 时取 updated_at 更新者。
    let newer = summon_core::chat_meta::ChatMeta {
        chat_id: Some("oc_beef".into()),
        chat_type: Some("group".into()),
        chat_name: Some("研发群".into()),
        tag: Some("gbeef".into()),
        last_sender: Some("老板".into()),
        updated_at: Some("2099-01-01 00:00:00".into()),
        creator: Some("ou_new".into()),
    };
    let newer_path = chat_meta_path(&agent, "soul-starlight-gnew");
    std::fs::write(newer_path, newer.render()).unwrap();
    let got = chat_session_on_disk(&agent, "oc_beef").expect("重复记录应确定性选最新");
    assert_eq!(got.0, "soul-starlight-gnew");
    assert_eq!(got.2.as_deref(), Some("ou_new"));
    // 不存在的 chat_id → None。
    assert!(chat_session_on_disk(&agent, "oc_nope").is_none());
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn handle_chat_gone_offline_tears_down_and_enqueues_outbox() {
    // 内存无 handle(daemon 重启后)收到 ChatGone:读 .meta 重建 → do_teardown(无 tmux 即 no-op)
    // → 退出回执**入队 outbox**(不内联联网)。断言 pending/ 多了一条 NotifyChat,字段正确。
    let tmp = std::env::temp_dir().join(format!("summon-gone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    write_chat_meta(
        &agent,
        "soul-starlight-gbeef",
        "oc_beef",
        "group",
        Some("研发群"),
        "老板",
        Some("ou_boss"),
    );
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true); // dry-run
    let messaging = builtin_messaging_for_test();
    let outbox = crate::runtime::outbox_store::OutboxStore::new(tmp.join("outbox"));
    let peers = crate::runtime::peers_store::PeersStore::new(tmp.join("peers"));
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    let inflight = crate::runtime::inflight::InFlight::new();
    // 不在线分支:chats 空(inflight 未用,仅满足签名)。
    handle_chat_gone(
        &mut chats,
        &agent,
        &fc,
        &messaging,
        &outbox,
        &peers,
        &inflight,
        200,
        800,
        "oc_beef",
        summon_core::exit::ExitReason::GroupDisbanded,
        "ou_op",
    );
    assert!(chats.is_empty(), "无残留 handle");
    // 入队而非真发:pending/ 里恰好一条 NotifyChat,reason/creator/operator 正确。
    let due = outbox.list_due(now_unix_ms() + 1);
    assert_eq!(due.len(), 1, "离线 gone 应入队一条退出通知");
    match &due[0].kind {
        summon_core::outbox::OutboxKind::NotifyChat {
            chat_id,
            chat_type,
            creator,
            event_operator,
            reason,
            text,
            ..
        } => {
            assert_eq!(chat_id, "oc_beef");
            assert_eq!(chat_type, "group");
            assert_eq!(creator.as_deref(), Some("ou_boss"));
            assert_eq!(event_operator.as_deref(), Some("ou_op"));
            assert_eq!(*reason, summon_core::exit::ExitReason::GroupDisbanded);
            assert!(!text.trim().is_empty(), "文案快照非空");
            assert!(text.contains("群「研发群」"), "退出私聊必须指出来源群");
        }
        other => panic!("应入队 NotifyChat,得到 {other:?}"),
    }
    assert_eq!(due[0].agent, "starlight");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn handle_chat_gone_falls_back_when_online_thread_drops_without_ack() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let outbox = crate::runtime::outbox_store::OutboxStore::new(tmp.path().join("outbox"));
    let peers = crate::runtime::peers_store::PeersStore::new(tmp.path().join("peers"));
    let inflight = crate::runtime::inflight::InFlight::new();
    let (tx, rx) = channel();
    let receiver = std::thread::spawn(move || {
        // 模拟 Clean/Kill 先退出：Gone 已 send 成功，但消息被 drop，没有本地 ACK。
        let _ = rx.recv();
    });
    let mut chats = HashMap::from([(
        "oc_ack_lost".to_string(),
        ChatHandle {
            tx,
            last_activity: SystemTime::now(),
            idle_prompted: false,
            chat_type: "group".into(),
            tmux_name: agent.tmux_name_for_chat("group", "oc_ack_lost", false),
            tmux_seen_live: false,
            local_claimed: false,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    )]);

    handle_chat_gone(
        &mut chats,
        &agent,
        &fc,
        &messaging,
        &outbox,
        &peers,
        &inflight,
        200,
        800,
        "oc_ack_lost",
        summon_core::exit::ExitReason::BotRemoved,
        "ou_owner",
    );
    receiver.join().unwrap();

    assert!(chats.is_empty());
    assert_eq!(outbox.list_due(now_unix_ms() + 1).len(), 1);
}

/// 离线 kill 的夹具:只有盘上 `.meta`,没有 chat 线程、没有活的 tmux(会话名不存在)。
fn offline_kill_fixture(
    label: &str,
) -> (
    std::path::PathBuf,
    Agent,
    FeishuClient,
    crate::runtime::outbox_store::OutboxStore,
) {
    let tmp = std::env::temp_dir().join(format!("summon-kill-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    write_chat_meta(
        &agent,
        "soul-starlight-px",
        "oc_px",
        "p2p",
        None,
        "老板",
        None,
    );
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let outbox = crate::runtime::outbox_store::OutboxStore::new(tmp.join("outbox"));
    (tmp, agent, fc, outbox)
}

fn only_notify_reason(
    outbox: &crate::runtime::outbox_store::OutboxStore,
) -> summon_core::exit::ExitReason {
    let due = outbox.list_due(now_unix_ms() + 1);
    assert_eq!(due.len(), 1, "应入队一条退出通知");
    match &due[0].kind {
        summon_core::outbox::OutboxKind::NotifyChat {
            reason, chat_id, ..
        } => {
            assert_eq!(chat_id, "oc_px");
            *reason
        }
        other => panic!("应入队 NotifyChat,得到 {other:?}"),
    }
}

#[test]
fn handle_teardown_chat_offline_enqueues_manual_kill_notify() {
    // agents kill 离线分支:读 .meta 重建 → 打断在跑轮 → do_teardown → 入队 ManualKill(不内联联网)。
    let (tmp, agent, fc, outbox) = offline_kill_fixture("off");
    let inflight = crate::runtime::inflight::InFlight::new();
    inflight.insert(
        crate::runtime::inflight::TurnKey::new("starlight", chat_tag("p2p", "oc_px")),
        crate::runtime::inflight::InFlightTurn {
            killer: fake_killer(),
            base: tmp.clone(),
            pending: pending_arc("活儿", "半截"),
            shape: crate::runtime::inflight::TurnShape::OwnedProcess,
        },
    );
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    handle_teardown_chat(
        &mut chats,
        &agent,
        &fc,
        &builtin_messaging_for_test(),
        &outbox,
        &inflight,
        200,
        800,
        "oc_px",
        false,
    );
    assert_eq!(
        only_notify_reason(&outbox),
        summon_core::exit::ExitReason::ManualKill
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 会话早已退出、记录也早清了(用户先在空闲提醒卡上点了退出),再 kill 一次什么都没改变:
/// 不发「会话已关闭」,否则群里会多出一张像是又被关了一次的卡。
#[test]
fn handle_teardown_chat_offline_with_nothing_left_sends_no_receipt() {
    let (tmp, agent, fc, outbox) = offline_kill_fixture("nothing");
    let inflight = crate::runtime::inflight::InFlight::new();
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    for reset in [true, false] {
        handle_teardown_chat(
            &mut chats,
            &agent,
            &fc,
            &builtin_messaging_for_test(),
            &outbox,
            &inflight,
            200,
            800,
            "oc_px",
            reset,
        );
    }
    assert!(outbox.list_due(now_unix_ms() + 1).is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
}

/// reset 真清掉了一份会话记录也是改变:下次从空上下文冷启,要告诉群里。
#[test]
fn handle_teardown_chat_offline_reset_that_clears_a_record_notifies() {
    let (tmp, agent, fc, outbox) = offline_kill_fixture("record");
    crate::session::resume_store::write(&agent, "soul-starlight-px", "claude", "sid-1", None, None);
    let inflight = crate::runtime::inflight::InFlight::new();
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    handle_teardown_chat(
        &mut chats,
        &agent,
        &fc,
        &builtin_messaging_for_test(),
        &outbox,
        &inflight,
        200,
        800,
        "oc_px",
        true,
    );
    assert_eq!(
        only_notify_reason(&outbox),
        summon_core::exit::ExitReason::ManualKillReset
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn handle_teardown_chat_online_interrupts_inflight_before_teardown() {
    // R2:online kill 必须先 SIGINT 在跑无头轮——否则 KillTeardown 被 chat 线程的 poll 当控制消息
    // requeue 到本轮自然 EOF 之后才处理,届时 interrupt_one 已查无在跑轮,「先打断」成空操作。
    let tmp = std::env::temp_dir().join(format!("summon-kill-online-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent = serde_yaml::from_str("name: starlight").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let outbox = crate::runtime::outbox_store::OutboxStore::new(tmp.join("outbox"));
    let inflight = crate::runtime::inflight::InFlight::new();
    // 注册一条在跑无头轮(pgid 用不存在的:interrupt 只置幂等闸,killpg 对缺失 pgid 无副作用)。
    let killer = fake_killer();
    let tag = chat_tag("p2p", "oc_live");
    inflight.insert(
        crate::runtime::inflight::TurnKey::new("starlight", tag),
        crate::runtime::inflight::InFlightTurn {
            killer: killer.clone(),
            base: tmp.clone(),
            pending: pending_arc("活儿", "半截"),
            shape: crate::runtime::inflight::TurnShape::OwnedProcess,
        },
    );
    // online 句柄:带一条可收的 channel。
    let (tx, rx) = channel::<ThreadMsg>();
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert(
        "oc_live".into(),
        ChatHandle {
            tx,
            last_activity: std::time::SystemTime::now(),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: String::new(),
            tmux_seen_live: false,
            local_claimed: false,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    );
    handle_teardown_chat(
        &mut chats, &agent, &fc, &messaging, &outbox, &inflight, 200, 800, "oc_live", false,
    );
    // 先打断:killer 幂等闸已置(was_killed=true);killer 与注册表里的 clone 共享同一 Arc<AtomicBool>。
    assert!(killer.was_killed(), "online kill 应先打断在跑无头轮(R2)");
    // 再投 KillTeardown 给 chat 线程收尾。
    match rx.recv_timeout(Duration::from_millis(200)).unwrap() {
        ThreadMsg::KillTeardown { reset } => assert!(!reset, "默认 Clean(reset=false)"),
        _ => panic!("应投 KillTeardown"),
    }
    // 在线分支不入队 outbox(回执由 chat 线程发)。
    assert_eq!(
        outbox.list_due(now_unix_ms() + 1).len(),
        0,
        "在线分支不入队 outbox"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

fn idle_closed_handle(session: &str, stale: SystemTime) -> (ChatHandle, Receiver<ThreadMsg>) {
    let (tx, rx) = channel();
    (
        ChatHandle {
            tx,
            last_activity: stale,
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: session.into(),
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
        rx,
    )
}

#[test]
fn closed_idle_sessions_emit_neither_prompts_nor_retire_intents_until_reopened() {
    for cli in ["claude", "codex", "cursor", "cursor-acp"] {
        let (mut agent, _tmp) = crate::test_support::agent_dir(cli, "");
        agent.runtime.as_mut().unwrap().resume = Some(true);
        let mut messaging = builtin_messaging_for_test();
        messaging.idle_retire_secs = Some(1);
        let stale = SystemTime::now() - messaging.idle_ttl() - Duration::from_secs(10);
        let session = format!("soul-idle-closed-{}", uuid::Uuid::new_v4().simple());
        let tag = chat_tag("p2p", "closed-idle");
        let (handle, rx) = idle_closed_handle(&session, stale);
        let mut chats = HashMap::from([("closed-idle".into(), handle)]);
        crate::session::resume_store::write(
            &agent,
            &session,
            &agent.resume_cli_kind(),
            "native-ended",
            None,
            None,
        );
        crate::session::lifecycle::mark_closed(&agent, &tag);
        let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
        let (self_tx, self_rx) = channel();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
        assert!(rx.try_recv().is_err(), "{cli}: 已关闭会话不自动退出");
        assert!(
            self_rx.try_recv().is_err(),
            "{cli}: 已关闭会话不新增退出回执"
        );
        assert!(!chats["closed-idle"].idle_prompted, "{cli}");
        assert!(!idle_prompt_marker_path(&agent, "p2p", "closed-idle").exists());
        let outbound = agent
            .state_dir()
            .join("threads")
            .join(&tag)
            .join("outbound.ndjson");
        assert!(!outbound.exists(), "{cli}: 已关闭会话不联网发空闲卡");
        assert_eq!(
            crate::session::resume_store::read_id(&agent, &session, &agent.resume_cli_kind())
                .as_deref(),
            Some("native-ended")
        );

        crate::session::lifecycle::mark_open(&agent, &tag).unwrap();
        messaging.idle_retire_secs = None;
        run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
        assert!(
            chats["closed-idle"].idle_prompted,
            "{cli}: 重新准入后按正常空闲规则提示"
        );
        assert!(std::fs::read_to_string(outbound)
            .unwrap()
            .contains("idle_prompt_card"));
    }
}

#[test]
fn empty_orphan_batches_leave_workers_activity_and_idle_markers_untouched() {
    for existing in [false, true] {
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let stale = SystemTime::now() - Duration::from_secs(4000);
        let mut chats = HashMap::new();
        let (mut handle, rx) = idle_closed_handle("soul-orphan-empty", stale);
        handle.idle_prompted = true;
        handle.last_inbound_id = "latest-human".into();
        if existing {
            chats.insert("orphan-chat".into(), handle);
        }
        write_idle_prompt_marker(&agent, "p2p", "orphan-chat", stale, stale).unwrap();
        let marker = idle_prompt_marker_path(&agent, "p2p", "orphan-chat");
        let before = std::fs::read(&marker).unwrap();
        assert_eq!(
            redeliver_orphaned_tasks(&mut chats, &agent, "orphan-chat", Vec::new(), || panic!(
                "空批次不能创建 worker"
            )),
            Some(0)
        );
        assert_eq!(chats.len(), usize::from(existing));
        assert_eq!(std::fs::read(marker).unwrap(), before);
        assert!(rx.try_recv().is_err());
        if existing {
            assert_eq!(chats["orphan-chat"].last_activity, stale);
            assert!(chats["orphan-chat"].idle_prompted);
            assert_eq!(chats["orphan-chat"].last_inbound_id, "latest-human");
        }
    }
}

#[test]
fn orphan_redelivery_keeps_the_chat_absent_when_worker_creation_is_denied() {
    let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
    let mut chats = HashMap::new();
    let task = match task_msg_with_id("om_orphan", "待交接消息") {
        ThreadMsg::Task(task) => *task,
        _ => unreachable!(),
    };
    let mut attempts = 0;
    assert_eq!(
        redeliver_orphaned_tasks(&mut chats, &agent, "denied-chat", vec![task], || {
            attempts += 1;
            None
        }),
        None
    );
    assert_eq!(attempts, 1);
    assert!(chats.is_empty());
    assert!(!idle_prompt_marker_path(&agent, "p2p", "denied-chat").exists());
}

#[test]
fn orphan_redelivery_starts_one_worker_and_preserves_task_order() {
    let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
    let mut chats = HashMap::new();
    let (handle, rx) = idle_closed_handle("soul-orphan-full", SystemTime::now());
    let tasks = ["om_first", "om_second"]
        .into_iter()
        .map(|id| match task_msg_with_id(id, id) {
            ThreadMsg::Task(task) => *task,
            _ => unreachable!(),
        })
        .collect();
    let mut spawns = 0;
    assert_eq!(
        redeliver_orphaned_tasks(&mut chats, &agent, "orphan-chat", tasks, || {
            spawns += 1;
            Some(handle)
        }),
        Some(2)
    );
    assert_eq!(spawns, 1);
    for expected in ["om_first", "om_second"] {
        match rx.try_recv().unwrap() {
            ThreadMsg::Task(task) => assert_eq!(task.message_id, expected),
            _ => panic!("交接应为原任务"),
        }
    }
    assert!(rx.try_recv().is_err());
    assert_eq!(chats["orphan-chat"].last_inbound_id, "om_second");
}

#[test]
fn run_idle_scan_prompts_every_capture_mode() {
    // 超 idle_ttl 的会话一律发空闲卡,不分捕获模式:claude 的 tmux 与 codex/cursor 的常驻
    // RPC 会话都是「挂着不说话也一直占着」的东西,卡上的退出/保活对它们语义相同。
    // dry-run fc 让 send_card 短路 Ok;tmux_name 用不存在的会话名(has_session=false),不依赖 tmux/网络。
    let tmp = std::env::temp_dir().join(format!("summon-idle-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, _self_rx) = channel::<DispatchMsg>();
    let stale =
        std::time::SystemTime::now() - Duration::from_secs(messaging.idle_ttl().as_secs() + 10);
    let mk_stale = || {
        let (tx, _rx) = channel::<ThreadMsg>();
        ChatHandle {
            tx,
            last_activity: stale,
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: "soul-nope-idle-test".into(), // 不存在 → has_session=false,不依赖 tmux
            tmux_seen_live: false,
            local_claimed: false,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        }
    };

    // Transcript(claude):到点 → 发卡、置 idle_prompted。
    let mut claude_agent: Agent =
        serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    claude_agent.dir = tmp.join("agents").join(&claude_agent.name);
    std::fs::create_dir_all(&claude_agent.dir).unwrap();
    std::fs::create_dir_all(claude_agent.state_dir()).unwrap();
    let claude_tag = chat_tag("p2p", "c1");
    crate::session::resume_store::write(
        &claude_agent,
        "soul-nope-idle-test",
        &claude_agent.resume_cli_kind(),
        "sid-idle-context",
        None,
        None,
    );
    crate::runtime::status_snapshot::record_context(
        &claude_agent.state_dir(),
        &claude_tag,
        "sid-idle-context",
        summon_core::context_budget::ContextBudget::from_tokens(350_000, Some(1_000_000)),
    )
    .unwrap();
    // 空目录上先取迁移证明(真路径,不可绕过):本测试的 chat 全部生于新机制之下。
    let migrated = ensure_idle_prompt_marker_migration(&claude_agent).unwrap();
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert("c1".into(), mk_stale());
    run_idle_scan(
        &mut chats,
        &claude_agent,
        &messaging,
        &self_tx,
        &fc,
        &migrated,
    );
    assert!(
        chats["c1"].idle_prompted,
        "Transcript 超 idle_ttl 应发空闲卡"
    );
    let archived = std::fs::read_to_string(
        claude_agent
            .state_dir()
            .join("threads")
            .join(&claude_tag)
            .join("outbound.ndjson"),
    )
    .unwrap();
    assert!(archived.contains("当前 ctx"));
    assert!(archived.contains("350k / 1.0M"));
    assert!(archived.contains("35%"));
    // 同一会话已提示过 → 不重发(idle_prompted 仍 true、不报错)。
    run_idle_scan(
        &mut chats,
        &claude_agent,
        &messaging,
        &self_tx,
        &fc,
        &migrated,
    );
    assert!(chats["c1"].idle_prompted);

    // 无头(cursor ACP / codex Rpc):同样到点 → 同样发卡,卡也真落了存档。
    for cli in ["cursor", "codex"] {
        let mut headless_agent: Agent =
            serde_yaml::from_str(&format!("name: starlight\nruntime:\n  cli: {cli}\n")).unwrap();
        headless_agent.dir = tmp.join(cli).join("agents").join(&headless_agent.name);
        std::fs::create_dir_all(&headless_agent.dir).unwrap();
        std::fs::create_dir_all(headless_agent.state_dir()).unwrap();
        let migrated_headless = ensure_idle_prompt_marker_migration(&headless_agent).unwrap();
        let mut headless: HashMap<String, ChatHandle> = HashMap::new();
        headless.insert("h1".into(), mk_stale());
        run_idle_scan(
            &mut headless,
            &headless_agent,
            &messaging,
            &self_tx,
            &fc,
            &migrated_headless,
        );
        assert!(headless["h1"].idle_prompted, "{cli} 超 idle_ttl 应发空闲卡");
        let archived = std::fs::read_to_string(
            headless_agent
                .state_dir()
                .join("threads")
                .join(chat_tag("p2p", "h1"))
                .join("outbound.ndjson"),
        )
        .unwrap_or_else(|e| panic!("{cli} 空闲卡应落存档: {e}"));
        assert!(archived.contains("会话空闲提醒"), "{cli}");
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn run_idle_scan_retires_scheduled_codex_and_cursor_tails() {
    // 定时尾轮到点,无头会话与 claude 走同一条通路:默认 Clean(归档续聊,常驻 RPC 会话进程
    // 由 Clean 臂收掉、resume 记录留着),开了 timer_idle_retire_reset 才 Reset。两者都静默。
    let tmp = tempfile::tempdir().unwrap();
    for cli in ["codex", "cursor"] {
        for reset in [false, true] {
            let mut agent: Agent =
                serde_yaml::from_str(&format!("name: starlight\nruntime:\n  cli: {cli}\n"))
                    .unwrap();
            agent.dir = tmp
                .path()
                .join(format!("{cli}-{reset}"))
                .join("agents")
                .join(&agent.name);
            std::fs::create_dir_all(agent.state_dir()).unwrap();

            let mut messaging = builtin_messaging_for_test();
            messaging.timer_idle_retire_secs = 1;
            messaging.timer_idle_retire_reset = reset;
            let session = format!("soul-{cli}-timer-{reset}");
            crate::session::resume_store::write(
                &agent,
                &session,
                &agent.resume_cli_kind(),
                "thread-before-retire",
                None,
                None,
            );

            let (chat_tx, chat_rx) = channel::<ThreadMsg>();
            let mut chats = HashMap::from([(
                "c1".to_string(),
                ChatHandle {
                    tx: chat_tx,
                    last_activity: SystemTime::now() - Duration::from_secs(2),
                    idle_prompted: false,
                    chat_type: "p2p".into(),
                    tmux_name: session,
                    tmux_seen_live: false,
                    local_claimed: true,
                    last_inbound_scheduled: true,
                    last_inbound_id: "scheduled-1".into(),
                    turn_was_active: false,
                    subagent_state: None,
                    menu_ledger: None,
                },
            )]);
            let (self_tx, _self_rx) = channel::<DispatchMsg>();
            let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
            let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();

            run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);

            let why = format!("{cli} reset={reset}");
            match chat_rx.try_recv() {
                Ok(ThreadMsg::Control {
                    ctrl,
                    notify_exit,
                    idle_retired,
                    ..
                }) => {
                    if reset {
                        assert!(matches!(ctrl, Control::Reset), "{why}");
                    } else {
                        assert!(matches!(ctrl, Control::Clean), "{why}");
                    }
                    assert!(!notify_exit, "{why}:定时尾轮回收保持静默");
                    assert!(idle_retired, "{why}");
                }
                _ => panic!("{why}:定时尾轮到点应收到回收意图"),
            }
            // 静默回收托管了空闲处理 → 同一轮不再发提醒卡。
            assert!(!chats["c1"].idle_prompted, "{why}:定时尾轮不出提醒卡");
        }
    }
}

#[test]
fn run_idle_scan_keeps_sessions_whose_yielded_background_work_is_running() {
    // 主 Agent 让出本轮后，后台任务还在跑的会话等同在跑：不回收、不发空闲提醒。
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent =
        serde_yaml::from_str("name: starlight\nruntime:\n  cli: codex\n").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let mut messaging = builtin_messaging_for_test();
    messaging.timer_idle_retire_secs = 1;
    let holders =
        crate::runtime::background_holder::BackgroundHolderStore::for_agent(&agent.state_dir());
    let stale = SystemTime::now() - Duration::from_secs(messaging.idle_ttl().as_secs() + 10);
    let mut chats = HashMap::new();
    let mut receivers = HashMap::new();
    for (chat, scheduled, last_activity) in [
        ("c1", true, SystemTime::now() - Duration::from_secs(2)),
        ("c2", false, stale),
    ] {
        let session = format!("soul-codex-held-{chat}");
        crate::session::resume_store::write(
            &agent,
            &session,
            &agent.resume_cli_kind(),
            "thread-held",
            None,
            None,
        );
        holders
            .add(
                &chat_tag("p2p", chat),
                crate::runtime::background_holder::BackgroundHolder {
                    turn_id: format!("turn-{chat}"),
                    message_id: format!("om-{chat}"),
                    chat_id: chat.into(),
                    chat_type: "p2p".into(),
                    origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
                    requester_open_id: "ou_requester".into(),
                    requester_union_id: String::new(),
                    requester_is_peer: false,
                    tmux_session: session.clone(),
                    session_id: "thread-held".into(),
                    transcript: tmp.path().join("t.jsonl"),
                    tasks: Vec::new(),
                    created_at_ms: 0,
                    expires_at_ms: i64::MAX,
                },
            )
            .unwrap();
        let (tx, rx) = channel::<ThreadMsg>();
        receivers.insert(chat, rx);
        chats.insert(
            chat.to_string(),
            ChatHandle {
                tx,
                last_activity,
                idle_prompted: false,
                chat_type: "p2p".into(),
                tmux_name: session,
                tmux_seen_live: false,
                local_claimed: true,
                last_inbound_scheduled: scheduled,
                last_inbound_id: format!("in-{chat}"),
                turn_was_active: false,
                subagent_state: None,
                menu_ledger: None,
            },
        );
    }
    let (self_tx, _self_rx) = channel::<DispatchMsg>();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();

    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert!(
        receivers["c1"].try_recv().is_err(),
        "后台任务在跑时定时尾轮不回收"
    );
    assert!(!chats["c2"].idle_prompted, "后台任务在跑时不发空闲提醒");

    holders.clear(&chat_tag("p2p", "c1"));
    holders.clear(&chat_tag("p2p", "c2"));
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert!(
        matches!(receivers["c1"].try_recv(), Ok(ThreadMsg::Control { .. })),
        "后台任务结束后照常回收"
    );
    assert!(
        chats["c2"].last_activity > stale,
        "后台任务结束时刻起算空闲，而不是沿用让轮前的入站时刻"
    );
}

#[test]
fn run_idle_scan_drops_background_holders_of_a_vanished_claude_session() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent: Agent =
        serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    agent.dir = tmp.path().join("agents").join(&agent.name);
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    const CHAT: &str = "c1";
    const SESSION: &str = "soul-holder-vanished-idle-test";
    let tag = chat_tag("group", CHAT);
    let holders =
        crate::runtime::background_holder::BackgroundHolderStore::for_agent(&agent.state_dir());
    holders
        .add(
            &tag,
            crate::runtime::background_holder::BackgroundHolder {
                turn_id: "turn-held".into(),
                message_id: "om-held".into(),
                chat_id: CHAT.into(),
                chat_type: "group".into(),
                origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
                requester_open_id: "ou_requester".into(),
                requester_union_id: String::new(),
                requester_is_peer: false,
                tmux_session: SESSION.into(),
                session_id: "sid-1".into(),
                transcript: std::path::PathBuf::from("/nonexistent"),
                tasks: Vec::new(),
                created_at_ms: 0,
                expires_at_ms: i64::MAX,
            },
        )
        .unwrap();
    let (tx, _rx) = channel::<ThreadMsg>();
    let mut chats = HashMap::from([(
        CHAT.to_string(),
        ChatHandle {
            tx,
            last_activity: SystemTime::now(),
            idle_prompted: false,
            chat_type: "group".into(),
            tmux_name: SESSION.into(),
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: "in-1".into(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    )]);
    let (self_tx, _self_rx) = channel::<DispatchMsg>();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    run_idle_scan(
        &mut chats,
        &agent,
        &builtin_messaging_for_test(),
        &self_tx,
        &fc,
        &migrated,
    );
    assert!(!holders.has_any(&tag), "会话不在了,后台任务也不在了");
}

#[test]
fn run_idle_scan_falls_back_to_prompt_card_when_timer_quiet_gates_unprovable() {
    // 定时尾轮 + timer_idle_retire 开着(内置默认),但结构门槛证不了(这里:tmux 不在,
    // resume 记录也无从谈起)→ 静默回收不可达。此时必须回退旧行为:到点照发提醒卡,
    // 也绝不产 retire 意图 —— 不许把会话晾在「既无卡又不回收」的悬空态。
    let tmp = std::env::temp_dir().join(format!("summon-timer-quiet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent =
        serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    assert!(
        messaging.timer_idle_retire().is_some(),
        "前提:内置默认必须是开(owner 2026-08-13 授权),否则本测试测了个寂寞"
    );
    let (self_tx, self_rx) = channel::<DispatchMsg>();
    let (tx, thread_rx) = channel::<ThreadMsg>();
    let stale =
        std::time::SystemTime::now() - Duration::from_secs(messaging.idle_ttl().as_secs() + 10);
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert(
        "c1".into(),
        ChatHandle {
            tx,
            last_activity: stale,
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: "soul-nope-timer-quiet-test".into(), // 不存在 → has_session=false
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: true, // 尾轮是定时任务
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    );
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert!(
        chats["c1"].idle_prompted,
        "门槛证不了 → 回退旧行为,提醒卡照发"
    );
    assert!(
        thread_rx.try_recv().is_err(),
        "门槛证不了 → 不产 Control::Clean 意图"
    );
    assert!(
        self_rx.try_recv().is_err(),
        "未曾观测 live 的缺失 tmux 不该被合成外带 kill 回执"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// owner 2026-08-14 报告的修复主线(提醒卡半边):主轮结束但后台 subagent/workflow 还在跑
/// → 会话不算空闲,提醒卡不发;subagent 全部结束后,空闲期从**它的最后活动**起算 ——
/// 未到点不发、到点照发(绝不因观测存在而永不提醒)。判活沿用观测子系统的 2h 时效轴:
/// 冻结超窗的 Running(watcher 死了的残留形态)不再挡卡。
/// 变异自验(实测,2026-08-14):去掉 `effective_idle_anchor` 的 max → ②必红;去掉判活的
/// 时效窗 → ④必红。注意①在默认 ttl(1d > 2h 时效窗)下被 anchor 推进与 active 门**双重**
/// 保护,单去 `should_prompt_idle` 的 subagent 门①不红 —— 那个门由单元真值表
/// (`should_prompt_idle_only_when_stale_and_not_yet_prompted` 末条)钉死;它在 ttl < 2h
/// 的配置下才是唯一防线。
#[test]
fn run_idle_scan_defers_idle_card_to_subagent_activity() {
    let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, _self_rx) = channel::<DispatchMsg>();
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    let ttl = messaging.idle_ttl();
    let now_utc = chrono::Utc::now();

    let state: SharedSubagentState = Arc::new(std::sync::Mutex::new(
        summon_core::subagent::SubagentState::new(),
    ));
    // 后台 subagent 在跑(活跃锚在时效窗内)。
    state.lock().unwrap().promote_to_running(
        "bg1",
        "后台修 bug",
        "tu_1",
        None,
        now_utc - chrono::Duration::minutes(10),
    );
    let mk_handle = |st: Option<SharedSubagentState>| {
        let (tx, _rx) = channel::<ThreadMsg>();
        ChatHandle {
            tx,
            // 入站早已停(远超 ttl):没有 subagent 观测时本该发卡。
            last_activity: SystemTime::now() - ttl - Duration::from_secs(600),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: "soul-nope-subagent-idle".into(), // 不存在 → has_session=false
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: st,
            menu_ledger: None,
        }
    };
    let idle_cards = |chat_id: &str| {
        std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(chat_tag("p2p", chat_id))
                .join("outbound.ndjson"),
        )
        .map(|s| {
            s.lines()
                .filter(|l| l.contains(r#""kind":"idle_prompt_card""#))
                .count()
        })
        .unwrap_or(0)
    };

    // ① subagent 在跑 → 不发卡(会话在工作,卡只是噪音)。
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert("oc_sub".into(), mk_handle(Some(state.clone())));
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(idle_cards("oc_sub"), 0, "subagent 在跑时不得发空闲卡");
    assert!(
        !chats["oc_sub"].idle_prompted,
        "去重位不得被占用(不是漏发,是未到点)"
    );

    // ② subagent 刚结束(完成时刻在 ttl 内)→ 空闲期从它的最后活动起算,仍未到点。
    state
        .lock()
        .unwrap()
        .mark_completed("bg1", now_utc - chrono::Duration::minutes(5));
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(
        idle_cards("oc_sub"),
        0,
        "空闲期从 subagent 最后活动起算,未到点不发"
    );

    // ③ subagent 结束已超 ttl → 到点照发(观测存在不等于永不提醒)。
    {
        let mut st = state.lock().unwrap();
        *st = summon_core::subagent::SubagentState::new();
        st.promote_to_running(
            "bg2",
            "早已结束的",
            "tu_2",
            None,
            now_utc - chrono::Duration::seconds(ttl.as_secs() as i64 + 7200),
        );
        st.mark_completed(
            "bg2",
            now_utc - chrono::Duration::seconds(ttl.as_secs() as i64 + 3600),
        );
    }
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(
        idle_cards("oc_sub"),
        1,
        "subagent 结束后的空闲期到点要照常发卡"
    );
    assert!(chats["oc_sub"].idle_prompted);

    // ④ 冻结超窗的 Running(watcher 死了/卡死的残留形态):判活沿用 2h 时效轴 → 不算活跃,
    // 回退现行为照发 —— 一个死 watcher 不能把会话变成永不提醒/永不回收。
    let frozen: SharedSubagentState = Arc::new(std::sync::Mutex::new(
        summon_core::subagent::SubagentState::new(),
    ));
    frozen.lock().unwrap().promote_to_running(
        "bg3",
        "冻结的残留",
        "tu_3",
        None,
        now_utc - summon_core::subagent::DIR_SUBAGENT_STALE_AFTER - chrono::Duration::hours(24),
    );
    chats.insert("oc_frozen".into(), mk_handle(Some(frozen)));
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(
        idle_cards("oc_frozen"),
        1,
        "超时效窗的冻结 Running 不得再挡空闲卡"
    );

    // ⑤ 观测不可得(None,与既有全部会话同形)→ 现行为:照发。
    chats.insert("oc_none".into(), mk_handle(None));
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(idle_cards("oc_none"), 1, "无观测句柄回退现行为");
}

/// 长 turn(>idle_ttl)结束的那一刻,入站锚点早已过点;不重置的话空闲提醒会在 turn 结束后
/// 立刻补发(owner 2026-08-19 实测:12:58 回复、13:03 即提醒 55min)。`turn_was_active`
/// 在扫描间记「上轮有活跃 turn」,翻转(在跑→不在跑)时把空闲时钟拨到 turn 结束时刻;
/// 定时尾轮(`last_inbound_scheduled`)不重置,保持「从 timer fire 起算静默回收」的原行为。
#[test]
fn run_idle_scan_resets_idle_clock_when_long_turn_ends() {
    let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, _self_rx) = channel::<DispatchMsg>();
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    let ttl = messaging.idle_ttl();

    // 翻转检测在 first loop 的 `if present` 分支里 → 需要真活着的 tmux 会话(与生产同形:
    // transcript 会话的 turn 就在 tmux 里跑)。名字带 pid+纳秒防并行撞名;Drop 兜底 kill,
    // panic 也不泄漏。测试环境需有 tmux 二进制(与 go sidecar 测试同一先例)。
    let sess = format!(
        "soul-test-turn-end-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    struct TmuxGuard(crate::session::tmux::Tmux);
    impl Drop for TmuxGuard {
        fn drop(&mut self) {
            let _ = self.0.kill_session();
        }
    }
    let tmux = TmuxGuard(crate::session::tmux::Tmux::new(sess.clone()));
    tmux.0
        .new_session("/tmp", "sleep 1000000")
        .expect("起测试 tmux 会话(测试环境需装 tmux)");

    let mk_handle = |turn_was_active: bool, scheduled: bool| {
        let (tx, _rx) = channel::<ThreadMsg>();
        ChatHandle {
            tx,
            // 入站锚点早已过点(模拟 12:06 入站、turn 跑到 12:58 的形态)。
            last_activity: SystemTime::now() - ttl - Duration::from_secs(600),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: sess.clone(),
            tmux_seen_live: true,
            local_claimed: true,
            last_inbound_scheduled: scheduled,
            last_inbound_id: String::new(),
            turn_was_active,
            subagent_state: None,
            menu_ledger: None,
        }
    };
    let idle_cards = |chat_id: &str| {
        std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(chat_tag("p2p", chat_id))
                .join("outbound.ndjson"),
        )
        .map(|s| {
            s.lines()
                .filter(|l| l.contains(r#""kind":"idle_prompt_card""#))
                .count()
        })
        .unwrap_or(0)
    };
    let anchor_age = |chats: &HashMap<String, ChatHandle>, chat_id: &str| {
        chats[chat_id]
            .last_activity
            .elapsed()
            .unwrap_or(Duration::ZERO)
    };

    // ① 长 turn 刚结束(上轮扫描时还在跑)→ 时钟拨到结束时刻,本轮不发卡。
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert("oc_turn".into(), mk_handle(true, false));
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(idle_cards("oc_turn"), 0, "长 turn 结束后不得立刻补发空闲卡");
    assert!(
        !chats["oc_turn"].idle_prompted,
        "去重位不得被占用(未到点,不是漏发)"
    );
    assert!(
        anchor_age(&chats, "oc_turn") < Duration::from_secs(60),
        "空闲时钟必须拨到 turn 结束时刻(锚点被重置)"
    );
    assert!(!chats["oc_turn"].turn_was_active, "翻转后状态位落为 false");

    // ② 对照:没有「在跑→结束」翻转(如 daemon 重启后 turn_was_active 丢失)→ 旧行为:锚点过点照发卡。
    // 证明 ① 不是被别的判据挡掉的,而是翻转重置确实生效。
    let mut chats2: HashMap<String, ChatHandle> = HashMap::new();
    chats2.insert("oc_ctrl".into(), mk_handle(false, false));
    run_idle_scan(&mut chats2, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(
        idle_cards("oc_ctrl"),
        1,
        "无翻转观测时锚点照旧过点 → 发卡(旧行为兜底)"
    );

    // ③ 定时尾轮的 turn 结束不重置:保持「从 timer fire 起算静默回收」,锚点不动、回退照发兜底卡。
    let mut chats3: HashMap<String, ChatHandle> = HashMap::new();
    chats3.insert("oc_sched".into(), mk_handle(true, true));
    run_idle_scan(&mut chats3, &agent, &messaging, &self_tx, &fc, &migrated);
    assert!(
        anchor_age(&chats3, "oc_sched") > ttl,
        "定时尾轮不重置锚点(从 timer fire 起算,不从 turn 结束起算)"
    );
    assert_eq!(
        idle_cards("oc_sched"),
        1,
        "定时尾轮结构门槛证不了 → 回退发卡"
    );
}

/// `ChatHandle::subagent_probe` 的可得性契约:无句柄 → None;锁被占(watcher 正批量消费 /
/// 卡死在持锁段)→ None **且不阻塞**(dispatcher 主循环线程绝不能被一个 watcher 挂住);
/// 锁 poison(watcher panic)→ 恢复内层值照读。None 的消费方向 = 回退现行为(允许提醒/回收)。
#[test]
fn subagent_probe_is_nonblocking_and_poison_tolerant() {
    let now = chrono::Utc::now();
    let mk_handle = |st: Option<SharedSubagentState>| {
        let (tx, _rx) = channel::<ThreadMsg>();
        ChatHandle {
            tx,
            last_activity: SystemTime::now(),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: "soul-probe-test".into(),
            tmux_seen_live: false,
            local_claimed: false,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: st,
            menu_ledger: None,
        }
    };
    // 无句柄:观测不可得。
    assert!(mk_handle(None).subagent_probe(now).is_none());

    // 锁被占:必须立即返回 None,不许阻塞等锁。
    let state: SharedSubagentState = Arc::new(std::sync::Mutex::new(
        summon_core::subagent::SubagentState::new(),
    ));
    let h = mk_handle(Some(state.clone()));
    let guard = state.lock().unwrap();
    assert!(
        h.subagent_probe(now).is_none(),
        "争用必须回退,不得阻塞扫描线程"
    );
    drop(guard);
    assert!(h.subagent_probe(now).is_some(), "锁空闲时正常读出探针");

    // poison:恢复内层值继续(状态是纯观测副作用,读到什么算什么)。
    let poisoned: SharedSubagentState = Arc::new(std::sync::Mutex::new(
        summon_core::subagent::SubagentState::new(),
    ));
    poisoned
        .lock()
        .unwrap()
        .promote_to_running("a1", "在跑", "tu_1", None, now);
    let clone = poisoned.clone();
    let _ = std::thread::spawn(move || {
        let _g = clone.lock().unwrap();
        panic!("模拟 watcher panic 毒化锁");
    })
    .join();
    let probe = mk_handle(Some(poisoned))
        .subagent_probe(now)
        .expect("poison 必须恢复内层值,不得把扫描一起带崩");
    assert!(probe.active, "毒化前写入的 Running 仍读得出");
}

/// 终审回归①(2026-08-13):真人尾轮之后迟到的重复 fire(WAL 已 started → `AlreadyStarted`)
/// 绝不能把会话改判回定时尾轮。投递 ≠ 执行:ScheduledFire 臂只登记来源代际(fire_id),
/// 置定时尾轮的唯一路径是 worker 取得 `Marked` 后回报的 `ScheduledAdmitted` —— 而
/// `AlreadyStarted` 进不了 `run_after_durable_start` 的 run 闭包,admission 根本发不出来。
/// 变异自验:把投递点改回「直接置 true」(note_inbound_source 对 fire 置 true),或让
/// admission 绕过 Marked 门(闭包外无条件发),本测试必红。
#[test]
fn late_already_started_fire_never_reclassifies_human_tail() {
    let temp = tempfile::tempdir().unwrap();
    let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
    // fire 曾正常开跑:WAL 记录停在 started —— 这正是「迟到重复 fire」到达时的盘上事实。
    wal.append_scheduled(
        "starlight",
        "scope-a",
        "oc1",
        "p2p",
        "定时正文",
        summon_core::ScheduleOutput::Progress,
        summon_core::ScheduleContext::Resume,
        "sched-f1",
        100,
    )
    .unwrap();
    assert_eq!(
        wal.mark_started("starlight", "scope-a", "sched-f1")
            .unwrap(),
        crate::runtime::inbox_wal::MarkStartedOutcome::Marked
    );

    let (tx, _rx) = channel::<ThreadMsg>();
    let mut h = ChatHandle {
        tx,
        last_activity: SystemTime::now(),
        idle_prompted: false,
        chat_type: "p2p".into(),
        tmux_name: "soul-late-fire-test".into(),
        tmux_seen_live: false,
        local_claimed: true,
        last_inbound_scheduled: false,
        last_inbound_id: String::new(),
        turn_was_active: false,
        subagent_state: None,
        menu_ledger: None,
    };
    // 真人尾轮在后(Inbound 臂的写法):
    h.note_inbound_source("om_human");
    // 迟到的重复 fire 投递(ScheduledFire 臂的写法):只登记代际,不得置定时尾轮。
    h.note_inbound_source("sched-f1");
    assert!(!h.last_inbound_scheduled, "投递本身绝不能置定时尾轮");

    // worker 侧:mark_started 判 AlreadyStarted → run 闭包不执行 → admission 不发。
    let (admit_tx, admit_rx) = channel::<DispatchMsg>();
    let (outcome, ran) =
        run_after_durable_start(&wal, "starlight", "scope-a", "sched-f1", false, || {
            let _ = admit_tx.send(DispatchMsg::ScheduledAdmitted {
                chat_id: "oc1".into(),
                fire_id: "sched-f1".into(),
            });
        })
        .unwrap();
    assert_eq!(
        outcome,
        crate::runtime::inbox_wal::MarkStartedOutcome::AlreadyStarted
    );
    assert!(ran.is_none(), "AlreadyStarted 本轮不执行");
    assert!(
        admit_rx.try_recv().is_err(),
        "AlreadyStarted 不得回报 admission"
    );
    assert!(
        !h.last_inbound_scheduled,
        "最终保持真人尾轮(false),不被迟到 fire 改判回定时尾轮"
    );
}

/// 终审回归②(2026-08-13):ChatExited(旧 handle 已摘)→ 真人入站重建 handle →
/// 旧 worker 的 OrphanedTasks 迟到,队尾虽是 ScheduledPrompt,但已有 handle 的来源
/// (真人,写于旧 worker 死后)更新 —— 重投不得覆盖;该孤儿定时轮此后真的开跑并回报
/// admission,也要被 exact id 代际围栏拒掉,最终保持 false(普通空闲规则)。
/// 反向钉死特性不被「一刀切 false」阉割:孤儿重投**新建** handle(无真人插话)时,
/// 登记队尾 fire 为 pending,admission 命中后照常置定时尾轮。
/// 变异自验:去掉 `confirm_scheduled_admission` 的 id 围栏(无条件置 true)→ 第一段必红;
/// 去掉 `note_orphan_requeue_activity` 的 rebuilt 判据(对已有 handle 也覆盖来源)→ 第一段必红;
/// 把 admission 一刀切拒掉 → 第二段必红。
#[test]
fn scheduled_and_sentinel_turns_drop_the_react_ban_when_reactions_are_off() {
    let m = builtin_messaging_for_test();
    // 开着:那句消歧义在,因为 PRIMER 确实教了「每一次回复都必须 react」。
    let on = super::scheduled_chat_task(
        "sched-on".to_string(),
        "跑日报",
        false,
        m.clone(),
        summon_core::ScheduleOutput::Progress,
        summon_core::ScheduleContext::Resume,
        true,
    );
    assert!(on.text.contains("summon react"));

    // 关掉:PRIMER 没教 react,这句话就从「消歧义」变成「凭空引入一个 agent 不知道的命令」。
    let off = super::scheduled_chat_task(
        "sched-off".to_string(),
        "跑日报",
        false,
        m.clone(),
        summon_core::ScheduleOutput::Progress,
        summon_core::ScheduleContext::Resume,
        false,
    );
    assert!(!off.text.contains("react"), "{}", off.text);
    assert!(
        off.text.contains("跑日报"),
        "任务正文一字不能少:{}",
        off.text
    );

    // sentinel 走的是另一条构造路径,同一条规矩要各自钉住 —— 它没有别的测试覆盖。
    let sentinel_on = super::sentinel_chat_task(
        "sent-on".to_string(),
        "看一下",
        None,
        true,
        "张三".to_string(),
        "ou_1".to_string(),
        String::new(),
        m.clone(),
        true,
    );
    assert!(sentinel_on.text.contains("summon react"));
    let sentinel_off = super::sentinel_chat_task(
        "sent-off".to_string(),
        "看一下",
        None,
        true,
        "张三".to_string(),
        "ou_1".to_string(),
        String::new(),
        m,
        false,
    );
    assert!(
        !sentinel_off.text.contains("react"),
        "{}",
        sentinel_off.text
    );
    assert!(sentinel_off.text.contains("看一下"));
}

/// 上下文提醒卡的 Handoff 经代投通道进来,chat 线程拿到的 task 与 `/handoff <备注>`(不带目标)的
/// `handoff` 字段一致 —— 轮末交接、拆除、不并进别的轮,全靠这个字段。
#[test]
fn sentinel_task_from_a_context_card_handoff_is_a_handoff_turn() {
    let task = super::sentinel_chat_task(
        "ctx-handoff-om_card".to_string(),
        &summon_core::handoff::instruction(
            summon_core::handoff::HandoffAfter::NewSession,
            "先修登录",
        ),
        Some("先修登录".to_string()),
        true,
        "张三".to_string(),
        "ou_1".to_string(),
        "on_1".to_string(),
        builtin_messaging_for_test(),
        true,
    );
    assert_eq!(
        task.handoff,
        Some(summon_core::handoff::HandoffCommand {
            target: None,
            note: "先修登录".into(),
        })
    );
    assert_eq!(task.kind, ChatTaskKind::Conversation);
    assert!(task.hide_context_hint);
    assert_eq!(task.feishu_anchor(), None);

    let plain = super::sentinel_chat_task(
        "sent-1".to_string(),
        "看一下",
        None,
        true,
        "张三".to_string(),
        "ou_1".to_string(),
        String::new(),
        builtin_messaging_for_test(),
        true,
    );
    assert_eq!(plain.handoff, None, "普通代投消息不是交接轮");
}

#[test]
fn orphan_requeue_never_overwrites_newer_human_source() {
    let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
    let m = builtin_messaging_for_test();
    let sched = scheduled_chat_task(
        "sched-f1".to_string(),
        "跑日报",
        false,
        m,
        summon_core::ScheduleOutput::Progress,
        summon_core::ScheduleContext::Resume,
        true,
    );
    let mk_handle = || {
        let (tx, _rx) = channel::<ThreadMsg>();
        ChatHandle {
            tx,
            last_activity: SystemTime::now(),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: "soul-orphan-test".into(),
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        }
    };

    // ① 命中已有 handle(真人尾轮更新)→ 来源不覆盖;迟到 admission 被代际围栏拒掉。
    let mut human = mk_handle();
    human.note_inbound_source("om_human"); // ChatExited 之后的真人入站(重建 handle)
    note_orphan_requeue_activity(
        &mut human,
        &agent,
        "oc1",
        false,
        std::slice::from_ref(&sched),
    );
    assert_eq!(
        human.last_inbound_id, "om_human",
        "孤儿重投不得覆盖更新的真人来源"
    );
    assert!(!human.last_inbound_scheduled);
    assert!(
        !human.confirm_scheduled_admission("sched-f1"),
        "孤儿定时轮的迟到 admission 必须被代际围栏拒掉"
    );
    assert!(
        !human.last_inbound_scheduled,
        "最终保持 false:真人会话不得被改判定时尾轮"
    );

    // ② 孤儿重投新建 handle(队尾就是最后入站)→ 登记 pending;admission 命中才置定时尾轮。
    let mut rebuilt = mk_handle();
    note_orphan_requeue_activity(
        &mut rebuilt,
        &agent,
        "oc1",
        true,
        std::slice::from_ref(&sched),
    );
    assert_eq!(rebuilt.last_inbound_id, "sched-f1");
    assert!(
        !rebuilt.last_inbound_scheduled,
        "登记只是 pending,开跑前不判定时尾轮"
    );
    assert!(rebuilt.confirm_scheduled_admission("sched-f1"));
    assert!(
        rebuilt.last_inbound_scheduled,
        "孤儿定时轮真的开跑后,静默回收照常接管"
    );
}

/// 终审阻塞②回归(2026-08-13):到期回收意图(Control::Clean)投递失败 —— receiver 已断
/// (chat 线程已退、ChatExited 尚未收口)—— 必须撤销该 chat 本轮的静默托管,让同轮扫描
/// 后段的提醒卡循环按原 TTL 兜底;否则门槛全真、Clean 丢失、卡又被压,会话落进
/// 「既不回收也不发卡」的悬空态,违反「证不了就回退发卡」的声明。
/// 变异自验:去掉 `dispatch_retire_intents` 的撤销分支(恢复 `let _ = h.tx.send(..)`)→ 第一段必红。
#[test]
fn retire_intent_send_failure_revokes_timer_quiet_ownership() {
    let agent: Agent = serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    let mk_handle = |tx: Sender<ThreadMsg>| ChatHandle {
        tx,
        last_activity: SystemTime::now(),
        idle_prompted: false,
        chat_type: "p2p".into(),
        tmux_name: "soul-retire-send-test".into(),
        tmux_seen_live: true,
        local_claimed: true,
        last_inbound_scheduled: true,
        last_inbound_id: "sched-f1".into(),
        turn_was_active: false,
        subagent_state: None,
        menu_ledger: None,
    };

    // receiver 已断:send 必失败 → 本轮 quiet 托管必须撤销。
    let (dead_tx, dead_rx) = channel::<ThreadMsg>();
    drop(dead_rx);
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert("c1".into(), mk_handle(dead_tx));
    let mut quiet = std::collections::HashSet::from(["c1".to_string()]);
    dispatch_retire_intents(
        &chats,
        &agent,
        vec![("c1".into(), 3300, true, false)],
        &mut quiet,
    );
    assert!(
        quiet.is_empty(),
        "Clean 投递失败必须撤销本轮静默托管(回退提醒卡兜底)"
    );

    // 对照:receiver 活着 → 意图送达(idle_retired 语义完整),托管保持(本轮不发卡)。
    let (live_tx, live_rx) = channel::<ThreadMsg>();
    let mut chats2: HashMap<String, ChatHandle> = HashMap::new();
    chats2.insert("c2".into(), mk_handle(live_tx));
    let mut quiet2 = std::collections::HashSet::from(["c2".to_string()]);
    dispatch_retire_intents(
        &chats2,
        &agent,
        vec![("c2".into(), 3300, true, false)],
        &mut quiet2,
    );
    assert!(quiet2.contains("c2"), "投递成功则托管保持,本轮不发提醒卡");
    match live_rx.try_recv() {
        Ok(ThreadMsg::Control {
            ctrl: Control::Clean,
            notify_exit,
            idle_retired,
            ..
        }) => {
            assert!(
                !notify_exit,
                "定时尾轮要**完全**安静地走:连退出回执都不发(owner 2026-08-16)"
            );
            assert!(idle_retired, "回收意图必须带 idle_retired(归档续聊文案)");
        }
        _ => panic!("应投 Control::Clean 回收意图"),
    }
}

/// **「安静地走」只对定时尾轮成立,普通会话照旧要发退出回执。**
///
/// 需求(owner 2026-08-16)是「定时任务 55 分钟自动归档时别再往群里推卡」。原实现只压掉了
/// 到点的**空闲提醒卡**(`timer_quiet_owns_idle`),真回收时那张「📦 会话已归档」照发 ——
/// 对一条没人跟进的定时会话来说,那正是它唯一还会在群里冒的泡。
///
/// 反向那半同等重要:**人聊出来的会话被自动回收时必须留话**。那是用户没预期的终止,
/// 静默拆掉等于人回来发现上下文没了却不知道发生过什么。判据只能是「尾轮是不是定时器合成的」。
#[test]
fn only_a_timer_tail_retires_silently_a_human_session_still_gets_its_receipt() {
    let agent: Agent = serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    let mk_handle = |tx: Sender<ThreadMsg>, scheduled: bool| ChatHandle {
        tx,
        last_activity: SystemTime::now(),
        idle_prompted: false,
        chat_type: "p2p".into(),
        tmux_name: "soul-retire-quiet-test".into(),
        tmux_seen_live: true,
        local_claimed: true,
        last_inbound_scheduled: scheduled,
        last_inbound_id: "m1".into(),
        turn_was_active: false,
        subagent_state: None,
        menu_ledger: None,
    };

    for (timer_tail, want_notify, why) in [
        (true, false, "定时尾轮:静默归档,不发回执"),
        (
            false,
            true,
            "人类尾轮:自动回收必须发回执,否则用户回来发现会话没了却不知为何",
        ),
    ] {
        let (tx, rx) = channel::<ThreadMsg>();
        let mut chats: HashMap<String, ChatHandle> = HashMap::new();
        chats.insert("c1".into(), mk_handle(tx, timer_tail));
        let mut quiet = std::collections::HashSet::new();
        dispatch_retire_intents(
            &chats,
            &agent,
            vec![("c1".into(), 3300, timer_tail, false)],
            &mut quiet,
        );
        match rx.try_recv() {
            Ok(ThreadMsg::Control {
                ctrl: Control::Clean,
                notify_exit,
                idle_retired,
                ..
            }) => {
                assert_eq!(notify_exit, want_notify, "{why}");
                // 拆除语义不因静默而变:两种都仍是「归档续聊」。
                assert!(idle_retired, "{why}:仍必须是归档续聊,不是重置");
            }
            _ => panic!("应投 Control::Clean 回收意图({why})"),
        }
    }
}

/// 定时尾轮开了重置 → 到点走 Reset(清 resume 记录);没开 / 人类尾轮 → 照旧 Clean。
#[test]
fn timer_tail_with_reset_routes_to_reset_otherwise_clean() {
    let agent: Agent = serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    let mk_handle = |tx: Sender<ThreadMsg>, scheduled: bool| ChatHandle {
        tx,
        last_activity: SystemTime::now(),
        idle_prompted: false,
        chat_type: "p2p".into(),
        tmux_name: "soul-timer-reset-test".into(),
        tmux_seen_live: true,
        local_claimed: true,
        last_inbound_scheduled: scheduled,
        last_inbound_id: "m1".into(),
        turn_was_active: false,
        subagent_state: None,
        menu_ledger: None,
    };

    for (timer_tail, timer_reset, want_reset, why) in [
        (true, true, true, "定时尾轮 + 开重置 → Reset"),
        (true, false, false, "定时尾轮 + 没开 → Clean(归档续聊)"),
        (false, true, false, "人类尾轮 → 永远 Clean,重置位无效"),
    ] {
        let (tx, rx) = channel::<ThreadMsg>();
        let mut chats: HashMap<String, ChatHandle> = HashMap::new();
        chats.insert("c1".into(), mk_handle(tx, timer_tail));
        let mut quiet = std::collections::HashSet::new();
        dispatch_retire_intents(
            &chats,
            &agent,
            vec![("c1".into(), 3300, timer_tail, timer_reset)],
            &mut quiet,
        );
        match rx.try_recv() {
            Ok(ThreadMsg::Control {
                ctrl,
                notify_exit,
                idle_retired,
                ..
            }) => {
                if want_reset {
                    assert!(matches!(ctrl, Control::Reset), "{why}");
                } else {
                    assert!(matches!(ctrl, Control::Clean), "{why}");
                }
                // 定时尾轮静默走(不发回执);人类尾轮照发。
                assert_eq!(notify_exit, !timer_tail, "{why}");
                assert!(idle_retired, "{why}:自动退出必须带 idle_retired");
            }
            _ => panic!("应投回收意图({why})"),
        }
    }
}

#[test]
fn run_idle_scan_does_not_resend_idle_card_after_restart() {
    // 重启风暴回归(owner 2026-08-13 两次部署实测):b81bdd8 把空闲时钟从盘上恢复之后,
    // 「本段空闲期已发过提醒卡」却只活在内存 —— 重启重建 handle 时 idle_prompted 归零,
    // 第一轮扫描看到空闲 ≥ ttl 就给每个空闲会话再发一张。账本 = durable marker(先写后发,
    // 终审阻塞①的收口;outbound 归档是 best-effort 观测日志,当不了账本)。本测试按
    // spawn_chat_thread 的同一双恢复函数(read_chat_last_activity +
    // read_idle_prompt_marker_since)重建扫描态,钉死三件事:
    // ① 盘上有本段空闲期的 marker → 重启后不重发;
    // ② 无 marker 的空闲会话照常发,发送前 marker 已落盘,再次重启不重发(风暴止于第一张);
    // ③ 新活动重置空闲期后,下一段空闲期照常发卡(跨重启)。
    let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, _self_rx) = channel::<DispatchMsg>();
    let ttl = messaging.idle_ttl().as_secs() as i64;
    let now = chrono::Local::now();
    // 迁移证明在**空目录**上先取好:本测试的 chat 全部诞生于新机制之下(marker 稳态),
    // 升级窗口(老版本发过卡、无 marker)的行为由 upgrade_first_boot 迁移测试单独钉。
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();

    // 盘上布出「上一代 daemon 已发过卡」的现场:活动 → 空闲一个 ttl → 卡(marker)→ 空闲至今。
    let card_at = now - chrono::Duration::seconds(2 * ttl + 200);
    let active_at = card_at - chrono::Duration::seconds(ttl + 60);
    let session = "soul-nope-idle-dedup";
    let write_meta = |session: &str, chat_id: &str, at: chrono::DateTime<chrono::Local>| {
        let path = chat_meta_path(&agent, session);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "chat_id={chat_id}\nchat_type=p2p\nupdated_at={}\n",
                at.format("%Y-%m-%d %H:%M:%S")
            ),
        )
        .unwrap();
    };
    write_meta(session, "oc_idle", active_at);
    write_idle_prompt_marker(
        &agent,
        "p2p",
        "oc_idle",
        SystemTime::from(active_at),
        SystemTime::from(card_at),
    )
    .unwrap();
    let idle_cards_for = |chat_id: &str| {
        std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(chat_tag("p2p", chat_id))
                .join("outbound.ndjson"),
        )
        .map(|s| {
            s.lines()
                .filter(|l| l.contains(r#""kind":"idle_prompt_card""#))
                .count()
        })
        .unwrap_or(0)
    };

    // 「重启」= 按 spawn_chat_thread 的恢复逻辑重建 handle。
    let restore_handle = |chat_id: &str, tmux_name: &str| {
        let last = read_chat_last_activity(&agent, tmux_name).unwrap_or_else(SystemTime::now);
        let (tx, _rx) = channel::<ThreadMsg>();
        ChatHandle {
            tx,
            last_activity: last,
            idle_prompted: read_idle_prompt_marker_since(&agent, "p2p", chat_id, last),
            chat_type: "p2p".into(),
            tmux_name: tmux_name.into(), // 不存在 → has_session=false,不依赖 tmux
            tmux_seen_live: false,
            local_claimed: false,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        }
    };

    // ① 重启后扫描:空闲已远超 ttl,但本段空闲期的 marker 在盘上 → 不得重发。
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert("oc_idle".into(), restore_handle("oc_idle", session));
    assert!(
        chats["oc_idle"].idle_prompted,
        "恢复的扫描态必须认出「本段空闲期已发过卡」"
    );
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(
        idle_cards_for("oc_idle"),
        0,
        "重启后第一轮扫描不得再发空闲卡"
    );

    // ② 无 marker 的空闲会话(新机制下诞生、确实从未提醒过的):照常发第一张,且发送前
    // marker 已落盘;随后「重启」恢复出「已提醒」→ 不重发。重启风暴止于第一张。
    // (老版本发过卡、无 marker 的存量会话不走这条 —— 升级迁移已在任何扫描前补写,
    //  见 upgrade_first_boot_migration_suppresses_legacy_idle_period。)
    write_meta("soul-nope-idle-dedup2", "oc_first", active_at);
    let first_handle = restore_handle("oc_first", "soul-nope-idle-dedup2");
    assert!(
        !first_handle.idle_prompted,
        "无 marker = 本机制从未尝试过 → 恢复成「未提醒」,照常发"
    );
    chats.insert("oc_first".into(), first_handle);
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(idle_cards_for("oc_first"), 1, "第一张照常发,功能不静默死掉");
    assert!(
        idle_prompt_marker_path(&agent, "p2p", "oc_first").is_file(),
        "发送前 marker 必须已落盘(先写后发)"
    );
    chats.insert(
        "oc_first".into(),
        restore_handle("oc_first", "soul-nope-idle-dedup2"),
    );
    assert!(chats["oc_first"].idle_prompted, "marker 使去重跨重启成立");
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(
        idle_cards_for("oc_first"),
        1,
        "再次重启后不得重发(风暴止于第一张)"
    );

    // ③ 卡之后用户又说过话(updated_at 越过 marker 的 attempted_at),再度空闲到点,
    // 又一次重启:照常发。
    write_meta(
        session,
        "oc_idle",
        now - chrono::Duration::seconds(ttl + 10),
    );
    chats.insert("oc_idle".into(), restore_handle("oc_idle", session));
    assert!(
        !chats["oc_idle"].idle_prompted,
        "新活动重置空闲期后,恢复态不得再判「已发过」"
    );
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(idle_cards_for("oc_idle"), 1, "新一段空闲期到点要照常发卡");
    assert!(chats["oc_idle"].idle_prompted);
}

/// 升级窗口回归(终审阻塞点名向量,2026-08-13):旧 `.meta` + 无 marker + 已过 TTL 的存量
/// 会话 —— 部署新版本本身就重启 daemon、boot 恢复旧会话,若砍掉迁移,「marker 缺失判可发」
/// 会让升级首启的第一轮扫描给它再发一张,完整复演 owner 报告的事故。钉死:
/// ① 升级首启(迁移之后)**0 发送**;② 之后新入站、再次到期,**恰好 1 张**(迁移只压当段)。
/// 变异自验:把 `ensure_idle_prompt_marker_migration` 的补写枚举去掉(空迁移直接返回证明)
/// → 断言①必红。
#[test]
fn upgrade_first_boot_migration_suppresses_legacy_idle_period() {
    let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, _self_rx) = channel::<DispatchMsg>();
    let ttl = messaging.idle_ttl().as_secs() as i64;
    let now = chrono::Local::now();

    // 老版本留下的现场:只有 `.meta`(活动远早于一个 ttl 之前),没有任何 marker。
    let session = "soul-upgrade-legacy";
    let meta = chat_meta_path(&agent, session);
    std::fs::create_dir_all(meta.parent().unwrap()).unwrap();
    std::fs::write(
        &meta,
        format!(
            "chat_id=oc_legacy\nchat_type=p2p\nupdated_at={}\n",
            (now - chrono::Duration::seconds(2 * ttl + 200)).format("%Y-%m-%d %H:%M:%S")
        ),
    )
    .unwrap();

    // 升级首启:迁移在任何扫描之前跑(生产顺序由源码守卫钉死,这里按同一顺序编排)。
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    assert!(
        idle_prompt_marker_path(&agent, "p2p", "oc_legacy").is_file(),
        "迁移必须为升级前已有的 chat 补写「已提醒」marker"
    );

    // boot 恢复(与 spawn_chat_thread 同一对恢复函数)→ 当段空闲期被判「已提醒」。
    let last = read_chat_last_activity(&agent, session).unwrap();
    let (tx, _rx) = channel::<ThreadMsg>();
    let h = ChatHandle {
        tx,
        last_activity: last,
        idle_prompted: read_idle_prompt_marker_since(&agent, "p2p", "oc_legacy", last),
        chat_type: "p2p".into(),
        tmux_name: session.into(), // 不存在 → has_session=false,不依赖 tmux
        tmux_seen_live: false,
        local_claimed: false,
        last_inbound_scheduled: false,
        last_inbound_id: String::new(),
        turn_was_active: false,
        subagent_state: None,
        menu_ledger: None,
    };
    assert!(
        h.idle_prompted,
        "迁移补写的 marker 必须把当段空闲期判成已提醒"
    );
    let idle_cards = || {
        std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(chat_tag("p2p", "oc_legacy"))
                .join("outbound.ndjson"),
        )
        .map(|s| {
            s.lines()
                .filter(|l| l.contains(r#""kind":"idle_prompt_card""#))
                .count()
        })
        .unwrap_or(0)
    };
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert("oc_legacy".into(), h);
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(idle_cards(), 0, "升级首启对存量已到点会话必须 0 发送");

    // 新入站(统一收口)作废迁移 marker → 新一段空闲期;再次到期恰好发 1 张。
    chats
        .get_mut("oc_legacy")
        .unwrap()
        .note_activity(&agent, "oc_legacy", Some("om_new_inbound"));
    assert!(
        !idle_prompt_marker_path(&agent, "p2p", "oc_legacy").exists(),
        "新入站必须作废迁移补写的 marker(迁移只压当段)"
    );
    chats.get_mut("oc_legacy").unwrap().last_activity =
        SystemTime::now() - Duration::from_secs((ttl + 10) as u64);
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert_eq!(idle_cards(), 1, "下一段空闲期到点要恰好发 1 张,不多不少");
}

/// 迁移与扫描/boot 恢复的顺序结构保证(终审处方:不靠调用顺序默契):
/// ① 「迁移未完成不进扫描」在**类型层**钉死 —— `run_idle_scan` 的签名要求
///    `&IdleMarkersMigrated`,其唯一构造点是 `ensure_idle_prompt_marker_migration` 成功
///    返回(字段私有,本测试也造不出来,只能走真迁移)。这里守住签名不被退化。
/// ② 「迁移先于两个 boot 恢复扫描」是源码顺序守卫:恢复出的 handle 在 spawn 时读 marker
///    恢复 idle_prompted,迁移若晚于 boot 恢复,存量会话仍会被恢复成「未提醒」——
///    这半边类型证明管不到,文本钉死调用点次序。
#[test]
fn idle_marker_migration_precedes_boot_restore_and_gates_scan() {
    let dispatcher = include_str!("dispatcher.rs");
    let sig = dispatcher
        .find("pub(super) fn run_idle_scan(")
        .expect("run_idle_scan 必须存在");
    let tail = &dispatcher[sig..];
    let body = tail.find('{').expect("fn 必有函数体");
    assert!(
        tail[..body].contains("_migrated: &IdleMarkersMigrated"),
        "run_idle_scan 必须以 IdleMarkersMigrated 类型证明为必填参数(迁移未完成不进扫描)"
    );
    let migrate_at = dispatcher
        .find("ensure_idle_prompt_marker_migration(&agent)")
        .expect("dispatcher 启动必须跑空闲卡账本迁移");
    let pending_at = dispatcher
        .find("in pending_chats_on_disk(&agent")
        .expect("boot 启动续跑扫描应存在");
    let mirror_at = dispatcher
        .find("in mirror_behind_chats_on_disk(&agent)")
        .expect("boot 镜像预热扫描应存在");
    assert!(
        migrate_at < pending_at && migrate_at < mirror_at,
        "迁移必须先于两个 boot 恢复扫描(spawn 从 marker 账本恢复 idle_prompted)"
    );
}

#[test]
fn idle_card_marker_write_failure_blocks_send() {
    // 账本先行的 fail-closed 半边(终审阻塞①):durable marker 落不了盘就**不发**——
    // 漏一张、下轮扫描重试,绝不「先发再记」(发送成功 → 记账失败/崩溃 → 重启误判「没发过」
    // 而重发,正是 outbound 归档当账本被终审否掉的失败形状)。
    // 构造:marker 路径被一个目录占住 → 原子写的 rename 必败。
    let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, _self_rx) = channel::<DispatchMsg>();
    let stale =
        std::time::SystemTime::now() - Duration::from_secs(messaging.idle_ttl().as_secs() + 10);
    let (tx, _rx) = channel::<ThreadMsg>();
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert(
        "oc_x".into(),
        ChatHandle {
            tx,
            last_activity: stale,
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: "soul-nope-marker-fail".into(), // 不存在 → 不依赖 tmux
            tmux_seen_live: false,
            local_claimed: false,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    );
    let marker = idle_prompt_marker_path(&agent, "p2p", "oc_x");
    std::fs::create_dir_all(&marker).unwrap(); // 目录占住 marker 路径
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    let outbound = agent
        .state_dir()
        .join("threads")
        .join(chat_tag("p2p", "oc_x"))
        .join("outbound.ndjson");
    assert!(!outbound.exists(), "marker 写失败必须拦下发送(先写后发)");
    assert!(
        !chats["oc_x"].idle_prompted,
        "写失败不置位:磁盘恢复后下轮照常补发"
    );
    // 磁盘「恢复」:让出 marker 路径 → 下一轮扫描补发,功能不因 fail-closed 静默死掉。
    std::fs::remove_dir(&marker).unwrap();
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert!(outbound.exists(), "恢复后下一轮应补发空闲卡");
    assert!(marker.is_file(), "补发时 marker 已先行落盘");
    assert!(chats["oc_x"].idle_prompted);
}

/// 终审阻塞③回归(replacement 半边,2026-08-13):Inbound 投递失败 → 同轮重建的 handle 从
/// 盘上账本恢复(idle_prompted 可能为 true、时钟为旧 `.meta`、来源代际空串),但它接住的是
/// **这条**新入站任务 —— Replacement 臂必须给新 handle 补上完整活动收口
/// (`ChatHandle::note_activity`)。修复前:真人任务进来了,去重位却还是恢复出的 true,
/// 下段空闲期不发提醒卡、timer 托管撤销后也无兜底卡;来源代际空串使 admission 围栏失去参照。
/// 变异自验:把 Replacement 臂的 note_activity 去掉,或把 note_activity 里的去重位清除去掉,
/// 本测试必红。
#[test]
fn replacement_worker_gets_full_inbound_activity_treatment() {
    let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
    let now = chrono::Local::now();
    let session = "soul-replacement-dedup";
    let active_at = now - chrono::Duration::seconds(7200);
    let meta = chat_meta_path(&agent, session);
    std::fs::create_dir_all(meta.parent().unwrap()).unwrap();
    std::fs::write(
        &meta,
        format!(
            "chat_id=oc_x\nchat_type=p2p\nupdated_at={}\n",
            active_at.format("%Y-%m-%d %H:%M:%S")
        ),
    )
    .unwrap();
    write_idle_prompt_marker(
        &agent,
        "p2p",
        "oc_x",
        SystemTime::from(active_at),
        SystemTime::from(now - chrono::Duration::seconds(3600)),
    )
    .unwrap();

    // 「重建」= 按 spawn_chat_thread 的恢复逻辑还原 handle(账本恢复出 true)。
    let last = read_chat_last_activity(&agent, session).unwrap();
    let (tx, _rx) = channel::<ThreadMsg>();
    let mut replacement = ChatHandle {
        tx,
        last_activity: last,
        idle_prompted: read_idle_prompt_marker_since(&agent, "p2p", "oc_x", last),
        chat_type: "p2p".into(),
        tmux_name: session.into(),
        tmux_seen_live: false,
        local_claimed: true,
        last_inbound_scheduled: false,
        last_inbound_id: String::new(),
        turn_was_active: false,
        subagent_state: None,
        menu_ledger: None,
    };
    assert!(replacement.idle_prompted, "前提:账本恢复出「已提醒」");

    // Replacement 臂的活动收口(TaskDelivery::Replacement)。
    replacement.note_activity(&agent, "oc_x", Some("om_human"));
    assert!(
        !replacement.idle_prompted,
        "replacement 收到真人任务后必须回到「未提醒」"
    );
    assert_eq!(
        replacement.last_inbound_id, "om_human",
        "来源代际必须登记本条新入站"
    );
    assert!(!replacement.last_inbound_scheduled);
    assert!(
        !idle_prompt_marker_path(&agent, "p2p", "oc_x").exists(),
        "durable marker 一并作废(新一段空闲期从头计)"
    );
    assert!(
        replacement.last_activity.elapsed().unwrap() < Duration::from_secs(5),
        "空闲时钟推进到「此刻」"
    );
}

/// 终审阻塞③复合回归(2026-08-13):spawn 从账本恢复 `idle_prompted=true` → 孤儿
/// scheduled task 重投(修复前只推时钟、不清去重位)→ admission 确认成定时尾轮 →
/// 到点回收但 `Control::Clean` 投递失败(receiver 已断)→ `timer_quiet` 托管撤销 →
/// 兜底提醒卡**必须发得出去**。修复前:残留的 idle_prompted=true 把兜底卡也挡住,
/// 会话落进「既不回收也不发卡」的悬空态。
/// 变异自验:把 `note_orphan_requeue_activity` 退回「只推时钟 + 登来源」(不走
/// note_activity 的去重清除)→ 本测试必红(第一断言与末尾兜底卡断言同时红)。
#[test]
fn orphan_requeue_clears_restored_dedup_so_fallback_card_survives_clean_failure() {
    let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, _self_rx) = channel::<DispatchMsg>();
    let now = chrono::Local::now();
    let ttl = messaging.idle_ttl().as_secs() as i64;
    let session = "soul-orphan-dedup-fallback";
    let active_at = now - chrono::Duration::seconds(2 * ttl + 200);
    let meta = chat_meta_path(&agent, session);
    std::fs::create_dir_all(meta.parent().unwrap()).unwrap();
    std::fs::write(
        &meta,
        format!(
            "chat_id=oc_x\nchat_type=p2p\nupdated_at={}\n",
            active_at.format("%Y-%m-%d %H:%M:%S")
        ),
    )
    .unwrap();
    write_idle_prompt_marker(
        &agent,
        "p2p",
        "oc_x",
        SystemTime::from(active_at),
        SystemTime::from(now - chrono::Duration::seconds(ttl + 100)),
    )
    .unwrap();

    // 「重启」恢复(与 spawn_chat_thread 同一对函数):账本恢复出 true。
    let last = read_chat_last_activity(&agent, session).unwrap();
    let (dead_tx, dead_rx) = channel::<ThreadMsg>();
    let mut h = ChatHandle {
        tx: dead_tx,
        last_activity: last,
        idle_prompted: read_idle_prompt_marker_since(&agent, "p2p", "oc_x", last),
        chat_type: "p2p".into(),
        tmux_name: session.into(), // 不存在 → has_session=false,不依赖 tmux
        tmux_seen_live: false,
        local_claimed: true,
        last_inbound_scheduled: false,
        last_inbound_id: String::new(),
        turn_was_active: false,
        subagent_state: None,
        menu_ledger: None,
    };
    assert!(h.idle_prompted, "前提:账本恢复出「已提醒」");

    // 孤儿 scheduled task 重投(OrphanedTasks 臂,rebuilt=true):活动收口必须清去重位。
    let sched = scheduled_chat_task(
        "sched-f9".to_string(),
        "跑日报",
        false,
        builtin_messaging_for_test(),
        summon_core::ScheduleOutput::Progress,
        summon_core::ScheduleContext::Resume,
        true,
    );
    note_orphan_requeue_activity(&mut h, &agent, "oc_x", true, std::slice::from_ref(&sched));
    assert!(
        !h.idle_prompted,
        "孤儿重投是真实活动,恢复出的去重位必须清掉(修复点)"
    );
    assert!(
        !idle_prompt_marker_path(&agent, "p2p", "oc_x").exists(),
        "durable marker 一并作废"
    );
    assert_eq!(h.last_inbound_id, "sched-f9");
    // worker 取得 durable start 后确认定时尾轮。
    assert!(h.confirm_scheduled_admission("sched-f9"));

    // 定时窗口到点(模拟时间流逝:把时钟拨回陈旧值);receiver 已断 → Clean 必然投不出去。
    h.last_activity = SystemTime::from(now - chrono::Duration::seconds(ttl + 10));
    drop(dead_rx);
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert("oc_x".into(), h);
    let mut quiet = std::collections::HashSet::from(["oc_x".to_string()]);
    dispatch_retire_intents(
        &chats,
        &agent,
        vec![("oc_x".into(), 3300, true, false)],
        &mut quiet,
    );
    assert!(quiet.is_empty(), "Clean 投递失败必须撤销静默托管");

    // 同轮后段的提醒卡兜底(run_idle_scan 尾段;tmux 不在 → 回收/托管都不可达):
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    let idle_cards = std::fs::read_to_string(
        agent
            .state_dir()
            .join("threads")
            .join(chat_tag("p2p", "oc_x"))
            .join("outbound.ndjson"),
    )
    .map(|s| {
        s.lines()
            .filter(|l| l.contains(r#""kind":"idle_prompt_card""#))
            .count()
    })
    .unwrap_or(0);
    assert_eq!(
        idle_cards, 1,
        "兜底提醒卡必须发得出去(修复前被残留 idle_prompted=true 挡住)"
    );
    assert!(chats["oc_x"].idle_prompted);
}

/// 终审阻塞③的结构守卫:入站活动三件套(空闲时钟 / 空闲卡去重含 durable marker / 来源代际)
/// 唯一合法入口是 `ChatHandle::note_activity`(mod.rs)。dispatcher/cards 再出现散装拨弄
/// (单推时钟、单清去重位、单登来源)= 又开出一条能漏一半的路 —— replacement 与孤儿重投
/// 两处翻车都是这个形状(推了时钟/来源,漏了去重位)。
#[test]
fn inbound_activity_sites_route_through_note_activity_only() {
    let dispatcher = include_str!("dispatcher.rs");
    let cards = include_str!("cards.rs");
    for (name, src) in [("dispatcher.rs", dispatcher), ("cards.rs", cards)] {
        for pat in [
            ".note_inbound_source(",
            ".idle_prompted = false",
            ".last_activity = SystemTime::now()",
        ] {
            assert_eq!(
                src.matches(pat).count(),
                0,
                "{name} 不得绕过 note_activity 直接写 `{pat}`"
            );
        }
    }
    assert!(
        dispatcher.matches(".note_activity(").count() >= 5,
        "dispatcher 的入站臂(Inbound/replacement/孤儿重投/ScheduledFire/SentinelFire)应全部走统一收口"
    );
    assert_eq!(
        cards.matches(".note_activity(").count(),
        1,
        "保活卡应走统一收口"
    );
    // spawn 恢复接线(变异自验发现:行为测试按同一对恢复函数重建 handle,摸不到 spawn 内部,
    // 把恢复行换成常量测试照样绿)——账本恢复必须真的接在 spawn_chat_thread 上。
    assert!(
        dispatcher
            .contains("read_idle_prompt_marker_since(&agent, &chat_type, &chat_id, last_activity)"),
        "spawn_chat_thread 必须从 durable marker 账本恢复 idle_prompted"
    );
}

#[test]
fn run_idle_scan_silently_exits_and_reports_externally_killed_transcript() {
    // PF1 + F3-b:Transcript 会话曾观测 live、tmux 又消失(外带 agents kill)→ ① 投 SilentExit 让残留
    // chat 线程退出(否则它持自己 sender 的 clone,rx 永不 Disconnected,线程+mirror watcher 永久泄漏);
    // ② 合成 ManualKill ChatExited 回执。tmux_name 用不存在的会话名(has_session=false),不依赖 tmux。
    let tmp = std::env::temp_dir().join(format!("summon-extkill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent =
        serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, self_rx) = channel::<DispatchMsg>();
    let (tx, rx) = channel::<ThreadMsg>();
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert(
        "oc1".into(),
        ChatHandle {
            tx,
            last_activity: std::time::SystemTime::now(),
            idle_prompted: false,
            chat_type: "p2p".into(),
            // 不存在的会话名 → has_session=false(tmux 缺失也返回 false,故不依赖 tmux)。
            tmux_name: format!("soul-extkill-not-real-{}", std::process::id()),
            tmux_seen_live: true, // 曾观测 live
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    );
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    // ① 残留 chat 线程收到 SilentExit(收掉它 + MirrorGuard,不泄漏)。
    match rx.try_recv() {
        Ok(ThreadMsg::SilentExit) => {}
        _ => panic!("应向残留 chat 线程投 SilentExit(PF1)"),
    }
    // ② 合成 ManualKill ChatExited(notify=true)。
    match self_rx.try_recv() {
        Ok(DispatchMsg::ChatExited {
            chat_id,
            reason,
            notify,
            ..
        }) => {
            assert_eq!(chat_id, "oc1");
            assert_eq!(reason, summon_core::exit::ExitReason::ManualKill);
            assert!(notify, "外杀回执应通知");
        }
        _ => panic!("应合成 ChatExited"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// summon 自己拆掉的会话(定时轮 `context: fresh` / 不发回执的重置)在销毁前已写 lifecycle Closed。
/// 扫描若恰好落在「已拆、新 worker 尚未冷启」的窗口,只能投 SilentExit 收残留线程,**不能**再合成外杀回执:
/// 退出路径自己已发 ChatExited,再合成一条会摘掉紧接着重建的新 worker、并往群里发一条假回执。
#[test]
fn run_idle_scan_does_not_report_a_session_summon_tore_down_itself() {
    let tmp = std::env::temp_dir().join(format!("summon-selftear-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent =
        serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, self_rx) = channel::<DispatchMsg>();
    let (tx, rx) = channel::<ThreadMsg>();
    crate::session::lifecycle::mark_closed(&agent, &chat_tag("p2p", "oc1"));
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert(
        "oc1".into(),
        ChatHandle {
            tx,
            last_activity: std::time::SystemTime::now(),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: format!("soul-selftear-not-real-{}", std::process::id()),
            tmux_seen_live: true,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    );
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert!(
        matches!(rx.try_recv(), Ok(ThreadMsg::SilentExit)),
        "残留线程照样要收 SilentExit"
    );
    assert!(
        self_rx.try_recv().is_err(),
        "自拆的会话不得再合成外杀 ChatExited"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 空闲扫描的**盲区**:合成条件是 `tmux_seen_live`,而那个标志只由扫描自己置位、周期 300s。
/// 会话若在两次 tick 之间建起又死掉(实测 `/quit`:11:38:38 建、11:38:44 死),扫描从没「见过它
/// 活着」→ SilentExit 与回执**一起**不发,线程/watcher 也留着。这条钉住盲区确实存在,解释了为什么
/// 退出上报不能只靠扫描(真正的兜底在 chat 线程本轮结束时自查,见
/// `task_arm_reports_exit_when_session_vanished_mid_turn`)。
#[test]
fn idle_scan_cannot_see_sessions_that_died_within_one_tick() {
    let tmp = std::env::temp_dir().join(format!("summon-idle-blind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut agent: Agent =
        serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, self_rx) = channel::<DispatchMsg>();
    let (tx, rx) = channel::<ThreadMsg>();
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert(
        "oc1".into(),
        ChatHandle {
            tx,
            last_activity: std::time::SystemTime::now(),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: format!("soul-idleblind-not-real-{}", std::process::id()),
            // 与上一条测试唯一的差别:从没被扫描观测到 live(新建会话的初值就是它)。
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    );
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    assert!(
        rx.try_recv().is_err(),
        "盲区内扫描不投 SilentExit —— 线程与 watcher 就是这样留下来的"
    );
    assert!(
        self_rx.try_recv().is_err(),
        "盲区内扫描不合成回执 —— 群里什么都收不到"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 红线热路径 `chat_thread_run` 的 Task 臂:本 worker 见过活会话后，本轮结束时会话没了必须
/// 自己上报退出；但在 ensure_started 前静默/拒绝的任务不能把“从未存在”误报成“已关闭”。
#[test]
fn task_arm_reports_exit_when_session_vanished_mid_turn() {
    let source = include_str!("chat_thread.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("chat_thread production source");
    let arm = production
        .find("ThreadMsg::Task(task) => {")
        .expect("Task 臂");
    let next_arm = production[arm..]
        .find("ThreadMsg::Compact {")
        .expect("下一个臂");
    let body = &production[arm..arm + next_arm];
    assert!(
        body.contains("should_report_missing_tmux(")
            && body.contains("tmux_was_observed_live")
            && body.contains("sess.tmux.has_session()"),
        "Task 臂必须用本 worker 的 live 证据自查会话是否已消失"
    );
    assert!(
        body.contains("DispatchMsg::ChatExited"),
        "会话消失时必须上报 ChatExited,否则回执与线程回收都要等扫描(有 300s 盲区)"
    );
    assert!(
        production.contains("Ok(start) => {\n                *tmux_was_observed_live = true;"),
        "ensure_started 成功后必须立即留下 live 证据，不能依赖下一次 300s 扫描"
    );
}

/// `should_notify_exit` 单测全绿也不够:两条退出臂只要写回 `notify: notify_exit`,回执卡照样双发。
/// 这里钉住 Clean/Reset 臂确实接上了「原卡已成终态就不再发回执卡」的判定。
#[test]
fn exit_arms_feed_card_carried_state_into_notify_decision() {
    let source = include_str!("chat_thread.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("chat_thread production source");
    for (ctrl, next) in [
        (
            "ctrl: Control::Clean,",
            "ctrl: ctrl @ (Control::Reset | Control::Archive),",
        ),
        (
            "ctrl: ctrl @ (Control::Reset | Control::Archive),",
            "ThreadMsg::GoneTeardown {",
        ),
    ] {
        let arm = production.find(ctrl).unwrap_or_else(|| panic!("{ctrl} 臂"));
        let end = production[arm..]
            .find(next)
            .unwrap_or_else(|| panic!("{ctrl} 臂的下一个臂"));
        let body = &production[arm..arm + end];
        assert!(
            body.contains("let card_closed = mark_session_card_closed("),
            "{ctrl} 臂必须接住「原卡是否已承载终态」,否则返回值白算"
        );
        assert!(
            body.contains("notify: should_notify_exit(notify_exit, card_closed)"),
            "{ctrl} 臂必须据此决定发不发回执卡;写回裸 notify_exit 就是原卡+新卡双发"
        );
    }
}

/// `/session clean|reset` 是破坏性动作,必须与面板按钮走同一把 operate 闸,并显式声明
/// `notify_exit: true` —— 文字指令没有卡片 id,若沿用旧的「有 card_message_id 才算退出」推断,
/// 用户发了指令会话没了却收不到任何回执。
#[test]
fn session_teardown_subcommand_is_gated_and_reports_exit() {
    let source = include_str!("dispatcher.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("dispatcher production source");
    let start = production
        .find("summon_core::ipc::session_command(")
        .expect("/session 分派入口");
    let end = production[start..]
        .find("summon_core::ipc::model_command(")
        .expect("/session 分支收口");
    let branch = &production[start..start + end];
    assert!(
        branch.contains("can_operate("),
        "破坏性子命令必须过 operate-gate: {branch}"
    );
    assert!(
        branch.contains("notify_exit: true"),
        "文字指令触发的拆除必须显式报退出回执: {branch}"
    );
    assert!(
        branch.contains("Control::Archive") && branch.contains("Control::Clean"),
        "两个子命令都要接上面板同款控制消息"
    );
}

#[test]
fn help_is_local_read_only_and_intercepted_before_provider_slash() {
    let source = include_str!("dispatcher.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("dispatcher production source");
    let start = production
        .find("summon_core::ipc::help_command(")
        .expect("/help 分派入口");
    let end = production[start..]
        .find("// `/session`")
        .expect("/help 分支收口");
    let branch = &production[start..start + end];
    assert!(branch.contains("feishu_chat_help(&agent)"));
    assert!(branch.contains("send_card_archived("));
    assert!(!branch.contains("ensure_started("));
    assert!(!branch.contains("ThreadMsg::Task"));
    let passthrough = production
        .find("summon_core::ipc::slash_command_text(")
        .expect("slash 透传入口");
    assert!(
        start < passthrough,
        "/help 必须先于 provider slash 透传处理"
    );
}

#[test]
fn session_card_actions_are_wired_to_cot_refresh_and_exact_recall_handlers() {
    let dispatcher = include_str!("dispatcher.rs");
    for (variant, handler) in [
        ("CardAction::SessionCot", "handle_card_session_cot"),
        ("CardAction::SessionRefresh", "handle_card_session_refresh"),
        ("CardAction::SessionClose", "handle_card_session_close"),
    ] {
        assert!(dispatcher.contains(variant), "dispatcher 漏接 {variant}");
        assert!(dispatcher.contains(handler), "dispatcher 漏接 {handler}");
    }
    let cards = include_str!("cards.rs");
    let close = cards
        .find("fn handle_card_session_close(")
        .expect("关闭 handler");
    let next = cards[close..]
        .find("\npub(super) fn handle_card_set_effort(")
        .expect("关闭 handler 收口");
    let body = &cards[close..close + next];
    assert!(
        body.contains("fc.recall_message(&card.message_id)"),
        "{body}"
    );
    assert!(body.contains("ThreadMsg::ForgetSessionCard"), "{body}");
    assert!(!body.contains("Control::"), "关闭卡片不得改变会话:{body}");
}

#[test]
fn slash_model_is_gated_and_intercepted_before_passthrough() {
    let source = include_str!("dispatcher.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("dispatcher production source");
    let start = production
        .find("summon_core::ipc::model_command(")
        .expect("/model 分派入口");
    let end = production[start..]
        .find("// `/cd <路径>`")
        .expect("/model 分支收口");
    let branch = &production[start..start + end];
    assert!(
        branch.contains("can_operate("),
        "/model 必须过 operate-gate: {branch}"
    );
    assert!(
        branch.contains("cards::set_model_for_chat("),
        "/model 必须走与面板相同的持久化切换路径: {branch}"
    );
    let catalog_check = branch
        .find("model_effort::model_text_command(")
        .expect("/model 必须先对照候选目录");
    assert!(
        branch[..catalog_check].contains("model_effort::text_command_catalog("),
        "对照的目录要带上 CLI 别名与当前模型: {branch}"
    );
    // 带模型 id 的切换(本群 / 默认)都在对照候选目录之后;`inherit` 不带 id,切回的是默认模型。
    let switch = branch
        .find("ModelTextCommand::Switch(id) =>")
        .expect("候选命中才切换");
    assert!(
        catalog_check < switch && branch[switch..].contains("cards::set_model_for_chat("),
        "对照候选目录必须在切换之前"
    );
    let passthrough = production
        .find("summon_core::ipc::slash_command_text(")
        .expect("slash 透传入口");
    assert!(
        start < passthrough,
        "/model 必须先于 slash 透传处理,否则无头冷启会把它当 prompt"
    );
}

/// 卡片动作按名字分层:「设为默认」那几个动作交给写默认的那一支,本群动作交给写本群的那一支。
#[test]
fn default_card_actions_dispatch_to_the_default_layer() {
    let source = include_str!("dispatcher.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("dispatcher production source");
    let squash = |text: &str| text.split_whitespace().collect::<String>();
    let production = squash(production);
    for (action, call) in [
        (
            "CardAction::Model=>",
            "handle_card_set_model(&mutchats,&agent,&fc,&card,false)",
        ),
        (
            "CardAction::ModelDefault=>",
            "handle_card_set_model(&mutchats,&agent,&fc,&card,true)",
        ),
        (
            "CardAction::Effort=>",
            "handle_card_set_effort(&mutchats,&agent,&fc,&card,false)",
        ),
        (
            "CardAction::EffortDefault=>",
            "handle_card_set_effort(&mutchats,&agent,&fc,&card,true)",
        ),
        (
            "CardAction::CursorConfigDefault=>",
            "cards::handle_card_cursor_config_default(",
        ),
    ] {
        let at = production.find(action).expect(action);
        assert!(
            production[at..]
                .trim_start_matches(action)
                .trim_start_matches('{')
                .starts_with(call),
            "{action} 必须交给 {call}"
        );
    }
}

/// `/effort` 与 `/model` 同构:过 operate-gate(默认按 agent 范围、本群按本群范围),走与面板相同的
/// 切换路径,并在 slash 透传前截获 —— 透传给 claude 只会改它自己的会话、顺带改写用户级默认。
#[test]
fn slash_effort_is_gated_and_intercepted_before_passthrough() {
    let source = include_str!("dispatcher.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("dispatcher production source");
    let start = production
        .find("summon_core::ipc::effort_command(")
        .expect("/effort 分派入口");
    let end = production[start..]
        .find("summon_core::ipc::fast_command(")
        .expect("/effort 分支收口");
    let branch = &production[start..start + end];
    assert!(branch.contains("can_operate("), "{branch}");
    assert!(branch.contains("OperateCommand::EffortDefault"), "{branch}");
    assert!(branch.contains("cards::set_effort_for_chat("), "{branch}");
    let passthrough = production
        .find("summon_core::ipc::slash_command_text(")
        .expect("slash 透传入口");
    assert!(start < passthrough, "/effort 必须先于 slash 透传处理");
}

/// `/fast` 在 Codex 是 per-chat tier，在 Cursor-RPC 是本群参数覆盖；两条都必须过 operate-gate、
/// 复用卡片 apply 接缝，并在 slash 透传前截获。
#[test]
fn slash_fast_is_gated_and_intercepted_before_passthrough() {
    let source = include_str!("dispatcher.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("dispatcher production source");
    let start = production
        .find("summon_core::ipc::fast_command(")
        .expect("/fast 分派入口");
    let end = production[start..]
        .find("// `/mode <id>`")
        .expect("/fast 分支收口");
    let branch = &production[start..start + end];
    assert!(
        branch.contains("can_operate("),
        "/fast 必须过 operate-gate: {branch}"
    );
    assert!(
        branch.contains("cards::apply_codex_service_tier("),
        "Codex /fast 必须走与卡片相同的校验和持久化路径: {branch}"
    );
    assert!(
        branch.contains("cards::apply_cursor_config_option(")
            && branch.contains("CliDriver::Cursor"),
        "Cursor /fast 必须走动态目录校验与本群参数覆盖路径: {branch}"
    );
    assert!(
        branch.contains("CodexServiceTier::Fast") && branch.contains("CodexServiceTier::Standard"),
        "on/off 必须分别显式映射 Fast/Standard: {branch}"
    );
    assert!(
        branch.contains("FastCommand::Invalid"),
        "错误参数必须回用法而不是猜测档位: {branch}"
    );
    assert!(
        !branch.contains("SwitchApply"),
        "/fast 只影响下一轮 turn/start，不应打断或热切当前轮"
    );
    let passthrough = production
        .find("summon_core::ipc::slash_command_text(")
        .expect("slash 透传入口");
    assert!(start < passthrough, "/fast 必须先于 slash 透传处理");
}

/// 命令归属按 agent 自己的 CLI 判:Claude 上 `/fast` 归 Claude Code 原生命令。dispatcher 里
/// 三个按命令名分流的入口(专门分支、兜底用法、透传候选)都必须拿同一个 CLI,任何一处写死
/// 常量都会让 Claude 的 `/fast` 收到「不支持」/ 用法提示,或被当成普通对话。
#[test]
fn command_ownership_call_sites_use_the_agents_own_cli() {
    let source = include_str!("dispatcher.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("dispatcher production source");
    for call in [
        "summon_core::ipc::fast_command(",
        "summon_core::ipc::summon_owned_command(",
        "summon_core::ipc::slash_command_text(",
    ] {
        let start = production
            .find(call)
            .unwrap_or_else(|| panic!("{call} 调用点"));
        let args_end = production[start..].find(") {").expect("调用参数收口");
        let args = &production[start..start + args_end];
        assert!(
            args.contains("CliDriver::from_kind(agent.cli_kind())"),
            "{call} 必须传 agent 自己的 CLI: {args}"
        );
        assert_eq!(
            production.matches(call).count(),
            1,
            "{call} 只应有一个调用点"
        );
    }
}

/// `/mode` 改的是 Cursor ACP 当前 session 的 per-chat 状态：operate-gate、与卡片共用 apply
/// 接缝、只影响下一轮，并且必须早于 slash 透传。
#[test]
fn slash_mode_is_gated_shared_and_intercepted_before_passthrough() {
    let source = include_str!("dispatcher.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("dispatcher production source");
    let start = production
        .find("summon_core::ipc::mode_command(")
        .expect("/mode 分派入口");
    let end = production[start..]
        .find("// `/cd <路径>`")
        .expect("/mode 分支收口");
    let branch = &production[start..start + end];
    assert!(
        branch.contains("can_operate("),
        "/mode 必须过 operate-gate: {branch}"
    );
    assert!(
        branch.contains("cards::apply_cursor_session_mode("),
        "/mode 必须走与卡片相同的校验和持久化路径: {branch}"
    );
    assert!(
        branch.contains("ModeCommand::Invalid"),
        "错误参数必须回用法而不是落盘: {branch}"
    );
    assert!(
        !branch.contains("SwitchApply") && !branch.contains("evict"),
        "/mode 应在下一轮 set_mode，不应打断或重建当前会话"
    );
    let passthrough = production
        .find("summon_core::ipc::slash_command_text(")
        .expect("slash 透传入口");
    assert!(start < passthrough, "/mode 必须先于 slash 透传处理");
}

fn private_reply_test_agent(root: &std::path::Path) -> Agent {
    let mut agent: Agent = serde_yaml::from_str("name: private-reply-agent").unwrap();
    agent.dir = root.join("agents/private-reply-agent");
    std::fs::create_dir_all(&agent.dir).unwrap();
    agent
}

fn pwd_query(chat_type: &str) -> Inbound {
    Inbound {
        anchor_message_id: None,
        chat_id: "oc_pwd".into(),
        chat_type: Some(chat_type.into()),
        text: "/pwd".into(),
        sender: "ou_operator".into(),
        sender_union_id: String::new(),
        sender_type: "user".into(),
        message_id: "om_pwd".into(),
        parent_id: None,
        thread_id: None,
        root_id: None,
        mentions: vec![],
        resources: vec![],
        create_time_ms: 0,
        replayed: false,
        reattach: false,
    }
}

fn private_reply_archive(agent: &Agent, chat_type: &str, id: &str) -> std::path::PathBuf {
    agent
        .state_dir()
        .join("threads")
        .join(chat_tag(chat_type, id))
        .join("outbound.ndjson")
}

#[test]
fn pwd_delivery_writes_one_result_in_the_selected_conversation() {
    for chat_type in ["group", "topic_group", "p2p"] {
        let temp = tempfile::tempdir().unwrap();
        let agent = private_reply_test_agent(temp.path());
        let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
        let msg = pwd_query(chat_type);
        let group = summon_core::authz::is_group_chat(chat_type);
        deliver_pwd_report(
            &fc,
            &agent,
            &msg,
            chat_type,
            pwd_reply_target(group, true),
            "private/project",
        );
        let private = private_reply_archive(&agent, "p2p", &msg.sender);
        let origin = private_reply_archive(&agent, chat_type, &msg.chat_id);
        let result = if group {
            assert!(!origin.exists(), "群命令只应私聊回复");
            private
        } else {
            assert!(!private.exists(), "私聊命令只在原私聊回复");
            origin
        };
        let entries = std::fs::read_to_string(result).unwrap();
        assert_eq!(entries.lines().count(), 1);
        assert!(entries.contains("private/project"));
    }
    let temp = tempfile::tempdir().unwrap();
    let agent = private_reply_test_agent(temp.path());
    let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
    deliver_pwd_report(
        &fc,
        &agent,
        &pwd_query("group"),
        "group",
        PwdReplyTarget::Denied,
        "private/project",
    );
    assert!(!agent.state_dir().join("threads").exists());
}

#[test]
fn pwd_dm_failure_sends_one_safe_thread_reply() {
    for chat_type in ["group", "topic_group"] {
        let temp = tempfile::tempdir().unwrap();
        let agent = private_reply_test_agent(temp.path());
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (stop, requests) = (stopped.clone(), captured.clone());
        let thread = std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(50)).unwrap()
                else {
                    continue;
                };
                let url = request.url().to_string();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let response = if url.contains("auth/v3/tenant_access_token") {
                    r#"{"tenant_access_token":"token","expire":7200}"#
                } else if url.contains("receive_id_type=open_id") {
                    r#"{"code":230002,"msg":"forbidden"}"#
                } else {
                    r#"{"code":0,"data":{"message_id":"om_reply"}}"#
                };
                requests.lock().unwrap().push((url, body));
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
        });
        let fc = FeishuClient::with_test_base(base);
        let msg = pwd_query(chat_type);
        deliver_pwd_report(
            &fc,
            &agent,
            &msg,
            chat_type,
            PwdReplyTarget::OperatorDm,
            "private/project",
        );
        stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        thread.join().unwrap();
        let requests = captured.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(url, _)| url.contains("receive_id_type=open_id"))
                .count(),
            1
        );
        let replies: Vec<_> = requests
            .iter()
            .filter(|(url, _)| url.contains("/im/v1/messages/om_pwd/reply"))
            .collect();
        assert_eq!(replies.len(), 1);
        assert!(!replies[0].1.contains("private/project"));
        assert!(replies[0].1.contains("私信失败"));
        let archived =
            std::fs::read_to_string(private_reply_archive(&agent, chat_type, &msg.chat_id))
                .unwrap();
        assert_eq!(archived.lines().count(), 1);
        assert!(!archived.contains("private/project"));
        assert!(!private_reply_archive(&agent, "p2p", &msg.sender).exists());
    }
}

fn dash_command(chat_type: &str) -> Inbound {
    Inbound {
        text: "/dash".into(),
        message_id: "om_dash".into(),
        ..pwd_query(chat_type)
    }
}

/// 群里发的 `/dash`:卡送到了什么都不说,要说的话私聊发起人;私聊里发的就地回。私聊发不出去时的
/// 退路见 [`dash_notice_falls_back_to_the_group_only_when_the_dm_fails`]。
#[test]
fn dash_notices_go_to_the_sender_not_the_group() {
    use super::dashboard_entry::DashNotice;
    for chat_type in ["group", "topic_group", "p2p"] {
        let group = summon_core::authz::is_group_chat(chat_type);
        for notice in [
            DashNotice::Delivered,
            DashNotice::ToSender("🔒 仅授权用户可打开控制台。".into()),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let agent = private_reply_test_agent(temp.path());
            let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
            let msg = dash_command(chat_type);
            let says = notice != DashNotice::Delivered;
            deliver_dash_notice(&fc, &agent, &msg, chat_type, group, notice);
            let private = private_reply_archive(&agent, "p2p", &msg.sender);
            let origin = private_reply_archive(&agent, chat_type, &msg.chat_id);
            if group {
                assert!(!origin.exists(), "{chat_type}: 群里不留任何东西");
                assert_eq!(private.exists(), says, "{chat_type}: 要说的话私聊发起人");
            } else {
                assert!(!private.exists(), "私聊命令不另开私聊");
                assert_eq!(origin.exists(), says, "私聊命令就地回");
            }
        }
    }
}

/// 私聊发不出去时才回到群里,且只回一条。
#[test]
fn dash_notice_falls_back_to_the_group_only_when_the_dm_fails() {
    let temp = tempfile::tempdir().unwrap();
    let agent = private_reply_test_agent(temp.path());
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (stop, requests) = (stopped.clone(), captured.clone());
    let thread = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let Some(request) = server.recv_timeout(Duration::from_millis(50)).unwrap() else {
                continue;
            };
            let url = request.url().to_string();
            let response = if url.contains("auth/v3/tenant_access_token") {
                r#"{"tenant_access_token":"token","expire":7200}"#
            } else if url.contains("receive_id_type=open_id") {
                r#"{"code":230002,"msg":"forbidden"}"#
            } else {
                r#"{"code":0,"data":{"message_id":"om_reply"}}"#
            };
            requests.lock().unwrap().push(url);
            request
                .respond(tiny_http::Response::from_string(response))
                .unwrap();
        }
    });
    let fc = FeishuClient::with_test_base(base);
    deliver_dash_notice(
        &fc,
        &agent,
        &dash_command("group"),
        "group",
        true,
        super::dashboard_entry::DashNotice::ToSender("🔒 仅授权用户可打开控制台。".into()),
    );
    stopped.store(true, std::sync::atomic::Ordering::SeqCst);
    thread.join().unwrap();
    let requests = captured.lock().unwrap();
    let count = |needle: &str| requests.iter().filter(|url| url.contains(needle)).count();
    assert_eq!(count("receive_id_type=open_id"), 1, "{requests:?}");
    assert_eq!(count("/im/v1/messages/om_dash/reply"), 1, "{requests:?}");
}

#[test]
fn private_query_workdir_picker_sends_only_the_private_card() {
    let temp = tempfile::tempdir().unwrap();
    let agent = private_reply_test_agent(temp.path());
    let donor = temp.path().join("agents/donor");
    let project = temp.path().join("candidate-project");
    std::fs::create_dir_all(&donor).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        donor.join("agent.yaml"),
        format!(
            "name: donor\nruntime:\n  working_dir: {}\n",
            project.display()
        ),
    )
    .unwrap();
    let session = agent.tmux_name_for_chat("group", "oc_picker", false);
    write_chat_meta(
        &agent,
        &session,
        "oc_picker",
        "group",
        Some("picker group"),
        "operator",
        None,
    );
    let card = CardActionMsg {
        operator_open_id: "ou_operator".into(),
        chat_id: "oc_picker".into(),
        message_id: "om_picker".into(),
        event_id: String::new(),
        value: serde_json::json!({"action":"pick_workdir"}),
        wal_id: None,
        wal_replayed: false,
    };
    let fc = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true);
    handle_card_pick_workdir(&HashMap::new(), &agent, &fc, &card);
    let private =
        std::fs::read_to_string(private_reply_archive(&agent, "p2p", "ou_operator")).unwrap();
    assert_eq!(private.lines().count(), 1);
    assert!(private.contains("candidate-project"));
    assert!(!private_reply_archive(&agent, "group", "oc_picker").exists());
}

#[test]
fn pwd_is_read_only_and_does_not_start_or_inject_a_session() {
    let source = include_str!("dispatcher.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("dispatcher production source");
    let start = production
        .find("summon_core::ipc::pwd_command(")
        .expect("/pwd 分派入口");
    let end = production[start..]
        .find("// `/cd <路径>`")
        .expect("/pwd 分支收口");
    let branch = &production[start..start + end];
    assert!(branch.contains("work_dir_plan("));
    assert!(branch.contains("pane_current_path("));
    assert!(branch.contains("handoff_snapshot("));
    assert!(branch.contains("can_operate("));
    assert!(branch.contains("deliver_pwd_report("));
    assert!(!branch.contains("ensure_started("));
    assert!(!branch.contains("SwitchApply"));
    let passthrough = production
        .find("summon_core::ipc::slash_command_text(")
        .expect("slash 透传入口");
    assert!(start < passthrough, "/pwd 必须先于 slash 透传处理");
}

#[test]
fn pwd_delivery_keeps_paths_out_of_group_and_rejects_unauthorized_users() {
    assert_eq!(pwd_reply_target(false, true), PwdReplyTarget::CurrentChat);
    assert_eq!(pwd_reply_target(true, true), PwdReplyTarget::OperatorDm);
    assert_eq!(pwd_reply_target(false, false), PwdReplyTarget::Denied);
    assert_eq!(pwd_reply_target(true, false), PwdReplyTarget::Denied);
}

fn builtin_messaging_for_test() -> Messaging {
    summon_core::config::builtin_messaging()
}

#[test]
fn scheduled_chat_task_builds_streamed_prompt_not_slash_command() {
    let m = builtin_messaging_for_test();
    let t = super::scheduled_chat_task(
        "sched-SCH-3-1700000000000".to_string(),
        "跑日报",
        false,
        m,
        summon_core::ScheduleOutput::Progress,
        summon_core::ScheduleContext::Resume,
        true,
    );
    assert_eq!(t.message_id, "sched-SCH-3-1700000000000");
    assert!(t.text.contains("跑日报"));
    assert_eq!(t.kind, ChatTaskKind::ScheduledPrompt);
    assert!(t.kind.streams_turn(), "定时 prompt 必须走正常 stream_turn");
    assert!(!t.is_group);
    // PRIMER 教了 agent「每一次回复都必须 summon react」,而定时触发没有可回应的真实飞书消息
    // (message_id 是合成的 fire_id)。不在 prompt 里说清楚,agent 就会照做→失败→在回复里向
    // 用户解释一段与任务无关的工具错误(真机上撞到过)。
    assert!(
        t.text.contains("summon react"),
        "定时轮必须显式告诉 agent 别贴表情: {}",
        t.text
    );
    assert!(t.text.contains("不要在回复里提"), "也不能让它解释这件事");

    let result = super::scheduled_chat_task(
        "sched-SCH-4-1700000000000".to_string(),
        "跑日报",
        false,
        builtin_messaging_for_test(),
        summon_core::ScheduleOutput::Result,
        summon_core::ScheduleContext::Resume,
        true,
    );
    assert_eq!(result.kind, ChatTaskKind::ScheduledResult);
    assert!(result.schedule_notice.is_none());
    assert!(result.text.contains("仅结果"));
    assert!(result.text.contains("summon send-image"));
    assert!(!t.fresh_context && !result.fresh_context, "缺省续用上下文");

    let fresh = super::scheduled_chat_task(
        "sched-SCH-5-1700000000000".to_string(),
        "巡检",
        true,
        builtin_messaging_for_test(),
        summon_core::ScheduleOutput::Result,
        summon_core::ScheduleContext::Fresh,
        true,
    );
    assert!(fresh.fresh_context, "context: fresh 必须落到 task 标记上");
    assert_eq!(fresh.kind, ChatTaskKind::ScheduledResult);
}

#[test]
fn running_work_counts_yielded_background_tasks() {
    let (agent, _dir) = crate::test_support::agent_dir("claude", "");
    let tag = chat_tag("group", "oc_running_work");
    assert!(!super::chat_has_running_work(&agent, &tag));
    crate::runtime::background_holder::BackgroundHolderStore::for_agent(&agent.state_dir())
        .add(
            &tag,
            crate::runtime::background_holder::BackgroundHolder {
                turn_id: "turn-held".into(),
                message_id: "om-held".into(),
                chat_id: "oc_running_work".into(),
                chat_type: "group".into(),
                origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
                requester_open_id: "ou_requester".into(),
                requester_union_id: String::new(),
                requester_is_peer: false,
                tmux_session: "soul-held".into(),
                session_id: "sid".into(),
                transcript: std::path::PathBuf::from("/nonexistent"),
                tasks: Vec::new(),
                created_at_ms: 0,
                expires_at_ms: i64::MAX,
            },
        )
        .unwrap();
    assert!(super::chat_has_running_work(&agent, &tag));
    // 定时轮到点判「本 chat 有没有活在跑」走的是这一个判据。
    let dispatcher: String = include_str!("dispatcher.rs")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(dispatcher.contains(
        "schedule_context_at_fire(context,chat_has_running_work(&agent,&chat_tag(&chat_type,&chat_id)),"
    ));
}

#[test]
fn schedule_context_at_fire_keeps_a_running_turn_untouched() {
    use summon_core::ScheduleContext::{Fresh, Resume};
    assert_eq!(super::schedule_context_at_fire(Fresh, true), Resume);
    assert_eq!(super::schedule_context_at_fire(Fresh, false), Fresh);
    assert_eq!(super::schedule_context_at_fire(Resume, true), Resume);
    assert_eq!(super::schedule_context_at_fire(Resume, false), Resume);
}

#[test]
fn append_transcript_writes_and_appends_in_order() {
    let tmp = std::env::temp_dir().join(format!("summon-transcript-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    append_transcript(&tmp, "p2p", "oc_x", "用户 老板", "你好");
    append_transcript(&tmp, "p2p", "oc_x", "starlight", "在");
    let p = tmp
        .join("threads")
        .join(chat_tag("p2p", "oc_x"))
        .join("transcript.md");
    let s = std::fs::read_to_string(&p).unwrap();
    assert!(s.contains("用户 老板") && s.contains("你好"));
    assert!(s.contains("starlight") && s.contains("在"));
    assert!(s.find("你好").unwrap() < s.find("在").unwrap()); // 追加保序
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn reply_transcript_keeps_the_body_and_drops_the_feed_tag() {
    let tmp = tempfile::tempdir().unwrap();
    append_reply_transcript(
        tmp.path(),
        "p2p",
        "oc_feed",
        "agent",
        "正文结论\n<summon-feed>总结</summon-feed>",
    );
    let text = std::fs::read_to_string(
        tmp.path()
            .join("threads")
            .join(chat_tag("p2p", "oc_feed"))
            .join("transcript.md"),
    )
    .unwrap();
    assert!(text.contains("正文结论"));
    assert!(!text.contains("summon-feed"), "{text}");
}

#[test]
fn append_transcript_rotates_oversized_log() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp
        .path()
        .join("threads")
        .join(chat_tag("p2p", "oc_rotate"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("transcript.md");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(crate::observe::event_log::CHAT_LOG_MAX_BYTES + 1)
        .unwrap();

    append_transcript(tmp.path(), "p2p", "oc_rotate", "agent", "新的正文");

    assert!(path.with_file_name("transcript.md.1").exists());
    assert!(std::fs::read_to_string(path).unwrap().contains("新的正文"));
}

/// control.sock 端到端:客户端投一条 Command → handler 交给 Core → 回写 `CommandOutcome`。
/// 用 `RuntimeStatus`(Owner-only、纯读、必回 `Value`)当载体:这条测的是**连接层**
/// —— 一行 JSON 进、一行 JSON 出、然后关连接。
#[test]
fn control_conn_submits_command_and_returns_outcome() {
    use summon_core::core_port::{Actor, Command, CommandOutcome, Intent};
    let core = Arc::new(crate::runtime::core::Core::new(
        HashMap::new(),
        HashMap::new(),
        Vec::new(),
    ));
    let (client, server) = UnixStream::pair().unwrap();
    // server 端在线程里处理一条命令并回写。
    let core2 = Arc::clone(&core);
    let h = thread::spawn(move || handle_control_conn(server, core2));
    let cmd = Command {
        actor: Actor::Owner,
        intent: Intent::RuntimeStatus,
    };
    let mut client = client;
    let line = serde_json::to_string(&cmd).unwrap();
    client.write_all(line.as_bytes()).unwrap();
    client.write_all(b"\n").unwrap();
    client.flush().unwrap();
    let mut resp = String::new();
    BufReader::new(client).read_line(&mut resp).unwrap();
    h.join().unwrap();
    let outcome: CommandOutcome = serde_json::from_str(resp.trim()).unwrap();
    match outcome {
        CommandOutcome::Value(v) => {
            assert_eq!(v["pid"], std::process::id());
        }
        other => panic!("应返回 Value(运行时快照),得到 {other:?}"),
    }
}

/// 阻塞闸门端到端(走真 control.sock handler + 真 `summon ask` 意图):agent 经连接发
/// `ChatAsk` → Core 登记闸门、把提问卡投给 dispatcher、回 `Blocked` → handler **不关连接**、
/// 就地 `wait_gate` 阻塞;另一线程 resolve → handler 被唤醒、回写答案、关连接。
/// 验「持连接阻塞 + resolve 唤醒」这条 `summon ask` 赖以工作的通路。
#[test]
fn control_conn_blocks_on_chat_ask_until_resolved() {
    // 等 handler 真的 attach 再 resolve:钉「持连接阻塞 → 被唤醒」这条主路。
    run_control_conn_chat_ask(true, false);
}

/// 同一条端到端通路,但**不等 attach 就 resolve** —— 人点卡片抢在 handler 进 `wait_gate`
/// 之前到达。这是本竞态的生产形态:修复前裁决会连同还没被取走的 rx 一起被 remove,
/// handler 回写的是「闸门不存在或已被等待」,用户的答案被静默丢弃(实测 40 次挂 11 次)。
/// 现在 resolve 早到只是暂存,handler attach 时领走,回写的仍是用户那个答案。
#[test]
fn control_conn_chat_ask_survives_resolve_before_waiter_attaches() {
    run_control_conn_chat_ask(false, false);
}

#[test]
fn control_conn_chat_ask_drops_gate_when_client_disconnects() {
    run_control_conn_chat_ask(true, true);
}

/// 两条 chat_ask 端到端测的共用主体。`wait_for_attach` 决定 resolve 前是否等 handler 进
/// `wait_gate`:true = 已 attach 分支,false = 裁决抢先的暂存分支。除此之外完全同一条路径。
fn run_control_conn_chat_ask(wait_for_attach: bool, disconnect: bool) {
    use summon_core::core_port::{
        Actor, AskQuestion, ChatAgentIdentity, Command, CommandOutcome, CommandSink, Intent,
        QueryApi,
    };
    let dir = tempfile::tempdir().unwrap();
    let (gate_tx, gate_rx) = std::sync::mpsc::channel();
    let mut agent: Agent = serde_yaml::from_str("name: coder\n").unwrap();
    // `agent.dir` 必须是 tempdir 的**子目录**:`state_dir()` 取 `dir.parent()` 当 agents_dir,
    // `state_root()` 又取它的 parent —— 直接给 tempdir 根,两级 parent 会一路爬到文件系统根,
    // 算出 `/state/coder`,于是这几条测试在任何机器上都因权限失败。同文件其余夹具都带 `agents/`。
    let agents_dir = dir.path().join("agents");
    agent.dir = agents_dir.join("coder");
    // Core 按别名取 agent 以盘上 agent.yaml 为准,夹具也得落盘。
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::write(agent.manifest_path(), "name: coder\n").unwrap();
    let core = Arc::new(crate::runtime::core::Core::new(
        HashMap::new(),
        HashMap::from([("coder".to_string(), gate_tx)]),
        vec![agent.clone()],
    ));
    // 发布本轮短租约上下文:`summon ask` 的一次性凭证就来自它。
    let turn = crate::runtime::current_turn_store::CurrentTurnContext {
        turn_id: "turn-gate-1".into(),
        agent_alias: "coder".into(),
        delivery_scope: "test-scope".into(),
        chat_id: "oc_gate".into(),
        chat_type: "group".into(),
        conversation_tag: "g-gate".into(),
        message_id: "om_ask".into(),
        origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
        schedule_output: summon_core::ScheduleOutput::Progress,
        requester_open_id: "ou_asker".into(),
        requester_union_id: String::new(),
        requester_is_bot: false,
        requester_is_peer: false,
        thread_id: None,
        root_id: None,
        started_at_ms: 1_800_000_000_000,
        reattached: false,
    };
    let _lease =
        crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
            .activate(&turn)
            .unwrap();

    let (client, server) = UnixStream::pair().unwrap();
    let core_h = Arc::clone(&core);
    let (handler_done_tx, handler_done_rx) = std::sync::mpsc::channel();
    let h = thread::spawn(move || {
        handle_control_conn(server, core_h);
        let _ = handler_done_tx.send(());
    });
    let ask = Command {
        actor: Actor::ChatAgent(Box::new(ChatAgentIdentity {
            turn_id: turn.turn_id.clone(),
            alias: turn.agent_alias.clone(),
            requester_open_id: turn.requester_open_id.clone(),
            source_ref: summon_core::MsgRef::default(),
            chat_type: turn.chat_type.clone(),
            conversation_tag: turn.conversation_tag.clone(),
        })),
        intent: Intent::ChatAsk {
            turn_id: turn.turn_id.clone(),
            alias: turn.agent_alias.clone(),
            conversation_tag: turn.conversation_tag.clone(),
            questions: vec![AskQuestion {
                question: "要继续吗?".into(),
                options: vec!["是".into(), "否".into()],
                ..Default::default()
            }],
        },
    };
    let mut client = client;
    let line = serde_json::to_string(&ask).unwrap();
    client.write_all(line.as_bytes()).unwrap();
    client.write_all(b"\n").unwrap();
    client.flush().unwrap();
    // 提问卡必须真投到该 agent 的 dispatcher(否则用户根本看不到这一问)。gate_id 直接从卡片上取:
    // 卡片是 `do_chat_ask` 返回 `Blocked` **之前**投出的,所以拿到它 = 站在 handler 可能 attach 的
    // 最早时点 —— 不等 attach 的那一路要的就是这个时点,越早越接近生产里「人抢先点卡片」的形态。
    let gate_id = match gate_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
        DispatchMsg::ChatAskCard { gate_id, .. } => summon_core::ids::GateId::new(gate_id),
        _ => panic!("提问卡应投到 dispatcher,收到的却是别的 DispatchMsg"),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    assert!(!core.list_gates().is_empty(), "发卡前闸门就该已登记");
    // **「已登记」不等于「已经在等」**:闸门一登记就出现在表里,而 handler 还要从 `submit`
    // 一路返回才进 `wait_gate` 取 rx。要测「已 attach」那条分支就得等真正的那个状态转换,
    // 拿 `list_gates()` 当判据是在赌调度(会悄悄测到暂存分支去)。
    if wait_for_attach {
        loop {
            if core.gate_waiter_attached(&gate_id) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "handler 应在有界时间内进入 wait_gate"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    if disconnect {
        drop(client);
        handler_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("客户端断开后 handler 必须有界撤掉 gate 并退出");
        h.join().unwrap();
        assert!(core.list_gates().is_empty(), "断连后不能留下悬空 gate");
        return;
    }
    assert_eq!(
        core.submit(Command {
            actor: Actor::Owner,
            intent: Intent::QuestionResolve(gate_id, r#"["是"]"#.into()),
        }),
        CommandOutcome::Done
    );
    // handler 被唤醒,回写答案后关连接。
    let mut resp = String::new();
    BufReader::new(client).read_line(&mut resp).unwrap();
    handler_done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("resolve 后 handler 必须有界退出");
    h.join().unwrap();
    let outcome: CommandOutcome = serde_json::from_str(resp.trim()).unwrap();
    match outcome {
        CommandOutcome::Value(v) => assert_eq!(v["answer"], r#"["是"]"#),
        other => panic!("阻塞 handler 应回作答,得到 {other:?}"),
    }
}

// append_outbound 的测试随实现平移到 frontend/outbound.rs(A6)。

#[test]
fn mark_started_failure_blocks_handle_task_then_repair_executes_once() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("inbox");
    let wal = crate::runtime::inbox_wal::InboxWal::new(root.clone());
    let inbound = Inbound {
        anchor_message_id: None,
        chat_id: "oc_1".into(),
        chat_type: Some("p2p".into()),
        text: "只执行一次".into(),
        sender: "ou_1".into(),
        sender_union_id: String::new(),
        sender_type: String::new(),
        message_id: "om_start_barrier".into(),
        parent_id: None,
        thread_id: None,
        root_id: None,
        mentions: Vec::new(),
        resources: Vec::new(),
        create_time_ms: 1,
        replayed: false,
        reattach: false,
    };
    wal.append("agent-a", "scope-a", &inbound).unwrap();
    let bucket = std::fs::read_dir(root.join("pending"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let path = std::fs::read_dir(bucket)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let original = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    let calls = std::cell::Cell::new(0usize);
    let failed = run_after_durable_start(
        &wal,
        "agent-a",
        "scope-a",
        "om_start_barrier",
        false,
        || calls.set(calls.get() + 1),
    );
    assert!(failed.is_err());
    assert_eq!(
        calls.get(),
        0,
        "mark_started 失败时 handle_task 次数必须为 0"
    );

    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, original).unwrap();
    assert_eq!(
        run_after_durable_start(
            &wal,
            "agent-a",
            "scope-a",
            "om_start_barrier",
            false,
            || calls.set(calls.get() + 1),
        )
        .unwrap(),
        (
            crate::runtime::inbox_wal::MarkStartedOutcome::Marked,
            Some(())
        )
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(
        run_after_durable_start(
            &wal,
            "agent-a",
            "scope-a",
            "om_start_barrier",
            false,
            || calls.set(calls.get() + 1),
        )
        .unwrap(),
        (
            crate::runtime::inbox_wal::MarkStartedOutcome::AlreadyStarted,
            None
        )
    );
    assert_eq!(calls.get(), 1, "同一 durable started 记录不能再次执行");
}

/// 接管任务的 durable start 判据与普通轮**相反**:上一代已经把这条记录 `mark_started` 过,
/// 本代要接住的正是它。按普通轮的判据(只有 `Marked` 才执行)它永远进不了闭包,那一轮就再也
/// 没人接。放行的只有 `AlreadyStarted` 这一种:`AlreadyConsumed` 说明这一轮已经收过口、
/// `Missing` 说明连恢复依据都没有,再跑一遍都是双执行。
#[test]
fn reattach_task_accepts_already_started_record() {
    use crate::runtime::inbox_wal::MarkStartedOutcome;
    let temp = tempfile::tempdir().unwrap();
    let wal = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
    let inbound = Inbound {
        anchor_message_id: None,
        chat_id: "oc_1".into(),
        chat_type: Some("p2p".into()),
        text: "上一代已经注入过的那条".into(),
        sender: "ou_1".into(),
        sender_union_id: String::new(),
        sender_type: String::new(),
        message_id: "om_reattach".into(),
        parent_id: None,
        thread_id: None,
        root_id: None,
        mentions: Vec::new(),
        resources: Vec::new(),
        create_time_ms: 1,
        replayed: false,
        reattach: true,
    };
    wal.append("agent-a", "scope-a", &inbound).unwrap();
    let run = |reattach: bool| {
        run_after_durable_start(&wal, "agent-a", "scope-a", "om_reattach", reattach, || {
            1usize
        })
        .unwrap()
    };
    assert_eq!(run(false), (MarkStartedOutcome::Marked, Some(1)));
    assert_eq!(
        run(false),
        (MarkStartedOutcome::AlreadyStarted, None),
        "普通轮不得重跑已 started 的记录"
    );
    assert_eq!(
        run(true),
        (MarkStartedOutcome::AlreadyStarted, Some(1)),
        "接管轮要的就是这条已 started 的记录"
    );

    wal.mark_consumed_after_start("agent-a", "scope-a", "om_reattach")
        .unwrap();
    assert_eq!(run(true), (MarkStartedOutcome::AlreadyConsumed, None));
    assert_eq!(
        run_after_durable_start(&wal, "agent-a", "scope-a", "om_missing", true, || 1usize).unwrap(),
        (MarkStartedOutcome::Missing, None)
    );
}

// ---- pause+append(Item B)select 决策:poll_headless_steer ----

/// 造一条最小 ChatTask(只填 text;其余给默认),投到 chat channel 模拟 mid-turn 新 inbound。
fn task_msg_with_id(message_id: &str, text: &str) -> ThreadMsg {
    ThreadMsg::Task(Box::new(ChatTask {
        anchor_message_id: None,
        schedule_notice: None,
        fresh_context: false,
        fresh_requested: false,
        queued: false,
        handoff: None,
        sender_is_bot: false,
        text: text.to_string(),
        message_id: message_id.into(),
        who: "张三".into(),
        who_id: "ou_x".into(),
        who_union_id: String::new(),
        sender_is_peer: false,
        is_group: false,
        kind: ChatTaskKind::Conversation,
        slash_admitted: false,
        addressed: true,
        parent_id: None,
        thread_id: None,
        root_id: None,
        resources: vec![],
        co_mentioned: Vec::new(),
        hide_context_hint: false,
        messaging: summon_core::config::builtin_messaging(),
        reattach: false,
    }))
}

fn task_msg(text: &str) -> ThreadMsg {
    task_msg_with_id("m1", text)
}

fn test_followup_ctx<'a>(
    rx: &'a Receiver<ThreadMsg>,
    requeue: Sender<ThreadMsg>,
    _debounce_ms: u64,
) -> (tempfile::TempDir, FollowupCtx<'a>) {
    let temp = tempfile::tempdir().unwrap();
    let context = FollowupCtx {
        rx,
        requeue,
        // 测试只验排队/打断决策,正文组装恒等即可(真实组装在 handle_task 里构造)。
        compose: &|_t, body| body.to_string(),
        steer_transcript: false, // 默认关:既有回归测试仍验旧的串行排队语义
        steer_rpc: false,
        steer_cli_session: false,
        wal: crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox")),
        agent_name: "t".into(),
        delivery_scope: "test-scope".into(),
        requester_open_id: "ou_x".into(),
        requester_union_id: String::new(),
        session_end_evidence: None,
        handoff_turn: false,
    };
    (temp, context)
}

fn checkpoint_followup(context: &FollowupCtx, message_id: &str, text: &str) {
    context
        .wal
        .append(
            &context.agent_name,
            &context.delivery_scope,
            &Inbound {
                anchor_message_id: None,
                chat_id: "oc_1".into(),
                chat_type: Some("p2p".into()),
                text: text.into(),
                sender: "ou_1".into(),
                sender_union_id: String::new(),
                sender_type: String::new(),
                message_id: message_id.into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: Vec::new(),
                resources: Vec::new(),
                create_time_ms: 1,
                replayed: false,
                reattach: false,
            },
        )
        .unwrap();
}

fn reset_msg(card_message_id: Option<&str>) -> ThreadMsg {
    ThreadMsg::Control {
        ctrl: Control::Reset,
        // 这些用例造的都是卡片路径:有卡片 id = 面板点的 = 退出,与生产发起点一致。
        notify_exit: card_message_id.is_some(),
        idle_retired: false,
        card_message_id: card_message_id.map(str::to_string),
        models: Vec::new(),
        origin: CardOrigin::Panel,
        source_task: None,
        source_started: false,
    }
}

fn pending_arc(
    prompt: &str,
    partial: &str,
) -> Arc<std::sync::Mutex<summon_core::pending::PendingTurn>> {
    Arc::new(std::sync::Mutex::new(summon_core::pending::PendingTurn {
        kind: summon_core::pending::PendingTurnKind::Chat,
        delivery_scope: "test-scope".into(),
        agent_name: "t".into(),
        inbox_message_ids: vec!["m1".into()],
        resume_authorized: false,
        chat_id: "ou_x".into(),
        chat_type: "p2p".into(),
        cli: "cursor".into(),
        prompt: prompt.into(),
        session_id: Some("sid1".into()),
        partial: partial.into(),
    }))
}

/// 一个不存在的 pgid 的 ChildKiller:interrupt() 走幂等闸置位、killpg 对不存在组无副作用。
fn fake_killer() -> crate::runtime::executor::ChildKiller {
    crate::runtime::executor::ChildKiller::new(990_001)
}

/// `agents kill` 必须把 RPC 的**两条**登记都收掉:轮键 `<tag>`(这一轮)与会话键 `rpc:<tag>`
/// (跨轮存活的 app-server 进程)。键分开是刻意的,所以这里也必须各打一次 —— 只打轮键的话,
/// chat 拆了、会话进程还在,变成一个没人认领的孤儿,直到 daemon stop 才被收。
#[test]
fn explicit_kill_reaps_both_the_turn_and_the_persistent_rpc_session() {
    let inflight = crate::runtime::inflight::InFlight::new();
    let tag = chat_tag("p2p", "oc_rpc");
    let base = std::env::temp_dir();
    // pgid 取远超 pid_max 的值:只验注册表账目与幂等闸,不命中任何真进程组。
    let turn_killer = crate::runtime::executor::ChildKiller::new(990_301);
    let session_killer = crate::runtime::executor::ChildKiller::new(990_302);
    inflight.insert(
        crate::runtime::inflight::TurnKey::new("starlight", tag.clone()),
        crate::runtime::inflight::InFlightTurn {
            killer: turn_killer.clone(),
            base: base.clone(),
            pending: pending_arc("活儿", "半截"),
            shape: crate::runtime::inflight::TurnShape::OwnedProcess,
        },
    );
    inflight.insert(
        crate::capture::rpc_pool::inflight_key("starlight", &tag),
        crate::runtime::inflight::InFlightTurn {
            killer: session_killer.clone(),
            base,
            pending: pending_arc("", ""),
            shape: crate::runtime::inflight::TurnShape::OwnedProcess,
        },
    );

    interrupt_inflight_turn(&inflight, "starlight", "p2p", "oc_rpc");

    assert!(turn_killer.was_killed(), "在跑的那一轮要打断");
    assert!(
        session_killer.was_killed(),
        "常驻 RPC 会话进程也要收掉,否则 chat 拆了它还在"
    );
}

/// 无头形态的一轮打断把手(无软打断):`poll_headless_steer` / `await_interrupted_headless`
/// 现在收的是它,语义仍是 SIGINT 整组 → 宽限后 SIGKILL。
fn fake_stopper() -> crate::capture::TurnStopper {
    crate::capture::TurnStopper::child(fake_killer())
}

#[test]
fn interrupted_headless_wait_returns_on_done() {
    let (tx, rx) = channel::<TurnUpdate>();
    tx.send(TurnUpdate::Done(Ok(TurnOutcome::default())))
        .unwrap();
    let mut last_reply = None;
    let got = await_interrupted_headless(
        &rx,
        &fake_stopper(),
        Duration::from_secs(1),
        Duration::from_secs(1),
        &mut last_reply,
    );
    assert!(matches!(got, Some(Ok(_))), "worker Done 应立即收尾");
}

#[test]
fn interrupted_headless_wait_is_bounded_even_without_done() {
    let (_keep_open, rx) = channel::<TurnUpdate>();
    let started = std::time::Instant::now();
    let mut last_reply = None;
    let got = await_interrupted_headless(
        &rx,
        &fake_stopper(),
        Duration::from_millis(5),
        Duration::from_millis(5),
        &mut last_reply,
    );
    assert!(got.is_none(), "无 Done 时应在 SIGINT→SIGKILL 宽限后返回");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "显式打断不得裸 recv 永久等待"
    );
}

#[test]
fn interrupted_headless_wait_keeps_the_last_reply_before_done() {
    let (tx, rx) = channel::<TurnUpdate>();
    tx.send(TurnUpdate::Message("超时前的部分输出".into()))
        .unwrap();
    tx.send(TurnUpdate::Done(Ok(TurnOutcome {
        timed_out: true,
        ..Default::default()
    })))
    .unwrap();
    let mut last_reply = None;
    let got = await_interrupted_headless(
        &rx,
        &fake_stopper(),
        Duration::from_secs(1),
        Duration::from_secs(1),
        &mut last_reply,
    );
    assert!(matches!(
        got,
        Some(Ok(TurnOutcome {
            timed_out: true,
            ..
        }))
    ));
    assert_eq!(last_reply.as_deref(), Some("超时前的部分输出"));
}

#[test]
fn timeout_retains_streamed_partial_without_marking_the_turn_clean() {
    let out = Ok(TurnOutcome {
        timed_out: true,
        ..Default::default()
    });
    let mut last_reply = None;
    retain_timeout_partial_reply(&out, &mut last_reply, "已经生成的半段正文");
    assert_eq!(
        last_reply.as_deref(),
        Some("⚠️ 以下是超时前的部分输出：\n\n已经生成的半段正文")
    );
    assert_eq!(
        headless_leftover_outcome(
            Some(crate::runtime::inflight::FinishDisposition::Completed),
            &out,
            false,
            false,
        ),
        LeftoverCardOutcome::Failed(AbortReason::Timeout),
        "保留部分输出不能把 timeout 改判成完成"
    );
}

#[test]
fn unfinished_headless_worker_is_detached_without_implicit_join() {
    let (release_tx, release_rx) = channel::<()>();
    let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let exited_worker = Arc::clone(&exited);
    let worker = std::thread::spawn(move || {
        let _ = release_rx.recv();
        exited_worker.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let started = std::time::Instant::now();
    assert!(
        !reap_headless_worker(worker, false),
        "未结束 worker 必须 detach，不能 join"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "有界打断不得在 worker 尾部隐式等待"
    );
    let _ = release_tx.send(());
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !exited.load(std::sync::atomic::Ordering::SeqCst) && std::time::Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    assert!(exited.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn boot_recovery_and_shutdown_barriers_keep_structural_order() {
    // 这两条是线程顺序不变量,单测无法稳定用调度器复现竞态;直接钉住 boot 的
    // happens-before:恢复分类/清 pending 先于 dispatcher 扫描,冻结注册先于停机快照。
    let src = include_str!("boot.rs");
    let recover = src.find("    boot_recover_inbox(\n").unwrap();
    let dispatcher = src
        .find(
            "            agent_dispatcher(agent, poll, deb, fc, epoch, rx, self_tx, core_d, inflight_d)",
        )
        .unwrap();
    assert!(
        recover < dispatcher,
        "崩溃恢复必须在 dispatcher 扫 pending.json 前完成"
    );

    // 镜像补发的批准集必须在启动恢复之前定:接管让路靠的就是这一步先算完
    // (`plan_mirror_catchup` 里的接管判据 → `record_approved_mirror_catchup`)。倒过来的话
    // 恢复扫描已经把接管轮投出去了,镜像才去问「要不要让路」,同一段字节两条路都发。
    let catchup = src
        .find("        .map(|record| plan_mirror_catchup(")
        .unwrap();
    assert!(
        catchup < recover,
        "镜像补发批准集必须早于启动恢复,否则接管与补发会同时放行"
    );

    let freeze = src.find("    inflight.freeze();\n").unwrap();
    let drain = src
        .find("    let n = inflight.shutdown_interrupt_all(\n")
        .unwrap();
    assert!(freeze < drain, "停机必须先冻结新注册,再快照 drain");

    let signal = src.find("        signal_hook::flag::register(").unwrap();
    let pidfile = src
        .find("    write_identity_file(&pid_file(), daemon_pid)?;")
        .unwrap();
    assert!(signal < pidfile, "signal handler 必须早于可发现的 pidfile");

    // 自动拉起的熔断必须在拿到单例锁之后、一切启动期工作之前:预检失败退出的一代也得留下
    // 启动记录,补发积压(outbox / 恢复卡 / 错过的定时轮)更不能跑在熔断前面。
    let lock = src
        .find("    let mut singleton_lock = match try_exclusive_lock(")
        .unwrap();
    let guard = src
        .find("crash_guard::check_and_record(daemon_pid)")
        .unwrap();
    let preflight = src.find("    let inbox_health =").unwrap();
    assert!(
        lock < guard && guard < preflight && guard < signal,
        "熔断必须紧跟单例锁、早于预检与信号注册"
    );

    // 用户 PATH 必须在单线程段补上(set_var 只在那时安全),且早于任何 spawn:agent 会话的 PATH
    // 全继承自 daemon,补晚了第一批会话就缺目录。
    let healed = src
        .find("    crate::setup::env::heal_tool_path();\n")
        .unwrap();
    let user_path = src
        .find("    crate::setup::env::apply_saved_user_path();\n")
        .unwrap();
    assert!(
        healed < user_path && user_path < lock,
        "用户 PATH 必须紧跟 PATH 自愈、早于单例锁之后的一切启动工作"
    );

    // 熔断全靠启动记录判上一代:就绪时刻漏记,每一代崩溃后都会被当成「没跑稳」而不被拉起;
    // 停机漏删,每一次优雅停机都会被当成崩溃。
    let ready_written = src
        .find("    write_identity_file(&ready_file(), daemon_pid)?;")
        .unwrap();
    let marked_ready = src
        .find("    crash_guard::mark_ready(daemon_pid);")
        .unwrap();
    assert!(
        ready_written < marked_ready,
        "就绪时刻必须在 ready 写下之后补记"
    );
    let startup_stop = src.find("summon_i18n::text!(\"r4ff72dfdb8\"").unwrap();
    let cleared_on_startup_stop = src[..startup_stop]
        .rfind("crash_guard::clear_boot_record(daemon_pid);")
        .unwrap();
    assert!(
        startup_stop - cleared_on_startup_stop < 200,
        "启动期间被要求停机的分支必须清启动记录"
    );
    let clean_marker = src
        .find("atomic_write(&clean_shutdown_marker(), &clean_marker)")
        .unwrap();
    let final_pidfile = src
        .find("    let _ = remove_identity_file_if_matches(&pid_file(), daemon_pid);")
        .unwrap();
    let cleared_on_stop = src[clean_marker..final_pidfile]
        .find("crash_guard::clear_boot_record(daemon_pid);")
        .is_some();
    assert!(cleared_on_stop, "优雅停机收尾必须清启动记录");

    let dispatcher = src.find("            agent_dispatcher(").unwrap();
    let control = src
        .find("    let ctrl = UnixListener::bind(control_sock())")
        .unwrap();
    let events = src
        .find("    let listener = UnixListener::bind(events_sock())?")
        .unwrap();
    let scheduler = src.find("scheduler::spawn_scheduler(").unwrap();
    let ready = src
        .find("    write_identity_file(&ready_file(), daemon_pid)?;")
        .unwrap();
    assert!(
        [signal, dispatcher, control, events, scheduler]
            .into_iter()
            .all(|step| step < ready),
        "ready 只能在 signal/dispatcher/control/events/scheduler 全部就绪后写"
    );

    let scheduler_join = src.find("    if scheduler.join().is_err() {").unwrap();
    let dispatcher_shutdown = src
        .find("        let _ = tx.send(DispatchMsg::Shutdown);")
        .unwrap();
    assert!(
        scheduler_join < dispatcher_shutdown,
        "必须先 join scheduler，再关闭 dispatcher"
    );
}

#[test]
fn poll_button_interrupt_returns_stop_and_requeues_others() {
    // 任务4:控制卡「⏸ 打断」→ ThreadMsg::Interrupt。即便 `!` 总开关关,按钮也触发纯停 Stop;
    // 同批的普通消息原序回填(不丢)。
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(task_msg("排队的普通消息")).unwrap();
    tx.send(ThreadMsg::Interrupt).unwrap();
    let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 30);
    let stopper = fake_stopper();
    let pend = pending_arc("活儿", "");
    let r = poll_headless_steer(&fc, CliDriver::Cursor, Some(&stopper), &pend, "", "", "t");
    assert!(
        matches!(r, Some(crate::capture::TurnInterrupt::Stop)),
        "按钮应触发纯停 Stop"
    );
    let back = rx
        .recv_timeout(Duration::from_millis(200))
        .expect("普通消息应原序回填,不丢");
    assert!(matches!(back, ThreadMsg::Task(_)));
}

fn new_session_msg(message_id: &str) -> ThreadMsg {
    let ThreadMsg::Task(mut task) = task_msg_with_id(message_id, "帮我看下这个实验") else {
        unreachable!()
    };
    task.fresh_context = true;
    task.fresh_requested = true;
    ThreadMsg::Task(task)
}

/// 两条捕获路都以 `/new` 划分会话：之前的任务销账，之后的任务在新会话继续。
#[test]
fn a_new_session_request_stops_the_turn_on_both_capture_paths() {
    for headless in [true, false] {
        let (tx, rx) = channel::<ThreadMsg>();
        tx.send(task_msg_with_id("om_before", "排在前面的人话"))
            .unwrap();
        tx.send(new_session_msg("om_new")).unwrap();
        tx.send(task_msg_with_id("om_after", "新会话追加消息"))
            .unwrap();
        let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 30);
        checkpoint_followup(&fc, "om_before", "排在前面的人话");
        checkpoint_followup(&fc, "om_new", "帮我看下这个实验");
        checkpoint_followup(&fc, "om_after", "新会话追加消息");
        let r = if headless {
            let stopper = fake_stopper();
            let pend = pending_arc("活儿", "");
            poll_headless_steer(&fc, CliDriver::Cursor, Some(&stopper), &pend, "", "", "t")
        } else {
            poll_transcript_steer(&fc, "t", None)
        };
        assert!(
            matches!(r, Some(crate::capture::TurnInterrupt::SessionEnded)),
            "headless={headless}"
        );
        let first = rx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert!(
            matches!(&first, ThreadMsg::Task(task) if task.message_id == "om_new"),
            "headless={headless}: /new 排第一"
        );
        let second = rx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert!(matches!(&second, ThreadMsg::Task(task) if task.message_id == "om_after"));
        assert!(rx.try_recv().is_err(), "边界之前的消息不进新会话");
        let restarted = crate::runtime::inbox_wal::InboxWal::new(_wal_tmp.path().join("inbox"));
        assert!(
            matches!(
                restarted
                    .mark_started(&fc.agent_name, &fc.delivery_scope, "om_before")
                    .unwrap(),
                crate::runtime::inbox_wal::MarkStartedOutcome::AlreadyConsumed
            ),
            "丢弃在重启后仍生效"
        );
        assert!(
            matches!(
                restarted
                    .mark_started(&fc.agent_name, &fc.delivery_scope, "om_after")
                    .unwrap(),
                crate::runtime::inbox_wal::MarkStartedOutcome::Marked
            ),
            "边界之后的消息保持待处理"
        );
    }
}

#[test]
fn a_new_session_preserves_the_queue_when_discard_cannot_be_settled() {
    for headless in [true, false] {
        let (tx, rx) = channel::<ThreadMsg>();
        tx.send(task_msg_with_id("om_before", "旧会话追加消息"))
            .unwrap();
        tx.send(new_session_msg("om_new")).unwrap();
        let (_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 30);
        let result = if headless {
            let stopper = fake_stopper();
            poll_headless_steer(
                &fc,
                CliDriver::Cursor,
                Some(&stopper),
                &pending_arc("活儿", ""),
                "",
                "",
                "t",
            )
        } else {
            poll_transcript_steer(&fc, "t", None)
        };
        assert!(result.is_none());
        for expected in ["om_before", "om_new"] {
            assert!(
                matches!(rx.try_recv().unwrap(), ThreadMsg::Task(task) if task.message_id == expected)
            );
        }
    }
}

/// 已被撤回的 `/new` 在两条捕获路上都不打断:主循环会跳过它,打断只会让旧轮停下、新会话也不起。
#[test]
fn a_recalled_new_session_request_does_not_stop_the_turn() {
    for headless in [true, false] {
        let (tx, rx) = channel::<ThreadMsg>();
        tx.send(task_msg_with_id("om_before", "旧会话追加消息"))
            .unwrap();
        tx.send(new_session_msg("om_new")).unwrap();
        let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 30);
        checkpoint_followup(&fc, "om_before", "旧会话追加消息");
        checkpoint_followup(&fc, "om_new", "帮我看下这个实验");
        fc.wal.mark_recalled(&fc.agent_name, "om_new");
        let r = if headless {
            let stopper = fake_stopper();
            let pend = pending_arc("活儿", "");
            poll_headless_steer(&fc, CliDriver::Cursor, Some(&stopper), &pend, "", "", "t")
        } else {
            poll_transcript_steer(&fc, "t", None)
        };
        assert!(r.is_none(), "headless={headless}");
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(200)),
                Ok(ThreadMsg::Task(task)) if task.message_id == "om_before"
            ),
            "headless={headless}: 原样回填"
        );
    }
}

#[test]
fn config_reset_does_not_interrupt_current_turn() {
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(reset_msg(None)).unwrap();
    let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 10);
    let stopper = fake_stopper();
    let pend = pending_arc("当前回复", "");
    assert!(
        poll_headless_steer(&fc, CliDriver::Cursor, Some(&stopper), &pend, "", "", "t").is_none(),
        "配置切换的无卡片 Reset 只应排到下轮生效，不能中断当前回复"
    );
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(200)).unwrap(),
        ThreadMsg::Control {
            ctrl: Control::Reset,
            card_message_id: None,
            ..
        }
    ));
}

#[test]
fn explicit_teardown_is_requeued_before_normal_tasks() {
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(task_msg("已经排队的下一条消息")).unwrap();
    tx.send(reset_msg(Some("om_panel"))).unwrap();
    let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 10);
    let stopper = fake_stopper();
    let pend = pending_arc("当前回复", "");
    assert!(matches!(
        poll_headless_steer(&fc, CliDriver::Cursor, Some(&stopper), &pend, "", "", "t"),
        Some(crate::capture::TurnInterrupt::Stop)
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(200)).unwrap(),
        ThreadMsg::Control {
            ctrl: Control::Reset,
            card_message_id: Some(_),
            ..
        }
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(200)).unwrap(),
        ThreadMsg::Task(_)
    ));
}

#[test]
fn group_gone_is_requeued_before_other_teardowns() {
    let (tx, rx) = channel::<ThreadMsg>();
    let (ack, _ack_rx) = channel();
    let peers_dir = tempfile::tempdir().unwrap();
    let peers = crate::runtime::peers_store::PeersStore::new(peers_dir.path().join("peers"));
    requeue_teardown_first(
        &tx,
        vec![
            reset_msg(Some("om_panel")),
            task_msg("普通排队消息"),
            ThreadMsg::GoneTeardown {
                reason: summon_core::exit::ExitReason::GroupDisbanded,
                event_operator: "ou_owner".into(),
                peers,
                ack,
            },
        ],
    );

    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(200)).unwrap(),
        ThreadMsg::GoneTeardown { .. }
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(200)).unwrap(),
        ThreadMsg::Control {
            ctrl: Control::Reset,
            ..
        }
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(200)).unwrap(),
        ThreadMsg::Task(_)
    ));
}

#[test]
fn poll_transcript_button_interrupt_returns_stop() {
    // 任务4:claude 路同理——按钮 → Stop(真正的 Esc 由 run_turn 收到 Stop 后发,此处不杀)。
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(ThreadMsg::Interrupt).unwrap();
    let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 30);
    let r = poll_transcript_steer(&fc, "t", None);
    assert!(matches!(r, Some(crate::capture::TurnInterrupt::Stop)));
}

#[test]
fn poll_requeues_normal_message_no_interrupt() {
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(task_msg("普通消息,不该打断")).unwrap();
    let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 50);
    let stopper = fake_stopper();
    let pend = pending_arc("原始活儿", "已写一半");
    let merged = poll_headless_steer(&fc, CliDriver::Cursor, Some(&stopper), &pend, "", "", "t");
    assert!(merged.is_none(), "普通消息不打断");
    // 被偷看的普通消息应 requeue 回队尾,不丢。
    let back = rx.recv_timeout(Duration::from_millis(200)).unwrap();
    match back {
        ThreadMsg::Task(t) => assert_eq!(t.text, "普通消息,不该打断"),
        _ => panic!("应 requeue 同一条 Task"),
    }
}

#[test]
fn cursor_cli_session_steer_interrupts_and_resumes_by_default_policy() {
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(task_msg("补充：结果还要包含 beta")).unwrap();
    let (_wal_tmp, mut fc) = test_followup_ctx(&rx, tx.clone(), 0);
    fc.steer_cli_session = true;
    checkpoint_followup(&fc, "m1", "补充：结果还要包含 beta");
    let stopper = fake_stopper();
    let pending = pending_arc("PRIMER\n\n原始任务", "已生成 alpha");

    let request = match poll_headless_steer(
        &fc,
        CliDriver::Cursor,
        Some(&stopper),
        &pending,
        "原始任务",
        "已生成 alpha",
        "t",
    ) {
        Some(crate::capture::TurnInterrupt::Rerun(request)) => request,
        _ => panic!("Cursor CLI Session 的普通同源消息应立即转成可恢复重跑"),
    };
    assert_eq!(request.cause, crate::capture::RerunCause::CursorCliSteer);
    assert!(request.prompt.contains("原始任务"));
    assert!(request.prompt.contains("已生成 alpha"));
    assert!(request.prompt.contains("beta"));
    assert!(
        !request.prompt.contains("PRIMER"),
        "重跑只携带裸业务 prompt；首轮无 SID 时由下一次冷启统一注入 PRIMER，不能重复"
    );
    assert_eq!(request.message_ids, vec!["m1".to_string()]);
}

#[test]
fn cursor_cli_steer_never_leaks_into_acp_or_other_clis() {
    use crate::runtime::inflight::TurnShape;
    assert!(cursor_cli_steer_supported(
        true,
        CliDriver::Cursor,
        TurnShape::OwnedProcess
    ));
    assert!(
        !cursor_cli_steer_supported(true, CliDriver::Cursor, TurnShape::SharedSession),
        "ACP 是共享 RPC session，必须保持降级通道自己的能力边界"
    );
    assert!(!cursor_cli_steer_supported(
        true,
        CliDriver::Codex,
        TurnShape::OwnedProcess
    ));
    assert!(!cursor_cli_steer_supported(
        false,
        CliDriver::Cursor,
        TurnShape::OwnedProcess
    ));
}

#[test]
fn poll_bang_without_durable_wal_does_not_interrupt_or_execute() {
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(task_msg("!不能执行")).unwrap();
    let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 10);
    let stopper = fake_stopper();
    let pending = pending_arc("原任务", "");

    assert!(
        poll_headless_steer(
            &fc,
            CliDriver::Cursor,
            Some(&stopper),
            &pending,
            "",
            "",
            "t"
        )
        .is_none(),
        "缺 durable pending 时不得启动 steer 重跑"
    );
    assert!(fc.wal.scan_pending().is_empty());
}

#[test]
fn poll_kill_switch_off_requeues_bang() {
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(task_msg("!别打断我")).unwrap();
    let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 30);
    let stopper = fake_stopper();
    let pend = pending_arc("活儿", "");
    let merged = poll_headless_steer(&fc, CliDriver::Cursor, Some(&stopper), &pend, "", "", "t");
    assert!(merged.is_none(), "kill-switch 关:`!` 也不打断");
    // 仍 requeue 不丢。
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_ok());
}

#[test]
fn poll_no_killer_yet_does_not_interrupt() {
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(task_msg("!来不及")).unwrap();
    let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 30);
    let pend = pending_arc("活儿", "");
    // killer=None(spawn 还没回传 sid/句柄):这一刻无可打断。
    let merged = poll_headless_steer(&fc, CliDriver::Cursor, None, &pend, "", "", "t");
    assert!(merged.is_none(), "无 kill 句柄时不打断");
    // 消息没被消费(留在队列,下一轮 select 再看)。
    assert!(rx.try_recv().is_ok(), "无句柄时不应消费消息");
}

#[test]
fn poll_does_not_rotate_queued_messages() {
    // T2 回归:长 turn 里 poll 被反复调用,≥2 条排队的普通消息不得被轮转。旧实现逐 tick try_recv
    // 一条 + requeue 回队尾,奇数次调用会把 [A,B] 旋成 [B,A];整批抽干 + 原序回填后恒为 [A,B]。
    let (tx, rx) = channel::<ThreadMsg>();
    tx.send(task_msg("A")).unwrap();
    tx.send(task_msg("B")).unwrap();
    let (_wal_tmp, fc) = test_followup_ctx(&rx, tx.clone(), 10);
    let stopper = fake_stopper();
    let pend = pending_arc("活儿", "");
    for _ in 0..5 {
        // 5 次(奇数)——旧轮转实现到这步会留下 [B,A]。
        let m = poll_headless_steer(&fc, CliDriver::Cursor, Some(&stopper), &pend, "", "", "t");
        assert!(m.is_none(), "没有可 steer 的消息时不该动本轮");
    }
    let first = match rx.recv_timeout(Duration::from_millis(200)).unwrap() {
        ThreadMsg::Task(t) => t.text,
        _ => panic!("应为 Task"),
    };
    let second = match rx.recv_timeout(Duration::from_millis(200)).unwrap() {
        ThreadMsg::Task(t) => t.text,
        _ => panic!("应为 Task"),
    };
    assert_eq!(
        (first.as_str(), second.as_str()),
        ("A", "B"),
        "排队消息必须保原序,不得被 poll 轮转(T2)"
    );
}

#[test]
fn headless_stall_watchdog_decision() {
    // R7:静默超限 + 有 kill 句柄 + 未打断过 → 该打断;任一不满足 → 不动。
    let m = Duration::from_secs(240);
    assert!(headless_stalled(
        Duration::from_secs(240),
        m,
        true,
        false,
        false
    ));
    assert!(
        !headless_stalled(Duration::from_secs(239), m, true, false, false),
        "未超限不动"
    );
    assert!(
        !headless_stalled(Duration::from_secs(999), m, false, false, false),
        "无句柄打不了"
    );
    assert!(
        !headless_stalled(Duration::from_secs(999), m, true, true, false),
        "只打断一次"
    );
    assert!(
        !headless_stalled(Duration::from_secs(999), m, true, false, true),
        "问题卡正在等人时，长静默是合法状态"
    );
}

/// 真静默配置为240秒时，工具执行采用至少30分钟的有限上限。
#[test]
fn stall_bound_widens_only_while_a_tool_is_actually_running() {
    use crate::runtime::daemon::chat_thread::headless_stall_bound;
    assert_eq!(
        headless_stall_bound(Duration::from_secs(240), false, false),
        Duration::from_secs(240),
        "没有工具在飞 = 真静默,上界一秒都不能放宽"
    );
    let with_tool = headless_stall_bound(Duration::from_secs(240), true, false);
    assert!(
        with_tool > Duration::from_secs(240),
        "工具在飞时要放宽,否则长命令照旧被误杀:{with_tool:?}"
    );
    // 放宽不等于免疫:工具自己也会挂(命令在等 stdin、网络卡死),仍须有限。
    assert!(
        with_tool <= Duration::from_secs(60 * 60),
        "上界必须有限且不过分:{with_tool:?}"
    );

    // 分档只改「多久算静默」,不改其余四条判据 —— 长上界下同样要有句柄、没打断过、没在等人。
    assert!(!headless_stalled(
        Duration::from_secs(9999),
        with_tool,
        false,
        false,
        false
    ));
    assert!(!headless_stalled(
        Duration::from_secs(9999),
        with_tool,
        true,
        false,
        true
    ));
}

/// 新鲜协议心跳采用活跃执行档位，过期后采用本轮配置的真静默上限。
#[test]
fn engine_heartbeat_raises_the_stall_bound_but_never_disables_the_watchdog() {
    use crate::runtime::daemon::chat_thread::headless_stall_bound;
    assert_eq!(
        headless_stall_bound(Duration::from_secs(240), false, true),
        headless_stall_bound(Duration::from_secs(240), true, false),
        "心跳在 = 已知活着但没有可渲染产出,与工具在飞同档"
    );
    let alive = headless_stall_bound(Duration::from_secs(240), false, true);
    assert!(
        alive <= Duration::from_secs(60 * 60),
        "抬档不等于免疫,上界必须有限:{alive:?}"
    );
    // 心跳停了 → 回落严上界;长静默此刻就该收人。
    assert_eq!(
        headless_stall_bound(Duration::from_secs(240), false, false),
        Duration::from_secs(240)
    );
    assert!(headless_stalled(
        Duration::from_secs(240),
        headless_stall_bound(Duration::from_secs(240), false, false),
        true,
        false,
        false
    ));
    // 心跳仍在 → 同样的静默时长不到长上界,不收。
    assert!(!headless_stalled(
        Duration::from_secs(240),
        headless_stall_bound(Duration::from_secs(240), false, true),
        true,
        false,
        false
    ));
}

#[test]
fn rpc_steer_confirmation_controls_retry_and_failure_in_the_real_worker() {
    use crate::runtime::daemon::chat_thread::{run_headless_interruptible, TurnEngine};
    let _guard = crate::test_support::process_test_guard();
    let _quota = crate::capture::rpc_pool::default_codex_quota_test_guard();
    for (response, accepted, retry) in [
        (
            r#"printf '%s\n' '{"id":'"$id"',"result":{"turnId":"turn-1"}}'; printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}'"#,
            true,
            false,
        ),
        (
            r#"printf '%s\n' '{"id":'"$id"',"error":{"code":-32600,"message":"rejected"}}'"#,
            false,
            true,
        ),
        (":", false, false),
    ] {
        let (mut agent, tmp) = crate::test_support::agent_dir("codex", "");
        agent.name = format!("steer-probe-{}", uuid::Uuid::new_v4().simple());
        let cases = format!(
            r#"
    *'thread/start'*) printf '%s\n' '{{"id":'"$id"',"result":{{"thread":{{"id":"th-1","status":{{"type":"idle"}},"turns":[]}}}}}}' ;;
    *'turn/start'*) printf '%s\n' '{{"id":'"$id"',"result":{{"turn":{{"id":"turn-1","status":"inProgress"}}}}}}' ;;
    *'turn/steer'*) {response} ;;
    *'turn/interrupt'*) printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"th-1","turn":{{"id":"turn-1","status":"interrupted"}}}}}}' ;;
"#
        );
        let script = crate::test_support::write_fake_app_server(tmp.path(), &cases);
        agent.runtime.as_mut().unwrap().cli_cmd = Some(script.display().to_string());
        let chat = badge_chat(tmp.path(), "oc_steer_probe");
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let pool =
            crate::capture::rpc_pool::RpcPool::new(crate::runtime::inflight::InFlight::new());
        let engine = TurnEngine::Rpc {
            pool: pool.clone(),
            spec: Box::new(crate::capture::RpcTurnSpec {
                agent: agent.clone(),
                session: "fixture".into(),
                tag: tag.clone(),
                chat_id: chat.chat_id.into(),
            }),
        };
        let (tx, rx) = channel();
        let (_wal, mut fc) = test_followup_ctx(&rx, tx.clone(), 0);
        fc.steer_rpc = true;
        checkpoint_followup(&fc, "m1", "followup");
        let frontend = EmitterFrontend::new();
        let result = run_headless_interruptible(
            engine,
            CliDriver::Codex,
            "fixture",
            "fixture",
            None,
            &crate::capture::TurnCtx::default(),
            &frontend,
            &chat,
            chat.chat_id.into(),
            chat.chat_type.into(),
            &agent.name,
            &pending_arc("fixture", ""),
            Arc::new(move |stopper| {
                let stopper = stopper.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !stopper.has_active_turn() {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    tx.send(task_msg("followup")).unwrap();
                });
            }),
            Arc::new(|_| {}),
            Arc::new(|_| crate::runtime::inflight::FinishDisposition::Completed),
            &fc,
            Duration::from_secs(10),
            false,
            10,
            false,
            false,
            false,
            false,
            None,
            None,
            None,
            None,
            Default::default(),
            &test_outbox(tmp.path()),
        );
        assert!(!result.detached);
        assert!(
            !result.interrupted,
            "结果不明必须走失败通知，不能用显式暂停跳过通知"
        );
        assert_eq!(result.rerun.is_some(), retry);
        if retry {
            let rerun = result.rerun.unwrap();
            assert_eq!(rerun.message_ids, vec!["m1"]);
            assert_eq!(rerun.cause, crate::capture::RerunCause::NativeSteerFallback);
        } else {
            assert_eq!(
                result.steered_ids,
                vec!["m1"],
                "成功或失败通知都必须携带已 started 消息账目"
            );
            assert_eq!(result.out.is_ok(), accepted);
            if !accepted {
                assert!(result.out.unwrap_err().to_string().contains("可能已被处理"));
            }
        }
        let requests =
            std::fs::read_to_string(crate::test_support::fake_rpc_server_log(tmp.path())).unwrap();
        let methods: Vec<String> = requests
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|v| v["method"].as_str().map(str::to_string))
            .collect();
        assert_eq!(methods.iter().filter(|m| *m == "turn/steer").count(), 1);
        assert_eq!(
            methods.iter().filter(|m| *m == "turn/interrupt").count(),
            usize::from(!accepted)
        );
        pool.evict(&agent.name, &tag);
    }
}

/// 磁盘配置经真实RPC执行入口决定到期或完成，provider使用隔离假服务。
#[test]
fn configured_headless_watchdog_controls_the_real_rpc_worker() {
    use crate::runtime::daemon::chat_thread::{run_headless_interruptible, TurnEngine};
    let _guard = crate::test_support::process_test_guard();
    let _quota = crate::capture::rpc_pool::default_codex_quota_test_guard();
    for (seconds, timed_out) in [(1, true), (1800, false)] {
        let (mut agent, tmp) = crate::test_support::agent_dir("codex", "");
        agent.name = format!("watchdog-probe-{}", uuid::Uuid::new_v4().simple());
        let cases = r#"
    *'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-1","status":{"type":"idle"},"turns":[]}}}' ;;
    *'turn/start'*) printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-1","status":"inProgress"}}}';
        if [ "$FIXTURE_HANG" != 1 ]; then
        sleep 2;
        printf '%s\n' '{"method":"item/completed","params":{"threadId":"th-1","turnId":"turn-1","item":{"type":"agentMessage","id":"answer","text":"done","phase":"final_answer"}}}';
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed"}}}'; fi ;;
    *'turn/interrupt'*) printf '%s\n' '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"aborted"}}}' ;;
"#;
        // 正常夹具跨过一秒停滞线，验证长配置确实进入 worker；超时夹具等协议打断收尾。
        let cases = cases.replace("$FIXTURE_HANG", if timed_out { "1" } else { "0" });
        let script = crate::test_support::write_fake_app_server(tmp.path(), &cases);
        agent.runtime.as_mut().unwrap().cli_cmd = Some(script.display().to_string());
        assert_eq!(agent.cli_cmd(), script.display().to_string());
        let chat = badge_chat(tmp.path(), "oc_watchdog_probe");
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let pool =
            crate::capture::rpc_pool::RpcPool::new(crate::runtime::inflight::InFlight::new());
        let engine = TurnEngine::Rpc {
            pool: pool.clone(),
            spec: Box::new(crate::capture::RpcTurnSpec {
                agent: agent.clone(),
                session: "fixture".into(),
                tag: tag.clone(),
                chat_id: chat.chat_id.into(),
            }),
        };
        let (tx, rx) = channel();
        let (_wal, fc) = test_followup_ctx(&rx, tx, 1);
        let frontend = EmitterFrontend::new();
        let mut messaging = summon_core::config::builtin_messaging();
        messaging.apply(&serde_yaml::from_str(&format!("headless_stall_secs: {seconds}")).unwrap());
        let start = std::time::Instant::now();
        let result = run_headless_interruptible(
            engine,
            CliDriver::Codex,
            "fixture",
            "fixture",
            None,
            &crate::capture::TurnCtx::default(),
            &frontend,
            &chat,
            chat.chat_id.into(),
            chat.chat_type.into(),
            &agent.name,
            &pending_arc("fixture", ""),
            Arc::new(|_| {}),
            Arc::new(|_| {}),
            Arc::new(|_| crate::runtime::inflight::FinishDisposition::Completed),
            &fc,
            messaging.headless_stall(),
            false,
            10,
            false,
            false,
            false,
            false,
            None,
            None,
            None,
            None,
            Default::default(),
            &test_outbox(tmp.path()),
        );
        assert_eq!(result.out.as_ref().unwrap().timed_out, timed_out);
        assert!(start.elapsed() < Duration::from_secs(10));
        let requests =
            std::fs::read_to_string(crate::test_support::fake_rpc_server_log(tmp.path())).unwrap();
        assert!(requests.contains("turn/start"));
        assert!(!result.detached);
        if !timed_out {
            assert_eq!(result.out.unwrap().reply.as_deref(), Some("done"));
        }
        pool.evict(&agent.name, &tag);

        // 从磁盘agent配置到实际provider入口，核对调用层采用解析的上限。
        agent.messaging =
            Some(serde_yaml::from_str(&format!("headless_stall_secs: {seconds}")).unwrap());
        let mut manifest: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(agent.manifest_path()).unwrap()).unwrap();
        manifest["name"] = agent.name.clone().into();
        manifest["runtime"]["cli_cmd"] = script.display().to_string().into();
        manifest["messaging"] = serde_yaml::to_value(agent.messaging.as_ref().unwrap()).unwrap();
        std::fs::write(
            agent.manifest_path(),
            serde_yaml::to_string(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let inflight = crate::runtime::inflight::InFlight::new();
        let scope = feishu_storage_scope(&agent).key;
        let session = Session::with_name(&agent, format!("soul-{}-{tag}", agent.name), 20, 20);
        let (tx, rx) = channel();
        let (_wal, mut fc) = test_followup_ctx(&rx, tx, 1);
        fc.agent_name = agent.name.clone();
        fc.delivery_scope = scope.clone();
        let result = crate::runtime::daemon::chat_thread::stream_turn_headless(
            CliDriver::Codex,
            &agent,
            &scope,
            &session,
            &frontend,
            chat.chat_id,
            chat.chat_type,
            "fixture",
            &inflight,
            Vec::new(),
            Some(&fc),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            ResolvedSettings::resolve(&messaging, &ChatOverrides::default()),
        );
        assert_eq!(
            result,
            if timed_out {
                TurnResult::Failed
            } else {
                TurnResult::Done
            }
        );
        crate::capture::rpc_pool::evict_shared(&agent.name, &tag);
    }
}

#[test]
fn configured_headless_stall_keeps_30_minute_and_longer_deadlines() {
    use crate::runtime::daemon::chat_thread::headless_stall_bound;
    let mut m = summon_core::config::builtin_messaging();
    m.apply(&serde_yaml::from_str("headless_stall_secs: 1800").unwrap());
    let bound = headless_stall_bound(m.headless_stall(), false, false);
    for seconds in [240, 1799] {
        assert!(!headless_stalled(
            Duration::from_secs(seconds),
            bound,
            true,
            false,
            false
        ));
    }
    assert!(headless_stalled(
        Duration::from_secs(1800),
        bound,
        true,
        false,
        false
    ));
    m.apply(&serde_yaml::from_str("headless_stall_secs: 3600").unwrap());
    for (tool, engine) in [(false, false), (true, false), (false, true)] {
        assert_eq!(
            headless_stall_bound(m.headless_stall(), tool, engine),
            Duration::from_secs(3600)
        );
    }
}

/// 活性状态的跨 tick 序列语义:前进 → alive;冻结在窗口内 → 仍 alive;冻结满窗 → dead;
/// 回退不算前进;再前进 → 复活。状态必须跨调用存活 —— 每次构造新实例或中途归零都会让
/// 「冻结满窗 → dead」这半条失效,静默上界恒为长档。
#[test]
fn engine_liveness_goes_dead_only_after_a_full_quiet_window() {
    use crate::runtime::daemon::chat_thread::EngineLiveness;
    let t0 = std::time::Instant::now();
    let s = Duration::from_secs;
    let mut live = EngineLiveness::new(t0);
    assert!(live.observe(3, t0 + s(300)), "计数前进 → alive");
    assert!(
        live.observe(3, t0 + s(300 + 239)),
        "冻结未满一个窗口 → 仍 alive"
    );
    assert!(
        !live.observe(3, t0 + s(300 + 240)),
        "冻结满窗 → dead,240s 档回归(真挂死照旧被收)"
    );
    assert!(
        !live.observe(1, t0 + s(300 + 241)),
        "计数回退(别的实例的读数)→ 不算前进"
    );
    assert!(live.observe(4, t0 + s(300 + 242)), "再前进 → 复活");

    let mut fresh = EngineLiveness::new(t0);
    assert!(!fresh.observe(0, t0 + s(239)), "没有协议心跳不证明引擎存活");
    assert!(
        !fresh.observe(0, t0 + s(240)),
        "整轮没有任何心跳 → 首窗过后即 dead"
    );
}

/// 接线守卫:纯函数与序列测试杀不掉「调用点传死值 / 换喂源 / 状态移进循环」这类变异 ——
/// 与 [`live_card_turn_progress_always_carries_fence_token`] 同一失效形状(函数对、接线断,
/// 全绿)。逐段钉死这条链:beats 喂源是把手的活性计数、engine_alive 来自轮级
/// `EngineLiveness` 的观察、状态一轮构造一次、上界判据的消费点唯一且非字面量、判决入参
/// 是算出的 `stall`、静默计时 `last_update` 的喂点恰为已知集合。
///
/// 局限与样板相同:去注释 + 去空白的 substring 扫描,只管 chat_thread.rs 这一个文件。
#[test]
fn stall_bound_call_site_wires_engine_liveness_not_a_literal() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime/daemon/chat_thread.rs");
    let text = std::fs::read_to_string(&path).expect("读 chat_thread.rs 失败");
    let code: String = text
        .lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let squeezed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    // 非空自检:扫描器必须真读到定义,否则路径写错会让本测试恒绿。
    assert!(
        squeezed.contains("fnheadless_stall_bound("),
        "扫描器失效:chat_thread.rs 里找不到 headless_stall_bound 定义"
    );
    assert_eq!(
        squeezed
            .matches(
                "headless_stall_bound(silent_stall,tool_running||subagent_running,engine_alive,)"
            )
            .count(),
        1,
        "上界判据必须恰有一个消费点,且 engine_alive 实参不是字面量"
    );
    assert!(squeezed.contains("lettool_running=activity.has_tools();"));
    assert!(squeezed.contains("letsubagent_running=activity.has_subagents();"));
    assert!(squeezed.contains("TurnUpdate::Event(ev)=>{activity.observe(&ev);"));
    assert_eq!(
        squeezed
            .matches("letmutactivity=super::headless_activity::HeadlessActivity::default();")
            .count(),
        1
    );
    assert_eq!(
        squeezed.matches("engine_liveness.observe(").count(),
        1,
        "engine_alive 必须来自轮级 EngineLiveness 的观察,不是就地比较"
    );
    assert!(
        squeezed
            .contains("letengine_alive=engine_liveness.observe(beats,std::time::Instant::now());"),
        "engine_alive 必须整条来自轮级观察 —— 附加任何恒真项等于把看门狗关掉"
    );
    assert_eq!(
        squeezed
            .matches("letbeats=killer.as_ref().map_or(0,|k|k.liveness_beats());")
            .count(),
        1,
        "beats 的喂源必须是把手的活性计数 —— 换成任何本地单调量都会让 engine_alive 恒真"
    );
    assert_eq!(
        squeezed
            .matches("headless_stalled(last_update.elapsed(),stall,")
            .count(),
        1,
        "判决入参必须是算出的 stall —— 换成字面量等于把分档整个旁路"
    );
    assert_eq!(
        squeezed.matches("EngineLiveness::new(").count(),
        1,
        "EngineLiveness 只在轮头构造一次 —— 循环内重造 = 状态归零 = 上界恒长档"
    );
    let liveness = squeezed
        .find("letmutengine_liveness=EngineLiveness::new(std::time::Instant::now());")
        .expect("轮级活性状态声明");
    let turn_loop = squeezed.find("'turn:loop{").expect("无头轮循环");
    assert!(
        liveness < turn_loop,
        "活性状态必须在轮循环外构造：移进循环体会在每次迭代重置，上界恒长档，240s 档失效"
    );
    assert_eq!(
        squeezed
            .matches("last_update=std::time::Instant::now()")
            .count(),
        4,
        "静默计时只由 TurnUpdate 与 waiting 翻转喂(声明 + 3 处既知喂点)—— 新增喂点\
         (例如拿引擎心跳刷 last_update)会让看门狗永不触发,必须来这里显式过守卫"
    );
}

// --- WP5: bot peers 上下文注入 ---

use summon_core::peers::{compose_peers_hot_update, compose_peers_primer, Peer};

/// `/introduce` 后面交代的分工必须进注入文本 —— 用户发这条命令主要就是为了说清谁干什么,
/// 只播报 peer 名单等于把意图丢了(真机:「星光开发、蓝翼飞龙审核」曾被整段丢弃)。
#[test]
fn peers_note_reaches_both_hot_update_and_primer() {
    let peers = vec![
        Peer {
            open_id: "ou_1".into(),
            name: "星光".into(),
            ..Default::default()
        },
        Peer {
            open_id: "ou_2".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        },
    ];
    let note = "星光现在是开发,蓝翼飞龙做代码审核";

    let hot = compose_peers_hot_update(&peers, Some(note));
    assert!(hot.contains(note), "热更新丢了分工说明: {hot}");

    let primer = compose_peers_primer(&peers, &HashMap::new(), Some(note));
    assert!(primer.contains(note), "冷启 PRIMER 丢了分工说明: {primer}");

    // 没写说明(空串/None)时不留空标题
    let bare = compose_peers_hot_update(&peers, Some("   "));
    assert!(!bare.contains("分工"), "空说明不该产出分工小节: {bare}");
}

/// note 随 peers 一起落盘并被读回:冷启走的是文件而不是当时那条飞书消息,存不住等于没修。
#[test]
fn peers_store_persists_note_and_keeps_it_on_later_non_intro_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::runtime::peers_store::PeersStore::new(dir.path());
    let tag = chat_tag("group", "oc_note");
    let peers = vec![Peer {
        open_id: "ou_1".into(),
        name: "星光".into(),
        ..Default::default()
    }];

    store
        .add_peers(&tag, peers.clone(), Some("星光开发"))
        .expect("introduce 写入应成功");
    assert_eq!(
        store.load(&tag).and_then(|f| f.note).as_deref(),
        Some("星光开发")
    );

    // 后续入站消息顺带记身份(intro=None)不得冲掉已有分工
    store
        .add_peers(
            &tag,
            vec![Peer {
                open_id: "ou_2".into(),
                name: "蓝翼飞龙".into(),
                ..Default::default()
            }],
            None,
        )
        .expect("身份记录写入应成功");
    let loaded = store.load(&tag).expect("应能读回");
    assert_eq!(
        loaded.note.as_deref(),
        Some("星光开发"),
        "note 被非介绍写入冲掉了"
    );
    assert_eq!(loaded.peers.len(), 2);
}

/// PeersStore round-trip:写后能读到一致的 peers 列表(冷启注入的数据来源)。
#[test]
fn peers_store_round_trip_for_cold_start_injection() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::runtime::peers_store::PeersStore::new(dir.path().join("peers"));

    let peers = vec![
        Peer {
            open_id: "ou_1".into(),
            name: "星光".into(),
            ..Default::default()
        },
        Peer {
            open_id: "ou_2".into(),
            name: "arcadia".into(),
            ..Default::default()
        },
    ];
    store
        .add_peers("g1234abcd", peers.clone(), None)
        .expect("add_peers 应成功");

    let loaded = store.load("g1234abcd").expect("应能读到刚写入的 peers");
    assert_eq!(loaded.peers.len(), 2);
    assert_eq!(loaded.peers[0].name, "星光");

    // compose_peers_primer 用读到的 peers 应能生成注入文本
    let alias_map = std::collections::HashMap::new();
    let primer = compose_peers_primer(&loaded.peers, &alias_map, None);
    assert!(primer.contains("星光"));
    assert!(primer.contains("arcadia"));
}

#[test]
fn exit_receipt_renders_card_for_each_reason() {
    // 卡片化后六种 reason 都产出 schema 2.0 的 wire(不再是裸 markdown),
    // 且标题随 reason 区分——deliver_notify 是唯一投递函数体,一处改动覆盖全部退出路径。
    for (reason, title) in [
        (summon_core::exit::ExitReason::ManualKill, "会话已关闭"),
        (summon_core::exit::ExitReason::PanelClean, "会话已退出"),
        (summon_core::exit::ExitReason::PanelReset, "会话已归档"),
        (summon_core::exit::ExitReason::GroupDisbanded, "群已解散"),
        (summon_core::exit::ExitReason::BotRemoved, "已退出群聊"),
        (
            summon_core::exit::ExitReason::ShutdownInterrupt,
            "会话被打断",
        ),
    ] {
        let card = summon_core::feishu::card_session_exited(
            reason,
            "arcadia",
            "sample开发群2",
            "07-27 08:40",
            "本会话已被关闭。",
            None,
        );
        assert!(card.contains(r#""schema":"2.0""#), "{title} 应为 card 2.0");
        assert!(card.contains(title), "标题应含 {title}");
    }
}

#[test]
fn legacy_notify_event_without_card_falls_back_to_text() {
    // daemon 升级前入队的事件没有 rendered_card。投递侧必须回退 text 路径而不是发空卡片,
    // 否则升级会把已入队的回执变成一张空白卡。
    let ev = summon_core::outbox::build_notify_event(
        1_700_000_000_000,
        "arcadia",
        "scope-x",
        "oc_1",
        "group",
        None,
        None,
        summon_core::exit::ExitReason::ManualKill,
        "旧文案".into(),
        String::new(),
        "abcd1234",
    );
    match ev.kind {
        summon_core::outbox::OutboxKind::NotifyChat {
            rendered_card,
            text,
            ..
        } => {
            assert!(rendered_card.is_empty(), "空 card 代表走文本回退");
            assert_eq!(text, "旧文案");
        }
        _ => panic!("应为 NotifyChat"),
    }
}

#[test]
fn exit_chat_label_falls_back_to_tag_when_no_chat_name() {
    // 卡片「会话」列不能空着让人不知道说的是哪个会话:群名优先,缺失/空白回退稳定短标识。
    assert_eq!(
        exit_chat_label(Some("sample开发群2"), "group", "oc_1"),
        "sample开发群2"
    );
    assert_eq!(
        exit_chat_label(Some("   "), "group", "oc_1"),
        chat_tag("group", "oc_1"),
        "空白群名应回退 tag"
    );
    assert_eq!(
        exit_chat_label(None, "group", "oc_1"),
        chat_tag("group", "oc_1")
    );
}

#[test]
fn writing_chat_meta_clears_exit_mark() {
    // 会话重新开始(有新入站写 .meta)必须清掉上一次退出的标记,否则同一 chat 第二次 kill
    // 会被旧标记静默吞掉——这正是标记「按 chat 记」时最容易踩的坑。
    let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
    crate::session::exit_mark::mark(&agent, "group", "oc_reuse");
    assert!(crate::session::exit_mark::exists(
        &agent, "group", "oc_reuse"
    ));

    write_chat_meta(
        &agent,
        "soul-test-greuse",
        "oc_reuse",
        "group",
        Some("sample开发群2"),
        "ou_sender",
        Some("ou_creator"),
    );
    assert!(
        !crate::session::exit_mark::exists(&agent, "group", "oc_reuse"),
        "入站写 .meta 后标记应被清除"
    );
}

#[test]
fn idle_scan_skips_synthesizing_when_exit_mark_present() {
    // CLI 已入队回执并写了标记 → idle-scan 探测到 tmux 消失时必须让路,否则同一次退出发两条。
    // 与 run_idle_scan_silently_exits_and_reports_externally_killed_transcript 同构,
    // 只差「先写标记」与「断言不合成」两处。
    let tmp = std::env::temp_dir().join(format!("summon-markskip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let mut agent: Agent =
        serde_yaml::from_str("name: starlight\nruntime:\n  cli: claude\n").unwrap();
    agent.dir = tmp.join("agents").join(&agent.name);
    std::fs::create_dir_all(&agent.dir).unwrap();
    std::fs::create_dir_all(agent.state_dir()).unwrap();
    // 关键前置:标记已存在(模拟 CLI kill 已入队并负责投递)。
    crate::session::exit_mark::mark(&agent, "p2p", "oc1");

    let migrated = ensure_idle_prompt_marker_migration(&agent).unwrap();
    let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
    let messaging = builtin_messaging_for_test();
    let (self_tx, self_rx) = channel::<DispatchMsg>();
    let (tx, rx) = channel::<ThreadMsg>();
    let mut chats: HashMap<String, ChatHandle> = HashMap::new();
    chats.insert(
        "oc1".into(),
        ChatHandle {
            tx,
            last_activity: std::time::SystemTime::now(),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: format!("soul-markskip-not-real-{}", std::process::id()),
            tmux_seen_live: true, // 曾观测 live
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    );
    run_idle_scan(&mut chats, &agent, &messaging, &self_tx, &fc, &migrated);
    // SilentExit 必须照发:否则残留 chat 线程与 mirror watcher 永久泄漏(PF1),
    // 让路只是「不再合成回执」,不是「什么都不做」。
    match rx.try_recv() {
        Ok(ThreadMsg::SilentExit) => {}
        _ => panic!("即使让路也必须投 SilentExit(PF1)"),
    }
    assert!(
        self_rx.try_recv().is_err(),
        "有标记时不应合成 ChatExited(CLI 已负责)"
    );
    // 让路后标记应被清掉,避免它长期滞留影响下一次判定。
    assert!(!crate::session::exit_mark::exists(&agent, "p2p", "oc1"));
    let _ = std::fs::remove_dir_all(&tmp);
}

// ————————————————————————————————————————————————————————————————————————
// 卡片授权动作 → operate-gate → teardown 的接线(此前只有「拒绝」路径被人在飞书里点验过,
// 「放行 → 真的调到 do_teardown」这段既无测试也无真机覆盖:改任何一环全量门禁照样绿)。
//
// 穿真 dispatcher:tempdir 根 + `DispatcherStores{support_root: Some(..)}` + dry_run FeishuClient
// (同 go_sidecar_and_rust_daemon_round_trip_protocol_over_unix_socket 的隔离骨架)。chat worker
// 由「启动续跑扫描」拉起:claude=Transcript,`resume_pending_turn` 只清 pending 就返回,**不起 CLI**;
// chat_id 每次跑都唯一 ⇒ tmux 会话名唯一 ⇒ `has_session()` 恒 false ⇒ 绝不触碰任何真实 tmux。
// ————————————————————————————————————————————————————————————————————————

struct CardGateFixture {
    agent: Agent,
    tx: Sender<DispatchMsg>,
    dispatcher: Option<thread::JoinHandle<()>>,
    _tmp: tempfile::TempDir,
}

impl CardGateFixture {
    /// dispatcher 侧解析会话名的同一规则(无 `.meta` 时按 chat 派生,非 owner)。
    fn session_of(&self, chat_type: &str, chat_id: &str) -> String {
        self.agent.tmux_name_for_chat(chat_type, chat_id, false)
    }

    /// resume 记录在不在 = `do_teardown(reset)` 的可观测事实:reset 清、clean 留。
    fn resume_kept(&self, chat_type: &str, chat_id: &str) -> bool {
        crate::session::resume_store::read_id(
            &self.agent,
            &self.session_of(chat_type, chat_id),
            "claude",
        )
        .is_some()
    }

    fn seed_resume(&self, chat_type: &str, chat_id: &str) {
        crate::session::resume_store::write(
            &self.agent,
            &self.session_of(chat_type, chat_id),
            "claude",
            "sid-before-click",
            None,
            None,
        );
    }

    fn outbound(&self, chat_type: &str, chat_id: &str) -> Vec<serde_json::Value> {
        let path = self
            .agent
            .state_dir()
            .join("threads")
            .join(chat_tag(chat_type, chat_id))
            .join("outbound.ndjson");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// 归档里那张「会话已退出」终态卡的正文(有它 = Clean/Reset 分支真的走完了 teardown 收尾)。
    fn closed_card_text(&self, chat_type: &str, chat_id: &str) -> Option<String> {
        self.outbound(chat_type, chat_id)
            .into_iter()
            .find(|entry| entry["kind"] == "card_update")
            .map(|entry| entry["payload"].to_string())
    }

    fn denial_notice(&self, chat_type: &str, chat_id: &str) -> bool {
        self.outbound(chat_type, chat_id).iter().any(|entry| {
            entry["kind"] == "markdown_card"
                && entry["payload"]["markdown"]
                    .as_str()
                    .is_some_and(|text| text.contains("仅授权用户可执行此操作"))
        })
    }

    /// 模拟一次飞书卡片按钮回调(会话控制面板的「清理 / 重置」)。
    fn click(&self, chat_type: &str, chat_id: &str, action: &str, operator: &str) {
        self.tx
            .send(DispatchMsg::Card(CardActionMsg {
                chat_id: chat_id.to_string(),
                message_id: format!("om_panel_{action}_{chat_id}"),
                operator_open_id: operator.to_string(),
                event_id: format!("card-{action}-{chat_id}-{operator}"),
                value: serde_json::json!({
                    "action": action,
                    "chat": chat_tag(chat_type, chat_id),
                }),
                wal_id: None,
                wal_replayed: false,
            }))
            .expect("dispatcher 应在线");
    }

    fn wait_for(&self, label: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "等待超时:{label}");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for CardGateFixture {
    fn drop(&mut self) {
        let _ = self.tx.send(DispatchMsg::Shutdown);
        if let Some(handle) = self.dispatcher.take() {
            let _ = handle.join();
        }
    }
}

/// 起一个隔离 dispatcher,并让它在启动扫描里为每个 `(chat_id, chat_type)` 拉起真 chat worker。
fn spawn_card_gate_dispatcher(extra_yaml: &str, chats: &[(&str, &str)]) -> CardGateFixture {
    let (agent, tmp) = crate::test_support::agent_dir("claude", extra_yaml);
    let scope = feishu_storage_scope(&agent).key;
    let inbox = crate::runtime::inbox_wal::InboxWal::new(tmp.path().join("inbox"));
    for (chat_id, chat_type) in chats {
        let message_id = format!("om_seed_{chat_id}");
        let pending = summon_core::pending::PendingTurn {
            kind: summon_core::pending::PendingTurnKind::Chat,
            delivery_scope: scope.clone(),
            agent_name: agent.name.clone(),
            inbox_message_ids: vec![message_id.clone()],
            resume_authorized: true,
            chat_id: (*chat_id).to_string(),
            chat_type: (*chat_type).to_string(),
            cli: "claude".into(),
            prompt: "上一轮被打断".into(),
            session_id: None,
            partial: String::new(),
        };
        crate::runtime::inflight::write_pending(
            &agent.state_dir(),
            &chat_tag(chat_type, chat_id),
            &pending,
        )
        .unwrap();
        let inbound = Inbound {
            anchor_message_id: None,
            chat_id: (*chat_id).to_string(),
            chat_type: Some((*chat_type).to_string()),
            text: "上一轮被打断".into(),
            sender: "ou_owner".into(),
            sender_union_id: String::new(),
            sender_type: String::new(),
            message_id: message_id.clone(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms: 1,
            replayed: false,
            reattach: false,
        };
        inbox.append(&agent.name, &scope, &inbound).unwrap();
        inbox
            .mark_started(&agent.name, &scope, &message_id)
            .unwrap();
    }

    let (tx, rx) = channel::<DispatchMsg>();
    let core = Arc::new(crate::runtime::core::Core::for_routes(HashMap::new()));
    let stores = super::dispatcher::DispatcherStores {
        bridge: crate::bridge::store::BridgeStore::new(tmp.path().join("bridge")),
        outbox: crate::runtime::outbox_store::OutboxStore::new(tmp.path().join("outbox")),
        inbox,
        card_actions: crate::runtime::card_action_wal::CardActionWal::new(
            tmp.path().join("card-wal"),
        ),
        support_root: Some(tmp.path().join("dispatcher-support")),
    };
    let client = Arc::new(FeishuClient::new(
        "feishu.cn",
        "test-app".into(),
        "test-secret".into(),
        true,
    ));
    let dispatcher = {
        let agent = agent.clone();
        let epoch = super::FeishuEpochIdentity::capture(&agent);
        let tx = tx.clone();
        thread::spawn(move || {
            super::dispatcher::agent_dispatcher_with_stores(
                agent,
                20,
                20,
                client,
                epoch,
                rx,
                tx,
                core,
                crate::runtime::inflight::InFlight::new(),
                stores,
            )
        })
    };
    CardGateFixture {
        agent,
        tx,
        dispatcher: Some(dispatcher),
        _tmp: tmp,
    }
}

/// 每次跑都唯一的 chat_id:tag/tmux 名随之唯一,`has_session()` 恒 false ⇒ 测试永不触碰真实 tmux。
fn unique_chat(prefix: &str) -> String {
    format!("oc_{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// 陈旧提示从 dispatcher 主循环真的发出去:名单里的人私聊晚到被丢,归档里有提示;名单外的人、
/// 以及群里证不出 bot 身份的(dry-run 客户端取不到)都不出声。
#[test]
fn stale_inbound_notice_reaches_only_senders_we_would_answer() {
    let fx = spawn_card_gate_dispatcher(
        "feishu:\n  allowed_users:\n    - ou_owner\nmessaging:\n  session_mirror: false\n",
        &[],
    );
    let stranger_chat = unique_chat("stale_stranger");
    let group_chat = unique_chat("stale_group");
    let owner_chat = unique_chat("stale_owner");
    let eleven_minutes_ago = now_unix_ms() as i64 - 11 * 60_000;
    let late = |chat_id: &str, chat_type: &str, sender: &str| Inbound {
        anchor_message_id: None,
        chat_id: chat_id.to_string(),
        chat_type: Some(chat_type.to_string()),
        text: "@_user_1 帮我看看".into(),
        sender: sender.to_string(),
        sender_union_id: String::new(),
        sender_type: "user".into(),
        message_id: format!("om_late_{chat_id}"),
        parent_id: None,
        thread_id: None,
        root_id: None,
        mentions: vec![summon_core::ipc::Mention {
            key: "@_user_1".into(),
            open_id: "ou_bot".into(),
            name: "bot".into(),
            ..Default::default()
        }],
        resources: Vec::new(),
        create_time_ms: eleven_minutes_ago,
        replayed: false,
        reattach: false,
    };
    for (chat_id, chat_type, sender) in [
        (&stranger_chat, "p2p", "ou_stranger"),
        (&group_chat, "group", "ou_owner"),
        (&owner_chat, "p2p", "ou_owner"),
    ] {
        fx.tx
            .send(DispatchMsg::Inbound(late(chat_id, chat_type, sender)))
            .expect("dispatcher 应在线");
    }
    let is_notice = |entry: &serde_json::Value| {
        entry["kind"] == "markdown_card"
            && entry["payload"]["markdown"]
                .as_str()
                .is_some_and(|text| text.contains("已判为过期"))
    };
    fx.wait_for("名单里的人收到过期提示", || {
        fx.outbound("p2p", &owner_chat).iter().any(is_notice)
    });
    // dispatcher 单线程按序处理:最后一条已有结果,前两条必然早已判完。
    assert!(
        !fx.outbound("p2p", &stranger_chat).iter().any(is_notice),
        "名单外的人不出声"
    );
    assert!(
        !fx.outbound("group", &group_chat).iter().any(is_notice),
        "证不出 bot 身份的群消息不出声"
    );
}

/// 防环熔断只数 bot:被 introduce 的同事一分钟连发五条照常处理,bot peer 第四条就熔断;
/// peer 发来消息时,盘上补记它的身份。用 `/term` 当探针 —— dispatcher 就地回复,不起 worker。
#[test]
fn colleague_peers_burst_freely_while_bot_peers_trip_the_loop_breaker() {
    let fx = spawn_card_gate_dispatcher(
        "feishu:\n  allowed_users:\n    - ou_owner\n  operators:\n    - ou_owner\n\
messaging:\n  session_mirror: false\n",
        &[],
    );
    let peers = crate::runtime::peers_store::PeersStore::new(
        fx._tmp
            .path()
            .join("dispatcher-support")
            .join("peers")
            .join(feishu_storage_scope(&fx.agent).key),
    );
    let human_chat = unique_chat("peer_human");
    let bot_chat = unique_chat("peer_bot");
    for (chat_id, open_id) in [(&human_chat, "ou_colleague"), (&bot_chat, "ou_peer_bot")] {
        peers
            .add_peers(
                &chat_tag("p2p", chat_id),
                vec![summon_core::peers::Peer {
                    open_id: open_id.into(),
                    name: open_id.into(),
                    kind: summon_core::peers::PeerKind::Unknown,
                }],
                Some(""),
            )
            .unwrap();
    }
    let term = |chat_id: &str, sender: &str, sender_type: &str, n: usize| Inbound {
        anchor_message_id: None,
        chat_id: chat_id.to_string(),
        chat_type: Some("p2p".to_string()),
        text: "/term".into(),
        sender: sender.to_string(),
        sender_union_id: String::new(),
        sender_type: sender_type.into(),
        message_id: format!("om_term_{chat_id}_{n}"),
        parent_id: None,
        thread_id: None,
        root_id: None,
        mentions: Vec::new(),
        resources: Vec::new(),
        create_time_ms: now_unix_ms() as i64,
        replayed: false,
        reattach: false,
    };
    for n in 0..5 {
        fx.tx
            .send(DispatchMsg::Inbound(term(
                &human_chat,
                "ou_colleague",
                "user",
                n,
            )))
            .expect("dispatcher 应在线");
    }
    for n in 0..5 {
        fx.tx
            .send(DispatchMsg::Inbound(term(
                &bot_chat,
                "ou_peer_bot",
                "app",
                n,
            )))
            .expect("dispatcher 应在线");
    }
    let count = |chat_id: &str, needle: &str| {
        fx.outbound("p2p", chat_id)
            .iter()
            .filter(|entry| {
                entry["payload"]["markdown"]
                    .as_str()
                    .is_some_and(|text| text.contains(needle))
            })
            .count()
    };
    fx.wait_for("bot peer 触发熔断", || {
        count(&bot_chat, "检测到 bot 间") == 1
    });
    // dispatcher 单线程按序处理:bot 那组已有结果,同事那组必然早已判完。
    assert_eq!(
        count(&human_chat, "仅授权用户可打开终端"),
        5,
        "同事五条都照常处理"
    );
    assert_eq!(count(&human_chat, "检测到 bot 间"), 0);
    assert_eq!(
        count(&bot_chat, "仅授权用户可打开终端"),
        3,
        "bot 第四条熔断、第五条冷却"
    );

    let kind_of = |chat_id: &str| peers.load(&chat_tag("p2p", chat_id)).unwrap().peers[0].kind;
    assert_eq!(kind_of(&human_chat), summon_core::peers::PeerKind::Human);
    assert_eq!(kind_of(&bot_chat), summon_core::peers::PeerKind::Bot);
}

/// `/term` 由 dispatcher 主循环就地回复,不落入站 WAL(也就没进 worker 队列,不会排在卡住的
/// 那一轮后面);没有 operate 权限的人拿到 🔒,任何地方都没有终端卡。
#[test]
fn term_is_answered_by_the_dispatcher_and_refused_to_non_operators() {
    let fx = spawn_card_gate_dispatcher(
        "feishu:\n  allowed_users:\n    - ou_owner\n    - ou_guest\n  operators:\n    - ou_owner\n\
messaging:\n  session_mirror: false\n",
        &[],
    );
    let guest_chat = unique_chat("term_guest");
    let owner_chat = unique_chat("term_owner");
    let term = |chat_id: &str, sender: &str| Inbound {
        anchor_message_id: None,
        chat_id: chat_id.to_string(),
        chat_type: Some("p2p".to_string()),
        text: "/term".into(),
        sender: sender.to_string(),
        sender_union_id: String::new(),
        sender_type: "user".into(),
        message_id: format!("om_term_{chat_id}"),
        parent_id: None,
        thread_id: None,
        root_id: None,
        mentions: Vec::new(),
        resources: Vec::new(),
        create_time_ms: now_unix_ms() as i64,
        replayed: false,
        reattach: false,
    };
    for (chat_id, sender) in [(&guest_chat, "ou_guest"), (&owner_chat, "ou_owner")] {
        fx.tx
            .send(DispatchMsg::Inbound(term(chat_id, sender)))
            .expect("dispatcher 应在线");
    }
    let says = |chat_id: &str, needle: &str| {
        fx.outbound("p2p", chat_id).iter().any(|entry| {
            entry["payload"]["markdown"]
                .as_str()
                .is_some_and(|text| text.contains(needle))
        })
    };
    // 测试进程里没有终端服务:operator 拿到的是「没有启动」,来自 dispatcher 的签发路径。
    fx.wait_for("operator 收到回复", || {
        says(&owner_chat, "终端服务没有启动")
    });
    // dispatcher 单线程按序处理:后一条有了结果,前一条必然早已判完。
    assert!(says(&guest_chat, "仅授权用户可打开终端"));

    let files_containing = |root: &std::path::Path, needle: &str| -> Vec<std::path::PathBuf> {
        let mut hits = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle)) {
                    hits.push(path);
                }
            }
        }
        hits
    };
    assert!(
        files_containing(&fx.agent.state_dir(), "\"terminal_card\"").is_empty(),
        "谁都没拿到终端卡"
    );
    assert!(
        files_containing(&fx._tmp.path().join("inbox"), "om_term_").is_empty(),
        "/term 不进入站 WAL,也就没交给 worker"
    );
}

/// `/dash` 与 `/term` 同样由 dispatcher 就地回复、不落入站 WAL;没有 operate 权限的人拿到 🔒,
/// 有权限的人在测试进程里拿到「控制台服务没有启动」(测试从不装服务),谁都没拿到控制台卡。
#[test]
fn dash_denied_without_operate() {
    let fx = spawn_card_gate_dispatcher(
        "feishu:\n  allowed_users:\n    - ou_owner\n    - ou_guest\n  operators:\n    - ou_owner\n\
messaging:\n  session_mirror: false\n",
        &[],
    );
    let guest_chat = unique_chat("dash_guest");
    let owner_chat = unique_chat("dash_owner");
    let dash = |chat_id: &str, sender: &str| Inbound {
        anchor_message_id: None,
        chat_id: chat_id.to_string(),
        chat_type: Some("p2p".to_string()),
        text: "/dash".into(),
        sender: sender.to_string(),
        sender_union_id: String::new(),
        sender_type: "user".into(),
        message_id: format!("om_dash_{chat_id}"),
        parent_id: None,
        thread_id: None,
        root_id: None,
        mentions: Vec::new(),
        resources: Vec::new(),
        create_time_ms: now_unix_ms() as i64,
        replayed: false,
        reattach: false,
    };
    for (chat_id, sender) in [(&guest_chat, "ou_guest"), (&owner_chat, "ou_owner")] {
        fx.tx
            .send(DispatchMsg::Inbound(dash(chat_id, sender)))
            .expect("dispatcher 应在线");
    }
    let says = |chat_id: &str, needle: &str| {
        fx.outbound("p2p", chat_id).iter().any(|entry| {
            entry["payload"]["markdown"]
                .as_str()
                .is_some_and(|text| text.contains(needle))
        })
    };
    fx.wait_for("operator 收到回复", || {
        says(&owner_chat, "控制台服务没有启动")
    });
    assert!(says(&guest_chat, "仅授权用户可打开控制台"));
    assert!(
        !says(&guest_chat, "控制台服务没有启动"),
        "没权限的人不走签发"
    );

    let files_containing = |root: &std::path::Path, needle: &str| -> Vec<std::path::PathBuf> {
        let mut hits = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle)) {
                    hits.push(path);
                }
            }
        }
        hits
    };
    assert!(
        files_containing(&fx.agent.state_dir(), "\"dashboard_card\"").is_empty(),
        "谁都没拿到控制台卡"
    );
    assert!(
        files_containing(&fx._tmp.path().join("inbox"), "om_dash_").is_empty(),
        "/dash 不进入站 WAL,也就没交给 worker"
    );
}

#[test]
fn fleet_archive_online_preserves_current_session_when_snapshot_fails() {
    let chat_id = unique_chat("fleet_archive_failure");
    let fx = spawn_card_gate_dispatcher(
        "feishu:\n  operators:\n    - ou_owner\nmessaging:\n  session_mirror: false\n",
        &[(&chat_id, "p2p")],
    );
    fx.seed_resume("p2p", &chat_id);
    std::fs::create_dir(crate::session::resume_store::previous_path(
        &fx.agent,
        &fx.session_of("p2p", &chat_id),
    ))
    .unwrap();
    fx.tx
        .send(DispatchMsg::FleetSessionControl {
            chat_id: chat_id.clone(),
            archive: true,
        })
        .unwrap();
    let outbound = fx
        .agent
        .state_dir()
        .join("threads")
        .join(chat_tag("p2p", &chat_id))
        .join("outbound.ndjson");
    fx.wait_for("归档失败回执", || {
        std::fs::read_to_string(&outbound).is_ok_and(|text| text.contains("归档失败"))
    });
    assert!(fx.resume_kept("p2p", &chat_id));
    assert!(!crate::session::lifecycle::read(&fx.agent, &chat_tag("p2p", &chat_id)).is_closed());
}

#[test]
fn fleet_offline_archive_and_exit_keep_previous_session_and_use_matching_receipts() {
    for archive in [false, true] {
        for fail in [false, true] {
            let (agent, tmp) = crate::test_support::agent_dir("claude", "");
            let chat_id = unique_chat("fleet_offline");
            let session = agent.tmux_name_for_chat("p2p", &chat_id, false);
            write_chat_meta(
                &agent,
                &session,
                &chat_id,
                "p2p",
                None,
                "ou_owner",
                Some("ou_owner"),
            );
            crate::session::resume_store::write_checked(
                &agent,
                &session,
                "claude",
                "sid-fleet",
                Some(tmp.path().to_str().unwrap()),
                None,
            )
            .unwrap();
            if fail {
                std::fs::create_dir(crate::session::resume_store::previous_path(
                    &agent, &session,
                ))
                .unwrap();
            }
            let fc = FeishuClient::new("feishu.cn", "test".into(), "test".into(), true);
            let outbox = test_outbox(tmp.path());
            super::teardown::handle_fleet_chat_control(
                &mut HashMap::new(),
                &agent,
                &fc,
                &builtin_messaging_for_test(),
                &outbox,
                &crate::runtime::inflight::InFlight::new(),
                50,
                50,
                &chat_id,
                archive,
            );
            let kept = crate::session::resume_store::read_id(&agent, &session, "claude");
            assert_eq!(kept.is_some(), fail || !archive);
            let events = outbox.pending_events();
            if fail {
                assert!(events.is_empty());
            } else {
                assert_eq!(
                    crate::session::resume_store::previous_checked(&agent, &session)
                        .unwrap()
                        .id,
                    "sid-fleet"
                );
                assert_eq!(events.len(), 1);
                let summon_core::outbox::OutboxKind::NotifyChat {
                    reason,
                    rendered_card,
                    ..
                } = &events[0].kind
                else {
                    panic!("会话回执");
                };
                assert_eq!(
                    *reason,
                    if archive {
                        summon_core::exit::ExitReason::PanelReset
                    } else {
                        summon_core::exit::ExitReason::PanelClean
                    }
                );
                assert!(rendered_card.contains(if archive { "📦" } else { "会话已退出" }));
            }
        }
    }
}

#[test]
fn fleet_archive_with_a_closed_worker_channel_executes_from_persisted_identity() {
    let (agent, tmp) = crate::test_support::agent_dir("claude", "");
    let chat_id = unique_chat("fleet_closed_channel");
    let session = agent.tmux_name_for_chat("p2p", &chat_id, false);
    write_chat_meta(
        &agent,
        &session,
        &chat_id,
        "p2p",
        None,
        "ou_owner",
        Some("ou_owner"),
    );
    crate::session::resume_store::write_checked(
        &agent,
        &session,
        "claude",
        "sid-fleet-closed",
        None,
        None,
    )
    .unwrap();
    let (tx, rx) = channel::<ThreadMsg>();
    drop(rx);
    let mut chats = HashMap::from([(
        chat_id.clone(),
        ChatHandle {
            tx,
            last_activity: std::time::SystemTime::now(),
            idle_prompted: false,
            chat_type: "p2p".into(),
            tmux_name: session.clone(),
            tmux_seen_live: false,
            local_claimed: true,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        },
    )]);
    let fc = FeishuClient::new("feishu.cn", "test".into(), "test".into(), true);
    let outbox = test_outbox(tmp.path());
    super::teardown::handle_fleet_chat_control(
        &mut chats,
        &agent,
        &fc,
        &builtin_messaging_for_test(),
        &outbox,
        &crate::runtime::inflight::InFlight::new(),
        50,
        50,
        &chat_id,
        true,
    );
    assert!(!chats.contains_key(&chat_id));
    assert!(crate::session::resume_store::read_record(&agent, &session).is_none());
    assert_eq!(
        crate::session::resume_store::previous_checked(&agent, &session)
            .unwrap()
            .id,
        "sid-fleet-closed"
    );
    let events = outbox.pending_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].kind,
        summon_core::outbox::OutboxKind::NotifyChat {
            reason: summon_core::exit::ExitReason::PanelReset,
            ..
        }
    ));
}

/// 授权 operator 点「清理」→ gate 放行 → `do_teardown(reset=false)`:**保留** resume 记录(续聊)。
/// 点「重置」→ `do_teardown(reset=true)`:**丢弃** resume 记录(下次冷启真重置)。
#[test]
fn authorized_operator_card_clean_keeps_resume_and_reset_drops_it() {
    let clean_chat = unique_chat("clean");
    let reset_chat = unique_chat("reset");
    let fx = spawn_card_gate_dispatcher(
        "feishu:\n  operators:\n    - ou_owner\nmessaging:\n  session_mirror: false\n",
        &[(&clean_chat, "p2p"), (&reset_chat, "p2p")],
    );
    fx.seed_resume("p2p", &clean_chat);
    fx.seed_resume("p2p", &reset_chat);

    fx.click("p2p", &clean_chat, "clean", "ou_owner");
    fx.wait_for("Clean 走完 teardown 收尾", || {
        fx.closed_card_text("p2p", &clean_chat).is_some()
    });
    assert!(
        fx.resume_kept("p2p", &clean_chat),
        "Clean 语义是留记录续聊,teardown 不该带 reset"
    );
    let clean_card = fx.closed_card_text("p2p", &clean_chat).unwrap();
    assert!(
        clean_card.contains("接着本次会话续聊"),
        "终态卡应按 reset=false 渲染:{clean_card}"
    );

    fx.click("p2p", &reset_chat, "reset", "ou_owner");
    fx.wait_for("Reset 走完 teardown 收尾", || {
        fx.closed_card_text("p2p", &reset_chat).is_some()
    });
    assert!(
        !fx.resume_kept("p2p", &reset_chat),
        "Reset 必须清 resume 记录,否则下次冷启还会续上旧上下文"
    );
    assert!(fx
        .closed_card_text("p2p", &reset_chat)
        .unwrap()
        .contains("/resume"));
}

/// 非 operator 点同一个按钮:gate 挡下、回执「仅授权用户」,teardown **一次都没被调用**。
/// 随后同一张卡由 operator 点 → 真的拆除:证明上面的「没拆」不是接线断了,而是 gate 挡住了。
#[test]
fn non_operator_card_action_is_denied_and_never_reaches_teardown() {
    let chat = unique_chat("gate");
    let fx = spawn_card_gate_dispatcher(
        "feishu:\n  operators:\n    - ou_owner\nmessaging:\n  session_mirror: false\n",
        &[(&chat, "p2p")],
    );
    fx.seed_resume("p2p", &chat);

    fx.click("p2p", &chat, "reset", "ou_stranger");
    fx.wait_for("非授权回执已归档", || {
        fx.denial_notice("p2p", &chat)
    });
    assert!(
        fx.resume_kept("p2p", &chat),
        "被拒的卡片动作绝不能走到 do_teardown"
    );
    assert!(
        fx.closed_card_text("p2p", &chat).is_none(),
        "被拒时不该有任何退出收尾"
    );

    fx.click("p2p", &chat, "reset", "ou_owner");
    fx.wait_for("授权 operator 点同一按钮应真拆除", || {
        !fx.resume_kept("p2p", &chat)
    });
}

/// operators/allowed_users 皆空(allow-all)时 operate 档的 fail-closed 方向:群里一律拒、
/// 私聊放行(裸会话即 owner)。判 is_group 的方向搞反 = 对群里任何人开放破坏性按钮。
#[test]
fn empty_operator_list_denies_group_card_but_allows_p2p_card() {
    let group_chat = unique_chat("gategroup");
    let p2p_chat = unique_chat("gatep2p");
    let fx = spawn_card_gate_dispatcher(
        "feishu:\n  app_id_env: SUMMON_TEST_APP\nmessaging:\n  session_mirror: false\n",
        &[(&group_chat, "group"), (&p2p_chat, "p2p")],
    );
    fx.seed_resume("group", &group_chat);
    fx.seed_resume("p2p", &p2p_chat);

    fx.click("group", &group_chat, "reset", "ou_anyone");
    fx.wait_for("群里 fail-closed 回执", || {
        fx.denial_notice("group", &group_chat)
    });
    assert!(
        fx.resume_kept("group", &group_chat),
        "白名单为空时群里必须 fail-closed,不得拆会话"
    );

    fx.click("p2p", &p2p_chat, "reset", "ou_anyone");
    fx.wait_for("私聊放行并真拆除", || {
        !fx.resume_kept("p2p", &p2p_chat)
    });
    assert!(
        !fx.denial_notice("p2p", &p2p_chat),
        "私聊不该被 operate-gate 挡下"
    );
}

/// **超时不是消息**:steward 的观测类白名单只该筛真收到的消息。把 `Err(Timeout)` 也筛掉,
/// 就会跳过循环尾的 idle scan 与 `last_scan` 重置,进而退化成忙循环(见上一条测试)。
///
/// 这里钉的是那个前置条件本身 —— 删掉 `dispatch.is_ok()`,本测试立刻红。
#[test]
fn steward_message_whitelist_only_filters_real_messages() {
    let src = include_str!("dispatcher.rs");
    let start = src
        .find("if agent.is_steward()")
        .expect("steward 白名单判定");
    let head = &src[start..start + 200];
    assert!(
        head.contains("dispatch.is_ok()"),
        "白名单必须先确认这是一条真消息;超时/断连不该走消息白名单:{head}"
    );
}

/// **引用分流的判据是「引用的是谁的消息」,不是「内容能不能读到」。**
///
/// 2026-08-10 实测:飞书对 interactive 返回卡片的降级文本 —— 简单 markdown 卡能提取出真内容,
/// 复杂卡才退化成「请升级客户端」。上一版把分流绑在"是否占位"上,于是读得到全文时就把整段塞
/// 进 prompt,恰恰是 owner 要省的那部分 token。这条钉住判据里必须有 sender 比对。
#[test]
fn quote_split_keys_on_sender_not_on_placeholder() {
    let src = include_str!("chat_thread.rs");
    let start = src
        .find("pub(super) fn fetch_quote_note(")
        .expect("fetch_quote_note 定义");
    let body = &src[start..(start + 2600).min(src.len())];
    assert!(
        body.contains("bot_open_id()"),
        "必须比对被引用消息的 sender 与本 bot,才能认出「引用的是自己」:{body}"
    );
    assert!(
        body.contains("is_self ||"),
        "自己发的消息应无条件走指代分支,不该只在读到占位时才分流"
    );
}

/// 红线守卫(2026-08-13):两条 live 卡投影路构造的 `TurnProgress` **必须**带 fencing token。
///
/// 为什么要用源码扫描而不是行为测试:出事的那一环是**调用点漏写**
/// (`run_headless_interruptible` 写了裸 `TurnProgress::new()`),而无头可打断路当时没有任何
/// 非 ignored 测试打得到 —— 行为测试证明不了「没人再漏写一次」。这条守卫按调用点钉:谁在
/// chat_thread.rs 里新建一个不带 fence 的 TurnProgress,本测试就红,逼他显式复核。
///
/// 局限:去注释 + 去空白后的 substring 扫描,只管 chat_thread.rs 这一个文件(两条投影路都在
/// 这里)。换来的是零依赖、跨 rustfmt 折行稳定。
#[test]
fn live_card_turn_progress_always_carries_fence_token() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime/daemon/chat_thread.rs");
    let text = std::fs::read_to_string(&path).expect("读 chat_thread.rs 失败");
    // 去掉注释行(文档里会引用裸构造做反面教材),再去掉全部空白抹平 rustfmt 折行。
    let code: String = text
        .lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let squeezed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let total = squeezed.matches("TurnProgress::new()").count();
    let fenced = squeezed
        .matches("TurnProgress::new().with_turn_id(")
        .count();
    // 非空自检:扫描器必须真读到构造点,否则路径写错会让本测试恒绿(与 tmux 红线守卫同一防线)。
    assert!(
        total >= 2,
        "扫描器失效:chat_thread.rs 里应至少有两处 TurnProgress 构造(两条投影路),实际 {total}"
    );
    assert_eq!(
        total,
        fenced,
        "有 {} 处 TurnProgress::new() 没接 .with_turn_id() —— 打断按钮会渲出空 token、点击侧 \
         fail-closed 拒绝(2026-08-13 无头路实况)。新建构造点请从 fence_turn_id() 取 token。",
        total - fenced
    );
}

#[test]
fn process_only_question_boundary_keeps_one_live_cot_run() {
    use summon_core::agent_msg::{AgentKind, AgentMessage};
    let frontend = EmitterFrontend::new();
    frontend.process_only.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_cot_ask",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let mut proj = TurnProjector::new(
        CotEmitter::new(None),
        summon_core::progress::TurnProgress::new().with_turn_id("turn-cot"),
        None,
    );
    let mut before = AgentMessage::of(AgentKind::Assistant);
    before.text = Some("提问前".into());
    proj.project_event(&frontend, &chat, "tester", &before);
    proj.progress
        .tool_started_with_kind("等待工具", "tool_wait", None);

    proj.note_question_pending();
    let mut after = AgentMessage::of(AgentKind::Assistant);
    after.text = Some("回答后继续".into());
    proj.project_event(&frontend, &chat, "tester", &after);

    let calls = frontend.calls.borrow();
    assert_eq!(calls.iter().filter(|call| **call == "begin").count(), 1);
    assert!(!calls.contains(&"finish"), "轮内边界不得提前关闭 CoT");
    assert_eq!(
        proj.progress.protocol_tools().len(),
        1,
        "同一轮未完成工具的协议归属必须跨 ask 边界保留"
    );
}

/// 走真实接线的端到端切卡:`card_stream_consume` 的 `run_source` 里调生产 `on_interactive`
/// 闭包(发卡 + 置提问待答态),而非直接调 `note_question_pending` —— 守住「发卡成功才置位」
/// 这层接线不被重构绕过。
///
/// 序列:提问前 Assistant → on_interactive(置位) → ToolResult(回答后首事件,触发切卡)
/// → Assistant(进新卡) → 第二个 Assistant(验证一次性消费,不再切卡)。
#[test]
fn question_ask_split_fires_through_real_on_interactive_chain() {
    use summon_core::agent_msg::{AgentKind, AgentMessage};
    use summon_core::interactive::{Choice, InteractiveState};
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_ask_split_e2e",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (_, _) = card_stream_consume(
        &frontend,
        &chat,
        false, // summary_card
        summon_core::progress::DEFAULT_MAX_STEPS,
        false, // hide_thinking
        false, // tool_trail
        None,
        None,
        None, // badge_seed: 测试不校验角标
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        move |_on_msg, _on_stream, _on_tool, on_event, on_interactive, _on_tick, _on_steer| {
            // ① 提问前的内容开出第一张卡。
            let mut before = AgentMessage::of(AgentKind::Assistant);
            before.text = Some("提问前的输出".into());
            on_event(&before);
            // ② 交互菜单弹出:走生产 on_interactive 闭包(发卡 + 置提问待答态),并回报送达。
            assert_eq!(
                on_interactive(&InteractiveState {
                    title: "选一个方向".into(),
                    options: vec![
                        Choice {
                            index: 1,
                            label: "方案A".into(),
                        },
                        Choice {
                            index: 2,
                            label: "方案B".into(),
                        },
                    ],
                    raw: String::new(),
                }),
                crate::capture::MediationCard::Settled,
                "发卡成功要回报送达,否则捕获层会白白重发"
            );
            // ③ 回答生效后的第一条事件是 ToolResult:必须在它这里切卡(旧卡定格、新卡开),
            //    而不是等到后面的 Assistant 才切。
            let done = AgentMessage::of(AgentKind::ToolResult);
            on_event(&done);
            // ④ 回答后的 Assistant 内容进新卡。
            let mut after = AgentMessage::of(AgentKind::Assistant);
            after.text = Some("回答后的继续输出".into());
            on_event(&after);
            // ⑤ 第二个后续事件:question_pending 已在 ③ 一次性消费,不得再切卡。
            let mut second = AgentMessage::of(AgentKind::Assistant);
            second.text = Some("第二条后续".into());
            on_event(&second);
            Ok(crate::capture::TurnOutcome::default())
        },
    );

    let calls = frontend.calls.borrow();
    let payloads = frontend.payloads.borrow();
    // 交互中介卡必须真发出去(否则 on_interactive 不会置位,后续断言恒绿)。
    let card_idx = calls
        .iter()
        .position(|c| *c == "card")
        .expect("on_interactive 必须发交互中介卡");
    assert_eq!(
        calls[card_idx + 1],
        "steer_boundary",
        "回答后的首事件打开同一 CoT 的新区段"
    );
    assert_eq!(
        calls[card_idx + 1..]
            .iter()
            .filter(|call| **call == "steer_boundary")
            .count(),
        1
    );
    assert!(
        !calls[card_idx + 1..].contains(&"begin"),
        "过程保持同一载体"
    );
    assert!(payloads.iter().any(|body| body.contains("提问前的输出")));
}

/// 发卡失败不切卡:`on_interactive` 发卡失败时不置提问待答态,回答后内容继续 PATCH 原卡。
#[test]
fn ask_split_skipped_when_interactive_card_send_fails() {
    use summon_core::agent_msg::{AgentKind, AgentMessage};
    use summon_core::interactive::{Choice, InteractiveState};
    let frontend = EmitterFrontend::new();
    frontend.fail_card.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_ask_split_fail",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let (_, _) = card_stream_consume(
        &frontend,
        &chat,
        false, // summary_card
        summon_core::progress::DEFAULT_MAX_STEPS,
        false, // hide_thinking
        false, // tool_trail
        None,
        None,
        None, // badge_seed: 测试不校验角标
        &test_outbox(dir.path()),
        &|| None,
        None, // 测试:无 subagent 状态源
        &mut |_: &[String]| {},
        move |_on_msg, _on_stream, _on_tool, on_event, on_interactive, _on_tick, _on_steer| {
            let mut before = AgentMessage::of(AgentKind::Assistant);
            before.text = Some("提问前的输出".into());
            on_event(&before);
            assert_eq!(
                on_interactive(&InteractiveState {
                    title: "选一个方向".into(),
                    options: vec![
                        Choice {
                            index: 1,
                            label: "方案A".into(),
                        },
                        Choice {
                            index: 2,
                            label: "方案B".into(),
                        },
                    ],
                    raw: String::new(),
                }),
                crate::capture::MediationCard::Undelivered,
                "发卡失败必须回报未达 —— 捕获层据它重发,这是用户看见菜单的唯一途径"
            );
            let mut after = AgentMessage::of(AgentKind::Assistant);
            after.text = Some("回答后的继续输出".into());
            on_event(&after);
            Ok(crate::capture::TurnOutcome::default())
        },
    );

    let calls = frontend.calls.borrow();
    let payloads = frontend.payloads.borrow();
    let card_idx = calls
        .iter()
        .position(|c| *c == "card")
        .expect("发卡尝试仍记录");
    assert!(
        !calls[card_idx + 1..].contains(&"begin"),
        "发卡失败不得开新卡: {:?}",
        &calls[card_idx + 1..]
    );
    assert!(
        !calls[card_idx + 1..].contains(&"steer_boundary"),
        "提问未送达时不创建交互边界"
    );
    assert!(payloads.iter().any(|body| body.contains("提问前的输出")));
}

/// M2 回归：同进程续卡的失败守卫在 preflight 失败（Drop 未被 take）时，必须定格旧卡
/// （去打断按钮）并清 stream_state.json，避免旧卡永远停在「正在续跑」且带打断按钮。
#[test]
fn handoff_guard_freezes_card_and_clears_checkpoint_on_drop() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_handoff_guard",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let tag = chat_tag(chat.chat_type, chat.chat_id);
    // 模拟上一轮 begin_live_card 落盘的 checkpoint
    let saved = summon_core::session::stream_card::StreamCardState {
        message_id: "om_old".into(),
        chat_tag: tag.clone(),
        with_interrupt: true,
        created_ms: now_unix_ms(),
    };
    crate::runtime::inflight::write_stream_state(dir.path(), &tag, &saved).unwrap();
    assert!(
        crate::runtime::inflight::read_stream_state(dir.path(), &tag).is_some(),
        "前置：checkpoint 已落盘"
    );

    // 武装守卫后立即 Drop（模拟 preflight 失败提前返回，未走到 take）
    {
        let _handoff = CotHandoff::armed(&frontend, &chat, StreamId(1));
    }

    assert_eq!(
        &*frontend.calls.borrow(),
        &["finish_text"],
        "preflight 失败必须定格旧卡（stream_freeze → stream_finish）"
    );
    let payload = frontend.payloads.borrow().last().cloned().unwrap();
    assert!(
        payload.contains("续跑失败"),
        "定格内容应表明续跑失败: {payload}"
    );
    assert!(
        crate::runtime::inflight::read_stream_state(dir.path(), &tag).is_none(),
        "preflight 失败必须清 checkpoint，防冷启恢复到死卡"
    );
}

/// M2 回归：守卫被 take（投影器接管）后，Drop 必须是 no-op——不定格旧卡、不清 checkpoint。
/// 旧卡的 stream_map 映射保留给递归轮的投影器续用。
#[test]
fn handoff_guard_take_disarms_drop() {
    let frontend = EmitterFrontend::new();
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_handoff_take",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let tag = chat_tag(chat.chat_type, chat.chat_id);
    let saved = summon_core::session::stream_card::StreamCardState {
        message_id: "om_old".into(),
        chat_tag: tag.clone(),
        with_interrupt: true,
        created_ms: now_unix_ms(),
    };
    crate::runtime::inflight::write_stream_state(dir.path(), &tag, &saved).unwrap();

    let mut handoff = CotHandoff::armed(&frontend, &chat, StreamId(1));
    let id = handoff.take();
    assert_eq!(id, Some(StreamId(1)), "take 返回 id 供投影器使用");
    drop(handoff); // Drop 不应定格

    assert!(
        frontend.calls.borrow().is_empty(),
        "take 后 Drop 不应定格旧卡（投影器已接管）"
    );
    assert!(
        crate::runtime::inflight::read_stream_state(dir.path(), &tag).is_some(),
        "take 后 checkpoint 应保留（供 daemon crash 恢复）"
    );
}

/// M2 失败分支回归：`stream_freeze` 失败时必须保留 checkpoint，让未定格的卡片仍有
/// 冷启恢复指针。若失败也清 checkpoint，卡片会成为永久孤儿（带按钮、无恢复点）。
#[test]
fn handoff_guard_keeps_checkpoint_when_freeze_fails() {
    let frontend = EmitterFrontend::new();
    // stream_freeze 默认委托 stream_finish；让它失败模拟远端 PATCH 故障。
    frontend.fail_finish.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = ChatRef {
        thread_anchor: None,
        chat_id: "oc_handoff_freeze_fail",
        chat_type: "p2p",
        agent_dir: dir.path(),
        agent_name: "tester",
        delivery_scope: "test-scope",
    };
    let tag = chat_tag(chat.chat_type, chat.chat_id);
    let saved = summon_core::session::stream_card::StreamCardState {
        message_id: "om_old".into(),
        chat_tag: tag.clone(),
        with_interrupt: true,
        created_ms: now_unix_ms(),
    };
    crate::runtime::inflight::write_stream_state(dir.path(), &tag, &saved).unwrap();

    // 武装守卫后 Drop（preflight 失败），但 stream_freeze 失败
    {
        let _handoff = CotHandoff::armed(&frontend, &chat, StreamId(1));
    }

    assert!(
        frontend.calls.borrow().contains(&"finish_text"),
        "应尝试定格（stream_freeze → stream_finish）"
    );
    assert!(
        crate::runtime::inflight::read_stream_state(dir.path(), &tag).is_some(),
        "freeze 失败时必须保留 checkpoint，否则未定格的卡片成永久孤儿"
    );
}

/// 测试5：headless_leftover_outcome 的 rerun truth table。明确「已转移 ownership 时兜底
/// no-op」由 owner 状态（stream_id 已清）保证，而不是靠调用者手写清 id——本测试钉住裁决
/// 函数本身的语义，ownership 转移由 handoff_guard_take_disarms_drop 覆盖。
#[test]
fn headless_leftover_outcome_rerun_truth_table() {
    use crate::runtime::inflight::FinishDisposition;
    let clean: anyhow::Result<crate::capture::TurnOutcome> = Ok(Default::default());
    let err: anyhow::Result<crate::capture::TurnOutcome> =
        Err(anyhow::anyhow!("provider exploded"));

    // ShutdownClaimed → PreserveForRecovery（无论 rerun，下一代接管）
    assert_eq!(
        headless_leftover_outcome(
            Some(FinishDisposition::ShutdownClaimed),
            &clean,
            false,
            false
        ),
        LeftoverCardOutcome::PreserveForRecovery
    );
    assert_eq!(
        headless_leftover_outcome(
            Some(FinishDisposition::ShutdownClaimed),
            &clean,
            false,
            true
        ),
        LeftoverCardOutcome::PreserveForRecovery
    );

    // Missing → Failed（完成权不在本轮，不能渲「已完成」）
    assert_eq!(
        headless_leftover_outcome(Some(FinishDisposition::Missing), &clean, false, false),
        LeftoverCardOutcome::Failed(AbortReason::OwnershipMissing)
    );
    assert_eq!(
        headless_leftover_outcome(Some(FinishDisposition::Missing), &clean, false, true),
        LeftoverCardOutcome::Failed(AbortReason::OwnershipMissing)
    );

    // Completed + 干净 + 无 rerun → Completed
    assert_eq!(
        headless_leftover_outcome(Some(FinishDisposition::Completed), &clean, false, false),
        LeftoverCardOutcome::Completed
    );
    // Completed + 干净 + rerun → Failed（rerun 轮的卡已移交，兜底裁决 Failed 但 stream_id
    // 已清 → finish_leftover_live_card no-op，不会双重定格）
    assert_eq!(
        headless_leftover_outcome(Some(FinishDisposition::Completed), &clean, false, true),
        LeftoverCardOutcome::Failed(AbortReason::Rerun)
    );
    // Completed + Err → Failed
    assert_eq!(
        headless_leftover_outcome(Some(FinishDisposition::Completed), &err, false, false),
        LeftoverCardOutcome::Failed(AbortReason::ProviderError)
    );
    // Completed + interrupted → Failed
    assert_eq!(
        headless_leftover_outcome(Some(FinishDisposition::Completed), &clean, true, false),
        LeftoverCardOutcome::Failed(AbortReason::Interrupted)
    );

    // None（无 on_finish 探针）+ 干净 → Completed
    assert_eq!(
        headless_leftover_outcome(None, &clean, false, false),
        LeftoverCardOutcome::Completed
    );
    // None + 干净 + rerun → Failed
    assert_eq!(
        headless_leftover_outcome(None, &clean, false, true),
        LeftoverCardOutcome::Failed(AbortReason::Rerun)
    );
}

#[test]
fn continuation_golden_keeps_18_process_replies_and_6_interims_out_of_the_final() {
    use summon_core::agent_msg::AgentKind;
    let frontend = CotRecorder::default();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_continuation_golden");
    let outbox = test_outbox(dir.path());
    let mut projector = cot_projector(None);
    let process: Vec<_> = (0..18)
        .map(|index| format!("过程说明{index:02}。"))
        .collect();
    let interims: Vec<_> = (0..6)
        .map(|index| format!("等待后台任务{index}。"))
        .collect();
    for (index, text) in process.iter().enumerate() {
        run_events(
            &frontend,
            &chat,
            &mut projector,
            &[process_reply_event(AgentKind::Assistant, text)],
        );
        run_events(&frontend, &chat, &mut projector, &process_tool_events());
        if index == 2 {
            frontend.append_failed.set(true);
        }
        if (3..=13).contains(&index) && index % 2 == 1 {
            let interim = &interims[(index - 3) / 2];
            run_events(
                &frontend,
                &chat,
                &mut projector,
                &[process_reply_event(AgentKind::Assistant, interim)],
            );
            assert!(
                projector
                    .deliver_interim_reply(&frontend, &chat, "tester", interim, None)
                    .unwrap()
                    .complete
            );
        }
    }
    let final_text = "流水线检查完成。";
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[process_reply_event(AgentKind::Assistant, final_text)],
    );
    projector.deliver_process_supplement(&frontend, &chat, "tester", &outbox, Some(final_text));
    projector.deliver_process_supplement(&frontend, &chat, "tester", &outbox, Some(final_text));
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some(final_text))
            .as_deref(),
        Some(final_text)
    );
    assert_eq!(*frontend.sent.borrow(), interims);
    let cards = frontend.supplements.borrow();
    assert_eq!(cards.len(), 1);
    let card: serde_json::Value = serde_json::from_str(&cards[0]).unwrap();
    assert_eq!(card["body"]["elements"][0]["expanded"], false);
    assert_eq!(
        card["body"]["elements"][0]["elements"][0]["content"],
        process[3..].join("\n\n")
    );
    assert!(outbox.pending_events().is_empty());
}

#[test]
fn independent_final_preserves_an_unconfirmed_process_reply_for_supplement() {
    use summon_core::agent_msg::AgentKind;
    let frontend = CotRecorder::default();
    frontend.append_failed.set(true);
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_distinct_final");
    let mut projector = cot_projector(None);
    run_events(
        &frontend,
        &chat,
        &mut projector,
        &[process_reply_event(AgentKind::Assistant, "过程说明。")],
    );
    run_events(&frontend, &chat, &mut projector, &process_tool_events());
    projector.live.confirm_cot_replies(&frontend);
    assert_eq!(
        projector
            .live
            .unconfirmed_process_replies(&projector.segments, Some("独立结论。"))
            .as_deref(),
        Some("过程说明。")
    );
    assert_eq!(
        projector
            .live
            .final_reply_body(&projector.segments, Some("独立结论。"))
            .as_deref(),
        Some("独立结论。")
    );
}

#[test]
fn process_supplements_respect_result_only_silent_reset_and_reattach() {
    use summon_core::agent_msg::AgentKind;
    for mode in ["result_only", "silent", "reattached"] {
        let frontend = CotRecorder::default();
        frontend.append_failed.set(true);
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_supplement_boundaries");
        let outbox = test_outbox(dir.path());
        let mut projector = cot_projector(None);
        if mode == "reattached" {
            projector = projector.with_process_replies(false);
        }
        run_events(
            &frontend,
            &chat,
            &mut projector,
            &[
                process_reply_event(AgentKind::Assistant, "过程文字。"),
                process_reply_event(AgentKind::Thinking, "继续思考"),
            ],
        );
        if mode == "result_only" {
            let buffered = crate::frontend::ResultOnlyFrontend::new(&frontend);
            projector.deliver_process_supplement(
                &buffered,
                &chat,
                "tester",
                &outbox,
                Some("结论。"),
            );
        } else {
            projector.deliver_process_supplement(
                &frontend,
                &chat,
                "tester",
                &outbox,
                Some(if mode == "silent" {
                    "__SUMMON_SILENT_EXIT__"
                } else {
                    "结论。"
                }),
            );
        }
        assert!(frontend.supplements.borrow().is_empty(), "{mode}");
        assert!(outbox.pending_events().is_empty(), "{mode}");
    }
}

#[test]
fn split_process_supplement_retries_only_the_unsent_cards() {
    for successes in [0, 1] {
        let frontend = CotRecorder::default();
        frontend.flush_fails.set(true);
        frontend.fail_supplement_after.set(Some(successes));
        let dir = tempfile::tempdir().unwrap();
        let chat = badge_chat(dir.path(), "oc_split_supplement");
        let mut projector = generation_before_a_cursor_restart(&frontend, &chat);
        let prefix = "第一段说明。\n\n";
        let suffix = "收到,先看这个。";
        frontend.split_at.set(Some(prefix.len()));
        let outbox = test_outbox(dir.path());
        projector.hand_off_generation_replies(&frontend, &chat, "tester", &outbox);
        let pending = outbox.pending_events();
        assert_eq!(pending.len(), 2 - successes);
        let mut bodies = std::collections::BTreeSet::new();
        for event in pending {
            let summon_core::outbox::OutboxKind::ProcessSupplement { rendered_card, .. } =
                event.kind
            else {
                panic!("process supplement");
            };
            let card: serde_json::Value = serde_json::from_str(&rendered_card).unwrap();
            bodies.insert(
                card["body"]["elements"][0]["elements"][0]["content"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        let expected = if successes == 0 {
            vec![prefix.to_string(), suffix.to_string()]
        } else {
            vec![suffix.to_string()]
        };
        assert_eq!(bodies, expected.into_iter().collect());
        assert_eq!(frontend.supplements.borrow().len(), successes);
        assert!(frontend.sent.borrow().is_empty());
    }
}

/// 与飞书前端一样把 live 卡投成原生 CoT:开卡时按 `view.query` 起投影器(取不到用「处理请求」),
/// 之后按游标续投,回复投递时按 done 收口。供飞书效果预览的一致性测试用。
#[derive(Default)]
struct PreviewParityFrontend {
    projector: std::cell::RefCell<Option<summon_core::cot::CotProjector>>,
    events: std::cell::RefCell<Vec<summon_core::cot::CotEvent>>,
}

impl PreviewParityFrontend {
    fn project(&self, view: &summon_core::cot::CotView) {
        let mut projector = self.projector.borrow_mut();
        let projector = projector.get_or_insert_with(|| {
            summon_core::cot::CotProjector::new(
                "preview",
                "preview",
                view.query.unwrap_or("处理请求"),
            )
        });
        self.events
            .borrow_mut()
            .extend(projector.project_with_replies(
                view.segments,
                view.released_replies,
                view.progress,
                "0",
            ));
    }
}

impl Frontend for PreviewParityFrontend {
    fn send_message(&self, _chat: &ChatRef, _body: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn stream_begin(&self, _: &ChatRef, _: &str, _: bool) -> anyhow::Result<StreamId> {
        Ok(StreamId(1))
    }
    fn stream_patch(&self, _: StreamId, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn live_begin(
        &self,
        _chat: &ChatRef,
        view: &summon_core::cot::CotView,
    ) -> anyhow::Result<StreamId> {
        self.project(view);
        Ok(StreamId(1))
    }
    fn live_update(
        &self,
        _id: StreamId,
        view: &summon_core::cot::CotView,
    ) -> anyhow::Result<crate::frontend::StreamPatchOutcome> {
        self.project(view);
        Ok(crate::frontend::StreamPatchOutcome::Applied)
    }
    fn stream_is_process_only(&self, _id: StreamId) -> bool {
        true
    }
    fn stream_finish(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
        if let Some(projector) = self.projector.borrow_mut().as_mut() {
            self.events
                .borrow_mut()
                .extend(projector.finish("done", "0"));
        }
        Ok(())
    }
    fn react(&self, _: &crate::frontend::MsgRef, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn unreact(&self, _: &crate::frontend::MsgRef, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 飞书效果预览与 chat 线程投出的过程一致:可按群设置的开关全部组合下,同一段示例事件分别走
/// 生产投影器(`TurnProjector`,设置按两条流式入口的取法接)与预览引擎,折成的 CoT 逐条相同。
#[test]
fn gate_matches_thread_projection_for_all_switch_combinations() {
    use summon_core::config::settings::{
        chat_scoped, ChatOverrides, ResolvedSettings, SETTING_COUNT,
    };
    use summon_core::dashboard::preview;

    let messaging = summon_core::config::builtin_messaging();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_preview_parity");
    let mut distinct = Vec::new();
    for mask in 0..1u32 << SETTING_COUNT {
        let mut overrides = ChatOverrides::default();
        for (bit, decl) in chat_scoped().enumerate() {
            overrides.set(decl.id, mask & (1 << bit) != 0);
        }
        let settings = ResolvedSettings::resolve(&messaging, &overrides);
        let hide_thinking = settings.hide_thinking();
        let show_cot = settings.cot_visible();
        let show_tool_details = settings.cot_tool_details();
        let frontend = PreviewParityFrontend::default();
        let mut projector = TurnProjector::new(
            CotEmitter::new(None).with_task_summary(summon_core::turn_gate::cot_task_summary(
                show_cot,
                Some(preview::scenario().task),
            )),
            summon_core::progress::TurnProgress::new()
                .with_max_steps(messaging.progress_max_steps)
                .with_hide_thinking(hide_thinking)
                .with_tool_trail(messaging.tool_trail)
                .with_tool_details(show_tool_details),
            None,
        )
        .with_cot_visible(show_cot);
        for event in preview::scenario_events(&settings) {
            projector.project_event(&frontend, &chat, "tester", &event);
        }
        // 回复投递时的那次收口(`deliver_final_reply` / `finalize_turn_summary` 里的 stream_finish)。
        frontend.stream_finish(StreamId(1), "").unwrap();
        let thread = preview::cot_message(&frontend.events.borrow());
        let view = preview::render(&messaging, &settings, "");
        let engine = view
            .messages
            .iter()
            .find(|message| matches!(message, preview::PreviewMessage::Cot { .. }))
            .expect("预览里要有过程消息");
        assert_eq!(&thread, engine, "开关组合 {mask:#09b}");
        if !distinct.contains(&thread) {
            distinct.push(thread);
        }
    }
    // 非空自检:开关确实改变了过程的样子,对比没有退化成两边都空。
    assert!(distinct.len() >= 4, "过程形态种数 {}", distinct.len());
}

/// 预览组回复卡用的 `compose_reply_card`,与两条终态投递(单卡收尾、总结卡)发出去的卡逐字相同。
#[test]
fn compose_matches_delivery_path() {
    use summon_core::image::ReplyPiece;
    use summon_core::reply_card::{compose_reply_card, ReplyInput};

    let body =
        "CI 挂在单测上,已修好。日志在 `/tmp/report.md`。\n\n<summon-feed>CI 已修复</summon-feed>";
    let badge = summon_core::feishu::TurnBadge {
        cli: "claude".into(),
        model: Some("Opus 5".into()),
        effort: Some("high".into()),
        ..Default::default()
    };
    let tagged = summon_core::feed_tag::split_feed_tag(body);
    let badge_line = badge.render();
    let expected = |timestamp: &str| {
        compose_reply_card(&ReplyInput {
            pieces: &[ReplyPiece::Text(tagged.body.to_string())],
            badge: badge_line.as_deref(),
            timestamp,
            feed: tagged.summary.as_deref(),
            preview_buttons: &[],
        })
        .card_json
    };
    // 投递侧的时刻是发卡那一刻的 `HH:MM`;前后各取一次,跨分钟也不误判。
    let now = || chrono::Local::now().format("%H:%M").to_string();
    let dir = tempfile::tempdir().unwrap();
    let chat = badge_chat(dir.path(), "oc_compose_parity");

    let frontend = EmitterFrontend::new();
    let before = now();
    let report = deliver_final_reply(
        &frontend,
        &chat,
        "tester",
        &mut None,
        &mut false,
        body,
        Some(&badge),
        false,
    );
    let after = now();
    assert!(report.any_sent && report.complete);
    assert_eq!(&*frontend.calls.borrow(), &["card"]);
    let sent = frontend.payloads.borrow().last().cloned().unwrap();
    assert!(
        sent == expected(&before) || sent == expected(&after),
        "单卡收尾: {sent}"
    );

    let frontend = EmitterFrontend::new();
    let segments = vec![
        summon_core::segment::TurnSegment::Thinking("先看失败的用例".into()),
        summon_core::segment::TurnSegment::Reply(body.into()),
    ];
    let before = now();
    let report = finalize_turn_summary(
        &frontend,
        &chat,
        "tester",
        &mut None,
        &segments,
        &segments,
        Some(body),
        None,
        true,
        Some(&badge),
        None,
        false,
    )
    .expect("有思考的轮走总结卡");
    let after = now();
    assert!(report.any_sent && report.complete);
    assert_eq!(&*frontend.calls.borrow(), &["card"]);
    let sent = frontend.payloads.borrow().last().cloned().unwrap();
    assert!(
        sent == expected(&before) || sent == expected(&after),
        "总结卡: {sent}"
    );
}
