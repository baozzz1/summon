//! pause+append(Item B)的纯逻辑:把「被打断那轮的原 prompt(+ 已累积部分回答)」与「打断后
//! debounce 窗口里抽干的若干条新消息」按 CLI 语义合并成重跑轮要喂的 prompt。
//!
//! 逐 CLI 差异(经验探针,见 `docs/plan/headless-turn-lifecycle.md` §0):
//! - **codex**:SIGINT 是 graceful interrupt,CLI 自己往 rollout 写 `<turn_aborted>` 并保留被打断的
//!   user prompt + 部分 assistant 输出;resume 时模型清楚「上一轮被主动打断」。⇒ 合并 prompt **只需**
//!   把新追加的消息拼起来,**不必**重述原 prompt / 部分回答(CLI 已有上下文)。
//! - **cursor**:`--resume` 只能续到**上一次干净完成**的状态,被打断那轮的 prompt + 部分输出被
//!   **丢弃**(CLI 不回放)。⇒ summon 必须**自己重喂**:把原 prompt(+ 已累积部分回答)连同新消息
//!   一起拼进重跑轮。
//!
//! 纯函数:输入字符串、输出字符串,零 IO。`#[cfg(test)]` 覆盖两 CLI × 有/无 partial × 单/多追加。

use crate::driver::CliDriver;

/// 被打断那一轮的快照 + 打断后要追加的新消息,供 [`merge_followup`] 合成重跑 prompt。
pub struct InterruptedTurn<'a> {
    /// 被打断那轮喂进 CLI 的 prompt(信封后的正文;cursor 重喂时用,codex 不用)。
    pub original_prompt: &'a str,
    /// 打断前已流式累积的部分回答(可能空);cursor 重喂时拼进去,codex 不用。
    pub partial_reply: &'a str,
    /// 打断后 debounce 窗口里抽干的新消息正文(按到达顺序,≥1 条)。
    pub appended: &'a [String],
}

impl InterruptedTurn<'_> {
    /// 把若干条追加消息按到达顺序拼成一段(多条时用换行分隔,单条即原文)。
    fn appended_joined(&self) -> String {
        self.appended.join("\n")
    }
}

/// 一条轮内到达的**普通**消息是否该 steer 进当前这一轮(#3 决策接缝,**默认关**、可 kill-switch)。
///
/// **两条注入路共用这一个判据**:claude/tmux(往 TUI 打字,`messaging.steer_transcript`)与
/// codex/app-server(发 `turn/steer` 帧,`messaging.steer_rpc`)。机制毫无共同点,但「什么样的
/// 消息**该**进当前轮」这个问题的答案逐字相同,所以判据只留一份、开关由调用方按路传入 ——
/// 抄第二份的唯一结果是两条路的归属规则迟早漂开。
///
/// 命中需三条**同时**成立:① 该路的 steer 开关已开 ② 确有在跑轮 ③ **与本轮发起人同一人**。
///
/// 第 ③ 条是归属闸:群里 A 的轮跑着时放 B 进来,B 能把 A 的轮改跑偏,而且 N 条消息合成的
/// 那一条回复该挂谁的卡、贴谁的表情都没有正确答案。跨人消息继续排队成独立轮。
///
/// (2026-08-20 之前还有第四条「不是 `!`」—— 那时 `!` 走强制打断路、优先级更高。强制打断
/// 整条删除后这一维连同它的理由一起失效,签名里也不再有这个参数。)
pub fn should_steer(steer_enabled: bool, turn_running: bool, same_requester: bool) -> bool {
    steer_enabled && turn_running && same_requester
}

