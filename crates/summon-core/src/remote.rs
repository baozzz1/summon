//! cmux 遥控的纯数据:cmux 会话记录解析、绑定记录、群消息预处理。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::messaging::ipc::Mention;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmuxClaudeSession {
    pub session_id: String,
    pub surface_id: String,
    pub workspace_id: String,
    pub pid: u32,
    pub pid_start_seconds: i64,
    pub transcript_path: String,
    pub cwd: String,
}

/// `~/.cmuxterm/claude-hook-sessions.json` 里遥控用得到的部分。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmuxHookSessions {
    pub sessions: Vec<CmuxClaudeSession>,
    /// surfaceId → 这个窗口此刻挂着的 sessionId(`activeSessionsBySurface`)。同一个进程里
    /// `/clear` 之类会换会话,旧会话的记录还在、pid 也没变,只有这张表看得出窗口已经换了人。
    pub active_by_surface: BTreeMap<String, String>,
    /// 字段类型不符、被单条跳过的记录(`sessions` 下的键),由调用方记日志。
    pub skipped: Vec<String>,
}

#[derive(Deserialize)]
struct HookFile {
    #[serde(default)]
    sessions: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "activeSessionsBySurface")]
    active_sessions_by_surface: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookRecord {
    session_id: Option<String>,
    surface_id: Option<String>,
    workspace_id: Option<String>,
    pid: Option<u32>,
    pid_start_seconds: Option<i64>,
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: String,
}

impl HookRecord {
    fn into_session(self) -> Option<CmuxClaudeSession> {
        Some(CmuxClaudeSession {
            session_id: self.session_id.filter(|v| !v.is_empty())?,
            surface_id: self.surface_id.filter(|v| !v.is_empty())?,
            workspace_id: self.workspace_id.unwrap_or_default(),
            pid: self.pid.filter(|p| *p > 0)?,
            pid_start_seconds: self.pid_start_seconds.filter(|s| *s > 0)?,
            transcript_path: self.transcript_path.filter(|v| !v.is_empty())?,
            cwd: self.cwd,
        })
    }
}

/// 解析 `~/.cmuxterm/claude-hook-sessions.json`。缺 pid、起始时间、窗口或转录路径的记录
/// 证明不了身份,直接跳过;单条记录字段类型不符只跳过那一条,别的会话照常可用。顶层结构
/// 坏了(不是 JSON、`sessions` 不是对象)按错误返回。
pub fn parse_claude_hook_sessions(json: &str) -> Result<CmuxHookSessions, serde_json::Error> {
    let file: HookFile = serde_json::from_str(json)?;
    let mut skipped = Vec::new();
    let mut sessions = Vec::new();
    for (key, value) in file.sessions {
        let record: HookRecord = match serde_json::from_value(value) {
            Ok(record) => record,
            Err(_) => {
                skipped.push(key);
                continue;
            }
        };
        sessions.extend(record.into_session());
    }
    // 表里的条目认不出就当这个窗口不在表里:这张表只用来判「换了会话」,缺了不据此判定。
    let active_by_surface = match file.active_sessions_by_surface {
        Some(serde_json::Value::Object(map)) => map
            .into_iter()
            .filter_map(|(surface, entry)| {
                let sid = match &entry {
                    serde_json::Value::String(sid) => Some(sid.as_str()),
                    serde_json::Value::Object(fields) => {
                        fields.get("sessionId").and_then(serde_json::Value::as_str)
                    }
                    _ => None,
                }?;
                (!sid.is_empty()).then(|| (surface, sid.to_string()))
            })
            .collect(),
        _ => BTreeMap::new(),
    };
    Ok(CmuxHookSessions {
        sessions,
        active_by_surface,
        skipped,
    })
}

/// 这个窗口此刻挂着的是不是另一条会话。表里没有这个窗口时证明不了,返回 false。
pub fn surface_holds_other_session(
    active_by_surface: &BTreeMap<String, String>,
    surface_id: &str,
    session_id: &str,
) -> bool {
    active_by_surface
        .get(surface_id)
        .is_some_and(|current| current != session_id)
}

