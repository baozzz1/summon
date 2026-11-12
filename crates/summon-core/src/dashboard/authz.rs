//! 控制台的修改判权(纯)。规则见 spec「权限」一节,一句话:
//!
//! - **签发 agent 自己**:直接调 [`Agent::can_operate`],按签发 chat 的私聊 / 群语义判 —— 与
//!   `/term` 在同一个 chat 上的判法逐条相同。效果只落在签发 chat 上的(这个群的按群配置)用
//!   [`OperateScope::Chat`],其余(agent 级设置、名单、说明文件、定时任务、别的群的按群配置)
//!   用 [`OperateScope::Agent`]。
//! - **别的 agent、整机**:只认明确授权 —— 在它的操作名单里(操作名单为空时看对话名单),或它
//!   两份名单都为空且此人是它的飞书应用所有者;整机要求对**每一个** agent(含停用的)都满足。
//!   名单为空时「私聊人人可操作」那条放行在这里不算数:它的前提是「这个 bot 的私聊」。
//! - **跨 agent 认人只用 union_id**:点击回调给的 open_id 只在签发 agent 的应用里有效。
//!
//! 输入全部由调用方从盘上现读的 agent.yaml 与 daemon 查到的应用所有者构造([`AgentLists`]),
//! 本模块不碰文件。

use crate::config::registry::{
    Agent, AppOwnerSlot, AppOwnerState, Feishu, OperateDenial, OperateScope,
};
use std::collections::HashMap;

/// 一次修改的效果落在哪。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// 整机设置与全局说明:效果落在全部 agent 上。
    Machine,
    /// 某个 agent 的设置、名单、说明文件、定时任务。
    Agent(&'a str),
    /// 某个 agent 在某个 chat 上的按群配置。
    Chat { agent: &'a str, chat_id: &'a str },
}

/// 谁在改。身份只来自 control.sock 的本机 owner,或飞书卡片回调里的点击人。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Owner,
    Feishu {
        issuer_agent: String,
        issuer_chat_id: String,
        issuer_is_group: bool,
        open_id: String,
        union_id: String,
    },
}

/// 一个 agent 判权要用到的全部事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLists {
    pub alias: String,
    pub enabled: bool,
    /// agent.yaml 有没有 `feishu:` 段(没有时名单为空也不放行应用所有者)。
    pub has_feishu: bool,
    pub allowed_users: Vec<String>,
    pub operators: Vec<String>,
    pub operators_error: Option<String>,
    pub chat_operators: HashMap<String, Vec<String>>,
    pub app_owner: AppOwnerState,
}

impl AgentLists {
    /// 从现读的 agent.yaml 构造。`enabled` 是 availability 的启停状态(只做展示;整机判权
    /// 不看它,见 [`decide`])。
    pub fn from_agent(agent: &Agent, enabled: bool) -> Self {
        let feishu = agent.feishu.as_ref();
        let operators = agent.effective_operators();
        AgentLists {
            alias: agent.name.clone(),
            enabled,
            has_feishu: feishu.is_some(),
            allowed_users: feishu.map(|f| f.allowed_users.clone()).unwrap_or_default(),
            operators: operators.clone().unwrap_or_default(),
            operators_error: operators.err(),
            chat_operators: feishu.map(|f| f.chat_operators.clone()).unwrap_or_default(),
            app_owner: agent.app_owner.get(),
        }
    }

