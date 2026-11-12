use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use summon_core::bridge::{
    BridgeState, ChatSlot, ContentPermit, ProcessFingerprint, ResumeProfile, SessionKey,
    BRIDGE_SCHEMA_VERSION,
};

pub const HOOK_ENVELOPE_VERSION: u32 = 1;
pub const MAX_PENDING_FILES: u64 = 4_096;
pub const MAX_PENDING_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_HOOK_EVENT_BYTES: usize = 1024 * 1024;
pub const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ARCHIVE_FILES: usize = 8_192;
const MAX_DEAD_FILES: usize = 1_024;
const MAX_RECALL_SOURCE_OBSERVATIONS: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEnvelope {
    pub schema_version: u32,
    pub event_id: String,
    pub observed_at_ms: u64,
    pub session: SessionKey,
    pub transcript_path: String,
    pub cwd: String,
    pub process: Option<ProcessFingerprint>,
    pub resume_profile: ResumeProfile,
    #[serde(default)]
    pub content_permit: Option<ContentPermit>,
    #[serde(default)]
    pub managed_takeover: bool,
    /// 仅 summon-managed staging 的一次 SessionStart 携带；保存 token digest，绝不落 raw token。
    #[serde(default)]
    pub handoff_ack: Option<HandoffHookAck>,
    pub event: HookEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffHookAck {
    pub attempt_id: String,
    pub token_sha256: String,
}

#[derive(Debug, Deserialize)]
struct HookTerminal {
    schema_version: u32,
    event_id: String,
    outcome: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart {
        source: String,
        model: Option<String>,
    },
    CwdChanged {
        old_cwd: Option<String>,
        new_cwd: String,
    },
    UserPromptSubmit {
        prompt_id: Option<String>,
        /// 未建立内容 binding 时恒为 None，避免 discovery spool 变成第二份转录。
        prompt: Option<String>,
    },
    Stop {
        prompt_id: Option<String>,
        /// 未建立内容 binding 时恒为 None。
        last_assistant_message: Option<String>,
        #[serde(default)]
        stop_hook_active: bool,
        #[serde(default)]
        has_pending_wakeups: bool,
    },
    StopFailure {
        prompt_id: Option<String>,
        error: Option<String>,
        /// 未建立内容 binding 时恒为 None。
        error_details: Option<String>,
        /// 未建立内容 binding 时恒为 None。
        last_assistant_message: Option<String>,
    },
    SessionEnd {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct BridgeStore {
    root: PathBuf,
    /// 只读路径的解析缓存，见 `load_state_cached`。clone 出来的 store 共享同一份缓存，
    /// 因为它们指向同一个 root，且每条缓存都自带盘上身份、命中前必复验。
    state_cache: Arc<Mutex<Option<CachedState>>>,
}

/// `state.json` 的盘上身份。state 只经 `atomic_write`(tmp + rename)提交，任何写者——包括
/// Claude Hook 这类**进程外**写者——每次提交都换一个 inode，因此 (dev, ino, len, mtime)
/// 变化能覆盖全部写者，不依赖任何进程内约定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StateFileIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
}

impl StateFileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            mtime_sec: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
        }
    }
}

