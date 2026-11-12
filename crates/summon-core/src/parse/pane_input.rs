//! 注入前的**输入态证明**:把一屏 claude TUI 文本判成五态之一,纯文本、零 IO。
//!
//! 为什么需要正向证明(设计见 `docs/spec/2026-08-14-transcript-slash-passthrough-design.md` 第二节):
//! TUI 的「空闲」不是单一状态。`/status` `/cost` 这类全屏面板渲染完之后逐字节静止,画面比任何
//! 完成态都稳,但整屏没有输入框 —— 之后注入的**每一条**按键都被它吞掉(实测 `/cost` 面板吃掉
//! 紧跟的 `/compact`,那条命令根本没进 claude)。所以判据必须取**正向特征**,不能取「没看到坏
//! 东西」:举证义务在注入方,证明不了的默认答案是「不可注入」。
//!
//! 判据全部来自真机抓屏(2026-08-14 抓了 5 个在跑的 `soul-*` pane):
//!
//! ```text
//! ✻ Cooked for 2m 57s
//! ❯ /cd /Users/…                     ← scrollback 里的历史回显,不是输入框
//!   ⎿  Moved to /Users/…
//! ──────────────── summon:soul-… ──   ← 输入框上框线(带会话名)
//! ❯                                   ← 真·输入行(空)
//! ────────────────────────────────
//!   ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
//! ```
//!
//! 三个必须区分开、却都含 `❯` 的形态:菜单光标 `❯ 1. …`、scrollback 回显 `❯ /clear`、
//! attach 用户打了一半没发送的草稿 `❯ <文本>`。旧的 `/compact` 判据「`❯` 存在即可」是它的
//! 弱形式,恰好对不弹菜单的 compact 安全,不能泛化。

use crate::scrubber::{is_ready_lines, strip_ansi};

/// claude TUI 的输入/光标标记(U+276F)。**不收裸 `>`**:markdown 引用块、diff、邮件式引用
/// 里的 `> …` 会把整个判据变成假阳(与 `interactive.rs` 同一决策)。
const PROMPT_MARKER: char = '❯';

/// 输入框的**下框线**:一整行全是框线字符的满宽横杠。它是定位输入行的锚。
///
/// 为什么不用「距底 N 行」这种固定窗口(2026-08-14 对抗审查实证的阻塞项):claude 把**后台
/// agent / dynamic workflow 托盘渲染在输入框下方**,每多一个后台任务就多一行。真机
/// `soul-flare-5-g00000000a11ce04d` 抓屏里输入行之后已经有 8 行(下框线 / 空行 / 快捷键提示 /
/// 空行 / `⏺ main` / 3 行 agent 列表),再多两个后台任务就把空 `❯` 顶出 10 行窗口 —— 判定退化成
/// `Unknown`,而 `Unknown` 对 `IdleOnly` 与 `PromptQueue` 双双拒绝、也不授权 Esc:**这条会话对
/// 飞书整体失联,且没有自愈路径**(屏没坏,是判据窗口太窄,attach 也修不好)。方向虽是 fail-closed,
/// 代价却不是「多拒一次」,所以不能靠调大常数,得换锚。
///
/// 下框线是结构性的:托盘行、快捷键提示行、状态行里都不会出现「整行只有框线字符」的形态,
/// 而它永远紧贴输入行下方。找到它再往上找最近的 `❯`,下框线以下的一切自动被排除,托盘再长都不影响。
const INPUT_BOX_MIN_RULE: usize = 8;

/// 从下框线往上回溯几行找输入行。草稿换行会把输入行顶高,留一点余量;这几行都在框内,
/// 不会碰到 scrollback。
const INPUT_BOX_HEIGHT: usize = 10;

/// 兜底窗口:抓屏里**没有**满宽下框线时(合成样本 / 未来版本改了框线形态)才用。
/// 这条路径回到旧判据的语义,窗口偏小的代价只是 fail-closed 多拒一次(安全方向)。
const PROMPT_TAIL_LINES: usize = 10;

/// 已注册的**面板特征库**(采样池):只收真机见过的正向特征,不收传闻。
///
/// ① **页脚**:全屏面板没有输入框,底部印着这行 —— 它同时是「Esc 在此屏的语义由 TUI 自己写在
///    屏幕上」的凭据(`/status` 的 `Esc to cancel`,真机 2026-08-14,claude 2.1.231)。
///    `esc to dismiss` 是 `/goal` 目标面板的页脚(同日同版本真机):该面板**没有 tab 栏**,
///    页脚是它唯一的正向特征 —— 不注册它,裸 `/goal` 的面板落 `Unknown`,既退不掉(Esc 不授权)
///    又拒一切注入,panel 收尾永远等到超时。
const PANEL_FOOTERS: &[&str] = &["esc to cancel", "esc to dismiss"];

/// ② **顶部 tab 栏**:面板页脚**随视口高度出现/消失**(真机 2026-08-14 复测:200 行全高的
/// `/cost` 底部有 `Esc to cancel`,矮视口需滚动时页脚消失、只剩滚动指示符 `↓`)——只靠 ① 在
/// 矮视口下会把面板判成 `Unknown`,方向安全但 Esc 自愈永不触发,面板会一直堵着这条会话。
/// 三个面板(`/status` `/cost` `/config`)共用同一块 tab 栏,是**唯一稳定的面板身份**。
const PANEL_TABS: &[&str] = &["Settings", "Status", "Config", "Usage", "Stats"];

/// tab 栏要几个词同行命中才算数。取 3 是为了让助手正文里偶然并排出现两个同名词不构成面板。
const PANEL_TABS_MIN_HITS: usize = 3;

/// 页脚特征只在末尾这几行里认:scrollback 里出现同样字样(如助手正文引用过这句)不算数。
const FOOTER_TAIL_LINES: usize = 6;

/// 已注册的 modal 种类。**只登记有真机采样的形态**;认不出的静止画面一律 `Unknown`,不猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKind {
    /// 全屏面板(`/status` `/cost` `/config`):无输入框 + 页脚 `Esc to cancel`。
    Panel,
    /// 选择菜单:光标停在编号选项上(`❯ 1. Yes`)。
    Menu,
}

/// 一屏 TUI 的注入相关状态。**这五态互斥且穷尽**,新增形态只能从 `Unknown` 里切出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    /// 输入框就绪:末尾存在一个**空的** `❯` 行。唯一可无条件投喂的状态。
    InputReady,
    /// 运行中:存在「… 结尾的 spinner 行」**且输入框是空的**。带 marker 的注入有自己的两相
    /// 送达协议,故它对 prompt 类闸是放行的;但**绝不允许被归类成 modal**(那会让 Esc 打断别人的轮)。
    Running,
    /// 输入框里有未发送的文本 —— attach 用户打了一半的草稿,或上一次注入的 Enter 被吞。
    /// 往上面接着打字 = 把两段文本拼成一句,比丢一条消息更糟。
    ///
    /// **它优先于 `Running`**:最容易出现草稿的时刻恰恰是「用户 attach 进去看一轮长任务、
    /// 边看边打字」,那一屏 spinner 与草稿同时在。spinner 先判就等于草稿保护在最常见的场景下
    /// 根本不存在 —— `PromptQueue` 会放行,而 claude 的 `clear_input_before_send()` 为 false,
    /// `send_line` 是纯追加,飞书正文会被拼在用户那半句后面一起提交(2026-08-14 对抗审查实证)。
    Draft,
    /// 命中特征库的已知 modal。
    Modal(ModalKind),
    /// 以上皆不是,或抓屏本身失败/空屏。fail-closed。
    Unknown,
}

/// 注入闸的策略:调用方**必须**显式选一个,新增注入口在编译期就被逼着表态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectGate {
    /// 普通 prompt / 轮内 steer:`InputReady` 或 `Running` 都放行 —— 运行中排队投喂是 claude
    /// 支持的语义,且这条路径带 transport marker、有自己的送达判据。modal / 草稿 / 未知一律拒。
    PromptQueue,
    /// 斜杠命令、peers 热更新、菜单前置操作:**必须**空输入框。运行中注入斜杠命令只会被当成
    /// 一段 prompt 文本排队(`/compact` 变成聊天内容),那是静默的语义损坏。
    IdleOnly,
    /// 冷启期:会话刚由我们自己拉起,pane 归属明确(不存在 attach 用户的草稿/面板),且冷启有
    /// `wait_ready` / 信任目录对话的专用等待。这里**不抓屏、不判定**,名字直白地记着这是一个
    /// 未经证明的注入 —— 它不在 spec 点名的三条盲注路径里,压上闸只会把「起不来」换成新的失败态。
    StartupUnproven,
}

/// 某状态下该策略是否放行注入。
pub fn gate_accepts(gate: InjectGate, state: PaneState) -> bool {
    match gate {
        InjectGate::StartupUnproven => true,
        InjectGate::PromptQueue => matches!(state, PaneState::InputReady | PaneState::Running),
        InjectGate::IdleOnly => matches!(state, PaneState::InputReady),
    }
}

