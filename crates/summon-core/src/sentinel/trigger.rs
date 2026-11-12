//! 触发指令解析——owner 在任意会话里发的「@<alias> <动作词> <正文>」或规范入口
//! 「/summon <alias> <正文>」。
//!
//! 判据刻意严格:必须是行首 `/summon` 完整 token，或行首字面 `@`(bot 不在群时打不出真
//! mention,只能是纯文本)；alias 必须命中注册表，口语入口还必须命中动作词字面前缀。
//! 认不出返回 `None`,绝不猜——上游搜索召回是宽的,这里是唯一的严格闸。

/// 触发的动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerAction {
    /// 「进来回答」:把 bot 拉进当前群,基于群历史作答。
    JoinAnswer,
    /// 「建群」:以 bot 身份建群(owner 为群主),基于原单聊历史作答。
    CreateGroup,
    /// **只拉人,不建会话**(`/invite`)。
    ///
    /// 与 `JoinAnswer` 的差别不是程度而是意图:那条是「进来并回答」,这条是「先进来待命」。
    /// owner 2026-08-07 的用例是把 bot 拉进群之后再用 `/bridge` 桥接一个已有 CLI 会话 ——
    /// 此时任何由 summon 自己起的会话都是多余的,还会先占住这个 chat 的会话位。
    InviteOnly,
}

/// 解析出的触发指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    /// 命中的注册表 alias(原样保留——中文名没有大小写归一化问题)。
    pub alias: String,
    pub action: TriggerAction,
    /// 动作词**之后**的正文(trim 过,可为空——空 body 也是合法召唤)。
    /// 只用来回答「除了召唤词,owner 还附问题了没有」,**不要拿它当 prompt**,见 [`Trigger::verbatim`]。
    pub body: String,
    /// alias 之后的**完整原话**(含动作词,trim 过)。给 agent 的 prompt、开场卡、确认卡摘要
    /// 一律用它。
    ///
    /// **为什么不能用 `body`**:动作词是按前缀剥的,而口语召唤词往往是句子的一部分 ——
    /// 2026-08-12 owner 实测「@<alias> 来帮我看下这个 code review」命中 join 词「来帮」,
    /// body 剩「我看下这个 code review」,主语从 bot 变成了 owner 自己,意思全反。
    /// 剥离本身是有损的:owner 写的每个字都是他要说的,转发时不该替他删。
    pub verbatim: String,
}

/// 尽力把一条消息文本解析成触发指令。判据(全部满足才命中):
/// ① `text.trim()` 以 `@` 开头;② `@` 后紧跟某个已知 alias(**最长优先**,防前缀 alias 误配;
/// **ASCII 大小写不敏感**——飞书 bot 显示名常是首字母大写的「Arcadia」,而 agent.yaml 里是
/// 小写「arcadia」,2026-08-07 真机首测就 miss 在这里;中文不受影响);
/// ③ alias 后(允许空白)紧跟 `join_verbs` 或 `create_verbs` 之一的字面前缀;④ 其余为 body。
pub fn parse_trigger(
    text: &str,
    aliases: &[String],
    join_verbs: &[String],
    create_verbs: &[String],
    invite_verbs: &[String],
) -> Option<Trigger> {
    // 最长优先:aliases 里若同时有「阿卡」「阿卡迪亚」,必须命中更长的那个。
    let mut sorted: Vec<&String> = aliases.iter().filter(|a| !a.is_empty()).collect();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.len()));

    let text = text.trim();
    if let Some(after_command) = text.strip_prefix("/summon") {
        // 命令必须是完整 token；`/summonArcadia` 不能误命中。
        if !after_command.starts_with(char::is_whitespace) {
            return None;
        }
        let rest = after_command.trim_start();
        let alias = sorted.iter().copied().find(|a| {
            rest.len() >= a.len()
                && rest.is_char_boundary(a.len())
                && rest[..a.len()].eq_ignore_ascii_case(a)
                && rest[a.len()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })?;
        // `/summon` 是规范入口,没有动作词可剥,原话与正文本来就同一份。
        let body = rest[alias.len()..].trim().to_string();
        return Some(Trigger {
            alias: alias.clone(),
            action: TriggerAction::JoinAnswer,
            verbatim: body.clone(),
            body,
        });
    }

    let rest = text.strip_prefix('@')?;
    // ASCII 不敏感比较只在字节长度一致时成立(ASCII 大小写同宽),先守 char 边界再切片。
    let alias = sorted.into_iter().find(|a| {
        rest.len() >= a.len()
            && rest.is_char_boundary(a.len())
            && rest[..a.len()].eq_ignore_ascii_case(a)
    })?;
    let after = rest[alias.len()..].trim_start();
    // **invite 必须先于 join 匹配**:`/invite` 是显式命令,语义最窄,不能被任何自然语言动作词
    // 抢先命中。顺序即优先级。
    let (action, verb) = match_verb(after, invite_verbs, TriggerAction::InviteOnly)
        .or_else(|| match_verb(after, join_verbs, TriggerAction::JoinAnswer))
        .or_else(|| match_verb(after, create_verbs, TriggerAction::CreateGroup))?;
    Some(Trigger {
        alias: alias.clone(),
        action,
        body: after[verb.len()..].trim().to_string(),
        verbatim: after.trim().to_string(),
    })
}

