//! 工程状态表情(working/done)的**纯决策**(slice 3 地基)。
//!
//! 会话镜像把输出解耦成 pub-sub 后,「哪条飞书消息该贴 done」**不再能靠 `run_turn` 返回**判定(终端侧
//! turn 没有对应飞书消息;飞书侧可能同时多条在飞)。改用一个**文件化的 message_id→状态队列**(IO 仿
//! `inbox_wal` 的 pending/done 目录,在 `summon` 层):消息**真送达 claude** 即入 working(记 `delivered_at`),
//! 每当一个 assistant turn 完成,就按本模块的纯决策把对应飞书消息收尾成 done、清理。
//!
//! owner 决策(2026-06-05):done = 「消息真送达后的**首次完成**」;多条并发按 **FIFO** 归因(turn↔message
//! 本就模糊,FIFO 是可接受近似)。本模块只算「这次完成该把哪条 working 收成 done」,状态/落盘在 `summon`。

/// 一条待收尾的工程表情记录(IO 层从文件读出后喂进来判定)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReaction {
    /// 飞书消息 id(working/done 表情贴在它上面;也是文件队列的键)。
    pub message_id: String,
    /// 该消息**真正注入 claude** 的时刻(毫秒)。不是飞书收到那刻——消息可能排队。
    pub delivered_at_ms: i64,
}

/// 本次 assistant turn 完成(`completed_at_ms`)时,该收尾成 done 的那条 working 记录的 message_id:
/// **FIFO** = `delivered_at` 最早、且 `≤` 完成时刻(已送达)的一条。没有符合的(都晚于完成时刻 / 队列空)
/// → `None`(这次完成不对应任何在飞的飞书消息,如终端侧 turn)。纯函数,不改状态;调用方据返回值贴 done +
/// 从队列清理那条。
pub fn resolve_done(pending: &[PendingReaction], completed_at_ms: i64) -> Option<&str> {
    pending
        .iter()
        .filter(|p| p.delivered_at_ms <= completed_at_ms)
        .min_by_key(|p| p.delivered_at_ms)
        .map(|p| p.message_id.as_str())
}

/// `summon react` 的纯决策:agent 语义表情是**单值、可改可撤**(不叠加)。IO 层据返回执行
/// unreact 旧 / react 新 / 持久化。与工程 working/done 两条独立轨。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactPlan {
    /// 撤掉当前语义表情(若有),随后持久化「已撤」。
    Clear { unreact: Option<String> },
    /// 先撤旧(若有)再贴 `emoji`,随后持久化新 id。
    Set {
        unreact: Option<String>,
        emoji: String,
    },
    /// `emoji` 不在安全集 —— 拒绝、**不动任何表情**(回执列可选集)。
    Reject { emoji: String },
    /// `emoji` 与本轮在飞的工程「处理中」占位表情同名 —— 拒绝、**不动任何表情**。
    ///
    /// why:飞书对**同一 operator + 同一 emoji** 的重复贴是幂等合并(实测 2026-08-13:重复 POST
    /// 返回成功 + 既有 reaction_id,不产生新表情)。此时「贴语义表情」什么都不会出现,反而让语义轨
    /// 记下与工程轨相同的 reaction_id —— 后续任一轨撤销都会把对方的表情一并撤掉(真机事故:agent
    /// `summon react OnIt` 报成功但用户看不到任何新表情,轮末工程收尾又把它撤了个干净)。
    RejectWorkingCollision { emoji: String },
}

