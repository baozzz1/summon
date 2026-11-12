//! 解析 claude(及类似 CLI)在 tmux pane 上弹出的**交互式选择菜单**——best-effort、无状态。
//!
//! 设计前提(owner 决策):被监察的 TUI 不稳定、会随 CLI 迭代变。故这里**只做尽力识别**,识别不出
//! 就返回 `None`,由调用方降级成「贴原始屏幕 + 自由文本输入」卡片,**绝不依赖对 TUI 内部态的精确
//! 建模**。识别要求严格(必须看到「光标停在某个编号选项上」+ ≥2 个编号选项),以几乎杜绝把正常
//! 输出里的编号列表误判成菜单(正常 prose 没有 `❯` 光标行)。

/// 一个可选项(从菜单行 `N. label` 抽出)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// 菜单里的编号(原样,通常 1-based)。注入时按它定位/按键。
    pub index: usize,
    /// 选项文字(已剥光标与编号前缀)。
    pub label: String,
}

/// 识别出的交互态。`raw` 留给降级/展示卡片贴原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveState {
    /// 菜单问句/说明:对话框顶线到选项之间的原文(整理过空行与公共缩进)。
    pub title: String,
    /// 选项(≥2 才算菜单)。
    pub options: Vec<Choice>,
    /// 参与识别的 pane 尾部原文(降级卡片「贴原始屏幕」用)。
    pub raw: String,
}

impl InteractiveState {
    /// 菜单签名,判「是不是同一张菜单」用(见 [`menu_signature`])。
    pub fn signature(&self) -> u64 {
        menu_signature(
            &self.title,
            self.options.iter().map(|c| (c.index, c.label.as_str())),
        )
    }
}

/// 菜单签名:问句 + 真实选项。
///
/// 问句的比较形态保留词边界(连续空白压成一个空格),换行当作一个空格:对话框随终端宽度重排、按词
/// 折行时,换行位置和每行的 `│ ` 前缀会变,签名不变。长词被硬折断时,断处多出一个空格:签名跟着变
/// (点击侧认不出、不按键),也可能恰好与断处本来就有空格的另一条命令同签名。尾部 TUI 动作项不参与:
/// 轮内探测按转录里的选项数截、注入被拒按字面量剥,口径不同,同一张菜单不能因此算成两张。
///
/// 签名写进飞书按钮 value、跨进程跨版本拿回来比对,所以算法写死成 FNV-1a。
pub fn menu_signature<'a>(title: &str, options: impl IntoIterator<Item = (usize, &'a str)>) -> u64 {
    let mut h = Fnv1a::default();
    h.write(comparable_text(title).as_bytes());
    let options: Vec<(usize, &str)> = options.into_iter().collect();
    let real = real_option_count(options.iter().map(|(_, label)| *label));
    // 0xff / 0xfe 不会出现在 UTF-8 文本里,拿来分隔各段,段与段之间换一种切分撞不出同一串。
    for (index, label) in &options[..real] {
        h.write(&[0xff]);
        h.write(index.to_string().as_bytes());
        h.write(&[0xfe]);
        h.write(label.as_bytes());
    }
    h.0
}

