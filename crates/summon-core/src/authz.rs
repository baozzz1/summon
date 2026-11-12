//! 卡片回调 / 定时器的**归属与鉴权判定**(纯件:无 IO、无网络、无进程)。
//!
//! why:恢复卡(recover / dismiss_recovery)是 dispatcher 中央闸门刻意豁免的两个动作,判定内联在
//! daemon handler 里,一条测试都没有——删掉整段鉴权,全量门禁照样绿。定时器取消更早一步:全局
//! `schedules.yaml` 混装所有 agent × 所有 chat 的条目,而卡片回调只带 id,压根没有归属判定。
//! 判定下沉到这里之后 IO 侧只剩一行调用,鉴权矩阵由本文件的表驱动测试钉死。
//!
//! 不变量:这些函数只允许**收紧**。operate-gate 的语义(operators 优先、空回退 allowed_users、
//! 皆空时群里只放行飞书应用所有者)仍由 `Agent::can_operate` 单点持有,本模块只组合、绝不复制。

use crate::config::registry::{Agent, OperateScope};
use crate::schedule::Schedule;

/// 飞书 chat 是否群(含话题群)。判错方向即 fail-open:`can_operate` 在名单皆空时返回 `!is_group`。
pub fn is_group_chat(chat_type: &str) -> bool {
    matches!(chat_type, "group" | "topic_group")
}

/// 飞书用户身份对。open_id 按 app 隔离、union_id 同开发者下跨 app 稳定,故「是不是同一个人」
/// 要两条都比。**空串不算命中**——union_id 取自联系人 API,失败时是空串,若按相等判定会把
/// 「两边都查不到 union」误判成同一人。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeishuUser<'a> {
    pub open_id: &'a str,
    pub union_id: &'a str,
}

impl<'a> FeishuUser<'a> {
    pub fn new(open_id: &'a str, union_id: &'a str) -> Self {
        Self { open_id, union_id }
    }

    /// 是否同一人(任一非空 id 相等)。
    pub fn same_person(&self, other: &FeishuUser<'_>) -> bool {
        (!self.open_id.is_empty() && self.open_id == other.open_id)
            || (!self.union_id.is_empty() && self.union_id == other.union_id)
    }
}

/// 本轮发起人身份。判「能不能动写类工具」要三样东西:
/// - `open_id` / `union_id`:两条 id 任一命中名单即算(同 `Agent::can_operate` 语义)。
///   **两条都要给** —— 只给 open_id 会让「名单里只配了 union_id」的 owner 被判成外部人,
///   而配了 open_id 的真外部人反而全权,策略整体反转(2026-08-05 盘上实证)。
/// - `is_peer`:发起人是不是本 chat 已 `/introduce` 的成员(bot 或同事)。peer 由 operate-gate
///   保护的 `/introduce` 显式认下,在这个群里算内部人 —— 认下它的人就是在授权它在本群让 agent
///   干活。它通常不在 operators 名单里,不单独标记就会被整体降成只读,认识关系当场作废。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnRequester<'a> {
    pub open_id: &'a str,
    pub union_id: &'a str,
    pub is_peer: bool,
}

impl<'a> TurnRequester<'a> {
    pub fn new(open_id: &'a str, union_id: &'a str, is_peer: bool) -> Self {
        Self {
            open_id,
            union_id,
            is_peer,
        }
    }

    /// 身份完全缺失(两条 id 都空)。定时任务轮、以及 owner 手工 attach 打字都长这样。
    fn is_anonymous(&self) -> bool {
        self.open_id.is_empty() && self.union_id.is_empty()
    }
}

