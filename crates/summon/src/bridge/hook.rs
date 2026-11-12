use crate::bridge::process_probe::{self, ProbeResult};
use crate::bridge::store::{
    BridgeStore, HandoffHookAck, HookEnvelope, HookEvent, HOOK_ENVELOPE_VERSION,
};
use crate::bridge::{
    HANDOFF_ATTEMPT_ENV, HANDOFF_TOKEN_ENV, HOOK_PROTOCOL, MANAGED_SESSION_ENV,
    MANAGED_TAKEOVER_ENV,
};
use anyhow::{bail, Context, Result};
use serde::de::{IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use summon_core::bridge::SessionKey;

const MAX_STDIN_BYTES: u64 = 1024 * 1024;
const MAX_PATH_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    transcript_path: String,
    cwd: String,
    hook_event_name: String,
    #[serde(default)]
    prompt_id: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    last_assistant_message: Option<String>,
    #[serde(default)]
    stop_hook_active: bool,
    #[serde(
        default,
        rename = "background_tasks",
        deserialize_with = "deserialize_non_empty_array"
    )]
    has_background_tasks: bool,
    #[serde(
        default,
        rename = "session_crons",
        deserialize_with = "deserialize_non_empty_array"
    )]
    has_session_crons: bool,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    old_cwd: Option<String>,
    #[serde(default)]
    new_cwd: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

fn deserialize_non_empty_array<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    struct NonEmptyArray;

    impl<'de> Visitor<'de> for NonEmptyArray {
        type Value = bool;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut non_empty = false;
            while sequence.next_element::<IgnoredAny>()?.is_some() {
                non_empty = true;
            }
            Ok(non_empty)
        }
    }

    deserializer.deserialize_seq(NonEmptyArray)
}

#[derive(Debug, Clone)]
struct HookEnvironment {
    managed: bool,
    home: PathBuf,
    config_dir: PathBuf,
    config_source: summon_core::bridge::ClaudeConfigSource,
    provider: summon_core::bridge::ClaudeProvider,
    handoff_attempt: Option<String>,
    handoff_token: Option<String>,
    managed_takeover: bool,
}

impl HookEnvironment {
    fn current() -> Self {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_default();
        let explicit_config =
            std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty());
        let config_source = if explicit_config.is_some() {
            summon_core::bridge::ClaudeConfigSource::Explicit
        } else {
            summon_core::bridge::ClaudeConfigSource::Default
        };
        let config_dir = explicit_config
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        // 只采样 provider 键是否非空；不读取、更不持久化终端中的任意 env 值。
        let present_provider_keys = summon_core::bridge::CLAUDE_PROVIDER_ENV_KEYS
            .iter()
            .copied()
            .filter(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
            .map(|key| (key, "present"));
        let provider = summon_core::bridge::classify_claude_provider(present_provider_keys);
        Self {
            managed: std::env::var(MANAGED_SESSION_ENV).as_deref() == Ok("1"),
            home,
            config_dir,
            config_source,
            provider,
            handoff_attempt: std::env::var(HANDOFF_ATTEMPT_ENV)
                .ok()
                .filter(|value| !value.is_empty()),
            handoff_token: std::env::var(HANDOFF_TOKEN_ENV)
                .ok()
                .filter(|value| !value.is_empty()),
            managed_takeover: std::env::var(MANAGED_TAKEOVER_ENV).as_deref() == Ok("1"),
        }
    }

    fn is_handoff_staging(&self) -> bool {
        self.managed && self.handoff_attempt.is_some() && self.handoff_token.is_some()
    }

    fn observes_managed_start(&self) -> bool {
        self.is_handoff_staging() || (self.managed && self.managed_takeover)
    }
}

/// Claude Hook 的顶层入口：有意吞掉所有错误并保持 exit 0，绝不因 bridge 故障阻塞用户会话。
/// 本函数不写 stdout/stderr，避免污染 Hook JSON 协议或终端。
pub fn run_safely(protocol: &str) {
    let env = HookEnvironment::current();
    if env.managed && !env.observes_managed_start() {
        return;
    }
    let _ = run_reader(
        protocol,
        std::io::stdin().lock(),
        &BridgeStore::runtime_default(),
        env,
    );
}

