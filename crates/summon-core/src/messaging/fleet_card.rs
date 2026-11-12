//! fleet 状态的飞书卡片渲染(纯函数)。
//!
//! 数据全部来自 [`crate::fleet::FleetSnapshot`] —— 与 Menu Bar、Electron 控制台**同一份契约**,
//! 这里只负责把它画成卡片,不做任何判断:三态是 Rust 下发的 `state`,人读时长是下发的
//! `last_active_text`。**这个模块里不该出现第二处状态推导** —— 那正是 2026-08-09 把三态
//! 下沉进 `fleet.rs` 要消除的东西。
//!
//! 容量约束(2026-08-09 真机实测):单卡 markdown 表格上限 5 个,长度在 2 万中文字符内不是瓶颈。
//! 所以总览用**一个**表格渲完会话列表(每台远端机器各一个),下钻卡同理 —— 不需要分页。

use crate::fleet::{AgentStatus, FleetSnapshot, GroupStatus, VisibleState};
use crate::waiting::{feishu_web_link, WaitingStatus};

/// 总览里最多列几个会话。顺序是在跑的在前、其余从近到远,截掉的是最久没动的那些,并报出个数。
const MAX_SESSION_ROWS: usize = 20;

/// 单 agent 下钻卡里最多列几条会话。超出按最后活跃排序截断并明示 —— 静默截断会被读成
/// 「就这些」,那比列不全更糟。
const MAX_CHATS_PER_AGENT: usize = 12;

/// 状态的人读文案。形状与文字共同编码(飞书卡片里没有图标色可用,只靠颜色的话就丢信息了)。
///
/// `idle_tmux` 单独成一档不是啰嗦:它意味着「attach 上去就能接着用」,与无头空闲**可操作性不同**,
/// 而那正是用户看这份列表时想知道的下一步。
fn state_text(state: VisibleState, idle_tmux: bool) -> &'static str {
    match state {
        // 泄漏排在最前判:它是事故,不能被 busy/idle 的正常语义盖住。文案直接写"需处理",
        // 因为看这份列表的人要的是下一步动作,而这一档的下一步和其它档完全不同。
        VisibleState::Leaked => summon_i18n::text!("r3530fe2016"),
        VisibleState::Working => summon_i18n::text!("r8d0080097f"),
        VisibleState::Idle if idle_tmux => summon_i18n::text!("rc04320ead4"),
        VisibleState::Idle => summon_i18n::text!("r7406a86efd"),
        VisibleState::Exited => summon_i18n::text!("r26bde1dc64"),
    }
}

/// 一条会话(含群成员)的状态:处于 waiting 就写「待处理」—— agent 回完、轮到 owner。
/// 泄漏先判,它是事故。在干活又处于 waiting 两样都写:主轮交付了、它派出的子 agent 还在跑,
/// 那条回复照样等着 owner。
fn session_state_text(state: VisibleState, idle_tmux: bool, waiting: bool) -> &'static str {
    match state {
        VisibleState::Leaked => state_text(state, idle_tmux),
        VisibleState::Working if waiting => summon_i18n::text!("rd06459ef2b"),
        VisibleState::Working => state_text(state, idle_tmux),
        _ if waiting => summon_i18n::text!("rfde5aad81f"),
        _ => state_text(state, idle_tmux),
    }
}

/// 群的状态格:有成员在干活就报在干活的会话数,再报待处理的成员数。计数单位与抬头一致,都是会话。
fn group_state_text(group: &GroupStatus) -> String {
    let working = group
        .members
        .iter()
        .filter(|m| m.state == VisibleState::Working)
        .count();
    let waiting = group.members.iter().filter(|m| m.waiting).count();
    let mut parts = vec![match group.state {
        VisibleState::Leaked => state_text(group.state, group.idle_tmux).to_string(),
        _ if working > 0 => summon_i18n::format!("rc80cc02d43"),
        _ => state_text(group.state, group.idle_tmux).to_string(),
    }];
    if waiting > 0 {
        parts.push(summon_i18n::format!("r1a6b31ee17"));
    }
    parts.join(" · ")
}

