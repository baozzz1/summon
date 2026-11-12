//! 网页里的滚动:页面只报格数(`{"scroll": n}`),送进窗格的滚轮事件由服务端按固定格式生成。
//!
//! Claude Code 的全屏界面跑在备用屏上,tmux 不存它的历史,历史靠它自己收 SGR 鼠标滚轮事件翻。
//! 只读页也能滚(owner 2026-09-23 定),但只能送滚动事件:这里没有任何接收调用方字节的入口,
//! 能造出 [`WheelEvents`] 的只有 [`WheelEvents::for_pane`] 与 [`WheelEvents::for_restore`]。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 一条滚动消息最多翻这么多格,多的丢掉。
pub const MAX_NOTCHES_PER_MESSAGE: u32 = 5;
/// 每条连接任意 1 秒内最多送这么多次滚动,多的消息丢掉。页面按 50ms 合并一条(每秒 20 条),
/// 这里留出余量,网络抖动把几条挤到一起时不误丢。
pub const MAX_SCROLLS_PER_SECOND: usize = 30;
/// 连接结束时往下翻回去的总格数上限。
pub const MAX_RESTORE_NOTCHES: u32 = 200;
/// 翻回去时一次 tmux 调用送的格数上限。
pub const RESTORE_NOTCHES_PER_CALL: u32 = 20;

/// SGR 鼠标协议里滚轮往上、往下的按钮号。
const WHEEL_UP: u8 = 64;
const WHEEL_DOWN: u8 = 65;

/// 窗格此刻的模式与鼠标汇报状态,一次 tmux 查询读出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneMouse {
    /// `#{pane_in_mode}`:窗格在 copy-mode 等模式里。
    pub in_mode: bool,
    /// `#{mouse_sgr_flag}`:程序开了 SGR 汇报(`?1006h`)。
    pub sgr: bool,
    /// `#{mouse_any_flag}`:开了任意一种跟踪(`?1000h` / `?1002h` / `?1003h`)。
    pub any: bool,
    /// `#{mouse_button_flag}`:`?1002h`。
    pub button: bool,
    /// `#{mouse_standard_flag}`:`?1000h`。
    pub standard: bool,
    /// `#{synchronize-panes}`:这个窗格开着同步输入。
    pub synchronized: bool,
    pub width: u16,
    pub height: u16,
}

impl PaneMouse {
    /// 这个窗格此刻收不收滚轮事件。
    ///
    /// - 在模式里(本地有人在 copy-mode 里滚屏)不送:那是他的,SGR 序列开头的 ESC 会把 copy-mode
    ///   取消掉;也绝不先 `copy-mode -q` 把它退掉。
    /// - 程序没开 SGR 汇报、或一种跟踪都没开,不送:这些字节它不当鼠标事件读,就成了输入框里的字。
    /// - 开着 `synchronize-panes` 不送:tmux 会把送进来的字节照抄给同一窗口里别的同步窗格,不管
    ///   那些窗格里的程序开没开鼠标汇报——滚轮事件就成了隔壁窗格输入框里的字。
    pub fn takes_wheel(&self) -> bool {
        !self.synchronized
            && !self.in_mode
            && self.sgr
            && (self.any || self.button || self.standard)
    }
}

/// 一次滚动要送进窗格的 SGR 滚轮事件。字节只由 [`Self::for_pane`] 与 [`Self::for_restore`] 生成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelEvents {
    bytes: Vec<u8>,
    /// 这些事件翻的格数,往上为负。
    notches: i64,
}

impl WheelEvents {
    /// 窗格收滚轮时,按格数生成事件:负数往上翻(更早的内容)、正数往下,每条至多
    /// [`MAX_NOTCHES_PER_MESSAGE`] 格。窗格不收(见 [`PaneMouse::takes_wheel`])或格数为 0 时为 None。
    pub fn for_pane(pane: &PaneMouse, notches: i64) -> Option<Self> {
        let max = i64::from(MAX_NOTCHES_PER_MESSAGE);
        Self::gated(pane, notches.clamp(-max, max))
    }

    /// 连接结束时往下翻回去的一批:`down` 格往下,每批至多 [`RESTORE_NOTCHES_PER_CALL`] 格。闸与
    /// [`Self::for_pane`] 相同,窗格不收时为 None。
    pub fn for_restore(pane: &PaneMouse, down: u32) -> Option<Self> {
        Self::gated(pane, i64::from(down.min(RESTORE_NOTCHES_PER_CALL)))
    }

