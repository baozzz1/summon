//! 历史(无源)Claude 会话完整托管:把纯盘上 transcript 恢复进目标群 G 的 canonical tmux,
//! 并绑群,使群消息经 per-chat dispatcher 往返。
//!
//! 为什么自足、不进 handoff coordinator:coordinator 的 resume 半段耦合 source(要求
//! `resumed != source.process`、`check_source_process(source) == Gone`、`generation == source+1`),
//! 历史真无源会话满足不了。故这里复用 handoff 的**叶子原语**(staging tmux / rename_exact /
//! SessionStart-hook ACK),但自建轻量 4 相位流程,**绝不碰 handoff.rs 的 coordinator 生产代码**。
//!
//! 红线:失败/超时只 cleanup 自己的 staging(`summon-bridge-handoff-<id>`),**绝不** kill/碰
//! canonical 或 soul-* 持久层。never-cold-start 靠 `rename_exact`(canonical 缺席否则拒绝)与
//! 源无关 ACK 三重锁双重保证——冷启会打破三重锁,续不上就只清 staging,历史 transcript 不可变、
//! 重试幂等。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use summon_core::bridge::{
    ClaudeProvider, ExpectedResumeIdentity, ProcessFingerprint, ScopedChatKey, SessionKey,
};
use summon_core::registry::Agent;

use crate::bridge::handoff::{HandoffTargetProfile, HandoffTokenDigest, VerifiedTargetRuntime};
use crate::bridge::store::{BridgeStore, HookEnvelope, HookEvent};
use crate::observe::log::log;
use crate::session::tmux::{
    create_handoff_staging, has_session_strict, OwnedHandoffStaging, RenamedHandoffTmux,
    TmuxSessionIdentity,
};

pub(crate) const RECALL_RESUME_SCHEMA_VERSION: u32 = 1;

/// staging tmux 名前缀。必须与 `session::tmux` 的 `HANDOFF_STAGING_PREFIX` 字面一致
/// (`create_handoff_staging`/`validate_staging_name` 据此校验),复用同一命名空间;不同 UUID
/// 保证与 takeover staging 永不撞名。改这里必须同步改 tmux.rs。
pub(crate) const STAGING_PREFIX: &str = "summon-bridge-handoff-";

/// ACK 等待窗口。历史会话无源、失败只清 staging,故给足时间等被恢复进程 SessionStart(resume)。
const ACK_DEADLINE_MS: u64 = 60_000;

/// recall 全托管的持久相位。对比 handoff 的 11 相位:无源即无 source-exit 半段,只剩
/// 起 staging → 验 ACK → 改名绑群 3 步 + 终态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RecallResumePhase {
    /// staging 已起,等待被恢复进程的 SessionStart(resume) hook 产 ACK。
    Launched,
    /// ACK 已过源无关三重锁,确认真续接;等待 rename 到 canonical + 绑群。
    /// 存 staging 身份:daemon 若崩在 rename 后、绑群前,重入时据此复验 canonical 确是我们改名
    /// 的那个(session_id/created/pane),绝不把群绑到被外部会话抢占的同名 canonical 上。
    Acked {
        resumed: ProcessFingerprint,
        staging: TmuxSessionIdentity,
    },
    /// 已 rename 到 canonical 且绑群,终态。
    Bound,
    /// 失败/超时,staging 已清,终态。
    Failed,
}

/// 一次历史会话全托管的 durable 记录。落 `recall_resume_dir/<id>.json`。
///
/// 它同时承担四个职责:①ACK 反陈旧摘要源(`store::current_ack_digest` 认它取 `token_digest`);
/// ②源无关三重锁的期望值持有者(`verify_recall_ack`);③staging 所有权恢复(name+marker);
/// ④rename 目标 + 绑群参数(canonical/target/actor)。无任何 source 字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecallResumeAttempt {
    pub schema_version: u32,
    /// UUID。同时是 `HANDOFF_ATTEMPT_ENV` 关联键、staging nonce 与 ACK 文件名 key。
    pub id: String,
    pub token_digest: HandoffTokenDigest,
    pub staging_name: String,
    pub ownership_marker: String,
    /// = `agent.tmux_name_for_chat(target_chat_type, target.chat_id, false)`。
    pub canonical_tmux: String,
    pub target: ScopedChatKey,
    pub target_chat_type: String,
    /// operate-gate 放行的发起者 open_id → 绑群时的 chat_meta creator/last_sender。
    pub actor_open_id: String,
    // ---- resume 身份(无 source):被恢复进程应当自证的 launch 身份 ----
    pub program: String,
    pub args: Vec<String>,
    pub home: String,
    pub config_dir: String,
    pub provider: ClaudeProvider,
    pub sid: String,
    pub transcript_path: String,
    pub cwd: String,
    pub created_at_ms: u64,
    pub ack_deadline_ms: u64,
    pub phase: RecallResumePhase,
}

impl RecallResumeAttempt {
    /// 当前相位的 token digest(供 store ACK 反陈旧校验)。只有 `Launched` 接受 ACK;
    /// 终态/已 Acked 不再接受新 ACK 顶替。
    pub(crate) fn launched_token_digest(&self) -> Option<&HandoffTokenDigest> {
        matches!(self.phase, RecallResumePhase::Launched).then_some(&self.token_digest)
    }

    fn session_key(&self) -> SessionKey {
        SessionKey::new(self.config_dir.clone(), self.sid.clone())
    }
}

