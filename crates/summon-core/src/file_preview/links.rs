//! 出站 markdown 里提到的本地文件:找出来交给调用方挂成回复末尾的打开按钮。
//!
//! 判据 fail-closed(设计见 `docs/spec/2026-08-25-file-preview-server-design.md` 与
//! `docs/spec/2026-10-20-link-click-auth-design.md`):认行内链接里的绝对路径目标,以及**内容整体
//! 就是一条绝对路径的行内 code span** —— 那是 agent 写文件路径的自然形态;围栏内不认(字面量);
//! 图片语法不认;句子里的裸路径不认。任何一条不满足原样保留 —— 宁可漏认不可错认。

/// 找出 markdown 里 `accept` 认账的本地路径,按出现顺序去重返回。
///
/// 正文里的 code span 原样保留;认账的行内链接只留文字 —— 点文字链接不会回传点击人,打不开,
/// 打开入口统一是回复末尾的按钮。`accept` 收到一个绝对路径(已剥 `file://` 前缀),返回 `false`
/// (不是常规文件 / 超限 / 预览服务没开)则原样保留。
pub fn extract_local_links<F: FnMut(&str) -> bool>(
    md: &str,
    mut accept: F,
) -> (String, Vec<String>) {
    let mut paths: Vec<String> = Vec::new();
    let found = |path: &str, paths: &mut Vec<String>| {
        if !paths.iter().any(|known| known == path) {
            paths.push(path.to_string());
        }
    };
    // 语法字符全是 ASCII,在 (字节偏移, char) 向量上扫描;切片段时字节偏移天然落在 char 边界上。
    let chars: Vec<(usize, char)> = md.char_indices().collect();
    let n = chars.len();
    let mut out = String::with_capacity(md.len());
    // 已拷进 out 的原文字节终点;认出行内链接时先 flush 到链接起点,再只拼它的文字。
    let mut copied = 0usize;
    let mut i = 0usize;
    // >0 = 正在 code span 里,值是开序列的反引号长度(CommonMark:闭序列须等长)。
    let mut code_ticks = 0usize;
    // 围栏状态:None = 不在围栏;Some((字符, 长度)) = 围栏开着。
    // CommonMark:闭合围栏必须同字符、长度 ≥ 开长度,且闭合行围栏后只剩空白。
    let mut fence: Option<(char, usize)> = None;

    while i < n {
        let (bi, c) = chars[i];

        // 转义对:下一字符不是语法,整对原样保留。
        if c == '\\' && i + 1 < n {
            i += 2;
            continue;
        }

        if c == '`' || c == '~' {
            let mut j = i;
            while j < n && chars[j].1 == c {
                j += 1;
            }
            let run = j - i;
            if code_ticks > 0 {
                // 在 code span 里:只有等长反引号序列才闭合(~ 不参与 code span)。
                if c == '`' && run == code_ticks {
                    code_ticks = 0;
                }
            } else if run >= 3 && line_has_only_spaces_before(&chars, i) {
                // 行首 ≥3 个 ` 或 ~:按围栏开/合处理(非行首的长反引号串是 code span 开序列)。
                match fence {
                    None => fence = Some((c, run)),
                    Some((fc, fl))
                        if c == fc && run >= fl && line_has_only_spaces_after(&chars, j) =>
                    {
                        fence = None
                    }
                    _ => {}
                }
            } else if c == '`' {
                // 内容整体是一条本机绝对路径的 code span → 记下路径,正文不动。围栏内不适用:
                // 那里是字面量。
                if fence.is_none() {
                    if let Some((content_s, content_e, _)) = scan_code_span(&chars, j, run) {
                        let content = &md[chars[content_s].0..chars[content_e].0];
                        if let Some(path) = code_span_path(content) {
                            if accept(path) {
                                found(path, &mut paths);
                            }
                        }
                    }
                }
                code_ticks = run;
            }
            i = j;
            continue;
        }

        if fence.is_some() || code_ticks > 0 {
            i += 1;
            continue;
        }

        if c == '[' && !is_image_syntax(&chars, i) {
            if let Some((label_s, label_e, target_s, target_e, link_end)) =
                scan_inline_link(&chars, i)
            {
                let target = &md[chars[target_s].0..chars[target_e].0];
                let stripped = strip_file_scheme(target);
                let abs_path = stripped.starts_with('/').then_some(stripped);
                if let Some(path) = abs_path {
                    if accept(path) {
                        found(path, &mut paths);
                        out.push_str(&md[copied..bi]);
                        out.push_str(&md[chars[label_s].0..chars[label_e].0]);
                        // 链接可能正好在文末,link_end == n 时取不到字节偏移。
                        copied = if link_end < n {
                            chars[link_end].0
                        } else {
                            md.len()
                        };
                        i = link_end;
                        continue;
                    }
                }
            }
        }

        i += 1;
    }
    out.push_str(&md[copied..]);
    (out, paths)
}