fn run_reader<R: Read>(
    protocol: &str,
    reader: R,
    store: &BridgeStore,
    mut env: HookEnvironment,
) -> Result<()> {
    if protocol != HOOK_PROTOCOL {
        bail!(summon_i18n::format!("re6170ba99b"));
    }
    if env.managed && !env.observes_managed_start() {
        return Ok(());
    }
    // SessionKey 必须基于真实 config root，而不是可能指向同一目录的多个 symlink 文本；
    // 解析失败时不猜测身份，直接跳过本次 discovery（顶层仍保持 exit 0）。
    env.config_dir = canonical_directory(&env.config_dir, "Claude config root")?;
    env.home = canonical_directory(&env.home, "HOME")?;
    let mut input = read_input(reader)?;
    // managed staging 只放行唯一一次 resume SessionStart 作为接管 ACK；其余生命周期继续按
    // 普通 summon-managed 会话忽略，避免进入 external Mirror 路径造成双发。
    if env.observes_managed_start()
        && (input.hook_event_name != "SessionStart" || input.source.as_deref() != Some("resume"))
    {
        return Ok(());
    }
    canonicalize_hook_paths(&mut input, &env.config_dir)?;
    if env.managed_takeover && !env.is_handoff_staging() {
        let session = SessionKey::new(path_text(&env.config_dir), input.session_id.clone());
        if !store.is_managed_takeover_session(&session)? {
            return Ok(());
        }
    }
    let mut probe = process_probe::probe(&input.session_id, &env.config_dir, &env.home);
    probe.profile.config_source = env.config_source;
    probe.profile.provider = env.provider;
    if probe.managed_origin && !env.observes_managed_start() {
        return Ok(());
    }
    capture(input, store, env, probe).map(|_| ())
}

