//! 浏览器 → 窗格的两条路:按键(只有可操作页有)与滚动(两种页面都有)。
//!
//! 只读页结构上拿不到按键那条路:它的 [`PageInput`] 里只有一个 [`ScrollPane`],而 `ScrollPane`
//! 没有任何接收调用方字节的方法——它只收格数,送进窗格的滚轮事件由 `WheelEvents` 按固定格式生成
//! (owner 2026-09-23:只读页「只能送滚动事件，不能输入文本，enter之类的」)。两种页面都拿不到 pty
//! 句柄:读 pty 输出的线程握着 master 的一份副本,那条路径只读。

use std::io;
use std::time::Instant;

use summon_core::web_terminal::grants::TerminalMode;
use summon_core::web_terminal::scroll::{NetScroll, PaneMouse, ScrollRate, WheelEvents};

use crate::session::tmux::{PaneId, Tmux, WheelSend};

/// 可操作页的按键去处:目标会话的当前窗格。
pub(super) struct KeyPane {
    tmux: Tmux,
}

impl KeyPane {
    /// 按键字节原样送进窗格(见 [`Tmux::send_bytes`])。
    pub(super) fn send(&self, bytes: &[u8]) -> io::Result<()> {
        self.tmux.send_bytes(bytes)
    }

    pub(super) fn has_session(&self) -> bool {
        self.tmux.has_session()
    }
}

/// 滚轮事件的去处:查窗格、抓画面、往查到的窗格送。生产里就是目标会话的 tmux;测试换成假的,
/// 数查了几次、记下送到了哪。
pub(super) trait WheelTarget {
    /// 会话此刻的当前窗格及其状态。
    fn query(&self) -> io::Result<(PaneId, PaneMouse)>;
    /// 指定窗格的状态;窗格没了就 Err。
    fn query_pane(&self, pane: &PaneId) -> io::Result<PaneMouse>;
    /// 指定窗格此刻的可见画面;抓不到就 Err。
    fn capture(&self, pane: &PaneId) -> io::Result<String>;
    fn send(&self, pane: &PaneId, events: &WheelEvents) -> io::Result<WheelSend>;
}

impl WheelTarget for Tmux {
    fn query(&self) -> io::Result<(PaneId, PaneMouse)> {
        self.pane_mouse()
    }

    fn query_pane(&self, pane: &PaneId) -> io::Result<PaneMouse> {
        pane.mouse()
    }

    fn capture(&self, pane: &PaneId) -> io::Result<String> {
        pane.capture_visible()
    }

    fn send(&self, pane: &PaneId, events: &WheelEvents) -> io::Result<WheelSend> {
        self.send_wheel(pane, events)
    }
}

/// 滚动的去处:目标会话的当前窗格。只收格数。
pub(super) struct ScrollPane<T: WheelTarget = Tmux> {
    target: T,
    rate: ScrollRate,
    /// 这条连接净翻了多少格,连接结束时据此翻回去(见 [`Self::restore`])。
    net: NetScroll,
    /// 这条连接确实送出去的滚动落在了哪些窗格:翻回去只往那一个窗格翻。
    panes: ScrolledPanes,
}

/// 一条连接的滚动落在了哪。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum ScrolledPanes {
    #[default]
    None,
    One(PaneId),
    /// 落在过不止一个窗格(期间当前窗格换过):翻回去翻哪个、各翻几格都说不清,整个不翻。
    Several,
}

impl ScrolledPanes {
    fn record(&mut self, pane: &PaneId) {
        *self = match std::mem::take(self) {
            Self::None => Self::One(pane.clone()),
            Self::One(p) if p == *pane => Self::One(p),
            Self::One(_) | Self::Several => Self::Several,
        };
    }
}

/// 连接结束时翻回去的结果,只用来写日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Restore {
    /// 没往上翻过(净往下或为 0),不用翻。
    NotNeeded,
    /// 往下翻回去了这么多格;某一批闸不过或出错时停在半路。
    Scrolled(u32),
    /// 一格都没翻:原因。
    Skipped(&'static str),
}

