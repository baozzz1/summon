//! 预览页 HTML 渲染(纯逻辑,不碰 IO/网络)。
//!
//! 图表在独立沙箱内绘制，外层只加载固定的尺寸更新脚本；HTML 文件在独立沙箱内预览。
//! 所有外层注入点一律 HTML 转义，markdown 里的原始 HTML 标签当文本显示。

use base64::Engine;
use serde_json::Value;

/// 文件名的扩展名(小写;没有 `.` 或长于 8 字符视为无扩展名)。预览页与 `/raw` 都从这里取,
/// 同一个文件在两边才会认成同一种类型。
pub fn file_ext(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|e| !e.is_empty() && e.len() <= 8)
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 按扩展名选渲染形态。认不出的一律按纯文本(fail-closed:不猜内容)。
pub fn detect_kind(filename: &str) -> PreviewKind {
    let ext = file_ext(filename);
    if raster_image_mime(&ext).is_some() {
        return PreviewKind::Image;
    }
    match ext.as_str() {
        "md" | "markdown" => PreviewKind::Markdown,
        "json" => PreviewKind::Json,
        "svg" => PreviewKind::Svg,
        "html" | "htm" => PreviewKind::Html,
        _ => PreviewKind::Text,
    }
}

/// 浏览器能直接解码的位图格式(`ext` 取自 [`file_ext`])→ MIME。预览页据此用 `<img>` 显示,
/// `/raw` 据此给 Content-Type 内联服务 —— 两边认的格式只有这一张表。
/// svg 不在表里:它是能带脚本的文档,`/raw` 不内联它。
pub fn raster_image_mime(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "avif" => Some("image/avif"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    /// 自包含网页：脚本仅在不具备同源权限的 iframe 内执行。
    Html,
    Markdown,
    Json,
    Text,
    /// 位图:页内 `<img>` 指向同一 token 的 `/raw`。
    Image,
    /// svg:页内 `<img>` 用 data URI 内联。以图片加载的 svg 不执行脚本,
    /// 而直接打开 `/raw` 时它是顶层文档,所以不能指向 `/raw`。
    Svg,
}

/// 渲染整个预览页。图片按扩展名显示;其余 `bytes` 非 UTF-8 时给二进制下载提示页。
pub fn render_page(filename: &str, path: &str, size: u64, token: &str, bytes: &[u8]) -> String {
    let mut has_diagrams = false;
    let (body, outline) = match (detect_kind(filename), std::str::from_utf8(bytes)) {
        (PreviewKind::Image, _) => (
            render_image(filename, &format!("/p/{token}/raw")),
            String::new(),
        ),
        (PreviewKind::Svg, _) => {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            (
                render_image(filename, &format!("data:image/svg+xml;base64,{data}")),
                String::new(),
            )
        }
        (_, Err(_)) => (
            summon_i18n::format!(
                "rdcb53913a7"
            ),
            String::new(),
        ),
        (PreviewKind::Markdown, Ok(text)) => {
            let document = render_markdown_document(text, Some(token));
            has_diagrams = document.has_diagrams;
            (document.body, render_outline(&document.headings))
        }
        (PreviewKind::Json, Ok(text)) => (render_json(text), String::new()),
        (PreviewKind::Html, Ok(text)) => (
            format!(
                "<iframe title=\"{}\" sandbox=\"allow-scripts\" referrerpolicy=\"no-referrer\" srcdoc=\"{}\"></iframe>",
                escape_html(filename), escape_html(text)
            ),
            String::new(),
        ),
        (PreviewKind::Text, Ok(text)) => {
            (format!("<pre>{}</pre>", escape_html(text)), String::new())
        }
    };
    let content_class = if outline.is_empty() {
        "content"
    } else {
        "content with-toc"
    };
    let body_class = if detect_kind(filename) == PreviewKind::Html {
        "html-preview"
    } else if outline.is_empty() {
        ""
    } else {
        "has-toc"
    };
    let scripts = if has_diagrams {
        format!(
            "<script src=\"{}\" defer></script>",
            super::diagram::RESIZE_PATH
        )
    } else {
        String::new()
    };
    summon_i18n::format!(
        "r52213bdca4",
        filename = escape_html(filename),
        path = escape_html(path),
    )
}

/// 图片正文。`src` 只由 [`render_page`] 拼出(同 token 的 `/raw` 或 base64 data URI),
/// 不含未转义的外部输入;文件名进 alt 前转义。
fn render_image(filename: &str, src: &str) -> String {
    format!(
        "<p class='image'><img src=\"{src}\" alt=\"{}\"></p>",
        escape_html(filename)
    )
}

/// 目录列举页的一个条目(预览 URL 已由调用方 mint 好,渲染层不碰 token)。
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 文件的预览 URL;子目录为 `None`,只显示名字。
    pub url: Option<String>,
}

