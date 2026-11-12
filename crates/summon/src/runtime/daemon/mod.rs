use crate::capture::{MediationCard, TurnOutcome};
use crate::frontend::client::FeishuClient;
// 出站投递原语已下沉 frontend::outbound(A6);daemon 各 handler 经此 import 使用,
// 「发送成功才归档 outbound.ndjson」不变量由 frontend 层唯一收口。
use crate::frontend::outbound::{
    send_card_archived, send_card_archived_id, send_card_to_open_id_archived_id,
    send_markdown_archived, send_markdown_archived_anchored, send_markdown_archived_replying,
    send_markdown_to_open_id_archived, send_rendered_markdown_idempotent_archived,
    update_card_archived, update_card_redacted_archived,
};
use crate::frontend::{ChatRef, FeishuFrontend, Frontend, MsgRef, StreamId};
// 根路径与日志已下沉(A1):paths 是纯路径拼装、log 是 stderr 行日志,与 daemon 行为无关。
use crate::observe::log::log;
use crate::runtime::paths::{
    boot_record_file, clean_shutdown_marker, control_sock, daemon_version_file, events_sock,
    lock_file, pid_file, ready_file, refusal_file, run_dir, start_request_file, stop_request_file,
    summon_home,
};
use crate::session::session::{Session, Start};
use crate::session::slash_exec::{compose_slash_receipt, SlashInjection, SlashOutcome};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use summon_core::attach::{guess_image_ext, resource_note, safe_file_name};
use summon_core::config::{Config, Messaging};
use summon_core::driver::slash_pool::{
    self, LifecycleIntent, RpcSlashCommand, SettingAction, SlashClass, SlashEntry, SlashPlan,
    SlashRefusal,
};
use summon_core::driver::{CliDriver, CompactSpec, ModelSwitchSpec};
use summon_core::ipc::{Event, Resource};
use summon_core::pane_input::InjectGate;
use summon_core::registry::{chat_tag, Agent, CaptureMode, OperateScope, Registry};
use summon_core::settings::{ChatOverrides, ResolvedSettings, SettingKey};

/// 显式无头打断的优雅宽限:SIGINT 后本时间内未收尾即 SIGKILL 整组。
/// 只用于 ChildKiller/InFlight,与 tmux/Transcript 无关。
const HEADLESS_INTERRUPT_GRACE: Duration = Duration::from_secs(2);
/// SIGKILL 后再给 worker 回收 stdout/Child 的有界窗口。
const HEADLESS_FORCE_KILL_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct MirrorStopFence {
    agent: String,
    /// 上一代实际使用的飞书 app/env/secret/scope/agent-dir 摘要；只存 SHA-256，不落凭据。
    feishu_epoch_fingerprint: String,
    session: String,
    tag: String,
    chat_id: String,
    chat_type: String,
    /// stop snapshot 时盘上那枚 exact v3 TerminalBoundary 原文。**不再要求它已追平 EOF** ——
    /// 「停机那一刻正好有一轮在跑」恰恰保证追不平,而那正是最需要补发的一轮(见 `stop_eof`)。
    offset_record: String,
    /// 停机快照那一刻的 transcript EOF。
    ///
    /// **它是「主路径不可能发过」的分界线。** `[stop_eof, ∞)` 的字节全部落盘于 daemon 死后,
    /// 那段时间没有任何 chat 线程活着能投递它们;而 `[offset, stop_eof)` 是停机前就已落盘的旧欠账,
    /// 旧 main 发没发不可证明,只能保守不发。旧记录反序列化成 0,配合下面的新鲜度闸自动 fail-closed。
    #[serde(default)]
    stop_eof: u64,
    /// `stop_eof` 位置前 1KiB 的 exact checkpoint(base64)。只比 offset 数字会被同长度 replacement
    /// 冒充 —— 与 `mirror_offset_is_exact_eof` 同一条理由,分界线本身也必须能验真。
    #[serde(default)]
    stop_eof_checkpoint: String,
    /// 停机时刻(unix 秒)。新鲜度闸的输入:daemon 停了三天再起,中间 tmux 里跑的东西不该被当成
    /// 「停机窗口产物」一股脑灌进群。旧记录为 0 → 恒判过期 → 不补。
    #[serde(default)]
    stopped_at_secs: u64,
}

/// 停机窗口补发的新鲜度上限。超过这个时长的停机,窗口内那些 turn 已经不再是「用户刚才在等的
/// 那条回复」,补发只会变成往群里倒历史。取 30min:比任何一次 deploy/重启都宽裕一个量级。
const MIRROR_CATCHUP_MAX_STALENESS_SECS: u64 = 30 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CleanShutdownRecordV2 {
    version: u8,
    #[serde(default)]
    mirror_fences: Vec<MirrorStopFence>,
    /// 本代 daemon 停机的那一刻(unix 秒)。**daemon 级事实,与任何 chat 无关** ——
    /// 接管的新鲜度判据要它:mirror fence 只有「已提交过 offset」的 chat 才签得出,
    /// 首轮就被重启的会话一条 fence 都没有,拿 fence 的时刻当停机时刻会让它永远判过期。
    /// 旧记录反序列化为 0 → 恒判过期 → 不接管。
    #[serde(default)]
    stopped_at_secs: u64,
}

/// 本代获批的停机窗口补发计划:`(agent, tag) → catchup_floor`。
///
/// 值是上一代的 `stop_eof` —— 只有**终点**落在它右侧的 turn 才允许联网补发(见
/// [`MirrorStopFence::stop_eof`])。以前这里只是个 `HashSet`,因为当时的批准语义是「拉起来收敛
/// offset」,一个字节都不发;现在要发,就必须把「从哪条线开始能发」一起带上。
static APPROVED_MIRROR_CATCHUP: std::sync::OnceLock<
    std::collections::HashMap<(String, String), u64>,
> = std::sync::OnceLock::new();

fn record_approved_mirror_catchup(approved: std::collections::HashMap<(String, String), u64>) {
    let _ = APPROVED_MIRROR_CATCHUP.set(approved);
}

/// 本 chat 的补发下界。`None` = 未获批,历史一律只观察不发(与本次改动前逐字节一致)。
fn mirror_catchup_floor(agent: &str, tag: &str) -> Option<u64> {
    APPROVED_MIRROR_CATCHUP
        .get()
        .and_then(|approved| approved.get(&(agent.to_string(), tag.to_string())))
        .copied()
}

/// 已经用掉补发批准的 chat。
static CONSUMED_MIRROR_CATCHUP: std::sync::Mutex<std::collections::BTreeSet<(String, String)>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// chat 线程起来时领取本 chat 的补发下界,每个 chat 每代只领得到一次。批准的判据(上一代的
/// `stop_eof` 与本代 boot 冻结的 EOF)只对 chat 线程本代第一次起来时的 watcher 成立:之后才起的
/// watcher(重载、镜像从关到开)以那一刻的 EOF 为起点,中间那段是本代主路径已经投递过的轮,
/// 再按补发放行就是重发。
fn take_mirror_catchup_floor(agent: &str, tag: &str) -> Option<u64> {
    take_catchup_floor(
        mirror_catchup_floor(agent, tag),
        &CONSUMED_MIRROR_CATCHUP,
        agent,
        tag,
    )
}

fn take_catchup_floor(
    approved: Option<u64>,
    consumed: &std::sync::Mutex<std::collections::BTreeSet<(String, String)>>,
    agent: &str,
    tag: &str,
) -> Option<u64> {
    let floor = approved?;
    let first = consumed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((agent.to_string(), tag.to_string()));
    first.then_some(floor)
}

fn mirror_catchup_is_approved(agent: &str, tag: &str) -> bool {
    mirror_catchup_floor(agent, tag).is_some()
}