fn read_input<R: Read>(reader: R) -> Result<HookInput> {
    let mut limited = reader.take(MAX_STDIN_BYTES + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .context(summon_i18n::text!("r913a3984c1"))?;
    if bytes.len() as u64 > MAX_STDIN_BYTES {
        bail!(summon_i18n::format!("r638e9ba84e"));
    }
    serde_json::from_slice(&bytes).context(summon_i18n::text!("r487d0d0883"))
}

fn capture(
    input: HookInput,
    store: &BridgeStore,
    env: HookEnvironment,
    probe: ProbeResult,
) -> Result<PathBuf> {
    validate_common(&input, &env)?;
    let session = SessionKey::new(path_text(&env.config_dir), input.session_id.clone());
    // state 缺失或损坏都不允许正文落盘；permit 还把正文钉到精确 binding generation，
    // daemon 归约时必须重验，避免解绑/重绑同一 SID 后把旧 spool 错投给新群。
    let content_permit = store
        .content_permit_for_hook(
            &session,
            &input.transcript_path,
            probe.process.as_ref(),
            &probe.profile,
        )
        .unwrap_or(None);
    let accepts_content = content_permit.is_some();
    let event = match input.hook_event_name.as_str() {
        "SessionStart" => HookEvent::SessionStart {
            source: safe_label(input.source.as_deref()).unwrap_or_else(|| "unknown".into()),
            model: safe_label(input.model.as_deref()),
        },
        "CwdChanged" => HookEvent::CwdChanged {
            old_cwd: input.old_cwd.filter(|path| valid_absolute_path_text(path)),
            new_cwd: input
                .new_cwd
                .filter(|path| valid_absolute_path_text(path))
                .unwrap_or_else(|| input.cwd.clone()),
        },
        "UserPromptSubmit" => HookEvent::UserPromptSubmit {
            prompt_id: safe_identifier(input.prompt_id.as_deref()),
            prompt: accepts_content
                .then_some(input.prompt)
                .flatten()
                .map(|text| {
                    summon_core::bridge::clip_external_body_for_state(
                        &text,
                        summon_core::bridge::EXTERNAL_PROMPT_STATE_JSON_BUDGET,
                    )
                }),
        },
        "Stop" => HookEvent::Stop {
            prompt_id: safe_identifier(input.prompt_id.as_deref()),
            last_assistant_message: accepts_content
                .then_some(input.last_assistant_message)
                .flatten()
                .map(|text| {
                    summon_core::bridge::clip_external_body_for_state(
                        &text,
                        summon_core::bridge::EXTERNAL_ASSISTANT_STATE_JSON_BUDGET,
                    )
                }),
            stop_hook_active: input.stop_hook_active,
            has_pending_wakeups: input.has_background_tasks || input.has_session_crons,
        },
        "StopFailure" => HookEvent::StopFailure {
            prompt_id: safe_identifier(input.prompt_id.as_deref()),
            // 错误/堆栈不是用户可见回答，也可能含本机路径和内部细节；领域层只认最终回答。
            error: None,
            error_details: None,
            last_assistant_message: None,
        },
        "SessionEnd" => HookEvent::SessionEnd {
            reason: safe_label(input.reason.as_deref()),
        },
        other => bail!(summon_i18n::format!("r9e3de7f5a2")),
    };
    let envelope = HookEnvelope {
        schema_version: HOOK_ENVELOPE_VERSION,
        event_id: uuid::Uuid::new_v4().to_string(),
        observed_at_ms: now_ms(),
        session,
        transcript_path: input.transcript_path,
        cwd: input.cwd,
        process: probe.process,
        resume_profile: probe.profile,
        content_permit,
        managed_takeover: env.managed_takeover && !env.is_handoff_staging(),
        handoff_ack: if env.is_handoff_staging() {
            let attempt_id = env
                .handoff_attempt
                .context(summon_i18n::text!("r69405a39b9"))?;
            uuid::Uuid::parse_str(&attempt_id).context(summon_i18n::text!("rffcec76ea5"))?;
            let token = env
                .handoff_token
                .context(summon_i18n::text!("r711dd9d03e"))?;
            crate::bridge::handoff::HandoffToken::parse(&token)?;
            Some(HandoffHookAck {
                attempt_id,
                token_sha256: crate::bridge::handoff::token_digest_hex(&token),
            })
        } else {
            None
        },
        event,
    };
    if envelope.handoff_ack.is_some() {
        store.persist_handoff_ack(&envelope)?;
    }
    store.enqueue_hook(&envelope)
}

fn validate_common(input: &HookInput, env: &HookEnvironment) -> Result<()> {
    if !valid_session_id(&input.session_id) {
        bail!(summon_i18n::format!("rdbbf1df51a"));
    }
    if !valid_absolute_path_text(&input.transcript_path) || !valid_absolute_path_text(&input.cwd) {
        bail!(summon_i18n::format!("r379987fee9"));
    }
    if !env.config_dir.is_absolute() || !env.home.is_absolute() {
        bail!(summon_i18n::format!("rdd6acdd7cb"));
    }
    if !Path::new(&input.transcript_path).starts_with(&env.config_dir) {
        bail!(summon_i18n::format!("r1a1c347d7a"));
    }
    Ok(())
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_path_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_PATH_BYTES && !value.contains('\0')
}

fn valid_absolute_path_text(value: &str) -> bool {
    use std::path::Component;

    valid_path_text(value)
        && Path::new(value).is_absolute()
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical =
        std::fs::canonicalize(path).with_context(|| summon_i18n::format!("re652b53f6b"))?;
    if !canonical.is_dir() {
        bail!(summon_i18n::format!("rc4ef446f97"));
    }
    Ok(canonical)
}

fn canonicalize_hook_paths(input: &mut HookInput, config_dir: &Path) -> Result<()> {
    let transcript = canonical_transcript(
        Path::new(&input.transcript_path),
        config_dir,
        &input.session_id,
    )?;
    input.transcript_path = path_text(&transcript);
    input.cwd = path_text(&canonical_directory(Path::new(&input.cwd), "Hook cwd")?);
    input.old_cwd = input.old_cwd.take().and_then(|path| {
        canonical_directory(Path::new(&path), "Hook old_cwd")
            .ok()
            .map(|canonical| path_text(&canonical))
    });
    input.new_cwd = input.new_cwd.take().and_then(|path| {
        canonical_directory(Path::new(&path), "Hook new_cwd")
            .ok()
            .map(|canonical| path_text(&canonical))
    });
    if input.transcript_path.is_empty() || input.cwd.is_empty() {
        bail!(summon_i18n::format!("r639b07a5cb"));
    }
    Ok(())
}

fn canonical_transcript(path: &Path, config_dir: &Path, session_id: &str) -> Result<PathBuf> {
    let text = path.to_str().unwrap_or_default();
    let expected_name = format!("{session_id}.jsonl");
    if !valid_absolute_path_text(text)
        || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
        || path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str())
    {
        bail!(summon_i18n::format!("r4a49ce4342"));
    }
    match std::fs::canonicalize(path) {
        Ok(canonical) => {
            if !canonical.is_file() || !canonical.starts_with(config_dir) {
                bail!(summon_i18n::format!("rd017c77d47"));
            }
            Ok(canonical)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // SessionStart 可能早于 transcript 文件创建；不能因此永久漏发现。只允许已存在、已
            // canonicalize 且位于 config root 内的 parent + 单一 `.jsonl` 文件名，后续真正读取时
            // 仍会再次检查普通文件与 confinement。
            let parent = path.parent().context(summon_i18n::text!("ra23cbe88f9"))?;
            let canonical_parent = std::fs::canonicalize(parent)
                .with_context(|| summon_i18n::format!("re9b9c02f23"))?;
            if !canonical_parent.is_dir() || !canonical_parent.starts_with(config_dir) {
                bail!(summon_i18n::format!("r3a7823b5d6"));
            }
            let file_name = path
                .file_name()
                .context(summon_i18n::text!("r268b05c3ed"))?;
            Ok(canonical_parent.join(file_name))
        }
        Err(err) => Err(err).with_context(|| summon_i18n::format!("rdc41b4de7c")),
    }
}

