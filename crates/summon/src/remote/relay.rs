//! 回传:跟踪被遥控会话的转录,每轮最终回复经 outbox 发回绑定的群。

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Context;
use summon_core::outbox::{OutboxEvent, OutboxKind};
use summon_core::remote::RemoteBinding;

use super::store::RemoteStore;
use crate::capture::tail_external_replies;
use crate::runtime::daemon::now_unix_ms;
use crate::runtime::outbox_store::OutboxStore;

/// 跟踪转录的起始轮询间隔(空闲时由 tail 指数放宽)。群里看回复是秒级的事,与会话镜像同节拍。
const RELAY_POLL: Duration = Duration::from_millis(300);

/// 一轮回复对应的 outbox 事件(纯组装)。`delivery_scope` 取绑定记录的 scope:drain 只投递
/// 与当前飞书 app scope 一致的事件。
pub(crate) fn relay_event(
    binding: &RemoteBinding,
    boundary: u64,
    text: &str,
    now_ms: u64,
) -> OutboxEvent {
    OutboxEvent {
        // 同一轮重放得到同一个 id(时间戳取绑定时刻、hex 取 boundary 低 32 位):崩在入队之后、
        // 写 offset 之前,重启重放这一轮时 `find` 认出它,不重复入队。转录超过 4 GiB 才可能撞号。
        id: summon_core::outbox::format_id(
            binding.created_at_ms,
            &binding.agent,
            &format!("{:08x}", boundary as u32),
        ),
        agent: binding.agent.clone(),
        delivery_scope: binding.scope.clone(),
        created_ms: now_ms,
        attempts: 0,
        next_attempt_ms: now_ms,
        last_error: None,
        kind: OutboxKind::TurnReply {
            chat_id: binding.chat_id.clone(),
            chat_type: binding.chat_type.clone(),
            text: text.to_string(),
            parent_message_id: None,
        },
    }
}

/// 从落盘 offset 起跟踪转录直到 `stop`;每轮回复先入 outbox,再把 offset 推到这一轮之后,
/// 不转发的轮也推进 offset。
pub(crate) fn run_relay(
    binding: &RemoteBinding,
    store: &RemoteStore,
    outbox: &OutboxStore,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    relay_transcript(binding, store, outbox, stop, RELAY_POLL, &mut || {})
}