// A1 拆分:daemon 目录模块。各子文件 `use super::*` 共享本文件的 imports 与跨子模块条目。
// 子模块默认只在 daemon 内互通；真正被 crate 其他模块复用的接缝在下方具名导出，避免 glob
// 把 chat 热路径实现意外扩成 crate-wide API。
mod admit;
mod agent_env;
mod boot;
mod bridge;
mod cards;
mod chat_meta;
mod chat_thread;
/// summon 自己处理的指令回执:纯确认在群里贴完成表情,不为它开话题。
mod command_ack;
/// 压缩后上下文长度卡的去重:summon 发起的压缩在途登记 + 每份转录已报告到哪条边界。
mod compact_ledger;
/// 主轮结束后继续跟随本轮 claude 子 agent 的 CoT。
mod cot_follower;
/// 崩溃后自动拉起的熔断:上一代没跑稳就死的,非人工启动不起。
mod crash_guard;
/// `/dash` 签发控制台卡与卡上按钮的点击激活。
mod dashboard_entry;
/// 控制台接口够不着的 daemon 侧事实与动作(会话活没活着、按群设置文件、免 @ 核验队列)。
pub(crate) mod dashboard_port;
pub(crate) mod delegate;
mod dispatcher;
pub(crate) mod document_comment_draft;
mod document_comments;
/// 会话诊断包:收集、投影、打包(CLI `summon session export` 与飞书 `/summon-export` 共用)。
pub(crate) mod export;
/// `/summon-export`:导出本会话的诊断包 / 完整包,只私聊发起人。
mod export_entry;
/// 启动期把 fd 软上限提到够用(launchd 拉起的只有 256)。
mod fd_limit;
/// 跨群 fork 的落盘侧:邀请函(双向授权的短期记录)+ 唯一提交点。
mod fork;
mod fork_entry;
pub(crate) mod group_target;
mod headless_activity;
mod headless_completion;
mod mentionless;
mod mentionless_setup;
mod outbox_drain;
/// 换代后接管上一代那一轮的失败分类与去处(锚点本体在 `summon_core::TurnAnchor`)。
mod reattach;
mod record;
mod scheduler;
mod sentinel;
/// 主线程的停机等待器:自管道坏了退回轮询停机 flag,不让 daemon 随之退出。
mod shutdown_wait;
mod steward;
mod subagent_card;
mod subagent_watcher;
mod teardown;
/// 终端卡的签发:`/term`、面板与回执上的「>_ 打开终端」、私聊卡上的「🔄 换一组新链接」。
mod terminal;
/// waiting 标记的 daemon 侧时机:轮末置位、owner 的明确动作清除。
mod waiting;
// 供 run 等 daemon 外模块复用的窄接缝。
pub(crate) use chat_thread::{
    fetch_inbound_resources, prepare_chat_system_prompt_file, tool_input_brief,
};
// 回放与 frontend 单测才需直驱消费器；生产 crate 不暴露热路径实现。
#[cfg(test)]
pub(crate) use chat_thread::{
    card_stream_consume, completion_badge_for_test, completion_badge_with_session_for_test,
    failure_notice_for_test,
};
#[cfg(test)]
mod tests;
pub(crate) use admit::*;
pub(crate) use agent_env::*;
pub use boot::*;
use bridge::*;
#[cfg(test)]
pub(crate) use cards::cursor_context_budget_from_pool;
/// Menu Bar 的会话列表与飞书面板要同一份 model 候选,所以这一个接缝开到 crate 级。
pub(crate) use cards::model_candidates;
use cards::*;
pub(crate) use chat_meta::*;
use chat_thread::*;
use command_ack::*;
pub(crate) use crash_guard::{last_refusal, write_start_request, write_stop_request};
#[cfg(npm_dist)]
pub(crate) use crash_guard::{left_unfinished, stop_requested};
use dispatcher::*;
// 遥控运行时启动恢复要逐 agent 拿飞书句柄。
#[cfg(test)]
pub(crate) use outbox_drain::drain_agent_for_test;
pub(crate) use outbox_drain::DrainAgents;
use outbox_drain::*;
use reattach::*;
// record 的 debug-only 项在 release 编译掉,pub(crate) glob 再导出会报空警告;
// 模块内私有 glob 引入,外部(均为 debug-only 调用点)只具名导出所需两项
use record::*;
#[cfg(debug_assertions)]
pub(crate) use record::{record_flag_file, record_outbound_timeline, InboundRecord};
use subagent_watcher::*;
pub(crate) use teardown::now_unix_ms;
use teardown::*;

/// 交给某 agent dispatcher 的一条入站消息。
/// `pub(crate)`:Core Port 接缝(`crate::runtime::core::Core::dispatch_inbound`)按它委派给 dispatcher。
///
/// serde(`Serialize`/`Deserialize`)纯为录制-回放夹具(RR2):入站 tee 落 `inbound.ndjson`、回放
/// 注入器读回喂进 dispatcher。**只加 derive、不改语义**:字段全部已 serde-able(`Mention`/`Resource`
/// 自带 serde),无运行期不可序列化语义,故无须 serde 镜像。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Inbound {
    pub(crate) chat_id: String,
    pub(crate) chat_type: Option<String>,
    pub(crate) text: String,
    pub(crate) sender: String,
    /// 发信人 union_id(同开发者下跨 app 稳定);白名单优先用它命中,空则退回 open_id。
    pub(crate) sender_union_id: String,
    /// 发信人类型:`"user"` = 真人,`"app"` = bot,空 = 老 sidecar 未带(未知)。
    /// 注入 prompt 时据此标注发送方身份,让 agent 能判断「这条是不是另一个 bot 发的」。
    /// `#[serde(default)]`:旧录制 cassette 无此字段,回放按空串兜底。
    #[serde(default)]
    pub(crate) sender_type: String,
    pub(crate) message_id: String,
    /// 被引用/被回复消息的 message_id(引用回复时有,普通消息为空);chat 线程据此拉回原文做上下文。
    pub(crate) parent_id: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) root_id: Option<String>,
    /// 该消息的 @ 列表(群消息门控:只有命中本 bot open_id 才应答)。
    pub(crate) mentions: Vec<summon_core::ipc::Mention>,
    /// 随消息发来的附件(图片/文件等);chat 线程下载到本地后把路径喂给 agent。
    pub(crate) resources: Vec<Resource>,
    /// 飞书消息创建时间(毫秒,sidecar 透传 msg.create_time;老 sidecar 为 0=未知)。
    /// 入站去重+时效闸(`InboundGate`)据此判「抖动后补投的旧事件」。
    /// `#[serde(default)]`:旧录制 cassette 无此字段,反序列化按 0 兜底(回放向后兼容)。
    #[serde(default)]
    pub(crate) create_time_ms: i64,
    /// daemon 显式重放的消息(优雅重启重注入 / 恢复卡重放)——时效闸对它免检,只保留
    /// message_id 去重。此前的做法是把 `create_time_ms` 归零当免检信号,代价是 WAL 落盘后
    /// 永久丢失真实发送时间(2026-08-05 真机排障时无法回答「这条消息原本何时发出」);
    /// 控制信号与数据分离后 `create_time_ms` 全程保真。
    /// `#[serde(default)]`:旧记录无此字段,按 false(非重放)兜底。
    #[serde(default)]
    pub(crate) replayed: bool,
    /// 这条消息在**飞书上的真实 message_id**。`None` = `message_id` 本身就是真的。
    ///
    /// 恢复卡重放时 `message_id` 被换成确定性的 `recovery-run-<hash>`(见
    /// `outbox::recovery_replay_id`):那是本地幂等键,原 id 已被 InboundGate 封住不能复用。
    /// 但贴表情、回复原消息、建原生 CoT 都要拿**真实**的 `om_...` 去调飞书,拿合成 id 一律
    /// 400(`99992354 not a valid open_message_id`)—— 表现就是恢复轮没有表情、总结卡发不出、
    /// 原生 CoT 建不起来退回卡片。所以真实 id 必须跟着消息一起传下去,而不是只留在 WAL 里。
    #[serde(default)]
    pub(crate) anchor_message_id: Option<String>,
    /// 这条是 daemon 换代后的**接管**投递:上一代已经把它注入进 CLI 且那一轮还在跑,本代只接住
    /// 输出,绝不重新注入(重注入就是同一句话跑两遍)。由启动扫描按盘上锚点产出,普通入站恒 false。
    ///
    /// **产出方必须同时置 `replayed = true`**:接管复用原始 message_id 与 create_time_ms(锚点要
    /// 拿它们与 WAL 对账),而「消息发出 → 跑了很久 → 换代 → 启动扫描」轻易就超过入站闸的
    /// max_age —— 不免时效判定的话整条接管路径会被 DropStale 静默吞掉。闸的调用点因此对
    /// `replayed || reattach` 都传 create_time=0(只去重、不判时效),msg 本身的时间保真。
    /// `#[serde(default)]`:旧记录/旧 cassette 无此字段,按 false(普通轮)兜底。
    #[serde(default)]
    pub(crate) reattach: bool,
}

