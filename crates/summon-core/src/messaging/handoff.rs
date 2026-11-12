//! `/handoff` 的纯逻辑:命令解析、写交接那一轮的指令、接手那一轮的第一条消息。
//!
//! 流程由 daemon 编排:当前 agent 按 [`TEMPLATE`] 写交接文件,回复末尾用交接标签报出路径
//! (见 [`crate::feed_tag::handoff_path`]);summon 校验文件存在后按 [`HandoffAfter`] 收尾 ——
//! 带目标时把 [`followup`] 投给本群另一个 agent;不带目标时,有手动压缩通道的交互会话原地压缩
//! 上下文,其余把 [`followup`] 投给当前 agent 的新会话。

use crate::config::registry::{Agent, CaptureMode};
use crate::driver::{CliDriver, CompactSpec};
use crate::ipc::{mention_key_starts, render_mentions, Mention};

/// 命令名。只认这个 exact 拼写;同形变体由 `slash_pool::plan_slash` 收口拒绝。
pub const HANDOFF_COMMAND: &str = "/handoff";

/// 交接文件的模板。写交接那一轮的指令与 `summon guide handoff` 都列这几项。
pub const TEMPLATE: &[&str] = &[
    "rfbada32235",
    "r72de6cf7aa",
    "r12143278c0",
    "r96b5df4e23",
    "r34bc3ffca6",
    "rfae040ee1d",
];

/// 接手方没收到备注时要做的事。
const DEFAULT_NEXT: &str = "rfc23a772e8";

/// 解析出的 `/handoff [@目标] [备注]`。
#[derive(Debug, Clone, PartialEq)]
pub struct HandoffCommand {
    /// 紧跟命令名的那个 @;没有就是交给当前 agent 的新会话。它是不是本群的 summon agent 由
    /// daemon 校验,这里只认位置。
    pub target: Option<Mention>,
    /// 其余正文,@ 渲染成 `@名字`。
    pub note: String,
}

/// 把一条消息解析成 `/handoff`:开头 @ 本 bot 的那个 mention(若有)之后,首 token 恰好是
/// [`HANDOFF_COMMAND`]。`self_open_id` 是本 bot 的 open_id,私聊或取不到时传 `None`。
pub fn handoff_command(
    text: &str,
    mentions: &[Mention],
    self_open_id: Option<&str>,
) -> Option<HandoffCommand> {
    let mut rest = text.trim_start();
    if let Some(me) = self_open_id.filter(|me| !me.is_empty()) {
        if let Some(own) = mentions
            .iter()
            .find(|m| !m.key.is_empty() && m.open_id == me && mention_key_starts(rest, &m.key))
        {
            rest = rest[own.key.len()..].trim_start();
        }
    }
    let after = rest.strip_prefix(HANDOFF_COMMAND)?;
    if !(after.is_empty() || after.starts_with(char::is_whitespace)) {
        return None;
    }
    let mut after = after.trim_start();
    let target = mentions
        .iter()
        .find(|m| !m.key.is_empty() && mention_key_starts(after, &m.key))
        .cloned();
    if let Some(target) = &target {
        after = &after[target.key.len()..];
    }
    // 与普通对话的正文同一种渲染:有 @ 时整段归一化,私聊里没有 @ 时换行原样保留。
    let note = if mentions.is_empty() {
        after.trim().to_string()
    } else {
        render_mentions(after, mentions)
    };
    Some(HandoffCommand { target, note })
}

/// 交接文件写好之后谁来接手。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffAfter<'a> {
    /// 本群另一个 agent(显示名):接手消息投给它,当前会话退出。
    Peer(&'a str),
    /// 压缩上下文之后的自己:当前会话原地压缩,不拆会话、不投接手消息。
    CompactInPlace,
    /// 当前 agent 的新会话:当前会话归档,接手消息投给新会话。
    NewSession,
}

/// 不带目标的 `/handoff` 是否原地压缩:只有 driver 声明了交互式压缩命令的会话才有这条通道,
/// 其余开新会话。
pub fn compacts_in_place(driver: CliDriver, capture: CaptureMode) -> bool {
    matches!(driver.compact_spec(capture), CompactSpec::Tmux { .. })
}

/// 按 agent 当前的 CLI 与捕获方式判 [`compacts_in_place`]。写交接的指令、轮末收尾与提醒卡入口
/// 都从 agent 推导,不各自拼判据。
pub fn agent_compacts_in_place(agent: &Agent) -> bool {
    compacts_in_place(CliDriver::from_kind(agent.cli_kind()), agent.capture_mode())
}

/// 写交接那一轮发给当前 agent 的指令。
pub fn instruction(after: HandoffAfter<'_>, note: &str) -> String {
    let receiver = match after {
        HandoffAfter::Peer(name) => summon_i18n::format!("rf426fbdeac"),
        HandoffAfter::CompactInPlace => summon_i18n::text!("rffe9aa1445").to_string(),
        HandoffAfter::NewSession => summon_i18n::text!("ra22ebe3bfe").to_string(),
    };
    let handover = match after {
        HandoffAfter::CompactInPlace => summon_i18n::format!(
            "r3fc40b284a",
            followup(summon_i18n::text!("r13a74772bf"), note, None)
        ),
        HandoffAfter::Peer(_) | HandoffAfter::NewSession => summon_i18n::format!(
            "rfa6a5edf70",
            followup(summon_i18n::text!("r13a74772bf"), note, None)
        ),
    };
    let after = match after {
        HandoffAfter::Peer(name) => {
            summon_i18n::format!("r60eab53496")
        }
        HandoffAfter::CompactInPlace => summon_i18n::text!("r85d9817533").to_string(),
        HandoffAfter::NewSession => summon_i18n::text!("r8d0da16b5e").to_string(),
    };
    let template: String = TEMPLATE
        .iter()
        .map(|item| format!("   - {}\n", crate::i18n::label(item)))
        .collect();
    summon_i18n::format!("r8aafc879f3")
}

