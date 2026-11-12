//! 保留注释的 YAML 按键路径改写(纯:不碰文件)。`config.yaml` 与 `agent.yaml` 共用。
//!
//! why 不走 serde 往返:这两份文件是 owner 手写的,带大段说明注释、对齐和自己的引号习惯,
//! serde_yaml 序列化一遍就全没了。所以这里只做「按行定位 + 只替换目标那几行」:认块映射、
//! 块序列和单行值;判不准的写法(锚点与引用、多文档、行内映射作父级、跨行的值、块标量)
//! 一律拒绝,由调用方引导到原文模式,不去猜。
//!
//! 每一步改写之后都用 serde_yaml 重新解析,与「在原文的解析结果上做同一个改动」逐值比对,
//! 不一致就拒绝 —— 行级手术哪怕出了一点偏差,也不会把错的文本交给调用方。

use serde_yaml::{Mapping, Value};
use std::ops::Range;

/// 键路径:从顶层起逐级的键名。
pub type Path = Vec<String>;

/// 点分路径 → [`Path`]。只给不含点的键用(chat id、环境变量名都不含点)。
pub fn path(dotted: &str) -> Path {
    dotted.split('.').map(str::to_string).collect()
}

/// 一次改写。
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// 设值;缺失的上级键按块风格补齐。
    Set(Path, Value),
    /// 删键(即「恢复继承」);键本来就不在时什么也不做。
    Remove(Path),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    /// 判不准的写法。`line` 从 1 起;`None` = 整份文件层面的问题。
    #[error("{}{}", line_prefix(*.line), .reason)]
    Unsupported { line: Option<usize>, reason: String },
    /// 路径上某一级已经是标量或序列,不能在它下面按键设值。
    #[error("{}", summon_i18n::format!("r761bb0dc57", path = .path))]
    NotAMapping { path: String },
}

fn line_prefix(line: Option<usize>) -> String {
    line.map(|l| summon_i18n::format!("r76cd8f3cbd"))
        .unwrap_or_default()
}

fn unsupported(line: usize, reason: impl Into<String>) -> EditError {
    EditError::Unsupported {
        line: Some(line + 1),
        reason: reason.into(),
    }
}

fn unsupported_file(reason: impl Into<String>) -> EditError {
    EditError::Unsupported {
        line: None,
        reason: reason.into(),
    }
}

/// 依次应用全部改写;任一条不行就整体失败,不返回半成品。
pub fn apply(src: &str, edits: &[Edit]) -> Result<String, EditError> {
    let mut text = src.to_string();
    for edit in edits {
        text = apply_one(&text, edit)?;
    }
    Ok(text)
}

fn apply_one(src: &str, edit: &Edit) -> Result<String, EditError> {
    if src.starts_with('\u{feff}') {
        return Err(unsupported_file(summon_i18n::text!("r81a1649629")));
    }
    let edit_path = match edit {
        Edit::Set(p, _) | Edit::Remove(p) => p,
    };
    if edit_path.is_empty() || edit_path.iter().any(|seg| seg.is_empty()) {
        return Err(unsupported_file(summon_i18n::text!("re3a7adf366")));
    }
    let before = parse_value(src)?;
    let mut expected = before.clone();
    value_apply(&mut expected, edit)?;
    let out = match edit {
        Edit::Set(p, v) => match text_set(src, p, v)? {
            SetOutcome::Done(text) => text,
            // 整张映射替换成另一张映射:逐键设 / 删,没变的键连同注释原样留着。
            SetOutcome::Diff => {
                let mut text = src.to_string();
                for sub in map_diff(&before, p, v) {
                    text = apply_one(&text, &sub)?;
                }
                text
            }
        },
        Edit::Remove(p) => text_remove(src, p)?,
    };
    let after =
        parse_value(&out).map_err(|_| unsupported_file(summon_i18n::text!("rb4bc1ff960")))?;
    if !same_document(&after, &expected) {
        return Err(unsupported_file(summon_i18n::text!("rd2d2a0af99")));
    }
    Ok(out)
}

/// 解析成通用值;失败按行列报。多文档在这里就会失败。
fn parse_value(src: &str) -> Result<Value, EditError> {
    serde_yaml::from_str::<Value>(src).map_err(|e| EditError::Unsupported {
        line: e.location().map(|l| l.line()),
        reason: summon_i18n::format!("r21c2b9a929"),
    })
}

/// 只剩注释的文件解析成 null,与「空映射」是同一个意思。
fn same_document(a: &Value, b: &Value) -> bool {
    let empty = |v: &Value| match v {
        Value::Null => true,
        Value::Mapping(m) => m.is_empty(),
        _ => false,
    };
    a == b || (empty(a) && empty(b))
}

// ── 值层面的同一个改动(自检的期望值) ──────────────────────────────────────

fn key_value(seg: &str) -> Value {
    Value::String(seg.to_string())
}

fn value_apply(root: &mut Value, edit: &Edit) -> Result<(), EditError> {
    match edit {
        Edit::Set(path, value) => {
            let mut cur = root;
            for (depth, seg) in path.iter().enumerate() {
                if cur.is_null() {
                    *cur = Value::Mapping(Mapping::new());
                }
                let Value::Mapping(map) = cur else {
                    return Err(EditError::NotAMapping {
                        path: path[..depth].join("."),
                    });
                };
                if depth + 1 == path.len() {
                    map.insert(key_value(seg), value.clone());
                    return Ok(());
                }
                if !map.contains_key(seg.as_str()) {
                    map.insert(key_value(seg), Value::Mapping(Mapping::new()));
                }
                cur = map
                    .get_mut(seg.as_str())
                    .ok_or_else(|| unsupported_file(summon_i18n::text!("radc5bf3e5b")))?;
            }
            Ok(())
        }
        Edit::Remove(path) => {
            let Some((last, parents)) = path.split_last() else {
                return Ok(());
            };
            let mut cur = root;
            for seg in parents {
                match cur.as_mapping_mut().and_then(|m| m.get_mut(seg.as_str())) {
                    Some(next) => cur = next,
                    None => return Ok(()),
                }
            }
            if let Some(map) = cur.as_mapping_mut() {
                map.remove(last.as_str());
            }
            Ok(())
        }
    }
}

fn value_at<'v>(root: &'v Value, path: &[String]) -> Option<&'v Value> {
    path.iter()
        .try_fold(root, |cur, seg| cur.as_mapping()?.get(seg.as_str()))
}