/// 投递给某 agent dispatcher 的消息。
/// `pub(crate)`:Core 接缝按它把入站消息/卡片回调委派给 dispatcher。
pub(crate) enum DispatchMsg {
    /// daemon 优雅停机屏障:让 dispatcher 立即停止消费/创建 chat worker。
    /// 只关本进程内 producer,不执行任何 tmux 销毁。
    Shutdown,
    /// 停用 agent 的清理屏障；只有所有 chat worker 已退出才确认。
    Retire(std::sync::mpsc::SyncSender<Result<(), String>>),
    /// 本机 CLI 刚改了这个 agent 的 agent.yaml:唤醒 dispatcher,循环顶部的 mtime 检查当场把改动
    /// 推给在线 chat 线程。不唤醒就要等下一个事件或空闲扫描节拍(`idle_scan_secs`)。
    AgentYamlWritten,
    Inbound(Inbound),
    MentionlessChecked(mentionless::Checked),
    MentionlessSetupChecked(mentionless_setup::Checked),
    /// 控制台发起的免 @ 开启:进与 `/mentionless on` 同一条核验队列,结果交回页面。
    DashboardMentionless(mentionless_setup::DashboardRequest),
    Card(CardActionMsg),
    /// 某 chat 线程清理完毕,dispatcher 摘除其 handle 并(`notify=true` 时)发「会话已退出」回执。
    /// `reason` 决定文案 + 群在/不在分支;`creator` = 该会话群主近似(p2p 回退目标)。
    /// `notify=false`:配置变更快重置(不是「退出」动作),只摘 handle、不发回执(避免误报)。
    ChatExited {
        chat_id: String,
        chat_type: String,
        reason: summon_core::exit::ExitReason,
        creator: Option<String>,
        notify: bool,
        /// 本会话首次冷启时间,退出卡片展示「起始」用。teardown 前从 `.session` 捕获
        /// (teardown 会清掉 `.session`)。
        session_created_at: Option<String>,
    },
    /// chat worker 意外死亡(如轮末发现 tmux 已不在)退出前交还的排队消息。worker 先发
    /// `ChatExited` 再发本消息(同 sender FIFO):dispatcher 处理到这里时旧 handle 必已摘除,
    /// 直接 get-or-spawn 新 worker 重投。排队 Task 已过完整 admission(WAL pending、去重闸已
    /// seen,飞书重投救不回),**不重跑入站闸**;重投失败不销账,WAL 仍是唯一真相、冷启兜底。
    /// 2026-08-05 真机:此前 worker 退出即 drop rx,队列静默蒸发,一条消息悬在 WAL 2.5h
    /// 直到 daemon 重启才被冷启重注入。
    OrphanedTasks {
        chat_id: String,
        chat_type: String,
        /// 原 worker 的 tmux 会话名(重建沿用,不在 dispatcher 侧重推导 owner-p2p 命名)。
        tmux_name: String,
        tasks: Vec<ChatTask>,
    },
    /// 外部飞书事件(群解散 / bot 被移除)驱动的拆除:对该 chat 的活跃会话走 do_teardown +
    /// p2p 通知群主。这是**显式事件驱动**的合法 kill 路径(等同用户点退出),不是 idle 线程擅杀。
    ChatGone {
        chat_id: String,
        reason: summon_core::exit::ExitReason,
        operator_open_id: String,
    },
    /// `summon agents kill` 经 control.sock → Core 路由来的无头会话拆除:在线投
    /// `ThreadMsg::KillTeardown`(线程先打断在跑子进程再 do_teardown);不在线则按盘上 .meta 归档。
    /// `reset` 由 `TeardownMode::Reset` 决定(清 `.session`=真重置)。这是**显式用户命令**驱动的
    /// 合法 kill 路径(等同 `agents down`/面板退出),不是 idle 线程擅杀。
    /// 经目标 agent 操作权限检查的 fleet 会话动作。
    FleetSessionControl {
        chat_id: String,
        archive: bool,
    },
    /// 控制台会话页的「打断」:与控制卡「⏸ 打断」同一道本轮凭证围栏,投 `ThreadMsg::Interrupt`
    /// 给 chat 线程;结果经 `reply` 交回。
    DashboardInterrupt {
        chat_id: String,
        turn_id: String,
        reply: std::sync::mpsc::Sender<DashInterrupt>,
    },
    TeardownChat {
        chat_id: String,
        reset: bool,
    },
    /// chat 轮内 agent 提问:往该会话发一张提问卡(form:每题一个选择/输入 + 提交/全部跳过)。
    /// 由 `Core::do_chat_ask` 产出;人点提交后经卡片回调 resolve 闸门,唤醒阻塞的 `summon ask`。
    ChatAskCard {
        gate_id: String,
        chat_id: String,
        chat_type: String,
        requester_open_id: String,
        questions: Vec<summon_core::core_port::AskQuestion>,
    },
    /// chat 提问等太久:往该会话补一张提醒卡片(@ 提问者)。**这不是超时** —— 闸门仍在等,
    /// 只是提醒人还有一问未答。由 `Core::wait_gate` 的 chat 分支按固定间隔产出。
    ChatAskRemind {
        gate_id: String,
        chat_id: String,
        chat_type: String,
        requester_open_id: String,
        waited_secs: u64,
    },
    /// 提问 waiter 已随 turn/客户端结束，或 non-blocking 窗口到期：把 exact 原卡置灰。
    /// 不进 callback WAL；它只是内存 gate 生命周期的 UI 投影，daemon 重启后旧卡本就失效。
    ChatAskInvalidated {
        chat_id: String,
        chat_type: String,
        message_id: String,
        requester_open_id: String,
        questions: Vec<summon_core::core_port::AskQuestion>,
        reason: ChatAskEndReason,
    },
    /// Claude 的 PermissionRequest hook 交来一次工具授权:往该会话发一张「允许 / 拒绝」卡。
    /// 由 `Core::do_claude_hook_ask` 产出;人点按钮经卡片回调 resolve 闸门,hook 把裁决回给 Claude。
    ClaudePermissionCard {
        gate_id: String,
        chat_id: String,
        chat_type: String,
        requester_open_id: String,
        request: summon_core::core_port::ClaudeHookRequest,
    },
    /// 授权闸门没等到人就结束了(hook 断开:终端里答过、被自动拒绝、claude 退出):原卡原地作废。
    ClaudePermissionVoid {
        chat_id: String,
        chat_type: String,
        message_id: String,
        requester_open_id: String,
        request: summon_core::core_port::ClaudeHookRequest,
    },
    /// Claude 的 PostCompact hook 报告某个会话压缩完了(由 `Core::do_claude_compacted` 产出):
    /// dispatcher 另起线程找到这次压缩的边界、推压缩后的上下文长度卡。summon 自己发起的那次由发起方
    /// 推,同一条边界只推一张(见 `compact_ledger`)。
    ClaudeCompacted {
        chat_id: String,
        chat_type: String,
        session: String,
        /// Core 收到上报的时刻(Unix 毫秒)。只认不早于它太多的边界,不受 dispatcher 排队拖慢。
        reported_at_ms: i64,
    },
    /// 定时器到点触发(scheduler 线程产):路由到目标 chat 线程跑一轮 prompt。已在创建时 operate-gate
    /// 授权,故绕 talk-gate;chat_type 决定会话命名;fire_id 作合成 message_id(防去重误杀)。
    ScheduledFire {
        chat_id: String,
        chat_type: String,
        prompt: String,
        fire_id: String,
        output: summon_core::ScheduleOutput,
        context: summon_core::ScheduleContext,
    },
    /// chat worker 回报:某条定时 fire **真正取得 durable start**(inbox WAL `Marked`),这一轮
    /// 确定要跑。静默回收(`timer_idle_retire_secs`)的来源判据只认这条确认,不认投递:
    /// 投递 ≠ 执行 —— 迟到的重复 fire / 启动恢复重注入会被 WAL 判 `AlreadyStarted` 跳过,
    /// 它们若在投递点就能把会话改判成定时尾轮,真人会话会被误回收(终审阻塞 2026-08-13)。
    /// dispatcher 收到后仅当 `fire_id` 仍是该 chat 的最新入站(exact id 代际围栏)才置定时尾轮。
    ScheduledAdmitted {
        chat_id: String,
        fire_id: String,
    },
    /// 用户撤回消息:dispatcher 把 message_id 标记为 recalled(写 WAL tombstone),
    /// chat 线程消费 Task 前检查、已撤回则跳过。仅影响排队中消息,不打断正在跑的轮。
    RecallMessage {
        message_id: String,
    },
    /// sentinel 触发(owner 在任意会话显式召唤,sentinel 线程产):往目标 chat 注入
    /// 「上下文 + 触发原文」跑一轮。触发本身是 owner 亲手发的指令,故绕 talk-gate;
    /// `fire_id` 用**真实触发消息 id**(bot 已进群,react/回复可用),与 WAL 键、task
    /// message_id 三者一致。who 三元组 = 触发人(owner)真实身份。
    ///
    /// `/handoff` 的接手消息走同一条路(`fire_id` = `handoff-<发起消息 id>`):发起人在源 agent 过了
    /// operate-gate,在目标 agent 过了对话名单,who 三元组是发起人在接收方 app 下的身份。
    ///
    /// 上下文提醒卡的 Handoff 也走这里(`fire_id` = `ctx-handoff-<卡片 message_id>`,一张卡只落一次
    /// 盘):点的人在本 agent 过了 operate-gate,`handoff_note` 带输入框内容,这一轮按
    /// `/handoff <备注>`(不带目标)跑。
    SentinelFire {
        chat_id: String,
        chat_type: String,
        prompt: String,
        fire_id: String,
        who: String,
        who_id: String,
        who_union_id: String,
        handoff_note: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatAskEndReason {
    TimedOut,
    TurnEnded,
    /// hook 来的提问在终端里答过或被 Claude 收回。
    ResolvedElsewhere,
}

/// chat worker 退出前抽干自己的收件队列,返回 (排队 Task, 其余消息条数)。
/// worker 一旦 return,rx 随之 drop,mpsc 里未取出的消息**静默蒸发**——没有回调、没有日志、
/// WAL 不知情;排队 Task 的 WAL 记录停在 pending/started=false,运行期无人再扫,只能悬到冷启
/// (2026-08-05 真机事故:2.5h)。因此每个退出路径都必须显式抽干:意外死亡路径把 Task 经
/// `DispatchMsg::OrphanedTasks` 交还 dispatcher;主动拆除路径至少把损失记进日志。
pub(crate) fn drain_orphaned_tasks(rx: &Receiver<ThreadMsg>) -> (Vec<ChatTask>, usize) {
    let mut tasks = Vec::new();
    let mut dropped = 0usize;
    while let Ok(m) = rx.try_recv() {
        match m {
            ThreadMsg::Task(t) => tasks.push(*t),
            _ => dropped += 1,
        }
    }
    (tasks, dropped)
}

/// 主动拆除(Clean/Reset/Gone/Kill/SilentExit)路径的队列收尾:拆除语义下排队消息不重投,
/// 但绝不允许**静默**丢——把损失落日志(WAL 记录仍在 pending,冷启会兜底重注入)。
pub(crate) fn log_dropped_queue_on_teardown(
    rx: &Receiver<ThreadMsg>,
    agent_name: &str,
    chat_id: &str,
) {
    let (tasks, others) = drain_orphaned_tasks(rx);
    if !tasks.is_empty() || others > 0 {
        log(&summon_i18n::format!("r9be7a8eb3e", tasks.len()));
    }
}

impl DispatchMsg {
    /// 日志用的种类名。只给人看,别拿它做分派 —— 分派要走 `match` 让编译器逼出新分支。
    pub(crate) fn kind_label(&self) -> &'static str {
        match self {
            Self::Shutdown => "Shutdown",
            Self::Retire(_) => "Retire",
            Self::AgentYamlWritten => "AgentYamlWritten",
            Self::Inbound(_) => "Inbound",
            Self::MentionlessChecked(_) => "MentionlessChecked",
            Self::MentionlessSetupChecked(_) => "MentionlessSetupChecked",
            Self::DashboardMentionless(_) => "DashboardMentionless",
            Self::Card(_) => "Card",
            Self::ChatExited { .. } => "ChatExited",
            Self::OrphanedTasks { .. } => "OrphanedTasks",
            Self::ChatGone { .. } => "ChatGone",
            Self::TeardownChat { .. } => "TeardownChat",
            Self::FleetSessionControl { .. } => "FleetSessionControl",
            Self::DashboardInterrupt { .. } => "DashboardInterrupt",
            Self::ChatAskCard { .. } => "ChatAskCard",
            Self::ChatAskRemind { .. } => "ChatAskRemind",
            Self::ChatAskInvalidated { .. } => "ChatAskInvalidated",
            Self::ClaudePermissionCard { .. } => "ClaudePermissionCard",
            Self::ClaudePermissionVoid { .. } => "ClaudePermissionVoid",
            Self::ClaudeCompacted { .. } => "ClaudeCompacted",
            Self::ScheduledFire { .. } => "ScheduledFire",
            Self::ScheduledAdmitted { .. } => "ScheduledAdmitted",
            Self::SentinelFire { .. } => "SentinelFire",
            Self::RecallMessage { .. } => "RecallMessage",
        }
    }

    /// 测试用:造一条最小 Inbound(只填 Core 接缝单测关心的 chat/text,其余取默认)。
    /// 仅 `cfg(test)`,不进生产。
    #[cfg(test)]
    pub(crate) fn inbound_for_test(chat_id: &str, text: &str) -> Self {
        DispatchMsg::Inbound(Inbound {
            anchor_message_id: None,
            chat_id: chat_id.to_string(),
            chat_type: None,
            text: text.to_string(),
            sender: "ou_sender".to_string(),
            sender_union_id: String::new(),
            sender_type: String::new(),
            message_id: "om_test".to_string(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: vec![],
            resources: vec![],
            create_time_ms: 0,
            replayed: false,
            reattach: false,
        })
    }
}

/// 卡片回调:chat + 被点卡片的 message_id(选项卡片 PATCH 用)+ 按钮完整 value(daemon 解析)
/// 与点按钮的人 open_id(破坏性动作 `清理` 鉴权用;sidecar 已回传)。
///
/// `pub(crate)`:作为 `DispatchMsg::Card` 的载荷,随 `DispatchMsg` 的 `pub(crate)` 暴露给 Core 接缝。
///
/// serde 同 `Inbound`:RR2 录制-回放夹具用(卡片回调 tee/回放)。`value` 是 `serde_json::Value`,
/// 本就可序列化;**只加 derive、不改语义**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CardActionMsg {
    pub(crate) chat_id: String,
    pub(crate) message_id: String,
    pub(crate) operator_open_id: String,
    /// 飞书事件 header.event_id；同一回调重投保持不变，用于副作用幂等。
    #[serde(default)]
    pub(crate) event_id: String,
    pub(crate) value: serde_json::Value,
    /// daemon durable callback WAL id；sidecar/录制输入没有此字段，由接收层落盘后注入。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) wal_id: Option<String>,
    /// 仅内存态：本 callback 来自 daemon boot 的 WAL replay，而不是本代 live ingress。
    /// 旧 WAL / 录制夹具默认 false；boot 入队前显式置 true，不回写原记录。
    #[serde(default)]
    pub(crate) wal_replayed: bool,
}

/// 本轮 worker 持有的消费证明，不序列化、不跨轮沿用。
#[derive(Default)]
struct SessionEndEvidence(std::cell::RefCell<Option<String>>);

/// dispatcher 投递给单个 chat 线程的消息。
/// mid-turn 打断/steer 上下文:无头一轮跑在 worker 线程时，chat 线程据它消费新消息并决定
/// 强制打断、Cursor CLI resume 或原生 steer。无头路使用；Transcript 仅复用相关开关与 WAL 契约。
struct FollowupCtx<'a> {
    rx: &'a Receiver<ThreadMsg>,
    /// 本 chat 自己 channel 的发送端(clone):mid-turn 偷看 rx 时取出的、不 steer 进本轮的消息
    /// 无法塞回 mpsc 队首,故经它原序回填队尾。poll_* 整批抽干后由 `plan_steer` 原序回填——
    /// **顺序恒等(不轮转,T2)**。(`!` 强制打断那条会打乱顺序的路 2026-08-20 已删。)
    requeue: Sender<ThreadMsg>,
    /// claude/Transcript 路的**轮内 steer** 开关(messaging.steer_transcript,**默认开**):
    /// 轮内到达的普通消息直接注入 claude 队列,不再由 summon 自己排队等下一轮。
    steer_transcript: bool,
    /// RPC(codex app-server)路的**轮内 steer** 开关(messaging.steer_rpc,**默认开**):
    /// 开了之后轮内到达的普通消息经 `turn/steer` 追加进正在跑的那一轮,不再排队等下一轮。
    /// 与 `steer_transcript` 分开:注入机制毫无共同点(打字 vs 发帧),要能各自灰度。
    /// cursor 走同一条 RPC 路但 ACP 无对应能力,开了对它是空操作(消息照常退回队列)。
    steer_rpc: bool,
    /// Cursor Headless CLI Session 的中断+resume steer。默认开；只对 Cursor + 独占 CLI
    /// 子进程生效，显式切到 ACP(`capture: rpc`)后不借这条语义冒充原生 steer。
    steer_cli_session: bool,
    /// followup 正文进入任何 steer/重跑副作用前必须先在这里 durable 标记 started。
    wal: crate::runtime::inbox_wal::InboxWal,
    agent_name: String,
    delivery_scope: String,
    /// steer/`!` 只能修改同一 requester 发起的当前 turn；其他人的消息排队成独立 turn。
    requester_open_id: String,
    /// 入站时解析的 union_id，供未送达重投复验同一发起人。
    requester_union_id: String,
    session_end_evidence: Option<&'a SessionEndEvidence>,
    /// 当前这一轮是 `/handoff` 写交接的那一轮:轮内到达的消息一律不并进来,排到交接之后再跑。
    /// 交接只看这一轮自己的回复,并进来的消息会让回复变成对它的回答。
    handoff_turn: bool,
    /// 轮内 steer / `!` 重跑的**完整入站组装**(引用回文、附件路径、发件人信封、共 @ 提示)。
    ///
    /// 2026-08-07 回归:此前轮内路径只取裸文本,于是追加消息在 agent 眼里既没有来源也没有引用
    /// (owner 实测「带引用发的消息,attach 里看不到引用信息」)。修复方式是让它跟 `handle_task`
    /// 共用同一份组装。
    ///
    /// 做成闭包而不是把 `&Agent`/`&FeishuClient` 塞进本结构:那会把网络与注册表依赖拖进
    /// FollowupCtx 的生命周期,单测再想造一个就得连带造 client。测试传恒等闭包即可。
    compose: &'a dyn Fn(&ChatTask, &str) -> String,
}