/// FNV-1a 64 位。
struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv1a {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// 问句的比较形态:每行去掉行首空白与框线前缀(TUI 折行的 `│ `),全部空白连续段压成一个空格,
/// 再抹掉 claude 自动拒绝倒计时的读数(见 [`mask_auto_decision_countdown`])。
fn comparable_text(text: &str) -> String {
    let words: Vec<&str> = text
        .lines()
        .map(|line| line.trim_start_matches(|c: char| c.is_whitespace() || is_box_drawing(c)))
        .flat_map(str::split_whitespace)
        .collect();
    mask_auto_decision_countdown(&words).join(" ")
}

/// 倒计时在签名里的替身。
const COUNTDOWN_PLACEHOLDER: &str = "<countdown>";
/// 倒计时句的固定前缀与后缀(claude 2.1.284 的原文,逐词、不分大小写比较)。
const COUNTDOWN_LEAD: [&str; 8] = [
    "claude",
    "code",
    "will",
    "automatically",
    "deny",
    "this",
    "request",
    "in",
];
const COUNTDOWN_TAIL: [&str; 8] = [
    "to",
    "avoid",
    "blocking",
    "progress",
    "on",
    "an",
    "unattended",
    "session",
];

/// claude 对无人值守会话的授权框带一行每秒刷新的倒计时:`Claude Code will automatically deny this
/// request in 1:55, to avoid blocking progress on an unattended session`。它是同一张菜单的钟面,
/// 不是内容 —— 留在签名里,每一拍抓屏都算新菜单,每两秒就往群里补发一张卡。
///
/// 只认前缀、读数、后缀三段齐全的整句,只替换读数。问句里别处的数字(命令里的端口、时刻,乃至
/// 命令里抄了半句这段话)都参与签名,点卡片批准的只能是屏上那一条。唯一的例外是命令逐字抄下了
/// 这整句、且两条命令只差其中的读数:它们算同一张菜单。
fn mask_auto_decision_countdown<'a>(words: &[&'a str]) -> Vec<&'a str> {
    let mut out = words.to_vec();
    let mut i = 0;
    while i + COUNTDOWN_LEAD.len() <= words.len() {
        let reading_at = i + COUNTDOWN_LEAD.len();
        if words_match(&words[i..reading_at], &COUNTDOWN_LEAD) {
            if let Some(len) = countdown_reading_len(&words[reading_at..]) {
                let tail_at = reading_at + len;
                let tail_end = tail_at + COUNTDOWN_TAIL.len();
                if tail_end <= words.len()
                    && words_match(&words[tail_at..tail_end], &COUNTDOWN_TAIL)
                {
                    out.splice(
                        reading_at..tail_at,
                        std::iter::repeat_n(COUNTDOWN_PLACEHOLDER, len),
                    );
                    i = tail_end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn words_match(words: &[&str], want: &[&str]) -> bool {
    words.len() == want.len()
        && words
            .iter()
            .zip(want)
            .all(|(word, want)| word.eq_ignore_ascii_case(want))
}

/// 读数占几个词:`1:55,`(分:秒)是 1 个;读屏模式的 `about 45 seconds,` / `about 2 minutes,` 是 3 个。
/// 读数后的逗号是句子的一部分,必须有。
fn countdown_reading_len(words: &[&str]) -> Option<usize> {
    let clock = |word: &str| {
        word.strip_suffix(',').is_some_and(|reading| {
            reading.split_once(':').is_some_and(|(minutes, seconds)| {
                !minutes.is_empty()
                    && minutes.bytes().all(|b| b.is_ascii_digit())
                    && seconds.len() == 2
                    && seconds.bytes().all(|b| b.is_ascii_digit())
            })
        })
    };
    let spoken = |words: &[&str]| {
        words.len() >= 3
            && words[0].eq_ignore_ascii_case("about")
            && !words[1].is_empty()
            && words[1].bytes().all(|b| b.is_ascii_digit())
            && words[2].strip_suffix(',').is_some_and(|unit| {
                ["second", "seconds", "minute", "minutes"]
                    .iter()
                    .any(|want| unit.eq_ignore_ascii_case(want))
            })
    };
    if words.first().is_some_and(|word| clock(word)) {
        Some(1)
    } else if spoken(words) {
        Some(3)
    } else {
        None
    }
}

/// 签名进按钮 value 时的文本形态。飞书回调走 JSON,u64 按双精度解析会丢位,所以存定长十六进制串。
pub fn signature_token(signature: u64) -> String {
    format!("{signature:016x}")
}

/// [`signature_token`] 的反向。
pub fn parse_signature_token(token: &str) -> Option<u64> {
    (token.len() == 16)
        .then(|| u64::from_str_radix(token, 16).ok())
        .flatten()
}

/// 标题是不是缺了开头的对话框(见 `menu_title`)。只看得到结尾,证明不了点下去批准的是哪一条,
/// 这种卡不带签名。
pub fn title_is_cut(title: &str) -> bool {
    title.starts_with(TITLE_CUT_MARKER)
}

pub fn localized_title(title: &str) -> std::borrow::Cow<'_, str> {
    match title.strip_prefix(TITLE_CUT_MARKER) {
        Some(rest) => format!("{}{rest}", crate::i18n::text("r67d6e43803")).into(),
        None => title.into(),
    }
}

/// 光标标记:只认 claude TUI 真正用的 `❯`(U+276F)。**不收裸 `>`**——否则 markdown 引用块
/// (`> 1. …` / `> 2. …`)、diff、邮件式引用里的编号行会被误判成菜单(对抗 review 确认的假阳)。
const CURSOR_MARKERS: [char; 1] = ['❯'];
/// 屏上找不到对话框顶线时,标题最多往选项上方回溯几行(避免把无关 scrollback 拖进标题)。
const TITLE_LOOKBACK: usize = 10;
/// 对话框顶线最短多长。顶线是从第 0 列画起的满宽 `─`;正文里偶然出现的短横线、命令块里带
/// 缩进或 `│ ` 前缀的框线行都过不了这道。
const DIALOG_RULE_MIN: usize = 20;
/// 降级卡片 `raw` 最多保留 pane 末尾几行。
const RAW_TAIL_LINES: usize = 20;
/// 两个编号选项之间最多隔几行非选项行(选项描述/分隔线)。AskUserQuestion 的 TUI 每个选项
/// 下面跟 1~n 行缩进描述,描述行不是编号选项,块扩展须跨过它们去找下一个选项,否则只收得到
/// 锚点那一个选项、凑不够 ≥2 而漏检。
const MAX_INTER_OPTION_GAP: usize = 6;
/// 对话框开头不在屏上时,标题第一行的说明。
const TITLE_CUT_MARKER: &str = summon_i18n::source!("r67d6e43803");
/// 选项块之下最多看几个非空行找对话框页脚。最后一个选项可以带几行描述(AskUserQuestion),页脚
/// 在描述之后。
const DIALOG_FOOTER_WINDOW: usize = 8;
/// 对话框页脚字样(按小写比较)。工具授权框是 `Esc to cancel · Tab to amend`,AskUserQuestion 是
/// `Enter to select · ↑/↓ to navigate · Esc to cancel`。
const DIALOG_FOOTERS: [&str; 2] = ["esc to cancel", "enter to select"];
/// 轮外守望连续几拍认不出屏上是什么,才当菜单已经没了。一拍认不出可能只是抓屏落在重绘中途。
const UNKNOWN_PROBES_TO_FORGET: u32 = 2;
/// 同一张菜单的轮外卡最多发几次(只有发送失败才会再发)。
const ANNOUNCE_TRIES: u32 = 3;

/// 尽力把 pane 解析成一个选择菜单。判据(两条都满足才算):
/// ① 出现「光标停在某个编号选项上」(`❯ 1.`);② 至少 2 个编号选项。
/// 这两条几乎不会被正常输出里的编号列表误命中(正常 prose 无 `❯` 光标行)。识别不出 → `None`。
pub fn parse_interactive(pane: &str) -> Option<InteractiveState> {
    let lines: Vec<&str> = pane.lines().collect();
    parse_lines(&lines).map(|parsed| parsed.state)
}

/// 屏上**此刻正等人选**的对话框菜单。轮外守望没有「这一轮正在跑」这层前提,所以在
/// [`parse_interactive`] 之上再要两条正向证据;尾部 TUI 动作项一并剥掉,剥完不足 2 项不算。
/// - 菜单光标行就是整屏最后一个 `❯` 行。用户发过的编号列表回显成 `❯ 1. …` / `  2. …`,与菜单逐字
///   同形;对话框弹出时 claude 收起了输入框,回显的下面却永远还压着一个输入框。
/// - 选项块下面有对话框页脚(`Esc to cancel` / `Enter to select`)。输入框里打着编号列表的草稿
///   同样是最后一个 `❯`,它下面隔着输入框的下框线。
pub fn awaiting_choice(pane: &str) -> Option<InteractiveState> {
    let lines: Vec<&str> = pane.lines().collect();
    let parsed = parse_lines(&lines)?;
    let last_prompt = lines.iter().rposition(|l| prompt_rest(l).is_some())?;
    (last_prompt == parsed.anchor && parsed.footer)
        .then_some(parsed.state)
        .map(strip_trailing_ui_actions)
        .filter(|state| state.options.len() >= 2)
}

/// 解析结果:菜单本身、锚点行号(菜单光标所在行)、选项块下面有没有对话框页脚。
struct ParsedMenu {
    state: InteractiveState,
    anchor: usize,
    footer: bool,
}

fn parse_lines(lines: &[&str]) -> Option<ParsedMenu> {
    let is_opt = |l: &str| parse_numbered(strip_cursor(l).1).is_some();

    // 锚点:既带光标 ❯ 又是编号选项的行里**最靠下**的一行。TUI 的活光标恒在屏幕底部,它上方
    // 同形的行只能是历史(编号 prose、回显);以锚点为中心扩展菜单块,块外的编号行一律不收。
    let anchor = lines.iter().rposition(|l| {
        let (cur, rest) = strip_cursor(l);
        cur && parse_numbered(rest).is_some()
    })?;

    // 从锚点向上/下扩展「编号选项块」:容忍块内**单个**空行(选项间空行),遇非空非选项行即停。
    let mut start = anchor;
    while start > 0 {
        let prev = lines[start - 1];
        if is_opt(prev) {
            start -= 1;
        } else if prev.trim().is_empty() && start >= 2 && is_opt(lines[start - 2]) {
            start -= 2;
        } else {
            break;
        }
    }
    let mut end = anchor; // inclusive
    while end + 1 < lines.len() {
        let next = lines[end + 1];
        if is_opt(next) {
            end += 1;
        } else if next.trim().is_empty() && end + 2 < lines.len() && is_opt(lines[end + 2]) {
            end += 2;
        } else if !next.trim().is_empty() {
            // 非空非选项行(选项描述/分隔线):在有限行数内向前找下一个编号选项,找到就跨过
            // 这些行继续扩块。AskUserQuestion 的 TUI 每个选项下跟缩进描述行,旧逻辑在这里
            // 直接 break,导致只收得到锚点一个选项、凑不够 ≥2 而整张菜单漏检。
            let mut jumped = false;
            for skip in 1..=MAX_INTER_OPTION_GAP {
                let idx = end + 1 + skip;
                if idx >= lines.len() || lines[idx].trim().is_empty() {
                    break; // 空行是块边界,不跨
                }
                if is_opt(lines[idx]) {
                    end = idx;
                    jumped = true;
                    break;
                }
            }
            if !jumped {
                break;
            }
        } else {
            break;
        }
    }

    let options: Vec<Choice> = lines[start..=end]
        .iter()
        .filter_map(|l| {
            let (_c, rest) = strip_cursor(l);
            parse_numbered(rest).map(|(n, lbl)| Choice {
                index: n,
                label: lbl.to_string(),
            })
        })
        .collect();
    if options.len() < 2 {
        return None;
    }
    let footer = has_dialog_footer(&lines[end + 1..]);
    let state = InteractiveState {
        title: menu_title(lines, start, footer),
        options,
        raw: tail_lines(lines, RAW_TAIL_LINES),
    };
    Some(ParsedMenu {
        state,
        anchor,
        footer,
    })
}

/// 菜单问句/说明。
///
/// 对话框(带页脚)确认要看的是整段正文(命令、描述、警告),签名也要覆盖整段,所以取它的顶线
/// (见 `dialog_top`)到选项之间的全部原文,顶线再远也一样。找不到顶线就是开头不在屏上:取选项上方
/// `TITLE_LOOKBACK` 行,第一行写明没显示全(见 [`title_is_cut`])。不是对话框的菜单取选项上方
/// `TITLE_LOOKBACK` 行,其中有横线就从横线下面取起 —— 横线上方是状态栏与转录,不是问句。
///
/// claude 的对话框(工具授权、AskUserQuestion)顶上画一条从第 0 列起的满宽 `─`,往下依次是标题、
/// 命令或问题正文、描述、警告与问句。
fn menu_title(lines: &[&str], start: usize, footer: bool) -> String {
    let window = start.saturating_sub(TITLE_LOOKBACK);
    if !footer {
        let from = lines[window..start]
            .iter()
            .rposition(|l| is_dialog_rule(l))
            .map_or(window, |rule| window + rule + 1);
        return tidy_block(&lines[from..start]);
    }
    match dialog_top(lines, start) {
        Some(rule) => tidy_block(&lines[rule + 1..start]),
        None => format!("{TITLE_CUT_MARKER}\n{}", tidy_block(&lines[window..start])),
    }
}

/// 对话框顶线:选项上方最近的一条满宽 `─`,且确实属于这个对话框。输入框的上下边框也是这种横线,
/// 靠它框住的输入行认出来 —— 上边框下一行就是输入行,下边框与上边框之间夹着输入行;顶线到选项之间
/// 只有对话框自己的标题与正文,再出现输入行就说明这条横线属于更早的转录。
///
/// 顶线之上是什么不参与判断:用户发一条消息、claude 直接弹框时,紧贴顶线的就是那条 prompt 回显。
fn dialog_top(lines: &[&str], start: usize) -> Option<usize> {
    let rule = lines[..start].iter().rposition(|l| is_dialog_rule(l))?;
    let has_prompt = |block: &[&str]| block.iter().any(|l| prompt_rest(l).is_some());
    let input_top = rule + 1 < start && prompt_rest(lines[rule + 1]).is_some();
    let input_bottom = lines[..rule]
        .iter()
        .rposition(|l| is_dialog_rule(l))
        .is_some_and(|above| has_prompt(&lines[above + 1..rule]));
    (!input_top && !input_bottom && !has_prompt(&lines[rule + 1..start])).then_some(rule)
}

/// 选项块下面有没有对话框页脚:前 `DIALOG_FOOTER_WINDOW` 个非空行里出现页脚字样,且中间没隔着
/// 满宽横线(输入框的边框)。
fn has_dialog_footer(below: &[&str]) -> bool {
    below
        .iter()
        .take_while(|l| !is_dialog_rule(l))
        .filter(|l| !l.trim().is_empty())
        .take(DIALOG_FOOTER_WINDOW)
        .any(|l| {
            let l = l.to_lowercase();
            DIALOG_FOOTERS.iter().any(|footer| l.contains(footer))
        })
}

fn is_dialog_rule(line: &str) -> bool {
    let line = line.trim_end();
    line.chars().count() >= DIALOG_RULE_MIN && line.chars().all(|c| c == '─')
}

/// 屏幕行整理成一段可读的块:去行尾空白、去首尾空行、连续空行并成一行、整体去掉公共的前导空格。
/// 行内容与行间的相对缩进原样保留 —— TUI 折行后 `│ ` 前缀的对齐靠的就是它。
fn tidy_block(lines: &[&str]) -> String {
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    let Some(first) = lines.iter().position(|l| !l.is_empty()) else {
        return String::new();
    };
    let last = lines.iter().rposition(|l| !l.is_empty()).unwrap_or(first);
    let body = &lines[first..=last];
    // 只数 ASCII 空格:按它切片永远落在字符边界上,全角空格之类的多字节缩进留在行内。
    let indent = body
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| l.bytes().take_while(|b| *b == b' ').count())
        .min()
        .unwrap_or(0);
    let mut out: Vec<&str> = Vec::with_capacity(body.len());
    for line in body {
        if line.is_empty() {
            if out.last() != Some(&"") {
                out.push("");
            }
        } else {
            out.push(&line[indent..]);
        }
    }
    out.join("\n")
}

/// 一行若是 `❯` 提示行(菜单光标或输入框),返回 `❯` 之后的内容。行首的空白与框线字符不算:
/// 输入框在某些版本里带左框线,判据不该被一根竖线推翻。
fn prompt_rest(line: &str) -> Option<&str> {
    line.trim_start_matches(|c: char| c.is_whitespace() || is_box_drawing(c))
        .strip_prefix(|c: char| CURSOR_MARKERS.contains(&c))
        .map(str::trim)
}

fn is_box_drawing(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// AskUserQuestion 菜单尾部固定跟着的两个 TUI 动作项(claude 自己的 UI 元素,不是模型给的选项)。
/// 它们在飞书单选卡上是陷阱按钮:卡上只有选项按钮,点「Type something.」会让 TUI 进自由文本态,
/// 而飞书侧既打不了字也退不出来。
const TRAILING_UI_ACTIONS: [&str; 2] = ["Type something.", "Chat about this"];

/// 截掉菜单尾部的 TUI 动作项。**只从尾部按序剥、只认字面量相等**:真实选项撞名的概率可忽略,
/// 而中间位置的同名项一定是模型给的真选项,不碰。
///
/// 这是给**拿不到转录侧选项计数**的调用方用的(如注入闸拒绝时的菜单镜像):轮内路径有 tail 记的
/// 真实选项数,按计数截更准,不走这条。
pub fn strip_trailing_ui_actions(st: InteractiveState) -> InteractiveState {
    let mut options = st.options;
    let real = real_option_count(options.iter().map(|c| c.label.as_str()));
    options.truncate(real);
    InteractiveState { options, ..st }
}

/// 剥掉尾部 TUI 动作项之后还剩几个选项。
fn real_option_count<'a>(
    mut labels: impl DoubleEndedIterator<Item = &'a str> + ExactSizeIterator,
) -> usize {
    labels
        .rposition(|label| !TRAILING_UI_ACTIONS.contains(&label))
        .map_or(0, |i| i + 1)
}

/// 这条会话上**已经推到飞书**的菜单。发菜单卡的路径不止一条(轮内探测、注入被拒的回执、切 effort
/// 的确认卡、轮外守望),谁发出去谁记一笔,轮外守望据此不把别人发过的菜单再发一遍。
///
/// 只记最近一张:屏上同一时刻只有一个活菜单,换了菜单,旧记录就没有意义了。卡片上的选择注入会话后
/// 记一次「答过」([`Self::note_answered`]),同一个问题紧接着再弹出,守望把它当成新问题。
#[derive(Debug, Default)]
pub struct MenuLedger(std::sync::Mutex<LedgerState>);

#[derive(Debug, Default)]
struct LedgerState {
    recorded: Option<u64>,
    /// 卡片上答过几次、最近答掉的是哪一张。
    answered: (u64, Option<u64>),
}

impl MenuLedger {
    pub fn record(&self, menu: &InteractiveState) {
        self.state().recorded = Some(menu.signature());
    }

    pub fn recorded(&self) -> Option<u64> {
        self.state().recorded
    }

    /// 卡片上对这张菜单的选择已经注入会话:它答掉了。记着的若正是它就作废,并记一次「答过」。
    pub fn note_answered(&self, signature: u64) {
        let mut state = self.state();
        if state.recorded == Some(signature) {
            state.recorded = None;
        }
        state.answered = (state.answered.0 + 1, Some(signature));
    }

    /// 一次加锁读出账上记着的菜单和最近一次「答过」。
    fn snapshot(&self) -> (Option<u64>, (u64, Option<u64>)) {
        let state = self.state();
        (state.recorded, state.answered)
    }

    fn clear(&self) {
        self.state().recorded = None;
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        // 记录只用来去重:持锁方 panic 留下的值照样可用。
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 一屏抓屏对轮外守望意味着什么(见 [`screen_menu`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenMenu {
    /// 对话框菜单正等人选(见 [`awaiting_choice`])。
    Awaiting(InteractiveState),
    /// 正向证明屏上没有待答的对话框:最后一个 `❯` 行是输入框。对话框弹出时 claude 收起输入框。
    Absent,
    /// 两样都证明不了:没有 `❯` 行,或菜单形态认不全(比如输入框里打着编号列表的草稿)。
    Unknown,
}

/// 把一屏抓屏判成 [`ScreenMenu`]。
pub fn screen_menu(pane: &str) -> ScreenMenu {
    if let Some(menu) = awaiting_choice(pane) {
        return ScreenMenu::Awaiting(menu);
    }
    match pane.lines().rev().find_map(prompt_rest) {
        Some(rest) if parse_numbered(rest).is_none() => ScreenMenu::Absent,
        _ => ScreenMenu::Unknown,
    }
}

/// 这一拍该不该主动发菜单卡。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuWatchVerdict {
    Quiet,
    Announce,
}

/// 轮外菜单守望。会话不在 summon 的轮里时(后台子代理、终端侧自己起的轮),claude 弹出的授权 /
/// 提问对话框没有别的路径看得见,会话就停在那里,等一个不知道它存在的人。守望按拍喂抓屏,
/// 决定哪一张菜单要**主动**发进群里。
///
/// 主动往群里发东西,判据从目的倒推 ——「屏上新弹出了一个等人选的对话框,飞书里还没有它的卡」。
/// 三条缺一不发:
/// - **守望期间弹出**:守望起步那一拍屏上已有的菜单不发。起步发生在 daemon 重启、会话重载之后,
///   那一刻屏上菜单的来历证不了(可能早就发过卡);它留给用户下次说话时注入被拒的回执去带。
/// - **没人发过**:[`MenuLedger`] 里记着同一张就不发。
/// - **连续两拍都在**:菜单是静止画面,真菜单一定能连着看到两次;一拍的抓屏可能落在重绘中途。
///
/// 菜单没了,它的记录(起步记录、候选、失败计数、账本)一起作废,之后再弹出一模一样的菜单是新问题。
/// 「没了」要有证据:看到输入框回来([`ScreenMenu::Absent`])当拍就算;只是认不出
/// ([`ScreenMenu::Unknown`])要连续两拍;卡片上答过([`MenuLedger::note_answered`])也算。
#[derive(Debug, Default)]
pub struct MenuWatch {
    started: bool,
    on_screen_at_start: Option<u64>,
    candidate: Option<u64>,
    unknown: u32,
    failures: Option<(u64, u32)>,
    answers_seen: u64,
}

impl MenuWatch {
    /// 喂一拍观测。抓屏失败、空屏证明不了屏上有什么,不该拿来喂。
    pub fn observe(&mut self, screen: &ScreenMenu, ledger: &MenuLedger) -> MenuWatchVerdict {
        let first = !self.started;
        self.started = true;
        let (recorded, (answers, answered)) = ledger.snapshot();
        if answers != self.answers_seen {
            self.answers_seen = answers;
            // 卡片上答掉的正是守望手里跟着的那张:关于它的状态作废。答掉的是别的菜单就不动。
            if answered.is_some()
                && (answered == self.candidate || answered == self.on_screen_at_start)
            {
                self.on_screen_at_start = None;
                self.candidate = None;
                self.failures = None;
            }
        }
        let menu = match screen {
            ScreenMenu::Awaiting(menu) => menu,
            ScreenMenu::Absent => {
                self.forget(ledger);
                return MenuWatchVerdict::Quiet;
            }
            ScreenMenu::Unknown => {
                self.unknown += 1;
                if self.unknown >= UNKNOWN_PROBES_TO_FORGET {
                    self.forget(ledger);
                }
                return MenuWatchVerdict::Quiet;
            }
        };
        let sig = menu.signature();
        self.unknown = 0;
        if first {
            self.on_screen_at_start = Some(sig);
            return MenuWatchVerdict::Quiet;
        }
        if self.on_screen_at_start == Some(sig) || recorded == Some(sig) || self.gave_up(sig) {
            return MenuWatchVerdict::Quiet;
        }
        if self.candidate == Some(sig) {
            return MenuWatchVerdict::Announce;
        }
        self.candidate = Some(sig);
        MenuWatchVerdict::Quiet
    }

    /// 发卡失败记一次。菜单还在的话下一拍重发,同一张失败到 `ANNOUNCE_TRIES` 次就不再发。
    pub fn note_announce_failed(&mut self, menu: &InteractiveState) {
        let sig = menu.signature();
        self.failures = match self.failures {
            Some((failed, n)) if failed == sig => Some((sig, n + 1)),
            _ => Some((sig, 1)),
        };
    }

    fn forget(&mut self, ledger: &MenuLedger) {
        self.on_screen_at_start = None;
        self.candidate = None;
        self.unknown = 0;
        self.failures = None;
        ledger.clear();
    }

    fn gave_up(&self, sig: u64) -> bool {
        self.failures
            .is_some_and(|(failed, n)| failed == sig && n >= ANNOUNCE_TRIES)
    }
}

/// 剥行首光标标记;返回 (是否有光标, 剥后内容)。先 `trim_start` 再看首字符。
fn strip_cursor(line: &str) -> (bool, &str) {
    let t = line.trim_start();
    for m in CURSOR_MARKERS {
        if let Some(rest) = t.strip_prefix(m) {
            return (true, rest.trim_start());
        }
    }
    (false, t)
}

/// 解析 `N. label`(N 正整数,后跟 `. ` 再跟非空文字)。非此形态返回 `None`。
fn parse_numbered(s: &str) -> Option<(usize, &str)> {
    let s = s.trim_start();
    let dot = s.find(". ")?;
    let num: usize = s[..dot].parse().ok()?;
    let label = s[dot + 2..].trim();
    if label.is_empty() {
        return None;
    }
    Some((num, label))
}

/// pane 末尾 `n` 行原文(降级卡片用),去掉尾部空行。
fn tail_lines(lines: &[&str], n: usize) -> String {
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// owner 截图的 `/effort` 确认菜单:应解析出标题 + 2 个选项,光标在 1。
    #[test]
    fn parses_effort_confirm_menu() {
        let pane = "\
/effort ultracode

Change effort level?
Your next response will be slower and use more tokens

This conversation is cached for the current effort level. Switching to xhigh means the full history gets re-read on your next message.

❯ 1. Yes, switch to xhigh
  2. No, go back";
        let st = parse_interactive(pane).expect("应识别为菜单");
        assert_eq!(st.options.len(), 2);
        assert_eq!(
            st.options[0],
            Choice {
                index: 1,
                label: "Yes, switch to xhigh".into()
            }
        );
        assert_eq!(
            st.options[1],
            Choice {
                index: 2,
                label: "No, go back".into()
            }
        );
        assert!(st.title.contains("Change effort level?"));
        assert!(st.raw.contains("❯ 1. Yes"));
    }

    /// 正常输出里的编号列表(无 `❯` 光标)绝不能误判成菜单。
    #[test]
    fn normal_numbered_prose_is_not_a_menu() {
        let pane = "\
我做了三件事:
1. 改了解析器
2. 加了测试
这条对话里没有交互菜单。";
        assert!(parse_interactive(pane).is_none());
    }

    /// 有光标但只有 1 个选项 → 不算菜单(需 ≥2)。
    #[test]
    fn single_option_with_cursor_is_not_menu() {
        let pane = "确认?\n❯ 1. 好的";
        assert!(parse_interactive(pane).is_none());
    }

    /// 多位编号(光标 ❯ 在中间项)也能解析。
    #[test]
    fn handles_multidigit_options() {
        let pane = "选一个:\n  9. 九\n❯ 10. 十\n  11. 十一";
        let st = parse_interactive(pane).expect("应识别");
        assert_eq!(st.options.len(), 3);
        assert_eq!(
            st.options[1],
            Choice {
                index: 10,
                label: "十".into()
            }
        );
    }

    /// markdown 引用块的编号行(裸 `>` 前缀)**不得**被当菜单(对抗 review 假阳回归)。
    #[test]
    fn markdown_blockquote_numbered_is_not_a_menu() {
        let pane = "这是一段计划引用:\n\n> 1. 改解析器\n> 2. 加测试\n> 3. 发版\n\n以上是计划。";
        assert!(
            parse_interactive(pane).is_none(),
            "引用块编号行不该被当交互菜单"
        );
    }

    /// 选项块之后的无关行不被吞进 options。
    #[test]
    fn stops_collecting_after_menu_block() {
        let pane = "Pick:\n❯ 1. A\n  2. B\n\n(some later unrelated line. with a dot)";
        let st = parse_interactive(pane).expect("应识别");
        assert_eq!(st.options.len(), 2);
    }

    /// 纯文本、无任何编号选项 → None(走降级)。
    #[test]
    fn plain_text_returns_none() {
        assert!(parse_interactive("just a normal assistant reply, nothing to pick").is_none());
    }

    /// review #2 回归:真菜单上方有编号 prose 列表时,以 ❯ 光标为锚点只取真菜单,不被 prose 污染/漏检。
    #[test]
    fn prose_numbered_above_real_menu_does_not_pollute() {
        let pane = "Plan:\n1. do A\n2. do B\nNow choosing:\n❯ 1. Yes\n  2. No";
        let st = parse_interactive(pane).expect("应识别真菜单");
        assert_eq!(st.options.len(), 2, "只取 ❯ 块的两项,不含上方 prose");
        assert_eq!(
            st.options[0],
            Choice {
                index: 1,
                label: "Yes".into()
            }
        );
        assert_eq!(
            st.options[1],
            Choice {
                index: 2,
                label: "No".into()
            }
        );
        assert!(st.title.contains("Now choosing:"));
    }

    /// AskUserQuestion 的 TUI:每个编号选项下跟缩进描述行,选项间还可能夹分隔线。块扩展须跨过
    /// 这些非选项行找到下一个选项,否则只收得到锚点一个、凑不够 ≥2 而整张菜单漏检(实测 bug)。
    #[test]
    fn parses_ask_user_question_menu_with_descriptions() {
        let pane = "\
 ☐ 部署环境

你希望部署到哪个环境？

❯ 1. Staging（推荐）
     预发布环境，与生产配置一致但不面向真实用户。适合先验证部署流程和功能，风险低，出问题可快速回滚。
  2. Production
     生产环境，直接面向真实用户。仅在 staging 验证通过后使用，发布前需确认回滚方案和监控告警就绪。
  3. Type something.
────────────────────────────────────────────────────────────────────────
  4. Chat about this

Enter to select · ↑/↓ to navigate · Esc to cancel";
        let st = parse_interactive(pane).expect("应识别 AskUserQuestion 菜单");
        assert_eq!(st.options.len(), 4, "四个编号项都要收进来");
        assert_eq!(
            st.options[0],
            Choice {
                index: 1,
                label: "Staging（推荐）".into()
            }
        );
        assert_eq!(
            st.options[1],
            Choice {
                index: 2,
                label: "Production".into()
            }
        );
        assert!(st.title.contains("你希望部署到哪个环境？"));
        assert!(st.raw.contains("❯ 1. Staging"));
    }

    /// 尾部 TUI 动作项只按字面量、只从尾部剥:中间位置的同名项是模型给的真选项,不许动。
    #[test]
    fn trailing_ui_actions_are_stripped_only_from_the_tail() {
        let opt = |index: usize, label: &str| Choice {
            index,
            label: label.into(),
        };
        let st = |options: Vec<Choice>| InteractiveState {
            title: "选哪个?".into(),
            options,
            raw: "raw".into(),
        };
        let stripped = strip_trailing_ui_actions(st(vec![
            opt(1, "Staging"),
            opt(2, "Production"),
            opt(3, "Type something."),
            opt(4, "Chat about this"),
        ]));
        assert_eq!(stripped.options.len(), 2);
        assert_eq!(stripped.options[1].label, "Production");
        assert_eq!(stripped.title, "选哪个?", "title/raw 不动");
        assert_eq!(stripped.raw, "raw");

        // 只有其中一个、或换个顺序,同样按尾部剥。
        let one = strip_trailing_ui_actions(st(vec![
            opt(1, "A"),
            opt(2, "B"),
            opt(3, "Chat about this"),
        ]));
        assert_eq!(one.options.len(), 2);

        // 中间撞名的是真选项(模型真的把一项叫这个名字),不碰。
        let middle = strip_trailing_ui_actions(st(vec![
            opt(1, "Type something."),
            opt(2, "B"),
            opt(3, "C"),
        ]));
        assert_eq!(middle.options.len(), 3);

        // `/effort` 这类确认菜单没有 UI 动作项,原样。
        let confirm =
            strip_trailing_ui_actions(st(vec![opt(1, "Yes, switch to xhigh"), opt(2, "No")]));
        assert_eq!(confirm.options.len(), 2);
    }

    /// 选项描述是多行时也要跨过(描述行宽超 pane 被 TUI 折成多行)。
    #[test]
    fn parses_menu_with_multiline_descriptions() {
        let pane = "\
选一个:
❯ 1. 方案A
     第一行描述
     第二行描述
  2. 方案B
     另一段描述";
        let st = parse_interactive(pane).expect("应识别带多行描述的菜单");
        assert_eq!(st.options.len(), 2);
        assert_eq!(st.options[0].label, "方案A");
        assert_eq!(st.options[1].label, "方案B");
    }

    /// 真机抓屏(2026-09-11,Claude Code 2.1.268,139×49):长 Bash 命令的授权对话框。
    /// 命令折成多行、每行带 `│ ` 前缀;对话框顶上是一条满宽 `─`。
    const PERMISSION_DIALOG_LONG: &str = r#"
 ▐▛███▛█   Claude Code v2.1.268
▝▜██████▀  Haiku 4.5 · Claude Max
  ▝▝ ▝▝    ~/workspace/personal/summon · /rc


❯ Use the Bash tool to run exactly this command, unchanged, with description 'probe long command'. Do nothing else:
  LOGS=/tmp/summon-perm-probe-nonexistent; cd /tmp && echo "archive step" > /dev/null && (cd /tmp && echo "converter step" > /dev/null);
  rm -f $LOGS/convert//.gguf $LOGS/convert/.gguf $LOGS/bin/.o $LOGS/vlm_*.bin $LOGS/.step4.bin $LOGS/mut/.vit $LOGS/mut/.bin; ls
  $LOGS/.bin 2>/dev/null | head; du -sh /tmp/summon-perm-probe-nonexistent 2>/dev/null; git -C /tmp/summon-perm-probe-engine worktree list
  2>/dev/null | grep review; git -C /tmp/summon-perm-probe-tools worktree list 2>/dev/null | grep review; ls -d /tmp/summon-perm-probe-*
  2>/dev/null; echo "worktrees left:"; git -C /tmp/summon-perm-probe-engine worktree prune 2>/dev/null; git -C
  /tmp/summon-perm-probe-tools worktree prune 2>/dev/null; echo "phase two"; git -C /tmp/summon-perm-probe-engine worktree list
  2>/dev/null | grep review; git -C /tmp/summon-perm-probe-tools worktree list 2>/dev/null | grep review; echo CLEANUP_OK

  Probing long command
  ⎿  $ LOGS=/tmp/summon-perm-probe-nonexistent; cd /tmp && echo "archive step" > /dev/null && (cd /tmp && echo "converter step" >
     /dev/null); rm -f $LOGS/convert/*/*.gguf $LOGS/convert/*.gguf $LOGS/bin/*.o $LOGS/vlm_*.bin $LOGS/*.step4.bin $LOGS/mut/*.vit
     $LOGS/mut/*.bin; ls $LOGS/*.bin 2>/dev/null | he…

───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 Bash command

   │ LOGS=/tmp/summon-perm-probe-nonexistent; cd /tmp && echo "archive step" > /dev/null && (cd /tmp && echo "converter step" >
   │ /dev/null); rm -f $LOGS/convert/*/*.gguf $LOGS/convert/*.gguf $LOGS/bin/*.o $LOGS/vlm_*.bin $LOGS/*.step4.bin $LOGS/mut/*.vit
   │ $LOGS/mut/*.bin; ls $LOGS/*.bin 2>/dev/null | head; du -sh /tmp/summon-perm-probe-nonexistent 2>/dev/null; git -C
   │ /tmp/summon-perm-probe-engine worktree list 2>/dev/null | grep review; git -C /tmp/summon-perm-probe-tools worktree list
   │ 2>/dev/null | grep review; ls -d /tmp/summon-perm-probe-* 2>/dev/null; echo "worktrees left:"; git -C /tmp/summon-perm-probe-engine
   │ worktree prune 2>/dev/null; git -C /tmp/summon-perm-probe-tools worktree prune 2>/dev/null; echo "phase two"; git -C
   │ /tmp/summon-perm-probe-engine worktree list 2>/dev/null | grep review; git -C /tmp/summon-perm-probe-tools worktree list
   │ 2>/dev/null | grep review; echo CLEANUP_OK
   probe long command

 This command uses shell operators that require approval for safety

 Do you want to proceed?
 ❯ 1. Yes
   2. No

 Esc to cancel · Tab to amend"#;

    /// 真机抓屏(同次):短命令的授权对话框,三个选项,命令不带 `│ ` 前缀。
    const PERMISSION_DIALOG_SHORT: &str = r#"
 ▐▛███▛█   Claude Code v2.1.268
▝▜██████▀  Haiku 4.5 · Claude Max
  ▝▝ ▝▝    ~/workspace/personal/summon · /rc


❯ Use the Bash tool to run exactly this command, unchanged, with description 'probe long command'. Do nothing else:
  LOGS=/tmp/summon-perm-probe-nonexistent; cd /tmp && echo "archive step" > /dev/null && (cd /tmp && echo "converter step" > /dev/null);
  rm -f $LOGS/convert//.gguf $LOGS/convert/.gguf $LOGS/bin/.o $LOGS/vlm_*.bin $LOGS/.step4.bin $LOGS/mut/.vit $LOGS/mut/.bin; ls
  $LOGS/.bin 2>/dev/null | head; du -sh /tmp/summon-perm-probe-nonexistent 2>/dev/null; git -C /tmp/summon-perm-probe-engine worktree list
  2>/dev/null | grep review; git -C /tmp/summon-perm-probe-tools worktree list 2>/dev/null | grep review; ls -d /tmp/summon-perm-probe-*
  2>/dev/null; echo "worktrees left:"; git -C /tmp/summon-perm-probe-engine worktree prune 2>/dev/null; git -C
  /tmp/summon-perm-probe-tools worktree prune 2>/dev/null; echo "phase two"; git -C /tmp/summon-perm-probe-engine worktree list
  2>/dev/null | grep review; git -C /tmp/summon-perm-probe-tools worktree list 2>/dev/null | grep review; echo CLEANUP_OK

  Ran 1 shell command
  ⎿  Interrupted · What should Claude do instead?

✻ Brewed for 6s · done 7:35 PM

❯ Use the Bash tool to run exactly: ls /tmp/summon-perm-probe-nonexistent   (description 'probe short'). Nothing else.

⏺ Probing short
  ⎿  $ ls /tmp/summon-perm-probe-nonexistent

───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 Bash command

   ls /tmp/summon-perm-probe-nonexistent
   probe short

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, allow reading from /tmp from this project
   3. No

 Esc to cancel · Tab to amend"#;

    /// 标题覆盖整个对话框:从顶线下的 `Bash command` 到问句,命令一行不少。警告点名的那段命令
    /// (`rm -f $LOGS/...`)在命令开头,标题少了开头,用户确认的就是一条看不全的命令。
    #[test]
    fn permission_dialog_title_spans_the_whole_dialog() {
        let st = parse_interactive(PERMISSION_DIALOG_LONG).expect("应识别授权对话框");
        let labels: Vec<&str> = st.options.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Yes", "No"]);
        let lines: Vec<&str> = st.title.lines().collect();
        assert_eq!(lines.first(), Some(&"Bash command"), "{}", st.title);
        assert_eq!(
            lines.last(),
            Some(&"Do you want to proceed?"),
            "{}",
            st.title
        );
        assert!(
            st.title
                .contains("  │ LOGS=/tmp/summon-perm-probe-nonexistent;"),
            "{}",
            st.title
        );
        assert!(
            st.title.contains("rm -f $LOGS/convert/*/*.gguf"),
            "{}",
            st.title
        );
        assert!(
            st.title.contains("This command uses shell operators"),
            "{}",
            st.title
        );
        assert!(
            !st.title.contains("Probing long command"),
            "顶线之上的转录不进标题: {}",
            st.title
        );
        assert!(
            !st.title.contains("Esc to cancel"),
            "选项之下的页脚不进标题"
        );
    }

    /// 空行分节保留、连续空行并成一行、公共缩进去掉,相对缩进不变。
    #[test]
    fn permission_dialog_title_keeps_sections_and_relative_indent() {
        let st = parse_interactive(PERMISSION_DIALOG_SHORT).expect("应识别授权对话框");
        assert_eq!(
            st.title,
            "Bash command\n\n  ls /tmp/summon-perm-probe-nonexistent\n  probe short\n\nDo you want to proceed?"
        );
        assert_eq!(st.options.len(), 3);
        assert_eq!(
            st.options[1].label,
            "Yes, allow reading from /tmp from this project"
        );
    }

    /// 公共缩进只按 ASCII 空格算,全角空格之类的多字节缩进不会被切成半个字符。
    #[test]
    fn title_indent_trim_is_char_safe() {
        let pane = "─".repeat(40) + "\n\u{3000}全角缩进的问句\n  普通缩进\n❯ 1. Yes\n  2. No";
        let st = parse_interactive(&pane).expect("应识别");
        assert_eq!(st.title, "\u{3000}全角缩进的问句\n  普通缩进");
    }

    /// 授权对话框弹出时输入框收起,菜单光标就是屏上最后一个 `❯`。
    #[test]
    fn awaiting_choice_recognizes_dialogs_at_the_bottom() {
        assert_eq!(
            awaiting_choice(PERMISSION_DIALOG_LONG).map(|st| st.options.len()),
            Some(2)
        );
        assert_eq!(
            awaiting_choice(PERMISSION_DIALOG_SHORT).map(|st| st.options.len()),
            Some(3)
        );
    }

    /// 用户发过的编号列表回显在转录里,与菜单逐字同形;它下面还压着输入框,不是在等人选。
    #[test]
    fn numbered_prompt_echo_above_the_input_box_is_not_awaiting_a_choice() {
        let pane = "\
❯ 1. 先改解析器
  2. 再加测试

⏺ 好的,两步都做完了。

  Esc to cancel · Tab to amend

─────────── summon:soul-x ──
❯\u{a0}
────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert!(parse_interactive(pane).is_some(), "形态上确实像菜单");
        assert!(
            awaiting_choice(pane).is_none(),
            "活光标在输入框里,不是在等人选"
        );
    }

    /// 屏上既有形似菜单的历史回显、又有真对话框时,锚点落在最底下的真对话框上。
    #[test]
    fn anchor_is_the_bottom_most_menu() {
        let pane = format!("❯ 1. 先改解析器\n  2. 再加测试\n\n{PERMISSION_DIALOG_SHORT}");
        let st = parse_interactive(&pane).expect("应识别");
        assert_eq!(st.options.len(), 3);
        assert_eq!(st.options[0].label, "Yes");
    }

    /// 同一张菜单剥不剥尾部 TUI 动作项,签名都一样;换了问句或选项就不一样。
    #[test]
    fn signature_ignores_trailing_ui_actions_only() {
        let opt = |index: usize, label: &str| Choice {
            index,
            label: label.into(),
        };
        let full = InteractiveState {
            title: "选哪个?".into(),
            options: vec![
                opt(1, "A"),
                opt(2, "B"),
                opt(3, "Type something."),
                opt(4, "Chat about this"),
            ],
            raw: "raw".into(),
        };
        let stripped = strip_trailing_ui_actions(full.clone());
        assert_eq!(full.signature(), stripped.signature());
        let other_title = InteractiveState {
            title: "选另一个?".into(),
            ..stripped.clone()
        };
        assert_ne!(stripped.signature(), other_title.signature());
        let other_option = InteractiveState {
            options: vec![opt(1, "A"), opt(2, "C")],
            ..stripped.clone()
        };
        assert_ne!(stripped.signature(), other_option.signature());
    }

    fn yes_no(title: &str) -> InteractiveState {
        InteractiveState {
            title: title.into(),
            options: vec![
                Choice {
                    index: 1,
                    label: "Yes".into(),
                },
                Choice {
                    index: 2,
                    label: "No".into(),
                },
            ],
            raw: String::new(),
        }
    }

    /// 守望起步那一拍屏上已有的菜单来历证不了,一直不发;它消失后再弹出,是新问题,照常发。
    #[test]
    fn watch_never_announces_the_menu_on_screen_at_start() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        let old = yes_no("old");
        for _ in 0..5 {
            assert_eq!(
                watch.observe(&ScreenMenu::Awaiting(old.clone()), &ledger),
                MenuWatchVerdict::Quiet
            );
        }
        watch.observe(&ScreenMenu::Unknown, &ledger);
        watch.observe(&ScreenMenu::Unknown, &ledger);
        assert_eq!(
            watch.observe(&ScreenMenu::Awaiting(old.clone()), &ledger),
            MenuWatchVerdict::Quiet,
            "重新弹出的第一拍只是候选"
        );
        assert_eq!(
            watch.observe(&ScreenMenu::Awaiting(old.clone()), &ledger),
            MenuWatchVerdict::Announce
        );
    }

    /// 新弹出的菜单连续看到两拍才发;发出去记了账,之后不再发。
    #[test]
    fn watch_announces_a_new_menu_once_on_the_second_sighting() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        let menu = yes_no("new");
        assert_eq!(
            watch.observe(&ScreenMenu::Unknown, &ledger),
            MenuWatchVerdict::Quiet
        );
        assert_eq!(
            watch.observe(&ScreenMenu::Awaiting(menu.clone()), &ledger),
            MenuWatchVerdict::Quiet
        );
        assert_eq!(
            watch.observe(&ScreenMenu::Awaiting(menu.clone()), &ledger),
            MenuWatchVerdict::Announce
        );
        ledger.record(&menu);
        for _ in 0..5 {
            assert_eq!(
                watch.observe(&ScreenMenu::Awaiting(menu.clone()), &ledger),
                MenuWatchVerdict::Quiet
            );
        }
    }

    /// 别的路径(轮内探测、注入被拒)已经发过的菜单,守望不再发。
    #[test]
    fn watch_skips_menus_another_path_already_announced() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        let menu = yes_no("carded in turn");
        watch.observe(&ScreenMenu::Unknown, &ledger);
        ledger.record(&menu);
        for _ in 0..5 {
            assert_eq!(
                watch.observe(&ScreenMenu::Awaiting(menu.clone()), &ledger),
                MenuWatchVerdict::Quiet
            );
        }
    }

    /// 一拍没看到不算菜单没了,账不清;连续两拍没看到才清。
    #[test]
    fn watch_forgets_a_menu_only_after_two_absent_probes() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        let menu = yes_no("flicker");
        watch.observe(&ScreenMenu::Unknown, &ledger);
        ledger.record(&menu);
        watch.observe(&ScreenMenu::Awaiting(menu.clone()), &ledger);
        watch.observe(&ScreenMenu::Unknown, &ledger);
        assert_eq!(ledger.recorded(), Some(menu.signature()), "一拍缺席不清账");
        assert_eq!(
            watch.observe(&ScreenMenu::Awaiting(menu.clone()), &ledger),
            MenuWatchVerdict::Quiet
        );
        watch.observe(&ScreenMenu::Unknown, &ledger);
        watch.observe(&ScreenMenu::Unknown, &ledger);
        assert_eq!(ledger.recorded(), None, "连续两拍缺席才清账");
    }

    /// 发卡失败下一拍重发,同一张菜单失败到上限就停。
    #[test]
    fn watch_retries_a_failed_announce_up_to_the_budget() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        let menu = yes_no("network down");
        watch.observe(&ScreenMenu::Unknown, &ledger);
        watch.observe(&ScreenMenu::Awaiting(menu.clone()), &ledger);
        for _ in 0..ANNOUNCE_TRIES {
            assert_eq!(
                watch.observe(&ScreenMenu::Awaiting(menu.clone()), &ledger),
                MenuWatchVerdict::Announce
            );
            watch.note_announce_failed(&menu);
        }
        assert_eq!(
            watch.observe(&ScreenMenu::Awaiting(menu.clone()), &ledger),
            MenuWatchVerdict::Quiet
        );
    }

    /// 同一个对话框随终端宽度重排折行,签名不变;命令不同,签名就不同。
    #[test]
    fn signature_survives_terminal_reflow() {
        let wide = yes_no(
            "Bash command\n\n  │ rm -f $LOGS/convert/*.gguf $LOGS/*.step4.bin; git worktree\n  │ remove --force /tmp/r2\n\nDo you want to proceed?",
        );
        let narrow = yes_no(
            "Bash command\n\n  │ rm -f $LOGS/convert/*.gguf\n  │ $LOGS/*.step4.bin; git worktree remove\n  │ --force /tmp/r2\n\nDo you want to proceed?",
        );
        assert_eq!(wide.signature(), narrow.signature());
        let other = yes_no("Bash command\n\n  │ rm -f $LOGS/other.bin\n\nDo you want to proceed?");
        assert_ne!(wide.signature(), other.signature());
    }

    /// 输入框里打着编号列表的草稿也是最后一个 `❯`;它下面没有对话框页脚,不是在等人选。
    #[test]
    fn numbered_draft_in_the_input_box_is_not_awaiting_a_choice() {
        let pane = "\
⏺ 好的,两步都做完了。

─────────── summon:soul-x ──
❯ 1. 先改解析器
  2. 再加测试
────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)
  ⎿  Tip: press Esc to cancel a queued message";
        assert!(parse_interactive(pane).is_some(), "形态上确实像菜单");
        assert!(awaiting_choice(pane).is_none(), "草稿在输入框里,不是对话框");
    }

    /// 真机抓屏(2026-09-11,同版本):后台子代理申请的授权。主会话已经收尾、在等子代理,输入框收起,
    /// 对话框标题带「from the general-purpose agent」。
    const PERMISSION_DIALOG_FROM_SUBAGENT: &str = r#"
 ▐▛███▛█   Claude Code v2.1.268
