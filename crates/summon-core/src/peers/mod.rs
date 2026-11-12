//! `/introduce` 认识关系的纯逻辑:Peer 结构体、peers 文件 schema、匹配/合并纯函数。
//!
//! peer 可以是别的 bot,也可以是真人(同事)。两者在群里的送达方式不同 —— bot 收不到没 @ 它的
//! 群消息,@ 是唯一通道;真人本来就看得见群消息,@ 只是提醒 —— 所以名单按身份分开告诉 agent。
//!
//! 设计原则:
//! - per-chat 隔离:bot A 和 B 在群 X 认识,不代表在群 Y 也认识
//! - 不自动发现:只能通过 `/introduce` 显式认识,绝不扫群成员自动加
//! - 纯函数无副作用:所有匹配/合并逻辑可单测,IO 由 `summon::peers_store` 承担

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// peer 的身份。来源只有飞书给的两处事实:`/introduce` 时 @ 事件的 `mentioned_type`,以及
/// 这个 peer 自己发消息时的 `sender_type`。两处都没给过就是 `Unknown`,不猜。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerKind {
    #[default]
    Unknown,
    Bot,
    Human,
}

impl PeerKind {
    /// 飞书 @ 事件的 `mentioned_type`:`user` / `bot`。
    pub fn from_mentioned_type(raw: &str) -> Self {
        match raw {
            "bot" => Self::Bot,
            "user" => Self::Human,
            _ => Self::Unknown,
        }
    }

    /// 飞书消息的 `sender_type`:`user` / `app`。
    pub fn from_sender_type(raw: &str) -> Self {
        match raw {
            "app" | "bot" => Self::Bot,
            "user" => Self::Human,
            _ => Self::Unknown,
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// 一个已认识 peer 的最小标识。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// 飞书 open_id(`ou_xxx`),唯一键。
    pub open_id: String,
    /// 显示名(contact.v3 解析结果或 mention 里的 name)。
    pub name: String,
    /// 未确认身份不落盘:没有这个字段的旧文件读进来就是 `Unknown`,写回也不凭空多出一个身份。
    #[serde(default, skip_serializing_if = "PeerKind::is_unknown")]
    pub kind: PeerKind,
}

/// per-chat peers 文件内容(对应 `~/.summon/run/peers/<chat-tag>.yaml`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeersFile {
    pub peers: Vec<Peer>,
    /// `/introduce` 执行时间(ISO-8601 字符串),用于展示和调试。
    #[serde(default)]
    pub introduced_at: Option<String>,
    /// `/introduce` 命令词后面跟的说明文字(如「starlight 开发、blue-wing 审核」)。
    /// 描述这组 peer 的协作关系,冷启 PRIMER 与热更新都会带上——否则用户交代的分工只存在于
    /// 那一条飞书消息里,bot 认识了彼此却不知道各自该干什么。老文件无此字段 → None,不影响读取。
    #[serde(default)]
    pub note: Option<String>,
}

/// 精确匹配 name(大小写敏感,完全相等)。
pub fn match_peer_by_name<'a>(peers: &'a [Peer], name: &str) -> Option<&'a Peer> {
    peers.iter().find(|p| p.name == name)
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
/// 按文本匹配 peer:text 可以是 alias 或 name。
///
/// 匹配顺序:
/// 1. 先在 peers 里精确匹配 name
/// 2. 没命中:用 alias_map 把 text 转成 name(alias→name),再匹配
///
/// 返回**拥有所有权**的 `Peer` clone(方便调用方持有,避免生命周期纠缠)。
pub(crate) fn match_peer_by_text(
    peers: &[Peer],
    alias_map: &HashMap<String, String>,
    text: &str,
) -> Option<Peer> {
    // ① 直接按 name 匹配
    if let Some(p) = match_peer_by_name(peers, text) {
        return Some(p.clone());
    }
    // ② 通过 alias_map 转 name 再匹配
    if let Some(resolved_name) = alias_map.get(text) {
        if let Some(p) = match_peer_by_name(peers, resolved_name) {
            return Some(p.clone());
        }
    }
    None
}

