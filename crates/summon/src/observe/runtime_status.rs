//! daemon 运行时观测面(进程级):谁此刻在跑一轮 + 哪些会话的子 agent 还在跑 + 哪些无头会话进程
//! 还活着 + daemon 起来多久。
//!
//! **与 `InFlight` 严格分开,永不合并。** `InFlight` 是「可 kill 的子进程」注册表——停机时
//! `shutdown_interrupt_all` 据它发 SIGINT。Transcript(claude/tmux)会话根本没有可杀的子进程,
//! 把它登记进 `InFlight` 会让停机逻辑够得着 tmux 会话,逼近「重启期间绝不杀 tmux agent 会话」
//! 这条红线。这里只是**呈现态**:登记谁在跑,除了被读走以外不产生任何副作用。
//!
//! 无头会话进程活没活着直接读 RPC 会话池(`capture::rpc_pool`):进程在池里,daemon 一重启就全没了,
//! 盘上看不出来。只读池,不登记、不摘除、不发信号,与上面那条分开的约束不冲突。
//!
//! 为什么是进程级全局而不是穿参数:`stream_turn` 在调用链末端且参数已过多,把注册表从
//! `Core::new` 一路穿下去要改 5 个签名,侵入面远大于收益。观测面不参与所有权、不影响业务语义,
//! 全局是它的自然形态(日志/metrics 同理)。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use summon_core::credential_identity::AgentCredentialIdentity;
use summon_core::fleet::{BusyTurn, LiveSession, RuntimeStatus};
use summon_core::subagent::SubagentState;

type TurnKey = (String, String); // (agent, chat tag)

fn turns() -> &'static Mutex<HashMap<TurnKey, SystemTime>> {
    static TURNS: OnceLock<Mutex<HashMap<TurnKey, SystemTime>>> = OnceLock::new();
    TURNS.get_or_init(Default::default)
}

fn started_at() -> &'static OnceLock<SystemTime> {
    static STARTED: OnceLock<SystemTime> = OnceLock::new();
    &STARTED
}

/// 观测面的锁绝不 panic:某个线程持锁时 panic 会毒化锁,但「谁在跑」这种呈现态坏了
/// 也不该带崩整个 daemon —— 取回内部值继续用。
fn lock_turns() -> std::sync::MutexGuard<'static, HashMap<TurnKey, SystemTime>> {
    turns().lock().unwrap_or_else(|e| e.into_inner())
}

/// daemon `run()` 启动时打点。uptime 只在 daemon 进程内算得出(CLI 是另一个进程),
/// 故 `agents status` 必须经 control.sock 问它。重复调用无效(OnceLock),测试里可安全多次调。
pub fn mark_daemon_started() {
    let _ = started_at().set(SystemTime::now());
}

pub fn daemon_uptime(now: SystemTime) -> Option<Duration> {
    started_at().get().and_then(|t| now.duration_since(*t).ok())
}

/// 登记「这一轮开始跑了」,返回的 guard 落地即注销。
///
/// 必须用 RAII 而非手工 insert/remove:一轮有正常完成 / 被打断 / 早退 / panic 多条退出路径,
/// 只有 drop 能保证不留幽灵 busy —— 一个卡住的 busy 标记会让 Menu Bar 永远显示「在跑」,
/// 而这个谎言没有任何东西能纠正它。
pub fn enter_turn(agent: &str, tag: &str) -> TurnGuard {
    let key = (agent.to_string(), tag.to_string());
    lock_turns().insert(key.clone(), SystemTime::now());
    TurnGuard { key }
}

pub struct TurnGuard {
    key: TurnKey,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        lock_turns().remove(&self.key);
    }
}

/// 这个会话此刻在跑的那一轮从什么时候开始;没在跑为 `None`。
pub fn turn_started(agent: &str, tag: &str) -> Option<SystemTime> {
    lock_turns()
        .get(&(agent.to_string(), tag.to_string()))
        .copied()
}

/// 当前在跑的轮(按 agent/tag 稳定排序,便于人读与测试断言)。
pub fn busy_turns() -> Vec<BusyTurn> {
    let mut busy: Vec<BusyTurn> = lock_turns()
        .keys()
        .map(|(agent, tag)| BusyTurn {
            agent: agent.clone(),
            tag: tag.clone(),
        })
        .collect();
    busy.sort_by(|a, b| a.agent.cmp(&b.agent).then(a.tag.cmp(&b.tag)));
    busy
}