/// 把「整张映射换成 `new`」拆成逐键改写。
fn map_diff(before: &Value, path: &[String], new: &Value) -> Vec<Edit> {
    let old = value_at(before, path).and_then(Value::as_mapping);
    let Some(new) = new.as_mapping() else {
        return Vec::new();
    };
    let child = |k: &str| {
        let mut p = path.to_vec();
        p.push(k.to_string());
        p
    };
    let mut edits = Vec::new();
    for (k, v) in new {
        let Some(k) = scalar_key(k) else { continue };
        if old.and_then(|m| m.get(k.as_str())) != Some(v) {
            edits.push(Edit::Set(child(&k), v.clone()));
        }
    }
    if let Some(old) = old {
        for k in old.keys() {
            let Some(k) = scalar_key(k) else { continue };
            if !new.contains_key(k.as_str()) {
                edits.push(Edit::Remove(child(&k)));
            }
        }
    }
    edits
}

fn scalar_key(k: &Value) -> Option<String> {
    match k {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

// ── 行模型 ────────────────────────────────────────────────────────────────

struct Line {
    text: String,
    /// 本行原有的换行符:`""`(文件末尾没有换行)、`"\n"` 或 `"\r\n"`。
    eol: &'static str,
}

struct Doc {
    lines: Vec<Line>,
    /// 新插入的行用的换行符:跟文件第一行走。
    eol: &'static str,
}

impl Doc {
    fn parse(src: &str) -> Doc {
        let lines: Vec<Line> = src
            .split_inclusive('\n')
            .map(|piece| {
                if let Some(text) = piece.strip_suffix("\r\n") {
                    Line {
                        text: text.to_string(),
                        eol: "\r\n",
                    }
                } else if let Some(text) = piece.strip_suffix('\n') {
                    Line {
                        text: text.to_string(),
                        eol: "\n",
                    }
                } else {
                    Line {
                        text: piece.to_string(),
                        eol: "",
                    }
                }
            })
            .collect();
        let eol = lines
            .iter()
            .map(|l| l.eol)
            .find(|e| !e.is_empty())
            .unwrap_or("\n");
        Doc { lines, eol }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.text);
            out.push_str(line.eol);
        }
        out
    }

    /// 在 `at` 前插入新行。插在没有末尾换行的最后一行之后时,沿用「没有末尾换行」。
    fn insert(&mut self, at: usize, texts: Vec<String>) {
        if texts.is_empty() {
            return;
        }
        let eol = self.eol;
        let keep_open_end =
            at == self.lines.len() && self.lines.last().is_some_and(|l| l.eol.is_empty());
        if keep_open_end {
            if let Some(last) = self.lines.last_mut() {
                last.eol = eol;
            }
        }
        let count = texts.len();
        let new_lines = texts.into_iter().enumerate().map(|(i, text)| Line {
            text,
            eol: if keep_open_end && i + 1 == count {
                ""
            } else {
                eol
            },
        });
        self.lines.splice(at..at, new_lines);
    }

    /// 删掉一段行。删到文件末尾、而被删的最后一行没有换行时,新的末行也不带换行。
    fn remove(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let open_end =
            range.end == self.lines.len() && self.lines.last().is_some_and(|l| l.eol.is_empty());
        let start = range.start;
        self.lines.drain(range);
        if open_end && start > 0 {
            self.lines[start - 1].eol = "";
        }
    }
}

// ── 结构定位 ──────────────────────────────────────────────────────────────

#[derive(Debug)]
struct MapNode {
    indent: usize,
    entries: Vec<Entry>,
    /// 在这一层追加新键时插入的位置(最后一个键的范围之后)。
    end: usize,
}

#[derive(Debug)]
struct Entry {
    key: String,
    line: usize,
    indent: usize,
    /// 键行里冒号的字节位置。
    colon: usize,
    value: Span,
    /// 这一项占到哪一行为止(不含);不包括它后面的空行与注释。
    end: usize,
}

