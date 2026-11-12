//! 入站 WAL(write-ahead log):每条入站消息在进入「易失内存通道」之前先**原子落盘**,daemon
//! 意外被杀(SIGKILL)重启后据此列出「收到了但没处理完」的消息。真人入站按 started 状态决定
//! 自动重注入或恢复卡确认；定时器合成入站始终自动重注入，因为 schedule checkpoint 已把这条
//! WAL 当成唯一交付事实，不能依赖一次易失 channel send。
//!
//! dir-as-state:一文件一消息,文件所在目录即状态。
//! - `pending/<agent>/<message_id>.json`:收到、待处理**或处理中**——记录内 `started` 布尔区分二者
//!   (`false`=排队未开跑;`true`=chat 线程已取出、进入 handle_task)。优雅重启据此按**消息粒度**
//!   决定:未开跑的重注入(不丢),已开跑的不静默自动重放(claude 的存活 tmux 里原轮可能已跑完,
//!   重放=副作用双执行 → 走恢复卡由用户确认)。文件始终留在 `pending/`,崩溃扫描仍能列全两类。
//! - `recovery/active/<batch_id>.json`:恢复卡的 durable 权威清单。卡片 outbox 成功落盘后才从
//!   pending 移除原记录；恢复点击先提交确定性 replay claim，再进入易失 dispatcher。
//! - `consumed/<agent>/<message_id>.json`:本轮已处理完,留作排查、可按龄清理。
//!
//! 复用 outbox 的 `tmp+rename` 原子写法(同文件系统内 rename 原子,消费者读不到半截文件);
//! 键 = `message_id`,故重复入站幂等覆盖、且天然给出**去重键**(重放时按它判是否处理过)。

use crate::runtime::daemon::Inbound;
use crate::runtime::fsio::{atomic_write, atomic_write_mode};
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const RECOVERY_SCHEMA_VERSION: u32 = 1;
static RECOVERY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// 定时器合成入站的可重放载荷。独立于 `DispatchMsg` 序列化，避免把整个 daemon 内存消息面变成
/// 持久协议；`fire_id` 同时是 WAL 键与 chat task message_id。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScheduledInbox {
    pub(crate) chat_id: String,
    pub(crate) chat_type: String,
    pub(crate) prompt: String,
    pub(crate) fire_id: String,
    #[serde(default)]
    pub(crate) output: summon_core::ScheduleOutput,
    #[serde(default)]
    pub(crate) context: summon_core::ScheduleContext,
}

/// sentinel 合成入站的可重放载荷。`fire_id` 用**真实触发消息 id**(owner 亲手发的那条,
/// bot 进群后可 react/回复),同时是 WAL 键与 chat task message_id——三者一致才能让
/// mark_started/mark_consumed 正常销账。who 三元组是触发人(owner)的真实身份,重放时原样带回。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SentinelInbox {
    pub(crate) chat_id: String,
    pub(crate) chat_type: String,
    pub(crate) prompt: String,
    pub(crate) fire_id: String,
    pub(crate) who: String,
    pub(crate) who_id: String,
    pub(crate) who_union_id: String,
    /// `Some` = 上下文提醒卡上点的 Handoff:这一轮是 `/handoff <备注>`(不带目标)的写交接轮,
    /// 值是输入框里的接手重点。重放时原样带回,轮末照常交接。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) handoff_note: Option<String>,
}

impl SentinelInbox {
    /// 落盘之后交给 dispatcher 的那条消息。首次投递与启动恢复重投都从这里构造,载荷的每个字段
    /// 原样带过去。
    pub(crate) fn fire(&self) -> crate::runtime::daemon::DispatchMsg {
        crate::runtime::daemon::DispatchMsg::SentinelFire {
            chat_id: self.chat_id.clone(),
            chat_type: self.chat_type.clone(),
            prompt: self.prompt.clone(),
            fire_id: self.fire_id.clone(),
            who: self.who.clone(),
            who_id: self.who_id.clone(),
            who_union_id: self.who_union_id.clone(),
            handoff_note: self.handoff_note.clone(),
        }
    }
}

/// [`InboxWal::recorded_sentinel`] 查到的那条代投:`consumed` = 那一轮已经跑完。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedSentinel {
    pub(crate) consumed: bool,
    pub(crate) payload: Option<SentinelInbox>,
}

/// 定时 fire 写 WAL 的幂等结果。`Pending`/`Consumed` 都表示这个逻辑 fire 已有持久事实，
/// scheduler 只需补 checkpoint，绝不能再次路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduledAppend {
    Inserted,
    Pending,
    Consumed,
}

/// `mark_started` 的 durable 结果。只有 `Marked` 授权本次易失 worker 执行；其余状态都是
/// 可观测 no-op，不能被调用方误当成“已成功启动”。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkStartedOutcome {
    Marked,
    Disabled,
    AlreadyStarted,
    AlreadyConsumed,
    Missing,
    EmptyMessageId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkConsumedOutcome {
    Consumed,
    AlreadyConsumed,
    Missing,
    EmptyMessageId,
}

