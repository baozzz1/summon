//! 主轮结束后继续跟随本轮 claude 子 agent 的 CoT。
//!
//! claude 的 `Agent` 调用默认异步:主 agent 派完十几秒就收轮,子 agent 的过程大多发生在那之后。
//! 回复照常先发;这一轮的 CoT 从本轮前端摘出来(`Frontend::stream_detach_cot`),交给一个独立
//! 线程继续把本轮派出的子 agent 的过程推进**同一条** CoT,都结束再收口。
//!
//! 不变量:
//! - 只往这一轮已有的 CoT 追加事件,不发新消息;
//! - 有界:跟随总时长、子转录静默时长、会话生命周期(拆除 / 重置 / 清理)、本 chat 的 `/stop`
//!   任一到头就收口,没结束的子 agent 块尾由投影器标「（仍在运行）」;`/stop` 只停展示,不碰
//!   子 agent;
//! - append 失败即不确定终态:不重放,只尝试 `complete(error)`;
//! - 同一 chat 的下一轮照常开自己的 CoT,两条并行、互不相干。
//!
//! daemon 停机时跟随线程随进程结束,不另做处理:恢复记录仍在盘上,下一代启动收口按 `done`
//! 收掉它(带停机说明)。

use super::*;
use crate::capture::SubagentTail;
use crate::frontend::client::CotCompleteReason;
use crate::frontend::{CotCarrier, DetachedCot};
use crate::runtime::executor::{Executor, LocalExecutor};
use std::sync::atomic::{AtomicBool, Ordering};
use summon_core::cot::{CotEvent, CotProjector};
use summon_core::progress::TurnProgress;
use summon_core::segment::TurnSegment;
use summon_core::subagent_projection::SubagentProjection;

/// 读子转录、推 CoT 的节拍。
const FOLLOW_TICK: Duration = Duration::from_secs(1);
/// 跟随总时长上限。
const FOLLOW_MAX: Duration = Duration::from_secs(2 * 60 * 60);
/// 子转录连续这么久没有新字节就不再等。
const FOLLOW_SILENCE_MAX: Duration = Duration::from_secs(30 * 60);
/// 与轮内 CoT 同一个合并窗口(frontend 的 `COT_UPDATE_MIN_INTERVAL_MS`)。
const FOLLOW_PUSH_INTERVAL: Duration = Duration::from_millis(600);

/// 跟随线程写 CoT 的出口。生产实现包 `FeishuClient` + 恢复记录;测试注入假的。
pub(super) trait CotSink: Send {
    /// 追加一批事件。失败即不确定终态,调用方不得重放。
    fn push(&mut self, events: &[CotEvent]) -> anyhow::Result<()>;
    /// push 成功后把投影器游标写进恢复记录。
    fn snapshot(&mut self, projector: &CotProjector);
    fn complete(&mut self, reason: CotCompleteReason) -> anyhow::Result<()>;
    /// 放手恢复记录。`closed` = 远端已收口。
    fn release(&mut self, closed: bool);
}

pub(super) trait FollowClock: Send {
    fn now(&self) -> Instant;
    /// 等到下一拍。
    fn wait(&mut self, tick: Duration);
}

struct SystemClock;

impl FollowClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wait(&mut self, tick: Duration) {
        thread::sleep(tick);
    }
}

struct FeishuCotSink {
    fc: Arc<FeishuClient>,
    carrier: CotCarrier,
}

impl CotSink for FeishuCotSink {
    fn push(&mut self, events: &[CotEvent]) -> anyhow::Result<()> {
        self.carrier.push(&self.fc, events)
    }

    fn snapshot(&mut self, projector: &CotProjector) {
        self.carrier.snapshot(projector);
    }

    fn complete(&mut self, reason: CotCompleteReason) -> anyhow::Result<()> {
        self.carrier.complete(&self.fc, reason)
    }

    fn release(&mut self, closed: bool) {
        self.carrier.release(closed);
    }
}

/// 跟随为什么收口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FollowEnd {
    AllFinished,
    TimedOut,
    Silent,
    Stopped,
    Failed,
}

impl FollowEnd {
    fn as_str(self) -> &'static str {
        match self {
            Self::AllFinished => summon_i18n::text!("rde4fff8b21"),
            Self::TimedOut => summon_i18n::text!("rb766d9b612"),
            Self::Silent => summon_i18n::text!("r3946c5a6b3"),
            Self::Stopped => summon_i18n::text!("r6a0fdf3eca"),
            Self::Failed => summon_i18n::text!("rf0edb3010b"),
        }
    }
}

