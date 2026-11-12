//! 已遥控群的入站裁决:dispatcher 在建 ChatHandle 之前问这里,这条群消息该怎么处理。

use summon_core::registry::Agent;
use summon_core::remote::{RemoteBinding, RemoteMessage, RemoteMessageError};

use super::runtime::ended_notice;
use super::store::RemoteStore;
use crate::observe::log::log;

const UNBIND_DENIED: &str = "r118577916f";
const EMPTY_MESSAGE: &str = "r37383cfa49";
const BACKSLASH_MESSAGE: &str = "rbfca03caa3";
const CONTROL_CHAR_MESSAGE: &str = "r845ba0ccbb";
const SLASH_COMMAND_MESSAGE: &str = "r91b7e54ebc";
pub(crate) const NOT_READY: &str = "ref9e98ab4b";
const UNBOUND_MEANWHILE: &str = "r6f3dfba741";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InboundPlan {
    NotBound,
    /// 绑定已结束:告知一次,dispatcher 发完删记录。
    EndedNotice(String),
    Unbind,
    Submit(Vec<String>),
    Reply(String),
}

/// `has_attachments`:消息带附件。只有附件、没有文字的消息照样打进去,worker 只打附件说明。
pub(crate) fn plan_inbound(
    binding: Option<&RemoteBinding>,
    message: Result<RemoteMessage, RemoteMessageError>,
    can_operate: bool,
    has_attachments: bool,
) -> InboundPlan {
    let Some(binding) = binding else {
        return InboundPlan::NotBound;
    };
    if let Some(reason) = &binding.ended {
        return InboundPlan::EndedNotice(ended_notice(reason));
    }
    match message {
        Ok(RemoteMessage::Reset) if can_operate => InboundPlan::Unbind,
        Ok(RemoteMessage::Reset) => {
            InboundPlan::Reply(::summon_core::i18n::label(UNBIND_DENIED).into())
        }
        Ok(RemoteMessage::Lines(lines)) => InboundPlan::Submit(lines),
        Err(RemoteMessageError::Empty) if has_attachments => InboundPlan::Submit(Vec::new()),
        Err(RemoteMessageError::Empty) => {
            InboundPlan::Reply(::summon_core::i18n::label(EMPTY_MESSAGE).into())
        }
        Err(RemoteMessageError::ControlChar) => {
            InboundPlan::Reply(::summon_core::i18n::label(CONTROL_CHAR_MESSAGE).into())
        }
        Err(RemoteMessageError::Backslash) => {
            InboundPlan::Reply(::summon_core::i18n::label(BACKSLASH_MESSAGE).into())
        }
        Err(RemoteMessageError::SlashCommand) => {
            InboundPlan::Reply(::summon_core::i18n::label(SLASH_COMMAND_MESSAGE).into())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartRefused {
    /// 记录已结束:告知并删记录,同 `InboundPlan::EndedNotice`。
    EndedNotice(String),
    Reply(&'static str),
}

/// 未结束的绑定没有在跑的 worker、`runtime::start` 也没起来之后,按重读的记录照实回这条消息。
/// start 只按盘上仍有效的同一份记录起,起不来时记录可能已结束、已解除、已换绑或读不出。
pub(crate) fn plan_start_refused(reloaded: &anyhow::Result<Option<RemoteBinding>>) -> StartRefused {
    match reloaded {
        Ok(Some(current)) => match &current.ended {
            Some(reason) => StartRefused::EndedNotice(ended_notice(reason)),
            None => StartRefused::Reply(::summon_core::i18n::label(NOT_READY)),
        },
        Ok(None) => StartRefused::Reply(::summon_core::i18n::label(UNBOUND_MEANWHILE)),
        Err(_) => StartRefused::Reply(::summon_core::i18n::label(NOT_READY)),
    }
}

/// 这个群是否在遥控中,本地会话入口据此跳过。已结束但还没在群里告知的记录也算:告知之前群里
/// 的下一条消息仍走遥控路径。记录读不出按在遥控中处理,宁可不起本地会话。
pub(crate) fn remote_bound(agent: &Agent, chat_id: &str) -> bool {
    match RemoteStore::for_agent(agent).load(chat_id) {
        Ok(binding) => binding.is_some(),
        Err(error) => {
            log(&summon_i18n::format!("r6b20e68a82", agent.name));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::remote::REMOTE_BINDING_SCHEMA_VERSION;

    fn binding(ended: Option<String>) -> RemoteBinding {
        RemoteBinding {
            schema_version: REMOTE_BINDING_SCHEMA_VERSION,
            agent: "a".into(),
            scope: "a-s".into(),
            chat_id: "oc_group".into(),
            chat_type: "group".into(),
            session_id: "sid-1".into(),
            session_name: "n".into(),
            surface_id: "S".into(),
            workspace_id: "W".into(),
            pid: 42,
            pid_start_seconds: 1000,
            transcript_path: "/t.jsonl".into(),
            actor_open_id: "ou".into(),
            created_at_ms: 1,
            ended,
        }
    }

    fn lines(lines: &[&str]) -> RemoteMessage {
        RemoteMessage::Lines(lines.iter().map(|l| l.to_string()).collect())
    }

    #[test]
    fn plan_covers_every_case() {
        let live = binding(None);
        assert_eq!(
            plan_inbound(None, Ok(RemoteMessage::Reset), true, false),
            InboundPlan::NotBound
        );
        assert!(matches!(
            plan_inbound(Some(&binding(Some("会话进程已经退出".into()))), Ok(lines(&["x"])), true, false),
            InboundPlan::EndedNotice(ref t) if t.contains("会话进程已经退出")
        ));
        assert_eq!(
            plan_inbound(Some(&live), Ok(RemoteMessage::Reset), true, false),
            InboundPlan::Unbind
        );
        assert!(matches!(
            plan_inbound(Some(&live), Ok(RemoteMessage::Reset), false, false),
            InboundPlan::Reply(_)
        ));
        assert_eq!(
            plan_inbound(Some(&live), Ok(lines(&["你好"])), false, false),
            InboundPlan::Submit(vec!["你好".into()])
        );
        assert!(matches!(
            plan_inbound(Some(&live), Err(RemoteMessageError::Backslash), true, false),
            InboundPlan::Reply(ref t) if t.contains("反斜杠")
        ));
        assert!(matches!(
            plan_inbound(Some(&live), Err(RemoteMessageError::Empty), true, false),
            InboundPlan::Reply(_)
        ));
    }

    #[test]
    fn control_characters_are_refused_with_the_reason() {
        assert_eq!(
            plan_inbound(
                Some(&binding(None)),
                Err(RemoteMessageError::ControlChar),
                true,
                true
            ),
            InboundPlan::Reply("消息里有控制字符,cmux 会把它当按键,这条没有送进去。".into())
        );
    }

    #[test]
    fn attachment_only_message_is_submitted_and_a_bare_empty_one_is_not() {
        let live = binding(None);
        assert_eq!(
            plan_inbound(Some(&live), Err(RemoteMessageError::Empty), false, true),
            InboundPlan::Submit(Vec::new()),
            "只有附件的消息照样打进去,正文为空"
        );
        assert_eq!(
            plan_inbound(Some(&live), Err(RemoteMessageError::Empty), false, false),
            InboundPlan::Reply("消息是空的,没有送进去。".into())
        );
        // 附件不放宽别的拒绝理由。
        assert!(matches!(
            plan_inbound(Some(&live), Err(RemoteMessageError::Backslash), false, true),
            InboundPlan::Reply(ref t) if t.contains("反斜杠")
        ));
        assert!(matches!(
            plan_inbound(Some(&live), Err(RemoteMessageError::ControlChar), false, true),
            InboundPlan::Reply(ref t) if t.contains("控制字符")
        ));
    }

    #[test]
    fn slash_command_is_refused_with_the_reason() {
        assert_eq!(
            plan_inbound(Some(&binding(None)), Err(RemoteMessageError::SlashCommand), true, false),
            InboundPlan::Reply(
                "遥控中不转发斜杠命令(像 /quit 这样的命令会结束 cmux 里的会话)。要用斜杠命令请到终端里操作。"
                    .into()
            )
        );
    }

    #[test]
    fn start_refusal_is_answered_from_the_reloaded_record() {
        assert_eq!(
            plan_start_refused(&Ok(Some(binding(Some("会话进程已经退出".into()))))),
            StartRefused::EndedNotice(
                "遥控已结束:会话进程已经退出。要继续请重新 `/bridge`。".into()
            )
        );
        assert_eq!(
            plan_start_refused(&Ok(None)),
            StartRefused::Reply("遥控已解除,这条没有处理。")
        );
        assert_eq!(
            plan_start_refused(&Ok(Some(binding(None)))),
            StartRefused::Reply("遥控还没准备好,稍后再发。")
        );
        assert_eq!(
            plan_start_refused(&Err(anyhow::anyhow!("坏记录"))),
            StartRefused::Reply("遥控还没准备好,稍后再发。")
        );
    }

    #[test]
    fn ended_binding_replies_once_and_unbinds() {
        // 结束的绑定只给一次 EndedNotice;dispatcher 发完即删,下一条按未绑定处理。
        let ended = binding(Some("x".into()));
        assert!(matches!(
            plan_inbound(Some(&ended), Ok(lines(&["a"])), true, false),
            InboundPlan::EndedNotice(_)
        ));
        assert_eq!(
            plan_inbound(None, Ok(lines(&["a"])), true, false),
            InboundPlan::NotBound
        );
    }

    #[test]
    fn ended_binding_takes_precedence_over_reset_and_bad_messages() {
        // 已结束的绑定不论来的是什么都只告知一次:`/reset` 也不必再鉴权,记录本来就要删。
        let ended = binding(Some("飞书身份已变化".into()));
        for message in [
            Ok(RemoteMessage::Reset),
            Err(RemoteMessageError::Empty),
            Err(RemoteMessageError::Backslash),
        ] {
            assert_eq!(
                plan_inbound(Some(&ended), message, false, false),
                InboundPlan::EndedNotice(
                    "遥控已结束:飞书身份已变化。要继续请重新 `/bridge`。".into()
                )
            );
        }
    }

    #[test]
    fn remote_bound_reads_the_agents_record_and_fails_closed() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let store = super::super::store::RemoteStore::for_agent(&agent);
        assert!(!remote_bound(&agent, "oc_group"));

        store.save(&binding(None)).unwrap();
        assert!(remote_bound(&agent, "oc_group"));

        // 已结束但还没在群里告知的记录照样算:告知并删除之前,本地会话不能抢先起来。
        store
            .save(&binding(Some("会话进程已经退出".into())))
            .unwrap();
        assert!(remote_bound(&agent, "oc_group"));

        store.remove("oc_group").unwrap();
        assert!(!remote_bound(&agent, "oc_group"));

        std::fs::write(agent.state_dir().join("remote").join("oc_group.json"), "{").unwrap();
        assert!(
            remote_bound(&agent, "oc_group"),
            "读不出记录时宁可不起本地会话"
        );
    }

    /// 已遥控的群不能从任何本地入口起会话。只测 `remote_bound` 本身不够:哪个入口漏接一行,
    /// 别的测试都不会变红,只在真机上表现为遥控中的群又冒出一个 tmux 会话。
    #[test]
    fn remote_bound_chat_blocks_local_paths() {
        let source = include_str!("../runtime/daemon/dispatcher.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("dispatcher 生产源");
        let slice = |from: &str, to: &str| {
            let start = production
                .find(from)
                .unwrap_or_else(|| panic!("找不到 {from}"));
            let end = production[start..]
                .find(to)
                .unwrap_or_else(|| panic!("{from} 之后找不到 {to}"));
            &production[start..start + end]
        };
        let guard = "crate::remote::inbound::remote_bound(";

        let boot = slice("fn boot_chat_allowed_by_bridge(", "\n}\n");
        assert!(boot.contains(guard), "启动续跑 / 镜像预热要跳过遥控中的群");

        let replacement = slice("deliver_task_with_one_replacement(", "match delivery {");
        let respawn = replacement
            .find("spawn_chat_thread(")
            .expect("replacement 重建点");
        assert!(
            replacement[..respawn].contains(guard),
            "chat worker 重建之前要再查一次遥控绑定"
        );

        for (arm, end, creation) in [
            (
                "Ok(DispatchMsg::ScheduledFire {",
                "Ok(DispatchMsg::ScheduledAdmitted {",
                "chats.entry(",
            ),
            (
                "Ok(DispatchMsg::SentinelFire {",
                "Ok(DispatchMsg::RecallMessage {",
                "chats.entry(",
            ),
            (
                "Ok(DispatchMsg::OrphanedTasks {",
                "Ok(DispatchMsg::ChatGone {",
                "spawn_chat_thread(",
            ),
        ] {
            let body = slice(arm, end);
            let entry = body.find(creation).expect("建 ChatHandle 的位置");
            assert!(
                body[..entry].contains(guard),
                "{arm} 要在建 ChatHandle 之前跳过遥控中的群"
            );
        }

        let inbound = slice("Ok(DispatchMsg::Inbound(msg)) => {", "chats.entry(");
        assert!(
            inbound.contains("crate::remote::store::RemoteStore::for_agent(")
                && inbound.contains("handle_remote_inbound("),
            "群入站要在建 ChatHandle 之前查遥控绑定并交给遥控处理"
        );

        // 群消息按需拉起 worker 只走先复验、从当前末尾回传的那条路;直接 `start` 会从落盘
        // offset 补发没经过复验的轮。
        let on_demand = slice("fn handle_remote_inbound(", "\n}\n");
        assert!(on_demand.contains("runtime::start_on_demand("));
        assert!(
            !on_demand.contains("runtime::start("),
            "群消息路径不能绕过复验直接起 worker"
        );
    }
}