/// RPC handoff detach 的可取消租约。dispatcher 超时后把 Active→Canceled；chat worker 只有先把
/// Active→Claimed 才能退出。这样一个迟到的 mpsc 控制消息不会在 handler 已报失败后悄悄拆掉源路由。
pub(crate) struct RpcHandoffDetachLease {
    state: std::sync::atomic::AtomicU8,
}

impl RpcHandoffDetachLease {
    const ACTIVE: u8 = 0;
    const CLAIMED: u8 = 1;
    const CANCELED: u8 = 2;

    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::atomic::AtomicU8::new(Self::ACTIVE),
        })
    }

    pub(crate) fn claim(&self) -> bool {
        self.state
            .compare_exchange(
                Self::ACTIVE,
                Self::CLAIMED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    /// 返回 true 表示成功取消；false 表示 worker 已经 claim，调用方应继续等它的即时 ACK。
    pub(crate) fn cancel(&self) -> bool {
        self.state
            .compare_exchange(
                Self::ACTIVE,
                Self::CANCELED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}

/// 等 claude 认了才写的模型切换:切到哪个模型,认了之后写到哪一层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelConfirm {
    pub model: String,
    pub write: ModelWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelWrite {
    /// bot 默认(agent.yaml 顶层 `model`)。
    Default,
    /// 本群覆盖(值是 chat tag)。
    Chat(String),
    /// 清掉本群覆盖:切回的是默认模型,认了之后删掉本群文件。
    ChatInherit(String),
}

impl ModelConfirm {
    /// claude 认了之后落盘。
    pub(crate) fn write(&self, agent: &Agent) -> anyhow::Result<()> {
        match &self.write {
            ModelWrite::Default => {
                crate::setup::bind::set_model(&agent.manifest_path(), &self.model)
            }
            ModelWrite::Chat(tag) => {
                crate::runtime::chat_model::set_chat_model(agent, tag, Some(&self.model))
            }
            ModelWrite::ChatInherit(tag) => {
                crate::runtime::chat_model::set_chat_model(agent, tag, None)
            }
        }
    }

    /// 回执里说「哪份配置」:改默认是 agent.yaml,改本群是本群模型。
    pub(crate) fn config_name(&self) -> &'static str {
        match self.write {
            ModelWrite::Default => "agent.yaml",
            ModelWrite::Chat(_) | ModelWrite::ChatInherit(_) => summon_i18n::text!("rb8f8f4a4e1"),
        }
    }
}

pub(crate) enum ThreadMsg {
    Task(Box<ChatTask>), // Box:ChatTask 带 Messaging 快照较大,避免撑大枚举
    /// 生命周期动作。`card_message_id` = 触发该动作的会话面板卡片(收尾后 PATCH 成「已退出」
    /// 终态:保留面板原貌、控件置灰 + footer);非卡片触发(配置变更快重置)为 None。
    /// `models` = 当时生效的模型候选(重渲染置灰面板时复现模型按钮)。
    Control {
        ctrl: Control,
        card_message_id: Option<String>,
        models: Vec<String>,
        /// 被点卡片来源(空闲卡 vs /session 面板)——收尾时据它选对应布局的终态渲染,
        /// 避免空闲卡被错画成面板(布局漂移 bug)。
        origin: CardOrigin,
        /// 这次拆除是不是**用户主动的退出动作**(要发退出回执)。
        ///
        /// 此前是靠 `card_message_id.is_some()` 隐式推断的:有卡片 id = 面板点的 = 退出。该推断在
        /// 加入 `/session reset` 文字指令后直接失效(文字指令没有卡片 id,却是不折不扣的退出),
        /// 所以改成由各发起点显式声明。
        notify_exit: bool,
        /// 这次 `Clean` 是**空闲扫描自动发起**的(而非用户点面板/发 `/session clean`)。
        ///
        /// 只影响退出回执的原因与文案(`IdleRetired` vs `PanelClean`)——拆除行为逐字节相同,
        /// 都是「留 resume 记录、只释放 tmux」。做成 bool 而不是让发起点直接传 `ExitReason`:
        /// 后者允许构造出 `ctrl=Reset` 配 `reason=PanelClean` 这种自相矛盾的组合,回执会把
        /// 「已重置」说成「续聊」。bool 只能在两个同语义的 reason 之间选,填错也不会说谎。
        /// `Reset` 臂忽略它:自动退出永远是归档续聊,不会走重置。
        idle_retired: bool,
        /// mid-turn 收到 exact `/quit`/`/exit` 时，把原 Task 连同 durable WAL 身份一起提升为
        /// teardown。普通面板/文本控制为 None；Some 只由 fresh dispatcher admission 产生。
        source_task: Option<Box<ChatTask>>,
        /// `source_task` 是否已由 mid-turn poll durable 标成 started。false 时主循环必须先 claim
        /// 再拆；true 时不得重复 claim，只在拆除完成后销账。
        source_started: bool,
    },
    /// compact 卡片动作:先贴 working 表情、跑 `/compact` 等它真结束、再回执已压缩。
    /// 带被点卡片 message_id(贴表情用)+ 当时 messaging 快照(working/done 表情)。
    Compact {
        message_id: String,
        messaging: Box<Messaging>,
        /// 被点卡片来源:Idle 时 compact 成功后把空闲卡定格为终态(而非另发回执)。
        origin: CardOrigin,
    },
    /// 外部事件(群解散 / bot 被移除)驱动的拆除:线程 do_teardown(Clean=留 .session)后经
    /// ChatExited 带 `reason` 收口发 p2p 退出回执。与面板 Control 区分:reason 是事件原因、
    /// 通知方向恒 p2p 群主。`event_operator` = 事件操作者(群主回退源之一)。
    GoneTeardown {
        reason: summon_core::exit::ExitReason,
        event_operator: String,
        peers: crate::runtime::peers_store::PeersStore,
        /// chat 线程完成本地 session/persisted-state/peers 回收后回 ACK。dispatcher
        /// 只在收到 ACK 时相信在线路已收口；线程被其他 teardown 抢先退出时
        /// sender 会断开，调用方立即转离线幂等清理。
        ack: Sender<()>,
    },
    /// `summon agents kill` 驱动的拆除(经 control.sock→Core→dispatcher 投来)。线程**先打断本
    /// chat 在跑的无头一轮**(查 InFlight 注册表 `killer.interrupt()`,让子进程组随 stdout EOF 退),
    /// **再** do_teardown(archive=true,reset 由 mode 决定),最后经 ChatExited 摘 handle + 发回执。
    /// 与 GoneTeardown 区分:reason=ManualKill、通知方向按群在/不在走默认链(非恒 p2p)。
    KillTeardown {
        reset: bool,
    },
    /// F3-b 外杀(`agents kill`/`down` 对 claude 直接 `Tmux::kill`,对飞书静默)被 idle 扫描探测到后,
    /// dispatcher 投这条让**残留的 chat 线程静默退出**(PF1)。chat 线程持自己 channel 的 sender clone,
    /// `rx.recv()` 永远不会因 dispatcher 摘 handle 而 Disconnected,不投这条则线程连同 MirrorGuard
    /// (300ms 轮询转录的 mirror watcher)每外杀一次就永久泄漏两个。本条只 `return Exit` 收掉 MirrorGuard
    /// 结束线程:**不** do_teardown(tmux 已被外部杀)、**不**发 ChatExited(run_idle_scan 已合成回执)。
    SilentExit,
    /// Bridge 把 summon-owned RPC session 搬到另一个群前，要求源 chat worker 在两轮之间退出。
    /// 不 teardown、不清盘、不碰 RPC 进程；可取消 lease 防超时后迟到退出。
    RpcHandoffDetach {
        lease: Arc<RpcHandoffDetachLease>,
        ack: Sender<bool>,
    },
    /// 任务4:控制卡「⏸ 打断」按钮 → dispatcher 投这条到本 chat 线程。turn 进行中由 poll_*_interrupt
    /// 抽到 → 纯停当前轮(无头 SIGINT / claude Esc)、定格过程卡为「已暂停」、贴 interrupted 表情;
    /// 不在跑(队列里)时被当普通控制消息消费掉(无副作用)。
    Interrupt,
    /// 面板改 model/effort 后的**就地生效**(不拆会话、不丢上下文;`CliDriver::model_switch_spec`
    /// 分流出的 TmuxSlash/NextTurn 走这条):`slash`=Some 先往 tmux 注入(claude `/model <m>`),
    /// None 则无需注入(无头下一轮 spawn 自带新 flag);随后线程**就地重读** agent.yaml 重建
    /// Session(外层 loop),同一 rx 继续——队列里排在后面的消息不丢、tmux/`.session` 全不动。
    /// `receipt` 在切换动作真正完成后才发(不提前谎报)。`anchor` 是回执要挂进的 thread 锚点
    /// (文本指令触发时是那条指令消息;面板按钮触发时为 None,回执照旧发群主干);纯确认在
    /// 有锚点时贴完成表情代替(见 [`CommandAck`])。
    /// `confirm_model`=Some 时这个模型还没写进配置:线程注入 `slash` 后从转录读 claude 的
    /// 回应,认了才按 [`ModelWrite`] 写盘、发 `receipt`,不认或读不到就不写，如实回执。
    SwitchApply {
        slash: Option<String>,
        receipt: CommandAck,
        anchor: Option<String>,
        confirm_model: Option<ModelConfirm>,
    },
    /// agent.yaml 改了(飞书身份没变):线程在两轮之间就地换用新 agent,会话镜像与子代理观测原样
    /// 继续;只有它们依赖的配置变了才走 Reload 重建(见 `chat_thread::plan_agent_reload`)。在跑的
    /// 轮不受影响,队列不丢,不发回执。model 变了的 claude 会话注入 `/model` 就地切换
    /// (见 `chat_thread::live_model_switch`)。
    AgentReload,
    /// Session 卡切换 CoT 展示：dispatcher 已原子落盘，本消息只让活跃 worker 的内存覆盖同步到
    /// 同一终态；未启动/已退出的 worker 由下次启动读 `.cot` 自然恢复。
    CotApply {
        visible: Option<bool>,
        thinking: Option<bool>,
        tools: Option<bool>,
    },
    /// `/introduce` 后给持久 Transcript 会话的上下文热更新。必须由 chat 线程在两轮之间串行注入，
    /// 不能由 dispatcher mid-turn 直写 tmux；无头/未冷启会话靠下次 PRIMER 从 peers 文件读取。
    PeersNotice {
        notice: String,
    },
    /// 面板卡刚被创建/重渲 → 把**此刻**的 subagent 快照补渲到这张卡上,**一次**。
    ///
    /// 投递方 = 每一条会产出或重画面板卡的路径(dispatcher 的 `/session`、`/clear` 确认卡、
    /// 改 model/effort 后的 `refresh_config_panel`);它们渲的骨架不含 subagent 区块
    /// (dispatcher/卡片线程摸不到 chat 线程的 `Arc<Mutex<SubagentState>>`),故由 chat 线程
    /// 收到本条后现读共享状态补一次 PATCH。
    ///
    /// 补渲完还要**把这张卡登记为「群里最新那张」**:owner 2026-08-14 收敛出的判据是
    /// **哪张卡最新**,不是卡的类型 —— 面板卡恰好是最新那张时它就该跟着更新,等下一张 turn 卡
    /// 定格,登记被覆盖,刷新目标自然移过去。退役掉的是「面板卡**无条件**持续 PATCH」(不管还
    /// 是不是焦点都刷,吵),不是「面板卡永远不更新」。
    ///
    /// 驱动方式也因此不同:watcher **不持有** chat 线程的 Sender,没有「状态变化 → 立刻 PATCH」
    /// 的推送通路;刷新由 chat 线程自己的 tick 拉动(见 `SUBAGENT_CARD_REFRESH_TICK`)。
    RenderSubagentPanelSnapshot {
        card_id: String,
    },
    /// Session 卡撤回成功后清除子代理刷新目标，避免 tick 继续 PATCH 已不存在的消息。
    ForgetSessionCard {
        card_id: String,
    },
}

/// 定时触发那一轮的正文开头,后面接定时任务自己的 prompt。
pub(super) const SCHEDULED_PROMPT_PREFIX: &str = summon_i18n::source!("r0964cde26d");

/// 控制台「打断」的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DashInterrupt {
    /// 已交给 chat 线程。
    Sent,
    /// 页面看到的那一轮已经结束(凭证对不上),没有动手。
    Stale,
    /// 这个会话此刻没有在跑的线程。
    NotRunning,
}

/// chat worker 收到的载荷类型。消息是否套发件人信封、是否能走 CLI 命令通道都由这个枚举
/// 集中决定，不能再复用一个 `raw` 布尔值同时表达「定时器不套信封」和「斜杠命令」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatTaskKind {
    /// 真人/bot 发来的普通对话，渲染来源信封、引用与附件。
    Conversation,
    /// owner 预设的定时 prompt：不套来源信封，但仍是一轮完整 agent turn，必须流式回传答案。
    ScheduledPrompt,
    /// owner 预设的定时 prompt：完整执行并记录，但只向会话交付终态结果。
    ScheduledResult,
    /// provider 自己的 prompt 语法，不是交互式 CLI 命令。当前仅 Codex 把 `/skill` 映射成
    /// `$skill`，仍走完整 turn/current-turn/tool guard；不套聊天信封，避免破坏 provider 语法。
    ProviderPrompt,
    /// provider 命令候选；worker 只放行 operate-gate 之后、且**不在 slash 黑名单**里的形态
    /// (`driver::slash_pool`，owner 2026-08-14 起准入是可配置黑名单而非采样池白名单)。
    /// 池内命令走各自调优过的收尾判据，未知命令走按观测归类的通用执行器；
    /// 生命周期命令及其同形变体走受控映射 / 明确拒绝，永不裸注入。
    SlashCommand,
}