#[derive(Debug)]
struct CachedState {
    identity: StateFileIdentity,
    state: Arc<BridgeState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeStateHealth {
    Absent,
    Healthy,
    Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStoreHealth {
    pub state: BridgeStateHealth,
    pub sessions: usize,
    pub active_sessions: usize,
    pub conflicts: usize,
    pub mirror_bindings: usize,
    pub activating_bindings: usize,
    pub handoff_sensitive_bindings: usize,
    pub managed_takeovers: usize,
    pub closing_bindings: usize,
    pub pending_turns: usize,
    pub delivery_receipts: usize,
    pub hook_pending: usize,
    pub hook_dead: usize,
}

impl BridgeStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state_cache: Arc::new(Mutex::new(None)),
        }
    }

    pub fn runtime_default() -> Self {
        Self::new(crate::runtime::paths::external_bridge_dir())
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn pending_dir(&self) -> PathBuf {
        self.root.join("events").join("pending")
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.root.join("events").join("archive")
    }

    pub fn dead_dir(&self) -> PathBuf {
        self.root.join("events").join("dead")
    }

    pub fn handoff_ack_dir(&self) -> PathBuf {
        self.root.join("handoff").join("acks")
    }

    pub fn handoff_ack_path(&self, attempt_id: &str) -> PathBuf {
        self.handoff_ack_dir().join(format!("{attempt_id}.json"))
    }

    pub fn handoff_attempt_dir(&self) -> PathBuf {
        self.root.join("handoff").join("attempts")
    }

    /// recall 全托管的 durable 记录目录(与 takeover 的 attempts 平行)。ACK 仍落共享
    /// `handoff_ack_dir`(hook 不区分 takeover/recall),但摘要源与孤儿判定都要认这里。
    pub fn recall_resume_dir(&self) -> PathBuf {
        self.root.join("handoff").join("recall_resume")
    }

    pub fn recall_resume_path(&self, id: &str) -> PathBuf {
        self.recall_resume_dir().join(format!("{id}.json"))
    }

    pub fn recall_source_observation_dir(&self) -> PathBuf {
        self.root.join("handoff").join("recall_sources")
    }

    pub fn recall_source_observation_path(&self, id: &str) -> PathBuf {
        self.recall_source_observation_dir()
            .join(format!("{id}.json"))
    }

    pub(crate) fn persist_recall_source_observation(
        &self,
        observation: &crate::bridge::recall::RecallSourceObservation,
    ) -> Result<()> {
        uuid::Uuid::parse_str(&observation.id).context(summon_i18n::text!("r1b26ecd06f"))?;
        anyhow::ensure!(
            observation.schema_version
                == crate::bridge::recall::RECALL_SOURCE_OBSERVATION_SCHEMA_VERSION,
            summon_i18n::format!("r4fa7c801e5")
        );
        anyhow::ensure!(
            !observation.sid.is_empty(),
            summon_i18n::format!("r2baf05f976")
        );
        anyhow::ensure!(
            observation.process.is_complete(),
            summon_i18n::format!("rc532d69e81")
        );
        self.ensure_layout()?;
        let path = self.recall_source_observation_path(&observation.id);
        let content =
            serde_json::to_vec_pretty(observation).context(summon_i18n::text!("red3f0dca03"))?;
        crate::runtime::fsio::atomic_write_mode(&path, &content, 0o600)
            .with_context(|| summon_i18n::format!("rd78aa88a2e"))?;
        self.prune_directory(
            &self.recall_source_observation_dir(),
            MAX_RECALL_SOURCE_OBSERVATIONS,
        )?;
        Ok(())
    }

    pub(crate) fn load_recall_source_observation(
        &self,
        id: &str,
    ) -> Result<crate::bridge::recall::RecallSourceObservation> {
        uuid::Uuid::parse_str(id).context(summon_i18n::text!("r1b26ecd06f"))?;
        let path = self.recall_source_observation_path(id);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("stat recall source observation {path:?}"))?;
        anyhow::ensure!(metadata.is_file(), summon_i18n::format!("rc7bd18d12e"));
        anyhow::ensure!(
            metadata.len() <= MAX_STATE_BYTES as u64,
            summon_i18n::format!("r7197762c32")
        );
        let mut content = Vec::new();
        File::open(&path)
            .with_context(|| summon_i18n::format!("raf14e1d51c"))?
            .take(MAX_STATE_BYTES as u64 + 1)
            .read_to_end(&mut content)
            .with_context(|| summon_i18n::format!("ra2e683349a"))?;
        anyhow::ensure!(
            content.len() <= MAX_STATE_BYTES,
            summon_i18n::format!("recd254a0aa")
        );
        let observation: crate::bridge::recall::RecallSourceObservation =
            serde_json::from_slice(&content)
                .with_context(|| summon_i18n::format!("rc912f1d80f"))?;
        anyhow::ensure!(observation.id == id, summon_i18n::format!("r28285a02ea"));
        anyhow::ensure!(
            observation.schema_version
                == crate::bridge::recall::RECALL_SOURCE_OBSERVATION_SCHEMA_VERSION,
            summon_i18n::format!("r4fa7c801e5")
        );
        anyhow::ensure!(
            !observation.sid.is_empty(),
            summon_i18n::format!("r2baf05f976")
        );
        anyhow::ensure!(
            observation.process.is_complete(),
            summon_i18n::format!("rc532d69e81")
        );
        Ok(observation)
    }

    pub fn is_managed_takeover_session(&self, session: &SessionKey) -> Result<bool> {
        Ok(self.load_state()?.is_some_and(|state| {
            state
                .managed_takeovers
                .values()
                .any(|takeover| takeover.session == *session)
        }))
    }

    pub fn persist_handoff_attempt(
        &self,
        attempt: &crate::bridge::handoff::HandoffAttempt,
    ) -> Result<()> {
        self.ensure_layout()?;
        let content =
            serde_json::to_vec_pretty(attempt).context(summon_i18n::text!("r4dea716c77"))?;
        crate::runtime::fsio::atomic_write_mode(
            &self
                .handoff_attempt_dir()
                .join(format!("{}.json", attempt.attempt_id())),
            &content,
            0o600,
        )
        .context(summon_i18n::text!("rf52b390a99"))
    }

    pub fn load_handoff_attempts(&self) -> Result<Vec<crate::bridge::handoff::HandoffAttempt>> {
        let dir = self.handoff_attempt_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| summon_i18n::format!("r658cab853b")),
        };
        let mut attempts: Vec<crate::bridge::handoff::HandoffAttempt> = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let content =
                std::fs::read(&path).with_context(|| summon_i18n::format!("r784e6a3d02"))?;
            attempts.push(
                serde_json::from_slice(&content)
                    .with_context(|| summon_i18n::format!("rcf95d9cf6c"))?,
            );
        }
        attempts.sort_by(|left, right| left.attempt_id().cmp(right.attempt_id()));
        Ok(attempts)
    }

    /// 只有 terminal 状态通知已经 archive/dead 后才回收 attempt。逐字段核对避免旧 outbox ACK
    /// 删除同 id 下被异常替换的其它 binding；不存在视为幂等成功。
    pub fn remove_terminal_handoff_attempt(
        &self,
        attempt_id: &str,
        binding_id: &str,
        binding_revision: u64,
    ) -> Result<bool> {
        uuid::Uuid::parse_str(attempt_id).context(summon_i18n::text!("rffcec76ea5"))?;
        let path = self
            .handoff_attempt_dir()
            .join(format!("{attempt_id}.json"));
        let content = match std::fs::read(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).with_context(|| summon_i18n::format!("r784e6a3d02")),
        };
        let attempt: crate::bridge::handoff::HandoffAttempt = serde_json::from_slice(&content)
            .with_context(|| summon_i18n::format!("rb63e6009af"))?;
        anyhow::ensure!(
            attempt.binding_id() == binding_id
                && attempt.binding_revision() == binding_revision
                && matches!(
                    attempt.phase(),
                    crate::bridge::handoff::HandoffPhase::Completed
                        | crate::bridge::handoff::HandoffPhase::Failed
                        | crate::bridge::handoff::HandoffPhase::CleanupBlocked { .. }
                ),
            summon_i18n::format!("rf4ccd8969e")
        );
        // ACK 含本机 cwd/transcript/process 元数据，只服务于活跃事务。先删 ACK、再删 terminal
        // attempt：任一步崩溃都可幂等重试，且不会留下“attempt 已不存在但 ACK 永久孤儿”。
        let ack_path = self.handoff_ack_path(attempt_id);
        match std::fs::remove_file(&ack_path) {
            Ok(()) => {
                let _ = std::fs::File::open(self.handoff_ack_dir()).and_then(|dir| dir.sync_all());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| summon_i18n::format!("r677ef44074")),
        }
        std::fs::remove_file(&path).with_context(|| summon_i18n::format!("r2956c9c5f9"))?;
        let _ = std::fs::File::open(self.handoff_attempt_dir()).and_then(|dir| dir.sync_all());
        Ok(true)
    }

    pub(crate) fn persist_recall_resume_attempt(
        &self,
        attempt: &crate::bridge::recall_resume::RecallResumeAttempt,
    ) -> Result<()> {
        uuid::Uuid::parse_str(&attempt.id).context(summon_i18n::text!("rdb8eb95e71"))?;
        self.ensure_layout()?;
        let content =
            serde_json::to_vec_pretty(attempt).context(summon_i18n::text!("r67a208429c"))?;
        crate::runtime::fsio::atomic_write_mode(
            &self.recall_resume_path(&attempt.id),
            &content,
            0o600,
        )
        .context(summon_i18n::text!("r36ded800ea"))
    }

    pub(crate) fn load_recall_resume_attempts(
        &self,
    ) -> Result<Vec<crate::bridge::recall_resume::RecallResumeAttempt>> {
        let dir = self.recall_resume_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| summon_i18n::format!("r658cab853b")),
        };
        let mut attempts: Vec<crate::bridge::recall_resume::RecallResumeAttempt> = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let content =
                std::fs::read(&path).with_context(|| summon_i18n::format!("r784e6a3d02"))?;
            attempts.push(
                serde_json::from_slice(&content)
                    .with_context(|| summon_i18n::format!("ree62fa05a6"))?,
            );
        }
        attempts.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(attempts)
    }

    /// recall 全托管终态(Bound/Failed)后回收:先删共享 ACK、再删 recall_resume 记录。
    /// 任一步崩溃可幂等重试;不存在视作成功。
    pub(crate) fn remove_terminal_recall_resume_attempt(&self, id: &str) -> Result<bool> {
        uuid::Uuid::parse_str(id).context(summon_i18n::text!("rdb8eb95e71"))?;
        let ack_path = self.handoff_ack_path(id);
        match std::fs::remove_file(&ack_path) {
            Ok(()) => {
                let _ = std::fs::File::open(self.handoff_ack_dir()).and_then(|dir| dir.sync_all());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| summon_i18n::format!("r677ef44074")),
        }
        let path = self.recall_resume_path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                let _ =
                    std::fs::File::open(self.recall_resume_dir()).and_then(|dir| dir.sync_all());
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| summon_i18n::format!("r2956c9c5f9")),
        }
    }

    /// 回收旧版本或异常崩溃留下的 ACK。只处理 summon 自己的 canonical UUID 文件名，且在
    /// 删除前重新确认对应 attempt 不存在；未知文件、目录和符号链接一律保留以避免越权删除。
    /// recall 全托管的 ACK 也落此共享目录、但归属 `recall_resume_dir` 而非 `handoff_attempt_dir`，
    /// 故两个所有者都不存在才算孤儿——否则会与 recall_resume_tick 读 ACK 竞态、误删活跃 ACK。
    pub fn prune_orphan_handoff_acks(&self) -> Result<u64> {
        self.ensure_layout()?;
        let mut removed = 0u64;
        for entry in
            std::fs::read_dir(self.handoff_ack_dir()).context(summon_i18n::text!("ra356ca583a"))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(attempt_id) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(parsed_id) = uuid::Uuid::parse_str(attempt_id) else {
                continue;
            };
            if parsed_id.to_string() != attempt_id {
                continue;
            }
            // 任一所有者存在(takeover 的 handoff_attempt 或 recall 的 recall_resume)都不算孤儿。
            let mut owned = false;
            for owner_path in [
                self.handoff_attempt_dir()
                    .join(format!("{attempt_id}.json")),
                self.recall_resume_path(attempt_id),
            ] {
                match std::fs::symlink_metadata(&owner_path) {
                    Ok(_) => {
                        owned = true;
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| format!("stat {owner_path:?}"));
                    }
                }
            }
            if owned {
                continue;
            }
            let ack_path = entry.path();
            match std::fs::remove_file(&ack_path) {
                Ok(()) => removed = removed.saturating_add(1),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| summon_i18n::format!("r677ef44074"))
                }
            }
        }
        if removed > 0 {
            let _ = std::fs::File::open(self.handoff_ack_dir()).and_then(|dir| dir.sync_all());
        }
        Ok(removed)
    }

    /// Handoff ACK 在进入通用 Hook spool 前单独 durable，避免 reducer 归档脱敏后丢失 token
    /// digest/staging 证明。内容只含 SessionStart 元数据，不含 prompt/assistant 正文。
    pub fn persist_handoff_ack(&self, envelope: &HookEnvelope) -> Result<()> {
        let ack = envelope
            .handoff_ack
            .as_ref()
            .context(summon_i18n::text!("r57a69c2d7e"))?;
        uuid::Uuid::parse_str(&ack.attempt_id).context(summon_i18n::text!("rffcec76ea5"))?;
        let incoming_digest =
            crate::bridge::handoff::HandoffTokenDigest::from_hex(&ack.token_sha256)
                .context(summon_i18n::text!("rd97213e225"))?;
        anyhow::ensure!(
            matches!(envelope.event, HookEvent::SessionStart { ref source, .. } if source == "resume")
                && envelope.content_permit.is_none(),
            summon_i18n::format!("r3e6c2fc3d0")
        );
        // 同一 token 理论上只有一个 SessionStart；仍用全局 state flock 串行化多 Hook writer，
        // 避免两个不同 envelope 同时越过 stale 检查后发生 last-writer-wins。
        let _lock = self.open_state_lock()?;
        // ResumePrepared 必须先 durable 才允许 launch staging。Hook 只能证明当前阶段的
        // capability；旧 staging 的迟到 Hook 即使 attempt id 相同也不得覆盖当前 ACK。
        let expected_digest = self.current_ack_digest(&ack.attempt_id)?;
        anyhow::ensure!(
            incoming_digest.constant_time_eq(&expected_digest),
            summon_i18n::format!("rb0f08f38fb")
        );
        let path = self.handoff_ack_path(&ack.attempt_id);
        if path.exists() {
            let current = self.read_hook(&path)?;
            if current == *envelope {
                return Ok(());
            }
            let current_ack = current
                .handoff_ack
                .as_ref()
                .context(summon_i18n::text!("reaecc78b22"))?;
            anyhow::ensure!(
                current_ack.attempt_id == ack.attempt_id,
                summon_i18n::format!("raa843a6c0a")
            );
            let current_digest =
                crate::bridge::handoff::HandoffTokenDigest::from_hex(&current_ack.token_sha256)
                    .context(summon_i18n::text!("re15fc62c4b"))?;
            anyhow::ensure!(
                !current_digest.constant_time_eq(&expected_digest),
                summon_i18n::format!("rce38f7868c")
            );
        }
        let content =
            serde_json::to_vec_pretty(envelope).context(summon_i18n::text!("rfd0be19add"))?;
        crate::runtime::fsio::atomic_write_mode(&path, &content, 0o600)
            .context(summon_i18n::text!("r136807c05d"))
    }

    fn current_handoff_ack_digest(
        &self,
        attempt_id: &str,
    ) -> Result<crate::bridge::handoff::HandoffTokenDigest> {
        let path = self
            .handoff_attempt_dir()
            .join(format!("{attempt_id}.json"));
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("stat handoff attempt {path:?}"))?;
        anyhow::ensure!(metadata.is_file(), summon_i18n::format!("r334c9f855b"));
        anyhow::ensure!(
            metadata.len() <= MAX_STATE_BYTES as u64,
            summon_i18n::format!("r3ed6ac5421")
        );
        let mut content = Vec::new();
        File::open(&path)
            .with_context(|| summon_i18n::format!("rfafba85921"))?
            .take(MAX_STATE_BYTES as u64 + 1)
            .read_to_end(&mut content)
            .with_context(|| summon_i18n::format!("r51dd30e68c"))?;
        anyhow::ensure!(
            content.len() <= MAX_STATE_BYTES,
            summon_i18n::format!("r516b7e19d2")
        );
        let attempt: crate::bridge::handoff::HandoffAttempt = serde_json::from_slice(&content)
            .with_context(|| summon_i18n::format!("rcf95d9cf6c"))?;
        anyhow::ensure!(
            attempt.attempt_id() == attempt_id,
            summon_i18n::format!("r3a639efdb7")
        );
        match attempt.phase() {
            crate::bridge::handoff::HandoffPhase::ResumePrepared { token_digest, .. }
            | crate::bridge::handoff::HandoffPhase::Resuming { token_digest, .. } => {
                Ok(token_digest.clone())
            }
            _ => anyhow::bail!(summon_i18n::format!("re933357015")),
        }
    }

    /// ACK 反陈旧摘要的统一入口(seam):同一 id 要么归 takeover 的 `handoff_attempt_dir`
    /// (原样,既有行为字节不变),要么归 recall 的 `recall_resume_dir`。先查前者、缺席才落后者,
    /// 保证 takeover 路径永远不经 recall 分支;两者都不存在时保留原 takeover 语义的错误
    /// (提及 "handoff attempt"),即拒绝无 durable attempt 的 ACK。
    fn current_ack_digest(&self, id: &str) -> Result<crate::bridge::handoff::HandoffTokenDigest> {
        let handoff_path = self.handoff_attempt_dir().join(format!("{id}.json"));
        match std::fs::symlink_metadata(&handoff_path) {
            Ok(_) => return self.current_handoff_ack_digest(id),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("stat {handoff_path:?}")),
        }
        let recall_path = self.recall_resume_path(id);
        match std::fs::symlink_metadata(&recall_path) {
            Ok(_) => self.recall_resume_ack_digest(id),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(summon_i18n::format!("red3b74740f"))
            }
            Err(error) => Err(error).with_context(|| format!("stat {recall_path:?}")),
        }
    }

    fn recall_resume_ack_digest(
        &self,
        id: &str,
    ) -> Result<crate::bridge::handoff::HandoffTokenDigest> {
        let path = self.recall_resume_path(id);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("stat recall_resume attempt {path:?}"))?;
        anyhow::ensure!(metadata.is_file(), summon_i18n::format!("rda5c70ba88"));
        anyhow::ensure!(
            metadata.len() <= MAX_STATE_BYTES as u64,
            summon_i18n::format!("ra3d82c49b8")
        );
        let mut content = Vec::new();
        File::open(&path)
            .with_context(|| summon_i18n::format!("ra5debcdce2"))?
            .take(MAX_STATE_BYTES as u64 + 1)
            .read_to_end(&mut content)
            .with_context(|| summon_i18n::format!("ra964e59684"))?;
        anyhow::ensure!(
            content.len() <= MAX_STATE_BYTES,
            summon_i18n::format!("r2a0003b593")
        );
        let attempt: crate::bridge::recall_resume::RecallResumeAttempt =
            serde_json::from_slice(&content)
                .with_context(|| summon_i18n::format!("ree62fa05a6"))?;
        anyhow::ensure!(attempt.id == id, summon_i18n::format!("r4baa251fed"));
        attempt
            .launched_token_digest()
            .cloned()
            .context(summon_i18n::text!("r1edd2658d6"))
    }

    pub fn load_handoff_ack(&self, attempt_id: &str) -> Result<Option<HookEnvelope>> {
        let path = self.handoff_ack_path(attempt_id);
        if !path
            .try_exists()
            .context(summon_i18n::text!("r70fbd7affd"))?
        {
            return Ok(None);
        }
        self.read_hook(&path).map(Some)
    }

    /// ACK 先单独 durable、再进入通用 spool；若进程恰在两步之间退出，coordinator 用此
    /// 只读检查决定是否补投。文件名中的 event id 已在 enqueue 边界校验，可精确匹配后缀。
    pub fn hook_event_is_pending(&self, event_id: &str) -> Result<bool> {
        anyhow::ensure!(
            !event_id.is_empty()
                && event_id.len() <= 256
                && event_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            summon_i18n::format!("r644ade820e")
        );
        let entries = match std::fs::read_dir(self.pending_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context(summon_i18n::text!("r7da6d4cf65")),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_name().to_str().is_some_and(|name| {
                name.len() == 20 + 1 + event_id.len() + 5
                    && name.as_bytes()[..20]
                        .iter()
                        .all(|byte| byte.is_ascii_digit())
                    && name.as_bytes()[20] == b'-'
                    && &name[21..name.len() - 5] == event_id
                    && name.ends_with(".json")
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 运维只读快照；不建目录、不拿写锁、不修复文件。state 原子 rename，故无锁读取只会看到
    /// 完整旧版或新版。损坏单独标记，避免 `bridge status` 把 fail-closed 状态误报成空闲。
    pub fn inspect_health(&self) -> Result<BridgeStoreHealth> {
        let hook_pending = count_json_files_read_only(&self.pending_dir())?;
        let hook_dead = count_json_files_read_only(&self.dead_dir())?;
        let Some(state) = (match self.load_state_unlocked() {
            Ok(state) => state,
            Err(_) => {
                return Ok(BridgeStoreHealth {
                    state: BridgeStateHealth::Corrupt,
                    sessions: 0,
                    active_sessions: 0,
                    conflicts: 0,
                    mirror_bindings: 0,
                    activating_bindings: 0,
                    handoff_sensitive_bindings: 0,
                    managed_takeovers: 0,
                    closing_bindings: 0,
                    pending_turns: 0,
                    delivery_receipts: 0,
                    hook_pending,
                    hook_dead,
                })
            }
        }) else {
            return Ok(BridgeStoreHealth {
                state: BridgeStateHealth::Absent,
                sessions: 0,
                active_sessions: 0,
                conflicts: 0,
                mirror_bindings: 0,
                activating_bindings: 0,
                handoff_sensitive_bindings: 0,
                managed_takeovers: 0,
                closing_bindings: 0,
                pending_turns: 0,
                delivery_receipts: 0,
                hook_pending,
                hook_dead,
            });
        };
        let active_sessions = state
            .sessions
            .values()
            .filter(|record| {
                matches!(
                    record.lifecycle,
                    summon_core::bridge::ExternalLifecycle::ActiveIdle
                        | summon_core::bridge::ExternalLifecycle::ActiveBusy
                )
            })
            .count();
        let conflicts = state
            .sessions
            .values()
            .filter(|record| record.lifecycle == summon_core::bridge::ExternalLifecycle::Conflict)
            .count();
        let mirror_bindings = state
            .slots
            .values()
            .filter(|slot| matches!(slot, ChatSlot::MirrorBound(_)))
            .count();
        let activating_bindings = state
            .slots
            .values()
            .filter(|slot| matches!(slot, ChatSlot::Activating(_)))
            .count();
        let handoff_sensitive_bindings = state
            .slots
            .values()
            .filter(|slot| {
                matches!(
                    slot,
                    ChatSlot::HandoffPending(_) | ChatSlot::ExternalOwned(_)
                )
            })
            .count();
        let closing_bindings = state
            .slots
            .values()
            .filter(|slot| matches!(slot, ChatSlot::Closing { .. }))
            .count();
        Ok(BridgeStoreHealth {
            state: BridgeStateHealth::Healthy,
            sessions: state.sessions.len(),
            active_sessions,
            conflicts,
            mirror_bindings,
            activating_bindings,
            handoff_sensitive_bindings,
            managed_takeovers: state.managed_takeovers.len(),
            closing_bindings,
            pending_turns: state.pending_mirror_turns.len(),
            delivery_receipts: state.activation_audit_receipts.len()
                + state.mirror_export_receipts.len()
                + state.context_export_receipts.len(),
            hook_pending,
            hook_dead,
        })
    }

    fn state_lock_path(&self) -> PathBuf {
        self.root.join(".state.lock")
    }

    fn event_sequence_path(&self) -> PathBuf {
        self.root.join(".event-sequence")
    }

    fn ensure_layout(&self) -> Result<()> {
        for directory in [
            self.root.clone(),
            self.root.join("events"),
            self.pending_dir(),
            self.archive_dir(),
            self.dead_dir(),
            self.root.join("handoff"),
            self.handoff_attempt_dir(),
            self.handoff_ack_dir(),
            self.recall_resume_dir(),
            self.recall_source_observation_dir(),
        ] {
            std::fs::create_dir_all(&directory)
                .with_context(|| summon_i18n::format!("red7b170bf0"))?;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .with_context(|| summon_i18n::format!("raff1645415"))?;
        }
        for file in [
            self.state_path(),
            self.state_lock_path(),
            self.event_sequence_path(),
        ] {
            if file.exists() {
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                    .with_context(|| summon_i18n::format!("r6d21548ec2"))?;
            }
        }
        Ok(())
    }

    fn open_state_lock(&self) -> Result<File> {
        self.ensure_layout()?;
        let path = self.state_lock_path();
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| summon_i18n::format!("raf90dba99d"))?;
        lock.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| summon_i18n::format!("r7ea5778ef2"))?;
        lock.lock().with_context(|| format!("flock {path:?}"))?;
        Ok(lock)
    }

    /// 状态不存在是正常冷启；坏 JSON 必须向上传递，调用方据此 fail-closed。
    pub fn load_state(&self) -> Result<Option<BridgeState>> {
        self.ensure_layout()?;
        self.load_state_unlocked()
    }

    /// drain 每 100ms 会在多个只读点各读一次 state.json(实测 200KB+),而绝大多数 tick
    /// state 根本没变。这里按盘上身份复验后复用上次的解析结果:身份不变才命中，因此进程外
    /// 写者(Claude Hook)的改动一定在下一次调用被看见。
    ///
    /// **只给纯只读路径用。** 写路径必须继续走 `with_state`,在 flock 内 load→mutate→save。
    pub fn load_state_cached(&self) -> Result<Option<Arc<BridgeState>>> {
        self.ensure_layout()?;
        let path = self.state_path();
        let observed = match std::fs::metadata(&path) {
            Ok(metadata) => Some(StateFileIdentity::from_metadata(&metadata)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).with_context(|| format!("stat {path:?}")),
        };
        let Some(observed) = observed else {
            *self.lock_state_cache() = None;
            return Ok(None);
        };
        if let Some(hit) = self
            .lock_state_cache()
            .as_ref()
            .filter(|cached| cached.identity == observed)
        {
            return Ok(Some(Arc::clone(&hit.state)));
        }
        // 身份取自读取所用的同一个 fd:即使刚才 stat 之后立刻发生 rename，缓存里的身份也
        // 一定描述我们真正读到的那份字节，不会给新内容盖旧身份的章。
        let Some((state, identity)) = self.load_state_with_identity()? else {
            *self.lock_state_cache() = None;
            return Ok(None);
        };
        let state = Arc::new(state);
        *self.lock_state_cache() = Some(CachedState {
            identity,
            state: Arc::clone(&state),
        });
        Ok(Some(state))
    }

    /// 缓存纯属加速且每条自带盘上身份，中毒的锁不会污染正确性，直接接管即可。
    fn lock_state_cache(&self) -> std::sync::MutexGuard<'_, Option<CachedState>> {
        self.state_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn load_state_unlocked(&self) -> Result<Option<BridgeState>> {
        Ok(self
            .load_state_with_identity()?
            .map(|(state, _identity)| state))
    }

    fn load_state_with_identity(&self) -> Result<Option<(BridgeState, StateFileIdentity)>> {
        let path = self.state_path();
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| summon_i18n::format!("rf45ace8440")),
        };
        let metadata = file.metadata().with_context(|| format!("stat {path:?}"))?;
        anyhow::ensure!(
            metadata.len() <= MAX_STATE_BYTES as u64,
            summon_i18n::format!("ra149ea5ac2", MAX_STATE_BYTES)
        );
        let mut content = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(MAX_STATE_BYTES as u64 + 1)
            .read_to_end(&mut content)
            .with_context(|| summon_i18n::format!("rf45ace8440"))?;
        anyhow::ensure!(
            content.len() <= MAX_STATE_BYTES,
            summon_i18n::format!("rf0a988b127")
        );
        let state: BridgeState = serde_json::from_slice(&content)
            .with_context(|| summon_i18n::format!("rb63e6009af"))?;
        anyhow::ensure!(
            state.schema_version == BRIDGE_SCHEMA_VERSION,
            summon_i18n::format!("r3fb317f1c3", state.schema_version)
        );
        Ok(Some((state, StateFileIdentity::from_metadata(&metadata))))
    }

    /// 所有 bridge state 写者共用这一把 flock，在锁内完成 load→mutate→atomic save。
    /// 闭包失败时不保存，损坏 state 也绝不按空状态覆盖。
    pub fn with_state<T>(&self, mutate: impl FnOnce(&mut BridgeState) -> Result<T>) -> Result<T> {
        let _lock = self.open_state_lock()?;
        let mut state = self.load_state_unlocked()?.unwrap_or_default();
        let result = mutate(&mut state)?;
        let content =
            serde_json::to_vec_pretty(&state).context(summon_i18n::text!("r025a921def"))?;
        anyhow::ensure!(
            content.len() <= MAX_STATE_BYTES,
            summon_i18n::format!("ra149ea5ac2", MAX_STATE_BYTES)
        );
        crate::runtime::fsio::atomic_write_mode(&self.state_path(), &content, 0o600)
            .context(summon_i18n::text!("rca168354ae"))?;
        Ok(result)
    }

    /// Hook 只有在已激活 Mirror 或已由 summon 接管时才能落正文。
    /// HandoffPending 仍在等待安全边界、源进程退出和 resume ACK，此时未完成最终群权限复核，
    /// 因此不得提前扩大内容落盘范围。
    /// 状态缺失/损坏由调用方当作 false，不让本地损坏扩大成隐私泄漏。
    /// 返回正文采样所绑定的确切 binding generation。若状态异常地出现同一 SID 多个终态 slot，
    /// fail-closed 而不是任取一个。
    #[cfg(test)]
    pub fn content_permit(&self, session: &SessionKey) -> Result<Option<ContentPermit>> {
        let Some(state) = self.load_state()? else {
            return Ok(None);
        };
        content_permit_from_state(&state, session)
    }

    /// Hook 热路径的正文授权。正常 epoch 必须与当前 process 完全一致；daemon 尚未归约 verified
    /// same-SID resume 时，只接受更早 immutable SessionStart(resume) 对同一新 process 的证明，
    /// 并签发 `generation + 1` grace permit。任何并发 SID、旧进程仍活或 probe 失败都不保存正文。
    pub fn content_permit_for_hook(
        &self,
        session: &SessionKey,
        transcript_path: &str,
        process: Option<&ProcessFingerprint>,
        profile: &ResumeProfile,
    ) -> Result<Option<ContentPermit>> {
        let Some(state) = self.load_state()? else {
            return Ok(None);
        };
        content_permit_for_hook_from_state(self, &state, session, transcript_path, process, profile)
    }

    /// 事件文件提交后永不修改。先完整写入同目录临时文件并 fsync，再 rename；event id 是随机
    /// UUID，故不同 Hook writer 不共享目标名。正文 permit 在同一 state flock 内最后复验；
    /// capture→enqueue 间若 stop/chat-gone 撤权，落盘前原地剥离正文。
    pub fn enqueue_hook(&self, envelope: &HookEnvelope) -> Result<PathBuf> {
        anyhow::ensure!(
            !envelope.event_id.is_empty()
                && envelope.event_id.len() <= 256
                && envelope
                    .event_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            summon_i18n::format!("r644ade820e")
        );
        let mut envelope = envelope.clone();
        let _lock = self.open_state_lock()?;
        if envelope.content_permit.is_some() {
            let current = self.load_state_unlocked().ok().flatten().and_then(|state| {
                content_permit_for_hook_from_state(
                    self,
                    &state,
                    &envelope.session,
                    &envelope.transcript_path,
                    envelope.process.as_ref(),
                    &envelope.resume_profile,
                )
                .ok()
                .flatten()
            });
            if envelope.content_permit != current {
                strip_hook_content(&mut envelope);
            }
        } else {
            // Store 是最后一道隐私边界；未来即使出现绕过 capture 的调用者，无 permit 也
            // 不能把 prompt/assistant/error 正文写入 spool。
            strip_hook_content(&mut envelope);
        }
        let json = serde_json::to_vec(&envelope).context(summon_i18n::text!("red44bf183e"))?;
        anyhow::ensure!(
            json.len() <= MAX_HOOK_EVENT_BYTES,
            summon_i18n::format!("rbeda4d1587", MAX_HOOK_EVENT_BYTES)
        );
        self.cleanup_stale_tmp_unlocked()?;
        let (pending_files, pending_bytes) = self.pending_usage_unlocked()?;
        anyhow::ensure!(
            quota_allows(pending_files, pending_bytes, json.len() as u64),
            summon_i18n::format!("r8226baaba0")
        );
        let dir = self.pending_dir();
        let sequence = self.next_event_sequence_unlocked()?;
        let final_path = dir.join(format!("{sequence:020}-{}.json", envelope.event_id));
        let tmp_path = dir.join(format!(
            ".{sequence:020}-{}.{}.tmp",
            envelope.event_id,
            std::process::id()
        ));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&tmp_path)
                .with_context(|| summon_i18n::format!("r67c92c64d8"))?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| summon_i18n::format!("r87f3f93ed2"))?;
            file.write_all(&json)
                .with_context(|| summon_i18n::format!("r57327d83c8"))?;
            file.sync_all()
                .with_context(|| format!("sync {tmp_path:?}"))?;
            anyhow::ensure!(!final_path.exists(), summon_i18n::format!("r45dc79d8f3"));
            std::fs::rename(&tmp_path, &final_path)
                .with_context(|| format!("rename {tmp_path:?} → {final_path:?}"))?;
            let _ = File::open(&dir).and_then(|d| d.sync_all());
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        result?;
        Ok(final_path)
    }

    fn pending_usage_unlocked(&self) -> Result<(u64, u64)> {
        let mut files = 0u64;
        let mut bytes = 0u64;
        for entry in
            std::fs::read_dir(self.pending_dir()).context(summon_i18n::text!("r5c9760abf0"))?
        {
            let entry = entry.context(summon_i18n::text!("r6cc089064b"))?;
            let metadata = entry
                .metadata()
                .context(summon_i18n::text!("rba91bd25d0"))?;
            if metadata.is_file() {
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(metadata.len());
            }
        }
        Ok((files, bytes))
    }

    fn cleanup_stale_tmp_unlocked(&self) -> Result<()> {
        for entry in
            std::fs::read_dir(self.pending_dir()).context(summon_i18n::text!("r9616678eb5"))?
        {
            let path = entry.context(summon_i18n::text!("rbbdf378290"))?.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.') && name.ends_with(".tmp"))
            {
                std::fs::remove_file(&path).with_context(|| summon_i18n::format!("r818c3c7423"))?;
            }
        }
        Ok(())
    }

    fn next_event_sequence_unlocked(&self) -> Result<u64> {
        let path = self.event_sequence_path();
        let current = match std::fs::read_to_string(&path) {
            Ok(raw) => {
                anyhow::ensure!(!raw.trim().is_empty(), summon_i18n::format!("r2bf394a733"));
                raw.trim()
                    .parse::<u64>()
                    .with_context(|| summon_i18n::format!("rb63e6009af"))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error).with_context(|| summon_i18n::format!("rf45ace8440")),
        };
        let next = current
            .checked_add(1)
            .context(summon_i18n::text!("rdbe892762b"))?;
        crate::runtime::fsio::atomic_write_mode(&path, next.to_string().as_bytes(), 0o600)
            .with_context(|| summon_i18n::format!("reda6565c38"))?;
        Ok(next)
    }

    pub fn pending_paths(&self) -> Result<Vec<PathBuf>> {
        self.ensure_layout()?;
        let mut paths = std::fs::read_dir(self.pending_dir())
            .context(summon_i18n::text!("r2650f0e481"))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        paths.sort();
        Ok(paths)
    }

    pub fn read_hook(&self, path: &Path) -> Result<HookEnvelope> {
        let metadata = std::fs::symlink_metadata(path).with_context(|| format!("stat {path:?}"))?;
        anyhow::ensure!(metadata.is_file(), summon_i18n::format!("r7017cc9a57"));
        anyhow::ensure!(
            metadata.len() <= MAX_HOOK_EVENT_BYTES as u64,
            summon_i18n::format!("r987a974ea7")
        );
        let mut content = Vec::new();
        File::open(path)
            .with_context(|| summon_i18n::format!("raf90dba99d"))?
            .take(MAX_HOOK_EVENT_BYTES as u64 + 1)
            .read_to_end(&mut content)
            .with_context(|| summon_i18n::format!("rf45ace8440"))?;
        anyhow::ensure!(
            content.len() <= MAX_HOOK_EVENT_BYTES,
            summon_i18n::format!("r7daf7b1790")
        );
        serde_json::from_slice(&content).with_context(|| summon_i18n::format!("rb63e6009af"))
    }

    pub fn event_sequence(path: &Path) -> Option<u64> {
        path.file_name()?.to_str()?.split_once('-')?.0.parse().ok()
    }

    pub fn archive_pending(
        &self,
        path: &Path,
        observation: &summon_core::bridge::ExternalHookObservation,
    ) -> Result<PathBuf> {
        self.terminalize_pending(path, &self.archive_dir(), Some(observation), "applied")
    }

    pub fn dead_letter(&self, path: &Path) -> Result<PathBuf> {
        self.terminalize_pending(path, &self.dead_dir(), None, "invalid")
    }

    /// Hook 正文只为 state 归约服务；archive/dead 只留元数据 tombstone。先原子写脱敏终态，
    /// 再删 plaintext pending，避免 rename 或 crash 把第二份完整 transcript 长期留盘。
    fn terminalize_pending(
        &self,
        path: &Path,
        destination: &Path,
        observation: Option<&summon_core::bridge::ExternalHookObservation>,
        outcome: &str,
    ) -> Result<PathBuf> {
        self.ensure_layout()?;
        anyhow::ensure!(
            path.parent() == Some(self.pending_dir().as_path()),
            summon_i18n::format!("rfcbde560d6")
        );
        let file_name = path
            .file_name()
            .context(summon_i18n::text!("r1c4c6c21d6"))?;
        let (event_id, observed_at_ms, event_type, target) = match observation {
            Some(observation) => (
                observation.event_id.clone(),
                Some(observation.observed_at_ms),
                Some(hook_event_type(&observation.event)),
                destination.join(file_name),
            ),
            None => match path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.split_once('-'))
                .map(|(_, event_id)| event_id)
                .filter(|event_id| !event_id.is_empty())
            {
                Some(event_id) => (
                    event_id.to_string(),
                    None,
                    None,
                    destination.join(file_name),
                ),
                None => {
                    // 文件名本身也不可信，不能拿原文作 dead 路径或 tombstone identity；双 FNV
                    // 只用于生成稳定不透明隔离键，不承担鉴权/加密语义。
                    let event_id = opaque_malformed_hook_id(file_name.as_bytes());
                    let target = destination.join(format!("{event_id}.json"));
                    (event_id, None, None, target)
                }
            },
        };
        anyhow::ensure!(!event_id.is_empty(), summon_i18n::format!("rd1324c45fe"));
        if target.exists() {
            // 只有严格可读且 event/outcome 完全一致的 tombstone 才能压过唯一 plaintext pending；
            // 损坏或伪造的同名文件必须 fail-closed 保留 pending，等待人工/下次恢复。
            anyhow::ensure!(
                valid_hook_terminal(&target, &event_id, outcome)?,
                summon_i18n::format!("r8a0dea1207")
            );
            std::fs::remove_file(path).with_context(|| summon_i18n::format!("r909e769698"))?;
            return Ok(target);
        }
        let record = serde_json::json!({
            "schema_version": HOOK_ENVELOPE_VERSION,
            "event_id": event_id,
            "observed_at_ms": observed_at_ms,
            "event_type": event_type,
            "outcome": outcome,
        });
        let content =
            serde_json::to_vec_pretty(&record).context(summon_i18n::text!("rd4d8e0f1ce"))?;
        crate::runtime::fsio::atomic_write_mode(&target, &content, 0o600)
            .with_context(|| summon_i18n::format!("rfb9f3c12e2"))?;
        // terminal 已 durable；删失败只会让下一 tick 再归约为 Duplicate 后重试删除，不会把
        // plaintext 移入长期 archive/dead。
        std::fs::remove_file(path).with_context(|| summon_i18n::format!("r12834147f6"))?;
        let max_files = if destination == self.archive_dir() {
            MAX_ARCHIVE_FILES
        } else {
            MAX_DEAD_FILES
        };
        self.prune_directory(destination, max_files)?;
        Ok(target)
    }

    fn prune_directory(&self, directory: &Path, max_files: usize) -> Result<()> {
        let mut files = std::fs::read_dir(directory)
            .with_context(|| summon_i18n::format!("rd6c984a2f9"))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        files.sort();
        let remove = files.len().saturating_sub(max_files);
        for path in files.into_iter().take(remove) {
            std::fs::remove_file(&path).with_context(|| summon_i18n::format!("rf685c116bf"))?;
        }
        Ok(())
    }
}

