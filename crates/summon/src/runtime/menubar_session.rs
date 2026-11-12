//! Menu Bar 的 Exit / Archive:结束一条会话,等到它确实结束再返回。
//!
//! 动手的是 daemon:CLI 经 control.sock 投 `Intent::EndChatSession`,落到与管家卡「退出 / 📦 归档」
//! 同一条 dispatcher 通道。这里只多做两件 Menu Bar 需要的事:
//!
//! - **在跑的轮先问人。** 菜单开着时不自动刷新,它手里的 busy 可能是几分钟前的。所以动手前按此刻的
//!   daemon 真值再判一次,在跑且调用方没表明「打断也行」就不动手,回 `running` 让菜单弹确认。
//! - **等到结束再返回。** 拆除在 chat 线程里异步进行;菜单要的是「完成后刷新」,刷早了看到的还是旧状态。
//!   判据是这条会话从 fleet 列表里消失(`fleet::is_listed`,与菜单看到的列表同一个口径):拆除写下
//!   路由退休、claude 的 tmux 随之关掉,发现层就把它判成 Gone;无头会话的进程摘出会话池,它就不在列表里。

use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Result};
use summon_core::core_port::{Actor, Command, CommandOutcome, Intent};
use summon_core::fleet::RuntimeStatus;
use summon_core::ids::ChatId;
use summon_core::registry::Agent;

/// 结束会话的结果。Menu Bar 按 JSON 里的词给人反馈,词是契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EndSessionOutcome {
    /// 会话已从列表里消失。
    Ended,
    /// 此刻正在跑一轮,没有动手;带上「打断也行」再来一次才执行。
    Running,
    /// 已经交给 daemon,但在等待时限内还没看到它结束。
    Pending,
    /// 列表里本来就没有这条会话,没有可结束的。
    Absent,
}

/// 「列表里还有没有它」用的等待时限与查看间隔。打断一轮再拆除通常一两秒,时限给到能覆盖慢收尾。
const WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(250);

/// 流程里碰外部世界的几处,抽出来便于测试。
pub(crate) trait EndSessionPort {
    /// 这条会话此刻还在列表里吗:不在 → `None`;在 → 它的 chat_id(缺 `.meta` 时为 `None`)。
    fn listed(&self) -> Option<Option<String>>;
    /// daemon 的运行时真值;拿不到 = daemon 没在跑。
    fn runtime(&self) -> Option<RuntimeStatus>;
    fn submit(&self, cmd: &Command) -> Result<CommandOutcome>;
    fn sleep(&self, d: Duration);
    fn now(&self) -> Instant;
}

pub(crate) fn end_session(
    port: &impl EndSessionPort,
    alias: &str,
    tag: &str,
    archive: bool,
    interrupt: bool,
) -> Result<EndSessionOutcome> {
    let Some(chat_id) = port.listed() else {
        return Ok(EndSessionOutcome::Absent);
    };
    let Some(chat_id) = chat_id else {
        bail!(summon_i18n::format!("ra3c423a526"));
    };
    let Some(runtime) = port.runtime() else {
        bail!(summon_i18n::format!("r5b7fed80e2"));
    };
    let busy = runtime
        .busy
        .iter()
        .any(|turn| turn.agent == alias && turn.tag == tag);
    if busy && !interrupt {
        return Ok(EndSessionOutcome::Running);
    }
    let cmd = Command {
        actor: Actor::Owner,
        intent: Intent::EndChatSession {
            alias: alias.to_string(),
            chat_id: ChatId::new(chat_id),
            archive,
        },
    };
    match port.submit(&cmd)? {
        CommandOutcome::Done => {}
        CommandOutcome::Denied(reason) => bail!(summon_i18n::format!("r8d6dac91e5")),
        other => bail!(summon_i18n::format!("rd38e7dff0f")),
    }
    let deadline = port.now() + WAIT;
    loop {
        if port.listed().is_none() {
            return Ok(EndSessionOutcome::Ended);
        }
        if port.now() >= deadline {
            return Ok(EndSessionOutcome::Pending);
        }
        port.sleep(POLL);
    }
}

/// 真实的外部世界:盘上会话发现 + control.sock。
pub(crate) struct LivePort<'a> {
    pub(crate) agent: &'a Agent,
    pub(crate) tag: &'a str,
}