/// `run_relay` 的本体。轮询间隔与首读回调由调用方给:测试用短间隔、并在首读之后才往转录里
/// 追加,不必按生产节拍真等。
pub(super) fn relay_transcript(
    binding: &RemoteBinding,
    store: &RemoteStore,
    outbox: &OutboxStore,
    stop: &AtomicBool,
    poll: Duration,
    on_ready: &mut dyn FnMut(),
) -> anyhow::Result<()> {
    let start = store
        .read_offset(&binding.chat_id)?
        .context(summon_i18n::text!("r941d47a28c"))?;
    let path = std::path::PathBuf::from(&binding.transcript_path);
    tail_external_replies(
        &path,
        start,
        poll,
        stop,
        on_ready,
        &mut |boundary, text| {
            let event = relay_event(binding, boundary, text, now_unix_ms());
            // 入队先于写 offset(同一轮先回调 on_reply 再回调 on_boundary):崩在两者之间时
            // 重启重放这一轮,`find` 挡住重复。
            if outbox.find(&event.id)?.is_none() {
                outbox.enqueue(&event)?;
            }
            Ok(())
        },
        // 每轮都落 offset,不转发的轮也一样:否则它们在下次重启时重新取得补发资格、晚发进群。
        &mut |boundary| store.write_offset(&binding.chat_id, boundary),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    fn binding(path: &Path) -> RemoteBinding {
        RemoteBinding {
            schema_version: summon_core::remote::REMOTE_BINDING_SCHEMA_VERSION,
            agent: "a".into(),
            scope: "a-s".into(),
            chat_id: "oc_a".into(),
            chat_type: "group".into(),
            session_id: "sid-1".into(),
            session_name: "n".into(),
            surface_id: "S".into(),
            workspace_id: "W".into(),
            pid: 42,
            pid_start_seconds: 1000,
            transcript_path: path.display().to_string(),
            actor_open_id: "ou".into(),
            created_at_ms: 1,
            ended: None,
        }
    }

    /// 与 transcript.rs 里 `tail_forward_turns` 的 live 测试同形:一行一条 assistant 文本记录。
    fn assistant(text: &str, stop_reason: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": "s",
            "message": {"role": "assistant", "stop_reason": stop_reason,
                "content": [{"type": "text", "text": text}]}
        })
        .to_string()
            + "\n"
    }

    fn assistant_end_turn(text: &str) -> String {
        assistant(text, "end_turn")
    }

    fn append(path: &Path, line: &str) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
    }

    fn file_len(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    /// pending 里全部 TurnReply 的正文,排序后返回(目录枚举顺序不固定)。
    fn pending_texts(outbox: &OutboxStore) -> Vec<String> {
        let mut texts: Vec<String> = outbox
            .scan_pending()
            .unwrap()
            .events
            .into_iter()
            .filter_map(|e| match e.kind {
                summon_core::outbox::OutboxKind::TurnReply { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        texts.sort();
        texts
    }

    /// 测试里回传的起始轮询间隔。它只决定多快读到新字节,不影响报哪些轮;生产节拍是 300ms
    /// 起、空闲放宽到 5s,照它真等,每次叫停都要等满手上那一觉。
    const TEST_POLL: Duration = Duration::from_millis(5);

    /// 轮询 outbox 直到攒够 `n` 条或到 15s 上限(END_TURN_GRACE 为 3s,留足余量)。
    fn wait_for_texts(outbox: &OutboxStore, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let texts = pending_texts(outbox);
            if texts.len() >= n || Instant::now() > deadline {
                return texts;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// 等回传线程把转录读到此刻的末尾:同一轮先入队、再推 offset,offset 到了末尾,末尾之前
    /// 每一轮该入的队(包括不该有的重复)都已经入了。
    fn wait_until_consumed(store: &RemoteStore, b: &RemoteBinding, transcript: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while store.read_offset(&b.chat_id).unwrap() != Some(file_len(transcript)) {
            assert!(Instant::now() < deadline, "回传 15s 内没读到转录末尾");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct RelayThread {
        stop: Arc<AtomicBool>,
        /// 首读已完成:此后追加的字节一定不在首读里。
        ready: Arc<AtomicBool>,
        handle: JoinHandle<anyhow::Result<()>>,
    }

    impl RelayThread {
        fn wait_ready(&self) {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !self.ready.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline, "回传 15s 内没完成首读");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn spawn_relay(b: &RemoteBinding, store_root: PathBuf, outbox_root: PathBuf) -> RelayThread {
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let (b, thread_stop, thread_ready) = (b.clone(), stop.clone(), ready.clone());
        let handle = std::thread::spawn(move || {
            relay_transcript(
                &b,
                &RemoteStore::new(store_root),
                &OutboxStore::new(outbox_root),
                &thread_stop,
                TEST_POLL,
                &mut || thread_ready.store(true, Ordering::SeqCst),
            )
        });
        RelayThread {
            stop,
            ready,
            handle,
        }
    }

    /// 叫停并等回传线程退出;之后 outbox 与 offset 不会再变。
    fn stop_relay(relay: RelayThread) {
        relay.stop.store(true, Ordering::SeqCst);
        relay.handle.join().unwrap().unwrap();
    }

    #[test]
    fn relays_only_turns_after_the_start_offset_and_persists_offset() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, assistant_end_turn("历史回复")).unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        let b = binding(&transcript);
        store.save(&b).unwrap();
        store
            .write_offset(&b.chat_id, file_len(&transcript))
            .unwrap();
        let outbox = OutboxStore::new(dir.path().join("outbox"));
        let relay = spawn_relay(&b, dir.path().join("remote"), dir.path().join("outbox"));
        append(&transcript, &assistant_end_turn("新回复"));
        let texts = wait_for_texts(&outbox, 1);
        stop_relay(relay);
        assert_eq!(texts, vec!["新回复".to_string()], "绑定之前的历史一条不发");
        assert_eq!(
            store.read_offset(&b.chat_id).unwrap(),
            Some(file_len(&transcript))
        );
    }

    #[test]
    fn same_turn_gets_the_same_event_id() {
        let b = binding(Path::new("/t.jsonl"));
        assert_eq!(
            relay_event(&b, 99, "x", 1).id,
            relay_event(&b, 99, "x", 2).id
        );
        assert_ne!(
            relay_event(&b, 99, "x", 1).id,
            relay_event(&b, 100, "x", 1).id
        );
    }

    /// 重启后从落盘 offset 续读:已发过的那一轮不再回调,停机期间结束的轮照发。
    #[test]
    fn resume_from_saved_offset_relays_only_turns_after_the_last_relayed_one() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, assistant_end_turn("历史回复")).unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        let b = binding(&transcript);
        store
            .write_offset(&b.chat_id, file_len(&transcript))
            .unwrap();

        let first_outbox = OutboxStore::new(dir.path().join("outbox-1"));
        let relay = spawn_relay(&b, dir.path().join("remote"), dir.path().join("outbox-1"));
        append(&transcript, &assistant_end_turn("第一轮"));
        let first = wait_for_texts(&first_outbox, 1);
        stop_relay(relay);
        assert_eq!(first, vec!["第一轮".to_string()]);
        assert_eq!(
            store.read_offset(&b.chat_id).unwrap(),
            Some(file_len(&transcript)),
            "offset 落在第一轮 end_turn 记录之后"
        );

        // 停机期间结束的一轮;第二次运行换一个空 outbox,第一轮若被重新回调就会出现在里面。
        append(&transcript, &assistant_end_turn("第二轮"));
        let second_outbox = OutboxStore::new(dir.path().join("outbox-2"));
        let relay = spawn_relay(&b, dir.path().join("remote"), dir.path().join("outbox-2"));
        let second = wait_for_texts(&second_outbox, 1);
        // 读到末尾再叫停:可能的重复回调此时都已落盘,叫停之后 outbox 不会再变。
        wait_until_consumed(&store, &b, &transcript);
        stop_relay(relay);
        let second_final = pending_texts(&second_outbox);
        assert_eq!(second, vec!["第二轮".to_string()]);
        assert_eq!(second_final, vec!["第二轮".to_string()], "第一轮不得重发");
        assert_eq!(
            store.read_offset(&b.chat_id).unwrap(),
            Some(file_len(&transcript))
        );
    }

    /// daemon 重启时会话正忙:那一轮开始于重启之前、结束于重启之后,仍要发回群里。
    #[test]
    fn turn_in_progress_at_restart_is_relayed_when_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, assistant_end_turn("上一轮")).unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        let b = binding(&transcript);
        store
            .write_offset(&b.chat_id, file_len(&transcript))
            .unwrap();
        append(&transcript, &assistant("先查一下", "tool_use"));

        let outbox = OutboxStore::new(dir.path().join("outbox"));
        let relay = spawn_relay(&b, dir.path().join("remote"), dir.path().join("outbox"));
        // 先让回传线程开读(读到的是进行中的半轮),再让这一轮收尾。
        relay.wait_ready();
        append(&transcript, &assistant_end_turn("查完了"));
        let texts = wait_for_texts(&outbox, 1);
        stop_relay(relay);
        assert_eq!(texts, vec!["查完了".to_string()]);
        assert_eq!(
            store.read_offset(&b.chat_id).unwrap(),
            Some(file_len(&transcript))
        );
    }

    /// 停机期间结束的轮超过补发上限:多出的只记日志、offset 照样越过它们;再重启一次也不晚发。
    #[test]
    fn turns_beyond_the_catchup_cap_are_skipped_for_good() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, assistant_end_turn("历史回复")).unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        let b = binding(&transcript);
        store
            .write_offset(&b.chat_id, file_len(&transcript))
            .unwrap();
        let downtime: Vec<String> = (1..=7).map(|i| format!("停机第{i}轮")).collect();
        for text in &downtime {
            append(&transcript, &assistant_end_turn(text));
        }

        // 第一次重启:补发到上限,offset 走到最后一轮之后。
        let first_outbox = OutboxStore::new(dir.path().join("outbox-1"));
        let relay = spawn_relay(&b, dir.path().join("remote"), dir.path().join("outbox-1"));
        wait_until_consumed(&store, &b, &transcript);
        stop_relay(relay);
        let mut relayed = downtime[..5].to_vec();
        relayed.sort();
        assert_eq!(pending_texts(&first_outbox), relayed, "只补发前 5 轮");
        assert_eq!(
            store.read_offset(&b.chat_id).unwrap(),
            Some(file_len(&transcript)),
            "超出上限的轮也推进 offset"
        );

        // 第二次重启:没有新轮时什么都不发;随后的新一轮照发,超限的轮不会跟着出来。
        let second_outbox = OutboxStore::new(dir.path().join("outbox-2"));
        let relay = spawn_relay(&b, dir.path().join("remote"), dir.path().join("outbox-2"));
        relay.wait_ready();
        let idle = pending_texts(&second_outbox);
        append(&transcript, &assistant_end_turn("新一轮"));
        let texts = wait_for_texts(&second_outbox, 1);
        // 晚发的轮只可能出在新一轮之前的字节里:读到末尾再叫停,它们若被报出此时都已入队。
        wait_until_consumed(&store, &b, &transcript);
        stop_relay(relay);
        let texts_final = pending_texts(&second_outbox);
        assert!(idle.is_empty(), "第二次重启不该晚发:{idle:?}");
        assert_eq!(texts, vec!["新一轮".to_string()]);
        assert_eq!(texts_final, vec!["新一轮".to_string()]);
        assert_eq!(
            store.read_offset(&b.chat_id).unwrap(),
            Some(file_len(&transcript))
        );
    }

    /// 入队之后、写 offset 之前崩掉:重启重放这一轮,同 id 已在 outbox 里,不重复入队。
    #[test]
    fn replayed_turn_already_in_outbox_is_not_enqueued_again() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let store = RemoteStore::new(dir.path().join("remote"));
        let b = binding(&transcript);
        store.write_offset(&b.chat_id, 0).unwrap();
        append(&transcript, &assistant_end_turn("崩前已入队"));
        let outbox = OutboxStore::new(dir.path().join("outbox"));
        let earlier = relay_event(&b, file_len(&transcript), "崩前已入队", 7);
        outbox.enqueue(&earlier).unwrap();

        let relay = spawn_relay(&b, dir.path().join("remote"), dir.path().join("outbox"));
        wait_until_consumed(&store, &b, &transcript);
        stop_relay(relay);
        assert_eq!(
            store.read_offset(&b.chat_id).unwrap(),
            Some(file_len(&transcript)),
            "重放的这一轮照常推进 offset"
        );
        let events = outbox.scan_pending().unwrap().events;
        assert_eq!(events.len(), 1, "同一轮只留一条");
        assert_eq!(events[0], earlier, "保留崩前入队的那条,不被覆盖");
    }
}