/// 主轮交给跟随线程的全部现场。
pub(super) struct FollowParts {
    pub(super) name: String,
    pub(super) tag: String,
    /// 主轮最后的段(与轮内投影同一份切分):投影器的游标指着它。
    pub(super) segments: Vec<TurnSegment>,
    pub(super) progress: TurnProgress,
    pub(super) subagents: SubagentProjection,
    pub(super) tail: Option<Box<SubagentTail>>,
    pub(super) state: Option<SharedSubagentState>,
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
        .to_string()
}

/// `follow` 对出口的持有。没走到 `release` 就析构(跟随中途 panic,unwind 经过这里)时替它
/// `release(false)`:恢复记录不再算本进程 active,开新卡前的反熵与下一代的启动收口会收掉这条
/// CoT。只做这一件本地的事 —— unwind 途中再碰网络,二次 panic 会让整个进程 abort。
struct ReleaseGuard<'a> {
    sink: &'a mut dyn CotSink,
    released: bool,
}

impl CotSink for ReleaseGuard<'_> {
    fn push(&mut self, events: &[CotEvent]) -> anyhow::Result<()> {
        self.sink.push(events)
    }

    fn snapshot(&mut self, projector: &CotProjector) {
        self.sink.snapshot(projector);
    }

    fn complete(&mut self, reason: CotCompleteReason) -> anyhow::Result<()> {
        self.sink.complete(reason)
    }

    fn release(&mut self, closed: bool) {
        self.released = true;
        self.sink.release(closed);
    }
}

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.sink.release(false);
        }
    }
}

/// 跟随到收口为止。每拍:读子转录 → 投影进账本 → 按状态源标结束 → 投影 CoT → 节流推送 →
/// 推成功就落快照。
pub(super) fn follow<E: Executor>(
    mut projector: CotProjector,
    mut parts: FollowParts,
    sink: &mut dyn CotSink,
    clock: &mut dyn FollowClock,
    exec: &E,
    stop: &AtomicBool,
) -> FollowEnd {
    let sink = &mut ReleaseGuard {
        sink,
        released: false,
    };
    let started = clock.now();
    let mut last_bytes = started;
    let mut last_push: Option<Instant> = None;
    let mut pending: Vec<CotEvent> = Vec::new();
    // 主 agent 这一轮已经写完,子 agent 的行不会再和它交错:在播块一直可以实时推。
    parts.progress.set_parent_waiting(true);
    let end = loop {
        if stop.load(Ordering::Relaxed) {
            break FollowEnd::Stopped;
        }
        let now = clock.now();
        if let Some(tail) = parts.tail.as_mut() {
            let mut events = Vec::new();
            if tail.poll(exec, now, &mut |event| events.push(event.clone())) {
                last_bytes = now;
            }
            for event in &events {
                if let Some(parent) = event.parent_run_id.as_ref() {
                    parts
                        .subagents
                        .project(&mut parts.progress, &parent.0, event, now);
                }
            }
            for (key, agent_id) in tail.agent_ids() {
                parts.subagents.note_agent_id(key, agent_id);
            }
        }
        if let Some(state) = &parts.state {
            let state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            parts.subagents.reconcile(&mut parts.progress, &state);
        }
        pending.extend(projector.project(&parts.segments, Some(&parts.progress), &timestamp()));
        if !parts.subagents.has_unfinished(&parts.progress) {
            break FollowEnd::AllFinished;
        }
        if now.saturating_duration_since(started) >= FOLLOW_MAX {
            break FollowEnd::TimedOut;
        }
        if now.saturating_duration_since(last_bytes) >= FOLLOW_SILENCE_MAX {
            break FollowEnd::Silent;
        }
        let push_due =
            last_push.is_none_or(|at| now.saturating_duration_since(at) >= FOLLOW_PUSH_INTERVAL);
        if !pending.is_empty() && push_due {
            if let Err(error) = sink.push(&pending) {
                // append 没有幂等键,响应丢失时服务端可能已提交:不重放,只收口。
                log(&summon_i18n::format!("r7b6f8306c9", parts.name, parts.tag));
                let closed = sink.complete(CotCompleteReason::Error).is_ok();
                sink.release(closed);
                log_end(&parts, FollowEnd::Failed, clock.now(), started);
                return FollowEnd::Failed;
            }
            pending.clear();
            last_push = Some(now);
            sink.snapshot(&projector);
        }
        clock.wait(FOLLOW_TICK);
    };
    pending.extend(projector.finish("done", &timestamp()));
    let closed = match sink.push(&pending) {
        Ok(()) => true,
        Err(error) => {
            log(&summon_i18n::format!("r826797e0f1", parts.name, parts.tag));
            sink.complete(CotCompleteReason::Done).is_ok()
        }
    };
    sink.release(closed);
    log_end(&parts, end, clock.now(), started);
    end
}