#[derive(Debug)]
enum Span {
    /// `key:` 后面什么都没有,也没有子行 → null。
    Empty,
    /// 单行值:`[start, end)` 是键行里值的字节范围(不含行尾注释)。
    Inline {
        start: usize,
        end: usize,
        form: InlineForm,
    },
    Map(MapNode),
    Seq {
        item_indent: usize,
    },
    /// 块标量、跨行的字符串或行内集合:能整体删掉,不能按键改写。
    Opaque(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineForm {
    Null,
    Scalar,
    EmptyMap,
    EmptySeq,
    FlowMap,
    FlowSeq,
}

fn is_blank(text: &str) -> bool {
    text.trim_matches([' ', '\t']).is_empty()
}

fn is_blank_or_comment(text: &str) -> bool {
    let s = text.trim_start_matches([' ', '\t']);
    s.is_empty() || s.starts_with('#')
}

fn leading_spaces(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}

fn content_indent(text: &str, line: usize) -> Result<usize, EditError> {
    let n = leading_spaces(text);
    if text[n..].starts_with('\t') {
        return Err(unsupported(line, summon_i18n::text!("rf64a94b62f")));
    }
    Ok(n)
}

fn is_seq_item(body: &str) -> bool {
    body == "-" || body.starts_with("- ") || body.starts_with("-\t")
}

fn is_doc_marker(text: &str) -> bool {
    (text.starts_with("---") || text.starts_with("..."))
        && (text.len() == 3 || text[3..].starts_with([' ', '\t']))
}

fn parse_root(lines: &[Line]) -> Result<MapNode, EditError> {
    let mut start = 0;
    if let Some(first) = lines.iter().position(|l| !is_blank_or_comment(&l.text)) {
        let text = lines[first].text.trim_end();
        if text.starts_with('%') {
            return Err(unsupported(first, summon_i18n::text!("r5f705798a6")));
        }
        if is_doc_marker(text) && text.starts_with("---") {
            let rest = text[3..].trim_start();
            if !(rest.is_empty() || rest.starts_with('#')) {
                return Err(unsupported(first, summon_i18n::text!("r3a55ec2180")));
            }
            start = first + 1;
        }
    }
    if let Some(i) = (start..lines.len()).find(|&i| is_doc_marker(&lines[i].text)) {
        return Err(unsupported(i, summon_i18n::text!("r12944dc727")));
    }
    let Some(first) = (start..lines.len()).find(|&i| !is_blank_or_comment(&lines[i].text)) else {
        return Ok(MapNode {
            indent: 0,
            entries: Vec::new(),
            end: lines.len(),
        });
    };
    let indent = content_indent(&lines[first].text, first)?;
    if is_seq_item(&lines[first].text[indent..]) {
        return Err(EditError::NotAMapping {
            path: summon_i18n::text!("rdcc7801e05").to_string(),
        });
    }
    parse_map(lines, start, lines.len(), indent)
}

fn parse_map(
    lines: &[Line],
    start: usize,
    end: usize,
    indent: usize,
) -> Result<MapNode, EditError> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut i = start;
    while i < end {
        let text = &lines[i].text;
        if is_blank_or_comment(text) {
            i += 1;
            continue;
        }
        if content_indent(text, i)? != indent {
            return Err(unsupported(i, summon_i18n::text!("rc51be29b67")));
        }
        let entry = parse_entry(lines, i, end, indent)?;
        if entries.iter().any(|e| e.key == entry.key) {
            return Err(unsupported(
                i,
                summon_i18n::format!("ra5a39aa077", entry.key),
            ));
        }
        i = entry.end;
        entries.push(entry);
    }
    let end = entries.last().map(|e| e.end).unwrap_or(start);
    Ok(MapNode {
        indent,
        entries,
        end,
    })
}

fn parse_entry(lines: &[Line], i: usize, end: usize, indent: usize) -> Result<Entry, EditError> {
    let text = &lines[i].text;
    let body = &text[indent..];
    if is_seq_item(body) {
        return Err(unsupported(i, summon_i18n::text!("raadc8271d5")));
    }
    let (key, colon_in_body) = parse_key(body, i)?;
    let colon = indent + colon_in_body;
    let (vs, ve) = value_bounds(&text[colon + 1..]);
    let (vstart, vend) = (colon + 1 + vs, colon + 1 + ve);
    let inline = &text[vstart..vend];
    match inline.as_bytes().first() {
        Some(b'&' | b'*') => return Err(unsupported(i, summon_i18n::text!("rb8163e5ade"))),
        Some(b'!') => return Err(unsupported(i, summon_i18n::text!("r6f54411e9f"))),
        _ => {}
    }
    let block_scalar = inline.starts_with('|') || inline.starts_with('>');

    let mut last = i;
    let mut first_child: Option<usize> = None;
    let mut seq_at_same_indent = false;
    let mut j = i + 1;
    while j < end {
        let t = &lines[j].text;
        if is_blank(t) {
            j += 1;
            continue;
        }
        if block_scalar {
            // 块标量的内容行里 `#` 开头的也是正文,不是注释。
            if leading_spaces(t) > indent {
                last = j;
                j += 1;
                continue;
            }
            break;
        }
        if is_blank_or_comment(t) {
            j += 1;
            continue;
        }
        let ind = content_indent(t, j)?;
        if ind > indent {
            first_child.get_or_insert(j);
            last = j;
            j += 1;
            continue;
        }
        let zero_indent_item = ind == indent
            && inline.is_empty()
            && is_seq_item(&t[ind..])
            && (first_child.is_none() || seq_at_same_indent);
        if zero_indent_item {
            seq_at_same_indent = true;
            first_child.get_or_insert(j);
            last = j;
            j += 1;
            continue;
        }
        break;
    }
    let entry_end = last + 1;

    let value = if block_scalar {
        Span::Opaque(summon_i18n::text!("r69aacd320b"))
    } else if let Some(c) = first_child {
        if !inline.is_empty() {
            Span::Opaque(summon_i18n::text!("r0c66d28ea0"))
        } else {
            let child_text = &lines[c].text;
            let child_indent = leading_spaces(child_text);
            if is_seq_item(&child_text[child_indent..]) {
                scan_seq_for_anchors(lines, c..entry_end)?;
                Span::Seq {
                    item_indent: child_indent,
                }
            } else {
                Span::Map(parse_map(lines, i + 1, entry_end, child_indent)?)
            }
        }
    } else if inline.is_empty() {
        Span::Empty
    } else {
        Span::Inline {
            start: vstart,
            end: vend,
            form: classify_inline(inline, i)?,
        }
    };
    Ok(Entry {
        key,
        line: i,
        indent,
        colon,
        value,
        end: entry_end,
    })
}

/// 解析键:返回去引号后的键名与冒号在 `body` 里的字节位置。
fn parse_key(body: &str, line: usize) -> Result<(String, usize), EditError> {
    let not_key = || unsupported(line, summon_i18n::text!("rf855f97093"));
    let quoted_end = match body.as_bytes().first() {
        Some(b'"') => Some(closing_double_quote(body, 0).ok_or_else(not_key)?),
        Some(b'\'') => Some(closing_single_quote(body, 0).ok_or_else(not_key)?),
        Some(b'?') => return Err(unsupported(line, summon_i18n::text!("r4664c8032f"))),
        Some(b'&' | b'*') => return Err(unsupported(line, summon_i18n::text!("rb8163e5ade"))),
        Some(b'!') => return Err(unsupported(line, summon_i18n::text!("r6f54411e9f"))),
        Some(b'[' | b'{' | b'|' | b'>' | b'%' | b'@' | b'`') => return Err(not_key()),
        _ => None,
    };
    let (key_text, colon) = match quoted_end {
        Some(close) => {
            let after = &body[close + 1..];
            let gap = after.len() - after.trim_start_matches(' ').len();
            if !after[gap..].starts_with(':') {
                return Err(not_key());
            }
            (&body[..=close], close + 1 + gap)
        }
        None => {
            let bytes = body.as_bytes();
            let mut colon = None;
            for (idx, &b) in bytes.iter().enumerate() {
                if b == b'#' && idx > 0 && matches!(bytes[idx - 1], b' ' | b'\t') {
                    break;
                }
                let next = bytes.get(idx + 1);
                if b == b':' && matches!(next, None | Some(b' ' | b'\t')) {
                    colon = Some(idx);
                    break;
                }
            }
            let colon = colon.ok_or_else(not_key)?;
            (body[..colon].trim_end(), colon)
        }
    };
    if !matches!(body.as_bytes().get(colon + 1), None | Some(b' ' | b'\t')) {
        return Err(not_key());
    }
    if key_text.is_empty() {
        return Err(not_key());
    }
    if key_text == "<<" {
        return Err(unsupported(line, summon_i18n::text!("r2b8b1d02e0")));
    }
    let key = if quoted_end.is_some() {
        match serde_yaml::from_str::<Value>(key_text) {
            Ok(Value::String(s)) => s,
            _ => return Err(not_key()),
        }
    } else {
        key_text.to_string()
    };
    Ok((key, colon))
}

/// `s[open]` 是 `"`;返回配对的 `"` 的位置。
fn closing_double_quote(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// `s[open]` 是 `'`;`''` 是转义的单引号。
fn closing_single_quote(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = open + 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 冒号之后那段文本里,值的字节范围(去掉前后空白与行尾注释)。
fn value_bounds(rest: &str) -> (usize, usize) {
    let start = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let bytes = rest.as_bytes();
    let comment_at = |from: usize| {
        (from..bytes.len())
            .find(|&i| bytes[i] == b'#' && (i == 0 || matches!(bytes[i - 1], b' ' | b'\t')))
            .unwrap_or(bytes.len())
    };
    let stop = match bytes.get(start) {
        Some(b'"') => closing_double_quote(rest, start).map_or(bytes.len(), |c| comment_at(c + 1)),
        Some(b'\'') => closing_single_quote(rest, start).map_or(bytes.len(), |c| comment_at(c + 1)),
        Some(b'[' | b'{') => flow_comment_at(rest, start),
        _ => comment_at(start),
    };
    let end = start + rest[start..stop].trim_end_matches([' ', '\t']).len();
    (start, end)
}

/// 行内集合里找行尾注释:引号只在集合的元素开头才算引号(`[it's]` 里的 `'` 是正文)。
fn flow_comment_at(rest: &str, start: usize) -> usize {
    let bytes = rest.as_bytes();
    let mut i = start;
    let mut prev_sig = b'[';
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' | b'\'' if matches!(prev_sig, b'[' | b'{' | b',' | b':') => {
                let close = if b == b'"' {
                    closing_double_quote(rest, i)
                } else {
                    closing_single_quote(rest, i)
                };
                let Some(close) = close else {
                    return bytes.len();
                };
                i = close + 1;
                prev_sig = b;
                continue;
            }
            b'#' if i > 0 && matches!(bytes[i - 1], b' ' | b'\t') => return i,
            b' ' | b'\t' => {}
            _ => prev_sig = b,
        }
        i += 1;
    }
    bytes.len()
}

fn classify_inline(inline: &str, line: usize) -> Result<InlineForm, EditError> {
    let squeezed: String = inline.chars().filter(|c| !c.is_whitespace()).collect();
    Ok(match inline.as_bytes().first() {
        Some(b'&' | b'*') => return Err(unsupported(line, summon_i18n::text!("rb8163e5ade"))),
        Some(b'!') => return Err(unsupported(line, summon_i18n::text!("r6f54411e9f"))),
        Some(b'{') if squeezed == "{}" => InlineForm::EmptyMap,
        Some(b'{') => InlineForm::FlowMap,
        Some(b'[') if squeezed == "[]" => InlineForm::EmptySeq,
        Some(b'[') => InlineForm::FlowSeq,
        _ if matches!(inline, "null" | "Null" | "NULL" | "~") => InlineForm::Null,
        _ => InlineForm::Scalar,
    })
}

/// 块序列是整体替换的,但里面的锚点 / 引用会让「别处的值跟着变」—— 发现了就整份拒绝。
fn scan_seq_for_anchors(lines: &[Line], range: Range<usize>) -> Result<(), EditError> {
    for i in range {
        let text = &lines[i].text;
        if is_blank_or_comment(text) {
            continue;
        }
        let mut s = text.trim_start();
        while is_seq_item(s) {
            s = s[1..].trim_start();
        }
        let after_colon = s.find(": ").map(|p| s[p + 2..].trim_start());
        let anchored = |t: &str| t.starts_with('&') || t.starts_with('*');
        if anchored(s) || after_colon.is_some_and(anchored) {
            return Err(unsupported(i, summon_i18n::text!("rb8163e5ade")));
        }
    }
    Ok(())
}

/// 文件里的缩进单位:最小的非零缩进,缺省 2。
fn detect_unit(lines: &[Line]) -> usize {
    lines
        .iter()
        .filter(|l| !is_blank_or_comment(&l.text))
        .map(|l| leading_spaces(&l.text))
        .filter(|&n| n > 0)
        .min()
        .map(|n| n.clamp(2, 8))
        .unwrap_or(2)
}

// ── 改写 ──────────────────────────────────────────────────────────────────

enum SetOutcome {
    Done(String),
    /// 目标是一张块映射、新值也是非空映射:交给调用方逐键改。
    Diff,
}

fn text_set(src: &str, path: &[String], value: &Value) -> Result<SetOutcome, EditError> {
    let mut doc = Doc::parse(src);
    let root = parse_root(&doc.lines)?;
    let unit = detect_unit(&doc.lines);
    let mut map = &root;
    for (depth, seg) in path.iter().enumerate() {
        let Some(entry) = map.entries.iter().find(|e| e.key == *seg) else {
            let lines = render_new(&path[depth..], value, map.indent, unit)?;
            doc.insert(map.end, lines);
            return Ok(SetOutcome::Done(doc.render()));
        };
        if depth + 1 == path.len() {
            return replace_value(doc, entry, value, unit);
        }
        match &entry.value {
            Span::Map(child) => map = child,
            Span::Empty
            | Span::Inline {
                form: InlineForm::Null | InlineForm::EmptyMap,
                ..
            } => {
                strip_inline(&mut doc, entry);
                let lines = render_new(&path[depth + 1..], value, entry.indent + unit, unit)?;
                doc.insert(entry.line + 1, lines);
                return Ok(SetOutcome::Done(doc.render()));
            }
            Span::Inline {
                form: InlineForm::FlowMap,
                ..
            } => return Err(unsupported(entry.line, summon_i18n::text!("reda1f5eb10"))),
            Span::Opaque(why) => return Err(unsupported(entry.line, *why)),
            Span::Seq { .. } | Span::Inline { .. } => {
                return Err(EditError::NotAMapping {
                    path: path[..=depth].join("."),
                })
            }
        }
    }
    Err(unsupported_file(summon_i18n::text!("re3a7adf366")))
}

fn replace_value(
    mut doc: Doc,
    entry: &Entry,
    value: &Value,
    unit: usize,
) -> Result<SetOutcome, EditError> {
    let non_empty_map = value.as_mapping().is_some_and(|m| !m.is_empty());
    match &entry.value {
        Span::Opaque(why) => return Err(unsupported(entry.line, *why)),
        Span::Map(_) if non_empty_map => return Ok(SetOutcome::Diff),
        _ => {}
    }
    let keeps_flow = matches!(
        entry.value,
        Span::Inline {
            form: InlineForm::FlowSeq | InlineForm::EmptySeq,
            ..
        }
    );
    let inline = match inline_repr(value)? {
        Some(s) => Some(s),
        None if keeps_flow => flow_seq_repr(value)?,
        None => None,
    };
    let children = entry.line + 1..entry.end;
    match inline {
        Some(s) => {
            let text = &mut doc.lines[entry.line].text;
            match &entry.value {
                Span::Inline { start, end, .. } => text.replace_range(*start..*end, &s),
                _ => text.insert_str(entry.colon + 1, &format!(" {s}")),
            }
            if !matches!(entry.value, Span::Inline { .. }) {
                doc.remove(children);
            }
        }
        None => {
            let child_indent = match (&entry.value, value) {
                (Span::Seq { item_indent }, Value::Sequence(_)) => *item_indent,
                (Span::Map(m), _) => m.indent,
                _ => entry.indent + unit,
            };
            let lines = render_block(value, child_indent, unit)?;
            strip_inline(&mut doc, entry);
            doc.remove(children);
            doc.insert(entry.line + 1, lines);
        }
    }
    Ok(SetOutcome::Done(doc.render()))
}

/// 去掉键行上的单行值,保留冒号与行尾注释(`key: {}  # c` → `key:  # c`)。
fn strip_inline(doc: &mut Doc, entry: &Entry) {
    if let Span::Inline { start, end, .. } = entry.value {
        let text = &mut doc.lines[entry.line].text;
        let head = text[..start].trim_end_matches([' ', '\t']).len();
        text.replace_range(head..end, "");
    }
}

fn text_remove(src: &str, path: &[String]) -> Result<String, EditError> {
    let mut doc = Doc::parse(src);
    let root = parse_root(&doc.lines)?;
    let Some((last, parents)) = path.split_last() else {
        return Ok(src.to_string());
    };
    let mut map = &root;
    let mut parent_entry: Option<&Entry> = None;
    for seg in parents {
        let Some(entry) = map.entries.iter().find(|e| e.key == *seg) else {
            return Ok(src.to_string());
        };
        match &entry.value {
            Span::Map(child) => {
                map = child;
                parent_entry = Some(entry);
            }
            Span::Inline {
                form: InlineForm::FlowMap,
                ..
            } => return Err(unsupported(entry.line, summon_i18n::text!("reda1f5eb10"))),
            // 上一级是空值或标量:这个键本来就不存在。
            _ => return Ok(src.to_string()),
        }
    }
    let Some(entry) = map.entries.iter().find(|e| e.key == *last) else {
        return Ok(src.to_string());
    };
    doc.remove(entry.line..entry.end);
    // 删掉的是这一层唯一的键:上一级留成 `key: {}`,否则它会读成 null,而不是「空的这一段」。
    if let Some(parent) = parent_entry.filter(|_| map.entries.len() == 1) {
        doc.lines[parent.line]
            .text
            .insert_str(parent.colon + 1, " {}");
    }
    Ok(doc.render())
}

// ── 只读定位(原文校验报行号、脱敏找值的位置) ──────────────────────────────

/// 键在文件里的行号(从 1 起)。路径不在、或结构判不准时返回 None。
pub fn key_line(src: &str, path: &[String]) -> Option<usize> {
    let doc = Doc::parse(src);
    let root = parse_root(&doc.lines).ok()?;
    let mut map = &root;
    let (last, parents) = path.split_last()?;
    for seg in parents {
        let entry = map.entries.iter().find(|e| e.key == *seg)?;
        let Span::Map(child) = &entry.value else {
            return None;
        };
        map = child;
    }
    map.entries
        .iter()
        .find(|e| e.key == *last)
        .map(|e| e.line + 1)
}

/// 某张映射下一个键的值在原文里的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildValue {
    pub key: String,
    /// 从冒号之后到值的末尾(不含行尾注释与最后的换行)的字节范围。值为空时范围为空。
    pub range: Range<usize>,
    /// 键所在行,从 1 起。
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChildLookup {
    /// 路径不存在,或那里是空值。
    Missing,
    /// 那里有值但不是块映射(行内映射、标量、序列):逐键定位不了。
    NotBlockMap {
        line: usize,
    },
    Children(Vec<ChildValue>),
}

/// 列出 `path` 处那张块映射的每个键及其值的位置。
pub(crate) fn child_values(src: &str, path: &[String]) -> Result<ChildLookup, EditError> {
    let doc = Doc::parse(src);
    let root = parse_root(&doc.lines)?;
    let mut map = &root;
    for seg in path {
        let Some(entry) = map.entries.iter().find(|e| e.key == *seg) else {
            return Ok(ChildLookup::Missing);
        };
        match &entry.value {
            Span::Map(child) => map = child,
            Span::Empty
            | Span::Inline {
                form: InlineForm::Null | InlineForm::EmptyMap,
                ..
            } => return Ok(ChildLookup::Missing),
            _ => {
                return Ok(ChildLookup::NotBlockMap {
                    line: entry.line + 1,
                })
            }
        }
    }
    let mut offsets = Vec::with_capacity(doc.lines.len() + 1);
    let mut at = 0;
    for line in &doc.lines {
        offsets.push(at);
        at += line.text.len() + line.eol.len();
    }
    let children = map
        .entries
        .iter()
        .map(|e| {
            let start = offsets[e.line] + e.colon + 1;
            let end = match &e.value {
                Span::Inline { end, .. } => offsets[e.line] + end,
                Span::Empty => start,
                // 跨行的值:到这一项最后一行的行尾。
                _ => offsets[e.end - 1] + doc.lines[e.end - 1].text.len(),
            };
            ChildValue {
                key: e.key.clone(),
                range: start..end,
                line: e.line + 1,
            }
        })
        .collect();
    Ok(ChildLookup::Children(children))
}

// ── 渲染 ──────────────────────────────────────────────────────────────────

/// 新建一条(可能带若干级新上级的)键。
fn render_new(
    segs: &[String],
    value: &Value,
    indent: usize,
    unit: usize,
) -> Result<Vec<String>, EditError> {
    let pad = " ".repeat(indent);
    let Some((first, rest)) = segs.split_first() else {
        return Ok(Vec::new());
    };
    let key = render_key(first);
    if !rest.is_empty() {
        let mut lines = vec![format!("{pad}{key}:")];
        lines.extend(render_new(rest, value, indent + unit, unit)?);
        return Ok(lines);
    }
    Ok(match inline_repr(value)? {
        Some(s) => vec![format!("{pad}{key}: {s}")],
        None => {
            let mut lines = vec![format!("{pad}{key}:")];
            lines.extend(render_block(value, indent + unit, unit)?);
            lines
        }
    })
}

/// 非空映射 / 序列的块形态(作为某个键的子行)。
fn render_block(value: &Value, indent: usize, unit: usize) -> Result<Vec<String>, EditError> {
    let pad = " ".repeat(indent);
    let mut lines = Vec::new();
    match value {
        Value::Mapping(map) => {
            for (k, v) in map {
                let key = scalar_key(k)
                    .ok_or_else(|| unsupported_file(summon_i18n::text!("r242cb39c32")))?;
                lines.extend(render_new(&[key], v, indent, unit)?);
            }
        }
        Value::Sequence(items) => {
            for item in items {
                match inline_repr(item)? {
                    Some(s) => lines.push(format!("{pad}- {s}")),
                    None => {
                        lines.push(format!("{pad}-"));
                        lines.extend(render_block(item, indent + 2, unit)?);
                    }
                }
            }
        }
        _ => {
            if let Some(s) = inline_repr(value)? {
                lines.push(format!("{pad}{s}"));
            }
        }
    }
    Ok(lines)
}

/// 能写在一行里的值:标量与空集合;非空集合返回 None。
fn inline_repr(value: &Value) -> Result<Option<String>, EditError> {
    Ok(Some(match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => render_str(s, false),
        Value::Sequence(items) if items.is_empty() => "[]".to_string(),
        Value::Mapping(map) if map.is_empty() => "{}".to_string(),
        Value::Sequence(_) | Value::Mapping(_) => return Ok(None),
        Value::Tagged(_) => return Err(unsupported_file(summon_i18n::text!("rbdc0637b01"))),
    }))
}

