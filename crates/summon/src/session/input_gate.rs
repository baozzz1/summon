//! **注入闸**:向空闲 TUI 注入任何按键之前,正向证明 pane 处于可投喂输入态;证明不了时,要么
//! 先安全退出一个被正向识别的已知 modal 再重证,要么 fail-closed 拒绝。
//!
//! 判据本体是纯函数(`summon_core::pane_input`),这里只做 IO 编排:退 copy-mode → 抓屏 →
//! 判定 →(仅对已注册面板)静止复验 + 按一次 Esc + 重判。
//!
//! **闸收口在注入原语层,不是某个执行器的私有逻辑。** 面板/菜单进入 pane 的路径根本不经过飞书
//! 透传:① 用户本地 attach 后自己跑 `/status`,面板留在 pane;② `/effort` 弹的确认菜单转成飞书
//! 卡片、没人点,菜单一直留着;③ peers 热更新 / 普通 prompt 的注入只退 copy-mode、从不查输入态。
//! 闸若只装在透传路径上,这三条照旧盲注。收口后:带 marker 的普通 prompt 从「注定被吞、等
//! arrival 窗超时才外显」提前为「先拒绝或先修复」;markerless 注入从「静默丢失」变为「可证明的
//! 送达或明确的失败」。
//!
//! **at-most-once**:被拒 = 零正文/Enter;Esc 至多一次;只有「未 attach + 光标在输入原点」的
//! 歧义态允许一次不改文本的 End 探测(真草稿随即 Home 复位);不自动重试(用户重发是唯一重试途径)。

use std::thread::sleep;
use std::time::Duration;

use summon_core::pane_input::{
    classify_pane, esc_authorized, gate_accepts, pending_input, InjectGate, InjectionRefused,
    PaneState,
};
use summon_core::scrubber::strip_ansi;
use unicode_width::UnicodeWidthStr;

use crate::session::tmux::{PaneCursor, Tmux};

/// 闸需要的 pane 能力面。抽成 trait 只为一件事:让 Esc 授权顺序(静止复验 → 至多一次 Esc →
/// 重判)能在无 tmux 的单测里被逐步钉死 —— 那是本模块唯一可能造成伤害的动作。
pub trait PaneProbe {
    fn exit_copy_mode(&self);
    fn capture(&self) -> std::io::Result<String>;
    fn send_escape(&self) -> std::io::Result<()>;
    fn cursor(&self) -> std::io::Result<PaneCursor>;
    fn send_end(&self) -> std::io::Result<()>;
    fn send_home(&self) -> std::io::Result<()>;
}

impl PaneProbe for Tmux {
    fn exit_copy_mode(&self) {
        Tmux::exit_copy_mode(self)
    }
    fn capture(&self) -> std::io::Result<String> {
        // 输入态只属于**当前可见屏**。带 scrollback 会让 cursor_y 无法和行号对齐，也会把历史
        // `❯`/spinner 带进判据；转录提取仍继续用 Tmux::capture 的 -500 行版本。
        Tmux::capture_visible(self)
    }
    fn send_escape(&self) -> std::io::Result<()> {
        self.send_key("Escape")
    }
    fn cursor(&self) -> std::io::Result<PaneCursor> {
        self.pane_cursor()
    }
    fn send_end(&self) -> std::io::Result<()> {
        self.send_key("End")
    }
    fn send_home(&self) -> std::io::Result<()> {
        self.send_key("Home")
    }
}

/// Esc 自愈的两个时间窗。
#[derive(Debug, Clone, Copy)]
pub struct GateTiming {
    /// 「画面连续静止满窗」的窗口:两次抓屏必须逐字节相同,才认这屏是静止的面板而不是正在重绘。
    pub still_window: Duration,
    /// 按下 Esc 后等重绘的时间。等不够会把「面板正在退」误判成「Esc 没生效」,平白 fail-closed。
    pub esc_redraw: Duration,
    /// 非破坏性 End 光标探测后的 TUI 重绘等待。
    pub cursor_redraw: Duration,
}

impl Default for GateTiming {
    fn default() -> Self {
        // 面板渲染实测约 1s 内完成、之后逐字节静止;700ms 静止窗足以把「还在重绘」筛掉,
        // 又只在**已经被判成面板**的路径上付出,正常注入零额外延迟。
        Self {
            still_window: Duration::from_millis(700),
            esc_redraw: Duration::from_millis(800),
            cursor_redraw: Duration::from_millis(80),
        }
    }
}

