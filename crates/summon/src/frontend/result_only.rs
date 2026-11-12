use std::cell::{Cell, RefCell};

use super::{ChatRef, Frontend, MsgRef, StreamId, StreamPatchOutcome};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResultOnlyOutcome {
    Completed,
    Failed,
    Interrupted,
    Deferred,
}

#[derive(Debug, Default)]
enum BufferedResult {
    #[default]
    Empty,
    Provisional,
    Messages(Vec<String>),
    Card {
        json: String,
        continuations: Vec<String>,
    },
}

/// 定时轮的“仅结果”前端：过程投影照常运行，但所有 live-card 操作只落内存；轮末把权威回复
/// 作为终态消息或图片卡交给真实前端。平台单卡预算要求分段时仍会发送多段，避免截断结果。
pub(crate) struct ResultOnlyFrontend<'a> {
    inner: &'a dyn Frontend,
    next_id: Cell<u64>,
    buffered: RefCell<BufferedResult>,
    result_body: RefCell<Option<String>>,
    /// 缓冲里最后一条终态消息带的会话列表预览。终态缓冲被替换、或之后又追加了普通消息时作废:
    /// 预览只属于它跟着的那条消息。
    tail_feed: RefCell<Option<String>>,
    explicit_failure: Cell<bool>,
    failure_notice_attempted: Cell<bool>,
}

impl<'a> ResultOnlyFrontend<'a> {
    pub(crate) fn new(inner: &'a dyn Frontend) -> Self {
        Self {
            inner,
            next_id: Cell::new(0),
            buffered: RefCell::new(BufferedResult::Empty),
            result_body: RefCell::new(None),
            tail_feed: RefCell::new(None),
            explicit_failure: Cell::new(false),
            failure_notice_attempted: Cell::new(false),
        }
    }

    fn fake_stream_id(&self) -> StreamId {
        let next = self.next_id.get().saturating_add(1);
        self.next_id.set(next);
        StreamId(next)
    }

    fn provisional(&self) {
        self.explicit_failure.set(false);
        self.tail_feed.borrow_mut().take();
        *self.buffered.borrow_mut() = BufferedResult::Provisional;
    }

    fn terminal_message(&self, body: &str, append: bool) {
        self.tail_feed.borrow_mut().take();
        let mut buffered = self.buffered.borrow_mut();
        if append {
            match &mut *buffered {
                BufferedResult::Messages(parts) => {
                    parts.push(body.to_string());
                    return;
                }
                BufferedResult::Card { continuations, .. } => {
                    continuations.push(body.to_string());
                    return;
                }
                _ => {}
            }
        }
        *buffered = BufferedResult::Messages(vec![body.to_string()]);
    }