fn opaque_malformed_hook_id(bytes: &[u8]) -> String {
    fn fnv64(mut hash: u64, bytes: &[u8]) -> u64 {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
    format!(
        "malformed-{:016x}{:016x}",
        fnv64(0xcbf29ce484222325, bytes),
        fnv64(0x84222325cbf29ce4, bytes)
    )
}

fn count_json_files_read_only(directory: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).with_context(|| summon_i18n::format!("rd6c984a2f9")),
    };
    let mut count = 0usize;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json")
            && std::fs::symlink_metadata(&path)?.is_file()
        {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

fn content_permit_from_state(
    state: &BridgeState,
    session: &SessionKey,
) -> Result<Option<ContentPermit>> {
    let Some(record) = state.session(session) else {
        return Ok(None);
    };
    if !matches!(
        record.lifecycle,
        summon_core::bridge::ExternalLifecycle::ActiveIdle
            | summon_core::bridge::ExternalLifecycle::ActiveBusy
    ) || !record.locator.profile.process_verified
        || record.process.as_ref().is_none_or(|process| {
            !process.is_complete() || process.executable != record.locator.profile.claude_bin
        })
    {
        return Ok(None);
    }
    let mut found = None;
    for slot in state.slots.values() {
        match slot {
            ChatSlot::MirrorBound(binding) | ChatSlot::ExternalOwned(binding)
                if binding.session == *session =>
            {
                anyhow::ensure!(
                    state
                        .session_leases
                        .get(&session.storage_key())
                        .is_some_and(|lease| {
                            lease.binding_id == binding.id && lease.target == binding.target
                        }),
                    summon_i18n::format!("r5a8852b9cd")
                );
                anyhow::ensure!(found.is_none(), summon_i18n::format!("r6229395cc8"));
                found = Some(ContentPermit {
                    binding_id: binding.id.clone(),
                    binding_revision: binding.revision,
                    generation: record.generation,
                });
            }
            ChatSlot::Pending { .. }
            | ChatSlot::Activating(_)
            | ChatSlot::HandoffPending(_)
            | ChatSlot::MirrorBound(_)
            | ChatSlot::ExternalOwned(_)
            | ChatSlot::Closing { .. } => {}
        }
    }
    Ok(found)
}

fn content_permit_for_hook_from_state(
    store: &BridgeStore,
    state: &BridgeState,
    session: &SessionKey,
    transcript_path: &str,
    process: Option<&ProcessFingerprint>,
    profile: &ResumeProfile,
) -> Result<Option<ContentPermit>> {
    let Some(process) = process.filter(|process| process.is_complete()) else {
        return Ok(None);
    };
    let Some(record) = state.session(session) else {
        return Ok(None);
    };
    if record.process.as_ref() == Some(process) {
        return content_permit_from_state(state, session);
    }
    if !profile.process_verified
        || profile.config_dir != session.config_dir
        || profile.claude_bin != process.executable
        || record.process.as_ref().is_none_or(|old| !old.is_complete())
    {
        return Ok(None);
    }
    let old_retired = record.lifecycle == summon_core::bridge::ExternalLifecycle::Ended
        || matches!(
            record
                .process
                .as_ref()
                .map(crate::bridge::process_probe::probe_exact_process),
            Some(Ok(crate::bridge::process_probe::ExactProcessStatus::Gone))
                | Some(Ok(
                    crate::bridge::process_probe::ExactProcessStatus::PidReused(_)
                ))
        );
    if !old_retired {
        return Ok(None);
    }

    let mut matched_resume = false;
    for path in store.pending_paths()? {
        let Some(sequence) = BridgeStore::event_sequence(&path) else {
            return Ok(None);
        };
        let candidate = match store.read_hook(&path) {
            Ok(candidate) => candidate,
            Err(_) => return Ok(None),
        };
        if candidate.session != *session {
            continue;
        }
        // daemon 可能已归约过旧 SessionStart、却在 archive 前崩溃。pending 文件仍在不代表它
        // 是本次 resume 的证明；按 state 的 event-id/sequence 水位跳过，避免陈旧 resume 阻塞
        // 后续同 SID 的合法新进程。两者同时检查，是因为 processed_events 会按容量裁剪。
        if state.processed_events.contains_key(&candidate.event_id)
            || sequence <= record.last_applied_sequence
        {
            continue;
        }
        let HookEvent::SessionStart { source, .. } = &candidate.event else {
            continue;
        };
        // 同一 SID 尚未归约的 SessionStart 必须唯一指向当前 resume 进程。startup 或另一个
        // process 都表示身份存在歧义，宁可剥离正文也不能把首轮内容签给错误 generation。
        if source != "resume"
            || candidate.process.as_ref() != Some(process)
            || candidate.transcript_path != transcript_path
            || !candidate.resume_profile.process_verified
            || candidate.resume_profile.config_dir != session.config_dir
            || candidate.resume_profile.claude_bin != process.executable
        {
            return Ok(None);
        }
        matched_resume = true;
    }
    if !matched_resume {
        return Ok(None);
    }

    let generation = record
        .generation
        .checked_add(1)
        .context(summon_i18n::text!("r52263c307b"))?;
    let mut found = None;
    for slot in state.slots.values() {
        if let ChatSlot::MirrorBound(binding) = slot {
            if binding.session != *session {
                continue;
            }
            anyhow::ensure!(found.is_none(), summon_i18n::format!("r35995e197b"));
            anyhow::ensure!(
                state
                    .session_leases
                    .get(&session.storage_key())
                    .is_some_and(|lease| {
                        lease.binding_id == binding.id && lease.target == binding.target
                    }),
                summon_i18n::format!("r6d4f182fc6")
            );
            found = Some(ContentPermit {
                binding_id: binding.id.clone(),
                binding_revision: binding.revision,
                generation,
            });
        }
    }
    Ok(found)
}

fn strip_hook_content(envelope: &mut HookEnvelope) {
    envelope.content_permit = None;
    match &mut envelope.event {
        HookEvent::UserPromptSubmit { prompt, .. } => *prompt = None,
        HookEvent::Stop {
            last_assistant_message,
            ..
        } => *last_assistant_message = None,
        HookEvent::StopFailure {
            error,
            error_details,
            last_assistant_message,
            ..
        } => {
            *error = None;
            *error_details = None;
            *last_assistant_message = None;
        }
        HookEvent::SessionStart { .. }
        | HookEvent::CwdChanged { .. }
        | HookEvent::SessionEnd { .. } => {}
    }
}

fn valid_hook_terminal(path: &Path, event_id: &str, outcome: &str) -> Result<bool> {
    let metadata = std::fs::symlink_metadata(path).with_context(|| format!("stat {path:?}"))?;
    if !metadata.is_file() || metadata.len() > MAX_HOOK_EVENT_BYTES as u64 {
        return Ok(false);
    }
    let mut content = Vec::new();
    File::open(path)
        .with_context(|| summon_i18n::format!("raf90dba99d"))?
        .take(MAX_HOOK_EVENT_BYTES as u64 + 1)
        .read_to_end(&mut content)
        .with_context(|| summon_i18n::format!("rf45ace8440"))?;
    if content.len() > MAX_HOOK_EVENT_BYTES {
        return Ok(false);
    }
    let terminal: HookTerminal = match serde_json::from_slice(&content) {
        Ok(terminal) => terminal,
        Err(_) => return Ok(false),
    };
    Ok(terminal.schema_version == HOOK_ENVELOPE_VERSION
        && terminal.event_id == event_id
        && terminal.outcome == outcome)
}

fn hook_event_type(event: &summon_core::bridge::ExternalHookEvent) -> &'static str {
    use summon_core::bridge::ExternalHookEvent;
    match event {
        ExternalHookEvent::SessionStart { .. } => "session_start",
        ExternalHookEvent::CwdChanged { .. } => "cwd_changed",
        ExternalHookEvent::UserPromptSubmit { .. } => "user_prompt_submit",
        ExternalHookEvent::Stop { .. } => "stop",
        ExternalHookEvent::StopFailure { .. } => "stop_failure",
        ExternalHookEvent::SessionEnd { .. } => "session_end",
    }
}

