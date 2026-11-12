//! Strict external Claude handoff 的独立安全基础与 coordinator。
//!
//! Takeover 不复用会 cold-start 的通用 `Session` 启动路径。source identity、target agent
//! runtime、退出证明与 ACK 都在本模块逐层收窄后，才允许创建 staging tmux。

use crate::observe::log::log;
use crate::session::tmux::{create_handoff_staging, OwnedHandoffStaging, TmuxSessionIdentity};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use summon_core::bridge::{
    ClaudeConfigSource, ClaudeProvider, ExpectedResumeIdentity, ExternalLifecycle, ExternalLocator,
    HandoffEligibility, ProcessFingerprint, ResumeProfile, ScopedChatKey, SessionKey,
};
use summon_core::driver::CliDriver;
use summon_core::registry::{Agent, CaptureMode};

pub const HANDOFF_SCHEMA_VERSION: u32 = 1;
const STAGING_PREFIX: &str = "summon-bridge-handoff-";
const MAX_TRANSCRIPT_TAIL_BYTES: u64 = 64 * 1024;
const MIN_STABILITY: Duration = Duration::from_millis(500);
const SOURCE_TERMINATION_GRACE_MS: u64 = 15_000;
const RESUME_RETRY_DELAY_MS: u64 = 2_000;
const RESUME_LAUNCH_RETRY_DELAY_MS: u64 = 10_000;
const MAX_RESUME_ERROR_BYTES: usize = 2_048;
const TARGET_AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// `claude auth status` 冷启动在整机高负载时偶发超过时限,单次超时就判失败会让召回/接管在
/// 杀源之前白白失败。只有超时才再跑一次;未登录、坏 JSON、非零退出这类判定结果不重试。
const TARGET_AUTH_ATTEMPTS: u32 = 2;
const MAX_AUTH_STATUS_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffTargetProfile {
    agent_name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    provider: ClaudeProvider,
    system_prompt_path: PathBuf,
}

/// 目标 Bot 当前标准 Claude runtime 的短期证明。它每次 preflight 都重新生成，不绑定源版本，
/// 也不持久化凭据；source executable 只继续服务于 exact SIGTERM 身份证明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTargetRuntime {
    program: PathBuf,
    provider: ClaudeProvider,
    home: PathBuf,
    config: PathBuf,
}

impl HandoffTargetProfile {
    pub fn provider_mode(agent: &Agent) -> ClaudeProvider {
        let extra_env = agent.extra_env();
        summon_core::bridge::classify_claude_provider(
            extra_env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
    }

    pub fn agent_is_eligible(agent: &Agent) -> bool {
        let extra_env = agent.extra_env();
        let provider = summon_core::bridge::classify_claude_provider(
            extra_env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        agent.cli_kind() == "claude"
            && crate::remote_tools::integration::binding(agent).is_none()
            && agent.capture_mode() == CaptureMode::Transcript
            && agent.resume_enabled(summon_core::config::Config::load().resume_sessions)
            && agent
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.cli_cmd.as_ref())
                .is_none()
            && {
                let default = crate::runtime::chat_model::effective(agent, None);
                CliDriver::Claude
                    .resolve_launch(default.model(), default.effort())
                    .is_ok()
            }
            && extra_env
                .iter()
                .all(|(key, value)| target_extra_env_is_safe(key, value))
            && provider_env_is_structurally_supported(provider, |key| {
                extra_env
                    .iter()
                    .find(|(candidate, _)| candidate == key)
                    .map(|(_, value)| value.as_str())
                    .filter(|value| !value.is_empty())
            })
    }

    pub fn for_agent(
        agent: &Agent,
        chat_type: &str,
        chat_id: &str,
        target_tmux: &str,
    ) -> Result<Self> {
        if !Self::agent_is_eligible(agent) {
            bail!(summon_i18n::format!("r2abaca92e4"));
        }
        let driver = CliDriver::Claude;
        let effective = crate::runtime::chat_model::effective(
            agent,
            Some(&summon_core::registry::chat_tag(chat_type, chat_id)),
        );
        let mut args = driver
            .resolve_launch(effective.model(), effective.effort())
            .context(summon_i18n::text!("r8e65e48c4e"))?
            .args;
        args.extend(
            driver
                .interactive_permission_args()
                .iter()
                .map(|value| (*value).to_string()),
        );
        args.extend(["--name".into(), format!("summon:{target_tmux}")]);
        let extra_env = agent.extra_env();
        let provider = summon_core::bridge::classify_claude_provider(
            extra_env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        let mut env = std::collections::BTreeMap::new();
        env.extend(crate::runtime::daemon::essential_system_env());
        env.extend(extra_env);
        let (_, feishu) = crate::runtime::daemon::agent_feishu_isolation_tmux(agent);
        env.extend(feishu);
        env.insert("SUMMON_AGENT".into(), agent.name.clone());
        env.insert("SUMMON_CHAT_ID".into(), chat_id.to_string());
        for (key, value) in &env {
            validate_env_pair(key, value)?;
        }
        // PreToolUse 执行闸:与 `Session::augment_launch` 共用 `write_guard_settings` 注入
        // `--settings`,不另写一份 —— 少了它,经 `/bridge` 接管拉起的会话整条没有执行闸,外部发起人
        // 在那种会话里可以随意写文件、跑命令。
        //
        // 写失败只跳过注入、不 fail-closed:与交互路同一取舍(起不了会话比少一层闸更糟,
        // 而 owner 路径本来就放行)。
        //
        // PostCompact 按这条路径实际会启动的 claude 判:与启动时同一套解析(目标环境的 PATH、
        // 跳过脚本 wrapper),解析不出就不写。
        let program = env
            .get("PATH")
            .and_then(|path| resolve_standard_claude_in_path(path).ok());
        match crate::session::session::write_guard_settings(agent, target_tmux, program.as_deref())
        {
            Ok(path) => args.extend(["--settings".into(), path.to_string_lossy().into_owned()]),
            Err(error) => {
                crate::observe::log::log(&summon_i18n::format!("r5e73efe3f2", agent.name))
            }
        }
        let system_prompt_path = crate::runtime::daemon::prepare_chat_system_prompt_file(
            agent,
            chat_type,
            chat_id,
            target_tmux,
        )?;
        args.extend([
            "--append-system-prompt-file".into(),
            system_prompt_path.to_string_lossy().into_owned(),
        ]);

        Ok(Self {
            agent_name: agent.name.clone(),
            args,
            env: env.into_iter().collect(),
            provider,
            system_prompt_path,
        })
    }

    // pure-read accessor,供 recall_resume 兄弟模块复用同一份 launch identity(单一真相源,避免
    // profile/env 组装漂移)。**不参与 takeover 安全逻辑**——不碰 coordinator/状态机/证明类型。
    // args 内已含 `--name summon:<canonical>` 与 `--append-system-prompt-file`,env 含隔离飞书凭据。
    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    pub(crate) fn env(&self) -> &[(String, String)] {
        &self.env
    }
}

impl VerifiedTargetRuntime {
    /// 只证明目标 Bot 此刻能够启动标准 Claude；证明不持久化凭据，也不依赖源 Claude 版本。
    pub fn verify(target: &HandoffTargetProfile) -> Result<Self> {
        let home = target_env_value(target, "HOME")
            .map(PathBuf::from)
            .context(summon_i18n::text!("r5614430761"))?;
        let home = canonical_directory(&home, summon_i18n::text!("r8d814c7092"))?;
        let config = canonical_directory(&home.join(".claude"), summon_i18n::text!("r8c72dc0238"))?;
        let program = resolve_standard_claude(target)?;
        verify_target_provider_ready(target, &program, TARGET_AUTH_TIMEOUT)?;
        Ok(Self {
            program,
            provider: target.provider,
            home,
            config,
        })
    }

    // pure-read accessor,供 recall_resume 复用已复验的 program/home/config/provider。
    // **不参与 takeover 安全逻辑**——只暴露已算好的字段,不含任何逻辑。
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    pub(crate) fn config(&self) -> &Path {
        &self.config
    }

    pub(crate) fn provider(&self) -> ClaudeProvider {
        self.provider
    }
}

fn target_env_value<'a>(target: &'a HandoffTargetProfile, key: &str) -> Option<&'a str> {
    target
        .env
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

fn provider_env_is_structurally_supported<'a>(
    provider: ClaudeProvider,
    mut value: impl FnMut(&str) -> Option<&'a str>,
) -> bool {
    match provider {
        ClaudeProvider::Default => true,
        ClaudeProvider::Unknown => false,
        ClaudeProvider::Custom => {
            let cloud = [
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
                "CLAUDE_CODE_USE_FOUNDRY",
            ]
            .into_iter()
            .any(|key| value(key).is_some());
            let credential = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
                .into_iter()
                .any(|key| value(key).is_some());
            !cloud && credential
        }
    }
}

fn resolve_standard_claude(target: &HandoffTargetProfile) -> Result<PathBuf> {
    let path = target_env_value(target, "PATH").context(summon_i18n::text!("rbde913a981"))?;
    resolve_standard_claude_in_path(path)
}

/// `#!` 脚本 wrapper 永远做不了 exact 身份证明,故 PATH 扫描必须跳过它继续往后找。
///
/// why:ACK 里的 `claude_bin` 来自 hook 对**运行镜像**的观测(macOS `proc_pidpath` /
/// Linux `/proc/<pid>/exe`),`observe_exact_launch` 也按同一口径取 executable。脚本一 exec
/// 就把镜像换成解释器或它最终 exec 的真 claude,于是 `claude_bin == plan.program` 结构上
/// **不可能**成立。选中这种 wrapper 只会起一个注定 ACK 不过的 staging(真机上表现为
/// 「起了 staging → 几秒后身份不一致 → 清掉」)。跳过它不放松任何校验:被跳过的候选本来
/// 就 100% 会被拒,只是把拒绝提前到 preflight 并让日志说得清。
fn resolve_standard_claude_in_path(path: &str) -> Result<PathBuf> {
    let mut wrappers = Vec::new();
    for directory in std::env::split_paths(path).filter(|directory| directory.is_absolute()) {
        let candidate = directory.join("claude");
        let Ok(program) = canonical_executable(&candidate) else {
            continue;
        };
        if runs_as_its_own_image(&program) {
            return Ok(program);
        }
        wrappers.push(program.to_string_lossy().into_owned());
    }
    if wrappers.is_empty() {
        bail!(summon_i18n::format!("r6dbf66446f"))
    }
    bail!(summon_i18n::format!("rb6da81b0d6", wrappers.join(", ")))
}

/// 该文件被 exec 后进程镜像是否仍是它自己(即不是 `#!` 脚本)。读不到按「不是」处理:
/// 证明不了就不选,与本模块其它证明一致 fail-closed。
fn runs_as_its_own_image(program: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(program) else {
        return false;
    };
    let mut magic = [0_u8; 2];
    match file.read(&mut magic) {
        // 不足 2 字节的文件不可能是 `#!` 脚本;能不能跑交给 exec 自己判。
        Ok(read) => read < 2 || magic != *b"#!",
        Err(_) => false,
    }
}

fn verify_target_provider_ready(
    target: &HandoffTargetProfile,
    program: &Path,
    timeout: Duration,
) -> Result<()> {
    if !provider_env_is_structurally_supported(target.provider, |key| target_env_value(target, key))
    {
        bail!(summon_i18n::format!("r7c1b595c55"));
    }
    match target.provider {
        ClaudeProvider::Custom => Ok(()),
        ClaudeProvider::Default => verify_default_claude_auth(target, program, timeout),
        ClaudeProvider::Unknown => bail!(summon_i18n::format!("r0a311b6d06")),
    }
}

fn verify_default_claude_auth(
    target: &HandoffTargetProfile,
    program: &Path,
    timeout: Duration,
) -> Result<()> {
    judge_claude_auth_status(|| run_claude_auth_status(target, program, timeout))
}

/// 重试策略与判定:`run` 跑一次 `claude auth status`,超时返回 `None`。
fn judge_claude_auth_status(
    mut run: impl FnMut() -> Result<Option<(std::process::ExitStatus, Vec<u8>)>>,
) -> Result<()> {
    let mut attempt = 1;
    let (status, bytes) = loop {
        match run()? {
            Some(finished) => break finished,
            None if attempt < TARGET_AUTH_ATTEMPTS => attempt += 1,
            None => bail!(summon_i18n::format!("rb4f7fe2ad5")),
        }
    };
    if !status.success() || bytes.len() as u64 > MAX_AUTH_STATUS_BYTES {
        bail!(summon_i18n::format!("rab5b60c6b7"));
    }
    #[derive(Deserialize)]
    struct AuthStatus {
        #[serde(rename = "loggedIn")]
        logged_in: bool,
    }
    let status: AuthStatus =
        serde_json::from_slice(&bytes).context(summon_i18n::text!("rc8e43a801e"))?;
    if !status.logged_in {
        bail!(summon_i18n::format!("rfd65c2b0ab"));
    }
    Ok(())
}

/// 跑一次 `claude auth status --json`。超时返回 `None`(子进程已收掉),跑完返回退出状态与
/// 截断到 `MAX_AUTH_STATUS_BYTES + 1` 的输出。
fn run_claude_auth_status(
    target: &HandoffTargetProfile,
    program: &Path,
    timeout: Duration,
) -> Result<Option<(std::process::ExitStatus, Vec<u8>)>> {
    let mut output = tempfile::tempfile().context(summon_i18n::text!("rc177669ab1"))?;
    let output_writer = output
        .try_clone()
        .context(summon_i18n::text!("r0b43f669c2"))?;
    let mut child = Command::new(program)
        .args(["auth", "status", "--json"])
        .env_clear()
        .envs(target.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_writer))
        .stderr(Stdio::null())
        .spawn()
        .context(summon_i18n::text!("r273cafa7f3"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context(summon_i18n::text!("r0586de65fd"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            // 这里只终止本次预检创建的精确子进程；不涉及 tmux 或外部 TTY。
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(25));
    };
    output
        .seek(SeekFrom::Start(0))
        .context(summon_i18n::text!("rec96bc6b09"))?;
    let mut bytes = Vec::new();
    output
        .take(MAX_AUTH_STATUS_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .context(summon_i18n::text!("ref98fc92a2"))?;
    Ok(Some((status, bytes)))
}

#[derive(Clone, PartialEq, Eq)]
pub struct HandoffToken(String);

impl HandoffToken {
    fn generate() -> Self {
        let raw =
            uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
        Self::parse(&raw).expect("UUID pair is a valid handoff token")
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn digest(&self) -> HandoffTokenDigest {
        let digest = ring::digest::digest(&ring::digest::SHA256, self.0.as_bytes());
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(digest.as_ref());
        HandoffTokenDigest(bytes)
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!(summon_i18n::format!("r18091985b9"));
        }
        Ok(Self(value.to_string()))
    }
}

impl fmt::Debug for HandoffToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HandoffToken([redacted])")
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffTokenDigest([u8; 32]);

impl HandoffTokenDigest {
    pub(crate) fn constant_time_eq(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
    }

    pub fn from_hex(value: &str) -> Result<Self> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!(summon_i18n::format!("ra5032c48a5"));
        }
        let mut bytes = [0u8; 32];
        for (index, chunk) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let text = std::str::from_utf8(chunk).context(summon_i18n::text!("r4bd9607a2f"))?;
            bytes[index] =
                u8::from_str_radix(text, 16).context(summon_i18n::text!("ra1b6466d3c"))?;
        }
        Ok(Self(bytes))
    }
}

pub(crate) fn token_digest_hex(token: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl fmt::Debug for HandoffTokenDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HandoffTokenDigest([redacted])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffSourceSnapshot {
    session: SessionKey,
    generation: u64,
    locator: ExternalLocator,
    process: ProcessFingerprint,
    profile: ResumeProfile,
    /// 只在 locator/path/恢复语义/process 身份变化时递增；不能复用每个 Hook 都刷新的
    /// `ExternalLocator::revision`。Claude 版本是诊断信息，不参与此身份。
    locator_identity_revision: u64,
}

impl HandoffSourceSnapshot {
    pub fn new(
        session: SessionKey,
        generation: u64,
        locator: ExternalLocator,
        process: ProcessFingerprint,
        profile: ResumeProfile,
        locator_identity_revision: u64,
    ) -> Result<Self> {
        if generation == 0
            || !process.is_complete()
            || locator.key != session
            || profile != locator.profile
            || profile.handoff_eligibility(Some(&process)) != HandoffEligibility::Eligible
            || locator_identity_revision == 0
        {
            bail!(summon_i18n::format!("r1ad2f83e7b"));
        }
        Ok(Self {
            session,
            generation,
            locator,
            process,
            profile,
            locator_identity_revision,
        })
    }

    pub fn session(&self) -> &SessionKey {
        &self.session
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn locator(&self) -> &ExternalLocator {
        &self.locator
    }

    pub fn process(&self) -> &ProcessFingerprint {
        &self.process
    }

    pub fn profile(&self) -> &ResumeProfile {
        &self.profile
    }
}

pub fn locator_identity_revision(locator: &ExternalLocator, process: &ProcessFingerprint) -> u64 {
    let config_source = match locator.profile.config_source {
        ClaudeConfigSource::Unknown => "unknown",
        ClaudeConfigSource::Default => "default",
        ClaudeConfigSource::Explicit => "explicit",
    };
    let provider = match locator.profile.provider {
        ClaudeProvider::Unknown => "unknown",
        ClaudeProvider::Default => "default",
        ClaudeProvider::Custom => "custom",
    };
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            locator.key.storage_key(),
            locator.transcript_path,
            locator.resume_lookup_cwd,
            locator.profile.claude_bin,
            locator.profile.home,
            locator.profile.config_dir,
            config_source,
            provider,
            process.executable,
        )
        .as_bytes(),
    );
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest.as_ref()[..8]);
    u64::from_be_bytes(bytes).max(1)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffAttempt {
    schema_version: u32,
    attempt_id: String,
    ticket_id: String,
    binding_id: String,
    binding_revision: u64,
    target: ScopedChatKey,
    target_chat_type: String,
    target_tmux: String,
    #[serde(default)]
    target_provider: ClaudeProvider,
    source: HandoffSourceSnapshot,
    #[serde(default)]
    created_at_ms: u64,
    phase: HandoffPhase,
}