/// 卡片上的名字:方括号会提前闭合 markdown 链接,`\` 会转义掉闭合的 `]`,`|` 会撑破所在的表格
/// 单元格,都换成全角。
fn link_text(text: &str) -> String {
    text.replace('[', "［")
        .replace(']', "］")
        .replace('\\', "＼")
        .replace('|', "｜")
}

/// 会话的显示名:有群名用群名;没有名字的私聊写「私聊 · tag」,其余写 tag。
fn chat_name(name: Option<&str>, chat_type: Option<&str>, tag: &str) -> String {
    match name.filter(|n| !n.is_empty()) {
        Some(name) => name.to_string(),
        None if chat_type == Some("p2p") => summon_i18n::format!("r6e73cc8242"),
        None => tag.to_string(),
    }
}

/// 有可用的飞书跳转就渲成链接,否则原样。
fn linked(text: &str, link: Option<&str>) -> String {
    match link.and_then(feishu_web_link) {
        Some(url) => format!("[{}]({url})", link_text(text)),
        None => link_text(text),
    }
}

/// 没有时间就写「—」而不是留空:空单元格在飞书表格里看不出是「没有」还是「渲漏了」。
fn since_text(text: Option<&str>) -> &str {
    text.filter(|s| !s.is_empty()).unwrap_or("—")
}

/// 抬头行:daemon 状态 + 在跑的会话数 + 待处理数 + sentinel 徽标。
///
/// **daemon 不在时这一行是整张卡最重要的信息** —— 底下的列表此时全是盘上事实、没有 busy 真值,
/// 不说清楚会让人把「不知道」读成「都闲着」。待处理是盘上标记,与 daemon 在不在无关,照常报。
fn header_line(snap: &FleetSnapshot) -> String {
    let mut parts: Vec<String> = Vec::new();
    if snap.daemon.running {
        let uptime = snap
            .daemon
            .uptime_secs
            // humanize 渲的是「多久之前」,所以说「启动于 1h前」而不是「已运行 1h前」。
            .map(|s| {
                summon_i18n::format!("r2de96c0e4d", crate::session_view::humanize_elapsed_secs(s))
            })
            .unwrap_or_default();
        parts.push(summon_i18n::format!("rfc92c658ee"));
        parts.push(summon_i18n::format!("rebdea96b85", snap.working_sessions()));
    } else {
        // 措辞刻意直白:这不是"暂时没数据",是整个 fleet 停了。
        parts.push(summon_i18n::text!("r3c75a32787").to_string());
    }
    if !snap.waiting.is_empty() {
        parts.push(summon_i18n::format!("ra193be5ee7", snap.waiting.len()));
    }
    if let Some(s) = snap.sentinel.as_deref().filter(|s| !s.is_empty()) {
        parts.push(format!("sentinel {s}"));
    }
    parts.join(" · ")
}

/// 总览:抬头 + 会话列表。
pub fn fleet_overview_markdown(snap: &FleetSnapshot) -> String {
    format!(
        "{}\n\n{}",
        header_line(snap),
        sessions_markdown(&snap.agents, &snap.groups)
    )
}

/// 会话列表:一个会话(群或私聊)一行,列出在里面的 agent。顺序沿用快照 —— 有一轮在跑的在前,
/// 其余按最后活跃从近到远(`fleet::build_groups`),这里不重排。没有会话的 agent 另起一行列名字,
/// 它们不出现在表里,但人要知道它们在。
fn sessions_markdown(agents: &[AgentStatus], groups: &[GroupStatus]) -> String {
    if agents.is_empty() {
        return summon_i18n::text!("r325acf1e08").to_string();
    }
    let mut out = String::new();
    if groups.is_empty() {
        out.push_str(summon_i18n::text!("r8b7763a999"));
    } else {
        out.push_str(summon_i18n::text!("rdfe5e083bf"));
        for g in groups.iter().take(MAX_SESSION_ROWS) {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                // 会话名点开就回到那个群(不定位到消息)。
                linked(
                    &chat_name(g.name.as_deref(), g.chat_type.as_deref(), &g.tag),
                    g.link.as_deref()
                ),
                group_state_text(g),
                g.members
                    .iter()
                    .map(|m| m.alias.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                since_text(g.last_active_text.as_deref())
            ));
        }
        if groups.len() > MAX_SESSION_ROWS {
            out.push_str(&summon_i18n::format!(
                "r48f2b946d1",
                groups.len() - MAX_SESSION_ROWS
            ));
        }
    }
    let idle: Vec<&str> = agents
        .iter()
        .filter(|a| a.chats.is_empty())
        .map(|a| a.alias.as_str())
        .collect();
    if !idle.is_empty() {
        out.push_str(&summon_i18n::format!("rfcf91f8d25", idle.join(" ")));
    }
    out
}

