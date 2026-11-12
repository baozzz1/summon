//! `logs.tail`:日志页。从文件末尾往前读,按条目(见 `summon_core::log_view`)过滤、脱敏后给最新的一页;
//! 翻页游标是「文件的 inode : 读到的字节位置」,日志在两次翻页之间轮转了也认得出那一份去了 `.1`。
//!
//! 只给本机 owner 与能改整机设置的人(owner 2026-10-21 定):日志里有各个群、各个 agent 的运行细节。

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use summon_core::dashboard::authz::Target;
use summon_core::log_view::{self, LogEntry, LogFile, LogFilter};

use super::sessions::iso_ms;
use super::types::{LogLine, LogsPage};
use super::{invalid, params, reply};
use crate::runtime::dashboard::server::{ApiCtx, ApiError};

/// 一页最多多少条。
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 500;
/// 每次往前读多少字节。
const CHUNK: u64 = 256 * 1024;
/// 一次请求最多往前翻多少字节:筛得很窄时不让一次请求把几十 MB 的日志全读一遍,剩下的下一页接着翻。
const SCAN_BUDGET: u64 = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct TailParams {
    file: LogFile,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    errors: bool,
}

/// 一个日志文件的一份:当前那份在前,轮转出去的 `.1` 在后。
struct Segment {
    path: PathBuf,
    ino: u64,
    len: u64,
}

fn segments(run_dir: &Path, file: LogFile) -> Vec<Segment> {
    let name = file.file_name();
    [run_dir.join(name), run_dir.join(format!("{name}.1"))]
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok()?;
            Some(Segment {
                ino: meta.ino(),
                len: meta.len(),
                path,
            })
        })
        .collect()
}

fn cursor(seg: &Segment, offset: u64) -> String {
    format!("{}:{offset}", seg.ino)
}

/// 游标指向哪一份的哪个位置。
fn resolve(segs: &[Segment], before: &str) -> Result<(usize, u64), ApiError> {
    let (ino, offset) = before
        .split_once(':')
        .and_then(|(i, o)| Some((i.parse::<u64>().ok()?, o.parse::<u64>().ok()?)))
        .ok_or_else(|| invalid(summon_i18n::text!("rface9086b8")))?;
    let index = segs
        .iter()
        .position(|s| s.ino == ino)
        .ok_or_else(|| invalid(summon_i18n::text!("r816da6ef32")))?;
    Ok((index, offset.min(segs[index].len)))
}

fn time_param(value: Option<&str>, name: &str) -> Result<Option<i64>, ApiError> {
    value
        .filter(|v| !v.is_empty())
        .map(|v| {
            chrono::DateTime::parse_from_rfc3339(v)
                .map(|at| at.timestamp_millis())
                .map_err(|_| invalid(summon_i18n::format!("re7e6efe8ff")))
        })
        .transpose()
}

/// 读 `[start, end)`,对齐到整行:开头那半行属于更早的部分,不要。返回对齐后的起点与字节。
fn read_lines(path: &Path, start: u64, end: u64) -> std::io::Result<(u64, Vec<u8>)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(end - start).read_to_end(&mut bytes)?;
    let skip = if start == 0 {
        0
    } else {
        bytes
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| i + 1)
    };
    bytes.drain(..skip);
    Ok((start + skip as u64, bytes))
}