/// 按 CLI 语义合成重跑轮(resume / 冷启)要喂的 prompt。
///
/// - codex / 未知:只用追加消息(codex 靠 `<turn_aborted>` 保留被打断上下文;其他 CLI 本不该进
///   pause+append,保守只发追加正文)。
/// - cursor:重喂「原 prompt(+ 已累积部分回答)+ 追加消息」,因为 cursor 丢弃被打断那轮。
pub fn merge_followup(cli: CliDriver, t: &InterruptedTurn) -> String {
    let appended = t.appended_joined();
    match cli {
        // codex:CLI 已保留 <turn_aborted> + 原 prompt + 部分输出,只需带一句说明 + 追加正文。
        CliDriver::Codex => {
            summon_i18n::format!("rbb0979ac5c")
        }
        // cursor:CLI 不保留被打断那轮,必须把原 prompt(+ partial)重喂回去。
        CliDriver::Cursor => {
            let partial_note = if t.partial_reply.trim().is_empty() {
                String::new()
            } else {
                summon_i18n::format!("rab2a964dea", t.partial_reply.trim())
            };
            summon_i18n::format!("r0c0d8b4082", t.original_prompt.trim())
        }
        // claude(Transcript):tmux Esc 打断后会话仍在、上下文(原 prompt + 部分输出)都在 claude
        // 记忆里,只需带说明 + 追加正文(同 codex 思路,不重喂原 prompt/partial)。
        CliDriver::Claude => {
            summon_i18n::format!("rbb0979ac5c")
        }
        // Other 无捕获路径。保守只发追加正文。
        _ => appended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn<'a>(orig: &'a str, partial: &'a str, appended: &'a [String]) -> InterruptedTurn<'a> {
        InterruptedTurn {
            original_prompt: orig,
            partial_reply: partial,
            appended,
        }
    }

    #[test]
    fn codex_omits_restate_only_appended() {
        let app = vec!["改成数到 100".to_string()];
        let s = merge_followup(CliDriver::Codex, &turn("数到 40", "1 2 3", &app));
        assert!(s.contains("改成数到 100"), "应含追加正文");
        assert!(
            !s.contains("数到 40"),
            "codex 不重述原 prompt(CLI 自留上下文)"
        );
        assert!(!s.contains("1 2 3"), "codex 不重喂部分回答");
    }

    #[test]
    fn cursor_restates_original_and_partial() {
        let app = vec!["改成数到 100".to_string()];
        let s = merge_followup(CliDriver::Cursor, &turn("数到 40", "1 2 3", &app));
        assert!(s.contains("数到 40"), "cursor 必须重喂原 prompt");
        assert!(s.contains("1 2 3"), "cursor 必须重喂已累积部分回答");
        assert!(s.contains("改成数到 100"), "应含追加正文");
    }

    #[test]
    fn cursor_omits_partial_when_empty() {
        let app = vec!["继续".to_string()];
        let s = merge_followup(CliDriver::Cursor, &turn("写函数", "", &app));
        assert!(s.contains("写函数"));
        assert!(
            !s.contains("你已经回答了一部分"),
            "无 partial 时不渲染部分回答段"
        );
        assert!(s.contains("继续"));
    }

    #[test]
    fn coalesces_multiple_appended_in_order() {
        let app = vec!["还有".to_string(), "顺便".to_string(), "另外".to_string()];
        let s = merge_followup(CliDriver::Cursor, &turn("活儿", "", &app));
        // 三条按到达顺序都在,且顺序正确(还有 在 顺便 前,顺便 在 另外 前)。
        let i_a = s.find("还有").unwrap();
        let i_b = s.find("顺便").unwrap();
        let i_c = s.find("另外").unwrap();
        assert!(i_a < i_b && i_b < i_c, "追加消息应按到达顺序合并");
        // codex 同样合并多条。
        let s2 = merge_followup(CliDriver::Codex, &turn("活儿", "", &app));
        assert!(s2.contains("还有") && s2.contains("顺便") && s2.contains("另外"));
    }

    #[test]
    fn claude_keeps_context_with_interrupt_note() {
        // claude/Transcript:tmux Esc 打断后会话仍在、上下文(原 prompt + 部分输出)在 claude 记忆里,
        // 故只带「被打断」说明 + 追加正文,不重述原 prompt / 部分回答(同 codex 思路)。
        let app = vec!["改成数到 100".to_string()];
        let s = merge_followup(CliDriver::Claude, &turn("数到 40", "1 2 3", &app));
        assert!(s.contains("改成数到 100"), "应含追加正文");
        assert!(
            s.contains("打断"),
            "带「被打断」说明,让 claude 知道上轮被中断"
        );
        assert!(!s.contains("数到 40"), "claude 保留上下文,不重述原 prompt");
        assert!(!s.contains("1 2 3"), "claude 不重喂部分回答");
    }

    #[test]
    fn steer_requires_switch_running_turn_and_same_requester() {
        // 三条全满足 → steer 进本轮。(`!` 那一维已随强制打断一起删,2026-08-20)
        assert!(should_steer(true, true, true));
        // kill-switch 关 → 回到 summon 自己排队的旧行为。
        assert!(
            !should_steer(false, true, true),
            "开关关时必须逐字节回到旧的串行排队"
        );
        // 没有在跑轮 → 普通新消息,正常起一轮。
        assert!(!should_steer(true, false, true));
        // 归属闸:别人的轮不许 steer 进去。否则 B 能把 A 的轮改跑偏,且合成的那条回复该挂谁的卡、
        // 贴谁的表情都没有正确答案 —— 这是归属问题,不是并发问题。
        assert!(
            !should_steer(true, true, false),
            "跨发起人不得 steer,排队成独立轮"
        );
    }

    #[test]
    fn other_only_appended() {
        // Other 无捕获路径,保守只发追加正文(claude 另有「被打断」说明,见上一条测试)。
        let app = vec!["补充".to_string()];
        assert_eq!(
            merge_followup(CliDriver::Other, &turn("x", "y", &app)),
            "补充"
        );
    }
}
