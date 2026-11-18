//! 无头全量事件流落盘(IO 半):把 drain seam 处的 typed `AgentMessage` tee 进
//! `<base>/threads/<tag>/events.jsonl`(append-only,一行一个 `EventRecord`)。
//!
//! 可观测性是 best-effort:**任何写失败都 log-ignore、绝不中断本轮**(打开失败 → sink 为 None,
//! 后续 write 直接跳过)。`ts` 在落盘时盖真实毫秒(IO 层取系统时间无妨),`seq` 单文件单调自增。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use summon_core::agent_msg::AgentMessage;
use summon_core::event_log::{EventRecord, EventStamp};

/// 单文件上限(PF2):events.jsonl 每轮 tee 全量事件(thinking/工具入参/结果),重度使用按月可到
/// GB 级且无任何清理。超限在**轮初 open 时**轮转(单轮内不切,保证一轮事件不跨文件)。
pub(crate) const CHAT_LOG_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// PF2:超限轮转——文件超过 `max` 字节时 rename 成 `<name>.1`(覆盖上一代,只保留一代),
/// 新内容从空文件重新 append。轮转失败忽略(继续 append,宁可大不可断观测)。返回是否轮转。
pub(crate) fn rotate_if_oversized(path: &Path, max: u64) -> bool {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > max)
        .unwrap_or(false);
    if !too_big {
        return false;
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    std::fs::rename(path, std::path::PathBuf::from(rotated)).is_ok()
}

/// 流式增量合并后最晚多久落盘。`agents tail` 跟随时正文以这个节拍成段出现。
const DELTA_FLUSH_AGE: Duration = Duration::from_secs(1);
/// 合并中的增量正文到这个字节数就落盘,一条记录不无限长大。
const DELTA_FLUSH_BYTES: usize = 4096;
/// 后台落盘线程检查到期缓冲的间隔。
const DELTA_FLUSH_TICK: Duration = Duration::from_millis(200);

/// 一个 chat 一轮的事件写口:持 append 模式文件句柄 + 单调 seq + 本轮出处。
///
/// codex 的流式增量(每条一两个字)按段合并后再写(判据见
/// [`EventRecord::is_mergeable_delta`]):下一条不属于同一段、合并中的正文超过
/// [`DELTA_FLUSH_BYTES`]、或已缓冲超过 [`DELTA_FLUSH_AGE`] 时落盘。到期检查由进程内一条后台
/// 线程做,不依赖写口使用方的循环:provider 卡住不再出声时,最后一段增量也会按期落盘。
/// 写口销毁(轮结束、被打断、线程 unwind)与 daemon 停机([`flush_open_sinks`])时写掉缓冲。
/// 进程被 SIGKILL 或 abort 时,最多丢掉一段未到期的缓冲(不超过 1 秒、4096 字节)。
///
/// 合并记录的 `ts` 是该段首条增量的时刻;按 `ts` 推算的「最后活动时间」最多早 1 秒。
pub struct EventSink {
    inner: Arc<Mutex<SinkInner>>,
}

struct SinkInner {
    file: File,
    /// 会话目录:写失败报空间不足时记在它上面(见 `observe::storage_full`)。
    scope: std::path::PathBuf,
    seq: u64,
    stamp: EventStamp,
    /// 合并中的增量与它开始缓冲的时刻。
    pending: Option<(EventRecord, Instant)>,
}

/// 进程里开着的写口。停机时主线程据此写掉各写口缓冲的增量:chat 线程可能来不及走到销毁。
static OPEN_SINKS: Mutex<Vec<Weak<Mutex<SinkInner>>>> = Mutex::new(Vec::new());

/// 写掉所有开着的写口里缓冲的增量。daemon 停机收尾时调。
pub(crate) fn flush_open_sinks() {
    for_each_open_sink(SinkInner::flush_pending);
}

fn for_each_open_sink(mut each: impl FnMut(&mut SinkInner)) {
    let sinks = OPEN_SINKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for sink in sinks.iter().filter_map(Weak::upgrade) {
        each(&mut sink.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

/// 首个写口打开时起后台落盘线程,此后常驻:每个节拍写掉到期的缓冲。
fn ensure_flush_ticker() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("events-flush".into())
            .spawn(|| loop {
                std::thread::sleep(DELTA_FLUSH_TICK);
                for_each_open_sink(SinkInner::flush_due);
            });
        if let Err(e) = spawned {
            crate::observe::log::log(&summon_i18n::format!("event_log.flush_ticker_failed"));
        }
    });
}