/// 单 agent 下钻:它的每条会话一行。
pub fn fleet_agent_markdown(agent: &AgentStatus) -> String {
    let mut out = summon_i18n::format!(
        "r905e7999c5",
        agent.alias,
        state_text(agent.state, agent.idle_tmux),
        agent.sessions
    );
    if agent.chats.is_empty() {
        out.push_str(summon_i18n::text!("r0ef831f3e8"));
        return out;
    }
    out.push_str(summon_i18n::text!("rf18f9da98c"));
    for c in agent.chats.iter().take(MAX_CHATS_PER_AGENT) {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            chat_name(c.name.as_deref(), c.chat_type.as_deref(), &c.tag),
            session_state_text(c.state, c.idle_tmux, c.waiting),
            c.capture,
            since_text(c.last_active_text.as_deref())
        ));
    }
    if agent.chats.len() > MAX_CHATS_PER_AGENT {
        out.push_str(&summon_i18n::format!(
            "r48f2b946d1",
            agent.chats.len() - MAX_CHATS_PER_AGENT
        ));
    }
    out
}

/// 会话操作页里最多列几条。比只读下钻卡更小:每条会话要带两个按钮,一屏堆太多既渲不下
/// (飞书对分栏数有硬上限),也让「点错行」的概率变高 —— 而这里点错是要停掉别人的会话的。
pub const MAX_CHATS_PER_OPS_PAGE: usize = 6;

/// 会话操作页的抬头 + 一条会话的人读描述(按钮由卡片层渲,这里只出文案)。
///
/// **why 单独一页而不是把按钮挂到只读下钻卡上**:只读卡的按钮一直是免鉴权的纯重渲,
/// 「看一眼状态」与「停掉一条会话」混在同一次点击里,点错的代价不对等。分成两张卡后,
/// 只读卡保持原样,破坏性动作独占一页 + 每个按钮各自二次确认(owner 2026-08-10)。
pub fn fleet_ops_header(agent: &AgentStatus) -> String {
    summon_i18n::format!(
        "rda9eaa02fb",
        agent.alias,
        agent.sessions,
        if agent.chats.len() > MAX_CHATS_PER_OPS_PAGE {
            summon_i18n::format!("ra13e9f75ec")
        } else {
            String::new()
        }
    )
}

/// 操作页里一条会话的描述行。序号让人对得上下面那排按钮。
pub fn fleet_ops_chat_line(index: usize, chat: &crate::fleet::ChatStatus) -> String {
    format!(
        "**{}. {}** · {} · {} · {}",
        index + 1,
        chat_name(chat.name.as_deref(), chat.chat_type.as_deref(), &chat.tag),
        session_state_text(chat.state, chat.idle_tmux, chat.waiting),
        chat.capture,
        since_text(chat.last_active_text.as_deref())
    )
}

/// 总览卡顶部最多列几条待处理(每条带一个按钮)。按等待时长从久到新排,
/// 截掉的是较新的几条,并明示条数。
pub const MAX_WAITING_ROWS: usize = 10;

/// 待处理区的标题行。
pub fn fleet_waiting_header(count: usize) -> String {
    summon_i18n::format!("rb7304bd270")
}

