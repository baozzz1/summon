//! 会话诊断包的纯逻辑:时间窗、结构化记录的白名单投影、日志行的模板白名单与人员编号。读文件、
//! 打包在 `summon::runtime::export`。设计见 `docs/spec/2026-10-29-session-export-design.md`。
//!
//! 诊断包(默认档)只由白名单投影生成:每个输出结构的字段逐个列出,源记录新增的字段不会自己
//! 跑进诊断包。完整包在诊断包之外另附原件。

pub mod events;
pub mod logs;
pub mod native;

use std::collections::HashMap;

use serde::Serialize;

use crate::observe::turn_ledger::{
    ModelUsage, TurnCtx, TurnEnd, TurnOrigin, TurnQuota, TurnRecord,
};

/// 导出哪一档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// 不含对话内容。
    Diagnostic,
    /// 另附对话内容与原生记录。
    Full,
}

impl Mode {
    pub fn is_full(self) -> bool {
        self == Mode::Full
    }

    /// 包文件名里的档位后缀。
    pub fn file_suffix(self) -> &'static str {
        match self {
            Mode::Diagnostic => "",
            Mode::Full => "-full",
        }
    }
}

/// 默认时间窗覆盖的轮数(在跑的那一轮算一轮)。
pub const DEFAULT_TURNS: usize = 3;
/// 按轮取窗时,起点在最早那轮开始前留的余量:入站、排队、拉起会话都发生在轮开始之前。
pub const WINDOW_LEAD_MS: i64 = 5 * 60 * 1000;
/// 本会话一轮记录都没有时往回看多久。
pub const EMPTY_LOOKBACK_MS: i64 = 24 * 3600 * 1000;

