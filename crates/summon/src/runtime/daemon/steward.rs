//! 管家(steward)身份的入站处理:**daemon 内的一条同步路径**,收消息 → 读快照 → 回卡片。
//!
//! 与 sentinel 同族:它是 daemon 的子系统,不是 agent。
//!
//! **红线:这条路径永不进会话层。** 不起 CLI 会话、不碰终端会话层、不消耗任何模型额度 ——
//! 一旦它开始「起个会话让模型回答 fleet 状态」,成本、延迟、可预测性三样一起失去。
//! 文件底部有结构性测试自扫源码钉死这条(同 sentinel 的 `never_touches_chat_hot_path`)。
//!
//! 只读默认可用;破坏性动作(清理/重置某条会话)不在这条路径上 —— 那些走各自的确认卡。

use summon_core::fleet::FleetSnapshot;
use summon_core::registry::Agent;

use crate::frontend::client::FeishuClient;
use crate::frontend::outbound;

use super::log;

/// 组装当前 fleet 快照。**runtime 直接读自己内存**,不绕 control.sock 问自己。
/// **自己 load 配置**而不是从调用方穿一层:状态查询本来就该看最新配置(热重载),
/// 而 dispatcher 手上并没有现成的 `Config`。
fn snapshot() -> anyhow::Result<FleetSnapshot> {
    let cfg = summon_core::config::Config::load();
    let reg = summon_core::registry::Registry::load(&cfg.agents_dir)?;
    let now = std::time::SystemTime::now();
    let rt = crate::observe::runtime_status::snapshot(
        std::process::id(),
        Some(env!("CARGO_PKG_VERSION").to_string()),
        now,
    );
    Ok(crate::observe::fleet_snapshot::build(
        &cfg,
        &reg,
        Some(&rt),
        now,
    ))
}

/// 渲染一张 fleet 卡。`alias` 有值 = 下钻某个 agent;`remote` 有值 = 那个 agent 在远端机器上。
///
/// 下钻找不到该 agent 时**明说找不到**,而不是悄悄退回总览 —— 后者会让人以为自己看到的是
/// 那个 agent 的详情。
fn render(
    snap: &FleetSnapshot,
    alias: Option<&str>,
    remote: Option<&str>,
    remotes: &[crate::observe::remote_fleet::RemoteSnapshot],
) -> String {
    render_with_notice(snap, alias, remote, remotes, None)
}

/// [`render`] + 页首一行操作结果。待处理区只上总览:它是本机的 waiting,
/// 「标记已处理」也只清得了本机的标记;下钻页看的是某一个 agent,状态列里已经写着「待处理」。
fn render_with_notice(
    snap: &FleetSnapshot,
    alias: Option<&str>,
    remote: Option<&str>,
    remotes: &[crate::observe::remote_fleet::RemoteSnapshot],
    notice: Option<&str>,
) -> String {
    use summon_core::fleet_card as fc;
    // 下钻远端 agent:从那台机器最后一次拉到的快照里找。找不到(机器没数据/没这个 agent)
    // 与本机找不到走同一套「明说」的处置。
    if let (Some(a), Some(r)) = (alias, remote) {
        let found = remotes
            .iter()
            .find(|x| x.name == r)
            .and_then(|x| x.snapshot.as_ref())
            .and_then(|s| s.agents.iter().find(|x| x.alias == a));
        return match found {
            Some(agent) => summon_core::feishu::card_fleet(
                &summon_i18n::format!("rc06e57d427", fc::fleet_agent_markdown(agent)),
                true,
                // 远端 agent 暂不给「管理会话」——停会话要经目标机器的 operate 闸,
                // 那条链路还没接;给一个点了没用的入口比不给更糟。
                None,
                &[],
                &[],
                notice,
            ),
            None => summon_core::feishu::card_fleet(
                &summon_i18n::format!("r2352beb83d"),
                true,
                None,
                &drilldown_targets(snap, remotes),
                &[],
                notice,
            ),
        };
    }
    match alias {
        Some(a) => match snap.agents.iter().find(|x| x.alias == a) {
            Some(agent) => summon_core::feishu::card_fleet(
                &fc::fleet_agent_markdown(agent),
                true,
                Some(a),
                // 下钻页不再列全体按钮:这一层的下一步是「管理会话」或「返回」,
                // 再摆一排同级 agent 会把层级搅浑。
                &[],
                &[],
                notice,
            ),
            None => summon_core::feishu::card_fleet(
                &not_found_text(snap, a),
                true,
                // 找不到的 agent 不给「管理会话」入口:那一页同样会落空,不如不给。
                None,
                // 但要把可选的 agent 摆成按钮 —— 人打错名字时,能点比再打一遍强。
                &drilldown_targets(snap, remotes),
                &[],
                notice,
            ),
        },
        None => summon_core::feishu::card_fleet(
            &overview_with_remotes(snap, remotes),
            false,
            None,
            &drilldown_targets(snap, remotes),
            &snap.waiting,
            notice,
        ),
    }
}