impl ChatTask {
    /// 本轮可以拿去调飞书的那个 `message_id`(贴表情、回复原消息、建原生 CoT 的 origin)。
    ///
    /// **`None` = 这一轮没有可锚的飞书消息**,调用方必须跳过那次调用而不是拿 `message_id`
    /// 顶上:合成 id 会被飞书判成 `99992354 not a valid open_message_id`,整条回执链路
    /// (表情 / 总结卡 / 原生 CoT)一起 400,而 CoT 那条会静默退回卡片形态 —— 用户看到的是
    /// 「配了原生 CoT 却出来一张思考卡片」。
    ///
    /// 三种来源收在这一处,不散在各调用点:恢复重放带真实原 id;定时 / sentinel 的合成 id
    /// 没有对应消息;其余真实入站就是它自己。
    pub(crate) fn feishu_anchor(&self) -> Option<&str> {
        if let Some(anchor) = self.anchor_message_id.as_deref() {
            return (!anchor.is_empty()).then_some(anchor);
        }
        if self.kind.is_scheduled() || self.hide_context_hint {
            return None;
        }
        (!self.message_id.is_empty()).then_some(self.message_id.as_str())
    }
}

impl ChatTaskKind {
    fn is_slash(self) -> bool {
        self == Self::SlashCommand
    }

