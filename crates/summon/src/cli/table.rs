//! 终端表格渲染(纯函数,无 IO)。会话列表从「一条会话 6 行块」改成「一条会话 2 行表格」:
//! 定长字段按列对齐,可照抄指令跨全部列排在下面,上下用完整 box-drawing 分隔线包围。
//!
//! 两条硬约束(踩过就知道为什么):
//! - **列宽按裸文本算,着色在 pad 之后套**。ANSI 序列零显示宽,先上色再 pad 必然错位,
//!   所以本模块只收 `(裸文本, 语义色)`,自己不解析 ANSI。
//! - **显示宽用 `unicode-width`**。中文/emoji 群名宽度是 2,`chars().count()` 会让含中文的行
//!   短一截;自维护宽度表在 emoji 与组合字符上必翻车。
//!
//! 设计文档:`docs/spec/2026-08-05-cli-session-table-design.md`

use super::style;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// 单元格的语义色(渲染时才套,见模块注释)。
#[derive(Clone, Copy)]
pub(crate) enum Paint {
    /// 内容值:保持终端前景色,不染。
    None,
    /// 标题 / agent 名(加粗)。
    Heading,
    /// 字段标签 / 弱提示(暗灰)。
    Label,
    /// 可直接复制运行的指令(青)。
    Command,
    /// 会话状态(按色相)。
    Status(style::Hue),
}

/// 一个单元格:裸文本 + 语义色。
#[derive(Clone)]
pub(crate) struct Cell {
    text: String,
    paint: Paint,
}

impl Cell {
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            paint: Paint::None,
        }
    }

    pub(crate) fn heading(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            paint: Paint::Heading,
        }
    }

    pub(crate) fn label(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            paint: Paint::Label,
        }
    }

    pub(crate) fn command(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            paint: Paint::Command,
        }
    }

    pub(crate) fn status(text: impl Into<String>, hue: style::Hue) -> Self {
        Self {
            text: text.into(),
            paint: Paint::Status(hue),
        }
    }

    /// 裸文本的显示宽。
    fn width(&self) -> usize {
        self.text.width()
    }

    /// pad 到 `width` 显示宽后上色。**顺序不可换**(ANSI 零宽,先上色再 pad 会错位)。
    fn render(&self, width: usize) -> String {
        let padded = pad(&self.text, width);
        match self.paint {
            Paint::None => padded,
            Paint::Heading => style::heading(&padded),
            Paint::Label => style::label(&padded),
            Paint::Command => style::command(&padded),
            Paint::Status(hue) => style::status(&padded, hue),
        }
    }
}

/// 一条记录:定长字段行 + 可选的跨列续行(该会话的可照抄指令)。
pub(crate) struct Row {
    cells: Vec<Cell>,
    span: Option<Cell>,
}

impl Row {
    pub(crate) fn new(cells: Vec<Cell>) -> Self {
        Self { cells, span: None }
    }

    /// 挂一条跨全部列的续行。
    pub(crate) fn with_span(mut self, cell: Cell) -> Self {
        self.span = Some(cell);
        self
    }
}

/// 行的列结构 —— 决定它上下分隔线的 junction 字符。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// 按列分格(表头 / 数据行)。
    Split,
    /// 跨全部列(指令续行)。
    Span,
}

pub(crate) struct Table {
    headers: Vec<Cell>,
    rows: Vec<Row>,
    /// 每列的显示宽上限(`None` 不限)。飞书群名无长度上界,不封顶能把总宽拉到 200+。
    caps: Vec<Option<usize>>,
}

impl Table {
    pub(crate) fn new(headers: Vec<&str>) -> Self {
        let caps = vec![None; headers.len()];
        Self {
            headers: headers.into_iter().map(Cell::label).collect(),
            rows: Vec::new(),
            caps,
        }
    }

    /// 给某列封顶(超出截尾加 `…`)。列号越界时静默忽略 —— 表头是编译期常量,
    /// 越界只可能是调用方笔误,不该让一个显示宽约束把命令打挂。
    pub(crate) fn cap(mut self, col: usize, max: usize) -> Self {
        if let Some(slot) = self.caps.get_mut(col) {
            *slot = Some(max);
        }
        self
    }

    pub(crate) fn push(&mut self, row: Row) {
        self.rows.push(row);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 每列显示宽 = max(表头, 各行该列),再受 `caps` 封顶。
    fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.headers.iter().map(Cell::width).collect();
        for row in &self.rows {
            for (i, cell) in row.cells.iter().enumerate() {
                if let Some(w) = widths.get_mut(i) {
                    *w = (*w).max(cell.width());
                }
            }
        }
        for (i, w) in widths.iter_mut().enumerate() {
            if let Some(Some(cap)) = self.caps.get(i) {
                *w = (*w).min(*cap);
            }
        }
        // 续行(可照抄指令)也是内容,窄于它就把表格撑宽 —— 截断的命令跑不通,而让它溢出会把
        // 右边框撞成犬牙。差额补在最后一列,右侧留白远好过两种坏结局。
        let needed = self
            .rows
            .iter()
            .filter_map(|r| r.span.as_ref())
            .map(Cell::width)
            .max();
        if let (Some(needed), Some(last)) = (needed, widths.len().checked_sub(1)) {
            let avail = span_width(&widths).saturating_sub(2);
            if needed > avail {
                widths[last] += needed - avail;
            }
        }
        widths
    }

