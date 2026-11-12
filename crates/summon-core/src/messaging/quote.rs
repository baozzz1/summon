//! 被引用消息(飞书「引用回复」)的纯文本抽取与上下文渲染。
//! webhook 的 `parent_id` 指向被引用消息;daemon 据此拉回原消息,用这里的纯函数把
//! 各类 content(text/post/卡片)尽力抽成可读文本,再拼成喂给 agent 的「引用上下文」。
//! 纯逻辑放 core 便于单测;网络拉取在 `summon` 层。
//!
//! 对于文件/图片/语音/视频类被引用消息,`quoted_plaintext_full` 抽不到文本 → 返回空;
//! 此时用 `extract_quoted_resource` 从 content JSON 里提取资源键,交给 daemon 下载落盘。

use crate::messaging::ipc::Resource;

/// 引用上下文最多带多少字符,避免被引用的长卡片把 prompt 撑爆(超出截断 + 省略号)。
const MAX_QUOTED_CHARS: usize = 1200;

/// 卡片里 @ 的 open_id 查不到名字时的占位。**不回落到 open_id 本身**:那串 `ou_…` 会被
/// agent 原样抄进回复,而回复里出现 open_id 是明令禁止的。
const UNRESOLVED_MENTION: &str = "r61edb99c5b";

/// 被引用消息里的 @ 提及怎么还原成人名。两条来源、两种形态,2026-08-12 真机 mget 实测确证:
/// - **文本消息**:正文里只留占位(`@_user_1`),名字在同一响应的 `mentions` 数组里 —— 查表。
/// - **卡片**:`mentions` 字段直接是 `null`(即便卡片 AST 里确实有 `at` 节点),AST 的 `at`
///   只有 `property.userID`(open_id) —— 名字只能回查通讯录缓存,所以给一个回调。
///
/// 两者共存于同一个类型,是因为调用方拿不准被引用的是哪种;`default()` 表示「没有任何来源」,
/// 此时占位保持原样、卡片里的 @ 渲染成 `@某人` —— 都比整块丢掉强。
/// 卡片 `at` 节点的 open_id → 名字回查(实现上是群成员名缓存)。
type OpenIdResolver<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Default, Clone, Copy)]
pub struct Mentions<'a> {
    keys: &'a [(String, String)],
    resolve_open_id: Option<OpenIdResolver<'a>>,
}

impl<'a> Mentions<'a> {
    /// `keys` 是 mget 响应 `mentions` 数组的 `(key, name)`;`resolve_open_id` 查通讯录缓存。
    pub fn new(keys: &'a [(String, String)], resolve_open_id: OpenIdResolver<'a>) -> Self {
        Self {
            keys,
            resolve_open_id: Some(resolve_open_id),
        }
    }

    fn name_for_open_id(&self, open_id: &str) -> Option<String> {
        self.resolve_open_id
            .and_then(|f| f(open_id))
            .filter(|name| !name.is_empty())
    }

    /// 把文本里的 `@_user_N` 占位换成 `@名字`。**长 key 先换**:否则 `@_user_1` 会先吃掉
    /// `@_user_10` 的前缀,把第 10 个人的 @ 切成「@名字0」。
    fn substitute_keys(&self, text: &str) -> String {
        if self.keys.is_empty() {
            return text.to_string();
        }
        let mut ordered: Vec<&(String, String)> = self.keys.iter().collect();
        ordered.sort_by_key(|(key, _)| std::cmp::Reverse(key.len()));
        let mut out = text.to_string();
        for (key, name) in ordered {
            if key.is_empty() {
                continue;
            }
            let name = match name.trim() {
                "" => ::summon_core::i18n::label(UNRESOLVED_MENTION),
                name => name,
            };
            out = out.replace(key.as_str(), &format!("@{name}"));
        }
        out
    }
}

/// 从飞书消息的 `msg_type` + `content`(content 是 JSON 字符串)里尽力抽出可读纯文本。
/// - text:取 `.text`;
/// - 其它(post / interactive 卡片 / 富媒体):递归收集 `text` / `content` 串字段兜底拼接。
///
/// 抽取被引用消息的**完整**纯文本(不截断)。长引用 spill-to-file 用:写盘留全文,prompt 只给摘要。
/// 解析失败或抽不到内容回空串。
pub fn quoted_plaintext_full(msg_type: &str, content: &str, mentions: Mentions<'_>) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
        return String::new();
    };
    match msg_type {
        "text" => mentions.substitute_keys(v["text"].as_str().unwrap_or_default().trim()),
        "post" => post_plaintext(&v, mentions),
        _ => {
            // 拉取时带了 `card_msg_content_type=raw_card_content`:卡片以保真 AST 回来,
            // 形如 `{"json_card":"<嵌套 JSON 串>","card_schema":N}`。样式、原始 `<>`、换行都在。
            if let Some(ast) = v
                .get("json_card")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            {
                let rendered = render_card_ast(&ast, mentions);
                if !rendered.is_empty() {
                    return rendered;
                }
            }
            // 降级结构(post / 未带 raw_card_content 的卡片):飞书把 `<` 编成实体,需解码。
            mentions.substitute_keys(&decode_entities(&harvest_text(&v)))
        }
    }
}

/// 富文本 post 的逐元素解析,与 sidecar `parsePost` 同一套规则。
///
/// **不走下面那条 `harvest_text` 兜底**:它只是把所有 `text` 字段串起来,于是代码块没有围栏、
/// 链接丢掉地址、元素之间不分行,agent 读到的是一团。同一条消息直接发过来和被引用,
/// 看到的内容必须是一回事,否则「引用」会悄悄变成信息更少的那一份。
///
/// 与 sidecar 的一处有意差异:内嵌图片/视频在这条路上没有下载通路,故给占位而不是收附件。
fn post_plaintext(v: &serde_json::Value, mentions: Mentions<'_>) -> String {
    let mut out = String::new();
    if let Some(title) = v.get("title").and_then(serde_json::Value::as_str) {
        if !title.is_empty() {
            out.push_str(title);
            out.push('\n');
        }
    }
    let paragraphs = v
        .get("content")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten();
    for para in paragraphs {
        for el in para.as_array().into_iter().flatten() {
            let str_of = |key: &str| {
                el.get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
            };
            let text = str_of("text");
            match str_of("tag") {
                "img" => out.push_str(summon_i18n::text!("ra71f445d35")),
                "media" => out.push_str(summon_i18n::text!("rda5d0399eb")),
                "a" => match str_of("href") {
                    "" => out.push_str(text),
                    href => out.push_str(&format!("[{text}]({href})")),
                },
                "at" => {
                    // open_id 先回查通讯录,查不到退到元素自带的名字;都没有就不写 ——
                    // 裸 open_id 对 agent 没有可读性。
                    let name = mentions
                        .name_for_open_id(str_of("user_id"))
                        .unwrap_or_else(|| str_of("user_name").to_string());
                    if !name.is_empty() {
                        out.push_str(&format!("@{name}"));
                    }
                }
                "code_block" => out.push_str(&format!(
                    "\n```{}\n{text}\n```\n",
                    str_of("language").to_lowercase()
                )),
                "emotion" => {
                    let emoji = str_of("emoji_type");
                    if !emoji.is_empty() {
                        out.push_str(&format!(":{emoji}:"));
                    }
                }
                "hr" => out.push_str("\n---\n"),
                // text / md,以及飞书将来新增的任何带 text 的元素:只要带正文就不丢。
                _ => out.push_str(text),
            }
        }
        out.push('\n');
    }
    mentions.substitute_keys(out.trim())
}

