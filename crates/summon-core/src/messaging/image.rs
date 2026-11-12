//! 出站图片的纯件:扩展名白名单、大小上限、消息 `content` 构造,以及**回复正文里的内联图片解析**
//! (无 IO、无网络,必单测)。
//! 权威 spec:docs/spec/2026-08-03-outbound-image-design.md

/// 飞书图片上限 10MB(见开放平台「上传图片」)。**在本地判掉**——不拿飞书的 400 当校验器,
/// 那样错误要等一次完整上传之后才出现,还占带宽。
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// 一条回复里最多附几张图。超出的引用**保持原样文本**。
///
/// why 要有上限:每张图是一次同步上传,而这发生在终态回复的投递路径上——十几张图能把一条回复
/// 拖上好几秒。四张覆盖「贴个图表/截图」的绝大多数场景,再多通常是 agent 在列目录而非真要发图。
pub const MAX_INLINE_IMAGES: usize = 4;

/// 飞书支持的图片扩展名(小写比较)。列表取自开放平台「上传图片」的格式说明。
const IMAGE_EXTS: [&str; 9] = [
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "ico", "tiff", "heic",
];

/// 上传前的本地校验。`ext` 传文件扩展名(不含点,大小写不敏感),`bytes` 传文件字节数。
///
/// 失败一律给中文原因:这句话会原样回给 agent,它得据此决定是换个文件还是压缩一下,
/// 而不是把一串英文 API 错误转述给用户。
pub fn validate(ext: Option<&str>, bytes: u64) -> Result<(), String> {
    let Some(ext) = ext.map(|e| e.to_ascii_lowercase()) else {
        return Err(summon_i18n::format!("rcde9de4c47", IMAGE_EXTS.join("/")));
    };
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return Err(summon_i18n::format!("rddac81ea86", IMAGE_EXTS.join("/")));
    }
    if bytes == 0 {
        return Err(summon_i18n::text!("ra22b9b4888").to_string());
    }
    if bytes > MAX_IMAGE_BYTES {
        return Err(summon_i18n::format!(
            "r1fe30d0dfe",
            bytes as f64 / 1024.0 / 1024.0
        ));
    }
    Ok(())
}

/// 图片消息的 `content` 字段(飞书要求是** JSON 字符串**,不是嵌套对象)。
pub fn message_content(image_key: &str) -> String {
    serde_json::json!({ "image_key": image_key }).to_string()
}

/// 回复正文里的一处内联图片引用:markdown 标准语法 `![alt](path)`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineImage {
    pub alt: String,
    pub path: String,
    /// 在原文中的字节范围(含 `![` 到 `)`),供 [`split_by_images`] 切段。
    pub start: usize,
    pub end: usize,
}

/// 从回复正文里找出内联图片引用。
///
/// **用 markdown 标准语法,不发明私有标记。** 本仓 2026-06 删掉过一整套 `[[im]]` marker 子系统
/// (随 TUI 抓屏一起退场),那套要专门教 agent 输出私有标记;`![](…)` 是 agent 本来就会写的东西,
/// 不需要教、也不需要在 PRIMER 里维护一份协议。
///
/// 三条收紧,都是为了「宁可不发,也不要发错」:
/// - **只认绝对路径与 `~/`**:相对路径的基准目录有歧义(agent 的 cwd 会被 `/cd` 改),不猜。
/// - **跳过 fenced code block**:agent 在 ``` 里展示 markdown 示例时,不该被当成真要发图。
/// - **最多 [`MAX_INLINE_IMAGES`] 张**:多出来的原样留作文本。
pub fn parse_inline_images(text: &str) -> Vec<InlineImage> {
    let fences = fenced_ranges(text);
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 2 < bytes.len() {
        if bytes[i] != b'!' || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let Some(parsed) = parse_one_at(text, i) else {
            i += 1;
            continue;
        };
        // 落在代码块里的示例不算数。
        if fences
            .iter()
            .any(|(s, e)| parsed.start >= *s && parsed.start < *e)
        {
            i = parsed.end;
            continue;
        }
        i = parsed.end;
        if is_supported_path(&parsed.path) {
            out.push(parsed);
            if out.len() >= MAX_INLINE_IMAGES {
                break;
            }
        }
    }
    out
}

/// 解析 `text[at..]` 处的 `![alt](path)`;不成形则 `None`。
fn parse_one_at(text: &str, at: usize) -> Option<InlineImage> {
    let rest = text.get(at..)?;
    let close_alt = rest.find("](")?;
    // alt 里不允许换行:那多半是把两处无关的 `![` 和 `](` 撞在了一起。
    let alt = rest.get(2..close_alt)?;
    if alt.contains('\n') {
        return None;
    }
    let after = close_alt + 2;
    let tail = rest.get(after..)?;
    let close = tail.find(')')?;
    let path = tail.get(..close)?.trim();
    if path.is_empty() || path.contains('\n') {
        return None;
    }
    Some(InlineImage {
        alt: alt.trim().to_string(),
        path: path.to_string(),
        start: at,
        end: at + after + close + 1,
    })
}