/// chat 线程手上正在处理的消息,按登记序号存(同一 chat 的新旧线程可能短暂并存,同子 agent 登记)。
fn handling() -> &'static Mutex<HashMap<u64, TurnKey>> {
    static HANDLING: OnceLock<Mutex<HashMap<u64, TurnKey>>> = OnceLock::new();
    HANDLING.get_or_init(Default::default)
}

fn lock_handling() -> std::sync::MutexGuard<'static, HashMap<u64, TurnKey>> {
    handling().lock().unwrap_or_else(|e| e.into_inner())
}

/// 登记「这个 chat 线程正在处理一条消息」,返回的 guard 落地即注销。
///
/// 比 [`enter_turn`] 宽:轮前的准备、轮末的交接与原地压缩、拆除、面板动作都在 chat 线程里同步做完,
/// 那段时间不在 busy 里,却同样不能被 daemon 换代打断。
pub fn enter_handling(agent: &str, tag: &str) -> HandlingGuard {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    lock_handling().insert(id, (agent.to_string(), tag.to_string()));
    mark_handling_change();
    HandlingGuard { id }
}

pub struct HandlingGuard {
    id: u64,
}

impl Drop for HandlingGuard {
    fn drop(&mut self) {
        lock_handling().remove(&self.id);
        mark_handling_change();
    }
}

/// 最近一次有会话开始或结束处理消息的时刻(unix 毫秒,0 = 从没有过)。
static LAST_HANDLING_CHANGE_MS: AtomicU64 = AtomicU64::new(0);

fn mark_handling_change() {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    LAST_HANDLING_CHANGE_MS.fetch_max(now, Ordering::SeqCst);
}

/// 最近一次有会话开始或结束处理消息的时刻。两次采样之间跑完的短轮,靠它也看得见。
pub fn last_handling_change() -> Option<SystemTime> {
    match LAST_HANDLING_CHANGE_MS.load(Ordering::SeqCst) {
        0 => None,
        ms => Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms)),
    }
}

/// 此刻正在处理消息的会话(按 agent/tag 稳定排序、去重)。
pub fn handling_chats() -> Vec<BusyTurn> {
    let mut chats: Vec<BusyTurn> = lock_handling()
        .values()
        .map(|(agent, tag)| BusyTurn {
            agent: agent.clone(),
            tag: tag.clone(),
        })
        .collect();
    chats.sort_by(|a, b| a.agent.cmp(&b.agent).then(a.tag.cmp(&b.tag)));
    chats.dedup();
    chats
}

type SubagentHandle = Arc<Mutex<SubagentState>>;

/// 各 chat 线程的子 agent 观测状态,按登记序号存:同一个 chat 的新线程可能在旧线程的登记注销前
/// 就起来了,按 (agent, tag) 存会让旧线程注销时抹掉新线程的那一份。
fn subagent_states() -> &'static Mutex<HashMap<u64, (TurnKey, SubagentHandle)>> {
    static STATES: OnceLock<Mutex<HashMap<u64, (TurnKey, SubagentHandle)>>> = OnceLock::new();
    STATES.get_or_init(Default::default)
}

fn lock_subagent_states() -> std::sync::MutexGuard<'static, HashMap<u64, (TurnKey, SubagentHandle)>>
{
    subagent_states().lock().unwrap_or_else(|e| e.into_inner())
}

/// 登记一个 chat 线程的子 agent 观测状态,返回的 guard 落地即注销(线程退出即退出观测)。
///
/// 登记的是状态句柄本身,读的时候现算:主轮收尾后子 agent 何时跑完只有 watcher 知道,
/// 由 chat 线程推送「还在跑」要另起节拍,推漏一次就是一条永远显示在干活的会话。
pub fn watch_subagents(agent: &str, tag: &str, state: &SubagentHandle) -> SubagentWatch {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let key = (agent.to_string(), tag.to_string());
    lock_subagent_states().insert(id, (key, Arc::clone(state)));
    SubagentWatch { id }
}

pub struct SubagentWatch {
    id: u64,
}

impl Drop for SubagentWatch {
    fn drop(&mut self) {
        lock_subagent_states().remove(&self.id);
    }
}