/// 是否**授权**对该状态按一次 Esc 做试探性自愈。
///
/// 只授权 `Panel`。理由(owner 2026-08-13 裁决「允许试探性 Esc,但按 spec 的授权条件执行」之下
/// 的进一步收紧):
/// - Panel 的页脚 `Esc to cancel` 是 **TUI 自己印在屏幕上的 Esc 语义**,按它等于照做,且面板
///   definitionally 不是一轮对话,退出它不打断任何人。
/// - Menu 不授权:编号菜单也可能是**工具授权提示**,此时轮次在跑却没有 spinner(claude 停下来
///   等人),按 Esc 等于替用户取消工具调用 / 打断别人的轮 —— 正是废弃分支被否决的那个错误。
///   而 `/effort` 那类菜单已有飞书中介卡与陈旧点击防护兜底,不缺这次自愈。
/// - Draft 永不授权:Esc 打在带草稿的输入框上会清掉用户打了一半的文字。
///
/// **已知残留(如实记下,未消除)**:面板是**底部锚定**的,一轮跑着时 spinner 仍留在面板上方
/// 可见(真机 2026-08-14 全高抓屏实证,不是「被盖住」),地平线判定会把这一屏判成
/// `Modal(Panel)` 而不是 `Running` —— 「Running 必须先于 modal」的排序在面板屏上保护不了任何
/// 东西。今天靠的是「面板自己的 Esc 语义就是关面板」(页脚 `Esc to cancel` 是 TUI 印在屏上的
/// 凭据),关掉面板之后那一轮照旧在跑。`/cost` 的页脚随视口高度出现/消失:200 行全高抓屏底部
/// 采样到了同一句 `Esc to cancel`(2026-08-14),矮视口下页脚消失时,其 Esc 语义是从同族
/// tab 栏推出的同一语义。
pub fn esc_authorized(state: PaneState) -> bool {
    matches!(state, PaneState::Modal(ModalKind::Panel))
}

/// 输入行上那段字是 **claude 自己画上去的**(答完一轮后的建议下一句、空会话的 placeholder),
/// 而不是用户敲了一半的草稿。判据是**样式**,所以入参必须是带转义的抓屏(`capture-pane -pe`)。
///
/// 真机 2026-09-09(`ghost-probe`)的逐字节对照,同一条输入行:
/// - claude 画的:`\x1b[39m❯\u{a0}\x1b[2m再列一下 …\x1b[0m` —— 整段被 SGR 2(dim)包着;
/// - 用户草稿:  `\x1b[39m❯\u{a0}这是一段真草稿`            —— 提示符之后直接是正文,没有 dim。
///
/// 两者在**纯文本**抓屏里逐字节同形(都是 `❯ <非空文字>`),所以 [`classify_pane`] 只能把
/// claude 画的字判成 [`PaneState::Draft`] —— 那正是「输入框里有一段没发送的文字」这句我们其实
/// 没有证据的话。已知被它坑到的两处:`/compact` 这类慢命令的完成判据会一直等到硬顶把成功报成
/// 超时;通用 slash 执行器的 print 类收尾判不出、只能回「不确定」。
///
/// 注入闸另有一条更贵但不依赖样式的证据(按一次 End 看光标动不动,见 `input_gate`),两条互不
/// 替代:这条便宜、可在轮询循环里每拍用,那条在拿不到样式时仍然成立。
pub fn prompt_text_is_dim(styled_pane: &str) -> bool {
    let Some(line) = styled_pane
        .lines()
        .rfind(|line| strip_ansi(line).trim_start().starts_with(PROMPT_MARKER))
    else {
        return false;
    };
    let Some((_, after)) = line.split_once(PROMPT_MARKER) else {
        return false;
    };
    // 提示符与正文之间是 NBSP(真机形态),也容普通空格;跳过它们再看第一个转义序列。
    let rest = after.trim_start_matches([' ', '\u{a0}']);
    sgr_params(rest).is_some_and(|params| params.contains(&2))
}

/// 串首若是一条 SGR 序列(`ESC [ … m`),返回它的参数表;不是则 `None`。
fn sgr_params(text: &str) -> Option<Vec<u16>> {
    let body = text.strip_prefix("\u{1b}[")?;
    let end = body.find('m')?;
    let params = &body[..end];
    if !params.chars().all(|c| c.is_ascii_digit() || c == ';') {
        return None;
    }
    Some(
        params
            .split(';')
            .map(|part| part.parse::<u16>().unwrap_or(0))
            .collect(),
    )
}

/// [`classify_pane`] 的**带样式**版本:除了纯文本判据,再用样式把「claude 画的字」从草稿里摘出来。
///
/// 只放宽一条:纯文本判成 [`PaneState::Draft`],而样式证明那段字是 claude 画的(见
/// [`prompt_text_is_dim`])→ 判 [`PaneState::InputReady`]。其余状态逐字节沿用纯文本判据 ——
/// 样式只用来**证否草稿**,不参与 spinner / 面板那几条。
pub fn classify_pane_styled(styled_pane: &str) -> PaneState {
    let state = classify_pane(styled_pane);
    if state == PaneState::Draft && prompt_text_is_dim(styled_pane) {
        return PaneState::InputReady;
    }
    state
}

/// claude 把多行粘贴折叠成的占位(`[Pasted text #1 +11 lines]`)的开头。
const PASTE_PLACEHOLDER: &str = "[Pasted text #";

/// 没折叠时,输入行至少要显示出正文第一行开头这么多字才算「就是它」;第一行更短时整行相等。
/// 短的一截不够:未编号菜单的 `❯ Yes`、attach 用户打的半句草稿,都可能恰好是正文的开头。
const PROMPT_PROOF_CHARS: usize = 24;

/// 输入框里停着的是**这条注入的正文**、还没发出去。注入后补按回车的唯一依据,三条都要证得:
/// ① 纯文本判成 [`PaneState::Draft`];
/// ② 输入行显示的是它:没折叠时见 [`PROMPT_PROOF_CHARS`];多行粘贴被折叠成占位时,占位只说明
///    输入框里是一段粘贴、不说明是哪一段 —— 注入闸已拒掉带草稿的输入框,所以它是本轮正文,
///    除非有人 attach 进去、在送达窗口里自己又输入或粘贴了内容。只看 `❯` 那一行:占位之后的字、
///    多行正文的后续行都不检查。草稿态本身证明不了这段字是谁的:
///    没编号的选择菜单(信任门的 `❯ No, exit`)也判成草稿,回车落上去就替人选掉了默认项;
/// ③ 样式证明那段字不是 claude 画的建议句(见 [`prompt_text_is_dim`])。
///
/// `styled` 只在前两条成立时才取(多一次抓屏);取不到样式就证不了 ③,返回 false。
pub fn holds_unsent_prompt(
    pane: &str,
    first_line: &str,
    styled: impl FnOnce() -> Option<String>,
) -> bool {
    classify_pane(pane) == PaneState::Draft
        && pending_input(pane).is_some_and(|shown| input_shows_prompt(&shown, first_line))
        && styled().is_some_and(|styled| classify_pane_styled(&styled) == PaneState::Draft)
}

fn input_shows_prompt(shown: &str, first_line: &str) -> bool {
    if shown.starts_with(PASTE_PLACEHOLDER) {
        return true;
    }
    let first_line = first_line.trim();
    let needed = first_line.chars().count().min(PROMPT_PROOF_CHARS);
    needed > 0 && shown.chars().count() >= needed && first_line.starts_with(shown)
}

/// pane 是否处于「空输入框」态。`/compact` 这类不打完成标记的命令用它判收尾 —— 旧判据
/// `pane.contains('❯')` 会把菜单光标行、scrollback 回显、用户草稿全算成就绪。
pub fn is_input_ready(pane: &str) -> bool {
    classify_pane(pane) == PaneState::InputReady
}

/// 把一屏 pane 文本判成 [`PaneState`]。顺序即优先级,不可随意调换:
/// ① 空屏 → 证明不了(死会话的 `capture` 返回 `Ok("")`,见 `pane_proves_session_gone`);
/// ② 输入行**拿着草稿** → `Draft`,**先于 spinner**(理由见 [`PaneState::Draft`]:草稿与 spinner
///    同屏是最常见的形态,spinner 先判会让草稿保护整个失效);
/// ③ spinner 在跑 → `Running`(**必须先于 modal**:运行中的画面里也有菜单/面板残影时,判成
///    modal 就等于授权对运行中的轮按 Esc。注意这里不会吃掉 ② —— 草稿已经在上一步返回了);
/// ④ 输入行的形态决定剩下的两态(空 / 菜单光标);
/// ⑤ 没有输入行时才轮到面板特征库;⑥ 其余 `Unknown`。
///
/// ②③④ 都只看**面板地平线以下**(见 [`below_horizon`]):面板是底部锚定的,横杠上方是被它
/// 压住的陈旧转录,那里的 `❯` 回显 / 工具框线 / spinner 都是历史,拿它们判「此刻」就是把面板屏
/// 误判成 `Draft`(锁死会话)或 `Running`(`PromptQueue` 放行,把整条 prompt 注进面板)。
/// ⑤ 仍看整屏:横杠与 tab 栏本身就是地平线上的面板身份。无面板时地平线为 0,①—⑥ 逐字节同旧判据。
pub fn classify_pane(pane: &str) -> PaneState {
    let plain = strip_ansi(pane);
    if plain.trim().is_empty() {
        return PaneState::Unknown;
    }
    let lines: Vec<&str> = plain.lines().collect();
    let below = below_horizon(&lines);
    let rest = last_prompt_rest(below);
    match rest {
        // 草稿先于 spinner。只有「非空且不是菜单光标」才算草稿:菜单光标(`❯ 1. Yes`)是
        // modal 的一部分,运行中的菜单仍该判 `Running`(工具授权提示等人,Esc 会替用户取消)。
        Some(text) if !text.is_empty() && !is_menu_option(text) => PaneState::Draft,
        _ if !is_ready_lines(below) => PaneState::Running,
        Some("") => PaneState::InputReady,
        Some(_) => PaneState::Modal(ModalKind::Menu),
        None if has_panel_feature(&lines) => PaneState::Modal(ModalKind::Panel),
        None => PaneState::Unknown,
    }
}