fn safe_label(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b' ')
                })
        })
        .map(str::to_string)
}

fn safe_identifier(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .map(str::to_string)
}

fn path_text(path: &Path) -> String {
    path.to_str().unwrap_or_default().to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::store::HookEvent;
    use summon_core::bridge::{
        BridgeMode, BridgeState, ChatBinding, ChatSlot, ExternalLifecycle, ExternalLocator,
        ExternalSessionRecord, ProcessFingerprint, ResumeProfile, ScopedChatKey, SessionLease,
    };

    const PROMPT: &str =
        include_str!("../../../summon-core/tests/fixtures/claude-hooks/user-prompt-submit.json");
    const CONFIG_DIR: &str = "/tmp/summon-bridge-fixture";

    fn environment() -> HookEnvironment {
        HookEnvironment {
            managed: false,
            home: PathBuf::from("/tmp/home"),
            config_dir: PathBuf::from(CONFIG_DIR),
            config_source: summon_core::bridge::ClaudeConfigSource::Default,
            provider: summon_core::bridge::ClaudeProvider::Default,
            handoff_attempt: None,
            handoff_token: None,
            managed_takeover: false,
        }
    }

    fn probe() -> ProbeResult {
        ProbeResult {
            process: None,
            profile: ResumeProfile {
                home: "/tmp/home".into(),
                config_dir: CONFIG_DIR.into(),
                ..ResumeProfile::default()
            },
            managed_origin: false,
        }
    }

    fn verified_probe() -> ProbeResult {
        ProbeResult {
            process: Some(ProcessFingerprint {
                pid: 42,
                start_time: "start".into(),
                executable: "/opt/claude".into(),
            }),
            profile: ResumeProfile {
                claude_bin: "/opt/claude".into(),
                home: "/tmp/home".into(),
                config_dir: CONFIG_DIR.into(),
                process_verified: true,
                ..ResumeProfile::default()
            },
            managed_origin: false,
        }
    }

    fn read_only_event(path: &Path) -> HookEnvelope {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn managed_handoff_staging_rejects_ack_without_durable_attempt() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let config = home.join(".claude");
        let project = root.path().join("project");
        let transcript = config.join("projects/demo/sid.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let attempt = uuid::Uuid::new_v4().to_string();
        let token = "a".repeat(64);
        let payload = serde_json::json!({
            "session_id": "sid",
            "transcript_path": transcript,
            "cwd": project,
            "hook_event_name": "SessionStart",
            "source": "resume",
            "model": "sonnet"
        })
        .to_string();
        let store = BridgeStore::new(root.path().join("bridge"));
        let error = run_reader(
            HOOK_PROTOCOL,
            payload.as_bytes(),
            &store,
            HookEnvironment {
                managed: true,
                home: std::fs::canonicalize(home).unwrap(),
                config_dir: std::fs::canonicalize(config).unwrap(),
                config_source: summon_core::bridge::ClaudeConfigSource::Default,
                provider: summon_core::bridge::ClaudeProvider::Default,
                handoff_attempt: Some(attempt.clone()),
                handoff_token: Some(token.clone()),
                managed_takeover: false,
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("handoff attempt"));
        assert!(!store.handoff_ack_path(&attempt).exists());
    }

    /// recall 全托管的正向闭环:attempt 已 durable 时,staging 里那次 resume SessionStart
    /// 必须落成 ACK。反向用例见上一个测试。真机上「起了 staging → 60s ACK 超时」时,先跑它
    /// 区分「hook 侧写不进」还是「hook 压根没跑到」。
    #[test]
    fn recall_staging_ack_lands_when_attempt_is_durable() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let config = home.join(".claude");
        let project = root.path().join("project");
        let transcript = config.join("projects/demo/sid.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let home = std::fs::canonicalize(&home).unwrap();
        let config = std::fs::canonicalize(&config).unwrap();
        let transcript = std::fs::canonicalize(&transcript).unwrap();
        let project = std::fs::canonicalize(&project).unwrap();
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let token = "a".repeat(64);
        let store = BridgeStore::new(root.path().join("bridge"));
        let attempt = crate::bridge::recall_resume::RecallResumeAttempt {
            schema_version: crate::bridge::recall_resume::RECALL_RESUME_SCHEMA_VERSION,
            id: attempt_id.clone(),
            token_digest: crate::bridge::handoff::HandoffTokenDigest::from_hex(
                &crate::bridge::handoff::token_digest_hex(&token),
            )
            .unwrap(),
            staging_name: format!(
                "{}{attempt_id}",
                crate::bridge::recall_resume::STAGING_PREFIX
            ),
            ownership_marker: "b".repeat(64),
            canonical_tmux: "soul-demo-gchat".into(),
            target: ScopedChatKey::new("scope", "demo", "gchat"),
            target_chat_type: "group".into(),
            actor_open_id: "ou_actor".into(),
            program: "/opt/claude".into(),
            args: vec!["--resume".into(), "sid".into()],
            home: path_text(&home),
            config_dir: path_text(&config),
            provider: summon_core::bridge::ClaudeProvider::Default,
            sid: "sid".into(),
            transcript_path: path_text(&transcript),
            cwd: path_text(&project),
            created_at_ms: 1,
            ack_deadline_ms: u64::MAX,
            phase: crate::bridge::recall_resume::RecallResumePhase::Launched,
        };
        store.persist_recall_resume_attempt(&attempt).unwrap();
        let payload = serde_json::json!({
            "session_id": "sid",
            "transcript_path": transcript,
            "cwd": project,
            "hook_event_name": "SessionStart",
            "source": "resume"
        })
        .to_string();
        run_reader(
            HOOK_PROTOCOL,
            payload.as_bytes(),
            &store,
            HookEnvironment {
                managed: true,
                home,
                config_dir: config,
                config_source: summon_core::bridge::ClaudeConfigSource::Default,
                provider: summon_core::bridge::ClaudeProvider::Default,
                handoff_attempt: Some(attempt_id.clone()),
                handoff_token: Some(token),
                managed_takeover: true,
            },
        )
        .unwrap();
        assert!(
            store.handoff_ack_path(&attempt_id).exists(),
            "attempt 已 durable 时 resume SessionStart 必须落成 ACK"
        );
    }

    fn prompt_input() -> HookInput {
        serde_json::from_str(PROMPT).unwrap()
    }

    #[test]
    fn session_id_rejects_option_and_control_injection() {
        assert!(valid_session_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(valid_session_id("sid_1"));
        assert!(!valid_session_id("--danger"));
        assert!(!valid_session_id("sid with space"));
        assert!(!valid_session_id("sid\0tail"));
    }

    #[test]
    fn unbound_prompt_body_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path());
        let path = capture(prompt_input(), &store, environment(), probe()).unwrap();
        let envelope = read_only_event(&path);
        assert!(envelope.content_permit.is_none());
        assert!(matches!(
            envelope.event,
            HookEvent::UserPromptSubmit { prompt: None, .. }
        ));
    }

    #[test]
    fn corrupt_state_also_strips_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(store.state_path(), b"{broken").unwrap();
        let path = capture(prompt_input(), &store, environment(), probe()).unwrap();
        assert!(matches!(
            read_only_event(&path).event,
            HookEvent::UserPromptSubmit { prompt: None, .. }
        ));
    }

    #[test]
    fn bound_prompt_body_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path());
        let session = SessionKey::new(CONFIG_DIR, prompt_input().session_id);
        let target = ScopedChatKey::new("app", "agent", "chat");
        let binding = ChatBinding {
            id: "binding".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session,
            mode: BridgeMode::Mirror,
            actor_open_id: "ou_actor".into(),
            created_at_ms: 1,
            revision: 1,
        };
        let verified = verified_probe();
        let mut state = BridgeState::default();
        state.sessions.insert(
            binding.session.storage_key(),
            ExternalSessionRecord {
                key: binding.session.clone(),
                generation: 1,
                locator: ExternalLocator {
                    key: binding.session.clone(),
                    transcript_path: prompt_input().transcript_path,
                    resume_lookup_cwd: "/tmp/demo".into(),
                    current_cwd: "/tmp/demo".into(),
                    profile: verified.profile.clone(),
                    revision: 1,
                },
                process: verified.process.clone(),
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
            binding.session.storage_key(),
            SessionLease {
                binding_id: binding.id.clone(),
                target: binding.target.clone(),
            },
        );
        let json = serde_json::to_vec_pretty(&state).unwrap();
        crate::runtime::fsio::atomic_write(&store.state_path(), &json).unwrap();

        let path = capture(prompt_input(), &store, environment(), verified.clone()).unwrap();
        let envelope = read_only_event(&path);
        assert!(envelope.content_permit.is_some());
        assert!(matches!(
            envelope.event,
            HookEvent::UserPromptSubmit {
                prompt: Some(ref prompt),
                ..
            } if prompt == "请概括当前改动。"
        ));

        let mut oversized = prompt_input();
        oversized.prompt = Some("\0<&汉\n".repeat(40_000));
        let clipped_path = capture(oversized, &store, environment(), verified.clone()).unwrap();
        let clipped = read_only_event(&clipped_path);
        let HookEvent::UserPromptSubmit {
            prompt: Some(clipped),
            ..
        } = clipped.event
        else {
            panic!("绑定后的大 prompt 应保留有界正文")
        };
        assert!(
            serde_json::to_string(&clipped).unwrap().len()
                <= summon_core::bridge::EXTERNAL_PROMPT_STATE_JSON_BUDGET
        );

        let mut modern_stop = prompt_input();
        modern_stop.hook_event_name = "Stop".into();
        modern_stop.prompt = None;
        modern_stop.last_assistant_message = Some("\0<&答\n".repeat(40_000));
        modern_stop.stop_hook_active = true;
        modern_stop.has_background_tasks = true;
        let stop_path = capture(modern_stop, &store, environment(), verified).unwrap();
        let stop = read_only_event(&stop_path);
        let HookEvent::Stop {
            last_assistant_message: Some(answer),
            stop_hook_active,
            has_pending_wakeups,
            ..
        } = stop.event
        else {
            panic!("现代 Stop 字段应进入有界 envelope")
        };
        assert!(stop_hook_active && has_pending_wakeups);
        assert!(
            serde_json::to_string(&answer).unwrap().len()
                <= summon_core::bridge::EXTERNAL_ASSISTANT_STATE_JSON_BUDGET
        );
    }

    #[test]
    fn modern_stop_arrays_are_reduced_to_non_sensitive_presence_bits() {
        let input = br#"{
            "session_id":"sid",
            "transcript_path":"/tmp/summon-bridge-fixture/projects/demo/sid.jsonl",
            "cwd":"/tmp/demo",
            "hook_event_name":"Stop",
            "stop_hook_active":true,
            "last_assistant_message":"answer",
            "background_tasks":[{"command":"VERY_SECRET_COMMAND"}],
            "session_crons":[]
        }"#;
        let parsed = read_input(input.as_slice()).unwrap();
        assert!(parsed.stop_hook_active);
        assert!(parsed.has_background_tasks);
        assert!(!parsed.has_session_crons);
    }

    #[test]
    fn daemon_offline_verified_resume_grace_preserves_first_turn_for_next_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path());
        let session = SessionKey::new(CONFIG_DIR, prompt_input().session_id);
        let target = ScopedChatKey::new("app", "agent", "chat");
        let binding = ChatBinding {
            id: "binding-resume".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: session.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "ou_actor".into(),
            created_at_ms: 1,
            revision: 7,
        };
        store
            .with_state(|state| {
                state.upsert_session(ExternalSessionRecord {
                    key: session.clone(),
                    generation: 1,
                    locator: ExternalLocator {
                        key: session.clone(),
                        transcript_path: prompt_input().transcript_path,
                        resume_lookup_cwd: "/tmp/demo".into(),
                        current_cwd: "/tmp/demo".into(),
                        profile: ResumeProfile {
                            claude_bin: "/opt/claude".into(),
                            home: "/tmp/home".into(),
                            config_dir: CONFIG_DIR.into(),
                            process_verified: true,
                            ..ResumeProfile::default()
                        },
                        revision: 1,
                    },
                    process: Some(ProcessFingerprint {
                        pid: 41,
                        start_time: "old".into(),
                        executable: "/opt/claude".into(),
                    }),
                    lifecycle: ExternalLifecycle::Ended,
                    first_seen_ms: 1,
                    last_seen_ms: 2,
                    last_applied_sequence: 0,
                    pending_prompt: None,
                });
                state
                    .slots
                    .insert(target.storage_key(), ChatSlot::MirrorBound(binding.clone()));
                state.session_leases.insert(
                    session.storage_key(),
                    SessionLease {
                        binding_id: binding.id.clone(),
                        target: binding.target.clone(),
                    },
                );
                Ok(())
            })
            .unwrap();
        let resumed_probe = ProbeResult {
            process: Some(ProcessFingerprint {
                pid: 42,
                start_time: "new".into(),
                executable: "/opt/claude".into(),
            }),
            profile: ResumeProfile {
                claude_bin: "/opt/claude".into(),
                home: "/tmp/home".into(),
                config_dir: CONFIG_DIR.into(),
                process_verified: true,
                ..ResumeProfile::default()
            },
            managed_origin: false,
        };

        let mut start = prompt_input();
        start.hook_event_name = "SessionStart".into();
        start.prompt = None;
        start.source = Some("resume".into());
        capture(start, &store, environment(), resumed_probe.clone()).unwrap();
        let prompt_path =
            capture(prompt_input(), &store, environment(), resumed_probe.clone()).unwrap();
        let prompt = read_only_event(&prompt_path);
        assert_eq!(prompt.content_permit.as_ref().unwrap().generation, 2);
        assert!(matches!(
            prompt.event,
            HookEvent::UserPromptSubmit {
                prompt: Some(ref text),
                ..
            } if text == "请概括当前改动。"
        ));
        let mut stop = prompt_input();
        stop.hook_event_name = "Stop".into();
        stop.prompt = None;
        stop.last_assistant_message = Some("resume 后首轮回答".into());
        capture(stop, &store, environment(), resumed_probe).unwrap();

        let stats = crate::bridge::drain::drain_tick(&store).unwrap();
        assert_eq!(stats.applied, 3);
        let state = store.load_state().unwrap().unwrap();
        assert_eq!(state.session(&session).unwrap().generation, 2);
        let turn = state
            .pending_mirror_turns
            .values()
            .next()
            .expect("resume grace turn 应被归约");
        assert_eq!(turn.generation, 2);
        assert_eq!(turn.messages[0].text, "请概括当前改动。");
        assert_eq!(turn.messages[1].text, "resume 后首轮回答");
    }

    #[test]
    fn handoff_pending_does_not_enable_content_capture() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path());
        let session = SessionKey::new(CONFIG_DIR, prompt_input().session_id);
        let target = ScopedChatKey::new("app", "agent", "chat");
        let binding = ChatBinding {
            id: "binding".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session,
            mode: BridgeMode::Handoff,
            actor_open_id: "ou_actor".into(),
            created_at_ms: 1,
            revision: 1,
        };
        let mut state = BridgeState::default();
        state
            .slots
            .insert(target.storage_key(), ChatSlot::HandoffPending(binding));
        let json = serde_json::to_vec_pretty(&state).unwrap();
        crate::runtime::fsio::atomic_write(&store.state_path(), &json).unwrap();

        let path = capture(prompt_input(), &store, environment(), probe()).unwrap();
        assert!(matches!(
            read_only_event(&path).event,
            HookEvent::UserPromptSubmit { prompt: None, .. }
        ));
    }

    #[test]
    fn concurrent_hook_writers_create_distinct_complete_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path());
        let mut workers = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            workers.push(std::thread::spawn(move || {
                capture(prompt_input(), &store, environment(), probe()).unwrap()
            }));
        }
        let paths: Vec<PathBuf> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        let mut names: Vec<_> = paths
            .iter()
            .map(|path| path.file_name().unwrap().to_owned())
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 16);
        for path in paths {
            let event = read_only_event(&path);
            assert_eq!(event.schema_version, HOOK_ENVELOPE_VERSION);
        }
        assert!(std::fs::read_dir(store.pending_dir())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.')));
    }

    #[test]
    fn input_limit_rejects_more_than_one_mibibyte() {
        let input = vec![b'x'; MAX_STDIN_BYTES as usize + 1];
        assert!(read_input(input.as_slice()).is_err());
    }

    #[test]
    fn relative_locator_is_rejected_before_spooling() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path());
        let mut input = prompt_input();
        input.transcript_path = "relative/transcript.jsonl".into();
        assert!(capture(input, &store, environment(), probe()).is_err());
        assert!(!store.pending_dir().exists());
    }

    #[test]
    fn parent_segments_and_transcript_outside_config_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path());
        let mut input = prompt_input();
        input.cwd = "/tmp/demo/../other".into();
        assert!(capture(input, &store, environment(), probe()).is_err());

        let mut input = prompt_input();
        input.transcript_path = "/tmp/other/transcript.jsonl".into();
        assert!(capture(input, &store, environment(), probe()).is_err());
        assert!(!store.pending_dir().exists());
    }

    #[test]
    fn session_start_can_reference_not_yet_created_transcript_in_canonical_parent() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".claude");
        let project = config.join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let future = project.join("sid.jsonl");
        let resolved =
            canonical_transcript(&future, &std::fs::canonicalize(&config).unwrap(), "sid").unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(&project).unwrap().join("sid.jsonl")
        );
        assert!(!future.exists());
    }

    #[test]
    fn same_config_root_transcript_for_different_session_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".claude");
        let project = config.join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let wrong = project.join("session-b.jsonl");
        std::fs::write(&wrong, "{}\n").unwrap();
        assert!(canonical_transcript(
            &wrong,
            &std::fs::canonicalize(&config).unwrap(),
            "session-a",
        )
        .is_err());
    }
}