/// 目录 token 的能力范围是这一层的文件:子目录不给链接,否则一个目录链接能一路点进整棵子树。
/// 以 `.` 开头的条目(`.ssh`、`.env` 一类)不列。
pub fn dir_listing_shows(name: &str) -> bool {
    !name.starts_with('.')
}

/// 目录在文件系统里的身份(设备号, inode)。同一目录换个写法(符号链接、macOS firmlink
/// `/System/Volumes/Data/...`、Linux bind mount)身份不变,按路径字符串比较则认不出。
pub type DirIdentity = (u64, u64);

/// 目录能不能生成预览链接。`protected` 是家目录及其各级上级(含根目录)的身份 —— 它们的列表
/// 直接摊开整个家目录或整机,一律不给;`protected` 为空说明家目录取不到,也一律不给。
pub fn directory_link_allowed(dir: DirIdentity, protected: &[DirIdentity]) -> bool {
    !protected.is_empty() && !protected.contains(&dir)
}

/// 渲染目录列举页。条目由调用方排好序(目录在前、文件在后、各自字母序);
/// `truncated` = 有条目因超过上限未显示。不提供父目录/`..` 导航,子目录只显示名字——
/// token 的能力范围就是这一层,列举页不许把它扩成目录树遍历。
pub fn render_dir_page(dirname: &str, path: &str, entries: &[DirEntry], truncated: bool) -> String {
    let count = entries.len();
    let mut rows = String::new();
    for e in entries {
        let marker = if e.is_dir { "📁" } else { "📄" };
        let size = if e.is_dir {
            String::new()
        } else {
            human_size(e.size)
        };
        let name = match &e.url {
            Some(url) => format!(
                "<a href=\"{}\">{}</a>",
                escape_html(url),
                escape_html(&e.name)
            ),
            None => escape_html(&e.name),
        };
        rows.push_str(&format!(
            "<tr><td>{marker} {name}</td><td>{}</td></tr>\n",
            escape_html(&size),
        ));
    }
    let body = if entries.is_empty() {
        summon_i18n::text!("rd86c4658fc").to_string()
    } else {
        summon_i18n::format!("rf091a90d5e")
    };
    let trunc_note = if truncated {
        summon_i18n::format!("r7941507a79", DIR_LISTING_LIMIT, count)
    } else {
        String::new()
    };
    summon_i18n::format!(
        "rf835b145a1",
        dirname = escape_html(dirname),
        path = escape_html(path),
    )
}

/// 目录列举的条目上限:超过只显示前 N 项,防超大目录把页面撑爆。
pub const DIR_LISTING_LIMIT: usize = 200;

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// markdown → HTML。原始 HTML 标签当文本显示(防 XSS),表格/删除线启用。
/// 链接 href 过 scheme 白名单:只放行 http/https/mailto 与无 scheme 的相对路径,
/// `javascript:` / `data:` 等一律置空——预览页不许成为 XSS 入口。
struct MarkdownDocument {
    body: String,
    headings: Vec<Heading>,
    has_diagrams: bool,
}

struct Heading {
    level: u8,
    id: String,
    title: String,
}