/// 总览正文 = 本机表格 + 每台远端机器一段。
///
/// **远端单独成段而不是混进同一张表**:混在一起就必须给每行加一列「在哪台机器」,而绝大多数
/// 时候只有本机 —— 为一个偶尔出现的维度让常见情形永久变宽,不划算。分段还顺带解决了新鲜度:
/// 每段各自标注自己那份数据是什么时候拉的(本机是此刻,远端是 N 秒前)。
fn overview_with_remotes(
    snap: &FleetSnapshot,
    remotes: &[crate::observe::remote_fleet::RemoteSnapshot],
) -> String {
    use summon_core::fleet_card as fc;
    let mut out = fc::fleet_overview_markdown(snap);
    for r in remotes {
        out.push_str("\n\n");
        let age = r
            .age
            .map(|d| fc::freshness_text(d.as_secs()))
            .unwrap_or_default();
        out.push_str(&fc::fleet_remote_markdown(
            &r.name,
            r.snapshot.as_ref(),
            (!age.is_empty()).then_some(age.as_str()),
            r.error.as_deref(),
        ));
    }
    out
}

/// 下钻按钮的目标:先本机、后远端。顺序与正文段落一致 —— 眼睛能对上。
fn drilldown_targets(
    snap: &FleetSnapshot,
    remotes: &[crate::observe::remote_fleet::RemoteSnapshot],
) -> Vec<summon_core::feishu::DrilldownTarget> {
    use summon_core::feishu::DrilldownTarget;
    let mut out: Vec<DrilldownTarget> = snap
        .agents
        .iter()
        .map(|a| DrilldownTarget::local(&a.alias))
        .collect();
    for r in remotes {
        if !r.has_data() {
            continue;
        }
        let Some(s) = r.snapshot.as_ref() else {
            continue;
        };
        out.extend(
            s.agents
                .iter()
                .map(|a| DrilldownTarget::remote(&a.alias, &r.name)),
        );
    }
    out
}

/// 下钻找不到该 agent 时的文案:列出可选,别让人以为自己看到的是那个 agent 的详情。
fn not_found_text(snap: &FleetSnapshot, alias: &str) -> String {
    let names: Vec<&str> = snap.agents.iter().map(|x| x.alias.as_str()).collect();
    summon_i18n::format!("r802ce35965", names.join(" "))
}

/// 渲染某个 agent 的**会话操作页**(「管理会话」按钮的去处)。
///
/// 注意这里只是**渲染**:steward 依旧不碰会话层,页上按钮点下去走的是 dispatcher 的
/// 卡片动作路由(两道 operate 闸 + 既有的 `TeardownChat` 通道),与本文件无关。
fn render_sessions(snap: &FleetSnapshot, alias: &str, notice: Option<&str>) -> String {
    match snap.agents.iter().find(|x| x.alias == alias) {
        Some(agent) => summon_core::feishu::card_fleet_sessions(agent, notice),
        None => summon_core::feishu::card_fleet(
            &not_found_text(snap, alias),
            true,
            None,
            // 会话操作页只管本机;这里的按钮同样只列本机 agent。
            &snap
                .agents
                .iter()
                .map(|a| summon_core::feishu::DrilldownTarget::local(&a.alias))
                .collect::<Vec<_>>(),
            &[],
            None,
        ),
    }
}

/// 当前已知的远端机器状态(只读缓存,永不发起网络 —— 见 `observe::remote_fleet` 的模块注释)。
fn remotes() -> Vec<crate::observe::remote_fleet::RemoteSnapshot> {
    let cfg = summon_core::config::Config::load();
    crate::observe::remote_fleet::cached_all(&cfg.remotes)
}