/// 据「当前已贴的 agent 语义表情 id / 请求贴的表情名 / `--clear` / 可用安全集 / 本轮在飞的工程
/// 占位表情」决定动作。`current` 无 = None;`requested` 没给 = None;`allowed` = 该 agent **解析后**
/// 的 `messaging.reactions`(热重载,与喂给 agent 的 `{emoji_file}` 同源);`working_in_flight` =
/// 工程轨已成功贴在该消息上的 working 表情名(无在飞 OnIt = None,此时不构成撞轨)。
///
/// 不变量:① 清除优先(`--clear` 或没给表情都撤掉当前);② **先校验后撤旧** —— 越界/撞轨表情走
/// Reject 族、绝不先撤旧,手滑/typo 不丢已贴反应。
pub fn plan_agent_react(
    current: Option<&str>,
    requested: Option<&str>,
    clear: bool,
    allowed: &[String],
    working_in_flight: Option<&str>,
) -> ReactPlan {
    // 清除优先:`--clear` 或没给表情 → 撤掉当前(若有)。
    let Some(emoji) = requested.filter(|_| !clear) else {
        return ReactPlan::Clear {
            unreact: current.map(str::to_string),
        };
    };
    // 先校验后撤:越界即 Reject,旧表情保住。
    if !allowed.iter().any(|r| r == emoji) {
        return ReactPlan::Reject {
            emoji: emoji.to_string(),
        };
    }
    // 与在飞的工程占位表情同名 → 拒绝(见 `RejectWorkingCollision` 的 why)。同样不撤旧。
    if working_in_flight == Some(emoji) {
        return ReactPlan::RejectWorkingCollision {
            emoji: emoji.to_string(),
        };
    }
    ReactPlan::Set {
        unreact: current.map(str::to_string),
        emoji: emoji.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: &str, t: i64) -> PendingReaction {
        PendingReaction {
            message_id: id.into(),
            delivered_at_ms: t,
        }
    }

    #[test]
    fn fifo_oldest_eligible_is_done() {
        let q = vec![p("om_b", 2000), p("om_a", 1000), p("om_c", 3000)];
        // 完成时刻 2500:a(1000)、b(2000) 都已送达,c(3000) 还没 → 取最早的 a。
        assert_eq!(resolve_done(&q, 2500), Some("om_a"));
    }

    #[test]
    fn skips_not_yet_delivered() {
        let q = vec![p("om_future", 5000)];
        // 完成早于送达(并发/时钟边界)→ 不对应它,None。
        assert_eq!(resolve_done(&q, 1000), None);
    }

    #[test]
    fn empty_queue_is_none() {
        // 终端侧 turn:队列空 → 没有飞书消息该 done。
        assert_eq!(resolve_done(&[], 9999), None);
    }

    fn allowed() -> Vec<String> {
        ["BUG", "DONE", "THUMBSUP"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn clear_flag_with_existing_unreacts_it() {
        assert_eq!(
            plan_agent_react(Some("r1"), None, true, &allowed(), None),
            ReactPlan::Clear {
                unreact: Some("r1".into())
            }
        );
    }

    #[test]
    fn clear_without_existing_is_empty_clear() {
        assert_eq!(
            plan_agent_react(None, None, true, &allowed(), None),
            ReactPlan::Clear { unreact: None }
        );
    }

    #[test]
    fn no_emoji_arg_is_clear_even_without_flag() {
        // 空入参 == 撤掉(与 --clear 等价,owner 决策)。
        assert_eq!(
            plan_agent_react(Some("r1"), None, false, &allowed(), None),
            ReactPlan::Clear {
                unreact: Some("r1".into())
            }
        );
    }

    #[test]
    fn valid_emoji_replaces_old() {
        assert_eq!(
            plan_agent_react(Some("r1"), Some("BUG"), false, &allowed(), None),
            ReactPlan::Set {
                unreact: Some("r1".into()),
                emoji: "BUG".into()
            }
        );
    }

    #[test]
    fn valid_emoji_with_no_old_just_sets() {
        assert_eq!(
            plan_agent_react(None, Some("DONE"), false, &allowed(), None),
            ReactPlan::Set {
                unreact: None,
                emoji: "DONE".into()
            }
        );
    }

    #[test]
    fn out_of_set_emoji_rejected_and_preserves_old() {
        // 关键不变量:越界/typo 走 Reject,**不撤旧** —— 旧语义表情保住,不因手滑丢反应。
        assert_eq!(
            plan_agent_react(Some("r1"), Some("NOPE"), false, &allowed(), None),
            ReactPlan::Reject {
                emoji: "NOPE".into()
            }
        );
    }

    #[test]
    fn working_collision_rejected_and_preserves_old() {
        // 根因回归(2026-08-13 真机):agent 对 bot 派活消息 `summon react OnIt`,而 OnIt 正是
        // 工程 working 占位表情且已在飞 —— 飞书幂等合并,重复贴返回**同一个 reaction_id**,
        // 语义轨与工程轨从此记同一个 id,轮末工程 unreact 把「语义表情」也撤没了,用户全程
        // 看不到任何新表情。纯决策必须在贴之前拒掉,且**不撤旧**语义表情。
        assert_eq!(
            plan_agent_react(Some("r1"), Some("DONE"), false, &allowed(), Some("DONE")),
            ReactPlan::RejectWorkingCollision {
                emoji: "DONE".into()
            }
        );
    }

    #[test]
    fn working_emoji_allowed_when_not_in_flight() {
        // 工程 OnIt 没挂在这条消息上(working_in_flight=None)→ 不构成撞轨,正常 Set。
        assert_eq!(
            plan_agent_react(None, Some("DONE"), false, &allowed(), Some("BUG")),
            ReactPlan::Set {
                unreact: None,
                emoji: "DONE".into()
            }
        );
    }

    #[test]
    fn out_of_set_beats_collision_check() {
        // 越界优先于撞轨:不在安全集的名字走 Reject(回执列可选集),即便它恰好等于 working。
        assert_eq!(
            plan_agent_react(None, Some("NOPE"), false, &allowed(), Some("NOPE")),
            ReactPlan::Reject {
                emoji: "NOPE".into()
            }
        );
    }
}