/// 只接受绝对路径与 `~/` 开头,且扩展名在图片白名单内。
fn is_supported_path(path: &str) -> bool {
    if !(path.starts_with('/') || path.starts_with("~/")) {
        return false;
    }
    let ext = path.rsplit_once('.').map(|(_, e)| e);
    ext.is_some_and(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// ``` 围栏的字节区间(成对;落单的开围栏一直算到文末)。
fn fenced_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut open: Option<usize> = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            match open.take() {
                Some(start) => out.push((start, offset + line.len())),
                None => open = Some(offset),
            }
        }
        offset += line.len();
    }
    if let Some(start) = open {
        out.push((start, text.len()));
    }
    out
}

/// 卡片正文的一个片段:要么是文本,要么是一张已上传的图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyPiece {
    Text(String),
    Image { key: String, alt: String },
}

/// 按图片引用把正文切成交替片段。
///
/// `resolved` 是**上传成功**的 `path → image_key`;不在其中的引用(文件不存在、格式不对、上传失败)
/// **原样保留成文本**——图没附上是可惜,但绝不能因此让这条回复发不出去。
pub fn split_by_images(
    text: &str,
    images: &[InlineImage],
    resolved: &std::collections::BTreeMap<String, String>,
) -> Vec<ReplyPiece> {
    let mut out: Vec<ReplyPiece> = Vec::new();
    let mut cursor = 0usize;
    let push_text = |out: &mut Vec<ReplyPiece>, s: &str| {
        if s.is_empty() {
            return;
        }
        // 相邻文本合并:未 resolved 的引用留在文本里时,不该把一段话切成碎片。
        if let Some(ReplyPiece::Text(last)) = out.last_mut() {
            last.push_str(s);
        } else {
            out.push(ReplyPiece::Text(s.to_string()));
        }
    };
    for img in images {
        let Some(key) = resolved.get(&img.path) else {
            continue; // 没传上去 → 连同它的原文留给下一段文本
        };
        if img.start > cursor {
            push_text(&mut out, &text[cursor..img.start]);
        }
        out.push(ReplyPiece::Image {
            key: key.clone(),
            alt: img.alt.clone(),
        });
        cursor = img.end;
    }
    if cursor < text.len() {
        push_text(&mut out, &text[cursor..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_every_supported_ext_case_insensitively() {
        for ext in IMAGE_EXTS {
            assert!(validate(Some(ext), 1024).is_ok(), "{ext} 应被接受");
            assert!(
                validate(Some(&ext.to_ascii_uppercase()), 1024).is_ok(),
                "{ext} 大写也该被接受"
            );
        }
    }

    #[test]
    fn validate_rejects_non_images_with_readable_reason() {
        // 这些错误串会原样回给 agent,得说清楚「支持什么」而不只是「不支持」。
        let e = validate(Some("pdf"), 1024).unwrap_err();
        assert!(e.contains("pdf") && e.contains("png"), "{e}");
        let e = validate(None, 1024).unwrap_err();
        assert!(e.contains("扩展名"), "{e}");
    }

    #[test]
    fn validate_enforces_size_bounds() {
        assert!(
            validate(Some("png"), MAX_IMAGE_BYTES).is_ok(),
            "上限本身可用"
        );
        let e = validate(Some("png"), MAX_IMAGE_BYTES + 1).unwrap_err();
        assert!(e.contains("10MB"), "{e}");
        // 超限提示要带上实际大小,否则用户不知道差多少。
        let e = validate(Some("png"), 25 * 1024 * 1024).unwrap_err();
        assert!(e.contains("25.0MB"), "{e}");
        assert!(validate(Some("png"), 0).is_err(), "空文件要拒");
    }

    fn paths(text: &str) -> Vec<String> {
        parse_inline_images(text)
            .into_iter()
            .map(|i| i.path)
            .collect()
    }

    #[test]
    fn parses_standard_markdown_image_syntax() {
        let t = "分析完了,趋势见下图。\n\n![季度趋势](/tmp/chart.png)\n\n结论:环比 +12%";
        let imgs = parse_inline_images(t);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].path, "/tmp/chart.png");
        assert_eq!(imgs[0].alt, "季度趋势");
        // 字节范围要正好框住整个 `![…](…)`,切段才不会漏字或吃字。
        assert_eq!(
            &t[imgs[0].start..imgs[0].end],
            "![季度趋势](/tmp/chart.png)"
        );
        // 空 alt 是最常见的写法。
        assert_eq!(paths("![](~/a.png)"), vec!["~/a.png"]);
    }

    #[test]
    fn only_accepts_absolute_or_tilde_image_paths() {
        // 相对路径不猜:agent 的 cwd 会被 /cd 改,基准目录有歧义。
        assert!(paths("![](chart.png)").is_empty());
        assert!(paths("![](./chart.png)").is_empty());
        assert!(paths("![](../chart.png)").is_empty());
        // 网图不碰(要下载,是另一件事)。
        assert!(paths("![](https://example.com/a.png)").is_empty());
        // 非图片扩展名不碰。
        assert!(paths("![](/tmp/a.pdf)").is_empty());
        assert!(paths("![](/tmp/noext)").is_empty());
        // 大小写扩展名要认。
        assert_eq!(paths("![](/tmp/A.PNG)"), vec!["/tmp/A.PNG"]);
    }

    #[test]
    fn skips_images_inside_fenced_code_blocks() {
        // agent 在代码块里演示 markdown 语法时,不该被当成真要发图。
        let t = "这样写就能发图:\n\n```\n![](/tmp/example.png)\n```\n\n实际的图:![](/tmp/real.png)";
        assert_eq!(paths(t), vec!["/tmp/real.png"]);
        // 落单的开围栏:后面全算代码块,宁可少发不可错发。
        assert!(paths("```\n![](/tmp/x.png)").is_empty());
    }

    #[test]
    fn caps_the_number_of_inline_images() {
        // 每张图是一次同步上传,而这发生在终态回复的投递路径上。
        let many: String = (0..10).map(|i| format!("![](/tmp/{i}.png)\n")).collect();
        assert_eq!(parse_inline_images(&many).len(), MAX_INLINE_IMAGES);
    }

    #[test]
    fn ignores_malformed_references_without_panicking() {
        assert!(paths("![](").is_empty());
        assert!(paths("![no close](/tmp/a.png").is_empty());
        assert!(paths("![](  )").is_empty());
        assert!(paths("![alt\nwith newline](/tmp/a.png)").is_empty());
        // 多字节字符边界:切片不能落在字符中间(会 panic)。
        assert!(paths("图![说明](/tmp/图.png)表").len() <= 1);
        assert_eq!(paths("图![说明](/tmp/图.png)表"), vec!["/tmp/图.png"]);
    }

    #[test]
    fn split_puts_images_in_place_and_keeps_unresolved_as_text() {
        let t = "前言\n\n![图A](/tmp/a.png)\n\n中间\n\n![图B](/tmp/b.png)\n\n结尾";
        let imgs = parse_inline_images(t);
        assert_eq!(imgs.len(), 2);
        // 只有 a.png 传上去了;b.png 保持原文本。
        let mut resolved = std::collections::BTreeMap::new();
        resolved.insert("/tmp/a.png".to_string(), "img_a".to_string());
        let pieces = split_by_images(t, &imgs, &resolved);

        assert_eq!(pieces[0], ReplyPiece::Text("前言\n\n".into()));
        assert_eq!(
            pieces[1],
            ReplyPiece::Image {
                key: "img_a".into(),
                alt: "图A".into()
            }
        );
        // b 未 resolved → 它的原文留在后续文本里,且相邻文本已合并成一段。
        let ReplyPiece::Text(tail) = &pieces[2] else {
            panic!("尾段应是文本: {pieces:?}");
        };
        assert!(
            tail.contains("中间") && tail.contains("![图B](/tmp/b.png)") && tail.contains("结尾")
        );
        assert_eq!(pieces.len(), 3);

        // 一张都没传上 → 整段原样,等价于什么都没做。
        let none = split_by_images(t, &imgs, &std::collections::BTreeMap::new());
        assert_eq!(none, vec![ReplyPiece::Text(t.to_string())]);
    }

    #[test]
    fn split_without_images_is_a_single_text_piece() {
        // 绝大多数回复没有图:这条路径必须零开销、零改变。
        let t = "普通回复,没有任何图片。";
        let pieces = split_by_images(t, &[], &std::collections::BTreeMap::new());
        assert_eq!(pieces, vec![ReplyPiece::Text(t.to_string())]);
    }

    #[test]
    fn message_content_is_a_json_string_with_image_key() {
        let c = message_content("img_v2_abc");
        assert_eq!(c, r#"{"image_key":"img_v2_abc"}"#);
        // 必须是可解析的 JSON 字符串——飞书对 content 的要求是「JSON 序列化后的字符串」。
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["image_key"], "img_v2_abc");
    }
}