const OUTLINE_LIMIT: usize = 200;

#[cfg(test)]
fn render_markdown(text: &str) -> String {
    render_markdown_document(text, None).body
}

pub(super) fn markdown_options() -> pulldown_cmark::Options {
    use pulldown_cmark::Options;
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_FOOTNOTES
}

fn render_markdown_document(text: &str, token: Option<&str>) -> MarkdownDocument {
    use pulldown_cmark::{CowStr, Event, Parser, Tag, TagEnd};
    let opts = markdown_options();
    let events: Vec<_> = Parser::new_ext(text, opts).collect();
    let mut headings = Vec::new();
    let mut active_heading: Option<(u8, String, String)> = None;
    let mut heading_index = 0usize;
    for event in &events {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                heading_index += 1;
                active_heading = Some((
                    *level as u8,
                    format!("section-{heading_index}"),
                    String::new(),
                ));
            }
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if let Some((_, _, title)) = active_heading.as_mut() {
                    title.push_str(text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, _, title)) = active_heading.as_mut() {
                    title.push(' ');
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, id, title)) = active_heading.take() {
                    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !title.is_empty() && headings.len() < OUTLINE_LIMIT {
                        headings.push(Heading { level, id, title });
                    }
                }
            }
            _ => {}
        }
    }

    let blocks = if token.is_some() {
        super::diagram::diagram_blocks(&events)
    } else {
        Vec::new()
    };
    let has_diagrams = !blocks.is_empty();
    let mut block_index = 0usize;
    let mut heading_index = 0usize;
    let mut input = events.into_iter().enumerate();
    let mut sanitized = Vec::new();
    while let Some((index, event)) = input.next() {
        if let Some(block) = blocks.get(block_index).filter(|block| block.start == index) {
            for _ in index..block.end {
                input.next();
            }
            let token = escape_html(token.unwrap());
            sanitized.push(Event::Html(CowStr::from(summon_i18n::format!(
                "r293ca5dc5b",
                block_index + 1,
            ))));
            block_index += 1;
            continue;
        }
        sanitized.push(match event {
            // 原始 HTML 一律降级成 Text:html writer 会转义 Text 事件,标签当文本显示,
            // 预览页不允许 agent 内容注入 DOM。
            Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                // URL 只在 Start 事件上(End 是不带字段的 TagEnd::Link),危险 scheme 在这里置空。
                let dest_url = if href_is_safe(&dest_url) {
                    dest_url
                } else {
                    "".into()
                };
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                })
            }
            Event::Start(Tag::Heading {
                level,
                classes,
                attrs,
                ..
            }) => {
                heading_index += 1;
                Event::Start(Tag::Heading {
                    level,
                    id: Some(CowStr::from(format!("section-{heading_index}"))),
                    classes,
                    attrs,
                })
            }
            other => other,
        });
    }
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, sanitized.into_iter());
    MarkdownDocument {
        body: html,
        headings,
        has_diagrams,
    }
}

fn render_outline(headings: &[Heading]) -> String {
    if headings.is_empty() {
        return String::new();
    }
    let mut items = String::new();
    for heading in headings {
        items.push_str(&format!(
            "<li class='toc-level-{}'><a href=\"#{}\">{}</a></li>\n",
            heading.level,
            heading.id,
            escape_html(&heading.title),
        ));
    }
    summon_i18n::format!("rc1510f3f2b")
}

/// href 白名单判据。浏览器导航前会剥空白/控制字符、解码 HTML 实体,判据必须在
/// 同样归一化后的形态上做,否则 `java\tscript:` / `&#106;avascript:` 这类变体
/// 会带着危险 scheme 进 DOM。
fn href_is_safe(url: &str) -> bool {
    let decoded = decode_entities(url);
    let normalized: String = decoded
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_control())
        .collect();
    let Some((scheme, _)) = normalized.split_once(':') else {
        return true; // 无冒号:相对路径/锚点,安全
    };
    if !is_scheme(scheme) {
        return true; // 冒号在路径里(如 ../a:b),不是 scheme
    }
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "mailto"
    )
}

