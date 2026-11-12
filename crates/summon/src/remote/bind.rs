//! 召回卡「遥控」落绑定之前的身份与占用核对。

use summon_core::remote::{
    surface_holds_other_session, CmuxClaudeSession, CmuxHookSessions, RemoteBinding,
};

const NOT_IN_CMUX: &str = "r1eaf372f60";
const PROCESS_EXITED: &str = "r2ac29182b6";
const PID_REUSED: &str = "ra9602795db";
const SURFACE_SWITCHED: &str = "r3bf51f681a";
const GROUP_ALREADY_BOUND: &str = "r495905e319";
const SESSION_BOUND_ELSEWHERE: &str = "r339ed8e146";

/// 落绑定前的核对:sid 在 cmux 记录里,它的窗口此刻挂着的仍是它,记录里的 pid 此刻还是同一个
/// 进程(起始时间相同,防 pid 复用),本群没有未结束的遥控,这条会话没有绑在别的群上。已结束的
/// 记录不挡,可以被新绑定替换。通过时返回那条 cmux 记录;`Err` 是回给点按钮那个人的原因。
pub(crate) fn prepare_binding(
    sid: &str,
    hooks: &CmuxHookSessions,
    process_start: &dyn Fn(u32) -> Option<i64>,
    already_bound_chat: Option<&RemoteBinding>,
    session_bound_elsewhere: Option<&RemoteBinding>,
) -> Result<CmuxClaudeSession, String> {
    let Some(session) = hooks.sessions.iter().find(|s| s.session_id == sid) else {
        return Err(::summon_core::i18n::label(NOT_IN_CMUX).into());
    };
    if surface_holds_other_session(&hooks.active_by_surface, &session.surface_id, sid) {
        return Err(::summon_core::i18n::label(SURFACE_SWITCHED).into());
    }
    match process_start(session.pid) {
        Some(start) if start == session.pid_start_seconds => {}
        Some(_) => return Err(::summon_core::i18n::label(PID_REUSED).into()),
        None => return Err(::summon_core::i18n::label(PROCESS_EXITED).into()),
    }
    if already_bound_chat.is_some_and(|binding| binding.ended.is_none()) {
        return Err(::summon_core::i18n::label(GROUP_ALREADY_BOUND).into());
    }
    if session_bound_elsewhere.is_some_and(|binding| binding.ended.is_none()) {
        return Err(::summon_core::i18n::label(SESSION_BOUND_ELSEWHERE).into());
    }
    Ok(session.clone())
}

/// 群里与回执里怎么称呼这条会话:Claude 自己的会话名,没有就用工作目录名,再没有就用 sid。
pub(crate) fn session_label(claude_name: Option<&str>, cwd: &str, sid: &str) -> String {
    claude_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .or_else(|| {
            std::path::Path::new(cwd)
                .file_name()
                .and_then(|name| name.to_str())
        })
        .unwrap_or(sid)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::remote::REMOTE_BINDING_SCHEMA_VERSION;

    fn session() -> CmuxClaudeSession {
        CmuxClaudeSession {
            session_id: "sid-1".into(),
            surface_id: "S".into(),
            workspace_id: "W".into(),
            pid: 42,
            pid_start_seconds: 1000,
            transcript_path: "/t.jsonl".into(),
            cwd: "/w".into(),
        }
    }

    fn hooks(sessions: Vec<CmuxClaudeSession>) -> CmuxHookSessions {
        CmuxHookSessions {
            sessions,
            ..CmuxHookSessions::default()
        }
    }

    fn binding(chat: &str, sid: &str, ended: Option<&str>) -> RemoteBinding {
        RemoteBinding {
            schema_version: REMOTE_BINDING_SCHEMA_VERSION,
            agent: "a".into(),
            scope: "a-s".into(),
            chat_id: chat.into(),
            chat_type: "group".into(),
            session_id: sid.into(),
            session_name: "n".into(),
            surface_id: "S".into(),
            workspace_id: "W".into(),
            pid: 42,
            pid_start_seconds: 1000,
            transcript_path: "/t.jsonl".into(),
            actor_open_id: "ou".into(),
            created_at_ms: 1,
            ended: ended.map(String::from),
        }
    }

    fn alive(pid: u32) -> Option<i64> {
        assert_eq!(pid, 42, "核对的是 cmux 记录里的 pid");
        Some(1000)
    }

    #[test]
    fn passes_when_the_same_process_is_running_and_nothing_is_bound() {
        assert_eq!(
            prepare_binding("sid-1", &hooks(vec![session()]), &alive, None, None),
            Ok(session())
        );
    }

    #[test]
    fn an_ended_record_does_not_block_rebinding() {
        let ended_here = binding("oc_g", "sid-9", Some("会话进程已经退出"));
        let ended_elsewhere = binding("oc_other", "sid-1", Some("会话进程已经退出"));
        assert_eq!(
            prepare_binding(
                "sid-1",
                &hooks(vec![session()]),
                &alive,
                Some(&ended_here),
                Some(&ended_elsewhere)
            ),
            Ok(session())
        );
    }

    #[test]
    fn refuses_a_session_cmux_does_not_know() {
        assert_eq!(
            prepare_binding("sid-2", &hooks(vec![session()]), &alive, None, None),
            Err(::summon_core::i18n::label(NOT_IN_CMUX).to_string())
        );
    }

    #[test]
    fn refuses_when_the_window_now_holds_another_session() {
        let switched = CmuxHookSessions {
            sessions: vec![session()],
            active_by_surface: [("S".to_string(), "sid-2".to_string())].into(),
            skipped: Vec::new(),
        };
        assert_eq!(
            prepare_binding("sid-1", &switched, &alive, None, None),
            Err(::summon_core::i18n::label(SURFACE_SWITCHED).to_string())
        );
    }

    #[test]
    fn refuses_when_the_process_has_exited() {
        assert_eq!(
            prepare_binding("sid-1", &hooks(vec![session()]), &|_| None, None, None),
            Err(::summon_core::i18n::label(PROCESS_EXITED).to_string())
        );
    }

    #[test]
    fn refuses_when_the_pid_now_belongs_to_another_process() {
        assert_eq!(
            prepare_binding(
                "sid-1",
                &hooks(vec![session()]),
                &|_| Some(2000),
                None,
                None
            ),
            Err(::summon_core::i18n::label(PID_REUSED).to_string())
        );
    }

    #[test]
    fn refuses_when_the_group_is_already_remote_controlled() {
        let live_here = binding("oc_g", "sid-9", None);
        assert_eq!(
            prepare_binding(
                "sid-1",
                &hooks(vec![session()]),
                &alive,
                Some(&live_here),
                None
            ),
            Err(::summon_core::i18n::label(GROUP_ALREADY_BOUND).to_string())
        );
    }

    #[test]
    fn label_prefers_the_claude_name_then_the_directory_then_the_sid() {
        assert_eq!(session_label(Some(" 修图 "), "/w/photos", "sid-1"), "修图");
        assert_eq!(session_label(Some(""), "/w/photos", "sid-1"), "photos");
        assert_eq!(session_label(None, "/", "sid-1"), "sid-1");
    }

    #[test]
    fn refuses_when_the_session_is_bound_to_another_group() {
        let live_elsewhere = binding("oc_other", "sid-1", None);
        assert_eq!(
            prepare_binding(
                "sid-1",
                &hooks(vec![session()]),
                &alive,
                None,
                Some(&live_elsewhere)
            ),
            Err(::summon_core::i18n::label(SESSION_BOUND_ELSEWHERE).to_string())
        );
    }
}