/// 这条会话此刻在不在「给人看的列表」里(与 fleet 快照同一道闸:Menu Bar 上看得到的会话,这里也
/// 认得到);在 → 它的 chat_id。
fn listed_with(
    agent: &Agent,
    tag: &str,
    runtime: Option<&RuntimeStatus>,
) -> Option<Option<String>> {
    let all = crate::session::tmux::list_all_sessions();
    let live = crate::session::tmux::filter_names(&all, &agent.tmux_name());
    let ttl = crate::runtime::daemon::resolve_messaging(agent).idle_ttl();
    crate::session::discovery::discover_sessions(agent, &live, SystemTime::now(), ttl)
        .into_iter()
        .find(|view| view.tag == tag && summon_core::fleet::is_listed(&agent.name, view, runtime))
        .map(|view| view.chat_id)
}

impl EndSessionPort for LivePort<'_> {
    fn listed(&self) -> Option<Option<String>> {
        // 无头会话在不在列表里要 daemon 作证(进程活着 / 在跑一轮),每次查都现问。不复用动手前
        // 取的那份:拆除先把进程摘出会话池、再写路由退休(写失败只记日志),用旧名单就只剩后者
        // 一个证据,它没写成时会把已经结束的会话拖成 pending。每一拍本来就要起一次 tmux、扫一遍盘,
        // 多一次本机 control.sock 往返不是大头。
        let runtime = crate::observe::fleet_snapshot::fetch_runtime_status();
        listed_with(self.agent, self.tag, runtime.as_ref())
    }

    fn runtime(&self) -> Option<RuntimeStatus> {
        crate::observe::fleet_snapshot::fetch_runtime_status()
    }

    fn submit(&self, cmd: &Command) -> Result<CommandOutcome> {
        crate::runtime::daemon::submit_command(cmd)
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// daemon 进程里的同一套(控制台的会话页):运行时真值读自己的内存,命令直接交给 Core,不绕
/// control.sock 问自己。
pub(crate) struct InProcessPort<'a> {
    pub(crate) core: &'a crate::runtime::core::Core,
    pub(crate) agent: &'a Agent,
    pub(crate) tag: &'a str,
}