/// 此刻有活跃子 agent / workflow 的会话(按 agent/tag 稳定排序、去重)。判活用
/// [`SubagentState::activity_probe`],与空闲回收、定时器 fresh 判「有活跃子代理」是同一条时效轴。
pub fn active_subagents(now: SystemTime) -> Vec<BusyTurn> {
    // 先拷出句柄再逐个上锁:不在持有登记表锁时去拿 chat 线程的状态锁。状态锁只 try:
    // 管家的快照跑在它所在 agent 的 dispatcher 线程上,阻塞等锁会把那个 agent 的消息路由一起挂住
    // (同 dispatcher 的 `subagent_probe`)。此刻有人持锁就这一次少报它,下次快照再看。
    let handles: Vec<(TurnKey, SubagentHandle)> =
        lock_subagent_states().values().cloned().collect();
    let now = chrono::DateTime::<chrono::Utc>::from(now);
    let mut active: Vec<BusyTurn> = handles
        .into_iter()
        .filter(|(_, state)| match state.try_lock() {
            Ok(st) => st.activity_probe(now).active,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().activity_probe(now).active,
            Err(std::sync::TryLockError::WouldBlock) => false,
        })
        .map(|((agent, tag), _)| BusyTurn { agent, tag })
        .collect();
    active.sort_by(|a, b| a.agent.cmp(&b.agent).then(a.tag.cmp(&b.tag)));
    active.dedup();
    active
}

/// 本 daemon epoch 冻结的 per-agent 飞书身份指纹(agent 名 → SHA-256 hex)。
/// **只存摘要**;它和 busy 一样是纯呈现态,被读走以外不产生任何副作用。
fn credentials() -> &'static Mutex<BTreeMap<String, String>> {
    static CREDENTIALS: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
    CREDENTIALS.get_or_init(Default::default)
}

fn lock_credentials() -> std::sync::MutexGuard<'static, BTreeMap<String, String>> {
    credentials().lock().unwrap_or_else(|e| e.into_inner())
}

/// dispatcher 起来时登记它冻结的 epoch 身份指纹。
///
/// 为什么由 dispatcher 登记而不是 boot 统一登记:**掐入站的就是 dispatcher 手里那个 epoch**。
/// boot 另有一次 capture(给 outbox drain 用),两次 capture 之间盘上可能已经变了;上报别人的
/// 快照会让 doctor 说出一个「谁都没在用」的结论。宁可 daemon 起来前的窗口里报「无法确认」。
pub fn register_credential_identity(agent: &str, fingerprint: String) {
    lock_credentials().insert(agent.to_string(), fingerprint);
}

/// 当前已登记的 epoch 身份清单(按 agent 名稳定排序)。
pub fn credential_identities() -> Vec<AgentCredentialIdentity> {
    lock_credentials()
        .iter()
        .map(|(agent, fingerprint)| AgentCredentialIdentity {
            agent: agent.clone(),
            fingerprint: fingerprint.clone(),
        })
        .collect()
}

/// 会话进程此刻还活着的无头会话(codex / traex / cursor ACP 的常驻会话)。
///
/// cli-session(cursor `-p`)不在这里:它每轮一个进程,轮间没有可活着的东西;在跑的那一轮已经在
/// `busy` 里。
pub fn live_headless() -> Vec<LiveSession> {
    crate::capture::rpc_pool::live_sessions_shared()
        .into_iter()
        .map(|(agent, tag)| LiveSession { agent, tag })
        .collect()
}