fn quota_allows(existing_files: u64, existing_bytes: u64, incoming_bytes: u64) -> bool {
    existing_files < MAX_PENDING_FILES
        && existing_bytes
            .checked_add(incoming_bytes)
            .is_some_and(|total| total <= MAX_PENDING_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Arc;
    use summon_core::bridge::{
        BridgeMode, ChatBinding, ContentPermit, ExternalLifecycle, ExternalLocator,
        ExternalSessionRecord, ScopedChatKey, SessionLease,
    };

    fn envelope(id: &str) -> HookEnvelope {
        HookEnvelope {
            schema_version: HOOK_ENVELOPE_VERSION,
            event_id: id.into(),
            observed_at_ms: 10,
            session: SessionKey::new("/tmp/config", "sid"),
            transcript_path: "/tmp/config/projects/demo/sid.jsonl".into(),
            cwd: "/tmp/demo".into(),
            process: None,
            resume_profile: ResumeProfile::default(),
            content_permit: None,
            managed_takeover: false,
            handoff_ack: None,
            event: HookEvent::SessionStart {
                source: "startup".into(),
                model: None,
            },
        }
    }

    fn mode(path: impl AsRef<std::path::Path>) -> u32 {
        std::fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn health_inspection_is_read_only_and_marks_corrupt_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bridge");
        let store = BridgeStore::new(&root);
        let absent = store.inspect_health().unwrap();
        assert_eq!(absent.state, BridgeStateHealth::Absent);
        assert!(!root.exists(), "只读 status 不应创建运行目录");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("state.json"), b"{broken").unwrap();
        let corrupt = store.inspect_health().unwrap();
        assert_eq!(corrupt.state, BridgeStateHealth::Corrupt);
    }

    #[test]
    fn flocked_state_rmw_does_not_lose_concurrent_updates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(BridgeStore::new(dir.path().join("bridge")));
        let joins = (0..16)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    store
                        .with_state(|state| {
                            state.revision += 1;
                            Ok(())
                        })
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for join in joins {
            join.join().unwrap();
        }
        assert_eq!(store.load_state().unwrap().unwrap().revision, 16);
    }

    #[test]
    fn content_permit_is_exact_and_only_for_final_content_slots() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        let session = SessionKey::new("/tmp/config", "sid");
        let target = ScopedChatKey::new("app", "agent", "chat");
        let binding = ChatBinding {
            id: "binding".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: session.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "owner".into(),
            created_at_ms: 1,
            revision: 7,
        };
        store
            .with_state(|state| {
                let process = ProcessFingerprint {
                    pid: 42,
                    start_time: "start".into(),
                    executable: "/opt/claude".into(),
                };
                state.sessions.insert(
                    session.storage_key(),
                    ExternalSessionRecord {
                        key: session.clone(),
                        generation: 1,
                        locator: ExternalLocator {
                            key: session.clone(),
                            transcript_path: "/tmp/config/sid.jsonl".into(),
                            resume_lookup_cwd: "/tmp/project".into(),
                            current_cwd: "/tmp/project".into(),
                            profile: ResumeProfile {
                                claude_bin: process.executable.clone(),
                                home: "/tmp/home".into(),
                                config_dir: "/tmp/config".into(),
                                process_verified: true,
                                ..ResumeProfile::default()
                            },
                            revision: 1,
                        },
                        process: Some(process),
                        lifecycle: ExternalLifecycle::ActiveIdle,
                        first_seen_ms: 1,
                        last_seen_ms: 1,
                        last_applied_sequence: 1,
                        pending_prompt: None,
                    },
                );
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
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store.content_permit(&session).unwrap(),
            Some(ContentPermit {
                binding_id: "binding".into(),
                binding_revision: 7,
                generation: 1,
            })
        );
        assert_eq!(
            store
                .content_permit(&SessionKey::new("/tmp/config", "other"))
                .unwrap(),
            None
        );

        let permit = store.content_permit(&session).unwrap().unwrap();
        store
            .with_state(|state| {
                state.slots.remove(&target.storage_key());
                Ok(())
            })
            .unwrap();
        let mut revoked = envelope("revoked-before-enqueue");
        revoked.session = session;
        revoked.content_permit = Some(permit);
        revoked.event = HookEvent::UserPromptSubmit {
            prompt_id: Some("prompt".into()),
            prompt: Some("VERY_SECRET_REVOKED_BODY".into()),
        };
        let path = store.enqueue_hook(&revoked).unwrap();
        let saved = store.read_hook(&path).unwrap();
        assert!(saved.content_permit.is_none());
        assert!(matches!(
            saved.event,
            HookEvent::UserPromptSubmit { prompt: None, .. }
        ));
        assert!(!std::fs::read_to_string(path)
            .unwrap()
            .contains("VERY_SECRET_REVOKED_BODY"));
    }

    #[test]
    fn invalid_existing_hook_terminal_never_deletes_plaintext_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        let path = store.enqueue_hook(&envelope("event-terminal")).unwrap();
        let target = store.dead_dir().join(path.file_name().unwrap());
        std::fs::write(&target, b"{broken").unwrap();
        assert!(store.dead_letter(&path).is_err());
        assert!(path.exists());

        std::fs::write(
            &target,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": HOOK_ENVELOPE_VERSION,
                "event_id": "other",
                "outcome": "invalid",
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(store.dead_letter(&path).is_err());
        assert!(path.exists());

        std::fs::write(
            &target,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": HOOK_ENVELOPE_VERSION,
                "event_id": "event-terminal",
                "outcome": "invalid",
            }))
            .unwrap(),
        )
        .unwrap();
        store.dead_letter(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn bridge_layout_and_sensitive_files_have_explicit_private_modes() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.with_state(|_| Ok(())).unwrap();
        let event = store.enqueue_hook(&envelope("event-1")).unwrap();
        for directory in [
            store.root.clone(),
            store.root.join("events"),
            store.pending_dir(),
            store.archive_dir(),
            store.dead_dir(),
            store.root.join("handoff"),
            store.handoff_attempt_dir(),
            store.handoff_ack_dir(),
            store.recall_resume_dir(),
            store.recall_source_observation_dir(),
        ] {
            assert_eq!(mode(directory), 0o700);
        }
        for file in [
            store.state_path(),
            store.state_lock_path(),
            store.event_sequence_path(),
            event,
        ] {
            assert_eq!(mode(file), 0o600);
        }
    }

    #[test]
    fn recall_source_observation_round_trips_privately_and_rejects_identity_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        let observation = crate::bridge::recall::RecallSourceObservation::new(
            "sid-active",
            ProcessFingerprint {
                pid: 4242,
                start_time: "Fri Jul 10 23:24:07 2026".into(),
                executable: "/opt/claude".into(),
            },
            123,
        );
        store
            .persist_recall_source_observation(&observation)
            .unwrap();
        let path = store.recall_source_observation_path(&observation.id);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(
            store
                .load_recall_source_observation(&observation.id)
                .unwrap(),
            observation
        );

        let mut tampered: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        tampered["id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
        std::fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        assert!(store
            .load_recall_source_observation(&observation.id)
            .is_err());
    }

    #[test]
    fn orphan_handoff_ack_cleanup_preserves_live_and_unknown_entries() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.ensure_layout().unwrap();
        let live_id = uuid::Uuid::new_v4().to_string();
        let orphan_id = uuid::Uuid::new_v4().to_string();
        let symlink_id = uuid::Uuid::new_v4().to_string();
        let live_ack = store.handoff_ack_path(&live_id);
        let orphan_ack = store.handoff_ack_path(&orphan_id);
        let unknown_ack = store.handoff_ack_dir().join("unknown.json");
        let symlink_ack = store.handoff_ack_path(&symlink_id);
        let symlink_target = store.root.join("outside-ack-target");
        std::fs::write(&live_ack, b"live").unwrap();
        std::fs::write(&orphan_ack, b"orphan").unwrap();
        std::fs::write(&unknown_ack, b"unknown").unwrap();
        std::fs::write(&symlink_target, b"linked").unwrap();
        symlink(&symlink_target, &symlink_ack).unwrap();
        std::fs::write(
            store.handoff_attempt_dir().join(format!("{live_id}.json")),
            b"durable-attempt",
        )
        .unwrap();

        assert_eq!(store.prune_orphan_handoff_acks().unwrap(), 1);
        assert!(live_ack.exists());
        assert!(!orphan_ack.exists());
        assert!(unknown_ack.exists());
        assert!(std::fs::symlink_metadata(symlink_ack)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    fn recall_attempt(
        id: &str,
        token: &str,
        phase: crate::bridge::recall_resume::RecallResumePhase,
    ) -> crate::bridge::recall_resume::RecallResumeAttempt {
        crate::bridge::recall_resume::RecallResumeAttempt {
            schema_version: crate::bridge::recall_resume::RECALL_RESUME_SCHEMA_VERSION,
            id: id.into(),
            token_digest: crate::bridge::handoff::HandoffTokenDigest::from_hex(
                &crate::bridge::handoff::token_digest_hex(token),
            )
            .unwrap(),
            staging_name: format!("summon-bridge-handoff-{id}"),
            ownership_marker: "a".repeat(64),
            canonical_tmux: "soul-bot-gx".into(),
            target: summon_core::bridge::ScopedChatKey::new("app", "bot", "oc"),
            target_chat_type: "group".into(),
            actor_open_id: "ou".into(),
            program: "/usr/bin/claude".into(),
            args: Vec::new(),
            home: "/home/bot".into(),
            config_dir: "/home/bot/.claude".into(),
            provider: summon_core::bridge::ClaudeProvider::Default,
            sid: "sid".into(),
            transcript_path: "/home/bot/.claude/projects/d/sid.jsonl".into(),
            cwd: "/w".into(),
            created_at_ms: 1,
            ack_deadline_ms: 2,
            phase,
        }
    }

    #[test]
    fn ack_digest_reads_recall_resume_when_no_handoff_attempt() {
        use crate::bridge::recall_resume::RecallResumePhase;
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.ensure_layout().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let token = "b".repeat(64);
        store
            .persist_recall_resume_attempt(&recall_attempt(
                &id,
                &token,
                RecallResumePhase::Launched,
            ))
            .unwrap();
        let expected = crate::bridge::handoff::HandoffTokenDigest::from_hex(
            &crate::bridge::handoff::token_digest_hex(&token),
        )
        .unwrap();
        assert!(store
            .current_ack_digest(&id)
            .unwrap()
            .constant_time_eq(&expected));
    }

    #[test]
    fn ack_digest_rejects_recall_resume_outside_launched_phase() {
        use crate::bridge::recall_resume::RecallResumePhase;
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.ensure_layout().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .persist_recall_resume_attempt(&recall_attempt(
                &id,
                &"c".repeat(64),
                RecallResumePhase::Bound,
            ))
            .unwrap();
        assert!(
            store.current_ack_digest(&id).is_err(),
            "非 Launched 不接受 ACK"
        );
    }

    #[test]
    fn ack_digest_prefers_handoff_attempt_over_recall_resume() {
        use crate::bridge::recall_resume::RecallResumePhase;
        // 同 id 两个所有者都在:必须走 takeover 分支(此处 garbage → 解析报错),证明 recall
        // 泛化没有改变既有 takeover 摘要来源的优先级(handoff_attempt 恒优先)。
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.ensure_layout().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            store.handoff_attempt_dir().join(format!("{id}.json")),
            b"not-json",
        )
        .unwrap();
        store
            .persist_recall_resume_attempt(&recall_attempt(
                &id,
                &"d".repeat(64),
                RecallResumePhase::Launched,
            ))
            .unwrap();
        let error = format!("{:#}", store.current_ack_digest(&id).unwrap_err());
        assert!(
            error.contains("handoff attempt"),
            "应走 takeover 分支报错:{error}"
        );
    }

    #[test]
    fn ack_digest_errors_when_neither_owner_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.ensure_layout().unwrap();
        assert!(store
            .current_ack_digest(&uuid::Uuid::new_v4().to_string())
            .is_err());
    }

    #[test]
    fn orphan_prune_spares_recall_resume_owned_ack_but_still_reaps_true_orphan() {
        use crate::bridge::recall_resume::RecallResumePhase;
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.ensure_layout().unwrap();
        let recall_id = uuid::Uuid::new_v4().to_string();
        let orphan_id = uuid::Uuid::new_v4().to_string();
        std::fs::write(store.handoff_ack_path(&recall_id), b"ack").unwrap();
        std::fs::write(store.handoff_ack_path(&orphan_id), b"ack").unwrap();
        store
            .persist_recall_resume_attempt(&recall_attempt(
                &recall_id,
                &"e".repeat(64),
                RecallResumePhase::Launched,
            ))
            .unwrap();
        // 真孤儿(既无 handoff_attempt 也无 recall_resume)仍被回收;recall 拥有的 ACK 保留。
        assert_eq!(store.prune_orphan_handoff_acks().unwrap(), 1);
        assert!(store.handoff_ack_path(&recall_id).exists());
        assert!(!store.handoff_ack_path(&orphan_id).exists());
    }

    #[test]
    fn remove_terminal_recall_resume_deletes_ack_and_record_idempotently() {
        use crate::bridge::recall_resume::RecallResumePhase;
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.ensure_layout().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(store.handoff_ack_path(&id), b"ack").unwrap();
        store
            .persist_recall_resume_attempt(&recall_attempt(
                &id,
                &"f".repeat(64),
                RecallResumePhase::Bound,
            ))
            .unwrap();
        assert!(store.remove_terminal_recall_resume_attempt(&id).unwrap());
        assert!(!store.handoff_ack_path(&id).exists());
        assert!(!store.recall_resume_path(&id).exists());
        // 幂等:再删返回 false,不报错。
        assert!(!store.remove_terminal_recall_resume_attempt(&id).unwrap());
    }

    #[test]
    fn cached_state_reuses_parse_only_while_file_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        // 冷启没有 state 文件时不得缓存出一个假状态。
        assert!(store.load_state_cached().unwrap().is_none());
        store
            .with_state(|state| {
                state.revision = 3;
                Ok(())
            })
            .unwrap();

        let first = store.load_state_cached().unwrap().unwrap();
        let second = store.load_state_cached().unwrap().unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "文件未变时必须复用上次解析结果"
        );
        assert_eq!(second.revision, 3);

        store
            .with_state(|state| {
                state.revision = 4;
                Ok(())
            })
            .unwrap();
        let third = store.load_state_cached().unwrap().unwrap();
        assert!(!Arc::ptr_eq(&second, &third));
        assert_eq!(third.revision, 4);
    }

    #[test]
    fn cached_state_sees_out_of_process_writer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bridge");
        let store = BridgeStore::new(&root);
        store
            .with_state(|state| {
                state.revision = 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(store.load_state_cached().unwrap().unwrap().revision, 1);

        // Claude Hook 是独立进程,拿不到本进程的缓存;用另一个 store 实例模拟它的写入。
        let hook_writer = BridgeStore::new(&root);
        hook_writer
            .with_state(|state| {
                state.revision = 2;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store.load_state_cached().unwrap().unwrap().revision,
            2,
            "进程外写者改动后,下一次读必须看到新内容"
        );

        std::fs::remove_file(store.state_path()).unwrap();
        assert!(store.load_state_cached().unwrap().is_none());
    }

    #[test]
    fn cached_state_propagates_corrupt_state_instead_of_serving_stale() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store
            .with_state(|state| {
                state.revision = 9;
                Ok(())
            })
            .unwrap();
        assert_eq!(store.load_state_cached().unwrap().unwrap().revision, 9);
        crate::runtime::fsio::atomic_write_mode(&store.state_path(), b"{ not json", 0o600).unwrap();
        // 连续两次都必须报错:损坏不会因为「上一轮解析失败没刷新缓存」而退回旧快照。
        for _ in 0..2 {
            assert!(
                store.load_state_cached().is_err(),
                "state 损坏必须 fail-closed,不能拿旧缓存顶上"
            );
        }
    }

    #[test]
    fn pending_quota_is_fail_closed_at_file_and_byte_boundaries() {
        assert!(quota_allows(0, 0, 1));
        assert!(!quota_allows(MAX_PENDING_FILES, 0, 1));
        assert!(quota_allows(
            MAX_PENDING_FILES - 1,
            MAX_PENDING_BYTES - 1,
            1
        ));
        assert!(!quota_allows(MAX_PENDING_FILES - 1, MAX_PENDING_BYTES, 1));
        assert!(!quota_allows(MAX_PENDING_FILES - 1, u64::MAX, 1));
    }

    #[test]
    fn oversized_state_and_empty_sequence_fail_closed() {
        use std::os::unix::fs::{symlink, OpenOptionsExt};

        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.pending_paths().unwrap();
        let state = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(store.state_path())
            .unwrap();
        state.set_len(MAX_STATE_BYTES as u64 + 1).unwrap();
        assert!(store.load_state().is_err());
        std::fs::remove_file(store.state_path()).unwrap();
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(store.event_sequence_path())
            .unwrap();
        assert!(store.enqueue_hook(&envelope("event")).is_err());
        assert!(store.pending_paths().unwrap().is_empty());

        let oversized = store
            .pending_dir()
            .join("00000000000000000001-oversized.json");
        let event = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&oversized)
            .unwrap();
        event.set_len(MAX_HOOK_EVENT_BYTES as u64 + 1).unwrap();
        assert!(store.read_hook(&oversized).is_err());

        let target = store.root.join("outside.json");
        std::fs::write(&target, b"{}").unwrap();
        let linked = store.pending_dir().join("00000000000000000002-linked.json");
        symlink(&target, &linked).unwrap();
        assert!(store.read_hook(&linked).is_err());
    }

    #[test]
    fn enqueue_cleans_crash_left_tmp_only_while_holding_spool_lock() {
        use std::os::unix::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        store.pending_paths().unwrap();
        let stale = store.pending_dir().join(".stale.tmp");
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&stale)
            .unwrap();
        store.enqueue_hook(&envelope("event")).unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn handoff_ack_recovery_detects_exact_pending_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        assert!(!store.hook_event_is_pending("ack-event").unwrap());
        store.enqueue_hook(&envelope("ack-event")).unwrap();
        assert!(store.hook_event_is_pending("ack-event").unwrap());
        assert!(!store.hook_event_is_pending("ack").unwrap());
        assert!(store.hook_event_is_pending("../escape").is_err());
    }
}
