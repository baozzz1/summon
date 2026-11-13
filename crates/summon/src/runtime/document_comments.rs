//! 文档评论的 durable 收件箱与审批存根。网络动作在事务之外执行，发布权由 Sending 状态认领。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use crate::runtime::fsio::atomic_write_mode;

pub(crate) const DRAFT_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_RECORD_BYTES: u64 = 256 * 1024;
const MAX_PENDING: usize = 4096;

/// 评论通知不收:格式无效、身份字段与已有存根不一致、已有存根读不出、待办已满,或收到时文档评论
/// 功能没开(不留存根)。这些都不是传输故障,收件 ACK 按已处理答复;否则 sidecar 会把它当成传输
/// 失败退出,所有 bot 的长连接一起断。
#[derive(Debug, thiserror::Error)]
#[error("{}", summon_i18n::text!("r954764a4f1"))]
pub(crate) struct NoticeRejected;

pub(crate) fn is_rejected(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NoticeRejected>().is_some()
}

/// 文档归属及 owner 提及都未取得处理授权。这类通知静默拒收，避免任意评论者触发 owner 私聊。
#[derive(Debug, thiserror::Error)]
#[error("{}", summon_i18n::text!("r0b58318263"))]
pub(crate) struct OutOfScope;

pub(crate) fn is_out_of_scope(error: &anyhow::Error) -> bool {
    error.downcast_ref::<OutOfScope>().is_some()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommentNotice {
    pub app_id: String,
    pub event_id: String,
    pub file_token: String,
    pub file_type: String,
    pub comment_id: String,
    pub reply_id: String,
    pub sender_open_id: String,
    pub recipient_open_id: String,
}

impl CommentNotice {
    pub(crate) fn validate(&self) -> Result<()> {
        for value in [
            &self.app_id,
            &self.event_id,
            &self.file_token,
            &self.file_type,
            &self.comment_id,
            &self.reply_id,
            &self.sender_open_id,
            &self.recipient_open_id,
        ] {
            anyhow::ensure!(
                !value.is_empty()
                    && value.len() <= 256
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
                summon_i18n::format!("r2f67e555b5")
            );
        }
        anyhow::ensure!(
            matches!(
                self.file_type.as_str(),
                "doc" | "docx" | "sheet" | "bitable" | "file" | "slides"
            ),
            summon_i18n::format!("rd6dfe1a463")
        );
        Ok(())
    }

    pub(crate) fn key(&self) -> String {
        // 一条回复可能产生多份通知；app 与目标回复决定逻辑身份，event_id 仅用于传输 ACK。
        let parts = [
            &self.app_id,
            &self.file_type,
            &self.file_token,
            &self.comment_id,
            &self.reply_id,
        ];
        let source = serde_json::to_vec(&parts).expect("string array serializes");
        hex_digest(&source)
    }
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    Received,
    Prepared,
    Notified,
    Drafting,
    DraftReady,
    Sending,
    Replied,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommentContext {
    #[serde(default)]
    pub owner_mention: bool,
    pub title: String,
    pub url: String,
    pub quote: String,
    pub thread: String,
    pub fingerprint: String,
    pub document_modified_time: String,
    #[serde(default, skip_serializing)]
    pub document_text: String,
    pub reply_allowed: bool,
}

/// PID 与启动时刻一起确认，避免把重启后复用的 PID 显示为起草进程。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DraftProcessIdentity {
    pid: i32,
    start: String,
    daemon_pid: u32,
}
impl DraftProcessIdentity {
    pub(crate) fn capture(pid: i32) -> Option<Self> {
        Some(Self {
            pid,
            start: crate::capture::rpc_procs::probe_start(pid)?,
            daemon_pid: std::process::id(),
        })
    }
    pub(crate) fn live_pid(&self, daemon_pid: Option<u32>) -> Option<i32> {
        (daemon_pid == Some(self.daemon_pid)
            && crate::capture::rpc_procs::probe_start(self.pid).as_deref()
                == Some(self.start.as_str()))
        .then_some(self.pid)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    pub schema: u32,
    pub id: String,
    pub notice: CommentNotice,
    pub received_ms: i64,
    pub stage: Stage,
    pub agent_name: String,
    pub delivery_scope: String,
    pub epoch: String,
    pub owner_entry: String,
    pub owner_open_id: String,
    pub owner_union_id: String,
    pub bot_open_id: String,
    pub context: Option<CommentContext>,
    pub notice_message_id: Option<String>,
    pub owner_chat_id: Option<String>,
    pub revision: u32,
    pub draft: Option<String>,
    pub drafted_ms: Option<i64>,
    pub approval_message_id: Option<String>,
    pub reply_id: Option<String>,
    pub outcome: Option<String>,
    pub retry_after_ms: i64,
    pub failures: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub updated_ms: i64,
    #[serde(default)]
    pub draft_process: Option<DraftProcessIdentity>,
    #[serde(default = "legacy_draft_started")]
    pub draft_started: bool,
    #[serde(default)]
    pub failure_notice_message_id: Option<String>,
    #[serde(default)]
    pub failure_notice_attempts: u32,
    /// 这次失败要通知 owner:处理失败那一刻登记,文档不在处理范围、功能已关或身份已变时不登记。
    /// 缺省为假:没登记过的失败不发通知。
    #[serde(default)]
    pub notify_failure: bool,
}

fn legacy_draft_started() -> bool {
    // 旧存根缺少单次认领信息，只有尚未进入起草的阶段能证明模型未启动。
    true
}

impl Record {
    fn received(notice: CommentNotice, now_ms: i64) -> Self {
        Self {
            schema: 1,
            id: notice.key(),
            notice,
            received_ms: now_ms,
            stage: Stage::Received,
            agent_name: String::new(),
            delivery_scope: String::new(),
            epoch: String::new(),
            owner_entry: String::new(),
            owner_open_id: String::new(),
            owner_union_id: String::new(),
            bot_open_id: String::new(),
            context: None,
            notice_message_id: None,
            owner_chat_id: None,
            revision: 0,
            draft: None,
            drafted_ms: None,
            approval_message_id: None,
            reply_id: None,
            outcome: None,
            retry_after_ms: 0,
            failures: 0,
            last_error: None,
            updated_ms: now_ms,
            draft_process: None,
            draft_started: false,
            failure_notice_message_id: None,
            failure_notice_attempts: 0,
            notify_failure: false,
        }
    }

    fn is_pending(&self) -> bool {
        !matches!(
            self.stage,
            Stage::Closed | Stage::Replied | Stage::DraftReady | Stage::Sending
        ) || self.needs_failure_notice()
    }

    pub(crate) fn needs_failure_notice(&self) -> bool {
        self.stage == Stage::Closed
            && self.failures > 0
            && self.notify_failure
            && !self.owner_open_id.is_empty()
            && self.failure_notice_message_id.is_none()
            && self.failure_notice_attempts == 0
    }

    pub(crate) fn close(&mut self, outcome: &str) {
        self.stage = Stage::Closed;
        self.outcome = Some(outcome.to_string());
    }

    pub(crate) fn approval_matches(
        &self,
        agent_name: &str,
        scope: &str,
        chat_id: &str,
        message_id: &str,
        operator: &str,
        now_ms: i64,
    ) -> bool {
        self.stage == Stage::DraftReady
            && self.agent_name == agent_name
            && self.delivery_scope == scope
            && self.owner_chat_id.as_deref() == Some(chat_id)
            && self.approval_message_id.as_deref() == Some(message_id)
            && !operator.is_empty()
            && self.owner_open_id == operator
            && self
                .drafted_ms
                .is_some_and(|at| at <= now_ms && now_ms - at < DRAFT_TTL_MS)
            && self
                .draft
                .as_ref()
                .is_some_and(|text| !text.trim().is_empty())
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[derive(Clone)]
pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn stop_draft(&self, id: &str, agent: &summon_core::registry::Agent) -> Result<()> {
        let current = agent.reread().context(summon_i18n::text!("r50c89c40c6"))?;
        let app = crate::runtime::daemon::resolve_creds(&current)
            .0
            .context(summon_i18n::text!("r0d4dd70317"))?;
        let binding = TaskIdentity {
            agent: current.name.clone(),
            app,
            epoch: crate::runtime::daemon::feishu_identity_fingerprint(&current),
            owner: current
                .feishu
                .as_ref()
                .and_then(|f| f.allowed_users.first())
                .cloned()
                .unwrap_or_default(),
        };
        self.stop_bound(id, &binding)
    }

    fn stop_bound(&self, id: &str, binding: &TaskIdentity) -> Result<()> {
        self.update(id, |r| {
            anyhow::ensure!(
                r.notice.app_id == binding.app
                    && (r.agent_name.is_empty() || r.agent_name == binding.agent)
                    && (r.epoch.is_empty() || r.epoch == binding.epoch)
                    && (r.owner_entry.is_empty() || r.owner_entry == binding.owner),
                summon_i18n::format!("rc68f1262ea")
            );
            if r.stage == Stage::Closed {
                return Ok(());
            }
            anyhow::ensure!(
                r.draft.is_none()
                    && matches!(
                        r.stage,
                        Stage::Received | Stage::Prepared | Stage::Notified | Stage::Drafting
                    ),
                summon_i18n::format!("r602f2c1f25")
            );
            r.close(summon_i18n::text!("r2f57ba776f"));
            Ok(())
        })
    }

    pub(crate) fn claim_draft(&self, id: &str) -> Result<()> {
        self.update(id, |record| {
            anyhow::ensure!(
                record.stage == Stage::Drafting
                    && !record.draft_started
                    && record.draft_process.is_none()
                    && record.failures == 0,
                summon_i18n::format!("r3380189627")
            );
            record.draft_started = true;
            Ok(())
        })
    }

    pub(crate) fn claim_reply(
        &self,
        id: &str,
        binding: Approval<'_>,
        context: &CommentContext,
    ) -> Result<String> {
        self.update(id, |r| {
            anyhow::ensure!(
                r.approval_matches(
                    binding.agent,
                    binding.scope,
                    binding.chat,
                    binding.card,
                    binding.operator,
                    binding.now_ms
                ),
                summon_i18n::format!("r8bc6fc440e")
            );
            let frozen = r
                .context
                .as_ref()
                .context(summon_i18n::text!("r2563ec0f7d"))?;
            anyhow::ensure!(
                context.reply_allowed
                    && frozen.fingerprint == context.fingerprint
                    && frozen.document_modified_time == context.document_modified_time,
                summon_i18n::format!("rbb11dd559c")
            );
            let text = r.draft.clone().context(summon_i18n::text!("re4221ed441"))?;
            r.last_error = None;
            r.stage = Stage::Sending;
            Ok(text)
        })
    }
    pub(crate) fn at_run_dir() -> Self {
        Self::new(crate::runtime::paths::run_dir().join("document-comments"))
    }

    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        anyhow::ensure!(valid_id(id), summon_i18n::format!("r1b427c2a88"));
        Ok(self.root.join(format!("{id}.json")))
    }

    fn lock(&self) -> Result<File> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(self.root.join(".lock"))?;
        lock.lock()?;
        Ok(lock)
    }

    pub(crate) fn get(&self, id: &str) -> Result<Record> {
        let path = self.path(id)?;
        anyhow::ensure!(
            std::fs::metadata(&path)?.len() <= MAX_RECORD_BYTES,
            summon_i18n::format!("r23bfdf0b55")
        );
        let record: Record = serde_json::from_slice(&std::fs::read(&path)?)
            .context(summon_i18n::text!("r392f04d2d4"))?;
        anyhow::ensure!(
            record.schema == 1 && record.id == id && record.notice.key() == id,
            summon_i18n::format!("r85bf3f99b7")
        );
        Ok(record)
    }

    fn write(&self, record: &Record) -> Result<()> {
        let bytes = serde_json::to_vec(record)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_RECORD_BYTES,
            summon_i18n::format!("r23bfdf0b55")
        );
        atomic_write_mode(&self.path(&record.id)?, &bytes, 0o600)
    }

    pub(crate) fn receive(
        &self,
        notice: CommentNotice,
        now_ms: i64,
        active: bool,
    ) -> Result<String> {
        self.admit(notice, now_ms, active, MAX_PENDING)
    }

    fn admit(
        &self,
        notice: CommentNotice,
        now_ms: i64,
        active: bool,
        max_pending: usize,
    ) -> Result<String> {
        notice.validate().context(NoticeRejected)?;
        let _lock = self.lock()?;
        let id = notice.key();
        if self.path(&id)?.exists() {
            let existing = self.get(&id).context(NoticeRejected)?;
            if existing.notice.sender_open_id != notice.sender_open_id
                || existing.notice.recipient_open_id != notice.recipient_open_id
            {
                return Err(
                    anyhow::anyhow!(summon_i18n::format!("rae288f74ad")).context(NoticeRejected)
                );
            }
            return Ok(id);
        }
        if !active {
            return Err(
                anyhow::anyhow!(summon_i18n::format!("r568d5ec67a")).context(NoticeRejected)
            );
        }
        if self.backlog()? >= max_pending {
            return Err(
                anyhow::anyhow!(summon_i18n::format!("r871aa67c34")).context(NoticeRejected)
            );
        }
        self.write(&Record::received(notice, now_ms))?;
        Ok(id)
    }

    pub(crate) fn update<T>(
        &self,
        id: &str,
        change: impl FnOnce(&mut Record) -> Result<T>,
    ) -> Result<T> {
        let _lock = self.lock()?;
        let mut record = self.get(id)?;
        let result = change(&mut record)?;
        record.updated_ms = chrono::Utc::now().timestamp_millis();
        self.write(&record)?;
        Ok(result)
    }

    pub(crate) fn all(&self) -> Result<Vec<Record>> {
        let mut records = self.scan()?.into_iter().collect::<Result<Vec<_>>>()?;
        records.sort_by_key(|record| record.received_ms);
        Ok(records)
    }

    /// 逐个读存根;单个读不出时留在结果里,由调用方决定整体失败还是照常计数。
    fn scan(&self) -> Result<Vec<Result<Record>>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut records = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            records.push(
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .context(summon_i18n::text!("r77b00fe354"))
                    .and_then(|id| {
                        self.get(id)
                            .with_context(|| summon_i18n::format!("rae1e461160"))
                    }),
            );
        }
        Ok(records)
    }

    /// 待处理的存根,以及读不出的存根各自的错误(带存根 id)。读不出的存根本来就无法处理,不挡
    /// 其余评论;处理线程据第二项报出它们,各端展示经 [`Self::all`] 报出。
    pub(crate) fn inbox(&self) -> Result<(Vec<Record>, Vec<String>)> {
        let mut records = Vec::new();
        let mut unreadable = Vec::new();
        for record in self.scan()? {
            match record {
                Ok(record) if record.is_pending() => records.push(record),
                Ok(_) => {}
                Err(error) => unreadable.push(format!("{error:#}")),
            }
        }
        records.sort_by_key(|record| record.received_ms);
        Ok((records, unreadable))
    }

    /// 收件时的待办数。读不出的存根也算一条待办:一份坏文件不挡新通知入箱,各端展示仍经
    /// [`Self::all`] 报出它。
    fn backlog(&self) -> Result<usize> {
        Ok(self
            .scan()?
            .iter()
            .filter(|record| record.as_ref().map_or(true, Record::is_pending))
            .count())
    }

    pub(crate) fn set_draft(&self, id: &str, revision: u32, text: &str, now_ms: i64) -> Result<()> {
        anyhow::ensure!(
            !text.trim().is_empty() && text.chars().count() <= 10_000,
            summon_i18n::format!("r03c3b6a310")
        );
        self.update(id, |record| {
            if record.stage != Stage::Drafting || record.revision != revision {
                bail!(summon_i18n::format!("r98a3e96780"));
            }
            record.draft = Some(text.to_string());
            record.drafted_ms = Some(now_ms);
            // 草稿卡的 message_id 落盘后才有批准权。
            record.stage = Stage::Prepared;
            Ok(())
        })
    }
}