/// 原来就是行内列表时,新值(全是标量)也写成行内,保持手写风格。
fn flow_seq_repr(value: &Value) -> Result<Option<String>, EditError> {
    let Some(items) = value.as_sequence() else {
        return Ok(None);
    };
    let mut parts = Vec::new();
    for item in items {
        match item {
            Value::String(s) => parts.push(render_str(s, true)),
            Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => return Ok(None),
            other => match inline_repr(other)? {
                Some(s) => parts.push(s),
                None => return Ok(None),
            },
        }
    }
    Ok(Some(format!("[{}]", parts.join(", "))))
}

fn render_key(key: &str) -> String {
    render_str(key, false)
}

/// 字符串:能写成裸值且读回仍是同一个字符串时写裸值,否则写双引号(JSON 转义即合法 YAML)。
fn render_str(s: &str, in_flow: bool) -> String {
    if plain_safe(s, in_flow) {
        s.to_string()
    } else {
        serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
    }
}

fn plain_safe(s: &str, in_flow: bool) -> bool {
    let Some(first) = s.chars().next() else {
        return false;
    };
    if s.trim() != s || s.chars().any(char::is_control) {
        return false;
    }
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    if s.contains(": ") || s.contains(" #") || s.ends_with(':') {
        return false;
    }
    if in_flow && s.contains([',', '[', ']', '{', '}']) {
        return false;
    }
    matches!(serde_yaml::from_str::<Value>(s), Ok(Value::String(ref back)) if back == s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(src: &str, dotted: &str, yaml_value: &str) -> Result<String, EditError> {
        let value: Value = serde_yaml::from_str(yaml_value).expect("测试值应可解析");
        apply(src, &[Edit::Set(path(dotted), value)])
    }

    fn remove(src: &str, dotted: &str) -> Result<String, EditError> {
        apply(src, &[Edit::Remove(path(dotted))])
    }

    fn parsed(text: &str) -> Value {
        serde_yaml::from_str(text).expect("结果应可解析")
    }

    #[test]
    fn set_scalar_replaces_only_value_keeps_trailing_comment() {
        let src = "# 整机配置\nmessaging:\n  cot_visible: true   # 过程\n  idle_ttl_secs: 86400\n\
                   poll_ms: 400 # 轮询\n";
        let out = set(src, "messaging.cot_visible", "false").unwrap();
        assert_eq!(
            out,
            "# 整机配置\nmessaging:\n  cot_visible: false   # 过程\n  idle_ttl_secs: 86400\n\
             poll_ms: 400 # 轮询\n"
        );
        let out = set(src, "poll_ms", "250").unwrap();
        assert_eq!(
            out,
            "# 整机配置\nmessaging:\n  cot_visible: true   # 过程\n  idle_ttl_secs: 86400\n\
             poll_ms: 250 # 轮询\n"
        );
        // 值本身带 `#`、引号,替换后注释照旧。
        let src = "preview_server:\n  public_base_url: \"http://a#b\"  # 对外\n";
        let out = set(
            src,
            "preview_server.public_base_url",
            "http://10.0.0.1:7765",
        )
        .unwrap();
        assert_eq!(
            out,
            "preview_server:\n  public_base_url: http://10.0.0.1:7765  # 对外\n"
        );
    }

    #[test]
    fn set_scalar_quotes_strings_that_would_read_back_differently() {
        let src = "menubar:\n  icon: xyz\n";
        for (raw, expect) in [
            ("\"true\"", "  icon: \"true\"\n"),
            ("\"a: b\"", "  icon: \"a: b\"\n"),
            ("\"#x\"", "  icon: \"#x\"\n"),
            ("\"\"", "  icon: \"\"\n"),
            ("\"~/work/x\"", "  icon: ~/work/x\n"),
            ("\"多行\\n文本\"", "  icon: \"多行\\n文本\"\n"),
        ] {
            let out = set(src, "menubar.icon", raw).unwrap();
            assert_eq!(out, format!("menubar:\n{expect}"), "写入 {raw}");
        }
    }

    #[test]
    fn set_nested_creates_missing_parents_in_block_style() {
        let src = "poll_ms: 400\n# 结尾注释\n";
        let out = set(src, "cursor.quota.interval_secs", "900").unwrap();
        assert_eq!(
            out,
            "poll_ms: 400\ncursor:\n  quota:\n    interval_secs: 900\n# 结尾注释\n"
        );
        // 上级在、缺最后一级:追加到上级最后一个键之后,用上级已有的缩进。
        let src = "messaging:\n    cot_visible: true\n\n# 下一段\nsentinel:\n    enabled: false\n";
        let out = set(src, "messaging.session_exited.panel_clean", "\"已退出\"").unwrap();
        assert_eq!(
            out,
            "messaging:\n    cot_visible: true\n    session_exited:\n        panel_clean: 已退出\n\n\
             # 下一段\nsentinel:\n    enabled: false\n"
        );
        // 上级是 null / {}:换成块,行尾注释留着。
        let src = "feishu:\n  chat_operators: {}  # 本群名单\n  scope: im\n";
        let out = set(src, "feishu.chat_operators.oc_a", "[ou_1]").unwrap();
        assert_eq!(
            out,
            "feishu:\n  chat_operators:  # 本群名单\n    oc_a:\n      - ou_1\n  scope: im\n"
        );
        let out = set("messaging:\n", "messaging.mask_email", "false").unwrap();
        assert_eq!(out, "messaging:\n  mask_email: false\n");
        // 空文件。
        assert_eq!(set("", "a.b", "1").unwrap(), "a:\n  b: 1\n");
    }

    #[test]
    fn remove_key_drops_its_lines_only() {
        let src = "runtime:\n  # 工作目录\n  working_dir: ~/w   # 默认\n  env:\n    A: \"1\"\n\
                   \n    B: \"2\"\n  cli: claude\n# 尾注\n";
        let out = remove(src, "runtime.env").unwrap();
        assert_eq!(
            out,
            "runtime:\n  # 工作目录\n  working_dir: ~/w   # 默认\n  cli: claude\n# 尾注\n"
        );
        let out = remove(src, "runtime.working_dir").unwrap();
        assert_eq!(
            out,
            "runtime:\n  # 工作目录\n  env:\n    A: \"1\"\n\n    B: \"2\"\n  cli: claude\n# 尾注\n"
        );
        // 不存在的键、上一级是标量:什么都不做。
        assert_eq!(remove(src, "runtime.nope").unwrap(), src);
        assert_eq!(remove(src, "runtime.cli.x").unwrap(), src);
        assert_eq!(remove(src, "missing.key").unwrap(), src);
        // 删掉这一层唯一的键:上一级留成空映射,解析出来是空的这一段而不是 null。
        let out = remove(
            "messaging:\n  mask_email: false\nb: 1\n",
            "messaging.mask_email",
        )
        .unwrap();
        assert_eq!(out, "messaging: {}\nb: 1\n");
        assert_eq!(parsed(&out)["messaging"], Value::Mapping(Mapping::new()));
        // 块标量整体删掉,内容里像注释的行也一起删。
        let src = "a: |\n  正文\n  # 也是正文\nb: 1\n";
        assert_eq!(remove(src, "a").unwrap(), "b: 1\n");
    }

    #[test]
    fn set_list_replaces_flow_and_block_lists() {
        let src =
            "feishu:\n  allowed_users: [ou_a, \"ou_b\"]  # 名单\n  operators:\n    - ou_a   # 我\n\
                   \x20   - ou_b\n  scope: im\n";
        let out = set(src, "feishu.allowed_users", "[ou_c, on_d]").unwrap();
        assert_eq!(
            out,
            "feishu:\n  allowed_users: [ou_c, on_d]  # 名单\n  operators:\n    - ou_a   # 我\n\
             \x20   - ou_b\n  scope: im\n"
        );
        let out = set(src, "feishu.operators", "[on_x]").unwrap();
        assert_eq!(
            out,
            "feishu:\n  allowed_users: [ou_a, \"ou_b\"]  # 名单\n  operators:\n    - on_x\n  scope: im\n"
        );
        let out = set(src, "feishu.operators", "[]").unwrap();
        assert_eq!(
            out,
            "feishu:\n  allowed_users: [ou_a, \"ou_b\"]  # 名单\n  operators: []\n  scope: im\n"
        );
        // 顶格写的序列:新项沿用同样的缩进。
        let src = "join_verbs:\n- 进来回答\n- 来\nb: 1\n";
        assert_eq!(
            set(src, "join_verbs", "[\"进来\"]").unwrap(),
            "join_verbs:\n- 进来\nb: 1\n"
        );
        // 原来是空列表 / 标量:非空列表写成行内(空列表本来就是行内)或块。
        assert_eq!(
            set("a: []\n", "a", "[x, \"y,z\"]").unwrap(),
            "a: [x, \"y,z\"]\n"
        );
        assert_eq!(set("a: 1\n", "a", "[x]").unwrap(), "a:\n  - x\n");
    }

    #[test]
    fn set_map_entry_under_chat_operators() {
        let src = "feishu:\n  chat_operators:\n    oc_a: [ou_1]   # 示例群\n  scope: im\n";
        let out = set(src, "feishu.chat_operators.oc_b", "[ou_2, ou_3]").unwrap();
        assert_eq!(
            out,
            "feishu:\n  chat_operators:\n    oc_a: [ou_1]   # 示例群\n    oc_b:\n      - ou_2\n      - ou_3\n\
             \x20 scope: im\n"
        );
        let out = set(src, "feishu.chat_operators.oc_a", "[ou_9]").unwrap();
        assert_eq!(
            out,
            "feishu:\n  chat_operators:\n    oc_a: [ou_9]   # 示例群\n  scope: im\n"
        );
        // 整张映射替换:没变的键连同注释原样,变了的逐键改,没了的删掉。
        let src = "runtime:\n  env:\n    A: \"1\"   # 代理\n    B: x\n";
        let out = set(src, "runtime.env", "{A: \"1\", C: y}").unwrap();
        assert_eq!(out, "runtime:\n  env:\n    A: \"1\"   # 代理\n    C: y\n");
        let out = set("runtime:\n  cli: codex\n", "runtime.env", "{K: v}").unwrap();
        assert_eq!(out, "runtime:\n  cli: codex\n  env:\n    K: v\n");
    }

    #[test]
    fn edit_preserves_unrelated_bytes_across_shapes() {
        // CRLF、引号键、四格缩进、行尾注释、文件末尾没有换行。
        let src =
            "# 头\r\n\"messaging\":\r\n    'cot_visible': true # 开\r\n    idle_ttl_secs: 60\r\n\
                   tools:\r\n    git: /usr/bin/git";
        let out = set(src, "messaging.cot_visible", "false").unwrap();
        assert_eq!(
            out,
            "# 头\r\n\"messaging\":\r\n    'cot_visible': false # 开\r\n    idle_ttl_secs: 60\r\n\
             tools:\r\n    git: /usr/bin/git"
        );
        let out = set(src, "tools.tmux", "/opt/homebrew/bin/tmux").unwrap();
        assert_eq!(
            out,
            "# 头\r\n\"messaging\":\r\n    'cot_visible': true # 开\r\n    idle_ttl_secs: 60\r\n\
             tools:\r\n    git: /usr/bin/git\r\n    tmux: /opt/homebrew/bin/tmux"
        );
        let out = remove(src, "tools.git").unwrap();
        assert_eq!(
            out,
            "# 头\r\n\"messaging\":\r\n    'cot_visible': true # 开\r\n    idle_ttl_secs: 60\r\n\
             tools: {}"
        );
        // 两格缩进 + 块标量邻居:块标量里像「键: 值」的行不被当成键。
        let src = "messaging:\n  system_prompt: |\n    cot_visible: 这是正文\n    # 也是正文\n  \
                   cot_visible: true\n";
        let out = set(src, "messaging.cot_visible", "false").unwrap();
        assert_eq!(
            out,
            "messaging:\n  system_prompt: |\n    cot_visible: 这是正文\n    # 也是正文\n  \
             cot_visible: false\n"
        );
        assert_eq!(
            parsed(&out)["messaging"]["system_prompt"],
            Value::String("cot_visible: 这是正文\n# 也是正文\n".into())
        );
        // 开头单独一行 `---` 不算多文档。
        let out = set("---\na: 1\n", "a", "2").unwrap();
        assert_eq!(out, "---\na: 2\n");
    }

    #[test]
    fn edit_refuses_anchor_and_multi_document() {
        let anchored = "base: &b\n  x: 1\nother: *b\n";
        assert!(matches!(
            set(anchored, "base.x", "2"),
            Err(EditError::Unsupported { .. })
        ));
        assert!(matches!(
            set(anchored, "other.x", "2"),
            Err(EditError::Unsupported { .. })
        ));
        let merged = "base:\n  x: 1\nother:\n  <<: {x: 1}\n  y: 2\n";
        assert!(matches!(
            set(merged, "other.y", "3"),
            Err(EditError::Unsupported { .. })
        ));
        let in_seq = "verbs:\n  - &v 进来\n  - *v\nb: 1\n";
        assert!(matches!(
            set(in_seq, "b", "2"),
            Err(EditError::Unsupported { .. })
        ));
        let multi = "a: 1\n---\nb: 2\n";
        assert!(matches!(
            set(multi, "a", "2"),
            Err(EditError::Unsupported { .. })
        ));
        assert!(matches!(
            remove(multi, "a"),
            Err(EditError::Unsupported { .. })
        ));
        let tagged = "a: !!str 1\n";
        assert!(matches!(
            set(tagged, "a", "2"),
            Err(EditError::Unsupported { .. })
        ));
        // 跨行的值与块标量:不按键改写。
        let multiline = "a: 第一行\n  第二行\nb: 1\n";
        assert!(matches!(
            set(multiline, "a", "x"),
            Err(EditError::Unsupported { .. })
        ));
        assert!(matches!(
            set("a: |\n  x\n", "a", "y"),
            Err(EditError::Unsupported { .. })
        ));
    }

    #[test]
    fn edit_refuses_flow_mapping_parent() {
        let src = "feishu: {scope: im, operators: [ou_a]}\n";
        let err = set(src, "feishu.operators", "[ou_b]").unwrap_err();
        assert!(
            matches!(err, EditError::Unsupported { line: Some(1), .. }),
            "{err}"
        );
        assert!(matches!(
            remove(src, "feishu.scope"),
            Err(EditError::Unsupported { .. })
        ));
        // 整个替换行内映射本身是可以的。
        assert_eq!(set(src, "feishu", "{}").unwrap(), "feishu: {}\n");
        // 上一级是标量 / 序列:报出是哪一级。
        assert_eq!(
            set("a: 1\n", "a.b", "2").unwrap_err(),
            EditError::NotAMapping { path: "a".into() }
        );
        assert_eq!(
            set("a:\n  - x\n", "a.b", "2").unwrap_err(),
            EditError::NotAMapping { path: "a".into() }
        );
        assert!(matches!(
            set("- a\n- b\n", "x", "1"),
            Err(EditError::NotAMapping { .. })
        ));
    }

    #[test]
    fn edits_apply_in_order_and_fail_as_a_whole() {
        let src = "a: 1\nb:\n  c: 2\n";
        let out = apply(
            src,
            &[
                Edit::Set(path("a"), Value::from(5)),
                Edit::Remove(path("b.c")),
                Edit::Set(path("d.e"), Value::from("f")),
            ],
        )
        .unwrap();
        assert_eq!(out, "a: 5\nb: {}\nd:\n  e: f\n");
        let err = apply(
            src,
            &[
                Edit::Set(path("a"), Value::from(5)),
                Edit::Set(path("a.x"), Value::from(1)),
            ],
        );
        assert!(err.is_err(), "第二条不行,整体失败");
        let broken = "a: [1\n";
        assert!(matches!(
            set(broken, "a", "2"),
            Err(EditError::Unsupported { line: Some(_), .. })
        ));
    }

    #[test]
    fn locators_find_keys_and_value_spans() {
        let src = "# 头\nruntime:\n  env:\n    A: \"x\"  # 注\n    B: |\n      多行\n    C:\n  cli: claude\n";
        assert_eq!(key_line(src, &path("runtime.cli")), Some(8));
        assert_eq!(key_line(src, &path("runtime.env.B")), Some(5));
        assert_eq!(key_line(src, &path("runtime.nope")), None);
        let ChildLookup::Children(children) = child_values(src, &path("runtime.env")).unwrap()
        else {
            panic!("env 是块映射");
        };
        let spans: Vec<(&str, &str)> = children
            .iter()
            .map(|c| (c.key.as_str(), &src[c.range.clone()]))
            .collect();
        assert_eq!(spans, [("A", " \"x\""), ("B", " |\n      多行"), ("C", "")]);
        assert_eq!(
            child_values("runtime: {env: {}}\n", &path("runtime.env")).unwrap(),
            ChildLookup::NotBlockMap { line: 1 }
        );
        assert_eq!(
            child_values("name: a\n", &path("runtime.env")).unwrap(),
            ChildLookup::Missing
        );
    }

    /// 拿本机真实配置的副本做往返:`SUMMON_YAML_SAMPLES=<目录>` 下的每个 *.yaml,对字段表里
    /// 每一项「设回原值 → 删掉 → 设一个新值」,每一步都要么成功(apply 内部已核对语义),要么
    /// 明确拒绝。副本不入库。
    #[test]
    #[ignore]
    fn round_trips_local_samples() {
        use crate::dashboard::fields::FIELDS;
        let Ok(dir) = std::env::var("SUMMON_YAML_SAMPLES") else {
            return;
        };
        let mut refused = Vec::new();
        let mut done = 0;
        for entry in std::fs::read_dir(&dir).expect("样本目录") {
            let file = entry.expect("目录项").path();
            if file.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let src = std::fs::read_to_string(&file).expect("读样本");
            let is_config = file.file_name().and_then(|n| n.to_str()) == Some("config.yaml");
            let doc: Value = serde_yaml::from_str(&src).expect("样本应可解析");
            for field in FIELDS {
                let applies = if is_config {
                    field.has_layer(crate::dashboard::fields::Layer::Global)
                } else {
                    field.has_layer(crate::dashboard::fields::Layer::Agent)
                };
                if !applies || field.write == crate::dashboard::fields::Write::Readonly {
                    continue;
                }
                let p = path(field.key);
                let current = value_at(&doc, &p).cloned();
                let sample = match field.kind {
                    crate::dashboard::fields::FieldKind::Bool => Value::Bool(true),
                    crate::dashboard::fields::FieldKind::Int
                    | crate::dashboard::fields::FieldKind::DurationSecs => Value::from(42),
                    crate::dashboard::fields::FieldKind::StringList => {
                        serde_yaml::from_str("[a, b]").unwrap()
                    }
                    crate::dashboard::fields::FieldKind::StringMap => {
                        serde_yaml::from_str("{k: v}").unwrap()
                    }
                    _ => Value::from("sample"),
                };
                let mut edits = vec![Edit::Remove(p.clone()), Edit::Set(p.clone(), sample)];
                if let Some(v) = current {
                    edits.insert(0, Edit::Set(p.clone(), v));
                }
                for edit in edits {
                    // 只打印键路径,不打印值:样本里可能有密钥。
                    let what = match &edit {
                        Edit::Set(..) => "设",
                        Edit::Remove(_) => "删",
                    };
                    match apply(&src, std::slice::from_ref(&edit)) {
                        Ok(_) => done += 1,
                        Err(e) => {
                            refused.push(format!("{} {what} {}: {e}", file.display(), field.key))
                        }
                    }
                }
            }
        }
        eprintln!("往返成功 {done} 次;拒绝 {} 次", refused.len());
        for r in &refused {
            eprintln!("  {r}");
        }
        assert!(done > 0);
    }
}
