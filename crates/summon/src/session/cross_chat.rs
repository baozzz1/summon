//! 跨会话索引/检索的落盘侧:扫 `runtime/chats/*.meta` 与 `threads/<tag>/transcript.md`,
//! 喂给纯件 `summon_core::cross_chat` 渲染。纯件负责「渲染什么」,这里只负责「从哪读」。
//!
//! 全部是**只读**本 agent 目录下已有的文件:不联网、不起进程、不写盘。索引段要拼进冷启
//! PRIMER,所以这条路径上只允许 `read_dir` + 小文件读 + `stat`,绝不整份读 3MB 的转录
//! ——那会把冷启的「零延迟」承诺变成谎话。正文只在 agent 显式跑 `summon chats read` 时才读。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use summon_core::chat_meta::ChatMeta;
use summon_core::cross_chat::{ChatDigest, GrepHit};

/// 单次 `summon chats grep` 每个会话最多回多少条命中(防一个关键词把整份转录拖出来)。
pub const GREP_MAX_PER_CHAT: usize = 5;

fn chats_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("runtime").join("chats")
}

pub fn transcript_path(agent_dir: &Path, tag: &str) -> PathBuf {
    agent_dir.join("threads").join(tag).join("transcript.md")
}

/// 文件 mtime → `%Y-%m-%d %H:%M:%S` 本地时刻。取不到(文件不存在/平台不支持)→ None。
fn mtime_stamp(p: &Path) -> Option<String> {
    let modified = std::fs::metadata(p).ok()?.modified().ok()?;
    let dt: chrono::DateTime<chrono::Local> = modified.into();
    Some(dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// 扫出本 agent 全部会话的元数据。**只读 `.meta` 小文件 + `stat` 转录**,不读转录内容。
///
/// 同一个 tag 可能有多份 `.meta`(改过 tmux 名 / 历史遗留),按 `.meta` 自身 mtime 取最新那份。
/// 任何一份读不出/缺 tag 的直接跳过——索引是尽力而为的观测,不该因为一个坏文件整段消失。
pub fn collect_digests(agent_dir: &Path) -> Vec<ChatDigest> {
    let Ok(entries) = std::fs::read_dir(chats_dir(agent_dir)) else {
        return Vec::new();
    };
    // tag → (该 .meta 的 mtime, digest);同 tag 多份时留 mtime 最大的。
    let mut best: HashMap<String, (std::time::SystemTime, ChatDigest)> = HashMap::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("meta") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        let m = ChatMeta::parse(&content);
        let Some(tag) = m.tag.clone().filter(|t| !t.is_empty()) else {
            continue;
        };
        let is_group = matches!(m.chat_type.as_deref(), Some("group") | Some("topic_group"));
        let tpath = transcript_path(agent_dir, &tag);
        let has_body = std::fs::metadata(&tpath)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        // 最后活动:转录 mtime 最准(真有一轮落盘才变),回退 `.meta` 的 updated_at(收到消息就写)。
        let last_active = mtime_stamp(&tpath).or_else(|| m.updated_at.clone());
        let digest = ChatDigest {
            name: m.chat_name.filter(|n| !n.is_empty()).unwrap_or_else(|| {
                if is_group {
                    summon_i18n::text!("r35b49ee58a").into()
                } else {
                    summon_i18n::text!("r6add2714c4").into()
                }
            }),
            tag: tag.clone(),
            is_group,
            last_active,
            last_sender: m.last_sender,
            has_body,
            chat_id: m.chat_id.filter(|id| !id.is_empty()),
        };
        let meta_mtime = std::fs::metadata(&p)
            .and_then(|md| md.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        match best.get(&tag) {
            Some((prev, _)) if *prev >= meta_mtime => {}
            _ => {
                best.insert(tag, (meta_mtime, digest));
            }
        }
    }
    best.into_values().map(|(_, d)| d).collect()
}

/// 读某会话最近 `last_n` 轮正文。返回 `(正文, 前面被跳过的轮数)`。
/// 只读当前代 `transcript.md`——轮转出去的旧代不在检索面内(说明见调用方输出)。
pub fn read_turns(agent_dir: &Path, tag: &str, last_n: usize) -> Result<(String, usize)> {
    let p = transcript_path(agent_dir, tag);
    let text = std::fs::read_to_string(&p)
        .map_err(|e| anyhow::anyhow!(summon_i18n::format!("rd8ed2ba019", p.display())))?;
    Ok(summon_core::cross_chat::tail_turns(&text, last_n))
}

/// 跨全部会话搜关键词。`skip_tag` 排除当前会话(它已经在上下文里了)。
/// 每个会话最多回 `max_per_chat` 条命中,超出的条数一并返回——不静默截断。
pub fn grep_chats(
    agent_dir: &Path,
    needle: &str,
    skip_tag: Option<&str>,
    max_per_chat: usize,
) -> Vec<(ChatDigest, Vec<GrepHit>, usize)> {
    let mut digests = collect_digests(agent_dir);
    digests.sort_by(|a, b| b.last_active.cmp(&a.last_active).then(a.tag.cmp(&b.tag)));
    let mut out = Vec::new();
    for d in digests {
        if !d.has_body || skip_tag == Some(d.tag.as_str()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(transcript_path(agent_dir, &d.tag)) else {
            continue;
        };
        let (hits, dropped) = summon_core::cross_chat::grep_transcript(&text, needle, max_per_chat);
        if !hits.is_empty() {
            out.push((d, hits, dropped));
        }
    }
    out
}

/// 把用户/agent 给的 target 解析成唯一一个会话。接受三种写法:
/// 序号(与列表一致)、完整 tag、会话名子串(大小写不敏感)。
///
/// **匹配到多个就报错,绝不替调用方猜一个**——猜错是读了另一个群的内容,而这正是本特性
/// 最需要防的事。报错时把候选列出来,让它换个更具体的写法。
pub fn resolve_target(listed: &[ChatDigest], target: &str) -> Result<ChatDigest> {
    let t = target.trim();
    if t.is_empty() {
        bail!(summon_i18n::format!("r261d5edb78"));
    }
    if let Ok(n) = t.parse::<usize>() {
        return listed
            .get(n.wrapping_sub(1))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r634ad61808", listed.len())));
    }
    if let Some(d) = listed.iter().find(|d| d.tag == t) {
        return Ok(d.clone());
    }
    let lc = t.to_lowercase();
    let hits: Vec<&ChatDigest> = listed
        .iter()
        .filter(|d| d.name.to_lowercase().contains(&lc))
        .collect();
    match hits.len() {
        0 => bail!(summon_i18n::format!("r27e9aa2276")),
        1 => Ok(hits[0].clone()),
        _ => bail!(summon_i18n::format!(
            "r4af3a83f34",
            hits.len(),
            hits.iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join("、")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个 agent 目录:`runtime/chats/<session>.meta` + `threads/<tag>/transcript.md`。
    fn seed_chat(dir: &Path, tag: &str, name: &str, chat_type: &str, body: Option<&str>) {
        let chats = chats_dir(dir);
        std::fs::create_dir_all(&chats).unwrap();
        let meta = ChatMeta {
            chat_id: Some(format!("oc_{tag}")),
            chat_type: Some(chat_type.into()),
            chat_name: Some(name.into()),
            tag: Some(tag.into()),
            last_sender: Some("张三".into()),
            updated_at: Some("2026-08-01 09:00:00".into()),
            creator: None,
        };
        std::fs::write(chats.join(format!("soul-x-{tag}.meta")), meta.render()).unwrap();
        if let Some(b) = body {
            let tp = transcript_path(dir, tag);
            std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
            std::fs::write(&tp, b).unwrap();
        }
    }

    #[test]
    fn collect_reads_meta_and_marks_body_presence() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_chat(
            dir,
            "g1",
            "甲群",
            "group",
            Some("### 2026-08-01 09:00:00 用户\n话\n"),
        );
        seed_chat(dir, "p1", "张三", "p2p", None); // 无转录
        let mut got = collect_digests(dir);
        got.sort_by(|a, b| a.tag.cmp(&b.tag));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "甲群");
        assert!(got[0].is_group && got[0].has_body);
        assert_eq!(got[1].name, "张三");
        assert!(!got[1].is_group, "p2p 不是群");
        assert!(!got[1].has_body, "没有 transcript.md → 无正文可读");
    }

    #[test]
    fn collect_tolerates_missing_dir_and_broken_meta() {
        let tmp = tempfile::tempdir().unwrap();
        // 目录压根不存在 → 空,不报错。
        assert!(collect_digests(tmp.path()).is_empty());
        let chats = chats_dir(tmp.path());
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chats.join("坏的.meta"), "根本不是kv\n").unwrap(); // 缺 tag
        std::fs::write(chats.join("不是meta.session"), "cli=claude\n").unwrap(); // 非 .meta
        assert!(
            collect_digests(tmp.path()).is_empty(),
            "坏文件跳过而非整段失败"
        );
    }

    #[test]
    fn collect_falls_back_to_generic_name_when_meta_lacks_one() {
        let tmp = tempfile::tempdir().unwrap();
        let chats = chats_dir(tmp.path());
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chats.join("soul-x-g9.meta"), "chat_type=group\ntag=g9\n").unwrap();
        let got = collect_digests(tmp.path());
        assert_eq!(got[0].name, "群聊", "缺群名回退通用词,不是空字符串");
    }

    #[test]
    fn read_turns_returns_tail_and_skipped_count() {
        let tmp = tempfile::tempdir().unwrap();
        seed_chat(
            tmp.path(),
            "g1",
            "甲群",
            "group",
            Some("### h1\n一\n\n### h2\n二\n\n### h3\n三\n"),
        );
        let (text, skipped) = read_turns(tmp.path(), "g1", 2).unwrap();
        assert_eq!(skipped, 1);
        assert!(text.contains("二") && text.contains("三") && !text.contains("一"));
        // 不存在的 tag → Err(而不是静默空串,那会让调用方以为「该会话没内容」)。
        assert!(read_turns(tmp.path(), "g404", 5).is_err());
    }

    #[test]
    fn grep_skips_current_chat_and_bodyless() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_chat(dir, "g1", "甲群", "group", Some("### h\n提到 worktree\n"));
        seed_chat(dir, "g2", "乙群", "group", Some("### h\n也提到 worktree\n"));
        seed_chat(dir, "g3", "丙群", "group", None);
        let hits = grep_chats(dir, "worktree", Some("g1"), 5);
        assert_eq!(hits.len(), 1, "当前会话 g1 与无正文的 g3 都不搜");
        assert_eq!(hits[0].0.tag, "g2");
        assert_eq!(hits[0].1[0].line, "也提到 worktree");
    }

    fn dg(tag: &str, name: &str) -> ChatDigest {
        ChatDigest {
            tag: tag.into(),
            name: name.into(),
            is_group: true,
            last_active: None,
            last_sender: None,
            has_body: true,
            chat_id: Some(format!("oc_{tag}")),
        }
    }

    #[test]
    fn resolve_target_accepts_index_tag_and_name_fragment() {
        let listed = vec![dg("gaaa", "sample开发群"), dg("gbbb", "Demo支持XYZ")];
        assert_eq!(resolve_target(&listed, "1").unwrap().tag, "gaaa");
        assert_eq!(resolve_target(&listed, "gbbb").unwrap().tag, "gbbb");
        assert_eq!(
            resolve_target(&listed, "demo").unwrap().tag,
            "gbbb",
            "名字匹配大小写不敏感"
        );
    }

    #[test]
    fn resolve_target_refuses_to_guess_on_ambiguity() {
        // 猜错 = 读了另一个群的内容,正是本特性最该防的事 → 必须 bail 并列出候选。
        let listed = vec![dg("g1", "开发群一"), dg("g2", "开发群二")];
        let err = resolve_target(&listed, "开发群").unwrap_err().to_string();
        assert!(
            err.contains("命中 2 个") && err.contains("开发群一"),
            "{err}"
        );
        // 越界序号 / 无匹配 / 空串,一律 Err。
        assert!(resolve_target(&listed, "9").is_err());
        assert!(resolve_target(&listed, "不存在的群").is_err());
        assert!(resolve_target(&listed, "  ").is_err());
    }
}