/// 时间窗怎么取。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSpec {
    /// 最近 N 轮。
    Turns(usize),
    /// 最近这么多毫秒。
    SinceMs(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Window {
    pub start_ms: i64,
    pub end_ms: i64,
}

impl Window {
    pub fn contains(&self, at_ms: i64) -> bool {
        at_ms >= self.start_ms && at_ms <= self.end_ms
    }
}

/// 算时间窗。`starts` 是本会话已收尾各轮的开始时刻,`running_since` 是在跑那一轮的开始时刻。
pub fn window(spec: WindowSpec, starts: &[i64], running_since: Option<i64>, now_ms: i64) -> Window {
    let start_ms = match spec {
        WindowSpec::SinceMs(span) => now_ms - span.max(0),
        WindowSpec::Turns(n) => {
            let mut all: Vec<i64> = starts.iter().copied().chain(running_since).collect();
            all.sort_unstable();
            let n = n.max(1);
            match all
                .len()
                .checked_sub(n)
                .map(|i| all[i])
                .or(all.first().copied())
            {
                Some(earliest) => earliest.min(now_ms) - WINDOW_LEAD_MS,
                None => now_ms - EMPTY_LOOKBACK_MS,
            }
        }
    };
    Window {
        start_ms,
        end_ms: now_ms,
    }
}

/// 飞书里的人在包内的编号。同一个串恒得同一个编号;[`People::link`] 把同一个人的名字和 id
/// 绑到一个编号上。
#[derive(Debug, Default)]
pub struct People {
    numbers: HashMap<String, usize>,
    /// 已发出的编号数。名字与 id 共用编号,所以不能拿 `numbers.len()` 当计数。
    issued: usize,
}

impl People {
    /// 这个人的编号(`user-N`)。
    pub fn alias(&mut self, key: &str) -> String {
        let n = match self.numbers.get(key) {
            Some(n) => *n,
            None => {
                let n = self.issue();
                self.numbers.insert(key.to_string(), n);
                n
            }
        };
        format!("user-{n}")
    }

    fn issue(&mut self) -> usize {
        self.issued += 1;
        self.issued
    }

    /// 名字与 id 是同一个人:已认识其中一个就沿用它的编号。
    pub fn link(&mut self, name: &str, id: &str) -> String {
        let known = self
            .numbers
            .get(id)
            .or_else(|| self.numbers.get(name))
            .copied();
        let n = known.unwrap_or_else(|| self.issue());
        self.numbers.insert(id.to_string(), n);
        if !name.is_empty() {
            self.numbers.insert(name.to_string(), n);
        }
        format!("user-{n}")
    }

    /// 文本里所有飞书用户 id(`ou_` / `on_` 开头)换成编号。
    pub fn replace_ids(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = find_user_id(rest) {
            out.push_str(&rest[..at]);
            let id_len = rest[at..]
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            let id = &rest[at..at + id_len];
            out.push_str(&self.alias(id));
            rest = &rest[at + id_len..];
        }
        out.push_str(rest);
        out
    }
}

/// 下一个飞书用户 id 的起点:`ou_` / `on_` 前面不是字母数字,后面至少 8 个字母数字。
fn find_user_id(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = text[from..].find(['o']) {
        let at = from + rel;
        let prefix_ok = bytes.get(at + 1).is_some_and(|b| *b == b'u' || *b == b'n')
            && bytes.get(at + 2) == Some(&b'_');
        let boundary = at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        let body = bytes[(at + 3).min(bytes.len())..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        if prefix_ok && boundary && body >= 8 {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// 家目录换成 `~`:用户名常常就是人名。
pub fn tilde(text: &str, home: &str) -> String {
    if home.is_empty() || home == "/" {
        return text.to_string();
    }
    let home = home.trim_end_matches('/');
    let replaced = text.replace(&format!("{home}/"), "~/");
    if replaced == home {
        "~".to_string()
    } else {
        replaced
    }
}

/// 诊断包里的一轮。触发消息开头与 feed 总结是对话内容,不在这里。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiagTurn {
    pub chat_id: String,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    pub origin: TurnOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    pub message_id: String,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub end: TurnEnd,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub reattached: bool,
    pub tool_steps: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx: Option<TurnCtx>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<ModelUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<TurnQuota>,
}

pub fn diag_turn(rec: &TurnRecord, people: &mut People) -> DiagTurn {
    DiagTurn {
        chat_id: rec.chat_id.clone(),
        tag: rec.tag.clone(),
        turn: rec.turn.clone(),
        origin: rec.origin,
        schedule: rec.schedule.clone(),
        requester: rec.requester.as_deref().map(|id| people.alias(id)),
        message_id: rec.message_id.clone(),
        started_at_ms: rec.started_at_ms,
        ended_at_ms: rec.ended_at_ms,
        end: rec.end,
        error: rec.error.clone(),
        reattached: rec.reattached,
        tool_steps: rec.tool_steps,
        cli: rec.cli.clone(),
        model: rec.model.clone(),
        effort: rec.effort.clone(),
        ctx: rec.ctx.clone(),
        usage: rec.usage.clone(),
        quota: rec.quota.clone(),
    }
}

/// JSON 值的数字骨架:保留对象结构与数字、布尔,字符串一律去掉。token 用量、额度这类全是数字的
/// 结构经它原样保留,夹在里面的文字(套餐名、说明)不会带出去。
pub fn numeric_skeleton(value: &serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::Value;
    match value {
        Value::Number(_) | Value::Bool(_) => Some(value.clone()),
        Value::Object(map) => {
            let kept: serde_json::Map<String, Value> = map
                .iter()
                .filter_map(|(k, v)| numeric_skeleton(v).map(|v| (k.clone(), v)))
                .collect();
            (!kept.is_empty()).then_some(Value::Object(kept))
        }
        Value::Array(items) => {
            let kept: Vec<Value> = items.iter().filter_map(numeric_skeleton).collect();
            (!kept.is_empty()).then_some(Value::Array(kept))
        }
        Value::Null | Value::String(_) => None,
    }
}

/// 失败原文的类别。原文可能复述对话内容,诊断包只留类别。
pub fn error_class(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if has(&[
        "newer version",
        "upgrade to the latest",
        "unsupported model",
        "not supported",
    ]) {
        "client_version"
    } else if has(&[
        "usage limit",
        "rate limit",
        "quota",
        summon_i18n::text!("r32ce9f0658"),
        "insufficient",
    ]) {
        "quota"
    } else if has(&["capacity", "overloaded"]) {
        "capacity"
    } else if has(&[
        "stream",
        "disconnect",
        "connection",
        "timed out",
        "timeout",
        "reset by peer",
    ]) {
        "stream"
    } else if has(&["auth", "unauthorized", "forbidden", "401", "403", "login"]) {
        "auth"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_window_starts_before_the_earliest_of_the_last_n_turns() {
        let w = window(
            WindowSpec::Turns(3),
            &[100_000_000, 200_000_000, 300_000_000, 400_000_000],
            None,
            500_000_000,
        );
        assert_eq!(w.start_ms, 200_000_000 - WINDOW_LEAD_MS);
        assert_eq!(w.end_ms, 500_000_000);
    }

    #[test]
    fn a_running_turn_counts_as_one_of_the_last_n() {
        let w = window(
            WindowSpec::Turns(2),
            &[100_000_000, 200_000_000, 300_000_000],
            Some(450_000_000),
            500_000_000,
        );
        assert_eq!(w.start_ms, 300_000_000 - WINDOW_LEAD_MS);
    }

    #[test]
    fn fewer_turns_than_asked_starts_at_the_first_one_and_none_looks_back_a_day() {
        let few = window(WindowSpec::Turns(3), &[200_000_000], None, 500_000_000);
        assert_eq!(few.start_ms, 200_000_000 - WINDOW_LEAD_MS);
        let none = window(WindowSpec::Turns(3), &[], None, 500_000_000);
        assert_eq!(none.start_ms, 500_000_000 - EMPTY_LOOKBACK_MS);
        let since = window(WindowSpec::SinceMs(7_200_000), &[1], None, 500_000_000);
        assert_eq!(since.start_ms, 500_000_000 - 7_200_000);
    }

    #[test]
    fn people_get_stable_numbers_and_linked_names_share_one() {
        let mut people = People::default();
        assert_eq!(people.alias("ou_aaaaaaaaaaaa"), "user-1");
        assert_eq!(people.alias("ou_bbbbbbbbbbbb"), "user-2");
        assert_eq!(people.alias("ou_aaaaaaaaaaaa"), "user-1");
        assert_eq!(people.link("示例用户", "ou_bbbbbbbbbbbb"), "user-2");
        assert_eq!(people.alias("示例用户"), "user-2");
        assert_eq!(people.link("另一人", "ou_cccccccccccc"), "user-3");
    }

    #[test]
    fn user_ids_in_free_text_become_numbers() {
        let mut people = People::default();
        let text = "sender=ou_0123456789abcdef chat=oc_00000000000000000000000000000001 on_0000000000000002,ou_short";
        assert_eq!(
            people.replace_ids(text),
            "sender=user-1 chat=oc_00000000000000000000000000000001 user-2,ou_short"
        );
        assert_eq!(
            people.replace_ids("youou_0123456789abc"),
            "youou_0123456789abc"
        );
    }

    #[test]
    fn home_becomes_tilde_only_at_a_path_boundary() {
        assert_eq!(tilde("/Users/someone/work/x", "/Users/someone"), "~/work/x");
        assert_eq!(tilde("/Users/someone", "/Users/someone/"), "~");
        assert_eq!(
            tilde("/Users/someone-else/x", "/Users/someone"),
            "/Users/someone-else/x"
        );
    }

    #[test]
    fn numeric_skeleton_keeps_numbers_and_drops_text() {
        let v = serde_json::json!({
            "info": {"total": {"input": 10, "note": "secret"}, "window": 200000},
            "plan": "pro",
            "flags": [true, "x", 3],
        });
        assert_eq!(
            numeric_skeleton(&v).unwrap(),
            serde_json::json!({"info": {"total": {"input": 10}, "window": 200000}, "flags": [true, 3]})
        );
    }

    #[test]
    fn error_class_recognises_the_common_provider_failures() {
        assert_eq!(
            error_class("The 'm' model requires a newer version of Codex. Please upgrade to the latest app or CLI"),
            "client_version"
        );
        assert_eq!(error_class("You've hit your usage limit"), "quota");
        assert_eq!(error_class("Selected model is at capacity"), "capacity");
        assert_eq!(
            error_class("stream disconnected before completion"),
            "stream"
        );
        assert_eq!(error_class("something odd"), "other");
    }

    #[test]
    fn diag_turn_drops_the_prompt_head_and_feed_and_numbers_the_requester() {
        let rec: TurnRecord = serde_json::from_value(serde_json::json!({
            "agent": "a", "chat_id": "oc_1", "tag": "g1", "turn": "t1", "origin": "human",
            "requester": "ou_0123456789abcdef", "message_id": "om_1",
            "prompt_head": "CANARY prompt", "feed": "CANARY feed",
            "started_at_ms": 1, "ended_at_ms": 2, "end": "done", "tool_steps": 3,
        }))
        .unwrap();
        let mut people = People::default();
        let out = serde_json::to_string(&diag_turn(&rec, &mut people)).unwrap();
        assert!(!out.contains("CANARY"), "{out}");
        assert!(out.contains("\"requester\":\"user-1\""), "{out}");
        assert!(out.contains("\"tool_steps\":3"), "{out}");
    }
}