/// 一条 WAL 记录:入站消息本体 + 路由所需的 `agent` 名。`Inbound` 不带 app_id,重放时经 agent 名
/// 走该 agent 的 dispatcher 重新注入(见 Core 的 agent_routes)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct InboxRecord {
    pub(crate) agent: String,
    /// 入站被 daemon 接受时冻结的飞书 app scope。旧 schema 为空，冷启不得猜成当前 app。
    #[serde(default)]
    pub(crate) delivery_scope: String,
    pub(crate) inbound: Inbound,
    /// `Some` 表示 schedule 合成入站。旧 WAL 没有本字段，按真人消息兼容读取。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scheduled: Option<ScheduledInbox>,
    /// `Some` 表示 sentinel 合成入站(owner 显式召唤)。与 scheduled 互斥,有各自恢复协议。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sentinel: Option<SentinelInbox>,
    /// 消息粒度「已开跑」标记:chat 线程取出本消息、进 `handle_task` 前经 `mark_started` 置 true。
    /// 优雅重启据此区分「排队未开跑」(重注入不丢)与「已开跑」(不静默自动重放)。
    /// `#[serde(default)]`:本字段之前落盘的旧记录无它 → 反序列化按 false(按「未开跑」处理)。
    #[serde(default)]
    pub(crate) started: bool,
    #[serde(default)]
    pub(crate) rejected_disabled: bool,
    /// `/new` 已确认丢弃的旧会话排队任务。
    #[serde(default)]
    pub(crate) superseded_by_new: bool,
    /// 恢复卡重放使用新的确定性 message_id；这里保留最初的飞书 id，最终 consumed 预热时
    /// 同时封住原消息，避免 daemon 重启后网络重投绕过用户显式恢复决策。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) recovery_origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) recovery_batch_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub(crate) enum RecoveryItemState {
    Awaiting,
    Claimed { replay_id: String },
    Completed,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RecoveryBatchItem {
    pub(crate) record: InboxRecord,
    pub(crate) preview: String,
    pub(crate) state: RecoveryItemState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RecoveryBatch {
    schema_version: u32,
    pub(crate) id: String,
    pub(crate) outbox_id: String,
    pub(crate) scope: String,
    pub(crate) agent: String,
    pub(crate) chat_id: String,
    pub(crate) chat_type: String,
    pub(crate) stage: Option<String>,
    pub(crate) items: Vec<RecoveryBatchItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) card_message_id: Option<String>,
    pub(crate) created_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompletedRecoveryBatch {
    schema_version: u32,
    id: String,
    outbox_id: String,
    scope: String,
    agent: String,
    chat_id: String,
    card_message_id: Option<String>,
    #[serde(default)]
    source_message_ids: Vec<String>,
    completed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AbandonedInboxRecord {
    schema_version: u32,
    agent: String,
    delivery_scope: String,
    message_id: String,
    reason: String,
    abandoned_at_ms: u64,
}

struct RecoveryRecordOnDisk {
    record: InboxRecord,
    /// scoped 优先；仅有 legacy 时就地使用 legacy，避免迁移过程中制造两个 started 状态不同的副本。
    path: PathBuf,
    /// scoped/legacy 完全相同时保留待删除的 legacy 副本。写 scoped 前先删它，任一崩溃窗口
    /// 至少还留一份旧状态，下一次冷启仍会 fail-closed。
    duplicate_legacy: Option<PathBuf>,
}

impl RecoveryBatch {
    pub(crate) fn claimed_ids(&self) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|item| match &item.state {
                // Claimed 只表示 replay 已获准/排队，按钮仍可点；重复点击由 claim_recovery
                // 返回 AlreadyClaimed 去重。只有已完成执行才在卡片上置灰。
                RecoveryItemState::Completed => Some(item.record.inbound.message_id.clone()),
                RecoveryItemState::Awaiting
                | RecoveryItemState::Claimed { .. }
                | RecoveryItemState::Dismissed => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RecoveryClaimOutcome {
    Claimed(Box<InboxRecord>),
    AlreadyClaimed,
    NotAwaiting,
}

/// 入站 WAL 句柄,根目录通常是 `~/.summon/run/inbox/`。
pub(crate) struct InboxWal {
    root: PathBuf,
}

impl InboxWal {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn agent_enabled(&self, agent: &str) -> anyhow::Result<bool> {
        super::agent_availability::AgentAvailability::lock(&self.root, agent)?.enabled()
    }

    pub(crate) fn reject_disabled(
        &self,
        agent: &str,
        delivery_scope: &str,
        message_id: &str,
    ) -> anyhow::Result<bool> {
        let gate = super::agent_availability::AgentAvailability::lock(&self.root, agent)?;
        let pending =
            self.read_recovery_record_variants(&self.pending_dir(), agent, message_id, "pending")?;
        let consumed = self.read_recovery_record_variants(
            &self.consumed_dir(),
            agent,
            message_id,
            "consumed",
        )?;
        anyhow::ensure!(
            pending.is_none() || consumed.is_none(),
            summon_i18n::format!("r93bf4ba646")
        );
        let Some(mut pending) = pending else {
            if let Some(consumed) = consumed {
                validate_wal_transition_identity(
                    &consumed.record,
                    agent,
                    delivery_scope,
                    message_id,
                )?;
                return Ok(consumed.record.rejected_disabled);
            }
            return Ok(false);
        };
        validate_wal_transition_identity(&pending.record, agent, delivery_scope, message_id)?;
        self.reject_disabled_record(&gate, &mut pending)
    }

    fn reject_disabled_record(
        &self,
        gate: &super::agent_availability::AgentAvailability,
        pending: &mut RecoveryRecordOnDisk,
    ) -> anyhow::Result<bool> {
        use summon_core::outbox::{agent_disabled_delivery_id, OutboxEvent, OutboxKind};
        if pending.record.started {
            return Ok(false);
        }
        if !pending.record.rejected_disabled && gate.enabled()? {
            return Ok(false);
        }
        if let Some(legacy) = pending.duplicate_legacy.take() {
            std::fs::remove_file(legacy)?;
        }
        if !pending.record.rejected_disabled {
            pending.record.rejected_disabled = true;
            atomic_write_mode(
                &pending.path,
                &serde_json::to_vec_pretty(&pending.record)?,
                0o600,
            )?;
        }
        let record = &pending.record;
        let outbox = super::outbox_store::OutboxStore::new(
            self.root
                .parent()
                .context(summon_i18n::text!("rfcc4fd4f2c"))?
                .join("outbox"),
        );
        let id = agent_disabled_delivery_id(
            &record.agent,
            &record.delivery_scope,
            &record.inbound.message_id,
        );
        let existing = outbox.find(&id)?;
        if existing.is_none() {
            let text = match &record.scheduled {
                Some(schedule) => summon_i18n::format!(
                    "rd67a08c007",
                    record.agent,
                    summon_core::schedule_id_of_fire(&schedule.fire_id)
                ),
                None => summon_i18n::format!("r87fb6cf0ea", record.agent),
            };
            let created_ms = record.inbound.create_time_ms.max(0) as u64;
            outbox.enqueue_once(&OutboxEvent {
                id,
                agent: record.agent.clone(),
                delivery_scope: record.delivery_scope.clone(),
                created_ms,
                attempts: 0,
                next_attempt_ms: created_ms,
                last_error: None,
                kind: OutboxKind::AgentDisabled {
                    chat_id: record.inbound.chat_id.clone(),
                    chat_type: record
                        .inbound
                        .chat_type
                        .clone()
                        .unwrap_or_else(|| "p2p".into()),
                    rendered_card: summon_core::feishu::card_markdown_v2(&text),
                },
            })?;
        }
        let destination = self.consumed_path(&record.agent, &record.inbound.message_id);
        std::fs::create_dir_all(
            destination
                .parent()
                .context(summon_i18n::text!("r58dfd06567"))?,
        )?;
        std::fs::rename(&pending.path, &destination)?;
        if let Some(batch_id) = &record.recovery_batch_id {
            self.complete_recovery_claim(batch_id, &record.inbound.message_id)?;
        }
        Ok(true)
    }

    fn pending_dir(&self) -> PathBuf {
        self.root.join("pending")
    }

    fn consumed_dir(&self) -> PathBuf {
        self.root.join("consumed")
    }

    fn recovery_dir(&self) -> PathBuf {
        self.root.join("recovery").join("active")
    }

    fn recalled_dir(&self) -> PathBuf {
        self.root.join("recalled")
    }

    fn pending_path(&self, agent: &str, message_id: &str) -> PathBuf {
        self.pending_dir()
            .join(agent_bucket(agent))
            .join(format!("{}.json", sanitize_id(message_id)))
    }

    fn consumed_path(&self, agent: &str, message_id: &str) -> PathBuf {
        self.consumed_dir()
            .join(agent_bucket(agent))
            .join(format!("{}.json", sanitize_id(message_id)))
    }

    fn recalled_path(&self, agent: &str, message_id: &str) -> PathBuf {
        self.recalled_dir()
            .join(agent_bucket(agent))
            .join(format!("{}.json", sanitize_id(message_id)))
    }

    fn recovery_path(&self, batch_id: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            valid_recovery_id(batch_id),
            summon_i18n::format!("r38e6e59400")
        );
        Ok(self.recovery_dir().join(format!("{batch_id}.json")))
    }

    fn completed_recovery_path(&self, batch_id: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            valid_recovery_id(batch_id),
            summon_i18n::format!("r1e6d7a9ba3")
        );
        Ok(self
            .root
            .join("recovery")
            .join("completed")
            .join(format!("{batch_id}.json")))
    }

    fn existing_pending_path(
        &self,
        agent: &str,
        message_id: &str,
    ) -> anyhow::Result<Option<PathBuf>> {
        let scoped = self.pending_path(agent, message_id);
        if scoped.exists() {
            return Ok(Some(scoped));
        }
        // 兼容升级前的 flat WAL；只有记录内 agent 精确一致才允许接管。
        let legacy = self
            .pending_dir()
            .join(format!("{}.json", sanitize_id(message_id)));
        match read_record(&legacy)? {
            Some(record) if record.agent == agent => Ok(Some(legacy)),
            Some(_) => Ok(None),
            None => Ok(None),
        }
    }

    /// 原子落一条入站到 `pending/`(键 = message_id,重复入站幂等覆盖)。失败返回 Err，调用方
    /// 必须停止分派；否则消息可能先执行、started 事实却从未 durable。空 message_id 不入。
    pub(crate) fn append(
        &self,
        agent: &str,
        delivery_scope: &str,
        inbound: &Inbound,
    ) -> anyhow::Result<()> {
        if agent.trim().is_empty()
            || delivery_scope.trim().is_empty()
            || inbound.message_id.trim().is_empty()
        {
            anyhow::bail!(summon_i18n::format!("rfe7f9af3a7"));
        }
        let mut rec = InboxRecord {
            agent: agent.to_string(),
            delivery_scope: delivery_scope.to_string(),
            inbound: inbound.clone(),
            scheduled: None,
            sentinel: None,
            // 每次入站(含重投幂等覆盖 / 优雅重启重注入)都当「刚收到、未开跑」——开跑标记由
            // chat 线程取出后 mark_started 打,避免重注入的消息带着旧 started=true 被误判。
            started: false,
            rejected_disabled: false,
            superseded_by_new: false,
            recovery_origin: None,
            recovery_batch_id: None,
        };
        let existing_path = self.existing_pending_path(agent, &inbound.message_id)?;
        if let Some(path) = existing_path.as_ref() {
            if let Some(existing) = read_record(path)? {
                if existing.rejected_disabled || existing.superseded_by_new {
                    validate_wal_transition_identity(
                        &existing,
                        agent,
                        delivery_scope,
                        &inbound.message_id,
                    )?;
                    return Ok(());
                }
                // durable recovery claim 在 dispatcher append 前已存在；保留其来源/批次，使
                // crash-before-start 能自动续送，而不是重新向用户发一张恢复卡。
                if existing.recovery_origin.is_some() {
                    anyhow::ensure!(
                        existing.inbound.chat_id == inbound.chat_id,
                        summon_i18n::format!("r0505d47a89")
                    );
                    anyhow::ensure!(
                        existing.delivery_scope == delivery_scope,
                        summon_i18n::format!("rffcfd482b0")
                    );
                    rec.recovery_origin = existing.recovery_origin;
                    rec.recovery_batch_id = existing.recovery_batch_id;
                }
            }
        }
        let json = serde_json::to_string_pretty(&rec)?;
        let scoped = self.pending_path(agent, &inbound.message_id);
        atomic_write(&scoped, json.as_bytes())?;
        if let Some(legacy) = existing_path.filter(|path| *path != scoped) {
            // scoped copy 已 fsync+rename 后才删升级前 flat 文件；任一窗口最多留下两份相同记录，
            // scan_pending 会按完整 identity 去重，不会生成第二个 recovery item。
            let _ = std::fs::remove_file(legacy);
        }
        Ok(())
    }

    /// 把定时 fire 在进入 dispatcher 内存 channel 前先落到 WAL。相同 fire_id 已在 pending 或
    /// consumed 时只返回既有状态，绝不覆盖记录、绝不把 `started=true` 重置成 false。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_scheduled(
        &self,
        agent: &str,
        delivery_scope: &str,
        chat_id: &str,
        chat_type: &str,
        prompt: &str,
        output: summon_core::ScheduleOutput,
        context: summon_core::ScheduleContext,
        fire_id: &str,
        create_time_ms: i64,
    ) -> anyhow::Result<ScheduledAppend> {
        if fire_id.trim().is_empty() {
            anyhow::bail!(summon_i18n::format!("r7748344245"));
        }
        let scheduled = ScheduledInbox {
            chat_id: chat_id.to_string(),
            chat_type: chat_type.to_string(),
            prompt: prompt.to_string(),
            fire_id: fire_id.to_string(),
            output,
            context,
        };
        let mut pending = self.read_recovery_record_variants(
            &self.pending_dir(),
            agent,
            fire_id,
            "scheduled pending",
        )?;
        let mut consumed = self.read_recovery_record_variants(
            &self.consumed_dir(),
            agent,
            fire_id,
            "scheduled consumed",
        )?;
        anyhow::ensure!(
            pending.is_none() || consumed.is_none(),
            summon_i18n::format!("rbb7f024ed2")
        );
        if let Some(existing) = pending.as_mut() {
            validate_scheduled_record(&existing.record, agent, delivery_scope, &scheduled)?;
            if let Some(legacy) = existing.duplicate_legacy.take() {
                std::fs::remove_file(&legacy)
                    .with_context(|| summon_i18n::format!("r0fe07f2583"))?;
            }
            return Ok(ScheduledAppend::Pending);
        }
        if let Some(existing) = consumed.as_mut() {
            validate_scheduled_record(&existing.record, agent, delivery_scope, &scheduled)?;
            if let Some(legacy) = existing.duplicate_legacy.take() {
                std::fs::remove_file(&legacy)
                    .with_context(|| summon_i18n::format!("r2f8d6ec7af"))?;
            }
            return Ok(ScheduledAppend::Consumed);
        }
        let inbound = Inbound {
            // 定时 fire 是合成的,飞书上没有对应消息可锚。
            anchor_message_id: None,
            chat_id: chat_id.to_string(),
            chat_type: Some(chat_type.to_string()),
            text: prompt.to_string(),
            sender: String::new(),
            sender_union_id: String::new(),
            sender_type: String::new(),
            message_id: fire_id.to_string(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms,
            // schedule 合成入站本就自动重注入且 fire_id 幂等,不是重放语义。
            replayed: false,
            reattach: false,
        };
        let rec = InboxRecord {
            agent: agent.to_string(),
            delivery_scope: delivery_scope.to_string(),
            inbound,
            scheduled: Some(scheduled),
            sentinel: None,
            started: false,
            rejected_disabled: false,
            superseded_by_new: false,
            recovery_origin: None,
            recovery_batch_id: None,
        };
        let json = serde_json::to_string_pretty(&rec)?;
        atomic_write(&self.pending_path(agent, fire_id), json.as_bytes())?;
        Ok(ScheduledAppend::Inserted)
    }

    /// 把 sentinel fire 在进入 dispatcher 内存 channel 前先落到 WAL(durable-before-route,
    /// 与 [`Self::append_scheduled`] 同构)。相同 fire_id 已在 pending/consumed 时只返回既有状态。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_sentinel(
        &self,
        agent: &str,
        delivery_scope: &str,
        payload: &SentinelInbox,
        create_time_ms: i64,
    ) -> anyhow::Result<ScheduledAppend> {
        let fire_id = payload.fire_id.as_str();
        if fire_id.trim().is_empty() {
            anyhow::bail!(summon_i18n::format!("r4f86d0d672"));
        }
        let mut pending = self.read_recovery_record_variants(
            &self.pending_dir(),
            agent,
            fire_id,
            "sentinel pending",
        )?;
        let mut consumed = self.read_recovery_record_variants(
            &self.consumed_dir(),
            agent,
            fire_id,
            "sentinel consumed",
        )?;
        anyhow::ensure!(
            pending.is_none() || consumed.is_none(),
            summon_i18n::format!("r1e406ba38c")
        );
        if let Some(existing) = pending.as_mut() {
            validate_sentinel_record(&existing.record, agent, delivery_scope, payload)?;
            if let Some(legacy) = existing.duplicate_legacy.take() {
                std::fs::remove_file(&legacy)
                    .with_context(|| summon_i18n::format!("rb885bf7fb7"))?;
            }
            return Ok(ScheduledAppend::Pending);
        }
        if let Some(existing) = consumed.as_mut() {
            validate_sentinel_record(&existing.record, agent, delivery_scope, payload)?;
            if let Some(legacy) = existing.duplicate_legacy.take() {
                std::fs::remove_file(&legacy)
                    .with_context(|| summon_i18n::format!("r193f5efdc8"))?;
            }
            return Ok(ScheduledAppend::Consumed);
        }
        let inbound = Inbound {
            // sentinel 召唤同样是合成的,没有对应的飞书消息。
            anchor_message_id: None,
            chat_id: payload.chat_id.clone(),
            chat_type: Some(payload.chat_type.clone()),
            text: payload.prompt.clone(),
            sender: payload.who_id.clone(),
            sender_union_id: payload.who_union_id.clone(),
            sender_type: "user".to_string(),
            message_id: fire_id.to_string(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms,
            // sentinel 合成入站自动重注入且 fire_id 幂等,不是重放语义。
            replayed: false,
            reattach: false,
        };
        let rec = InboxRecord {
            agent: agent.to_string(),
            delivery_scope: delivery_scope.to_string(),
            inbound,
            scheduled: None,
            sentinel: Some(payload.clone()),
            started: false,
            rejected_disabled: false,
            superseded_by_new: false,
            recovery_origin: None,
            recovery_batch_id: None,
        };
        let json = serde_json::to_string_pretty(&rec)?;
        atomic_write(&self.pending_path(agent, fire_id), json.as_bytes())?;
        Ok(ScheduledAppend::Inserted)
    }

    /// 这个 sentinel `fire_id` 已有的持久事实:还在待跑,还是已经跑完。只认键、不比载荷:同一次
    /// 代投重新提交时载荷可能不同(发起人的名字缓存冷了之类),那仍是同一次。
    pub(crate) fn recorded_sentinel(
        &self,
        agent: &str,
        fire_id: &str,
    ) -> anyhow::Result<Option<RecordedSentinel>> {
        if let Some(pending) = self.read_recovery_record_variants(
            &self.pending_dir(),
            agent,
            fire_id,
            "sentinel pending",
        )? {
            return Ok(Some(RecordedSentinel {
                consumed: false,
                payload: pending.record.sentinel,
            }));
        }
        Ok(self
            .read_recovery_record_variants(
                &self.consumed_dir(),
                agent,
                fire_id,
                "sentinel consumed",
            )?
            .map(|consumed| RecordedSentinel {
                consumed: true,
                payload: consumed.record.sentinel,
            }))
    }

    /// sentinel 版本的路由失败撤销(语义同 [`Self::discard_unrouted_scheduled`])。
    pub(crate) fn discard_unrouted_sentinel(
        &self,
        agent: &str,
        fire_id: &str,
    ) -> anyhow::Result<()> {
        let Some(path) = self.existing_pending_path(agent, fire_id)? else {
            return Ok(());
        };
        let Some(rec) = read_record(&path)? else {
            return Ok(());
        };
        if rec.sentinel.is_none() || rec.started {
            anyhow::bail!(summon_i18n::format!("rba4ef5c3a6"));
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    /// route 明确失败(`Core` 无该 agent route / receiver 已断)时撤销本次新插入。只允许删尚未
    /// started 的 scheduled 记录；若状态已推进则 fail-closed 保留，交给启动恢复。
    pub(crate) fn discard_unrouted_scheduled(
        &self,
        agent: &str,
        fire_id: &str,
    ) -> anyhow::Result<()> {
        let Some(path) = self.existing_pending_path(agent, fire_id)? else {
            return Ok(());
        };
        let Some(rec) = read_record(&path)? else {
            return Ok(());
        };
        if rec.scheduled.is_none() || rec.started {
            anyhow::bail!(summon_i18n::format!("r97b5e73318"));
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    /// chat worker 在调用 `handle_task` / 启动 CLI 前的 write-ahead 屏障。读、解析、identity
    /// 校验、原子写任一步失败都返回 Err；调用方必须 fail-closed，不得执行本轮。
    pub(crate) fn mark_started(
        &self,
        agent: &str,
        delivery_scope: &str,
        message_id: &str,
    ) -> anyhow::Result<MarkStartedOutcome> {
        let gate = super::agent_availability::AgentAvailability::lock(&self.root, agent)?;
        anyhow::ensure!(
            !agent.trim().is_empty(),
            summon_i18n::format!("rd3ac5d4a7c")
        );
        anyhow::ensure!(
            !delivery_scope.trim().is_empty(),
            summon_i18n::format!("rb81915cd12")
        );
        if message_id.trim().is_empty() {
            return Ok(MarkStartedOutcome::EmptyMessageId);
        }
        let pending =
            self.read_recovery_record_variants(&self.pending_dir(), agent, message_id, "pending")?;
        let consumed = self.read_recovery_record_variants(
            &self.consumed_dir(),
            agent,
            message_id,
            "consumed",
        )?;
        anyhow::ensure!(
            pending.is_none() || consumed.is_none(),
            summon_i18n::format!("r93bf4ba646")
        );
        let Some(mut pending) = pending else {
            if let Some(consumed) = consumed {
                validate_wal_transition_identity(
                    &consumed.record,
                    agent,
                    delivery_scope,
                    message_id,
                )?;
                return Ok(MarkStartedOutcome::AlreadyConsumed);
            }
            return Ok(MarkStartedOutcome::Missing);
        };
        validate_wal_transition_identity(&pending.record, agent, delivery_scope, message_id)?;
        if pending.record.superseded_by_new {
            self.discard_superseded(agent, delivery_scope, message_id)?;
            return Ok(MarkStartedOutcome::AlreadyConsumed);
        }
        if self.reject_disabled_record(&gate, &mut pending)? {
            return Ok(MarkStartedOutcome::Disabled);
        }
        if let Some(legacy) = pending.duplicate_legacy.take() {
            // 两份相同才走到这里。先删 legacy 再推进 scoped：崩溃窗口最多退回未 started，
            // 不会留下 true/false 双副本让冷启静默重放。
            std::fs::remove_file(&legacy).with_context(|| summon_i18n::format!("r4bfdb1ece3"))?;
        }
        if pending.record.started {
            return Ok(MarkStartedOutcome::AlreadyStarted);
        }
        pending.record.started = true;
        atomic_write_mode(
            &pending.path,
            &serde_json::to_vec_pretty(&pending.record)?,
            0o600,
        )?;
        Ok(MarkStartedOutcome::Marked)
    }

    /// 只销账已经 durable `started=true` 的记录。供正常 Task 与 `!` 合并重跑共用；缺失、旧 scope、
    /// scoped/legacy 冲突都显式返回，避免“执行完成”被误写成不可验证的 best-effort。
    pub(crate) fn mark_consumed_after_start(
        &self,
        agent: &str,
        delivery_scope: &str,
        message_id: &str,
    ) -> anyhow::Result<MarkConsumedOutcome> {
        self.settle_pending(agent, delivery_scope, message_id, true, true)
    }

    /// `/new` 取代的排队任务进入 consumed，冷启恢复时也保持终态。
    pub(crate) fn discard_superseded(
        &self,
        agent: &str,
        delivery_scope: &str,
        message_id: &str,
    ) -> anyhow::Result<MarkConsumedOutcome> {
        self.settle_pending(agent, delivery_scope, message_id, false, true)
    }

    fn settle_pending(
        &self,
        agent: &str,
        delivery_scope: &str,
        message_id: &str,
        require_started: bool,
        complete_claim: bool,
    ) -> anyhow::Result<MarkConsumedOutcome> {
        anyhow::ensure!(
            !agent.trim().is_empty(),
            summon_i18n::format!("r5b891560af")
        );
        anyhow::ensure!(
            !delivery_scope.trim().is_empty(),
            summon_i18n::format!("r98ec3a6542")
        );
        if message_id.trim().is_empty() {
            return Ok(MarkConsumedOutcome::EmptyMessageId);
        }
        let pending =
            self.read_recovery_record_variants(&self.pending_dir(), agent, message_id, "pending")?;
        let consumed = self.read_recovery_record_variants(
            &self.consumed_dir(),
            agent,
            message_id,
            "consumed",
        )?;
        anyhow::ensure!(
            pending.is_none() || consumed.is_none(),
            summon_i18n::format!("r93bf4ba646")
        );
        let Some(mut pending) = pending else {
            if let Some(consumed) = consumed {
                validate_wal_transition_identity(
                    &consumed.record,
                    agent,
                    delivery_scope,
                    message_id,
                )?;
                anyhow::ensure!(
                    !require_started || consumed.record.started,
                    summon_i18n::format!("rd82535307b")
                );
                return Ok(MarkConsumedOutcome::AlreadyConsumed);
            }
            return Ok(MarkConsumedOutcome::Missing);
        };
        validate_wal_transition_identity(&pending.record, agent, delivery_scope, message_id)?;
        anyhow::ensure!(
            !require_started || pending.record.started,
            summon_i18n::format!("r975ce48b65")
        );
        if let Some(legacy) = pending.duplicate_legacy.take() {
            std::fs::remove_file(&legacy).with_context(|| summon_i18n::format!("r4bfdb1ece3"))?;
        }
        if !require_started && !pending.record.superseded_by_new {
            pending.record.superseded_by_new = true;
            atomic_write_mode(
                &pending.path,
                &serde_json::to_vec_pretty(&pending.record)?,
                0o600,
            )?;
        }
        let recovery = pending
            .record
            .recovery_batch_id
            .clone()
            .map(|batch_id| (batch_id, pending.record.inbound.message_id.clone()));
        let target = self.consumed_path(agent, message_id);
        let parent = target.parent().context(summon_i18n::text!("r7e312c8a20"))?;
        std::fs::create_dir_all(parent).with_context(|| summon_i18n::format!("r2eb6a0e955"))?;
        std::fs::rename(&pending.path, &target)
            .with_context(|| format!("inbox pending → consumed {:?} → {target:?}", pending.path))?;
        if let Some((batch_id, replay_id)) = recovery.filter(|_| complete_claim) {
            self.complete_recovery_claim(&batch_id, &replay_id)?;
        }
        Ok(MarkConsumedOutcome::Consumed)
    }

    /// 本轮处理完:把 `pending/<id>` 迁到 `consumed/`(rename)。不存在则 no-op。best-effort。
    pub(crate) fn mark_consumed(&self, agent: &str, message_id: &str) {
        if let Ok(Some(from)) = self.existing_pending_path(agent, message_id) {
            let recovery = read_record(&from).ok().flatten().and_then(|record| {
                record
                    .recovery_batch_id
                    .map(|batch_id| (batch_id, record.inbound.message_id))
            });
            let to = self.consumed_path(agent, message_id);
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::rename(from, to).is_ok() {
                if let Some((batch_id, replay_id)) = recovery {
                    let _ = self.complete_recovery_claim(&batch_id, &replay_id);
                }
            }
        }
    }

    /// 标记某消息已撤回(写 tombstone 文件到 `recalled/`)。消息可能还没进 pending(撤回比消息先到),
    /// 也可能已在 pending 里——两种情况都创建 tombstone,chat 线程检查时统一跳过。
    /// best-effort:失败返回 Err 由调用方 log-ignore。
    pub(crate) fn mark_recalled(&self, agent: &str, message_id: &str) {
        let path = self.recalled_path(agent, message_id);
        // 空 JSON 作 tombstone:存在即"已撤回"。用 atomic_write 保持一致。
        let _ = atomic_write(&path, b"{}");
    }

    /// 某消息是否被标记为已撤回。
    pub(crate) fn is_recalled(&self, agent: &str, message_id: &str) -> bool {
        self.recalled_path(agent, message_id).exists()
            || self
                .recalled_dir()
                .join(format!("{}.json", sanitize_id(message_id)))
                .exists()
    }

    /// 清除撤回标记(跳过决策执行后清理,避免 recalled/ 无限增长)。best-effort。
    pub(crate) fn clear_recalled(&self, agent: &str, message_id: &str) {
        let _ = std::fs::remove_file(self.recalled_path(agent, message_id));
        let _ = std::fs::remove_file(
            self.recalled_dir()
                .join(format!("{}.json", sanitize_id(message_id))),
        );
    }

    /// 宽松只读接缝主要供测试/展示；生产冷启必须用 [`Self::scan_pending_strict`]，不能把损坏
    /// WAL 静默解释为“没有待恢复消息”。
    #[cfg(test)]
    pub(crate) fn scan_pending(&self) -> Vec<InboxRecord> {
        self.scan_pending_strict().unwrap_or_default()
    }

    /// daemon 冷启的 fail-closed 扫描：active/completed recovery、目录、任一 JSON/identity 只要
    /// 不可读就整体返回 Err；调用方必须停止自动重放/恢复卡生成，等待人工修复。
    pub(crate) fn scan_pending_strict(&self) -> anyhow::Result<Vec<InboxRecord>> {
        let managed = self.recovery_managed_sources()?;
        let mut records = Vec::<InboxRecord>::new();
        let mut identities =
            std::collections::BTreeMap::<(String, String), (InboxRecord, PathBuf)>::new();
        for path in json_paths_recursive_strict(&self.pending_dir())? {
            let record =
                read_record(&path)?.with_context(|| summon_i18n::format!("r427288a14a"))?;
            self.validate_pending_record_path(&path, &record)?;
            let identity = (record.agent.clone(), record.inbound.message_id.clone());
            if let Some((existing, existing_path)) = identities.get(&identity) {
                anyhow::ensure!(existing == &record, summon_i18n::format!("r4e36c242cd"));
                continue;
            }
            let is_recovery_managed = managed.contains(&identity);
            if record.recovery_batch_id.is_some() && !is_recovery_managed {
                self.validate_active_recovery_replay(&record)?;
            }
            identities.insert(identity, (record.clone(), path));
            if is_recovery_managed {
                continue;
            }
            records.push(record);
        }
        Ok(records)
    }

    /// 校验一个无头 Chat checkpoint 是否精确持有当前 chat 的全部 `started` Inbox 记录。
    /// 空/重复/缺失/多余 id、已 consumed、scheduled/recovery replay 或身份不一致都返回 false；
    /// WAL 本身损坏则返回 Err，调用方必须禁止启动 CLI，不能把“不可证明”当作空集。
    pub(crate) fn resume_sources_match(
        &self,
        agent: &str,
        delivery_scope: &str,
        chat_id: &str,
        chat_type: &str,
        message_ids: &[String],
    ) -> anyhow::Result<bool> {
        Ok(self
            .resume_source(agent, delivery_scope, chat_id, chat_type, message_ids)?
            .is_some())
    }

    /// 集合校验成功后交出首条原始入站，供续跑重建 requester 与会话凭证。
    pub(crate) fn resume_source(
        &self,
        agent: &str,
        delivery_scope: &str,
        chat_id: &str,
        chat_type: &str,
        message_ids: &[String],
    ) -> anyhow::Result<Option<Inbound>> {
        if agent.trim().is_empty()
            || delivery_scope.trim().is_empty()
            || chat_id.trim().is_empty()
            || chat_type.trim().is_empty()
            || message_ids.is_empty()
            || message_ids.iter().any(|id| id.trim().is_empty())
        {
            return Ok(None);
        }
        let expected = message_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        if expected.len() != message_ids.len() {
            return Ok(None);
        }

        let mut actual = std::collections::BTreeSet::new();
        let mut primary = None;
        for record in self.scan_pending_strict()? {
            let record_chat_type = record.inbound.chat_type.as_deref().unwrap_or("p2p");
            if record.agent != agent
                || record.delivery_scope != delivery_scope
                || record.inbound.chat_id != chat_id
                || record_chat_type != chat_type
                || !record.started
            {
                continue;
            }
            // 定时 fire、sentinel fire 与人工授权的 recovery replay 有各自的恢复协议，
            // 绝不能借普通 Chat checkpoint 绕过它们的幂等/确认边界。
            if record.scheduled.is_some()
                || record.sentinel.is_some()
                || record.recovery_origin.is_some()
                || record.recovery_batch_id.is_some()
            {
                return Ok(None);
            }
            if message_ids.first() == Some(&record.inbound.message_id) {
                primary = Some(record.inbound.clone());
            }
            actual.insert(record.inbound.message_id);
        }
        Ok(if actual == expected { primary } else { None })
    }

    fn validate_pending_record_path(
        &self,
        path: &Path,
        record: &InboxRecord,
    ) -> anyhow::Result<()> {
        let message_id = record.inbound.message_id.as_str();
        anyhow::ensure!(
            !record.agent.trim().is_empty()
                && !record.delivery_scope.trim().is_empty()
                && !message_id.trim().is_empty()
                && !record.inbound.chat_id.trim().is_empty(),
            summon_i18n::format!("r7ddd815e4b")
        );
        let expected_file = format!("{}.json", sanitize_id(message_id));
        anyhow::ensure!(
            path.file_name().and_then(|name| name.to_str()) == Some(expected_file.as_str()),
            summon_i18n::format!("r5e1504449f")
        );
        let pending_dir = self.pending_dir();
        let parent = path.parent().context(summon_i18n::text!("r528ea70d4c"))?;
        if parent != pending_dir {
            let expected_bucket = agent_bucket(&record.agent);
            anyhow::ensure!(
                parent.parent() == Some(pending_dir.as_path())
                    && parent.file_name().and_then(|name| name.to_str())
                        == Some(expected_bucket.as_str()),
                summon_i18n::format!("rfbb7711534")
            );
        }
        match (
            record.recovery_origin.as_deref(),
            record.recovery_batch_id.as_deref(),
        ) {
            (None, None) => {}
            (Some(origin), Some(batch_id)) => {
                anyhow::ensure!(
                    !origin.trim().is_empty()
                        && valid_recovery_id(batch_id)
                        && summon_core::outbox::recovery_replay_id(batch_id, origin) == message_id,
                    summon_i18n::format!("rf5db3a9c43")
                );
            }
            _ => anyhow::bail!(summon_i18n::format!("r5e5633e383")),
        }
        if let Some(scheduled) = &record.scheduled {
            anyhow::ensure!(
                scheduled.fire_id == message_id
                    && scheduled.chat_id == record.inbound.chat_id
                    && record.inbound.chat_type.as_deref() == Some(scheduled.chat_type.as_str())
                    && scheduled.prompt == record.inbound.text,
                summon_i18n::format!("rdfe3b4cea9")
            );
        }
        Ok(())
    }

    fn validate_active_recovery_replay(&self, record: &InboxRecord) -> anyhow::Result<()> {
        let batch_id = record
            .recovery_batch_id
            .as_deref()
            .context(summon_i18n::text!("rbcf28a0f0e"))?;
        let origin = record
            .recovery_origin
            .as_deref()
            .context(summon_i18n::text!("r4da5debaa7"))?;
        let batch = self
            .load_recovery_batch(batch_id)?
            .context(summon_i18n::text!("rf617611022"))?;
        anyhow::ensure!(
            batch.agent == record.agent,
            summon_i18n::format!("rf52fd28d6d")
        );
        let item = batch
            .items
            .iter()
            .find(|item| item.record.inbound.message_id == origin)
            .context(summon_i18n::text!("rc849d9043a"))?;
        let (replay_id, expected) = recovery_replay_record(batch_id, origin, &item.record);
        validate_recovery_replay(record, &expected, &replay_id)?;
        match &item.state {
            RecoveryItemState::Awaiting => Ok(()),
            RecoveryItemState::Claimed {
                replay_id: claimed_id,
            } => {
                anyhow::ensure!(
                    claimed_id == &replay_id,
                    summon_i18n::format!("r3f9bebe355")
                );
                Ok(())
            }
            RecoveryItemState::Completed | RecoveryItemState::Dismissed => {
                anyhow::bail!(summon_i18n::format!("rde1530b8fd"))
            }
        }
    }

    /// Bridge 占用检查：该 agent/chat 是否还有 pending 或 started WAL。与恢复 UI 的宽松扫描不同，
    /// 这里任何目录/记录读取错误都向上传递，调用方必须 fail-closed，不能因损坏记录 cold-start。
    pub(crate) fn has_pending(&self, agent: &str, chat_id: &str) -> anyhow::Result<bool> {
        for path in json_paths_recursive_strict(&self.pending_dir())? {
            let record = read_record(&path)?
                .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r427288a14a")))?;
            if record.agent == agent && record.inbound.chat_id == chat_id {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 升级迁移旧 RecoveryCard 时只读 consumed 权威原消息。优先 agent-scoped，新版之前的
    /// flat 文件必须由记录内 agent/chat 再校验，绝不按 message_id 跨 Bot 猜测。
    pub(crate) fn read_consumed(
        &self,
        agent: &str,
        message_id: &str,
    ) -> anyhow::Result<Option<InboxRecord>> {
        if let Some(record) = read_record(&self.consumed_path(agent, message_id))? {
            anyhow::ensure!(record.agent == agent, summon_i18n::format!("rcb1aa3dec7"));
            return Ok(Some(record));
        }
        let legacy = self
            .consumed_dir()
            .join(format!("{}.json", sanitize_id(message_id)));
        match read_record(&legacy)? {
            Some(record) if record.agent == agent => Ok(Some(record)),
            Some(_) => Ok(None),
            None => Ok(None),
        }
    }

    /// 先把完整恢复批次持久化，随后 producer 才有权 enqueue RecoveryCard 并删除 source pending。
    /// 批次 id 对 scope/agent/chat/消息集合确定，同一次崩溃在任意窗口重启都只会得到同一张卡。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_recovery_batch(
        &self,
        scope: &str,
        agent: &str,
        chat_id: &str,
        chat_type: &str,
        stage: Option<String>,
        mut records: Vec<InboxRecord>,
        created_ms: u64,
    ) -> anyhow::Result<RecoveryBatch> {
        anyhow::ensure!(
            !scope.is_empty() && !agent.is_empty() && !chat_id.is_empty() && !records.is_empty(),
            summon_i18n::format!("r2996f4e25e")
        );
        records.sort_by(|left, right| {
            (
                left.inbound.create_time_ms,
                left.inbound.message_id.as_str(),
            )
                .cmp(&(
                    right.inbound.create_time_ms,
                    right.inbound.message_id.as_str(),
                ))
        });
        let mut ids = std::collections::BTreeSet::new();
        for record in &records {
            anyhow::ensure!(
                record.agent == agent
                    && record.delivery_scope == scope
                    && record.inbound.chat_id == chat_id
                    && record.scheduled.is_none()
                    && !record.inbound.message_id.is_empty()
                    && ids.insert(record.inbound.message_id.clone()),
                summon_i18n::format!("ra85a915ee7")
            );
        }
        let id = recovery_batch_id(scope, agent, chat_id, ids);
        let batch = RecoveryBatch {
            schema_version: RECOVERY_SCHEMA_VERSION,
            id: id.clone(),
            outbox_id: id.clone(),
            scope: scope.to_string(),
            agent: agent.to_string(),
            chat_id: chat_id.to_string(),
            chat_type: chat_type.to_string(),
            stage,
            items: records
                .into_iter()
                .map(|record| RecoveryBatchItem {
                    preview: summon_core::quote::quote_preview(&record.inbound.text),
                    record,
                    state: RecoveryItemState::Awaiting,
                })
                .collect(),
            card_message_id: None,
            // identity 不依赖时钟；首次 durable manifest 的时间只用于观察，重启读既有值。
            created_ms,
        };
        let _guard = recovery_lock()?;
        let path = self.recovery_path(&id)?;
        if let Some(existing) = read_recovery_batch_optional(&path)? {
            anyhow::ensure!(
                same_recovery_batch_identity(&existing, &batch),
                summon_i18n::format!("r2a34a94dd2")
            );
            return Ok(existing);
        }
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
        Ok(batch)
    }

    pub(crate) fn active_recovery_batches(&self) -> anyhow::Result<Vec<RecoveryBatch>> {
        let _guard = recovery_lock()?;
        let mut batches = Vec::new();
        for path in json_paths_recursive_strict(&self.recovery_dir())? {
            batches.push(read_recovery_batch(&path)?);
        }
        batches.sort_by(|left, right| {
            (left.created_ms, left.id.as_str()).cmp(&(right.created_ms, right.id.as_str()))
        });
        Ok(batches)
    }

    pub(crate) fn load_recovery_batch(
        &self,
        batch_id: &str,
    ) -> anyhow::Result<Option<RecoveryBatch>> {
        let _guard = recovery_lock()?;
        read_recovery_batch_optional(&self.recovery_path(batch_id)?)
    }

    /// 修复 recovery claim 的持久状态窗口：
    /// 1. 确定性 replay 已写入 pending，但 active batch 仍停在 Awaiting；
    /// 2. replay 已从 pending rename 到 consumed，但 batch 仍停在 Claimed。
    /// 3. batch 仍是 Claimed，但 pending.started=true 表明上次已开跑、结果未知；退回 Awaiting
    ///    等用户再次确认，绝不静默重放。
    ///
    /// 冷启必须在普通 inbox 扫描/重注入前调用。只认当前 batch 推导出的确定性 replay，并
    /// 逐项复验 scope/agent/chat/origin/batch id 与完整 Inbound。任何冲突都 fail-closed，且
    /// 同一 batch 在全部项校验通过后才原子重写；这里已持 recovery lock，不调用会再次取锁的
    /// 公共方法。
    pub(crate) fn repair_pending_recovery_claims(&self) -> anyhow::Result<usize> {
        let _guard = recovery_lock()?;
        let mut repaired = 0usize;
        for path in json_paths_recursive_strict(&self.recovery_dir())? {
            let mut batch = read_recovery_batch(&path)?;
            let batch_id = batch.id.clone();
            let agent = batch.agent.clone();
            let mut batch_repaired = 0usize;
            let mut duplicate_legacy_paths = Vec::new();
            for item in &mut batch.items {
                let (replay_id, expected) = recovery_replay_record(
                    &batch_id,
                    &item.record.inbound.message_id,
                    &item.record,
                );
                match &item.state {
                    RecoveryItemState::Awaiting => {
                        let pending = self.read_recovery_record_variants(
                            &self.pending_dir(),
                            &agent,
                            &replay_id,
                            "pending",
                        )?;
                        let consumed = self.read_recovery_record_variants(
                            &self.consumed_dir(),
                            &agent,
                            &replay_id,
                            "consumed",
                        )?;
                        anyhow::ensure!(
                            pending.is_none() || consumed.is_none(),
                            summon_i18n::format!("rb3f4c7ba55")
                        );
                        let Some(pending) = pending else {
                            if let Some(consumed) = consumed {
                                validate_recovery_replay(&consumed.record, &expected, &replay_id)?;
                                if consumed.record.superseded_by_new {
                                    item.state = RecoveryItemState::Completed;
                                    batch_repaired += 1;
                                    continue;
                                }
                                anyhow::ensure!(
                                    !consumed.record.started,
                                    summon_i18n::format!("r326b17e47f")
                                );
                                // 旧版本 send 失败可能把未开跑 replay 错迁到 consumed；保持
                                // Awaiting，等用户再次点击时从 batch 权威副本重建 pending。
                            }
                            continue;
                        };
                        validate_recovery_replay(&pending.record, &expected, &replay_id)?;
                        if pending.record.superseded_by_new {
                            self.settle_pending(
                                &agent,
                                &pending.record.delivery_scope,
                                &replay_id,
                                false,
                                false,
                            )?;
                            item.state = RecoveryItemState::Completed;
                            batch_repaired += 1;
                            continue;
                        }
                        if let Some(legacy) = pending.duplicate_legacy {
                            duplicate_legacy_paths.push(legacy);
                        }
                        if pending.record.started {
                            // 这不是 claim 的「写 pending、未写 batch」窗口，而是上一轮已经开跑；
                            // 保持 Awaiting，必须由用户再次点击才允许清零并重投。
                            continue;
                        }
                        item.state = RecoveryItemState::Claimed { replay_id };
                        batch_repaired += 1;
                    }
                    RecoveryItemState::Claimed {
                        replay_id: claimed_id,
                    } => {
                        anyhow::ensure!(
                            claimed_id == &replay_id,
                            summon_i18n::format!("r6c469d6d44")
                        );
                        let pending = self.read_recovery_record_variants(
                            &self.pending_dir(),
                            &agent,
                            &replay_id,
                            "pending",
                        )?;
                        let consumed = self.read_recovery_record_variants(
                            &self.consumed_dir(),
                            &agent,
                            &replay_id,
                            "consumed",
                        )?;
                        anyhow::ensure!(
                            pending.is_none() || consumed.is_none(),
                            summon_i18n::format!("rb3f4c7ba55")
                        );
                        if let Some(pending) = pending {
                            validate_recovery_replay(&pending.record, &expected, &replay_id)?;
                            if pending.record.superseded_by_new {
                                self.settle_pending(
                                    &agent,
                                    &pending.record.delivery_scope,
                                    &replay_id,
                                    false,
                                    false,
                                )?;
                                item.state = RecoveryItemState::Completed;
                                batch_repaired += 1;
                                continue;
                            }
                            if let Some(legacy) = pending.duplicate_legacy {
                                duplicate_legacy_paths.push(legacy);
                            }
                            if pending.record.started {
                                // 上次执行结果未知，撤回授权状态但保留 started replay 作为证据；
                                // classifier 会 hold，用户再次点击原卡后才会显式重投。
                                item.state = RecoveryItemState::Awaiting;
                                batch_repaired += 1;
                            }
                            continue;
                        }
                        let Some(consumed) = consumed else {
                            continue;
                        };
                        validate_recovery_replay(&consumed.record, &expected, &replay_id)?;
                        if !consumed.record.started
                            && !consumed.record.rejected_disabled
                            && !consumed.record.superseded_by_new
                        {
                            // consumed 不是执行完成的充分证据：旧 dispatcher send-failure 会留下
                            // started=false。退回 Awaiting，绝不把“从未执行”永久确认丢失。
                            item.state = RecoveryItemState::Awaiting;
                            batch_repaired += 1;
                            continue;
                        }
                        if let Some(legacy) = consumed.duplicate_legacy {
                            duplicate_legacy_paths.push(legacy);
                        }
                        item.state = RecoveryItemState::Completed;
                        batch_repaired += 1;
                    }
                    RecoveryItemState::Completed | RecoveryItemState::Dismissed => continue,
                }
            }
            if batch_repaired > 0 {
                atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
                repaired += batch_repaired;
            }
            // 只有整批校验和 batch 原子写都成功后才清理迁移重复项；scoped 权威副本始终保留。
            for legacy in duplicate_legacy_paths {
                std::fs::remove_file(&legacy)
                    .with_context(|| summon_i18n::format!("r52081a699e"))?;
            }
            // 也补偿「active 已全终态、completed tombstone 尚未落盘」的最后一个窗口。
            self.terminalize_recovery_if_done(&path, &batch)?;
        }
        Ok(repaired)
    }

    /// recovery WAL 升级前可能还在 flat 目录。scoped/legacy 两份完全相同是迁移崩溃留下的
    /// 可接受副本；两份载荷不同则无法判断哪份权威，必须 fail-closed。
    fn read_recovery_record_variants(
        &self,
        dir: &Path,
        agent: &str,
        replay_id: &str,
        state: &str,
    ) -> anyhow::Result<Option<RecoveryRecordOnDisk>> {
        let scoped_path = dir
            .join(agent_bucket(agent))
            .join(format!("{}.json", sanitize_id(replay_id)));
        let legacy_path = dir.join(format!("{}.json", sanitize_id(replay_id)));
        let scoped_record = read_record(&scoped_path)?;
        let legacy_record = read_record(&legacy_path)?;
        match (scoped_record, legacy_record) {
            (Some(scoped), Some(legacy)) => {
                anyhow::ensure!(scoped == legacy, summon_i18n::format!("re1d2b8376b"));
                Ok(Some(RecoveryRecordOnDisk {
                    record: scoped,
                    path: scoped_path,
                    duplicate_legacy: Some(legacy_path),
                }))
            }
            (Some(record), None) => Ok(Some(RecoveryRecordOnDisk {
                record,
                path: scoped_path,
                duplicate_legacy: None,
            })),
            (None, Some(record)) => Ok(Some(RecoveryRecordOnDisk {
                record,
                path: legacy_path,
                duplicate_legacy: None,
            })),
            (None, None) => Ok(None),
        }
    }

    fn remove_pending_record_exact(
        &self,
        agent: &str,
        message_id: &str,
        expected: &InboxRecord,
        label: &str,
    ) -> anyhow::Result<()> {
        let Some(mut on_disk) =
            self.read_recovery_record_variants(&self.pending_dir(), agent, message_id, "pending")?
        else {
            return Ok(());
        };
        anyhow::ensure!(
            on_disk.record == *expected,
            summon_i18n::format!("r0b1f1d2689")
        );
        if let Some(legacy) = on_disk.duplicate_legacy.take() {
            std::fs::remove_file(&legacy).with_context(|| summon_i18n::format!("rcb46d1ad10"))?;
        }
        std::fs::remove_file(&on_disk.path)
            .with_context(|| summon_i18n::format!("r59c25d097e", on_disk.path))?;
        Ok(())
    }

    fn remove_recovery_replay_pending(
        &self,
        batch_id: &str,
        agent: &str,
        item: &RecoveryBatchItem,
    ) -> anyhow::Result<()> {
        let (replay_id, expected) =
            recovery_replay_record(batch_id, &item.record.inbound.message_id, &item.record);
        if let RecoveryItemState::Claimed {
            replay_id: claimed_id,
        } = &item.state
        {
            anyhow::ensure!(
                claimed_id == &replay_id,
                summon_i18n::format!("rd35757b46c")
            );
        }
        let Some(mut on_disk) =
            self.read_recovery_record_variants(&self.pending_dir(), agent, &replay_id, "pending")?
        else {
            return Ok(());
        };
        // started 是执行进度而非载荷 identity；Awaiting crash-left 正常会是 true，其余字段必须
        // 与 deterministic replay 完全一致。
        validate_recovery_replay(&on_disk.record, &expected, &replay_id)?;
        if let Some(legacy) = on_disk.duplicate_legacy.take() {
            std::fs::remove_file(&legacy).with_context(|| summon_i18n::format!("rf4bc3ecc64"))?;
        }
        std::fs::remove_file(&on_disk.path)
            .with_context(|| summon_i18n::format!("r26ba64a96a", on_disk.path))?;
        Ok(())
    }

    /// RecoveryCard outbox 已 durable 后删除它接管的原 pending。batch manifest 是完整副本，
    /// 中途崩溃时重复调用只补删残留；发现同 key 不同载荷则 fail-closed。
    pub(crate) fn finalize_recovery_sources(&self, batch: &RecoveryBatch) -> anyhow::Result<()> {
        let _guard = recovery_lock()?;
        let current = read_recovery_batch(&self.recovery_path(&batch.id)?)?;
        anyhow::ensure!(
            same_recovery_batch_identity(&current, batch),
            summon_i18n::format!("r21c331b8a9")
        );
        let mut source_paths = Vec::new();
        for item in &current.items {
            let mid = &item.record.inbound.message_id;
            let Some(on_disk) = self.read_recovery_record_variants(
                &self.pending_dir(),
                &current.agent,
                mid,
                "source pending",
            )?
            else {
                continue;
            };
            anyhow::ensure!(
                on_disk.record == item.record,
                summon_i18n::format!("r39cefd12db")
            );
            source_paths.push(on_disk.path);
            if let Some(legacy) = on_disk.duplicate_legacy {
                source_paths.push(legacy);
            }
        }
        // 整批复验完成后再删除，后项冲突不能让前项先被部分接管。
        for path in source_paths {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    pub(crate) fn record_recovery_card_delivery(
        &self,
        batch_id: &str,
        outbox_id: &str,
        message_id: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!message_id.is_empty(), summon_i18n::format!("re2571a1882"));
        let _guard = recovery_lock()?;
        let path = self.recovery_path(batch_id)?;
        let mut batch = read_recovery_batch(&path)?;
        anyhow::ensure!(
            batch.outbox_id == outbox_id,
            summon_i18n::format!("r0c0619f8e2")
        );
        match batch.card_message_id.as_deref() {
            Some(existing) => {
                anyhow::ensure!(existing == message_id, summon_i18n::format!("r48c5161c03"))
            }
            None => {
                batch.card_message_id = Some(message_id.to_string());
                atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn claim_recovery(
        &self,
        batch_id: &str,
        scope: &str,
        agent: &str,
        chat_id: &str,
        card_message_id: &str,
        original_message_id: &str,
    ) -> anyhow::Result<RecoveryClaimOutcome> {
        let _guard = recovery_lock()?;
        let path = self.recovery_path(batch_id)?;
        let mut batch = read_recovery_batch(&path)?;
        validate_recovery_callback_identity(&batch, scope, agent, chat_id, card_message_id)?;
        let item = batch
            .items
            .iter_mut()
            .find(|item| item.record.inbound.message_id == original_message_id)
            .context(summon_i18n::text!("r083d33bde2"))?;
        match &item.state {
            RecoveryItemState::Claimed { .. } => return Ok(RecoveryClaimOutcome::AlreadyClaimed),
            RecoveryItemState::Completed | RecoveryItemState::Dismissed => {
                return Ok(RecoveryClaimOutcome::NotAwaiting)
            }
            RecoveryItemState::Awaiting => {}
        }
        let (replay_id, replay) =
            recovery_replay_record(batch_id, original_message_id, &item.record);
        let pending =
            self.read_recovery_record_variants(&self.pending_dir(), agent, &replay_id, "pending")?;
        let consumed = self.read_recovery_record_variants(
            &self.consumed_dir(),
            agent,
            &replay_id,
            "consumed",
        )?;
        anyhow::ensure!(
            pending.is_none() || consumed.is_none(),
            summon_i18n::format!("rb3f4c7ba55")
        );
        if let Some(existing) = pending {
            validate_recovery_replay(&existing.record, &replay, &replay_id)?;
            if let Some(legacy) = existing.duplicate_legacy {
                // scoped 仍保留旧 started 状态；先删完全相同的 legacy，再原子清零 scoped。
                std::fs::remove_file(&legacy)
                    .with_context(|| summon_i18n::format!("r52081a699e"))?;
            }
            // Awaiting + existing replay 可能是 claim 首次半提交，也可能是上一轮 started=true 后
            // 冷启退回的待确认。此刻 callback 就是显式授权：先把同一 replay 原子清零，再推进
            // batch；任一窗口崩溃都能由本 repair 重新判定，绝不生成第二个 replay id。
            atomic_write_mode(&existing.path, &serde_json::to_vec_pretty(&replay)?, 0o600)?;
            item.state = RecoveryItemState::Claimed { replay_id };
            atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
            return Ok(RecoveryClaimOutcome::Claimed(Box::new(replay)));
        }
        if let Some(consumed) = consumed {
            validate_recovery_replay(&consumed.record, &replay, &replay_id)?;
            anyhow::ensure!(
                !consumed.record.started,
                summon_i18n::format!("r08adaa4969")
            );
            // 明确点击重新授权旧 send-failure 的误销账 replay。batch manifest 仍保存完整副本，
            // 先删 consumed 再重建 pending 的任一崩溃窗口都可由同一 callback/按钮重试。
            if let Some(legacy) = consumed.duplicate_legacy {
                std::fs::remove_file(&legacy)
                    .with_context(|| summon_i18n::format!("r6878b8d49e"))?;
            }
            std::fs::remove_file(&consumed.path)
                .with_context(|| summon_i18n::format!("rf9cdd5864a", consumed.path))?;
            let pending = self.pending_path(agent, &replay_id);
            atomic_write_mode(&pending, &serde_json::to_vec_pretty(&replay)?, 0o600)?;
            item.state = RecoveryItemState::Claimed { replay_id };
            atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
            return Ok(RecoveryClaimOutcome::Claimed(Box::new(replay)));
        }
        // claim 先 durable，batch 后推进。任一窗口崩溃：pending replay 仍会在冷启自动续送；
        // batch 尚 Awaiting 时再次点击只会识别同一确定性 replay，不会造第二轮。
        let pending = self.pending_path(agent, &replay_id);
        atomic_write_mode(&pending, &serde_json::to_vec_pretty(&replay)?, 0o600)?;
        item.state = RecoveryItemState::Claimed { replay_id };
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
        Ok(RecoveryClaimOutcome::Claimed(Box::new(replay)))
    }

    fn complete_recovery_claim(&self, batch_id: &str, replay_id: &str) -> anyhow::Result<()> {
        let _guard = recovery_lock()?;
        let path = self.recovery_path(batch_id)?;
        let Some(mut batch) = read_recovery_batch_optional(&path)? else {
            return Ok(());
        };
        let item = batch
            .items
            .iter_mut()
            .find(|item| {
                matches!(
                    &item.state,
                    RecoveryItemState::Claimed { replay_id: current } if current == replay_id
                )
            })
            .context(summon_i18n::text!("rf4afbb4d23"))?;
        item.state = RecoveryItemState::Completed;
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
        self.terminalize_recovery_if_done(&path, &batch)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dismiss_recovery_batch(
        &self,
        batch_id: &str,
        scope: &str,
        agent: &str,
        chat_id: &str,
        card_message_id: &str,
    ) -> anyhow::Result<usize> {
        let _guard = recovery_lock()?;
        let path = self.recovery_path(batch_id)?;
        let mut batch = read_recovery_batch(&path)?;
        validate_recovery_callback_identity(&batch, scope, agent, chat_id, card_message_id)?;
        let mut dismissed = 0usize;
        let durable_batch_id = batch.id.clone();
        for item in &mut batch.items {
            if item.state != RecoveryItemState::Awaiting {
                continue;
            }
            // Claimed→started crash 经 repair 会退回 Awaiting，但 deterministic replay 仍在
            // pending。拒绝整批时必须 exact 删除，不能让 batch 终态后 replay 复活。
            self.remove_recovery_replay_pending(&durable_batch_id, agent, item)?;
            let consumed = self.consumed_path(agent, &item.record.inbound.message_id);
            atomic_write_mode(&consumed, &serde_json::to_vec_pretty(&item.record)?, 0o600)?;
            item.state = RecoveryItemState::Dismissed;
            dismissed += 1;
        }
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
        self.terminalize_recovery_if_done(&path, &batch)?;
        Ok(dismissed)
    }

    fn terminalize_recovery_if_done(
        &self,
        active_path: &Path,
        batch: &RecoveryBatch,
    ) -> anyhow::Result<()> {
        if batch.items.iter().any(|item| {
            matches!(
                item.state,
                RecoveryItemState::Awaiting | RecoveryItemState::Claimed { .. }
            )
        }) {
            return Ok(());
        }
        let completed = CompletedRecoveryBatch {
            schema_version: RECOVERY_SCHEMA_VERSION,
            id: batch.id.clone(),
            outbox_id: batch.outbox_id.clone(),
            scope: batch.scope.clone(),
            agent: batch.agent.clone(),
            chat_id: batch.chat_id.clone(),
            card_message_id: batch.card_message_id.clone(),
            source_message_ids: batch
                .items
                .iter()
                .map(|item| item.record.inbound.message_id.clone())
                .collect(),
            completed_at_ms: unix_now_ms(),
        };
        let completed_path = self.completed_recovery_path(&batch.id)?;
        // 先写不含正文的终态，再删除 active 全量 manifest。崩溃只会短暂保留两份，不会丢状态。
        atomic_write_mode(
            &completed_path,
            &serde_json::to_vec_pretty(&completed)?,
            0o600,
        )?;
        std::fs::remove_file(active_path)?;
        Ok(())
    }

    pub(crate) fn has_unresolved_recovery(
        &self,
        scope: &str,
        agent: &str,
        chat_id: &str,
    ) -> anyhow::Result<bool> {
        Ok(self.active_recovery_batches()?.iter().any(|batch| {
            batch.scope == scope
                && batch.agent == agent
                && batch.chat_id == chat_id
                && batch.items.iter().any(|item| {
                    matches!(
                        item.state,
                        RecoveryItemState::Awaiting | RecoveryItemState::Claimed { .. }
                    )
                })
        }))
    }

    fn recovery_managed_sources(
        &self,
    ) -> anyhow::Result<std::collections::BTreeSet<(String, String)>> {
        let mut managed = std::collections::BTreeSet::new();
        for batch in self.active_recovery_batches()? {
            for item in batch.items {
                managed.insert((batch.agent.clone(), item.record.inbound.message_id));
            }
        }
        for path in json_paths_recursive_strict(&self.root.join("recovery").join("completed"))? {
            let completed = read_completed_recovery_batch_checked(&path)?;
            for message_id in completed.source_message_ids {
                let replay_id = summon_core::outbox::recovery_replay_id(&completed.id, &message_id);
                managed.insert((completed.agent.clone(), message_id));
                managed.insert((completed.agent.clone(), replay_id));
            }
        }
        Ok(managed)
    }

    /// Active recovery 是授权状态，不受 InboundGate 的有界 LRU 容量约束。每条网络入站都对
    /// durable batch 做 exact check；任一 batch 损坏向上传递，dispatcher 对该入站 fail-closed。
    pub(crate) fn is_recovery_original(
        &self,
        agent: &str,
        message_id: &str,
    ) -> anyhow::Result<bool> {
        Ok(self.active_recovery_batches()?.iter().any(|batch| {
            batch.agent == agent
                && batch.items.iter().any(|item| {
                    item.record.inbound.message_id == message_id
                        || item.record.recovery_origin.as_deref() == Some(message_id)
                })
        }))
    }

    /// 群解散/bot removed 是恢复控制面的显式终点：删除该 chat 的 replay/source pending，
    /// 把 active batch 写成脱敏 completed tombstone。只碰 inbox/recovery 文件，不触碰 tmux。
    pub(crate) fn terminalize_recovery_chat(
        &self,
        scope: &str,
        agent: &str,
        chat_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        let _guard = recovery_lock()?;
        let mut outbox_ids = Vec::new();
        for path in json_paths_recursive_strict(&self.recovery_dir())? {
            let mut batch = read_recovery_batch(&path)?;
            if batch.scope != scope || batch.agent != agent || batch.chat_id != chat_id {
                continue;
            }
            for item in &batch.items {
                self.remove_recovery_replay_pending(&batch.id, agent, item)?;
                self.remove_pending_record_exact(
                    agent,
                    &item.record.inbound.message_id,
                    &item.record,
                    "recovery source",
                )?;
            }
            for item in &mut batch.items {
                item.state = RecoveryItemState::Dismissed;
            }
            atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
            self.terminalize_recovery_if_done(&path, &batch)?;
            outbox_ids.push(batch.outbox_id);
        }
        Ok(outbox_ids)
    }

    pub(crate) fn recovery_outbox_completed(
        &self,
        batch_id: &str,
        outbox_id: &str,
    ) -> anyhow::Result<bool> {
        Ok(self
            .completed_recovery_for_outbox(batch_id, outbox_id)?
            .is_some())
    }

    /// 只有 completed tombstone 与出站事件的完整 delivery identity 一致时才允许本地 ACK。
    /// 旧的二参数查询只供升级兼容；新 outbox consumer 必须调用本方法。
    pub(crate) fn recovery_outbox_completed_exact(
        &self,
        batch_id: &str,
        outbox_id: &str,
        scope: &str,
        agent: &str,
        chat_id: &str,
    ) -> anyhow::Result<bool> {
        let Some(completed) = self.completed_recovery_for_outbox(batch_id, outbox_id)? else {
            return Ok(false);
        };
        anyhow::ensure!(
            completed.scope == scope && completed.agent == agent && completed.chat_id == chat_id,
            summon_i18n::format!("r40bcb3ede6")
        );
        Ok(true)
    }

    fn completed_recovery_for_outbox(
        &self,
        batch_id: &str,
        outbox_id: &str,
    ) -> anyhow::Result<Option<CompletedRecoveryBatch>> {
        let path = self.completed_recovery_path(batch_id)?;
        let Some(completed) = read_completed_recovery_batch_optional(&path)? else {
            return Ok(None);
        };
        anyhow::ensure!(
            completed.id == batch_id && completed.outbox_id == outbox_id,
            summon_i18n::format!("r75f722da2c")
        );
        Ok(Some(completed))
    }

    /// POST 成功后 `record_recovery_card_delivery` 会先持久化 message_id。若随后在 outbox ACK
    /// 前崩溃，冷启可据此 exact 查询本地 ACK，避免同一恢复卡再次 POST。
    pub(crate) fn recovery_outbox_delivered(
        &self,
        batch_id: &str,
        outbox_id: &str,
        scope: &str,
        agent: &str,
        chat_id: &str,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !scope.is_empty() && !agent.is_empty() && !chat_id.is_empty(),
            summon_i18n::format!("r73d5f9c0a8")
        );
        let _guard = recovery_lock()?;
        let path = self.recovery_path(batch_id)?;
        let Some(batch) = read_recovery_batch_optional(&path)? else {
            return Ok(false);
        };
        anyhow::ensure!(
            batch.outbox_id == outbox_id
                && batch.scope == scope
                && batch.agent == agent
                && batch.chat_id == chat_id,
            summon_i18n::format!("r1679f6f1fb")
        );
        match batch.card_message_id.as_deref() {
            Some(message_id) if !message_id.trim().is_empty() => Ok(true),
            Some(_) => anyhow::bail!(summon_i18n::format!("r348054b8bc")),
            None => Ok(false),
        }
    }

    pub(crate) fn abandon_stale_pending(&self, records: &[InboxRecord]) -> anyhow::Result<usize> {
        let mut abandoned = 0usize;
        for record in records {
            let tombstone = AbandonedInboxRecord {
                schema_version: RECOVERY_SCHEMA_VERSION,
                agent: record.agent.clone(),
                delivery_scope: record.delivery_scope.clone(),
                message_id: record.inbound.message_id.clone(),
                reason: "stale_delivery_scope".into(),
                abandoned_at_ms: unix_now_ms(),
            };
            let path = self
                .root
                .join("abandoned")
                .join(agent_bucket(&record.agent))
                .join(format!("{}.json", sanitize_id(&record.inbound.message_id)));
            atomic_write_mode(&path, &serde_json::to_vec_pretty(&tombstone)?, 0o600)?;
            if let Some(source) =
                self.existing_pending_path(&record.agent, &record.inbound.message_id)?
            {
                std::fs::remove_file(source)?;
            }
            abandoned += 1;
        }
        Ok(abandoned)
    }

    pub(crate) fn abandon_recovery_batch(&self, batch_id: &str) -> anyhow::Result<Option<String>> {
        let _guard = recovery_lock()?;
        let path = self.recovery_path(batch_id)?;
        let Some(mut batch) = read_recovery_batch_optional(&path)? else {
            return Ok(None);
        };
        for item in &batch.items {
            self.remove_recovery_replay_pending(&batch.id, &batch.agent, item)?;
            self.remove_pending_record_exact(
                &batch.agent,
                &item.record.inbound.message_id,
                &item.record,
                "recovery source",
            )?;
        }
        for item in &mut batch.items {
            item.state = RecoveryItemState::Dismissed;
        }
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&batch)?, 0o600)?;
        self.terminalize_recovery_if_done(&path, &batch)?;
        Ok(Some(batch.outbox_id))
    }

    /// 清理 `consumed/` 里超过保留期(7 天)的记录,防无限增长。completed recovery tombstone
    /// 是永久去重/ACK 事实，未建立引用安全的 GC 前绝不能按时间无条件删除。
    pub(crate) fn cleanup_consumed(&self, now_ms: u64) {
        cleanup_dir_older_than(&self.consumed_dir(), now_ms, RETENTION_MS);
    }

    /// 撤回可能先于原消息到达，或原消息永远不会再到；对应 tombstone 没有消费路径可清。
    /// 启动时按与 consumed 相同的 7 天保留期轮转，防 `recalled/` 随空撤回无界增长。
    pub(crate) fn cleanup_recalled(&self, now_ms: u64) {
        cleanup_dir_older_than(&self.recalled_dir(), now_ms, RETENTION_MS);
    }

    /// 列出某 agent 最近(按 `create_time_ms` 新→旧)已消费的 message_id,至多 `limit` 条。dispatcher 重启
    /// 时用它预热 `InboundGate` 去重集(R5:跨重启去重,补齐与出站 outbox 的对称)。读 `consumed/` 全量
    /// 记录(损坏跳过)、按 agent 过滤、按 create_time 降序截断——只保留最可能被飞书重投命中的最近一批。
    pub(crate) fn recent_consumed_ids(&self, agent: &str, limit: usize) -> Vec<String> {
        let mut recs: Vec<(i64, String)> = read_records_recursive(&self.consumed_dir())
            .into_iter()
            // scheduled 绕过 InboundGate，本身靠 WAL fire_id 幂等；把它灌进真人入站 seen 只会
            // 挤掉有限容量里的飞书 message_id，削弱跨重启去重。
            .filter(|r| {
                r.agent == agent && r.scheduled.is_none() && !r.inbound.message_id.trim().is_empty()
            })
            .flat_map(|r| {
                let at = r.inbound.create_time_ms;
                let mut ids = vec![(at, r.inbound.message_id)];
                if let Some(origin) = r.recovery_origin {
                    ids.push((at, origin));
                }
                ids
            })
            .collect();
        // create_time 降序(新→旧):create_time 未知(0)的排最后,优先保留有时间戳的近期条目。
        recs.sort_by_key(|(ct, _)| std::cmp::Reverse(*ct));
        recs.truncate(limit);
        recs.into_iter().map(|(_, id)| id).collect()
    }
}

fn recovery_lock() -> anyhow::Result<std::sync::MutexGuard<'static, ()>> {
    RECOVERY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("rf1d5ee65f5")))
}

fn read_recovery_batch(path: &Path) -> anyhow::Result<RecoveryBatch> {
    let batch: RecoveryBatch = serde_json::from_slice(
        &std::fs::read(path).with_context(|| summon_i18n::format!("rf563840e87"))?,
    )
    .with_context(|| summon_i18n::format!("ra7507ece1b"))?;
    anyhow::ensure!(
        batch.schema_version == RECOVERY_SCHEMA_VERSION
            && valid_recovery_id(&batch.id)
            && batch.outbox_id == batch.id
            && path.file_stem().and_then(|stem| stem.to_str()) == Some(batch.id.as_str())
            && !batch.scope.is_empty()
            && !batch.agent.is_empty()
            && !batch.chat_id.is_empty()
            && !batch.items.is_empty(),
        summon_i18n::format!("rb12a838947")
    );
    let mut ids = std::collections::BTreeSet::new();
    for item in &batch.items {
        anyhow::ensure!(
            item.record.agent == batch.agent
                && item.record.delivery_scope == batch.scope
                && item.record.inbound.chat_id == batch.chat_id
                && item.record.scheduled.is_none()
                && ids.insert(item.record.inbound.message_id.clone()),
            summon_i18n::format!("rcdda05d424")
        );
    }
    anyhow::ensure!(
        batch.id == recovery_batch_id(&batch.scope, &batch.agent, &batch.chat_id, ids.into_iter(),),
        summon_i18n::format!("r95eca88fcb")
    );
    Ok(batch)
}

fn read_completed_recovery_batch_checked(path: &Path) -> anyhow::Result<CompletedRecoveryBatch> {
    let completed: CompletedRecoveryBatch = serde_json::from_slice(
        &std::fs::read(path).with_context(|| summon_i18n::format!("rccfc07b830"))?,
    )
    .with_context(|| summon_i18n::format!("r17b302e692"))?;
    anyhow::ensure!(
        completed.schema_version == RECOVERY_SCHEMA_VERSION
            && valid_recovery_id(&completed.id)
            && path.file_stem().and_then(|stem| stem.to_str()) == Some(completed.id.as_str())
            && path.extension().and_then(|extension| extension.to_str()) == Some("json")
            && completed.outbox_id == completed.id
            && !completed.scope.is_empty()
            && !completed.agent.is_empty()
            && !completed.chat_id.is_empty()
            && completed
                .card_message_id
                .as_deref()
                .is_none_or(|message_id| !message_id.trim().is_empty()),
        summon_i18n::format!("r62c1148bff")
    );
    anyhow::ensure!(
        !completed.source_message_ids.is_empty(),
        summon_i18n::format!("r9911462ee2")
    );
    let mut source_ids = std::collections::BTreeSet::new();
    for source_id in &completed.source_message_ids {
        anyhow::ensure!(
            !source_id.is_empty() && source_ids.insert(source_id.clone()),
            summon_i18n::format!("rb97d96f624")
        );
    }
    anyhow::ensure!(
        completed.id
            == recovery_batch_id(
                &completed.scope,
                &completed.agent,
                &completed.chat_id,
                source_ids,
            ),
        summon_i18n::format!("r11a9dd3cfa")
    );
    Ok(completed)
}

fn read_completed_recovery_batch_optional(
    path: &Path,
) -> anyhow::Result<Option<CompletedRecoveryBatch>> {
    match read_completed_recovery_batch_checked(path) {
        Ok(completed) => Ok(Some(completed)),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn recovery_batch_id(
    scope: &str,
    agent: &str,
    chat_id: &str,
    message_ids: impl IntoIterator<Item = String>,
) -> String {
    let identity = format!(
        "{}\0{}\0{}\0{}",
        scope,
        agent,
        chat_id,
        message_ids.into_iter().collect::<Vec<_>>().join("\0")
    );
    summon_core::outbox::recovery_delivery_id(&identity)
}

fn read_recovery_batch_optional(path: &Path) -> anyhow::Result<Option<RecoveryBatch>> {
    match read_recovery_batch(path) {
        Ok(batch) => Ok(Some(batch)),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn same_recovery_batch_identity(left: &RecoveryBatch, right: &RecoveryBatch) -> bool {
    left.id == right.id
        && left.outbox_id == right.outbox_id
        && left.scope == right.scope
        && left.agent == right.agent
        && left.chat_id == right.chat_id
        && left.chat_type == right.chat_type
        && left.items.len() == right.items.len()
        && left
            .items
            .iter()
            .zip(&right.items)
            .all(|(left, right)| left.record == right.record && left.preview == right.preview)
}

fn validate_recovery_callback_identity(
    batch: &RecoveryBatch,
    scope: &str,
    agent: &str,
    chat_id: &str,
    card_message_id: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        batch.scope == scope
            && batch.agent == agent
            && batch.chat_id == chat_id
            && batch.card_message_id.as_deref() == Some(card_message_id),
        summon_i18n::format!("r40f707068b")
    );
    Ok(())
}

fn recovery_replay_record(
    batch_id: &str,
    original_message_id: &str,
    source: &InboxRecord,
) -> (String, InboxRecord) {
    let replay_id = summon_core::outbox::recovery_replay_id(batch_id, original_message_id);
    let mut replay = source.clone();
    replay.inbound.message_id = replay_id.clone();
    // 合成 id 只当本地幂等键;飞书那边的表情/回复/CoT 仍要落在**原消息**上。
    replay.inbound.anchor_message_id = Some(original_message_id.to_string());
    // 重放免时效闸走 replayed 标记,create_time_ms 保真(此前归零,WAL 丢真实发送时间)。
    replay.inbound.replayed = true;
    // 恢复卡投出的是**一条新的普通轮**:它带合成 message_id,盘上锚点认领的是原始 id,拿接管
    // 形态去查恒判假 —— 又退一张恢复卡,死循环。接管的机会已经在退卡那一刻用掉了。
    replay.inbound.reattach = false;
    replay.started = false;
    replay.recovery_origin = Some(
        source
            .recovery_origin
            .clone()
            .unwrap_or_else(|| original_message_id.to_string()),
    );
    replay.recovery_batch_id = Some(batch_id.to_string());
    (replay_id, replay)
}

fn validate_recovery_replay(
    actual: &InboxRecord,
    expected: &InboxRecord,
    replay_id: &str,
) -> anyhow::Result<()> {
    // 飞书锚点是**派生字段**(由 batch + origin 推出),不参与身份判定 —— 比较前两边一起抹平。
    //
    // 它必须被排除,否则「盘上早于本次升级写下的 replay 记录」会与现算的 expected 不等,而
    // 这条校验挂了就是 daemon **拒绝启动**(冷启完整性预检 fail-closed)。身份该由 agent /
    // scope / chat / replay_id / origin / batch 这几项决定,它们一个没少。
    let normalize = |record: &InboxRecord| {
        let mut inbound = record.inbound.clone();
        inbound.anchor_message_id = None;
        inbound
    };
    anyhow::ensure!(
        actual.agent == expected.agent
            && actual.delivery_scope == expected.delivery_scope
            && actual.inbound.chat_id == expected.inbound.chat_id
            && actual.inbound.message_id == replay_id
            && actual.recovery_origin == expected.recovery_origin
            && actual.recovery_batch_id == expected.recovery_batch_id
            && actual.scheduled.is_none()
            && normalize(actual) == normalize(expected),
        summon_i18n::format!("r995acb3139")
    );
    Ok(())
}

fn validate_wal_transition_identity(
    record: &InboxRecord,
    agent: &str,
    delivery_scope: &str,
    message_id: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(record.agent == agent, summon_i18n::format!("r99999ef109"));
    anyhow::ensure!(
        record.delivery_scope == delivery_scope,
        summon_i18n::format!("r4129a471eb")
    );
    anyhow::ensure!(
        record.inbound.message_id == message_id,
        summon_i18n::format!("r4e7899c9a3")
    );
    Ok(())
}

/// `consumed/` 与 `recalled/` 保留期:7 天后启动时清理(够排查、不无限涨)。
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn cleanup_dir_older_than(dir: &Path, now_ms: u64, retention_ms: u64) {
    let cutoff = now_ms.saturating_sub(retention_ms);
    for p in json_paths_recursive(dir) {
        let too_old = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| (d.as_millis() as u64) < cutoff);
        if too_old {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// message_id 落成文件名前的兜底:只留 `[A-Za-z0-9_-]`,其余换 `_`(飞书 `om_xxx` 通常已安全,
/// 防御异常 id 带 `/`/`.` 破坏路径或与 `.json` 后缀冲突)。
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// agent display name 允许任意 Unicode，不能用有损 sanitize 作为 namespace（星光/雨云会同桶）。
/// SHA-256 只做目录键；每个 JSON 仍携带原 agent 并在读取时精确复验。
fn agent_bucket(agent: &str) -> String {
    use std::fmt::Write as _;
    let digest = ring::digest::digest(&ring::digest::SHA256, agent.as_bytes());
    let mut encoded = String::with_capacity(6 + digest.as_ref().len() * 2);
    encoded.push_str("agent-");
    for byte in digest.as_ref() {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn read_record(path: &Path) -> anyhow::Result<Option<InboxRecord>> {
    match std::fs::read_to_string(path) {
        Ok(json) => {
            Ok(Some(serde_json::from_str(&json).map_err(|e| {
                anyhow::anyhow!(summon_i18n::format!("r30f0468835"))
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn validate_sentinel_record(
    rec: &InboxRecord,
    agent: &str,
    delivery_scope: &str,
    expected: &SentinelInbox,
) -> anyhow::Result<()> {
    if rec.agent != agent
        || rec.delivery_scope != delivery_scope
        || rec.sentinel.as_ref() != Some(expected)
    {
        anyhow::bail!(summon_i18n::format!(
            "r8640042af9",
            expected.fire_id,
            rec.agent,
            rec.sentinel
        ));
    }
    Ok(())
}

fn validate_scheduled_record(
    rec: &InboxRecord,
    agent: &str,
    delivery_scope: &str,
    expected: &ScheduledInbox,
) -> anyhow::Result<()> {
    if rec.agent != agent
        || rec.delivery_scope != delivery_scope
        || rec.scheduled.as_ref() != Some(expected)
    {
        anyhow::bail!(summon_i18n::format!(
            "r24a3da76c4",
            expected.fire_id,
            rec.agent,
            rec.scheduled
        ));
    }
    Ok(())
}

/// 递归读 agent 分桶和升级前 flat 目录下所有 `*.json` 记录(损坏项宽松跳过)。
fn read_records_recursive(dir: &Path) -> Vec<InboxRecord> {
    let mut out = Vec::new();
    for path in json_paths_recursive(dir) {
        if let Some(rec) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<InboxRecord>(&s).ok())
        {
            out.push(rec);
        }
    }
    out
}

fn json_paths_recursive(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            paths.extend(json_paths_recursive(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

fn json_paths_recursive_strict(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            paths.extend(json_paths_recursive_strict(&path)?);
        } else if metadata.is_file()
            && path.extension().and_then(|ext| ext.to_str()) == Some("json")
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn valid_recovery_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    /// 上下文提醒卡发起的交接记在 sentinel 载荷里;没有这个字段的旧记录照常读成普通代投消息,
    /// 新写的普通代投消息也不多出这个键。
    #[test]
    fn sentinel_handoff_note_is_optional_on_disk() {
        let legacy: SentinelInbox = serde_json::from_str(
            r#"{"chat_id":"oc_1","chat_type":"group","prompt":"p","fire_id":"f","who":"w","who_id":"ou_1","who_union_id":""}"#,
        )
        .unwrap();
        assert_eq!(legacy.handoff_note, None);
        assert!(!serde_json::to_string(&legacy)
            .unwrap()
            .contains("handoff_note"));
        let handoff = SentinelInbox {
            handoff_note: Some("先修登录".into()),
            ..legacy
        };
        let back: SentinelInbox =
            serde_json::from_str(&serde_json::to_string(&handoff).unwrap()).unwrap();
        assert_eq!(back.handoff_note.as_deref(), Some("先修登录"));
        match back.fire() {
            crate::runtime::daemon::DispatchMsg::SentinelFire {
                fire_id,
                who_id,
                handoff_note,
                ..
            } => {
                assert_eq!(fire_id, "f");
                assert_eq!(who_id, "ou_1");
                assert_eq!(
                    handoff_note.as_deref(),
                    Some("先修登录"),
                    "重投要带着交接备注,否则那一轮不再是交接轮"
                );
            }
            _ => unreachable!(),
        }
    }

    /// **升级兼容**:盘上早于锚点字段写下的 replay 记录(`anchor_message_id: None`)必须仍能通过
    /// 身份校验。这条挂了不是「一条恢复失败」,而是 **daemon 拒绝启动** —— 冷启完整性预检是
    /// fail-closed 的,整台机器的全部 agent 一起下线(2026-09-01 devbox 实际发生)。
    #[test]
    fn recovery_replay_identity_ignores_the_derived_anchor() {
        let base = InboxRecord {
            agent: "a".into(),
            delivery_scope: "scope".into(),
            inbound: crate::runtime::daemon::Inbound {
                anchor_message_id: None,
                chat_id: "oc_x".into(),
                chat_type: Some("group".into()),
                text: "hi".into(),
                sender: "ou_x".into(),
                sender_union_id: String::new(),
                sender_type: "user".into(),
                message_id: "om_real".into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: Vec::new(),
                resources: Vec::new(),
                create_time_ms: 1,
                replayed: false,
                reattach: false,
            },
            started: true,
            scheduled: None,
            sentinel: None,
            rejected_disabled: false,
            superseded_by_new: false,
            recovery_origin: None,
            recovery_batch_id: None,
        };
        let (replay_id, expected) = recovery_replay_record("batch-1", "om_real", &base);

        // 盘上那条是升级前写的:一切相同,只差派生出来的锚点。
        let mut on_disk = expected.clone();
        on_disk.inbound.anchor_message_id = None;
        validate_recovery_replay(&on_disk, &expected, &replay_id)
            .expect("旧记录缺派生锚点不该判成身份冲突");

        // 真正的身份冲突照旧要拦住。
        let mut wrong = expected.clone();
        wrong.inbound.chat_id = "oc_other".into();
        assert!(validate_recovery_replay(&wrong, &expected, &replay_id).is_err());
    }

    /// 恢复重放把 `message_id` 换成本地幂等键,**真实的那条必须原样带进 `anchor_message_id`** ——
    /// 下游贴表情 / 回复 / 建原生 CoT 全靠它;丢了就是整轮回执 400(`99992354`)。
    #[test]
    fn recovery_replay_keeps_the_real_message_id_as_the_feishu_anchor() {
        let source = InboxRecord {
            agent: "a".into(),
            delivery_scope: "scope".into(),
            inbound: crate::runtime::daemon::Inbound {
                anchor_message_id: None,
                chat_id: "oc_x".into(),
                chat_type: Some("group".into()),
                text: "hi".into(),
                sender: "ou_x".into(),
                sender_union_id: String::new(),
                sender_type: "user".into(),
                message_id: "om_real".into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: Vec::new(),
                resources: Vec::new(),
                create_time_ms: 1,
                replayed: false,
                reattach: false,
            },
            started: true,
            scheduled: None,
            sentinel: None,
            rejected_disabled: false,
            superseded_by_new: false,
            recovery_origin: None,
            recovery_batch_id: None,
        };
        let (replay_id, replay) = recovery_replay_record("batch-1", "om_real", &source);
        assert!(replay_id.starts_with("recovery-run-"));
        assert_eq!(replay.inbound.message_id, replay_id, "本地键换成合成 id");
        assert_eq!(
            replay.inbound.anchor_message_id.as_deref(),
            Some("om_real"),
            "飞书锚点必须还是原消息"
        );
    }

    /// 恢复卡投出的重放是一条**普通轮**。带着接管形态出去,chat 线程会拿合成 message_id 去查
    /// 锚点认领的原始 id —— 恒判不符,又退一张恢复卡,用户每点一次「恢复」就多一张卡。
    #[test]
    fn recovery_replay_drops_the_reattach_form() {
        let source = InboxRecord {
            agent: "a".into(),
            delivery_scope: "scope".into(),
            inbound: crate::runtime::daemon::Inbound {
                anchor_message_id: None,
                chat_id: "oc_x".into(),
                chat_type: Some("group".into()),
                text: "上一代注入过的那条".into(),
                sender: "ou_x".into(),
                sender_union_id: String::new(),
                sender_type: "user".into(),
                message_id: "om_real".into(),
                parent_id: None,
                thread_id: None,
                root_id: None,
                mentions: Vec::new(),
                resources: Vec::new(),
                create_time_ms: 1,
                replayed: false,
                reattach: true,
            },
            started: true,
            scheduled: None,
            sentinel: None,
            rejected_disabled: false,
            superseded_by_new: false,
            recovery_origin: None,
            recovery_batch_id: None,
        };
        let (_, replay) = recovery_replay_record("batch-1", "om_real", &source);
        assert!(
            !replay.inbound.reattach,
            "恢复卡重放必须以普通轮形态出去,否则锚点恒对不上、卡片无限自繁殖"
        );
    }

    use super::*;

    #[test]
    fn legacy_scheduled_inbox_defaults_to_progress_output() {
        let legacy = serde_json::json!({
            "chat_id": "oc_1",
            "chat_type": "p2p",
            "prompt": "巡检",
            "fire_id": "sched-1"
        });
        let parsed: ScheduledInbox = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.output, summon_core::ScheduleOutput::Progress);
        assert_eq!(parsed.context, summon_core::ScheduleContext::Resume);
    }

    fn mk_inbound(message_id: &str, text: &str) -> Inbound {
        Inbound {
            anchor_message_id: None,
            chat_id: "oc_1".into(),
            chat_type: Some("p2p".into()),
            text: text.into(),
            sender: "ou_1".into(),
            sender_union_id: "on_1".into(),
            sender_type: String::new(),
            message_id: message_id.into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: vec![],
            resources: vec![],
            create_time_ms: 0,
            replayed: false,
            reattach: false,
        }
    }

    fn set_enabled(wal: &InboxWal, enabled: bool) {
        super::super::agent_availability::AgentAvailability::lock(&wal.root, "agent-a")
            .unwrap()
            .set_enabled(enabled)
            .unwrap();
    }

    #[test]
    fn disabled_agent_rejects_queued_work_but_preserves_started_turn() {
        let root = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(root.path().join("inbox"));
        for message in ["running", "queued", "followup"] {
            wal.append("agent-a", "scope-a", &mk_inbound(message, "任务"))
                .unwrap();
        }
        assert_eq!(
            wal.mark_started("agent-a", "scope-a", "running").unwrap(),
            MarkStartedOutcome::Marked
        );
        set_enabled(&wal, false);
        let restarted = InboxWal::new(root.path().join("inbox"));
        assert!(!restarted.agent_enabled("agent-a").unwrap());
        assert_eq!(
            restarted
                .mark_started("agent-a", "scope-a", "running")
                .unwrap(),
            MarkStartedOutcome::AlreadyStarted
        );
        for message in ["queued", "followup"] {
            assert_eq!(
                restarted
                    .mark_started("agent-a", "scope-a", message)
                    .unwrap(),
                MarkStartedOutcome::Disabled
            );
            let consumed = restarted
                .read_consumed("agent-a", message)
                .unwrap()
                .unwrap();
            assert!(!consumed.started);
            assert!(consumed.rejected_disabled);
            assert_eq!(
                restarted
                    .mark_started("agent-a", "scope-a", message)
                    .unwrap(),
                MarkStartedOutcome::AlreadyConsumed
            );
        }
        assert_eq!(
            restarted
                .mark_consumed_after_start("agent-a", "scope-a", "running")
                .unwrap(),
            MarkConsumedOutcome::Consumed
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("outbox/pending"))
                .unwrap()
                .count(),
            2
        );
        set_enabled(&wal, true);
        wal.append("agent-a", "scope-a", &mk_inbound("future", "任务"))
            .unwrap();
        assert_eq!(
            wal.mark_started("agent-a", "scope-a", "future").unwrap(),
            MarkStartedOutcome::Marked
        );
        assert_eq!(
            wal.mark_started("agent-a", "scope-a", "queued").unwrap(),
            MarkStartedOutcome::AlreadyConsumed
        );
    }

    #[test]
    fn disabled_schedule_notifies_once_with_schedule_id_even_for_result_output() {
        let root = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(root.path().join("inbox"));
        let append = || {
            wal.append_scheduled(
                "agent-a",
                "scope-a",
                "oc_1",
                "group",
                "私有任务内容",
                summon_core::ScheduleOutput::Result,
                summon_core::ScheduleContext::Fresh,
                "sched-SCH-17-0",
                123,
            )
        };
        assert_eq!(append().unwrap(), ScheduledAppend::Inserted);
        set_enabled(&wal, false);
        assert!(wal
            .reject_disabled("agent-a", "scope-a", "sched-SCH-17-0")
            .unwrap());
        assert!(wal
            .reject_disabled("agent-a", "scope-a", "sched-SCH-17-0")
            .unwrap());
        assert_eq!(append().unwrap(), ScheduledAppend::Consumed);
        let outbox = super::super::outbox_store::OutboxStore::new(root.path().join("outbox"));
        let id =
            summon_core::outbox::agent_disabled_delivery_id("agent-a", "scope-a", "sched-SCH-17-0");
        let (_, event) = outbox.find(&id).unwrap().unwrap();
        let summon_core::outbox::OutboxKind::AgentDisabled {
            rendered_card,
            chat_id,
            chat_type,
        } = event.kind
        else {
            panic!("禁用通知类型")
        };
        assert_eq!(chat_id, "oc_1");
        assert_eq!(chat_type, "group");
        assert!(rendered_card.contains("定时任务 SCH-17 未正常启动"));
        assert!(!rendered_card.contains("私有任务内容"));
        assert_eq!(
            std::fs::read_dir(root.path().join("outbox/pending"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn disabled_rejection_survives_outbox_failure_and_enable() {
        let root = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(root.path().join("inbox"));
        wal.append("agent-a", "scope-a", &mk_inbound("queued", "任务"))
            .unwrap();
        set_enabled(&wal, false);
        std::fs::write(root.path().join("outbox"), "blocked").unwrap();
        assert!(wal.mark_started("agent-a", "scope-a", "queued").is_err());
        let pending = wal.scan_pending().pop().unwrap();
        assert!(!pending.started);
        assert!(pending.rejected_disabled);
        set_enabled(&wal, true);
        std::fs::remove_file(root.path().join("outbox")).unwrap();
        wal.append("agent-a", "scope-a", &mk_inbound("queued", "任务"))
            .unwrap();
        assert_eq!(
            wal.mark_started("agent-a", "scope-a", "queued").unwrap(),
            MarkStartedOutcome::Disabled
        );
        assert!(wal.scan_pending().is_empty());
    }

    #[test]
    fn disabled_rejection_retries_consumption_without_duplicate_notification() {
        let root = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(root.path().join("inbox"));
        wal.append("agent-a", "scope-a", &mk_inbound("queued", "任务"))
            .unwrap();
        set_enabled(&wal, false);
        assert!(wal.reject_disabled("agent-a", "scope-a", "queued").unwrap());
        std::fs::remove_dir_all(wal.pending_dir()).unwrap();
        std::fs::rename(wal.consumed_dir(), wal.pending_dir()).unwrap();
        assert_eq!(
            std::fs::read_dir(root.path().join("outbox/pending"))
                .unwrap()
                .count(),
            1
        );
        set_enabled(&wal, true);
        assert!(wal.reject_disabled("agent-a", "scope-a", "queued").unwrap());
        assert_eq!(
            std::fs::read_dir(root.path().join("outbox/pending"))
                .unwrap()
                .count(),
            1
        );
        assert!(wal.scan_pending().is_empty());
    }

    #[test]
    fn disabled_rejection_requires_exact_delivery_identity() {
        let root = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(root.path().join("inbox"));
        wal.append("agent-a", "scope-a", &mk_inbound("queued", "任务"))
            .unwrap();
        set_enabled(&wal, false);
        assert!(wal.reject_disabled("agent-a", "scope-b", "queued").is_err());
        assert!(!root.path().join("outbox").exists());
        assert!(!wal.scan_pending()[0].rejected_disabled);
    }

    #[test]
    fn disabled_recovery_replay_is_terminal_after_restart() {
        let (_root, wal, batch, replay_id, _) = setup_claimed_recovery();
        set_enabled(&wal, false);
        assert_eq!(
            wal.mark_started("agent-a", "scope-a", &replay_id).unwrap(),
            MarkStartedOutcome::Disabled
        );
        set_enabled(&wal, true);
        wal.repair_pending_recovery_claims().unwrap();
        assert!(wal.load_recovery_batch(&batch.id).unwrap().is_none());
        assert!(wal.scan_pending().is_empty());
    }

    fn setup_half_committed_recovery_claim() -> (
        tempfile::TempDir,
        InboxWal,
        RecoveryBatch,
        String,
        InboxRecord,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        wal.append("agent-a", "scope-a", &mk_inbound("om-original", "执行一次"))
            .unwrap();
        let source = wal.scan_pending().pop().unwrap();
        let batch = wal
            .create_recovery_batch(
                "scope-a",
                "agent-a",
                "oc_1",
                "p2p",
                None,
                vec![source.clone()],
                100,
            )
            .unwrap();
        wal.finalize_recovery_sources(&batch).unwrap();
        let (replay_id, replay) =
            recovery_replay_record(&batch.id, &source.inbound.message_id, &source);
        atomic_write_mode(
            &wal.pending_path("agent-a", &replay_id),
            &serde_json::to_vec_pretty(&replay).unwrap(),
            0o600,
        )
        .unwrap();
        (tmp, wal, batch, replay_id, replay)
    }

    fn setup_claimed_recovery() -> (
        tempfile::TempDir,
        InboxWal,
        RecoveryBatch,
        String,
        InboxRecord,
    ) {
        let (tmp, wal, batch, replay_id, replay) = setup_half_committed_recovery_claim();
        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        assert!(matches!(
            wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
            RecoveryItemState::Claimed { .. }
        ));
        (tmp, wal, batch, replay_id, replay)
    }

    fn setup_completed_recovery() -> (tempfile::TempDir, InboxWal, RecoveryBatch, PathBuf) {
        let (tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
        set_replay_started(&wal, &replay_id, true);
        move_replay_to_consumed(&wal, &replay_id, false);
        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        let completed_path = wal.completed_recovery_path(&batch.id).unwrap();
        assert!(completed_path.is_file());
        (tmp, wal, batch, completed_path)
    }

    #[test]
    fn resume_sources_require_exact_started_primary_and_followups() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        for id in ["om-primary", "om-followup"] {
            wal.append("agent-a", "scope-a", &mk_inbound(id, id))
                .unwrap();
            assert_eq!(
                wal.mark_started("agent-a", "scope-a", id).unwrap(),
                MarkStartedOutcome::Marked
            );
        }
        let exact = vec!["om-followup".to_string(), "om-primary".to_string()];
        assert!(wal
            .resume_sources_match("agent-a", "scope-a", "oc_1", "p2p", &exact)
            .unwrap());
        for invalid in [
            vec!["om-primary".to_string()],
            vec![
                "om-primary".to_string(),
                "om-followup".to_string(),
                "om-extra".to_string(),
            ],
            vec!["om-primary".to_string(), "om-primary".to_string()],
            Vec::new(),
        ] {
            assert!(
                !wal.resume_sources_match("agent-a", "scope-a", "oc_1", "p2p", &invalid)
                    .unwrap(),
                "缺失/多余/重复/空 id 均不得自动续跑: {invalid:?}"
            );
        }

        wal.mark_consumed_after_start("agent-a", "scope-a", "om-primary")
            .unwrap();
        assert!(
            !wal.resume_sources_match("agent-a", "scope-a", "oc_1", "p2p", &["om-primary".into()])
                .unwrap(),
            "已 consumed 的 id 不能授权 checkpoint"
        );
    }

    #[test]
    fn resume_sources_reject_scheduled_and_recovery_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        wal.append_scheduled(
            "agent-a",
            "scope-a",
            "oc_1",
            "p2p",
            "巡检",
            summon_core::ScheduleOutput::Progress,
            summon_core::ScheduleContext::Resume,
            "sched-1",
            1,
        )
        .unwrap();
        wal.mark_started("agent-a", "scope-a", "sched-1").unwrap();
        assert!(!wal
            .resume_sources_match("agent-a", "scope-a", "oc_1", "p2p", &["sched-1".into()])
            .unwrap());

        let (_tmp, recovery_wal, _batch, replay_id, _) = setup_claimed_recovery();
        recovery_wal
            .mark_started("agent-a", "scope-a", &replay_id)
            .unwrap();
        assert!(
            !recovery_wal
                .resume_sources_match(
                    "agent-a",
                    "scope-a",
                    "oc_1",
                    "p2p",
                    std::slice::from_ref(&replay_id),
                )
                .unwrap(),
            "recovery replay 只能由 RecoveryBatch 协议恢复"
        );
    }

    fn move_replay_to_consumed(wal: &InboxWal, replay_id: &str, legacy: bool) -> PathBuf {
        let from = wal.pending_path("agent-a", replay_id);
        let to = if legacy {
            wal.consumed_dir()
                .join(format!("{}.json", sanitize_id(replay_id)))
        } else {
            wal.consumed_path("agent-a", replay_id)
        };
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::rename(from, &to).unwrap();
        to
    }

    fn set_replay_started(wal: &InboxWal, replay_id: &str, started: bool) {
        let path = wal.pending_path("agent-a", replay_id);
        let mut replay = read_record(&path).unwrap().unwrap();
        replay.started = started;
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&replay).unwrap(), 0o600).unwrap();
    }

    #[test]
    fn strict_scan_rejects_corrupt_pending_json() {
        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        std::fs::create_dir_all(wal.pending_dir()).unwrap();
        let corrupt = wal.pending_dir().join("om-corrupt.json");
        std::fs::write(&corrupt, b"{").unwrap();

        let error = wal.scan_pending_strict().unwrap_err();
        assert!(
            format!("{error:#}").contains("解析"),
            "坏 JSON 必须向上传播，而不是被解释成空队列:{error:#}"
        );
        assert!(corrupt.exists(), "严格扫描只诊断，不得擅自删除证据");
    }

    #[test]
    fn strict_scan_canonicalizes_exact_legacy_duplicate_but_rejects_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        wal.append(
            "agent-a",
            "scope-a",
            &mk_inbound("om-duplicate", "同一载荷"),
        )
        .unwrap();
        let scoped = wal.pending_path("agent-a", "om-duplicate");
        let legacy = wal.pending_dir().join("om-duplicate.json");
        std::fs::copy(&scoped, &legacy).unwrap();

        let records = wal.scan_pending_strict().unwrap();
        assert_eq!(records.len(), 1, "升级窗口里的等价双副本只恢复一次");

        let mut conflicting = read_record(&legacy).unwrap().unwrap();
        conflicting.inbound.text = "漂移载荷".into();
        atomic_write_mode(
            &legacy,
            &serde_json::to_vec_pretty(&conflicting).unwrap(),
            0o600,
        )
        .unwrap();
        let error = wal.scan_pending_strict().unwrap_err();
        assert!(
            format!("{error:#}").contains("同 identity 载荷冲突"),
            "同 ID 不同正文必须 fail-closed:{error:#}"
        );
    }

    #[test]
    fn strict_scan_checks_conflicts_before_recovery_managed_suppression() {
        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        wal.append("agent-a", "scope-a", &mk_inbound("om-managed", "权威原文"))
            .unwrap();
        let source = wal.scan_pending_strict().unwrap().pop().unwrap();
        wal.create_recovery_batch(
            "scope-a",
            "agent-a",
            "oc_1",
            "p2p",
            None,
            vec![source.clone()],
            100,
        )
        .unwrap();
        let mut conflicting = source;
        conflicting.inbound.text = "冲突原文".into();
        atomic_write_mode(
            &wal.pending_dir().join("om-managed.json"),
            &serde_json::to_vec_pretty(&conflicting).unwrap(),
            0o600,
        )
        .unwrap();

        let error = wal.scan_pending_strict().unwrap_err();
        assert!(
            format!("{error:#}").contains("同 identity 载荷冲突"),
            "active recovery 已接管也不能掩盖磁盘冲突:{error:#}"
        );
    }

    #[test]
    fn strict_scan_rejects_path_namespace_and_missing_scope() {
        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        wal.append("agent-a", "scope-a", &mk_inbound("om-path", "路径校验"))
            .unwrap();
        let scoped = wal.pending_path("agent-a", "om-path");
        let wrong_bucket = wal.pending_dir().join("agent-wrong");
        std::fs::create_dir_all(&wrong_bucket).unwrap();
        let misplaced = wrong_bucket.join("om-path.json");
        std::fs::rename(&scoped, &misplaced).unwrap();
        let error = wal.scan_pending_strict().unwrap_err();
        assert!(
            format!("{error:#}").contains("agent namespace"),
            "记录不能从别的 agent 桶被接管:{error:#}"
        );

        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        wal.append("agent-a", "scope-a", &mk_inbound("om-no-scope", "旧身份"))
            .unwrap();
        let path = wal.pending_path("agent-a", "om-no-scope");
        let mut record = read_record(&path).unwrap().unwrap();
        record.delivery_scope.clear();
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&record).unwrap(), 0o600).unwrap();
        assert!(
            wal.scan_pending_strict().is_err(),
            "旧 schema 无法证明 app scope，冷启不得猜当前配置"
        );
    }

    #[test]
    fn strict_scan_rejects_invalid_replay_and_scheduled_identity() {
        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        wal.append("agent-a", "scope-a", &mk_inbound("om-replay", "恢复载荷"))
            .unwrap();
        let replay_path = wal.pending_path("agent-a", "om-replay");
        let mut replay = read_record(&replay_path).unwrap().unwrap();
        replay.recovery_origin = Some("om-origin".into());
        atomic_write_mode(
            &replay_path,
            &serde_json::to_vec_pretty(&replay).unwrap(),
            0o600,
        )
        .unwrap();
        let error = wal.scan_pending_strict().unwrap_err();
        assert!(
            format!("{error:#}").contains("replay identity 不完整"),
            "recovery origin/batch 必须成对:{error:#}"
        );

        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        let batch_id = "recovery-orphan-batch";
        let origin = "om-orphan-origin";
        let replay_id = summon_core::outbox::recovery_replay_id(batch_id, origin);
        wal.append("agent-a", "scope-a", &mk_inbound(&replay_id, "孤儿 replay"))
            .unwrap();
        let replay_path = wal.pending_path("agent-a", &replay_id);
        let mut replay = read_record(&replay_path).unwrap().unwrap();
        replay.recovery_origin = Some(origin.into());
        replay.recovery_batch_id = Some(batch_id.into());
        atomic_write_mode(
            &replay_path,
            &serde_json::to_vec_pretty(&replay).unwrap(),
            0o600,
        )
        .unwrap();
        let error = wal.scan_pending_strict().unwrap_err();
        assert!(
            format!("{error:#}").contains("active batch 不存在"),
            "仅有自洽 ID、没有 durable 授权批次的 replay 不得自动执行:{error:#}"
        );

        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        wal.append_scheduled(
            "agent-a",
            "scope-a",
            "oc_1",
            "p2p",
            "定时任务",
            summon_core::ScheduleOutput::Progress,
            summon_core::ScheduleContext::Resume,
            "sched-fire-1",
            100,
        )
        .unwrap();
        let scheduled_path = wal.pending_path("agent-a", "sched-fire-1");
        let mut scheduled = read_record(&scheduled_path).unwrap().unwrap();
        scheduled.inbound.text = "漂移任务".into();
        atomic_write_mode(
            &scheduled_path,
            &serde_json::to_vec_pretty(&scheduled).unwrap(),
            0o600,
        )
        .unwrap();
        let error = wal.scan_pending_strict().unwrap_err();
        assert!(
            format!("{error:#}").contains("scheduled identity/payload 不一致"),
            "定时 WAL 的镜像载荷必须完全一致:{error:#}"
        );
    }

    #[test]
    fn repairs_half_committed_recovery_claim_before_replay_scan() {
        let (_tmp, wal, batch, replay_id, _) = setup_half_committed_recovery_claim();
        assert_eq!(
            wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
            RecoveryItemState::Awaiting,
            "夹具必须精确模拟 pending replay 已落盘、batch 尚 Awaiting 的 crash window"
        );

        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        let repaired = wal.load_recovery_batch(&batch.id).unwrap().unwrap();
        assert_eq!(
            repaired.items[0].state,
            RecoveryItemState::Claimed {
                replay_id: replay_id.clone()
            }
        );
        let pending = wal.scan_pending();
        assert_eq!(pending.len(), 1, "修复只能接管既有 replay，不能再生成一份");
        assert_eq!(pending[0].inbound.message_id, replay_id);
        assert_eq!(
            pending[0].recovery_batch_id.as_deref(),
            Some(batch.id.as_str())
        );

        // 模拟普通 inbox 只执行这一个既有 replay 并销账：修复后的 Claimed 能正常推进 Completed
        // 并终态化 batch，不会留下 Awaiting 供第二次点击再生成/执行。
        wal.mark_consumed("agent-a", &pending[0].inbound.message_id);
        assert!(wal.scan_pending().is_empty());
        assert!(
            wal.load_recovery_batch(&batch.id).unwrap().is_none(),
            "一次执行完成后 active batch 应终态化，不能再被点击"
        );
    }

    #[test]
    fn half_committed_recovery_claim_repair_is_idempotent() {
        let (_tmp, wal, batch, replay_id, _) = setup_half_committed_recovery_claim();
        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        let batch_path = wal.recovery_path(&batch.id).unwrap();
        let first_batch = std::fs::read(&batch_path).unwrap();
        let first_pending = std::fs::read(wal.pending_path("agent-a", &replay_id)).unwrap();

        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 0);
        assert_eq!(std::fs::read(batch_path).unwrap(), first_batch);
        assert_eq!(
            std::fs::read(wal.pending_path("agent-a", &replay_id)).unwrap(),
            first_pending,
            "幂等修复不得重写或再生成 replay"
        );
        assert_eq!(wal.scan_pending().len(), 1);
    }

    #[test]
    fn half_committed_recovery_claim_rejects_wrong_identity() {
        for field in ["scope", "agent", "chat", "origin", "batch"] {
            let (_tmp, wal, batch, replay_id, mut replay) = setup_half_committed_recovery_claim();
            match field {
                "scope" => replay.delivery_scope = "scope-other".into(),
                "agent" => replay.agent = "agent-other".into(),
                "chat" => replay.inbound.chat_id = "oc_other".into(),
                "origin" => replay.recovery_origin = Some("om-other".into()),
                "batch" => replay.recovery_batch_id = Some("recovery-other".into()),
                _ => unreachable!(),
            }
            atomic_write_mode(
                &wal.pending_path("agent-a", &replay_id),
                &serde_json::to_vec_pretty(&replay).unwrap(),
                0o600,
            )
            .unwrap();

            let error = wal.repair_pending_recovery_claims().unwrap_err();
            assert!(
                error.to_string().contains("identity 冲突"),
                "{field} 冲突应 fail-closed: {error:#}"
            );
            assert_eq!(
                wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
                RecoveryItemState::Awaiting,
                "{field} 冲突不得推进 batch"
            );
        }
    }

    #[test]
    fn finalize_recovery_sources_rejects_any_source_payload_drift_and_preserves_file() {
        for field in [
            "scope",
            "body",
            "sender",
            "resources",
            "origin",
            "batch",
            "started",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let wal = InboxWal::new(tmp.path().join("inbox"));
            wal.append("agent-a", "scope-a", &mk_inbound("om-original", "执行一次"))
                .unwrap();
            let source = wal.scan_pending().pop().unwrap();
            let batch = wal
                .create_recovery_batch("scope-a", "agent-a", "oc_1", "p2p", None, vec![source], 100)
                .unwrap();
            let source_path = wal.pending_path("agent-a", "om-original");
            let mut drifted = read_record(&source_path).unwrap().unwrap();
            match field {
                "scope" => drifted.delivery_scope = "scope-other".into(),
                "body" => drifted.inbound.text = "另一条命令".into(),
                "sender" => drifted.inbound.sender = "ou-other".into(),
                "resources" => drifted.inbound.resources.push(summon_core::ipc::Resource {
                    file_key: "file-other".into(),
                    kind: "file".into(),
                    name: "secret.txt".into(),
                }),
                "origin" => drifted.recovery_origin = Some("om-other".into()),
                "batch" => drifted.recovery_batch_id = Some("recovery-other".into()),
                "started" => drifted.started = !drifted.started,
                _ => unreachable!(),
            }
            atomic_write_mode(
                &source_path,
                &serde_json::to_vec_pretty(&drifted).unwrap(),
                0o600,
            )
            .unwrap();

            let error = wal.finalize_recovery_sources(&batch).unwrap_err();
            assert!(
                format!("{error:#}").contains("完整载荷冲突"),
                "{field} 漂移必须 fail-closed: {error:#}"
            );
            assert!(source_path.is_file(), "{field} 漂移时绝不能删除 source");
            assert_eq!(read_record(&source_path).unwrap().unwrap(), drifted);
        }
    }

    #[test]
    fn finalize_recovery_sources_removes_exact_scoped_and_legacy_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        wal.append("agent-a", "scope-a", &mk_inbound("om-original", "执行一次"))
            .unwrap();
        let source = wal.scan_pending().pop().unwrap();
        let batch = wal
            .create_recovery_batch("scope-a", "agent-a", "oc_1", "p2p", None, vec![source], 100)
            .unwrap();
        let scoped = wal.pending_path("agent-a", "om-original");
        let legacy = wal.pending_dir().join("om-original.json");
        std::fs::copy(&scoped, &legacy).unwrap();

        wal.finalize_recovery_sources(&batch).unwrap();
        assert!(!scoped.exists());
        assert!(!legacy.exists());
    }

    #[test]
    fn finalize_recovery_sources_validates_entire_batch_before_deleting_any_source() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        for id in ["om-a", "om-b"] {
            wal.append("agent-a", "scope-a", &mk_inbound(id, id))
                .unwrap();
        }
        let batch = wal
            .create_recovery_batch(
                "scope-a",
                "agent-a",
                "oc_1",
                "p2p",
                None,
                wal.scan_pending(),
                100,
            )
            .unwrap();
        let second = wal.pending_path("agent-a", "om-b");
        let mut drifted = read_record(&second).unwrap().unwrap();
        drifted.inbound.text = "drifted".into();
        atomic_write_mode(
            &second,
            &serde_json::to_vec_pretty(&drifted).unwrap(),
            0o600,
        )
        .unwrap();

        assert!(wal.finalize_recovery_sources(&batch).is_err());
        assert!(wal.pending_path("agent-a", "om-a").is_file());
        assert!(second.is_file());
    }

    #[test]
    fn repairs_consumed_recovery_claim_and_terminalizes_batch() {
        let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
        set_replay_started(&wal, &replay_id, true);
        move_replay_to_consumed(&wal, &replay_id, false);

        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        assert!(wal.load_recovery_batch(&batch.id).unwrap().is_none());
        assert!(
            wal.root
                .join("recovery/completed")
                .join(format!("{}.json", batch.id))
                .is_file(),
            "全部 item 终态后必须先写 completed tombstone 再删除 active manifest"
        );
    }

    #[test]
    fn consumed_recovery_claim_repair_is_idempotent() {
        let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
        set_replay_started(&wal, &replay_id, true);
        move_replay_to_consumed(&wal, &replay_id, false);
        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        let completed = wal
            .root
            .join("recovery/completed")
            .join(format!("{}.json", batch.id));
        let first = std::fs::read(&completed).unwrap();

        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 0);
        assert_eq!(std::fs::read(completed).unwrap(), first);
    }

    #[test]
    fn completed_recovery_tombstone_is_strictly_validated_for_suppression_and_ack() {
        let (_tmp, wal, batch, completed_path) = setup_completed_recovery();
        let completed = read_completed_recovery_batch_checked(&completed_path).unwrap();
        assert_eq!(completed.id, batch.id);
        assert!(wal
            .recovery_managed_sources()
            .unwrap()
            .contains(&("agent-a".into(), "om-original".into())));
        assert!(wal
            .recovery_outbox_completed(&batch.id, &batch.outbox_id)
            .unwrap());
        assert!(wal
            .recovery_outbox_completed_exact(
                &batch.id,
                &batch.outbox_id,
                "scope-a",
                "agent-a",
                "oc_1",
            )
            .unwrap());
    }

    #[test]
    fn corrupted_completed_tombstone_never_suppresses_or_acks() {
        for field in [
            "schema",
            "id",
            "outbox",
            "scope",
            "agent",
            "chat",
            "duplicate_source",
            "recomputed_source",
        ] {
            let (_tmp, wal, batch, completed_path) = setup_completed_recovery();
            let mut completed = read_completed_recovery_batch_checked(&completed_path).unwrap();
            match field {
                "schema" => completed.schema_version += 1,
                "id" => completed.id = "recovery-other".into(),
                "outbox" => completed.outbox_id = "recovery-other".into(),
                "scope" => completed.scope = "scope-other".into(),
                "agent" => completed.agent = String::new(),
                "chat" => completed.chat_id = String::new(),
                "duplicate_source" => completed
                    .source_message_ids
                    .push(completed.source_message_ids[0].clone()),
                "recomputed_source" => completed.source_message_ids[0] = "om-other".into(),
                _ => unreachable!(),
            }
            atomic_write_mode(
                &completed_path,
                &serde_json::to_vec_pretty(&completed).unwrap(),
                0o600,
            )
            .unwrap();

            assert!(
                wal.recovery_managed_sources().is_err(),
                "{field} 漂移不得成为 source suppression 权威"
            );
            assert!(
                wal.recovery_outbox_completed(&batch.id, &batch.outbox_id)
                    .is_err(),
                "{field} 漂移不得本地 ACK outbox"
            );
            assert!(wal
                .recovery_outbox_completed_exact(
                    &batch.id,
                    &batch.outbox_id,
                    "scope-a",
                    "agent-a",
                    "oc_1",
                )
                .is_err());
        }
    }

    #[test]
    fn completed_tombstone_path_mismatch_fails_managed_source_scan() {
        let (_tmp, wal, batch, completed_path) = setup_completed_recovery();
        let drifted = completed_path.with_file_name("recovery-other.json");
        std::fs::rename(&completed_path, &drifted).unwrap();

        assert!(read_completed_recovery_batch_checked(&drifted).is_err());
        assert!(wal.recovery_managed_sources().is_err());
        assert!(!wal
            .recovery_outbox_completed(&batch.id, &batch.outbox_id)
            .unwrap());
    }

    #[test]
    fn completed_recovery_exact_ack_rejects_event_identity_drift() {
        for field in ["outbox", "scope", "agent", "chat"] {
            let (_tmp, wal, batch, _) = setup_completed_recovery();
            let (outbox, scope, agent, chat) = match field {
                "outbox" => ("recovery-other", "scope-a", "agent-a", "oc_1"),
                "scope" => (&*batch.outbox_id, "scope-other", "agent-a", "oc_1"),
                "agent" => (&*batch.outbox_id, "scope-a", "agent-other", "oc_1"),
                "chat" => (&*batch.outbox_id, "scope-a", "agent-a", "oc_other"),
                _ => unreachable!(),
            };
            assert!(wal
                .recovery_outbox_completed_exact(&batch.id, outbox, scope, agent, chat,)
                .is_err());
        }
    }

    #[test]
    fn active_recovery_delivery_is_an_exact_durable_local_ack_fact() {
        let (_tmp, wal, batch, _, _) = setup_half_committed_recovery_claim();
        assert!(!wal
            .recovery_outbox_delivered(&batch.id, &batch.outbox_id, "scope-a", "agent-a", "oc_1",)
            .unwrap());
        wal.record_recovery_card_delivery(&batch.id, &batch.outbox_id, "om-card")
            .unwrap();
        assert!(wal
            .recovery_outbox_delivered(&batch.id, &batch.outbox_id, "scope-a", "agent-a", "oc_1",)
            .unwrap());
        for (outbox, scope, agent, chat) in [
            ("recovery-other", "scope-a", "agent-a", "oc_1"),
            (&*batch.outbox_id, "scope-other", "agent-a", "oc_1"),
            (&*batch.outbox_id, "scope-a", "agent-other", "oc_1"),
            (&*batch.outbox_id, "scope-a", "agent-a", "oc_other"),
        ] {
            assert!(wal
                .recovery_outbox_delivered(&batch.id, outbox, scope, agent, chat)
                .is_err());
        }
    }

    #[test]
    fn cleanup_consumed_never_expires_completed_recovery_authority() {
        let (_tmp, wal, batch, completed_path) = setup_completed_recovery();

        wal.cleanup_consumed(u64::MAX);
        assert!(completed_path.is_file());
        assert!(wal
            .recovery_outbox_completed_exact(
                &batch.id,
                &batch.outbox_id,
                "scope-a",
                "agent-a",
                "oc_1",
            )
            .unwrap());
    }

    #[test]
    fn consumed_recovery_claim_rejects_every_identity_mismatch_without_progress() {
        for field in ["scope", "agent", "chat", "origin", "batch", "inbound"] {
            let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
            let consumed_path = move_replay_to_consumed(&wal, &replay_id, false);
            let mut consumed = read_record(&consumed_path).unwrap().unwrap();
            consumed.started = true;
            match field {
                "scope" => consumed.delivery_scope = "scope-other".into(),
                "agent" => consumed.agent = "agent-other".into(),
                "chat" => consumed.inbound.chat_id = "oc_other".into(),
                "origin" => consumed.recovery_origin = Some("om-other".into()),
                "batch" => consumed.recovery_batch_id = Some("recovery-other".into()),
                "inbound" => consumed.inbound.text = "载荷已变化".into(),
                _ => unreachable!(),
            }
            atomic_write_mode(
                &consumed_path,
                &serde_json::to_vec_pretty(&consumed).unwrap(),
                0o600,
            )
            .unwrap();
            let active = wal.recovery_path(&batch.id).unwrap();
            let before = std::fs::read(&active).unwrap();

            let error = wal.repair_pending_recovery_claims().unwrap_err();
            assert!(
                format!("{error:#}").contains("identity 冲突"),
                "{field} 冲突应 fail-closed: {error:#}"
            );
            assert_eq!(std::fs::read(&active).unwrap(), before);
            assert!(matches!(
                wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
                RecoveryItemState::Claimed { .. }
            ));
        }
    }

    #[test]
    fn pending_recovery_claim_is_not_mistaken_for_completed_for_scoped_or_legacy_path() {
        for legacy in [false, true] {
            let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
            if legacy {
                let scoped = wal.pending_path("agent-a", &replay_id);
                let flat = wal
                    .pending_dir()
                    .join(format!("{}.json", sanitize_id(&replay_id)));
                std::fs::rename(scoped, flat).unwrap();
            }

            assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 0);
            assert!(matches!(
                wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
                RecoveryItemState::Claimed { .. }
            ));
        }
    }

    #[test]
    fn exact_scoped_and_legacy_pending_duplicates_are_normalized_before_execution() {
        let (_tmp, wal, batch, replay_id, _) = setup_half_committed_recovery_claim();
        let scoped = wal.pending_path("agent-a", &replay_id);
        let legacy = wal
            .pending_dir()
            .join(format!("{}.json", sanitize_id(&replay_id)));
        std::fs::copy(&scoped, &legacy).unwrap();

        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        assert!(scoped.is_file());
        assert!(
            !legacy.exists(),
            "必须删掉 flat 重复项，否则 mark_started 后会分叉"
        );
        assert_eq!(
            wal.mark_started("agent-a", "scope-a", &replay_id).unwrap(),
            MarkStartedOutcome::Marked
        );
        assert_eq!(
            wal.mark_consumed_after_start("agent-a", "scope-a", &replay_id)
                .unwrap(),
            MarkConsumedOutcome::Consumed
        );
        assert!(wal.load_recovery_batch(&batch.id).unwrap().is_none());
        assert!(wal.scan_pending().is_empty());
    }

    #[test]
    fn superseded_recovery_replays_settle_both_manifest_and_queue_crash_windows() {
        for half_claimed in [false, true] {
            for consumed in [false, true] {
                let (_tmp, wal, batch, replay_id, _) = if half_claimed {
                    setup_half_committed_recovery_claim()
                } else {
                    setup_claimed_recovery()
                };
                let path = if consumed {
                    move_replay_to_consumed(&wal, &replay_id, false)
                } else {
                    wal.pending_path("agent-a", &replay_id)
                };
                let mut record = read_record(&path).unwrap().unwrap();
                record.superseded_by_new = true;
                atomic_write_mode(&path, &serde_json::to_vec_pretty(&record).unwrap(), 0o600)
                    .unwrap();
                assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
                assert!(wal.load_recovery_batch(&batch.id).unwrap().is_none());
                assert!(wal.scan_pending_strict().unwrap().is_empty());
                assert!(
                    wal.read_consumed("agent-a", &replay_id)
                        .unwrap()
                        .unwrap()
                        .superseded_by_new
                );
            }
        }
    }

    #[test]
    fn legacy_consumed_recovery_claim_is_repaired() {
        let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
        set_replay_started(&wal, &replay_id, true);
        move_replay_to_consumed(&wal, &replay_id, true);

        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        assert!(wal.load_recovery_batch(&batch.id).unwrap().is_none());
    }

    #[test]
    fn simultaneous_pending_and_consumed_recovery_replay_fails_closed() {
        let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
        let pending = wal.pending_path("agent-a", &replay_id);
        let consumed = wal.consumed_path("agent-a", &replay_id);
        std::fs::create_dir_all(consumed.parent().unwrap()).unwrap();
        std::fs::copy(&pending, &consumed).unwrap();
        let active = wal.recovery_path(&batch.id).unwrap();
        let before = std::fs::read(&active).unwrap();

        let error = wal.repair_pending_recovery_claims().unwrap_err();
        assert!(format!("{error:#}").contains("同时存在 pending/consumed"));
        assert_eq!(std::fs::read(active).unwrap(), before);
    }

    #[test]
    fn consumed_mismatch_does_not_partially_advance_multi_item_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        for id in ["om-a", "om-b"] {
            wal.append("agent-a", "scope-a", &mk_inbound(id, id))
                .unwrap();
        }
        let records = wal.scan_pending();
        let batch = wal
            .create_recovery_batch("scope-a", "agent-a", "oc_1", "p2p", None, records, 100)
            .unwrap();
        wal.finalize_recovery_sources(&batch).unwrap();
        let mut replay_ids = Vec::new();
        for item in &batch.items {
            let (replay_id, replay) =
                recovery_replay_record(&batch.id, &item.record.inbound.message_id, &item.record);
            atomic_write_mode(
                &wal.pending_path("agent-a", &replay_id),
                &serde_json::to_vec_pretty(&replay).unwrap(),
                0o600,
            )
            .unwrap();
            replay_ids.push(replay_id);
        }
        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 2);
        for replay_id in &replay_ids {
            move_replay_to_consumed(&wal, replay_id, false);
        }
        let bad_path = wal.consumed_path("agent-a", &replay_ids[1]);
        let mut bad = read_record(&bad_path).unwrap().unwrap();
        bad.recovery_batch_id = Some("recovery-other".into());
        atomic_write_mode(&bad_path, &serde_json::to_vec_pretty(&bad).unwrap(), 0o600).unwrap();
        let active = wal.recovery_path(&batch.id).unwrap();
        let before = std::fs::read(&active).unwrap();

        assert!(wal.repair_pending_recovery_claims().is_err());
        assert_eq!(std::fs::read(&active).unwrap(), before);
        assert!(wal
            .load_recovery_batch(&batch.id)
            .unwrap()
            .unwrap()
            .items
            .iter()
            .all(|item| matches!(item.state, RecoveryItemState::Claimed { .. })));
    }

    #[test]
    fn started_claim_is_reverted_to_awaiting_until_user_confirms_again() {
        let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
        set_replay_started(&wal, &replay_id, true);

        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        assert_eq!(
            wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
            RecoveryItemState::Awaiting
        );
        assert!(
            read_record(&wal.pending_path("agent-a", &replay_id))
                .unwrap()
                .unwrap()
                .started,
            "冷启只能撤回 batch 授权，必须保留 started replay 作为未知执行证据"
        );
        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 0);
        assert_eq!(
            wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
            RecoveryItemState::Awaiting,
            "Awaiting + started replay 不得在下一次冷启静默变回 Claimed"
        );
    }

    #[test]
    fn unstarted_consumed_claim_returns_to_awaiting_and_click_rebuilds_pending() {
        let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
        let consumed = move_replay_to_consumed(&wal, &replay_id, false);

        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        assert_eq!(
            wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
            RecoveryItemState::Awaiting
        );
        assert!(consumed.is_file());
        assert!(!wal.pending_path("agent-a", &replay_id).exists());
        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 0);
        wal.record_recovery_card_delivery(&batch.id, &batch.outbox_id, "om-card")
            .unwrap();

        let outcome = wal
            .claim_recovery(
                &batch.id,
                "scope-a",
                "agent-a",
                "oc_1",
                "om-card",
                "om-original",
            )
            .unwrap();
        assert!(matches!(outcome, RecoveryClaimOutcome::Claimed(_)));
        assert!(!consumed.exists());
        let pending = read_record(&wal.pending_path("agent-a", &replay_id))
            .unwrap()
            .unwrap();
        assert!(!pending.started);
    }

    #[test]
    fn recovery_card_disables_only_completed_items() {
        let (_tmp, wal, batch, _, _) = setup_claimed_recovery();
        let mut active = wal.load_recovery_batch(&batch.id).unwrap().unwrap();
        assert!(
            active.claimed_ids().is_empty(),
            "Claimed 仍须保留可点击按钮"
        );
        active.items[0].state = RecoveryItemState::Completed;
        assert_eq!(active.claimed_ids(), vec!["om-original".to_string()]);
    }

    #[test]
    fn dismiss_awaiting_removes_crash_left_replay_and_completed_manages_both_ids() {
        let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
        set_replay_started(&wal, &replay_id, true);
        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        assert_eq!(
            wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
            RecoveryItemState::Awaiting
        );
        wal.record_recovery_card_delivery(&batch.id, &batch.outbox_id, "om-card")
            .unwrap();

        assert_eq!(
            wal.dismiss_recovery_batch(&batch.id, "scope-a", "agent-a", "oc_1", "om-card")
                .unwrap(),
            1
        );
        assert!(
            !wal.pending_path("agent-a", &replay_id).exists(),
            "Awaiting 的 crash-left deterministic replay 必须随拒绝删除"
        );
        assert!(wal.load_recovery_batch(&batch.id).unwrap().is_none());
        let managed = wal.recovery_managed_sources().unwrap();
        assert!(managed.contains(&("agent-a".to_string(), "om-original".to_string())));
        assert!(managed.contains(&("agent-a".to_string(), replay_id)));
    }

    #[test]
    fn cleanup_identity_failure_keeps_active_batch_fail_closed() {
        let (_tmp, wal, batch, _replay_id, _) = setup_claimed_recovery();
        let mut conflicting = batch.items[0].record.inbound.clone();
        conflicting.text = "different payload".into();
        wal.append("agent-a", "scope-a", &conflicting).unwrap();

        assert!(wal.abandon_recovery_batch(&batch.id).is_err());
        assert!(
            wal.load_recovery_batch(&batch.id).unwrap().is_some(),
            "任一 replay/source 删除或 identity 校验失败都必须保留 active manifest"
        );
        assert!(
            !wal.completed_recovery_path(&batch.id).unwrap().exists(),
            "cleanup 未完成不得先写 completed tombstone"
        );
    }

    #[test]
    fn clicking_awaiting_started_replay_clears_started_and_requeues_same_record() {
        let (_tmp, wal, batch, replay_id, _) = setup_claimed_recovery();
        set_replay_started(&wal, &replay_id, true);
        assert_eq!(wal.repair_pending_recovery_claims().unwrap(), 1);
        wal.record_recovery_card_delivery(&batch.id, &batch.outbox_id, "om-card")
            .unwrap();

        let outcome = wal
            .claim_recovery(
                &batch.id,
                "scope-a",
                "agent-a",
                "oc_1",
                "om-card",
                "om-original",
            )
            .unwrap();
        let RecoveryClaimOutcome::Claimed(requeued) = outcome else {
            panic!("再次点击必须返回 Claimed(record) 触发显式重投");
        };
        assert_eq!(requeued.inbound.message_id, replay_id);
        assert!(!requeued.started);
        let pending = read_record(&wal.pending_path("agent-a", &replay_id))
            .unwrap()
            .unwrap();
        assert!(!pending.started);
        assert!(matches!(
            &wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
            RecoveryItemState::Claimed { replay_id: current } if current == &replay_id
        ));
    }

    #[test]
    fn clicking_awaiting_unstarted_replay_requeues_instead_of_reporting_already_claimed() {
        let (_tmp, wal, batch, replay_id, _) = setup_half_committed_recovery_claim();
        wal.record_recovery_card_delivery(&batch.id, &batch.outbox_id, "om-card")
            .unwrap();

        let outcome = wal
            .claim_recovery(
                &batch.id,
                "scope-a",
                "agent-a",
                "oc_1",
                "om-card",
                "om-original",
            )
            .unwrap();
        let RecoveryClaimOutcome::Claimed(requeued) = outcome else {
            panic!("半提交的 existing pending 也必须返回 Claimed(record) 触发重投");
        };
        assert_eq!(requeued.inbound.message_id, replay_id);
        assert!(!requeued.started);
        assert!(matches!(
            &wal.load_recovery_batch(&batch.id).unwrap().unwrap().items[0].state,
            RecoveryItemState::Claimed { replay_id: current } if current == &replay_id
        ));
    }

    #[test]
    fn append_then_scan_then_consume() {
        let tmp = std::env::temp_dir().join(format!("summon-wal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let wal = InboxWal::new(tmp.clone());

        // 空 → 扫不到。
        assert!(wal.scan_pending().is_empty());

        // 入两条 → 都在 pending。
        wal.append("starlight", "test-scope", &mk_inbound("om_a", "做个 X"))
            .unwrap();
        wal.append("starlight", "test-scope", &mk_inbound("om_b", "再做个 Y"))
            .unwrap();
        let mut got: Vec<String> = wal
            .scan_pending()
            .into_iter()
            .map(|r| r.inbound.message_id)
            .collect();
        got.sort();
        assert_eq!(got, vec!["om_a".to_string(), "om_b".to_string()]);

        // 重复入站同 message_id 幂等(不重复计数)。
        wal.append(
            "starlight",
            "test-scope",
            &mk_inbound("om_a", "做个 X(重发)"),
        )
        .unwrap();
        assert_eq!(wal.scan_pending().len(), 2);

        // 标记一条已处理 → 只剩另一条在 pending。
        wal.mark_consumed("starlight", "om_a");
        let left: Vec<String> = wal
            .scan_pending()
            .into_iter()
            .map(|r| r.inbound.message_id)
            .collect();
        assert_eq!(left, vec!["om_b".to_string()]);

        // 记录带 agent 名(重放路由用)。
        assert_eq!(wal.scan_pending()[0].agent, "starlight");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn has_pending_filters_agent_and_chat_and_fails_on_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().to_path_buf());
        assert!(!wal.has_pending("a", "oc_a").unwrap());
        let mut inbound = mk_inbound("om-a", "work");
        inbound.chat_id = "oc_a".into();
        wal.append("a", "test-scope", &inbound).unwrap();
        assert!(wal.has_pending("a", "oc_a").unwrap());
        assert!(!wal.has_pending("b", "oc_a").unwrap());
        assert!(!wal.has_pending("a", "oc_b").unwrap());
        wal.mark_consumed("a", "om-a");
        assert!(!wal.has_pending("a", "oc_a").unwrap());

        std::fs::create_dir_all(wal.pending_dir()).unwrap();
        std::fs::write(wal.pending_dir().join("broken.json"), "{").unwrap();
        assert!(wal.has_pending("a", "oc_a").is_err());
    }

    #[test]
    fn scheduled_append_is_idempotent_without_resetting_started() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        let append = || {
            wal.append_scheduled(
                "a",
                "test-scope",
                "oc_1",
                "p2p",
                "巡检",
                summon_core::ScheduleOutput::Progress,
                summon_core::ScheduleContext::Resume,
                "sched-SCH-1-0",
                1000,
            )
        };

        assert_eq!(append().unwrap(), ScheduledAppend::Inserted);
        assert_eq!(
            wal.mark_started("a", "test-scope", "sched-SCH-1-0")
                .unwrap(),
            MarkStartedOutcome::Marked
        );
        assert_eq!(append().unwrap(), ScheduledAppend::Pending);
        let pending = wal.scan_pending();
        assert_eq!(pending.len(), 1, "同一 fire 只能有一个 pending 事实");
        assert!(pending[0].started, "幂等 append 不能把 started 重置");
        assert_eq!(pending[0].scheduled.as_ref().unwrap().prompt, "巡检");

        wal.mark_consumed("a", "sched-SCH-1-0");
        assert_eq!(append().unwrap(), ScheduledAppend::Consumed);
        assert!(
            wal.scan_pending().is_empty(),
            "已消费 fire 不得重新入 pending"
        );
    }

    #[test]
    fn scheduled_append_recognizes_legacy_flat_consumed_fire() {
        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        let append = || {
            wal.append_scheduled(
                "a",
                "test-scope",
                "oc_1",
                "p2p",
                "巡检",
                summon_core::ScheduleOutput::Progress,
                summon_core::ScheduleContext::Resume,
                "sched-legacy-consumed",
                1_000,
            )
        };
        assert_eq!(append().unwrap(), ScheduledAppend::Inserted);
        let legacy = wal.consumed_dir().join("sched-legacy-consumed.json");
        std::fs::create_dir_all(wal.consumed_dir()).unwrap();
        std::fs::rename(wal.pending_path("a", "sched-legacy-consumed"), &legacy).unwrap();

        assert_eq!(append().unwrap(), ScheduledAppend::Consumed);
        assert!(legacy.exists(), "legacy 单副本可就地读取，不必冒险迁移");
        assert!(wal.scan_pending().is_empty(), "已执行 fire 不得升级后重跑");
    }

    #[test]
    fn scheduled_append_rejects_same_id_with_different_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        wal.append_scheduled(
            "a",
            "test-scope",
            "oc_1",
            "p2p",
            "巡检",
            summon_core::ScheduleOutput::Progress,
            summon_core::ScheduleContext::Resume,
            "sched-SCH-1-0",
            1000,
        )
        .unwrap();
        assert!(wal
            .append_scheduled(
                "a",
                "test-scope",
                "oc_1",
                "p2p",
                "不同任务",
                summon_core::ScheduleOutput::Progress,
                summon_core::ScheduleContext::Resume,
                "sched-SCH-1-0",
                1001,
            )
            .is_err());
    }

    // `InboundRecord` 是 debug-only 录制设施,release 下不存在。
    #[cfg(debug_assertions)]
    #[test]
    fn recovery_lists_recorded_inbound_fixture() {
        use crate::runtime::daemon::InboundRecord;
        // 用仓内**录制的真实入站**(cursor-pong/inbound.ndjson)驱动 WAL → scan,验「收到没跑完」
        // 的恢复列表 —— 复用项目已有的消息录制夹具做端到端自测。
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/scenarios/cursor-pong/inbound.ndjson");
        let body = std::fs::read_to_string(&fixture).expect("读录制入站夹具");
        let tmp = std::env::temp_dir().join(format!("summon-wal-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let wal = InboxWal::new(tmp.clone());
        let mut appended = 0;
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let rec: InboundRecord = serde_json::from_str(line).expect("解析录制入站");
            if let InboundRecord::Message { inbound, .. } = rec {
                wal.append("replayer", "test-scope", &inbound).unwrap();
                appended += 1;
            }
        }
        let pending = wal.scan_pending();
        assert_eq!(pending.len(), appended);
        // 录制里那条 ping(om_replay_1)进了恢复列表。
        assert!(pending
            .iter()
            .any(|p| p.inbound.message_id == "om_replay_1" && p.inbound.text == "ping"));
        // 模拟这一轮跑完 → 不再在 pending(不会被列入恢复)。
        wal.mark_consumed("replayer", "om_replay_1");
        assert!(wal
            .scan_pending()
            .iter()
            .all(|p| p.inbound.message_id != "om_replay_1"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn empty_message_id_not_persisted() {
        let tmp = std::env::temp_dir().join(format!("summon-wal-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let wal = InboxWal::new(tmp.clone());
        assert!(wal
            .append("a", "test-scope", &mk_inbound("", "无 id"))
            .is_err());
        assert!(wal.scan_pending().is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_falls_back_to_consumed_and_cleanup_prunes_by_age() {
        // review 修复:恢复卡片入队后条目已迁到 consumed,但「恢复」按钮仍要据 message_id 取回;
        // 且 consumed 要能按龄清理。
        let tmp = std::env::temp_dir().join(format!("summon-wal-rc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let wal = InboxWal::new(tmp.clone());
        wal.append("a", "test-scope", &mk_inbound("om_x", "hi"))
            .unwrap();
        wal.mark_consumed("a", "om_x");
        // 已不在 pending(下次崩溃扫描不再列),但 read() 回退 consumed 仍取得到。
        assert!(wal.scan_pending().is_empty());
        assert_eq!(
            wal.read_consumed("a", "om_x")
                .unwrap()
                .map(|r| r.inbound.message_id),
            Some("om_x".to_string())
        );
        // now=0 → cutoff=0,文件 mtime>0 不 < 0 → 不清。
        wal.cleanup_consumed(0);
        assert!(
            wal.read_consumed("a", "om_x").unwrap().is_some(),
            "保留期内不该清"
        );
        // now=MAX → 远超保留期 → 清掉。
        wal.cleanup_consumed(u64::MAX);
        assert!(
            wal.read_consumed("a", "om_x").unwrap().is_none(),
            "超保留期应清掉"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn recent_consumed_ids_filters_by_agent_and_orders_by_recency() {
        // R5:重启预热源——按 agent 过滤、按 create_time 新→旧排序、limit 截断保留最近。
        let tmp = std::env::temp_dir().join(format!("summon-wal-recent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let wal = InboxWal::new(tmp.clone());
        let consume = |agent: &str, id: &str, ct: i64| {
            let mut i = mk_inbound(id, "x");
            i.create_time_ms = ct;
            wal.append(agent, "test-scope", &i).unwrap();
            wal.mark_consumed(agent, id);
        };
        consume("starlight", "om_old", 100);
        consume("starlight", "om_new", 300);
        consume("starlight", "om_mid", 200);
        consume("other", "om_other", 999); // 别的 agent,不该出现
        wal.append_scheduled(
            "starlight",
            "test-scope",
            "oc_1",
            "p2p",
            "巡检",
            summon_core::ScheduleOutput::Progress,
            summon_core::ScheduleContext::Resume,
            "sched-SCH-1-0",
            1000,
        )
        .unwrap();
        wal.mark_consumed("starlight", "sched-SCH-1-0");
        let ids = wal.recent_consumed_ids("starlight", 10);
        assert_eq!(
            ids,
            vec![
                "om_new".to_string(),
                "om_mid".to_string(),
                "om_old".to_string()
            ],
            "按 create_time 降序、过滤 agent"
        );
        assert!(!ids.contains(&"om_other".to_string()), "应过滤别的 agent");
        assert!(
            !ids.contains(&"sched-SCH-1-0".to_string()),
            "scheduled 不应挤占真人入站去重容量"
        );
        // limit 截断保留最近。
        assert_eq!(
            wal.recent_consumed_ids("starlight", 1),
            vec!["om_new".to_string()]
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn recall_mark_check_and_clear() {
        // mark_recalled 写 tombstone → is_recalled 命中 → clear_recalled 清掉。
        let tmp = std::env::temp_dir().join(format!("summon-wal-recall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let wal = InboxWal::new(tmp.clone());

        // 初始:未撤回。
        assert!(!wal.is_recalled("a", "om_x"));

        // 标记撤回 → 应命中。
        wal.mark_recalled("a", "om_x");
        assert!(wal.is_recalled("a", "om_x"), "mark 后应检测为已撤回");

        // 清除 → 不再命中。
        wal.clear_recalled("a", "om_x");
        assert!(!wal.is_recalled("a", "om_x"), "clear 后不应再命中");

        // 空/异常 id 不崩(sanitize 兜底)。
        wal.mark_recalled("a", "");
        wal.mark_recalled("a", "../../etc/passwd");
        assert!(
            wal.is_recalled("a", "../../etc/passwd"),
            "路径型 id 经 sanitize 后仍可工作"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn orphan_recall_tombstones_are_pruned_by_age() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        // 原消息从未到达/消费，只有 tombstone；过去这类记录没有任何清理路径。
        wal.mark_recalled("a", "om_never_arrived");
        assert!(wal.is_recalled("a", "om_never_arrived"));

        wal.cleanup_recalled(0);
        assert!(wal.is_recalled("a", "om_never_arrived"), "保留期内不应清理");
        wal.cleanup_recalled(u64::MAX);
        assert!(
            !wal.is_recalled("a", "om_never_arrived"),
            "超过保留期的孤儿 tombstone 应清理"
        );
    }

    #[test]
    fn a_superseded_queue_entry_is_terminal_across_the_pending_to_consumed_crash_window() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        wal.append("a", "test-scope", &mk_inbound("om_old", "旧会话内容"))
            .unwrap();
        let path = wal.pending_path("a", "om_old");
        let mut record = read_record(&path).unwrap().unwrap();
        record.superseded_by_new = true;
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&record).unwrap(), 0o600).unwrap();
        let restarted = InboxWal::new(tmp.path().join("inbox"));
        restarted
            .append("a", "test-scope", &mk_inbound("om_old", "网络重投"))
            .unwrap();
        assert_eq!(
            restarted.mark_started("a", "test-scope", "om_old").unwrap(),
            MarkStartedOutcome::AlreadyConsumed
        );
        let consumed = restarted.read_consumed("a", "om_old").unwrap().unwrap();
        assert!(consumed.superseded_by_new);
        assert!(!consumed.started);
        assert_eq!(consumed.inbound.text, "旧会话内容");
        assert!(restarted.scan_pending().is_empty());
    }

    #[test]
    fn superseding_a_queued_message_validates_its_delivery_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(tmp.path().join("inbox"));
        wal.append("a", "test-scope", &mk_inbound("om_old", "旧会话内容"))
            .unwrap();
        assert!(wal
            .discard_superseded("a", "other-scope", "om_old")
            .is_err());
        assert_eq!(wal.scan_pending().len(), 1);
        assert!(!wal.scan_pending()[0].superseded_by_new);
    }

    #[test]
    fn mark_started_flips_pending_record_and_defaults_false() {
        // C8:消息粒度「已开跑」标记。新入 pending 记录默认未开跑;chat 线程进 handle_task 前
        // mark_started 置 true;优雅重启据此区分「排队未跑」(重注入不丢)与「已开跑」(不自动重放)。
        let tmp = std::env::temp_dir().join(format!("summon-wal-started-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let wal = InboxWal::new(tmp.clone());

        wal.append("a", "test-scope", &mk_inbound("om_s", "活儿"))
            .unwrap();
        assert!(
            !wal.scan_pending()[0].started,
            "新入 pending 记录默认未开跑(serde default)"
        );

        assert_eq!(
            wal.mark_started("a", "test-scope", "om_s").unwrap(),
            MarkStartedOutcome::Marked
        );
        assert!(wal.scan_pending()[0].started, "mark_started 后应置已开跑");
        // 记录本体不被破坏(agent / 正文仍在)。
        let rec = &wal.scan_pending()[0];
        assert_eq!(rec.agent, "a");
        assert_eq!(rec.inbound.text, "活儿");

        // 幂等:再标一次仍 true、不报错。
        assert_eq!(
            wal.mark_started("a", "test-scope", "om_s").unwrap(),
            MarkStartedOutcome::AlreadyStarted
        );
        assert!(wal.scan_pending()[0].started);

        // 不在 pending 的 id:no-op 不 panic。
        assert_eq!(
            wal.mark_started("a", "test-scope", "om_absent").unwrap(),
            MarkStartedOutcome::Missing
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn normal_scoped_and_legacy_duplicates_are_canonicalized_before_start() {
        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        wal.append("a", "scope-a", &mk_inbound("om-dual", "一次"))
            .unwrap();
        let scoped = wal.pending_path("a", "om-dual");
        let legacy = wal.pending_dir().join("om-dual.json");
        std::fs::copy(&scoped, &legacy).unwrap();

        assert_eq!(
            wal.mark_started("a", "scope-a", "om-dual").unwrap(),
            MarkStartedOutcome::Marked
        );
        assert!(!legacy.exists(), "started 推进前必须清掉等价 flat 副本");
        assert!(read_record(&scoped).unwrap().unwrap().started);
        assert_eq!(
            wal.mark_consumed_after_start("a", "scope-a", "om-dual")
                .unwrap(),
            MarkConsumedOutcome::Consumed
        );
        assert!(wal.scan_pending().is_empty(), "flat 不能在销账后复活");
    }

    #[test]
    fn conflicting_scoped_and_legacy_pending_blocks_start() {
        let temp = tempfile::tempdir().unwrap();
        let wal = InboxWal::new(temp.path().join("inbox"));
        wal.append("a", "scope-a", &mk_inbound("om-conflict", "scoped"))
            .unwrap();
        let mut legacy_record = read_record(&wal.pending_path("a", "om-conflict"))
            .unwrap()
            .unwrap();
        legacy_record.inbound.text = "legacy-drift".into();
        atomic_write_mode(
            &wal.pending_dir().join("om-conflict.json"),
            &serde_json::to_vec_pretty(&legacy_record).unwrap(),
            0o600,
        )
        .unwrap();

        assert!(wal.mark_started("a", "scope-a", "om-conflict").is_err());
        assert!(
            !read_record(&wal.pending_path("a", "om-conflict"))
                .unwrap()
                .unwrap()
                .started,
            "冲突时不得推进任一副本"
        );
    }

    #[test]
    fn recall_before_pending_still_skips() {
        // 场景 B:撤回比消息先到 → tombstone 已存在,消息到了仍应被跳过。
        let tmp = std::env::temp_dir().join(format!("summon-wal-recall-b4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let wal = InboxWal::new(tmp.clone());

        // 先撤回(消息还没到)。
        wal.mark_recalled("a", "om_early");
        assert!(wal.is_recalled("a", "om_early"));

        // 消息到达 → 进 pending。
        wal.append("a", "test-scope", &mk_inbound("om_early", "do x"))
            .unwrap();
        assert_eq!(
            wal.scan_pending().len(),
            1,
            "消息仍进 pending(撤回不挡入队)"
        );

        // 但消费前检查 → 已撤回,应跳过。
        assert!(
            wal.is_recalled("a", "om_early"),
            "消费前检查应检测到撤回(tombstone 仍在)"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