    /// 飞书原始文本采用 provider slash 语法。`ProviderPrompt` 虽然不是可直接操作 TUI 的
    /// `SlashCommand`，群聊里仍必须要求开头 exact @ 本 bot；否则 Codex 的 `/skill` 会让同群
    /// 每个 bot 都各跑一轮。
    fn uses_provider_slash_syntax(self) -> bool {
        matches!(self, Self::ProviderPrompt | Self::SlashCommand)
    }

    fn streams_turn(self) -> bool {
        matches!(
            self,
            Self::Conversation
                | Self::ScheduledPrompt
                | Self::ScheduledResult
                | Self::ProviderPrompt
        )
    }

    fn is_scheduled(self) -> bool {
        matches!(self, Self::ScheduledPrompt | Self::ScheduledResult)
    }

    fn accepts_inbound_context(self) -> bool {
        self == Self::Conversation
    }
}

/// 一条待 chat 线程处理的任务。dispatcher 已解析 who、判定群/私聊,
/// 并附上当时生效的 messaging 快照(承载 prompt 模板/表情集/system_prompt/working/done,支持热重载)。
/// 群名留给 chat 线程解析(避免在 dispatcher 扇出路径上阻塞别的 chat)。
pub(crate) struct ChatTask {
    text: String,
    message_id: String,
    /// 这条消息在飞书上的真实 message_id(见 [`Inbound::anchor_message_id`])。
    /// `None` = `message_id` 本身就是真的,或本轮压根没有对应的飞书消息(定时 / sentinel)。
    /// **别直接读它**,用 [`ChatTask::feishu_anchor`]。
    anchor_message_id: Option<String>,
    who: String,
    /// 发送人 open_id(日志同时展示名字+id,多用户场景可溯源)。
    who_id: String,
    /// 发送人 union_id(入站事件自带;事件没带则为空)。**与 `who_id` 成对使用** —— 执行权限
    /// 判定走 `can_operate`,名单里可能只配了 union_id,少了它 owner 会被判成外部人。
    who_union_id: String,
    /// 发送人是不是本 chat 已 `/introduce` 的成员(bot 或同事)。peer 经 operate-gate 显式认下,
    /// 执行权限上视为内部;不带这个标记它会被当外部人整体降只读,认识关系即作废。
    sender_is_peer: bool,
    is_group: bool,
    kind: ChatTaskKind,
    /// dispatcher 用当时最新 Agent 快照冻结的完整 provider slash 准入事实：operate 权限、
    /// Transcript capture 与 driver exact command 缺一不可。旧 chat worker 只能二次收紧，
    /// 绝不能拿 spawn 时旧权限/旧 capture 把 fresh dispatcher 已拒绝的命令重新放行。
    slash_admitted: bool,
    /// 本 bot 是否被直接 @(self-mention 在消息开头且被剥离),或这是经一人一 Bot 验证放行的
    /// 免 @ 普通对话。
    /// true = 用户在对本 bot 说话(应响应);false = 本 bot 只是被 CC 或没被 @(自行判断是否响应)。
    addressed: bool,
    /// 发信人是不是另一个 bot(sidecar 透传的 `sender_type == "app"`)。
    /// handle_task 据此在 prompt 信封里标注发送方身份 —— agent 要判断「要不要应答」,
    /// 先得分得清对面是人还是 bot。
    sender_is_bot: bool,
    /// 被引用消息 message_id(引用回复时有);handle_task 拉回原文、当「引用上下文」前置进 prompt。
    /// SlashCommand 不注入引用。
    parent_id: Option<String>,
    /// 飞书话题定位；Task promote 会把它们持久化到 source_ref，终态回执据此回到原上下文。
    thread_id: Option<String>,
    root_id: Option<String>,
    /// 随消息发来的附件(图片/文件等);handle_task 下载到本地后把路径附到 prompt。
    resources: Vec<Resource>,
    /// 本条消息同时 @ 的其他 bot 名字(不含自己)。handle_task 据此在 prompt 末尾追加
    /// 一条提示,帮 agent 判断自己是不是主要收件人、是否该静默。空则无提示。
    co_mentioned: Vec<String>,
    /// 当时生效的消息层配置快照。
    /// 本轮的正文是 **summon 内部构造的 prompt**,不是用户原话——总结卡底注「关于:X」必须留空。
    ///
    /// sentinel 召唤(拉进群 / 建三方群)的 prompt 由 `compose_join_prompt` / `compose_create_prompt`
    /// 拼出,开头就是「🛰 owner 刚把你拉进群…以下是 sentinel 代拉的最近群聊上下文」。底注取正文
    /// 前 60 字,于是这段内部实现被原样贴进群里(2026-08-12 owner 实测)。建群那条更糟:原话出自
    /// owner 与同事的**单聊**,显示在新建的三方群里等于把单聊内容搬了过去。
    ///
    /// 用显式字段而不是「按 message_id 前缀猜」:前缀是技术条件,漏一条新路径就再泄一次;
    /// 加字段则由编译器把所有构造点列出来,漏不掉。
    hide_context_hint: bool,
    /// 定时触发要先发的**预告卡**文案(非定时任务为 `None`)。
    ///
    /// **必须由 chat 线程在 inbox WAL 取得 start 之后再发,不能在 dispatcher 收到 fire 时发。**
    /// 2026-08-12 真机:devbox 上一条 08-11 14:00 的 fire 卡在 WAL 里执行不掉,每次 daemon 重启
    /// 都被「启动恢复」重注入一次,而预告卡当时发在 dispatcher 侧 —— 于是每重启一次就往群里发
    /// 一张「⏰ 定时任务触发」,内容是昨天 14:00 的任务、发送时刻却是今天傍晚,owner 一眼看出
    /// 时间对不上。那一轮日志紧跟着就是「inbox WAL 未取得新 start(AlreadyStarted),本轮未执行」
    /// —— 卡发了,任务根本没跑。
    ///
    /// 判据要回到目的:预告说的是「这条定时任务**要开始跑了**」,那就得等到确认它真的会跑。
    /// 顺带把 dispatcher 从主动出站里摘出去 —— 它不该直接往群里发东西。
    schedule_notice: Option<String>,
    /// 定时任务要求从空上下文起跑(`ScheduleContext::Fresh`):chat 线程在本轮起跑前清 resume
    /// 记录、拆掉旧会话,再把本轮交还 dispatcher 冷启重投。到达时本 chat 正有一轮在跑的,
    /// dispatcher 投递时 / chat 线程回填时清掉这个标记,该轮沿用现有上下文。
    fresh_context: bool,
    /// 用户用 `/new <内容>` 明确要求的新会话:不因排队消息、活跃子代理或在跑的一轮而沿用旧上下文,
    /// 也不被 steer 并进当前轮。
    fresh_requested: bool,
    /// 用户用 `/queue <内容>` 指定排队的一条:不 steer 进正在跑的那一轮,排在它后面单独跑。
    queued: bool,
    /// 获准的 `/handoff`:本轮是写交接的那一轮(正文已改写成指令)。chat 线程起跑前校验目标,
    /// 轮末按回复里的交接标签交给目标 agent 或本 agent 的新会话;它不被 steer 并进别的轮。
    handoff: Option<summon_core::handoff::HandoffCommand>,
    /// 这一轮是 daemon 换代后的接管(见 [`Inbound::reattach`]):chat 线程读盘上锚点、以零注入
    /// 形态接住上一代那一轮的输出。锚点证不了就退人工恢复卡,绝不当新轮重注入。
    reattach: bool,
    messaging: Messaging,
}