    /// 每格一个 `ESC [ < 按钮 ; x ; y M`。坐标是窗格正中那一格(从 1 起),只按窗格自己的尺寸算,
    /// 不用浏览器给的位置。
    fn gated(pane: &PaneMouse, notches: i64) -> Option<Self> {
        if notches == 0 || !pane.takes_wheel() {
            return None;
        }
        let button = if notches < 0 { WHEEL_UP } else { WHEEL_DOWN };
        let (x, y) = (centre(pane.width), centre(pane.height));
        let one = format!("\x1b[<{button};{x};{y}M");
        Some(Self {
            bytes: one.repeat(notches.unsigned_abs() as usize).into_bytes(),
            notches,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 翻的格数,往上为负。
    pub fn notches(&self) -> i64 {
        self.notches
    }
}

/// 一条连接净翻了多少格,只数确实送出去的(往上为负)。
///
/// Claude Code 翻在历史里时,新输出来了也停在原处;而 summon 的注入闸、轮内的菜单识别都读当前
/// 屏幕。页面关掉时停在历史里,那些判据读到的就是历史而不是输入框,所以连接结束时要把自己净往上
/// 翻的格数往下翻回去。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NetScroll(i64);

impl NetScroll {
    /// 记下一次确实送出去的滚动。
    pub fn record(&mut self, sent: &WheelEvents) {
        self.0 = self.0.saturating_add(sent.notches());
    }

    /// 往下翻回去的各批格数:净往上才翻,每批至多 [`RESTORE_NOTCHES_PER_CALL`],总共至多
    /// [`MAX_RESTORE_NOTCHES`];净往下或为 0 时不翻。
    pub fn restore_batches(&self) -> Vec<u32> {
        let mut left = u32::try_from(self.0.min(0).unsigned_abs())
            .unwrap_or(u32::MAX)
            .min(MAX_RESTORE_NOTCHES);
        let mut batches = Vec::new();
        while left > 0 {
            let batch = left.min(RESTORE_NOTCHES_PER_CALL);
            batches.push(batch);
            left -= batch;
        }
        batches
    }
}

/// 从 1 起数的正中那一格;偶数取靠前的那格。
fn centre(size: u16) -> u16 {
    size.max(1).div_ceil(2)
}

/// 每条连接的滚动限频:任意 1 秒内至多 [`MAX_SCROLLS_PER_SECOND`] 次,多的丢掉。在查 tmux 之前判,
/// 每次滚动至少起一个 tmux 进程,浏览器发多快都不能让 daemon 跟着起多快。
#[derive(Debug, Default)]
pub struct ScrollRate {
    recent: VecDeque<Instant>,
}

impl ScrollRate {
    const WINDOW: Duration = Duration::from_secs(1);

    /// 这一次能不能送;能送就记下。
    pub fn allow(&mut self, now: Instant) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Self::WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= MAX_SCROLLS_PER_SECOND {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(width: u16, height: u16) -> PaneMouse {
        PaneMouse {
            in_mode: false,
            sgr: true,
            any: true,
            button: false,
            standard: true,
            synchronized: false,
            width,
            height,
        }
    }

    fn events(n: usize, button: u8, x: u16, y: u16) -> Vec<u8> {
        format!("\x1b[<{button};{x};{y}M").repeat(n).into_bytes()
    }

    #[test]
    fn negative_scrolls_up_and_positive_scrolls_down_one_event_per_notch() {
        let pane = open(80, 24);
        assert_eq!(
            WheelEvents::for_pane(&pane, -2).unwrap().as_bytes(),
            events(2, 64, 40, 12)
        );
        assert_eq!(
            WheelEvents::for_pane(&pane, 3).unwrap().as_bytes(),
            events(3, 65, 40, 12)
        );
        assert_eq!(WheelEvents::for_pane(&pane, 0), None, "0 格不送");
    }

    #[test]
    fn one_message_scrolls_at_most_five_notches() {
        let pane = open(80, 24);
        assert_eq!(MAX_NOTCHES_PER_MESSAGE, 5);
        assert_eq!(
            WheelEvents::for_pane(&pane, 6).unwrap().as_bytes(),
            events(5, 65, 40, 12)
        );
        assert_eq!(
            WheelEvents::for_pane(&pane, i64::MIN).unwrap().as_bytes(),
            events(5, 64, 40, 12)
        );
        assert_eq!(
            WheelEvents::for_pane(&pane, i64::MAX).unwrap().as_bytes(),
            events(5, 65, 40, 12)
        );
    }

    #[test]
    fn events_land_on_the_centre_cell_of_the_pane() {
        let at = |w, h| WheelEvents::for_pane(&open(w, h), 1).unwrap();
        assert_eq!(at(81, 25).as_bytes(), events(1, 65, 41, 13), "奇数取正中");
        assert_eq!(at(80, 24).as_bytes(), events(1, 65, 40, 12), "偶数取靠前的");
        assert_eq!(at(1, 1).as_bytes(), events(1, 65, 1, 1));
        assert_eq!(at(2, 3).as_bytes(), events(1, 65, 1, 2));
        assert_eq!(
            at(0, 0).as_bytes(),
            events(1, 65, 1, 1),
            "读到 0 也不出 0 坐标"
        );
    }

    /// 在模式里、没开 SGR、一种跟踪都没开,都不送;开了任意一种跟踪加 SGR 才送。
    #[test]
    fn only_a_pane_outside_any_mode_with_sgr_and_tracking_takes_the_wheel() {
        let base = PaneMouse {
            in_mode: false,
            sgr: true,
            any: false,
            button: false,
            standard: false,
            synchronized: false,
            width: 80,
            height: 24,
        };
        assert!(!base.takes_wheel(), "一种跟踪都没开");
        for pane in [
            PaneMouse { any: true, ..base },
            PaneMouse {
                button: true,
                ..base
            },
            PaneMouse {
                standard: true,
                ..base
            },
        ] {
            assert!(pane.takes_wheel(), "{pane:?}");
            assert!(WheelEvents::for_pane(&pane, -1).is_some());
            let in_mode = PaneMouse {
                in_mode: true,
                ..pane
            };
            assert!(!in_mode.takes_wheel(), "在模式里");
            assert_eq!(WheelEvents::for_pane(&in_mode, -1), None);
            let no_sgr = PaneMouse { sgr: false, ..pane };
            assert!(!no_sgr.takes_wheel(), "没开 SGR");
            assert_eq!(WheelEvents::for_pane(&no_sgr, -1), None);
            let synchronized = PaneMouse {
                synchronized: true,
                ..pane
            };
            assert!(!synchronized.takes_wheel(), "开着 synchronize-panes");
            assert_eq!(WheelEvents::for_pane(&synchronized, -1), None);
            assert_eq!(WheelEvents::for_restore(&synchronized, 3), None);
        }
    }

    #[test]
    fn events_carry_their_signed_notch_count() {
        let pane = open(80, 24);
        assert_eq!(WheelEvents::for_pane(&pane, -2).unwrap().notches(), -2);
        assert_eq!(WheelEvents::for_pane(&pane, 9).unwrap().notches(), 5);
        assert_eq!(
            WheelEvents::for_pane(&pane, i64::MIN).unwrap().notches(),
            -5
        );
    }

    /// 翻回去只往下,每批至多 20 格、落在窗格正中;闸与平时相同。
    #[test]
    fn a_restore_batch_scrolls_down_at_most_twenty_notches() {
        let pane = open(80, 24);
        let three = WheelEvents::for_restore(&pane, 3).unwrap();
        assert_eq!(three.as_bytes(), events(3, 65, 40, 12));
        assert_eq!(three.notches(), 3);
        assert_eq!(RESTORE_NOTCHES_PER_CALL, 20);
        let capped = WheelEvents::for_restore(&pane, 50).unwrap();
        assert_eq!(capped.as_bytes(), events(20, 65, 40, 12));
        assert_eq!(WheelEvents::for_restore(&pane, 0), None);
        let in_mode = PaneMouse {
            in_mode: true,
            ..pane
        };
        assert_eq!(WheelEvents::for_restore(&in_mode, 3), None);
    }

    /// 净往上才翻回去,每批至多 20、总共至多 200;净往下或为 0 不翻。
    #[test]
    fn only_a_net_upward_scroll_is_restored_in_capped_batches() {
        let pane = open(80, 24);
        let net = |moves: &[i64]| {
            let mut net = NetScroll::default();
            for &n in moves {
                net.record(&WheelEvents::for_pane(&pane, n).unwrap());
            }
            net
        };
        assert_eq!(net(&[-3]).restore_batches(), vec![3]);
        assert_eq!(net(&[-5, -5, 2]).restore_batches(), vec![8]);
        assert_eq!(net(&[3]).restore_batches(), Vec::<u32>::new(), "净往下不翻");
        assert_eq!(net(&[-3, 3]).restore_batches(), Vec::<u32>::new());
        assert_eq!(NetScroll::default().restore_batches(), Vec::<u32>::new());
        assert_eq!(net(&[-5; 9]).restore_batches(), vec![20, 20, 5]);
        assert_eq!(MAX_RESTORE_NOTCHES, 200);
        assert_eq!(
            net(&[-5; 60]).restore_batches(),
            vec![20; 10],
            "总共至多 200"
        );
        assert_eq!(NetScroll(i64::MIN).restore_batches(), vec![20; 10]);
    }

    #[test]
    fn at_most_thirty_scrolls_go_through_in_any_second() {
        assert_eq!(MAX_SCROLLS_PER_SECOND, 30);
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut rate = ScrollRate::default();
        for i in 0..30 {
            assert!(rate.allow(ms(i * 10)), "第 {i} 次");
        }
        assert!(!rate.allow(ms(300)), "一秒内第 31 次丢掉");
        assert!(!rate.allow(ms(999)), "丢掉的不占名额,也不放行");
        assert!(rate.allow(ms(1000)), "最早那次满 1 秒后让出一个名额");
        assert!(!rate.allow(ms(1001)));
        assert!(rate.allow(ms(1010)));
        let mut rate = ScrollRate::default();
        for i in 0..100 {
            assert!(rate.allow(ms(i * 50)), "每 50ms 一次一直放行:第 {i} 次");
        }
    }
}
