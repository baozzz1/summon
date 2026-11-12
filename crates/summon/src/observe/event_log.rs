//! 无头全量事件流落盘(IO 半):把 drain seam 处的 typed `AgentMessage` tee 进
//! `<base>/threads/<tag>/events.jsonl`(append-only,一行一个 `EventRecord`)。
//!
//! 可观测性是 best-effort:**任何写失败都 log-ignore、绝不中断本轮**(打开失败 → sink 为 None,
//! 后续 write 直接跳过)。`ts` 在落盘时盖真实毫秒(IO 层取系统时间无妨),`seq` 单文件单调自增。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// 一个 chat 一轮的事件写口:持 append 模式文件句柄 + 单调 seq + 本轮出处。
pub struct EventSink {
    file: File,
    seq: u64,
    stamp: EventStamp,
}

impl EventSink {
    /// 打开(或新建)`<base>/threads/<tag>/events.jsonl` 的 append 写口;建目录。
    /// 超过单文件上限先轮转(PF2)。失败上抛给调用方,由其转成 `None`(不开 sink),不影响本轮。
    /// `stamp` 是本轮出处,此后写入的每条记录都盖上它。
    pub fn open(base: &Path, tag: &str, stamp: EventStamp) -> anyhow::Result<Self> {
        let dir = base.join("threads").join(tag);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("events.jsonl");
        rotate_if_oversized(&path, CHAT_LOG_MAX_BYTES);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            file,
            seq: 0,
            stamp,
        })
    }

    /// 把一个事件 tee 进文件:盖 `ts = now_ms`、`seq += 1`,投影成 `EventRecord`,序列化后写一行。
    /// 全程 log-ignore:观测落盘失败不该让一轮挂掉。
    pub fn write(&mut self, ev: &AgentMessage) {
        // 子 agent 的事件不落盘:读方按 `end_turn` 收轮、按 Reply 重建正文,混进来会串到主轮里。
        if ev.parent_run_id.is_some() {
            return;
        }
        self.seq += 1;
        let ts = now_ms();
        let rec = EventRecord::from_event(ev, ts, self.seq).with_stamp(&self.stamp);
        self.write_record(&rec);
    }

    /// 写一条入站用户消息记录(kind=`user_input`),让 `agents tail` 看到触发本轮的消息。
    pub fn write_user_input(&mut self, text: &str) {
        self.seq += 1;
        let rec = EventRecord::user_input(text, now_ms(), self.seq).with_stamp(&self.stamp);
        self.write_record(&rec);
    }

    /// 序列化一条记录写一行,全程 log-ignore(观测落盘失败不该让一轮挂掉)。
    fn write_record(&mut self, rec: &EventRecord) {
        match serde_json::to_string(rec) {
            Ok(line) => {
                if let Err(e) = writeln!(self.file, "{line}") {
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
        Ok(mut sink) => {
            sink.seq += 1;
            let rec = EventRecord::model_catalog(text, now_ms(), sink.seq).with_stamp(&sink.stamp);
            sink.write_record(&rec);
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