/// 定时任务到点时本 chat 正有一轮在跑 → 该轮沿用现有上下文(不拆别人正在跑的轮);其余照定时器配置。
pub(super) fn schedule_context_at_fire(
    context: summon_core::ScheduleContext,
    turn_active: bool,
) -> summon_core::ScheduleContext {
    if turn_active {
        summon_core::ScheduleContext::Resume
    } else {
        context
    }
}

/// 合成一条「定时触发」的 chat Task:唯一 message_id(防入站去重)、⏰ 前缀提示，作为完整
/// `ScheduledPrompt` 走正常 turn/stream；它只是不套「来自 X」信封，绝不是 CLI 命令。
fn scheduled_chat_task(
    fire_id: String,
    prompt: &str,
    is_group: bool,
    messaging: Messaging,
    output: summon_core::ScheduleOutput,
    context: summon_core::ScheduleContext,
    // 本会话是否注入了表情约定。关掉时下面那段「不要 react」整段不发 —— 它存在的唯一理由是
    // 与 PRIMER 里「每一次回复都必须 react」消歧义,PRIMER 没说,它就成了凭空引入一个
    // agent 从未被告知的命令。
    reactions_injected: bool,
) -> ChatTask {
    let output_hint = match output {
        summon_core::ScheduleOutput::Progress => String::new(),
        summon_core::ScheduleOutput::Result => summon_i18n::text!("r336d7ad5b2").to_string(),
    };
    ChatTask {
        // 预告卡文案随 task 一起交给 chat 线程,由它在 WAL 取得 start 后再发 —— 见字段注释。
        schedule_notice: (output == summon_core::ScheduleOutput::Progress)
            .then(|| scheduled_fire_notice(prompt)),
        fresh_context: context == summon_core::ScheduleContext::Fresh,
        fresh_requested: false,
        queued: false,
        handoff: None,
        sender_is_bot: false,
        // 这句约束是必要的:PRIMER 教了 agent「每一次回复都必须 summon react」,而定时触发
        // **没有可回应的真实飞书消息**(message_id 是合成的 fire_id),react 必然失败。不说清楚,
        // agent 就会照做、失败、然后在回复里向用户解释一段与任务无关的工具错误。
        text: format!(
            "{SCHEDULED_PROMPT_PREFIX}{prompt}{}{output_hint}",
            if reactions_injected {
                summon_i18n::text!("r930cbbcad8")
            } else {
                ""
            }
        ),
        message_id: fire_id,
        // 合成的 fire_id 在飞书上没有对应消息 —— 表情/回复/CoT 都不能拿它去调。
        anchor_message_id: None,
        who: summon_i18n::text!("rf9a58325cf").to_string(),
        // 定时触发没有发送人:两条 id 都空 → 执行闸判「身份缺失」= 全权(owner 预设的任务)。
        who_id: String::new(),
        who_union_id: String::new(),
        sender_is_peer: false,
        is_group,
        kind: match output {
            summon_core::ScheduleOutput::Progress => ChatTaskKind::ScheduledPrompt,
            summon_core::ScheduleOutput::Result => ChatTaskKind::ScheduledResult,
        },
        slash_admitted: false,
        addressed: true,
        parent_id: None,
        thread_id: None,
        root_id: None,
        resources: Vec::new(),
        co_mentioned: Vec::new(),
        hide_context_hint: false,
        reattach: false,
        messaging,
    }
}

/// 合成一条「sentinel 注入」的 chat Task(sentinel 召唤、`/handoff` 的接手消息、上下文提醒卡的
/// Handoff)。与定时触发的两点关键差异:
/// ① 发送人是真实 owner(who 三元组),operate 权限与真人一致;② `Conversation` 走完整信封
/// (「在群 X 里 who @你」)+ Transcript 冷启自动前置 PRIMER——bot 刚被拉进新群,这正是
/// 它的第一轮,记忆索引/peers 必须到位。message_id 是真实触发消息 id,react/回复可用。
#[allow(clippy::too_many_arguments)]
fn sentinel_chat_task(
    fire_id: String,
    prompt: &str,
    // `Some` = 上下文提醒卡发起的写交接轮,值是接手重点。
    handoff_note: Option<String>,
    is_group: bool,
    who: String,
    who_id: String,
    who_union_id: String,
    messaging: Messaging,
    // 同 `scheduled_chat_task`:PRIMER 没教 react,这段消歧义就不该发。
    reactions_injected: bool,
) -> ChatTask {
    ChatTask {
        // sentinel 召唤不是定时任务,没有预告卡。
        schedule_notice: None,
        fresh_context: false,
        fresh_requested: false,
        // 委派任务需要独立的接收轮，消息键在 WAL 恢复时仍保留此排队语义。
        queued: fire_id.starts_with("delegate-"),
        handoff: handoff_note
            .map(|note| summon_core::handoff::HandoffCommand { target: None, note }),
        sender_is_bot: false,
        // 与 `scheduled_chat_task` 同源的约束:PRIMER 教了 agent「每一次回复都必须 summon react」,
        // 而 sentinel 召唤的 message_id 是合成 fire_id,没有可回应的真实飞书消息,react 必然报缺
        // SUMMON_CHAT_ID。不说清楚,agent 就会照做、失败、然后在回复末尾向群里解释一段与任务
        // 无关的工具错误(2026-08-12 owner 实测)。
        text: format!(
            "{prompt}{}",
            if reactions_injected {
                summon_i18n::text!("r617d121a14")
            } else {
                ""
            }
        ),
        message_id: fire_id,
        // 同定时:合成 fire_id 在飞书上没有对应消息。
        anchor_message_id: None,
        who,
        who_id,
        who_union_id,
        sender_is_peer: false,
        is_group,
        kind: ChatTaskKind::Conversation,
        slash_admitted: false,
        addressed: true,
        parent_id: None,
        thread_id: None,
        root_id: None,
        resources: Vec::new(),
        co_mentioned: Vec::new(),
        // sentinel 的 prompt 是内部构造的(见字段注释),底注必须留空。
        hide_context_hint: true,
        reattach: false,
        messaging,
    }
}