/// 像 URL scheme 吗:ASCII 字母开头,其余字母/数字/`+`/`.`/`-`。
fn is_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// 解码数字字符引用与基础命名实体,仅供 href 判据做归一化(不追求完整实体表,
/// 覆盖能还原出 `:` 或字母的形态即可)。
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(rel) = rest.find('&') {
        out.push_str(&rest[..rel]);
        let after = &rest[rel + 1..];
        match decode_one_entity(after) {
            Some((c, len)) => {
                out.push(c);
                rest = &after[len..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 解析 `after` 开头的实体(不含 `&`),返回 (字符, 消耗字节数)。
fn decode_one_entity(after: &str) -> Option<(char, usize)> {
    let b = after.as_bytes();
    if b.first() == Some(&b'#') {
        // 数字引用:&#..; 或 &#x..;(分号可省,浏览器对数字引用不要求分号)。
        let (radix, digits_start) = if matches!(b.get(1), Some(&b'x') | Some(&b'X')) {
            (16, 2)
        } else {
            (10, 1)
        };
        let mut val = 0u32;
        let mut k = digits_start;
        let mut any = false;
        while k < b.len() {
            let digit = match (b[k], radix) {
                (d @ b'0'..=b'9', _) => (d - b'0') as u32,
                (d @ b'a'..=b'f', 16) => (d - b'a' + 10) as u32,
                (d @ b'A'..=b'F', 16) => (d - b'A' + 10) as u32,
                _ => break,
            };
            val = val.saturating_mul(radix).saturating_add(digit);
            any = true;
            k += 1;
        }
        if !any {
            return None;
        }
        let semi = usize::from(b.get(k) == Some(&b';'));
        return Some((char::from_u32(val).unwrap_or('\u{FFFD}'), k + semi));
    }
    // 命名实体:只列能参与 scheme 绕过的(冒号)与五个基础项;分号可省。
    const NAMED: [(&str, char); 6] = [
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
        ("colon", ':'),
    ];
    for (name, ch) in NAMED {
        if after.starts_with(name) {
            let len = name.len() + usize::from(b.get(name.len()) == Some(&b';'));
            return Some((ch, len));
        }
    }
    None
}

/// JSON → 折叠树。解析失败降级为转义纯文本(不因为一个坏 JSON 让整页 500)。
fn render_json(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => {
            let mut out = String::new();
            render_json_value(&v, 0, &mut out);
            out
        }
        Err(e) => summon_i18n::format!("r5ecf2f51c3", escape_html(text)),
    }
}

fn render_json_value(v: &Value, depth: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("<span class='j-null'>null</span>"),
        Value::Bool(b) => out.push_str(&format!("<span class='j-bool'>{b}</span>")),
        Value::Number(n) => out.push_str(&format!("<span class='j-num'>{n}</span>")),
        Value::String(s) => out.push_str(&format!(
            "<span class='j-str'>\"{}\"</span>",
            escape_html(s)
        )),
        Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str("[]");
                return;
            }
            let open = if depth == 0 { " open" } else { "" };
            out.push_str(&summon_i18n::format!("ra74ce20147", arr.len()));
            for item in arr {
                render_json_value(item, depth + 1, out);
                out.push('\n');
            }
            out.push_str("</div></details>");
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let open = if depth == 0 { " open" } else { "" };
            out.push_str(&summon_i18n::format!("r78787338bb", map.len()));
            for (k, val) in map {
                out.push_str(&format!(
                    "<span class='j-key'>\"{}\"</span>: ",
                    escape_html(k)
                ));
                render_json_value(val, depth + 1, out);
                out.push('\n');
            }
            out.push_str("</div></details>");
        }
    }
}

