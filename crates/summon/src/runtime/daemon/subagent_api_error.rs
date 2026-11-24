//! 子 agent 的 claude API 错误上报:detached 线程 tail 当前会话 `subagents/` 下各子 agent 的转录,
//! 读到 API 错误记录(见 [`ClaudeApiError::from_subagent_line`])就往本 chat 发一张提示卡。
//!
//! 与主路径、会话镜像都不相交:那两条只读主转录,而且主转录口径不认子 agent 的记录 —— 按子 agent
//! 的报错收主轮会截断仍在运行的主 agent。这里只发提示,不碰主轮的任何状态,所以主轮进行中、
//! 已经收尾、或根本不是飞书发起的轮(终端里直接敲的),子 agent 报错都照样发。
//!
//! 只报线程起来之后才写出的记录:第一次扫描时(以及换到轮换后的 sid 时)已在盘上的子 agent
//! 转录从文件末尾开始读,之后新出现的文件从头读。等 sid 就位(退避最长 5 秒一拍)与发现 sid
//! 轮换(5 秒一查)期间新建并已写下错误的转录也算在盘上,其中的错误不报:宁漏勿重。每个文件的读取位置只前进,同一条记录只会读到一次;发送带
//! 由记录身份派生的幂等键,重试不会在群里出现第二张。

use super::*;
use summon_core::claude_api_error::ClaudeApiError;

/// 列目录、读新字节的节拍。错误提示晚一秒到无妨,不必跟主路径一样每拍都扫。
const SCAN_EVERY: Duration = Duration::from_secs(1);
/// 重读 `.session` 跟随 sid 轮换的节拍,与 subagent watcher 相同。
const SID_CHECK_EVERY: Duration = Duration::from_secs(5);
/// 单个子 agent 转录里没有换行的残行上限:超过就丢掉,不让坏文件把内存撑爆。
const PARTIAL_LINE_LIMIT: usize = 4 * 1024 * 1024;

