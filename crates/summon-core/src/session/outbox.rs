//! 事件 outbox 的纯 schema + 抗雪崩纯决策(Transactional Outbox / at-least-once / 死信)。
//!
//! 「需要联网但马上要被打断」的那一刻(停机/kill/离线拆除),producer 只往本地 outbox 写一条
//! 事件(快、不联网、不阻塞);daemon 下次启动由独立 drain 线程消费——投递→ACK(归档)→失败退避
//! 重试→封顶进死信。**本模块只放纯逻辑**(serde 形状 + 退避/到期/封顶判定),所有 IO(原子写 /
//! 目录扫描 / rename 状态迁移 / 真发飞书)在 `summon` 侧,镜像 `pending.rs` 的切分纪律。
//!
//! 状态用**目录**表达(pending/archive/dead),不进字段,故记录本身不含 state 字段。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::bridge::SessionKey;
use crate::session::exit::ExitReason;
use crate::session::pending::PendingTurn;

/// 抗雪崩数值(纯常量,IO 侧复用)。第 `MAX_ATTEMPTS` 次仍失败 → 死信。
/// 150 次配合 10 分钟退避上限，覆盖约 24 小时网络故障；不能在约 3 分钟后就把
/// “网络恢复后补发”的消息永久转死信。
pub const MAX_ATTEMPTS: u32 = 150;
/// 退避基数(ms):5s。
pub const BACKOFF_BASE_MS: u64 = 5_000;
/// 退避封顶(ms):10min。
pub const BACKOFF_CAP_MS: u64 = 10 * 60 * 1_000;
/// 抖动比例:±20%。
pub const BACKOFF_JITTER: f64 = 0.20;
/// 每 tick 最多投递条数(防 thundering herd)。
pub const MAX_PER_TICK: usize = 8;
/// 同 tick 并发投递上限。
pub const MAX_CONCURRENCY: usize = 2;
/// drain loop 空转间隔(ms)。
pub const TICK_MS: u64 = 2_000;
/// archive/ 清理阈值(ms):7 天。
pub const ARCHIVE_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
/// 冻结飞书卡片的持久化 wire schema；模板改版时新增兼容版本，不能重渲染旧 outbox。
pub const BRIDGE_CARD_WIRE_VERSION: u32 = 1;
pub const MAX_FROZEN_BRIDGE_CARD_BYTES: usize = 30 * 1024;

/// 一条 outbox 事件。落 `~/.summon/run/outbox/{pending,archive,dead}/<id>.json`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxEvent {
    /// 唯一 id,内编码 agent + ts 前缀,文件名即 `<id>.json`。
    /// 形如 `<unix_ms>-<agent>-<8hex 随机>`,既排序友好(ts 前缀)又含归属(agent)。
    pub id: String,
    /// 哪个 agent(消费时 re-resolve Agent / FeishuClient / messaging / owner)。
    pub agent: String,
    /// 事件创建时冻结的飞书 app storage scope。凭据/app 变化后的新 daemon 绝不能把旧事件
    /// 发到新 app。旧 schema 缺失时为空，消费者必须 fail-closed 或做显式迁移。
    #[serde(default)]
    pub delivery_scope: String,
    /// 入队时刻(unix ms),观测 + 排序用。
    pub created_ms: u64,
    /// 已尝试投递次数(初始 0)。抗雪崩计数,达 max → 死信。
    pub attempts: u32,
    /// 下次允许投递的最早时刻(unix ms)。退避用;首次入队 = created_ms(立即可投)。
    pub next_attempt_ms: u64,
    /// 最近一次失败原因(人读,死信排查用;成功归档时无所谓)。
    pub last_error: Option<String>,
    /// 动作载荷(可扩展枚举)。
    pub kind: OutboxKind,
}

/// Bridge 网络投递前必须逐字段重验的持久化围栏。
///
/// 这里刻意保留 `agent` 与 [`OutboxEvent::agent`] 的冗余：前者属于创建 receipt 时的安全
/// 快照，后者属于 outbox 路由。消费者必须要求两者相等，不能让文件所在 agent 替代 binding
/// 的原始 scope。`lease_binding_id` 对应当前 [`crate::bridge::SessionLease`]；lease 的 target
/// 则由同一围栏里的 scope/agent/chat 精确复验。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeDeliveryFence {
    pub scope: String,
    pub agent: String,
    pub chat_id: String,
    pub chat_type: String,
    pub binding_id: String,
    pub binding_revision: u64,
    pub session: SessionKey,
    pub generation: u64,
    pub lease_binding_id: String,
    pub receipt_id: String,
    /// 同一 binding 内的持久化单调序列；outbox 用它选唯一队首。
    pub sequence: u64,
}

/// 确定性 outbox id / 飞书官方 uuid 的用途域。用途前缀必须参与 id，避免同一 receipt 在
/// audit、turn、context 三类消息之间相互去重。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeDeliveryKind {
    ActivationAudit,
    ExternalTurn,
    ContextCapsule,
    HandoffStatus,
    /// recall 全托管的终态回执(Bound/Failed)。与 `HandoffStatus` 分域:两者是**并列**的
    /// 状态机,attempt id 空间不保证互斥,共用前缀会让两条流水互相去重掉。
    RecallResumeReceipt,
}