impl std::fmt::Display for Restore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotNeeded => f.write_str("0"),
            Self::Scrolled(n) => write!(f, "{n}"),
            Self::Skipped(why) => write!(f, "{}", summon_i18n::format!("r0388d77dfc")),
        }
    }
}

/// 一次滚动的结果,只用来计数写日志;页面那边什么都不知道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scrolled {
    Sent,
    Dropped,
}

impl<T: WheelTarget> ScrollPane<T> {
    fn new(target: T) -> Self {
        Self {
            target,
            rate: ScrollRate::default(),
            net: NetScroll::default(),
            panes: ScrolledPanes::default(),
        }
    }

    /// 翻 `notches` 格:先过限频,再查一次窗格,窗格收滚轮才送。任何一步不成都静默丢掉,
    /// 不结束连接——滚不动只是这一下没滚。只有确实送出去的才记进净格数:tmux 那边的闸不过
    /// ([`WheelSend::Refused`])什么都没送,算丢掉。
    pub(super) fn scroll(&mut self, notches: i64, now: Instant) -> Scrolled {
        // 限频在查 tmux 之前:被限掉的那次不起任何 tmux 进程。
        if !self.rate.allow(now) {
            return Scrolled::Dropped;
        }
        let Some((pane, events)) = wheel_for(self.target.query(), notches) else {
            return Scrolled::Dropped;
        };
        match self.target.send(&pane, &events) {
            Ok(WheelSend::Sent) => {
                self.net.record(&events);
                self.panes.record(&pane);
                Scrolled::Sent
            }
            Ok(WheelSend::Refused) | Err(_) => Scrolled::Dropped,
        }
    }

    /// 连接结束时调用:这条连接净往上翻过,就往下翻回去同样的格数。
    ///
    /// Claude Code 翻在历史里时新输出来了也停在原处,而注入闸、轮内的菜单识别读的都是当前屏幕;
    /// 页面关掉就把它留在历史里,那些判据读到的是历史。
    /// - 只翻这条连接的滚动落在的那一个窗格(按窗格 id 查、送);落在过不止一个窗格就整个不翻,
    ///   窗格没了也不翻。
    /// - 先抓一屏:屏上有待选的菜单就整个不翻,抓不到也不翻。飞书卡片上的选择按「光标在首项、
    ///   往下按几次」注入;滚轮要是挪动了菜单光标,之后卡片上选的就成了另一项。
    /// - 每批(至多 20 格)各查一次窗格、过同一道闸;闸不过(本地有人进了 copy-mode 等)或出错就
    ///   停下,绝不替人退出 copy-mode。不受限频管:只在连接结束时跑一次,总共至多 200 格。
    pub(super) fn restore(&mut self) -> Restore {
        let batches = std::mem::take(&mut self.net).restore_batches();
        let panes = std::mem::take(&mut self.panes);
        if batches.is_empty() {
            return Restore::NotNeeded;
        }
        let pane = match panes {
            ScrolledPanes::One(pane) => pane,
            ScrolledPanes::Several => return Restore::Skipped(summon_i18n::text!("ra8ba08a72d")),
            // 净往上翻过就一定记下了窗格;走到这里是账记错了,不翻。
            ScrolledPanes::None => return Restore::Skipped(summon_i18n::text!("r3ebb59754d")),
        };
        if let Some(why) = restore_blocked_by_screen(self.target.capture(&pane)) {
            return Restore::Skipped(why);
        }
        let mut restored = 0;
        for batch in batches {
            let Ok(mouse) = self.target.query_pane(&pane) else {
                break;
            };
            let Some(events) = WheelEvents::for_restore(&mouse, batch) else {
                break;
            };
            if !matches!(self.target.send(&pane, &events), Ok(WheelSend::Sent)) {
                break;
            }
            restored += batch;
        }
        Restore::Scrolled(restored)
    }
}