/// daemon 可能在 staging 启动后、ACK 前重启，因此必须持久化本次恢复真正使用的无敏感
/// launch identity。不能在 ACK 时用“当前 agent 配置”反推，也不能回退到源 runtime。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeLaunchIdentity {
    program: String,
    args: Vec<String>,
    home: String,
    config_dir: String,
    provider: ClaudeProvider,
}

impl ResumeLaunchIdentity {
    fn is_complete(&self) -> bool {
        !self.program.is_empty()
            && !self.args.is_empty()
            && !self.home.is_empty()
            && !self.config_dir.is_empty()
            && self.provider != ClaudeProvider::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HandoffPhase {
    /// 用户已经在飞书明确授权远程接管；忙碌时只等待本轮 Stop，不触碰源进程。
    AwaitingSafeBoundary,
    /// 终止意图先持久化，再做进程信号副作用；daemon 崩溃后可安全重试。
    SourceTerminationRequested {
        requested_at_ms: u64,
    },
    /// `None/None` 只用于兼容旧版“等待用户手工退出”的 attempt；新远程接管必带信号时间与
    /// 截止时间。截止后只失败并释放 reservation，绝不升级为 SIGKILL。
    AwaitingSourceExit {
        #[serde(default)]
        signal_sent_at_ms: Option<u64>,
        #[serde(default)]
        deadline_ms: Option<u64>,
    },
    SourceExitProved {
        proof: SourceExitProof,
        /// 源 writer 已确认退出后，恢复准备错误只能保留并重试；不能销毁唯一的接管证据。
        #[serde(default)]
        last_resume_error: Option<String>,
        #[serde(default)]
        retry_at_ms: u64,
    },
    ResumePrepared {
        proof: SourceExitProof,
        staging_name: String,
        ownership_marker: String,
        token_digest: HandoffTokenDigest,
        ack_deadline_ms: u64,
        #[serde(default)]
        launch: ResumeLaunchIdentity,
    },
    Resuming {
        proof: SourceExitProof,
        staging: TmuxSessionIdentity,
        token_digest: HandoffTokenDigest,
        ack_deadline_ms: u64,
        #[serde(default)]
        launch: ResumeLaunchIdentity,
    },
    Acked {
        proof: HandoffAckProof,
    },
    RenamedAwaitingAudit {
        proof: HandoffAckProof,
        target: TmuxSessionIdentity,
    },
    CleanupBlocked {
        reason: String,
    },
    Failed,
    Completed,
}

impl HandoffAttempt {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        attempt_id: String,
        ticket_id: String,
        binding_id: String,
        binding_revision: u64,
        target: ScopedChatKey,
        target_chat_type: String,
        target_tmux: String,
        target_provider: ClaudeProvider,
        source: HandoffSourceSnapshot,
        created_at_ms: u64,
    ) -> Result<Self> {
        uuid::Uuid::parse_str(&attempt_id).context(summon_i18n::text!("r2bfda8a274"))?;
        if ticket_id.is_empty()
            || binding_id.is_empty()
            || binding_revision == 0
            || !matches!(target_chat_type.as_str(), "group" | "topic_group")
            || !valid_tmux_name(&target_tmux)
            || target_tmux.starts_with(STAGING_PREFIX)
            || target_provider == ClaudeProvider::Unknown
            || created_at_ms == 0
        {
            bail!(summon_i18n::format!("r9160af28b7"));
        }
        Ok(Self {
            schema_version: HANDOFF_SCHEMA_VERSION,
            attempt_id,
            ticket_id,
            binding_id,
            binding_revision,
            target,
            target_chat_type,
            target_tmux,
            target_provider,
            source,
            created_at_ms,
            phase: HandoffPhase::AwaitingSafeBoundary,
        })
    }

    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub fn ticket_id(&self) -> &str {
        &self.ticket_id
    }

    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }

    pub fn binding_revision(&self) -> u64 {
        self.binding_revision
    }

    pub fn target(&self) -> &ScopedChatKey {
        &self.target
    }

    pub fn target_tmux(&self) -> &str {
        &self.target_tmux
    }

    pub fn target_chat_type(&self) -> &str {
        &self.target_chat_type
    }

    pub fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    pub fn source(&self) -> &HandoffSourceSnapshot {
        &self.source
    }

    pub fn phase(&self) -> &HandoffPhase {
        &self.phase
    }

    pub fn mark_termination_requested(&mut self, requested_at_ms: u64) -> Result<()> {
        if requested_at_ms == 0 || !matches!(self.phase, HandoffPhase::AwaitingSafeBoundary) {
            bail!(summon_i18n::format!("r35096c784c"));
        }
        self.phase = HandoffPhase::SourceTerminationRequested { requested_at_ms };
        Ok(())
    }

    pub fn mark_source_signal_sent(
        &mut self,
        signal_sent_at_ms: u64,
        deadline_ms: u64,
    ) -> Result<()> {
        if signal_sent_at_ms == 0
            || deadline_ms <= signal_sent_at_ms
            || !matches!(self.phase, HandoffPhase::SourceTerminationRequested { .. })
        {
            bail!(summon_i18n::format!("r14622dbe9a"));
        }
        self.phase = HandoffPhase::AwaitingSourceExit {
            signal_sent_at_ms: Some(signal_sent_at_ms),
            deadline_ms: Some(deadline_ms),
        };
        Ok(())
    }

    pub fn mark_source_exit(&mut self, proof: SourceExitProof) -> Result<()> {
        if proof.attempt_id != self.attempt_id
            || !matches!(
                self.phase,
                HandoffPhase::AwaitingSafeBoundary
                    | HandoffPhase::SourceTerminationRequested { .. }
                    | HandoffPhase::AwaitingSourceExit { .. }
            )
        {
            bail!(summon_i18n::format!("r8567ac4266"));
        }
        self.phase = HandoffPhase::SourceExitProved {
            proof,
            last_resume_error: None,
            retry_at_ms: 0,
        };
        Ok(())
    }

    fn defer_resume(&mut self, error: &anyhow::Error, retry_at_ms: u64) -> Result<()> {
        let HandoffPhase::SourceExitProved {
            last_resume_error,
            retry_at_ms: current_retry_at_ms,
            ..
        } = &mut self.phase
        else {
            bail!(summon_i18n::format!("rdce4fd867c"));
        };
        let mut reason = format!("{error:#}");
        if reason.len() > MAX_RESUME_ERROR_BYTES {
            let mut boundary = MAX_RESUME_ERROR_BYTES;
            while !reason.is_char_boundary(boundary) {
                boundary -= 1;
            }
            reason.truncate(boundary);
        }
        *last_resume_error = Some(reason);
        *current_retry_at_ms = retry_at_ms.max(1);
        Ok(())
    }

    pub fn mark_resuming(
        &mut self,
        proof: SourceExitProof,
        staging: TmuxSessionIdentity,
        token_digest: HandoffTokenDigest,
        ack_deadline_ms: u64,
    ) -> Result<()> {
        let launch = match &self.phase {
            HandoffPhase::ResumePrepared {
                proof: prepared,
                staging_name,
                ownership_marker,
                token_digest: prepared_digest,
                ack_deadline_ms: prepared_deadline,
                launch,
            } if prepared == &proof
                && staging_name == &staging.name
                && ownership_marker == &staging.ownership_marker
                && prepared_digest == &token_digest
                && prepared_deadline == &ack_deadline_ms
                && launch.is_complete() =>
            {
                launch.clone()
            }
            _ => bail!(summon_i18n::format!("r9c2e01f307")),
        };
        anyhow::ensure!(
            proof.attempt_id == self.attempt_id && ack_deadline_ms != 0,
            summon_i18n::format!("r9c2e01f307")
        );
        self.phase = HandoffPhase::Resuming {
            proof,
            staging,
            token_digest,
            ack_deadline_ms,
            launch,
        };
        Ok(())
    }

    pub fn mark_resume_prepared(
        &mut self,
        proof: SourceExitProof,
        staging_name: String,
        ownership_marker: String,
        token_digest: HandoffTokenDigest,
        ack_deadline_ms: u64,
        launch: ResumeLaunchIdentity,
    ) -> Result<()> {
        if proof.attempt_id != self.attempt_id
            || !matches!(self.phase, HandoffPhase::SourceExitProved { .. })
            || !valid_tmux_name(&staging_name)
            || ownership_marker.is_empty()
            || ack_deadline_ms == 0
            || !launch.is_complete()
        {
            bail!(summon_i18n::format!("r9137cc340e"));
        }
        self.phase = HandoffPhase::ResumePrepared {
            proof,
            staging_name,
            ownership_marker,
            token_digest,
            ack_deadline_ms,
            launch,
        };
        Ok(())
    }

    pub fn mark_acked(&mut self, proof: HandoffAckProof) -> Result<()> {
        if proof.attempt_id != self.attempt_id
            || !matches!(self.phase, HandoffPhase::Resuming { .. })
        {
            bail!(summon_i18n::format!("rc9c2a73998"));
        }
        self.phase = HandoffPhase::Acked { proof };
        Ok(())
    }

    pub fn mark_renamed(
        &mut self,
        proof: HandoffAckProof,
        target: TmuxSessionIdentity,
    ) -> Result<()> {
        if proof.attempt_id != self.attempt_id || !matches!(self.phase, HandoffPhase::Acked { .. })
        {
            bail!(summon_i18n::format!("r21739346ff"));
        }
        self.phase = HandoffPhase::RenamedAwaitingAudit { proof, target };
        Ok(())
    }