impl EventSink {
    /// 打开(或新建)`<base>/threads/<tag>/events.jsonl` 的 append 写口;建目录。
    /// 超过单文件上限先轮转(PF2)。失败上抛给调用方,由其转成 `None`(不开 sink),不影响本轮。
    /// `stamp` 是本轮出处,此后写入的每条记录都盖上它。
    pub fn open(base: &Path, tag: &str, stamp: EventStamp) -> anyhow::Result<Self> {
        use crate::observe::storage_full::{noted, thread_scope};
        let dir = thread_scope(base, tag);
        noted(&dir, std::fs::create_dir_all(&dir))?;
        let path = dir.join("events.jsonl");
        rotate_if_oversized(&path, CHAT_LOG_MAX_BYTES);
        let file = noted(
            &dir,
            OpenOptions::new().create(true).append(true).open(&path),
        )?;
        let inner = Arc::new(Mutex::new(SinkInner {
            file,
            scope: dir,
            seq: 0,
            stamp,
            pending: None,
        }));
        {
            let mut open = OPEN_SINKS.lock().unwrap_or_else(PoisonError::into_inner);
            open.retain(|sink| sink.strong_count() > 0);
            open.push(Arc::downgrade(&inner));
        }
        ensure_flush_ticker();
        Ok(Self { inner })
    }

    fn lock(&self) -> MutexGuard<'_, SinkInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 把一个事件 tee 进文件:盖 `ts = now_ms`、`seq += 1`,投影成 `EventRecord`,序列化后写一行;
    /// 可合并的流式增量先缓冲。全程 log-ignore:观测落盘失败不该让一轮挂掉。
    pub fn write(&mut self, ev: &AgentMessage) {
        // 子 agent 的事件不落盘:读方按 `end_turn` 收轮、按 Reply 重建正文,混进来会串到主轮里。
        if ev.parent_run_id.is_some() {
            return;
        }
        let mut inner = self.lock();
        let rec = EventRecord::from_event(ev, now_ms(), 0).with_stamp(&inner.stamp);
        inner.push(rec);
    }

    /// 写一条入站用户消息记录(kind=`user_input`),让 `agents tail` 看到触发本轮的消息。
    pub fn write_user_input(&mut self, text: &str) {
        let mut inner = self.lock();
        let rec = EventRecord::user_input(text, now_ms(), 0).with_stamp(&inner.stamp);
        inner.push(rec);
    }
}

impl Drop for EventSink {
    fn drop(&mut self) {
        self.lock().flush_pending();
    }
}

impl SinkInner {
    fn flush_due(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() >= DELTA_FLUSH_AGE)
        {
            self.flush_pending();
        }
    }

    fn push(&mut self, mut rec: EventRecord) {
        if let Some((pending, since)) = self.pending.as_mut() {
            if pending.absorb_delta(&rec) {
                let long = pending.text.as_ref().map_or(0, String::len) >= DELTA_FLUSH_BYTES;
                if long || since.elapsed() >= DELTA_FLUSH_AGE {
                    self.flush_pending();
                }
                return;
            }
            self.flush_pending();
        }
        self.seq += 1;
        rec.seq = self.seq;
        if rec.is_mergeable_delta() {
            self.pending = Some((rec, Instant::now()));
        } else {
            self.write_record(&rec);
        }
    }

    fn flush_pending(&mut self) {
        if let Some((rec, _)) = self.pending.take() {
            self.write_record(&rec);
        }
    }

    /// 序列化一条记录写一行,全程 log-ignore(观测落盘失败不该让一轮挂掉)。
    fn write_record(&mut self, rec: &EventRecord) {
        match serde_json::to_string(rec) {
            Ok(mut line) => {
                line.push('\n');
                // 正文连同换行一次写出。
                if let Err(e) = self.file.write_all(line.as_bytes()) {
                    crate::observe::storage_full::note(&self.scope, &e);
                    crate::observe::log::log(&summon_i18n::format!("rd0b31b6bde"));
                }
            }
            // 序列化失败极罕见(都是可序列化字段);忽略这一条,不中断。
            Err(e) => crate::observe::log::log(&summon_i18n::format!("rd0aeeb674c")),
        }
    }
}

/// best-effort:把一条入站用户消息追加进该 chat 的 events.jsonl(开 append 写口写一行即关)。
/// 失败 log-ignore——观测落盘绝不中断本轮。供 dispatcher 在注入 prompt 那刻调用。
pub fn append_user_input(base: &std::path::Path, tag: &str, text: &str, stamp: EventStamp) {
    match EventSink::open(base, tag, stamp) {
        Ok(mut sink) => sink.write_user_input(text),
        Err(e) => crate::observe::log::log(&summon_i18n::format!("r64885af509")),
    }
}