/// 证明「此刻可以往这个 pane 注入」。放行返回被证明的状态;拒绝返回带用户回执的 [`InjectionRefused`]。
///
/// 抓屏失败按 `Unknown` 处理(**不是**放行):`capture` 对死会话返回 `Ok("")`,所以「抓屏 Err」
/// 本就罕见;两种情况都同样证明不了输入态。
pub fn prove_injectable(
    probe: &impl PaneProbe,
    gate: InjectGate,
    timing: GateTiming,
) -> Result<PaneState, InjectionRefused> {
    if gate == InjectGate::StartupUnproven {
        // 冷启期显式豁免:pane 刚由我们自己拉起,且冷启另有 wait_ready / 信任目录等待。
        return Ok(PaneState::Unknown);
    }
    // copy-mode(用户滚屏/鼠标选择)既吞按键、也让抓屏抓到滚上去的历史区域 —— 必须先退,
    // 否则底部的输入行根本不在这一屏里,判据会对着 scrollback 做决定。
    probe.exit_copy_mode();
    let pane = probe.capture().unwrap_or_default();
    let state = classify_pane(&pane);
    if gate_accepts(gate, state) {
        return Ok(state);
    }
    if state == PaneState::Draft && input_buffer_is_proven_empty(probe, &pane, timing.cursor_redraw)
    {
        // Claude 会往空编辑器里画东西:`Try "…"` 提示词,以及答完一轮后由回复衍生的**建议下一句**
        // (ghost text,内容读起来就像用户自己写的)。纯文本抓屏里它们与草稿同形,但缓冲区是空的 ——
        // 这里返回逻辑输入态,后续正文的第一个字节由 Claude 正常替换掉画上去的字,不会与它拼接。
        return Ok(PaneState::InputReady);
    }
    if !esc_authorized(state) {
        return Err(InjectionRefused::on_pane(state, &pane));
    }
    // 授权条件的最后一条:画面连续静止满窗。面板刚弹出还在渲染时按 Esc,可能落到渲染前的旧界面上。
    sleep(timing.still_window);
    let again = probe.capture().unwrap_or_default();
    if again != pane || classify_pane(&again) != state {
        // 还在动 / 判定漂移 → 这一屏不是「静止的已知面板」,不按键,停手上浮。
        return Err(InjectionRefused::on_pane(classify_pane(&again), &again));
    }
    if probe.send_escape().is_err() {
        return Err(InjectionRefused::new(state));
    }
    sleep(timing.esc_redraw);
    let after = classify_pane(&probe.capture().unwrap_or_default());
    if gate_accepts(gate, after) {
        Ok(after)
    } else {
        // 不试第二个键、不试第二次:「静止但退不出去」永远上浮给人,不赌未知界面上的下一个键。
        Err(InjectionRefused::after_esc(after))
    }
}

/// 只读地判「此刻 pane 是不是空输入框」。与 [`prove_injectable`] 的差别只有一条,却是本质的:
/// 它**一个会改变 TUI 状态的键都不发** —— 不退 copy-mode、不按 Esc。
///
/// 接管一条在跑的轮时必须用它:Esc 的授权条件是「静止的已知面板」,而画面静止恰恰也是长工具 /
/// 等 API 响应时的常态,把 Esc 打在正跑着的那一轮上就是打断它 —— 而「那一轮还在跑」正是接管的
/// 前提。举证义务照旧在判定方:认不出就 `false`(= 证明不了「没送到」),宁可多等一会儿。
///
/// `Draft` 是唯一走探测的一态:claude 会往**空**编辑器里画 placeholder / 建议下一句,纯文本抓屏
/// 与真草稿同形。它交给 [`input_buffer_is_proven_empty`] —— 那条路只按一次 End 复核光标位置,
/// 不动编辑缓冲区、不改 TUI 状态,认出真草稿还会 Home 复位。
pub fn observe_idle_prompt(probe: &impl PaneProbe, timing: GateTiming) -> bool {
    let Ok(pane) = probe.capture() else {
        return false;
    };
    match classify_pane(&pane) {
        PaneState::InputReady => true,
        PaneState::Draft => input_buffer_is_proven_empty(probe, &pane, timing.cursor_redraw),
        _ => false,
    }
}