pub(crate) struct Approval<'a> {
    pub agent: &'a str,
    pub scope: &'a str,
    pub chat: &'a str,
    pub card: &'a str,
    pub operator: &'a str,
    pub now_ms: i64,
}

pub(crate) fn read_only_context_prompt(record: &Record) -> Result<String> {
    let context = record
        .context
        .as_ref()
        .context(summon_i18n::text!("r4826c9cae1"))?;
    let data = serde_json::json!({
        "document_title": context.title,
        "document_url": context.url,
        "quoted_text": context.quote,
        "comment_thread": context.thread,
    });
    Ok(summon_i18n::format!("r23dbfd68e2"))
}

pub(crate) struct TaskIdentity {
    pub agent: String,
    pub app: String,
    pub epoch: String,
    pub owner: String,
}

/// 只按当前 registry 的 app 归属投影任务；每次读取仍验证存根身份。
pub(crate) fn task_snapshot(
    store: &Store,
    agents: &[TaskIdentity],
    daemon_pid: Option<u32>,
    now_ms: i64,
) -> Result<Vec<summon_core::fleet::CommentTask>> {
    let mut tasks = Vec::new();
    for r in store.all()? {
        let matched: Vec<_> = agents
            .iter()
            .filter(|identity| identity.app == r.notice.app_id)
            .collect();
        let [binding] = matched.as_slice() else {
            continue;
        };
        if !r.agent_name.is_empty() && r.agent_name != binding.agent {
            continue;
        }
        if (!r.epoch.is_empty() && r.epoch != binding.epoch)
            || (!r.owner_entry.is_empty() && r.owner_entry != binding.owner)
        {
            continue;
        }
        let updated = r.updated_ms.max(r.received_ms);
        if matches!(r.stage, Stage::Closed | Stage::Replied)
            && now_ms - updated > 7 * 24 * 3600 * 1000
        {
            continue;
        }
        let pid = r
            .draft_process
            .as_ref()
            .and_then(|p| p.live_pid(daemon_pid));
        let (stage, status, status_en) = match r.stage {
            Stage::Received => ("received", summon_i18n::text!("rfaca7cef53"), "Received"),
            Stage::Prepared if r.draft.is_some() => (
                "prepared",
                summon_i18n::text!("r4cf8408ea4"),
                "Delivering draft",
            ),
            Stage::Prepared | Stage::Notified => (
                "queued",
                summon_i18n::text!("r8c6696b158"),
                "Queued for draft",
            ),
            Stage::Drafting if pid.is_some() => {
                ("drafting", summon_i18n::text!("rc5af8b49b5"), "Drafting")
            }
            Stage::Drafting => (
                "queued",
                summon_i18n::text!("r8c6696b158"),
                "Queued for draft",
            ),
            Stage::DraftReady if r.drafted_ms.is_some_and(|t| now_ms - t >= DRAFT_TTL_MS) => (
                "expired",
                summon_i18n::text!("r114d6e196d"),
                "Draft expired",
            ),
            Stage::DraftReady if r.last_error.is_some() => (
                "approval_failed",
                summon_i18n::text!("r63bfaec9ed"),
                "Approval failed",
            ),
            Stage::DraftReady => (
                "draft_ready",
                summon_i18n::text!("r43ad5d3f01"),
                "Awaiting approval",
            ),
            Stage::Sending => (
                "sending",
                summon_i18n::text!("r57ee8db689"),
                "Verifying delivery",
            ),
            Stage::Replied => ("replied", summon_i18n::text!("r6e764af4b0"), "Replied"),
            Stage::Closed if pid.is_some() => (
                "stopping",
                summon_i18n::text!("r62a4f7023c"),
                "Stopping draft",
            ),
            Stage::Closed if r.last_error.is_some() || r.failures > 0 => {
                ("failed", summon_i18n::text!("rae173945fb"), "Failed")
            }
            Stage::Closed => ("closed", summon_i18n::text!("rd79b1d0e5c"), "Closed"),
        };
        let (stage, status, status_en) = if daemon_pid.is_none()
            && r.draft.is_none()
            && !matches!(
                r.stage,
                Stage::DraftReady | Stage::Sending | Stage::Replied | Stage::Closed
            ) {
            (
                "paused",
                summon_i18n::text!("r5a6d6d2a09"),
                "Waiting for daemon",
            )
        } else {
            (stage, status, status_en)
        };
        tasks.push(summon_core::fleet::CommentTask {
            id: r.id,
            agent: binding.agent.clone(),
            title: r
                .context
                .as_ref()
                .map(|c| c.title.clone())
                .unwrap_or_else(|| summon_i18n::text!("r2549c41532").into()),
            stage: stage.into(),
            status: status.into(),
            status_en: status_en.into(),
            busy: pid.is_some(),
            received_ms: r.received_ms,
            updated_ms: updated,
            pid,
            error: r.last_error.or(r.outcome),
        });
    }
    tasks.sort_by_key(|task| std::cmp::Reverse(task.updated_ms));
    let mut terminal = 0;
    tasks.retain(|task| {
        if matches!(task.stage.as_str(), "closed" | "replied" | "failed") {
            terminal += 1;
            terminal <= 100
        } else {
            true
        }
    });
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_contexts_keep_bot_owned_access() {
        let context: CommentContext = serde_json::from_value(serde_json::json!({
            "title":"Title", "url":"https://example.feishu.cn/docx/test", "quote":"", "thread":"",
            "fingerprint":"abc", "document_modified_time":"100", "reply_allowed":true
        }))
        .unwrap();
        assert!(!context.owner_mention);
    }

    #[test]
    fn stopping_is_bound_durable_and_cannot_replace_a_frozen_draft() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = store.receive(notice(), 100, true).unwrap();
        let mut binding = TaskIdentity {
            agent: "writer".into(),
            app: "other".into(),
            epoch: "epoch".into(),
            owner: "ou_owner".into(),
        };
        assert!(store.stop_bound(&id, &binding).is_err());
        binding.app = "cli_test".into();
        store
            .update(&id, |r| {
                r.stage = Stage::Drafting;
                r.agent_name = "writer".into();
                r.epoch = "epoch".into();
                r.owner_entry = "ou_owner".into();
                Ok(())
            })
            .unwrap();
        for field in ["agent", "epoch", "owner"] {
            let mut wrong = TaskIdentity {
                agent: binding.agent.clone(),
                app: binding.app.clone(),
                epoch: binding.epoch.clone(),
                owner: binding.owner.clone(),
            };
            match field {
                "agent" => wrong.agent = "other".into(),
                "epoch" => wrong.epoch = "other".into(),
                _ => wrong.owner = "other".into(),
            }
            assert!(store.stop_bound(&id, &wrong).is_err());
        }
        store.claim_draft(&id).unwrap();
        store.stop_bound(&id, &binding).unwrap();
        let restarted = Store::new(dir.path());
        assert_eq!(restarted.get(&id).unwrap().stage, Stage::Closed);
        assert!(restarted.inbox().unwrap().0.is_empty());
        assert!(restarted.claim_draft(&id).is_err());
        assert!(restarted.set_draft(&id, 0, "late result", 200).is_err());
        restarted.stop_bound(&id, &binding).unwrap();
        restarted
            .update(&id, |r| {
                r.stage = Stage::Prepared;
                r.draft = Some("frozen".into());
                Ok(())
            })
            .unwrap();
        assert!(restarted.stop_bound(&id, &binding).is_err());
        assert_eq!(restarted.get(&id).unwrap().draft.as_deref(), Some("frozen"));
    }

    #[test]
    fn initial_prompt_discloses_document_link_without_document_body() {
        let mut record = Record::received(notice(), 1);
        record.context = Some(CommentContext {
            owner_mention: false,
            title: "Title".into(),
            url: "https://example.com/docx/doc_test".into(),
            quote: "Quoted selection".into(),
            thread: "Reader: Question".into(),
            fingerprint: "frozen".into(),
            document_modified_time: "1".into(),
            document_text: "FULL_DOCUMENT_SENTINEL".into(),
            reply_allowed: true,
        });
        let prompt = read_only_context_prompt(&record).unwrap();
        assert!(prompt.contains("https://example.com/docx/doc_test"));
        assert!(prompt.contains("Quoted selection") && prompt.contains("Reader: Question"));
        assert!(!prompt.contains("FULL_DOCUMENT_SENTINEL"));
        assert!(!prompt.contains("\"document_text\""));
        assert!(!serde_json::to_string(&record)
            .unwrap()
            .contains("FULL_DOCUMENT_SENTINEL"));
    }

    fn notice() -> CommentNotice {
        CommentNotice {
            app_id: "cli_test".into(),
            event_id: "evt_1".into(),
            file_token: "doc_test".into(),
            file_type: "docx".into(),
            comment_id: "100".into(),
            reply_id: "200".into(),
            sender_open_id: "ou_reader".into(),
            recipient_open_id: "ou_bot".into(),
        }
    }

    #[test]
    fn draft_claim_is_durable_exclusive_and_legacy_drafting_is_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = store.receive(notice(), 100, true).unwrap();
        store
            .update(&id, |r| {
                r.stage = Stage::Drafting;
                Ok(())
            })
            .unwrap();
        store.claim_draft(&id).unwrap();
        let restarted = Store::new(dir.path());
        assert!(restarted.get(&id).unwrap().draft_started);
        assert!(restarted.claim_draft(&id).is_err());
        let mut legacy = serde_json::to_value(store.get(&id).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("draft_started");
        let record: Record = serde_json::from_value(legacy).unwrap();
        assert!(record.draft_started);
    }

    #[test]
    fn task_projection_survives_restart_and_keeps_private_content_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = store.receive(notice(), 100, true).unwrap();
        let binding = |agent: &str, app: &str| TaskIdentity {
            agent: agent.into(),
            app: app.into(),
            epoch: "epoch".into(),
            owner: "ou_owner".into(),
        };
        let agents = vec![binding("writer", "cli_test")];
        let projection = |store: &Store, daemon| {
            task_snapshot(
                store,
                &agents,
                daemon,
                chrono::Utc::now().timestamp_millis(),
            )
            .unwrap()
        };
        assert_eq!(projection(&store, None)[0].stage, "paused");
        assert_eq!(projection(&store, Some(123))[0].stage, "received");
        store
            .update(&id, |r| {
                r.stage = Stage::DraftReady;
                r.draft = Some("private-draft".into());
                r.drafted_ms = Some(chrono::Utc::now().timestamp_millis());
                Ok(())
            })
            .unwrap();
        let restarted = Store::new(dir.path());
        assert_eq!(projection(&restarted, Some(123))[0].stage, "draft_ready");
        assert!(!serde_json::to_string(&projection(&restarted, Some(123)))
            .unwrap()
            .contains("private-draft"));
        store
            .update(&id, |r| {
                r.stage = Stage::Drafting;
                r.draft_process = DraftProcessIdentity::capture(std::process::id() as i32);
                Ok(())
            })
            .unwrap();
        assert!(projection(&store, Some(std::process::id()))[0].busy);
        assert_eq!(
            projection(&store, Some(std::process::id() + 1))[0].pid,
            None
        );
        store
            .update(&id, |r| {
                r.close("处理失败");
                r.last_error = Some("创建者无法验证".into());
                r.draft_process = None;
                Ok(())
            })
            .unwrap();
        let tasks = projection(&restarted, Some(123));
        assert_eq!(tasks[0].stage, "failed");
        assert_eq!(tasks[0].error.as_deref(), Some("创建者无法验证"));
        assert!(
            task_snapshot(&store, &[binding("other", "cli_other")], Some(123), 200)
                .unwrap()
                .is_empty()
        );
        assert!(task_snapshot(
            &store,
            &[
                binding("writer", "cli_test"),
                binding("ambiguous", "cli_test")
            ],
            Some(123),
            200
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn notifications_deduplicate_by_app_and_reply_across_event_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let first = store.receive(notice(), 1, true).unwrap();
        let mut again = notice();
        again.event_id = "evt_retry".into();
        assert_eq!(store.receive(again.clone(), 2, true).unwrap(), first);
        again.app_id = "cli_another".into();
        assert_ne!(store.receive(again, 3, true).unwrap(), first);
        assert_eq!(store.inbox().unwrap().0.len(), 2);
    }

    #[test]
    fn corrupt_state_and_path_traversal_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = store.receive(notice(), 1, true).unwrap();
        assert!(store.get("../anything").is_err());
        std::fs::write(dir.path().join(format!("{id}.json")), "{}").unwrap();
        let replay = store.receive(notice(), 2, true).unwrap_err();
        assert!(is_rejected(&replay), "{replay:#}");
        assert!(store.inbox().unwrap().0.is_empty());
        assert!(store.all().is_err());
    }

    #[test]
    fn notices_that_can_never_be_stored_are_rejected_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let mut unsupported = notice();
        unsupported.file_type = "mindnote".into();
        let error = store.receive(unsupported, 1, true).unwrap_err();
        assert!(is_rejected(&error), "{error:#}");

        store.receive(notice(), 1, true).unwrap();
        let mut forged = notice();
        forged.sender_open_id = "ou_other".into();
        let error = store.receive(forged, 2, true).unwrap_err();
        assert!(is_rejected(&error), "{error:#}");

        let mut next = notice();
        next.reply_id = "201".into();
        let error = store.admit(next, 3, true, 1).unwrap_err();
        assert!(is_rejected(&error), "{error:#}");
        assert_eq!(store.inbox().unwrap().0.len(), 1);
    }

    #[test]
    fn notices_received_while_disabled_leave_no_stub() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let error = store.receive(notice(), 1, false).unwrap_err();
        assert!(is_rejected(&error), "{error:#}");
        assert!(store.all().unwrap().is_empty());
    }

    #[test]
    fn a_corrupt_record_blocks_neither_new_notices_nor_other_work_and_stays_visible() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = store.receive(notice(), 1, true).unwrap();
        std::fs::write(dir.path().join(format!("{id}.json")), "{}").unwrap();
        let mut next = notice();
        next.reply_id = "201".into();
        let stored = store.receive(next, 2, true).unwrap();
        let pending = store.inbox().unwrap().0;
        assert_eq!(
            pending.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            [stored.as_str()]
        );
        let (_, unreadable) = store.inbox().unwrap();
        assert_eq!(unreadable.len(), 1);
        assert!(unreadable[0].contains(&id), "{unreadable:?}");
        let shown = format!("{:#}", store.all().unwrap_err());
        assert!(shown.contains(&id), "{shown}");

        let mut third = notice();
        third.reply_id = "202".into();
        let error = store.admit(third, 3, true, 2).unwrap_err();
        assert!(is_rejected(&error), "坏存根也占一条待办: {error:#}");
    }

    #[test]
    fn storage_failures_stay_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("inbox");
        std::fs::write(&root, "not a directory").unwrap();
        let error = Store::new(&root).receive(notice(), 1, true).unwrap_err();
        assert!(!is_rejected(&error), "{error:#}");
    }

    #[test]
    fn approval_is_bound_to_owner_card_scope_chat_and_exact_pending_draft() {
        let mut r = Record::received(notice(), 1);
        r.stage = Stage::DraftReady;
        r.agent_name = "writer".into();
        r.delivery_scope = "scope".into();
        r.owner_open_id = "ou_owner".into();
        r.owner_chat_id = Some("oc_owner".into());
        r.approval_message_id = Some("om_approval".into());
        r.drafted_ms = Some(100);
        r.draft = Some("回复正文".into());
        let allowed = |r: &Record, agent, scope, chat, card, operator, now| {
            r.approval_matches(agent, scope, chat, card, operator, now)
        };
        assert!(allowed(
            &r,
            "writer",
            "scope",
            "oc_owner",
            "om_approval",
            "ou_owner",
            101
        ));
        for (agent, scope, chat, card, op, now) in [
            ("other", "scope", "oc_owner", "om_approval", "ou_owner", 101),
            (
                "writer",
                "other",
                "oc_owner",
                "om_approval",
                "ou_owner",
                101,
            ),
            (
                "writer",
                "scope",
                "oc_other",
                "om_approval",
                "ou_owner",
                101,
            ),
            ("writer", "scope", "oc_owner", "om_copy", "ou_owner", 101),
            (
                "writer",
                "scope",
                "oc_owner",
                "om_approval",
                "ou_reader",
                101,
            ),
            (
                "writer",
                "scope",
                "oc_owner",
                "om_approval",
                "ou_owner",
                100 + DRAFT_TTL_MS,
            ),
            ("writer", "scope", "oc_owner", "om_approval", "ou_owner", 99),
        ] {
            assert!(!allowed(&r, agent, scope, chat, card, op, now));
        }
        r.stage = Stage::Sending;
        assert!(!allowed(
            &r,
            "writer",
            "scope",
            "oc_owner",
            "om_approval",
            "ou_owner",
            101
        ));
    }

    #[test]
    fn concurrent_claims_authorize_only_one_publisher() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = store.receive(notice(), 1, true).unwrap();
        store
            .update(&id, |r| {
                r.stage = Stage::DraftReady;
                r.agent_name = "writer".into();
                r.delivery_scope = "scope".into();
                r.owner_open_id = "ou_owner".into();
                r.owner_chat_id = Some("oc_owner".into());
                r.approval_message_id = Some("om_card".into());
                r.drafted_ms = Some(100);
                r.draft = Some("frozen draft".into());
                r.context = Some(CommentContext {
                    owner_mention: false,
                    title: "Title".into(),
                    url: "https://example.com".into(),
                    quote: String::new(),
                    thread: String::new(),
                    fingerprint: "original".into(),
                    document_modified_time: "100".into(),
                    document_text: String::new(),
                    reply_allowed: true,
                });
                Ok(())
            })
            .unwrap();
        let frozen = store.get(&id).unwrap().context.unwrap();
        for change in ["thread", "document", "solved"] {
            let mut changed = frozen.clone();
            match change {
                "thread" => changed.fingerprint = "new reply".into(),
                "document" => changed.document_modified_time = "101".into(),
                _ => changed.reply_allowed = false,
            }
            assert!(store
                .claim_reply(
                    &id,
                    Approval {
                        agent: "writer",
                        scope: "scope",
                        chat: "oc_owner",
                        card: "om_card",
                        operator: "ou_owner",
                        now_ms: 101
                    },
                    &changed
                )
                .is_err());
            assert_eq!(store.get(&id).unwrap().stage, Stage::DraftReady);
        }
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let s = store.clone();
                let id = id.clone();
                std::thread::spawn(move || {
                    let context = s.get(&id).unwrap().context.unwrap();
                    let claimed = s.claim_reply(
                        &id,
                        Approval {
                            agent: "writer",
                            scope: "scope",
                            chat: "oc_owner",
                            card: "om_card",
                            operator: "ou_owner",
                            now_ms: 101,
                        },
                        &context,
                    );
                    if let Ok(text) = &claimed {
                        assert_eq!(text, "frozen draft");
                    }
                    claimed.is_ok()
                })
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .filter(|claimed| *claimed)
                .count(),
            1
        );
        assert_eq!(
            Store::new(dir.path()).get(&id).unwrap().stage,
            Stage::Sending
        );
        let restarted = Store::new(dir.path());
        let context = restarted.get(&id).unwrap().context.unwrap();
        assert!(restarted
            .claim_reply(
                &id,
                Approval {
                    agent: "writer",
                    scope: "scope",
                    chat: "oc_owner",
                    card: "om_card",
                    operator: "ou_owner",
                    now_ms: 102
                },
                &context
            )
            .is_err());
        assert!(restarted.inbox().unwrap().0.is_empty());
    }

    #[test]
    fn incomplete_or_stale_drafts_cannot_become_approvable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = store.receive(notice(), 1, true).unwrap();
        assert!(store.set_draft(&id, 0, "draft", 10).is_err());
        store
            .update(&id, |r| {
                r.stage = Stage::Drafting;
                Ok(())
            })
            .unwrap();
        assert!(store.set_draft(&id, 1, "draft", 10).is_err());
        assert!(store.set_draft(&id, 0, " ", 10).is_err());
        store.set_draft(&id, 0, "complete draft", 10).unwrap();
        let r = store.get(&id).unwrap();
        assert_eq!(r.stage, Stage::Prepared);
        assert!(!r.approval_matches("writer", "scope", "chat", "card", "ou_owner", 11));
        assert!(store.set_draft(&id, 0, "different", 11).is_err());
        assert_eq!(
            store.get(&id).unwrap().draft.as_deref(),
            Some("complete draft")
        );
    }
}
