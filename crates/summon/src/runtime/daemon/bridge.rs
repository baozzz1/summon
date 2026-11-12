//! External Claude bridge 的飞书控制面。这里只做 ticket/鉴权/占用与 Takeover reservation；
//! 正文投递、source-exit/resume/ACK 编排分别由 drain/reconcile 与 handoff coordinator 承担。

use super::*;
use crate::frontend::outbound::send_card_idempotent_archived_id;
use anyhow::{Context, Result};
use summon_core::bridge::{
    BeginChoiceOutcome, BridgeChatGoneAction, BridgeCloseReason, BridgeMode, BridgeState,
    BridgeTicket, ChatSlot, HandoffBeginProof, ScopedChatKey, TicketStatus,
};
use summon_core::ipc::BridgeCommand;

/// Bridge 的 operate 判定。**只认 agent 级名单,本群名单不算数**:召回列的是本机该 CLI 的
/// 全部磁盘会话(session/cwd/prompt),接管会向本机外部进程发信号 —— 效果都不落在发起的那个群里。
/// 群语义硬编码(`is_group = true`):目标恒为群,名单为空必须读成拒绝 —— 名单皆空时应用所有者的
/// 放行只限本群范围的动作,这里同样拒绝。
pub(super) fn bridge_operator_allowed(agent: &Agent, open_id: &str, union_id: &str) -> bool {
    agent.can_operate(open_id, union_id, true, OperateScope::Agent)
}

const CONTROL_RECOVERY_PAGE_SIZE: usize = 12;
/// 召回卡每页会话数(飞书卡片不宜过长;翻页由 card_recall 的 recall_page 按钮承载)。
const RECALL_PAGE_SIZE: usize = 10;
const RPC_HANDOFF_CARD_TTL_MS: u64 = 10 * 60 * 1_000;
const RPC_HANDOFF_CARD_CLOCK_SKEW_MS: u64 = 60 * 1_000;
static RECALL_TARGET_LIFECYCLE_HEALTHY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

fn recall_target_lifecycle_path(agent: &Agent, chat_id: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{}.recall-target", chat_tag("group", chat_id)))
}

fn rotate_recall_target_lifecycle(agent: &Agent, chat_id: &str) -> Result<String> {
    let token = uuid::Uuid::new_v4().to_string();
    let result = crate::runtime::fsio::atomic_write(
        &recall_target_lifecycle_path(agent, chat_id),
        format!("{token}\n").as_bytes(),
    )
    .context(summon_i18n::text!("rd6540c9e0f"))
    .map(|()| token);
    if result.is_err() {
        // 发新卡与 ChatGone 都走这条底层写。任一 target 代际无法持久化时，旧 token 是否仍
        // 有效已经不可证明；全局冻结比让旧私聊卡复活一个终态群安全。
        RECALL_TARGET_LIFECYCLE_HEALTHY.store(false, std::sync::atomic::Ordering::Release);
    }
    result
}

pub(super) fn invalidate_recall_target_lifecycle(agent: &Agent, chat_id: &str) -> Result<()> {
    // 写新代而不是 remove：旧卡手里的 token 必须与“群已终态”形成可比较的 durable revision。
    let result = rotate_recall_target_lifecycle(agent, chat_id).map(|_| ());
    if result.is_err() {
        RECALL_TARGET_LIFECYCLE_HEALTHY.store(false, std::sync::atomic::Ordering::Release);
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtectedRecallLifecycle {
    agent: String,
    chat_id: String,
    token: String,
}

fn protected_recall_lifecycles(
    records: &[crate::runtime::card_action_wal::CardActionRecord],
    now_ms: u64,
) -> Vec<ProtectedRecallLifecycle> {
    records
        .iter()
        .filter_map(|record| {
            let value = &record.card.value;
            let issued_at_ms = value.get("card_issued_at_ms")?.as_u64()?;
            let action = value.get("action")?.as_str()?;
            let agent = value.get("bound_agent")?.as_str()?;
            let scope = value.get("bound_scope")?.as_str()?;
            let requester = value.get("bound_requester")?.as_str()?;
            let chat_id = value.get("chat_id")?.as_str()?;
            let chat_type = value.get("chat_type")?.as_str()?;
            let token = value.get("target_lifecycle_token")?.as_str()?;
            (action == "recall_rpc_handoff"
                && !agent.is_empty()
                && !scope.is_empty()
                && requester == record.card.operator_open_id
                && !chat_id.is_empty()
                && matches!(chat_type, "group" | "topic_group")
                && !token.is_empty()
                && issued_at_ms <= now_ms.saturating_add(RPC_HANDOFF_CARD_CLOCK_SKEW_MS)
                && now_ms.saturating_sub(issued_at_ms) <= RPC_HANDOFF_CARD_TTL_MS)
                .then(|| ProtectedRecallLifecycle {
                    agent: agent.to_string(),
                    chat_id: chat_id.to_string(),
                    token: token.to_string(),
                })
        })
        .collect()
}

fn invalidate_recall_target_lifecycles_at(
    agents_root: &std::path::Path,
    protected: &[ProtectedRecallLifecycle],
) -> Result<(usize, usize)> {
    RECALL_TARGET_LIFECYCLE_HEALTHY.store(false, std::sync::atomic::Ordering::Release);
    let agents = match std::fs::read_dir(agents_root) {
        Ok(agents) => agents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            RECALL_TARGET_LIFECYCLE_HEALTHY.store(true, std::sync::atomic::Ordering::Release);
            return Ok((0, 0));
        }
        Err(error) => return Err(error).context(summon_i18n::text!("r840a0103f3")),
    };
    let mut rotated = 0usize;
    let mut preserved = 0usize;
    for agent in agents {
        let agent = agent.context(summon_i18n::text!("r0f9c9598f7"))?;
        let agent_dir = agent.path();
        // 与 Registry 的成员判据对齐；agents 根目录会自然出现 `.DS_Store` 等非 agent 条目。
        // 这些条目没有 manifest，也不可能持有合法 lifecycle token，必须跳过而非阻断全 daemon。
        let manifest_path = agent_dir.join("agent.yaml");
        if !manifest_path.is_file() {
            continue;
        }
        // 状态路径走 state_dir 解析(迁移后读新位置,未迁移回退旧位置)
        let alias = agent.file_name().to_string_lossy().to_string();
        let state_dir = summon_core::registry::agent_state_dir(agents_root, &alias);
        let agent_name = if protected.is_empty() {
            None
        } else {
            let manifest = std::fs::read_to_string(&manifest_path)
                .with_context(|| summon_i18n::format!("rb0326182a1", manifest_path.display()))?;
            #[derive(serde::Deserialize)]
            struct AgentIdentity {
                name: String,
            }
            Some(
                serde_yaml::from_str::<AgentIdentity>(&manifest)
                    .with_context(|| summon_i18n::format!("r86b7ea2def", manifest_path.display()))?
                    .name,
            )
        };
        let mut protected_paths = std::collections::HashMap::<PathBuf, String>::new();
        if let Some(agent_name) = agent_name.as_deref() {
            for capability in protected.iter().filter(|item| item.agent == agent_name) {
                let path = state_dir.join("runtime").join("chats").join(format!(
                    "{}.recall-target",
                    chat_tag("group", &capability.chat_id)
                ));
                protected_paths
                    .entry(path)
                    .and_modify(|token| {
                        if token != &capability.token {
                            token.clear();
                        }
                    })
                    .or_insert_with(|| capability.token.clone());
            }
        }
        let chats = state_dir.join("runtime").join("chats");
        let entries = match std::fs::read_dir(&chats) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| summon_i18n::format!("r86279f8eef", chats.display()))
            }
        };
        for entry in entries {
            let path = entry.context(summon_i18n::text!("r6006bd086a"))?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("recall-target") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| summon_i18n::format!("rf652d19871", path.display()))?;
            anyhow::ensure!(
                metadata.file_type().is_file(),
                summon_i18n::format!("rd6e4cca6c4", path.display())
            );
            if protected_paths.get(&path).is_some_and(|expected| {
                !expected.is_empty()
                    && std::fs::read_to_string(&path)
                        .ok()
                        .is_some_and(|actual| actual.trim() == expected)
            }) {
                preserved += 1;
                continue;
            }
            crate::runtime::fsio::atomic_write(
                &path,
                format!("{}\n", uuid::Uuid::new_v4()).as_bytes(),
            )
            .with_context(|| summon_i18n::format!("rf478f65dc1", path.display()))?;
            rotated += 1;
        }
    }
    RECALL_TARGET_LIFECYCLE_HEALTHY.store(true, std::sync::atomic::Ordering::Release);
    Ok((rotated, preserved))
}

/// daemon 每次启动都让上一代未点击的召回卡失效。唯一例外是：sidecar 已收到点击、daemon 已
/// fsync 到 durable callback WAL，且该 callback 的 token 仍与崩溃前最后一代完全一致。保留它
/// 才能兑现 ACK 后必重放；点击时仍会重验 TTL、operate、目标群实时成员关系和 source revision。
pub(super) fn invalidate_all_recall_target_lifecycles(
    replay_records: &[crate::runtime::card_action_wal::CardActionRecord],
) -> Result<(usize, usize)> {
    let protected = protected_recall_lifecycles(replay_records, now_unix_ms());
    invalidate_recall_target_lifecycles_at(
        &crate::runtime::paths::summon_home().join("agents"),
        &protected,
    )
}

fn recall_target_lifecycle_matches(agent: &Agent, chat_id: &str, expected: &str) -> bool {
    RECALL_TARGET_LIFECYCLE_HEALTHY.load(std::sync::atomic::Ordering::Acquire)
        && !expected.is_empty()
        && std::fs::read_to_string(recall_target_lifecycle_path(agent, chat_id))
            .ok()
            .is_some_and(|actual| actual.trim() == expected)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalChatEvidence {
    pub(super) live_tmux: bool,
    pub(super) resume: crate::session::resume_store::ResumePresence,
    pub(super) legacy_resume: crate::session::resume_store::ResumePresence,
    pub(super) pending_turn_file: bool,
    pub(super) inbox_pending: bool,
    pub(super) inbox_recovery_pending: bool,
    pub(super) local_outbox_pending: bool,
    pub(super) inflight: bool,
    pub(super) queued_local_work: bool,
    pub(super) remote_bound: bool,
}

/// 拒绝回执送到哪里:群里 `/bridge` 的回执就在目标群里,召回卡回调的回执在私聊。解法里的
/// `/reset` 必须指明在目标群发 —— 在私聊里照着发,重置的是私聊自己的会话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReplyPlace {
    TargetGroup,
    Private,
}

impl ReplyPlace {
    /// (怎么称呼目标群, 去哪发 `/reset`)
    fn wording(self) -> (&'static str, &'static str) {
        match self {
            ReplyPlace::TargetGroup => (
                summon_i18n::text!("r0d5f4509dd"),
                summon_i18n::text!("r8c7059e4aa"),
            ),
            ReplyPlace::Private => (
                summon_i18n::text!("r2186962e00"),
                summon_i18n::text!("r9f9c744fea"),
            ),
        }
    }
}

fn remote_bound_blocker(place: ReplyPlace) -> String {
    let (chat, reset) = place.wording();
    summon_i18n::format!("r33de6eb1cc")
}

/// 群是否正在遥控 cmux 会话。已结束的记录不算占用(群里下一条消息会告知并删掉它,也允许重新
/// 绑定);记录读不出按错误返回,调用方 fail-closed。
fn remote_binding_live(agent: &Agent, chat_id: &str) -> Result<bool> {
    Ok(crate::remote::store::RemoteStore::for_agent(agent)
        .load(chat_id)
        .with_context(|| summon_i18n::format!("r3f42f12c36"))?
        .is_some_and(|binding| binding.ended.is_none()))
}

const SESSION_REMOTE_CONTROLLED: &str = "r89b4569f9c";
const REMOTE_RECORDS_UNREADABLE: &str = "rf55257c8c7";

/// 接管 / 恢复一条 Claude 会话会结束它的进程或另起一个 writer,遥控中的进程一个信号都不能收。
/// 按会话查遥控绑定(目标群查的是群,挡不住别的群在遥控同一条会话),返回拒绝回执;遥控记录
/// 读不出同样拒绝。遥控记录按 agent 存放,别的 agent 的遥控查不到。
///
/// `pid` 是这次要结束的进程:信号打的是 pid,而同一个进程里 `/clear` 会换掉 sid,只比 sid 会
/// 放过绑在旧 sid 上、仍在同一个进程里的遥控。调用方此刻还不知道 pid(找候选之前、历史会话、
/// 不发信号的恢复)时传 None,只按 sid 查。pid 被复用时会误拒,按 fail-closed 接受。
fn session_remote_refusal(agent: &Agent, sid: &str, pid: Option<u32>) -> Option<&'static str> {
    let bindings = crate::remote::store::RemoteStore::for_agent(agent).list();
    match bindings.map(|bindings| {
        bindings.into_iter().find(|binding| {
            binding.ended.is_none()
                && (binding.session_id == sid || pid.is_some_and(|pid| binding.pid == pid))
        })
    }) {
        Ok(None) => None,
        Ok(Some(binding)) => {
            log(&summon_i18n::format!(
                "r704d5adb68",
                agent.name,
                binding.chat_id,
                binding.session_id,
                binding.pid
            ));
            Some(::summon_core::i18n::label(SESSION_REMOTE_CONTROLLED))
        }
        Err(error) => {
            log(&summon_i18n::format!("r8077422012", agent.name));
            Some(::summon_core::i18n::label(REMOTE_RECORDS_UNREADABLE))
        }
    }
}

const SESSION_TAKEOVER_PENDING: &str = "r37fd1cc4d8";
const TAKEOVER_RECORDS_UNREADABLE: &str = "r57fbf150fa";

/// 这条会话是否有还没走完的 Bridge 接管:接管会在安全边界结束源进程,绑上遥控的进程随后就会
/// 被它结束。`pid` 是要遥控的那个进程(cmux 记录里的):接管的信号打 pid,`/clear` 换掉 sid
/// 之后只比 sid 认不出是同一个进程。还没读 cmux 记录、不知道 pid 时传 None,只按 sid 查。
fn takeover_pending_for(
    store: &crate::bridge::store::BridgeStore,
    sid: &str,
    pid: Option<u32>,
) -> Result<bool> {
    use crate::bridge::handoff::HandoffPhase;
    Ok(store.load_handoff_attempts()?.iter().any(|attempt| {
        (attempt.source().session().session_id == sid
            || pid.is_some_and(|pid| attempt.source().process().pid == pid))
            && !matches!(
                attempt.phase(),
                HandoffPhase::Completed
                    | HandoffPhase::Failed
                    | HandoffPhase::CleanupBlocked { .. }
            )
    }))
}

impl LocalChatEvidence {
    pub(super) fn occupied(&self) -> bool {
        !self.blockers(ReplyPlace::TargetGroup).is_empty()
    }

    /// 每条命中的占用证据 + 它的解法,拒绝回执逐条列出。`occupied` 由它派生:判据与回执
    /// 只有这一份,新增一类证据时不会出现「拦了却说不出为什么」。
    pub(super) fn blockers(&self, place: ReplyPlace) -> Vec<String> {
        use crate::session::resume_store::ResumePresence;
        let (chat, reset) = place.wording();
        let mut blockers = Vec::new();
        if self.remote_bound {
            blockers.push(remote_bound_blocker(place));
        }
        if self.live_tmux {
            blockers.push(summon_i18n::format!("r9443b2ac06"));
        }
        if !matches!(self.resume, ResumePresence::Missing)
            || !matches!(self.legacy_resume, ResumePresence::Missing)
        {
            blockers.push(summon_i18n::format!("rbab765eb5e"));
        }
        if self.inflight || self.queued_local_work {
            blockers.push(summon_i18n::format!("r0c65b9c578"));
        }
        if self.inbox_recovery_pending {
            blockers.push(summon_i18n::format!("r4097539271"));
        }
        if self.pending_turn_file {
            blockers.push(summon_i18n::format!("r023fded6f2"));
        }
        if self.inbox_pending {
            blockers.push(summon_i18n::format!("rfef864ab19"));
        }
        if self.local_outbox_pending {
            blockers.push(summon_i18n::format!("rdc62b2e9f9"));
        }
        blockers
    }
}

/// 占用拒绝回执:一句结论 + 逐条命中的证据与解法。
fn occupied_reply(headline: &str, blockers: &[String]) -> String {
    let mut reply = format!("{headline}\n");
    for blocker in blockers {
        reply.push_str("\n- ");
        reply.push_str(blocker);
    }
    reply
}

fn presence_rank(value: &crate::session::resume_store::ResumePresence) -> u8 {
    use crate::session::resume_store::ResumePresence;
    match value {
        ResumePresence::Missing => 0,
        ResumePresence::Valid(_) => 1,
        ResumePresence::Corrupt => 2,
        ResumePresence::Unreadable => 3,
    }
}

fn strongest_presence(
    current: crate::session::resume_store::ResumePresence,
    incoming: crate::session::resume_store::ResumePresence,
) -> crate::session::resume_store::ResumePresence {
    if presence_rank(&incoming) > presence_rank(&current) {
        incoming
    } else {
        current
    }
}

/// Bridge 占用判定不能复用 teardown 的 best-effort meta 扫描：任何坏/不可读历史 meta 都可能
/// 隐藏同 chat 的非 canonical session，因此必须整体 fail-closed。
fn historical_chat_sessions_strict(agent: &Agent, chat_id: &str) -> Result<Vec<String>> {
    let dir = agent.state_dir().join("runtime").join("chats");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| summon_i18n::format!("r658cab853b")),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| summon_i18n::format!("r8d91b48b7d"))?;
        let path = entry.path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("session") => {
                // `.session` 不含 chat_id，不能据此把一个无法归属的旧记录扩大成“所有群都
                // 被占用”。目标群 canonical 记录由 local_chat_evidence 精确检查；有 meta 的
                // 历史别名在下方按 chat_id 归属。其余孤儿留给 doctor/agents 显式报告和修复。
                continue;
            }
            Some("meta") => {}
            _ => continue,
        }
        let session = path
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .context(summon_i18n::text!("r2273b232c4"))?;
        let content =
            std::fs::read_to_string(&path).with_context(|| summon_i18n::format!("r9fa19c4d9a"))?;
        let meta = summon_core::chat_meta::ChatMeta::parse(&content);
        let stored_chat = meta
            .chat_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .with_context(|| summon_i18n::format!("r024d9908b7"))?;
        if stored_chat == chat_id {
            sessions.push(session.to_string());
        }
    }
    Ok(sessions)
}

#[derive(Clone, Copy)]
pub(super) struct LocalChatStores<'a> {
    inbox: &'a crate::runtime::inbox_wal::InboxWal,
    inflight: &'a crate::runtime::inflight::InFlight,
    outbox: &'a crate::runtime::outbox_store::OutboxStore,
}

impl<'a> LocalChatStores<'a> {
    pub(super) fn new(
        inbox: &'a crate::runtime::inbox_wal::InboxWal,
        inflight: &'a crate::runtime::inflight::InFlight,
        outbox: &'a crate::runtime::outbox_store::OutboxStore,
    ) -> Self {
        Self {
            inbox,
            inflight,
            outbox,
        }
    }
}

fn local_chat_evidence(
    agent: &Agent,
    storage_scope: &FeishuStorageScope,
    chat_type: &str,
    chat_id: &str,
    chats: &HashMap<String, ChatHandle>,
    stores: LocalChatStores<'_>,
) -> Result<LocalChatEvidence> {
    let remote_bound = remote_binding_live(agent, chat_id)?;
    let tag = chat_tag(chat_type, chat_id);
    let canonical = agent.tmux_name_for_chat(chat_type, chat_id, false);
    let mut sessions = vec![canonical];
    sessions.extend(historical_chat_sessions_strict(agent, chat_id)?);
    sessions.sort();
    sessions.dedup();

    let mut live_tmux = false;
    let mut resume = crate::session::resume_store::ResumePresence::Missing;
    for session in sessions {
        live_tmux |= crate::session::tmux::has_session_strict(&session)
            .with_context(|| summon_i18n::format!("r7ced136873"))?;
        resume = strongest_presence(
            resume,
            crate::session::resume_store::inspect_record(agent, &session),
        );
    }
    Ok(LocalChatEvidence {
        live_tmux,
        resume,
        legacy_resume: crate::session::resume_store::inspect_legacy(agent, &tag),
        pending_turn_file: crate::runtime::inflight::pending_exists(&agent.state_dir(), &tag)
            .with_context(|| summon_i18n::format!("r705114dd69"))?,
        inbox_pending: stores
            .inbox
            .has_pending(&agent.name, chat_id)
            .with_context(|| summon_i18n::format!("rc57cd687e3"))?,
        inbox_recovery_pending: stores
            .inbox
            .has_unresolved_recovery(&storage_scope.key, &agent.name, chat_id)
            .with_context(|| summon_i18n::format!("r45f00a89d0"))?,
        local_outbox_pending: stores
            .outbox
            .has_unresolved_local_chat(&agent.name, chat_id)
            .with_context(|| summon_i18n::format!("rb6050a9753"))?,
        inflight: stores
            .inflight
            .contains(&crate::runtime::inflight::TurnKey::new(&agent.name, &tag)),
        queued_local_work: chats
            .get(chat_id)
            .is_some_and(|handle| handle.local_claimed),
        remote_bound,
    })
}

fn local_chat_occupied(
    agent: &Agent,
    storage_scope: &FeishuStorageScope,
    chat_type: &str,
    chat_id: &str,
    chats: &HashMap<String, ChatHandle>,
    stores: LocalChatStores<'_>,
) -> Result<bool> {
    Ok(local_chat_evidence(agent, storage_scope, chat_type, chat_id, chats, stores)?.occupied())
}

fn local_chat_blockers(
    agent: &Agent,
    storage_scope: &FeishuStorageScope,
    chat_type: &str,
    chat_id: &str,
    chats: &HashMap<String, ChatHandle>,
    stores: LocalChatStores<'_>,
    place: ReplyPlace,
) -> Result<Vec<String>> {
    Ok(
        local_chat_evidence(agent, storage_scope, chat_type, chat_id, chats, stores)?
            .blockers(place),
    )
}

fn card_for_ticket(state: &BridgeState, ticket: &BridgeTicket, agent: &Agent) -> Result<String> {
    let mut candidates = Vec::new();
    for choice in &ticket.choices {
        let record = state
            .session(&choice.session)
            .context(summon_i18n::text!("r08a5747df2"))?;
        anyhow::ensure!(
            record.locator.revision == choice.registry_revision,
            summon_i18n::format!("r717622e5c9")
        );
        let mut candidate = record.safe_candidate(choice.id.clone());
        if !crate::bridge::handoff::HandoffTargetProfile::agent_is_eligible(agent)
            || crate::bridge::handoff::HandoffTargetProfile::provider_mode(agent)
                != ticket.target_provider
        {
            candidate.handoff = summon_core::bridge::HandoffEligibility::MirrorOnly(
                summon_core::bridge::HandoffBlock::TargetIncompatible,
            );
        }
        candidates.push(candidate);
    }
    Ok(summon_core::feishu::card_bridge_ticket(
        &ticket.id,
        &candidates,
        ticket.target_provider,
        super::teardown::now_unix_ms(),
    ))
}

fn redeem_ticket(
    store: &crate::bridge::store::BridgeStore,
    agent: &Agent,
    code: &str,
    scope: &str,
    requester_open_id: &str,
    now_ms: u64,
) -> Result<(BridgeTicket, String)> {
    let state = store
        .load_state()?
        .context(summon_i18n::text!("rc60a1f525b"))?;
    let ticket = state
        .redeem_short_code(code, scope, requester_open_id, now_ms)
        .map_err(anyhow::Error::from)?;
    let card = card_for_ticket(&state, &ticket, agent)?;
    Ok((ticket, card))
}

fn attach_sent_card(
    store: &crate::bridge::store::BridgeStore,
    ticket_id: &str,
    requester_open_id: &str,
    private_chat_id: Option<&str>,
    message_id: &str,
) -> Result<()> {
    store.with_state(|state| {
        state
            .attach_control_card(ticket_id, requester_open_id, private_chat_id, message_id)
            .map_err(anyhow::Error::from)
    })
}