/// 源无关 ACK 三重锁:证明 staging 里的进程"真续接了原 sid"而非冷启,**不依赖任何 source**。
///
/// 冷启会打破 `source=="resume"` / `session_id==sid` / `transcript_path` 三条中的任意一条 →
/// fail-closed。**无** `source_process` / `generation+1` / `check_source_process(source)`——历史
/// 会话没有源可比。`owns_process` 由调用方用 staging 的 `owns_process(resumed.pid)` 现测后传入
/// (纯逻辑与 tmux 解耦,便于单测)。成功返回被恢复进程指纹,供 tick 记入 `Acked`。
pub(crate) fn verify_recall_ack(
    plan: &RecallResumeAttempt,
    envelope: &HookEnvelope,
    owns_process: bool,
) -> Result<ProcessFingerprint> {
    // ① SessionStart 的 source 必须是 resume(冷启是 "startup"/"clear"/"compact" 等)。
    let HookEvent::SessionStart { source, .. } = &envelope.event else {
        bail!(summon_i18n::format!("r3b16eee3f7"));
    };
    if source != "resume" {
        bail!(summon_i18n::format!("rbb49fbf32b"));
    }
    // ② 续接的必须是原 sid(冷启会拿到新 sid)。
    if envelope.session.session_id != plan.sid {
        bail!(summon_i18n::format!("re293b83c03"));
    }
    // ③ 续接的必须是原盘上 transcript(冷启写新文件)。
    if envelope.transcript_path != plan.transcript_path {
        bail!(summon_i18n::format!("r153f1fbbc6"));
    }
    // token digest:这条 ACK 来自本次 staging 注入的 token(反陈旧 / 反迟到旧 staging 顶替)。
    let ack = envelope
        .handoff_ack
        .as_ref()
        .context(summon_i18n::text!("r7e3b55ff1c"))?;
    if ack.attempt_id != plan.id {
        bail!(summon_i18n::format!("rf5ffcad58f"));
    }
    let observed = HandoffTokenDigest::from_hex(&ack.token_sha256)
        .context(summon_i18n::text!("rc0bc8458c9"))?;
    if !observed.constant_time_eq(&plan.token_digest) {
        bail!(summon_i18n::format!("rfa79c08bb2"));
    }
    // resume profile 必须与 plan 的 launch 身份一致,并且是经进程/环境复验的默认配置。
    // 与 handoff 共用同一个逐项比对函数:拒绝时点名到字段 + 期望值/实际值,不再压成一句「不一致」
    // (7 个条件压一句时,真机上根本没法判断是哪条不匹配)。比对的都是路径/枚举/布尔,无 token。
    let mismatches = summon_core::bridge::diff_resume_identity(
        &envelope.resume_profile,
        &ExpectedResumeIdentity {
            claude_bin: &plan.program,
            home: &plan.home,
            config_dir: &plan.config_dir,
            provider: plan.provider,
        },
    );
    if !mismatches.is_empty() {
        bail!(summon_i18n::format!(
            "r7c91a3c3ab",
            summon_core::bridge::describe_resume_identity_mismatches(&mismatches)
        ));
    }
    // 这个被恢复进程确实归本 staging 所有(pane pid 匹配),排除别处巧合的同名会话。
    if !owns_process {
        bail!(summon_i18n::format!("r2074368ee3"));
    }
    let resumed = envelope
        .process
        .clone()
        .context(summon_i18n::text!("rad3c88e8ec"))?;
    if !resumed.is_complete() {
        bail!(summon_i18n::format!("r6553aaf927"));
    }
    Ok(resumed)
}

/// 起一次 recall staging 所需的材料。`env` 含一次性 raw token(`HANDOFF_TOKEN_ENV`),**只在内存、
/// 绝不落盘**——`attempt` 只存 `token_digest`。调用方:先 `persist(attempt)` 让 store 认摘要(hook
/// 才写得进 ACK),再 `launch_staging()`。
pub(crate) struct RecallResumeLaunch {
    attempt: RecallResumeAttempt,
    env: Vec<(String, String)>,
}

impl RecallResumeLaunch {
    pub(crate) fn attempt(&self) -> &RecallResumeAttempt {
        &self.attempt
    }

    /// staging 的唯一 launch 入口:在会话原 cwd 里 `claude --resume <sid>`,带 token/attempt env。
    pub(crate) fn launch_staging(&self) -> Result<OwnedHandoffStaging> {
        create_handoff_staging(
            &self.attempt.staging_name,
            &self.attempt.ownership_marker,
            std::path::Path::new(&self.attempt.cwd),
            std::path::Path::new(&self.attempt.program),
            &self.attempt.args,
            &self.env,
        )
        .context(summon_i18n::text!("rf37e52eb9f"))
    }
}

/// 造一份"历史会话 → 群 G canonical"的无源 resume plan。复用 takeover 的目标 profile/runtime
/// 复验(eligibility + auth + program/home/config 已 canonicalize),env 组装照
/// `StrictResumePreflight::finish` 但**删所有 source 交叉核**:去 `CLAUDE_CONFIG_DIR`、置
/// `HOME`/MANAGED/TOKEN/ATTEMPT。
pub(crate) fn build_recall_resume_plan(
    agent: &Agent,
    target: ScopedChatKey,
    chat_type: &str,
    sid: &str,
    cwd: &str,
    actor_open_id: &str,
) -> Result<RecallResumeLaunch> {
    if !matches!(chat_type, "group" | "topic_group") {
        bail!(summon_i18n::format!("r963ea7ba0b"));
    }
    let canonical = agent.tmux_name_for_chat(chat_type, &target.chat_id, false);
    let profile = HandoffTargetProfile::for_agent(agent, chat_type, &target.chat_id, &canonical)
        .context(summon_i18n::text!("rcab245ef00"))?;
    let runtime =
        VerifiedTargetRuntime::verify(&profile).context(summon_i18n::text!("red42992bf7"))?;
    // program/home/config 已由 verify canonicalize + 复验,这里只取用。
    let program = runtime.program().to_path_buf();
    let home = runtime.home().to_path_buf();
    let config = runtime.config().to_path_buf();
    let provider = runtime.provider();
    // work_dir = 会话原 cwd:`claude --resume <sid>` 只在原 project 目录下找得到那条 transcript,
    // 否则冷启新会话(会被三重锁拒),故必须用会话原 cwd 起 staging。
    let work_dir =
        std::fs::canonicalize(cwd).with_context(|| format!("canonicalize recall cwd {cwd}"))?;
    if !work_dir.is_dir() {
        bail!(summon_i18n::format!("r6bc1f36fab"));
    }
    // 历史 transcript:在目标 Bot 默认 config 的 projects 下扫 <sid>.jsonl,canonicalize 后须是
    // config root 内普通文件——与 hook 侧 confinement 对齐,ACK 三重锁据此比 transcript_path。
    let transcript_path = resolve_managed_transcript(&config, sid)?;

    let mut args = profile.args().to_vec();
    args.push("--resume".into());
    args.push(sid.to_string());
    let raw_token = generate_token();
    let ownership_marker = generate_token();
    let id = uuid::Uuid::new_v4().to_string();
    let staging_name = format!("{STAGING_PREFIX}{id}");
    let mut env: std::collections::BTreeMap<String, String> =
        profile.env().iter().cloned().collect();
    // 默认配置语义 = CLAUDE_CONFIG_DIR 缺席(显式写同路径也会触发 onboarding);hook 据此判 Default。
    env.remove("CLAUDE_CONFIG_DIR");
    env.insert("HOME".into(), home.to_string_lossy().into_owned());
    env.insert(crate::bridge::MANAGED_SESSION_ENV.into(), "1".into());
    env.insert(crate::bridge::MANAGED_TAKEOVER_ENV.into(), "1".into());
    env.insert(crate::bridge::HANDOFF_TOKEN_ENV.into(), raw_token.clone());
    env.insert(crate::bridge::HANDOFF_ATTEMPT_ENV.into(), id.clone());
    let env: Vec<(String, String)> = env.into_iter().collect();

    let token_digest =
        HandoffTokenDigest::from_hex(&crate::bridge::handoff::token_digest_hex(&raw_token))
            .context(summon_i18n::text!("rbe6087402e"))?;
    let now = now_ms();
    let attempt = RecallResumeAttempt {
        schema_version: RECALL_RESUME_SCHEMA_VERSION,
        id,
        token_digest,
        staging_name,
        ownership_marker,
        canonical_tmux: canonical,
        target,
        target_chat_type: chat_type.to_string(),
        actor_open_id: actor_open_id.to_string(),
        program: program.to_string_lossy().into_owned(),
        args,
        home: home.to_string_lossy().into_owned(),
        config_dir: config.to_string_lossy().into_owned(),
        provider,
        sid: sid.to_string(),
        transcript_path: transcript_path.to_string_lossy().into_owned(),
        cwd: work_dir.to_string_lossy().into_owned(),
        created_at_ms: now,
        ack_deadline_ms: now.saturating_add(ACK_DEADLINE_MS),
        phase: RecallResumePhase::Launched,
    };
    Ok(RecallResumeLaunch { attempt, env })
}

