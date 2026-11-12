//! 跨会话索引纯件:把「本 agent 在其它飞书会话的活动」渲染成一段可注入 PRIMER 的索引文本。
//!
//! 不变量(设计的全部重点,改之前先读):
//! - **只有索引,没有正文**。这里渲染出来的每一行都只含元数据(会话名 / 最后活动时刻 / 最后
//!   发言人),绝不含任何一句对话内容。正文必须由 agent 显式跑 `summon chats read` 去读 ——
//!   把跨会话正文默认灌进上下文,等于让 A 群的内容随时可能被带进 B 群。
//! - **截断必须说出来**。列表有上限,被砍掉的条数要渲染进文本(见 `docs` 里 fail-closed 那条:
//!   静默截断会让 agent 以为「我看到的就是全部」)。
//! - 纯件:不碰文件系统。扫盘在 `summon::session::cross_chat`。

/// 一个会话在索引里的元数据。**不含任何对话正文**——这是本模块的核心不变量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDigest {
    /// `chat_tag()` 产出的稳定标识(`g…`/`p…`)。
    pub tag: String,
    /// 群名或私聊对象名;解析不到时由扫盘侧填「群聊」/「私聊」。
    pub name: String,
    pub is_group: bool,
    /// 最后活动本地时刻,`%Y-%m-%d %H:%M:%S`。缺失(旧 `.meta`)→ None,排到最后。
    pub last_active: Option<String>,
    /// 最后一个说话的人(`.meta` 的 `last_sender`)。
    pub last_sender: Option<String>,
    /// 该会话是否有可读正文(`threads/<tag>/transcript.md` 非空)。无正文的不进索引 ——
    /// 列出来 agent 也 recall 不到东西,只是噪音。
    pub has_body: bool,
    /// 该会话的 chat id(`.meta` 的 `chat_id`)。旧 `.meta` 可能没有 → None。
    ///
    /// **不是给索引渲染用的** —— 渲染只用 `name`。它是给「要对这个会话做点什么」的路径用的
    /// (跨群 fork 要据它算出目标的会话名),而那些路径**不能从 tag 反推 chat_id**:tag 是
    /// `fnv1a_hex(chat_id)`,单向。
    pub chat_id: Option<String>,
}

impl ChatDigest {
    /// `is_group` 还原成 `.meta` 里的 chat_type 字面值。
    ///
    /// digest 只留了一个 bool(索引渲染只需要区分「群 / 私聊」),但要算 tmux 会话名就得要字面值。
    /// `topic_group` 与 `group` 在这里合并成 `group` 是安全的:两者 `chat_tag` 同前缀 `g`,
    /// 而会话名只由 tag 决定。
    pub fn chat_type_str(&self) -> &'static str {
        if self.is_group {
            "group"
        } else {
            "p2p"
        }
    }
}

/// 索引最多列几行。超出的按「更早」折叠成一行提示(不静默丢)。
pub const INDEX_MAX_ROWS: usize = 12;

/// 挑出该进索引的会话并排序:剔除当前会话与无正文的,按最后活动倒序(无时间的垫底、名字兜底
/// 保证顺序稳定)。返回 `(选中, 被截断的条数)`。
///
/// 排序稳定性是硬要求:PRIMER 里的序号和 `summon chats` 列表必须指向同一行,否则 agent 按
/// 序号 read 会读错会话。
pub fn select_for_index(
    digests: &[ChatDigest],
    current_tag: &str,
    max_rows: usize,
) -> (Vec<ChatDigest>, usize) {
    let mut rows: Vec<ChatDigest> = digests
        .iter()
        .filter(|d| d.tag != current_tag && d.has_body)
        .cloned()
        .collect();
    // 倒序:时间字符串是定宽 `%Y-%m-%d %H:%M:%S`,字典序即时序。None 垫底。
    rows.sort_by(|a, b| {
        b.last_active
            .cmp(&a.last_active)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.tag.cmp(&b.tag))
    });
    let dropped = rows.len().saturating_sub(max_rows);
    rows.truncate(max_rows);
    (rows, dropped)
}