#[derive(Debug, Clone)]
struct RecoverableControl {
    ticket_id: String,
    choice_id: String,
    binding_id: String,
    target: ScopedChatKey,
    requester_open_id: String,
    private_chat_id: Option<String>,
    control_card_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ControlRecoveryOutcome {
    Empty,
    PageOutOfRange {
        total_pages: usize,
    },
    Reissued {
        count: usize,
        total: usize,
        page: usize,
        total_pages: usize,
    },
}

fn recovery_page_window(total: usize, page: usize) -> Option<(usize, usize, usize)> {
    let total_pages = total.div_ceil(CONTROL_RECOVERY_PAGE_SIZE);
    let start = page
        .checked_sub(1)?
        .checked_mul(CONTROL_RECOVERY_PAGE_SIZE)?;
    (start < total).then(|| {
        (
            start,
            start.saturating_add(CONTROL_RECOVERY_PAGE_SIZE).min(total),
            total_pages,
        )
    })
}

fn recoverable_controls(
    state: &BridgeState,
    scope: &str,
    agent_name: &str,
) -> Vec<RecoverableControl> {
    let mut controls = state
        .tickets
        .values()
        .filter_map(|ticket| {
            let (binding_id, choice_id) = match &ticket.status {
                TicketStatus::Committed {
                    binding_id,
                    choice_id,
                    mode: BridgeMode::Mirror,
                } => (binding_id, choice_id),
                _ => return None,
            };
            if ticket.target.scope != scope || ticket.target.agent != agent_name {
                return None;
            }
            match state.slot(&ticket.target) {
                Some(ChatSlot::MirrorBound(binding)) if binding.id == *binding_id => {
                    Some(RecoverableControl {
                        ticket_id: ticket.id.clone(),
                        choice_id: choice_id.clone(),
                        binding_id: binding_id.clone(),
                        target: ticket.target.clone(),
                        requester_open_id: ticket.requester_open_id.clone(),
                        private_chat_id: ticket.private_chat_id.clone(),
                        control_card_message_id: ticket.control_card_message_id.clone(),
                    })
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    controls.sort_by_key(|control| control.target.storage_key());
    controls
}

fn reissue_bound_controls(
    store: &crate::bridge::store::BridgeStore,
    agent: &Agent,
    fc: &FeishuClient,
    storage_scope: &FeishuStorageScope,
    msg: &Inbound,
    operator_union_id: &str,
    page: usize,
) -> Result<ControlRecoveryOutcome> {
    let state = store
        .load_state()?
        .context(summon_i18n::text!("rc60a1f525b"))?;
    let controls = recoverable_controls(&state, &storage_scope.key, &agent.name);
    let total = controls.len();
    if total == 0 {
        return Ok(ControlRecoveryOutcome::Empty);
    }
    let total_pages = total.div_ceil(CONTROL_RECOVERY_PAGE_SIZE);
    let Some((start, end, _)) = recovery_page_window(total, page) else {
        return Ok(ControlRecoveryOutcome::PageOutOfRange { total_pages });
    };
    let mut reissued = 0usize;
    for control in controls.into_iter().skip(start).take(end - start) {
        let card = summon_core::feishu::card_bridge_mirror_control(
            &control.ticket_id,
            &control.choice_id,
            &summon_core::feishu::bridge_target_label(
                &control.target.agent,
                &control.target.chat_id,
            ),
        );
        let uuid = summon_core::outbox::bridge_delivery_id(
            summon_core::outbox::BridgeDeliveryKind::ActivationAudit,
            &format!("recover\0{}\0{}", msg.message_id, control.ticket_id),
        );
        let message_id = send_card_idempotent_archived_id(
            fc,
            &agent.state_dir(),
            "p2p",
            &msg.chat_id,
            "bridge_control_recovery",
            &card,
            &uuid,
        )?;
        let transferred = crate::bridge::runtime::with_chat_delivery_gate(&control.target, || {
            store.with_state(|state| {
                state
                    .transfer_bound_control_card(
                        &control.ticket_id,
                        &control.binding_id,
                        &msg.sender,
                        operator_union_id,
                        &msg.chat_id,
                        &message_id,
                    )
                    .map_err(anyhow::Error::new)
            })
        });
        match transferred {
            Ok(()) => {
                reissued = reissued.saturating_add(1);
                log(&summon_i18n::format!(
                    "r0e61ff2fa6",
                    agent.name,
                    control.target.storage_key(),
                    control.binding_id
                ));
            }
            Err(error) => log(&summon_i18n::format!("r614612816a", agent.name)),
        }
    }
    Ok(ControlRecoveryOutcome::Reissued {
        count: reissued,
        total,
        page,
        total_pages,
    })
}

/// 群里重复 `/bridge` 时优先恢复当前 target，而不是把“候选已被自己 lease”误报成无候选。
/// 只有原操作者且已知其私聊 chat_id 时才主动重发；其它授权操作者走私聊 `/bridge` 的显式
/// transfer，避免把控制卡发进前一位操作者的私聊。
enum ControlRecoveryReply {
    Notice(String),
    PrivateCardSent,
}

fn recover_existing_target_control(
    store: &crate::bridge::store::BridgeStore,
    agent: &Agent,
    fc: &FeishuClient,
    storage_scope: &FeishuStorageScope,
    msg: &Inbound,
    target: &ScopedChatKey,
) -> Result<Option<ControlRecoveryReply>> {
    let Some(state) = store.load_state()? else {
        return Ok(None);
    };
    let Some(slot) = state.slot(target) else {
        return Ok(None);
    };
    let ChatSlot::MirrorBound(_) = slot else {
        return Ok(
            bridge_slot_text(Some(slot)).map(|text| ControlRecoveryReply::Notice(text.to_string()))
        );
    };
    let control = recoverable_controls(&state, &storage_scope.key, &agent.name)
        .into_iter()
        .find(|control| control.target == *target)
        .context(summon_i18n::text!("rf4431a919d"))?;
    if control.requester_open_id != msg.sender {
        return Ok(Some(ControlRecoveryReply::Notice(
            summon_i18n::text!("rfa90bec1c1").to_string(),
        )));
    }
    let (Some(private_chat_id), Some(old_message_id)) = (
        control.private_chat_id.as_deref(),
        control.control_card_message_id.as_deref(),
    ) else {
        return Ok(Some(ControlRecoveryReply::Notice(
            summon_i18n::text!("r6c35041dcf").to_string(),
        )));
    };
    let card = summon_core::feishu::card_bridge_mirror_control(
        &control.ticket_id,
        &control.choice_id,
        &summon_core::feishu::bridge_target_label(&control.target.agent, &control.target.chat_id),
    );
    let uuid = summon_core::outbox::bridge_delivery_id(
        summon_core::outbox::BridgeDeliveryKind::ActivationAudit,
        &format!("group-recover\0{}\0{}", msg.message_id, control.ticket_id),
    );
    let message_id = send_card_idempotent_archived_id(
        fc,
        &agent.state_dir(),
        "p2p",
        private_chat_id,
        "bridge_control_recovery",
        &card,
        &uuid,
    )?;
    crate::bridge::runtime::with_chat_delivery_gate(target, || {
        store.with_state(|state| {
            state
                .replace_bound_control_card(
                    &control.ticket_id,
                    &control.binding_id,
                    &msg.sender,
                    private_chat_id,
                    old_message_id,
                    &message_id,
                )
                .map_err(anyhow::Error::new)
        })
    })?;
    Ok(Some(ControlRecoveryReply::PrivateCardSent))
}

fn handle_control_recovery(
    store: &crate::bridge::store::BridgeStore,
    agent: &Agent,
    fc: &FeishuClient,
    storage_scope: &FeishuStorageScope,
    msg: &Inbound,
    operator_union_id: &str,
    page: usize,
) {
    if msg.chat_type.as_deref() != Some("p2p") || !msg.mentions.is_empty() {
        return;
    }
    if !bridge_operator_allowed(agent, &msg.sender, operator_union_id) {
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &msg.chat_id,
            summon_i18n::text!("r23acbb71fa"),
        );
        return;
    }
    match reissue_bound_controls(
        store,
        agent,
        fc,
        storage_scope,
        msg,
        operator_union_id,
        page,
    ) {
        Ok(ControlRecoveryOutcome::Empty) => send_bridge_reply(
            fc,
            agent,
            "p2p",
            &msg.chat_id,
            summon_i18n::text!("rf66558e1a0"),
        ),
        Ok(ControlRecoveryOutcome::PageOutOfRange { total_pages }) => send_bridge_reply(
            fc,
            agent,
            "p2p",
            &msg.chat_id,
            &summon_i18n::format!("rbe5aea65df"),
        ),
        Ok(ControlRecoveryOutcome::Reissued {
            count,
            total,
            page,
            total_pages,
        }) => {
            let next = if page < total_pages {
                summon_i18n::format!("r074877e99a", page + 1)
            } else {
                String::new()
            };
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &msg.chat_id,
                &summon_i18n::format!("r53818c99fa"),
            );
        }
        Err(error) => {
            log(&summon_i18n::format!("r06b102350a", agent.name));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &msg.chat_id,
                summon_i18n::text!("r894ebc6861"),
            );
        }
    }
}

fn send_bridge_reply(fc: &FeishuClient, agent: &Agent, chat_type: &str, chat_id: &str, text: &str) {
    if let Err(error) = send_markdown_archived(fc, &agent.state_dir(), chat_type, chat_id, text) {
        log(&summon_i18n::format!("r7cbcffa04f", agent.name));
    }
}

/// `/bridge` 文本指令的回执:群里挂进那条指令消息的 thread(判据同 dispatcher 的
/// `command_anchor`),私聊照常发。卡片回调的回执仍走 [`send_bridge_reply`]。
fn send_bridge_command_reply(fc: &FeishuClient, agent: &Agent, msg: &Inbound, text: &str) {
    if let Err(error) = crate::frontend::outbound::send_markdown_archived_anchored(
        fc,
        &agent.state_dir(),
        msg.chat_type.as_deref().unwrap_or("p2p"),
        &msg.chat_id,
        super::dispatcher::command_anchor(msg),
        text,
    ) {
        log(&summon_i18n::format!("r7cbcffa04f", agent.name));
    }
}

/// 多适配器参数只在本 IO 编排边界聚合；测试注入独立 store，避免修改真实 `~/.summon`。
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_bridge_command_with_store(
    command: BridgeCommand,
    agent: &Agent,
    fc: &FeishuClient,
    storage_scope: &FeishuStorageScope,
    msg: &Inbound,
    chats: &HashMap<String, ChatHandle>,
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
    inflight: &crate::runtime::inflight::InFlight,
    store: &crate::bridge::store::BridgeStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
) {
    let chat_type = msg.chat_type.as_deref().unwrap_or("p2p");
    let is_group = matches!(chat_type, "group" | "topic_group");
    let op_union = if msg.sender_union_id.is_empty() {
        fc.resolve_union_id(&msg.sender)
    } else {
        msg.sender_union_id.clone()
    };
    match command {
        BridgeCommand::Start => {
            if !is_group || !bridge_operator_allowed(agent, &msg.sender, &op_union) {
                send_bridge_command_reply(fc, agent, msg, summon_i18n::text!("r8b2b230042"));
                return;
            }
            match local_chat_blockers(
                agent,
                storage_scope,
                chat_type,
                &msg.chat_id,
                chats,
                LocalChatStores::new(inbox_wal, inflight, outbox),
                ReplyPlace::TargetGroup,
            ) {
                Ok(blockers) if !blockers.is_empty() => {
                    // 不同证据清法不同,逐条点名。
                    send_bridge_command_reply(
                        fc,
                        agent,
                        msg,
                        &occupied_reply(summon_i18n::text!("r76a1b03657"), &blockers),
                    );
                    return;
                }
                Err(error) => {
                    log(&summon_i18n::format!("r493e06efcb", agent.name));
                    send_bridge_command_reply(fc, agent, msg, summon_i18n::text!("r4adf4e921b"));
                    return;
                }
                Ok(_) => {}
            }
            let target = ScopedChatKey::new(&storage_scope.key, &agent.name, &msg.chat_id);
            match recover_existing_target_control(store, agent, fc, storage_scope, msg, &target) {
                Ok(Some(ControlRecoveryReply::Notice(reply))) => {
                    send_bridge_command_reply(fc, agent, msg, &reply);
                    return;
                }
                Ok(Some(ControlRecoveryReply::PrivateCardSent)) => return,
                Err(error) => {
                    log(&summon_i18n::format!("rffb065d304", agent.name));
                    send_bridge_command_reply(fc, agent, msg, summon_i18n::text!("r0efab59f85"));
                    return;
                }
                Ok(None) => {}
            }
            // 这一条真实群入站证明 target 当前可达；每次 `/bridge` 都旋转 revision，使群解散、
            // bot 移除或另一张更新的召回卡之后，旧私聊卡不能再把唯一 owner 搬回过期生命周期。
            if !RECALL_TARGET_LIFECYCLE_HEALTHY.load(std::sync::atomic::Ordering::Acquire) {
                send_bridge_command_reply(fc, agent, msg, summon_i18n::text!("r07e349f790"));
                return;
            }
            let target_lifecycle_token = match rotate_recall_target_lifecycle(agent, &msg.chat_id) {
                Ok(token) => token,
                Err(error) => {
                    log(&summon_i18n::format!("rb6ea736de0", agent.name));
                    send_bridge_command_reply(fc, agent, msg, summon_i18n::text!("rbf839e9fe3"));
                    return;
                }
            };
            // 先成功换代，再让真实群入站开启新生命周期。若 token 落盘失败，旧 exit mark 继续
            // 作为第二道保守闸，绝不先清掉后让上一代卡重新可用。
            crate::session::exit_mark::clear(agent, chat_type, &msg.chat_id);
            // 召回替代旧 hook 发现：recall_card 合并活跃 + 历史全集，活跃项额外落本机 exact
            // observation，再私聊发 card_recall。**不预留旧 Bridge group slot**——召回有自己的
            // durable attempt/staging/ACK/群绑定协议；若复用 Pending ticket，反而会让 dispatcher
            // 在召回完成前硬冻结本群。callback 只带 sid/opaque observation，鉴权在回调内
            // operate-gate，不依赖旧 ticket/短码机制。
            // 原群 G 的 (chat_id, chat_type) 穿透进按钮 value:私聊卡的回调据此把历史会话恢复进
            // 群 G 的 canonical 并绑群(群消息经 per-chat dispatcher 往返)。鉴权在回调内对群 G
            // operate-gate(is_group=true),不信 value。
            // 主动私聊召回卡标注来源群(/bridge 是在哪个群发的),owner 并发收多群召回卡时不混。
            // 标注随卡一起渲染,召回卡按真正发出去的大小控制预算。
            let note = summon_core::feishu::source_group_note(
                fc.resolve_source_group(&msg.chat_id).as_deref(),
                &msg.chat_id,
            );
            let card = crate::bridge::recall::recall_card_for_agent(
                agent,
                inflight,
                store,
                crate::bridge::recall::RecallCardRequest {
                    page: 1,
                    page_size: RECALL_PAGE_SIZE,
                    origin: Some((&msg.chat_id, chat_type)),
                    binding_scope: &storage_scope.key,
                    requester_open_id: &msg.sender,
                    target_lifecycle_token: &target_lifecycle_token,
                    card_issued_at_ms: now_unix_ms(),
                    source_note: Some(&note),
                },
            );
            match send_card_to_open_id_archived_id(
                fc,
                &agent.state_dir(),
                &msg.sender,
                "bridge_recall",
                &card,
            ) {
                Ok(_message_id) => log(&summon_i18n::format!(
                    "r7a6050bd4e",
                    agent.name,
                    msg.sender,
                    msg.chat_id,
                )),
                Err(error) => {
                    log(&summon_i18n::format!("rc88ddc6c87", agent.name));
                    send_bridge_command_reply(fc, agent, msg, summon_i18n::text!("r25193ad366"));
                }
            }
        }
        BridgeCommand::Redeem { code } => {
            if msg.chat_type.as_deref() != Some("p2p") || !msg.mentions.is_empty() {
                log(&summon_i18n::format!(
                    "r88631b0fb1",
                    agent.name,
                    msg.chat_id
                ));
                return;
            }
            let (ticket, card) = match redeem_ticket(
                store,
                agent,
                &code,
                &storage_scope.key,
                &msg.sender,
                now_unix_ms(),
            ) {
                Ok(value) => value,
                Err(_) => {
                    send_bridge_command_reply(fc, agent, msg, summon_i18n::text!("r1b616210b6"));
                    return;
                }
            };
            if ticket.target.agent != agent.name
                || !matches!(ticket.target_chat_type.as_str(), "group" | "topic_group")
                || !bridge_operator_allowed(agent, &msg.sender, &op_union)
            {
                send_bridge_command_reply(fc, agent, msg, summon_i18n::text!("r459c93e720"));
                return;
            }
            let message_id = match send_card_archived_id(
                fc,
                &agent.state_dir(),
                "p2p",
                &msg.chat_id,
                None,
                "bridge_ticket",
                &card,
            ) {
                Ok(message_id) => message_id,
                Err(error) => {
                    log(&summon_i18n::format!("r19863097a2", agent.name));
                    return;
                }
            };
            if let Err(error) = attach_sent_card(
                store,
                &ticket.id,
                &msg.sender,
                Some(&msg.chat_id),
                &message_id,
            ) {
                log(&summon_i18n::format!("r8848134540", agent.name));
            }
        }
        BridgeCommand::Help => {
            handle_control_recovery(store, agent, fc, storage_scope, msg, &op_union, 1)
        }
        BridgeCommand::RecoveryPage { page } => {
            handle_control_recovery(store, agent, fc, storage_scope, msg, &op_union, page)
        }
        BridgeCommand::Invalid => send_bridge_command_reply(
            fc,
            agent,
            msg,
            if is_group {
                summon_i18n::text!("r4b39bd31e0")
            } else {
                summon_i18n::text!("r1f1e1abf54")
            },
        ),
    }
}

/// 活跃会话(有前台源)的止损引导文案。Stage 2 的「安全退出源 + 迁移」落地前,一律先引导用户
/// 手动退出源、转历史再恢复——绝不在 Stage 1 里杀源。
fn active_takeover_stopgap_text(
    candidate: &summon_core::recall::SessionCandidate,
    sid: &str,
) -> String {
    summon_i18n::format!(
        "rff4b89cc64",
        if candidate.name.is_empty() {
            sid
        } else {
            &candidate.name
        }
    )
}

/// 无原群(旧卡 / DM)时的本地兜底:把历史会话恢复为独立托管 tmux(`soul-<alias>-recall-<sid8>`),
/// 不绑群。**调用方已过 operate-gate**。活跃会话仍走止损引导。
fn recall_resume_reply(agent: &Agent, sid: &str) -> String {
    use summon_core::recall::{resume_mode, RecallResumeMode};
    // `claude agents` 瞬时失败时活跃会话会显示成历史,恢复就成了第二个 writer。这条路不发信号,
    // 也没有可比的活着的 pid,只按 sid 查。
    if let Some(refusal) = session_remote_refusal(agent, sid, None) {
        return refusal.to_string();
    }
    let Some(home) = dirs::home_dir() else {
        return summon_i18n::text!("rde14ef2c2a").to_string();
    };
    let projects = home.join(".claude/projects");
    let Some(candidate) = crate::bridge::recall::find_candidate(&projects, sid) else {
        return summon_i18n::format!("rba893cccca");
    };
    if let RecallResumeMode::ActiveKillSource { .. } = resume_mode(&candidate) {
        return active_takeover_stopgap_text(&candidate, sid);
    }
    let cfg = summon_core::config::Config::load();
    match crate::bridge::recall::resume_historical(
        agent,
        sid,
        &candidate.cwd,
        cfg.poll_ms,
        cfg.debounce_ms,
    ) {
        Ok(tmux) => summon_i18n::format!("raff7607be6"),
        Err(e) => summon_i18n::format!("r5af34233e8"),
    }
}

/// 会话「完整托管进原群 G」的执行体。**调用方已过 operate-gate(针对群 G,is_group=true)**。
/// 历史会话直接造无源 plan；活跃会话先完成 Stage 2 杀源前置步(Gone + transcript 稳定)，
/// 两者随后共用 Stage 1 的 durable attempt → staging → ACK → rename → 绑群。
fn recall_requires_source_exit(
    candidate: &summon_core::recall::SessionCandidate,
    source_observation_id: Option<&str>,
) -> bool {
    candidate.is_active() || source_observation_id.is_some()
}

struct RecallGroupRequest<'a> {
    storage_scope: &'a FeishuStorageScope,
    sid: &'a str,
    chat_id: &'a str,
    chat_type: &'a str,
    actor_open_id: &'a str,
    source_observation_id: Option<&'a str>,
}

struct HeadlessRecallGroupRequest<'a> {
    provider: summon_core::recall::RecallProvider,
    storage_scope: &'a FeishuStorageScope,
    sid: &'a str,
    chat_id: &'a str,
    chat_type: &'a str,
    actor_open_id: &'a str,
}

struct RpcHandoffGroupRequest<'a> {
    provider: summon_core::recall::RecallProvider,
    storage_scope: &'a FeishuStorageScope,
    sid: &'a str,
    chat_id: &'a str,
    chat_type: &'a str,
    actor_open_id: &'a str,
    source_ref: &'a str,
    observed_at_ms: u64,
    replayed_from_wal: bool,
}

struct RpcHandoffHooks<'a> {
    prove_target_reachable: &'a dyn Fn(&str) -> Result<()>,
    record_gone_owner: &'a dyn Fn(&str, &str, &str, &str) -> Result<()>,
}

fn source_rpc_handoff_quiescent(
    agent: &Agent,
    storage_scope: &FeishuStorageScope,
    source_chat_id: &str,
    source_tag: &str,
    stores: LocalChatStores<'_>,
) -> Result<()> {
    anyhow::ensure!(
        matches!(
            crate::session::resume_store::inspect_legacy(agent, source_tag),
            crate::session::resume_store::ResumePresence::Missing
        ),
        summon_i18n::format!("rf604fadf3b")
    );
    anyhow::ensure!(
        !crate::runtime::inflight::pending_exists(&agent.state_dir(), source_tag)
            .with_context(|| summon_i18n::format!("red62d10fdd"))?,
        summon_i18n::format!("r3467631c27")
    );
    let pending = stores.inbox.scan_pending_strict()?;
    anyhow::ensure!(
        !pending.iter().any(|record| {
            record.agent == agent.name && record.inbound.chat_id == source_chat_id
        }),
        summon_i18n::format!("r010229a364")
    );
    anyhow::ensure!(
        !stores
            .inbox
            .has_unresolved_recovery(&storage_scope.key, &agent.name, source_chat_id)?,
        summon_i18n::format!("r1da43aa013")
    );
    anyhow::ensure!(
        !stores
            .outbox
            .has_unresolved_local_chat(&agent.name, source_chat_id)?,
        summon_i18n::format!("r16fd9f85c2")
    );
    anyhow::ensure!(
        !stores
            .inflight
            .contains(&crate::runtime::inflight::TurnKey::new(
                &agent.name,
                source_tag
            )),
        summon_i18n::format!("r07938b6d66")
    );
    Ok(())
}

fn detach_rpc_chat_worker(
    chats: &mut HashMap<String, ChatHandle>,
    chat_id: &str,
    expected_session: &str,
    require_claimed: bool,
) -> Result<()> {
    let Some(handle) = chats.get(chat_id) else {
        anyhow::ensure!(!require_claimed, summon_i18n::format!("r68da17ea15"));
        return Ok(());
    };
    anyhow::ensure!(
        handle.tmux_name == expected_session,
        summon_i18n::format!("r8db79fe631")
    );
    if require_claimed {
        anyhow::ensure!(handle.local_claimed, summon_i18n::format!("r8c9345dd0f"));
    } else {
        anyhow::ensure!(!handle.local_claimed, summon_i18n::format!("r2038f9e2be"));
    }
    let tx = handle.tx.clone();
    let lease = RpcHandoffDetachLease::new();
    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
    tx.send(ThreadMsg::RpcHandoffDetach {
        lease: Arc::clone(&lease),
        ack: ack_tx,
    })
    .context(summon_i18n::text!("rcc59f635e4"))?;
    let detached = match ack_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(detached) => detached,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) if lease.cancel() => {
            anyhow::bail!(summon_i18n::format!("r72ea44453c"))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            ack_rx.recv().context(summon_i18n::text!("r4502f63392"))?
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!(summon_i18n::format!("r0fa75373a3"))
        }
    };
    anyhow::ensure!(detached, summon_i18n::format!("r4894d07702"));
    chats.remove(chat_id);
    Ok(())
}

fn ensure_handoff_target_files_missing(agent: &Agent, session: &str) -> Result<()> {
    for path in [
        chat_meta_path(agent, session),
        chat_cwd_path(agent, session),
        crate::session::resume_store::record_path(agent, session),
        crate::session::resume_store::takeover_marker_path(agent, session),
    ] {
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => anyhow::bail!(summon_i18n::format!("r7e6a9041ed", path.display())),
            Err(error) => return Err(error).with_context(|| summon_i18n::format!("r3311eaa8f8")),
        }
    }
    Ok(())
}

fn cleanup_handoff_source_metadata(agent: &Agent, session: &str) {
    for path in [
        chat_meta_path(agent, session),
        chat_cwd_path(agent, session),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log(&summon_i18n::format!(
                "rf2634614bc",
                agent.name,
                path.display()
            )),
        }
    }
}