/// 处理一条发给 steward 的入站消息。返回 `true` = 已消费(调用方必须 `continue`,
/// **绝不能让它继续往会话层走**)。
///
/// 认不出的文本也返回 `true` 并回一句用法 —— steward 没有会话层可以兜底,
/// 沉默会让人以为 bot 挂了。
#[allow(clippy::too_many_arguments)] // 身份三元组与会话三元组职责各异,合并无意义
pub(crate) fn handle_inbound(
    agent: &Agent,
    fc: &FeishuClient,
    chat_type: &str,
    chat_id: &str,
    text: &str,
    mentions: &[summon_core::ipc::Mention],
    sender_open_id: &str,
    sender_union_id: &str,
) -> bool {
    // **空名单对管家 fail-closed。** 同一个默认值在两种角色下含义不同:普通 agent 空名单 =
    // 「群里谁都能 @ 它干活」,合理;管家空名单 = 「谁都能看全 fleet」,不合理。
    // 而 `allows()` 对空名单一律放行 —— 那是给前者定的规矩,不能直接套到后者头上。
    let roster_empty = agent
        .feishu
        .as_ref()
        .is_none_or(|f| f.allowed_users.is_empty());
    if roster_empty {
        log(&summon_i18n::format!("rf169064ebc", agent.name));
        return true;
    }
    // **talk 名单必须在这里自己过一遍。** 分叉点在会话层之前,那道通用的「谁能让 bot 应答」
    // 检查在下游 —— 走不到了。少了这一句,任何人 @ 管家都能看到全 fleet 状态:
    // 它比普通 agent 的权力更大,闸门只能更紧,不能因为"只读"就放行。
    if !agent.allows(sender_open_id, sender_union_id) {
        log(&summon_i18n::format!("rf0e41914c7", agent.name));
        return true;
    }
    if summon_core::ipc::help_command(text, mentions) {
        let help = summon_core::ipc::steward_chat_help_text();
        let _ = outbound::send_markdown_archived(fc, &agent.state_dir(), chat_type, chat_id, &help);
        return true;
    }
    let alias = match summon_core::ipc::fleet_command(text, mentions) {
        Some(a) => a,
        None => {
            let _ = outbound::send_markdown_archived(
                fc,
                &agent.state_dir(),
                chat_type,
                chat_id,
                summon_i18n::text!("r926b67e067"),
            );
            return true;
        }
    };
    let snap = match snapshot() {
        Ok(s) => s,
        Err(e) => {
            log(&summon_i18n::format!("rf83af85d5b", agent.name));
            let _ = outbound::send_markdown_archived(
                fc,
                &agent.state_dir(),
                chat_type,
                chat_id,
                &summon_i18n::format!("r0d97c85406"),
            );
            return true;
        }
    };
    let card = render(&snap, alias.as_deref(), None, &remotes());
    if let Err(e) = outbound::send_card_archived(
        fc,
        &agent.state_dir(),
        chat_type,
        chat_id,
        None,
        "fleet",
        &card,
    ) {
        log(&summon_i18n::format!("rfb87ce227d", agent.name));
    }
    true
}

/// 处理 fleet 卡上的按钮(刷新 / 下钻 / 返回总览):**原地重渲同一张卡**,不新发一条。
/// 状态是即时算出来的,没有缓存要失效 —— 所以"刷新"就是重算一遍。
/// `target_alias` 有值 = 这次点的是总览页上某个 agent 的下钻按钮。
pub(crate) fn handle_card(
    agent: &Agent,
    fc: &FeishuClient,
    message_id: &str,
    target_alias: Option<&str>,
    target_remote: Option<&str>,
) {
    let snap = match snapshot() {
        Ok(s) => s,
        Err(e) => {
            log(&summon_i18n::format!("r485c8e8b8f", agent.name));
            return;
        }
    };
    let card = render(&snap, target_alias, target_remote, &remotes());
    if let Err(e) = fc.update_card(message_id, &card) {
        log(&summon_i18n::format!("r87bc821a8f", agent.name));
    }
}

/// 「标记已处理」:删本机 `target_alias` 在 `tag` 上的 waiting 标记 —— 仅当它仍是卡上看到的那条
/// 回复;然后原地重渲总览,页首说清结果。
///
/// 只动观测标记,不碰会话。管家自己那道 operate 闸已在 dispatcher 集中过完;回复 id 对不上
/// (期间 agent 又回了一条)就不删,与 Menu Bar 的同名动作同一个判据、同一把锁。
pub(crate) fn handle_waiting_handled(
    agent: &Agent,
    fc: &FeishuClient,
    message_id: &str,
    value: &serde_json::Value,
) {
    // 集中闸过的是收到回调的这个 agent 的 operate 名单;只有管家的名单才代表「能管全 fleet」。
    if !agent.is_steward() {
        log(&summon_i18n::format!("rff112b3ddb", agent.name));
        return;
    }
    let WaitingHandledArgs {
        target_alias,
        tag,
        reply,
    } = WaitingHandledArgs::from_value(value);
    let notice = if target_alias.is_empty() || tag.is_empty() || reply.is_empty() {
        summon_i18n::text!("r791bc5e2f2").to_string()
    } else {
        let cfg = summon_core::config::Config::load();
        let outcome = summon_core::registry::Registry::load(&cfg.agents_dir)
            .map_err(anyhow::Error::from)
            .and_then(|reg| clear_waiting(&reg, target_alias, tag, reply));
        waiting_handled_notice(target_alias, outcome)
    };
    log(&summon_i18n::format!("rdd35288160", agent.name));
    let snap = snapshot();
    if let Err(e) = &snap {
        log(&summon_i18n::format!("r485c8e8b8f", agent.name));
    }
    let card = waiting_handled_card(snap, &remotes(), &notice);
    if let Err(e) = fc.update_card(message_id, &card) {
        log(&summon_i18n::format!("r87bc821a8f", agent.name));
    }
}

/// 点完「标记已处理」后的那张卡:总览 + 页首结果。标记可能已经清了,快照读不出也要把结果
/// 写回卡上,否则点的人看不到任何反馈。
fn waiting_handled_card(
    snap: anyhow::Result<FleetSnapshot>,
    remotes: &[crate::observe::remote_fleet::RemoteSnapshot],
    notice: &str,
) -> String {
    match snap {
        Ok(snap) => render_with_notice(&snap, None, None, remotes, Some(notice)),
        Err(e) => summon_core::feishu::card_fleet(
            &summon_i18n::format!("r0d97c85406"),
            false,
            None,
            &[],
            &[],
            Some(notice),
        ),
    }
}