    pub fn mark_completed(&mut self) -> Result<()> {
        if !matches!(self.phase, HandoffPhase::RenamedAwaitingAudit { .. }) {
            bail!(summon_i18n::format!("r43d2c59b39"));
        }
        self.phase = HandoffPhase::Completed;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceTerminationEvidence {
    SessionEnd { event_id: String, sequence: u64 },
    ProcessDisappeared,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessCheck {
    AliveExact,
    Gone,
    PidReused(ProcessFingerprint),
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTakeoverReadiness {
    WaitForSafeBoundary,
    RequestTermination,
    ProveSourceExit,
}

/// 远程接管只把 `Stop` 后的 Idle/Ended 当作安全终止边界。源进程已经消失时可直接进入稳定性
/// 证明；PID reuse/不可读都不能被解释成“已经退出”。
pub fn remote_takeover_readiness(
    lifecycle: ExternalLifecycle,
    process: ProcessCheck,
) -> Result<RemoteTakeoverReadiness> {
    match process {
        ProcessCheck::Gone => return Ok(RemoteTakeoverReadiness::ProveSourceExit),
        ProcessCheck::PidReused(_) => bail!(summon_i18n::format!("r4ab685cb4a")),
        ProcessCheck::Unreadable => bail!(summon_i18n::format!("r07da37e6e8")),
        ProcessCheck::AliveExact => {}
    }
    match lifecycle {
        ExternalLifecycle::ActiveBusy => Ok(RemoteTakeoverReadiness::WaitForSafeBoundary),
        ExternalLifecycle::ActiveIdle | ExternalLifecycle::Ended => {
            Ok(RemoteTakeoverReadiness::RequestTermination)
        }
        ExternalLifecycle::Conflict | ExternalLifecycle::Unverified => {
            bail!(summon_i18n::format!("r09cdf13c36"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptSnapshot {
    canonical_path: String,
    device: u64,
    inode: u64,
    len: u64,
    modified_ns: i64,
    last_line_len: u64,
    observed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSample {
    snapshot: TranscriptSnapshot,
    last_line: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableTranscriptProof {
    snapshot: TranscriptSnapshot,
    stability_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceExitEvidence {
    pub process_before: ProcessCheck,
    pub process_after: ProcessCheck,
    pub locator_identity_revision_before: u64,
    pub locator_identity_revision_after: u64,
    pub termination: SourceTerminationEvidence,
    pub first: TranscriptSample,
    pub second: TranscriptSample,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceExitProof {
    attempt_id: String,
    session: SessionKey,
    source_generation: u64,
    source_process: ProcessFingerprint,
    locator_identity_revision: u64,
    termination: SourceTerminationEvidence,
    transcript: StableTranscriptProof,
}

pub fn verify_source_exit(
    attempt: &HandoffAttempt,
    evidence: SourceExitEvidence,
) -> Result<SourceExitProof> {
    if !matches!(
        attempt.phase,
        HandoffPhase::AwaitingSafeBoundary
            | HandoffPhase::SourceTerminationRequested { .. }
            | HandoffPhase::AwaitingSourceExit { .. }
    ) {
        bail!(summon_i18n::format!("r61b23980d7"));
    }
    if evidence.process_before != ProcessCheck::Gone || evidence.process_after != ProcessCheck::Gone
    {
        bail!(summon_i18n::format!("rb5cbcc7251"));
    }
    if evidence.locator_identity_revision_before != attempt.source.locator_identity_revision
        || evidence.locator_identity_revision_after != evidence.locator_identity_revision_before
    {
        bail!(summon_i18n::format!("rbed6a8e1f8"));
    }
    if evidence.elapsed < MIN_STABILITY {
        bail!(summon_i18n::format!("rb060ef174c"));
    }
    if !stable_snapshot_equal(&evidence.first.snapshot, &evidence.second.snapshot)
        || evidence.first.last_line != evidence.second.last_line
    {
        bail!(summon_i18n::format!("rc07dd138b1"));
    }
    if evidence.first.snapshot.canonical_path != attempt.source.locator.transcript_path {
        bail!(summon_i18n::format!("r4e6a3b81bf"));
    }
    if let SourceTerminationEvidence::SessionEnd { event_id, sequence } = &evidence.termination {
        if event_id.is_empty() || *sequence == 0 {
            bail!(summon_i18n::format!("ra587b526cd"));
        }
    }
    Ok(SourceExitProof {
        attempt_id: attempt.attempt_id.clone(),
        session: attempt.source.session.clone(),
        source_generation: attempt.source.generation,
        source_process: attempt.source.process.clone(),
        locator_identity_revision: evidence.locator_identity_revision_after,
        termination: evidence.termination,
        transcript: StableTranscriptProof {
            snapshot: evidence.second.snapshot,
            stability_ms: evidence.elapsed.as_millis().try_into().unwrap_or(u64::MAX),
        },
    })
}

#[derive(Clone, PartialEq, Eq)]
struct StrictResumePreflight {
    program: PathBuf,
    home: PathBuf,
    config: PathBuf,
    provider: ClaudeProvider,
    work_dir: PathBuf,
    transcript_path: PathBuf,
    target: HandoffTargetProfile,
}

impl StrictResumePreflight {
    fn build(
        runtime: &VerifiedTargetRuntime,
        attempt: &HandoffAttempt,
        target: &HandoffTargetProfile,
    ) -> Result<Self> {
        if runtime.provider != target.provider || runtime.provider == ClaudeProvider::Unknown {
            bail!(summon_i18n::format!("r8d69051933"));
        }
        let program = canonical_executable(&runtime.program)?;
        let home = canonical_directory(&runtime.home, summon_i18n::text!("r8d814c7092"))?;
        let config = canonical_directory(&runtime.config, summon_i18n::text!("r8c72dc0238"))?;
        let source_home = canonical_directory(
            Path::new(&attempt.source.profile.home),
            summon_i18n::text!("r7eb2f483db"),
        )?;
        let source_config = canonical_directory(
            Path::new(&attempt.source.profile.config_dir),
            summon_i18n::text!("r814e7ff22b"),
        )?;
        let legacy_after_source_exit = matches!(
            attempt.phase(),
            HandoffPhase::SourceExitProved { .. }
                | HandoffPhase::ResumePrepared { .. }
                | HandoffPhase::Resuming { .. }
                | HandoffPhase::Acked { .. }
                | HandoffPhase::RenamedAwaitingAudit { .. }
        );
        if attempt.target_provider != runtime.provider
            && !(attempt.target_provider == ClaudeProvider::Unknown && legacy_after_source_exit)
        {
            bail!(summon_i18n::format!("r3b00d8d72b"));
        }
        match attempt.source.profile.config_source {
            ClaudeConfigSource::Default => {}
            ClaudeConfigSource::Unknown if legacy_after_source_exit => {}
            ClaudeConfigSource::Unknown => {
                bail!(summon_i18n::format!("r79d2278f99"))
            }
            ClaudeConfigSource::Explicit => bail!(summon_i18n::format!("rfcfc690d6a")),
        }
        if source_home != home || source_config != config || config != home.join(".claude") {
            bail!(summon_i18n::format!("r30d5895ba4"));
        }
        let work_dir = canonical_directory(
            Path::new(&attempt.source.locator.resume_lookup_cwd),
            "resume cwd",
        )?;
        let transcript_path = std::fs::canonicalize(&attempt.source.locator.transcript_path)
            .context("canonicalize transcript")?;
        if !transcript_path.is_file() || !transcript_path.starts_with(&config) {
            bail!(summon_i18n::format!("r4360a6b4d6"));
        }
        if target.agent_name != attempt.target.agent || !target.system_prompt_path.is_file() {
            bail!(summon_i18n::format!("rd1e23bd1b2"));
        }
        for (key, value) in &target.env {
            validate_env_pair(key, value)?;
        }
        Ok(Self {
            program,
            home,
            config,
            provider: runtime.provider,
            work_dir,
            transcript_path,
            target: target.clone(),
        })
    }

    fn finish(
        self,
        attempt: &HandoffAttempt,
        source_exit: &SourceExitProof,
    ) -> Result<StrictResumePlan> {
        if source_exit.attempt_id != attempt.attempt_id
            || source_exit.session != attempt.source.session
            || source_exit.source_generation != attempt.source.generation
            || source_exit.source_process != attempt.source.process
            || source_exit.locator_identity_revision != attempt.source.locator_identity_revision
        {
            bail!(summon_i18n::format!("rdc20c66fbc"));
        }
        // preflight 与真正 launch 可能跨 tick；所有磁盘身份再核一次，不能拿旧证明启动漂移资产。
        if canonical_executable(&self.program)? != self.program
            || canonical_directory(&self.home, summon_i18n::text!("r8d814c7092"))? != self.home
            || canonical_directory(&self.config, summon_i18n::text!("r8c72dc0238"))? != self.config
            || canonical_directory(
                Path::new(&attempt.source.locator.resume_lookup_cwd),
                "resume cwd",
            )? != self.work_dir
            || std::fs::canonicalize(&attempt.source.locator.transcript_path)
                .context("canonicalize transcript")?
                != self.transcript_path
            || !self.target.system_prompt_path.is_file()
        {
            bail!(summon_i18n::format!("rf7c0521487"));
        }
        let mut args = self.target.args.clone();
        args.push("--resume".into());
        args.push(attempt.source.session.session_id.clone());
        let token = HandoffToken::generate();
        let ownership_marker = HandoffToken::generate().0;
        let mut env = self
            .target
            .env
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        // 默认配置的语义就是变量缺席；显式写入同一路径也会触发 Claude 首次 onboarding。
        env.remove("CLAUDE_CONFIG_DIR");
        env.insert("HOME".into(), self.home.to_string_lossy().into_owned());
        env.insert(crate::bridge::MANAGED_SESSION_ENV.into(), "1".into());
        env.insert(crate::bridge::MANAGED_TAKEOVER_ENV.into(), "1".into());
        env.insert(
            crate::bridge::HANDOFF_TOKEN_ENV.into(),
            token.as_str().to_string(),
        );
        env.insert(
            crate::bridge::HANDOFF_ATTEMPT_ENV.into(),
            attempt.attempt_id.clone(),
        );
        let env = env.into_iter().collect::<Vec<_>>();
        validate_plan_env(&env)?;
        Ok(StrictResumePlan {
            attempt_id: attempt.attempt_id.clone(),
            binding_id: attempt.binding_id.clone(),
            binding_revision: attempt.binding_revision,
            session: attempt.source.session.clone(),
            source_generation: attempt.source.generation,
            source_process: attempt.source.process.clone(),
            locator_identity_revision: attempt.source.locator_identity_revision,
            program: self.program,
            args,
            env,
            work_dir: self.work_dir,
            transcript_path: self.transcript_path,
            staging_name: format!("{STAGING_PREFIX}{}", attempt.attempt_id),
            target_tmux: attempt.target_tmux.clone(),
            ownership_marker,
            token,
            home: self.home,
            config: self.config,
            provider: self.provider,
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct StrictResumePlan {
    attempt_id: String,
    binding_id: String,
    binding_revision: u64,
    session: SessionKey,
    source_generation: u64,
    source_process: ProcessFingerprint,
    locator_identity_revision: u64,
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    work_dir: PathBuf,
    transcript_path: PathBuf,
    staging_name: String,
    target_tmux: String,
    ownership_marker: String,
    token: HandoffToken,
    home: PathBuf,
    config: PathBuf,
    provider: ClaudeProvider,
}

impl fmt::Debug for StrictResumePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StrictResumePlan")
            .field("attempt_id", &self.attempt_id)
            .field("binding_id", &self.binding_id)
            .field("session", &self.session)
            .field("program", &self.program)
            .field("args", &self.args)
            .field("work_dir", &self.work_dir)
            .field("staging_name", &self.staging_name)
            .field("target_tmux", &self.target_tmux)
            .field("token", &"[redacted]")
            .finish()
    }
}

impl StrictResumePlan {
    pub fn build(
        runtime: &VerifiedTargetRuntime,
        attempt: &HandoffAttempt,
        source_exit: &SourceExitProof,
        target: &HandoffTargetProfile,
    ) -> Result<Self> {
        StrictResumePreflight::build(runtime, attempt, target)?.finish(attempt, source_exit)
    }

    /// coordinator 的唯一 staging launch 入口。
    pub fn launch_staging(&self) -> Result<OwnedHandoffStaging> {
        create_handoff_staging(
            &self.staging_name,
            &self.ownership_marker,
            &self.work_dir,
            &self.program,
            &self.args,
            &self.env,
        )
        .context(summon_i18n::text!("r69e37d1bdd"))
    }

    pub fn token_digest(&self) -> HandoffTokenDigest {
        self.token.digest()
    }

    pub fn staging_name(&self) -> &str {
        &self.staging_name
    }

    fn launch_identity(&self) -> ResumeLaunchIdentity {
        ResumeLaunchIdentity {
            program: self.program.to_string_lossy().into_owned(),
            args: self.args.clone(),
            home: self.home.to_string_lossy().into_owned(),
            config_dir: self.config.to_string_lossy().into_owned(),
            provider: self.provider,
        }
    }

    pub fn ownership_marker(&self) -> &str {
        &self.ownership_marker
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffAckObservation {
    pub token_digest: HandoffTokenDigest,
    pub source: String,
    pub event_id: String,
    pub sequence: u64,
    pub session: SessionKey,
    pub cwd: String,
    pub transcript_path: String,
    pub process: ProcessFingerprint,
    pub profile: ResumeProfile,
    /// dedicated handoff probe 产出的真实 exec argv；generic parser 会把 `--resume` 标成
    /// unsupported，不能拿 ResumeProfile 的 unsupported_args 证明本次命令。
    pub launch: HandoffLaunchObservation,
    pub tmux_pane: String,
    pub staging: TmuxSessionIdentity,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffLaunchObservation {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffAckProof {
    attempt_id: String,
    binding_id: String,
    binding_revision: u64,
    session: SessionKey,
    source_generation: u64,
    resumed_generation: u64,
    source_process: ProcessFingerprint,
    resumed_process: ProcessFingerprint,
    locator_identity_revision: u64,
    session_start_event_id: String,
    session_start_sequence: u64,
    staging: TmuxSessionIdentity,
    #[serde(default)]
    target_provider: ClaudeProvider,
    token_digest: HandoffTokenDigest,
}

#[cfg(test)]
pub fn verify_handoff_ack(
    plan: &StrictResumePlan,
    source_exit: &SourceExitProof,
    observation: HandoffAckObservation,
    source_now: ProcessCheck,
    resumed_now: ProcessCheck,
) -> Result<HandoffAckProof> {
    if source_exit.attempt_id != plan.attempt_id
        || source_exit.session != plan.session
        || source_exit.source_generation != plan.source_generation
        || source_exit.source_process != plan.source_process
        || source_exit.locator_identity_revision != plan.locator_identity_revision
    {
        bail!("ACK 使用了不匹配的 source-exit proof");
    }
    if source_now != ProcessCheck::Gone || resumed_now != ProcessCheck::AliveExact {
        bail!("ACK 时 source/resumed process 状态不满足");
    }
    if !observation
        .token_digest
        .constant_time_eq(&plan.token.digest())
        || observation.source != "resume"
        || observation.event_id.is_empty()
        || observation.sequence == 0
        || observation.session != plan.session
        || observation.cwd != plan.work_dir.to_string_lossy()
        || observation.transcript_path != plan.transcript_path.to_string_lossy()
        || !observation.process.is_complete()
        || observation.process == plan.source_process
        || observation.process.pid == plan.source_process.pid
        || observation.process.executable != plan.program.to_string_lossy()
        || observation.profile.claude_bin != plan.program.to_string_lossy()
        || observation.profile.home != plan.home.to_string_lossy()
        || observation.profile.config_dir != plan.config.to_string_lossy()
        || observation.profile.config_source != ClaudeConfigSource::Default
        || observation.profile.provider != plan.provider
        || !observation.profile.process_verified
        || !observation.profile.environment_verified
        || observation.launch.program != plan.program.to_string_lossy()
        || observation.launch.args != plan.args
        || observation.tmux_pane != observation.staging.pane_id
        || observation.staging.name != plan.staging_name
        || observation.staging.ownership_marker != plan.ownership_marker
    {
        bail!("SessionStart ACK 与 strict resume plan 不匹配");
    }
    Ok(HandoffAckProof {
        attempt_id: plan.attempt_id.clone(),
        binding_id: plan.binding_id.clone(),
        binding_revision: plan.binding_revision,
        session: plan.session.clone(),
        source_generation: plan.source_generation,
        resumed_generation: plan.source_generation.saturating_add(1),
        source_process: plan.source_process.clone(),
        resumed_process: observation.process,
        locator_identity_revision: plan.locator_identity_revision,
        session_start_event_id: observation.event_id,
        session_start_sequence: observation.sequence,
        staging: observation.staging,
        target_provider: plan.provider,
        token_digest: observation.token_digest,
    })
}

/// coordinator 在 stability window 前后各调用一次。
///
/// **必须与 `process_probe` 同源**:expected 指纹由召回卡渲染时刻的 `observe_exact_fingerprint`
/// 落盘,杀源前的复验若另起一套采集,只要两者对同一进程给出不同 executable,整条链就永远
/// 判 PidReused。此前这里自带一份 `ps -o command=` 实现,取 argv[0];而 macOS 的
/// `process_probe` 走 `proc_pidpath` 取内核解析后的真实映像。标准安装
/// `~/.local/bin/claude -> ~/.local/share/claude/versions/<ver>` 正是软链,于是活跃接管必然
/// 在此报「身份已漂移」。收敛成唯一探针,不再保留第二份实现。
pub fn check_source_process(expected: &ProcessFingerprint) -> ProcessCheck {
    match crate::bridge::process_probe::probe_exact_process(expected) {
        Ok(crate::bridge::process_probe::ExactProcessStatus::AliveExact) => {
            ProcessCheck::AliveExact
        }
        Ok(crate::bridge::process_probe::ExactProcessStatus::Gone) => ProcessCheck::Gone,
        Ok(crate::bridge::process_probe::ExactProcessStatus::PidReused(observed)) => {
            ProcessCheck::PidReused(observed)
        }
        // 指纹不完整 / ps 或 libproc 探测失败都在这里收口成 Unreadable(fail-closed)。
        Err(_) => ProcessCheck::Unreadable,
    }
}

/// SIGTERM 原语必须携带的 resume 资产守卫。把 transcript 可读/SID/confinement 校验下沉到
/// 发信号函数本身，调用方无法只靠“先前某处做过 preflight”的脆弱顺序约定绕过。
pub struct SourceTranscriptGuard<'a> {
    transcript_path: &'a Path,
    config_dir: &'a Path,
    session_id: &'a str,
}

impl<'a> SourceTranscriptGuard<'a> {
    pub fn new(transcript_path: &'a Path, config_dir: &'a Path, session_id: &'a str) -> Self {
        Self {
            transcript_path,
            config_dir,
            session_id,
        }
    }
}

/// 只向经过 `(pid,start_time,executable)` 复验的 Claude 主进程发送一次 SIGTERM。这里刻意不用
/// killpg，也不碰父 shell/tmux；发信号前在同一原语内强制证明 transcript 可读且 SID/confinement
/// 正确。ESRCH 表示复验后进程已自然退出，可由后续退出证明收口。
pub fn signal_exact_source_termination<F>(
    expected: &ProcessFingerprint,
    guard: &SourceTranscriptGuard<'_>,
    verify_safe_boundary: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    if check_source_process(expected) != ProcessCheck::AliveExact {
        bail!(summon_i18n::format!("raddd7fbc8d"));
    }
    let _asset = sample_transcript(guard.transcript_path, guard.config_dir, guard.session_id)
        .context(summon_i18n::text!("r5f4e600053"))?;
    // 读 transcript 期间进程可能退出/PID reuse；发信号前必须再做最后一次 exact 复验。
    if check_source_process(expected) != ProcessCheck::AliveExact {
        bail!(summon_i18n::format!("r09d9871b3b"));
    }
    // 安全边界也在原语内做最后一次现测。recall 用 `claude agents` 查 exact SID/PID 仍活跃；
    // coordinator 用 durable lifecycle + locator identity。闭包返回后再做一次 exact 复验再 kill。
    verify_safe_boundary().context(summon_i18n::text!("r43e196a8f5"))?;
    if check_source_process(expected) != ProcessCheck::AliveExact {
        bail!(summon_i18n::format!("r9938651d3d"));
    }
    let pid = i32::try_from(expected.pid).context(summon_i18n::text!("r3c491b0aab"))?;
    if pid <= 1 {
        bail!(summon_i18n::format!("rb0798b77dd"));
    }
    match nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    ) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(error).context(summon_i18n::text!("r767b2b0e56")),
    }
}

/// 源 Gone 后的 transcript 稳定窗。只有两次采样的 canonical path/inode/len/mtime/尾行完全
/// 一致才允许进入 Stage 1；否则保留盘上会话、拒绝启动第二个 writer。
pub fn verify_stable_transcript(path: &Path, config_dir: &Path, session_id: &str) -> Result<()> {
    let first = sample_transcript(path, config_dir, session_id)?;
    std::thread::sleep(MIN_STABILITY);
    let second = sample_transcript(path, config_dir, session_id)?;
    if !stable_snapshot_equal(&first.snapshot, &second.snapshot)
        || first.last_line != second.last_line
    {
        bail!(summon_i18n::format!("r058d69a38d"));
    }
    Ok(())
}

pub fn sample_transcript(
    path: &Path,
    config_dir: &Path,
    session_id: &str,
) -> Result<TranscriptSample> {
    if session_id.is_empty() {
        bail!(summon_i18n::format!("rbc190f686f"));
    }
    let symlink = std::fs::symlink_metadata(path).context("stat transcript")?;
    if symlink.file_type().is_symlink() || !symlink.is_file() {
        bail!(summon_i18n::format!("r84e4a1735d"));
    }
    let canonical_config = std::fs::canonicalize(config_dir).context("canonicalize config")?;
    let canonical = std::fs::canonicalize(path).context("canonicalize transcript")?;
    if !canonical.starts_with(&canonical_config) {
        bail!(summon_i18n::format!("ra1ecc49339"));
    }
    let mut file = File::open(&canonical).context(summon_i18n::text!("r3b521b81fe"))?;
    let before = file.metadata().context("stat transcript before")?;
    if before.len() == 0 {
        bail!(summon_i18n::format!("rea6e7af50b"));
    }
    let start = before.len().saturating_sub(MAX_TRANSCRIPT_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))
        .context("seek transcript tail")?;
    let mut tail = Vec::new();
    file.take(MAX_TRANSCRIPT_TAIL_BYTES + 1)
        .read_to_end(&mut tail)
        .context(summon_i18n::text!("r8c7d82fc6e"))?;
    if tail.len() as u64 > MAX_TRANSCRIPT_TAIL_BYTES || tail.last() != Some(&b'\n') {
        bail!(summon_i18n::format!("r1fa033df88"));
    }
    tail.pop();
    let last = tail
        .rsplit(|byte| *byte == b'\n')
        .next()
        .filter(|line| !line.is_empty())
        .context(summon_i18n::text!("r57d0060e47"))?
        .to_vec();
    let json: serde_json::Value =
        serde_json::from_slice(&last).context(summon_i18n::text!("r10ea091ba9"))?;
    let observed_sid = json
        .get("sessionId")
        .or_else(|| json.get("session_id"))
        .and_then(serde_json::Value::as_str);
    if observed_sid != Some(session_id) {
        bail!(summon_i18n::format!("ra0d21cd424"));
    }
    let after = std::fs::metadata(&canonical).context("stat transcript after")?;
    let before_identity = metadata_identity(&before);
    if before_identity != metadata_identity(&after) {
        bail!(summon_i18n::format!("rb2f7e8533e"));
    }
    Ok(TranscriptSample {
        snapshot: TranscriptSnapshot {
            canonical_path: canonical.to_string_lossy().into_owned(),
            device: before.dev(),
            inode: before.ino(),
            len: before.len(),
            modified_ns: modified_ns(&before),
            last_line_len: last.len() as u64,
            observed_at_ms: now_ms(),
        },
        last_line: last,
    })
}

fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, u64, u64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        modified_ns(metadata),
    )
}

fn stable_snapshot_equal(left: &TranscriptSnapshot, right: &TranscriptSnapshot) -> bool {
    left.canonical_path == right.canonical_path
        && left.device == right.device
        && left.inode == right.inode
        && left.len == right.len
        && left.modified_ns == right.modified_ns
        && left.last_line_len == right.last_line_len
}

fn modified_ns(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(metadata.mtime_nsec())
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path).with_context(|| format!("canonicalize {label}"))?;
    if !canonical.is_dir() {
        bail!(summon_i18n::format!("re8ae36da8f"));
    }
    Ok(canonical)
}

fn canonical_executable(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path).context("canonicalize claude binary")?;
    let metadata = std::fs::metadata(&canonical).context("stat claude binary")?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!(summon_i18n::format!("r1f4e8ff147"));
    }
    Ok(canonical)
}

fn validate_plan_env(env: &[(String, String)]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (key, value) in env {
        validate_env_pair(key, value)?;
        if !seen.insert(key.as_str()) {
            bail!(summon_i18n::format!("re45f1dcf7d"));
        }
    }
    if seen.contains("CLAUDE_CONFIG_DIR") {
        bail!(summon_i18n::format!("r0991874712"));
    }
    for required in [
        "HOME",
        crate::bridge::MANAGED_SESSION_ENV,
        crate::bridge::MANAGED_TAKEOVER_ENV,
        crate::bridge::HANDOFF_TOKEN_ENV,
        crate::bridge::HANDOFF_ATTEMPT_ENV,
    ] {
        if !seen.contains(required)
            || env
                .iter()
                .find(|(key, _)| key == required)
                .is_none_or(|(_, value)| value.is_empty())
        {
            bail!(summon_i18n::format!("r6692b2ce4f"));
        }
    }
    Ok(())
}

fn validate_env_pair(key: &str, value: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > 128
        || value.len() > 16 * 1024
        || value.contains('\0')
        || !valid_env_key(key)
    {
        bail!(summon_i18n::format!("r13e237c5d0"));
    }
    Ok(())
}

fn target_extra_env_is_safe(key: &str, value: &str) -> bool {
    validate_env_pair(key, value).is_ok()
        && !matches!(key, "HOME" | "CLAUDE_CONFIG_DIR")
        // takeover 也是 runtime 管理的执行上下文；复用统一 namespace 边界，避免新增内部键时
        // 这条旁路再次漏掉 TASK/RUN/CONTROL 或任意 FEISHU_*。
        && !summon_core::config::agent_identity::is_runtime_owned_env_key(key)
}

fn valid_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_tmux_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HandoffCoordinatorStats {
    pub source_signals: u64,
    pub source_exits: u64,
    pub resumes_started: u64,
    pub acks_verified: u64,
    pub completed: u64,
    pub resume_retries: u64,
    pub sentinels_released: u64,
}

fn notice_for_phase(phase: &HandoffPhase) -> Option<summon_core::outbox::BridgeHandoffNotice> {
    use summon_core::outbox::BridgeHandoffNotice;
    match phase {
        HandoffPhase::AwaitingSafeBoundary => Some(BridgeHandoffNotice::Requested),
        HandoffPhase::SourceTerminationRequested { .. }
        | HandoffPhase::AwaitingSourceExit {
            signal_sent_at_ms: Some(_),
            ..
        } => Some(BridgeHandoffNotice::TerminatingSource),
        HandoffPhase::SourceExitProved { .. }
        | HandoffPhase::ResumePrepared { .. }
        | HandoffPhase::Resuming { .. }
        | HandoffPhase::Acked { .. } => Some(BridgeHandoffNotice::Resuming),
        HandoffPhase::RenamedAwaitingAudit { .. } | HandoffPhase::Completed => {
            Some(BridgeHandoffNotice::Completed)
        }
        HandoffPhase::Failed => Some(BridgeHandoffNotice::Failed),
        // canonical ownership 不确定时不能发送“未启动新 writer”的普通失败文案，也不能回收证明。
        HandoffPhase::CleanupBlocked { .. } => None,
        // 旧版 attempt 仍保持“等待用户手工退出”的原语义，不新增远程控制面通知。
        HandoffPhase::AwaitingSourceExit {
            signal_sent_at_ms: None,
            ..
        } => None,
    }
}

pub fn handoff_notice_is_current(
    attempt: &HandoffAttempt,
    notice: summon_core::outbox::BridgeHandoffNotice,
) -> bool {
    notice_for_phase(attempt.phase()) == Some(notice)
}

fn ensure_handoff_notice(
    outbox: &crate::runtime::outbox_store::OutboxStore,
    attempt: &HandoffAttempt,
    notice: summon_core::outbox::BridgeHandoffNotice,
) -> Result<crate::runtime::outbox_store::EnqueueOnceOutcome> {
    use summon_core::outbox::{
        bridge_delivery_id, BridgeDeliveryFence, BridgeDeliveryKind, BridgeHandoffNotice,
        OutboxEvent, OutboxKind, BRIDGE_CARD_WIRE_VERSION,
    };
    let (sequence, slug, text) = match notice {
        BridgeHandoffNotice::Requested => (1, "requested", summon_i18n::text!("r31371f0cdc")),
        BridgeHandoffNotice::TerminatingSource => {
            (2, "terminating", summon_i18n::text!("r1a0dc20e38"))
        }
        BridgeHandoffNotice::Resuming => (3, "resuming", summon_i18n::text!("r56636ef8f5")),
        BridgeHandoffNotice::Completed => (4, "completed", summon_i18n::text!("rd326a733c7")),
        BridgeHandoffNotice::Failed => (4, "failed", summon_i18n::text!("raff0afb2aa")),
    };
    let receipt_id = format!("{}-{slug}", attempt.attempt_id());
    let id = bridge_delivery_id(BridgeDeliveryKind::HandoffStatus, &receipt_id);
    let created_ms = attempt.created_at_ms().saturating_add(sequence);
    let event = OutboxEvent {
        id,
        agent: attempt.target().agent.clone(),
        delivery_scope: attempt.target().scope.clone(),
        created_ms,
        attempts: 0,
        next_attempt_ms: created_ms,
        last_error: None,
        kind: OutboxKind::BridgeHandoffStatus {
            fence: BridgeDeliveryFence {
                scope: attempt.target().scope.clone(),
                agent: attempt.target().agent.clone(),
                chat_id: attempt.target().chat_id.clone(),
                chat_type: attempt.target_chat_type().to_string(),
                binding_id: attempt.binding_id().to_string(),
                binding_revision: attempt.binding_revision(),
                session: attempt.source().session().clone(),
                generation: attempt.source().generation(),
                lease_binding_id: attempt.binding_id().to_string(),
                receipt_id,
                sequence,
            },
            attempt_id: attempt.attempt_id().to_string(),
            notice,
            card_version: BRIDGE_CARD_WIRE_VERSION,
            rendered_card: summon_core::feishu::card_markdown_v2(text),
        },
    };
    outbox.enqueue_once(&event)
}

pub fn coordinator_tick(
    store: &crate::bridge::store::BridgeStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
) -> Result<HandoffCoordinatorStats> {
    let mut stats = HandoffCoordinatorStats::default();
    for mut attempt in store.load_handoff_attempts()? {
        let notice_outcome = if let Some(notice) = notice_for_phase(attempt.phase()) {
            let outcome = ensure_handoff_notice(outbox, &attempt, notice)?;
            if matches!(
                attempt.phase(),
                HandoffPhase::Completed
                    | HandoffPhase::Failed
                    | HandoffPhase::CleanupBlocked { .. }
            ) && matches!(
                &outcome,
                crate::runtime::outbox_store::EnqueueOnceOutcome::Existing(
                    crate::runtime::outbox_store::OutboxLocation::Archive
                        | crate::runtime::outbox_store::OutboxLocation::Dead,
                    _,
                )
            ) {
                store.remove_terminal_handoff_attempt(
                    attempt.attempt_id(),
                    attempt.binding_id(),
                    attempt.binding_revision(),
                )?;
                continue;
            }
            Some(outcome)
        } else {
            None
        };
        if matches!(
            attempt.phase(),
            HandoffPhase::Failed | HandoffPhase::CleanupBlocked { .. }
        ) {
            continue;
        }
        let state = store.load_state()?;
        if matches!(attempt.phase(), HandoffPhase::Completed) {
            continue;
        }
        // audit 已发且 core completion 已 durable、但 attempt 最后一写失败时，slot 已正常释放。
        // 只有 receipt 与 rename proof 全字段一致才补写 Completed，不能把“binding 消失”猜成成功。
        let completion_is_durable = match attempt.phase() {
            HandoffPhase::RenamedAwaitingAudit { proof, .. } => {
                state.as_ref().is_some_and(|state| {
                    state
                        .managed_takeovers
                        .get(&attempt.target().storage_key())
                        .is_some_and(|takeover| {
                            takeover.binding_id == attempt.binding_id()
                                && takeover.binding_revision == attempt.binding_revision
                                && takeover.target_session == attempt.target_tmux()
                                && takeover.session == proof.session
                                && takeover.generation == proof.resumed_generation
                                && takeover.process == proof.resumed_process
                                && takeover.session_start_event_id == proof.session_start_event_id
                        })
                })
            }
            _ => false,
        };
        if completion_is_durable {
            attempt.mark_completed()?;
            store.persist_handoff_attempt(&attempt)?;
            stats.completed = stats.completed.saturating_add(1);
            continue;
        }
        let binding_live = state.as_ref().is_some_and(|state| {
            matches!(
                state.slot(attempt.target()),
                Some(summon_core::bridge::ChatSlot::HandoffPending(binding))
                    if binding.id == attempt.binding_id
                        && binding.revision == attempt.binding_revision
                        && binding.session == attempt.source.session
            )
        });
        let binding_closing = state.as_ref().is_some_and(|state| {
            matches!(
                state.slot(attempt.target()),
                Some(summon_core::bridge::ChatSlot::Closing { binding, .. })
                    if binding.id == attempt.binding_id
                        && binding.revision == attempt.binding_revision
                        && binding.session == attempt.source.session
            )
        });
        if !binding_live {
            let mut resources_released = false;
            match attempt.phase().clone() {
                HandoffPhase::ResumePrepared {
                    staging_name,
                    ownership_marker,
                    ..
                } => {
                    if let Some(staging) =
                        OwnedHandoffStaging::recover_named(&staging_name, &ownership_marker)?
                    {
                        staging.cleanup()?;
                    }
                    attempt.phase = HandoffPhase::Failed;
                    resources_released = true;
                }
                HandoffPhase::Resuming { staging, .. }
                | HandoffPhase::Acked {
                    proof: HandoffAckProof { staging, .. },
                } => {
                    if crate::session::tmux::has_session_strict(&staging.name)? {
                        OwnedHandoffStaging::recover(&staging)?.cleanup()?;
                        attempt.phase = HandoffPhase::Failed;
                        resources_released = true;
                    } else if !crate::session::tmux::has_session_strict(attempt.target_tmux())? {
                        attempt.phase = HandoffPhase::Failed;
                        resources_released = true;
                    } else {
                        attempt.phase = HandoffPhase::CleanupBlocked {
                            reason: summon_i18n::text!("r043cdbe48e").into(),
                        };
                    }
                }
                HandoffPhase::RenamedAwaitingAudit { .. } => {
                    attempt.phase = HandoffPhase::CleanupBlocked {
                        reason: summon_i18n::text!("rab990b637a").into(),
                    };
                }
                HandoffPhase::AwaitingSafeBoundary
                | HandoffPhase::SourceTerminationRequested { .. }
                | HandoffPhase::AwaitingSourceExit { .. }
                | HandoffPhase::SourceExitProved { .. } => {
                    attempt.phase = HandoffPhase::Failed;
                    resources_released = true;
                }
                HandoffPhase::CleanupBlocked { .. }
                | HandoffPhase::Failed
                | HandoffPhase::Completed => {}
            }
            if binding_closing && resources_released {
                store.with_state(|state| {
                    state
                        .complete_closing(attempt.target(), attempt.binding_id())
                        .map(|_| ())
                        .map_err(anyhow::Error::new)
                })?;
            }
            store.persist_handoff_attempt(&attempt)?;
            continue;
        }
        match attempt.phase().clone() {
            HandoffPhase::AwaitingSafeBoundary => {
                let Some(record) = state
                    .as_ref()
                    .and_then(|state| state.session(attempt.source().session()))
                else {
                    fail_before_resume(store, &mut attempt)?;
                    continue;
                };
                if exact_source_identity(
                    state.as_ref().expect(summon_i18n::text!("r9e656ad45d")),
                    &attempt,
                )
                .is_none()
                {
                    fail_before_resume(store, &mut attempt)?;
                    continue;
                }
                match remote_takeover_readiness(
                    record.lifecycle,
                    check_source_process(attempt.source().process()),
                ) {
                    Ok(RemoteTakeoverReadiness::WaitForSafeBoundary) => continue,
                    Ok(RemoteTakeoverReadiness::RequestTermination) => {
                        attempt.mark_termination_requested(now_ms())?;
                        store.persist_handoff_attempt(&attempt)?;
                    }
                    Ok(RemoteTakeoverReadiness::ProveSourceExit) => {
                        prove_source_exit(store, &mut attempt, state.as_ref().unwrap())?;
                        stats.source_exits = stats.source_exits.saturating_add(1);
                    }
                    Err(_) => fail_before_resume(store, &mut attempt)?,
                }
            }
            HandoffPhase::SourceTerminationRequested { .. } => {
                let Some(record) = state
                    .as_ref()
                    .and_then(|state| state.session(attempt.source().session()))
                else {
                    fail_before_resume(store, &mut attempt)?;
                    continue;
                };
                if exact_source_identity(
                    state.as_ref().expect(summon_i18n::text!("r9e656ad45d")),
                    &attempt,
                )
                .is_none()
                {
                    fail_before_resume(store, &mut attempt)?;
                    continue;
                }
                match remote_takeover_readiness(
                    record.lifecycle,
                    check_source_process(attempt.source().process()),
                ) {
                    Ok(RemoteTakeoverReadiness::WaitForSafeBoundary) => {
                        // 用户在 Stop 与 SIGTERM 之间又提交了新一轮：退回安全边界等待，绝不打断。
                        attempt.phase = HandoffPhase::AwaitingSafeBoundary;
                        store.persist_handoff_attempt(&attempt)?;
                    }
                    Ok(RemoteTakeoverReadiness::RequestTermination) => {
                        if let Err(error) = prepare_resume_preflight(&attempt) {
                            log(&summon_i18n::format!("r684898044f", attempt.attempt_id()));
                            fail_before_resume(store, &mut attempt)?;
                            continue;
                        }
                        if sample_transcript(
                            Path::new(&attempt.source().locator().transcript_path),
                            Path::new(&attempt.source().profile().config_dir),
                            &attempt.source().session().session_id,
                        )
                        .is_err()
                        {
                            // 杀源前先证明 resume 资产可读；否则失败也必须保留源 Claude。
                            fail_before_resume(store, &mut attempt)?;
                            continue;
                        }
                        // preflight/磁盘采样期间源终端可能收到新 prompt；发信号前必须用最新
                        // durable lifecycle 再过一次安全边界，不能拿 tick 开头的 Idle 结论杀新一轮。
                        let latest_state = store
                            .load_state()?
                            .context(summon_i18n::text!("r67048310ce"))?;
                        let Some(latest_record) = latest_state.session(attempt.source().session())
                        else {
                            fail_before_resume(store, &mut attempt)?;
                            continue;
                        };
                        if exact_source_identity(&latest_state, &attempt).is_none() {
                            fail_before_resume(store, &mut attempt)?;
                            continue;
                        }
                        match remote_takeover_readiness(
                            latest_record.lifecycle,
                            check_source_process(attempt.source().process()),
                        ) {
                            Ok(RemoteTakeoverReadiness::WaitForSafeBoundary) => {
                                attempt.phase = HandoffPhase::AwaitingSafeBoundary;
                                store.persist_handoff_attempt(&attempt)?;
                                continue;
                            }
                            Ok(RemoteTakeoverReadiness::ProveSourceExit) => {
                                prove_source_exit(store, &mut attempt, &latest_state)?;
                                stats.source_exits = stats.source_exits.saturating_add(1);
                                continue;
                            }
                            Ok(RemoteTakeoverReadiness::RequestTermination) => {}
                            Err(_) => {
                                fail_before_resume(store, &mut attempt)?;
                                continue;
                            }
                        }
                        let guard = SourceTranscriptGuard::new(
                            Path::new(&attempt.source().locator().transcript_path),
                            Path::new(&attempt.source().profile().config_dir),
                            &attempt.source().session().session_id,
                        );
                        signal_exact_source_termination(
                            attempt.source().process(),
                            &guard,
                            || {
                                let latest_state = store
                                    .load_state()?
                                    .context(summon_i18n::text!("rc4ed4c47b4"))?;
                                let record = latest_state
                                    .session(attempt.source().session())
                                    .context(summon_i18n::text!("rcb8132cc70"))?;
                                anyhow::ensure!(
                                    exact_source_identity(&latest_state, &attempt).is_some(),
                                    summon_i18n::format!("rf1f385f7ff")
                                );
                                anyhow::ensure!(
                                    matches!(
                                        remote_takeover_readiness(
                                            record.lifecycle,
                                            check_source_process(attempt.source().process())
                                        )?,
                                        RemoteTakeoverReadiness::RequestTermination
                                    ),
                                    summon_i18n::format!("rb64e39f3a3")
                                );
                                Ok(())
                            },
                        )?;
                        let sent_at = now_ms();
                        attempt.mark_source_signal_sent(
                            sent_at,
                            sent_at.saturating_add(SOURCE_TERMINATION_GRACE_MS),
                        )?;
                        store.persist_handoff_attempt(&attempt)?;
                        stats.source_signals = stats.source_signals.saturating_add(1);
                    }
                    Ok(RemoteTakeoverReadiness::ProveSourceExit) => {
                        prove_source_exit(store, &mut attempt, state.as_ref().unwrap())?;
                        stats.source_exits = stats.source_exits.saturating_add(1);
                    }
                    Err(_) => fail_before_resume(store, &mut attempt)?,
                }
            }
            HandoffPhase::AwaitingSourceExit {
                signal_sent_at_ms,
                deadline_ms,
            } => match check_source_process(attempt.source().process()) {
                ProcessCheck::Gone => {
                    let Some(state) = state.as_ref() else {
                        fail_before_resume(store, &mut attempt)?;
                        continue;
                    };
                    prove_source_exit(store, &mut attempt, state)?;
                    stats.source_exits = stats.source_exits.saturating_add(1);
                }
                ProcessCheck::AliveExact => {
                    if signal_sent_at_ms.is_some()
                        && deadline_ms.is_some_and(|deadline| now_ms() > deadline)
                    {
                        // 默认远程接管永不升级 SIGKILL；宽限期后释放 reservation，保留源进程。
                        fail_before_resume(store, &mut attempt)?;
                    }
                }
                ProcessCheck::PidReused(_) | ProcessCheck::Unreadable => {
                    fail_before_resume(store, &mut attempt)?;
                }
            },
            HandoffPhase::ResumePrepared {
                proof,
                staging_name,
                ownership_marker,
                token_digest,
                ack_deadline_ms,
                launch,
            } => {
                if !launch.is_complete() {
                    if let Some(staging) =
                        OwnedHandoffStaging::recover_named(&staging_name, &ownership_marker)?
                    {
                        staging.cleanup()?;
                    }
                    schedule_resume_retry(
                        store,
                        &mut attempt,
                        proof,
                        summon_i18n::text!("r187de8b306"),
                        RESUME_LAUNCH_RETRY_DELAY_MS,
                    )?;
                    continue;
                }
                match OwnedHandoffStaging::recover_named(&staging_name, &ownership_marker)? {
                    Some(staging) => {
                        attempt.mark_resuming(
                            proof,
                            staging.identity().clone(),
                            token_digest,
                            ack_deadline_ms,
                        )?;
                        store.persist_handoff_attempt(&attempt)?;
                    }
                    None => {
                        // prepared 已 durable 但 launch 尚未发生时进程退出：raw token 从未落盘，
                        // 不能猜旧 token，但 source-exit proof 仍有效；生成全新 plan/token 自动重试。
                        schedule_resume_retry(
                            store,
                            &mut attempt,
                            proof,
                            summon_i18n::text!("r4d1ea37de4"),
                            RESUME_LAUNCH_RETRY_DELAY_MS,
                        )?;
                    }
                }
            }
            HandoffPhase::Resuming {
                proof,
                staging,
                token_digest,
                ack_deadline_ms,
                launch,
            } => {
                if !launch.is_complete() {
                    fail_resuming(
                        store,
                        &mut attempt,
                        &staging,
                        summon_i18n::text!("r56916f030f"),
                    )?;
                    continue;
                }
                let Some(envelope) = store.load_handoff_ack(attempt.attempt_id())? else {
                    if now_ms() > ack_deadline_ms {
                        fail_resuming(
                            store,
                            &mut attempt,
                            &staging,
                            summon_i18n::text!("r338290941a"),
                        )?;
                    }
                    continue;
                };
                if now_ms() > ack_deadline_ms {
                    fail_resuming(
                        store,
                        &mut attempt,
                        &staging,
                        summon_i18n::text!("r298e086126"),
                    )?;
                    continue;
                }
                let Some(ack) = envelope.handoff_ack.as_ref() else {
                    fail_resuming(
                        store,
                        &mut attempt,
                        &staging,
                        summon_i18n::text!("r71a9dfd4d1"),
                    )?;
                    continue;
                };
                if ack.attempt_id != attempt.attempt_id() {
                    fail_resuming(
                        store,
                        &mut attempt,
                        &staging,
                        summon_i18n::text!("re6a1939d5f"),
                    )?;
                    continue;
                }
                let observed_digest = match HandoffTokenDigest::from_hex(&ack.token_sha256) {
                    Ok(digest) => digest,
                    Err(_) => {
                        fail_resuming(
                            store,
                            &mut attempt,
                            &staging,
                            summon_i18n::text!("r116ac1a7e6"),
                        )?;
                        continue;
                    }
                };
                if !observed_digest.constant_time_eq(&token_digest) {
                    // 同一 attempt 的恢复重试会生成全新 token。磁盘上的旧 ACK 只代表上一轮
                    // staging，不能据此杀掉当前 writer；Hook 会按当前 durable token 原子替换它。
                    continue;
                }
                let crate::bridge::store::HookEvent::SessionStart { source, .. } = &envelope.event
                else {
                    fail_resuming(
                        store,
                        &mut attempt,
                        &staging,
                        summon_i18n::text!("r35e182c8eb"),
                    )?;
                    continue;
                };
                let Some(resumed) = envelope.process.clone() else {
                    fail_resuming(
                        store,
                        &mut attempt,
                        &staging,
                        summon_i18n::text!("r7a39985483"),
                    )?;
                    continue;
                };
                let state = store
                    .load_state()?
                    .context(summon_i18n::text!("reaf3b51c60"))?;
                let Some(sequence) = state.processed_events.get(&envelope.event_id).copied() else {
                    if now_ms() > ack_deadline_ms {
                        fail_resuming(
                            store,
                            &mut attempt,
                            &staging,
                            summon_i18n::text!("r513a28d65a"),
                        )?;
                    } else if !store.hook_event_is_pending(&envelope.event_id)? {
                        // Hook 按“ACK proof → 通用 spool”顺序写；崩溃在两者之间时用脱敏 ACK
                        // envelope 补投。pending 已存在则等待 reducer，避免每 tick 制造重复事件。
                        store.enqueue_hook(&envelope)?;
                    }
                    continue;
                };
                let record = state
                    .session(attempt.source().session())
                    .context(summon_i18n::text!("r5f726e1faa"))?;
                let target_runtime = match load_target_agent(attempt.target()).and_then(|agent| {
                    let target = HandoffTargetProfile::for_agent(
                        &agent,
                        attempt.target_chat_type(),
                        &attempt.target().chat_id,
                        attempt.target_tmux(),
                    )?;
                    let runtime = VerifiedTargetRuntime::verify(&target)?;
                    Ok((target, runtime))
                }) {
                    Ok(target_runtime) => target_runtime,
                    Err(_) => {
                        fail_resuming(
                            store,
                            &mut attempt,
                            &staging,
                            summon_i18n::text!("r1a4b043ce1"),
                        )?;
                        continue;
                    }
                };
                let (target, runtime) = target_runtime;
                let mut current_args = target.args.clone();
                current_args.extend([
                    "--resume".into(),
                    attempt.source().session().session_id.clone(),
                ]);
                let observed_launch = crate::bridge::process_probe::observe_exact_launch(&resumed)?;
                let expected_program = launch.program.as_str();
                let owned = OwnedHandoffStaging::recover(&staging)
                    .context(summon_i18n::text!("r2b5e5fe5ff"))?;
                // launch 身份逐项比对与 recall 全托管共用同一个纯函数(单一真相源,别再分叉);
                // 不匹配时把字段名 + 期望值/实际值带进失败原因,失败语义仍是 fail-closed。
                let identity_mismatches = summon_core::bridge::diff_resume_identity(
                    &envelope.resume_profile,
                    &ExpectedResumeIdentity {
                        claude_bin: expected_program,
                        home: &launch.home,
                        config_dir: &launch.config_dir,
                        provider: launch.provider,
                    },
                );
                if source != "resume"
                    || proof.attempt_id != attempt.attempt_id
                    || envelope.session != *attempt.source().session()
                    || envelope.cwd != attempt.source().locator().resume_lookup_cwd
                    || envelope.transcript_path != attempt.source().locator().transcript_path
                    || !identity_mismatches.is_empty()
                    || resumed == *attempt.source().process()
                    || observed_launch.program != expected_program
                    || observed_launch.args != launch.args
                    || runtime.program.to_string_lossy() != expected_program
                    || runtime.home.to_string_lossy() != launch.home
                    || runtime.config.to_string_lossy() != launch.config_dir
                    || runtime.provider != launch.provider
                    || current_args != launch.args
                    || !owned.owns_process(resumed.pid)?
                    || check_source_process(attempt.source().process()) != ProcessCheck::Gone
                    || record.generation != attempt.source().generation().saturating_add(1)
                    || record.process.as_ref() != Some(&resumed)
                {
                    let identity_detail = if identity_mismatches.is_empty() {
                        String::new()
                    } else {
                        summon_i18n::format!(
                            "r743f1aeaa5",
                            summon_core::bridge::describe_resume_identity_mismatches(
                                &identity_mismatches
                            )
                        )
                    };
                    fail_resuming(
                        store,
                        &mut attempt,
                        &staging,
                        &summon_i18n::format!("rc5cdbfc5b6"),
                    )?;
                    continue;
                }
                let ack_proof = HandoffAckProof {
                    attempt_id: attempt.attempt_id.clone(),
                    binding_id: attempt.binding_id.clone(),
                    binding_revision: attempt.binding_revision,
                    session: attempt.source.session.clone(),
                    source_generation: attempt.source.generation,
                    resumed_generation: record.generation,
                    source_process: attempt.source.process.clone(),
                    resumed_process: resumed,
                    locator_identity_revision: attempt.source.locator_identity_revision,
                    session_start_event_id: envelope.event_id,
                    session_start_sequence: sequence,
                    staging,
                    target_provider: launch.provider,
                    token_digest,
                };
                attempt.mark_acked(ack_proof)?;
                store.persist_handoff_attempt(&attempt)?;
                stats.acks_verified = stats.acks_verified.saturating_add(1);
            }
            HandoffPhase::Acked { proof } => {
                let target_identity =
                    if crate::session::tmux::has_session_strict(&proof.staging.name)? {
                        let owned = OwnedHandoffStaging::recover(&proof.staging)
                            .context(summon_i18n::text!("rabc1086520"))?;
                        owned
                            .rename_exact(attempt.target_tmux())
                            .context("handoff staging rename canonical")?
                            .identity()
                            .clone()
                    } else {
                        crate::session::tmux::RenamedHandoffTmux::recover(
                            &proof.staging,
                            attempt.target_tmux(),
                        )?
                        .identity()
                        .clone()
                    };
                attempt.mark_renamed(proof, target_identity)?;
                store.persist_handoff_attempt(&attempt)?;
            }
            HandoffPhase::RenamedAwaitingAudit { proof, target } => {
                let _renamed = crate::session::tmux::RenamedHandoffTmux::recover(
                    &proof.staging,
                    &target.name,
                )?;
                if matches!(
                    &notice_outcome,
                    Some(crate::runtime::outbox_store::EnqueueOnceOutcome::Existing(
                        crate::runtime::outbox_store::OutboxLocation::Dead,
                        _,
                    ))
                ) {
                    attempt.phase = HandoffPhase::CleanupBlocked {
                        reason: summon_i18n::text!("rd9212053fb").into(),
                    };
                    store.persist_handoff_attempt(&attempt)?;
                    continue;
                }
                if !matches!(
                    &notice_outcome,
                    Some(crate::runtime::outbox_store::EnqueueOnceOutcome::Existing(
                        crate::runtime::outbox_store::OutboxLocation::Archive,
                        _,
                    ))
                ) {
                    continue;
                }
                finalize_takeover(store, &mut attempt, &proof)?;
                stats.completed = stats.completed.saturating_add(1);
            }
            HandoffPhase::SourceExitProved {
                proof, retry_at_ms, ..
            } => {
                if retry_at_ms > now_ms() {
                    continue;
                }
                // daemon 若在落 source proof 后、启动 staging 前退出，下一 tick 从同一 proof 继续。
                let plan = prepare_resume_plan(&attempt, &proof);
                let plan = match plan {
                    Ok(plan) => plan,
                    Err(error) => {
                        log(&summon_i18n::format!("r7c4086e4f0", attempt.attempt_id()));
                        attempt
                            .defer_resume(&error, now_ms().saturating_add(RESUME_RETRY_DELAY_MS))?;
                        store.persist_handoff_attempt(&attempt)?;
                        stats.resume_retries = stats.resume_retries.saturating_add(1);
                        continue;
                    }
                };
                let deadline = now_ms().saturating_add(60_000);
                attempt.mark_resume_prepared(
                    proof.clone(),
                    plan.staging_name().to_string(),
                    plan.ownership_marker().to_string(),
                    plan.token_digest(),
                    deadline,
                    plan.launch_identity(),
                )?;
                store.persist_handoff_attempt(&attempt)?;
                let staging = plan.launch_staging()?;
                attempt.mark_resuming(
                    proof,
                    staging.identity().clone(),
                    plan.token_digest(),
                    deadline,
                )?;
                store.persist_handoff_attempt(&attempt)?;
                stats.resumes_started = stats.resumes_started.saturating_add(1);
            }
            HandoffPhase::CleanupBlocked { .. }
            | HandoffPhase::Failed
            | HandoffPhase::Completed => {}
        }
    }
    store.prune_orphan_handoff_acks()?;
    stats.sentinels_released = release_stale_managed_takeovers(store)?;
    Ok(stats)
}

/// 普通 chat 的 reset / 群生命周期 teardown 才会同时删除 marker 并终止 canonical tmux。
/// coordinator 只在这两个独立证据都消失时释放 SID 冲突哨兵；Clean 保留 marker，因此仍可
/// 在下一次普通 resume 后继续更新 generation/process。
fn release_stale_managed_takeovers(store: &crate::bridge::store::BridgeStore) -> Result<u64> {
    // 与 reconcile 同理:coordinator 每 100ms 都会走到这里,而绝大多数 tick 一个哨兵都没有。
    // 只读判断走缓存,释放动作仍在下面的 with_state 里按 flock 重新读一遍。
    let receipts = store
        .load_state_cached()?
        .map(|state| {
            state
                .managed_takeovers
                .values()
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut released = 0u64;
    for receipt in receipts {
        if !valid_tmux_name(&receipt.target_session) {
            continue;
        }
        let Ok(agent) = load_target_agent(&receipt.target) else {
            // agent registry 不可证明时保持哨兵，交给 status/人工修复；不能按目录名猜所有权。
            continue;
        };
        if crate::session::resume_store::takeover_marker_path(&agent, &receipt.target_session)
            .exists()
            || crate::session::tmux::has_session_strict(&receipt.target_session)?
        {
            continue;
        }
        if store.with_state(|state| {
            state
                .release_managed_takeover(&receipt)
                .map_err(anyhow::Error::new)
        })? {
            released = released.saturating_add(1);
        }
    }
    Ok(released)
}

fn load_target_agent(target: &ScopedChatKey) -> Result<Agent> {
    let config = summon_core::config::Config::load();
    let (registry, _) = summon_core::registry::Registry::load_tolerant(&config.agents_dir)
        .context(summon_i18n::text!("rea8b704388"))?;
    registry
        .get(&target.agent)
        .cloned()
        .context(summon_i18n::text!("r0cd0a8d3a0"))
}

fn prepare_resume_preflight(attempt: &HandoffAttempt) -> Result<StrictResumePreflight> {
    let agent = load_target_agent(attempt.target())?;
    let target = HandoffTargetProfile::for_agent(
        &agent,
        attempt.target_chat_type(),
        &attempt.target().chat_id,
        attempt.target_tmux(),
    )?;
    let runtime = VerifiedTargetRuntime::verify(&target)?;
    StrictResumePreflight::build(&runtime, attempt, &target)
}

fn prepare_resume_plan(
    attempt: &HandoffAttempt,
    source_exit: &SourceExitProof,
) -> Result<StrictResumePlan> {
    let agent = load_target_agent(attempt.target())?;
    let target = HandoffTargetProfile::for_agent(
        &agent,
        attempt.target_chat_type(),
        &attempt.target().chat_id,
        attempt.target_tmux(),
    )?;
    let runtime = VerifiedTargetRuntime::verify(&target)?;
    StrictResumePlan::build(&runtime, attempt, source_exit, &target)
}

fn exact_source_identity(
    state: &summon_core::bridge::BridgeState,
    attempt: &HandoffAttempt,
) -> Option<u64> {
    let record = state.session(attempt.source().session())?;
    let process = record.process.as_ref()?;
    if record.generation != attempt.source().generation()
        || process != attempt.source().process()
        || record.locator.key != *attempt.source().session()
    {
        return None;
    }
    let identity = locator_identity_revision(&record.locator, process);
    (identity == attempt.source().locator_identity_revision).then_some(identity)
}

fn prove_source_exit(
    store: &crate::bridge::store::BridgeStore,
    attempt: &mut HandoffAttempt,
    state_before: &summon_core::bridge::BridgeState,
) -> Result<()> {
    let Some(identity_before) = exact_source_identity(state_before, attempt) else {
        bail!(summon_i18n::format!("rc9a3ce5658"));
    };
    let first = sample_transcript(
        Path::new(&attempt.source().locator().transcript_path),
        Path::new(&attempt.source().profile().config_dir),
        &attempt.source().session().session_id,
    )?;
    std::thread::sleep(MIN_STABILITY);
    let second = sample_transcript(
        Path::new(&attempt.source().locator().transcript_path),
        Path::new(&attempt.source().profile().config_dir),
        &attempt.source().session().session_id,
    )?;
    let state_after = store
        .load_state()?
        .context(summon_i18n::text!("r941463fcf9"))?;
    let Some(identity_after) = exact_source_identity(&state_after, attempt) else {
        bail!(summon_i18n::format!("r4c57be3a69"));
    };
    let proof = verify_source_exit(
        attempt,
        SourceExitEvidence {
            process_before: ProcessCheck::Gone,
            process_after: check_source_process(attempt.source().process()),
            locator_identity_revision_before: identity_before,
            locator_identity_revision_after: identity_after,
            termination: SourceTerminationEvidence::ProcessDisappeared,
            first,
            second,
            elapsed: MIN_STABILITY,
        },
    )?;
    attempt.mark_source_exit(proof)?;
    store.persist_handoff_attempt(attempt)
}

fn fail_before_resume(
    store: &crate::bridge::store::BridgeStore,
    attempt: &mut HandoffAttempt,
) -> Result<()> {
    // 只有还能证明源 writer 正在运行时，取消 reservation 才会把控制权安全退还给源终端。
    // Gone/PID reuse/不可读都可能已跨过不可逆边界，必须保留 attempt/lease 供后续证明或人工处理。
    if check_source_process(attempt.source().process()) != ProcessCheck::AliveExact {
        bail!(summon_i18n::format!("r4ee1bc6fdf"));
    }
    store.with_state(|state| {
        state
            .cancel_ticket(attempt.ticket_id())
            .map_err(anyhow::Error::new)
    })?;
    attempt.phase = HandoffPhase::Failed;
    store.persist_handoff_attempt(attempt)
}

fn fail_resuming(
    store: &crate::bridge::store::BridgeStore,
    attempt: &mut HandoffAttempt,
    staging: &TmuxSessionIdentity,
    reason: &str,
) -> Result<()> {
    let proof = match attempt.phase() {
        HandoffPhase::Resuming { proof, .. } => proof.clone(),
        _ => bail!(summon_i18n::format!("rf3d6bdb483")),
    };
    if crate::session::tmux::has_session_strict(&staging.name)? {
        OwnedHandoffStaging::recover(staging)
            .with_context(|| summon_i18n::format!("r1758a2cc51"))?
            .cleanup()
            .with_context(|| summon_i18n::format!("rbedb67f85a"))?;
    }
    schedule_resume_retry(store, attempt, proof, reason, RESUME_LAUNCH_RETRY_DELAY_MS)
}

fn schedule_resume_retry(
    store: &crate::bridge::store::BridgeStore,
    attempt: &mut HandoffAttempt,
    proof: SourceExitProof,
    reason: &str,
    delay_ms: u64,
) -> Result<()> {
    let current_proof = match attempt.phase() {
        HandoffPhase::ResumePrepared { proof, .. } | HandoffPhase::Resuming { proof, .. } => proof,
        _ => bail!(summon_i18n::format!("r15707318e9")),
    };
    if current_proof != &proof {
        bail!(summon_i18n::format!("r2189345af1"));
    }
    attempt.phase = HandoffPhase::SourceExitProved {
        proof,
        last_resume_error: None,
        retry_at_ms: 0,
    };
    attempt.defer_resume(
        &anyhow::anyhow!(reason.to_string()),
        now_ms().saturating_add(delay_ms),
    )?;
    log(&summon_i18n::format!("reeea9ed8eb", attempt.attempt_id()));
    store.persist_handoff_attempt(attempt)
}

fn finalize_takeover(
    store: &crate::bridge::store::BridgeStore,
    attempt: &mut HandoffAttempt,
    proof: &HandoffAckProof,
) -> Result<()> {
    let agent = load_target_agent(attempt.target())?;
    anyhow::ensure!(
        HandoffTargetProfile::agent_is_eligible(&agent)
            && HandoffTargetProfile::provider_mode(&agent) == proof.target_provider
            && agent.tmux_name_for_chat(
                attempt.target_chat_type(),
                &attempt.target().chat_id,
                false,
            ) == attempt.target_tmux(),
        summon_i18n::format!("r256901ab67")
    );
    let actor = store
        .load_state()?
        .and_then(|state| {
            state
                .slot(attempt.target())
                .and_then(summon_core::bridge::ChatSlot::binding)
                .map(|binding| binding.actor_open_id.clone())
        })
        .context(summon_i18n::text!("rda5cec6256"))?;
    crate::session::resume_store::write_checked(
        &agent,
        attempt.target_tmux(),
        "claude",
        &proof.session.session_id,
        Some(&attempt.source().locator().resume_lookup_cwd),
        None,
    )?;
    crate::session::resume_store::mark_takeover(&agent, attempt.target_tmux(), &proof.session)?;
    crate::runtime::daemon::write_chat_meta_checked(
        &agent,
        attempt.target_tmux(),
        &attempt.target().chat_id,
        attempt.target_chat_type(),
        None,
        &actor,
        Some(&actor),
    )?;
    // Takeover 成功 = 新的路由生命周期开始;bridge 不过 admission,残留的 `closed` 必须在这里清掉。
    crate::session::lifecycle::mark_open(
        &agent,
        &summon_core::registry::chat_tag(attempt.target_chat_type(), &attempt.target().chat_id),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    store.with_state(|state| {
        state
            .complete_handoff_takeover(
                attempt.ticket_id(),
                attempt.binding_id(),
                attempt.target_tmux(),
                proof.resumed_generation,
                &proof.resumed_process,
                &proof.session_start_event_id,
            )
            .map_err(anyhow::Error::new)
    })?;
    attempt.mark_completed()?;
    store.persist_handoff_attempt(attempt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_handoff_never_calls_generic_session_launch_paths() {
        let source = include_str!("handoff.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("handoff production source");
        for forbidden in ["ensure_started(", "cold_start(", "augment_launch("] {
            assert!(
                !production.contains(forbidden),
                "strict handoff 不得调用会 fallback/cold-start/注入 agent 配置的通用路径:{forbidden}"
            );
        }
    }

    /// Claude 的启动参数有**两条**构造路径:交互会话的 `Session::augment_launch`,与 bridge
    /// 接管目标的 `HandoffTargetProfile::for_agent`。后者镜像前者时曾漏掉 `--settings`,于是
    /// 经 `/bridge` 拉起的会话整条没有 PreToolUse 执行闸。两条都必须落盘并注入同一份 settings。
    ///
    /// 为什么是源码断言而不是行为测试:`for_agent` 要一个 eligible 的真 agent、可写的
    /// system-prompt 目录与完整 registry/config,单测里造这些的成本远高于它能证明的东西;
    /// 而漂移的形态恰恰是「某条路径少了一行」——源码断言正好盯得住。
    #[test]
    fn both_claude_launch_paths_inject_tool_guard_settings() {
        for (name, source) in [
            ("bridge/handoff.rs", include_str!("handoff.rs")),
            ("session/session.rs", include_str!("../session/session.rs")),
        ] {
            let production = source
                .split("#[cfg(test)]")
                .next()
                .expect("production source");
            assert!(
                production.contains(concat!("write_guard_", "settings(")),
                "{name} 必须调用共用的 settings 落盘函数,不许各写一份"
            );
            assert!(
                production.contains("--settings"),
                "{name} 必须把 tool-guard settings 注入 Claude 启动参数"
            );
        }
    }

    /// bridge 接管的 PostCompact 按它启动时同一套解析判:目标环境的 PATH、跳过脚本 wrapper
    /// (`resolve_standard_claude` 用的就是这份 env 与这个解析函数)。源码断言:行为测试要一个
    /// eligible 的真 agent 与完整 registry/config,而要防的漂移是「探测换了一套解析」,源码断言足以
    /// 覆盖。
    #[test]
    fn bridge_asks_post_compact_of_the_claude_it_will_launch() {
        let source = include_str!("handoff.rs");
        let body = &source[source.find("pub fn for_agent(").unwrap()..];
        let body = &body[..body.find("\n    }\n").unwrap()];
        let env_built = body.find("env.extend(extra_env);").unwrap();
        let resolved = body
            .find("resolve_standard_claude_in_path(path)")
            .expect("按启动时的解析取程序");
        let written = body.find(concat!("write_guard_", "settings(")).unwrap();
        assert!(env_built < resolved && resolved < written);
        assert!(body[written..].contains("program.as_deref()"));
    }

    struct Fixture {
        _root: tempfile::TempDir,
        source: HandoffSourceSnapshot,
        attempt: HandoffAttempt,
        runtime: VerifiedTargetRuntime,
        target: HandoffTargetProfile,
        first: TranscriptSample,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home 空格");
        let config = home.join(".claude");
        let cwd = root.path().join("project 中文");
        let transcript_dir = config.join("projects/demo");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = transcript_dir.join("sid.jsonl");
        std::fs::write(
            &transcript,
            b"{\"type\":\"assistant\",\"sessionId\":\"sid\",\"message\":{}}\n",
        )
        .unwrap();
        let binary = root.path().join("claude fake");
        crate::test_support::write_test_executable(&binary, "#!/bin/sh\nexec /bin/cat\n");
        let binary = std::fs::canonicalize(binary).unwrap();
        let config = std::fs::canonicalize(config).unwrap();
        let home = std::fs::canonicalize(home).unwrap();
        let cwd = std::fs::canonicalize(cwd).unwrap();
        let transcript = std::fs::canonicalize(transcript).unwrap();
        let session = SessionKey::new(config.to_string_lossy(), "sid");
        let process = ProcessFingerprint {
            pid: 41,
            start_time: "start-source".into(),
            executable: binary.to_string_lossy().into_owned(),
        };
        let profile = ResumeProfile {
            claude_bin: binary.to_string_lossy().into_owned(),
            home: home.to_string_lossy().into_owned(),
            config_dir: config.to_string_lossy().into_owned(),
            version: "2.1.207".into(),
            safe_args: vec!["--model".into(), "sonnet".into()],
            unsupported_args: vec![],
            process_verified: true,
            environment_verified: true,
            config_source: summon_core::bridge::ClaudeConfigSource::Default,
            provider: ClaudeProvider::Default,
        };
        let locator = ExternalLocator {
            key: session.clone(),
            transcript_path: transcript.to_string_lossy().into_owned(),
            resume_lookup_cwd: cwd.to_string_lossy().into_owned(),
            current_cwd: cwd.to_string_lossy().into_owned(),
            profile: profile.clone(),
            revision: 7,
        };
        let identity = locator_identity_revision(&locator, &process);
        let source =
            HandoffSourceSnapshot::new(session, 3, locator, process, profile, identity).unwrap();
        let attempt = HandoffAttempt::new(
            uuid::Uuid::new_v4().to_string(),
            "ticket".into(),
            "binding".into(),
            9,
            ScopedChatKey::new("app", "agent", "group"),
            "group".into(),
            "summon-bridge-test-target".into(),
            ClaudeProvider::Default,
            source.clone(),
            1,
        )
        .unwrap();
        let system_prompt_path = root.path().join("target-system-prompt.txt");
        std::fs::write(&system_prompt_path, "TARGET SOUL").unwrap();
        let target = HandoffTargetProfile {
            agent_name: "agent".into(),
            args: vec![
                "--model".into(),
                "opus".into(),
                "--permission-mode".into(),
                "bypassPermissions".into(),
                "--name".into(),
                "summon:summon-bridge-test-target".into(),
                "--append-system-prompt-file".into(),
                system_prompt_path.to_string_lossy().into_owned(),
            ],
            env: vec![
                ("HOME".into(), home.to_string_lossy().into_owned()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ],
            provider: ClaudeProvider::Default,
            system_prompt_path,
        };
        let runtime = VerifiedTargetRuntime {
            program: binary,
            provider: ClaudeProvider::Default,
            home: home.clone(),
            config: config.clone(),
        };
        let first = sample_transcript(
            Path::new(&source.locator.transcript_path),
            Path::new(&source.profile.config_dir),
            &source.session.session_id,
        )
        .unwrap();
        Fixture {
            _root: root,
            source,
            attempt,
            runtime,
            target,
            first,
        }
    }

    fn source_exit(fixture: &Fixture) -> SourceExitProof {
        verify_source_exit(
            &fixture.attempt,
            SourceExitEvidence {
                process_before: ProcessCheck::Gone,
                process_after: ProcessCheck::Gone,
                locator_identity_revision_before: fixture.source.locator_identity_revision,
                locator_identity_revision_after: fixture.source.locator_identity_revision,
                termination: SourceTerminationEvidence::ProcessDisappeared,
                first: fixture.first.clone(),
                second: fixture.first.clone(),
                elapsed: MIN_STABILITY,
            },
        )
        .unwrap()
    }

    #[test]
    fn remote_takeover_waits_for_busy_turn_then_requests_exact_termination() {
        assert_eq!(
            remote_takeover_readiness(
                summon_core::bridge::ExternalLifecycle::ActiveBusy,
                ProcessCheck::AliveExact,
            )
            .unwrap(),
            RemoteTakeoverReadiness::WaitForSafeBoundary
        );
        assert_eq!(
            remote_takeover_readiness(
                summon_core::bridge::ExternalLifecycle::ActiveIdle,
                ProcessCheck::AliveExact,
            )
            .unwrap(),
            RemoteTakeoverReadiness::RequestTermination
        );
        assert_eq!(
            remote_takeover_readiness(
                summon_core::bridge::ExternalLifecycle::Ended,
                ProcessCheck::Gone,
            )
            .unwrap(),
            RemoteTakeoverReadiness::ProveSourceExit
        );
        assert!(remote_takeover_readiness(
            summon_core::bridge::ExternalLifecycle::Conflict,
            ProcessCheck::AliveExact,
        )
        .is_err());
    }

    struct ChildGuard(std::process::Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            // 只回收本测试创建的进程，断言失败也不能把它留在后台。
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn sleep_source(program: &Path) -> (ChildGuard, ProcessFingerprint) {
        let mut child = ChildGuard(
            std::process::Command::new(program)
                .arg("300")
                .spawn()
                .unwrap(),
        );
        let executable = std::fs::canonicalize("/bin/sleep").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        // 并行冷跑时 spawn 返回后仍可能读到 exec 前的测试程序映像。先证明 sleep 已就绪，
        // 再冻结待复验的指纹；生产身份闸照旧拒绝任何变化，不能为夹具放宽它。
        loop {
            assert!(child.0.try_wait().unwrap().is_none(), "sleep 夹具提前退出");
            let observed = crate::bridge::process_probe::observe_exact_fingerprint(child.0.id());
            if let Ok(Some(ref process)) = observed {
                if Path::new(&process.executable) == executable {
                    return (child, process.clone());
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sleep 夹具未在期限内完成 exec: {observed:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn exact_termination_refuses_identity_drift_without_signalling() {
        let (mut child, observed) = sleep_source(Path::new("/bin/sleep"));
        let mut drifted = observed.clone();
        drifted.start_time.push_str("-drift");
        let guard = SourceTranscriptGuard::new(
            Path::new("/not-read-for-drifted-identity"),
            Path::new("/not-read-for-drifted-identity"),
            "sid",
        );
        assert!(signal_exact_source_termination(&drifted, &guard, || Ok(())).is_err());
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "身份漂移时不得发送信号"
        );
    }

    /// 回归(2026-07-21 现场):活跃接管在 macOS 上必失败,报「源 Claude 进程身份已漂移」。
    /// 根因是同一条安全链上有两套指纹实现,对**经符号链接启动**的进程给出不同的 executable:
    /// `process_probe`(卡片渲染时落盘的 expected,也是 `probe_exact_process` 用的)在 macOS 走
    /// `proc_pidpath`,拿到内核解析后的真实映像;而 `check_source_process` 另起一套 `ps -o
    /// command=` 只取首 token,拿到的是 argv[0] = 启动用的软链路径。
    /// 标准安装 `~/.local/bin/claude -> ~/.local/share/claude/versions/<ver>` 恰好就是软链,于是
    /// 前一步判 AliveExact 放行、后一步立刻判 PidReused 拒发信号——接管 100% 卡死。
    /// 用软链启动 `/bin/sleep` 精确复现:两处必须对同一进程给出同一身份。
    #[test]
    fn exact_source_check_agrees_with_probe_for_symlink_launched_process() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("napper");
        std::os::unix::fs::symlink("/bin/sleep", &link).unwrap();
        let (child, expected) = sleep_source(&link);
        let check = check_source_process(&expected);
        drop(child);
        assert_eq!(
            check,
            ProcessCheck::AliveExact,
            "杀源前的 exact 复验必须与卡片落盘时的指纹实现同源,否则软链启动的进程永远被误判漂移"
        );
    }

    #[test]
    fn exact_termination_refuses_unreadable_resume_asset_without_signalling() {
        let (mut child, observed) = sleep_source(Path::new("/bin/sleep"));
        let guard = SourceTranscriptGuard::new(
            Path::new("/missing-recall-transcript"),
            Path::new("/missing-recall-config"),
            "sid",
        );
        assert!(signal_exact_source_termination(&observed, &guard, || Ok(())).is_err());
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "resume 资产不可读时不得发送信号"
        );
    }

    #[test]
    fn handoff_attempt_persists_remote_termination_intent_before_signal() {
        let mut intent_fixture = fixture();
        assert!(matches!(
            intent_fixture.attempt.phase(),
            HandoffPhase::AwaitingSafeBoundary
        ));
        intent_fixture
            .attempt
            .mark_termination_requested(100)
            .unwrap();
        assert!(matches!(
            intent_fixture.attempt.phase(),
            HandoffPhase::SourceTerminationRequested {
                requested_at_ms: 100
            }
        ));
        intent_fixture
            .attempt
            .mark_source_signal_sent(120, 10_120)
            .unwrap();
        assert!(matches!(
            intent_fixture.attempt.phase(),
            HandoffPhase::AwaitingSourceExit {
                signal_sent_at_ms: Some(120),
                deadline_ms: Some(10_120),
            }
        ));
        let json = serde_json::to_string(&intent_fixture.attempt).unwrap();
        let restored: HandoffAttempt = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, intent_fixture.attempt);
        let legacy: HandoffPhase =
            serde_json::from_str(r#"{"state":"awaiting_source_exit"}"#).unwrap();
        assert!(matches!(
            legacy,
            HandoffPhase::AwaitingSourceExit {
                signal_sent_at_ms: None,
                deadline_ms: None,
            }
        ));

        let mut retry_fixture = fixture();
        let proof = source_exit(&retry_fixture);
        retry_fixture.attempt.phase = HandoffPhase::SourceExitProved {
            proof,
            last_resume_error: Some("old retry".into()),
            retry_at_ms: 99,
        };
        let mut legacy_json = serde_json::to_value(&retry_fixture.attempt).unwrap();
        let phase = legacy_json["phase"].as_object_mut().unwrap();
        phase.remove("last_resume_error");
        phase.remove("retry_at_ms");
        let restored: HandoffAttempt = serde_json::from_value(legacy_json).unwrap();
        assert!(matches!(
            restored.phase(),
            HandoffPhase::SourceExitProved {
                last_resume_error: None,
                retry_at_ms: 0,
                ..
            }
        ));
    }

    #[test]
    fn coordinator_keeps_source_alive_when_resume_preflight_fails() {
        use summon_core::bridge::{
            BridgeChoice, BridgeMode, BridgeTicket, ChatBinding, ChatSlot, ExternalSessionRecord,
            SessionLease, TicketStatus,
        };

        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let config = home.join(".claude");
        let cwd = root.path().join("project");
        let transcript = config.join("projects/demo/remote-sid.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(&transcript, "{\"sessionId\":\"remote-sid\"}\n").unwrap();
        let (mut child, process) = sleep_source(Path::new("/bin/sleep"));
        let home = std::fs::canonicalize(home).unwrap();
        let config = std::fs::canonicalize(config).unwrap();
        let cwd = std::fs::canonicalize(cwd).unwrap();
        let transcript = std::fs::canonicalize(transcript).unwrap();
        let session = SessionKey::new(config.to_string_lossy(), "remote-sid");
        let profile = ResumeProfile {
            claude_bin: process.executable.clone(),
            home: home.to_string_lossy().into_owned(),
            config_dir: config.to_string_lossy().into_owned(),
            version: "test-version".into(),
            unsupported_args: vec!["<unsupported-flag>".into(), "<positional>".into()],
            process_verified: true,
            environment_verified: true,
            config_source: ClaudeConfigSource::Default,
            provider: ClaudeProvider::Default,
            ..ResumeProfile::default()
        };
        let locator = ExternalLocator {
            key: session.clone(),
            transcript_path: transcript.to_string_lossy().into_owned(),
            resume_lookup_cwd: cwd.to_string_lossy().into_owned(),
            current_cwd: cwd.to_string_lossy().into_owned(),
            profile: profile.clone(),
            revision: 1,
        };
        let target = ScopedChatKey::new("scope", "missing-test-agent", "oc_remote");
        let binding = ChatBinding {
            id: "binding-remote".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: session.clone(),
            mode: BridgeMode::Handoff,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            revision: 7,
        };
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        store
            .with_state(|state| {
                state.upsert_session(ExternalSessionRecord {
                    key: session.clone(),
                    generation: 1,
                    locator: locator.clone(),
                    process: Some(process.clone()),
                    lifecycle: ExternalLifecycle::ActiveIdle,
                    first_seen_ms: 1,
                    last_seen_ms: 1,
                    last_applied_sequence: 1,
                    pending_prompt: None,
                });
                state.slots.insert(
                    target.storage_key(),
                    ChatSlot::HandoffPending(binding.clone()),
                );
                state.session_leases.insert(
                    session.storage_key(),
                    SessionLease {
                        binding_id: binding.id.clone(),
                        target: target.clone(),
                    },
                );
                state.tickets.insert(
                    "ticket-remote".into(),
                    BridgeTicket {
                        id: "ticket-remote".into(),
                        short_code: None,
                        target: target.clone(),
                        target_chat_type: "group".into(),
                        target_provider: ClaudeProvider::Default,
                        requester_open_id: "ou_owner".into(),
                        requester_union_id: String::new(),
                        control_card_message_id: Some("om_card".into()),
                        private_chat_id: Some("oc_private".into()),
                        slot_revision: binding.revision,
                        choices: vec![BridgeChoice {
                            id: "choice-remote".into(),
                            session: session.clone(),
                            registry_revision: locator.revision,
                        }],
                        issued_at_ms: 1,
                        expires_at_ms: u64::MAX,
                        status: TicketStatus::Activating {
                            binding_id: binding.id.clone(),
                            choice_id: "choice-remote".into(),
                            mode: BridgeMode::Handoff,
                        },
                    },
                );
                Ok(())
            })
            .unwrap();
        let identity_revision = locator_identity_revision(&locator, &process);
        let source = HandoffSourceSnapshot::new(
            session,
            1,
            locator.clone(),
            process,
            profile,
            identity_revision,
        )
        .unwrap();
        let attempt = HandoffAttempt::new(
            uuid::Uuid::new_v4().to_string(),
            "ticket-remote".into(),
            binding.id.clone(),
            binding.revision,
            target,
            "group".into(),
            "soul-missing-test-agent-gremote".into(),
            ClaudeProvider::Default,
            source,
            1,
        )
        .unwrap();
        store.persist_handoff_attempt(&attempt).unwrap();
        let outbox = crate::runtime::outbox_store::OutboxStore::new(root.path().join("outbox"));

        coordinator_tick(&store, &outbox).unwrap();
        assert!(matches!(
            store.load_handoff_attempts().unwrap()[0].phase(),
            HandoffPhase::SourceTerminationRequested { .. }
        ));
        let stats = coordinator_tick(&store, &outbox).unwrap();
        assert_eq!(stats.source_signals, 0);
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(matches!(
            store.load_handoff_attempts().unwrap()[0].phase(),
            HandoffPhase::Failed
        ));
        assert_eq!(
            std::fs::read_dir(root.path().join("outbox/pending"))
                .unwrap()
                .count(),
            2,
            "等待与预备终止两个状态必须先落 durable outbox"
        );
    }

    #[test]
    fn source_exit_plan_failure_keeps_attempt_and_binding_for_retry() {
        use summon_core::bridge::{BridgeMode, ChatBinding, ChatSlot};

        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(root.path().join("outbox"));
        let mut fixture = fixture();
        fixture.attempt.target.agent = format!("missing-{}", uuid::Uuid::new_v4().simple());
        let binding = ChatBinding {
            id: fixture.attempt.binding_id.clone(),
            target: fixture.attempt.target.clone(),
            target_chat_type: fixture.attempt.target_chat_type.clone(),
            session: fixture.source.session.clone(),
            mode: BridgeMode::Handoff,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            revision: fixture.attempt.binding_revision,
        };
        store
            .with_state(|state| {
                state.slots.insert(
                    fixture.attempt.target.storage_key(),
                    ChatSlot::HandoffPending(binding),
                );
                Ok(())
            })
            .unwrap();
        fixture.attempt.phase = HandoffPhase::SourceExitProved {
            proof: source_exit(&fixture),
            last_resume_error: None,
            retry_at_ms: 0,
        };
        store.persist_handoff_attempt(&fixture.attempt).unwrap();

        let stats = coordinator_tick(&store, &outbox).unwrap();
        assert_eq!(stats.resume_retries, 1);
        let attempt = &store.load_handoff_attempts().unwrap()[0];
        assert!(matches!(
            attempt.phase(),
            HandoffPhase::SourceExitProved {
                last_resume_error: Some(reason),
                retry_at_ms,
                ..
            // 本机没有 agents 目录时(CI)报的是读 registry 失败,两种都是目标 agent 加载不了。
            } if reason.contains("Takeover 目标 agent") && *retry_at_ms > 0
        ));
        assert!(matches!(
            store.load_state().unwrap().unwrap().slot(attempt.target()),
            Some(ChatSlot::HandoffPending(_))
        ));
        let retry_at = match attempt.phase() {
            HandoffPhase::SourceExitProved { retry_at_ms, .. } => *retry_at_ms,
            _ => unreachable!(),
        };
        let stats = coordinator_tick(&store, &outbox).unwrap();
        assert_eq!(stats.resume_retries, 0, "backoff 内不得热循环重试");
        assert!(matches!(
            store.load_handoff_attempts().unwrap()[0].phase(),
            HandoffPhase::SourceExitProved { retry_at_ms, .. } if *retry_at_ms == retry_at
        ));
    }

    #[test]
    fn cancellation_requires_source_writer_still_alive_exact() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let mut fixture = fixture();
        fixture.attempt.source.process = ProcessFingerprint {
            pid: 0,
            start_time: String::new(),
            executable: String::new(),
        };
        assert_eq!(
            check_source_process(fixture.attempt.source().process()),
            ProcessCheck::Unreadable
        );
        assert!(fail_before_resume(&store, &mut fixture.attempt).is_err());
        assert!(matches!(
            fixture.attempt.phase(),
            HandoffPhase::AwaitingSafeBoundary
        ));
    }

    #[test]
    fn prepared_resume_failure_regenerates_plan_instead_of_failing() {
        use summon_core::bridge::{BridgeMode, ChatBinding, ChatSlot};

        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let mut fixture = fixture();
        let proof = source_exit(&fixture);
        let launch =
            StrictResumePlan::build(&fixture.runtime, &fixture.attempt, &proof, &fixture.target)
                .unwrap()
                .launch_identity();
        let binding = ChatBinding {
            id: fixture.attempt.binding_id.clone(),
            target: fixture.attempt.target.clone(),
            target_chat_type: fixture.attempt.target_chat_type.clone(),
            session: fixture.source.session.clone(),
            mode: BridgeMode::Handoff,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            revision: fixture.attempt.binding_revision,
        };
        store
            .with_state(|state| {
                state.slots.insert(
                    fixture.attempt.target.storage_key(),
                    ChatSlot::HandoffPending(binding),
                );
                Ok(())
            })
            .unwrap();
        fixture.attempt.phase = HandoffPhase::ResumePrepared {
            proof: proof.clone(),
            staging_name: format!("{STAGING_PREFIX}{}", fixture.attempt.attempt_id()),
            ownership_marker: "a".repeat(64),
            token_digest: HandoffToken::generate().digest(),
            ack_deadline_ms: 100,
            launch,
        };
        store.persist_handoff_attempt(&fixture.attempt).unwrap();

        schedule_resume_retry(
            &store,
            &mut fixture.attempt,
            proof,
            "handoff staging launch 未提交，重新准备恢复",
            RESUME_LAUNCH_RETRY_DELAY_MS,
        )
        .unwrap();
        assert!(matches!(
            store.load_handoff_attempts().unwrap()[0].phase(),
            HandoffPhase::SourceExitProved {
                last_resume_error: Some(reason),
                retry_at_ms,
                ..
            } if reason.contains("staging launch 未提交") && *retry_at_ms > 0
        ));
        assert!(matches!(
            store
                .load_state()
                .unwrap()
                .unwrap()
                .slot(fixture.attempt.target()),
            Some(ChatSlot::HandoffPending(_))
        ));
    }

    #[test]
    fn terminal_notice_archive_allows_attempt_cleanup_without_reenqueue() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(root.path().join("outbox"));
        let mut fixture = fixture();
        fixture.attempt.phase = HandoffPhase::Completed;
        store.persist_handoff_attempt(&fixture.attempt).unwrap();
        let ack_path = store.handoff_ack_path(fixture.attempt.attempt_id());
        crate::runtime::fsio::atomic_write_mode(&ack_path, b"terminal-ack-metadata", 0o600)
            .unwrap();

        coordinator_tick(&store, &outbox).unwrap();
        assert_eq!(outbox.drain_tick(u64::MAX, &|_| Ok(())), 1);
        coordinator_tick(&store, &outbox).unwrap();
        assert!(store.load_handoff_attempts().unwrap().is_empty());
        assert!(
            !ack_path.exists(),
            "terminal attempt 不得留下 ACK 元数据孤儿"
        );
        assert_eq!(outbox.drain_tick(u64::MAX, &|_| Ok(())), 0);
    }

    #[test]
    fn target_provider_preflight_is_fail_closed_without_copying_credentials() {
        let mut fixture = fixture();
        fixture.target.provider = ClaudeProvider::Custom;
        fixture.target.env.push((
            "ANTHROPIC_AUTH_TOKEN".into(),
            "target-secret-never-persisted".into(),
        ));
        assert!(verify_target_provider_ready(
            &fixture.target,
            &fixture.runtime.program,
            Duration::from_millis(100),
        )
        .is_ok());
        fixture
            .target
            .env
            .retain(|(key, _)| key != "ANTHROPIC_AUTH_TOKEN");
        assert!(verify_target_provider_ready(
            &fixture.target,
            &fixture.runtime.program,
            Duration::from_millis(100),
        )
        .is_err());
        fixture.target.env.extend([
            ("ANTHROPIC_AUTH_TOKEN".into(), "target-secret".into()),
            ("CLAUDE_CODE_USE_BEDROCK".into(), "1".into()),
        ]);
        assert!(verify_target_provider_ready(
            &fixture.target,
            &fixture.runtime.program,
            Duration::from_millis(100),
        )
        .is_err());
    }

    #[test]
    fn default_provider_preflight_requires_logged_in_json_and_is_bounded() {
        let fixture = fixture();
        let root = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = root.path().join(name);
            crate::test_support::write_test_executable(&path, &format!("#!/bin/sh\n{body}\n"));
            path
        };
        // 前两段验的是「JSON 判定」,不是「够不够快」——超时必须给足,否则并行测试负载下
        // fixture 子进程 spawn 超过 1s 就把本测试变成 flaky(实测连跑 5 次出现过 1.90s)。
        // 更坏的是 logged_out 那段:超时同样让 `is_err()` 通过,于是静默失去「loggedIn:false 被拒」
        // 这条覆盖。有界性由下面第三段用 50ms 单独钉,与这里的宽超时互不干扰。
        let verdict_timeout = Duration::from_secs(20);
        let logged_in = script("logged-in", "printf '%s' '{\"loggedIn\":true}'");
        let result = verify_target_provider_ready(&fixture.target, &logged_in, verdict_timeout);
        assert!(result.is_ok(), "{result:?}");
        let logged_out = script("logged-out", "printf '%s' '{\"loggedIn\":false}'");
        let out = verify_target_provider_ready(&fixture.target, &logged_out, verdict_timeout);
        let msg = format!("{out:?}");
        assert!(out.is_err(), "loggedIn:false 应被拒");
        assert!(
            !msg.contains("超时"),
            "必须因判定失败而拒,不能因超时而拒: {msg}"
        );
        // 超时后子进程必须被收掉:每次都睡满 30s 的话两次尝试要 60s,10s 的上限给并行负载下
        // 两次 spawn 留足余量,同时仍远低于「没收掉」的耗时。
        let timeout = script("timeout", "exec /bin/sleep 30");
        let started = Instant::now();
        assert!(
            verify_target_provider_ready(&fixture.target, &timeout, Duration::from_millis(50),)
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// 超时只重试一次,判定失败不重试:一次偶发的冷启动慢不该让召回失败,而「未登录」重跑
    /// 也不会变,多跑只是拖长 dispatcher 上这一下同步阻塞。
    #[test]
    fn auth_status_retries_only_on_timeout() {
        use std::os::unix::process::ExitStatusExt;
        let finished = |code: i32, body: &str| {
            Some((
                std::process::ExitStatus::from_raw(code << 8),
                body.as_bytes().to_vec(),
            ))
        };
        let judge = |mut outcomes: Vec<Option<(std::process::ExitStatus, Vec<u8>)>>| {
            outcomes.reverse();
            let mut calls = 0;
            let result = judge_claude_auth_status(|| {
                calls += 1;
                Ok(outcomes.pop().expect("调用次数超出预期"))
            });
            (result, calls)
        };

        let (result, calls) = judge(vec![None, finished(0, r#"{"loggedIn":true}"#)]);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls, 2);

        let (result, calls) = judge(vec![None, None]);
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("连续 2 次超时"), "{error}");
        assert_eq!(calls, 2);

        let (result, calls) = judge(vec![finished(0, r#"{"loggedIn":false}"#)]);
        assert!(result.is_err());
        assert_eq!(calls, 1, "未登录不重试");

        let (result, calls) = judge(vec![finished(1, "")]);
        assert!(result.is_err());
        assert_eq!(calls, 1, "非零退出不重试");
    }

    /// PATH 里排在前面的 `#!` wrapper(真机上是 cmux 的 `claude` shim)必须被跳过:它 exec 之后
    /// 运行镜像就不是自己了,hook 报的 `claude_bin` 永远对不上,选中它等于起一个注定失败的 staging。
    #[test]
    fn path_scan_skips_shebang_wrappers_and_picks_the_native_claude() {
        let root = tempfile::tempdir().unwrap();
        let shim_dir = root.path().join("shims");
        let real_dir = root.path().join("bin");
        std::fs::create_dir_all(&shim_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        let install = |dir: &Path, body: &[u8]| {
            let path = dir.join("claude");
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::canonicalize(&path).unwrap()
        };
        let shim = install(&shim_dir, b"#!/usr/bin/env bash\nexec claude \"$@\"\n");
        let native = install(&real_dir, b"\xcf\xfa\xed\xfe not-a-script");
        let path = format!("{}:{}", shim_dir.display(), real_dir.display());
        assert_eq!(resolve_standard_claude_in_path(&path).unwrap(), native);

        // 只有 wrapper 时 fail-closed,并把被跳过的路径写进原因(路径可打,不含任何凭据)。
        let only_shim = resolve_standard_claude_in_path(&shim_dir.display().to_string())
            .expect_err("只有脚本 wrapper 必须拒");
        let text = format!("{only_shim:#}");
        assert!(
            text.contains(&shim.to_string_lossy().into_owned()),
            "{text}"
        );
        assert!(
            resolve_standard_claude_in_path(&root.path().display().to_string()).is_err(),
            "PATH 里没有 claude 也必须拒"
        );
    }

    #[test]
    fn target_profile_accepts_only_standard_claude_transcript_agent() {
        let mut standard = crate::test_support::agent_with("claude", "");
        standard.runtime.as_mut().unwrap().resume = Some(true);
        assert!(HandoffTargetProfile::agent_is_eligible(&standard));

        // claude 的 `headless: true` 在 2026-08-20 之后不再有独立落点(Subprocess 已删),
        // 它掉回 Transcript —— 于是也照样能被接管。判据始终是 `capture_mode() == Transcript`,
        // 不是这个标志,所以这里断言的是「标志已经改变不了资格」。
        let mut headless = standard.clone();
        headless.runtime.as_mut().unwrap().headless = Some(true);
        assert!(HandoffTargetProfile::agent_is_eligible(&headless));

        let mut resume_disabled = standard.clone();
        resume_disabled.runtime.as_mut().unwrap().resume = Some(false);
        assert!(!HandoffTargetProfile::agent_is_eligible(&resume_disabled));

        let mut custom_command = standard.clone();
        custom_command.runtime.as_mut().unwrap().cli_cmd = Some("/opt/custom-claude".into());
        assert!(!HandoffTargetProfile::agent_is_eligible(&custom_command));

        let cursor = crate::test_support::agent_with("cursor", "");
        assert!(!HandoffTargetProfile::agent_is_eligible(&cursor));

        let mut invalid_effort = standard.clone();
        invalid_effort.effort = Some("turbo".into());
        assert!(!HandoffTargetProfile::agent_is_eligible(&invalid_effort));

        let mut custom_provider = standard.clone();
        custom_provider.runtime.as_mut().unwrap().env = Some(
            [("ANTHROPIC_BASE_URL".into(), "https://proxy.invalid".into())]
                .into_iter()
                .collect(),
        );
        assert!(!HandoffTargetProfile::agent_is_eligible(&custom_provider));
        custom_provider
            .runtime
            .as_mut()
            .unwrap()
            .env
            .as_mut()
            .unwrap()
            .insert("ANTHROPIC_AUTH_TOKEN".into(), "target-token".into());
        assert!(HandoffTargetProfile::agent_is_eligible(&custom_provider));
        custom_provider
            .runtime
            .as_mut()
            .unwrap()
            .env
            .as_mut()
            .unwrap()
            .insert("CLAUDE_CODE_USE_BEDROCK".into(), "1".into());
        assert!(!HandoffTargetProfile::agent_is_eligible(&custom_provider));

        let mut unsafe_env = standard;
        unsafe_env.runtime.as_mut().unwrap().env =
            Some([("BAD-KEY".into(), "value".into())].into_iter().collect());
        assert!(!HandoffTargetProfile::agent_is_eligible(&unsafe_env));

        for key in [
            "SUMMON_AGENT",
            "SUMMON_CHAT_ID",
            "SUMMON_CONVERSATION_TAG",
            "SUMMON_CONTROL_SOCK",
            "SUMMON_FUTURE_INTERNAL",
            "FEISHU_APP_ID",
            "FEISHU_FUTURE_CREDENTIAL",
        ] {
            let mut identity_override = crate::test_support::agent_with("claude", "");
            identity_override.runtime.as_mut().unwrap().env =
                Some([(key.into(), "other".into())].into_iter().collect());
            assert!(
                !HandoffTargetProfile::agent_is_eligible(&identity_override),
                "takeover target 必须拒绝 runtime-owned env {key}"
            );
        }
    }

    #[test]
    fn exact_process_probe_distinguishes_alive_reused_and_unreadable() {
        let current = crate::bridge::process_probe::observe_exact_fingerprint(std::process::id())
            .unwrap()
            .expect("当前测试进程应可见");
        assert_eq!(check_source_process(&current), ProcessCheck::AliveExact);
        let mut wrong_start = current;
        wrong_start.start_time.push_str("-different");
        assert!(matches!(
            check_source_process(&wrong_start),
            ProcessCheck::PidReused(_)
        ));
        assert_eq!(
            check_source_process(&ProcessFingerprint {
                pid: 0,
                start_time: String::new(),
                executable: String::new(),
            }),
            ProcessCheck::Unreadable
        );
    }

    #[test]
    fn resume_plan_uses_source_sid_and_target_agent_profile() {
        let fixture = fixture();
        let mut attempt = fixture.attempt.clone();
        attempt.source.profile.version.clear();
        attempt.source.locator.profile.version.clear();
        let mut runtime = fixture.runtime.clone();
        runtime.program = std::fs::canonicalize("/bin/cat").unwrap();
        let plan =
            StrictResumePlan::build(&runtime, &attempt, &source_exit(&fixture), &fixture.target)
                .unwrap();
        assert_eq!(plan.program, runtime.program);
        assert_ne!(
            plan.program,
            PathBuf::from(&fixture.source.profile.claude_bin),
            "恢复必须使用目标 Bot 当前 Claude，而非源二进制"
        );
        assert_eq!(
            plan.args,
            [
                "--model",
                "opus",
                "--permission-mode",
                "bypassPermissions",
                "--name",
                "summon:summon-bridge-test-target",
                "--append-system-prompt-file",
                fixture.target.system_prompt_path.to_str().unwrap(),
                "--resume",
                "sid",
            ]
        );
        let debug = format!("{plan:?}");
        assert!(debug.contains("append-system-prompt-file"));
        assert!(debug.contains("summon:summon-bridge-test-target"));
        assert!(!debug.contains("TARGET SOUL"));
        assert!(!debug.contains("sonnet"), "不得继承源 model");
        assert!(!debug.contains(plan.token.as_str()));
        assert_eq!(
            plan.env
                .iter()
                .find(|(key, _)| key == crate::bridge::MANAGED_SESSION_ENV)
                .map(|(_, value)| value.as_str()),
            Some("1")
        );
        assert!(
            plan.env
                .iter()
                .all(|(key, _)| key != "CLAUDE_CONFIG_DIR"),
            "默认 Claude 配置必须保持 CLAUDE_CONFIG_DIR 缺席，显式注入 ~/.claude 会触发首次 onboarding"
        );
    }

    #[test]
    fn source_binary_and_version_do_not_control_resume_runtime() {
        let fixture = fixture();
        let mut attempt = fixture.attempt.clone();
        attempt.source.profile.claude_bin = "/source/version/that/no-longer-exists".into();
        attempt.source.profile.version = "source-version-is-diagnostic-only".into();
        attempt.source.locator.profile = attempt.source.profile.clone();
        let plan = StrictResumePlan::build(
            &fixture.runtime,
            &attempt,
            &source_exit(&fixture),
            &fixture.target,
        )
        .unwrap();
        assert_eq!(plan.program, fixture.runtime.program);
    }

    #[test]
    fn legacy_attempt_can_migrate_only_after_source_exit_proof() {
        let fixture = fixture();
        let proof = source_exit(&fixture);
        let mut legacy = fixture.attempt.clone();
        legacy.source.profile.config_source = ClaudeConfigSource::Unknown;
        legacy.source.profile.provider = ClaudeProvider::Unknown;
        legacy.source.locator.profile = legacy.source.profile.clone();
        legacy.phase = HandoffPhase::SourceExitProved {
            proof: proof.clone(),
            last_resume_error: Some("旧版失败原因".into()),
            retry_at_ms: 1,
        };
        let mut json = serde_json::to_value(&legacy).unwrap();
        json["source"]["profile"]
            .as_object_mut()
            .unwrap()
            .remove("config_source");
        json["source"]["profile"]
            .as_object_mut()
            .unwrap()
            .remove("provider");
        json["source"]["locator"]["profile"]
            .as_object_mut()
            .unwrap()
            .remove("config_source");
        json["source"]["locator"]["profile"]
            .as_object_mut()
            .unwrap()
            .remove("provider");
        json.as_object_mut().unwrap().insert(
            "capability_claude_bin".into(),
            serde_json::Value::String(fixture.source.profile.claude_bin.clone()),
        );
        json.as_object_mut().unwrap().insert(
            "capability_claude_version".into(),
            serde_json::Value::String("legacy-version".into()),
        );
        json.as_object_mut().unwrap().remove("target_provider");
        let restored: HandoffAttempt = serde_json::from_value(json).unwrap();
        let plan =
            StrictResumePlan::build(&fixture.runtime, &restored, &proof, &fixture.target).unwrap();
        assert!(plan.env.iter().all(|(key, _)| key != "CLAUDE_CONFIG_DIR"));

        let mut before_source_exit = restored;
        before_source_exit.phase = HandoffPhase::AwaitingSafeBoundary;
        assert!(StrictResumePreflight::build(
            &fixture.runtime,
            &before_source_exit,
            &fixture.target,
        )
        .is_err());
    }

    #[test]
    fn resume_env_contains_only_target_provider_values() {
        let mut fixture = fixture();
        fixture.target.provider = ClaudeProvider::Custom;
        fixture.attempt.target_provider = ClaudeProvider::Custom;
        fixture.target.env.extend([
            ("ANTHROPIC_BASE_URL".into(), "https://target.invalid".into()),
            ("ANTHROPIC_AUTH_TOKEN".into(), "target-token".into()),
            ("ANTHROPIC_MODEL".into(), "target-model".into()),
        ]);
        fixture.runtime.provider = ClaudeProvider::Custom;
        let plan = StrictResumePlan::build(
            &fixture.runtime,
            &fixture.attempt,
            &source_exit(&fixture),
            &fixture.target,
        )
        .unwrap();
        let env = plan
            .env
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("https://target.invalid")
        );
        assert_eq!(
            env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
            Some("target-token")
        );
        assert_eq!(
            env.get("ANTHROPIC_MODEL").map(String::as_str),
            Some("target-model")
        );
        assert!(!serde_json::to_string(&fixture.source)
            .unwrap()
            .contains("target-token"));
    }

    #[test]
    fn provider_drift_after_confirmation_is_rejected_before_source_signal() {
        let mut fixture = fixture();
        fixture.target.provider = ClaudeProvider::Custom;
        fixture.runtime.provider = ClaudeProvider::Custom;
        assert!(
            StrictResumePreflight::build(&fixture.runtime, &fixture.attempt, &fixture.target,)
                .is_err()
        );
    }

    #[test]
    fn source_exit_rejects_live_reused_unstable_and_changed_locator() {
        let fixture = fixture();
        let identity = fixture.source.locator_identity_revision;
        let evidence = |before, after, revision, second, elapsed| SourceExitEvidence {
            process_before: before,
            process_after: after,
            locator_identity_revision_before: identity,
            locator_identity_revision_after: revision,
            termination: SourceTerminationEvidence::ProcessDisappeared,
            first: fixture.first.clone(),
            second,
            elapsed,
        };
        assert!(verify_source_exit(
            &fixture.attempt,
            evidence(
                ProcessCheck::AliveExact,
                ProcessCheck::Gone,
                identity,
                fixture.first.clone(),
                MIN_STABILITY,
            ),
        )
        .is_err());
        assert!(verify_source_exit(
            &fixture.attempt,
            evidence(
                ProcessCheck::PidReused(fixture.source.process.clone()),
                ProcessCheck::Gone,
                identity,
                fixture.first.clone(),
                MIN_STABILITY,
            ),
        )
        .is_err());
        let mut changed = fixture.first.clone();
        changed.snapshot.len += 1;
        assert!(verify_source_exit(
            &fixture.attempt,
            evidence(
                ProcessCheck::Gone,
                ProcessCheck::Gone,
                identity,
                changed,
                MIN_STABILITY,
            ),
        )
        .is_err());
        assert!(verify_source_exit(
            &fixture.attempt,
            evidence(
                ProcessCheck::Gone,
                ProcessCheck::Gone,
                identity.wrapping_add(1),
                fixture.first.clone(),
                MIN_STABILITY,
            ),
        )
        .is_err());
        assert!(verify_source_exit(
            &fixture.attempt,
            evidence(
                ProcessCheck::Gone,
                ProcessCheck::Gone,
                identity,
                fixture.first.clone(),
                MIN_STABILITY - Duration::from_millis(1),
            ),
        )
        .is_err());
    }

    #[test]
    fn coordinator_source_identity_requires_same_generation_process_and_locator() {
        let fixture = fixture();
        let mut state = summon_core::bridge::BridgeState::default();
        state.sessions.insert(
            fixture.source.session.storage_key(),
            summon_core::bridge::ExternalSessionRecord {
                key: fixture.source.session.clone(),
                generation: fixture.source.generation,
                locator: fixture.source.locator.clone(),
                process: Some(fixture.source.process.clone()),
                lifecycle: summon_core::bridge::ExternalLifecycle::Ended,
                first_seen_ms: 1,
                last_seen_ms: 2,
                last_applied_sequence: 3,
                pending_prompt: None,
            },
        );
        assert_eq!(
            exact_source_identity(&state, &fixture.attempt),
            Some(fixture.source.locator_identity_revision)
        );

        state
            .sessions
            .get_mut(&fixture.source.session.storage_key())
            .unwrap()
            .locator
            .resume_lookup_cwd
            .push_str("/changed");
        assert_eq!(exact_source_identity(&state, &fixture.attempt), None);
    }

    #[test]
    fn transcript_sampling_rejects_partial_and_wrong_sid() {
        let fixture = fixture();
        let path = Path::new(&fixture.source.locator.transcript_path);
        std::fs::write(path, b"{\"sessionId\":\"sid\"}").unwrap();
        assert!(
            sample_transcript(path, Path::new(&fixture.source.profile.config_dir), "sid").is_err()
        );
        std::fs::write(path, b"{\"sessionId\":\"other\"}\n").unwrap();
        assert!(
            sample_transcript(path, Path::new(&fixture.source.profile.config_dir), "sid").is_err()
        );
    }

    fn ack_fixture() -> (
        Fixture,
        SourceExitProof,
        StrictResumePlan,
        HandoffAckObservation,
    ) {
        let fixture = fixture();
        let exit = source_exit(&fixture);
        let plan =
            StrictResumePlan::build(&fixture.runtime, &fixture.attempt, &exit, &fixture.target)
                .unwrap();
        let resumed = ProcessFingerprint {
            pid: 99,
            start_time: "start-resumed".into(),
            executable: plan.program.to_string_lossy().into_owned(),
        };
        let staging = TmuxSessionIdentity {
            name: plan.staging_name.clone(),
            session_id: "$42".into(),
            session_created: 100,
            pane_id: "%42".into(),
            ownership_marker: plan.ownership_marker.clone(),
        };
        let observation = HandoffAckObservation {
            token_digest: plan.token.digest(),
            source: "resume".into(),
            event_id: "event".into(),
            sequence: 10,
            session: plan.session.clone(),
            cwd: plan.work_dir.to_string_lossy().into_owned(),
            transcript_path: plan.transcript_path.to_string_lossy().into_owned(),
            process: resumed,
            profile: fixture.source.profile.clone(),
            launch: HandoffLaunchObservation {
                program: plan.program.to_string_lossy().into_owned(),
                args: plan.args.clone(),
            },
            tmux_pane: "%42".into(),
            staging,
        };
        (fixture, exit, plan, observation)
    }

    #[test]
    fn ack_accepts_only_exact_token_source_session_paths_process_and_staging() {
        let (_fixture, exit, plan, observation) = ack_fixture();
        assert!(verify_handoff_ack(
            &plan,
            &exit,
            observation.clone(),
            ProcessCheck::Gone,
            ProcessCheck::AliveExact,
        )
        .is_ok());

        let mut variants = Vec::new();
        let mut wrong = observation.clone();
        wrong.token_digest = HandoffToken::generate().digest();
        variants.push(wrong);
        let mut wrong = observation.clone();
        wrong.source = "startup".into();
        variants.push(wrong);
        let mut wrong = observation.clone();
        wrong.session.session_id = "other".into();
        variants.push(wrong);
        let mut wrong = observation.clone();
        wrong.cwd = "/tmp/other".into();
        variants.push(wrong);
        let mut wrong = observation.clone();
        wrong.process.executable = "/tmp/other".into();
        variants.push(wrong);
        let mut wrong = observation.clone();
        wrong.launch.args = vec!["--resume".into(), "other".into()];
        variants.push(wrong);
        let mut wrong = observation;
        wrong.tmux_pane = "%other".into();
        variants.push(wrong);
        for wrong in variants {
            assert!(verify_handoff_ack(
                &plan,
                &exit,
                wrong,
                ProcessCheck::Gone,
                ProcessCheck::AliveExact,
            )
            .is_err());
        }
    }

    #[test]
    fn ack_store_replaces_only_a_stale_retry_token() {
        let mut fixture = fixture();
        let store =
            crate::bridge::store::BridgeStore::new(fixture._root.path().join("bridge-ack-retry"));
        let exit = source_exit(&fixture);
        let old_plan =
            StrictResumePlan::build(&fixture.runtime, &fixture.attempt, &exit, &fixture.target)
                .unwrap();
        fixture.attempt.mark_source_exit(exit.clone()).unwrap();
        fixture
            .attempt
            .mark_resume_prepared(
                exit.clone(),
                old_plan.staging_name.clone(),
                old_plan.ownership_marker.clone(),
                old_plan.token.digest(),
                100,
                old_plan.launch_identity(),
            )
            .unwrap();
        store.persist_handoff_attempt(&fixture.attempt).unwrap();

        let attempt_id = fixture.attempt.attempt_id.clone();
        let session = fixture.source.session.clone();
        let transcript_path = fixture.source.locator.transcript_path.clone();
        let cwd = fixture.source.locator.current_cwd.clone();
        let resume_profile = fixture.source.profile.clone();
        let resumed_program = old_plan.program.to_string_lossy().into_owned();
        let envelope_for =
            |event_id: &str, token: &HandoffToken| crate::bridge::store::HookEnvelope {
                schema_version: crate::bridge::store::HOOK_ENVELOPE_VERSION,
                event_id: event_id.into(),
                observed_at_ms: 10,
                session: session.clone(),
                transcript_path: transcript_path.clone(),
                cwd: cwd.clone(),
                process: Some(ProcessFingerprint {
                    pid: 99,
                    start_time: "resumed".into(),
                    executable: resumed_program.clone(),
                }),
                resume_profile: resume_profile.clone(),
                content_permit: None,
                managed_takeover: false,
                handoff_ack: Some(crate::bridge::store::HandoffHookAck {
                    attempt_id: attempt_id.clone(),
                    token_sha256: token_digest_hex(token.as_str()),
                }),
                event: crate::bridge::store::HookEvent::SessionStart {
                    source: "resume".into(),
                    model: None,
                },
            };
        let old_ack = envelope_for("old", &old_plan.token);
        store.persist_handoff_ack(&old_ack).unwrap();
        store.persist_handoff_ack(&old_ack).unwrap();

        let new_token = HandoffToken::generate();
        let HandoffPhase::ResumePrepared { token_digest, .. } = &mut fixture.attempt.phase else {
            panic!("attempt should be resume prepared");
        };
        *token_digest = new_token.digest();
        store.persist_handoff_attempt(&fixture.attempt).unwrap();
        let new_ack = envelope_for("new", &new_token);
        store.persist_handoff_ack(&new_ack).unwrap();
        assert_eq!(
            store
                .load_handoff_ack(fixture.attempt.attempt_id())
                .unwrap(),
            Some(new_ack.clone())
        );

        assert!(
            store.persist_handoff_ack(&old_ack).is_err(),
            "迟到旧 Hook 必须拒绝"
        );
        let mut conflicting = new_ack.clone();
        conflicting.event_id = "same-token-different-event".into();
        assert!(
            store.persist_handoff_ack(&conflicting).is_err(),
            "当前 token 已提交后不得改变 envelope"
        );
        let raw =
            std::fs::read_to_string(store.handoff_ack_path(fixture.attempt.attempt_id())).unwrap();
        assert!(!raw.contains(old_plan.token.as_str()));
        assert!(!raw.contains(new_token.as_str()));
    }

    #[test]
    fn handoff_attempt_schema_round_trips_without_exposing_token_in_debug() {
        let mut fixture = fixture();
        let source_exit = source_exit(&fixture);
        let launch = StrictResumePlan::build(
            &fixture.runtime,
            &fixture.attempt,
            &source_exit,
            &fixture.target,
        )
        .unwrap()
        .launch_identity();
        let token = HandoffToken::generate();
        let raw = token.as_str().to_string();
        fixture.attempt.phase = HandoffPhase::Resuming {
            proof: source_exit,
            staging: TmuxSessionIdentity {
                name: format!("{STAGING_PREFIX}{}", fixture.attempt.attempt_id),
                session_id: "$1".into(),
                session_created: 1,
                pane_id: "%1".into(),
                ownership_marker: "a".repeat(64),
            },
            token_digest: token.digest(),
            ack_deadline_ms: 100,
            launch,
        };
        let json = serde_json::to_string(&fixture.attempt).unwrap();
        let back: HandoffAttempt = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fixture.attempt);
        let debug = format!("{token:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains(&raw));
        assert!(!json.contains(&raw));
    }
}