/// 组装 control.sock 上回给 CLI 的运行时快照。
///
/// `live_headless` 恒为 `Some`(一个都没有也是 `Some(空)`):`None` 留给「旧 daemon 没带这个字段」,
/// CLI 据此退回盘上判据。
pub fn snapshot(pid: u32, version: Option<String>, now: SystemTime) -> RuntimeStatus {
    RuntimeStatus {
        pid,
        version,
        uptime_secs: daemon_uptime(now).map(|d| d.as_secs()).unwrap_or(0),
        busy: busy_turns(),
        credentials: credential_identities(),
        live_headless: Some(live_headless()),
        subagents: active_subagents(now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局注册表在测试间共享,故各用例用**自己的 agent 名**隔离,不清表(清表会打断并发用例)。
    fn busy_of(agent: &str) -> Vec<String> {
        busy_turns()
            .into_iter()
            .filter(|b| b.agent == agent)
            .map(|b| b.tag)
            .collect()
    }

    #[test]
    fn handling_is_registered_while_a_message_is_handled() {
        let agent = "t-handling";
        let of = || {
            handling_chats()
                .into_iter()
                .filter(|c| c.agent == agent)
                .map(|c| c.tag)
                .collect::<Vec<_>>()
        };
        let before = SystemTime::now() - Duration::from_millis(1);
        {
            let _first = enter_handling(agent, "g1");
            let _second = enter_handling(agent, "g1");
            assert_eq!(of(), vec!["g1"], "同一会话去重");
        }
        assert!(of().is_empty(), "guard 落地即注销");
        assert!(last_handling_change().is_some_and(|at| at >= before));
    }

    #[test]
    fn guard_registers_and_drop_deregisters() {
        let agent = "t-guard";
        assert!(busy_of(agent).is_empty());
        {
            let _g = enter_turn(agent, "g1a2b3c4");
            assert_eq!(busy_of(agent), vec!["g1a2b3c4"]);
        }
        assert!(busy_of(agent).is_empty(), "guard 落地即注销");
    }

    #[test]
    fn panicking_turn_does_not_leave_a_ghost_busy() {
        // 一轮 panic 后 busy 必须自动清 —— 幽灵 busy 会让 Menu Bar 永远显示「在跑」。
        let agent = "t-panic";
        let hit = std::panic::catch_unwind(|| {
            let _g = enter_turn(agent, "g0000001");
            panic!("turn blew up");
        });
        assert!(hit.is_err());
        assert!(busy_of(agent).is_empty());
    }

    #[test]
    fn concurrent_turns_of_one_agent_are_all_listed() {
        let agent = "t-multi";
        let _a = enter_turn(agent, "g1111111");
        let _b = enter_turn(agent, "p2222222");
        assert_eq!(busy_of(agent), vec!["g1111111", "p2222222"]);
    }

    #[test]
    fn uptime_counts_from_daemon_start() {
        mark_daemon_started();
        let start = *started_at().get().unwrap();
        let uptime = daemon_uptime(start + Duration::from_secs(90)).unwrap();
        assert_eq!(uptime.as_secs(), 90);
    }

    #[test]
    fn snapshot_carries_registered_credential_identities() {
        register_credential_identity("t-cred", "deadbeef".into());
        let snap = snapshot(1, None, SystemTime::now());
        assert_eq!(
            summon_core::credential_identity::fingerprint_of(&snap.credentials, "t-cred"),
            Some("deadbeef")
        );
        // 未登记的 agent 不能借别人的结论(fail-closed 是 per-agent 的)。
        assert_eq!(
            summon_core::credential_identity::fingerprint_of(&snap.credentials, "t-cred-absent"),
            None
        );
    }

    fn subagents_of(agent: &str) -> Vec<String> {
        active_subagents(SystemTime::now())
            .into_iter()
            .filter(|b| b.agent == agent)
            .map(|b| b.tag)
            .collect()
    }

    fn running_state() -> SubagentHandle {
        let mut state = SubagentState::new();
        state.promote_to_running("a1", "后台生成素材", "tu_1", None, chrono::Utc::now());
        Arc::new(Mutex::new(state))
    }

    #[test]
    fn running_subagent_is_reported_until_its_watch_drops() {
        let agent = "t-subagent";
        let idle = Arc::new(Mutex::new(SubagentState::new()));
        let _idle = watch_subagents(agent, "g0000002", &idle);
        let running = running_state();
        {
            let _watch = watch_subagents(agent, "g0000001", &running);
            assert_eq!(
                subagents_of(agent),
                vec!["g0000001"],
                "只报有活跃子 agent 的会话"
            );
            let snap = snapshot(1, None, SystemTime::now());
            assert!(snap.subagents.iter().any(|b| b.agent == agent));
            assert!(
                snap.busy.iter().all(|b| b.agent != agent),
                "子 agent 在跑不等于在跑一轮"
            );
        }
        assert!(subagents_of(agent).is_empty(), "chat 线程退出即退出观测");
    }

    /// 同一个 chat 换了新线程:旧线程晚一步注销,不能把新线程的登记一并抹掉。
    #[test]
    fn an_old_thread_leaving_does_not_unwatch_its_successor() {
        let agent = "t-subagent-successor";
        let old = watch_subagents(agent, "g0000003", &running_state());
        let _new = watch_subagents(agent, "g0000003", &running_state());
        drop(old);
        assert_eq!(subagents_of(agent), vec!["g0000003"]);
    }

    /// 新 daemon 必须报出进程名单,哪怕一个都没有:缺了它 CLI 会当成旧 daemon,
    /// 把进程早没了的无头会话按盘上事件继续显示成空闲。
    #[test]
    fn snapshot_always_reports_the_live_headless_list() {
        let snap = snapshot(1, None, SystemTime::now());
        assert!(snap.live_headless.is_some());
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains(r#""live_headless":"#), "{json}");
    }

    #[test]
    fn snapshot_carries_pid_version_and_busy() {
        mark_daemon_started();
        let agent = "t-snapshot";
        let _g = enter_turn(agent, "g3333333");
        let snap = snapshot(4242, Some("0.1.0".into()), SystemTime::now());
        assert_eq!(snap.pid, 4242);
        assert_eq!(snap.version.as_deref(), Some("0.1.0"));
        assert!(snap
            .busy
            .iter()
            .any(|b| b.agent == agent && b.tag == "g3333333"));
    }
}