impl EndSessionPort for InProcessPort<'_> {
    fn listed(&self) -> Option<Option<String>> {
        listed_with(self.agent, self.tag, self.runtime().as_ref())
    }

    fn runtime(&self) -> Option<RuntimeStatus> {
        Some(crate::observe::runtime_status::snapshot(
            std::process::id(),
            Some(crate::version::VERSION.to_string()),
            SystemTime::now(),
        ))
    }

    fn submit(&self, cmd: &Command) -> Result<CommandOutcome> {
        Ok(summon_core::core_port::CommandSink::submit(
            self.core,
            cmd.clone(),
        ))
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use summon_core::fleet::BusyTurn;

    /// `listed` 按调用次序依次吐出预设值,用完后停在最后一个。
    struct FakePort {
        listed: RefCell<Vec<Option<Option<String>>>>,
        busy: Vec<BusyTurn>,
        daemon_up: bool,
        reply: CommandOutcome,
        submitted: RefCell<Vec<Command>>,
        clock: Cell<Duration>,
    }

    impl FakePort {
        fn new(listed: Vec<Option<Option<String>>>) -> Self {
            Self {
                listed: RefCell::new(listed),
                busy: Vec::new(),
                daemon_up: true,
                reply: CommandOutcome::Done,
                submitted: RefCell::new(Vec::new()),
                clock: Cell::new(Duration::ZERO),
            }
        }

        fn origin() -> Instant {
            static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            *ORIGIN.get_or_init(Instant::now)
        }
    }

    impl EndSessionPort for FakePort {
        fn listed(&self) -> Option<Option<String>> {
            let mut queue = self.listed.borrow_mut();
            if queue.len() > 1 {
                queue.remove(0)
            } else {
                queue[0].clone()
            }
        }
        fn runtime(&self) -> Option<RuntimeStatus> {
            self.daemon_up.then(|| RuntimeStatus {
                pid: 1,
                version: None,
                uptime_secs: 1,
                busy: self.busy.clone(),
                credentials: Vec::new(),
                live_headless: Some(Vec::new()),
                subagents: Vec::new(),
            })
        }
        fn submit(&self, cmd: &Command) -> Result<CommandOutcome> {
            self.submitted.borrow_mut().push(cmd.clone());
            Ok(self.reply.clone())
        }
        fn sleep(&self, d: Duration) {
            self.clock.set(self.clock.get() + d);
        }
        fn now(&self) -> Instant {
            Self::origin() + self.clock.get()
        }
    }

    fn listed(chat_id: &str) -> Option<Option<String>> {
        Some(Some(chat_id.to_string()))
    }

    fn busy(agent: &str, tag: &str) -> BusyTurn {
        BusyTurn {
            agent: agent.into(),
            tag: tag.into(),
        }
    }

    #[test]
    fn exit_submits_to_the_daemon_and_waits_until_the_session_is_gone() {
        let port = FakePort::new(vec![listed("oc_room"), listed("oc_room"), None]);
        let outcome = end_session(&port, "starlight", "g1", false, false).unwrap();
        assert_eq!(outcome, EndSessionOutcome::Ended);
        let submitted = port.submitted.borrow();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].actor, Actor::Owner);
        assert_eq!(
            submitted[0].intent,
            Intent::EndChatSession {
                alias: "starlight".into(),
                chat_id: ChatId::new("oc_room"),
                archive: false,
            }
        );
        assert!(port.clock.get() > Duration::ZERO, "结束前要等过至少一拍");
    }

    #[test]
    fn archive_carries_the_archive_flag() {
        let port = FakePort::new(vec![listed("oc_room"), None]);
        assert_eq!(
            end_session(&port, "starlight", "g1", true, false).unwrap(),
            EndSessionOutcome::Ended
        );
        assert!(matches!(
            port.submitted.borrow()[0].intent,
            Intent::EndChatSession { archive: true, .. }
        ));
    }

    /// 菜单上看着空闲、此刻却在跑:不动手,交回去让人确认。
    #[test]
    fn a_running_turn_is_left_alone_without_interrupt() {
        let mut port = FakePort::new(vec![listed("oc_room")]);
        port.busy = vec![busy("starlight", "g1")];
        assert_eq!(
            end_session(&port, "starlight", "g1", false, false).unwrap(),
            EndSessionOutcome::Running
        );
        assert!(port.submitted.borrow().is_empty());
    }

    #[test]
    fn a_running_turn_is_ended_when_interrupt_is_allowed() {
        let mut port = FakePort::new(vec![listed("oc_room"), None]);
        port.busy = vec![busy("starlight", "g1")];
        assert_eq!(
            end_session(&port, "starlight", "g1", true, true).unwrap(),
            EndSessionOutcome::Ended
        );
        assert_eq!(port.submitted.borrow().len(), 1);
    }

    /// 同一个 agent 别的会话在跑、或别的 agent 在同一个群里跑,都不算这条会话在跑。
    #[test]
    fn busy_is_matched_on_both_agent_and_tag() {
        let mut port = FakePort::new(vec![listed("oc_room"), None]);
        port.busy = vec![busy("starlight", "g2"), busy("saturn", "g1")];
        assert_eq!(
            end_session(&port, "starlight", "g1", false, false).unwrap(),
            EndSessionOutcome::Ended
        );
    }

    #[test]
    fn a_session_not_in_the_list_is_absent_and_nothing_is_sent() {
        let port = FakePort::new(vec![None]);
        assert_eq!(
            end_session(&port, "starlight", "g1", true, false).unwrap(),
            EndSessionOutcome::Absent
        );
        assert!(port.submitted.borrow().is_empty());
    }

    #[test]
    fn a_session_without_chat_id_is_an_error_and_nothing_is_sent() {
        let port = FakePort::new(vec![Some(None)]);
        assert!(end_session(&port, "starlight", "g1", false, false).is_err());
        assert!(port.submitted.borrow().is_empty());
    }

    #[test]
    fn daemon_down_is_an_error_and_nothing_is_sent() {
        let mut port = FakePort::new(vec![listed("oc_room")]);
        port.daemon_up = false;
        assert!(end_session(&port, "starlight", "g1", false, false).is_err());
        assert!(port.submitted.borrow().is_empty());
    }

    #[test]
    fn a_denied_command_is_an_error() {
        let mut port = FakePort::new(vec![listed("oc_room")]);
        port.reply = CommandOutcome::Denied("无此 agent 路由".into());
        let error = end_session(&port, "starlight", "g1", false, false).unwrap_err();
        assert!(format!("{error:#}").contains("无此 agent 路由"));
    }

    #[test]
    fn a_session_still_listed_after_the_wait_is_pending() {
        let port = FakePort::new(vec![listed("oc_room")]);
        assert_eq!(
            end_session(&port, "starlight", "g1", false, false).unwrap(),
            EndSessionOutcome::Pending
        );
        assert!(port.clock.get() >= WAIT);
    }

    #[test]
    fn outcome_words_are_the_menubar_contract() {
        for (outcome, word) in [
            (EndSessionOutcome::Ended, "\"ended\""),
            (EndSessionOutcome::Running, "\"running\""),
            (EndSessionOutcome::Pending, "\"pending\""),
            (EndSessionOutcome::Absent, "\"absent\""),
        ] {
            assert_eq!(serde_json::to_string(&outcome).unwrap(), word);
        }
    }
}
