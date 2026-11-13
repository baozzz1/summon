use std::borrow::Cow;

/// 拆散尖括号标签形态用的零宽空格。选 U+200B:飞书对它的渲染久经实测(常被用来打断 @/URL
/// 识别),视觉零宽;U+2060 等更冷门的不可见字符在部分端上有渲成方框的风险。
pub const LARK_MD_TAG_BREAKER: char = '\u{200B}';

/// 飞书 lark_md 把内容按 HTML 解析:`<alias>`、`Vec<T>` 这种「字母开头的尖括号」会被当成未知
/// 标签**静默剥离**(中文开头的 `<档位>` 因标签名非法而侥幸留存)。所以任何**外部文本**——agent
/// 回复、用户问题摘要、终端原始输出——进卡片前都必须消毒 `<`,否则内容凭空消失且无人察觉。
///
/// 但**不能用 HTML 实体转义**:飞书只解析标签、不解码实体(2026-08-12 owner 真机实测,
/// `&lt;id>` 原样示人),`&amp;`/`&lt;` 会满屏字面出现。所以这里改成最小干预:只在「`<` +
/// 会被当成标签起始的下一字符」之间插一个零宽空格,拆掉标签形态;其余 `<` 和所有 `&`
/// 原样保留——飞书不解码实体,它们本来就安全。
///
/// 触发集:ASCII 字母是**已在真机记录过被剥离**的形态;`/` `!` `?` 是 HTML 另外三种标签起始
/// (闭标签/注释/处理指令),未真机复核,按同一解析器家族防御性拆散——尤其字面 `</at>`、`<!--`
/// 一旦被当标签解析,会闭掉受信 mention 或吞掉后续全部内容。不动 `>`:单独的 `>` 无歧义。
///
/// 代价(显式取舍):视觉正确,但用户从卡片**复制**含 `<` 标签形态的代码会带上隐形字符
/// (`Vec<T>` 粘进编译器会报 unknown token)。旧实体方案复制出来是 `Vec&lt;T>`,同样编译不过
/// ——复制保真在「飞书剥标签 + 不解实体」的前提下无解,两害相权取视觉正确的那个。
///
/// 幂等:插入后 `<` 的下一字符是零宽空格,不再命中触发集,重复调用不会叠加。
/// summon 自己的模板(如 `<font color='grey'>`、`<at>`)**不走这里**——消毒的是塞进模板的值,
/// 不是模板;这也是 [`RenderedLarkMd`] 信任边界成立的前提:外部文本过此函数后不可能再含
/// `<at`/`</at` 字面形态。
pub fn escape_lark_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '<'
            && chars
                .peek()
                .is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?'))
        {
            out.push(LARK_MD_TAG_BREAKER);
        }
    }
    out
}

/// 已完成「外部文本转义 + summon 受信标签插入」的 lark_md。
///
/// 构造器不对外开放：调用方只能通过 [`render_external_lark_md`] 从外部文本得到它，避免把
/// agent 输出里的 `<tag>` 误当成 summon 模板；后续卡片包装也不会再次转义其中由 summon 插入的
/// `<at>`。这条类型边界把“不可信文本”和“可信 mention 标签”从字符串约定提升成编译期约束。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedLarkMd(String);

impl RenderedLarkMd {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 流式单卡裁剪。输入已经安全渲染，裁剪结果仍保持同一信任级别。
    pub fn clipped_for_stream(&self, max: usize) -> Self {
        Self(clip_rendered_for_stream(&self.0, max).unwrap_or_else(|| self.0.clone()))
    }
}

/// 已渲染 markdown 的裁剪必须把 summon 生成的 `<at>…</at>` 当原子 token；纯 char 边界仍可能
/// 切在标签属性或闭标签中间，导致飞书拒绝整张流式卡。外部的 `<字母`/`</` 已被零宽空格拆散，
/// 因此这里出现的 `<at `/`</at>` 只可能来自受信渲染器。
fn clip_rendered_for_stream(text: &str, max: usize) -> Option<String> {
    if text.len() <= max {
        return None;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }

    let mut search_from = 0;
    while let Some(rel_open) = text[search_from..].find("<at ") {
        let open = search_from + rel_open;
        if open >= end {
            break;
        }
        let Some(rel_close) = text[open..].find("</at>") else {
            // 类型不变量被破坏时也 fail-closed：不把残缺受信标签继续送给飞书。
            end = open;
            break;
        };
        let close_end = open + rel_close + "</at>".len();
        if end < close_end {
            end = open;
            break;
        }
        search_from = close_end;
    }

    Some(summon_i18n::format!("r7e7c1e3d66", &text[..end]))
}

/// 飞书卡片的 markdown 不渲染嵌在列表项里的代码块:列表项下缩进的 ``` / ~~~ 围栏连同代码整段
/// 消失,读回的正文里也没有(2026-10-16 真机);顶格的围栏正常。所以把有缩进、在本容器内闭合的
/// 围栏连同代码提到顶格,前后补空行让它独立成块;代码行只去掉开行那么多的缩进,更深的缩进原样
/// 保留。配对按 CommonMark:闭合行相对开行最多再缩进 3 格;列表里的围栏遇到缩进回退的行即随
/// 列表项结束。没在本容器内闭合的(含流式中途)、与列表标记同一行起的围栏都原样不动,不含可提
/// 围栏的文本逐字节不变。
pub fn hoist_indented_code_fences(text: &str) -> Cow<'_, str> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + 4);
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let Some(open) = CodeFence::open(lines[i]) else {
            out.push(lines[i]);
            i += 1;
            continue;
        };
        match open.end(&lines[i + 1..]) {
            FenceEnd::Closed(offset) => {
                let close = i + 1 + offset;
                if open.indent == 0 || open.after_list_marker {
                    out.extend_from_slice(&lines[i..=close]);
                } else {
                    changed = true;
                    if out.last().is_some_and(|l| !l.trim().is_empty()) {
                        out.push("");
                    }
                    out.push(&lines[i][open.indent..]);
                    out.extend(
                        lines[i + 1..close]
                            .iter()
                            .map(|l| strip_indent(l, open.indent)),
                    );
                    out.push(&lines[close][leading_blank(lines[close])..]);
                    if lines.get(close + 1).is_some_and(|l| !l.trim().is_empty()) {
                        out.push("");
                    }
                }
                i = close + 1;
            }
            FenceEnd::ContainerEnded(offset) => {
                out.extend_from_slice(&lines[i..=i + offset]);
                i += 1 + offset;
            }
            FenceEnd::Unclosed => {
                out.extend_from_slice(&lines[i..]);
                break;
            }
        }
    }
    if changed {
        Cow::Owned(out.join("\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// 围栏开行:至少 3 个 ` 或 ~。
struct CodeFence {
    /// 围栏所在容器的内容缩进:开行之前的字节数(空格与 tab 都按一个字节)。
    indent: usize,
    /// 开在列表标记同一行(`1. ```` / `- ````)。只认出来配对,不改写。
    after_list_marker: bool,
    marker: u8,
    len: usize,
}

enum FenceEnd {
    /// 开行之后第几行是闭合行。
    Closed(usize),
    /// 开行之后第几行缩进回退、列表项已结束;它之前的都是这个围栏的代码。
    ContainerEnded(usize),
    /// 到文本末尾都没闭合:后面全是它的代码。
    Unclosed,
}

impl CodeFence {
    fn open(line: &str) -> Option<Self> {
        let leading = leading_blank(line);
        if let Some((marker, len)) = fence_run(&line[leading..]) {
            return Some(Self {
                indent: leading,
                after_list_marker: false,
                marker,
                len,
            });
        }
        let content = leading + list_marker_width(&line[leading..])?;
        let (marker, len) = fence_run(&line[content..])?;
        Some(Self {
            indent: content,
            after_list_marker: true,
            marker,
            len,
        })
    }

    fn end(&self, after: &[&str]) -> FenceEnd {
        for (offset, line) in after.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let leading = leading_blank(line);
            if leading < self.indent {
                return FenceEnd::ContainerEnded(offset);
            }
            if leading - self.indent <= 3 && self.closed_by(&line[leading..]) {
                return FenceEnd::Closed(offset);
            }
        }
        FenceEnd::Unclosed
    }

    /// 闭合行(已去掉前导空白):同种字符、不短于开行,后面只有空白。
    fn closed_by(&self, rest: &str) -> bool {
        let len = rest.bytes().take_while(|&b| b == self.marker).count();
        len >= self.len && rest[len..].trim().is_empty()
    }
}

/// 去掉前导空白后的一段若以围栏起头,返回围栏字符与长度。
fn fence_run(rest: &str) -> Option<(u8, usize)> {
    let marker = *rest.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = rest.bytes().take_while(|&b| b == marker).count();
    // 反引号围栏的信息串里不能再有反引号,否则那是行内代码。
    if len < 3 || (marker == b'`' && rest[len..].contains('`')) {
        return None;
    }
    Some((marker, len))
}

/// 列表标记(`-`/`*`/`+` 或 1–9 位数字加 `.`/`)`)连同其后空白的字节数;不是列表标记返回 None。
fn list_marker_width(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let marker = match bytes.first()? {
        b'-' | b'*' | b'+' => 1,
        _ => {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if !(1..=9).contains(&digits) || !matches!(bytes.get(digits), Some(b'.' | b')')) {
                return None;
            }
            digits + 1
        }
    };
    let gap = leading_blank(&rest[marker..]);
    (gap > 0).then_some(marker + gap)
}

fn leading_blank(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn strip_indent(line: &str, indent: usize) -> &str {
    &line[leading_blank(line).min(indent)..]
}

/// 一段回复被飞书内容审核拦下时,顶替它发进群里的说明。`reason` 是飞书给的原因;只留类别名
/// 那几种字符并截短 —— 说明本身也要过审核,原因里若回显了命中的内容,原样带上就会再被拦一次。
pub fn content_rejected_notice(reason: &str) -> String {
    let reason: String = reason
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | ':' | ',' | '.' | '-'))
        .take(60)
        .collect();
    let reason = reason.trim();
    if reason.is_empty() {
        summon_i18n::text!("ree28e62a8e").to_string()
    } else {
        summon_i18n::format!("rdb6e30c0b9")
    }
}

/// 外部正文进 markdown 元素前的统一处理:列表里的代码块提到顶格,再拆散标签形态。
fn lark_md_body(text: &str) -> String {
    escape_lark_md(&hoist_indented_code_fences(text))
}

/// 把**外部文本**渲染为可直接送进飞书 markdown 元素的安全内容：
///
/// 1. 先把列表里的代码块提到顶格([`hoist_indented_code_fences`]),再把外部的尖括号标签形态
///    拆散（[`escape_lark_md`]），阻断任意标签注入；
/// 2. 再只为已知 peer 插入 summon 自己生成的 `<at>` 标签。
///
/// 顺序不可反转：先插 `<at>` 再整体消毒会把 mention 的 `<at` 拆成「`<`+零宽空格+`at`」，
/// 飞书不再识别，mention 静默失效。
pub fn render_external_lark_md(
    text: &str,
    peers: &[crate::peers::Peer],
    alias_map: &std::collections::HashMap<String, String>,
) -> RenderedLarkMd {
    let escaped = lark_md_body(text);
    RenderedLarkMd(render_outbound_mentions(&escaped, peers, alias_map))
}

/// 一个装**外部文本**的 markdown 元素(内容转义)。summon 自己的固定文案不必走它。
fn md_el(content: &str) -> serde_json::Value {
    serde_json::json!({"tag": "markdown", "content": lark_md_body(content)})
}

/// 装已经过 [`render_external_lark_md`] 处理的 markdown；不得接受裸外部字符串。
fn rendered_md_el(content: &RenderedLarkMd) -> serde_json::Value {
    serde_json::json!({"tag": "markdown", "content": content.as_str()})
}

/// 总览页上一个下钻按钮的目标:本机 agent(`remote = None`)或某台远端机器上的 agent。
///
/// 远端目标必须带上机器名:两台机器上完全可以有同名 agent,只凭 alias 会点到另一台去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrilldownTarget {
    pub alias: String,
    pub remote: Option<String>,
}

impl DrilldownTarget {
    pub fn local(alias: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
            remote: None,
        }
    }

    pub fn remote(alias: impl Into<String>, remote: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
            remote: Some(remote.into()),
        }
    }

    /// 按钮上的字。远端加机器名前缀 —— 一排按钮里必须一眼看出哪个在哪台机器上。
    fn label(&self) -> String {
        match &self.remote {
            Some(r) => format!("{r}/{}", self.alias),
            None => self.alias.clone(),
        }
    }
}

/// fleet 状态卡:[`待处理区`](crate::fleet_card::fleet_waiting_header) + 正文(由
/// [`crate::fleet_card`] 渲好)+ 一行按钮。
///
/// 下钻卡(`detail=true`)另给「返回总览」+(有 alias 时)「管理会话」。
///
/// `waiting` = 卡片最上面的待处理区(与 Menu Bar 的 Waiting 区同一份数据、同一个位置),只在总览
/// 上给,下钻页传空。每条整行是跳回那条回复的链接,右侧「标记已处理」只清这一条:按钮带上
/// 渲染时看到的回复 id,期间 agent 又回了一条就不清。
///
/// 刷新 / 下钻 / 管理会话这些按钮免鉴权:它们只是重新渲染一张只读卡片。「标记已处理」
/// 改盘上的 waiting 标记,过管家自己的 operate 闸,但不触碰任何会话。停会话的按钮不在这张卡上 ——
/// 「管理会话」只是翻到 [`card_fleet_sessions`] 那一页,真正的退出按钮在那页上,各自带二次确认。
/// 「看一眼状态」和「停掉一条会话」不共用同一次点击(owner 2026-08-10 加操作页时保持了这条)。
///
/// `drilldown` = 总览页上要给出下钻按钮的 agent(其它页传空)。**why**:下钻此前只能手打
/// `/fleet <agent>`,而人看完总览的下一个动作八成就是点进某一个 —— 让他再去输入框敲一遍名字,
/// 是把已经在眼前的信息又要了一次(owner 2026-08-10)。
///
/// `notice` = 上一次「标记已处理」的结果,渲在页首;下一次刷新自然清掉。
pub fn card_fleet(
    markdown: &str,
    detail: bool,
    alias: Option<&str>,
    drilldown: &[DrilldownTarget],
    waiting: &[crate::waiting::WaitingStatus],
    notice: Option<&str>,
) -> String {
    let mut buttons = vec![cb_btn(
        summon_i18n::text!("r9da4432b05"),
        "default",
        serde_json::json!({"action": "fleet_refresh"}),
    )];
    if detail {
        buttons.push(cb_btn(
            summon_i18n::text!("r9c33d3fc06"),
            "default",
            serde_json::json!({"action": "fleet_refresh"}),
        ));
        // 下钻到某个 agent 才有「管理会话」——总览页没有唯一的操作对象。
        if let Some(alias) = alias.map(str::trim).filter(|a| !a.is_empty()) {
            buttons.push(cb_btn(
                summon_i18n::text!("r23acb4b993"),
                "default",
                serde_json::json!({"action": "fleet_sessions", "agent": alias}),
            ));
        }
    }
    let mut elements: Vec<serde_json::Value> = Vec::new();
    if let Some(notice) = notice.map(str::trim).filter(|s| !s.is_empty()) {
        elements.push(md_el(notice));
    }
    if !waiting.is_empty() {
        use crate::fleet_card::{fleet_waiting_header, MAX_WAITING_ROWS};
        elements.push(md_el(&fleet_waiting_header(waiting.len())));
        let shown = waiting.len().min(MAX_WAITING_ROWS);
        for item in &waiting[..shown] {
            elements.push(fleet_waiting_row(item));
        }
        if waiting.len() > shown {
            elements.push(json_grey_line(&summon_i18n::format!(
                "r644cd69862",
                waiting.len() - shown
            )));
        }
        elements.push(serde_json::json!({"tag": "hr"}));
    }
    elements.push(md_el(markdown));
    // 总览页:每个 agent 一个下钻按钮。**按钮数必须收口** —— 一股脑塞进分栏会让飞书拒渲
    // (模型候选那边真机踩过 230099),所以截到上限 + 每行固定几个,截掉的明示可手打 `/fleet <名>`。
    let names: Vec<&DrilldownTarget> = drilldown
        .iter()
        .filter(|t| !t.alias.trim().is_empty())
        .collect();
    if !names.is_empty() {
        elements.push(serde_json::json!({"tag": "hr"}));
        elements.push(md_el(summon_i18n::text!("r2e468e45b9")));
        let shown = names.len().min(MODEL_PANEL_BTN_CAP);
        for row in names[..shown].chunks(MODEL_PANEL_BTNS_PER_ROW) {
            let btns: Vec<serde_json::Value> = row
                .iter()
                .map(|target| {
                    let mut value = serde_json::json!({
                        "action": "fleet_refresh",
                        "agent": target.alias.as_str(),
                    });
                    // 远端目标必须带机器名:两台机器可以有同名 agent,只凭 alias 会点到另一台去。
                    if let Some(remote) = &target.remote {
                        value["remote"] = serde_json::Value::String(remote.clone());
                    }
                    cb_btn(&target.label(), "default", value)
                })
                .collect();
            elements.push(button_row(btns));
        }
        if names.len() > shown {
            elements.push(json_grey_line(&summon_i18n::format!(
                "re1cce256ab",
                names.len() - shown
            )));
        }
    }
    elements.push(serde_json::json!({"tag": "hr"}));
    elements.push(button_row(buttons));
    closeable_card(serde_json::json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// 待处理区的一条:左边整行链接,右边「标记已处理」。
fn fleet_waiting_row(item: &crate::waiting::WaitingStatus) -> serde_json::Value {
    let button = cb_btn(
        summon_i18n::text!("r83c5d13412"),
        "default",
        serde_json::json!({
            "action": "fleet_waiting_handled",
            "agent": item.alias,
            "tag": item.tag,
            "reply": item.reply_message_id,
        }),
    );
    serde_json::json!({
        "tag": "column_set",
        "flex_mode": "none",
        "horizontal_spacing": "8px",
        "columns": [
            {"tag": "column", "width": "weighted", "weight": 1, "vertical_align": "center",
             "elements": [md_el(&crate::fleet_card::fleet_waiting_line(item))]},
            {"tag": "column", "width": "auto", "vertical_align": "center",
             "elements": [button]},
        ],
    })
}

/// 卡片里一行灰字说明(截断提示这类"次要但不能省"的话)。
fn json_grey_line(text: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "markdown",
        "content": format!("<font color='grey'>{}</font>", escape_lark_md(text)),
    })
}

/// 某个 agent 的**会话操作页**:每条会话一行描述 + 一排退出按钮。
///
/// 这是管家(steward)唯一一张带破坏性动作的卡。三条约束刻在渲染里,别在别处放宽:
///
/// 1. **每个按钮都带二次确认,确认文案里写死 agent 名 + 会话名**。跨 agent 操作最容易犯的错是
///    「点错了行」——一屏几条会话长得都差不多,而点下去停的是别人正在用的会话。确认框把
///    「你要停的是谁的哪条」摆到眼前,是这里唯一能挡住手滑的东西。
/// 2. **`chat_id` 缺失的会话不渲按钮**,只留一行灰字说明。tag 是 chat_id 的哈希、不可逆,
///    没有 chat_id 就没法把动作路由回目标 agent —— 渲一个点了没反应的按钮比不渲更糟。
/// 3. **列表截断到 [`crate::fleet_card::MAX_CHATS_PER_OPS_PAGE`] 并明示**,不静默吞掉。
///
/// 真正的鉴权在 daemon 侧(管家自己的 operate 闸 + 目标 agent 的 operate 闸,两道都过);
/// 卡片只负责别把不能操作的东西渲成能点的样子。
/// `notice` = 上一次操作的结果提示(成功/被拒/失败),渲在页首。**结果和列表同处一张卡**:
/// 点完退出就地看到发生了什么,不用再翻消息;下一次刷新自然把它清掉。
pub fn card_fleet_sessions(agent: &crate::fleet::AgentStatus, notice: Option<&str>) -> String {
    use crate::fleet_card::{fleet_ops_chat_line, fleet_ops_header, MAX_CHATS_PER_OPS_PAGE};
    let mut elements: Vec<serde_json::Value> = Vec::new();
    if let Some(notice) = notice.map(str::trim).filter(|s| !s.is_empty()) {
        elements.push(md_el(notice));
    }
    elements.push(md_el(&fleet_ops_header(agent)));

    if agent.chats.is_empty() {
        elements.push(md_el(summon_i18n::text!("r969eafa4d5")));
    }
    for (i, chat) in agent.chats.iter().take(MAX_CHATS_PER_OPS_PAGE).enumerate() {
        elements.push(serde_json::json!({"tag": "hr"}));
        elements.push(md_el(&fleet_ops_chat_line(i, chat)));
        let label = chat.name.as_deref().unwrap_or(&chat.tag);
        match chat
            .chat_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(chat_id) => elements.push(button_row(vec![
                cb_btn_confirm(
                    summon_i18n::text!("r498e1d59b4"),
                    "default",
                    serde_json::json!({
                        "action": "fleet_session_clean",
                        "agent": agent.alias,
                        "chat_id": chat_id,
                    }),
                    &summon_i18n::format!("r318cae58c8", agent.alias),
                ),
                cb_btn_confirm(
                    summon_i18n::text!("rdc2e173490"),
                    "danger",
                    serde_json::json!({
                        "action": "fleet_session_reset",
                        "agent": agent.alias,
                        "chat_id": chat_id,
                    }),
                    &summon_i18n::format!("r63243e95e4", agent.alias),
                ),
            ])),
            // 拿不到 chat_id = 路由不回去。诚实说明,而不是渲一个点了没反应的按钮。
            None => elements.push(json_grey_line(summon_i18n::text!("raa9c1cef7b"))),
        }
    }
    if agent.chats.len() > MAX_CHATS_PER_OPS_PAGE {
        elements.push(md_el(&summon_i18n::format!(
            "r5de98d536f",
            agent.chats.len() - MAX_CHATS_PER_OPS_PAGE
        )));
    }

    elements.push(serde_json::json!({"tag": "hr"}));
    elements.push(button_row(vec![
        cb_btn(
            summon_i18n::text!("r9da4432b05"),
            "default",
            serde_json::json!({"action": "fleet_sessions", "agent": agent.alias}),
        ),
        cb_btn(
            summon_i18n::text!("r9c33d3fc06"),
            "default",
            serde_json::json!({"action": "fleet_refresh"}),
        ),
    ]));
    closeable_card(serde_json::json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
/// 把 markdown 文本包成飞书 interactive 卡片的 `content` 字符串。
/// (飞书 im/v1/messages 的 content 字段必须是 JSON 字符串,不是嵌套对象。)
pub(crate) fn card_content(title: &str, markdown: &str) -> String {
    let card = serde_json::json!({
        "config": { "wide_screen_mode": true },
        "header": { "title": { "tag": "plain_text", "content": title } },
        "elements": [ md_el(markdown) ]
    });
    card.to_string()
}

/// 1.0 骨架的纯 markdown 卡,仅测试用,用来构造非 2.0 的输入。生产路径一律走 [`card_markdown_v2`]。
#[cfg(test)]
pub fn card_markdown(markdown: &str) -> String {
    let card = serde_json::json!({
        "config": { "wide_screen_mode": true },
        "elements": [ md_el(markdown) ]
    });
    card.to_string()
}

/// [`card_markdown`] 的已渲染形态。只接收 [`RenderedLarkMd`]，保留其中受信 `<at>`。新发的卡
/// 一律走 [`card_markdown_v2_rendered`]。
#[cfg(test)]
pub fn card_markdown_rendered(markdown: &RenderedLarkMd) -> String {
    let card = serde_json::json!({
        "config": { "wide_screen_mode": true },
        "elements": [ rendered_md_el(markdown) ]
    });
    card.to_string()
}

fn cb_btn(label: &str, ty: &str, value: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": label},
        "type": ty,
        "behaviors": [{"type": "callback", "value": value}]
    })
}

/// 打开链接并回传点击人的按钮:飞书卡片 2.0 同时生效跳转与回传交互,daemon 凭回调里的点击人
/// 激活这条链接(见 `docs/spec/2026-10-20-link-click-auth-design.md`)。
fn url_callback_btn(
    label: &str,
    ty: &str,
    url: &str,
    value: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": label},
        "type": ty,
        "behaviors": [
            {
                "type": "open_url",
                "default_url": url,
                "pc_url": url,
                "ios_url": url,
                "android_url": url
            },
            {"type": "callback", "value": value}
        ]
    })
}

/// 终端链接按钮的回调动作。
pub const TERMINAL_AUTH_ACTION: &str = "term_auth";
/// 委派任务的公开摘要卡，附带 Owner 专用详情入口。
pub fn card_delegate_summary(summary: &str, task_id: &str) -> String {
    let mut card: serde_json::Value =
        serde_json::from_str(&card_markdown_v2(summary)).expect("markdown card is JSON");
    card["body"]["elements"].as_array_mut().expect("card body").push(serde_json::json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": summon_i18n::text!("r9e4c60f011")},
        "type": "default",
        "behaviors": [{"type": "callback", "value": {"action": "delegate_details", "task_id": task_id}}]
    }));
    card.to_string()
}

/// 本地文件按钮的回调动作。
pub const PREVIEW_OPEN_ACTION: &str = "preview_open";
/// 回复卡末尾最多列几个文件按钮;多出来的只写个数。
pub const PREVIEW_BUTTONS_MAX: usize = 8;
const PREVIEW_FILES_ELEMENT_ID: &str = "preview_files";
const PREVIEW_FILES_MORE_ELEMENT_ID: &str = "preview_files_more";

/// 回复里提到的一个可预览本地文件。`token` 只用来定位文件,打开链接由点击回调私聊给点的人。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewButton {
    pub label: String,
    pub token: String,
    pub is_dir: bool,
}

/// 在 2.0 卡片末尾挂上本地文件按钮(替换已有的那一组)。没有按钮或不是 2.0 卡时原样返回 ——
/// 1.0 卡不支持按钮的 `behaviors`。挂上按钮会超出单卡总预算时从末尾少挂几个,一个也放不下时
/// 只留一句个数说明,连它也放不下就只发正文:正文先于按钮,不能因为按钮让整条回复发不出去。
pub fn card_with_preview_buttons(card_json: &str, buttons: &[PreviewButton]) -> Option<String> {
    let mut shown = buttons.len().min(PREVIEW_BUTTONS_MAX);
    loop {
        let card = attach_preview_buttons(card_json, &buttons[..shown], buttons.len())?;
        if card.len() <= CARD_TOTAL_SAFE_LIMIT {
            return Some(card);
        }
        if shown == 0 {
            return attach_preview_buttons(card_json, &[], 0);
        }
        shown -= 1;
    }
}

/// 挂 `shown` 这几个按钮;`total` 是回复里一共认出的文件数,多出来的写个数。
fn attach_preview_buttons(
    card_json: &str,
    shown: &[PreviewButton],
    total: usize,
) -> Option<String> {
    let mut card: serde_json::Value = serde_json::from_str(card_json).ok()?;
    let Some(elements) = card
        .get_mut("body")
        .and_then(|body| body.get_mut("elements"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Some(card_json.to_string());
    };
    elements.retain(|el| {
        !matches!(
            el.get("element_id").and_then(|v| v.as_str()),
            Some(PREVIEW_FILES_ELEMENT_ID) | Some(PREVIEW_FILES_MORE_ELEMENT_ID)
        )
    });
    if total == 0 {
        return Some(card.to_string());
    }
    if shown.is_empty() {
        let mut note = json_grey_line(&summon_i18n::format!("r6f7a1f301b"));
        note["element_id"] = serde_json::json!(PREVIEW_FILES_MORE_ELEMENT_ID);
        elements.push(note);
        return Some(card.to_string());
    }
    let columns: Vec<serde_json::Value> = shown
        .iter()
        .map(|button| {
            let icon = if button.is_dir { "📁" } else { "📄" };
            serde_json::json!({
                "tag": "column", "width": "auto",
                "elements": [cb_btn(
                    &format!("{icon} {}", button.label),
                    "default",
                    serde_json::json!({"action": PREVIEW_OPEN_ACTION, "token": button.token}),
                )]
            })
        })
        .collect();
    elements.push(serde_json::json!({
        "tag": "column_set", "element_id": PREVIEW_FILES_ELEMENT_ID,
        "flex_mode": "flow", "horizontal_spacing": "8px",
        "columns": columns
    }));
    if total > shown.len() {
        let mut more = json_grey_line(&summon_i18n::format!("rb6b0ce8403", total - shown.len()));
        more["element_id"] = serde_json::json!(PREVIEW_FILES_MORE_ELEMENT_ID);
        elements.push(more);
    }
    Some(card.to_string())
}

/// 只打开链接的按钮,不回传点击。
fn url_btn(label: &str, ty: &str, url: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": label},
        "type": ty,
        "behaviors": [{
            "type": "open_url",
            "default_url": url,
            "pc_url": url,
            "ios_url": url,
            "android_url": url
        }]
    })
}

/// 私聊给点了文件按钮的人的打开卡:链接只发给他本人,打开即看,`ttl_secs` 秒内有效。
pub fn card_preview_link(label: &str, is_dir: bool, url: &str, ttl_secs: u64) -> String {
    use serde_json::json;
    let icon = if is_dir { "📁" } else { "📄" };
    let elements = vec![
        json!({
            "tag": "markdown",
            "content": summon_i18n::format!(
                "r1232b0d781",
                escape_lark_md(label),
                human_duration(ttl_secs),
            )
        }),
        button_row(vec![url_btn(
            summon_i18n::text!("rc771248e51"),
            "primary",
            url,
        )]),
    ];
    json!({
        "schema": "2.0",
        "config": {"summary": {"content": summon_i18n::format!("r22442399e5")}},
        "body": {"elements": elements}
    })
    .to_string()
}

/// 置灰不可点的按钮(已恢复/已操作的终态反馈)。无 behaviors=点了无反应,配合 disabled=true 视觉置灰。
fn cb_btn_disabled(label: &str, ty: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": label},
        "type": ty,
        "disabled": true,
        "behaviors": []
    })
}

// 带二次确认弹窗的回调按钮(破坏性动作用):点了先弹 confirm,确认才触发回调。
fn cb_btn_confirm(
    label: &str,
    ty: &str,
    value: serde_json::Value,
    confirm_text: &str,
) -> serde_json::Value {
    serde_json::json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": label},
        "type": ty,
        "behaviors": [{"type": "callback", "value": value}],
        "confirm": {
            "title": {"tag": "plain_text", "content": summon_i18n::text!("rf8347f9a9d")},
            "text": {"tag": "plain_text", "content": confirm_text}
        }
    })
}

// 一排按钮塞进分栏:每个按钮单独一列(width auto),PC 横排;
// flex_mode=stretch 让窄屏/移动端把每列拉伸成整宽行、上下堆叠,长文案不再被压。
fn button_row(buttons: Vec<serde_json::Value>) -> serde_json::Value {
    let columns: Vec<serde_json::Value> = buttons
        .into_iter()
        .map(|b| serde_json::json!({"tag": "column", "width": "auto", "elements": [b]}))
        .collect();
    serde_json::json!({
        "tag": "column_set",
        "flex_mode": "stretch",
        "horizontal_spacing": "8px",
        "columns": columns
    })
}

fn human_duration(secs: u64) -> String {
    match secs {
        s if s % 86_400 == 0 => summon_i18n::format!("r758b17ae26", s / 86_400),
        s if s % 3_600 == 0 => summon_i18n::format!("r8ba62c6229", s / 3_600),
        s if s % 60 == 0 => summon_i18n::format!("ra8442c80e9", s / 60),
        s => summon_i18n::format!("r91a4473b5c"),
    }
}

/// 空闲提示卡片:说明触发原因,只提供当前会话生命周期按钮。
/// 空闲卡按钮统一带 `origin:idle` + `idle_secs`:收尾(点退出/保活/compact)时据此复现空闲布局
/// 与时长,与 `/session` 面板(无 origin)区分开——否则收尾路径分不清两种卡,会把空闲卡错画成
/// 带模型/工作目录的面板布局(见 [`card_idle_closed`])。
fn idle_action_value(
    action: &str,
    chat_tag: &str,
    idle_ttl_secs: u64,
    context: Option<&crate::context_budget::ContextBudget>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "action": action, "chat": chat_tag, "origin": "idle", "idle_secs": idle_ttl_secs
    });
    if let Some(context) = context {
        insert_context_fields(&mut value, context);
    }
    value
}

/// 把上下文预算写进按钮值(`ctx_*` 三个键,缺项不写),点完收尾时据此重绘同样的读数。
fn insert_context_fields(
    value: &mut serde_json::Value,
    context: &crate::context_budget::ContextBudget,
) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if let Some(used) = context.used_tokens {
        object.insert("ctx_used_tokens".into(), used.into());
    }
    if let Some(window) = context.window_tokens {
        object.insert("ctx_window_tokens".into(), window.into());
    }
    if let Some(pct) = context.reported_pct {
        object.insert("ctx_reported_pct".into(), pct.into());
    }
}

/// 空闲提醒卡的元素列表(摘要 + 3 个生命周期按钮)。抽出来给 [`card_idle_prompt`](可点态)与
/// [`card_idle_closed`](禁用终态)共用,保证两者布局一致。
fn idle_prompt_elements(
    chat_tag: &str,
    idle_ttl_secs: u64,
    context: Option<&crate::context_budget::ContextBudget>,
) -> Vec<serde_json::Value> {
    let idle_for = human_duration(idle_ttl_secs);
    let context_line = context
        .and_then(crate::context_budget::render_usage)
        .map(|value| summon_i18n::format!("rf28f1f714e"))
        .unwrap_or_default();
    let summary = summon_i18n::format!("re606292ed0");
    vec![
        serde_json::json!({"tag": "markdown", "content": summary}),
        button_row(vec![
            cb_btn_confirm(
                summon_i18n::text!("rdc2e173490"),
                "danger",
                idle_action_value("reset", chat_tag, idle_ttl_secs, context),
                summon_i18n::text!("r841fc7a89f"),
            ),
            cb_btn(
                summon_i18n::text!("r498e1d59b4"),
                "default",
                idle_action_value("clean", chat_tag, idle_ttl_secs, context),
            ),
            cb_btn(
                "compact",
                "default",
                idle_action_value("compact", chat_tag, idle_ttl_secs, context),
            ),
        ]),
    ]
}

pub fn card_idle_prompt(chat_tag: &str, idle_ttl_secs: u64) -> String {
    card_idle_prompt_with_context(chat_tag, idle_ttl_secs, None)
}

pub fn card_idle_prompt_with_context(
    chat_tag: &str,
    idle_ttl_secs: u64,
    context: Option<&crate::context_budget::ContextBudget>,
) -> String {
    let mut elements = idle_prompt_elements(chat_tag, idle_ttl_secs, context);
    elements.push(idle_close_row(chat_tag, idle_ttl_secs, context));
    closeable_card(serde_json::json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

fn idle_close_row(
    chat_tag: &str,
    idle_ttl_secs: u64,
    context: Option<&crate::context_budget::ContextBudget>,
) -> serde_json::Value {
    button_row(vec![cb_btn(
        summon_i18n::text!("r5242df9b30"),
        "default",
        idle_action_value("card_close", chat_tag, idle_ttl_secs, context),
    )])
}

/// 空闲提醒卡被点后的收尾原因 → footer 文案。reset/clean 是退出(复用面板退出话术);
/// compact/keepalive 会话仍在,据实说明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleClosedReason {
    Reset,
    Clean,
    Compact,
    KeepAlive,
}

impl IdleClosedReason {
    /// 「你已交互」footer 的正文(终态卡底部)。卡片刷新失败退回普通回执时亦复用。
    pub fn footer_tail(self) -> &'static str {
        match self {
            IdleClosedReason::Reset => {
                summon_i18n::text!("ra0d93e92ae")
            }
            IdleClosedReason::Clean => summon_i18n::text!("reb68f88592"),
            IdleClosedReason::Compact => summon_i18n::text!("r65320bb32b"),
            IdleClosedReason::KeepAlive => summon_i18n::text!("rcbbaadcda2"),
        }
    }

    /// 会话列表预览里的结果标签。
    fn feed_label(self) -> &'static str {
        match self {
            IdleClosedReason::Reset => summon_i18n::text!("r82395d4230"),
            IdleClosedReason::Clean => summon_i18n::text!("r899df7e244"),
            IdleClosedReason::Compact => summon_i18n::text!("r77eebae8e8"),
            IdleClosedReason::KeepAlive => summon_i18n::text!("rafce9157af"),
        }
    }
}

/// 空闲提醒卡被点后的终态:**保留空闲布局原貌**(3 个生命周期按钮置灰),底部 hr + 「你已交互」footer。
/// 与 [`card_session_closed`](/session 面板布局)区分——空闲卡收尾绝不能走面板渲染,否则布局漂移
/// (空闲卡被错画成带模型/工作目录的面板)。必须 schema 2.0(与原卡同 schema,PATCH 不可换)。
pub fn card_idle_closed(chat_tag: &str, idle_ttl_secs: u64, reason: IdleClosedReason) -> String {
    card_idle_closed_with_context(chat_tag, idle_ttl_secs, reason, None)
}

pub fn card_idle_closed_with_context(
    chat_tag: &str,
    idle_ttl_secs: u64,
    reason: IdleClosedReason,
    context: Option<&crate::context_budget::ContextBudget>,
) -> String {
    use serde_json::json;
    let mut elements = idle_prompt_elements(chat_tag, idle_ttl_secs, context);
    elements.iter_mut().for_each(disable_interactive);
    let footer = summon_i18n::format!("r98d33a9049", reason.footer_tail());
    elements.push(json!({"tag": "hr"}));
    elements.push(json!({"tag": "markdown", "content": footer}));
    elements.push(idle_close_row(chat_tag, idle_ttl_secs, context));
    with_outcome_feed_summary(
        json!({"schema": "2.0", "body": {"elements": elements}}),
        reason.feed_label(),
    )
}

/// 上下文过重提醒卡(owner 2026-10-15)的渲染参数。按钮值把它原样带回来,点完按它重绘同一张卡、
/// 只把控件置灰 —— 卡片本身不落盘。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextWarnCard {
    /// 跨过的那一档(见 [`crate::context_budget::WARN_STEPS`])。
    pub step: u8,
    pub budget: crate::context_budget::ContextBudget,
    pub remedy: crate::context_budget::ContextRemedy,
}

/// Handoff 输入框的控件名;提交时内容在 `form_value.<它>`。
pub const CTX_HANDOFF_NOTE_FIELD: &str = "ctx_handoff_note";

const CTX_WARN_ORIGIN: &str = "ctx_warn";

impl ContextWarnCard {
    /// `session_id` 是发卡时这个 chat 的原生会话 id:点击时据它确认会话没换过,旧会话的卡不能
    /// 作用到新会话上。取不到就不写。
    fn action_value(&self, action: &str, session_id: Option<&str>) -> serde_json::Value {
        let mut value = serde_json::json!({
            "action": action,
            "origin": CTX_WARN_ORIGIN,
            "step": self.step,
            "remedy": self.remedy.wire(),
        });
        insert_context_fields(&mut value, &self.budget);
        if let (Some(sid), Some(object)) = (
            session_id.filter(|sid| !sid.is_empty()),
            value.as_object_mut(),
        ) {
            object.insert("sid".into(), sid.into());
        }
        value
    }

    /// 从按钮回调的 `value` 读回卡片参数。不是这种卡、或字段读不全 → None。
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        if value.get("origin")?.as_str()? != CTX_WARN_ORIGIN {
            return None;
        }
        let step = u8::try_from(value.get("step")?.as_u64()?).ok()?;
        let remedy =
            crate::context_budget::ContextRemedy::from_wire(value.get("remedy")?.as_str()?)?;
        let budget = crate::context_budget::ContextBudget {
            used_tokens: value.get("ctx_used_tokens").and_then(|v| v.as_u64()),
            window_tokens: value.get("ctx_window_tokens").and_then(|v| v.as_u64()),
            reported_pct: value
                .get("ctx_reported_pct")
                .and_then(|v| v.as_u64())
                .map(|pct| pct.min(100) as u8),
        };
        Some(Self {
            step,
            budget,
            remedy,
        })
    }
}

/// 上下文提醒卡按钮值里的原生会话 id(发卡时写入);没有 → None。
pub fn context_warn_session(value: &serde_json::Value) -> Option<&str> {
    value.get("sid")?.as_str().filter(|sid| !sid.is_empty())
}

/// 正文 + 按钮:「接手的规划重点」输入框 + Handoff 与 Ignore 都有,能压缩的会话另有 Compact。
/// Handoff 必须和输入框在同一个表单里,提交时才带得上输入框内容;它写完交接后按卡上的去向原地压缩
/// 或归档当前会话,与「📦 归档」一样先弹确认框(owner 2026-10-15)。
fn context_warning_elements(
    card: &ContextWarnCard,
    session_id: Option<&str>,
) -> Vec<serde_json::Value> {
    use crate::context_budget::{warning_text, ContextRemedy};
    use serde_json::json;
    let mut elements = vec![json!({
        "tag": "markdown",
        "content": warning_text(card.step, &card.budget, card.remedy),
    })];
    let ignore = cb_btn(
        "Ignore",
        "default",
        card.action_value("ctx_ignore", session_id),
    );
    let compact_in_place = card.remedy == ContextRemedy::Compact;
    elements.push(json!({
        "tag": "form",
        "name": "ctx_handoff_form",
        "elements": [
            {
                "tag": "input",
                "name": CTX_HANDOFF_NOTE_FIELD,
                "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("r76631b5bb2")},
            },
            {
                "tag": "button",
                "text": {"tag": "plain_text", "content": "Handoff"},
                "type": if compact_in_place { "default" } else { "primary" },
                "behaviors": [{
                    "type": "callback",
                    "value": card.action_value("ctx_handoff", session_id),
                }],
                "form_action_type": "submit",
                "name": "ctx_handoff_submit",
                "confirm": {
                    "title": {"tag": "plain_text", "content": summon_i18n::text!("r9ab5de5816")},
                    "text": {
                        "tag": "plain_text",
                        "content": if compact_in_place {
                            summon_i18n::text!("r0bec20eec7")
                        } else {
                            summon_i18n::text!("rc370d44805")
                        },
                    },
                },
            },
        ],
    }));
    let mut row = Vec::new();
    if compact_in_place {
        row.push(cb_btn(
            "Compact",
            "primary",
            card.action_value("ctx_compact", session_id),
        ));
    }
    row.push(ignore);
    elements.push(button_row(row));
    elements
}

pub fn card_context_warning(card: &ContextWarnCard, session_id: Option<&str>) -> String {
    closeable_card(
        serde_json::json!({
            "schema": "2.0",
            "body": {"elements": context_warning_elements(card, session_id)},
        })
        .to_string(),
    )
}

/// 上下文提醒卡被点之后的结果,写在定格卡的底部。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextWarnOutcome {
    /// 点了 Compact,压缩已交给会话,还没跑完。
    Compacting,
    Compacted,
    /// 压缩没完成;原因另有一条消息说明。
    CompactFailed,
    /// 证得发卡之后会话已经换过(归档 / 重置 / 新会话),按钮不再作用。
    Stale,
    /// 证不了这张卡还是当前会话的:卡上或盘上缺会话 id,对不上。交接因此不执行。
    Unverified,
    /// 交接已发起;`note` 是输入框里填的接手重点。
    HandoffStarted {
        note: String,
    },
    /// 这张卡发起的交接已经处理过:那一轮跑完,或因 Bridge / 遥控 / agent 停用被跳过,结果另有说明。
    HandoffDone,
    Ignored,
}

impl ContextWarnOutcome {
    /// 结果说明,写在定格卡底部;卡片定格失败时作为普通消息发出。
    pub fn text(&self, card: &ContextWarnCard) -> String {
        match self {
            Self::Compacting => summon_i18n::text!("r3dcf35996c").to_string(),
            Self::Compacted => summon_i18n::text!("rcf241b9472").to_string(),
            Self::CompactFailed => summon_i18n::text!("r3f993c2a63").to_string(),
            Self::Stale => summon_i18n::text!("r32d8fe17c6").to_string(),
            Self::Unverified => summon_i18n::text!("rdadb3d6cf1").to_string(),
            Self::HandoffDone => summon_i18n::text!("r9469730c2a").to_string(),
            Self::HandoffStarted { note } => {
                let started = match card.remedy {
                    crate::context_budget::ContextRemedy::Compact => {
                        summon_i18n::text!("r7c0e247eae")
                    }
                    crate::context_budget::ContextRemedy::Handoff => {
                        summon_i18n::text!("r5d475b44de")
                    }
                };
                match note.trim() {
                    "" => started.to_string(),
                    note => summon_i18n::format!("r6cde2dff3b", escape_lark_md(note)),
                }
            }
            Self::Ignored => summon_i18n::format!(
                "r5d95e11642",
                crate::context_budget::next_warning_hint(card.step)
            ),
        }
    }

    fn feed_label(&self) -> &'static str {
        match self {
            Self::Compacting => summon_i18n::text!("rdc5967ac17"),
            Self::Compacted => summon_i18n::text!("r77eebae8e8"),
            Self::CompactFailed => summon_i18n::text!("r4d04a38d38"),
            Self::Stale => summon_i18n::text!("r2fe5a8d0ee"),
            Self::Unverified => summon_i18n::text!("r231fe22042"),
            Self::HandoffDone => summon_i18n::text!("r59f6c83692"),
            Self::HandoffStarted { .. } => summon_i18n::text!("r20eec8d5bb"),
            Self::Ignored => summon_i18n::text!("rc9bb70bcaa"),
        }
    }
}

/// 上下文提醒卡被点之后的定格态:保留原布局(含输入框),控件全部置灰,底部写结果。与原卡同为
/// schema 2.0(PATCH 不能换 schema)。
pub fn card_context_warning_closed(card: &ContextWarnCard, outcome: &ContextWarnOutcome) -> String {
    use serde_json::json;
    let mut elements = context_warning_elements(card, None);
    elements.iter_mut().for_each(disable_interactive);
    elements.push(json!({"tag": "hr"}));
    elements.push(json!({"tag": "markdown", "content": outcome.text(card)}));
    with_outcome_feed_summary(
        json!({"schema": "2.0", "body": {"elements": elements}}),
        outcome.feed_label(),
    )
}

/// 压缩完成后的上下文长度卡。`compacted` 是从转录读到的压缩结果;没读到压缩边界时如实说
/// 无法确认,不写数字。
pub fn card_context_usage(compacted: Option<&crate::context_budget::CompactedContext>) -> String {
    use crate::context_budget::{render_tokens, usage_detail};
    let budget = compacted.and_then(crate::context_budget::CompactedContext::budget);
    let content = match (compacted, &budget) {
        (Some(compacted), Some(budget)) => summon_i18n::format!(
            "r3dcc1207d6",
            usage_detail(budget),
            render_tokens(compacted.summary_tokens),
            render_tokens(compacted.base_tokens.unwrap_or_default()),
        ),
        (Some(compacted), None) => {
            summon_i18n::format!("r29a8cfbbec", render_tokens(compacted.summary_tokens))
        }
        (None, _) => summon_i18n::text!("r42a47fa048").to_string(),
    };
    let summary = match budget.as_ref().and_then(|budget| budget.used_pct()) {
        Some(pct) => summon_i18n::format!("r63ca13087f"),
        None => summon_i18n::text!("r7af57fd1b2").to_string(),
    };
    card_with_feed_summary(
        &serde_json::json!({
            "schema": "2.0",
            "body": {"elements": [{"tag": "markdown", "content": content}]},
        })
        .to_string(),
        &summary,
    )
}

/// 过程回复的独立补充载体，正文保持折叠。
pub fn card_process_supplement(body: &str) -> String {
    serde_json::json!({
        "schema": "2.0",
        "config": {"summary": {"content": summon_i18n::text!("r2d18503ec7")}},
        "body": {"elements": [collapsible_panel_unclipped(
            summon_i18n::text!("r5dc5d4b79c"), false, &escape_lark_md(body)
        )]},
    })
    .to_string()
}

/// 交织 live renderer 的调用方会用整卡真实 JSON 大小做拆卡，这里不能再对
/// **已消毒**文本静默裁剪：`<字母` 插零宽空格后字节数大于原文，按 20KB 裁会让
/// 上层提交全段 cursor，实际少送一截。
fn collapsible_panel_unclipped(title: &str, expanded: bool, body: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "collapsible_panel",
        "expanded": expanded,
        "header": {"title": {"tag": "markdown", "content": title}},
        // body 已由调用方转义好(有的还外包了 `<font>` 模板),这里不能再转——否则把模板也转掉。
        "elements": [{"tag": "markdown", "content": body}],
    })
}

/// 把进行中形状渲成一段多行 markdown。未启用增强 / 无步 → None(卡片一行不多)。
///
/// 渲染取舍:已完成步用 `✓` + 灰字(压低存在感,它们只是背景),在跑那步用 `⟳` + 正常字色 + 计时
/// (这是用户此刻最需要的一条)。thinking 只有时长可显 —— 正文已被上游抹掉,这里不编造。
fn progress_trail_line(progress: &crate::progress::TurnProgress) -> Option<String> {
    use crate::progress::{humanize_secs, ProgressStep};
    if !progress.enabled() || progress.steps().is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    for step in progress.steps() {
        match step {
            ProgressStep::Thinking { secs } => {
                lines.push(summon_i18n::format!("rd9b44b6e77", humanize_secs(*secs)));
            }
            ProgressStep::Tool {
                label, secs, done, ..
            } => {
                let label = escape_lark_md(label);
                if *done {
                    lines.push(format!(
                        "<font color='grey'>✓ {label}  {}</font>",
                        humanize_secs(*secs)
                    ));
                } else {
                    lines.push(format!("⟳ {label}  {}", humanize_secs(*secs)));
                }
            }
        }
    }
    Some(lines.join("\n"))
}

/// turn 完成后把「过程卡」(流式卡)**定格**的 card1:顶 `✅ 已完成` + **恰好一个**默认收起的折叠面板
/// (标题 `📋 过程`),把**全部交织段按到达顺序**拼进这一个面板里——**无 💭/📄 标记**,只靠字色区分:
/// thinking 用偏淡(灰)字色 `<font color='grey'>…</font>`、reply 正常字色直显,相邻块以空行 + `---` 分隔。
/// 最终回复另发 [`card_turn_summary`] 直显(card2),两卡不重复(owner 反馈)。schema 2.0(与流式卡同,
/// PATCH 不换)。无段时只显「✅ 已完成」(降级单卡的判定在调用方,这里不会被空段命中)。
/// 把多行文本**逐行**用灰字色包裹。飞书 markdown 的 `<font>` 不跨块级换行——整段只包一个会
/// 只染第一行(owner 反馈「只有第一行/标题偏淡」即此),逐行包才能让 thinking 全文都偏淡。空行留空。
fn dim_lines(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                // 先转义行内容(外部文本),再包 `<font>` 模板 —— 顺序反了会把模板一起转掉。
                format!("<font color='grey'>{}</font>", escape_lark_md(l))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 去掉**末尾那个 Reply 段**——它由总结卡(card2)直显,留在过程卡里就是逐字重复(owner 实测截图:
/// 过程面板末尾那段与下面的总结卡一模一样)。末尾不是 Reply(以 thinking 收尾)则原样返回。
///
/// 只砍末尾一个:中途的 Reply 段是交织过程的一部分,总结卡不承载它们,砍掉会丢信息。
pub fn segments_without_trailing_reply(
    segs: &[crate::segment::TurnSegment],
) -> &[crate::segment::TurnSegment] {
    match segs.last() {
        Some(crate::segment::TurnSegment::Reply(_)) => &segs[..segs.len() - 1],
        _ => segs,
    }
}

/// 收尾折叠面板的正文。段内容在前、工具轨迹在后,都没有 → `None`(调用方降级成只有标题的单元素卡)。
///
/// **轨迹为什么要进收尾卡。** 这里破的是 `progress` 模块「产物只进 live 卡」那条边界,理由是原设计
/// 漏了一种形状:自家 claude 的 thinking 正文 100% 为空(本机真机 1533/1533),一轮如果又没写中间
/// 说明,`segments` 去掉末尾终态回复后就是空的 —— 判据落空、退回单卡、终态回复原地覆盖 live 卡,
/// **飞书不发新消息,人就收不到通知**(owner 2026-08-13 实测:本会话 232 轮里 71 轮是这形状)。
/// 折叠区里放什么才不算噪音也有了答案:轨迹的 label 优先取工具 `description`(claude 给的人话摘要),
/// 那本就是「这一步在干什么」的自述,而不是「思考 12s」那种占位。默认收起,不占版面。
///
/// **只对「思考不可见」那一档生效**:`progress_trail_line` 内部看 `progress.enabled()`,有明文
/// thinking 的模型(codex/cursor)恒返回 `None` —— 它们的过程已由段渲出来,这里逐字节等同旧行为。
fn turn_done_body(
    segs: &[crate::segment::TurnSegment],
    progress: Option<&crate::progress::TurnProgress>,
) -> Option<String> {
    use crate::segment::TurnSegment;
    let mut blocks: Vec<String> = segs
        .iter()
        .map(|seg| match seg {
            TurnSegment::Thinking(text) => dim_lines(text),
            TurnSegment::Reply(text) => lark_md_body(text),
        })
        .collect();
    // 轨迹已是渲好的 markdown(自带灰字模板),不能再过 escape/dim,否则模板被转成字面量。
    blocks.extend(progress.and_then(progress_trail_line));
    if blocks.is_empty() {
        return None;
    }
    Some(blocks.join("\n\n---\n\n"))
}

/// 收尾折叠区**会不会有内容**。判据与 [`turn_done_body`] 同源:一旦两边漂移,要么渲出一张空的
/// 「✅ 已完成」,要么把本该折叠的过程整个丢掉,而且都是静默的。
pub fn has_turn_done_body(
    segs: &[crate::segment::TurnSegment],
    progress: Option<&crate::progress::TurnProgress>,
) -> bool {
    turn_done_body(segs, progress).is_some()
}

/// Unix 秒 → 本机时区的 `HH:MM`。与额度耗尽卡预填定时器用的是同一个时区。
fn local_hhmm(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|at| at.with_timezone(&chrono::Local).format("%H:%M").to_string())
}

/// Unix 秒 → 本机时区的 `M/D HH:MM`。周窗口的重置点在几天之后,只写时刻分不出是哪一天。
fn local_md_hhmm(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0).map(|at| {
        at.with_timezone(&chrono::Local)
            .format("%-m/%-d %H:%M")
            .to_string()
    })
}

/// 回复卡底部的**实时角标**数据:CLI 类型 / 生效模型 / 思考档位 / 上下文已用 / 剩余额度。各项
/// 各自可缺(取不到就不显示该项),渲染成一行
/// `claude · Opus 5 · xhigh · 350k/1.0M ████░░░░░░ 35% · 5h 88% ↻17:30 7d 87% ↻10/20 09:00`。
///
/// Cursor 的上下文项取决于 ACP 是否发送 `usage_update`，额度仍无统一口径；全部缺 → [`Self::render`]
/// 返回 `None`,上下文行退回纯时间戳、逐字节等同没有角标的老路径。渲染出的文本是**未转义原文**,
/// 由卡片层(`card_turn_summary` 的 `escape_lark_md`)统一转义,与 `card_turn_summary_pieces`
/// 处置 context 的做法一致。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnBadge {
    /// CLI 类型(`agent.cli`,稳定)。空串 = 不显示这一项。
    pub cli: String,
    /// 生效模型名(codex:本轮 model;cursor:set_model 后的 id;取不到用 agent 配置值)。
    pub model: Option<String>,
    /// 长窗额度**剩余**百分比 0..=100。**全 CLI 同一口径**——codex 由 app-server rate-limit 直接给
    /// (周额度),claude 由 statusLine 的「已用」在采集侧翻成「剩余」(周额度),cursor 由主动查询的
    /// 个人额度在采集侧换算(计费周期)。
    ///
    /// **口径统一在采集侧、不在渲染侧**:字段名自带 `left` 语义,任何往这里塞「已用」的写法读代码
    /// 时就露馅;所有窗口都只显示剩余量,标签只区分窗口,用户不必再分辨哪个 CLI 是哪种口径
    /// (owner 2026-08-10)。
    pub quota_left_pct: Option<u8>,
    /// 长窗的重置时刻(Unix 秒)。只在标签是 `7d` 时渲染,写成本机时区的 `M/D HH:MM` 接在百分比
    /// 后面;取不到就不显示。
    pub quota_resets_at: Option<i64>,
    /// 5 小时窗口额度**剩余**百分比 0..=100,口径同 `quota_left_pct`。只有
    /// [`crate::driver::CliDriver::has_five_hour_and_weekly_quota`]
    /// 的 CLI 有这个窗口,其余恒为 None。
    pub quota_5h_left_pct: Option<u8>,
    /// 5 小时窗口的重置时刻(Unix 秒),渲染成本机时区的 `HH:MM` 接在百分比后面;取不到就不显示。
    pub quota_5h_resets_at: Option<i64>,
    /// 是否渲染额度项。**默认 false(关)**:额度是敏感面,群卡默认不露;由 `/session` 面板
    /// 「额度显示」开关按会话粒度打开(owner 2026-08-10)。bool 的 Default 就是 false ——
    /// 任何忘记设置的构造路径自动落在「不显示」,失败方向与默认一致。
    /// 采集(`quota_left_pct`)不受它影响:开关只管渲染,开了立即有值可显。
    pub show_quota: bool,
    /// 该 Codex agent 已证明与 daemon rollout watcher 使用同一默认账号；仅供收尾刷新选择
    /// 正确的内存分桶，不参与卡片渲染。
    pub merge_default_codex_quota: bool,
    /// 思考档位(如 `xhigh`)。claude 取 statusLine 的 `effort.level`(实时),其余取配置里会被下发的值
    /// (本会话覆盖 > agent.yaml);没配、或 CLI 没有独立 effort 轴(cursor 烤进模型名)→ None。
    pub effort: Option<String>,
    /// 是否**不**渲染档位项。分级开关 `badge.effort` 的取反,**出厂开**(见 `messaging.default.yaml`)。
    ///
    /// 方向与 `show_quota` 相反,遵循的却是同一条原则:`bool` 的 `Default`(false)必须落在
    /// **出厂默认**那一侧 —— 额度出厂关故存「显示」,档位出厂开故存「隐藏」。这样任何忘记赋值的
    /// 构造路径都自动与出厂默认一致,不会静默偏离配置说的话。
    ///
    /// 与 `show_quota` 的另一处差别是生效时机:这一项走分级开关的「一轮一快照」契约,轮中途改了
    /// 要下一轮才生效(回执文案即如此承诺),所以收尾**不重读盘**,照用轮初种子里的值。
    pub hide_effort: bool,
    /// 是否**不**渲染 CLI 类型项。分级开关 `badge.cli` 的取反,出厂开;方向与生效时机同 `hide_effort`。
    pub hide_cli: bool,
    /// 是否**不**渲染模型项。分级开关 `badge.model` 的取反,出厂开;方向与生效时机同 `hide_effort`。
    pub hide_model: bool,
    /// 是否**不**渲染上下文用量项。分级开关 `badge.ctx` 的取反,出厂开;方向与生效时机同
    /// `hide_effort`。**只管角标这一行**:`ctx` 照常填,上下文过重提醒卡与 `context_remedy` 不看它。
    pub hide_ctx: bool,
    /// 本轮的上下文预算(占用 / 窗口上限)。渲染成进度条,见 [`Self::render`]。
    ///
    /// 三个 backend 走同一个 [`ContextBudget`]：Claude 取 statusLine，Codex 取
    /// `thread/tokenUsage/updated`，Cursor 取 ACP `usage_update`。
    ///
    /// 上游缺窗口时只显示绝对值，不画进度条(没有分母的进度条是假的)。
    pub ctx: crate::observe::context_budget::ContextBudget,
    /// 不参与角标渲染:轮末上下文过重时,提醒卡按它决定有没有 Compact、Handoff 之后原地压缩还是
    /// 换新会话。由轮初种子按 agent 的手动压缩通道定,收尾照用。
    pub context_remedy: crate::observe::context_budget::ContextRemedy,
}

impl TurnBadge {
    /// 只渲染上下文预算那一项,供 [`Self::render`] 拼进角标行。没有分母时仍如实显示
    /// 绝对 token 数;完全取不到则不产生占位。
    pub fn render_context(&self) -> Option<String> {
        crate::context_budget::render_usage_compact(&self.ctx)
    }

    /// 渲染成一行角标文本(未转义)。**每项取不到就省略该项**;一项都没有 → `None`
    /// (调用方据此退回纯时间戳行)。项间用 ` · ` 分隔。
    pub fn render(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        let cli = self.cli.trim();
        if !self.hide_cli && !cli.is_empty() {
            parts.push(cli.to_string());
        }
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !self.hide_model)
        {
            parts.push(model.to_string());
        }
        if !self.hide_effort {
            if let Some(effort) = self
                .effort
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                parts.push(effort.to_string());
            }
        }
        // 有窗口 → 使用量 + 进度条 + 百分比；缺窗口 → 只报可证实的绝对值。
        // **不渲染空槽进度条**:空条会被读成「0%」,那是错误信息,比不显示更糟。
        if let Some(context) = self.render_context().filter(|_| !self.hide_ctx) {
            parts.push(context);
        }
        if self.show_quota {
            if let Some(quota) = self.render_quota() {
                parts.push(quota);
            }
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// 额度项 `5h 88% ↻17:30 7d 87% ↻10/20 09:00`(都是剩余;↻ 后是该窗口的重置时刻)。标签是
    /// 最短的 ASCII 写法(5 小时窗 `5h`、周窗 `7d`);标签、数值、重置时刻之间各留一个空格。
    /// 两个窗口的显示范围见 [`crate::driver::CliDriver::has_five_hour_and_weekly_quota`];
    /// 其余 CLI 只显示长窗,标签用 `quota N%`、不带重置时刻。缺哪个窗口就省哪一半。
    fn render_quota(&self) -> Option<String> {
        let weekly =
            crate::driver::CliDriver::from_kind(self.cli.trim()).has_five_hour_and_weekly_quota();
        let long_window = if weekly { "7d" } else { "quota" };
        let long_reset = self
            .quota_resets_at
            .filter(|_| weekly)
            .and_then(local_md_hhmm);
        let parts: Vec<String> = [
            self.quota_5h_left_pct
                .map(|pct| match self.quota_5h_resets_at.and_then(local_hhmm) {
                    Some(at) => format!("5h {pct}% ↻{at}"),
                    None => format!("5h {pct}%"),
                }),
            self.quota_left_pct.map(|pct| match &long_reset {
                Some(at) => format!("{long_window} {pct}% ↻{at}"),
                None => format!("{long_window} {pct}%"),
            }),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

/// turn 完成后新发的「总结卡」= 完成信号(新消息气泡):**正文打头**,末尾一条分隔线 + 更小字号
/// 的灰字元信息行。`badge` 在前、`context_line` 在后，同一行用 ` · ` 连接；任一为空就只渲另一项。
///
/// Feed 预览由 `config.summary` 显式承载正文；卡片打开后同样以正文打头，上下文行收在末尾。
///
/// 元信息行用飞书卡片 2.0 的 `text_size:"notation"`(12px,比 `normal` 14px 小一档；非法值回退
/// normal 不报错)。来源关系由飞书 reply 自身表达，尾部只需要把实时角标与时刻收在同一视觉层。
pub fn card_turn_summary(reply: &str, context_line: Option<&str>, badge: Option<&str>) -> String {
    // 单文本片段 = 内联图片版的退化情形。走同一份渲染,避免两条路日后漂移。
    card_turn_summary_pieces(
        &[crate::image::ReplyPiece::Text(reply.to_string())],
        context_line,
        badge,
    )
}

/// 角标的更小字号档:飞书卡片 2.0 markdown 组件 `text_size` 的合法值 `notation`(12px,比 `normal`
/// 14px 小一档)。**已知安全档**——飞书文档明确 markdown 支持 `text_size`,且填非法值只回退 `normal`
/// 不报错(与「不支持的属性直接 230099」不同)。集中一处便于日后调档。
const BADGE_TEXT_SIZE: &str = "notation";

/// Feed 只展示很短的一段；在卡片 JSON 内保留 200 个 Unicode 字符，兼顾中英文与 30KB 总预算。
const FEED_SUMMARY_MAX_CHARS: usize = 200;

/// 会话列表预览(`config.summary`)。预览是纯文本,卡内渲染正常的 markdown 标记在那里会原样露出来,
/// 所以先去掉标记、只留读得到的字,再压成一行。
pub fn feed_summary(text: &str) -> Option<String> {
    let escaped = escape_lark_md(&plain_preview_text(text));
    let summary = escaped.split_whitespace().collect::<Vec<_>>().join(" ");
    let summary = summary
        .chars()
        .take(FEED_SUMMARY_MAX_CHARS)
        .collect::<String>();
    (!summary.is_empty()).then_some(summary)
}

fn rendered_feed_summary(markdown: &RenderedLarkMd) -> Option<String> {
    feed_summary(markdown.as_str())
}

/// lark markdown 里会出现在卡上、但在纯文本预览里只剩噪音的标签。只认这几个:外部文本里的 `<`
/// 已被 [`escape_lark_md`] 拆开,剩下的 `<字母` 都是 summon 自己插的;但原始文本也会走到这里,
/// 白名单让 `Vec<String>` 这类正文不被当成标签吃掉。
const PREVIEW_STRIPPED_TAGS: &[&str] =
    &["at", "font", "text_tag", "link", "local_datetime", "person"];

/// 把 lark markdown 变成预览里读得到的字:去掉行首的标题 / 引用 / 列表标记、代码围栏与分隔线,
/// 行内成对的加粗、删除线、行内代码与上面那几个标签;链接与图片留文字。只认同一行里能配上对的
/// 标记,单个 `*` `_` 也不动 —— `crates/**/*.rs`、`2**10`、`snake_case` 这类正文里的符号
/// 误删比漏删更伤。
fn plain_preview_text(markdown: &str) -> String {
    let lines: Vec<&str> = markdown
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("```") && !is_markdown_rule(line))
        .map(strip_line_marker)
        .collect();
    // `（<at id=…></at>）` 这类只装着标签的括号,标签去掉后只剩一对空括号。
    strip_inline_markup(&lines.join("\n")).replace("（）", "")
}

fn is_markdown_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|mark| compact.chars().all(|c| c == *mark))
}

fn strip_line_marker(line: &str) -> &str {
    let mut line = line;
    loop {
        let before = line;
        if let Some(rest) = line.strip_prefix('>') {
            line = rest.trim_start();
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
            line = line[hashes..].trim_start();
        }
        for bullet in ["- ", "* ", "+ "] {
            if let Some(rest) = line.strip_prefix(bullet) {
                line = rest.trim_start();
            }
        }
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 {
            let rest = &line[digits..];
            if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
                line = rest.trim_start();
            }
        }
        if line == before {
            return line;
        }
    }
}

fn strip_inline_markup(text: &str) -> String {
    text.split('\n')
        .map(strip_line_markup)
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_line_markup(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(c) = rest.chars().next() {
        let after_word = out
            .chars()
            .next_back()
            .is_some_and(|p| p.is_ascii_alphanumeric());
        if let Some((inner, after)) = ["**", "~~", "`"]
            .iter()
            .filter(|_| !after_word)
            .find_map(|mark| paired_markup(rest, mark))
        {
            out.push_str(&strip_line_markup(inner));
            rest = after;
            continue;
        }
        if let Some((label, after)) = markdown_link(rest.strip_prefix('!').unwrap_or(rest)) {
            out.push_str(label);
            rest = after;
            continue;
        }
        if let Some(after) = preview_stripped_tag(rest) {
            rest = after;
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// 以 `mark` 开头、同一行里还有一个 `mark` 把它合上、中间夹着字 → (中间的字, 合上之后的剩余)。
/// 贴着英文字母或数字的不算标记(调用方管开标记前一个字,这里管合标记后一个字):`2**10`、
/// `crates/**/*.rs … 2**10` 里的星号都是正文;开标记后紧跟空白的也不认,`2 ** 10` 是算式。
/// 汉字前后的加粗(`这是**重点**内容`)照常认。
fn paired_markup<'a>(text: &'a str, mark: &str) -> Option<(&'a str, &'a str)> {
    let after_open = text.strip_prefix(mark)?;
    if after_open.starts_with(char::is_whitespace) {
        return None;
    }
    after_open.match_indices(mark).find_map(|(close, _)| {
        let inner = &after_open[..close];
        let after = &after_open[close + mark.len()..];
        let closes = !inner.is_empty()
            && !after
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_alphanumeric());
        closes.then_some((inner, after))
    })
}

/// `[文字](地址)` → (文字, 其后剩余)。不成形就不认。
fn markdown_link(text: &str) -> Option<(&str, &str)> {
    let inner = text.strip_prefix('[')?;
    let close = inner.find(']')?;
    let after = inner[close + 1..].strip_prefix('(')?;
    let end = after.find(')')?;
    Some((&inner[..close], &after[end + 1..]))
}

/// 以白名单标签的开、闭或自闭合标记开头时,返回标记之后的剩余;标签里的文字照常留下。
fn preview_stripped_tag(text: &str) -> Option<&str> {
    let inner = text.strip_prefix('<')?;
    let name_start = inner.strip_prefix('/').unwrap_or(inner);
    let name_len = name_start
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(name_start.len());
    if !PREVIEW_STRIPPED_TAGS.contains(&&name_start[..name_len]) {
        return None;
    }
    let end = inner.find('>')?;
    Some(&inner[end + 1..])
}

/// 给没有预览的 2.0 卡补上一句,取标题与第一段正文。会话列表只认 2.0 卡的 `config.summary`,
/// 没写时飞书自己从卡里取字,加粗的星号就原样出现在列表里。卡片被交互改写时同样经过这里,
/// 预览因此跟着新内容走;已经写了预览的卡(agent 给的 feed 总结、带状态的预览)原样不动。
///
/// 不需要改时返回 `None`:不是 2.0 卡、已有预览、取不出字,或补上之后会超出整卡预算。
pub fn card_with_derived_feed_summary(card_json: &str) -> Option<String> {
    let mut card: serde_json::Value = serde_json::from_str(card_json).ok()?;
    if card.get("schema").and_then(|v| v.as_str()) != Some("2.0") {
        return None;
    }
    let has_summary = card
        .pointer("/config/summary/content")
        .and_then(|v| v.as_str())
        .is_some_and(|content| !content.trim().is_empty());
    if has_summary {
        return None;
    }
    let title = card
        .pointer("/header/title/content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let body = first_body_text(&card).unwrap_or_default();
    let summary = feed_summary(&format!("{title}\n{body}"))?;
    if card_json.len() + summary.len() + r#","config":{"summary":{"content":""}}"#.len()
        > CARD_TOTAL_SAFE_LIMIT
    {
        return None;
    }
    set_feed_summary(&mut card, summary);
    Some(card.to_string())
}

fn closeable_card(card: String) -> String {
    card_with_close_button(&card).unwrap_or(card)
}

/// 为操作面板和提醒卡补上撤回入口，由构造函数在冻结 durable 请求前显式调用。
pub fn card_with_close_button(card_json: &str) -> Option<String> {
    fn has_close(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("disabled").and_then(serde_json::Value::as_bool) == Some(true) {
                    return false;
                }
                map.get("action")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|action| matches!(action, "session_close" | "card_close"))
                    || map.values().any(has_close)
            }
            serde_json::Value::Array(values) => values.iter().any(has_close),
            _ => false,
        }
    }
    let mut card: serde_json::Value = serde_json::from_str(card_json).ok()?;
    if has_close(&card) {
        return None;
    }
    let v2 = card.get("schema").and_then(serde_json::Value::as_str) == Some("2.0");
    let button = if v2 {
        cb_btn(
            summon_i18n::text!("r5242df9b30"),
            "default",
            serde_json::json!({"action": "card_close"}),
        )
    } else {
        serde_json::json!({
            "tag": "button", "text": {"tag": "plain_text", "content": summon_i18n::text!("r5242df9b30")},
            "type": "default", "value": {"action": "card_close"}
        })
    };
    // 操作卡定格后仍可关闭，复用原位置以保持卡片布局。
    fn restore_close(value: &mut serde_json::Value, button: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("tag").and_then(serde_json::Value::as_str) == Some("button")
                    && map.get("disabled").and_then(serde_json::Value::as_bool) == Some(true)
                    && map.values().any(has_close)
                {
                    *value = button.clone();
                    return true;
                }
                map.values_mut().any(|child| restore_close(child, button))
            }
            serde_json::Value::Array(items) => {
                items.iter_mut().any(|child| restore_close(child, button))
            }
            _ => false,
        }
    }
    if restore_close(&mut card, &button) {
        return Some(card.to_string());
    }
    let elements = if v2 {
        card.pointer_mut("/body/elements")?
    } else {
        card.get_mut("elements")?
    }
    .as_array_mut()?;
    let button = if v2 {
        button_row(vec![button])
    } else {
        serde_json::json!({"tag": "action", "actions": [button]})
    };
    elements.push(button);
    Some(card.to_string())
}

/// 交互之后的卡:原卡内容留着(按钮置灰、底部加一行结果),从正文推出来的预览会停在交互前那一句,
/// 看不出这张卡已经处理过。预览因此写成「结果|原卡第一段」,与退出 / 归档的状态预览同一形状。
fn with_outcome_feed_summary(mut card: serde_json::Value, outcome: &str) -> String {
    let summary = match (
        feed_summary(outcome),
        first_body_text(&card).and_then(feed_summary),
    ) {
        (Some(outcome), Some(body)) => format!("{outcome}{FEED_STATUS_SEPARATOR}{body}")
            .chars()
            .take(FEED_SUMMARY_MAX_CHARS)
            .collect(),
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => return closeable_card(card.to_string()),
    };
    set_feed_summary(&mut card, summary);
    closeable_card(card.to_string())
}

/// 卡上第一段正文:跳过角标那种小字号行,只认顶层的 markdown / div 文本。
fn first_body_text(card: &serde_json::Value) -> Option<&str> {
    let elements = card
        .get("body")
        .and_then(|body| body.get("elements"))
        .or_else(|| card.get("elements"))
        .and_then(|v| v.as_array())?;
    elements.iter().find_map(|element| {
        if element.get("text_size").and_then(|v| v.as_str()) == Some("notation") {
            return None;
        }
        match element.get("tag").and_then(|v| v.as_str()) {
            Some("markdown") => element.get("content").and_then(|v| v.as_str()),
            Some("div") => element
                .get("text")
                .and_then(|v| v.get("content"))
                .and_then(|v| v.as_str()),
            _ => None,
        }
    })
}

fn feed_summary_for_pieces(pieces: &[crate::image::ReplyPiece]) -> Option<String> {
    use crate::image::ReplyPiece;
    let body = pieces
        .iter()
        .map(|piece| match piece {
            ReplyPiece::Text(text) => text.as_str(),
            ReplyPiece::Image { alt, .. } if alt.trim().is_empty() => {
                summon_i18n::text!("rd24c10d37d")
            }
            ReplyPiece::Image { alt, .. } => alt.as_str(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    feed_summary(&body).or_else(|| Some(summon_i18n::text!("rcf945e21dd").to_string()))
}

/// 卡片里的一张图。**只给 `tag`/`img_key`/`alt` 三个字段**——飞书卡片 2.0 对不支持的属性是
/// 报错而非忽略(定时器参数卡的 `label` 已经踩过一次),没有真机证据的属性一律不加。
fn img_el(key: &str, alt: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "img",
        "img_key": key,
        "alt": {"tag": "plain_text", "content": alt},
    })
}

/// 总结卡的内联图片版:文本与图按**正文里的原始顺序**交替成卡片元素。
///
/// 每个文本片段各自走与无图时**完全相同**的渲染(`md_el` + `clip_for_stream`),因此「没有图」这条
/// 路径逐字节不变——这是把它接进终态投递热路径的前提:绝大多数回复没有图,它们不该有任何变化。
/// subagent 尾部区块在卡 JSON 里的 `element_id`。有它才能**幂等替换**——否则每刷一次就往卡里
/// 追加一段,几分钟后卡片全是重复的子代理列表。
pub const SUBAGENT_TAIL_ELEMENT_ID: &str = "summon_subagent_tail";
/// 尾部区块自带的分隔线 id(与正文里其它 `hr` 区分开,替换时才能连它一起摘掉)。
pub const SUBAGENT_TAIL_HR_ELEMENT_ID: &str = "summon_subagent_tail_hr";

/// 往一张**已经渲染好**的卡 JSON 上挂(或换掉)subagent 尾部区块。
///
/// 为什么作用在渲染产物上、而不是给各卡的渲染函数加参数:subagent 尾部**只渲在 live 卡**是
/// 一条既有规则(收尾/总结卡一行都不渲,`subagent_tail_renders_only_on_live_card` 钉死)。
/// 本函数不碰那条规则——它只在 IO 层给一张已发出的卡追加区块,用于「main agent 已推送完、
/// 子代理还在跑」这个窗口里持续刷新状态(owner 2026-08-14 需求)。渲染函数的行为一字不变。
///
/// 幂等:每次都先摘掉上一轮挂的区块再挂新的,所以可以反复调用。`lines` 为空 = 只摘不挂
/// (子代理全部结束后卡片自动干净收场)。
///
/// 解析失败 / 结构不认识 → `None`,调用方跳过这次刷新,**绝不**把一张坏卡 PATCH 上去。
pub fn card_with_subagent_tail(card_json: &str, lines: &[String]) -> Option<String> {
    let mut card: serde_json::Value = serde_json::from_str(card_json).ok()?;
    let elements = card.get_mut("body")?.get_mut("elements")?.as_array_mut()?;
    // 先摘旧的(含它自己的分隔线),保证幂等。
    elements.retain(|el| {
        !matches!(
            el.get("element_id").and_then(serde_json::Value::as_str),
            Some(SUBAGENT_TAIL_ELEMENT_ID) | Some(SUBAGENT_TAIL_HR_ELEMENT_ID)
        )
    });
    if !lines.is_empty() {
        let body = lines
            .iter()
            .map(|l| escape_lark_md(l))
            .collect::<Vec<_>>()
            .join("\n");
        elements.push(serde_json::json!({
            "tag": "hr",
            "element_id": SUBAGENT_TAIL_HR_ELEMENT_ID,
        }));
        elements.push(serde_json::json!({
            "tag": "markdown",
            "element_id": SUBAGENT_TAIL_ELEMENT_ID,
            "content": summon_i18n::format!("rf8ddd9f584"),
        }));
    }
    Some(card.to_string())
}

/// 会话列表预览里状态与内容之间的分隔符。
const FEED_STATUS_SEPARATOR: char = '|';

/// 写进会话列表预览开头的会话状态。标签比卡内那行说明短:预览那一行只放得下几个字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedStatus {
    Exited,
    Archived,
}

impl FeedStatus {
    const ALL: [FeedStatus; 2] = [FeedStatus::Exited, FeedStatus::Archived];

    fn label_in(self, language: crate::i18n::Language) -> &'static str {
        let key = match self {
            FeedStatus::Exited => "r899df7e244",
            FeedStatus::Archived => "r82395d4230",
        };
        crate::i18n::text_in(language, key)
    }

    fn label(self) -> &'static str {
        self.label_in(crate::i18n::language())
    }
}

/// 把一句总结设成卡片的会话列表预览。卡片 JSON 认不出时原样返回,预览留给卡内正文。
pub fn card_with_feed_summary(card_json: &str, summary: &str) -> String {
    let (Ok(mut card), Some(summary)) = (
        serde_json::from_str::<serde_json::Value>(card_json),
        feed_summary(summary),
    ) else {
        return card_json.to_string();
    };
    set_feed_summary(&mut card, summary);
    card.to_string()
}

fn set_feed_summary(card: &mut serde_json::Value, summary: String) {
    let Some(root) = card.as_object_mut() else {
        return;
    };
    let config = root
        .entry("config")
        .or_insert_with(|| serde_json::json!({}));
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    config["summary"] = serde_json::json!({"content": summary});
}

/// 预览写成 `状态|内容`。内容沿用卡片此刻的预览(agent 给的总结,或正文摘要),先摘掉上一次写进去的
/// 状态,连续的状态更新只留当前那个;卡片没有预览时取卡内第一段正文。
fn update_status_feed_summary(card: &mut serde_json::Value, status: FeedStatus) {
    let current = card
        .pointer("/config/summary/content")
        .and_then(|v| v.as_str())
        .map(|content| without_feed_status(content).to_string())
        .filter(|content| !content.trim().is_empty());
    let content = current.or_else(|| feed_summary(first_body_text(card)?));
    let summary = match content {
        Some(content) => format!("{}{FEED_STATUS_SEPARATOR}{content}", status.label())
            .chars()
            .take(FEED_SUMMARY_MAX_CHARS)
            .collect(),
        None => status.label().to_string(),
    };
    set_feed_summary(card, summary);
}

/// 摘掉预览开头上一次写进去的状态。也认 `会话已… · 内容` 这种写法:升级前落盘的退出凭据里,
/// 卡片预览是整句状态说明加间隔号,归档时同样要换掉,不能叠在新状态后面。
fn without_feed_status(content: &str) -> &str {
    if let Some(rest) = FeedStatus::ALL
        .iter()
        .flat_map(|status| {
            [
                crate::i18n::Language::Chinese,
                crate::i18n::Language::English,
            ]
            .map(|language| status.label_in(language))
        })
        .find_map(|label| {
            content
                .strip_prefix(label)
                .and_then(|rest| rest.strip_prefix(FEED_STATUS_SEPARATOR))
        })
    {
        return rest;
    }
    match content.split_once(" · ") {
        Some((status, rest)) if status.starts_with(summon_i18n::source!("r523d885a46")) => rest,
        _ => content,
    }
}

/// 在已发出的卡片末尾追加一条分割线和一行灰字说明(如「会话已归档」),并把会话列表预览改成
/// `status` 打头。认卡片 2.0(`body.elements`)与旧版(顶层 `elements`)两种骨架;认不出就返回
/// `None`,调用方另发短消息。
/// `status` 为 `None` 时只追加说明行,不改消息列表预览打头的状态。
pub fn card_with_footer_note(
    card_json: &str,
    note: &str,
    status: Option<FeedStatus>,
) -> Option<String> {
    let mut card: serde_json::Value = serde_json::from_str(card_json).ok()?;
    let elements = if card.get("body").is_some() {
        card.get_mut("body")?.get_mut("elements")?.as_array_mut()?
    } else {
        card.get_mut("elements")?.as_array_mut()?
    };
    elements.push(serde_json::json!({"tag": "hr"}));
    elements.push(json_grey_line(note));
    if let Some(status) = status {
        update_status_feed_summary(&mut card, status);
    }
    Some(card.to_string())
}

/// 退出说明与归档入口共用一行，正文与角标保留在原卡中。`archived` 同时决定预览打头的状态。
pub fn card_with_exit_archive(
    card_json: &str,
    note: &str,
    token: &str,
    archived: bool,
) -> Option<String> {
    let mut card: serde_json::Value = serde_json::from_str(card_json).ok()?;
    let elements = if card.get("body").is_some() {
        card.get_mut("body")?.get_mut("elements")?.as_array_mut()?
    } else {
        card.get_mut("elements")?.as_array_mut()?
    };
    elements.retain(|el| {
        !matches!(
            el.get("element_id").and_then(|v| v.as_str()),
            Some("exit_archive_hr") | Some("exit_archive_row")
        )
    });
    let mut button = cb_btn(
        summon_i18n::text!("rdc2e173490"),
        "default",
        serde_json::json!({
            "action": "archive_ended", "token": token,
        }),
    );
    if archived {
        button["disabled"] = serde_json::json!(true);
        button["behaviors"] = serde_json::json!([]);
    }
    elements.push(serde_json::json!({"tag": "hr", "element_id": "exit_archive_hr"}));
    elements.push(serde_json::json!({
        "tag": "column_set", "element_id": "exit_archive_row",
        "flex_mode": "none", "horizontal_spacing": "8px",
        "columns": [
            {"tag": "column", "width": "weighted", "weight": 1,
             "vertical_align": "center", "elements": [json_grey_line(note)]},
            {"tag": "column", "width": "auto", "vertical_align": "center",
             "elements": [button]},
        ],
    }));
    update_status_feed_summary(
        &mut card,
        if archived {
            FeedStatus::Archived
        } else {
            FeedStatus::Exited
        },
    );
    Some(card.to_string())
}

pub fn card_turn_summary_pieces(
    pieces: &[crate::image::ReplyPiece],
    context_line: Option<&str>,
    badge: Option<&str>,
) -> String {
    use crate::image::ReplyPiece;
    use serde_json::json;
    let mut elements: Vec<serde_json::Value> = Vec::new();
    for piece in pieces {
        match piece {
            ReplyPiece::Text(text) => {
                let clipped = clip_for_stream(text, CARD_TEXT_SAFE_LIMIT);
                elements.push(md_el(clipped.as_deref().unwrap_or(text)));
            }
            ReplyPiece::Image { key, alt } => elements.push(img_el(key, alt)),
        }
    }
    if elements.is_empty() {
        elements.push(md_el(""));
    }
    let context = context_line.map(str::trim).filter(|s| !s.is_empty());
    let badge = badge.map(str::trim).filter(|s| !s.is_empty());
    if context.is_some() || badge.is_some() {
        elements.push(json!({"tag": "hr"}));
        let footer = badge
            .into_iter()
            .chain(context)
            .collect::<Vec<_>>()
            .join(" · ");
        elements.push(json!({
            "tag": "markdown",
            "content": format!("<font color='grey'>{}</font>", escape_lark_md(&footer)),
            "text_size": BADGE_TEXT_SIZE,
        }));
    }
    let mut card = json!({"schema": "2.0", "body": {"elements": elements}});
    if let Some(summary) = feed_summary_for_pieces(pieces) {
        card["config"] = json!({"summary": {"content": summary}});
    }
    card.to_string()
}

/// markdown 卡片的 **schema 2.0** 形态(任务4):与带按钮的流式卡同 schema,飞书 PATCH 不允许在
/// v1↔v2 间换 schema,故流式生命周期(begin/patch/finish)统一走 2.0——`card_markdown`(v1)用于
/// 非流式的独立卡(回执/错误等),两者不混在同一张卡的刷新链上。
pub fn card_markdown_v2(markdown: &str) -> String {
    let summary =
        feed_summary(markdown).unwrap_or_else(|| summon_i18n::text!("rcf945e21dd").to_string());
    serde_json::json!({
        "schema": "2.0",
        "config": {"summary": {"content": summary}},
        "body": {"elements": [md_el(markdown)]}
    })
    .to_string()
}

/// [`card_markdown_v2`] 的已渲染形态：保留 [`render_external_lark_md`] 插入的受信 mention。
pub fn card_markdown_v2_rendered(markdown: &RenderedLarkMd) -> String {
    let summary = rendered_feed_summary(markdown)
        .unwrap_or_else(|| summon_i18n::text!("rcf945e21dd").to_string());
    serde_json::json!({
        "schema": "2.0",
        "config": {"summary": {"content": summary}},
        "body": {"elements": [rendered_md_el(markdown)]}
    })
    .to_string()
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
/// **正在输入**的流式卡片(任务4 改:打断按钮**嵌进正在刷的消息卡**,不再单独发控制卡):正文 +
/// 分割线 + 末尾一个极简「⏸ 打断」按钮(`action=interrupt`,operate-gate;路由按 card.chat_id,
/// value 无需带 tag)。turn 收尾时整卡被 `stream_finish`(无按钮)/ 折叠卡 / 暂停卡覆盖,按钮自然消失。
pub(crate) fn card_stream_with_interrupt(markdown: &str) -> String {
    use serde_json::json;
    let elements = vec![
        md_el(markdown),
        json!({"tag": "hr"}),
        // danger=红框 + confirm 二次确认弹窗(误点防护:正在生成时一点就停,给个拦截)。
        button_row(vec![cb_btn_confirm(
            "⏸ 打断",
            "danger",
            json!({"action": "interrupt"}),
            "确认打断当前这一轮?已生成的内容会停在原处。",
        )]),
    ];
    json!({"schema": "2.0", "body": {"elements": elements}}).to_string()
}

/// [`card_stream_with_interrupt`] 的已渲染形态：正文只接收安全类型，不二次转义 `<at>`。
pub fn card_stream_with_interrupt_rendered(markdown: &RenderedLarkMd) -> String {
    use serde_json::json;
    let elements = vec![
        rendered_md_el(markdown),
        json!({"tag": "hr"}),
        button_row(vec![cb_btn_confirm(
            summon_i18n::text!("r80f61fcff8"),
            "danger",
            json!({"action": "interrupt"}),
            summon_i18n::text!("r30a2531ee1"),
        )]),
    ];
    json!({"schema": "2.0", "body": {"elements": elements}}).to_string()
}

/// turn 被打断后把「过程卡」(流式卡)定格的卡片:⏸ 已暂停 + 折叠已生成部分(partial)。
/// schema 2.0(与流式卡同 schema,PATCH 不可换)。超长裁剪。
/// 流中断卡:claude 报 `API Error: Response stalled mid-stream…` 时发一张,带「继续」按钮。
///
/// 为什么值得单独一张卡:这一态**是可续的** —— 会话还活着、上下文都在,往里发一句「继续」就能
/// 接着跑。只贴个 ERROR 表情等于把一个一键可恢复的情况呈现成了失败,用户还得自己想到去说「继续」。
///
/// 按钮复用既有的 `retry_now` 动作(注入「继续」+ Esc),不新增动作类型。**不加二次确认**:
/// 它不销毁任何东西,最坏是白催一次。
pub fn card_stream_stalled(partial: &str) -> String {
    use serde_json::json;
    let mut elements = vec![json!({
        "tag": "markdown",
        "content": summon_i18n::text!("rf508e0e2a4")
    })];
    let clipped = clip_for_stream(partial, CARD_TEXT_SAFE_LIMIT);
    let body = clipped.as_deref().unwrap_or(partial);
    if !body.trim().is_empty() {
        elements.push(json!({
            "tag": "collapsible_panel",
            "expanded": false,
            "header": {"title": {"tag": "markdown", "content": summon_i18n::text!("r7254f11f74")}},
            "elements": [{"tag": "markdown", "content": lark_md_body(body)}],
        }));
    }
    elements.push(json!({"tag": "hr"}));
    elements.push(button_row(vec![cb_btn(
        summon_i18n::text!("rd582054c8b"),
        "primary",
        json!({"action": "retry_now"}),
    )]));
    json!({"schema": "2.0", "body": {"elements": elements}}).to_string()
}

/// sentinel 授权缺口卡(**只私聊 owner**)。
///
/// 为什么值得单独一张卡:2026-08-07 现场,sentinel 因为 owner 的 lark-cli 用户身份掉线而**整体
/// 停摆两小时**,飞书侧毫无表示 —— owner 只觉得「@ 了没反应」,像功能坏了。而 lark-cli 早就把
/// 缺哪个 scope 写在响应里了,这条信息一路被丢到日志里没人看。
///
/// 卡上直接给出**从错误原文取的** scope 名与可复制的命令。人靠记忆猜 scope 名是会错的
/// (当天就猜了个不存在的 `im:chat:readonly`,白授权一次);机器从 `missing_scopes` 里取不会错。
///
/// 不带按钮:device-flow 要在**终端**里完成(浏览器授权),卡片上点不动;给命令比给假按钮诚实。
pub fn card_sentinel_auth_gap(missing: &[String], detail: &str) -> String {
    use serde_json::json;
    let scopes = missing.join(" ");
    let head = if missing.is_empty() {
        summon_i18n::text!("rc0f124ad42").to_string()
    } else {
        summon_i18n::format!("rf67e775144", missing.len())
    };
    let mut elements = vec![json!({"tag": "markdown", "content": head})];
    if !scopes.is_empty() {
        elements.push(json!({
            "tag": "markdown",
            "content": summon_i18n::format!("r6d96e145d1", escape_lark_md(&scopes)),
        }));
        elements.push(json!({
            "tag": "markdown",
            "content": summon_i18n::format!(
                "r738e9aafb8",
                scopes
            ),
        }));
    } else {
        elements.push(json!({
            "tag": "markdown",
            "content": summon_i18n::text!("rbbe2356a37"),
        }));
    }
    elements.push(json!({
        "tag": "markdown",
        "content": summon_i18n::format!(
            "r81b8e07c43",
            escape_lark_md(detail)
        ),
    }));
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// sentinel 用的 lark-cli profile 没有 owner 的用户身份时私聊 owner 的卡。`profile=None` 表示
/// config.yaml 没指定、跟着 lark-cli 默认 profile 走 —— 这正是最常见的来路:默认 profile 被改成
/// 只有 bot 身份,sentinel 跟着一起失明。
pub fn card_sentinel_login_missing(profile: Option<&str>) -> String {
    use serde_json::json;
    let which = match profile {
        Some(p) => summon_i18n::format!("r35842968eb", escape_lark_md(p)),
        None => summon_i18n::text!("r5cad457512").to_string(),
    };
    let login = format!(
        "lark-cli auth login --profile {}",
        profile.unwrap_or(summon_i18n::text!("r8ea64c857d"))
    );
    let elements = vec![
        json!({"tag": "markdown", "content": summon_i18n::text!("r3a3e0ed8d0")}),
        json!({
            "tag": "markdown",
            "content": summon_i18n::format!("r38a402816a"),
        }),
        json!({
            "tag": "markdown",
            "content": summon_i18n::format!(
                "r4ec7caa51b"
            ),
        }),
        json!({
            "tag": "markdown",
            "content": summon_i18n::text!("raff230e1d7"),
        }),
    ];
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// 孤儿 live 卡的终态:**只断言观察,不断言工作**。
///
/// daemon 死掉时,claude 还在 tmux(持久层)里跑,而且大概率会跑完 —— 所以 daemon 没有资格说
/// 「已中断」(工作可能好好完成了),也没有资格说「已完成」(它并不知道)。它唯一知道的事实是:
/// **自己在这里停止了观察**。真实结果由转录镜像随后补发收敛(那就是反熵/读修复)。
///
/// 卡上不留打断按钮 —— 它对应的那一轮在本 daemon 里已经不存在。
///
/// **这个函数是兜底,不是首选。** 它整卡重渲、会把已经渲出来的正文抹掉;正常路径应当走
/// [`card_observation_interrupted_with_body`](带上重建出来的正文)。只有正文重建不出来时才退到这里。
pub fn card_observation_interrupted(at_label: &str) -> String {
    use serde_json::json;
    closeable_card(
        json!({
            "schema": "2.0",
            "body": {"elements": observation_interrupted_elements(at_label)}
        })
        .to_string(),
    )
}

/// 「观察中断」那两行本身(标题 + 灰字说明),给整卡重渲与追加两条路共用。
fn observation_interrupted_elements(at_label: &str) -> Vec<serde_json::Value> {
    use serde_json::json;
    let head = if at_label.is_empty() {
        summon_i18n::text!("r7abf0694a0").to_string()
    } else {
        summon_i18n::format!("rfec79cb104", escape_lark_md(at_label))
    };
    vec![
        json!({"tag": "markdown", "content": head}),
        json!({"tag": "markdown", "content": summon_i18n::text!("r01a108feaa")}),
    ]
}

/// 孤儿 live 卡的终态卡:**已生成的正文 + 分隔线 + 中断说明**。
///
/// **why 要重新渲一整张,而不是在原卡上追加**(owner 2026-08-10 定的 B 方案):
/// 追加需要先读回原卡内容,而**飞书不提供这个能力** —— `im/v1/messages/:id` 对 interactive
/// 返回的是「请升级至最新版本客户端」这样的降级占位,没有 `body.elements`。上一版按"读回再追加"
/// 实现的方案因此从未生效过,每次都静默退回整卡覆盖(正文被两行说明盖掉,owner 实测)。
///
/// 正文由调用方从 `events.jsonl` 重建后传进来(那份数据本来就在全量落盘,零新增写入);
/// 这里只负责渲染。`segments` 为空 = 那一轮还没产出任何内容,退化成纯说明卡。
///
/// **卡上不留任何按钮**:打断按钮对应的那一轮在本 daemon 里已经不存在,留着是误导。
pub fn card_observation_interrupted_with_body(
    segments: &[crate::segment::TurnSegment],
    at_label: &str,
) -> String {
    use serde_json::json;
    let mut elements: Vec<serde_json::Value> = Vec::new();
    // 重建的回复段是 events.jsonl 里的原文,末尾的总结标签不进卡。
    let segments: Vec<crate::segment::TurnSegment> = segments
        .iter()
        .map(|segment| match segment {
            crate::segment::TurnSegment::Reply(text) => crate::segment::TurnSegment::Reply(
                crate::feed_tag::strip_feed_tag(text).into_owned(),
            ),
            other => other.clone(),
        })
        .collect();
    // progress 传 None:这张卡是 daemon 重启后从 events.jsonl 重建的,那一轮的轨迹随进程一起没了。
    if let Some(body) = turn_done_body(&segments, None) {
        // live 卡中的思考原本默认收起。daemon 重启只能改变「是否继续观察」，不能顺带把展示
        // 密度改成全量展开；已生成过程（含思考）整体折叠，需要时再由用户点开。
        elements.push(collapsible_panel_unclipped(
            summon_i18n::text!("re937a1d331"),
            false,
            &body,
        ));
        elements.push(json!({"tag": "hr"}));
    } else {
        return card_observation_interrupted(at_label);
    }
    elements.extend(observation_interrupted_elements(at_label));
    let out = json!({"schema": "2.0", "body": {"elements": elements}}).to_string();
    // 超限就退回纯说明卡:宁可信息少,也不产一张飞书拒渲的卡(那样连中断说明都看不到)。
    if out.len() > CARD_TOTAL_SAFE_LIMIT {
        return card_observation_interrupted(at_label);
    }
    out
}

/// 崩溃恢复卡片:列出某 chat 里「收到但没跑完」的消息(入站 WAL pending),每条一个「恢复」按钮
/// (回传 `message_id`,daemon 据此从 WAL 取回原消息重新注入),外加一个「全部忽略」。daemon 意外被杀
/// 重启后由恢复扫描现构。**不自动重跑**——交用户点选(#3 设计)。`items` = (message_id, 预览) 列表。
pub fn card_crash_recovery(
    batch_id: &str,
    chat_tag: &str,
    stage: Option<&str>,
    items: &[(&str, &str)],
    recovered_ids: &[String],
) -> String {
    use serde_json::json;
    let n_pending = items
        .iter()
        .filter(|(mid, _)| !recovered_ids.iter().any(|r| r == *mid))
        .count();
    let mut elements = vec![json!({
        "tag": "markdown",
        "content": summon_i18n::format!(
            "rbf0c42b745",
            n_pending
        )
    })];
    // #3:崩溃时 agent「进行到什么阶段」—— pending.json 里那轮已累积的部分回答(有才渲染)。
    if let Some(stage) = stage.map(str::trim).filter(|s| !s.is_empty()) {
        let stage = escape_recovery_plain_text(stage);
        elements.push(json!({
            "tag": "markdown",
            "content": summon_i18n::format!("rb56623979f")
        }));
    }
    for (mid, preview) in items {
        let is_recovered = recovered_ids.iter().any(|r| r == mid);
        let preview = escape_recovery_plain_text(preview);
        elements.push(json!({"tag": "markdown", "content": format!("- {preview}")}));
        if is_recovered {
            elements.push(button_row(vec![cb_btn_disabled(
                summon_i18n::text!("re43d9ea9b4"),
                "default",
            )]));
        } else {
            elements.push(button_row(vec![cb_btn(
                summon_i18n::text!("re0534b8a4e"),
                "primary",
                json!({"action": "recover", "batch": batch_id, "mid": mid, "chat": chat_tag}),
            )]));
        }
    }
    elements.push(button_row(vec![cb_btn(
        summon_i18n::text!("r2f56ad4f73"),
        "default",
        json!({"action": "dismiss_recovery", "batch": batch_id, "chat": chat_tag}),
    )]));
    let card = json!({"schema": "2.0", "body": {"elements": elements}}).to_string();
    closeable_card(card)
}

fn escape_recovery_plain_text(value: &str) -> String {
    let single_line = value
        .chars()
        .map(|character| match character {
            '\r' | '\n' => ' ',
            '`' => '｀',
            other => other,
        })
        .collect::<String>();
    escape_lark_md(&single_line)
}

/// 主动唤出的会话控制面板卡片(`/session`),飞书 **Card JSON 2.0**。
/// 元素:摘要(cli/通道/model/effort)→ 模型预设按钮(候选按 cli 分桶,`messaging.models_by_cli`,
/// 各 CLI 模型命名空间不同、互不通用)→ effort 按钮(仅有独立 effort 轴的 cli)→
/// 生命周期按钮(reset/清理/compact)→「私信候选目录」按钮(改工作目录一律走私信选择器)。
/// 每个回调按钮 `behaviors.value` 带 `action` + `chat`(tag)供 daemon 路由。
/// v2 用 `behaviors:[{type:callback,value}]` 声明回调(回调仍落在 `action.value`,sidecar 既有逻辑可收)。
///
/// **隐私(A):面板绝不携带/展示工作目录路径**——群面板是群可见面,路径一旦进来就进群。故摘要
/// 工作目录行只放引导语、且**不再接收 work_dir 参数**(路径物理上到不了群卡构建器,是类型级的
/// 「路径不进群」保证,而非仅靠不渲染)。查看 / 切换工作目录一律经私信选择器(`pick_workdir`)。
/// Codex Fast 控件的运行态。`selected=None` 不是 Standard，而是历史会话尚未由 summon 接管档位；
/// 此时卡片只据实标「沿用 Codex」，用户一旦选择就固定为 Standard/Fast 两态之一。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexSpeedControl {
    pub selected: Option<crate::driver::CodexServiceTier>,
    /// 来自 Codex 本机模型目录：Some(true/false)=已知支持性；None=目录缺失/模型未知，不能假装可用。
    pub supported: Option<bool>,
}

/// Cursor ACP 动态 mode catalog 中可供卡片选择的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorModeOption {
    pub id: String,
    pub name: String,
}

/// Cursor 会话模式控件。mode 是标准 ACP 的 session 级状态，不等同于会改 owner 全局配置的
/// `configOptions` 模型参数；只有 Cursor RPC 通道才构造本控件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorModeControl {
    /// app-server 对当前 live session 报告的实际 mode；没有 live session 时必须是 None。
    pub effective: Option<String>,
    /// 盘上期望值仅在尚未与 effective 收敛时展示，不能拿它驱动高亮。
    pub pending: Option<String>,
    /// 持久偏好不可读/非法。执行层会 fail-closed，面板也必须明示而非伪装成未配置。
    pub preference_error: bool,
    pub options: Vec<CursorModeOption>,
    /// true=来自 live server catalog；false=会话尚未建立时的标准三态兜底。
    pub catalog_live: bool,
}

/// Cursor ACP 动态 `configOptions` 里的一个可选值。所有字段都来自 server，
/// 渲染时必须把它们当不可信文本；`value` 只在通过 wire 预算后才原样进 callback。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorConfigOption {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// Cursor ACP 动态配置轴。Core 不维护 fast/context/reasoning 白名单，只渲染上层
/// 筛选后传入的轴；因此 Cursor 增删选项不需要改飞书卡协议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorConfigControl {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Cursor live catalog 报告的当前生效值；只有它能驱动按钮高亮。
    pub current: Option<String>,
    /// 尚未收敛的期望值；只用于状态说明，不得伪装成已生效。
    pub pending: Option<String>,
    pub options: Vec<CursorConfigOption>,
    /// 本群覆盖了这个轴。
    pub overridden: bool,
    /// bot 默认在当前模型下的值(本群覆盖时用来写「默认 Y」)。
    pub default_value: Option<String>,
}

/// Cursor 参数化目录的整体可信状态。按钮改本群覆盖;「设为默认」把本群这套写成 bot 默认。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorConfigPanel {
    pub controls: Vec<CursorConfigControl>,
    /// 盘上期望值不可读/非法：展示状态，但不允许再写入。
    pub preference_error: bool,
    /// false 表示当前没有可信的 live Cursor catalog。
    pub catalog_live: bool,
    /// 模型尚未收敛时 configOptions 可能属于旧模型，必须先禁用点击。
    pub model_pending: bool,
    /// 模型待收敛时的原因与处理办法(上层按目标模型在不在 live 目录里写好,已转义);None 用通用说明。
    pub model_pending_note: Option<String>,
}

/// Session 卡中三条相互正交的 CoT 展示轴，值已经合并 per-chat 覆盖与 agent 默认配置。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCotControls {
    pub visible: bool,
    pub thinking: bool,
    pub tools: bool,
}

/// 会话面板上与 chat 绑定的控制项。收成结构体，避免每新增一个正交开关都扩一条长参数链。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionPanelControls {
    pub quota_badge: bool,
    /// 回复卡角标是否显示思考档位(分级开关 `badge.effort` 的本会话生效值)。
    pub badge_effort: bool,
    /// 同上,CLI 类型(`badge.cli`)。
    pub badge_cli: bool,
    /// 同上,生效模型(`badge.model`)。
    pub badge_model: bool,
    /// 同上,上下文用量(`badge.ctx`)。
    pub badge_ctx: bool,
    /// 当前 agent 账号最后一次可靠的周额度剩余百分比；采集与 `quota_badge` 展示开关解耦。
    pub quota_left_pct: Option<u8>,
    /// 同上,5 小时窗口;只有 [`crate::driver::CliDriver::has_five_hour_and_weekly_quota`]
    /// 的 CLI 显示。
    pub quota_5h_left_pct: Option<u8>,
    /// 当前会话最后一次可靠的上下文占用快照。各维度可缺，渲染侧会明确标出不可用项。
    pub context: crate::context_budget::ContextBudget,
    pub cot: SessionCotControls,
    /// 非 Codex 为 None；Codex 为 Some，即使能力未知也会展示诚实的状态说明。
    pub codex_speed: Option<CodexSpeedControl>,
    /// 非 Cursor-RPC 为 None；Cursor-RPC 始终展示会话级 mode。
    pub cursor_mode: Option<CursorModeControl>,
    /// 非 Cursor-RPC 为 None；Some 时只包含上层决定开放的动态参数轴。
    pub cursor_config: Option<CursorConfigPanel>,
    /// 会话跑在 tmux 里(claude)时为 true:面板显示「>_ 终端」。无头会话没有终端。
    pub terminal: bool,
    /// 本群设了自己的模型时,bot 默认是什么(面板「本群 X(覆盖),默认 Y」);没设时为 None。
    pub model_default: Option<crate::model_effort::ResolvedModel>,
    /// 同上,思考档位。
    pub effort_default: Option<crate::model_effort::ResolvedEffort>,
}

/// 兼容既有只关心额度角标的调用点；Codex 生产路径会显式传完整 controls。
impl From<bool> for SessionPanelControls {
    fn from(quota_badge: bool) -> Self {
        Self {
            quota_badge,
            // 只关心额度角标的老调用点不表达档位偏好;取出厂默认(开),与 `messaging.default.yaml` 一致。
            badge_effort: true,
            badge_cli: true,
            badge_model: true,
            badge_ctx: true,
            quota_left_pct: None,
            quota_5h_left_pct: None,
            context: Default::default(),
            cot: Default::default(),
            codex_speed: None,
            cursor_mode: None,
            cursor_config: None,
            terminal: false,
            model_default: None,
            effort_default: None,
        }
    }
}

/// `/model <id>` 没有切换时的回执：一句说明，加一组模型按钮(与会话面板的模型按钮同一个动作)。
/// `note` 是调用方拼好的 lark_md,里面引用的用户输入已转义。`default_layer` 为 true 时按钮改
/// bot 默认(来自 `/model default <id>`),否则改本群。
pub fn card_model_suggestions(
    note: &str,
    suggestions: &[String],
    chat_tag: &str,
    default_layer: bool,
) -> String {
    let action = if default_layer {
        "model_default"
    } else {
        "model"
    };
    use serde_json::json;
    let mut elements = vec![json!({"tag": "markdown", "content": note})];
    let models: Vec<&String> = suggestions
        .iter()
        .filter(|m| !m.trim().is_empty())
        .collect();
    for row in models.chunks(MODEL_PANEL_BTNS_PER_ROW) {
        let btns = row
            .iter()
            .map(|m| {
                cb_btn(
                    m,
                    "default",
                    json!({"action": action, "model": m, "chat": chat_tag}),
                )
            })
            .collect();
        elements.push(button_row(btns));
    }
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

#[allow(clippy::too_many_arguments)] // 各参数职责独立(标签/cli/通道/模型/effort/候选/effort候选/subagent),无合并意义
pub fn card_session_panel(
    chat_tag: &str,
    cli: &str,
    channel: &str,
    model: &crate::model_effort::ResolvedModel,
    effort: &crate::model_effort::ResolvedEffort,
    models: &[String],
    efforts: &[String],
    subagents: &crate::subagent::SubagentDisplay,
    controls: impl Into<SessionPanelControls>,
) -> String {
    use serde_json::json;
    let controls: SessionPanelControls = controls.into();
    let elements = session_panel_elements(
        chat_tag, cli, channel, model, effort, models, efforts, subagents, controls,
    );
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// 面板摘要一行「前缀:(灰字说明)」——没有具体值可显示时用(CLI 默认 / 未配置 / 随模型)。
/// `note` 全是 summon 内置串,不含外部文本。
fn panel_grey_line(prefix: &str, note: &str) -> String {
    format!("- {prefix}:<font color='grey'>{note}</font>")
}

fn panel_context_line(context: crate::context_budget::ContextBudget) -> String {
    crate::context_budget::render_usage(&context)
        .map(|usage| summon_i18n::format!("rfcef8bd617"))
        .unwrap_or_else(|| {
            panel_grey_line(
                summon_i18n::text!("r6fc38ef360"),
                summon_i18n::text!("r5c82137dc8"),
            )
        })
}

/// 一行最多放几个模型按钮。每个按钮占一个 `column`,列太多飞书会拒渲(真机 230099);
/// 也影响可读性(窄屏堆叠)。3 与生命周期行的按钮数同量级。
const MODEL_PANEL_BTNS_PER_ROW: usize = 3;
/// 按钮式模型选择的候选上限；其余候选可经 `/model <id>` 切换。
const MODEL_PANEL_BTN_CAP: usize = 12;
/// 2026-10-28 开发群 Card 2.0 实测:一段 markdown + 单选框 + placeholder，
/// 197 项可发送，198 项返回 230099/11310。选项文本也计入单卡 200 元素限制；
/// 完整面板按同一元素预算收口；三种 mode 与 context/reasoning/fast 参数的测试面板
/// 每层 43 项的关闭态可发送，44 项被拒。关闭态的 hr + markdown 需预留两个元素。
const CURSOR_MODEL_OPTION_CAP: usize = 197;
/// 会话面板里 Cursor 每个模型下拉框最多列几项。目录已按常用程度排好序(推荐段在前),
/// 面板只放开头这一截;完整目录走无参 `/model` 的选择卡。
const CURSOR_PANEL_MODEL_OPTION_CAP: usize = 32;
/// 单卡元素数的安全线(飞书上限 200,留出余量);会话面板与无参 `/model` 选择卡共用。
const SAFE_OBJECT_LIMIT: usize = 190;
/// Cursor mode 同样来自 server 动态目录；异常膨胀时不能把整张卡撑成非法 body。
const CURSOR_MODE_BTN_CAP: usize = 9;
/// 飞书 plain_text 也有尺寸边界；含省略号在内，单个动态标签最多占这些字符。
const CURSOR_MODE_LABEL_CHAR_CAP: usize = 32;
/// effective / pending id 只用于状态说明，ACP 安全 id 的上限也是 64 字符。
const CURSOR_MODE_STATE_ID_CHAR_CAP: usize = 64;
/// 给动态 mode 区域划固定 wire 预算，避免 server 目录吃掉整张会话卡的可用空间。
const CURSOR_MODE_SECTION_WIRE_BYTE_CAP: usize = 4 * 1024;

/// Cursor 参数目录也来自 server，而且 callback 必须保留完整 id/value。因此不能
/// 靠裁 callback 保卡片，只能逐项试装：放不下就不展示该按钮/轴。
const CURSOR_CONFIG_CONTROL_CAP: usize = 6;
const CURSOR_CONFIG_OPTION_CAP: usize = 9;
const CURSOR_CONFIG_LABEL_CHAR_CAP: usize = 32;
const CURSOR_CONFIG_DESCRIPTION_CHAR_CAP: usize = 192;
const CURSOR_CONFIG_STATE_CHAR_CAP: usize = 64;
const CURSOR_CONFIG_SECTION_WIRE_BYTE_CAP: usize = 4 * 1024;
const CURSOR_CONFIG_PANEL_WIRE_BYTE_CAP: usize = 10 * 1024;

/// 截断不可信的动态文本，且 `max` 包含尾部省略号本身。只扫描前 `max + 1` 个字符，
/// 避免一个异常超长 server display name 放大渲染成本。
fn bounded_cursor_mode_text(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut chars = value.chars();
    let mut bounded: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        bounded.pop();
        bounded.push('…');
    }
    bounded
}

fn cursor_mode_display_id(value: &str) -> String {
    let value = value.trim();
    let value = if value.is_empty() {
        summon_i18n::text!("r4d8c1c5b42")
    } else {
        value
    };
    escape_lark_md(&bounded_cursor_mode_text(
        value,
        CURSOR_MODE_STATE_ID_CHAR_CAP,
    ))
}

fn cursor_mode_status_note(mode: &CursorModeControl) -> String {
    if mode.preference_error {
        return summon_i18n::text!("r5c7b630f3a").to_string();
    }
    let pending = mode
        .pending
        .as_deref()
        .filter(|pending| Some(*pending) != mode.effective.as_deref());
    let pending_missing = pending.is_some_and(|pending| {
        mode.catalog_live && !mode.options.iter().any(|option| option.id == pending)
    });
    let effective = mode.effective.as_deref().map(cursor_mode_display_id);
    let pending = pending.map(cursor_mode_display_id);

    match (effective, pending) {
        (Some(effective), Some(pending)) if pending_missing => summon_i18n::format!("r4e0ce7d99b"),
        (Some(effective), Some(pending)) if mode.catalog_live => {
            summon_i18n::format!("r738d4cdbef")
        }
        (Some(effective), Some(pending)) => summon_i18n::format!("r11f9c2a473"),
        (None, Some(pending)) if pending_missing => summon_i18n::format!("r8453580bd6"),
        (None, Some(pending)) if mode.catalog_live => summon_i18n::format!("r80de6f5a54"),
        (None, Some(pending)) => summon_i18n::format!("r2c05f49909"),
        (Some(effective), None) if mode.catalog_live => {
            summon_i18n::format!("r89e0faeb99")
        }
        (Some(effective), None) => {
            summon_i18n::format!("r04a55953fa")
        }
        (None, None) if mode.catalog_live => summon_i18n::text!("r0a53d3255f").to_string(),
        (None, None) => summon_i18n::text!("rb7df4793ad").to_string(),
    }
}

fn build_cursor_mode_elements(
    mode: &CursorModeControl,
    buttons: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": summon_i18n::format!(
            // 占位符**不能写成尖括号**:同一行的 `<font>` 说明飞书在解析尖括号标签,裸 `<id>`
            // 会被当成未知标签吞掉。而 `&lt;` 也不行——飞书不解析 HTML 实体,原样吐出 `&lt;id>`
            // (2026-08-12 owner 在卡片上看到的就是它)。改用中文占位,两头的坑都躲开。
            "r0682903466",
            cursor_mode_status_note(mode)
        )
    })];
    for row in buttons.chunks(MODEL_PANEL_BTNS_PER_ROW) {
        elements.push(button_row(row.to_vec()));
    }
    if mode.options.len() > buttons.len() {
        let extra = mode.options.len() - buttons.len();
        elements.push(serde_json::json!({
            "tag": "markdown",
            "content": summon_i18n::format!(
                "r0f17845128"
            )
        }));
    }
    elements
}

fn cursor_mode_elements(mode: &CursorModeControl, chat_tag: &str) -> Vec<serde_json::Value> {
    let mut buttons = Vec::new();
    for option in mode.options.iter().take(CURSOR_MODE_BTN_CAP) {
        let raw_label = if option.name.trim().is_empty() {
            option.id.as_str()
        } else {
            option.name.trim()
        };
        let label = bounded_cursor_mode_text(raw_label, CURSOR_MODE_LABEL_CHAR_CAP);
        let button_type = if mode.effective.as_deref() == Some(option.id.as_str()) {
            "primary"
        } else {
            "default"
        };
        buttons.push(cb_btn(
            &label,
            button_type,
            serde_json::json!({"action": "cursor_mode", "mode": option.id, "chat": chat_tag}),
        ));

        let candidate = build_cursor_mode_elements(mode, &buttons);
        let fits = serde_json::to_vec(&candidate)
            .is_ok_and(|wire| wire.len() <= CURSOR_MODE_SECTION_WIRE_BYTE_CAP);
        if !fits {
            buttons.pop();
        }
    }
    build_cursor_mode_elements(mode, &buttons)
}

fn cursor_config_display_text(value: &str, max: usize, fallback: &str) -> String {
    let value = value.trim();
    let value = if value.is_empty() { fallback } else { value };
    escape_lark_md(&bounded_cursor_mode_text(value, max))
}

fn cursor_config_panel_note(panel: &CursorConfigPanel) -> String {
    if panel.preference_error {
        summon_i18n::text!("r399833d0b4").to_string()
    } else if panel.model_pending {
        panel
            .model_pending_note
            .clone()
            .unwrap_or_else(|| summon_i18n::text!("r9ac8658991").to_string())
    } else if !panel.catalog_live {
        summon_i18n::text!("r8d53dc31e6").to_string()
    } else {
        summon_i18n::text!("r4b6e84d814").to_string()
    }
}

fn cursor_config_control_note(control: &CursorConfigControl) -> String {
    let current = control.current.as_deref().map(|value| {
        cursor_config_display_text(
            value,
            CURSOR_CONFIG_STATE_CHAR_CAP,
            summon_i18n::text!("r4d8c1c5b42"),
        )
    });
    let pending = control
        .pending
        .as_deref()
        .filter(|pending| Some(*pending) != control.current.as_deref())
        .map(|value| {
            cursor_config_display_text(
                value,
                CURSOR_CONFIG_STATE_CHAR_CAP,
                summon_i18n::text!("r4d8c1c5b42"),
            )
        });
    let mut parts = Vec::new();
    if let Some(current) = current {
        parts.push(summon_i18n::format!("rec0fcb3db7"));
    } else {
        parts.push(summon_i18n::text!("r5e969c20b5").to_string());
    }
    if let Some(pending) = pending {
        parts.push(summon_i18n::format!("r964dbcb580"));
    }
    if control.overridden {
        let default = control.default_value.as_deref().map(|value| {
            cursor_config_display_text(
                value,
                CURSOR_CONFIG_STATE_CHAR_CAP,
                summon_i18n::text!("r4d8c1c5b42"),
            )
        });
        parts.push(match default {
            Some(default) => summon_i18n::format!("r6c930fffa0"),
            None => summon_i18n::text!("rbba20088ac").to_string(),
        });
    } else {
        parts.push(summon_i18n::text!("ra44f9f9fca").to_string());
    }
    if let Some(description) = control
        .description
        .as_deref()
        .filter(|description| !description.trim().is_empty())
    {
        parts.push(cursor_config_display_text(
            description,
            CURSOR_CONFIG_DESCRIPTION_CHAR_CAP,
            "",
        ));
    }
    if control.id == "fast" {
        // Cursor 目录的描述可变，成本风险不能依赖 server 恰好给出某句文案。
        parts.push(summon_i18n::text!("r1d08618fa0").to_string());
    }
    format!("{}。", parts.join("；"))
}

fn build_cursor_config_control_section(
    control: &CursorConfigControl,
    buttons: &[serde_json::Value],
    interactive: bool,
) -> Vec<serde_json::Value> {
    let raw_name = if control.name.trim().is_empty() {
        control.id.as_str()
    } else {
        control.name.as_str()
    };
    let name = cursor_config_display_text(
        raw_name,
        CURSOR_CONFIG_LABEL_CHAR_CAP,
        summon_i18n::text!("r5cd6061f4c"),
    );
    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": format!(
            "**{name}**:<font color='grey'>{}</font>",
            cursor_config_control_note(control)
        )
    })];
    for row in buttons.chunks(MODEL_PANEL_BTNS_PER_ROW) {
        elements.push(button_row(row.to_vec()));
    }
    if interactive && control.options.len() > buttons.len() {
        elements.push(serde_json::json!({
            "tag": "markdown",
            "content": summon_i18n::format!(
                "rf7fee4b87f",
                control.options.len() - buttons.len()
            )
        }));
    } else if !interactive {
        elements.push(serde_json::json!({
            "tag": "markdown",
            "content": summon_i18n::text!("rd7c991c092")
        }));
    }
    elements
}

fn cursor_config_control_section(
    control: &CursorConfigControl,
    chat_tag: &str,
    interactive: bool,
) -> Vec<serde_json::Value> {
    let mut buttons = Vec::new();
    if interactive && !control.id.trim().is_empty() {
        for option in control
            .options
            .iter()
            .filter(|option| !option.value.trim().is_empty())
            .take(CURSOR_CONFIG_OPTION_CAP)
        {
            let raw_label = if option.name.trim().is_empty() {
                option.value.as_str()
            } else {
                option.name.as_str()
            };
            let label = bounded_cursor_mode_text(raw_label, CURSOR_CONFIG_LABEL_CHAR_CAP);
            let button_type = if control.current.as_deref() == Some(option.value.as_str()) {
                "primary"
            } else {
                "default"
            };
            buttons.push(cb_btn(
                &label,
                button_type,
                serde_json::json!({
                    "action": "cursor_config",
                    "config_id": control.id,
                    "value": option.value,
                    "chat": chat_tag,
                }),
            ));
            let candidate = build_cursor_config_control_section(control, &buttons, interactive);
            let fits = serde_json::to_vec(&candidate)
                .is_ok_and(|wire| wire.len() <= CURSOR_CONFIG_SECTION_WIRE_BYTE_CAP);
            if !fits {
                buttons.pop();
                break;
            }
        }
        if control.overridden {
            buttons.push(cb_btn(
                summon_i18n::text!("ra44f9f9fca"),
                "default",
                serde_json::json!({
                    "action": "cursor_config",
                    "config_id": control.id,
                    "inherit": true,
                    "chat": chat_tag,
                }),
            ));
        }
    }
    build_cursor_config_control_section(control, &buttons, interactive)
}

/// 首段是全局副作用说明，后续每段恰好对应一个动态轴，便于整轴做 wire 预算取舍。
fn cursor_config_panel_sections(
    panel: &CursorConfigPanel,
    chat_tag: &str,
) -> Vec<Vec<serde_json::Value>> {
    let interactive = panel.catalog_live && !panel.preference_error && !panel.model_pending;
    let mut axis_sections = Vec::new();
    for control in panel.controls.iter().take(CURSOR_CONFIG_CONTROL_CAP) {
        let section = cursor_config_control_section(control, chat_tag, interactive);
        let mut candidate = axis_sections.clone();
        candidate.push(section.clone());
        // 给前置全局说明和“未列出”数量留固定余量，避免最后一个轴卡死整段预算。
        let fits = serde_json::to_vec(&candidate)
            .is_ok_and(|wire| wire.len() <= CURSOR_CONFIG_PANEL_WIRE_BYTE_CAP - 512);
        if !fits {
            break;
        }
        axis_sections.push(section);
    }
    let omitted = panel.controls.len().saturating_sub(axis_sections.len());
    let omitted_note = if omitted == 0 {
        String::new()
    } else {
        summon_i18n::format!("r4d4769be9a")
    };
    let mut intro = vec![serde_json::json!({
        "tag": "markdown",
        "content": summon_i18n::format!(
            "r8d3a54d9eb",
            cursor_config_panel_note(panel),
            omitted_note
        )
    })];
    if interactive {
        intro.push(button_row(vec![cb_btn(
            summon_i18n::text!("r9a4ee059bf"),
            "default",
            serde_json::json!({"action": "cursor_config_default", "chat": chat_tag}),
        )]));
    }
    let mut sections = vec![intro];
    sections.extend(axis_sections);
    sections
}

/// 「默认 Y」那半句:有具体值写值和来源,没有写「由 CLI 自定」。
fn layered_default_text(value: Option<&str>, source: &str) -> String {
    match value {
        Some(value) => format!(
            "`{}` <font color='grey'>({source})</font>",
            escape_lark_md(value)
        ),
        None => summon_i18n::text!("r7140928934").to_string(),
    }
}

/// 一组候选按钮,按行分栏;与 `selected` 相同的那个高亮。
fn push_layered_rows(
    elements: &mut Vec<serde_json::Value>,
    values: &[&String],
    selected: Option<&str>,
    value: impl Fn(&str) -> serde_json::Value,
) {
    for row in values.chunks(MODEL_PANEL_BTNS_PER_ROW) {
        let btns = row
            .iter()
            .map(|v| {
                let ty = if Some(v.as_str()) == selected {
                    "primary"
                } else {
                    "default"
                };
                cb_btn(v, ty, value(v))
            })
            .collect();
        elements.push(button_row(btns));
    }
}

fn model_dropdown(
    models: &[&String],
    selected: Option<&str>,
    action: &str,
    chat: &str,
) -> serde_json::Value {
    use serde_json::json;
    let mut select = json!({
        "tag": "select_static",
        "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("r1afed6a81a")},
        "width": "fill",
        "options": models.iter().map(|model| json!({
            "text": {"tag": "plain_text", "content": model}, "value": model
        })).collect::<Vec<_>>(),
        "behaviors": [{"type": "callback", "value": {"action": action, "chat": chat}}]
    });
    if let Some(selected) =
        selected.filter(|selected| models.iter().any(|model| model.as_str() == *selected))
    {
        select["initial_option"] = selected.into();
    }
    select
}

/// 取下拉框要列的前 `limit` 项;当前选中的模型排在外面时,用它顶替最后一项,
/// 让下拉框仍能高亮当前值、也能原样选回。总数不超过 `limit`。
fn capped_model_options<'a>(
    models: &[&'a String],
    limit: usize,
    selected: Option<&str>,
) -> Vec<&'a String> {
    let limit = limit.min(models.len());
    let mut shown: Vec<&'a String> = models.iter().take(limit).copied().collect();
    let outside = selected.and_then(|selected| {
        models
            .iter()
            .skip(limit)
            .find(|model| model.as_str() == selected)
            .copied()
    });
    if let Some(outside) = outside {
        if shown.pop().is_some() {
            shown.push(outside);
        }
    }
    shown
}

/// 无参 `/model`(或 `/model default`)给 Cursor 发的选择卡:一个下拉框列出完整目录(已排好序)。
/// 选中即走与会话面板下拉框相同的 `model` / `model_default` 回调,鉴权与校验沿用既有处理。
/// 选项数先按 [`CURSOR_MODEL_OPTION_CAP`] 截,再按整卡(含关闭按钮)的元素数安全线与体积收口;
/// 截掉的部分提示改用 `/model <id>`。
pub fn card_cursor_model_picker(
    models: &[String],
    selected: Option<&str>,
    chat_tag: &str,
    default_layer: bool,
) -> String {
    use serde_json::json;
    let mut seen = std::collections::HashSet::new();
    let models: Vec<&String> = models
        .iter()
        .filter(|m| !m.trim().is_empty() && seen.insert(m.as_str()))
        .collect();
    let total = models.len();
    let (header, action) = if default_layer {
        (summon_i18n::format!("r1a96031a27"), "model_default")
    } else {
        (summon_i18n::format!("rb3ed27d837"), "model")
    };
    let build = |limit: usize| {
        let options = capped_model_options(&models, limit, selected);
        let mut elements = vec![json!({"tag": "markdown", "content": header})];
        if !options.is_empty() {
            elements.push(model_dropdown(&options, selected, action, chat_tag));
        }
        if total > options.len() {
            let extra = total - options.len();
            elements.push(json!({
                "tag": "markdown",
                "content": summon_i18n::format!("r735865b2b5")
            }));
        }
        closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
    };
    let fits = |card: &str| {
        card.len() <= CARD_TOTAL_SAFE_LIMIT
            && serde_json::from_str::<serde_json::Value>(card)
                .is_ok_and(|value| tagged_object_count(&value) <= SAFE_OBJECT_LIMIT)
    };
    let cap = CURSOR_MODEL_OPTION_CAP.min(total);
    for limit in (1..=cap).rev() {
        let card = build(limit);
        if fits(&card) {
            return card;
        }
    }
    build(0)
}

/// 卡片里带 `tag` 的对象个数,即飞书计入单卡元素上限的那些。
fn tagged_object_count(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => {
            usize::from(map.contains_key("tag"))
                + map.values().map(tagged_object_count).sum::<usize>()
        }
        serde_json::Value::Array(values) => values.iter().map(tagged_object_count).sum(),
        _ => 0,
    }
}

fn session_panel_elements_fit_card_budget(elements: &[serde_json::Value], cli: &str) -> bool {
    fn object_count(value: &serde_json::Value, tagged_only: bool) -> usize {
        match value {
            serde_json::Value::Object(map) => {
                usize::from(!tagged_only || map.contains_key("tag"))
                    + map
                        .values()
                        .map(|v| object_count(v, tagged_only))
                        .sum::<usize>()
            }
            serde_json::Value::Array(values) => {
                values.iter().map(|v| object_count(v, tagged_only)).sum()
            }
            _ => 0,
        }
    }
    let card = serde_json::json!({"schema": "2.0", "body": {"elements": elements}});
    // Cursor 的单卡 200 元素预算包含选项文本，关闭态追加 hr + markdown 两个元素。
    let element_limit = if cli == "cursor" {
        198
    } else {
        SAFE_OBJECT_LIMIT
    };
    object_count(&card, cli == "cursor") <= element_limit
        && serde_json::to_vec(&card).is_ok_and(|wire| wire.len() <= CARD_TOTAL_SAFE_LIMIT)
}

/// 会话面板的元素列表(摘要 + 模型按钮 + 生命周期按钮 + 活跃子代理 + work_dir 表单)。
/// 抽出来给 `card_session_panel`(交互态)与 `card_session_closed`(禁用态)共用,保证两者内容一致。
#[allow(clippy::too_many_arguments)] // 同上,各参数独立渲染一段,合并会降低可读性
fn session_panel_elements(
    chat_tag: &str,
    cli: &str,
    channel: &str,
    model: &crate::model_effort::ResolvedModel,
    effort: &crate::model_effort::ResolvedEffort,
    models: &[String],
    efforts: &[String],
    subagents: &crate::subagent::SubagentDisplay,
    controls: SessionPanelControls,
) -> Vec<serde_json::Value> {
    // Cursor 两层下拉框共享条数预算，优先保留动态参数控件。
    // 按钮式面板优先保留本群操作，默认层可经 `/model default <id>` 设置。
    let model_cap = if cli == "cursor" {
        CURSOR_PANEL_MODEL_OPTION_CAP
    } else {
        MODEL_PANEL_BTN_CAP
    }
    .min(models.len());
    let (full, complete) = session_panel_elements_with(
        chat_tag,
        cli,
        channel,
        model,
        effort,
        models,
        efforts,
        subagents,
        controls.clone(),
        true,
        model_cap,
    );
    if complete && session_panel_elements_fit_card_budget(&full, cli) {
        return full;
    }
    let mut partial = None;
    for model_limit in (0..=model_cap).rev() {
        let (elements, complete) = session_panel_elements_with(
            chat_tag,
            cli,
            channel,
            model,
            effort,
            models,
            efforts,
            subagents,
            controls.clone(),
            cli == "cursor",
            model_limit,
        );
        if session_panel_elements_fit_card_budget(&elements, cli) {
            if cli != "cursor" || complete {
                return elements;
            }
            partial.get_or_insert(elements);
        }
    }
    if let Some(elements) = partial {
        return elements;
    }
    vec![md_el(summon_i18n::text!("rad5ed92dea"))]
}

/// 返回元素与「Cursor 参数段是否全部放下」。
#[allow(clippy::too_many_arguments)]
fn session_panel_elements_with(
    chat_tag: &str,
    cli: &str,
    channel: &str,
    model: &crate::model_effort::ResolvedModel,
    effort: &crate::model_effort::ResolvedEffort,
    models: &[String],
    efforts: &[String],
    subagents: &crate::subagent::SubagentDisplay,
    controls: SessionPanelControls,
    default_rows: bool,
    model_limit: usize,
) -> (Vec<serde_json::Value>, bool) {
    use crate::model_effort::EffortSource;
    use serde_json::json;
    // `controls` 后续按字段逐个 move 出去(context/codex_speed/…),提前取出这个 Copy 字段,
    // 避免用到它时已经借不到整个结构体。
    let show_terminal = controls.terminal;
    // 模型行:设了本群模型写「本群 X(覆盖),默认 Y」,没设写「跟随默认 Y」;默认是 CLI 自定时
    // 没有具体 id,灰字「由 CLI 自定」,不显示假「未指定」。
    let model_overridden = model.source == crate::model_effort::ModelSource::ChatOverride;
    let model_default = controls.model_default.clone();
    let model_line = match (model_overridden, &model_default) {
        (true, Some(default)) => summon_i18n::format!(
            "r029d33db32",
            escape_lark_md(model.value.as_deref().unwrap_or("")),
            layered_default_text(default.value.as_deref(), default.source.label())
        ),
        _ => match &model.value {
            Some(v) => summon_i18n::format!("r99c93928d3", escape_lark_md(v), model.source.label()),
            None => panel_grey_line(
                summon_i18n::text!("rc98e118e0a"),
                summon_i18n::text!("r5c0a4c6c82"),
            ),
        },
    };
    // effort 行同理。cursor/Other 的 effort 烤在 model 名里 → 标「随模型」而非假「默认」。
    let effort_overridden = effort.source == EffortSource::ChatOverride;
    let effort_default = controls.effort_default.clone();
    let effort_line = match effort.source {
        EffortSource::ModelInherent => panel_grey_line("effort", summon_i18n::text!("rc01d5e0d1f")),
        EffortSource::CursorParameter => match &effort.value {
            Some(value) => summon_i18n::format!("rdedfb92b4e", escape_lark_md(value)),
            None => panel_grey_line("effort", summon_i18n::text!("r3b92aab1a6")),
        },
        EffortSource::CursorParameterUnknown => {
            panel_grey_line("effort", summon_i18n::text!("r0c93509650"))
        }
        EffortSource::ChatOverride => summon_i18n::format!(
            "r983cef66e6",
            escape_lark_md(effort.value.as_deref().unwrap_or("")),
            match &effort_default {
                Some(default) =>
                    layered_default_text(default.value.as_deref(), default.source.label()),
                None => summon_i18n::text!("reed209d203").to_string(),
            }
        ),
        EffortSource::AgentConfig => summon_i18n::format!(
            "r13d03670c2",
            escape_lark_md(effort.value.as_deref().unwrap_or("")),
            effort.source.label()
        ),
        EffortSource::CliDefault => panel_grey_line("effort", summon_i18n::text!("r70d37880a7")),
    };
    // 工作目录行(A/隐私):**绝不显示路径**——群面板是群可见面,连「当前路径」都不进群。只放引导语,
    // 查看 / 切换都走「私信候选目录」私信选择器(路径只进私信)。
    let wd_line = panel_grey_line(
        summon_i18n::text!("r3db7b06b5f"),
        summon_i18n::text!("r393c76a50b"),
    );
    let context_line = panel_context_line(controls.context);
    // B(精简):cli 与通道并一行(`- `cursor` · RPC · ACP 常驻`)——两者都是「跑在哪条通道上」的
    // 同一件事,拆两行偏密;其余行仍各自保留生效值 + 来源。
    let feed_lead = summon_i18n::format!("r75c96e81aa");
    let details = format!("{model_line}\n{effort_line}\n{context_line}\n{wd_line}");
    let mut elements: Vec<serde_json::Value> = vec![
        json!({"tag": "markdown", "content": feed_lead}),
        json!({"tag": "markdown", "content": details}),
    ];
    elements.push(json!({"tag": "hr"}));
    elements.push(json!({"tag": "markdown", "content": summon_i18n::text!("r615b8335c7")}));

    // Cursor 目录保持上游顺序；两层选择共用完整 id，回调由 sidecar 归一到 model 动作。
    let mut seen = std::collections::HashSet::new();
    let model_btns: Vec<&String> = models
        .iter()
        .filter(|m| !m.trim().is_empty() && (cli != "cursor" || seen.insert(m.as_str())))
        .collect();
    if !model_btns.is_empty() {
        let shown = model_btns.len().min(model_limit);
        let default_value = if model_overridden {
            model_default.as_ref().and_then(|d| d.value.clone())
        } else {
            model.value.clone()
        };
        elements.push(json!({"tag": "markdown", "content": summon_i18n::text!("rab77270602")}));
        if cli == "cursor" {
            if shown > 0 {
                elements.push(model_dropdown(
                    &capped_model_options(&model_btns, shown, model.value.as_deref()),
                    model.value.as_deref(),
                    "model",
                    chat_tag,
                ));
            }
        } else {
            push_layered_rows(
                &mut elements,
                &model_btns[..shown],
                model.value.as_deref(),
                |m| json!({"action": "model", "model": m, "chat": chat_tag}),
            );
        }
        if model_overridden {
            elements.push(button_row(vec![cb_btn(
                summon_i18n::text!("ra44f9f9fca"),
                "default",
                json!({"action": "model", "inherit": true, "chat": chat_tag}),
            )]));
        }
        if model_btns.len() > shown {
            let extra = model_btns.len() - shown;
            let total = model_btns.len();
            elements.push(json!({
                "tag": "markdown",
                "content": if cli == "cursor" {
                    summon_i18n::format!("rea56fde596")
                } else {
                    summon_i18n::format!("r3215c530e5")
                }
            }));
        }
        if default_rows {
            elements.push(json!({
                "tag": "markdown",
                "content": summon_i18n::text!("r6fb4db6595")
            }));
            if cli == "cursor" {
                if shown > 0 {
                    elements.push(model_dropdown(
                        &capped_model_options(&model_btns, shown, default_value.as_deref()),
                        default_value.as_deref(),
                        "model_default",
                        chat_tag,
                    ));
                }
            } else {
                push_layered_rows(
                    &mut elements,
                    &model_btns[..shown],
                    default_value.as_deref(),
                    |m| json!({"action": "model_default", "model": m, "chat": chat_tag}),
                );
            }
        }
    }

    // effort 档位:只对有独立 effort 轴的 cli(claude/codex,cursor/Other 的 efforts 为空)渲染。
    // 候选同样来自 `options_for(cli).efforts`(messaging.efforts ∩ cli.legal_efforts)。
    if !efforts.is_empty() {
        let effort_btns: Vec<&String> = efforts.iter().collect();
        let default_value = if effort_overridden {
            effort_default.as_ref().and_then(|d| d.value.clone())
        } else {
            effort.value.clone()
        };
        elements.push(json!({"tag": "markdown", "content": summon_i18n::text!("rf28eec8824")}));
        push_layered_rows(
            &mut elements,
            &effort_btns,
            effort.value.as_deref(),
            |e| json!({"action": "effort", "effort": e, "chat": chat_tag}),
        );
        if effort_overridden {
            elements.push(button_row(vec![cb_btn(
                summon_i18n::text!("ra44f9f9fca"),
                "default",
                json!({"action": "effort", "inherit": true, "chat": chat_tag}),
            )]));
        }
        if default_rows {
            elements.push(json!({
                "tag": "markdown",
                "content": summon_i18n::text!("r693bcb9149")
            }));
            push_layered_rows(
                &mut elements,
                &effort_btns,
                default_value.as_deref(),
                |e| json!({"action": "effort_default", "effort": e, "chat": chat_tag}),
            );
        }
    }

    // Fast 是延迟/额度的 per-chat 选择，不是模型或 agent 身份。只对 Codex 渲染；模型目录明确
    // 不支持或无法确认时都不产出可点击 Fast，避免「卡片说开、Codex 静默回落」的假成功；
    // Standard 永远保留，给已经持久 Fast、后来换到不支持模型的会话一条明确退出路径。
    if let Some(speed) = controls.codex_speed {
        let standard_ty = if speed.selected == Some(crate::driver::CodexServiceTier::Standard) {
            "primary"
        } else {
            "default"
        };
        let standard_button = || {
            cb_btn(
                summon_i18n::text!("r6bea77acef"),
                standard_ty,
                json!({"action": "codex_service_tier", "mode": "default", "chat": chat_tag}),
            )
        };
        match speed.supported {
            Some(true) => {
                let inherited = speed.selected.is_none();
                elements.push(json!({
                    "tag": "markdown",
                    "content": if inherited {
                        summon_i18n::text!("red6de00a1c")
                    } else {
                        summon_i18n::text!("r68210f52c8")
                    }
                }));
                let fast_ty = if speed.selected == Some(crate::driver::CodexServiceTier::Fast) {
                    "primary"
                } else {
                    "default"
                };
                elements.push(button_row(vec![
                    standard_button(),
                    cb_btn(
                        "Fast",
                        fast_ty,
                        json!({"action": "codex_service_tier", "mode": "fast", "chat": chat_tag}),
                    ),
                ]));
            }
            Some(false) => {
                elements.push(json!({
                    "tag": "markdown",
                    "content": if speed.selected == Some(crate::driver::CodexServiceTier::Fast) {
                        summon_i18n::text!("r78df0e71d7")
                    } else {
                        summon_i18n::text!("r588ac0841b")
                    }
                }));
                elements.push(button_row(vec![standard_button()]));
            }
            None => {
                elements.push(json!({
                    "tag": "markdown",
                    "content": if speed.selected == Some(crate::driver::CodexServiceTier::Fast) {
                        summon_i18n::text!("r6a0b27efab")
                    } else {
                        summon_i18n::text!("r4a7389647f")
                    }
                }));
                elements.push(button_row(vec![standard_button()]));
            }
        }
    }

    // Cursor mode 是标准 ACP 的**当前 session**状态，安全地按 chat 隔离。effective 只认 live
    // server 状态；盘上 desired 仅作为 pending 说明，不能高亮成已经生效。
    if let Some(mode) = &controls.cursor_mode {
        elements.extend(cursor_mode_elements(mode, chat_tag));
    }
    // 参数轴最终会插在 mode 后面；先记住位置，等其它固定区域都构造完再用
    // 整卡实际 JSON 试装，否则只算局部 section 仍可能越过飞书总上限。
    let cursor_config_insert_at = elements.len();

    let cot_button = |label: &str, axis: &str, enabled: bool| {
        cb_btn(
            &format!(
                "{label}:{}",
                if enabled {
                    summon_i18n::text!("r39eae64cfc")
                } else {
                    summon_i18n::text!("r5d0ae622f6")
                }
            ),
            if enabled { "primary" } else { "default" },
            json!({"action": "session_cot", "axis": axis, "on": !enabled, "chat": chat_tag}),
        )
    };
    elements.push(json!({
        "tag": "markdown",
        "content": summon_i18n::text!("r779fa6c5e6")
    }));
    elements.push(button_row(vec![
        cot_button("CoT", "visible", controls.cot.visible),
        cot_button(
            summon_i18n::text!("r3de8c943a2"),
            "thinking",
            controls.cot.thinking,
        ),
        cot_button(
            summon_i18n::text!("rdbb894e3cd"),
            "tools",
            controls.cot.tools,
        ),
    ]));

    elements.push(json!({"tag": "hr"}));
    elements.push(json!({"tag": "markdown", "content": summon_i18n::text!("r3853b40558")}));

    // 额度显示开关:回复卡角标的额度项是否渲染,per-chat 持久,**默认关**(额度是敏感面,
    // 群卡默认不露;owner 2026-08-10)。按钮标签即当前态,点击发相反值;开态高亮(primary),
    // 与模型/effort 按钮的高亮语义一致。
    let quota_content = if controls.quota_badge {
        let windows: Vec<String> =
            if crate::driver::CliDriver::from_kind(cli).has_five_hour_and_weekly_quota() {
                [
                    controls
                        .quota_5h_left_pct
                        .map(|pct| format!("5h **{}%**", pct.min(100))),
                    controls
                        .quota_left_pct
                        .map(|pct| summon_i18n::format!("rad699b4e14", pct.min(100))),
                ]
                .into_iter()
                .flatten()
                .collect()
            } else {
                Vec::new()
            };
        match (windows.is_empty(), controls.quota_left_pct) {
            (false, _) => summon_i18n::format!("r6fa079d022", windows.join(" · ")),
            (true, Some(pct)) => summon_i18n::format!("r0890c54b6c", pct.min(100)),
            (true, None) => summon_i18n::text!("r5b176f26f7").to_string(),
        }
    } else {
        summon_i18n::text!("r66660b0672").to_string()
    };
    elements.push(json!({"tag": "markdown", "content": quota_content}));
    elements.push(button_row(vec![cb_btn(
        if controls.quota_badge {
            summon_i18n::text!("rd1385d5e87")
        } else {
            summon_i18n::text!("r059f090388")
        },
        if controls.quota_badge {
            "primary"
        } else {
            "default"
        },
        json!({"action": "quota_badge", "on": !controls.quota_badge, "chat": chat_tag}),
    )]));

    // 角标四项(CLI / 模型 / 档位 / 上下文用量)的显示开关,与额度同属角标显示项,**默认开**
    // (都不敏感,只是嫌噪音的人要能关)。一行四个,按钮标签即当前态、开态高亮,点击发相反值;
    // 每项一个独立 action,沿用 `badge_effort` 的回调与鉴权。按钮收整句标签:模型那一项的
    // 英文要单数,不能借用候选列表标题那条复数文案。
    let badge_state = |name: &str, enabled: bool| {
        format!(
            "{name}:{}",
            if enabled {
                summon_i18n::text!("r39eae64cfc")
            } else {
                summon_i18n::text!("r5d0ae622f6")
            }
        )
    };
    let badge_button = |label: String, action: &str, enabled: bool| {
        cb_btn(
            &label,
            if enabled { "primary" } else { "default" },
            json!({"action": action, "on": !enabled, "chat": chat_tag}),
        )
    };
    let model_label = if controls.badge_model {
        summon_i18n::text!("re2c2d63a69")
    } else {
        summon_i18n::text!("ra84c0e33d5")
    };
    elements.push(json!({
        "tag": "markdown",
        "content": summon_i18n::text!("r326cb98953")
    }));
    elements.push(button_row(vec![
        badge_button(
            badge_state("cli", controls.badge_cli),
            "badge_cli",
            controls.badge_cli,
        ),
        badge_button(model_label.to_string(), "badge_model", controls.badge_model),
        badge_button(
            badge_state(summon_i18n::text!("r5be7a415a7"), controls.badge_effort),
            "badge_effort",
            controls.badge_effort,
        ),
        badge_button(
            badge_state("ctx", controls.badge_ctx),
            "badge_ctx",
            controls.badge_ctx,
        ),
    ]));

    // 归档关闭自动续聊入口，退出保留自动续聊入口。
    // / compact。保活只属于空闲提醒卡片,不放在主动唤出的 session 面板里。
    elements.push(json!({"tag": "markdown", "content": summon_i18n::text!("rc188b2173e")}));
    elements.push(button_row(vec![
        cb_btn_confirm(
            summon_i18n::text!("rdc2e173490"),
            "danger",
            json!({"action": "reset", "chat": chat_tag}),
            summon_i18n::text!("r841fc7a89f"),
        ),
        cb_btn(
            summon_i18n::text!("r498e1d59b4"),
            "default",
            json!({"action": "clean", "chat": chat_tag}),
        ),
        cb_btn(
            "compact",
            "default",
            json!({"action": "compact", "chat": chat_tag}),
        ),
    ]));
    if show_terminal {
        elements.push(button_row(vec![cb_btn(
            summon_i18n::text!("rd5719f8955"),
            "default",
            json!({"action": "term_open", "chat": chat_tag}),
        )]));
    }

    // 活跃子代理:展示当前 session spawn 的 subagents(Running/Spawning 实时显示,
    // Completed 30s 内保留,Failed 30min 内保留;历史条目由状态机按事件时间戳过滤,
    // 这里拿到什么渲什么)。让用户不 attach tmux 也能瞟一眼就知道哪些子任务在跑、跑完没。
    // workflow 成组:组头「▸ workflow <名> · 状态 · 阶段共 y · N 跑 M 完」,成员缩进其下
    // (阶段**进行位置**盘上无记录,只渲总数,不猜)。
    if !subagents.is_empty() {
        let now = chrono::Utc::now();
        let mut lines: Vec<String> = vec![summon_i18n::text!("raa6865db67").into()];
        for sa in &subagents.agents {
            lines.push(subagent_panel_line(sa, now, false));
        }
        for wf in &subagents.workflows {
            lines.push(format!("- ▸ {}", escape_lark_md(&wf.header_line(now))));
            for child in &wf.children {
                lines.push(subagent_panel_line(child, now, true));
            }
        }
        elements.push(json!({"tag": "markdown", "content": lines.join("\n")}));
    }

    // 「改工作目录」私信候选:触发 pick_workdir → 主动私聊操作者一张候选列表卡(同群>本agent>fleet
    // 去重排序,路径**只进私信不进群**)。候选在点击时扫盘现算,故此按钮恒渲染;空集在私信侧降级提示。
    //
    // **A(隐私)去掉了群卡里的 work_dir 手打表单**:手打表单的 `default_value` 会把当前路径回填进群卡、
    // 提交回执又带路径,二者都让路径进群。改工作目录**一律经私信选择器**(路径只进私信),群里只留这个
    // 入口按钮。手打兜底仍在——回本会话发 `/cd <路径>`(私聊/本会话,不进群展示面)。
    elements.push(json!({"tag": "markdown", "content": summon_i18n::text!("r5d9c963494")}));
    elements.push(button_row(vec![cb_btn(
        summon_i18n::text!("rbfbe742486"),
        "default",
        json!({"action": "pick_workdir", "chat": chat_tag}),
    )]));

    elements.push(json!({
        "tag": "markdown",
        "content": summon_i18n::text!("r1317190bc5")
    }));

    elements.push(button_row(vec![
        cb_btn(
            summon_i18n::text!("raee8874341"),
            "default",
            json!({"action": "session_refresh", "chat": chat_tag}),
        ),
        cb_btn(
            summon_i18n::text!("r5242df9b30"),
            "default",
            json!({"action": "session_close", "chat": chat_tag}),
        ),
    ]));

    let mut complete = true;
    if let Some(panel) = &controls.cursor_config {
        let mut insert_at = cursor_config_insert_at;
        for section in cursor_config_panel_sections(panel, chat_tag) {
            let mut candidate = elements.clone();
            candidate.splice(insert_at..insert_at, section.iter().cloned());
            if !session_panel_elements_fit_card_budget(&candidate, cli) {
                complete = false;
                break;
            }
            insert_at += section.len();
            elements = candidate;
        }
    }

    (elements, complete)
}

/// 面板上一条 subagent 的渲染行。`indent`=workflow 组成员(缩进 + ↳ 标出从属)。
/// description / model 是外部文本 → 转义;「(model)」拿不到就不显示,不猜。
///
/// 运行中条目追加「最后活跃 HH:MM:SS」(来自子代理转录 mtime;本地时区,与「更新于」
/// 时间戳同风格):started_at 只说明起了多久,last_activity 才说明它最近何时还在动——
/// 把「轮内在跑 / 僵尸 / 陈旧」区分开。刻意用绝对时刻而非「Ys 前」:面板只在 mtime
/// 量化推进时 PATCH,子代理停写后不再刷新,相对文案会永久停在「活跃 0s 前」说谎。
fn subagent_panel_line(
    sa: &crate::subagent::SubagentStatus,
    now: chrono::DateTime<chrono::Utc>,
    indent: bool,
) -> String {
    let emoji = match sa {
        crate::subagent::SubagentStatus::Spawning { .. } => "🟡",
        crate::subagent::SubagentStatus::Running { .. } => "🔄",
        crate::subagent::SubagentStatus::Completed { .. } => "✅",
        crate::subagent::SubagentStatus::Failed { .. } => "❌",
    };
    let desc = escape_lark_md(sa.description());
    let model = sa
        .model()
        .map(|m| format!(" ({})", escape_lark_md(crate::subagent::short_model(m))))
        .unwrap_or_default();
    let elapsed = sa.elapsed_human(now);
    let prefix = if indent { "  - ↳" } else { "-" };
    match sa.activity_human(&chrono::Local) {
        Some(act) => format!("{prefix} {emoji} {desc}{model} ({elapsed} · {act})"),
        None => format!("{prefix} {emoji} {desc}{model} ({elapsed})"),
    }
}

/// 会被「终态置灰」覆盖的交互控件 tag。**新增一种交互控件就必须登记进来**——漏一个,那个控件在
/// 终态卡上仍然点得动,而它背后的动作已经做完了(重复提交、或拿一张旧卡去改已经改过的东西)。
const INTERACTIVE_TAGS: [&str; 6] = [
    "button",
    "input",
    "picker_time",
    "date_picker",
    "multi_select_static",
    "select_static",
];

/// 把元素树里所有交互控件置灰(`disabled: true`),递归进 column_set / form 等容器。
/// 终态卡「保留原貌、只让控件不可再点」用。
fn disable_interactive(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            if matches!(map.get("tag"), Some(serde_json::Value::String(t)) if INTERACTIVE_TAGS.contains(&t.as_str()))
            {
                map.insert("disabled".to_string(), serde_json::Value::Bool(true));
            }
            for child in map.values_mut() {
                disable_interactive(child);
            }
        }
        serde_json::Value::Array(arr) => arr.iter_mut().for_each(disable_interactive),
        _ => {}
    }
}

/// 会话退出后的终态卡片:**保留面板原貌**,只把所有可交互控件置灰,底部加分隔线 + 「你已交互」footer。
/// `reset=true` = 归档(下次新建);`false` = 退出(下次续聊)。
/// **必须用 schema 2.0**:会话面板是 2.0,飞书 PATCH 不允许 v2→v1 换 schema。
// 参数即面板渲染字段(cli/通道/model/effort/候选)+ reset 标志,逐项透传给共享的
// session_panel_elements;拆 struct 只为凑参数数反而割裂调用点,这里按面板字段平铺。
// 与面板一致:终态卡也**不带工作目录路径**(它同样是群可见面,路径不进群)。
#[allow(clippy::too_many_arguments)]
pub fn card_session_closed(
    chat_tag: &str,
    cli: &str,
    channel: &str,
    model: &crate::model_effort::ResolvedModel,
    effort: &crate::model_effort::ResolvedEffort,
    models: &[String],
    efforts: &[String],
    controls: impl Into<SessionPanelControls>,
    reset: bool,
) -> String {
    use serde_json::json;
    let controls: SessionPanelControls = controls.into();
    let mut elements = session_panel_elements(
        chat_tag,
        cli,
        channel,
        model,
        effort,
        models,
        efforts,
        &crate::subagent::SubagentDisplay::default(), // 会话已退出,不展示 subagent
        controls,
    );
    elements.iter_mut().for_each(disable_interactive);
    let tail = if reset {
        summon_i18n::text!("ra0d93e92ae")
    } else {
        summon_i18n::text!("reb68f88592")
    };
    elements.push(json!({"tag": "hr"}));
    elements.push(json!({"tag": "markdown", "content": summon_i18n::format!("r7452aa3f57")}));
    with_outcome_feed_summary(
        json!({"schema": "2.0", "body": {"elements": elements}}),
        if reset {
            summon_i18n::text!("r82395d4230")
        } else {
            summon_i18n::text!("r899df7e244")
        },
    )
}

/// 私聊给触发者的终端卡。链接只放在跳转按钮上;回调按钮只带绑定信息,换新链接时由 daemon 校验。
pub struct TerminalCard<'a> {
    pub agent: &'a str,
    pub chat_label: &'a str,
    pub read_only_url: &'a str,
    pub operate_url: &'a str,
    pub target_chat_id: &'a str,
    pub target_chat_type: &'a str,
    pub recipient_open_id: &'a str,
}

/// 群名是完全不可控的外部文本,可能构造出 `<CJK>` 这种被通用 `escape_lark_md` 判定「标签名非法
/// 侥幸留存」而不处理的形态(该判断对普通展示场景成立:反正飞书不当标签解析,原样显示不丢字)。
/// 但这张卡紧挨着真实终端链接、面向单个收件人,不承受「侥幸」的容错空间——不管 `<` 后面跟什么
/// 一律拆散,比通用版本更保守。
fn escape_terminal_chat_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.push(c);
        if c == '<' {
            out.push(LARK_MD_TAG_BREAKER);
        }
    }
    out
}

/// 终端链接按钮:打开链接的同时把点击人回传给 daemon,只有收件人本人的点击能激活这条链接。
/// 一次性码取链接的最后一段(`<base>/t/<code>`,格式由 web_terminal 签发时决定)。
fn terminal_link_btn(label: &str, ty: &str, url: &str, card: &TerminalCard) -> serde_json::Value {
    let code = url.rsplit('/').next().unwrap_or_default();
    url_callback_btn(
        label,
        ty,
        url,
        serde_json::json!({
            "action": TERMINAL_AUTH_ACTION,
            "code": code,
            "bound_agent": card.agent,
            "bound_requester": card.recipient_open_id,
            "target_chat_id": card.target_chat_id,
            "target_chat_type": card.target_chat_type,
        }),
    )
}

pub fn card_terminal(card: &TerminalCard) -> String {
    use serde_json::json;
    let elements = vec![
        json!({
            "tag": "markdown",
            "content": summon_i18n::format!(
                "ra2fd738c4f",
                escape_lark_md(card.agent),
                escape_terminal_chat_label(card.chat_label),
            )
        }),
        button_row(vec![
            terminal_link_btn(
                summon_i18n::text!("r9ba663b840"),
                "default",
                card.read_only_url,
                card,
            ),
            terminal_link_btn(
                summon_i18n::text!("r5e7bbc1fe5"),
                "danger",
                card.operate_url,
                card,
            ),
        ]),
        button_row(vec![cb_btn(
            summon_i18n::text!("r4dee8db30d"),
            "default",
            json!({
                "action": "term_regen",
                "bound_agent": card.agent,
                "bound_requester": card.recipient_open_id,
                "target_chat_id": card.target_chat_id,
                "target_chat_type": card.target_chat_type,
            }),
        )]),
    ];
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// 私聊给 `/dash` 发起人的控制台卡:只有一个打开链接的按钮。链接一次性、`ttl_secs` 秒内有效,
/// 进入后的会话 12 小时。
pub fn card_dashboard(url: &str, ttl_secs: u64) -> String {
    use serde_json::json;
    let elements = vec![
        json!({
            "tag": "markdown",
            "content": summon_i18n::format!(
                "red05ad23bf",
                human_duration(ttl_secs)
            )
        }),
        button_row(vec![url_btn(
            summon_i18n::text!("r7622b58750"),
            "primary",
            url,
        )]),
    ];
    json!({
        "schema": "2.0",
        "config": {"summary": {"content": summon_i18n::text!("r29d2fdabc4")}},
        "body": {"elements": elements}
    })
    .to_string()
}

/// 提示人去现场处理的回执:正文照旧,下面带一个「>_ 打开终端」,点了私聊一张终端卡。
/// 正文里有用户发的命令原文与错误串,与 `card_markdown` 一样过 `escape_lark_md`。
pub fn card_notice_with_terminal(markdown: &str) -> String {
    use serde_json::json;
    let elements = vec![
        md_el(markdown),
        button_row(vec![cb_btn(
            summon_i18n::text!("rb66023a802"),
            "primary",
            json!({"action": "term_open"}),
        )]),
    ];
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// 私信 work_dir 选择器一行候选。
pub struct WorkDirPickItem<'a> {
    /// 折叠 home 后的展示路径(既是按钮 label 也是回调 `value.path`;apply_chat_workdir 的
    /// `expand_tilde` 会还原 `~`,round-trip 无损)。
    pub path: &'a str,
    /// 短来源提示(同群其他 agent / 本 agent 常用 / fleet 其他)。
    pub hint: &'a str,
}

/// 一行放一个候选按钮:路径 label 可能很长,横排会被压扁,一律整宽单列。
const WORKDIR_PICK_BTNS_PER_ROW: usize = 1;
/// 私信卡最多列几个候选按钮:fleet 大了全渲会撑爆卡片(同 model 面板的 230099 教训);超出的
/// 靠 `/cd <路径>` 手打兜底。
const WORKDIR_PICK_CAP: usize = 10;

/// 私信 work_dir 选择器卡(**DM-only**):群面板「改工作目录」触发,主动私聊操作者。每个候选同时给出
/// 可复制的路径文本和操作按钮;点击写**目标 chat**(不是这张私信所在的 p2p)的 `.cwd`。
///
/// 按钮 `value` 带 `target_chat_id` / `target_chat_type` —— chat tag 是单向 hash(`chat_tag`)解不回,
/// 必须原样带 chat 标识让 handler 路由回原会话。`path` 带折叠路径,handler 侧 `expand_tilde` 还原。
/// `current_dir` 展示目标会话当前的有效目录;解析不到时显示「未解析」。
/// **路径只活在这张私信卡里,群里不出现。**
pub fn card_workdir_picker(
    target_chat_id: &str,
    target_chat_type: &str,
    current_dir: Option<&str>,
    items: &[WorkDirPickItem<'_>],
) -> String {
    use serde_json::json;
    let current_dir = current_dir
        .map(escape_lark_md)
        .map(|path| path.replace('`', "\\`"))
        .unwrap_or_else(|| summon_i18n::text!("r837d0622f8").to_string());
    let mut elements: Vec<serde_json::Value> = vec![json!({
        "tag": "markdown",
        "content": summon_i18n::format!(
            "rd1f471e666"
        )
    })];
    let shown = items.len().min(WORKDIR_PICK_CAP);
    for row in items[..shown].chunks(WORKDIR_PICK_BTNS_PER_ROW) {
        for it in row {
            let path = escape_lark_md(it.path).replace('`', "\\`");
            elements.push(json!({
                "tag": "markdown",
                "content": summon_i18n::format!("r1cd9c7c93d")
            }));
        }
        let btns: Vec<serde_json::Value> = row
            .iter()
            .map(|it| {
                // 按钮文案 = 路径 · 来源提示(两者都是本地/内置文本,飞书 plain_text 不解析标签)。
                cb_btn(
                    &format!("{} · {}", it.path, it.hint),
                    "default",
                    json!({
                        "action": "set_workdir_to",
                        "target_chat_id": target_chat_id,
                        "target_chat_type": target_chat_type,
                        "path": it.path,
                    }),
                )
            })
            .collect();
        elements.push(button_row(btns));
    }
    if items.len() > shown {
        let extra = items.len() - shown;
        elements.push(json!({
            "tag": "markdown",
            "content": summon_i18n::format!("r93abcd6239")
        }));
    }
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// D:主动私聊回执的「来源群」标注**原文**(未转义)。owner 并发收到多群的主动私信卡/回执时,靠这行
/// 认出「这条是哪个群的」,免得搞混。`group_name` = 已解析的群名(拿不到传 None → 退到 chat_id 尾段,
/// 仍能区分不同群)。返回形如 `(来自群「<名>」)`。**纯拼接,无副作用**——转义由 embed 时统一做
/// ([`card_with_source_note`] / [`card_markdown_with_note`]),与 badge/context 的处置一致。
pub fn source_group_note(group_name: Option<&str>, chat_id: &str) -> String {
    let shown = group_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| source_chat_tail(chat_id));
    summon_i18n::format!("rfcdd209ecb")
}

/// chat_id 尾段:群名解析不到时的兜底区分码。取最后一个分隔符(`_`/`-`/`:`)之后的段,再截末 12 字符
/// (够区分不同群、又不刷屏)。全空则退回原串。
fn source_chat_tail(chat_id: &str) -> String {
    let seg = chat_id
        .rsplit(['_', '-', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or(chat_id);
    let chars: Vec<char> = seg.chars().collect();
    if chars.len() > 12 {
        chars[chars.len() - 12..].iter().collect()
    } else if seg.is_empty() {
        chat_id.to_string()
    } else {
        seg.to_string()
    }
}

/// D:把「来源群」标注作为一行灰字插到 2.0 卡片正文**最前面**。解析失败 / 结构异常 → **原样返回**
/// (best-effort:绝不因加标注而让主动私聊卡发不出去)。`note` 是 [`source_group_note`] 的未转义原文,
/// 这里统一 `escape_lark_md` 后进 `<font>` 模板(群名是外部文本,必须转义)。
pub fn card_with_source_note(card_json: &str, note: &str) -> String {
    let note = note.trim();
    if note.is_empty() {
        return card_json.to_string();
    }
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(card_json) else {
        return card_json.to_string();
    };
    let line = format!("<font color='grey'>{}</font>", escape_lark_md(note));
    let el = serde_json::json!({"tag": "markdown", "content": line});
    match v
        .get_mut("body")
        .and_then(|b| b.get_mut("elements"))
        .and_then(|e| e.as_array_mut())
    {
        Some(arr) => {
            arr.insert(0, el);
            v.to_string()
        }
        None => card_json.to_string(),
    }
}

/// 正文 + 一行灰字注脚的 2.0 卡片。
///
/// 两段必须是**两个独立元素**：`markdown` 是不可信正文，走 [`md_el`] 转义；`note` 转义后才包
/// summon 自己的 `<font>` 模板。不能先拼成一个 String 再交给 [`card_markdown`] —— 后者会正确地
/// 转义整个外部字符串，结果模板 `<font>` 作为明文展示给用户（2026-08-12 工作目录私信回执、
/// 2026-08-28 `/help` 卡两次实测）。
pub fn card_markdown_with_note(markdown: &str, note: &str) -> String {
    use serde_json::json;
    let mut elements = vec![md_el(markdown)];
    let note = note.trim();
    if !note.is_empty() {
        elements.push(json_grey_line(note));
    }
    json!({"schema": "2.0", "body": {"elements": elements}}).to_string()
}

/// 退出回执卡片：reason 决定图标与标题，回执展示会话的退出状态和发生时间。
/// `at` 是**事件发生/入队**时刻(非投递时刻):daemon 停机期间入队的回执可能迟到数天,
/// 卡片必须能自证说的是什么时候的事。
pub fn card_session_exited(
    reason: crate::exit::ExitReason,
    agent: &str,
    chat_label: &str,
    at: &str,
    text: &str,
    session_created_at: Option<&str>,
) -> String {
    use crate::exit::ExitReason;
    use serde_json::json;
    let (icon, title) = match reason {
        ExitReason::ManualKill => ("🔴", summon_i18n::text!("rce522a454c")),
        // 强制重置与会话面板的归档分别显示各自状态。
        ExitReason::ManualKillReset => ("♻️", summon_i18n::text!("r414eb2152c")),
        ExitReason::PanelClean => ("🚪", summon_i18n::text!("re51e1cc44f")),
        ExitReason::PanelReset => ("📦", summon_i18n::text!("r01d6a0d295")),
        // 空闲退出保留自动续聊入口。
        ExitReason::IdleRetired => ("🚪", summon_i18n::text!("r4bd09ea6d7")),
        ExitReason::AgentArchived => ("🚪", summon_i18n::text!("re51e1cc44f")),
        ExitReason::GroupDisbanded => ("👋", summon_i18n::text!("r3e56e1ba52")),
        ExitReason::BotRemoved => ("👋", summon_i18n::text!("r1aab74b681")),
        ExitReason::ShutdownInterrupt => ("⏸️", summon_i18n::text!("r53dfda12e9")),
    };
    // 「时间」= 退出时刻;「起始」= 本会话首次冷启时间(resume 不更新,reset 后重写)。
    // 起始时间未知(外部接管/历史会话)时不展示该行。
    let mut fields = summon_i18n::format!("r2665afee8d");
    if let Some(created) = session_created_at.filter(|s| !s.trim().is_empty()) {
        fields.push_str(&summon_i18n::format!("r8bb1a5a59e"));
    }
    let elements = vec![
        json!({"tag": "markdown", "content": format!("{icon} **{title}**")}),
        json!({"tag": "hr"}),
        json!({"tag": "markdown", "content": fields}),
        json!({"tag": "hr"}),
        json!({"tag": "markdown", "content": text}),
    ];
    let card = closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string());
    card_with_derived_feed_summary(&card).unwrap_or(card)
}

/// `card_chat_ask` 的入参。
pub struct ChatAskCard<'a> {
    pub gate_id: &'a str,
    /// 提问者 open_id:卡片顶部 @ 他,别让问题淹没在群里没人认领。
    pub requester_open_id: &'a str,
    pub questions: &'a [crate::core_port::AskQuestion],
}

/// chat 轮内 agent 的提问卡:**一次多题、一张卡答完**。
///
/// 每题一个控件(有候选 → 下拉单选;无候选 → 自由文本输入),共用一个 form,底部「提交」+
/// 「全部跳过」。控件 name 用 `q<i>`;允许自定义答案的选项题另有 `q<i>_other`,且非空
/// 自定义答案优先。提交后按题序取回。
/// **允许只答一部分**:没选的项回 `null`,agent 自行判断 —— 三个问题里用户往往只对其中一个
/// 有意见,逼他答完或全部跳过都不合理(设计不变量 7)。
pub fn card_chat_ask(card: &ChatAskCard) -> String {
    closeable_card(ask_card_json(card, None).to_string())
}

/// 提问卡的**终态**:保留原卡全貌(题目、控件、按钮都还在),只把所有交互控件 `disabled` 置灰,
/// 并在每题下补一行「→ 你的回答」。
///
/// why 不重渲染成纯文本列表:真机反馈过 —— 那样卡片「变了个样子」,用户既认不出是原来那张,
/// 也看不出控件当时长什么样。与 `card_session_closed` 同一套做法(保留原貌 + `disable_interactive`)。
/// **必须 schema 2.0**:原卡是 2.0,飞书 PATCH 不允许换 schema(230099)。
pub fn card_chat_ask_frozen(
    card: &ChatAskCard,
    answers: &[Option<String>],
    skipped: bool,
) -> String {
    let (footer, outcome) = if skipped {
        (
            summon_i18n::text!("rc151ffcef4"),
            summon_i18n::text!("r71e95b289a"),
        )
    } else {
        (
            summon_i18n::text!("r8b9fa1d7ca"),
            summon_i18n::text!("rbc37a6110a"),
        )
    };
    freeze_ask_card(card, answers, footer, outcome)
}

/// 提问卡为什么不再接受回答。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskCardEnd {
    TurnEnded,
    TimedOut,
    /// Claude 的提问已在终端里答过或被它自己收掉(hook 从转录看到了结果)。
    Elsewhere,
}

/// waiter 已不存在的终态。必须保留问题供回看，但不能暗示“用户主动跳过”。
pub fn card_chat_ask_invalidated(card: &ChatAskCard, end: AskCardEnd) -> String {
    let (footer, outcome) = match end {
        AskCardEnd::TimedOut => (
            summon_i18n::text!("r3620b9f82e"),
            summon_i18n::text!("r73ce15fee4"),
        ),
        AskCardEnd::TurnEnded => (
            summon_i18n::text!("r0c5f43a740"),
            summon_i18n::text!("rd79b1d0e5c"),
        ),
        AskCardEnd::Elsewhere => (
            summon_i18n::text!("raf25fee78f"),
            summon_i18n::text!("rd79b1d0e5c"),
        ),
    };
    freeze_ask_card(card, &[], footer, outcome)
}

fn freeze_ask_card(
    card: &ChatAskCard,
    answers: &[Option<String>],
    footer: &str,
    outcome: &str,
) -> String {
    use serde_json::json;
    let mut v = ask_card_json(card, Some(answers));
    disable_interactive(&mut v);
    if let Some(elements) = v
        .get_mut("body")
        .and_then(|b| b.get_mut("elements"))
        .and_then(|e| e.as_array_mut())
    {
        elements.push(json!({"tag": "hr"}));
        elements.push(json!({"tag": "markdown", "content": footer}));
    }
    with_outcome_feed_summary(v, outcome)
}

/// 提问卡的元素树。`answers=Some` 时是终态:每题下补一行「→ 回答」(未答标出来)。
/// 提问态与终态共用同一棵树 —— 终态才能「保留原貌」,两边分头拼必然长歪。
fn ask_card_json(card: &ChatAskCard, answers: Option<&[Option<String>]>) -> serde_json::Value {
    use serde_json::json;
    let questions: Vec<crate::core_port::AskQuestion> =
        card.questions.iter().map(bounded_ask_question).collect();
    let mut elements: Vec<serde_json::Value> = vec![json!({
        "tag": "markdown",
        "content": summon_i18n::format!("r178af9bbe4", at_suffix(card.requester_open_id))
    })];
    let mut form_elements: Vec<serde_json::Value> = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        let name = format!("q{i}");
        let question = escape_lark_md(q.question.trim());
        let heading = match q.header.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            Some(header) if question.is_empty() => {
                format!("**{}.** {}", i + 1, escape_lark_md(header))
            }
            Some(header) => format!("**{}.** {}\n{}", i + 1, escape_lark_md(header), question),
            None => format!("**{}.** {}", i + 1, question),
        };
        form_elements.push(json!({
            "tag": "markdown",
            "content": heading
        }));
        if q.options.is_empty() {
            form_elements.push(json!({
                "tag": "input",
                "name": name,
                "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("r6a46c25d15")}
            }));
        } else {
            // 展示文案可以有界截断，但 callback value 必须保留原候选；否则长 label 会被
            // 静默改写，Codex 收到的答案不再等于它给出的 option label。
            let raw_options = &card.questions[i].options;
            let opts: Vec<serde_json::Value> = q
                .options
                .iter()
                .enumerate()
                .map(|(index, shown)| {
                    let value = raw_options.get(index).unwrap_or(shown);
                    json!({"text": {"tag": "plain_text", "content": shown}, "value": value})
                })
                .collect();
            let (tag, placeholder) = if q.multi_select {
                ("multi_select_static", summon_i18n::text!("ra0f4fae496"))
            } else {
                ("select_static", summon_i18n::text!("r611e3bcd4f"))
            };
            form_elements.push(json!({
                "tag": tag,
                "name": name,
                "placeholder": {"tag": "plain_text", "content": placeholder},
                "options": opts
            }));
            let descriptions: Vec<String> = q
                .options
                .iter()
                .zip(q.descriptions.iter())
                .filter_map(|(option, description)| {
                    let description = description.trim();
                    if description.is_empty() {
                        return None;
                    }
                    Some(format!(
                        "• {} — {}",
                        escape_lark_md(option.trim()),
                        escape_lark_md(description)
                    ))
                })
                .collect();
            if !descriptions.is_empty() {
                form_elements.push(json!({
                    "tag": "markdown",
                    "content": format!(
                        "<font color='grey'>{}</font>",
                        descriptions.join("\n")
                    )
                }));
            }
            if q.allow_other {
                form_elements.push(json!({
                    "tag": "input",
                    "name": format!("{name}_other"),
                    "placeholder": {
                        "tag": "plain_text",
                        "content": summon_i18n::text!("rfdfdb0ae97")
                    }
                }));
            }
        }
        // 终态:控件下方补一行回答。控件本身置灰后不显示选中值(飞书不回填),没这行就看不出选了啥。
        if let Some(answers) = answers {
            let shown = match answers.get(i).cloned().flatten() {
                Some(a) if !a.trim().is_empty() => format!(
                    "**{}**",
                    escape_lark_md(&truncate_chars(a.trim(), ASK_ECHO_ANSWER_MAX))
                ),
                _ => summon_i18n::text!("r796b2e0304").to_string(),
            };
            form_elements.push(json!({"tag": "markdown", "content": format!("→ {shown}")}));
        }
    }
    // 题目与选项随按钮 value 带回来:定格时要**用原样重建整张卡**再置灰,而回调里只有
    // form_value 的答案,题面和候选都不在其中。逐题截断题干,防 value 过大被飞书拒。
    let qs: Vec<serde_json::Value> = questions
        .iter()
        .map(|q| {
            json!({
                "q": q.question,
                "header": q.header,
                "options": q.options,
                "descriptions": q.descriptions,
                "allow_other": q.allow_other,
                "multi_select": q.multi_select,
            })
        })
        .collect();
    form_elements.push(json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": summon_i18n::text!("r08a85f4ab4")},
        "type": "primary",
        "behaviors": [{"type": "callback", "value": {"action": "gate_ask_submit", "gate_id": card.gate_id, "qs": qs}}],
        "form_action_type": "submit",
        "name": "gate_ask_submit_btn"
    }));
    elements.push(json!({
        "tag": "form",
        "name": "gate_ask_form",
        "elements": form_elements
    }));
    // 「全部跳过」不在 form 里:它不需要收集任何输入,点了就是显式放弃这一问 ——
    // 让 agent 拿到明确的「用户跳过」而不是无声挂死,这是「不超时」的逃生阀之一。
    elements.push(button_row(vec![cb_btn(
        summon_i18n::text!("r11153705f4"),
        "default",
        json!({"action": "gate_ask_skip", "gate_id": card.gate_id, "qs": qs}),
    )]));
    json!({"schema": "2.0", "body": {"elements": elements}})
}

/// 卡片字段随 callback 带回用于定格，全部按字符裁剪：既保留足够上下文，也避免模型输出
/// 把按钮 value 和终态卡撑过飞书上限。初态同样使用这份有界副本，保证提交前后题面一致。
const ASK_ECHO_Q_MAX: usize = 240;
const ASK_ECHO_HEADER_MAX: usize = 48;
const ASK_ECHO_OPTION_MAX: usize = 80;
const ASK_ECHO_DESCRIPTION_MAX: usize = 160;
const ASK_ECHO_ANSWER_MAX: usize = 500;

fn bounded_ask_question(q: &crate::core_port::AskQuestion) -> crate::core_port::AskQuestion {
    crate::core_port::AskQuestion {
        question: truncate_chars(q.question.trim(), ASK_ECHO_Q_MAX),
        header: q
            .header
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| truncate_chars(h, ASK_ECHO_HEADER_MAX)),
        options: q
            .options
            .iter()
            .map(|option| truncate_chars(option.trim(), ASK_ECHO_OPTION_MAX))
            .collect(),
        descriptions: q
            .descriptions
            .iter()
            .take(q.options.len())
            .map(|description| truncate_chars(description.trim(), ASK_ECHO_DESCRIPTION_MAX))
            .collect(),
        allow_other: q.allow_other,
        multi_select: q.multi_select,
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// 提问等太久的提醒卡。**不是超时通知** —— 闸门仍在等,agent 也还阻塞着;
/// 这只是把人拉回来。带 gate_id 便于回到原卡。
pub fn card_chat_ask_remind(gate_id: &str, requester_open_id: &str, waited_mins: u64) -> String {
    use serde_json::json;
    let _ = gate_id;
    closeable_card(
        json!({
            "schema": "2.0",
            "body": {"elements": [json!({
                "tag": "markdown",
                "content": summon_i18n::format!(
                    "r301d11c689",
                    at_suffix(requester_open_id)
                )
            })]}
        })
        .to_string(),
    )
}

/// 标题后的 @:有提问人时是全角括号里的 at 标签,没有就什么都不加 —— 轮外的后台请求没有提问人,
/// 空的 at 标签在飞书上会显示成一个不认识的人。
fn at_suffix(open_id: &str) -> String {
    let open_id = open_id.trim();
    if open_id.is_empty() {
        String::new()
    } else {
        format!("（<at id={open_id}></at>）")
    }
}

/// `card_claude_permission` 的入参。
pub struct ClaudePermissionCard<'a> {
    pub gate_id: &'a str,
    /// 本轮发起人;轮外的后台请求为空串,卡上就不 @ 人。
    pub requester_open_id: &'a str,
    pub request: &'a crate::core_port::ClaudeHookRequest,
}

/// 授权卡的终态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudePermissionEnd<'a> {
    Allowed {
        operator_open_id: &'a str,
    },
    Denied {
        operator_open_id: &'a str,
    },
    /// 入参太长、卡上显示不全,有人点了「到终端处理」:这一步回到终端里的 Claude 对话框。
    Terminal {
        operator_open_id: &'a str,
    },
    /// 请求已在别处结束(终端里答过、被 Claude 自动拒绝、本轮被打断),或 hook 已经退出。
    Elsewhere,
}

/// Claude 要人确认的一次工具调用:工具、发起方、完整入参,底部「允许」「拒绝」。
///
/// 批准的是这一条调用,卡上看不全就无从判断:入参超出预算被省略时卡上**不给「允许」**,只给
/// 「拒绝」与「到终端处理」。按钮 value 只带 gate_id:权威的请求内容在 Core 的闸门里。
pub fn card_claude_permission(card: &ClaudePermissionCard) -> String {
    closeable_card(permission_card_json(card, None).to_string())
}

/// 这次请求的入参能不能在卡上完整显示。显示不全的请求不能在飞书上批准(Core 据此拒绝 allow)。
pub fn claude_permission_fully_shown(request: &crate::core_port::ClaudeHookRequest) -> bool {
    !permission_fields(request).1
}

/// 授权卡定格:保留原卡全貌,按钮置灰,底部写明结局。
pub fn card_claude_permission_settled(
    card: &ClaudePermissionCard,
    end: ClaudePermissionEnd,
) -> String {
    use serde_json::json;
    let mut v = permission_card_json(card, Some(end));
    let (footer, outcome) = match end {
        ClaudePermissionEnd::Allowed { operator_open_id } => (
            summon_i18n::format!("ra65e9c56a5", at_suffix(operator_open_id)),
            summon_i18n::text!("r75d889ac43"),
        ),
        ClaudePermissionEnd::Denied { operator_open_id } => (
            summon_i18n::format!("rb6f8b5abf7", at_suffix(operator_open_id)),
            summon_i18n::text!("rf098218a30"),
        ),
        ClaudePermissionEnd::Terminal { operator_open_id } => (
            summon_i18n::format!("r7536addaf8", at_suffix(operator_open_id)),
            summon_i18n::text!("rfa07f14636"),
        ),
        ClaudePermissionEnd::Elsewhere => (
            summon_i18n::text!("r32fa5c84e4").to_string(),
            summon_i18n::text!("rd79b1d0e5c"),
        ),
    };
    if let Some(elements) = v
        .get_mut("body")
        .and_then(|b| b.get_mut("elements"))
        .and_then(|e| e.as_array_mut())
    {
        elements.push(json!({"tag": "hr"}));
        elements.push(json!({"tag": "markdown", "content": footer}));
    }
    with_outcome_feed_summary(v, outcome)
}

/// 授权卡上先列的字段:一眼要看到的是要执行什么、动哪个文件。其余字段按原顺序跟在后面。
const PERMISSION_LEAD_FIELDS: [&str; 6] = [
    "command",
    "file_path",
    "notebook_path",
    "url",
    "path",
    "pattern",
];
/// 单个字段在卡上最多占多少字节;超出按行省略中间,这张卡也就不能批准。
const PERMISSION_FIELD_MAX_BYTES: usize = 6_000;
/// 全部字段合计的预算,防一个塞满大段内容的 Write 把卡撑过飞书上限。
const PERMISSION_BODY_MAX_BYTES: usize = 10_000;
/// 预算用完之后,后面的字段每个仍留这么多字节,至少看得出有这个字段。
const PERMISSION_FIELD_MIN_BYTES: usize = 256;

/// 卡上展示的字段(字段名, 展示文本)与「有没有字段被省略」。字符串形态的 `description` 单独
/// 作灰字说明;入参不是对象时整体作为一个字段展示 —— 卡上看不到的东西不能被批准。
fn permission_fields(
    request: &crate::core_port::ClaudeHookRequest,
) -> (Vec<(String, String)>, bool) {
    let empty = serde_json::Map::new();
    let (object, whole) = match &request.tool_input {
        serde_json::Value::Object(object) => (object, None),
        serde_json::Value::Null => (&empty, None),
        other => (&empty, Some(("input", other))),
    };
    let lead = PERMISSION_LEAD_FIELDS
        .iter()
        .filter_map(|key| object.get_key_value(*key))
        .map(|(key, value)| (key.as_str(), value));
    let rest = object
        .iter()
        .filter(|(key, value)| {
            !(key.as_str() == "description" && value.is_string())
                && !PERMISSION_LEAD_FIELDS.contains(&key.as_str())
        })
        .map(|(key, value)| (key.as_str(), value));
    let mut budget = PERMISSION_BODY_MAX_BYTES;
    let mut truncated = false;
    let mut fields = Vec::new();
    for (key, value) in whole.into_iter().chain(lead).chain(rest) {
        let text = match value {
            serde_json::Value::String(text) => text.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        let limit = PERMISSION_FIELD_MAX_BYTES.min(budget.max(PERMISSION_FIELD_MIN_BYTES));
        let shown = elide_middle_lines(&text, limit);
        truncated |= shown != text;
        budget = budget.saturating_sub(shown.len());
        fields.push((key.to_string(), shown));
    }
    (fields, truncated)
}

fn permission_card_json(
    card: &ClaudePermissionCard,
    end: Option<ClaudePermissionEnd>,
) -> serde_json::Value {
    use serde_json::json;
    let request = card.request;
    let tool = escape_lark_md(&request.tool_name);
    let mut elements = vec![json!({
        "tag": "markdown",
        "content": summon_i18n::format!("r4bc3e784e3", at_suffix(card.requester_open_id)),
    })];
    let who = match request.agent_type.as_deref().map(str::trim) {
        _ if request.agent_id.is_none() => summon_i18n::text!("rb3414e9b50").to_string(),
        Some(kind) if !kind.is_empty() => summon_i18n::format!("r6de84f2088"),
        _ => summon_i18n::text!("r20b5a5c368").to_string(),
    };
    elements.push(json_grey_line(&who));
    if let Some(description) = request
        .tool_input
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        elements.push(json_grey_line(description));
    }
    let (fields, truncated) = permission_fields(request);
    for (key, shown) in &fields {
        elements
            .push(json!({"tag": "markdown", "content": format!("**{}**", escape_lark_md(key))}));
        elements.push(menu_title_element(shown));
    }
    let mut note = summon_i18n::text!("rac7d46a54c").to_string();
    if end.is_none() {
        if truncated {
            note.push_str(summon_i18n::text!("r601e50054a"));
        }
        if request.agent_id.is_none() {
            note.push_str(summon_i18n::text!("r04ddf36701"));
        }
    }
    elements.push(json_grey_line(&note));
    elements.push(json!({"tag": "hr"}));
    // 终态按钮不带回调:置灰之外再拿掉 behaviors,点了也不会进 daemon。选中的那个标 ✅。
    let deny = |gate_id: &str| {
        cb_btn(
            summon_i18n::text!("r136de7a8c4"),
            "danger",
            json!({"action": "gate_perm_deny", "gate_id": gate_id}),
        )
    };
    let buttons = match end {
        None if truncated => vec![
            deny(card.gate_id),
            cb_btn(
                summon_i18n::text!("rf75257c210"),
                "default",
                json!({"action": "gate_perm_terminal", "gate_id": card.gate_id}),
            ),
        ],
        None => vec![
            cb_btn(
                summon_i18n::text!("rce7ef28b67"),
                "primary",
                json!({"action": "gate_perm_allow", "gate_id": card.gate_id}),
            ),
            deny(card.gate_id),
        ],
        Some(ClaudePermissionEnd::Allowed { .. }) => vec![
            cb_btn_disabled(summon_i18n::text!("ra3d9eb8030"), "primary"),
            cb_btn_disabled(summon_i18n::text!("r136de7a8c4"), "default"),
        ],
        Some(ClaudePermissionEnd::Denied { .. }) => vec![
            cb_btn_disabled(summon_i18n::text!("rce7ef28b67"), "default"),
            cb_btn_disabled(summon_i18n::text!("rd4c4aa9f82"), "danger"),
        ],
        Some(ClaudePermissionEnd::Terminal { .. }) => vec![
            cb_btn_disabled(summon_i18n::text!("r136de7a8c4"), "default"),
            cb_btn_disabled(summon_i18n::text!("rc80fe0110e"), "default"),
        ],
        Some(ClaudePermissionEnd::Elsewhere) => vec![
            cb_btn_disabled(summon_i18n::text!("rce7ef28b67"), "default"),
            cb_btn_disabled(summon_i18n::text!("r136de7a8c4"), "default"),
        ],
    };
    elements.push(button_row(buttons));
    json!({"schema": "2.0", "body": {"elements": elements}})
}

/// Codex 额度耗尽的终态卡。定时触发没有真实 message_id，ERROR 表情必然失败，因此失败本身
/// 必须有一张可见卡；同时明确“不补跑”，避免用户误以为额度恢复后会自动产出漏掉的结果。
pub fn card_codex_usage_limit_exceeded(scheduled: bool) -> String {
    use serde_json::json;
    let (title, tail) = if scheduled {
        (
            summon_i18n::text!("ra69c6b8dc4"),
            summon_i18n::text!("r5061c5239e"),
        )
    } else {
        (
            summon_i18n::text!("r831832dc77"),
            summon_i18n::text!("rd53b01ba93"),
        )
    };
    closeable_card(
        json!({
            "schema": "2.0",
            "body": {"elements": [json!({
                "tag": "markdown",
                "content": summon_i18n::format!(
                    "rbb3f65b850"
                )
            })]}
        })
        .to_string(),
    )
}

/// Codex 五小时额度耗尽时的预填定时任务卡：取快照里 5h 窗口的 `resets_at`，
/// 预填重置后 2 分钟的一次性定时器，prompt 固定为继续指令。用户确认后才创建。
pub fn card_codex_session_limit(resets_at: i64) -> String {
    use crate::schedule::form::{CardFreq, Prefill};
    use serde_json::json;

    let reset = chrono::DateTime::from_timestamp(resets_at, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::Local);
    let reminder = reset + chrono::Duration::minutes(2);
    let pre = Prefill {
        prompt: summon_i18n::text!("ra30ec628e0").to_string(),
        time: reminder.format("%H:%M").to_string(),
        date: reminder.format("%Y-%m-%d").to_string(),
        ..Default::default()
    };
    let base = card_schedule_params(&ScheduleParamsCard {
        freq: CardFreq::Once,
        pre: &pre,
        edit_id: None,
        current: None,
        error: None,
    });
    let Ok(mut card) = serde_json::from_str::<serde_json::Value>(&base) else {
        return closeable_card(card_markdown_v2(summon_i18n::text!("r04dd3d8ec2")));
    };
    if let Some(elements) = card
        .pointer_mut("/body/elements/0/elements")
        .and_then(serde_json::Value::as_array_mut)
    {
        elements.insert(
            1,
            json!({
                "tag": "markdown",
                "content": summon_i18n::format!(
                    "rd896f524d2",
                    reset.format("%Y-%m-%d %H:%M %:z"),
                    reminder.format("%Y-%m-%d %H:%M %:z")
                )
            }),
        );
    }
    card.to_string()
}

#[cfg(test)]
mod usage_limit_card_tests {
    use chrono::{DateTime, Local, Utc};

    #[test]
    fn claude_auto_resume_card_shows_reset_and_continuation_without_scheduler_form() {
        let reset_at = DateTime::parse_from_rfc3339("2026-09-02T15:10:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let limit = crate::claude_session_limit::ClaudeSessionLimit {
            reset_at,
            timezone: "Asia/Singapore".into(),
            reset_time: "11:10pm".into(),
        };
        let wire = super::card_claude_session_limit_auto_resume(&limit);
        let card: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(card["schema"], "2.0");
        assert!(wire.contains("五小时额度已用完"));
        assert!(wire.contains(
            &reset_at
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M %:z")
                .to_string()
        ));
        assert!(wire.contains("自动尝试继续当前任务"));
        assert!(wire.contains("请保持 Claude 会话运行"));
        assert!(!wire.contains("schedule_create"));
        assert!(!wire.contains(r#""tag":"form""#));
        assert!(!wire.contains(r#""tag":"input""#));
    }

    #[test]
    fn scheduled_usage_limit_card_says_the_run_has_no_result_and_will_not_backfill() {
        let card = super::card_codex_usage_limit_exceeded(true);
        assert!(card.contains("定时任务未完成"));
        assert!(card.contains("Codex 额度"));
        assert!(card.contains("本次不会自动补跑"));
    }

    #[test]
    fn claude_session_limit_card_prefills_reset_plus_two_minutes() {
        let reset_at = DateTime::parse_from_rfc3339("2026-09-02T15:10:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let limit = crate::claude_session_limit::ClaudeSessionLimit {
            reset_at,
            timezone: "Asia/Singapore".to_string(),
            reset_time: "11:10pm".to_string(),
        };
        let reminder = limit.reminder_at().with_timezone(&Local);
        let card = super::card_claude_session_limit(&limit);
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        fn field<'a>(value: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
            if value.get("name").and_then(serde_json::Value::as_str) == Some(name) {
                return Some(value);
            }
            match value {
                serde_json::Value::Array(items) => items.iter().find_map(|item| field(item, name)),
                serde_json::Value::Object(map) => map.values().find_map(|item| field(item, name)),
                _ => None,
            }
        }
        let date = reminder.format("%Y-%m-%d").to_string();
        let time = reminder.format("%H:%M").to_string();
        assert_eq!(field(&value, "date").unwrap()["initial_date"], date);
        assert_eq!(field(&value, "time").unwrap()["initial_time"], time);

        let form = serde_json::json!({"date": date, "time": time});
        let (_, spec) = crate::schedule::form::to_spec(
            crate::schedule::form::CardFreq::Once,
            &form,
            reminder - chrono::Duration::minutes(1),
        )
        .unwrap();
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&spec)
                .unwrap()
                .timestamp(),
            reminder.timestamp()
        );
        assert!(card.contains("Claude 五小时会话额度已用完"));
        assert!(card.contains("确认后才会创建定时任务"));
        assert!(card.contains("继续完成刚才因 Claude 额度限制中断的任务"));
        assert!(card.contains(r#""action":"schedule_create""#));
        assert!(card.contains(r#""kind":"once""#));
    }

    #[test]
    fn codex_session_limit_card_prefills_reset_plus_two_minutes() {
        let reset_at = DateTime::parse_from_rfc3339("2026-09-02T15:10:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let reminder = (reset_at + chrono::Duration::minutes(2)).with_timezone(&Local);
        let card = super::card_codex_session_limit(reset_at.timestamp());
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        fn field<'a>(value: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
            if value.get("name").and_then(serde_json::Value::as_str) == Some(name) {
                return Some(value);
            }
            match value {
                serde_json::Value::Array(items) => items.iter().find_map(|item| field(item, name)),
                serde_json::Value::Object(map) => map.values().find_map(|item| field(item, name)),
                _ => None,
            }
        }
        let date = reminder.format("%Y-%m-%d").to_string();
        let time = reminder.format("%H:%M").to_string();
        assert_eq!(field(&value, "date").unwrap()["initial_date"], date);
        assert_eq!(field(&value, "time").unwrap()["initial_time"], time);

        let form = serde_json::json!({"date": date, "time": time});
        let (_, spec) = crate::schedule::form::to_spec(
            crate::schedule::form::CardFreq::Once,
            &form,
            reminder - chrono::Duration::minutes(1),
        )
        .unwrap();
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&spec)
                .unwrap()
                .timestamp(),
            reminder.timestamp()
        );
        assert!(card.contains("Codex 五小时额度已用完"));
        assert!(card.contains("确认后才会创建定时任务"));
        assert!(card.contains("继续完成刚才因 Codex 额度限制中断的任务"));
        assert!(card.contains(r#""action":"schedule_create""#));
        assert!(card.contains(r#""kind":"once""#));
    }
}

/// 交互菜单 → **单选**卡片(Q1):标题/说明 + 每个选项一个回调按钮。点击回灌
/// `{action:"tui_choice", ordinal, title, options, signature}`——`title`/`options` 一并回传,
/// 这样 `handle_tui_inject` 能重建带置灰按钮的终态卡(用户要求:选完按钮仍可见、置灰、
/// 确认行追加在底部,类似流式卡的「已打断」效果)。`signature` 是这张菜单的签名(按完整问句算,
/// 不受卡上省略影响),`handle_tui_inject` 据它确认屏上还是发卡时那一张。
/// `ordinal` = 选项在列表里的 0-based 位置——注入时按 `Down × ordinal + Enter` 定位,
/// 与「显示编号」无关(故 9/10/11 这种非 1-based 菜单也对)。
/// `options` 是 `(显示编号, 文字)` 列表(显示编号仅用于按钮文案)。
pub fn card_interactive_select(title: &str, options: &[(usize, String)]) -> String {
    // 缺了开头的对话框不带签名:只看得到结尾,点击侧证明不了点下去批准的是哪一条。
    let signature = (!crate::interactive::title_is_cut(title)).then(|| {
        crate::interactive::signature_token(crate::interactive::menu_signature(
            title,
            options
                .iter()
                .map(|(index, label)| (*index, label.as_str())),
        ))
    });
    let display_title = crate::interactive::localized_title(title);
    let mut budget = MENU_TITLE_MAX_BYTES;
    loop {
        let shown = elide_middle_lines(&display_title, budget);
        let card = render_interactive_select(&shown, options, signature.as_deref());
        if card.len() <= CARD_TOTAL_SAFE_LIMIT || budget <= MENU_TITLE_MIN_BYTES {
            return card;
        }
        budget /= 2;
    }
}

/// 菜单卡上对话框原文最多占多少字节。原文随按钮 value 复制(每个按钮一份,供点击后重建终态卡),
/// 整张卡大约是「原文 × (选项数 + 1)」,所以从这个上限起逐级减半,直到整卡落进
/// [`CARD_TOTAL_SAFE_LIMIT`] 或减到 `MENU_TITLE_MIN_BYTES`。
const MENU_TITLE_MAX_BYTES: usize = 6_000;
/// 原文预算减到这里就不再减:再小,卡上已经看不出在问什么,照发。
const MENU_TITLE_MIN_BYTES: usize = 256;

fn render_interactive_select(
    title: &str,
    options: &[(usize, String)],
    signature: Option<&str>,
) -> String {
    use serde_json::json;
    let mut elements: Vec<serde_json::Value> =
        vec![json!({"tag": "markdown", "content": summon_i18n::text!("r9d1dc35b39")})];
    if !title.trim().is_empty() {
        elements.push(menu_title_element(title));
    }
    elements.push(json!({"tag": "hr"}));
    // 没有签名的是缺了开头的对话框:只看得到结尾,点下去批准的是哪一条证明不了,卡上不给选项按钮。
    let Some(signature) = signature else {
        elements.push(json!({
            "tag": "markdown",
            "content": summon_i18n::text!("r9976d92b41"),
        }));
        return closeable_card(
            json!({"schema": "2.0", "body": {"elements": elements}}).to_string(),
        );
    };
    // 把 title + 全部选项的(编号,文字)快照进每个按钮 value,回调时能重建终态卡。
    let opts_snap: Vec<serde_json::Value> = options.iter().map(|(n, l)| json!([n, l])).collect();
    let btns: Vec<serde_json::Value> = options
        .iter()
        .enumerate()
        .map(|(ord, (num, label))| {
            cb_btn(
                &format!("{num}. {label}"),
                "default",
                json!({
                    "action": "tui_choice",
                    "ordinal": ord,
                    "title": title,
                    "options": opts_snap,
                    "signature": signature,
                }),
            )
        })
        .collect();
    elements.push(button_row(btns));
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// 对话框原文进卡片:放进代码块。原文是终端画面 —— 折行对齐、`│ ` 前缀、shell 里的 `*` `|` `_`
/// 都得照原样示人,当 markdown 渲染会被吃掉或改排。它是外部文本,照例过 [`escape_lark_md`];
/// 原文里的 ``` 会提前闭合代码块,反引号之间插零宽空格拆开,显示不变。
fn menu_title_element(title: &str) -> serde_json::Value {
    let breaker = LARK_MD_TAG_BREAKER;
    let body = escape_lark_md(title).replace("```", &format!("`{breaker}`{breaker}`"));
    serde_json::json!({"tag": "markdown", "content": format!("```\n{body}\n```")})
}

/// 超过 `max_bytes` 时留头留尾、省掉中间的行,并补一行说明省了几行。头是标题与命令开头,
/// 尾是警告与问句,确认时两头都得看得见。单行就超出预算时按字符截断。
fn elide_middle_lines(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // 给省略说明那一行留出余量,结果不会超过 max_bytes。
    let half = max_bytes.saturating_sub(64) / 2;
    let lines: Vec<&str> = text.lines().collect();
    let mut head_bytes = 0;
    let head = lines
        .iter()
        .take_while(|l| {
            head_bytes += l.len() + 1;
            head_bytes <= half
        })
        .count();
    let mut tail_bytes = 0;
    let tail = lines[head..]
        .iter()
        .rev()
        .take_while(|l| {
            tail_bytes += l.len() + 1;
            tail_bytes <= half
        })
        .count();
    if head == 0 && tail == 0 {
        let mut end = max_bytes.saturating_sub('…'.len_utf8());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        return format!("{}…", &text[..end]);
    }
    let omitted = lines.len() - head - tail;
    let mut out: Vec<String> = lines[..head].iter().map(|l| l.to_string()).collect();
    out.push(summon_i18n::format!("re0fe674a85"));
    out.extend(lines[lines.len() - tail..].iter().map(|l| l.to_string()));
    out.join("\n")
}

/// 交互态**降级 / 自由文本**卡片(Q1):解析不出选项(或本就是自由文本提问)时用——贴原始屏幕 +
/// 文本输入框 + [发送][Esc 取消]。submit 回灌 `{action:"tui_text"}`(文本在 `form_value.tui_text_input`),
/// Esc 回灌 `{action:"tui_esc"}`。这是「不稳定 TUI」的兜底:解析失配最坏退到这,绝不崩。
/// `terminal` 为 true(会话在 tmux 里,且终端服务出得了一条点得开的链接——在跑,且不是只听回环)
/// 时再带一个「>_ 打开终端」。
/// 终端自报状态的只读提示。终端入口由调用方按现有权限提供。
pub fn card_program_status_hint(title: &str, raw: &str, terminal: bool) -> String {
    use serde_json::json;
    let lines: Vec<_> = raw.lines().collect();
    let shown = lines[lines.len().saturating_sub(15)..].join("\n");
    let mut elements = vec![
        json!({"tag":"div","text":{"tag":"plain_text","content":format!("⏳ {title}\n\n{shown}")}}),
    ];
    if terminal {
        elements.push(button_row(vec![cb_btn(
            summon_i18n::text!("rb66023a802"),
            "default",
            json!({"action":"term_open"}),
        )]));
    }
    json!({"schema":"2.0","body":{"elements":elements}}).to_string()
}

pub fn card_interactive_raw(raw_pane: &str, max_lines: usize, terminal: bool) -> String {
    use serde_json::json;
    let shown: String = {
        let lines: Vec<&str> = raw_pane.lines().collect();
        let start = lines.len().saturating_sub(max_lines.max(1));
        lines[start..].join("\n")
    };
    let elements: Vec<serde_json::Value> = vec![
        json!({"tag": "markdown", "content": summon_i18n::text!("rff8317750e")}),
        json!({"tag": "markdown", "content": summon_i18n::format!("rd9a22802b4")}),
        json!({
            "tag": "form",
            "name": "tui_text_form",
            "elements": [
                {"tag": "input", "name": "tui_text_input", "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("rab29aec351")}},
                {"tag": "button", "text": {"tag": "plain_text", "content": summon_i18n::text!("redecf0ae6e")}, "type": "primary", "behaviors": [{"type": "callback", "value": {"action": "tui_text"}}], "form_action_type": "submit", "name": "tui_text_submit"}
            ]
        }),
        button_row(
            std::iter::once(cb_btn(
                summon_i18n::text!("r382dcb1a1a"),
                "default",
                json!({"action": "tui_esc"}),
            ))
            .chain(terminal.then(|| {
                cb_btn(
                    summon_i18n::text!("rb66023a802"),
                    "default",
                    json!({"action": "term_open"}),
                )
            }))
            .collect(),
        ),
    ];
    json!({"schema": "2.0", "body": {"elements": elements}}).to_string()
}

/// 交互菜单**已解决**终态卡(Q1):与 `card_interactive_select` 同结构,但按钮全部置灰
/// (`behaviors: []`,不可再点)、被选项文字前加 ✅ 并标 primary、底部追加确认行。
/// 用户要求:选完按钮仍可见(不消失)、置灰、确认行像流式卡「已打断」那样单独留一行。
///
/// `options` = (显示编号, 文字) 列表;`selected` = 被选 ordinal(0-based)。
pub fn card_interactive_select_result(
    title: &str,
    options: &[(usize, String)],
    selected: usize,
) -> String {
    use serde_json::json;
    let mut elements: Vec<serde_json::Value> =
        vec![json!({"tag": "markdown", "content": summon_i18n::text!("r9d1dc35b39")})];
    if !title.trim().is_empty() {
        elements.push(menu_title_element(title));
    }
    elements.push(json!({"tag": "hr"}));
    // 置灰按钮:disabled=true 不可点击,behaviors=[] 防误触,被选项标 primary + ✅ 前缀。
    let btns: Vec<serde_json::Value> = options
        .iter()
        .enumerate()
        .map(|(ord, (num, label))| {
            let is_selected = ord == selected;
            let text = if is_selected {
                format!("✅ {num}. {label}")
            } else {
                format!("{num}. {label}")
            };
            let btype = if is_selected { "primary" } else { "default" };
            json!({
                "tag": "button",
                "text": {"tag": "plain_text", "content": text},
                "type": btype,
                "disabled": true,
                "behaviors": [],
            })
        })
        .collect();
    elements.push(button_row(btns));
    // 底部确认行(类似流式卡的「已打断」效果)
    let confirm = summon_i18n::format!("r2108af7b35", selected + 1);
    elements.push(json!({"tag": "markdown", "content": confirm}));
    with_outcome_feed_summary(
        json!({"schema": "2.0", "body": {"elements": elements}}),
        &summon_i18n::format!("r8cc311c5a7", selected + 1),
    )
}

/// 卡片动作的鉴权档位(T4)。此前「action → 是否过 operate-gate」只活在 6 个 handler 的
/// 散落判断和注释里——新加 handler 忘了鉴权,编译照过、全绿照旧。集中成查表:dispatcher 在
/// 分派前统一判,表测试钉死全部 action 的档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardGate {
    /// 无需卡片层鉴权:无破坏性(keepalive/compact),或重注入会再过 talk 白名单(recover),
    /// 或在 Core 层按 `Intent::gate_tier()` 鉴权(gate_*)。
    None,
    /// operate 档(operators 优先、空回退 allowed_users、群里 fail-closed):破坏性生命周期、
    /// 配置变更、按键注入(自由文本即任意指令)、销毁待办。
    Operate,
}

/// 卡片动作的规范枚举(C12):把散落在 dispatcher `match action.as_str()` 里的字符串收敛成单一
/// 枚举。`CardAction::gate` 与 dispatcher 的分派 `match` 都对本枚举**穷举、无 catch-all**——新增
/// 一种卡片动作时,编译器同时逼出「鉴权档位」和「分派臂」两处,漏一处不编译。这才是 dispatcher
/// 注释宣称的「从结构上焊死」的真身(旧实现靠 `_ => None` 通配,新动作忘登记即静默 fail-open)。
///
/// `gate_*` 是开放子族(具体子动作由 `handle_gate_card_action` 再解析全串),故收敛成一个
/// Family 变体。未识别串 → `Unknown`,鉴权 fail-closed。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAction {
    Clean,
    Reset,
    ArchiveEnded,
    /// 改本群模型(含 `inherit` 回到默认)。
    Model,
    /// 改 bot 默认模型:所有没有本群模型的群一起变。
    ModelDefault,
    /// 改本群思考档位(含 `inherit`)。
    Effort,
    /// 改 bot 默认思考档位。
    EffortDefault,
    /// Codex Standard/Fast：写 per-chat 偏好并改变后续轮的服务档位，属于额度相关配置变更。
    CodexServiceTier,
    /// Cursor ACP session mode：写 per-chat 偏好并在下一轮走标准 `session/set_mode`。
    CursorMode,
    /// Cursor ACP 动态参数的本群覆盖(含 `inherit`)。
    CursorConfig,
    /// 把本群当前的 Cursor 模型与参数写成 bot 默认。
    CursorConfigDefault,
    SetWorkdir,
    /// `pick_workdir`:群面板「改工作目录」按钮 → 私信候选目录列表卡给操作者。破坏性入口(触发主动
    /// 私聊 + 后续写盘),过 operate-gate。
    PickWorkdir,
    /// `set_workdir_to`:私信选择器候选按钮 → 写**目标 chat**(value 里带 `target_chat_id`)的 `.cwd`。
    /// 破坏性,过 operate-gate(handler 内再按目标群语义 fail-closed 复核)。
    SetWorkdirTo,
    TuiChoice,
    TuiText,
    TuiEsc,
    DismissRecovery,
    ScheduleCancel,
    /// `schedule_new`:拉起起点卡(纯渲染,不碰盘)。
    ScheduleNew,
    /// `schedule_pick`:选定频率 → 参数卡(纯渲染,不碰盘)。
    SchedulePick,
    /// `schedule_edit`:列表「改」→ 预填好的参数卡。纯渲染,但**预填值来自盘上**,
    /// handler 内必须过归属过滤,否则会把别的会话的定时器正文画到本群卡上。
    ScheduleEdit,
    /// `schedule_create`:参数卡提交 → 新建落盘。**破坏性**(改本机持久状态)。
    ScheduleCreate,
    /// `schedule_save`:参数卡提交 → 覆盖已有条目。**破坏性**。
    ScheduleSave,
    /// `schedule_timer_reset_toggle`:schedules 面板「定时归档」开关 → 翻转本 chat 的
    /// per-chat 重置覆盖(写 sidecar 文件)。**破坏性**(改本机持久状态),过 operate-gate。
    ScheduleTimerResetToggle,
    Interrupt,
    /// `quota_badge`:面板「额度显示」开关 → 写 per-chat 标记文件,控制回复卡角标是否渲染
    /// 剩余额度。改本机持久状态且事关敏感面的展示,过 operate-gate。
    QuotaBadge,
    /// `badge_effort`:会话面板角标行的档位开关 → 写 per-chat 分级开关 `badge.effort`,控制回复卡
    /// 角标是否渲染思考档位。改本机持久状态,与其它配置变更同档,过 operate-gate。
    BadgeEffort,
    /// `badge_cli` / `badge_model` / `badge_ctx`:角标行的另外三个开关,写 per-chat 分级开关
    /// `badge.cli` / `badge.model` / `badge.ctx`。与 `BadgeEffort` 同一个处理函数、同档鉴权。
    BadgeCli,
    BadgeModel,
    BadgeCtx,
    /// Session 卡 CoT 展示开关：写 per-chat 配置，下一轮生效。
    SessionCot,
    /// Session 卡原地刷新：只重读状态并重绘当前卡。
    SessionRefresh,
    /// 卡片关闭（兼容 `session_close`）：撤回当前卡片消息，不改变会话。
    SessionClose,
    /// `fleet_refresh`:管家卡上的刷新 / 下钻 / 返回总览。**只重渲一张只读卡**,
    /// 免鉴权(见 [`Self::gate`])—— 破坏性动作不在这张卡上。
    FleetRefresh,
    /// `fleet_sessions`:翻到某 agent 的会话操作页 / 在该页刷新。**同样只是重渲一张卡**,
    /// 免鉴权 —— 页上那两个退出按钮才是破坏性的,各自单独过闸。
    FleetSessions,
    /// `fleet_waiting_handled`:管家卡待处理区的「标记已处理」。只删盘上那一条 waiting 标记
    /// (回复 id 对得上才删),不触碰会话;改本机持久状态,过管家自己的 operate 闸。
    FleetWaitingHandled,
    /// `fleet_session_clean` / `fleet_session_reset`:管家在**别的 agent** 的会话上按下退出。
    ///
    /// **operate 档,而且是两道闸**:这一层由 dispatcher 过「管家自己的 operate 名单」,
    /// 落到目标 agent 时再过「那个 agent 自己的 operate 名单」。理由是这条路径能跨身份停会话,
    /// 单靠管家一张名单就等于把所有 agent 的操作权合并到一处 —— 每个 agent 自己的闸不该被绕过。
    FleetSessionControl,
    /// `retry_now`:live 卡在「等 API 重试」态时多出的按钮 —— 往 pane 注入「继续」+ Esc,不等
    /// 退避倒计时。**破坏性**(注入正文 + 打断在跑的请求),与 `TuiText`/`Interrupt` 同档。
    RetryNow,
    KeepAlive,
    Compact,
    /// `ctx_compact`:上下文提醒卡的 Compact。效果与面板 compact 相同,另外定格这张卡。
    ContextCompact,
    /// `ctx_handoff`:上下文提醒卡的 Handoff,等同在本群发 `/handoff <输入框内容>`(不带目标):
    /// 写交接后原地压缩或**归档当前会话**,会话生命周期动作,过 operate-gate。
    ContextHandoff,
    /// `ctx_ignore`:上下文提醒卡的 Ignore。只定格这张卡,但会收走卡上的 Handoff 入口,与
    /// `session_close`(撤回面板卡)同档。
    ContextIgnore,
    Recover,
    /// `gate_*`:plan 批准/改/拒绝、question 回答、chat 提问卡提交/跳过/取消。
    GateFamily,
    /// `bridge_*`:外部 Claude ticket 选择、上下文胶囊与停止镜像；目标群只允许服务端 ticket 恢复。
    BridgeFamily,
    /// `sentinel_*`:sentinel 三方建群确认卡(confirm/dismiss)。**破坏性**(建群并拉真人),
    /// 过 operate-gate;handler 内再按 proposal 存根做 fencing 复验。
    SentinelFamily,
    /// owner 审批文档评论草稿，效果落在云文档。
    DocumentComment,
    /// `recall_resume`:召回卡「恢复为托管/接管」按钮。**破坏性**(可能 SIGTERM 活跃源),过 operate-gate。
    RecallResume,
    /// `recall_rpc_handoff`:把 summon-owned RPC 会话从源群迁到目标群。涉及进程与 durable
    /// owner 重键，必须过 operate-gate；handler 内还会按目标群语义再验一次。
    RecallRpcHandoff,
    /// `recall_page`:召回卡翻页按钮。列表含 cwd / prompt / 托管源群，必须按目标群重验
    /// operate；dispatcher 交专用 handler，按目标群语义判。
    RecallPage,
    /// `recall_cmux_remote`:召回卡「遥控」按钮,把目标群接到一条 cmux 里的 Claude 会话上。
    /// 群消息会打进本机外部进程的输入框,与接管同档过 operate-gate,handler 按目标群重验。
    RecallCmuxRemote,
    /// `term_open`:面板 / 回执 / 降级菜单卡上的「>_ 打开终端」→ 私聊触发者一张终端卡。卡在群里
    /// (或 owner 私聊)被点,集中闸按所在 chat 判权限,语义正确。
    TerminalOpen,
    /// `term_regen`:私聊终端卡上的「🔄 换一组新链接」。卡在私聊里,集中闸会按私聊语义判,
    /// 所以 dispatcher 不走集中闸,交专用 handler 按目标 chat 的群语义重验(同 `RecallPage`)。
    TerminalRegenerate,
    /// `term_auth`:私聊终端卡上的「只读查看」「可操作」—— 点击同时打开链接,回调把点击人交给
    /// daemon 激活这条终端链接。handler 认点击人是收件人并按目标 chat 重验 operate(同 `term_regen`)。
    TerminalAuth,
    /// `preview_open`:回复卡末尾的本地文件按钮 —— 只回传点击,handler 按本 chat 的 talk gate 或
    /// 本群范围的 operate-gate 判点击人:过了私聊他一条打开链接,没过私聊他没有权限。判权在
    /// handler 里,回执只进私聊。
    PreviewOpen,
    /// 委派详情由 handler 严格验证 Owner，并只向其私聊返回。
    DelegateDetails,
    /// 源群 Fork 表单与同名目标选择；handler 复验发起人及两端权限。
    Fork,
    /// 未登记动作:鉴权 fail-closed(operate 档),分派走无害 lifecycle 兜底(KeepAlive)。
    Unknown,
}

impl CardAction {
    /// 把飞书卡片回调的 `value.action` 字符串解析成枚举。前缀族(`task_`/`gate_`)镜像 dispatcher
    /// 路由;其余精确匹配;未识别 → `Unknown`。
    pub fn from_action(action: &str) -> Self {
        match action {
            "fork_submit" | "fork_choose" | "fork_execute" => Self::Fork,
            "clean" => Self::Clean,
            "reset" => Self::Reset,
            "archive_ended" => Self::ArchiveEnded,
            "model" => Self::Model,
            "model_default" => Self::ModelDefault,
            "effort" => Self::Effort,
            "effort_default" => Self::EffortDefault,
            "codex_service_tier" => Self::CodexServiceTier,
            "cursor_mode" => Self::CursorMode,
            "cursor_config" => Self::CursorConfig,
            "cursor_config_default" => Self::CursorConfigDefault,
            "set_workdir" => Self::SetWorkdir,
            "pick_workdir" => Self::PickWorkdir,
            "set_workdir_to" => Self::SetWorkdirTo,
            "tui_choice" => Self::TuiChoice,
            "tui_text" => Self::TuiText,
            "tui_esc" => Self::TuiEsc,
            "dismiss_recovery" => Self::DismissRecovery,
            "schedule_cancel" => Self::ScheduleCancel,
            "schedule_new" => Self::ScheduleNew,
            "schedule_pick" => Self::SchedulePick,
            "schedule_edit" => Self::ScheduleEdit,
            "schedule_create" => Self::ScheduleCreate,
            "schedule_save" => Self::ScheduleSave,
            "schedule_timer_reset_toggle" => Self::ScheduleTimerResetToggle,
            "interrupt" => Self::Interrupt,
            "quota_badge" => Self::QuotaBadge,
            "badge_effort" => Self::BadgeEffort,
            "badge_cli" => Self::BadgeCli,
            "badge_model" => Self::BadgeModel,
            "badge_ctx" => Self::BadgeCtx,
            "session_cot" => Self::SessionCot,
            "session_refresh" => Self::SessionRefresh,
            "session_close" | "card_close" => Self::SessionClose,
            "retry_now" => Self::RetryNow,
            "keepalive" => Self::KeepAlive,
            "compact" => Self::Compact,
            "ctx_compact" => Self::ContextCompact,
            "ctx_handoff" => Self::ContextHandoff,
            "ctx_ignore" => Self::ContextIgnore,
            "recover" => Self::Recover,
            "fleet_refresh" => Self::FleetRefresh,
            "fleet_sessions" => Self::FleetSessions,
            "fleet_waiting_handled" => Self::FleetWaitingHandled,
            "fleet_session_clean" | "fleet_session_reset" => Self::FleetSessionControl,
            a if a.starts_with("gate_") => Self::GateFamily,
            "bridge_mirror" | "bridge_handoff" | "bridge_context" | "bridge_stop" => {
                Self::BridgeFamily
            }
            "sentinel_confirm" | "sentinel_dismiss" => Self::SentinelFamily,
            "doc_comment_approve" | "doc_comment_dismiss" => Self::DocumentComment,
            "recall_resume" => Self::RecallResume,
            "recall_rpc_handoff" => Self::RecallRpcHandoff,
            "recall_page" => Self::RecallPage,
            "recall_cmux_remote" => Self::RecallCmuxRemote,
            "term_open" => Self::TerminalOpen,
            "term_regen" => Self::TerminalRegenerate,
            TERMINAL_AUTH_ACTION => Self::TerminalAuth,
            PREVIEW_OPEN_ACTION => Self::PreviewOpen,
            "delegate_details" => Self::DelegateDetails,
            _ => Self::Unknown,
        }
    }

    /// 鉴权档位。对枚举**穷举、无通配**:新增变体必须在此显式声明档位,否则不编译。
    /// `Unknown` → `Operate`:未登记动作 fail-closed(最坏后果从「群里任意人可触发」降为
    /// 「仅 operate 名单可触发」),与 CLAUDE.md「破坏性动作经 operate-gate、群里 fail-closed」同向。
    /// 角标行开关对应的分级开关;其余动作为 None。dispatcher 据此把四个开关交给同一个处理函数,
    /// 映射只在这一处。
    pub fn badge_setting(self) -> Option<crate::settings::SettingKey> {
        use crate::settings::SettingKey;
        match self {
            Self::BadgeCli => Some(SettingKey::BadgeCli),
            Self::BadgeModel => Some(SettingKey::BadgeModel),
            Self::BadgeEffort => Some(SettingKey::BadgeEffort),
            Self::BadgeCtx => Some(SettingKey::BadgeCtx),
            _ => None,
        }
    }

    pub fn gate(self) -> CardGate {
        match self {
            // 破坏性生命周期 + 配置变更 + 按键注入(自由文本即任意指令)+ 销毁待办 + 打断在跑轮。
            Self::Clean
            | Self::Reset
            | Self::ArchiveEnded
            | Self::Model
            | Self::ModelDefault
            | Self::Effort
            | Self::EffortDefault
            | Self::CodexServiceTier
            | Self::CursorMode
            | Self::CursorConfig
            | Self::CursorConfigDefault
            | Self::SetWorkdir
            | Self::PickWorkdir
            | Self::SetWorkdirTo
            | Self::TuiChoice
            | Self::TuiText
            | Self::TuiEsc
            | Self::DismissRecovery
            | Self::ScheduleCancel
            // 建 / 改定时器都改本机持久状态,与取消同档。
            | Self::ScheduleCreate
            | Self::ScheduleSave
            // 定时归档开关:写 per-chat sidecar,改本机持久状态,与取消同档。
            | Self::ScheduleTimerResetToggle
            | Self::Interrupt
            // 额度显示开关:改本机持久状态 + 决定敏感面进不进群卡,与其它配置变更同档。
            | Self::QuotaBadge
            // 角标各项显示开关:同样写 per-chat 持久状态,与额度开关同档。
            | Self::BadgeEffort
            | Self::BadgeCli
            | Self::BadgeModel
            | Self::BadgeCtx
            | Self::SessionCot
            | Self::SessionClose
            // 交接后原地压缩或归档当前会话,与 /handoff 同一道闸;Ignore 收走卡上的入口,与关闭面板卡同档。
            | Self::ContextHandoff
            | Self::ContextIgnore
            // 立即重试 = 注入正文 + 打断当前请求,破坏性与 TuiText/Interrupt 同源。
            | Self::RetryNow
            | Self::BridgeFamily
            // sentinel 建群会把真人拉进新群,不可逆对外动作,必须 operate 档。
            | Self::SentinelFamily
            | Self::DocumentComment
            // 召回恢复可能 SIGTERM 活跃源进程,与 BridgeFamily 同属破坏性,过 operate-gate。
            | Self::RecallResume
            | Self::RecallRpcHandoff
            | Self::RecallPage
            // 遥控把群消息打进本机外部进程的输入框,与接管同档。
            | Self::RecallCmuxRemote
            // 私聊一张能操作会话的终端卡,与按键注入同档。
            | Self::TerminalOpen
            | Self::TerminalRegenerate
            // 激活一条能操作会话的终端链接,与发卡同档;handler 另认收件人。
            | Self::TerminalAuth
            // 标记已处理改盘上的 waiting 标记,与其它写本机持久状态的开关同档。
            | Self::FleetWaitingHandled
            // 管家停别的 agent 的会话:这里过管家自己那道,目标 agent 那道在落地时再过。
            | Self::FleetSessionControl
            | Self::Fork => CardGate::Operate,
            // 卡片层免鉴权:keepalive/compact 无破坏性;recover 重注入会再过 talk 白名单;
            // gate_* 在 Core 层按 Intent::gate_tier 鉴权。
            Self::KeepAlive
            | Self::Compact
            | Self::ContextCompact
            | Self::Recover
            // fleet 卡(含会话操作页)只是重渲只读状态,没有任何持久后果;
            // 操作页上那两个退出按钮是 FleetSessionControl,走 operate 档。
            | Self::FleetRefresh
            | Self::FleetSessions
            | Self::SessionRefresh
            | Self::GateFamily
            // 三个纯渲染动作:不碰盘、不改状态。schedule_edit 会读盘预填,但 handler 内按
            // (agent, chat) 归属过滤,读不到别人的条目——鉴权在那里,不在这一层。
            | Self::ScheduleNew
            | Self::SchedulePick
            | Self::ScheduleEdit
            // 预览按钮在 handler 里按 talk gate 或本群 operate-gate 判,集中闸不拦。
            | Self::PreviewOpen
            | Self::DelegateDetails => CardGate::None,
            // fail-closed:未登记/未来新增动作忘登记 → 默认过 operate-gate。
            Self::Unknown => CardGate::Operate,
        }
    }

    /// operate 档动作的效果范围,决定本群名单算不算数。`chat_id` 是效果落在的那个 chat:集中闸
    /// 给卡所在 chat;私聊卡交专用 handler 的那几个,由 handler 按目标 chat 自己判。
    /// 同样**穷举、无通配**:新增变体必须在这里声明它会不会越出一个群。
    pub fn operate_scope(self, chat_id: &str) -> crate::config::registry::OperateScope<'_> {
        use crate::config::registry::OperateScope;
        match self {
            // 改 bot 默认(agent.yaml 的 model/effort、cursor 的 per-agent bundle):所有没有本群
            // 覆盖的群一起变。
            Self::ModelDefault
            | Self::EffortDefault
            | Self::CursorConfigDefault
            // 召回 / Bridge 列的是本机全部磁盘会话,接管会给本机外部进程发信号;
            // RPC 会话迁移同时改源群与目标群。
            | Self::BridgeFamily
            | Self::RecallResume
            | Self::RecallRpcHandoff
            | Self::RecallPage
            // 遥控操作的是本机一个外部进程,不在发起的那个群里。
            | Self::RecallCmuxRemote
            // 建群拉真人,效果在任何现有群之外。
            | Self::SentinelFamily
            | Self::DocumentComment
            // 管家卡:按钮不在被操作的那个群里。
            | Self::FleetRefresh
            | Self::FleetSessions
            | Self::FleetWaitingHandled
            | Self::FleetSessionControl
            // 未登记动作:不知道效果落在哪,只认 agent 级名单。
            | Self::Unknown => OperateScope::Agent,
            Self::Clean
            | Self::Reset
            | Self::ArchiveEnded
            // 本群模型 / 思考档位 / Cursor 参数覆盖:效果只落在本群。
            | Self::Model
            | Self::Effort
            | Self::CursorConfig
            | Self::CodexServiceTier
            | Self::CursorMode
            | Self::SetWorkdir
            | Self::PickWorkdir
            | Self::SetWorkdirTo
            | Self::TuiChoice
            | Self::TuiText
            | Self::TuiEsc
            | Self::DismissRecovery
            | Self::ScheduleCancel
            | Self::ScheduleNew
            | Self::SchedulePick
            | Self::ScheduleEdit
            | Self::ScheduleCreate
            | Self::ScheduleSave
            | Self::ScheduleTimerResetToggle
            | Self::Interrupt
            | Self::QuotaBadge
            | Self::BadgeEffort
            | Self::BadgeCli
            | Self::BadgeModel
            | Self::BadgeCtx
            | Self::SessionCot
            | Self::SessionRefresh
            | Self::SessionClose
            | Self::RetryNow
            | Self::KeepAlive
            | Self::Compact
            | Self::ContextCompact
            | Self::ContextHandoff
            | Self::ContextIgnore
            | Self::Recover
            | Self::GateFamily
            | Self::TerminalOpen
            | Self::TerminalRegenerate
            | Self::TerminalAuth
            | Self::PreviewOpen
            | Self::DelegateDetails
            | Self::Fork => OperateScope::Chat(chat_id),
        }
    }
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
pub(crate) fn card_gate_tier(action: &str) -> CardGate {
    CardAction::from_action(action).gate()
}

/// sentinel 三方建群确认卡(发到 owner↔bot 私聊)。callback value 只带
/// `{action, proposal}` 两个 opaque 字段——候选人 open_id/群名/触发原文都不进 value,
/// 与 bridge ticket 同一纪律;真实参数由 handler 从盘上 proposal 存根取回并 fencing 复验。
/// `summary` 为外部文本(含对方名字/触发正文),此处统一转义。
pub fn card_sentinel_confirm(proposal_id: &str, summary: &str) -> String {
    let body = summon_i18n::format!("ra558c1af20", escape_lark_md(summary));
    // 不套 cb_btn_confirm 的二次弹窗:这张卡本身就是前置确认步,再弹一层是纯摩擦
    // (owner 2026-08-07 反馈);后果已写在卡片正文里。
    let confirm = cb_btn(
        summon_i18n::text!("r9a4e813f4a"),
        "danger",
        serde_json::json!({"action": "sentinel_confirm", "proposal": proposal_id}),
    );
    let dismiss = cb_btn(
        summon_i18n::text!("r2cd0f3be87"),
        "default",
        serde_json::json!({"action": "sentinel_dismiss", "proposal": proposal_id}),
    );
    closeable_card(
        serde_json::json!({
            "config": { "wide_screen_mode": true },
            "elements": [ md_el(&body), button_row(vec![confirm, dismiss]) ]
        })
        .to_string(),
    )
}

/// sentinel 开场卡:召唤完成后立即发进目标群,向在场的人交代「谁召唤了谁、要回答什么」——
/// bot 生成回答要几十秒,没有这张卡,被拉进群的人只能对着系统提示干等。
pub fn card_sentinel_intro(who: &str, agent_display: &str, ask: &str, history_n: usize) -> String {
    let body = summon_i18n::format!(
        "r67e4c7b0e6",
        escape_lark_md(who),
        escape_lark_md(agent_display),
        escape_lark_md(ask),
        escape_lark_md(agent_display),
        history_n
    );
    serde_json::json!({
        "config": { "wide_screen_mode": true },
        "elements": [ md_el(&body) ]
    })
    .to_string()
}

/// 确认卡的终态形(按钮置灰 + 结果行),confirm/dismiss/过期后原卡更新成它。
pub fn card_sentinel_confirm_done(summary: &str, outcome: &str) -> String {
    let body = summon_i18n::format!(
        "r0cc3f7e3de",
        escape_lark_md(summary),
        escape_lark_md(outcome)
    );
    closeable_card(
        serde_json::json!({
            "config": { "wide_screen_mode": true },
            "elements": [ md_el(&body), button_row(vec![cb_btn_disabled(summon_i18n::text!("r59f6c83692"), "default")]) ]
        })
        .to_string(),
    )
}

/// 私聊里的 external Claude 选择卡。candidate DTO 已由 bridge core 剥离 SID/cwd/config/transcript；
/// callback value 进一步限定为 action/ticket/choice 三个 opaque 字段，目标群永不下发客户端。
pub fn card_bridge_ticket(
    ticket_id: &str,
    candidates: &[crate::bridge::SafeBridgeCandidate],
    target_provider: crate::bridge::ClaudeProvider,
    now_ms: u64,
) -> String {
    use serde_json::json;

    let mut elements = vec![json!({
        "tag": "markdown",
        "content": summon_i18n::text!("r877a203e5e")
    })];
    if candidates.is_empty() {
        elements.push(json!({"tag": "markdown", "content": summon_i18n::text!("r47e61d0901")}));
    }
    for candidate in candidates {
        let takeover_hint = match candidate.handoff {
            crate::bridge::HandoffEligibility::Eligible => String::new(),
            crate::bridge::HandoffEligibility::MirrorOnly(reason) => {
                summon_i18n::format!("r7588142fb9", bridge_handoff_block_hint(reason))
            }
        };
        elements.push(json!({
            "tag": "markdown",
            "content": summon_i18n::format!(
                "r9bac640d36",
                escape_lark_md(&candidate.project_name),
                candidate.lifecycle.label(),
                crate::bridge::humanize_last_seen(candidate.last_seen_ms, now_ms),
                candidate.source_provider.label(),
                target_provider.label(),
                takeover_hint,
            )
        }));
        let mut buttons = Vec::new();
        if candidate.lifecycle == crate::bridge::ExternalLifecycle::ActiveIdle {
            buttons.push(cb_btn(
                "Mirror",
                "primary",
                json!({
                    "action": "bridge_mirror",
                    "ticket": ticket_id,
                    "choice": candidate.choice_id,
                }),
            ));
        }
        if candidate.handoff == crate::bridge::HandoffEligibility::Eligible {
            buttons.push(cb_btn_confirm(
                summon_i18n::text!("r5d7ce32f72"),
                "danger",
                json!({
                    "action": "bridge_handoff",
                    "ticket": ticket_id,
                    "choice": candidate.choice_id,
                }),
                &summon_i18n::format!(
                    "r54f5d9e620",
                    candidate.source_provider.label(),
                    target_provider.label(),
                ),
            ));
        }
        elements.push(button_row(buttons));
    }
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

fn bridge_handoff_block_hint(reason: crate::bridge::HandoffBlock) -> &'static str {
    use crate::bridge::HandoffBlock;

    match reason {
        HandoffBlock::TargetIncompatible => summon_i18n::text!("r74de26d809"),
        HandoffBlock::SessionBusy => summon_i18n::text!("ra35f2c7bae"),
        HandoffBlock::SessionEnded => summon_i18n::text!("r0507e7cb0e"),
        HandoffBlock::SessionConflict => summon_i18n::text!("rebc5f1636e"),
        HandoffBlock::UnverifiedProcess
        | HandoffBlock::MissingProcessFingerprint
        | HandoffBlock::UnverifiedEnvironment
        | HandoffBlock::UnverifiedProvider
        | HandoffBlock::IncompleteProfile
        | HandoffBlock::ExecutableMismatch
        | HandoffBlock::LocatorInvalid => summon_i18n::text!("r92a54ffd79"),
        HandoffBlock::CustomConfig => summon_i18n::text!("r0cdfabc2eb"),
    }
}

/// 私聊里的「会话召回」交互卡片:列出可召回的外部 Claude 会话(活跃 + 历史),点按钮把某个会话
/// 恢复为本 Bot 的托管会话。
///
/// 安全边界:按钮 callback value 只带不透明的 `action` + `sid` + 本机 observation UUID,
/// **绝不带 cwd/transcript/任务预览/进程指纹**——目标机器路径与起始任务只在私聊卡正文可读，
/// 不进 callback。活跃
/// 候选(有前台源)的「接管」按钮标 danger + 二次确认,说清会退出源终端里那个 Claude 进程。
///
/// `now_ms` 显式传入而不内部读时钟:与 [`card_bridge_ticket`] 一致,让相对时间可确定性单测、保持
/// core 纯函数(不碰时钟/IO)。
/// 发起 `/bridge` 的原群定位。恢复候选时用它把会话绑进群 G 的 canonical。
#[derive(Debug, Clone, Copy)]
pub struct CardRecallOrigin<'a> {
    pub chat_id: &'a str,
    pub chat_type: &'a str,
}

fn compact_recall_path(path: &str) -> String {
    let parts = std::path::Path::new(path)
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [] => summon_i18n::text!("re89982ec33").to_string(),
        [only] => (*only).to_string(),
        [left, right] => format!("{left}/{right}"),
        _ => format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]),
    }
}

const RECALL_FOLDED_TITLE_CHARS: usize = 36;

/// 折叠态只承担“扫描索引”，完整内容始终留在展开区。按 Unicode 字符裁剪，避免中文或 emoji
/// 被切坏；先压平空白再裁剪，防 transcript/name 中的换行把单条候选重新撑高。
fn compact_recall_title(title: &str) -> String {
    let compact = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= RECALL_FOLDED_TITLE_CHARS {
        return compact;
    }
    let mut clipped: String = compact
        .chars()
        .take(RECALL_FOLDED_TITLE_CHARS.saturating_sub(1))
        .collect();
    clipped.push('…');
    clipped
}

/// 确认框里最多点名几个后台任务,其余写「等 N 个」。
const RECALL_TAKEOVER_LISTED_TASKS: usize = 3;

/// 活跃 Claude 候选「接管」的确认文案。会话忙时先说代价:正在跑的这一轮会被打断,后台任务
/// 随源进程一起停(Claude Code 退出时会结束它们),续上后 Claude 从自己的任务通知里知道这件事。
/// 无清单的忙碌文案不比空闲文案长:极限页(10 条活跃候选)只剩几百字节余量,清单另由调用方按
/// 预算收缩。
fn recall_takeover_confirm_text(item: &crate::recall::SessionCandidate) -> String {
    if item.live_status.as_deref() != Some("busy") {
        return summon_i18n::text!("r8f64a12822").to_string();
    }
    let tasks = &item.background_tasks;
    if tasks.is_empty() {
        return summon_i18n::text!("r7677b53ae1").to_string();
    }
    let mut listed = tasks
        .iter()
        .take(RECALL_TAKEOVER_LISTED_TASKS)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("、");
    if tasks.len() > RECALL_TAKEOVER_LISTED_TASKS {
        listed.push_str(&summon_i18n::format!("r20dd1bf5c2", tasks.len()));
    }
    summon_i18n::format!("r049f78b4bc")
}

pub fn card_recall(
    provider: crate::recall::RecallProvider,
    items: &[crate::recall::SessionCandidate],
    page: usize,
    total: usize,
    total_pages: usize,
    now_ms: u64,
    origin: Option<CardRecallOrigin<'_>>,
) -> String {
    card_recall_with_remote(
        provider,
        items,
        page,
        total,
        total_pages,
        now_ms,
        origin,
        &RecallRemoteMarks::default(),
    )
}

/// 召回卡上与 cmux 遥控有关的两组 Claude 会话 sid,都由调用方判定。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallRemoteMarks {
    /// 活跃候选,在「接管」旁多一个「遥控」按钮。
    pub offer: std::collections::BTreeSet<String>,
    /// 正在被遥控的会话:「接管」「恢复」「遥控」都不出,只给禁用的「遥控中」。
    pub controlled: std::collections::BTreeSet<String>,
}

/// 同 [`card_recall`],按 `remote` 给 Claude 候选加「遥控」按钮或换成禁用的「遥控中」。
/// 哪些 sid 能遥控(跑在 cmux 里、有目标群)、哪些正在遥控、整卡是否放得下这些按钮,都由
/// 调用方判定:卡片之后还要加绑定字段与来源标注,只有调用方量得到真正发出去的大小。
#[allow(clippy::too_many_arguments)] // 参数逐个由调用方注入
pub fn card_recall_with_remote(
    provider: crate::recall::RecallProvider,
    items: &[crate::recall::SessionCandidate],
    page: usize,
    total: usize,
    total_pages: usize,
    now_ms: u64,
    origin: Option<CardRecallOrigin<'_>>,
    remote: &RecallRemoteMarks,
) -> String {
    use serde_json::json;

    // 发起 `/bridge` 的原群(chat_id + chat_type):作为**不透明**载荷进按钮 value,让私聊卡的回调
    // 知道要把会话恢复进哪个群 G。chat_id 非密;真正的鉴权在回调内对目标群 operate-gate,不信 value。
    let origin_fields = |value: &mut serde_json::Value| {
        if let Some(origin) = origin {
            if let Some(object) = value.as_object_mut() {
                object.insert("chat_id".into(), json!(origin.chat_id));
                object.insert("chat_type".into(), json!(origin.chat_type));
            }
        }
    };

    let mut elements = vec![json!({
        "tag": "markdown",
        "content": summon_i18n::format!(
            "rab864a1aa7",
            provider.label(),
            items.len(),
            total,
            page,
            total_pages,
        )
    })];
    if items.is_empty() {
        elements.push(json!({
            "tag": "markdown",
            "content": summon_i18n::format!("r9ec47fd493", provider.label())
        }));
    }
    for item in items {
        // 卡片只能承载同一 provider 的候选。发现侧若违反该契约，宁可跳过异类项，也不能生成
        // 一张写着 Cursor、实际 callback 却指向 Codex 的串线卡。
        if item.provider != provider {
            continue;
        }
        // 起始任务比 Claude 自动 name/SID 更能区分同 cwd 会话；拿不到时才回退 name。
        let raw_title = if !item.prompt_preview.is_empty() {
            item.prompt_preview.as_str()
        } else if item.name.is_empty() {
            summon_i18n::text!("r452334e466")
        } else {
            item.name.as_str()
        };
        // 外部文本分别在“裁剪前原文”和“裁剪后索引”上消毒；不能先消毒再按字符裁剪，否则可能
        // 把「`<`+零宽空格」切开、让尾部的 `<` 重新贴上后文变回标签形态。展开区保留完整起始
        // 任务，折叠态只显示短索引。
        let title = escape_lark_md(raw_title);
        let folded_title = escape_lark_md(&compact_recall_title(raw_title));
        let last_seen = crate::bridge::humanize_last_seen(item.last_active_ms, now_ms);
        // 折叠态只保留状态+相对时间；路径和 Claude name 放展开区，避免 10 条列表重复堆三行。
        let status = if item.ownership == crate::recall::RecallOwnership::ManagedRpc {
            summon_i18n::format!("rf27255d8c2")
        } else if item.is_active() {
            summon_i18n::format!("r703973f45e")
        } else {
            summon_i18n::format!("re752ae8d31")
        };
        let mut detail_metadata = format!("📁 {}", escape_lark_md(&compact_recall_path(&item.cwd)));
        if !item.name.is_empty() {
            detail_metadata.push_str(summon_i18n::text!("r4180c236a6"));
            detail_metadata.push_str(&escape_lark_md(&item.name));
        }
        let detail = format!("{title}\n\n<font color='grey'>{detail_metadata}</font>");
        let panel = json!({
            "tag": "collapsible_panel",
            "expanded": false,
            "header": {
                "title": {
                    "tag": "markdown",
                    "content": format!("**{folded_title}**\n<font color='grey'>{status}</font>")
                },
                "vertical_align": "center"
            },
            "border": {"color": "grey", "corner_radius": "5px"},
            "vertical_spacing": "4px",
            "padding": "6px 8px",
            "elements": [{"tag": "markdown", "content": detail}]
        });
        // callback 只带不透明 action + sid + observation UUID(+ 原群 origin);cwd/transcript/
        // 进程指纹不进 callback。observation 的真实内容只落本机 0600 文件。
        let action = if item.ownership == crate::recall::RecallOwnership::ManagedRpc {
            "recall_rpc_handoff"
        } else {
            "recall_resume"
        };
        let mut resume_value = json!({
            "action": action,
            "cli": item.provider.as_str(),
            "sid": item.sid
        });
        if let Some(observation_id) = &item.source_observation_id {
            resume_value["source_observation"] = json!(observation_id);
        }
        if item.ownership == crate::recall::RecallOwnership::ManagedRpc {
            resume_value["observed_at_ms"] = json!(now_ms);
        }
        origin_fields(&mut resume_value);
        let buttons = match (item.ownership, item.provider, item.is_active()) {
            (crate::recall::RecallOwnership::ManagedRpc, _, _) => vec![cb_btn_confirm(
                summon_i18n::text!("rc175e224c8"),
                "primary",
                resume_value,
                summon_i18n::text!("r12efbebc8d"),
            )],
            // 接管会结束遥控中的进程,恢复会给它另起一个 writer:正在遥控的会话一个回调都不给。
            (_, crate::recall::RecallProvider::Claude, _)
                if remote.controlled.contains(&item.sid) =>
            {
                vec![cb_btn_disabled(
                    summon_i18n::text!("r248cc95df8"),
                    "default",
                )]
            }
            (_, crate::recall::RecallProvider::Claude, true) => {
                // Claude 有 transcript 稳定窗 + exact PID 身份闸，允许安全退出源后接管。
                let mut buttons = vec![cb_btn_confirm(
                    summon_i18n::text!("rea878d5d09"),
                    "danger",
                    resume_value,
                    &recall_takeover_confirm_text(item),
                )];
                if remote.offer.contains(&item.sid) {
                    let mut remote_value = json!({
                        "action": "recall_cmux_remote",
                        "cli": item.provider.as_str(),
                        "sid": item.sid
                    });
                    origin_fields(&mut remote_value);
                    buttons.push(cb_btn_confirm(
                        summon_i18n::text!("rf1177073c4"),
                        "primary",
                        remote_value,
                        summon_i18n::text!("r0bccc525a0"),
                    ));
                }
                buttons
            }
            (_, _, true) => {
                // Cursor/Codex 没有与 Claude 等价的 source ACK 证明。活跃 writer 只展示，
                // 不下发 callback capability；用户退出源后刷新即可按历史会话恢复。
                vec![cb_btn_disabled(
                    summon_i18n::text!("rfa48e89389"),
                    "default",
                )]
            }
            (_, _, false) => vec![cb_btn(
                summon_i18n::text!("re0534b8a4e"),
                "primary",
                resume_value,
            )],
        };
        let mut columns = vec![json!({
            "tag": "column",
            "width": "weighted",
            "weight": 1,
            "vertical_align": "top",
            "elements": [panel]
        })];
        columns.extend(buttons.into_iter().map(|button| {
            json!({
                "tag": "column",
                "width": "auto",
                "vertical_align": "top",
                "elements": [button]
            })
        }));
        elements.push(json!({
            "tag": "column_set",
            "flex_mode": "flow",
            "horizontal_spacing": "8px",
            "columns": columns
        }));
    }
    // 分页:仅多页时给翻页按钮;首页无「上一页」、末页无「下一页」。翻页纯读,Task 3 回调无需 gate。
    if total_pages > 1 {
        let mut pager = Vec::new();
        if page > 1 {
            let mut prev = json!({
                "action": "recall_page",
                "cli": provider.as_str(),
                "page": page - 1
            });
            origin_fields(&mut prev);
            pager.push(cb_btn(summon_i18n::text!("rc9b9ae7a61"), "default", prev));
        }
        if page < total_pages {
            let mut next = json!({
                "action": "recall_page",
                "cli": provider.as_str(),
                "page": page + 1
            });
            origin_fields(&mut next);
            pager.push(cb_btn(summon_i18n::text!("r8a8542f696"), "default", next));
        }
        if !pager.is_empty() {
            elements.push(button_row(pager));
        }
    }
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// Bridge 卡片裁剪标记。放在已经转义的 external markdown 后，属于 summon 固定模板。
pub const BRIDGE_TRUNCATED_NOTICE: &str = "r6fce88ca65";
const BRIDGE_TURN_USER_JSON_BUDGET: usize = 6_500;
const BRIDGE_TURN_ASSISTANT_JSON_BUDGET: usize = 15_500;
const BRIDGE_CONTEXT_MAX_MESSAGES: usize = 12;
const BRIDGE_CONTEXT_RAW_BUDGET: usize = 16 * 1024;
const BRIDGE_CONTEXT_MESSAGE_JSON_BUDGET: usize = 20_000;

fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn json_string_len(text: &str) -> usize {
    // String 序列化不会失败；错误时用 MAX 令预算判定 fail-closed。
    serde_json::to_string(text).map_or(usize::MAX, |json| json.len())
}

/// 在原始 UTF-8 边界上找最长前缀，再消毒为不带 mention 的 lark_md。预算按最终 JSON string
/// 计算，因而同时覆盖 `<字母` 的零宽空格膨胀与换行、引号的 JSON 转义膨胀。
fn clip_external_lark_md(text: &str, json_budget: usize) -> String {
    let escaped = escape_lark_md(text);
    if json_string_len(&escaped) <= json_budget {
        return escaped;
    }

    let boundaries = text
        .char_indices()
        .map(|(index, ch)| index + ch.len_utf8())
        .collect::<Vec<_>>();
    let mut low = 0usize;
    let mut high = boundaries.len();
    while low < high {
        let mid = low + (high - low) / 2;
        let candidate = format!(
            "{}{}",
            escape_lark_md(&text[..boundaries[mid]]),
            ::summon_core::i18n::label(BRIDGE_TRUNCATED_NOTICE)
        );
        if json_string_len(&candidate) <= json_budget {
            low = mid + 1;
        } else {
            high = mid;
        }
    }

    let end = low
        .checked_sub(1)
        .and_then(|index| boundaries.get(index).copied())
        .unwrap_or(0);
    format!(
        "{}{}",
        escape_lark_md(&text[..end]),
        ::summon_core::i18n::label(BRIDGE_TRUNCATED_NOTICE)
    )
}

fn render_external_bridge_turn(
    user_text: &str,
    assistant_text: &str,
    user_budget: usize,
    assistant_budget: usize,
) -> (String, usize, usize) {
    use serde_json::json;

    let user = clip_external_lark_md(user_text, user_budget);
    let assistant = clip_external_lark_md(assistant_text, assistant_budget);
    let user_len = json_string_len(&user);
    let assistant_len = json_string_len(&assistant);
    let elements = vec![
        json!({"tag": "markdown", "content": summon_i18n::text!("r065895503a")}),
        json!({"tag": "markdown", "content": user}),
        json!({"tag": "hr"}),
        json!({"tag": "markdown", "content": "🤖 **Claude**"}),
        json!({"tag": "markdown", "content": assistant}),
    ];
    (
        json!({"schema": "2.0", "body": {"elements": elements}}).to_string(),
        user_len,
        assistant_len,
    )
}

/// 一轮 external Claude 镜像的唯一网络载荷。用户与 Claude 正文固定在同一张卡里且角色顺序
/// 固定，重试只能整卡重试，不能出现 assistant-before-user。正文只走 [`escape_lark_md`] 与
/// UTF-8 安全裁剪，刻意不调用 mention renderer。
pub fn card_external_bridge_turn(user_text: &str, assistant_text: &str) -> String {
    let mut user_budget = BRIDGE_TURN_USER_JSON_BUDGET;
    let mut assistant_budget = BRIDGE_TURN_ASSISTANT_JSON_BUDGET;
    loop {
        let (card, user_len, assistant_len) =
            render_external_bridge_turn(user_text, assistant_text, user_budget, assistant_budget);
        if card.len() <= CARD_TOTAL_SAFE_LIMIT {
            return card;
        }

        let shrink = card
            .len()
            .saturating_sub(CARD_TOTAL_SAFE_LIMIT)
            .saturating_add(128);
        if assistant_len >= user_len && assistant_budget > 128 {
            assistant_budget = assistant_budget.saturating_sub(shrink).max(128);
        } else if user_budget > 128 {
            user_budget = user_budget.saturating_sub(shrink).max(128);
        } else if assistant_budget > 128 {
            assistant_budget = assistant_budget.saturating_sub(shrink).max(128);
        } else {
            // 两段都已缩到固定模板量级；此时只有卡片结构自身超限才可能到达这里。
            return card;
        }
    }
}

/// Activating → MirrorBound 之前必须先送达的群审计卡。只接受服务端生成的安全目标标签，不接受
/// external body 参数，从 API 形状上避免把 prompt/answer 提前带进审计事件。标签与私聊控制卡
/// 一致，让用户能从目标群反查哪张卡控制本群。
pub fn card_bridge_activation_audit(target_label: &str) -> String {
    let target_label = escape_lark_md(utf8_prefix(target_label.trim(), 96));
    let card = serde_json::json!({
        "schema": "2.0",
        "body": {"elements": [{
            "tag": "markdown",
            "content": summon_i18n::format!("r95b5ce8c79", target_label)
        }]}
    })
    .to_string();
    closeable_card(card)
}

/// 私聊控制卡使用的安全目标标签：展示 agent 与不可逆群标识，足够区分多绑定，但不泄露 raw
/// chat_id、SID、cwd。标签不是鉴权凭据，哈希只承担人类辨识。
pub fn bridge_target_label(agent_name: &str, chat_id: &str) -> String {
    let hash = chat_id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    let agent = utf8_prefix(agent_name.trim(), 48);
    summon_i18n::format!("r7c75258233", agent)
}

/// MirrorBound 后保留在私聊里的控制卡。两个回调 value 均严格只有 action/ticket/choice，
/// target、SID、cwd 等服务端私有信息不下发；读取上下文也可能暴露敏感正文，因此同样二次确认。
pub fn card_bridge_mirror_control(ticket_id: &str, choice_id: &str, target_label: &str) -> String {
    use serde_json::json;

    let buttons = vec![
        cb_btn_confirm(
            summon_i18n::text!("r9d73c86255"),
            "primary",
            json!({
                "action": "bridge_context",
                "ticket": ticket_id,
                "choice": choice_id,
            }),
            summon_i18n::text!("r83a8e9d197"),
        ),
        cb_btn_confirm(
            summon_i18n::text!("ra21e29db50"),
            "danger",
            json!({
                "action": "bridge_stop",
                "ticket": ticket_id,
                "choice": choice_id,
            }),
            summon_i18n::text!("r3fb2344c3b"),
        ),
    ];
    let target_label = escape_lark_md(utf8_prefix(target_label.trim(), 96));
    let elements = vec![
        json!({
            "tag": "markdown",
            "content": summon_i18n::format!("r718e360d1f", target_label)
        }),
        button_row(buttons),
    ];
    let card = json!({"schema": "2.0", "body": {"elements": elements}}).to_string();
    closeable_card(card)
}

/// 上下文胶囊卡。只消费 transcript parser 的强类型可见消息，防御性保留最近 12 条、原文
/// 16KB；实际 JSON 超限时继续从最旧消息开始收缩，始终保住最新可见上下文。
pub fn card_bridge_context_capsule(
    capsule: &crate::parse::claude_transcript::ContextCapsule,
) -> String {
    use crate::parse::claude_transcript::CapsuleRole;
    use serde_json::json;

    let mut remaining = BRIDGE_CONTEXT_RAW_BUDGET;
    let mut selected = Vec::new();
    let mut truncated = capsule.truncated || capsule.messages.len() > BRIDGE_CONTEXT_MAX_MESSAGES;
    for message in capsule
        .messages
        .iter()
        .rev()
        .take(BRIDGE_CONTEXT_MAX_MESSAGES)
    {
        if remaining == 0 {
            truncated = true;
            break;
        }
        let text = utf8_prefix(&message.text, remaining);
        truncated |= text.len() < message.text.len();
        remaining = remaining.saturating_sub(text.len());
        selected.push((message.role, text.to_string()));
    }
    selected.reverse();

    let render = |messages: &[(CapsuleRole, String)], was_truncated: bool| {
        let mut elements = vec![json!({
            "tag": "markdown",
            "content": summon_i18n::text!("rd5ee500af9")
        })];
        if was_truncated {
            elements.push(json!({
                "tag": "markdown",
                "content": summon_i18n::text!("rf690aafe80")
            }));
        }
        if messages.is_empty() {
            elements.push(json!({"tag": "markdown", "content": summon_i18n::text!("re772bc5503")}));
        }
        for (role, text) in messages {
            let label = match role {
                CapsuleRole::User => summon_i18n::text!("r065895503a"),
                CapsuleRole::Assistant => "🤖 **Claude**",
            };
            elements.push(json!({"tag": "markdown", "content": label}));
            elements.push(json!({
                "tag": "markdown",
                "content": clip_external_lark_md(text, BRIDGE_CONTEXT_MESSAGE_JSON_BUDGET)
            }));
        }
        json!({"schema": "2.0", "body": {"elements": elements}}).to_string()
    };

    let mut card = render(&selected, truncated);
    while card.len() > CARD_TOTAL_SAFE_LIMIT && selected.len() > 1 {
        selected.remove(0);
        truncated = true;
        card = render(&selected, truncated);
    }
    if card.len() <= CARD_TOTAL_SAFE_LIMIT {
        return card;
    }

    // 单条消息仍因实体/JSON 转义膨胀超限：按最终 JSON 预算再收缩，保留最新消息的前缀。
    if let Some((role, text)) = selected.last() {
        // 最坏每个 `&` 转为 5 字节实体，3KB 原文仍为固定安全余量；render 内只转义一次。
        card = render(&[(*role, utf8_prefix(text, 3_000).to_string())], true);
    }
    card
}

/// 定时器创建确认文案(markdown)。节律说人话,与列表卡 / CLI 同一个渲染器。
pub fn schedule_confirm_markdown(s: &crate::schedule::Schedule) -> String {
    summon_i18n::format!(
        "r01437cbc38",
        s.id,
        crate::schedule::human_readable(s),
        s.output.label(),
        schedule_context_note(s),
        s.agent,
        s.chat_id,
        s.prompt
    )
}

/// 起跑前重置会话的定时器,在确认文案里多说一行;缺省续用不显示。
fn schedule_context_note(s: &crate::schedule::Schedule) -> &'static str {
    match s.context {
        crate::schedule::ScheduleContext::Fresh => summon_i18n::text!("r7fff3f04a4"),
        crate::schedule::ScheduleContext::Resume => "",
    }
}

/// 定时器修改确认文案(markdown)。
pub fn schedule_saved_markdown(s: &crate::schedule::Schedule, rescheduled: bool) -> String {
    let mut tail = String::new();
    // 改了节律必须明说「从下一个周期开始」:用户改完最担心的就是「它会不会立刻补跑一次」。
    if rescheduled {
        tail.push_str(summon_i18n::text!("r89a3a1032c"));
    }
    // 已归档的条目改完照样不跑(`due_plan` 第一行就挡掉),不说清楚就是一句骗人的「已更新」。
    if !s.enabled {
        tail.push_str(summon_i18n::text!("r30ae7cb3e6"));
    }
    summon_i18n::format!(
        "rc9a1d26db3",
        s.id,
        crate::schedule::human_readable(s),
        s.output.label(),
        schedule_context_note(s),
        s.prompt,
        tail
    )
}

/// 起点卡:内容输入框 + 两行频率按钮。**五颗按钮都是表单提交键**——点频率的同时把已填内容一并
/// 带走,参数卡再预填回去。这正是「daemon 不存草稿」能成立的原因:状态一直在用户手上那张卡里。
///
/// `edit_id` 非空 = 正在改某条已有定时器(从参数卡点「换个频率」回来),后续动作要把 id 带下去。
pub fn card_schedule_start(prompt: &str, edit_id: Option<&str>) -> String {
    use crate::schedule::form::CardFreq;
    use serde_json::json;
    let title = match edit_id {
        Some(id) => summon_i18n::format!("r5a1c354a0b"),
        None => summon_i18n::text!("r49e6bae330").to_string(),
    };
    let freq_btn = |freq: CardFreq| {
        let mut value = json!({"action": "schedule_pick", "kind": freq.as_str()});
        if let Some(id) = edit_id {
            value["id"] = json!(id);
        }
        json!({
            "tag": "button",
            "text": {"tag": "plain_text", "content": freq.label()},
            "type": "default",
            "behaviors": [{"type": "callback", "value": value}],
            "form_action_type": "submit",
            "name": format!("pick_{}", freq.as_str())
        })
    };
    let elements = vec![json!({
        "tag": "form",
        "name": "sched_start",
        "elements": [
            json!({"tag": "markdown", "content": title}),
            prompt_input(prompt),
            json!({"tag": "markdown", "content": summon_i18n::text!("r33fef69cae")}),
            // 「过多久」放第一颗:定一次性提醒时,「20 分钟后」远比滚到某个绝对时刻常见。
            button_row(vec![
                freq_btn(CardFreq::After),
                freq_btn(CardFreq::Once),
                freq_btn(CardFreq::Daily),
            ]),
            button_row(vec![
                freq_btn(CardFreq::Weekday),
                freq_btn(CardFreq::Weekly),
                freq_btn(CardFreq::Every),
            ]),
        ]
    })];
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// 参数卡的全部输入。字段多且都可选,用结构体而非长参数列表(与 `TaskCard` 同范式)。
#[derive(Clone, Copy)]
pub struct ScheduleParamsCard<'a> {
    pub freq: crate::schedule::form::CardFreq,
    pub pre: &'a crate::schedule::form::Prefill,
    /// 非空 = 在改这一条;它会一路带到提交键与「换个频率」键上。
    pub edit_id: Option<&'a str>,
    /// 非空 = 在卡上显示一行「当前:…」(编辑态的原值)。
    pub current: Option<&'a str>,
    /// 非空 = 上次提交没过校验,顶部显示这行原因。
    pub error: Option<&'a str>,
}

/// 参数卡:按频率只摆该频率真正需要的控件,一次提交建成 / 存回。
///
/// `current` 非空(编辑态)时在卡上写一行「当前:…」。这不是装饰——四个选择器控件的**初始值字段名
/// 本仓没有真机证据**,万一预填没生效,这行文本仍能让用户看见原值,不至于盲改。
///
/// `error` 非空 = 上一次提交没过校验:**整张卡按用户刚填的内容重画**,顶部加一行错误。绝不把卡换成
/// 一句纯文本错误——填错时间是最常见的一次失误,代价不该是把正文星期全部重填一遍。
pub fn card_schedule_params(card: &ScheduleParamsCard) -> String {
    use crate::schedule::form::{field, CardFreq};
    use serde_json::json;

    let ScheduleParamsCard {
        freq,
        pre,
        edit_id,
        current,
        error,
    } = *card;

    let title = match edit_id {
        Some(id) => summon_i18n::format!("rd3e247ab2d", id, freq.label()),
        None => summon_i18n::format!("rdf62cd060f", freq.label()),
    };
    let mut elements = vec![json!({"tag": "markdown", "content": title})];
    if let Some(e) = error {
        elements.push(json!({"tag": "markdown", "content": format!("⚠️ {e}")}));
    }
    if let Some(cur) = current {
        elements.push(json!({"tag": "markdown", "content": summon_i18n::format!("rf919a8b8c1")}));
    }
    elements.push(labeled(
        summon_i18n::text!("r7f8d44adea"),
        prompt_input(&pre.prompt),
    ));
    elements.push(labeled(
        summon_i18n::text!("r8c6a8f237b"),
        schedule_output_select(field::OUTPUT, pre.output),
    ));

    match freq {
        CardFreq::Once => {
            elements.push(labeled(
                summon_i18n::text!("ra0e5ebf35b"),
                date_picker_el(field::DATE, &pre.date),
            ));
            elements.push(labeled(
                summon_i18n::text!("rc67d6914bd"),
                time_picker_el(field::TIME, &pre.time),
            ));
        }
        CardFreq::Daily | CardFreq::Weekday => elements.push(labeled(
            summon_i18n::text!("rc67d6914bd"),
            time_picker_el(field::TIME, &pre.time),
        )),
        CardFreq::Weekly => {
            elements.push(labeled(
                summon_i18n::text!("re1aacf924b"),
                weekday_multi_select(field::DAYS, &pre.days),
            ));
            elements.push(labeled(
                summon_i18n::text!("rc67d6914bd"),
                time_picker_el(field::TIME, &pre.time),
            ));
        }
        CardFreq::After | CardFreq::Every => {
            let label = if freq == CardFreq::After {
                summon_i18n::text!("re0f10b3654")
            } else {
                summon_i18n::text!("r100107ffb5")
            };
            elements.push(labeled(
                label,
                json!({
                    "tag": "input",
                    "name": field::EVERY_N,
                    "default_value": pre.every_n,
                    "placeholder": {"tag": "plain_text", "content": "20"}
                }),
            ));
            elements.push(unit_select(field::EVERY_UNIT, &pre.every_unit));
        }
    }

    let (submit_label, submit_action) = match edit_id {
        Some(_) => (summon_i18n::text!("ra3030bf8f1"), "schedule_save"),
        None => (summon_i18n::text!("rcde2cd071d"), "schedule_create"),
    };
    let mut submit_value = json!({"action": submit_action, "kind": freq.as_str()});
    let mut back_value = json!({"action": "schedule_new"});
    if let Some(id) = edit_id {
        submit_value["id"] = json!(id);
        back_value["id"] = json!(id);
    }
    elements.push(button_row(vec![
        json!({
            "tag": "button",
            "text": {"tag": "plain_text", "content": submit_label},
            "type": "primary",
            "behaviors": [{"type": "callback", "value": submit_value}],
            "form_action_type": "submit",
            "name": "sched_submit"
        }),
        json!({
            "tag": "button",
            "text": {"tag": "plain_text", "content": summon_i18n::text!("r3443bc8d83")},
            "type": "default",
            "behaviors": [{"type": "callback", "value": back_value}],
            "form_action_type": "submit",
            "name": "sched_back"
        }),
    ]));

    closeable_card(
        json!({
            "schema": "2.0",
            "body": {"elements": [{"tag": "form", "name": "sched_params", "elements": elements}]}
        })
        .to_string(),
    )
}

/// Claude 五小时会话额度终态卡：把 provider 给出的重置时刻换算成本机绝对时间，复用一次性
/// 定时器参数卡预填「重置后两分钟」。卡片只提供创建入口，不在用户点击前写入定时器。
pub fn card_claude_session_limit(
    limit: &crate::claude_session_limit::ClaudeSessionLimit,
) -> String {
    use crate::schedule::form::{CardFreq, Prefill};
    use serde_json::json;

    let reminder = limit.reminder_at().with_timezone(&chrono::Local);
    let pre = Prefill {
        prompt: summon_i18n::text!("r0dead2d6ae").to_string(),
        time: reminder.format("%H:%M").to_string(),
        date: reminder.format("%Y-%m-%d").to_string(),
        ..Default::default()
    };
    let base = card_schedule_params(&ScheduleParamsCard {
        freq: CardFreq::Once,
        pre: &pre,
        edit_id: None,
        current: None,
        error: None,
    });
    let Ok(mut card) = serde_json::from_str::<serde_json::Value>(&base) else {
        return closeable_card(card_markdown_v2(summon_i18n::text!("r30653aa1b4")));
    };
    if let Some(elements) = card
        .pointer_mut("/body/elements/0/elements")
        .and_then(serde_json::Value::as_array_mut)
    {
        elements.insert(
            1,
            json!({
                "tag": "markdown",
                "content": summon_i18n::format!(
                    "r72cc7147bd",
                    limit.reset_time,
                    limit.timezone,
                    reminder.format("%Y-%m-%d %H:%M %:z")
                )
            }),
        );
    }
    card.to_string()
}

/// Claude 自动恢复状态卡：展示额度重置时刻与续跑方式。
pub fn card_claude_session_limit_auto_resume(
    limit: &crate::claude_session_limit::ClaudeSessionLimit,
) -> String {
    let reset = limit.reset_at.with_timezone(&chrono::Local);
    closeable_card(card_markdown_v2(&summon_i18n::format!(
        "r6c6444f67a",
        reset.format("%Y-%m-%d %H:%M %:z"),
    )))
}

/// 定时器**建成 / 存好之后**的终态卡:保留参数卡原貌,把每个控件置灰,底部加分隔线 + 回执。
///
/// why 不换成一句纯文本:那样用户看不到自己刚填了什么,回头核对只能去 `/schedules`。更要紧的是
/// **控件必须真的点不动**——一张还能提交的卡放在聊天记录里,滚上去再点一次就会重复建一条,
/// 或者拿过期的值覆盖已经改好的定时器。与会话退出卡 / 交互菜单终态卡同一范式。
pub fn card_schedule_settled(card: &ScheduleParamsCard, receipt: &str) -> String {
    let mut v: serde_json::Value = match serde_json::from_str(&card_schedule_params(card)) {
        Ok(v) => v,
        // 渲染出的卡片一定是合法 JSON(刚由 json! 生成);真出了意外就退化成纯回执,不 panic。
        Err(_) => return card_markdown_v2(receipt),
    };
    disable_interactive(&mut v);
    if let Some(elements) = v["body"]["elements"].as_array_mut() {
        elements.push(serde_json::json!({"tag": "hr"}));
        elements.push(serde_json::json!({"tag": "markdown", "content": receipt}));
    }
    with_outcome_feed_summary(v, receipt)
}

/// 参数卡的字段说明行。
///
/// why 不用控件自带的 `label` 属性:v2 的字段表里并没有为每个选择器都列出 `label`,而卡片 2.0 对
/// **不支持的属性是报错而非忽略**——赌错一个属性名就是整张卡发不出去。仓内既有的 `input` 用法
/// (工作目录 / 评论表单)也从不用 label。一行 markdown 表达力一样,风险为零。
fn labeled(label: &str, control: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "tag": "column_set",
        "flex_mode": "stretch",
        "horizontal_spacing": "8px",
        "columns": [
            {"tag": "column", "width": "auto", "elements": [
                {"tag": "markdown", "content": format!("**{label}**")}
            ]},
            {"tag": "column", "width": "weighted", "weight": 1, "elements": [control]}
        ]
    })
}

/// 任务正文输入框。起点卡与参数卡共用,`name` 必须一致——两张卡之间靠它传递已填内容。
fn prompt_input(default: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "input",
        "name": crate::schedule::form::field::PROMPT,
        "default_value": default,
        "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("r326978fd60")}
    })
}

fn time_picker_el(name: &str, initial: &str) -> serde_json::Value {
    let mut v = serde_json::json!({
        "tag": "picker_time",
        "name": name,
        "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("r7410d4d667")}
    });
    if !initial.is_empty() {
        v["initial_time"] = serde_json::json!(initial);
    }
    v
}

fn date_picker_el(name: &str, initial: &str) -> serde_json::Value {
    let mut v = serde_json::json!({
        "tag": "date_picker",
        "name": name,
        "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("r556b0ef212")}
    });
    if !initial.is_empty() {
        v["initial_date"] = serde_json::json!(initial);
    }
    v
}

/// 星期多选。option 的 `value` 直接用 cron 的周编号(0=周日),解析侧 parse 即可,不做名字映射。
fn weekday_multi_select(name: &str, selected: &[u32]) -> serde_json::Value {
    const NAMES: [&str; 7] = [
        "ree239f3943",
        "rc430f4c12e",
        "rd1378a68e6",
        "r2961168962",
        "re9d3b01a5a",
        "r572388f3b0",
        "rf9aa11dbb1",
    ];
    let options: Vec<serde_json::Value> = (0..7u32)
        .map(|i| {
            serde_json::json!({
                "text": {"tag": "plain_text", "content": crate::i18n::label(NAMES[i as usize])},
                "value": i.to_string()
            })
        })
        .collect();
    let mut v = serde_json::json!({
        "tag": "multi_select_static",
        "name": name,
        "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("r4714b033a1")},
        "options": options
    });
    if !selected.is_empty() {
        let picked: Vec<String> = selected.iter().map(|d| d.to_string()).collect();
        v["selected_values"] = serde_json::json!(picked);
    }
    v
}

/// 间隔单位下拉。**新建时默认选中「小时」**——不给默认值的话,用户不动它就会撞上
/// 「看不懂的单位「」」这种自己没做错什么却被拒的提示。
fn unit_select(name: &str, initial: &str) -> serde_json::Value {
    let options: Vec<serde_json::Value> = [
        ("m", summon_i18n::text!("rbd957bc497")),
        ("h", summon_i18n::text!("r3b6fefc50f")),
        ("d", summon_i18n::text!("r49da61ceee")),
    ]
    .iter()
    .map(|(val, label)| {
        serde_json::json!({
            "text": {"tag": "plain_text", "content": label},
            "value": val
        })
    })
    .collect();
    let picked = if initial.is_empty() { "h" } else { initial };
    serde_json::json!({
        "tag": "select_static",
        "name": name,
        "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("r3b6fefc50f")},
        "options": options,
        "initial_option": picked
    })
}

fn schedule_output_select(
    name: &str,
    selected: crate::schedule::ScheduleOutput,
) -> serde_json::Value {
    let options: Vec<serde_json::Value> = [
        ("progress", summon_i18n::text!("r1fd525f2e1")),
        ("result", summon_i18n::text!("ra3b407f03d")),
    ]
    .iter()
    .map(|(value, label)| {
        serde_json::json!({
            "text": {"tag": "plain_text", "content": label},
            "value": value
        })
    })
    .collect();
    let selected = match selected {
        crate::schedule::ScheduleOutput::Progress => "progress",
        crate::schedule::ScheduleOutput::Result => "result",
    };
    serde_json::json!({
        "tag": "select_static",
        "name": name,
        "placeholder": {"tag": "plain_text", "content": summon_i18n::text!("r1fd525f2e1")},
        "options": options,
        "initial_option": selected
    })
}

/// 定时器列表卡片(飞书 Card JSON 2.0)。
///
/// **只列在跑的**,已归档的折成底部一行计数。`enabled=false` 就是归档态——它只有两个来源:
/// 用户点「归档」、或一次性定时器跑完自动收尾,两者语义一致,不值得再加一个持久化字段去区分
/// 「暂停」和「归档」(那需要一个「恢复」动作才有意义,而现在没有)。
///
/// why 折叠而不是继续列出来:定时器只增不减,已归档的堆在列表里会一直往下长,几十条之后既看不清
/// 在跑的是哪些,也会撞上飞书卡片的组件数上限。要翻旧账走 `summon schedules ls`(那里列全量)。
pub fn card_schedules_panel(all: &[crate::schedule::Schedule], timer_reset: bool) -> String {
    use serde_json::json;
    // 「＋ 新建定时」在空表与非空表都要给:空表恰恰是最需要建一条的时候。
    let new_btn = || {
        button_row(vec![cb_btn(
            summon_i18n::text!("r5ae436062a"),
            "primary",
            json!({"action": "schedule_new"}),
        )])
    };
    // 「定时归档」开关:显示本 chat 当前生效值,点击翻转。续聊 = 保留 resume 记录、下次触发
    // 接回上下文;重置 = 清 resume 记录、下次触发全新 session。
    let timer_reset_btn = || {
        button_row(vec![cb_btn(
            &summon_i18n::format!(
                "ra709576e6c",
                if timer_reset {
                    summon_i18n::text!("r5292ab1a36")
                } else {
                    summon_i18n::text!("r498e1d59b4")
                }
            ),
            "default",
            json!({"action": "schedule_timer_reset_toggle"}),
        )])
    };
    let live: Vec<&crate::schedule::Schedule> = all.iter().filter(|s| s.enabled).collect();
    let archived = all.len() - live.len();
    // 归档计数行:让人知道旧的没丢、去哪儿翻,而不是凭空消失。
    let archived_note = |elements: &mut Vec<serde_json::Value>| {
        if archived > 0 {
            elements.push(json!({
                "tag": "markdown",
                "content": summon_i18n::format!("rd637149b56"),
            }));
        }
    };
    if live.is_empty() {
        let mut elements =
            vec![json!({"tag": "markdown", "content": summon_i18n::text!("r9edba71f4a")})];
        archived_note(&mut elements);
        elements.push(timer_reset_btn());
        elements.push(new_btn());
        return closeable_card(
            json!({"schema": "2.0", "body": {"elements": elements}}).to_string(),
        );
    }
    let mut elements: Vec<serde_json::Value> =
        vec![json!({"tag": "markdown", "content": summon_i18n::text!("r97ec245b99")})];
    for s in live {
        elements.push(json!({
            "tag": "markdown",
            "content": format!(
                "- **{}** {} · {} → {}  ·  {}",
                s.id,
                crate::schedule::human_readable(s),
                s.output.label(),
                s.prompt,
                s.agent
            ),
        }));
        {
            // 「改」是纯渲染(拉起预填好的参数卡),不带二次确认;「归档」是破坏性,保留确认弹窗。
            elements.push(button_row(vec![
                cb_btn(
                    summon_i18n::text!("r3441804940"),
                    "default",
                    json!({"action": "schedule_edit", "id": s.id}),
                ),
                cb_btn_confirm(
                    summon_i18n::text!("r5292ab1a36"),
                    "danger",
                    json!({"action": "schedule_cancel", "id": s.id}),
                    &summon_i18n::format!("rb3f333033b", s.id, s.prompt),
                ),
            ]));
        }
    }
    elements.push(json!({"tag": "hr"}));
    archived_note(&mut elements);
    elements.push(timer_reset_btn());
    elements.push(new_btn());
    closeable_card(json!({"schema": "2.0", "body": {"elements": elements}}).to_string())
}

/// 单张 markdown 卡片正文的安全上限(U2)。飞书 interactive 卡片 content 上限约 30KB,
/// 留出卡片 JSON 包装/转义余量取 20KB。超限的终态回复按此分段成多张卡片,流式 patch 超限截断。
pub const CARD_TEXT_SAFE_LIMIT: usize = 20_000;

/// **整张**卡片 JSON 的安全上限(字节)。飞书 interactive 卡片 content 上限约 30KB,
/// 留出 API 包装余量取 25KB。`card_stream_interleaved` 渲染多段交织内容时,总 JSON 超限
/// 会从最早段开始丢弃直到回到预算内(保最新、丢最早)。
pub const CARD_TOTAL_SAFE_LIMIT: usize = 25_000;

/// 一段终态回复经过外部文本转义、mention 展开后，是否同时满足 markdown 与实际卡片 JSON
/// 的安全预算。终态各段都以 schema 2.0 卡投递(首段 PATCH、后续段新发)。
pub fn rendered_final_reply_fits(markdown: &RenderedLarkMd) -> bool {
    markdown.as_str().len() <= CARD_TEXT_SAFE_LIMIT
        && card_markdown_v2_rendered(markdown).len() <= CARD_TOTAL_SAFE_LIMIT
}

/// 按调用方提供的**最终渲染预算**切原始文本。与 [`split_for_card`] 的区别是：切点不能只看
/// 原始字节数，因为 `&` / `<` 转义和 `<at>` 展开会显著放大实际飞书 payload。
///
/// 性质：
/// - UTF-8 边界安全，`segments.concat() == text`；
/// - 空文本仍返回一个空段；
/// - 优先在靠近预算末尾的空白后切，避免把 `@name` 拆开；无合适空白时才按字符硬切；
/// - `fits` 应保证单个 Unicode scalar 可容纳。飞书卡预算远大于单字符，因此生产调用满足此约束。
pub fn split_for_card_by_budget(text: &str, fits: impl Fn(&str) -> bool) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    if fits(text) {
        return vec![text.to_string()];
    }

    let mut segments = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if fits(rest) {
            segments.push(rest.to_string());
            break;
        }

        let boundaries: Vec<usize> = rest
            .char_indices()
            .map(|(index, ch)| index + ch.len_utf8())
            .collect();
        // 渲染大小对普通转义/mention 展开是单调的；二分找可容纳的最长前缀。
        let mut low = 0usize;
        let mut high = boundaries.len();
        while low < high {
            let mid = low + (high - low) / 2;
            if fits(&rest[..boundaries[mid]]) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        // 防御异常 renderer：即使它不满足单调性，也回退向前寻找一个真实可容纳的切点。
        let mut boundary_index = low.saturating_sub(1);
        while boundary_index > 0 && !fits(&rest[..boundaries[boundary_index]]) {
            boundary_index -= 1;
        }
        let mut end = boundaries[boundary_index];
        if !fits(&rest[..end]) {
            // 单字符正常必然可容纳；这里仍保证游标前进，最终发送入口还会再次做预算校验。
            end = boundaries[0];
        }

        // 优先在后半区的空白后切。保留空白本身，才能维持逐字节拼接不变量。
        let preferred = rest[..end].char_indices().rev().find_map(|(index, ch)| {
            let after = index + ch.len_utf8();
            (ch.is_whitespace() && after >= end / 2).then_some(after)
        });
        if let Some(preferred) = preferred {
            if fits(&rest[..preferred]) {
                end = preferred;
            }
        }

        segments.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    segments
}

/// 把超长回复按字节上限分段(U2),供「终态回复 → 多张卡片」用。性质:
/// - 各段 ≤ `max` 字节、UTF-8 边界安全;`segs.concat() == text`(不丢不改一字)。
/// - 优先按行边界切(`split_inclusive('\n')`,分隔符保留在段内);单行超限再按 char 硬切。
/// - 空文本返回单个空段(调用方语义不变:仍发一张卡)。
pub fn split_for_card(text: &str, max: usize) -> Vec<String> {
    if text.len() <= max {
        return vec![text.to_string()];
    }
    let mut segs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let push_piece = |segs: &mut Vec<String>, cur: &mut String, piece: &str| {
        if cur.len() + piece.len() > max && !cur.is_empty() {
            segs.push(std::mem::take(cur));
        }
        if piece.len() <= max {
            cur.push_str(piece);
            return;
        }
        // 单行超限:按 char 边界硬切(切点回退到 ≤max 的最近边界,绝不越界)。
        let mut rest = piece;
        while rest.len() > max {
            let mut end = max;
            while end > 0 && !rest.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                break; // 防御:max 小于单 char 宽度才会走到,防死循环
            }
            segs.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        cur.push_str(rest);
    };
    for line in text.split_inclusive('\n') {
        push_piece(&mut segs, &mut cur, line);
    }
    if !cur.is_empty() {
        segs.push(cur);
    }
    segs
}

/// 流式 patch 的超限保护(U2):累积全文超过 `max` 时,截到 char 边界并加「后续另卡」提示,
/// 让打字机卡片在上限附近优雅停住而不是 PATCH 整体被 API 拒绝;尾部内容由终态分段补卡送达。
/// 未超限返回 None(调用方原样发)。
pub fn clip_for_stream(text: &str, max: usize) -> Option<String> {
    if text.len() <= max {
        return None;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(summon_i18n::format!("r7e7c1e3d66", &text[..end]))
}

/// 判断字符是否为「ASCII 单词字符」(字母/数字/下划线),用于区分 @mention 与邮箱(user@example.com)。
/// 必须用 `is_ascii_alphanumeric` 而非 `is_alphanumeric`:后者对 CJK 字符返回 true,
/// 导致 `请@星光`(中文前置无空格)被误判为邮箱、跳过不替换。
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 净化用于插入 `<at>` 标签内的文本:转义可能破坏标签结构的字符。
/// 防止恶意 bot 名(如 `evil</at><at user_id="ou_victim">x`)造成注入。
fn escape_at_content(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `<at user_id="…">` 属性值的防御性转义。飞书 open_id 正常只含安全字符，但这里的数据最终
/// 来自事件/落盘缓存，不能把平台格式约定当作 HTML 注入边界。
fn escape_at_attribute(s: &str) -> String {
    escape_at_content(s).replace('\'', "&#39;")
}

/// 尾边界判据:候选(name/alias)在 `rest` 开头命中后,若候选**末字符**与 `rest` 中紧随其后的
/// **后继字符**同为 ASCII 单词字符([A-Za-z0-9_]),说明文本里那个词其实比候选更长——名单里
/// 只有「Flare 5」时的 `@Flare 5x`/`@Flare 50`/`@Flare 5_x` 是另一个(不在名单的)名字,消费
/// 候选会把它从中间劈成 `<at>Flare 5</at>x` —— 拒绝该候选。后继是 CJK/标点/空白/句尾时不受限
/// (`@Flare 5请看` 仍命中),候选以 CJK 收尾时同样不受限(`@星光abc` 的歧义由入站侧约定兜底,
/// 行为与旧实现一致)。
///
/// 调用前提:`rest.starts_with(candidate)` 已成立,故 `candidate.len()` 落在字符边界上。
fn candidate_fits_tail_boundary(candidate: &str, rest: &str) -> bool {
    let last_is_word = candidate.chars().next_back().is_some_and(is_word_char);
    let next_is_word = rest[candidate.len()..]
        .chars()
        .next()
        .is_some_and(is_word_char);
    !(last_is_word && next_is_word)
}

/// 在 `rest`(@ 后面的全部剩余文本)开头做**名单内最长匹配**:候选键 = peer name + alias,
/// 谁在 `rest` 开头完整出现且最长,就命中谁。返回 `(命中的 peer, 消费的字节数)`。
///
/// 为什么不按空白分词再匹配:peer 名字可以含空格(如「Flare 5」),按词切会把名字腰斩成
/// 「Flare」,永远查不到。名单是有限已知集合,直接拿集合往文本开头比,既支持带空格的名字,
/// 也天然允许名字后紧跟标点/中文/换行(不要求名字后必须是空白);不在集合内的 `@xxx` 永远
/// 不会被误消费。
///
/// 尾边界:候选还须通过 [`candidate_fits_tail_boundary`] —— 候选末字符与后继字符同为 ASCII
/// 单词字符时拒绝,防止名单只有短名字时把更长的(名单外)名字劈开消费。
///
/// 歧义规则:互为前缀的名字取最长(「Flare 5」和「Flare 5x」同在名单时,`@Flare 5x` 归后者);
/// 同长度时真名优先于 alias(先遍历 peers,后续仅在**严格更长**时才替换)。
fn find_peer_longest_match(
    peers: &[crate::peers::Peer],
    alias_map: &std::collections::HashMap<String, String>,
    rest: &str,
) -> Option<(crate::peers::Peer, usize)> {
    let mut best: Option<(crate::peers::Peer, usize)> = None;
    // 空键要挡掉:空串 starts_with 恒真,会造成消费 0 字节的假命中。
    // ① 真名。
    for p in peers {
        if !p.name.is_empty()
            && rest.starts_with(p.name.as_str())
            && candidate_fits_tail_boundary(p.name.as_str(), rest)
            && best.as_ref().is_none_or(|(_, l)| p.name.len() > *l)
        {
            best = Some((p.clone(), p.name.len()));
        }
    }
    // ② alias → name:消费的是 alias 在文本里的长度,显示仍用真名。
    //    HashMap 遍历序无关紧要:等长的两个不同键不可能同时是 rest 的前缀。
    for (alias, name) in alias_map {
        if !alias.is_empty()
            && rest.starts_with(alias.as_str())
            && candidate_fits_tail_boundary(alias.as_str(), rest)
            && best.as_ref().is_none_or(|(_, l)| alias.len() > *l)
        {
            if let Some(p) = crate::peers::match_peer_by_name(peers, name) {
                best = Some((p.clone(), alias.len()));
            }
        }
    }
    best
}

/// 把 agent 回复里的 @<name> 或 @<alias> 替换成飞书 `<at user_id="ou_xxx">名字</at>` 标签。
///
/// 匹配规则:
/// - `@` 前面不能是 ASCII 单词字符(避免匹配 `user@example.com` 邮箱)。
/// - 对 `@` 后面的剩余文本做名单内最长匹配(见 [`find_peer_longest_match`]):**不按空白
///   分词**——名字可以含空格(如「Flare 5」),按词切会把这类名字腰斩、永远查不到。
/// - alias 命中时按 alias_map 转成 name 再替换,显示用真名。
/// - 都没命中:保留原样(可能是 @真人,由飞书客户端处理)。
/// - 名字/alias 之后的文本(标点、CJK 后缀如 `@星光你好` 的「你好」)原样保留在标签后。
pub fn render_outbound_mentions(
    text: &str,
    peers: &[crate::peers::Peer],
    alias_map: &std::collections::HashMap<String, String>,
) -> String {
    let mut result = String::with_capacity(text.len());
    // 邮箱判据用:原文中当前位置的前一个字符(mention 被消费时取被消费文本的末字符)。
    let mut prev: Option<char> = None;
    let mut i = 0; // 字节游标,循环保证始终落在字符边界上

    while let Some(c) = text[i..].chars().next() {
        // 前面是单词字符 → 可能是邮箱(user@x.com),跳过不替换。
        if c == '@' && !prev.is_some_and(is_word_char) {
            // '@' 是单字节,i+1 仍是字符边界。
            if let Some((peer, consumed)) =
                find_peer_longest_match(peers, alias_map, &text[i + 1..])
            {
                // name/open_id 净化:防注入(如 name 含 </at> 破坏标签)。
                result.push_str(&format!(
                    "<at user_id=\"{}\">{}</at>",
                    escape_at_attribute(&peer.open_id),
                    escape_at_content(&peer.name)
                ));
                prev = text[i + 1..i + 1 + consumed].chars().next_back();
                i += 1 + consumed;
                continue;
            }
        }

        result.push(c);
        prev = Some(c);
        i += c.len_utf8();
    }

    result
}

/// 群成员列表里的一个成员。`is_bot` 的判据是**成员项带非空 `app_id`** —— 飞书用它区分应用与人,
/// 这是同一份 payload 里唯一可靠的判据(名字、id 前缀都不可靠)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMember {
    pub open_id: String,
    pub name: String,
    pub is_bot: bool,
}

/// 解析 `im/v1/chats/<id>/members` 的响应体。空 open_id / 空 name 的项直接丢。
///
/// 结构不符只返回空,绝不 panic —— 调用方在网络路径上,坏 body 不该掀掉整条入站。
pub fn parse_chat_members(body: &serde_json::Value) -> Vec<ChatMember> {
    let Some(items) = body["data"]["items"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let open_id = item["member_id"].as_str().unwrap_or_default();
            let name = item["name"].as_str().unwrap_or_default();
            if open_id.is_empty() || name.is_empty() {
                return None;
            }
            Some(ChatMember {
                open_id: open_id.to_string(),
                name: name.to_string(),
                is_bot: !item["app_id"].as_str().unwrap_or_default().is_empty(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {

    // 业务控件断言与关闭入口分别验收，保留原有布局和回调约束。
    pub(super) fn workflow_card(card_json: &str) -> Result<serde_json::Value, serde_json::Error> {
        let mut card: serde_json::Value = serde_json::from_str(card_json)?;
        let elements = if card["schema"] == "2.0" {
            card.pointer_mut("/body/elements")
        } else {
            card.get_mut("elements")
        };
        if let Some(elements) = elements.and_then(serde_json::Value::as_array_mut) {
            elements.retain(|element| {
                let callbacks = callback_values(element);
                !callbacks
                    .iter()
                    .any(|value| value["action"] == "card_close")
            });
        }
        Ok(card)
    }

    fn close_action_count(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(map) => {
                usize::from(
                    map.get("action")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|action| matches!(action, "card_close" | "session_close")),
                ) + map.values().map(close_action_count).sum::<usize>()
            }
            serde_json::Value::Array(items) => items.iter().map(close_action_count).sum(),
            _ => 0,
        }
    }

    fn has_close_action(value: &serde_json::Value) -> bool {
        close_action_count(value) > 0
    }

    fn feed_preview_of(card_json: &str) -> String {
        let card: serde_json::Value = serde_json::from_str(card_json).unwrap();
        card["config"]["summary"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn content_cards_preserve_their_payload_without_close_actions() {
        use crate::image::ReplyPiece;
        use crate::parse::claude_transcript::{CapsuleMessage, CapsuleRole, ContextCapsule};
        let text = "正文保留\n第二行中的 card_close 是文字";
        let rendered = render_external_lark_md(text, &[], &std::collections::HashMap::new());
        let capsule = ContextCapsule {
            messages: vec![CapsuleMessage {
                role: CapsuleRole::Assistant,
                text: text.into(),
            }],
            ..Default::default()
        };
        let summary = card_turn_summary(text, Some("上下文"), Some("标记"));
        let cards = [
            card_markdown(text),
            card_markdown_v2(text),
            card_markdown_v2_rendered(&rendered),
            summary.clone(),
            card_turn_summary_pieces(
                &[
                    ReplyPiece::Text(text.into()),
                    ReplyPiece::Image {
                        key: "img_1".into(),
                        alt: "图片".into(),
                    },
                ],
                None,
                None,
            ),
            card_stream_with_interrupt_rendered(&rendered),
            card_stream_stalled(text),
            card_observation_interrupted_with_body(
                &[crate::segment::TurnSegment::Reply(text.into())],
                "12:00",
            ),
            card_external_bridge_turn("用户问题", text),
            card_bridge_context_capsule(&capsule),
            card_with_subagent_tail(&summary, &["子任务运行中".into()]).unwrap(),
            card_with_footer_note(&summary, "已退出", Some(FeedStatus::Exited)).unwrap(),
            card_with_exit_archive(&summary, "已退出", "token", false).unwrap(),
            card_with_source_note(&summary, "来源"),
            card_markdown_with_note(text, "说明"),
            card_program_status_hint("运行状态", text, false),
            card_interactive_raw(text, 20, false),
        ];
        for wire in cards {
            let card: serde_json::Value = serde_json::from_str(&wire).unwrap();
            assert!(!has_close_action(&card), "{wire}");
            assert!(wire.contains("正文保留"), "{wire}");
            assert!(wire.contains("第二行"), "{wire}");
        }
        let image_card = card_turn_summary_pieces(
            &[ReplyPiece::Image {
                key: "img_1".into(),
                alt: "图片".into(),
            }],
            None,
            None,
        );
        assert!(image_card.contains("img_1"));
        assert!(!has_close_action(
            &serde_json::from_str(&image_card).unwrap()
        ));
    }

    #[test]
    fn operation_and_reminder_cards_explicitly_offer_close() {
        let pre = crate::schedule::form::Prefill::default();
        let params = ScheduleParamsCard {
            freq: crate::schedule::form::CardFreq::Once,
            pre: &pre,
            edit_id: None,
            current: None,
            error: None,
        };
        let limit = crate::claude_session_limit::ClaudeSessionLimit {
            reset_at: chrono::DateTime::parse_from_rfc3339("2026-10-17T15:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            timezone: "Asia/Singapore".into(),
            reset_time: "11:00pm".into(),
        };
        for wire in [
            card_idle_prompt("chat", 3300),
            card_idle_closed("chat", 3300, IdleClosedReason::KeepAlive),
            card_context_warning(
                &ctx_card(crate::context_budget::ContextRemedy::Compact),
                Some("sid"),
            ),
            card_schedule_start("任务", None),
            card_schedule_params(&params),
            card_schedule_settled(&params, "已创建"),
            card_schedules_panel(&[], false),
            card_sentinel_auth_gap(&["scope".into()], "缺少授权"),
            card_sentinel_login_missing(None),
            card_observation_interrupted("12:00"),
            card_terminal(&TerminalCard {
                agent: "agent",
                chat_label: "群聊",
                read_only_url: "http://127.0.0.1/t/read",
                operate_url: "http://127.0.0.1/t/operate",
                target_chat_id: "oc_group",
                target_chat_type: "group",
                recipient_open_id: "ou_owner",
            }),
            card_notice_with_terminal("请到终端处理"),
            card_crash_recovery("batch", "chat", None, &[], &[]),
            card_chat_ask(&ChatAskCard {
                gate_id: "gate",
                requester_open_id: "ou_owner",
                questions: &[],
            }),
            card_interactive_select("选择", &[(1, "同意".into())]),
            card_sentinel_confirm("proposal", "请求"),
            card_sentinel_confirm_done("请求", "完成"),
            card_bridge_mirror_control("ticket", "choice", "目标"),
            card_bridge_activation_audit("目标"),
            card_codex_session_limit(1_792_248_000),
            card_codex_usage_limit_exceeded(false),
            card_claude_session_limit(&limit),
            card_claude_session_limit_auto_resume(&limit),
        ] {
            let card: serde_json::Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(close_action_count(&card), 1, "{wire}");
            assert!(
                card_with_close_button(&wire).is_none(),
                "关闭入口应只有一个:{wire}"
            );
        }
    }

    #[test]
    fn close_button_covers_both_schemas_and_is_idempotent() {
        for card in [
            card_markdown("原文"),
            serde_json::json!({
                "schema": "2.0", "body": {"elements": [{"tag": "markdown", "content": "原文"}]}
            })
            .to_string(),
        ] {
            let closed = card_with_close_button(&card).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&closed).unwrap();
            if parsed["schema"] == "2.0" {
                assert_eq!(
                    callback_values(&parsed),
                    vec![&serde_json::json!({"action": "card_close"})]
                );
            } else {
                assert_eq!(
                    parsed["elements"][1]["actions"][0]["value"]["action"],
                    "card_close"
                );
            }
            assert!(closed.contains("原文"));
            assert!(card_with_close_button(&closed).is_none());
        }
        assert!(card_with_close_button("not json").is_none());
        assert!(card_with_close_button("{}").is_none());
        assert_eq!(
            CardAction::from_action("card_close"),
            CardAction::SessionClose
        );
        assert_eq!(
            CardAction::from_action("card_close").gate(),
            CardGate::Operate
        );
        assert!(matches!(
            CardAction::from_action("card_close").operate_scope("oc_1"),
            crate::registry::OperateScope::Chat("oc_1")
        ));
    }

    #[test]
    fn close_button_keeps_enabled_session_entry_and_replaces_disabled_entry() {
        let card = serde_json::json!({"schema": "2.0", "body": {"elements": [
            button_row(vec![cb_btn("关闭卡片", "default", serde_json::json!({"action":"session_close"}))])
        ]}});
        assert!(card_with_close_button(&card.to_string()).is_none());
        let mut disabled = card;
        disabled["body"]["elements"][0]["columns"][0]["elements"][0]["disabled"] = true.into();
        assert!(card_with_close_button(&disabled.to_string())
            .unwrap()
            .contains("card_close"));
    }

    #[test]
    fn feed_preview_drops_markdown_marks_but_keeps_the_words() {
        let preview = feed_summary(
            "**通知** · 跨会话 1 条\n- `cargo test` 跑完\n> 引用\n## 标题\n\
             [文档](https://x.y) ~~旧~~ <font color='grey'>灰字</font>\n---\n1. 第一\n\
             <at user_id=\"ou_1\">小明</at> 看下",
        )
        .unwrap();
        assert_eq!(
            preview,
            "通知 · 跨会话 1 条 cargo test 跑完 引用 标题 文档 旧 灰字 第一 小明 看下"
        );
    }

    #[test]
    fn feed_preview_leaves_ordinary_symbols_in_the_text() {
        let preview = feed_summary(
            "Vec<String> 里 a*b 与 snake_case、foo() 都在;crates/**/*.rs 与 2**10、`未闭合",
        )
        .unwrap();
        assert_eq!(
            feed_summary("这是**重点**内容,`cargo`跑完").unwrap(),
            "这是重点内容,cargo跑完"
        );
        for kept in ["a**b**", "2 ** 10 与 3 ** 4", "def f(**kwargs)"] {
            assert_eq!(feed_summary(kept).unwrap(), kept, "{kept}");
        }
        // 同一行两个不带反引号的 glob 在卡里会被渲染成加粗,预览与卡面保持一致。
        assert_eq!(
            feed_summary("crates/**/*.rs 与 tests/**/*.rs").unwrap(),
            "crates//*.rs 与 tests//*.rs"
        );
        for kept in [
            "String",
            "a*b",
            "snake_case",
            "foo()",
            "crates/**/*.rs",
            "2**10",
            "`未闭合",
        ] {
            assert!(preview.contains(kept), "{kept} 被当成标记删掉了:{preview}");
        }
    }

    #[test]
    fn cards_without_a_preview_get_one_from_their_title_and_first_paragraph() {
        let idle = card_with_derived_feed_summary(&card_idle_prompt("g-x", 3300)).unwrap();
        let preview = feed_preview_of(&idle);
        assert!(
            preview.starts_with("会话空闲提醒 这个会话已空闲"),
            "{preview}"
        );
        assert!(!preview.contains('*'), "{preview}");

        let titled = serde_json::json!({
            "schema": "2.0",
            "header": {"title": {"tag": "plain_text", "content": "后台任务"}},
            "body": {"elements": [
                {"tag": "markdown", "content": "角标", "text_size": "notation"},
                {"tag": "markdown", "content": "**3 个**已完成"},
            ]},
        });
        let titled = card_with_derived_feed_summary(&titled.to_string()).unwrap();
        assert_eq!(feed_preview_of(&titled), "后台任务 3 个已完成");
    }

    #[test]
    fn derived_preview_leaves_cards_that_already_have_one_or_cannot_carry_one() {
        assert!(card_with_derived_feed_summary(&card_markdown_v2("正文")).is_none());
        assert!(
            card_with_derived_feed_summary(&card_markdown("正文")).is_none(),
            "旧版骨架不认 config.summary"
        );
        assert!(card_with_derived_feed_summary("不是 JSON").is_none());
        let near_limit = serde_json::json!({
            "schema": "2.0",
            "body": {"elements": [
                {"tag": "markdown", "content": "汉".repeat(CARD_TOTAL_SAFE_LIMIT / 3 - 20)},
            ]},
        });
        assert!(
            card_with_derived_feed_summary(&near_limit.to_string()).is_none(),
            "补上预览会超出整卡预算时宁可不补"
        );
    }

    #[test]
    fn interacted_cards_lead_their_preview_with_the_outcome() {
        for (reason, label) in [
            (IdleClosedReason::Reset, "已归档"),
            (IdleClosedReason::Clean, "已退出"),
            (IdleClosedReason::Compact, "已压缩"),
            (IdleClosedReason::KeepAlive, "已保活"),
        ] {
            let preview = feed_preview_of(&card_idle_closed("g-x", 3300, reason));
            assert!(
                preview.starts_with(&format!("{label}|会话空闲提醒")),
                "{preview}"
            );
        }

        let picked = card_interactive_select_result(
            "选模型",
            &[(1, "甲".to_string()), (2, "乙".to_string())],
            1,
        );
        assert!(
            feed_preview_of(&picked).starts_with("已选第 2 项|⏳ 需要你确认"),
            "{}",
            feed_preview_of(&picked)
        );

        let questions = [crate::core_port::AskQuestion {
            question: "用哪个方案?".into(),
            header: None,
            options: vec!["A".into(), "B".into()],
            descriptions: Vec::new(),
            ..Default::default()
        }];
        let ask = ChatAskCard {
            gate_id: "g1",
            requester_open_id: "ou_1",
            questions: &questions,
        };
        for (card, label) in [
            (
                card_chat_ask_frozen(&ask, &[Some("A".into())], false),
                "已提交",
            ),
            (card_chat_ask_frozen(&ask, &[None], true), "已跳过"),
            (
                card_chat_ask_invalidated(&ask, AskCardEnd::TimedOut),
                "已超时",
            ),
            (
                card_chat_ask_invalidated(&ask, AskCardEnd::TurnEnded),
                "已结束",
            ),
            (
                card_chat_ask_invalidated(&ask, AskCardEnd::Elsewhere),
                "已结束",
            ),
        ] {
            let preview = feed_preview_of(&card);
            assert!(
                preview.starts_with(&format!("{label}|❓ 需要你决定")),
                "{preview}"
            );
            assert!(!preview.contains("（）"), "{preview}");
        }

        let pre = crate::schedule::form::Prefill {
            prompt: "写周报".into(),
            time: "10:30".into(),
            ..Default::default()
        };
        let settled = card_schedule_settled(
            &ScheduleParamsCard {
                freq: crate::schedule::form::CardFreq::Daily,
                pre: &pre,
                edit_id: None,
                current: None,
                error: None,
            },
            "**已建定时器 SCH-9**:每天 10:30",
        );
        assert!(
            feed_preview_of(&settled).starts_with("已建定时器 SCH-9:每天 10:30|"),
            "{}",
            feed_preview_of(&settled)
        );
    }

    #[test]
    fn exit_archive_row_keeps_the_reply_and_places_its_button_on_the_right() {
        for base in [
            card_markdown("正文"),
            card_turn_summary("正文", Some("角标"), None),
        ] {
            let original: serde_json::Value = serde_json::from_str(&base).unwrap();
            let card = card_with_exit_archive(&base, "会话已退出", "cap-token", false).unwrap();
            let v: serde_json::Value = serde_json::from_str(&card).unwrap();
            let elements = v
                .get("body")
                .map(|b| &b["elements"])
                .unwrap_or(&v["elements"])
                .as_array()
                .unwrap();
            let originals = original
                .get("body")
                .map(|b| &b["elements"])
                .unwrap_or(&original["elements"])
                .as_array()
                .unwrap();
            assert_eq!(&elements[..originals.len()], originals.as_slice());
            let row = elements.last().unwrap();
            assert_eq!(row["columns"][0]["width"], "weighted");
            assert!(row["columns"][0]["elements"][0]["content"]
                .as_str()
                .unwrap()
                .contains("会话已退出"));
            let button = &row["columns"][1]["elements"][0];
            assert_eq!(button["text"]["content"], "📦 归档");
            assert_eq!(button["behaviors"][0]["value"]["token"], "cap-token");
            assert_eq!(
                CardAction::from_action(
                    button["behaviors"][0]["value"]["action"].as_str().unwrap()
                ),
                CardAction::ArchiveEnded
            );
            let done = card_with_exit_archive(&card, "会话已归档", "cap-token", true).unwrap();
            let done: serde_json::Value = serde_json::from_str(&done).unwrap();
            let els = done
                .get("body")
                .map(|b| &b["elements"])
                .unwrap_or(&done["elements"])
                .as_array()
                .unwrap();
            assert_eq!(els.len(), originals.len() + 2);
            let button = &els.last().unwrap()["columns"][1]["elements"][0];
            assert_eq!(button["disabled"], true);
            assert_eq!(button["behaviors"].as_array().unwrap().len(), 0);
        }
        assert!(card_with_exit_archive("{}", "退出", "cap", false).is_none());
    }

    fn feed_of(card: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(card).unwrap();
        value["config"]["summary"]["content"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn lifecycle_updates_put_the_current_state_first_in_feed_preview() {
        let legacy = card_markdown("正文结论");
        let modern = card_turn_summary("正文结论", Some("角标"), None);
        for base in [legacy, modern] {
            let exited = card_with_exit_archive(&base, "会话已退出", "token", false).unwrap();
            let archived = card_with_exit_archive(&exited, "会话已归档", "token", true).unwrap();
            let repeated = card_with_exit_archive(&archived, "会话已归档", "token", true).unwrap();
            assert_eq!(feed_of(&exited), "已退出|正文结论");
            assert_eq!(feed_of(&archived), "已归档|正文结论");
            assert_eq!(feed_of(&repeated), "已归档|正文结论");

            let footer =
                card_with_footer_note(&base, "会话已退出", Some(FeedStatus::Exited)).unwrap();
            let footer =
                card_with_footer_note(&footer, "会话已归档", Some(FeedStatus::Archived)).unwrap();
            assert_eq!(feed_of(&footer), "已归档|正文结论");

            // 会话还活着时只追加说明,预览不带状态。
            let note_only = card_with_footer_note(&base, "交接文件已写好", None).unwrap();
            assert!(note_only.contains("交接文件已写好"));
            let summary = |card: &str| {
                serde_json::from_str::<serde_json::Value>(card).unwrap()["config"]["summary"]
                    .clone()
            };
            assert_eq!(summary(&note_only), summary(&base));
        }
        let long = card_turn_summary(&"正文".repeat(1000), None, None);
        let updated = card_with_footer_note(&long, "会话已退出", Some(FeedStatus::Exited)).unwrap();
        let summary = feed_of(&updated);
        assert!(summary.starts_with("已退出|正文"), "{summary}");
        assert!(summary.chars().count() <= FEED_SUMMARY_MAX_CHARS);
    }

    /// agent 给的总结是预览内容;退出、归档只在前面换状态,不退回正文摘要。
    #[test]
    fn agent_feed_summary_survives_exit_and_archive_updates() {
        let card = card_with_feed_summary(
            &card_turn_summary("很长的一段正文结论", Some("角标"), None),
            "修好额度显示",
        );
        assert_eq!(feed_of(&card), "修好额度显示");
        let exited = card_with_exit_archive(&card, "会话已退出", "token", false).unwrap();
        assert_eq!(feed_of(&exited), "已退出|修好额度显示");
        let archived = card_with_exit_archive(&exited, "会话已归档", "token", true).unwrap();
        assert_eq!(feed_of(&archived), "已归档|修好额度显示");
        assert_eq!(card_with_feed_summary("not json", "x"), "not json");
    }

    #[test]
    fn rebuilt_orphan_card_drops_the_feed_tag() {
        let card = card_observation_interrupted_with_body(
            &[
                crate::segment::TurnSegment::Thinking("思考".into()),
                crate::segment::TurnSegment::Reply(
                    "已生成的结论\n<summon-feed>总结</summon-feed>".into(),
                ),
            ],
            "12:00",
        );
        assert!(card.contains("已生成的结论"), "{card}");
        assert!(!card.contains("summon-feed"), "{card}");
    }

    #[test]
    fn archiving_a_card_exited_before_the_upgrade_replaces_the_old_status() {
        let mut card: serde_json::Value =
            serde_json::from_str(&card_turn_summary("正文结论", Some("角标"), None)).unwrap();
        card["config"]["summary"]["content"] =
            serde_json::json!("会话已退出,发消息即可接着聊 · 正文结论");
        let archived =
            card_with_exit_archive(&card.to_string(), "会话已归档", "token", true).unwrap();
        assert_eq!(feed_of(&archived), "已归档|正文结论");
    }

    #[test]
    fn footer_note_lands_after_a_divider_on_both_card_schemas() {
        let legacy = card_markdown_rendered(&render_external_lark_md(
            "结论",
            &[],
            &std::collections::HashMap::new(),
        ));
        let modern = card_turn_summary("结论", Some("21:30"), None);
        for card in [legacy, modern] {
            let noted = card_with_footer_note(&card, "会话已归档", Some(FeedStatus::Archived))
                .expect("认得出的卡");
            let value: serde_json::Value = serde_json::from_str(&noted).unwrap();
            let elements = value
                .pointer("/body/elements")
                .or_else(|| value.get("elements"))
                .and_then(serde_json::Value::as_array)
                .unwrap();
            let n = elements.len();
            assert_eq!(elements[n - 2]["tag"], "hr");
            assert_eq!(
                elements[n - 1]["content"],
                "<font color='grey'>会话已归档</font>"
            );
            assert!(noted.contains("结论"), "原内容保留");
        }
        assert_eq!(
            card_with_footer_note("not json", "x", Some(FeedStatus::Exited)),
            None
        );
        assert_eq!(
            card_with_footer_note("{}", "x", Some(FeedStatus::Exited)),
            None
        );
    }
    use crate::observe::context_budget::ContextBudget;

    /// P1:live 卡尾部渲 subagent 行(title + model),**只进 live 卡**——
    /// 挂尾部必须**幂等**:反复刷新只替换、不堆积。没有这条,「每 N 秒刷一次」会在几分钟内
    /// 把卡片撑成一长串重复的子代理列表。
    #[test]
    fn attaching_the_subagent_tail_replaces_instead_of_appending() {
        let base = card_turn_summary("回复正文", None, None);
        let once = card_with_subagent_tail(&base, &["🔄 甲".to_string()]).expect("首次挂载");
        let twice = card_with_subagent_tail(&once, &["🔄 甲".to_string()]).expect("再挂一次");
        assert_eq!(once, twice, "同样的行反复挂,卡应逐字节相同");
        assert_eq!(once.matches("**子代理**").count(), 1);

        let changed = card_with_subagent_tail(&once, &["✅ 甲".to_string()]).expect("状态变了");
        assert_eq!(changed.matches("**子代理**").count(), 1, "换内容也只有一段");
        assert!(changed.contains("✅ 甲"));
        assert!(!changed.contains("🔄 甲"), "旧状态必须被换掉");
    }

    /// 空行数组 = 只摘不挂:子代理全部结束后,卡片自动回到干净的收场形态。
    #[test]
    fn attaching_an_empty_tail_strips_the_block() {
        let base = card_turn_summary("回复正文", None, None);
        let with_tail = card_with_subagent_tail(&base, &["🔄 甲".to_string()]).unwrap();
        let stripped = card_with_subagent_tail(&with_tail, &[]).unwrap();
        assert!(!stripped.contains("**子代理**"));
        assert!(stripped.contains("回复正文"), "正文不能被顺手抹掉");
    }

    /// 正文里的外部文本必须仍然转义;坏 JSON 一律 None(绝不 PATCH 一张坏卡上去)。
    #[test]
    fn tail_escapes_external_text_and_refuses_broken_cards() {
        let base = card_turn_summary("正文", None, None);
        let card = card_with_subagent_tail(&base, &["🔄 <at id=all> 注入".to_string()]).unwrap();
        assert!(
            !card.contains("<at id=all>"),
            "子代理标题里的尖括号必须转义"
        );
        assert_eq!(card_with_subagent_tail("{ 坏 json", &[]), None);
        assert_eq!(card_with_subagent_tail("{\"schema\":\"2.0\"}", &[]), None);
    }

    /// P2:面板 workflow 成组——组头带名字/阶段总数/配对计数,成员缩进其下并带 model 短名。
    #[test]
    fn session_panel_groups_workflow_children_under_header() {
        use crate::model_effort::{EffortSource, ModelSource, ResolvedEffort, ResolvedModel};
        let rm = ResolvedModel {
            value: Some("m".into()),
            source: ModelSource::AgentConfig,
        };
        let re = ResolvedEffort {
            value: None,
            source: EffortSource::CliDefault,
        };
        let now = chrono::Utc::now();
        let display = crate::subagent::SubagentDisplay {
            agents: vec![],
            workflows: vec![crate::subagent::WorkflowGroup {
                run_id: "wf_sp".into(),
                name: "slash-passthrough-impl".into(),
                finished: None,
                phase_total: Some(2),
                journal_started: 3,
                journal_done: 1,
                started_at: now - chrono::Duration::minutes(7),
                children: vec![crate::subagent::SubagentStatus::Running {
                    agent_id: "c1".into(),
                    description: "slash-passthrough-impl #1".into(),
                    agent_type: "workflow-subagent".into(),
                    tool_use_id: String::new(),
                    started_at: now,
                    last_activity: None,
                    model: Some("opus".into()),
                }],
            }],
        };
        let c = card_session_panel(
            "tag-wf",
            "claude",
            "tmux",
            &rm,
            &re,
            &[],
            &[],
            &display,
            false,
        );
        assert!(c.contains("workflow slash-passthrough-impl"), "{c}");
        assert!(c.contains("阶段共 2"), "{c}");
        assert!(c.contains("2 跑 1 完"), "{c}");
        assert!(c.contains("↳"), "成员应缩进标出从属: {c}");
        assert!(c.contains("(opus)"), "成员应带 model 短名: {c}");
    }

    #[test]
    fn session_panel_shows_context_tokens_and_percentage() {
        let controls = SessionPanelControls {
            context: crate::context_budget::ContextBudget::from_tokens(238_000, Some(1_000_000)),
            ..Default::default()
        };
        let card = card_session_panel(
            "tag-context",
            "codex",
            "RPC",
            &rm(Some("gpt-5"), ModelSource::SessionLive),
            &reff(Some("high"), EffortSource::ChatOverride),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            controls,
        );
        assert!(card.contains("上下文"), "{card}");
        assert!(card.contains("238k / 1.0M"), "{card}");
        assert!(card.contains("23%"), "{card}");

        let claude = card_session_panel(
            "tag-context-claude",
            "claude",
            "tmux",
            &rm(Some("Opus"), ModelSource::SessionLive),
            &reff(Some("high"), EffortSource::ChatOverride),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                context: crate::context_budget::ContextBudget::from_reported_pct(
                    42,
                    Some(1_000_000),
                ),
                ..Default::default()
            },
        );
        assert!(claude.contains("≈420k / 1.0M"), "{claude}");
        assert!(claude.contains("42%"), "{claude}");
    }

    #[test]
    fn session_panel_groups_model_and_summon_controls_with_cot_refresh_and_close() {
        let card = card_session_panel(
            "tag-layout",
            "claude",
            "tmux",
            &rm(Some("opus"), ModelSource::SessionLive),
            &reff(Some("high"), EffortSource::ChatOverride),
            &["opus".into()],
            &["high".into()],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                cot: SessionCotControls {
                    visible: true,
                    thinking: false,
                    tools: true,
                },
                ..Default::default()
            },
        );
        let model_group = card.find("模型与推理").expect("模型分组");
        let summon_group = card.find("Summon 会话管理").expect("Summon 分组");
        assert!(model_group < summon_group, "{card}");
        for axis in ["visible", "thinking", "tools"] {
            assert!(
                card.contains(&format!(r#""action":"session_cot","axis":"{axis}""#)),
                "CoT 开关遗漏 {axis}:{card}"
            );
        }
        assert!(card.contains(r#""action":"session_refresh""#), "{card}");
        assert!(card.contains(r#""action":"session_close""#), "{card}");
    }

    #[test]
    fn session_panel_feed_lead_is_compact_and_instructions_are_sunk() {
        let card = card_session_panel(
            "tag-feed",
            "codex",
            "RPC · app-server 常驻",
            &rm(Some("gpt-5"), ModelSource::SessionLive),
            &reff(Some("high"), EffortSource::ChatOverride),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls::default(),
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let lead = value["body"]["elements"][0]["content"]
            .as_str()
            .expect("首元素必须是 feed 摘要");
        assert_eq!(
            lead,
            "**Session 控制面板** · `codex` · RPC · app-server 常驻"
        );
        assert!(!lead.contains("仅授权"));
        assert!(!lead.contains("工作目录"));
        let management = card.find("Summon 会话管理").unwrap();
        let instructions = card.find("仅授权用户可操作").unwrap();
        assert!(
            management < instructions,
            "操作说明应下沉到管理控件之后:{card}"
        );
    }

    #[test]
    fn session_card_utility_actions_have_explicit_gates() {
        assert_eq!(
            CardAction::from_action("session_cot"),
            CardAction::SessionCot
        );
        assert_eq!(CardAction::SessionCot.gate(), CardGate::Operate);
        assert_eq!(
            CardAction::from_action("session_refresh"),
            CardAction::SessionRefresh
        );
        assert_eq!(CardAction::SessionRefresh.gate(), CardGate::None);
        assert_eq!(
            CardAction::from_action("session_close"),
            CardAction::SessionClose
        );
        assert_eq!(CardAction::SessionClose.gate(), CardGate::Operate);
    }

    #[test]
    fn session_panel_marks_context_dimensions_that_are_unavailable() {
        let controls = SessionPanelControls {
            context: crate::context_budget::ContextBudget::from_tokens(238_000, None),
            ..Default::default()
        };
        let card = card_session_panel(
            "tag-context-partial",
            "cursor",
            "RPC",
            &rm(None, ModelSource::CliDefault),
            &reff(None, EffortSource::ModelInherent),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            controls,
        );
        assert!(card.contains("238k"), "{card}");
        assert!(card.contains("百分比不可用"), "{card}");

        let unavailable = card_session_panel(
            "tag-context-none",
            "claude",
            "tmux",
            &rm(None, ModelSource::CliDefault),
            &reff(None, EffortSource::CliDefault),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls::default(),
        );
        assert!(unavailable.contains("暂未取得可靠用量"), "{unavailable}");
        assert!(!unavailable.contains("ctx 0%"), "{unavailable}");
    }

    use super::*;
    use crate::model_effort::{
        EffortSource, ModelSource, ResolvedEffort as RE, ResolvedModel as RM,
    };

    /// 面板测试造 `ResolvedModel` 的糖。
    fn rm(value: Option<&str>, source: ModelSource) -> RM {
        RM {
            value: value.map(str::to_string),
            source,
        }
    }
    /// 面板测试造 `ResolvedEffort` 的糖。
    fn reff(value: Option<&str>, source: EffortSource) -> RE {
        RE {
            value: value.map(str::to_string),
            source,
        }
    }

    #[test]
    fn session_exited_card_carries_fields_and_close_button() {
        let wire = card_session_exited(
            crate::exit::ExitReason::ManualKill,
            "arcadia",
            "sample开发群2",
            "07-27 08:40",
            "本会话已被关闭。下次给我发消息会开一段新的对话。",
            None,
        );
        let v: serde_json::Value = serde_json::from_str(&wire).expect("卡片必须是合法 JSON");
        assert_eq!(v["schema"], "2.0");
        // 四个信息字段与文案都要出现在正文里。
        for needle in ["arcadia", "sample开发群2", "07-27 08:40", "本会话已被关闭"] {
            assert!(wire.contains(needle), "卡片应含 {needle}");
        }
        assert_eq!(
            callback_values(&v),
            vec![&serde_json::json!({"action": "card_close"})]
        );
        assert!(
            !wire.contains(r#""tag":"select_static""#),
            "退出卡不应有下拉"
        );
    }

    #[test]
    fn session_exited_card_shows_created_at_when_provided() {
        let wire = card_session_exited(
            crate::exit::ExitReason::ManualKill,
            "arcadia",
            "sample开发群2",
            "08-07 16:18",
            "本会话已关闭。下次给我发消息会接着这段上下文续聊。",
            Some("2026-08-07 16:10:00"),
        );
        assert!(wire.contains("起始"), "有起始时间时应展示「起始」行");
        assert!(
            wire.contains("2026-08-07 16:10:00"),
            "起始时间值应原样出现在卡片中"
        );
        // 无起始时间时不展示该行。
        let wire_no = card_session_exited(
            crate::exit::ExitReason::ManualKill,
            "arcadia",
            "群",
            "08-07 16:18",
            "文案",
            None,
        );
        assert!(!wire_no.contains("起始"), "无起始时间时不应展示「起始」行");
    }

    #[test]
    fn session_exited_card_title_varies_by_reason() {
        use crate::exit::ExitReason;
        let title_of = |r| card_session_exited(r, "a", "群", "07-27 08:40", "文案", None);
        // 六种 reason 必须给出各自可区分的标题,不能共用一句。
        assert!(title_of(ExitReason::ManualKill).contains("会话已关闭"));
        assert!(title_of(ExitReason::GroupDisbanded).contains("群已解散"));
        assert!(title_of(ExitReason::ShutdownInterrupt).contains("会话被打断"));
        assert!(title_of(ExitReason::PanelClean).contains("会话已退出"));
        assert!(title_of(ExitReason::PanelReset).contains("会话已归档"));
        assert!(title_of(ExitReason::BotRemoved).contains("已退出群聊"));
    }

    #[test]
    fn escape_lark_md_neutralizes_tag_like_angle_brackets() {
        // 字母开头 = 合法 HTML 标签名 → 飞书会剥掉。这正是 `<alias>` 消失的机制。
        // 消毒改成插零宽空格:标签形态被拆散,视觉上仍是 `<alias>`,不再出现字面 `&lt;`。
        assert_eq!(
            escape_lark_md("~/.summon/agents/<alias>/agent.yaml"),
            "~/.summon/agents/<\u{200B}alias>/agent.yaml"
        );
        assert_eq!(escape_lark_md("Vec<T>"), "Vec<\u{200B}T>");
        // HTML 另外三种标签起始(闭标签/注释/处理指令)同样拆散:字面 `</at>` 若被当闭标签,
        // 会闭掉受信 mention;`<!--` 会吞掉后续全部内容。基于 HTML 解析规则防御,未真机复核。
        assert_eq!(escape_lark_md("</View>"), "<\u{200B}/View>");
        assert_eq!(escape_lark_md("<!-- x -->"), "<\u{200B}!-- x -->");
        assert_eq!(escape_lark_md("<?php"), "<\u{200B}?php");
    }

    fn fleet_waiting_item(i: usize) -> crate::waiting::WaitingStatus {
        crate::waiting::WaitingStatus {
            alias: format!("a{i}"),
            tag: format!("g{i}"),
            name: Some(format!("群{i}")),
            chat_type: Some("group".into()),
            reply_message_id: format!("om_{i}"),
            since_secs: 60,
            since_text: "1m前".into(),
            since_text_en: "1m ago".into(),
            link: format!("lark://applink.feishu.cn/client/chat/open?openChatId=oc_{i}"),
        }
    }

    /// 待处理区在卡顶(操作结果之下、正文之上),每条一个「标记已处理」,超出上限的明示条数。
    #[test]
    fn fleet_card_puts_waiting_rows_above_the_body() {
        use crate::fleet_card::MAX_WAITING_ROWS;
        let waiting: Vec<_> = (0..MAX_WAITING_ROWS + 2).map(fleet_waiting_item).collect();
        let card: serde_json::Value = serde_json::from_str(&card_fleet(
            "正文",
            false,
            None,
            &[],
            &waiting,
            Some("✅ 好了"),
        ))
        .unwrap();
        let els = card["body"]["elements"].as_array().unwrap();
        assert_eq!(els[0]["content"], "✅ 好了");
        assert!(
            els[1]["content"]
                .as_str()
                .unwrap()
                .contains(&format!("待处理({})", MAX_WAITING_ROWS + 2)),
            "{}",
            els[1]
        );
        let rows: Vec<&serde_json::Value> = els
            .iter()
            .filter(|e| e.to_string().contains("fleet_waiting_handled"))
            .collect();
        assert_eq!(rows.len(), MAX_WAITING_ROWS);
        let value = &rows[0]["columns"][1]["elements"][0]["behaviors"][0]["value"];
        assert_eq!(
            value,
            &serde_json::json!({
                "action": "fleet_waiting_handled",
                "agent": "a0",
                "tag": "g0",
                "reply": "om_0",
            })
        );
        assert!(
            rows[0]["columns"][0]["elements"][0]["content"]
                .as_str()
                .unwrap()
                .contains("(https://applink.feishu.cn/"),
            "{}",
            rows[0]
        );
        let body = els.iter().position(|e| e["content"] == "正文").unwrap();
        let last_row = els
            .iter()
            .rposition(|e| e.to_string().contains("fleet_waiting_handled"))
            .unwrap();
        assert!(last_row < body, "待处理区要在正文之上");
        assert!(
            els[last_row + 1]["content"]
                .as_str()
                .unwrap()
                .contains("另有 2 条"),
            "{}",
            els[last_row + 1]
        );
    }

    #[test]
    fn fleet_card_without_waiting_starts_with_the_body() {
        let card: serde_json::Value =
            serde_json::from_str(&card_fleet("正文", false, None, &[], &[], None)).unwrap();
        assert_eq!(card["body"]["elements"][0]["content"], "正文");
        assert!(!card.to_string().contains("fleet_waiting_handled"));
    }

    #[test]
    fn escape_lark_md_leaves_plain_text_untouched() {
        assert_eq!(escape_lark_md("普通文本 a > b"), "普通文本 a > b");
        // 非标签形态的 `<` 一律原样:数字/空白/CJK 开头都不是合法标签名(真机记录:`<档位>` 存活)。
        assert_eq!(escape_lark_md("a < b, x<3"), "a < b, x<3");
        assert_eq!(escape_lark_md("<档位>"), "<档位>");
        assert_eq!(escape_lark_md("行尾悬空 <"), "行尾悬空 <");
    }

    #[test]
    fn escape_lark_md_keeps_ampersand_and_entities_verbatim() {
        // 飞书不解码 HTML 实体(2026-08-12 owner 真机实测 `&lt;id>` 原样示人),所以 `&` 决不能
        // 转成 `&amp;`——那会满屏字面实体。`&` 与 agent 字面写的 `&lt;` 都原样进卡片。
        assert_eq!(escape_lark_md("A & B"), "A & B");
        assert_eq!(escape_lark_md("?a=1&b=2"), "?a=1&b=2");
        assert_eq!(escape_lark_md("&lt;"), "&lt;");
        assert_eq!(escape_lark_md("a && b"), "a && b");
    }

    #[test]
    fn escape_lark_md_is_idempotent() {
        // 插入零宽空格后 `<` 的下一字符不再命中触发集 → 重复消毒不叠加。
        // 重试/多层包装路径可能对同一段文本走两次,这条性质保证不会越走越花。
        for s in ["Vec<T>", "</at>", "a & b", "<档位>", "<at id=all>"] {
            let once = escape_lark_md(s);
            assert_eq!(escape_lark_md(&once), once, "对 {s:?} 不幂等");
        }
    }

    #[test]
    fn card_markdown_escapes_agent_angle_brackets() {
        let c = card_markdown("路径 `<alias>` 会被吃掉");
        assert!(c.contains("<\u{200B}alias>"), "应拆散标签形态: {c}");
        assert!(!c.contains("<alias>"), "裸标签不能进卡片: {c}");
        assert!(!c.contains("&lt;"), "不能出现飞书不解码的字面实体: {c}");
    }

    #[test]
    fn card_markdown_v2_escapes_agent_angle_brackets() {
        let c = card_markdown_v2("泛型 Vec<T>");
        assert!(c.contains("Vec<\u{200B}T>"), "应拆散标签形态: {c}");
        assert!(!c.contains("Vec<T>"), "裸标签不能进卡片: {c}");
    }

    /// **接热路径的前提**:没有图时,新旧两条渲染必须逐字节一致。
    /// 绝大多数回复不带图,它们不该因为这个特性有任何变化。
    #[test]
    fn summary_card_without_images_is_byte_identical() {
        use crate::image::ReplyPiece;
        for (reply, ctx) in [
            ("普通回复", Some("关于:某问题")),
            ("泛型 Vec<T> 与 <font>", None),
            ("", Some("")),
        ] {
            let old = card_turn_summary(reply, ctx, None);
            let new = card_turn_summary_pieces(&[ReplyPiece::Text(reply.to_string())], ctx, None);
            assert_eq!(old, new, "无图渲染必须逐字节相同: {reply:?}");
        }
    }

    #[test]
    fn summary_card_interleaves_images_in_source_order() {
        use crate::image::ReplyPiece;
        let pieces = vec![
            ReplyPiece::Text("分析完了,趋势见下图。\n".into()),
            ReplyPiece::Image {
                key: "img_v3_abc".into(),
                alt: "季度趋势".into(),
            },
            ReplyPiece::Text("\n结论:环比 +12%".into()),
        ];
        let c = card_turn_summary_pieces(&pieces, Some("关于:季度复盘"), None);
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        let els = v["body"]["elements"].as_array().unwrap();
        // 文本、图、文本按原顺序,后面才是 hr + 上下文行。
        assert_eq!(els[0]["tag"], "markdown");
        assert!(els[0]["content"].as_str().unwrap().contains("趋势见下图"));
        assert_eq!(els[1]["tag"], "img");
        assert_eq!(els[1]["img_key"], "img_v3_abc");
        assert_eq!(els[1]["alt"]["content"], "季度趋势");
        assert_eq!(els[2]["tag"], "markdown");
        assert!(els[2]["content"].as_str().unwrap().contains("环比"));
        assert_eq!(els[3]["tag"], "hr");
        // img 元素只带三个字段:v2 对不支持的属性是报错而非忽略。
        let img = els[1].as_object().unwrap();
        assert_eq!(img.len(), 3, "img 只应有 tag/img_key/alt: {img:?}");
    }

    /// 说明本身也要过审核:原因只留类别名用的字符并截短,不把可能回显的命中内容带进群。
    #[test]
    fn content_rejected_notice_keeps_only_the_category() {
        assert_eq!(
            content_rejected_notice("contain sensitive data: EMAIL_ADDRESS"),
            "⚠️ 有一段回复被飞书内容审核拦下,没能发出(contain sensitive data: EMAIL_ADDRESS)。"
        );
        let echoed = content_rejected_notice("hit: someone@example.com 敏感词");
        assert!(
            !echoed.contains('@') && !echoed.contains("敏感词"),
            "{echoed}"
        );
        assert_eq!(
            content_rejected_notice(""),
            "⚠️ 有一段回复被飞书内容审核拦下,没能发出。"
        );
        assert!(content_rejected_notice(&"A".repeat(500)).len() < 200);
    }

    #[test]
    fn card_turn_summary_escapes_values_but_keeps_font_template() {
        // reply 与 context_line 都是外部文本 → 消毒;`<font>` 是 summon 模板 → 必须存活。
        let c = card_turn_summary("泛型 Vec<T>", Some("问题 <x> 摘要"), None);
        assert!(c.contains("Vec<\u{200B}T>"), "reply 应拆散标签形态: {c}");
        assert!(c.contains("问题 <\u{200B}x> 摘要"), "模板里的值应消毒: {c}");
        assert!(c.contains("<font color='grey'>"), "模板标签必须存活: {c}");
    }

    /// 有序列表第 2 项下缩进 3 格的代码块。
    const LIST_ITEM_FENCE: &str = "1. 拖进 playground。\n2. 在终端里整行执行：\n   ```\n   cd /x && python fidelity.py 2>&1 | tee out.log\n   ```\n3. 把 out.tgz 拖进群里。";

    fn first_markdown(card_json: &str) -> String {
        let card: serde_json::Value = serde_json::from_str(card_json).unwrap();
        card["body"]["elements"][0]["content"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn reply_cards_lift_list_item_code_fences_to_top_level() {
        let lifted = "1. 拖进 playground。\n2. 在终端里整行执行：\n\n```\ncd /x && python fidelity.py 2>&1 | tee out.log\n```\n\n3. 把 out.tgz 拖进群里。";
        for card in [
            card_turn_summary(LIST_ITEM_FENCE, None, None),
            card_markdown_v2(LIST_ITEM_FENCE),
            card_markdown_v2_rendered(&render_external_lark_md(
                LIST_ITEM_FENCE,
                &[],
                &Default::default(),
            )),
        ] {
            assert_eq!(first_markdown(&card), lifted, "{card}");
        }
    }

    /// 配错一对就会把后面的列表围栏留在原地(或把别的行当成代码提上去)。
    #[test]
    fn fence_pairing_follows_commonmark_before_hoisting() {
        for (text, hoisted) in [
            // 顶格围栏里缩进 4 格的 ``` 是代码,顶格围栏到下一个 ``` 才闭合。
            (
                "```text\n    ```\n```\n- a\n  ```\n  x\n  ```",
                "```text\n    ```\n```\n- a\n\n```\nx\n```",
            ),
            // 同行开的围栏在第一个闭合行结束,同一项里的第二个围栏照常提。
            (
                "1. ```\n   a\n   ```\n   b\n   ```\n   c\n   ```",
                "1. ```\n   a\n   ```\n   b\n\n```\nc\n```",
            ),
        ] {
            assert_eq!(hoist_indented_code_fences(text), hoisted, "{text:?}");
        }
    }

    #[test]
    fn closing_fence_is_same_marker_and_bare() {
        for (code, closer_like) in [
            ("```", "```bash"),
            ("```", "~~~"),
            ("~~~", "```"),
            ("````", "```"),
        ] {
            let text = format!("- a\n  {code}\n  {closer_like}\n  {code}");
            assert_eq!(
                hoist_indented_code_fences(&text),
                format!("- a\n\n{code}\n{closer_like}\n{code}"),
                "{closer_like:?} 不闭合 {code:?} 围栏"
            );
        }
    }

    #[test]
    fn hoisted_code_keeps_indentation_beyond_the_fence() {
        let text = "- 改成:\n  ~~~python\n  def f():\n      return 1\n  ~~~\n  然后重跑";
        assert_eq!(
            hoist_indented_code_fences(text),
            "- 改成:\n\n~~~python\ndef f():\n    return 1\n~~~\n\n  然后重跑"
        );
    }

    #[test]
    fn top_level_unclosed_and_fenceless_text_stay_byte_identical() {
        for text in [
            "```bash\n  ls\n```\n- 列表",
            "1. 正在写:\n   ```\n   cd /x",
            "普通 `行内代码` 与 ``` 中段",
            "> ```\n> quoted\n> ```",
            // 顶格围栏里缩进 4 格的 ``` 是代码,不是闭合行。
            "```python\ndef f():\n    \"\"\"\n        ```\n        f()\n        ```\n    \"\"\"\n```\n\n后面的说明\n- 列表",
            // 与列表标记同一行起的围栏只配对:它的闭合行不能被当成新的开行。
            "1. ```\n   code\n   ```\n2. text\n\n```bash\nls\n```",
            // 列表项里的围栏没闭合,列表项结束时它也结束,不去吞后面的围栏。
            "1. step:\n   ```\n   cd x\n2. next\n\n```bash\nls\n```",
            // 信息串里带反引号的不是开行。
            "1. a\n   ```x`y\n   b\n   ```",
        ] {
            assert!(
                matches!(hoist_indented_code_fences(text), Cow::Borrowed(t) if t == text),
                "{text:?}"
            );
        }
    }

    /// 流中断卡必须给出可执行的下一步(「继续」),而不是只报个错。
    #[test]
    fn stalled_card_offers_a_continue_button() {
        let c = card_stream_stalled("已经写了一半");
        assert!(c.contains("回复中断"));
        assert!(c.contains("继续"), "必须有继续按钮:{c}");
        assert!(c.contains("retry_now"), "复用既有注入动作,不新增类型:{c}");
        assert!(c.contains("已经写了一半"), "已生成部分要留给用户");
        // 空 partial 不渲空折叠面板。
        assert!(!card_stream_stalled("   ").contains("collapsible_panel"));
    }

    /// 授权缺口卡必须给出**从错误原文取的** scope 名和可直接执行的命令 —— 2026-08-07 现场,
    /// 人靠记忆猜 scope 名猜错了(`im:chat:readonly` 根本不存在),害 owner 白授权一次。
    #[test]
    fn auth_gap_card_carries_exact_scopes_and_command() {
        let c = card_sentinel_auth_gap(
            &["im:chat:read".into()],
            "lark-cli 缺授权 scope: im:chat:read",
        );
        assert!(c.contains("im:chat:read"), "必须带 scope 原名:{c}");
        assert!(c.contains("lark-cli auth login"), "必须给可执行命令:{c}");
        assert!(c.contains("60s"), "要说明恢复方式(下一轮自愈,不用重启)");
        // device-flow 在终端完成，卡上只提供关闭入口。
        assert!(callback_values(&workflow_card(&c).unwrap()).is_empty());
    }

    /// lark-cli 只说「授权不足」没给 scope 时,卡片仍要可用:告诉 owner 去哪查,而不是渲个空壳。
    #[test]
    fn auth_gap_card_degrades_when_scopes_unknown() {
        let c = card_sentinel_auth_gap(&[], "authorization error");
        assert!(c.contains("auth status"), "至少指路到 auth status:{c}");
    }

    /// **owner 2026-08-10 实测的回归**:daemon 重启时那张过程卡被两行说明整个盖掉,
    /// 引用它引到一片空白。已经产生的内容是事实,daemon 观察不下去不构成抹掉它的理由。
    ///
    /// 上一版按"读回原卡再追加"实现,但飞书不提供读回卡片原文的能力,那条路从未生效;
    /// 现在改为从 events.jsonl 重建正文后整张重渲 —— 这条钉的是**正文真的在卡上**。
    #[test]
    fn interrupted_card_keeps_the_body_it_was_given() {
        let segments = vec![
            crate::segment::TurnSegment::Reply("我已经写了一半的正文".into()),
            crate::segment::TurnSegment::Reply("还有第二段".into()),
        ];
        let out = card_observation_interrupted_with_body(&segments, "08-10 11:04");
        assert!(
            out.contains("我已经写了一半的正文"),
            "正文一个字都不能丢:{out}"
        );
        assert!(out.contains("还有第二段"), "{out}");
        assert!(out.contains("中断了观察"), "说明也要在:{out}");
        // 顺序:正文在前、说明在后 —— 反了就成了「先告诉你中断、再给你内容」。
        assert!(
            out.find("我已经写了一半的正文").unwrap() < out.find("中断了观察").unwrap(),
            "{out}"
        );
        // 陈旧的打断按钮绝不能出现:那一轮在本 daemon 里已经不存在,留着是误导。
        assert!(!out.contains("interrupt"), "{out}");

        // 生产回归(2026-08-12):daemon 重启收敛时若把重建正文塞回普通 markdown，原 live 卡里
        // 默认收起的思考面板会被整块替换，数十条 reasoning summary 直接铺满群聊。
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        let panel = value["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| element["tag"] == "collapsible_panel")
            .expect("中断卡的已生成过程必须保持为折叠面板");
        assert_eq!(panel["expanded"], false, "已生成过程默认必须收起");
    }

    /// 那一轮还没产出任何内容(或重建失败)时,退化成纯说明卡 —— 与改动前等价,不能渲出空正文块。
    #[test]
    fn interrupted_card_without_body_falls_back_to_notice_only() {
        let out = card_observation_interrupted_with_body(&[], "08-10 11:04");
        assert!(out.contains("中断了观察"), "{out}");
        assert_eq!(
            out,
            card_observation_interrupted("08-10 11:04"),
            "应与纯说明卡逐字节一致"
        );
    }

    /// 正文过长导致整卡超限时,宁可退回纯说明卡 —— 飞书拒渲一张超限卡会让**连说明都看不到**。
    #[test]
    fn oversized_body_falls_back_instead_of_producing_a_rejected_card() {
        let huge = crate::segment::TurnSegment::Reply("x".repeat(CARD_TOTAL_SAFE_LIMIT));
        let out = card_observation_interrupted_with_body(&[huge], "");
        assert!(
            out.len() <= CARD_TOTAL_SAFE_LIMIT,
            "超限必须退回:{}",
            out.len()
        );
        assert!(out.contains("中断了观察"));
    }

    /// 孤儿卡终态只断言**观察**,不断言工作 —— daemon 死时 claude 还在 tmux 里跑,
    /// 说「已中断」或「已完成」都是它没资格下的判断。
    #[test]
    fn orphan_card_claims_observation_not_outcome() {
        let c = card_observation_interrupted("08-07 12:09");
        assert!(c.contains("中断了观察"));
        assert!(!c.contains("已完成"), "不能谎称完成:{c}");
        assert!(!c.contains("interrupt"), "终态卡不留打断按钮:{c}");
    }

    #[test]
    fn card_gate_tier_table_pins_every_action() {
        use crate::config::registry::OperateScope;
        for action in ["doc_comment_approve", "doc_comment_dismiss"] {
            assert_eq!(CardAction::from_action(action), CardAction::DocumentComment);
            assert_eq!(card_gate_tier(action), CardGate::Operate);
            assert_eq!(
                CardAction::from_action(action).operate_scope("oc_any"),
                OperateScope::Agent
            );
        }
        // T4:钉死「哪些卡片动作过 operate-gate」。改这张表 = 改安全边界,必须连测试一起改。
        use CardGate::*;
        for a in [
            "clean",
            "reset",
            "archive_ended",
            "model",
            "effort",
            "cursor_config",
            "set_workdir",
            "pick_workdir",
            "set_workdir_to",
            "tui_choice",
            "tui_text",
            "tui_esc",
            "dismiss_recovery",
            "schedule_cancel",
            "schedule_timer_reset_toggle",
            "interrupt",
            "retry_now",
            "bridge_mirror",
            "bridge_handoff",
            "bridge_context",
            "bridge_stop",
            "recall_resume",
            "recall_rpc_handoff",
            "recall_page",
            "recall_cmux_remote",
            "term_open",
            "term_regen",
            "term_auth",
            "ctx_handoff",
            "ctx_ignore",
        ] {
            assert_eq!(card_gate_tier(a), Operate, "{a} 必须过 operate-gate");
        }
        for a in [
            "keepalive",
            "compact",
            "ctx_compact",
            "recover",
            "gate_plan_approve",
            "gate_question_answer",
            "gate_ask_submit",
            "gate_ask_skip",
            "gate_cancel",
            "preview_open",
        ] {
            assert_eq!(
                card_gate_tier(a),
                None,
                "{a} 卡片层免鉴权(Core 层/白名单另管)"
            );
        }
    }

    #[test]
    fn card_operate_scope_table_pins_what_stays_inside_one_chat() {
        // 钉死「哪些卡片动作本群 operator 也能点」。改这张表 = 改本群名单的权力边界。
        use crate::config::registry::OperateScope;
        for a in [
            "model_default",
            "effort_default",
            "cursor_config_default",
            "bridge_mirror",
            "bridge_stop",
            "recall_resume",
            "recall_rpc_handoff",
            "recall_page",
            "recall_cmux_remote",
            "sentinel_confirm",
            "fleet_session_clean",
            "fleet_waiting_handled",
            "no_such_action",
        ] {
            assert_eq!(
                CardAction::from_action(a).operate_scope("oc_x"),
                OperateScope::Agent,
                "{a} 的效果越出一个群,只认 agent 级名单"
            );
        }
        for a in [
            "clean",
            "reset",
            "archive_ended",
            "model",
            "effort",
            "cursor_config",
            "codex_service_tier",
            "cursor_mode",
            "set_workdir",
            "pick_workdir",
            "set_workdir_to",
            "tui_text",
            "dismiss_recovery",
            "schedule_create",
            "interrupt",
            "quota_badge",
            "badge_effort",
            "badge_cli",
            "badge_model",
            "badge_ctx",
            "session_cot",
            "retry_now",
            "term_open",
            "term_regen",
            "term_auth",
            "preview_open",
            "ctx_compact",
            "ctx_handoff",
            "ctx_ignore",
        ] {
            assert_eq!(
                CardAction::from_action(a).operate_scope("oc_x"),
                OperateScope::Chat("oc_x"),
                "{a} 的效果只落在这个群"
            );
        }
    }

    fn ctx_card(remedy: crate::context_budget::ContextRemedy) -> ContextWarnCard {
        ContextWarnCard {
            step: 60,
            budget: crate::context_budget::ContextBudget::from_tokens(600_000, Some(1_000_000)),
            remedy,
        }
    }

    fn actions_of(card: &serde_json::Value) -> Vec<String> {
        callback_values(card)
            .iter()
            .map(|v| v["action"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// 卡上全部按钮 / 输入框;`disabled` 缺省视为可点。
    fn controls(card: &serde_json::Value) -> Vec<(String, bool)> {
        fn walk(v: &serde_json::Value, out: &mut Vec<(String, bool)>) {
            match v {
                serde_json::Value::Object(map) => {
                    if let Some(tag @ ("button" | "input")) =
                        map.get("tag").and_then(|t| t.as_str())
                    {
                        let disabled = map.get("disabled").and_then(|d| d.as_bool()) == Some(true);
                        out.push((tag.to_string(), disabled));
                    }
                    map.values().for_each(|child| walk(child, out));
                }
                serde_json::Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(card, &mut out);
        out
    }

    /// 能原地压缩的会话(claude):Compact、Handoff(先写交接再原地压缩)、Ignore 都有。
    #[test]
    fn context_warning_offers_compact_handoff_and_ignore_when_the_session_can_compact() {
        use crate::context_budget::ContextRemedy;
        let card: serde_json::Value = super::tests::workflow_card(&card_context_warning(
            &ctx_card(ContextRemedy::Compact),
            Some("sid-1"),
        ))
        .unwrap();
        assert_eq!(card["schema"], "2.0");
        assert_eq!(
            actions_of(&card),
            ["ctx_handoff", "ctx_compact", "ctx_ignore"]
        );
        let form = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "form")
            .expect("Handoff 和输入框在同一个表单里");
        assert_eq!(form["elements"][0]["name"], CTX_HANDOFF_NOTE_FIELD);
        let submit = &form["elements"][1];
        assert_eq!(submit["type"], "default", "Compact 是这张卡的主按钮");
        let confirm = submit["confirm"]["text"]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(confirm.contains("原地压缩"), "{confirm}");
        assert!(
            !confirm.contains("归档") && !confirm.contains("/resume"),
            "{confirm}"
        );
        assert!(card.to_string().contains("上下文已超过 60%"));
    }

    #[test]
    fn context_warning_offers_a_focus_box_and_handoff_when_the_session_cannot_compact() {
        use crate::context_budget::ContextRemedy;
        let card: serde_json::Value = super::tests::workflow_card(&card_context_warning(
            &ctx_card(ContextRemedy::Handoff),
            Some("sid-1"),
        ))
        .unwrap();
        assert_eq!(actions_of(&card), ["ctx_handoff", "ctx_ignore"]);
        let form = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "form")
            .expect("Handoff 和输入框在同一个表单里,提交时才带得上输入框内容");
        let input = &form["elements"][0];
        assert_eq!(input["tag"], "input");
        assert_eq!(input["name"], CTX_HANDOFF_NOTE_FIELD);
        assert!(input["placeholder"]["content"]
            .as_str()
            .unwrap()
            .contains("规划重点"));
        let submit = &form["elements"][1];
        assert_eq!(submit["form_action_type"], "submit");
        assert_eq!(submit["behaviors"][0]["value"]["action"], "ctx_handoff");
        let confirm = submit["confirm"]["text"]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(
            confirm.contains("归档") && confirm.contains("/resume"),
            "交接会归档当前会话,点下去先确认:{submit}"
        );
    }

    #[test]
    fn context_warning_buttons_carry_what_the_click_needs_to_redraw_the_card() {
        use crate::context_budget::ContextRemedy;
        for remedy in [ContextRemedy::Compact, ContextRemedy::Handoff] {
            let warn = ctx_card(remedy);
            let card: serde_json::Value =
                super::tests::workflow_card(&card_context_warning(&warn, Some("sid-1"))).unwrap();
            for value in callback_values(&card) {
                assert_eq!(ContextWarnCard::from_value(value), Some(warn), "{value}");
                assert_eq!(context_warn_session(value), Some("sid-1"));
            }
            let card: serde_json::Value =
                super::tests::workflow_card(&card_context_warning(&warn, None)).unwrap();
            for value in callback_values(&card) {
                assert_eq!(context_warn_session(value), None, "取不到会话 id 就不带");
            }
        }
        assert_eq!(
            ContextWarnCard::from_value(
                &serde_json::json!({"action": "compact", "origin": "idle"})
            ),
            None,
            "别的卡的按钮值不是上下文提醒卡"
        );
    }

    #[test]
    fn closed_context_warning_keeps_layout_and_disables_remedy_controls() {
        use crate::context_budget::ContextRemedy;
        for remedy in [ContextRemedy::Compact, ContextRemedy::Handoff] {
            let warn = ctx_card(remedy);
            let open: serde_json::Value =
                super::tests::workflow_card(&card_context_warning(&warn, Some("sid-1"))).unwrap();
            let closed: serde_json::Value = super::tests::workflow_card(
                &card_context_warning_closed(&warn, &ContextWarnOutcome::Ignored),
            )
            .unwrap();
            assert_eq!(closed["schema"], "2.0", "PATCH 不能换 schema");
            let open_controls = controls(&open);
            let closed_controls = controls(&closed);
            assert_eq!(open_controls.len(), closed_controls.len(), "布局不变");
            assert!(open_controls.iter().all(|(_, disabled)| !disabled));
            assert!(
                closed_controls.iter().all(|(_, disabled)| *disabled),
                "点过之后任何控件都不能再点:{closed_controls:?}"
            );
            let text = closed.to_string();
            assert!(text.contains("已忽略"));
            assert!(text.contains("到 80% 之前不会再提醒"));
        }
    }

    #[test]
    fn every_context_warning_outcome_says_what_happened() {
        use crate::context_budget::ContextRemedy;
        let cases = [
            (ContextWarnOutcome::Compacting, "正在压缩"),
            (ContextWarnOutcome::Compacted, "已压缩"),
            (ContextWarnOutcome::CompactFailed, "没压缩成"),
            (ContextWarnOutcome::Stale, "已结束或换过"),
            (ContextWarnOutcome::Unverified, "缺会话 id"),
            (ContextWarnOutcome::HandoffDone, "已处理过"),
            (
                ContextWarnOutcome::HandoffStarted {
                    note: String::new(),
                },
                "写交接",
            ),
            (ContextWarnOutcome::Ignored, "已忽略"),
        ];
        for (outcome, expected) in cases {
            let closed = card_context_warning_closed(&ctx_card(ContextRemedy::Compact), &outcome);
            assert!(closed.contains(expected), "{outcome:?} → {closed}");
        }
    }

    /// 发起交接后的定格说明按卡上的去向说:能原地压缩的写「原地压缩」,否则写归档换新会话。
    #[test]
    fn handoff_outcome_names_where_the_session_goes() {
        use crate::context_budget::ContextRemedy;
        let started = ContextWarnOutcome::HandoffStarted {
            note: String::new(),
        };
        let in_place = started.text(&ctx_card(ContextRemedy::Compact));
        assert!(
            in_place.contains("原地压缩") && !in_place.contains("归档"),
            "{in_place}"
        );
        let renewed = started.text(&ctx_card(ContextRemedy::Handoff));
        assert!(
            renewed.contains("归档") && !renewed.contains("原地"),
            "{renewed}"
        );
    }

    #[test]
    fn context_usage_card_estimates_the_total_or_says_what_it_cannot_tell() {
        use crate::context_budget::CompactedContext;
        let read = |compacted: Option<CompactedContext>| {
            let card: serde_json::Value =
                serde_json::from_str(&card_context_usage(compacted.as_ref())).unwrap();
            assert_eq!(card["schema"], "2.0");
            (
                card["body"]["elements"][0]["content"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                card["config"]["summary"]["content"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        };
        let (text, summary) = read(Some(CompactedContext {
            summary_tokens: 20_854,
            base_tokens: Some(58_003),
            window_tokens: Some(1_000_000),
        }));
        assert!(text.contains("78k / 1.0M"), "摘要加基础占用才是总量:{text}");
        assert!(
            text.contains("保留消息约 20k") && text.contains("基础占用约 58k"),
            "{text}"
        );
        assert!(
            text.contains("估算") && text.contains("以角标为准"),
            "{text}"
        );
        assert_eq!(summary, "上下文约 7%");

        let (text, summary) = read(Some(CompactedContext {
            summary_tokens: 20_854,
            base_tokens: None,
            window_tokens: Some(1_000_000),
        }));
        assert!(
            text.contains("保留消息约 20k") && text.contains("不含系统提示"),
            "{text}"
        );
        assert!(!text.contains('%'), "只有摘要体量不能当总量画进度:{text}");
        assert_eq!(summary, "上下文长度");

        let (text, _) = read(None);
        assert!(text.contains("没在转录里找到压缩边界"), "{text}");
        assert!(!text.contains("已压缩"), "没有证据不说压缩成了:{text}");
    }

    #[test]
    fn handoff_outcome_shows_the_focus_note_escaped() {
        use crate::context_budget::ContextRemedy;
        let closed = card_context_warning_closed(
            &ctx_card(ContextRemedy::Handoff),
            &ContextWarnOutcome::HandoffStarted {
                note: "先合 <at id=all></at> 的 MR".into(),
            },
        );
        assert!(closed.contains("接手重点"));
        assert!(closed.contains("的 MR"));
        assert!(
            !closed.contains("<at id=all>"),
            "输入框内容是外部文本,要转义"
        );
        let empty = card_context_warning_closed(
            &ctx_card(ContextRemedy::Handoff),
            &ContextWarnOutcome::HandoffStarted { note: "  ".into() },
        );
        assert!(!empty.contains("接手重点"), "没写重点就不留空行");
    }

    #[test]
    fn from_action_parses_families_and_unknown() {
        // 钉死字符串→枚举映射:前缀族镜像 dispatcher 路由,未识别串落 Unknown。
        use CardAction::*;
        assert_eq!(CardAction::from_action("gate_plan_approve"), GateFamily);
        assert_eq!(CardAction::from_action("gate_question_answer"), GateFamily);
        assert_eq!(CardAction::from_action("gate_ask_submit"), GateFamily);
        assert_eq!(CardAction::from_action("gate_cancel"), GateFamily);
        assert_eq!(CardAction::from_action("bridge_mirror"), BridgeFamily);
        assert_eq!(CardAction::from_action("bridge_handoff"), BridgeFamily);
        assert_eq!(CardAction::from_action("bridge_context"), BridgeFamily);
        assert_eq!(CardAction::from_action("bridge_stop"), BridgeFamily);
        assert_eq!(CardAction::from_action("recall_resume"), RecallResume);
        assert_eq!(
            CardAction::from_action("recall_rpc_handoff"),
            RecallRpcHandoff
        );
        assert_eq!(CardAction::from_action("recall_page"), RecallPage);
        assert_eq!(
            CardAction::from_action("recall_cmux_remote"),
            RecallCmuxRemote
        );
        assert_eq!(CardAction::from_action("clean"), Clean);
        assert_eq!(CardAction::from_action("interrupt"), Interrupt);
        assert_eq!(CardAction::from_action("keepalive"), KeepAlive);
        assert_eq!(CardAction::from_action("pick_workdir"), PickWorkdir);
        assert_eq!(CardAction::from_action("set_workdir_to"), SetWorkdirTo);
        assert_eq!(PickWorkdir.gate(), CardGate::Operate);
        assert_eq!(SetWorkdirTo.gate(), CardGate::Operate);
        // 预览收起:免鉴权。撤的是 bot 自己刚发的一张卡,没有持久后果;而被长卡挡住的人
        // 如果收不起来才是真没辙。**这条 assert 是防止有人"顺手收紧"成 Operate 的护栏。**
        assert_eq!(CardAction::from_action("fleet_refresh"), FleetRefresh);
        assert_eq!(FleetRefresh.gate(), CardGate::None);
        // 标记已处理改盘上的 waiting 标记:写本机持久状态,过 operate 闸。
        assert_eq!(
            CardAction::from_action("fleet_waiting_handled"),
            FleetWaitingHandled
        );
        assert_eq!(FleetWaitingHandled.gate(), CardGate::Operate);
        // 未识别 / 空 → Unknown(分派侧走无害兜底,鉴权侧 fail-closed)。
        assert_eq!(CardAction::from_action("purge_memory"), Unknown);
        assert_eq!(CardAction::from_action(""), Unknown);
        // 前缀家族的 gate 免卡片层鉴权(Core 层另管)。
        assert_eq!(GateFamily.gate(), CardGate::None);
        assert_eq!(BridgeFamily.gate(), CardGate::Operate);
        // 召回列表本身含敏感上下文，恢复/迁移/翻页都归 operate 档；专用 handler 再按目标群复验。
        assert_eq!(RecallResume.gate(), CardGate::Operate);
        assert_eq!(RecallRpcHandoff.gate(), CardGate::Operate);
        assert_eq!(RecallPage.gate(), CardGate::Operate);
        // 遥控会把群消息打进本机一个外部进程的输入框,与接管同档。
        assert_eq!(RecallCmuxRemote.gate(), CardGate::Operate);
        // 终端卡片动作:一张能操作会话的终端卡,与按键注入同档。
        assert_eq!(CardAction::from_action("term_open"), TerminalOpen);
        assert_eq!(CardAction::from_action("term_regen"), TerminalRegenerate);
        assert_eq!(TerminalOpen.gate(), CardGate::Operate);
        assert_eq!(TerminalRegenerate.gate(), CardGate::Operate);
        // 链接点击回调:终端按钮的 handler 自己判收件人 + operate;文件按钮按 talk gate 或本群
        // operate-gate 判,判不过也要走到 handler 才能把激活窗口作废,所以卡片层放行。
        assert_eq!(CardAction::from_action("term_auth"), TerminalAuth);
        assert_eq!(TerminalAuth.gate(), CardGate::Operate);
        assert_eq!(CardAction::from_action("preview_open"), PreviewOpen);
        assert_eq!(PreviewOpen.gate(), CardGate::None);
    }

    /// 控制台卡:一个只打开链接的按钮,没有回调;写明链接的有效期与进入后的会话期限。
    #[test]
    fn dash_card_button_only_opens_the_link() {
        let url = "http://10.0.0.5:7767/l/0123abcd";
        let card = card_dashboard(url, 3600);
        let v: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert_eq!(v["schema"], "2.0");
        let text = card.as_str();
        assert!(text.contains("summon 控制台"), "{text}");
        assert!(
            text.contains("点按钮打开。链接 1 小时内有效、只能用一次;进入后 12 小时内有效。"),
            "{text}"
        );
        let button = find_button(&v, "打开控制台").expect("要有打开控制台按钮");
        let behaviors = button["behaviors"].as_array().unwrap();
        assert_eq!(behaviors.len(), 1, "只打开链接");
        assert_eq!(behaviors[0]["type"], "open_url");
        for key in ["default_url", "pc_url", "ios_url", "android_url"] {
            assert_eq!(behaviors[0][key], url, "{key}");
        }
        assert!(callback_values(&v).is_empty(), "没有回调按钮");
        assert!(card_dashboard(url, 600).contains("链接 10 分钟内有效"));
    }

    fn preview_button(n: usize) -> PreviewButton {
        PreviewButton {
            label: format!("docs/f{n}.md"),
            token: format!("t{n}"),
            is_dir: n == 0,
        }
    }

    #[test]
    fn preview_buttons_go_at_the_end_of_a_v2_card_and_replace_the_previous_set() {
        let base = card_markdown_v2("回复正文");
        let buttons: Vec<PreviewButton> = (0..10).map(preview_button).collect();
        let card = card_with_preview_buttons(&base, &buttons).unwrap();
        let v: serde_json::Value = serde_json::from_str(&card).unwrap();
        let elements = v["body"]["elements"].as_array().unwrap();
        let set = &elements[elements.len() - 2];
        assert_eq!(set["element_id"], "preview_files");
        assert_eq!(
            set["columns"].as_array().unwrap().len(),
            PREVIEW_BUTTONS_MAX
        );
        assert!(elements
            .last()
            .unwrap()
            .to_string()
            .contains("另有 2 个文件未列出"));
        let first = &set["columns"][0]["elements"][0];
        assert_eq!(first["text"]["content"], "📁 docs/f0.md");
        assert_eq!(
            set["columns"][1]["elements"][0]["text"]["content"],
            "📄 docs/f1.md"
        );
        assert_eq!(
            first["behaviors"],
            serde_json::json!([{"type": "callback", "value": {"action": "preview_open", "token": "t0"}}]),
            "按钮只回传点击,不带打开链接"
        );

        let again = card_with_preview_buttons(&card, &buttons[1..2]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&again).unwrap();
        let elements = v["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements
                .iter()
                .filter(|el| el["element_id"] == "preview_files")
                .count(),
            1
        );
        assert!(!again.contains("另有"), "按钮数没超就不留个数说明");
        assert!(again.contains("回复正文"));
        let cleared = card_with_preview_buttons(&card, &[]).unwrap();
        assert!(!cleared.contains("preview_files"));
    }

    #[test]
    fn preview_buttons_never_push_a_reply_past_the_card_budget() {
        let buttons: Vec<PreviewButton> = (0..3).map(preview_button).collect();
        let one = card_with_preview_buttons(&card_markdown_v2("x"), &buttons[..1]).unwrap();
        let per_button = one.len() - card_markdown_v2("x").len();
        // 正文卡离单卡上限只剩 `room` 字节。
        let body_for = |room: usize| {
            let mut chars = (CARD_TOTAL_SAFE_LIMIT - room) / 3;
            loop {
                let card = card_markdown_v2(&"字".repeat(chars));
                if card.len() + room <= CARD_TOTAL_SAFE_LIMIT {
                    return card;
                }
                chars -= 1;
            }
        };

        let base = card_markdown_v2("x");
        let more_line = attach_preview_buttons(&base, &buttons[..1], 3)
            .unwrap()
            .len()
            - attach_preview_buttons(&base, &buttons[..1], 1)
                .unwrap()
                .len();
        let note = attach_preview_buttons(&base, &[], 3).unwrap().len() - base.len();
        assert!(note + 16 < per_button);
        let shown_in = |card: &str| {
            let v: serde_json::Value = serde_json::from_str(card).unwrap();
            v["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .find(|el| el["element_id"] == "preview_files")
                .map_or(0, |set| set["columns"].as_array().unwrap().len())
        };

        let roomy = card_with_preview_buttons(&body_for(per_button * 3), &buttons).unwrap();
        assert!(roomy.len() <= CARD_TOTAL_SAFE_LIMIT);
        assert_eq!(shown_in(&roomy), 3, "放得下就全挂");
        assert!(!roomy.contains("另有"));

        let partial =
            card_with_preview_buttons(&body_for(per_button + more_line + 16), &buttons).unwrap();
        assert!(partial.len() <= CARD_TOTAL_SAFE_LIMIT);
        assert_eq!(shown_in(&partial), 1, "放不下就从末尾少挂");
        assert!(partial.contains("另有 2 个文件未列出"));

        let tight = card_with_preview_buttons(&body_for(note + 16), &buttons).unwrap();
        assert!(tight.len() <= CARD_TOTAL_SAFE_LIMIT);
        assert_eq!(shown_in(&tight), 0);
        assert!(tight.contains("回复里提到的 3 个文件放不下打开按钮"));

        let full = body_for(0);
        assert_eq!(
            card_with_preview_buttons(&full, &buttons).unwrap(),
            serde_json::from_str::<serde_json::Value>(&full)
                .unwrap()
                .to_string(),
            "连个数说明也放不下就只发正文"
        );
    }

    #[test]
    fn the_private_open_card_carries_only_an_opening_link() {
        let card = card_preview_link("docs/<x>.md", false, "http://10.0.0.5:7765/p/abc", 3600);
        let v: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert_eq!(v["schema"], "2.0");
        let text = v["body"]["elements"][0]["content"].as_str().unwrap();
        assert!(text.contains("📄"), "{text}");
        assert!(text.contains("1 小时内有效"), "{text}");
        assert!(!text.contains("<x>"), "文件名里的标签形态要转义:{text}");
        let button = &v["body"]["elements"][1]["columns"][0]["elements"][0];
        assert_eq!(
            button["behaviors"],
            serde_json::json!([{
                "type": "open_url",
                "default_url": "http://10.0.0.5:7765/p/abc",
                "pc_url": "http://10.0.0.5:7765/p/abc",
                "ios_url": "http://10.0.0.5:7765/p/abc",
                "android_url": "http://10.0.0.5:7765/p/abc"
            }]),
            "打开卡不回传点击"
        );
        assert!(card_preview_link("docs", true, "u", 3600).contains("📁"));
    }

    #[test]
    fn preview_buttons_leave_v1_cards_alone() {
        let v1 = r#"{"config":{},"elements":[{"tag":"markdown","content":"x"}]}"#;
        assert_eq!(
            card_with_preview_buttons(v1, &[preview_button(1)]).unwrap(),
            v1
        );
        assert_eq!(
            card_with_preview_buttons("not json", &[preview_button(1)]),
            None
        );
    }

    #[test]
    fn a_notice_card_offers_the_terminal_button() {
        let card = card_notice_with_terminal("**本条没有执行**");
        let v: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert_eq!(v["schema"], "2.0");
        assert!(card.contains("本条没有执行"));
        assert!(find_callback_value(&v, "term_open").is_some());
    }

    #[test]
    fn the_session_panel_shows_the_terminal_button_only_when_asked() {
        let with = SessionPanelControls {
            terminal: true,
            ..SessionPanelControls::from(false)
        };
        let panel = |c: SessionPanelControls| {
            card_session_panel(
                "gabc",
                "claude",
                "tmux",
                &crate::model_effort::ResolvedModel {
                    value: None,
                    source: crate::model_effort::ModelSource::CliDefault,
                },
                &crate::model_effort::ResolvedEffort {
                    value: None,
                    source: crate::model_effort::EffortSource::ModelInherent,
                },
                &[],
                &[],
                &crate::subagent::SubagentDisplay::default(),
                c,
            )
        };
        assert!(panel(with).contains("\"term_open\""));
        assert!(!panel(SessionPanelControls::from(false)).contains("\"term_open\""));
    }

    #[test]
    fn the_raw_menu_card_offers_the_terminal_button_only_when_asked() {
        assert!(card_interactive_raw("some screen", 10, true).contains("\"term_open\""));
        assert!(!card_interactive_raw("some screen", 10, false).contains("\"term_open\""));
    }

    /// 回执正文里有用户发的命令原文与错误串:进卡前要拆掉标签形态,否则 `<at user_id="all">`
    /// 会被飞书当成真 mention 渲染。
    #[test]
    fn a_notice_card_breaks_tags_in_its_text() {
        let card = card_notice_with_terminal("`/foo <at user_id=\"all\">x</at>` 没有执行");
        let v: serde_json::Value = serde_json::from_str(&card).unwrap();
        let text = v["body"]["elements"][0]["content"].as_str().unwrap();
        assert!(!text.contains("<at"), "{text}");
        assert!(!text.contains("</at"), "{text}");
        assert!(text.contains(&format!("<{LARK_MD_TAG_BREAKER}at")));
    }

    fn find_button(v: &serde_json::Value, label: &str) -> Option<serde_json::Value> {
        match v {
            serde_json::Value::Object(map) => {
                if map.get("tag").and_then(|t| t.as_str()) == Some("button")
                    && v["text"]["content"] == label
                {
                    return Some(v.clone());
                }
                map.values().find_map(|x| find_button(x, label))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|x| find_button(x, label)),
            _ => None,
        }
    }

    fn find_callback_value(v: &serde_json::Value, action: &str) -> Option<serde_json::Value> {
        match v {
            serde_json::Value::Object(map) => {
                if map.get("action").and_then(|a| a.as_str()) == Some(action) {
                    return Some(v.clone());
                }
                map.values().find_map(|x| find_callback_value(x, action))
            }
            serde_json::Value::Array(items) => {
                items.iter().find_map(|x| find_callback_value(x, action))
            }
            _ => None,
        }
    }

    /// 卡片是给人读的。此前这里直接打了 `last_seen_ms` 和 `{:?}` 的枚举,飞书上就出现
    /// 「状态:`ActiveIdle` · 最近活动:1784044645534」—— 一串 unix 毫秒和一个 Rust 枚举名。
    #[test]
    fn bridge_card_shows_human_readable_state_and_time_not_raw_epoch() {
        use crate::bridge::{
            ClaudeProvider, ExternalLifecycle, HandoffEligibility, SafeBridgeCandidate,
        };
        let now_ms: u64 = 1_784_044_645_534;
        let candidates = vec![SafeBridgeCandidate {
            choice_id: "choice-a".into(),
            project_name: "client-ai-llm-eval-e2e".into(),
            lifecycle: ExternalLifecycle::ActiveIdle,
            handoff: HandoffEligibility::Eligible,
            source_provider: ClaudeProvider::Default,
            last_seen_ms: now_ms - 12 * 60 * 1_000, // 12 分钟前
        }];
        let card = card_bridge_ticket("t-1", &candidates, ClaudeProvider::Default, now_ms);

        assert!(card.contains("状态:空闲"), "枚举名不该出现在卡片上:{card}");
        assert!(!card.contains("ActiveIdle"));
        assert!(card.contains("12m前"), "最近活动要人读:{card}");
        assert!(
            !card.contains(&(now_ms - 12 * 60 * 1_000).to_string()),
            "unix 毫秒不该出现在卡片上"
        );
    }

    #[test]
    fn every_lifecycle_has_a_human_label() {
        use crate::bridge::ExternalLifecycle::*;
        for state in [Unverified, ActiveIdle, ActiveBusy, Ended, Conflict] {
            let label = state.label();
            assert!(!label.is_empty());
            assert!(!label.is_ascii(), "标签应是中文人话:{label}");
        }
    }

    #[test]
    fn last_seen_in_the_future_reads_as_just_now() {
        // 时钟回拨:last_seen 在 now 之后。不该谎报成一个巨大的时长。
        assert_eq!(crate::bridge::humanize_last_seen(2_000, 1_000), "刚刚");
    }

    #[test]
    fn bridge_card_callback_values_contain_only_opaque_ticket_choice_and_action() {
        use crate::bridge::{
            ClaudeProvider, ExternalLifecycle, HandoffBlock, HandoffEligibility,
            SafeBridgeCandidate,
        };
        let candidates = vec![
            SafeBridgeCandidate {
                choice_id: "choice-m".into(),
                project_name: "project-<T>".into(),
                lifecycle: ExternalLifecycle::ActiveIdle,
                handoff: HandoffEligibility::MirrorOnly(HandoffBlock::UnverifiedProcess),
                source_provider: ClaudeProvider::Default,
                last_seen_ms: 10,
            },
            SafeBridgeCandidate {
                choice_id: "choice-h".into(),
                project_name: "project-b".into(),
                lifecycle: ExternalLifecycle::ActiveBusy,
                handoff: HandoffEligibility::Eligible,
                source_provider: ClaudeProvider::Custom,
                last_seen_ms: 11,
            },
        ];
        let card: serde_json::Value = super::tests::workflow_card(&card_bridge_ticket(
            "ticket-opaque",
            &candidates,
            ClaudeProvider::Default,
            1_000,
        ))
        .unwrap();
        let mut values = Vec::new();
        fn collect_values<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("type").and_then(|v| v.as_str()) == Some("callback") {
                        out.push(map.get("value").expect("callback 必须有 value"));
                    }
                    for child in map.values() {
                        collect_values(child, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for child in items {
                        collect_values(child, out);
                    }
                }
                _ => {}
            }
        }
        collect_values(&card, &mut values);
        assert_eq!(
            values.len(),
            2,
            "忙碌候选只能远程接管，不能从半轮开始 Mirror"
        );
        assert!(card.to_string().contains("远程接管"));
        assert!(card.to_string().contains("confirm"));
        assert!(card
            .to_string()
            .contains("自定义 API/Provider → 当前 Bot 默认 Claude 账号"));
        assert!(card
            .to_string()
            .contains("接管不可用：源会话身份或恢复路径未通过复验"));
        for value in values {
            let keys = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                keys,
                std::collections::BTreeSet::from(["action", "choice", "ticket"])
            );
        }
        let json = card.to_string();
        assert!(json.contains("project-<\u{200B}T>"));
        assert!(!json.contains("project-<T>"));
        for private_field in [
            "session_id",
            "cwd",
            "config_dir",
            "transcript",
            "target_chat",
        ] {
            assert!(
                !json.contains(private_field),
                "卡片泄露字段 {private_field}"
            );
        }
    }

    // 递归收集卡片里所有 button 节点(测试用)。
    fn collect_buttons(v: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
        match v {
            serde_json::Value::Object(map) => {
                if map.get("tag").and_then(|t| t.as_str()) == Some("button") {
                    out.push(v.clone());
                }
                for child in map.values() {
                    collect_buttons(child, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for c in arr {
                    collect_buttons(c, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn recall_card_shows_count_distinct_buttons_and_opaque_callbacks() {
        use crate::recall::SessionCandidate;

        let items = vec![
            SessionCandidate {
                provider: crate::recall::RecallProvider::Claude,
                ownership: crate::recall::RecallOwnership::External,
                sid: "sid-active".into(),
                cwd: "/secret/machine/proj".into(),
                name: "活跃会话".into(),
                prompt_preview: "修复登录页崩溃并补回归测试；这里还有一段超过旧版四十八字限制、但仍应完整显示的任务背景。".into(),
                last_active_ms: 5_000,
                live_pid: Some(1234),
                live_status: Some("idle".into()),
                source_observation_id: Some("00000000-0000-4000-8000-000000000001".into()),
                background_tasks: Vec::new(),
            },
            SessionCandidate {
                provider: crate::recall::RecallProvider::Claude,
                ownership: crate::recall::RecallOwnership::External,
                sid: "sid-hist".into(),
                cwd: "/secret/machine/hist".into(),
                name: String::new(),
                prompt_preview: String::new(),
                last_active_ms: 1_000,
                live_pid: None,
                live_status: None,
                source_observation_id: None,
                background_tasks: Vec::new(),
            },
        ];
        let card = card_recall(
            crate::recall::RecallProvider::Claude,
            &items,
            2,
            25,
            3,
            10_000,
            None,
        );
        let v: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert_eq!(v["schema"], "2.0");

        // 标题含条数(本页 2 / 共 25)与页码。
        let header = v["body"]["elements"][0]["content"].as_str().unwrap();
        assert!(header.contains("2/25"), "标题应含条数: {header}");
        assert!(header.contains("第 2/3 页"), "标题应含页码: {header}");

        // 折叠标题是短索引，完整起始任务仍原样保存在展开区；无预览时回落到「(无名)」。
        let full_prompt =
            "修复登录页崩溃并补回归测试；这里还有一段超过旧版四十八字限制、但仍应完整显示的任务背景。";
        assert!(card.contains(full_prompt));
        assert!(card.contains("(无名)"), "空预览+空 name 应显示 (无名)");
        // cwd 只显示尾两级，降低噪音；仍足以辨认项目位置。
        assert!(card.contains("…/machine/proj"), "cwd 应以紧凑路径展示");
        assert!(!card.contains("/secret/machine/proj"));

        // 每个会话是一条左右分栏：左侧默认收起、右侧按钮常驻。展开区保留完整任务、路径、
        // Claude 会话名；折叠标题只做扫描索引，不重复撑高卡片。
        let rows = &v["body"]["elements"].as_array().unwrap()[1..=2];
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row["tag"], "column_set");
            assert_eq!(row["flex_mode"], "flow");
            let columns = row["columns"].as_array().unwrap();
            assert_eq!(columns.len(), 2);
            assert_eq!(columns[0]["width"], "weighted");
            let panel = &columns[0]["elements"][0];
            assert_eq!(panel["tag"], "collapsible_panel");
            assert_eq!(panel["expanded"], false, "候选默认收起");
            assert_eq!(panel["elements"][0]["tag"], "markdown");
            assert_eq!(columns[1]["width"], "auto");
            assert_eq!(columns[1]["elements"][0]["tag"], "button");
            assert_eq!(columns[1]["vertical_align"], "top");
            if index == 0 {
                let folded = panel["header"]["title"]["content"].as_str().unwrap();
                let detail = panel["elements"][0]["content"].as_str().unwrap();
                assert!(folded.contains("🟢 活跃·刚刚"));
                assert!(folded.contains('…'), "长任务折叠标题应有省略号: {folded}");
                assert!(!folded.contains(full_prompt), "折叠态不得重复完整任务");
                assert!(detail.contains(full_prompt), "展开区必须保留完整任务");
                assert!(detail.contains("…/machine/proj"));
                assert!(detail.contains("活跃会话"));
            }
        }

        let mut buttons = Vec::new();
        collect_buttons(&v, &mut buttons);
        // 按 callback 里的 sid 定位活跃/历史两个恢复按钮。
        let button_for_sid = |sid: &str| -> serde_json::Value {
            buttons
                .iter()
                .find(|b| {
                    b["behaviors"]
                        .as_array()
                        .and_then(|arr| arr.first())
                        .and_then(|beh| beh["value"]["sid"].as_str())
                        == Some(sid)
                })
                .cloned()
                .unwrap_or_else(|| panic!("找不到 sid={sid} 的按钮"))
        };
        let active_btn = button_for_sid("sid-active");
        let hist_btn = button_for_sid("sid-hist");

        // 活跃/历史按钮文案不同。
        let active_label = active_btn["text"]["content"].as_str().unwrap();
        let hist_label = hist_btn["text"]["content"].as_str().unwrap();
        assert_ne!(active_label, hist_label, "活跃/历史按钮文案应不同");
        assert!(
            active_label.contains("接管"),
            "活跃按钮应是接管: {active_label}"
        );
        assert!(
            hist_label.contains("恢复"),
            "历史按钮应是恢复: {hist_label}"
        );

        // 活跃接管是危险色,历史恢复不是。
        assert_eq!(active_btn["type"], "danger", "活跃接管应标 danger 危险色");
        assert_ne!(hist_btn["type"], "danger", "历史恢复不应是 danger");

        // callback 只含不透明字段,绝不含 cwd/transcript/进程指纹。活跃项额外带本机
        // observation UUID；历史项仍只有 action+sid。
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let resume_cbs: Vec<&serde_json::Value> = cbs
            .iter()
            .filter(|c| c["action"] == "recall_resume")
            .collect();
        assert_eq!(resume_cbs.len(), 2, "两个候选各一个恢复 callback");
        for c in &resume_cbs {
            let obj = c.as_object().unwrap();
            assert!(
                obj.contains_key("action") && obj.contains_key("cli") && obj.contains_key("sid")
            );
            assert_eq!(obj["cli"], "claude");
            assert!(!obj.contains_key("cwd"), "callback 不得含 cwd");
            assert!(
                !obj.contains_key("prompt_preview"),
                "callback 不得含任务预览"
            );
            assert!(!obj.contains_key("process"), "callback 不得含进程指纹");
            assert!(
                !c.to_string().contains("/secret"),
                "callback 不得泄露 cwd 路径: {c}"
            );
            assert!(
                !c.to_string().contains("修复登录页"),
                "callback 不得泄露任务预览: {c}"
            );
        }
        let active_callback = resume_cbs
            .iter()
            .find(|callback| callback["sid"] == "sid-active")
            .unwrap();
        assert_eq!(
            active_callback["source_observation"],
            "00000000-0000-4000-8000-000000000001"
        );
        let historical_callback = resume_cbs
            .iter()
            .find(|callback| callback["sid"] == "sid-hist")
            .unwrap();
        assert!(historical_callback.get("source_observation").is_none());

        // 分页按钮:page=2 / total_pages=3 → 上一页(1)+下一页(3)都在。
        let pages: Vec<u64> = cbs
            .iter()
            .filter(|c| c["action"] == "recall_page")
            .filter_map(|c| c["page"].as_u64())
            .collect();
        assert!(pages.contains(&1), "应有上一页 → page 1: {pages:?}");
        assert!(pages.contains(&3), "应有下一页 → page 3: {pages:?}");
        assert!(cbs
            .iter()
            .filter(|callback| callback["action"] == "recall_page")
            .all(|callback| callback["cli"] == "claude"));
    }

    #[test]
    fn headless_active_recall_is_visible_but_has_no_callback_capability() {
        use crate::recall::{RecallProvider, SessionCandidate};
        let items = vec![
            SessionCandidate {
                provider: RecallProvider::Cursor,
                ownership: crate::recall::RecallOwnership::External,
                sid: "cursor-active".into(),
                cwd: "/x".into(),
                name: "使用中".into(),
                prompt_preview: String::new(),
                last_active_ms: 2,
                live_pid: Some(42),
                live_status: None,
                source_observation_id: None,
                background_tasks: Vec::new(),
            },
            SessionCandidate {
                provider: RecallProvider::Cursor,
                ownership: crate::recall::RecallOwnership::External,
                sid: "cursor-history".into(),
                cwd: "/x".into(),
                name: "历史".into(),
                prompt_preview: String::new(),
                last_active_ms: 1,
                live_pid: None,
                live_status: None,
                source_observation_id: None,
                background_tasks: Vec::new(),
            },
        ];
        let card = card_recall(RecallProvider::Cursor, &items, 1, 2, 1, 10, None);
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut buttons = Vec::new();
        collect_buttons(&value, &mut buttons);
        let disabled = buttons
            .iter()
            .find(|button| button["text"]["content"] == "使用中")
            .expect("活跃 Cursor 应显示禁用按钮");
        assert_eq!(disabled["disabled"], true);
        assert!(
            disabled["behaviors"].as_array().is_some_and(Vec::is_empty),
            "禁用项不能携带可重放 callback capability"
        );
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        let resume = callbacks
            .iter()
            .filter(|callback| callback["action"] == "recall_resume")
            .collect::<Vec<_>>();
        assert_eq!(resume.len(), 1);
        assert_eq!(resume[0]["cli"], "cursor");
        assert_eq!(resume[0]["sid"], "cursor-history");
        assert!(!card.contains("退出源终端里那个 Claude"));
    }

    #[test]
    fn managed_rpc_recall_exposes_only_the_handoff_capability() {
        use crate::recall::{RecallOwnership, RecallProvider, SessionCandidate};
        let items = vec![SessionCandidate {
            provider: RecallProvider::Codex,
            ownership: RecallOwnership::ManagedRpc,
            sid: "thread-owned".into(),
            cwd: "/x".into(),
            name: "源群".into(),
            prompt_preview: String::new(),
            last_active_ms: 1,
            live_pid: Some(42),
            live_status: Some("idle".into()),
            source_observation_id: Some("opaque-source-revision".into()),
            background_tasks: Vec::new(),
        }];
        let card = card_recall(
            RecallProvider::Codex,
            &items,
            1,
            1,
            1,
            10,
            Some(CardRecallOrigin {
                chat_id: "oc_target",
                chat_type: "group",
            }),
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        let handoff = callbacks
            .iter()
            .find(|callback| callback["action"] == "recall_rpc_handoff")
            .expect("托管 RPC 候选应只有迁移 capability");
        assert_eq!(handoff["cli"], "codex");
        assert_eq!(handoff["sid"], "thread-owned");
        assert_eq!(handoff["chat_id"], "oc_target");
        assert_eq!(handoff["source_observation"], "opaque-source-revision");
        assert_eq!(handoff["observed_at_ms"], 10);
        assert!(!callbacks
            .iter()
            .any(|callback| callback["action"] == "recall_resume"));
        assert!(card.contains("托管·空闲"));
        assert!(card.contains("迁移"));
    }

    fn claude_candidate(sid: &str, live_pid: Option<u32>) -> crate::recall::SessionCandidate {
        crate::recall::SessionCandidate {
            provider: crate::recall::RecallProvider::Claude,
            ownership: crate::recall::RecallOwnership::External,
            sid: sid.into(),
            cwd: "/x".into(),
            name: "n".into(),
            prompt_preview: "task".into(),
            last_active_ms: 1,
            live_pid,
            live_status: None,
            source_observation_id: None,
            background_tasks: Vec::new(),
        }
    }

    /// 卡上每个「接管」按钮:(callback 的 sid, 确认框正文, callback value)。
    fn takeover_buttons(card: &str) -> Vec<(String, String, serde_json::Value)> {
        fn walk(v: &serde_json::Value, out: &mut Vec<(String, String, serde_json::Value)>) {
            match v {
                serde_json::Value::Object(map) => {
                    if map.get("tag").and_then(|t| t.as_str()) == Some("button")
                        && v["text"]["content"] == "接管"
                    {
                        let value = v["behaviors"][0]["value"].clone();
                        out.push((
                            value["sid"].as_str().unwrap_or_default().to_string(),
                            v["confirm"]["text"]["content"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            value,
                        ));
                    }
                    for child in map.values() {
                        walk(child, out);
                    }
                }
                serde_json::Value::Array(items) => items.iter().for_each(|child| walk(child, out)),
                _ => {}
            }
        }
        let value: serde_json::Value = serde_json::from_str(card).unwrap();
        let mut out = Vec::new();
        walk(&value, &mut out);
        out
    }

    #[test]
    fn busy_takeover_confirm_names_the_interruption_and_the_background_tasks() {
        let idle = claude_candidate("idle", Some(41));
        let mut busy_main_turn = claude_candidate("busy-turn", Some(42));
        busy_main_turn.live_status = Some("busy".into());
        let mut busy_with_tasks = claude_candidate("busy-tasks", Some(43));
        busy_with_tasks.live_status = Some("busy".into());
        busy_with_tasks.background_tasks = [
            "后台命令「心跳」",
            "子 agent「审一下」",
            "后台命令「跑全量测试」",
            "子 agent「写报告」",
        ]
        .map(String::from)
        .to_vec();
        let card = card_recall(
            crate::recall::RecallProvider::Claude,
            &[idle, busy_main_turn, busy_with_tasks],
            1,
            3,
            1,
            10,
            Some(CardRecallOrigin {
                chat_id: "oc_g",
                chat_type: "group",
            }),
        );
        let buttons = takeover_buttons(&card);
        assert_eq!(buttons.len(), 3);
        assert_eq!(buttons[0].0, "idle");
        assert_eq!(
            buttons[0].1,
            "将退出源终端里那个 Claude 进程,再把会话恢复为本 Bot 的托管会话。只结束经复验的 Claude PID,不碰 shell/终端/tmux。确认接管?",
            "不忙的会话文案不变"
        );
        assert_eq!(buttons[1].0, "busy-turn");
        assert_eq!(
            buttons[1].1,
            "会话正忙:接管会打断这一轮并停掉后台任务(如有)。将退出源 Claude 进程(不碰终端/tmux),恢复为本 Bot 托管会话。确认接管?"
        );
        assert!(
            buttons[1].1.len() <= buttons[0].1.len(),
            "忙时文案不比平时长,极限页预算靠这一条"
        );
        assert_eq!(buttons[2].0, "busy-tasks");
        assert_eq!(
            buttons[2].1,
            "会话正忙:接管会打断这一轮并停掉后台任务:后台命令「心跳」、子 agent「审一下」、\
             后台命令「跑全量测试」 等 4 个。将退出源 Claude 进程(不碰终端/tmux),恢复为本 Bot \
             托管会话;续上后 Claude 知道哪些任务停了,需要可让它重启。确认接管?"
        );
        assert!(
            !buttons[2].2.to_string().contains("心跳"),
            "后台任务清单只进确认框,不进 callback"
        );
    }

    #[test]
    fn remote_button_sits_beside_takeover_only_for_active_claude_in_the_remote_set() {
        let items = vec![
            claude_candidate("in-cmux", Some(42)),
            claude_candidate("elsewhere", Some(43)),
            claude_candidate("history-in-cmux", None),
        ];
        let remote = RecallRemoteMarks {
            offer: ["in-cmux", "history-in-cmux"]
                .into_iter()
                .map(String::from)
                .collect(),
            controlled: Default::default(),
        };
        let card = card_recall_with_remote(
            crate::recall::RecallProvider::Claude,
            &items,
            1,
            3,
            1,
            10,
            Some(CardRecallOrigin {
                chat_id: "oc_g",
                chat_type: "group",
            }),
            &remote,
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        let remote_callbacks: Vec<_> = callbacks
            .iter()
            .filter(|callback| callback["action"] == "recall_cmux_remote")
            .collect();
        assert_eq!(
            remote_callbacks.len(),
            1,
            "只有活跃且在 cmux 里的那条有遥控"
        );
        assert_eq!(remote_callbacks[0]["sid"], "in-cmux");
        assert_eq!(remote_callbacks[0]["cli"], "claude");
        assert_eq!(remote_callbacks[0]["chat_id"], "oc_g");
        assert_eq!(remote_callbacks[0]["chat_type"], "group");
        assert!(card.contains("遥控"));
        assert!(card.contains("不会结束或重启这个进程"), "遥控要二次确认");
        assert!(
            card.contains("将把目标群接到这条 cmux 会话上"),
            "卡在私聊里,接的是发起 /bridge 的目标群"
        );
        // 接管照旧都在,遥控是多出来的一个按钮,与接管同一行。
        let row = value["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| element.to_string().contains("recall_cmux_remote"))
            .expect("遥控按钮所在的行");
        assert_eq!(row["tag"], "column_set");
        assert_eq!(row["columns"].as_array().unwrap().len(), 3);
        let mut row_callbacks = Vec::new();
        collect_callback_values(row, &mut row_callbacks);
        assert!(row_callbacks
            .iter()
            .any(|callback| callback["action"] == "recall_resume" && callback["sid"] == "in-cmux"));
        assert_eq!(
            callbacks
                .iter()
                .filter(|callback| callback["action"] == "recall_resume")
                .count(),
            3
        );
    }

    #[test]
    fn card_recall_is_card_recall_with_remote_with_an_empty_set() {
        let items = vec![
            claude_candidate("active", Some(42)),
            claude_candidate("history", None),
        ];
        let origin = Some(CardRecallOrigin {
            chat_id: "oc_g",
            chat_type: "group",
        });
        assert_eq!(
            card_recall(
                crate::recall::RecallProvider::Claude,
                &items,
                1,
                30,
                3,
                10,
                origin
            ),
            card_recall_with_remote(
                crate::recall::RecallProvider::Claude,
                &items,
                1,
                30,
                3,
                10,
                origin,
                &RecallRemoteMarks::default(),
            )
        );
    }

    #[test]
    fn remote_controlled_session_gets_only_a_disabled_button() {
        let items = vec![
            claude_candidate("controlled", Some(42)),
            claude_candidate("controlled-history", None),
            claude_candidate("free", Some(43)),
        ];
        let remote = RecallRemoteMarks {
            offer: ["controlled", "free"]
                .into_iter()
                .map(String::from)
                .collect(),
            controlled: ["controlled", "controlled-history"]
                .into_iter()
                .map(String::from)
                .collect(),
        };
        let card = card_recall_with_remote(
            crate::recall::RecallProvider::Claude,
            &items,
            1,
            3,
            1,
            10,
            Some(CardRecallOrigin {
                chat_id: "oc_g",
                chat_type: "group",
            }),
            &remote,
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        for sid in ["controlled", "controlled-history"] {
            assert!(
                callbacks.iter().all(|callback| callback["sid"] != sid),
                "正在遥控的 {sid} 不能有接管、恢复或遥控回调"
            );
        }
        assert!(callbacks
            .iter()
            .any(|callback| callback["action"] == "recall_resume" && callback["sid"] == "free"));
        assert!(callbacks.iter().any(
            |callback| callback["action"] == "recall_cmux_remote" && callback["sid"] == "free"
        ));
        let rows: Vec<&serde_json::Value> = value["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|element| element.to_string().contains("遥控中"))
            .collect();
        assert_eq!(rows.len(), 2, "两条遥控中的会话各一个禁用按钮");
        for row in rows {
            let button = &row["columns"][1]["elements"][0];
            assert_eq!(button["text"]["content"], "遥控中");
            assert_eq!(button["disabled"], true);
        }
    }

    #[test]
    fn recall_card_pagination_respects_boundaries() {
        use crate::recall::SessionCandidate;
        let one = vec![SessionCandidate {
            provider: crate::recall::RecallProvider::Claude,
            ownership: crate::recall::RecallOwnership::External,
            sid: "s".into(),
            cwd: "/x".into(),
            name: "n".into(),
            prompt_preview: "task".into(),
            last_active_ms: 1,
            live_pid: None,
            live_status: None,
            source_observation_id: None,
            background_tasks: Vec::new(),
        }];
        let pages_of = |card: &str| -> Vec<u64> {
            let v: serde_json::Value = serde_json::from_str(card).unwrap();
            let mut cbs = Vec::new();
            collect_callback_values(&v, &mut cbs);
            cbs.iter()
                .filter(|c| c["action"] == "recall_page")
                .filter_map(|c| c["page"].as_u64())
                .collect()
        };

        // 单页:无翻页按钮。
        assert!(
            pages_of(&card_recall(
                crate::recall::RecallProvider::Claude,
                &one,
                1,
                1,
                1,
                10,
                None,
            ))
            .is_empty(),
            "单页不应有翻页"
        );
        // 首页(多页):只给下一页。
        assert_eq!(
            pages_of(&card_recall(
                crate::recall::RecallProvider::Claude,
                &one,
                1,
                30,
                3,
                10,
                None,
            )),
            vec![2],
            "首页只给下一页"
        );
        // 末页:只给上一页。
        assert_eq!(
            pages_of(&card_recall(
                crate::recall::RecallProvider::Claude,
                &one,
                3,
                30,
                3,
                10,
                None,
            )),
            vec![2],
            "末页只给上一页"
        );
        // 页码只在紧凑标题显示，不再为 footer 额外占一行。
        assert!(
            card_recall(
                crate::recall::RecallProvider::Claude,
                &one,
                2,
                30,
                3,
                10,
                None,
            )
            .contains("第 2/3 页"),
            "标题应显示当前页/总页"
        );
    }

    #[test]
    fn recall_card_ten_maximum_previews_stay_within_total_budget() {
        use crate::recall::SessionCandidate;
        let preview =
            "任".repeat(crate::claude_transcript::PromptPreviewLimits::default().max_preview_chars);
        let items = (0..10)
            .map(|index| SessionCandidate {
                provider: crate::recall::RecallProvider::Claude,
                ownership: crate::recall::RecallOwnership::External,
                sid: format!("sid-{index}"),
                cwd: "/Users/me/project".into(),
                name: format!("session-{index}"),
                prompt_preview: preview.clone(),
                last_active_ms: 1,
                live_pid: None,
                live_status: None,
                source_observation_id: None,
                background_tasks: Vec::new(),
            })
            .collect::<Vec<_>>();
        let card = card_recall(
            crate::recall::RecallProvider::Claude,
            &items,
            1,
            10,
            1,
            10,
            None,
        );
        assert!(
            card.len() <= CARD_TOTAL_SAFE_LIMIT,
            "10 条极限预览仍须落在飞书整卡安全预算内: {} > {}",
            card.len(),
            CARD_TOTAL_SAFE_LIMIT
        );
    }

    #[test]
    fn recall_card_escapes_external_text_in_folded_title_and_expanded_detail() {
        use crate::recall::SessionCandidate;
        let one = vec![SessionCandidate {
            provider: crate::recall::RecallProvider::Claude,
            ownership: crate::recall::RecallOwnership::External,
            sid: "sid-safe".into(),
            cwd: "/x/<project>".into(),
            name: "会话 <alias>".into(),
            prompt_preview: "<View>修复布局</View> & 验证".into(),
            last_active_ms: 1,
            live_pid: None,
            live_status: None,
            source_observation_id: None,
            background_tasks: Vec::new(),
        }];
        let card = card_recall(
            crate::recall::RecallProvider::Claude,
            &one,
            1,
            1,
            1,
            10,
            None,
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let panel = &value["body"]["elements"][1]["columns"][0]["elements"][0];
        let folded = panel["header"]["title"]["content"].as_str().unwrap();
        let detail = panel["elements"][0]["content"].as_str().unwrap();
        for rendered in [folded, detail] {
            assert!(rendered.contains("<\u{200B}View>修复布局<\u{200B}/View> & 验证"));
            assert!(
                !rendered.contains("<View>"),
                "用户标签不得进入 lark_md: {rendered}"
            );
        }
        assert!(detail.contains("<\u{200B}project>"));
        assert!(detail.contains("会话 <\u{200B}alias>"));
    }

    #[test]
    fn recall_card_embeds_origin_group_into_every_callback_value() {
        use crate::recall::SessionCandidate;
        let one = vec![SessionCandidate {
            provider: crate::recall::RecallProvider::Claude,
            ownership: crate::recall::RecallOwnership::External,
            sid: "sid-hist".into(),
            cwd: "/x".into(),
            name: "n".into(),
            prompt_preview: "task".into(),
            last_active_ms: 1,
            live_pid: None,
            live_status: None,
            source_observation_id: None,
            background_tasks: Vec::new(),
        }];
        let origin = CardRecallOrigin {
            chat_id: "oc_group_g",
            chat_type: "group",
        };
        // 多页,确保 recall_resume 与 recall_page 两类按钮都出现。
        let card = card_recall(
            crate::recall::RecallProvider::Claude,
            &one,
            1,
            30,
            3,
            10,
            Some(origin),
        );
        let v: serde_json::Value = super::tests::workflow_card(&card).unwrap();
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let mut resume = 0;
        let mut page = 0;
        for c in &cbs {
            let obj = c.as_object().unwrap();
            match c["action"].as_str() {
                Some("recall_resume") => {
                    resume += 1;
                    assert_eq!(obj["chat_id"], "oc_group_g", "恢复按钮须带原群 chat_id");
                    assert_eq!(obj["chat_type"], "group", "恢复按钮须带原群 chat_type");
                    assert_eq!(obj["sid"], "sid-hist");
                }
                Some("recall_page") => {
                    page += 1;
                    assert_eq!(
                        obj["chat_id"], "oc_group_g",
                        "翻页按钮须保留原群,防翻页后丢群"
                    );
                    assert_eq!(obj["chat_type"], "group");
                }
                other => panic!("未预期的 action: {other:?}"),
            }
        }
        assert_eq!(resume, 1, "一个历史候选一个恢复按钮");
        assert!(page >= 1, "多页应有翻页按钮且带 origin");
        // 无 origin 时保持旧形状(只 action+sid),向后兼容旧在途卡片。
        let plain = card_recall(
            crate::recall::RecallProvider::Claude,
            &one,
            1,
            1,
            1,
            10,
            None,
        );
        let pv: serde_json::Value = super::tests::workflow_card(&plain).unwrap();
        let mut pcbs = Vec::new();
        collect_callback_values(&pv, &mut pcbs);
        let resume_cb = pcbs
            .iter()
            .find(|c| c["action"] == "recall_resume")
            .unwrap();
        assert_eq!(
            resume_cb.as_object().unwrap().len(),
            3,
            "无 origin 仍只含 action+cli+sid"
        );
    }

    fn callback_values(card: &serde_json::Value) -> Vec<&serde_json::Value> {
        fn collect<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("type").and_then(|value| value.as_str()) == Some("callback") {
                        out.push(map.get("value").expect("callback 必须有 value"));
                    }
                    for child in map.values() {
                        collect(child, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for child in items {
                        collect(child, out);
                    }
                }
                _ => {}
            }
        }

        let mut values = Vec::new();
        collect(card, &mut values);
        values
    }

    #[test]
    fn bridge_control_card_keeps_strict_three_key_confirmed_callbacks() {
        let card: serde_json::Value = serde_json::from_str(&card_bridge_mirror_control(
            "ticket-opaque",
            "choice-opaque",
            "coder · 群#12345678",
        ))
        .unwrap();
        let values = callback_values(&card)
            .into_iter()
            .filter(|value| value["action"] != "card_close")
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 2);
        let actions = values
            .iter()
            .map(|value| value["action"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            actions,
            std::collections::BTreeSet::from(["bridge_context", "bridge_stop"])
        );
        for value in values {
            let object = value.as_object().unwrap();
            assert_eq!(
                object
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                std::collections::BTreeSet::from(["action", "choice", "ticket"])
            );
            assert_eq!(object["ticket"], "ticket-opaque");
            assert_eq!(object["choice"], "choice-opaque");
        }

        let json = card.to_string();
        assert_eq!(
            json.matches("\"confirm\"").count(),
            2,
            "两个动作都必须二次确认"
        );
    }

    #[test]
    fn bridge_control_target_label_is_distinct_and_does_not_expose_raw_chat_id() {
        let first = bridge_target_label("coder", "oc_secret_alpha");
        let second = bridge_target_label("coder", "oc_secret_beta");
        assert_ne!(first, second);
        assert!(!first.contains("oc_secret_alpha"));
        let card = card_bridge_mirror_control("ticket", "choice", &first);
        let audit = card_bridge_activation_audit(&first);
        assert!(card.contains("coder"));
        assert!(card.contains("群#"));
        assert!(audit.contains(&first));
        assert!(!card.contains("oc_secret_alpha"));
        assert!(!audit.contains("oc_secret_alpha"));
    }

    #[test]
    fn external_bridge_turn_is_one_card_user_then_claude_without_mention_rendering() {
        let card = card_external_bridge_turn(
            "先问 <at user_id=\"ou_bad\">所有人</at> & Vec<T>",
            "再答 <unsafe> & done",
        );
        let parsed: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert_eq!(parsed["schema"], "2.0");
        let user = card.find("**用户**").unwrap();
        let claude = card.find("**Claude**").unwrap();
        assert!(user < claude, "单卡必须固定用户在 Claude 之前");
        assert!(
            card.contains("<\u{200B}at user_id="),
            "外部 mention 必须被拆散成字面形态"
        );
        assert!(
            !card.contains("<at user_id="),
            "external body 绝不能进入 mention renderer"
        );
        assert!(!card.contains("&amp;"), "不得出现飞书不解码的字面实体");
        assert!(card.len() <= CARD_TOTAL_SAFE_LIMIT);
    }

    #[test]
    fn external_bridge_turn_clips_utf8_after_escape_and_stays_under_total_budget() {
        let user = "<&\"汉\n".repeat(10_000);
        let assistant = "&<Claude>界\n".repeat(20_000);
        let card = card_external_bridge_turn(&user, &assistant);
        let _: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert!(card.len() <= CARD_TOTAL_SAFE_LIMIT, "实际 JSON 必须 ≤25KB");
        assert!(card.contains("内容过长，已截断"));
        assert!(card.contains("**用户**"));
        assert!(card.contains("**Claude**"));
    }

    #[test]
    fn bridge_activation_audit_is_fixed_and_contains_no_external_body_slot() {
        let card = card_bridge_activation_audit("coder · 群#12345678");
        let parsed: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert_eq!(parsed["schema"], "2.0");
        assert!(card.contains("镜像激活审计"));
        assert_eq!(
            callback_values(&parsed),
            vec![&serde_json::json!({"action": "card_close"})]
        );
        for body_key in ["user_text", "assistant_text", "transcript", "prompt"] {
            assert!(!card.contains(body_key));
        }
    }

    #[test]
    fn bridge_context_capsule_keeps_latest_twelve_visible_messages_within_budget() {
        use crate::claude_transcript::{CapsuleMessage, CapsuleRole, ContextCapsule};

        let messages = (0..14)
            .map(|index| CapsuleMessage {
                role: if index % 2 == 0 {
                    CapsuleRole::User
                } else {
                    CapsuleRole::Assistant
                },
                text: format!(
                    "context-{index} <at user_id=\"bad\">x</at> {}",
                    "&".repeat(2_000)
                ),
            })
            .collect();
        let card = card_bridge_context_capsule(&ContextCapsule {
            messages,
            truncated: true,
            skipped_lines: 2,
        });
        let _: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert!(card.len() <= CARD_TOTAL_SAFE_LIMIT);
        assert!(!card.contains("context-0 "), "只保留末尾最多 12 条");
        assert!(!card.contains("context-1 "), "只保留末尾最多 12 条");
        assert!(card.contains("context-13"), "必须保留最新可见消息");
        assert!(!card.contains("<at user_id="), "胶囊正文不得 mention 渲染");
        assert!(card.contains("<\u{200B}at user_id="));
    }

    #[test]
    fn bridge_context_capsule_clips_single_worst_case_message_under_total_budget() {
        use crate::claude_transcript::{CapsuleMessage, CapsuleRole, ContextCapsule};

        let card = card_bridge_context_capsule(&ContextCapsule {
            messages: vec![CapsuleMessage {
                role: CapsuleRole::Assistant,
                text: "<&\"界\n".repeat(200_000),
            }],
            truncated: false,
            skipped_lines: 0,
        });
        let _: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert!(card.len() <= CARD_TOTAL_SAFE_LIMIT);
        assert!(card.contains("**Claude**"));
        assert!(card.contains("内容过长，已截断"));
        assert!(!card.contains("<at user_id="));
    }

    #[test]
    fn unknown_card_action_is_fail_closed_operate() {
        // C12 红线:未登记/未来新增的卡片动作必须 fail-closed —— 默认按 operate 档鉴权,
        // 而非静默放行(旧实现 `_ => None` 让群里任意人可触发未登记的破坏性动作)。
        use CardGate::*;
        for a in ["purge_memory", "restart_cli", "", "totally_new_action"] {
            assert_eq!(
                card_gate_tier(a),
                Operate,
                "未登记动作 {a:?} 必须 fail-closed 走 operate"
            );
        }
    }

    #[test]
    fn split_for_card_preserves_text_and_respects_limit() {
        // 短文本:单段原样。
        assert_eq!(split_for_card("你好", 100), vec!["你好".to_string()]);
        // 多行长文本:按行边界分段,各段 ≤ 上限,拼回与原文逐字节相等。
        let text = (0..200)
            .map(|i| format!("第 {i} 行:一些中文内容混 ascii mixed content\n"))
            .collect::<String>();
        let segs = split_for_card(&text, 1000);
        assert!(segs.len() > 1, "超限必须分段");
        assert!(segs.iter().all(|s| s.len() <= 1000), "各段不得超上限");
        assert_eq!(segs.concat(), text, "分段不丢不改一字");
        // 单行超限(无换行):按 char 边界硬切,仍无损。
        let one_line = "汉".repeat(900); // 2700 字节无换行
        let segs = split_for_card(&one_line, 1000);
        assert!(segs.len() >= 3);
        assert!(segs.iter().all(|s| s.len() <= 1000));
        assert_eq!(segs.concat(), one_line);
    }

    fn split_with_real_feishu_budget(
        body: &str,
        peers: &[crate::peers::Peer],
        alias: &std::collections::HashMap<String, String>,
    ) -> Vec<String> {
        split_for_card_by_budget(body, |segment| {
            let rendered = render_external_lark_md(segment, peers, alias);
            rendered_final_reply_fits(&rendered)
        })
    }

    fn assert_real_feishu_budget(
        body: &str,
        segments: &[String],
        peers: &[crate::peers::Peer],
        alias: &std::collections::HashMap<String, String>,
    ) {
        assert_eq!(segments.concat(), body, "分段必须逐字节还原原文");
        assert!(segments.iter().all(|segment| {
            let rendered = render_external_lark_md(segment, peers, alias);
            rendered_final_reply_fits(&rendered)
        }));
    }

    #[test]
    fn rendered_budget_split_handles_five_thousand_tag_pairs() {
        // 消毒膨胀源换了:`<a` 对插零宽空格,10KB 原文渲染成 25KB,预算必须按渲染后算。
        let body = "<a".repeat(5_000);
        let segments = split_with_real_feishu_budget(&body, &[], &std::collections::HashMap::new());

        assert!(segments.len() > 1, "消毒后为 25KB，不能误判成单卡");
        assert_real_feishu_budget(&body, &segments, &[], &std::collections::HashMap::new());
    }

    #[test]
    fn rendered_budget_no_longer_inflates_ampersands() {
        // `&` 不再转成 `&amp;`(飞书不解码实体):5KB 的 `&` 渲染后仍是 5KB,单卡就装得下。
        // 若这条开始拆卡,说明有人把 `&` 转义加了回来——那会满屏字面 `&amp;`。
        let body = "&".repeat(5_000);
        let segments = split_with_real_feishu_budget(&body, &[], &std::collections::HashMap::new());

        assert_eq!(segments.len(), 1, "`&` 不膨胀,不该拆卡");
        assert_real_feishu_budget(&body, &segments, &[], &std::collections::HashMap::new());
    }

    #[test]
    fn rendered_budget_split_handles_five_thousand_angle_brackets() {
        let body = "<".repeat(5_000);
        let segments = split_with_real_feishu_budget(&body, &[], &std::collections::HashMap::new());

        assert_real_feishu_budget(&body, &segments, &[], &std::collections::HashMap::new());
    }

    #[test]
    fn rendered_budget_split_preserves_five_thousand_mentions() {
        let peers = vec![crate::peers::Peer {
            open_id: "ou_bot_with_a_long_open_id".into(),
            name: "星光".into(),
            ..Default::default()
        }];
        let alias = std::collections::HashMap::from([("bot".to_string(), "星光".to_string())]);
        let body = "@bot ".repeat(5_000);
        let segments = split_with_real_feishu_budget(&body, &peers, &alias);

        assert!(segments.len() > 1, "mention 展开后必须按真实预算拆卡");
        assert_real_feishu_budget(&body, &segments, &peers, &alias);
        let rendered_mentions = segments
            .iter()
            .map(|segment| render_external_lark_md(segment, &peers, &alias))
            .map(|rendered| rendered.as_str().matches("<at ").count())
            .sum::<usize>();
        assert_eq!(rendered_mentions, 5_000, "切点不能破坏 mention 语义");
    }

    #[test]
    fn rendered_budget_split_keeps_one_empty_segment() {
        assert_eq!(split_for_card_by_budget("", |_| true), vec![String::new()]);
    }

    #[test]
    fn clip_for_stream_only_when_over_limit_and_char_safe() {
        assert_eq!(clip_for_stream("短", 100), None);
        let long = "汉".repeat(50); // 150 字节
        let clipped = clip_for_stream(&long, 100).expect("超限应截断");
        assert!(clipped.contains("回复过长"));
        // 截断点落在 char 边界(不 panic 即证),且头部是原文前缀。
        assert!(long.starts_with(clipped.split('\n').next().unwrap()));
    }

    #[test]
    fn interactive_select_card_has_option_buttons_with_ordinals() {
        let c = card_interactive_select(
            "Change effort level?",
            &[
                (1, "Yes, switch".to_string()),
                (2, "No, go back".to_string()),
            ],
        );
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let choices: Vec<i64> = cbs
            .iter()
            .filter(|x| x["action"] == "tui_choice")
            .filter_map(|x| x["ordinal"].as_i64())
            .collect();
        assert_eq!(choices, vec![0, 1], "两个选项各带 0-based ordinal");
        assert!(c.contains("1. Yes, switch") && c.contains("2. No, go back"));
        // 按钮 value 应含 title + options 快照(供 handle_tui_inject 重建终态卡)。
        let titles: Vec<&str> = cbs
            .iter()
            .filter(|x| x["action"] == "tui_choice")
            .filter_map(|x| x["title"].as_str())
            .collect();
        assert_eq!(
            titles,
            vec!["Change effort level?", "Change effort level?"],
            "每个按钮 value 都带 title 快照"
        );
        let opts0 = &cbs[0]["options"];
        assert!(
            opts0.is_array() && opts0.as_array().unwrap().len() == 2,
            "按钮 value 带 options (编号,文字) 快照"
        );
    }

    #[test]
    fn interactive_select_result_has_disabled_buttons_and_confirmation() {
        let c = card_interactive_select_result(
            "Change effort level?",
            &[
                (1, "Yes, switch".to_string()),
                (2, "No, go back".to_string()),
            ],
            0, // 选了第 1 项(ordinal=0)
        );
        let v: serde_json::Value = super::tests::workflow_card(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        // 所有按钮 behaviors 应为空(置灰、不可再点)。
        let elements = v["body"]["elements"].as_array().unwrap();
        let mut found_btns = 0;
        let mut found_selected = false;
        let mut found_confirm = false;
        for el in elements {
            if el["tag"] == "column_set" {
                for col in el["columns"].as_array().unwrap() {
                    for e in col["elements"].as_array().unwrap() {
                        if e["tag"] == "button" {
                            found_btns += 1;
                            let beh = e["behaviors"].as_array().unwrap();
                            assert!(beh.is_empty(), "按钮应置灰(behaviors=[])");
                            assert!(
                                e["disabled"].as_bool() == Some(true),
                                "按钮应 disabled=true 置灰不可点击"
                            );
                            let text = e["text"]["content"].as_str().unwrap_or("");
                            if text.starts_with("✅") {
                                found_selected = true;
                                assert_eq!(
                                    e["type"].as_str(),
                                    Some("primary"),
                                    "被选按钮应标 primary"
                                );
                            }
                        }
                    }
                }
            }
            if el["tag"] == "markdown" {
                let content = el["content"].as_str().unwrap_or("");
                if content.contains("已选第 1 项") {
                    found_confirm = true;
                }
            }
        }
        assert_eq!(found_btns, 2, "应有 2 个置灰按钮");
        assert!(found_selected, "被选按钮应带 ✅ 前缀");
        assert!(found_confirm, "底部应有确认行");
    }

    #[test]
    fn interactive_raw_card_has_text_form_and_esc() {
        let c = card_interactive_raw("Change effort level?\n❯ 1. Yes\n  2. No", 15, false);
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let actions: Vec<&str> = cbs.iter().filter_map(|x| x["action"].as_str()).collect();
        assert!(
            actions.contains(&"tui_text"),
            "应有文本提交动作: {actions:?}"
        );
        assert!(actions.contains(&"tui_esc"), "应有取消动作: {actions:?}");
        assert!(c.contains("❯ 1. Yes"), "应贴原始屏幕");
    }

    /// 对话框原文进代码块:终端里的 `*` `|` 原样示人,标签形态拆开,原文自带的 ``` 不闭合代码块;
    /// 点击后的终态卡与原卡是同一段原文。
    #[test]
    fn interactive_select_card_puts_dialog_text_in_a_code_block() {
        let title = "Bash command\n\n  │ rm -f $LOGS/*.bin | head\n  │ echo ```<at user_id=\"all\">```\n\nDo you want to proceed?";
        let options = [(1, "Yes".to_string()), (2, "No".to_string())];
        let c = card_interactive_select(title, &options);
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        let body = v["body"]["elements"][1]["content"].as_str().unwrap();
        assert!(
            body.starts_with("```\n") && body.ends_with("\n```"),
            "{body}"
        );
        assert_eq!(
            body.matches("```").count(),
            2,
            "原文里的 ``` 要拆开: {body}"
        );
        assert!(body.contains("│ rm -f $LOGS/*.bin | head"), "{body}");
        assert!(!body.contains("<at"), "标签形态要拆开: {body}");
        let result = card_interactive_select_result(title, &options, 0);
        let rv: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(rv["body"]["elements"][1]["content"].as_str(), Some(body));
    }

    /// 长命令的对话框原文留头留尾,整卡落进飞书的体积预算(原文随每个按钮复制一份)。
    #[test]
    fn interactive_select_card_elides_long_dialog_text_within_budget() {
        let mut lines = vec!["Bash command".to_string(), String::new()];
        lines.extend((0..400).map(|i| format!("  │ step {i:03} {}", "x".repeat(100))));
        lines.extend([
            String::new(),
            "Dangerous rm operation on possibly-empty variable path".to_string(),
            String::new(),
            "Do you want to proceed?".to_string(),
        ]);
        let title = lines.join("\n");
        let options: Vec<(usize, String)> = (1..=4).map(|n| (n, format!("option {n}"))).collect();
        let c = card_interactive_select(&title, &options);
        assert!(c.len() <= CARD_TOTAL_SAFE_LIMIT, "{} bytes", c.len());
        let v: serde_json::Value = super::tests::workflow_card(&c).unwrap();
        let body = v["body"]["elements"][1]["content"].as_str().unwrap();
        assert!(
            body.contains("Bash command") && body.contains("step 000"),
            "{body}"
        );
        assert!(
            body.contains("step 399") && body.contains("Do you want to proceed?"),
            "{body}"
        );
        assert!(body.contains("中间省略"), "{body}");
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        assert_eq!(cbs.len(), 4);
        for value in &cbs {
            let shown = menu_title_element(value["title"].as_str().unwrap());
            assert_eq!(
                shown["content"].as_str(),
                Some(body),
                "按钮快照与卡上原文一致"
            );
        }
    }

    /// 每个按钮都带发卡时那一张菜单的签名,按完整问句算:卡上省略了中间行也不影响。
    #[test]
    fn interactive_select_card_binds_every_button_to_the_menu_signature() {
        let title = (0..400)
            .map(|i| format!("line {i} {}", "x".repeat(60)))
            .collect::<Vec<_>>()
            .join("\n");
        let options = [(1, "Yes".to_string()), (2, "No".to_string())];
        let expected = crate::interactive::signature_token(crate::interactive::menu_signature(
            &title,
            options.iter().map(|(i, l)| (*i, l.as_str())),
        ));
        let v: serde_json::Value =
            super::tests::workflow_card(&card_interactive_select(&title, &options)).unwrap();
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        assert_eq!(cbs.len(), 2);
        for value in &cbs {
            assert_eq!(value["signature"].as_str(), Some(expected.as_str()));
        }
        let body = v["body"]["elements"][1]["content"].as_str().unwrap();
        assert!(body.contains("中间省略"), "{body}");
    }

    /// 缺了开头的对话框卡不给选项按钮:只看得到结尾,证明不了点下去批准的是哪一条。
    #[test]
    fn interactive_select_card_with_cut_dialog_has_no_choice_controls() {
        let cut = crate::interactive::parse_interactive(
            "   │ tail of a long command\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel",
        )
        .unwrap();
        assert!(crate::interactive::title_is_cut(&cut.title));
        let options = [(1, "Yes".to_string()), (2, "No".to_string())];
        let v: serde_json::Value =
            super::tests::workflow_card(&card_interactive_select(&cut.title, &options)).unwrap();
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        assert!(cbs.is_empty(), "{cbs:?}");
        assert!(v.to_string().contains("不能直接选"), "{v}");
    }

    #[test]
    fn elide_middle_lines_stays_within_budget_on_char_boundaries() {
        let one_line = "汉".repeat(1000);
        assert!(elide_middle_lines(&one_line, 100).len() <= 100);
        let many = (0..50)
            .map(|i| format!("第{i}行"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = elide_middle_lines(&many, 120);
        assert!(out.len() <= 120, "{out}");
        assert!(out.starts_with("第0行") && out.ends_with("第49行"), "{out}");
        assert_eq!(elide_middle_lines("短", 120), "短");
    }

    #[test]
    fn closed_card_keeps_panel_disabled_and_appends_footer() {
        let models = vec!["m-a".to_string(), "m-b".to_string()];
        let reset = card_session_closed(
            "g-x",
            "cursor",
            "RPC · ACP 常驻",
            &rm(Some("m-a"), ModelSource::AgentConfig),
            &reff(None, EffortSource::ModelInherent),
            &models,
            &[],
            false,
            true,
        );
        let clean = card_session_closed(
            "g-x",
            "cursor",
            "RPC · ACP 常驻",
            &rm(Some("m-a"), ModelSource::AgentConfig),
            &reff(None, EffortSource::ModelInherent),
            &models,
            &[],
            false,
            false,
        );
        assert!(reset.contains("新建"));
        assert!(clean.contains("续聊"));
        assert!(feed_preview_of(&reset).starts_with("已归档|"));
        assert!(feed_preview_of(&clean).starts_with("已退出|"));
        for c in [&reset, &clean] {
            let v: serde_json::Value = super::tests::workflow_card(c).unwrap();
            // schema 2.0:必须与会话面板同 schema,否则飞书 PATCH 报 v2→v1 不可换。
            assert_eq!(v["schema"], "2.0");
            // 保留面板原貌:仍有按钮,但每个交互控件都 disabled=true(A 后面板已无手打输入框)。
            let mut saw_button = false;
            walk(&v, &mut |node| {
                if matches!(node.get("tag"), Some(serde_json::Value::String(t)) if t == "button" || t == "input")
                {
                    saw_button = true;
                    assert_eq!(node["disabled"], serde_json::Value::Bool(true));
                }
            });
            assert!(saw_button, "终态卡片应保留(置灰的)面板控件");
            // A(隐私):终态卡也绝不带工作目录路径。
            assert!(!c.contains("\"tag\":\"form\""), "终态卡不该再有手打表单");
            // 底部:分隔线 + 「你已交互」footer。
            let els = v["body"]["elements"].as_array().unwrap();
            assert_eq!(els[els.len() - 2]["tag"], "hr");
            assert!(els.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("你已交互"));
        }
    }

    #[test]
    fn idle_closed_keeps_idle_layout_disabled_with_footer() {
        let s = card_idle_closed("g-abc", 86400, IdleClosedReason::Reset);
        let v: serde_json::Value = super::tests::workflow_card(&s).unwrap();
        // schema 2.0:与原空闲卡同 schema,飞书 PATCH 不可换。
        assert_eq!(v["schema"], "2.0");
        // 保留空闲布局:生命周期按钮俱在且全部置灰。
        let mut saw_button = false;
        walk(&v, &mut |node| {
            if matches!(node.get("tag"), Some(serde_json::Value::String(t)) if t == "button") {
                saw_button = true;
                assert_eq!(node["disabled"], serde_json::Value::Bool(true));
            }
        });
        assert!(saw_button, "空闲终态应保留(置灰的)空闲按钮");
        // 绝不带 /session 面板专属内容(这正是 bug:空闲卡不该漂成面板)。
        assert!(!s.contains("切换模型"));
        assert!(!s.contains("切换 effort"));
        assert!(!s.contains("改工作目录"));
        // 保留空闲标题 + 底部 hr + 「你已交互」footer。
        assert!(s.contains("会话空闲提醒"));
        let els = v["body"]["elements"].as_array().unwrap();
        assert_eq!(els[els.len() - 2]["tag"], "hr");
        assert!(els.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("你已交互"));
        assert!(s.contains("新建会话"));
    }

    #[test]
    fn idle_prompt_shows_latest_context_when_available() {
        let context = crate::context_budget::ContextBudget::from_tokens(350_000, Some(1_000_000));
        let card = card_idle_prompt_with_context("g-x", 3600, Some(&context));
        assert!(card.contains("当前 ctx"));
        assert!(card.contains("350k / 1.0M"));
        assert!(card.contains("35%"));
        assert!(card.contains(r#""ctx_used_tokens":350000"#));
        assert!(card.contains(r#""ctx_window_tokens":1000000"#));
        assert!(!card_idle_prompt("g-x", 3600).contains("当前 ctx"));
    }

    #[test]
    fn idle_closed_footer_varies_by_reason() {
        assert!(card_idle_closed("g", 1, IdleClosedReason::KeepAlive).contains("已保活"));
        assert!(card_idle_closed("g", 1, IdleClosedReason::Compact).contains("已压缩"));
        assert!(card_idle_closed("g", 1, IdleClosedReason::Clean).contains("续聊"));
        assert!(card_idle_closed("g", 1, IdleClosedReason::Reset).contains("新建会话"));
    }

    #[test]
    fn idle_close_retains_its_origin_in_active_and_frozen_cards() {
        for wire in [
            card_idle_prompt("g-room", 3600),
            card_idle_closed("g-room", 3600, IdleClosedReason::Compact),
        ] {
            let card: serde_json::Value = serde_json::from_str(&wire).unwrap();
            let mut callbacks = Vec::new();
            collect_callback_values(&card, &mut callbacks);
            let closes: Vec<_> = callbacks
                .iter()
                .filter(|v| v["action"] == "card_close")
                .collect();
            assert_eq!(closes.len(), 1);
            assert_eq!(closes[0]["origin"], "idle");
            assert_eq!(closes[0]["chat"], "g-room");
            walk(&card, &mut |node| {
                if node["tag"] == "button" && node["text"]["content"] == "关闭卡片" {
                    assert_ne!(node["disabled"], true);
                }
            });
            assert!(!callbacks.iter().any(|v| v["action"] == "keepalive"));
        }
    }

    #[test]
    fn idle_prompt_buttons_carry_origin_idle_for_finalize_routing() {
        let s = card_idle_prompt("g-x", 3600);
        // 收尾路径据 origin/idle_secs 复现空闲布局(区别于无 origin 的 /session 面板)。
        assert!(s.contains("\"origin\":\"idle\""));
        assert!(s.contains("\"idle_secs\":3600"));
    }

    #[test]
    fn turn_summary_card_carries_reply() {
        let s = card_turn_summary("最终答案内容", None, None);
        assert!(s.contains("最终答案内容"));
    }

    #[test]
    fn turn_summary_feed_preview_uses_reply_instead_of_badge() {
        let s = card_turn_summary(
            "最终答案内容",
            Some("14:30"),
            Some("codex · gpt-5.6-sol · ctx 12.3k"),
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["config"]["summary"]["content"], "最终答案内容");
    }

    #[test]
    fn turn_summary_feed_preview_is_compact_and_unicode_safe() {
        let reply = format!("第一行\n\n{}", "界".repeat(FEED_SUMMARY_MAX_CHARS));
        let s = card_turn_summary(&reply, None, Some("codex · gpt-5.6-sol"));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let summary = v["config"]["summary"]["content"]
            .as_str()
            .expect("summary content");
        assert!(summary.starts_with("第一行 界"));
        assert_eq!(summary.chars().count(), FEED_SUMMARY_MAX_CHARS);
        assert!(!summary.contains("codex"));
    }

    #[test]
    fn turn_summary_feed_preview_describes_images_with_empty_alt() {
        let pieces = [crate::image::ReplyPiece::Image {
            key: "img-key".into(),
            alt: String::new(),
        }];
        let s = card_turn_summary_pieces(&pieces, None, Some("codex · gpt-5.6-sol"));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["config"]["summary"]["content"], "图片");
    }

    #[test]
    fn turn_summary_feed_preview_combines_image_alt_and_text() {
        let pieces = [
            crate::image::ReplyPiece::Image {
                key: "img-key".into(),
                alt: "架构图".into(),
            },
            crate::image::ReplyPiece::Text(" 验证通过".into()),
        ];
        let s = card_turn_summary_pieces(&pieces, None, Some("codex · gpt-5.6-sol"));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["config"]["summary"]["content"], "架构图 验证通过");
    }

    #[test]
    fn turn_summary_feed_preview_never_falls_back_to_badge() {
        let s = card_turn_summary("   ", None, Some("codex · gpt-5.6-sol"));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["config"]["summary"]["content"], "回复");
    }

    /// 正文必须打头，时刻下沉到末尾，卡片内部只承载有信息量的内容。
    #[test]
    fn turn_summary_leads_with_body_and_sinks_timestamp_to_the_end() {
        let s = card_turn_summary("最终答案", Some("14:30"), None);
        assert!(
            !s.contains("总结"),
            "不得再有固定标题,否则群列表预览恒为同一句: {s}"
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let els = v["body"]["elements"].as_array().expect("elements");
        let first = els[0]["content"].as_str().unwrap_or_default();
        assert!(first.contains("最终答案"), "首个元素必须是正文: {first}");

        assert_eq!(els.len(), 3, "正文 + hr + 时刻: {s}");
        assert_eq!(els[1]["tag"], "hr", "时刻前要有分隔线");
        let last = els[2]["content"].as_str().unwrap_or_default();
        assert!(last.contains("14:30"), "{last}");
        assert!(
            !last.contains("关于：") && !last.contains("回复于"),
            "{last}"
        );
        assert!(last.contains("color='grey'"), "时刻应灰字显示");
        assert_eq!(els[2]["text_size"], "notation");
    }

    /// 无上下文行时不留裸分隔线(否则卡片末尾挂一条没有内容的线)。
    #[test]
    fn turn_summary_without_context_has_no_dangling_divider() {
        let s = card_turn_summary("最终答案", None, None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let els = v["body"]["elements"].as_array().expect("elements");
        assert_eq!(els.len(), 1, "只该有正文一个元素: {s}");
        let s2 = card_turn_summary("最终答案", Some("   "), Some("   "));
        let v2: serde_json::Value = serde_json::from_str(&s2).unwrap();
        assert_eq!(
            v2["body"]["elements"].as_array().unwrap().len(),
            1,
            "空白上下文行 + 空白角标同样不渲染"
        );
    }

    /// 角标与时刻共享一个 notation 灰字元素，避免同一层信息占两行。
    #[test]
    fn turn_summary_badge_and_timestamp_share_one_notation_line() {
        let s = card_turn_summary(
            "最终答案",
            Some("14:30"),
            Some("codex · gpt-5.6-sol · ctx 12.3k"),
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let els = v["body"]["elements"].as_array().unwrap();
        assert_eq!(els.len(), 3, "正文+hr+角标及时刻: {s}");
        assert_eq!(els[1]["tag"], "hr");
        let badge = els[2].as_object().unwrap();
        assert!(badge["content"]
            .as_str()
            .unwrap()
            .contains("codex · gpt-5.6-sol · ctx 12.3k · 14:30"));
        assert!(!badge["content"].as_str().unwrap().contains("关于："));
        assert_eq!(badge["text_size"], "notation", "角标应用更小字号档");
        assert!(
            badge["content"].as_str().unwrap().contains("color='grey'"),
            "角标仍灰字"
        );
        // 只有角标时仍先来一条 hr,再角标(不贴正文)。
        let only_badge = card_turn_summary("答", None, Some("claude · opus"));
        let vb: serde_json::Value = serde_json::from_str(&only_badge).unwrap();
        let eb = vb["body"]["elements"].as_array().unwrap();
        assert_eq!(eb.len(), 3, "正文+hr+角标: {only_badge}");
        assert_eq!(eb[1]["tag"], "hr");
        assert_eq!(eb[2]["text_size"], "notation");
    }

    #[test]
    fn stream_card_embeds_minimal_interrupt_button() {
        // 任务4:打断按钮嵌进正在刷的流式卡(末尾,带分割线),不再单独发控制卡。
        // 红框(danger)+ 二次确认弹窗(confirm)防误点。
        let s = card_stream_with_interrupt("正在输出的内容……");
        assert!(s.contains("正在输出的内容"));
        assert!(s.contains("\"action\":\"interrupt\""));
        assert!(s.contains("\"confirm\""), "打断按钮应带二次确认弹窗");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["schema"], "2.0",
            "流式卡须 2.0(与带按钮的同 schema,PATCH 不换)"
        );
        let mut danger_btn = false;
        let mut saw_hr = false;
        walk(&v, &mut |node| {
            let tag = node.get("tag").and_then(|t| t.as_str());
            if tag == Some("hr") {
                saw_hr = true;
            }
            if tag == Some("button") && node.get("type").and_then(|t| t.as_str()) == Some("danger")
            {
                danger_btn = true;
            }
        });
        assert!(saw_hr && danger_btn, "应有分割线 + 红框(danger)打断按钮");
    }

    #[test]
    fn markdown_v2_is_schema_2_without_button() {
        // 流式终态(stream_finish)/无按钮回退用:2.0 markdown,无按钮(与流式卡同 schema)。
        let s = card_markdown_v2("终态文本");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["schema"], "2.0");
        assert_eq!(v["config"]["summary"]["content"], "终态文本");
        assert!(s.contains("终态文本"));
        assert!(!s.contains("\"tag\":\"button\""));
    }

    #[test]
    fn rendered_markdown_feed_summary_keeps_name_and_drops_open_id() {
        let peers = [crate::peers::Peer {
            open_id: "ou_internal".into(),
            name: "星光".into(),
            ..Default::default()
        }];
        let rendered =
            render_external_lark_md("请 @星光 复核", &peers, &std::collections::HashMap::new());
        let card: serde_json::Value =
            serde_json::from_str(&card_markdown_v2_rendered(&rendered)).unwrap();
        assert_eq!(card["config"]["summary"]["content"], "请 星光 复核");
        assert!(!card["config"]["summary"]["content"]
            .as_str()
            .unwrap()
            .contains("ou_internal"));
    }

    /// 递归遍历卡片 JSON 的每个 object 节点(测试辅助)。
    fn walk(v: &serde_json::Value, f: &mut impl FnMut(&serde_json::Value)) {
        match v {
            serde_json::Value::Object(m) => {
                f(v);
                m.values().for_each(|c| walk(c, f));
            }
            serde_json::Value::Array(a) => a.iter().for_each(|c| walk(c, f)),
            _ => {}
        }
    }

    #[test]
    fn card_is_valid_json_with_payload() {
        let c = card_content("星光", "你好**世界**");
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["header"]["title"]["content"], "星光");
        assert_eq!(v["elements"][0]["content"], "你好**世界**");
        assert_eq!(v["elements"][0]["tag"], "markdown");
    }

    #[test]
    fn idle_prompt_explains_lifecycle_controls() {
        let c = card_idle_prompt("pdeadbeef", 10_800);
        let v: serde_json::Value = super::tests::workflow_card(&c).unwrap();
        assert!(c.contains("已空闲 **3 小时**"));
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let labels: Vec<&str> = cbs.iter().filter_map(|a| a["action"].as_str()).collect();
        assert_eq!(labels, ["reset", "clean", "compact"]);
        assert!(!c.contains("保活"));
        assert!(!labels.contains(&"model"));
        assert!(!labels.contains(&"set_workdir"));
        // 每个按钮带 chat tag,供回调路由
        assert!(cbs.iter().all(|a| a["chat"] == "pdeadbeef"));
    }

    /// 递归收集 v2 卡片里所有 `behaviors[].value`(回调回传数据),方便断言。
    fn collect_callback_values(v: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(behs) = map.get("behaviors").and_then(|b| b.as_array()) {
                    for b in behs {
                        if b.get("type").and_then(|t| t.as_str()) == Some("callback") {
                            if let Some(val) = b.get("value") {
                                out.push(val.clone());
                            }
                        }
                    }
                }
                for child in map.values() {
                    collect_callback_values(child, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for child in arr {
                    collect_callback_values(child, out);
                }
            }
            _ => {}
        }
    }

    /// 递归收集指定 tag 的所有元素。
    fn collect_by_tag(v: &serde_json::Value, tag: &str, out: &mut Vec<serde_json::Value>) {
        match v {
            serde_json::Value::Object(map) => {
                if map.get("tag").and_then(|t| t.as_str()) == Some(tag) {
                    out.push(v.clone());
                }
                for child in map.values() {
                    collect_by_tag(child, tag, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for child in arr {
                    collect_by_tag(child, tag, out);
                }
            }
            _ => {}
        }
    }

    /// 递归收集所有**表单取值控件**的 `name`。按钮也有 name(提交键标识),不算取值控件,故排除。
    fn collect_input_names(v: &serde_json::Value, out: &mut Vec<String>) {
        const VALUE_TAGS: [&str; 5] = [
            "input",
            "picker_time",
            "date_picker",
            "multi_select_static",
            "select_static",
        ];
        match v {
            serde_json::Value::Object(map) => {
                if let (Some(tag), Some(name)) = (
                    map.get("tag").and_then(|t| t.as_str()),
                    map.get("name").and_then(|n| n.as_str()),
                ) {
                    if VALUE_TAGS.contains(&tag) {
                        out.push(name.to_string());
                    }
                }
                for child in map.values() {
                    collect_input_names(child, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for child in arr {
                    collect_input_names(child, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn crash_recovery_card_has_per_item_recover_and_dismiss() {
        let items = [("om_1", "ping"), ("om_2", "做个 X")];
        let c = card_crash_recovery("recovery-card-1", "pdead", None, &items, &[]);
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        // 每条消息一个 recover 按钮,带其 message_id。
        let recover: Vec<&serde_json::Value> =
            cbs.iter().filter(|x| x["action"] == "recover").collect();
        assert_eq!(recover.len(), 2);
        let mids: Vec<&str> = recover.iter().filter_map(|x| x["mid"].as_str()).collect();
        assert!(mids.contains(&"om_1") && mids.contains(&"om_2"));
        // 一个「全部忽略」。
        assert_eq!(
            cbs.iter()
                .filter(|x| x["action"] == "dismiss_recovery")
                .count(),
            1
        );
        // 所有回调带 chat tag 供路由。
        assert!(cbs
            .iter()
            .filter(|x| x["action"] != "card_close")
            .all(|x| x["chat"] == "pdead"));
        // 预览文本出现在卡片里。
        assert!(c.contains("ping") && c.contains("做个 X"));
    }

    #[test]
    fn crash_recovery_card_shows_stage_when_partial() {
        // #3:崩溃时 agent「进行到什么阶段」(pending.json 的 partial)应出现在卡片;无 partial 则不渲染该段。
        let items = [("om_1", "数到 100")];
        let with = card_crash_recovery(
            "recovery-card-1",
            "pdead",
            Some("我正在数:1 2 3 …"),
            &items,
            &[],
        );
        assert!(
            with.contains("我正在数:1 2 3 …"),
            "有 partial 时应展示「进行到的阶段」"
        );
        let without = card_crash_recovery("recovery-card-1", "pdead", None, &items, &[]);
        assert!(!without.contains("正处理到"), "无 partial 时不渲染阶段段");
    }

    #[test]
    fn crash_recovery_card_neutralizes_untrusted_markdown_and_tags() {
        let items = [("om_1", "<at user_id=\"all\">x</at> & `code`\n伪造新段")];
        let card = card_crash_recovery(
            "recovery-card-1",
            "pdead",
            Some("<at user_id=\"all\">stage</at>\n`stage`"),
            &items,
            &[],
        );
        assert!(!card.contains("<at user_id"));
        assert!(card.contains("<\u{200B}at user_id"));
        assert!(card.contains(" & "), "`&` 应原样保留而非变成实体");
        assert!(!card.contains("&amp;"));
        assert!(!card.contains("`code`"));
        assert!(!card.contains("`stage`"));
        let parsed: serde_json::Value = serde_json::from_str(&card).unwrap();
        let markdown = parsed["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|element| element.get("content").and_then(|value| value.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!markdown.contains("\n伪造新段"), "外部换行必须收缩成单行");
    }

    #[test]
    fn crash_recovery_card_recovered_items_have_disabled_button() {
        let items = [("om_1", "ping"), ("om_2", "做个 X")];
        let recovered = vec!["om_1".to_string()];
        let c = card_crash_recovery("recovery-card-1", "pdead", None, &items, &recovered);
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        // om_1 已恢复 → 不应有 recover 回调按钮(被 disabled 替代)
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let recover_mids: Vec<&str> = cbs
            .iter()
            .filter(|x| x["action"] == "recover")
            .filter_map(|x| x["mid"].as_str())
            .collect();
        assert!(
            !recover_mids.contains(&"om_1"),
            "om_1 已恢复不应有可点恢复按钮"
        );
        assert!(
            recover_mids.contains(&"om_2"),
            "om_2 未恢复仍有可点恢复按钮"
        );
        // 卡片应含「已恢复」文本
        assert!(c.contains("已恢复"), "已恢复项应展示置灰的已恢复按钮");
        // 待处理计数 = 1(只 om_2 还没恢复)
        assert!(c.contains("**1**"), "待处理计数应只算未恢复的");
    }

    #[test]
    fn session_panel_cursor_has_model_lifecycle_and_pick_workdir() {
        let models = vec!["gpt-5.5-high".to_string(), "opus-4.8".to_string()];
        let c = card_session_panel(
            "pdeadbeef",
            "cursor",
            "RPC · ACP 常驻",
            &rm(Some("gpt-5.5-high"), ModelSource::SessionLive),
            &reff(None, EffortSource::ModelInherent),
            &models,
            &[],
            &crate::subagent::SubagentDisplay::default(),
            false,
        );
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let actions: Vec<&str> = cbs.iter().filter_map(|x| x["action"].as_str()).collect();
        // A:改工作目录只剩「私信候选目录」(pick_workdir);群卡不再有手打 set_workdir 表单。
        for a in ["reset", "clean", "compact", "pick_workdir"] {
            assert!(actions.contains(&a), "缺少动作 {a}: {actions:?}");
        }
        assert!(
            !actions.contains(&"set_workdir"),
            "A:群面板不该再有手打 set_workdir 表单: {actions:?}"
        );
        assert!(
            !actions.contains(&"keepalive"),
            "/session 面板不应该出现保活按钮"
        );
        let selects: Vec<_> = v["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|el| el["tag"] == "select_static")
            .collect();
        assert_eq!(selects.len(), 2);
        assert_eq!(selects[0]["options"][0]["value"], "gpt-5.5-high");
        assert_eq!(selects[0]["options"][1]["value"], "opus-4.8");
        assert_eq!(selects[0]["initial_option"], "gpt-5.5-high");
        // cursor 无独立 effort 轴(efforts 传空)→ 不渲染 effort 按钮
        assert!(cbs.iter().all(|x| x["action"] != "effort"));
        // 所有回调都带 chat tag 供路由
        assert!(cbs.iter().all(|x| x["chat"] == "pdeadbeef"));
        // A(隐私):群面板绝不含手打表单 / workdir 输入框(路径不进群)。
        assert!(!c.contains("\"tag\":\"form\""), "群面板不该有手打表单");
        assert!(
            !c.contains("\"name\":\"workdir\""),
            "群面板不该有 workdir 输入框"
        );
        // 工作目录行只放引导语,不带任何路径。
        assert!(
            c.contains("点下方「私信候选目录」查看 / 切换"),
            "工作目录行应为引导语"
        );
        // 退出按钮文案 + reset 带二次确认弹窗
        assert!(c.contains("📦 归档"));
        assert!(c.contains("退出"));
        assert!(c.contains("\"confirm\""));
    }

    /// 递归收集所有 column_set 元素,断言按钮分栏带窄屏自适应(stretch):移动端把每列堆叠成整宽行,
    /// 否则 4 个生命周期按钮横排会被压扁、文案看不清。
    fn collect_column_sets<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        match v {
            serde_json::Value::Object(map) => {
                if map.get("tag").and_then(|t| t.as_str()) == Some("column_set") {
                    out.push(v);
                }
                for child in map.values() {
                    collect_column_sets(child, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for child in arr {
                    collect_column_sets(child, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn session_panel_button_rows_are_mobile_adaptive() {
        let models = vec!["gpt-5.5-high".to_string(), "opus-4.8".to_string()];
        let c = card_session_panel(
            "pdead",
            "cursor",
            "RPC · ACP 常驻",
            &rm(Some("gpt-5.5-high"), ModelSource::SessionLive),
            &reff(None, EffortSource::ModelInherent),
            &models,
            &[],
            &crate::subagent::SubagentDisplay::default(),
            false,
        );
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        let mut sets = Vec::new();
        collect_column_sets(&v, &mut sets);
        // 至少模型行 + 生命周期行两处分栏
        assert!(sets.len() >= 2, "应有按钮分栏: {}", sets.len());
        // 每处都带 flex_mode=stretch,窄屏才会把列堆叠成整宽行
        assert!(
            sets.iter().all(|s| s["flex_mode"] == "stretch"),
            "按钮分栏需 flex_mode=stretch 才能移动端自适应"
        );
    }

    /// P0-C 证据①:**候选为空不是 230099 的根因**。空候选走 `!model_btns.is_empty()` guard,
    /// 面板压根不渲染 model 区块,body 合法。这条钉住「空候选优雅降级」并否掉「候选空→非法卡」的
    /// 假设(真机根因是候选**过多**,见下一条)。
    #[test]
    fn empty_model_candidates_render_a_valid_panel_without_model_block() {
        let c = card_session_panel(
            "pcur",
            "cursor",
            "RPC · ACP 常驻",
            &rm(None, ModelSource::CliDefault),
            &reff(None, EffortSource::ModelInherent),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            false,
        );
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        assert!(!c.contains("切换模型"), "空候选不该渲染 model 区块");
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        assert!(
            cbs.iter().all(|x| x["action"] != "model"),
            "空候选不该产出任何 model 按钮"
        );
        // 生命周期 + 私信候选目录仍在:面板本身可用,只是没有 model 行。
        let actions: Vec<&str> = cbs.iter().filter_map(|x| x["action"].as_str()).collect();
        for a in ["reset", "clean", "compact", "pick_workdir"] {
            assert!(actions.contains(&a), "缺少动作 {a}: {actions:?}");
        }
    }

    /// 递归收集某个 column_set 里 `action=="model"` 的按钮列数。
    fn model_cols_in(set: &serde_json::Value) -> usize {
        set["columns"]
            .as_array()
            .map(|cols| {
                cols.iter()
                    .filter(|col| {
                        let mut cbs = Vec::new();
                        collect_callback_values(col, &mut cbs);
                        cbs.iter().any(|x| x["action"] == "model")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn many_model_candidates_are_capped_and_chunked_into_valid_rows() {
        let models: Vec<String> = (0..32)
            .map(|i| format!("gpt-5.6-sol[context=272k,reasoning=medium,fast=false,v={i}]"))
            .collect();
        let c = card_session_panel(
            "pcur",
            "codex",
            "RPC · ACP 常驻",
            &rm(None, ModelSource::CliDefault),
            &reff(None, EffortSource::ModelInherent),
            &models,
            &[],
            &crate::subagent::SubagentDisplay::default(),
            false,
        );
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["schema"], "2.0");

        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let model_btns = cbs.iter().filter(|x| x["action"] == "model").count();
        assert!(
            model_btns <= MODEL_PANEL_BTN_CAP,
            "列出的 model 按钮数必须 ≤ 上限,实际 {model_btns}"
        );

        let mut sets = Vec::new();
        collect_column_sets(&v, &mut sets);
        // 任何一个 column_set 里的 model 列都不能超过每行上限(否则就是「32 列一行」的旧非法形态)。
        for set in &sets {
            let cols = model_cols_in(set);
            assert!(
                cols <= MODEL_PANEL_BTNS_PER_ROW,
                "单个 column_set 的 model 列数必须 ≤ 每行上限,实际 {cols}"
            );
        }
        // 超出上限时给出「可 /model 手打」提示。
        assert!(c.contains("/model"), "截断后应提示可手打 /model");
    }

    /// 拿不到上下文预算时不渲染 ctx 项(任何 CLI 同理)。预算取不到就整项省略，
    /// 绝不渲染空进度条冒充 0%。
    #[test]
    fn turn_badge_omits_ctx_when_budget_unavailable() {
        let badge = TurnBadge {
            cli: "codex".into(),
            model: Some("gpt-5.6-sol".into()),
            quota_left_pct: Some(85),
            quota_5h_left_pct: None,
            show_quota: true,
            ..Default::default() // ctx 为 Default:两个字段都是 None
        };
        let rendered = badge.render();
        assert_eq!(rendered.as_deref(), Some("codex · gpt-5.6-sol · 7d 85%"));
        assert!(
            !rendered.unwrap().contains("ctx"),
            "预算不可得时不该有 ctx 项"
        );
    }

    /// codex 拿到 usage + 窗口 → 与 claude 同一渲染(进度条),不再走 `if cli == "codex"` 分支。
    #[test]
    fn turn_badge_codex_renders_progress_bar_with_window() {
        let badge = TurnBadge {
            cli: "codex".into(),
            model: Some("gpt-5.6-sol".into()),
            ctx: ContextBudget::from_tokens(600_000, Some(1_000_000)),
            ..Default::default()
        };
        assert_eq!(
            badge.render().as_deref(),
            Some("codex · gpt-5.6-sol · 600k/1.0M ██████░░░░ 60%")
        );
    }

    /// 上游缺窗口 → 只报绝对值,**不画进度条**(没有分母的进度条是假的)。
    #[test]
    fn turn_badge_reports_absolute_tokens_without_bar_when_window_missing() {
        let badge = TurnBadge {
            cli: "cursor".into(),
            model: Some("gpt-5.5".into()),
            ctx: ContextBudget::from_tokens(238_000, None),
            ..Default::default()
        };
        let rendered = badge.render().unwrap();
        assert_eq!(rendered, "cursor · gpt-5.5 · ctx 238k");
        assert!(!rendered.contains('█'), "无窗口不得画进度条");
    }

    #[test]
    fn turn_badge_cursor_renders_usage_and_percentage_from_acp() {
        let badge = TurnBadge {
            cli: "cursor".into(),
            model: Some("gpt-5.5".into()),
            ctx: ContextBudget::from_tokens(53_000, Some(200_000)),
            ..Default::default()
        };
        assert_eq!(
            badge.render().as_deref(),
            Some("cursor · gpt-5.5 · 53k/200k ███░░░░░░░ 26%")
        );
    }

    /// claude/statusLine 形态:模型 + 思考档 + 上下文百分比 + 5 小时与周额度。owner 要的就是这一行。
    #[test]
    fn turn_badge_renders_the_claude_statusline_shape() {
        let badge = TurnBadge {
            cli: "claude".into(),
            model: Some("Opus 5".into()),
            effort: Some("xhigh".into()),
            ctx: ContextBudget::from_reported_pct(35, Some(1_000_000)),
            quota_left_pct: Some(87),
            quota_resets_at: Some(1_786_356_600),
            quota_5h_left_pct: Some(92),
            quota_5h_resets_at: Some(1_786_097_400),
            show_quota: true,
            merge_default_codex_quota: false,
            hide_effort: false,
            hide_cli: false,
            hide_model: false,
            hide_ctx: false,
            context_remedy: Default::default(),
        };
        let local = |ts: i64, fmt: &str| {
            chrono::DateTime::from_timestamp(ts, 0)
                .unwrap()
                .with_timezone(&chrono::Local)
                .format(fmt)
                .to_string()
        };
        let reset = local(1_786_097_400, "%H:%M");
        let weekly_reset = local(1_786_356_600, "%-m/%-d %H:%M");
        assert_eq!(
            badge.render(),
            Some(format!(
                "claude · Opus 5 · xhigh · 350k/1.0M ████░░░░░░ 35% · 5h 92% ↻{reset} 7d 87% ↻{weekly_reset}"
            ))
        );
        let unknown_reset = TurnBadge {
            quota_5h_resets_at: None,
            quota_resets_at: None,
            ..badge
        };
        assert!(
            unknown_reset.render().unwrap().ends_with("· 5h 92% 7d 87%"),
            "取不到重置时刻就只显示百分比"
        );
    }

    /// 周窗口的重置点在几天之后,要带上日期;`quota` 长窗(非 claude/codex)不带重置时刻。
    #[test]
    fn turn_badge_dates_the_weekly_reset_and_leaves_other_long_windows_bare() {
        // 本机时区 2026-10-20 09:05,不依赖跑测试那台机器的时区。
        let at = chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 20, 9, 5, 0)
            .single()
            .unwrap()
            .timestamp();
        let codex = TurnBadge {
            cli: "codex".into(),
            quota_left_pct: Some(85),
            quota_resets_at: Some(at),
            show_quota: true,
            ..Default::default()
        };
        assert_eq!(
            codex.render().as_deref(),
            Some("codex · 7d 85% ↻10/20 09:05")
        );
        let cursor = TurnBadge {
            cli: "cursor".into(),
            ..codex
        };
        assert_eq!(cursor.render().as_deref(), Some("cursor · quota 85%"));
    }

    /// **额度默认不渲染**(owner 2026-08-10:敏感面,群卡默认不露)。有值但没开开关 →
    /// 角标其余项照常、额度整项消失;`show_quota` 是 bool,Default 即 false,任何忘记设置的
    /// 构造路径都自动落在这条(fail-closed 方向)。
    #[test]
    fn turn_badge_quota_hidden_unless_opted_in() {
        let badge = TurnBadge {
            cli: "claude".into(),
            model: Some("Opus 5".into()),
            effort: Some("xhigh".into()),
            ctx: ContextBudget::from_reported_pct(35, Some(1_000_000)),
            quota_left_pct: Some(87),
            quota_5h_left_pct: None,
            ..Default::default() // show_quota 默认 false
        };
        assert_eq!(
            badge.render().as_deref(),
            Some("claude · Opus 5 · xhigh · 350k/1.0M ████░░░░░░ 35%"),
            "默认关:额度项不得出现"
        );
    }

    /// **档位出厂显示,关掉才消失**。与额度相反:`hide_effort` 是 bool,Default 即 false,
    /// 任何忘记设置的构造路径都落在「显示」——那正是出厂默认,方向与配置说的话一致。
    #[test]
    fn turn_badge_effort_disappears_only_when_switched_off() {
        let on = TurnBadge {
            cli: "claude".into(),
            model: Some("Opus 5".into()),
            effort: Some("xhigh".into()),
            ctx: ContextBudget::from_reported_pct(35, Some(1_000_000)),
            ..Default::default()
        };
        assert_eq!(
            on.render().as_deref(),
            Some("claude · Opus 5 · xhigh · 350k/1.0M ████░░░░░░ 35%"),
            "默认开:档位照渲"
        );
        let off = TurnBadge {
            hide_effort: true,
            ..on.clone()
        };
        assert_eq!(
            off.render().as_deref(),
            Some("claude · Opus 5 · 350k/1.0M ████░░░░░░ 35%"),
            "关掉后只少档位这一项,其余项与分隔符照旧"
        );
    }

    /// CLI / 模型 / 上下文用量与档位同理:出厂显示,各自关掉只少那一项。关上下文只动角标这一行,
    /// `ctx` 本身照常留着(过重提醒卡读的是它)。
    #[test]
    fn turn_badge_cli_model_ctx_disappear_only_when_switched_off() {
        let on = TurnBadge {
            cli: "claude".into(),
            model: Some("Opus 5".into()),
            effort: Some("xhigh".into()),
            ctx: ContextBudget::from_reported_pct(35, Some(1_000_000)),
            ..Default::default()
        };
        let cases = [
            (
                TurnBadge {
                    hide_cli: true,
                    ..on.clone()
                },
                "Opus 5 · xhigh · 350k/1.0M ████░░░░░░ 35%",
            ),
            (
                TurnBadge {
                    hide_model: true,
                    ..on.clone()
                },
                "claude · xhigh · 350k/1.0M ████░░░░░░ 35%",
            ),
            (
                TurnBadge {
                    hide_ctx: true,
                    ..on.clone()
                },
                "claude · Opus 5 · xhigh",
            ),
        ];
        for (badge, want) in cases {
            assert_eq!(badge.render().as_deref(), Some(want));
            assert_eq!(badge.ctx, on.ctx, "关显示不清上下文数据");
        }
        let all_off = TurnBadge {
            hide_cli: true,
            hide_model: true,
            hide_effort: true,
            hide_ctx: true,
            ..on.clone()
        };
        assert_eq!(
            all_off.render(),
            None,
            "四项全关、额度没开 → 退回纯时间戳行"
        );
        let quota_only = TurnBadge {
            show_quota: true,
            quota_left_pct: Some(87),
            ..all_off
        };
        assert_eq!(
            quota_only.render().as_deref(),
            Some("7d 87%"),
            "额度开关独立于四项"
        );
    }

    /// 运行中的 subagent 有 last_activity 时,面板行追加「最后活跃 HH:MM:SS」(绝对时刻,
    /// 本地时区,与「更新于」同风格);已完成条目和没回填活跃时间的条目不带。
    #[test]
    fn session_panel_running_subagent_shows_last_activity() {
        use crate::model_effort::{EffortSource, ModelSource, ResolvedEffort, ResolvedModel};
        let rm = ResolvedModel {
            value: Some("m".into()),
            source: ModelSource::AgentConfig,
        };
        let re = ResolvedEffort {
            value: None,
            source: EffortSource::CliDefault,
        };
        let now = chrono::Utc::now();
        let act = now - chrono::Duration::seconds(8);
        let subs = crate::subagent::SubagentDisplay {
            agents: vec![
                crate::subagent::SubagentStatus::Running {
                    agent_id: "a1".into(),
                    description: "研究任务".into(),
                    agent_type: "Explore".into(),
                    tool_use_id: "t1".into(),
                    started_at: now - chrono::Duration::minutes(5),
                    last_activity: Some(act),
                    model: Some("claude-opus-5[1m]".into()),
                },
                crate::subagent::SubagentStatus::Completed {
                    agent_id: "a2".into(),
                    description: "已完任务".into(),
                    finished_at: now,
                    model: None,
                },
            ],
            workflows: vec![],
        };
        let c = card_session_panel("tag-sa", "claude", "tmux", &rm, &re, &[], &[], &subs, false);
        assert!(c.contains("研究任务"), "{c}");
        assert!(c.contains("运行中"), "{c}");
        // model 有值渲短名「(opus-5[1m])」;拿不到(a2)不显示。
        assert!(c.contains("(opus-5[1m])"), "model 应渲短名:{c}");
        // 绝对时刻:与渲染同一转换公式(本地时区 HH:MM:SS),钉死接线与格式。
        let expected = format!(
            "· 最后活跃 {}",
            act.with_timezone(&chrono::Local).format("%H:%M:%S")
        );
        assert!(
            c.contains(&expected),
            "运行中条目应带绝对活跃时刻 {expected}:{c}"
        );
        // 只有运行中那一条带;已完成条目不带。且不再出现「Ys 前」相对文案。
        assert_eq!(c.matches("· 最后活跃 ").count(), 1, "{c}");
        assert!(!c.contains("s 前"), "不得出现相对时间措辞:{c}");
    }

    /// 面板角标那一行:CLI / 模型 / 档位 / 上下文用量四个开关并排,与额度同族的显示项,按钮标签
    /// 即当前态、点击翻转。它们走分级开关,所以旁边必须写明「下一轮起生效」——用户点完当前这一轮
    /// 的角标不会变。
    #[test]
    fn session_panel_badge_item_toggles_reflect_state() {
        use crate::model_effort::{EffortSource, ModelSource, ResolvedEffort, ResolvedModel};
        use serde_json::Value;
        let rm = ResolvedModel {
            value: Some("m".into()),
            source: ModelSource::AgentConfig,
        };
        let re = ResolvedEffort {
            value: None,
            source: EffortSource::CliDefault,
        };
        let panel = |controls: SessionPanelControls| {
            card_session_panel(
                "tag-x",
                "claude",
                "tmux",
                &rm,
                &re,
                &[],
                &[],
                &crate::subagent::SubagentDisplay::default(),
                controls,
            )
        };
        fn find_button<'a>(v: &'a Value, action: &str) -> Option<&'a Value> {
            match v {
                Value::Object(map) => {
                    let hit = map.get("tag").and_then(Value::as_str) == Some("button")
                        && map.get("behaviors").map(|b| &b[0]["value"]["action"])
                            == Some(&Value::from(action));
                    if hit {
                        return Some(v);
                    }
                    map.values().find_map(|child| find_button(child, action))
                }
                Value::Array(items) => items.iter().find_map(|child| find_button(child, action)),
                _ => None,
            }
        }
        let all_on = SessionPanelControls {
            badge_cli: true,
            badge_model: true,
            badge_effort: true,
            badge_ctx: true,
            ..Default::default()
        };
        let card = panel(all_on.clone());
        assert!(card.contains("从下一轮起生效"), "必须说明生效时机:{card}");
        let value: Value = serde_json::from_str(&card).unwrap();
        let rows: Vec<&Value> = ["badge_cli", "badge_model", "badge_effort", "badge_ctx"]
            .iter()
            .map(|action| find_button(&value, action).expect(action))
            .collect();
        let labels: Vec<&str> = rows
            .iter()
            .map(|button| button["text"]["content"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["cli:开", "模型:开", "档位:开", "ctx:开"]);
        let row = card.find("cli:开").unwrap();
        assert!(
            card[row..].find("ctx:开").unwrap() < card[row..].find("\"tag\":\"markdown\"").unwrap(),
            "四个按钮在同一行:{card}"
        );

        for action in ["badge_cli", "badge_model", "badge_effort", "badge_ctx"] {
            let mut controls = all_on.clone();
            *match action {
                "badge_cli" => &mut controls.badge_cli,
                "badge_model" => &mut controls.badge_model,
                "badge_effort" => &mut controls.badge_effort,
                _ => &mut controls.badge_ctx,
            } = false;
            let value: Value = serde_json::from_str(&panel(controls)).unwrap();
            for other in ["badge_cli", "badge_model", "badge_effort", "badge_ctx"] {
                let button = find_button(&value, other).unwrap();
                let behavior = &button["behaviors"][0]["value"];
                let off = other == action;
                assert_eq!(behavior["on"], off, "{action} 关时 {other} 点击发相反值");
                assert_eq!(behavior["chat"], "tag-x");
                assert_eq!(button["type"], if off { "default" } else { "primary" });
                let label = button["text"]["content"].as_str().unwrap();
                assert_eq!(label.ends_with(":关"), off, "{label}");
            }
        }

        // 英文面板:模型按钮是单数的 `Model`,其余三项拼法不变。
        let english = |controls: SessionPanelControls| {
            crate::i18n::with_language(crate::i18n::Language::English, || panel(controls))
        };
        let label_of = |card: &str, action: &str| {
            let value: Value = serde_json::from_str(card).unwrap();
            find_button(&value, action).unwrap()["text"]["content"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let on = english(all_on.clone());
        let labels: Vec<String> = ["badge_cli", "badge_model", "badge_effort", "badge_ctx"]
            .iter()
            .map(|action| label_of(&on, action))
            .collect();
        assert_eq!(labels, ["cli:On", "Model:On", "Effort:On", "ctx:On"]);
        let off = english(SessionPanelControls {
            badge_model: false,
            ..all_on.clone()
        });
        assert_eq!(label_of(&off, "badge_model"), "Model:Off");
    }

    /// 面板「额度显示」开关:关态渲「关」按钮(default)、回调带 on:true;开态渲「开」按钮
    /// (primary)、回调带 on:false —— 按钮标签即当前态,点击即翻转。
    #[test]
    fn session_panel_quota_toggle_reflects_state() {
        use crate::model_effort::{EffortSource, ModelSource, ResolvedEffort, ResolvedModel};
        let rm = ResolvedModel {
            value: Some("m".into()),
            source: ModelSource::AgentConfig,
        };
        let re = ResolvedEffort {
            value: None,
            source: EffortSource::CliDefault,
        };
        let off = card_session_panel(
            "tag-x",
            "claude",
            "tmux",
            &rm,
            &re,
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            false,
        );
        assert!(off.contains("额度显示:关"), "关态按钮标签");
        assert!(
            off.contains(r#""action":"quota_badge","chat":"tag-x","on":true"#)
                || off.contains(r#""on":true"#),
            "关态点击应发 on:true:{off}"
        );
        let on = card_session_panel(
            "tag-x",
            "claude",
            "tmux",
            &rm,
            &re,
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                quota_badge: true,
                quota_left_pct: Some(73),
                quota_5h_left_pct: None,
                ..Default::default()
            },
        );
        assert!(on.contains("额度显示:开"), "开态按钮标签");
        assert!(
            on.contains("额度剩余:周 **73%**"),
            "开态应显示最新额度:{on}"
        );
        assert!(on.contains(r#""on":false"#), "开态点击应发 on:false");
        assert!(!off.contains("73%"), "关态不得泄露额度值:{off}");

        let unknown = card_session_panel(
            "tag-x",
            "claude",
            "tmux",
            &rm,
            &re,
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                quota_badge: true,
                quota_left_pct: None,
                quota_5h_left_pct: None,
                ..Default::default()
            },
        );
        assert!(unknown.contains("额度:<font color='grey'>暂未获取</font>"));

        let both = card_session_panel(
            "tag-x",
            "claude",
            "tmux",
            &rm,
            &re,
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                quota_badge: true,
                quota_left_pct: Some(73),
                quota_5h_left_pct: Some(41),
                ..Default::default()
            },
        );
        assert!(
            both.contains("额度剩余:5h **41%** · 周 **73%**"),
            "两个窗口都要显示:{both}"
        );

        let cursor = card_session_panel(
            "tag-x",
            "cursor",
            "rpc",
            &rm,
            &re,
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                quota_badge: true,
                quota_left_pct: Some(73),
                ..Default::default()
            },
        );
        assert!(
            cursor.contains("额度:**73% 剩余**"),
            "计费周期不标周:{cursor}"
        );
    }

    #[test]
    fn session_panel_codex_speed_uses_explicit_standard_and_fast_actions() {
        use crate::driver::CodexServiceTier;
        use crate::model_effort::{EffortSource, ModelSource, ResolvedEffort, ResolvedModel};
        let model = ResolvedModel {
            value: Some("gpt-5.6-sol".into()),
            source: ModelSource::AgentConfig,
        };
        let effort = ResolvedEffort {
            value: Some("high".into()),
            source: EffortSource::AgentConfig,
        };
        let card = card_session_panel(
            "tag-fast",
            "codex",
            "RPC · app-server 常驻",
            &model,
            &effort,
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                quota_badge: false,
                badge_effort: true,
                badge_cli: true,
                badge_model: true,
                badge_ctx: true,
                quota_left_pct: None,
                quota_5h_left_pct: None,
                context: Default::default(),
                cot: Default::default(),
                codex_speed: Some(CodexSpeedControl {
                    selected: Some(CodexServiceTier::Fast),
                    supported: Some(true),
                }),
                cursor_mode: None,
                cursor_config: None,
                terminal: false,
                model_default: None,
                effort_default: None,
            },
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        let speed: Vec<&serde_json::Value> = callbacks
            .iter()
            .filter(|callback| callback["action"] == "codex_service_tier")
            .collect();
        assert_eq!(speed.len(), 2, "Standard/Fast 必须是两个显式动作");
        assert!(speed.iter().any(|callback| callback["mode"] == "default"));
        assert!(speed.iter().any(|callback| callback["mode"] == "fast"));
        assert!(speed.iter().all(|callback| callback["chat"] == "tag-fast"));
        assert!(card.contains("Fast 约 1.5×"));
        assert!(card.contains("/fast on|off"), "面板应提示文字快捷键");
    }

    #[test]
    fn session_panel_does_not_offer_fast_when_model_is_unsupported_or_unknown() {
        use crate::model_effort::{EffortSource, ModelSource, ResolvedEffort, ResolvedModel};
        let model = ResolvedModel {
            value: Some("gpt-5.4-mini".into()),
            source: ModelSource::AgentConfig,
        };
        let effort = ResolvedEffort {
            value: None,
            source: EffortSource::CliDefault,
        };
        for (supported, note) in [
            (Some(false), "当前模型不支持"),
            (None, "无法确认当前模型支持性"),
        ] {
            let card = card_session_panel(
                "tag-mini",
                "codex",
                "RPC · app-server 常驻",
                &model,
                &effort,
                &[],
                &[],
                &crate::subagent::SubagentDisplay::default(),
                SessionPanelControls {
                    quota_badge: false,
                    badge_effort: true,
                    badge_cli: true,
                    badge_model: true,
                    badge_ctx: true,
                    quota_left_pct: None,
                    quota_5h_left_pct: None,
                    context: Default::default(),
                    cot: Default::default(),
                    codex_speed: Some(CodexSpeedControl {
                        selected: Some(crate::driver::CodexServiceTier::Fast),
                        supported,
                    }),
                    cursor_mode: None,
                    cursor_config: None,
                    terminal: false,
                    model_default: None,
                    effort_default: None,
                },
            );
            let value: serde_json::Value = serde_json::from_str(&card).unwrap();
            let mut callbacks = Vec::new();
            collect_callback_values(&value, &mut callbacks);
            let speed: Vec<&serde_json::Value> = callbacks
                .iter()
                .filter(|callback| callback["action"] == "codex_service_tier")
                .collect();
            assert_eq!(speed.len(), 1, "必须保留 Standard 退出路径:{card}");
            assert_eq!(speed[0]["mode"], "default");
            assert!(card.contains(note), "应解释为什么没有 Fast 按钮:{card}");
            assert!(card.contains("/fast off"), "仍应提示关闭 Fast 的文字入口");
        }
    }

    /// `quota_badge` 动作:解析 + operate 档鉴权(改持久状态 + 敏感面展示,不得免鉴权)。
    #[test]
    fn quota_badge_action_parses_and_requires_operate() {
        assert_eq!(
            CardAction::from_action("quota_badge"),
            CardAction::QuotaBadge
        );
        assert_eq!(CardAction::QuotaBadge.gate(), CardGate::Operate);
    }

    /// 角标各项开关:解析到各自的变体、对到各自的分级开关,改 per-chat 持久状态,与额度开关同档
    /// operate。
    #[test]
    fn badge_item_actions_parse_and_require_operate() {
        use crate::settings::SettingKey;
        for (action, want, key) in [
            ("badge_cli", CardAction::BadgeCli, SettingKey::BadgeCli),
            (
                "badge_model",
                CardAction::BadgeModel,
                SettingKey::BadgeModel,
            ),
            (
                "badge_effort",
                CardAction::BadgeEffort,
                SettingKey::BadgeEffort,
            ),
            ("badge_ctx", CardAction::BadgeCtx, SettingKey::BadgeCtx),
        ] {
            assert_eq!(CardAction::from_action(action), want);
            assert_eq!(want.badge_setting(), Some(key), "{action}");
            assert_eq!(want.gate(), CardGate::Operate, "{action}");
        }
        assert_eq!(CardAction::QuotaBadge.badge_setting(), None);
        assert_eq!(CardAction::SessionCot.badge_setting(), None);
    }

    #[test]
    fn codex_service_tier_action_parses_and_requires_operate() {
        assert_eq!(
            CardAction::from_action("codex_service_tier"),
            CardAction::CodexServiceTier
        );
        assert_eq!(CardAction::CodexServiceTier.gate(), CardGate::Operate);
    }

    #[test]
    fn session_panel_cursor_mode_uses_dynamic_catalog_and_operate_action() {
        use crate::model_effort::{EffortSource, ModelSource, ResolvedEffort, ResolvedModel};
        let card = card_session_panel(
            "tag-mode",
            "cursor",
            "RPC · ACP 常驻",
            &ResolvedModel {
                value: Some("gpt-5.6-sol".into()),
                source: ModelSource::SessionLive,
            },
            &ResolvedEffort {
                value: None,
                source: EffortSource::ModelInherent,
            },
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                quota_badge: false,
                badge_effort: true,
                badge_cli: true,
                badge_model: true,
                badge_ctx: true,
                quota_left_pct: None,
                quota_5h_left_pct: None,
                context: Default::default(),
                cot: Default::default(),
                codex_speed: None,
                cursor_mode: Some(CursorModeControl {
                    effective: Some("agent".into()),
                    pending: Some("review-v2".into()),
                    preference_error: false,
                    options: vec![
                        CursorModeOption {
                            id: "agent".into(),
                            name: "Agent".into(),
                        },
                        CursorModeOption {
                            id: "review-v2".into(),
                            name: "Review".into(),
                        },
                    ],
                    catalog_live: true,
                }),
                cursor_config: None,
                terminal: false,
                model_default: None,
                effort_default: None,
            },
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        let modes: Vec<&serde_json::Value> = callbacks
            .iter()
            .filter(|callback| callback["action"] == "cursor_mode")
            .collect();
        assert_eq!(modes.len(), 2);
        assert!(modes.iter().any(|callback| callback["mode"] == "agent"));
        assert!(modes.iter().any(|callback| callback["mode"] == "review-v2"));
        let mut buttons = Vec::new();
        collect_buttons(&value, &mut buttons);
        let mode_buttons: Vec<&serde_json::Value> = buttons
            .iter()
            .filter(|button| button["behaviors"][0]["value"]["action"] == "cursor_mode")
            .collect();
        assert_eq!(
            mode_buttons
                .iter()
                .find(|button| button["behaviors"][0]["value"]["mode"] == "agent")
                .unwrap()["type"],
            "primary",
            "只应高亮 live effective mode"
        );
        assert_eq!(
            mode_buttons
                .iter()
                .find(|button| button["behaviors"][0]["value"]["mode"] == "review-v2")
                .unwrap()["type"],
            "default",
            "盘上 pending mode 不能伪装成已生效"
        );
        assert!(card.contains("当前生效 `agent`"));
        assert!(card.contains("已保存待切换 `review-v2`"));
        // 占位符不能是尖括号(被飞书当标签)也不能是 HTML 实体(飞书不解析,原样吐 `&lt;id>`)。
        assert!(card.contains("/mode 模式名"));
        assert!(
            !card.contains("&lt;"),
            "卡片不该出现未被飞书解析的 HTML 实体"
        );
        assert_eq!(
            CardAction::from_action("cursor_mode"),
            CardAction::CursorMode
        );
        assert_eq!(CardAction::CursorMode.gate(), CardGate::Operate);
    }

    #[test]
    fn session_panel_cursor_mode_bounds_dynamic_wire_and_preserves_callback_ids() {
        let options: Vec<CursorModeOption> = (0..40)
            .map(|index| CursorModeOption {
                id: format!("mode-{index}-{}", "x".repeat(48)),
                name: format!("模式{index}-{}", "超长显示名".repeat(300)),
            })
            .collect();
        let control = CursorModeControl {
            effective: None,
            pending: Some("plan".into()),
            preference_error: false,
            options: options.clone(),
            catalog_live: true,
        };

        let elements = cursor_mode_elements(&control, "gdeadbeef");
        let wire = serde_json::to_vec(&elements).unwrap();
        assert!(
            wire.len() <= CURSOR_MODE_SECTION_WIRE_BYTE_CAP,
            "动态 mode 区域必须有独立 wire 预算，实际 {} bytes",
            wire.len()
        );
        let value = serde_json::Value::Array(elements);
        let mut buttons = Vec::new();
        collect_buttons(&value, &mut buttons);
        assert!(buttons.len() <= CURSOR_MODE_BTN_CAP);
        assert!(!buttons.is_empty(), "正常安全 id 至少应展示一个选项");
        for button in &buttons {
            let label = button["text"]["content"].as_str().unwrap();
            assert!(
                label.chars().count() <= CURSOR_MODE_LABEL_CHAR_CAP,
                "单个 plain_text 标签也必须有界:{label}"
            );
            assert_eq!(button["type"], "default", "没有 effective 时不能高亮");

            let callback_id = button["behaviors"][0]["value"]["mode"].as_str().unwrap();
            assert!(
                options.iter().any(|option| option.id == callback_id),
                "callback 必须保留 server 公布的完整 id:{callback_id}"
            );
        }
        assert!(
            String::from_utf8(wire).unwrap().contains("个模式未列出"),
            "被条数或 wire 预算省略的目录必须据实提示"
        );

        let card = card_session_panel(
            "gdeadbeef",
            "cursor",
            "RPC · ACP 常驻",
            &crate::model_effort::ResolvedModel {
                value: None,
                source: crate::model_effort::ModelSource::CliDefault,
            },
            &crate::model_effort::ResolvedEffort {
                value: None,
                source: crate::model_effort::EffortSource::ModelInherent,
            },
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                cursor_mode: Some(control),
                ..Default::default()
            },
        );
        assert!(
            card.len() <= CARD_TOTAL_SAFE_LIMIT,
            "异常 mode 目录不得把完整会话卡撑过总 wire 预算:{} bytes",
            card.len()
        );
    }

    #[test]
    fn session_panel_cursor_mode_exposes_preference_error_without_fake_pending() {
        let control = CursorModeControl {
            effective: Some("agent".into()),
            pending: None,
            preference_error: true,
            options: vec![CursorModeOption {
                id: "agent".into(),
                name: "Agent".into(),
            }],
            catalog_live: true,
        };

        let wire = serde_json::to_string(&cursor_mode_elements(&control, "gdeadbeef")).unwrap();
        assert!(wire.contains("保存的模式偏好不可读"));
        assert!(wire.contains("下一轮将拒绝启动"));
        assert!(!wire.contains("待切换"));
    }

    fn sample_cursor_config_panel() -> CursorConfigPanel {
        CursorConfigPanel {
            controls: vec![
                CursorConfigControl {
                    overridden: false,
                    default_value: None,
                    id: "fast".into(),
                    name: "Fast".into(),
                    description: Some("更低延迟".into()),
                    current: Some("false".into()),
                    pending: Some("true".into()),
                    options: vec![
                        CursorConfigOption {
                            value: "false".into(),
                            name: "关".into(),
                            description: None,
                        },
                        CursorConfigOption {
                            value: "true".into(),
                            name: "开".into(),
                            description: Some("优先速度".into()),
                        },
                    ],
                },
                CursorConfigControl {
                    overridden: false,
                    default_value: None,
                    id: "context".into(),
                    name: "Context".into(),
                    description: Some("上下文窗口".into()),
                    current: Some("272k".into()),
                    pending: None,
                    options: vec![
                        CursorConfigOption {
                            value: "272k".into(),
                            name: "272K".into(),
                            description: None,
                        },
                        CursorConfigOption {
                            value: "1m".into(),
                            name: "1M".into(),
                            description: None,
                        },
                    ],
                },
            ],
            preference_error: false,
            catalog_live: true,
            model_pending: false,
            model_pending_note: None,
        }
    }

    #[test]
    fn session_panel_cursor_config_is_per_chat_dynamic_highlighted_and_operate_gated() {
        let card = card_session_panel(
            "tag-config",
            "cursor",
            "RPC · ACP 常驻",
            &rm(Some("gpt-5.6-sol"), ModelSource::SessionLive),
            &reff(None, EffortSource::ModelInherent),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                cursor_config: Some(sample_cursor_config_panel()),
                ..Default::default()
            },
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        let configs: Vec<&serde_json::Value> = callbacks
            .iter()
            .filter(|callback| callback["action"] == "cursor_config")
            .collect();
        assert_eq!(configs.len(), 4, "两个动态轴的选项都应来自上层目录");
        assert!(configs
            .iter()
            .all(|callback| callback["chat"] == "tag-config"));
        assert!(configs
            .iter()
            .any(|callback| { callback["config_id"] == "fast" && callback["value"] == "true" }));
        assert!(configs
            .iter()
            .any(|callback| { callback["config_id"] == "context" && callback["value"] == "1m" }));

        let mut buttons = Vec::new();
        collect_buttons(&value, &mut buttons);
        let config_buttons: Vec<&serde_json::Value> = buttons
            .iter()
            .filter(|button| button["behaviors"][0]["value"]["action"] == "cursor_config")
            .collect();
        assert_eq!(
            config_buttons
                .iter()
                .find(|button| {
                    button["behaviors"][0]["value"]["config_id"] == "context"
                        && button["behaviors"][0]["value"]["value"] == "272k"
                })
                .unwrap()["type"],
            "primary",
            "只高亮 Cursor 报告的 current"
        );
        assert_eq!(
            config_buttons
                .iter()
                .find(|button| {
                    button["behaviors"][0]["value"]["config_id"] == "fast"
                        && button["behaviors"][0]["value"]["value"] == "true"
                })
                .unwrap()["type"],
            "default",
            "pending 不能伪装成已生效"
        );
        assert!(card.contains("Cursor 参数(本群)"));
        assert!(card.contains("跟随默认"));
        assert!(callbacks
            .iter()
            .any(|callback| callback["action"] == "cursor_config_default"
                && callback["chat"] == "tag-config"));
        assert!(
            !configs.iter().any(|callback| callback["inherit"] == true),
            "没有覆盖的轴不给「跟随默认」按钮"
        );
        assert!(card.contains("Fast 可能带来 2× 消耗"));
        assert!(card.contains("当前生效 `false`"));
        assert!(card.contains("待切换 `true`"));
        assert_eq!(
            CardAction::from_action("cursor_config"),
            CardAction::CursorConfig
        );
        assert_eq!(CardAction::CursorConfig.gate(), CardGate::Operate);
    }

    /// 面板的模型与 effort 分「本群」「设为默认」两组:有本群覆盖时写「本群 X(覆盖),默认 Y」、给
    /// 「跟随默认」,两组各自高亮自己那一层;没有覆盖时写「跟随默认 Y」,不给「跟随默认」按钮。
    #[test]
    fn session_panel_splits_chat_and_default_layers() {
        let models: Vec<String> = ["opus", "sonnet"].iter().map(|s| s.to_string()).collect();
        let efforts: Vec<String> = ["low", "high"].iter().map(|s| s.to_string()).collect();
        let render = |model: RM, effort: RE, controls: SessionPanelControls| {
            let card = card_session_panel(
                "g-a",
                "claude",
                "tmux",
                &model,
                &effort,
                &models,
                &efforts,
                &crate::subagent::SubagentDisplay::default(),
                controls,
            );
            let value: serde_json::Value = serde_json::from_str(&card).unwrap();
            let mut buttons = Vec::new();
            collect_buttons(&value, &mut buttons);
            (card, buttons)
        };
        let highlighted = |buttons: &[serde_json::Value], action: &str, key: &str| {
            buttons
                .iter()
                .filter(|b| b["behaviors"][0]["value"]["action"] == action)
                .filter(|b| b["type"] == "primary")
                .filter_map(|b| b["behaviors"][0]["value"][key].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        };
        let inherit = |buttons: &[serde_json::Value], action: &str| {
            buttons.iter().any(|b| {
                b["behaviors"][0]["value"]["action"] == action
                    && b["behaviors"][0]["value"]["inherit"] == true
            })
        };

        let (card, buttons) = render(
            rm(Some("sonnet"), ModelSource::ChatOverride),
            reff(Some("low"), EffortSource::ChatOverride),
            SessionPanelControls {
                model_default: Some(rm(Some("opus"), ModelSource::AgentConfig)),
                effort_default: Some(reff(Some("high"), EffortSource::AgentConfig)),
                ..Default::default()
            },
        );
        assert!(
            card.contains("模型:本群 `sonnet`(覆盖),默认 `opus`"),
            "{card}"
        );
        assert!(
            card.contains("effort:本群 `low`(覆盖),默认 `high`"),
            "{card}"
        );
        assert_eq!(highlighted(&buttons, "model", "model"), ["sonnet"]);
        assert_eq!(highlighted(&buttons, "model_default", "model"), ["opus"]);
        assert_eq!(highlighted(&buttons, "effort", "effort"), ["low"]);
        assert_eq!(highlighted(&buttons, "effort_default", "effort"), ["high"]);
        assert!(inherit(&buttons, "model") && inherit(&buttons, "effort"));

        let (card, buttons) = render(
            rm(Some("opus"), ModelSource::AgentConfig),
            reff(None, EffortSource::CliDefault),
            SessionPanelControls::default(),
        );
        assert!(card.contains("模型:跟随默认 `opus`"), "{card}");
        assert!(card.contains("跟随默认(CLI 默认)"), "{card}");
        assert_eq!(highlighted(&buttons, "model", "model"), ["opus"]);
        assert_eq!(highlighted(&buttons, "model_default", "model"), ["opus"]);
        assert!(!inherit(&buttons, "model") && !inherit(&buttons, "effort"));
    }

    /// Cursor 参数化 ACP 的 effort 行指向 Cursor 参数里的思考档位轴;模型待收敛时用上层写好的
    /// 原因与处理办法。「随模型」只留给 legacy 形态。
    #[test]
    fn cursor_effort_line_points_to_the_parameter_axis() {
        let render = |effort: RE, panel: Option<CursorConfigPanel>| {
            card_session_panel(
                "g-a",
                "cursor",
                "RPC · ACP 常驻",
                &rm(Some("gpt-5.6-sol"), ModelSource::SessionLive),
                &effort,
                &[],
                &[],
                &crate::subagent::SubagentDisplay::default(),
                SessionPanelControls {
                    cursor_config: panel,
                    ..Default::default()
                },
            )
        };
        let card = render(reff(Some("xhigh"), EffortSource::CursorParameter), None);
        assert!(card.contains("effort:`xhigh`"), "{card}");
        assert!(card.contains("在下方 Cursor 参数里切换"), "{card}");
        assert!(!card.contains("随模型"), "{card}");
        let card = render(reff(None, EffortSource::CursorParameter), None);
        assert!(card.contains("当前模型没有思考档位参数"), "{card}");
        let card = render(reff(None, EffortSource::CursorParameterUnknown), None);
        assert!(card.contains("会话公布参数目录后显示"), "{card}");

        let mut panel = sample_cursor_config_panel();
        panel.model_pending = true;
        panel.model_pending_note = Some("目标模型 `gpt-5.6` 不在目录里".into());
        let card = render(reff(None, EffortSource::CursorParameter), Some(panel));
        assert!(card.contains("目标模型 `gpt-5.6` 不在目录里"), "{card}");
        assert!(!card.contains("当前参数目录可能属于旧模型"), "{card}");
    }

    /// 本群覆盖的轴写明「本群覆盖(默认 Y)」并给「跟随默认」按钮,回调只带轴、不带值。
    #[test]
    fn an_overridden_cursor_axis_offers_to_follow_the_default() {
        let mut panel = sample_cursor_config_panel();
        panel.controls[0].overridden = true;
        panel.controls[0].default_value = Some("272k".into());
        let card = card_session_panel(
            "tag-config",
            "cursor",
            "RPC · ACP 常驻",
            &rm(Some("gpt-5.6-sol"), ModelSource::ChatOverride),
            &reff(None, EffortSource::ModelInherent),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                cursor_config: Some(panel.clone()),
                ..Default::default()
            },
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        let inherit: Vec<&serde_json::Value> = callbacks
            .iter()
            .filter(|callback| callback["action"] == "cursor_config" && callback["inherit"] == true)
            .collect();
        assert_eq!(inherit.len(), 1);
        assert_eq!(inherit[0]["config_id"], panel.controls[0].id.as_str());
        assert!(inherit[0].get("value").is_none());
        assert!(card.contains("本群覆盖(默认 `272k`)"));
    }

    #[test]
    fn session_panel_cursor_config_fail_closes_when_catalog_or_preferences_are_untrusted() {
        for (preference_error, catalog_live, model_pending, note) in [
            (true, true, false, "偏好不可读"),
            (false, false, false, "目录不可用"),
            (false, true, true, "模型待收敛"),
        ] {
            let mut panel = sample_cursor_config_panel();
            panel.preference_error = preference_error;
            panel.catalog_live = catalog_live;
            panel.model_pending = model_pending;
            let card = card_session_panel(
                "tag-unsafe",
                "cursor",
                "RPC · ACP 常驻",
                &rm(Some("gpt-5.6-sol"), ModelSource::SessionLive),
                &reff(None, EffortSource::ModelInherent),
                &[],
                &[],
                &crate::subagent::SubagentDisplay::default(),
                SessionPanelControls {
                    cursor_config: Some(panel),
                    ..Default::default()
                },
            );
            let value: serde_json::Value = serde_json::from_str(&card).unwrap();
            let mut callbacks = Vec::new();
            collect_callback_values(&value, &mut callbacks);
            assert!(
                callbacks
                    .iter()
                    .all(|callback| callback["action"] != "cursor_config"),
                "{note} 时不得留下可点配置按钮:{card}"
            );
            assert!(card.contains(note), "必须说明 fail-closed 原因:{card}");
            assert!(card.contains("Fast"), "安全状态下仍展示轴的只读状态");
        }
    }

    #[test]
    fn session_panel_cursor_config_bounds_untrusted_catalog_and_preserves_callbacks() {
        let controls: Vec<CursorConfigControl> = (0..24)
            .map(|axis| CursorConfigControl {
                overridden: false,
                default_value: None,
                id: format!("axis-{axis}-{}", "i".repeat(240)),
                name: format!("<axis-{axis}>-{}", "超长轴名".repeat(300)),
                description: Some(format!("<script>{}</script>", "超长说明".repeat(600))),
                current: Some(format!("current-{axis}-{}", "c".repeat(160))),
                pending: Some(format!("pending-{axis}-{}", "p".repeat(160))),
                options: (0..40)
                    .map(|option| CursorConfigOption {
                        value: format!("value-{axis}-{option}-{}", "v".repeat(240)),
                        name: format!("<value-{option}>-{}", "超长选项".repeat(300)),
                        description: Some("option-description".repeat(300)),
                    })
                    .collect(),
            })
            .collect();
        let originals = controls.clone();
        let panel = CursorConfigPanel {
            controls,
            preference_error: false,
            catalog_live: true,
            model_pending: false,
            model_pending_note: None,
        };
        let sections = cursor_config_panel_sections(&panel, "g-budget");
        for section in &sections {
            assert!(
                serde_json::to_vec(section).unwrap().len() <= CURSOR_CONFIG_SECTION_WIRE_BYTE_CAP,
                "每个动态轴都必须有独立 wire 预算"
            );
        }

        let card = card_session_panel(
            "g-budget",
            "cursor",
            "RPC · ACP 常驻",
            &rm(None, ModelSource::CliDefault),
            &reff(None, EffortSource::ModelInherent),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                cursor_config: Some(panel),
                ..Default::default()
            },
        );
        assert!(
            card.len() <= CARD_TOTAL_SAFE_LIMIT,
            "异常动态目录不得把整卡撑过总预算:{} bytes",
            card.len()
        );
        assert!(
            !card.contains("<script>"),
            "markdown 里的不可信描述必须消毒"
        );
        assert!(card.contains("<\u{200B}script>"));
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        let configs: Vec<&serde_json::Value> = callbacks
            .iter()
            .filter(|callback| callback["action"] == "cursor_config")
            .collect();
        assert!(!configs.is_empty(), "有界的目录至少应展示一个可选值");
        assert!(configs.len() <= CURSOR_CONFIG_CONTROL_CAP * CURSOR_CONFIG_OPTION_CAP);
        for callback in configs {
            let config_id = callback["config_id"].as_str().unwrap();
            let option_value = callback["value"].as_str().unwrap();
            assert!(
                originals.iter().any(|control| {
                    control.id == config_id
                        && control
                            .options
                            .iter()
                            .any(|option| option.value == option_value)
                }),
                "callback 必须保留 server 的完整 id/value"
            );
        }
    }

    #[test]
    fn closed_session_card_keeps_cursor_config_visible_but_disabled() {
        let card = card_session_closed(
            "g-closed",
            "cursor",
            "RPC · ACP 常驻",
            &rm(None, ModelSource::CliDefault),
            &reff(None, EffortSource::ModelInherent),
            &[],
            &[],
            SessionPanelControls {
                cursor_config: Some(sample_cursor_config_panel()),
                ..Default::default()
            },
            false,
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let mut saw_cursor_config = false;
        walk(&value, &mut |node| {
            if node["behaviors"][0]["value"]["action"] == "cursor_config" {
                saw_cursor_config = true;
                assert_eq!(node["disabled"], true, "终态卡只展示、不可交互");
            }
        });
        assert!(saw_cursor_config, "终态卡应保留 Cursor 参数原貌");
    }

    /// **口径全 CLI 统一成「还剩多少」**:codex 与 claude 同一个数值渲成同一个标签,同一句话在两个
    /// CLI 上不能读出相反的意思(owner 2026-08-10:codex 显剩余、claude 显已用,读起来会反)。
    #[test]
    fn quota_renders_as_remaining_for_every_cli() {
        let badge = |cli: &str, five_hour: Option<u8>, weekly: Option<u8>| TurnBadge {
            cli: cli.into(),
            quota_left_pct: weekly,
            quota_5h_left_pct: five_hour,
            show_quota: true,
            ..Default::default()
        };
        assert_eq!(
            badge("codex", Some(40), Some(85)).render().as_deref(),
            Some("codex · 5h 40% 7d 85%")
        );
        assert_eq!(
            badge("codex", None, Some(85)).render().as_deref(),
            Some("codex · 7d 85%")
        );
        assert_eq!(
            badge("claude", Some(7), None).render().as_deref(),
            Some("claude · 5h 7%")
        );
        assert_eq!(
            badge("cursor", None, Some(85)).render().as_deref(),
            Some("cursor · quota 85%")
        );
    }

    /// P1-E:Cursor 未收到可选 `usage_update` 时只有 CLI + 模型两项。
    #[test]
    fn turn_badge_cursor_degrades_to_cli_and_model() {
        let badge = TurnBadge {
            cli: "cursor".into(),
            model: Some("gpt-5.5".into()),
            quota_left_pct: None,
            quota_5h_left_pct: None,
            ..Default::default()
        };
        assert_eq!(badge.render().as_deref(), Some("cursor · gpt-5.5"));
    }

    /// F:claude/Transcript 也带角标。还没读到 statusLine 快照时 ctx/额度都不可得 → 角标退化成
    /// `claude · <模型>`(角标各项缺就省)。
    #[test]
    fn turn_badge_claude_shows_cli_model_without_ctx_when_snapshot_absent() {
        let badge = TurnBadge {
            cli: "claude".into(),
            model: Some("claude-opus-4-8".into()),
            quota_left_pct: None,
            quota_5h_left_pct: None,
            ..Default::default()
        };
        let rendered = badge.render();
        assert_eq!(rendered.as_deref(), Some("claude · claude-opus-4-8"));
        assert!(!rendered.unwrap().contains("ctx"), "快照不可得时不显 ctx");
    }

    /// P1-E:每项独立可缺——缺模型只剩 cli+ctx,缺 cli(空串)则该项不显示。
    #[test]
    fn turn_badge_omits_each_missing_field_independently() {
        let no_model = TurnBadge {
            cli: "claude".into(),
            model: None,
            ctx: ContextBudget::from_reported_pct(42, Some(1_000_000)),
            quota_left_pct: None,
            quota_5h_left_pct: None,
            ..Default::default()
        };
        assert_eq!(
            no_model.render().as_deref(),
            Some("claude · 420k/1.0M █████░░░░░ 42%")
        );

        let blank_cli = TurnBadge {
            cli: "  ".into(),
            model: Some("m".into()),
            quota_left_pct: Some(3),
            quota_5h_left_pct: None,
            show_quota: true,
            ..Default::default()
        };
        assert_eq!(blank_cli.render().as_deref(), Some("m · quota 3%"));
    }

    /// P1-E 关键不变量:全空 → None(调用方据此退回纯时间戳行,逐字节等同老路径)。
    #[test]
    fn turn_badge_all_empty_renders_none() {
        assert_eq!(TurnBadge::default().render(), None);
        assert_eq!(
            TurnBadge {
                cli: "".into(),
                model: Some("   ".into()),
                quota_left_pct: None,
                quota_5h_left_pct: None,
                ..Default::default()
            }
            .render(),
            None
        );
    }

    /// 空白候选 id 必须丢掉:空的 `text.content` 同样会让飞书拒渲(230099)。
    #[test]
    fn blank_model_candidate_is_dropped() {
        let models = vec![
            "gpt-5.5-high".to_string(),
            "   ".to_string(),
            String::new(),
            "gpt-5.5-high".to_string(),
        ];
        let c = card_session_panel(
            "pcur",
            "cursor",
            "RPC · ACP 常驻",
            &rm(None, ModelSource::CliDefault),
            &reff(None, EffortSource::ModelInherent),
            &models,
            &[],
            &crate::subagent::SubagentDisplay::default(),
            false,
        );
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        let selects: Vec<_> = v["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|el| el["tag"] == "select_static")
            .collect();
        assert_eq!(selects.len(), 2);
        for select in selects {
            assert_eq!(select["options"].as_array().unwrap().len(), 1);
            assert_eq!(select["options"][0]["value"], "gpt-5.5-high");
            assert!(select.get("initial_option").is_none());
        }
    }

    #[test]
    fn schedule_confirm_markdown_has_id_prompt_spec() {
        let s = crate::schedule::Schedule {
            id: "SCH-1".into(),
            kind: crate::schedule::Kind::Cron,
            spec: "0 9 * * 1-5".into(),
            agent: "ambrose".into(),
            chat_id: "c".into(),
            chat_type: "group".into(),
            prompt: "跑日报".into(),
            created_by: "ou".into(),
            created_at: "2026-06-14 00:00:00".into(),
            output: crate::schedule::ScheduleOutput::Progress,
            context: crate::schedule::ScheduleContext::Resume,
            enabled: true,
            last_fired: None,
            revision: 0,
        };
        let md = schedule_confirm_markdown(&s);
        // 回执给人话,不再回显 cron 表达式——用户排完最需要确认的是「我理解的节律对不对」。
        assert!(md.contains("SCH-1") && md.contains("跑日报"));
        assert!(md.contains("每个工作日 09:00"), "确认文案应说人话: {md}");

        // 改节律的回执要额外说明「从下一个周期开始」,免得用户以为会立刻补跑一次。
        let saved = schedule_saved_markdown(&s, true);
        assert!(saved.contains("SCH-1") && saved.contains("每个工作日 09:00"));
        assert!(saved.contains("下一个周期"));
        // 只改正文时不提节律变更。
        let renamed = schedule_saved_markdown(&s, false);
        assert!(!renamed.contains("下一个周期"));
    }

    #[test]
    fn card_schedules_panel_cancel_buttons() {
        let on = crate::schedule::Schedule {
            id: "SCH-1".into(),
            kind: crate::schedule::Kind::Interval,
            spec: "3600".into(),
            agent: "ambrose".into(),
            chat_id: "c".into(),
            chat_type: "group".into(),
            prompt: "巡检".into(),
            created_by: "ou".into(),
            created_at: "2026-06-14 00:00:00".into(),
            output: crate::schedule::ScheduleOutput::Progress,
            context: crate::schedule::ScheduleContext::Resume,
            enabled: true,
            last_fired: None,
            revision: 0,
        };
        let mut off = on.clone();
        off.id = "SCH-2".into();
        off.enabled = false;
        off.prompt = "旧的".into();
        let c = card_schedules_panel(&[on, off], false);
        let v: serde_json::Value = super::tests::workflow_card(&c).unwrap();
        // 只列在跑的那条;归档的折成一行计数,不再逐条堆在列表里(定时器只增不减,
        // 堆着会一直往下长,几十条后既看不清在跑的是谁,也会撞卡片组件数上限)。
        assert!(c.contains("SCH-1"), "在跑的要列出来");
        assert!(!c.contains("SCH-2"), "归档的不该逐条列出: {c}");
        assert!(!c.contains("旧的"), "归档条目的正文也不该出现");
        assert!(c.contains("另有 1 条已归档"), "但要让人知道它还在: {c}");
        assert!(c.contains("每隔 1 小时"), "节律应渲染成人话: {c}");
        assert!(!c.contains("Interval"), "不该把枚举 Debug 名漏给用户");
        // 在跑的 SCH-1 渲染「改 + 归档」;底部恒有「新建」。
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let actions: Vec<(&str, &str)> = cbs
            .iter()
            .map(|c| {
                (
                    c["action"].as_str().unwrap_or(""),
                    c["id"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            actions,
            vec![
                ("schedule_edit", "SCH-1"),
                ("schedule_cancel", "SCH-1"),
                ("schedule_timer_reset_toggle", ""),
                ("schedule_new", ""),
            ]
        );
        // 空列表也必须给「新建」入口——那正是最需要建一条的时候。
        let empty = card_schedules_panel(&[], false);
        let ev: serde_json::Value = super::tests::workflow_card(&empty).unwrap();
        assert_eq!(ev["schema"], "2.0");
        let mut ecbs = Vec::new();
        collect_callback_values(&ev, &mut ecbs);
        assert_eq!(ecbs.len(), 2);
        assert_eq!(ecbs[0]["action"], "schedule_timer_reset_toggle");
        assert_eq!(ecbs[1]["action"], "schedule_new");
        assert!(!empty.contains("已归档"), "一条都没有时不该提归档");
    }

    /// 全部归档时不能画成「一条都没有」——那会让人以为定时器丢了。
    #[test]
    fn card_schedules_panel_all_archived_still_accounts_for_them() {
        let mut s = crate::schedule::Schedule {
            id: "SCH-1".into(),
            kind: crate::schedule::Kind::Cron,
            spec: "0 9 * * 1-5".into(),
            agent: "ambrose".into(),
            chat_id: "c".into(),
            chat_type: "group".into(),
            prompt: "跑日报".into(),
            created_by: "ou".into(),
            created_at: "2026-06-14 00:00:00".into(),
            output: crate::schedule::ScheduleOutput::Progress,
            context: crate::schedule::ScheduleContext::Resume,
            enabled: false,
            last_fired: None,
            revision: 0,
        };
        let c = card_schedules_panel(std::slice::from_ref(&s), false);
        assert!(c.contains("没有在跑的定时器"));
        assert!(c.contains("另有 1 条已归档"), "得说清楚旧的还在: {c}");
        assert!(!c.contains("跑日报"), "归档条目正文不展开");
        // 归档条目不给任何操作按钮,只留「定时归档」开关 + 「新建」。
        let v: serde_json::Value = super::tests::workflow_card(&c).unwrap();
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        assert_eq!(cbs.len(), 2);
        assert_eq!(cbs[0]["action"], "schedule_timer_reset_toggle");
        assert_eq!(cbs[1]["action"], "schedule_new");
        // 恢复成在跑后照常出现在列表里。
        s.enabled = true;
        let live = card_schedules_panel(std::slice::from_ref(&s), false);
        assert!(live.contains("跑日报") && live.contains("每个工作日 09:00"));
        assert!(!live.contains("已归档"));
    }

    #[test]
    fn schedule_start_card_carries_prompt_through_every_freq_button() {
        // 起点卡的频率键必须**全部**是表单提交键:少一颗,从那条路径进参数卡就会丢正文。
        let c = card_schedule_start("跑日报", None);
        let v: serde_json::Value = super::tests::workflow_card(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        assert!(c.contains("跑日报"), "已填正文要回填进输入框");
        let mut btns = Vec::new();
        collect_buttons(&v, &mut btns);
        assert_eq!(btns.len(), 6, "六种频率各一颗按钮");
        for b in &btns {
            assert_eq!(
                b["form_action_type"], "submit",
                "频率键必须提交表单,否则正文丢失: {b}"
            );
            assert_eq!(b["behaviors"][0]["value"]["action"], "schedule_pick");
        }
        let kinds: Vec<&str> = btns
            .iter()
            .filter_map(|b| b["behaviors"][0]["value"]["kind"].as_str())
            .collect();
        // 「过多久」排第一:定一次性提醒时它远比滚到某个绝对时刻常见。
        assert_eq!(
            kinds,
            vec!["after", "once", "daily", "weekday", "weekly", "every"]
        );
        // 新建态不带 id;编辑态每颗键都要把 id 带下去,否则「换频率」后会变成新建一条。
        assert!(btns
            .iter()
            .all(|b| b["behaviors"][0]["value"].get("id").is_none()));
        let e: serde_json::Value =
            super::tests::workflow_card(&card_schedule_start("x", Some("SCH-9"))).unwrap();
        let mut ebtns = Vec::new();
        collect_buttons(&e, &mut ebtns);
        assert!(ebtns
            .iter()
            .all(|b| b["behaviors"][0]["value"]["id"] == "SCH-9"));
    }

    #[test]
    fn schedule_params_card_matches_freq_and_edit_state() {
        use crate::schedule::form::{CardFreq, Prefill};
        let pre = Prefill {
            prompt: "写周报".into(),
            output: crate::schedule::ScheduleOutput::Result,
            time: "10:30".into(),
            days: vec![1, 3],
            ..Default::default()
        };
        let c = card_schedule_params(&ScheduleParamsCard {
            freq: CardFreq::Weekly,
            pre: &pre,
            edit_id: Some("SCH-7"),
            current: Some("每周一、三 10:30"),
            error: None,
        });
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        // 控件 name 必须与解析侧常量一致,否则提交上来的值全部取不到。
        let mut names = Vec::new();
        collect_input_names(&v, &mut names);
        assert!(names.contains(&"prompt".to_string()));
        assert!(names.contains(&"output".to_string()));
        assert!(names.contains(&"days".to_string()));
        assert!(names.contains(&"time".to_string()));
        assert!(!names.contains(&"date".to_string()), "每周不该出现日期控件");
        // 预填与「当前」兜底文本都在。
        assert!(c.contains("写周报") && c.contains("10:30"));
        assert!(c.contains(r#""initial_option":"result""#));
        assert!(c.contains("当前:每周一、三 10:30"));
        // 编辑态:提交键是 save 且带 id;「换频率」也带 id。
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let save = cbs.iter().find(|c| c["action"] == "schedule_save").unwrap();
        assert_eq!(save["id"], "SCH-7");
        assert_eq!(save["kind"], "weekly");
        let back = cbs.iter().find(|c| c["action"] == "schedule_new").unwrap();
        assert_eq!(back["id"], "SCH-7");
    }

    #[test]
    fn schedule_params_card_only_shows_fields_that_freq_needs() {
        use crate::schedule::form::{CardFreq, Prefill};
        let pre = Prefill::default();
        let plain = |f: CardFreq| ScheduleParamsCard {
            freq: f,
            pre: &pre,
            edit_id: None,
            current: None,
            error: None,
        };
        let names_for = |f: CardFreq| {
            let v: serde_json::Value =
                serde_json::from_str(&card_schedule_params(&plain(f))).unwrap();
            let mut names = Vec::new();
            collect_input_names(&v, &mut names);
            names.sort();
            names
        };
        // 这是两步交互存在的全部理由:每种频率只摆它自己需要的字段。
        assert_eq!(names_for(CardFreq::Daily), vec!["output", "prompt", "time"]);
        assert_eq!(
            names_for(CardFreq::Weekday),
            vec!["output", "prompt", "time"]
        );
        assert_eq!(
            names_for(CardFreq::Weekly),
            vec!["days", "output", "prompt", "time"]
        );
        assert_eq!(
            names_for(CardFreq::Once),
            vec!["date", "output", "prompt", "time"]
        );
        assert_eq!(
            names_for(CardFreq::Every),
            vec!["every_n", "every_unit", "output", "prompt"]
        );
        // 新建态:提交键是 create 且不带 id。
        let v: serde_json::Value =
            serde_json::from_str(&card_schedule_params(&plain(CardFreq::Daily))).unwrap();
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let create = cbs
            .iter()
            .find(|c| c["action"] == "schedule_create")
            .unwrap();
        assert!(create.get("id").is_none());
        // 间隔卡的单位下拉必须自带默认值,否则用户不动它就撞上「看不懂的单位「」」。
        let every = card_schedule_params(&plain(CardFreq::Every));
        assert!(every.contains(r#""initial_option":"h""#), "单位应默认小时");
    }

    /// 校验没过时**重画参数卡并保留已填内容**,而不是把整张卡换成一句错误文本。
    /// 填错时间是最常见的一次失误,代价不该是把正文和星期全部重填。
    #[test]
    fn schedule_params_card_keeps_input_when_showing_error() {
        use crate::schedule::form::{CardFreq, Prefill};
        let pre = Prefill {
            prompt: "写周报".into(),
            time: "25:61".into(),
            days: vec![1, 3],
            ..Default::default()
        };
        let c = card_schedule_params(&ScheduleParamsCard {
            freq: CardFreq::Weekly,
            pre: &pre,
            edit_id: None,
            current: None,
            error: Some("时间越界:「25:61」"),
        });
        assert!(c.contains("⚠️ 时间越界"), "要显示原因");
        assert!(c.contains("写周报"), "正文不能丢");
        assert!(c.contains("25:61"), "填错的值也要回显,让人看得见自己填了啥");
        assert!(c.contains(r#""selected_values":["1","3"]"#), "星期不能丢");
        // 仍然是一张可提交的参数卡,不是终态。
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        assert!(cbs.iter().any(|c| c["action"] == "schedule_create"));
    }

    /// 建成之后那张卡**必须真的点不动**:聊天记录会一直留着它,滚上去再点一次「创建」就会重复
    /// 建一条定时器,或拿过期的值覆盖已经改好的那条。四个新控件都得被置灰覆盖到。
    #[test]
    fn settled_schedule_card_disables_form_controls() {
        use crate::schedule::form::{CardFreq, Prefill};
        let pre = Prefill {
            prompt: "跑日报".into(),
            time: "09:00".into(),
            days: vec![1, 3],
            ..Default::default()
        };
        let c = card_schedule_settled(
            &ScheduleParamsCard {
                freq: CardFreq::Weekly,
                pre: &pre,
                edit_id: None,
                current: None,
                error: None,
            },
            "⏰ 已排定 **SCH-9**",
        );
        let v: serde_json::Value = super::tests::workflow_card(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        // 保留原貌:填过的内容还看得见。
        assert!(c.contains("跑日报") && c.contains("09:00"));
        // 回执追加在底部。
        assert!(c.contains("已排定") && c.contains("SCH-9"));
        // 每个交互控件都置灰——按钮和四个选择器一个都不能漏。
        let mut btns = Vec::new();
        collect_buttons(&v, &mut btns);
        assert!(!btns.is_empty());
        for b in &btns {
            assert_eq!(b["disabled"], true, "按钮必须置灰: {b}");
        }
        for tag in [
            "input",
            "picker_time",
            "multi_select_static",
            "select_static",
            "date_picker",
        ] {
            let mut found = Vec::new();
            collect_by_tag(&v, tag, &mut found);
            for el in &found {
                assert_eq!(el["disabled"], true, "{tag} 必须置灰: {el}");
            }
        }
        // 反证:未置灰的同一张卡确实是可点的(否则上面的断言可能是空跑)。
        let live = card_schedule_params(&ScheduleParamsCard {
            freq: CardFreq::Weekly,
            pre: &pre,
            edit_id: None,
            current: None,
            error: None,
        });
        assert!(!live.contains(r#""disabled":true"#));
    }

    /// v2 对不支持的属性是**报错**而非忽略,赌错一个属性名就是整张卡发不出去。
    /// 四个新控件一律不用 `label`,字段说明走 markdown 行。
    #[test]
    fn schedule_cards_avoid_unverified_control_attributes() {
        use crate::schedule::form::{CardFreq, Prefill};
        let pre = Prefill::default();
        for f in [
            CardFreq::Once,
            CardFreq::Daily,
            CardFreq::Weekday,
            CardFreq::Weekly,
            CardFreq::Every,
        ] {
            let c = card_schedule_params(&ScheduleParamsCard {
                freq: f,
                pre: &pre,
                edit_id: None,
                current: None,
                error: None,
            });
            assert!(
                !c.contains(r#""label""#),
                "控件不该带未经真机验证的 label: {c}"
            );
        }
        assert!(!card_schedule_start("", None).contains(r#""label""#));
    }

    #[test]
    fn schedule_card_actions_are_registered_with_right_gate() {
        use CardGate::*;
        // 纯渲染免鉴权;落盘动作过 operate。漏登记会被 `Unknown` fail-closed 兜住,
        // 但那会让「改」按钮对非 operator 静默失效,所以这里正面钉死。
        assert_eq!(card_gate_tier("schedule_new"), None);
        assert_eq!(card_gate_tier("schedule_pick"), None);
        assert_eq!(card_gate_tier("schedule_edit"), None);
        assert_eq!(card_gate_tier("schedule_create"), Operate);
        assert_eq!(card_gate_tier("schedule_save"), Operate);
        assert_eq!(card_gate_tier("schedule_cancel"), Operate);
    }

    #[test]
    fn cursor_model_dropdowns_preserve_catalog_order_and_selected_layers() {
        let models: Vec<_> = (0..32).rev().map(|i| format!("model-{i}")).collect();
        let card = card_session_panel(
            "tag-dropdown",
            "cursor",
            "RPC",
            &rm(Some("model-1"), ModelSource::ChatOverride),
            &reff(None, EffortSource::ModelInherent),
            &models,
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                model_default: Some(rm(Some("model-20"), ModelSource::AgentConfig)),
                ..Default::default()
            },
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let selects: Vec<_> = value["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["tag"] == "select_static")
            .collect();
        assert_eq!(selects.len(), 2);
        for (select, action, current) in [
            (selects[0], "model", "model-1"),
            (selects[1], "model_default", "model-20"),
        ] {
            assert_eq!(select["behaviors"][0]["value"]["action"], action);
            assert_eq!(select["behaviors"][0]["value"]["chat"], "tag-dropdown");
            assert_eq!(select["initial_option"], current);
            let ids: Vec<_> = select["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["value"].as_str().unwrap())
                .collect();
            assert_eq!(ids, models.iter().map(String::as_str).collect::<Vec<_>>());
        }
        assert!(!card.contains("个候选未列出"));
    }

    #[test]
    fn cursor_model_dropdowns_budget_keeps_parameter_controls_and_freezes_selections() {
        let models: Vec<_> = (0..500).map(|i| format!("model-{i}")).collect();
        let controls = SessionPanelControls {
            cursor_config: Some(CursorConfigPanel {
                catalog_live: true,
                controls: ["context", "reasoning", "fast"]
                    .into_iter()
                    .map(|id| CursorConfigControl {
                        id: id.into(),
                        name: id.into(),
                        description: None,
                        current: Some("a".into()),
                        pending: None,
                        overridden: false,
                        default_value: None,
                        options: ["a", "b"]
                            .into_iter()
                            .map(|value| CursorConfigOption {
                                value: value.into(),
                                name: value.into(),
                                description: None,
                            })
                            .collect(),
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let model = rm(Some("outside-catalog"), ModelSource::SessionLive);
        let effort = reff(None, EffortSource::CursorParameter);
        let card = card_session_panel(
            "tag-budget",
            "cursor",
            "RPC",
            &model,
            &effort,
            &models,
            &[],
            &crate::subagent::SubagentDisplay::default(),
            controls.clone(),
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let selects: Vec<_> = value["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["tag"] == "select_static")
            .collect();
        assert_eq!(selects.len(), 2);
        let shown = selects[0]["options"].as_array().unwrap().len();
        assert!(shown > 12 && shown < models.len());
        assert_eq!(selects[1]["options"].as_array().unwrap().len(), shown);
        assert!(selects.iter().all(|v| v.get("initial_option").is_none()));
        assert!(card.contains("这里只列"));
        assert!(card.len() <= CARD_TOTAL_SAFE_LIMIT);
        let mut callbacks = Vec::new();
        collect_callback_values(&value, &mut callbacks);
        assert_eq!(
            callbacks
                .iter()
                .filter(|v| v["action"] == "cursor_config")
                .count(),
            6
        );
        let closed = card_session_closed(
            "tag-budget",
            "cursor",
            "RPC",
            &model,
            &effort,
            &models,
            &[],
            controls,
            false,
        );
        let closed: serde_json::Value = serde_json::from_str(&closed).unwrap();
        let selects: Vec<_> = closed["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["tag"] == "select_static")
            .collect();
        assert_eq!(selects.len(), 2);
        assert!(selects.iter().all(|v| v["disabled"] == true));
    }

    fn select_values(select: &serde_json::Value) -> Vec<String> {
        select["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["value"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn cursor_panel_lists_first_32_and_keeps_selected_models_selectable() {
        let models: Vec<_> = (0..110).map(|i| format!("model-{i}")).collect();
        let card = card_session_panel(
            "tag-cap",
            "cursor",
            "RPC",
            &rm(Some("model-80"), ModelSource::ChatOverride),
            &reff(None, EffortSource::ModelInherent),
            &models,
            &[],
            &crate::subagent::SubagentDisplay::default(),
            SessionPanelControls {
                model_default: Some(rm(Some("model-3"), ModelSource::AgentConfig)),
                ..Default::default()
            },
        );
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let selects: Vec<_> = value["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["tag"] == "select_static")
            .collect();
        assert_eq!(selects.len(), 2);
        // 本群当前值排在 32 名之外:顶替最后一项,总数仍是 32,且被高亮。
        let chat = select_values(selects[0]);
        assert_eq!(chat.len(), 32);
        assert_eq!(chat[..31], models[..31]);
        assert_eq!(chat[31], "model-80");
        assert_eq!(selects[0]["initial_option"], "model-80");
        // 默认层当前值本就在前 32 里:原样列前 32。
        assert_eq!(select_values(selects[1]), models[..32]);
        assert_eq!(selects[1]["initial_option"], "model-3");
        assert!(card.contains("共 110 个模型,这里只列 32 个"));
        assert!(session_panel_elements_fit_card_budget(
            value["body"]["elements"].as_array().unwrap(),
            "cursor"
        ));
    }

    #[test]
    fn capped_model_options_never_exceed_limit() {
        let models: Vec<String> = (0..5).map(|i| format!("m{i}")).collect();
        let refs: Vec<&String> = models.iter().collect();
        let shown = |limit, selected| {
            capped_model_options(&refs, limit, selected)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(shown(3, Some("m4")), ["m0", "m1", "m4"]);
        assert_eq!(shown(3, Some("m1")), ["m0", "m1", "m2"]);
        assert_eq!(shown(3, Some("absent")), ["m0", "m1", "m2"]);
        assert!(shown(0, Some("m4")).is_empty());
        assert_eq!(shown(9, None).len(), 5);
    }

    #[test]
    fn cursor_model_picker_lists_full_catalog_with_layer_actions() {
        let models: Vec<_> = (0..110).map(|i| format!("model-{i}")).collect();
        for (default_layer, action) in [(false, "model"), (true, "model_default")] {
            let card =
                card_cursor_model_picker(&models, Some("model-100"), "tag-pick", default_layer);
            let value: serde_json::Value = serde_json::from_str(&card).unwrap();
            let elements = value["body"]["elements"].as_array().unwrap();
            let selects: Vec<_> = elements
                .iter()
                .filter(|v| v["tag"] == "select_static")
                .collect();
            assert_eq!(selects.len(), 1);
            assert_eq!(select_values(selects[0]), models);
            assert_eq!(selects[0]["initial_option"], "model-100");
            assert_eq!(selects[0]["behaviors"][0]["value"]["action"], action);
            assert_eq!(selects[0]["behaviors"][0]["value"]["chat"], "tag-pick");
            assert!(card.contains("共 110 个"));
            assert!(!card.contains("个模型未列出"));
            let mut callbacks = Vec::new();
            collect_callback_values(&value, &mut callbacks);
            assert!(callbacks.iter().any(|v| v["action"] == "card_close"));
        }
    }

    #[test]
    fn cursor_model_picker_truncates_huge_catalog_within_card_budget() {
        let models: Vec<_> = (0..500).map(|i| format!("model-{i}")).collect();
        let card = card_cursor_model_picker(&models, Some("model-499"), "tag-pick", false);
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        let select = value["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["tag"] == "select_static")
            .unwrap();
        let shown = select_values(select);
        assert!(shown.len() <= CURSOR_MODEL_OPTION_CAP && shown.len() > 150);
        assert_eq!(shown.last().map(String::as_str), Some("model-499"));
        assert_eq!(select["initial_option"], "model-499");
        assert!(tagged_object_count(&value) <= SAFE_OBJECT_LIMIT);
        assert!(card.len() <= CARD_TOTAL_SAFE_LIMIT);
        assert!(card.contains(&format!("还有 {} 个模型未列出", 500 - shown.len())));
    }

    #[test]
    fn cursor_model_picker_survives_empty_catalog() {
        let card = card_cursor_model_picker(&[], None, "tag-pick", false);
        let value: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert!(value["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["tag"] != "select_static"));
    }

    #[test]
    fn session_panels_fit_component_budget_with_codex_catalog() {
        fn wire_objects(value: &serde_json::Value) -> usize {
            match value {
                serde_json::Value::Object(map) => 1 + map.values().map(wire_objects).sum::<usize>(),
                serde_json::Value::Array(items) => items.iter().map(wire_objects).sum(),
                _ => 0,
            }
        }
        let efforts = ["low", "medium", "high", "xhigh", "max", "ultra"].map(str::to_string);
        for count in [9, 12, 32] {
            let models: Vec<_> = (0..count).map(|i| format!("model-{i}")).collect();
            let controls = SessionPanelControls {
                codex_speed: Some(CodexSpeedControl {
                    selected: None,
                    supported: Some(true),
                }),
                model_default: Some(rm(Some("model-0"), ModelSource::AgentConfig)),
                effort_default: Some(reff(Some("high"), EffortSource::AgentConfig)),
                ..Default::default()
            };
            let card = card_session_panel(
                "tag-budget",
                "codex",
                "RPC",
                &rm(Some("model-1"), ModelSource::ChatOverride),
                &reff(Some("ultra"), EffortSource::ChatOverride),
                &models,
                &efforts,
                &crate::subagent::SubagentDisplay::default(),
                controls.clone(),
            );
            let value: serde_json::Value = serde_json::from_str(&card).unwrap();
            assert!(
                wire_objects(&value) <= 200,
                "{count} models: {} wire objects",
                wire_objects(&value)
            );
            assert!(card.len() <= CARD_TOTAL_SAFE_LIMIT);
            let closed = card_session_closed(
                "tag-budget",
                "codex",
                "RPC",
                &rm(Some("model-1"), ModelSource::ChatOverride),
                &reff(Some("ultra"), EffortSource::ChatOverride),
                &models,
                &efforts,
                controls,
                false,
            );
            let closed: serde_json::Value = serde_json::from_str(&closed).unwrap();
            assert!(
                wire_objects(&closed) <= 200,
                "closed {count} models: {} wire objects",
                wire_objects(&closed)
            );
            let mut callbacks = Vec::new();
            collect_callback_values(&value, &mut callbacks);
            for action in [
                "model",
                "effort",
                "codex_service_tier",
                "session_cot",
                "clean",
                "reset",
                "pick_workdir",
                "session_refresh",
                "session_close",
            ] {
                assert!(
                    callbacks.iter().any(|v| v["action"] == action),
                    "missing {action}"
                );
            }
        }
    }

    #[test]
    fn session_panel_claude_renders_model_and_effort_rows() {
        // #2a:claude/codex 现在也渲染 model 行,且有独立 effort 行(cursor 无 effort 轴)。
        let models = vec![
            "claude-opus-4-8".to_string(),
            "claude-sonnet-4-6".to_string(),
        ];
        let efforts = vec!["medium".to_string(), "high".to_string()];
        let c = card_session_panel(
            "pdead",
            "claude",
            "tmux 交互",
            &rm(Some("claude-opus-4-8"), ModelSource::AgentConfig),
            &reff(Some("high"), EffortSource::AgentConfig),
            &models,
            &efforts,
            &crate::subagent::SubagentDisplay::default(),
            false,
        );
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        // model 按钮:每候选一个,带 model 名。
        let model_btns: Vec<&serde_json::Value> =
            cbs.iter().filter(|x| x["action"] == "model").collect();
        assert_eq!(model_btns.len(), 2);
        assert_eq!(model_btns[0]["model"], "claude-opus-4-8");
        // 独立 effort 行:每候选一个 effort 按钮。
        let effort_btns: Vec<&serde_json::Value> =
            cbs.iter().filter(|x| x["action"] == "effort").collect();
        assert_eq!(effort_btns.len(), 2);
        assert_eq!(effort_btns[1]["effort"], "high");
        // 生命周期 + 私信候选目录仍在(A:手打 set_workdir 表单已移除)。
        let actions: Vec<&str> = cbs.iter().filter_map(|x| x["action"].as_str()).collect();
        assert!(actions.contains(&"reset"));
        assert!(actions.contains(&"pick_workdir"));
        assert!(
            !actions.contains(&"set_workdir"),
            "A:不该再有手打 set_workdir"
        );
    }

    /// 摘要区:每个可配项标「生效值 + 来源」;cursor 的 effort 显示「随模型」而非假默认;
    /// 通道行存在;绝不再出现「未指定」。工作目录行(A)只放引导语、绝不含路径。
    #[test]
    fn session_panel_summary_shows_effective_values_and_sources() {
        let c = card_session_panel(
            "pcur",
            "cursor",
            "RPC · ACP 常驻",
            &rm(Some("gpt-5.6-sol-high"), ModelSource::SessionLive),
            &reff(None, EffortSource::ModelInherent),
            &["gpt-5.6-sol-high".to_string()],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            false,
        );
        // B(精简):cli 与通道并一行 `- `cursor` · RPC · ACP 常驻`。
        assert!(
            c.contains("`cursor` · RPC · ACP 常驻"),
            "cli/通道 应并一行: {c}"
        );
        assert!(!c.contains("通道:"), "B:不该再有单独的「通道:」行: {c}");
        assert!(c.contains("gpt-5.6-sol-high"), "缺模型生效值");
        assert!(c.contains("会话实时"), "模型来源未标注: {c}");
        // cursor effort 烤在模型名里 → 标「随模型」,不显示假「默认」。
        assert!(c.contains("随模型"), "cursor effort 应标随模型");
        // A(隐私):工作目录只放引导语,绝不带路径 / 来源标注。
        assert!(
            c.contains("点下方「私信候选目录」查看 / 切换"),
            "工作目录行应为引导语"
        );
        assert!(!c.contains("本会话 /cd"), "A:工作目录来源/路径不该进群卡");
        assert!(!c.contains("未指定"), "不该再出现「未指定」: {c}");
    }

    /// CliDefault(桶也空)的模型显示「由 CLI 自定」占位而非「未指定」;codex 有独立 effort 轴
    /// 但没配显示「CLI 默认」;工作目录行(A)恒为引导语(无论是否配过),不再有「未配置」灰字。
    #[test]
    fn session_panel_cli_default_shows_placeholders_not_unspecified() {
        let c = card_session_panel(
            "pcur",
            "codex",
            "RPC · app-server 常驻",
            &rm(None, ModelSource::CliDefault),
            &reff(None, EffortSource::CliDefault),
            &[],
            &[],
            &crate::subagent::SubagentDisplay::default(),
            false,
        );
        assert!(c.contains("由 CLI 自定"), "CliDefault 模型应显示占位: {c}");
        assert!(c.contains("CLI 默认"), "codex 未配 effort 应显示 CLI 默认");
        // A:工作目录行恒为引导语(不再区分「未配置(用启动目录)」——路径链一律走私信)。
        assert!(
            c.contains("点下方「私信候选目录」查看 / 切换"),
            "工作目录行应为引导语"
        );
        assert!(!c.contains("未指定"), "不该再出现「未指定」: {c}");
    }

    /// 私信选择器卡:候选按钮带**目标 chat 的 raw id/type**(tag 解不回)+ 各自路径;`/cd` 手打兜底。
    #[test]
    fn workdir_picker_card_carries_target_and_candidate_paths() {
        let items = vec![
            WorkDirPickItem {
                path: "~/peer/worktree",
                hint: "同群其他 agent",
            },
            WorkDirPickItem {
                path: "~/projects",
                hint: "本 agent 常用",
            },
        ];
        let c = card_workdir_picker("oc_group", "group", Some("~/current/worktree"), &items);
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["schema"], "2.0");
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let picks: Vec<&serde_json::Value> = cbs
            .iter()
            .filter(|x| x["action"] == "set_workdir_to")
            .collect();
        assert_eq!(picks.len(), 2);
        assert!(
            picks
                .iter()
                .all(|p| p["target_chat_id"] == "oc_group" && p["target_chat_type"] == "group"),
            "候选必须原样带回目标 chat(tag 单向 hash 解不回)"
        );
        assert_eq!(picks[0]["path"], "~/peer/worktree");
        assert_eq!(picks[1]["path"], "~/projects");
        let markdowns: Vec<&str> = v["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|element| element["tag"] == "markdown")
            .filter_map(|element| element["content"].as_str())
            .collect();
        assert!(
            markdowns
                .iter()
                .any(|text| text.contains("可复制路径") && text.contains("~/peer/worktree")),
            "每个候选应有非按钮的可复制路径文本: {c}"
        );
        assert!(
            markdowns
                .iter()
                .any(|text| text.contains("可复制路径") && text.contains("~/projects")),
            "每个候选应有非按钮的可复制路径文本: {c}"
        );
        assert!(
            c.contains("当前目录") && c.contains("~/current/worktree"),
            "私信选择器必须明确展示当前工作目录: {c}"
        );
        assert!(c.contains("/cd"), "应保留 /cd 手打兜底提示");
    }

    #[test]
    fn workdir_picker_card_marks_unresolved_current_dir() {
        let c = card_workdir_picker("oc_group", "group", None, &[]);
        assert!(
            c.contains("当前目录") && c.contains("未解析"),
            "当前目录无法解析时不应完全隐去该字段: {c}"
        );
    }

    /// 候选过多时收口到上限并提示手打(同 model 面板的 230099 防线)。
    #[test]
    fn workdir_picker_caps_candidates_and_hints_overflow() {
        let paths: Vec<String> = (0..15).map(|i| format!("~/d{i}")).collect();
        let items: Vec<WorkDirPickItem> = paths
            .iter()
            .map(|p| WorkDirPickItem {
                path: p,
                hint: "fleet 其他",
            })
            .collect();
        let c = card_workdir_picker("oc_g", "group", Some("~/current"), &items);
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        let mut cbs = Vec::new();
        collect_callback_values(&v, &mut cbs);
        let picks = cbs
            .iter()
            .filter(|x| x["action"] == "set_workdir_to")
            .count();
        assert!(picks <= 10, "候选按钮数必须收口,实际 {picks}");
        assert!(c.contains("未列出"), "超出上限应提示可 /cd 手打");
    }

    // ── D:主动私聊回执「来源群」标注 ──────────────────────────────────────

    /// 有群名 → 直接用群名;无群名(None)→ 退到 chat_id 尾段;都能唯一区分不同群。
    #[test]
    fn source_group_note_uses_name_or_falls_back_to_chat_tail() {
        assert_eq!(
            source_group_note(Some("sample开发群2"), "oc_abcdef"),
            "(来自群「sample开发群2」)"
        );
        // 空/空白群名等同拿不到 → 退尾段。
        assert_eq!(
            source_group_note(Some("   "), "oc_tail1234"),
            "(来自群「tail1234」)"
        );
        // None → 尾段(最后一个分隔符后的段)。
        assert_eq!(source_group_note(None, "oc_grpXYZ"), "(来自群「grpXYZ」)");
        // 超长尾段截末 12 字符(段 "0123456789abcdef" 16 位 → 末 12 位),仍稳定可区分。
        assert_eq!(
            source_group_note(None, "oc_0123456789abcdef"),
            "(来自群「456789abcdef」)"
        );
    }

    /// card_with_source_note:标注作为灰字插到卡片正文最前;群名里的 `<` 被转义(外部文本)。
    #[test]
    fn card_with_source_note_prepends_escaped_grey_line() {
        let base = r#"{"schema":"2.0","body":{"elements":[{"tag":"markdown","content":"正文"}]}}"#;
        let note = source_group_note(Some("<script>群"), "oc_x");
        let out = card_with_source_note(base, &note);
        let v: serde_json::Value = serde_json::from_str(&out).expect("仍是合法卡片 JSON");
        let els = v["body"]["elements"].as_array().unwrap();
        // 插到最前面。
        let first = els[0]["content"].as_str().unwrap();
        assert!(first.contains("来自群"), "标注应在正文最前: {first}");
        assert!(first.contains("<font color='grey'>"), "标注应为灰字");
        // 群名里的尖括号标签形态被拆散,不会当成标签吞掉。
        assert!(
            first.contains("<\u{200B}script>群"),
            "群名外部文本必须消毒: {first}"
        );
        // 原正文仍在(第 2 个元素)。
        assert_eq!(els[1]["content"], "正文");
    }

    /// best-effort:非法卡片 JSON / 空 note → 原样返回,绝不因加标注而让卡片发不出去。
    #[test]
    fn card_with_source_note_is_best_effort_on_bad_input() {
        assert_eq!(
            card_with_source_note("not json", "(来自群「x」)"),
            "not json"
        );
        let ok = r#"{"schema":"2.0","body":{"elements":[]}}"#;
        assert_eq!(card_with_source_note(ok, "   "), ok, "空 note 不改卡片");
    }

    /// 简单回执与来源标注必须分成两个卡片元素：正文是外部文本，来源行才含受信 `<font>` 模板。
    /// 若先拼成一个 String 再走 `card_markdown`，安全转义会把 `<font>` 变成用户可见的明文标签。
    #[test]
    fn card_markdown_with_note_keeps_font_as_trusted_template() {
        let note = source_group_note(Some("<b>群"), "oc_x");
        let out = card_markdown_with_note("已把工作目录设为 `~/w`", &note);
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        let elements = value["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[0]["content"], "已把工作目录设为 `~/w`");
        let source = elements[1]["content"].as_str().unwrap();
        assert!(
            source.starts_with("<font color='grey'>"),
            "模板标签必须可渲染:{out}"
        );
        assert!(
            !source.contains("<\u{200B}font"),
            "模板不能被二次消毒成明文:{out}"
        );
        assert!(source.contains("<\u{200B}b>群"), "群名必须消毒:{out}");
        // 空注脚不占一个空元素。
        let bare: serde_json::Value =
            serde_json::from_str(&card_markdown_with_note("正文", "  ")).unwrap();
        assert_eq!(bare["body"]["elements"].as_array().unwrap().len(), 1);
    }

    // ── render_outbound_mentions 测试 ──────────────────────────────────────

    /// 会话镜像必须走 `card_markdown_rendered` 而不是 `card_markdown` —— 这条钉住的是**为什么**。
    ///
    /// 2026-08-12 owner 实测的静默断链:镜像路径把裸文本喂给 `card_markdown`,`<at>` 被当外部
    /// 文本消毒(当时转义成 `&lt;at&gt;`,现在拆散成「`<`+零宽空格+`at`」——形态变了,后果一样),
    /// 飞书于是认定这条消息没 @ 任何人 —— 被 @ 的 bot 的 app **收不到消息事件**,日志里连一行
    /// 拒绝都没有,派单链路静默断掉。两个卡片构造函数的差别只有消毒,但这个差别决定消息投不投得到人。
    #[test]
    fn mirror_must_use_rendered_card_or_the_mention_is_escaped_away() {
        let peers = sample_peers_for_mention();
        let alias = sample_alias_map();
        let rendered = render_external_lark_md("请 @星光 看下这个 review", &peers, &alias);
        assert!(
            rendered.as_str().contains("<at user_id=\"ou_botA\">"),
            "渲染这一步本身要产出真实 at: {}",
            rendered.as_str()
        );
        // 正确路径:受信 <at> 原样留在卡片里 —— 飞书据此产生 mention、投递给被 @ 的 bot。
        let good = card_markdown_rendered(&rendered);
        assert!(
            good.contains("<at user_id=\\\"ou_botA\\\">"),
            "已渲染卡片必须保留 at 标签: {good}"
        );
        // 出事的那条路径:同一段文本走裸 `card_markdown`,at 标签被拆散,飞书看不见 mention。
        let broken = card_markdown(rendered.as_str());
        assert!(
            !broken.contains("<at user_id=\\\"ou_botA\\\">"),
            "裸 card_markdown 会拆散 at——这正是断链成因,若此断言失败说明消毒语义变了: {broken}"
        );
        assert!(
            broken.contains("<\u{200B}at"),
            "消毒后应留下拆散形态: {broken}"
        );
    }

    fn sample_peers_for_mention() -> Vec<crate::peers::Peer> {
        vec![
            crate::peers::Peer {
                open_id: "ou_botA".into(),
                name: "星光".into(),
                ..Default::default()
            },
            crate::peers::Peer {
                open_id: "ou_botB".into(),
                name: "黑魔术师".into(),
                ..Default::default()
            },
            crate::peers::Peer {
                open_id: "ou_botC".into(),
                name: "arcadia".into(),
                ..Default::default()
            },
        ]
    }

    fn sample_alias_map() -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        m.insert("starlight".into(), "星光".into());
        m.insert("black-magician".into(), "黑魔术师".into());
        m
    }

    #[test]
    fn render_outbound_mentions_name_exact_match() {
        let peers = sample_peers_for_mention();
        let alias = sample_alias_map();
        // 直接 name 命中
        let result = render_outbound_mentions("请 @星光 帮忙看看", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botA\">星光</at>"),
            "name 精确匹配应替换: {result}"
        );
        assert!(result.contains("请 "), "前后文保留");
        assert!(result.contains(" 帮忙看看"), "前后文保留");
    }

    #[test]
    fn render_outbound_mentions_alias_match() {
        let peers = sample_peers_for_mention();
        let alias = sample_alias_map();
        // alias → name 命中
        let result = render_outbound_mentions("@starlight 来一下", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botA\">星光</at>"),
            "alias 匹配应转成 name 再替换: {result}"
        );
        // 显示名是 name("星光")而非 alias("starlight")
        assert!(!result.contains("@starlight"), "alias 应被替换掉");
    }

    #[test]
    fn render_outbound_mentions_mixed_text() {
        let peers = sample_peers_for_mention();
        let alias = sample_alias_map();
        // 混合:部分 @bot + 普通文本 + 标点
        let result = render_outbound_mentions(
            "我让 @星光 和 @arcadia 合作,@黑魔术师 也来。",
            &peers,
            &alias,
        );
        assert!(
            result.contains("<at user_id=\"ou_botA\">星光</at>"),
            "星光应被替换"
        );
        assert!(
            result.contains("<at user_id=\"ou_botC\">arcadia</at>"),
            "arcadia 应被替换"
        );
        assert!(
            result.contains("<at user_id=\"ou_botB\">黑魔术师</at>"),
            "黑魔术师应被替换"
        );
        assert!(result.contains("我让 "), "前文保留");
        assert!(result.contains(" 和 "), "中间文本保留");
        assert!(result.contains(" 合作,"), "标点保留");
        assert!(result.contains(" 也来。"), "尾部标点保留");
    }

    #[test]
    fn render_outbound_mentions_no_match_keeps_original() {
        let peers = sample_peers_for_mention();
        let alias = sample_alias_map();
        // 不认识的 @ → 保留原样
        let result = render_outbound_mentions("@unknown 你好", &peers, &alias);
        assert_eq!(result, "@unknown 你好", "未命中应保留原样");
    }

    #[test]
    fn render_outbound_mentions_does_not_replace_email() {
        let peers = sample_peers_for_mention();
        let alias = sample_alias_map();
        // 邮箱格式 user@x → 不替换
        let result =
            render_outbound_mentions("发邮件到 user@example.com 或 cc @星光", &peers, &alias);
        assert!(
            result.contains("user@example.com"),
            "邮箱不应被替换: {result}"
        );
        // 但后面的 @星光 仍应被替换
        assert!(
            result.contains("<at user_id=\"ou_botA\">星光</at>"),
            "合法 @mention 仍应替换: {result}"
        );
    }

    #[test]
    fn render_outbound_mentions_trailing_punctuation_preserved() {
        let peers = sample_peers_for_mention();
        let alias = sample_alias_map();
        // 尾部标点保留:@星光， → <at>星光</at>，
        let result = render_outbound_mentions("@星光，帮我看看", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botA\">星光</at>，"),
            "尾部中文逗号应保留: {result}"
        );
        // 英文标点
        let result = render_outbound_mentions("@星光, help me", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botA\">星光</at>,"),
            "尾部英文逗号应保留: {result}"
        );
    }

    #[test]
    fn render_outbound_mentions_empty_inputs() {
        let peers = sample_peers_for_mention();
        let alias = sample_alias_map();
        // 空文本
        assert_eq!(render_outbound_mentions("", &peers, &alias), "");
        // 无 @ 的文本
        assert_eq!(
            render_outbound_mentions("纯文本无 mention", &peers, &alias),
            "纯文本无 mention"
        );
        // 空 peers
        assert_eq!(
            render_outbound_mentions("@星光 你好", &[], &alias),
            "@星光 你好",
            "空 peers 不替换"
        );
    }

    /// 在 sample 基础上加一个带空格的多词名 peer(复现 2026-08 「@Flare 5」原样发出的 bug:
    /// 旧实现按空白分词只拿到「Flare」,名单里查不到,mention 永远转不成 `<at>`)。
    fn peers_with_multiword_name() -> Vec<crate::peers::Peer> {
        let mut peers = sample_peers_for_mention();
        peers.push(crate::peers::Peer {
            open_id: "ou_botD".into(),
            name: "Flare 5".into(),
            ..Default::default()
        });
        peers
    }

    #[test]
    fn render_outbound_mentions_multiword_name_with_space() {
        let peers = peers_with_multiword_name();
        let alias = sample_alias_map();

        // 名字含空格也要能命中——这是本次修复的主场景。
        let result = render_outbound_mentions("请 @Flare 5 帮忙 review", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botD\">Flare 5</at>"),
            "带空格的名字应整体命中: {result}"
        );
        assert!(result.contains(" 帮忙 review"), "名字之后的正文保留");

        // 同一段文本里,单词名(含 CJK 名)不回归。
        let result = render_outbound_mentions("@Flare 5 和 @星光 一起看", &peers, &alias);
        assert!(result.contains("<at user_id=\"ou_botD\">Flare 5</at>"));
        assert!(
            result.contains("<at user_id=\"ou_botA\">星光</at>"),
            "单词名不因多词名支持而回归: {result}"
        );
    }

    #[test]
    fn render_outbound_mentions_multiword_name_boundaries() {
        let peers = peers_with_multiword_name();
        let alias = sample_alias_map();

        // 句尾:名字后面什么都没有。
        assert_eq!(
            render_outbound_mentions("收到请回复 @Flare 5", &peers, &alias),
            "收到请回复 <at user_id=\"ou_botD\">Flare 5</at>"
        );
        // 名字后紧跟标点(不能要求名字后必须是空白)。
        let result = render_outbound_mentions("@Flare 5，收到吗", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botD\">Flare 5</at>，收到吗"),
            "名字后接中文标点应命中且标点保留: {result}"
        );
        // 名字后紧跟中文。
        let result = render_outbound_mentions("@Flare 5请看一下", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botD\">Flare 5</at>请看一下"),
            "名字后直接接中文应命中: {result}"
        );
        // 名字后紧跟换行。
        let result = render_outbound_mentions("@Flare 5\n下一行", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botD\">Flare 5</at>\n下一行"),
            "名字后接换行应命中: {result}"
        );
    }

    #[test]
    fn render_outbound_mentions_multiword_lookalike_stays_verbatim() {
        let peers = peers_with_multiword_name();
        let alias = sample_alias_map();

        // 空格后的部分对不上名单 → 整段保持原样(@Flare 不是任何 peer 的完整名字)。
        assert_eq!(
            render_outbound_mentions("@Flare X 你好", &peers, &alias),
            "@Flare X 你好",
            "不在名单内的 @ 不许被消费"
        );
        // 只写名字的前半截也不命中:匹配的是集合内的完整名字,不是任意前缀。
        assert_eq!(
            render_outbound_mentions("@Flare 在吗", &peers, &alias),
            "@Flare 在吗",
            "「Flare」不是完整 peer 名,不该匹配「Flare 5」"
        );
    }

    #[test]
    fn render_outbound_mentions_prefix_peers_longest_wins() {
        // 两个 peer 名字互为前缀:必须取最长命中,否则「Flare 5x」会被错拆成「Flare 5」+ x。
        let mut peers = peers_with_multiword_name(); // 含 "Flare 5"(ou_botD)
        peers.push(crate::peers::Peer {
            open_id: "ou_botE".into(),
            name: "Flare 5x".into(),
            ..Default::default()
        });
        let alias = sample_alias_map();

        let result = render_outbound_mentions("@Flare 5x 接手", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botE\">Flare 5x</at>"),
            "互为前缀时取最长命中: {result}"
        );
        assert!(
            !result.contains("ou_botD"),
            "短名字不该抢走长名字的 mention: {result}"
        );
        // 短的那个单独出现时仍归短的。
        let result = render_outbound_mentions("@Flare 5 接手", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botD\">Flare 5</at> 接手"),
            "精确写短名字时命中短的: {result}"
        );
    }

    /// 尾边界回归:名单里**只有**短名字「Flare 5」(没有 Flare 5x)时,`@Flare 5x` 指的是另一个
    /// 不在名单的名字,不许被劈成 `<at>Flare 5</at>x`。上面 prefix 测试因为同时放入了长名字,
    /// 覆盖不到这个过度匹配场景。
    #[test]
    fn render_outbound_mentions_word_char_tail_blocks_short_name_overmatch() {
        let peers = peers_with_multiword_name(); // 只含 "Flare 5",无更长的同前缀名
        let alias = sample_alias_map();

        // 候选末字符('5')与后继字符同为 ASCII 单词字符 → 不消费,整段原样保留。
        for text in ["@Flare 5x 接手", "@Flare 50 接手", "@Flare 5_x 接手"] {
            assert_eq!(
                render_outbound_mentions(text, &peers, &alias),
                text,
                "长名字不在名单时不许过度匹配消费"
            );
        }
        // 后继是 CJK,不构成 ASCII 词延续 → 仍命中。
        let result = render_outbound_mentions("@Flare 5请看", &peers, &alias);
        assert!(
            result.contains("<at user_id=\"ou_botD\">Flare 5</at>请看"),
            "CJK 后继不受尾边界限制: {result}"
        );
    }

    #[test]
    fn external_markdown_escapes_text_but_keeps_trusted_mention() {
        let peers = sample_peers_for_mention();
        let alias = sample_alias_map();
        let rendered = render_external_lark_md("<img src=x> A & B，请 @星光", &peers, &alias);

        assert!(rendered.as_str().contains("<\u{200B}img src=x>"));
        assert!(
            rendered.as_str().contains("A & B"),
            "`&` 原样保留,不出实体: {}",
            rendered.as_str()
        );
        assert!(
            rendered
                .as_str()
                .contains("<at user_id=\"ou_botA\">星光</at>"),
            "summon 插入的 mention 不应被二次转义: {}",
            rendered.as_str()
        );

        let card: serde_json::Value =
            serde_json::from_str(&card_markdown_rendered(&rendered)).unwrap();
        let content = card["elements"][0]["content"].as_str().unwrap();
        assert!(content.contains("<\u{200B}img src=x>"));
        assert!(content.contains("<at user_id=\"ou_botA\">星光</at>"));
        assert!(!content.contains("<\u{200B}at"), "受信 at 不得被二次消毒");
    }

    #[test]
    fn mention_matches_peer_name_containing_ampersand() {
        // 既有限制(A 分支 fix/mention-name-with-space 报告过):旧实现先把 `&` 转成 `&amp;`
        // 再做 @ 匹配,peer 名含 `&` 时文本形态与名单键对不上,永远匹配不到。`&` 不再转义后
        // 这条限制自然消失——钉住,防止将来把 `&` 转义加回来时无声退化。
        // (名含「`<`+字母」的 peer 仍会因零宽空格拆散而匹配不到,属剩余已知限制。)
        let peers = vec![crate::peers::Peer {
            open_id: "ou_rd".into(),
            name: "R&D 平台".into(),
            ..Default::default()
        }];
        let alias = std::collections::HashMap::new();
        let rendered = render_external_lark_md("请 @R&D 平台 看一下", &peers, &alias);
        // 标签内文本仍走 `escape_at_content` 防注入(信任边界不动),故内文是 `R&amp;D`。
        assert!(
            rendered
                .as_str()
                .contains("<at user_id=\"ou_rd\">R&amp;D 平台</at>"),
            "含 & 的 peer 名应能命中出站 @: {}",
            rendered.as_str()
        );
    }

    #[test]
    fn mention_escapes_hostile_cached_identity_fields() {
        let peers = vec![crate::peers::Peer {
            open_id: "ou_bad\" onmouseover=\"x".into(),
            name: "坏</at><at user_id=\"ou_victim\">人".into(),
            ..Default::default()
        }];
        let alias = std::collections::HashMap::from([(
            "bad".to_string(),
            "坏</at><at user_id=\"ou_victim\">人".to_string(),
        )]);
        let rendered = render_external_lark_md("@bad", &peers, &alias);

        assert!(rendered
            .as_str()
            .contains("ou_bad&quot; onmouseover=&quot;x"));
        assert!(rendered
            .as_str()
            .contains("坏&lt;/at&gt;&lt;at user_id=&quot;ou_victim&quot;&gt;人"));
        assert_eq!(rendered.as_str().matches("<at ").count(), 1);
    }

    #[test]
    fn rendered_stream_clip_never_splits_trusted_mention_tag() {
        let peers = vec![crate::peers::Peer {
            open_id: "ou_bot".into(),
            name: "星光".into(),
            ..Default::default()
        }];
        let alias = std::collections::HashMap::from([("bot".to_string(), "星光".to_string())]);
        // mention 从第 91 字节开始，100 字节裁剪点必落在 `<at user_id=...>` 内。
        let rendered =
            render_external_lark_md(&format!("{} @bot 后文", "a".repeat(90)), &peers, &alias);
        let clipped = rendered.clipped_for_stream(100);

        assert!(clipped.as_str().contains("回复过长"));
        assert!(!clipped.as_str().contains("<at "));
        assert!(!clipped.as_str().contains("</at>"));
        assert!(clipped.as_str().starts_with(&"a".repeat(90)));
    }

    #[test]
    fn parses_humans_and_bots_from_chat_members() {
        // 真机样本形状(2026-08-05 SampleML答疑群):人无 app_id,bot 带非空 app_id。
        let raw = serde_json::json!({
            "data": {"items": [
                {"member_id": "ou_human", "name": "李四", "member_id_type": "open_id"},
                {"member_id": "ou_bot", "name": "Arcadia", "app_id": "cli_xxx"},
                {"member_id": "", "name": "空 id 应跳过"},
                {"member_id": "ou_noname", "name": ""},
                {"member_id": "ou_botempty", "name": "空 app_id 算人", "app_id": ""}
            ]}
        });
        let members = parse_chat_members(&raw);
        assert_eq!(members.len(), 3, "空 open_id / 空 name 的项必须丢掉");
        assert_eq!(members[0].open_id, "ou_human");
        assert_eq!(members[0].name, "李四");
        assert!(
            !members[0].is_bot,
            "人不该被判成 bot——这是名字解析退化成占位的老 bug 根源"
        );
        assert!(members[1].is_bot, "非空 app_id 才是 bot");
        assert!(!members[2].is_bot, "空 app_id 按人处理");
    }

    #[test]
    fn parse_chat_members_tolerates_malformed_payload() {
        // 网络/权限异常时飞书会回结构不同的 body;解析不许 panic,只返回空。
        assert!(parse_chat_members(&serde_json::json!({})).is_empty());
        assert!(
            parse_chat_members(&serde_json::json!({"data": {"items": "not-an-array"}})).is_empty()
        );
    }
}

#[cfg(test)]
mod claude_permission_card_tests {
    use super::*;
    use crate::core_port::{AskQuestion, ClaudeHookRequest};
    use serde_json::{json, Value};

    fn request(input: Value, agent: Option<&str>) -> ClaudeHookRequest {
        ClaudeHookRequest {
            tool_name: "Bash".into(),
            tool_input: input,
            agent_id: agent.map(str::to_string),
            agent_type: agent.map(|_| "general-purpose".to_string()),
        }
    }

    fn callbacks(card: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        fn walk(v: &Value, out: &mut Vec<Value>) {
            match v {
                Value::Object(map) => {
                    if let Some(behaviors) = map.get("behaviors").and_then(|b| b.as_array()) {
                        for b in behaviors {
                            if let Some(value) = b.get("value") {
                                out.push(value.clone());
                            }
                        }
                    }
                    map.values().for_each(|v| walk(v, out));
                }
                Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        walk(card, &mut out);
        out
    }

    #[test]
    fn main_agent_card_shows_command_buttons_and_auto_deny_window() {
        let req = request(
            json!({"command": "bash -c 'rm -f $1'", "description": "清理"}),
            None,
        );
        let card = card_claude_permission(&ClaudePermissionCard {
            gate_id: "gate-1",
            requester_open_id: "ou_owner",
            request: &req,
        });
        assert!(card.contains("rm -f $1"), "命令原样上卡");
        assert!(card.contains("发起方:主 agent"));
        assert!(card.contains("清理"));
        assert!(card.contains("2 分钟"), "主 agent 要提示自动拒绝窗口");
        assert!(card.contains("<at id=ou_owner></at>"));
        let v: Value = super::tests::workflow_card(&card).unwrap();
        let actions: Vec<_> = callbacks(&v)
            .iter()
            .map(|c| {
                (
                    c["action"].as_str().unwrap().to_string(),
                    c["gate_id"].clone(),
                )
            })
            .collect();
        assert_eq!(
            actions,
            vec![
                ("gate_perm_allow".to_string(), json!("gate-1")),
                ("gate_perm_deny".to_string(), json!("gate-1")),
            ]
        );
        assert!(
            matches!(
                CardAction::from_action("gate_perm_allow"),
                CardAction::GateFamily
            ),
            "授权卡动作必须路由进闸门 handler"
        );
    }

    #[test]
    fn subagent_card_names_agent_type_and_skips_main_only_note() {
        let req = request(json!({"command": "rm x"}), Some("a1"));
        let card = card_claude_permission(&ClaudePermissionCard {
            gate_id: "g",
            requester_open_id: "",
            request: &req,
        });
        assert!(card.contains("子 agent · general-purpose"));
        assert!(!card.contains("2 分钟"));
        assert!(!card.contains("<at"), "没有提问人就不 @");
    }

    #[test]
    fn settled_card_keeps_content_disables_permission_controls_and_names_outcome() {
        let req = request(json!({"command": "rm x"}), None);
        let spec = ClaudePermissionCard {
            gate_id: "g",
            requester_open_id: "",
            request: &req,
        };
        for (end, text) in [
            (
                ClaudePermissionEnd::Allowed {
                    operator_open_id: "ou_a",
                },
                "已允许",
            ),
            (
                ClaudePermissionEnd::Denied {
                    operator_open_id: "ou_a",
                },
                "已拒绝",
            ),
            (
                ClaudePermissionEnd::Terminal {
                    operator_open_id: "ou_a",
                },
                "已交给终端处理",
            ),
            (ClaudePermissionEnd::Elsewhere, "已在别处结束"),
        ] {
            let card = card_claude_permission_settled(&spec, end);
            assert!(card.contains("rm x"), "定格保留原内容");
            assert!(card.contains(text));
            assert!(!card.contains("2 分钟"), "定格后不再提示倒计时");
            let v: Value = super::tests::workflow_card(&card).unwrap();
            assert!(callbacks(&v).is_empty(), "定格后按钮不能再回调");
        }
    }

    #[test]
    fn huge_inputs_stay_under_card_limit_and_keep_both_ends() {
        let content: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let req = ClaudeHookRequest {
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "/tmp/x", "content": content, "extra": {"k": [1, 2]}}),
            agent_id: None,
            agent_type: None,
        };
        let card = card_claude_permission(&ClaudePermissionCard {
            gate_id: "g",
            requester_open_id: "",
            request: &req,
        });
        assert!(
            card.len() < CARD_TOTAL_SAFE_LIMIT,
            "卡片 {} 字节",
            card.len()
        );
        assert!(card.contains("/tmp/x"));
        assert!(card.contains("line 0") && card.contains("line 4999"));
        assert!(card.contains("中间省略"));
        let file = card.find("/tmp/x").unwrap();
        let body = card.find("line 0").unwrap();
        assert!(file < body, "file_path 排在其它字段前面");
        assert!(!claude_permission_fully_shown(&req));
    }

    /// 看不全就不能批准:省略过的请求卡上没有「允许」,只有「拒绝」与「到终端处理」。
    /// 单行超长(结尾整段被截掉)与多行省略中间两种都算。
    #[test]
    fn a_request_not_fully_shown_cannot_be_allowed_from_the_card() {
        let one_line = format!("bash -c '{} ; rm -rf $TARGET'", "true ".repeat(3000));
        let many_lines: String = (0..4000).map(|i| format!("{i}\n")).collect();
        for input in [
            json!({"command": one_line}),
            json!({"command": "ls", "content": many_lines}),
        ] {
            let req = request(input, Some("a1"));
            assert!(!claude_permission_fully_shown(&req));
            let card = card_claude_permission(&ClaudePermissionCard {
                gate_id: "g",
                requester_open_id: "",
                request: &req,
            });
            let v: Value = super::tests::workflow_card(&card).unwrap();
            let actions: Vec<_> = callbacks(&v)
                .iter()
                .map(|c| c["action"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(actions, vec!["gate_perm_deny", "gate_perm_terminal"]);
            assert!(card.contains("不能在这里批准"));
        }
        let short = request(json!({"command": "rm x"}), None);
        assert!(claude_permission_fully_shown(&short));
    }

    /// 入参不是对象、`description` 不是字符串时,内容照样上卡,不会「看不到却能批准」。
    #[test]
    fn unusual_input_shapes_are_still_shown_on_the_card() {
        for (input, needle) in [
            (json!("rm -rf $T"), "rm -rf $T"),
            (json!(["rm", "-rf", "/x"]), "/x"),
            (
                json!({"command": "ls", "description": {"hidden": "rm -rf /y"}}),
                "rm -rf /y",
            ),
        ] {
            let req = request(input, None);
            let card = card_claude_permission(&ClaudePermissionCard {
                gate_id: "g",
                requester_open_id: "",
                request: &req,
            });
            assert!(card.contains(needle), "{needle} 必须上卡: {card}");
            assert!(claude_permission_fully_shown(&req));
        }
    }

    #[test]
    fn code_fences_in_input_cannot_break_out_of_the_block() {
        let req = request(
            json!({"command": "echo ```\n**bold** <at id=x></at>"}),
            None,
        );
        let card = card_claude_permission(&ClaudePermissionCard {
            gate_id: "g",
            requester_open_id: "",
            request: &req,
        });
        assert!(!card.contains("echo ```"), "原文的 ``` 必须被拆开");
        assert!(!card.contains("<at id=x>"), "原文里的 at 标签必须转义");
    }

    #[test]
    fn multi_select_question_renders_multi_select_control() {
        let questions = vec![AskQuestion {
            question: "Toppings?".into(),
            options: vec!["A".into(), "B".into()],
            allow_other: true,
            multi_select: true,
            ..Default::default()
        }];
        let card = card_chat_ask(&ChatAskCard {
            gate_id: "g",
            requester_open_id: "",
            questions: &questions,
        });
        assert!(card.contains("multi_select_static"));
        assert!(!card.contains("<at"), "没有提问人就不 @");
    }
}