    pub(crate) fn render(&self) -> String {
        let widths = self.widths();
        // 跨列续行的可用内宽:各列宽 + 每列两侧 padding + 被吃掉的内部竖线。
        let inner = span_width(&widths);
        let mut out = String::new();

        out.push_str(&divider(&widths, None, Some(Shape::Split)));
        out.push('\n');
        out.push_str(&split_line(&self.headers, &widths, &self.caps));
        out.push('\n');

        let mut prev = Shape::Split;
        for row in &self.rows {
            out.push_str(&divider(&widths, Some(prev), Some(Shape::Split)));
            out.push('\n');
            out.push_str(&split_line(&row.cells, &widths, &self.caps));
            out.push('\n');
            prev = Shape::Split;
            if let Some(span) = &row.span {
                out.push_str(&divider(&widths, Some(prev), Some(Shape::Span)));
                out.push('\n');
                out.push_str(&span_line(span, inner));
                out.push('\n');
                prev = Shape::Span;
            }
        }
        out.push_str(&divider(&widths, Some(prev), None));
        out
    }
}

/// 跨列续行的可用内宽:列宽之和 + 每列两侧各 1 空格 + 被跨掉的 n-1 条内部竖线。
fn span_width(widths: &[usize]) -> usize {
    let sum: usize = widths.iter().sum();
    sum + widths.len() * 2 + widths.len().saturating_sub(1)
}

/// 分列行:`│ a │ b │`。
fn split_line(cells: &[Cell], widths: &[usize], caps: &[Option<usize>]) -> String {
    let mut out = String::from("│");
    for (i, w) in widths.iter().enumerate() {
        let blank = Cell::plain("");
        let cell = cells.get(i).unwrap_or(&blank);
        // 封顶列先截尾再 pad —— 截断后宽度必 <= cap,pad 才补得回来。
        let cell = match caps.get(i).copied().flatten() {
            Some(cap) if cell.width() > cap => Cell {
                text: truncate(&cell.text, cap),
                paint: cell.paint,
            },
            _ => cell.clone(),
        };
        out.push(' ');
        out.push_str(&cell.render(*w));
        out.push_str(" │");
    }
    out
}

/// 跨列续行:`│ cmd ... │`。内宽由 `Table::widths` 保证不小于最长命令,故这里只 pad 不截。
fn span_line(cell: &Cell, inner: usize) -> String {
    let body = inner.saturating_sub(2);
    format!("│ {} │", cell.render(body))
}

/// 分隔线。`above`/`below` 为 `None` 表示表格边界。junction 只由上下两行是否**分列**决定:
/// 上下都分列 → `┼`;只上分列(下面是跨列续行或到底) → `┴`;只下分列(上面是续行或到顶) → `┬`;
/// 都不分列(续行结尾) → 直线不打点。
fn divider(widths: &[usize], above: Option<Shape>, below: Option<Shape>) -> String {
    let up = above == Some(Shape::Split);
    let down = below == Some(Shape::Split);
    let junction = match (up, down) {
        (true, true) => '┼',
        (true, false) => '┴',
        (false, true) => '┬',
        (false, false) => '─',
    };
    let left = match (above, below) {
        (None, _) => '┌',
        (_, None) => '└',
        _ => '├',
    };
    let right = match (above, below) {
        (None, _) => '┐',
        (_, None) => '┘',
        _ => '┤',
    };

    let mut out = String::new();
    out.push(left);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            out.push(junction);
        }
        for _ in 0..(w + 2) {
            out.push('─');
        }
    }
    out.push(right);
    style::label(&out)
}

/// 右侧补空格到 `width` 显示宽。已超宽则原样返回(不截断,截断由调用方按 cap 决定)。
fn pad(text: &str, width: usize) -> String {
    let w = text.width();
    if w >= width {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(width - w))
}