/// 合并转发里一条待渲染的嵌套消息。
pub struct MergeForwardItem<'a> {
    pub sender: &'a str,
    pub msg_type: &'a str,
    pub content: &'a str,
    /// 每条嵌套消息有各自的 @ 提及,不能共用外层的。
    pub mentions: Mentions<'a>,
}

/// 把合并转发里的多条嵌套消息渲染成一段完整引用文本。
///
/// 飞书 `merge_forward` 外层正文只是固定占位，真正内容位于同一 GET 响应的 nested items。
/// 调用方负责过滤外层 item；这里保持与 sidecar 入站展开一致的可读格式，并给无法抽成文本的
/// 资源保留类型/文件名占位，避免引用上下文静默变空。
pub fn merge_forward_plaintext(items: &[MergeForwardItem<'_>]) -> String {
    if items.is_empty() {
        return summon_i18n::text!("rce34f452ff").to_string();
    }

    let mut out = summon_i18n::format!("rec58cafeda", items.len());
    for item in items {
        out.push_str("\n\n---\n");
        let sender = match item.sender.trim() {
            "" => summon_i18n::text!("r1be0cad4e9"),
            sender => sender,
        };
        out.push_str(&summon_i18n::format!("re8babae35a"));
        let text = quoted_plaintext_full(item.msg_type, item.content, item.mentions);
        if !text.is_empty() {
            out.push_str(&text);
            continue;
        }
        match item.msg_type {
            "image" => out.push_str(summon_i18n::text!("ra71f445d35")),
            "file" => {
                let name = serde_json::from_str::<serde_json::Value>(item.content)
                    .ok()
                    .and_then(|v| v["file_name"].as_str().map(str::to_string));
                match name.as_deref().filter(|name| !name.is_empty()) {
                    Some(name) => out.push_str(&summon_i18n::format!("rc6ff19efc2")),
                    None => out.push_str(summon_i18n::text!("r54e20fcfb5")),
                }
            }
            "audio" => out.push_str(summon_i18n::text!("rfee630ed57")),
            "media" => out.push_str(summon_i18n::text!("rda5d0399eb")),
            "text" | "post" => {}
            other => out.push_str(&format!("[{other}]")),
        }
    }
    out
}

/// 渲染 `raw_card_content` 的卡片 AST(`{"body":{"elements":[...]}}`)成纯文本。
fn render_card_ast(ast: &serde_json::Value, mentions: Mentions<'_>) -> String {
    // **卡片 1.0 与 2.0 的 body 形状不同,两种都要认。**
    //
    // 1.0:`body.elements[]`;2.0:`body` 自己是个节点(`{id, property:{elements:[...]}, tag}`),
    // 元素在 `body.property.elements[]`。飞书现在回的是 2.0 —— 2026-08-12 用真机 mget 响应
    // 确证(夹具 `tests/fixtures/quoted_card_2_0.json`,`body` 顶层只有 id/property/tag,
    // 没有 elements)。
    //
    // 只认 1.0 的后果是**静默返回空**:引用一张卡片时正文一个字都提不出,而
    // `fetch_quote_note` 对空正文一路落到「全空则返回空串」,agent 连"用户引用过东西"都不
    // 知道,链路上没有任何报错。owner 实测的「引用卡片看不到内容」就是这个。
    //
    // 之所以拖到今天才发现:所有 `json_card` 测试夹具都是**手写的 1.0 结构**,与真实 API
    // 形状不符 —— 测试全绿、生产全空。本次修复的第一件事就是把真机响应落成夹具。
    //
    // 内层不用改:`render_md_node` 本来就读 `property.elements`,2.0 的每个 block 正是这个形状。
    let elements = ast["body"]["elements"]
        .as_array()
        .or_else(|| ast["body"]["property"]["elements"].as_array());
    let Some(elements) = elements else {
        return String::new();
    };
    let mut out = String::new();
    for el in elements {
        // `fallback` 与 `property` 是同一内容的两种表示(前者扁平、markdown 标记已在 content 里)。
        // 必须二选一,否则正文翻倍;取 fallback 更省事——列表/换行都已摊平进文本。
        render_md_node(el.get("fallback").unwrap_or(el), &mut out, mentions);
    }
    out.trim().to_string()
}

/// 渲染一个 markdown 节点的 `property.elements`:`plain_text` 直接拼、`br` 换行、`list` 逐项成行。
/// 同段落内的多个 run(按加粗等样式切分)**直接拼接**——它们本就是一句话。
fn render_md_node(node: &serde_json::Value, out: &mut String, mentions: Mentions<'_>) {
    let Some(elements) = node["property"]["elements"].as_array() else {
        return;
    };
    for e in elements {
        match e["tag"].as_str().unwrap_or_default() {
            "br" => out.push('\n'),
            // `code_span` 是**反引号代码片段**,文本和 `plain_text` 一样在 `property.content`,
            // 只是 tag 不同 —— 此前落进下面的 `_ => {}` 被整块丢掉。owner 2026-08-12 引用一条
            // 满是代码片段的卡片实测:`提交(18db85c)` 到对面变成 `提交()`,句子还在、关键信息
            // 全没了,比整段丢失更难察觉。真机夹具里它有 19 个,是出现频次第二高的 tag。
            //
            // 用反引号包回去:读的人要能分辨哪段是代码,而且原文本来就是这么写的。
            "code_span" => {
                out.push('`');
                push_content(e, out);
                out.push('`');
            }
            "plain_text" => push_content(e, out),
            "list" => render_list(e, out),
            // `at` 是卡片里的 @ 提及,结构只有 `property.userID`(open_id),名字不在卡片里 ——
            // 实测 interactive 消息的 `mentions` 字段是 `null`,文本那套「占位 + 数组」用不上,
            // 只能回查通讯录缓存。查不到退成 `@某人`:含糊胜过整块丢掉,丢掉会让引用读起来
            // 像在对空气说话(owner 实测「@某人 帮我看下」被抽成「 帮我看下」)。
            "at" => {
                out.push('@');
                let name = e["property"]["userID"]
                    .as_str()
                    .and_then(|id| mentions.name_for_open_id(id));
                out.push_str(
                    name.as_deref()
                        .unwrap_or(::summon_core::i18n::label(UNRESOLVED_MENTION)),
                );
            }
            _ => {}
        }
    }
}

fn render_list(e: &serde_json::Value, out: &mut String) {
    let Some(items) = e["property"]["items"].as_array() else {
        return;
    };
    for item in items {
        out.push_str("- ");
        if let Some(runs) = item["elements"].as_array() {
            runs.iter().for_each(|r| push_content(r, out));
        }
        out.push('\n');
    }
}

fn push_content(e: &serde_json::Value, out: &mut String) {
    if let Some(c) = e["property"]["content"].as_str() {
        out.push_str(c);
    }
}

/// 解码飞书降级结构里的 HTML 实体。`&amp;` 必须最后解,否则 `&amp;lt;` 会被二次解码成 `<`。
fn decode_entities(s: &str) -> String {
    s.replace("&#60;", "<")
        .replace("&#62;", ">")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
/// (向后兼容)抽取并按 `MAX_QUOTED_CHARS` 截断。短引用内联仍走它;长引用改走
/// `quoted_plaintext_full` + spill。解析失败或抽不到内容回空串,调用方据空决定是否注入。
pub(crate) fn quoted_plaintext(msg_type: &str, content: &str, mentions: Mentions<'_>) -> String {
    truncate_chars(
        &quoted_plaintext_full(msg_type, content, mentions),
        MAX_QUOTED_CHARS,
    )
}

/// 已有完整文本但 spill 落盘失败时的内联兜底。与 `quoted_plaintext` 共用同一上限，避免故障
/// 路径把超长合并转发整段塞进 prompt。
pub fn quote_inline_fallback(full_text: &str) -> String {
    truncate_chars(full_text, MAX_QUOTED_CHARS)
}

/// 引用超过此字符数就 spill 到本地文件、prompt 里只给「摘要 + 路径」——长引用既不撑爆 prompt、也不
/// 因截断丢信息(原 1200 截断会把后文丢掉,正是 owner 反馈的问题)。短引用仍内联全文。
pub const QUOTE_INLINE_MAX_CHARS: usize = 600;
/// spill 时 prompt 里内联的摘要长度(给 agent 即时上下文,完整在文件里按需 Read)。
pub const QUOTE_PREVIEW_CHARS: usize = 280;

/// 引用是否该 spill 到文件(完整文本超过内联上限)。
pub fn should_spill_quote(full_text: &str) -> bool {
    full_text.chars().count() > QUOTE_INLINE_MAX_CHARS
}

/// 取引用前 `QUOTE_PREVIEW_CHARS` 字作摘要(spill 时内联;不带省略号,省略号由 render 拼)。
pub fn quote_preview(full_text: &str) -> String {
    full_text
        .trim()
        .chars()
        .take(QUOTE_PREVIEW_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// 写进本地引用文件的完整内容(带元信息头,保真:谁发的、原消息 id)。纯函数,便于单测。
pub fn quote_file_markdown(quoted_sender: &str, parent_id: &str, full_text: &str) -> String {
    let who = quoted_sender.trim();
    let who = if who.is_empty() {
        summon_i18n::text!("rddfdac569e")
    } else {
        who
    };
    summon_i18n::format!("rf1c9531288", full_text.trim())
}

/// spill 版引用上下文:prompt 里只放「摘要 + 文件路径」,让 agent 按需 Read 全文(复用附件「给路径」
/// 范式,体验一致)。完整原文已写到 `file_path`。
pub fn render_quote_note_spilled(quoted_sender: &str, preview: &str, file_path: &str) -> String {
    let who = quoted_sender.trim();
    let who = if who.is_empty() {
        summon_i18n::text!("rddfdac569e")
    } else {
        who
    };
    summon_i18n::format!("r6c3de4a420", preview.trim())
}

/// 把抽出的被引用文本拼成给 agent 的「引用上下文」段落。空内容给出占位说明。
/// 措辞明确告诉 agent:这是用户引用/回复的消息,应优先据此理解意图。
pub fn render_quote_note(quoted_sender: &str, quoted_text: &str) -> String {
    let who = if quoted_sender.trim().is_empty() {
        summon_i18n::text!("rddfdac569e")
    } else {
        quoted_sender.trim()
    };
    let body = quoted_text.trim();
    if body.is_empty() {
        return summon_i18n::format!("r697969bff3");
    }
    summon_i18n::format!("rc54759fe97")
}

/// 被引用消息是文件/图片/语音/视频时,从 content JSON 提取资源键 + 文件名。
/// 抽不到(非资源类型 / JSON 损坏)返回 None。纯函数,便于单测。
///
/// 飞书各资源类型的 content 结构:
/// - file:  `{"file_key":"file_v3_xxx","file_name":"report.pdf"}`
/// - image: `{"image_key":"img_v3_xxx"}`(无文件名)
/// - audio: `{"file_key":"file_v3_xxx","duration":...}`
/// - media: `{"file_key":"file_v3_xxx","file_name":"video.mp4","duration":...}`
pub fn extract_quoted_resource(msg_type: &str, content: &str) -> Option<Resource> {
    let is_resource = matches!(msg_type, "file" | "image" | "audio" | "media");
    if !is_resource {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    // image 用 image_key,其余用 file_key
    let file_key = if msg_type == "image" {
        v["image_key"].as_str()
    } else {
        v["file_key"].as_str()
    }
    .unwrap_or_default()
    .to_string();
    if file_key.is_empty() {
        return None;
    }
    let name = v["file_name"].as_str().unwrap_or_default().to_string();
    Some(Resource {
        file_key,
        kind: msg_type.to_string(),
        name,
    })
}

/// 渲染「被引用消息含附件」的上下文注记(与 `resource_note` 风格一致,但带引用语境)。
/// `saved` = `[(kind, local_path)]`,与 `resource_note` 入参同构。
pub fn render_quote_note_with_files(quoted_sender: &str, saved: &[(String, String)]) -> String {
    let who = if quoted_sender.trim().is_empty() {
        summon_i18n::text!("rddfdac569e")
    } else {
        quoted_sender.trim()
    };
    let file_list = crate::attach::resource_note(saved);
    summon_i18n::format!("rc17e07cc5e")
}

/// 递归收集 JSON 里 `text` / `content` 两类串字段(卡片 markdown 元素、post 文本段都落这两个键)。
///
/// post 与「降级卡片」的正文是**行数组**(`[[run, run], [run]]`):外层一项一行,内层是同一行里按
/// 样式切分出的 run。同一行的 run 必须**直接拼接**——它们本就是一句话,拿 `\n` 连会把句子拆断。
/// 其余形态(普通卡片的一维 elements)保持原样:一个元素一行。
fn harvest_text(v: &serde_json::Value) -> String {
    if let Some(rows) = v
        .get("content")
        .or_else(|| v.get("elements"))
        .and_then(serde_json::Value::as_array)
        .filter(|rows| !rows.is_empty() && rows.iter().all(serde_json::Value::is_array))
    {
        let lines: Vec<String> = rows
            .iter()
            .map(|row| {
                let mut runs: Vec<String> = Vec::new();
                harvest_into(row, &mut runs);
                runs.join("")
            })
            .collect();
        return lines.join("\n").trim().to_string();
    }
    let mut out: Vec<String> = Vec::new();
    harvest_into(v, &mut out);
    out.join("\n").trim().to_string()
}

fn harvest_into(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            // 链接型节点(按钮 / 富文本超链)→ 输出「文案 (链接)」,让 agent 感知到按钮背后的 URL
            // (owner 反馈:被引用卡片里按钮的链接抽不出来)。命中即不再深入本节点,避免文案被重复收。
            if let Some(link) = extract_link(map) {
                out.push(link);
                return;
            }
            for (k, child) in map {
                if k == "text" || k == "content" {
                    if let Some(s) = child.as_str() {
                        let t = s.trim();
                        if !t.is_empty() {
                            out.push(t.to_string());
                        }
                        continue;
                    }
                }
                harvest_into(child, out);
            }
        }
        serde_json::Value::Array(arr) => arr.iter().for_each(|c| harvest_into(c, out)),
        _ => {}
    }
}

/// 从一个 JSON 对象里尽力抽出「可点链接」:覆盖飞书卡片按钮的多种形态——
/// `url` / `multi_url.url`(卡片 1.0 按钮)、`behaviors[].default_url`(卡片 2.0 行为式按钮)、
/// `href`(post 富文本 `a` 标签)。命中则连同文案返回「文案 (链接)」,无文案就只给链接。
/// 不是链接节点(无上述键)返回 None,交回普通递归。
fn extract_link(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let url = map
        .get("url")
        .and_then(serde_json::Value::as_str)
        .or_else(|| map.get("href").and_then(serde_json::Value::as_str))
        .or_else(|| {
            map.get("multi_url")
                .and_then(|m| m.get("url"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            map.get("behaviors")
                .and_then(serde_json::Value::as_array)
                .and_then(|arr| arr.iter().find_map(behavior_url))
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    // 文案:按钮多为 `text.content`/`text`(post `a` 标签),兜底本节点的 `content` 串
    let label = map
        .get("text")
        .and_then(|t| {
            t.get("content")
                .and_then(serde_json::Value::as_str)
                .or(t.as_str())
        })
        .or_else(|| map.get("content").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Some(match label {
        Some(l) => format!("{l} ({url})"),
        None => url.to_string(),
    })
}

/// 卡片 2.0 `behaviors` 数组里取 open_url 行为的链接(优先 `default_url`,回退 `url`)。
fn behavior_url(b: &serde_json::Value) -> Option<&str> {
    b.get("default_url")
        .and_then(serde_json::Value::as_str)
        .or_else(|| b.get("url").and_then(serde_json::Value::as_str))
}

/// 按「字符数」(非字节)截断,超长加省略号。中文友好,避免切坏 UTF-8。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    summon_i18n::format!("r94d8ff5ead", kept.trim_end())
}

#[cfg(test)]
#[cfg(test)]
mod quote_hint_tests {
    use super::*;

    /// **引用自己发的消息不塞全文** —— 那段内容已经在 agent 上下文里,再灌一遍是重复计费。
    /// 只需要让它认出"是哪一条"。
    /// 富文本的九种 tag(飞书 `create_json` 文档)在引用路径上也要各自留痕,规则与 sidecar
    /// `parsePost` 一致 —— 夹具是同一份。同一条消息直接发和被引用,agent 看到的不能是两样。
    const POST_ALL_TAGS: &str = r#"{"title":"标题","content":[[{"tag":"text","text":"文本"},{"tag":"a","text":"链接文字","href":"https://example.com"},{"tag":"at","user_id":"ou_known"},{"tag":"at","user_id":"ou_unknown","user_name":"名字兜底"},{"tag":"img","image_key":"img_1"},{"tag":"media","file_key":"file_1","image_key":"cover_1"},{"tag":"emotion","emoji_type":"SMILE"},{"tag":"code_block","language":"PYTHON","text":"print(1)"},{"tag":"hr"},{"tag":"md","text":"**粗体**"}]]}"#;

    #[test]
    fn 引用富文本时每一种元素都留痕() {
        let resolve = |id: &str| (id == "ou_known").then(|| "某人".to_string());
        let got = super::quoted_plaintext_full("post", POST_ALL_TAGS, Mentions::new(&[], &resolve));
        for want in [
            "标题",
            "文本",
            "[链接文字](https://example.com)",
            "@某人",     // open_id 回查通讯录
            "@名字兜底", // 查不到时退到元素自带的名字
            "[图片]",
            "[视频]",
            ":SMILE:",
            "```python\nprint(1)\n```",
            "---",
            "**粗体**",
        ] {
            assert!(got.contains(want), "缺 {want:?}:\n{got}");
        }
    }

    #[test]
    fn 自己的消息只给指代不给全文() {
        let digest =
            quote_digest("盘点如下——有一件事卡在我这里,该由你定。\n\n1. sentinel 的两个问题");
        let hint = render_quote_hint(&QuoteKind::SelfMessage {
            message_id: "om_abc",
            digest: &digest,
        });
        assert!(hint.contains("source=self body=history"), "{hint}");
        assert!(hint.contains("盘点如下"), "要有摘要供它对上号:{hint}");
        assert!(hint.contains("om_abc"), "留 id 供确需原文时取:{hint}");
        // 极短:这段是指代,不是内容。
        assert!(
            hint.chars().count() < 160,
            "指代不该长:{}",
            hint.chars().count()
        );
    }

    /// 控制台卡上是只给收件人本人的链接:同终端卡,只说是什么。
    #[test]
    fn a_quoted_dashboard_card_carries_no_id_and_no_way_to_fetch_it() {
        let hint = render_quote_hint(&QuoteKind::DashboardCard);
        assert!(hint.contains("控制台卡"), "{hint}");
        for leak in ["om_", "message_id", "mget", "lark-cli", "http"] {
            assert!(!hint.contains(leak), "{leak}:{hint}");
        }
    }

    /// 终端卡上是一次性链接:只说是一张终端卡,不给 id、不给取法。
    #[test]
    fn a_quoted_terminal_card_carries_no_id_and_no_way_to_fetch_it() {
        let hint = render_quote_hint(&QuoteKind::TerminalCard);
        assert!(hint.contains("终端卡"), "{hint}");
        for leak in ["om_", "message_id", "mget", "lark-cli"] {
            assert!(!hint.contains(leak), "{leak}:{hint}");
        }
    }

    /// 引用提示携带消息数据与手册入口，正文按需读取。
    #[test]
    fn 别人的引用带定位信息与手册入口() {
        let hint = render_quote_hint(&QuoteKind::ForeignUnreadable {
            message_id: "om_xyz",
        });
        assert!(hint.contains("source=other body=unavailable"), "{hint}");
        assert!(hint.contains("om_xyz"), "取用参数要齐:{hint}");
        assert!(hint.contains("guide=\"summon guide quotes\""), "{hint}");
        assert!(!hint.contains("+messages-mget"), "{hint}");
        assert!(!hint.contains("lark-cli"), "{hint}");
        assert!(hint.chars().count() < 85, "{hint}");
    }

    /// 摘要压成单行并截断:多行卡片直接进 prompt 会把"指代"撑成"内容"。
    #[test]
    fn 摘要压平并截断() {
        let d = quote_digest("第一行\n\n   第二行   \n第三行");
        assert_eq!(d, "第一行 第二行 第三行");
        let long = quote_digest(&"字".repeat(100));
        assert!(long.chars().count() <= QUOTE_DIGEST_CHARS + 1, "{d}");
        assert!(long.ends_with('…'), "截断要有省略号:{long}");
    }
}

#[cfg(test)]
mod card_placeholder_tests {
    use super::*;

    /// owner 2026-08-10:引用一张 bot 卡片,agent 读到的是「请升级至最新版本客户端」——
    /// 飞书对 interactive 的读回接口只给这个降级文本,带 `raw_card_content` 也一样。
    #[test]
    fn 识别飞书卡片降级占位() {
        assert!(looks_like_card_placeholder(
            "请升级至最新版本客户端，以查看内容"
        ));
        assert!(looks_like_card_placeholder(
            "  请升级至最新版本客户端，以查看内容  "
        ));
    }

    /// **误判的代价是拿归档正文覆盖真内容**,所以判据必须窄:长文本一律不算占位,
    /// 哪怕里面恰好提到了那句话(比如两个人正在讨论这个提示)。
    #[test]
    fn 长文本里提到那句话不算占位() {
        let real = "我刚才看到卡片显示请升级至最新版本客户端，以查看内容，这个是不是签名问题导致的?我们要不要先查一下 entitlements 再决定";
        assert!(!looks_like_card_placeholder(real));
    }

    /// 正常引用不受影响。
    #[test]
    fn 普通正文不被当成占位() {
        assert!(!looks_like_card_placeholder("这条消息里没有那种提示"));
        assert!(!looks_like_card_placeholder(""));
    }

    /// 归档解析:markdown 卡直接取正文;结构化卡片摘出可读文本;没有 message_id 的旧记录跳过。
    #[test]
    fn 从出站归档取回正文() {
        let md = r#"{"kind":"markdown_card","payload":{"message_id":"om_a","markdown":"真正的回复正文"}}"#;
        assert_eq!(
            outbound_record_body(md),
            Some(("om_a".to_string(), "真正的回复正文".to_string()))
        );

        let card = r#"{"kind":"fleet","payload":{"message_id":"om_b","card":{"schema":"2.0","body":{"elements":[{"tag":"markdown","content":"卡片里的字"}]}}}}"#;
        let (id, body) = outbound_record_body(card).expect("结构化卡片也要取得出正文");
        assert_eq!(id, "om_b");
        assert!(body.contains("卡片里的字"), "{body}");

        // 改动前写下的归档没有 message_id —— 跳过而不是 panic(老数据必须能共存)。
        let legacy = r#"{"kind":"markdown_card","payload":{"markdown":"老记录"}}"#;
        assert_eq!(outbound_record_body(legacy), None);
        assert_eq!(outbound_record_body("not json"), None);
    }
}

#[cfg(test)]
mod tests {
    /// **用真机 mget 响应做夹具**,不是手写的理想形状。
    ///
    /// 2026-08-12:所有 `json_card` 夹具此前都是手写的卡片 1.0 结构
    /// (`{"body":{"elements":[…]}}`),而飞书真实回的是 2.0(`body.property.elements`)。
    /// `render_card_ast` 只认 1.0 → 提取恒空 → 引用卡片的正文一个字都进不了 agent 上下文,
    /// 且全链路无报错。**测试全绿、生产全空**,靠手写夹具永远发现不了。
    ///
    /// 这条直接喂真实响应,断言能提到正文关键词。夹具来源:`im/v1/messages/mget`
    /// (`card_msg_content_type=raw_card_content`)对一条 bot 复杂卡的原样返回。
    /// 引用里的**反引号代码片段**不能丢。
    ///
    /// 2026-08-12 owner 实测:引用一条满是代码片段的卡片,`提交(18db85c)` 到对面变成
    /// `提交()` —— 句子还在、关键信息全没了,比整段丢失更难察觉。根因是 `code_span` 的文本
    /// 和 `plain_text` 一样在 `property.content`,只因 tag 不同就落进了 `_ => {}`。
    /// 真机夹具里它有 19 个,出现频次第二高。
    #[test]
    fn real_card_keeps_code_spans() {
        let content = include_str!("../../tests/fixtures/quoted_card_2_0.json");
        let got = super::quoted_plaintext_full("interactive", content, Mentions::default());
        // 夹具那张卡里的代码片段(版本号 / 提交号 / 路径),丢一个都是信息损失。
        for needle in ["1.2.3", "main = abc1234"] {
            assert!(
                got.contains(needle),
                "代码片段 {needle} 丢了,实得:{}",
                &got[..got.len().min(300)]
            );
        }
        assert!(got.contains('`'), "代码片段应带回反引号,读的人要能分辨");
    }

    /// 卡片里的 `at` 节点要渲染成可读的 `@名字`。
    ///
    /// 卡片和文本是两套机制:文本消息在正文里留 `@_user_N` 占位、名字在响应的 `mentions`
    /// 数组里;卡片的 `mentions` 实测恒为 `null`(2026-08-12 真机 mget),AST 的 `at` 只有
    /// `property.userID`(open_id)。所以卡片这条只能靠通讯录缓存回查。
    #[test]
    fn card_at_node_renders_resolved_name() {
        let content = include_str!("../../tests/fixtures/quoted_card_2_0.json");
        // 夹具那张卡里唯一的 at 指向这个 open_id。
        let resolve =
            |id: &str| (id == "ou_33333333333333333333333333333333").then(|| "星光".to_string());
        let got =
            super::quoted_plaintext_full("interactive", content, Mentions::new(&[], &resolve));
        assert!(
            got.contains("@星光"),
            "卡片里的 @ 应渲染成人名,实得:{}",
            &got[..got.len().min(300)]
        );
        assert!(
            !got.contains("ou_00000000"),
            "绝不能把 open_id 漏进正文(会被 agent 抄进回复)"
        );
    }

    /// 查不到名字时退成 `@某人`,**不能整块丢掉**:丢掉会让引用读起来像在对空气说话。
    #[test]
    fn card_at_node_falls_back_to_placeholder() {
        let content = include_str!("../../tests/fixtures/quoted_card_2_0.json");
        let resolve = |_: &str| None;
        let got =
            super::quoted_plaintext_full("interactive", content, Mentions::new(&[], &resolve));
        assert!(
            got.contains("@某人"),
            "解析不出名字也要留下「这里有个 @」,实得:{}",
            &got[..got.len().min(300)]
        );
    }

    /// 文本消息:正文里只有 `@_user_1` 占位,名字得从 `mentions` 数组查回来。
    /// 正文与 mentions 的拼法取自真机 mget 响应的形状。
    #[test]
    fn text_mention_placeholder_becomes_name() {
        let keys = [("@_user_1".to_string(), "Arcadia".to_string())];
        let resolve = |_: &str| None;
        let got = super::quoted_plaintext_full(
            "text",
            r#"{"text":"@_user_1 这两件事先做完"}"#,
            Mentions::new(&keys, &resolve),
        );
        assert_eq!(got, "@Arcadia 这两件事先做完");
    }

    /// 长 key 必须先替换:否则 `@_user_1` 会先吃掉 `@_user_10` 的前缀,
    /// 把第 10 个人的 @ 切成「@甲0」。群里 @ 满 10 个人不罕见。
    #[test]
    fn longer_mention_key_is_substituted_first() {
        let keys = [
            ("@_user_1".to_string(), "甲".to_string()),
            ("@_user_10".to_string(), "乙".to_string()),
        ];
        let resolve = |_: &str| None;
        let got = super::quoted_plaintext_full(
            "text",
            r#"{"text":"@_user_10 和 @_user_1 都看下"}"#,
            Mentions::new(&keys, &resolve),
        );
        assert_eq!(got, "@乙 和 @甲 都看下");
    }

    #[test]
    fn real_card_2_0_fixture_yields_body_text() {
        let content = include_str!("../../tests/fixtures/quoted_card_2_0.json");
        let got = super::quoted_plaintext_full("interactive", content, Mentions::default());
        assert!(
            !got.trim().is_empty(),
            "真机卡片必须提取出正文,实得空串(说明又只认了某一种 body 形状)"
        );
        // 这张卡是一条 review 请求,正文里必然有这些词。
        assert!(
            got.contains("对抗"),
            "应含正文关键词,实得:{}",
            &got[..got.len().min(200)]
        );
        assert!(
            !got.contains("json_card"),
            "不该把原始 AST 键名漏进正文:{}",
            &got[..got.len().min(200)]
        );
    }

    /// 卡片 1.0 的老形状必须继续可用 —— 兼容不是替换。
    ///
    /// (写这条时我第一版夹具又写错了:`plain_text` 的文本在 `property.content`,不在顶层。
    /// 手写夹具就是这么容易与真实形状脱节 —— 正是上面那条要用真机响应的理由。)
    #[test]
    fn card_1_0_shape_still_renders() {
        let content = serde_json::json!({
            "json_card": serde_json::json!({
                "body": {"elements": [
                    {"tag":"markdown","property":{"elements":[{"tag":"plain_text","property":{"content":"老形状正文"}}]}}
                ]}
            }).to_string()
        })
        .to_string();
        let got = super::quoted_plaintext_full("interactive", &content, Mentions::default());
        assert!(
            got.contains("老形状正文"),
            "1.0 结构不得因兼容 2.0 而失效:{got}"
        );
    }

    use super::*;

    #[test]
    fn text_message_extracts_plain_text() {
        let c = r#"{"text":"  你咋知道要 review 哪个 MR  "}"#;
        assert_eq!(
            quoted_plaintext("text", c, Mentions::default()),
            "你咋知道要 review 哪个 MR"
        );
    }

    #[test]
    fn interactive_card_harvests_markdown_content() {
        // agent 回复都是无 header 的 markdown 卡片:应抽出 markdown 元素的 content。
        let c = r#"{"config":{"wide_screen_mode":true},"elements":[{"tag":"markdown","content":"评审结论:LGTM"},{"tag":"markdown","content":"细节看 MR"}]}"#;
        assert_eq!(
            quoted_plaintext("interactive", c, Mentions::default()),
            "评审结论:LGTM\n细节看 MR"
        );
    }

    #[test]
    fn post_message_joins_inline_runs_and_breaks_lines() {
        // post 的 content 是二维数组:外层 = 行,内层 = 同一行内的 text run(按样式切分)。
        // 同一行的 run 直接拼接,只有跨行才换行。标题是用户写的正文,和正文一起进 prompt;
        // `a` 渲染成 markdown 链接,与 sidecar 那侧同一形态。
        let c = r#"{"title":"标题","content":[[{"tag":"text","text":"第一段"},{"tag":"a","text":"链接","href":"http://x"}],[{"tag":"text","text":"第二段"}]]}"#;
        assert_eq!(
            quoted_plaintext("post", c, Mentions::default()),
            "标题\n第一段[链接](http://x)\n第二段"
        );
    }

    #[test]
    fn degraded_card_html_entities_are_decoded() {
        // 不带 raw_card_content 时飞书把 `<` `>` 编码成实体,原样进 prompt 会让 agent 看到 `&#60;`。
        let c =
            r#"{"title":null,"elements":[[{"tag":"text","text":"注入 /effort &#60;档位&#62;"}]]}"#;
        assert_eq!(
            quoted_plaintext("interactive", c, Mentions::default()),
            "注入 /effort <档位>"
        );
    }

    #[test]
    fn amp_entity_decodes_last_so_escaped_entities_survive() {
        // `&amp;lt;` 应还原成字面量 `&lt;`,不能被二次解码成 `<`。
        let c = r#"{"title":null,"elements":[[{"tag":"text","text":"&amp;lt; 与 &amp;"}]]}"#;
        assert_eq!(
            quoted_plaintext("interactive", c, Mentions::default()),
            "&lt; 与 &"
        );
    }

    #[test]
    fn raw_card_content_joins_inline_runs_within_paragraph() {
        // 带 card_msg_content_type=raw_card_content 时,content 是 {json_card:"<嵌套JSON>",card_schema:N}。
        // 同段落内因加粗切分的 plain_text run 必须直接拼接,只有 br 才换行。
        let inner = r#"{"body":{"elements":[{"tag":"markdown_v1","property":{"elements":[
            {"tag":"plain_text","property":{"content":"往该会话的 tmux pane "}},
            {"tag":"plain_text","property":{"content":"注入 `/effort <档位>`","textStyle":{"attributes":["bold"]}}},
            {"tag":"plain_text","property":{"content":" slash,就地热切。"}}
        ]}}]}}"#;
        let c = serde_json::json!({"json_card": inner, "card_schema": 1}).to_string();
        assert_eq!(
            quoted_plaintext("interactive", &c, Mentions::default()),
            "往该会话的 tmux pane 注入 `/effort <档位>` slash,就地热切。"
        );
    }

    #[test]
    fn raw_card_br_becomes_newline() {
        let inner = r#"{"body":{"elements":[{"tag":"markdown_v1","property":{"elements":[
            {"tag":"plain_text","property":{"content":"第一行"}},
            {"tag":"br","property":{}},
            {"tag":"plain_text","property":{"content":"第二行"}}
        ]}}]}}"#;
        let c = serde_json::json!({"json_card": inner, "card_schema": 1}).to_string();
        assert_eq!(
            quoted_plaintext("interactive", &c, Mentions::default()),
            "第一行\n第二行"
        );
    }

    #[test]
    fn raw_card_prefers_fallback_and_does_not_duplicate() {
        // 真实卡片里 element 同时带 `fallback`(扁平 markdown 版)和 `property`(结构化版),
        // 二者是同一内容的两个表示。必须二选一,否则正文翻倍。
        let inner = r#"{"body":{"elements":[{
            "tag":"markdown_v1",
            "fallback":{"tag":"markdown","property":{"elements":[
                {"tag":"plain_text","property":{"content":"- 甲\n- 乙"}}
            ]}},
            "property":{"elements":[
                {"tag":"list","property":{"items":[
                    {"elements":[{"tag":"plain_text","property":{"content":"甲"}}],"type":"ul"},
                    {"elements":[{"tag":"plain_text","property":{"content":"乙"}}],"type":"ul"}
                ]}}
            ]}
        }]}}"#;
        let c = serde_json::json!({"json_card": inner, "card_schema": 1}).to_string();
        assert_eq!(
            quoted_plaintext("interactive", &c, Mentions::default()),
            "- 甲\n- 乙"
        );
    }

    #[test]
    fn raw_card_renders_list_when_no_fallback() {
        let inner = r#"{"body":{"elements":[{"tag":"markdown_v1","property":{"elements":[
            {"tag":"list","property":{"items":[
                {"elements":[{"tag":"plain_text","property":{"content":"甲"}}],"type":"ul"},
                {"elements":[
                    {"tag":"plain_text","property":{"content":"乙 "}},
                    {"tag":"plain_text","property":{"content":"加粗","textStyle":{"attributes":["bold"]}}}
                ],"type":"ul"}
            ]}}
        ]}}]}}"#;
        let c = serde_json::json!({"json_card": inner, "card_schema": 1}).to_string();
        assert_eq!(
            quoted_plaintext("interactive", &c, Mentions::default()),
            "- 甲\n- 乙 加粗"
        );
    }

    #[test]
    fn malformed_json_card_falls_back_to_harvest() {
        // json_card 内层不是合法 JSON 时,不能整条丢失 —— 退回递归 harvest。
        let c = r#"{"json_card":"not json","card_schema":1,"elements":[[{"tag":"text","text":"兜底文本"}]]}"#;
        assert_eq!(
            quoted_plaintext("interactive", c, Mentions::default()),
            "兜底文本"
        );
    }

    #[test]
    fn card_button_url_is_harvested_with_label() {
        // 卡片 1.0 url 按钮:被引用时按钮文案+链接都要进引用文本(owner 反馈:按钮链接感知不到)。
        let c = r#"{"elements":[{"tag":"action","actions":[{"tag":"button","text":{"tag":"plain_text","content":"打开 MR"},"url":"https://git.example.org/x/mr/1"}]}]}"#;
        assert_eq!(
            quoted_plaintext("interactive", c, Mentions::default()),
            "打开 MR (https://git.example.org/x/mr/1)"
        );
    }

    #[test]
    fn card_button_multi_url_is_harvested() {
        // multi_url 按钮:取默认 url。
        let c = r#"{"elements":[{"tag":"button","text":{"tag":"lark_md","content":"查看详情"},"multi_url":{"url":"https://x.com/d","pc_url":"","android_url":"","ios_url":""}}]}"#;
        assert_eq!(
            quoted_plaintext("interactive", c, Mentions::default()),
            "查看详情 (https://x.com/d)"
        );
    }

    #[test]
    fn card2_behaviors_open_url_is_harvested() {
        // 卡片 2.0 行为式按钮:链接在 behaviors[].default_url。
        let c = r#"{"elements":[{"tag":"button","text":{"content":"立即查看"},"behaviors":[{"type":"open_url","default_url":"https://x.com/o","pc_url":"https://x.com/pc"}]}]}"#;
        assert_eq!(
            quoted_plaintext("interactive", c, Mentions::default()),
            "立即查看 (https://x.com/o)"
        );
    }

    #[test]
    fn card_text_without_link_still_plain() {
        // 无链接的纯 markdown 卡片:行为不变,不平白多出括号。
        let c = r#"{"elements":[{"tag":"markdown","content":"评审结论:LGTM"}]}"#;
        assert_eq!(
            quoted_plaintext("interactive", c, Mentions::default()),
            "评审结论:LGTM"
        );
    }

    #[test]
    fn malformed_or_empty_returns_blank() {
        assert_eq!(
            quoted_plaintext("text", "not json", Mentions::default()),
            ""
        );
        assert_eq!(
            quoted_plaintext("text", r#"{"text":"   "}"#, Mentions::default()),
            ""
        );
        assert_eq!(
            quoted_plaintext("image", r#"{"image_key":"img_1"}"#, Mentions::default()),
            ""
        );
    }

    #[test]
    fn merge_forward_renders_every_nested_message_and_resource_placeholder() {
        let items = [
            MergeForwardItem {
                mentions: Mentions::default(),
                sender: "Alice",
                msg_type: "text",
                content: r#"{"text":"第一条"}"#,
            },
            MergeForwardItem {
                mentions: Mentions::default(),
                sender: "Bob",
                msg_type: "text",
                content: r#"{"text":"第二条"}"#,
            },
            MergeForwardItem {
                mentions: Mentions::default(),
                sender: "Alice",
                msg_type: "file",
                content: r#"{"file_key":"file_1","file_name":"方案.pdf"}"#,
            },
        ];
        let text = merge_forward_plaintext(&items);
        assert!(text.contains("转发了 3 条消息"));
        assert_eq!(text.matches("发送人: Alice").count(), 2);
        assert_eq!(text.matches("发送人: Bob").count(), 1);
        assert!(text.contains("发送人: Alice\n第一条"));
        assert!(text.contains("发送人: Bob\n第二条"));
        assert!(text.contains("发送人: Alice\n[文件: 方案.pdf]"));
    }

    #[test]
    fn long_content_is_truncated() {
        let long = "啊".repeat(MAX_QUOTED_CHARS + 50);
        let c = format!(r#"{{"text":"{long}"}}"#);
        let out = quoted_plaintext("text", &c, Mentions::default());
        assert!(out.chars().count() <= MAX_QUOTED_CHARS + 12);
        assert!(out.ends_with("已截断)"));
    }

    #[test]
    fn render_note_wraps_sender_and_body() {
        let note = render_quote_note("Ambrose", "评审结论:LGTM");
        assert!(note.contains("Ambrose"));
        assert!(note.contains("「评审结论:LGTM」"));
        assert!(note.contains("优先据此理解"));
    }

    #[test]
    fn render_note_handles_empty_body_and_sender() {
        let note = render_quote_note("", "");
        assert!(note.contains("对方"));
        assert!(note.contains("无法解析"));
    }

    #[test]
    fn full_extractor_does_not_truncate() {
        // quoted_plaintext 会截断,quoted_plaintext_full 必须留全文(spill 用)。
        let long = "啊".repeat(MAX_QUOTED_CHARS + 200);
        let c = format!(r#"{{"text":"{long}"}}"#);
        let full = quoted_plaintext_full("text", &c, Mentions::default());
        assert_eq!(full.chars().count(), MAX_QUOTED_CHARS + 200);
        assert!(!full.contains("已截断"));
    }

    #[test]
    fn spill_decision_by_length() {
        assert!(!should_spill_quote(&"短".repeat(QUOTE_INLINE_MAX_CHARS)));
        assert!(should_spill_quote(&"长".repeat(QUOTE_INLINE_MAX_CHARS + 1)));
    }

    #[test]
    fn preview_caps_without_ellipsis() {
        let full = "字".repeat(QUOTE_PREVIEW_CHARS + 100);
        let p = quote_preview(&full);
        assert_eq!(p.chars().count(), QUOTE_PREVIEW_CHARS);
        assert!(!p.contains('…'), "摘要本身不带省略号(由 render 拼)");
    }

    #[test]
    fn spilled_note_has_preview_and_path() {
        let note = render_quote_note_spilled("Ambrose", "前几句摘要", "/x/inbound/quotes/om_1.md");
        assert!(note.contains("Ambrose"));
        assert!(note.contains("前几句摘要…"));
        assert!(note.contains("/x/inbound/quotes/om_1.md"));
        assert!(note.contains("Read"));
    }

    #[test]
    fn quote_file_md_has_metadata_and_full_body() {
        let md = quote_file_markdown("Ambrose", "om_abc", "完整正文内容");
        assert!(md.contains("发送人: Ambrose"));
        assert!(md.contains("原消息 id: om_abc"));
        assert!(md.contains("完整正文内容"));
        // 空发送人回退「对方」。
        assert!(quote_file_markdown("", "om_x", "body").contains("发送人: 对方"));
    }

    #[test]
    fn extract_file_resource() {
        let r = extract_quoted_resource(
            "file",
            r#"{"file_key":"file_v3_abc","file_name":"报告.pdf"}"#,
        )
        .expect("file 应能提取");
        assert_eq!(r.file_key, "file_v3_abc");
        assert_eq!(r.kind, "file");
        assert_eq!(r.name, "报告.pdf");
    }

    #[test]
    fn extract_image_resource() {
        let r = extract_quoted_resource("image", r#"{"image_key":"img_v3_xyz"}"#)
            .expect("image 应能提取");
        assert_eq!(r.file_key, "img_v3_xyz");
        assert_eq!(r.kind, "image");
        assert!(r.name.is_empty());
    }

    #[test]
    fn extract_audio_resource() {
        let r = extract_quoted_resource("audio", r#"{"file_key":"file_v3_aud","duration":10}"#)
            .expect("audio 应能提取");
        assert_eq!(r.file_key, "file_v3_aud");
        assert_eq!(r.kind, "audio");
    }

    #[test]
    fn extract_media_resource_with_name() {
        let r = extract_quoted_resource(
            "media",
            r#"{"file_key":"file_v3_vid","file_name":"demo.mp4","duration":30}"#,
        )
        .expect("media 应能提取");
        assert_eq!(r.file_key, "file_v3_vid");
        assert_eq!(r.name, "demo.mp4");
    }

    #[test]
    fn extract_resource_ignores_text_type() {
        assert!(extract_quoted_resource("text", r#"{"text":"hello"}"#).is_none());
    }

    #[test]
    fn extract_resource_ignores_bad_json() {
        assert!(extract_quoted_resource("file", "not json").is_none());
    }

    #[test]
    fn extract_resource_ignores_missing_key() {
        assert!(extract_quoted_resource("file", r#"{"file_name":"x.pdf"}"#).is_none());
    }

    #[test]
    fn render_note_with_files() {
        let saved = vec![
            ("file".to_string(), "/a/b/报告.pdf".to_string()),
            ("image".to_string(), "/a/b/1-image.png".to_string()),
        ];
        let note = render_quote_note_with_files("Ambrose", &saved);
        assert!(note.contains("Ambrose"));
        assert!(note.contains("含附件"));
        assert!(note.contains("报告.pdf"));
        assert!(note.contains("优先据此理解"));
    }

    #[test]
    fn render_note_with_files_empty_sender() {
        let saved = vec![("file".to_string(), "/x.pdf".to_string())];
        let note = render_quote_note_with_files("", &saved);
        assert!(note.contains("对方"));
    }
}

/// 飞书对「读回一张卡片」给出的降级占位特征词。
///
/// 实测(2026-08-10):`GET im/v1/messages/:id` 对 interactive 消息返回的是
/// `{"elements":[[{"tag":"text","text":"请升级至最新版本客户端，以查看内容"}]]}`,
/// **即便带上 `card_msg_content_type=raw_card_content` 也一样**。于是引用一张 bot 发的卡片时,
/// agent 读到的"被引用内容"就是这句废话(owner 实测:「我引用的消息 flare 读不到」)。
const CARD_PLACEHOLDER_MARKS: &[&str] = &[
    summon_i18n::source!("r1d4f52c95f"),
    summon_i18n::source!("r154b89be45"),
];

/// 这段引用正文是不是飞书的卡片降级占位(而非真内容)。
///
/// **判据取词根而非整句**:飞书随时可能微调文案,但"升级客户端/查看内容"这组词根是这类占位的
/// 稳定特征。**且要求长度足够短** —— 真内容里出现这句话是可能的(比如有人在讨论这个提示),
/// 但那种消息通常还有别的内容;占位则短得只剩这一句。两个条件一起用,把误判压到最低。
pub fn looks_like_card_placeholder(text: &str) -> bool {
    let t = text.trim();
    if t.chars().count() > 40 {
        return false;
    }
    CARD_PLACEHOLDER_MARKS.iter().any(|m| t.contains(m))
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
/// 从一行 `outbound.ndjson` 里取出 (message_id, 正文)。不是出站卡片记录 → `None`。
///
/// 归档的 payload 有两种形态:`{"message_id":..,"markdown":".."}`(markdown 卡)与
/// `{"message_id":..,"card":{..}}`(结构化卡片)。前者直接就是正文;后者取卡片里的 markdown
/// 文本拼起来 —— 引用只需要人读得懂的内容,不需要还原卡片结构。
pub(crate) fn outbound_record_body(line: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = v.get("payload")?;
    let id = payload.get("message_id")?.as_str()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    if let Some(md) = payload.get("markdown").and_then(serde_json::Value::as_str) {
        return Some((id, md.to_string()));
    }
    let card = payload.get("card")?;
    let text = harvest_text(card);
    (!text.trim().is_empty()).then_some((id, text))
}

/// 被引用消息与当前 agent 的关系,决定引用要以什么形态进 prompt。
///
/// **why 分流**(owner 2026-08-10):把被引用卡片的全文无脑塞进 prompt 是双重浪费 ——
/// 引用自己刚说过的话时,那段内容**已经在 agent 的上下文里**,再塞一遍纯属重复计费;
/// 而引用别人的卡片时,多数情况下 agent 未必真需要全文,给它一条"要看自己去取"的路
/// 比预先灌进来更省。
pub enum QuoteKind<'a> {
    /// 引用的是本 agent 自己发的消息 —— 只给指代,不给全文。
    SelfMessage {
        message_id: &'a str,
        digest: &'a str,
    },
    /// 引用的是别人发的、**正文取不回来**的消息 —— 给取用方式,不给全文。
    ///
    /// 不止卡片:合并转发、富文本等类型同样可能提取不出正文(owner 2026-08-13 实测,被引用的是
    /// 一条合并转发,文案却硬说成「卡片消息」)。命名与文案都按「读不到」而不是按某个具体类型说。
    ForeignUnreadable { message_id: &'a str },
    /// 引用的是 summon 私聊发出去的终端卡 —— 只说是什么,不给 message_id 与取法:卡上是只给
    /// 收件人本人的一次性链接,bot 身份按 id 能把它取回来。
    TerminalCard,
    /// 引用的是 summon 私聊发出去的控制台卡 —— 理由同终端卡:卡上的链接只给收件人本人。
    DashboardCard,
}

/// 引用摘要的截取长度。只用来让人/agent **认出是哪一条**,不承担传达内容的职责。
const QUOTE_DIGEST_CHARS: usize = 40;

/// 取一段用于指代的摘要(压成单行、截断)。
pub fn quote_digest(full: &str) -> String {
    let flat: String = full
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut out: String = flat.chars().take(QUOTE_DIGEST_CHARS).collect();
    if flat.chars().count() > QUOTE_DIGEST_CHARS {
        out.push('…');
    }
    out
}

/// 渲染成进 prompt 的一段话。**两种形态都刻意保持极短**。
pub fn render_quote_hint(kind: &QuoteKind<'_>) -> String {
    match kind {
        QuoteKind::SelfMessage { message_id, digest } => {
            let digest = serde_json::to_string(digest).expect(summon_i18n::text!("r77925f71d7"));
            summon_i18n::format!("r580929b14c")
        }
        QuoteKind::ForeignUnreadable { message_id } => {
            summon_i18n::format!("r0c0a074ab9")
        }
        QuoteKind::TerminalCard => summon_i18n::text!("r2eac32d1af").to_string(),
        QuoteKind::DashboardCard => summon_i18n::text!("r01a1c63d64").to_string(),
    }
}