    pub(crate) fn flush(&self, chat: &ChatRef, outcome: ResultOnlyOutcome) {
        // 被打断的轮由 chat 线程统一发「定时任务 SCH-x 被打断，未送达」,这里缓冲的过程与半截结果
        // 一概不发:发了就和那句「未送达」互相矛盾。
        if matches!(
            outcome,
            ResultOnlyOutcome::Deferred | ResultOnlyOutcome::Interrupted
        ) {
            return;
        }
        let failure_notice_attempted = self.failure_notice_attempted.replace(false);
        let buffered = std::mem::take(&mut *self.buffered.borrow_mut());
        let result_body = self.result_body.borrow_mut().take();
        // 预览只给成功完成的那份回复:失败时发出的是提示或带失败后缀的正文。
        let tail_feed = self
            .tail_feed
            .borrow_mut()
            .take()
            .filter(|_| outcome == ResultOnlyOutcome::Completed);
        let explicit_failure = self.explicit_failure.replace(false);
        if outcome == ResultOnlyOutcome::Failed
            && failure_notice_attempted
            && result_body.is_none()
            && matches!(
                buffered,
                BufferedResult::Empty | BufferedResult::Provisional
            )
        {
            return;
        }
        let failure_suffix = if failure_notice_attempted {
            summon_i18n::text!("rc09ed261aa")
        } else {
            summon_i18n::text!("r354ead8311")
        };
        if outcome == ResultOnlyOutcome::Completed
            && result_body
                .as_deref()
                .is_some_and(summon_core::messaging::result_control::is_silent_reset_result)
        {
            return;
        }
        // 兜底正文直接取自 agent 的原始 result,末尾的总结标签由这里拿掉。
        let result_body =
            result_body.map(|body| summon_core::feed_tag::strip_feed_tag(&body).into_owned());
        let buffered = match buffered {
            BufferedResult::Card {
                json,
                continuations,
            } => {
                let delivered = self.inner.send_card(chat, &json);
                if delivered.is_ok() {
                    let count = continuations.len();
                    for (index, continuation) in continuations.into_iter().enumerate() {
                        let feed = tail_feed.as_deref().filter(|_| index + 1 == count);
                        send_or_retry(self.inner, chat, &continuation, feed);
                    }
                    self.inner.settle_last_sent_as_reply();
                    return;
                }
                let fallback = result_body.unwrap_or_else(|| match outcome {
                    ResultOnlyOutcome::Completed => summon_i18n::text!("r06f1a131bd").to_string(),
                    ResultOnlyOutcome::Failed => summon_i18n::text!("rd7adab8ddc").to_string(),
                    ResultOnlyOutcome::Interrupted | ResultOnlyOutcome::Deferred => {
                        unreachable!("{}", summon_i18n::format!("r137812584f"))
                    }
                });
                for part in self.inner.split_final_reply(chat, &fallback) {
                    send_or_retry(self.inner, chat, &part, None);
                }
                return;
            }
            other => other,
        };
        // 缓冲里的终态消息原样发出时,最后一条带上它的预览;兜底文案不带。
        let mut feed_for_last = None;
        let messages = match (outcome, buffered) {
            (ResultOnlyOutcome::Completed, BufferedResult::Messages(parts))
                if !parts.is_empty() =>
            {
                feed_for_last = tail_feed;
                parts
            }
            (ResultOnlyOutcome::Failed, BufferedResult::Messages(mut parts))
                if !parts.is_empty() =>
            {
                if !explicit_failure {
                    let last = parts.last_mut().expect(summon_i18n::text!("rb59aaa63f1"));
                    last.push_str(&format!("\n\n{failure_suffix}"));
                }
                parts
            }
            (ResultOnlyOutcome::Completed, _) => result_body
                .map(|body| vec![body])
                .unwrap_or_else(|| vec![summon_i18n::text!("raf868523ce").to_string()]),
            (ResultOnlyOutcome::Failed, _) => result_body
                .map(|body| vec![format!("{body}\n\n{failure_suffix}")])
                .unwrap_or_else(|| vec![summon_i18n::text!("rd7adab8ddc").to_string()]),
            (ResultOnlyOutcome::Interrupted | ResultOnlyOutcome::Deferred, _) => {
                unreachable!("{}", summon_i18n::format!("r137812584f"))
            }
        };
        let parts: Vec<String> = messages
            .iter()
            .flat_map(|body| self.inner.split_final_reply(chat, body))
            .collect();
        let count = parts.len();
        for (index, part) in parts.iter().enumerate() {
            let feed = feed_for_last.as_deref().filter(|_| index + 1 == count);
            send_or_retry(self.inner, chat, part, feed);
        }
        self.inner.settle_last_sent_as_reply();
    }
}

fn send_or_retry(frontend: &dyn Frontend, chat: &ChatRef, body: &str, feed: Option<&str>) {
    let sent = match feed {
        Some(feed) => frontend.send_message_with_feed(chat, body, feed),
        None => frontend.send_message(chat, body),
    };
    if let Err(error) = sent {
        crate::observe::log::log(&summon_i18n::format!("r06adcbb3a5", chat.agent_name));
        enqueue_retry(chat, body);
    }
}

fn enqueue_retry(chat: &ChatRef, body: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let event = summon_core::outbox::OutboxEvent {
        id: summon_core::outbox::format_id(now, chat.agent_name, &nonce[..8]),
        agent: chat.agent_name.to_string(),
        delivery_scope: chat.delivery_scope.to_string(),
        created_ms: now,
        attempts: 0,
        next_attempt_ms: now,
        last_error: None,
        kind: summon_core::outbox::OutboxKind::TurnReply {
            chat_id: chat.chat_id.to_string(),
            chat_type: chat.chat_type.to_string(),
            text: body.to_string(),
            parent_message_id: None,
        },
    };
    if let Err(error) = crate::runtime::outbox_store::OutboxStore::at_run_dir().enqueue(&event) {
        crate::observe::log::log(&summon_i18n::format!("r8b1a53ed10", chat.agent_name));
    }
}