/// best-effort:把会话建立时 provider 下发的候选模型追加进该 chat 的 events.jsonl,
/// 供 `agents tail` 显示。失败 log-ignore —— 观测落盘绝不影响建会话。
pub fn append_model_catalog(base: &Path, tag: &str, text: &str, stamp: EventStamp) {
    match EventSink::open(base, tag, stamp) {
        Ok(sink) => {
            let mut inner = sink.lock();
            let rec = EventRecord::model_catalog(text, now_ms(), 0).with_stamp(&inner.stamp);
            inner.push(rec);
        }
        Err(e) => crate::observe::log::log(&summon_i18n::format!("r5e0c4a7d21")),
    }
}

/// 当前 unix 毫秒;时钟回拨(早于 EPOCH)兜底 0。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 读回某 chat 落盘的事件流(孤儿卡收敛用)。
///
/// **只读最后 `max_lines` 行**:events.jsonl 单文件可到 50MB(见 `CHAT_LOG_MAX_BYTES`),
/// 而这里只需要最后一轮。整份读进内存是把一个冷路径变成内存尖峰。
///
/// 坏行宽松跳过(与写侧的 best-effort 对称):可观测性数据不该因为一行畸形就整体失效。
/// 文件不存在 / 读失败 → 空 Vec,调用方据此退回"没有正文可渲"。
pub fn read_tail_records(
    base: &std::path::Path,
    tag: &str,
    max_lines: usize,
) -> Vec<summon_core::event_log::EventRecord> {
    let path = base.join("threads").join(tag).join("events.jsonl");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::agent_msg::{AgentKind, AgentMessage};
    use summon_core::event_log::EventRecord;

    #[test]
    fn open_and_write_appends_jsonl_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = EventSink::open(dir.path(), "p1234", EventStamp::default()).unwrap();

        let mut thinking = AgentMessage::of(AgentKind::Thinking);
        thinking.text = Some("hmm".to_string());
        sink.write(&thinking);

        let mut tooluse = AgentMessage::of(AgentKind::ToolUse);
        tooluse.tool = Some("Read".to_string());
        tooluse.tool_input = Some(serde_json::json!({"file_path": "/etc/foo"}));
        sink.write(&tooluse);

        let path = dir
            .path()
            .join("threads")
            .join("p1234")
            .join("events.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "应写两行");

        let r0: EventRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(r0.kind, "thinking");
        assert_eq!(r0.seq, 1);
        assert_eq!(r0.text.as_deref(), Some("hmm"));

        let r1: EventRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r1.kind, "tool_use");
        assert_eq!(r1.seq, 2);
        assert_eq!(r1.tool.as_deref(), Some("Read"));
        assert!(r1.tool_input.is_some(), "tool_input 应落盘");
        assert!(r1.ts >= r0.ts, "ts 单调不减");
    }

    #[test]
    fn model_catalog_note_lands_in_the_chat_event_log() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = EventStamp {
            agent: Some("a1".into()),
            ..EventStamp::default()
        };
        append_model_catalog(dir.path(), "g-a", "gpt-5.6-sol, composer-2.5", stamp);
        let content = std::fs::read_to_string(dir.path().join("threads/g-a/events.jsonl")).unwrap();
        let rec: EventRecord = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(rec.kind, "model_catalog");
        assert_eq!(rec.text.as_deref(), Some("gpt-5.6-sol, composer-2.5"));
        assert_eq!(rec.agent.as_deref(), Some("a1"));
    }

    #[test]
    fn sink_stamps_every_record_it_writes() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = EventStamp {
            agent: Some("arcadia".into()),
            model: Some("opus".into()),
            turn: Some("turn-1".into()),
        };
        append_user_input(dir.path(), "g1", "张三:hi", stamp.clone());
        let mut sink = EventSink::open(dir.path(), "g1", stamp).unwrap();
        sink.write(&AgentMessage::of(AgentKind::ToolUse));

        let path = dir.path().join("threads").join("g1").join("events.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let recs: Vec<EventRecord> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(recs.len(), 2);
        for rec in recs {
            assert_eq!(rec.agent.as_deref(), Some("arcadia"), "{}", rec.kind);
            assert_eq!(rec.model.as_deref(), Some("opus"), "{}", rec.kind);
            assert_eq!(rec.turn.as_deref(), Some("turn-1"), "{}", rec.kind);
        }
    }

    #[test]
    fn open_rotates_oversized_events_file() {
        // PF2:超限的 events.jsonl 在轮初被 rename 成 .1,新一轮从空文件 append。
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("threads").join("p9");
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("events.jsonl");
        std::fs::write(&p, "x".repeat(100)).unwrap();
        assert!(rotate_if_oversized(&p, 50), "超限应轮转");
        assert!(d.join("events.jsonl.1").exists(), "上一代保留为 .1");
        assert!(!p.exists());
        std::fs::write(&p, "y").unwrap();
        assert!(!rotate_if_oversized(&p, 50), "未超限不动");
        assert!(p.exists());
    }

    fn delta(text: &str, item: &str) -> AgentMessage {
        let mut ev = AgentMessage::of(AgentKind::Assistant);
        ev.text = Some(text.to_string());
        ev.partial = true;
        ev.stream_source = Some(summon_core::agent_msg::ContentStreamSource::CodexRpcDelta);
        ev.source_item_id = Some(item.to_string());
        ev
    }

    fn records(dir: &std::path::Path, tag: &str) -> Vec<EventRecord> {
        let path = dir.join("threads").join(tag).join("events.jsonl");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn codex_deltas_land_as_one_record_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = EventSink::open(dir.path(), "g1", EventStamp::default()).unwrap();
        for piece in ["你", "好", ",世", "界"] {
            sink.write(&delta(piece, "msg_1"));
        }
        assert!(records(dir.path(), "g1").is_empty(), "同段增量先缓冲");
        // 换 item 是段边界:上一段落盘。
        sink.write(&delta("再见", "msg_2"));
        let recs = records(dir.path(), "g1");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text.as_deref(), Some("你好,世界"));
        assert_eq!(recs[0].partial, Some(true));
        assert_eq!(recs[0].source_item_id.as_deref(), Some("msg_1"));
        // 非增量事件先写掉缓冲,顺序与 seq 不乱。
        sink.write(&AgentMessage::of(AgentKind::ToolUse));
        let recs = records(dir.path(), "g1");
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[1].text.as_deref(), Some("再见"));
        assert_eq!(recs[2].kind, "tool_use");
        assert_eq!(
            recs.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn buffered_deltas_survive_turn_end_shutdown_and_long_pauses() {
        let dir = tempfile::tempdir().unwrap();
        // 写口销毁(轮结束 / 被打断)写掉缓冲。
        {
            let mut sink = EventSink::open(dir.path(), "end", EventStamp::default()).unwrap();
            sink.write(&delta("半句", "msg_1"));
        }
        assert_eq!(records(dir.path(), "end")[0].text.as_deref(), Some("半句"));

        // 停机:写口还在 chat 线程手里,主线程写掉缓冲。
        let mut alive = EventSink::open(dir.path(), "stop", EventStamp::default()).unwrap();
        alive.write(&delta("停机前", "msg_1"));
        flush_open_sinks();
        assert_eq!(
            records(dir.path(), "stop")[0].text.as_deref(),
            Some("停机前")
        );
        drop(alive);
        assert_eq!(records(dir.path(), "stop").len(), 1, "不重复写");

        // 之后没有任何事件(provider 卡住):后台线程按期写掉,不等下一条事件或写口销毁。
        let mut sink = EventSink::open(dir.path(), "pause", EventStamp::default()).unwrap();
        sink.write(&delta("卡住前最后一段", "msg_1"));
        assert!(records(dir.path(), "pause").is_empty(), "未到期先缓冲");
        let deadline = Instant::now() + DELTA_FLUSH_AGE * 3;
        while records(dir.path(), "pause").is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            records(dir.path(), "pause")[0].text.as_deref(),
            Some("卡住前最后一段"),
            "缓冲超过 {DELTA_FLUSH_AGE:?} 必须落盘"
        );
        drop(sink);

        // 合并中的正文到上限就落盘。
        let mut sink = EventSink::open(dir.path(), "long", EventStamp::default()).unwrap();
        let piece = "x".repeat(DELTA_FLUSH_BYTES / 2);
        for _ in 0..3 {
            sink.write(&delta(&piece, "msg_1"));
        }
        let recs = records(dir.path(), "long");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text.as_ref().unwrap().len(), DELTA_FLUSH_BYTES);
    }

    #[test]
    fn write_is_append_across_sinks() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut s = EventSink::open(dir.path(), "g99", EventStamp::default()).unwrap();
            s.write(&AgentMessage::of(AgentKind::System));
        }
        {
            // 第二个 sink 同 tag:append,不截断前一个的内容。
            let mut s = EventSink::open(dir.path(), "g99", EventStamp::default()).unwrap();
            s.write(&AgentMessage::of(AgentKind::Result));
        }
        let path = dir.path().join("threads").join("g99").join("events.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2, "第二轮应 append 不覆盖");
    }
}
