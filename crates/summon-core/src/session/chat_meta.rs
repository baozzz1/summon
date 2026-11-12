//! `chats/<session>.meta`(k=v 行格式)的 schema 纯件(A4)。
//!
//! 它是会话身份的持久层:此前 1 个写端 + 3 个解析端各自手写 `split_once('=')` 循环、路径拼装
//! 还有 2 份拷贝——schema 演进(如后加 creator)要记得同步 4 处,漏一处就是静默读不到(群散
//! 通知找不到群主)。解析/渲染/路径收口在此,IO(读写文件)留在 summon。

/// `.meta` 的全部字段。任何字段缺行/空值 → None(向后兼容旧 `.meta`)。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ChatMeta {
    pub chat_id: Option<String>,
    pub chat_type: Option<String>,
    pub chat_name: Option<String>,
    pub tag: Option<String>,
    pub last_sender: Option<String>,
    /// 写 .meta 的本地时刻(`%Y-%m-%d %H:%M:%S`)。last-active 的回退源(主源是 events.jsonl mtime)。
    pub updated_at: Option<String>,
    /// 群主近似(首个 inbound 的 sender open_id);群没了通知谁用。
    pub creator: Option<String>,
}

/// `.meta` 相对 agent 目录的路径(`runtime/chats/<session>.meta`)。两端拼装统一走它。
pub fn meta_rel_path(session: &str) -> String {
    format!("runtime/chats/{session}.meta")
}

/// 值消毒:k=v 行格式不允许换行(换行会被解析端当新行截断)。
pub fn sanitize_value(value: &str) -> String {
    value.replace(['\n', '\r'], " ").trim().to_string()
}

impl ChatMeta {
    /// 解析 `.meta` 文本。未知键忽略(向前兼容),空值当缺失。
    pub fn parse(content: &str) -> ChatMeta {
        let mut m = ChatMeta::default();
        for line in content.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().to_string();
            if v.is_empty() {
                continue;
            }
            match k.trim() {
                "chat_id" => m.chat_id = Some(v),
                "chat_type" => m.chat_type = Some(v),
                "chat_name" => m.chat_name = Some(v),
                "tag" => m.tag = Some(v),
                "last_sender" => m.last_sender = Some(v),
                "updated_at" => m.updated_at = Some(v),
                "creator" => m.creator = Some(v),
                _ => {}
            }
        }
        m
    }

    /// 渲染成 k=v 文档。与历史写端逐字节同格式:前六行无条件写(缺失渲染成空值行),
    /// `creator` 只在非空时落行(首写记一次、续写沿用,避免被后说话的人覆盖)。
    pub fn render(&self) -> String {
        let g = |v: &Option<String>| sanitize_value(v.as_deref().unwrap_or(""));
        let mut doc = format!(
            "chat_id={}\nchat_type={}\nchat_name={}\ntag={}\nlast_sender={}\nupdated_at={}\n",
            g(&self.chat_id),
            g(&self.chat_type),
            g(&self.chat_name),
            g(&self.tag),
            g(&self.last_sender),
            g(&self.updated_at),
        );
        if let Some(c) = self
            .creator
            .as_deref()
            .map(sanitize_value)
            .filter(|c| !c.is_empty())
        {
            doc.push_str(&format!("creator={c}\n"));
        }
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_parse_round_trips_and_skips_empty() {
        let m = ChatMeta {
            chat_id: Some("oc_x".into()),
            chat_type: Some("group".into()),
            chat_name: None, // 渲染成空值行,解析回 None
            tag: Some("g-12ab34cd".into()),
            last_sender: Some("老板".into()),
            updated_at: Some("2026-06-12 10:00:00".into()),
            creator: Some("ou_boss".into()),
        };
        let doc = m.render();
        assert!(doc.contains("chat_name=\n"), "缺失字段仍占一行(历史格式)");
        assert_eq!(ChatMeta::parse(&doc), m);
    }

    #[test]
    fn parse_tolerates_legacy_and_unknown_lines() {
        // 旧 .meta:无 creator/updated_at 行;夹杂未知键与坏行,都不 panic。
        let doc = "chat_id=oc_y\nchat_type=p2p\nfuture_key=v\n不是kv行\n";
        let m = ChatMeta::parse(doc);
        assert_eq!(m.chat_id.as_deref(), Some("oc_y"));
        assert_eq!(m.creator, None);
        assert_eq!(m.updated_at, None);
    }

    #[test]
    fn sanitize_strips_newlines_and_render_omits_empty_creator() {
        // 换行/回车 → 空格(\r\n 是两个字符 → 两个空格),首尾再 trim。
        assert_eq!(sanitize_value("a\nb\r\nc"), "a b  c");
        assert_eq!(sanitize_value("  x\n"), "x");
        let m = ChatMeta {
            creator: Some("   ".into()),
            ..Default::default()
        };
        assert!(!m.render().contains("creator="), "空 creator 不落行");
        assert_eq!(meta_rel_path("soul-x-g1"), "runtime/chats/soul-x-g1.meta");
    }
}