/// 本轮发起人是否「外部」——即在执行权限上要降成只读。
///
/// **prompt 的外部标签与 PreToolUse 执行闸必须共用这一个函数**:标了「外部」却能写、或能写却
/// 没标,都会让 agent 判断失据;两处各写一份判据就是漂移的开始(H1 正是漂移的首例)。
///
/// 判据顺序:身份缺失 → 不外部(fail-open,见 `tool_guard_verdict` 文档);peer → 不外部;
/// 其余交给 `Agent::can_operate`(operators 优先、空回退 allowed_users、皆空则群里只放行飞书应用
/// 所有者;本轮所在 chat 的本群名单另行放行 —— 读写权限只作用于这一轮,效果落在这个 chat 上)。
pub fn requester_is_external(
    requester: TurnRequester<'_>,
    agent: &Agent,
    is_group: bool,
    chat_id: &str,
) -> bool {
    if requester.is_anonymous() || requester.is_peer {
        return false;
    }
    !agent.can_operate(
        requester.open_id,
        requester.union_id,
        is_group,
        OperateScope::Chat(chat_id),
    )
}

/// PreToolUse 执行闸的裁决。`Deny` 里的字符串会被原样交给模型当拒绝原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    Allow,
    Deny(String),
}

/// 纯判定:本轮发起人能不能动写类工具。
///
/// **为什么 fail-open**(`requester = None` 或身份全空即放行):current-turn 是 daemon 在轮次
/// 开始时原子写的,外部人的轮次一定带这条记录;而 owner `summon agents attach` 手工打字时没有
/// 记录,fail-closed 会把 owner 自己锁在门外。攻击面只是「daemon 写 current-turn 失败的那一瞬」,
/// 代价是「owner 日常操作全被拦」—— 两者不对等。
///
/// 拒绝文案里**不放 open_id**:`skills/feishu-agent/SKILL.md` 明确 agent 不该见到裸 open_id,
/// 而 hook 进程不联网、也解析不出名字,所以只用泛称。
pub fn tool_guard_verdict(
    requester: Option<TurnRequester<'_>>,
    agent: &Agent,
    is_group: bool,
    chat_id: &str,
) -> GuardVerdict {
    let Some(requester) = requester else {
        return GuardVerdict::Allow;
    };
    if !requester_is_external(requester, agent, is_group, chat_id) {
        return GuardVerdict::Allow;
    }
    GuardVerdict::Deny(summon_i18n::text!("rd9ff400710").to_string())
}

/// 恢复卡的两个动作。二者鉴权**不对称**,这不是疏漏:
/// - `Recover` 重注入的是 WAL 里那条**原始入站**(点击者提供不了任何正文),且重注入会回到
///   dispatcher 再过一遍 talk 白名单 → 群里放宽到「原发送者本人也可以自助恢复」。
/// - `Dismiss` 是不可逆地销毁整批待办、且没有第二道白名单兜底 → 群里只认 operate 名单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    Recover,
    Dismiss,
}

