//! per-chat 会话事实发现(IO 层):统一枚举 tmux、`.meta` 与 `threads/` 三个来源。
//!
//! 所有本机命令都必须从这里拿同一份会话集合，避免一个入口只看 tmux、另一个入口能看
//! 无头历史的真相分叉。liveness 判定仍在 `summon-core::session_view` 的纯逻辑里。

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use summon_core::chat_meta::ChatMeta;
use summon_core::registry::Agent;
use summon_core::session_view::{chat_type_from_tag, classify_liveness, SessionView};

fn session_belongs_to_agent(session: &str, base: &str) -> bool {
    session == base || session.starts_with(&format!("{base}-"))
}

fn chat_meta_path(agent: &Agent, session: &str) -> std::path::PathBuf {
    agent
        .state_dir()
        .join(summon_core::chat_meta::meta_rel_path(session))
}

pub(crate) fn read_chat_meta(agent: &Agent, session: &str) -> ChatMeta {
    let Ok(s) = std::fs::read_to_string(chat_meta_path(agent, session)) else {
        return ChatMeta::default();
    };
    ChatMeta::parse(&s)
}

pub(crate) fn chat_tag_for_session(base: &str, session: &str, meta: &ChatMeta) -> String {
    if let Some(tag) = &meta.tag {
        return tag.clone();
    }
    // `.meta` 没记 tag(legacy 会话)时,**优先用 chat_type + chat_id 重建 canonical tag**,
    // 而不是直接从 session 名去猜。两者会给出不同答案:所有按 tag 命名的旁路文件
    // (`<tag>.lifecycle`、`<tag>.exit-notified`、`threads/<tag>/`)都用 canonical tag,
    // 而裸名会话猜出来的是 `(base/owner)` —— 对不上就等于这些事实全都读不到。
    if let (Some(chat_type), Some(chat_id)) = (&meta.chat_type, &meta.chat_id) {
        return summon_core::registry::chat_tag(chat_type, chat_id);
    }
    session
        .strip_prefix(&format!("{base}-"))
        .unwrap_or("(base/owner)")
        .to_string()
}

/// 把 `.meta.updated_at`(`%Y-%m-%d %H:%M:%S` 本地时区)解析成 SystemTime。
pub(crate) fn parse_meta_updated_at(s: &str) -> Option<SystemTime> {
    use chrono::{Local, NaiveDateTime, TimeZone};
    let naive = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let dt = Local.from_local_datetime(&naive).earliest()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(dt.timestamp().max(0) as u64))
}

fn thread_events_mtime(agent: &Agent, tag: &str) -> Option<SystemTime> {
    let path = agent
        .state_dir()
        .join("threads")
        .join(tag)
        .join("events.jsonl");
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub(crate) fn session_last_active(agent: &Agent, tag: &str, meta: &ChatMeta) -> Option<SystemTime> {
    thread_events_mtime(agent, tag)
        .or_else(|| meta.updated_at.as_deref().and_then(parse_meta_updated_at))
}

/// 扫 `<agent>/threads/*/events.jsonl`，返回真正有事件流的 chat-tag。
pub(crate) fn event_log_tags(agent: &Agent) -> Vec<String> {
    let threads = agent.state_dir().join("threads");
    let Ok(entries) = std::fs::read_dir(&threads) else {
        return Vec::new();
    };
    let mut tags: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.is_dir() && p.join("events.jsonl").is_file())
                .then(|| p.file_name().and_then(|s| s.to_str()).map(String::from))
                .flatten()
        })
        .collect();
    tags.sort();
    tags
}

/// 枚举某 agent 的全量 per-chat 会话(tmux ∪ `.meta` ∪ threads-only 孤儿)。
pub(crate) fn discover_sessions(
    agent: &Agent,
    tmux_sessions: &[String],
    now: SystemTime,
    ttl: Duration,
) -> Vec<SessionView> {
    let base = agent.tmux_name();
    let capture = agent.capture_mode();
    let mut by_tag: BTreeMap<String, (String, ChatMeta, bool)> = BTreeMap::new();

    let chats_dir = agent.state_dir().join("runtime").join("chats");
    if let Ok(entries) = std::fs::read_dir(&chats_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(session) = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_suffix(".meta"))
            else {
                continue;
            };
            let meta = read_chat_meta(agent, session);
            let tag = chat_tag_for_session(&base, session, &meta);
            by_tag
                .entry(tag)
                .or_insert_with(|| (session.to_string(), meta, false));
        }
    }

    for session in tmux_sessions
        .iter()
        .filter(|s| session_belongs_to_agent(s, &base))
    {
        let meta = read_chat_meta(agent, session);
        let tag = chat_tag_for_session(&base, session, &meta);
        let entry = by_tag
            .entry(tag)
            .or_insert_with(|| (session.clone(), meta, false));
        entry.2 = true;
    }

    for tag in event_log_tags(agent) {
        by_tag.entry(tag.clone()).or_insert_with(|| {
            let meta = ChatMeta {
                chat_type: chat_type_from_tag(&tag).map(String::from),
                ..Default::default()
            };
            (format!("{base}-{tag}"), meta, false)
        });
    }

    by_tag
        .into_iter()
        .map(|(tag, (session, meta, tmux_present))| {
            let last_active = session_last_active(agent, &tag, &meta);
            // 路由生命周期事实。缺记录(升级前的存量会话)= Unknown = 不是 closed,判活原样走旧判据。
            let lifecycle_closed = crate::session::lifecycle::read(agent, &tag).is_closed();
            let liveness = classify_liveness(
                tmux_present,
                capture,
                last_active,
                now,
                ttl,
                Some(Duration::from_secs(7 * 86400)),
                lifecycle_closed,
            );
            SessionView {
                agent: agent.name.clone(),
                tag,
                session,
                chat_id: meta.chat_id,
                chat_type: meta.chat_type,
                chat_name: meta.chat_name,
                capture,
                liveness,
                last_active,
            }
        })
        .collect()
}