/// 上报线程的 RAII guard:Drop 时停掉并 join。它会往 `threads/<tag>` 写出站归档,删目录前必须先停。
pub(crate) struct SubagentApiErrorGuard {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for SubagentApiErrorGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 起上报线程。只有 claude 的 Transcript 捕获有子 agent 转录可读,其余返回 `None`。
pub(crate) fn spawn_subagent_api_error_reporter(
    agent: &Agent,
    sess: &Session,
    fc: Arc<FeishuClient>,
    frozen_epoch: FeishuEpochIdentity,
    chat_id: String,
    chat_type: String,
) -> Option<SubagentApiErrorGuard> {
    if agent.capture_mode() != CaptureMode::Transcript {
        return None;
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (agent, tmux_name, poll, thread_stop) = (
        agent.clone(),
        sess.tmux.session.clone(),
        sess.poll,
        stop.clone(),
    );
    let handle = thread::spawn(move || {
        run_reporter(
            agent,
            tmux_name,
            poll,
            fc,
            frozen_epoch,
            chat_id,
            chat_type,
            thread_stop,
        )
    });
    Some(SubagentApiErrorGuard {
        stop,
        handle: Some(handle),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_reporter(
    agent: Agent,
    tmux_name: String,
    poll: Duration,
    fc: Arc<FeishuClient>,
    frozen_epoch: FeishuEpochIdentity,
    chat_id: String,
    chat_type: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let sess = Session::with_name(&agent, tmux_name.clone(), poll.as_millis() as u64, 800)
        .with_chat_id(&chat_id);
    // 等会话 sid 与转录路径就位。拿不到就一直低频重试:半途放弃等于这个 chat 此后再也不报。
    let mut wait = Duration::from_millis(500);
    let target = loop {
        if stop.load(Relaxed) {
            return;
        }
        if let Some(target) = sess
            .native_session_id()
            .and_then(|sid| resolve_target(&agent, &tmux_name, Some(&chat_id), &sid))
        {
            break target;
        }
        thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_secs(5));
    };
    log(&summon_i18n::format!(
        "re3bf2f70be",
        agent.name,
        target.subagents_dir.display()
    ));
    let mut sid = target.sid;
    let mut scanner = SubagentErrorScanner::new(target.subagents_dir);
    let epoch_fingerprint = frozen_epoch.fingerprint();
    let tick = poll.clamp(Duration::from_millis(50), SCAN_EVERY);
    let mut last_scan: Option<Instant> = None;
    let mut last_sid_check = Instant::now();
    while !stop.load(Relaxed) {
        if last_sid_check.elapsed() >= SID_CHECK_EVERY {
            last_sid_check = Instant::now();
            // sid 轮换(/clear、/bridge 召回):换到新会话的目录,那里已有的转录都是历史。
            if let Some(current) = sess.native_session_id().filter(|current| *current != sid) {
                if let Some(target) = resolve_target(&agent, &tmux_name, Some(&chat_id), &current) {
                    sid = target.sid;
                    scanner = SubagentErrorScanner::new(target.subagents_dir);
                    last_scan = None;
                }
            }
        }
        if last_scan.is_none_or(|at| at.elapsed() >= SCAN_EVERY) {
            last_scan = Some(Instant::now());
            for report in scanner.poll() {
                let agent_id = report.agent_id.clone();
                let sent = deliver_subagent_api_error(
                    &report,
                    || ensure_mirror_epoch_current(&agent, &frozen_epoch),
                    |card| {
                        let uuid =
                            subagent_api_error_uuid(&epoch_fingerprint, &chat_id, &sid, &report);
                        crate::frontend::outbound::send_card_idempotent_archived_id(
                            &fc,
                            &agent.state_dir(),
                            &chat_type,
                            &chat_id,
                            "subagent_api_error_card",
                            card,
                            &uuid,
                        )
                        .map(|_| ())
                    },
                );
                match sent {
                    Ok(()) => log(&summon_i18n::format!("r008e45584e", agent.name)),
                    // 一张提示卡发不出去不值得停掉整个上报:身份失效时 chat 线程自己会收场。
                    Err(error) => log(&summon_i18n::format!("r61f2061d25", agent.name)),
                }
            }
        }
        thread::sleep(tick);
    }
}

/// 子 agent 转录里读到的一条 API 错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubagentApiErrorReport {
    /// 子 agent 的 agentId(转录文件名 `agent-<id>.jsonl` 里的那段)。
    pub(super) agent_id: String,
    /// 给人看的子 agent 名:meta 里的 description,没有时退回 agentType,再退回 agentId。
    pub(super) label: String,
    pub(super) error: ClaudeApiError,
    /// 记录所在行行尾之后的字节偏移。记录没有 uuid 时与 agentId 一起充当它的身份。
    pub(super) end_offset: u64,
}

/// 发一条子 agent 错误:紧挨着发送前校验身份(零网络),校验不过不发。
pub(super) fn deliver_subagent_api_error(
    report: &SubagentApiErrorReport,
    preflight: impl FnOnce() -> anyhow::Result<()>,
    send: impl FnOnce(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    preflight()?;
    let card = summon_core::feishu::card_subagent_api_error(
        &report.label,
        report.error.category.as_deref(),
        &report.error.reason(),
    );
    send(&card)
}

/// 发送幂等键:同一会话里同一条记录(uuid,缺 uuid 时按所在文件与偏移)恒得同一个键。
fn subagent_api_error_uuid(
    epoch_fingerprint: &str,
    chat_id: &str,
    sid: &str,
    report: &SubagentApiErrorReport,
) -> String {
    let identity = match &report.error.uuid {
        Some(uuid) => format!("{}\n{uuid}", report.agent_id),
        None => format!("{}\n@{}", report.agent_id, report.end_offset),
    };
    mirror_message_uuid(
        epoch_fingerprint,
        chat_id,
        sid,
        0,
        identity.as_bytes(),
        "subagent_api_error",
        0,
    )
}

/// 一个会话 `subagents/` 目录的增量读取器:顶层的 `agent-<id>.jsonl` 与 workflow 扇出的
/// `workflows/<run>/agent-<id>.jsonl`。第一次 [`Self::poll`] 只记下已有文件的末尾、不报。
pub(super) struct SubagentErrorScanner {
    dir: PathBuf,
    files: std::collections::HashMap<PathBuf, SubagentFileCursor>,
    primed: bool,
}

#[derive(Default)]
struct SubagentFileCursor {
    offset: u64,
    pending: Vec<u8>,
}

impl SubagentErrorScanner {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            files: std::collections::HashMap::new(),
            primed: false,
        }
    }

    /// 读一拍:返回上一拍之后新写出的 API 错误记录,按文件内顺序。
    pub(super) fn poll(&mut self) -> Vec<SubagentApiErrorReport> {
        let history = !self.primed;
        self.primed = true;
        let mut reports = Vec::new();
        for (path, agent_id, len) in self.transcripts() {
            let cursor = self.files.entry(path.clone()).or_insert_with(|| {
                // 起跑时已在盘上的是历史,从末尾读;之后才出现的文件从头读。
                SubagentFileCursor {
                    offset: if history { len } else { 0 },
                    pending: Vec::new(),
                }
            });
            if len < cursor.offset {
                // 文件被截短或换掉:不知道哪些读过,从新末尾续读,宁漏勿重。
                *cursor = SubagentFileCursor {
                    offset: len,
                    pending: Vec::new(),
                };
                continue;
            }
            if len == cursor.offset {
                continue;
            }
            let Ok(chunk) = read_from(&path, cursor.offset) else {
                continue;
            };
            let chunk_start = cursor.offset - cursor.pending.len() as u64;
            cursor.offset += chunk.len() as u64;
            cursor.pending.extend_from_slice(&chunk);
            let Some(last_newline) = cursor.pending.iter().rposition(|byte| *byte == b'\n') else {
                if cursor.pending.len() > PARTIAL_LINE_LIMIT {
                    cursor.pending.clear();
                }
                continue;
            };
            let complete: Vec<u8> = cursor.pending.drain(..=last_newline).collect();
            let mut line_start = 0usize;
            for (index, byte) in complete.iter().enumerate() {
                if *byte != b'\n' {
                    continue;
                }
                let line = String::from_utf8_lossy(&complete[line_start..index]);
                line_start = index + 1;
                let Some(error) = ClaudeApiError::from_subagent_line(&line) else {
                    continue;
                };
                reports.push(SubagentApiErrorReport {
                    label: subagent_label(&path, &agent_id),
                    agent_id: agent_id.clone(),
                    error,
                    end_offset: chunk_start + line_start as u64,
                });
            }
        }
        reports
    }

    /// 目录里的子 agent 转录:`(路径, agentId, 当前长度)`。
    fn transcripts(&self) -> Vec<(PathBuf, String, u64)> {
        let mut out = Vec::new();
        collect_transcripts(&self.dir, &mut out);
        if let Ok(runs) = std::fs::read_dir(self.dir.join("workflows")) {
            for run in runs.flatten() {
                if run.file_type().is_ok_and(|kind| kind.is_dir()) {
                    collect_transcripts(&run.path(), &mut out);
                }
            }
        }
        out
    }
}

fn collect_transcripts(dir: &std::path::Path, out: &mut Vec<(PathBuf, String, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(agent_id) = name
            .to_str()
            .and_then(|name| name.strip_prefix("agent-"))
            .and_then(|rest| rest.strip_suffix(".jsonl"))
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() {
            out.push((entry.path(), agent_id.to_string(), meta.len()));
        }
    }
}

fn read_from(path: &std::path::Path, offset: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _};
    let mut file = std::fs::File::open(path)?;
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// 子 agent 的展示名:同目录 `agent-<id>.meta.json` 的 description,没有时 agentType,再没有用 agentId。
fn subagent_label(transcript: &std::path::Path, agent_id: &str) -> String {
    let meta = std::fs::read(transcript.with_file_name(format!("agent-{agent_id}.meta.json")))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let field = |key: &str| {
        meta.as_ref()
            .and_then(|meta| meta.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    field("description")
        .or_else(|| field("agentType"))
        .unwrap_or_else(|| agent_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUB_ERROR: &str = r#"{"parentUuid":"p-1","isSidechain":true,"agentId":"a1","type":"assistant","uuid":"sub-err-1","error":"server_error","isApiErrorMessage":true,"message":{"model":"<synthetic>","role":"assistant","stop_reason":"stop_sequence","error":null,"content":[{"type":"text","text":"API Error: The response stopped arriving. The response above may be incomplete."}]}}"#;
    const SUB_REPLY: &str = r#"{"isSidechain":true,"agentId":"a1","type":"assistant","uuid":"sub-ok","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"结论:没问题"}]}}"#;
    const SUB_TOOL: &str = r#"{"isSidechain":true,"agentId":"a1","type":"assistant","uuid":"sub-tool","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#;

    fn append(path: &std::path::Path, text: &str) {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    fn line(record: &str) -> String {
        format!("{record}\n")
    }

    fn uuids(reports: &[SubagentApiErrorReport]) -> Vec<Option<String>> {
        reports.iter().map(|r| r.error.uuid.clone()).collect()
    }

    /// 起跑前已在盘上的错误是历史,不报;之后写出的报一次,再读不重报。
    #[test]
    fn reports_new_errors_once_and_never_history() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent-a1.jsonl");
        append(&file, &line(&SUB_ERROR.replace("sub-err-1", "old-err")));
        let mut scanner = SubagentErrorScanner::new(dir.path().to_path_buf());
        assert!(scanner.poll().is_empty(), "历史错误不能补发进群");

        append(&file, &line(SUB_TOOL));
        append(&file, &line(SUB_ERROR));
        let reports = scanner.poll();
        assert_eq!(uuids(&reports), [Some("sub-err-1".to_string())]);
        assert_eq!(reports[0].agent_id, "a1");
        assert_eq!(
            reports[0].end_offset,
            std::fs::metadata(&file).unwrap().len()
        );
        assert!(scanner.poll().is_empty(), "同一条记录只报一次");
    }

    /// 起跑之后才出现的子 agent 转录从头读;顶层与 workflow 扇出的都看。
    #[test]
    fn reads_transcripts_created_after_start_from_the_beginning() {
        let dir = tempfile::tempdir().unwrap();
        let mut scanner = SubagentErrorScanner::new(dir.path().to_path_buf());
        assert!(scanner.poll().is_empty());
        let top = dir.path().join("agent-a1.jsonl");
        append(&top, &line(SUB_REPLY));
        append(&top, &line(SUB_ERROR));
        let run = dir.path().join("workflows").join("wf-1");
        std::fs::create_dir_all(&run).unwrap();
        append(
            &run.join("agent-w1.jsonl"),
            &line(&SUB_ERROR.replace("sub-err-1", "wf-err")),
        );
        let mut got = uuids(&scanner.poll());
        got.sort();
        assert_eq!(
            got,
            [Some("sub-err-1".to_string()), Some("wf-err".to_string())]
        );
    }

    /// 半行先不解析,写完整之后再报;普通记录不报。
    #[test]
    fn waits_for_the_complete_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent-a1.jsonl");
        let mut scanner = SubagentErrorScanner::new(dir.path().to_path_buf());
        scanner.poll();
        let (head, tail) = SUB_ERROR.split_at(40);
        append(&file, &line(SUB_REPLY));
        append(&file, head);
        assert!(scanner.poll().is_empty());
        append(&file, &line(tail));
        assert_eq!(uuids(&scanner.poll()), [Some("sub-err-1".to_string())]);
    }

    /// 文件被截短:从新末尾续读,不把剩下的内容当新记录重报。
    #[test]
    fn a_truncated_transcript_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent-a1.jsonl");
        let mut scanner = SubagentErrorScanner::new(dir.path().to_path_buf());
        scanner.poll();
        append(&file, &line(SUB_REPLY));
        append(&file, &line(SUB_ERROR));
        assert_eq!(scanner.poll().len(), 1);
        std::fs::write(&file, line(SUB_ERROR)).unwrap();
        assert!(scanner.poll().is_empty());
        assert!(
            scanner.poll().is_empty(),
            "截短前读过的内容下一拍也不能重报"
        );
        append(&file, &line(&SUB_ERROR.replace("sub-err-1", "sub-err-2")));
        assert_eq!(uuids(&scanner.poll()), [Some("sub-err-2".to_string())]);
    }

    #[test]
    fn label_prefers_description_then_agent_type() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent-a1.jsonl");
        assert_eq!(subagent_label(&file, "a1"), "a1");
        std::fs::write(
            dir.path().join("agent-a1.meta.json"),
            r#"{"agentType":"adversarial-reviewer"}"#,
        )
        .unwrap();
        assert_eq!(subagent_label(&file, "a1"), "adversarial-reviewer");
        std::fs::write(
            dir.path().join("agent-a1.meta.json"),
            r#"{"agentType":"adversarial-reviewer","description":"审一下改动"}"#,
        )
        .unwrap();
        assert_eq!(subagent_label(&file, "a1"), "审一下改动");
    }

    /// 接线:chat 线程起上报线程,并在每处先停 watcher 再删 `threads/<tag>` 的地方一并停掉它
    /// (它写出站归档,晚停会把删掉的目录重新建出来)。
    #[test]
    fn chat_thread_starts_the_reporter_and_stops_it_with_the_watcher() {
        let src = include_str!("chat_thread.rs");
        let prod = src.split_once("#[cfg(test)]").expect("测试模块分界").0;
        assert_eq!(
            prod.matches("spawn_subagent_api_error_reporter(").count(),
            1
        );
        let watcher_stops = prod.matches("drop(subagent_watcher.take());").count();
        assert!(watcher_stops > 0, "扫描器失效");
        assert_eq!(
            prod.matches("drop(subagent_api_errors.take());").count(),
            watcher_stops,
            "停 watcher 的地方都要同时停上报线程"
        );
    }

    fn report(uuid: Option<&str>, end_offset: u64) -> SubagentApiErrorReport {
        let mut error = ClaudeApiError::from_subagent_line(SUB_ERROR).unwrap();
        error.uuid = uuid.map(str::to_string);
        SubagentApiErrorReport {
            agent_id: "a1".into(),
            label: "审一下改动".into(),
            error,
            end_offset,
        }
    }

    /// 身份校验不过就不发;通过时发的是子 agent 提示卡。
    #[test]
    fn delivery_is_gated_by_the_identity_preflight() {
        let mut sent = Vec::new();
        let result = deliver_subagent_api_error(
            &report(Some("sub-err-1"), 10),
            || anyhow::bail!("identity revoked"),
            |card| {
                sent.push(card.to_string());
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(sent.is_empty(), "身份失效必须 fail-closed");

        deliver_subagent_api_error(
            &report(Some("sub-err-1"), 10),
            || Ok(()),
            |card| {
                sent.push(card.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("审一下改动") && sent[0].contains("server_error"));
        assert!(!sent[0].contains("retry_now"), "不给「继续」");
    }

    /// 幂等键:同一条记录恒同键;换记录、换子 agent、换会话都换键。缺 uuid 时按偏移区分。
    #[test]
    fn idempotency_key_follows_the_record() {
        let key = |sid: &str, report: &SubagentApiErrorReport| {
            subagent_api_error_uuid("epoch", "oc_chat", sid, report)
        };
        let base = key("sid", &report(Some("sub-err-1"), 10));
        assert_eq!(base, key("sid", &report(Some("sub-err-1"), 99)));
        assert_ne!(base, key("sid", &report(Some("sub-err-2"), 10)));
        assert_ne!(base, key("sid-2", &report(Some("sub-err-1"), 10)));
        let mut other_agent = report(Some("sub-err-1"), 10);
        other_agent.agent_id = "a2".into();
        assert_ne!(base, key("sid", &other_agent));
        assert_ne!(key("sid", &report(None, 10)), key("sid", &report(None, 20)));
    }
}