/// 把解析出的动作落到「这个会话里物理上做得到」的那个。
///
/// **为什么需要这层**:动作词表是按 owner 的说法分的,而「说法」和「会话类型」是两件事 ——
/// 私聊里说「@Arcadia 帮帮我」,意图是「让它来帮我」,唯一的实现方式是**建群**;
/// 但「帮」配在 join 表里,直译过去是 JoinAnswer,而 p2p 根本加不了成员
/// (`chat.members` 只支持 group/topic),必然撞进一个注定失败的 API。
///
/// 2026-08-08 真机:owner 在私聊里发「@Arcadia 帮帮我」,即便过了水位闸也只会拉人失败。
/// 意图对、说法对、动作错 —— 错在把「说法」当成了「动作」。
///
/// `InviteOnly` 不降级:`/invite` 是显式命令,语义就是「只拉人」,p2p 里做不到就该如实失败,
/// 悄悄替 owner 改成建群是替他做决定。
pub fn effective_action(action: TriggerAction, chat_mode: &str) -> TriggerAction {
    match (action, chat_mode) {
        (TriggerAction::JoinAnswer, "p2p") => TriggerAction::CreateGroup,
        (a, _) => a,
    }
}

/// `CreateGroup` 能不能落地,取决于**有没有一个证明得了的对象**。
///
/// p2p 里「成员表里不是 owner 的那个人」唯一,拿它当建群对象成立;群里同一条判据会从成员表里
/// 挑出**任意**一个同事(取决于飞书返回顺序)。2026-08-12 owner 实测:群里发「@<alias> 帮我 …」
/// (当时「帮我」配在 create_verbs)被落成 CreateGroup,`propose_create_group` 照 p2p 判据挑人,
/// 提案成了「与一位无关同事建三方群」。
///
/// 判据回到目的:**证明不了对象就不动作**,而不是猜一个。未知会话类型同样返回 false —— 建群会
/// 真的拉人、撤不回来,猜错的代价高于拒绝。
pub fn create_group_has_unambiguous_counterpart(chat_mode: &str) -> bool {
    chat_mode == "p2p"
}

/// 在一张动作词表里找字面前缀命中;返回 (动作, 命中的动作词)。
/// **最长优先**:配置里同时有「进来」「进来回答」时必须命中长的,否则「回答」会残留进 body。
fn match_verb<'v>(
    after: &str,
    verbs: &'v [String],
    action: TriggerAction,
) -> Option<(TriggerAction, &'v str)> {
    verbs
        .iter()
        .filter(|v| !v.is_empty())
        .filter(|v| after.starts_with(v.as_str()))
        .max_by_key(|v| v.len())
        .map(|v| (action, v.as_str()))
}

#[cfg(test)]
mod tests {

    /// 既有测试统一传空 invite 表 = 改动前语义(新增动作不改变老行为)。
    fn iv() -> Vec<String> {
        Vec::new()
    }

