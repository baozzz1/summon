//! 控制台日志页怎么读日志:daemon 与 sidecar 的日志按行首时间戳切成条目(一条可以跨多行,比如带出
//! 来的命令输出),控制台审计一行一条;按关键字、agent、时间与「只看出错」过滤;显示前把密钥形态的串
//! 换掉。纯函数,读文件、翻页在 summon 侧。

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};

/// 日志页能看的文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFile {
    Daemon,
    Sidecar,
    Audit,
}

impl LogFile {
    /// `~/.summon/run/` 下的文件名;轮转出去的那份在后面加 `.1`。
    pub fn file_name(self) -> &'static str {
        match self {
            LogFile::Daemon => "daemon.log",
            LogFile::Sidecar => "sidecar.log",
            LogFile::Audit => "dashboard-audit.ndjson",
        }
    }

    /// 这一行是不是一条新条目的开头;是的话带上它的时刻(毫秒,认不出时刻时为 `None`)。
    pub fn entry_start(self, line: &str) -> Option<Option<i64>> {
        let local = |text: &str, format: &str| {
            NaiveDateTime::parse_from_str(text, format)
                .ok()
                .and_then(|at| Local.from_local_datetime(&at).earliest())
                .map(|at| at.timestamp_millis())
        };
        match self {
            // `[2026-10-21 15:35:20] [summon] …`
            LogFile::Daemon => {
                let stamp = line.strip_prefix('[')?.get(..19)?;
                local(stamp, "%Y-%m-%d %H:%M:%S").map(Some)
            }
            // Go 的 log 包,两种写法都有:`2026/10/21 16:55:16 [Info] …` 与
            // `[Error] 2026/07/31 05:35:25.229368 …`(小数秒不看)。
            LogFile::Sidecar => {
                let rest = match line.strip_prefix('[') {
                    Some(tagged) => tagged.split_once("] ")?.1,
                    None => line,
                };
                local(rest.get(..19)?, "%Y/%m/%d %H:%M:%S").map(Some)
            }
            LogFile::Audit => {
                if line.trim().is_empty() {
                    return None;
                }
                let ts = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("ts")?.as_str().map(str::to_string));
                Some(
                    ts.and_then(|ts| DateTime::parse_from_rfc3339(&ts).ok())
                        .map(|at| at.timestamp_millis()),
                )
            }
        }
    }
}

/// 单条最多给这么多字节:一条带出几十 MB 命令输出的日志不能把一页撑爆。
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;

/// 一条日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// 这一条在所给字节里从第几个字节开始。
    pub offset: usize,
    pub at_ms: Option<i64>,
    pub text: String,
}

/// 把一段按行对齐的字节切成条目,按出现顺序。第一条开头之前的那些行属于更早的一条(它在这段之前
/// 开头),不算进来,只交回它们占的字节数:往前翻页的人下一次读到那里为止,那一条就完整了。
/// 偏移按原始字节算;不是 UTF-8 的字节逐行换成 �,不影响位置。
pub fn split(file: LogFile, bytes: &[u8]) -> (usize, Vec<LogEntry>) {
    let mut lead = bytes.len();
    let mut out: Vec<LogEntry> = Vec::new();
    let mut offset = 0;
    for raw in bytes.split_inclusive(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(raw);
        match file.entry_start(line.trim_end_matches(['\n', '\r'])) {
            Some(at_ms) => {
                if out.is_empty() {
                    lead = offset;
                }
                out.push(LogEntry {
                    offset,
                    at_ms,
                    text: line.into_owned(),
                });
            }
            None => {
                if let Some(last) = out.last_mut() {
                    last.text.push_str(&line);
                }
            }
        }
        offset += raw.len();
    }
    for entry in &mut out {
        let trimmed = entry.text.trim_end().len();
        entry.text.truncate(trimmed);
    }
    (lead, out)
}

/// 「出错」的条目:含 error / panic / fail 或「失败」「错误」。
pub fn is_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "error",
        "panic",
        "fail",
        summon_i18n::text!("r28384d7afd"),
        summon_i18n::text!("r0bc1fb72ae"),
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// 日志页的筛选条件。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// 关键字,不分大小写。
    pub query: Option<String>,
    /// daemon 日志里写成 `[agent]`,审计里是 JSON 里的 `"agent"`。
    pub agent: Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub errors_only: bool,
}

impl LogFilter {
    pub fn keeps(&self, entry: &LogEntry) -> bool {
        if let (Some(until), Some(at)) = (self.until_ms, entry.at_ms) {
            if at > until {
                return false;
            }
        }
        if let (Some(since), Some(at)) = (self.since_ms, entry.at_ms) {
            if at < since {
                return false;
            }
        }
        if self.errors_only && !is_error(&entry.text) {
            return false;
        }
        if let Some(agent) = self.agent.as_deref().filter(|a| !a.is_empty()) {
            if !entry.text.contains(&format!("[{agent}]"))
                && !entry.text.contains(&format!("\"{agent}\""))
            {
                return false;
            }
        }
        match self
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        {
            Some(query) => entry.text.to_lowercase().contains(&query.to_lowercase()),
            None => true,
        }
    }

    /// 这一条已经早于时间范围:日志按时间顺序写,再往前翻也不会有符合的了。
    pub fn before_range(&self, entry: &LogEntry) -> bool {
        matches!((self.since_ms, entry.at_ms), (Some(since), Some(at)) if at < since)
    }
}