impl Frontend for ResultOnlyFrontend<'_> {
    fn send_failure_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<()> {
        self.failure_notice_attempted.set(true);
        self.inner.send_failure_card(chat, card_json)
    }

    fn result_only(&self) -> bool {
        true
    }

    fn reset_result_buffer(&self) {
        self.tail_feed.borrow_mut().take();
        *self.buffered.borrow_mut() = BufferedResult::Empty;
        self.result_body.borrow_mut().take();
        self.explicit_failure.set(true);
    }

    fn stage_result_body(&self, body: &str) {
        *self.result_body.borrow_mut() = Some(body.to_string());
    }

    fn send_message(&self, _chat: &ChatRef, body: &str) -> anyhow::Result<()> {
        self.terminal_message(body, true);
        Ok(())
    }

    fn send_message_with_feed(
        &self,
        _chat: &ChatRef,
        body: &str,
        feed: &str,
    ) -> anyhow::Result<()> {
        self.terminal_message(body, true);
        *self.tail_feed.borrow_mut() = Some(feed.to_string());
        Ok(())
    }

    fn split_final_reply(&self, chat: &ChatRef, body: &str) -> Vec<String> {
        self.inner.split_final_reply(chat, body)
    }

    fn resolve_inline_images(
        &self,
        chat: &ChatRef,
        body: &str,
    ) -> std::collections::BTreeMap<String, String> {
        self.inner.resolve_inline_images(chat, body)
    }

    fn stream_begin(
        &self,
        _chat: &ChatRef,
        _content: &str,
        _with_interrupt: bool,
    ) -> anyhow::Result<StreamId> {
        self.provisional();
        Ok(self.fake_stream_id())
    }

    fn stream_patch(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
        self.provisional();
        Ok(())
    }

    fn stream_begin_card(&self, _chat: &ChatRef, _card_json: &str) -> anyhow::Result<StreamId> {
        self.provisional();
        Ok(self.fake_stream_id())
    }

    fn stream_patch_card(&self, _id: StreamId, _card_json: &str) -> anyhow::Result<()> {
        self.provisional();
        Ok(())
    }

    fn stream_patch_card_observed(
        &self,
        _id: StreamId,
        _card_json: &str,
    ) -> anyhow::Result<StreamPatchOutcome> {
        self.provisional();
        Ok(StreamPatchOutcome::Applied)
    }

    fn stream_finish(&self, _id: StreamId, content: &str) -> anyhow::Result<()> {
        self.explicit_failure.set(false);
        self.terminal_message(content, false);
        Ok(())
    }

    fn stream_freeze(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
        self.provisional();
        Ok(())
    }

    fn stream_finish_card(&self, _id: StreamId, _card_json: &str) -> anyhow::Result<()> {
        self.tail_feed.borrow_mut().take();
        *self.buffered.borrow_mut() = BufferedResult::Card {
            json: _card_json.to_string(),
            continuations: Vec::new(),
        };
        Ok(())
    }

    fn stream_freeze_card(&self, _id: StreamId, _card_json: &str) -> anyhow::Result<()> {
        self.provisional();
        Ok(())
    }

    fn react(&self, _msg: &MsgRef, key: &str) -> anyhow::Result<String> {
        Ok(format!("result-only-{key}"))
    }

    fn unreact(&self, _msg: &MsgRef, _reaction_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn send_card(&self, _chat: &ChatRef, _card_json: &str) -> anyhow::Result<()> {
        self.tail_feed.borrow_mut().take();
        *self.buffered.borrow_mut() = BufferedResult::Card {
            json: _card_json.to_string(),
            continuations: Vec::new(),
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CaptureFrontend {
        messages: RefCell<Vec<String>>,
        cards: RefCell<Vec<String>>,
        image_requests: Cell<usize>,
        /// 带会话列表预览发出的消息:(正文, 总结)。
        feeds: RefCell<Vec<(String, String)>>,
    }

    impl Frontend for CaptureFrontend {
        fn send_message_with_feed(
            &self,
            chat: &ChatRef,
            body: &str,
            feed: &str,
        ) -> anyhow::Result<()> {
            self.feeds
                .borrow_mut()
                .push((body.to_string(), feed.to_string()));
            self.send_message(chat, body)
        }

        fn send_message(&self, _chat: &ChatRef, body: &str) -> anyhow::Result<()> {
            self.messages.borrow_mut().push(body.to_string());
            Ok(())
        }

        fn resolve_inline_images(
            &self,
            _chat: &ChatRef,
            _body: &str,
        ) -> std::collections::BTreeMap<String, String> {
            self.image_requests
                .set(self.image_requests.get().saturating_add(1));
            std::collections::BTreeMap::from([(
                "/tmp/chart.png".to_string(),
                "img-key".to_string(),
            )])
        }

        fn send_card(&self, _chat: &ChatRef, card_json: &str) -> anyhow::Result<()> {
            self.cards.borrow_mut().push(card_json.to_string());
            Ok(())
        }

        fn stream_begin(
            &self,
            _chat: &ChatRef,
            _content: &str,
            _with_interrupt: bool,
        ) -> anyhow::Result<StreamId> {
            unreachable!()
        }

        fn stream_patch(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
            unreachable!()
        }

        fn react(&self, _msg: &MsgRef, _key: &str) -> anyhow::Result<String> {
            unreachable!()
        }

        fn unreact(&self, _msg: &MsgRef, _reaction_id: &str) -> anyhow::Result<()> {
            unreachable!()
        }
    }

    fn chat<'a>(dir: &'a std::path::Path) -> ChatRef<'a> {
        ChatRef {
            chat_id: "oc_result",
            chat_type: "group",
            agent_dir: dir,
            agent_name: "tester",
            delivery_scope: "scope",
            thread_anchor: None,
        }
    }

    #[test]
    fn silent_reset_result_consumes_all_buffered_output_without_sending() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        let id = buffered.stream_begin_card(&chat, "过程").unwrap();
        buffered.stage_result_body(summon_core::messaging::result_control::SILENT_RESET_KEYWORD);
        buffered.stream_finish_card(id, "完成过程").unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Completed);
        assert!(inner.messages.borrow().is_empty());
        assert!(inner.cards.borrow().is_empty());
        assert_eq!(inner.image_requests.get(), 0);
    }

    #[test]
    fn silent_reset_does_not_match_progress_or_normalize_result_whitespace() {
        let keyword = summon_core::messaging::result_control::SILENT_RESET_KEYWORD;
        for body in [
            "正常结果".to_string(),
            format!("{keyword}\n"),
            format!(" {keyword}"),
        ] {
            let inner = CaptureFrontend::default();
            let buffered = ResultOnlyFrontend::new(&inner);
            let dir = tempfile::tempdir().unwrap();
            let chat = chat(dir.path());
            let id = buffered.stream_begin(&chat, keyword, false).unwrap();
            buffered.stage_result_body(&body);
            buffered.stream_finish(id, &body).unwrap();
            buffered.flush(&chat, ResultOnlyOutcome::Completed);
            assert_eq!(*inner.messages.borrow(), vec![body]);
        }
    }

    #[test]
    fn silent_reset_result_does_not_suppress_a_failed_turn() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered.stage_result_body(summon_core::messaging::result_control::SILENT_RESET_KEYWORD);
        buffered.flush(&chat, ResultOnlyOutcome::Failed);
        assert!(!inner.messages.borrow().is_empty());
    }

    /// 被打断的轮什么都不从缓冲里发:过程卡、半截正文、已缓冲的终态消息都不发,「未送达」那句由
    /// chat 线程统一发。
    #[test]
    fn interrupted_turn_flushes_nothing_from_the_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        let staged: [&dyn Fn(&ResultOnlyFrontend); 4] = [
            &|buffered| {
                let id = buffered.stream_begin_card(&chat, "过程").unwrap();
                buffered
                    .stream_finish_card(id, "{\"paused\":true}")
                    .unwrap();
            },
            &|buffered| {
                let id = buffered.stream_begin(&chat, "过程", false).unwrap();
                buffered.stream_finish(id, "半截").unwrap();
            },
            &|buffered| buffered.stage_result_body("半截正文"),
            &|buffered| {
                buffered
                    .send_message_with_feed(&chat, "回复", "日报已生成")
                    .unwrap();
            },
        ];
        for stage in staged {
            let inner = CaptureFrontend::default();
            let buffered = ResultOnlyFrontend::new(&inner);
            stage(&buffered);
            buffered.flush(&chat, ResultOnlyOutcome::Interrupted);
            assert!(inner.messages.borrow().is_empty());
            assert!(inner.cards.borrow().is_empty());
            assert!(inner.feeds.borrow().is_empty());
        }
    }

    #[test]
    fn process_updates_are_suppressed_and_terminal_text_is_flushed_once() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        let id = buffered.stream_begin_card(&chat, "过程 1").unwrap();
        buffered.stream_patch_card(id, "过程 2").unwrap();
        assert!(inner.messages.borrow().is_empty());
        buffered.stream_finish(id, "最终结果").unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Completed);
        assert_eq!(inner.messages.borrow().as_slice(), ["最终结果"]);
    }

    #[test]
    fn failure_card_becomes_one_explicit_failure_message() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered.send_card(&chat, "{\"failure\":true}").unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Failed);
        assert!(inner.messages.borrow().is_empty());
        assert_eq!(inner.cards.borrow().as_slice(), ["{\"failure\":true}"]);
    }

    #[test]
    fn explicit_failure_replaces_partial_reply() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        let id = buffered.stream_begin(&chat, "部分", true).unwrap();
        buffered.stream_finish(id, "部分结果").unwrap();
        buffered.reset_result_buffer();
        buffered
            .send_message(&chat, "执行失败:provider error")
            .unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Failed);
        assert_eq!(
            inner.messages.borrow().as_slice(),
            ["执行失败:provider error"]
        );
    }

    #[test]
    fn api_error_keeps_partial_body_and_marks_it_failed() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered.stage_result_body("已经完成前半段");
        let id = buffered.stream_begin(&chat, "过程", true).unwrap();
        buffered.stream_freeze(id, "中止").unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Failed);
        assert_eq!(inner.messages.borrow().len(), 1);
        let messages = inner.messages.borrow();
        let message = &messages[0];
        assert!(message.contains("已经完成前半段"));
        assert!(message.contains("执行失败"));
        assert!(message.contains("不完整结果"));
    }

    /// 定时轮的终态在轮末才真正发出,总结跟着缓冲里那条消息一起发。
    #[test]
    fn result_feed_travels_with_the_buffered_reply() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered.send_card(&chat, "{}").unwrap();
        buffered
            .send_message_with_feed(&chat, "续段", "日报已生成")
            .unwrap();
        assert!(inner.feeds.borrow().is_empty(), "轮末之前什么都不发");
        buffered.flush(&chat, ResultOnlyOutcome::Completed);
        assert_eq!(
            &*inner.feeds.borrow(),
            &[("续段".to_string(), "日报已生成".to_string())]
        );
    }

    /// 失败时发出的是带失败后缀的正文,agent 写的成功总结不能挂上去。
    #[test]
    fn result_feed_is_dropped_when_the_turn_fails() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered
            .send_message_with_feed(&chat, "回复", "日报已生成")
            .unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Failed);
        assert!(inner.feeds.borrow().is_empty());
        assert_eq!(inner.messages.borrow().len(), 1);
    }

    /// 预览只属于它跟着的那条消息:同一个消息缓冲里后面又追加了一条不带总结的,旧总结作废。
    #[test]
    fn an_appended_untagged_message_discards_the_earlier_feed() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered
            .send_message_with_feed(&chat, "早先", "日报已生成")
            .unwrap();
        buffered.send_message(&chat, "最终").unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Completed);
        assert!(inner.feeds.borrow().is_empty());
        assert_eq!(&*inner.messages.borrow(), &["早先", "最终"]);
    }

    /// 结果卡的续段带预览只在本轮成功完成时:失败轮照发卡与续段,但续段不挂总结。
    #[test]
    fn card_continuation_feed_needs_a_completed_turn() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered.send_card(&chat, "{}").unwrap();
        buffered
            .send_message_with_feed(&chat, "续段", "日报已生成")
            .unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Failed);
        assert!(inner.feeds.borrow().is_empty());
        assert_eq!(&*inner.messages.borrow(), &["续段"]);
    }

    /// 预览只属于它跟着的那条消息:之后终态被一张新卡替换,旧总结作废。
    #[test]
    fn a_later_untagged_reply_discards_the_earlier_feed() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered
            .send_message_with_feed(&chat, "早先的回复", "日报已生成")
            .unwrap();
        buffered.send_card(&chat, "{}").unwrap();
        buffered.send_message(&chat, "最终续段").unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Completed);
        assert!(inner.feeds.borrow().is_empty());
    }

    /// 兜底正文取自原始 result,末尾的总结标签不能随它发出去。
    #[test]
    fn result_fallback_drops_the_feed_tag() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered.stage_result_body("日报已生成\n<summon-feed>日报已生成</summon-feed>");
        buffered.flush(&chat, ResultOnlyOutcome::Failed);
        let messages = inner.messages.borrow();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("日报已生成"));
        assert!(!messages[0].contains("summon-feed"), "{}", messages[0]);
    }

    #[test]
    fn failed_message_buffer_keeps_partial_body_and_adds_failure_status() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered.send_message(&chat, "部分结果").unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Failed);
        let messages = inner.messages.borrow();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("部分结果"));
        assert!(messages[0].contains("执行失败"));
        assert!(messages[0].contains("不完整结果"));
    }

    #[test]
    fn api_error_with_partial_reply_survives_the_turn_consumer() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        let outbox = crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox"));
        let (out, _) = crate::runtime::daemon::card_stream_consume(
            &buffered,
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
            |on_msg, _on_str, _on_tool, on_event, _on_int, _on_tick, _on_steer| {
                let mut event = summon_core::agent_msg::AgentMessage::of(
                    summon_core::agent_msg::AgentKind::Assistant,
                );
                event.text = Some("已经完成前半段".to_string());
                on_event(&event);
                on_msg("已经完成前半段");
                Ok(crate::capture::TurnOutcome {
                    reply: Some("已经完成前半段".to_string()),
                    api_error: Some("session died".to_string()),
                    ..Default::default()
                })
            },
        );
        assert!(out.unwrap().api_error.is_some());
        buffered.flush(&chat, ResultOnlyOutcome::Failed);
        let messages = inner.messages.borrow();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("已经完成前半段"));
        assert!(messages[0].contains("执行失败"));
    }

    #[test]
    fn terminal_card_keeps_inline_image_resolution_and_flushes_once() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        let resolved = buffered.resolve_inline_images(&chat, "![图](/tmp/chart.png)");
        assert_eq!(
            resolved.get("/tmp/chart.png").map(String::as_str),
            Some("img-key")
        );
        buffered.stage_result_body("![图](/tmp/chart.png)");
        buffered
            .send_card(&chat, "{\"image_key\":\"img-key\"}")
            .unwrap();
        assert!(inner.cards.borrow().is_empty());
        buffered.flush(&chat, ResultOnlyOutcome::Completed);
        assert_eq!(inner.image_requests.get(), 1);
        assert_eq!(
            inner.cards.borrow().as_slice(),
            ["{\"image_key\":\"img-key\"}"]
        );
        assert!(inner.messages.borrow().is_empty());
    }

    #[test]
    fn terminal_card_keeps_platform_required_continuation_segments() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        buffered.send_card(&chat, "{\"first\":true}").unwrap();
        buffered.send_message(&chat, "超长结果的后续段").unwrap();
        buffered.flush(&chat, ResultOnlyOutcome::Completed);
        assert_eq!(inner.cards.borrow().as_slice(), ["{\"first\":true}"]);
        assert_eq!(inner.messages.borrow().as_slice(), ["超长结果的后续段"]);
    }

    #[test]
    fn inline_image_survives_the_turn_consumer_as_one_result_card() {
        let inner = CaptureFrontend::default();
        let buffered = ResultOnlyFrontend::new(&inner);
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(dir.path());
        let outbox = crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox"));
        let body = "结论如下\n\n![图](/tmp/chart.png)";
        let (out, _) = crate::runtime::daemon::card_stream_consume(
            &buffered,
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
            |on_msg, _on_str, _on_tool, on_event, _on_int, _on_tick, _on_steer| {
                let mut event = summon_core::agent_msg::AgentMessage::of(
                    summon_core::agent_msg::AgentKind::Assistant,
                );
                event.text = Some(body.to_string());
                on_event(&event);
                on_msg(body);
                Ok(crate::capture::TurnOutcome {
                    reply: Some(body.to_string()),
                    ..Default::default()
                })
            },
        );
        assert!(out.unwrap().api_error.is_none());
        assert!(inner.cards.borrow().is_empty());
        buffered.flush(&chat, ResultOnlyOutcome::Completed);
        assert_eq!(inner.image_requests.get(), 1);
        assert_eq!(inner.cards.borrow().len(), 1);
        assert!(inner.cards.borrow()[0].contains("img-key"));
        assert!(inner.messages.borrow().is_empty());
    }
}