    fn verbs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// `/invite` 只拉人不建会话 —— owner 拉进来后要用 `/bridge` 桥接已有 CLI 会话,
    /// 此时 summon 自己起的会话会占住该 chat 的会话位。
    #[test]
    fn invite_verb_yields_invite_only_action() {
        let t = parse_trigger(
            "@Flare 5 /invite",
            &verbs(&["Flare 5"]),
            &verbs(&["进来"]),
            &verbs(&["建群"]),
            &verbs(&["/invite"]),
        )
        .expect("应命中");
        assert_eq!(t.action, TriggerAction::InviteOnly);
        assert_eq!(t.alias, "Flare 5");
        assert!(t.body.is_empty(), "只邀请时不该带正文当 prompt");
    }

    /// **匹配顺序即优先级**:`/invite` 是显式命令,不能被任何自然语言动作词抢先。
    /// 若哪天有人把「/」开头的词也配进 join_verbs,这条测试会立刻炸。
    #[test]
    fn invite_wins_over_join_when_both_could_match() {
        let t = parse_trigger(
            "@Arcadia /invite",
            &verbs(&["Arcadia"]),
            // 故意让 join 也能命中同一串
            &verbs(&["/invite", "进来"]),
            &verbs(&["建群"]),
            &verbs(&["/invite"]),
        )
        .expect("应命中");
        assert_eq!(
            t.action,
            TriggerAction::InviteOnly,
            "显式命令必须优先于自然语言动作词"
        );
    }

    /// 既有的「进来」行为一个字不能变 —— 它仍要建会话、并把整句话当 prompt。
    #[test]
    fn join_verb_behavior_is_unchanged() {
        let t = parse_trigger(
            "@Arcadia 进来帮我看下这个报错",
            &verbs(&["Arcadia"]),
            &verbs(&["进来"]),
            &verbs(&["建群"]),
            &verbs(&["/invite"]),
        )
        .expect("应命中");
        assert_eq!(t.action, TriggerAction::JoinAnswer);
        assert_eq!(t.body, "帮我看下这个报错");
    }

    /// 没配 invite_verbs(空表)时行为退回改动前 —— 老配置文件不会因为升级而改变语义。
    #[test]
    fn empty_invite_verbs_degrade_to_previous_behavior() {
        let t = parse_trigger(
            "@Arcadia 进来",
            &verbs(&["Arcadia"]),
            &verbs(&["进来"]),
            &verbs(&["建群"]),
            &[],
        )
        .expect("应命中");
        assert_eq!(t.action, TriggerAction::JoinAnswer);
    }

    /// 私聊里的 join 类说法必须落成建群 —— p2p 加不了成员,直译过去只会拉人失败。
    #[test]
    fn 私聊里的join动作降级为建群() {
        assert_eq!(
            effective_action(TriggerAction::JoinAnswer, "p2p"),
            TriggerAction::CreateGroup
        );
    }

    /// owner 2026-08-12 实测:「@Arcadia 来帮我看下这个 code review」命中 join 词「来帮」,
    /// 转发给 agent 的却是剥过的「我看下这个 code review」——主语从 bot 变成 owner 自己,
    /// 指令反了。口语召唤词是句子的一部分,原话必须完整转发。
    #[test]
    fn 口语召唤词不能从转发给agent的原话里剥掉() {
        let t = parse_trigger(
            "@Arcadia 来帮我看下这个 code review",
            &verbs(&["Arcadia"]),
            &verbs(&["来帮", "进来"]),
            &verbs(&["建群"]),
            &verbs(&["/invite"]),
        )
        .expect("应命中");
        assert_eq!(t.action, TriggerAction::JoinAnswer);
        assert_eq!(
            t.verbatim, "来帮我看下这个 code review",
            "转发给 agent 的必须是 owner 的原话"
        );
        assert_eq!(
            t.body, "我看下这个 code review",
            "body 仍是剥过动作词的部分——它只回答「附问题了没有」"
        );
    }

    /// 只喊召唤词、不附问题:`body` 空(下游据此走「主动接话」兜底文案),但原话仍在。
    #[test]
    fn 只喊召唤词时body为空而原话保留() {
        let t = parse_trigger(
            "@Arcadia 进来回答",
            &verbs(&["Arcadia"]),
            &verbs(&["进来回答", "进来"]),
            &verbs(&["建群"]),
            &verbs(&["/invite"]),
        )
        .expect("应命中");
        assert!(t.body.is_empty(), "body={:?}", t.body);
        assert_eq!(t.verbatim, "进来回答");
    }