/// 「标记已处理」按钮 value 里带回来的参数(由 `feishu::card_fleet` 渲进按钮)。
struct WaitingHandledArgs<'a> {
    target_alias: &'a str,
    tag: &'a str,
    reply: &'a str,
}

impl<'a> WaitingHandledArgs<'a> {
    fn from_value(value: &'a serde_json::Value) -> Self {
        let field = |key: &str| {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or_default()
        };
        Self {
            target_alias: field("agent"),
            tag: field("tag"),
            reply: field("reply"),
        }
    }
}

/// 在 `reg` 里找到这个 agent 并清它那一条 waiting。`None` = 本机没有这个 agent(管家自己也算没有:
/// 它没有会话,不会有标记)。
fn clear_waiting(
    reg: &summon_core::registry::Registry,
    target_alias: &str,
    tag: &str,
    reply: &str,
) -> anyhow::Result<Option<crate::runtime::waiting_store::HandledOutcome>> {
    let Some(target) = reg.get(target_alias).filter(|a| !a.is_steward()) else {
        return Ok(None);
    };
    crate::runtime::waiting_store::clear_if_reply(&target.state_dir(), tag, reply).map(Some)
}

/// 「标记已处理」的结果文案。
fn waiting_handled_notice(
    target_alias: &str,
    outcome: anyhow::Result<Option<crate::runtime::waiting_store::HandledOutcome>>,
) -> String {
    use crate::runtime::waiting_store::HandledOutcome;
    match outcome {
        Ok(Some(HandledOutcome::Cleared)) => {
            summon_i18n::format!("r0b201210af")
        }
        Ok(Some(HandledOutcome::Superseded)) => {
            summon_i18n::format!("r3bce83a2b8")
        }
        Ok(Some(HandledOutcome::Absent)) => summon_i18n::text!("r87a1e88659").to_string(),
        Ok(None) => summon_i18n::format!("rbd429d675a"),
        Err(e) => summon_i18n::format!("rc816a29c8a"),
    }
}