fn wait_rpc_group_gone(pgid: i32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while crate::runtime::inflight::group_alive(pgid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !crate::runtime::inflight::group_alive(pgid)
}

fn recall_rpc_handoff_into_group(
    agent: &Agent,
    fc: &FeishuClient,
    request: RpcHandoffGroupRequest<'_>,
    chats: &mut HashMap<String, ChatHandle>,
    stores: LocalChatStores<'_>,
    bridge_store: &crate::bridge::store::BridgeStore,
) -> String {
    match perform_rpc_handoff(agent, fc, request, chats, stores, bridge_store) {
        Ok(reply) => reply,
        Err(error) => summon_i18n::format!("rff5d613051"),
    }
}

fn perform_rpc_handoff(
    agent: &Agent,
    fc: &FeishuClient,
    request: RpcHandoffGroupRequest<'_>,
    chats: &mut HashMap<String, ChatHandle>,
    stores: LocalChatStores<'_>,
    bridge_store: &crate::bridge::store::BridgeStore,
) -> Result<String> {
    let pool = crate::capture::rpc_pool::shared(stores.inflight);
    perform_rpc_handoff_with_pool(
        agent,
        request,
        chats,
        stores,
        bridge_store,
        &pool,
        RpcHandoffHooks {
            prove_target_reachable: &|chat_id| fc.prove_bot_in_chat(chat_id),
            record_gone_owner: &crate::capture::rpc_procs::record_gone_owner,
        },
    )
}

fn perform_rpc_handoff_with_pool(
    agent: &Agent,
    request: RpcHandoffGroupRequest<'_>,
    chats: &mut HashMap<String, ChatHandle>,
    stores: LocalChatStores<'_>,
    bridge_store: &crate::bridge::store::BridgeStore,
    pool: &crate::capture::rpc_pool::RpcPool,
    hooks: RpcHandoffHooks<'_>,
) -> Result<String> {
    let RpcHandoffGroupRequest {
        provider,
        storage_scope,
        sid,
        chat_id,
        chat_type,
        actor_open_id,
        source_ref,
        observed_at_ms,
        replayed_from_wal,
    } = request;
    let now_ms = now_unix_ms();
    anyhow::ensure!(
        observed_at_ms <= now_ms.saturating_add(RPC_HANDOFF_CARD_CLOCK_SKEW_MS)
            && now_ms.saturating_sub(observed_at_ms) <= RPC_HANDOFF_CARD_TTL_MS,
        summon_i18n::format!("rcb3c1db2f6")
    );
    anyhow::ensure!(
        matches!(chat_type, "group" | "topic_group"),
        summon_i18n::format!("rbbf198b948")
    );
    anyhow::ensure!(
        summon_core::recall::RecallProvider::from_cli_kind(agent.cli_kind()) == Some(provider),
        summon_i18n::format!("rcfe7cf007c")
    );
    anyhow::ensure!(
        agent.capture_mode() == CaptureMode::Rpc,
        summon_i18n::format!("r7a112c7dca")
    );
    anyhow::ensure!(
        !crate::session::exit_mark::exists(agent, chat_type, chat_id),
        summon_i18n::format!("r2e1230ec86")
    );
    let target_key = ScopedChatKey::new(&storage_scope.key, &agent.name, chat_id);
    anyhow::ensure!(
        bridge_slot_message(bridge_store, &target_key)?.is_none(),
        summon_i18n::format!("r79930b8853")
    );
    let target_session = agent.tmux_name_for_chat(chat_type, chat_id, false);
    let target_tag = chat_tag(chat_type, chat_id);
    if replayed_from_wal {
        if let Some(target_record) =
            crate::session::resume_store::read_record_checked(agent, &target_session)?
        {
            if target_record.cli == agent.resume_cli_kind() && target_record.id == sid {
                anyhow::ensure!(
                    crate::bridge::recall_rpc::durable_source_count_by_sid(agent, chat_id, sid)?
                        == 0,
                    summon_i18n::format!("r7f84157bbc")
                );
                anyhow::ensure!(
                    matches!(
                        crate::session::resume_store::inspect_legacy(agent, &target_tag),
                        crate::session::resume_store::ResumePresence::Missing
                    ),
                    summon_i18n::format!("r783cb027c9")
                );
                let cwd = target_record
                    .cwd
                    .as_deref()
                    .filter(|cwd| {
                        std::path::Path::new(cwd).is_absolute()
                            && std::path::Path::new(cwd).is_dir()
                    })
                    .context(summon_i18n::text!("r6b9a59a618"))?;
                if let Some(snapshot) = pool.handoff_snapshot(&agent.name, &target_tag)? {
                    anyhow::ensure!(
                        snapshot.session_id == sid && snapshot.spawn_cwd.as_deref() == Some(cwd),
                        summon_i18n::format!("r5d7c5e8dcb")
                    );
                }
                if let Some(existing) = read_chat_cwd(agent, &target_session) {
                    anyhow::ensure!(existing == cwd, summon_i18n::format!("r7c6b622236"));
                }
                if let Some(meta) = read_chat_meta(agent, &target_session) {
                    anyhow::ensure!(
                        meta.chat_id.as_deref() == Some(chat_id)
                            && meta.chat_type.as_deref() == Some(chat_type),
                        summon_i18n::format!("re5ff8f1799")
                    );
                }
                (hooks.prove_target_reachable)(chat_id)
                    .context(summon_i18n::text!("ra616dc7442"))?;
                anyhow::ensure!(
                    !crate::session::exit_mark::exists(agent, chat_type, chat_id),
                    summon_i18n::format!("r91bd617860")
                );
                anyhow::ensure!(
                    bridge_slot_message(bridge_store, &target_key)?.is_none(),
                    summon_i18n::format!("r7887d8360b")
                );
                write_chat_cwd_checked(agent, &target_session, cwd)
                    .context(summon_i18n::text!("r3161118011"))?;
                write_chat_meta_checked(
                    agent,
                    &target_session,
                    chat_id,
                    chat_type,
                    None,
                    actor_open_id,
                    Some(actor_open_id),
                )
                .context(summon_i18n::text!("rcb44f09213"))?;
                return Ok(summon_i18n::format!("r0b2acad6c6", provider.label()));
            }
        }
    }
    anyhow::ensure!(
        pool.handoff_target_free(&agent.name, &target_tag)?,
        summon_i18n::format!("r64f5fc13cd")
    );
    let matches = crate::bridge::recall_rpc::discover(agent, chat_id, pool)?
        .into_iter()
        .filter(|entry| {
            entry.candidate.sid == sid
                && entry.candidate.source_observation_id.as_deref() == Some(source_ref)
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(matches.len() <= 1, summon_i18n::format!("r95e925072d"));
    let (
        source_chat_id,
        source_chat_type,
        source_session,
        source_tag,
        source_record,
        source_name,
        expected_live_pool,
    ) = if let Some(source) = matches.into_iter().next() {
        (
            source.source_chat_id,
            source.source_chat_type,
            source.source_session,
            source.source_tag,
            source.record,
            source.candidate.name,
            Some(source.pool),
        )
    } else {
        anyhow::ensure!(replayed_from_wal, summon_i18n::format!("rfa7134567d"));
        let source =
            crate::bridge::recall_rpc::find_durable_source(agent, chat_id, sid, source_ref)?
                .context(summon_i18n::text!("r5d7f6d6b9a"))?;
        anyhow::ensure!(
            pool.handoff_snapshot(&agent.name, &source.source_tag)?
                .is_none(),
            summon_i18n::format!("rd0bfe268ac")
        );
        (
            source.source_chat_id,
            source.source_chat_type,
            source.source_session,
            source.source_tag,
            source.record,
            source.name,
            None,
        )
    };
    anyhow::ensure!(
        source_chat_id != chat_id,
        summon_i18n::format!("r506b2e70f9")
    );
    anyhow::ensure!(
        source_record.cli == agent.resume_cli_kind(),
        summon_i18n::format!("r9aa9de326c")
    );

    anyhow::ensure!(
        !local_chat_occupied(agent, storage_scope, chat_type, chat_id, chats, stores)?,
        summon_i18n::format!("r3c3d65d432")
    );
    ensure_handoff_target_files_missing(agent, &target_session)?;
    match (chats.get(&source_chat_id), expected_live_pool.is_some()) {
        (Some(source_handle), true) => anyhow::ensure!(
            source_handle.chat_type == source_chat_type,
            summon_i18n::format!("rb00e7a85b6")
        ),
        (None, true) => anyhow::bail!(summon_i18n::format!("r68da17ea15")),
        (None, false) => {}
        (Some(_), false) => {
            anyhow::bail!(summon_i18n::format!("raf78c0dc32"))
        }
    }
    source_rpc_handoff_quiescent(agent, storage_scope, &source_chat_id, &source_tag, stores)?;

    // target 可能只有一条从未启动 CLI 的 `/session` 空 handle；先静默退出，避免 commit 后它仍
    // 持旧 Session 快照。source 必须在两轮边界 ACK 后退出，冻结旧路由。
    detach_rpc_chat_worker(chats, chat_id, &target_session, false)?;
    if expected_live_pool.is_some() {
        detach_rpc_chat_worker(chats, &source_chat_id, &source_session, true)?;
    }
    source_rpc_handoff_quiescent(agent, storage_scope, &source_chat_id, &source_tag, stores)?;
    // target worker 的 detach 控制消息排在它原有队列末尾；等待 ACK 期间它可能刚处理完一条更早
    // 的本地任务。提交前必须重新证明盘、WAL 与 External Bridge slot 仍为空，不能沿用 detach 前
    // 的快照。
    anyhow::ensure!(
        !local_chat_occupied(agent, storage_scope, chat_type, chat_id, chats, stores)?,
        summon_i18n::format!("re20e7191b9")
    );
    ensure_handoff_target_files_missing(agent, &target_session)?;
    anyhow::ensure!(
        bridge_slot_message(bridge_store, &target_key)?.is_none(),
        summon_i18n::format!("ra67ac89b50")
    );
    anyhow::ensure!(
        pool.handoff_target_free(&agent.name, &target_tag)?,
        summon_i18n::format!("re3a2645708")
    );
    let fresh = match expected_live_pool.as_ref() {
        Some(expected) => {
            let fresh = pool
                .handoff_snapshot(&agent.name, &source_tag)?
                .context(summon_i18n::text!("r0d7fdb4ca9"))?;
            anyhow::ensure!(&fresh == expected, summon_i18n::format!("r508a17ed07"));
            Some(fresh)
        }
        None => {
            anyhow::ensure!(
                pool.handoff_snapshot(&agent.name, &source_tag)?.is_none(),
                summon_i18n::format!("r137db786b4")
            );
            None
        }
    };
    // 前面的通用 click-time 检查保护翻页/外部召回；托管迁移还要在 source 已冻结、durable
    // commit 即将发生的线性化点再查一次，避免慢发现/worker detach 把可达性证据拖陈旧。
    (hooks.prove_target_reachable)(chat_id).context(summon_i18n::text!("r57395ecfd5"))?;
    anyhow::ensure!(
        !crate::session::exit_mark::exists(agent, chat_type, chat_id),
        summon_i18n::format!("rb01ab8ca44")
    );
    anyhow::ensure!(
        bridge_slot_message(bridge_store, &target_key)?.is_none(),
        summon_i18n::format!("r94a480d939")
    );

    let cwd = source_record
        .cwd
        .clone()
        .context(summon_i18n::text!("r447e9cd7be"))?;
    if fresh.is_none() {
        // boot replay 没有 live pool 可 rekey；boot 已在任何 dispatcher ready 前证明上一代组 Gone。
        // 先落 Gone tombstone，再写 strict intent：两步之间再次崩溃时 source 尚非 strict；之后
        // 任一点崩溃则 source/target strict owner 都能按 exact SID 匹配这条 Gone 证明。
        (hooks.record_gone_owner)(&agent.name, &target_tag, &target_session, sid)?;
    }
    // strict intent 直接写进 source `.session`，随后随原子 move 穿过唯一提交点。
    let marked = crate::session::resume_store::mark_strict_checked(
        agent,
        &source_session,
        &source_record.cli,
        sid,
    )?;
    anyhow::ensure!(
        marked.cwd.as_deref() == Some(cwd.as_str()),
        summon_i18n::format!("r1a999a05e2")
    );

    let (moved, committed_pgid) = if let Some(fresh) = fresh.as_ref() {
        let receipt = pool
            .rekey_idle_exact(&agent.name, &source_tag, &target_tag, fresh)
            .context(summon_i18n::text!("r773df511fc"))?;
        let moved = match crate::session::resume_store::transfer_exact(
            agent,
            &source_session,
            &target_session,
            &source_record.cli,
            sid,
        ) {
            Ok(record) => record,
            Err(commit_error) => {
                let rollback = pool.rollback_rekey(&receipt);
                if let Err(rollback_error) = rollback {
                    let converging = pool.evict(&agent.name, &target_tag)
                        || pool.evict(&agent.name, &source_tag);
                    let gone = wait_rpc_group_gone(receipt.pgid, Duration::from_secs(5));
                    if !gone {
                        // source `.session` 仍是 strict durable owner，但旧 target-key writer 尚未 Gone。
                        stores.inflight.freeze();
                    }
                    anyhow::bail!(summon_i18n::format!(
                        "rf6766bdc45",
                        if gone {
                            ""
                        } else {
                            summon_i18n::text!("rca68266f5f")
                        }
                    ));
                }
                return Err(commit_error).context(summon_i18n::text!("r2024792d7c"));
            }
        };
        (moved, Some(receipt.pgid))
    } else {
        let moved = crate::session::resume_store::transfer_exact(
            agent,
            &source_session,
            &target_session,
            &source_record.cli,
            sid,
        )
        .context(summon_i18n::text!("r2eee0410a3"))?;
        (moved, None)
    };
    if !moved.strict_resume || moved.id != sid || moved.cli != source_record.cli {
        // transfer_exact 已在原子 rename 前按同一份 bytes 验过 identity；到这里不应可达。即便
        // 真遇到实现 bug，durable owner 已经是 target，不能再把成功提交谎报成“未迁移”。
        log(&summon_i18n::format!("r1fed39b1d6", agent.name));
    }
    // `.session` 已是唯一 durable commit。此后才写 canonical cwd/meta：dispatcher 当前被本
    // callback 串行占用，不存在并发 target 入站；若此处崩溃/失败，strict record 自带 cwd，
    // target 下一条真实入站会先恢复 `.cwd` 再允许 provider checkout，不会 cold-start。
    let cwd_ready = write_chat_cwd_checked(agent, &target_session, &cwd);
    let meta_ready = write_chat_meta_checked(
        agent,
        &target_session,
        chat_id,
        chat_type,
        None,
        actor_open_id,
        Some(actor_open_id),
    );
    if let Err(error) = &cwd_ready {
        log(&summon_i18n::format!("r6a799a8fcd", agent.name));
    }
    if let Err(error) = &meta_ready {
        log(&summon_i18n::format!("r996168d05e", agent.name));
    }
    // handoff 已提交 = target 上开了新的路由生命周期。与同段 cwd/meta 一致取 best-effort:
    // 写不成只 log,target 下一条真实入站会经 admission 再写一次 Open,不至于卡死恢复流程。
    if let Err(error) = crate::session::lifecycle::mark_open(agent, &chat_tag(chat_type, chat_id)) {
        log(&summon_i18n::format!("r9319a75358", agent.name));
    }
    cleanup_handoff_source_metadata(agent, &source_session);
    log(&summon_i18n::format!(
        "r4dd2764b07",
        agent.name,
        source_chat_type,
        source_chat_id,
        chat_type,
        chat_id,
        sid,
        committed_pgid
            .map(|pgid| format!("live-pgid-{pgid}"))
            .unwrap_or_else(|| "boot-proven-gone".to_string())
    ));
    Ok(summon_i18n::format!(
        "r3b9cbf32e7",
        provider.label(),
        if source_name.is_empty() {
            summon_i18n::text!("r9d4a2e95b5")
        } else {
            &source_name
        },
        if cwd_ready.is_ok() && meta_ready.is_ok() {
            ""
        } else {
            summon_i18n::text!("r43f863015a")
        }
    ))
}

fn recall_headless_into_group(
    agent: &Agent,
    request: HeadlessRecallGroupRequest<'_>,
    chats: &HashMap<String, ChatHandle>,
    stores: LocalChatStores<'_>,
) -> String {
    let HeadlessRecallGroupRequest {
        provider,
        storage_scope,
        sid,
        chat_id,
        chat_type,
        actor_open_id,
    } = request;
    if crate::session::exit_mark::exists(agent, chat_type, chat_id) {
        return summon_i18n::text!("rd9fd9c3ed9").to_string();
    }
    let expected = summon_core::recall::RecallProvider::from_cli_kind(agent.cli_kind());
    if expected != Some(provider) {
        return summon_i18n::format!("r377921a016", provider.label(), agent.cli_kind());
    }
    if !external_headless_channel_compatible(agent) {
        return summon_i18n::format!(
            "rc2c63acacf",
            provider.label(),
            agent.cli_kind(),
            agent.resume_cli_kind()
        );
    }
    let found = match crate::bridge::recall_headless::find(provider, sid) {
        Ok(Some(found)) => found,
        Ok(None) => return summon_i18n::format!("r46c74247e9", provider.label()),
        Err(error) => return summon_i18n::format!("r5574ed2ffc", provider.label()),
    };
    if let Err(error) = crate::bridge::recall_headless::ensure_not_open(&found) {
        return summon_i18n::format!("rf37a6602d8");
    }
    let cwd = found.candidate.cwd.trim();
    if !std::path::Path::new(cwd).is_absolute() || !std::path::Path::new(cwd).is_dir() {
        return summon_i18n::text!("r74e7c1450b").to_string();
    }
    match local_chat_blockers(
        agent,
        storage_scope,
        chat_type,
        chat_id,
        chats,
        stores,
        ReplyPlace::Private,
    ) {
        Ok(blockers) if !blockers.is_empty() => {
            return occupied_reply(summon_i18n::text!("rf691a5c449"), &blockers)
        }
        Err(error) => return summon_i18n::format!("r1381e88a80"),
        Ok(_) => {}
    }
    let canonical =
        match persist_headless_recall_binding(agent, &found, chat_id, chat_type, actor_open_id) {
            Ok(canonical) => canonical,
            Err(error) => return summon_i18n::format!("r790fe78bbb"),
        };
    log(&summon_i18n::format!(
        "rec4d776fdc",
        agent.name,
        provider.label(),
        sid
    ));
    summon_i18n::format!(
        "r9a7e4e3474",
        provider.label(),
        if found.candidate.name.is_empty() {
            sid
        } else {
            &found.candidate.name
        }
    )
}

fn external_headless_channel_compatible(agent: &Agent) -> bool {
    agent.resume_cli_kind() == agent.cli_kind()
}

fn persist_headless_recall_binding(
    agent: &Agent,
    found: &crate::bridge::recall_headless::HeadlessRecallCandidate,
    chat_id: &str,
    chat_type: &str,
    actor_open_id: &str,
) -> Result<String> {
    let expected = summon_core::recall::RecallProvider::from_cli_kind(agent.cli_kind());
    anyhow::ensure!(
        expected == Some(found.candidate.provider),
        summon_i18n::format!("r3ef5328dad")
    );
    let cwd = found.candidate.cwd.trim();
    anyhow::ensure!(
        std::path::Path::new(cwd).is_absolute() && std::path::Path::new(cwd).is_dir(),
        summon_i18n::format!("r8b3f70255c")
    );
    let canonical = agent.tmux_name_for_chat(chat_type, chat_id, false);
    // `.session` 是提交点：cwd/meta 先写，即使中途崩溃也不会触发 resume；只有三项都准备好后
    // 才写 session。反过来先写 session 会让并发入站在错误 cwd 启动 provider。
    write_chat_cwd_checked(agent, &canonical, cwd).context(summon_i18n::text!("r9c9e754ab9"))?;
    write_chat_meta_checked(
        agent,
        &canonical,
        chat_id,
        chat_type,
        None,
        actor_open_id,
        Some(actor_open_id),
    )
    .context(summon_i18n::text!("re26d298c3e"))?;
    crate::session::resume_store::write_checked(
        agent,
        &canonical,
        agent.cli_kind(),
        &found.candidate.sid,
        Some(cwd),
        None,
    )
    .context(summon_i18n::text!("rcbf01bc04f"))?;
    // resume 记录是这条路的 durable commit;提交成功 = 新的路由生命周期开始。bridge 不过
    // `handle_task` 的 admission,残留的 `closed` 必须在这里清掉,否则刚恢复的会话会被判成已关闭。
    //
    // **失败只记日志,绝不向上传播**。上面那行 resume 记录已经是不可逆的 durable commit:此刻
    // 报错会造成"界面说失败、绑定其实已生效"的部分提交,而用户再点一次恢复会被
    // `local_chat_occupied` 以"resume 记录已存在"拒掉 —— 原操作再也重试不了。这条路不像
    // recall/takeover 有 durable 状态机兜重试,只能就地降级:状态晚一拍,由目标 chat 的下一条
    // 入站经 admission 自愈(与 RPC handoff 同一取舍)。
    if let Err(error) = crate::session::lifecycle::mark_open(agent, &chat_tag(chat_type, chat_id)) {
        log(&summon_i18n::format!("ra1b176c63f", agent.name));
    }
    Ok(canonical)
}

fn recall_resume_into_group(
    agent: &Agent,
    store: &crate::bridge::store::BridgeStore,
    request: RecallGroupRequest<'_>,
) -> String {
    use summon_core::recall::{resume_mode, RecallResumeMode};
    let RecallGroupRequest {
        storage_scope,
        sid,
        chat_id,
        chat_type,
        actor_open_id,
        source_observation_id,
    } = request;
    if crate::session::exit_mark::exists(agent, chat_type, chat_id) {
        return summon_i18n::text!("rd9fd9c3ed9").to_string();
    }
    // 这条接管路径不走 local_chat_blockers,遥控占用要单独拦,且必须在任何杀源之前。
    match remote_binding_live(agent, chat_id) {
        Ok(false) => {}
        Ok(true) => {
            return occupied_reply(
                summon_i18n::text!("ree3d6b44ab"),
                &[remote_bound_blocker(ReplyPlace::Private)],
            )
        }
        Err(error) => return summon_i18n::format!("r2ed3be4590"),
    }
    // 同一条会话可能正被别的群遥控:接管会结束 cmux 里那个进程,同样在任何杀源之前拦下。此刻
    // 还没找候选、不知道要结束哪个 pid,先按 sid 查;杀源前按 pid 再查一次。
    if let Some(refusal) = session_remote_refusal(agent, sid, None) {
        return refusal.to_string();
    }
    let Some(home) = dirs::home_dir() else {
        return summon_i18n::text!("rde14ef2c2a").to_string();
    };
    let projects = home.join(".claude/projects");
    let Some(candidate) = crate::bridge::recall::find_candidate(&projects, sid) else {
        return summon_i18n::format!("rba893cccca");
    };
    let target = ScopedChatKey::new(&storage_scope.key, &agent.name, chat_id);
    // 先完成目标 runtime/auth/transcript/cwd 全量 preflight；任何失败都发生在杀源前，保留源。
    let launch = match crate::bridge::recall_resume::build_recall_resume_plan(
        agent,
        target,
        chat_type,
        sid,
        &candidate.cwd,
        actor_open_id,
    ) {
        Ok(launch) => launch,
        Err(error) => {
            log(&summon_i18n::format!("r1c306f2ea0", agent.name));
            return summon_i18n::format!("rdfa0ed3fc5");
        }
    };
    // 只要“当前仍活跃”或“卡片渲染时曾活跃”任一成立，都必须走 source-exit proof。
    // 后者防 `claude agents` 瞬时失败/源变 summon-managed 后候选表面降成历史，绕过杀源证明。
    let requires_source_exit = recall_requires_source_exit(&candidate, source_observation_id);
    if requires_source_exit {
        let Some(observation_id) = source_observation_id else {
            return summon_i18n::text!("r26cdf42887").to_string();
        };
        let observation = match store.load_recall_source_observation(observation_id) {
            Ok(observation) => observation,
            Err(error) => return summon_i18n::format!("r375ef789f7"),
        };
        let current_pid = match resume_mode(&candidate) {
            RecallResumeMode::ActiveKillSource { pid } => Some(pid),
            RecallResumeMode::Historical => None,
        };
        if observation.sid != sid || current_pid.is_some_and(|pid| observation.process.pid != pid) {
            return summon_i18n::text!("r7d94bf595d").to_string();
        }
        let observed_pid = observation.process.pid;
        // 信号打的是这个 pid:它若在遥控中(sid 可能已被 `/clear` 换掉),一样不能结束。
        if let Some(refusal) = session_remote_refusal(agent, sid, Some(observed_pid)) {
            return refusal.to_string();
        }
        let attempt = launch.attempt();
        match crate::bridge::recall::terminate_active_source(
            &observation,
            sid,
            std::path::Path::new(&attempt.config_dir),
            std::path::Path::new(&attempt.transcript_path),
            std::path::Path::new(&attempt.program),
        ) {
            Ok(outcome) => log(&summon_i18n::format!("rec868a83b1", agent.name)),
            Err(error) => return summon_i18n::format!("r003e5896ea"),
        }
    } else if let Err(error) = crate::bridge::recall::prove_no_source_writer(
        sid,
        std::path::Path::new(&launch.attempt().config_dir),
        std::path::Path::new(&launch.attempt().transcript_path),
    ) {
        return summon_i18n::format!("r359f39811c");
    }
    let canonical = launch.attempt().canonical_tmux.clone();
    let attempt_id = launch.attempt().id.clone();
    // ★ Stage 2 时序红线：活跃源只有在上面确认 Gone + transcript 稳定后才到这里落 attempt。
    // attempt durable 后 store 才接受 ACK；随后才起 staging。绝不能把 persist 移到杀源前。
    if let Err(error) = store.persist_recall_resume_attempt(launch.attempt()) {
        return summon_i18n::format!("r9dfb55fc65");
    }
    match launch.launch_staging() {
        Ok(_staging) => {
            log(&summon_i18n::format!("r1526dc4344", agent.name));
            summon_i18n::format!(
                "rd85c0727d4",
                if candidate.name.is_empty() {
                    sid
                } else {
                    &candidate.name
                }
            )
        }
        Err(error) => {
            // 起 staging 失败:清掉刚落盘的 attempt,不留 Launched 孤儿等超时(tick 也会兜底)。
            let _ = store.remove_terminal_recall_resume_attempt(&attempt_id);
            summon_i18n::format!("rae670bf41e")
        }
    }
}

/// 召回卡按钮回调的纯决策:把「回调 value + 是否已过 operate-gate」映射成一个动作计划。
///
/// **这是起会话/绑群的唯一守门**:破坏性恢复只在 `authorized == true` 时返回 Resume*;
/// authorized=false 一律 Reject,**绝不进破坏性臂**(回归测钉死)。抽成纯函数便于脱离 IO 单测。
/// `recall_page` 纯读,与 authorized 无关;坏参数 → Reject。原群 origin 从 value 解析(不透明穿透)。
enum RecallCallbackPlan<'a> {
    /// 翻页(纯读):渲染第 N 页,保留原群 origin(否则翻页后丢群)。
    Page {
        page: usize,
        origin: Option<(&'a str, &'a str)>,
    },
    /// 恢复进原群 G(破坏性,已过 operate-gate)。
    ResumeGroup {
        provider: summon_core::recall::RecallProvider,
        sid: &'a str,
        chat_id: &'a str,
        chat_type: &'a str,
        source_observation_id: Option<&'a str>,
    },
    /// 无原群兜底:恢复为本地托管 tmux(旧卡 / DM;已过 operate-gate)。
    ResumeLocal {
        provider: summon_core::recall::RecallProvider,
        sid: &'a str,
    },
    /// summon-owned RPC 会话从源群迁到 origin 目标群（破坏性，已过 operate-gate）。
    RpcHandoffGroup {
        provider: summon_core::recall::RecallProvider,
        sid: &'a str,
        chat_id: &'a str,
        chat_type: &'a str,
        source_ref: &'a str,
        observed_at_ms: u64,
    },
    /// 把 origin 目标群接到一条 cmux 里的 Claude 会话上遥控(已过 operate-gate)。
    RemoteGroup {
        sid: &'a str,
        chat_id: &'a str,
        chat_type: &'a str,
    },
    /// 拒绝:回执文案(gate 未过 / 参数无效)。
    Reject(&'static str),
}

/// 从回调 value 解析原群 origin。fail-closed:chat_id 空 / chat_type 非群 → None(回落本地)。
fn parse_recall_origin(value: &serde_json::Value) -> Option<(&str, &str)> {
    let chat_id = value.get("chat_id").and_then(|v| v.as_str())?;
    let chat_type = value.get("chat_type").and_then(|v| v.as_str())?;
    if chat_id.is_empty() || !matches!(chat_type, "group" | "topic_group") {
        return None;
    }
    Some((chat_id, chat_type))
}

fn callback_recall_provider(
    value: &serde_json::Value,
) -> Option<summon_core::recall::RecallProvider> {
    // 兼容修复发布前已经发出的 Claude 卡；缺 cli 只可解释为旧 Claude，不能按当前 agent 猜。
    match value.get("cli").and_then(|value| value.as_str()) {
        None => Some(summon_core::recall::RecallProvider::Claude),
        Some(kind) => summon_core::recall::RecallProvider::from_cli_kind(kind),
    }
}

fn plan_recall_callback(
    value: &serde_json::Value,
    authorized: bool,
    expected_provider: Option<summon_core::recall::RecallProvider>,
) -> RecallCallbackPlan<'_> {
    let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let Some(provider) = callback_recall_provider(value) else {
        return RecallCallbackPlan::Reject(summon_i18n::text!("rb4db0d8698"));
    };
    if expected_provider != Some(provider) {
        return RecallCallbackPlan::Reject(summon_i18n::text!("rc5e58b95a1"));
    }
    match action {
        "recall_page" if !authorized => {
            RecallCallbackPlan::Reject(summon_i18n::text!("r8d6762be6f"))
        }
        "recall_page" => match value.get("page").and_then(|v| v.as_u64()) {
            Some(page) => RecallCallbackPlan::Page {
                page: page as usize,
                origin: parse_recall_origin(value),
            },
            None => RecallCallbackPlan::Reject(summon_i18n::text!("r1279571640")),
        },
        "recall_resume" => {
            // ★ 破坏性恢复红线:gate 未过绝不落到 Resume*(=绝不 resume/绑群/杀源)。
            if !authorized {
                return RecallCallbackPlan::Reject(summon_i18n::text!("r4218b74f2d"));
            }
            let Some(sid) = value.get("sid").and_then(|v| v.as_str()) else {
                return RecallCallbackPlan::Reject(summon_i18n::text!("rfaf6fc664c"));
            };
            match parse_recall_origin(value) {
                Some((chat_id, chat_type)) => RecallCallbackPlan::ResumeGroup {
                    provider,
                    sid,
                    chat_id,
                    chat_type,
                    source_observation_id: value
                        .get("source_observation")
                        .and_then(|value| value.as_str()),
                },
                None => RecallCallbackPlan::ResumeLocal { provider, sid },
            }
        }
        "recall_rpc_handoff" => {
            if !authorized {
                return RecallCallbackPlan::Reject(summon_i18n::text!("rc59c3a9228"));
            }
            let Some(sid) = value.get("sid").and_then(|v| v.as_str()) else {
                return RecallCallbackPlan::Reject(summon_i18n::text!("r8ac88abfa2"));
            };
            let Some(source_ref) = value
                .get("source_observation")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
            else {
                return RecallCallbackPlan::Reject(summon_i18n::text!("r61092bdb0a"));
            };
            let Some(observed_at_ms) = value.get("observed_at_ms").and_then(|value| value.as_u64())
            else {
                return RecallCallbackPlan::Reject(summon_i18n::text!("r88a24f0a6e"));
            };
            match parse_recall_origin(value) {
                Some((chat_id, chat_type)) => RecallCallbackPlan::RpcHandoffGroup {
                    provider,
                    sid,
                    chat_id,
                    chat_type,
                    source_ref,
                    observed_at_ms,
                },
                None => RecallCallbackPlan::Reject(summon_i18n::text!("r03ccea4b86")),
            }
        }
        "recall_cmux_remote" => {
            if !authorized {
                return RecallCallbackPlan::Reject(summon_i18n::text!("rbecca4dfc7"));
            }
            if provider != summon_core::recall::RecallProvider::Claude {
                return RecallCallbackPlan::Reject(summon_i18n::text!("r9b9b9e0819"));
            }
            let Some(sid) = value.get("sid").and_then(|v| v.as_str()) else {
                return RecallCallbackPlan::Reject(summon_i18n::text!("r7a0a8694b6"));
            };
            match parse_recall_origin(value) {
                Some((chat_id, chat_type)) => RecallCallbackPlan::RemoteGroup {
                    sid,
                    chat_id,
                    chat_type,
                },
                None => RecallCallbackPlan::Reject(summon_i18n::text!("r97e2b2f2e5")),
            }
        }
        _ => RecallCallbackPlan::Reject(summon_i18n::text!("r27bc830e7d")),
    }
}

/// 召回卡按钮回调 handler(`recall_page` / `recall_resume` / `recall_rpc_handoff` /
/// `recall_cmux_remote`)。召回无状态(每次从盘 + `claude agents` 现采),除 RPC 迁移外不走
/// ticket/durable WAL,处理完由 dispatcher fall-through `ack_card_callback` 收尾。
///
/// 所有召回动作（包括翻页）都按目标群作用域重新过 operate-gate：列表含工作目录和任务摘要，
/// 不是可公开转发的只读数据。绝不用私聊卡的 is_group（空 operate 名单会因此 allow-all）。
/// 鉴权结果只作唯一安全变量喂给 [`plan_recall_callback`]，由其 fail-closed 断死。
pub(super) fn handle_bridge_recall_card_action(
    agent: &Agent,
    fc: &Arc<FeishuClient>,
    storage_scope: &FeishuStorageScope,
    chats: &mut HashMap<String, ChatHandle>,
    store: &crate::bridge::store::BridgeStore,
    card: &CardActionMsg,
    local_stores: LocalChatStores<'_>,
) {
    let issued_at_ms = card
        .value
        .get("card_issued_at_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let target_chat_id = card
        .value
        .get("chat_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let target_chat_type = card
        .value
        .get("chat_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let target_lifecycle_token = card
        .value
        .get("target_lifecycle_token")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let now_ms = now_unix_ms();
    let binding_valid = card
        .value
        .get("bound_agent")
        .and_then(serde_json::Value::as_str)
        == Some(agent.name.as_str())
        && card
            .value
            .get("bound_scope")
            .and_then(serde_json::Value::as_str)
            == Some(storage_scope.key.as_str())
        && card
            .value
            .get("bound_requester")
            .and_then(serde_json::Value::as_str)
            == Some(card.operator_open_id.as_str())
        && matches!(target_chat_type, "group" | "topic_group")
        && !crate::session::exit_mark::exists(agent, target_chat_type, target_chat_id)
        && recall_target_lifecycle_matches(agent, target_chat_id, target_lifecycle_token)
        && issued_at_ms <= now_ms.saturating_add(RPC_HANDOFF_CARD_CLOCK_SKEW_MS)
        && now_ms.saturating_sub(issued_at_ms) <= RPC_HANDOFF_CARD_TTL_MS;
    if !binding_valid {
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            summon_i18n::text!("rc356b22d77"),
        );
        return;
    }
    let action = card
        .value
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // 翻页同样暴露 session/cwd/prompt，必须在点击时重验当前 operate 权限。
    let authorized = if matches!(
        action,
        "recall_page" | "recall_resume" | "recall_rpc_handoff" | "recall_cmux_remote"
    ) {
        let op_union = fc.resolve_union_id(&card.operator_open_id);
        // 洞#3:硬编码 is_group=true,针对目标群鉴权;绝不用私聊卡的 is_group。
        bridge_operator_allowed(agent, &card.operator_open_id, &op_union)
    } else {
        false
    };
    if !authorized {
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            summon_i18n::text!("r8d6762be6f"),
        );
        return;
    }
    if let Err(error) = fc.prove_bot_in_chat(target_chat_id) {
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            &summon_i18n::format!("rdcf538834c"),
        );
        return;
    }
    match plan_recall_callback(
        &card.value,
        authorized,
        summon_core::recall::RecallProvider::from_cli_kind(agent.cli_kind()),
    ) {
        RecallCallbackPlan::Page { page, origin } => {
            let rendered = crate::bridge::recall::recall_card_for_agent(
                agent,
                local_stores.inflight,
                store,
                crate::bridge::recall::RecallCardRequest {
                    page,
                    page_size: RECALL_PAGE_SIZE,
                    origin,
                    binding_scope: &storage_scope.key,
                    requester_open_id: &card.operator_open_id,
                    target_lifecycle_token,
                    card_issued_at_ms: issued_at_ms,
                    source_note: None,
                },
            );
            if let Err(error) = update_card_archived(
                fc,
                &agent.state_dir(),
                "p2p",
                &card.chat_id,
                &card.message_id,
                &rendered,
            ) {
                // PATCH 失败不致命:原卡仍在,用户可重点翻页;记录即可,不回执刷屏。
                log(&summon_i18n::format!("r623e583a2f", agent.name));
            }
        }
        // 以下两臂必然已过 operate-gate(plan_recall_callback fail-closed)。
        RecallCallbackPlan::ResumeGroup {
            provider,
            sid,
            chat_id,
            chat_type,
            source_observation_id,
        } => {
            let target = ScopedChatKey::new(&storage_scope.key, &agent.name, chat_id);
            let reply = match bridge_slot_message(store, &target) {
                Ok(Some(message)) => {
                    summon_i18n::format!("rbc3006c1c1")
                }
                Err(error) => summon_i18n::format!("r68231b712d"),
                Ok(None) if provider == summon_core::recall::RecallProvider::Claude => {
                    recall_resume_into_group(
                        agent,
                        store,
                        RecallGroupRequest {
                            storage_scope,
                            sid,
                            chat_id,
                            chat_type,
                            actor_open_id: &card.operator_open_id,
                            source_observation_id,
                        },
                    )
                }
                Ok(None) => recall_headless_into_group(
                    agent,
                    HeadlessRecallGroupRequest {
                        provider,
                        storage_scope,
                        sid,
                        chat_id,
                        chat_type,
                        actor_open_id: &card.operator_open_id,
                    },
                    chats,
                    local_stores,
                ),
            };
            send_bridge_reply(fc, agent, "p2p", &card.chat_id, &reply);
        }
        RecallCallbackPlan::ResumeLocal { provider, sid } => {
            let reply = if provider == summon_core::recall::RecallProvider::Claude {
                recall_resume_reply(agent, sid)
            } else {
                summon_i18n::format!("r62510aab7a", provider.label())
            };
            send_bridge_reply(fc, agent, "p2p", &card.chat_id, &reply);
        }
        RecallCallbackPlan::RpcHandoffGroup {
            provider,
            sid,
            chat_id,
            chat_type,
            source_ref,
            observed_at_ms,
        } => {
            let reply = recall_rpc_handoff_into_group(
                agent,
                fc,
                RpcHandoffGroupRequest {
                    provider,
                    storage_scope,
                    sid,
                    chat_id,
                    chat_type,
                    actor_open_id: &card.operator_open_id,
                    source_ref,
                    observed_at_ms,
                    replayed_from_wal: card.wal_replayed,
                },
                chats,
                local_stores,
                store,
            );
            send_bridge_reply(fc, agent, "p2p", &card.chat_id, &reply);
        }
        RecallCallbackPlan::RemoteGroup {
            sid,
            chat_id,
            chat_type,
        } => {
            let target = ScopedChatKey::new(&storage_scope.key, &agent.name, chat_id);
            let reply = match bridge_slot_message(store, &target) {
                Ok(Some(message)) => {
                    summon_i18n::format!("ra0fe1b719d")
                }
                Err(error) => summon_i18n::format!("rc1924bc100"),
                // 先按 sid 查,不读 cmux 记录;读到 cmux 记录里的 pid 之后,bind_cmux_remote
                // 再按 pid 查一次。
                Ok(None) => match takeover_pending_for(store, sid, None) {
                    Ok(true) => ::summon_core::i18n::label(SESSION_TAKEOVER_PENDING).to_string(),
                    Err(error) => {
                        log(&summon_i18n::format!("re821cb35c8", agent.name));
                        ::summon_core::i18n::label(TAKEOVER_RECORDS_UNREADABLE).to_string()
                    }
                    Ok(false) => bind_cmux_remote(
                        agent,
                        fc,
                        RemoteBindRequest {
                            storage_scope,
                            sid,
                            chat_id,
                            chat_type,
                            actor_open_id: &card.operator_open_id,
                            takeovers: store,
                        },
                        chats,
                        local_stores,
                    ),
                },
            };
            send_bridge_reply(fc, agent, "p2p", &card.chat_id, &reply);
        }
        RecallCallbackPlan::Reject(text) => {
            send_bridge_reply(fc, agent, "p2p", &card.chat_id, text);
        }
    }
}

struct RemoteBindRequest<'a> {
    storage_scope: &'a FeishuStorageScope,
    sid: &'a str,
    chat_id: &'a str,
    chat_type: &'a str,
    actor_open_id: &'a str,
    /// Bridge 接管记录:同一个进程有没走完的接管就不绑。
    takeovers: &'a crate::bridge::store::BridgeStore,
}

/// 召回卡「遥控」:逐项核对后把目标群接到一条 cmux 里的 Claude 会话上,返回私聊回执。
///
/// 记录与 offset 先落盘再起 worker:worker 只按盘上仍有效的记录起,回传从 offset 读起;起不来
/// 就撤掉刚写的记录。全程不对那个 Claude 进程发任何信号。往群里只发一条接入提示。
fn bind_cmux_remote(
    agent: &Agent,
    fc: &Arc<FeishuClient>,
    request: RemoteBindRequest<'_>,
    chats: &HashMap<String, ChatHandle>,
    stores: LocalChatStores<'_>,
) -> String {
    use crate::remote::bind::{prepare_binding, session_label};
    use crate::remote::cmux::{load_cmux_sessions, CmuxCli, CmuxPort};
    use crate::remote::store::RemoteStore;
    use summon_core::remote::{RemoteBinding, REMOTE_BINDING_SCHEMA_VERSION};
    let RemoteBindRequest {
        storage_scope,
        sid,
        chat_id,
        chat_type,
        actor_open_id,
        takeovers,
    } = request;
    let refuse = |reason: &str, detail: &str| {
        log(&summon_i18n::format!("r2292454d55", agent.name));
        reason.to_string()
    };
    let store = RemoteStore::for_agent(agent);
    // 读不出就不知道群或会话是不是已经在遥控,一律不绑。
    let (here, elsewhere) = match (store.load(chat_id), store.find_by_session(sid)) {
        (Ok(here), Ok(elsewhere)) => (here, elsewhere),
        (Err(error), _) | (_, Err(error)) => {
            return refuse(summon_i18n::text!("re24be7d2ae"), &format!(" {error:#}"))
        }
    };
    let sessions = match load_cmux_sessions() {
        Ok(sessions) => sessions,
        Err(error) => return refuse(summon_i18n::text!("rb8859ae587"), &format!(" {error:#}")),
    };
    let probe_failed = std::cell::Cell::new(false);
    let process_start = |pid: u32| match crate::bridge::process_probe::process_start_epoch_secs(pid)
    {
        Ok(start) => start,
        Err(error) => {
            log(&summon_i18n::format!("ra0c49588e3", agent.name));
            probe_failed.set(true);
            None
        }
    };
    let session = match prepare_binding(
        sid,
        &sessions,
        &process_start,
        here.as_ref(),
        elsewhere.as_ref(),
    ) {
        Ok(session) => session,
        Err(_) if probe_failed.get() => return refuse(summon_i18n::text!("rd16f7f8f74"), ""),
        Err(reason) => return refuse(&reason, ""),
    };
    // 接管的信号打 pid:`/clear` 换掉 sid 之后,同一个进程上没走完的接管只能按 pid 认出来。
    match takeover_pending_for(takeovers, sid, Some(session.pid)) {
        Ok(false) => {}
        Ok(true) => {
            return refuse(
                ::summon_core::i18n::label(SESSION_TAKEOVER_PENDING),
                &format!(" pid={}", session.pid),
            )
        }
        Err(error) => {
            return refuse(
                ::summon_core::i18n::label(TAKEOVER_RECORDS_UNREADABLE),
                &format!(" {error:#}"),
            )
        }
    }
    if let Err(error) = CmuxCli::installed().ping() {
        log(&summon_i18n::format!("reee0bedb88", agent.name));
        return if crate::remote::cmux::is_auth_failure(&error) {
            summon_i18n::text!("r6a9f9dfc81")
        } else {
            summon_i18n::text!("r47be202055")
        }
        .to_string();
    }
    let claude_name = crate::bridge::recall::discover_active_for_display()
        .into_iter()
        .find(|entry| entry.session_id == sid)
        .and_then(|entry| entry.name);
    let name = session_label(claude_name.as_deref(), &session.cwd, sid);
    // 占用紧挨着落盘查:前面的 ping 与会话发现各要等几秒,这期间 Bridge 接管可能已经占了群。
    match local_chat_blockers(
        agent,
        storage_scope,
        chat_type,
        chat_id,
        chats,
        stores,
        ReplyPlace::Private,
    ) {
        Ok(blockers) if !blockers.is_empty() => {
            return occupied_reply(summon_i18n::text!("r18ad9a743a"), &blockers)
        }
        Err(error) => return summon_i18n::format!("rd17fd6e0d6"),
        Ok(_) => {}
    }
    // 回传起点是落盘前这一刻的转录末尾:更早量的话,ping 与会话发现那几秒里收尾的轮会被当成
    // 绑定之后的回复发进群。
    let offset = match std::fs::metadata(&session.transcript_path) {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        other => {
            return refuse(
                summon_i18n::text!("rc08c78227a"),
                &format!(" path={} {:?}", session.transcript_path, other.err()),
            )
        }
    };
    let binding = RemoteBinding {
        schema_version: REMOTE_BINDING_SCHEMA_VERSION,
        agent: agent.name.clone(),
        // 回传事件按这个 scope 投递,必须与 outbox drain 当前的 storage scope 一致。
        scope: storage_scope.key.clone(),
        chat_id: chat_id.to_string(),
        chat_type: chat_type.to_string(),
        session_id: session.session_id.clone(),
        session_name: name.clone(),
        surface_id: session.surface_id.clone(),
        workspace_id: session.workspace_id.clone(),
        pid: session.pid,
        pid_start_seconds: session.pid_start_seconds,
        transcript_path: session.transcript_path.clone(),
        actor_open_id: actor_open_id.to_string(),
        created_at_ms: now_unix_ms(),
        ended: None,
    };
    // 本群已结束的旧记录连同它的 offset 先清掉。offset 先于记录写:记录是提交点,有记录就有
    // 它的 offset,回传从绑定这一刻的转录末尾读起。
    let written = store
        .remove(chat_id)
        .and_then(|()| store.write_offset(chat_id, offset))
        .and_then(|()| store.save(&binding));
    if let Err(error) = written {
        if let Err(cleanup) = store.remove(chat_id) {
            log(&summon_i18n::format!("r783cd7ffca", agent.name));
        }
        return refuse(summon_i18n::text!("r81166b898d"), &format!(" {error:#}"));
    }
    if !crate::remote::runtime::start(agent, Arc::clone(fc), binding) {
        log(&summon_i18n::format!("r70926adcc7", agent.name));
        return match store.remove(chat_id) {
            Ok(()) => summon_i18n::text!("r2a19279513").to_string(),
            Err(error) => {
                log(&summon_i18n::format!("rd4918bdceb", agent.name));
                summon_i18n::text!("r329f66eaa9").to_string()
            }
        };
    }
    log(&summon_i18n::format!(
        "r9b08f9c66f",
        agent.name,
        session.surface_id,
        session.pid
    ));
    let label = summon_core::feishu::escape_lark_md(&name);
    send_bridge_reply(
        fc,
        agent,
        chat_type,
        chat_id,
        &summon_i18n::format!("re2347d961e"),
    );
    summon_i18n::format!("r4469fcfbf3")
}

pub(super) fn bridge_slot_message(
    store: &crate::bridge::store::BridgeStore,
    target: &ScopedChatKey,
) -> Result<Option<&'static str>> {
    bridge_slot_message_at(store, target, now_unix_ms())
}