pub const REMOTE_BINDING_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBinding {
    pub schema_version: u32,
    pub agent: String,
    pub scope: String,
    pub chat_id: String,
    pub chat_type: String,
    pub session_id: String,
    pub session_name: String,
    pub surface_id: String,
    pub workspace_id: String,
    pub pid: u32,
    pub pid_start_seconds: i64,
    pub transcript_path: String,
    pub actor_open_id: String,
    pub created_at_ms: u64,
    /// Some = 遥控已结束(原因),等群里下一条消息回一次「遥控已结束」后删除记录。
    #[serde(default)]
    pub ended: Option<String>,
}

/// 绑定只认同一个窗口里的同一个进程:pid 复用、进程重启、换窗口都算换了人。
pub fn binding_matches(binding: &RemoteBinding, session: &CmuxClaudeSession) -> bool {
    binding.session_id == session.session_id
        && binding.surface_id == session.surface_id
        && binding.pid == session.pid
        && binding.pid_start_seconds == session.pid_start_seconds
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteMessage {
    /// exact `/reset`:解除遥控。
    Reset,
    Lines(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMessageError {
    Empty,
    /// `cmux send` 把 `\n` `\r` `\t` 当按键转义,反斜杠会让消息中途提交。
    Backslash,
    /// 首行以 `/` 开头且不是 exact `/reset`。
    SlashCommand,
    /// 去掉 `\r`、制表符换成空格之后仍有控制字符(换行除外):cmux 会把它当成按键。
    ControlChar,
}

/// 群消息 → 要打进输入框的行。去掉对 bot 的 @,其他人的 @ 还原成 `@名字`,保留换行。
pub fn remote_message(
    text: &str,
    mentions: &[Mention],
    bot_open_id: Option<&str>,
) -> Result<RemoteMessage, RemoteMessageError> {
    // key 按长度从长到短替换:`@_user_1` 是 `@_user_10` 的前缀,短的先换会把长的截坏。
    let mut ordered: Vec<&Mention> = mentions.iter().filter(|m| !m.key.is_empty()).collect();
    ordered.sort_by_key(|m| std::cmp::Reverse(m.key.len()));
    let mut body = text.to_string();
    for m in ordered {
        let replacement = if Some(m.open_id.as_str()) == bot_open_id {
            String::new()
        } else {
            format!("@{}", m.name)
        };
        body = body.replace(&m.key, &replacement);
    }
    if body.contains('\\') {
        return Err(RemoteMessageError::Backslash);
    }
    let body = body.replace('\r', "").replace('\t', "    ");
    if body.chars().any(|c| c.is_control() && c != '\n') {
        return Err(RemoteMessageError::ControlChar);
    }
    let mut lines: Vec<String> = body.lines().map(|l| l.trim_end().to_string()).collect();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return Err(RemoteMessageError::Empty);
    }
    // 去掉 @ 之后首行留下的前导空格。
    lines[0] = lines[0].trim_start().to_string();
    if lines.len() == 1 && lines[0] == "/reset" {
        return Ok(RemoteMessage::Reset);
    }
    // summon 群里 `/quit` `/exit` 是归档会话的习惯用语,原样打进去会让 cmux 里的长任务退出。
    if lines[0].starts_with('/') {
        return Err(RemoteMessageError::SlashCommand);
    }
    Ok(RemoteMessage::Lines(lines))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::ipc::Mention;

    const HOOKS: &str = r#"{"version":1,"sessions":{
      "sid-a":{"sessionId":"sid-a","surfaceId":"S-A","workspaceId":"W-A","pid":33487,
               "pidStartSeconds":1791030026,"transcriptPath":"/t/a.jsonl","cwd":"/w","agentLifecycle":"running"},
      "sid-b":{"sessionId":"sid-b","surfaceId":"S-B","workspaceId":"W-B","cwd":"/w"}},
      "activeSessionsBySurface":{}}"#;

    #[test]
    fn parses_complete_records_and_skips_incomplete() {
        let sessions = parse_claude_hook_sessions(HOOKS).unwrap().sessions;
        assert_eq!(
            sessions.len(),
            1,
            "缺 pid/起始时间/转录路径的记录不能拿来证明身份"
        );
        assert_eq!(sessions[0].surface_id, "S-A");
        assert_eq!(sessions[0].pid, 33487);
        assert_eq!(sessions[0].pid_start_seconds, 1791030026);
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_claude_hook_sessions("{").is_err());
        assert!(
            parse_claude_hook_sessions(r#"{"sessions":[]}"#).is_err(),
            "sessions 不是对象就是顶层结构坏了"
        );
    }

    #[test]
    fn a_mistyped_record_is_skipped_alone() {
        // sid-bad 的 pid 是字符串:只跳过它,同一文件里的 sid-a 照常可用。
        let json = r#"{"sessions":{
          "sid-a":{"sessionId":"sid-a","surfaceId":"S-A","workspaceId":"W-A","pid":33487,
                   "pidStartSeconds":1791030026,"transcriptPath":"/t/a.jsonl","cwd":"/w"},
          "sid-bad":{"sessionId":"sid-bad","surfaceId":"S-B","pid":"33488",
                     "pidStartSeconds":1791030026,"transcriptPath":"/t/b.jsonl"},
          "sid-worse":"not an object"}}"#;
        let parsed = parse_claude_hook_sessions(json).unwrap();
        assert_eq!(parsed.sessions.len(), 1);
        assert_eq!(parsed.sessions[0].session_id, "sid-a");
        assert_eq!(
            parsed.skipped,
            vec!["sid-bad".to_string(), "sid-worse".to_string()]
        );
    }

    #[test]
    fn active_sessions_by_surface_maps_each_window_to_its_current_session() {
        // 真实文件里每个窗口是一个对象(带 sessionId / updatedAt / allowsNewSessionReplacement)。
        let json = r#"{"sessions":{},
          "activeSessionsBySurface":{
            "S-A":{"sessionId":"sid-a","updatedAt":1,"allowsNewSessionReplacement":false},
            "S-B":"sid-b",
            "S-C":{"updatedAt":1},
            "S-D":42,
            "S-E":{"sessionId":""}}}"#;
        let parsed = parse_claude_hook_sessions(json).unwrap();
        assert_eq!(
            parsed.active_by_surface,
            BTreeMap::from([
                ("S-A".to_string(), "sid-a".to_string()),
                ("S-B".to_string(), "sid-b".to_string()),
            ]),
            "认不出的条目当成不在表里"
        );
        let odd =
            parse_claude_hook_sessions(r#"{"sessions":{},"activeSessionsBySurface":[1]}"#).unwrap();
        assert!(odd.active_by_surface.is_empty(), "整张表认不出就不据此判定");
    }

    #[test]
    fn a_window_holding_another_session_is_detected_only_when_listed() {
        let table = BTreeMap::from([("S-A".to_string(), "sid-new".to_string())]);
        assert!(surface_holds_other_session(&table, "S-A", "sid-old"));
        assert!(!surface_holds_other_session(&table, "S-A", "sid-new"));
        assert!(
            !surface_holds_other_session(&table, "S-X", "sid-old"),
            "表里没有这个窗口时证明不了换了会话"
        );
    }

    fn binding() -> RemoteBinding {
        RemoteBinding {
            schema_version: REMOTE_BINDING_SCHEMA_VERSION,
            agent: "arcadia".into(),
            scope: "arcadia-x".into(),
            chat_id: "oc_g".into(),
            chat_type: "group".into(),
            session_id: "sid-a".into(),
            session_name: "修图".into(),
            surface_id: "S-A".into(),
            workspace_id: "W-A".into(),
            pid: 33487,
            pid_start_seconds: 1791030026,
            transcript_path: "/t/a.jsonl".into(),
            actor_open_id: "ou_owner".into(),
            created_at_ms: 1,
            ended: None,
        }
    }

    #[test]
    fn binding_matches_only_the_same_process_in_the_same_surface() {
        let s = parse_claude_hook_sessions(HOOKS)
            .unwrap()
            .sessions
            .remove(0);
        assert!(binding_matches(&binding(), &s));
        let mut other_pid = s.clone();
        other_pid.pid = 1;
        assert!(!binding_matches(&binding(), &other_pid));
        let mut restarted = s.clone();
        restarted.pid_start_seconds += 10;
        assert!(!binding_matches(&binding(), &restarted));
        let mut moved = s;
        moved.surface_id = "S-X".into();
        assert!(!binding_matches(&binding(), &moved));
    }

    #[test]
    fn binding_round_trips_and_old_records_default_to_live() {
        let json = serde_json::to_string(&binding()).unwrap();
        assert_eq!(
            serde_json::from_str::<RemoteBinding>(&json).unwrap(),
            binding()
        );
        let without_ended = json.replace(",\"ended\":null", "");
        assert_eq!(
            serde_json::from_str::<RemoteBinding>(&without_ended)
                .unwrap()
                .ended,
            None
        );
    }

    fn bot() -> Mention {
        Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "Arcadia".into(),
            kind: "bot".into(),
        }
    }

    #[test]
    fn strips_only_the_bot_mention_and_keeps_newlines() {
        let other = Mention {
            open_id: "ou_x".into(),
            key: "@_user_2".into(),
            name: "张三".into(),
            kind: "user".into(),
        };
        let got = remote_message(
            "@_user_1 第一行\n  问 @_user_2 第二行\t尾",
            &[bot(), other],
            Some("ou_bot"),
        )
        .unwrap();
        assert_eq!(
            got,
            RemoteMessage::Lines(vec!["第一行".into(), "  问 @张三 第二行    尾".into()])
        );
    }

    #[test]
    fn longer_mention_key_is_replaced_before_its_prefix() {
        let person = Mention {
            open_id: "ou_li".into(),
            key: "@_user_10".into(),
            name: "李四".into(),
            kind: "user".into(),
        };
        // 顺序故意让短 key 在前:`@_user_1` 是 `@_user_10` 的前缀,先替换会把后者截坏。
        let got = remote_message(
            "@_user_1 请 @_user_10 看一下",
            &[bot(), person],
            Some("ou_bot"),
        )
        .unwrap();
        assert_eq!(got, RemoteMessage::Lines(vec!["请 @李四 看一下".into()]));
    }

    #[test]
    fn slash_commands_are_refused_except_exact_reset() {
        for text in [
            "@_user_1 /quit",
            "@_user_1 /exit",
            "@_user_1 /clear",
            "@_user_1 /compact foo",
            "@_user_1   /quit",
            " /quit",
            "@_user_1 /reset now",
            "@_user_1 /reset\n再说一句",
        ] {
            assert_eq!(
                remote_message(text, &[bot()], Some("ou_bot")),
                Err(RemoteMessageError::SlashCommand),
                "{text:?}"
            );
        }
        assert_eq!(
            remote_message("@_user_1 /reset", &[bot()], Some("ou_bot")),
            Ok(RemoteMessage::Reset)
        );
    }

    #[test]
    fn slash_inside_the_text_is_not_a_command() {
        assert_eq!(
            remote_message("@_user_1 看看 a/b", &[bot()], Some("ou_bot")),
            Ok(RemoteMessage::Lines(vec!["看看 a/b".into()]))
        );
        // 只有首行开头的斜杠会被 Claude Code 当成命令;后面的行是同一条提示里的文字。
        assert_eq!(
            remote_message("@_user_1 看下面\n/quit", &[bot()], Some("ou_bot")),
            Ok(RemoteMessage::Lines(vec!["看下面".into(), "/quit".into()]))
        );
    }

    #[test]
    fn backslash_is_refused() {
        assert_eq!(
            remote_message(r"@_user_1 路径 C:\temp", &[bot()], Some("ou_bot")),
            Err(RemoteMessageError::Backslash)
        );
    }

    #[test]
    fn control_characters_are_refused() {
        // C0(Ctrl-C/D/U、ESC)、DEL 与 C1(CSI)都会被 cmux 当成按键。
        for c in ['\x03', '\x04', '\x1b', '\x15', '\x7f', '\u{9b}'] {
            assert_eq!(
                remote_message(&format!("@_user_1 前{c}后"), &[bot()], Some("ou_bot")),
                Err(RemoteMessageError::ControlChar),
                "{c:?}"
            );
        }
        // 回车去掉、制表符换空格、换行分行,这三样不算。
        assert_eq!(
            remote_message("@_user_1 甲\r\n乙\t丙", &[bot()], Some("ou_bot")),
            Ok(RemoteMessage::Lines(vec!["甲".into(), "乙    丙".into()]))
        );
    }

    #[test]
    fn blank_message_is_empty() {
        assert_eq!(
            remote_message("@_user_1  \n ", &[bot()], Some("ou_bot")),
            Err(RemoteMessageError::Empty)
        );
    }
}