/// HTML 转义。所有不可信文本进 HTML 前的唯一通道。
pub(super) fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_preview_embeds_escaped_source_in_a_script_only_sandbox() {
        let source = "</iframe><script>parent.document.body.textContent='escaped'</script>\"&";
        for name in ["demo.html", "demo.HTM", ".HTML"] {
            let page = render_page(
                name,
                "/work/demo.html",
                source.len() as u64,
                "token",
                source.as_bytes(),
            );
            assert!(page.contains("sandbox=\"allow-scripts\""));
            assert!(page.contains("referrerpolicy=\"no-referrer\""));
            assert!(page.contains(&format!("srcdoc=\"{}\"", escape_html(source))));
            assert!(!page.contains(source));
            assert!(!page.contains("allow-same-origin"));
            assert!(page.contains("/p/token/raw"));
        }
        let binary = render_page("bad.html", "/work/bad.html", 1, "token", &[0xff]);
        assert!(!binary.contains("<iframe"));
    }

    #[test]
    fn detect_kind_by_extension() {
        assert_eq!(detect_kind("a.md"), PreviewKind::Markdown);
        assert_eq!(detect_kind("a.MARKDOWN"), PreviewKind::Markdown);
        assert_eq!(detect_kind("a.json"), PreviewKind::Json);
        assert_eq!(detect_kind("a.log"), PreviewKind::Text);
        assert_eq!(detect_kind("noext"), PreviewKind::Text);
    }

    #[test]
    fn markdown_renders_and_escapes_raw_html() {
        let html =
            render_markdown("# 标题\n\n<script>alert(1)</script>\n\n[a](https://example.com)");
        assert!(html.contains("<h1 id=\"section-1\">标题</h1>"), "{html}");
        assert!(html.contains("<a href=\"https://example.com\">"), "{html}");
        assert!(!html.contains("<script>"), "原始 HTML 必须转义:{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn markdown_page_has_a_clickable_heading_outline() {
        let page = render_page(
            "guide.md",
            "docs/guide.md",
            64,
            "tok",
            b"# Overview\n\n## Install\n\n### Linux\n\n## Usage\n",
        );

        assert!(page.contains("<nav class='toc'"), "{page}");
        assert!(page.contains("href=\"#section-1\">Overview</a>"), "{page}");
        assert!(page.contains("href=\"#section-2\">Install</a>"), "{page}");
        assert!(page.contains("class='toc-level-3'"), "{page}");
        assert!(
            page.contains("<h1 id=\"section-1\">Overview</h1>"),
            "{page}"
        );
        assert!(page.contains("<h2 id=\"section-4\">Usage</h2>"), "{page}");
    }

    #[test]
    fn markdown_outline_uses_unique_ids_and_escapes_heading_text() {
        let page = render_page(
            "guide.md",
            "guide.md",
            64,
            "tok",
            b"# Same\n\n# Same\n\n# <img src=x onerror=alert(1)>\n",
        );

        assert!(page.contains("href=\"#section-1\">Same</a>"), "{page}");
        assert!(page.contains("href=\"#section-2\">Same</a>"), "{page}");
        assert!(page.contains("id=\"section-1\""), "{page}");
        assert!(page.contains("id=\"section-2\""), "{page}");
        assert!(
            !page.contains("<img src=x"),
            "目录和正文都不能注入 HTML:{page}"
        );
        assert!(
            page.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "{page}"
        );
    }

    #[test]
    fn markdown_outline_is_bounded_without_truncating_the_body() {
        let markdown = (1..=OUTLINE_LIMIT + 1)
            .map(|index| format!("# Section {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let page = render_page(
            "guide.md",
            "guide.md",
            markdown.len() as u64,
            "tok",
            markdown.as_bytes(),
        );

        assert_eq!(page.matches("class='toc-level-").count(), OUTLINE_LIMIT);
        assert!(
            page.contains("href=\"#section-200\">Section 200</a>"),
            "{page}"
        );
        assert!(!page.contains("href=\"#section-201\">"), "{page}");
        assert!(
            page.contains("<h1 id=\"section-201\">Section 201</h1>"),
            "{page}"
        );
    }

    #[test]
    fn non_markdown_pages_do_not_show_a_document_outline() {
        let text = render_page("guide.txt", "guide.txt", 8, "tok", b"# heading");
        let json = render_page("guide.json", "guide.json", 2, "tok", b"{}");
        assert!(!text.contains("<nav class='toc'"), "{text}");
        assert!(!json.contains("<nav class='toc'"), "{json}");
    }

    #[test]
    fn markdown_escapes_text_angle_brackets() {
        let html = render_markdown("Vec<T> 和 &str");
        assert!(html.contains("Vec&lt;T&gt;"), "{html}");
        assert!(html.contains("&amp;str"), "{html}");
    }

    #[test]
    fn markdown_links_with_dangerous_schemes_are_neutralized() {
        let html = render_markdown(
            "[x](javascript:alert(1)) [y](data:text/html,<script>alert(2)</script>) [z](https://example.com)",
        );
        assert!(
            !html.contains("javascript:"),
            "javascript: 不许进 href:{html}"
        );
        assert!(!html.contains("data:text/html"), "data: 不许进 href:{html}");
        assert!(
            html.contains("<a href=\"https://example.com\">"),
            "http 链接放行:{html}"
        );
    }

    #[test]
    fn markdown_link_scheme_bypass_variants_are_neutralized() {
        // 实体编码 / 空白填充 / 尖括号目标里的 tab:浏览器会归一化后执行,判据必须先归一化。
        let html = render_markdown("[a](&#106;avascript:alert(1)) [b](<java\tscript:alert(2)>)");
        assert!(!html.contains("javascript"), "实体编码绕过:{html}");
        assert!(!html.contains("alert(2)"), "空白填充绕过:{html}");
    }

    #[test]
    fn json_renders_collapsible_tree() {
        let html = render_json(r#"{"a": 1, "b": [true, null, "x"], "c": {"d": 2}}"#);
        assert!(html.contains("<details open>"), "顶层展开:{html}");
        assert!(html.contains("\"a\""), "{html}");
        assert!(html.contains("3 项"), "{html}");
        assert!(html.contains("j-null"), "{html}");
        // 嵌套节点默认折叠:open 的 details 只应有顶层一个
        assert_eq!(html.matches("<details open>").count(), 1, "{html}");
        assert!(html.contains("<details><summary>[ … ]"), "嵌套折叠:{html}");
    }

    #[test]
    fn invalid_json_falls_back_to_text() {
        let html = render_json("{ broken");
        assert!(html.contains("JSON 解析失败"), "{html}");
        assert!(html.contains("<pre>"), "{html}");
    }

    #[test]
    fn detect_kind_recognizes_images() {
        for name in [
            "a.png", "a.JPG", "a.jpeg", "a.gif", "a.webp", "a.bmp", "a.avif",
        ] {
            assert_eq!(detect_kind(name), PreviewKind::Image, "{name}");
        }
        assert_eq!(detect_kind("a.svg"), PreviewKind::Svg);
        // 没有扩展名的文件名不能被当成扩展名本身
        assert_eq!(detect_kind("png"), PreviewKind::Text);
    }

    #[test]
    fn raster_image_page_embeds_raw_url() {
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let page = render_page("chart.png", "out/chart.png", 8, "tok", &png);
        assert!(page.contains("<img src=\"/p/tok/raw\""), "{page}");
        assert!(page.contains("alt=\"chart.png\""), "{page}");
        assert!(
            !page.contains("二进制文件"),
            "图片不该落到二进制提示页:{page}"
        );
    }

    #[test]
    fn svg_page_inlines_data_uri_without_raw_markup() {
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>";
        let page = render_page("x.svg", "x.svg", svg.len() as u64, "tok", svg);
        assert!(
            page.contains("<img src=\"data:image/svg+xml;base64,"),
            "{page}"
        );
        assert!(!page.contains("<script>"), "svg 源码不许进 DOM:{page}");
        assert!(
            !page.contains("/p/tok/raw\" alt"),
            "svg 不经 /raw 内联:{page}"
        );
    }

    #[test]
    fn image_page_escapes_filename_in_alt() {
        let page = render_page("a\"onerror=x.png", "a.png", 1, "tok", &[0xff]);
        assert!(!page.contains("\"onerror=x"), "{page}");
        assert!(page.contains("alt=\"a&quot;onerror=x.png\""), "{page}");
    }

    #[test]
    fn raster_image_mime_covers_every_image_kind() {
        for ext in ["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif"] {
            assert_eq!(detect_kind(&format!("a.{ext}")), PreviewKind::Image);
            assert!(raster_image_mime(ext).is_some(), "{ext}");
        }
        assert_eq!(raster_image_mime("svg"), None, "svg 不走 /raw 内联");
    }

    #[test]
    fn binary_bytes_get_download_page() {
        let page = render_page("x.bin", "/tmp/x.bin", 4, "tok", &[0xff, 0xfe, 0x00, 0x80]);
        assert!(page.contains("二进制文件"), "{page}");
        assert!(page.contains("/p/tok/raw"), "{page}");
    }

    #[test]
    fn page_escapes_filename_and_path() {
        let page = render_page("<b>.md", "/tmp/<b>.md", 3, "tok", b"abc");
        assert!(!page.contains("<h1><b></h1>"), "文件名必须转义:{page}");
        assert!(page.contains("&lt;b&gt;"), "{page}");
    }

    #[test]
    fn text_page_escapes_content() {
        let page = render_page("a.txt", "/tmp/a.txt", 10, "tok", b"<script>x</script>");
        assert!(!page.contains("<script>x</script>"), "{page}");
        assert!(page.contains("&lt;script&gt;"), "{page}");
    }

    #[test]
    fn directory_links_exclude_protected_identities() {
        let protected = [(1, 2), (1, 30), (1, 31)];
        for dir in protected {
            assert!(!directory_link_allowed(dir, &protected), "{dir:?}");
        }
        assert!(directory_link_allowed((1, 32), &protected));
        assert!(
            directory_link_allowed((2, 30), &protected),
            "设备不同即不同目录"
        );
        assert!(
            !directory_link_allowed((1, 32), &[]),
            "家目录取不到时一律不给"
        );
    }

    #[test]
    fn dir_listing_hides_dot_entries() {
        for name in [".ssh", ".env", ".git", "."] {
            assert!(!dir_listing_shows(name), "{name}");
        }
        for name in ["src", "a.env", "README.md"] {
            assert!(dir_listing_shows(name), "{name}");
        }
    }

    #[test]
    fn dir_page_links_files_and_shows_subdirectories_by_name() {
        let entries = vec![
            DirEntry {
                name: "sub".to_string(),
                is_dir: true,
                size: 0,
                url: None,
            },
            DirEntry {
                name: "a.md".to_string(),
                is_dir: false,
                size: 1024,
                url: Some("http://srv/p/tok-a".to_string()),
            },
        ];
        let page = render_dir_page("agents", "/tmp/agents", &entries, false);
        assert!(page.contains("<h1>agents/</h1>"), "{page}");
        assert!(page.contains("2 项"), "{page}");
        assert!(page.contains("📁 sub</td>"), "子目录只显示名字:{page}");
        assert_eq!(page.matches("<a href=").count(), 1, "只有文件是链接:{page}");
        assert!(
            page.contains("<a href=\"http://srv/p/tok-a\">a.md</a>"),
            "文件是链接:{page}"
        );
        assert!(page.contains("📁"), "目录标记:{page}");
        assert!(page.contains("1.0 KB"), "文件大小:{page}");
    }

    #[test]
    fn dir_page_empty() {
        let page = render_dir_page("empty", "/tmp/empty", &[], false);
        assert!(page.contains("空目录"), "{page}");
        assert!(!page.contains("<table"), "空目录不该有表格:{page}");
    }
}