fn log_end(parts: &FollowParts, end: FollowEnd, now: Instant, started: Instant) {
    log(&summon_i18n::format!(
        "rc523211cec",
        parts.name,
        end.as_str(),
        parts.tag,
        now.saturating_duration_since(started).as_secs()
    ));
}

type SinkFactory = Box<dyn Fn(CotCarrier) -> Box<dyn CotSink>>;
type ClockFactory = Box<dyn Fn() -> Box<dyn FollowClock>>;

struct Follower {
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<FollowEnd>,
}

/// 本 chat 的 CoT 跟随线程。chat 线程持有,跨配置重载不变;析构即通知所有跟随线程收口 ——
/// 会话拆除 / 重置 / 清理都经 chat 线程退出这一条路。
pub(super) struct CotFollowers {
    make_sink: SinkFactory,
    make_clock: ClockFactory,
    live: std::cell::RefCell<Vec<Follower>>,
}

impl CotFollowers {
    pub(super) fn feishu(fc: Arc<FeishuClient>) -> Self {
        Self::with_parts(
            Box::new(move |carrier| {
                Box::new(FeishuCotSink {
                    fc: fc.clone(),
                    carrier,
                })
            }),
            Box::new(|| Box::new(SystemClock)),
        )
    }

    fn with_parts(make_sink: SinkFactory, make_clock: ClockFactory) -> Self {
        Self {
            make_sink,
            make_clock,
            live: Default::default(),
        }
    }

    /// 接手一条摘出来的 CoT,起线程跟随到收口。
    pub(super) fn spawn(&self, cot: DetachedCot, parts: FollowParts) {
        let DetachedCot { carrier, projector } = cot;
        log(&summon_i18n::format!(
            "ra50c326515",
            parts.name,
            parts.subagents.unfinished_count(&parts.progress),
            parts.tag
        ));
        let mut sink = (self.make_sink)(carrier);
        let mut clock = (self.make_clock)();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = thread::spawn(move || {
            let (name, tag) = (parts.name.clone(), parts.tag.clone());
            // panic 时恢复记录已由 `follow` 里的守卫放手;这里只把它记成一次失败收口。
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                follow(
                    projector,
                    parts,
                    sink.as_mut(),
                    clock.as_mut(),
                    &LocalExecutor,
                    &stop_flag,
                )
            }))
            .unwrap_or_else(|_| {
                log(&summon_i18n::format!("r581e46f9ec"));
                FollowEnd::Failed
            })
        });
        let mut live = self.live.borrow_mut();
        live.retain(|follower| !follower.handle.is_finished());
        live.push(Follower { stop, handle });
    }

    /// 给本 chat 还在跟随的线程置停止信号(各自按 `Stopped` 收口),返回这次新停下的个数。
    /// 只停展示,子 agent 本身不受影响。
    pub(super) fn stop_all(&self) -> usize {
        let mut stopped = 0;
        for follower in self.live.borrow().iter() {
            if !follower.handle.is_finished() && !follower.stop.swap(true, Ordering::Relaxed) {
                stopped += 1;
            }
        }
        stopped
    }

    #[cfg(test)]
    fn stop_flags(&self) -> Vec<Arc<AtomicBool>> {
        self.live
            .borrow()
            .iter()
            .map(|follower| follower.stop.clone())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn spawned(&self) -> usize {
        self.live.borrow().len()
    }

    #[cfg(test)]
    fn all_finished(&self) -> bool {
        self.live
            .borrow()
            .iter()
            .all(|follower| follower.handle.is_finished())
    }

    #[cfg(test)]
    pub(super) fn join_all(&self) -> Vec<FollowEnd> {
        self.live
            .borrow_mut()
            .drain(..)
            .map(|follower| {
                follower
                    .handle
                    .join()
                    .expect(summon_i18n::text!("r3be631e5e0"))
            })
            .collect()
    }
}