▝▜██████▀  Haiku 4.5 · Claude Max
  ▝▝ ▝▝    ~/workspace/personal/summon · /rc


❯ Launch exactly one background subagent with the Agent tool (subagent_type general-purpose, run_in_background true). Its only task: run
  the Bash command 'ls /tmp/summon-perm-probe-subagent' and report the output. After launching it, reply with the single word launched and
  end your turn. Do not run any tool yourself.

⏺ Agent(Probe /tmp directory listing)
  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)

⏺ launched

✻ Waiting for 1 background agent to finish

───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 Bash command · from the general-purpose agent

   ls /tmp/summon-perm-probe-subagent
   列出目录内容

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, allow reading from /tmp from this project
   3. No

 Esc to cancel · Tab to amend"#;

    /// 后台子代理的授权框(轮外最典型的一种)认得出在等人选,标题从对话框顶线取起。
    #[test]
    fn background_subagent_permission_dialog_is_awaiting_a_choice() {
        let st = awaiting_choice(PERMISSION_DIALOG_FROM_SUBAGENT).expect("应认出在等人选");
        assert_eq!(st.options.len(), 3);
        assert_eq!(
            st.title.lines().next(),
            Some("Bash command · from the general-purpose agent")
        );
        assert!(
            st.title.contains("ls /tmp/summon-perm-probe-subagent"),
            "{}",
            st.title
        );
        assert!(
            !st.title.contains("Waiting for 1 background agent"),
            "顶线之上的转录不进标题: {}",
            st.title
        );
    }

    fn awaiting(menu: &InteractiveState) -> ScreenMenu {
        ScreenMenu::Awaiting(menu.clone())
    }

    /// 真机抓屏(2026-09-12,Claude Code 2.1.269):AskUserQuestion 的对话框。顶线紧贴着用户那条
    /// prompt 回显 —— 发一条消息、claude 直接弹框,真机上最常见的形态。选项里 3 与 4 是 claude 自己的
    /// TUI 动作项,它俩之间还画着一条与顶线同宽的横线。
    const ASK_USER_QUESTION_DIALOG: &str = r#"
 ▐▛███▛█   Claude Code v2.1.269