/// 一行索引:`- [3] sample开发群(群) · 08-07 14:53 · 最后发言 张三`。
/// 日期省掉年份(索引只用于判断新鲜度,年份占字数不提供信息);缺时间则标「时间未知」。
///
/// PRIMER 段与 `summon chats` 列表共用它:两处渲染必须逐字一致,否则 agent 按 PRIMER 里
/// 看到的名字去 `chats read` 会对不上。
pub fn render_row(idx: usize, d: &ChatDigest) -> String {
    let kind = if d.is_group {
        summon_i18n::text!("r0df737cb62")
    } else {
        summon_i18n::text!("r6add2714c4")
    };
    let when = d
        .last_active
        .as_deref()
        .map(short_stamp)
        .unwrap_or_else(|| summon_i18n::text!("r664939a1fa").to_string());
    let mut line = format!("- [{idx}] {}({kind}) · {when}", d.name);
    if let Some(s) = d.last_sender.as_deref().filter(|s| !s.is_empty()) {
        line.push_str(&summon_i18n::format!("r38ff4222a6"));
    }
    line
}

/// `2026-08-07 14:53:21` → `08-07 14:53`。格式不符就原样返回(宁可长一点也别猜错)。
fn short_stamp(stamp: &str) -> String {
    let bytes = stamp.as_bytes();
    if bytes.len() >= 16 && bytes[4] == b'-' && bytes[7] == b'-' && bytes[10] == b' ' {
        return stamp[5..16].to_string();
    }
    stamp.to_string()
}

/// 渲染注入 PRIMER 的整段索引。没有可列的会话 → 返回空串(调用方据此整段跳过)。
///
/// 文本里那段「什么时候该 / 不该 recall」是 owner 明确要求的约束(2026-08-08):索引默认开、
/// 正文按需读,且**只在必要相关时读**。它是软闸(靠模型遵守),硬闸只有一条 ——
/// 跨 agent 不可读(见 `summon chats` 的身份校验)。
pub fn compose_cross_chat_index(
    digests: &[ChatDigest],
    current_tag: &str,
    max_rows: usize,
) -> String {
    let (rows, dropped) = select_for_index(digests, current_tag, max_rows);
    if rows.is_empty() {
        return String::new();
    }
    let mut s = String::from(summon_i18n::text!("rb7d2b02ecf"));
    for (i, d) in rows.iter().enumerate() {
        s.push_str(&render_row(i + 1, d));
        s.push('\n');
    }
    if dropped > 0 {
        s.push_str(&summon_i18n::format!("rf45a6e7d48"));
    }
    s.push_str(summon_i18n::text!("r2bcc281736"));
    s
}

// ─── transcript.md 的解析(纯串处理,与磁盘无关) ────────────────────────────────
//
// 格式由 `agent_env::append_transcript` 写入:每轮一块,`### <本地时刻> <角色>` 起头,
// 其后到下一个 `### ` 之前都是正文。解析必须容忍**任何**畸形内容(转录是 append-only 的
// 运行时产物,正文里完全可能出现以 `### ` 开头的 markdown 行),所以只做尽力切分:
// 切错最坏是多切/少切一块,绝不 panic、绝不吞掉尾部。

/// 转录里的一轮。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// `### ` 之后的整个头行(时刻 + 角色),不再二次拆分——角色名可能含空格。
    pub header: String,
    pub body: String,
}

/// 按 `### ` 头行切分转录。首个头行之前的内容(理论上没有)被丢弃。
pub fn split_turns(transcript: &str) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    for line in transcript.lines() {
        if let Some(h) = line.strip_prefix("### ") {
            out.push(Turn {
                header: h.trim().to_string(),
                body: String::new(),
            });
        } else if let Some(cur) = out.last_mut() {
            cur.body.push_str(line);
            cur.body.push('\n');
        }
    }
    for t in out.iter_mut() {
        t.body = t.body.trim_end().to_string();
    }
    out
}

/// 取最后 `n` 轮,渲染回可读文本。`n=0` 或转录为空 → 空串。
/// 返回值第二项是「前面还有多少轮没显示」,调用方必须把它说出来(不静默截断)。
pub fn tail_turns(transcript: &str, n: usize) -> (String, usize) {
    let turns = split_turns(transcript);
    if turns.is_empty() || n == 0 {
        return (String::new(), turns.len());
    }
    let start = turns.len().saturating_sub(n);
    let mut s = String::new();
    for t in &turns[start..] {
        s.push_str(&format!("### {}\n{}\n\n", t.header, t.body));
    }
    (s.trim_end().to_string(), start)
}