/// 冷启期 claude TUI 要求的下一个动作。**互斥且穷尽**,新形态只能从 [`Wait`] 里切出来。
///
/// [`Wait`]: ClaudeStartup::Wait
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeStartup {
    /// 信任门在屏上,光标**已经**落在「Yes, I trust this folder」——此刻的 Enter 才是「信任」。
    ConfirmTrust,
    /// 信任门在屏上,光标在别的选项上:朝信任项走一格,下一轮重新读屏再决定。
    MoveTrustCursor(TrustCursorMove),
    /// 信任门在屏上,但光标读不出来(渲染到一半、或文案换了认不出选项行)。**一个键都不许按**。
    TrustCursorUnknown,
    /// 已到输入提示,无需信任。
    Ready,
    /// 还在启动。
    Wait,
}

/// 把光标挪向信任项的方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustCursorMove {
    Down,
    Up,
}

/// 信任项的行特征。选项文案就是它的全部身份 —— 编号(`1. `)在 2.1.263 已经没有了,不能当锚。
const TRUST_OPTION: &str = "trust this folder";

/// 选项文案整体改掉时的兜底特征:各版都带这一句的疑问句尾。它只用来**否掉** `Ready`,
/// 命中之后照样定位不了选项行,结果仍是「一个键都不许按」。
const TRUST_GATE_HINT: &str = "you trust?";

/// 冷启期读屏:回答「现在该按什么」。
///
/// **绝不盲按 Enter。** 2.1.263 的信任门光标默认落在「No, exit」上(2.1.250 起如此),
/// 而 2026-08-14 抓到的旧版是 `❯ 1. Yes, I trust this folder` 在前 —— 同一个 Enter,
/// 在两个版本上一个是信任、一个是**退出**,退出会让 tmux 会话当场消失、冷启报「CLI 启动即退出」。
/// 所以位置只能从屏上读,不能从版本推:光标在哪就朝信任项走,走完重新读屏确认,确认不了就不按。
pub fn claude_startup(pane: &str) -> ClaudeStartup {
    let plain = strip_ansi(pane);
    let lines: Vec<&str> = plain.lines().collect();

    let trust_line = lines.iter().position(|l| l.contains(TRUST_OPTION));
    // 兜底特征只在输入框不在时才算数:resume 会把历史转录铺满一屏,里面聊到同样的字眼时,
    // 一个已经就绪的会话会被判成信任门,然后对着它空转满整个信任窗口。
    let gate_on_screen = trust_line.is_some()
        || (plain.contains(TRUST_GATE_HINT) && classify_pane(pane) != PaneState::InputReady);
    if !gate_on_screen {
        return if plain.contains(PROMPT_MARKER) {
            ClaudeStartup::Ready
        } else {
            ClaudeStartup::Wait
        };
    }

    // 信任门在屏上:从这里往下,`❯` 是菜单光标而不是输入行,任何一条路都不许再落到 `Ready`。
    let (Some(trust_line), Some(cursor_line)) = (
        trust_line,
        lines.iter().position(|l| l.contains(PROMPT_MARKER)),
    ) else {
        return ClaudeStartup::TrustCursorUnknown;
    };
    match cursor_line.cmp(&trust_line) {
        std::cmp::Ordering::Equal => ClaudeStartup::ConfirmTrust,
        std::cmp::Ordering::Less => ClaudeStartup::MoveTrustCursor(TrustCursorMove::Down),
        std::cmp::Ordering::Greater => ClaudeStartup::MoveTrustCursor(TrustCursorMove::Up),
    }
}

/// 输入行此刻拿着的文本:空串 = 空输入框,`Some("…")` = 有草稿 / 菜单光标 / 刚打进去还没提交的
/// 命令,`None` = 这一屏根本没有输入行(面板 / 未知)。**运行中也返回输入行内容** —— 与
/// [`classify_pane`] 不同,这里只回答「输入行上写着什么」,不做状态判定。
///
/// 用途:注入之后正向确认「命令已经提交」(输入行不再拿着它),取代盲补 Enter。
///
/// 同样只看地平线以下:面板上方陈旧转录里恰好有一条同名回显(`❯ /status`)时,把它当成
/// 「还没提交」会让提交确认往面板上补 Enter —— `/config` 面板上 Enter 是「选中并切换某项设置」。
pub fn pending_input(pane: &str) -> Option<String> {
    let plain = strip_ansi(pane);
    let lines: Vec<&str> = plain.lines().collect();
    last_prompt_rest(below_horizon(&lines)).map(|s| s.to_string())
}

/// 面板横杠即**地平线**:返回最后一条面板顶栏横杠(满宽 `▔`)以下的部分;无面板时原样返回。
///
/// 为什么必须有它(2026-08-14 真机证据裁决的 P1):面板是**底部锚定**而非整屏覆盖 —— 200 行
/// 全高抓屏里欢迎框与 `/cost` 面板同屏共存,横杠上方是被面板压住的陈旧转录。而面板自己的 `▔`
/// (U+2594,Block Elements)不满足 `is_box_drawing`(U+2500–257F),于是不加地平线时
/// `boxed_prompt_rest` 的 `rposition(is_full_rule)` 会越过面板、锚到上方转录里的工具输出框线
/// (`╰──╯` 是纯框线满宽行,转录里到处都是),再往上撞见历史回显 `❯ /cd …`,把面板屏误判成
/// `Draft`:两个闸双拒、Esc 不授权,会话被锁死无自愈,回执还怪用户「attach 时打了一半?」。
///
/// 前提「屏上有横杠 = 此刻有面板」经真机二次验证:面板关闭后全高抓屏零残留(`/config` 三次
/// Esc 退出后,横杠与 tab 栏一个都搜不到)。即使未来出现残留(如正文引用了一行 `▔`),输入框
/// 永远在残留之下的屏幕底部,仍在地平线以下 —— 判定只会少看陈旧内容,不会丢掉输入行。
fn below_horizon<'a, 'b>(lines: &'b [&'a str]) -> &'b [&'a str] {
    let horizon = lines
        .iter()
        .rposition(|l| crate::pane_output::is_panel_rule(l))
        .map_or(0, |i| i + 1);
    &lines[horizon..]
}

/// 输入行的内容。锚优先取输入框的**下框线**(结构性、不受下方托盘长度影响);抓屏里没有
/// 满宽下框线时才退回末尾窗口。scrollback 深处的历史回显 `❯ /cd …` 两条路径都不会命中:
/// 前者被框线切掉,后者在窗口之外。调用方须先切到地平线以下(见 [`below_horizon`]):
/// 面板屏上这里的「下框线」会错认成转录里的工具框线,那正是 P1 锁死事故的锚。
fn last_prompt_rest<'a>(lines: &[&'a str]) -> Option<&'a str> {
    boxed_prompt_rest(lines).or_else(|| tail_window_prompt_rest(lines))
}

/// 满宽下框线之上、框内最近的一个 `❯` 行。下框线以下的一切(空行 / 快捷键提示 / 后台任务托盘)
/// 自动被排除 —— 托盘再长都不影响。
fn boxed_prompt_rest<'a>(lines: &[&'a str]) -> Option<&'a str> {
    let bottom = lines.iter().rposition(|l| is_full_rule(l))?;
    lines[bottom.saturating_sub(INPUT_BOX_HEIGHT)..bottom]
        .iter()
        .rev()
        .find_map(|l| prompt_rest(l))
}

fn tail_window_prompt_rest<'a>(lines: &[&'a str]) -> Option<&'a str> {
    lines[lines.len().saturating_sub(PROMPT_TAIL_LINES)..]
        .iter()
        .rev()
        .find_map(|l| prompt_rest(l))
}

/// 一整行只有框线字符的满宽横杠。输入框的**上**框线带着会话名(`──── summon:soul-… ──`),
/// 只有下框线是纯框线 —— 正好是我们要的那一条。
pub(crate) fn is_full_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= INPUT_BOX_MIN_RULE && t.chars().all(is_box_drawing)
}

/// 一行若是 `❯` 提示行,返回其后的内容(已 trim)。允许行首有空白/框线字符:输入框在某些
/// 版本里带左框线,判据不该被一根竖线推翻。
pub(crate) fn prompt_rest(line: &str) -> Option<&str> {
    line.trim_end()
        .trim_start_matches(|c: char| c.is_whitespace() || is_box_drawing(c))
        .strip_prefix(PROMPT_MARKER)
        .map(str::trim)
}