▝▜██████▀  Haiku 4.5 · Claude Max
  ▝▝ ▝▝    /private/tmp/askq-probe · /rc


❯ Use the AskUserQuestion tool right now to ask me which deployment environment I prefer. Two options: Staging (pre-release, mirrors production config) and Production (live, real users). Do nothing
  else, call no other tool.

⏺ User declined to answer questions
  ⎿  · Which deployment environment do you prefer? (Staging / Production)

✻ Sautéed for 4s · done 10:31 AM

❯ Ask me tea or coffee via AskUserQuestion. Nothing else.
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 ☐ Beverage

Tea or coffee?

❯ 1. Tea
     Tea
  2. Coffee
     Coffee
  3. Type something.
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  4. Chat about this

Enter to select · ↑/↓ to navigate · Esc to cancel"#;

    /// AskUserQuestion 的对话框认得出在等人选,标题从顶线取起 —— 顶线紧贴着 prompt 回显,而输入框
    /// 要靠它框住的输入行认,这屏上没有。
    #[test]
    fn ask_user_question_dialog_takes_its_title_from_the_top_rule() {
        let st = awaiting_choice(ASK_USER_QUESTION_DIALOG).expect("应认出在等人选");
        assert!(!title_is_cut(&st.title), "{}", st.title);
        assert_eq!(st.title.lines().next(), Some(" ☐ Beverage"));
        assert!(st.title.contains("Tea or coffee?"), "{}", st.title);
        assert!(
            !st.title.contains("Ask me tea or coffee"),
            "顶线之上的 prompt 回显不进标题: {}",
            st.title
        );
        let real = strip_trailing_ui_actions(st);
        let labels: Vec<&str> = real.options.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Tea", "Coffee"]);
    }

    /// 只差一个空格的两条命令是两张菜单(点击绑定靠签名防误批准)。
    #[test]
    fn signature_keeps_word_boundaries() {
        let title = |cmd: &str| format!("Bash command\n\n  {cmd}\n\nDo you want to proceed?");
        for (a, b) in [
            ("rm -rf ./build/", "rm -rf ./build /"),
            ("git push origin :main", "git push origin : main"),
            ("chmod -R 700 ~/a", "chmod -R 7 00 ~/a"),
        ] {
            assert_ne!(
                yes_no(&title(a)).signature(),
                yes_no(&title(b)).signature(),
                "{a} vs {b}"
            );
        }
    }

    /// 屏幕判成什么:四种真机对话框在等人选;回到输入框是正向的「没有菜单」;输入框里的编号草稿、
    /// 空屏都证明不了。
    #[test]
    fn screen_menu_classifies_real_screens() {
        for pane in [
            PERMISSION_DIALOG_LONG,
            PERMISSION_DIALOG_SHORT,
            PERMISSION_DIALOG_FROM_SUBAGENT,
            ASK_USER_QUESTION_DIALOG,
        ] {
            assert!(matches!(screen_menu(pane), ScreenMenu::Awaiting(_)));
        }
        let idle = "⏺ 好的。\n\n─────────── summon:soul-x ──\n❯\u{a0}\n────────────────────────────\n  ⏵⏵ bypass permissions on";
        assert_eq!(screen_menu(idle), ScreenMenu::Absent);
        let draft = "─────────── summon:soul-x ──\n❯ 1. 先改解析器\n  2. 再加测试\n────────────────────────────";
        assert_eq!(screen_menu(draft), ScreenMenu::Unknown);
        assert_eq!(screen_menu(""), ScreenMenu::Unknown);
    }

    /// 输入框回来是菜单没了的正向证据,当拍清账;同一条命令紧接着再申请,照常发。
    #[test]
    fn watch_forgets_at_once_when_the_input_box_is_back() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        let menu = yes_no("same command again");
        watch.observe(&ScreenMenu::Absent, &ledger);
        ledger.record(&menu);
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Quiet
        );
        watch.observe(&ScreenMenu::Absent, &ledger);
        assert_eq!(ledger.recorded(), None);
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Quiet
        );
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Announce
        );
    }

    /// 后台子代理一条接一条申请授权:每一张新菜单都发。
    #[test]
    fn watch_announces_every_prompt_in_a_sequence() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        watch.observe(&ScreenMenu::Absent, &ledger);
        let mut announced = 0;
        for i in 0..10 {
            let menu = yes_no(&format!("command {i}"));
            for _ in 0..2 {
                if watch.observe(&awaiting(&menu), &ledger) == MenuWatchVerdict::Announce {
                    ledger.record(&menu);
                    announced += 1;
                }
            }
            watch.observe(&ScreenMenu::Unknown, &ledger);
        }
        assert_eq!(announced, 10);
    }

    /// 最后一个选项带几行描述,页脚在描述之后,照样认得出对话框。
    #[test]
    fn footer_after_a_multiline_last_description_still_counts() {
        let pane = "\
──────────────────────────────
 你希望部署到哪个环境？

 ❯ 1. Staging
      预发布环境。
   2. Production
      生产环境,直接面向真实用户。
      发布前需确认回滚方案。
      还要确认监控告警就绪。

 Enter to select · ↑/↓ to navigate · Esc to cancel";
        assert_eq!(awaiting_choice(pane).map(|st| st.options.len()), Some(2));
    }

    /// 对话框(带页脚)在屏上找不到顶线,标题第一行写明开头没显示全;找得到顶线、或不是对话框的菜单不写。
    #[test]
    fn dialog_whose_top_is_off_screen_says_so() {
        let cut = "   │ tail of a long command\n   probe\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel · Tab to amend";
        let st = parse_interactive(cut).expect("应识别");
        assert_eq!(
            st.title.lines().next(),
            Some(TITLE_CUT_MARKER),
            "{}",
            st.title
        );
        let full = parse_interactive(PERMISSION_DIALOG_SHORT).unwrap();
        assert!(!full.title.contains(TITLE_CUT_MARKER));
        let effort = parse_interactive("Change effort level?\n❯ 1. Yes\n  2. No").unwrap();
        assert!(!effort.title.contains(TITLE_CUT_MARKER));
    }

    /// 签名写进飞书按钮、跨版本拿回来比对:算法写死,同样的菜单永远是同一个值。
    #[test]
    fn signature_is_stable_across_builds() {
        assert_eq!(
            yes_no("Do you want to proceed?").signature(),
            0x7cc20a7f181e96a0
        );
        let token = signature_token(yes_no("Do you want to proceed?").signature());
        assert_eq!(
            parse_signature_token(&token),
            Some(yes_no("Do you want to proceed?").signature())
        );
        assert_eq!(parse_signature_token("not-a-token"), None);
    }

    /// claude 对无人值守会话的授权框(带每秒刷新的自动拒绝倒计时)。
    fn countdown_dialog(command: &str, reading: &str) -> String {
        format!(
            "{}\n Bash command\n\n   {command}\n   Probe a dummy job\n\n \
             │ Dangerous rm operation on possibly-empty variable path: $S/* in `{command}`\n \
             │ (rewrite it as \"${{S:?}}\"/* or use a literal path)\n \
             ⚠ Claude Code will automatically deny this request in {reading}, to avoid blocking\n \
             progress on an unattended session\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n \
             Esc to cancel · Tab to amend",
            "─".repeat(40)
        )
    }

    fn countdown_menu(command: &str, reading: &str) -> InteractiveState {
        awaiting_choice(&countdown_dialog(command, reading)).expect("应认出授权框")
    }

    /// 倒计时走动不是新菜单,否则每一拍探测都会补发一张卡。
    #[test]
    fn the_auto_deny_countdown_does_not_change_the_signature() {
        let first = countdown_menu("rm -f $S/*", "1:55").signature();
        assert_eq!(countdown_menu("rm -f $S/*", "1:53").signature(), first);
        assert_eq!(countdown_menu("rm -f $S/*", "0:09").signature(), first);
        assert_ne!(
            countdown_menu("rm -f $T/*", "1:55").signature(),
            first,
            "命令不同是两张菜单"
        );
        // 读屏模式的读数形态。
        let spoken = countdown_menu("rm -f $S/*", "about 45 seconds").signature();
        assert_eq!(
            countdown_menu("rm -f $S/*", "about 2 minutes").signature(),
            spoken
        );
    }

    /// 只抹整句倒计时里的读数:问句里别处的数字 —— 包括命令里抄了半句这段话 —— 都区分菜单。
    #[test]
    fn other_numbers_in_the_title_distinguish_menus() {
        let sig = |cmd: &str| {
            yes_no(&format!(
                "Bash command\n\n  {cmd}\n\nDo you want to proceed?"
            ))
            .signature()
        };
        let differ = |a: &str, b: &str| assert_ne!(sig(a), sig(b), "{a} / {b}");
        differ("curl localhost:8080", "curl localhost:8081");
        // 只有前缀,读数后面跟的不是固定后缀(词数足够,只能靠后缀比对区分)。
        differ(
            "echo Claude Code will automatically deny this request in 1:55, then keep going with the rest of it",
            "echo Claude Code will automatically deny this request in 9:55, then keep going with the rest of it",
        );
        // 前后缀齐全,但中间不是倒计时读数。
        differ(
            "echo Claude Code will automatically deny this request in 8080, to avoid blocking progress on an unattended session",
            "echo Claude Code will automatically deny this request in 22, to avoid blocking progress on an unattended session",
        );
        differ(
            "echo Claude Code will automatically deny this request in 1:5, to avoid blocking progress on an unattended session",
            "echo Claude Code will automatically deny this request in 1:6, to avoid blocking progress on an unattended session",
        );
        // 读数后没有句中的逗号。
        differ(
            "echo Claude Code will automatically deny this request in 1:55 to avoid blocking progress on an unattended session",
            "echo Claude Code will automatically deny this request in 1:56 to avoid blocking progress on an unattended session",
        );
        // 读屏模式的读数缺 `about`、单位不对。
        differ(
            "echo Claude Code will automatically deny this request in 45 seconds, to avoid blocking progress on an unattended session",
            "echo Claude Code will automatically deny this request in 46 seconds, to avoid blocking progress on an unattended session",
        );
        differ(
            "echo Claude Code will automatically deny this request in about 1 hours, to avoid blocking progress on an unattended session",
            "echo Claude Code will automatically deny this request in about 2 hours, to avoid blocking progress on an unattended session",
        );
        // 动词不是 deny。
        differ(
            "echo Claude Code will automatically run this request in 1:55, to avoid blocking progress on an unattended session",
            "echo Claude Code will automatically run this request in 1:56, to avoid blocking progress on an unattended session",
        );
    }

    /// 轮外守望:同一张授权框的倒计时在走,只发一次卡(签名随倒计时变的话一次都凑不齐)。
    /// 后半段钉的是起步规则:守望起步时就在屏上的那张,倒计时走动也一直不发。
    #[test]
    fn watch_announces_a_counting_down_dialog_once() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        watch.observe(&ScreenMenu::Absent, &ledger);
        let beats =
            ["1:55", "1:53", "1:51", "1:49", "1:47"].map(|r| countdown_menu("rm -f $S/*", r));
        let verdicts: Vec<MenuWatchVerdict> = beats
            .iter()
            .map(|menu| {
                let verdict = watch.observe(&awaiting(menu), &ledger);
                if verdict == MenuWatchVerdict::Announce {
                    ledger.record(menu);
                }
                verdict
            })
            .collect();
        assert_eq!(
            verdicts
                .iter()
                .filter(|v| **v == MenuWatchVerdict::Announce)
                .count(),
            1,
            "{verdicts:?}"
        );

        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        for reading in ["1:55", "1:53", "1:51"] {
            assert_eq!(
                watch.observe(&awaiting(&countdown_menu("rm -f $S/*", reading)), &ledger),
                MenuWatchVerdict::Quiet
            );
        }
    }

    /// 对话框再高,标题和签名都从它的顶线取起:开头不同、结尾相同的两条长命令是两张菜单。
    #[test]
    fn tall_dialogs_differing_only_at_the_top_have_different_signatures() {
        let dialog = |first: &str| {
            let mut pane = format!("{}\n Bash command\n\n   {first}\n", "─".repeat(40));
            for i in 0..120 {
                pane.push_str(&format!("   echo step {i}\n"));
            }
            pane.push_str("\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel");
            pane
        };
        let a = awaiting_choice(&dialog("rm -rf /tmp/scratch")).expect("应认出");
        let b = awaiting_choice(&dialog("rm -rf $HOME")).expect("应认出");
        assert!(a.title.starts_with("Bash command"), "{}", a.title);
        assert!(!title_is_cut(&a.title));
        assert_ne!(a.signature(), b.signature());
    }

    /// 卡片上答过的菜单,同一条命令紧接着再申请(中间没回输入框)照样发。
    #[test]
    fn watch_announces_an_identical_prompt_after_its_card_was_answered() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        let menu = yes_no("queued identical request");
        watch.observe(&ScreenMenu::Absent, &ledger);
        watch.observe(&awaiting(&menu), &ledger);
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Announce
        );
        ledger.record(&menu);
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Quiet
        );
        ledger.note_answered(menu.signature());
        assert_eq!(ledger.recorded(), None);
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Quiet
        );
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Announce
        );
    }

    /// 答过的是另一张菜单,账上这张的记录不动。
    #[test]
    fn note_answered_only_drops_the_matching_record() {
        let ledger = MenuLedger::default();
        let (a, b) = (yes_no("a"), yes_no("b"));
        ledger.record(&b);
        ledger.note_answered(a.signature());
        assert_eq!(ledger.recorded(), Some(b.signature()));
    }

    /// 不是对话框的菜单,标题从选项上方最近的横线下面取起:横线上方的状态栏在走秒,签名不能跟着变。
    #[test]
    fn a_menu_without_footer_takes_its_title_below_the_nearest_rule() {
        let pane = |secs: u32| {
            format!(
                "✻ Waiting for 1 background agent · {secs}s\n\n{}\nChange effort level?\n❯ 1. Yes\n  2. No",
                "─".repeat(40)
            )
        };
        let a = parse_interactive(&pane(12)).expect("应识别");
        let b = parse_interactive(&pane(17)).expect("应识别");
        assert_eq!(a.title, "Change effort level?");
        assert_eq!(a.signature(), b.signature());
    }

    /// 输入框的边框不是对话框顶线:紧挨着 `❯` 的横线、与选项之间隔着 `❯` 的横线,对话框都按缺了开头算。
    #[test]
    fn input_box_borders_are_not_dialog_tops() {
        let tail = (0..120)
            .map(|i| format!("   │ tail {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let dialog = "\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel";
        let r = "─".repeat(40);
        let beside = format!("{r}\n❯\u{a0}\n{r}\n  status line\n{tail}{dialog}");
        let st = parse_interactive(&beside).expect("应识别");
        assert!(title_is_cut(&st.title), "{}", st.title);
        let between =
            format!("{r}\n  earlier output\n❯ an earlier prompt\n  more output\n{tail}{dialog}");
        let st = parse_interactive(&between).expect("应识别");
        assert!(title_is_cut(&st.title), "{}", st.title);
        let full = parse_interactive(PERMISSION_DIALOG_FROM_SUBAGENT).unwrap();
        assert!(!title_is_cut(&full.title), "{}", full.title);
    }

    /// 起步就在屏上的菜单,卡片上答过之后同一张再弹出,是新问题。
    #[test]
    fn watch_announces_a_start_menu_again_once_its_card_was_answered() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        let menu = yes_no("pending when the watch started");
        watch.observe(&awaiting(&menu), &ledger);
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Quiet
        );
        ledger.note_answered(menu.signature());
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Quiet
        );
        assert_eq!(
            watch.observe(&awaiting(&menu), &ledger),
            MenuWatchVerdict::Announce
        );
    }

    /// 卡片上答掉的是别的菜单,守望手里跟着的这张不受影响。
    #[test]
    fn answering_another_menu_leaves_the_watched_one_alone() {
        let (mut watch, ledger) = (MenuWatch::default(), MenuLedger::default());
        let (on_screen, other) = (
            yes_no("still pending at start"),
            yes_no("answered elsewhere"),
        );
        watch.observe(&awaiting(&on_screen), &ledger);
        ledger.note_answered(other.signature());
        for _ in 0..3 {
            assert_eq!(
                watch.observe(&awaiting(&on_screen), &ledger),
                MenuWatchVerdict::Quiet
            );
        }
    }
}
