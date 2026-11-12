//! P2 跨会话记忆分层 —— **纯件**(无 IO):多层索引合成(读路径)+ 事实文件/索引行渲染(写路径)。
//! 层目录读写、注入 prompt、`summon remember` 写回的 IO 在 summon 层。设计权威:
//! `docs/superpowers/specs/2026-06-14-p2-memory-layering-design.md`。
//!
//! 本切片**与 spec §7 的决策点无关**(那些是注入默认/写回鉴权,属 IO/接线层),故先落地:
//! 召回靠 description 索引(注入各层 `MEMORY.md` 的描述行 + 根路径,agent 自己 Read 取全文),
//! 不引向量库;事实文件 frontmatter 与 owner 现用记忆格式同构。

use crate::event_log::EventStamp;

/// 一个已解析的记忆层(注入 prompt 用)。`index` 为该层 `MEMORY.md` 的描述行正文(空=该层无记忆)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLayer {
    /// 段标题(prompt 小标题,如「全局」「本 agent」)。
    pub title: String,
    /// 该层根目录的人读路径(告诉 agent 全文文件在哪,用 Read 取)。
    pub root_display: String,
    /// 该层 `MEMORY.md` 索引正文(描述行;空=跳过本层)。
    pub index: String,
}

/// 把多层记忆索引合成进一段注入文本:非空层各起一小节(标题 + 根路径 + 索引行)。
/// 全空 → 返回空串(调用方据此不注入记忆段)。只拼**索引**不拼全文(省 token,agent 按需 Read)。
pub fn compose_memory_index(layers: &[MemoryLayer]) -> String {
    let mut out = String::new();
    for l in layers {
        if l.index.trim().is_empty() {
            continue;
        }
        if out.is_empty() {
            out.push_str(summon_i18n::text!("r8337f96748"));
        }
        out.push_str(&summon_i18n::format!(
            "r6be670dfc5",
            l.title,
            l.root_display,
            l.index.trim()
        ));
    }
    out
}

/// 由标题生成文件名 slug(写回 `<slug>.md` 用):小写,保留 ASCII 字母数字与 CJK,其余成 `-`,
/// 折叠连续 `-`、去首尾 `-`,截到 48 字符。全空(纯符号)→ 回退 `mem`。
pub fn slugify(title: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for ch in title.trim().chars() {
        let keep = ch.is_ascii_alphanumeric() || is_cjk(ch);
        if keep {
            for lc in ch.to_lowercase() {
                s.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash && !s.is_empty() {
            s.push('-');
            prev_dash = true;
        }
    }
    let s: String = s.trim_matches('-').chars().take(48).collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "mem".to_string()
    } else {
        s
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF)
}

/// 渲染一条事实的 markdown(frontmatter + 正文),与 owner 现用记忆格式同构(`name`/`description`/
/// `metadata.type`)。`fact_type` ∈ user|feedback|project|reference(不在此校验,写回层兜)。
///
/// `provenance` 是写下这条事实的那一轮(agent/模型/turn,与 events.jsonl 同一份出处),有值的项
/// 写在 `metadata` 下、与 `type` 并列;记忆文件离开 agent 目录后仍能追到是谁在哪一轮写的。
pub fn render_fact_md(
    name: &str,
    description: &str,
    fact_type: &str,
    body: &str,
    provenance: &EventStamp,
) -> String {
    let mut metadata = format!("  type: {fact_type}\n");
    for (key, value) in [
        ("agent", &provenance.agent),
        ("model", &provenance.model),
        ("turn", &provenance.turn),
    ] {
        if let Some(value) = value {
            metadata.push_str(&format!("  {key}: {value}\n"));
        }
    }
    format!(
        "---\nname: {name}\ndescription: {description}\nmetadata:\n{metadata}---\n\n{}\n",
        body.trim()
    )
}

/// 一条 `MEMORY.md` 索引行:`- [Title](slug.md) — hook`(hook 空则省破折号)。
pub fn index_line(title: &str, slug: &str, hook: &str) -> String {
    if hook.trim().is_empty() {
        format!("- [{title}]({slug}.md)")
    } else {
        format!("- [{title}]({slug}.md) — {}", hook.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(title: &str, root: &str, index: &str) -> MemoryLayer {
        MemoryLayer {
            title: title.into(),
            root_display: root.into(),
            index: index.into(),
        }
    }

    #[test]
    fn compose_skips_empty_layers_and_joins_nonempty() {
        let layers = vec![
            layer(
                "全局",
                "~/.summon/memory/global/",
                "- [偏好](p.md) — 中文回复",
            ),
            layer("空层", "~/x/", "   \n  "), // 全空白 → 跳过
            layer(
                "本 agent",
                "~/.summon/agents/sk/memory/",
                "- [项目](proj.md) — summon",
            ),
        ];
        let out = compose_memory_index(&layers);
        assert!(out.starts_with("## 记忆"));
        assert!(out.contains("### 全局(根:~/.summon/memory/global/)"));
        assert!(out.contains("- [偏好](p.md) — 中文回复"));
        assert!(out.contains("### 本 agent(根:~/.summon/agents/sk/memory/)"));
        assert!(!out.contains("空层"), "全空白层应被跳过");
    }

    #[test]
    fn compose_all_empty_returns_empty() {
        let layers = vec![layer("a", "~/a", ""), layer("b", "~/b", "  ")];
        assert_eq!(compose_memory_index(&layers), "");
        assert_eq!(compose_memory_index(&[]), "");
    }

    #[test]
    fn slugify_kebabs_ascii_keeps_cjk_falls_back() {
        assert_eq!(slugify("Owner Prefs!! "), "owner-prefs");
        assert_eq!(slugify("  Foo__Bar  "), "foo-bar");
        // CJK 保留(macOS 文件名安全),不折成空。
        assert_eq!(slugify("飞书 取消"), "飞书-取消");
        // 纯符号 → 回退。
        assert_eq!(slugify("!!! @@@"), "mem");
        // 截断 + 去首尾 dash。
        assert!(slugify(&"a ".repeat(60)).len() <= 48);
    }

    #[test]
    fn render_fact_has_frontmatter_and_trimmed_body() {
        let md = render_fact_md(
            "owner-prefs",
            "中文回复",
            "feedback",
            "\n  正文。\n",
            &EventStamp::default(),
        );
        assert!(md.starts_with("---\nname: owner-prefs\n"));
        assert!(md.contains("description: 中文回复"));
        assert!(md.contains("metadata:\n  type: feedback\n---"));
        assert!(md.contains("---\n\n正文。\n"));
        for key in ["agent:", "model:", "turn:"] {
            assert!(!md.contains(key), "没有出处就不写 {key}:{md}");
        }
    }

    #[test]
    fn render_fact_puts_provenance_under_metadata() {
        let provenance = EventStamp {
            agent: Some("arcadia".into()),
            model: Some("ark/seed-evolving".into()),
            turn: Some("turn-1".into()),
        };
        let md = render_fact_md("n", "d", "project", "正文", &provenance);
        assert!(
            md.contains(
                "metadata:\n  type: project\n  agent: arcadia\n  model: ark/seed-evolving\n  turn: turn-1\n---"
            ),
            "{md}"
        );
    }

    #[test]
    fn index_line_with_and_without_hook() {
        assert_eq!(
            index_line("偏好", "owner-prefs", "中文回复"),
            "- [偏好](owner-prefs.md) — 中文回复"
        );
        assert_eq!(index_line("空 hook", "x", "  "), "- [空 hook](x.md)");
    }
}