/// 按 open_id 去重合并两组 peers。new 里的同 open_id 覆盖 existing 里的(name 取 new 的);
/// new 没带身份时保留 existing 已确认的身份 —— 「这次不知道」不能冲掉「之前知道」。
///
/// 顺序:existing 在前、new 中独有的追加在后,保持稳定排序。
pub fn merge_peers(existing: &[Peer], new: &[Peer]) -> Vec<Peer> {
    let mut out: Vec<Peer> = existing.to_vec();
    for np in new {
        if let Some(pos) = out.iter().position(|ep| ep.open_id == np.open_id) {
            let kind = if np.kind.is_unknown() {
                out[pos].kind
            } else {
                np.kind
            };
            out[pos] = Peer { kind, ..np.clone() };
        } else {
            out.push(np.clone());
        }
    }
    out
}

/// 某个 peer 发来消息时,按飞书给的发送者类型补记 / 更正它的身份。有改动才返回新名单
/// (调用方据此决定要不要落盘);观察不到身份、不在名单里、或身份没变都返回 None。
pub fn with_observed_kind(peers: &[Peer], open_id: &str, observed: PeerKind) -> Option<Vec<Peer>> {
    if observed.is_unknown() {
        return None;
    }
    let pos = peers.iter().position(|p| p.open_id == open_id)?;
    if peers[pos].kind == observed {
        return None;
    }
    let mut out = peers.to_vec();
    out[pos].kind = observed;
    Some(out)
}

/// 按身份把名单拆成 (bot, 真人, 未确认) 三组,组内保持原序。
fn split_by_kind(peers: &[Peer]) -> (Vec<&Peer>, Vec<&Peer>, Vec<&Peer>) {
    let pick = |kind: PeerKind| peers.iter().filter(move |p| p.kind == kind).collect();
    (
        pick(PeerKind::Bot),
        pick(PeerKind::Human),
        pick(PeerKind::Unknown),
    )
}

/// 回执 / 热更新里点名用:按身份分组列名字,如「bot:星光;同事(真人):张三、李四」。
pub fn describe_by_kind(peers: &[Peer]) -> String {
    let (bots, humans, unknown) = split_by_kind(peers);
    let mut groups = Vec::new();
    if !bots.is_empty() {
        groups.push(format!("bot:{}", join_names(&bots)));
    }
    if !humans.is_empty() {
        groups.push(summon_i18n::format!("r986fcc5e05", join_names(&humans)));
    }
    if !unknown.is_empty() {
        groups.push(summon_i18n::format!("r16f1bf2fe6", join_names(&unknown)));
    }
    groups.join(";")
}

fn join_names(peers: &[&Peer]) -> String {
    peers
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

/// 生成 peers 上下文注入文本(供 PRIMER 冷启注入使用),按身份分组:
///
/// ```text
/// 你在本群认识的 bot:
/// - starlight（星光）
/// bot 收不到没 @ 它的群消息:需要它们帮忙时直接 @名字。
/// 你在本群认识的同事(真人):
/// - 张三
/// 群消息他们本来就看得见,@ 只是提醒;按真人对待,bot 之间的协作与防回音规则不适用。
/// ```
///
/// - `alias_map`:alias→name 映射(由 `build_alias_name_map` 提供),用于反查 name→alias 显示
/// - `note`:`/introduce` 时交代的分工说明(见 `PeersFile::note`),有则附在末尾
/// - 身份未确认的单独一组,说明以他们发来的消息为准
/// - peers 为空返回空串(不注入)
pub fn compose_peers_primer(
    peers: &[Peer],
    alias_map: &HashMap<String, String>,
    note: Option<&str>,
) -> String {
    if peers.is_empty() {
        return String::new();
    }
    // 反查 name→alias(alias_map 是 alias→name)
    let mut name_to_alias = HashMap::new();
    for (alias, name) in alias_map {
        name_to_alias.insert(name.as_str(), alias.as_str());
    }
    let list = |group: &[&Peer]| -> String {
        let mut out = String::new();
        for peer in group {
            match name_to_alias.get(peer.name.as_str()) {
                Some(alias) => out.push_str(&format!("- {}（{}）\n", alias, peer.name)),
                None => out.push_str(&format!("- {}\n", peer.name)),
            }
        }
        out
    };
    let (bots, humans, unknown) = split_by_kind(peers);
    let mut sections = Vec::new();
    if !bots.is_empty() {
        sections.push(summon_i18n::format!(
            "rd109f316a5",
            list(&bots),
            BOT_GUIDANCE = ::summon_core::i18n::label(BOT_GUIDANCE)
        ));
    }
    if !humans.is_empty() {
        sections.push(summon_i18n::format!(
            "r1faaecae1c",
            list(&humans),
            HUMAN_GUIDANCE = ::summon_core::i18n::label(HUMAN_GUIDANCE)
        ));
    }
    if !unknown.is_empty() {
        sections.push(summon_i18n::format!(
            "r5f53eccdcf",
            list(&unknown),
            UNKNOWN_GUIDANCE = ::summon_core::i18n::label(UNKNOWN_GUIDANCE)
        ));
    }
    let mut out = sections.join("\n");
    if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
        out.push_str(&summon_i18n::format!("rf332158e41"));
    }
    out
}