/// 待处理的一条:`研发群 · starlight · 12m前`,整行是跳到那条回复的链接。
pub fn fleet_waiting_line(item: &WaitingStatus) -> String {
    let chat = chat_name(item.name.as_deref(), item.chat_type.as_deref(), &item.tag);
    linked(
        &format!("{chat} · {} · {}", item.alias, item.since_text),
        Some(&item.link),
    )
}

/// 一台远端机器在总览卡上的段落。
///
/// **三种状态都要说得出口**,而且必须彼此可分:
/// - 有数据 → 表格 + 「N 前」的新鲜度(数据是拉来的,不是此刻的真值,不标注就是谎报);
/// - 有旧数据但这次拉失败 → 表格照列,另起一行说「现在连不上」—— 一次网络抖动不该让整台
///   机器从卡上消失,但也不能让人以为看到的是当前状态;
/// - 从没拉到过 → 只说失败原因。**不能什么都不显示**:配了 remote 却一条不显示,
///   和「没配」在卡上长得一模一样,人会以为配置没生效。
///
/// 列表与本机同一种形状(会话一行)。`age_text` 由调用方渲好(与 `last_active_text` 同哲学:
/// 时长文案只有一份事实源)。
pub fn fleet_remote_markdown(
    name: &str,
    snapshot: Option<&FleetSnapshot>,
    age_text: Option<&str>,
    error: Option<&str>,
) -> String {
    let mut out = format!("**🖥 {name}**");
    match age_text {
        Some(age) => out.push_str(&summon_i18n::format!("r63315bebaa")),
        None if snapshot.is_some() => out.push_str(summon_i18n::text!("r0a3a238047")),
        None => {}
    }
    if let Some(err) = error.map(str::trim).filter(|e| !e.is_empty()) {
        // 有旧数据时措辞是「现在连不上」,没有时是「拉不到」——两种处境对读者意味着不同的下一步。
        let prefix = if snapshot.is_some() {
            summon_i18n::text!("r0a707e8034")
        } else {
            summon_i18n::text!("r53c3407d9b")
        };
        out.push_str(&format!("{prefix}:{err}"));
    }
    out.push_str("\n\n");
    if let Some(snap) = snapshot {
        out.push_str(&sessions_markdown(&snap.agents, &snap.groups));
    }
    out
}