impl Drop for CotFollowers {
    fn drop(&mut self) {
        for follower in self.live.get_mut().iter() {
            follower.stop.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use crate::runtime::daemon::tests::{
        CLAUDE_AGENT_CALL, CLAUDE_AGENT_LAUNCHED, CLAUDE_SUB_BASH,
    };

    #[derive(Debug, Default)]
    pub(crate) struct SinkLog {
        pub(crate) pushes: Vec<Vec<CotEvent>>,
        pub(crate) snapshots: usize,
        pub(crate) completes: Vec<CotCompleteReason>,
        pub(crate) releases: Vec<bool>,
    }

    impl SinkLog {
        pub(crate) fn events(&self) -> Vec<CotEvent> {
            self.pushes.iter().flatten().cloned().collect()
        }

        pub(crate) fn count(&self, event_type: &str) -> usize {
            self.events()
                .iter()
                .filter(|event| event.event_type == event_type)
                .count()
        }
    }

    /// 记账的假出口。`fail_push` = 第几次 push(从 0 起)失败;`panic_push` = push 直接 panic。
    pub(crate) struct RecordingSink {
        pub(crate) log: Arc<std::sync::Mutex<SinkLog>>,
        pub(crate) fail_push: Option<usize>,
        pub(crate) panic_push: bool,
        pub(crate) attempts: usize,
    }

    impl RecordingSink {
        pub(crate) fn new(log: Arc<std::sync::Mutex<SinkLog>>) -> Self {
            Self {
                log,
                fail_push: None,
                panic_push: false,
                attempts: 0,
            }
        }
    }

    impl CotSink for RecordingSink {
        fn push(&mut self, events: &[CotEvent]) -> anyhow::Result<()> {
            let attempt = self.attempts;
            self.attempts += 1;
            assert!(!self.panic_push, "注入的 push panic");
            if self.fail_push == Some(attempt) {
                anyhow::bail!("push failed");
            }
            self.log.lock().unwrap().pushes.push(events.to_vec());
            Ok(())
        }

        fn snapshot(&mut self, _projector: &CotProjector) {
            self.log.lock().unwrap().snapshots += 1;
        }

        fn complete(&mut self, reason: CotCompleteReason) -> anyhow::Result<()> {
            self.log.lock().unwrap().completes.push(reason);
            Ok(())
        }

        fn release(&mut self, closed: bool) {
            self.log.lock().unwrap().releases.push(closed);
        }
    }

    /// 由测试一拍一拍驱动的时钟:每次 `wait` 前进 `step`,并执行排好的下一步动作
    /// (写子转录、置停止信号……)。
    pub(crate) struct ScriptedClock {
        pub(crate) now: Instant,
        pub(crate) step: Duration,
        pub(crate) script: std::collections::VecDeque<Box<dyn FnOnce() + Send>>,
        pub(crate) waits: usize,
    }

    impl ScriptedClock {
        pub(crate) fn new(step: Duration) -> Self {
            Self {
                now: Instant::now(),
                step,
                script: Default::default(),
                waits: 0,
            }
        }
    }

    impl FollowClock for ScriptedClock {
        fn now(&self) -> Instant {
            self.now
        }

        fn wait(&mut self, _tick: Duration) {
            self.waits += 1;
            self.now += self.step;
            if let Some(action) = self.script.pop_front() {
                action();
            }
        }
    }

    /// 由另一个线程发令推进的时钟:没有令就阻塞,令发完(发送端断开)就照常前进。
    pub(crate) struct GatedClock {
        pub(crate) now: Instant,
        pub(crate) gate: std::sync::mpsc::Receiver<()>,
    }

    impl FollowClock for GatedClock {
        fn now(&self) -> Instant {
            self.now
        }

        fn wait(&mut self, tick: Duration) {
            let _ = self.gate.recv();
            self.now += tick;
        }
    }

    pub(crate) fn followers(
        log: Arc<std::sync::Mutex<SinkLog>>,
        gate: std::sync::mpsc::Receiver<()>,
    ) -> CotFollowers {
        followers_with(gate, move || RecordingSink::new(log.clone()))
    }

    /// 出口由 `make_sink` 造(可以注入失败 / panic);时钟由 `gate` 发令推进。
    pub(crate) fn followers_with(
        gate: std::sync::mpsc::Receiver<()>,
        make_sink: impl Fn() -> RecordingSink + 'static,
    ) -> CotFollowers {
        let gate = std::sync::Mutex::new(Some(gate));
        CotFollowers::with_parts(
            Box::new(move |_carrier| Box::new(make_sink())),
            Box::new(move || {
                Box::new(GatedClock {
                    now: Instant::now(),
                    gate: gate.lock().unwrap().take().expect("测试里只起一个跟随线程"),
                })
            }),
        )
    }

    fn parse(lines: &[&str]) -> Vec<summon_core::agent_msg::AgentMessage> {
        lines
            .iter()
            .flat_map(|line| summon_core::stream_json::parse_stream_json_line(line))
            .collect()
    }

    /// 主轮收尾时的现场:派出一个异步子 agent(已拿到派出回执),它还没出过声。
    pub(crate) fn launched(
        dir: &std::path::Path,
        state: Option<SharedSubagentState>,
    ) -> (CotProjector, FollowParts) {
        launched_with(dir, state, true)
    }

    /// `receipt_agent_id` = 派出回执里带不带 agentId。
    pub(crate) fn launched_with(
        dir: &std::path::Path,
        state: Option<SharedSubagentState>,
        receipt_agent_id: bool,
    ) -> (CotProjector, FollowParts) {
        let main = dir.join("sid-1.jsonl");
        std::fs::write(&main, "").unwrap();
        std::fs::create_dir_all(dir.join("sid-1").join("subagents")).unwrap();
        let mut tail = SubagentTail::new(&main, "sid-1");
        let mut subagents = SubagentProjection::default();
        let mut progress = TurnProgress::new().with_tool_details(true);
        for mut event in parse(&[CLAUDE_AGENT_CALL, CLAUDE_AGENT_LAUNCHED]) {
            if !receipt_agent_id {
                if let Some(launch) = event.async_launch.as_mut() {
                    launch.agent_id = None;
                }
            }
            tail.note(&event);
            match event.kind {
                summon_core::agent_msg::AgentKind::ToolUse => {
                    progress.tool_started_with_description(
                        "Agent",
                        "Agent adversarial-reviewer",
                        "Agent",
                        None,
                        None,
                        None,
                    );
                    subagents.note_main_tool(&mut progress, "Agent", &event);
                }
                _ => {
                    progress.tool_finished(0);
                    subagents.note_main_result(&mut progress, &event);
                }
            }
        }
        let mut projector = CotProjector::new("oc_follow", "turn-1", "任务");
        // 轮内已推过的部分:主 agent 的派出行。
        projector.project(&[], Some(&progress), "1000");
        let parts = FollowParts {
            name: "tester".into(),
            tag: "p2p-oc_follow".into(),
            segments: Vec::new(),
            progress,
            subagents,
            tail: Some(Box::new(tail)),
            state,
        };
        (projector, parts)
    }

    pub(crate) fn write_sub(dir: &std::path::Path, lines: &[&str]) {
        let subagents = dir.join("sid-1").join("subagents");
        std::fs::write(
            subagents.join("agent-a1c7b6848c7434fad.meta.json"),
            r#"{"agentType":"adversarial-reviewer","description":"审一下","toolUseId":"toolu_async"}"#,
        )
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(subagents.join("agent-a1c7b6848c7434fad.jsonl"))
            .unwrap();
        for line in lines {
            std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()).unwrap();
        }
    }

    /// 起一个跟随线程:主轮派出的异步子 agent 已有一行工具在跑,不会自己结束。它推完首拍后
    /// 停在等下一拍,由返回的发令端推进。
    pub(crate) fn one_follower(
        dir: &std::path::Path,
    ) -> (
        CotFollowers,
        std::sync::mpsc::Sender<()>,
        Arc<std::sync::Mutex<SinkLog>>,
    ) {
        let (projector, parts) = launched(dir, None);
        write_sub(dir, &[CLAUDE_SUB_BASH]);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let registry = followers(log.clone(), gate_rx);
        registry.spawn(
            DetachedCot {
                carrier: CotCarrier::for_test("cot_follow", "om_follow"),
                projector,
            },
            parts,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while log.lock().unwrap().pushes.is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        (registry, gate_tx, log)
    }

    /// 发令直到所有跟随线程退出。有上限:到点还没退出就判失败,不去 join 一个停不下来的线程。
    pub(crate) fn drive_to_end(
        followers: &CotFollowers,
        gate: &std::sync::mpsc::Sender<()>,
    ) -> Vec<FollowEnd> {
        for _ in 0..200 {
            if followers.all_finished() {
                return followers.join_all();
            }
            let _ = gate.send(());
            thread::sleep(Duration::from_millis(5));
        }
        panic!("跟随线程没有收口");
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::runtime::daemon::tests::{
        CLAUDE_SUB_BASH, CLAUDE_SUB_BASH_RESULT, CLAUDE_SUB_REPORT,
    };

    fn tool_titles(events: &[CotEvent]) -> Vec<String> {
        events
            .iter()
            .filter(|event| event.event_type == "TOOL_CALL_START")
            .map(|event| {
                serde_json::from_str::<serde_json::Value>(&event.content).unwrap()["toolCallName"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    fn reasoning(events: &[CotEvent]) -> String {
        events
            .iter()
            .filter(|event| event.event_type == "REASONING_MESSAGE_CONTENT")
            .map(|event| {
                serde_json::from_str::<serde_json::Value>(&event.content).unwrap()["delta"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    fn run(
        projector: CotProjector,
        parts: FollowParts,
        sink: &mut RecordingSink,
        clock: &mut ScriptedClock,
        stop: &AtomicBool,
    ) -> FollowEnd {
        follow(projector, parts, sink, clock, &LocalExecutor, stop)
    }

    /// 回复之后子 agent 才开始干活:它的工具行实时进同一条 CoT,结束后收口恰好一次。
    #[test]
    fn subagent_work_after_the_reply_streams_into_the_same_cot() {
        let dir = tempfile::tempdir().unwrap();
        let (projector, parts) = launched(dir.path(), None);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let mut sink = RecordingSink::new(log.clone());
        let mut clock = ScriptedClock::new(Duration::from_secs(1));
        let sub = dir.path().to_path_buf();
        clock
            .script
            .push_back(Box::new(move || write_sub(&sub, &[CLAUDE_SUB_BASH])));
        clock.script.push_back(Box::new(|| {}));
        let sub = dir.path().to_path_buf();
        clock.script.push_back(Box::new(move || {
            write_sub(&sub, &[CLAUDE_SUB_BASH_RESULT, CLAUDE_SUB_REPORT])
        }));

        let end = run(
            projector,
            parts,
            &mut sink,
            &mut clock,
            &AtomicBool::new(false),
        );
        assert_eq!(end, FollowEnd::AllFinished);
        let log = log.lock().unwrap();
        let before_end: Vec<CotEvent> = log.pushes[..log.pushes.len() - 1]
            .iter()
            .flatten()
            .cloned()
            .collect();
        assert!(
            tool_titles(&before_end).contains(&"git diff --stat".to_string()),
            "子 agent 的工具在收口前就实时推出:{:?}",
            log.pushes
        );
        let events = log.events();
        assert_eq!(log.count("RUN_FINISHED"), 1);
        assert_eq!(events.last().unwrap().event_type, "RUN_FINISHED");
        assert_eq!(log.count("TOOL_CALL_RESULT"), 1, "工具结果挂回自己那一行");
        let text = reasoning(&events);
        assert!(text.contains("Agent adversarial-reviewer"), "{text}");
        assert!(!text.contains("仍在运行"), "{text}");
        assert!(!text.contains("结论:没问题"), "给主 agent 的报告不进 CoT");
        assert_eq!(log.releases, [true]);
        assert!(
            log.completes.is_empty(),
            "RUN_FINISHED 已经收口,不再显式 complete"
        );
        assert_eq!(
            log.snapshots,
            log.pushes.len() - 1,
            "每次成功推送(收口那次除外)都落快照"
        );
    }

    /// 结束信号来自状态源:子转录没写 `end_turn` 也能收口。
    #[test]
    fn the_shared_state_ends_the_follow() {
        let dir = tempfile::tempdir().unwrap();
        let state: SharedSubagentState = Arc::new(std::sync::Mutex::new(
            summon_core::subagent::SubagentState::new(),
        ));
        // 派出回执不带 agentId:只能靠子转录 meta 的文件名认人。
        let (projector, parts) = launched_with(dir.path(), Some(state.clone()), false);
        write_sub(dir.path(), &[CLAUDE_SUB_BASH]);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let mut sink = RecordingSink::new(log.clone());
        let mut clock = ScriptedClock::new(Duration::from_secs(1));
        clock.script.push_back(Box::new(move || {
            // 结束的条目只剩 agentId:这里故意不经在跑态,只能靠子转录 meta 里的 agentId 认。
            let mut st = state.lock().unwrap();
            st.promote_to_running(
                "a1c7b6848c7434fad",
                "审一下",
                "toolu_other_session",
                None,
                chrono::Utc::now(),
            );
            st.mark_completed("a1c7b6848c7434fad", chrono::Utc::now());
        }));
        let end = run(
            projector,
            parts,
            &mut sink,
            &mut clock,
            &AtomicBool::new(false),
        );
        assert_eq!(end, FollowEnd::AllFinished);
        assert_eq!(clock.waits, 1);
        let log = log.lock().unwrap();
        assert_eq!(log.count("RUN_FINISHED"), 1);
        assert!(!reasoning(&log.events()).contains("仍在运行"));
    }

    /// 子 agent 一直有动静却不结束:两小时到头收口,块尾标「（仍在运行）」。
    #[test]
    fn follow_gives_up_after_two_hours() {
        let dir = tempfile::tempdir().unwrap();
        let (projector, parts) = launched(dir.path(), None);
        write_sub(dir.path(), &[CLAUDE_SUB_BASH]);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let mut sink = RecordingSink::new(log.clone());
        let mut clock = ScriptedClock::new(Duration::from_secs(10 * 60));
        for _ in 0..20 {
            let sub = dir.path().to_path_buf();
            clock
                .script
                .push_back(Box::new(move || write_sub(&sub, &[CLAUDE_SUB_BASH_RESULT])));
        }
        let end = run(
            projector,
            parts,
            &mut sink,
            &mut clock,
            &AtomicBool::new(false),
        );
        assert_eq!(end, FollowEnd::TimedOut);
        assert_eq!(clock.waits, 12, "每拍 10 分钟,第 12 拍后满两小时");
        let log = log.lock().unwrap();
        assert_eq!(log.count("RUN_FINISHED"), 1);
        assert!(reasoning(&log.events()).contains("（仍在运行）"));
        assert_eq!(log.releases, [true]);
    }

    /// 子转录 30 分钟没有新字节:不再等。
    #[test]
    fn follow_gives_up_after_thirty_silent_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let (projector, parts) = launched(dir.path(), None);
        write_sub(dir.path(), &[CLAUDE_SUB_BASH]);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let mut sink = RecordingSink::new(log.clone());
        let mut clock = ScriptedClock::new(Duration::from_secs(5 * 60));
        let end = run(
            projector,
            parts,
            &mut sink,
            &mut clock,
            &AtomicBool::new(false),
        );
        assert_eq!(end, FollowEnd::Silent);
        assert_eq!(clock.waits, 6, "首拍读到字节,之后 6 拍 × 5 分钟静默");
        let log = log.lock().unwrap();
        assert_eq!(log.count("RUN_FINISHED"), 1);
        assert!(reasoning(&log.events()).contains("（仍在运行）"));
    }

    /// 会话拆除的停止信号:下一拍就收口。
    #[test]
    fn the_stop_signal_closes_the_cot() {
        let dir = tempfile::tempdir().unwrap();
        let (projector, parts) = launched(dir.path(), None);
        write_sub(dir.path(), &[CLAUDE_SUB_BASH]);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let mut sink = RecordingSink::new(log.clone());
        let mut clock = ScriptedClock::new(Duration::from_secs(1));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        clock
            .script
            .push_back(Box::new(move || flag.store(true, Ordering::Relaxed)));
        let end = run(projector, parts, &mut sink, &mut clock, &stop);
        assert_eq!(end, FollowEnd::Stopped);
        assert_eq!(clock.waits, 1);
        let log = log.lock().unwrap();
        assert_eq!(log.count("RUN_FINISHED"), 1);
        assert!(reasoning(&log.events()).contains("（仍在运行）"));
        assert_eq!(log.releases, [true]);
    }

    /// 追加失败即不确定终态:不重放、不再推,只 `complete(error)` 收口。
    #[test]
    fn an_append_failure_is_never_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let (projector, parts) = launched(dir.path(), None);
        write_sub(dir.path(), &[CLAUDE_SUB_BASH]);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let mut sink = RecordingSink::new(log.clone());
        sink.fail_push = Some(0);
        let mut clock = ScriptedClock::new(Duration::from_secs(1));
        let end = run(
            projector,
            parts,
            &mut sink,
            &mut clock,
            &AtomicBool::new(false),
        );
        assert_eq!(end, FollowEnd::Failed);
        assert_eq!(sink.attempts, 1, "失败后一次都不再推");
        assert_eq!(clock.waits, 0);
        let log = log.lock().unwrap();
        assert!(log.pushes.is_empty());
        assert_eq!(log.completes, [CotCompleteReason::Error]);
        assert_eq!(log.snapshots, 0);
        assert_eq!(log.releases, [true]);
    }

    /// 收口那批写不进去:显式 `complete(done)` 兜底。
    #[test]
    fn a_failed_final_push_falls_back_to_an_explicit_complete() {
        let dir = tempfile::tempdir().unwrap();
        let (projector, parts) = launched(dir.path(), None);
        write_sub(dir.path(), &[CLAUDE_SUB_BASH, CLAUDE_SUB_REPORT]);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let mut sink = RecordingSink::new(log.clone());
        sink.fail_push = Some(0);
        let mut clock = ScriptedClock::new(Duration::from_secs(1));
        let end = run(
            projector,
            parts,
            &mut sink,
            &mut clock,
            &AtomicBool::new(false),
        );
        assert_eq!(end, FollowEnd::AllFinished);
        let log = log.lock().unwrap();
        assert_eq!(log.completes, [CotCompleteReason::Done]);
        assert_eq!(log.releases, [true]);
    }

    /// 推送按 600ms 节流:拍子更密时攒着一起推。
    #[test]
    fn pushes_are_throttled() {
        let dir = tempfile::tempdir().unwrap();
        let (projector, parts) = launched(dir.path(), None);
        write_sub(dir.path(), &[CLAUDE_SUB_BASH]);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let mut sink = RecordingSink::new(log.clone());
        let mut clock = ScriptedClock::new(Duration::from_millis(250));
        for index in 0..8 {
            let sub = dir.path().to_path_buf();
            clock.script.push_back(Box::new(move || {
                let line = if index == 7 {
                    CLAUDE_SUB_REPORT
                } else {
                    CLAUDE_SUB_BASH
                };
                write_sub(&sub, &[line]);
            }));
        }
        let end = run(
            projector,
            parts,
            &mut sink,
            &mut clock,
            &AtomicBool::new(false),
        );
        assert_eq!(end, FollowEnd::AllFinished);
        let log = log.lock().unwrap();
        // 每拍都有新行,但只有距上次推送满 600ms 的才推:第 0、3、6 拍,加收口那一次。
        assert_eq!(log.pushes.len(), 4, "{:?}", log.pushes.len());
    }

    /// 析构即通知跟随线程收口(会话拆除时 chat 线程退出,这份登记随之析构)。
    #[test]
    fn dropping_the_registry_stops_its_followers() {
        let dir = tempfile::tempdir().unwrap();
        // 首拍推完后它停在等下一拍;停止信号先于放行,下一拍一定先看见它。
        let (registry, gate_tx, log) = one_follower(dir.path());
        let stops = registry.stop_flags();
        assert_eq!(stops.len(), 1);
        assert!(!stops[0].load(Ordering::Relaxed));
        drop(registry);
        assert!(stops[0].load(Ordering::Relaxed), "析构即置停止信号");
        drop(gate_tx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while log.lock().unwrap().releases.is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let log = log.lock().unwrap();
        assert_eq!(log.releases, [true], "析构后跟随线程应自行收口");
        assert_eq!(log.count("RUN_FINISHED"), 1);
        assert!(reasoning(&log.events()).contains("（仍在运行）"));
    }

    /// 跟随线程中途 panic:恢复记录照样放手(`release(false)`),交给反熵收口 —— 否则它一直算
    /// 本进程 active,开新卡前的反熵不收它,这条 CoT 就一直转着。panic 之后不再碰远端。
    #[test]
    fn a_panicking_follower_hands_its_record_to_anti_entropy() {
        let dir = tempfile::tempdir().unwrap();
        let (projector, parts) = launched(dir.path(), None);
        write_sub(dir.path(), &[CLAUDE_SUB_BASH]);
        let log = Arc::new(std::sync::Mutex::new(SinkLog::default()));
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let sink_log = log.clone();
        let registry = followers_with(gate_rx, move || {
            let mut sink = RecordingSink::new(sink_log.clone());
            sink.panic_push = true;
            sink
        });
        registry.spawn(
            DetachedCot {
                carrier: CotCarrier::for_test("cot_follow", "om_follow"),
                projector,
            },
            parts,
        );
        assert_eq!(drive_to_end(&registry, &gate_tx), [FollowEnd::Failed]);
        let log = log.lock().unwrap();
        assert_eq!(log.releases, [false]);
        assert!(log.completes.is_empty(), "{:?}", log.completes);
        assert!(log.pushes.is_empty());
    }
}