    /// `/summon` 是规范入口,没有动作词可剥,两者本来就同一份。
    #[test]
    fn summon入口的原话与正文一致() {
        let t = parse_trigger(
            "/summon Arcadia 这个接口为什么超时",
            &verbs(&["Arcadia"]),
            &verbs(&["进来"]),
            &verbs(&["建群"]),
            &iv(),
        )
        .expect("应命中");
        assert_eq!(t.body, "这个接口为什么超时");
        assert_eq!(t.verbatim, t.body);
    }

    /// 只有 p2p 能证明「对方是谁」。群/话题/未知类型一律拒绝——挑「第一个不是 owner 的人」
    /// 在群里会命中任意同事,那正是 2026-08-12 误建三方群的成因。
    #[test]
    fn 只有单聊能证明建群对象() {
        assert!(create_group_has_unambiguous_counterpart("p2p"));
        for mode in ["group", "topic", "", "P2P", "unknown"] {
            assert!(
                !create_group_has_unambiguous_counterpart(mode),
                "chat_mode={mode} 不能证明建群对象,必须拒绝"
            );
        }
    }

    /// 群里不变:JoinAnswer 在 group/topic 里本来就做得到。
    #[test]
    fn 群里的join动作保持原样() {
        for mode in ["group", "topic", ""] {
            assert_eq!(
                effective_action(TriggerAction::JoinAnswer, mode),
                TriggerAction::JoinAnswer,
                "chat_mode={mode}"
            );
        }
    }

    /// `/invite` 是显式命令,任何会话类型下都不替 owner 改写意图。
    #[test]
    fn invite与建群动作从不降级() {
        for mode in ["p2p", "group", "topic"] {
            assert_eq!(
                effective_action(TriggerAction::InviteOnly, mode),
                TriggerAction::InviteOnly
            );
            assert_eq!(
                effective_action(TriggerAction::CreateGroup, mode),
                TriggerAction::CreateGroup
            );
        }
    }

    /// 端到端:owner 在私聊里发「@Arcadia 帮帮我」,「帮」配在 join 表里 —— 解析成 JoinAnswer,
    /// 经会话类型归一后必须是建群。这正是 2026-08-08 那条没拉起群的消息。
    #[test]
    fn 私聊里帮帮我最终落成建群() {
        let t = parse_trigger(
            "@Arcadia 帮帮我",
            &verbs(&["Arcadia"]),
            &verbs(&["帮"]),
            &verbs(&["建群"]),
            &verbs(&["/invite"]),
        )
        .expect("应命中 join 表里的「帮」");
        assert_eq!(t.action, TriggerAction::JoinAnswer);
        assert_eq!(t.body, "帮我");
        assert_eq!(
            effective_action(t.action, "p2p"),
            TriggerAction::CreateGroup
        );
    }

    use super::*;

    fn aliases() -> Vec<String> {
        // 含前缀 alias("阿卡"是"阿卡迪亚"的前缀),验最长优先。
        vec!["阿卡迪亚".into(), "星光".into(), "阿卡".into()]
    }
    fn jv() -> Vec<String> {
        vec!["进来回答".into()]
    }
    fn cv() -> Vec<String> {
        vec!["建群".into()]
    }

    #[test]
    fn 群聊触发_带空格() {
        let t = parse_trigger(
            "@阿卡迪亚 进来回答 这个接口为什么超时",
            &aliases(),
            &jv(),
            &cv(),
            &iv(),
        )
        .unwrap();
        assert_eq!(t.alias, "阿卡迪亚");
        assert_eq!(t.action, TriggerAction::JoinAnswer);
        assert_eq!(t.body, "这个接口为什么超时");
    }

    #[test]
    fn 无空格也命中() {
        let t =
            parse_trigger("@阿卡迪亚进来回答这些消息", &aliases(), &jv(), &cv(), &iv()).unwrap();
        assert_eq!(t.action, TriggerAction::JoinAnswer);
        assert_eq!(t.body, "这些消息");
    }

    #[test]
    fn 建群动作() {
        let t = parse_trigger("@星光 建群回答这些消息", &aliases(), &jv(), &cv(), &iv()).unwrap();
        assert_eq!(t.action, TriggerAction::CreateGroup);
        assert_eq!(t.body, "回答这些消息");
    }