/// `[` 的前一个非转义字符是 `!` = 图片语法,不认。
fn is_image_syntax(chars: &[(usize, char)], open: usize) -> bool {
    open > 0 && chars[open - 1].1 == '!'
}

/// 行首到 `pos` 之间只有空格/制表符(围栏的 CommonMark 缩进上限是 3 格,这里放宽到任意空白)。
fn line_has_only_spaces_before(chars: &[(usize, char)], pos: usize) -> bool {
    let mut j = pos;
    while j > 0 {
        j -= 1;
        match chars[j].1 {
            '\n' => return true,
            ' ' | '\t' => continue,
            _ => return false,
        }
    }
    true
}

/// 行内 `pos` 之后到行尾只有空格/制表符(闭合围栏行的 CommonMark 要求)。
fn line_has_only_spaces_after(chars: &[(usize, char)], pos: usize) -> bool {
    let mut j = pos;
    while j < chars.len() {
        match chars[j].1 {
            '\n' => return true,
            ' ' | '\t' => j += 1,
            _ => return false,
        }
    }
    true
}

/// 从开序列之后扫一个行内 code span。`run` = 开序列反引号个数,闭合序列须**恰好**等长
/// (CommonMark)。跨行不认——fail-closed,与 [`scan_inline_link`] 同一取舍。
/// 返回 (内容起, 内容止, span 终点),均为 char 下标,区间左闭右开。
fn scan_code_span(
    chars: &[(usize, char)],
    content_start: usize,
    run: usize,
) -> Option<(usize, usize, usize)> {
    let n = chars.len();
    let mut k = content_start;
    while k < n {
        match chars[k].1 {
            '\n' => return None,
            '`' => {
                let mut e = k;
                while e < n && chars[e].1 == '`' {
                    e += 1;
                }
                if e - k == run {
                    return Some((content_start, k, e));
                }
                k = e;
            }
            _ => k += 1,
        }
    }
    None
}

/// code span 的内容是不是「整体就是一条绝对路径」。
///
/// 判据窄到只剩一种形态:trim 后非空、不含空白、剥掉 `file://` 后以 `/` 开头。含空白的一律不算
/// ——那是命令行(`ls /tmp`)或散文,不是「这个文件给你看」。存在性交给调用方的 `accept` 判定。
fn code_span_path(content: &str) -> Option<&str> {
    let trimmed = content.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return None;
    }
    let stripped = strip_file_scheme(trimmed);
    stripped.starts_with('/').then_some(stripped)
}