/// dispatcher 持有的每个活跃 chat 的句柄。
struct ChatHandle {
    tx: Sender<ThreadMsg>,
    last_activity: SystemTime,
    idle_prompted: bool,
    /// chat_type 字符串(p2p/group/topic_group),清理总结 frontmatter 用。
    chat_type: String,
    /// 该 chat 的 tmux 会话名(Transcript 才有真进程);idle-scan 据它探测 `agents kill` 外带拆除。
    tmux_name: String,
    /// idle-scan 曾观测到该 tmux 会话存在过——只有「曾活后又消失」才合成 ManualKill 退出回执,
    /// 避免从未冷启(无头/未发 Task)的会话被误判退出。**只观测、绝不杀**(合规 CLAUDE.md)。
    tmux_seen_live: bool,
    /// 是否已排入会创建/恢复本地 CLI 上下文的工作。纯 `/session` 空 handle 保持 false；
    /// Task/蒸馏/定时续跑在入队前置 true，覆盖 SID/WAL 尚未落盘的占用空窗。
    local_claimed: bool,
    /// 最后一次投给本 chat 的入站是否为**已确认开跑的定时器合成轮**。
    /// 静默回收(`timer_idle_retire_secs`)的来源判据。**置 true 的唯一路径**是
    /// [`ChatHandle::confirm_scheduled_admission`]:worker 对该 fire 真正取得 durable start
    /// (`MarkStartedOutcome::Marked`)后回报 `DispatchMsg::ScheduledAdmitted`,且该 fire_id
    /// 仍等于 [`Self::last_inbound_id`](exact id 代际围栏)。投递点绝不置 true —— 投递 ≠ 执行,
    /// 迟到的重复 fire / WAL 重投会被判 `AlreadyStarted` 跳过,不得把真人尾轮改判回定时尾轮。
    /// 人类/bot 消息、sentinel、卡片保活经 [`ChatHandle::note_activity`] 把它清回 false
    /// (回到普通空闲规则)。纯内存态:daemon 重启后为 false,该 chat 回退普通规则
    /// (到点发提醒卡)——方向是「多打扰一次」,不是「拆错一个」。
    last_inbound_scheduled: bool,
    /// 本 chat 最新一次入站投递的 **exact message_id/fire_id**(真人消息=飞书 message_id、
    /// 定时器=fire_id、sentinel=触发消息 id、卡片保活=卡片 message_id)。
    /// 定时尾轮判定的代际围栏:`ScheduledAdmitted` 只有命中它才生效 —— admission 在途期间
    /// 一旦有更新的入站(真人插话),迟到的确认自动作废。空串 = 本 handle 尚无入站,
    /// 任何 admission 都不命中(fire_id 非空,WAL 对空 id 也不会产出 `Marked`)。
    last_inbound_id: String,
    /// 上一轮空闲扫描时该 chat 是否有活跃 turn(盘上 current-turn 记录在否)。扫描间跨态存储,
    /// 用于检测「在跑 → 刚结束」翻转:长 turn(>idle_ttl)结束的那一刻,入站锚点早已过点,
    /// 不重置的话提醒/回收会在 turn 结束后立刻补发(owner 2026-08-19 实测:12:58 回复、13:03
    /// 即提醒 55min)。翻转时把空闲时钟拨到 turn 结束时刻;定时尾轮(`last_inbound_scheduled`)
    /// 不在此重置,保持「从 timer fire 起算 55min 静默回收」的原行为。
    turn_was_active: bool,
    /// 本 chat 的 subagent 观测状态:与 chat 线程 / subagent watcher 共享**同一个**
    /// `Arc<Mutex<SubagentState>>`(生产写入方只有 watcher,且仅 Transcript 会话有观测;
    /// 无头会话的 Arc 恒为空状态)。dispatcher 侧只读,供空闲判定消费:有活跃 subagent/
    /// workflow 的会话不发提醒卡、不自动回收——拆 tmux 会连 claude 进程一起杀,后台
    /// subagent/workflow 的在途工作全部丢失;subagent 的最后活动同时推进空闲时钟。
    /// None = 观测不可得(测试直造的 handle 等)→ 空闲判定回退「无 subagent」的现行为。
    subagent_state: Option<SharedSubagentState>,
    /// 本 chat 的菜单账本,与 chat 线程的会话共享同一个 `Arc`。卡片上的单选在 dispatcher 线程注入,
    /// 注入成功在这里记一笔「答过」,chat 线程的轮外守望据此把同一个问题再弹出当成新问题。
    /// None = 测试直造的 handle。
    menu_ledger: Option<std::sync::Arc<summon_core::interactive::MenuLedger>>,
}

impl ChatHandle {
    /// 入站活动的**统一收口**(终审阻塞③,2026-08-13):推进空闲时钟 + 清空闲卡去重
    /// (内存位与 durable marker 一起)+ 登记来源代际,三件事必须一起发生。散装拼装漏掉
    /// 任何一半都会造出悬空态 —— 例:孤儿重投只推时钟不清去重位,spawn 从账本恢复出的
    /// `idle_prompted=true` 会一路残留到 timer 静默托管撤销之后,挡住兜底提醒卡,会话
    /// 「既不回收也不发卡」。所有入站投递点(真人/bot 消息、replacement 重建、孤儿重投、
    /// ScheduledFire、SentinelFire、卡片保活)一律走这里,不许直接拨弄字段
    /// (tests.rs 源码守卫钉死 dispatcher/cards 无旁路)。
    ///
    /// `source=None` 仅孤儿重投命中**已有** handle 时用:已有来源写于旧 worker 死后,
    /// 必然比死前入队的孤儿新,不得回拨;时钟与去重照常推进(重投仍是真实活动)。
    fn note_activity(&mut self, agent: &Agent, chat_id: &str, source: Option<&str>) {
        self.last_activity = SystemTime::now();
        self.idle_prompted = false;
        // durable marker 一并作废:新活动开启新一段空闲期,盘上「已提醒」凭证过期。
        // 删除失败方向安全(重启后误判「已提醒」→ 漏一张卡,不会重发),best-effort。
        chat_meta::clear_idle_prompt_marker(agent, &self.chat_type, chat_id);
        if let Some(id) = source {
            self.note_inbound_source(id);
        }
    }

    /// 登记一次入站投递的来源代际:记下 exact id,并把「定时尾轮」判定回退到未确认(false)。
    /// **所有**入站投递点统一经 [`Self::note_activity`] 间接走这里 —— 定时尾轮要等 worker 的
    /// `ScheduledAdmitted` 命中该 id 才成立。
    fn note_inbound_source(&mut self, id: &str) {
        id.clone_into(&mut self.last_inbound_id);
        self.last_inbound_scheduled = false;
    }

    /// 消费 worker 的 `ScheduledAdmitted`:仅当该 fire 仍是本 chat 的最新入站(exact id 相等)
    /// 才把会话判成定时尾轮;否则说明 admission 在途期间已有更新的入站(真人插话/新 fire),
    /// 迟到的确认作废,保持普通空闲规则。返回是否生效(日志用)。
    fn confirm_scheduled_admission(&mut self, fire_id: &str) -> bool {
        let current = self.last_inbound_id == fire_id;
        if current {
            self.last_inbound_scheduled = true;
        }
        current
    }

    /// 读本 chat 的 subagent 活跃探针。观测不可得(无句柄 / 锁正被占)→ None,调用方按
    /// 「无 subagent」回退现行为(允许提醒/回收)——owner 定的方向:别让一个卡死的
    /// watcher 把会话变成永不回收。
    ///
    /// **try_lock 而非 lock**:空闲扫描跑在 dispatcher 主循环线程,watcher 若卡死在持锁段,
    /// 阻塞式 lock 会把整个 agent 的消息路由一起挂住。偶发争用(watcher 正批量消费转录)
    /// 按「本轮观测不可得」回退;真正危险的消费点(自动回收)在 chat 线程执行 `Clean` 时
    /// 还有第二重确认(阻塞锁,见 chat_thread 的 idle_retired 分支)兜住这条竞态。
    /// 锁 poison(watcher panic)恢复内层值继续:状态是纯观测副作用,读到什么算什么。
    fn subagent_probe(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<summon_core::subagent::ActivityProbe> {
        let state = self.subagent_state.as_ref()?;
        match state.try_lock() {
            Ok(st) => Some(st.activity_probe(now)),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner().activity_probe(now)),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}

#[cfg(test)]
mod anchor_tests {
    use super::*;

    fn task(message_id: &str, anchor: Option<&str>, kind: ChatTaskKind) -> ChatTask {
        let mut t = scheduled_chat_task(
            message_id.to_string(),
            "x",
            true,
            summon_core::config::builtin_messaging(),
            summon_core::ScheduleOutput::Progress,
            summon_core::ScheduleContext::Resume,
            false,
        );
        t.kind = kind;
        t.anchor_message_id = anchor.map(str::to_string);
        t.hide_context_hint = false;
        t
    }

    /// 三种来源各自的锚点。**恢复重放那条是这次修复的核心**:它的 `message_id` 是合成的
    /// `recovery-run-…`,拿去调飞书一律 400(`99992354`)——表现是恢复轮没有表情、总结卡发不出、
    /// 原生 CoT 建不起来退回卡片形态。真实 id 必须从 WAL 一路带到这里。
    #[test]
    fn feishu_anchor_separates_the_local_key_from_the_real_message() {
        let replay = task(
            "recovery-run-deadbeef",
            Some("om_original"),
            ChatTaskKind::Conversation,
        );
        assert_eq!(
            replay.feishu_anchor(),
            Some("om_original"),
            "恢复重放:锚点是原消息,不是本地幂等键"
        );

        let normal = task("om_real", None, ChatTaskKind::Conversation);
        assert_eq!(normal.feishu_anchor(), Some("om_real"));

        let timer = task("fire-123", None, ChatTaskKind::ScheduledPrompt);
        assert_eq!(
            timer.feishu_anchor(),
            None,
            "定时 fire 在飞书上没有对应消息,调用方必须跳过而不是拿合成 id 顶上"
        );

        let mut sentinel = task("fire-456", None, ChatTaskKind::Conversation);
        sentinel.hide_context_hint = true;
        assert_eq!(sentinel.feishu_anchor(), None, "sentinel 同理");
    }
}