/// 给人看之前:密钥形态的串换成 `[REDACTED]`(与过程卡片里命令明细同一套规则)。
pub fn redact(text: &str) -> String {
    crate::messaging::segment::redact_secrets(text)
}

/// 太长的一条只留开头 [`MAX_ENTRY_BYTES`],写明还有多少没显示。
pub fn clip(text: &str) -> String {
    if text.len() <= MAX_ENTRY_BYTES {
        return text.to_string();
    }
    let mut cut = MAX_ENTRY_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    summon_i18n::format!("rd798746802", &text[..cut], text.len() - cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_entries_keep_their_continuation_lines() {
        let text = "  tail of an older entry\n\
                    [2026-10-21 15:35:20] [summon] [starlight] 跑了一条命令\n\
                    ls docs\n\
                    cat notes.md\n\
                    [2026-10-21 15:35:21] [summon] 下一条\n";
        let (lead, entries) = split(LogFile::Daemon, text.as_bytes());
        assert_eq!(lead, "  tail of an older entry\n".len());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].offset, lead);
        assert!(entries[0].text.ends_with("cat notes.md"));
        assert!(entries[0].at_ms.is_some());
        assert_eq!(entries[1].text, "[2026-10-21 15:35:21] [summon] 下一条");
        let (none, nothing) = split(LogFile::Daemon, b"no stamp here\n");
        assert_eq!(none, "no stamp here\n".len());
        assert!(nothing.is_empty());
    }

    #[test]
    fn sidecar_lines_start_entries_whichever_way_the_level_is_written() {
        let text = "2026/10/21 16:55:16 [Info] [event-dispatch is ready]\n\
                    [Error] 2026/07/31 05:35:25.229368 handle message failed, message_type: event\n\
                    [Error] 2026/07/31 05:35:40.341152 handle message failed\n\
                    \tgoroutine 1 [running]:\n";
        let (lead, entries) = split(LogFile::Sidecar, text.as_bytes());
        assert_eq!(lead, 0);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.at_ms.is_some()));
        assert!(entries[1].at_ms < entries[0].at_ms);
        assert!(entries[2].text.ends_with("goroutine 1 [running]:"));
    }

    #[test]
    fn offsets_count_raw_bytes_even_when_a_line_is_not_utf8() {
        let mut bytes = b"[2026-10-21 08:00:00] [summon] \xff\xfe\xfd bad\n".to_vec();
        let second = bytes.len();
        bytes.extend_from_slice(b"[2026-10-21 08:00:01] [summon] next\n");
        let (_, entries) = split(LogFile::Daemon, &bytes);
        assert_eq!(entries[1].offset, second);
        assert!(entries[0].text.contains('\u{FFFD}'));
    }

    #[test]
    fn long_entries_are_clipped_on_a_character_boundary() {
        assert_eq!(clip("短"), "短");
        let long = "日".repeat(MAX_ENTRY_BYTES);
        let shown = clip(&long);
        assert!(shown.len() < MAX_ENTRY_BYTES + 100);
        assert!(shown.ends_with("字节未显示）"));
    }

    #[test]
    fn sidecar_and_audit_lines_are_recognised() {
        assert!(LogFile::Sidecar
            .entry_start("2026/10/21 16:55:16 [Info] [event-dispatch is ready]")
            .is_some_and(|at| at.is_some()));
        assert_eq!(
            LogFile::Sidecar.entry_start("\tgoroutine 1 [running]:"),
            None
        );
        let at = LogFile::Audit
            .entry_start(r#"{"ts":"2026-10-21T12:59:47+08:00","file":"global","result":"ok"}"#);
        assert_eq!(
            at,
            Some(Some(
                DateTime::parse_from_rfc3339("2026-10-21T12:59:47+08:00")
                    .unwrap()
                    .timestamp_millis()
            ))
        );
        assert_eq!(LogFile::Audit.entry_start("not json"), Some(None));
        assert_eq!(LogFile::Audit.entry_start("  "), None);
    }

    #[test]
    fn filters_combine_keyword_agent_time_and_errors() {
        let entry = |text: &str, at: i64| LogEntry {
            offset: 0,
            at_ms: Some(at),
            text: text.into(),
        };
        let failed = entry("[t] [summon] [starlight] 投递失败:timeout", 100);
        let quiet = entry("[t] [summon] [orbit] 会话镜像已启用", 200);
        let all = LogFilter::default();
        assert!(all.keeps(&failed) && all.keeps(&quiet));
        let errors = LogFilter {
            errors_only: true,
            ..Default::default()
        };
        assert!(errors.keeps(&failed) && !errors.keeps(&quiet));
        let agent = LogFilter {
            agent: Some("orbit".into()),
            ..Default::default()
        };
        assert!(!agent.keeps(&failed) && agent.keeps(&quiet));
        let query = LogFilter {
            query: Some("TIMEOUT".into()),
            ..Default::default()
        };
        assert!(query.keeps(&failed) && !query.keeps(&quiet));
        let window = LogFilter {
            since_ms: Some(150),
            until_ms: Some(250),
            ..Default::default()
        };
        assert!(!window.keeps(&failed) && window.keeps(&quiet));
        assert!(window.before_range(&failed));
        assert!(!window.before_range(&quiet));
    }

    #[test]
    fn secrets_are_masked_before_display() {
        let shown = redact("[t] env OPENAI_API_KEY=sk-0123456789abcdef ok");
        assert!(shown.contains("OPENAI_API_KEY=[REDACTED]"), "{shown}");
        assert!(!shown.contains("sk-0123"));
    }
}