/// 按**显示宽**截断到 `max`,末尾加 `…`(省略号本身占 1 宽)。按 char 累加宽度,
/// 不会把一个中文字符劈成半个。
fn truncate(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let budget = max - 1;
    let mut out = String::new();
    let mut w = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试里统一比裸文本 —— 单测进程 stdout 非 tty,`style::*` 恒不上色。
    fn table() -> Table {
        let mut t = Table::new(vec!["AGENT", "STATE"]);
        t.push(
            Row::new(vec![Cell::plain("arcadia"), Cell::plain("live")]).with_span(Cell::plain(
                "summon agents attach arcadia --chat p00000000a11ce04f",
            )),
        );
        t
    }

    #[test]
    fn junction_follows_neighbour_shapes() {
        let w = vec![3, 2];
        // 顶边:上面没有行,下面是表头(分列)→ ┬
        assert_eq!(divider(&w, None, Some(Shape::Split)), "┌─────┬────┐");
        // 分列 → 分列
        assert_eq!(
            divider(&w, Some(Shape::Split), Some(Shape::Split)),
            "├─────┼────┤"
        );
        // 分列 → 跨列:收拢
        assert_eq!(
            divider(&w, Some(Shape::Split), Some(Shape::Span)),
            "├─────┴────┤"
        );
        // 跨列 → 分列:展开
        assert_eq!(
            divider(&w, Some(Shape::Span), Some(Shape::Split)),
            "├─────┬────┤"
        );
        // 底边且上面是分列行 → ┴
        assert_eq!(divider(&w, Some(Shape::Split), None), "└─────┴────┘");
        // 底边且上面是跨列续行 → 一条直线,不打点
        assert_eq!(divider(&w, Some(Shape::Span), None), "└──────────┘");
    }

    #[test]
    fn span_line_width_matches_split_line() {
        let rendered = table().render();
        let widths: Vec<usize> = rendered.lines().map(|l| l.width()).collect();
        // 每行显示宽必须一致,否则右边框会犬牙交错。
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "行宽不齐: {widths:?}"
        );
    }

    /// 回归:续行(命令)比列内容宽时,表格必须被撑宽而不是让续行溢出撞坏右边框。
    #[test]
    fn long_span_widens_table_instead_of_overflowing() {
        let mut t = Table::new(vec!["A", "B"]);
        let cmd = "summon agents kill soul-blue-wing-g00000000a11ce02a --reset";
        t.push(Row::new(vec![Cell::plain("x"), Cell::plain("y")]).with_span(Cell::plain(cmd)));
        let rendered = t.render();
        let widths: Vec<usize> = rendered.lines().map(|l| l.width()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "撑宽后行宽不齐: {widths:?}"
        );
        // 命令一字不少地留在行内。
        assert!(rendered.contains(cmd));
    }

    #[test]
    fn cjk_column_aligns_by_display_width() {
        let mut t = Table::new(vec!["CHAT"]);
        t.push(Row::new(vec![Cell::plain("周末读书分享会")]));
        t.push(Row::new(vec![Cell::plain("私聊")]));
        let rendered = t.render();
        let widths: Vec<usize> = rendered.lines().map(|l| l.width()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "中文行宽不齐: {widths:?}"
        );
        // 列宽由最宽的中文值(14 显示宽)决定,不是字符数(7)。
        assert_eq!(widths[0], 14 + 4);
    }

    #[test]
    fn cap_truncates_by_display_width_not_char_count() {
        assert_eq!(truncate("周末读书分享会", 24), "周末读书分享会"); // 14 宽,不截
        assert_eq!(truncate("周末读书分享会", 8), "周末读…"); // 3 字(6 宽)+ …
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("每天", 1), "…"); // 预算不足只剩省略号
    }

    #[test]
    fn capped_column_keeps_rows_aligned() {
        let mut t = Table::new(vec!["CHAT"]).cap(0, 6);
        t.push(Row::new(vec![Cell::plain("周末读书分享会超长群名")]));
        t.push(Row::new(vec![Cell::plain("私聊")]));
        let rendered = t.render();
        let widths: Vec<usize> = rendered.lines().map(|l| l.width()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "截断后行宽不齐: {widths:?}"
        );
        assert_eq!(widths[0], 6 + 4);
        assert!(rendered.contains('…'));
    }

    #[test]
    fn header_width_wins_when_values_shorter() {
        let mut t = Table::new(vec!["AGENT"]);
        t.push(Row::new(vec![Cell::plain("a")]));
        let rendered = t.render();
        // 列宽至少容得下表头本身。
        assert!(rendered.lines().next().unwrap().width() >= 5 + 4);
    }

    #[test]
    fn missing_cells_render_blank_not_panic() {
        let mut t = Table::new(vec!["A", "B", "C"]);
        t.push(Row::new(vec![Cell::plain("only-one")]));
        let rendered = t.render();
        let widths: Vec<usize> = rendered.lines().map(|l| l.width()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "缺列时行宽不齐: {widths:?}"
        );
    }

    #[test]
    fn span_row_reaches_both_borders() {
        let rendered = table().render();
        let span = rendered
            .lines()
            .find(|l| l.contains("summon agents attach"))
            .expect("续行应存在");
        assert!(span.starts_with("│ ") && span.ends_with(" │"));
        // 续行内部不该残留竖线 —— 它是跨列的。
        assert_eq!(span.matches('│').count(), 2);
    }

    #[test]
    fn pad_never_shrinks_oversized_text() {
        assert_eq!(pad("abcdef", 3), "abcdef");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("私", 4), "私  "); // 2 显示宽 + 2 空格
    }
}