fn bridge_slot_message_at(
    store: &crate::bridge::store::BridgeStore,
    target: &ScopedChatKey,
    now_ms: u64,
) -> Result<Option<&'static str>> {
    let Some(state) = store.load_state()? else {
        return Ok(None);
    };
    if state.takeover_conflict(target) {
        return Ok(Some(summon_i18n::text!("r47c41dbc50")));
    }
    let needs_expiry = matches!(
        state.slot(target),
        Some(ChatSlot::Pending { expires_at_ms, .. }) if *expires_at_ms < now_ms
    );
    if !needs_expiry {
        return Ok(bridge_slot_text(state.slot(target)));
    }
    // 只有确实命中过期 Pending 才做 RMW；普通群消息在无 bridge 时不制造 state 写放大。
    store.with_state(|state| {
        state.expire_pending(now_ms);
        Ok(bridge_slot_text(state.slot(target)))
    })
}

fn bridge_slot_text(slot: Option<&ChatSlot>) -> Option<&'static str> {
    match slot {
        None => None,
        Some(ChatSlot::Pending { .. }) => Some(summon_i18n::text!("r9dcb110b79")),
        Some(ChatSlot::Activating(_)) => Some(summon_i18n::text!("rfdb3dd4773")),
        Some(ChatSlot::MirrorBound(_)) => Some(summon_i18n::text!("r2f8454b716")),
        Some(ChatSlot::HandoffPending(_)) => Some(summon_i18n::text!("r2b880341dc")),
        Some(ChatSlot::ExternalOwned(_)) => Some(summon_i18n::text!("re3acb785a2")),
        Some(ChatSlot::Closing { .. }) => Some(summon_i18n::text!("r7e54a7bfc4")),
    }
}

fn valid_bridge_value(card: &CardActionMsg) -> Option<(&str, &str, &str)> {
    let object = card.value.as_object()?;
    let keys = object
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if keys != std::collections::BTreeSet::from(["action", "choice", "ticket"]) {
        return None;
    }
    Some((
        object.get("action")?.as_str()?,
        object.get("ticket")?.as_str()?,
        object.get("choice")?.as_str()?,
    ))
}

/// durable card callback 只有在动作已经持久化，或已确定不应再执行时才能 ACK。
/// `Retry` 会让 dispatcher 保留 callback WAL，daemon 冷启后按原顺序重放。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallbackDisposition {
    Terminal,
    Retry,
}

/// `BridgeStore::with_state` 同时承载领域拒绝与持久化 I/O 错误。用显式 marker 把“状态已变化、
/// callback 无需再试”的领域终态从可恢复 I/O 中分开，避免按错误文案猜测 ACK 时机。
#[derive(Debug)]
struct TerminalCallbackError(String);

impl std::fmt::Display for TerminalCallbackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TerminalCallbackError {}

fn terminal_callback_error(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::Error::new(TerminalCallbackError(error.to_string()))
}

fn is_terminal_callback_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<TerminalCallbackError>().is_some()
}

fn cancel_ticket_durably(store: &crate::bridge::store::BridgeStore, ticket_id: &str) -> Result<()> {
    store.with_state(|state| match state.cancel_ticket(ticket_id) {
        Ok(())
        | Err(summon_core::bridge::BridgeError::TicketFinal)
        | Err(summon_core::bridge::BridgeError::TicketMissing) => Ok(()),
        Err(error) => Err(anyhow::Error::new(error)),
    })
}

fn context_callback_receipt_id(
    card: &CardActionMsg,
    ticket_id: &str,
    choice_id: &str,
) -> Option<String> {
    if card.event_id.is_empty() {
        return None;
    }
    // event_id 只用于求不透明稳定 receipt，避免把平台标识直接扩散到更多状态字段。
    Some(summon_core::outbox::bridge_delivery_id(
        summon_core::outbox::BridgeDeliveryKind::ContextCapsule,
        &format!(
            "callback\0{}\0{}\0{}\0{}",
            card.event_id, card.message_id, ticket_id, choice_id
        ),
    ))
}

/// callback adapter 的依赖注入接缝；生产 dispatcher 固定 runtime stores，fake E2E 使用 tempdir。
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_bridge_card_action_with_stores(
    agent: &Agent,
    fc: &FeishuClient,
    storage_scope: &FeishuStorageScope,
    chats: &HashMap<String, ChatHandle>,
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
    inflight: &crate::runtime::inflight::InFlight,
    card: &CardActionMsg,
    store: &crate::bridge::store::BridgeStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
) -> CallbackDisposition {
    let Some((action, ticket_id, choice_id)) = valid_bridge_value(card) else {
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            summon_i18n::text!("raa8858653f"),
        );
        return CallbackDisposition::Terminal;
    };
    let state = match store.load_state() {
        Ok(Some(state)) => state,
        Ok(None) => {
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("rb7f79bd2b8"),
            );
            return CallbackDisposition::Terminal;
        }
        Err(error) => {
            log(&summon_i18n::format!("ref45a47e3c", agent.name));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("r103ea4ce51"),
            );
            return CallbackDisposition::Retry;
        }
    };
    let Some(ticket) = state.tickets.get(ticket_id).cloned() else {
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            summon_i18n::text!("rb7f79bd2b8"),
        );
        return CallbackDisposition::Terminal;
    };
    // callback 只带 open_id；优先复用 ticket 创建时由可信入站事件记录的 union_id，避免
    // 联系人 API 短暂失败让 union-only operator 永久无法操作。open_id 必须先精确匹配，故
    // 这里不能被另一位操作者借用。
    let op_union = if ticket.requester_open_id == card.operator_open_id
        && !ticket.requester_union_id.is_empty()
    {
        ticket.requester_union_id.clone()
    } else {
        fc.resolve_union_id(&card.operator_open_id)
    };
    // 主动 DM/短码发卡都存在“飞书已接收、message_id 尚未登记时进程崩溃”的窗口。只有原
    // requester 的 Pending Mirror/Handoff 卡允许第一次 callback 原子 claim chat+message；
    // stop/context 和已有任一 identity 的 ticket 仍要求 exact match，不能借恢复口放宽。
    let recover_unattached_card = matches!(ticket.status, TicketStatus::Pending)
        && ticket.private_chat_id.is_none()
        && ticket.control_card_message_id.is_none()
        && matches!(action, "bridge_mirror" | "bridge_handoff");
    if ticket.target.scope != storage_scope.key
        || ticket.target.agent != agent.name
        || !matches!(ticket.target_chat_type.as_str(), "group" | "topic_group")
        || ticket.requester_open_id != card.operator_open_id
        || !(recover_unattached_card
            || ticket.private_chat_id.as_deref() == Some(card.chat_id.as_str())
            || (ticket.private_chat_id.is_none()
                && matches!(action, "bridge_mirror" | "bridge_handoff")))
        || !(recover_unattached_card
            || ticket.control_card_message_id.as_deref() == Some(card.message_id.as_str()))
        || !bridge_operator_allowed(agent, &card.operator_open_id, &op_union)
    {
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            summon_i18n::text!("r8541e00100"),
        );
        return CallbackDisposition::Terminal;
    }
    match action {
        "bridge_context" => {
            return handle_bridge_context_action(agent, fc, storage_scope, card, store, outbox);
        }
        "bridge_stop" => {
            return handle_bridge_stop_action(
                agent,
                fc,
                storage_scope,
                card,
                ticket_id,
                choice_id,
                store,
            );
        }
        "bridge_mirror" | "bridge_handoff" => {}
        _ => return CallbackDisposition::Terminal,
    }
    let mode = if action == "bridge_mirror" {
        BridgeMode::Mirror
    } else {
        BridgeMode::Handoff
    };
    match local_chat_blockers(
        agent,
        storage_scope,
        &ticket.target_chat_type,
        &ticket.target.chat_id,
        chats,
        LocalChatStores::new(inbox_wal, inflight, outbox),
        ReplyPlace::Private,
    ) {
        Ok(blockers) if blockers.is_empty() => {}
        Ok(blockers) => {
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                &occupied_reply(summon_i18n::text!("rab7d6dbfc8"), &blockers),
            );
            return CallbackDisposition::Terminal;
        }
        Err(error) => {
            log(&summon_i18n::format!("rd4ac3b164e", agent.name));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("rb672094a78"),
            );
            return CallbackDisposition::Retry;
        }
    }
    let binding_id = match &ticket.status {
        TicketStatus::Activating {
            binding_id,
            choice_id: current_choice,
            mode: current_mode,
        }
        | TicketStatus::Committed {
            binding_id,
            choice_id: current_choice,
            mode: current_mode,
        } if current_choice == choice_id && *current_mode == mode => binding_id.clone(),
        _ => format!("binding-{}", uuid::Uuid::new_v4().simple()),
    };
    let handoff_attempt_id = if mode == BridgeMode::Handoff {
        match store.load_handoff_attempts() {
            Ok(attempts) => attempts
                .into_iter()
                .find(|attempt| attempt.binding_id() == binding_id)
                .map(|attempt| attempt.attempt_id().to_string())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            Err(error) => {
                log(&summon_i18n::format!("r0fda154ab8", agent.name));
                return CallbackDisposition::Retry;
            }
        }
    } else {
        String::new()
    };
    let target_tmux =
        agent.tmux_name_for_chat(&ticket.target_chat_type, &ticket.target.chat_id, false);
    if mode == BridgeMode::Handoff
        && !crate::bridge::handoff::HandoffTargetProfile::agent_is_eligible(agent)
    {
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            summon_i18n::text!("re6ce888fbd"),
        );
        return CallbackDisposition::Terminal;
    }
    if mode == BridgeMode::Handoff
        && crate::bridge::handoff::HandoffTargetProfile::provider_mode(agent)
            != ticket.target_provider
    {
        let disposition = if let Err(error) = cancel_ticket_durably(store, ticket_id) {
            log(&summon_i18n::format!("rf1672b04cc", agent.name));
            CallbackDisposition::Retry
        } else {
            CallbackDisposition::Terminal
        };
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            summon_i18n::text!("r0b4b2550b2"),
        );
        return disposition;
    }
    // 接管会在安全边界结束源进程:正在被遥控的会话不能预留接管,仍可选 Mirror。按 sid 与要
    // 结束的那个 pid 都查;会话记录里没有进程时只按 sid 查。
    if mode == BridgeMode::Handoff {
        let refusal = ticket
            .choices
            .iter()
            .find(|choice| choice.id == choice_id)
            .and_then(|choice| {
                let pid = state
                    .session(&choice.session)
                    .and_then(|record| record.process.as_ref())
                    .map(|process| process.pid);
                session_remote_refusal(agent, &choice.session.session_id, pid)
            });
        if let Some(refusal) = refusal {
            send_bridge_reply(fc, agent, "p2p", &card.chat_id, refusal);
            return CallbackDisposition::Terminal;
        }
    }
    let expected_process = ticket
        .choices
        .iter()
        .find(|choice| choice.id == choice_id)
        .and_then(|choice| state.session(&choice.session))
        .and_then(|record| record.process.clone());
    if matches!(ticket.status, TicketStatus::Pending) {
        let live = expected_process
            .as_ref()
            .map(crate::bridge::process_probe::probe_exact_process);
        if matches!(live, Some(Err(_))) {
            log(&summon_i18n::format!("rb674ad1b71", agent.name, ticket_id));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("r5590871612"),
            );
            return CallbackDisposition::Retry;
        }
        if !matches!(
            live,
            Some(Ok(
                crate::bridge::process_probe::ExactProcessStatus::AliveExact
            ))
        ) {
            log(&summon_i18n::format!("rfb50a5340a", agent.name, ticket_id));
            let disposition = if let Err(error) = cancel_ticket_durably(store, ticket_id) {
                log(&summon_i18n::format!("rb55c28f8f1", agent.name, ticket_id));
                CallbackDisposition::Retry
            } else {
                CallbackDisposition::Terminal
            };
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                if disposition == CallbackDisposition::Terminal {
                    summon_i18n::text!("r58e4baf4b5")
                } else {
                    summon_i18n::text!("rf53dd2bc6a")
                },
            );
            return disposition;
        }
    }
    let result = store.with_state(|state| {
        match local_chat_occupied(
            agent,
            storage_scope,
            &ticket.target_chat_type,
            &ticket.target.chat_id,
            chats,
            LocalChatStores::new(inbox_wal, inflight, outbox),
        ) {
            Ok(false) => {}
            Ok(true) => {
                return Err(anyhow::Error::new(
                    summon_core::bridge::BridgeError::ChatOccupied,
                ))
            }
            Err(error) => return Err(error),
        }
        let current =
            state.tickets.get(ticket_id).cloned().ok_or_else(|| {
                anyhow::Error::new(summon_core::bridge::BridgeError::TicketMissing)
            })?;
        if current.target != ticket.target || current.target.scope != storage_scope.key {
            return Err(terminal_callback_error(summon_i18n::text!("r338dfe09a3")));
        }
        let choice = current
            .choices
            .iter()
            .find(|choice| choice.id == choice_id)
            .cloned()
            .ok_or_else(|| anyhow::Error::new(summon_core::bridge::BridgeError::ChoiceMissing))?;
        if matches!(current.status, TicketStatus::Pending) {
            let process = state
                .session(&choice.session)
                .and_then(|record| record.process.as_ref());
            if process != expected_process.as_ref() {
                return Err(anyhow::Error::new(
                    summon_core::bridge::BridgeError::StaleSession,
                ));
            }
        }
        // 主动 open_id 发卡时 private_chat_id 初始未知：第一次合法 callback 在同一 state lock
        // first-write；短码兑换已登记 p2p chat_id，则这里只允许精确匹配。
        state
            .attach_control_card(
                ticket_id,
                &card.operator_open_id,
                Some(&card.chat_id),
                &card.message_id,
            )
            .map_err(anyhow::Error::from)?;
        if mode == BridgeMode::Mirror {
            return state
                .begin_choice(
                    ticket_id,
                    choice_id,
                    &card.operator_open_id,
                    &card.message_id,
                    mode,
                    &binding_id,
                    now_unix_ms(),
                )
                .map_err(anyhow::Error::from);
        }
        if matches!(current.status, TicketStatus::Activating { .. }) {
            return state
                .begin_handoff(
                    ticket_id,
                    choice_id,
                    &card.operator_open_id,
                    &card.message_id,
                    &binding_id,
                    now_unix_ms(),
                    &HandoffBeginProof {
                        session: choice.session.clone(),
                        generation: state
                            .session(&choice.session)
                            .map(|record| record.generation)
                            .unwrap_or_default(),
                        registry_revision: choice.registry_revision,
                        process: expected_process.clone().unwrap_or_else(|| {
                            summon_core::bridge::ProcessFingerprint {
                                pid: 0,
                                start_time: String::new(),
                                executable: String::new(),
                            }
                        }),
                    },
                )
                .map_err(anyhow::Error::from);
        }
        let record = state.session(&choice.session).cloned().ok_or_else(|| {
            anyhow::Error::new(summon_core::bridge::BridgeError::SessionUnavailable)
        })?;
        let process = record.process.clone().ok_or_else(|| {
            anyhow::Error::new(summon_core::bridge::BridgeError::HandoffIneligible)
        })?;
        let proof = HandoffBeginProof {
            session: record.key.clone(),
            generation: record.generation,
            registry_revision: record.locator.revision,
            process: process.clone(),
        };
        let outcome = state
            .begin_handoff(
                ticket_id,
                choice_id,
                &card.operator_open_id,
                &card.message_id,
                &binding_id,
                now_unix_ms(),
                &proof,
            )
            .map_err(anyhow::Error::from)?;
        if matches!(outcome, BeginChoiceOutcome::Started(_)) {
            let binding = match &outcome {
                BeginChoiceOutcome::Started(binding) => binding,
                BeginChoiceOutcome::AlreadyStarted(_) => unreachable!(),
            };
            let identity_revision =
                crate::bridge::handoff::locator_identity_revision(&record.locator, &process);
            let profile = record.locator.profile.clone();
            let source = crate::bridge::handoff::HandoffSourceSnapshot::new(
                record.key,
                record.generation,
                record.locator,
                process,
                profile,
                identity_revision,
            )?;
            let attempt = crate::bridge::handoff::HandoffAttempt::new(
                handoff_attempt_id.clone(),
                ticket_id.to_string(),
                binding.id.clone(),
                binding.revision,
                binding.target.clone(),
                ticket.target_chat_type.clone(),
                target_tmux.clone(),
                ticket.target_provider,
                source,
                now_unix_ms(),
            )?;
            store.persist_handoff_attempt(&attempt)?;
        }
        Ok(outcome)
    });
    match result {
        Ok(BeginChoiceOutcome::Started(_)) | Ok(BeginChoiceOutcome::AlreadyStarted(_)) => {
            let status = summon_core::feishu::card_markdown_v2(if mode == BridgeMode::Mirror {
                summon_i18n::text!("re58219f18a")
            } else {
                summon_i18n::text!("r8d9210c553")
            });
            let _ = update_card_archived(
                fc,
                &agent.state_dir(),
                "p2p",
                &card.chat_id,
                &card.message_id,
                &status,
            );
            CallbackDisposition::Terminal
        }
        Err(error) => {
            log(&summon_i18n::format!("rbe6923612f", agent.name, ticket_id));
            let domain_error = error.downcast_ref::<summon_core::bridge::BridgeError>();
            let unrecoverable_pending = domain_error.is_some_and(|error| {
                matches!(
                    error,
                    summon_core::bridge::BridgeError::StaleSession
                        | summon_core::bridge::BridgeError::SessionUnavailable
                )
            });
            let disposition = if unrecoverable_pending {
                match cancel_ticket_durably(store, ticket_id) {
                    Ok(()) => CallbackDisposition::Terminal,
                    Err(cancel_error) => {
                        log(&summon_i18n::format!("r35e2a25255", agent.name, ticket_id));
                        CallbackDisposition::Retry
                    }
                }
            } else if domain_error.is_some() || is_terminal_callback_error(&error) {
                CallbackDisposition::Terminal
            } else {
                // state flock/read/write 或占用证据 I/O 失败时动作并未 durable，必须保留 WAL。
                CallbackDisposition::Retry
            };
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                if disposition == CallbackDisposition::Retry {
                    summon_i18n::text!("r6c78c73013")
                } else if unrecoverable_pending {
                    summon_i18n::text!("r763def38cb")
                } else {
                    summon_i18n::text!("r3f97cf4366")
                },
            );
            disposition
        }
    }
}

#[derive(Clone)]
struct BoundMirrorAction {
    binding: summon_core::bridge::ChatBinding,
    transcript_path: String,
    config_dir: String,
    locator_revision: u64,
    generation: u64,
}

fn bound_mirror_action(
    state: &BridgeState,
    scope: &str,
    agent_name: &str,
    ticket_id: &str,
    choice_id: &str,
    card: &CardActionMsg,
) -> Result<BoundMirrorAction> {
    let ticket = state
        .tickets
        .get(ticket_id)
        .context(summon_i18n::text!("rb59e2659bc"))?;
    anyhow::ensure!(
        ticket.target.scope == scope
            && ticket.target.agent == agent_name
            && ticket.requester_open_id == card.operator_open_id
            && ticket.private_chat_id.as_deref() == Some(card.chat_id.as_str())
            && ticket.control_card_message_id.as_deref() == Some(card.message_id.as_str())
            && matches!(ticket.target_chat_type.as_str(), "group" | "topic_group")
            && matches!(
                &ticket.status,
                TicketStatus::Committed {
                    binding_id: _,
                    choice_id: current_choice,
                    mode: BridgeMode::Mirror,
                } if current_choice == choice_id
            ),
        summon_i18n::format!("r104af45e25")
    );
    let binding = match state.slot(&ticket.target) {
        Some(ChatSlot::MirrorBound(binding)) if binding.mode == BridgeMode::Mirror => {
            binding.clone()
        }
        _ => anyhow::bail!(summon_i18n::format!("r70725b43e7")),
    };
    let status_binding = match &ticket.status {
        TicketStatus::Committed { binding_id, .. } => binding_id,
        _ => unreachable!("{}", summon_i18n::format!("rf9376ec31a")),
    };
    anyhow::ensure!(
        binding.id == *status_binding,
        summon_i18n::format!("r212fc91c0e")
    );
    let record = state
        .session(&binding.session)
        .context(summon_i18n::text!("r7aa66d61dd"))?;
    match state.session_leases.get(&binding.session.storage_key()) {
        Some(lease) if lease.binding_id == binding.id && lease.target == binding.target => {}
        _ => anyhow::bail!(summon_i18n::format!("rb948ca7a6c")),
    }
    Ok(BoundMirrorAction {
        binding,
        transcript_path: record.locator.transcript_path.clone(),
        config_dir: record.locator.profile.config_dir.clone(),
        locator_revision: record.locator.revision,
        generation: record.generation,
    })
}

fn active_verified_bound_mirror_action(
    state: &BridgeState,
    scope: &str,
    agent_name: &str,
    ticket_id: &str,
    choice_id: &str,
    card: &CardActionMsg,
) -> Result<BoundMirrorAction> {
    let action = bound_mirror_action(state, scope, agent_name, ticket_id, choice_id, card)?;
    let record = state
        .session(&action.binding.session)
        .context(summon_i18n::text!("r7aa66d61dd"))?;
    anyhow::ensure!(
        matches!(
            record.lifecycle,
            summon_core::bridge::ExternalLifecycle::ActiveIdle
                | summon_core::bridge::ExternalLifecycle::ActiveBusy
        ) && record.locator.profile.process_verified
            && record.process.as_ref().is_some_and(|process| {
                process.is_complete() && process.executable == record.locator.profile.claude_bin
            }),
        summon_i18n::format!("re99fac3775")
    );
    Ok(action)
}

fn handle_bridge_stop_action(
    agent: &Agent,
    fc: &FeishuClient,
    storage_scope: &FeishuStorageScope,
    card: &CardActionMsg,
    ticket_id: &str,
    choice_id: &str,
    store: &crate::bridge::store::BridgeStore,
) -> CallbackDisposition {
    let state = match store.load_state() {
        Ok(Some(state)) => state,
        Ok(None) => {
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("rbf60f6f0e5"),
            );
            return CallbackDisposition::Terminal;
        }
        Err(error) => {
            log(&summon_i18n::format!("rac806f4eda", agent.name, ticket_id));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("re2e80a22af"),
            );
            return CallbackDisposition::Retry;
        }
    };
    let target = match bound_mirror_action(
        &state,
        &storage_scope.key,
        &agent.name,
        ticket_id,
        choice_id,
        card,
    ) {
        Ok(action) => action.binding.target,
        Err(error) => {
            log(&summon_i18n::format!("rb9a94f94f9", agent.name, ticket_id));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("rbf60f6f0e5"),
            );
            return CallbackDisposition::Terminal;
        }
    };
    let result = crate::bridge::runtime::with_chat_delivery_gate(&target, || {
        store.with_state(|state| {
            let action = bound_mirror_action(
                state,
                &storage_scope.key,
                &agent.name,
                ticket_id,
                choice_id,
                card,
            )
            .map_err(terminal_callback_error)?;
            // 领域 stop 只清 slot/lease/ticket/receipts；本路径不持有 Session/tmux/signal。
            state
                .stop_mirror(
                    &action.binding.target,
                    &action.binding.id,
                    action.binding.revision,
                )
                .map_err(terminal_callback_error)?;
            Ok(())
        })
    });
    match result {
        Ok(()) => {
            let stopped = summon_core::feishu::card_markdown_v2(summon_i18n::text!("raea04af67c"));
            if let Err(error) = update_card_archived(
                fc,
                &agent.state_dir(),
                "p2p",
                &card.chat_id,
                &card.message_id,
                &stopped,
            ) {
                log(&summon_i18n::format!("r7605aa911c", agent.name));
            }
            CallbackDisposition::Terminal
        }
        Err(error) => {
            log(&summon_i18n::format!("ra44980b05c", agent.name, ticket_id));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                if is_terminal_callback_error(&error) {
                    summon_i18n::text!("rbf60f6f0e5")
                } else {
                    summon_i18n::text!("re2e80a22af")
                },
            );
            if is_terminal_callback_error(&error) {
                CallbackDisposition::Terminal
            } else {
                CallbackDisposition::Retry
            }
        }
    }
}