pub(crate) fn tail(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    env.require(ctx, &Target::Machine)?;
    let req: TailParams = params(p)?;
    let filter = LogFilter {
        query: req.query,
        agent: req.agent,
        since_ms: time_param(req.since.as_deref(), "since")?,
        until_ms: time_param(req.until.as_deref(), "until")?,
        errors_only: req.errors,
    };
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let segs = segments(&env.run_dir(), req.file);
    let (mut index, mut end) = match req.before.as_deref() {
        Some(before) => resolve(&segs, before)?,
        None => (0, segs.first().map_or(0, |s| s.len)),
    };
    let mut entries = Vec::new();
    let mut scanned = 0;
    let mut chunk = CHUNK;
    let next = loop {
        let Some(seg) = segs.get(index) else {
            break None;
        };
        if end == 0 {
            index += 1;
            end = segs.get(index).map_or(0, |s| s.len);
            continue;
        }
        if scanned >= SCAN_BUDGET {
            break Some(cursor(seg, end));
        }
        let start = end.saturating_sub(chunk);
        let (begin, text) = read_lines(&seg.path, start, end).map_err(|e| {
            super::internal(summon_i18n::format!("raf458f69ba", seg.path.display()))
        })?;
        let (lead, found) = log_view::split(req.file, &text);
        if found.is_empty() && start > 0 {
            // 一条比这一段还长:往前多读一些再切,这一次请求读的总量仍不超过预算。预算用完还没读到这一条
            // 的开头,就跳过读过的这段,下一页从它前面接着找(那一条只剩开头的部分)。
            if scanned + 2 * chunk > SCAN_BUDGET {
                break Some(cursor(seg, start));
            }
            chunk *= 2;
            continue;
        }
        scanned += end - begin;
        chunk = CHUNK;
        let mut stop = None;
        for entry in found.iter().rev() {
            if filter.before_range(entry) {
                stop = Some(None);
                break;
            }
            // 先脱敏再筛:关键字探不出被换掉的那部分原文。
            let shown = LogEntry {
                text: log_view::redact(&entry.text),
                ..entry.clone()
            };
            if !filter.keeps(&shown) {
                continue;
            }
            entries.push(LogLine {
                at: shown.at_ms.and_then(iso_ms),
                error: log_view::is_error(&shown.text),
                text: log_view::clip(&shown.text),
            });
            if entries.len() == limit {
                let at = begin + entry.offset as u64;
                // 停在最早那份文件的最开头:往前没有了,不给一个只翻得出空页的位置。
                stop = Some((at > 0 || index + 1 < segs.len()).then(|| cursor(seg, at)));
                break;
            }
        }
        if let Some(next) = stop {
            break next;
        }
        // 文件开头之前没有更早的行了;开头那几行是 `.1` 里最后一条的续行,不单独成条。
        end = if start == 0 { 0 } else { begin + lead as u64 };
    };
    reply(&LogsPage {
        file: req.file,
        entries,
        before: next,
    })
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::runtime::dashboard::server::ErrorCode;
    use crate::runtime::dashboard::sessions::{ChatKind, Issuer};
    use serde_json::json;

    fn log(kit: &Kit, name: &str, text: &str) {
        let run = kit.home().join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join(name), text).unwrap();
    }

    fn lines(out: &Value) -> Vec<String> {
        out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                e["text"]
                    .as_str()
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn newest_entries_come_first_and_pages_cross_into_the_rotated_file() {
        let kit = Kit::new();
        log(
            &kit,
            "daemon.log.1",
            "[2026-10-20 09:00:00] [summon] 一\n[2026-10-20 09:00:01] [summon] 二\n",
        );
        log(
            &kit,
            "daemon.log",
            "[2026-10-21 09:00:00] [summon] [starlight] 三\nls docs\n[2026-10-21 09:00:01] [summon] 四\n",
        );
        let owner = kit.owner();
        let first = kit
            .call(&owner, tail, json!({"file": "daemon", "limit": 3}))
            .unwrap();
        assert_eq!(
            lines(&first),
            [
                "[2026-10-21 09:00:01] [summon] 四",
                "[2026-10-21 09:00:00] [summon] [starlight] 三",
                "[2026-10-20 09:00:01] [summon] 二",
            ]
        );
        assert!(first["entries"][1]["text"]
            .as_str()
            .unwrap()
            .ends_with("ls docs"));
        assert!(first["entries"][0]["at"].is_string());
        let before = first["before"].as_str().unwrap().to_string();
        let rest = kit
            .call(&owner, tail, json!({"file": "daemon", "before": before}))
            .unwrap();
        assert_eq!(lines(&rest), ["[2026-10-20 09:00:00] [summon] 一"]);
        assert!(rest["before"].is_null());
    }

    #[test]
    fn filters_and_redaction_apply_before_the_page_is_cut() {
        let kit = Kit::new();
        log(
            &kit,
            "daemon.log",
            "[2026-10-21 08:00:00] [summon] [orbit] 早上的\n\
             [2026-10-21 09:00:00] [summon] [starlight] 投递失败 TOKEN=abc123\n\
             [2026-10-21 10:00:00] [summon] [starlight] 一切正常\n",
        );
        let owner = kit.owner();
        let errors = kit
            .call(&owner, tail, json!({"file": "daemon", "errors": true}))
            .unwrap();
        assert_eq!(errors["entries"].as_array().unwrap().len(), 1);
        let text = errors["entries"][0]["text"].as_str().unwrap();
        assert!(text.contains("TOKEN=[REDACTED]"), "{text}");
        assert_eq!(errors["entries"][0]["error"], true);
        let orbit = kit
            .call(&owner, tail, json!({"file": "daemon", "agent": "orbit"}))
            .unwrap();
        assert_eq!(
            lines(&orbit),
            ["[2026-10-21 08:00:00] [summon] [orbit] 早上的"]
        );
        let local = |h: u32, m: u32| {
            chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 21, h, m, 0)
                .unwrap()
                .to_rfc3339()
        };
        let window = kit
            .call(
                &owner,
                tail,
                json!({"file": "daemon", "since": local(8, 30), "until": local(9, 30)}),
            )
            .unwrap();
        assert_eq!(
            lines(&window),
            ["[2026-10-21 09:00:00] [summon] [starlight] 投递失败 TOKEN=[REDACTED]"]
        );
        let bad = kit
            .call(&owner, tail, json!({"file": "daemon", "since": "昨天"}))
            .unwrap_err();
        assert_eq!(bad.code, ErrorCode::Invalid);
    }

    #[test]
    fn only_people_who_can_change_machine_settings_read_logs() {
        let kit = Kit::new();
        let agent = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&agent, "group", GROUP, "示例群");
        log(&kit, "daemon.log", "[2026-10-21 08:00:00] [summon] 一\n");
        let stranger = kit.feishu(Issuer {
            open_id: "ou_00000000000000000000000000000004".into(),
            union_id: PEER_UNION.into(),
            ..issuer("starlight", GROUP, ChatKind::Group)
        });
        let err = kit
            .call(&stranger, tail, json!({"file": "daemon"}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        let empty = kit
            .call(&kit.owner(), tail, json!({"file": "sidecar"}))
            .unwrap();
        assert_eq!(empty["entries"], json!([]));
        assert!(empty["before"].is_null());
    }

    /// 条目跨过一次读的块边界(含很长的多行条目):小页一路翻到底,与整份文件正着切出来的逐条一致,
    /// 不漏、不重;翻到最早那一条就不再给翻页位置。
    #[test]
    fn paging_across_read_chunks_neither_drops_nor_repeats_entries() {
        let kit = Kit::new();
        let mut text = String::new();
        for i in 0..2_000u32 {
            text.push_str(&format!(
                "[2026-10-21 08:{:02}:{:02}] [summon] 第 {i} 条\n",
                (i / 60) % 60,
                i % 60
            ));
            // 每十条一条带 40 行续行的:一块读不满 500 条,一页要跨好几块读。
            if i % 10 == 0 {
                for j in 0..40 {
                    text.push_str(&format!("  续行 {j} {}\n", "x".repeat(200)));
                }
            }
        }
        assert!(text.len() as u64 > 2 * CHUNK);
        log(&kit, "daemon.log", &text);
        let (_, all) = log_view::split(LogFile::Daemon, text.as_bytes());
        let expected: Vec<String> = all.iter().rev().map(|e| e.text.clone()).collect();
        for limit in [37, 500] {
            let mut seen: Vec<String> = Vec::new();
            let mut before: Option<String> = None;
            loop {
                let mut body = json!({"file": "daemon", "limit": limit});
                if let Some(b) = &before {
                    body["before"] = json!(b);
                }
                let page = kit.call(&kit.owner(), tail, body).unwrap();
                seen.extend(
                    page["entries"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|e| e["text"].as_str().unwrap().to_string()),
                );
                match page["before"].as_str() {
                    Some(b) => before = Some(b.to_string()),
                    None => break,
                }
            }
            assert_eq!(seen.len(), expected.len(), "limit {limit}");
            assert_eq!(seen, expected, "limit {limit}");
        }
    }

    #[test]
    fn a_cursor_from_a_file_that_rotated_away_is_rejected() {
        let kit = Kit::new();
        log(&kit, "daemon.log", "[2026-10-21 08:00:00] [summon] 一\n");
        let err = kit
            .call(
                &kit.owner(),
                tail,
                json!({"file": "daemon", "before": "1:10"}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
    }
}