/// 从 `[` 位置扫一个行内链接。成功返回
/// (label 起, label 止, target 起, target 止, 链接终点)——均为 char 下标,区间左闭右开。
/// 只认形态,不校验 target 内容(绝对路径/存在性交给调用方的 `accept`)。
fn scan_inline_link(
    chars: &[(usize, char)],
    open: usize,
) -> Option<(usize, usize, usize, usize, usize)> {
    let n = chars.len();
    // 找匹配的 ](允许嵌套,跨行不认——fail-closed)。
    let mut depth = 0i32;
    let mut j = open;
    let close = loop {
        if j >= n {
            return None;
        }
        let c = chars[j].1;
        if c == '\\' && j + 1 < n {
            j += 2;
            continue;
        }
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            '\n' => return None,
            _ => {}
        }
        j += 1;
    };
    // ] 后必须紧跟 (。
    let paren_open = close + 1;
    if paren_open >= n || chars[paren_open].1 != '(' {
        return None;
    }
    let mut k = paren_open + 1;
    let (target_s, target_e, link_end) = if k < n && chars[k].1 == '<' {
        // 尖括号形式:读到 >(允许空白),其后必须紧跟 )。
        k += 1;
        let ts = k;
        while k < n && chars[k].1 != '>' {
            if chars[k].1 == '\\' && k + 1 < n {
                k += 2;
            } else {
                k += 1;
            }
        }
        if k >= n || k + 1 >= n || chars[k + 1].1 != ')' {
            return None;
        }
        (ts, k, k + 2)
    } else {
        // 括号形式:配平圆括号,遇未转义空白即放弃(CommonMark:非尖括号 target 不含空白)。
        let ts = k;
        let mut pdepth = 1i32;
        while k < n {
            let c = chars[k].1;
            if c == '\\' && k + 1 < n {
                k += 2;
                continue;
            }
            if c.is_whitespace() {
                return None;
            }
            match c {
                '(' => pdepth += 1,
                ')' => {
                    pdepth -= 1;
                    if pdepth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        if k >= n || pdepth != 0 {
            return None;
        }
        (ts, k, k + 1)
    };
    Some((open + 1, close, target_s, target_e, link_end))
}

/// 剥 `file://` 前缀。`file:///abs/path` → `/abs/path`;不带前缀的原样返回。
fn strip_file_scheme(target: &str) -> &str {
    target
        .strip_prefix("file://")
        .or_else(|| target.strip_prefix("FILE://"))
        .unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用 accept:认所有绝对路径(不做存在性校验,只验证认路径的形态)。
    fn extract(md: &str) -> (String, Vec<String>) {
        extract_local_links(md, |path| path.starts_with('/'))
    }

    fn paths(md: &str) -> Vec<String> {
        extract(md).1
    }

    /// agent 写文件路径的自然形态是 `` `/abs/path` ``,不是 markdown 链接语法。只认链接语法
    /// 等于覆盖率为零(2026-08-29 owner 在飞书上撞见:remote-codex 报告路径点不开)。
    #[test]
    fn finds_a_code_span_that_is_exactly_one_absolute_path_and_keeps_the_text() {
        assert_eq!(
            extract("报告:`/tmp/a.md`"),
            ("报告:`/tmp/a.md`".into(), vec!["/tmp/a.md".into()])
        );
        // 双反引号同样成立(闭合序列须等长)。
        assert_eq!(paths("``/tmp/a.md``"), ["/tmp/a.md"]);
        // file:// 前缀照剥。
        assert_eq!(paths("`file:///tmp/a.md`"), ["/tmp/a.md"]);
    }

    /// 「整体就是一条路径」是唯一判据 —— 命令、散文、相对路径都不算。
    #[test]
    fn ignores_code_spans_that_are_not_exactly_a_path() {
        for src in [
            "`ls /tmp/a.md`",
            "`relative/a.md`",
            "`/tmp/a.md 和别的`",
            "``",
            "`https://example.com/x`",
        ] {
            assert_eq!(extract(src), (src.to_string(), vec![]), "不该认:{src}");
        }
    }

    /// 围栏内是字面量。
    #[test]
    fn ignores_paths_inside_fences() {
        let src = "```\n`/tmp/a.md`\n```";
        assert_eq!(extract(src), (src.to_string(), vec![]));
    }

    /// 跨行的反引号对不认(与 `scan_inline_link` 同样 fail-closed)。
    #[test]
    fn ignores_code_span_spanning_a_newline() {
        let src = "`/tmp/a.md\n还有别的`";
        assert_eq!(extract(src), (src.to_string(), vec![]));
    }

    /// accept 不认账(文件不存在/超限/服务没开)时原样保留。
    #[test]
    fn keeps_everything_when_accept_declines() {
        for src in ["报告:`/tmp/a.md`", "看 [报告](/tmp/a.md) 吧"] {
            assert_eq!(
                extract_local_links(src, |_| false),
                (src.to_string(), vec![])
            );
        }
    }

    /// 流式与终态都可能过这里:结果再过一遍,正文不变、路径不变。
    #[test]
    fn extraction_is_idempotent() {
        let (once, found) = extract("报告:`/tmp/a.md` 与 [b](/tmp/b.md)");
        let (twice, again) = extract(&once);
        assert_eq!(twice, once);
        assert_eq!(found, ["/tmp/a.md", "/tmp/b.md"]);
        assert_eq!(again, ["/tmp/a.md"], "文字链接已摘掉,只剩 code span");
    }

    #[test]
    fn an_absolute_link_keeps_only_its_label() {
        assert_eq!(
            extract("看 [报告](/tmp/a.md) 吧"),
            ("看 报告 吧".into(), vec!["/tmp/a.md".into()])
        );
        assert_eq!(
            extract("[x](file:///tmp/a.md)"),
            ("x".into(), vec!["/tmp/a.md".into()])
        );
    }

    #[test]
    fn paths_are_deduplicated_in_order() {
        assert_eq!(
            paths("`/tmp/b.md` [a](/tmp/a.md) `/tmp/b.md` [again](/tmp/a.md)"),
            ["/tmp/b.md", "/tmp/a.md"]
        );
    }

    #[test]
    fn leaves_relative_and_url_links_untouched() {
        let src = "[a](relative.md) [b](https://example.com/x) [c](/v1/users)";
        // 只认存在的文件:/v1/users 也是绝对路径,但不是本机文件。
        let got = extract_local_links(src, |p| std::path::Path::new(p).is_file());
        assert_eq!(
            got,
            (src.to_string(), vec![]),
            "相对路径与 URL 不认;不存在的绝对路径也不认"
        );
    }

    #[test]
    fn leaves_links_inside_code_span_and_fence_untouched() {
        let src = "`[code](/tmp/a.md)` 和\n```\n[fence](/tmp/b.md)\n```\n外 [ok](/tmp/c.md)";
        let (text, found) = extract(src);
        assert!(
            text.contains("`[code](/tmp/a.md)`"),
            "行内 code 不动:{text}"
        );
        assert!(text.contains("[fence](/tmp/b.md)"), "围栏内不动:{text}");
        assert!(text.contains("外 ok"), "围栏外的链接只留文字:{text}");
        assert_eq!(found, ["/tmp/c.md"]);
    }

    #[test]
    fn leaves_image_syntax_untouched() {
        let (text, found) = extract("![图](/tmp/a.png) 和 [链接](/tmp/b.md)");
        assert!(text.contains("![图](/tmp/a.png)"), "图片不动:{text}");
        assert_eq!(found, ["/tmp/b.md"]);
    }

    #[test]
    fn leaves_bare_paths_untouched() {
        let (text, found) = extract("文件在 /tmp/a.md 和 [链接](/tmp/b.md)");
        assert!(text.contains("文件在 /tmp/a.md"), "裸路径不动:{text}");
        assert_eq!(found, ["/tmp/b.md"]);
    }

    #[test]
    fn handles_nested_brackets_in_label() {
        assert_eq!(
            extract("[a [b] c](/tmp/a.md)"),
            ("a [b] c".into(), vec!["/tmp/a.md".into()])
        );
    }

    #[test]
    fn parens_in_path_are_balanced() {
        assert_eq!(paths("[x](/tmp/a(1).md)"), ["/tmp/a(1).md"]);
    }

    #[test]
    fn whitespace_in_paren_destination_is_not_a_link() {
        // 非尖括号形式含空白 = 非法 CommonMark 目标,语义不明,fail-closed 不认;
        // 要带空格请用尖括号形式(见 handles_angle_bracket_destination_with_spaces)。
        let src = "[x](/tmp/a b.md)";
        assert_eq!(extract(src), (src.to_string(), vec![]));
    }

    #[test]
    fn handles_angle_bracket_destination_with_spaces() {
        assert_eq!(paths("[x](</tmp/a b.md>)"), ["/tmp/a b.md"]);
    }

    #[test]
    fn leaves_escaped_brackets_untouched() {
        let (text, found) = extract(r"\[x\](/tmp/a.md) [y](/tmp/b.md)");
        assert!(text.contains(r"\[x\](/tmp/a.md)"), "转义括号不动:{text}");
        assert_eq!(found, ["/tmp/b.md"]);
    }

    #[test]
    fn leaves_unclosed_link_untouched() {
        let src = "[x](/tmp/a.md 未闭合";
        assert_eq!(extract(src), (src.to_string(), vec![]));
    }

    #[test]
    fn leaves_multiline_label_untouched() {
        let src = "[x\n多行](/tmp/a.md)";
        assert_eq!(
            extract(src),
            (src.to_string(), vec![]),
            "跨行 label 不认(fail-closed)"
        );
    }

    #[test]
    fn tilde_fence_blocks_extraction() {
        let (text, found) = extract("~~~\n[x](/tmp/a.md)\n~~~\n[y](/tmp/b.md)");
        assert!(text.contains("[x](/tmp/a.md)"), "~~~ 围栏内不动:{text}");
        assert_eq!(found, ["/tmp/b.md"]);
    }

    #[test]
    fn strikethrough_tildes_are_not_fence() {
        // 行内 ~~ 删除线 ~~ 不是围栏(只有行首 ≥3 个才是)。
        assert_eq!(paths("~~删掉~~ [x](/tmp/a.md)"), ["/tmp/a.md"]);
    }

    #[test]
    fn mixed_fence_markers_do_not_close_each_other() {
        // ``` 开的围栏里出现 ~~~ 不闭合(CommonMark:闭合须同字符)。
        let (text, found) = extract("```\n~~~\n[x](/tmp/a.md)\n```\n[y](/tmp/b.md)");
        assert!(text.contains("[x](/tmp/a.md)"), "围栏内不动:{text}");
        assert_eq!(found, ["/tmp/b.md"]);
    }

    #[test]
    fn shorter_fence_run_does_not_close() {
        // ```` 开、``` 长度不够,不闭合。
        let (text, found) = extract("````\n```\n[x](/tmp/a.md)\n````\n[y](/tmp/b.md)");
        assert!(text.contains("[x](/tmp/a.md)"), "围栏内不动:{text}");
        assert_eq!(found, ["/tmp/b.md"]);
    }

    #[test]
    fn fence_close_with_trailing_text_does_not_close() {
        // 闭合行围栏后带文字 = 不是闭合,仍是围栏内容。
        let (text, found) = extract("```\n``` not-close\n[x](/tmp/a.md)\n```\n[y](/tmp/b.md)");
        assert!(text.contains("[x](/tmp/a.md)"), "围栏内不动:{text}");
        assert_eq!(found, ["/tmp/b.md"]);
    }

    #[test]
    fn no_links_is_identical() {
        let src = "普通文本,没有任何链接\n第二段";
        assert_eq!(extract(src), (src.to_string(), vec![]));
    }

    #[test]
    fn unicode_text_does_not_break_scanner() {
        let (text, found) = extract("中文 [报告](/tmp/文件.md) 结尾 🎉");
        assert_eq!(text, "中文 报告 结尾 🎉");
        assert_eq!(found, ["/tmp/文件.md"]);
    }
}