fn handle_bridge_context_action(
    agent: &Agent,
    fc: &FeishuClient,
    storage_scope: &FeishuStorageScope,
    card: &CardActionMsg,
    bridge_store: &crate::bridge::store::BridgeStore,
    outbox: &crate::runtime::outbox_store::OutboxStore,
) -> CallbackDisposition {
    use summon_core::bridge::ContextExportReceipt;
    use summon_core::outbox::{
        bridge_delivery_id, BridgeDeliveryFence, BridgeDeliveryKind, OutboxEvent, OutboxKind,
    };
    let Some(("bridge_context", ticket_id, choice_id)) = valid_bridge_value(card) else {
        return CallbackDisposition::Terminal;
    };
    let Some(receipt_id) = context_callback_receipt_id(card, ticket_id, choice_id) else {
        log(&summon_i18n::format!("r0224a92447", agent.name));
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            summon_i18n::text!("ra96f2da99f"),
        );
        return CallbackDisposition::Terminal;
    };
    let outbox_id = bridge_delivery_id(BridgeDeliveryKind::ContextCapsule, &receipt_id);

    // 先归约 Hook 并导出更早 turn，再拍 locator/transcript 快照。若在读 transcript 之后才
    // reconcile，reconciler 自己推进 locator revision 会把这次合法 callback 判成陈旧请求。
    if let Err(error) = crate::bridge::drain::drain_tick(bridge_store).and_then(|_| {
        crate::bridge::reconcile::reconcile_once(bridge_store, outbox, now_unix_ms()).map(|_| ())
    }) {
        log(&summon_i18n::format!("r6ad74ae2c0", agent.name));
        send_bridge_reply(
            fc,
            agent,
            "p2p",
            &card.chat_id,
            summon_i18n::text!("r9a63572415"),
        );
        return CallbackDisposition::Retry;
    }
    let state = match bridge_store.load_state() {
        Ok(Some(state)) => state,
        Ok(None) => {
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("r2ceb57e123"),
            );
            return CallbackDisposition::Terminal;
        }
        Err(error) => {
            log(&summon_i18n::format!("rb1ad9fad96", agent.name));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("r9a63572415"),
            );
            return CallbackDisposition::Retry;
        }
    };
    let snapshot = match active_verified_bound_mirror_action(
        &state,
        &storage_scope.key,
        &agent.name,
        ticket_id,
        choice_id,
        card,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            log(&summon_i18n::format!("r18e10a3a76", agent.name));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("r2ceb57e123"),
            );
            return CallbackDisposition::Terminal;
        }
    };
    match outbox.find(&outbox_id) {
        Ok(Some((crate::runtime::outbox_store::OutboxLocation::Dead, _))) => {
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("r6f7918d428"),
            );
            return CallbackDisposition::Terminal;
        }
        Ok(Some(_)) => {
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("r8beece9d9d"),
            );
            return CallbackDisposition::Terminal;
        }
        Ok(None) => {}
        Err(error) => {
            log(&summon_i18n::format!("r1648451bc8", agent.name));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("r9a63572415"),
            );
            return CallbackDisposition::Retry;
        }
    }
    let capsule = match crate::bridge::context::read_bounded_context(
        std::path::Path::new(&snapshot.transcript_path),
        std::path::Path::new(&snapshot.config_dir),
        &snapshot.binding.session.session_id,
    ) {
        Ok(capsule) if capsule.messages.is_empty() => {
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("r698494d7a7"),
            );
            return CallbackDisposition::Terminal;
        }
        Ok(capsule) => capsule,
        Err(error) => {
            log(&summon_i18n::format!("rcbba54a733", agent.name));
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                summon_i18n::text!("rcd433b5606"),
            );
            return CallbackDisposition::Terminal;
        }
    };
    let rendered_card = summon_core::feishu::card_bridge_context_capsule(&capsule);

    let now = now_unix_ms();
    let mut outbox_durable = false;
    let durable = bridge_store.with_state(|state| {
        let current = active_verified_bound_mirror_action(
            state,
            &storage_scope.key,
            &agent.name,
            ticket_id,
            choice_id,
            card,
        )
        .map_err(terminal_callback_error)?;
        if current.binding != snapshot.binding
            || current.generation != snapshot.generation
            || current.locator_revision != snapshot.locator_revision
            || current.transcript_path != snapshot.transcript_path
        {
            return Err(terminal_callback_error(summon_i18n::text!("r241373fc4b")));
        }
        if let Some(existing) = state.context_export_receipts.get(&receipt_id) {
            if existing.outbox_id != outbox_id
                || existing.ticket_id != ticket_id
                || existing.choice_id != choice_id
                || existing.requester_open_id != card.operator_open_id
                || existing.binding_id != current.binding.id
                || existing.binding_revision != current.binding.revision
            {
                return Err(terminal_callback_error(summon_i18n::text!("rd1c7feb37b")));
            }
            return Ok(false);
        }
        let sequence = state
            .allocate_context_delivery_sequence(&current.binding.id, current.binding.revision)
            .map_err(terminal_callback_error)?;
        let fence = BridgeDeliveryFence {
            scope: current.binding.target.scope.clone(),
            agent: current.binding.target.agent.clone(),
            chat_id: current.binding.target.chat_id.clone(),
            chat_type: current.binding.target_chat_type.clone(),
            binding_id: current.binding.id.clone(),
            binding_revision: current.binding.revision,
            session: current.binding.session.clone(),
            generation: current.generation,
            lease_binding_id: current.binding.id.clone(),
            receipt_id: receipt_id.clone(),
            sequence,
        };
        let event = OutboxEvent {
            id: outbox_id.clone(),
            agent: agent.name.clone(),
            delivery_scope: current.binding.target.scope.clone(),
            created_ms: now,
            attempts: 0,
            next_attempt_ms: now,
            last_error: None,
            kind: OutboxKind::BridgeContextCapsule {
                fence: fence.clone(),
                ticket_id: ticket_id.to_string(),
                choice_id: choice_id.to_string(),
                requester_open_id: card.operator_open_id.clone(),
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card,
            },
        };
        let receipt = ContextExportReceipt {
            id: receipt_id.clone(),
            outbox_id: outbox_id.clone(),
            ticket_id: ticket_id.to_string(),
            choice_id: choice_id.to_string(),
            requester_open_id: card.operator_open_id.clone(),
            binding_id: fence.binding_id.clone(),
            binding_revision: fence.binding_revision,
            target: current.binding.target,
            target_chat_type: fence.chat_type.clone(),
            session: fence.session,
            generation: fence.generation,
            sequence: fence.sequence,
            delivered_at_ms: None,
        };
        outbox.enqueue_once(&event)?;
        // 从此刻起 callback 的唯一 payload 已由确定性 outbox id 持久化；即使随后 state
        // receipt 写失败，冷启 reconcile/同 callback 重放也只会接续这一条事件。
        outbox_durable = true;
        state
            .record_context_export(receipt)
            .map_err(terminal_callback_error)?;
        Ok(true)
    });
    match durable {
        Ok(enqueued) => {
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                if enqueued {
                    summon_i18n::text!("r822a7363ae")
                } else {
                    summon_i18n::text!("r8beece9d9d")
                },
            );
            CallbackDisposition::Terminal
        }
        Err(error) => {
            log(&summon_i18n::format!("r1b22cd412c", agent.name));
            let disposition = if outbox_durable || is_terminal_callback_error(&error) {
                CallbackDisposition::Terminal
            } else {
                CallbackDisposition::Retry
            };
            send_bridge_reply(
                fc,
                agent,
                "p2p",
                &card.chat_id,
                if disposition == CallbackDisposition::Terminal {
                    summon_i18n::text!("rff4cb1d19a")
                } else {
                    summon_i18n::text!("r9a63572415")
                },
            );
            disposition
        }
    }
}