fn resolve_managed_transcript(config: &std::path::Path, sid: &str) -> Result<PathBuf> {
    let projects = config.join("projects");
    let entries =
        std::fs::read_dir(&projects).with_context(|| summon_i18n::format!("rb4126d3e9e"))?;
    let target = format!("{sid}.jsonl");
    for project in entries.flatten() {
        let candidate = project.path().join(&target);
        if candidate.is_file() {
            let canonical = std::fs::canonicalize(&candidate)
                .with_context(|| format!("canonicalize {candidate:?}"))?;
            if canonical.is_file() && canonical.starts_with(config) {
                return Ok(canonical);
            }
        }
    }
    bail!(summon_i18n::format!("r56474b35ee"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RecallResumeStats {
    pub acked: u64,
    pub bound: u64,
    pub failed: u64,
    pub reaped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecallResumeStep {
    Waiting,
    Acked,
    Bound,
    Failed,
    Reaped,
}

/// 每 tick 驱动所有 recall 全托管 attempt。单个 attempt 出错只 log + 保留(下 tick 重试),
/// 不影响其它 attempt——与 coordinator_tick 一样 fail-closed。
pub(crate) fn recall_resume_tick(
    store: &BridgeStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
) -> Result<RecallResumeStats> {
    let mut stats = RecallResumeStats::default();
    for mut attempt in store.load_recall_resume_attempts()? {
        match advance_recall_resume_attempt(store, outbox, &mut attempt, now_ms()) {
            Ok(RecallResumeStep::Waiting) => {}
            Ok(RecallResumeStep::Acked) => stats.acked = stats.acked.saturating_add(1),
            Ok(RecallResumeStep::Bound) => stats.bound = stats.bound.saturating_add(1),
            Ok(RecallResumeStep::Failed) => stats.failed = stats.failed.saturating_add(1),
            Ok(RecallResumeStep::Reaped) => stats.reaped = stats.reaped.saturating_add(1),
            Err(error) => log(&summon_i18n::format!("rbe0b587bb6", attempt.id)),
        }
    }
    Ok(stats)
}

fn advance_recall_resume_attempt(
    store: &BridgeStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    attempt: &mut RecallResumeAttempt,
    now: u64,
) -> Result<RecallResumeStep> {
    match attempt.phase.clone() {
        // 终态残留(崩溃在 remove 之前):幂等清记录 + ACK。
        RecallResumePhase::Bound | RecallResumePhase::Failed => {
            store.remove_terminal_recall_resume_attempt(&attempt.id)?;
            Ok(RecallResumeStep::Reaped)
        }
        RecallResumePhase::Launched => advance_launched(store, outbox, attempt, now),
        RecallResumePhase::Acked { staging, .. } => advance_acked(store, outbox, attempt, staging),
    }
}

fn advance_launched(
    store: &BridgeStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    attempt: &mut RecallResumeAttempt,
    now: u64,
) -> Result<RecallResumeStep> {
    let Some(envelope) = store.load_handoff_ack(&attempt.id)? else {
        if now > attempt.ack_deadline_ms {
            fail_recall_resume(store, outbox, attempt, summon_i18n::text!("r3e82666e70"))?;
            return Ok(RecallResumeStep::Failed);
        }
        return Ok(RecallResumeStep::Waiting);
    };
    // 有 ACK:必须有存活 staging 才能证明 owns_process;staging 消失 = 被恢复进程已死,fail-closed。
    let Some(owned) =
        OwnedHandoffStaging::recover_named(&attempt.staging_name, &attempt.ownership_marker)?
    else {
        fail_recall_resume(store, outbox, attempt, summon_i18n::text!("rfc3bb6cfe7"))?;
        return Ok(RecallResumeStep::Failed);
    };
    let owns = match envelope.process.as_ref() {
        Some(process) => owned.owns_process(process.pid)?,
        None => false,
    };
    match verify_recall_ack(attempt, &envelope, owns) {
        Ok(resumed) => {
            attempt.phase = RecallResumePhase::Acked {
                resumed,
                staging: owned.identity().clone(),
            };
            store.persist_recall_resume_attempt(attempt)?;
            Ok(RecallResumeStep::Acked)
        }
        Err(error) => {
            fail_recall_resume(store, outbox, attempt, &summon_i18n::format!("r0583e6e3f2"))?;
            Ok(RecallResumeStep::Failed)
        }
    }
}

fn advance_acked(
    store: &BridgeStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    attempt: &mut RecallResumeAttempt,
    staging: TmuxSessionIdentity,
) -> Result<RecallResumeStep> {
    // 1) staging → canonical。canonical 缺席 rename 才成;已被占则放弃并清自己的 staging。
    if has_session_strict(&staging.name)? {
        let owned =
            OwnedHandoffStaging::recover(&staging).context(summon_i18n::text!("re79f8b2756"))?;
        match owned.rename_exact(&attempt.canonical_tmux) {
            Ok(_renamed) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // canonical 已被占(并发 cold-start 抢名等):放弃本次恢复,只清自己的 staging。
                fail_recall_resume(store, outbox, attempt, summon_i18n::text!("rad52b0fd0b"))?;
                return Ok(RecallResumeStep::Failed);
            }
            Err(error) => {
                // 其它(可能瞬时)tmux 错误:保留 Acked,下 tick 重试,绝不猜测性绑群。
                return Err(anyhow::Error::new(error).context("recall staging rename canonical"));
            }
        }
    } else {
        // staging 名已不在:或已改名成 canonical(崩溃在 rename 后),或 staging 进程已死。
        if !has_session_strict(&attempt.canonical_tmux)? {
            fail_recall_resume(store, outbox, attempt, summon_i18n::text!("r9420589620"))?;
            return Ok(RecallResumeStep::Failed);
        }
        // canonical 在:必须是我们改名后的那个(session_id/created/pane 一致)才绑;否则(外部同名
        // 会话抢占)绝不绑定、也绝不碰它。
        if RenamedHandoffTmux::recover(&staging, &attempt.canonical_tmux).is_err() {
            fail_recall_resume(store, outbox, attempt, summon_i18n::text!("raa320927fe"))?;
            return Ok(RecallResumeStep::Failed);
        }
    }
    // 2) 绑群三件套(幂等覆盖写)。
    bind_group_to_canonical(attempt)?;
    attempt.phase = RecallResumePhase::Bound;
    store.persist_recall_resume_attempt(attempt)?;
    store.remove_terminal_recall_resume_attempt(&attempt.id)?;
    log(&summon_i18n::format!(
        "r8c1182f2a3",
        attempt.id,
        attempt.canonical_tmux,
        attempt.target.chat_id
    ));
    enqueue_recall_receipt(outbox, attempt, "bound", &bound_receipt_text(attempt));
    Ok(RecallResumeStep::Bound)
}

/// 成功回执正文。**不列 sid / tmux 名**:飞书侧看的是「这个会话之前聊到哪」,机器标识对人没用
/// (排查用的 tmux 名私聊卡片已经给过)。带上最近一条 assistant 回复当上下文锚点——历史正文只该
/// 在**接管成功这一刻**出现一次,而不是等下次谁碰这个会话时被镜像当 backlog 补投(实测踩过)。
fn bound_receipt_text(attempt: &RecallResumeAttempt) -> String {
    const HEAD: &str = "rb056e940ad";
    match last_assistant_reply(&attempt.transcript_path) {
        Some(text) => summon_i18n::format!(
            "r209c1f7e53",
            quote_block(&text),
            HEAD = ::summon_core::i18n::label(HEAD)
        ),
        None => summon_i18n::format!("r451d27d330", HEAD = ::summon_core::i18n::label(HEAD)),
    }
}

/// 取 transcript 里最后一条**可见 assistant** 正文,截断到适合当预览的长度。
/// 复用 `read_context_capsule`:流式 + 有界内存,且它本身就挡住了 thinking/tool/sidechain
/// ——绝不能为了做预览另开一条能读到不可见内容的旁路。
fn last_assistant_reply(transcript_path: &str) -> Option<String> {
    use summon_core::claude_transcript::{read_context_capsule, CapsuleLimits, CapsuleRole};
    let file = std::fs::File::open(transcript_path).ok()?;
    let capsule = read_context_capsule(
        std::io::BufReader::new(file),
        CapsuleLimits {
            max_messages: 4,
            max_output_bytes: 8 * 1024,
            ..CapsuleLimits::default()
        },
    )
    .ok()?;
    let text = capsule
        .messages
        .iter()
        .rev()
        .find(|m| m.role == CapsuleRole::Assistant)
        .map(|m| m.text.trim())
        .filter(|t| !t.is_empty())?;
    Some(clip_preview(&collapse_blank_lines(text), PREVIEW_CHARS))
}

/// 预览长度,单位是**字符不是字节**。首版按字节切,中文一字 3 字节 → 600 字节只剩 200 字,
/// 实测「省略得太厉害」。卡片总预算是 20000 字符级别,预览给 1200 字绰绰有余。
const PREVIEW_CHARS: usize = 1200;

/// 连续空行压成一个。历史正文常是长 markdown,而引用块里每个空行都渲染成一个孤零零的 `>`,
/// 纵向被撑得极长(实测截图)。
fn collapse_blank_lines(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() && out.last().is_some_and(|p: &&str| p.trim().is_empty()) {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

/// 按**字符**截断,并尽量落在行边界(切在句子中间读起来像坏掉了,实测切出过 `Error | You are…`)。
/// 只有当行边界不至于砍掉太多(≥ 60% 预算)时才用它,否则退回字符边界。
/// 绝不按字节切——`/remind` 曾因切在 UTF-8 码点中间 panic 掉整个 daemon。
fn clip_preview(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return close_dangling_fence(text.to_string());
    }
    let cut = text
        .char_indices()
        .nth(max_chars)
        .map_or(text.len(), |(i, _)| i);
    let head = &text[..cut];
    let body = match head.rfind('\n') {
        Some(nl) if head[..nl].chars().count() * 10 >= max_chars * 6 => &head[..nl],
        _ => head,
    };
    close_dangling_fence(format!("{}…", body.trim_end()))
}

/// 截断可能把 ``` 代码块切成半开状态,后面的正文会被卡片当成代码整块吞掉。奇数个围栏就补一个。
fn close_dangling_fence(text: String) -> String {
    if text
        .lines()
        .filter(|l| l.trim_start().starts_with("```"))
        .count()
        % 2
        == 0
    {
        return text;
    }
    format!("{text}\n```")
}

/// 渲染成飞书引用块:**每一行**都要带 `> `,否则只有首行被引用、其余散在正文里。
fn quote_block(text: &str) -> String {
    text.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 终态回执入队(**成功/失败都发**)。私聊卡片已经向用户承诺「数秒内完成后即可直接在群里继续
/// 对话」,却只写 daemon 日志——用户只能盯着群干等,成败都判断不了;失败时更是彻底静默。并列的
/// Takeover 状态机(`handoff.rs`)五个相位全都发群内状态,这里必须对齐。
///
/// 确定性 id ⇒ 崩溃后重入不会重复发。入队失败**只记日志不上抛**:回执是旁路事实,绝不能让它
/// 把一次已经成功的绑群回滚成 Err(下个 tick 会重跑 rename/绑群,徒增风险)。
fn enqueue_recall_receipt(
    outbox: &crate::runtime::outbox_store::OutboxStore,
    attempt: &RecallResumeAttempt,
    slug: &str,
    text: &str,
) {
    use summon_core::outbox::{bridge_delivery_id, BridgeDeliveryKind, OutboxEvent, OutboxKind};
    let event = OutboxEvent {
        id: bridge_delivery_id(
            BridgeDeliveryKind::RecallResumeReceipt,
            &format!("{}-{slug}", attempt.id),
        ),
        agent: attempt.target.agent.clone(),
        delivery_scope: attempt.target.scope.clone(),
        created_ms: now_ms(),
        attempts: 0,
        next_attempt_ms: now_ms(),
        last_error: None,
        kind: OutboxKind::TurnReply {
            chat_id: attempt.target.chat_id.clone(),
            chat_type: attempt.target_chat_type.clone(),
            text: text.to_string(),
            parent_message_id: None,
        },
    };
    if let Err(error) = outbox.enqueue_once(&event) {
        log(&summon_i18n::format!("r0e5b750281", attempt.id));
    }
}

/// 失败收口:只清自己的 staging(identity 复验),**绝不碰 canonical/soul-***;staging 已不在则
/// no-op。先落 Failed 再删记录,保证崩溃可幂等重入终态清理。
fn fail_recall_resume(
    store: &BridgeStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
    attempt: &mut RecallResumeAttempt,
    reason: &str,
) -> Result<()> {
    if let Some(staging) =
        OwnedHandoffStaging::recover_named(&attempt.staging_name, &attempt.ownership_marker)?
    {
        staging
            .cleanup()
            .with_context(|| summon_i18n::format!("rdd79b6c7d7"))?;
    }
    attempt.phase = RecallResumePhase::Failed;
    store.persist_recall_resume_attempt(attempt)?;
    store.remove_terminal_recall_resume_attempt(&attempt.id)?;
    log(&summon_i18n::format!("r94e6c672c8", attempt.id));
    enqueue_recall_receipt(
        outbox,
        attempt,
        "failed",
        &summon_i18n::format!("r73ef22e318"),
    );
    Ok(())
}

/// 绑群三件套:resume 记录 + takeover marker + chat_meta。全是覆盖写,重入幂等。
fn bind_group_to_canonical(attempt: &RecallResumeAttempt) -> Result<()> {
    let agent = load_target_agent(&attempt.target)?;
    anyhow::ensure!(
        agent.tmux_name_for_chat(&attempt.target_chat_type, &attempt.target.chat_id, false)
            == attempt.canonical_tmux,
        summon_i18n::format!("r4e15060629")
    );
    let session = attempt.session_key();
    crate::session::resume_store::write_checked(
        &agent,
        &attempt.canonical_tmux,
        "claude",
        &attempt.sid,
        Some(&attempt.cwd),
        None,
    )?;
    crate::session::resume_store::mark_takeover(&agent, &attempt.canonical_tmux, &session)?;
    crate::runtime::daemon::write_chat_meta_checked(
        &agent,
        &attempt.canonical_tmux,
        &attempt.target.chat_id,
        &attempt.target_chat_type,
        None,
        &attempt.actor_open_id,
        Some(&attempt.actor_open_id),
    )?;
    // 恢复成功 = 这个 chat 开了一条新的路由生命周期。**必须显式写 Open**:bridge 不经过
    // `handle_task` 的 admission,若目标 chat 上一次是被关掉的,残留的 `closed` 会让刚恢复
    // 出来的会话在列表里显示成已关闭甚至误报泄漏。
    crate::session::lifecycle::mark_open(
        &agent,
        &summon_core::registry::chat_tag(&attempt.target_chat_type, &attempt.target.chat_id),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

fn load_target_agent(target: &ScopedChatKey) -> Result<Agent> {
    let config = summon_core::config::Config::load();
    let (registry, _) = summon_core::registry::Registry::load_tolerant(&config.agents_dir)
        .context(summon_i18n::text!("r88971fe68c"))?;
    registry
        .get(&target.agent)
        .cloned()
        .context(summon_i18n::text!("rba504a9753"))
}

fn generate_token() -> String {
    // 两个 UUID simple 拼成 64 hex:`HandoffToken::parse` 与 staging marker 校验都认。
    uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::handoff::token_digest_hex;
    use crate::bridge::store::{HandoffHookAck, HOOK_ENVELOPE_VERSION};
    use summon_core::bridge::{ClaudeConfigSource, ResumeProfile, SessionKey};

    const CONFIG: &str = "/home/bot/.claude";
    const SID: &str = "sid-abc";
    const TRANSCRIPT: &str = "/home/bot/.claude/projects/demo/sid-abc.jsonl";
    const PROGRAM: &str = "/usr/bin/claude";
    const HOME: &str = "/home/bot";

    fn raw_token() -> String {
        // 与 handoff 一致:两个 UUID simple 拼成 64 hex,`HandoffToken::parse` 与 marker 校验都认。
        uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string()
    }

    fn plan(token: &str) -> RecallResumeAttempt {
        let id = uuid::Uuid::new_v4().to_string();
        RecallResumeAttempt {
            schema_version: RECALL_RESUME_SCHEMA_VERSION,
            token_digest: HandoffTokenDigest::from_hex(&token_digest_hex(token)).unwrap(),
            staging_name: format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4()),
            ownership_marker: raw_token(),
            canonical_tmux: "soul-bot-gdeadbeef".into(),
            target: ScopedChatKey::new("app", "bot", "oc_group"),
            target_chat_type: "group".into(),
            actor_open_id: "ou_actor".into(),
            program: PROGRAM.into(),
            args: vec!["--resume".into(), SID.into()],
            home: HOME.into(),
            config_dir: CONFIG.into(),
            provider: ClaudeProvider::Default,
            sid: SID.into(),
            transcript_path: TRANSCRIPT.into(),
            cwd: "/work".into(),
            created_at_ms: 1,
            ack_deadline_ms: 60_000,
            phase: RecallResumePhase::Launched,
            id,
        }
    }

    fn good_envelope(plan: &RecallResumeAttempt, token: &str) -> HookEnvelope {
        HookEnvelope {
            schema_version: HOOK_ENVELOPE_VERSION,
            event_id: uuid::Uuid::new_v4().to_string(),
            observed_at_ms: 2,
            session: SessionKey::new(CONFIG, SID),
            transcript_path: TRANSCRIPT.into(),
            cwd: "/work".into(),
            process: Some(ProcessFingerprint {
                pid: 4242,
                start_time: "s".into(),
                executable: PROGRAM.into(),
            }),
            resume_profile: ResumeProfile {
                claude_bin: PROGRAM.into(),
                home: HOME.into(),
                config_dir: CONFIG.into(),
                process_verified: true,
                environment_verified: true,
                config_source: ClaudeConfigSource::Default,
                provider: ClaudeProvider::Default,
                ..ResumeProfile::default()
            },
            content_permit: None,
            managed_takeover: false,
            handoff_ack: Some(HandoffHookAck {
                attempt_id: plan.id.clone(),
                token_sha256: token_digest_hex(token),
            }),
            event: HookEvent::SessionStart {
                source: "resume".into(),
                model: Some("sonnet".into()),
            },
        }
    }

    #[test]
    fn genuine_resume_ack_passes_and_returns_resumed_process() {
        let token = raw_token();
        let plan = plan(&token);
        let envelope = good_envelope(&plan, &token);
        let resumed = verify_recall_ack(&plan, &envelope, true).expect("真续接应过三重锁");
        assert_eq!(resumed.pid, 4242);
    }

    #[test]
    fn cold_start_source_startup_is_rejected() {
        let token = raw_token();
        let plan = plan(&token);
        let mut envelope = good_envelope(&plan, &token);
        envelope.event = HookEvent::SessionStart {
            source: "startup".into(),
            model: None,
        };
        assert!(
            verify_recall_ack(&plan, &envelope, true).is_err(),
            "冷启 source!=resume 必拒"
        );
    }

    #[test]
    fn cold_start_new_sid_is_rejected() {
        let token = raw_token();
        let plan = plan(&token);
        let mut envelope = good_envelope(&plan, &token);
        // 冷启会得到新 sid。
        envelope.session = SessionKey::new(CONFIG, "sid-cold-new");
        assert!(
            verify_recall_ack(&plan, &envelope, true).is_err(),
            "新 sid 必拒"
        );
    }

    #[test]
    fn cold_start_new_transcript_is_rejected() {
        let token = raw_token();
        let plan = plan(&token);
        let mut envelope = good_envelope(&plan, &token);
        envelope.transcript_path = "/home/bot/.claude/projects/demo/sid-cold-new.jsonl".into();
        assert!(
            verify_recall_ack(&plan, &envelope, true).is_err(),
            "新 transcript 必拒"
        );
    }

    #[test]
    fn wrong_token_is_rejected() {
        let token = raw_token();
        let plan = plan(&token);
        // ACK 携带另一条 token 的 digest。
        let other = raw_token();
        let mut envelope = good_envelope(&plan, &token);
        envelope.handoff_ack = Some(HandoffHookAck {
            attempt_id: plan.id.clone(),
            token_sha256: token_digest_hex(&other),
        });
        assert!(
            verify_recall_ack(&plan, &envelope, true).is_err(),
            "坏 token 必拒"
        );
    }

    #[test]
    fn wrong_attempt_id_is_rejected() {
        let token = raw_token();
        let plan = plan(&token);
        let mut envelope = good_envelope(&plan, &token);
        envelope.handoff_ack = Some(HandoffHookAck {
            attempt_id: uuid::Uuid::new_v4().to_string(),
            token_sha256: token_digest_hex(&token),
        });
        assert!(
            verify_recall_ack(&plan, &envelope, true).is_err(),
            "attempt_id 不符必拒"
        );
    }

    /// (把某个字段改坏, 期望被点名的字段名)。
    type ProfileDriftCase = (fn(&mut ResumeProfile), &'static str);

    #[test]
    fn profile_drift_is_rejected_field_by_field() {
        let token = raw_token();
        let plan = plan(&token);
        // 逐字段漂移都必须 fail-closed,且报错**点名**是哪个字段(非捕获闭包 → fn 指针,
        // 避免 Box<dyn Fn> 的复杂类型)。
        let mutators: [ProfileDriftCase; 7] = [
            (|p| p.claude_bin = "/other/claude".into(), "claude_bin"),
            (|p| p.home = "/other/home".into(), "home"),
            (|p| p.config_dir = "/other/.claude".into(), "config_dir"),
            (
                |p| p.config_source = ClaudeConfigSource::Explicit,
                "config_source",
            ),
            (|p| p.provider = ClaudeProvider::Custom, "provider"),
            (|p| p.process_verified = false, "process_verified"),
            (|p| p.environment_verified = false, "environment_verified"),
        ];
        for (mutate, field) in mutators {
            let mut envelope = good_envelope(&plan, &token);
            mutate(&mut envelope.resume_profile);
            let error = verify_recall_ack(&plan, &envelope, true).expect_err("profile 漂移必拒");
            let text = format!("{error:#}");
            // 用 `字段(期望=` 作锚点:路径值里本来就含 "home" 等字样,裸子串匹配会误判。
            for other in mutators.iter().map(|(_, name)| *name) {
                let named = text.contains(&format!("{other}(期望="));
                assert_eq!(named, other == field, "点名字段应恰好是 {field}:{text}");
            }
        }
    }

    /// 诊断增强不得把密钥打进日志:漂移报错只带路径/枚举/布尔,绝不含 raw token 或其摘要。
    #[test]
    fn profile_drift_error_never_leaks_token_or_digest() {
        let token = raw_token();
        let plan = plan(&token);
        let mut envelope = good_envelope(&plan, &token);
        envelope.resume_profile.claude_bin = "/other/claude".into();
        let error = verify_recall_ack(&plan, &envelope, true).expect_err("profile 漂移必拒");
        let text = format!("{error:#}");
        assert!(!text.contains(&token), "报错不得含 raw token:{text}");
        assert!(
            !text.contains(&token_digest_hex(&token)),
            "报错不得含 token 摘要:{text}"
        );
        assert!(text.contains("claude_bin") && text.contains("/other/claude"));
    }

    #[test]
    fn not_owned_process_is_rejected() {
        let token = raw_token();
        let plan = plan(&token);
        let envelope = good_envelope(&plan, &token);
        assert!(
            verify_recall_ack(&plan, &envelope, false).is_err(),
            "staging 不拥有该进程必拒"
        );
    }

    #[test]
    fn missing_or_incomplete_process_is_rejected() {
        let token = raw_token();
        let plan = plan(&token);
        let mut envelope = good_envelope(&plan, &token);
        envelope.process = None;
        assert!(
            verify_recall_ack(&plan, &envelope, true).is_err(),
            "缺 process 必拒"
        );

        let mut envelope = good_envelope(&plan, &token);
        envelope.process = Some(ProcessFingerprint {
            pid: 0,
            start_time: String::new(),
            executable: String::new(),
        });
        assert!(
            verify_recall_ack(&plan, &envelope, true).is_err(),
            "不完整指纹必拒"
        );
    }

    #[test]
    fn non_session_start_event_is_rejected() {
        let token = raw_token();
        let plan = plan(&token);
        let mut envelope = good_envelope(&plan, &token);
        envelope.event = HookEvent::Stop {
            prompt_id: None,
            last_assistant_message: None,
            stop_hook_active: false,
            has_pending_wakeups: false,
        };
        assert!(
            verify_recall_ack(&plan, &envelope, true).is_err(),
            "非 SessionStart 必拒"
        );
    }

    #[test]
    fn launched_token_digest_only_exposed_in_launched_phase() {
        let token = raw_token();
        let mut plan = plan(&token);
        assert!(plan.launched_token_digest().is_some());
        plan.phase = RecallResumePhase::Bound;
        assert!(
            plan.launched_token_digest().is_none(),
            "终态不再接受 ACK 顶替"
        );
    }

    /// 结构守护(仿 handoff.rs 的 strict_handoff_never_calls_generic_session_launch_paths):
    /// recall_resume 的生产代码绝不能调用会冷启/fallback/注入 agent 配置的通用启动路径。
    /// never-cold-start 只能靠 staging + rename_exact + 三重锁,不许退回 ensure_started。
    #[test]
    fn recall_resume_never_calls_generic_session_launch_paths() {
        let source = include_str!("recall_resume.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("recall_resume production source");
        for forbidden in ["ensure_started(", "cold_start(", "augment_launch("] {
            assert!(
                !production.contains(forbidden),
                "recall_resume 不得调用会 fallback/cold-start 的通用路径:{forbidden}"
            );
        }
    }

    /// 回执必须投到**发起恢复的原群**(而不是点按钮的私聊),两个终态各自一条,且确定性 id 让
    /// 崩溃重入不会重复发。实测缺陷:绑群成功只写 daemon 日志,用户在群里干等看不到任何结果。
    #[test]
    fn receipt_targets_origin_group_and_is_idempotent_per_terminal_state() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox"));
        let attempt = plan(&raw_token());

        enqueue_recall_receipt(&outbox, &attempt, "bound", "✅ 已接管");
        enqueue_recall_receipt(&outbox, &attempt, "bound", "✅ 已接管"); // 重入
        let pending = outbox.pending_events();
        assert_eq!(pending.len(), 1, "确定性 id:重入不得重复发");
        let summon_core::outbox::OutboxKind::TurnReply {
            chat_id, chat_type, ..
        } = &pending[0].kind
        else {
            panic!("回执应为 TurnReply");
        };
        assert_eq!(chat_id, "oc_group", "必须投回原群,不是发起按钮的私聊");
        assert_eq!(chat_type, "group");
        assert_eq!(pending[0].agent, "bot");
        assert_eq!(pending[0].delivery_scope, "app");

        // 失败回执与成功回执必须是不同的 id,否则先 bound 后 failed 会被去重吞掉。
        enqueue_recall_receipt(&outbox, &attempt, "failed", "⚠️ 未完成");
        assert_eq!(outbox.pending_events().len(), 2, "两个终态各自一条");
    }

    /// owner 反馈:回执列 sid + tmux 名对飞书侧的人没用,他要的是「上次聊到哪」。这里同时钉住
    /// **历史正文只在接管成功这一刻出现**——它此前是被镜像当 backlog 在下次有人碰会话时补投的。
    #[test]
    fn bound_receipt_shows_last_reply_not_machine_identifiers() {
        let dir = tempfile::tempdir().unwrap();
        let tr = dir.path().join("t.jsonl");
        std::fs::write(
            &tr,
            "{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"老问题\"}}\n\
             {\"type\":\"assistant\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"assistant\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"上一轮结论\"}]}}\n\
             {\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"后来又问了一句\"}}\n",
        )
        .unwrap();
        let mut attempt = plan(&raw_token());
        attempt.transcript_path = tr.to_string_lossy().into_owned();

        let text = bound_receipt_text(&attempt);
        assert!(text.contains("已接管历史会话"), "{text}");
        assert!(
            text.contains("> 上一轮结论"),
            "要带最近一条 assistant 回复当锚点: {text}"
        );
        assert!(
            !text.contains("后来又问了一句"),
            "锚点取 assistant,不是最后一条可见消息: {text}"
        );
        assert!(
            !text.contains(&attempt.sid) && !text.contains(&attempt.canonical_tmux),
            "不得把 sid / tmux 名摆给飞书侧用户看: {text}"
        );

        // 没有历史回复时不得渲染空引用块。
        attempt.transcript_path = dir
            .path()
            .join("missing.jsonl")
            .to_string_lossy()
            .into_owned();
        let empty = bound_receipt_text(&attempt);
        assert!(empty.contains("还没有历史回复"), "{empty}");
        assert!(!empty.contains('>'), "无正文时不该留空引用块: {empty}");
    }

    /// 截断必须落在字符边界:`/remind` 曾因按字节切多字节文本 panic 掉整个 daemon。
    #[test]
    fn preview_budget_counts_characters_not_bytes() {
        let cjk = "验证一下多字节截断".repeat(300); // 2700 字 = 8100 字节
        let clipped = clip_preview(&cjk, 1200);
        let kept = clipped.trim_end_matches('…').chars().count();
        assert!(
            (1000..=1200).contains(&kept),
            "1200 字预算应保住约 1200 个字符,实得 {kept}"
        );
        assert!(clipped.ends_with('…'));
        assert!(cjk.starts_with(clipped.trim_end_matches('…')));
        assert_eq!(clip_preview("短", 1200), "短", "不超长不动它");
    }

    /// 截断优先落在行边界:切在句子中间读起来像坏掉了(实测切出过 `Error | You are…`)。
    /// 但行边界砍掉太多时宁可切字符——否则一段超长首行会把预览削成空的。
    #[test]
    fn preview_prefers_line_boundary_but_not_at_any_cost() {
        let lines = "第一行内容\n第二行内容\n第三行会被截断的内容";
        let clipped = clip_preview(lines, 14);
        assert!(!clipped.contains("第三行"), "越预算的行整行丢掉: {clipped}");
        assert!(clipped.starts_with("第一行内容\n第二行内容"), "{clipped}");

        // 首行就超预算:退回字符边界,不能因为「没有可用行边界」而输出空串。
        let one_long_line = "没有换行的超长内容".repeat(50);
        let clipped = clip_preview(&one_long_line, 20);
        assert!(
            clipped.trim_end_matches('…').chars().count() >= 15,
            "{clipped}"
        );
    }

    /// 空行压缩 + 引用前缀 + 半开代码块收口。截图里每个空行都渲染成孤零零的 `>`,
    /// 而截断切开 ``` 会让后面的正文被卡片当成代码整块吞掉。
    #[test]
    fn preview_collapses_blanks_quotes_all_lines_and_closes_fences() {
        assert_eq!(
            collapse_blank_lines("a\n\n\n\nb"),
            "a\n\nb",
            "连续空行压成一个"
        );
        assert_eq!(collapse_blank_lines("\n\na\n"), "a", "首尾空白清掉");
        assert_eq!(quote_block("a\nb\nc"), "> a\n> b\n> c", "每一行都要带前缀");

        let cut_in_fence = clip_preview("说明\n```\nlet x = 1;\nlet y = 2;\n", 10);
        assert_eq!(
            cut_in_fence
                .lines()
                .filter(|l| l.trim_start().starts_with("```"))
                .count()
                % 2,
            0,
            "半开代码块必须补齐: {cut_in_fence}"
        );
    }

    /// 两个终态**都**要回执。失败静默比成功静默更糟(用户永远等不到结果),这里钉死两条路径,
    /// 防后续重构只保留成功分支。并列的 Takeover 状态机五个相位全发,recall 不得倒退。
    #[test]
    fn both_terminal_paths_enqueue_a_receipt() {
        let source = include_str!("recall_resume.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("recall_resume production source");
        // 窗口必须**掐到本分支收口为止**:开到文件尾会把 `fn enqueue_recall_receipt` 的定义和
        // 另一条终态的调用一起圈进来,守护就成了永真式(实测:删掉 Bound 回执后仍能过)。
        let bound_body = slice_between(
            production,
            "RecallResumePhase::Bound;",
            "Ok(RecallResumeStep::Bound)",
        );
        assert!(
            bound_body.contains("enqueue_recall_receipt("),
            "绑群成功后必须发群内回执"
        );
        let fail_body = slice_between(production, "fn fail_recall_resume(", "\n    Ok(())\n}");
        assert!(
            fail_body.contains("enqueue_recall_receipt("),
            "失败收口必须发群内回执,不得静默"
        );
    }

    /// 取 `[start, end)` 之间的源码切片;任一锚点缺失即 panic(锚点漂移必须炸,不能悄悄放空)。
    fn slice_between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        let from = source
            .find(start)
            .unwrap_or_else(|| panic!("源码锚点缺失:{start}"));
        let len = source[from..]
            .find(end)
            .unwrap_or_else(|| panic!("源码收口锚点缺失:{end}"));
        &source[from..from + len]
    }

    /// 真机:起一个 recall staging(fake program),`fail_recall_resume` 只清它自己,
    /// 所有既有 soul-* 前后完全一致(守 CLAUDE.md 第一红线)。
    #[test]
    #[ignore = "需要真实 tmux server"]
    fn fail_recall_resume_cleans_only_its_own_staging() {
        let soul_before = crate::session::tmux::list_sessions_with_prefix("soul-");
        let dir = tempfile::tempdir().unwrap();
        let store = BridgeStore::new(dir.path().join("bridge"));
        let token = generate_token();
        let mut attempt = plan(&token);
        attempt.ownership_marker = generate_token();
        attempt.staging_name = format!("{STAGING_PREFIX}{}", attempt.id);
        let script = dir.path().join("fake claude.sh");
        crate::test_support::write_test_executable(&script, "#!/bin/sh\nexec /bin/cat\n");
        let staging = create_handoff_staging(
            &attempt.staging_name,
            &attempt.ownership_marker,
            dir.path(),
            &script,
            &[],
            &[],
        )
        .unwrap();
        assert!(has_session_strict(&attempt.staging_name).unwrap());
        store.persist_recall_resume_attempt(&attempt).unwrap();
        let outbox = crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox"));
        fail_recall_resume(&store, &outbox, &mut attempt, "test cleanup").unwrap();
        assert!(
            !has_session_strict(&attempt.staging_name).unwrap(),
            "staging 应被清"
        );
        assert!(
            !store.recall_resume_path(&attempt.id).exists(),
            "记录应被回收"
        );
        assert_eq!(
            crate::session::tmux::list_sessions_with_prefix("soul-"),
            soul_before,
            "绝不触碰 soul-* 持久层"
        );
        let _ = staging; // 已被 cleanup 杀掉,drop 只丢所有权句柄
    }
}