const BOT_GUIDANCE: &str = "r03730d8be1";
const HUMAN_GUIDANCE: &str = "r01fda332ed";
const UNKNOWN_GUIDANCE: &str = "ra386f56011";

/// 生成热更新注入文本(供 `/introduce` 时往 tmux pane 注入使用),同样按身份分组。
///
/// 格式:`[summon] 你在本群新认识了 bot:星光;同事(真人):张三。…`
/// `note`(`/introduce` 后面跟的分工说明)非空时另起一句附上——这是用户发这条命令的主要意图,
/// 只存 peer 名单会把「谁负责什么」丢掉。peers 为空返回空串。
pub fn compose_peers_hot_update(new_peers: &[Peer], note: Option<&str>) -> String {
    if new_peers.is_empty() {
        return String::new();
    }
    let (bots, humans, unknown) = split_by_kind(new_peers);
    let guidance: String = [
        (!bots.is_empty(), ::summon_core::i18n::label(BOT_GUIDANCE)),
        (
            !humans.is_empty(),
            ::summon_core::i18n::label(HUMAN_GUIDANCE),
        ),
        (
            !unknown.is_empty(),
            ::summon_core::i18n::label(UNKNOWN_GUIDANCE),
        ),
    ]
    .iter()
    .filter(|(present, _)| *present)
    .map(|(_, text)| *text)
    .collect();
    let mut out = summon_i18n::format!("raa86fa8ba1", describe_by_kind(new_peers));
    if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
        out.push_str(&summon_i18n::format!("rdce5186f39"));
    }
    out
}

/// 生成**解除**认识的热更新注入文本(供 `/unintroduce` 时往 tmux pane 注入)。
///
/// 格式:`[summon] 你在本群不再认识 星光、张三。之后不要再 @ 其中的 bot。`
///
/// **必须注入,不能只删文件**:peers 是冷启时进 PRIMER 的,已经跑起来的会话上下文里还留着
/// 「你认识 X」那句话。只删盘上文件的话,agent 会继续按旧认知去 @ 一个已经不认识它的 bot ——
/// 消息送达但被对方 talk 闸拒,日志里只留一行「拒绝非白名单」,两边都看不出发生了什么。
/// 真人不受这条影响(他们看得见群消息),所以「不要再 @」只对 bot 说。
///
/// 为空返回空串(调用方据此跳过注入)。
pub fn compose_peers_removal_notice(removed: &[Peer]) -> String {
    if removed.is_empty() {
        return String::new();
    }
    let all: Vec<&Peer> = removed.iter().collect();
    let mut out = summon_i18n::format!("r129a924048", join_names(&all));
    if removed.iter().any(|p| !matches!(p.kind, PeerKind::Human)) {
        out.push_str(summon_i18n::text!("rcc2ed9a332"));
    }
    out
}