/// 翻到某个 agent 的会话操作页(「管理会话」/ 该页「刷新」):**原地重渲同一张卡**。
///
/// 与 [`handle_card`] 一样只读快照 + 渲卡。页上退出按钮的执行不经过这里。
pub(crate) fn handle_sessions_card(
    agent: &Agent,
    fc: &FeishuClient,
    message_id: &str,
    target_alias: &str,
    notice: Option<&str>,
) {
    let snap = match snapshot() {
        Ok(s) => s,
        Err(e) => {
            log(&summon_i18n::format!("ra861a9c31e", agent.name));
            return;
        }
    };
    let card = render_sessions(&snap, target_alias, notice);
    if let Err(e) = fc.update_card(message_id, &card) {
        log(&summon_i18n::format!("ra861a9c31e", agent.name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::fleet::{AgentStatus, DaemonStatus, VisibleState};

    fn snap_with(agents: Vec<AgentStatus>) -> FleetSnapshot {
        FleetSnapshot {
            comment_tasks: Vec::new(),
            comment_tasks_error: None,
            daemon: DaemonStatus {
                running: true,
                pid: Some(1),
                version: None,
                uptime_secs: Some(60),
            },
            agents,
            groups: Vec::new(),
            sentinel: Some("off".into()),
            waiting: Vec::new(),
            menubar_icon: None,
            menubar_show_counts: None,
        }
    }

    fn chat_status(tag: &str, chat_id: Option<&str>) -> summon_core::fleet::ChatStatus {
        summon_core::fleet::ChatStatus {
            tag: tag.into(),
            target: format!("soul-x-{tag}"),
            name: Some(format!("群{tag}")),
            chat_type: Some("group".into()),
            chat_id: chat_id.map(str::to_string),
            busy: false,
            capture: "transcript".into(),
            liveness: "live".into(),
            last_active_secs: Some(12),
            last_active_text: Some("12s前".into()),
            last_active_text_en: None,
            waiting: false,
            state: VisibleState::Idle,
            idle_tmux: false,
        }
    }

    fn agent_status(alias: &str) -> AgentStatus {
        AgentStatus {
            alias: alias.into(),
            busy: false,
            sessions: 0,
            last_active_secs: None,
            last_active_text: None,
            last_active_text_en: None,
            state: VisibleState::Exited,
            idle_tmux: false,
            chats: Vec::new(),
        }
    }

    /// 下钻找不到 agent 时**明说找不到并列出可选**,不悄悄退回总览 ——
    /// 后者会让人以为自己看到的就是那个 agent 的详情。
    #[test]
    fn 下钻未知agent时明说而不是退回总览() {
        let snap = snap_with(vec![agent_status("starlight")]);
        let card = render(&snap, Some("nobody"), None, &[]);
        assert!(card.contains("找不到 agent"), "{card}");
        assert!(card.contains("starlight"), "应列出可选:{card}");
    }

    /// 总览是一张会话列表,底部只有一个刷新按钮。
    #[test]
    fn 总览是一张会话列表() {
        let snap = snap_with(vec![agent_status("a")]);
        let card: serde_json::Value =
            serde_json::from_str(&render(&snap, None, None, &[])).unwrap();
        let els = card["body"]["elements"].as_array().unwrap();
        let body = els[0]["content"].as_str().unwrap();
        assert!(body.contains("还没有任何会话"), "{body}");
        assert!(body.contains("没有会话的 agent:a"), "{body}");
        let nav = &els
            .iter()
            .find(|element| {
                element["columns"][0]["elements"][0]["behaviors"][0]["value"]
                    == serde_json::json!({"action": "fleet_refresh"})
            })
            .unwrap()["columns"];
        assert_eq!(nav.as_array().unwrap().len(), 1, "{nav}");
        assert_eq!(
            nav[0]["elements"][0]["behaviors"][0]["value"],
            serde_json::json!({"action": "fleet_refresh"})
        );
    }

    /// **管家的权力比普通 agent 大,闸门只能更紧。** 分叉点在会话层之前,通用的 talk 检查
    /// 在下游走不到了 —— 所以 `handle_inbound` 必须自己过一遍名单。这条测试盯的是
    /// 「那句检查还在不在」:删掉它,任何人 @ 管家都能看到全 fleet 状态。
    #[test]
    fn 入站处理自带talk名单检查() {
        let src = include_str!("steward.rs");
        let src = src
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("测试模块边界必须存在")
            .0;
        assert!(
            src.contains("agent.allows(sender_open_id, sender_union_id)"),
            "steward 入站必须自己过 talk 名单 —— 分叉点在通用检查之前"
        );
    }

    #[test]
    fn 管家帮助只展示实际支持的命令() {
        let help = summon_core::ipc::steward_chat_help_text();
        assert!(help.contains("`/help`"));
        assert!(help.contains("`/fleet [agent]`"));
        assert!(!help.contains("`/session`"));
        let source = include_str!("steward.rs");
        let help_branch = source.find("ipc::help_command(").expect("/help 分支");
        let fleet_branch = source.find("ipc::fleet_command(").expect("/fleet 分支");
        assert!(
            help_branch < fleet_branch,
            "/help 必须先于 /fleet 的兜底提示"
        );
    }

    /// 空名单对管家必须 fail-closed。这条同样是「删了之后功能测试全绿」的检查 ——
    /// 而它挡的是「新建的管家还没配名单就先被拉进群」这个最容易发生的时刻。
    #[test]
    fn 空名单时拒绝而不是全放行() {
        let src = include_str!("steward.rs");
        let src = src
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("测试模块边界必须存在")
            .0;
        assert!(
            src.contains("f.allowed_users.is_empty()"),
            "管家必须显式拒绝 allow-all —— allows() 的空名单放行规矩是给普通 agent 定的"
        );
    }

    /// 会话操作页只是**另一张只读渲染**:它列出目标 agent 的会话并挂上按钮,
    /// 但按钮的执行不在本文件 —— 那条路走 dispatcher 的卡片动作路由。这条测试盯的是
    /// 「渲染函数还在只读侧」,与下面那条结构性红线互补。
    #[test]
    fn 会话操作页只渲染不执行() {
        let mut agent = agent_status("starlight");
        agent.sessions = 1;
        agent.chats = vec![chat_status("g1", Some("oc_real"))];
        let snap = snap_with(vec![agent]);
        let card = render_sessions(&snap, "starlight", None);
        assert!(card.contains("fleet_session_clean"), "{card}");
        assert!(card.contains("fleet_session_reset"), "{card}");
        // 目标 chat_id 必须原样进 value:tag 是哈希,路由不回去。
        assert!(card.contains("oc_real"), "按钮必须带 chat_id:{card}");
    }

    /// **拿不到 chat_id 的会话绝不渲成可点**:tag 是 chat_id 的哈希、不可逆,没有 chat_id
    /// 就没法把动作路由回目标 agent。渲一个点了没反应的按钮比不渲更糟。
    #[test]
    fn 无chat_id的会话不渲操作按钮() {
        let mut agent = agent_status("starlight");
        agent.sessions = 1;
        agent.chats = vec![chat_status("g1", None)];
        let snap = snap_with(vec![agent]);
        let card = render_sessions(&snap, "starlight", None);
        assert!(!card.contains("fleet_session_clean"), "{card}");
        assert!(card.contains("不可操作"), "要明说为什么不能点:{card}");
    }

    /// owner 2026-08-10:「能不能直接在 fleet 根面板,各个 agent 支持触发 `/fleet <agent>` 的
    /// 按钮」—— 人看完总览的下一个动作八成就是点进某一个,让他再去输入框敲一遍名字,
    /// 是把已经在眼前的信息又要了一次。
    #[test]
    fn 总览页给每个agent一个下钻按钮() {
        let snap = snap_with(vec![agent_status("starlight"), agent_status("flare-5")]);
        let card = render(&snap, None, None, &[]);
        assert!(card.contains("starlight"), "{card}");
        assert!(card.contains("flare-5"), "{card}");
        // 按钮走的是同一个只读动作,带上目标 agent。
        assert!(
            card.contains("\"agent\":\"starlight\""),
            "按钮要带目标:{card}"
        );
        assert!(card.contains("fleet_refresh"), "{card}");
    }

    /// 下钻页不再摆一排同级 agent:这一层的下一步是「管理会话」或「返回」,
    /// 再列全体会把层级搅浑。
    #[test]
    fn 下钻页不重复列全体agent按钮() {
        let snap = snap_with(vec![agent_status("starlight"), agent_status("flare-5")]);
        let card = render(&snap, Some("starlight"), None, &[]);
        assert!(
            !card.contains("\"agent\":\"flare-5\""),
            "下钻页不该出现别的 agent 的下钻按钮:{card}"
        );
        assert!(card.contains("fleet_sessions"), "但要有管理会话入口:{card}");
    }

    /// 名字打错时把可选项摆成按钮 —— 能点比再打一遍强。
    #[test]
    fn 找不到agent时把可选项摆成按钮() {
        let snap = snap_with(vec![agent_status("starlight")]);
        let card = render(&snap, Some("nobody"), None, &[]);
        assert!(card.contains("找不到 agent"), "{card}");
        assert!(
            card.contains("\"agent\":\"starlight\""),
            "要能一键点过去:{card}"
        );
    }

    fn remote_snap(
        name: &str,
        aliases: &[&str],
        age_secs: Option<u64>,
        error: Option<&str>,
    ) -> crate::observe::remote_fleet::RemoteSnapshot {
        crate::observe::remote_fleet::RemoteSnapshot {
            name: name.into(),
            snapshot: (!aliases.is_empty())
                .then(|| snap_with(aliases.iter().map(|a| agent_status(a)).collect())),
            age: age_secs.map(std::time::Duration::from_secs),
            error: error.map(str::to_string),
        }
    }

    /// 跨机聚合的主路径:远端 agent 出现在总览上,并且**带机器名**可点。
    #[test]
    fn 总览列出远端机器的agent() {
        let snap = snap_with(vec![agent_status("starlight")]);
        let remotes = vec![remote_snap("devbox", &["remote-codex"], Some(30), None)];
        let card = render(&snap, None, None, &remotes);
        assert!(card.contains("devbox"), "机器名要出现:{card}");
        assert!(card.contains("remote-codex"), "远端 agent 要出现:{card}");
        // 按钮必须带 remote:两台机器可以有同名 agent。
        assert!(card.contains("\"remote\":\"devbox\""), "{card}");
        // 新鲜度必须标注 —— 远端数据不是此刻的真值,不标就是谎报。
        assert!(card.contains("数据于"), "{card}");
    }

    /// **拉不到的机器不能从卡上消失**,也不能把失败伪装成「这台机器没有 agent」。
    #[test]
    fn 远端拉不到时明说而不是静默省略() {
        let snap = snap_with(vec![agent_status("starlight")]);
        let remotes = vec![remote_snap(
            "devbox",
            &[],
            None,
            Some("ssh: connect refused"),
        )];
        let card = render(&snap, None, None, &remotes);
        assert!(card.contains("devbox"), "机器仍要在卡上:{card}");
        assert!(card.contains("拉不到"), "要说清拉不到:{card}");
        assert!(card.contains("connect refused"), "带上原因:{card}");
    }

    /// 有旧数据 + 这次拉失败:**两个事实都要在**。只显示旧数据会让人以为是当前状态,
    /// 只显示错误又丢掉了本来还有用的信息。
    #[test]
    fn 远端有旧数据又连不上时两个事实都说() {
        let snap = snap_with(vec![]);
        let remotes = vec![remote_snap(
            "devbox",
            &["remote-codex"],
            Some(120),
            Some("timeout"),
        )];
        let card = render(&snap, None, None, &remotes);
        assert!(card.contains("remote-codex"), "旧数据仍要列:{card}");
        assert!(card.contains("现在连不上"), "但要说明它已经不新鲜:{card}");
        assert!(card.contains("timeout"), "{card}");
    }

    /// 下钻远端 agent 走远端快照;**远端页暂不给「管理会话」** —— 停会话要经目标机器的
    /// operate 闸,那条链路还没接,给一个点了没用的入口比不给更糟。
    #[test]
    fn 下钻远端agent走远端快照且暂不给操作入口() {
        let snap = snap_with(vec![agent_status("starlight")]);
        let remotes = vec![remote_snap("devbox", &["remote-codex"], Some(5), None)];
        let card = render(&snap, Some("remote-codex"), Some("devbox"), &remotes);
        assert!(card.contains("remote-codex"), "{card}");
        assert!(
            !card.contains("fleet_sessions"),
            "远端页暂不给管理会话:{card}"
        );
    }

    /// 远端段落与本机同一种形状:一个会话一行,机器上没有会话的 agent 另列名字。
    #[test]
    fn 远端段落按会话列() {
        let snap = snap_with(vec![]);
        let mut remote = remote_snap("devbox", &["remote-codex", "idle-one"], Some(5), None);
        let remote_fleet = remote.snapshot.as_mut().unwrap();
        remote_fleet.agents[0].chats = vec![chat_status("g9", Some("oc_g9"))];
        remote_fleet.groups = vec![summon_core::fleet::GroupStatus {
            tag: "g9".into(),
            name: Some("远端群".into()),
            chat_type: Some("group".into()),
            busy: false,
            agents: 1,
            last_active_secs: Some(60),
            last_active_text: Some("1m前".into()),
            last_active_text_en: None,
            chat_id: Some("oc_g9".into()),
            link: None,
            state: summon_core::fleet::VisibleState::Idle,
            idle_tmux: false,
            members: Vec::new(),
        }];
        let card = render(&snap, None, None, &[remote]);
        assert!(card.contains("| 远端群 |"), "{card}");
        assert!(card.contains("没有会话的 agent:idle-one"), "{card}");
    }

    /// 远端机器没这个 agent(或数据还没拉到)时明说,不悄悄退回本机同名 agent。
    #[test]
    fn 远端找不到agent时明说() {
        let snap = snap_with(vec![agent_status("starlight")]);
        let remotes = vec![remote_snap("devbox", &["remote-codex"], Some(5), None)];
        let card = render(&snap, Some("starlight"), Some("devbox"), &remotes);
        assert!(card.contains("找不到 agent"), "{card}");
        assert!(card.contains("devbox"), "要说清是哪台机器上找不到:{card}");
    }

    /// 没配远端时,总览必须与改动前**逐字节一致** —— 绝大多数用户没有第二台机器。
    #[test]
    fn 没有远端时总览不变() {
        let snap = snap_with(vec![agent_status("starlight")]);
        let with_empty = render(&snap, None, None, &[]);
        assert!(
            !with_empty.contains("🖥"),
            "没有远端就不该出现机器段:{with_empty}"
        );
    }

    /// 下钻到某个 agent 才给「管理会话」入口;总览页没有唯一操作对象,给了就是误导。
    #[test]
    fn 只有下钻卡才有管理会话入口() {
        let snap = snap_with(vec![agent_status("starlight")]);
        assert!(render(&snap, Some("starlight"), None, &[]).contains("fleet_sessions"));
        assert!(!render(&snap, None, None, &[]).contains("fleet_sessions"));
        // 找不到的 agent 同样不给入口:那一页也会落空。
        assert!(!render(&snap, Some("nobody"), None, &[]).contains("fleet_sessions"));
    }

    fn waiting(alias: &str, tag: &str) -> summon_core::waiting::WaitingStatus {
        summon_core::waiting::WaitingStatus {
            alias: alias.into(),
            tag: tag.into(),
            name: Some(format!("群{tag}")),
            chat_type: Some("group".into()),
            reply_message_id: format!("om_{tag}"),
            since_secs: 60,
            since_text: "1m前".into(),
            since_text_en: "1m ago".into(),
            link: format!("lark://applink.feishu.cn/client/chat/open?openChatId=oc_{tag}"),
        }
    }

    /// 待处理区与 Menu Bar 的 Waiting 区同一份数据:总览卡顶列出,按钮带回复 id;下钻页不列。
    #[test]
    fn 总览列待处理下钻页不列() {
        let mut snap = snap_with(vec![agent_status("starlight")]);
        snap.waiting = vec![waiting("starlight", "g1")];
        let card = render(&snap, None, None, &[]);
        assert!(card.contains("待处理(1)"), "{card}");
        assert!(card.contains("fleet_waiting_handled"), "{card}");
        assert!(card.contains("\"reply\":\"om_g1\""), "{card}");
        assert!(
            card.contains("https://applink.feishu.cn/client/chat/open?openChatId=oc_g1"),
            "{card}"
        );
        let drill = render(&snap, Some("starlight"), None, &[]);
        assert!(!drill.contains("fleet_waiting_handled"), "{drill}");
        // 按钮渲出去的 value 与点击时解析的参数是同一份契约。
        let card: serde_json::Value =
            serde_json::from_str(&render(&snap, None, None, &[])).unwrap();
        let value = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e.to_string().contains("fleet_waiting_handled"))
            .map(|row| row["columns"][1]["elements"][0]["behaviors"][0]["value"].clone())
            .unwrap();
        let args = WaitingHandledArgs::from_value(&value);
        assert_eq!(
            (args.target_alias, args.tag, args.reply),
            ("starlight", "g1", "om_g1")
        );
        let notice = render_with_notice(&snap, None, None, &[], Some("已处理"));
        assert!(notice.contains("已处理"), "{notice}");
    }

    /// 「标记已处理」只清卡上看到的那条回复;期间有了新回复就不清。找不到的 agent、管家自己,
    /// 都不碰盘。
    #[test]
    fn 标记已处理只清卡上那条回复() {
        use crate::runtime::waiting_store::{list, set, HandledOutcome};
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let mark = |reply: &str| summon_core::waiting::WaitingMark {
            chat_id: "oc_1".into(),
            reply_message_id: reply.into(),
            link: "lark://applink.feishu.cn/client/chat/open?openChatId=oc_1".into(),
            since_unix: 1_800_000_000,
        };
        set(&agent.state_dir(), "g1", &mark("om_new")).unwrap();
        let reg = summon_core::registry::Registry {
            agents: vec![agent.clone()],
        };
        assert_eq!(
            clear_waiting(&reg, &agent.name, "g1", "om_old").unwrap(),
            Some(HandledOutcome::Superseded)
        );
        assert_eq!(list(&agent.state_dir()).len(), 1, "新回复仍算待处理");
        assert_eq!(clear_waiting(&reg, "nobody", "g1", "om_new").unwrap(), None);
        assert_eq!(
            clear_waiting(&reg, &agent.name, "g1", "om_new").unwrap(),
            Some(HandledOutcome::Cleared)
        );
        assert!(list(&agent.state_dir()).is_empty());

        let mut steward = agent.clone();
        steward.role = summon_core::registry::AgentRole::Steward;
        set(&agent.state_dir(), "g1", &mark("om_new")).unwrap();
        let reg = summon_core::registry::Registry {
            agents: vec![steward],
        };
        assert_eq!(
            clear_waiting(&reg, &agent.name, "g1", "om_new").unwrap(),
            None
        );
        assert_eq!(list(&agent.state_dir()).len(), 1, "管家身份不该被当成目标");
    }

    /// 集中闸认的是收到回调的那个 agent 的名单:别的 bot 发出同样 value 的卡,点的人过了那个
    /// agent 的名单也不该清全 fleet 的标记。判据必须先于任何读盘。
    #[test]
    fn 标记已处理只认管家身份() {
        let src = include_str!("steward.rs");
        let body = &src[src.find("pub(crate) fn handle_waiting_handled(").unwrap()..];
        let guard = body.find("if !agent.is_steward()").expect("缺管家身份判据");
        let first_read = body.find("WaitingHandledArgs::from_value").unwrap();
        assert!(guard < first_read, "身份判据要在解析参数、读盘之前");
        assert!(
            body[guard..first_read].contains("return;"),
            "非管家必须直接返回,只记日志不算挡住"
        );
    }

    /// 快照读不出时,点的人仍要在卡上看到这次点击的结果,并能刷新。
    #[test]
    fn 标记已处理后快照失败也回写结果() {
        let card = waiting_handled_card(Err(anyhow::anyhow!("registry 坏了")), &[], "✅ 已标记");
        assert!(card.contains("✅ 已标记"), "{card}");
        assert!(card.contains("registry 坏了"), "{card}");
        assert!(card.contains("fleet_refresh"), "{card}");
        let ok = waiting_handled_card(
            Ok(snap_with(vec![agent_status("starlight")])),
            &[],
            "✅ 已标记",
        );
        assert!(ok.contains("✅ 已标记"), "{ok}");
        assert!(ok.contains("没有会话的 agent:starlight"), "回到总览:{ok}");
    }

    /// 每种结果的页首文案各不相同,失败要带原因。
    #[test]
    fn 标记已处理的结果文案() {
        use crate::runtime::waiting_store::HandledOutcome;
        let texts = [
            waiting_handled_notice("a", Ok(Some(HandledOutcome::Cleared))),
            waiting_handled_notice("a", Ok(Some(HandledOutcome::Superseded))),
            waiting_handled_notice("a", Ok(Some(HandledOutcome::Absent))),
            waiting_handled_notice("a", Ok(None)),
            waiting_handled_notice("a", Err(anyhow::anyhow!("磁盘满"))),
        ];
        let uniq: std::collections::HashSet<_> = texts.iter().collect();
        assert_eq!(uniq.len(), texts.len(), "{texts:?}");
        assert!(texts[0].contains("已处理"), "{}", texts[0]);
        assert!(texts[1].contains("又回了一条"), "{}", texts[1]);
        assert!(texts[4].contains("磁盘满"), "{}", texts[4]);
    }

    /// **结构性红线**:steward 只许读快照、渲卡片、发消息,绝不许伸手进会话热路径。
    /// 与 sentinel 的同名守卫一致;禁词运行时拼接,免得本测试自身的字面量把自己扫红。
    #[test]
    fn steward_never_touches_chat_hot_path() {
        // 判据是**调用形态**(终端会话层的模块/类型名 + 双冒号),不是那几个字母本身 ——
        // 快照里的 `idle_tmux` 只是一位只读数据,与「伸手进终端会话层」是两回事。
        // 禁词全部运行时拼接:写成字面量的话,这段注释和这张表自己就会把测试扫红
        // (2026-08-09 就先踩了一次)。
        let forbidden = [
            format!("tm{}::", "ux"),
            format!("Tm{}::", "ux"),
            format!("spawn_chat_{}", "thread"),
            format!("handle_{}", "task"),
            format!("Thread{}", "Msg"),
            format!("ChatHand{}", "le"),
        ];
        let src = include_str!("steward.rs");
        for f in &forbidden {
            assert!(!src.contains(f.as_str()), "steward 源码不得出现 {f}");
        }
    }
}