/// 翻回去之前抓的那一屏挡不挡翻回去:抓不到(窗格没了、tmux 出错)或抓回空屏,证明不了屏上没有
/// 菜单,挡;认得出一张待选的菜单(与飞书选择卡核对菜单用的同一个解析),挡。返回挡住的原因。
fn restore_blocked_by_screen(captured: io::Result<String>) -> Option<&'static str> {
    let Ok(screen) = captured else {
        return Some(summon_i18n::text!("r842976575a"));
    };
    if screen.trim().is_empty() {
        return Some(summon_i18n::text!("r6caf14b36b"));
    }
    summon_core::interactive::parse_interactive(&screen)
        .is_some()
        .then_some(summon_i18n::text!("r0967680df5"))
}

/// 查询结果 → 该送的事件。查询出错、窗格不收滚轮(在模式里、开着同步、没开 SGR、一种跟踪都没开)、
/// 格数为 0,都不送。
fn wheel_for(
    queried: io::Result<(PaneId, PaneMouse)>,
    notches: i64,
) -> Option<(PaneId, WheelEvents)> {
    let (pane, mouse) = queried.ok()?;
    let events = WheelEvents::for_pane(&mouse, notches)?;
    Some((pane, events))
}

/// 一条连接能往窗格送什么,按页面模式装配。
pub(super) enum PageInput {
    ReadOnly { scroll: ScrollPane },
    Operate { keys: KeyPane, scroll: ScrollPane },
}

impl PageInput {
    pub(super) fn for_mode(mode: TerminalMode, session: &str) -> Self {
        let scroll = ScrollPane::new(Tmux::new(session.to_string()));
        match mode {
            TerminalMode::ReadOnly => Self::ReadOnly { scroll },
            TerminalMode::Operate => Self::Operate {
                keys: KeyPane {
                    tmux: Tmux::new(session.to_string()),
                },
                scroll,
            },
        }
    }

    /// 按键的去处;只读页没有。
    pub(super) fn keys(&self) -> Option<&KeyPane> {
        match self {
            Self::ReadOnly { .. } => None,
            Self::Operate { keys, .. } => Some(keys),
        }
    }