/// 全群解除(`/unintroduce --all`)的热更新注入文本。
///
/// 与 [`compose_peers_removal_notice`] 分开:全清时逐个点名没有意义(可能十几个),
/// 且用户的意图是「这个群的认识关系整体作废」,一句话说清比列名单准确。
pub fn compose_peers_cleared_notice() -> String {
    summon_i18n::text!("r8ed9ff9d24").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_peers() -> Vec<Peer> {
        vec![
            bot("ou_botA", "星光"),
            bot("ou_botB", "黑魔术师"),
            bot("ou_botC", "arcadia"),
        ]
    }

    fn peer(open_id: &str, name: &str, kind: PeerKind) -> Peer {
        Peer {
            open_id: open_id.into(),
            name: name.into(),
            kind,
        }
    }

    fn bot(open_id: &str, name: &str) -> Peer {
        peer(open_id, name, PeerKind::Bot)
    }

    #[test]
    fn match_peer_by_name_exact() {
        let peers = sample_peers();
        assert_eq!(
            match_peer_by_name(&peers, "星光").map(|p| &p.open_id),
            Some(&"ou_botA".to_string())
        );
        assert_eq!(
            match_peer_by_name(&peers, "arcadia").map(|p| &p.open_id),
            Some(&"ou_botC".to_string())
        );
        // 大小写敏感/不模糊匹配
        assert!(match_peer_by_name(&peers, "鹿").is_none());
        assert!(match_peer_by_name(&peers, "Arcadia").is_none());
    }

    #[test]
    fn match_peer_by_text_name_and_alias() {
        let peers = sample_peers();
        let mut alias_map = HashMap::new();
        alias_map.insert("starlight".into(), "星光".into());
        alias_map.insert("black-magician".into(), "黑魔术师".into());

        // 直接 name 命中
        let hit = match_peer_by_text(&peers, &alias_map, "星光").unwrap();
        assert_eq!(hit.open_id, "ou_botA");

        // alias 转 name 命中
        let hit = match_peer_by_text(&peers, &alias_map, "starlight").unwrap();
        assert_eq!(hit.open_id, "ou_botA");

        let hit = match_peer_by_text(&peers, &alias_map, "black-magician").unwrap();
        assert_eq!(hit.open_id, "ou_botB");

        // 都不命中
        assert!(match_peer_by_text(&peers, &alias_map, "unknown").is_none());
    }

    #[test]
    fn merge_peers_dedup_by_open_id() {
        let existing = vec![
            Peer {
                open_id: "ou_1".into(),
                name: "旧名A".into(),
                ..Default::default()
            },
            Peer {
                open_id: "ou_2".into(),
                name: "B".into(),
                ..Default::default()
            },
        ];
        let new = vec![
            Peer {
                open_id: "ou_1".into(),
                name: "新名A".into(),
                ..Default::default()
            }, // 同 open_id,name 更新
            Peer {
                open_id: "ou_3".into(),
                name: "C".into(),
                ..Default::default()
            }, // 新增
        ];
        let merged = merge_peers(&existing, &new);
        assert_eq!(merged.len(), 3);
        // ou_1 的 name 被 new 覆盖
        assert_eq!(
            merged.iter().find(|p| p.open_id == "ou_1").unwrap().name,
            "新名A"
        );
        // ou_2 保留
        assert!(merged.iter().any(|p| p.open_id == "ou_2"));
        // ou_3 追加
        assert!(merged.iter().any(|p| p.open_id == "ou_3"));
    }

    #[test]
    fn merge_peers_empty_inputs() {
        let peers = sample_peers();
        assert_eq!(merge_peers(&[], &[]), Vec::<Peer>::new());
        assert_eq!(merge_peers(&peers, &[]), peers);
        assert_eq!(merge_peers(&[], &peers), peers);
    }

    #[test]
    fn compose_peers_primer_with_alias_map() {
        let peers = sample_peers();
        let mut alias_map = HashMap::new();
        alias_map.insert("starlight".into(), "星光".into());
        alias_map.insert("black-magician".into(), "黑魔术师".into());
        // arcadia 没有对应的 alias

        let primer = compose_peers_primer(&peers, &alias_map, None);
        assert!(primer.contains("你在本群认识的 bot:"));
        assert!(primer.contains("- starlight（星光）"));
        assert!(primer.contains("- black-magician（黑魔术师）"));
        assert!(primer.contains("- arcadia"));
        // 没有 alias 的不显示括号
        assert!(!primer.contains("arcadia（"));
        assert!(primer.contains("需要它们帮忙时直接 @名字"));
    }

    #[test]
    fn compose_peers_primer_empty_peers_returns_empty() {
        let alias_map: HashMap<String, String> = HashMap::new();
        assert!(compose_peers_primer(&[], &alias_map, None).is_empty());
    }

    #[test]
    fn compose_peers_primer_no_alias_map_shows_only_names() {
        let peers = sample_peers();
        let empty_alias: HashMap<String, String> = HashMap::new();

        let primer = compose_peers_primer(&peers, &empty_alias, None);
        assert!(primer.contains("你在本群认识的 bot:"));
        assert!(primer.contains("- 星光"));
        assert!(primer.contains("- 黑魔术师"));
        assert!(primer.contains("- arcadia"));
        // 没有 alias_map,不应出现括号
        assert!(!primer.contains("（"));
    }

    /// 解除通知要点名说清是谁 —— agent 读到后才知道该把谁从认知里去掉。「不要再 @」只对 bot 说:
    /// 真人本来就看得见群消息,@ 他们只是提醒。
    #[test]
    fn compose_removal_notice_names_targets_and_warns_only_about_bots() {
        let s = compose_peers_removal_notice(&[bot("ou_a", "星光"), bot("ou_b", "炼金师")]);
        assert!(s.contains("不再认识 星光、炼金师"), "{s}");
        assert!(s.contains("不要再 @ 其中的 bot"), "{s}");

        let human_only = compose_peers_removal_notice(&[peer("ou_h", "张三", PeerKind::Human)]);
        assert!(human_only.contains("张三"), "{human_only}");
        assert!(!human_only.contains("不要再 @"), "{human_only}");

        // 空 → 空串,调用方据此跳过注入
        assert!(compose_peers_removal_notice(&[]).is_empty());
    }

    /// 全清不逐个点名:可能十几个,且用户意图是「整体作废」,一句话比列名单准确。
    #[test]
    fn compose_cleared_notice_states_scope_without_listing() {
        let s = compose_peers_cleared_notice();
        assert!(s.contains("全部解除"), "{s}");
        assert!(s.contains("不要再 @ 任何 bot"), "{s}");
    }

    #[test]
    fn compose_peers_hot_update_groups_by_kind() {
        let msg = compose_peers_hot_update(&[bot("ou_1", "星光"), bot("ou_2", "黑魔术师")], None);
        assert!(
            msg.contains("[summon] 你在本群新认识了 bot:星光、黑魔术师。"),
            "{msg}"
        );
        assert!(msg.contains("需要它们帮忙时直接 @名字"), "{msg}");
        assert!(!msg.contains("同事"), "{msg}");

        let mixed = compose_peers_hot_update(
            &[
                peer("ou_h", "张三", PeerKind::Human),
                bot("ou_1", "星光"),
                peer("ou_u", "李四", PeerKind::Unknown),
            ],
            Some("张三提需求"),
        );
        assert!(
            mixed.contains("bot:星光;同事(真人):张三;身份未确认的成员:李四"),
            "{mixed}"
        );
        assert!(mixed.contains("@ 只是提醒"), "{mixed}");
        assert!(mixed.contains("以他们发来的消息为准"), "{mixed}");
        assert!(mixed.ends_with("介绍时交代的分工:张三提需求"), "{mixed}");
    }

    #[test]
    fn compose_peers_hot_update_empty_returns_empty() {
        assert!(compose_peers_hot_update(&[], None).is_empty());
    }

    /// 真人进「同事」一组,并说明 @ 只是提醒;只有 bot 那组说「收不到没 @ 它的消息」。
    #[test]
    fn compose_peers_primer_separates_colleagues_from_bots() {
        let peers = vec![
            bot("ou_bot", "星光"),
            peer("ou_h1", "张三", PeerKind::Human),
            peer("ou_h2", "李四", PeerKind::Human),
        ];
        let primer = compose_peers_primer(&peers, &HashMap::new(), None);
        let bots_at = primer.find("你在本群认识的 bot:\n- 星光\n").unwrap();
        let humans_at = primer
            .find("你在本群认识的同事(真人):\n- 张三\n- 李四\n")
            .unwrap();
        assert!(bots_at < humans_at, "{primer}");
        assert!(primer.contains("@ 只是提醒"), "{primer}");
        assert!(!primer.contains("身份未确认"), "{primer}");

        let humans_only = compose_peers_primer(&peers[1..], &HashMap::new(), None);
        assert!(!humans_only.contains("bot:"), "{humans_only}");
        assert!(!humans_only.contains("收不到没 @"), "{humans_only}");
    }

    /// 没记过身份的(老文件)单独一组,不冒充 bot 也不冒充真人。
    #[test]
    fn compose_peers_primer_lists_unknown_kind_separately() {
        let primer = compose_peers_primer(
            &[peer("ou_x", "王五", PeerKind::Unknown)],
            &HashMap::new(),
            None,
        );
        assert!(primer.contains("身份未确认:\n- 王五\n"), "{primer}");
        assert!(!primer.contains("认识的 bot"), "{primer}");
        assert!(!primer.contains("认识的同事"), "{primer}");
    }

    #[test]
    fn peer_kind_follows_feishu_fields() {
        assert_eq!(PeerKind::from_mentioned_type("bot"), PeerKind::Bot);
        assert_eq!(PeerKind::from_mentioned_type("user"), PeerKind::Human);
        assert_eq!(PeerKind::from_mentioned_type(""), PeerKind::Unknown);
        assert_eq!(PeerKind::from_sender_type("app"), PeerKind::Bot);
        assert_eq!(PeerKind::from_sender_type("user"), PeerKind::Human);
        assert_eq!(PeerKind::from_sender_type("anonymous"), PeerKind::Unknown);
    }

    /// 老 peers 文件没有身份字段:照常读成 Unknown,写回也不凭空多出一个身份。
    #[test]
    fn peers_file_without_kind_round_trips_unchanged() {
        let old = "peers:\n- open_id: ou_a\n  name: 张三\nintroduced_at: null\nnote: null\n";
        let pf: PeersFile = serde_yaml::from_str(old).unwrap();
        assert_eq!(pf.peers[0].kind, PeerKind::Unknown);
        assert!(!serde_yaml::to_string(&pf).unwrap().contains("kind"));

        let known = PeersFile {
            peers: vec![peer("ou_a", "张三", PeerKind::Human)],
            introduced_at: None,
            note: None,
        };
        let yaml = serde_yaml::to_string(&known).unwrap();
        assert!(yaml.contains("kind: human"), "{yaml}");
        let back: PeersFile = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.peers[0].kind, PeerKind::Human);
    }

    /// 重新 introduce 时 @ 事件没给身份,不能把之前确认过的身份冲掉;给了就以新的为准。
    #[test]
    fn merge_peers_keeps_known_kind_when_new_is_unknown() {
        let existing = vec![peer("ou_a", "张三", PeerKind::Human)];
        let merged = merge_peers(&existing, &[peer("ou_a", "张三2", PeerKind::Unknown)]);
        assert_eq!(merged[0].kind, PeerKind::Human);
        assert_eq!(merged[0].name, "张三2");
        let merged = merge_peers(&existing, &[peer("ou_a", "张三", PeerKind::Bot)]);
        assert_eq!(merged[0].kind, PeerKind::Bot);
    }

    #[test]
    fn observed_kind_updates_only_when_it_adds_information() {
        let peers = vec![peer("ou_a", "张三", PeerKind::Unknown), bot("ou_b", "星光")];
        let updated = with_observed_kind(&peers, "ou_a", PeerKind::Human).unwrap();
        assert_eq!(updated[0].kind, PeerKind::Human);
        assert_eq!(updated[1], peers[1]);
        assert!(
            with_observed_kind(&peers, "ou_b", PeerKind::Bot).is_none(),
            "没变不写"
        );
        assert!(
            with_observed_kind(&peers, "ou_a", PeerKind::Unknown).is_none(),
            "观察不到不写"
        );
        assert!(
            with_observed_kind(&peers, "ou_z", PeerKind::Human).is_none(),
            "不在名单"
        );
        // 发送者类型是飞书给的事实,与记录冲突时以它为准。
        let fixed = with_observed_kind(&peers, "ou_b", PeerKind::Human).unwrap();
        assert_eq!(fixed[1].kind, PeerKind::Human);
    }
}