/// 秒 → 卡片上的新鲜度文案。复用会话列表那套分桶,三端一份事实源。
pub fn freshness_text(secs: u64) -> String {
    crate::session_view::humanize_elapsed_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{ChatStatus, DaemonStatus, GroupMemberStatus, GroupStatus};
    use crate::waiting::WaitingStatus;

    fn chat(tag: &str, state: VisibleState, idle_tmux: bool, capture: &str) -> ChatStatus {
        ChatStatus {
            tag: tag.into(),
            target: format!("soul-x-{tag}"),
            name: Some(format!("群{tag}")),
            chat_type: Some("group".into()),
            chat_id: Some(format!("oc_{tag}")),
            busy: matches!(state, VisibleState::Working),
            capture: capture.into(),
            liveness: "live".into(),
            last_active_secs: Some(12),
            last_active_text: Some("12s前".into()),
            last_active_text_en: None,
            waiting: false,
            state,
            idle_tmux,
        }
    }

    fn agent(
        alias: &str,
        state: VisibleState,
        idle_tmux: bool,
        chats: Vec<ChatStatus>,
    ) -> AgentStatus {
        AgentStatus {
            alias: alias.into(),
            busy: matches!(state, VisibleState::Working),
            sessions: chats.len(),
            last_active_secs: Some(12),
            last_active_text: Some("12s前".into()),
            last_active_text_en: None,
            state,
            idle_tmux,
            chats,
        }
    }

    fn snap(agents: Vec<AgentStatus>, running: bool) -> FleetSnapshot {
        FleetSnapshot {
            comment_tasks: Vec::new(),
            comment_tasks_error: None,
            daemon: DaemonStatus {
                running,
                pid: running.then_some(1),
                version: None,
                uptime_secs: running.then_some(3600),
            },
            agents,
            groups: Vec::new(),
            sentinel: Some("on".into()),
            waiting: Vec::new(),
            menubar_icon: None,
            menubar_show_counts: None,
        }
    }

    /// 四种状态各有各的说法,**Tmux 空闲不能被压成普通空闲** —— 那一档的意义是
    /// 「attach 上去就能接着用」,与无头空闲可操作性不同。
    #[test]
    fn 四种状态文案互不相同() {
        let texts = [
            state_text(VisibleState::Working, false),
            state_text(VisibleState::Idle, true),
            state_text(VisibleState::Idle, false),
            state_text(VisibleState::Exited, false),
        ];
        let uniq: std::collections::HashSet<_> = texts.iter().collect();
        assert_eq!(uniq.len(), 4, "四种状态必须各自可辨:{texts:?}");
        assert!(texts[1].contains("Tmux"));
    }

    fn member(alias: &str, busy: bool, waiting: bool) -> GroupMemberStatus {
        GroupMemberStatus {
            alias: alias.into(),
            target: format!("soul-{alias}-g1"),
            busy,
            capture: "rpc".into(),
            liveness: "live".into(),
            last_active_secs: Some(5),
            last_active_text: Some("5s前".into()),
            last_active_text_en: None,
            waiting,
            state: if busy {
                VisibleState::Working
            } else {
                VisibleState::Idle
            },
            idle_tmux: false,
        }
    }

    fn group(name: &str, members: Vec<GroupMemberStatus>, link: Option<&str>) -> GroupStatus {
        let busy = members.iter().any(|m| m.busy);
        let working = members.iter().any(|m| m.state == VisibleState::Working);
        GroupStatus {
            tag: format!("g{name}"),
            name: Some(name.into()),
            chat_type: Some("group".into()),
            busy,
            agents: members.len(),
            last_active_secs: Some(5),
            last_active_text: Some("5s前".into()),
            last_active_text_en: None,
            chat_id: Some(format!("oc_{name}")),
            link: link.map(str::to_string),
            state: if working {
                VisibleState::Working
            } else {
                VisibleState::Idle
            },
            idle_tmux: false,
            members,
        }
    }

    /// 总览只用**一个**表格 —— 飞书单卡表格上限 5 个,总览不该把预算花在这里。
    #[test]
    fn 总览只用一个表格() {
        let mut s = snap(
            vec![
                agent(
                    "a",
                    VisibleState::Working,
                    false,
                    vec![chat("t1", VisibleState::Working, false, "rpc")],
                ),
                agent("b", VisibleState::Exited, false, vec![]),
            ],
            true,
        );
        s.groups = vec![group("研发群", vec![member("a", true, false)], None)];
        let md = fleet_overview_markdown(&s);
        // 数**分隔行的行数**而不是 "|---|" 出现次数:四列分隔行里含两个不重叠的 "|---|",
        // 按子串数会把一个表算成两个。
        let tables = md.lines().filter(|l| l.starts_with("|---")).count();
        assert_eq!(tables, 1, "{md}");
        assert!(md.contains("● 1 个工作中"), "{md}");
        assert!(md.contains("1 个会话工作中"), "{md}");
        assert!(md.contains("sentinel on"));
    }

    /// 总览一个会话一行,顺序照搬快照(在跑的在前、其余从近到远);超出上限截掉最久没动的,报出个数。
    #[test]
    fn 总览按快照顺序列会话并截断报数() {
        let mut s = snap(vec![agent("a", VisibleState::Idle, false, vec![])], true);
        s.groups = (0..MAX_SESSION_ROWS + 2)
            .map(|i| group(&format!("群{i:02}"), vec![member("a", false, false)], None))
            .collect();
        let md = fleet_overview_markdown(&s);
        let rows: Vec<&str> = md.lines().filter(|l| l.starts_with("| 群")).collect();
        assert_eq!(rows.len(), MAX_SESSION_ROWS, "{md}");
        assert!(rows[0].starts_with("| 群00 |"), "{md}");
        assert!(rows[MAX_SESSION_ROWS - 1].starts_with("| 群19 |"), "{md}");
        assert!(md.contains("另有 2 个更早的会话未列出"), "{md}");
        // 正好满上限时不截断,也不报「另有 0 个」。
        s.groups.truncate(MAX_SESSION_ROWS);
        let md = fleet_overview_markdown(&s);
        assert!(!md.contains("未列出"), "{md}");
        // 没有名字的私聊写「私聊 · tag」。
        let mut dm = group("x", vec![member("a", false, false)], None);
        dm.name = None;
        dm.tag = "p9f8".into();
        dm.chat_type = Some("p2p".into());
        s.groups = vec![dm];
        assert!(fleet_overview_markdown(&s).contains("| 私聊 · p9f8 |"));
    }

    /// 没有会话的 agent 不在表里,但要列出名字 —— 不然人以为它没注册。
    #[test]
    fn 没有会话的agent另起一行列名字() {
        let mut s = snap(
            vec![
                agent(
                    "a",
                    VisibleState::Idle,
                    false,
                    vec![chat("t1", VisibleState::Idle, false, "rpc")],
                ),
                agent("b", VisibleState::Exited, false, vec![]),
                agent("c", VisibleState::Exited, false, vec![]),
            ],
            true,
        );
        s.groups = vec![group("研发群", vec![member("a", false, false)], None)];
        let md = fleet_overview_markdown(&s);
        assert!(md.contains("没有会话的 agent:b c"), "{md}");
        assert!(!md.contains("没有会话的 agent:a"), "{md}");
    }

    /// **daemon 不在时必须说出来**:底下列表此刻没有 busy 真值,不说清楚会被读成「都闲着」。
    #[test]
    fn daemon不在时抬头明说() {
        let md = fleet_overview_markdown(&snap(
            vec![agent("a", VisibleState::Idle, false, vec![])],
            false,
        ));
        assert!(md.contains("daemon 没在跑"), "{md}");
        assert!(md.contains("无法判断谁在工作"), "{md}");
        // 不该在 daemon 不在时报「0 个在跑」—— 那是"确认都闲着"的口气。
        assert!(!md.contains("个会话工作中"), "{md}");
    }

    /// 会话超过上限要截断**并报数**。
    #[test]
    fn 下钻超过上限时明示未列出的条数() {
        let chats: Vec<ChatStatus> = (0..MAX_CHATS_PER_AGENT + 3)
            .map(|i| chat(&format!("t{i}"), VisibleState::Idle, false, "rpc"))
            .collect();
        let md = fleet_agent_markdown(&agent("a", VisibleState::Idle, false, chats));
        assert!(md.contains("另有 3 个更早的会话未列出"), "{md}");
    }

    /// 空态不留白:没有 agent / 没有会话时说人话,不给一张空表。
    #[test]
    fn 空态给出说明而不是空表() {
        assert!(fleet_overview_markdown(&snap(vec![], true)).contains("没有已注册的 agent"));
        let a = agent("a", VisibleState::Exited, false, vec![]);
        assert!(fleet_agent_markdown(&a).contains("没有会话"));
        let md = fleet_overview_markdown(&snap(vec![a], true));
        assert!(md.contains("还没有任何会话"), "{md}");
        assert!(!md.contains("|---"), "{md}");
    }

    /// 缺时间戳渲成「—」,不留空单元格(空格看不出是"没有"还是"渲漏了")。
    #[test]
    fn 缺最后活跃渲成破折号() {
        let mut s = snap(vec![agent("a", VisibleState::Idle, false, vec![])], true);
        let mut g = group("研发群", vec![member("a", false, false)], None);
        g.last_active_text = None;
        s.groups = vec![g];
        let md = fleet_overview_markdown(&s);
        assert!(md.contains("| — |"), "{md}");
    }

    /// 一个会话一行,列出在里面的全部 agent。
    #[test]
    fn 会话行列出成员() {
        let mut s = snap(vec![agent("a", VisibleState::Idle, false, vec![])], true);
        s.groups = vec![group(
            "研发群",
            vec![
                member("starlight", true, false),
                member("arcadia", false, false),
            ],
            None,
        )];
        let md = fleet_overview_markdown(&s);
        assert!(md.contains("| 研发群 |"), "{md}");
        assert!(md.contains("starlight arcadia"), "{md}");
        assert!(md.contains("● 1 个工作中"), "{md}");
    }

    fn waiting_item(alias: &str, tag: &str, name: Option<&str>, link: &str) -> WaitingStatus {
        WaitingStatus {
            alias: alias.into(),
            tag: tag.into(),
            name: name.map(str::to_string),
            chat_type: Some(if tag.starts_with('p') { "p2p" } else { "group" }.into()),
            reply_message_id: format!("om_{tag}"),
            since_secs: 720,
            since_text: "12m前".into(),
            since_text_en: "12m ago".into(),
            link: link.into(),
        }
    }

    const LINK: &str = "lark://applink.feishu.cn/client/chat/open?openChatId=oc_g1&position=88";

    /// 抬头的两个数与 Menu Bar 同口径:working 按会话数(一个 agent 两个群在跑就是 2),
    /// 待处理按 (agent, 会话) 数。
    #[test]
    fn 抬头报在跑的会话数与待处理数() {
        let mut s = snap(
            vec![
                agent(
                    "a",
                    VisibleState::Working,
                    false,
                    vec![
                        chat("t1", VisibleState::Working, false, "rpc"),
                        chat("t2", VisibleState::Working, false, "rpc"),
                        chat("t3", VisibleState::Idle, false, "rpc"),
                    ],
                ),
                agent(
                    "b",
                    VisibleState::Working,
                    false,
                    vec![chat("t4", VisibleState::Working, false, "rpc")],
                ),
            ],
            true,
        );
        let md = fleet_overview_markdown(&s);
        assert!(md.contains("3 个会话工作中"), "{md}");
        assert!(!md.contains("待处理"), "没有待处理就不报这一段:{md}");
        s.waiting = vec![
            waiting_item("a", "g1", Some("研发群"), LINK),
            waiting_item("b", "p2", None, LINK),
        ];
        assert!(
            fleet_overview_markdown(&s).contains("2 条回复待处理"),
            "{}",
            fleet_overview_markdown(&s)
        );
    }

    /// 待处理是盘上标记,daemon 不在时照样要报 —— 那条回复 owner 还是没看。
    #[test]
    fn daemon不在时仍报待处理数() {
        let mut s = snap(vec![agent("a", VisibleState::Idle, false, vec![])], false);
        s.waiting = vec![waiting_item("a", "g1", Some("研发群"), LINK)];
        let md = fleet_overview_markdown(&s);
        assert!(md.contains("daemon 没在跑"), "{md}");
        assert!(md.contains("1 条回复待处理"), "{md}");
    }

    /// 待处理的一条整行是跳到那条回复的链接,卡片里用 https 的 AppLink。
    #[test]
    fn 待处理条目是跳回那条回复的链接() {
        let line = fleet_waiting_line(&waiting_item("starlight", "g1", Some("研发群"), LINK));
        assert_eq!(
            line,
            "[研发群 · starlight · 12m前](https://applink.feishu.cn/client/chat/open?openChatId=oc_g1&position=88)"
        );
    }

    /// 没有群名时私聊写「私聊 · tag」;认不出的链接不渲成链接;名字里的方括号不能提前闭合链接。
    #[test]
    fn 待处理条目的名字与链接兜底() {
        let dm = fleet_waiting_line(&waiting_item("arcadia", "p9", None, LINK));
        assert!(dm.starts_with("[私聊 · p9 · arcadia · 12m前]("), "{dm}");
        let plain = fleet_waiting_line(&waiting_item(
            "arcadia",
            "g1",
            Some("研发群"),
            "https://example.com/x",
        ));
        assert_eq!(plain, "研发群 · arcadia · 12m前");
        let bracket = fleet_waiting_line(&waiting_item("a", "g1", Some("[周会]研发"), LINK));
        assert!(
            bracket.starts_with("[［周会］研发 · a · 12m前]("),
            "{bracket}"
        );
        let pipe = fleet_waiting_line(&waiting_item("a", "g1", Some("A|B\\"), LINK));
        assert!(pipe.starts_with("[A｜B＼ · a · 12m前]("), "{pipe}");
    }

    /// 会话级状态:处于 waiting 写「待处理」,在干活时与「工作中」并列;泄漏先判。
    #[test]
    fn 会话处于waiting时状态写待处理() {
        let mut waiting = chat("t1", VisibleState::Idle, true, "transcript");
        waiting.waiting = true;
        let mut working = chat("t2", VisibleState::Working, false, "rpc");
        working.waiting = true;
        let mut leaked = chat("t3", VisibleState::Leaked, false, "rpc");
        leaked.waiting = true;
        let md = fleet_agent_markdown(&agent(
            "a",
            VisibleState::Working,
            false,
            vec![waiting.clone(), working, leaked],
        ));
        let row = |tag: &str| {
            md.lines()
                .find(|l| l.contains(&format!("群{tag}")))
                .unwrap_or_default()
                .to_string()
        };
        assert!(row("t1").contains("◆ 待处理"), "{md}");
        assert!(row("t2").contains("● 工作中 · ◆ 待处理"), "{md}");
        assert!(row("t3").contains("▲"), "{md}");
        assert!(fleet_ops_chat_line(0, &waiting).contains("◆ 待处理"));
        // 没有名字的私聊在下钻与操作页里同样写「私聊 · tag」。
        let mut dm = chat("p7", VisibleState::Idle, false, "rpc");
        dm.name = None;
        dm.chat_type = Some("p2p".into());
        assert!(fleet_ops_chat_line(0, &dm).contains("私聊 · p7"));
        let md = fleet_agent_markdown(&agent("a", VisibleState::Idle, false, vec![dm]));
        assert!(md.contains("| 私聊 · p7 |"), "{md}");
    }

    /// 只有子 agent 在跑的成员(busy 为 false)也算进群格的工作中,与抬头同一口径。
    #[test]
    fn 群格的工作中算上只有子agent在跑的成员() {
        let mut s = snap(vec![agent("a", VisibleState::Idle, false, vec![])], true);
        let mut subagents = member("b", false, true);
        subagents.state = VisibleState::Working;
        s.groups = vec![group(
            "研发群",
            vec![member("a", true, false), subagents],
            None,
        )];
        let md = fleet_overview_markdown(&s);
        assert!(md.contains("● 2 个工作中 · ◆ 1 个待处理"), "{md}");
    }

    /// 会话的状态格报在跑的会话数与待处理的成员数;会话名点开回到那个群。
    #[test]
    fn 会话行报在跑与待处理并给群链接() {
        let mut s = snap(vec![agent("a", VisibleState::Idle, false, vec![])], true);
        s.groups = vec![group(
            "研发群",
            vec![
                member("a", true, false),
                member("b", true, false),
                member("c", false, true),
            ],
            Some("lark://applink.feishu.cn/client/chat/open?openChatId=oc_g1"),
        )];
        let md = fleet_overview_markdown(&s);
        assert!(md.contains("● 2 个工作中 · ◆ 1 个待处理"), "{md}");
        assert!(
            md.contains("[研发群](https://applink.feishu.cn/client/chat/open?openChatId=oc_g1)"),
            "{md}"
        );
        s.groups = vec![group("研发群", vec![member("c", false, true)], None)];
        let md = fleet_overview_markdown(&s);
        assert!(md.contains("| 研发群 | ○ 空闲 · ◆ 1 个待处理 |"), "{md}");
        // 没有链接的群名同样不能撑破表格。
        s.groups[0].name = Some("A|B".into());
        let md = fleet_overview_markdown(&s);
        assert!(md.contains("| A｜B | ○ 空闲"), "{md}");
        // 泄漏是事故,即使有成员在跑也先报泄漏。
        let mut leaked = group(
            "研发群",
            vec![member("a", true, false), member("c", false, true)],
            None,
        );
        leaked.state = VisibleState::Leaked;
        s.groups = vec![leaked];
        let md = fleet_overview_markdown(&s);
        assert!(
            md.contains(&format!(
                "| 研发群 | {} · ◆ 1 个待处理 |",
                state_text(VisibleState::Leaked, false)
            )),
            "{md}"
        );
    }
}