pub(super) fn handle_bridge_chat_gone_with_store(
    storage_scope: &FeishuStorageScope,
    agent: &Agent,
    chat_id: &str,
    reason: summon_core::exit::ExitReason,
    store: &crate::bridge::store::BridgeStore,
) -> Result<BridgeChatGoneAction> {
    let close_reason = match reason {
        summon_core::exit::ExitReason::GroupDisbanded => BridgeCloseReason::ChatDisbanded,
        summon_core::exit::ExitReason::BotRemoved => BridgeCloseReason::BotRemoved,
        _ => return Ok(BridgeChatGoneAction::None),
    };
    let target = ScopedChatKey::new(&storage_scope.key, &agent.name, chat_id);
    crate::bridge::runtime::with_chat_delivery_gate(&target, || {
        store.with_state(|state| {
            state
                .chat_gone(&target, close_reason)
                .map_err(anyhow::Error::from)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::resume_store::ResumePresence;

    fn empty_evidence() -> LocalChatEvidence {
        LocalChatEvidence {
            live_tmux: false,
            resume: ResumePresence::Missing,
            legacy_resume: ResumePresence::Missing,
            pending_turn_file: false,
            inbox_pending: false,
            inbox_recovery_pending: false,
            local_outbox_pending: false,
            inflight: false,
            queued_local_work: false,
            remote_bound: false,
        }
    }

    fn remote_binding(chat_id: &str, ended: Option<&str>) -> summon_core::remote::RemoteBinding {
        summon_core::remote::RemoteBinding {
            schema_version: summon_core::remote::REMOTE_BINDING_SCHEMA_VERSION,
            agent: "a".into(),
            scope: "a-s".into(),
            chat_id: chat_id.into(),
            chat_type: "group".into(),
            session_id: "sid-remote".into(),
            session_name: "n".into(),
            surface_id: "S".into(),
            workspace_id: "W".into(),
            pid: 42,
            pid_start_seconds: 1000,
            transcript_path: "/t.jsonl".into(),
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            ended: ended.map(str::to_string),
        }
    }

    fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !predicate() {
            assert!(std::time::Instant::now() < deadline, "等待超时:{label}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    struct BoundCallbackFixture {
        agent: Agent,
        _agent_dir: tempfile::TempDir,
        root: tempfile::TempDir,
        scope: FeishuStorageScope,
        store: crate::bridge::store::BridgeStore,
        outbox: crate::runtime::outbox_store::OutboxStore,
        card: CardActionMsg,
        target: ScopedChatKey,
    }

    fn bound_callback_fixture(action: &str, event_id: &str) -> BoundCallbackFixture {
        use summon_core::bridge::{
            BridgeChoice, ExternalLifecycle, ExternalLocator, ExternalSessionRecord,
            ProcessFingerprint, ResumeProfile, SessionKey, SessionLease,
        };

        let (agent, agent_dir) =
            crate::test_support::agent_dir("claude", "feishu:\n  operators: [ou_owner]\n");
        let scope = feishu_storage_scope(&agent);
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let session_id = "sid-context";
        let transcript = config_dir.join(format!("{session_id}.jsonl"));
        std::fs::write(
            &transcript,
            concat!(
                "{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",",
                "\"message\":{\"role\":\"user\",\"content\":\"callback context\"}}\n",
                "{\"type\":\"assistant\",\"isSidechain\":false,",
                "\"message\":{\"role\":\"assistant\",\"stop_reason\":\"end_turn\",",
                "\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n"
            ),
        )
        .unwrap();
        let session = SessionKey::new(config_dir.to_string_lossy(), session_id);
        let target = ScopedChatKey::new(&scope.key, &agent.name, "oc_group");
        let binding = summon_core::bridge::ChatBinding {
            id: "binding-callback".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: session.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            revision: 7,
        };
        let process = ProcessFingerprint {
            pid: 42,
            start_time: "start-42".into(),
            executable: "/opt/claude".into(),
        };
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        store
            .with_state(|state| {
                state.sessions.insert(
                    session.storage_key(),
                    ExternalSessionRecord {
                        key: session.clone(),
                        generation: 3,
                        locator: ExternalLocator {
                            key: session.clone(),
                            transcript_path: transcript.to_string_lossy().into_owned(),
                            resume_lookup_cwd: root.path().to_string_lossy().into_owned(),
                            current_cwd: root.path().to_string_lossy().into_owned(),
                            profile: ResumeProfile {
                                claude_bin: process.executable.clone(),
                                config_dir: config_dir.to_string_lossy().into_owned(),
                                process_verified: true,
                                ..ResumeProfile::default()
                            },
                            revision: 9,
                        },
                        process: Some(process),
                        lifecycle: ExternalLifecycle::ActiveIdle,
                        first_seen_ms: 1,
                        last_seen_ms: 2,
                        last_applied_sequence: 2,
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
                state
                    .delivery_sequence_cursors
                    .insert(binding.id.clone(), 1);
                state.tickets.insert(
                    "ticket-callback".into(),
                    BridgeTicket {
                        id: "ticket-callback".into(),
                        short_code: None,
                        target: target.clone(),
                        target_chat_type: "group".into(),
                        target_provider: summon_core::bridge::ClaudeProvider::Default,
                        requester_open_id: "ou_owner".into(),
                        requester_union_id: String::new(),
                        control_card_message_id: Some("om_control".into()),
                        private_chat_id: Some("oc_private".into()),
                        slot_revision: 1,
                        choices: vec![BridgeChoice {
                            id: "choice-callback".into(),
                            session: session.clone(),
                            registry_revision: 9,
                        }],
                        issued_at_ms: 1,
                        expires_at_ms: u64::MAX,
                        status: TicketStatus::Committed {
                            binding_id: binding.id,
                            choice_id: "choice-callback".into(),
                            mode: BridgeMode::Mirror,
                        },
                    },
                );
                Ok(())
            })
            .unwrap();
        let card = CardActionMsg {
            chat_id: "oc_private".into(),
            message_id: "om_control".into(),
            operator_open_id: "ou_owner".into(),
            event_id: event_id.into(),
            value: serde_json::json!({
                "action": action,
                "ticket": "ticket-callback",
                "choice": "choice-callback",
            }),
            wal_id: None,
            wal_replayed: false,
        };
        let outbox = crate::runtime::outbox_store::OutboxStore::new(root.path().join("outbox"));
        BoundCallbackFixture {
            agent,
            _agent_dir: agent_dir,
            root,
            scope,
            store,
            outbox,
            card,
            target,
        }
    }

    fn invoke_bridge_callback(fixture: &BoundCallbackFixture) -> CallbackDisposition {
        handle_bridge_card_action_with_stores(
            &fixture.agent,
            &FeishuClient::new("feishu.cn", String::new(), String::new(), true),
            &fixture.scope,
            &HashMap::new(),
            &crate::runtime::inbox_wal::InboxWal::new(fixture.root.path().join("inbox")),
            &crate::runtime::inflight::InFlight::new(),
            &fixture.card,
            &fixture.store,
            &fixture.outbox,
        )
    }

    fn persist_callback(
        wal: &crate::runtime::card_action_wal::CardActionWal,
        card: &CardActionMsg,
    ) -> CardActionMsg {
        match wal.persist("app-callback", card).unwrap() {
            crate::runtime::card_action_wal::PersistOutcome::Pending { record, .. } => record.card,
            crate::runtime::card_action_wal::PersistOutcome::Consumed => {
                panic!("新 callback 不应预先 consumed")
            }
        }
    }

    fn finish_callback(
        card: &CardActionMsg,
        wal: &crate::runtime::card_action_wal::CardActionWal,
        disposition: CallbackDisposition,
    ) {
        super::super::dispatcher::finish_durable_card_callback(card, wal, disposition);
    }

    fn durable_recall_record(
        root: &std::path::Path,
        token: &str,
        issued_at_ms: u64,
    ) -> crate::runtime::card_action_wal::CardActionRecord {
        let wal = crate::runtime::card_action_wal::CardActionWal::new(root.join("card-wal"));
        let card = CardActionMsg {
            chat_id: "oc_private".into(),
            message_id: "om_recall".into(),
            operator_open_id: "ou_owner".into(),
            event_id: format!("event-{token}"),
            value: serde_json::json!({
                "action": "recall_rpc_handoff",
                "cli": "codex",
                "sid": "th-1",
                "chat_id": "oc_target",
                "chat_type": "group",
                "bound_agent": "blue",
                "bound_scope": "scope-blue",
                "bound_requester": "ou_owner",
                "target_lifecycle_token": token,
                "card_issued_at_ms": issued_at_ms,
                "source_observation": "source-ref",
                "observed_at_ms": issued_at_ms,
            }),
            wal_id: None,
            wal_replayed: false,
        };
        match wal.persist("app-blue", &card).unwrap() {
            crate::runtime::card_action_wal::PersistOutcome::Pending { record, .. } => *record,
            crate::runtime::card_action_wal::PersistOutcome::Consumed => unreachable!(),
        }
    }

    #[test]
    fn boot_rotation_preserves_only_an_exact_unexpired_durable_recall_callback() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join(".DS_Store"), b"finder metadata").unwrap();
        let stray_chats = agents.join("not-an-agent/runtime/chats");
        std::fs::create_dir_all(&stray_chats).unwrap();
        std::fs::write(stray_chats.join("g-stray.recall-target"), "must-not-read\n").unwrap();
        let chats = agents.join("alias-blue/runtime/chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(agents.join("alias-blue/agent.yaml"), "name: blue\n").unwrap();
        let keep_path = chats.join(format!("{}.recall-target", chat_tag("group", "oc_target")));
        let rotate_path = chats.join("g-unused.recall-target");
        std::fs::write(&keep_path, "token-current\n").unwrap();
        std::fs::write(&rotate_path, "token-unused\n").unwrap();
        let record = durable_recall_record(root.path(), "token-current", 1_000);

        let protected = protected_recall_lifecycles(&[record], 1_000);
        let (rotated, preserved) =
            invalidate_recall_target_lifecycles_at(&agents, &protected).unwrap();

        assert_eq!((rotated, preserved), (1, 1));
        assert_eq!(
            std::fs::read_to_string(keep_path).unwrap(),
            "token-current\n"
        );
        assert_ne!(
            std::fs::read_to_string(rotate_path).unwrap(),
            "token-unused\n"
        );
        assert_eq!(
            std::fs::read_to_string(stray_chats.join("g-stray.recall-target")).unwrap(),
            "must-not-read\n",
            "没有 agent.yaml 的目录不属于 registry，boot 不得读取或改写"
        );
    }

    #[test]
    fn boot_rotation_never_preserves_expired_or_already_superseded_callbacks() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("agents");
        let chats = agents.join("alias-blue/runtime/chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(agents.join("alias-blue/agent.yaml"), "name: blue\n").unwrap();
        let token_path = chats.join(format!("{}.recall-target", chat_tag("group", "oc_target")));
        std::fs::write(&token_path, "newer-token\n").unwrap();
        let record = durable_recall_record(root.path(), "old-token", 1_000);

        let expired = protected_recall_lifecycles(
            std::slice::from_ref(&record),
            1_000 + RPC_HANDOFF_CARD_TTL_MS + 1,
        );
        assert!(expired.is_empty());
        let still_fresh = protected_recall_lifecycles(&[record], 1_000);
        let (rotated, preserved) =
            invalidate_recall_target_lifecycles_at(&agents, &still_fresh).unwrap();

        assert_eq!((rotated, preserved), (1, 0));
        assert_ne!(
            std::fs::read_to_string(token_path).unwrap(),
            "newer-token\n"
        );
    }

    #[test]
    fn corrupt_bridge_state_retries_and_keeps_callback_wal() {
        let fixture = bound_callback_fixture("bridge_stop", "event-corrupt-state");
        let wal = crate::runtime::card_action_wal::CardActionWal::new(
            fixture.root.path().join("card-wal"),
        );
        let durable_card = persist_callback(&wal, &fixture.card);
        std::fs::write(fixture.store.state_path(), b"not-json").unwrap();

        let disposition = invoke_bridge_callback(&fixture);
        assert_eq!(disposition, CallbackDisposition::Retry);
        finish_callback(&durable_card, &wal, disposition);
        let health = wal.inspect_health().unwrap();
        assert_eq!(health.pending, 1);
        assert_eq!(health.consumed, 0);
    }

    #[test]
    fn stop_write_failure_replays_once_after_repair_and_then_acks() {
        let fixture = bound_callback_fixture("bridge_stop", "event-stop-retry");
        let wal = crate::runtime::card_action_wal::CardActionWal::new(
            fixture.root.path().join("card-wal"),
        );
        let durable_card = persist_callback(&wal, &fixture.card);
        let lock_path = fixture.root.path().join("bridge").join(".state.lock");
        std::fs::remove_file(&lock_path).unwrap();
        std::fs::create_dir(&lock_path).unwrap();

        let first = invoke_bridge_callback(&fixture);
        assert_eq!(first, CallbackDisposition::Retry);
        finish_callback(&durable_card, &wal, first);
        assert_eq!(wal.inspect_health().unwrap().pending, 1);
        assert!(fixture
            .store
            .load_state()
            .unwrap()
            .unwrap()
            .slot(&fixture.target)
            .is_some());

        std::fs::remove_dir(&lock_path).unwrap();
        let replay = invoke_bridge_callback(&fixture);
        assert_eq!(replay, CallbackDisposition::Terminal);
        finish_callback(&durable_card, &wal, replay);
        let stopped = fixture.store.load_state().unwrap().unwrap();
        assert!(stopped.slot(&fixture.target).is_none());
        let stopped_revision = stopped.revision;
        let health = wal.inspect_health().unwrap();
        assert_eq!(health.pending, 0);
        assert_eq!(health.consumed, 1);

        assert_eq!(
            invoke_bridge_callback(&fixture),
            CallbackDisposition::Terminal
        );
        assert_eq!(
            fixture.store.load_state().unwrap().unwrap().revision,
            stopped_revision,
            "终态 callback 重放不得再次修改 Bridge state"
        );
    }

    #[test]
    fn context_outbox_failure_retries_then_replay_enqueues_exactly_once() {
        let fixture = bound_callback_fixture("bridge_context", "event-context-retry");
        let wal = crate::runtime::card_action_wal::CardActionWal::new(
            fixture.root.path().join("card-wal"),
        );
        let durable_card = persist_callback(&wal, &fixture.card);
        let outbox_root = fixture.root.path().join("outbox");
        std::fs::create_dir_all(&outbox_root).unwrap();
        let pending_path = outbox_root.join("pending");
        std::fs::write(&pending_path, b"not-a-directory").unwrap();

        let first = invoke_bridge_callback(&fixture);
        assert_eq!(first, CallbackDisposition::Retry);
        finish_callback(&durable_card, &wal, first);
        assert_eq!(wal.inspect_health().unwrap().pending, 1);

        std::fs::remove_file(&pending_path).unwrap();
        let replay = invoke_bridge_callback(&fixture);
        assert_eq!(replay, CallbackDisposition::Terminal);
        finish_callback(&durable_card, &wal, replay);
        let receipt_id =
            context_callback_receipt_id(&fixture.card, "ticket-callback", "choice-callback")
                .unwrap();
        let outbox_id = summon_core::outbox::bridge_delivery_id(
            summon_core::outbox::BridgeDeliveryKind::ContextCapsule,
            &receipt_id,
        );
        assert!(fixture.outbox.find(&outbox_id).unwrap().is_some());
        assert_eq!(
            fixture
                .store
                .load_state()
                .unwrap()
                .unwrap()
                .context_export_receipts
                .len(),
            1
        );

        assert_eq!(
            invoke_bridge_callback(&fixture),
            CallbackDisposition::Terminal
        );
        assert_eq!(
            fixture
                .store
                .load_state()
                .unwrap()
                .unwrap()
                .context_export_receipts
                .len(),
            1,
            "同一 callback 重放不得产生第二条 context receipt"
        );
        let health = wal.inspect_health().unwrap();
        assert_eq!(health.pending, 0);
        assert_eq!(health.consumed, 1);
    }

    #[test]
    fn invalid_and_unauthorized_bridge_callbacks_are_terminal_and_ackable() {
        let fixture = bound_callback_fixture("bridge_stop", "event-unauthorized");
        let wal = crate::runtime::card_action_wal::CardActionWal::new(
            fixture.root.path().join("card-wal"),
        );
        let mut unauthorized = fixture.card.clone();
        unauthorized.operator_open_id = "ou_intruder".into();
        let durable_unauthorized = persist_callback(&wal, &unauthorized);
        let unauthorized_disposition = handle_bridge_card_action_with_stores(
            &fixture.agent,
            &FeishuClient::new("feishu.cn", String::new(), String::new(), true),
            &fixture.scope,
            &HashMap::new(),
            &crate::runtime::inbox_wal::InboxWal::new(fixture.root.path().join("inbox")),
            &crate::runtime::inflight::InFlight::new(),
            &unauthorized,
            &fixture.store,
            &fixture.outbox,
        );
        assert_eq!(unauthorized_disposition, CallbackDisposition::Terminal);
        finish_callback(&durable_unauthorized, &wal, unauthorized_disposition);

        let mut invalid = fixture.card.clone();
        invalid.event_id = "event-invalid".into();
        invalid.value.as_object_mut().unwrap().remove("choice");
        let durable_invalid = persist_callback(&wal, &invalid);
        let invalid_disposition = handle_bridge_card_action_with_stores(
            &fixture.agent,
            &FeishuClient::new("feishu.cn", String::new(), String::new(), true),
            &fixture.scope,
            &HashMap::new(),
            &crate::runtime::inbox_wal::InboxWal::new(fixture.root.path().join("inbox")),
            &crate::runtime::inflight::InFlight::new(),
            &invalid,
            &fixture.store,
            &fixture.outbox,
        );
        assert_eq!(invalid_disposition, CallbackDisposition::Terminal);
        finish_callback(&durable_invalid, &wal, invalid_disposition);

        let health = wal.inspect_health().unwrap();
        assert_eq!(health.pending, 0);
        assert_eq!(health.consumed, 2);
        assert!(fixture
            .store
            .load_state()
            .unwrap()
            .unwrap()
            .slot(&fixture.target)
            .is_some());
    }

    #[test]
    fn unauthorized_recall_resume_never_reaches_kill_or_resume() {
        // ★ 红线回归:resume/绑群/杀源的唯一入口是 ResumeGroup/ResumeLocal 臂。未过 operate-gate
        // (authorized=false)的 recall_resume 必须落 Reject,绝不进破坏性臂(见 bd6dd92)。
        let resume = serde_json::json!({"action": "recall_resume", "sid": "sid-x"});
        assert!(
            matches!(
                plan_recall_callback(
                    &resume,
                    false,
                    Some(summon_core::recall::RecallProvider::Claude),
                ),
                RecallCallbackPlan::Reject(_)
            ),
            "未授权 recall_resume 必须被拒,不得起会话/绑群/杀源"
        );
        // 过 gate 且无原群 → ResumeLocal(本地兜底),sid 原样带出。
        assert!(matches!(
            plan_recall_callback(
                &resume,
                true,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::ResumeLocal {
                provider: summon_core::recall::RecallProvider::Claude,
                sid: "sid-x"
            }
        ));

        // provider 是 capability 的一部分：Cursor 卡不能交给 Claude agent；旧卡缺 cli 也只按
        // Claude 解释，不能在 Cursor agent 上按“当前 CLI”猜测。
        let cursor = serde_json::json!({
            "action": "recall_resume",
            "cli": "cursor",
            "sid": "cursor-sid",
            "chat_id": "oc_g",
            "chat_type": "group"
        });
        assert!(matches!(
            plan_recall_callback(
                &cursor,
                true,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::Reject(_)
        ));
        assert!(matches!(
            plan_recall_callback(
                &resume,
                true,
                Some(summon_core::recall::RecallProvider::Cursor),
            ),
            RecallCallbackPlan::Reject(_)
        ));
        assert!(matches!(
            plan_recall_callback(
                &cursor,
                true,
                Some(summon_core::recall::RecallProvider::Cursor),
            ),
            RecallCallbackPlan::ResumeGroup {
                provider: summon_core::recall::RecallProvider::Cursor,
                sid: "cursor-sid",
                ..
            }
        ));
        // 过 gate 且带原群 → ResumeGroup,sid/chat_id/chat_type 原样带出。
        let resume_group = serde_json::json!({
            "action": "recall_resume", "sid": "sid-x",
            "chat_id": "oc_g", "chat_type": "group"
        });
        assert!(matches!(
            plan_recall_callback(
                &resume_group,
                true,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::ResumeGroup {
                provider: summon_core::recall::RecallProvider::Claude,
                sid: "sid-x",
                chat_id: "oc_g",
                chat_type: "group",
                source_observation_id: None,
            }
        ));
        let resume_active = serde_json::json!({
            "action": "recall_resume", "sid": "sid-active",
            "chat_id": "oc_g", "chat_type": "group",
            "source_observation": "00000000-0000-4000-8000-000000000001"
        });
        assert!(matches!(
            plan_recall_callback(
                &resume_active,
                true,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::ResumeGroup {
                provider: summon_core::recall::RecallProvider::Claude,
                sid: "sid-active",
                chat_id: "oc_g",
                chat_type: "group",
                source_observation_id: Some("00000000-0000-4000-8000-000000000001"),
            }
        ));
        // 带原群但未授权:gate 仍先断死(绝不因带群就放行)。
        assert!(matches!(
            plan_recall_callback(
                &resume_group,
                false,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::Reject(_)
        ));
        // 坏 chat_type(非群)→ 回落本地兜底,不当群处理。
        let resume_bad_origin = serde_json::json!({
            "action": "recall_resume", "sid": "sid-x",
            "chat_id": "oc_g", "chat_type": "p2p"
        });
        assert!(matches!(
            plan_recall_callback(
                &resume_bad_origin,
                true,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::ResumeLocal {
                provider: summon_core::recall::RecallProvider::Claude,
                sid: "sid-x"
            }
        ));
        // 缺 sid 即便授权也拒(不拿脏参数去 find_candidate)。
        let no_sid = serde_json::json!({"action": "recall_resume"});
        assert!(matches!(
            plan_recall_callback(
                &no_sid,
                true,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::Reject(_)
        ));
        // 翻页会暴露 cwd / prompt，也必须在点击时重验 operate；原群 origin 随翻页保留。
        let page = serde_json::json!({
            "action": "recall_page", "page": 3, "chat_id": "oc_g", "chat_type": "group"
        });
        assert!(matches!(
            plan_recall_callback(
                &page,
                false,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::Reject(_)
        ));
        assert!(matches!(
            plan_recall_callback(
                &page,
                true,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::Page {
                page: 3,
                origin: Some(("oc_g", "group"))
            }
        ));

        let handoff = serde_json::json!({
            "action": "recall_rpc_handoff",
            "cli": "codex",
            "sid": "thread-owned",
            "chat_id": "oc_target",
            "chat_type": "topic_group",
            "source_observation": "source-revision",
            "observed_at_ms": 42
        });
        assert!(matches!(
            plan_recall_callback(
                &handoff,
                false,
                Some(summon_core::recall::RecallProvider::Codex),
            ),
            RecallCallbackPlan::Reject(_)
        ));
        assert!(matches!(
            plan_recall_callback(
                &handoff,
                true,
                Some(summon_core::recall::RecallProvider::Codex),
            ),
            RecallCallbackPlan::RpcHandoffGroup {
                provider: summon_core::recall::RecallProvider::Codex,
                sid: "thread-owned",
                chat_id: "oc_target",
                chat_type: "topic_group",
                source_ref: "source-revision",
                observed_at_ms: 42,
            }
        ));
        let handoff_without_group = serde_json::json!({
            "action": "recall_rpc_handoff", "cli": "codex", "sid": "thread-owned",
            "source_observation": "source-revision", "observed_at_ms": 42
        });
        assert!(matches!(
            plan_recall_callback(
                &handoff_without_group,
                true,
                Some(summon_core::recall::RecallProvider::Codex),
            ),
            RecallCallbackPlan::Reject(_)
        ));
    }

    #[test]
    fn recall_cmux_remote_callback_needs_operate_and_a_target_group() {
        let claude = Some(summon_core::recall::RecallProvider::Claude);
        let remote = serde_json::json!({
            "action": "recall_cmux_remote", "cli": "claude", "sid": "s",
            "chat_id": "oc_g", "chat_type": "group"
        });
        assert!(matches!(
            plan_recall_callback(&remote, true, claude),
            RecallCallbackPlan::RemoteGroup {
                sid: "s",
                chat_id: "oc_g",
                chat_type: "group",
            }
        ));
        assert!(
            matches!(
                plan_recall_callback(&remote, false, claude),
                RecallCallbackPlan::Reject(_)
            ),
            "未过 operate-gate 的遥控必须被拒,不得落绑定"
        );
        // 遥控绑的是发起 /bridge 的群:没有群 origin 的卡不回落到本地,直接拒。
        for value in [
            serde_json::json!({"action": "recall_cmux_remote", "cli": "claude", "sid": "s"}),
            serde_json::json!({
                "action": "recall_cmux_remote", "cli": "claude", "sid": "s",
                "chat_id": "oc_g", "chat_type": "p2p"
            }),
            serde_json::json!({
                "action": "recall_cmux_remote", "cli": "claude",
                "chat_id": "oc_g", "chat_type": "group"
            }),
        ] {
            assert!(
                matches!(
                    plan_recall_callback(&value, true, claude),
                    RecallCallbackPlan::Reject(_)
                ),
                "{value}"
            );
        }
        // 只遥控 Claude 会话:别的 CLI 的卡即便与 agent 一致也拒。
        let codex = serde_json::json!({
            "action": "recall_cmux_remote", "cli": "codex", "sid": "s",
            "chat_id": "oc_g", "chat_type": "group"
        });
        assert!(matches!(
            plan_recall_callback(
                &codex,
                true,
                Some(summon_core::recall::RecallProvider::Codex)
            ),
            RecallCallbackPlan::Reject(_)
        ));
    }

    fn headless_candidate(
        provider: summon_core::recall::RecallProvider,
        sid: &str,
        cwd: &std::path::Path,
    ) -> crate::bridge::recall_headless::HeadlessRecallCandidate {
        crate::bridge::recall_headless::HeadlessRecallCandidate {
            candidate: summon_core::recall::SessionCandidate {
                provider,
                ownership: summon_core::recall::RecallOwnership::External,
                sid: sid.into(),
                cwd: cwd.to_string_lossy().into_owned(),
                name: "历史任务".into(),
                prompt_preview: String::new(),
                last_active_ms: 1,
                live_pid: None,
                live_status: None,
                source_observation_id: None,
                background_tasks: Vec::new(),
            },
            source_path: cwd.join("source"),
        }
    }

    #[test]
    fn headless_recall_binding_writes_native_resume_cwd_and_group_meta() {
        let (agent, _agent_dir) = crate::test_support::agent_dir("cursor", "");
        let workspace = tempfile::tempdir().unwrap();
        let found = headless_candidate(
            summon_core::recall::RecallProvider::Cursor,
            "cursor-native-id",
            workspace.path(),
        );
        let canonical =
            persist_headless_recall_binding(&agent, &found, "oc_cursor", "group", "ou_owner")
                .unwrap();
        let record = crate::session::resume_store::read_record(&agent, &canonical).unwrap();
        assert_eq!(record.cli, "cursor");
        assert_eq!(record.id, "cursor-native-id");
        assert_eq!(
            record.cwd.as_deref(),
            Some(workspace.path().to_string_lossy().as_ref())
        );
        assert_eq!(
            read_chat_cwd(&agent, &canonical).as_deref(),
            Some(workspace.path().to_string_lossy().as_ref())
        );
        let meta = read_chat_meta(&agent, &canonical).unwrap();
        assert_eq!(meta.chat_id.as_deref(), Some("oc_cursor"));
        assert_eq!(meta.chat_type.as_deref(), Some("group"));
        assert_eq!(meta.creator.as_deref(), Some("ou_owner"));
    }

    #[test]
    fn external_cursor_history_is_never_fed_to_cursor_acp() {
        // 2026-08-20 删掉 CliSession 之后 cursor 只剩 ACP 一条通道,`resume_cli_kind()` 恒
        // "cursor-acp" —— 于是**任何** cursor agent 都与外部 `cursor -p` 历史不兼容。
        // 这是删除的连带后果:cursor 的外部会话召回从此不可行(两个 id 命名空间不通,
        // 硬喂过去只会报错或串到别的会话)。codex 不分通道,照旧兼容。
        let cursor = crate::test_support::agent_with("cursor", "");
        assert_eq!(cursor.resume_cli_kind(), "cursor-acp");
        assert!(!external_headless_channel_compatible(&cursor));

        let codex_rpc: Agent =
            serde_yaml::from_str("name: test\nruntime:\n  cli: codex\n").unwrap();
        assert!(external_headless_channel_compatible(&codex_rpc));
    }

    #[test]
    fn managed_codex_handoff_moves_the_only_owner_and_reuses_the_live_process() {
        let _process_guard = crate::test_support::process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let cases = r#"
    *'thread/start'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-owned","status":{"type":"idle"},"turns":[]}}}' ;;
    *'thread/resume'*) printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"th-owned","status":{"type":"idle"},"turns":[]}}}' ;;
"#;
        let script = crate::test_support::write_fake_app_server(tmp.path(), cases);
        let yaml = format!(
            "name: test\nruntime:\n  cli: codex\n  capture: rpc\n  cli_cmd: {}\n",
            script.to_string_lossy()
        );
        let mut agent: Agent = serde_yaml::from_str(&yaml).unwrap();
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串味。
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        let cwd = tmp.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = cwd.to_string_lossy().into_owned();
        let source_chat_id = "oc_source";
        let target_chat_id = "oc_target";
        let source_type = "group";
        let source_tag = chat_tag(source_type, source_chat_id);
        let target_tag = chat_tag("topic_group", target_chat_id);
        let source_session = agent.tmux_name_for_chat(source_type, source_chat_id, false);
        let target_session = agent.tmux_name_for_chat("topic_group", target_chat_id, false);
        write_chat_cwd_checked(&agent, &source_session, &cwd).unwrap();
        write_chat_meta_checked(
            &agent,
            &source_session,
            source_chat_id,
            source_type,
            Some("原群"),
            "ou_owner",
            Some("ou_owner"),
        )
        .unwrap();

        let inflight = crate::runtime::inflight::InFlight::new();
        let pool = crate::capture::rpc_pool::RpcPool::new(inflight.clone());
        let source_client = pool
            .checkout(&crate::capture::rpc_pool::RpcChat {
                agent: &agent,
                session: &source_session,
                tag: &source_tag,
                chat_id: "oc_bridge_test",
                cwd: Some(&cwd),
                primer: None,
                model: None,
                service_tier: None,
                cursor_mode: None,
                codex_policy: Default::default(),
                register_platform_tools: false,
            })
            .unwrap();
        let pgid = source_client.pgid();
        assert_eq!(source_client.session_id(), "th-owned");
        drop(source_client);

        let (source_tx, source_rx) = channel();
        let worker = std::thread::spawn(move || match source_rx.recv().unwrap() {
            ThreadMsg::RpcHandoffDetach { lease, ack } => {
                let claimed = lease.claim();
                let _ = ack.send(claimed);
                assert!(claimed);
            }
            _ => panic!("源 worker 只应收到 handoff detach"),
        });
        let mut chats = HashMap::new();
        chats.insert(
            source_chat_id.to_string(),
            ChatHandle {
                tx: source_tx,
                last_activity: SystemTime::now(),
                idle_prompted: false,
                chat_type: source_type.into(),
                tmux_name: source_session.clone(),
                tmux_seen_live: false,
                local_claimed: true,
                last_inbound_scheduled: false,
                last_inbound_id: String::new(),
                turn_was_active: false,
                subagent_state: None,
                menu_ledger: None,
            },
        );
        let inbox = crate::runtime::inbox_wal::InboxWal::new(tmp.path().join("inbox"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(tmp.path().join("outbox"));
        let stores = LocalChatStores::new(&inbox, &inflight, &outbox);
        let scope = feishu_storage_scope(&agent);
        let bridge_store = crate::bridge::store::BridgeStore::new(tmp.path().join("bridge"));
        let source_ref = crate::bridge::recall_rpc::discover(&agent, target_chat_id, &pool)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.candidate.sid == "th-owned")
            .and_then(|candidate| candidate.candidate.source_observation_id)
            .unwrap();

        let reply = perform_rpc_handoff_with_pool(
            &agent,
            RpcHandoffGroupRequest {
                provider: summon_core::recall::RecallProvider::Codex,
                storage_scope: &scope,
                sid: "th-owned",
                chat_id: target_chat_id,
                chat_type: "topic_group",
                actor_open_id: "ou_owner",
                source_ref: &source_ref,
                observed_at_ms: now_unix_ms(),
                replayed_from_wal: false,
            },
            &mut chats,
            stores,
            &bridge_store,
            &pool,
            RpcHandoffHooks {
                prove_target_reachable: &|_| Ok(()),
                record_gone_owner: &|_, _, _, _| Ok(()),
            },
        )
        .unwrap();
        worker.join().unwrap();

        assert!(reply.contains("已把 Codex 托管会话"));
        assert!(!chats.contains_key(source_chat_id));
        assert!(!crate::session::resume_store::record_path(&agent, &source_session).exists());
        let target_record = crate::session::resume_store::read_record(&agent, &target_session)
            .expect("target 必须成为唯一 durable owner");
        assert_eq!(target_record.id, "th-owned");
        assert!(target_record.strict_resume);
        assert!(pool
            .handoff_snapshot(&agent.name, &source_tag)
            .unwrap()
            .is_none());
        let target_snapshot = pool
            .handoff_snapshot(&agent.name, &target_tag)
            .unwrap()
            .unwrap();
        assert_eq!(target_snapshot.pgid, pgid);

        let target_client = pool
            .checkout(&crate::capture::rpc_pool::RpcChat {
                agent: &agent,
                session: &target_session,
                tag: &target_tag,
                chat_id: "oc_test",
                cwd: Some(&cwd),
                primer: None,
                model: None,
                service_tier: None,
                cursor_mode: None,
                codex_policy: Default::default(),
                register_platform_tools: false,
            })
            .unwrap();
        assert_eq!(
            target_client.checkout_mode(),
            crate::capture::rpc_pool::Checkout::Reused
        );
        assert_eq!(target_client.pgid(), pgid);
        assert!(
            !crate::session::resume_store::read_record(&agent, &target_session)
                .unwrap()
                .strict_resume
        );
        drop(target_client);
        let requests =
            std::fs::read_to_string(crate::test_support::fake_rpc_server_log(tmp.path())).unwrap();
        assert_eq!(requests.matches("\"method\":\"thread/start\"").count(), 1);
        assert_eq!(requests.matches("\"method\":\"thread/resume\"").count(), 0);
        assert!(pool.evict(&agent.name, &target_tag));
    }

    #[test]
    fn durable_wal_replay_moves_the_owner_only_after_recording_boot_gone_proof() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: test\nruntime:\n  cli: codex\n  capture: rpc\n").unwrap();
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串味。
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        let cwd_dir = tmp.path().join("work");
        std::fs::create_dir(&cwd_dir).unwrap();
        let cwd = cwd_dir.to_string_lossy().into_owned();
        let source_chat_id = "oc_source_replay";
        let target_chat_id = "oc_target_replay";
        let source_type = "group";
        let target_type = "topic_group";
        let source_session = agent.tmux_name_for_chat(source_type, source_chat_id, false);
        let target_session = agent.tmux_name_for_chat(target_type, target_chat_id, false);
        let target_tag = chat_tag(target_type, target_chat_id);
        write_chat_cwd_checked(&agent, &source_session, &cwd).unwrap();
        write_chat_meta_checked(
            &agent,
            &source_session,
            source_chat_id,
            source_type,
            Some("原群"),
            "ou_owner",
            Some("ou_owner"),
        )
        .unwrap();
        crate::session::resume_store::write_checked(
            &agent,
            &source_session,
            "codex",
            "th-replay",
            Some(&cwd),
            None,
        )
        .unwrap();

        let source_tag = chat_tag(source_type, source_chat_id);
        let source_record =
            crate::session::resume_store::read_record(&agent, &source_session).unwrap();
        let source_ref = crate::bridge::recall_rpc::durable_source_ref(
            &agent.name,
            &source_session,
            source_chat_id,
            source_type,
            &source_tag,
            &source_record,
        );

        let inflight = crate::runtime::inflight::InFlight::new();
        let pool = crate::capture::rpc_pool::RpcPool::new(inflight.clone());
        let inbox = crate::runtime::inbox_wal::InboxWal::new(tmp.path().join("inbox"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(tmp.path().join("outbox"));
        let stores = LocalChatStores::new(&inbox, &inflight, &outbox);
        let scope = feishu_storage_scope(&agent);
        let bridge_store = crate::bridge::store::BridgeStore::new(tmp.path().join("bridge"));
        let mut chats = HashMap::new();
        let gone_recorded = std::cell::Cell::new(false);

        let reply = perform_rpc_handoff_with_pool(
            &agent,
            RpcHandoffGroupRequest {
                provider: summon_core::recall::RecallProvider::Codex,
                storage_scope: &scope,
                sid: "th-replay",
                chat_id: target_chat_id,
                chat_type: target_type,
                actor_open_id: "ou_owner",
                source_ref: &source_ref,
                observed_at_ms: now_unix_ms(),
                replayed_from_wal: true,
            },
            &mut chats,
            stores,
            &bridge_store,
            &pool,
            RpcHandoffHooks {
                prove_target_reachable: &|_| Ok(()),
                record_gone_owner: &|record_agent, record_tag, record_session, record_sid| {
                    assert_eq!(record_agent, agent.name);
                    assert_eq!(record_tag, target_tag);
                    assert_eq!(record_session, target_session);
                    assert_eq!(record_sid, "th-replay");
                    assert!(
                        !crate::session::resume_store::read_record(&agent, &source_session)
                            .unwrap()
                            .strict_resume,
                        "Gone proof 必须先于 strict intent"
                    );
                    assert!(
                        crate::session::resume_store::read_record(&agent, &target_session)
                            .is_none(),
                        "Gone proof 必须先于 durable owner move"
                    );
                    gone_recorded.set(true);
                    Ok(())
                },
            },
        )
        .unwrap();

        assert!(gone_recorded.get());
        assert!(reply.contains("已把 Codex 托管会话"));
        assert!(!crate::session::resume_store::record_path(&agent, &source_session).exists());
        let target = crate::session::resume_store::read_record(&agent, &target_session).unwrap();
        assert_eq!(target.id, "th-replay");
        assert!(target.strict_resume);
        assert!(pool
            .handoff_snapshot(&agent.name, &target_tag)
            .unwrap()
            .is_none());
    }

    #[test]
    fn durable_wal_replay_is_idempotent_after_the_session_move_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: test\nruntime:\n  cli: codex\n  capture: rpc\n").unwrap();
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串味。
        let agent_dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        let cwd_dir = tmp.path().join("work");
        std::fs::create_dir(&cwd_dir).unwrap();
        let cwd = cwd_dir.to_string_lossy().into_owned();
        let target_chat_id = "oc_target_committed";
        let target_type = "topic_group";
        let target_session = agent.tmux_name_for_chat(target_type, target_chat_id, false);
        crate::session::resume_store::write_checked(
            &agent,
            &target_session,
            "codex",
            "th-committed",
            Some(&cwd),
            None,
        )
        .unwrap();
        crate::session::resume_store::mark_strict_checked(
            &agent,
            &target_session,
            "codex",
            "th-committed",
        )
        .unwrap();
        // 模拟 crash 点：`.session` 已成为 target owner，但 `.cwd/.meta` 尚未来得及补齐。
        assert!(read_chat_cwd(&agent, &target_session).is_none());
        assert!(read_chat_meta(&agent, &target_session).is_none());

        let inflight = crate::runtime::inflight::InFlight::new();
        let pool = crate::capture::rpc_pool::RpcPool::new(inflight.clone());
        let inbox = crate::runtime::inbox_wal::InboxWal::new(tmp.path().join("inbox"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(tmp.path().join("outbox"));
        let stores = LocalChatStores::new(&inbox, &inflight, &outbox);
        let scope = feishu_storage_scope(&agent);
        let bridge_store = crate::bridge::store::BridgeStore::new(tmp.path().join("bridge"));
        let mut chats = HashMap::new();
        let reachability_checked = std::cell::Cell::new(false);

        let reply = perform_rpc_handoff_with_pool(
            &agent,
            RpcHandoffGroupRequest {
                provider: summon_core::recall::RecallProvider::Codex,
                storage_scope: &scope,
                sid: "th-committed",
                chat_id: target_chat_id,
                chat_type: target_type,
                actor_open_id: "ou_owner",
                source_ref: "old-source-revision",
                observed_at_ms: now_unix_ms(),
                replayed_from_wal: true,
            },
            &mut chats,
            stores,
            &bridge_store,
            &pool,
            RpcHandoffHooks {
                prove_target_reachable: &|_| {
                    reachability_checked.set(true);
                    Ok(())
                },
                record_gone_owner: &|_, _, _, _| {
                    panic!("commit 后重放不得再伪造 Gone proof 或移动 owner")
                },
            },
        )
        .unwrap();

        assert!(reachability_checked.get());
        assert!(reply.contains("重放确认目标仍是唯一 owner"));
        assert_eq!(
            read_chat_cwd(&agent, &target_session).as_deref(),
            Some(cwd.as_str())
        );
        let meta = read_chat_meta(&agent, &target_session).unwrap();
        assert_eq!(meta.chat_id.as_deref(), Some(target_chat_id));
        assert_eq!(meta.chat_type.as_deref(), Some(target_type));
        let owner = crate::session::resume_store::read_record(&agent, &target_session).unwrap();
        assert_eq!(owner.id, "th-committed");
        assert!(owner.strict_resume);
    }

    #[test]
    fn headless_recall_session_is_not_committed_when_prerequisite_write_fails() {
        let (agent, _agent_dir) = crate::test_support::agent_dir("codex", "");
        let workspace = tempfile::tempdir().unwrap();
        let found = headless_candidate(
            summon_core::recall::RecallProvider::Codex,
            "codex-native-id",
            workspace.path(),
        );
        let canonical = agent.tmux_name_for_chat("group", "oc_codex", false);
        // 用同名目录占住 `.meta` 文件路径，稳定制造 meta 写失败；cwd 可先写，但提交点
        // `.session` 必须仍不存在。
        std::fs::create_dir_all(chat_meta_path(&agent, &canonical)).unwrap();
        let error =
            persist_headless_recall_binding(&agent, &found, "oc_codex", "group", "ou_owner")
                .expect_err("前置 meta 写失败必须中止");
        assert!(format!("{error:#}").contains("写群元数据"));
        assert!(!crate::session::resume_store::record_path(&agent, &canonical).exists());
    }

    #[test]
    fn headless_recall_binds_only_and_never_launches_or_terminates_source() {
        let source = include_str!("bridge.rs");
        let start = source.find("fn recall_headless_into_group(").unwrap();
        let end = source[start..]
            .find("fn recall_resume_into_group(")
            .map(|offset| start + offset)
            .unwrap();
        let production = &source[start..end];
        for forbidden in [
            "ensure_started(",
            "launch_staging(",
            "terminate_active_source(",
            "signal_exact",
            "killpg",
        ] {
            assert!(
                !production.contains(forbidden),
                "无头召回只准提交绑定，不得启动/终止进程:{forbidden}"
            );
        }
    }

    #[test]
    fn recall_cmux_remote_checks_then_persists_then_starts_and_never_signals() {
        let source = include_str!("bridge.rs");
        let start = source.find("fn bind_cmux_remote(").unwrap();
        let end = source[start..]
            .find("pub(super) fn bridge_slot_message(")
            .map(|offset| start + offset)
            .unwrap();
        let production = &source[start..end];
        let at = |needle: &str| {
            production
                .find(needle)
                .unwrap_or_else(|| panic!("bind_cmux_remote 里找不到 {needle}"))
        };
        let order = [
            at("prepare_binding("),
            at("takeover_pending_for(takeovers, sid, Some(session.pid))"),
            at(".ping()"),
            at("discover_active_for_display()"),
            at("local_chat_blockers("),
            at("std::fs::metadata(&session.transcript_path)"),
            at("store.write_offset("),
            at("store.save(&binding)"),
            at("runtime::start("),
            at("send_bridge_reply("),
        ];
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "核对 → 占用 → 量转录末尾 → offset → 记录 → 起 worker → 群里提示,顺序不能乱"
        );
        for forbidden in ["signal", "kill", "terminate", "SIGTERM"] {
            assert!(
                !production.contains(forbidden),
                "遥控绑定不对 cmux 里的进程发任何信号:{forbidden}"
            );
        }
    }

    #[test]
    fn active_recall_orders_preflight_then_kill_then_persist_then_launch() {
        let source = include_str!("bridge.rs");
        let start = source.find("fn recall_resume_into_group(").unwrap();
        let end = source[start..]
            .find("/// 召回卡按钮回调的纯决策")
            .map(|offset| start + offset)
            .unwrap();
        let production = &source[start..end];
        let preflight = production.find("build_recall_resume_plan(").unwrap();
        let kill = production.find("terminate_active_source(").unwrap();
        let historical_proof = production.find("prove_no_source_writer(").unwrap();
        let persist = production.find("persist_recall_resume_attempt(").unwrap();
        let launch = production.find("launch.launch_staging()").unwrap();
        assert!(
            preflight < kill && kill < persist && persist < launch,
            "安全时序必须是 preflight → source Gone → durable attempt → staging"
        );
        assert!(
            preflight < historical_proof && historical_proof < persist,
            "历史路径也必须在 durable attempt 前证明无 live writer"
        );
    }

    /// 忙碌与空闲会话走同一条接管路径,代价写在召回卡的确认框里。
    #[test]
    fn recall_takeover_does_not_gate_on_live_status() {
        let source = include_str!("bridge.rs");
        let start = source.find("fn recall_resume_into_group(").unwrap();
        let end = start
            + source[start..]
                .find("\nfn ")
                .expect("recall_resume_into_group 之后还有函数");
        let body = &source[start..end];
        assert!(!body.contains("live_status") && !body.contains("\"busy\""));
    }

    #[test]
    fn discovery_observation_keeps_source_exit_proof_required_after_live_list_degrades() {
        let historical_now = summon_core::recall::SessionCandidate {
            provider: summon_core::recall::RecallProvider::Claude,
            ownership: summon_core::recall::RecallOwnership::External,
            sid: "sid-active-at-render".into(),
            cwd: "/tmp".into(),
            name: "source".into(),
            prompt_preview: String::new(),
            last_active_ms: 1,
            live_pid: None,
            live_status: None,
            source_observation_id: None,
            background_tasks: Vec::new(),
        };
        assert!(!recall_requires_source_exit(&historical_now, None));
        assert!(
            recall_requires_source_exit(&historical_now, Some("durable-observation")),
            "卡片渲染时曾活跃就必须保留 source-exit proof，不能被点击时列表降级绕过"
        );
        let active_now = summon_core::recall::SessionCandidate {
            live_pid: Some(42),
            ..historical_now
        };
        assert!(recall_requires_source_exit(&active_now, None));
    }

    #[test]
    fn recall_resume_operate_gate_rejects_intruder_open_id() {
        // 把真实 operate 名单接到纯决策上:名单 [ou_owner],入侵者 open_id 过不了 can_operate → Reject;
        // owner 过 gate → ResumeGroup。handler 恒按 is_group=true 鉴权(洞#3),这里对齐。
        let agent = crate::test_support::agent_with("claude", "feishu:\n  operators: [ou_owner]\n");
        let resume = serde_json::json!({
            "action": "recall_resume", "sid": "sid-y", "chat_id": "oc_g", "chat_type": "group"
        });
        let intruder_ok = agent.can_operate("ou_intruder", "", true, OperateScope::Agent);
        assert!(!intruder_ok, "入侵者不在 operate 名单");
        assert!(matches!(
            plan_recall_callback(
                &resume,
                intruder_ok,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::Reject(_)
        ));
        let owner_ok = agent.can_operate("ou_owner", "", true, OperateScope::Agent);
        assert!(owner_ok, "owner 在 operate 名单");
        assert!(matches!(
            plan_recall_callback(
                &resume,
                owner_ok,
                Some(summon_core::recall::RecallProvider::Claude),
            ),
            RecallCallbackPlan::ResumeGroup {
                provider: summon_core::recall::RecallProvider::Claude,
                sid: "sid-y",
                chat_id: "oc_g",
                chat_type: "group",
                source_observation_id: None,
            }
        ));
    }

    #[test]
    fn recall_resume_gate_is_group_true_closes_empty_operate_allow_all_hole() {
        // 洞#3 核心:operate 名单为空时,can_operate 只在 is_group=false 才 allow-all。接管 handler
        // 硬编码 is_group=true,故空名单 = fail-closed(拒),绝不再让"私聊卡=谁都能接管"。
        let agent = crate::test_support::agent_with("claude", "feishu: {}\n");
        // 私聊卡的旧行为(is_group=false)会 allow-all —— 正是被修掉的洞。
        assert!(
            agent.can_operate("ou_anyone", "", false, OperateScope::Agent),
            "空名单 + is_group=false 会 allow-all(旧洞)"
        );
        // 接管实际用的 is_group=true → 拒。
        assert!(
            !agent.can_operate("ou_anyone", "", true, OperateScope::Agent),
            "空名单 + is_group=true 必须 fail-closed(洞#3 修复)"
        );
    }

    #[test]
    fn occupancy_matrix_is_fail_closed_but_history_only_is_free() {
        assert!(!empty_evidence().occupied());
        let mut cases = Vec::new();
        let mut live = empty_evidence();
        live.live_tmux = true;
        cases.push(live);
        let mut corrupt = empty_evidence();
        corrupt.resume = ResumePresence::Corrupt;
        cases.push(corrupt);
        let mut valid = empty_evidence();
        valid.resume = ResumePresence::Valid(summon_core::session_id::SessionRecord {
            cli: "claude".into(),
            id: "sid".into(),
            cwd: Some("/tmp".into()),
            created_at: None,
            ..Default::default()
        });
        cases.push(valid);
        let mut unreadable = empty_evidence();
        unreadable.resume = ResumePresence::Unreadable;
        cases.push(unreadable);
        let mut legacy_valid = empty_evidence();
        legacy_valid.legacy_resume =
            ResumePresence::Valid(summon_core::session_id::SessionRecord {
                cli: "claude".into(),
                id: "legacy-sid".into(),
                cwd: None,
                created_at: None,
                ..Default::default()
            });
        cases.push(legacy_valid);
        let mut legacy_corrupt = empty_evidence();
        legacy_corrupt.legacy_resume = ResumePresence::Corrupt;
        cases.push(legacy_corrupt);
        let mut legacy_unreadable = empty_evidence();
        legacy_unreadable.legacy_resume = ResumePresence::Unreadable;
        cases.push(legacy_unreadable);
        let mut pending = empty_evidence();
        pending.pending_turn_file = true;
        cases.push(pending);
        let mut wal = empty_evidence();
        wal.inbox_pending = true;
        cases.push(wal);
        let mut recovery = empty_evidence();
        recovery.inbox_recovery_pending = true;
        cases.push(recovery);
        let mut local_outbox = empty_evidence();
        local_outbox.local_outbox_pending = true;
        cases.push(local_outbox);
        let mut running = empty_evidence();
        running.inflight = true;
        cases.push(running);
        let mut queued = empty_evidence();
        queued.queued_local_work = true;
        cases.push(queued);
        let mut remote = empty_evidence();
        remote.remote_bound = true;
        cases.push(remote);
        assert!(cases.into_iter().all(|evidence| evidence.occupied()));
    }

    /// 拒绝回执说出拦住它的每一类证据和对应解法,解法还要指对地方:私聊里的回执若写「本群发
    /// `/reset`」,照做重置的是私聊自己的会话。
    #[test]
    fn occupied_reply_names_each_blocker_with_its_remedy_and_place() {
        let mut every = empty_evidence();
        every.live_tmux = true;
        every.resume = ResumePresence::Corrupt;
        every.legacy_resume = ResumePresence::Unreadable;
        every.inflight = true;
        every.queued_local_work = true;
        every.inbox_recovery_pending = true;
        every.pending_turn_file = true;
        every.inbox_pending = true;
        every.local_outbox_pending = true;
        every.remote_bound = true;
        let group = every.blockers(ReplyPlace::TargetGroup);
        assert_eq!(group.len(), 8, "同一解法的证据合并成一行");
        assert!(group.contains(&"本群正在遥控 cmux 会话 —— 发 `/reset` 解除遥控".to_string()));
        let private = every.blockers(ReplyPlace::Private);
        assert_eq!(private.len(), group.len());
        assert!(
            private.iter().all(|line| !line.contains("本群")),
            "{private:?}"
        );
        assert!(private
            .iter()
            .filter(|line| line.contains("`/reset`"))
            .all(|line| line.contains("在目标群发 `/reset`")));
        assert!(private
            .contains(&"目标群正在遥控 cmux 会话 —— 在目标群发 `/reset` 解除遥控".to_string()));
        assert_eq!(
            group
                .iter()
                .filter(|line| line.contains("`/reset`"))
                .count(),
            5
        );
        assert_eq!(
            group
                .iter()
                .filter(|line| line.contains("稍后重试"))
                .count(),
            3
        );

        let mut recovery = empty_evidence();
        recovery.inbox_recovery_pending = true;
        let blockers = recovery.blockers(ReplyPlace::TargetGroup);
        assert_eq!(blockers.len(), 1);
        assert!(blockers[0].contains("恢复卡") && blockers[0].contains("「全部忽略」"));
        assert_eq!(
            occupied_reply("拒绝绑定:", &blockers),
            format!("拒绝绑定:\n\n- {}", blockers[0])
        );
    }

    /// 回执措辞跟着回执送达的位置走:群里的 `/bridge` 回执在目标群,召回卡/控制卡回调的
    /// 回执在私聊。调用点传错位置,私聊回执就会让用户在私聊里发 `/reset`。
    #[test]
    fn occupied_reply_place_matches_where_each_reply_lands() {
        let code = include_str!("bridge.rs")
            .split("\n#[cfg(test)]")
            .next()
            .unwrap_or_default();
        let body_of = |signature: &str| {
            let start = code
                .find(signature)
                .unwrap_or_else(|| panic!("{signature} 不见了"));
            let rest = &code[start + signature.len()..];
            let end = ["\nfn ", "\npub(super) fn ", "\npub(crate) fn ", "\npub fn "]
                .iter()
                .filter_map(|next| rest.find(next))
                .min()
                .unwrap_or(rest.len());
            &rest[..end]
        };
        let group = body_of("pub(super) fn handle_bridge_command_with_store(");
        assert!(group.contains("ReplyPlace::TargetGroup,"));
        assert!(!group.contains("ReplyPlace::Private"));
        for private in [
            "fn recall_headless_into_group(",
            "pub(super) fn handle_bridge_card_action_with_stores(",
        ] {
            let body = body_of(private);
            assert!(body.contains("ReplyPlace::Private,"), "{private}");
            assert!(!body.contains("ReplyPlace::TargetGroup"), "{private}");
        }
    }

    #[test]
    fn history_only_meta_and_unclaimed_handle_are_free() {
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let chat_id = format!("oc_history_{}", uuid::Uuid::new_v4().simple());
        let historical_session = format!("history-{}", uuid::Uuid::new_v4().simple());
        write_chat_meta(
            &agent,
            &historical_session,
            &chat_id,
            "group",
            None,
            "ou_owner",
            Some("ou_owner"),
        );
        let (chat_tx, _chat_rx) = channel();
        let mut chats = HashMap::new();
        chats.insert(
            chat_id.clone(),
            ChatHandle {
                tx: chat_tx,
                last_activity: SystemTime::now(),
                idle_prompted: false,
                chat_type: "group".into(),
                tmux_name: historical_session,
                tmux_seen_live: false,
                local_claimed: false,
                last_inbound_scheduled: false,
                last_inbound_id: String::new(),
                turn_was_active: false,
                subagent_state: None,
                menu_ledger: None,
            },
        );
        let temp = tempfile::tempdir().unwrap();
        let storage_scope = feishu_storage_scope(&agent);
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let evidence = local_chat_evidence(
            &agent,
            &storage_scope,
            "group",
            &chat_id,
            &chats,
            LocalChatStores::new(
                &crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox")),
                &crate::runtime::inflight::InFlight::new(),
                &outbox,
            ),
        )
        .unwrap();
        assert!(!evidence.queued_local_work);
        assert!(!evidence.occupied());
    }

    /// 遥控中的群对 Bridge 而言是被占用的:一群一会话。已结束的记录不占用(允许重新绑定),
    /// 读不出的记录 fail-closed。
    #[test]
    fn local_chat_evidence_counts_a_live_remote_binding_as_occupied() {
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let chat_id = format!("oc_remote_{}", uuid::Uuid::new_v4().simple());
        let temp = tempfile::tempdir().unwrap();
        let storage_scope = feishu_storage_scope(&agent);
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let inbox = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let inflight = crate::runtime::inflight::InFlight::new();
        let chats = HashMap::new();
        let evidence = || {
            local_chat_evidence(
                &agent,
                &storage_scope,
                "group",
                &chat_id,
                &chats,
                LocalChatStores::new(&inbox, &inflight, &outbox),
            )
        };
        let store = crate::remote::store::RemoteStore::for_agent(&agent);

        store.save(&remote_binding(&chat_id, None)).unwrap();
        let live = evidence().unwrap();
        assert!(live.remote_bound && live.occupied());

        store
            .save(&remote_binding(&chat_id, Some("会话进程已经退出")))
            .unwrap();
        let ended = evidence().unwrap();
        assert!(!ended.remote_bound && !ended.occupied());

        std::fs::write(
            agent
                .state_dir()
                .join("remote")
                .join(format!("{chat_id}.json")),
            "{",
        )
        .unwrap();
        assert!(evidence().is_err(), "遥控记录读不出时不能判成空闲");
    }

    /// Claude 召回接管不走 local_chat_blockers,遥控占用由它自己在杀源之前拦下。
    #[test]
    fn recall_takeover_refuses_a_remote_bound_group_before_touching_the_source() {
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let chat_id = format!("oc_remote_{}", uuid::Uuid::new_v4().simple());
        let temp = tempfile::tempdir().unwrap();
        let bridge_store = crate::bridge::store::BridgeStore::new(temp.path().join("bridge"));
        let storage_scope = feishu_storage_scope(&agent);
        let request = || RecallGroupRequest {
            storage_scope: &storage_scope,
            sid: "sid-not-on-disk",
            chat_id: &chat_id,
            chat_type: "group",
            actor_open_id: "ou_owner",
            source_observation_id: None,
        };
        crate::remote::store::RemoteStore::for_agent(&agent)
            .save(&remote_binding(&chat_id, None))
            .unwrap();
        let reply = recall_resume_into_group(&agent, &bridge_store, request());
        assert!(
            reply.contains("目标群正在遥控 cmux 会话 —— 在目标群发 `/reset` 解除遥控"),
            "{reply}"
        );

        std::fs::write(
            agent
                .state_dir()
                .join("remote")
                .join(format!("{chat_id}.json")),
            "{",
        )
        .unwrap();
        let reply = recall_resume_into_group(&agent, &bridge_store, request());
        assert!(reply.starts_with("无法证明目标群空闲"), "{reply}");
    }

    /// 群 B 接管一条正被群 A 遥控的会话:目标群 B 空闲,拦下它的只能是按会话的检查,且要在
    /// 找候选、准备与杀源之前。
    #[test]
    fn recall_takeover_refuses_a_session_remote_controlled_by_another_group() {
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let group_a = format!("oc_remote_a_{}", uuid::Uuid::new_v4().simple());
        let group_b = format!("oc_remote_b_{}", uuid::Uuid::new_v4().simple());
        let temp = tempfile::tempdir().unwrap();
        let bridge_store = crate::bridge::store::BridgeStore::new(temp.path().join("bridge"));
        let storage_scope = feishu_storage_scope(&agent);
        let store = crate::remote::store::RemoteStore::for_agent(&agent);
        store
            .save(&summon_core::remote::RemoteBinding {
                session_id: "sid-remote-elsewhere".into(),
                ..remote_binding(&group_a, None)
            })
            .unwrap();
        let request = || RecallGroupRequest {
            storage_scope: &storage_scope,
            sid: "sid-remote-elsewhere",
            chat_id: &group_b,
            chat_type: "group",
            actor_open_id: "ou_owner",
            source_observation_id: Some("observation-of-the-live-source"),
        };
        assert_eq!(
            recall_resume_into_group(&agent, &bridge_store, request()),
            "这条会话正在被另一个群遥控,不能接管。先在那个群发 `/reset` 解除遥控。"
        );
        assert_eq!(
            recall_resume_reply(&agent, "sid-remote-elsewhere"),
            "这条会话正在被另一个群遥控,不能接管。先在那个群发 `/reset` 解除遥控。",
            "无目标群的恢复同样会另起一个 writer"
        );

        // 已结束的遥控不挡接管。
        store
            .save(&summon_core::remote::RemoteBinding {
                session_id: "sid-remote-elsewhere".into(),
                ..remote_binding(&group_a, Some("会话进程已经退出"))
            })
            .unwrap();
        assert_eq!(
            session_remote_refusal(&agent, "sid-remote-elsewhere", None),
            None
        );

        // 任何一份遥控记录读不出,都证明不了这条会话没在遥控。
        std::fs::write(
            agent.state_dir().join("remote").join("oc_corrupt.json"),
            "{",
        )
        .unwrap();
        assert_eq!(
            recall_resume_into_group(&agent, &bridge_store, request()),
            "遥控记录读不出,已拒绝接管。"
        );
        assert_eq!(
            recall_resume_reply(&agent, "sid-remote-elsewhere"),
            "遥控记录读不出,已拒绝接管。"
        );
    }

    /// `/clear` 在同一个进程里换了 sid:遥控绑在旧 sid 上,要结束的是同一个 pid。
    #[test]
    fn remote_refusal_matches_the_process_even_when_the_sid_differs() {
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let store = crate::remote::store::RemoteStore::for_agent(&agent);
        store
            .save(&summon_core::remote::RemoteBinding {
                session_id: "sid-before-clear".into(),
                pid: 4242,
                ..remote_binding("oc_remote_pid", None)
            })
            .unwrap();
        assert_eq!(
            session_remote_refusal(&agent, "sid-after-clear", Some(4242)),
            Some(::summon_core::i18n::label(SESSION_REMOTE_CONTROLLED))
        );
        assert_eq!(
            session_remote_refusal(&agent, "sid-after-clear", Some(4343)),
            None
        );
        assert_eq!(
            session_remote_refusal(&agent, "sid-after-clear", None),
            None,
            "不知道 pid 时只按 sid 查,不凭空拒"
        );
        store
            .save(&summon_core::remote::RemoteBinding {
                session_id: "sid-before-clear".into(),
                pid: 4242,
                ..remote_binding("oc_remote_pid", Some("会话进程已经退出"))
            })
            .unwrap();
        assert_eq!(
            session_remote_refusal(&agent, "sid-after-clear", Some(4242)),
            None,
            "已结束的遥控不挡"
        );
    }

    #[test]
    fn recall_takeover_checks_the_session_before_any_irreversible_step() {
        let source = include_str!("bridge.rs");
        let start = source.find("fn recall_resume_into_group(").unwrap();
        let end = source[start..]
            .find("/// 召回卡按钮回调的纯决策")
            .map(|offset| start + offset)
            .unwrap();
        let production = &source[start..end];
        let pid_guard = production
            .find("session_remote_refusal(agent, sid, Some(observed_pid))")
            .expect("杀源前按要结束的 pid 再查一次");
        assert!(pid_guard < production.find("terminate_active_source(").unwrap());
        let guard = production.find("session_remote_refusal(").unwrap();
        for later in [
            "find_candidate(",
            "build_recall_resume_plan(",
            "terminate_active_source(",
            "prove_no_source_writer(",
            "persist_recall_resume_attempt(",
        ] {
            assert!(
                guard < production.find(later).unwrap(),
                "按会话的遥控检查要先于 {later}"
            );
        }
    }

    #[test]
    fn control_recovery_pages_cover_every_binding_without_truncation() {
        assert_eq!(recovery_page_window(25, 1), Some((0, 12, 3)));
        assert_eq!(recovery_page_window(25, 2), Some((12, 24, 3)));
        assert_eq!(recovery_page_window(25, 3), Some((24, 25, 3)));
        assert_eq!(recovery_page_window(25, 4), None);
        assert_eq!(recovery_page_window(25, 0), None);
    }

    // Task 2 把群 `@Bot /bridge`(Start)从「hook 发现 + ticket + card_bridge_ticket」改成召回私聊发卡
    // (recall_card),群入口不再预留 ticket、也不再是 Mirror 绑定入口。本 E2E 断言旧 group→ticket→Mirror
    // turn/stop 入口,其 turn/stop 编排本身未删(drain/reconcile/handoff 各有测),仅入口被替换。按仓库
    // 「先停用不删、保留代码到验证稳定」的约定暂停用,由 Task 4 决定重写(直接 seed binding)或删除。
    #[ignore = "旧 group→ticket→Mirror 入口已被召回替换,待 Task 4 重写/删除"]
    #[test]
    fn fake_feishu_group_to_private_mirror_turn_and_stop_e2e() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use summon_core::bridge::{
            ExternalLifecycle, ProcessFingerprint, ResumeProfile, SessionKey, TicketStatus,
        };
        use summon_core::outbox::OutboxKind;

        let (agent, _agent_dir) = crate::test_support::agent_dir(
            "claude",
            "feishu:\n  app_id_env: TEST_APP\n  operators: [ou_owner]\n",
        );
        let target_chat_id = format!("oc_bridge_e2e_{}", uuid::Uuid::new_v4().simple());
        let temp = tempfile::tempdir().unwrap();
        let bridge_store = crate::bridge::store::BridgeStore::new(temp.path().join("bridge"));
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let inflight = crate::runtime::inflight::InFlight::new();

        let config_dir = temp.path().join("claude-config");
        let project_dir = temp.path().join("project");
        let transcript_dir = config_dir.join("projects").join("demo");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::create_dir_all(&project_dir).unwrap();
        let transcript = transcript_dir.join("sid-e2e.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let session = SessionKey::new(config_dir.to_string_lossy().into_owned(), "sid-e2e");
        let process = crate::bridge::process_probe::observe_exact_fingerprint(std::process::id())
            .unwrap()
            .expect("测试进程应可探测");
        let profile = ResumeProfile {
            claude_bin: process.executable.clone(),
            home: temp.path().to_string_lossy().into_owned(),
            config_dir: config_dir.to_string_lossy().into_owned(),
            version: "test".into(),
            process_verified: true,
            ..ResumeProfile::default()
        };
        let hook = |id: &str,
                    at: u64,
                    event: crate::bridge::store::HookEvent,
                    permit: Option<summon_core::bridge::ContentPermit>| {
            crate::bridge::store::HookEnvelope {
                schema_version: crate::bridge::store::HOOK_ENVELOPE_VERSION,
                event_id: id.into(),
                observed_at_ms: at,
                session: session.clone(),
                transcript_path: transcript.to_string_lossy().into_owned(),
                cwd: project_dir.to_string_lossy().into_owned(),
                process: Some(ProcessFingerprint {
                    pid: process.pid,
                    start_time: process.start_time.clone(),
                    executable: process.executable.clone(),
                }),
                resume_profile: profile.clone(),
                content_permit: permit,
                managed_takeover: false,
                handoff_ack: None,
                event,
            }
        };
        bridge_store
            .enqueue_hook(&hook(
                "start-e2e",
                1,
                crate::bridge::store::HookEvent::SessionStart {
                    source: "startup".into(),
                    model: None,
                },
                None,
            ))
            .unwrap();
        assert_eq!(
            crate::bridge::drain::drain_tick(&bridge_store)
                .unwrap()
                .applied,
            1
        );

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let stop_server = std::sync::Arc::new(AtomicBool::new(false));
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let message_sequence = std::sync::Arc::new(AtomicUsize::new(0));
        let server_stop = std::sync::Arc::clone(&stop_server);
        let server_captured = std::sync::Arc::clone(&captured);
        let server_sequence = std::sync::Arc::clone(&message_sequence);
        let server_thread = std::thread::spawn(move || {
            while !server_stop.load(Ordering::SeqCst) {
                let Some(mut request) = server
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                else {
                    continue;
                };
                let url = request.url().to_string();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                server_captured
                    .lock()
                    .unwrap()
                    .push(format!("{} {url}\n{body}", request.method()));
                let response = if url.contains("auth/v3/tenant_access_token") {
                    r#"{"tenant_access_token":"token","expire":7200}"#.to_string()
                } else if url.contains("bot/v3/info") {
                    r#"{"bot":{"open_id":"ou_bot"}}"#.to_string()
                } else {
                    let id = server_sequence.fetch_add(1, Ordering::SeqCst) + 1;
                    format!(r#"{{"code":0,"data":{{"message_id":"om_fake_{id}"}}}}"#)
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
        });
        let client = Arc::new(FeishuClient::with_test_base(base));
        let (dispatch_tx, dispatch_rx) = std::sync::mpsc::channel();
        let dispatcher_tx = dispatch_tx.clone();
        let dispatcher = {
            let dispatcher_agent = agent.clone();
            let dispatcher_epoch = super::super::FeishuEpochIdentity::capture(&dispatcher_agent);
            let dispatcher_client = Arc::clone(&client);
            let dispatcher_core = Arc::new(crate::runtime::core::Core::for_routes(HashMap::new()));
            let dispatcher_inflight = inflight.clone();
            let stores = super::super::dispatcher::DispatcherStores {
                bridge: bridge_store.clone(),
                outbox: outbox.clone(),
                inbox: crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox")),
                card_actions: crate::runtime::card_action_wal::CardActionWal::new(
                    temp.path().join("card-inbox"),
                ),
                support_root: Some(temp.path().join("dispatcher-support")),
            };
            std::thread::spawn(move || {
                super::super::dispatcher::agent_dispatcher_with_stores(
                    dispatcher_agent,
                    20,
                    20,
                    dispatcher_client,
                    dispatcher_epoch,
                    dispatch_rx,
                    dispatcher_tx,
                    dispatcher_core,
                    dispatcher_inflight,
                    stores,
                )
            })
        };

        let group = Inbound {
            anchor_message_id: None,
            chat_id: target_chat_id.clone(),
            chat_type: Some("group".into()),
            text: "/bridge".into(),
            sender: "ou_owner".into(),
            sender_union_id: "on_owner".into(),
            sender_type: String::new(),
            message_id: "om_group_command".into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: vec![summon_core::ipc::Mention {
                open_id: "ou_bot".into(),
                key: "@_user_1".into(),
                name: "bot".into(),
                ..Default::default()
            }],
            resources: vec![],
            create_time_ms: 0,
            replayed: false,
            reattach: false,
        };
        dispatch_tx.send(DispatchMsg::Inbound(group)).unwrap();
        wait_until("dispatcher 创建并私聊登记 ticket", || {
            bridge_store
                .load_state()
                .ok()
                .flatten()
                .is_some_and(|state| {
                    state
                        .tickets
                        .values()
                        .any(|ticket| ticket.control_card_message_id.is_some())
                })
        });
        let state = bridge_store.load_state().unwrap().unwrap();
        let ticket = state.tickets.values().next().unwrap().clone();
        assert!(matches!(ticket.status, TicketStatus::Pending));
        assert!(ticket.control_card_message_id.is_some());
        let choice = ticket.choices[0].id.clone();
        let control_message = ticket.control_card_message_id.clone().unwrap();
        let local_session = agent.tmux_name_for_chat("group", &target_chat_id, false);
        wait_until("群命令回执完成", || {
            captured
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("已发送到私聊"))
        });
        assert!(
            !crate::session::tmux::Tmux::new(&local_session).has_session(),
            "首条 /bridge 绝不能创建本地 tmux"
        );
        for path in [
            chat_meta_path(&agent, &local_session),
            chat_cwd_path(&agent, &local_session),
            crate::session::resume_store::record_path(&agent, &local_session),
        ] {
            assert!(
                !path.exists(),
                "首条 /bridge 绝不能创建本地会话文件:{}",
                path.display()
            );
        }
        dispatch_tx
            .send(DispatchMsg::Inbound(Inbound {
                anchor_message_id: None,
                chat_id: target_chat_id.clone(),
                chat_type: Some("group".into()),
                text: "Pending 时的普通群消息".into(),
                sender: "ou_owner".into(),
                sender_union_id: "on_owner".into(),
                sender_type: String::new(),
                message_id: "om_pending_normal".into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: vec![summon_core::ipc::Mention {
                    open_id: "ou_bot".into(),
                    key: "@_user_1".into(),
                    name: "bot".into(),
                    ..Default::default()
                }],
                resources: vec![],
                create_time_ms: 0,
                replayed: false,
                reattach: false,
            }))
            .unwrap();
        wait_until("Pending 普通消息被 Bridge slot 拦截", || {
            captured
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("Bridge 绑定正在私聊确认中"))
        });
        assert!(!crate::session::tmux::Tmux::new(&local_session).has_session());

        let select = CardActionMsg {
            chat_id: "oc_private".into(),
            message_id: control_message.clone(),
            operator_open_id: "ou_owner".into(),
            event_id: "callback-select".into(),
            value: serde_json::json!({
                "action":"bridge_mirror",
                "ticket":ticket.id,
                "choice":choice,
            }),
            wal_id: None,
            wal_replayed: false,
        };
        dispatch_tx.send(DispatchMsg::Card(select)).unwrap();
        wait_until("dispatcher 处理 Mirror callback", || {
            bridge_store
                .load_state()
                .ok()
                .flatten()
                .is_some_and(|state| {
                    state.tickets.get(&ticket.id).is_some_and(|ticket| {
                        matches!(ticket.status, TicketStatus::Activating { .. })
                    })
                })
        });
        assert!(matches!(
            bridge_store.load_state().unwrap().unwrap().tickets[&ticket.id].status,
            TicketStatus::Activating { .. }
        ));

        crate::bridge::reconcile::reconcile_once(&bridge_store, &outbox, 10).unwrap();
        let activation = outbox
            .pending_events_checked()
            .unwrap()
            .into_iter()
            .find(|event| matches!(event.kind, OutboxKind::BridgeActivationAudit { .. }))
            .unwrap();
        super::super::outbox_drain::deliver_bridge_activation_with_store(
            &agent,
            &client,
            &activation,
            &bridge_store,
        )
        .unwrap();
        outbox.ack(&activation.id);
        assert!(matches!(
            bridge_store
                .load_state()
                .unwrap()
                .unwrap()
                .slot(&ticket.target),
            Some(ChatSlot::MirrorBound(_))
        ));
        dispatch_tx
            .send(DispatchMsg::Inbound(Inbound {
                anchor_message_id: None,
                chat_id: target_chat_id.clone(),
                chat_type: Some("group".into()),
                text: "/bridge".into(),
                sender: "ou_owner".into(),
                sender_union_id: "on_owner".into(),
                sender_type: String::new(),
                message_id: "om_bridge_retry".into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: vec![summon_core::ipc::Mention {
                    open_id: "ou_bot".into(),
                    key: "@_user_1".into(),
                    name: "bot".into(),
                    ..Default::default()
                }],
                resources: vec![],
                create_time_ms: 0,
                replayed: false,
                reattach: false,
            }))
            .unwrap();
        wait_until("MirrorBound 重试应私聊重发控制卡", || {
            bridge_store
                .load_state()
                .ok()
                .flatten()
                .and_then(|state| state.tickets.get(&ticket.id).cloned())
                .and_then(|ticket| ticket.control_card_message_id)
                .is_some_and(|message_id| message_id != control_message)
        });
        let retried = bridge_store.load_state().unwrap().unwrap();
        assert_eq!(retried.tickets.len(), 1, "幂等重试不得创建第二张 ticket");
        wait_until("群里收到 Mirror 控制卡恢复提示", || {
            captured
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("已在私聊重发 Mirror 控制卡"))
        });
        dispatch_tx
            .send(DispatchMsg::Inbound(Inbound {
                anchor_message_id: None,
                chat_id: target_chat_id.clone(),
                chat_type: Some("group".into()),
                text: "Mirror 时的普通群消息".into(),
                sender: "ou_owner".into(),
                sender_union_id: "on_owner".into(),
                sender_type: String::new(),
                message_id: "om_mirror_normal".into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: vec![summon_core::ipc::Mention {
                    open_id: "ou_bot".into(),
                    key: "@_user_1".into(),
                    name: "bot".into(),
                    ..Default::default()
                }],
                resources: vec![],
                create_time_ms: 0,
                replayed: false,
                reattach: false,
            }))
            .unwrap();
        wait_until("Mirror 普通消息被 Bridge slot 拦截", || {
            captured
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("外部 Claude Mirror"))
        });
        assert!(!crate::session::tmux::Tmux::new(&local_session).has_session());

        let permit = bridge_store.content_permit(&session).unwrap().unwrap();
        bridge_store
            .enqueue_hook(&hook(
                "prompt-e2e",
                20,
                crate::bridge::store::HookEvent::UserPromptSubmit {
                    prompt_id: Some("p1".into()),
                    prompt: Some("终端问题".into()),
                },
                Some(permit.clone()),
            ))
            .unwrap();
        bridge_store
            .enqueue_hook(&hook(
                "stop-e2e",
                30,
                crate::bridge::store::HookEvent::Stop {
                    prompt_id: Some("p1".into()),
                    last_assistant_message: Some("终端回答".into()),
                    stop_hook_active: false,
                    has_pending_wakeups: false,
                },
                Some(permit),
            ))
            .unwrap();
        assert_eq!(
            crate::bridge::drain::drain_tick(&bridge_store)
                .unwrap()
                .applied,
            2
        );
        crate::bridge::reconcile::reconcile_once(&bridge_store, &outbox, 40).unwrap();
        let turn = outbox
            .pending_events_checked()
            .unwrap()
            .into_iter()
            .find(|event| matches!(event.kind, OutboxKind::ExternalBridgeTurn { .. }))
            .unwrap();
        super::super::outbox_drain::deliver_external_bridge_turn_with_store(
            &agent,
            &client,
            &turn,
            &bridge_store,
        )
        .unwrap();
        super::super::outbox_drain::bridge_outbox_acked_with_store(&turn, &bridge_store).unwrap();
        outbox.ack(&turn.id);
        let after_ack = bridge_store.load_state().unwrap().unwrap();
        assert!(after_ack.pending_mirror_turns.is_empty());
        assert!(after_ack.mirror_export_receipts.is_empty());

        let ticket_count = after_ack.tickets.len();
        let pending_outbox = outbox.pending_events_checked().unwrap().len();
        std::fs::write(agent.dir.join(".env"), "TEST_APP=rotated-value\n").unwrap();
        dispatch_tx
            .send(DispatchMsg::Inbound(Inbound {
                anchor_message_id: None,
                chat_id: target_chat_id.clone(),
                chat_type: Some("group".into()),
                text: "/bridge".into(),
                sender: "ou_owner".into(),
                sender_union_id: "on_owner".into(),
                sender_type: String::new(),
                message_id: "om_identity_drift".into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: vec![summon_core::ipc::Mention {
                    open_id: "ou_bot".into(),
                    key: "@_user_1".into(),
                    name: "bot".into(),
                    ..Default::default()
                }],
                resources: vec![],
                create_time_ms: 0,
                replayed: false,
                reattach: false,
            }))
            .unwrap();
        wait_until(
            "resolved app identity 热漂移后 dispatcher fail-closed",
            || {
                captured
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|request| request.contains("配置身份已变化或暂不可用"))
            },
        );
        let drift_state = bridge_store.load_state().unwrap().unwrap();
        assert_eq!(drift_state.tickets.len(), ticket_count);
        assert!(matches!(
            drift_state.slot(&ticket.target),
            Some(ChatSlot::MirrorBound(_))
        ));
        assert_eq!(
            outbox.pending_events_checked().unwrap().len(),
            pending_outbox
        );
        assert!(!crate::session::tmux::Tmux::new(&local_session).has_session());
        std::fs::remove_file(agent.dir.join(".env")).unwrap();

        let final_ticket = bridge_store.load_state().unwrap().unwrap().tickets[&ticket.id].clone();
        let stop = CardActionMsg {
            chat_id: "oc_private".into(),
            message_id: final_ticket.control_card_message_id.clone().unwrap(),
            operator_open_id: "ou_owner".into(),
            event_id: "callback-stop".into(),
            value: serde_json::json!({
                "action":"bridge_stop",
                "ticket":ticket.id,
                "choice":choice,
            }),
            wal_id: None,
            wal_replayed: false,
        };
        dispatch_tx.send(DispatchMsg::Card(stop)).unwrap();
        wait_until("dispatcher 停止 Mirror", || {
            bridge_store
                .load_state()
                .ok()
                .flatten()
                .is_some_and(|state| state.slot(&ticket.target).is_none())
        });
        assert!(bridge_store
            .load_state()
            .unwrap()
            .unwrap()
            .slot(&ticket.target)
            .is_none());
        assert_eq!(
            crate::bridge::process_probe::probe_exact_process(&process).unwrap(),
            crate::bridge::process_probe::ExactProcessStatus::AliveExact,
            "停止 Mirror 绝不能退出或打断源进程"
        );

        dispatch_tx.send(DispatchMsg::Shutdown).unwrap();
        dispatcher.join().unwrap();
        stop_server.store(true, Ordering::SeqCst);
        server_thread.join().unwrap();
        let requests = captured.lock().unwrap();
        let target_label =
            summon_core::feishu::bridge_target_label(&ticket.target.agent, &ticket.target.chat_id);
        assert!(
            requests
                .iter()
                .filter(|request| request.contains(&target_label))
                .count()
                >= 2
        );
        let turn_request = requests
            .iter()
            .find(|request| request.contains("终端问题") && request.contains("终端回答"))
            .expect("完整 user→assistant turn 应在一张 frozen card 中");
        assert!(turn_request.find("终端问题").unwrap() < turn_request.find("终端回答").unwrap());
        assert!(bridge_store
            .load_state()
            .unwrap()
            .unwrap()
            .pending_mirror_turns
            .is_empty());
        assert_eq!(
            bridge_store.load_state().unwrap().unwrap().sessions[&session.storage_key()].lifecycle,
            ExternalLifecycle::ActiveIdle
        );
        assert!(!crate::session::tmux::Tmux::new(&local_session).has_session());
        assert!([
            chat_meta_path(&agent, &local_session),
            chat_cwd_path(&agent, &local_session),
            crate::session::resume_store::record_path(&agent, &local_session),
        ]
        .into_iter()
        .all(|path| !path.exists()));
    }

    // Task 2 起群 `@Bot /bridge` 走召回私聊发卡,主动私聊失败时不再保留短码兜换(召回恢复不绑定本群,
    // 预留 group-slot ticket 会硬冻结本群入口 5min 且无清理点)。本测试断言旧短码兜底流程,按仓库
    // 「先停用不删」约定暂停用;新的「DM 失败→群回退提示、不建 ticket」行为由飞书真机 E2E 覆盖,Task 4 定夺。
    #[ignore = "旧短码兜底流程已随召回入口移除,待 Task 4 重写/删除"]
    #[test]
    fn proactive_dm_failure_keeps_short_code_redeemable() {
        use summon_core::bridge::{
            ExternalLifecycle, ExternalLocator, ExternalSessionRecord, ProcessFingerprint,
            ResumeProfile, SessionKey, TicketStatus,
        };

        let (agent, _agent_dir) =
            crate::test_support::agent_dir("claude", "feishu:\n  operators: [ou_owner]\n");
        let scope = feishu_storage_scope(&agent);
        let temp = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(temp.path().join("bridge"));
        let config_dir = temp.path().join("claude-config");
        let project_dir = temp.path().join("project");
        let transcript = config_dir.join("projects/demo/sid-fallback.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(&transcript, "").unwrap();
        let key = SessionKey::new(config_dir.to_string_lossy(), "sid-fallback");
        let process = crate::bridge::process_probe::observe_exact_fingerprint(std::process::id())
            .unwrap()
            .expect("测试进程应可探测");
        store
            .with_state(|state| {
                state.upsert_session(ExternalSessionRecord {
                    key: key.clone(),
                    generation: 1,
                    locator: ExternalLocator {
                        key: key.clone(),
                        transcript_path: transcript.to_string_lossy().into_owned(),
                        resume_lookup_cwd: project_dir.to_string_lossy().into_owned(),
                        current_cwd: project_dir.to_string_lossy().into_owned(),
                        profile: ResumeProfile {
                            claude_bin: process.executable.clone(),
                            home: temp.path().to_string_lossy().into_owned(),
                            config_dir: config_dir.to_string_lossy().into_owned(),
                            version: "test".into(),
                            process_verified: true,
                            ..ResumeProfile::default()
                        },
                        revision: 1,
                    },
                    process: Some(ProcessFingerprint {
                        pid: process.pid,
                        start_time: process.start_time.clone(),
                        executable: process.executable.clone(),
                    }),
                    lifecycle: ExternalLifecycle::ActiveIdle,
                    first_seen_ms: 1,
                    last_seen_ms: 1,
                    last_applied_sequence: 1,
                    pending_prompt: None,
                });
                Ok(())
            })
            .unwrap();

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let server_captured = Arc::clone(&captured);
        let server_thread = std::thread::spawn(move || {
            for sequence in 0..4 {
                let mut request = server.recv().unwrap();
                let url = request.url().to_string();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                server_captured
                    .lock()
                    .unwrap()
                    .push(format!("{} {url}\n{body}", request.method()));
                let response = if url.contains("auth/v3/tenant_access_token") {
                    tiny_http::Response::from_string(
                        r#"{"tenant_access_token":"token","expire":7200}"#,
                    )
                } else if url.contains("receive_id_type=open_id") {
                    tiny_http::Response::from_string(r#"{"code":230002,"msg":"forbidden"}"#)
                        .with_status_code(403)
                } else {
                    tiny_http::Response::from_string(format!(
                        r#"{{"code":0,"data":{{"message_id":"om_fallback_{sequence}"}}}}"#
                    ))
                };
                request.respond(response).unwrap();
            }
        });
        let client = FeishuClient::with_test_base(base);
        let chats = HashMap::new();
        let inbox = crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox"));
        let inflight = crate::runtime::inflight::InFlight::new();
        let outbox = crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox"));
        let group = Inbound {
            anchor_message_id: None,
            chat_id: "oc_group".into(),
            chat_type: Some("group".into()),
            text: "/bridge".into(),
            sender: "ou_owner".into(),
            sender_union_id: "on_owner".into(),
            sender_type: String::new(),
            message_id: "om_group".into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: vec![],
            resources: vec![],
            create_time_ms: 0,
            replayed: false,
            reattach: false,
        };
        handle_bridge_command_with_store(
            BridgeCommand::Start,
            &agent,
            &client,
            &scope,
            &group,
            &chats,
            &inbox,
            &inflight,
            &store,
            &outbox,
        );
        let ticket = store
            .load_state()
            .unwrap()
            .unwrap()
            .tickets
            .values()
            .next()
            .unwrap()
            .clone();
        assert!(matches!(ticket.status, TicketStatus::Pending));
        assert!(ticket.control_card_message_id.is_none());
        let short_code = ticket.short_code.clone().unwrap();
        assert!(captured
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains(&short_code)));

        let private = Inbound {
            anchor_message_id: None,
            chat_id: "oc_private".into(),
            chat_type: Some("p2p".into()),
            text: format!("/bridge {short_code}"),
            sender: "ou_owner".into(),
            sender_union_id: "on_owner".into(),
            sender_type: String::new(),
            message_id: "om_private".into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: vec![],
            resources: vec![],
            create_time_ms: 0,
            replayed: false,
            reattach: false,
        };
        handle_bridge_command_with_store(
            BridgeCommand::Redeem { code: short_code },
            &agent,
            &client,
            &scope,
            &private,
            &chats,
            &inbox,
            &inflight,
            &store,
            &outbox,
        );
        server_thread.join().unwrap();
        let redeemed = &store.load_state().unwrap().unwrap().tickets[&ticket.id];
        assert_eq!(redeemed.private_chat_id.as_deref(), Some("oc_private"));
        assert!(redeemed.control_card_message_id.is_some());
        assert!(redeemed.short_code.is_none());
        assert_eq!(
            crate::bridge::process_probe::probe_exact_process(&process).unwrap(),
            crate::bridge::process_probe::ExactProcessStatus::AliveExact
        );
    }

    /// 一张等待选择的 Takeover ticket:源会话 `sid-handoff` 的进程是测试进程自己(只探测、
    /// 不发信号),目标群空闲。
    struct HandoffFixture {
        agent: Agent,
        _agent_dir: tempfile::TempDir,
        root: tempfile::TempDir,
        scope: FeishuStorageScope,
        store: crate::bridge::store::BridgeStore,
        target: ScopedChatKey,
        session: summon_core::bridge::SessionKey,
    }

    fn handoff_fixture() -> HandoffFixture {
        use summon_core::bridge::{
            BridgeChoice, ExternalLifecycle, ExternalLocator, ExternalSessionRecord, ResumeProfile,
            SessionKey, TicketRequest,
        };

        let (mut agent, agent_dir) =
            crate::test_support::agent_dir("claude", "feishu:\n  operators: [ou_owner]\n");
        agent.runtime.as_mut().unwrap().resume = Some(true);
        let scope = feishu_storage_scope(&agent);
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let config = home.join(".claude");
        let project = root.path().join("project");
        let transcript = config.join("projects/demo/sid-handoff.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(&transcript, "{\"sessionId\":\"sid-handoff\"}\n").unwrap();
        let home = std::fs::canonicalize(home).unwrap();
        let config = std::fs::canonicalize(config).unwrap();
        let project = std::fs::canonicalize(project).unwrap();
        let transcript = std::fs::canonicalize(transcript).unwrap();
        let process = crate::bridge::process_probe::observe_exact_fingerprint(std::process::id())
            .unwrap()
            .expect("测试进程应可探测");
        let session = SessionKey::new(config.to_string_lossy(), "sid-handoff");
        let chat_id = format!("oc_handoff_{}", uuid::Uuid::new_v4().simple());
        let target = ScopedChatKey::new(&scope.key, &agent.name, &chat_id);
        let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
        store
            .with_state(|state| {
                state.upsert_session(ExternalSessionRecord {
                    key: session.clone(),
                    generation: 1,
                    locator: ExternalLocator {
                        key: session.clone(),
                        transcript_path: transcript.to_string_lossy().into_owned(),
                        resume_lookup_cwd: project.to_string_lossy().into_owned(),
                        current_cwd: project.to_string_lossy().into_owned(),
                        profile: ResumeProfile {
                            claude_bin: process.executable.clone(),
                            home: home.to_string_lossy().into_owned(),
                            config_dir: config.to_string_lossy().into_owned(),
                            version: "test-version".into(),
                            process_verified: true,
                            environment_verified: true,
                            config_source: summon_core::bridge::ClaudeConfigSource::Default,
                            provider: summon_core::bridge::ClaudeProvider::Default,
                            ..ResumeProfile::default()
                        },
                        revision: 1,
                    },
                    process: Some(process.clone()),
                    lifecycle: ExternalLifecycle::ActiveIdle,
                    first_seen_ms: 1,
                    last_seen_ms: 1,
                    last_applied_sequence: 1,
                    pending_prompt: None,
                });
                state
                    .reserve_ticket(TicketRequest {
                        ticket_id: "ticket-handoff".into(),
                        short_code: None,
                        target: target.clone(),
                        target_chat_type: "group".into(),
                        target_provider: summon_core::bridge::ClaudeProvider::Default,
                        requester_open_id: "ou_owner".into(),
                        requester_union_id: String::new(),
                        choices: vec![BridgeChoice {
                            id: "choice-handoff".into(),
                            session: session.clone(),
                            registry_revision: 1,
                        }],
                        issued_at_ms: 1,
                        expires_at_ms: u64::MAX,
                    })
                    .map_err(anyhow::Error::new)
            })
            .unwrap();
        HandoffFixture {
            agent,
            _agent_dir: agent_dir,
            root,
            scope,
            store,
            target,
            session,
        }
    }

    fn click_handoff(fixture: &HandoffFixture) -> CallbackDisposition {
        let card = CardActionMsg {
            chat_id: "oc_private".into(),
            message_id: "om_handoff".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "event-handoff".into(),
            value: serde_json::json!({
                "action": "bridge_handoff",
                "ticket": "ticket-handoff",
                "choice": "choice-handoff",
            }),
            wal_id: None,
            wal_replayed: false,
        };
        handle_bridge_card_action_with_stores(
            &fixture.agent,
            &FeishuClient::new("feishu.cn", String::new(), String::new(), true),
            &fixture.scope,
            &HashMap::new(),
            &crate::runtime::inbox_wal::InboxWal::new(fixture.root.path().join("inbox")),
            &crate::runtime::inflight::InFlight::new(),
            &card,
            &fixture.store,
            &crate::runtime::outbox_store::OutboxStore::new(fixture.root.path().join("outbox")),
        )
    }

    #[test]
    fn handoff_callback_durably_reserves_exact_source_and_attempt() {
        let fixture = handoff_fixture();
        click_handoff(&fixture);

        let state = fixture.store.load_state().unwrap().unwrap();
        assert!(matches!(
            state.tickets["ticket-handoff"].status,
            TicketStatus::Activating {
                mode: BridgeMode::Handoff,
                ..
            }
        ));
        assert!(matches!(
            state.slot(&fixture.target),
            Some(ChatSlot::HandoffPending(binding)) if binding.session == fixture.session
        ));
        let attempts = fixture.store.load_handoff_attempts().unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].source().session(), &fixture.session);
        assert!(matches!(
            attempts[0].phase(),
            crate::bridge::handoff::HandoffPhase::AwaitingSafeBoundary
        ));
        // 接管走完之前,这条会话不能再绑遥控:接管会在安全边界结束它的进程。
        assert!(takeover_pending_for(&fixture.store, "sid-handoff", None).unwrap());
        assert!(!takeover_pending_for(&fixture.store, "sid-other", None).unwrap());
    }

    #[test]
    fn handoff_callback_refuses_a_session_remote_controlled_elsewhere() {
        let fixture = handoff_fixture();
        crate::remote::store::RemoteStore::for_agent(&fixture.agent)
            .save(&summon_core::remote::RemoteBinding {
                session_id: "sid-handoff".into(),
                ..remote_binding("oc_remote_elsewhere", None)
            })
            .unwrap();
        assert_eq!(click_handoff(&fixture), CallbackDisposition::Terminal);
        let state = fixture.store.load_state().unwrap().unwrap();
        assert!(
            matches!(
                state.tickets["ticket-handoff"].status,
                TicketStatus::Pending
            ),
            "被遥控的会话不能预留接管"
        );
        assert!(fixture.store.load_handoff_attempts().unwrap().is_empty());
    }

    #[test]
    fn handoff_callback_refuses_a_process_remote_controlled_under_another_sid() {
        let fixture = handoff_fixture();
        crate::remote::store::RemoteStore::for_agent(&fixture.agent)
            .save(&summon_core::remote::RemoteBinding {
                session_id: "sid-before-clear".into(),
                pid: std::process::id(),
                ..remote_binding("oc_remote_elsewhere", None)
            })
            .unwrap();
        assert_eq!(click_handoff(&fixture), CallbackDisposition::Terminal);
        let state = fixture.store.load_state().unwrap().unwrap();
        assert!(
            matches!(
                state.tickets["ticket-handoff"].status,
                TicketStatus::Pending
            ),
            "要结束的进程在遥控中,sid 不同也不能预留接管"
        );
        assert!(fixture.store.load_handoff_attempts().unwrap().is_empty());
    }

    #[test]
    fn pending_takeover_is_matched_by_the_source_process_too() {
        let fixture = handoff_fixture();
        click_handoff(&fixture);
        let source_pid = fixture.store.load_handoff_attempts().unwrap()[0]
            .source()
            .process()
            .pid;
        assert!(
            takeover_pending_for(&fixture.store, "sid-after-clear", Some(source_pid)).unwrap(),
            "同一个进程上没走完的接管,sid 不同也要认出来"
        );
        assert!(
            !takeover_pending_for(&fixture.store, "sid-after-clear", Some(source_pid + 1)).unwrap()
        );
        assert!(
            !takeover_pending_for(&fixture.store, "sid-after-clear", None).unwrap(),
            "不知道 pid 时只按 sid 查"
        );
    }

    type CapturedRequests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// 本机假飞书:换 token、Bot 所在群列表里只有 `target_chat`、发消息都成功;记下每个请求。
    fn fake_feishu(
        target_chat: &str,
    ) -> (
        String,
        CapturedRequests,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        fake_feishu_with_dm_failure(target_chat, false)
    }

    fn fake_feishu_with_dm_failure(
        target_chat: &str,
        fail_dm: bool,
    ) -> (
        String,
        CapturedRequests,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let captured = CapturedRequests::default();
        let (server_stop, server_captured) = (std::sync::Arc::clone(&stop), captured.clone());
        let chats = serde_json::json!({
            "code": 0,
            "data": {"items": [{"chat_id": target_chat}], "has_more": false}
        })
        .to_string();
        let thread = std::thread::spawn(move || {
            while !server_stop.load(std::sync::atomic::Ordering::SeqCst) {
                let Some(mut request) = server
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                else {
                    continue;
                };
                let url = request.url().to_string();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                server_captured
                    .lock()
                    .unwrap()
                    .push(format!("{} {url}\n{body}", request.method()));
                let response = if url.contains("auth/v3/tenant_access_token") {
                    r#"{"code":0,"tenant_access_token":"token","expire":7200}"#.to_string()
                } else if fail_dm && url.contains("receive_id_type=open_id") {
                    r#"{"code":230002,"msg":"forbidden"}"#.to_string()
                } else if url.contains("/im/v1/chats") {
                    chats.clone()
                } else {
                    r#"{"code":0,"data":{"message_id":"om_fake"}}"#.to_string()
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
        });
        (base, captured, stop, thread)
    }

    #[test]
    fn private_query_bridge_recall_success_and_failure_have_one_result() {
        for fail_dm in [false, true] {
            let (agent, _authored) =
                crate::test_support::agent_dir("claude", "feishu:\n  operators: [ou_owner]\n");
            let root = tempfile::tempdir().unwrap();
            let target = format!("oc_private_recall_{}", uuid::Uuid::new_v4().simple());
            let (base, captured, stop, server_thread) =
                fake_feishu_with_dm_failure(&target, fail_dm);
            let fc = FeishuClient::with_test_base(base);
            let msg = Inbound {
                anchor_message_id: None,
                chat_id: target.clone(),
                chat_type: Some("group".into()),
                text: "/bridge".into(),
                sender: "ou_owner".into(),
                sender_union_id: String::new(),
                sender_type: "user".into(),
                message_id: "om_private_bridge".into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: vec![],
                resources: vec![],
                create_time_ms: 0,
                replayed: false,
                reattach: false,
            };
            let scope = feishu_storage_scope(&agent);
            let inbox = crate::runtime::inbox_wal::InboxWal::new(root.path().join("inbox"));
            let store = crate::bridge::store::BridgeStore::new(root.path().join("bridge"));
            let outbox = crate::runtime::outbox_store::OutboxStore::new(root.path().join("outbox"));
            handle_bridge_command_with_store(
                BridgeCommand::Start,
                &agent,
                &fc,
                &scope,
                &msg,
                &HashMap::new(),
                &inbox,
                &crate::runtime::inflight::InFlight::new(),
                &store,
                &outbox,
            );
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            server_thread.join().unwrap();
            let requests = captured.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r.contains("receive_id_type=open_id"))
                    .count(),
                1
            );
            let replies: Vec<_> = requests
                .iter()
                .filter(|r| r.contains("/im/v1/messages/om_private_bridge/reply"))
                .collect();
            assert_eq!(replies.len(), usize::from(fail_dm), "{requests:?}");
            if fail_dm {
                assert!(replies[0].contains("无法主动私聊"));
            }
            assert!(store
                .load_state()
                .unwrap()
                .is_none_or(|s| s.tickets.is_empty()));
        }
    }

    #[test]
    fn private_query_bridge_control_reissue_has_no_group_reply() {
        let fixture = bound_callback_fixture("bridge_context", "private-reissue");
        let target = fixture.target.chat_id.clone();
        let (base, captured, stop, server_thread) = fake_feishu(&target);
        let fc = FeishuClient::with_test_base(base);
        let msg = Inbound {
            anchor_message_id: None,
            chat_id: target,
            chat_type: Some("group".into()),
            text: "/bridge".into(),
            sender: fixture.card.operator_open_id.clone(),
            sender_union_id: String::new(),
            sender_type: "user".into(),
            message_id: "om_private_reissue".into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: vec![],
            resources: vec![],
            create_time_ms: 0,
            replayed: false,
            reattach: false,
        };
        let inbox = crate::runtime::inbox_wal::InboxWal::new(fixture.root.path().join("inbox"));
        handle_bridge_command_with_store(
            BridgeCommand::Start,
            &fixture.agent,
            &fc,
            &fixture.scope,
            &msg,
            &HashMap::new(),
            &inbox,
            &crate::runtime::inflight::InFlight::new(),
            &fixture.store,
            &fixture.outbox,
        );
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        server_thread.join().unwrap();
        let requests = captured.lock().unwrap();
        let sent: Vec<_> = requests
            .iter()
            .filter(|r| r.starts_with("POST ") && r.contains("/im/v1/messages"))
            .collect();
        assert_eq!(sent.len(), 1, "{requests:?}");
        assert!(sent[0].contains("receive_id_type=chat_id"));
        assert!(!sent[0].contains("/reply"));
        assert!(sent[0].contains("oc_private"));
        assert_eq!(
            fixture.store.load_state().unwrap().unwrap().tickets.len(),
            1
        );
    }

    /// 在私聊召回卡上点「遥控」:目标群 `target_chat`、会话 `sid`,操作者是授权的 ou_owner。
    /// 返回发出去的私聊回执请求。
    fn click_recall_cmux_remote(
        fixture: &HandoffFixture,
        sid: &str,
        target_chat: &str,
    ) -> Vec<String> {
        let token = rotate_recall_target_lifecycle(&fixture.agent, target_chat).unwrap();
        let (base, captured, stop, server) = fake_feishu(target_chat);
        let client = Arc::new(FeishuClient::with_test_base(base));
        let card = CardActionMsg {
            chat_id: "oc_private".into(),
            message_id: "om_recall_card".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "event-recall-remote".into(),
            value: serde_json::json!({
                "action": "recall_cmux_remote",
                "cli": "claude",
                "sid": sid,
                "chat_id": target_chat,
                "chat_type": "group",
                "bound_agent": fixture.agent.name,
                "bound_scope": fixture.scope.key,
                "bound_requester": "ou_owner",
                "target_lifecycle_token": token,
                "card_issued_at_ms": now_unix_ms(),
            }),
            wal_id: None,
            wal_replayed: false,
        };
        let inbox = crate::runtime::inbox_wal::InboxWal::new(fixture.root.path().join("inbox"));
        let inflight = crate::runtime::inflight::InFlight::new();
        let outbox =
            crate::runtime::outbox_store::OutboxStore::new(fixture.root.path().join("outbox"));
        handle_bridge_recall_card_action(
            &fixture.agent,
            &client,
            &fixture.scope,
            &mut HashMap::new(),
            &fixture.store,
            &card,
            LocalChatStores::new(&inbox, &inflight, &outbox),
        );
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().unwrap();
        let requests = captured.lock().unwrap().clone();
        requests
            .into_iter()
            .filter(|request| request.contains("/im/v1/messages"))
            .collect()
    }

    fn assert_refused_without_binding(
        fixture: &HandoffFixture,
        target_chat: &str,
        replies: &[String],
        expected: &str,
    ) {
        assert!(
            replies.iter().any(|reply| reply.contains(expected)),
            "私聊回执应是「{expected}」:{replies:#?}"
        );
        let remote = crate::remote::store::RemoteStore::for_agent(&fixture.agent);
        assert_eq!(remote.load(target_chat).unwrap(), None, "不写遥控记录");
        assert_eq!(
            remote.read_offset(target_chat).unwrap(),
            None,
            "不写 offset"
        );
    }

    #[test]
    fn recall_cmux_remote_callback_refuses_a_session_with_a_pending_takeover() {
        let fixture = handoff_fixture();
        click_handoff(&fixture);
        assert!(matches!(
            fixture.store.load_handoff_attempts().unwrap()[0].phase(),
            crate::bridge::handoff::HandoffPhase::AwaitingSafeBoundary
        ));
        // 接管预留在 fixture 的目标群上;遥控要绑的是另一个空闲群。
        let target_chat = format!("oc_remote_target_{}", uuid::Uuid::new_v4().simple());
        let replies = click_recall_cmux_remote(&fixture, "sid-handoff", &target_chat);
        assert_refused_without_binding(
            &fixture,
            &target_chat,
            &replies,
            ::summon_core::i18n::label(SESSION_TAKEOVER_PENDING),
        );
    }

    #[test]
    fn recall_cmux_remote_callback_refuses_when_takeover_records_are_unreadable() {
        let fixture = handoff_fixture();
        let attempts = fixture.store.handoff_attempt_dir();
        std::fs::create_dir_all(&attempts).unwrap();
        std::fs::write(attempts.join("corrupt.json"), "{").unwrap();
        let target_chat = format!("oc_remote_target_{}", uuid::Uuid::new_v4().simple());
        let replies = click_recall_cmux_remote(&fixture, "sid-handoff", &target_chat);
        assert_refused_without_binding(
            &fixture,
            &target_chat,
            &replies,
            ::summon_core::i18n::label(TAKEOVER_RECORDS_UNREADABLE),
        );
    }

    #[test]
    fn stale_choice_cancels_pending_ticket_and_releases_group_immediately() {
        use summon_core::bridge::{
            BridgeChoice, ExternalLifecycle, ExternalLocator, ExternalSessionRecord,
            ProcessFingerprint, ResumeProfile, SessionKey, TicketRequest, TicketStatus,
        };

        let (agent, _agent_dir) =
            crate::test_support::agent_dir("claude", "feishu:\n  operators: [ou_owner]\n");
        let scope = feishu_storage_scope(&agent);
        let target = ScopedChatKey::new(&scope.key, &agent.name, "oc_stale");
        let session = SessionKey::new("/tmp/config", "sid-stale");
        let process = crate::bridge::process_probe::observe_exact_fingerprint(std::process::id())
            .unwrap()
            .expect("测试进程应可探测");
        let temp = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(temp.path().join("bridge"));
        store
            .with_state(|state| {
                state.upsert_session(ExternalSessionRecord {
                    key: session.clone(),
                    generation: 1,
                    locator: ExternalLocator {
                        key: session.clone(),
                        transcript_path: "/tmp/config/sid-stale.jsonl".into(),
                        resume_lookup_cwd: "/tmp".into(),
                        current_cwd: "/tmp".into(),
                        profile: ResumeProfile {
                            claude_bin: process.executable.clone(),
                            config_dir: "/tmp/config".into(),
                            process_verified: true,
                            ..ResumeProfile::default()
                        },
                        revision: 1,
                    },
                    process: Some(ProcessFingerprint {
                        pid: process.pid,
                        start_time: process.start_time.clone(),
                        executable: process.executable.clone(),
                    }),
                    lifecycle: ExternalLifecycle::ActiveIdle,
                    first_seen_ms: 1,
                    last_seen_ms: 1,
                    last_applied_sequence: 1,
                    pending_prompt: None,
                });
                state
                    .reserve_ticket(TicketRequest {
                        ticket_id: "ticket-stale".into(),
                        short_code: None,
                        target: target.clone(),
                        target_chat_type: "group".into(),
                        target_provider: summon_core::bridge::ClaudeProvider::Default,
                        requester_open_id: "ou_owner".into(),
                        requester_union_id: String::new(),
                        choices: vec![BridgeChoice {
                            id: "choice-stale".into(),
                            session: session.clone(),
                            registry_revision: 1,
                        }],
                        issued_at_ms: 1,
                        expires_at_ms: u64::MAX,
                    })
                    .map_err(anyhow::Error::new)?;
                // 模拟飞书已收卡、daemon 在登记 message_id 前崩溃；callback 必须能先原子 claim，
                // 随后仍因 locator revision 过期而取消 ticket、释放群。
                state
                    .sessions
                    .get_mut(&session.storage_key())
                    .unwrap()
                    .locator
                    .revision = 2;
                Ok(())
            })
            .unwrap();
        let card = CardActionMsg {
            chat_id: "oc_private".into(),
            message_id: "om_control".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "event-stale".into(),
            value: serde_json::json!({
                "action": "bridge_mirror",
                "ticket": "ticket-stale",
                "choice": "choice-stale",
            }),
            wal_id: None,
            wal_replayed: false,
        };
        handle_bridge_card_action_with_stores(
            &agent,
            &FeishuClient::new("feishu.cn", String::new(), String::new(), true),
            &scope,
            &HashMap::new(),
            &crate::runtime::inbox_wal::InboxWal::new(temp.path().join("inbox")),
            &crate::runtime::inflight::InFlight::new(),
            &card,
            &store,
            &crate::runtime::outbox_store::OutboxStore::new(temp.path().join("outbox")),
        );

        let state = store.load_state().unwrap().unwrap();
        assert!(matches!(
            state.tickets["ticket-stale"].status,
            TicketStatus::Cancelled
        ));
        assert!(state.slot(&target).is_none());
    }

    #[test]
    fn bridge_operate_is_independent_from_talk_allowlist() {
        let agent: Agent = serde_yaml::from_str(
            "name: bot\nfeishu:\n  allowed_users: [ou_talk]\n  operators: [ou_operator]\n",
        )
        .unwrap();
        assert!(!agent.allows("ou_operator", ""));
        assert!(agent.can_operate("ou_operator", "", true, OperateScope::Agent));
        assert!(agent.allows("ou_talk", ""));
        assert!(!agent.can_operate("ou_talk", "", true, OperateScope::Agent));
    }

    #[test]
    fn expired_pending_slot_no_longer_freezes_normal_group_routing() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::bridge::store::BridgeStore::new(dir.path().join("bridge"));
        let target = ScopedChatKey::new("scope", "agent", "group");
        store
            .with_state(|state| {
                state.tickets.insert(
                    "ticket".into(),
                    BridgeTicket {
                        id: "ticket".into(),
                        short_code: None,
                        target: target.clone(),
                        target_chat_type: "group".into(),
                        target_provider: summon_core::bridge::ClaudeProvider::Default,
                        requester_open_id: "owner".into(),
                        requester_union_id: "union".into(),
                        control_card_message_id: None,
                        private_chat_id: None,
                        slot_revision: 1,
                        choices: vec![],
                        issued_at_ms: 1,
                        expires_at_ms: 10,
                        status: TicketStatus::Pending,
                    },
                );
                state.slots.insert(
                    target.storage_key(),
                    ChatSlot::Pending {
                        ticket_id: "ticket".into(),
                        revision: 1,
                        expires_at_ms: 10,
                    },
                );
                Ok(())
            })
            .unwrap();

        assert_eq!(bridge_slot_message_at(&store, &target, 11).unwrap(), None);
        let state = store.load_state().unwrap().unwrap();
        assert!(matches!(
            state.tickets["ticket"].status,
            TicketStatus::Expired
        ));
        assert!(state.slot(&target).is_none());
    }

    #[test]
    fn historical_meta_corruption_makes_bridge_occupancy_fail_closed() {
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chats.join("legacy.meta"), "not-a-valid-chat-meta\n").unwrap();

        assert!(historical_chat_sessions_strict(&agent, "oc_target").is_err());
        std::fs::write(
            chats.join("legacy.meta"),
            "chat_id=oc_target\nchat_type=group\n",
        )
        .unwrap();
        assert_eq!(
            historical_chat_sessions_strict(&agent, "oc_target").unwrap(),
            ["legacy"]
        );
    }

    #[test]
    fn unrelated_orphan_resume_does_not_block_every_bridge_group() {
        let (agent, _dir) = crate::test_support::agent_dir("claude", "");
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chats.join("unknown.session"), "session_id: sid\n").unwrap();

        assert!(historical_chat_sessions_strict(&agent, "oc_target")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ticket_union_id_authorizes_callback_when_contact_lookup_is_unavailable() {
        let agent: Agent = serde_yaml::from_str(
            "name: bot\nfeishu:\n  allowed_users: []\n  operators: [on_stable]\n",
        )
        .unwrap();
        let mut ticket = BridgeTicket {
            id: "ticket".into(),
            short_code: None,
            target: ScopedChatKey::new("scope", "bot", "group"),
            target_chat_type: "group".into(),
            target_provider: summon_core::bridge::ClaudeProvider::Default,
            requester_open_id: "ou_requester".into(),
            requester_union_id: "on_stable".into(),
            control_card_message_id: Some("om_card".into()),
            private_chat_id: Some("oc_private".into()),
            slot_revision: 1,
            choices: vec![],
            issued_at_ms: 1,
            expires_at_ms: 2,
            status: TicketStatus::Pending,
        };
        let callback_open_id = "ou_requester";
        let trusted_union = if ticket.requester_open_id == callback_open_id {
            ticket.requester_union_id.as_str()
        } else {
            ""
        };
        assert!(agent.can_operate(callback_open_id, trusted_union, true, OperateScope::Agent));
        ticket.requester_open_id = "ou_other".into();
        let untrusted_union = if ticket.requester_open_id == callback_open_id {
            ticket.requester_union_id.as_str()
        } else {
            ""
        };
        assert!(!agent.can_operate(callback_open_id, untrusted_union, true, OperateScope::Agent));
    }

    #[test]
    fn context_callback_event_id_is_stable_opaque_and_scoped() {
        let card = CardActionMsg {
            chat_id: "private".into(),
            message_id: "message".into(),
            operator_open_id: "owner".into(),
            event_id: "callback-event-id".into(),
            value: serde_json::json!({}),
            wal_id: None,
            wal_replayed: false,
        };
        let first = context_callback_receipt_id(&card, "ticket", "choice");
        let second = context_callback_receipt_id(&card, "ticket", "choice");
        assert_eq!(first, second);
        assert_ne!(first, context_callback_receipt_id(&card, "ticket", "other"));
        assert!(!first.unwrap().contains("callback-event-id"));

        let mut missing = card;
        missing.event_id.clear();
        assert!(context_callback_receipt_id(&missing, "ticket", "choice").is_none());
    }

    #[test]
    fn context_requires_verified_active_epoch_but_stop_identity_remains_available() {
        use summon_core::bridge::{BridgeChoice, ExternalLifecycle};
        let key = summon_core::bridge::SessionKey::new("/tmp/config", "sid");
        let target = ScopedChatKey::new("scope", "agent", "group");
        let binding = summon_core::bridge::ChatBinding {
            id: "binding".into(),
            target: target.clone(),
            target_chat_type: "group".into(),
            session: key.clone(),
            mode: BridgeMode::Mirror,
            actor_open_id: "owner".into(),
            created_at_ms: 1,
            revision: 7,
        };
        let mut state = BridgeState::default();
        state.sessions.insert(
            key.storage_key(),
            summon_core::bridge::ExternalSessionRecord {
                key: key.clone(),
                generation: 2,
                locator: summon_core::bridge::ExternalLocator {
                    key: key.clone(),
                    transcript_path: "/tmp/config/sid.jsonl".into(),
                    resume_lookup_cwd: "/tmp/project".into(),
                    current_cwd: "/tmp/project".into(),
                    profile: summon_core::bridge::ResumeProfile {
                        claude_bin: "/opt/claude".into(),
                        home: "/tmp/home".into(),
                        config_dir: "/tmp/config".into(),
                        process_verified: false,
                        ..Default::default()
                    },
                    revision: 9,
                },
                process: None,
                lifecycle: ExternalLifecycle::Unverified,
                first_seen_ms: 1,
                last_seen_ms: 2,
                last_applied_sequence: 2,
                pending_prompt: None,
            },
        );
        state
            .slots
            .insert(target.storage_key(), ChatSlot::MirrorBound(binding.clone()));
        state.session_leases.insert(
            key.storage_key(),
            summon_core::bridge::SessionLease {
                binding_id: binding.id.clone(),
                target: target.clone(),
            },
        );
        state.tickets.insert(
            "ticket".into(),
            BridgeTicket {
                id: "ticket".into(),
                short_code: None,
                target,
                target_chat_type: "group".into(),
                target_provider: summon_core::bridge::ClaudeProvider::Default,
                requester_open_id: "owner".into(),
                requester_union_id: "union".into(),
                control_card_message_id: Some("message".into()),
                private_chat_id: Some("private".into()),
                slot_revision: 1,
                choices: vec![BridgeChoice {
                    id: "choice".into(),
                    session: key,
                    registry_revision: 9,
                }],
                issued_at_ms: 1,
                expires_at_ms: 2,
                status: TicketStatus::Committed {
                    binding_id: "binding".into(),
                    choice_id: "choice".into(),
                    mode: BridgeMode::Mirror,
                },
            },
        );
        let card = CardActionMsg {
            chat_id: "private".into(),
            message_id: "message".into(),
            operator_open_id: "owner".into(),
            event_id: "event".into(),
            value: serde_json::json!({}),
            wal_id: None,
            wal_replayed: false,
        };
        assert!(bound_mirror_action(&state, "scope", "agent", "ticket", "choice", &card).is_ok());
        assert!(active_verified_bound_mirror_action(
            &state, "scope", "agent", "ticket", "choice", &card
        )
        .is_err());
    }
}