    /// 换上指定的应用所有者状态。刚从新文本解析出来的 [`Agent`] 还没有查过所有者,
    /// 算「改完之后」的名单时沿用改之前那份。
    pub fn with_app_owner(mut self, app_owner: AppOwnerState) -> Self {
        self.app_owner = app_owner;
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub agents: Vec<AgentLists>,
}

impl Snapshot {
    fn agent(&self, alias: &str) -> Option<&AgentLists> {
        self.agents.iter().find(|a| a.alias == alias)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// 拒绝原因:缺哪一层权限、可以补哪个字段。
    Deny(String),
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// 能不能改落在 `target` 上的设置。
pub fn decide(principal: &Principal, target: &Target<'_>, snapshot: &Snapshot) -> Decision {
    let Principal::Feishu {
        issuer_agent,
        issuer_chat_id,
        issuer_is_group,
        open_id,
        union_id,
    } = principal
    else {
        return Decision::Allow;
    };
    let Some(issuer) = snapshot.agent(issuer_agent) else {
        return Decision::Deny(summon_i18n::format!("r3ee45edb6a"));
    };
    let user = User {
        open_id,
        union_id,
        is_group: *issuer_is_group,
    };
    match *target {
        Target::Machine => machine(issuer, user, snapshot),
        Target::Agent(alias) if alias == issuer_agent.as_str() => {
            issuer_gate(issuer, user, OperateScope::Agent)
        }
        Target::Chat { agent, chat_id } if agent == issuer_agent.as_str() => {
            // 只有签发 chat 上的按群配置算本群范围;别的群的效果不落在签发 chat 上。
            let scope = if chat_id == issuer_chat_id.as_str() {
                OperateScope::Chat(chat_id)
            } else {
                OperateScope::Agent
            };
            issuer_gate(issuer, user, scope)
        }
        Target::Agent(alias) | Target::Chat { agent: alias, .. } => match snapshot.agent(alias) {
            None => Decision::Deny(summon_i18n::format!("r487205d295")),
            // 别的 agent 的应用里,这个 open_id 不是同一个人:只拿 union_id 去比。
            Some(other) => match explicitly_authorized(other, "", union_id) {
                Ok(()) => Decision::Allow,
                Err(why) => Decision::Deny(why),
            },
        },
    }
}

/// 会话还能不能用:签发 agent 在签发 chat 上的 operate-gate(与签发、点击时同一道闸)。
pub fn may_enter(principal: &Principal, snapshot: &Snapshot) -> Decision {
    let Principal::Feishu {
        issuer_agent,
        issuer_chat_id,
        issuer_is_group,
        open_id,
        union_id,
    } = principal
    else {
        return Decision::Allow;
    };
    match snapshot.agent(issuer_agent) {
        None => Decision::Deny(summon_i18n::format!("r3ee45edb6a")),
        Some(issuer) => issuer_gate(
            issuer,
            User {
                open_id,
                union_id,
                is_group: *issuer_is_group,
            },
            OperateScope::Chat(issuer_chat_id),
        ),
    }
}

/// 飞书身份把签发 agent 的名单从 `before` 改成 `after`,会不会让自己失去操作权限
/// (签发 chat 上的,或 agent 级的)。本机 owner 不受此限。
pub fn would_lock_out(principal: &Principal, before: &AgentLists, after: &AgentLists) -> bool {
    let Principal::Feishu {
        issuer_agent,
        issuer_chat_id,
        issuer_is_group,
        open_id,
        union_id,
    } = principal
    else {
        return false;
    };
    // 进入只看签发 agent;改别的 agent 的名单锁不住自己。
    if before.alias != *issuer_agent {
        return false;
    }
    let allowed = |lists: &AgentLists, scope: OperateScope<'_>| {
        gate_agent(lists)
            .is_some_and(|agent| agent.can_operate(open_id, union_id, *issuer_is_group, scope))
    };
    [OperateScope::Chat(issuer_chat_id), OperateScope::Agent]
        .into_iter()
        .any(|scope| allowed(before, scope) && !allowed(after, scope))
}

#[derive(Clone, Copy)]
struct User<'a> {
    open_id: &'a str,
    union_id: &'a str,
    is_group: bool,
}

/// 签发 agent 自己:就是 operate-gate。
fn issuer_gate(lists: &AgentLists, user: User<'_>, scope: OperateScope<'_>) -> Decision {
    let Some(agent) = gate_agent(lists) else {
        return Decision::Deny(summon_i18n::text!("r13a70f4609").to_string());
    };
    let Some(denial) = agent.operate_denial(user.open_id, user.union_id, user.is_group, scope)
    else {
        return Decision::Allow;
    };
    let alias = &lists.alias;
    let chat_hint = match scope {
        OperateScope::Chat(_) => summon_i18n::text!("r5b55fe8c2c"),
        OperateScope::Agent => "",
    };
    let chat_hint_you = match scope {
        OperateScope::Chat(_) => summon_i18n::text!("rb30b8ff649"),
        OperateScope::Agent => "",
    };
    Decision::Deny(match denial {
        OperateDenial::NoListConfigured => summon_i18n::format!("r8fc007aa01"),
        OperateDenial::NotListed(list) => match scope {
            OperateScope::Agent if in_chat_list(lists, user) => {
                summon_i18n::format!("r3a1e3979df", list.field())
            }
            _ => summon_i18n::format!("re07396e613", list.field()),
        },
    })
}

fn in_chat_list(lists: &AgentLists, user: User<'_>) -> bool {
    lists.chat_operators.values().any(|ids| {
        ids.iter()
            .any(|id| same_id(id, user.open_id) || same_id(id, user.union_id))
    })
}

fn same_id(listed: &str, id: &str) -> bool {
    !id.is_empty() && listed == id
}

/// 整机:对每一个 agent(含停用的)都有明确授权。停用的也算:整机设置对它同样生效(随时可能被
/// 重新启用),而停用开关本身只要 agent 级权限就能拨 —— 不算它,被判权的人就能先停用一个没授权给
/// 他的 agent 再改整机。
fn machine(issuer: &AgentLists, user: User<'_>, snapshot: &Snapshot) -> Decision {
    let mut missing = Vec::new();
    for agent in &snapshot.agents {
        // 签发 agent 的 open_id 是同一个应用里的,可以用;别的 agent 只认 union_id。
        let open_id = if agent.alias == issuer.alias {
            user.open_id
        } else {
            ""
        };
        if let Err(why) = explicitly_authorized(agent, open_id, user.union_id) {
            missing.push(why);
        }
    }
    if snapshot.agents.is_empty() {
        return Decision::Deny(summon_i18n::text!("r853d20d979").to_string());
    }
    if missing.is_empty() {
        Decision::Allow
    } else {
        Decision::Deny(summon_i18n::format!("r4c413aea98", missing.join("；")))
    }
}

/// 明确授权:在操作名单里(为空时看对话名单),或两份名单都空且是飞书应用所有者。
/// 「名单皆空时私聊人人可操作」不在此列。
pub fn explicitly_authorized(
    lists: &AgentLists,
    open_id: &str,
    union_id: &str,
) -> Result<(), String> {
    let alias = &lists.alias;
    if let Some(error) = &lists.operators_error {
        return Err(error.clone());
    }
    let (list, field) = if !lists.operators.is_empty() {
        (&lists.operators, "operators")
    } else {
        (&lists.allowed_users, "allowed_users")
    };
    if !list.is_empty() {
        if list
            .iter()
            .any(|id| same_id(id, open_id) || same_id(id, union_id))
        {
            return Ok(());
        }
        if union_id.is_empty() {
            return Err(summon_i18n::format!("r5093e0e0e0"));
        }
        if list.iter().all(|id| id.starts_with("ou_")) {
            return Err(summon_i18n::format!("r891526fca2"));
        }
        return Err(summon_i18n::format!("rdcecda942e"));
    }
    let owner = match &lists.app_owner {
        AppOwnerState::Known(Some(owner)) if lists.has_feishu => Some(owner),
        _ => None,
    };
    if owner.is_some_and(|o| same_id(&o.open_id, open_id) || same_id(&o.union_id, union_id)) {
        return Ok(());
    }
    if union_id.is_empty() {
        return Err(summon_i18n::format!("r08a7a6660d"));
    }
    Err(summon_i18n::format!("r872a48258e"))
}

/// 判权用的最小 [`Agent`]:只带名单与应用所有者,拿它调 `can_operate`,语义与运行时逐条相同。
fn gate_agent(lists: &AgentLists) -> Option<Agent> {
    if lists.operators_error.is_some() {
        return None;
    }
    let name = serde_yaml::Mapping::from_iter([(
        serde_yaml::Value::from("name"),
        serde_yaml::Value::from(lists.alias.as_str()),
    )]);
    let mut agent: Agent = serde_yaml::from_value(serde_yaml::Value::Mapping(name)).ok()?;
    if lists.has_feishu {
        agent.feishu = Some(Feishu {
            app_id_env: None,
            app_secret_env: None,
            scope: String::new(),
            allowed_users: lists.allowed_users.clone(),
            operators: Some(lists.operators.clone()),
            chat_operators: lists.chat_operators.clone(),
            chat_reply_without_mention: HashMap::new(),
        });
    }
    // 新开一个槽:`AppOwnerSlot` 克隆后共享同一份,写进去会改到 daemon 正在用的那个。
    let slot = AppOwnerSlot::default();
    slot.set(lists.app_owner.clone());
    agent.app_owner = slot;
    Some(agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::registry::AppOwner;

    const ME_OPEN: &str = "ou_me";
    const ME_UNION: &str = "on_me";
    const GROUP: &str = "oc_group";
    const P2P: &str = "oc_p2p";

    #[derive(Clone, Copy, Debug)]
    enum L {
        /// 操作名单里有我(union_id)。
        In,
        /// 操作名单里没有我。
        Out,
        /// 两份名单都空,应用所有者未知。
        Empty,
        /// 两份名单都空,我是应用所有者。
        EmptyOwner,
        /// 名单里只有我的 open_id。
        OpenOnly,
        /// 只在签发群的本群操作名单里。
        ChatOnly,
    }

    fn lists(alias: &str, enabled: bool, l: L) -> AgentLists {
        let mut a = AgentLists {
            alias: alias.to_string(),
            enabled,
            has_feishu: true,
            allowed_users: Vec::new(),
            operators: Vec::new(),
            operators_error: None,
            chat_operators: HashMap::new(),
            app_owner: AppOwnerState::Unknown,
        };
        match l {
            L::In => a.operators = vec![ME_UNION.into()],
            L::Out => a.operators = vec!["on_else".into()],
            L::Empty => {}
            L::EmptyOwner => {
                a.app_owner = AppOwnerState::Known(Some(AppOwner {
                    open_id: format!("ou_owner_of_{alias}"),
                    union_id: ME_UNION.into(),
                }))
            }
            L::OpenOnly => a.operators = vec![ME_OPEN.into()],
            L::ChatOnly => {
                a.operators = vec!["on_else".into()];
                a.chat_operators.insert(GROUP.into(), vec![ME_UNION.into()]);
            }
        }
        a
    }

    fn feishu(is_group: bool) -> Principal {
        Principal::Feishu {
            issuer_agent: "issuer".into(),
            issuer_chat_id: if is_group { GROUP } else { P2P }.into(),
            issuer_is_group: is_group,
            open_id: ME_OPEN.into(),
            union_id: ME_UNION.into(),
        }
    }

    fn snapshot(issuer: L, other: L) -> Snapshot {
        Snapshot {
            agents: vec![
                lists("issuer", true, issuer),
                lists("other", true, other),
                // 停用的 agent 也在快照里(整机判权照样算它),默认给它明确授权,不影响下面各行。
                lists("off", false, L::In),
            ],
        }
    }

    #[test]
    fn decision_matrix() {
        use Target::*;
        let own_chat = |g: bool| Chat {
            agent: "issuer",
            chat_id: if g { GROUP } else { P2P },
        };
        let other_chat = Chat {
            agent: "issuer",
            chat_id: "oc_elsewhere",
        };
        // (群签发?, 签发 agent 名单, 别的 agent 名单, 目标, 期望)
        let rows: Vec<(bool, L, L, Target, bool)> = vec![
            // 签发 agent 自己,agent 范围:私聊名单皆空人人可操作,群里不行。
            (false, L::In, L::Out, Agent("issuer"), true),
            (true, L::In, L::Out, Agent("issuer"), true),
            (false, L::Out, L::Out, Agent("issuer"), false),
            (false, L::Empty, L::Out, Agent("issuer"), true),
            (true, L::Empty, L::Out, Agent("issuer"), false),
            (true, L::EmptyOwner, L::Out, Agent("issuer"), false),
            (false, L::OpenOnly, L::Out, Agent("issuer"), true),
            // 签发 chat 上的按群配置:本群范围,应用所有者与本群名单另行放行。
            (true, L::EmptyOwner, L::Out, own_chat(true), true),
            (true, L::Empty, L::Out, own_chat(true), false),
            (true, L::ChatOnly, L::Out, own_chat(true), true),
            (false, L::Empty, L::Out, own_chat(false), true),
            // 别的群的按群配置:agent 范围。
            (true, L::ChatOnly, L::Out, other_chat, false),
            (true, L::ChatOnly, L::Out, Agent("issuer"), false),
            (true, L::In, L::Out, other_chat, true),
            // 别的 agent:只认明确授权,只认 union_id。
            (false, L::Out, L::In, Agent("other"), true),
            (
                true,
                L::Out,
                L::In,
                Chat {
                    agent: "other",
                    chat_id: GROUP,
                },
                true,
            ),
            (false, L::In, L::Out, Agent("other"), false),
            (false, L::In, L::Empty, Agent("other"), false),
            (true, L::In, L::Empty, Agent("other"), false),
            (false, L::In, L::EmptyOwner, Agent("other"), true),
            (false, L::In, L::OpenOnly, Agent("other"), false),
            (
                true,
                L::In,
                L::ChatOnly,
                Chat {
                    agent: "other",
                    chat_id: GROUP,
                },
                false,
            ),
            (false, L::In, L::In, Agent("missing"), false),
            // 整机:每个 agent(含停用的)都要明确授权。
            (false, L::In, L::In, Machine, true),
            (true, L::In, L::EmptyOwner, Machine, true),
            (false, L::In, L::Out, Machine, false),
            (false, L::Empty, L::In, Machine, false),
            (false, L::OpenOnly, L::In, Machine, true),
            // 签发人的 open_id 只在签发 agent 的应用里有效:别的 agent 名单里只有它时不算授权。
            (false, L::In, L::OpenOnly, Machine, false),
            (false, L::EmptyOwner, L::EmptyOwner, Machine, true),
        ];
        for (i, (group, issuer, other, target, allowed)) in rows.into_iter().enumerate() {
            let got = decide(&feishu(group), &target, &snapshot(issuer, other));
            assert_eq!(
                got.is_allowed(),
                allowed,
                "第 {i} 行:群={group} 签发={issuer:?} 别的={other:?} 目标={target:?} → {got:?}"
            );
            if let Decision::Deny(reason) = got {
                assert!(!reason.is_empty(), "第 {i} 行拒绝要说明原因");
            }
        }
    }

    #[test]
    fn owner_is_always_allowed() {
        let empty = Snapshot::default();
        for target in [
            Target::Machine,
            Target::Agent("x"),
            Target::Chat {
                agent: "x",
                chat_id: GROUP,
            },
        ] {
            assert_eq!(decide(&Principal::Owner, &target, &empty), Decision::Allow);
        }
        assert_eq!(may_enter(&Principal::Owner, &empty), Decision::Allow);
        assert!(!would_lock_out(
            &Principal::Owner,
            &lists("issuer", true, L::In),
            &lists("issuer", true, L::Out)
        ));
    }

    #[test]
    fn chat_operator_only_can_touch_that_chat() {
        let snap = snapshot(L::ChatOnly, L::In);
        let me = feishu(true);
        assert!(
            may_enter(&me, &snap).is_allowed(),
            "本群操作名单里的人从这个群进得来"
        );
        let this_chat = Target::Chat {
            agent: "issuer",
            chat_id: GROUP,
        };
        assert!(decide(&me, &this_chat, &snap).is_allowed());
        assert!(!decide(&me, &Target::Agent("issuer"), &snap).is_allowed());
        let elsewhere = Target::Chat {
            agent: "issuer",
            chat_id: "oc_other",
        };
        assert!(!decide(&me, &elsewhere, &snap).is_allowed());
        assert!(!decide(&me, &Target::Machine, &snap).is_allowed());
    }

    #[test]
    fn machine_requires_every_agent() {
        let me = feishu(false);
        let mut snap = snapshot(L::In, L::In);
        assert!(decide(&me, &Target::Machine, &snap).is_allowed());
        snap.agents.push(lists("third", true, L::Out));
        let Decision::Deny(reason) = decide(&me, &Target::Machine, &snap) else {
            panic!("有一个 agent 没授权就不能改整机");
        };
        assert!(reason.contains("third"), "{reason}");
        // 签发 agent 不在(被删了):整机也拒,不因为「没有别的 agent」而放行。
        let lonely = Snapshot {
            agents: vec![lists("other", true, L::In)],
        };
        assert!(!decide(&me, &Target::Machine, &lonely).is_allowed());
    }

    /// 停用开关只要签发 agent 的 agent 级权限就能拨:按启用与否算整机,等于让被判权的人自己
    /// 先停用一个没授权给他的 agent、再改整机。所以停用的 agent 照样要有明确授权。
    #[test]
    fn disabled_agent_still_counts_for_machine() {
        let me = feishu(true);
        let mut snap = snapshot(L::In, L::In);
        snap.agents.retain(|a| a.alias != "off");
        snap.agents.push(lists("off", false, L::Out));
        let Decision::Deny(reason) = decide(&me, &Target::Machine, &snap) else {
            panic!("停用的 agent 没授权,整机也不能改");
        };
        assert!(reason.contains("off"), "{reason}");
        assert!(!decide(&me, &Target::Agent("off"), &snap).is_allowed());
        snap.agents.retain(|a| a.alias != "off");
        snap.agents.push(lists("off", false, L::In));
        assert!(decide(&me, &Target::Machine, &snap).is_allowed());
    }

    #[test]
    fn empty_lists_private_open_does_not_grant_cross_agent() {
        let me = feishu(false);
        let snap = snapshot(L::Empty, L::Empty);
        assert!(
            decide(&me, &Target::Agent("issuer"), &snap).is_allowed(),
            "签发 agent 的私聊照旧"
        );
        assert!(!decide(&me, &Target::Agent("other"), &snap).is_allowed());
        assert!(!decide(&me, &Target::Machine, &snap).is_allowed());
    }

    #[test]
    fn missing_union_id_is_read_only_across_agents() {
        let snap = snapshot(L::In, L::In);
        let Principal::Feishu {
            issuer_agent,
            issuer_chat_id,
            issuer_is_group,
            open_id,
            ..
        } = feishu(false)
        else {
            unreachable!()
        };
        let no_union = Principal::Feishu {
            issuer_agent,
            issuer_chat_id,
            issuer_is_group,
            open_id,
            union_id: String::new(),
        };
        let mut snap_open = snap.clone();
        snap_open.agents[0].operators = vec![ME_OPEN.into()];
        assert!(decide(&no_union, &Target::Agent("issuer"), &snap_open).is_allowed());
        let Decision::Deny(reason) = decide(&no_union, &Target::Agent("other"), &snap) else {
            panic!("拿不到 union_id 不能跨 agent 改");
        };
        assert!(reason.contains("union_id"), "{reason}");
        let mut open_only = snap.clone();
        open_only.agents[1].operators = vec![ME_OPEN.into()];
        let Decision::Deny(reason) = decide(&feishu(false), &Target::Agent("other"), &open_only)
        else {
            panic!("对方名单里只有 open_id 时判为无权");
        };
        assert!(reason.contains("union_id"), "{reason}");
    }

    #[test]
    fn removing_self_from_operators_is_lockout() {
        let me = feishu(true);
        let before = lists("issuer", true, L::In);
        let mut after = before.clone();
        after.operators = vec!["on_else".into()];
        assert!(would_lock_out(&me, &before, &after));
        after.operators = vec![ME_UNION.into(), "on_else".into()];
        assert!(!would_lock_out(&me, &before, &after));
        // 操作名单清空、回退到不含我的对话名单。
        after.operators.clear();
        after.allowed_users = vec!["on_else".into()];
        assert!(would_lock_out(&me, &before, &after));
        // 只靠本群名单进来的人,把自己从本群名单里删掉。
        let before = lists("issuer", true, L::ChatOnly);
        let mut after = before.clone();
        after.chat_operators.clear();
        assert!(would_lock_out(&me, &before, &after));
        // 改的是别的 agent 的名单:不算锁死(进入只看签发 agent)。
        let other_before = lists("other", true, L::In);
        let mut other_after = other_before.clone();
        other_after.operators.clear();
        assert!(!would_lock_out(&me, &other_before, &other_after));
        // 本来就没有权限的,谈不上锁死。
        let out = lists("issuer", true, L::Out);
        assert!(!would_lock_out(&me, &out, &out));
    }

    /// 签发 agent 自己的判定就是 `Agent::can_operate`:同一组名单下逐格比对。
    #[test]
    fn issuer_decision_matches_can_operate() {
        let variants = [
            L::In,
            L::Out,
            L::Empty,
            L::EmptyOwner,
            L::OpenOnly,
            L::ChatOnly,
        ];
        for l in variants {
            for group in [false, true] {
                let snap = snapshot(l, L::Out);
                let agent = gate_agent(&snap.agents[0]).expect("能构造判权用的 agent");
                let chat = if group { GROUP } else { P2P };
                let me = feishu(group);
                let pairs = [
                    (Target::Agent("issuer"), OperateScope::Agent),
                    (
                        Target::Chat {
                            agent: "issuer",
                            chat_id: chat,
                        },
                        OperateScope::Chat(chat),
                    ),
                ];
                for (target, scope) in pairs {
                    let expect = agent.can_operate(ME_OPEN, ME_UNION, group, scope);
                    assert_eq!(
                        decide(&me, &target, &snap).is_allowed(),
                        expect,
                        "{l:?} 群={group} {target:?}"
                    );
                }
                assert_eq!(
                    may_enter(&me, &snap).is_allowed(),
                    agent.can_operate(ME_OPEN, ME_UNION, group, OperateScope::Chat(chat))
                );
            }
        }
        // 从 agent.yaml 构造出来的名单与手写的等价。
        let yaml = "name: issuer\nfeishu:\n  app_id_env: A\n  operators: [on_me]\n  \
                    chat_operators: {oc_group: [on_x]}\n";
        let agent: Agent = serde_yaml::from_str(yaml).unwrap();
        let built = AgentLists::from_agent(&agent, true);
        assert_eq!(built.operators, [ME_UNION]);
        assert_eq!(built.chat_operators[GROUP], ["on_x"]);
        assert!(built.has_feishu);
    }
}