    /// 滚动的去处;两种页面都有。
    pub(super) fn scroll(&mut self) -> &mut ScrollPane {
        match self {
            Self::ReadOnly { scroll } | Self::Operate { scroll, .. } => scroll,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    fn open() -> PaneMouse {
        PaneMouse {
            in_mode: false,
            sgr: true,
            any: true,
            button: false,
            standard: true,
            synchronized: false,
            width: 80,
            height: 24,
        }
    }

    fn in_mode() -> PaneMouse {
        PaneMouse {
            in_mode: true,
            ..open()
        }
    }

    fn pane() -> PaneId {
        PaneId::parse("%7").unwrap()
    }

    /// 屏上一张等人选的授权对话框(与 `parse_interactive` 的夹具同形)。
    const MENU_SCREEN: &str = " Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel\n";
    /// 屏上只有输入框。
    const PROMPT_SCREEN: &str = "⏺ 好的\n\n────────\n❯ \n────────\n";

    /// 假的去处:两种查询共用一个按序给出的窗格状态脚本(用完就一直给最后一个);会话的当前窗格
    /// 可以换;`gone` 里的窗格查不到;送按序号脚本失败或闸不过。记下查了几次、送到了哪些窗格、各
    /// 送了几格。
    struct FakeTarget {
        queries: Cell<usize>,
        states: Vec<PaneMouse>,
        current: RefCell<PaneId>,
        gone: RefCell<Vec<PaneId>>,
        screen: RefCell<Option<String>>,
        fail_sends: Vec<usize>,
        refuse_sends: Vec<usize>,
        sends: Cell<usize>,
        sent: RefCell<Vec<(PaneId, i64)>>,
    }

    impl FakeTarget {
        fn new() -> Self {
            Self {
                queries: Cell::new(0),
                states: vec![open()],
                current: RefCell::new(pane()),
                gone: RefCell::new(Vec::new()),
                screen: RefCell::new(Some(PROMPT_SCREEN.to_string())),
                fail_sends: Vec::new(),
                refuse_sends: Vec::new(),
                sends: Cell::new(0),
                sent: RefCell::new(Vec::new()),
            }
        }

        fn next_state(&self) -> PaneMouse {
            let n = self.queries.get();
            self.queries.set(n + 1);
            self.states[n.min(self.states.len() - 1)]
        }

        fn notches(&self) -> Vec<i64> {
            self.sent.borrow().iter().map(|(_, n)| *n).collect()
        }
    }

    impl WheelTarget for FakeTarget {
        fn query(&self) -> io::Result<(PaneId, PaneMouse)> {
            Ok((self.current.borrow().clone(), self.next_state()))
        }

        fn query_pane(&self, pane: &PaneId) -> io::Result<PaneMouse> {
            if self.gone.borrow().contains(pane) {
                return Err(io::Error::other("窗格没了"));
            }
            Ok(self.next_state())
        }

        fn capture(&self, pane: &PaneId) -> io::Result<String> {
            if self.gone.borrow().contains(pane) {
                return Err(io::Error::other("窗格没了"));
            }
            self.screen
                .borrow()
                .clone()
                .ok_or_else(|| io::Error::other("抓不到"))
        }

        fn send(&self, pane: &PaneId, events: &WheelEvents) -> io::Result<WheelSend> {
            let n = self.sends.get();
            self.sends.set(n + 1);
            if self.fail_sends.contains(&n) {
                return Err(io::Error::other("送不进去"));
            }
            if self.refuse_sends.contains(&n) {
                return Ok(WheelSend::Refused);
            }
            self.sent
                .borrow_mut()
                .push((pane.clone(), events.notches()));
            Ok(WheelSend::Sent)
        }
    }

    #[test]
    fn only_the_operate_page_has_a_keystroke_path_and_both_scroll_the_target_pane() {
        let mut ro = PageInput::for_mode(TerminalMode::ReadOnly, "s");
        assert!(ro.keys().is_none(), "只读页没有按键的去处");
        assert_eq!(ro.scroll().target.session, "s", "只读页能滚");
        let mut op = PageInput::for_mode(TerminalMode::Operate, "s");
        assert_eq!(op.keys().map(|k| k.tmux.session.as_str()), Some("s"));
        assert_eq!(op.scroll().target.session, "s");
    }

    /// 查询出错、窗格在模式里、开着同步、没开 SGR、一种跟踪都没开,都不送;都满足才送,送到查到
    /// 的那个窗格。
    #[test]
    fn a_scroll_is_sent_only_when_the_query_proves_the_pane_takes_the_wheel() {
        let refused = |mouse: PaneMouse| wheel_for(Ok((pane(), mouse)), -2).is_none();
        assert!(wheel_for(Err(io::Error::other("tmux 不在")), -2).is_none());
        assert!(refused(in_mode()));
        assert!(refused(PaneMouse {
            synchronized: true,
            ..open()
        }));
        assert!(refused(PaneMouse {
            sgr: false,
            ..open()
        }));
        assert!(refused(PaneMouse {
            any: false,
            button: false,
            standard: false,
            ..open()
        }));
        assert!(wheel_for(Ok((pane(), open())), 0).is_none(), "0 格");
        let (to, events) = wheel_for(Ok((pane(), open())), -2).unwrap();
        assert_eq!(to, pane());
        assert_eq!(events.as_bytes(), b"\x1b[<64;40;12M\x1b[<64;40;12M");
    }

    /// 限频在查 tmux 之前:一秒内第 31 次不查窗格,被限掉的那次不起任何 tmux 进程。
    #[test]
    fn a_rate_limited_scroll_never_queries_the_pane() {
        let mut scroll = ScrollPane::new(FakeTarget::new());
        let now = Instant::now();
        for _ in 0..30 {
            assert_eq!(scroll.scroll(-1, now), Scrolled::Sent);
        }
        assert_eq!(scroll.target.queries.get(), 30);
        assert_eq!(scroll.scroll(-1, now), Scrolled::Dropped);
        assert_eq!(scroll.target.queries.get(), 30, "被限掉的那次没查窗格");
        assert_eq!(
            scroll.scroll(-1, now + Duration::from_secs(1)),
            Scrolled::Sent
        );
        assert_eq!(scroll.target.queries.get(), 31);
    }

    /// 只有确实送出去的才算进净格数:送失败的、Rust 这边的闸挡下的、tmux 那边的闸不过的都不算。
    /// 净往上才翻回去,往下翻回去同样的格数。
    #[test]
    fn only_scrolls_actually_sent_are_restored_and_only_upward_ones() {
        let t0 = Instant::now();
        let mut scroll = ScrollPane::new(FakeTarget {
            // 第 3 次查询时在模式里;第 2 次送失败,第 3 次送时 tmux 那边闸不过。
            states: vec![open(), open(), in_mode(), open()],
            fail_sends: vec![1],
            refuse_sends: vec![2],
            ..FakeTarget::new()
        });
        assert_eq!(scroll.scroll(-3, t0), Scrolled::Sent);
        assert_eq!(scroll.scroll(-5, t0), Scrolled::Dropped, "送失败");
        assert_eq!(scroll.scroll(-5, t0), Scrolled::Dropped, "在模式里");
        assert_eq!(scroll.scroll(-4, t0), Scrolled::Dropped, "tmux 那边闸不过");
        assert_eq!(scroll.scroll(1, t0), Scrolled::Sent);
        assert_eq!(scroll.restore(), Restore::Scrolled(2));
        assert_eq!(scroll.target.notches(), vec![-3, 1, 2]);
        assert_eq!(scroll.restore(), Restore::NotNeeded, "翻回去之后净格数归零");

        let mut down = ScrollPane::new(FakeTarget::new());
        assert_eq!(down.scroll(3, t0), Scrolled::Sent);
        assert_eq!(down.scroll(-1, t0), Scrolled::Sent);
        assert_eq!(down.restore(), Restore::NotNeeded, "净往下不翻回去");
        assert_eq!(down.target.notches(), vec![3, -1]);
    }

    /// 翻回去每批至多 20 格、总共至多 200 格,不受限频管;某一批闸不过(Rust 这边或 tmux 那边)
    /// 就停,后面的不再查、不再送。
    #[test]
    fn a_restore_is_capped_batched_and_stops_at_the_first_refusal() {
        let t0 = Instant::now();
        let mut far = ScrollPane::new(FakeTarget::new());
        for i in 0..60u64 {
            let at = t0 + Duration::from_secs(i / 30);
            assert_eq!(far.scroll(-5, at), Scrolled::Sent);
        }
        let queries = far.target.queries.get();
        assert_eq!(far.restore(), Restore::Scrolled(200));
        assert_eq!(far.target.queries.get() - queries, 10, "每批查一次窗格");
        assert_eq!(far.target.notches()[60..], [20; 10]);

        let mut refused = ScrollPane::new(FakeTarget {
            // 滚动时查 9 次;翻回去第 1 批查到开着,第 2 批查到在模式里,之后又开着——停下的就不会
            // 再去查第 3 批。
            states: [vec![open(); 10], vec![in_mode()], vec![open()]].concat(),
            ..FakeTarget::new()
        });
        for _ in 0..9 {
            assert_eq!(refused.scroll(-5, t0), Scrolled::Sent);
        }
        assert_eq!(
            refused.restore(),
            Restore::Scrolled(20),
            "第 2 批闸不过就停"
        );
        assert_eq!(refused.target.notches()[9..], [20]);
        assert_eq!(refused.target.queries.get(), 11, "停下之后不再查");

        let mut tmux_refused = ScrollPane::new(FakeTarget {
            // 滚动送 9 次;翻回去第 2 批送时 tmux 那边闸不过。
            refuse_sends: vec![10],
            ..FakeTarget::new()
        });
        for _ in 0..9 {
            assert_eq!(tmux_refused.scroll(-5, t0), Scrolled::Sent);
        }
        assert_eq!(tmux_refused.restore(), Restore::Scrolled(20));
        assert_eq!(tmux_refused.target.sends.get(), 11, "闸不过之后不再送");
    }

    /// 翻回去之前先抓一屏:屏上有待选的菜单、抓不到、抓回空屏,都整个不翻;只有输入框时照翻。
    #[test]
    fn a_restore_is_skipped_when_the_screen_shows_a_menu_or_cannot_be_captured() {
        assert_eq!(
            restore_blocked_by_screen(Ok(MENU_SCREEN.to_string())),
            Some("屏上有待选的菜单")
        );
        assert!(restore_blocked_by_screen(Err(io::Error::other("tmux 出错"))).is_some());
        assert!(restore_blocked_by_screen(Ok("  \n".to_string())).is_some());
        assert_eq!(
            restore_blocked_by_screen(Ok(PROMPT_SCREEN.to_string())),
            None
        );

        let t0 = Instant::now();
        for (screen, expected) in [
            (Some(MENU_SCREEN), Restore::Skipped("屏上有待选的菜单")),
            (None, Restore::Skipped("抓不到窗格画面")),
            (Some(PROMPT_SCREEN), Restore::Scrolled(3)),
        ] {
            let mut scroll = ScrollPane::new(FakeTarget::new());
            assert_eq!(scroll.scroll(-3, t0), Scrolled::Sent);
            *scroll.target.screen.borrow_mut() = screen.map(str::to_string);
            assert_eq!(scroll.restore(), expected, "{screen:?}");
            let restored: Vec<i64> = scroll.target.notches()[1..].to_vec();
            let want = if matches!(expected, Restore::Scrolled(_)) {
                vec![3]
            } else {
                vec![]
            };
            assert_eq!(restored, want, "{screen:?}");
        }
    }

    /// 翻回去只往滚动落在的那个窗格翻,即使会话此刻的当前窗格已经换了;滚动落在过不止一个窗格、
    /// 或那个窗格没了,整个不翻。
    #[test]
    fn a_restore_goes_only_to_the_one_pane_the_connection_scrolled() {
        let t0 = Instant::now();
        let other = PaneId::parse("%8").unwrap();

        let mut moved = ScrollPane::new(FakeTarget::new());
        assert_eq!(moved.scroll(-3, t0), Scrolled::Sent);
        *moved.target.current.borrow_mut() = other.clone();
        assert_eq!(moved.restore(), Restore::Scrolled(3));
        assert_eq!(
            *moved.target.sent.borrow(),
            vec![(pane(), -3), (pane(), 3)],
            "当前窗格换了也翻回原来那个"
        );

        let mut several = ScrollPane::new(FakeTarget::new());
        assert_eq!(several.scroll(-3, t0), Scrolled::Sent);
        *several.target.current.borrow_mut() = other.clone();
        assert_eq!(several.scroll(-2, t0), Scrolled::Sent);
        assert_eq!(
            several.restore(),
            Restore::Skipped("滚动落在过不止一个窗格")
        );
        assert_eq!(several.target.notches(), vec![-3, -2], "一格都没翻");

        let mut gone = ScrollPane::new(FakeTarget::new());
        assert_eq!(gone.scroll(-3, t0), Scrolled::Sent);
        gone.target.gone.borrow_mut().push(pane());
        assert!(matches!(gone.restore(), Restore::Skipped(_)));
        assert_eq!(gone.target.notches(), vec![-3], "窗格没了一格都不翻");

        let mut panes = ScrolledPanes::default();
        panes.record(&pane());
        panes.record(&pane());
        assert_eq!(panes, ScrolledPanes::One(pane()));
        panes.record(&other);
        panes.record(&pane());
        assert_eq!(panes, ScrolledPanes::Several, "换过一次就一直算多个");
    }
}