/// Claude 画在空编辑器里的字(placeholder、答完一轮后的建议下一句)与真草稿在纯文本抓屏里同形：
/// 都是 `❯ <非空文字>`，样式信息在 `capture-pane -p` 里本就不存在。不猜文案、不维护字符串表；
/// 只在会话无人 attach 且光标位于 `❯ ` 后时，按一次**不修改编辑缓冲区**的 End，看光标动不动：
///
/// - 缓冲区是空的（画上去的字不在里面）：光标一动不动 → 正向证明 InputReady；
/// - 真草稿且光标在开头：End 移到末尾 → 立即 Home 复位并继续按 Draft 拒绝；
/// - 输入行在两次抓屏之间变了、attach 竞争、IO 失败：证明失败，fail-closed。
///
/// **稳定性只判输入行,不判整屏**:pane 的其余部分有自己会走的东西 —— 后台 agent 托盘的已跑时长、
/// `done 8:42 PM` 这类时钟 —— 空闲会话上每秒都在变。拿整屏逐字节相等当判据,等于在挂着后台任务的
/// 会话上永远证明不了缓冲区是空的:2026-08-31 19:59 的真机事故就是这样把一条建议下一句判成草稿、
/// 拒掉了用户的正常消息(光标证据当时完全正确,是托盘计时行 `4m 0s → 4m 8s` 推翻了整屏相等)。
fn input_buffer_is_proven_empty(probe: &impl PaneProbe, before: &str, redraw: Duration) -> bool {
    let cursor_before = match probe.cursor() {
        Ok(c) => c,
        Err(e) => {
            // **读不到光标 ≠ 有草稿。** 这里返回 false 之后,调用方会把 pane 判成 Draft 并回
            // 「输入框里有一段没发送的文字」—— 一句我们其实没有证据的话。2026-08-15 就是这么
            // 坏的:tmux 3.7b 把格式串里的 `\t` 改写成 `_`,解析恒失败,于是**每一个画着
            // placeholder 的空输入框都被判成有草稿**,消息一律被拒,而日志里一个字都没有。
            // 判据仍然 fail-closed(证明不了就不注入),但必须留痕。
            crate::observe::log::log(&summon_i18n::format!("r78fe7ab82b"));
            return false;
        }
    };
    if !cursor_before.visible
        || cursor_before.attached_clients != 0
        || !cursor_is_at_prompt_origin(before, cursor_before)
    {
        return false;
    }
    if probe.send_end().is_err() {
        return false;
    }
    sleep(redraw);
    let Ok(after) = probe.capture() else {
        return false;
    };
    let Ok(cursor_after) = probe.cursor() else {
        return false;
    };
    if !same_input_line(before, &after) {
        // 输入行自己变了:可能有人在打字,也可能 TUI 换了画上去的那句。这一拍证明不了任何事。
        return false;
    }
    if cursor_after == cursor_before {
        return true;
    }
    // 真草稿:End 只移动了光标。会话仍无人 attach 时用 Home 精确复位；
    // 若中途有人 attach，停止发任何后续键，避免和人竞争。
    if cursor_after.attached_clients == 0 {
        let _ = probe.send_home();
        sleep(redraw);
    }
    false
}

/// 两次抓屏之间输入行是否一字未变(判据落在证明所依赖的那一行上,理由见
/// [`input_buffer_is_proven_empty`])。取的是 `classify_pane` 用的同一条输入行,不是整屏。
fn same_input_line(before: &str, after: &str) -> bool {
    let before = pending_input(before);
    before.is_some() && before == pending_input(after)
}