/// 恢复卡鉴权。`pending_senders` = 本次动作要处理的那些入站项的发送者(单条恢复传长度 1,
/// 「全部忽略」传整批仍处于待恢复态的项)。
///
/// 私聊:只认「全部由点击者本人发出」(私聊里对端只有本人,别的都是异常)。
/// 群聊:按 `RecoveryAction` 分档,见其文档。
///
/// 空 `pending_senders`(整批已被处理完)判为「本人」= true:此时后续 claim/dismiss 本身是
/// no-op,放行不产生任何越权效果,与旧实现同语义。
pub fn recovery_card_authorized(
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    operator: FeishuUser<'_>,
    pending_senders: &[FeishuUser<'_>],
    action: RecoveryAction,
) -> bool {
    let all_own = pending_senders.iter().all(|s| operator.same_person(s));
    if !is_group_chat(chat_type) {
        return all_own;
    }
    let can_operate = agent.can_operate(
        operator.open_id,
        operator.union_id,
        true,
        OperateScope::Chat(chat_id),
    );
    match action {
        RecoveryAction::Recover => all_own || can_operate,
        RecoveryAction::Dismiss => can_operate,
    }
}

/// 定时器归属:一条定时器属于**建它的那个 (agent, chat)**。
/// why:`~/.summon/schedules.yaml` 是全局单文件,卡片回调只带 `id` —— 没有这道校验,A 群的
/// operator 能停掉 B agent 私聊里的定时器(operate-gate 只答「这个人能不能操作」,答不了
/// 「这条东西归不归他管」)。
pub fn schedule_belongs_to(schedule: &Schedule, agent_name: &str, chat_id: &str) -> bool {
    schedule.agent == agent_name && schedule.chat_id == chat_id
}

/// 本 (agent, chat) 名下的定时器子集。列表卡与取消后的刷新都过它,保证「看得见的才点得动、
/// 点得动的才看得见」。定时器是人手排的、量级几十条,克隆代价可忽略,换渲染侧零改动。
pub fn schedules_owned_by(all: &[Schedule], agent_name: &str, chat_id: &str) -> Vec<Schedule> {
    all.iter()
        .filter(|s| schedule_belongs_to(s, agent_name, chat_id))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "oc_chat";

    fn agent_with(feishu_yaml: &str) -> Agent {
        serde_yaml::from_str(feishu_yaml).expect("测试 agent.yaml 应可解析")
    }

    /// operators 非空:群里只认 operators;operators 空 + allowed_users 空 = allow-all → 群里 fail-closed
    /// (未填应用所有者)。
    fn agent_with_operators() -> Agent {
        agent_with("name: a\nfeishu:\n  app_id_env: A\n  operators:\n    - ou_op\n")
    }

    fn agent_open() -> Agent {
        agent_with("name: a\nfeishu:\n  app_id_env: A\n")
    }

    #[test]
    fn recovery_gate_matrix_covers_group_and_p2p() {
        let sender = FeishuUser::new("ou_sender", "on_sender");
        let senders = [sender];
        let with_ops = agent_with_operators();
        let open = agent_open();

        // (agent, chat_type, operator, action, 期望)
        let cases: &[(&Agent, &str, FeishuUser<'_>, RecoveryAction, bool, &str)] = &[
            // —— 群 + operators 非空 ——
            (
                &with_ops,
                "group",
                sender,
                RecoveryAction::Recover,
                true,
                "群里原发送者可自助恢复自己的消息(重注入还要再过 talk 白名单)",
            ),
            (
                &with_ops,
                "group",
                FeishuUser::new("ou_op", ""),
                RecoveryAction::Recover,
                true,
                "群里 operator 可恢复他人消息",
            ),
            (
                &with_ops,
                "group",
                FeishuUser::new("ou_stranger", ""),
                RecoveryAction::Recover,
                false,
                "群里陌生人不得恢复他人消息",
            ),
            (
                &with_ops,
                "group",
                sender,
                RecoveryAction::Dismiss,
                false,
                "群里「全部忽略」不可逆,原发送者本人也不够——只认 operate 名单",
            ),
            (
                &with_ops,
                "group",
                FeishuUser::new("ou_op", ""),
                RecoveryAction::Dismiss,
                true,
                "群里 operator 可忽略整批",
            ),
            // —— 群 + 名单皆空(allow-all)——
            (
                &open,
                "group",
                FeishuUser::new("ou_stranger", ""),
                RecoveryAction::Recover,
                false,
                "名单皆空时群里 fail-closed,陌生人不得恢复",
            ),
            (
                &open,
                "group",
                sender,
                RecoveryAction::Dismiss,
                false,
                "名单皆空时群里 fail-closed,原发送者也不得整批忽略",
            ),
            (
                &open,
                "topic_group",
                FeishuUser::new("ou_stranger", ""),
                RecoveryAction::Dismiss,
                false,
                "话题群与普通群同档,不得被当成私聊放行",
            ),
            // —— 私聊 ——
            (
                &with_ops,
                "p2p",
                sender,
                RecoveryAction::Recover,
                true,
                "私聊里本人恢复自己的消息",
            ),
            (
                &with_ops,
                "p2p",
                FeishuUser::new("ou_op", ""),
                RecoveryAction::Recover,
                false,
                "私聊里非本人一律拒(哪怕是 operator——别人的私聊不该被翻出来重放)",
            ),
            (
                &open,
                "p2p",
                sender,
                RecoveryAction::Dismiss,
                true,
                "私聊里本人可忽略自己的待恢复消息",
            ),
            (
                &open,
                "p2p",
                FeishuUser::new("ou_stranger", ""),
                RecoveryAction::Dismiss,
                false,
                "私聊里非本人不得忽略",
            ),
        ];

        for (agent, chat_type, operator, action, expect, why) in cases {
            assert_eq!(
                recovery_card_authorized(agent, chat_type, CHAT, *operator, &senders, *action),
                *expect,
                "{why}(chat_type={chat_type} action={action:?} operator={operator:?})"
            );
        }
    }

    #[test]
    fn recovery_gate_matches_sender_by_union_id_and_rejects_empty_ids() {
        let with_ops = agent_with_operators();
        // 同一人换 app:open_id 对不上,union_id 命中 → 仍算本人。
        let sender = [FeishuUser::new("ou_appA", "on_stable")];
        assert!(recovery_card_authorized(
            &with_ops,
            "p2p",
            CHAT,
            FeishuUser::new("ou_appB", "on_stable"),
            &sender,
            RecoveryAction::Recover,
        ));
        // 两边 union 都查不到(空串)且 open_id 不同 → 不是同一人,不得因「空==空」放行。
        let anon = [FeishuUser::new("ou_someone", "")];
        assert!(!recovery_card_authorized(
            &with_ops,
            "p2p",
            CHAT,
            FeishuUser::new("ou_other", ""),
            &anon,
            RecoveryAction::Recover,
        ));
        // 连 open_id 都空(异常回调)→ 拒,不得与空 sender 撞成同一人。
        let blank = [FeishuUser::new("", "")];
        assert!(!recovery_card_authorized(
            &with_ops,
            "p2p",
            CHAT,
            FeishuUser::new("", ""),
            &blank,
            RecoveryAction::Recover,
        ));
    }

    #[test]
    fn dismiss_requires_every_pending_item_to_be_own_in_p2p() {
        let with_ops = agent_with_operators();
        let me = FeishuUser::new("ou_me", "");
        // 整批里混进别人的消息 → 私聊里不得整批忽略(`all` 而非 `any`)。
        let mixed = [
            FeishuUser::new("ou_me", ""),
            FeishuUser::new("ou_other", ""),
        ];
        assert!(!recovery_card_authorized(
            &with_ops,
            "p2p",
            CHAT,
            me,
            &mixed,
            RecoveryAction::Dismiss
        ));
        // 全是自己的 → 放行。
        let mine = [FeishuUser::new("ou_me", ""), FeishuUser::new("ou_me", "")];
        assert!(recovery_card_authorized(
            &with_ops,
            "p2p",
            CHAT,
            me,
            &mine,
            RecoveryAction::Dismiss
        ));
        // 空批(已处理完)→ 放行,但后续动作本身是 no-op,不构成越权。
        assert!(recovery_card_authorized(
            &with_ops,
            "p2p",
            CHAT,
            me,
            &[],
            RecoveryAction::Dismiss
        ));
    }

    /// H1 回归:名单里只配 union_id(owner 是 `on_0a1b2c3d…` 这种形态,
    /// 同列还有一位真外部人的 open_id)。union_id 传空会让判定整体反转:owner 被判外部、
    /// 被闸只读,而配了 open_id 的外部人反而全权。
    #[test]
    fn union_id_only_owner_is_internal_while_open_id_listed_outsider_is_not_privileged() {
        let agent = agent_with(
            "name: a\nfeishu:\n  app_id_env: A\n  allowed_users:\n    - on_owner_union\n",
        );
        // owner:本 app 的 open_id 不在名单,union_id 在 → 必须全权、不标外部。
        let owner = TurnRequester::new("ou_owner_in_this_app", "on_owner_union", false);
        assert!(!requester_is_external(owner, &agent, true, CHAT));
        assert_eq!(
            tool_guard_verdict(Some(owner), &agent, true, CHAT),
            GuardVerdict::Allow
        );
        // 传空 union_id(修复前的形态)会把同一个 owner 判成外部——这条断言就是 H1 的病历。
        let owner_without_union = TurnRequester::new("ou_owner_in_this_app", "", false);
        assert!(requester_is_external(
            owner_without_union,
            &agent,
            true,
            CHAT
        ));
        // 名单里没有的人:两条 id 都给全也仍是外部。
        let outsider = TurnRequester::new("ou_outsider", "on_outsider", false);
        assert!(requester_is_external(outsider, &agent, true, CHAT));
    }

    /// 本群 operator 只在自己那个群里是「内部」:到了别的群,同一个人照样按外部人降成只读。
    #[test]
    fn chat_operator_is_internal_only_in_their_own_chat() {
        let agent = agent_with(
            "name: a\nfeishu:\n  app_id_env: A\n  operators: [ou_owner]\n  chat_operators:\n    oc_shared: [ou_colleague]\n",
        );
        let colleague = TurnRequester::new("ou_colleague", "", false);
        assert!(!requester_is_external(colleague, &agent, true, "oc_shared"));
        assert_eq!(
            tool_guard_verdict(Some(colleague), &agent, true, "oc_shared"),
            GuardVerdict::Allow
        );
        assert!(requester_is_external(
            colleague,
            &agent,
            true,
            "oc_elsewhere"
        ));
        assert!(matches!(
            tool_guard_verdict(Some(colleague), &agent, true, "oc_elsewhere"),
            GuardVerdict::Deny(_)
        ));
    }

    #[test]
    fn chat_operator_can_dismiss_recovery_only_in_their_own_chat() {
        let agent = agent_with(
            "name: a\nfeishu:\n  app_id_env: A\n  operators: [ou_owner]\n  chat_operators:\n    oc_shared: [ou_colleague]\n",
        );
        let colleague = FeishuUser::new("ou_colleague", "");
        let senders = [FeishuUser::new("ou_someone", "")];
        assert!(recovery_card_authorized(
            &agent,
            "group",
            "oc_shared",
            colleague,
            &senders,
            RecoveryAction::Dismiss,
        ));
        assert!(!recovery_card_authorized(
            &agent,
            "group",
            "oc_elsewhere",
            colleague,
            &senders,
            RecoveryAction::Dismiss,
        ));
    }

    /// M6 回归:peer 经 operate-gate 保护的 `/introduce` 显式认下(bot 或同事),通常不在
    /// operators 名单里。不单独放行的话认识关系会被整体降成只读。
    #[test]
    fn introduced_peer_bot_counts_as_internal() {
        let agent = agent_with_operators();
        let peer = TurnRequester::new("ou_peer_bot", "", true);
        assert!(!requester_is_external(peer, &agent, true, CHAT));
        assert_eq!(
            tool_guard_verdict(Some(peer), &agent, true, CHAT),
            GuardVerdict::Allow
        );
        // 同一个 open_id 不带 peer 标记 → 仍是外部(能力来自 introduce 关系,不是 id 本身)。
        let same_id_not_peer = TurnRequester::new("ou_peer_bot", "", false);
        assert!(requester_is_external(same_id_not_peer, &agent, true, CHAT));
    }

    #[test]
    fn missing_or_anonymous_requester_fails_open() {
        let agent = agent_with_operators();
        // 无 current-turn:owner 手工 attach 打字的合法路径。
        assert_eq!(
            tool_guard_verdict(None, &agent, true, CHAT),
            GuardVerdict::Allow
        );
        // 定时任务轮:两条 id 都空,视为 owner 预设。
        let scheduled = TurnRequester::new("", "", false);
        assert!(!requester_is_external(scheduled, &agent, true, CHAT));
        assert_eq!(
            tool_guard_verdict(Some(scheduled), &agent, true, CHAT),
            GuardVerdict::Allow
        );
    }

    /// L8 回归:拒绝文案原样交给模型,必须说清「该做什么」,且**不得泄露裸 open_id**
    /// (`skills/feishu-agent/SKILL.md` 声明 agent 不该见到它)。
    #[test]
    fn deny_reason_is_actionable_and_leaks_no_open_id() {
        let agent = agent_with_operators();
        let outsider = TurnRequester::new("ou_outsider_9f3", "on_outsider_9f3", false);
        let GuardVerdict::Deny(reason) = tool_guard_verdict(Some(outsider), &agent, true, CHAT)
        else {
            panic!("外部发起人必须被拒");
        };
        assert!(reason.contains("只读"), "{reason}");
        assert!(reason.contains("owner"), "{reason}");
        assert!(
            !reason.contains("ou_outsider_9f3"),
            "文案泄露了 open_id: {reason}"
        );
        assert!(
            !reason.contains("on_outsider_9f3"),
            "文案泄露了 union_id: {reason}"
        );
    }

    /// 名单皆空时原样继承 `can_operate`:私聊放行、群里只放行应用所有者,guard 不另立规则。
    #[test]
    fn guard_inherits_can_operate_when_lists_are_empty() {
        let agent = agent_open();
        let anyone = TurnRequester::new("ou_any", "", false);
        assert_eq!(
            tool_guard_verdict(Some(anyone), &agent, false, CHAT),
            GuardVerdict::Allow
        );
        assert!(matches!(
            tool_guard_verdict(Some(anyone), &agent, true, CHAT),
            GuardVerdict::Deny(_)
        ));
        // 群/私聊分叉只能来自 chat_type:飞书私聊的 chat_id 同样是 `oc_` 开头,按前缀猜会误拒 owner。
        assert!(matches!(
            tool_guard_verdict(Some(anyone), &agent, is_group_chat("group"), CHAT),
            GuardVerdict::Deny(_)
        ));
        assert_eq!(
            tool_guard_verdict(Some(anyone), &agent, is_group_chat("p2p"), CHAT),
            GuardVerdict::Allow
        );
        // 应用所有者在皆空群里不是外部人;名单任一非空时它不获得隐含权限。
        let owner = TurnRequester::new("ou_app_owner", "on_app_owner", false);
        assert!(requester_is_external(owner, &agent, true, CHAT));
        agent
            .app_owner
            .set(crate::config::registry::AppOwnerState::Known(Some(
                crate::config::registry::AppOwner {
                    open_id: "ou_app_owner".into(),
                    union_id: "on_app_owner".into(),
                },
            )));
        assert!(!requester_is_external(owner, &agent, true, CHAT));
        assert_eq!(
            tool_guard_verdict(Some(owner), &agent, true, CHAT),
            GuardVerdict::Allow
        );
        assert!(requester_is_external(anyone, &agent, true, CHAT));
        let listed = agent_with_operators();
        listed.app_owner.set(agent.app_owner.get());
        assert!(requester_is_external(owner, &listed, true, CHAT));
    }

    fn schedule(id: &str, agent: &str, chat_id: &str) -> Schedule {
        Schedule {
            id: id.to_string(),
            kind: crate::schedule::Kind::Interval,
            spec: "3600".to_string(),
            agent: agent.to_string(),
            chat_id: chat_id.to_string(),
            chat_type: "group".to_string(),
            prompt: "p".to_string(),
            created_by: "ou_owner".to_string(),
            created_at: "2026-07-27 10:00:00".to_string(),
            output: crate::schedule::ScheduleOutput::Progress,
            context: crate::schedule::ScheduleContext::Resume,
            enabled: true,
            last_fired: None,
            revision: 0,
        }
    }

    #[test]
    fn schedule_ownership_isolates_agent_and_chat() {
        let mine = schedule("SCH-1", "starlight", "oc_a");
        let other_agent = schedule("SCH-2", "coder", "oc_a");
        let other_chat = schedule("SCH-3", "starlight", "oc_b");

        // 同 agent 同 chat → 归我。
        assert!(schedule_belongs_to(&mine, "starlight", "oc_a"));
        // 跨 agent:同一个群里另一个 bot 排的定时器,不归我。
        assert!(!schedule_belongs_to(&other_agent, "starlight", "oc_a"));
        // 跨 chat:同一个 agent 在别的会话排的定时器,不归本会话。
        assert!(!schedule_belongs_to(&other_chat, "starlight", "oc_a"));

        let all = [mine.clone(), other_agent, other_chat];
        assert_eq!(
            schedules_owned_by(&all, "starlight", "oc_a"),
            vec![mine],
            "列表/刷新只应看到本 (agent, chat) 名下的定时器"
        );
        assert!(schedules_owned_by(&all, "starlight", "oc_never_seen").is_empty());
    }
}