impl BridgeDeliveryKind {
    fn id_prefix(self) -> &'static str {
        match self {
            Self::ActivationAudit => "bridge-audit-",
            Self::ExternalTurn => "bridge-turn-",
            Self::ContextCapsule => "bridge-ctx-",
            Self::HandoffStatus => "bridge-handoff-",
            Self::RecallResumeReceipt => "bridge-recall-",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeHandoffNotice {
    Requested,
    TerminatingSource,
    Resuming,
    Completed,
    Failed,
}

/// 由 durable receipt 生成确定性 outbox id，同时可直接作为飞书 `uuid`。
///
/// 正常 UUID receipt 原样保留，便于排障；异常长或含文件名危险字符的 opaque id 用固定算法
/// 压成 128-bit 十六进制。该 digest 只用于幂等键，不参与授权或完整性判断；投递仍必须精确复验
/// [`BridgeDeliveryFence::receipt_id`]。所有结果均不超过飞书 `uuid` 的 50 字节限制。
pub fn bridge_delivery_id(kind: BridgeDeliveryKind, receipt_id: &str) -> String {
    let prefix = kind.id_prefix();
    let max_verbatim = 50usize.saturating_sub(prefix.len());
    let safe_verbatim = !receipt_id.is_empty()
        && receipt_id.len() <= max_verbatim
        && receipt_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if safe_verbatim {
        return format!("{prefix}{receipt_id}");
    }

    let left = stable_fnv1a(receipt_id.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let right = stable_fnv1a(receipt_id.as_bytes(), 0x8422_2325_cbf2_9ce4);
    format!("{prefix}{left:016x}{right:016x}")
}

/// RecoveryCard 同时把这个确定性 id 用作本地 outbox key 与飞书 uuid。输入必须包含冻结的
/// app scope、agent、chat 和有序 message-id 集合；128-bit 摘要只承担幂等，不承担授权。
pub fn recovery_delivery_id(identity: &str) -> String {
    stable_delivery_id("recovery-card-", identity.as_bytes())
}

/// 用户点击恢复后生成与原飞书 event id 分离的确定性 replay id。原 id 会被 InboundGate 预热
/// 封住网络重投；replay id 则在 crash-before-start 时可反复自动注入而不重复造任务。
pub fn recovery_replay_id(batch_id: &str, original_message_id: &str) -> String {
    let identity = format!("{batch_id}\0{original_message_id}");
    stable_delivery_id("recovery-run-", identity.as_bytes())
}

pub fn agent_disabled_delivery_id(agent: &str, scope: &str, message_id: &str) -> String {
    stable_delivery_id(
        "agent-disabled-",
        format!("{agent}\0{scope}\0{message_id}").as_bytes(),
    )
}

fn stable_delivery_id(prefix: &str, bytes: &[u8]) -> String {
    let left = stable_fnv1a(bytes, 0xcbf2_9ce4_8422_2325);
    let right = stable_fnv1a(bytes, 0x8422_2325_cbf2_9ce4);
    format!("{prefix}{left:016x}{right:016x}")
}

fn stable_fnv1a(bytes: &[u8], seed: u64) -> u64 {
    bytes.iter().fold(seed, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// outbox 动作种类(可扩展)。serde 内部 tag,便于加新种类不破旧文件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutboxKind {
    AgentDisabled {
        chat_id: String,
        chat_type: String,
        rendered_card: String,
    },
    /// 发一条「会话被打断 / 被 kill」通知(复用 notify_session_exited 的语义)。
    /// 本期**唯一被产**的 kind。
    NotifyChat {
        chat_id: String,
        chat_type: String,
        /// .meta 记的群主近似(p2p 回退链首选);可空。
        creator: Option<String>,
        /// 事件带的操作者(仅群解散/bot移除/面板点击路径有);可空。
        event_operator: Option<String>,
        /// 退出原因:既选文案模板又决定「先发原 chat vs 直接 p2p」。
        reason: ExitReason,
        /// 入队时刻**快照**的文案(跨重启模板可能变;「被打断」是入队那刻的事实)。
        text: String,
        /// 入队时刻**定格**的卡片 wire。跨重启模板会变,而回执描述的是入队那刻的事实,
        /// 必须冻结(与 RecoveryCard 同理)。`#[serde(default)]`:daemon 升级前入队的旧事件
        /// 无此字段 → 空串,投递侧回退 `text` 文本路径,不丢消息。
        #[serde(default)]
        rendered_card: String,
    },
    /// 续跑一轮被中断的无头 turn。**本期为 schema-only stub:定义但绝不构造**——续跑仍由
    /// `pending.json` + `resume_pending_turn` 负责(见设计 §6 决策 B,零双跑风险)。
    ResumeTurn {
        chat_id: String,
        chat_type: String,
        /// 复用 `pending.rs` 的 `PendingTurn` 整体。
        turn: PendingTurn,
    },
    /// 崩溃恢复卡片:daemon 意外被杀(SIGKILL)重启后,把某 chat 里「收到但没跑完」的消息(入站
    /// WAL 的 pending)列成一张卡片,按钮让用户挑哪些重跑(#3:**不自动重跑**)。卡片 wire 在
    /// 入队时冻结；`items` 只保留 callback/迁移所需的权威身份，投递时绝不能重新渲染。
    RecoveryCard {
        /// opaque durable recovery manifest id；同时是确定性 outbox id / 飞书 uuid。
        #[serde(default)]
        batch_id: String,
        /// 冻结的飞书 app storage scope，callback 与发送前都必须精确复验。
        #[serde(default)]
        scope: String,
        chat_id: String,
        chat_type: String,
        items: Vec<RecoveryItem>,
        /// #3:崩溃时该 chat 在跑那轮已累积的「部分回答」预览(pending.json 的 partial),给用户看
        /// agent「进行到什么阶段」。无在跑 partial → None。`#[serde(default)]`:旧入队事件无此字段。
        #[serde(default)]
        stage: Option<String>,
        /// 入队时冻结的飞书 wire card。旧事件缺字段时反序列化为 `0`/空串，消费者必须走
        /// 显式迁移，不能拿当前模板就地补画后沿用旧 UUID。
        #[serde(default)]
        card_version: u32,
        #[serde(default)]
        rendered_card: String,
    },
    /// 独立过程补充的冻结卡片；首次投递与重试共用事件 id 作为 uuid。
    ProcessSupplement {
        chat_id: String,
        chat_type: String,
        thread_anchor: Option<String>,
        rendered_card: String,
    },
    /// 「把一段 markdown 无条件投到某 chat」的通用原语。两个生产者:
    /// ① 一轮 agent 回复的补发(网络全断时 deliver_final_reply 发不出去 → 入队重试);
    /// ② recall 全托管的终态回执(见 `bridge/recall_resume.rs`)。
    /// 投递时作为新 markdown 消息发到原 chat(不做流式对账,网络恢复后能看到就行)。
    /// **不为 ② 另开变体**:载荷会与本变体逐字段同构,复制一份只增熵不增信息。
    TurnReply {
        chat_id: String,
        chat_type: String,
        /// 回复正文(markdown)。入队时快照。
        text: String,
        /// 可选:触发这轮回复的入站消息 message_id。
        #[serde(default)]
        parent_message_id: Option<String>,
    },
    /// Mirror 进入 Activating 后的群审计。事件不携带任何 external transcript 正文；审计成功
    /// 写 receipt 后，bridge coordinator 才有权 CAS 到 MirrorBound。
    BridgeActivationAudit {
        fence: BridgeDeliveryFence,
        ticket_id: String,
        choice_id: String,
        requester_open_id: String,
        /// 入队时冻结，确保同一 uuid 跨升级重试的群审计请求体不漂移。
        card_version: u32,
        rendered_card: String,
        /// 激活成功后的私聊控制卡也随同一 durable 事件冻结；PATCH 失败后的幂等 fallback
        /// 必须复用这份 wire，不能在 binary 升级后用同一 uuid 发送不同请求体。
        control_card: String,
    },
    /// 一轮 external Claude 镜像。user/assistant 同属一个 outbox 事件且渲染为一张卡，避免
    /// 两次网络调用在崩溃窗口里出现角色倒序或半轮。
    ExternalBridgeTurn {
        fence: BridgeDeliveryFence,
        turn_id: String,
        /// 入队时冻结的飞书 wire card；同一 uuid 跨 binary 升级重试时请求体保持不变。
        card_version: u32,
        rendered_card: String,
    },
    /// 授权用户显式请求的上下文胶囊。只保存 transcript parser 已收缩的可见消息；thinking、
    /// tool、system 与 sidechain 不得从其他字段旁路进入 outbox。
    BridgeContextCapsule {
        fence: BridgeDeliveryFence,
        ticket_id: String,
        choice_id: String,
        requester_open_id: String,
        /// 入队时冻结的飞书 wire card；terminal archive/dead 会整体清空。
        card_version: u32,
        rendered_card: String,
    },
    /// Session Takeover 的群内生命周期消息。状态机先持久化 phase，再以同一 binding 的固定
    /// sequence 入队；飞书 uuid 使用确定性 id，重试不会制造重复消息。
    BridgeHandoffStatus {
        fence: BridgeDeliveryFence,
        attempt_id: String,
        notice: BridgeHandoffNotice,
        card_version: u32,
        rendered_card: String,
    },
}

/// 只验证冻结 wire 的版本、边界和基本卡片形状；绝不能拿当前 renderer 重新渲染后逐字比较，
/// 否则升级会让已经 durable 的旧 UUID 永久无法恢复。
pub fn valid_frozen_bridge_card(version: u32, rendered_card: &str) -> bool {
    if version == 0
        || version > BRIDGE_CARD_WIRE_VERSION
        || rendered_card.is_empty()
        || rendered_card.len() > MAX_FROZEN_BRIDGE_CARD_BYTES
    {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(rendered_card)
        .ok()
        .is_some_and(|card| card.get("schema").and_then(|value| value.as_str()) == Some("2.0"))
}

impl OutboxKind {
    /// Bridge 事件统一暴露围栏，供 planner 串行化、IO 层 delivery 做 exact gate。
    pub fn bridge_fence(&self) -> Option<&BridgeDeliveryFence> {
        match self {
            Self::BridgeActivationAudit { fence, .. }
            | Self::ExternalBridgeTurn { fence, .. }
            | Self::BridgeContextCapsule { fence, .. }
            | Self::BridgeHandoffStatus { fence, .. } => Some(fence),
            Self::AgentDisabled { .. }
            | Self::NotifyChat { .. }
            | Self::ResumeTurn { .. }
            | Self::RecoveryCard { .. }
            | Self::TurnReply { .. }
            | Self::ProcessSupplement { .. } => None,
        }
    }
}

/// 崩溃恢复卡片里的一条:被中断消息的 `message_id`(重跑按钮回传它,据此从入站 WAL 取回原 Inbound
/// 重新注入)+ 预览文本(列表里给用户看是哪条)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryItem {
    pub message_id: String,
    pub preview: String,
}

/// 终态回复是否需要进入 TurnReply outbox。只要本轮终态正文没有完整送达，
/// 就补发权威全文；部分分段已送达时仍以 at-least-once 优先保证不丢尾段，可能重复
/// 已送达前缀。空白回复不入队。
pub fn should_enqueue_incomplete_turn_reply(delivery_complete: bool, body: &str) -> bool {
    !delivery_complete && !body.trim().is_empty()
}

/// 拼接 outbox 事件 id:`<unix_ms>-<agent>-<8hex>`。
///
/// 纯函数:时间(`now_ms`)与随机(`hex8`,IO 侧给一个 8 位十六进制串)由调用方注入,保证
/// summon-core 仍是确定性的。agent 名里的 `-` 不转义——解析靠 ts 前缀 + 末尾固定 8hex,不靠分隔。
pub fn format_id(now_ms: u64, agent: &str, hex8: &str) -> String {
    format!("{now_ms}-{agent}-{hex8}")
}

/// 构造一条 `NotifyChat` outbox 事件。纯组装,无 IO、无随机源(`id_suffix` 由调用方给)。
/// **CLI 与 daemon 共用**:两端入队的事件结构必须一致,分头组装迟早漂移。
///
/// 首次可立即投(`next_attempt_ms=created_ms`);`text`/`rendered_card` 均为**入队那刻**的快照
/// (跨重启文案/模板都可能变,回执描述的是入队那刻的事实)。
#[allow(clippy::too_many_arguments)] // NotifyChat 载荷字段多且各自独立,合并无意义
pub fn build_notify_event(
    now_ms: u64,
    agent_name: &str,
    delivery_scope: &str,
    chat_id: &str,
    chat_type: &str,
    creator: Option<&str>,
    event_operator: Option<&str>,
    reason: ExitReason,
    text: String,
    rendered_card: String,
    id_suffix: &str,
) -> OutboxEvent {
    OutboxEvent {
        id: format_id(now_ms, agent_name, id_suffix),
        agent: agent_name.to_string(),
        delivery_scope: delivery_scope.to_string(),
        created_ms: now_ms,
        attempts: 0,
        next_attempt_ms: now_ms,
        last_error: None,
        kind: OutboxKind::NotifyChat {
            chat_id: chat_id.to_string(),
            chat_type: chat_type.to_string(),
            creator: creator.map(str::to_string),
            event_operator: event_operator.map(str::to_string),
            reason,
            text,
            rendered_card,
        },
    }
}

/// 退避:`base * 3^(attempts-1)`,封顶 `BACKOFF_CAP_MS`,**不含抖动**(抖动在 IO 侧注入,
/// 保持纯函数确定性可测)。`attempts` 是「即将进行的这次重试是第几次失败后的等待」,即已失败
/// 次数(>=1);传 0 当作 1 处理(首次失败也至少等一个 base)。
///
/// 序列:5s,15s,45s,135s,405s→cap 600s,…
pub fn next_backoff_ms(attempts: u32) -> u64 {
    let n = attempts.max(1);
    // 3^(n-1) 用饱和乘法避免溢出;一旦越过 cap 直接钳到 cap。
    let mut backoff = BACKOFF_BASE_MS;
    for _ in 1..n {
        backoff = backoff.saturating_mul(3);
        if backoff >= BACKOFF_CAP_MS {
            return BACKOFF_CAP_MS;
        }
    }
    backoff.min(BACKOFF_CAP_MS)
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
/// 给退避值加 ±`BACKOFF_JITTER` 抖动。`rand01` 是 IO 侧注入的 [0,1) 随机数(便于测试给定值断言区间)。
/// 纯函数:`jitter = base * (1 + (rand01*2-1)*JITTER)`。
pub(crate) fn jittered_backoff_ms(base_ms: u64, rand01: f64) -> u64 {
    let r = rand01.clamp(0.0, 1.0);
    let factor = 1.0 + (r * 2.0 - 1.0) * BACKOFF_JITTER;
    (base_ms as f64 * factor).round() as u64
}

/// 事件是否到期可投:`next_attempt_ms <= now_ms`。
pub fn due(ev: &OutboxEvent, now_ms: u64) -> bool {
    ev.next_attempt_ms <= now_ms
}

/// drain 一 tick 的**纯计划**:从 `events`(pending/ 全量快照)里挑出到期(`due`)的、按
/// `created_ms` 升序(FIFO 公平:老事件先投,避免新事件持续插队饿死老事件)、截断到 `MAX_PER_TICK`。
/// Bridge 事件额外按 `(scope, agent, binding_id)` 严格串行：planner 先从**全部 pending**（包括
/// 尚未到重试时刻的事件）按 `sequence` 选唯一队首，只有队首到期才可进入计划。因而老事件处于
/// backoff 时，新 turn/context 也不能越过它；不同 binding 仍可并行。
///
/// 消费者(IO 侧 drain 线程)据返回顺序逐条投递。**纯函数**:不碰文件/时钟,now 由调用方注入,
/// 便于单测限流与到期过滤;返回 clone 出的事件(IO 侧据 id 做后续 ack/record_failure/to_dead)。
pub fn drain_plan(events: &[OutboxEvent], now_ms: u64) -> Vec<OutboxEvent> {
    let mut bridge_heads: BTreeMap<(&str, &str, &str), &OutboxEvent> = BTreeMap::new();
    for event in events {
        let Some(fence) = event.kind.bridge_fence() else {
            continue;
        };
        let key = (
            fence.scope.as_str(),
            fence.agent.as_str(),
            fence.binding_id.as_str(),
        );
        bridge_heads
            .entry(key)
            .and_modify(|head| {
                let head_fence = head
                    .kind
                    .bridge_fence()
                    .expect(summon_i18n::text!("rabded3676a"));
                if (fence.sequence, event.created_ms, event.id.as_str())
                    < (head_fence.sequence, head.created_ms, head.id.as_str())
                {
                    *head = event;
                }
            })
            .or_insert(event);
    }

    let mut due_evs: Vec<OutboxEvent> = events
        .iter()
        .filter(|event| {
            if !due(event, now_ms) {
                return false;
            }
            let Some(fence) = event.kind.bridge_fence() else {
                return true;
            };
            bridge_heads
                .get(&(
                    fence.scope.as_str(),
                    fence.agent.as_str(),
                    fence.binding_id.as_str(),
                ))
                .is_some_and(|head| head.id == event.id)
        })
        .cloned()
        .collect();
    // 升序按入队时刻:老事件优先,且同一批被推迟的(超 cap)下 tick 仍按序补投。
    due_evs.sort_by(|left, right| {
        (left.created_ms, left.id.as_str()).cmp(&(right.created_ms, right.id.as_str()))
    });
    due_evs.truncate(MAX_PER_TICK);
    due_evs
}

/// 一次投递失败后的处置:继续退避重试,还是封顶进死信。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// 仍可重试(已更新 `attempts` / `next_attempt_ms`,IO 侧覆写回 pending/)。
    Retry,
    /// 达到 max,移 dead/ 停止重试。
    Dead,
}

/// 处理一次投递失败(原地更新事件):`attempts += 1`,记 `last_error`;
/// 若 `attempts >= max` → `Dead`;否则按 `next_backoff_ms(attempts)` 排下次重试 → `Retry`。
///
/// 纯决策但需写回事件字段(IO 侧据 `attempts`/`next_attempt_ms` 落盘)。下次到期时刻
/// = `now_ms + next_backoff_ms(attempts)`,**不含抖动**(抖动若需要由 IO 侧另叠);测试可确定断言。
pub fn mark_attempt_failed(ev: &mut OutboxEvent, now_ms: u64, max: u32, err: &str) -> Disposition {
    ev.attempts = ev.attempts.saturating_add(1);
    ev.last_error = Some(err.to_string());
    if ev.attempts >= max {
        Disposition::Dead
    } else {
        ev.next_attempt_ms = now_ms.saturating_add(next_backoff_ms(ev.attempts));
        Disposition::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify_event() -> OutboxEvent {
        OutboxEvent {
            id: format_id(1_700_000_000_000, "starlight", "deadbeef"),
            agent: "starlight".into(),
            delivery_scope: "test-scope".into(),
            created_ms: 1_700_000_000_000,
            attempts: 0,
            next_attempt_ms: 1_700_000_000_000,
            last_error: None,
            kind: OutboxKind::NotifyChat {
                chat_id: "oc_123".into(),
                chat_type: "group".into(),
                creator: Some("ou_creator".into()),
                event_operator: None,
                reason: ExitReason::ShutdownInterrupt,
                text: "⏸️ 上一轮被打断,重启后会自动续上".into(),
                rendered_card: String::new(),
            },
        }
    }

    #[test]
    fn format_id_is_ts_agent_hex() {
        assert_eq!(
            format_id(1_700_000_000_000, "starlight", "deadbeef"),
            "1700000000000-starlight-deadbeef"
        );
    }

    #[test]
    fn turn_reply_enqueue_decision_covers_delivery_states() {
        assert!(should_enqueue_incomplete_turn_reply(false, "回复"));
        assert!(!should_enqueue_incomplete_turn_reply(true, "回复"));
        assert!(!should_enqueue_incomplete_turn_reply(false, "  \n "));
    }

    #[test]
    fn notify_chat_json_round_trip() {
        let ev = notify_event();
        let s = serde_json::to_string(&ev).unwrap();
        // 内部 tag:载荷里出现 "kind":"notify_chat"。
        assert!(s.contains("\"kind\":\"notify_chat\""));
        // reason snake_case 持久化。
        assert!(s.contains("\"shutdown_interrupt\""));
        let back: OutboxEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn notify_chat_without_rendered_card_deserializes_to_empty() {
        // 向后兼容:daemon 升级前入队的 pending 事件没有 rendered_card 字段。必须能反序列化
        // 且 card 为空,由投递侧回退文本路径——升级不能丢已入队的消息。
        // 注:OutboxKind 是内部 tag(`#[serde(tag = "kind")]`)+ ExitReason snake_case,不是外部 tag。
        let legacy = r#"{
            "kind": "notify_chat",
            "chat_id": "oc_1", "chat_type": "group",
            "creator": null, "event_operator": null,
            "reason": "manual_kill", "text": "本会话已被关闭。"
        }"#;
        let kind: OutboxKind = serde_json::from_str(legacy).expect("旧 schema 必须仍可反序列化");
        match kind {
            OutboxKind::NotifyChat {
                rendered_card,
                text,
                ..
            } => {
                assert_eq!(rendered_card, "", "旧事件无该字段应降级为空");
                assert_eq!(text, "本会话已被关闭。");
            }
            _ => panic!("应解析为 NotifyChat"),
        }
    }

    #[test]
    fn build_notify_event_freezes_card_and_is_immediately_due() {
        // 首次可立即投:next_attempt_ms == created_ms;卡片 wire 定格在入队那刻。
        let ev = build_notify_event(
            1_700_000_000_000,
            "arcadia",
            "arcadia-abc123",
            "oc_1",
            "group",
            None,
            None,
            ExitReason::ManualKill,
            "文案".into(),
            r#"{"schema":"2.0"}"#.into(),
            "deadbeef",
        );
        assert_eq!(ev.created_ms, ev.next_attempt_ms, "首次应可立即投");
        assert_eq!(ev.attempts, 0);
        assert_eq!(ev.delivery_scope, "arcadia-abc123");
        match ev.kind {
            OutboxKind::NotifyChat { rendered_card, .. } => {
                assert_eq!(rendered_card, r#"{"schema":"2.0"}"#, "卡片必须定格");
            }
            _ => panic!("应为 NotifyChat"),
        }
    }

    #[test]
    fn resume_turn_json_round_trip() {
        // ResumeTurn 本期不产,但 schema 必须完整往返(向前兼容)。
        let ev = OutboxEvent {
            id: format_id(1_700_000_000_001, "nimbus", "0badf00d"),
            agent: "nimbus".into(),
            delivery_scope: "test-scope".into(),
            created_ms: 1_700_000_000_001,
            attempts: 1,
            next_attempt_ms: 1_700_000_005_000,
            last_error: Some("timeout".into()),
            kind: OutboxKind::ResumeTurn {
                chat_id: "oc_9".into(),
                chat_type: "p2p".into(),
                turn: PendingTurn {
                    kind: crate::pending::PendingTurnKind::Chat,
                    delivery_scope: "test-scope".into(),
                    agent_name: "tester".into(),
                    inbox_message_ids: vec!["om_9".into()],
                    resume_authorized: true,
                    chat_id: "oc_9".into(),
                    chat_type: "p2p".into(),
                    cli: "codex".into(),
                    prompt: "数到 40".into(),
                    session_id: Some("th_abc".into()),
                    partial: "1 2 3".into(),
                },
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"kind\":\"resume_turn\""));
        let back: OutboxEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn recovery_card_json_round_trip() {
        let ev = OutboxEvent {
            id: format_id(1_700_000_000_002, "starlight", "abad1dea"),
            agent: "starlight".into(),
            delivery_scope: "tenant-app".into(),
            created_ms: 1_700_000_000_002,
            attempts: 0,
            next_attempt_ms: 1_700_000_000_002,
            last_error: None,
            kind: OutboxKind::RecoveryCard {
                batch_id: "recovery-card-batch".into(),
                scope: "tenant-app".into(),
                chat_id: "oc_1".into(),
                chat_type: "p2p".into(),
                items: vec![
                    RecoveryItem {
                        message_id: "om_a".into(),
                        preview: "ping".into(),
                    },
                    RecoveryItem {
                        message_id: "om_b".into(),
                        preview: "做个 X".into(),
                    },
                ],
                stage: Some("数到 3".into()),
                card_version: BRIDGE_CARD_WIRE_VERSION,
                rendered_card: r#"{"schema":"2.0","body":{"elements":[]}}"#.into(),
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"kind\":\"recovery_card\""));
        let back: OutboxEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn legacy_recovery_card_missing_frozen_wire_deserializes_for_explicit_migration() {
        let legacy = r#"{
            "kind":"recovery_card",
            "batch_id":"recovery-card-batch",
            "scope":"tenant-app",
            "chat_id":"oc_1",
            "chat_type":"group",
            "items":[{"message_id":"om_a","preview":"ping"}],
            "stage":null
        }"#;
        let kind: OutboxKind = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            kind,
            OutboxKind::RecoveryCard {
                card_version: 0,
                ref rendered_card,
                ..
            } if rendered_card.is_empty()
        ));
    }

    #[test]
    fn turn_reply_json_round_trip() {
        let ev = OutboxEvent {
            id: format_id(1_700_000_000_003, "starlight", "cafebabe"),
            agent: "starlight".into(),
            delivery_scope: "test-scope".into(),
            created_ms: 1_700_000_000_003,
            attempts: 0,
            next_attempt_ms: 1_700_000_000_003,
            last_error: None,
            kind: OutboxKind::TurnReply {
                chat_id: "oc_abc".into(),
                chat_type: "group".into(),
                text: "这是回复正文".into(),
                parent_message_id: Some("om_parent".into()),
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"kind\":\"turn_reply\""));
        assert!(s.contains("\"parent_message_id\":\"om_parent\""));
        let back: OutboxEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn turn_reply_no_parent_id_defaults() {
        // 不带 parent_message_id 的 TurnReply 应能正常反序列化(#[serde(default)] 生效)。
        let json = r#"{
            "id":"x","agent":"a","created_ms":1,"attempts":0,"next_attempt_ms":1,
            "last_error":null,
            "kind":{"kind":"turn_reply","chat_id":"oc","chat_type":"p2p","text":"hi"}
        }"#;
        let ev: OutboxEvent = serde_json::from_str(json).unwrap();
        match ev.kind {
            OutboxKind::TurnReply {
                parent_message_id, ..
            } => assert_eq!(parent_message_id, None),
            _ => panic!("应解析为 TurnReply"),
        }
    }

    #[test]
    fn unknown_extra_fields_tolerated() {
        // 旧消费者读新写者多带的字段不该炸(serde 默认忽略未知字段)。
        let json = r#"{
            "id":"x","agent":"a","created_ms":1,"attempts":0,"next_attempt_ms":1,
            "last_error":null,"future_field":42,
            "kind":{
                "kind":"notify_chat","chat_id":"c","chat_type":"p2p",
                "creator":null,"event_operator":null,"reason":"manual_kill",
                "text":"hi","future_payload_field":"x"
            }
        }"#;
        let ev: OutboxEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.agent, "a");
    }

    #[test]
    fn backoff_schedule_5s_15s_45s_135s_cap_600s() {
        assert_eq!(next_backoff_ms(1), 5_000);
        assert_eq!(next_backoff_ms(2), 15_000);
        assert_eq!(next_backoff_ms(3), 45_000);
        assert_eq!(next_backoff_ms(4), 135_000);
        // 5: 5s*3^4 = 405s < 600s cap。
        assert_eq!(next_backoff_ms(5), 405_000);
        // 6: 5s*3^5 = 1215s → cap 600s。
        assert_eq!(next_backoff_ms(6), BACKOFF_CAP_MS);
        assert_eq!(next_backoff_ms(99), BACKOFF_CAP_MS);
        // attempts 0 当作 1。
        assert_eq!(next_backoff_ms(0), 5_000);
    }

    #[test]
    fn retry_horizon_covers_a_full_day() {
        let horizon_ms: u64 = (1..MAX_ATTEMPTS).map(next_backoff_ms).sum();
        assert!(
            horizon_ms >= 24 * 60 * 60 * 1_000,
            "死信前重试窗口应至少覆盖 24 小时，实际 {horizon_ms}ms"
        );
    }

    #[test]
    fn jitter_stays_within_plus_minus_20_percent() {
        let base = 100_000u64;
        // rand01=0.5 → 不抖。
        assert_eq!(jittered_backoff_ms(base, 0.5), base);
        // rand01=0 → -20%。
        assert_eq!(jittered_backoff_ms(base, 0.0), 80_000);
        // rand01=1 → +20%。
        assert_eq!(jittered_backoff_ms(base, 1.0), 120_000);
        // 任意值都落在 [80%,120%]。
        for i in 0..=10 {
            let r = i as f64 / 10.0;
            let v = jittered_backoff_ms(base, r);
            assert!((80_000..=120_000).contains(&v), "r={r} v={v}");
        }
    }

    #[test]
    fn due_boundary() {
        let mut ev = notify_event();
        ev.next_attempt_ms = 1_000;
        assert!(due(&ev, 1_000), "等于即到期");
        assert!(due(&ev, 1_001), "大于到期");
        assert!(!due(&ev, 999), "小于未到期");
    }

    /// 造一条 due/未来到期的事件,id/created_ms 由 ts 控制(便于断言 FIFO 顺序)。
    fn ev_at(created_ms: u64, next_attempt_ms: u64) -> OutboxEvent {
        OutboxEvent {
            id: format_id(created_ms, "a", "deadbeef"),
            agent: "a".into(),
            delivery_scope: "test-scope".into(),
            created_ms,
            attempts: 0,
            next_attempt_ms,
            last_error: None,
            kind: OutboxKind::NotifyChat {
                chat_id: "oc".into(),
                chat_type: "p2p".into(),
                creator: None,
                event_operator: None,
                reason: ExitReason::ShutdownInterrupt,
                text: "hi".into(),
                rendered_card: String::new(),
            },
        }
    }

    #[test]
    fn drain_plan_only_due_sorted_fifo() {
        // 三条:两条已到期(created 100、50),一条未到期(next 在未来)。
        let evs = vec![
            ev_at(100, 100),  // due
            ev_at(50, 50),    // due,更早入队 → 应排前
            ev_at(60, 9_999), // 未到期 → 不入计划
        ];
        let plan = drain_plan(&evs, 1_000);
        assert_eq!(plan.len(), 2, "只取到期的两条");
        assert_eq!(plan[0].created_ms, 50, "FIFO:更早入队的先投");
        assert_eq!(plan[1].created_ms, 100);
    }

    #[test]
    fn drain_plan_caps_at_max_per_tick() {
        // 50 条全 due:一 tick 只取 MAX_PER_TICK 条,且取的是最早的那批。
        let evs: Vec<OutboxEvent> = (0..50).map(|i| ev_at(i, i)).collect();
        let plan = drain_plan(&evs, 1_000_000);
        assert_eq!(plan.len(), MAX_PER_TICK, "限流到 MAX_PER_TICK");
        // 取的是 created_ms 最小的 MAX_PER_TICK 条(0..8)。
        for (i, ev) in plan.iter().enumerate() {
            assert_eq!(ev.created_ms, i as u64);
        }
    }

    #[test]
    fn drain_plan_empty_when_none_due() {
        let evs = vec![ev_at(10, 9_999), ev_at(20, 8_888)];
        assert!(drain_plan(&evs, 100).is_empty(), "全未到期 → 空计划");
        assert!(drain_plan(&[], 100).is_empty(), "无事件 → 空计划");
    }

    #[test]
    fn attempt_failed_retries_then_dead_at_max() {
        let mut ev = notify_event();
        let now = 10_000u64;
        // 第 1~4 次失败:Retry,attempts 递增,next_attempt 按退避更新。
        for expect_attempts in 1..MAX_ATTEMPTS {
            let d = mark_attempt_failed(&mut ev, now, MAX_ATTEMPTS, "boom");
            assert_eq!(d, Disposition::Retry, "attempts={expect_attempts} 应重试");
            assert_eq!(ev.attempts, expect_attempts);
            assert_eq!(ev.last_error.as_deref(), Some("boom"));
            assert_eq!(ev.next_attempt_ms, now + next_backoff_ms(expect_attempts));
        }
        // 封顶这次失败:attempts 到 MAX_ATTEMPTS → Dead。
        let d = mark_attempt_failed(&mut ev, now, MAX_ATTEMPTS, "final");
        assert_eq!(d, Disposition::Dead);
        assert_eq!(ev.attempts, MAX_ATTEMPTS);
        assert_eq!(ev.last_error.as_deref(), Some("final"));
    }

    fn bridge_fence(binding_id: &str, sequence: u64) -> BridgeDeliveryFence {
        BridgeDeliveryFence {
            scope: "tenant-app".into(),
            agent: "starlight".into(),
            chat_id: "oc_group".into(),
            chat_type: "group".into(),
            binding_id: binding_id.into(),
            binding_revision: 7,
            session: crate::bridge::SessionKey::new("/tmp/claude", "sid-1"),
            generation: 3,
            lease_binding_id: binding_id.into(),
            receipt_id: format!("receipt-{binding_id}-{sequence}"),
            sequence,
        }
    }

    fn bridge_event(
        id: &str,
        binding_id: &str,
        sequence: u64,
        created_ms: u64,
        next_attempt_ms: u64,
    ) -> OutboxEvent {
        OutboxEvent {
            id: id.into(),
            agent: "starlight".into(),
            delivery_scope: "tenant-app".into(),
            created_ms,
            attempts: 0,
            next_attempt_ms,
            last_error: None,
            kind: OutboxKind::ExternalBridgeTurn {
                fence: bridge_fence(binding_id, sequence),
                turn_id: format!("turn-{id}"),
                card_version: BRIDGE_CARD_WIRE_VERSION,
                rendered_card: "frozen-card".into(),
            },
        }
    }

    #[test]
    fn bridge_outbox_schema_round_trips_all_kinds() {
        let fence = bridge_fence("binding-1", 11);
        let events = [
            OutboxKind::BridgeActivationAudit {
                fence: fence.clone(),
                ticket_id: "ticket-1".into(),
                choice_id: "choice-1".into(),
                requester_open_id: "ou_owner".into(),
                card_version: BRIDGE_CARD_WIRE_VERSION,
                rendered_card: "frozen-audit-card".into(),
                control_card: "frozen-control-card".into(),
            },
            OutboxKind::ExternalBridgeTurn {
                fence: fence.clone(),
                turn_id: "turn-1".into(),
                card_version: BRIDGE_CARD_WIRE_VERSION,
                rendered_card: "frozen-turn-card".into(),
            },
            OutboxKind::BridgeContextCapsule {
                fence: fence.clone(),
                ticket_id: "ticket-1".into(),
                choice_id: "choice-1".into(),
                requester_open_id: "ou_owner".into(),
                card_version: BRIDGE_CARD_WIRE_VERSION,
                rendered_card: "frozen-context-card".into(),
            },
            OutboxKind::BridgeHandoffStatus {
                fence,
                attempt_id: "attempt-1".into(),
                notice: BridgeHandoffNotice::Requested,
                card_version: BRIDGE_CARD_WIRE_VERSION,
                rendered_card: "frozen-handoff-card".into(),
            },
        ];
        for kind in events {
            let event = OutboxEvent {
                id: "bridge-id".into(),
                agent: "starlight".into(),
                delivery_scope: "tenant-app".into(),
                created_ms: 1,
                attempts: 0,
                next_attempt_ms: 1,
                last_error: None,
                kind,
            };
            let json = serde_json::to_string(&event).unwrap();
            let round_trip: OutboxEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(round_trip, event);
            assert_eq!(event.kind.bridge_fence(), round_trip.kind.bridge_fence());
        }
    }

    #[test]
    fn activation_audit_carries_no_external_body() {
        let kind = OutboxKind::BridgeActivationAudit {
            fence: bridge_fence("binding-1", 1),
            ticket_id: "ticket-1".into(),
            choice_id: "choice-1".into(),
            requester_open_id: "ou_owner".into(),
            card_version: BRIDGE_CARD_WIRE_VERSION,
            rendered_card: "fixed-audit-without-transcript".into(),
            control_card: "fixed-control-without-transcript".into(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        for body_key in ["user_text", "assistant_text", "capsule", "messages"] {
            assert!(
                !json.contains(body_key),
                "审计事件不得携带正文键 {body_key}"
            );
        }
    }

    #[test]
    fn deterministic_bridge_delivery_ids_are_stable_distinct_and_feishu_safe() {
        let receipt = "550e8400-e29b-41d4-a716-446655440000";
        let audit = bridge_delivery_id(BridgeDeliveryKind::ActivationAudit, receipt);
        let turn = bridge_delivery_id(BridgeDeliveryKind::ExternalTurn, receipt);
        let context = bridge_delivery_id(BridgeDeliveryKind::ContextCapsule, receipt);
        let handoff = bridge_delivery_id(BridgeDeliveryKind::HandoffStatus, receipt);
        assert_eq!(
            audit,
            bridge_delivery_id(BridgeDeliveryKind::ActivationAudit, receipt)
        );
        assert_ne!(audit, turn);
        assert_ne!(turn, context);
        assert_ne!(context, handoff);
        for id in [audit, turn, context, handoff] {
            assert!(id.len() <= 50, "飞书 uuid 最长 50 字节: {id}");
            assert!(
                id.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "id 必须可安全用作文件名与飞书 uuid: {id}"
            );
        }
    }

    #[test]
    fn frozen_bridge_card_validation_is_versioned_not_current_template_bound() {
        let legacy = r#"{"schema":"2.0","body":{"elements":[]},"legacy_template":true}"#;
        assert!(valid_frozen_bridge_card(BRIDGE_CARD_WIRE_VERSION, legacy));
        assert!(!valid_frozen_bridge_card(
            BRIDGE_CARD_WIRE_VERSION + 1,
            legacy
        ));
        assert!(!valid_frozen_bridge_card(
            BRIDGE_CARD_WIRE_VERSION,
            "not-json"
        ));
    }

    #[test]
    fn bridge_fifo_blocks_newer_event_while_oldest_is_in_backoff() {
        let events = vec![
            bridge_event("old", "binding-a", 1, 10, 9_999),
            bridge_event("new", "binding-a", 2, 20, 20),
            bridge_event("other", "binding-b", 1, 30, 30),
        ];
        let plan = drain_plan(&events, 100);
        assert_eq!(
            plan.iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            ["other"]
        );
    }

    #[test]
    fn bridge_fifo_returns_only_one_due_head_per_binding() {
        let events = vec![
            bridge_event("second", "binding-a", 2, 5, 5),
            bridge_event("first", "binding-a", 1, 50, 50),
            bridge_event("other", "binding-b", 1, 40, 40),
        ];
        let plan = drain_plan(&events, 100);
        assert_eq!(
            plan.iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            ["other", "first"],
            "同 binding 用 sequence 判头部；不同 binding 不互相阻塞"
        );
    }
}