/// 一条 grep 命中:所属轮的头行 + 命中的那一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepHit {
    pub header: String,
    pub line: String,
}

/// 在转录里搜关键词(大小写不敏感的子串匹配)。**只回命中行,不回整轮** ——
/// 回整轮等于绕过「正文按需读」这条约束,一次 grep 就把别的会话整段拖进上下文。
/// 返回 `(命中, 因上限被丢弃的条数)`。
pub fn grep_transcript(transcript: &str, needle: &str, max: usize) -> (Vec<GrepHit>, usize) {
    if needle.is_empty() {
        return (Vec::new(), 0);
    }
    let needle_lc = needle.to_lowercase();
    let mut hits = Vec::new();
    let mut total = 0usize;
    for t in split_turns(transcript) {
        for line in t.body.lines() {
            if !line.to_lowercase().contains(&needle_lc) {
                continue;
            }
            total += 1;
            if hits.len() < max {
                hits.push(GrepHit {
                    header: t.header.clone(),
                    line: line.trim().to_string(),
                });
            }
        }
    }
    (hits, total.saturating_sub(max.min(total)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(tag: &str, name: &str, at: Option<&str>) -> ChatDigest {
        ChatDigest {
            tag: tag.to_string(),
            name: name.to_string(),
            is_group: true,
            last_active: at.map(|s| s.to_string()),
            last_sender: Some("张三".into()),
            has_body: true,
            chat_id: Some(format!("oc_{tag}")),
        }
    }

    #[test]
    fn select_excludes_current_chat_and_bodyless() {
        let mut empty = d("g3", "空会话", Some("2026-08-08 10:00:00"));
        empty.has_body = false;
        let all = vec![
            d("g1", "甲群", Some("2026-08-01 10:00:00")),
            d("g2", "乙群", Some("2026-08-05 10:00:00")),
            empty,
        ];
        let (rows, dropped) = select_for_index(&all, "g1", 10);
        assert_eq!(dropped, 0);
        // 当前会话 g1 与无正文的 g3 都不进索引。
        assert_eq!(
            rows.iter().map(|r| r.tag.as_str()).collect::<Vec<_>>(),
            ["g2"]
        );
    }

    #[test]
    fn select_sorts_by_last_active_desc_with_none_last() {
        let all = vec![
            d("g1", "旧", Some("2026-06-01 09:00:00")),
            d("g2", "新", Some("2026-08-07 09:00:00")),
            d("g3", "无时间", None),
            d("g4", "中", Some("2026-07-15 09:00:00")),
        ];
        let (rows, _) = select_for_index(&all, "", 10);
        assert_eq!(
            rows.iter().map(|r| r.tag.as_str()).collect::<Vec<_>>(),
            ["g2", "g4", "g1", "g3"],
            "按最后活动倒序,无时间的垫底"
        );
    }

    #[test]
    fn truncation_is_reported_never_silent() {
        // 静默截断会让 agent 以为「我看到的就是全部」——被砍掉的数量必须出现在文本里。
        let all: Vec<ChatDigest> = (0..5)
            .map(|i| {
                d(
                    &format!("g{i}"),
                    &format!("群{i}"),
                    Some("2026-08-01 09:00:00"),
                )
            })
            .collect();
        let (rows, dropped) = select_for_index(&all, "", 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(dropped, 3);
        let text = compose_cross_chat_index(&all, "", 2);
        assert!(
            text.contains("另有 3 个更早的会话未列出"),
            "截断须说明:{text}"
        );
    }

    #[test]
    fn index_carries_no_conversation_body() {
        // 本模块的核心不变量:渲染产物只含元数据。ChatDigest 里根本没有正文字段,
        // 这条测试锁住「将来别人往 digest 里塞正文再渲染出来」。
        let all = vec![d("g1", "甲群", Some("2026-08-07 14:53:21"))];
        let text = compose_cross_chat_index(&all, "", 10);
        assert!(text.contains("甲群(群) · 08-07 14:53 · 最后发言 张三"));
        assert!(text.contains("没有任何一句对话内容"));
        assert!(text.contains("只在必要相关时才去读正文"));
    }

    #[test]
    fn the_index_never_renders_the_chat_id() {
        // `chat_id` 只服务「要对这个会话做点什么」的路径(跨群 fork 据它算目标会话名)。
        // 渲染出来就是把内部标识摊进 PRIMER 与飞书消息 —— 索引该给人看的是群名,不是 id。
        let all = vec![d("g1", "甲群", Some("2026-08-07 14:53:21"))];
        let id = all[0].chat_id.clone().expect("夹具该带 chat_id");
        assert!(!compose_cross_chat_index(&all, "", 10).contains(&id));
        assert!(!render_row(1, &all[0]).contains(&id));
    }

    #[test]
    fn empty_when_nothing_to_list() {
        assert!(compose_cross_chat_index(&[], "", 10).is_empty());
        // 唯一的会话就是当前会话 → 整段跳过(不该渲染一个空标题进 PRIMER)。
        let all = vec![d("g1", "甲群", Some("2026-08-07 14:53:21"))];
        assert!(compose_cross_chat_index(&all, "g1", 10).is_empty());
    }

    #[test]
    fn short_stamp_falls_back_on_unexpected_format() {
        assert_eq!(short_stamp("2026-08-07 14:53:21"), "08-07 14:53");
        assert_eq!(short_stamp("不是时间"), "不是时间");
        assert_eq!(short_stamp(""), "");
    }

    const SAMPLE: &str = "\
### 2026-07-09 11:31:18 用户 用户
第一问

### 2026-07-09 11:32:28 arcadia
第一答
带 worktree 的一行

### 2026-08-01 09:00:00 用户 用户
第二问
";

    #[test]
    fn split_turns_parses_header_and_body() {
        let ts = split_turns(SAMPLE);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].header, "2026-07-09 11:31:18 用户 用户");
        assert_eq!(ts[1].body, "第一答\n带 worktree 的一行");
        assert_eq!(ts[2].header, "2026-08-01 09:00:00 用户 用户");
    }

    #[test]
    fn split_turns_tolerates_garbage_without_panicking() {
        assert!(split_turns("").is_empty());
        // 没有任何头行 → 全部丢弃(而不是当成一轮无头正文)。
        assert!(split_turns("裸正文\n第二行").is_empty());
        // 只有头行、没正文 → 一轮空正文。
        let ts = split_turns("### 只有头\n");
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].body, "");
    }

    #[test]
    fn tail_turns_reports_how_many_were_skipped() {
        let (text, skipped) = tail_turns(SAMPLE, 1);
        assert_eq!(skipped, 2, "前面跳过的轮数必须回给调用方");
        assert!(text.contains("第二问") && !text.contains("第一问"));
        // n 大于总轮数 → 全给,skipped=0。
        let (all, skipped) = tail_turns(SAMPLE, 99);
        assert_eq!(skipped, 0);
        assert!(all.contains("第一问") && all.contains("第二问"));
        // n=0 → 空串,但仍如实报告总轮数。
        assert_eq!(tail_turns(SAMPLE, 0), (String::new(), 3));
    }

    #[test]
    fn grep_returns_only_matching_lines_not_whole_turns() {
        let (hits, dropped) = grep_transcript(SAMPLE, "WORKTREE", 10);
        assert_eq!(dropped, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, "带 worktree 的一行");
        assert_eq!(hits[0].header, "2026-07-09 11:32:28 arcadia");
        // 命中行的同轮邻居不能被顺带带出来——那等于绕过「正文按需读」。
        assert!(!hits[0].line.contains("第一答"));
    }

    #[test]
    fn grep_caps_results_and_reports_overflow() {
        let big = "### h\n命中\n命中\n命中\n";
        let (hits, dropped) = grep_transcript(big, "命中", 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(dropped, 1);
        // 空关键词不匹配任何东西(否则会把整份转录倒出来)。
        assert_eq!(grep_transcript(big, "", 10), (Vec::new(), 0));
    }

    #[test]
    fn private_chat_rendered_as_private() {
        let mut p = d("p1", "张三", Some("2026-08-05 15:24:00"));
        p.is_group = false;
        p.last_sender = None;
        let text = compose_cross_chat_index(&[p], "", 10);
        assert!(text.contains("张三(私聊) · 08-05 15:24\n"), "{text}");
    }
}