    #[test]
    fn 最长alias优先() {
        let t = parse_trigger("@阿卡迪亚 进来回答 x", &aliases(), &jv(), &cv(), &iv()).unwrap();
        assert_eq!(t.alias, "阿卡迪亚"); // 不能命中"阿卡"
    }

    #[test]
    fn 未知alias不命中() {
        assert!(parse_trigger("@张三 进来回答 x", &aliases(), &jv(), &cv(), &iv()).is_none());
    }

    #[test]
    fn 无动作词不命中() {
        assert!(parse_trigger("@阿卡迪亚 你好", &aliases(), &jv(), &cv(), &iv()).is_none());
    }

    #[test]
    fn 不以at开头不命中() {
        assert!(
            parse_trigger("请 @阿卡迪亚 进来回答 x", &aliases(), &jv(), &cv(), &iv()).is_none()
        );
    }

    #[test]
    fn 正文提及alias不误触() {
        assert!(parse_trigger(
            "我觉得阿卡迪亚进来回答比较好",
            &aliases(),
            &jv(),
            &cv(),
            &iv()
        )
        .is_none());
    }

    #[test]
    fn 空body合法() {
        let t = parse_trigger("@阿卡迪亚 进来回答", &aliases(), &jv(), &cv(), &iv()).unwrap();
        assert_eq!(t.body, "");
    }

    #[test]
    fn 空alias表不命中() {
        assert!(parse_trigger("@阿卡迪亚 进来回答 x", &[], &jv(), &cv(), &iv()).is_none());
    }

    #[test]
    fn 动作词最长优先() {
        // 配置同时有「进来」「进来回答」:必须命中长词,body 不残留「回答」。
        let verbs = vec!["进来".to_string(), "进来回答".to_string()];
        let t = parse_trigger("@星光 进来回答这个问题", &aliases(), &verbs, &cv(), &iv()).unwrap();
        assert_eq!(t.body, "这个问题");
        // 只命中短词的输入也照常工作。
        let t = parse_trigger("@星光 进来看看", &aliases(), &verbs, &cv(), &iv()).unwrap();
        assert_eq!(t.body, "看看");
    }

    #[test]
    fn 英文alias大小写不敏感() {
        // 真机首测 miss:飞书 bot 显示名「Arcadia」,agent.yaml 里「arcadia」。
        let aliases = vec!["arcadia".to_string()];
        for text in [
            "@Arcadia 进来回答你是谁",
            "@ARCADIA 进来回答你是谁",
            "@arcadia 进来回答你是谁",
        ] {
            let t = parse_trigger(text, &aliases, &jv(), &cv(), &iv()).unwrap();
            assert_eq!(t.alias, "arcadia");
            assert_eq!(t.body, "你是谁");
        }
        // 建群动作同样覆盖。
        let t = parse_trigger("@Arcadia 建群说明下你是谁", &aliases, &jv(), &cv(), &iv()).unwrap();
        assert_eq!(t.action, TriggerAction::CreateGroup);
        assert_eq!(t.body, "说明下你是谁");
    }

    #[test]
    fn summon规范入口直接召唤agent() {
        let aliases = vec!["arcadia".to_string(), "Flare 5".to_string()];
        for (text, alias, body) in [
            (
                "/summon Arcadia 看一下这个报错",
                "arcadia",
                "看一下这个报错",
            ),
            ("/summon Flare 5", "Flare 5", ""),
        ] {
            let t = parse_trigger(text, &aliases, &jv(), &cv(), &iv()).expect("规范入口应命中");
            assert_eq!(t.alias, alias);
            assert_eq!(t.action, TriggerAction::JoinAnswer);
            assert_eq!(t.body, body);
        }
    }

    #[test]
    fn summon规范入口保持严格边界() {
        let aliases = vec!["arcadia".to_string()];
        for text in [
            "/summonArcadia 看一下",     // 命令后缺空白
            "/summon unknown 看一下",    // 未知 alias
            "请 /summon arcadia 看一下", // 不在行首
        ] {
            assert!(
                parse_trigger(text, &aliases, &jv(), &cv(), &iv()).is_none(),
                "不该命中:{text}"
            );
        }
    }
}