/// 光标是否位于当前可见输入行 `❯ ` 之后。用 Unicode 显示宽度而不是字节数；NBSP 与普通空格
/// 都按一列处理。输入行若不在 cursor_y，说明抓屏与 display-message 间发生了重绘，拒绝探测。
fn cursor_is_at_prompt_origin(pane: &str, cursor: PaneCursor) -> bool {
    let plain = strip_ansi(pane);
    let Some(line) = plain.lines().nth(cursor.y) else {
        return false;
    };
    let Some(marker_byte) = line.find('❯') else {
        return false;
    };
    if !line[..marker_byte].trim().is_empty() {
        return false;
    }
    let after_marker = &line[marker_byte + '❯'.len_utf8()..];
    let whitespace_bytes = after_marker
        .char_indices()
        .take_while(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    let prefix_end = marker_byte + '❯'.len_utf8() + whitespace_bytes;
    cursor.x == UnicodeWidthStr::width(&line[..prefix_end])
}

/// 从 `anyhow` 错误链里认出「被闸拒绝」,取其用户回执文案。
///
/// 上层不能拿通用失败文案糊弄这类错误:被拒的语义是**什么都没执行**,而通用文案(「这轮没有
/// 正常完成」「等待超时」)会让用户以为命令跑了一半 —— 那正是 spec 要消灭的谎报。
pub fn refusal_notice_of(err: &anyhow::Error) -> Option<String> {
    err.chain()
        .find_map(|c| c.downcast_ref::<InjectionRefused>())
        .map(|r| r.notice.clone())
}

/// 从「被闸拒绝」里取拒绝那一刻的菜单快照(只有菜单拒因有)。出站侧据它在文字回执之外
/// 补一张交互中介卡 —— 菜单一次按键就能解决,不该只丢给用户一句「去 attach」。
pub fn refused_menu_of(err: &anyhow::Error) -> Option<summon_core::interactive::InteractiveState> {
    err.chain()
        .find_map(|c| c.downcast_ref::<InjectionRefused>())
        .and_then(|r| r.menu.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 可编排的假 pane:按顺序吐出预设的屏幕,并记下发过几次 Esc。
    struct FakeProbe {
        screens: RefCell<Vec<String>>,
        cursors: RefCell<Vec<PaneCursor>>,
        escapes: RefCell<usize>,
        ends: RefCell<usize>,
        homes: RefCell<usize>,
        copy_mode_exits: RefCell<usize>,
        capture_err: bool,
    }

    impl FakeProbe {
        fn new(screens: &[&str]) -> Self {
            Self {
                screens: RefCell::new(screens.iter().rev().map(|s| s.to_string()).collect()),
                cursors: RefCell::new(Vec::new()),
                escapes: RefCell::new(0),
                ends: RefCell::new(0),
                homes: RefCell::new(0),
                copy_mode_exits: RefCell::new(0),
                capture_err: false,
            }
        }
        fn with_cursors(screens: &[&str], cursors: &[PaneCursor]) -> Self {
            let mut p = Self::new(screens);
            p.cursors = RefCell::new(cursors.iter().rev().copied().collect());
            p
        }
        fn failing() -> Self {
            Self {
                screens: RefCell::new(Vec::new()),
                cursors: RefCell::new(Vec::new()),
                escapes: RefCell::new(0),
                ends: RefCell::new(0),
                homes: RefCell::new(0),
                copy_mode_exits: RefCell::new(0),
                capture_err: true,
            }
        }
        fn escapes(&self) -> usize {
            *self.escapes.borrow()
        }
        fn ends(&self) -> usize {
            *self.ends.borrow()
        }
        fn homes(&self) -> usize {
            *self.homes.borrow()
        }
        fn copy_mode_exits(&self) -> usize {
            *self.copy_mode_exits.borrow()
        }
    }

    impl PaneProbe for FakeProbe {
        fn exit_copy_mode(&self) {
            *self.copy_mode_exits.borrow_mut() += 1;
        }
        fn capture(&self) -> std::io::Result<String> {
            if self.capture_err {
                return Err(std::io::Error::other("capture 失败"));
            }
            let mut s = self.screens.borrow_mut();
            // 最后一屏保持不变(模拟静止画面)
            Ok(if s.len() > 1 {
                s.pop().unwrap()
            } else {
                s.last().cloned().unwrap_or_default()
            })
        }
        fn send_escape(&self) -> std::io::Result<()> {
            *self.escapes.borrow_mut() += 1;
            Ok(())
        }
        fn cursor(&self) -> std::io::Result<PaneCursor> {
            let mut cursors = self.cursors.borrow_mut();
            if cursors.is_empty() {
                return Err(std::io::Error::other("cursor 失败"));
            }
            Ok(if cursors.len() > 1 {
                cursors.pop().unwrap()
            } else {
                *cursors.last().unwrap()
            })
        }
        fn send_end(&self) -> std::io::Result<()> {
            *self.ends.borrow_mut() += 1;
            Ok(())
        }
        fn send_home(&self) -> std::io::Result<()> {
            *self.homes.borrow_mut() += 1;
            Ok(())
        }
    }

    const IDLE: &str =
        "✻ Worked for 7m 25s\n──── summon:soul-x ──\n❯ \n────\n\n  ⏵⏵ bypass permissions on\n";
    const PANEL: &str = "Claude Code Status\n Account · a@b.c\n\n            Esc to cancel\n";
    const DRAFT: &str = "✻ Worked for 7m 25s\n──── summon:soul-x ──\n❯ 我打了一半\n────\n";
    const PLACEHOLDER: &str =
        "Claude Code v2.1.212\n──── summon:soul-x ──\n❯ Try \"how do I log an error?\"\n────\n";
    const MENU: &str = "Change effort level?\n❯ 1. Yes, switch to xhigh\n  2. No\n";
    const RUNNING: &str = "✽ Composing… (11m 6s · ↓ 34.5k tokens)\n────\n❯ \n";

    fn fast() -> GateTiming {
        GateTiming {
            still_window: Duration::from_millis(0),
            esc_redraw: Duration::from_millis(0),
            cursor_redraw: Duration::from_millis(0),
        }
    }

    /// 2026-08-14 真机回归:Claude 2.1.212 在**空**编辑缓冲区里绘制 `Try "…"` placeholder，
    /// 纯文本分类会误判 Draft，导致每一条 bot-to-bot 消息都在接收端被拒绝。
    #[test]
    fn painted_placeholder_is_proven_empty_and_allowed() {
        let origin = PaneCursor {
            x: 2,
            y: 2,
            visible: true,
            attached_clients: 0,
        };
        let p = FakeProbe::with_cursors(&[PLACEHOLDER, PLACEHOLDER], &[origin, origin]);
        assert_eq!(
            prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap(),
            PaneState::InputReady
        );
        assert_eq!(p.ends(), 1, "只用一次非破坏性 End 验证空缓冲区");
        assert_eq!(p.homes(), 0, "placeholder 上 End 不移动,无需恢复");
    }

    /// 光标恰好在开头不能单独证明 placeholder：用户可能对真实草稿按过 Home。End 若移动，
    /// 必须把光标复原并继续拒绝，正文/Enter 一个都不能发。
    #[test]
    fn real_draft_at_origin_is_restored_and_refused() {
        let origin = PaneCursor {
            x: 2,
            y: 2,
            visible: true,
            attached_clients: 0,
        };
        let at_end = PaneCursor { x: 8, ..origin };
        let p = FakeProbe::with_cursors(&[DRAFT, DRAFT], &[origin, at_end]);
        let err = prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap_err();
        assert_eq!(err.state, PaneState::Draft);
        assert_eq!(p.ends(), 1, "歧义态只探一次");
        assert_eq!(p.homes(), 1, "真草稿必须恢复到探测前的 Home 位置");
        assert_eq!(p.escapes(), 0, "草稿上仍不允许 Esc");
    }

    /// 真机形态(2026-08-31 20:44 抓自 `soul-flare-5-g00000000a11ce021`):输入框下面挂着后台
    /// agent 托盘,那一行的已跑时长自己在走 —— 这种会话上整屏**永远**不会两次逐字节相等。
    fn pane_with_ticking_tray(input: &str, tray_elapsed: &str) -> String {
        format!(
            "  上一轮的回复正文。\n\n\
✻ Cooked for 1m 3s · done 8:42 PM\n\n\
──────────────────── summon:soul-flare-5-g00000000a11ce021 ─\n\
❯ {input}\n\
────────────────────────────────────────────────────────────\n\
                                                        /rc\n  \
⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n\n  \
⏺ main\n  \
◯ general-purpose  修 Draft 误判        {tray_elapsed} · ↓ 347.8k tokens\n"
        )
    }

    /// 输入行在 `pane_with_ticking_tray` 里的行号 —— 光标 y 必须指向它。
    const TRAY_INPUT_ROW: usize = 5;

    /// 2026-08-31 19:59 真机事故回归:Claude 答完一轮后往空编辑器里画了一句**建议下一句**
    /// (ghost text,内容由上一轮回复衍生,读起来就像用户自己写的),光标停在首字符上、缓冲区是空的。
    /// 光标证据当时完全正确,是后台托盘的计时行推翻了「整屏逐字节相等」,消息被当草稿拒掉。
    #[test]
    fn a_painted_suggestion_is_proven_empty_while_the_tray_keeps_ticking() {
        let origin = PaneCursor {
            x: 2,
            y: TRAY_INPUT_ROW,
            visible: true,
            attached_clients: 0,
        };
        let ghost = "修完验完直接合进 develop";
        let before = pane_with_ticking_tray(ghost, "4m 0s");
        let after = pane_with_ticking_tray(ghost, "4m 8s");
        assert_ne!(before, after, "样本必须复现「整屏在动」");
        assert_eq!(classify_pane(&before), PaneState::Draft, "样本要复现旧误判");
        let p = FakeProbe::with_cursors(&[&before, &after], &[origin, origin]);
        assert_eq!(
            prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap(),
            PaneState::InputReady
        );
        assert_eq!(p.ends(), 1, "只用一次非破坏性 End 验证空缓冲区");
        assert_eq!(p.homes(), 0, "光标没动就没有要复位的东西");
    }

    /// 同一张会动的屏上,真草稿仍然要拒:End 把光标移到了末尾 = 缓冲区里真有字。
    /// 判据放宽到输入行之后,这条是防它放宽过头的那道边。
    #[test]
    fn a_real_draft_is_still_refused_while_the_tray_keeps_ticking() {
        let origin = PaneCursor {
            x: 2,
            y: TRAY_INPUT_ROW,
            visible: true,
            attached_clients: 0,
        };
        let draft = "我打了一半";
        let p = FakeProbe::with_cursors(
            &[
                &pane_with_ticking_tray(draft, "4m 0s"),
                &pane_with_ticking_tray(draft, "4m 8s"),
            ],
            &[origin, PaneCursor { x: 12, ..origin }],
        );
        let err = prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap_err();
        assert_eq!(err.state, PaneState::Draft);
        assert!(err.notice.contains("没发送的文字"), "{}", err.notice);
        assert_eq!(p.homes(), 1, "真草稿必须恢复到探测前的 Home 位置");
        assert_eq!(p.escapes(), 0);
    }

    /// 输入行自己在两次抓屏之间变了 = 有人正在打字 / TUI 换了画上去的那句:这一拍什么都证明不了,
    /// 拒绝,并且不再发第二个键去和人抢输入框。
    #[test]
    fn an_input_line_that_changes_mid_probe_fails_closed() {
        let origin = PaneCursor {
            x: 2,
            y: TRAY_INPUT_ROW,
            visible: true,
            attached_clients: 0,
        };
        let p = FakeProbe::with_cursors(
            &[
                &pane_with_ticking_tray("我打了一半", "4m 0s"),
                &pane_with_ticking_tray("我打了一半 又接着打", "4m 8s"),
            ],
            &[origin, origin],
        );
        let err = prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap_err();
        assert_eq!(err.state, PaneState::Draft);
        assert_eq!(p.ends(), 1);
        assert_eq!(p.homes(), 0, "证明不了就停手,不再发键");
    }

    /// 人正 attach 时绝不发探测键：哪怕画面看起来和 placeholder 一样，也只能 fail-closed。
    #[test]
    fn attached_placeholder_shape_is_refused_without_probe_keys() {
        let attached = PaneCursor {
            x: 2,
            y: 2,
            visible: true,
            attached_clients: 1,
        };
        let p = FakeProbe::with_cursors(&[PLACEHOLDER], &[attached]);
        let err = prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap_err();
        assert_eq!(err.state, PaneState::Draft);
        assert_eq!(p.ends(), 0);
        assert_eq!(p.homes(), 0);
        assert_eq!(p.escapes(), 0);
    }

    /// 探测期间有人 attach 是竞争态：即使 End 没移动也不能据此放行，且不再发 Home 等第二个键。
    #[test]
    fn attach_race_during_placeholder_probe_fails_closed() {
        let before = PaneCursor {
            x: 2,
            y: 2,
            visible: true,
            attached_clients: 0,
        };
        let raced = PaneCursor {
            attached_clients: 1,
            ..before
        };
        let p = FakeProbe::with_cursors(&[PLACEHOLDER, PLACEHOLDER], &[before, raced]);
        assert!(prove_injectable(&p, InjectGate::PromptQueue, fast()).is_err());
        assert_eq!(p.ends(), 1);
        assert_eq!(p.homes(), 0, "attach 竞争后不再发送任何键");
    }

    /// 真机验收入口：目标必须是一个**无人 attach、空缓冲区但画着 placeholder** 的 Claude tmux。
    /// 只发一次无副作用 End，不提交模型请求；不设环境变量就明确跳过。
    #[test]
    #[ignore = "需要 SUMMON_TEST_PLACEHOLDER_TMUX 指向真实 Claude placeholder pane"]
    fn real_tmux_painted_placeholder_is_accepted_without_changing_screen() {
        let name = std::env::var("SUMMON_TEST_PLACEHOLDER_TMUX").expect("缺 tmux session 名");
        let tmux = Tmux::new(name);
        let before = tmux.capture_visible().expect("抓取真机 pane");
        assert_eq!(
            classify_pane(&before),
            PaneState::Draft,
            "样本必须复现旧误判"
        );
        assert_eq!(
            prove_injectable(&tmux, InjectGate::PromptQueue, GateTiming::default()).unwrap(),
            PaneState::InputReady
        );
        // 只钉输入行:后台托盘计时这类自己会走的 chrome 不在证明范围内(见
        // `input_buffer_is_proven_empty`),拿整屏比会把探测的正确性押在会话恰好闲到没有后台任务上。
        assert_eq!(
            pending_input(&tmux.capture_visible().unwrap()),
            pending_input(&before),
            "探测不得改变输入行"
        );
    }

    /// 只读判定的正面:空 `❯` 输入行 → true,且**一个键都没发**。接管路径靠它证明
    /// 「上一代的注入落了空」,所以它必须既能认出空输入框,又不能碰 pane。
    #[test]
    fn observe_idle_prompt_accepts_empty_input_line_without_any_key() {
        let p = FakeProbe::new(&[IDLE]);
        assert!(observe_idle_prompt(&p, fast()));
        assert_eq!((p.escapes(), p.ends(), p.homes()), (0, 0, 0));
        assert_eq!(
            p.copy_mode_exits(),
            0,
            "退 copy-mode 会把正滚屏看输出的用户踢回底部,只读判定不做"
        );
    }

    /// 画上去的 placeholder 与真草稿同形,只能靠一次非破坏性 End 复核光标 —— 这是本函数
    /// 唯一会发的键,不改编辑缓冲区、不改 TUI 状态。
    #[test]
    fn observe_idle_prompt_proves_painted_placeholder_is_empty() {
        let origin = PaneCursor {
            x: 2,
            y: 2,
            visible: true,
            attached_clients: 0,
        };
        let p = FakeProbe::with_cursors(&[PLACEHOLDER, PLACEHOLDER], &[origin, origin]);
        assert!(observe_idle_prompt(&p, fast()));
        assert_eq!(p.ends(), 1, "只用一次非破坏性 End 验证空缓冲区");
        assert_eq!(p.escapes(), 0, "只读判定永不按 Esc");
        assert_eq!(p.copy_mode_exits(), 0, "只读判定不退 copy-mode");
    }

    /// **接管路径的核心不变量**:轮还在跑时判 false,且绝不按 Esc。画面静止也可能是长工具 /
    /// 等 API 响应,`prove_injectable` 会在这种屏上按 Esc 自愈 —— 那等于打断正在接管的那一轮。
    #[test]
    fn observe_idle_prompt_never_escapes_a_running_turn_or_a_panel() {
        for pane in [RUNNING, PANEL, MENU, DRAFT] {
            let p = FakeProbe::new(&[pane, pane, IDLE]);
            assert!(
                !observe_idle_prompt(&p, fast()),
                "非空输入框一律判 false: {pane:?}"
            );
            assert_eq!(p.escapes(), 0, "只读判定永不按 Esc: {pane:?}");
            assert_eq!(p.copy_mode_exits(), 0, "只读判定不退 copy-mode: {pane:?}");
        }
    }

    /// 抓屏失败与死会话的 `Ok("")` 都证明不了输入态,一律 false —— 方向与 `prove_injectable`
    /// 的 fail-closed 相同,代价只是接管多等一拍。
    #[test]
    fn observe_idle_prompt_fails_closed_when_the_screen_proves_nothing() {
        assert!(!observe_idle_prompt(&FakeProbe::failing(), fast()));
        assert!(!observe_idle_prompt(&FakeProbe::new(&[""]), fast()));
    }

    #[test]
    fn empty_prompt_passes_without_touching_any_key() {
        let p = FakeProbe::new(&[IDLE]);
        assert_eq!(
            prove_injectable(&p, InjectGate::IdleOnly, fast()).unwrap(),
            PaneState::InputReady
        );
        assert_eq!(p.escapes(), 0, "就绪态不该按任何键");
    }

    /// 面板吞注入的正面修复:静止的已知面板 → 按一次 Esc → 回到空输入框 → 放行。
    #[test]
    fn still_registered_panel_recovers_with_exactly_one_escape() {
        // 两次抓屏相同(静止复验通过),Esc 之后回到空闲。
        let p = FakeProbe::new(&[PANEL, PANEL, IDLE]);
        assert_eq!(
            prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap(),
            PaneState::InputReady
        );
        assert_eq!(p.escapes(), 1, "Esc 至多一次");
    }

    /// 退不出去就 fail-closed:不试第二个键、不试第二次,回执写明已试过一次。
    #[test]
    fn panel_that_survives_escape_fails_closed_without_a_second_try() {
        let p = FakeProbe::new(&[PANEL, PANEL, PANEL]);
        let err = prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap_err();
        assert_eq!(p.escapes(), 1, "绝不按第二次");
        assert!(err.notice.contains("不再试第二次"), "回执: {}", err.notice);
        assert!(err.notice.contains("本条没有执行"));
    }

    /// attach 用户的草稿保护:草稿态零按键 —— Esc 会清掉用户打了一半的文字。
    #[test]
    fn user_draft_is_never_escaped_and_never_typed_over() {
        let p = FakeProbe::new(&[DRAFT]);
        for gate in [InjectGate::IdleOnly, InjectGate::PromptQueue] {
            let err = prove_injectable(&p, gate, fast()).unwrap_err();
            assert_eq!(err.state, PaneState::Draft);
            assert!(err.notice.contains("没发送的文字"), "{}", err.notice);
        }
        assert_eq!(p.escapes(), 0, "带草稿的输入框上永不按 Esc");
    }

    /// 菜单不授权 Esc(可能是工具授权提示):明确拒绝,零按键。
    #[test]
    fn known_menu_is_refused_without_pressing_escape() {
        let p = FakeProbe::new(&[MENU]);
        let err = prove_injectable(&p, InjectGate::IdleOnly, fast()).unwrap_err();
        assert!(matches!(err.state, PaneState::Modal(_)));
        assert_eq!(p.escapes(), 0);
    }

    /// 运行中永远不许被当 modal 处理:IdleOnly 拒绝但零按键(不打断别人的轮),
    /// PromptQueue 放行(带 marker 的两相协议自己负责送达)。
    #[test]
    fn running_turn_is_refused_for_idle_only_and_never_interrupted() {
        let p = FakeProbe::new(&[RUNNING]);
        let err = prove_injectable(&p, InjectGate::IdleOnly, fast()).unwrap_err();
        assert_eq!(err.state, PaneState::Running);
        assert_eq!(p.escapes(), 0, "运行中绝不按 Esc —— 那是打断键");
        assert_eq!(
            prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap(),
            PaneState::Running
        );
    }

    /// 面板还在重绘(两次抓屏不同)→ 不按键,fail-closed。
    #[test]
    fn moving_screen_is_not_still_enough_to_authorize_escape() {
        let p = FakeProbe::new(&[PANEL, &format!("{PANEL}\n  Usage · 13.4k\n")]);
        let err = prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap_err();
        assert_eq!(p.escapes(), 0, "画面还在动就不按键");
        assert!(err.notice.contains("本条没有执行"));
    }

    /// 真机 `/cost` 面板(2026-08-14,claude 2.1.231):**底部没有页脚**,只有滚动指示符 ↓,
    /// 靠 tab 栏认出。上面那个 `PANEL` 常量是带页脚的形态,两种都得能挡住。
    const COST_PANEL: &str = "▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n   \
Settings  Status   Config   Usage   Stats\n\n   Session\n\n   Total cost:  $0.0000\n   \
Current week (all models)\n   ███████████▌  23% used\n                              ↓\n";

    /// attach 竞争(spec 第八节场景 5)与面板吞注入(场景 1)的合流:用户自己在 pane 里开着
    /// `/cost`,这时飞书消息到达。合法结局只有两种 —— **确证面板退出后送达**,或**明确拒绝并
    /// 外显**;「没看见坏东西就注进去」不在其中(那正是 `/cost` 面板吃掉 `/compact` 的事故)。
    #[test]
    fn a_message_arriving_on_an_attached_panel_is_either_proven_or_refused() {
        // ① 面板能退:恰好一次 Esc,放行的是**被重新证明过**的空输入框(不是「按过键了就算」)。
        let recovers = FakeProbe::new(&[COST_PANEL, COST_PANEL, IDLE]);
        assert_eq!(
            prove_injectable(&recovers, InjectGate::IdleOnly, fast()).unwrap(),
            PaneState::InputReady
        );
        assert_eq!(recovers.escapes(), 1);
        // ② 退不掉(用户又开了一层 / Esc 被吞):拒绝、不试第二次,回执说清本条没执行。
        let stuck = FakeProbe::new(&[COST_PANEL, COST_PANEL, COST_PANEL]);
        let err = prove_injectable(&stuck, InjectGate::IdleOnly, fast()).unwrap_err();
        assert_eq!(stuck.escapes(), 1);
        assert!(err.notice.contains("本条没有执行"), "{}", err.notice);
        assert!(err.notice.contains("不再试第二次"), "{}", err.notice);
    }

    /// 抓屏失败 = 证明不了,按 Unknown fail-closed(而不是「没看到坏东西就放行」)。
    #[test]
    fn capture_failure_is_fail_closed() {
        let p = FakeProbe::failing();
        let err = prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap_err();
        assert_eq!(err.state, PaneState::Unknown);
        assert_eq!(p.escapes(), 0);
    }

    /// 冷启豁免不抓屏、不判定(名字自带说明);它不在 spec 点名的盲注路径里。
    #[test]
    fn startup_gate_short_circuits_before_any_capture() {
        let p = FakeProbe::failing();
        assert!(prove_injectable(&p, InjectGate::StartupUnproven, fast()).is_ok());
    }

    /// 拒绝必须能穿过 anyhow 链被上层认出来,否则出站侧只会用通用失败文案谎报。
    #[test]
    fn refusal_survives_anyhow_context_wrapping() {
        let err = anyhow::Error::new(InjectionRefused::new(PaneState::Modal(
            summon_core::pane_input::ModalKind::Panel,
        )))
        .context("注入 /compact 失败");
        let notice = refusal_notice_of(&err).expect("应能从错误链里认出被闸拒绝");
        assert!(notice.contains("全屏面板"));
        assert!(refusal_notice_of(&anyhow::anyhow!("别的错")).is_none());
    }

    /// 菜单拒因要把屏上的菜单一起带出去:出站侧只有拿到它才发得出可点的中介卡,
    /// 否则用户手上只有一句「去 attach」。面板/草稿这些拒因不带,凭空发卡点不动。
    #[test]
    fn a_menu_refusal_carries_the_menu_out_to_the_receipt_side() {
        let p = FakeProbe::new(&[MENU]);
        let err =
            anyhow::Error::new(prove_injectable(&p, InjectGate::PromptQueue, fast()).unwrap_err())
                .context("注入本条消息失败");
        let menu = refused_menu_of(&err).expect("菜单拒因必须把菜单带到出站侧");
        assert_eq!(menu.options.len(), 2);
        assert_eq!(menu.options[0].label, "Yes, switch to xhigh");
        assert_eq!(p.escapes(), 0, "菜单上一个键都不许按");

        for pane in [PANEL, RUNNING] {
            let p = FakeProbe::new(&[pane]);
            let err =
                anyhow::Error::new(prove_injectable(&p, InjectGate::IdleOnly, fast()).unwrap_err());
            assert!(refused_menu_of(&err).is_none(), "非菜单拒因不该带菜单");
        }
        assert!(refused_menu_of(&anyhow::anyhow!("别的错")).is_none());
    }
}