/// 接手那一轮的第一条消息。`from` 是交出工作的 agent 的显示名(交给别的 agent 时才有)。
pub fn followup(path: &str, note: &str, from: Option<&str>) -> String {
    let note = note.trim();
    let next = if note.is_empty() {
        ::summon_core::i18n::label(DEFAULT_NEXT)
    } else {
        note
    };
    match from {
        Some(from) => summon_i18n::format!("rc0af438a48"),
        None => summon_i18n::format!("r958eebd4ca"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mention(key: &str, open_id: &str, name: &str, kind: &str) -> Mention {
        Mention {
            open_id: open_id.into(),
            key: key.into(),
            name: name.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn the_mention_right_after_the_command_is_the_target() {
        let mentions = vec![
            mention("@_user_1", "ou_me", "Arcadia", "bot"),
            mention("@_user_2", "ou_b", "星光", "bot"),
            mention("@_user_3", "ou_z", "张三", "user"),
        ];
        let cmd = handoff_command(
            "@_user_1 /handoff @_user_2 合完找 @_user_3 确认",
            &mentions,
            Some("ou_me"),
        )
        .expect("带目标");
        assert_eq!(
            cmd.target.as_ref().map(|t| t.open_id.as_str()),
            Some("ou_b")
        );
        assert_eq!(cmd.note, "合完找 @张三 确认");

        let cmd = handoff_command(
            "@_user_1 /handoff 合完找 @_user_3 确认",
            &mentions,
            Some("ou_me"),
        )
        .expect("不带目标");
        assert_eq!(cmd.target, None, "@ 不在命令名之后就只是备注里提到的人");
        assert_eq!(cmd.note, "合完找 @张三 确认");

        let cmd = handoff_command("@_user_1 /handoff", &mentions, Some("ou_me")).unwrap();
        assert_eq!(
            cmd,
            HandoffCommand {
                target: None,
                note: String::new()
            }
        );
    }

    #[test]
    fn a_private_note_keeps_its_line_breaks() {
        let cmd = handoff_command("/handoff 第一行\n第二行", &[], None).unwrap();
        assert_eq!(cmd.note, "第一行\n第二行");
    }

    #[test]
    fn only_the_exact_command_parses() {
        let mentions = vec![mention("@_user_1", "ou_me", "Arcadia", "bot")];
        for text in [
            "@_user_1 /handoffs",
            "@_user_1 /Handoff",
            "@_user_1 /handoff!",
            "@_user_1 说说 /handoff",
            "/handoff是什么",
        ] {
            assert_eq!(
                handoff_command(text, &mentions, Some("ou_me")),
                None,
                "{text}"
            );
        }
        // 开头那个 @ 不是本 bot 时,命令不是对本 bot 说的。
        assert_eq!(
            handoff_command("@_user_1 /handoff", &mentions, Some("ou_other")),
            None
        );
    }

    #[test]
    fn the_instruction_names_the_receiver_the_tag_and_the_template() {
        let to_peer = instruction(HandoffAfter::Peer("星光"), "把 MR 合了");
        assert!(to_peer.contains("交给本群的 星光"));
        assert!(to_peer.contains("「先读交接文件 <交接文件路径>,然后把 MR 合了」"));
        assert!(to_peer.contains("当前会话随后退出"));
        let to_self = instruction(HandoffAfter::NewSession, "");
        assert!(to_self.contains("交给你自己的新会话"));
        assert!(to_self.contains("归档当前会话"));
        assert!(to_self.contains(::summon_core::i18n::label(DEFAULT_NEXT)));
        let compact = instruction(HandoffAfter::CompactInPlace, "先修登录");
        assert!(compact.contains("交给压缩上下文之后的你自己"));
        assert!(compact.contains("在当前会话原地压缩上下文"));
        assert!(compact.contains("压缩后不会自动开工"));
        assert!(compact.contains("「先读交接文件 <交接文件路径>,然后先修登录」"));
        assert!(
            !compact.contains("归档") && !compact.contains("新会话"),
            "{compact}"
        );
        for text in [&to_peer, &to_self, &compact] {
            assert!(text.contains("<summon-handoff>交接文件的绝对路径</summon-handoff>"));
            assert!(text.contains("tasks/<日期-主题>/HANDOFF.md"));
            for item in TEMPLATE {
                let item = crate::i18n::label(item);
                assert!(text.contains(item), "{item}");
            }
        }
        assert!(
            to_peer.contains(crate::feed_tag::HANDOFF_TAG_OPEN)
                && to_peer.contains(crate::feed_tag::HANDOFF_TAG_CLOSE),
            "指令里的标签就是 summon 认的那个"
        );
    }

    #[test]
    fn only_sessions_with_an_interactive_compact_command_compact_in_place() {
        assert!(compacts_in_place(
            CliDriver::Claude,
            CaptureMode::Transcript
        ));
        for (driver, capture) in [
            (CliDriver::Codex, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::CliSession),
        ] {
            assert!(
                !compacts_in_place(driver, capture),
                "{driver:?} {capture:?}"
            );
        }
    }

    #[test]
    fn the_followup_says_who_handed_over_and_what_to_do_next() {
        assert_eq!(
            followup("/w/HANDOFF.md", "", None),
            "先读交接文件 /w/HANDOFF.md,然后按其中的「下一步」接着做"
        );
        assert_eq!(
            followup("/w/HANDOFF.md", " 把 MR 合了 ", Some("Arcadia")),
            "Arcadia 把手上的工作交接给你。先读交接文件 /w/HANDOFF.md,然后把 MR 合了"
        );
    }
}