fn is_box_drawing(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// `1. Yes, switch to xhigh` 这种编号选项形态(菜单光标停在选项上)。
pub(crate) fn is_menu_option(rest: &str) -> bool {
    match rest.split_once('.') {
        Some((num, _)) => !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// 命中任一条已注册面板特征。两条特征**取并集**:真机三个面板里没有一条页脚是共有的
/// (`/status` 有 `Esc to cancel`,`/cost` 只有滚动指示符,`/config` 是 `Type to filter …`),
/// 而 tab 栏三者都有;反过来 tab 栏未来若改版,页脚还能兜住一部分。
fn has_panel_feature(lines: &[&str]) -> bool {
    has_panel_footer(lines) || has_panel_tabs(lines)
}

fn has_panel_footer(lines: &[&str]) -> bool {
    lines[lines.len().saturating_sub(FOOTER_TAIL_LINES)..]
        .iter()
        .any(|l| {
            let low = l.to_lowercase();
            PANEL_FOOTERS.iter().any(|f| low.contains(f))
        })
}

/// tab 栏在面板**顶部**,不在末尾几行,**所以这里不设窗口**:真机整屏 59 行时 tab 栏距底约
/// 56 行,任何「末尾 N 行」的窗口都可能整个扫不到它(2026-08-14 对抗审查实证)。扫不到的后果是
/// 面板落 `Unknown` —— 既退不掉(Esc 不授权)又拒一切注入,会话被面板长期锁死,正是 spec 第一节
/// 要消灭的那个事故。
///
/// 不设窗口换来的假阳风险用**合取项**压掉:同屏必须还有一条满宽 `▔` 横杠(面板顶栏)。助手正文
/// 里偶然写出 `| Settings | Status | Config |` 这种表格行,不会同时画出一条满宽 `▔`。
///
/// 另一半前提是真机验证过的:面板**底部锚定**覆盖屏幕下部(不是整屏 —— 200 行全高抓屏里
/// 欢迎框与面板同屏共存,2026-08-14 复测),且**退出后不留任何 scrollback 残留**
/// (Esc 退出 `/status` `/cost` `/config` 之后全高抓屏里一个 tab 栏、一条横杠都搜不到),
/// 所以屏上同时出现 tab 栏与横杠就等于此刻屏上有面板。
fn has_panel_tabs(lines: &[&str]) -> bool {
    let tabs = lines.iter().any(|l| {
        let words: Vec<&str> = l.split_whitespace().collect();
        words.iter().filter(|w| PANEL_TABS.contains(w)).count() >= PANEL_TABS_MIN_HITS
    });
    tabs && lines.iter().any(|l| crate::pane_output::is_panel_rule(l))
}

/// fail-closed 的用户回执:**必须**说清「本条没有执行」和「怎么办」。绝不静默丢、绝不谎报成功;
/// 被拒消息不自动重试 —— 用户重发是唯一重试途径,重发仍过同一道闸。
pub fn refusal_notice(state: PaneState) -> String {
    let body = match state {
        PaneState::Modal(ModalKind::Panel) => {
            summon_i18n::text!("r9411ba9313")
        }
        PaneState::Modal(ModalKind::Menu) => {
            summon_i18n::text!("r4d1463a665")
        }
        PaneState::Draft => {
            summon_i18n::text!("rb880eb8bdb")
        }
        PaneState::Running => summon_i18n::text!("ra558283429"),
        PaneState::InputReady | PaneState::Unknown => {
            summon_i18n::text!("r02f13193e9")
        }
    };
    summon_i18n::format!("r7c6c2ca1d5")
}

/// Esc 自愈失败后的回执:必须写清「已经试过一次」,否则用户会以为我们什么都没做。
pub fn refusal_notice_after_esc(state: PaneState) -> String {
    summon_i18n::format!("re1d84224ef", refusal_notice(state))
}

/// 注入被闸拒绝。`Display` 即用户回执文案,便于经 `anyhow` 链一路带到出站侧。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{notice}")]
pub struct InjectionRefused {
    pub state: PaneState,
    pub notice: String,
    /// 拒因是菜单时,拒绝那一刻屏上的菜单快照。出站侧据它把菜单镜像成一张可点的飞书卡:
    /// 菜单只要一次按键就能解决,而纯文字回执只能让用户自己去 attach。
    /// 其余拒因恒 `None` —— 面板 / 草稿不是可镜像的问句。
    pub menu: Option<crate::interactive::InteractiveState>,
}

impl InjectionRefused {
    pub fn new(state: PaneState) -> Self {
        Self {
            state,
            notice: refusal_notice(state),
            menu: None,
        }
    }

    /// 带上拒绝那一刻的屏:菜单态顺手留一份菜单快照(见 [`Self::menu`])。
    pub fn on_pane(state: PaneState, pane: &str) -> Self {
        Self {
            menu: menu_snapshot(state, pane),
            ..Self::new(state)
        }
    }

    pub fn after_esc(state: PaneState) -> Self {
        Self {
            state,
            notice: refusal_notice_after_esc(state),
            menu: None,
        }
    }
}

/// 菜单态才取快照,且以 `parse_interactive` 认得出为准:认不出就没有可点的卡可发,
/// 与轮内交互中介同一条判据(点击侧的新鲜度复验也是它)。
///
/// 快照在这里就把尾部的 TUI 动作项剥掉(`strip_trailing_ui_actions`):这条路拿不到转录侧的真实
/// 选项数(那是轮内 tail 的账),而镜像出去的单选卡上只有选项按钮 —— 留着「Type something.」
/// 就是给用户一个点下去会卡死在 TUI 自由文本态的按钮。剥完不足 2 项就不是可镜像的菜单
/// (同 `parse_interactive` 的 ≥2 判据),宁可只发文字回执。
fn menu_snapshot(state: PaneState, pane: &str) -> Option<crate::interactive::InteractiveState> {
    matches!(state, PaneState::Modal(ModalKind::Menu))
        .then(|| crate::interactive::parse_interactive(pane))
        .flatten()
        .map(crate::interactive::strip_trailing_ui_actions)
        .filter(|menu| menu.options.len() >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机抓屏(2026-08-14,`soul-flare-5-g00000000a11ce033`):答完一轮、输入框空。
    const IDLE: &str = "  Review」二级目录(现有四个二级目录里它最贴,没有新建)\n\n\
✻ Churned for 2m 35s\n\
                          new task? /clear to save 320k tokens\n\
─────────── summon:soul-flare-5-g00000000a11ce033 ──\n\
❯                                                   \n\
────────────────────────────────────────────────────\n\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n";

    /// 真机抓屏(2026-09-09,`compact-probe`):`/compact` 压缩进行中。
    ///
    /// 形态的要害是**输入行同时是空的** —— 判「在跑」的全部依据就是进度条那一行(spinner 行的
    /// 字形逐帧轮换,这一帧恰好是曾经漏网的 `✶`)。判成 `InputReady` 有两个后果:`IdleOnly` 闸
    /// 会把 slash 命令注进正在压缩的会话,`is_command_done` 会把压缩报成已完成。
    const COMPACTING: &str = "❯ /compact\n\
✶ Compacting conversation…\n\
  ▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱ 19%\n\
────────────────────────────────────────────────────\n\
❯                                                   \n\
────────────────────────────────────────────────────\n";

    /// 真机抓屏(同次,压缩完成后):进度条与 spinner 一起消失,只剩收尾行与空输入行。
    const COMPACTED: &str = "❯ /compact\n\
  ⎿  Compacted (ctrl+o to see full summary)\n\
────────────────────────────────────────────────────\n\
❯                                                   \n\
────────────────────────────────────────────────────\n";

    /// 真机抓屏(同日,`soul-starlight-g00000000a11ce02a`):输入框里留着没发送的文字。
    const DRAFT: &str = "  其他 agent 不用这个模型,无需改动。\n\n\
✻ Brewed for 49s\n\n\
────────── summon:soul-starlight-g00000000a11ce02a ──\n\
❯ 重启一下 starlight\n\
─────────────────────────────────────────────────────\n\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n";

    /// 真机抓屏(同日,`soul-arcadia-g00000000a11ce02c`):scrollback 里有历史命令回显
    /// `❯ /cd …`,底部才是真·空输入行。旧判据 `contains('❯')` 对这两者一视同仁。
    const ECHO_THEN_IDLE: &str = "  权重还在下,46%)。\n\n\
✻ Cooked for 2m 57s\n\n\
❯ /cd /Users/example/Desktop/business/2608_demoapp_lm_benchmark\n\
  ⎿  Moved to /Users/example/Desktop/business/2608_demoapp_lm_benchmark\n\
                     new task? /clear to save 410.4k tokens\n\
─────────── summon:soul-arcadia-g00000000a11ce02c ──\n\
❯                                                   \n\
────────────────────────────────────────────────────\n\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n";

    /// 真机抓屏(2026-08-14,claude 2.1.231,`/status`):整屏没有输入框,顶部 tab 栏,
    /// 底部页脚 `Esc to cancel`。面板正文含账号邮箱与 session id —— 裁决 2 让它走私聊的原因。
    const PANEL: &str = "│      someone1@example.com's Organization           │ /release-notes …\n\
▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n   \
Settings  Status   Config   Usage   Stats\n\n   \
Version:          2.1.231\n   Session ID:       9a86241d-6596-4b93-b110-9a611c935bee\n   \
Email:            someone1@example.com\n   Model:            opus (claude-opus-5)\n\n   \
Esc to cancel\n";

    /// 真机抓屏(2026-08-14,claude 2.1.231,probe 会话,裸 `/goal` 无目标时):底部锚定面板,
    /// 满宽 ▔ 顶栏 + 页脚 `Esc to dismiss`,**没有 tab 栏** —— 页脚是它唯一的正向特征。
    const GOAL_PANEL: &str = "╰──────────────────────────────────────╯\n\
▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n   \
Goal\n   No goal set\n   /goal <condition> to set one\n   Esc to dismiss\n";

    /// 同次采样:目标激活时的同一块面板(`/goal 完成一个测试目标` 之后再裸 `/goal`)。
    /// 正文形态不同(`◎ Goal active` + 运行秒数/token 数),页脚一致。
    const GOAL_PANEL_ACTIVE: &str = "  Ran 1 shell command\n  \
⎿  Interrupted · What should Claude do instead?\n\
▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n   \
◎ Goal active\n   running 37s · 586 tokens\n   Goal: 完成一个测试目标\n   \
/goal clear to stop early · Esc to dismiss\n";

    /// 真机抓屏(同日,`/cost`):**底部没有页脚**,最后一行只有滚动指示符 `↓`。
    /// 只认页脚的旧特征库对它恒 `Unknown` —— 这就是要加 tab 栏特征的直接原因。
    const PANEL_NO_FOOTER: &str = "▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n   \
Settings  Status   Config   Usage   Stats\n\n   Session\n\n   \
Total cost:            $0.0000\n   Total duration (wall): 1m 8s\n\n   \
Current week (all models)\n   ███████████▌       23% used\n   \
Resets Aug 20 at 2:59pm (Asia/Singapore)\n   \
Approximate, based on local sessions on this machine — does not include other devices or\n   \
claude.ai\n                                                                        ↓\n";

    /// 真机抓屏(同日,冷启信任目录对话框):**同时**带编号选项与 `Esc to cancel` 页脚。
    /// 它证明「输入行判定必须先于面板特征」不是理论顾虑:顺序反了,这个对话框会被判成
    /// 已注册面板并挨一次 Esc —— 那按的是「2. No, exit」旁边的取消键。
    const TRUST_DIALOG: &str = " Quick safety check: Is this a project you created or one you \
trust?\n\n Claude Code'll be able to read, edit, and execute files here.\n\n \
❯ 1. Yes, I trust this folder\n   2. No, exit\n\n Enter to confirm · Esc to cancel\n";

    /// 真机抓屏(2026-09-08,claude 2.1.263,`tmux new-session -c <新临时目录> claude`,逐行
    /// 复制自 `capture-pane -p`):**选项没有编号**,而且 `❯` 默认落在 `No, exit` 上。
    /// 它与 [`TRUST_DIALOG`](老版,`❯ 1. Yes, I trust this folder` 在前)是同一个对话框的两副
    /// 面孔 —— 同一个盲 Enter,在这一版上按的是「退出」。
    const TRUST_DIALOG_2_1_263: &str = " Accessing workspace:\n\n \
/private/tmp/claude-501/trustprobe-1788843174\n\n \
Quick safety check: Is this a project you created or one you trust? (Like your own code, a \
well-known open source project, or work from your team). If not, take a moment to review \
what's in this\n folder first.\n\n \
Claude Code'll be able to read, edit, and execute files here.\n\n \
Security guide\n\n \
❯ No, exit\n   Yes, I trust this folder\n\n \
Enter to confirm · Esc to cancel\n";

    /// 同一次真机采样,按一下 `Down` 之后的同一屏:光标挪到了信任项。这才是可以 Enter 的时刻。
    const TRUST_DIALOG_2_1_263_MOVED: &str = " Claude Code'll be able to read, edit, and execute \
files here.\n\n Security guide\n\n   No, exit\n ❯ Yes, I trust this folder\n\n \
Enter to confirm · Esc to cancel\n";

    /// 冷启对新目录时,盲按 Enter 会打死会话 —— 所以这一屏的答案必须是「先挪光标」,
    /// 而不是「确认」。把 `ConfirmTrust` 写回这里,tmux 会话会在冷启那一刻消失。
    #[test]
    fn the_2_1_263_trust_gate_asks_to_move_the_cursor_not_to_confirm() {
        assert_eq!(
            claude_startup(TRUST_DIALOG_2_1_263),
            ClaudeStartup::MoveTrustCursor(TrustCursorMove::Down)
        );
        assert_eq!(
            claude_startup(TRUST_DIALOG_2_1_263_MOVED),
            ClaudeStartup::ConfirmTrust
        );
    }

    /// 反向布局(2026-08-14 真机抓屏,信任项在前)必须往**上**挪 —— 方向是从屏上读出来的,
    /// 不是写死的。光标已在信任项上时不再多按一格。
    #[test]
    fn the_cursor_moves_toward_the_trust_option_from_either_side() {
        assert_eq!(claude_startup(TRUST_DIALOG), ClaudeStartup::ConfirmTrust);
        let cursor_on_exit = TRUST_DIALOG
            .replace("❯ 1. Yes", "  1. Yes")
            .replace("   2. No, exit", " ❯ 2. No, exit");
        assert_eq!(
            claude_startup(&cursor_on_exit),
            ClaudeStartup::MoveTrustCursor(TrustCursorMove::Up)
        );
    }

    /// 信任门在屏上却读不出光标(渲染到一半、或文案改到认不出选项行):**一个键都不许按**。
    /// 走不到 `Ready` 是这里的要害 —— 那会让冷启把整条 prompt 注进对话框,
    /// 而对话框上的每一次按键都可能落到「No, exit」。宁可等到超时报错,会话还活着。
    #[test]
    fn an_unreadable_trust_gate_never_degrades_into_ready() {
        let half_rendered = " Quick safety check: Is this a project you created or one you \
trust?\n\n Claude Code'll be able to read, edit, and execute files here.\n";
        assert_eq!(
            claude_startup(half_rendered),
            ClaudeStartup::TrustCursorUnknown
        );
        // 选项行在、光标还没画出来:同样不许按。
        let no_cursor = TRUST_DIALOG_2_1_263.replace("❯ No, exit", "  No, exit");
        assert_eq!(
            claude_startup(&no_cursor),
            ClaudeStartup::TrustCursorUnknown
        );
    }

    /// resume 把历史转录铺满一屏,里面正好聊到信任门 —— 输入框就在那儿,这是个就绪的会话。
    /// 判成信任门的代价是每次 resume 都对着它空转满整个信任窗口。
    #[test]
    fn a_resumed_transcript_that_mentions_the_gate_is_still_ready() {
        let recalled =
            format!("  上一轮聊的就是「or one you trust?」这句怎么解析。\n{TRAY_BELOW_INPUT}");
        assert_eq!(claude_startup(&recalled), ClaudeStartup::Ready);
    }

    /// 没有信任门时才回答启动进度:输入行在 = 就绪,空屏 = 继续等。
    #[test]
    fn without_a_trust_gate_the_input_line_means_ready() {
        assert_eq!(claude_startup(TRAY_BELOW_INPUT), ClaudeStartup::Ready);
        assert_eq!(claude_startup("\n  Loading…\n"), ClaudeStartup::Wait);
    }

    /// AskUserQuestion 的 TUI 菜单(真机形态,Claude Code 2.1.269):对话框顶线紧贴着用户那条 prompt
    /// 回显,每个真选项下跟缩进描述行,尾部固定挂着「Type something.」与「Chat about this」两个
    /// claude 自己的动作项,它俩之间还画着一条与顶线同宽的横线。
    const ASK_USER_QUESTION: &str = "❯ 帮我挑个部署环境\n\
────────────────────────────────────────────────────────────────────────\n\
 ☐ 部署环境\n\n你希望部署到哪个环境?\n\n\
❯ 1. Staging（推荐）\n     预发布环境,与生产配置一致但不面向真实用户。\n\
  2. Production\n     生产环境,直接面向真实用户。\n\
  3. Type something.\n\
────────────────────────────────────────────────────────────────────────\n\
  4. Chat about this\n\nEnter to select · ↑/↓ to navigate · Esc to cancel\n";

    /// 真机抓屏(2026-08-14,`soul-flare-5-g00000000a11ce04d`,逐行复制自
    /// `capture-pane -p -S -500` 的尾部):**输入行之后还有 8 行** —— 下框线 / 空行 / 快捷键提示 /
    /// 空行 / `⏺ main` / 3 行后台 agent 托盘。claude 把后台 agent 与 dynamic workflow 渲染在
    /// 输入框下方,每多一个后台任务就多一行。
    const TRAY_BELOW_INPUT: &str = "  在飞:P0 阻塞修复、空闲卡枚举修复、透传内部审查环。\n\n\
✻ Waiting for 2 background agents and 1 dynamic workflow to finish\n\n\
────────────────────────── summon:soul-flare-5-g00000000a11ce04d ──\n\
❯                                                                  \n\
───────────────────────────────────────────────────────────────────\n\n  \
⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n\n  \
⏺ main\n  \
◯ general-purpose         修迁移枚举静默漏项            7m 35s · ↓ 110.0k tokens\n  \
◯ general-purpose         修 P0 活跃时间冻结阻塞          31s · ↓ 50.5k tokens\n  \
◯ slash-passthrough-impl  按冻结 spec 实现 slash 透传   1h 51m · ↓ 1.3m tokens\n";

    /// 后台任务托盘会把空 `❯` 顶出任何固定的「距底 N 行」窗口 —— 而 `Unknown` 对 `IdleOnly` 与
    /// `PromptQueue` **双双拒绝**、也不授权 Esc:这条会话对飞书整体失联,且 attach 也修不好
    /// (屏没坏,是判据窗口太窄)。所以锚必须取输入框自己的下框线,与托盘长度无关。
    #[test]
    fn a_growing_background_task_tray_never_hides_the_input_line() {
        assert_eq!(classify_pane(TRAY_BELOW_INPUT), PaneState::InputReady);
        // 再挂 6 个后台任务(真机日常形态):判定一个字都不许变。
        let extra: String = (0..6)
            .map(|i| format!("  ◯ general-purpose  子任务{i}   1m · ↓ 1.0k tokens\n"))
            .collect();
        let crowded = format!("{TRAY_BELOW_INPUT}{extra}");
        assert_eq!(
            classify_pane(&crowded),
            PaneState::InputReady,
            "托盘再长也不该改变输入态判定"
        );
        assert!(is_input_ready(&crowded), "`/compact` 的收尾判据同源,一起守");
        assert!(gate_accepts(InjectGate::IdleOnly, classify_pane(&crowded)));
        assert_eq!(pending_input(&crowded).as_deref(), Some(""));
    }

    /// 草稿必须**先于** spinner 判定。用户 attach 去看一轮长任务、边看边打字是最常见的形态,
    /// spinner 先判会让 `PromptQueue` 放行 —— claude 不清输入框,飞书正文会被拼在用户那半句后面
    /// 一起提交(比丢一条消息更糟)。
    #[test]
    fn a_draft_beats_the_spinner_so_the_gate_can_still_see_it() {
        let running_with_draft = "✽ Composing… (3m 2s · ↓ 12.0k tokens)\n\
────────────────────────── summon:soul-flare-5-g00000000a11ce04d ──\n\
❯ 我打了一半还没发\n\
───────────────────────────────────────────────────────────────────\n";
        assert_eq!(classify_pane(running_with_draft), PaneState::Draft);
        assert!(
            !gate_accepts(InjectGate::PromptQueue, classify_pane(running_with_draft)),
            "带草稿的运行中输入框绝不放行 —— 放行就是把两段文本拼成一句"
        );
        assert!(!esc_authorized(classify_pane(running_with_draft)));
        assert_eq!(
            pending_input(running_with_draft).as_deref(),
            Some("我打了一半还没发")
        );
        // 空输入框 + spinner 仍是 Running(带 marker 的两相协议照旧放行)。
        let running_clean = "✽ Composing… (3m 2s · ↓ 12.0k tokens)\n\
────────────────────────── summon:soul-x ──\n❯ \n──────────────────────────────\n";
        assert_eq!(classify_pane(running_clean), PaneState::Running);
        // 运行中的菜单(工具授权提示等人)仍判 Running:Esc 在那儿是替用户取消工具调用。
        let running_menu = "✽ Composing… (3s)\n❯ 1. Yes\n  2. No\n";
        assert_eq!(classify_pane(running_menu), PaneState::Running);
        assert!(!esc_authorized(classify_pane(running_menu)));
    }

    #[test]
    fn empty_prompt_line_at_tail_is_the_only_ready_proof() {
        assert_eq!(classify_pane(IDLE), PaneState::InputReady);
        assert_eq!(
            classify_pane(ECHO_THEN_IDLE),
            PaneState::InputReady,
            "scrollback 里的历史回显不该影响底部输入行的判定"
        );
        assert!(is_input_ready(IDLE));
    }

    /// 收紧现行 `/compact` 弱判据的核心回归:三种「含 ❯ 但不就绪」的形态一个都不许算就绪。
    #[test]
    fn prompt_marker_alone_never_proves_ready() {
        let menu = "Change effort level?\n❯ 1. Yes, switch to xhigh\n  2. No";
        let scrollback_echo = "❯ /clear\n  ⎿  cleared\n";
        for pane in [DRAFT, menu, scrollback_echo] {
            assert!(pane.contains('❯'), "样本必须含 ❯,否则测的不是这条");
            assert!(!is_input_ready(pane), "含 ❯ ≠ 就绪: {pane:?}");
        }
        assert_eq!(classify_pane(DRAFT), PaneState::Draft);
        assert_eq!(classify_pane(menu), PaneState::Modal(ModalKind::Menu));
        assert_eq!(
            classify_pane(scrollback_echo),
            PaneState::Draft,
            "只剩回显、没有输入框的画面证明不了就绪(判成非就绪即可,方向安全)"
        );
    }

    #[test]
    fn full_screen_panel_is_a_registered_modal_not_idle() {
        assert_eq!(classify_pane(PANEL), PaneState::Modal(ModalKind::Panel));
        assert!(!is_input_ready(PANEL));
        // 页脚字样出现在 scrollback 深处(助手正文引用了这句)不算面板。
        let quoted = format!("助手说:底部会显示 Esc to cancel\n{IDLE}");
        assert_eq!(classify_pane(&quoted), PaneState::InputReady);
    }

    /// `/goal` 目标面板(2026-08-14 真机)**没有 tab 栏**,页脚是 `Esc to dismiss` ——
    /// 不注册这条页脚,面板落 `Unknown`:既退不掉(Esc 不授权)又拒一切注入,裸 `/goal` 的
    /// panel 收尾永远等到超时,面板一直堵着这条会话。两种正文形态(无目标 / 目标激活)都要认。
    #[test]
    fn the_goal_panel_is_recognized_by_its_dismiss_footer() {
        for pane in [GOAL_PANEL, GOAL_PANEL_ACTIVE] {
            assert!(
                !pane.to_lowercase().contains("esc to cancel"),
                "样本必须不含 `Esc to cancel`,否则测的不是 dismiss 这条"
            );
            let state = classify_pane(pane);
            assert_eq!(state, PaneState::Modal(ModalKind::Panel), "{pane:?}");
            assert!(esc_authorized(state), "页脚是 TUI 自己印的 Esc 语义");
            assert!(!gate_accepts(InjectGate::IdleOnly, state));
            assert_eq!(pending_input(pane), None, "面板屏没有输入行");
        }
    }

    /// 没有页脚的面板(`/cost` 真机)必须靠 tab 栏认出来,否则它落进 `Unknown`:
    /// 方向仍安全,但 Esc 自愈不触发 —— 面板会一直堵着这条会话直到有人 attach。
    #[test]
    fn footerless_panel_is_recognized_by_its_tab_bar() {
        assert!(
            !PANEL_NO_FOOTER.to_lowercase().contains("esc to cancel"),
            "样本必须不含页脚,否则测的不是 tab 栏这条"
        );
        assert_eq!(
            classify_pane(PANEL_NO_FOOTER),
            PaneState::Modal(ModalKind::Panel)
        );
        assert!(esc_authorized(classify_pane(PANEL_NO_FOOTER)));
        // 正文里零散提到这几个词不构成 tab 栏(要求同行命中 ≥3)。
        let prose = format!("我查了 Status,又看了 Config,还有别的\n{IDLE}");
        assert_eq!(classify_pane(&prose), PaneState::InputReady);
    }

    /// tab 栏在面板**顶部**:整屏 59 行时它距底约 56 行,任何「末尾 N 行」的窗口都扫不到它。
    /// 扫不到 = 面板落 `Unknown` = 既退不掉又拒一切注入,会话被面板锁死。
    #[test]
    fn a_tab_bar_at_the_top_of_a_full_height_screen_is_still_found() {
        // 把真机 `/cost` 面板补满到 59 行(面板整屏覆盖,底部是空白与滚动指示符)。
        let padded = format!("{PANEL_NO_FOOTER}{}", "\n".repeat(50));
        assert!(
            padded.lines().count() > 50,
            "样本必须比旧的 50 行窗口高,否则测的不是这条"
        );
        assert_eq!(classify_pane(&padded), PaneState::Modal(ModalKind::Panel));
        assert!(esc_authorized(classify_pane(&padded)));
    }

    /// tab 栏不设窗口的代价补偿:必须同屏还有一条满宽 `▔` 横杠。助手正文里的表格行
    /// `| Settings | Status | Config |` 命中三个词,但画不出横杠 —— 不该被判成面板并挨一次 Esc。
    #[test]
    fn a_prose_table_row_without_the_panel_rule_is_not_a_panel() {
        let table = "  下面这张表列了三块:\n  | Settings | Status | Config | Usage |\n  \
  ——就这些,没有别的。\n";
        assert_ne!(
            classify_pane(table),
            PaneState::Modal(ModalKind::Panel),
            "只有 tab 词、没有满宽横杠,不构成面板"
        );
        assert!(!esc_authorized(classify_pane(table)));
    }

    /// 顺序回归:输入行判定必须先于面板特征。信任目录对话框同屏带页脚 `Esc to cancel`,
    /// 判成 Panel 就意味着授权对一个「Yes / No, exit」选择框按 Esc。
    #[test]
    fn menu_that_also_shows_a_panel_footer_is_still_a_menu() {
        assert!(TRUST_DIALOG.to_lowercase().contains("esc to cancel"));
        assert_eq!(
            classify_pane(TRUST_DIALOG),
            PaneState::Modal(ModalKind::Menu)
        );
        assert!(!esc_authorized(classify_pane(TRUST_DIALOG)));
    }

    /// 运行中永远是 `Running`,绝不能被归类成 modal —— 归错的代价是对别人的轮按 Esc。
    #[test]
    fn running_pane_is_never_a_modal() {
        let running = "✢ Compacting… (2m 1s · ↓ 12.0k tokens)\n──────\n❯ \n";
        assert_eq!(classify_pane(running), PaneState::Running);
        assert!(!esc_authorized(classify_pane(running)));
        // 「Running 先于 modal」守的是**菜单/页脚**类(工具授权提示等人):spinner 与页脚字样
        // 同屏、又没有面板横杠时,仍判 Running。带 ▔ 横杠的面板屏不在此列 —— 面板底部锚定,
        // 横杠上方的 spinner 属于被压住的陈旧转录,那一屏判 Panel
        // (见 a_spinner_above_the_panel_never_makes_the_screen_running)。
        let running_footer = "✽ Composing… (3s)\n\n Esc to cancel\n";
        assert_eq!(classify_pane(running_footer), PaneState::Running);
        assert!(!esc_authorized(classify_pane(running_footer)));
    }

    /// 真机复合样本(2026-08-14 修真机证据裁决):真实转录(`❯ bot「炼金师」…` 历史回显、
    /// `⎿` 工具结果、`❯ /cd …` 回显、`╭─╮╰─╯` 工具输出框、运行中形态 spinner)+ 底部锚定的
    /// 真实 `/status` 面板(200 行全高抓屏尾部 18 行)。组装配方与逐屏原件见修真机证据包
    /// (e2e-probe,2026-08-14)。
    const PANEL_OVER_TRANSCRIPT: &str =
        include_str!("../../tests/fixtures/pane_panel_over_transcript.txt");

    /// 修法 1 的点名测试(P1):面板是**底部锚定**而非整屏覆盖,面板自己的 `▔`(U+2594)又不是
    /// box-drawing(U+2500–257F)—— 旧锚 `rposition(is_full_rule)` 于是越过面板、锚到上方陈旧
    /// 转录里的工具框线(`╰──╯`),再往上撞见历史回显 `❯ /cd …`,把面板屏判成 `Draft`:两闸
    /// 双拒、Esc 不授权,会话锁死无自愈,回执还怪用户。地平线修法:判定只看最后一条面板横杠
    /// 以下。**改回旧锚,这条必红。**
    #[test]
    fn a_bottom_anchored_panel_over_stale_transcript_is_a_panel_not_a_draft() {
        let pane = PANEL_OVER_TRANSCRIPT;
        // 样本必须真的带着全套干扰项,否则测的不是这条。
        assert!(pane.contains("❯ /cd "), "须含历史回显(旧锚的 Draft 来源)");
        assert!(
            pane.lines().any(is_full_rule),
            "须含纯框线满宽行(旧锚的假锚点)"
        );
        assert!(pane.lines().any(crate::pane_output::is_panel_rule));
        let state = classify_pane(pane);
        assert_eq!(
            state,
            PaneState::Modal(ModalKind::Panel),
            "不是 Draft,不是 Running"
        );
        assert!(esc_authorized(state), "认出面板,Esc 自愈才有入口");
        assert!(!gate_accepts(InjectGate::IdleOnly, state));
        assert!(!gate_accepts(InjectGate::PromptQueue, state));
        assert_eq!(
            pending_input(pane),
            None,
            "面板屏没有输入行 —— 陈旧回显不算,算了会让提交确认往面板上补 Enter"
        );
    }

    /// 修法 2(随修法 1 免费):面板上方的陈旧 spinner 不得把面板屏判成 `Running` ——
    /// 判成 Running,`PromptQueue` 会放行,把整条 prompt 注进面板,每个按键都被它吃掉。
    /// 面板屏的唯一身份是 `Modal(Panel)`;授权的 Esc 只关面板,不打断横杠上方可能还在跑的轮。
    #[test]
    fn a_spinner_above_the_panel_never_makes_the_screen_running() {
        assert!(
            !crate::scrubber::is_ready(PANEL_OVER_TRANSCRIPT),
            "样本的转录区里确有运行中形态的 spinner,否则测的不是这条"
        );
        assert_eq!(
            classify_pane(PANEL_OVER_TRANSCRIPT),
            PaneState::Modal(ModalKind::Panel)
        );
        // 最小合成形态同判:运行中形态 spinner + 真机 /status 面板。
        let synthetic = format!("✽ Composing… (3s)\n{PANEL}");
        let state = classify_pane(&synthetic);
        assert_eq!(state, PaneState::Modal(ModalKind::Panel));
        assert!(
            !gate_accepts(InjectGate::PromptQueue, state),
            "判成 Running 就等于放行注入进面板"
        );
    }

    /// 抓不到东西(死会话的 capture 是 `Ok("")`)只能是 Unknown,不许当就绪。
    /// 提交确认用的原语:输入行拿着什么。空输入框是 `Some("")`,没有输入行才是 `None`。
    #[test]
    fn pending_input_reads_the_prompt_line_verbatim() {
        assert_eq!(pending_input(IDLE).as_deref(), Some(""));
        assert_eq!(pending_input(DRAFT).as_deref(), Some("重启一下 starlight"));
        assert_eq!(pending_input(PANEL), None, "面板没有输入行");
        let typed = "──── summon:soul-x ──\n❯ /status\n────\n";
        assert_eq!(pending_input(typed).as_deref(), Some("/status"));
    }

    #[test]
    fn blank_capture_is_unknown_not_ready() {
        assert_eq!(classify_pane(""), PaneState::Unknown);
        assert_eq!(classify_pane("   \n\t\n"), PaneState::Unknown);
    }

    /// Esc 授权边界:只对已注册的全屏面板;菜单(可能是工具授权提示)、草稿、运行中、未知全不授权。
    #[test]
    fn esc_is_authorized_only_for_registered_panels() {
        assert!(esc_authorized(PaneState::Modal(ModalKind::Panel)));
        for st in [
            PaneState::Modal(ModalKind::Menu),
            PaneState::Draft,
            PaneState::Running,
            PaneState::Unknown,
            PaneState::InputReady,
        ] {
            assert!(!esc_authorized(st), "不该授权 Esc: {st:?}");
        }
    }

    #[test]
    fn gate_policies_differ_only_on_running() {
        for gate in [InjectGate::PromptQueue, InjectGate::IdleOnly] {
            assert!(gate_accepts(gate, PaneState::InputReady));
            for st in [
                PaneState::Draft,
                PaneState::Modal(ModalKind::Panel),
                PaneState::Modal(ModalKind::Menu),
                PaneState::Unknown,
            ] {
                assert!(!gate_accepts(gate, st), "{gate:?} 不该放行 {st:?}");
            }
        }
        assert!(gate_accepts(InjectGate::PromptQueue, PaneState::Running));
        assert!(!gate_accepts(InjectGate::IdleOnly, PaneState::Running));
        // 冷启期显式豁免,名字自带说明,不做隐式判定。
        assert!(gate_accepts(
            InjectGate::StartupUnproven,
            PaneState::Unknown
        ));
    }

    /// 回执必须外显「没执行」+ 下一步;含糊或只说「失败」都不合格(零静默丢失、零谎报成功)。
    #[test]
    fn refusal_notice_states_that_nothing_ran() {
        for st in [
            PaneState::Modal(ModalKind::Panel),
            PaneState::Modal(ModalKind::Menu),
            PaneState::Draft,
            PaneState::Running,
            PaneState::Unknown,
        ] {
            let n = refusal_notice(st);
            assert!(n.contains("本条没有执行"), "{st:?}: {n}");
            assert!(n.contains("重发"), "{st:?}: {n}");
            assert!(n.contains("不会自动重试"), "{st:?}: {n}");
        }
        assert!(
            refusal_notice_after_esc(PaneState::Modal(ModalKind::Panel)).contains("不再试第二次")
        );
        assert_eq!(
            InjectionRefused::new(PaneState::Draft).to_string(),
            refusal_notice(PaneState::Draft),
            "错误的 Display 就是用户回执,别在中间再包一层措辞"
        );
    }

    /// 菜单拒因要把菜单本身带出去:出站侧只有拿到它才发得出可点的中介卡。
    /// 其余拒因不带 —— 面板/草稿没有可镜像的问句,凭空发一张卡等于给用户一个点不动的东西。
    #[test]
    fn only_a_menu_refusal_carries_the_menu_snapshot() {
        let menu = "Change effort level?\n❯ 1. Yes, switch to xhigh\n  2. No";
        let state = classify_pane(menu);
        assert_eq!(state, PaneState::Modal(ModalKind::Menu));
        let snapshot = InjectionRefused::on_pane(state, menu)
            .menu
            .expect("菜单拒因必须带菜单快照");
        assert_eq!(snapshot.options.len(), 2);
        assert_eq!(snapshot.options[0].label, "Yes, switch to xhigh");

        for pane in [PANEL, DRAFT, IDLE] {
            assert!(
                InjectionRefused::on_pane(classify_pane(pane), pane)
                    .menu
                    .is_none(),
                "非菜单拒因不该带快照: {pane:?}"
            );
        }
        // 输入行是菜单光标、但认不出选项块(只有一项)→ 无卡可发,老实留空。
        let unparsable = "确认?\n❯ 1. 好的";
        assert_eq!(classify_pane(unparsable), PaneState::Modal(ModalKind::Menu));
        assert!(
            InjectionRefused::on_pane(classify_pane(unparsable), unparsable)
                .menu
                .is_none()
        );
    }

    /// AskUserQuestion 的快照必须剥掉尾部两个 TUI 动作项:这条路没有转录侧的选项计数可用,
    /// 而镜像卡上只有选项按钮 —— 留着它们就是留两个点下去会把会话推进自由文本态的陷阱。
    #[test]
    fn an_ask_user_question_snapshot_drops_the_tui_action_rows() {
        let snapshot =
            InjectionRefused::on_pane(classify_pane(ASK_USER_QUESTION), ASK_USER_QUESTION)
                .menu
                .expect("AskUserQuestion 菜单必须带快照");
        let labels: Vec<&str> = snapshot.options.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Staging（推荐）", "Production"]);

        // 剥完不足 2 项(开放式问题只给一个真选项)→ 不是可镜像的菜单,只发文字回执。
        let open_ended = "只有一个真选项?\n❯ 1. 就它\n  2. Type something.\n  3. Chat about this\n";
        assert_eq!(classify_pane(open_ended), PaneState::Modal(ModalKind::Menu));
        assert!(
            InjectionRefused::on_pane(classify_pane(open_ended), open_ended)
                .menu
                .is_none()
        );
    }

    /// 压缩中判 `Running`、压缩完判 `InputReady`。变异自验:把 `is_working_line` 里的
    /// `|| is_progress_bar_line(t)` 去掉,第一条必红(那正是 owner 2026-09-09 报的
    /// 「compact 秒返回」)。
    #[test]
    fn compact_in_progress_is_running_and_finished_is_input_ready() {
        assert_eq!(classify_pane(COMPACTING), PaneState::Running);
        assert!(!is_input_ready(COMPACTING));
        assert_eq!(classify_pane(COMPACTED), PaneState::InputReady);
        assert!(is_input_ready(COMPACTED));
        // 压缩中不放行 slash 注入:注进去只会变成压缩完之后的一段聊天文本。
        assert!(!gate_accepts(
            InjectGate::IdleOnly,
            classify_pane(COMPACTING)
        ));
    }

    /// 真机 2026-09-09(`ghost-probe`)逐字节抓的输入行(`capture-pane -pe`)。同一条会话、
    /// 前后两拍:claude 画的建议整段被 SGR 2 包着,用户草稿没有。纯文本抓屏里两者同形。
    const GHOST_STYLED: &str = "\u{1b}[39m✻ Churned for 2m 35s\u{1b}[0m\n\
────────────────────────────────────────────────────\n\
\u{1b}[39m❯\u{a0}\u{1b}[2m再列一下 crates/summon-core/src/session_view.rs 是干什么的\u{1b}[0m\n\
────────────────────────────────────────────────────\n";

    const DRAFT_STYLED: &str = "\u{1b}[39m✻ Churned for 2m 35s\u{1b}[0m\n\
────────────────────────────────────────────────────\n\
\u{1b}[39m❯\u{a0}这是一段真草稿\n\
────────────────────────────────────────────────────\n";

    /// claude 画在输入框里的字不是草稿。**纯文本判据分不出**,所以先钉住「不带样式时两者同形」
    /// 这个前提 —— 它一旦不成立(哪天 claude 改成插一个不可见标记),这条修复就该整个重想。
    #[test]
    fn claude_drawn_prompt_text_is_told_apart_from_a_real_draft_by_style() {
        assert_eq!(classify_pane(GHOST_STYLED), PaneState::Draft);
        assert_eq!(classify_pane(DRAFT_STYLED), PaneState::Draft);

        assert!(prompt_text_is_dim(GHOST_STYLED));
        assert!(!prompt_text_is_dim(DRAFT_STYLED));

        assert_eq!(classify_pane_styled(GHOST_STYLED), PaneState::InputReady);
        assert_eq!(classify_pane_styled(DRAFT_STYLED), PaneState::Draft);
        // 样式只用来证否草稿:其余状态一律沿用纯文本判据。
        assert_eq!(classify_pane_styled(COMPACTING), PaneState::Running);
        assert_eq!(classify_pane_styled(IDLE), PaneState::InputReady);
    }

    /// 只有 dim 才算 claude 画的。别的 SGR(历史回显的亮白 `38;5;231`)不能被误判成 ghost ——
    /// 那会把用户真草稿当空输入框,把消息注进去和草稿拼在一起。
    #[test]
    fn only_dim_counts_as_claude_drawn_text() {
        let bright = "❯\u{a0}\u{1b}[38;5;231m用户自己粘进来的一段\u{1b}[39m";
        assert!(!prompt_text_is_dim(bright));
        // 参数表里含 2 才算,含 "12"/"20" 这类不算。
        assert!(!prompt_text_is_dim("❯\u{a0}\u{1b}[22m半亮\u{1b}[0m"));
        assert!(prompt_text_is_dim("❯\u{a0}\u{1b}[0;2m暗\u{1b}[0m"));
        // 没有输入行 / 提示符后什么都没有:都不算。
        assert!(!prompt_text_is_dim("没有输入行"));
        assert!(!prompt_text_is_dim("❯\u{a0}"));
    }

    /// 真机形态(2026-10-27,claude 2.1.311,会话刚起来的第一条注入):多行正文被折叠成粘贴
    /// 占位,紧跟的回车全被吞,正文停在输入框里,屏上一片空闲。
    const PASTE_STUCK: &str = "─────────── summon:soul-flare-5-g00000000a11ce04d ──\n\
❯\u{a0}[Pasted text #1 +11 lines]\n\n\n\n\
────────────────────────────────────────────────────\n\
  ⏵⏵ bypass permissions on (shift+tab to cycle)\n";

    /// 没折叠的正文:`❯` 那行是正文第一行,太长时折到下一行显示。
    const PROMPT_STUCK_WRAPPED: &str = "─────────── summon:soul-flare-5-g00000000a11ce04d ──\n\
❯\u{a0}帮我看一下今天的部署日志,把失败的那几条和对应的报错\n\
  原因都贴上来\n\n\
<!-- summon-transport-turn:probe -->\n\
────────────────────────────────────────────────────\n";

    /// 没编号的选择菜单,光标停在一项上。
    const UNNUMBERED_MENU: &str = "────────────────────────────────────────────────────\n\
 Allow this action?\n\n\
❯ Yes\n\
  No\n\
────────────────────────────────────────────────────\n";

    #[test]
    fn only_the_injected_prompt_sitting_unsent_counts_for_a_resubmit() {
        const FIRST: &str = "帮我看一下今天的部署日志,把失败的那几条和对应的报错原因都贴上来";
        let styled = |pane: &'static str| move || Some(pane.to_string());
        let input = |text: &str| format!("──── summon:soul-x ──\n❯\u{a0}{text}\n────────────\n");
        let holds = |pane: &str, first: &str| {
            let owned = pane.to_string();
            holds_unsent_prompt(pane, first, move || Some(owned))
        };
        assert!(holds_unsent_prompt(PASTE_STUCK, FIRST, styled(PASTE_STUCK)));
        assert!(holds_unsent_prompt(
            PROMPT_STUCK_WRAPPED,
            FIRST,
            styled(PROMPT_STUCK_WRAPPED)
        ));
        // 第一行不足门槛时整行相等才算。
        assert!(holds_unsent_prompt(
            DRAFT_STYLED,
            "这是一段真草稿",
            styled(DRAFT_STYLED)
        ));
        assert!(!holds_unsent_prompt(
            DRAFT_STYLED,
            "这是一段真草稿,后面还有",
            styled(DRAFT_STYLED)
        ));
        // 别的草稿不是这条正文;恰好是正文开头的半句草稿也不是。
        assert!(!holds_unsent_prompt(
            DRAFT_STYLED,
            FIRST,
            styled(DRAFT_STYLED)
        ));
        assert!(!holds(&input("帮我"), FIRST));
        // 显示出的开头不够门槛:折行的第一行太短时证不了。
        assert!(!holds(&input("帮我看一下今天的部署日志,把失败的"), FIRST));
        // 没编号的菜单同样判成草稿,只有「显示的是这条正文」挡得住:回车会替人选掉默认项。
        for menu in [TRUST_DIALOG_2_1_263, UNNUMBERED_MENU] {
            assert_eq!(classify_pane(menu), PaneState::Draft, "{menu:?}");
            assert!(!holds_unsent_prompt(menu, FIRST, styled(menu)), "{menu:?}");
        }
        // 光标项恰好是正文开头。
        assert!(!holds_unsent_prompt(
            UNNUMBERED_MENU,
            "Yes, go ahead",
            styled(UNNUMBERED_MENU)
        ));
        // claude 画的建议句恰好与正文同头:补按回车会把它当用户输入发出去。
        let ghost = "再列一下 crates/summon-core/src/session_view.rs 是干什么的";
        assert!(!holds_unsent_prompt(
            GHOST_STYLED,
            ghost,
            styled(GHOST_STYLED)
        ));
        // 取不到样式就证不了那段字不是 claude 画的。
        assert!(!holds_unsent_prompt(PASTE_STUCK, FIRST, || None));
        // 空输入框、运行中、编号菜单、全屏面板:都不是停着的正文。
        for pane in [IDLE, COMPACTING, ASK_USER_QUESTION, PANEL] {
            assert!(!holds_unsent_prompt(pane, FIRST, styled(pane)), "{pane:?}");
        }
    }
}
